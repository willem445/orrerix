//! #3367: nit-only gates, auto-start from a worker's done report, `open_findings`, and the merge queue.
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── #3367 items 1 and 2, through the real tick and the real MCP arm ──────────

/// `WORKFLOW` with one extra line in its `driver:` block — the one axis the
/// #3367 tests vary. The stock fixture ends inside that block, so an append is
/// exactly an extra key there.
fn workflow_with(line: &str) -> String {
    format!("{WORKFLOW}  {line}\n")
}

/// Record a verdict with a chosen SUMMARY through the real MCP arm — the one
/// input the non-blocking round reads that `record_pass_for` leaves generic.
fn record_verdict_as(reg: &OrchRegistry, group: &GroupId, lane: &str, verdict: &str, summary: &str) {
    dispatch(
        reg,
        &Caller {
            agent_id: lane.to_string(),
            group: group.clone(),
            role: Role::Reviewer,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "review_verdict", "arguments": {
            "pr": "1758", "verdict": verdict, "summary": summary } }),
    )
    .unwrap_or_else(|e| panic!("{lane} could not record {verdict}: {e:?}"));
}

/// Everything a worker pane was told: its kickoff task (a spawned hand-back)
/// and every prompt typed into it (a reused or taken-over one).
fn told(reg: &OrchRegistry, group: &GroupId, agent: &str) -> String {
    let mut all = reg.agent(agent).map(|a| lf(&a.task)).unwrap_or_default();
    for t in texts_to(reg, group, agent) {
        all.push('\n');
        all.push_str(&lf(&t));
    }
    all
}

/// Tick until the drive's state is `want`, answering the clock of the tick that
/// got it there and every report along the way. Bounded and asserting.
fn tick_until(
    reg: &OrchRegistry,
    group: &GroupId,
    gh: &FakeGh,
    from_ms: u64,
    want: &str,
) -> (u64, Vec<RdDriveReport>) {
    let mut at = from_ms;
    let mut reports = Vec::new();
    for _ in 0..8 {
        reports.push(reg.rd_drive_group_with(group, gh, at));
        if status_state(reg, group) == want {
            return (at, reports);
        }
        at += 10_000;
    }
    panic!("the drive never reached {want}; it is at {}", status_state(reg, group));
}

