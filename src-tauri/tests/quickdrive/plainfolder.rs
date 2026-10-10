//! A quick run in a folder that is NOT a git repository (#3878).
//!
//! Every worker and reviewer used to be cut a git worktree, so a run started
//! in a plain folder was refused its first helper with git's own
//! `fatal: not a git repository`. It now opens them in the folder itself —
//! and only there: git is asked in one predicate (`qd_plain_folder`), scoped
//! to a quick group, and four things below hold it in place. What a pane was
//! GIVEN is read off records instead, and the last section pins that.
//!
//! 1. The behaviour, in both modes: a described run's `spawn_agent` and
//!    `fork_session`, and a steps run's own worker and reviewer.
//! 2. The control that it is not "always in place": the same calls in a
//!    repository still cut a worktree each.
//! 3. The control that it is not "everybody's": an ordinary group in the same
//!    plain folder is still refused, by git, as it always was.
//! 4. The refusal that it is not "any git failure": a folder git knows and
//!    will not open is an error, never read as a plain folder.

use super::described::{describing, open_helper, tasked, ROOT_BODY};
use super::*;

use loomux_lib::orchestration::{
    quick_folder_unknown_refusal, quick_plain_folder_disclosure, quick_plain_folder_note,
    QUICK_ROOT_PLAIN_FOLDER_NOTE, QUICK_ROOT_WORKSPACE_NOTE,
};

/// What the answer to a spawn carries when the helper opened in place.
const IN_PLACE: &str = "opened in the folder itself";

/// An agent's working directory, separators normalised to the form
/// [`Repo::path`] answers.
fn cwd_of(reg: &OrchRegistry, agent: &str) -> String {
    reg.agent(agent).unwrap_or_else(|| panic!("no such agent {agent}")).cwd.replace('\\', "/")
}

/// The directory a worktree of `repo` would have been cut under — the
/// sibling `git_worktree_add_sync` makes. Its absence is what "no worktree was
/// cut" means on disk.
fn worktrees_dir(repo: &Repo) -> std::path::PathBuf {
    let name = repo.repo.file_name().unwrap().to_string_lossy().into_owned();
    repo.repo.parent().unwrap().join(format!("{name}-worktrees"))
}

/// The one helper an answer says it opened: the roster's new pane.
fn opened_by(reg: &OrchRegistry, group: &GroupId, before: &[String]) -> String {
    let new: Vec<String> =
        live_agents(reg, group).into_iter().filter(|a| !before.contains(a)).collect();
    assert_eq!(new.len(), 1, "exactly one pane was opened: {new:?}");
    new[0].clone()
}

/// The disclosure sentence inside a spawn's answer: from its `NOTE:` to the
/// end of that paragraph. The answer of the call that BEGINS a task carries
/// the task's limits in a paragraph after it.
fn note_in(answer: &str) -> &str {
    let from = answer.find("NOTE: ").unwrap_or_else(|| panic!("the answer carries no NOTE: {answer}"));
    let rest = &answer[from..];
    &rest[..rest.find("\n\n").unwrap_or(rest.len())]
}

// ── the described run ───────────────────────────────────────────────────────

/// **A described run in a plain folder opens its worker and its reviewer in
/// that folder** — no worktree, no branch — and each answer says so, with the
/// reason.
///
/// On `main` the first `spawn_agent` is refused with
/// `fatal: not a git repository (or any of the parent directories): .git`.
#[test]
fn a_described_run_in_a_plain_folder_opens_its_worker_and_reviewer_in_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let folder = Repo::plain();
    let (group, root) = describing(&reg, &folder);
    assert_eq!(cwd_of(&reg, &root), folder.path(), "the root opens there, as it always did");

    for kind in ["worker", "reviewer"] {
        let before = live_agents(&reg, &group);
        let (is_error, answer) = call(
            &reg,
            &root,
            "spawn_agent",
            json!({ "kind": kind, "name": kind, "task": format!("be the {kind}") }),
        );
        assert!(!is_error, "a {kind} must open in a folder that is not a git repository: {answer}");
        let helper = opened_by(&reg, &group, &before);
        let a = reg.agent(&helper).unwrap();
        assert_eq!(a.role.as_str(), kind);
        assert_eq!(cwd_of(&reg, &helper), folder.path(), "the {kind} is in the folder itself");
        assert_eq!(a.branch, None, "and was cut no branch: there is nothing to cut one from");

        // The agent is told the truth, in the answer its next brief is written from.
        let note = note_in(&answer);
        assert!(note.contains(&format!("{helper} {IN_PLACE}")), "{answer}");
        assert!(note.contains("no worktree and no branch"), "{answer}");
        assert!(note.contains("is not a git repository"), "and why: {answer}");
        assert!(note.contains("do not have two workers changing it at once"), "{answer}");
        assert!(!note.contains("ignored"), "nothing was passed, so nothing was ignored: {answer}");
        assert!(is_one_paragraph(note), "{note:?}");
    }
    assert_eq!(live_agents(&reg, &group).len(), 3, "the root and its two helpers");
    assert!(!worktrees_dir(&folder).exists(), "no worktree directory was made beside the folder");

    // The audit row does not claim the worktree the spawn asked for.
    let spawns = audit_details(&reg, &group, "agent-spawn");
    let in_place: Vec<bool> = spawns.iter().map(|d| d["in_place"] == json!(true)).collect();
    assert_eq!(in_place, vec![false, true, true], "the root, then the two helpers: {spawns:?}");
}

