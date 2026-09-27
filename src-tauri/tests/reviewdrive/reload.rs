//! A workflow file that stops loading, or a driver switched off, with a drive on disk (#3330).
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── #3330: a workflow file that stops loading turns the driver off ──────────

/// The group's workflow file plus one top-level key the schema does not know.
/// `RawWorkflow` is `deny_unknown_fields`, so the WHOLE file is refused — the
/// shape of the measured incident, where the installed build predated a key the
/// file had just gained.
fn unparseable(key: &str) -> String {
    format!("{WORKFLOW}{key}: true\n")
}

/// The lines this group's orchestrator has received about its workflow file.
fn workflow_notices(reg: &OrchRegistry, group: &GroupId, orch: &str) -> Vec<String> {
    texts_to(reg, group, orch).into_iter().filter(|t| t.contains("does not load")).collect()
}

/// `workflow-invalid` rows the RELOAD pass wrote — the launch writes one of its
/// own under the same action, without `at`, and these tests are not about it.
fn reload_invalid_rows(reg: &OrchRegistry, group: &GroupId) -> Vec<serde_json::Value> {
    audit_details(reg, group, "workflow-invalid")
        .into_iter()
        .filter(|d| d["at"] == json!("reload"))
        .collect()
}

/// **The red.** A group with a live drive on disk is restarted over a
/// workflow file that no longer parses. The driver reads OFF — correctly, since
/// off is its answer to every uncertainty — and on `main` nothing says so: no
/// line in the orchestrator's pane, the drive listed and never ticked. That is
/// the eight silent hours #3330 measured.
///
/// Pinned here: the reload pass says it ONCE (two passes, one line), the line
/// names the error and the drive it is holding still, a fixed file clears the
/// latch without a line of its own, and a DIFFERENT error later is announced
/// again.
#[test]
fn a_workflow_file_that_stops_loading_says_so_once_and_names_the_drive_it_holds() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = {
        let reg = relaunch_registry(dir.path());
        let (group, _session) = driven(&reg, &repo, &gh);
        group
    };
    repo.rewrite_workflow(&unparseable("not_a_workflow_key"));
    let reg = relaunch_registry(dir.path());
    reattach(&reg, &repo, &group);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    make_delivery_land(&reg, &group, &orch.id, 7501);

    // The premise, both halves: the driver reads off, and the drive is still
    // on disk for it to be holding still.
    let status = reg.review_drive_status(&group);
    assert_eq!(status["enabled"], json!(false), "the broken file must read as off: {status}");
    assert_eq!(status["drives"][0]["pr"], json!(1758), "the drive must be on disk: {status}");
    assert!(workflow_notices(&reg, &group, &orch.id).is_empty(), "nothing said before the pass");

    reg.reload_merge_gate_if_changed(&group);
    reg.reload_merge_gate_if_changed(&group);

    let notices = workflow_notices(&reg, &group, &orch.id);
    assert_eq!(notices.len(), 1, "exactly one line across two passes: {notices:#?}");
    let line = &notices[0];
    for must in ["not_a_workflow_key", "PR #1758", "driver-disabled", "plan driver"] {
        assert!(line.contains(must), "the line must name `{must}`: {line}");
    }
    assert!(
        !line.contains('\n') && !line.contains("          "),
        "one paragraph, no source indentation: {line:?}"
    );
    let rows = reload_invalid_rows(&reg, &group);
    assert_eq!(rows.len(), 1, "one audit row per transition: {rows:#?}");
    assert_eq!(rows[0]["review_drives_unticked"], json!([1758]), "{rows:#?}");

    // The driver tick's own off-with-drives announcement stands aside for this
    // cause — same group, same drive, so one fact is not said twice.
    assert_eq!(reg.rd_driver_tick(10_000), None, "a disabled group is never picked");
    assert!(disabled_notices(&reg, &group, &orch.id).is_empty(), "the reload pass owns this case");
    assert_eq!(action_count(&reg, &group, "rd-disabled-with-drives"), 0);

    // Fixed: the driver is back on the next read, and the latch clears
    // silently — no second line, no second row.
    repo.rewrite_workflow(WORKFLOW);
    reg.reload_merge_gate_if_changed(&group);
    assert_eq!(reg.review_drive_status(&group)["enabled"], json!(true));
    assert_eq!(workflow_notices(&reg, &group, &orch.id).len(), 1);
    assert_eq!(reload_invalid_rows(&reg, &group).len(), 1);

    // Broken again, differently: a new transition, announced again.
    repo.rewrite_workflow(&unparseable("another_unknown_key"));
    reg.reload_merge_gate_if_changed(&group);
    let notices = workflow_notices(&reg, &group, &orch.id);
    assert_eq!(notices.len(), 2, "a later break is a new transition: {notices:#?}");
    assert!(notices[1].contains("another_unknown_key"), "{}", notices[1]);
    assert_eq!(reload_invalid_rows(&reg, &group).len(), 2);
}

