//! Board WIP limits and needs-you item resolution.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// Board WIP limits (#1175 / #1170 A2)
//
// Appended as one block rather than folded in beside the other board tests:
// #1156 is in flight on this same file, and a contiguous tail is the shape that
// rebases cleanly. The fixtures below deliberately do NOT reuse `board_with`,
// which creates its group on a fake repo with plain `rails()` — no
// `advanced_orchestrator`, no workflow file — so every cap would read as "the
// repo declares nothing" and every assertion here would pass against a feature
// that had never run. That is the trap `repo_with_resources` documents for the
// lock suite, and it is the same one here.
// ---------------------------------------------------------------------------

/// A repo whose `.loomux/workflow.yml` declares `body` (a `board:` block).
fn repo_with_board(tag: &str, body: &str) -> std::path::PathBuf {
    let repo = scratch_dir(tag);
    fs::create_dir_all(repo.join(".loomux")).unwrap();
    fs::write(
        repo.join(".loomux").join("workflow.yml"),
        format!(
            "version: {}\n\
             blocks:\n  - id: w\n    name: Worker\n    kind: worker\n    cli: claude\n    model: sonnet\n\
             {body}",
            workflow::SCHEMA_VERSION
        ),
    )
    .unwrap();
    // The fixture asserts its own validity: a file that never parsed would
    // otherwise fail the caller's assertion as "the limits did nothing".
    let loaded = workflow::load_workflow(repo.to_str().unwrap());
    assert!(
        matches!(&loaded, Ok(Some(wf)) if !wf.board.wip.is_empty()),
        "fixture must parse with a non-empty board.wip block, got {loaded:?}"
    );
    repo
}

/// A group on a repo declaring `body`, with `titles` already on its board.
/// Every row is created by an AGENT write, which is what a real board is.
fn wip_board(tag: &str, body: &str, titles: &[&str]) -> (OrchRegistry, tempfile::TempDir, GroupId) {
    let (reg, dir) = test_registry();
    let repo = repo_with_board(tag, body);
    let g = reg.create_group(repo.to_str().unwrap(), advanced_rails()).unwrap();
    for title in titles {
        reg.upsert_task(&g.id, "orch", None, patch(Some(title), None, None)).unwrap();
    }
    (reg, dir, g.id)
}

/// `review: 2`, warn posture (no `enforce:` key at all — the default).
const REVIEW_TWO_WARN: &str = "board:\n  wip:\n    review: 2\n";
/// The same cap, refusing agent writes.
const REVIEW_TWO_ENFORCED: &str = "board:\n  wip:\n    review: 2\n  enforce: true\n";

fn status_patch(status: &str) -> TaskPatch {
    TaskPatch { status: Some(status.into()), ..Default::default() }
}

fn wip_audit_actions(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    reg.audit_log(group).into_iter().map(|e| e.action).collect()
}

fn wip_statuses(reg: &OrchRegistry, group: &GroupId) -> Vec<(String, String)> {
    reg.tasks(group).into_iter().map(|t| (t.id, t.status)).collect()
}

/// The absent-block guarantee (acceptance criterion 1): a repo that declares no
/// `board:` block behaves exactly as it did before this feature existed.
///
/// Asserted on the two things that could betray it — a refusal, and an audit
/// row — rather than on "it did not crash": the failure mode of an opt-in
/// feature is that it is quietly always-on, and both of those are how that
/// would show.
#[test]
fn a_board_with_no_wip_block_never_refuses_and_never_audits_a_crossing() {
    let (reg, _d) = test_registry();
    let g = board_with(&reg, &["a", "b", "c", "d", "e"]);
    for t in ["t-1", "t-2", "t-3", "t-4", "t-5"] {
        reg.upsert_task(&g, "orch", Some(t), status_patch("review"))
            .unwrap_or_else(|e| panic!("{t} into review must land with no caps declared: {e}"));
    }
    assert_eq!(
        reg.tasks(&g).iter().filter(|t| t.status == "review").count(),
        5,
        "five rows in review, because nothing capped it"
    );
    assert!(
        !wip_audit_actions(&reg, &g).contains(&"task-wip-crossed".to_string()),
        "a group with no caps must not audit a crossing — there is no cap to cross"
    );
    assert!(
        reg.wip_status_for_agents(&g).is_empty(),
        "…and list_tasks reports no caps, rather than inventing defaults"
    );
}

/// Warn is the DEFAULT posture (acceptance criterion 2): the write lands, and
/// the crossing is both audited and delivered to the orchestrator's pane.
///
/// The notice matters more than it looks: under `enforce: false` it is the
/// ONLY effect the feature has, so a warn mode that audits but never tells
/// anyone is a feature that does nothing at all.
#[test]
fn a_warn_mode_crossing_lands_and_is_both_audited_and_announced() {
    let (reg, _d, g) = wip_board("wip-warn", REVIEW_TWO_WARN, &["a", "b", "c"]);
    let orch = reg.spawn_agent(&g, Role::Orchestrator, "orch", "run", false, None).unwrap();
    pause_with_pane(&reg, &g, &orch.id, 71);

    for t in ["t-1", "t-2"] {
        reg.upsert_task(&g, "orch", Some(t), status_patch("review")).unwrap();
    }
    assert!(
        !wip_audit_actions(&reg, &g).contains(&"task-wip-crossed".to_string()),
        "filling a cap to its limit is not crossing it — 2 of 2 is what the repo asked for"
    );

    let third = reg.upsert_task(&g, "orch", Some("t-3"), status_patch("review"));
    assert!(third.is_ok(), "warn mode LANDS the write: {third:?}");
    assert_eq!(
        reg.get_task(&g, "t-3").unwrap().status,
        "review",
        "…and the row really is in review, not silently left behind"
    );
    let crossing = reg
        .audit_log(&g)
        .into_iter()
        .find(|e| e.action == "task-wip-crossed")
        .expect("a landed crossing is audited, or the durable record cannot show the board went over");
    assert_eq!(crossing.detail["status"], "review");
    assert_eq!(crossing.detail["limit"], 2);
    assert_eq!(crossing.detail["count"], 3);
    assert_eq!(crossing.detail["enforce"], false);
    assert_eq!(crossing.detail["origin"], "agent");

    let notices = delivered_texts(&reg, &g);
    assert!(
        notices.iter().any(|t| t.contains("WIP limit crossed") && t.contains("review")),
        "the orchestrator must be TOLD — under enforce:false the notice is the whole feature: {notices:?}"
    );
}

/// `enforce: true` (acceptance criterion 3): an agent entry past the cap is
/// refused, the refusal names what a reader needs to act on, and — the part
/// that is easy to get wrong — the board is left exactly as it was.
#[test]
fn an_enforced_cap_refuses_an_agent_entry_and_writes_nothing() {
    let (reg, _d, g) = wip_board("wip-enforce", REVIEW_TWO_ENFORCED, &["a", "b", "c"]);
    for t in ["t-1", "t-2"] {
        reg.upsert_task(&g, "orch", Some(t), status_patch("review")).unwrap();
    }
    let before = wip_statuses(&reg, &g);

    let err = reg
        .upsert_task(&g, "orch", Some("t-3"), status_patch("review"))
        .expect_err("the third entry into a cap of 2 must be refused");
    // Actionable, not merely correct: the cap, the count, and WHICH rows are in
    // the way, so "finish one of these" needs no second call to act on.
    assert!(err.contains("review"), "the refusal names the status: {err}");
    assert!(err.contains("capped at 2"), "…the cap: {err}");
    assert!(err.contains("t-1") && err.contains("t-2"), "…and the rows already there: {err}");
    assert!(err.contains("t-3"), "…and the row it refused: {err}");
    assert!(
        err.contains("board.wip.review"),
        "…and where the cap is declared, so it can be changed rather than fought: {err}"
    );

    assert_eq!(wip_statuses(&reg, &g), before, "a refused write leaves the board byte-for-byte");
    assert!(
        !wip_audit_actions(&reg, &g).contains(&"task-wip-crossed".to_string()),
        "a refusal is not a crossing: auditing one would be the log claiming a write that never happened"
    );
}

/// The design call the issue asked to be argued, pinned as behaviour: under the
/// SAME `enforce: true` config that refuses the agent above, the human's own
/// board edit lands.
///
/// Pinned as a PAIR against the agent refusal, in one test, on one config: the
/// property is a difference between two origins, and two tests on two fixtures
/// could both pass while the difference itself was gone.
#[test]
fn an_enforced_cap_never_refuses_the_human_even_where_it_refuses_the_agent() {
    let (reg, _d, g) = wip_board("wip-human", REVIEW_TWO_ENFORCED, &["a", "b", "c", "d"]);
    let orch = reg.spawn_agent(&g, Role::Orchestrator, "orch", "run", false, None).unwrap();
    pause_with_pane(&reg, &g, &orch.id, 72);
    for t in ["t-1", "t-2"] {
        reg.upsert_task(&g, "orch", Some(t), status_patch("review")).unwrap();
    }
    assert!(
        reg.upsert_task(&g, "orch", Some("t-3"), status_patch("review")).is_err(),
        "control: the agent IS refused on this config, or the human case below proves nothing"
    );

    let human = reg.upsert_task_by_human(&g, "human", Some("t-3"), status_patch("review"));
    assert!(human.is_ok(), "the human's own board edit is never bounced by a cap: {human:?}");
    assert_eq!(reg.get_task(&g, "t-3").unwrap().status, "review");

    // Warned, though — never silently exempt. The orchestrator has to learn the
    // board moved past a cap whoever moved it.
    let crossing = reg
        .audit_log(&g)
        .into_iter()
        .find(|e| e.action == "task-wip-crossed")
        .expect("the human's crossing is audited like anyone's");
    assert_eq!(crossing.detail["origin"], "human");
    assert_eq!(crossing.detail["enforce"], true, "…on a config that DOES enforce, for agents");

    // …and the NOTICE says whose edit it was, which is the half that changes
    // what the orchestrator does about it: its own crossing is its to unwind,
    // the human's is a board to re-read rather than argue with (rev-1 N5).
    let notices = delivered_texts(&reg, &g);
    assert!(
        notices
            .iter()
            .any(|t| t.contains("WIP limit crossed") && t.contains("the human's board edit to t-3")),
        "the crossing notice must name the human as its author: {notices:?}"
    );
}

