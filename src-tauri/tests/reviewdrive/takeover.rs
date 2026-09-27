//! One live pane per session (#3203): hand-back takeovers, releases, and the round-1 residuals.
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// **The prune and `drive_review` clears are NOT pinned, and cannot be.**
//
// Review 3 asked for a test per path. Two of the three paths turn out to be
// unreachable, and the mutation is what proved it: with all three clears
// reverted, only the cancel test above reddens (run `34156057201`, 142 passed
// / 1 failed). The reason is the review-2 spend rule one function over — it
// discharges the mark on any tick whose entry ends outside `fix-wait`:
//
// - A drive becomes terminal either by a TICK (`pr_open == Some(false)` takes
//   it to `cancelled`, and the spend clears the mark on that same tick,
//   before the prune can ever see it) or by `cancel_review_drive`, which has
//   its own clear — the one the test above pins.
// - A reconcile-cancel never marks in the first place: the mark is written in
//   the branch the cancel arm skips.
//
// So no reachable sequence leaves a standing mark on an entry that then
// reaches the prune or a displacing `drive_review`. The two clears are kept as
// defence in depth — the spend rule is one edit away from stopping being
// exhaustive, and both sites are where `rd_signals` is already cleared — but
// a test asserting the property there passes whether or not the clear exists,
// which is a decoration, not coverage. Two such tests were written, run
// against the mutation, found green, and deleted rather than shipped.
// ── #3203: the driver never puts a SECOND live pane on one session ──────────

/// Every live pane in `group` whose session is `session` — the population
/// #3203 is a defect in, read off the roster an orchestrator would read.
///
/// Sorted, so an assertion on it is order-independent: nothing about WHICH pane
/// the driver reached is pinned here, only how many exist.
fn live_panes_on_session(reg: &OrchRegistry, group: &GroupId, session: &str) -> Vec<String> {
    let list = reg.list_agents(group);
    let mut out: Vec<String> = list
        .as_array()
        .expect("list_agents answers an array")
        .iter()
        .filter(|a| a["session"] == json!(session) && a["status"] != json!("dead"))
        .map(|a| a["id"].as_str().unwrap_or_default().to_string())
        .collect();
    out.sort();
    out
}

/// The `pane` field of every `rd-handback` row this group has written, in order
/// (#3203).
pub(crate) fn handback_panes(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    audit_details(reg, group, "rd-handback")
        .iter()
        .map(|d| d["pane"].as_str().unwrap_or("<missing>").to_string())
        .collect()
}

