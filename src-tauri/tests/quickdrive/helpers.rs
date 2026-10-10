//! Shared fixtures: the registry, a throwaway repo, and the helpers that
//! start a run, step it, and speak as one of its panes.

use super::*;

/// The clock every test starts at.
pub(crate) const T0: u64 = 1_000_000;
pub(crate) const MIN: u64 = 60_000;

/// The task every fixture run is handed. Two lines on purpose: the task is the
/// one multi-line value a brief carries, and a fixture that could not show a
/// line break could not show one surviving.
pub(crate) const TASK: &str = "add a --json flag to the list command\nkeep the text output the default";

/// Build a registry against `dir` with every test-only directory override
/// applied — `tests/reviewdrive/helpers.rs`'s `relaunch_registry`, duplicated
/// because helpers do not cross integration-test binaries. A registry built
/// without these falls through to the REAL `~/.claude/agents` /
/// `~/.copilot/agents` on its first spawn (#464); the proof that this helper
/// applies all four is `its_registry_helper_applies_every_override_this_allowlist_row_assumes`
/// in `guards.rs`.
pub(crate) fn relaunch_registry(dir: &std::path::Path) -> OrchRegistry {
    let reg = OrchRegistry::new(dir.to_path_buf());
    reg.set_port(45999);
    reg.set_claude_agents_dir_override(dir.join("claude-agents"));
    reg.set_copilot_agents_dir_override(dir.join("copilot-agents"));
    reg.set_compact_hook_dir_override(dir.join("compacthook"));
    reg.set_copilot_hooks_dir_override(dir.join("copilot-hooks"));
    reg
}

/// A throwaway git repo one level below its own temp root: a worker's worktree
/// is cut SIBLING to the repo (`<repo>-worktrees/…`), so nesting keeps it
/// inside the root that `Drop` reclaims.
pub(crate) struct Repo {
    _root: tempfile::TempDir,
    pub(crate) repo: std::path::PathBuf,
}

impl Repo {
    pub(crate) fn new() -> Repo {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let r = Repo { _root: root, repo };
        // `-b main` so the branch the fixtures name as a base really exists,
        // whatever `init.defaultBranch` the machine running this has.
        r.git(&["init", "-q", "-b", "main"]);
        r.git(&["config", "user.email", "t@t"]);
        r.git(&["config", "user.name", "t"]);
        std::fs::write(r.repo.join("f.txt"), "hi").unwrap();
        r.git(&["add", "-A"]);
        r.git(&["commit", "-qm", "init"]);
        r
    }

