//! A live drive's panes are visible and guarded: roster rows, kills, and conflicts at the gate (#2811 S2).
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── #2811 S2: a live drive's panes are visible and guarded (#2555 item 1) ───

/// A live drive in `fix-wait`, and the WORKER pane it handed the fix to:
/// `(group, orchestrator pane, worker pane)`.
///
/// Built on [`to_first_handback`] rather than on a fresh tick sequence, because
/// that helper is this file's proven route to a hand-back and the arcs it takes
/// are not this section's subject. Its lane-side twin is [`driven_lane`].
///
/// **No `with_pane` on the worker, deliberately.** `kill_agent_as` refuses a
/// pane with no pty ("has no terminal yet") BEFORE it reaches the `AppHandle`
/// test mode does not have, and that refusal is this section's positive control
/// for "the guard was passed and the real kill was reached" — a distinct,
/// deterministic error rather than the absence of one.
pub(crate) fn driven_worker(reg: &OrchRegistry, repo: &Repo, gh: &FakeGh) -> (GroupId, String, String) {
    let (group, _session) = driven(reg, repo, gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(reg, &orch.id, 7801);
    let handed = to_first_handback(reg, &group, gh);
    let (_pr, worker) = handed
        .handbacks
        .first()
        .cloned()
        .unwrap_or_else(|| panic!("the drive must hand back: {handed:?}"));
    assert_eq!(status_state(reg, &group), "fix-wait", "the fixture's own premise");
    (group, orch.id, worker)
}

/// A live drive in `review-wait`, and the reviewer LANE pane it opened:
/// `(group, orchestrator pane, lane pane)` — [`driven_worker`]'s twin, built on
/// [`lane_round_one`] for the same reason.
fn driven_lane(reg: &OrchRegistry, repo: &Repo, gh: &FakeGh) -> (GroupId, String, String) {
    let (group, orch, lane) = lane_round_one(reg, repo, gh);
    assert_eq!(status_state(reg, &group), "review-wait", "the fixture's own premise");
    (group, orch, lane)
}

/// The roster row for `agent_id` as `list_agents` publishes it.
fn roster_row(reg: &OrchRegistry, group: &GroupId, agent_id: &str) -> serde_json::Value {
    reg.list_agents(group)
        .as_array()
        .expect("list_agents answers an array")
        .iter()
        .find(|r| r["id"] == json!(agent_id))
        .cloned()
        .unwrap_or_else(|| panic!("{agent_id} is not on the roster"))
}

/// `kill_agent` as the group's ORCHESTRATOR, through the real MCP dispatch, and
/// the refusal it produced.
///
/// **A tool refusal is an `Ok` here, not an `Err`** — `dispatch` answers
/// `{content: [{text}], isError: true}` for a tool that declined, and reserves
/// `Err` for the protocol layer. Reading it the other way made every assertion
/// below fire on the helper rather than on the guard, which the red round
/// caught: the panic quoted `has no terminal yet` — the very sentence the test
/// wanted to read — from inside an `expect_err`.
///
/// Always a refusal in test mode, whichever branch produced it: no integration
/// test has the `AppHandle` `kill_agent_as` needs, so it stops at the pane's
/// missing pty and a kill this guard PASSES still declines — with a DIFFERENT
/// sentence, which is exactly what the assertions below tell apart.
fn orch_kill_err(
    reg: &OrchRegistry,
    group: &GroupId,
    orch: &str,
    args: serde_json::Value,
) -> String {
    let caller = Caller {
        agent_id: orch.to_string(),
        group: group.clone(),
        role: Role::Orchestrator,
        role_hint: None,
    };
    let out = dispatch(
        reg,
        &caller,
        "tools/call",
        &json!({ "name": "kill_agent", "arguments": args }),
    )
    .expect("the MCP protocol layer accepts this call");
    assert_eq!(
        out["isError"],
        json!(true),
        "test mode has no PtyManager, so no kill can succeed: {out}"
    );
    out["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

/// **#2811 S2 (i).** Every roster row says whether a live drive is using that
/// pane, so the question `kill_agent` now refuses on is one the orchestrator
/// could have asked first.
///
/// Both sides of a drive and a delegate it has nothing to do with. The third is
/// the control that makes the first two mean something — a build that stamped
/// `driven_by` on every row would satisfy them both.
///
/// The KEY is asserted present on the undriven row too, with `null` as its
/// value: "not driven" must never have to be told apart from "this build does
/// not report it", which is the rule `wip` and `current_sprint` already follow
/// on the board read.
#[test]
fn every_roster_row_says_whether_a_live_drive_is_using_that_pane() {
    // (arm, the driven pane's `driven_by`, a bystander's)
    type Row = (&'static str, serde_json::Value, serde_json::Value);
    let mut observed: Vec<Row> = Vec::new();

    for arm in ["worker", "lane"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, _orch, pane) = if arm == "worker" {
            driven_worker(&reg, &repo, &gh)
        } else {
            driven_lane(&reg, &repo, &gh)
        };
        let bystander = reg
            .spawn_agent(&group, Role::Worker, "unrelated", "", false, None)
            .expect("a delegate this drive never touched");

        // The key-presence rule is asserted on BOTH rows, not just the
        // bystander (rev round 1, N2): the driven row is read by indexing,
        // so a build that dropped the key there would fail with a serde
        // index panic rather than with the sentence that says what is wrong.
        let row = roster_row(&reg, &group, &bystander.id);
        let driven_row = roster_row(&reg, &group, &pane);
        for (which, r) in [("bystander", &row), ("driven", &driven_row)] {
            assert!(
                r.get("driven_by").is_some(),
                "{arm}/{which}: the key is always present — `null` is the answer, not the \
                 absence of one"
            );
        }
        observed.push((arm, driven_row["driven_by"].clone(), row["driven_by"].clone()));
    }

    let expected: Vec<Row> = vec![
        ("worker", json!("#1758"), json!(null)),
        ("lane", json!("#1758"), json!(null)),
    ];
    assert_eq!(
        observed, expected,
        "each row is (arm, the driven pane's `driven_by`, a bystander's). A `null` in the \
         middle column is the #3038 class — nothing on the roster says that pane belongs to \
         a drive. A `\"#1758\"` in the last is a marker that means nothing."
    );
}

/// **#2811 S2 (ii).** The orchestrator's `kill_agent` refuses a pane a live
/// drive is using, names the PR and the side, and says both ways out.
///
/// This is #3038 in one test: the orchestrator killed w-2460 to free a slot 42
/// seconds before the drive needed it, and nothing in `kill_agent` — which
/// checked group membership and nothing else — could have told it. The refusal
/// is not a veto: `force: true` goes through, in the same sentence that
/// refuses.
///
/// **`force: true` is asserted by the error it REACHES, not by an `Ok`.**
/// `kill_agent_as` needs an `AppHandle` to reach `PtyManager`, which no
/// integration test has, so it refuses a pane with no pty first — "has no
/// terminal yet". That refusal is a precise positive control: it is produced
/// only BELOW the guard, so reading it proves the guard was passed and the real
/// kill was reached, while its absence on the unforced arm proves the guard
/// stopped short of it. The bystander arm pins that a pane no drive owns has
/// never been able to reach anything else.
#[test]
fn a_kill_of_a_pane_a_live_drive_is_using_is_refused_unless_forced() {
    for arm in ["worker", "lane"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, orch, pane) = if arm == "worker" {
            driven_worker(&reg, &repo, &gh)
        } else {
            driven_lane(&reg, &repo, &gh)
        };
        let bystander = reg
            .spawn_agent(&group, Role::Worker, "unrelated", "", false, None)
            .expect("a delegate this drive never touched");

        let refused = orch_kill_err(&reg, &group, &orch, json!({ "agent_id": pane }));
        assert!(
            refused.contains(&format!(
                "{pane} is the {arm} pane of the live review drive on PR #1758"
            )),
            "{arm}: the refusal names the pane, its side of the drive, and the PR: {refused}"
        );
        assert!(
            refused.contains("cancel_review_drive first, or pass force:true"),
            "{arm}: …and both ways out, in the sentence that refuses: {refused}"
        );
        assert_ne!(
            reg.agent(&pane).expect("still on the roster").status,
            AgentStatus::Dead,
            "{arm}: a refusal that killed the pane anyway is not a refusal"
        );

        // The control: a pane no drive owns reaches the kill exactly as it
        // always did, and the only thing standing between it and death is test
        // mode.
        let free = orch_kill_err(&reg, &group, &orch, json!({ "agent_id": bystander.id }));
        assert!(
            free.contains("has no terminal yet"),
            "{arm}: an undriven delegate is not guarded — it reaches the kill: {free}"
        );
        assert!(
            !free.contains("review drive"),
            "{arm}: …and is not refused by this guard at all: {free}"
        );

        // `force` goes through the guard and into the kill. Same pane, same
        // tool, one added argument, and the error moves from the guard's to the
        // kill's.
        let forced = orch_kill_err(&reg, &group, &orch, json!({ "agent_id": pane, "force": true }));
        assert!(
            forced.contains("has no terminal yet"),
            "{arm}: `force: true` reaches the real kill: {forced}"
        );
        assert!(
            !forced.contains("review drive"),
            "{arm}: …and is not stopped by the guard: {forced}"
        );

        // And the guard is a fact about the DRIVE, not about the pane: the
        // refusal's own first remedy makes the same kill go through. This is
        // also what pins the `is_live` half of the ownership read — a drive
        // that has ended owns nothing.
        assert_eq!(
            reg.cancel_review_drive(&group, 1758, "orch-1")["cancelled"],
            json!(true),
            "{arm}: the drive cancels"
        );
        let after = orch_kill_err(&reg, &group, &orch, json!({ "agent_id": pane }));
        assert!(
            after.contains("has no terminal yet") && !after.contains("review drive"),
            "{arm}: `cancel_review_drive` is the remedy the refusal names, so it must work: \
             {after}"
        );
        assert_eq!(
            roster_row(&reg, &group, &pane)["driven_by"],
            json!(null),
            "{arm}: …and the roster stops claiming a drive owns it"
        );
    }
}

/// **#2811 S2 (ii), the other half.** What `force: true` COSTS, so the
/// orchestrator that overrode the refusal reads back the outcome it chose.
///
/// The drive does not silently limp on: the pane is gone, `fix-wait` observes
/// it on the next tick, and the hold quotes who ended it — the sentence
/// `rd_pane_exit` builds from `killed_by`, which is exactly what `kill_agent`
/// stamps. That is #3038's own audit line (`(w-2460) is gone (ended by
/// orchestrator)`), reproduced deliberately instead of by accident.
///
/// **The kill is completed the way `a_dead_lane_pane_is_re_opened_next_tick…`
/// completes one**, through the real initiator recorder plus
/// `mark_agent_dead_for_test`: `kill_agent_as` cannot finish in test mode (no
/// `AppHandle`), and the test above already pins that `force: true` reaches it.
/// So the `killed_by` read here is the field `kill_agent` writes, not a literal
/// this test invented.
#[test]
fn a_forced_kill_of_a_driven_worker_holds_the_drive_naming_the_kill() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, orch, worker) = driven_worker(&reg, &repo, &gh);

    // **The premise is the OVERRIDE, so it is spelled as a pair**: the same
    // kill refused, then admitted by the one added argument. Asserting only the
    // second half would make this a test about a kill rather than about
    // `force`, and it PASSED at the base for exactly that reason — the red
    // round measured it (`110 passed; 3 failed`, this the one that did not
    // move), which is what a by-test read of the failing set is for.
    let refused = orch_kill_err(&reg, &group, &orch, json!({ "agent_id": worker }));
    assert!(
        refused.contains("the live review drive on PR #1758"),
        "the premise: unforced, this kill is refused: {refused}"
    );
    let forced = orch_kill_err(&reg, &group, &orch, json!({ "agent_id": worker, "force": true }));
    assert!(forced.contains("has no terminal yet"), "…and `force` got past that guard");
    reg.record_exit_initiator(&worker, ExitInitiator::Orchestrator);
    assert!(reg.mark_agent_dead_for_test(&worker), "the pane really goes");

    reg.rd_drive_group_with(&group, &gh, 50_000);
    assert_eq!(status_state(&reg, &group), "held", "the drive cannot hand a fix to a dead pane");
    let helds = audit_details(&reg, &group, "rd-held");
    let last = helds.last().cloned().unwrap_or_default();
    assert_eq!(last["reason"], json!("worker-unresumable"), "{helds:?}");
    let why = format!("{last}");
    assert!(
        why.contains(&format!("({worker}) is gone (ended by orchestrator)")),
        "the hold names the pane and who ended it — the orchestrator reads back what it \
         chose, not a resume that appears to have died: {why}"
    );
}

