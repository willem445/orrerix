//! The described run's lifecycle since #3723: its root opens IDLE and is given
//! its tasks in its pane.
//!
//! What is pinned here is everything that follows from "the task is not on the
//! form": nothing is typed into the pane, no clock runs while it waits, a task
//! begins when the root first puts a helper to work, a second task is the same
//! pane's next one, and a pane nobody gave a task to leaves nothing behind.
//! `described.rs` carries the run and the class; the pure core's own half —
//! the arcs, `begin_task`, a record from the build before — is inline in
//! `crates/loomux-engine/src/quickdrive.rs`.

use super::described::{began, describing, describing_with, open_helper, tasked};
use super::*;

use loomux_lib::orchestration::{idle_start_types_nothing, ContractCarrier, SUPPORTED_CLIS};

/// A described run on `cli`, its root open and idle.
fn idle_on(reg: &OrchRegistry, repo: &Repo, cli: &str) -> (GroupId, String) {
    describing_with(reg, repo, |r| {
        r.root = QuickStepConfig { cli: cli.into(), ..QuickStepConfig::default() };
    })
}

/// End `agent`'s pane the way a human closing it does.
fn close_pane(reg: &OrchRegistry, agent: &str, pty: u32) {
    reg.set_pty_for_test(agent, pty);
    reg.on_pty_exit(pty, Some(0), "", 0, true);
}

// ── nothing is typed ────────────────────────────────────────────────────────

/// **An idle root is typed nothing**, on a CLI whose launch carries its role
/// instructions: claude takes the whole contract on its system prompt, copilot
/// a slim copy that points at the file. The control is a helper of the same
/// run, which IS typed a kickoff, carrying its task.
#[test]
fn an_idle_root_is_typed_nothing_where_its_instructions_ride_the_launch() {
    for cli in ["claude", "copilot"] {
        let dir = tempfile::tempdir().unwrap();
        let reg = relaunch_registry(dir.path());
        let repo = Repo::new();
        let (group, root) = idle_on(&reg, &repo, cli);
        let g = reg.group(&group).unwrap();
        let a = reg.agent(&root).unwrap();

        assert_eq!(a.task, "", "{cli}: opened with no task");
        assert_ne!(
            a.contract_carrier,
            ContractCarrier::KickoffOnly,
            "{cli}: the fixture's premise — its instructions are on the launch"
        );
        assert_eq!(
            reg.fresh_kickoff(&a, &g, "", None),
            None,
            "{cli}: an idle root is typed no kickoff at all"
        );
        // Its instructions file is there to be pointed at, with no task in it
        // and no placeholder left for one.
        let file = std::fs::read_to_string(dir.path().join(group.as_str()).join("quick.md"))
            .expect("the root's instructions were written");
        assert!(file.contains("You start idle"), "{cli}");
        assert!(!file.contains("{{"), "{cli}: a placeholder survived");

        // The control: the same question about a helper answers a kickoff.
        let worker = open_helper(&reg, &group, &root, "worker");
        let w = reg.agent(&worker).unwrap();
        let kickoff = reg.fresh_kickoff(&w, &g, "", None).expect("a helper is typed its kickoff");
        assert!(kickoff.contains("Your task:") && kickoff.contains("be the worker"), "{kickoff}");
    }
}

/// **Where a CLI cannot take instructions at launch, the root is typed one
/// thing: where they are, and to wait.** Gemini has no system-prompt seam, so
/// a pointer is the only way its agent learns its role. It carries no task.
#[test]
fn a_root_whose_cli_has_no_launch_seam_is_pointed_at_its_instructions_and_told_to_wait() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, root) = idle_on(&reg, &repo, "gemini");
    let g = reg.group(&group).unwrap();
    let a = reg.agent(&root).unwrap();
    assert_eq!(a.task, "", "it is still opened with no task");

    let typed = reg.fresh_kickoff(&a, &g, "", None).expect("gemini is typed the pointer");
    assert!(typed.contains("quick.md"), "it names the instructions file: {typed}");
    assert!(typed.contains("No task is given here"), "{typed}");
    assert!(typed.contains("the human will tell you what they want in this pane"), "{typed}");
    assert!(typed.contains("the agent this quick task was given to"), "{typed}");
    // Not task-shaped: none of the words a brief opens with.
    for word in ["Your task:", "The task, in the human's own words", "A task is in progress"] {
        assert!(!typed.contains(word), "{word:?} in an idle root's pointer: {typed}");
    }
    assert!(!typed.contains("the orchestrator of"), "{typed}");
    assert!(!typed.contains("worker agent in"), "{typed}");
}