/// **A cap is a door, not a fence** — the half of `wip_breaches`' test that says
/// a write is judged only when it RAISES a status's count. That is what keeps a
/// status already over its limit workable rather than frozen: an edit to a row
/// sitting in one, a re-assertion of the status it already has, and every move
/// OUT all leave the count where it is or lower, so none of them is refused.
///
/// Three cases in one test because they are one property, and each alone is a
/// rule a reader would reasonably doubt. (The docstring here used to state the
/// superseded "only an ENTRY is judged" rule — an entry is still the common
/// case, which is why the name keeps the word, but it is no longer what the
/// guard decides on: rev-1 B1 replaced it with the before/after comparison.)
#[test]
fn an_over_limit_status_still_takes_edits_and_always_lets_work_out() {
    let (reg, _d, g) = wip_board("wip-entry", REVIEW_TWO_ENFORCED, &["a", "b", "c"]);
    for t in ["t-1", "t-2"] {
        reg.upsert_task(&g, "orch", Some(t), status_patch("review")).unwrap();
    }

    // (a) A write to a row ALREADY in the full status. It adds nothing to the
    // count, so refusing it would freeze every note and PR ref on the work the
    // cap is asking to be finished — the exact opposite of the point.
    let note = reg.upsert_task(
        &g,
        "orch",
        Some("t-1"),
        TaskPatch { note: Some("reviewer engaged".into()), ..Default::default() },
    );
    assert!(note.is_ok(), "an edit to a row already in a full status must land: {note:?}");

    // (b) Re-asserting the same status is not an entry either.
    assert!(
        reg.upsert_task(&g, "orch", Some("t-1"), status_patch("review")).is_ok(),
        "re-writing the status a row already has moves nothing"
    );

    // (c) The way OUT is never blocked — including when the board is already
    // over the cap, which is how a lowered cap or a human edit unwinds.
    reg.upsert_task_by_human(&g, "human", Some("t-3"), status_patch("review")).unwrap();
    assert_eq!(reg.tasks(&g).iter().filter(|t| t.status == "review").count(), 3, "over the cap");
    assert!(
        reg.upsert_task(&g, "orch", Some("t-2"), status_patch("pr")).is_ok(),
        "moving work OUT of an over-limit status must always land, or the board deadlocks"
    );
}

/// A `claim` is an entry into `in-progress`, and it is the entry the whole
/// feature is about: an orchestrator that keeps starting new tasks while review
/// debt piles up is exactly a sequence of claims.
///
/// The claim path reaches the status by its own route (it does not set
/// `patch.status`), so a check that only read `patch.status` would pass every
/// other test in this file and leave the motivating case unguarded.
#[test]
fn a_claim_is_an_entry_into_in_progress_and_an_enforced_cap_refuses_it() {
    let (reg, _d, g) =
        wip_board("wip-claim", "board:\n  wip:\n    in-progress: 1\n  enforce: true\n", &["a", "b"]);
    reg.upsert_task(
        &g,
        "orch",
        Some("t-1"),
        TaskPatch { assignee: Some("w-1".into()), claim: true, ..Default::default() },
    )
    .expect("the first claim fills the cap");

    let err = reg
        .upsert_task(
            &g,
            "orch",
            Some("t-2"),
            TaskPatch { assignee: Some("w-2".into()), claim: true, ..Default::default() },
        )
        .expect_err("a second claim past in-progress: 1 must be refused");
    assert!(err.contains("in-progress"), "the refusal names the status a claim enters: {err}");
    let t2 = reg.get_task(&g, "t-2").unwrap();
    assert_eq!(t2.status, "queued", "the refused claim left the row queued");
    assert_eq!(t2.assignee, None, "…and unassigned — a refused claim assigns nobody");
}

/// A cap counts LEAF rows. Two halves, both load-bearing: a container sitting
/// in the capped status does not consume a slot, and moving a container into a
/// full status is not refused.
///
/// Without this, `in-progress: 4` means four items on a flat board and fewer on
/// a nested one — and #1156 is making nesting the normal shape.
#[test]
fn a_wip_cap_counts_leaf_rows_so_a_container_never_consumes_a_slot() {
    let (reg, _d, g) =
        wip_board("wip-leaf", REVIEW_TWO_ENFORCED, &["epic", "slice one", "slice two"]);
    // t-2 and t-3 sit inside t-1, which makes t-1 a container.
    for t in ["t-2", "t-3"] {
        reg.upsert_task(
            &g,
            "orch",
            Some(t),
            TaskPatch { parent: Some("t-1".into()), ..Default::default() },
        )
        .unwrap();
    }
    // The container goes to review first. If containers counted, it would eat
    // one of the two slots and the second slice below would be refused.
    reg.upsert_task(&g, "orch", Some("t-1"), status_patch("review")).unwrap();
    for t in ["t-2", "t-3"] {
        reg.upsert_task(&g, "orch", Some(t), status_patch("review"))
            .unwrap_or_else(|e| panic!("{t} is the 1st/2nd LEAF in review and must land: {e}"));
    }
    assert!(
        !wip_audit_actions(&reg, &g).contains(&"task-wip-crossed".to_string()),
        "two leaves under a cap of 2 is not a crossing, whatever their container is doing"
    );

    // The other half: a container's own move is exempt from the check, so a
    // full status never traps the rollup row above the work in it.
    reg.upsert_task(&g, "orch", Some("t-1"), status_patch("queued")).unwrap();
    assert!(
        reg.upsert_task(&g, "orch", Some("t-1"), status_patch("review")).is_ok(),
        "a container may enter a full status: what it carries is counted where the work is"
    );
}

/// Acceptance criterion 4 — the counts the board renders, and the same rows the
/// orchestrator reads back from `list_tasks`. One source, asserted through both
/// surfaces, because a chip that disagreed with the refusal would be worse than
/// no chip.
#[test]
fn the_caps_and_their_live_counts_reach_both_the_board_and_the_agent() {
    let (reg, _d, g) = wip_board("wip-counts", REVIEW_TWO_WARN, &["a", "b", "c"]);
    reg.upsert_task(&g, "orch", Some("t-1"), status_patch("review")).unwrap();
    // A container in review, to pin that the DISPLAYED count uses the same
    // leaf-only rule the refusal does rather than a second tally.
    reg.upsert_task(
        &g,
        "orch",
        Some("t-3"),
        TaskPatch { parent: Some("t-2".into()), ..Default::default() },
    )
    .unwrap();
    reg.upsert_task(&g, "orch", Some("t-2"), status_patch("review")).unwrap();

    let rows = reg.wip_status_for_agents(&g);
    assert_eq!(rows.len(), 1, "one declared cap, one row: {rows:?}");
    assert_eq!(rows[0]["status"], "review");
    assert_eq!(rows[0]["limit"], 2);
    assert_eq!(
        rows[0]["count"], 1,
        "t-1 is the only LEAF in review — t-2 is a container, and counting it would make the \
         chip say 2/2 on a board the seam would still let another row into"
    );
    assert_eq!(rows[0]["enforce"], false);

    let status = reg.workflow_status(&g);
    assert_eq!(status["wip"], serde_json::Value::Array(rows), "the board reads the same rows");
}

/// A refusal names up to `WIP_NAMED_OCCUPANTS` rows, and **says so when there
/// are more**. A truncated list that did not admit it reads as the whole set —
/// "finish one of these four" on a status holding six — which is a refusal that
/// misdescribes the board it is refusing on.
#[test]
fn a_refusal_that_cannot_name_every_occupant_says_how_many_it_left_out() {
    let (reg, _d, g) = wip_board(
        "wip-trunc",
        "board:\n  wip:\n    review: 6\n  enforce: true\n",
        &["a", "b", "c", "d", "e", "f", "g"],
    );
    for t in ["t-1", "t-2", "t-3", "t-4", "t-5", "t-6"] {
        reg.upsert_task(&g, "orch", Some(t), status_patch("review")).unwrap();
    }
    let err = reg
        .upsert_task(&g, "orch", Some("t-7"), status_patch("review"))
        .expect_err("the seventh entry into a cap of 6 must be refused");
    assert!(
        err.contains("holding 7"),
        "the count is the REAL post-write one, not the named subset: {err}"
    );
    assert!(err.contains("t-1") && err.contains("t-4"), "the first four are named: {err}");
    assert!(
        err.contains("and 2 more"),
        "…and the two it could not name are admitted rather than dropped: {err}"
    );
}

/// **rev-1 B1: the guard reads the whole post-write board, by one rule.**
///
/// The first cut derived the target status from the patch and the container
/// topology from the un-mutated board, which is CLAUDE.md's "a guard reads every
/// one of its inputs by one rule" violated exactly — and it failed in BOTH
/// directions.
///
/// The four crossings of {this row reparented in / out} × {the target status
/// full / not} are **four separate tests**, deliberately, and not four cases in
/// one: a red evidences only the assertion it reached and moved, and the first
/// cut of this WAS one test — whose panic on case (a) meant the round evidenced
/// nothing at all about (b) or (c). Split, each face reddens on its own
/// mutation and the negative control is visibly untouched.
///
/// (a) The false REFUSAL. `review: 2` full with two leaves; the write nests the
/// new row under one of them, which turns that row into a container and frees
/// the slot in the same write. The old guard counted the pre-write leaves and
/// refused, naming as an occupant the very row it was about to stop counting —
/// and `upsert_task`'s own tool description recommends exactly this call shape.
#[test]
fn nesting_a_row_as_you_move_it_frees_the_slot_its_new_container_held() {
    let (reg, _d, g) = wip_board("wip-b1-in", REVIEW_TWO_ENFORCED, &["a", "b", "c"]);
    for t in ["t-1", "t-2"] {
        reg.upsert_task(&g, "orch", Some(t), status_patch("review")).unwrap();
    }
    let landed = reg.upsert_task(
        &g,
        "orch",
        Some("t-3"),
        TaskPatch {
            parent: Some("t-2".into()),
            status: Some("review".into()),
            ..Default::default()
        },
    );
    assert!(
        landed.is_ok(),
        "t-2 becomes a container in this very write, so review holds two LEAVES afterwards \
         (t-1, t-3) and the write is within the cap: {landed:?}"
    );
    assert!(
        !wip_audit_actions(&reg, &g).contains(&"task-wip-crossed".to_string()),
        "…and it is not a crossing either — warn mode must not announce one"
    );
}

/// (b) The false PERMIT, and the worst of the four: clearing a `parent` while
/// entering a status adds TWO leaves in one write — the row arriving, and the
/// ex-container it leaves behind becoming countable. The old guard counted one
/// and let it through with no refusal, no audit and **no notice**, which under
/// the default warn posture is the feature failing at the only thing it does.
#[test]
fn clearing_a_parent_while_entering_a_status_counts_the_container_left_behind() {
    let (reg, _d, g) = wip_board("wip-b1-out", REVIEW_TWO_ENFORCED, &["a", "b", "c", "d"]);
    reg.upsert_task(&g, "orch", Some("t-4"), parent_patch("t-2")).unwrap();
    for t in ["t-1", "t-2"] {
        reg.upsert_task(&g, "orch", Some(t), status_patch("review")).unwrap();
    }
    assert_eq!(
        reg.wip_status_for_agents(&g)[0]["count"],
        1,
        "control: t-2 is a container, so review holds one leaf and there is room"
    );
    let err = reg
        .upsert_task(
            &g,
            "orch",
            Some("t-4"),
            TaskPatch {
                parent: Some(String::new()), // the clear
                status: Some("review".into()),
                ..Default::default()
            },
        )
        .expect_err("clearing the parent AND entering review adds two leaves to a cap of 2");
    assert!(err.contains("holding 3"), "the refusal counts the post-write board: {err}");
    let t4 = reg.get_task(&g, "t-4").unwrap();
    assert_eq!(t4.status, "queued", "and the refused write left the row alone");
    assert_eq!(t4.parent.as_deref(), Some("t-2"), "…parent included");
}