/// **#2811 S2 (iii).** The cap refusal's remedy — "reuse an idle agent or kill
/// one first" — cannot point at a pane a live drive is holding.
///
/// This is the composition that made #3038 reachable on the driver's OWN
/// advice: a cap-full notice lists the live delegates, half of them idle
/// workers a drive is between rounds with, and the orchestrator picks one. The
/// list now says which of those rows the advice does not apply to.
///
/// The bystander row is the negative control, and it is byte-for-byte what this
/// function produced before S2 — which `orchestration.rs`'s existing
/// `(worker, working)` pins already assert from the other side.
#[test]
fn the_cap_refusal_roster_marks_a_pane_a_live_drive_is_holding() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _orch, worker) = driven_worker(&reg, &repo, &gh);
    let bystander = reg
        .spawn_agent(&group, Role::Worker, "unrelated", "", false, None)
        .expect("a delegate this drive never touched");

    // Fill the group to its cap and read the refusal that comes back.
    let mut refusal = String::new();
    for i in 0..12 {
        match reg.spawn_agent(&group, Role::Worker, &format!("filler{i}"), "", false, None) {
            Ok(_) => continue,
            Err(e) => {
                refusal = e;
                break;
            }
        }
    }
    assert!(
        loomux_lib::orchestration::is_live_cap_refusal(&refusal),
        "the control: this spawn was refused by the CAP and not by something else: {refusal}"
    );

    let word = |id: &str| -> &'static str {
        if reg.agent(id).expect("on the roster").idle_since_ms.is_some() {
            "idle"
        } else {
            "working"
        }
    };
    assert!(
        refusal.contains(&format!("{worker} (worker, {}, driven #1758)", word(&worker))),
        "the drive's worker is marked, next to the `idle` that would otherwise recommend \
         it: {refusal}"
    );
    assert!(
        refusal.contains(&format!("{} (worker, {})", bystander.id, word(&bystander.id))),
        "…and a pane no drive owns reads exactly as it did before: {refusal}"
    );
    assert!(
        !refusal.contains(&format!("{} (worker, {}, driven", bystander.id, word(&bystander.id))),
        "a marker on every row is a marker that means nothing: {refusal}"
    );
}