/// **#3367 item 1, end to end: the driver hands a nit-only satisfied gate back
/// itself, re-reviews, and wakes the orchestrator once — with the residual.**
///
/// Round one: the only lane passes stating `0 blocking; 2 non-blocking`. With
/// `fix_nonblocking_rounds: 1` the gate-check does NOT satisfy: it spends a
/// review round, writes `rd-auto-handback`, and hands the worker a brief that
/// says every lane PASSED (not that one recorded FAIL). The worker pushes and
/// reports; the lane is re-briefed at the new head and passes again with one
/// nit left. The policy's one round is spent, so THIS gate-check satisfies, and
/// the one GATE SATISFIED line carries `1/1` and the residual it decided on.
#[test]
fn a_nit_only_satisfied_gate_is_handed_back_by_the_driver_and_wakes_once_with_the_residual() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::with(&workflow_with("fix_nonblocking_rounds: 1"));
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    reg.rd_drive_group_with(&group, &gh, 10_000); // ci-wait -> review-wait
    let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, _b, lane) = opened.lanes_opened.first().cloned().expect("lane 0 opens");
    record_verdict_as(&reg, &group, &lane, "pass", "Clean. 0 blocking; 2 non-blocking: a, b");

    let (at, reports) = tick_until(&reg, &group, &gh, 30_000, "fix-wait");
    let handbacks: Vec<(u64, String)> = reports.iter().flat_map(|r| r.handbacks.clone()).collect();
    assert_eq!(handbacks.len(), 1, "one hand-back, taken by the driver itself: {handbacks:?}");
    assert!(
        reports.iter().all(|r| r.notices.iter().all(|n| !n.contains("GATE SATISFIED"))),
        "no GATE SATISFIED on the round the driver handled — that wake is what it removes"
    );
    assert_eq!(review_rounds(&reg, &group), 1, "the round is charged to the SHARED bound");
    let rows = audit_details(&reg, &group, "rd-auto-handback");
    assert_eq!(rows.len(), 1, "one rd-auto-handback row: {rows:?}");
    assert_eq!(rows[0]["round"], json!(1));
    assert_eq!(rows[0]["of"], json!(1));
    assert_eq!(rows[0]["residual"], json!("rev-std 0 blocking / 2 non-blocking"));
    let worker = handbacks[0].1.clone();
    let brief = told(&reg, &group, &worker);
    assert!(brief.contains("Every required lane PASSED"), "the brief says what happened: {brief}");
    assert!(!brief.contains("recorded FAIL"), "…and not a FAIL nobody recorded: {brief}");

    // The worker pushes the fixes and reports; the lane re-reviews at the new head.
    with_pane(&reg, &worker, 7101);
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, at + 10_000); // arc 7
    report_as(&reg, &group, &worker, Role::Worker, "done");
    let (at, reports) = tick_until(&reg, &group, &gh, at + 20_000, "review-wait");
    let lane2 = reports
        .iter()
        .flat_map(|r| r.lanes_opened.clone())
        .next()
        .map(|(_, _, a)| a)
        .unwrap_or_else(|| {
            let r = reg.rd_drive_group_with(&group, &gh, at + 10_000);
            r.lanes_opened.first().cloned().expect("the lane is re-briefed at HEAD_B").2
        });
    record_verdict_as(&reg, &group, &lane2, "pass", "Round 2. 0 blocking; 1 non-blocking left");

    let mut satisfied = None;
    let mut t = at + 20_000;
    for _ in 0..6 {
        let r = reg.rd_drive_group_with(&group, &gh, t);
        if let Some(n) = r.notices.iter().find(|n| n.contains("GATE SATISFIED")) {
            satisfied = Some(n.clone());
            break;
        }
        t += 10_000;
    }
    let notice = satisfied.expect("the spent policy satisfies on the second nit-only gate");
    assert!(
        notice.contains(
            "Non-blocking rounds run by the driver: 1/1; residual: rev-std 0 blocking / 1 non-blocking."
        ),
        "the final notice carries the rounds used and the residual: {notice}"
    );
    assert_eq!(action_count(&reg, &group, "rd-auto-handback"), 1, "N=1 means one round, never two");
    // Read off the notice, because a terminal drive is no longer listed by the
    // status view `review_rounds` reads.
    assert!(notice.contains("; 1 rounds, "), "the second gate spent nothing: {notice}");
}

/// **The default is today's behaviour through the seam**: the same nit-only
/// pass satisfies on the first gate-check and the notice gains no clause. The
/// control for the test above — without it, that test's hand-back could be the
/// driver doing it for every repo.
#[test]
fn without_the_key_a_nit_only_satisfied_gate_wakes_the_orchestrator_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    reg.rd_drive_group_with(&group, &gh, 10_000);
    let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, _b, lane) = opened.lanes_opened.first().cloned().expect("lane 0 opens");
    record_verdict_as(&reg, &group, &lane, "pass", "Clean. 0 blocking; 2 non-blocking: a, b");
    let mut notice = None;
    let mut t = 30_000;
    for _ in 0..6 {
        let r = reg.rd_drive_group_with(&group, &gh, t);
        if let Some(n) = r.notices.iter().find(|n| n.contains("GATE SATISFIED")) {
            notice = Some(n.clone());
            break;
        }
        t += 10_000;
    }
    let notice = notice.expect("with the key absent the gate satisfies at once");
    assert!(!notice.contains("Non-blocking rounds"), "the stock line is unchanged: {notice}");
    assert_eq!(action_count(&reg, &group, "rd-auto-handback"), 0);
    assert!(notice.contains("; 0 rounds, "), "no round was spent: {notice}");
}

/// A group whose worker spawned on `branch`, an orchestrator whose deliveries
/// land, and the fake `gh` installed as the registry's runner — the setup every
/// auto-start test shares. Answers `(group, worker id, orchestrator id)`.
fn auto_start_group(
    reg: &OrchRegistry,
    repo: &Repo,
    gh: &std::sync::Arc<FakeGh>,
    branch: &str,
) -> (GroupId, String, String) {
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    make_delivery_land(reg, &group, &orch.id, 7201);
    let w = reg
        .spawn_agent(&group, Role::Worker, "w", "", false, Some(branch.to_string()))
        .expect("a worker on its own branch");
    assert_eq!(w.branch.as_deref(), Some(branch), "the fixture's premise: the branch is recorded");
    let runner: std::sync::Arc<dyn RdRunner> = gh.clone();
    reg.set_rd_runner_override(Some(runner));
    (group, w.id, orch.id)
}

