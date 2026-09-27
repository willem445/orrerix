//! The hand-back: session identity (#1961), the cap on idle panes (#1960), fix pushes, and fix-wait reports.
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── #1961: the hand-back resumes the worker's OWN session identity ──────────

/// A roster with a SECOND worker block, so **"the worker's block" and "the
/// roster's default worker block" can differ**.
///
/// Every fixture in this file until now declared exactly one worker, which made
/// those two strings the same value — CLAUDE.md's unpinned-axis rule exactly: a
/// value every fixture happens to share, and the axis #1961 is about. Under the
/// one-worker roster the defect is not merely undetected, it is unfalsifiable.
///
/// `worker` is declared FIRST because `Guardrails::block_for` answers with the
/// first block of a class: that makes `worker` the default and `worker-adv` the
/// one a hand-back can only reach by reading the session's own record.
const WORKFLOW_TWO_WORKERS: &str = r#"version: 1
blocks:
  - id: worker
    kind: worker
  - id: worker-adv
    name: Advanced worker
    kind: worker
  - id: rev-std
    name: Standard review
    kind: reviewer
gates:
  merge:
    require: all-pass
    reviewers: [rev-std]
    routing:
      - paths: [src/**]
        reviewers: [rev-std]
merge_queue:
  enabled: true
driver:
  enabled: true
"#;

/// [`driven`], with the worker spawned under a NAMED block — the fixture axis
/// `driven` cannot vary, since it spawns by class and therefore always lands on
/// the roster's default.
fn driven_as(
    reg: &OrchRegistry,
    repo: &Repo,
    gh: &FakeGh,
    block: &str,
) -> (GroupId, String, String) {
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let w = reg
        .spawn_agent_ex(&group, Role::Worker, Some(block.to_string()), "w", "", false, None, None, None, None, None)
        .expect("a worker to hand back to");
    assert_eq!(w.block, block, "the fixture must actually land in the block it names");
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    let out = reg.drive_review_with(&group, gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], serde_json::json!(true), "drive_review refused: {out}");
    (group, session, w.id)
}

/// Walk a fresh drive to its first hand-back and answer what the tick reported.
///
/// The sequence is the one every hand-back test in this file already performs
/// (open lane 0, turn the checks red at a new head, take arc 6 then arc 3);
/// factored out because four tests below need to reach that state and none of
/// them is about how it is reached.
pub(crate) fn to_first_handback(reg: &OrchRegistry, group: &GroupId, gh: &FakeGh) -> RdDriveReport {
    reg.rd_drive_group_with(group, gh, 10_000);
    reg.rd_drive_group_with(group, gh, 20_000);
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    gh.set_facts("OPEN", HEAD_B);
    reg.rd_drive_group_with(group, gh, 30_000);
    reg.rd_drive_group_with(group, gh, 40_000)
}

/// **#1961's root cause, in both directions.**
///
/// `rd_handback` resumed the worker with `block: None`, and `spawn_agent_bound`
/// — which has no session-inheritance rule of its own, #254's living in the MCP
/// `spawn_agent` arm — falls straight through to `block_for(Role::Worker)`. So
/// every drive whose worker was not the default block had its fix handed to the
/// wrong persona on whatever CLI that block pins: measured on the dogfood, a
/// `worker-adv` (Claude) session reopened by opencode, which exited 5.4s later
/// with `Invalid session ID`.
///
/// The `worker` row is the **positive control** and it is load-bearing rather
/// than decorative: it is what distinguishes this fix from one that hard-codes
/// a second block id, and it is the row that would still pass under the defect,
/// so a run where only it goes green says the harness is looking at the wrong
/// thing.
#[test]
fn a_handback_resumes_the_worker_under_the_sessions_own_block() {
    for declared in ["worker-adv", "worker"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::with(WORKFLOW_TWO_WORKERS);
        let gh = FakeGh::green(HEAD_A);
        let (group, _session, _w) = driven_as(&reg, &repo, &gh, declared);

        let handed = to_first_handback(&reg, &group, &gh);
        let (_pr, agent) = handed
            .handbacks
            .first()
            .cloned()
            .unwrap_or_else(|| panic!("the drive must hand back for {declared}: {handed:?}"));
        assert_eq!(
            reg.agent(&agent).expect("the resumed pane is registered").block,
            declared,
            "the hand-back resumed a {declared} session under another block — that is a \
             different persona on a CLI that may not be able to open the transcript at all"
        );
    }
}

/// **#3442, the driver's half: a hand-back's resumed pane records the worker's
/// own branch.** `rd_handback` resumes through `rd_spawn`, which passes the
/// session's workspace as `cwd_override` — the same `spawn_agent_full` arm the
/// MCP resume reaches, and the one that used to persist no branch. The close
/// gate reads that branch from `agent_owners`, so without it every worker the
/// driver handed back to could not close its own `-scratchN` PRs.
///
/// The `spawned` pane word is asserted as a PRE-condition: a hand-back that
/// reused the original pane would read the original's branch and pass under
/// the defect.
#[test]
fn a_handback_resume_records_the_workers_own_branch() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::with(WORKFLOW_TWO_WORKERS);
    let gh = FakeGh::green(HEAD_A);
    let (group, session, w) = driven_as(&reg, &repo, &gh, "worker");
    let own = reg
        .agent(&w)
        .and_then(|a| a.branch)
        .expect("fixture: the driven worker records a branch — otherwise there is nothing to inherit");

    let handed = to_first_handback(&reg, &group, &gh);
    let (_pr, agent) = handed.handbacks.first().cloned().unwrap_or_else(|| {
        panic!("the drive must hand back: {handed:?}")
    });
    assert_eq!(
        handback_panes(&reg, &group),
        vec!["spawned".to_string()],
        "the hand-back must have OPENED a pane — a reused one carries the original's branch \
         and proves nothing"
    );
    assert_ne!(agent, w, "the resumed pane is a new one");
    let resumed = reg.agent(&agent).expect("the resumed pane is registered");
    assert_eq!(resumed.session_id.as_deref(), Some(session.as_str()), "it runs the worker's session");
    assert_eq!(
        resumed.branch.as_deref(),
        Some(own.as_str()),
        "the driver's resumed worker must record its session's branch, or the close gate owns \
         nothing for it and refuses its own -scratchN PRs (#3442)"
    );
    let roster = std::fs::read_to_string(
        reg.state_root().join(group.as_str()).join(loomux_lib::orchestration::OWNER_ROSTER_FILE),
    )
    .expect("the owner roster is written");
    assert!(
        roster.lines().any(|l| l == format!("{agent} worker {own}")),
        "the gate's roster row for the resumed pane names the branch: {roster}"
    );
}

/// **#1961's amplifier, with the discriminating fixture the issue named**: two
/// roster rows for one session carrying different blocks, the OLDER one right.
///
/// `spawn_agent(resume_session:)` naming neither `kind` nor `block` inherits the
/// session's block (#254), and it used to inherit it from the LAST-TOUCHED row.
/// That is how one wrong resume poisoned every later one: the driver wrote a
/// `worker-std` row for a `worker-adv` session, and the orchestrator's own hand
/// recovery — the documented inherit-the-session's-block form — came back
/// `worker-std` too and died the same way. The rule is now the roster's FIRST
/// row, which is the pane that minted the session.
///
/// The second spawn is the fixture's whole point and is asserted as a
/// PRE-condition: without a newer row carrying a different block there is
/// nothing for `max_by_key` to prefer, and the test would pass under the defect.
#[test]
fn a_bare_resume_inherits_the_block_the_session_was_minted_under() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::with(WORKFLOW_TWO_WORKERS);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();

    // Row 1 — the pane that minted the session, under the NON-default block.
    let w = reg
        .spawn_agent_ex(&group, Role::Worker, Some("worker-adv".into()), "w", "", false, None, None, None, None, None)
        .expect("the original worker");
    let session = w.session_id.clone().expect("claude mints a session id at spawn");

    // Row 2 — a later, WRONGER row for the same session. This is exactly the
    // shape the driver's own hand-back used to write.
    let wrong = reg
        .spawn_agent_ex(
            &group, Role::Worker, Some("worker".into()), "w2", "", false, None, None,
            Some(session.clone()), Some(w.cwd.clone()), None,
        )
        .expect("a second pane on the same session");
    assert_eq!(
        (reg.agent(&w.id).unwrap().block, reg.agent(&wrong.id).unwrap().block),
        ("worker-adv".to_string(), "worker".to_string()),
        "the fixture must really carry two rows for one session with DIFFERENT blocks, \
         the older one right — otherwise there is nothing for the newest-row rule to get \
         wrong and this test passes under the defect"
    );
    assert_eq!(reg.agent(&wrong.id).unwrap().session_id.as_deref(), Some(session.as_str()));

    // The bare resume, through the real tool.
    let caller = Caller {
        agent_id: orch.id.clone(),
        group: group.clone(),
        role: Role::Orchestrator,
        role_hint: None,
    };
    let out = dispatch(
        &reg,
        &caller,
        "tools/call",
        &json!({ "name": "spawn_agent", "arguments": { "resume_session": session } }),
    )
    .expect("a bare resume of a recorded session must be accepted");
    // The tool's own reply, which names the block it resolved — read through the
    // MCP envelope rather than off the `Value`, since `tools/call` answers
    // `{content:[{type,text}]}`.
    let text = out["content"][0]["text"].as_str().unwrap_or_default().to_string();
    assert!(
        text.contains("block "),
        "the spawn reply must name a block at all — an empty read here is the envelope \
         moving, not the inheritance answering: {out}"
    );
    assert!(
        text.contains("block worker-adv"),
        "a bare resume inherited the NEWEST row's block instead of the identity the session \
         was minted under: {text}"
    );
}

/// **The hand-back that cannot be made is refused at the CALL** (#2819 (g),
/// S7): a session whose recorded block is no longer declared in this group's
/// roster is refused by `drive_review` with the sentence the eventual hold
/// would have quoted — one refusal instead of the hold every resume used to
/// buy (#1961 left the acceptance in place, deferring the block question to
/// the hand-back; #2819 measured what that costs when the block can never
/// come back: three holds and three orchestrator turns for one PR).
///
/// Degrading to the class default is what the session browser's rejoin does,
/// and it is right there — a human is present, and losing the persona beats
/// losing the session. Here nobody is watching, and a silently re-personad
/// worker is the whole of #1961, so orrerix refuses rather than guesses —
/// and since S7 it refuses at the call, before the drive exists to hold.
///
/// The session still RESOLVES — that is what makes this a block question and
/// not a session one — so the refusal names the block.
#[test]
fn drive_review_refuses_a_session_whose_block_left_the_roster() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::with(WORKFLOW_TWO_WORKERS);
    let gh = FakeGh::green(HEAD_A);

    // A group whose roster HAS `worker-adv`, and a worker session minted under
    // it. Its roster row outlives this registry — `agents.json` is on disk.
    let session = {
        let reg = relaunch_registry(dir.path());
        let group = reg.create_group(&repo.path(), rails()).unwrap().id;
        let w = reg
            .spawn_agent_ex(&group, Role::Worker, Some("worker-adv".into()), "w", "", false, None, None, None, None, None)
            .expect("the original worker");
        assert_eq!(w.block, "worker-adv");
        w.session_id.clone().expect("claude mints a session id at spawn")
    };

    // The human edits the workflow file and relaunches. §222's consent rule
    // pins a roster at LAUNCH, so this is the one moment a group's declared
    // blocks legitimately change under a recorded session — and it is exactly
    // the case the acceptance names.
    repo.rewrite_workflow(WORKFLOW);
    let reg = relaunch_registry(dir.path());
    let group = reg
        .create_group_ex(&repo.path(), rails(), Launch::Fresh)
        .expect("relaunching the same group")
        .id;
    assert!(
        reg.spawn_agent_ex(&group, Role::Worker, Some("worker-adv".into()), "x", "", false, None, None, None, None, None)
            .unwrap_err()
            .contains("unknown block"),
        "the fixture's premise: `worker-adv` is no longer declared in this group"
    );
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["refused"], json!("worker-unresumable"), "{out}");
    let detail = out["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("unknown block") && detail.contains("worker-adv"),
        "the refusal quotes the spawn guard's own sentence naming the block that is gone: \
         {out}"
    );
    // A roster read answered this — the refusal comes before the one `gh`
    // round trip the call's cheapest-first ladder spends, and no drive exists
    // to hold or tick over.
    assert!(gh.calls().is_empty(), "a block question costs no `gh` call: {:?}", gh.calls());
    let refused = audit_details(&reg, &group, "rd-refused");
    assert!(
        refused.iter().any(|d| d["reason"] == json!("worker-unresumable")
            && d["detail"].as_str().unwrap_or_default().contains("worker-adv")),
        "the audit row carries the same pair the tool answered with: {refused:?}"
    );
}

/// **A drive whose worker session is the ORCHESTRATOR's or the MANAGER's is
/// refused at the call** (#2819 (g), S7).
///
/// #2819's incident: the orchestrator passed its OWN session to
/// `drive_review`, the call accepted it, and the drive held
/// `worker-unresumable` on every resume — `block "orchestrator" is an
/// orchestrator block` — three holds and three orchestrator turns for a PR
/// that could never be handed back. The call now reads the same roster record
/// the hand-back reads and refuses with the SAME sentence the hold would have
/// quoted, for the manager's pane too (#1161 M3's rule is the same shape:
/// no agent-reachable route resumes one).
///
/// The worker arm is the control and it is load-bearing rather than
/// decorative: it is what distinguishes this refusal from one that refuses
/// every session, and it is the row that would still pass under a build that
/// broke the driver's hand-back entirely.
#[test]
fn drive_review_refuses_the_orchestrator_or_manager_session_at_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;

    // The orchestrator's own session — exactly what #2819's orchestrator
    // passed, believing the driver would hand a fix back to it.
    let orch =
        reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    let orch_session =
        orch.session_id.clone().expect("claude mints a session id at spawn");
    let out = reg.drive_review_with(&group, &gh, 1758, &orch_session, false, 0, "orch-1", 0);
    assert_eq!(out["refused"], json!("worker-unresumable"), "{out}");
    let detail = out["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("orchestrator") && detail.contains("is an orchestrator block"),
        "the refusal quotes the spawn guard's own sentence, naming the block: {out}"
    );

    // The manager's twin, on a roster that declares one — a manager session is
    // resolvable in the roster too, which is exactly what made the acceptance
    // dangerous.
    let dir2 = tempfile::tempdir().unwrap();
    let reg2 = relaunch_registry(dir2.path());
    let repo2 = Repo::with(WORKFLOW_WITH_MANAGER);
    let gh2 = FakeGh::green(HEAD_A);
    let group2 = reg2.create_group(&repo2.path(), rails()).unwrap().id;
    let mgr = reg2.spawn_agent(&group2, Role::Manager, "mgr", "", false, None).unwrap();
    let mgr_session = mgr.session_id.clone().expect("claude mints a session id at spawn");
    let out2 = reg2.drive_review_with(&group2, &gh2, 1758, &mgr_session, false, 0, "orch-1", 0);
    assert_eq!(out2["refused"], json!("worker-unresumable"), "{out2}");
    let detail2 = out2["detail"].as_str().unwrap_or_default();
    assert!(
        detail2.contains("manager") && detail2.contains("is this group's manager"),
        "the manager refusal is the manager guard's own sentence, not the orchestrator's: {out2}"
    );

    // The control: a roster worker session — the thing a drive exists to hand
    // back to — is accepted, on the SAME registry the orchestrator refusal
    // came from.
    let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).unwrap();
    let w_session = w.session_id.clone().expect("claude mints a session id at spawn");
    let ok = reg.drive_review_with(&group, &gh, 1758, &w_session, false, 0, "orch-1", 0);
    assert_eq!(ok["driving"], json!(true), "the control must pass: {ok}");
}

/// **A hand-back that fails the same way twice says so** (#2555 item 2, S7).
///
/// §5.1's passthrough arm — a full, well-shaped session id this roster never
/// recorded — is accepted at the call BY DESIGN ("resolving is not proving
/// resumable"), and its unresumability surfaces at the first hand-back. What
/// the first hold used to invite was another resume, which failed the same
/// way and held again: the reflex #2819 measured at three holds. The second
/// identical failure — same session, same failure line — now prefixes
/// "second time" onto the quoted refusal, so the notice reads as a decision
/// (re-point the drive, or cancel) rather than the same invitation again.
#[test]
fn a_second_identical_handback_failure_parks_saying_second_time() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    // Well-shaped and never recorded: the passthrough arm's exact subject.
    let session = "cafb930d-1111-2222-3333-444444444444";
    let out = reg.drive_review_with(&group, &gh, 1758, session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "the passthrough arm still accepts: {out}");

    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7301);

    to_first_handback(&reg, &group, &gh);
    assert_eq!(status_state(&reg, &group), "held");
    let helds = audit_details(&reg, &group, "rd-held");
    assert_eq!(helds.len(), 1, "one hold: {helds:?}");
    assert_eq!(helds[0]["reason"], json!("worker-unresumable"));
    let first = helds[0]["refusal"].as_str().unwrap_or_default();
    assert!(
        !first.contains("second time"),
        "the FIRST failure is a first, and the notice must not claim otherwise: {first}"
    );
    assert!(
        first.contains("no roster record"),
        "and it quotes what actually refused: {first}"
    );

    // The resume the first notice invites — same session, same drive, same
    // failure. ONE hand-back attempt per resume, then the hold again.
    let resumed = reg.drive_review_with(&group, &gh, 1758, session, false, 0, "orch-1", 50_000);
    assert_eq!(resumed["driving"], json!(true), "the held drive resumes: {resumed}");
    reg.rd_drive_group_with(&group, &gh, 60_000);
    reg.rd_drive_group_with(&group, &gh, 70_000);

    let helds = audit_details(&reg, &group, "rd-held");
    assert_eq!(helds.len(), 2, "two holds, one per failed hand-back: {helds:?}");
    assert_eq!(helds[1]["reason"], json!("worker-unresumable"), "{helds:?}");
    let second = helds[1]["refusal"].as_str().unwrap_or_default();
    assert!(
        second.contains("second time"),
        "the SECOND identical failure says so: {second}"
    );
    assert!(
        second.contains("no roster record"),
        "…and still quotes the failure itself beside it: {second}"
    );
    let notice = drive_notices(&reg, &group, 1758).join("\n");
    assert!(
        notice.contains("second time"),
        "the line the orchestrator actually reads carries it: {notice}"
    );
}

/// **A FRESH drive on the PR starts the second-failure count over** (N2 of the
/// review on this PR; the field doc's "cleared on a fresh drive").
///
/// The count belongs to the drive's history, not to the PR: #2819's reflex
/// ("resume and lose again") is broken by a cancel-and-redrive just as much as
/// by re-pointing, so a new drive gets one honest first failure. This is also
/// the observable half of the field doc's clear-on-fresh-drive claim — the
/// clear itself sits before the only `DriveEntry::new` site, so every new
/// drive passes it; what a regression there would look like is exactly the
/// assertion below going red (the first failure of the new drive saying
/// "second time").
#[test]
fn a_fresh_drive_on_the_pr_starts_the_second_failure_count_over() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let session = "cafb930d-1111-2222-3333-444444444444";
    let out = reg.drive_review_with(&group, &gh, 1758, session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "{out}");

    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7401);

    // Two identical failures: the state the second-time bound exists for.
    to_first_handback(&reg, &group, &gh);
    let resumed = reg.drive_review_with(&group, &gh, 1758, session, false, 0, "orch-1", 50_000);
    assert_eq!(resumed["driving"], json!(true), "{resumed}");
    reg.rd_drive_group_with(&group, &gh, 60_000);
    reg.rd_drive_group_with(&group, &gh, 70_000);
    let helds = audit_details(&reg, &group, "rd-held");
    assert_eq!(helds.len(), 2, "the premise, two holds: {helds:?}");
    assert!(
        helds[1]["refusal"].as_str().unwrap_or_default().contains("second time"),
        "the premise, the second is decorated: {:?}",
        helds[1]
    );

    // The orchestrator's other way out: cancel, then drive the PR again. The
    // new drive is a new entry with a clean count — its FIRST failure must not
    // inherit the previous drive's history.
    reg.cancel_review_drive(&group, 1758, "orch-1");
    gh.set_checks(r#"[{"name":"build","state":"SUCCESS","link":"x"}]"#);
    gh.set_facts("OPEN", HEAD_A);
    let redrive = reg.drive_review_with(&group, &gh, 1758, session, false, 0, "orch-1", 80_000);
    assert_eq!(redrive["driving"], json!(true), "the cancelled PR re-drives: {redrive}");
    to_first_handback(&reg, &group, &gh);

    let helds = audit_details(&reg, &group, "rd-held");
    assert_eq!(helds.len(), 3, "one hold per failed hand-back: {helds:?}");
    assert_eq!(helds[2]["reason"], json!("worker-unresumable"), "{helds:?}");
    let first_of_the_new_drive = helds[2]["refusal"].as_str().unwrap_or_default();
    assert!(
        first_of_the_new_drive.contains("no roster record")
            && !first_of_the_new_drive.contains("second time"),
        "the new drive's first failure is a first, not the old drive's second: \
         {first_of_the_new_drive}"
    );
}

/// **The hold path §2.2 still describes is pinned: an unknown block at the
/// hand-back, after the roster changed under a live drive** (N1 of the review
/// on this PR).
///
/// S7 moved the roster question to the call for every drive that STARTS after
/// the change — but a drive that started while the block was declared keeps
/// running, and the human can rewrite the workflow and relaunch under it at
/// any moment. The hand-back then refuses "unknown block" and holds, naming
/// the block; #1961's refuse-don't-degrade rule is what that hold still
/// enforces, and this is its only remaining pin.
#[test]
fn a_drive_overtaken_by_a_roster_change_holds_at_the_handback_naming_the_block() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::with(WORKFLOW_TWO_WORKERS);
    let gh = FakeGh::green(HEAD_A);

    // The drive starts while `worker-adv` is declared.
    let (group, session, _w) = {
        let reg = relaunch_registry(dir.path());
        let (group, session, _w) = driven_as(&reg, &repo, &gh, "worker-adv");
        (group, session, _w)
    };

    // …and the roster changes under it before the first hand-back.
    repo.rewrite_workflow(WORKFLOW);
    let reg = relaunch_registry(dir.path());
    let regrouped = reg
        .create_group_ex(&repo.path(), rails(), Launch::Fresh)
        .expect("relaunching the same group")
        .id;
    assert_eq!(regrouped, group, "the relaunch resumes the same group state dir");
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7501);

    let handed = to_first_handback(&reg, &group, &gh);
    assert!(handed.handbacks.is_empty(), "no pane may be opened for a block that is gone");
    assert_eq!(status_state(&reg, &group), "held");
    let helds = audit_details(&reg, &group, "rd-held");
    assert_eq!(helds.len(), 1, "one hold: {helds:?}");
    assert_eq!(helds[0]["reason"], json!("worker-unresumable"));
    let refusal = helds[0]["refusal"].as_str().unwrap_or_default();
    assert!(
        refusal.contains("unknown block") && refusal.contains("worker-adv"),
        "the hold quotes the spawn guard's own sentence, naming the block: {refusal}"
    );
    assert!(
        !refusal.contains("second time"),
        "a FIRST failure is never decorated: {refusal}"
    );
    let notice = drive_notices(&reg, &group, 1758).join("\n");
    assert!(
        notice.contains("worker-adv"),
        "the line the orchestrator reads names the block that is gone: {notice}"
    );
}

/// **The both-empty arm of the call check: a record with NO block, and no
/// worker block to fall back to** (N3 of the review on this PR; the fourth
/// sentence §5.1 enumerates).
///
/// A pre-#222 roster row records a role and no block identity; `rd_handback`
/// falls back to the class default, and when the roster declares no worker
/// block either, the spawn dies on `no_default_block_message`. The call now
/// answers that at drive time with the same sentence.
#[test]
fn drive_review_refuses_a_session_with_no_block_and_no_worker_block_to_fall_back_to() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    // No worker block at all — the class default the hand-back would fall
    // back to does not exist.
    let repo = Repo::with(WORKFLOW_NO_WORKER);
    let gh = FakeGh::green(HEAD_A);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;

    // A pre-#222-shaped roster row: role recorded, NO block key. No spawn path
    // produces one any more, so the fixture writes `agents.json` directly —
    // the same seeding `tests/groupid.rs` uses — rather than pretending a
    // modern spawn can mint the subject.
    let session = "cafb930d-3333-4444-5555-666666666666";
    let row = format!(
        r#"[{{"id":"w-1","role":"worker","name":"w","session":"{session}",
             "cwd":"{}","status":"running","updated_ms":1}}]"#,
        repo.path()
    );
    std::fs::write(dir.path().join(group.as_str()).join("agents.json"), row).unwrap();

    let out = reg.drive_review_with(&group, &gh, 1758, session, false, 0, "orch-1", 0);
    assert_eq!(out["refused"], json!("worker-unresumable"), "{out}");
    let detail = out["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("declares no worker block"),
        "the refusal is the class default's own sentence, not an unknown-block one: {out}"
    );
}

/// **The driver watches the pane it resumed** (#1961).
///
/// The measured incident: the hand-back "succeeded" — a pane was opened — and
/// the CLI exited 5.4 seconds later on `Invalid session ID`. Nothing in the
/// driver noticed; the drive sat in `fix-wait` for a whole fix timeout, and the
/// exit notice went to the ORCHESTRATOR, which is the turn the driver exists to
/// remove.
///
/// The live-pane half is the control, and it is the half that fails a fix which
/// simply holds whenever `fix-wait` has a worker pane: same tick, same state,
/// same everything but whether the pane is dead.
#[test]
fn a_resumed_worker_pane_that_exits_without_reporting_holds_the_drive() {
    for kill_it in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::with(WORKFLOW_TWO_WORKERS);
        let gh = FakeGh::green(HEAD_A);
        let (group, _session, _w) = driven_as(&reg, &repo, &gh, "worker-adv");
        let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
        with_pane(&reg, &orch.id, 7201);

        let handed = to_first_handback(&reg, &group, &gh);
        let (_pr, agent) = handed.handbacks.first().cloned().expect("the drive hands back");
        assert_eq!(status_state(&reg, &group), "fix-wait");

        if kill_it {
            with_pane(&reg, &agent, 7202);
            reg.on_pty_exit(7202, Some(1), "Error: Invalid session ID: unknown error", 44, false);
        }

        reg.rd_drive_group_with(&group, &gh, 50_000);
        if !kill_it {
            assert_eq!(
                status_state(&reg, &group),
                "fix-wait",
                "a LIVE resumed pane is a drive that is waiting, not a drive that is stuck — \
                 a fix that held on the mere presence of a worker pane would pass the other \
                 half of this test and break every hand-back there is"
            );
            continue;
        }
        assert_eq!(
            status_state(&reg, &group),
            "held",
            "the pane the driver resumed is gone and nothing can ever arrive from it; \
             waiting out the fix timeout is a whole timeout spent on a dead process"
        );
        let held = audit_details(&reg, &group, "rd-held");
        assert_eq!(held.len(), 1, "one hold: {held:?}");
        assert_eq!(held[0]["reason"], json!("worker-unresumable"));
        let notice = drive_notices(&reg, &group, 1758).join("\n");
        assert!(
            notice.contains("Invalid session ID"),
            "the notice must quote what the pane died saying — that line is the difference \
             between 'find another session' and 'this session cannot be opened by this \
             block's CLI': {notice}"
        );
        assert!(
            notice.contains(&agent),
            "…and name the pane, so the orchestrator can read it: {notice}"
        );
    }
}

// ── #1960: the driver stops spending the cap on its own idle panes ──────────

/// **A cap refusal is its own hold, because it is its own remedy** (#1960).
///
/// Reported as `worker-unresumable`, this hold told the orchestrator *"the
/// recorded worker session no longer resolves, so there is nothing to hand a
/// fix back to"* and pointed it at `drive_review(pr, <another session>)`. The
/// session resolved fine — its `.jsonl` was on disk and the same id re-pointed
/// the drive successfully the moment panes were killed. What was exhausted was
/// a delegate slot, and freeing one is a different action from finding another
/// session.
///
/// The guardrail string is asserted verbatim on the audit row, which is what
/// makes the classifier a pin rather than a decoration: `is_live_cap_refusal`
/// reads the one literal `live_cap_refusal` writes, and this is the test that
/// the message the REAL cap produces is the message it reads.
#[test]
fn a_handback_the_cap_refuses_holds_on_cap_refused_and_names_the_guardrail() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    // Two slots: the worker takes one, the reviewer lane the other, and the
    // hand-back's own pane is the one there is no room for. Exactly the shape
    // the dogfood hit at six, one round and a half into three concurrent
    // drives, with five of the six panes idle.
    let group = reg
        .create_group(&repo.path(), Guardrails { max_agents: 2, ..rails() })
        .unwrap()
        .id;
    let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).unwrap();
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7301);

    let handed = to_first_handback(&reg, &group, &gh);
    assert!(handed.handbacks.is_empty(), "the cap must actually refuse this hand-back");
    assert_eq!(status_state(&reg, &group), "held");

    let refused = audit_details(&reg, &group, "rd-refused");
    let cap = refused
        .iter()
        .find(|d| d["reason"] == json!("cap-refused"))
        .unwrap_or_else(|| panic!("no cap-refused row: {refused:?}"));
    assert!(
        cap["detail"].as_str().unwrap_or_default().contains("live agents already (max 2)"),
        "the row must carry the guardrail's own words — this is what pins the classifier \
         against the message the real cap produces: {cap:?}"
    );
    assert!(
        !refused.iter().any(|d| d["reason"] == json!("worker-unresumable")),
        "a cap refusal must not ALSO be filed as unresumable: {refused:?}"
    );

    let held = audit_details(&reg, &group, "rd-held");
    assert_eq!(held.len(), 1, "one hold: {held:?}");
    assert_eq!(held[0]["reason"], json!("cap-refused"));

    let notice = drive_notices(&reg, &group, 1758).join("\n");
    assert!(
        notice.contains("live-delegate cap") && notice.contains("kill_agent"),
        "the notice must name the guardrail and the action that clears it: {notice}"
    );
    assert!(
        !notice.contains("re-points the drive"),
        "…and must NOT send the orchestrator after a replacement session — that is the \
         one remedy that does not help here: {notice}"
    );
}

/// **Reuse before spawn** (#1960): a hand-back whose worker is idle in a live
/// pane is typed INTO that pane, and opens nothing.
///
/// The driver opened a new pane per resume and released none, so each round
/// cost net +1 or +2 live delegates and three concurrent drives exhausted the
/// cap in a round and a half — on panes the driver itself had opened, five of
/// six idle. The pane reused here is the ORIGINAL worker's, which is the one a
/// release-what-you-superseded design could never have freed: the orchestrator
/// opened it, and §3.1 item 5 does not let the driver kill it.
///
/// The no-pane half is the control and it is the whole discriminator: the two
/// runs differ in exactly one fact (`with_pane`), and `deliver_prompt` refuses
/// an agent with no `pty_id`, so a drive whose worker has no terminal still
/// spawns — which is every other test in this file, unchanged.
#[test]
fn a_handback_resumes_into_the_live_idle_pane_on_that_session() {
    for has_pane in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let group = reg.create_group(&repo.path(), rails()).unwrap().id;
        let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).unwrap();
        let session = w.session_id.clone().expect("claude mints a session id at spawn");
        assert!(
            reg.agent(&w.id).unwrap().idle_since_ms.is_some(),
            "the fixture's premise: a task-less worker is stamped idle at birth, so the \
             only axis this loop varies is whether it has a terminal to type into"
        );
        if has_pane {
            // A pane AND a paused group: `deliver_prompt` withdraws its own
            // admission and answers `Err` when no drainer can be spawned, and a
            // headless test has no `AppHandle` — so without the pause the reuse
            // path falls through to the spawn it is meant to replace, and this
            // test would read as the defect. Same probe #569 built for the
            // orchestrator-delivery tests, pointed at the worker.
            make_delivery_land(&reg, &group, &w.id, 7401);
            // #2089: and a pane the driver will call READY. Since that issue
            // the reuse needs delivery-state evidence as well as a terminal,
            // and this fixture's premise is that the pane is reusable — the
            // axis this loop varies is still `with_pane` alone.
            make_pane_ready(&reg, 7401, true);
        }
        let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
        assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");

        // **Counted across the HAND-BACK tick alone.** Taking the baseline
        // before the whole drive folds in the reviewer lane's own spawn, which
        // is a pane the hand-back neither opens nor reuses — a delta of 1 that
        // reads exactly like the defect. So the drive is walked inline to the
        // tick that hands back, and the baseline is banked at the tick before
        // it. (Same sequence `to_first_handback` performs; not that helper,
        // because the whole point here is to measure inside it.)
        reg.rd_drive_group_with(&group, &gh, 10_000);
        reg.rd_drive_group_with(&group, &gh, 20_000); // lane 0 opens: one spawn
        gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
        gh.set_facts("OPEN", HEAD_B);
        reg.rd_drive_group_with(&group, &gh, 30_000); // review-wait -> ci-wait (arc 6)
        let before = action_count(&reg, &group, "agent-spawn");
        let handed = reg.rd_drive_group_with(&group, &gh, 40_000); // -> fix-wait + hand-back
        let (_pr, agent) = handed.handbacks.first().cloned().expect("the drive hands back");
        let opened = action_count(&reg, &group, "agent-spawn") - before;

        if has_pane {
            assert_eq!(
                agent, w.id,
                "the hand-back must land in the pane already running that session, not a \
                 second pane on the same conversation (#338/#359: two worker panes on one \
                 session share one worktree)"
            );
            assert_eq!(opened, 0, "…and must open no pane at all, which is the cap cost");
            assert!(
                delivered_texts(&reg, &group).iter().any(|t| t.contains("is back with you at head")),
                "the fix brief must actually have been typed into it"
            );
        } else {
            assert_ne!(
                agent, w.id,
                "with no terminal to type into there is nothing to reuse — the driver spawns, \
                 exactly as it always did"
            );
            assert_eq!(opened, 1, "…and that is one new pane: {handed:?}");
        }
    }
}

// ── #2168 E1: a fix push leaves ci-wait on the report, not on green ──────────

/// **#1875's class, through the whole live seam.** The unit tests in
/// the engine's `reviewdrive/` pin `decide`'s answer; this pins that the tick, the
/// interception, the persisted flag and the lane brief compose into the
/// property the issue is about — the lane is briefed **once**, at the digest
/// the receipts left behind, instead of twice.
///
/// The sequence is the forced one #1875 documents. The worker cannot fill the
/// PR body's CI section until the checks have settled, so it lands *after* the
/// green the drive would have briefed on: brief at d1, receipts move the body
/// to d2, `first_stale_lane` reads the `(head, digest)` key, the fresh `pass`
/// is already stale, re-record. Head unmoved, not one line of code changed.
///
/// **What makes this non-vacuous** is the spawn count across the whole
/// sequence. Asserting only "the drive ends in `review-wait` with a lane open"
/// passes just as well under the defect — it opens a lane too, one round
/// earlier and one round more.
#[test]
fn a_fix_push_briefs_its_lane_once_at_the_digest_the_ci_receipts_left() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);

    // Round 1 to the hand-back: lane 0 briefed at HEAD_A, CI then red at HEAD_B.
    let handed = to_first_handback(&reg, &group, &gh);
    let (_pr, worker) = handed.handbacks.first().cloned().expect("the drive hands back");
    with_pane(&reg, &worker, 7503);
    assert_eq!(status_state(&reg, &group), "fix-wait", "the pre-state");

    // The worker pushes its fix. Arc 7 — and this is the arc the whole slice
    // turns on, so the head really has to move.
    gh.set_facts("OPEN", HEAD_C);
    reg.rd_drive_group_with(&group, &gh, 50_000);
    assert_eq!(status_state(&reg, &group), "ci-wait", "arc 7: the worker pushed");

    // The checks settle green. Before #2168 E1 this alone was arc 2.
    gh.set_checks(r#"[{"name":"build","state":"SUCCESS","link":"x"}]"#);
    let spawns_at_green = action_count(&reg, &group, "agent-spawn");
    let green_tick = reg.rd_drive_group_with(&group, &gh, 60_000);
    assert_eq!(
        status_state(&reg, &group),
        "ci-wait",
        "green says the checks settled, not that the round is over — the worker has not \
         reported, so its CI receipts have not landed and a lane briefed now is briefed \
         at a body digest about to move"
    );
    assert!(
        green_tick.lanes_opened.is_empty(),
        "…and no lane was opened on it: {green_tick:?}"
    );
    // **A second tick before the body moves, and it is what makes this test
    // fail under the defect rather than pass by accident.** Arc 2 and the lane
    // brief are different ticks (a lane brief is not a transition, §2.1), so a
    // `decide_ci_wait` that advanced on green alone needs one more tick to open
    // the lane at the OLD digest — and the whole point is that it is open,
    // holding a `pass` slot, when the receipts land below. Without this tick
    // the defect would brief at the new digest too and read green.
    let settling = reg.rd_drive_group_with(&group, &gh, 65_000);
    assert!(
        settling.lanes_opened.is_empty(),
        "still nothing briefed while the worker is reading its own matrix: {settling:?}"
    );

    // What green let the worker do: read the matrix, write the receipts into
    // the PR body, and only then report. This body move is the one that used to
    // stale a pass recorded seconds earlier.
    gh.set_body("b — plus the CI receipts the worker could not write until now");
    let caller = Caller {
        agent_id: worker.clone(),
        group: group.clone(),
        role: Role::Worker,
        role_hint: None,
    };
    dispatch(
        &reg,
        &caller,
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": "done", "note": "fix pushed, matrix green", "ref": "#1758" } }),
    )
    .expect("a driven worker must be able to report done");

    let arc2 = reg.rd_drive_group_with(&group, &gh, 70_000);
    assert_eq!(
        status_state(&reg, &group),
        "review-wait",
        "arc 2 fires once the revision has stopped moving: {arc2:?}"
    );
    let opened = reg.rd_drive_group_with(&group, &gh, 80_000);
    let (_pr, _block, lane) = opened
        .lanes_opened
        .first()
        .cloned()
        .expect("the tick after arc 2 opens the gate's first lane");

    // **The payoff.** The body does not move again, so the lane's brief is
    // still current on the next tick and nothing re-briefs it. Under the defect
    // the lane was opened before `set_body` above and this tick re-opened it.
    let after = reg.rd_drive_group_with(&group, &gh, 90_000);
    assert!(
        after.lanes_opened.is_empty(),
        "the lane was briefed at the digest the receipts left, so there is nothing to \
         re-brief: {after:?}"
    );
    assert_eq!(
        action_count(&reg, &group, "agent-spawn") - spawns_at_green,
        1,
        "exactly one reviewer pane for round 2 — the re-record round #1875 measures is a \
         second one, and counting them is what makes this test fail under the defect \
         rather than merely arrive at the same end state"
    );

    // And the brief that lane got names the revision the receipts are part of,
    // not the one the drive saw go green.
    let brief = lane_brief(&reg, &lane);
    assert!(
        brief.contains(HEAD_C),
        "the delta names the head the worker pushed: {brief}"
    );
}

// ── #1959: a worker's progress report in fix-wait is answered, not swallowed ─

/// Drive to a hand-back and have the worker `report` `outcome` through the real
/// MCP arm, exactly as a driven worker does. Answers the pane it reported from.
fn handback_then_report(
    reg: &OrchRegistry,
    group: &GroupId,
    gh: &FakeGh,
    outcome: &str,
) -> String {
    let handed = to_first_handback(reg, group, gh);
    let (_pr, worker) = handed.handbacks.first().cloned().expect("the drive hands back");
    // A pane is all this one needs: `deliver_prompt` writes its `prompt` audit
    // line BEFORE it decides whether it can actually paste, so `delivered_texts`
    // sees every delivery ATTEMPTED at a live pane — which is what the recipient
    // assertions below read, and what keeps them non-vacuous with no pause.
    with_pane(reg, &worker, 7502);
    let caller = Caller {
        agent_id: worker.clone(),
        group: group.clone(),
        role: Role::Worker,
        role_hint: None,
    };
    dispatch(
        reg,
        &caller,
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": outcome, "note": "for re-review round 2", "ref": "#1758" } }),
    )
    .unwrap_or_else(|e| panic!("a driven worker must be able to report ({outcome}): {e:?}"));
    worker
}

/// **#1959.** A `report(progress)` in `fix-wait` was consumed and then dropped:
/// the audit log carries the `rd-consumed report:worker` row, and the drive did
/// nothing for ten minutes until the idle watchdog woke the ORCHESTRATOR — the
/// turn the driver exists to remove.
///
/// It is answered in the WORKER's own pane instead. Three things are pinned
/// together because any one alone would pass under a wrong fix:
///
/// - the drive does **not** move (a progress report is not a fix signal, and
///   reading it as one would brief a reviewer over unfinished work);
/// - the worker's pane gets the line;
/// - the orchestrator's pane gets nothing, which is the whole saving.
///
/// The `done` half is the control, and it is the one that fails a "kick back on
/// any worker report" implementation: same state, same pane, same tool call,
/// one different word — and `done` must still take arc 8 with no line typed at
/// anybody.
#[test]
fn a_workers_progress_report_in_fix_wait_is_answered_in_its_own_pane() {
    for outcome in ["progress", "done"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, _session) = driven(&reg, &repo, &gh);
        let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
        with_pane(&reg, &orch.id, 7501);

        let worker = handback_then_report(&reg, &group, &gh, outcome);
        assert_eq!(status_state(&reg, &group), "fix-wait", "the pre-state for {outcome}");

        let tick = reg.rd_drive_group_with(&group, &gh, 50_000);

        if outcome == "done" {
            assert_eq!(
                status_state(&reg, &group),
                "review-wait",
                "the control: report(done) at an unchanged head is arc 8 and still is"
            );
            assert!(
                tick.kickbacks.is_empty(),
                "…and a worker that used the right word is told nothing: {tick:?}"
            );
            assert_eq!(action_count(&reg, &group, "rd-kickback"), 0);
            continue;
        }

        assert_eq!(
            status_state(&reg, &group),
            "fix-wait",
            "a progress report must NOT advance the drive — arc 8 is report(done), and \
             briefing a reviewer on 'still going' reviews unfinished work"
        );
        assert_eq!(
            tick.kickbacks,
            vec![(1758, worker.clone())],
            "the worker's own pane is where the answer goes: {tick:?}"
        );
        let kick = audit_details(&reg, &group, "rd-kickback");
        assert_eq!(kick.len(), 1, "one kick-back on the record: {kick:?}");
        assert_eq!(kick[0]["agent"], json!(worker));

        let typed: Vec<String> = delivered_texts(&reg, &group)
            .into_iter()
            .filter(|t| t.contains("this drive advances on report"))
            .collect();
        assert_eq!(typed.len(), 1, "exactly one line typed: {typed:?}");
        assert!(
            typed[0].contains("report(outcome=done, ref=#1758)"),
            "it must name the call that advances the drive: {}",
            typed[0]
        );
        assert!(
            typed[0].contains("nothing to push"),
            "…and the head-unchanged case, which is the round that produced the stall: {}",
            typed[0]
        );

        // §7's whole point: this costs the orchestrator no turn. Read by
        // RECIPIENT, never by text — `drive_notices` matches on the "review
        // drive PR #N:" prefix the kick-back shares with every drive notice, so
        // it finds this line wherever it went, which is the opposite of the
        // question being asked.
        assert!(
            !texts_to(&reg, &group, &orch.id).iter().any(|t| t.contains("advances on report")),
            "the kick-back must not reach the orchestrator's pane — the watchdog waking it \
             is the turn this exists to remove"
        );
        assert!(
            texts_to(&reg, &group, &worker).iter().any(|t| t.contains("advances on report")),
            "…and the control for that: it DID go to the worker's pane, so the assertion \
             above is about the recipient rather than about a line nobody sent"
        );
    }
}

/// **One per hand-back, not one per report** (#1959).
///
/// A kick-back is an EMISSION driven by a signal the drive does not control, so
/// it needs a bound for the mirror of the reason a suppression does. A worker
/// that reports progress three times in one fix round is answered once; the
/// budget renews on the next hand-back with nothing having to reset it, because
/// `fix_kickback_ms < fix_handback_ms` *is* the budget.
///
/// The second half is the control for the first: a test that only pinned "at
/// most one" would pass just as well under a fix that emitted at most one EVER.
#[test]
fn the_kick_back_is_bounded_to_one_per_handback_and_renews_on_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _session) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7601);

    handback_then_report(&reg, &group, &gh, "progress");
    assert_eq!(reg.rd_drive_group_with(&group, &gh, 50_000).kickbacks.len(), 1);
    // Two more ticks with the progress signal still standing — it is cleared
    // only by an arc, so this is the real "it keeps sitting there" shape.
    for now in [60_000, 70_000] {
        assert!(
            reg.rd_drive_group_with(&group, &gh, now).kickbacks.is_empty(),
            "a second kick-back for one hand-back: a worker that reports progress in a loop \
             would be answered in a loop"
        );
    }
    assert_eq!(action_count(&reg, &group, "rd-kickback"), 1);

    // A NEW hand-back renews it. The worker pushes (arc 7 to `ci-wait`), the
    // checks are red at the new head (arc 3 back to `fix-wait`), and it reports
    // progress again.
    gh.set_facts("OPEN", HEAD_A);
    gh.set_checks(r#"[{"name":"build","state":"SUCCESS","link":"x"}]"#);
    reg.rd_drive_group_with(&group, &gh, 80_000); // fix-wait -> ci-wait (arc 7: the head moved)
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    let again = reg.rd_drive_group_with(&group, &gh, 90_000); // ci-wait -> fix-wait (arc 3)
    let (_pr, worker2) = again
        .handbacks
        .first()
        .cloned()
        .unwrap_or_else(|| panic!("the second hand-back must happen: {again:?}"));
    assert_eq!(status_state(&reg, &group), "fix-wait");

    // **From the CURRENT pane.** The first hand-back's pane is superseded now,
    // and §7 consumes a superseded pane's report without feeding it in — a
    // claim about a revision the drive has moved past. Reporting from it here
    // would leave this half green for the wrong reason.
    let caller = Caller {
        agent_id: worker2.clone(),
        group: group.clone(),
        role: Role::Worker,
        role_hint: None,
    };
    dispatch(
        &reg,
        &caller,
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": "progress", "note": "still on it", "ref": "#1758" } }),
    )
    .expect("the worker reports progress again");
    assert_eq!(
        reg.rd_drive_group_with(&group, &gh, 100_000).kickbacks.len(),
        1,
        "the budget must RENEW on the next hand-back — a bound that never renewed would \
         satisfy the first half of this test and answer nobody for the rest of the drive"
    );
    assert_eq!(action_count(&reg, &group, "rd-kickback"), 2);
}

/// A roster whose required reviewer lane is **not** the default reviewer block
/// (#1961), so a lane RESUME that named no block is visible.
///
/// `rev-std` is declared first and is therefore `block_for(Reviewer)`'s answer;
/// the gate requires `rev-final`. Under the one-reviewer fixture the two are
/// the same string and the lane half of #1961 is unfalsifiable, exactly as the
/// worker half was.
const WORKFLOW_TWO_REVIEWERS: &str = r#"version: 1
blocks:
  - id: worker
    kind: worker
  - id: rev-std
    name: Standard review
    kind: reviewer
  - id: rev-final
    name: Final review
    kind: reviewer
gates:
  merge:
    require: all-pass
    reviewers: [rev-final]
    routing:
      - paths: [src/**]
        reviewers: [rev-final]
merge_queue:
  enabled: true
driver:
  enabled: true
"#;

/// **The lane half of #1961's root cause.** `rd_open_lane`'s RESUME arm passed
/// `block: None` too, so a re-briefed lane came back as the roster's default
/// reviewer block: a `rev-final` lane resumed as `rev-std`, on that block's CLI
/// and persona, still filed under `rev-final` and still expected to record
/// `rev-final`'s verdict.
///
/// The first spawn is the control and it always passed the block, so a run
/// where only the first assertion holds says the resume is still guessing.
#[test]
fn a_lane_re_brief_resumes_under_that_lanes_own_block() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::with(WORKFLOW_TWO_REVIEWERS);
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    reg.rd_drive_group_with(&group, &gh, 10_000);
    let first = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, block, lane) = first.lanes_opened.first().cloned().expect("lane 0 opens");
    assert_eq!(block, "rev-final", "the fixture's premise: the gate requires the NON-default lane");
    assert_eq!(reg.agent(&lane).unwrap().block, "rev-final", "the control: a FRESH lane spawn already named its block");

    // The lane answers, then the head moves under it — which stales the pass and
    // sends the drive back round to re-brief the SAME lane, by resume.
    let caller =
        Caller { agent_id: lane.clone(), group: group.clone(), role: Role::Reviewer, role_hint: None };
    dispatch(
        &reg,
        &caller,
        "tools/call",
        &json!({ "name": "review_verdict", "arguments": {
            "pr": "1758", "verdict": "pass", "summary": "pass - nothing blocking" } }),
    )
    .expect("the lane records");
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 30_000); // review-wait -> ci-wait (arc 6)
    reg.rd_drive_group_with(&group, &gh, 40_000); // ci-wait -> review-wait (arc 2)
    let again = reg.rd_drive_group_with(&group, &gh, 50_000); // re-brief, by resume
    let (_pr, _b, lane2) =
        again.lanes_opened.first().cloned().expect("the stale lane is re-briefed");
    assert_ne!(lane2, lane, "the re-brief opened a pane, which is what carries the block");
    assert_eq!(
        reg.agent(&lane2).unwrap().block,
        "rev-final",
        "the RESUME dropped the lane's block and fell through to the roster's default \
         reviewer — a different persona and CLI, still filed under rev-final"
    );
}

/// **A session this group has no record of refuses rather than defaults**
/// (#1961). `drive_review` accepts a well-shaped session id it never recorded
/// (§5.1: `resolve_session_ref`'s passthrough arm, and resolving is not proving
/// resumable), so the hand-back is where that is learned — and what it must not
/// do there is invent a capability class, which is #544's rule reaching the
/// driver. The pre-#1961 code defaulted to the roster's worker block and opened
/// a pane; a pane opened under a guessed class is exactly what produced the
/// dead worker this issue is about.
///
/// Its own test rather than a corollary of the stale-block one: that refusal
/// comes from `spawn_agent_bound`'s roster check, this one from the driver
/// declining to ask.
#[test]
fn a_handback_for_a_session_this_group_never_recorded_refuses_rather_than_guessing() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7701);

    // A full, well-shaped session id this roster has never seen.
    let stranger = "deadbeef-0000-4000-8000-000000000000";
    let out = reg.drive_review_with(&group, &gh, 1758, stranger, false, 0, "orch-1", 0);
    assert_eq!(
        out["driving"],
        json!(true),
        "the premise: drive_review ACCEPTS it — §5.1 says resolving is not proving \
         resumable, and this is the deferral that makes: {out}"
    );

    let handed = to_first_handback(&reg, &group, &gh);
    assert!(handed.handbacks.is_empty(), "no pane may be opened on a guessed class");
    assert_eq!(status_state(&reg, &group), "held");
    let held = audit_details(&reg, &group, "rd-held");
    assert_eq!(held[0]["reason"], json!("worker-unresumable"), "{held:?}");
    let refusal = held[0]["refusal"].as_str().unwrap_or_default();
    assert!(
        refusal.contains("refusing to guess"),
        "the row must say it DECLINED to pick a class, not that something failed: \
         {refusal:?}"
    );
}

/// **`driver-fix.md` names the report the drive advances on** (#1959), read off
/// the brief a real hand-back typed rather than off the template source — the
/// §5.5 rule that a pin must exercise the live render path.
///
/// The old sentence was *"Address it, push, and report when the checks are
/// green"*, which has no trigger at all for a body-only fix: nothing to push,
/// no new checks. A literal reader picks `progress`, and that is the round that
/// stalled for ten minutes.
#[test]
fn the_fix_brief_names_the_report_that_advances_the_drive() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    let handed = to_first_handback(&reg, &group, &gh);
    let (_pr, worker) = handed.handbacks.first().cloned().expect("the drive hands back");
    let brief = lane_brief(&reg, &worker);

    assert!(
        brief.contains("report(outcome=done, ref=#1758)"),
        "the brief must name the exact call, with this PR's number: {brief}"
    );
    assert!(
        brief.contains("If it did not move the head"),
        "…and the head-unchanged case, which the old wording had no trigger for: {brief}"
    );
    assert!(
        !brief.contains("report when the checks are green. This is attempt"),
        "the retracted sentence is what a literal reader answered with progress: {brief}"
    );
    // The fix brief is ONE paragraph per sentence, like the lane brief — a
    // rendered `\n` plus source indentation would ship the template's own
    // wrapping into a worker's pane.
    for line in brief.lines() {
        assert!(
            !line.starts_with(' '),
            "a rendered brief line must not be indented: {line:?}"
        );
    }
}

// ── review round 1: rev-final B3 and N1 ─────────────────────────────────────

/// **rev-final B3.** A live idle pane on the session is reused only if it is
/// running the block the hand-back resolved — and the pane that makes this a
/// defect rather than a hypothetical is one the PRE-#1961 driver minted.
///
/// The old `rd_handback` opened a DEFAULT-block pane on a non-default session.
/// Where the two blocks share a CLI — `worker` and `worker-adv` here, both
/// claude — that pane did not die on `Invalid session ID`: it opened fine, went
/// idle, and is still sitting there. Reusing it hands the fix to the wrong
/// persona on the wrong model, with `rd_resume_block` never consulted — #1961's
/// own defect, arriving through the mechanism added to fix #1960. The same pane
/// is reachable with no legacy at all, since
/// `spawn_agent(block:, resume_session:)` is permitted and skips inheritance.
///
/// **The two arms carry different halves, and neither is the other's control.**
/// The wrong-block pane is live, idle and typeable in both; what varies is
/// whether the RIGHT-block pane is typeable too.
///
/// - **Arm 1 (`right_block_typeable == false`)** — the wrong-block pane is the
///   ONLY candidate `idle_pane_on_session` could return, since the right-block
///   pane has no `pty_id`. It is refused anyway and the hand-back opens a pane
///   (`opened == 1`). **This is the arm that carries the red**: the block filter
///   is the only thing standing between the fix and the wrong persona.
/// - **Arm 2 (`right_block_typeable == true`)** — both are candidates and the
///   wrong-block one is NEWER, so `max_by_key(started_ms)` would prefer it; the
///   right-block pane is reused all the same, and nothing is opened. This is
///   what stops "refuse everything" from passing arm 1, and it is the only arm
///   in which the ordering is exercised at all.
///
/// Stated this way because the earlier wording claimed both panes were typeable
/// in both arms and gave recency as the mechanism arm 1 defeats — which is not
/// what arm 1 does, and a maintainer acting on it could delete the conditional
/// that creates the red-carrying arm (rev-final round 2 B1).
#[test]
fn a_live_idle_pane_on_the_wrong_block_is_not_reused_for_a_handback() {
    for right_block_typeable in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::with(WORKFLOW_TWO_WORKERS);
        let gh = FakeGh::green(HEAD_A);
        let (group, session, worker) = driven_as(&reg, &repo, &gh, "worker-adv");

        // The pane the pre-#1961 driver would have left behind: same session,
        // DEFAULT block, alive, idle, and with a terminal to type into.
        let wrong = reg
            .spawn_agent_ex(
                &group, Role::Worker, Some("worker".into()), "w-wrong", "", false, None, None,
                Some(session.clone()), Some(reg.agent(&worker).unwrap().cwd), None,
            )
            .expect("a default-block pane on a non-default session");
        assert_eq!(
            (reg.agent(&worker).unwrap().block, reg.agent(&wrong.id).unwrap().block),
            ("worker-adv".to_string(), "worker".to_string()),
            "the fixture's premise: two live panes on ONE session, different blocks"
        );
        make_delivery_land(&reg, &group, &wrong.id, 7801);
        // #2089: BOTH panes are made delivery-ready, so readiness is constant
        // across the arms and the block filter stays the only axis. Leaving the
        // wrong-block pane unready would let this test pass on the readiness
        // refusal instead of on the thing it is named for.
        make_pane_ready(&reg, 7801, true);
        if right_block_typeable {
            with_pane(&reg, &worker, 7802);
            make_pane_ready(&reg, 7802, true);
        }
        for id in [&worker, &wrong.id] {
            assert!(
                reg.agent(id).unwrap().idle_since_ms.is_some(),
                "both panes must be idle, or the block filter is not the axis under test"
            );
        }

        reg.rd_drive_group_with(&group, &gh, 10_000);
        reg.rd_drive_group_with(&group, &gh, 20_000);
        gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
        gh.set_facts("OPEN", HEAD_B);
        reg.rd_drive_group_with(&group, &gh, 30_000);
        let before = action_count(&reg, &group, "agent-spawn");
        let handed = reg.rd_drive_group_with(&group, &gh, 40_000);
        let (_pr, agent) = handed.handbacks.first().cloned().expect("the drive hands back");
        let opened = action_count(&reg, &group, "agent-spawn") - before;

        assert_ne!(
            agent, wrong.id,
            "the hand-back reused a pane running the WRONG block — that is #1961's defect \
             reached through #1960's mechanism: the wrong persona, the wrong model, and \
             rd_resume_block never consulted"
        );
        assert!(
            !texts_to(&reg, &group, &wrong.id).iter().any(|t| t.contains("is back with you")),
            "…and no fix brief may be typed into it either"
        );
        assert_eq!(
            reg.agent(&agent).unwrap().block,
            "worker-adv",
            "whichever pane took the hand-back, it runs the session's own block"
        );

        if right_block_typeable {
            assert_eq!(
                agent, worker,
                "the control: with the RIGHT-block pane typeable the hand-back reuses IT, \
                 even though the wrong-block pane is newer and would win on recency alone"
            );
            assert_eq!(opened, 0, "…opening nothing, which is #1960's whole point");
        } else {
            assert_eq!(
                opened, 1,
                "with no eligible pane the hand-back opens one, rather than settling for \
                 the wrong-block pane sitting in front of it"
            );
        }
    }
}

/// **rev-final N1.** The `cap` field on a LANE's `rd-refused` row — a documented
/// telemetry contract (§8: "so a reader can tell a capped lane … from a broken
/// one") that no assertion reached.
///
/// The negative control is the discriminator and is a real refusal rather than a
/// contrived one: the spawn-rate backstop refuses in exactly the same place with
/// a different sentence, so a `cap` hard-coded true, or read off "did the spawn
/// fail", passes the first half and fails here.
#[test]
fn a_lane_spawn_refused_by_the_cap_says_so_on_its_audit_row() {
    for capped in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        // One slot, or one spawn an hour: either way the WORKER takes it and the
        // reviewer lane is the spawn that gets refused.
        let rails = if capped {
            Guardrails { max_agents: 1, ..rails() }
        } else {
            Guardrails { max_spawns_per_hour: 1, ..rails() }
        };
        let group = reg.create_group(&repo.path(), rails).unwrap().id;
        let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).unwrap();
        let session = w.session_id.clone().expect("claude mints a session id at spawn");
        let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
        assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");

        reg.rd_drive_group_with(&group, &gh, 10_000);
        let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
        assert!(opened.lanes_opened.is_empty(), "the lane spawn must actually be refused");

        let rows = audit_details(&reg, &group, "rd-refused");
        let lane = rows
            .iter()
            .find(|d| d["reason"] == json!("lane-spawn-refused"))
            .unwrap_or_else(|| panic!("no lane-spawn-refused row: {rows:?}"));
        assert_eq!(
            lane["cap"],
            json!(capped),
            "the row must say whether the LIVE-DELEGATE CAP is what refused — a capped lane \
             retries and clears itself, a rate-limited or broken one does not: {lane:?}"
        );
        // …and either way the lane refusal is a back-off, never a hold: §8's
        // asymmetry with `fix-wait`, which has already spent its round.
        assert_eq!(status_state(&reg, &group), "review-wait");
    }
}