/// **#3203, the defect itself.** A second hand-back arriving while the pane the
/// FIRST one resumed is still working must land in that pane, not in a third.
///
/// Measured on PR #3198: two red heads inside one fix produced `rd-handback`
/// into `w-2659` and then into `w-2660`, while `w-2657` — the orchestrator's own
/// worker pane — was still alive. Three panes on one session, all writing one
/// worktree; the third correctly reported that another actor was editing its
/// worktree, and no work was lost only because two panes happened to make the
/// same edits.
///
/// **What makes this red on `main` is the pane going BUSY between the two
/// hand-backs**, and it is the whole fixture. Hand-back one reuses the pane by
/// #1960's ordinary arm, because a freshly spawned task-less pane is idle and
/// delivery-ready. Then the worker starts fixing, `idle_since_ms` clears, and
/// `idle_pane_on_session`'s idle half refuses it — which is `rd_reuse_pane`
/// answering `None`, which before #3203 fell straight through to `rd_spawn`. A
/// fixture whose pane stayed idle throughout is GREEN under the defect: both
/// hand-backs reuse, and the test would assert nothing.
///
/// The pane is made busy the way the product makes any delegate busy — its
/// orchestrator sends it a prompt, which stamps `idle_since_ms = None` before
/// the delivery (`mcp.rs`'s `send_prompt` arm) — rather than by reaching into
/// the registry, so the fixture is a state the running app really produces.
///
/// **Four assertions, and none of them is another restated.** The pane the
/// hand-back reached; the spawn count over that tick; how many live panes are
/// left on the session; and the two `pane` words on the audit rows. An
/// implementation that reached the right pane and opened one anyway fails the
/// second; one that reached it by dropping the block filter fails
/// `a_live_idle_pane_on_the_wrong_block_is_not_reused_for_a_handback`.
#[test]
fn a_second_handback_takes_over_the_working_pane_instead_of_opening_a_third() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).unwrap();
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    // A terminal and a paused group so a delivery is really admitted, plus the
    // delivery record that makes the pane READY — hand-back one has to take
    // #1960's clean-reuse arm, or the second hand-back is not the axis.
    make_delivery_land(&reg, &group, &w.id, 7203);
    make_pane_ready(&reg, 7203, true);
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    // Hand-back one: CI red at HEAD_A, into the pane that is already there.
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    let first = reg.rd_drive_group_with(&group, &gh, 10_000);
    let (_pr, w1) = first.handbacks.first().cloned().expect("the drive hands back");
    assert_eq!(
        w1, w.id,
        "the fixture's premise: hand-back ONE reuses the worker's own pane, so what the \
         SECOND one does is the only thing this test varies"
    );

    // …and now that pane is mid-fix.
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    let _ = dispatch(
        &reg,
        &Caller {
            agent_id: orch.id.clone(),
            group: group.clone(),
            role: Role::Orchestrator,
            role_hint: None,
        },
        "tools/call",
        &json!({ "name": "send_prompt", "arguments": {
            "agent_id": w.id.clone(), "text": "carry on with the fix" } }),
    );
    assert!(
        reg.agent(&w.id).is_some_and(|a| a.idle_since_ms.is_none()),
        "the fixture's premise: the pane is WORKING, which is what the reuse arm refuses"
    );

    // Hand-back two: the worker pushed a fix that is itself red.
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 20_000);
    let before = action_count(&reg, &group, "agent-spawn");
    let second = reg.rd_drive_group_with(&group, &gh, 30_000);
    let (_pr, w2) = second
        .handbacks
        .first()
        .cloned()
        .unwrap_or_else(|| panic!("a second red must hand back again: {second:?}"));
    let opened = action_count(&reg, &group, "agent-spawn") - before;

    assert_eq!(
        w2, w.id,
        "the second hand-back opened a pane on a session that already had a live one — \
         that is #3203: two panes writing one worktree, with nothing anywhere to say so"
    );
    assert_eq!(opened, 0, "…and it must have opened nothing to do it");
    assert_eq!(
        live_panes_on_session(&reg, &group, &session),
        vec![w.id.clone()],
        "the invariant: the driver never leaves two live panes on one session"
    );
    assert_eq!(
        handback_panes(&reg, &group),
        vec!["reused".to_string(), "taken-over".to_string()],
        "the two arms are told apart on the log — a busy take-over emits no \
         `rd-reuse-declined` row, so folding them into one word would make \"the driver \
         typed into a working delegate\" unreadable (§5.4)"
    );
    assert_eq!(
        texts_to(&reg, &group, &w.id).iter().filter(|t| t.contains("is back with you at head")).count(),
        2,
        "…and BOTH fix briefs really reached that pane, rather than one of them going to a \
         pane that was never opened"
    );
}