/// **A `branch` or `base` passed in a plain folder is IGNORED, and the answer
/// names which** — not refused: the root followed its tool description, and a
/// refusal would cost it a round to learn what one sentence says.
#[test]
fn a_branch_or_base_passed_in_a_plain_folder_is_ignored_and_the_answer_says_which() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let folder = Repo::plain();
    let (group, root) = describing_with_cap(&reg, &folder, 4);

    let open = |extra: Value| -> (String, String) {
        let before = live_agents(&reg, &group);
        let mut args = json!({ "kind": "worker", "task": "t" });
        for (k, v) in extra.as_object().unwrap() {
            args[k.as_str()] = v.clone();
        }
        let (is_error, answer) = call(&reg, &root, "spawn_agent", args);
        assert!(!is_error, "the argument is ignored, not refused: {answer}");
        (opened_by(&reg, &group, &before), answer)
    };

    let (both, answer) = open(json!({ "branch": "feat/x", "base": "main" }));
    assert!(
        note_in(&answer).contains("The `branch` and `base` you passed were ignored"),
        "{answer}"
    );
    assert_eq!(reg.agent(&both).unwrap().branch, None, "the branch it named was never recorded");
    assert_eq!(cwd_of(&reg, &both), folder.path());

    let (_one, answer) = open(json!({ "base": "main" }));
    assert!(note_in(&answer).contains("The `base` you passed was ignored"), "{answer}");
    assert!(!note_in(&answer).contains("`branch`"), "only what was passed is named: {answer}");

    // The control: with neither passed, nothing is said to have been ignored —
    // the run's own base, which the spawn defaults from the form, is not the
    // caller's argument.
    let (_none, answer) = open(json!({}));
    assert!(note_in(&answer).contains(IN_PLACE), "{answer}");
    assert!(!note_in(&answer).contains("ignored"), "{answer}");
    assert!(!worktrees_dir(&folder).exists());
}

/// A described run with room for `cap` helpers. `describing_with` is the
/// fixture; this only names the one knob these tests turn.
fn describing_with_cap(reg: &OrchRegistry, repo: &Repo, cap: u32) -> (GroupId, String) {
    super::described::describing_with(reg, repo, |r| r.max_agents = Some(cap))
}

/// **A fork is a spawn**, so it lands in the folder by the same decision and
/// says so in the same words: `fork_session` on a plain-folder worker opens
/// the fork beside it, with no worktree cut from a branch that does not exist.
#[test]
fn a_fork_in_a_plain_folder_opens_in_the_folder_too() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let folder = Repo::plain();
    let (group, root) = describing_with_cap(&reg, &folder, 4);
    let worker = open_helper(&reg, &group, &root, "worker");

    let before = live_agents(&reg, &group);
    let (is_error, answer) =
        call(&reg, &root, "fork_session", json!({ "agent": worker, "task": "the other half" }));
    assert!(!is_error, "a fork must open in a folder that is not a git repository: {answer}");
    let fork = opened_by(&reg, &group, &before);
    assert_eq!(cwd_of(&reg, &fork), folder.path());
    assert_eq!(reg.agent(&fork).unwrap().branch, None);
    assert!(note_in(&answer).contains(&format!("{fork} {IN_PLACE}")), "{answer}");
    assert!(!worktrees_dir(&folder).exists());
}