/// **#2811 S2, review round 1 N1.** A drive record orrerix cannot READ is a
/// fault, not evidence that nothing owns this pane — so the kill is refused,
/// and the roster says `unreadable` rather than `null`.
///
/// The first draft collapsed the two: `rd_driven_panes_locked` answered an empty
/// map for both "there are no live drives" and "`load_state` failed", so on the
/// one input where nothing else can tell — a `review_drives.json` present and
/// unparseable, which is what a downgrade produces — `kill_agent` admitted
/// exactly the kill this slice exists to refuse, silently.
///
/// It is this repo's settled posture on the same file, not a new one:
/// `a_torn_drive_record_refuses_the_enqueue_instead_of_reading_as_undriven`
/// pins `queue_merge` answering `rd-state-unreadable` on this input, with the
/// same sentence — "a drive record orrerix cannot read is a FAULT, not evidence
/// that the PR is undriven".
///
/// **The readable arm is the control**, and it is load-bearing rather than
/// decorative: it is what distinguishes this fix from one that refuses every
/// kill. Same pane, same call, one axis — whether the record parses.
#[test]
fn a_drive_record_orrerix_cannot_read_refuses_the_kill_rather_than_admitting_it() {
    // (arm, the refusal `kill_agent` produced, the pane's `driven_by`)
    type Row = (&'static str, &'static str, serde_json::Value);
    let mut observed: Vec<Row> = Vec::new();

    for arm in ["readable", "torn"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, orch, worker) = driven_worker(&reg, &repo, &gh);
        // An UNDRIVEN pane, so the torn arm cannot pass by accident: under the
        // defect this one is admitted, and it must stay refused when the record
        // is unreadable — "I could not look" is not "nothing owns it", and that
        // is true of a pane no drive happens to own too.
        let bystander = reg
            .spawn_agent(&group, Role::Worker, "unrelated", "", false, None)
            .expect("a delegate this drive never touched");

        if arm == "torn" {
            assert!(
                reg.corrupt_drive_record_for_test(&group),
                "the record must exist to be torn"
            );
        }

        let err = orch_kill_err(&reg, &group, &orch, json!({ "agent_id": bystander.id }));
        let word = if err.contains("cannot read this group's review-drive record") {
            "unreadable-refusal"
        } else if err.contains("has no terminal yet") {
            "reached-the-kill"
        } else {
            "other"
        };
        observed.push((arm, word, roster_row(&reg, &group, &worker)["driven_by"].clone()));

        // `force` is the way out of BOTH refusals, so a group whose record is
        // genuinely broken is never wedged — asserted on the torn arm, which is
        // the one where the new refusal stands between the caller and the kill.
        if arm == "torn" {
            let forced =
                orch_kill_err(&reg, &group, &orch, json!({ "agent_id": bystander.id, "force": true }));
            assert!(
                forced.contains("has no terminal yet"),
                "`force` overrides the unreadable-record refusal too: {forced}"
            );
        }
    }

    let expected: Vec<Row> = vec![
        // The control: the record parses, this pane is owned by nothing, and the
        // kill goes through exactly as it always did.
        ("readable", "reached-the-kill", json!("#1758")),
        // The fix: orrerix could not look, so it does not claim the pane is free
        // — and does not claim on the roster that the DRIVEN pane is unowned
        // either, which `null` would have said.
        ("torn", "unreadable-refusal", json!("unreadable")),
    ];
    assert_eq!(
        observed, expected,
        "each row is (arm, what `kill_agent` answered, the DRIVEN pane's `driven_by`). \
         `reached-the-kill` on the torn arm is the defect: an unreadable record read as \
         evidence that nothing is driven. A `null` there is the same mistake on the roster \
         the refusal is derived from."
    );
}