/// **A line that cannot land is retried, and its row is not re-written.** The
/// orchestrator pane is often not up yet on the first pass after a restart;
/// latching on the attempt rather than on the landing would spend the only
/// notice on a pane that was not there.
#[test]
fn a_workflow_notice_that_cannot_land_is_retried_until_it_does() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::with(&unparseable("not_a_workflow_key"));
    let reg = relaunch_registry(dir.path());
    let group = reg.create_group(&repo.path(), rails()).unwrap().id;
    // An orchestrator with no pane: `deliver_prompt` answers `Err` (#569).
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();

    reg.reload_merge_gate_if_changed(&group);
    assert!(workflow_notices(&reg, &group, &orch.id).is_empty(), "no pane, so nothing landed");
    assert_eq!(reload_invalid_rows(&reg, &group).len(), 1, "the row is written on the transition");

    make_delivery_land(&reg, &group, &orch.id, 7502);
    reg.reload_merge_gate_if_changed(&group);
    reg.reload_merge_gate_if_changed(&group);
    let notices = workflow_notices(&reg, &group, &orch.id);
    assert_eq!(notices.len(), 1, "retried once the pane is up, then latched: {notices:#?}");
    assert!(notices[0].contains("No review drive is on disk"), "{}", notices[0]);
    assert_eq!(reload_invalid_rows(&reg, &group).len(), 1, "and the row is not repeated");
}

/// **The brief's own pin, GREEN ON BASE and labelled so** (#3330). The issue
/// was first read as a stale `enabled` surviving a restart; the policy has no
/// cache — `driver_policy_for` re-reads the file on every call — so this held
/// before the fix too. Kept because it pins the half of the fix that must NOT
/// fire: a group whose file loads, driven across a restart, stays enabled,
/// really ticks, and gets neither a `workflow-invalid` row from the reload
/// pass nor a line.
#[test]
fn the_driver_reads_enabled_across_a_restart_and_the_reload_says_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = {
        let reg = relaunch_registry(dir.path());
        let (group, _session) = driven(&reg, &repo, &gh);
        group
    };
    let reg = relaunch_registry(dir.path());
    reattach(&reg, &repo, &group);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    make_delivery_land(&reg, &group, &orch.id, 7503);

    assert_eq!(reg.review_drive_status(&group)["enabled"], json!(true));
    let before = action_count(&reg, &group, "rd-recovered");
    reg.rd_drive_group_with(&group, &gh, 50_000);
    assert_eq!(
        action_count(&reg, &group, "rd-recovered"),
        before + 1,
        "the restarted registry must reconcile and tick, or `enabled` is a word and not a fact"
    );
    reg.reload_merge_gate_if_changed(&group);
    reg.reload_merge_gate_if_changed(&group);
    // The production tick too, which is where the off-with-drives sweep runs.
    let runner: std::sync::Arc<dyn RdRunner> = std::sync::Arc::new(FakeGh::green(HEAD_A));
    reg.set_rd_runner_override(Some(runner));
    assert_eq!(reg.rd_driver_tick(60_000), Some(group.clone()), "an enabled group is picked");

    let disabled: Vec<String> = audit_actions(&reg, &group)
        .into_iter()
        .filter(|a| (a.starts_with("rd-") && a.contains("disabled")) || a == "workflow-invalid")
        .collect();
    assert!(disabled.is_empty(), "no disabled or invalid row on a file that loads: {disabled:?}");
    assert!(workflow_notices(&reg, &group, &orch.id).is_empty());
    assert_eq!(reg.review_drive_status(&group)["enabled"], json!(true));
}

/// The HOLD lines a group's orchestrator has received about a driver that is
/// off while drives sit on disk (#3330 ask 2).
fn disabled_notices(reg: &OrchRegistry, group: &GroupId, orch: &str) -> Vec<String> {
    texts_to(reg, group, orch)
        .into_iter()
        .filter(|t| t.contains("review driver off with drives on disk"))
        .collect()
}