/// **The positive control: in a repository nothing changed.** The same two
/// calls each cut a worktree — neither is the repository, and they are not
/// each other's — and no answer says "in the folder itself". A predicate that
/// answered "plain" for every quick run fails here.
#[test]
fn a_described_run_in_a_repository_still_cuts_each_helper_a_worktree_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (group, root) = describing(&reg, &repo);

    let mut cwds = Vec::new();
    for kind in ["worker", "reviewer"] {
        let before = live_agents(&reg, &group);
        let (is_error, answer) =
            call(&reg, &root, "spawn_agent", json!({ "kind": kind, "task": format!("be the {kind}") }));
        assert!(!is_error, "{answer}");
        assert!(!answer.contains(IN_PLACE), "a repository's helper is not in place: {answer}");
        let helper = opened_by(&reg, &group, &before);
        let a = reg.agent(&helper).unwrap();
        assert_ne!(cwd_of(&reg, &helper), repo.path(), "the {kind} is never in the human's checkout");
        assert!(std::path::Path::new(&a.cwd).join(".git").exists(), "its cwd is a git worktree: {}", a.cwd);
        assert!(a.branch.is_some(), "on a branch that was cut for it");
        cwds.push(cwd_of(&reg, &helper));
    }
    assert_ne!(cwds[0], cwds[1], "and each has its own");
    assert!(worktrees_dir(&repo).is_dir(), "the control's control: worktrees really were cut");
    let spawns = audit_details(&reg, &group, "agent-spawn");
    assert!(spawns.iter().all(|d| d["in_place"] == json!(false)), "{spawns:?}");
}

// ── the steps run ───────────────────────────────────────────────────────────

/// **A steps run in a plain folder opens its worker in the folder, and then
/// its reviewer in the same one** — and neither brief names a worktree, a
/// branch or a git command that would fail there.
///
/// On `main` the first step cannot open the worker, and the run is held with
/// git's `fatal: not a git repository` as its note.
#[test]
fn a_steps_run_in_a_plain_folder_opens_its_worker_then_its_reviewer_in_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let folder = Repo::plain();
    let group = start(&reg, &folder);

    let out = step(&reg, &group, T0 + 1);
    let s = status(&reg, &group);
    let (side, worker, how) = out
        .handed_to
        .clone()
        .unwrap_or_else(|| panic!("the first step must open the worker in a plain folder: {s}"));
    assert_eq!((side.as_str(), how.as_str()), ("worker", "opened"), "{s}");
    let w = reg.agent(&worker).unwrap();
    assert_eq!(cwd_of(&reg, &worker), folder.path(), "the worker is in the folder itself");
    assert_eq!(w.branch, None);
    assert_eq!(s["cwd"].as_str().map(|c| c.replace('\\', "/")), Some(folder.path()), "the run recorded where: {s}");
    assert_eq!(s["branch"], json!(""), "and that there is no branch: {s}");

    // The work brief, as the pane was opened with it.
    let work = lf(&w.task);
    assert!(work.contains("Work in the folder you were opened in"), "{work}");
    assert!(work.contains("You were given no worktree and no branch"), "{work}");
    assert!(work.contains("name the files you changed"), "the reviewer has no diff to read: {work}");
    for wrong in ["Work in your worktree", "commit as you go", "the branch you were given"] {
        assert!(!work.contains(wrong), "the brief told a plain-folder worker to {wrong:?}: {work}");
    }
    // Everything that is not about the workspace is the repository's brief, to
    // the word: the task, and the one way out of the turn.
    assert!(work.starts_with(TASK), "{work}");
    assert!(work.contains("When the work is ready for review, call report(outcome=done"), "{work}");
    assert!(!work.contains("{{"), "a placeholder survived: {work}");

    report(&reg, &worker, json!({ "outcome": "done", "note": "added the flag in list.rs" }));
    let out = step(&reg, &group, T0 + 2);
    let s = status(&reg, &group);
    let (side, reviewer, how) = out
        .handed_to
        .clone()
        .unwrap_or_else(|| panic!("the worker's done must open the reviewer: {s}"));
    assert_eq!((side.as_str(), how.as_str()), ("reviewer", "opened"), "{s}");
    let r = reg.agent(&reviewer).unwrap();
    assert_eq!(cwd_of(&reg, &reviewer), folder.path(), "the reviewer reads the work where it is");
    assert_eq!(r.branch, None);
    assert!(!worktrees_dir(&folder).exists(), "no worktree directory was made beside the folder");

    let review = lf(&r.task);
    let recorded = s["cwd"].as_str().expect("the run recorded the worker's folder");
    assert!(review.contains(&format!("The work is in {recorded}.")), "{review}");
    assert!(review.contains("The worker was given no worktree and no branch there"), "{review}");
    assert!(review.contains("The worker's note: added the flag in list.rs"), "{review}");
    for wrong in ["on branch", "the worker's own worktree", "git status", "git diff", "git log"] {
        assert!(!review.contains(wrong), "the brief named {wrong:?} in a plain folder: {review}");
    }
    assert!(review.contains("report(outcome=approved"), "the verdict's way out is unchanged: {review}");
    assert!(!review.contains("{{"), "a placeholder survived: {review}");
    // The seam is the delivery, as in `briefs.rs`.
    assert_eq!(lf(&reg.qd_brief_for_test(&group)), review);

    // And the run finishes: the notice names no branch, because there is none.
    report(&reg, &reviewer, json!({ "outcome": "approved", "note": "reads right" }));
    assert_eq!(
        step(&reg, &group, T0 + 3).advanced,
        Some(("review-wait".to_string(), "satisfied".to_string()))
    );
    let items = open_items(&reg, &group);
    assert_eq!(items.len(), 1, "{items:?}");
    assert!(items[0].text.contains("Nothing was merged"), "{}", items[0].text);
    assert!(!items[0].text.contains("on branch"), "{}", items[0].text);
}

