//! Pane reuse by delivery state (#2089), resumed-not-duplicated lanes (#2109), declined and dead lane panes.
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── #2089: reuse takes DELIVERY STATE as "this pane will read what I type" ──

/// The pure predicate, whole: which `(queue depth, last-delivery outcome)` pairs
/// read ready, and what each refusal is CALLED (#2089).
///
/// The precedence rows are the ones a table-shaped rewrite would lose. A
/// non-empty queue decides on its own — an implementation that asked the last
/// delivery first would answer `Unconfirmed` for `(1, Some(false))` and
/// `NoRecord` for `(3, None)`, both of which are true statements about a pane
/// whose real problem is the queue, and both of which would put the wrong word
/// on the `rd-reuse-declined` row a human reads.
///
/// The `as_str` row is not decoration either: those three words ARE the audit
/// contract (§5.4), so a rename that leaves the variants intact still breaks
/// every reader keyed on them.
#[test]
fn the_reuse_readiness_predicate_is_a_confirmed_delivery_and_an_empty_queue() {
    assert_eq!(pane_delivery_readiness(0, Some(true)), None, "the only ready pair");

    assert_eq!(pane_delivery_readiness(0, Some(false)), Some(PaneNotReady::Unconfirmed));
    assert_eq!(pane_delivery_readiness(0, None), Some(PaneNotReady::NoRecord));
    assert_eq!(pane_delivery_readiness(1, Some(true)), Some(PaneNotReady::Queued));

    assert_eq!(
        pane_delivery_readiness(1, Some(false)),
        Some(PaneNotReady::Queued),
        "queue depth is asked FIRST: a pane that is both reads as queued"
    );
    assert_eq!(pane_delivery_readiness(3, None), Some(PaneNotReady::Queued));

    assert_eq!(
        [PaneNotReady::Queued, PaneNotReady::Unconfirmed, PaneNotReady::NoRecord]
            .map(PaneNotReady::as_str),
        ["queued", "unconfirmed", "no-record"],
        "the words the `rd-reuse-declined` row carries (§5.4)"
    );
}