/// **The positive control for the take-over arm**: a session with NO live pane
/// still spawns one (#3203).
///
/// Load-bearing rather than decorative. Every assertion in
/// `a_second_handback_takes_over_the_working_pane_instead_of_opening_a_third`
/// is satisfied by an implementation that never spawns for a hand-back at all —
/// which would park every drive whose worker pane has died on
/// `held(worker-unresumable)` forever. This is the row that fails under it, and
/// it is the row that already passed BEFORE #3203, so a run where only this goes
/// green says the take-over arm is refusing everything.
///
/// **The pane is made DEAD by the driver's own release, which is the production
/// sequence rather than a convenience.** #2501 releases a worker pane once the
/// drive has consumed its report, keeping the session — "a released pane's
/// session is what the next round resumes into" — so hand-back, report, release,
/// red again IS how a live drive reaches a session whose only pane is dead. Two
/// alternatives were tried and rejected: `kill_agent` needs a Tauri `AppHandle`
/// and answers `Err` in a headless test, leaving the pane alive and the premise
/// false (this test's own first draft, caught by CI); and a pane with no
/// terminal would control on `live_pane_on_session`'s pty condition instead of
/// its liveness one, which is the condition #3203 turns on.
#[test]
fn a_handback_to_a_session_with_no_live_pane_still_opens_one() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).unwrap();
    let session = w.session_id.clone().expect("claude mints a session id at spawn");
    make_delivery_land(&reg, &group, &w.id, 7204);
    make_pane_ready(&reg, 7204, true);
    let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
    assert_eq!(out["driving"], json!(true), "drive_review refused: {out}");
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    // Round one: red at HEAD_A hands back into the pane that is there, the
    // worker reports done, and the drive releases that pane on the tick that
    // consumes the report (#2501) — keeping the session.
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    let first = reg.rd_drive_group_with(&group, &gh, 10_000);
    let (_pr, w1) = first.handbacks.first().cloned().expect("the drive hands back");
    assert_eq!(w1, w.id, "round one reuses the pane that is already on the session");
    report_as(&reg, &group, &w.id, Role::Worker, "done");
    gh.set_checks(r#"[{"name":"build","state":"SUCCESS","link":"x"}]"#);
    reg.rd_drive_group_with(&group, &gh, 20_000);
    assert_eq!(
        reg.agent(&w.id).map(|a| a.status == AgentStatus::Dead),
        Some(true),
        "the fixture's premise: the drive released its worker pane, so the session it \
         still records has no live pane on it"
    );
    assert!(
        live_panes_on_session(&reg, &group, &session).is_empty(),
        "…and the roster agrees, which is what `live_pane_on_session` reads"
    );
    assert_eq!(
        driven_worker_session(&reg, &group),
        session,
        "…while the drive keeps the SESSION — a release that dropped it would make the \
         hand-back below refuse rather than spawn, and this test would pass for the \
         wrong reason"
    );

    // Round two: red again at a new head, with nothing left to take over.
    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    let before = action_count(&reg, &group, "agent-spawn");
    let handed = tick_until_handback(&reg, &group, &gh, 30_000);
    let (_pr, agent) = handed.handbacks.first().cloned().unwrap_or_else(|| {
        panic!("a hand-back with no pane to take over must OPEN one: {handed:?}")
    });
    assert_eq!(
        action_count(&reg, &group, "agent-spawn") - before,
        1,
        "the take-over arm must not have swallowed the spawn — a drive whose worker pane \
         has been released would otherwise never get another one"
    );
    assert_ne!(agent, w.id, "…and the pane it hands to is a new one");
    assert_eq!(
        handback_panes(&reg, &group),
        vec!["reused".to_string(), "spawned".to_string()],
        "…which the audit row says in its own word, beside round one's `reused` — the \
         two words in one drive, which is what makes this a control rather than a \
         second copy of the reuse test"
    );
}

/// Tick until the drive hands back, or give up — the sequence
/// `a_handback_to_a_session_with_no_live_pane_still_opens_one` needs after a
/// release, where the arc back into `fix-wait` may take a tick longer than the
/// hand-back tests that never left it.
///
/// Bounded and loud: four ticks, then the caller's own `expect` reports an empty
/// `handbacks`, so a drive that stopped handing back reads as a failure rather
/// than as a hang.
fn tick_until_handback(
    reg: &OrchRegistry,
    group: &GroupId,
    gh: &FakeGh,
    from_ms: u64,
) -> RdDriveReport {
    let mut last = reg.rd_drive_group_with(group, gh, from_ms);
    for k in 1..4u64 {
        if !last.handbacks.is_empty() {
            return last;
        }
        last = reg.rd_drive_group_with(group, gh, from_ms + k * 10_000);
    }
    last
}