/// **A planner still opens in a plain folder** — it never had a worktree — and
/// its brief no longer says the work "will be cut from" a branch.
#[test]
fn a_planner_opens_in_a_plain_folder_and_is_not_told_the_work_is_cut_from_a_branch() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let folder = Repo::plain();
    let group = start_with(&reg, &folder, |r| r.plan_step = true);
    let out = step(&reg, &group, T0 + 1);
    let (side, planner, _how) = out.handed_to.clone().expect("the planner is opened first");
    assert_eq!(side, "planner");
    assert_eq!(cwd_of(&reg, &planner), folder.path());

    let plan = lf(&reg.agent(&planner).unwrap().task);
    assert!(plan.contains("which is not a git repository"), "{plan}");
    assert!(plan.contains("plan no commit and no pull request"), "{plan}");
    assert!(!plan.contains("will be cut from"), "{plan}");
    assert!(!plan.contains("{{"), "a placeholder survived: {plan}");
}

/// **A plain-folder run resumes after a restart**, in the folder: the resume
/// reads the worker's recorded workspace, and that workspace being the run's
/// own folder is not the "main clone" the resume path refuses.
#[test]
fn a_plain_folder_run_resumes_its_worker_in_the_folder_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let folder = Repo::plain();
    let (group, worker, session) = {
        let reg = relaunch_registry(dir.path());
        let (group, worker) = working(&reg, &folder);
        let a = reg.agent(&worker).unwrap();
        (group, worker, a.session_id.clone().expect("claude mints a session id"))
    };

    let reg = relaunch_registry(dir.path());
    let after = reg.quick_resume_at(&group, T0 + 10 * MIN).expect("the run resumes");
    assert_eq!(after["state"], json!("work-wait"), "{after}");
    let reopened = pane(&reg, &group, QuickSide::Worker);
    assert_ne!(reopened, worker, "the old pane died with the old process");
    let a = reg.agent(&reopened).expect("a new pane is on the roster");
    assert_eq!(a.session_id.as_deref(), Some(session.as_str()), "running the recorded session");
    assert_eq!(cwd_of(&reg, &reopened), folder.path(), "in the folder the work is in");
    assert_eq!(a.branch, None);
    let brief = lf(&a.task);
    assert!(brief.contains("the human resumed this quick run"), "{brief}");
    assert!(brief.contains("Work in the folder you were opened in"), "the same brief it began with: {brief}");
}

// ── the two controls on the predicate ───────────────────────────────────────

/// **Scoped to a quick run.** An ordinary group in the very same plain folder
/// is still refused its worker, by git, in git's words — the worktree
/// guarantee (#338/#359) is not relaxed for a workflow built on branches and
/// a merge gate. A predicate that dropped its quick-group test fails here.
#[test]
fn an_ordinary_group_in_a_plain_folder_is_still_refused_its_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let folder = Repo::plain();
    // The control, first: a quick run's worker opens in this folder.
    let (quick, quick_worker) = working(&reg, &folder);
    assert_eq!(cwd_of(&reg, &quick_worker), folder.path());

    let ordinary = reg
        .create_group(&folder.path(), Guardrails { agent_cli: "claude".into(), ..Guardrails::default() })
        .unwrap()
        .id;
    assert_ne!(ordinary, quick, "the two launches must not share a group");
    assert!(!reg.is_quick_group(&ordinary));
    let orch = reg.spawn_agent(&ordinary, Role::Orchestrator, "orch", "", false, None).unwrap();
    for role in [Role::Worker, Role::Reviewer] {
        let err = reg
            .spawn_agent(&ordinary, role, "x", "t", true, None)
            .expect_err("an ordinary group's delegate needs a repository to be cut a worktree from");
        assert!(err.contains("not a git repository"), "git's own refusal, as before: {err}");
        assert!(!err.contains("quick run"), "and not the quick run's sentence: {err}");
    }
    assert_eq!(live_agents(&reg, &ordinary), vec![orch.id], "nothing was opened in place");
}