/// **The rule, over every CLI and every way the contract can ride.** Nothing
/// is typed exactly when all four hold: a quick root, no task, a CLI with a
/// launch seam, and a contract that really is on it. One CLI has no seam, and
/// the walk counts it rather than trusting the list it was written from.
#[test]
fn nothing_is_typed_exactly_when_a_taskless_root_already_has_its_instructions() {
    let on_launch = [ContractCarrier::SystemLayerFull, ContractCarrier::SystemLayerCore];
    let mut silent = Vec::new();
    for cli in SUPPORTED_CLIS {
        let quiet: Vec<bool> =
            on_launch.iter().map(|c| idle_start_types_nothing(Role::Quick, "", cli, *c)).collect();
        assert!(quiet.iter().all(|q| *q == quiet[0]), "{cli}: the two launch carriers agree");
        if quiet[0] {
            silent.push(cli);
        }
        // A contract that did not reach the launch is typed, on every CLI.
        assert!(
            !idle_start_types_nothing(Role::Quick, "", cli, ContractCarrier::KickoffOnly),
            "{cli}: a root with no instructions on its launch is pointed at them"
        );
        // A root spawned WITH text — a resumed task's message — is typed it.
        assert!(
            !idle_start_types_nothing(Role::Quick, "carry on", cli, ContractCarrier::SystemLayerFull),
            "{cli}"
        );
        assert!(
            idle_start_types_nothing(Role::Quick, "  \n", cli, ContractCarrier::SystemLayerFull)
                == quiet[0],
            "{cli}: whitespace is no task"
        );
        // And only a quick root: every other class keeps its kickoff, task or none.
        for role in Role::ALL {
            if role != Role::Quick {
                assert!(
                    !idle_start_types_nothing(role, "", cli, ContractCarrier::SystemLayerFull),
                    "{cli}: a {} is always typed its kickoff",
                    role.as_str()
                );
            }
        }
    }
    assert_eq!(SUPPORTED_CLIS.len(), 6, "the walk covered every CLI this build launches");
    assert_eq!(silent, vec!["claude", "copilot", "opencode", "pi", "codex"]);
}

// ── the clock ───────────────────────────────────────────────────────────────

/// **An idle root that sits past the run's time limit is NOT held** — the
/// limit is a task's, and there is no task. It is not on the tick's list at
/// all, so however long the human takes to say what they want costs nothing.
///
/// Then the root opens a helper, and the limit that did not apply to the
/// waiting applies to the task from that moment.
#[test]
fn an_idle_root_past_the_time_limit_is_not_held_and_the_limit_starts_with_the_task() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, root) = describing_with(&reg, &repo, |r| r.drive_timeout_minutes = Some(5));
    make_deliverable(&reg, &group, &root, 7801);
    let opened = status(&reg, &group)["started_ms"].as_u64().unwrap();
    assert_eq!(opened, T0, "the fixture's premise: the run was started at the fixture clock");

    // Six minutes, a week, and a year past the five-minute limit.
    for late in [6 * MIN, 7 * 24 * 60 * MIN, 365 * 24 * 60 * MIN] {
        assert_eq!(reg.qd_driver_tick(T0 + late), None, "an idle run is not polled");
        let out = step(&reg, &group, T0 + late);
        assert_eq!(state(&reg, &group), "root-idle", "{late} ms idle");
        assert_eq!((out.advanced.clone(), out.notice), (None, false), "{out:?}");
    }
    assert!(open_items(&reg, &group).is_empty(), "nothing was raised");
    assert!(delivered_texts(&reg, &group).is_empty(), "and nothing was typed into any pane");

    // The task begins — through the real funnel, on the process's own clock,
    // which is nowhere near the fixture's. That gap is the point: the bound is
    // measured from HERE, not from when the pane opened.
    let _worker = open_helper(&reg, &group, &root, "worker");
    let t = began(&reg, &group);
    assert!(t > opened + 365 * 24 * 60 * MIN, "the clock was re-anchored at the task's start");
    assert_eq!(reg.qd_driver_tick(t + 1), Some(group.clone()), "a task in progress IS polled");
    step(&reg, &group, t + 4 * MIN);
    assert_eq!(state(&reg, &group), "root-wait", "four minutes into a five-minute task");
    step(&reg, &group, t + 6 * MIN);
    assert_eq!(held_reason(&reg, &group), "drive-stalled", "six minutes into it");
}