    /// A folder that is NOT a git repository (#3878): the same nesting as
    /// [`Repo::new`], one file in it, and no `git init`.
    ///
    /// The premise is asserted where it is made. A machine whose temp
    /// directory sits inside somebody's work tree would otherwise run every
    /// plain-folder test against a repository and report on the wrong thing.
    pub(crate) fn plain() -> Repo {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("folder");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("f.txt"), "hi").unwrap();
        let r = Repo { _root: root, repo };
        let (ok, said) = r.git_try(&["rev-parse", "--show-toplevel"]);
        assert!(
            !ok && said.contains("not a git repository"),
            "the fixture's premise: {} must be outside every git work tree, and git said: {said}",
            r.repo.display()
        );
        r
    }

    /// A BARE repository (#3878): a folder git knows about and will not cut a
    /// worktree in, which is the failure that must never be read as "this is a
    /// plain folder". Asserted the same way: git must refuse it in its own
    /// words, and those words must not be the plain folder's.
    pub(crate) fn bare() -> Repo {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("bare");
        std::fs::create_dir_all(&repo).unwrap();
        let r = Repo { _root: root, repo };
        r.git(&["init", "-q", "--bare"]);
        let (ok, said) = r.git_try(&["rev-parse", "--show-toplevel"]);
        assert!(
            !ok && !said.contains("not a git repository"),
            "the fixture's premise: git must refuse a bare repository for a reason of its own: {said}"
        );
        r
    }

    fn git(&self, args: &[&str]) {
        let (ok, said) = self.git_try(args);
        assert!(ok, "git {args:?}: {said}");
    }

    /// Run git in the folder and answer whether it succeeded, with what it
    /// wrote to stderr.
    fn git_try(&self, args: &[&str]) -> (bool, String) {
        let out = std::process::Command::new("git")
            .current_dir(&self.repo)
            .args(args)
            .output()
            .expect("git must be installed for this test");
        (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    pub(crate) fn path(&self) -> String {
        self.repo.to_string_lossy().replace('\\', "/")
    }
}

/// The request the launcher form would send for a work + review run on
/// `repo`, with `main` named as the base so a brief's diff line is the same
/// on every machine.
pub(crate) fn request(repo: &Repo) -> QuickStartRequest {
    let step = || QuickStepConfig { cli: "claude".into(), ..QuickStepConfig::default() };
    QuickStartRequest {
        repo: repo.path(),
        task: TASK.to_string(),
        plan_step: false,
        review_step: true,
        base: "main".into(),
        plan: step(),
        work: step(),
        review: step(),
        ..QuickStartRequest::default()
    }
}

/// Start a run through the real `quick_start`, after `edit` has had its say
/// over the request. Answers the group the run was minted in.
pub(crate) fn start_with(
    reg: &OrchRegistry,
    repo: &Repo,
    edit: impl FnOnce(&mut QuickStartRequest),
) -> GroupId {
    let mut req = request(repo);
    edit(&mut req);
    let out = reg.quick_start_at(req, T0).expect("the run starts");
    GroupId::parse(out["group_id"].as_str().expect("quick_start answers the group id"))
        .expect("and it is a valid one")
}

pub(crate) fn start(reg: &OrchRegistry, repo: &Repo) -> GroupId {
    start_with(reg, repo, |_| {})
}

/// One step of the run, at `now`.
pub(crate) fn step(reg: &OrchRegistry, group: &GroupId, now: u64) -> QdDriveReport {
    reg.qd_drive_group(group, now)
}

pub(crate) fn status(reg: &OrchRegistry, group: &GroupId) -> Value {
    reg.quick_status(group)
}

pub(crate) fn state(reg: &OrchRegistry, group: &GroupId) -> String {
    status(reg, group)["state"].as_str().unwrap_or_default().to_string()
}

pub(crate) fn held_reason(reg: &OrchRegistry, group: &GroupId) -> String {
    status(reg, group)["held_reason"].as_str().unwrap_or_default().to_string()
}

/// The agent id of the pane currently recorded for `side` — empty when that
/// side has not been opened.
pub(crate) fn pane(reg: &OrchRegistry, group: &GroupId, side: QuickSide) -> String {
    status(reg, group)["panes"][side.as_str()]["agent"].as_str().unwrap_or_default().to_string()
}

fn caller(reg: &OrchRegistry, agent: &str) -> Caller {
    let a = reg.agent(agent).unwrap_or_else(|| panic!("no such agent {agent}"));
    reg.resolve_token(&a.token).expect("a spawned agent's token resolves")
}

/// Call one MCP tool as `agent`. Answers `(is_error, text)` — the tool's own
/// answer, which is what the pane that called it reads.
pub(crate) fn call(reg: &OrchRegistry, agent: &str, tool: &str, args: Value) -> (bool, String) {
    let out = dispatch(
        reg,
        &caller(reg, agent),
        "tools/call",
        &json!({ "name": tool, "arguments": args }),
    )
    .unwrap_or_else(|e| panic!("{agent} could not call {tool}: {e:?}"));
    (
        out["isError"] == json!(true),
        out["content"][0]["text"].as_str().unwrap_or_default().to_string(),
    )
}

/// `report(...)` as `agent`, which must succeed. Answers the tool's text.
pub(crate) fn report(reg: &OrchRegistry, agent: &str, args: Value) -> String {
    let (is_error, text) = call(reg, agent, "report", args);
    assert!(!is_error, "{agent}'s report was refused: {text}");
    text
}

/// Give `agent` a pane, pause its group so a mid-session delivery is really
/// admitted, and mark that pane's last delivery confirmed so the hand-back
/// ladder calls it ready.
///
/// `tests/reviewdrive/loopfixes.rs`'s `make_delivery_land` + `make_pane_ready`,
/// for their reasons: a delivery that lands alone at the front of an idle queue
/// has to spawn the drainer that pastes it, which needs a Tauri `AppHandle` a
/// test process does not have, so `deliver_prompt` withdraws the admission and
/// answers `Err`. A paused group takes the branch above that — admit, audit
/// the full `prompt` row, answer `Ok` — which is a real production state rather
/// than a mock.
pub(crate) fn make_deliverable(reg: &OrchRegistry, group: &GroupId, agent: &str, pty: u32) {
    reg.set_pty_for_test(agent, pty);
    reg.pause_group(group).expect("a live group pauses");
    reg.set_last_delivery_for_test(pty, true);
}

/// Every prompt delivered to `agent` mid-session, in order — deliveries are
/// audited as `prompt` with the text they carried and who they were for.
pub(crate) fn texts_to(reg: &OrchRegistry, group: &GroupId, agent: &str) -> Vec<String> {
    reg.audit_log(group)
        .into_iter()
        .filter(|e| e.action == "prompt" && e.detail["to"] == json!(agent))
        .filter_map(|e| e.detail["text"].as_str().map(str::to_string))
        .collect()
}

/// The brief the pane now recorded for `side` was last handed: the last text
/// typed into it mid-session, or — for a pane that has only ever been opened —
/// the task it was opened with.
///
/// Read off what was DELIVERED rather than re-rendered, because a delivery
/// clears the things that were only true of a brief still to deliver: the
/// pending notes, the forced marker, the resume preface.
pub(crate) fn delivered_brief(reg: &OrchRegistry, group: &GroupId, side: QuickSide) -> String {
    let agent = pane(reg, group, side);
    let typed = texts_to(reg, group, &agent).last().cloned();
    lf(&typed.unwrap_or_else(|| reg.agent(&agent).map(|a| a.task).unwrap_or_default()))
}

pub(crate) fn audit_details(reg: &OrchRegistry, group: &GroupId, action: &str) -> Vec<Value> {
    // (see `delivered_texts` below for the prompt rows specifically)
    reg.audit_log(group).into_iter().filter(|e| e.action == action).map(|e| e.detail).collect()
}

pub(crate) fn action_count(reg: &OrchRegistry, group: &GroupId, action: &str) -> usize {
    audit_details(reg, group, action).len()
}

/// Every prompt this group delivered to ANY pane, in order.
pub(crate) fn delivered_texts(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    audit_details(reg, group, "prompt")
        .iter()
        .filter_map(|d| d["text"].as_str().map(str::to_string))
        .collect()
}

/// The needs-you items this group has OPEN.
pub(crate) fn open_items(reg: &OrchRegistry, group: &GroupId) -> Vec<needsyou::Item> {
    reg.needs_you(group)
        .expect("the needs-you file reads")
        .into_iter()
        .filter(|i| !i.status.is_resolved())
        .collect()
}

/// Every agent in `group` that is not dead.
pub(crate) fn live_agents(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    let mut out: Vec<String> = reg
        .list_agents(group)
        .as_array()
        .expect("list_agents answers an array")
        .iter()
        .filter(|a| a["status"] != json!("dead"))
        .map(|a| a["id"].as_str().unwrap_or_default().to_string())
        .collect();
    out.sort();
    out
}

/// A user-facing sentence is ONE paragraph: no line break, and no run of
/// spaces a collapsed line-continuation leaves behind (CLAUDE.md, #1426 B2).
pub(crate) fn is_one_paragraph(s: &str) -> bool {
    !s.contains('\n') && !s.contains("          ")
}

/// Line endings normalised — `tests/reviewdrive/tick.rs`'s `lf`, for its
/// reason: the templates are `include_str!`'d, and a worktree cut before
/// `.gitattributes` pinned them LF still has them CRLF on disk.
pub(crate) fn lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// A run walked to the point where the worker has been opened and holds the
/// turn. Answers `(group, worker)`.
pub(crate) fn working(reg: &OrchRegistry, repo: &Repo) -> (GroupId, String) {
    working_with(reg, repo, |_| {})
}

pub(crate) fn working_with(
    reg: &OrchRegistry,
    repo: &Repo,
    edit: impl FnOnce(&mut QuickStartRequest),
) -> (GroupId, String) {
    let group = start_with(reg, repo, edit);
    let out = step(reg, &group, T0 + 1);
    let (side, worker, how) = out.handed_to.clone().unwrap_or_else(|| {
        panic!("the first step opens the worker: {out:?}")
    });
    assert_eq!((side.as_str(), how.as_str()), ("worker", "opened"), "{out:?}");
    (group, worker)
}

/// A run walked to the point where the worker has reported `done` and the
/// reviewer has been opened and holds the turn. Answers
/// `(group, worker, reviewer)`.
pub(crate) fn reviewing(reg: &OrchRegistry, repo: &Repo) -> (GroupId, String, String) {
    reviewing_with(reg, repo, |_| {})
}

pub(crate) fn reviewing_with(
    reg: &OrchRegistry,
    repo: &Repo,
    edit: impl FnOnce(&mut QuickStartRequest),
) -> (GroupId, String, String) {
    let (group, worker) = working_with(reg, repo, edit);
    report(reg, &worker, json!({ "outcome": "done", "note": "added the flag; tests pass" }));
    let out = step(reg, &group, T0 + 2);
    let (side, reviewer, how) = out.handed_to.clone().unwrap_or_else(|| {
        panic!("the worker's done opens the reviewer: {out:?}")
    });
    assert_eq!((side.as_str(), how.as_str()), ("reviewer", "opened"), "{out:?}");
    (group, worker, reviewer)
}

/// The names `tools/list` shows `agent` — its listed surface, as its own CLI
/// would read it.
pub(crate) fn listed_tools(reg: &OrchRegistry, agent: &str) -> Vec<String> {
    dispatch(reg, &caller(reg, agent), "tools/list", &Value::Null)
        .unwrap_or_else(|e| panic!("{agent} could not list its tools: {e:?}"))["tools"]
        .as_array()
        .expect("tools/list answers an array")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect()
}