/// (c) A parent-ONLY write can push a status over with no status in the patch at
/// all: promoting a container's last child to TOP LEVEL turns that container
/// into countable work, and nothing stops being counted in its place. (Moving it
/// under another row in the same status would NOT bite — that is case (a).) The
/// old `may_enter` predicate never even read the policy for this shape.
#[test]
fn a_parent_only_write_that_frees_a_container_into_a_full_status_is_refused() {
    let (reg, _d, g) = wip_board("wip-b1-parent-only", REVIEW_TWO_ENFORCED, &["a", "b", "c", "d"]);
    reg.upsert_task(&g, "orch", Some("t-3"), parent_patch("t-2")).unwrap();
    for t in ["t-1", "t-2", "t-4"] {
        reg.upsert_task(&g, "orch", Some(t), status_patch("review")).unwrap();
    }
    assert_eq!(
        reg.wip_status_for_agents(&g)[0]["count"],
        2,
        "control: t-1 and t-4 are the leaves in review; t-2 is a container"
    );
    let err = reg
        .upsert_task(
            &g,
            "orch",
            Some("t-3"),
            TaskPatch { parent: Some(String::new()), ..Default::default() },
        )
        .expect_err("promoting t-2's only child makes t-2 a third leaf in a review capped at 2");
    assert!(err.contains("review"), "the refusal names the status that went over: {err}");
    assert!(
        err.contains("holding 3"),
        "…and counts the board this write produces, not the one it started from: {err}"
    );
}

/// (d) The negative control the three above need: a reparent that moves nothing
/// into a capped status is not refused, so none of this is a guard that simply
/// says no to `parent`.
#[test]
fn a_reparent_that_touches_no_capped_status_is_never_refused() {
    let (reg, _d, g) = wip_board("wip-b1-control", REVIEW_TWO_ENFORCED, &["a", "b", "c"]);
    reg.upsert_task(&g, "orch", Some("t-1"), status_patch("review")).unwrap();
    assert!(
        reg.upsert_task(&g, "orch", Some("t-3"), parent_patch("t-2")).is_ok(),
        "a reparent among queued rows touches no cap and must land"
    );
    assert!(
        !wip_audit_actions(&reg, &g).contains(&"task-wip-crossed".to_string()),
        "…and announces nothing"
    );
}

/// rev-1 N3: the cost claim the PR makes out loud — a write that cannot move a
/// count does not read the workflow file — pinned on the predicate that decides
/// it, since the read itself has no observable effect to assert on.
#[test]
fn the_wip_guard_reads_the_policy_only_for_a_write_that_could_move_a_count() {
    let note = TaskPatch { note: Some("just a note".into()), ..Default::default() };
    assert!(
        !loomux_lib::orchestration::wip_may_change(false, &note),
        "a note on an existing row cannot change any count, so it must not pay a YAML parse"
    );
    let title = TaskPatch { title: Some("renamed".into()), ..Default::default() };
    assert!(!loomux_lib::orchestration::wip_may_change(false, &title));
    let pr = TaskPatch { pr: Some("#9".into()), ..Default::default() };
    assert!(!loomux_lib::orchestration::wip_may_change(false, &pr));

    // …and every shape that CAN move one does read it. `parent` is the one the
    // first cut missed (rev-1 B1): it moves no row's status and changes the
    // counts anyway, by changing which rows are leaves.
    assert!(loomux_lib::orchestration::wip_may_change(true, &note), "a new row enters a status");
    assert!(loomux_lib::orchestration::wip_may_change(false, &status_patch("review")));
    assert!(loomux_lib::orchestration::wip_may_change(false, &parent_patch("t-1")));
    assert!(loomux_lib::orchestration::wip_may_change(
        false,
        &TaskPatch { claim: true, ..Default::default() }
    ));
}


/// **rev-2 B4: a row this write is ADDING is not part of the board it started
/// from.**
///
/// The pre-write tally used to be taken below the `idx` resolution, which is
/// where a new row is pushed — so on a create, `before` and `after` both counted
/// the new row in its born status, agreed, and `wip_breaches` skipped the status
/// entirely. A `queued:` cap could therefore never fire on task creation: no
/// refusal under `enforce`, and — worse, because it is the default — no notice
/// and no audit under warn.
///
/// It is exactly `queued` and exactly on creation, because a row born `queued`
/// and left there is the only shape where the born status and the final status
/// are the same. That is precisely why no existing fixture caught it: every
/// other test here caps `review` or `in-progress`, which a new row only ever
/// reaches by moving off its born status.
#[test]
fn creating_a_row_into_a_full_born_status_is_refused() {
    let (reg, _d, g) =
        wip_board("wip-b4-create", "board:\n  wip:\n    queued: 2\n  enforce: true\n", &["a", "b"]);
    assert_eq!(reg.tasks(&g).len(), 2, "the fixture filled the cap exactly, and was allowed to");

    let err = reg
        .upsert_task(&g, "orch", None, patch(Some("third"), None, None))
        .expect_err("a third row born into a queued capped at 2 must be refused");
    assert!(err.contains("queued"), "the refusal names the born status: {err}");
    assert!(err.contains("holding 3"), "…and counts the board this write would produce: {err}");
    assert_eq!(
        reg.tasks(&g).len(),
        2,
        "and the refused create wrote nothing — no half-added row survives the refusal"
    );
}

/// The negative control B4's pin needs: creating into a capped status with room
/// still lands. Without it the test above passes on a guard that refuses every
/// create, which would be a far worse bug than the one it is pinning.
#[test]
fn creating_a_row_into_a_born_status_with_room_still_lands() {
    let (reg, _d, g) =
        wip_board("wip-b4-room", "board:\n  wip:\n    queued: 3\n  enforce: true\n", &["a", "b"]);
    let third = reg.upsert_task(&g, "orch", None, patch(Some("third"), None, None));
    assert!(third.is_ok(), "the third row fits a queued capped at 3: {third:?}");
    assert_eq!(reg.tasks(&g).len(), 3);
    assert!(
        !wip_audit_actions(&reg, &g).contains(&"task-wip-crossed".to_string()),
        "…and filling a cap to its limit is not a crossing"
    );
}