fn report_done(reg: &OrchRegistry, group: &GroupId, agent: &str, pr_ref: &str) -> String {
    dispatch(
        reg,
        &Caller { agent_id: agent.to_string(), group: group.clone(), role: Role::Worker, role_hint: None },
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": "done", "note": "ready for review, CI green", "ref": pr_ref } }),
    )
    .map(|v| v.to_string())
    .unwrap_or_else(|e| panic!("{agent} could not report: {e:?}"))
}

/// **#3367 item 2: a worker's done on its own PR starts the drive, and the
/// report rides in the drive's FIRST notice — and only the first.**
#[test]
fn a_workers_done_on_its_own_pr_starts_a_drive_and_its_first_notice_carries_the_report() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::with(&workflow_with("auto_drive_on_done: true"));
    let gh = std::sync::Arc::new(FakeGh::green(HEAD_A));
    let (group, worker, orch) = auto_start_group(&reg, &repo, &gh, AUTHOR_BRANCH);

    let reply = report_done(&reg, &group, &worker, "#1758");
    assert!(reply.contains("started a review drive"), "the worker is told what happened: {reply}");
    assert_eq!(status_state(&reg, &group), "ci-wait", "a drive exists on PR 1758");
    assert!(
        texts_to(&reg, &group, &orch).iter().all(|t| !t.contains("ready for review")),
        "the report did NOT wake the orchestrator on its own"
    );
    let started = audit_details(&reg, &group, "rd-auto-started");
    assert_eq!(started.len(), 1, "one rd-auto-started row: {started:?}");
    assert_eq!(started[0]["branch"], json!(AUTHOR_BRANCH));
    assert_eq!(
        drives_json(&reg, &group)["entries"][0]["worker_session"],
        json!(reg.agent(&worker).unwrap().session_id.unwrap()),
        "the drive hands fixes back to the REPORTING worker's session"
    );

    // The PR closes, so the drive's first (terminal) notice is owed now.
    gh.set_facts("CLOSED", HEAD_A);
    let end = reg.rd_drive_group_with(&group, &*gh, 10_000);
    let first = end
        .notices
        .iter()
        .find(|n| n.contains("review drive PR #1758"))
        .unwrap_or_else(|| panic!("the drive's notice: {:?}", end.notices));
    assert!(
        first.contains("started by a worker's report(done)") && first.contains("ready for review"),
        "the first notice carries the report: {first}"
    );
    assert!(!first.contains("] [orrerix]"), "no nested marker: {first}");
    assert_eq!(drives_json(&reg, &group)["entries"].as_array().map_or(0, |a| a
        .iter()
        .filter(|e| e.get("auto_report").is_some())
        .count()), 0, "taken, so no later notice can carry it twice");
}

