//! The **quick root's** MCP surface (#3679) — what the one agent a described
//! quick run is given to may call, listed and re-checked.
//!
//! Design note: `docs/design/quick-orchestration.md`. This is a child of
//! `mcp.rs` so that the surface's list, its refusal sentences and its two tool
//! definitions live in one small file a reader can hold, while `mcp.rs` — at
//! its line budget already — carries only the four call sites.
//!
//! # The surface is an enumeration, twice
//!
//! `Role::Quick` is a capability class of its own (`model.rs`), and what it may
//! call is not "an orchestrator minus some tools": most arms of `call_tool`
//! have no role check of their own, so a class that is merely *not refused*
//! reaches every one of them — the board reads, `list_verdicts`,
//! `list_needs_you`, the notify, lock and channel tools — none of which a quick
//! run has anything behind. So the surface is a POSITIVE list, and it is
//! spelled twice on purpose: [`tool_defs`] builds what `tools/list` shows, and
//! [`gate`] is what `call_tool` asks before it dispatches anything.
//! `the_gate_and_the_listing_agree_for_a_quick_root` asserts the two are the
//! same set, in both directions, so they cannot drift — which one shared
//! constant would not buy, since one edit would then move both halves of a
//! double gate at once. This is `Role::Manager`'s and `Role::Lead`'s pattern,
//! unchanged.
//!
//! # What is on it, and what is deliberately not
//!
//! Twelve tools. Three from the shared tier (`list_agents`, `request_compact`,
//! `note_directive`); the delegate-driving seven (`spawn_agent`,
//! `fork_session`, `send_prompt`, `get_output`, `kill_agent`, `focus_agent`,
//! `rename_agent`); `group_usage`; and `report`.
//!
//! `report` is the one a lead does not hold, and it is why this is not a lead:
//! the quick root's `report` is intercepted by the run (`qd_owner`) as the
//! run's END. Everything that could land, publish or decide on the human's
//! behalf is absent: the task board, the merge queue, `review_verdict`,
//! `post_issue_comment`, `ask_human`, `request_attention`, the notify, lock,
//! mailbox, state and channel tools, the to-do list. So is
//! `message_orchestrator` — the root IS the root; there is nobody above it,
//! and a root that called it would park its own run.

use serde_json::{json, Value};

use super::{fleet_control_tool_defs, fork_session_tool, group_usage_tool, tool, Role};

/// The shared-tier tools a quick root keeps. Every other shared tool is a read
/// of something a quick run does not have.
const SHARED: &[&str] = &["list_agents", "request_compact", "note_directive"];

/// What `tools/list` shows a quick root: the shared tier cut down to
/// [`SHARED`], then the tools this class adds.
pub(super) fn tool_defs(mut shared: Vec<Value>) -> Vec<Value> {
    shared.retain(|t| SHARED.contains(&t["name"].as_str().unwrap_or_default()));
    shared.push(spawn_agent_tool());
    shared.push(fork_session_tool());
    shared.extend(fleet_control_tool_defs());
    shared.push(group_usage_tool());
    shared.push(report_tool());
    shared
}

/// The dispatch gate: `Ok` for a tool on the surface, the refusal otherwise.
///
/// The same twelve names [`tool_defs`] builds, spelled again rather than
/// derived from it — see the module doc for why the duplication is the point.
pub(super) fn gate(name: &str) -> Result<(), String> {
    if matches!(
        name,
        "list_agents"
            | "request_compact"
            | "note_directive"
            | "spawn_agent"
            | "fork_session"
            | "send_prompt"
            | "get_output"
            | "kill_agent"
            | "focus_agent"
            | "rename_agent"
            | "group_usage"
            | "report"
    ) {
        return Ok(());
    }
    Err(format!(
        "permission denied: {name} is not on a quick run's surface — you were given one task. \
         You open helpers (spawn_agent, kind worker | reviewer | planner), drive them \
         (send_prompt / get_output / kill_agent / focus_agent / rename_agent / fork_session / \
         list_agents), read group_usage, and end the run with report. A quick run has no task \
         board, no merge queue, no verdict and no issue comment, and nobody above you to \
         message: if you cannot go on, report(outcome=blocked) and the human is told"
    ))
}