/// **A task begins when the root first puts a helper to work, and the call
/// that begins it says so** — with the limits, which an idle root was handed
/// no first message to carry. Looking around begins nothing, and neither does
/// a spawn that was refused.
#[test]
fn a_task_begins_with_the_roots_first_helper_and_that_answer_states_the_limits() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, root) = describing_with(&reg, &repo, |r| {
        r.drive_timeout_minutes = Some(45);
        r.max_review_rounds = Some(2);
    });
    let begun = || action_count(&reg, &group, quickdrive::audit_action::TASK_BEGUN);

    // The reads and the housekeeping: a root may look around for as long as
    // it likes without a clock starting.
    for (tool, args) in [
        ("list_agents", json!({})),
        ("group_usage", json!({})),
        ("rename_agent", json!({ "agent_id": root, "name": "the task agent" })),
    ] {
        let (_is_error, text) = call(&reg, &root, tool, args);
        assert!(!text.contains("start of a task"), "{tool}: {text}");
    }
    // A refused spawn opened nothing, so it began nothing.
    let (is_error, refused) = call(&reg, &root, "spawn_agent", json!({ "kind": "orchestrator", "task": "t" }));
    assert!(is_error, "{refused}");
    assert_eq!((state(&reg, &group).as_str(), begun()), ("root-idle", 0));

    let (is_error, answer) = call(
        &reg,
        &root,
        "spawn_agent",
        json!({ "kind": "worker", "name": "worker", "task": "add the flag" }),
    );
    assert!(!is_error, "{answer}");
    assert!(answer.contains("start of a task in this quick run (task 1)"), "{answer}");
    assert!(answer.contains("45 minutes from now"), "the time bound, as the human set it: {answer}");
    assert!(answer.contains("at most 2 review rounds"), "and the round bound: {answer}");
    let s = status(&reg, &group);
    assert_eq!((s["state"].clone(), s["task_seq"].clone()), (json!("root-wait"), json!(1)), "{s}");
    assert_eq!(s["turn"]["side"], json!("root"), "the root holds the turn now: {s}");
    assert_eq!(begun(), 1);

    // A second helper is part of the same task: no second start, no second note.
    let (is_error, again) = call(&reg, &root, "spawn_agent", json!({ "kind": "reviewer", "task": "review it" }));
    assert!(!is_error, "{again}");
    assert!(!again.contains("start of a task"), "{again}");
    assert_eq!((status(&reg, &group)["task_seq"].clone(), begun()), (json!(1), 1));
}

/// **A helper is cut from the branch the human set on the form** when the root
/// names none — it is told no base in any message now, so the default is
/// applied where the worktree is cut. A base the root does name still wins,
/// and a steps run is untouched.
#[test]
fn a_described_runs_helpers_default_to_the_base_the_human_set() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let side = std::process::Command::new("git")
        .current_dir(&repo.repo)
        .args(["branch", "release-1"])
        .status()
        .expect("git runs");
    assert!(side.success());
    let (group, root) = describing_with(&reg, &repo, |r| r.base = "release-1".into());
    let spawned_from = |agent: &str| {
        audit_details(&reg, &group, "agent-spawn")
            .into_iter()
            .find(|d| d["agent"] == json!(agent))
            .map(|d| d["base"].clone())
            .unwrap_or_else(|| panic!("no spawn row for {agent}"))
    };

    let worker = open_helper(&reg, &group, &root, "worker");
    assert_eq!(spawned_from(&worker), json!("release-1"), "the run's base, unasked");

    let (is_error, text) = call(
        &reg,
        &root,
        "spawn_agent",
        json!({ "kind": "worker", "name": "second", "task": "t", "base": "main" }),
    );
    assert!(!is_error, "{text}");
    let named = live_agents(&reg, &group).into_iter().find(|a| a != &root && a != &worker).unwrap();
    assert_eq!(spawned_from(&named), json!("main"), "a base the root names wins");
}