/// The same off-by-one under the DEFAULT posture, where the loss is quieter and
/// therefore worse: warn mode's whole effect is the notice and the audit row, so
/// a create that skipped the comparison lost the only thing the feature does.
#[test]
fn a_create_that_crosses_a_cap_is_announced_in_warn_mode() {
    let (reg, _d, g) = wip_board("wip-b4-warn", "board:\n  wip:\n    queued: 2\n", &["a", "b"]);
    let orch = reg.spawn_agent(&g, Role::Orchestrator, "orch", "run", false, None).unwrap();
    pause_with_pane(&reg, &g, &orch.id, 73);

    let third = reg.upsert_task(&g, "orch", None, patch(Some("third"), None, None));
    assert!(third.is_ok(), "warn mode lands the write: {third:?}");
    let crossing = reg
        .audit_log(&g)
        .into_iter()
        .find(|e| e.action == "task-wip-crossed")
        .expect("a create that puts queued over its cap is audited like any other crossing");
    assert_eq!(crossing.detail["status"], "queued");
    assert_eq!(crossing.detail["count"], 3);
    let notices = delivered_texts(&reg, &g);
    assert!(
        notices.iter().any(|t| t.contains("WIP limit crossed") && t.contains("queued")),
        "…and announced, which under enforce:false is the entire feature: {notices:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// The needs-you MCP surface (#1151 slice B)
//
// Slice A built the record; these are the three tools an agent reaches it
// through, and the one it does not. The property tested hardest is the same
// one the question registry's sweep pins for answering: **every agent may
// raise, no agent may ever resolve** — because resolving is the human saying
// they have looked, and an agent that could say it for them would be
// certifying a look that never happened.
// ═══════════════════════════════════════════════════════════════════════════

/// A group with an orchestrator and a worker, MCP callers for both, and one
/// board row that is **not** demo-gated.
///
/// The status matters: `in-progress` keeps slice A's lifecycle hook out of
/// these tests entirely, so every item that exists in one is an item a TOOL
/// asked for. A `prototype` row here would auto-raise a demo item on setup and
/// quietly supply half of what several of these tests are asserting.
fn setup_needs_you_mcp() -> (OrchRegistry, tempfile::TempDir, GroupId, Caller, Caller, String, String)
{
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let worker = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();
    let cw = reg.resolve_token(&worker.token).unwrap();
    let t = reg
        .upsert_task(&g.id, &orch.id, None, patch(Some("A live row"), Some("in-progress"), None))
        .unwrap();
    (reg, dir, g.id, co, cw, orch.id, t.id)
}

#[test]
fn request_attention_registers_an_item_and_answers_the_caller_immediately() {
    let (reg, _d, g, co, _cw, orch_id, task) = setup_needs_you_mcp();

    let out = q_call(&reg, &co, "request_attention", json!({
        "kind": "feedback",
        "text": "  Does the empty state read as broken or as calm?  ",
        "task": task.as_str(),
        "urgency": "high",
    }));
    assert_eq!(out["isError"], false, "{}", q_text(&out));

    // The reply leads with the id (it is what a board note cites) and then says
    // the one thing that decides what this agent does next.
    let text = q_text(&out);
    assert!(text.starts_with("n-1 registered"), "reply must lead with the id: {text}");
    assert!(text.contains("DO NOT WAIT"), "the reply must say not to block: {text}");
    assert!(
        text.contains("cannot resolve it"),
        "…and must say the one thing this tool does NOT buy: {text}"
    );

    let items = reg.needs_you(&g).expect("needs-you.json readable");
    assert_eq!(items.len(), 1);
    let i = &items[0];
    assert_eq!(i.id, "n-1");
    assert_eq!(i.kind, needsyou::Kind::Feedback);
    assert_eq!(i.raiser, orch_id, "the raiser is recorded, not inferred");
    assert_eq!(i.text, "Does the empty state read as broken or as calm?", "text is trimmed");
    assert_eq!(i.task.as_deref(), Some(task.as_str()));
    assert_eq!(i.urgency, needsyou::Urgency::High);
    assert_eq!(i.status, needsyou::Status::Open);
    assert!(i.created_ms > 0, "an item is stamped when it is raised");
    assert!(i.resolved_ms.is_none() && i.resolved_by.is_none() && i.resolution.is_none());

    let log = reg.audit_log(&g);
    let opened = audit_of(&log, "needs-you-open");
    assert_eq!(opened.len(), 1, "the raise is audited");
    assert_eq!(opened[0].detail["id"], "n-1");
    assert_eq!(opened[0].actor, orch_id);
}

/// **A deduped raise must say that the caller's words were thrown away.**
///
/// Parking a board row already raises its demo item (slice A's hook), and
/// `needsyou::admit` deliberately returns the EXISTING row for a second demo
/// raise on the same task rather than a duplicate. That is right for the queue
/// and invisible in a bare id: the row that comes back carries the *board's*
/// generic wording, and the ask the orchestrator actually wrote is gone. So the
/// reply branches on `Raised::fresh`, which is the only reason that bit is
/// returned at all.
#[test]
fn raising_a_demo_for_an_already_parked_task_returns_the_existing_item_and_says_so() {
    let (reg, _d, g, co, _cw, orch_id, task) = setup_needs_you_mcp();
    // Park it — the hook raises the demo item, with the board's own wording.
    reg.upsert_task(&g, &orch_id, Some(&task), patch(None, Some("prototype"), None)).unwrap();
    let before = reg.needs_you(&g).unwrap();
    assert_eq!(before.len(), 1, "parking raised exactly one item: {before:?}");
    assert_eq!(before[0].kind, needsyou::Kind::Demo);
    let board_text = before[0].text.clone();

    let out = q_call(&reg, &co, "request_attention", json!({
        "kind": "demo",
        "text": "specifically: does the empty state read as broken?",
        "task": task.as_str(),
    }));
    assert_eq!(out["isError"], false, "a deduped raise is not an error: {}", q_text(&out));
    let text = q_text(&out);
    assert!(text.starts_with("n-1 was ALREADY OPEN"), "the reply names the existing row: {text}");
    assert!(text.contains(&task), "…and the task it is about: {text}");
    assert!(
        text.contains("NOT recorded"),
        "the caller must be TOLD its text was dropped, not left to infer it: {text}"
    );
    assert!(
        text.contains("feedback"),
        "…and pointed at the tool that would have carried the ask: {text}"
    );

    // One row, still the board's wording, and no second audit line: a dedupe is
    // not a second open.
    let after = reg.needs_you(&g).unwrap();
    assert_eq!(after.len(), 1, "no duplicate row: {after:?}");
    assert_eq!(after[0].text, board_text, "the EXISTING text stands");
    assert_eq!(
        audit_of(&reg.audit_log(&g), "needs-you-open").len(),
        1,
        "the dedupe is not audited as a second raise"
    );
}

/// **The write tools refuse a delegate at the DISPATCH gate**, and the read
/// tool is deliberately not.
///
/// The role-filtered listing is cosmetic — a tool omitted from a listing is
/// still callable by name — so what matters is that a worker's *call* is
/// refused. `list_needs_you` is shared on purpose, and for one reason more than
/// `list_questions` has: the human's panel unions the two registries, so a
/// delegate able to read only half of what is waiting on the human would be
/// reasoning about a queue the human does not see.
#[test]
fn the_needs_you_write_tools_refuse_a_delegate_and_the_dispatch_check_is_the_gate() {
    let (reg, _d, g, co, cw, _orch, task) = setup_needs_you_mcp();
    q_call(&reg, &co, "request_attention", json!({ "kind": "feedback", "text": "A or B?" }));

    for (name, args) in [
        ("request_attention", json!({ "kind": "feedback", "text": "a worker's ask", "task": task.as_str() })),
        ("withdraw_attention", json!({ "id": "n-1" })),
    ] {
        let denied = q_call(&reg, &cw, name, args);
        assert_eq!(denied["isError"], true, "{name} must refuse a worker at dispatch");
        assert!(
            q_text(&denied).contains("orchestrator-only"),
            "{name}'s refusal must say why: {}",
            q_text(&denied)
        );
    }
    // The refused calls changed nothing: no second item, and n-1 is intact.
    let items = reg.needs_you(&g).unwrap();
    assert_eq!(items.len(), 1, "a refused raise must not register an item");
    assert_eq!(items[0].status, needsyou::Status::Open, "a refused withdraw must not settle one");

    // The read tool IS shared.
    let listed = q_call(&reg, &cw, "list_needs_you", json!({}));
    assert_eq!(listed["isError"], false, "list_needs_you is the shared read tier");
    let body: Value = serde_json::from_str(&q_text(&listed)).unwrap();
    assert_eq!(body["items"][0]["id"], "n-1");
    assert_eq!(body["omitted_resolved"], 0);

    // …and the cosmetic half: the orchestrator sees all three, a worker sees
    // only the read.
    let orch_names = tool_names(&reg, &co);
    let worker_names = tool_names(&reg, &cw);
    for name in ["request_attention", "withdraw_attention"] {
        assert!(orch_names.contains(&name.to_string()), "orchestrator must SEE {name}");
        assert!(!worker_names.contains(&name.to_string()), "a worker must not be offered {name}");
    }
    for names in [&orch_names, &worker_names] {
        assert!(names.contains(&"list_needs_you".to_string()), "both roles read items");
    }
}

/// **The liaison poses questions and does NOT raise items — and that asymmetry
/// is the decision this slice made, so it is pinned rather than left to prose.**
///
/// `#1151`'s plan specified `require_orchestrator_or_liaison` for
/// `request_attention`, by analogy with `ask_human`. `docs/design/liaison.md`
/// states a trip-wire against exactly that: the liaison's two widenings hang off
/// the root "a liaison faces the human", and *"a THIRD tool on the second root
/// is the trigger, and the next one that is a write is the trigger regardless of
/// count … the answer then is the fifth kind, deliberately, not a longer
/// table."* Raising an item fires both clauses at once, and the fifth kind now
/// exists (`Role::Manager`, #1161), citing this trip-wire as its reason. So the
/// human-facing pane's raise belongs to the manager's own enumerated surface,
/// and this test is what makes widening it later a deliberate act rather than a
/// one-word slip.
///
/// Pinned in three directions, because a refusal pinned on its own is
/// indistinguishable from a tool nobody can call:
///
/// 1. The liaison CAN still `ask_human` — the shipped widening is untouched, so
///    "the liaison was refused" is a fact about THIS tool, not about the block.
/// 2. It cannot `request_attention` or `withdraw_attention`, at the gate and in
///    the listing.
/// 3. The orchestrator of the SAME group can — so the refusal is a gate, not a
///    broken tool.
#[test]
fn a_liaison_may_pose_a_question_but_may_not_raise_a_needs_you_item() {
    let (reg, _d, _repo, gid) = liaison_group();
    let liaison = reviewer_caller(&reg, &gid, "human");
    let orch = reg.spawn_agent(&gid, Role::Orchestrator, "orch", "", false, None).unwrap();
    let co = reg.resolve_token(&orch.token).unwrap();

    // 1 — THE CONTROL FOR THE BLOCK. The question widening still holds.
    let posed = q_call(&reg, &liaison, "ask_human", json!({ "text": "Ship it, or hold?" }));
    assert_eq!(posed["isError"], false, "the liaison's shipped widening is untouched: {posed:?}");
    assert!(q_text(&posed).starts_with("q-1 registered"), "{}", q_text(&posed));

    // 2 — and neither item write is widened alongside it.
    for (name, args) in [
        ("request_attention", json!({ "kind": "feedback", "text": "does this feel right?" })),
        ("withdraw_attention", json!({ "id": "n-1" })),
    ] {
        let denied = q_call(&reg, &liaison, name, args);
        assert_eq!(denied["isError"], true, "{name} must refuse the liaison at the gate");
        assert!(
            q_text(&denied).contains("orchestrator-only"),
            "{name}'s refusal must say why: {}",
            q_text(&denied)
        );
        let names = listed_tools(&reg, &liaison);
        assert!(
            !names.contains(&name.to_string()),
            "…and the listing must agree with the gate on {name}: {names:?}"
        );
    }
    // The read half is NOT withheld — this narrows the write tier only.
    assert!(
        listed_tools(&reg, &liaison).contains(&"list_needs_you".to_string()),
        "the liaison still READS what is waiting on the human"
    );

    // 3 — POSITIVE CONTROL. Same group, same registry: the orchestrator can
    // raise. Without this, every assertion above would also hold in a build
    // where `request_attention` was broken for everybody.
    let raised = q_call(&reg, &co, "request_attention", json!({
        "kind": "feedback", "text": "the orchestrator's own ask",
    }));
    assert_eq!(raised["isError"], false, "the tool must work for its own tier: {raised:?}");
    assert!(q_text(&raised).starts_with("n-1 registered"), "{}", q_text(&raised));
    let items = reg.needs_you(&gid).unwrap();
    assert_eq!(items.len(), 1, "exactly the orchestrator's row exists: {items:?}");
    assert_eq!(items[0].raiser, co.agent_id, "…and it is attributed to it");
}

/// **`request_attention` refuses a `task` that is not on the board, and that
/// check lives here rather than in the registry.**
///
/// `needsyou::validate_raise` is pure and board-blind, and says in its own doc
/// that the existence check belongs at this entry point: for the board hook the
/// check would have nothing to catch, and for an agent it catches an id that
/// leaves the human a card joined to nothing — and, for a `demo`, an item the
/// board can never settle, since the auto-resolve filters `is_open_demo_for` and
/// so fires only for a demo item and only on a real row's transition. A
/// `feedback` item is never auto-resolved whatever its task, which is why the
/// check is justified by the LINK rather than by the settle alone.
#[test]
fn request_attention_refuses_a_phantom_task_a_bad_kind_and_a_demo_with_no_row() {
    let (reg, _d, g, co, _cw, _orch, task) = setup_needs_you_mcp();

    let phantom = q_call(&reg, &co, "request_attention", json!({
        "kind": "demo", "text": "go look", "task": "t-999",
    }));
    assert_eq!(phantom["isError"], true, "a task that is not on the board is refused");
    assert!(
        q_text(&phantom).contains("t-999"),
        "…BY NAME, so a caller that mistyped can see which string was wrong: {}",
        q_text(&phantom)
    );

    // A demo with nothing linked — `validate_raise`'s own refusal, reached
    // through the tool so the arm is shown to pass it through rather than
    // silently defaulting a task in.
    let unlinked =
        q_call(&reg, &co, "request_attention", json!({ "kind": "demo", "text": "go look" }));
    assert_eq!(unlinked["isError"], true, "a demo item needs a task");
    assert!(q_text(&unlinked).contains("demo item needs a task"), "{}", q_text(&unlinked));

    // An unrecognized kind is an ERROR, never a defaulted one: a caller that
    // wrote "demos" meant a demo, and filing it as feedback silently changes
    // what the human is being asked to do.
    let bad = q_call(&reg, &co, "request_attention", json!({ "kind": "demos", "text": "x" }));
    assert_eq!(bad["isError"], true, "an unknown kind is refused, not defaulted");
    assert!(q_text(&bad).contains("unknown kind"), "{}", q_text(&bad));
    let bad_urgency = q_call(&reg, &co, "request_attention", json!({
        "kind": "feedback", "text": "x", "urgency": "urgent",
    }));
    assert_eq!(bad_urgency["isError"], true, "…and so is an unknown urgency");

    // NOTHING was registered by any of the four.
    assert!(reg.needs_you(&g).unwrap().is_empty(), "a refused raise writes no row");

    // POSITIVE CONTROL: the same call with the real row succeeds, so the four
    // refusals above are about their inputs and not about the tool being dead.
    let ok = q_call(&reg, &co, "request_attention", json!({
        "kind": "demo", "text": "go look", "task": task.as_str(),
    }));
    assert_eq!(ok["isError"], false, "{}", q_text(&ok));
    assert_eq!(reg.needs_you(&g).unwrap().len(), 1);
}

/// Withdrawal is a settle, never a delete, and it is visibly not the human's
/// acknowledgement — driven through the tool, since slice A pinned the registry
/// method and this is the arm that reaches it.
#[test]
fn withdraw_attention_settles_the_row_and_never_passes_for_a_human_look() {
    let (reg, _d, g, co, _cw, orch_id, _task) = setup_needs_you_mcp();
    q_call(&reg, &co, "request_attention", json!({
        "kind": "feedback", "text": "overtaken by events",
    }));

    let out = q_call(&reg, &co, "withdraw_attention", json!({ "id": "n-1" }));
    assert_eq!(out["isError"], false, "{}", q_text(&out));
    assert!(q_text(&out).starts_with("n-1 withdrawn"), "{}", q_text(&out));
    assert!(
        q_text(&out).contains("never mistaken"),
        "the reply must say what a withdrawal is NOT: {}",
        q_text(&out)
    );

    let items = reg.needs_you(&g).unwrap();
    assert_eq!(items.len(), 1, "a withdrawal keeps the row");
    assert_eq!(items[0].status, needsyou::Status::Resolved);
    let expected_tag = format!("withdrawn:{orch_id}");
    assert_eq!(
        items[0].resolved_by.as_deref(),
        Some(expected_tag.as_str()),
        "a withdrawal names who took it back and is never spelled `webview`"
    );
    assert!(items[0].resolution.is_none(), "and puts no words in the human's mouth");

    // Neither an already-settled row nor an id this group does not have.
    for (id, why) in [("n-1", "already resolved"), ("n-99", "unknown needs-you item")] {
        let denied = q_call(&reg, &co, "withdraw_attention", json!({ "id": id }));
        assert_eq!(denied["isError"], true, "{id} must be refused");
        assert!(q_text(&denied).contains(why), "{id}: {}", q_text(&denied));
    }
}

/// **The agent-facing list is a projection, and the human's close-out note is
/// not in it.**
///
/// `needsyou::AgentItem` withholds `resolution` deliberately: the note is
/// delivered, sanitized, into the ORCHESTRATOR's own pane, which is the surface
/// it was written for — but this read is SHARED with every delegate, and a note
/// the human typed to their orchestrator is not thereby addressed to the whole
/// fleet. `had_resolution` carries the one bit an agent needs, so it can ask
/// rather than invent. This is the assertion that notices the day someone
/// swaps the projection for the stored struct.
#[test]
fn list_needs_you_shows_the_projection_and_never_the_humans_close_out_note() {
    let (reg, _d, g, co, cw, _orch, task) = setup_needs_you_mcp();
    q_call(&reg, &co, "request_attention", json!({
        "kind": "demo", "text": "go run it", "task": task.as_str(),
    }));
    q_call(&reg, &co, "request_attention", json!({ "kind": "feedback", "text": "still open" }));
    const SECRET: &str = "I looked and the spacing is wrong on the third row";
    reg.resolve_needs_you(&g, "n-1", Some(SECRET), needsyou::ResolveSource::Webview).unwrap();

    // Both roles read the same projection.
    for (caller, who) in [(&co, "the orchestrator"), (&cw, "a worker")] {
        let body: Value =
            serde_json::from_str(&q_text(&q_call(&reg, caller, "list_needs_you", json!({}))))
                .unwrap();
        let raw = body.to_string();
        assert!(!raw.contains(SECRET), "{who} was shown the human's note verbatim: {raw}");
        assert!(!raw.contains("\"resolution\""), "{who} was shown the note's key: {raw}");

        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "open rows first, then the resolved tail: {items:?}");
        // Open first, in raise order — the panel does the sorting, not this.
        assert_eq!(items[0]["id"], "n-2", "the open row leads: {items:?}");
        assert_eq!(items[0]["status"], "open");
        assert_eq!(items[0]["had_resolution"], false);
        assert_eq!(items[1]["id"], "n-1");
        assert_eq!(items[1]["status"], "resolved");
        assert_eq!(items[1]["kind"], "demo");
        assert_eq!(items[1]["task"], task);
        // The ONE bit an agent gets about the note: that there is one.
        assert_eq!(items[1]["had_resolution"], true, "{items:?}");
        // …and `resolved_by`, which is the field that distinguishes "the human
        // looked" from "the board moved on" from "I took it back myself".
        assert_eq!(items[1]["resolved_by"], "webview", "{items:?}");
        assert_eq!(body["omitted_resolved"], 0, "nothing was left off, and it says so");
    }

    // POSITIVE CONTROL for the withholding: the note really IS on the stored
    // row. Without this, "the agent could not see it" would also pass in a build
    // where the note was never written at all.
    assert_eq!(
        reg.needs_you(&g).unwrap().iter().find(|i| i.id == "n-1").unwrap().resolution.as_deref(),
        Some(SECRET),
        "the note must exist where the projection is refusing to show it"
    );
}

/// Membership is enforced by WHICH FILE was read, not by comparing a field:
/// each group's items live in its own group dir, so another group's id is
/// simply absent — the same refusal an id that never existed gets, leaking
/// nothing about the other group.
#[test]
fn a_needs_you_item_is_scoped_to_the_group_that_raised_it() {
    let (reg, _d, g_a, co_a, _cw, _orch, _task) = setup_needs_you_mcp();
    let g_b = reg.create_group("C:/tmp/other-repo", rails()).unwrap();
    let orch_b = reg.spawn_agent(&g_b.id, Role::Orchestrator, "orch-b", "", false, None).unwrap();
    let co_b = reg.resolve_token(&orch_b.token).unwrap();

    q_call(&reg, &co_a, "request_attention", json!({ "kind": "feedback", "text": "group A's ask" }));

    // Group B cannot see it…
    let listed: Value =
        serde_json::from_str(&q_text(&q_call(&reg, &co_b, "list_needs_you", json!({})))).unwrap();
    assert_eq!(listed["items"].as_array().unwrap().len(), 0, "{listed}");
    // …cannot withdraw it…
    let denied = q_call(&reg, &co_b, "withdraw_attention", json!({ "id": "n-1" }));
    assert_eq!(denied["isError"], true);
    assert!(
        q_text(&denied).contains("unknown needs-you item"),
        "the refusal must leak nothing about the other group: {}",
        q_text(&denied)
    );
    // …and cannot resolve it through its own group dir either.
    let err = reg
        .resolve_needs_you(&g_b.id, "n-1", None, needsyou::ResolveSource::Webview)
        .expect_err("another group's item is not resolvable here");
    assert!(err.contains("unknown needs-you item"), "{err}");

    // Group A's row is untouched, and B's raise of its OWN item mints `n-1`
    // in B's file — proof the two registries are separate files rather than
    // one keyed store.
    assert_eq!(reg.needs_you(&g_a).unwrap()[0].status, needsyou::Status::Open);
    q_call(&reg, &co_b, "request_attention", json!({ "kind": "feedback", "text": "B's own ask" }));
    let b_items = reg.needs_you(&g_b.id).unwrap();
    assert_eq!(b_items.len(), 1, "{b_items:?}");
    assert_eq!(b_items[0].id, "n-1", "ids are minted per group, not globally");
    assert_eq!(reg.needs_you(&g_a).unwrap().len(), 1, "…and A still has exactly its own");
}

/// **THE structural assertion of this slice: no agent token can RESOLVE.**
///
/// Resolving an item is the human saying they have looked at the thing. An
/// agent able to produce that would be certifying a look that never happened,
/// and the panel would be theatre. So this drives the entire MCP tool surface —
/// every tool BOTH roles are offered, with a resolve-shaped argument bag — plus
/// every name a future slice might plausibly give a resolve tool, and asserts
/// that after every one of them the item still carries no human acknowledgement.
///
/// **The forbidden act is precisely a `webview` settle**, the same way the
/// question sweep's is precisely `Answered`. A tool that settles a row as
/// `withdrawn:<agent>` is fine — an orchestrator taking back its own ask is not
/// the same power as deciding the human has looked — and those two assertions
/// are what pin the difference.
#[test]
fn no_agent_token_can_resolve_a_needs_you_item_through_the_mcp_surface() {
    let (reg, _d, g, co, cw, _orch, _task) = setup_needs_you_mcp();

    // Excluded from the sweep because each shells out to `gh` or waits on a
    // pane bind — driving them here would make this a network test, not because
    // any of them is trusted. They are covered instead by
    // `the_mcp_surface_has_no_path_to_the_item_resolve_entry_point`, which reads
    // the source of every arm including these. Asserted to be a SUBSET of what
    // is actually listed, so a renamed tool cannot silently drop out of the
    // sweep by matching nothing.
    const SHELLS_OUT: [&str; 6] = [
        "spawn_agent",
        "list_verdicts",
        "queue_merge",
        "merge_queue_status",
        "cancel_queued_merge",
        "session_digest",
    ];

    let mut names = tool_names(&reg, &co);
    names.extend(tool_names(&reg, &cw));
    for skipped in SHELLS_OUT {
        if skipped == "session_digest" {
            continue; // process-hinted blocks only; not listed for these two
        }
        assert!(names.contains(&skipped.to_string()), "{skipped} is no longer a listed tool — \
            update SHELLS_OUT so the sweep keeps covering the surface it claims to");
    }
    names.retain(|n| !SHELLS_OUT.contains(&n.as_str()));
    // Names a future slice might plausibly reach for. None of these exists, and
    // this is the assertion that notices the day one does.
    names.extend(
        [
            "resolve_needs_you",
            "resolve_attention",
            "needs_you_resolve",
            "orch_needs_you_resolve",
            "acknowledge_attention",
            "clear_needs_you",
            "mark_seen",
            // #2137: the human's second verb over this registry. Exactly as
            // forbidden to an agent as resolving — an agent whose ask went
            // stale has `withdraw_attention`.
            "dismiss_needs_you",
            "dismiss_attention",
            "orch_needs_you_dismiss",
        ]
        .map(str::to_string),
    );

    // EVERY swept tool gets its own FRESH OPEN item, and this is load-bearing
    // rather than tidiness — the question sweep learned it the hard way.
    // `withdraw_attention` is in the sweep and legitimately settles a row, so a
    // single shared item would be settled early and every later tool would be
    // aimed at a row that `resolve_needs_you` refuses on its own account. The
    // headline assertion could then no longer observe the thing it exists to
    // catch.
    //
    // Raised through the REGISTRY rather than through `request_attention`, and
    // that differs from the question sweep deliberately: `request_attention` is
    // itself one of the swept tools, so raising through it would leave the
    // sweep's own rows lying about, and "the item I am watching" would stop
    // being unambiguous. Housekeeping that cannot be mistaken for a call under
    // test is worth more here than symmetry with the older sweep.
    //
    // `feedback`, with no task: an item linked to a board row could be settled
    // by the LIFECYCLE HOOK if a swept tool moved that row, and `board:<status>`
    // is a legitimate settle. Keeping the watched row unlinked means the only
    // thing that can touch it is the tool under test.
    let raise_fresh = |label: &str| -> String {
        reg.raise_needs_you(&g, "sweep", feedback_req(&format!("open for {label}")))
            .expect("the sweep needs a fresh item")
            .item
            .id
    };

    for (caller, who) in [(&co, "the orchestrator"), (&cw, "a worker")] {
        for name in &names {
            let id = raise_fresh(name);
            // One bag carrying every argument name any of these tools takes, so
            // each call gets as far into its own handler as it possibly can.
            // `kind` is `feedback` rather than a `notify_when` kind on purpose:
            // the tool this sweep is about is the item surface, and a bag that
            // made `request_attention` fail at argument parsing would drive it
            // less deeply than every other tool here.
            let payload = json!({
                "id": id, "item": id, "note": "the human looked", "resolution": "looks good",
                "text": "looks good", "source": "webview", "resolved_by": "webview",
                "status": "resolved", "kind": "feedback", "state": "{}", "title": "t", "name": "n",
            });
            // The call may legitimately succeed, fail, or be unknown — none of
            // that is what is under test. What is under test is the state
            // afterwards.
            let _ = dispatch(
                &reg,
                caller,
                "tools/call",
                &json!({ "name": name, "arguments": payload }),
            );
            let items = reg.needs_you(&g).expect("needs-you.json readable");
            let i = items.iter().find(|i| i.id == id).expect("the item still exists");
            assert_ne!(
                i.resolved_by.as_deref(),
                Some("webview"),
                "{who} settled {id} as the HUMAN by calling {name:?} — no agent may ever resolve"
            );
            assert!(
                i.resolution.is_none(),
                "{who} put a close-out note on {id} by calling {name:?}: {:?}",
                i.resolution
            );
            // …and the one settle an agent legitimately HAS is still visibly a
            // withdrawal, which is what makes the two assertions above a
            // distinction rather than a blanket ban.
            if i.status.is_resolved() {
                assert!(
                    i.resolved_by.as_deref().is_some_and(|b| b.starts_with("withdrawn:")),
                    "{who} settled {id} via {name:?} as {:?} — the only settle an agent may \
                     perform is a withdrawal",
                    i.resolved_by
                );
            }
            // Clear the slot for the next tool. Through the registry, never a
            // tool call, so the sweep's own housekeeping can never be mistaken
            // for one of the calls under test. Already-settled is fine.
            let _ = reg.withdraw_needs_you(&g, "sweep", &id);
        }
    }

    // And the plausible names really are unknown, rather than existing and
    // merely refusing — the difference between a gate and a missing feature.
    // Against an OPEN item, so "unknown tool" is the only reason left for the
    // call to fail: aimed at a settled one, a real resolve tool would refuse for
    // its own reasons and read as absent when it is merely blocked.
    //
    // Both roles, not just the orchestrator: "this name does not exist" is a
    // claim about the whole surface, and a tool listed for nobody is still
    // callable by anybody.
    for name in [
        "resolve_needs_you",
        "orch_needs_you_resolve",
        "acknowledge_attention",
        "dismiss_needs_you",
        "orch_needs_you_dismiss",
    ] {
        for (caller, who) in [(&co, "the orchestrator"), (&cw, "a worker")] {
            let id = raise_fresh(name);
            let out = dispatch(
                &reg,
                caller,
                "tools/call",
                &json!({ "name": name, "arguments": { "id": id, "note": "seen" } }),
            )
            .unwrap();
            assert_eq!(out["isError"], true, "{who} got a non-error from {name}");
            assert!(
                q_text(&out).contains("unknown tool"),
                "{name} must not exist at all on the MCP surface, for {who}: {}",
                q_text(&out)
            );
            let _ = reg.withdraw_needs_you(&g, "sweep", &id);
        }
    }

    // ---- POSITIVE CONTROL: what de-shadows every assertion above ----
    //
    // A sweep that finds no resolve proves the boundary held ONLY IF the check
    // could have seen one. Every assertion above is satisfied trivially when a
    // tool simply does not exist — dispatch returns "unknown tool", nothing
    // happens, the item is untouched — which is indistinguishable from a tool
    // that exists and was properly refused. Left there, this test would keep
    // passing even if `resolved_by` stopped being written at all, or if
    // `needs_you()` started returning stale rows: it would be asserting against
    // a mechanism that can no longer report the thing it is watching for.
    //
    // So drive the ONE path that is allowed to resolve, and confirm the very
    // same observation fires. Green above now means "nothing resolved it", not
    // "nothing could have been observed".
    let id = raise_fresh("positive control");
    reg.resolve_needs_you(&g, &id, Some("the human looked"), needsyou::ResolveSource::Webview)
        .expect("the trusted webview path must be able to resolve");
    let items = reg.needs_you(&g).unwrap();
    let i = items.iter().find(|i| i.id == id).expect("the control item exists");
    assert_eq!(
        i.resolved_by.as_deref(),
        Some("webview"),
        "the trusted path could not resolve {id} — so the sweep above proves nothing: its \
         assertions would pass whether or not an agent had resolved"
    );
    assert_eq!(
        i.resolution.as_deref(),
        Some("the human looked"),
        "the note must land where the sweep above looks for it"
    );

    // #2137: the sweep's `withdrawn:` assertion is what catches an agent-driven
    // DISMISSAL (`dismissed:webview` does not start with `withdrawn:`), so it
    // needs its own control for the same reason — that assertion would be
    // satisfied for ever by a registry that had stopped writing the tag, and
    // the answer-side control above only exercises `webview`.
    let did = raise_fresh("dismiss control");
    reg.dismiss_needs_you(&g, &did, Some("not worth a look"), needsyou::ResolveSource::WebviewDismiss)
        .expect("the trusted webview path must be able to dismiss");
    let items = reg.needs_you(&g).unwrap();
    let d = items.iter().find(|i| i.id == did).expect("the control item exists");
    assert_eq!(
        d.resolved_by.as_deref(),
        Some("dismissed:webview"),
        "the trusted path could not dismiss {did} — so the `withdrawn:` assertion in the sweep \
         above proves nothing: it would pass whether or not an agent had dismissed"
    );
    assert!(
        !d.resolved_by.as_deref().is_some_and(|b| b.starts_with("withdrawn:")),
        "…and the tag a dismissal writes really is one the sweep's check rejects: {:?}",
        d.resolved_by
    );
}

/// **Each settle method refuses the OTHER's source** (#2137, review round 2 B1).
///
/// `is_dismissal`'s doc claims a resolve and a dismissal "cannot disagree about
/// which gesture happened". Nothing made that true until this guard existed:
/// both methods take a `ResolveSource` by value and each hard-codes its own
/// tag, audit action and delivery rule, so the mismatched pairing wrote a row
/// whose `resolved_by` said one thing while its audit line and its notice said
/// the other.
///
/// **This test performs the edit the guard forbids**, in both directions,
/// rather than asserting the guard exists — a counterfactual is only pinned by
/// a test that tries it (CLAUDE.md's escape-hatch rule). Neither pairing is
/// reachable from the two Tauri commands, each of which hard-codes one variant;
/// that is what makes this the kind of invariant a test has to hold, because
/// the compiler will not.
#[test]
fn a_resolve_and_a_dismissal_each_refuse_the_other_s_source() {
    let (reg, _d, g, _orch_id) = setup_needs_you();

    // resolve + a DISMISSAL source. Without the guard this wrote
    // `resolved_by: "dismissed:webview"` — which `list_needs_you`'s tool doc
    // and the panel's `isDismissedRow` both read as "the human did not look" —
    // while auditing `needs-you-resolve` and, with no note, delivering nothing.
    let a = reg.raise_needs_you(&g, "orch", feedback_req("first")).unwrap();
    let before = delivered_texts(&reg, &g).len();
    let e = reg
        .resolve_needs_you(&g, &a.item.id, None, needsyou::ResolveSource::WebviewDismiss)
        .expect_err("resolving with a dismissal's source is refused");
    assert!(e.contains("is a dismissal source"), "{e}");

    // The refusal SETTLES NOTHING — the row is untouched, not half-written.
    let still = reg.needs_you(&g).unwrap();
    let row = still.iter().find(|i| i.id == a.item.id).unwrap();
    assert_eq!(row.status, needsyou::Status::Open, "the refusal left the item open");
    assert!(row.resolved_by.is_none(), "…and wrote no provenance: {:?}", row.resolved_by);
    assert_eq!(delivered_texts(&reg, &g).len(), before, "…and delivered nothing");

    // dismiss + a RESOLVE source, the mirror. Without the guard this wrote
    // `resolved_by: "webview"` — read downstream as "the human looked" — while
    // auditing `needs-you-dismiss` and delivering the dismissal notice.
    let b = reg.raise_needs_you(&g, "orch", feedback_req("second")).unwrap();
    let e = reg
        .dismiss_needs_you(&g, &b.item.id, None, needsyou::ResolveSource::Webview)
        .expect_err("dismissing with a resolve's source is refused");
    assert!(e.contains("is not a dismissal source"), "{e}");
    let row = reg.needs_you(&g).unwrap().into_iter().find(|i| i.id == b.item.id).unwrap();
    assert_eq!(row.status, needsyou::Status::Open, "the mirror refusal also settled nothing");

    // Both refusals are AUDITED, the registry's posture for every turned-away
    // settle — so "who tried to settle this the wrong way" survives.
    let log = reg.audit_log(&g);
    let wrong: Vec<_> = audit_of(&log, "needs-you-reject")
        .into_iter()
        .filter(|r| r.detail["reason"] == json!("wrong-source"))
        .collect();
    assert_eq!(wrong.len(), 2, "each mismatched pairing is on the record: {wrong:?}");

    // POSITIVE CONTROL. Every assertion above is satisfied by a registry that
    // refuses EVERYTHING, so drive both correct pairings and confirm they still
    // settle — otherwise this test would pass against a pair of methods that
    // had stopped working entirely.
    reg.resolve_needs_you(&g, &a.item.id, None, needsyou::ResolveSource::Webview)
        .expect("the matching pairing still resolves");
    reg.dismiss_needs_you(&g, &b.item.id, None, needsyou::ResolveSource::WebviewDismiss)
        .expect("the matching pairing still dismisses");
    let items = reg.needs_you(&g).unwrap();
    assert_eq!(
        items.iter().find(|i| i.id == a.item.id).unwrap().resolved_by.as_deref(),
        Some("webview")
    );
    assert_eq!(
        items.iter().find(|i| i.id == b.item.id).unwrap().resolved_by.as_deref(),
        Some("dismissed:webview")
    );
}

/// The two resolve sources must SPELL THEMSELVES DIFFERENTLY (#2137).
///
/// The closed-set pin above says which variants exist; nothing there says the
/// tags they produce are distinguishable, and two variants writing one string
/// would collapse "the human looked" into "the human declined to look" with
/// every check still green. `resolved_by` is the only place that difference is
/// ever recorded, so it is asserted here as a DIFFERENCE first — which a
/// mechanical rename of both literals cannot satisfy — and only then as a
/// spelling.
#[test]
fn the_resolve_and_dismiss_tags_are_not_the_same_string() {
    let looked = needsyou::ResolveSource::Webview;
    let declined = needsyou::ResolveSource::WebviewDismiss;
    assert_ne!(
        looked.tag(),
        declined.tag(),
        "a resolve and a dismissal must be tellable apart in `resolved_by` for ever"
    );
    assert!(!looked.is_dismissal(), "a resolve is not a dismissal");
    assert!(declined.is_dismissal(), "…and a dismissal is");
    // The spelling itself, once, because the frontend mirrors this literal
    // (`DISMISSED_ITEM_TAG` in `src/decisions.ts`) and the panel's settled tail
    // reads it to label the row.
    assert_eq!(declined.tag(), "dismissed:webview");
}

/// The `gates.merge` body [`routed_repo`] writes unless a test wants another:
/// one static reviewer, and two more routed by path.
const ROUTED_GATE: &str = "    reviewers: [rev-lead]\n\
     \x20   routing:\n\
     \x20     - paths: [src/**]\n\
     \x20       reviewers: [rev-ui]\n\
     \x20     - paths: [\"**/Cargo.toml\", package-lock.json]\n\
     \x20       reviewers: [rev-deps]\n";

/// A repo with three reviewer lanes and `gate_body` as its `gates.merge` (#1176).
/// Separate from [`gated_repo`] rather than parameterized onto it: the roster
/// differs, and the capacity assertions elsewhere in this file are pinned to
/// that one's.
fn routed_repo_with(gate_body: &str) -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    let dir = td.path().join(".loomux");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("workflow.yml"),
        format!(
            "version: 1\nname: path-routed\n\
             blocks:\n\
             \x20 - id: worker\n    kind: worker\n\
             \x20 - id: rev-lead\n    kind: reviewer\n    prompt: Everything.\n\
             \x20 - id: rev-ui\n    kind: reviewer\n    prompt: Frontend only.\n\
             \x20 - id: rev-deps\n    kind: reviewer\n    prompt: Dependency manifests only.\n\
             gates:\n  merge:\n{gate_body}"
        ),
    )
    .unwrap();
    td
}