/// **A pane the REAPER calls idle is not thereby a pane at its prompt** (#2089,
/// deferred out of #1967) — and since #3203, what that buys is a NAMED refusal
/// rather than a second pane.
///
/// `idle_since_ms` is stamped when an agent reports done or is spawned without a
/// task. A pane that then parks behind a dialog — a permission prompt, a CLI
/// question, an `allow-scripts` gate — is still idle by that signal, and
/// `deliver_prompt` admits the brief into its queue and answers `Ok`, so the
/// caller's fallback-to-spawn never fires and the drive sits until
/// `fix-stalled`. The reuse arm therefore asks delivery state as well: last
/// delivery CONFIRMED, and nothing queued behind it.
///
/// **#3203 retracts the CONSEQUENCE of that refusal and nothing else, and this
/// test is repinned onto what survives.** The predicate is unchanged and still
/// names why each pane is not cleanly reusable; what changed is where a refused
/// pane falls through to. It used to be `rd_spawn`, which put a SECOND live pane
/// on a session that already had one — measured on PR #3198 as three panes
/// writing one worktree. It is now the take-over arm, which types the brief into
/// that same pane anyway, because landing behind a turn costs latency that
/// `fix-stalled` already bounds while a second pane costs the checkout. So
/// `opened` is 0 on every row now and the assertion moved off it.
///
/// **The `rd-reuse-declined` WORD is what discriminates the four arms**, and it
/// is the whole reason the reuse arm was kept in front of the take-over arm
/// rather than replaced by it: `ready` declines nothing, and each refusing arm
/// names the one fact the predicate read. An implementation that dropped the
/// readiness test altogether — the obvious way to "fix" #3203 — passes every
/// other column here and fails these three rows, which is the point.
///
/// **Every arm is walked and the whole table is compared ONCE, rather than each
/// arm asserting as it goes.** A red evidences only the assertion it reached, and
/// an assert-per-arm loop stops at the first failure — the round that reddened
/// this test for the first time reached `unconfirmed` and never ran `no-record`
/// or `queued` at all, so three arms would have been claimed on one arm's
/// evidence. Collecting first makes one run say what every arm did.
#[test]
fn a_pane_that_is_idle_but_not_delivery_ready_is_declined_by_name_then_taken_over() {
    // (arm, the hand-back landed in the existing pane, panes opened, decline
    // reasons, fix brief typed into the candidate) — filled per arm, compared
    // once at the end.
    type Row = (&'static str, bool, usize, Vec<String>, bool);
    let mut observed: Vec<Row> = Vec::new();

    for arm in ["ready", "unconfirmed", "no-record", "queued"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let group = reg.create_group(&repo.path(), rails()).unwrap().id;
        let w = reg.spawn_agent(&group, Role::Worker, "w", "", false, None).unwrap();
        let session = w.session_id.clone().expect("claude mints a session id at spawn");
        assert!(
            reg.agent(&w.id).unwrap().idle_since_ms.is_some(),
            "{arm}: every arm's pane is IDLE by the reaper's signal — that is the premise \
             this test exists to show is not enough on its own"
        );
        // A terminal and a paused group, so a delivery is really admitted (see
        // `make_delivery_land`); then this arm's delivery state on top of it.
        make_delivery_land(&reg, &group, &w.id, 7901);
        match arm {
            "ready" | "queued" => make_pane_ready(&reg, 7901, true),
            "unconfirmed" => make_pane_ready(&reg, 7901, false),
            // …and "no-record" writes nothing at all.
            _ => {}
        }
        if arm == "queued" {
            reg.deliver_prompt(&w.id, "[test] not pasted yet", "orch-1", Delivery::MidSession)
                .expect("a paused group admits a delivery into the queue");
        }
        assert_eq!(
            reg.queue_depth(7901),
            usize::from(arm == "queued"),
            "{arm}: the fixture's own premise — only the queued arm has anything waiting"
        );

        let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
        assert_eq!(out["driving"], json!(true), "{arm}: drive_review refused: {out}");

        // Counted across the HAND-BACK tick alone, for the reason
        // `a_handback_resumes_into_the_live_idle_pane_on_that_session` states:
        // a baseline taken before the whole drive folds in the reviewer lane's
        // own spawn, which is a pane the hand-back neither opens nor reuses.
        reg.rd_drive_group_with(&group, &gh, 10_000);
        reg.rd_drive_group_with(&group, &gh, 20_000);
        gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
        gh.set_facts("OPEN", HEAD_B);
        reg.rd_drive_group_with(&group, &gh, 30_000);
        let before = action_count(&reg, &group, "agent-spawn");
        let handed = reg.rd_drive_group_with(&group, &gh, 40_000);
        let (_pr, agent) = handed.handbacks.first().cloned().unwrap_or_else(|| panic!("{arm}: the drive hands back"));
        let opened = action_count(&reg, &group, "agent-spawn") - before;

        let declined: Vec<String> = audit_details(&reg, &group, "rd-reuse-declined")
            .into_iter()
            .filter(|d| d["pane"] == json!(w.id))
            .map(|d| d["reason"].as_str().unwrap_or("<no reason field>").to_string())
            .collect();
        let briefed =
            texts_to(&reg, &group, &w.id).iter().any(|t| t.contains("is back with you at head"));

        observed.push((arm, agent == w.id, opened, declined, briefed));
    }

    let expected: Vec<Row> = vec![
        // The control. A confirmed last delivery with an empty queue is reused
        // cleanly, declining nothing — so the three rows below are about the
        // predicate having READ something, not about a driver that declines
        // everything and takes it over regardless.
        ("ready", true, 0, vec![], true),
        // The last delivery is on record as not having landed: its text may
        // still be sitting unsubmitted in that box.
        ("unconfirmed", true, 0, vec!["unconfirmed".into()], true),
        // Nothing was ever delivered to this pty, so there is no evidence
        // either way — "we could not look" is not "there was nothing there".
        ("no-record", true, 0, vec!["no-record".into()], true),
        // Something is already waiting to be pasted; a brief admitted now lands
        // behind it. Queue depth is asked first, so this arm reads `queued`
        // even though its last delivery is confirmed.
        ("queued", true, 0, vec!["queued".into()], true),
    ];
    assert_eq!(
        observed, expected,
        "each row is (arm, the hand-back landed in the existing pane, panes opened, decline \
         reasons, fix brief typed into it). Since #3203 every arm lands in that pane and opens \
         nothing — the driver may not put a second pane on a session that already has one — so \
         the DECLINE REASONS are what separate the arms: an empty vector for the pane that was \
         cleanly reusable, and the one word the predicate actually read for each pane that was \
         not. An implementation that deleted the readiness test to close #3203 empties all four \
         vectors and fails here."
    );
}

/// **The two conditions are a CONJUNCTION, and it may not collapse into either
/// half** (#2089).
///
/// #2089 asked for the `idle_since_ms` test to be REPLACED by delivery
/// readiness. It is narrowed instead, and this is the pin for why: a pane that
/// is delivery-ready is exactly what one MID-TURN looks like — it took a brief,
/// the brief confirmed, and its queue is empty because the CLI is now thinking.
/// The two conditions are therefore still asked separately, and this test is
/// what stops them being folded into one.
///
/// **What #3203 retracted is the sentence "is this agent mid-thought? is not a
/// question a driver may answer" — for the hand-back FALL-THROUGH only.** A
/// mid-turn pane is still not something the reuse arm will take, and
/// `rd_open_lane` still opens a fresh reviewer rather than typing into a working
/// one. What changed is what a refused hand-back does next: it takes the pane
/// over anyway, because the alternative it used to take was a second pane on the
/// same session and worktree (#3198, three of them). The delivery is QUEUED, not
/// an interrupt — the CLI reads the brief when its current turn ends.
///
/// **So the axis moved from `opened` to the `pane` WORD**, and it had to: since
/// #3203 both arms land in the same pane and open nothing, so a table still
/// keyed on those columns would be identical in every column and would pass
/// under any implementation at all. `reused` and `taken-over` are the two arms
/// of `handback_pane`, and which one is written is decided by exactly the fact
/// this test varies.
///
/// The two arms differ in ONE fact — whether the worker was spawned with a task,
/// which is what stamps `idle_since_ms` — and both panes are delivery-ready, so
/// this test is blind to the readiness half by construction.
///
/// **The empty `rd-reuse-declined` assertion says WHICH half refused**: a
/// non-idle pane never reaches the readiness test at all, so a decline row for
/// it would mean the two conditions had been folded into one.
///
/// Both arms are walked before anything is compared, for the reason
/// `a_pane_that_is_idle_but_not_delivery_ready_is_not_reused_for_a_handback`
/// gives: an assert-per-arm loop evidences only the arm it stopped at.
#[test]
fn a_delivery_ready_pane_that_is_not_idle_is_taken_over_rather_than_reused() {
    // (idle, the `pane` word on the hand-back row, panes opened, decline rows
    // naming it)
    type Row = (bool, String, usize, usize);
    let mut observed: Vec<Row> = Vec::new();

    for idle in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let group = reg.create_group(&repo.path(), rails()).unwrap().id;
        // A task-less spawn is stamped idle at birth; one carrying a task is
        // not. That is the production route to both states, and the only thing
        // this loop varies.
        let task = if idle { "" } else { "still mid-turn on its last brief" };
        let w = reg.spawn_agent(&group, Role::Worker, "w", task, false, None).unwrap();
        let session = w.session_id.clone().expect("claude mints a session id at spawn");
        assert_eq!(
            reg.agent(&w.id).unwrap().idle_since_ms.is_some(),
            idle,
            "idle={idle}: the fixture's premise"
        );
        make_delivery_land(&reg, &group, &w.id, 7911);
        make_pane_ready(&reg, 7911, true);
        assert!(
            reg.pane_readiness(7911).is_none(),
            "idle={idle}: BOTH arms' panes are delivery-ready, so readiness cannot be what \
             separates them"
        );

        let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 0);
        assert_eq!(out["driving"], json!(true), "idle={idle}: drive_review refused: {out}");

        reg.rd_drive_group_with(&group, &gh, 10_000);
        reg.rd_drive_group_with(&group, &gh, 20_000);
        gh.set_checks(r#"[{"name":"build","state":"FAILURE","link":"x"}]"#);
        gh.set_facts("OPEN", HEAD_B);
        reg.rd_drive_group_with(&group, &gh, 30_000);
        let before = action_count(&reg, &group, "agent-spawn");
        let handed = reg.rd_drive_group_with(&group, &gh, 40_000);
        let (_pr, agent) = handed
            .handbacks
            .first()
            .cloned()
            .unwrap_or_else(|| panic!("idle={idle}: the drive hands back"));
        let opened = action_count(&reg, &group, "agent-spawn") - before;
        let declined = audit_details(&reg, &group, "rd-reuse-declined")
            .iter()
            .filter(|d| d["pane"] == json!(w.id))
            .count();

        assert_eq!(
            agent, w.id,
            "idle={idle}: both arms must land in the pane that is already on this session — \
             a second live pane on one worktree is #3203, and it is what the `pane` word \
             below then tells apart"
        );
        let pane = handback_panes(&reg, &group)
            .pop()
            .unwrap_or_else(|| panic!("idle={idle}: the hand-back owes an audit row"));
        observed.push((idle, pane, opened, declined));
    }

    assert_eq!(
        observed,
        vec![
            // The control: idle AND ready is the pane the reuse arm takes,
            // cleanly, and the row says so in its own word.
            (true, "reused".to_string(), 0, 0),
            // Delivery-ready but MID-TURN. The reuse arm still refuses it — the
            // brief lands behind whatever that agent is doing — so the pane is
            // TAKEN OVER instead, which is a different word for a different
            // thing (§5.4). The ZERO decline rows say WHICH half refused it: a
            // pane that is not idle is never a readiness candidate at all, so a
            // row here would mean the two conditions had been folded into one.
            (false, "taken-over".to_string(), 0, 0),
        ],
        "each row is (idle, the `pane` word on the `rd-handback` row, panes opened, \
         `rd-reuse-declined` rows naming it). Both panes are delivery-ready, so an \
         implementation that dropped the idle conjunct sends the mid-turn pane through the \
         REUSE arm and the second row's word goes `reused`; one that dropped the take-over arm \
         opens a pane and fails the assertion above the push."
    );
}