// ── a second task ───────────────────────────────────────────────────────────

/// **After a task's `done` the same pane takes another**, and nothing about
/// the first stands in its way: `spawn_agent` is not refused, a helper still
/// open can be given the work with `send_prompt` — which is itself what
/// begins the task — each finished task raises its own notice, and the first
/// one's notice is still there when the second arrives.
#[test]
fn a_second_task_in_the_same_pane_is_a_task_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, root, worker) = tasked(&reg, &repo);
    make_deliverable(&reg, &group, &worker, 7811);
    let first_began = began(&reg, &group);

    report(&reg, &root, json!({ "outcome": "done", "note": "the flag is on agent/one" }));
    step(&reg, &group, T0 + 5);
    assert_eq!(state(&reg, &group), "root-idle");
    assert_eq!(reg.qd_driver_tick(first_began + 10 * MIN), None, "idle between tasks is not polled either");

    // Task two is begun by PROMPTING the helper that is still open. A run
    // that only noticed spawns would have left this task with no clock, and
    // its `done` below with nothing to end.
    let (is_error, sent) = call(
        &reg,
        &root,
        "send_prompt",
        json!({ "agent_id": worker, "text": "now add a --yaml flag too" }),
    );
    assert!(!is_error, "{sent}");
    assert!(sent.contains("start of a task in this quick run (task 2)"), "{sent}");
    let s = status(&reg, &group);
    assert_eq!((s["state"].clone(), s["task_seq"].clone()), (json!("root-wait"), json!(2)), "{s}");
    assert!(began(&reg, &group) >= first_began, "its clock was stamped at its own start");
    assert_eq!(s["last_note"], json!(""), "the first task's account is not carried into the second: {s}");

    let first = open_items(&reg, &group);
    assert_eq!(first.len(), 1, "the first task's notice is still on the list");
    assert!(first[0].text.contains("the flag is on agent/one"), "{}", first[0].text);

    report(&reg, &root, json!({ "outcome": "done", "note": "yaml is on agent/one too" }));
    step(&reg, &group, T0 + 6);
    assert_eq!(state(&reg, &group), "root-idle");
    let items = open_items(&reg, &group);
    assert_eq!(items.len(), 2, "each finished task has its own notice: {items:?}");
    let second = items.iter().find(|i| i.text.contains("yaml is on agent/one too")).expect("the second");
    assert!(second.text.contains("(task 2 in this pane)"), "numbered, so the two do not read as one: {}", second.text);
    assert!(!first[0].text.contains("in this pane)"), "the first is not numbered: {}", first[0].text);

    // And a third, this time by opening a helper: the refusal that used to
    // say "this quick run has ended" after a done is gone.
    let (is_error, opened) = call(&reg, &root, "spawn_agent", json!({ "kind": "reviewer", "task": "check both flags" }));
    assert!(!is_error, "a finished task does not stop the next: {opened}");
    assert!(opened.contains("(task 3)"), "{opened}");
    assert_eq!(state(&reg, &group), "root-wait");
}

// ── a pane nobody gave a task to ────────────────────────────────────────────