/// **#3203's other half: the release reaches every worker pane on the session,
/// not only the one the drive last resumed.**
///
/// On PR #3198 the ORIGINAL worker pane sat idle through two hand-backs and two
/// releases. `releasable` answers per ROLE, and the worker role's release read
/// `worker_agent` alone, so a pane a hand-back had superseded stayed alive, idle
/// and counted against the live-delegate cap for the rest of the drive.
///
/// **The fixture builds the superseded pane the way the driver really produced
/// one** — `driven`'s worker has no terminal, so a hand-back cannot reuse or
/// take over and spawns instead, and the pane it supersedes is what is left
/// behind. Reaching into `prior_worker_agents` would pin the release against a
/// record shape rather than against the state the tick produces.
///
/// **The negative control is the pane that is NOT released**, and it is what
/// separates this from "kill everything on the session": the barrier
/// (`release_driven_pane` — idle, alive, bound to a terminal, not a manager) is
/// still asked per pane, so a superseded pane that is BUSY survives. An
/// implementation that widened the population and dropped the barrier passes the
/// first row and fails the second.
///
/// **What varies the arms is whether the superseded pane has FINISHED its turn,
/// and getting that wrong is what CI caught twice.** A pane the driver spawns is
/// given the fix brief as its task, so it is born mid-turn and `idle_since_ms`
/// is `None` — meaning a fixture that only withholds a `send_prompt` leaves the
/// pane busy in BOTH arms, the barrier refuses it in both, and the loop varies
/// nothing. So the idle arm makes the pane idle the way a real one becomes idle:
/// it `report`s. That a SUPERSEDED worker's report is consumed and moves the
/// drive nowhere is #1871 B2's own pinned behaviour
/// (`a_superseded_worker_pane_is_still_intercepted_and_never_moves_the_drive`),
/// which is what makes it usable here: it changes the pane's idleness and
/// nothing else about the drive.
///
/// Both arms are walked before anything is compared, for the reason
/// `a_pane_that_is_idle_but_not_delivery_ready_is_not_reused_for_a_handback`
/// gives: an assert-per-arm loop evidences only the arm it stopped at.
#[test]
fn a_release_reaches_every_worker_pane_the_drive_owns_on_that_session() {
    // (the superseded pane is busy, panes released, that pane is still alive)
    type Row = (bool, usize, bool);
    let mut observed: Vec<Row> = Vec::new();

    for busy in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, _s) = driven(&reg, &repo, &gh);
        let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
        with_pane(&reg, &orch.id, 7001);
        reg.set_pr_head_override(Some(HEAD_A.to_string()));

        // Hand-back one opens a pane.
        gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
        let first = reg.rd_drive_group_with(&group, &gh, 10_000);
        let (_pr, w1) = first.handbacks.first().cloned().expect("the drive hands back");

        // Hand-back two supersedes it — again by spawning, for the same reason.
        gh.set_facts("OPEN", HEAD_B);
        reg.set_pr_head_override(Some(HEAD_B.to_string()));
        reg.rd_drive_group_with(&group, &gh, 20_000);
        let second = reg.rd_drive_group_with(&group, &gh, 30_000);
        let (_pr, w2) = second.handbacks.first().cloned().expect("a second red hands back");
        assert_ne!(w1, w2, "busy={busy}: the fixture needs two panes to have two subjects");
        assert_eq!(
            reg.rd_owner(&group, &w1).map(|(_pr, p)| p.current),
            Some(false),
            "busy={busy}: the fixture's premise — w1 is superseded and still owned"
        );

        // **The terminal is given in BOTH arms**, and that is not tidying: the
        // barrier refuses a pane bound to none, so an arm without one fails on
        // the barrier's third condition rather than on the population this test
        // is about — which is how the first draft of this test read red against
        // the fix. Turn-state is then the only thing that varies.
        with_pane(&reg, &w1, 7301);
        if !busy {
            // The superseded pane finishes its turn. Consumed and inert by
            // #1871 B2, so the only thing it changes is `idle_since_ms` — which
            // is exactly the barrier condition this arm needs to satisfy.
            report_as(&reg, &group, &w1, Role::Worker, "done");
        }
        assert_eq!(
            reg.agent(&w1).and_then(|a| a.idle_since_ms).is_none(),
            busy,
            "busy={busy}: the fixture's premise — the superseded pane is mid-turn in one \
             arm and finished in the other, and a driver-spawned pane is born mid-turn, \
             so this is the assertion that keeps the two arms from being the same arm"
        );
        assert_eq!(
            reg.rd_owner(&group, &w1).map(|(_pr, p)| p.current),
            Some(false),
            "busy={busy}: …and it is still SUPERSEDED either way — a report that made it \
             current again would make this a test about the current pane"
        );

        // The current worker reports done; the drive consumes it and releases.
        report_as(&reg, &group, &w2, Role::Worker, "done");
        let report = reg.rd_drive_group_with(&group, &gh, 40_000);
        let released: Vec<String> = report.released.iter().map(|(_, _, a)| a.clone()).collect();
        assert!(
            released.contains(&w2),
            "busy={busy}: the CURRENT pane is released either way — that is #2501, and if it \
             stopped happening this test would be measuring the wrong thing: {released:?}"
        );
        observed.push((
            busy,
            released.len(),
            reg.agent(&w1).map(|a| a.status != AgentStatus::Dead).unwrap_or(false),
        ));
    }

    assert_eq!(
        observed,
        vec![
            // Idle and superseded: released alongside the current pane, so the
            // drive leaves no pane behind on a session it has finished with.
            (false, 2, false),
            // Busy and superseded: the barrier refuses it, exactly as it refuses
            // a busy CURRENT pane, and the current one still goes. Widening the
            // population is not the same as dropping the guard.
            (true, 1, true),
        ],
        "the release population is every worker pane the drive owns, filtered by the same \
         per-pane barrier — never a blanket kill of the session"
    );
}