/// **Every refusal the brief names — and the policy-off control — delivers the
/// report as today, and each refusal says why on `rd-auto-start-declined`.**
///
/// One registry per case, because each varies exactly one input from the
/// accepted case above: the title, the branch, the ref, a recorded verdict, an
/// existing drive, and the key itself.
#[test]
fn a_refused_auto_start_delivers_the_report_as_before_and_names_its_reason() {
    #[derive(Clone, Copy)]
    enum Case {
        Scratch,
        NotAuthor,
        NotAPr,
        HasVerdicts,
        AlreadyDriven,
        PrNotOpen,
        PolicyOff,
    }
    let cases = [
        (Case::Scratch, Some("scratch")),
        (Case::NotAuthor, Some("not-author")),
        (Case::NotAPr, Some("not-a-pr")),
        (Case::HasVerdicts, Some("has-verdicts")),
        (Case::AlreadyDriven, Some("already-driven")),
        (Case::PrNotOpen, Some("pr-not-open")),
        (Case::PolicyOff, None),
    ];
    let mut verified = 0;
    for (case, want) in cases {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let yaml = match case {
            Case::PolicyOff => WORKFLOW.to_string(),
            _ => workflow_with("auto_drive_on_done: true"),
        };
        let repo = Repo::with(&yaml);
        let gh = std::sync::Arc::new(FakeGh::green(HEAD_A));
        let branch = match case {
            Case::NotAuthor => "feat/someone-else",
            _ => AUTHOR_BRANCH,
        };
        let (group, worker, orch) = auto_start_group(&reg, &repo, &gh, branch);
        let mut pr_ref = "#1758";
        match case {
            Case::Scratch => gh.set_identity(AUTHOR_BRANCH, "[scratch] red evidence for #3367"),
            Case::NotAPr => pr_ref = "the PR",
            Case::PrNotOpen => gh.set_facts("MERGED", HEAD_A),
            Case::HasVerdicts => {
                reg.set_pr_head_override(Some(HEAD_A.to_string()));
                let rev = reg.spawn_agent(&group, Role::Reviewer, "rev-std", "", false, None).unwrap();
                record_verdict_as(&reg, &group, &rev.id, "pass", "0 blocking");
            }
            Case::AlreadyDriven => {
                let session = reg.agent(&worker).unwrap().session_id.unwrap();
                let out =
                    reg.drive_review_with(&group, &*gh, 1758, &session, false, 0, &orch, 0);
                assert_eq!(out["driving"], json!(true), "the premise: a drive exists: {out}");
            }
            _ => {}
        }
        // The status view rather than the file: before any drive exists there
        // is no `review_drives.json` for `drives_json` to read.
        let live_drives = |reg: &OrchRegistry| {
            reg.review_drive_status(&group)["drives"].as_array().map_or(0, |a| a.len())
        };
        let drives_before = live_drives(&reg);
        report_done(&reg, &group, &worker, pr_ref);
        assert!(
            texts_to(&reg, &group, &orch).iter().any(|t| t.contains("ready for review")),
            "the report reached the orchestrator exactly as before"
        );
        assert_eq!(live_drives(&reg), drives_before, "no drive was started");
        let rows = audit_details(&reg, &group, "rd-auto-start-declined");
        match want {
            Some(reason) => {
                assert_eq!(rows.len(), 1, "one declined row: {rows:?}");
                assert_eq!(rows[0]["reason"], json!(reason));
            }
            None => assert!(rows.is_empty(), "the policy off audits nothing: {rows:?}"),
        }
        assert_eq!(action_count(&reg, &group, "rd-auto-started"), 0);
        reg.set_rd_runner_override(None);
        verified += 1;
    }
    assert_eq!(verified, cases.len(), "every case ran");
}

/// **The report an auto-start persists is bounded** (rev-std premortem on
/// #3371). The legacy `summary` shape of `report` is scrubbed but uncapped, and
/// the text is written onto the drive entry — rewritten whole on every drive
/// write for the drive's life — and into the `rd-auto-started` row. A 40,000-
/// character summary must reach both bounded and on one line, and the drive
/// must still start: the cap trims the record, it never refuses the report.
#[test]
fn an_auto_started_report_is_persisted_capped_and_on_one_line() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::with(&workflow_with("auto_drive_on_done: true"));
    let gh = std::sync::Arc::new(FakeGh::green(HEAD_A));
    let (group, worker, _orch) = auto_start_group(&reg, &repo, &gh, AUTHOR_BRANCH);
    let huge = "line one of a long report\n".repeat(1_600);
    assert!(huge.len() >= 40_000, "the fixture really is large: {}", huge.len());
    dispatch(
        &reg,
        &Caller { agent_id: worker.clone(), group: group.clone(), role: Role::Worker, role_hint: None },
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "status": "done", "summary": huge, "ref": "#1758" } }),
    )
    .expect("a legacy-shape report is accepted");
    assert_eq!(status_state(&reg, &group), "ci-wait", "the cap trims the record, never the start");
    let stored = drives_json(&reg, &group)["entries"][0]["auto_report"]
        .as_str()
        .expect("the report is persisted on the entry")
        .to_string();
    let row = audit_details(&reg, &group, "rd-auto-started")[0]["report"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    for (surface, text) in [("entry", &stored), ("audit row", &row)] {
        assert!(text.contains("line one of a long report"), "{surface} carries the report: {text:.80}");
        assert!(text.chars().count() <= 2_100, "{surface} is capped: {} chars", text.chars().count());
        assert!(!text.contains('\n'), "{surface} is one paragraph");
    }
}

// ── #3367 item 5: `open_findings` and the clean case, through the real tick ──