/// **A git failure that is not "no repository" is an ERROR, in both modes** —
/// never read as a plain folder. The specimen is a bare repository: git knows
/// the folder and will not cut a worktree in it, and a helper put to work
/// there "in place" would be editing a repository's own object store.
#[test]
fn a_folder_git_will_not_open_is_refused_and_never_read_as_a_plain_folder() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let bare = Repo::bare();

    // Described: the root opens (it never needed git), and its helper does not.
    let (group, root) = describing(&reg, &bare);
    for kind in ["worker", "reviewer"] {
        let (is_error, answer) = call(&reg, &root, "spawn_agent", json!({ "kind": kind, "task": "t" }));
        assert!(is_error, "a {kind} must not open in place in a bare repository: {answer}");
        assert!(
            answer.contains("could not tell whether this quick run's folder"),
            "the refusal says what was being asked: {answer}"
        );
        assert!(answer.contains("so it opened nothing there"), "{answer}");
        // A failure that is not "no git" is QUOTED: git's own words, as this
        // machine's git says them, are in the answer.
        assert!(answer.contains(&bare.work_tree_refusal()), "{answer}");
        assert!(!answer.contains(IN_PLACE), "{answer}");
    }
    assert_eq!(live_agents(&reg, &group), vec![root.clone()], "no helper was opened");
    assert_eq!(state(&reg, &group), "root-idle", "and a refused spawn begins no task");

    // Steps: the run is held, with the refusal as its note.
    let steps = start(&reg, &bare);
    let out = step(&reg, &steps, T0 + 1);
    assert_eq!(out.handed_to, None, "{out:?}");
    let s = status(&reg, &steps);
    assert_eq!(s["state"], json!("held"), "{s}");
    assert_eq!(s["held_reason"], json!("unresumable"), "{s}");
    let note = s["held_note"].as_str().unwrap_or_default();
    assert!(note.contains("could not tell whether this quick run's folder"), "{s}");
    assert!(live_agents(&reg, &steps).is_empty());
}

/// **No git at all is worded, never shown as the git layer's sentinel.** What
/// that layer answers when there is no `git` to run is a token its other
/// callers compare against, and a refusal ending in it tells nobody what to
/// do. Every other failure is quoted as git said it.
///
/// A pure function, because a test cannot uninstall git. The one line that
/// turns the sentinel into `None` (`qd_plain_folder`) is therefore read, not
/// run; the QUOTED half of the same line is run, by the bare-repository test
/// above.
#[test]
fn a_missing_git_is_put_into_words_and_any_other_failure_is_quoted() {
    const OPENS: &str = "orrerix could not tell whether this quick run's folder (/tmp/folder) is a \
                         git repository, so it opened nothing there: ";
    let missing = quick_folder_unknown_refusal("/tmp/folder", None);
    assert!(missing.starts_with(OPENS), "{missing}");
    assert!(missing.contains("git is not installed, or is not on the PATH"), "{missing}");
    assert!(missing.contains("even in a plain folder"), "why a plain folder still needs it: {missing}");
    assert!(!missing.contains("git-not-found"), "the sentinel reached a reader: {missing}");
    assert!(is_one_paragraph(&missing), "{missing:?}");

    let said = "fatal: this operation must be run in a work tree";
    let other = quick_folder_unknown_refusal("/tmp/folder", Some(said));
    assert_eq!(other, format!("{OPENS}{said}"), "git's own words, and nothing put in their place");
    assert!(!other.contains("is not installed"), "{other}");
}

// ── which note a pane is opened with ────────────────────────────────────────

