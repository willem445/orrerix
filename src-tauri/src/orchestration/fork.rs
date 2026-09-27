//! Session fork: the fork launch line and the forked agent's kickoff prompt.
//! Design note: `docs/design/session-fork.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. Sibling files it
//! calls: `kickoff.rs`.

use super::*;

/// A fork request the line builders may act on (#3318 F2): the PARENT session
/// and the seam that says how this CLI spells a fork of it.
///
/// Only [`fork_line`] makes one, and it makes one only for a CLI whose seam is
/// not [`ForkSeam::None`] — so a builder arm holding a `ForkLine` never has to
/// ask whether its CLI can fork, only how.
#[derive(Clone, Copy, Debug)]
pub(in crate::orchestration) struct ForkLine<'a> {
    pub(in crate::orchestration) parent: &'a str,
    pub(in crate::orchestration) seam: ForkSeam,
}

/// Resolve a builder's `fork_of` against the capability table, REFUSING a CLI
/// that cannot fork (#3331 item 2).
///
/// F1's builders dropped `fork_of` silently for such a CLI — a line builder
/// had no way to refuse, and F1's only caller (the Solo pane menu) asked
/// `fork_refusal` before ever getting here. F2's `fork_session` tool is the
/// first caller that could hand a copilot or gemini source to the builder, and
/// a silent drop there builds a FRESH line (copilot's `--resume=` needs
/// `resume`, which a fork does not pass) and reports it as a fork. So the
/// builder refuses in the row's own words, which is also the sentence
/// `fork_refusal` gives a gesture: one predicate, asked by both.
pub(in crate::orchestration) fn fork_line<'a>(cli: &str, fork_of: Option<&'a str>) -> Result<Option<ForkLine<'a>>, String> {
    let Some(parent) = fork_of else { return Ok(None) };
    if let Some(refusal) = fork_refusal(cli) {
        return Err(refusal);
    }
    // `fork_refusal` answered `None`, which it does only for a known row whose
    // seam carries a spelling — so the row is there. Reported rather than
    // unwrapped all the same: this runs on a spawn path, and constraint 10
    // makes a panic here a process abort, not an error.
    let seam = cli_caps(cli)
        .map(|c| c.fork)
        .ok_or_else(|| format!("loomux cannot fork a {cli} session: no capability record"))?;
    Ok(Some(ForkLine { parent, seam }))
}

/// The source of a delegate fork, as the spawn path needs it (#3318 F2).
///
/// Built only by [`OrchRegistry::fork_agent`], after every refusal it owns has
/// passed — so holding one means "this parent may be forked", and the spawn
/// path reads it at exactly four places (see `spawn_agent_full`).
#[derive(Clone, Debug)]
#[doc(hidden)] // pub for integration tests (`fork_kickoff_prompt`)
pub struct ForkSpawn {
    pub parent_agent: String,
    pub parent_name: String,
    /// The session being forked — already validated by `sanitize_session`,
    /// because it reaches a launch line.
    pub parent_session: String,
    /// Who asked: the calling agent's id, or `"human"` for the pane menu.
    /// Recorded on the `agent-fork` audit row and read nowhere else.
    pub requested_by: String,
}

/// The workspace note a reviewer's kickoff (or fork turn) carries (#359), naming
/// where its worktree was ACTUALLY cut from.
///
/// It used to say "cut fresh from the default branch" unconditionally, which was
/// true of every reviewer spawn that named no `base` — and false the moment
/// `base` was passed: an orchestrator's `spawn_agent(kind: "reviewer", base:)`,
/// and every reviewer FORK (#3318 F2), which `fork_agent` cuts from its source's
/// branch. A reviewer told the wrong origin reasons about the wrong tree, so the
/// origin is read off the same `base` the worktree was cut from.
#[doc(hidden)] // pub for integration tests
pub fn reviewer_worktree_note(wt: &str, branch_name: &str, base: Option<&str>) -> String {
    let origin = match base {
        Some(b) => format!("branch '{b}'"),
        None => "the default branch".to_string(),
    };
    format!(
        "Your working directory is a dedicated git worktree at {wt}, cut fresh from {origin} — \
         its own branch '{branch_name}' is just scratch space, never the PR's own branch (which \
         may already be checked out in the worker's worktree). You review; you do not create \
         branches or push. To inspect the PR's actual code locally (e.g. to run tests), `gh pr \
         checkout <n> --detach` — never a bare `gh pr checkout <n>`, which grabs the PR branch by \
         NAME and collides with any other worktree (the worker's, or another reviewer's) that \
         already has it checked out."
    )
}

/// The refusal for a fork of a pane on a STRUCTURED driver (#2850). Shared by
/// `fork_agent` (which says it first) and `spawn_agent_full` (the backstop), so
/// the two cannot word the one fact differently.
pub(in crate::orchestration) fn fork_structured_refusal(block: &str) -> String {
    format!(
        "block {block} runs on the structured driver, whose launch spec has no fork yet — a fork \
         there would start a FRESH session and call it a fork, so orrerix refuses it (#3318 F2). \
         Spawn a fresh agent and brief it instead."
    )
}

/// The one turn a forked delegate is handed instead of a kickoff (#3318 F2).
///
/// A fork already holds its role, its instructions and every turn its parent
/// had — re-sending the full kickoff would re-brief a conversation mid-stream.
/// What it does NOT hold is the three things that changed at the fork, and this
/// names exactly those: **who it is now** (a new agent id, which is who every
/// MCP call it makes is attributed to — the parent's id in its own history is
/// not its own any more), **where it works** (the branch note), and **what to
/// do** (the task, or "you have none — say so and wait"). It carries its own
/// delivery id, and that id is NEW: the parent's delivery id sits in the
/// copied history already acted on, so a fork re-reading it must not mistake
/// its own brief for a duplicate of its parent's.
///
/// Pure, and `pub` for the integration tests that pin those properties.
#[doc(hidden)]
pub fn fork_kickoff_prompt(
    group_id: &GroupId,
    agent_id: &str,
    name: &str,
    fork: &ForkSpawn,
    instructions: &Path,
    branch_note: &str,
    task: &str,
) -> String {
    let task = task.trim();
    let todo = if task.is_empty() {
        format!(
            "You have no task yet. Do not continue {parent}'s work on your own initiative — tell \
             your orchestrator (or your lead's human) that you are an idle fork of {parent} and \
             ready for a brief, then wait.",
            parent = fork.parent_agent,
        )
    } else {
        format!("Your task:\n{task}")
    };
    let branch = if branch_note.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n{}", branch_note.trim())
    };
    format!(
        "[orrerix] You are a FORK. Your conversation so far is a copy of {parent_name}'s \
         ({parent}) session {session}, taken just now; {parent} keeps running exactly as it was, \
         and its task, its branch and its PR are still its own — do not act on them unless your \
         task below says to.\n\n\
         You are now agent {agent_id} (\"{name}\") in group {group_id}. Every orrerix tool call \
         you make from here on is attributed to {agent_id}, not to {parent}; anything in your \
         history addressed to {parent} was addressed to your parent. Your role instructions are \
         unchanged: {instructions}{branch}\n\n\
         {delivery}\n\n\
         {todo}",
        parent_name = fork.parent_name,
        parent = fork.parent_agent,
        session = fork.parent_session,
        instructions = instructions.display(),
        delivery = kickoff_delivery_note(group_id, agent_id),
    )
}