/// **A CONFLICTING PR is never `satisfied`, even with every lane passed and the
/// gate agreeing** (#2311) — the seam half of the engine's pins, driven through
/// the real tick.
///
/// The fixture is the measured incident (§1(d), #2942): a drive whose lanes
/// reviewed a clean PR and whose base moved under it, so mergeability turns
/// CONFLICTING while the drive sits in `gate-check`. Before this the drive
/// answered GATE SATISFIED, and the cost was paid outside the driver — a hand
/// rebase, a re-drive at `rounds_already_spent 3`, two fresh whole-diff lanes,
/// about ten minutes of cap starvation and three orchestrator turns.
///
/// It reuses `at_gate_check_holding_both_panes` rather than sequencing its own
/// ticks, so the premise "this drive really is one tick short of `satisfied`" is
/// the one that fixture already asserts.
///
/// Three things are asserted together because each alone has a passing
/// implementation that is wrong: the STATE (a drive that merely waited would
/// also not be `satisfied`), the COUNTER (a conflict misread as a red run
/// reaches `fix-wait` too, so only `rebase_attempts` discriminates the arc), and
/// the BRIEF (the worker must be told to rebase, not sent to findings that do
/// not exist).
#[test]
fn a_conflicting_pr_at_gate_check_is_handed_back_for_a_rebase_not_declared_satisfied() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _worker, _lane, _session) = at_gate_check_holding_both_panes(&reg, &repo, &gh);
    let before = reg.review_drive_status(&group);
    assert_eq!(
        before["drives"][0]["counters"]["rebase_attempts"],
        json!(0),
        "the fixture's premise: no rebase has been asked for yet: {before}"
    );

    // The base moves under the drive. Nothing else changes: same head, same
    // body, the same recorded pass — so the gate still says SATISFIED and
    // mergeability is the only thing that can have decided what follows.
    gh.set_merge_state("CONFLICTING");
    let audits_before = audit_actions(&reg, &group).len();
    let report = reg.rd_drive_group_with(&group, &gh, 90_000);

    let s = reg.review_drive_status(&group);
    assert_eq!(
        status_state(&reg, &group),
        "fix-wait",
        "arc 3 from `gate-check`: a conflicting PR is handed back, not declared satisfied: {s}"
    );
    assert_eq!(
        s["drives"][0]["counters"]["rebase_attempts"],
        json!(1),
        "the REBASE budget is what a conflict spends — a red run reaches `fix-wait` too, so the state alone does not say which arc was taken: {s}"
    );

    let mut all = audit_actions(&reg, &group);
    let after = all.split_off(audits_before);
    assert!(
        after.iter().any(|a| a == "rd-conflicting"),
        "the tick that classified the conflict must say so in the audit, or the `rd-handback why:conflict` beside it accounts for nothing (§5.4): {after:?}"
    );
    assert!(
        !after.iter().any(|a| a == "rd-satisfied"),
        "…and it must not ALSO have declared the gate satisfied: {after:?}"
    );

    let (_pr, worker) =
        report.handbacks.first().cloned().expect("the conflict hand-back resumed a worker pane");
    let fix = lane_brief(&reg, &worker);
    assert!(
        fix.contains("It is CONFLICTING against main."),
        "the hand-back must name the conflict — the brief is keyed on the observation, so the arm that renders here is the same one `ci-wait` renders: {fix}"
    );
    assert!(
        !fix.contains("Review requested changes"),
        "…and NOT the review-findings arm, which would send the worker to findings that do not exist: {fix}"
    );
}