/// [`routed_repo_with`] carrying the ordinary two-rule gate.
fn routed_repo() -> tempfile::TempDir {
    routed_repo_with(ROUTED_GATE)
}

/// A group on `repo`, with the advanced orchestrator on.
///
/// `max_agents` is raised above [`rails`]'s 2: this roster has three reviewer
/// lanes and the point of routing is that a PR can require more than one of
/// them, so a cap that stopped the third from spawning would make a test unable
/// to reach the state it exists to check.
fn group_for(repo: &tempfile::TempDir) -> (OrchRegistry, tempfile::TempDir, GroupId) {
    let (reg, d) = test_registry();
    reg.set_pr_head_override(Some(HEAD.into()));
    let g = reg
        .create_group(
            &repo.path().to_string_lossy(),
            Guardrails { advanced_orchestrator: true, max_agents: 4, ..rails() },
        )
        .unwrap();
    let id = g.id.clone();
    (reg, d, id)
}

/// [`gated_group`] for [`routed_repo`].
fn routed_group() -> (OrchRegistry, tempfile::TempDir, tempfile::TempDir, GroupId) {
    let repo = routed_repo();
    let (reg, d, id) = group_for(&repo);
    (reg, d, repo, id)
}

/// The `ok`-protocol answer the fake `gh` gives for a PR touching `files`.
fn fake_files(files: &[&str]) -> String {
    let mut out = String::from("ok\n");
    for f in files {
        out.push_str(&format!("p {f}\n"));
    }
    out
}