/// **#3330 ask 2, the deliberate case.** A VALID workflow that switches the
/// driver off while a drive is unfinished leaves that drive listed and never
/// ticked: `next_rd_group` skips the group, so no tick and no reconcile ever
/// runs there. On `main` nothing says so. Pinned: the tick says it ONCE (two
/// ticks, one line and one row), names the cause and the PR, turning the driver
/// back on clears the latch silently and really drives again, and switching it
/// off again is a new transition.
#[test]
fn a_driver_switched_off_with_a_drive_on_disk_says_so_once() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let group = {
        let reg = relaunch_registry(dir.path());
        let (group, _session) = driven(&reg, &repo, &gh);
        group
    };
    // The `driver:` block is WORKFLOW's last, so its `enabled: true` is the last
    // one (`merge_queue:` has the other). Spliced by position rather than by a
    // multi-line anchor, which a CRLF checkout of this file would not match.
    let at = WORKFLOW.rfind("enabled: true").expect("WORKFLOW enables the driver");
    assert!(WORKFLOW[..at].rfind("driver:") > WORKFLOW[..at].rfind("merge_queue:"));
    let off = format!("{}enabled: false{}", &WORKFLOW[..at], &WORKFLOW[at + "enabled: true".len()..]);
    repo.rewrite_workflow(&off);
    let reg = relaunch_registry(dir.path());
    reattach(&reg, &repo, &group);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    make_delivery_land(&reg, &group, &orch.id, 7504);
    let runner: std::sync::Arc<dyn RdRunner> = std::sync::Arc::new(FakeGh::green(HEAD_A));
    reg.set_rd_runner_override(Some(runner));

    let status = reg.review_drive_status(&group);
    assert_eq!(status["enabled"], json!(false), "the premise: the driver reads off: {status}");
    assert_eq!(status["drives"][0]["pr"], json!(1758), "…with the drive on disk: {status}");

    assert_eq!(reg.rd_driver_tick(10_000), None, "a disabled group is never picked");
    assert_eq!(reg.rd_driver_tick(20_000), None);
    let notices = disabled_notices(&reg, &group, &orch.id);
    assert_eq!(notices.len(), 1, "exactly one line across two ticks: {notices:#?}");
    for must in ["driver.enabled", "PR #1758", "HOLD"] {
        assert!(notices[0].contains(must), "the line must name `{must}`: {}", notices[0]);
    }
    assert!(
        !notices[0].contains('\n') && !notices[0].contains("          "),
        "one paragraph, no source indentation: {:?}",
        notices[0]
    );
    let rows = audit_details(&reg, &group, "rd-disabled-with-drives");
    assert_eq!(rows.len(), 1, "one row per transition: {rows:#?}");
    assert_eq!(rows[0]["prs"], json!([1758]), "{rows:#?}");
    assert!(workflow_notices(&reg, &group, &orch.id).is_empty(), "the file parses: not that notice");

    // Back on: the tick picks the group and the drive moves again; no line.
    repo.rewrite_workflow(WORKFLOW);
    let recovered = action_count(&reg, &group, "rd-recovered");
    assert_eq!(reg.rd_driver_tick(30_000), Some(group.clone()), "back on, the group is driven");
    assert_eq!(action_count(&reg, &group, "rd-recovered"), recovered + 1, "…and reconciled");
    assert_eq!(disabled_notices(&reg, &group, &orch.id).len(), 1);

    // Off again: a new transition, announced again.
    repo.rewrite_workflow(&off);
    assert_eq!(reg.rd_driver_tick(40_000), None);
    assert_eq!(disabled_notices(&reg, &group, &orch.id).len(), 2, "a later switch-off is announced");
    assert_eq!(action_count(&reg, &group, "rd-disabled-with-drives"), 2);
}

/// The same announcement for the OTHER way a driver goes off: the human
/// turning the advanced orchestrator off for the group, with the file still
/// declaring the driver. The cause is named, so the orchestrator knows which
/// switch to look at.
#[test]
fn an_advanced_orchestrator_toggled_off_with_a_drive_on_disk_says_so_once() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::new();
    let gh = FakeGh::green(HEAD_A);
    let reg = relaunch_registry(dir.path());
    let (group, _session) = driven(&reg, &repo, &gh);
    let orch = reg.spawn_agent(&group, Role::Orchestrator, "orch", "", false, None).unwrap();
    make_delivery_land(&reg, &group, &orch.id, 7505);

    reg.set_advanced_orchestrator(&group, false, "human").unwrap();
    assert_eq!(reg.review_drive_status(&group)["enabled"], json!(false), "the premise");

    assert_eq!(reg.rd_driver_tick(10_000), None);
    assert_eq!(reg.rd_driver_tick(20_000), None);
    let notices = disabled_notices(&reg, &group, &orch.id);
    assert_eq!(notices.len(), 1, "{notices:#?}");
    assert!(notices[0].contains("advanced orchestrator is off"), "{}", notices[0]);
    assert!(notices[0].contains("PR #1758"), "{}", notices[0]);
}