/// Record a verdict through the real MCP arm with `open_findings` set to `open`
/// exactly as a reviewer would send it (`Value::Null` omits the key) — the one
/// input item 5 adds. Answers the arm's own `Result`, so a refusal is testable.
fn record_declaring(
    reg: &OrchRegistry,
    group: &GroupId,
    lane: &str,
    verdict: &str,
    summary: &str,
    open: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut arguments = json!({ "pr": "1758", "verdict": verdict, "summary": summary });
    if !open.is_null() {
        arguments["open_findings"] = open;
    }
    dispatch(
        reg,
        &Caller {
            agent_id: lane.to_string(),
            group: group.clone(),
            role: Role::Reviewer,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "review_verdict", "arguments": arguments }),
    )
    .map_err(|e| format!("{e:?}"))
    // A tool refusal arrives as an MCP result flagged `isError`, not as `Err`.
    .and_then(|v| if v["isError"] == json!(true) { Err(v.to_string()) } else { Ok(v) })
}

/// Drive PR 1758 to its lane, record ONE pass declaring `open`, and tick to
/// the GATE SATISFIED line. Answers `(registry-owned group, the notice)`.
fn satisfied_with_declaration(
    reg: &OrchRegistry,
    repo: &Repo,
    gh: &FakeGh,
    summary: &str,
    open: serde_json::Value,
) -> (GroupId, String) {
    let (group, _s) = driven(reg, repo, gh);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    reg.rd_drive_group_with(&group, gh, 10_000); // ci-wait -> review-wait
    let opened = reg.rd_drive_group_with(&group, gh, 20_000);
    let (_pr, _b, lane) = opened.lanes_opened.first().cloned().expect("lane 0 opens");
    record_declaring(reg, &group, &lane, "pass", summary, open).expect("the pass records");
    let mut t = 30_000;
    for _ in 0..6 {
        let r = reg.rd_drive_group_with(&group, gh, t);
        if let Some(n) = r.notices.iter().find(|n| n.contains("GATE SATISFIED")) {
            return (group, n.clone());
        }
        t += 10_000;
    }
    panic!("the gate never satisfied; the drive is at {}", status_state(reg, &group));
}

fn queued_prs(reg: &OrchRegistry, group: &GroupId) -> Vec<u64> {
    reg.merge_queue_status(group)["entries"]
        .as_array()
        .map(|a| a.iter().filter_map(|e| e["pr"].as_u64()).collect())
        .unwrap_or_default()
}

/// **#3367 item 5, the queue route: a CLEAN satisfied gate is submitted to the
/// merge queue by the driver, and the notice says what the queue answered.**
///
/// Three cases, one registry each, varying one input apiece:
///
/// - **declared 0, base not the default** — queued. The notice carries
///   `clean: true` and the queue's position, `rd-clean` records the queue's own
///   answer, and the entry is really in `merge_queue_status()`.
/// - **declared 0, base IS the default** — the queue refuses (merge-queue.md §7,
///   structural), and the notice says so rather than claiming a queueing. This
///   is the realistic answer for a PR to `main`, and the one a reader must not
///   mistake for a success.
/// - **the control: omitted, with prose reading zero** — not clean. No clause,
///   no `rd-clean` row, and nothing queued: "never infer 0" performed through the
///   seam rather than asserted of a predicate.
#[test]
fn a_clean_gate_is_submitted_to_the_merge_queue_and_an_omitted_count_is_not() {
    #[derive(Clone, Copy, Debug)]
    enum Case {
        Queued,
        BaseIsDefault,
        Omitted,
    }
    let mut verified = 0;
    for case in [Case::Queued, Case::BaseIsDefault, Case::Omitted] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new(); // `merge_queue: enabled: true`
        let gh = FakeGh::green(HEAD_A);
        if !matches!(case, Case::BaseIsDefault) {
            gh.set_default_branch("trunk");
        }
        let open = match case {
            Case::Omitted => serde_json::Value::Null,
            _ => json!(0),
        };
        let (group, notice) =
            satisfied_with_declaration(&reg, &repo, &gh, "0 blocking, 0 non-blocking", open);
        let rows = audit_details(&reg, &group, "rd-clean");
        match case {
            Case::Queued => {
                assert!(
                    notice.contains("clean: true — 0 open findings on every lane"),
                    "{case:?}: the flag and the fact: {notice}"
                );
                assert!(
                    notice.contains("the driver submitted it to queue_merge — queued at position 1."),
                    "{case:?}: {notice}"
                );
                // The promise the owed clause made is REPLACED by what the
                // queue answered, never left beside it (#3388 review round 1).
                assert!(!notice.contains("once this exit is recorded"), "{case:?}: {notice}");
                assert!(!notice.contains("carries non-blocking findings"), "{case:?}: {notice}");
                assert_eq!(rows.len(), 1, "{case:?}: one rd-clean row: {rows:?}");
                assert_eq!(rows[0]["route"], json!("queue"));
                assert_eq!(rows[0]["queue_merge"]["queued"], json!(true));
                assert_eq!(queued_prs(&reg, &group), vec![1758], "{case:?}: really queued");
            }
            Case::BaseIsDefault => {
                assert!(notice.contains("clean: true"), "{case:?}: {notice}");
                assert!(
                    notice.contains("submitted it to queue_merge — refused: base-is-default"),
                    "{case:?}: the refusal, not a claimed queueing: {notice}"
                );
                assert!(!notice.contains("queued at position"), "{case:?}: {notice}");
                assert_eq!(rows.len(), 1, "{case:?}: {rows:?}");
                assert_eq!(rows[0]["queue_merge"]["refused"], json!("base-is-default"));
                assert!(queued_prs(&reg, &group).is_empty(), "{case:?}: nothing queued");
            }
            Case::Omitted => {
                assert!(!notice.contains("clean: true"), "{case:?}: an omission is never 0: {notice}");
                assert!(rows.is_empty(), "{case:?}: no rd-clean row: {rows:?}");
                assert!(queued_prs(&reg, &group).is_empty(), "{case:?}: nothing queued");
                assert!(
                    !gh.calls().iter().any(|a| a.iter().any(|s| s == "defaultBranchRef")),
                    "{case:?}: the queue was never even asked"
                );
            }
        }
        // Every case reached the same satisfied exit — the route is all that
        // item 5 changes.
        assert_eq!(action_count(&reg, &group, "rd-satisfied"), 1, "{case:?}");
        verified += 1;
    }
    assert_eq!(verified, 3, "every case ran");
}