/// **Both `rd_handback` call sites write the `pane` field** (#3203 × #2811 S10).
///
/// S10 added a SECOND caller — the restart re-hand-back, which takes no arc and
/// spends no counter — and it builds its own `rd-handback` row rather than
/// sharing the `fail`-route arm's. A field added to one row and not the other is
/// exactly the drift a closed vocabulary exists to prevent, and nothing about it
/// is a compile error: the row simply ships without the key and every reader
/// counting take-overs silently under-counts.
///
/// **The two arms are the two `why` values, and each carries the `pane` word its
/// own situation earns.** A restart took every pane with the old process, so the
/// re-hand-back finds nothing on the session and `spawned` is the honest answer —
/// which also makes this the one place the positive control and the second call
/// site are the same assertion.
///
/// The paired `why` is asserted beside it so this cannot pass by reading the
/// wrong row: `restart` is the arm under test, and a fixture that had somehow
/// produced an ordinary `ci-red` hand-back would be measuring the site that was
/// already covered.
#[test]
fn the_restart_re_handback_row_carries_the_pane_field_too() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (reg, group) = fix_wait_across_a_restart(dir.path(), &repo, &gh);

    let before = handback_panes(&reg, &group);
    assert_eq!(
        before,
        vec!["spawned".to_string()],
        "the fixture's own first hand-back, for reference — `driven`'s worker has no \
         terminal, so there was never a pane to reuse or take over"
    );

    let out = reg.rd_drive_group_with(&group, &gh, 50_000);
    assert!(
        !out.handbacks.is_empty(),
        "the fixture's premise: the first tick after a restart re-hands-back (#2811 S10): \
         {out:?}"
    );

    let rows = audit_details(&reg, &group, "rd-handback");
    let row = rows.last().expect("the re-hand-back owes an audit row");
    assert_eq!(
        row["why"],
        json!("restart"),
        "the row under test must be the RESTART arm's, not the fail route's: {row}"
    );
    assert_eq!(
        row["pane"],
        json!("spawned"),
        "S10's re-hand-back row must carry #3203's `pane` field like the other call site \
         — a restart left no pane on the session, so `spawned` is what it earns, and a \
         missing key here is a reader silently under-counting take-overs: {row}"
    );
}

// ── review round 1: the two residuals, pinned ───────────────────────────────