#[test]
fn gh_shim_harness_executes_path_routing_and_refuses_a_diff_it_cannot_account_for() {
    if !have_sh() {
        eprintln!("SKIP gh_shim_harness_executes_path_routing_and_refuses_a_diff_it_cannot_account_for: no POSIX sh");
        return;
    }
    // #1176. The pure decision is pinned in tests/workflow.rs; this runs the
    // SHELL, because a shim/mirror agreement asserted against source text is not
    // an agreement. Every claim below is the real generated shim, executed.
    let (reg, d, _repo, gid) = routed_group();
    let group_dir = d.path().join(gid.as_str());
    let bin = tempfile::tempdir().unwrap();
    let shim = shim_with_fake_gh(bin.path());

    // The declared rules reach the spec file the shim reads, keyed by rule.
    let spec = fs::read_to_string(group_dir.join("merge_gate")).unwrap();
    assert!(spec.contains("route-path 1 src/**"), "{spec}");
    assert!(spec.contains("route-reviewer 1 rev-ui"), "{spec}");
    assert!(spec.contains("route-path 2 **/Cargo.toml"), "{spec}");
    assert!(spec.contains("route-path 2 package-lock.json"), "{spec}");
    assert!(spec.contains("route-reviewer 2 rev-deps"), "{spec}");

    let lead = reviewer_caller(&reg, &gid, "rev-lead");
    recorded(&reg, &lead, "7", "pass", "reviewed");
    let merge_files = |files: &str| {
        reg.grant_merge(&gid, "7", None, "human").unwrap();
        merge_env(&shim, &group_dir, "main", HEAD, "0", &[("FAKE_FILES", files)])
    };

    // A diff no rule matches: the static list is the whole gate, and one pass
    // opens it. Routing is ADDITIVE — it never subtracts, and it never fires on
    // a PR that did not touch its paths.
    assert!(
        merge_files(&fake_files(&["docs/orchestration.md", "README.md"])).0,
        "no rule matched, so the gate is exactly the one the repo declared"
    );

    // The same PR, one file different: rule 1 fires and rev-ui is now required,
    // even though the gate's own `reviewers:` never names it.
    let (ok, err) = merge_files(&fake_files(&["docs/x.md", "src/app.ts"]));
    assert!(!ok, "a routed lane keeps the gate shut exactly as a declared one does");
    assert!(err.contains("rev-ui"), "the refusal must name the lane it is waiting on: {err}");
    assert!(
        err.contains("rule 1") && err.contains("src/**"),
        "…and WHY it is required — the rule that fired, and its paths: {err}"
    );
    assert!(!err.contains("rev-deps"), "rule 2 did not match, so its lane is not required: {err}");

    // Both rules, on one PR.
    let (ok, err) = merge_files(&fake_files(&["src/app.ts", "crates/x/Cargo.toml"]));
    assert!(!ok);
    assert!(err.contains("rev-ui") && err.contains("rev-deps"), "{err}");
    assert!(err.contains("rule 2"), "{err}");

    // The routed lanes record, and the gate opens. Nothing about them is
    // special once routing has resolved: they are counted, staled and blocked on
    // by the same code that handles a declared reviewer.
    let ui = reviewer_caller(&reg, &gid, "rev-ui");
    recorded(&reg, &ui, "7", "pass", "frontend reviewed");
    assert!(merge_files(&fake_files(&["src/app.ts"])).0, "the routed lane passed, so the gate opens");

    // …and a re-push re-stales that pass exactly as it would a declared one
    // (#1176 AC4). Executed here rather than reasoned about: routing resolves
    // into the reviewer list BEFORE the verdict counting, so this is the same
    // code path #197 already closed — which is a claim, until the shell runs it.
    reg.grant_merge(&gid, "7", None, "human").unwrap();
    let (ok, err) = merge_env(
        &shim,
        &group_dir,
        "main",
        NEW_HEAD,
        "0",
        &[("FAKE_FILES", fake_files(&["src/app.ts"]).as_str())],
    );
    assert!(!ok, "a routed lane's pass must not survive a re-push either");
    assert!(
        err.contains("rev-ui") && err.contains("EARLIER revision"),
        "the refusal must name the routed lane and say its pass is stale: {err}"
    );
    // Both lanes re-review the new head, and it clears — the routed one is not
    // a special case on the way back in either.
    reg.set_pr_head_override(Some(NEW_HEAD.into()));
    recorded(&reg, &lead, "7", "pass", "re-reviewed the new head");
    recorded(&reg, &ui, "7", "pass", "re-reviewed the new head");
    reg.grant_merge(&gid, "7", None, "human").unwrap();
    assert!(
        merge_env(&shim, &group_dir, "main", NEW_HEAD, "0",
                  &[("FAKE_FILES", fake_files(&["src/app.ts"]).as_str())]).0,
        "re-reviewing clears it, the same way it does for a declared reviewer"
    );
    // Put the fixture back on the original head for the rest of this test, both
    // verdicts with it — otherwise every assertion below would be measuring
    // staleness rather than the routing it is about.
    reg.set_pr_head_override(Some(HEAD.into()));
    recorded(&reg, &lead, "7", "pass", "reviewed");
    recorded(&reg, &ui, "7", "pass", "frontend reviewed");

    // A blocking verdict from a ROUTED lane refuses, the same way a declared
    // one's does — blockers beat approvals whoever recorded them.
    let deps = reviewer_caller(&reg, &gid, "rev-deps");
    recorded(&reg, &deps, "7", "fail", "an unreviewed dependency was added");
    let (ok, err) = merge_files(&fake_files(&["package-lock.json"]));
    assert!(!ok, "a routed lane's fail refuses the merge");
    assert!(err.contains("rev-deps"), "{err}");
    // …and that same fail is INERT on a PR whose paths its rule does not match,
    // because the rule never fires and the lane is never required.
    assert!(
        merge_files(&fake_files(&["src/app.ts"])).0,
        "rule 2 did not match, so rev-deps is not part of this PR's gate at all"
    );

    // THE FAIL-CLOSED CASE, and the one that matters most: the reduction could
    // not account for every changed file — a truncated page, a shape it could
    // not read, gh failing. Every required verdict on the PR is a live PASS and
    // it is STILL refused, because what is unknown here is *which lanes are
    // required*, and guessing "none" is guessing in favour of merging.
    // (An EMPTY capture is the same refusal and is pinned on `parse_routed_files`
    // instead: `Command::env(k, "")` is not portably distinguishable from an
    // unset variable, so driving it through this fake would be testing Windows'
    // environment-block semantics rather than the shim's.)
    for unaccountable in ["unaccountable", "null", "ok\nsrc/app.ts", "ok\nq src/app.ts"] {
        let (ok, err) = merge_files(unaccountable);
        assert!(!ok, "an unaccountable changed-file list must refuse: {unaccountable:?}");
        assert!(
            err.contains("account for every file"),
            "and say so, rather than reporting a missing verdict: {err}"
        );
    }
    let audit = fs::read_to_string(group_dir.join("audit.jsonl")).unwrap_or_default();
    assert!(audit.contains("routing-unaccountable"), "audited: {audit}");
}