/// **The same arc from `review-wait`, which is where the hold was WRONG rather
/// than merely late** (#2311, widened past plan-2504's S4 by the measurement on
/// #3118).
///
/// `decide_review_wait` asks `route_reviewers` first, and routing reads the
/// changed-file list — which GitHub does not compute for a conflicted head. So a
/// PR that went CONFLICTING with a lane mid-review parked
/// `held(routing-unaccountable)`: a notice saying *which reviewers are required
/// is unknown* about a PR whose actual problem is that it does not merge, and
/// whose stated remedy (`drive_review` again) re-reads the same unreadable
/// routing and re-holds. Reading mergeability above the per-state logic reports
/// the cause instead, and the cause has a hand-back.
///
/// The lane is deliberately left mid-review with no verdict, so the ONLY thing
/// that can move this drive is the conflict.
#[test]
fn a_conflicting_pr_in_review_wait_is_handed_back_rather_than_held_on_routing() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _lane) = briefed(&reg, &repo, &gh);
    assert_eq!(
        status_state(&reg, &group),
        "review-wait",
        "the fixture's premise: a lane is open and has recorded nothing"
    );

    gh.set_merge_state("CONFLICTING");
    let report = reg.rd_drive_group_with(&group, &gh, 30_000);

    let s = reg.review_drive_status(&group);
    assert_eq!(
        status_state(&reg, &group),
        "fix-wait",
        "arc 3 from `review-wait`: the conflict is read before the routing question it makes unanswerable: {s}"
    );
    assert_ne!(
        s["drives"][0]["held_reason"],
        json!("routing-unaccountable"),
        "the hold this replaces names a CONSEQUENCE of the conflict, and its remedy reproduces it: {s}"
    );
    assert_eq!(s["drives"][0]["counters"]["rebase_attempts"], json!(1), "{s}");
    assert_eq!(
        s["drives"][0]["counters"]["review_rounds"],
        json!(0),
        "…and no review round is spent: no lane delivered any findings"
    );

    let (_pr, worker) =
        report.handbacks.first().cloned().expect("the conflict hand-back resumed a worker pane");
    let fix = lane_brief(&reg, &worker);
    assert!(fix.contains("It is CONFLICTING against main."), "{fix}");
    assert!(
        !fix.contains("Review requested changes"),
        "the review-findings arm must not render: no lane recorded anything: {fix}"
    );
}