/// **A plain-folder helper is opened with the plain folder's note**, and the
/// note comes with the workspace it describes.
///
/// A test process has no pane to read a kickoff off, so the choice is asked of
/// the function the spawn takes it from, with real groups. The half a test CAN
/// see of that same answer — the folder — is checked against a real spawn, so
/// the pair is not a second opinion beside what the spawn did.
#[test]
fn a_plain_folder_helper_is_given_the_plain_folders_note_with_its_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let folder = Repo::plain();
    let (group, root) = describing(&reg, &folder);

    let mut notes = Vec::new();
    for role in [Role::Worker, Role::Reviewer] {
        let (cwd, note) = reg
            .qd_plain_workspace(&group, role)
            .expect("git answers for a plain folder")
            .unwrap_or_else(|| panic!("a {} in a plain folder opens in place", role.as_str()));
        assert_eq!(cwd.replace('\\', "/"), folder.path());
        assert_eq!(note, quick_plain_folder_note(role, &cwd), "the note is this role's, for this folder");
        assert!(!note.contains("dedicated git worktree"), "{note}");
        notes.push(note);
    }
    assert_ne!(notes[0], notes[1], "a worker and a reviewer are not told the same thing");
    // Only the two classes that would otherwise be cut a worktree.
    for role in [Role::Planner, Role::Quick] {
        assert_eq!(reg.qd_plain_workspace(&group, role), Ok(None), "{}", role.as_str());
    }
    // The spawn takes the folder from the same answer the note came in.
    let worker = open_helper(&reg, &group, &root, "worker");
    assert_eq!(cwd_of(&reg, &worker), folder.path());

    // The controls. In a repository there is no in-place workspace and so no
    // such note; an ordinary group in the same plain folder has none either;
    // and a folder git will not open is an error, not an absent answer.
    let repo = Repo::new();
    let (in_repo, _root) = describing(&reg, &repo);
    let ordinary = reg
        .create_group(&folder.path(), Guardrails { agent_cli: "claude".into(), ..Guardrails::default() })
        .unwrap()
        .id;
    let bare = Repo::bare();
    let (in_bare, _root) = describing(&reg, &bare);
    for role in [Role::Worker, Role::Reviewer] {
        assert_eq!(reg.qd_plain_workspace(&in_repo, role), Ok(None), "a repository: {}", role.as_str());
        assert_eq!(reg.qd_plain_workspace(&ordinary, role), Ok(None), "not a quick run: {}", role.as_str());
        let err = reg.qd_plain_workspace(&in_bare, role).expect_err("a bare repository is not a plain folder");
        assert!(err.contains("could not tell whether this quick run's folder"), "{err}");
    }
}

/// **A quick root's workspace line follows the folder too**: in a plain
/// folder it says its helpers share that folder, and everywhere else — a
/// repository, and a folder git cannot answer for — it is the line it always
/// was.
#[test]
fn a_quick_roots_workspace_note_follows_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let folder = Repo::plain();
    let repo = Repo::new();
    let bare = Repo::bare();
    let (in_folder, _root) = describing(&reg, &folder);
    let (in_repo, _root) = describing(&reg, &repo);
    let (in_bare, _root) = describing(&reg, &bare);

    assert_eq!(reg.qd_root_workspace_note(&in_folder), QUICK_ROOT_PLAIN_FOLDER_NOTE);
    assert_eq!(reg.qd_root_workspace_note(&in_repo), QUICK_ROOT_WORKSPACE_NOTE);
    assert_eq!(
        reg.qd_root_workspace_note(&in_bare),
        QUICK_ROOT_WORKSPACE_NOTE,
        "a root opens whatever git says, with the repository's line"
    );
    assert_ne!(QUICK_ROOT_PLAIN_FOLDER_NOTE, QUICK_ROOT_WORKSPACE_NOTE, "the two are different lines");
}

// ── a folder that changes mid-run ───────────────────────────────────────────

/// **A folder made a repository halfway through does not reword work that is
/// already in place.** The human runs `git init` and commits after the worker
/// opened: the worker still has no worktree and no branch, so its brief (typed
/// again on a resume) and its reviewer's must not start naming them.
///
/// The control is that the folder really did change: the question a NEW pane
/// would be asked now answers "a repository". Before the briefs were keyed on
/// the run's record they asked that same question, and this run's reviewer
/// was told its folder was "the worker's own worktree" and to `git diff`
/// against a branch the work was never on.
#[test]
fn a_folder_made_a_repository_mid_run_does_not_reword_work_already_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let folder = Repo::plain();
    let (group, worker) = working(&reg, &folder);
    assert_eq!(cwd_of(&reg, &worker), folder.path(), "the worker was opened in place");
    assert!(reg.qd_plain_workspace(&group, Role::Worker).unwrap().is_some(), "and the folder is still plain");

    folder.make_repository();
    assert_eq!(
        reg.qd_plain_workspace(&group, Role::Worker),
        Ok(None),
        "the control: the next pane's question now answers a repository"
    );

    // The work brief, as a resume would type it again.
    let work = lf(&reg.qd_brief_for_test(&group));
    assert!(work.contains("Work in the folder you were opened in"), "{work}");
    assert!(work.contains("You were given no worktree and no branch"), "{work}");
    for wrong in ["Work in your worktree", "commit as you go", "the branch you were given"] {
        assert!(!work.contains(wrong), "the brief began telling an in-place worker to {wrong:?}: {work}");
    }

    report(&reg, &worker, json!({ "outcome": "done", "note": "changed list.rs" }));
    let out = step(&reg, &group, T0 + 2);
    let (side, reviewer, _how) = out.handed_to.clone().expect("the reviewer is opened");
    assert_eq!(side, "reviewer");
    assert_eq!(cwd_of(&reg, &reviewer), folder.path(), "where the work is");
    let review = lf(&reg.agent(&reviewer).unwrap().task);
    assert!(review.contains("The worker was given no worktree and no branch there"), "{review}");
    for wrong in ["on branch", "the worker's own worktree", "git status", "git diff", "git log"] {
        assert!(!review.contains(wrong), "the brief named {wrong:?} for work done in place: {review}");
    }
}