// ── #2109: lanes are resumed, never duplicated, and a starved drive says so ──

/// The same roster with the reviewer block on a CLI that does **not** pre-assign
/// a session id.
///
/// `spawn_agent_bound` mints a uuid up front for `cli == "claude"` and for
/// nothing else, so this one fixture is the difference between a lane whose
/// session is on its record at `open_lane` time and one whose session exists
/// only on the pane, discovered after boot. Every claude fixture in this file
/// is on the first side of that line, which is why #2109 was invisible here.
const WORKFLOW_LATE_SESSION_REVIEWER: &str = r#"version: 1
blocks:
  - id: worker
    kind: worker
  - id: rev-std
    name: Standard review
    kind: reviewer
    cli: copilot
gates:
  merge:
    require: all-pass
    reviewers: [rev-std]
    routing:
      - paths: [src/**]
        reviewers: [rev-std]
driver:
  enabled: true
"#;

pub(crate) fn rows_for(reg: &OrchRegistry, group: &GroupId, action: &str) -> Vec<serde_json::Value> {
    reg.audit_log(group)
        .into_iter()
        .filter(|e| e.action == action)
        .map(|e| e.detail)
        .collect()
}

/// Every `rd-lane-spawned` row this group recorded, in order.
pub(crate) fn lane_spawn_rows(reg: &OrchRegistry, group: &GroupId) -> Vec<serde_json::Value> {
    rows_for(reg, group, "rd-lane-spawned")
}

/// Drive one PR to the end of its first `review-wait` round, and answer the
/// group, the orchestrator pane and the lane pane that round opened.
///
/// Factored out because three tests below need the same seven lines and differ
/// only in what they do between the rounds; three inline copies is how the
/// second and third drift from the first.
pub(crate) fn lane_round_one(reg: &OrchRegistry, repo: &Repo, gh: &FakeGh) -> (GroupId, String, String) {
    let (group, _s) = driven(reg, repo, gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    with_pane(reg, &orch.id, 7001);
    reg.set_pr_head_override(Some(HEAD_A.to_string()));
    reg.rd_drive_group_with(&group, gh, 10_000);
    let first = reg.rd_drive_group_with(&group, gh, 20_000);
    let (_pr, _b, lane) = first.lanes_opened.first().cloned().expect("lane 0 opens");
    (group, orch.id, lane)
}

/// **#2109 ask 1.** A lane is resumed on the conversation it already had, and
/// the lane RECORD is not the only place that conversation is written down.
///
/// The fixture is the whole finding. `LaneRecord::session` is what the spawn
/// RETURNED, and `spawn_agent_bound` returns one only for claude; copilot and
/// opencode mint theirs after boot, and the watcher binds the discovered id to
/// the PANE and the roster row — never to the drive's own lane record. So on
/// those CLIs the recorded session was empty for the life of the lane, the
/// resume arm read it, found nothing, and opened a fresh pane on a fresh
/// conversation every round. Measured on the dogfood: nine reviewer panes across
/// three PRs where six would have done, each new pane briefed `Your previous
/// verdict was fail` about a verdict it had never recorded.
///
/// **Round one is the control**, and it is not decoration: it pins that this
/// lane really does spawn with no session on its record, so the round-two
/// assertion is about the fall-back and not about a field that was populated all
/// along. A claude fixture passes the round-two assertion under the defect.
#[test]
fn a_lane_whose_session_arrived_after_the_spawn_is_resumed_rather_than_respawned() {
    const DISCOVERED: &str = "9f2c41ab-7777-4444-8888-1c0de5e55107";
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::with(WORKFLOW_LATE_SESSION_REVIEWER);
    let gh = FakeGh::green(HEAD_A);
    let (group, _orch, lane) = lane_round_one(&reg, &repo, &gh);

    assert_eq!(
        reg.agent(&lane).expect("the lane is on the roster").session_id,
        None,
        "the control: this CLI mints its session AFTER boot, so the spawn returned none"
    );
    let round1 = lane_spawn_rows(&reg, &group);
    assert_eq!(round1.len(), 1);
    assert_eq!(round1[0]["resumed"], json!(false), "a first round is never a resume");
    assert_eq!(
        round1[0]["session"],
        json!(""),
        "and the lane record therefore has no session to resume, which is the premise"
    );

    // The session watcher finds the id the CLI minted and binds it — to the pane
    // and the roster, which is everywhere it has ever been written.
    assert!(
        reg.associate_session(&group, &lane, DISCOVERED),
        "the watcher binds a discovered session to its pane"
    );

    // The head moves, which stales the lane and sends the drive back round.
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 30_000); // review-wait -> ci-wait (arc 6)
    reg.rd_drive_group_with(&group, &gh, 40_000); // ci-wait -> review-wait (arc 2)
    let again = reg.rd_drive_group_with(&group, &gh, 50_000);
    let (_pr, _b, lane2) =
        again.lanes_opened.first().cloned().expect("the staled lane re-opens at the new head");

    let rows = lane_spawn_rows(&reg, &group);
    assert_eq!(rows.len(), 2, "one row per round: {rows:?}");
    assert_eq!(
        rows[1]["resumed"],
        json!(true),
        "round two must CONTINUE the reviewer conversation, not start a new one: {rows:?}"
    );
    assert_eq!(
        rows[1]["session"],
        json!(DISCOVERED),
        "and on the session the watcher bound to round one's pane: {rows:?}"
    );
    assert_eq!(
        reg.agent(&lane2).expect("the resumed lane is on the roster").session_id.as_deref(),
        Some(DISCOVERED),
        "the pane the brief went to is running that same conversation"
    );
    assert!(
        rows_for(&reg, &group, "rd-lane-resume-failed").is_empty(),
        "nothing refused this resume, so nothing may claim one failed"
    );
}

/// **#2109 ask 1, the other half.** A resume that cannot be performed still
/// opens a lane — and says on the record why it had to.
///
/// The fall-through already existed and was SILENT (`.or_else(|_| fresh(self))`
/// discarded the error), so a reviewer that lost its conversation and one that
/// never had one produced the same row, the same kind of pane and the same
/// brief. That is `rd-reuse-declined`'s argument one arm over: the refusal's
/// only other visible effect is a fresh pane, which is what "there was nothing
/// to resume" looks like too.
///
/// The refusal is manufactured the way a reaped worktree produces one — the
/// recorded workspace is gone and the session is not in the CLI's own store, so
/// `resolve_worker_resume_cwd` refuses rather than resuming into the group's
/// main clone (#338/#359).
#[test]
fn a_resume_that_cannot_be_performed_opens_a_fresh_lane_and_records_why() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _orch, lane) = lane_round_one(&reg, &repo, &gh);

    let session = reg
        .agent(&lane)
        .expect("the lane is on the roster")
        .session_id
        .expect("claude pre-assigns a session id at spawn");
    let cwd = reg.agent(&lane).unwrap().cwd;
    assert!(std::path::Path::new(&cwd).is_dir(), "the control: the workspace exists first");
    std::fs::remove_dir_all(&cwd).expect("take the lane's workspace away");

    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 30_000);
    reg.rd_drive_group_with(&group, &gh, 40_000);
    let again = reg.rd_drive_group_with(&group, &gh, 50_000);
    let (_pr, _b, lane2) = again
        .lanes_opened
        .first()
        .cloned()
        .expect("a resume that refuses must still open a lane, not park the drive");
    assert_ne!(lane2, lane, "a fresh pane, since the recorded one could not be resumed");

    let failed = rows_for(&reg, &group, "rd-lane-resume-failed");
    assert_eq!(failed.len(), 1, "one row for the one refused resume: {failed:?}");
    assert_eq!(failed[0]["block"], json!("rev-std"));
    assert_eq!(
        failed[0]["session"],
        json!(session),
        "the row names the conversation that was lost"
    );
    assert!(
        failed[0]["detail"].as_str().unwrap_or_default().contains("resume-"),
        "and quotes what refused rather than diagnosing it: {failed:?}"
    );
    let rows = lane_spawn_rows(&reg, &group);
    assert_eq!(
        rows[1]["resumed"],
        json!(false),
        "resumed records what HAPPENED, and a resume that fell through is not one: {rows:?}"
    );
}

/// **#2109 ask 2.** One block, one live pane, one round — a second is refused,
/// on the record, rather than opened.
///
/// §8's body-changed row re-briefs a lane at an UNCHANGED head, and where that
/// lane's pane is idle the reuse arm types the delta into it. Where the pane is
/// BUSY — still writing the review it was briefed for — the reuse declines on
/// readiness and the spawn used to mint a second pane on the same conversation.
/// Measured: `rev-1825` and `rev-1826` both reviewed PR #2104's round 2, and
/// `rev-1832` reported "a duplicate rev-std round-2 review from a parallel pane
/// landed 41 seconds after mine with the same verdict and lead finding".
///
/// **The head-move half is the discriminator, not a bonus.** A refusal keyed on
/// nothing but "this lane has a live pane" would pass the first half and block
/// every legitimate supersede — the pane reviewing a revision the drive has
/// moved past is exactly what `prior_agents` exists for. Asserting both is what
/// makes this a pin on the (pr, block, head) key rather than on "never twice".
#[test]
fn a_block_that_already_has_a_live_pane_at_this_head_is_refused_a_second_lane() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _orch, lane) = lane_round_one(&reg, &repo, &gh);
    assert_eq!(lane_spawn_rows(&reg, &group).len(), 1);

    // The body moves under the same head. The lane is stale, the drive wants it
    // re-briefed, and its pane is mid-review — never idle, so never a reuse
    // candidate.
    gh.set_body("b2");
    let blocked = reg.rd_drive_group_with(&group, &gh, 30_000);
    assert!(
        blocked.lanes_opened.is_empty(),
        "no second pane while round one's is still holding the round: {:?}",
        blocked.lanes_opened
    );
    assert_eq!(
        lane_spawn_rows(&reg, &group).len(),
        1,
        "and no second rd-lane-spawned row either"
    );
    let refused = rows_for(&reg, &group, "rd-lane-duplicate-refused");
    assert_eq!(refused.len(), 1, "the refusal is on the record: {refused:?}");
    assert_eq!(refused[0]["pane"], json!(lane), "naming the pane that holds the round");
    assert_eq!(refused[0]["head"], json!(HEAD_A));
    assert_eq!(refused[0]["block"], json!("rev-std"));
    assert_eq!(status_state(&reg, &group), "review-wait", "the drive backs off, it does not park");

    // Now the HEAD moves. That pane is reviewing a revision the drive has left,
    // so a successor is exactly what is owed — and the refusal must not stand in
    // its way.
    gh.set_facts("OPEN", HEAD_B);
    reg.set_pr_head_override(Some(HEAD_B.to_string()));
    reg.rd_drive_group_with(&group, &gh, 40_000);
    reg.rd_drive_group_with(&group, &gh, 50_000);
    let moved = reg.rd_drive_group_with(&group, &gh, 60_000);
    assert!(
        !moved.lanes_opened.is_empty(),
        "a head change is a new round, and the refusal is keyed on the head: {:?}",
        rows_for(&reg, &group, "rd-lane-duplicate-refused")
    );
    assert_eq!(
        rows_for(&reg, &group, "rd-lane-duplicate-refused").len(),
        1,
        "so no second refusal was recorded either"
    );
}

// ── #2162: a declined pane is not a pane that is still reviewing ────────────

/// **#2162.** A round whose fix is BODY-ONLY re-opens its lane instead of
/// deadlocking on the very pane the reuse arm has just declined.
///
/// The measured failure (PR #2140, v1.3.0-beta6) is two guards composing, each
/// right alone and both about ONE pane. `rd_reuse_pane` declined the lane's own
/// pane on readiness — the #2089 class, `unconfirmed` — and #2109's duplicate
/// guard then refused a replacement because that same pane was "live … briefed
/// at this head", which is true precisely because a body-only fix cannot move
/// the head. Too unconfirmed to reuse and too live to replace: 38 minutes with
/// no lane open, the same three rows every tick, `lane-stalled` structurally
/// unreachable because its arm exempts the lane that has answered, and a human
/// in the end killing the pane.
///
/// **The fixture reproduces the incident's own state, not a shortcut to it.**
/// The reuse arm only ever considers an IDLE pane, so `rd-reuse-declined`
/// appearing at all proves the pane was idle — this lane therefore reports
/// (which is what stamps `idle_since_ms`) and its last delivery is on record as
/// not having landed, which is the exact pair the audit rows on #2162 show.
/// Both PR-body seams are set, because `review_verdict` digests what `pr_body`
/// answers while the drive digests what `observe_pr` read: left unset the
/// verdict records an EMPTY digest, `body_changed` answers "cannot tell", the
/// stale `fail` reads as CURRENT, and the drive hands the worker back a second
/// time instead of ever reaching the re-brief.
///
/// **The lane reports AFTER the fail routes, and since #2501 that ordering is
/// load-bearing rather than incidental.** A lane already idle when its current
/// `fail` routes is one the driver RELEASES — it has answered about the revision
/// on the PR, so §3.1 item 5's first state is exactly it — and the pane this test
/// is about would be gone before the re-brief, leaving `rd-reuse-declined` empty
/// and the test green about nothing. Moving the report below the arc-5 tick keeps
/// the specimen in the class this test witnesses; both orderings are real,
/// because a reviewer's report and the driver's poll are asynchronous.
///
/// **The negative control is `a_block_that_already_has_a_live_pane_at_this_head_
/// is_refused_a_second_lane`**, which moves the same body under the same head
/// with the one difference that decides — that lane's pane never went idle.
/// Duplicating its assertions here would make a red ambiguous between the two.
#[test]
fn a_body_only_fix_round_re_opens_the_lane_whose_pane_went_idle() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let (group, _orch, lane) = lane_round_one(&reg, &repo, &gh);
    reg.set_pr_body_override(Some("b".to_string()));
    assert!(
        reg.agent(&lane).expect("the lane is on the roster").idle_since_ms.is_none(),
        "the control: a lane spawned WITH a brief is working, and a working pane is what \
         #2109's refusal is for"
    );

    // The lane answers `fail` at HEAD_A. It reports BELOW, after the fail has
    // routed — see the note there: the report is what stamps the idle signal the
    // reuse arm reads, and a lane already carrying it when a current `fail`
    // routes is one #2501 releases.
    let reviewer = Caller {
        agent_id: lane.clone(),
        group: group.clone(),
        role: Role::Reviewer,
        role_hint: None,
    };
    dispatch(
        &reg,
        &reviewer,
        "tools/call",
        &json!({ "name": "review_verdict", "arguments": {
            "pr": "1758", "verdict": "fail", "summary": "fail - one stale byte count" } }),
    )
    .expect("the lane records its verdict");

    // Arc 5: the fail routes, spending a round, and the worker is handed back.
    //
    // **The lane's turn ends AFTER this tick, and since #2501 it has to.** A
    // lane that is already idle when its current `fail` routes is one the driver
    // RELEASES (§3.1 item 5) — it has answered about the revision on the PR and
    // the drive wants nothing more from that pane this round — so a fixture that
    // reported first would have no lane pane left to reuse, and this test would
    // be about a pane that no longer exists rather than about #2162's deadlock.
    // The reordering keeps the specimen in the class this test witnesses (an
    // idle, delivery-unconfirmed lane pane at a re-brief) and out of the one
    // #2501 releases; both orderings are real, because a reviewer's report and
    // the driver's poll are asynchronous.
    let handed = reg.rd_drive_group_with(&group, &gh, 30_000);
    let (_pr, worker) = handed
        .handbacks
        .first()
        .cloned()
        .expect("a current fail hands the PR back to its worker");
    assert_eq!(status_state(&reg, &group), "fix-wait");
    assert!(
        reg.agent(&lane).map(|a| a.status != AgentStatus::Dead).unwrap_or(false),
        "the fixture's other premise: the lane pane is still there, because it had not \
         finished its turn when the fail routed"
    );

    dispatch(
        &reg,
        &reviewer,
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": "request_changes", "note": "one stale byte count", "ref": "#1758" } }),
    )
    .expect("the lane reports, which ends its turn");
    assert!(
        reg.agent(&lane).expect("the lane is on the roster").idle_since_ms.is_some(),
        "the fixture's premise: that pane has finished its turn"
    );
    // …and its last delivery is on record as not having landed, so the reuse
    // arm will decline it. This is the pane's own pty, which a reuse candidate
    // must have.
    with_pane(&reg, &lane, 7002);
    make_pane_ready(&reg, 7002, false);

    // The fix is BODY-ONLY: the body moves, the head does not.
    gh.set_body("b2");
    reg.set_pr_body_override(Some("b2".to_string()));
    dispatch(
        &reg,
        &Caller { agent_id: worker, group: group.clone(), role: Role::Worker, role_hint: None },
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": "done", "note": "body fixed", "ref": "#1758" } }),
    )
    .expect("a driven worker reports");

    // Arc 8 back to `review-wait` at the same head…
    reg.rd_drive_group_with(&group, &gh, 40_000);
    assert_eq!(
        status_state(&reg, &group),
        "review-wait",
        "the fixture's premise: arc 8 returns at an UNCHANGED head, which is the only \
         shape this defect has"
    );

    // …and the very next tick must open the lane again rather than refuse it.
    let reopened = reg.rd_drive_group_with(&group, &gh, 50_000);
    let (_pr, _b, lane2) = reopened.lanes_opened.first().cloned().unwrap_or_else(|| {
        panic!(
            "the pane the reuse arm just declined was then refused as a duplicate of itself \
             — the #2162 deadlock. duplicate refusals: {:?}",
            rows_for(&reg, &group, "rd-lane-duplicate-refused")
        )
    });
    let declined = rows_for(&reg, &group, "rd-reuse-declined");
    assert_eq!(
        declined.len(),
        1,
        "the deadlock's other half must really have happened, or this test is about a pane \
         nobody tried to reuse: {declined:?}"
    );
    assert_eq!(declined[0]["pane"], json!(lane), "…and it is THIS lane's pane: {declined:?}");
    assert_eq!(declined[0]["reason"], json!("unconfirmed"), "…for the incident's own reason");
    assert!(
        rows_for(&reg, &group, "rd-lane-duplicate-refused").is_empty(),
        "one pane cannot be both too unsettled to type into and too busy to replace: {:?}",
        rows_for(&reg, &group, "rd-lane-duplicate-refused")
    );
    assert_ne!(lane2, lane, "the delta goes to a pane that will read it");

    // The conversation is kept — what makes this a resume rather than a fresh
    // reviewer paying for a cold PR read.
    let rows = lane_spawn_rows(&reg, &group);
    assert_eq!(rows.len(), 2, "one spawn per round, and exactly two rounds: {rows:?}");
    assert_eq!(rows[1]["resumed"], json!(true), "the re-brief is a resume: {rows:?}");
    assert_eq!(
        rows[1]["session"], rows[0]["session"],
        "…of the SAME conversation, not a new one that happens to be filed as a resume: \
         {rows:?}"
    );

    // §7 still owns the pane the re-brief superseded: a late report from it is
    // consumed rather than reaching the orchestrator as if undriven. That is
    // `prior_agents` doing its job across a supersede the refusal used to make
    // impossible.
    dispatch(
        &reg,
        &reviewer,
        "tools/call",
        &json!({ "name": "report", "arguments": {
            "outcome": "done", "note": "a late word from the superseded pane", "ref": "#1758" } }),
    )
    .expect("a superseded lane may still report");
    assert!(
        !delivered_texts(&reg, &group).iter().any(|t| t.contains("a late word")),
        "the superseded pane's late report reached the orchestrator's pane"
    );
}

// ── #2153: a re-drive keeps the lanes' conversations ────────────────────────

/// **#2153.** A drive started on a PR whose previous drive is over resumes each
/// lane's conversation instead of spawning it cold — and a lane whose session
/// can no longer be reopened still falls through to a fresh pane, on the record.
///
/// The gap is the CROSS-DRIVE boundary, and it is the ordinary path rather than
/// an edge: satisfied → the orchestrator dispositions the findings → re-drive is
/// the sequence a satisfied gate is designed to produce. Lane memory lives only
/// on the entry, and `drive_review` replaces a terminal entry with a
/// `DriveEntry::new`, so it went with it. Measured on PR #2141: both lanes
/// re-opened `resumed=false` although each had a live, resolvable session that
/// had already read the PR once.
///
/// **The gone-session arm is the control**, and it is not decoration: without it
/// this test passes under an implementation that seeded a session it never
/// checked and then reported every fresh spawn as a resume. It also pins the
/// fail direction — toward a COLD lane, never toward a brief on a conversation
/// that is not there.
#[test]
fn a_re_drive_resumes_each_lanes_conversation_and_a_lost_one_still_opens_fresh() {
    // (arm, resumed on the re-drive's first lane open, `rd-lane-resume-failed` rows)
    type Row = (&'static str, bool, usize);
    let mut observed: Vec<Row> = Vec::new();

    for arm in ["warm", "session-gone"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, session) = driven(&reg, &repo, &gh);
        let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
        with_pane(&reg, &orch.id, 7001);
        reg.set_pr_head_override(Some(HEAD_A.to_string()));
        reg.set_pr_body_override(Some("b".to_string()));

        reg.rd_drive_group_with(&group, &gh, 10_000);
        let first = reg.rd_drive_group_with(&group, &gh, 20_000);
        let (_pr, _b, lane) =
            first.lanes_opened.first().cloned().unwrap_or_else(|| panic!("{arm}: lane 0 opens"));
        assert_eq!(
            lane_spawn_rows(&reg, &group)[0]["resumed"],
            json!(false),
            "{arm}: the control — a first round is never a resume"
        );

        // The lane passes, the gate is satisfied, and the drive ends.
        dispatch(
            &reg,
            &Caller {
                agent_id: lane.clone(),
                group: group.clone(),
                role: Role::Reviewer,
                role_hint: None,
            },
            "tools/call",
            &json!({ "name": "review_verdict", "arguments": {
                "pr": "1758", "verdict": "pass", "summary": "pass - nothing blocking" } }),
        )
        .unwrap_or_else(|e| panic!("{arm}: the lane records: {e:?}"));
        reg.rd_drive_group_with(&group, &gh, 30_000); // review-wait -> gate-check
        reg.rd_drive_group_with(&group, &gh, 40_000); // gate-check -> satisfied
        assert!(
            reg.review_drive_status(&group)["drives"].as_array().is_some_and(|d| d.is_empty()),
            "{arm}: the fixture's premise — the previous drive really is TERMINAL, which is \
             the only state that reaches the replace branch: {}",
            reg.review_drive_status(&group)
        );

        if arm == "session-gone" {
            // The same refusal a reaped worktree produces: the recorded
            // workspace is gone, so the resume cannot be performed (#338/#359).
            let cwd = reg.agent(&lane).expect("the lane is on the roster").cwd;
            std::fs::remove_dir_all(&cwd).expect("take the lane's workspace away");
        }

        // The orchestrator dispositioned the findings, the worker pushed, and
        // the PR is driven again.
        gh.set_facts("OPEN", HEAD_B);
        reg.set_pr_head_override(Some(HEAD_B.to_string()));
        let out = reg.drive_review_with(&group, &gh, 1758, &session, false, 0, "orch-1", 50_000);
        assert_eq!(out["driving"], json!(true), "{arm}: the re-drive was refused: {out}");

        reg.rd_drive_group_with(&group, &gh, 60_000);
        let again = reg.rd_drive_group_with(&group, &gh, 70_000);
        assert!(
            !again.lanes_opened.is_empty(),
            "{arm}: the re-drive must brief its lane at the new head: {:?}",
            rows_for(&reg, &group, "rd-refused")
        );
        let rows = lane_spawn_rows(&reg, &group);
        assert_eq!(rows.len(), 2, "{arm}: one spawn per round: {rows:?}");
        observed.push((
            arm,
            rows[1]["resumed"] == json!(true),
            rows_for(&reg, &group, "rd-lane-resume-failed").len(),
        ));

        if arm == "warm" {
            assert_eq!(
                rows[1]["session"], rows[0]["session"],
                "…and on the SAME conversation the first drive opened: {rows:?}"
            );
            // **The RECORD carries no verdict and the BRIEF still does, and
            // both are right.** `reseeded` clears `last_verdict`/`at_head`
            // because the new entry may not assert something this drive has not
            // read; the tick then re-reads the live verdict FILE, which outlives
            // the drive that produced it, and `record_verdict_seen` re-derives
            // the pair from it before the brief is built. So a lane that really
            // did answer gets the delta template naming the head it answered at
            // — which is #2109's whole point, a reviewer asked again in its own
            // conversation rather than replaced by a stranger. Asserting the
            // first-call template here would be asserting that a warm reviewer
            // is told nothing about what it already said.
            let delta = lane_brief(&reg, &again.lanes_opened[0].2);
            assert!(
                delta.starts_with("DELTA on PR #1758"),
                "a lane whose own previous verdict is still on file is asked again in its \
                 own conversation, not briefed as a stranger: {delta}"
            );
            assert!(
                delta.contains(&format!("at head {HEAD_A}")),
                "…naming the revision it answered about: {delta}"
            );
            assert!(delta.contains(HEAD_B), "…and the one it is asked about now: {delta}");
        }
    }

    let expected: Vec<Row> = vec![
        ("warm", true, 0),
        // The fail direction is toward a cold lane, and it is audited rather
        // than silent — a fresh pane is what "there was no session" looks like
        // on this log too.
        ("session-gone", false, 1),
    ];
    assert_eq!(
        observed, expected,
        "each row is (arm, the re-drive's first lane open was a resume, \
         `rd-lane-resume-failed` rows). A `false` on the warm arm is the whole of #2153: a \
         reviewer that has already read this PR paying for a cold read on the round where \
         its warm session is cheapest. A `true` on the gone arm is a row claiming a resume \
         that did not happen."
    );
}

// ── #2163: a dead lane pane is observed in `review-wait`, not waited out ────

/// **#2163.** A reviewer lane whose pane has died is re-opened on the next
/// tick, on its own session, rather than waited out to `lane-stalled`.
///
/// Measured on PR #2140 (v1.3.0-beta6): the rev-final lane's pane was killed at
/// 20:12 by the orchestrator — on the driver's OWN advice, since a
/// `cap-refused` notice says to kill an idle delegate — and the drive then sat
/// in `review-wait` with no rd-* row for the PR for 25+ minutes. A pane exit was
/// observed only for the WORKER and only in `fix-wait`; the code's own comment
/// gave the reason as "`review-wait` has `lane-stalled` for its own panes",
/// which is true and is `lane_timeout_minutes` away, anchored at the brief
/// rather than at the death.
///
/// **The live arm is the control**, and it is the half that makes the dead arm
/// mean anything: an implementation that re-opened every lane on every tick
/// would satisfy the first assertion and destroy the wait `review-wait` is made
/// of. Both arms are collected and compared once, so one run says what each did
/// rather than stopping at the first.
#[test]
fn a_dead_lane_pane_is_re_opened_next_tick_and_a_live_one_is_left_alone() {
    // (arm, panes opened on the tick after, `rd-lane-reopened` rows)
    type Row = (&'static str, usize, usize);
    let mut observed: Vec<Row> = Vec::new();

    for arm in ["live", "dead"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let gh = FakeGh::green(HEAD_A);
        let (group, _orch, lane) = lane_round_one(&reg, &repo, &gh);
        assert_eq!(lane_spawn_rows(&reg, &group).len(), 1, "{arm}: one lane, round one");

        if arm == "dead" {
            // The incident's own cause: an orchestrator's `kill_agent`. The
            // initiator is stamped through the real recorder rather than being
            // faked onto the record, so the `killed_by` asserted below is the
            // field `kill_agent` writes and not a literal this test invented.
            reg.record_exit_initiator(&lane, ExitInitiator::Orchestrator);
            assert!(reg.mark_agent_dead_for_test(&lane), "{arm}: the fixture's own premise");
        }
        // Nothing else moves: same head, same body, no verdict. The ONE axis is
        // whether that pane is still alive.
        let next = reg.rd_drive_group_with(&group, &gh, 30_000);
        observed.push((
            arm,
            next.lanes_opened.len(),
            rows_for(&reg, &group, "rd-lane-reopened").len(),
        ));

        if arm == "dead" {
            let rows = lane_spawn_rows(&reg, &group);
            assert_eq!(rows.len(), 2, "the replacement is a spawn row of its own: {rows:?}");
            assert_eq!(
                rows[1]["resumed"],
                json!(true),
                "…and it RESUMES the lane's conversation — a reviewer that has read this PR \
                 once must not pay for a cold read because its pane was killed: {rows:?}"
            );
            let re = rows_for(&reg, &group, "rd-lane-reopened");
            assert_eq!(re[0]["pane"], json!(lane), "the row names the pane that went");
            assert_eq!(
                re[0]["killed_by"],
                json!("orchestrator"),
                "…and who ended it, which is what an orchestrator that has just killed an \
                 idle delegate needs to read: {re:?}"
            );
            assert_eq!(re[0]["head"], json!(HEAD_A));
            assert_eq!(re[0]["block"], json!("rev-std"));
        }
    }

    let expected: Vec<Row> = vec![
        // A live pane mid-review is waited for. Without this row a driver that
        // re-opened unconditionally passes the one below.
        ("live", 0, 0),
        ("dead", 1, 1),
    ];
    assert_eq!(
        observed, expected,
        "each row is (arm, panes opened on the tick after, `rd-lane-reopened` rows). A `0` on \
         the dead arm is the drive waiting on a verdict nothing can produce until \
         `lane-stalled` an hour later; a `1` on the live arm is a second reviewer per tick."
    );
}