/// **#3367 item 5, the notice route: with the merge queue off, a clean gate's
/// notice carries `clean: true` and "0 open findings on every lane", and says
/// there is nothing to disposition** — and nothing is enqueued.
#[test]
fn a_clean_gate_without_the_queue_says_clean_and_queues_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let yaml = WORKFLOW.replacen("merge_queue:\n  enabled: true\n", "", 1);
    assert_ne!(yaml, WORKFLOW, "the fixture's premise: the merge_queue block is gone");
    let repo = Repo::with(&yaml);
    let gh = FakeGh::green(HEAD_A);
    gh.set_default_branch("trunk"); // an enqueue would be ADMITTED, were one made
    let (group, notice) = satisfied_with_declaration(&reg, &repo, &gh, "lgtm", json!(0));
    assert!(
        notice.contains(
            "clean: true — 0 open findings on every lane, CI green: there is nothing to disposition."
        ),
        "{notice}"
    );
    assert!(!notice.contains("queue_merge"), "no queue was involved: {notice}");
    let rows = audit_details(&reg, &group, "rd-clean");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["route"], json!("notice"));
    assert!(queued_prs(&reg, &group).is_empty());
    // `list_verdicts`' record carries the declaration.
    assert_eq!(reg.verdicts(&group, 1758)[0].open_findings, Some(0));
}

/// **A declaration that is not a whole number >= 0 refuses the whole call** —
/// neither dropped (which would silently cost the clean case) nor read as 0
/// (which would claim a clean review nobody made) — and writes no verdict.
#[test]
fn a_malformed_open_findings_refuses_the_verdict_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let rev = reg.spawn_agent(&group, Role::Reviewer, "rev-std", "", false, None).unwrap();
    for bad in [json!(-1), json!("0"), json!(1.5), json!(5_000_000_000u64)] {
        let err = record_declaring(&reg, &group, &rev.id, "pass", "lgtm", bad.clone())
            .expect_err("a malformed declaration must refuse");
        assert!(err.contains("open_findings"), "{bad}: the error names the argument: {err}");
        assert!(reg.verdicts(&group, 1758).is_empty(), "{bad}: nothing was recorded");
    }
    // The positive control: the same call with a real count records it.
    record_declaring(&reg, &group, &rev.id, "pass", "lgtm", json!(2)).expect("a real count");
    assert_eq!(reg.verdicts(&group, 1758)[0].open_findings, Some(2));
}