/// **An idle run is not on the list of unfinished runs** — there is nothing
/// to resume and nothing to stop — while a run with a task in progress, in the
/// same registry, is. That second row is what shows the list was really read.
#[test]
fn an_idle_run_is_not_listed_as_unfinished_and_a_task_in_progress_is() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let idle_repo = Repo::new();
    let busy_repo = Repo::new();
    let (idle, _idle_root) = describing(&reg, &idle_repo);
    let (busy, busy_root, _worker) = tasked(&reg, &busy_repo);

    let listed = |reg: &OrchRegistry| -> Vec<String> {
        reg.quick_list()
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["group_id"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    assert_eq!(listed(&reg), vec![busy.as_str().to_string()], "the idle run is absent");
    assert!(!listed(&reg).contains(&idle.as_str().to_string()));

    // It joins the list while its task is in progress, and leaves it again
    // when that task is done — not "for ever".
    make_deliverable(&reg, &busy, &busy_root, 7821);
    report(&reg, &busy_root, json!({ "outcome": "done", "note": "done" }));
    step(&reg, &busy, T0 + 5);
    assert!(listed(&reg).is_empty(), "a finished task leaves nothing on the list");
}

/// **Closing an idle root's pane ends its run** — no notice, nothing left to
/// Stop, nothing left on the list. A task that had FINISHED before the pane
/// closed keeps its notice: that work is still where the notice says.
#[test]
fn closing_an_idle_root_ends_its_run_and_leaves_nothing_to_stop() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();

    // Never given a task.
    let (group, root) = describing(&reg, &repo);
    close_pane(&reg, &root, 7831);
    assert_eq!(state(&reg, &group), "cancelled");
    assert!(open_items(&reg, &group).is_empty(), "nothing was interrupted, so nothing is raised");
    assert_eq!(action_count(&reg, &group, quickdrive::audit_action::CLOSED), 1);
    assert!(reg.quick_list().as_array().unwrap().is_empty());
    let stop = reg.quick_control(&group, "stop", None).expect_err("there is nothing left to stop");
    assert!(stop.contains("already ended"), "{stop}");
    assert_eq!(step(&reg, &group, T0 + 9).state, "cancelled", "and a later look changes nothing");

    // Given one task, which finished; then closed while idle.
    let other = Repo::new();
    let (group, root, worker) = tasked(&reg, &other);
    make_deliverable(&reg, &group, &worker, 7832);
    report(&reg, &root, json!({ "outcome": "done", "note": "on agent/one" }));
    step(&reg, &group, T0 + 5);
    assert_eq!(open_items(&reg, &group).len(), 1);
    close_pane(&reg, &root, 7833);
    assert_eq!(state(&reg, &group), "cancelled");
    assert_eq!(open_items(&reg, &group).len(), 1, "the finished task's notice is not taken back");
}

/// **A restart ends an idle run rather than parking it** — its pane died with
/// the process and no task was interrupted, so there is nothing to resume and
/// no notice to raise. The control is a run with a task in progress, which
/// the same scan parks on `restart`.
#[test]
fn a_restart_ends_an_idle_run_and_parks_one_with_a_task_in_progress() {
    let dir = tempfile::tempdir().unwrap();
    let idle_repo = Repo::new();
    let busy_repo = Repo::new();
    let (idle, busy) = {
        let reg = relaunch_registry(dir.path());
        let (idle, _root) = describing(&reg, &idle_repo);
        let (busy, _root, _worker) = tasked(&reg, &busy_repo);
        (idle, busy)
    };

    let reg = relaunch_registry(dir.path());
    assert_eq!(state(&reg, &idle), "root-idle", "the fixture's premise: nothing has looked yet");
    reg.qd_driver_tick(T0 + 10 * MIN);
    assert_eq!(state(&reg, &idle), "cancelled", "the idle run is over");
    assert!(open_items(&reg, &idle).is_empty(), "with no notice");
    assert_eq!(held_reason(&reg, &busy), "restart", "the task in progress is parked, as before");
    let listed: Vec<Value> = reg.quick_list().as_array().unwrap().clone();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["group_id"], json!(busy.as_str()));

    let err = reg.quick_resume_at(&idle, T0 + 11 * MIN).expect_err("an ended run does not resume");
    assert!(err.contains("already ended"), "{err}");
}

