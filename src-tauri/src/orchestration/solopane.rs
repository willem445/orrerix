//! Standalone and lead panes' free pieces: the solo group id, the lead marker,
//! the human pane entry and the lead's MCP arguments.
//! Design note: `docs/design/lead-pane.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. Sibling files it
//! calls: `agentmodel.rs`, `clis/mod.rs`, `persona.rs`.

use super::*;

/// Reserved, backend-minted pseudo-group id for standalone (non-orchestration)
/// panes given a channel-scoped MCP identity (#271 W3 addendum, part A).
/// Never produced by `group_id_for_repo` (which always emits `{slug}-{8hex}`)
/// — a fixed constant, so its path-segment safety held by provenance even
/// before #904; it now parses like every other id (`solo_group_id`). Registered
/// lazily
/// (`OrchRegistry::ensure_solo_group`) the first time a solo pane is created.
pub const SOLO_GROUP: &str = "__solo__";

/// [`SOLO_GROUP`] as a validated [`GroupId`] (#904).
///
/// A `GroupId` owns a `String`, so it cannot be a `const` — hence the
/// `OnceLock`, parsed once and handed out by reference so the ~10 call sites
/// read exactly as they did when the constant was a `&str`.
///
/// The `expect` is deliberate, and is the one place in #904 that can panic. It
/// is an assertion about a **literal in this file**, not about anything a
/// caller supplies: the only way to reach it is to edit `SOLO_GROUP` itself to
/// something the alphabet refuses, and
/// `parse_accepts_every_group_id_shape_the_codebase_actually_uses` asserts
/// exactly that pairing — so the test goes red before this line can ever run.
/// The alternative, an `Option` threaded through ten call sites with ten
/// different return types, buys nothing: every one of them would be handling a
/// case that cannot happen, and the handling would be the only untested code.
pub fn solo_group_id() -> &'static GroupId {
    static SOLO: std::sync::OnceLock<GroupId> = std::sync::OnceLock::new();
    SOLO.get_or_init(|| GroupId::parse(SOLO_GROUP).expect("SOLO_GROUP must be a valid group id"))
}


/// Marker file, in the group dir, recording that a group was minted by the
/// "orrerix subagents" toggle rather than by a launcher orchestration (#2519).
///
/// It exists because the ROSTER cannot answer that question durably.
/// `read_blocks` resolves every persisted block `kind` through
/// `workflow::kind_from_str`, which has no `lead` arm by design — that absence
/// is what stops a repo's workflow file declaring one and stops a lead opening
/// a lead (`docs/design/lead-pane.md`, *Consent*) — so a `kind: "lead"` row in
/// `group.json` is DROPPED on reload rather than restored. Every reader that
/// needs the fact after a restart asks this file instead.
///
/// **It has a LIFETIME, and both ends of it are code** (rev-final B1). A group
/// id is repo-derived and handed out again: `next_group_id` returns the first
/// candidate with no LIVE agent, and the group directory is never removed. So a
/// marker that is only ever written outlives the group that wrote it, and the
/// next ordinary orchestration to reattach to that id answers `is_lead_group()`
/// forever — every session in it refused as unresumable, citing a toggle nobody
/// flipped, with `offersStartFresh` false for that kind so the UI offers no way
/// out. It is written by `lead_prepare` (last, below every step that can fail)
/// and CLEARED by `create_group_ex`, which is the one place an id is claimed.
///
/// That pairing is the `paused` marker's precedent taken WHOLE: `end_group`
/// removes that one so "a future relaunch on this repo starts clean rather than
/// silently paused", and `invalidate_usage_memo` beside it states the general
/// form — "a group id is chosen by liveness and can be handed out again".
/// Clearing at the CLAIM rather than at teardown is the one difference, and it
/// is required: a lead that simply dies never runs `end_group` at all.
///
/// Best-effort at both ends, and each failure leaves the pre-existing answer
/// standing rather than inventing one: a write that fails loses the resume
/// refusal, and a remove that fails leaves a refusal that was already there.
pub const LEAD_MARKER: &str = "lead";