#[test]
fn the_rust_gate_status_names_the_routing_rules_that_fired_and_refuses_what_the_shim_refuses() {
    // The SATISFACTION side of #1176 — the shim owns the refusal side (above),
    // being the only half that can speak at merge time; this is what the
    // reviewer that just recorded a verdict, and the task board, read.
    let (reg, _d, _repo, gid) = routed_group();
    let lead = reviewer_caller(&reg, &gid, "rev-lead");
    recorded(&reg, &lead, "7", "pass", "reviewed");

    // A diff matching rule 1: the routed lane is required, so the gate is NOT
    // satisfied by the declared reviewer's pass alone — and the line says which
    // rule pulled that lane in.
    reg.set_pr_files_override(Some(vec!["src/app.ts".into()]));
    let s = reg.gate_status_line(&gid, 7).expect("a declared gate");
    assert!(!s.contains("SATISFIED") || s.contains("NOT YET SATISFIED"), "{s}");
    assert!(s.contains("rev-ui"), "the routed lane is named: {s}");
    assert!(s.contains("rule 1") && s.contains("src/**"), "…with the rule and its paths: {s}");
    assert!(!s.contains("rev-deps"), "rule 2 did not match: {s}");

    // A diff matching nothing: the gate is exactly the declared one, and the
    // line says nothing about routing — a note about rules that did NOT fire
    // would be noise on every PR in the repo.
    reg.set_pr_files_override(Some(vec!["docs/x.md".into()]));
    let s = reg.gate_status_line(&gid, 7).expect("a declared gate");
    assert!(s.contains("SATISFIED"), "{s}");
    assert!(!s.contains("rev-ui") && !s.contains("Path routing"), "{s}");

    // A rule that fires but adds NOBODY NEW says nothing at all (rev-972 N2).
    // A rule whose reviewers are already on the static list is legal — see
    // `a_routed_reviewer_already_on_the_static_list_is_required_once_not_twice` —
    // and it fires with an empty added-set, which used to render as
    // "Path routing required  on top of…" with a hole where the names go. The
    // shim is silent in this case; the two halves describe one gate, so this is
    // too. Asserted on the ADDED-set being empty, not on the rule not firing:
    // the rule does fire, and that is the whole point of the case.
    let selfrouted = routed_repo_with(
        "    reviewers: [rev-lead]\n\
         \x20   routing:\n\
         \x20     - paths: [src/**]\n\
         \x20       reviewers: [rev-lead]\n",
    );
    let (reg2, _d2, gid2) = group_for(&selfrouted);
    let changed = vec!["src/app.ts".to_string()];
    reg2.set_pr_files_override(Some(changed.clone()));
    let lead2 = reviewer_caller(&reg2, &gid2, "rev-lead");
    recorded(&reg2, &lead2, "7", "pass", "reviewed");

    // POSITIVE CONTROL FIRST (rev-972 N10). Every assertion below is an
    // ABSENCE, and an absence passes just as well when the rule never fired at
    // all — a broken glob, a routing lookup that stopped happening, a fixture
    // whose paths silently stopped matching would all keep this test green
    // while proving nothing. So the two halves of the case are pinned
    // explicitly on the parsed gate this group is actually running: the rule
    // DOES match, and it adds nobody.
    let g2 = reg2.merge_gate(&gid2).expect("the fixture's gate must parse");
    let d2 = workflow::route_reviewers(&g2, Some(&changed)).expect("a resolvable list");
    assert_eq!(
        d2.fired.len(),
        1,
        "the rule must actually FIRE on this diff — otherwise the absences below are vacuous"
    );
    assert_eq!(
        d2.required,
        vec!["rev-lead".to_string()],
        "…and add nobody new, which is the case under test"
    );

    let s = reg2.gate_status_line(&gid2, 7).expect("a declared gate");
    assert!(s.contains("SATISFIED"), "{s}");
    assert!(!s.contains("Path routing"), "a rule that added nobody says nothing: {s}");
    // The defect's literal signature — the note rendered with a hole where the
    // names go. Subsumed by the line above, and kept because it is what a
    // regression would actually look like in the text a human reads.
    assert!(!s.contains("required  "), "and leaves no hole where the names would go: {s}");

    // And a list loomux cannot account for refuses HERE too — never SATISFIED,
    // which is the one thing the two halves must never disagree about.
    reg.set_pr_files_override(None);
    reg.set_gh_exec_override(Some((
        _repo.path().join("no-such-gh-binary"),
        Duration::from_secs(2),
    )));
    let s = reg.gate_status_line(&gid, 7).expect("a declared gate");
    assert!(s.contains("account for every file"), "{s}");
    assert!(!s.contains("SATISFIED"), "a status line may never claim what the shim would refuse: {s}");
    reg.set_gh_exec_override(None);
}