// ── what the agents are told ────────────────────────────────────────────────

/// **The words a plain-folder pane is opened with**, pinned for their shape
/// and for the two claims each must make: where the pane is, and that the
/// repository half of its role instructions does not apply there.
#[test]
fn the_plain_folder_notes_say_where_the_pane_is_and_that_nothing_is_committed() {
    let worker = quick_plain_folder_note(Role::Worker, "/tmp/folder");
    let reviewer = quick_plain_folder_note(Role::Reviewer, "/tmp/folder");
    for (who, note) in [("worker", &worker), ("reviewer", &reviewer)] {
        assert!(note.contains("the folder /tmp/folder itself"), "{who}: {note}");
        assert!(note.contains("It is not a git repository"), "{who}: {note}");
        assert!(note.contains("this is what holds here"), "{who}: {note}");
        assert!(!note.contains("dedicated git worktree"), "{who}: {note}");
        assert!(is_one_paragraph(note), "{who}: {note:?}");
    }
    assert_ne!(worker, reviewer, "a reviewer is told to read, a worker to change");
    assert!(worker.contains("change the files in place"), "{worker}");
    assert!(reviewer.contains("do not edit them"), "{reviewer}");

    assert!(is_one_paragraph(QUICK_ROOT_PLAIN_FOLDER_NOTE));
    assert!(QUICK_ROOT_PLAIN_FOLDER_NOTE.contains("it is not a git repository"));
    assert!(!QUICK_ROOT_PLAIN_FOLDER_NOTE.contains("worktree of its own"));
    assert!(QUICK_ROOT_WORKSPACE_NOTE.contains("worktree of its own"), "the repository's note is unchanged");

    // The disclosure names exactly what was passed.
    let none = quick_plain_folder_disclosure("w-1", "/tmp/folder", &[]);
    assert!(none.starts_with("w-1 opened in the folder itself (/tmp/folder)"), "{none}");
    assert!(!none.contains("ignored"), "{none}");
    let one = quick_plain_folder_disclosure("w-1", "/tmp/folder", &["branch"]);
    assert!(one.ends_with("The `branch` you passed was ignored: there is nothing to cut a branch from."), "{one}");
    for text in [&none, &one] {
        assert!(is_one_paragraph(text), "{text:?}");
    }
}