/// The `AgentEntry` for a pane the HUMAN's own launcher opens — a solo pane or
/// a lead — where orrerix mints the identity but never builds the command line.
///
/// Extracted rather than copied (#2519): this struct has around sixty fields
/// and four construction sites, and the two that share every one of these
/// defaults are exactly the two human-launched ones. A fifth hand-written copy
/// is how a field added next month gets the wrong value in one site and nothing
/// goes red to say so.
///
/// Every field below is the value BOTH sites already had, unchanged. What the
/// two genuinely differ on is the argument list: a solo pane carries its CLI
/// directly (`solo_cli`, because `__solo__` is one pseudo-group across panes
/// running different CLIs), while a lead's comes from its block like any other
/// group member's — see `AgentEntry::solo_cli`.
///
/// `spawn_agent_ex`'s and `register_orchestrator_pane`'s literals are
/// deliberately NOT folded in here: those two carry a session id, a task, a
/// branch, a board binding and a resolved contract carrier, so they would each
/// need a post-construction fixup for most of what they set. `solo_adopt`'s is
/// the one remaining near-twin, and it differs on the two fields this shape
/// makes load-bearing (`status`/`pty_id`: it adopts an already-running pane);
/// folding it in would mean a builder rather than a function, which is more
/// machinery than three call sites earn.
#[allow(clippy::too_many_arguments)] // the fields the two sites genuinely differ on
pub(in crate::orchestration) fn human_pane_entry(
    id: &str,
    group: GroupId,
    name: String,
    block: &str,
    role: Role,
    token: &str,
    cwd: &str,
    solo_cli: Option<String>,
) -> AgentEntry {
    AgentEntry {
        id: id.to_string(),
        group,
        name,
        name_source: NameSource::Default,
        block: block.to_string(),
        role,
        token: token.to_string(),
        status: AgentStatus::Starting,
        pty_id: None,
        pane_id: None,
        pane_kind: None,
        forked_from: None,
        task: String::new(),
        task_id: None, // a human-launched pane has no board binding
        session_id: None,
        cwd: cwd.to_string(),
        branch: None, // neither class is part of the multi-agent worktree/branch model
        idle_since_ms: None,
        started_ms: now_ms(),
        last_progress_ms: now_ms(),
        last_mcp_activity_ms: 0,
        // Neither class is idle-ticked (the tick is `Role::Orchestrator`-only);
        // inert for both.
        last_output_progress_ms: now_ms(),
        last_output_total: 0,
        watchdog_notified: false,
        watchdog_watch_suppressed: false,
        watchdog_drive_suppressed: false,
        idle_tick_notified: false,
        compact_nudge_notified: false,
        compact_nudge_last_output_total: 0,
        compact_requested: false,
        compact_pending: false,
        compact_seen_busy: false,
        compact_pending_baseline_tokens: None,
        compact_pending_baseline_marker_count: None,
        compact_pending_trusted: false,
        compact_reinject_attempted_ms: None,
        compact_reinject_attempts: 0,
        compact_reinject_busy_deferred: false,
        compact_pending_armed_ms: None,
        compact_last_lost_reason: None,
        compact_last_lost_ms: None,
        compact_last_ack: None,
        compact_last_ack_ms: None,
        last_context_tokens: None,
        last_context_model: None,
        last_context_window: None,
        last_context_window_rounded: false,
        last_context_effort: None,
        last_context_source: None,
        compact_inference_guard_until_ms: 0,
        compact_hook_precompact_seen_ms: None,
        compact_hook_sessionstart_seen_ms: None,
        compact_pending_evidence: None,
        compact_hook_native_notice_delivered: false,
        // A solo pane has no group/persona system at all, and a lead's contract
        // rides in its kickoff rather than in a system layer — so `KickoffOnly`,
        // the enum's own default, is the accurate value for both rather than
        // merely the conservative one.
        contract_carrier: ContractCarrier::default(),
        last_state_write_ms: 0,
        compact_escalation_notified: false,
        cache_idle_nudge_latched: false,
        idle_tick_skip_rearm_ms: 0,
        solo_cli,
        last_exit_tail: None,
        killed_by: None,
    }
}