/// **No `case` inside a `$( … )` in the generated `gh` shim** (#1176).
///
/// Not style, and not something `sh -n` on a developer machine will ever tell
/// you: a `case` pattern's `)` is unbalanced, and a shell that locates the end
/// of a command substitution by COUNTING parens rather than parsing recursively
/// stops at that `)`, mis-reads everything after it, and reports a syntax error
/// at the first `;;`. bash 3.2 is such a shell, and bash 3.2 is `/bin/sh` on
/// macOS — so the shim parses cleanly under bash 5 and dash, and is a broken
/// script on one third of this repo's own CI matrix. #1176's first cut did
/// exactly that (38 failures, macOS only, every shim test at once); the fix is
/// to put the `case` in a shell FUNCTION, whose body is parsed where it is
/// defined rather than inside the substitution.
///
/// **The scan models the failing parser, not a correct one.** It counts from a
/// `$(` the way bash 3.2 does, so what it flags is what bash 3.2 would mis-read.
/// Its stated limits: it is textual, and it does not track quoting — a literal
/// `(`/`)` inside a string shifts its count, which is also true of the shell it
/// models, so the two are wrong in the same direction. It does not need to be a
/// parser to be a floor.
#[test]
fn the_gh_shim_never_puts_a_case_inside_a_command_substitution() {
    let sh = gh_shim_sh("/usr/bin/gh", &shim_paths());
    let b = sh.as_bytes();
    let mut i = 0usize;
    let mut findings: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    while i + 1 < b.len() {
        if !(b[i] == b'$' && b[i + 1] == b'(') {
            i += 1;
            continue;
        }
        // Walk to the paren this substitution CLOSES ON under a counting scanner.
        let start = i;
        let mut depth = 0i32;
        let mut j = i + 1;
        let mut saw_case = false;
        while j < b.len() {
            match b[j] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            // Bytes, not `sh[j..]`: the shim carries non-ASCII (em dashes), so a
            // string slice at an arbitrary index is a panic waiting for the
            // first comment that moves.
            if b[j..].starts_with(b"case ") {
                saw_case = true;
            }
            j += 1;
        }
        if saw_case {
            let end = (start + 160).min(b.len());
            findings.push(String::from_utf8_lossy(&b[start..end]).replace('\n', " / "));
        }
        scanned += 1;
        i = start + 2;
    }
    // POSITIVE CONTROL, the same one the schema manifest pin carries ("this test
    // must actually compare something"). `findings.is_empty()` is an ABSENCE:
    // it passes exactly as well when the scan examined nothing at all — an empty
    // shim, a `$(` detection that stopped matching, a template that grew a new
    // spelling of command substitution. The floor is deliberately loose (this
    // asserts the scan HAPPENED, not how many substitutions the shim should
    // have) so it does not turn into a second, brittle pin on the shim's shape.
    assert!(
        scanned > 0,
        "the scan found no command substitutions in the generated shim at all — it is enforcing \
         nothing, and would stay green through exactly the defect it exists to catch"
    );
    assert!(
        findings.is_empty(),
        "a `case` inside `$( … )` is a shim that parses on this machine and is a SYNTAX ERROR \
         under macOS's /bin/sh (bash 3.2). Move the `case` into a shell function and call it \
         from the substitution:\n{}",
        findings.join("\n---\n")
    );
}

#[test]
fn gh_shim_harness_refuses_a_routing_gate_file_rust_could_not_read_back() {
    if !have_sh() {
        eprintln!("SKIP gh_shim_harness_refuses_a_routing_gate_file_rust_could_not_read_back: no POSIX sh");
        return;
    }
    // THE TWO HALVES MUST AGREE, and this is the shape where they nearly did not
    // (#1176 self-review). Rust refuses a gate file carrying more routing rules
    // than `ROUTING_RULES_MAX` — `parse_gate_file` answers `None`, which both
    // callers report as "malformed, every merge refused". The shim's loop had no
    // such cap, so the same file was unreadable to one half and perfectly
    // readable to the other.
    //
    // The divergence fell on the STRICT side (more rules is more required
    // reviewers), which is exactly why it would never have been noticed. The
    // property is "the two halves agree, and both fail closed" — not "the
    // disagreement is harmless this time".
    let (reg, d, _repo, gid) = routed_group();
    let group_dir = d.path().join(gid.as_str());
    let gate_file = group_dir.join("merge_gate");
    let bin = tempfile::tempdir().unwrap();
    let shim = shim_with_fake_gh(bin.path());
    let lead = reviewer_caller(&reg, &gid, "rev-lead");
    recorded(&reg, &lead, "7", "pass", "reviewed");

    let rules = |n: usize| {
        let mut s = String::from("require all-pass\nreviewer rev-lead\n");
        for i in 1..=n {
            s.push_str(&format!("route-path {i} d{i}/**\nroute-reviewer {i} rev-lead\n"));
        }
        s
    };
    let merge_now = |files: &str| {
        reg.grant_merge(&gid, "7", None, "human").unwrap();
        merge_env(&shim, &group_dir, "main", HEAD, "0", &[("FAKE_FILES", files)])
    };

    // AT the cap: Rust reads it, so the shim must too — otherwise this test
    // would pass against a shim that simply refused everything.
    fs::write(&gate_file, rules(workflow::ROUTING_RULES_MAX)).unwrap();
    assert!(
        workflow::parse_gate_file(&fs::read_to_string(&gate_file).unwrap()).is_some(),
        "the cap itself must be readable — otherwise the case below proves nothing"
    );
    assert!(merge_now(&fake_files(&["docs/x.md"])).0, "a gate AT the cap still merges");

    // The PER-RULE PATH cap is the same bound one level down (rev-972 N1), and
    // was the half left open when the rule cap was closed. Same two assertions:
    // Rust's answer first, then the shim's, so neither is taken on trust.
    let paths = |n: usize| {
        let mut s = String::from("require all-pass\nreviewer rev-lead\n");
        for i in 1..=n {
            s.push_str(&format!("route-path 1 d{i}/**\n"));
        }
        s.push_str("route-reviewer 1 rev-lead\n");
        s
    };
    fs::write(&gate_file, paths(workflow::ROUTING_PATHS_MAX)).unwrap();
    assert!(
        workflow::parse_gate_file(&fs::read_to_string(&gate_file).unwrap()).is_some(),
        "the path cap itself must be readable — otherwise the case below proves nothing"
    );
    assert!(merge_now(&fake_files(&["docs/x.md"])).0, "a rule AT the path cap still merges");
    fs::write(&gate_file, paths(workflow::ROUTING_PATHS_MAX + 1)).unwrap();
    assert!(
        workflow::parse_gate_file(&fs::read_to_string(&gate_file).unwrap()).is_none(),
        "Rust's half refuses past the path cap — that is the fact the shim has to match"
    );
    let (ok, err) = merge_now(&fake_files(&["docs/x.md"]));
    assert!(!ok, "and so must the shim's half");
    assert!(err.contains("merge gate"), "{err}");

    // One past it: Rust cannot read it back, so the shim must refuse rather than
    // enforce a file loomux itself calls malformed.
    fs::write(&gate_file, rules(workflow::ROUTING_RULES_MAX + 1)).unwrap();
    assert!(
        workflow::parse_gate_file(&fs::read_to_string(&gate_file).unwrap()).is_none(),
        "Rust's half refuses past the cap — that is the fact the shim has to match"
    );
    let (ok, err) = merge_now(&fake_files(&["docs/x.md"]));
    assert!(!ok, "and so must the shim's half");
    assert!(err.contains("merge gate"), "{err}");
}

// ---------------------------------------------------------------------------