/// **A root pane that cannot be opened parks the run, and Resume asks
/// again.** The hold quotes the refusal — which is what the launcher shows in
/// its form — and the run is reachable until it is resumed or stopped.
#[test]
fn a_root_pane_that_cannot_open_parks_the_run_and_resume_opens_it() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let group = start_with(&reg, &repo, |r| {
        r.mode = "describe".into();
        r.task = String::new();
        r.root = QuickStepConfig { cli: "claude".into(), ..QuickStepConfig::default() };
    });
    // A root the run does not know about is already in the group: the one
    // refusal a root's own open has, and the backstop on "exactly one root".
    let stray = reg
        .spawn_agent_bound(&group, Role::Quick, Some("quick".into()), "stray", "", false, None, None, None, None, None, None)
        .expect("the stray root opens")
        .id;

    let out = step(&reg, &group, T0 + 1);
    assert!(out.refusal.contains("already has a live root pane"), "{out:?}");
    assert_eq!((out.handed_to.clone(), out.notice), (None, true), "{out:?}");
    assert_eq!(held_reason(&reg, &group), "unresumable");
    let s = status(&reg, &group);
    assert!(s["held_note"].as_str().unwrap_or_default().contains("already has a live root pane"), "{s}");
    assert_eq!(reg.quick_list().as_array().unwrap().len(), 1, "a held run is on the list");

    close_pane(&reg, &stray, 7841);
    assert_eq!(held_reason(&reg, &group), "unresumable", "a stray pane closing does not end a held run");
    let after = reg.quick_resume_at(&group, T0 + 2).expect("the run resumes");
    assert_eq!(after["state"], json!("root-idle"), "back to idle, not into a task: {after}");
    let root = pane(&reg, &group, QuickSide::Root);
    assert!(!root.is_empty() && root != stray, "with a pane of its own: {after}");
    assert_eq!(reg.agent(&root).unwrap().task, "", "opened with nothing typed");
    assert!(open_items(&reg, &group).is_empty(), "the hold's notice is answered by the resume");
}

/// **The two verbs a described run does not have say so, each in one
/// sentence.** A hand-off has no other side to go to — the agent decides who
/// works — and an idle run has no task for a note to be about. The control is
/// the same note once a task is in progress, which is taken and typed.
#[test]
fn a_described_run_has_no_hand_off_and_an_idle_one_takes_no_note() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, root) = describing(&reg, &repo);

    let note = reg
        .quick_control(&group, "note", Some("try the other parser"))
        .expect_err("an idle run takes no note");
    assert!(note.contains("no task is in progress"), "{note}");
    assert!(note.contains("in its pane"), "it says where to say it instead: {note}");
    assert!(is_one_paragraph(&note), "{note:?}");
    let handoff = reg.quick_control(&group, "handoff", None).expect_err("there is no hand-off");
    assert!(handoff.contains("a described run has no hand-off"), "{handoff}");
    assert!(is_one_paragraph(&handoff), "{handoff:?}");
    assert_eq!(state(&reg, &group), "root-idle", "neither refusal moved the run");

    // The control: with a task in progress the note is taken, and typed.
    open_helper(&reg, &group, &root, "worker");
    make_deliverable(&reg, &group, &root, 7851);
    let taken = reg.quick_control(&group, "note", Some("try the other parser")).expect("a task takes a note");
    assert_eq!(taken["typed"], json!(true), "{taken}");
    assert!(
        texts_to(&reg, &group, &root).iter().any(|t| t.contains("try the other parser")),
        "and it reached the root's pane"
    );
    let handoff = reg.quick_control(&group, "handoff", None).expect_err("still no hand-off");
    assert!(handoff.contains("a described run has no hand-off"), "{handoff}");
}

/// **Only the run's own root begins a task.** A second quick root in the group
/// — one the record does not name — is a stranger to the run: its helper is
/// opened, because the spawn rule is the class's, but the run's clock is not
/// its to start. The control is the run's own root making the same call.
#[test]
fn a_root_the_run_does_not_name_begins_no_task() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, root) = describing(&reg, &repo);
    let stray = reg
        .spawn_agent_bound(&group, Role::Quick, Some("quick".into()), "stray", "", false, None, None, None, None, None, None)
        .expect("a second root opens when nothing checks for one")
        .id;
    assert_ne!(stray, root);

    let args = || json!({ "kind": "worker", "name": "w", "task": "do it" });
    let (is_error, text) = call(&reg, &stray, "spawn_agent", args());
    assert!(!is_error, "the stray's helper is opened: {text}");
    assert!(!text.contains("start of a task"), "but it begins nothing: {text}");
    let s = status(&reg, &group);
    assert_eq!((s["state"].clone(), s["task_seq"].clone()), (json!("root-idle"), json!(0)), "{s}");

    let (is_error, text) = call(&reg, &root, "spawn_agent", args());
    assert!(!is_error, "{text}");
    assert!(text.contains("start of a task in this quick run (task 1)"), "{text}");
    assert_eq!(state(&reg, &group), "root-wait");
}