/// The exact per-CLI flag string a LEAD pane's command line needs, appended by
/// the launcher to the line the human owns (#2519). Empty for a CLI with no arm
/// — `lead_prepare` treats that as a loomux bug and refuses, unlike
/// `solo_prepare`, which degrades.
///
/// **It is `solo_prepare`'s string plus a per-CLI subagent denial**, and the
/// two halves have different jobs. The first half is the MCP wiring, identical
/// to a solo pane's because the question it answers is identical. The second is
/// what makes the toggle mean what it says: a lead that can still reach its
/// harness's own in-process subagents will use them, because they are one call
/// away and `spawn_agent` is three.
///
/// # Per CLI, cited to the vendor page as fetched
///
/// - **claude — `--disallowedTools Agent`.** Two documented facts composed,
///   and the composition is stated because neither page states it alone. The
///   sub-agents page names the TOOL and the deny: "To prevent Claude from
///   delegating to any subagent, deny the `Agent` tool itself with
///   `permissions.deny`" (and, in a note: "In version 2.1.63, the Task tool was
///   renamed to Agent. Existing `Task(...)` references in settings and agent
///   definitions still work as aliases"). The CLI reference gives the flag that
///   spells a deny rule on a command line: `--disallowedTools` is "Deny rules. A
///   bare tool name removes the matching tools from Claude's context: `\"Edit\"`
///   removes Edit, `\"*\"` removes every tool". So a bare `Agent` on that flag is
///   the argv spelling of the deny the sub-agents page prescribes.
///   `CLAUDE_CODE_DISABLE_EXPLORE_PLAN_AGENTS` is narrower — the built-in
///   explore/plan agents only — and is not used here.
/// - **copilot — nothing, and the reason is the FLAG, not the tool.** The plan
///   this slice was written from recorded copilot as documenting no subagent
///   tool name at all; re-fetching says otherwise, so the premise is corrected
///   here rather than transcribed. `custom-agents-configuration` does name one
///   — the `agent` tool (aliases `custom-agent`, `Task`), "Allows a different
///   custom agent to be invoked to accomplish a task" — but it names it as a key
///   in a custom agent's own `tools:` list, which is a FILE orrerix would have to
///   hand the pane with `--agent`, replacing whatever agent the human chose.
///   The flag seam this function writes into cannot express it: the CLI
///   configuration guide says "To use the --deny-tool and --allow-tool options,
///   you must specify what type of tool you want to allow or deny" and lists
///   exactly three — shell commands, `write`, and MCP server tools (the same
///   enumeration `KNOWN_COPILOT_DENY_CATEGORIES` is pinned against). A bare tool
///   name is not among them. So a copilot lead is instruction-only:
///   `templates/lead.md` asks it to prefer `spawn_agent` and nothing
///   structurally stops it doing otherwise. Inventing a `--deny-tool agent`
///   value the vendor does not document would be a claim, not a denial; the
///   follow-up is the generated-custom-agent route, which is a product decision
///   about overriding the human's own `--agent` choice and not this slice's.
/// - **pi — nothing to deny.** `docs/usage.md` at the pinned tag: pi "intentionally
///   does not include built-in MCP, sub-agents, permission popups, plan mode,
///   to-dos, or background bash", and no delegation tool appears in its toolset
///   (`read`, `bash`, `powershell`, `edit`, `write`, `grep`, `find`, `ls`).
///
/// The absent CLIs (opencode, codex, gemini) never reach here:
/// `lead_prepare` refuses any CLI without an argv MCP seam before calling this.
pub(in crate::orchestration) fn lead_mcp_args(cli: &str, cfg: &Path) -> String {
    match cli {
        // `--disallowedTools` is APPENDED, and every join site appends, so a
        // lead launched with the launcher's autopilot flags on ends up with two
        // `--allowedTools` occurrences exactly as a solo pane does — see
        // `solo_prepare`'s arm for why that is harmless and deliberately not
        // relied on. This adds a THIRD flag rather than editing either.
        "claude" => format!(
            "--mcp-config \"{}\" --strict-mcp-config --allowedTools {tools} --disallowedTools Agent",
            cfg.display(),
            tools = brand::MCP_TOOL_PREFIX
        ),
        "copilot" => format!(
            "--additional-mcp-config \"@{}\" --allow-tool {server}",
            cfg.display(),
            server = MCP_SERVER
        ),
        "pi" => format!("--mcp-config \"{}\"", cfg.display()),
        _ => String::new(),
    }
}