/// **Finding 1, pinned.** The take-over arm falls through to a spawn when the
/// DELIVERY is refused, and a full queue is a refusal that needs nothing to be
/// wrong — so a hand-back landing on a pane at `QUEUE_MAX_PER_PANE` still opens
/// a second pane on a live session.
///
/// **A disclosed residual is a counterfactual, and only a test that performs
/// the edit pins one.** The code comment, the design note and the PR body all
/// now say this corner exists; without this test the suite pins only the arms
/// that work, and the disclosure could go false — in either direction — with
/// nothing red to say so. Both directions are asserted: the pane really is
/// still live afterwards (so this is genuinely a second pane on a live session,
/// not a replacement for a dead one), and the refusal really is on the audit log
/// (so the one remaining route to a duplicate is not silent, which is the whole
/// of what #2089 asked of the reuse arm).
///
/// **The negative control is the `depth = 7` arm**, and it carries the
/// discriminator. Seven queued entries is a pane just as far behind, just as
/// un-ready, and just as mid-turn — the take-over arm takes it anyway. So an
/// implementation that stopped taking over "backed-up" panes generally, or one
/// whose predicate refused on queue depth rather than on the delivery's own
/// answer, passes the `depth = 8` row and fails this one. Without it this test
/// would pass against a driver that had quietly reverted to spawning.
///
/// The queue is filled through `deliver_prompt` on a paused group, which is how
/// `a_pane_that_is_idle_but_not_delivery_ready_is_declined_by_name_then_taken_over`
/// builds its own `queued` arm — a real admitted queue, not a fabricated depth.
#[test]
fn a_takeover_refused_by_a_full_queue_says_so_and_falls_through() {
    // (queued depth, the hand-back landed in the existing pane, panes opened,
    //  `rd-takeover-declined` rows naming it, that pane still live)
    type Row = (usize, bool, usize, usize, bool);
    let mut observed: Vec<Row> = Vec::new();

    for depth in [7usize, 8] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let group = reg.create_group(&repo.path(), rails()).unwrap().id;
        let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).unwrap();
        let session = w.session_id.clone().expect("claude mints a session id at spawn");
        make_delivery_land(&reg, &group, &w.id, 7601);
        make_pane_ready(&reg, 7601, true);
        let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
        assert_eq!(out["driving"], json!(true), "depth={depth}: drive_review refused: {out}");
        reg.set_pr_head_override(Some(HEAD_A.to_string()));

        // Back the pane up to this arm's depth, through the real admission path.
        for k in 0..depth {
            reg.deliver_prompt(&w.id, &format!("[test] backlog {k}"), "orch-1", Delivery::MidSession)
                .unwrap_or_else(|e| panic!("depth={depth}: entry {k} must be admitted: {e}"));
        }
        assert_eq!(
            reg.queue_depth(7601),
            depth,
            "depth={depth}: the fixture's premise — the pane is backed up to exactly this depth"
        );

        gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
        let before = action_count(&reg, &group, "agent-spawn");
        let handed = reg.rd_drive_group_with(&group, &gh, 10_000);
        let (_pr, agent) = handed
            .handbacks
            .first()
            .cloned()
            .unwrap_or_else(|| panic!("depth={depth}: the drive hands back: {handed:?}"));
        let declined = audit_details(&reg, &group, "rd-takeover-declined")
            .iter()
            .filter(|d| d["pane"] == json!(w.id))
            .count();

        observed.push((
            depth,
            agent == w.id,
            action_count(&reg, &group, "agent-spawn") - before,
            declined,
            reg.agent(&w.id).is_some_and(|a| a.status != AgentStatus::Dead),
        ));
    }

    assert_eq!(
        observed,
        vec![
            // Below the cap: the delivery is admitted behind the backlog and the
            // take-over happens, declining nothing. This is the control — it is
            // what stops the row below being satisfied by a driver that spawns
            // for any backed-up pane.
            (7, true, 0, 0, true),
            // At the cap: `deliver_prompt` answers `Err`, the arm falls through,
            // and a SECOND pane is opened on a session whose first pane is still
            // live. That is #3203's own shape, narrowed to this corner and
            // disclosed rather than closed — and the `rd-takeover-declined` row
            // is what keeps it from looking like a session that had no pane.
            (8, false, 1, 1, true),
        ],
        "each row is (queued depth, the hand-back landed in the existing pane, panes opened, \
         `rd-takeover-declined` rows naming it, that pane still live). The second row is a \
         DISCLOSED residual, not a target to fix silently: if a later change closes it, this \
         test reddens and the code comment, the design note and the user docs all have to \
         stop saying the corner exists."
    );
}