// ── review round 1: a root that dies by itself, and whose session is whose ──

/// **An idle root that exits BY ITSELF parks the run; it does not end it.**
/// An idle root is recorded the moment its pane binds — nothing is typed, so
/// nothing waits for its CLI to boot — which means a CLI that dies at boot
/// dies after it is recorded. Ending the run silently there would make
/// every failed launch a pane that vanished without a word. So the run is
/// held, with what the pane went out saying, and Resume opens a fresh root.
///
/// The control is the same exit when orrerix or the human asked for it, which
/// ends the run with no notice.
#[test]
fn an_idle_root_that_exits_by_itself_parks_the_run_and_resume_opens_a_fresh_one() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, root) = describing(&reg, &repo);

    reg.set_pty_for_test(&root, 7861);
    reg.on_pty_exit(7861, Some(1), "error: unknown model opsu", 25, false);

    assert_eq!(held_reason(&reg, &group), "unresumable", "parked, not ended");
    let s = status(&reg, &group);
    let note = s["held_note"].as_str().unwrap_or_default().to_string();
    assert!(note.contains("unknown model"), "the hold quotes what the pane went out saying: {s}");
    assert!(note.contains(&root), "and names the pane: {s}");
    let items = open_items(&reg, &group);
    assert_eq!(items.len(), 1, "the human is told");
    assert!(items[0].text.contains("unknown model"), "{}", items[0].text);
    assert!(is_one_paragraph(&items[0].text), "{:?}", items[0].text);
    assert_eq!(action_count(&reg, &group, quickdrive::audit_action::CLOSED), 0, "it was not closed");
    assert_eq!(reg.quick_list().as_array().unwrap().len(), 1, "and it is reachable from the list");

    let after = reg.quick_resume_at(&group, T0 + 2).expect("the run resumes");
    assert_eq!(after["state"], json!("root-idle"), "back to idle, with no task: {after}");
    let fresh = pane(&reg, &group, QuickSide::Root);
    assert!(!fresh.is_empty() && fresh != root, "in a fresh pane: {after}");
    assert_eq!(reg.agent(&fresh).unwrap().task, "", "opened with nothing typed");
    assert!(open_items(&reg, &group).is_empty(), "the hold's notice is answered by the resume");

    // The control, in the same registry: the exit somebody asked for ends
    // the run, and raises nothing.
    reg.set_pty_for_test(&fresh, 7862);
    reg.on_pty_exit(7862, Some(1), "error: unknown model opsu", 25, true);
    assert_eq!(state(&reg, &group), "cancelled");
    assert!(open_items(&reg, &group).is_empty());
    assert_eq!(action_count(&reg, &group, quickdrive::audit_action::CLOSED), 1);
}

/// One codex session file, as the CLI writes it: its id and the directory it
/// was started in. `tests/orchestration/helpers.rs`'s fixture, duplicated
/// because helpers do not cross integration-test binaries.
fn codex_session(store: &std::path::Path, id: &str, cwd: &str) {
    assert!(!cwd.contains('\\'), "fixture cwds use forward slashes: {cwd}");
    let day = store.join("2026").join("10").join("08");
    std::fs::create_dir_all(&day).unwrap();
    let body = format!(
        "{{\"timestamp\":\"2026-10-08T10:00:00.000Z\",\"type\":\"session_meta\",\
         \"payload\":{{\"session_id\":\"{id}\",\"id\":\"{id}\",\
         \"timestamp\":\"2026-10-08T10:00:00.000Z\",\"cwd\":\"{cwd}\",\
         \"originator\":\"codex_cli_rs\",\"cli_version\":\"0.153.4\"}}}}\n"
    );
    std::fs::write(day.join(format!("rollout-2026-10-08T10-00-00-{id}.jsonl")), body).unwrap();
}