/// **The root's instructions carry both rules the issue asks for**, in the
/// template and in its lockstep twin: what a plain folder changes, and that a
/// helper is an orrerix pane and never the CLI's own subagent.
#[test]
fn the_roots_instructions_cover_a_plain_folder_and_forbid_cli_subagents() {
    let tpl = loomux_lib::orchestration::QUICK_TPL;
    let core = loomux_lib::orchestration::mechanics_core(Role::Quick, None);
    assert!(tpl.contains("## When the folder is not a git repository"), "{tpl}");
    assert!(tpl.contains("**Every helper works in that one folder.**"), "{tpl}");
    assert!(tpl.contains("**`branch` and `base` mean nothing there.**"), "{tpl}");
    // The rule sits under the hard rules, not in a preference section.
    let never = tpl.find("## What you never do").expect("the hard rules");
    let limits = tpl.find("## Your limits").expect("the section after them");
    let rule = tpl.find("**Never use your CLI's own subagents for a helper's work.**").expect("the rule");
    assert!(never < rule && rule < limits, "the subagent rule is one of the hard rules");
    for (which, text) in [("quick.md", tpl), ("mechanics_core", core.as_str())] {
        assert!(text.contains("Never use your CLI's own subagents"), "{which}");
        assert!(text.contains("not a git repository"), "{which}");
    }
    // And the tool the root reads before it reads anything else says it too.
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let repo = Repo::new();
    let (_group, root) = describing(&reg, &repo);
    let listing = loomux_lib::orchestration::mcp::dispatch(
        &reg,
        &reg.resolve_token(&reg.agent(&root).unwrap().token).unwrap(),
        "tools/list",
        &Value::Null,
    )
    .expect("the root lists its tools");
    let spawn = listing["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!("spawn_agent"))
        .expect("spawn_agent is on its surface");
    let said = spawn["description"].as_str().unwrap_or_default();
    assert!(said.contains("NOT a git repository"), "{said}");
    assert!(said.contains("`branch` and `base` are ignored"), "{said}");
    // …and so does every other line of its surface that names a branch or a
    // worktree: the two arguments, `report`'s note, and `fork_session`.
    let tool = |name: &str| -> Value {
        listing["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == json!(name))
            .unwrap_or_else(|| panic!("{name} is on its surface"))
            .clone()
    };
    for arg in ["branch", "base"] {
        let said = spawn["inputSchema"]["properties"][arg]["description"].as_str().unwrap_or_default();
        assert!(said.contains("Ignored where the run's folder is not a git repository"), "{arg}: {said}");
    }
    let report = tool("report");
    let said = report["description"].as_str().unwrap_or_default();
    assert!(
        said.contains("or the files that changed, for work a helper did in a folder that is not a git repository"),
        "{said}"
    );
    let fork = tool("fork_session");
    let said = fork["description"].as_str().unwrap_or_default();
    assert!(said.contains("a quick run whose folder is not a git repository"), "{said}");
    assert!(said.contains("the fork opens in that folder beside its source"), "{said}");
}

// ── the resume message of a described run ───────────────────────────────────

/// The two fragments of the resume message that name a branch in a
/// repository, and what stands in their place where a helper works in the
/// folder itself.
const RESUME_HELPERS_REPO: &str = "Branch helpers from: main.";
const RESUME_WORK_REPO: &str = "the branch, and the pull request if one was opened";
const RESUME_HELPERS_PLAIN: &str = "This folder is not a git repository: helpers open in the folder itself, with no worktree and no branch, so have one worker changing it at a time.";
const RESUME_WORK_IN_PLACE: &str = "the files that changed, for work a helper did in the folder itself, or the branch and any pull request for a helper that was given one";

/// **A described run resumed in a plain folder is not asked for a branch.**
/// Its resume message is the repository's, byte for byte, with exactly the two
/// branch fragments exchanged — so it is pinned as a golden without a second
/// copy of the whole text to keep in step.
///
/// Before this, the message told the agent "there is no branch" in one bullet
/// and then to report "the branch, and the pull request if one was opened",
/// and no test rendered it in a plain folder at all.
#[test]
fn a_resumed_described_run_in_a_plain_folder_is_not_asked_for_a_branch() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let folder = Repo::plain();
    let (group, root, _worker) = tasked(&reg, &folder);

    // The control on the golden's construction: the repository's text really
    // does carry both fragments, once each, so exchanging them changes it.
    for fragment in [RESUME_HELPERS_REPO, RESUME_WORK_REPO] {
        assert_eq!(ROOT_BODY.matches(fragment).count(), 1, "{fragment:?} in the repository's message");
    }
    let expected = ROOT_BODY
        .replace(RESUME_HELPERS_REPO, RESUME_HELPERS_PLAIN)
        .replace(RESUME_WORK_REPO, RESUME_WORK_IN_PLACE);
    let message = lf(&reg.qd_brief_for_test(&group));
    assert_eq!(message, expected, "the plain-folder resume message moved");
    assert!(!message.contains(RESUME_WORK_REPO), "it asks for a branch there is not: {message}");
    assert!(!message.contains("Branch helpers from"), "{message}");
    assert!(!message.contains("{{"), "a placeholder survived: {message}");

    // And as typed: the hold, the resume, and the message in the root's pane.
    make_deliverable(&reg, &group, &root, 7841);
    report(&reg, &root, json!({ "outcome": "blocked", "note": "which of the two files?" }));
    step(&reg, &group, T0 + 5);
    reg.quick_resume_at(&group, T0 + 6).expect("the run resumes");
    let typed = texts_to(&reg, &group, &root);
    let last = typed.last().map(|t| lf(t)).unwrap_or_default();
    assert!(last.contains("the human resumed this quick run"), "{typed:?}");
    assert!(last.ends_with(&expected), "the pane was typed the plain-folder message: {last}");
}

/// **The closing line follows what the helpers were GIVEN, not what the folder
/// is now.** The human makes the folder a repository halfway through a task:
/// the next helper will be cut a branch, and the message says so — but the
/// worker already open did its work in the folder, has no branch, and is
/// still asked for by its files. That is read off the roster.
///
/// The control is the first assertion: the folder really did change, so the
/// "next helper" bullet now names a branch.
#[test]
fn a_resume_after_the_folder_became_a_repository_still_asks_for_in_place_work_by_its_files() {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let folder = Repo::plain();
    let (group, _root, worker) = tasked(&reg, &folder);
    assert_eq!(cwd_of(&reg, &worker), folder.path(), "the worker was opened in place");

    folder.make_repository();
    let message = lf(&reg.qd_brief_for_test(&group));
    assert!(message.contains(RESUME_HELPERS_REPO), "the next helper is cut a branch: {message}");
    assert!(message.contains(RESUME_WORK_IN_PLACE), "the work already done in place is named by its files: {message}");
    assert!(!message.contains(RESUME_WORK_REPO), "{message}");
}