/// **Finding 3, pinned.** A superseded pane the release barrier SKIPS is never
/// re-asked on a later tick.
///
/// `releasable` gates its NON-terminal worker candidate on
/// `!entry.worker_agent.is_empty()`, and the release that just happened cleared
/// that field — so no later tick of a LIVE drive names the worker role again. A
/// superseded pane that was mid-turn at the release tick therefore stays owned
/// and counting against the cap until its own turn ends and the idle reaper
/// takes it.
///
/// Narrowed by #3250 at one end and no further: the SATISFIED exit now asks
/// about every live pane on the session, so a pane still alive there is taken.
/// This drive never reaches one — it is red at `HEAD_B` for every tick below —
/// which is what keeps the window this pins a real one.
///
/// **This pins the residual the design note now discloses, in the direction that
/// can go quietly false.** The busy arm of
/// `a_release_reaches_every_worker_pane_the_drive_owns_on_that_session` shows the
/// skip happening on ONE tick; what it cannot show is that no LATER tick fixes
/// it, because it never runs another. This runs three more and asserts the pane
/// is still there, so the note's "not re-asked" is a checked claim rather than
/// an assumption about code nobody exercised past that point.
///
/// The `rd-worker-released` count is asserted as well as the liveness, because
/// the two fail differently: a later tick that released the pane moves both, and
/// a later tick that emitted a row without a kill moves only the count — which
/// is the "a row means a pane went" rule §5.4 rests on.
#[test]
fn a_superseded_pane_the_barrier_skips_is_not_re_asked_on_a_later_tick() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _s) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(&reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));

    gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
    let first = reg.rd_drive_group_with(&group, &gh, 10_000);
    let (_pr, w1) = first.handbacks.first().cloned().expect("the drive hands back");

    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 20_000);
    let second = reg.rd_drive_group_with(&group, &gh, 30_000);
    let (_pr, w2) = second.handbacks.first().cloned().expect("a second red hands back");
    assert_ne!(w1, w2, "the fixture needs two panes to have two subjects");

    // w1 is superseded and left MID-TURN — the state the barrier skips. It keeps
    // the task it was spawned with, so nothing has to be done to it; the pane is
    // given a terminal so that the terminal is not what refuses it.
    with_pane(&reg, &w1, 7302);
    assert!(
        reg.agent(&w1).is_some_and(|a| a.idle_since_ms.is_none()),
        "the fixture's premise: the superseded pane is mid-turn, so the barrier skips it"
    );

    report_as(&reg, &group, &w2, Role::Worker, "done");
    let release = reg.rd_drive_group_with(&group, &gh, 40_000);
    let released: Vec<String> = release.released.iter().map(|(_, _, a)| a.clone()).collect();
    assert_eq!(released, vec![w2.clone()], "the release tick takes the current pane only");
    let rows_after_release = audit_details(&reg, &group, "rd-worker-released").len();
    assert!(
        reg.agent(&w1).is_some_and(|a| a.status != AgentStatus::Dead),
        "the fixture's premise: the skipped pane survived the release tick"
    );

    // Three more ticks. Nothing re-asks.
    for (k, at) in [50_000u64, 60_000, 70_000].into_iter().enumerate() {
        let out = reg.rd_drive_group_with(&group, &gh, at);
        assert!(
            out.released.is_empty(),
            "tick {k} released something after the worker role had already gone: {:?}",
            out.released
        );
    }

    assert!(
        reg.agent(&w1).is_some_and(|a| a.status != AgentStatus::Dead),
        "the residual the design note discloses: a superseded pane the barrier skipped is \
         never re-asked, so it is still live — reclaimed by the idle reaper once its own \
         turn ends, not by the driver. If this goes red the driver has started re-asking, \
         which is better behaviour but makes the note's disclosure false"
    );
    assert_eq!(
        audit_details(&reg, &group, "rd-worker-released").len(),
        rows_after_release,
        "…and no later tick wrote a release row for it either — a row means a pane went (§5.4)"
    );
}