/// **An idle root's session watch waits for the root's own first tool call**
/// where watching from the spawn could bind somebody else's session.
///
/// The search below is the watcher's own, asked at the two moments. While the
/// root waits, the only new session in its directory — the human's checkout —
/// is one the human started there, and the search answers THAT one: a watch
/// running then would bind it. Once the root has made a tool call its own
/// session is there too, and the search answers the root's, or — with both
/// present — refuses. Deferring is what moves the watch from the first moment
/// to the second.
#[test]
fn an_idle_roots_session_watch_waits_for_its_first_tool_call_where_the_store_is_shared() {
    use loomux_lib::orchestration::{defers_session_watch, SessionBaseline, SessionSearch};
    use std::collections::HashSet;

    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, root) = describing(&reg, &repo);
    let store = dir.path().join("codex-sessions");
    let here = repo.path();
    let codex = || SessionBaseline::Codex { ids: HashSet::new(), root: store.clone() };

    // The decision: deferred for an idle start on the one store that is the
    // human's and is written at the first turn, and for nothing else.
    assert!(defers_session_watch(true, &codex()));
    assert!(!defers_session_watch(false, &codex()), "a pane typed a kickoff watches at once");
    assert!(!defers_session_watch(true, &SessionBaseline::OpenCode { ids: HashSet::new() }));
    assert!(!defers_session_watch(
        true,
        &SessionBaseline::Copilot { ids: HashSet::new(), root: store.clone() }
    ));

    // THE HAZARD, at the spawn's moment: the human starts their own codex in
    // the repository while the root waits. It is the one new session there.
    codex_session(&store, "the-humans-own", &here);
    assert_eq!(
        reg.search_for_session(&group, &root, &here, &codex()),
        SessionSearch::Found("the-humans-own".to_string()),
        "the fixture's premise: a watch running now would bind a session that is not the root's"
    );

    // So the watch is held back instead of started…
    reg.qd_defer_session_watch_for_test(&group, &root, &here, codex());
    assert_eq!(reg.qd_deferred_watch_for_test(&group), Some(root.clone()));
    // …through a call that was refused, which says nothing about a turn…
    let (is_error, _) = call(&reg, &root, "spawn_agent", json!({ "kind": "orchestrator", "task": "t" }));
    assert!(is_error);
    assert_eq!(reg.qd_deferred_watch_for_test(&group), Some(root.clone()), "a refused call starts nothing");
    // …and is started by the root's first answered tool call, whatever it is:
    // the root's CLI is running a turn, so its own session exists.
    codex_session(&store, "the-roots-own", &here);
    let (is_error, text) = call(&reg, &root, "list_agents", json!({}));
    assert!(!is_error, "{text}");
    assert_eq!(reg.qd_deferred_watch_for_test(&group), None, "taken, once");
    assert_eq!(state(&reg, &group), "root-idle", "and looking around began no task");
    let watch: Vec<Value> = audit_details(&reg, &group, quickdrive::audit_action::SESSION_WATCH);
    assert_eq!(
        watch.iter().map(|d| d["when"].clone()).collect::<Vec<_>>(),
        vec![json!("deferred"), json!("started")],
        "{watch:?}"
    );

    // At THAT moment the human's session is no longer the only candidate: the
    // search refuses to choose between two, and never answers the human's.
    assert_eq!(
        reg.search_for_session(&group, &root, &here, &codex()),
        SessionSearch::Contested(2)
    );
    // And where the human started none, it is simply the root's.
    let quiet = dir.path().join("codex-sessions-quiet");
    codex_session(&quiet, "the-roots-own", &here);
    assert_eq!(
        reg.search_for_session(
            &group,
            &root,
            &here,
            &SessionBaseline::Codex { ids: HashSet::new(), root: quiet }
        ),
        SessionSearch::Found("the-roots-own".to_string())
    );

    // A watch held back for a root that then goes is dropped with it.
    reg.qd_defer_session_watch_for_test(&group, &root, &here, codex());
    close_pane(&reg, &root, 7871);
    assert_eq!(reg.qd_deferred_watch_for_test(&group), None);
}