/// **#3367's round-3 residual (rev-final on #3371): a HOLD line that reached no
/// pane does not lose the report it folded in.**
///
/// A hold's notice is delivered fire-and-forget. It used to TAKE the drive's
/// `auto_report` as it folded it, so a delivery that failed dropped the
/// worker's words from the pane for good. Now the report is spent only once the
/// line has landed. Two cases, identical but for whether the orchestrator's
/// delivery lands — the first is the fix, the second the control that the
/// report IS spent when it should be (a fix that never cleared it would repeat
/// the report on every notice forever).
#[test]
fn a_hold_line_that_reached_no_pane_keeps_the_report_it_folded() {
    for lands in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::with(&workflow_with("auto_drive_on_done: true"));
        let gh = std::sync::Arc::new(FakeGh::green(HEAD_A));
        let group = reg.create_group(&repo.path(), rails()).unwrap().id;
        let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
        if lands {
            make_delivery_land(&reg, &group, &orch.id, 7301);
        }
        let w = reg
            .spawn_agent(&group, Role::Worker, "w", "", false, Some(AUTHOR_BRANCH.to_string()))
            .unwrap();
        let runner: std::sync::Arc<dyn RdRunner> = gh.clone();
        reg.set_rd_runner_override(Some(runner));
        report_done(&reg, &group, &w.id, "#1758");
        assert!(
            drives_json(&reg, &group)["entries"][0]["auto_report"].is_string(),
            "lands={lands}: the premise — an auto-started drive holding its report"
        );

        reg.set_pr_head_override(Some(HEAD_A.to_string()));
        reg.rd_drive_group_with(&group, &*gh, 10_000); // ci-wait -> review-wait
        let opened = reg.rd_drive_group_with(&group, &*gh, 20_000);
        let (_pr, _b, lane) = opened.lanes_opened.first().cloned().expect("lane 0 opens");
        record_declaring(&reg, &group, &lane, "escalate", "a human must look", json!(1))
            .expect("the escalate records");
        let held = reg.rd_drive_group_with(&group, &*gh, 30_000);
        let line = held
            .notices
            .iter()
            .find(|n| n.contains("review drive PR #1758") && n.contains("ESCALATE"))
            .unwrap_or_else(|| panic!("lands={lands}: the hold line: {:?}", held.notices));
        assert!(line.contains("ready for review"), "lands={lands}: it carries the report: {line}");
        let kept = drives_json(&reg, &group)["entries"][0]["auto_report"].is_string();
        assert_eq!(
            kept, !lands,
            "lands={lands}: the report is spent exactly when its line reached the pane"
        );
        reg.set_rd_runner_override(None);
    }
}

/// **#3388 review round 1: a satisfied tick whose WRITE failed still tells the
/// orchestrator, and says the exit was not recorded.**
///
/// A terminal exit's notice is owed on the entry and delivered by the flush,
/// which reads owed notices FROM DISK — so when `store_state` fails, the notice
/// never reaches a pane, and the `rd-state-unreadable` row recording the failure
/// is on the audit log, not in the pane. The fix delivers it directly, marked
/// `NOT RECORDED`, and on the merge-queue route says nothing was submitted —
/// because the enqueue runs only after a persisted write, and did not run.
///
/// The sabotage is `RereadKiller`'s, proven elsewhere in this file: on the
/// tick's first `gh` call the state file becomes a directory, so the store
/// fails. It is aimed at the `gate-check -> satisfied` tick exactly.
#[test]
fn a_satisfied_tick_whose_write_failed_still_delivers_its_notice_marked_not_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new(); // merge_queue on, so the Queue clause is the one at stake
    let gh = FakeGh::green(HEAD_A);
    gh.set_default_branch("trunk"); // an enqueue WOULD be admitted, were one made
    let (group, _s) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    make_delivery_land(&reg, &group, &orch.id, 7401);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    reg.rd_drive_group_with(&group, &gh, 10_000); // ci-wait -> review-wait
    let opened = reg.rd_drive_group_with(&group, &gh, 20_000);
    let (_pr, _b, lane) = opened.lanes_opened.first().cloned().expect("lane 0 opens");
    record_declaring(&reg, &group, &lane, "pass", "lgtm", json!(0)).expect("the pass records");
    let (at, _) = tick_until(&reg, &group, &gh, 30_000, "gate-check");

    let state_file =
        reg.state_root().join(group.as_str()).join(reviewdrive::REVIEW_DRIVES_FILE);
    let killer =
        RereadKiller { inner: gh, state_file: state_file.clone(), fired: Default::default() };
    let out = reg.rd_drive_group_with(&group, &killer, at + 10_000);

    // The fixture: the store really was the writer that failed.
    assert!(state_file.is_dir(), "fixture: the state path must now be a directory");
    assert!(
        reg.audit_log(&group).iter().any(|e| e.action == "rd-state-unreadable"
            && e.detail["reason"] == json!("review_drives.json could not be written")),
        "fixture: the store must be the writer that failed"
    );
    // The promise: the line still goes out, and says it was not recorded.
    let line = out
        .notices
        .iter()
        .find(|n| n.contains("GATE SATISFIED"))
        .unwrap_or_else(|| panic!("the failed write must not silence the notice: {:?}", out.notices));
    assert!(line.contains("NOT RECORDED"), "…marked as not recorded: {line}");
    assert!(
        line.contains("nothing was submitted to queue_merge"),
        "…and claiming no submission, because none happened: {line}"
    );
    assert!(!line.contains("once this exit is recorded"), "no promise it cannot keep: {line}");
    assert!(
        texts_to(&reg, &group, &orch.id).iter().any(|t| t.contains("NOT RECORDED")),
        "it was really delivered to the orchestrator's pane"
    );
    // …and nothing was enqueued: the queue was never asked.
    assert!(
        !killer.inner.calls().iter().any(|a| a.iter().any(|s| s == "defaultBranchRef")),
        "no enqueue after a failed write"
    );
    assert!(
        audit_details(&reg, &group, "rd-clean").iter().all(|r| r["route"] != json!("queue")),
        "no queue rd-clean row for a submission that did not happen"
    );
}