/// **Which helpers a quick root may open**, decided on the spawn's EFFECTIVE
/// class — the named block's kind where a block is named, else `kind`.
///
/// The three delegate classes and nothing else. `effective` is `None` for a
/// spawn that resolves to no class at all, and that is refused too: a rule
/// that only named what it forbids would let through whatever it failed to
/// name.
///
/// **A second quick root gets no arm here**, and that absence is the
/// no-nesting rule rather than a gap in it: `kind: "quick"` is not in
/// `workflow::kind_from_str`'s vocabulary, so it is refused as an unknown kind
/// before this runs. Naming the root's own BLOCK is the one spelling that
/// reaches this function with `Some(Role::Quick)`, and it is refused here like
/// every other class that is not one of the three.
///
/// `cwd` and `task_id` are refused for a quick caller as well. A helper's
/// workspace is orrerix's to choose — a worker and a reviewer each in a
/// worktree of their own, a planner in the repository. A fresh worker's or
/// reviewer's `cwd` is already refused for EVERY caller by the
/// dedicated-workspace guardrail (#338/#359), which runs before this; what
/// this adds is the two cases that guardrail leaves to an orchestrator's
/// judgment — a planner's `cwd`, and a resume's — so that no spawn a root
/// makes chooses its own directory. `task_id` attaches a pane to a board row,
/// and there is no board.
pub(super) fn spawn_rule(effective: Option<Role>, args: &Value) -> Result<(), String> {
    if !matches!(effective, Some(Role::Worker | Role::Reviewer | Role::Planner)) {
        return Err(format!(
            "kind must be worker, reviewer or planner — a quick run's agent opens those three \
             kinds of helper and nothing else. It cannot open another agent like itself, an \
             orchestrator or a manager. (This spawn resolves to {}.)",
            effective
                .map(|k| format!("kind {:?}", k.as_str()))
                .unwrap_or_else(|| "no capability class at all".to_string())
        ));
    }
    let given = |key: &str| args.get(key).and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty());
    if given("cwd") {
        return Err("a quick run's helpers are placed by orrerix — a worker and a reviewer each \
                    in a worktree of their own, a planner in the repository — so cwd is not \
                    yours to set. Drop it."
            .to_string());
    }
    if given("task_id") {
        return Err("a quick run has no task board, so there is no task to attach this pane \
                    to. Drop task_id."
            .to_string());
    }
    Ok(())
}

/// `spawn_agent` as a quick root sees it: three kinds, and how helpers reach
/// each other's work.
fn spawn_agent_tool() -> Value {
    tool("spawn_agent",
        "Open a helper agent as a real orrerix pane in your group: a WORKER (does the task, in a \
         git worktree and branch of its own), a REVIEWER (reads the work and may not edit it) or \
         a PLANNER (reads the repository and may change nothing). Those three kinds and no other. \
         \
         Give it a full brief in `task` — it starts cold and knows only what you write there. Its \
         report comes back to YOU: when it calls report(done|blocked) the notice is typed into \
         this pane, and a planner's plan or a reviewer's findings arrive in that report. \
         \
         A helper's worktree shares this repository, so a reviewer can read a worker's work as \
         soon as the worker has COMMITTED it on its branch — no push and no pull request is \
         needed. Tell the reviewer the branch name. \
         \
         Guardrails apply: the live-agent cap and spawn-rate limit the human set for this run. \
         To send a helper back to work, use send_prompt on the pane it already has rather than \
         opening another.",
        json!({
            "name": { "type": "string", "description": "Short display name for the pane" },
            "kind": { "type": "string", "enum": ["worker", "reviewer", "planner"], "description": "Capability class. One of the three; anything else is refused with the reason. REQUIRED." },
            "task": { "type": "string", "description": "Full task brief; empty = an idle pane awaiting send_prompt." },
            "branch": { "type": "string", "description": "Branch name for a worker's worktree (default agent/<id>)" },
            "base": { "type": "string", "description": "Start-point for the worktree branch (default: the repo's default branch, fetched fresh from origin)." },
        }),
        &["task", "kind"])
}

/// `report` as a quick root sees it: the run's end, not a message to anyone.
fn report_tool() -> Value {
    tool("report",
        "END THE RUN. You are the one agent this quick task was given to, so your report is not \
         typed into any pane: orrerix reads it as the end of the run and tells the human. \
         outcome=done when the task is finished — put in `note` where the work is (the branch, \
         and the pull request if one was opened) and anything left open. outcome=blocked when \
         you cannot go on — put in `note` the one thing the human has to decide or fix; the run \
         is then held and they can resume it. Report once. A report(progress) moves nothing. \
         After a done report your helpers' panes and yours stay open for the human to read.",
        json!({
            "outcome": { "type": "string", "enum": ["done", "blocked"], "description": "done ends the run; blocked holds it for the human." },
            "note": { "type": "string", "description": "Where the work is and what is left open (done), or the one blocking fact (blocked). Hard-capped at ~500 chars." },
            "ref": { "type": "string", "description": "The pull request, if a helper opened one, e.g. \"#123\"." },
        }),
        &["outcome"])
}