/// **A conflicting PR briefs NO lane, on the one route that used to** (#2311) —
/// the pin that keeps `CiArm`'s dropped arm honest.
///
/// `rd_lane_brief` still carries a CONFLICTING sentence, and its match over a
/// closed enum must, so nothing about that arm's existence says whether a
/// reviewer can ever receive it. The route that could was arc 8: `fix-wait ->
/// review-wait` on a `report(done)` at an unchanged head, taken **without**
/// consulting `facts.ci`, so the next tick opened a lane and told a reviewer to
/// review a PR that does not merge on its merits. `decide` now reads
/// mergeability above the per-state logic, so that tick hands the worker back
/// instead — a paid review round saved on a PR that must be rebased anyway.
///
/// This is a counterfactual, so it is asserted by PERFORMING it rather than by
/// describing it: the identical fixture with the mergeability left CLEAN is the
/// positive control, and it must open a lane. Without that control "no lane
/// opened" is satisfied by a fixture that never reached `review-wait` at all.
#[test]
fn a_conflicting_pr_briefs_no_lane_at_all() {
    for conflicting in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, _session) = driven(&reg, &repo, &gh);

        // A red CI hands the PR back before any lane has opened.
        gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
        let handed = reg.rd_drive_group_with(&group, &gh, 10_000);
        let (_pr, worker) =
            handed.handbacks.first().cloned().expect("a red CI hands the PR back");
        assert_eq!(status_state(&reg, &group), "fix-wait");

        // The worker reports done WITHOUT pushing, so arc 8 is what answers and
        // the drive reaches `review-wait` at a head whose CI is not green.
        dispatch(
            &reg,
            &Caller {
                agent_id: worker.clone(),
                group: group.clone(),
                role: Role::Worker,
                role_hint: None,
            },
            "tools/call",
            &json!({ "name": "report", "arguments": {
                "status": "done", "summary": "that failure was unrelated" } }),
        )
        .expect("the driven worker reports");
        reg.rd_drive_group_with(&group, &gh, 20_000);
        assert_eq!(
            status_state(&reg, &group),
            "review-wait",
            "the fixture's premise: arc 8 reaches review-wait at an unchanged head"
        );

        // The ONE thing that differs between the two runs.
        if conflicting {
            gh.set_merge_state("CONFLICTING");
        }
        let opened = reg.rd_drive_group_with(&group, &gh, 30_000);

        if conflicting {
            assert!(
                opened.lanes_opened.is_empty(),
                "a conflicting PR must not have a reviewer briefed on it: {:?}",
                opened.lanes_opened
            );
            assert_eq!(
                status_state(&reg, &group),
                "fix-wait",
                "…and what it does instead is hand the worker back for the rebase"
            );
            let s = reg.review_drive_status(&group);
            assert_eq!(s["drives"][0]["counters"]["rebase_attempts"], json!(1), "{s}");
        } else {
            // The positive control: the same fixture, CLEAN, DOES open a lane —
            // so the emptiness above is the conflict and not a walk that never
            // got here.
            let (_pr, _b, lane) = opened
                .lanes_opened
                .first()
                .cloned()
                .expect("the control: a CLEAN PR at this point opens its lane");
            let brief = lane_brief(&reg, &lane);
            assert!(
                brief.contains("This PR's checks are RED at that head."),
                "…and briefs the reviewer with the CI it observed: {brief}"
            );
            assert!(
                !brief.contains("does not merge cleanly"),
                "…which is not the conflict sentence: {brief}"
            );
        }
    }
}