/// **A released lane gives up its scratch worktree, and the round that resumes
/// it gets it back at the same path** (#3443).
///
/// A lane's pane is released with its session KEPT, so the next round resumes
/// that conversation in a fresh pane, in the workspace the roster recorded.
/// Reclaiming the worktree at release is only safe because the resume cuts it
/// again first; without that, the resume refuses `resume-workspace-missing` and
/// the drive opens a cold lane (`rd-lane-resume-failed`). Both halves are here,
/// on the release the driver really performs.
#[test]
fn a_released_lane_gives_up_its_worktree_and_its_resume_cuts_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, lane) = briefed(&reg, &repo, &gh);
    reg.set_pr_body_override(Some("b".to_string()));
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    let entry = reg.agent(&lane).expect("the lane is on the roster");
    let cwd = entry.cwd.clone();
    let branch = entry.branch.clone().expect("a fresh lane cut a worktree, so it records a branch");
    let has_branch = || {
        std::process::Command::new("git")
            .current_dir(&repo.repo)
            .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
            .status()
            .expect("git")
            .success()
    };
    assert!(std::path::Path::new(&cwd).is_dir(), "control: the lane runs in its own worktree");
    assert!(has_branch(), "control: …cut on {branch}");

    record_pass_for(&reg, &group, &lane);
    report_as(&reg, &group, &lane, Role::Reviewer, "approved");
    let released = reg.rd_drive_group_with(&group, &gh, 30_000);
    assert_eq!(released.released.len(), 1, "the premise: the pane really was released");

    assert!(!std::path::Path::new(&cwd).exists(), "the released lane's worktree must be gone");
    assert!(!has_branch(), "…and its branch {branch}");
    let removed = audit_details(&reg, &group, "reviewer-worktree-removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["initiator"], json!("driver-release"), "{removed:?}");

    gh.set_facts("OPEN", HEAD_B);
    let reopened = tick_until_lane(&reg, &gh, &group, 40_000)
        .expect("the next round must brief the lane again");
    let last = audit_details(&reg, &group, "rd-lane-spawned").last().cloned().unwrap();
    assert_eq!(last["agent"], json!(reopened));
    assert_eq!(last["resumed"], json!(true), "the lane must be RESUMED, not respawned cold: {last}");
    assert!(
        audit_details(&reg, &group, "rd-lane-resume-failed").is_empty(),
        "nothing may refuse that resume"
    );
    assert_eq!(
        reg.agent(&reopened).unwrap().cwd,
        cwd,
        "the resume runs at the path its session ran in"
    );
    assert!(std::path::Path::new(&cwd).is_dir(), "…which was cut again");
    assert_eq!(audit_details(&reg, &group, "reviewer-worktree-recut").len(), 1);
}
