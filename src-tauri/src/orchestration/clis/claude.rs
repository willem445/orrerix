//! Claude Code's adapter: its allow/deny tool lists, the read-only and
//! question-deny settings, the compact hook script and settings, the
//! permission mode, and the `--model` argument.
//! Design note: `docs/design/compaction-settings.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. It calls no
//! sibling file.

use super::*;

/// git + gh pre-approval appended to Claude's `--allowedTools` for an unattended
/// agent, so the branch→commit→PR flow runs without prompts. `Bash(git *)`
/// matches every git subcommand; a planner's denials carve commit/push back out.
///
/// **#610 — this must stay CONTIGUOUS with the `--allowedTools` flag.** Per the
/// official [CLI reference](https://code.claude.com/docs/en/cli-reference),
/// `--allowedTools` takes space-separated values in one occurrence (the page's
/// own example: `"Bash(git log *)" "Bash(git diff *)" "Read"`), so a value list
/// ends at the next flag. #417 inserted `--settings` between the tool prefix and
/// these patterns, which silently demoted them from allow rules to stray
/// positional arguments on every real spawn. Pinned by
/// `claude_allow_patterns_are_not_severed_from_the_allowedtools_flag`.
pub const CLAUDE_UNATTENDED_ALLOW: &str = "\"Bash(git *)\" \"Bash(gh *)\"";

/// #610: the `permissions.allow` rules loomux writes into a **read-only**
/// (`dontAsk`) Claude pane's `--settings` file — the surface `dontAsk` is
/// *documented* against, per the
/// [permission modes reference](https://code.claude.com/docs/en/permission-modes)
/// ("Allow only pre-approved tools with dontAsk mode", verified 2026-08-01):
/// "Claude runs only actions matching your `permissions.allow` rules,
/// [read-only Bash commands], and calls approved by a PreToolUse hook."
///
/// **Why a settings layer at all, when `--allowedTools` is now emitted
/// correctly?** Because argv is not the surface the mode's contract names. The
/// #610 outage was a *flag-ordering* defect and the ordering fix above is what
/// repairs it, but nothing in Claude Code's docs states that `--allowedTools`
/// values become `permissions.allow` rules — only this settings key is
/// documented to be what `dontAsk` consults. Writing the rules where the
/// contract points them costs one JSON key and makes the planner's capability
/// independent of a parse detail that has already broken once.
///
/// **Additive, not a merge loomux performs.** Per the
/// [settings reference](https://code.claude.com/docs/en/settings), permission
/// rules "merge across scopes rather than override" — so this layer never
/// displaces the user's own `.claude/settings.json` allow/deny rules, and a
/// user `deny` still beats every allow here (deny rules "apply to every tool"
/// in every mode, per the permission-modes reference). That merge statement is
/// the one doc-grounded assumption this list rests on; loomux never parses or
/// rewrites the user's settings to achieve it.
///
/// **The entries, and why each is here:**
/// - the MCP tool prefix — `report`/`message_orchestrator`; the planner's only way
///   to answer the orchestrator at all. (The documented form: per the
///   permissions reference, `mcp__<server>` "matches any tool provided by" that
///   server.)
/// - `Bash(git *)` — read-only exploration **and `git fetch`**, which needs no
///   rule of its own: it is a git subcommand, so this pattern covers it, and
///   [`CLAUDE_READONLY_DENY_GIT`] carves out only `commit`/`push` (deny beats
///   allow). `git fetch` writes nothing but local remote-tracking refs and
///   mutates no remote, which is why it is not in the deny list — it was denied
///   before #610 only because the whole allow list never reached the CLI.
/// - `Bash(gh *)` — `gh issue view` to read its brief, `gh issue comment` to
///   post its plan: the planner's entire deliverable.
/// - `WebFetch` / `WebSearch` — the decision #610 asked for, made rather than
///   deferred. A planner's job is to ground a plan, and this repo's own
///   `agent-cli-reference` skill *requires* reading a vendor's official
///   reference before designing anything that depends on a CLI's behavior.
///   `gh api` reaches GitHub and nothing else; `curl` is not in Claude's
///   built-in read-only Bash set. Without these two a planner researches from
///   recall, which the #465 design note already recorded as a real, silent loss
///   of grounding. Both tools are non-mutating, and the read-only tier's
///   guarantee is about *mutation* — they take nothing away from it.
///
///   These two are **bare grants, honestly**: the permissions reference states
///   `WebFetch(domain:*)` "matches every domain and is equivalent to a bare
///   `WebFetch` rule", so a scoped-looking spelling would be decoration, not a
///   narrowing — and loomux is a generic product (constraint #8) that cannot
///   know which domains a given repo's planner needs. The residual is stated
///   rather than hidden: an arbitrary-host fetch is both an injection surface
///   and a data-egress channel. The escape hatch is real and needs no loomux
///   feature — a `WebFetch` (or `WebFetch(domain:...)`) **deny** rule in the
///   target repo's own `.claude/settings.json` beats any allow rule here, in
///   every mode.
///
/// **`Bash(git *)` is a prefix pattern, so it positively matches `git commit`
/// and `git push` — what carves them back out, and why that is now cited
/// rather than assumed (#614 review B1).** Before this list existed, the
/// carve-out was `--disallowedTools "Bash(git commit *)" "Bash(git push *)"`
/// alone, i.e. an *argv* deny expected to beat a *settings-file* allow. That is
/// exactly the kind of cross-mechanism equivalence the paragraph above refuses
/// to assume for allows, and it would have been inconsistent to assume it for
/// denies. It is not an assumption: the
/// [permissions reference](https://code.claude.com/docs/en/permissions) settles
/// it twice, and both quotes are verbatim (fetched 2026-08-01).
///
/// First, the flags are in the same precedence domain as settings rules — its
/// "Settings precedence" section opens "Permission rules follow the same
/// settings precedence as all other Claude Code settings", lists **"Command
/// line arguments: temporary session overrides"** as level 2, and names these
/// exact flags in it: "If a tool is denied at any level, no other level can
/// allow it. For example, a managed settings deny can't be overridden by
/// `--allowedTools`, and `--disallowedTools` can add restrictions beyond what
/// managed settings define." Second, within any level deny wins outright:
/// "Rules are evaluated in order: deny, then ask, then allow. The first match
/// in that order determines the outcome, and rule specificity doesn't change
/// the order", and "deny rules from any scope are evaluated before allow
/// rules".
///
/// So `--disallowedTools` beating this list is documented, not inherited from a
/// comment written when both flags lived on argv. `readonly_settings_deny`
/// writes the same denials into this same `permissions` object anyway — belt
/// and braces, the identical stance this constant takes toward
/// `--allowedTools`, and it removes even the cross-layer question by putting
/// the allow and the deny that must beat it in one object.
///
/// `readonly_pane_settings_carry_permissions_allow` pins the list, including
/// #465's no-bare-mutation-grant invariant restated for this layer: nothing but
/// the MCP tool prefix, a scoped `Name(...)` pattern, or one of the two enumerated
/// research tools may ever appear here.
pub const CLAUDE_READONLY_SETTINGS_ALLOW: &[&str] =
    &[brand::MCP_TOOL_PREFIX, "Bash(git *)", "Bash(gh *)", "WebFetch", "WebSearch"];

/// The `permissions.deny` rules that ride the same `--settings` object as
/// [`CLAUDE_READONLY_SETTINGS_ALLOW`] (#614 review B1) — **derived from the
/// same two predicates `build_agent_command` uses for `--disallowedTools`, not
/// a third literal list**, so the two layers cannot drift and a new
/// [`Containment`] tier changes both at once or neither.
///
/// Today only a read-only pane gets a `permissions` block at all, and a
/// read-only pane satisfies both predicates — so in practice this returns both
/// lists. Expressing it as the predicates rather than as a flat concatenation
/// is the point: it states *which tier earns which denial*, which is the thing
/// a future tier would get wrong.
///
/// Returns an empty vec for a tier with nothing to deny; the caller writes no
/// `deny` key at all in that case, for the same reason it writes no empty
/// `hooks` key (see `write_hook_settings_file`).
pub(in crate::orchestration) fn readonly_settings_deny(containment: Containment) -> Vec<&'static str> {
    let mut deny: Vec<&'static str> = Vec::new();
    if containment.denies_edits() {
        deny.extend_from_slice(CLAUDE_EDIT_DENY_TOOLS);
    }
    if containment.denies_git_mutation() {
        deny.extend_from_slice(CLAUDE_READONLY_DENY_GIT);
    }
    deny
}

/// Claude Code tool names Claude Code's own permission engine recognizes, per
/// the official [Tools reference](https://code.claude.com/docs/en/tools-reference)
/// ("The tool names are the exact strings you use in permission rules"),
/// fetched as raw markdown (`curl .../tools-reference.md`) and grepped for the
/// `| \`ToolName\`` table rows, verified 2026-07-28.
///
/// This is a point-in-time SNAPSHOT, not a live probe: Claude Code exposes no
/// machine-readable tool list at runtime loomux could query instead (the CLI's
/// `--help` output, which [`crate::cliprobe`] already parses for model ids,
/// lists flags, not tool names). So what this constant catches, via
/// `claude_edit_deny_tools_are_known_claude_tools`, is a **typo or a stale
/// name accidentally left in [`CLAUDE_EDIT_DENY_TOOLS`]** — exactly the
/// #448 failure mode, where `MultiEdit` (folded into `Edit` upstream, no
/// longer a real tool) sat in the deny list matching nothing, until a human
/// read the CLI's own startup warning ("Permission deny rule \"MultiEdit\"
/// matches no known tool") by hand. It does NOT catch a *future* Claude Code
/// release silently renaming or dropping a tool out from under an unrefreshed
/// snapshot — that half needs a human to re-run the refresh below; there is
/// no offline way to make that half automatic.
///
/// Refresh procedure: `curl https://code.claude.com/docs/en/tools-reference.md
/// | grep -oE '^\| \`[A-Za-z]+\`'`, update this list and the verified-on date.
#[doc(hidden)] // pub for integration tests
pub const KNOWN_CLAUDE_TOOLS: &[&str] = &[
    "Agent", "Artifact", "AskUserQuestion", "Bash", "CronCreate", "CronDelete", "CronList",
    "Edit", "EndConversation", "EnterPlanMode", "EnterWorktree", "ExitPlanMode", "ExitWorktree",
    "Glob", "Grep", "ListMcpResourcesTool", "LSP", "Monitor", "NotebookEdit", "PowerShell",
    "PushNotification", "Read", "ReadMcpResourceTool", "RemoteTrigger", "ReportFindings",
    "ScheduleWakeup", "SendMessage", "SendUserFile", "ShareOnboardingGuide", "Skill",
    "TaskCreate", "TaskGet", "TaskList", "TaskOutput", "TaskStop", "TaskUpdate", "TodoWrite",
    "ToolSearch", "WaitForMcpServers", "WebFetch", "WebSearch", "Workflow", "Write",
];

/// The file-editing tool names a Claude agent denies via `--disallowedTools`
/// once its class is contained at all — [`Containment::denies_edits`], i.e. a
/// planner *or* a reviewer (#462).
///
/// **One list, deliberately, not a `PLANNER_*` and a `REVIEWER_*` pair.** The
/// two classes want the identical answer to "which tools edit files", and that
/// answer is a fact about Claude Code's tool registry, not about a role: two
/// copies would be two things to re-verify against [`KNOWN_CLAUDE_TOOLS`] on
/// every upstream rename, and the failure mode of missing one is silent
/// (a deny rule that matches nothing reads exactly like containment — the #448
/// failure this pin exists to end). Where the classes genuinely differ — the
/// git-mutation denials — they use different lists ([`CLAUDE_READONLY_DENY_GIT`]
/// is `ReadOnly`-only), which is the honest way to spell a real difference.
///
/// The single source of truth for both [`OrchRegistry::build_agent_command`] and
/// [`OrchRegistry::build_agent_argv`], so the two spellings can't
/// independently drift (on top of the existing
/// `build_agent_argv_matches_command_line` consistency test). Every entry
/// here is a bare tool name, pinned against [`KNOWN_CLAUDE_TOOLS`] by
/// `claude_edit_deny_tools_are_known_claude_tools` — see that constant's
/// doc for exactly what the pin does and doesn't guarantee.
///
/// `MultiEdit` was dropped from here in #448: Claude Code folded it into
/// `Edit` and it no longer matches any tool, which is what printed
/// `Permission deny rule "MultiEdit" matches no known tool` on every
/// planner's first line of output. It was harmless — denying a nonexistent
/// tool is a no-op, and `Edit`/`Write`/`NotebookEdit` still carry the
/// guarantee — but it was a leftover, not a decision; now it's neither.
pub const CLAUDE_EDIT_DENY_TOOLS: &[&str] = &["Edit", "Write", "NotebookEdit"];

/// The git-mutation subcommands a [`Containment::ReadOnly`] Claude agent
/// denies — a planner only, NOT a reviewer (#462): a reviewer's whole job runs
/// through the shell, and `Bash(git commit *)` is exactly the escape hatch its
/// own template hands it in place of the forbidden `git stash` (#299). Scoped to
/// the `Bash` tool (not bare tool names, so they aren't checked against
/// [`KNOWN_CLAUDE_TOOLS`] the same way — and per the Tools reference, "Tool
/// names containing `_` or `*` are exempt from" Claude's own startup-warning
/// check, which these patterns' trailing `*` already satisfies). Canonical
/// space-form spelling only (`Bash(git commit *)`, not `Bash(git commit:*)`)
/// — the colon-mid form is malformed and triggers a startup warning of its
/// own; see `claude_command_minimizes_init_approvals_without_bypass`.
pub const CLAUDE_READONLY_DENY_GIT: &[&str] = &["Bash(git commit *)", "Bash(git push *)"];

/// The blocking interactive-choice tool a Claude agent denies once its ROLE
/// (not its [`Containment`] tier — see [`claude_denies_interactive_question`])
/// warrants it — #946 Q4 / #1091 slice H. One entry: Claude Code's own
/// `AskUserQuestion` dialog, which holds the whole pane until a human answers
/// it in person.
///
/// Same drift-pin discipline as [`CLAUDE_EDIT_DENY_TOOLS`] (#448):
/// `claude_question_deny_tools_are_known_claude_tools` asserts every entry is
/// in [`KNOWN_CLAUDE_TOOLS`], so a typo or an upstream rename reads as a
/// startup warning instead of a silent no-op deny.
pub const CLAUDE_QUESTION_DENY_TOOLS: &[&str] = &["AskUserQuestion"];

/// #413 S5 review r2: the first Claude Code release that knows the `PostCompact`
/// hook event — CHANGELOG `2.1.76`: "Added `PostCompact` hook that fires after
/// compaction completes". Compared numerically, part by part.
pub const CLAUDE_POSTCOMPACT_MIN_VERSION: [u64; 3] = [2, 1, 76];

/// Whether a Claude Code of `version` may be handed a `PostCompact` entry in its
/// per-pane `--settings` file (#413 S5 review r2).
///
/// **Why a gate and not a hope.** Before `2.1.101` an unknown hook event cost the
/// whole settings file — CHANGELOG `2.1.101`: "an unrecognized hook event name in
/// `settings.json` no longer causes the entire file to be ignored". Every release
/// older than `2.1.76` both lacks the event and predates that fix, so on one of
/// them the entry would take PreCompact, SessionStart(compact), UserPromptSubmit
/// and the status line down with it. The `2.1.101` fix does not retire the gate:
/// below `2.1.76` the event is unknown either way, so the entry buys nothing.
///
/// `None`, or a version whose first three dot-parts are not all numbers, is
/// `false`: an unknown version gets no `PostCompact`, and the compaction resolves
/// through busy-then-quiet as it did before #413 S5. Parts compare as NUMBERS —
/// `2.1.8` is older than `2.1.76`, which a string comparison gets backwards.
pub fn claude_supports_postcompact(version: Option<&str>) -> bool {
    let Some(version) = version else { return false };
    let mut parts = version.trim().split('.').map(|p| p.parse::<u64>().ok());
    let mut v = [0u64; 3];
    for slot in v.iter_mut() {
        match parts.next() {
            Some(Some(n)) => *slot = n,
            _ => return false,
        }
    }
    v >= CLAUDE_POSTCOMPACT_MIN_VERSION
}

thread_local! {
    /// Test seam for [`claude_cached_version`]: `Some(v)` answers `v` on the
    /// calling thread only, so parallel test threads never see each other's
    /// version and nothing spawns a real `claude --version` to fill the cache.
    static CLAUDE_VERSION_OVERRIDE: std::cell::RefCell<Option<Option<String>>> = const { std::cell::RefCell::new(None) };
}

/// Test-only seam: answer [`claude_cached_version`] with `version` on the calling
/// thread (`None` restores the real probe cache). A real `pub` function rather
/// than `#[cfg(test)]`, because the integration tests that link the lib cannot
/// see `cfg(test)` items.
#[doc(hidden)] // pub for the orchestration integration tests
pub fn set_claude_version_for_test(version: Option<Option<String>>) {
    CLAUDE_VERSION_OVERRIDE.with(|c| *c.borrow_mut() = version);
}

/// The Claude Code version the CACHED `claude` probe read (`cliprobe::cached_version`)
/// — a lookup on the spawn path, never a `claude --version` run there. A pane
/// spawned before the startup sweep's probe lands sees `None` (#413 S5 review r2).
pub(in crate::orchestration) fn claude_cached_version() -> Option<String> {
    if let Some(v) = CLAUDE_VERSION_OVERRIDE.with(|c| c.borrow().clone()) {
        return v;
    }
    crate::cliprobe::cached_version("claude")
}

/// Whether a Claude agent's launch denies [`CLAUDE_QUESTION_DENY_TOOLS`] —
/// #946 Q4 / #1091 slice H.
///
/// **The failure mode this closes.** An orchestrator's `AskUserQuestion`
/// modal holds the whole pane — every delivery to it, including a delegate's
/// report, queues instead of landing, and that queue is bounded
/// (`queue::QUEUE_MAX_PER_PANE`, today 8): once full, further admissions are
/// refused. A blocking question doesn't just stall the asker, it strands
/// every agent trying to report to it — the incident `docs/design/
/// human-questions.md`'s "The problem" section narrates (a run held
/// overnight on one unanswered question, nothing reviewed or merged until
/// morning). `ask_human` (#946 Q1, shipped) is the non-blocking replacement
/// for BOTH roles this deny covers: #1091 slice E widened its dispatch gate
/// from `require_orchestrator` to `require_orchestrator_or_liaison`, so a
/// liaison poses its own durable, registry-backed question through the same
/// tool rather than only relaying through the orchestrator — see the design
/// note's Q4/H section for the full picture.
///
/// **Orthogonal to [`Containment`] by construction** — deliberately NOT a
/// fourth tier on that ladder, which is about edits/git and answers a
/// different question. The orchestrator is `Containment::None` (denies
/// nothing today) and still gets this deny; a liaison-hinted reviewer is
/// `Containment::NoEdits` (#891) and gets both denials in the SAME
/// `--disallowedTools` list. A quick run's root (#3679) is denied it too, for
/// the orchestrator's reason. A worker, a planner, and a non-liaison reviewer
/// never get this one: a human standing at a DELEGATE's own pane, answering
/// its dialog in person, never stalls anyone else, so the dialog stays
/// reachable exactly where holding it is harmless.
///
/// **H7 (human decision, #1091 plan-783 part 2): the liaison is included.**
/// The liaison — the human-interface agent (#891, `kind: reviewer` +
/// `role_hint: liaison`) — can hold a pane just as the orchestrator can, so
/// it is named here explicitly rather than left to fall out of some other
/// rule. A group with no liaison block simply never asks this function the
/// question that would return `true` for one — no error, no special case:
/// the #891 principle that nothing may depend on a liaison existing.
pub fn claude_denies_interactive_question(role: Role, role_hint: Option<&str>) -> bool {
    // #3679: a quick root too, for the orchestrator's reason exactly. Its
    // helpers' reports are typed into its pane, and a blocking modal holds
    // that pane while they queue behind it — with nobody in the pane to
    // answer, since orrerix opened it. A root that needs the human says so
    // with `report(outcome=blocked)`, which holds the run and notifies them.
    role == Role::Orchestrator || role_hint == Some("liaison")
}

/// The generic Claude PreCompact / SessionStart(compact) hook body (#417),
/// written once per machine (`OrchRegistry::ensure_compact_hook_script`) and
/// invoked per agent with `event`, the group's state dir, and the agent's id
/// as literal argv — see `compact_hook_settings` for how those are supplied.
/// Deliberately carries NO repo/group/toolchain-specific text (constraint #8):
/// it only ever writes a marker file loomux's own compact-nudge tick polls,
/// and — for `sessionstart-compact` — prints Claude Code's own
/// `additionalContext` JSON shape with a fixed, generic re-grounding line.
///
/// **Already slim, verified consistent with `compact_reinjection_notice`'s
/// round-5 correction:** this `ctx` line was ALREADY the terse shape that
/// correction brought the Rust-side notice to — it names the contract as
/// durable (`--agent`, a generated custom-agent file) rather than
/// re-embedding it, and points at (never inlines) the ledger path. It
/// stays a bare pointer rather than an
/// inlined tail like the Rust side's belt-and-braces ledger embed: this
/// script is one generic file for the whole machine (constraint #8 again),
/// and embedding a properly-capped, line-safe tail would duplicate
/// `directive_ledger_embed`'s truncation logic in POSIX shell — exactly
/// the two-implementations-drifting-apart risk that function's own doc
/// already argues against. So the two channels agree on CONTENT (the
/// contract is durable, re-sync via list_tasks/get_state/list_agents, the
/// ledger's location) without being byte-identical strings.
///
/// Never fails: **`PreCompact` can BLOCK compaction itself** — Claude Code's
/// own hooks reference (code.claude.com/docs/en/hooks) confirms exit code 2
/// (or a `{"decision":"block"}` JSON) on `PreCompact` prevents the compact
/// from happening at all (rev-4 review round 3, safety finding). A marker-
/// write failure must never escalate into "the user's compaction didn't
/// happen" — so every path here exits 0 regardless of whether the marker
/// write actually landed: no `set -e`/`set -o pipefail` anywhere, and the
/// unconditional `exit 0` on the LAST line runs no matter what any earlier
/// command returned. A silently-lost marker just leaves this agent on the
/// pre-#417 inference tier for this one event — a degrade, never a block.
///
/// The marker is written with `touch`, NOT the more obvious `: > "$path"`
/// (round 3's first draft used that, and the real-execution test below
/// caught it): a bare `> "$path"` redirect that fails to OPEN its target
/// (e.g. the parent directory doesn't exist, as it wouldn't if `mkdir -p`
/// above also failed) is a shell-level redirection error, and POSIX makes
/// that FATAL for a non-interactive shell — it aborts the whole script on
/// the spot, before ever reaching the trailing `exit 0`, and the `2>/dev/
/// null` on the same line never even gets applied (the shell fails to set
/// up the redirect before it can process the next one). `touch`'s own
/// failure, by contrast, is an ordinary COMMAND failure (`touch`'s own error
/// handling, not the shell's redirection machinery), which a non-interactive
/// shell simply reports and continues past — exactly the behavior this
/// script needs. Pinned directly by `compact_hook_script_sh_exits_zero_when_
/// the_marker_dir_cant_be_created`, which runs this exact script under an
/// induced `mkdir` failure and asserts the real process exit code — the
/// ORIGINAL `: >` version of this script failed that test for real.
///
/// **#112's `promptsubmit` arm carries a STRICTER safety bar than either
/// event above**: Claude's hooks reference documents `UserPromptSubmit` exit
/// code 2 as not merely blocking but ERASING the user's submitted prompt, and
/// on exit 0 any stdout the hook prints is injected as context the model
/// sees. So this arm must (a) exit 0 unconditionally, on every path,
/// including every I/O failure, exactly like `precompact`/`sessionstart-
/// compact` above, AND (b) never write anything to its OWN stdout — the
/// touch-then-append is therefore structured so the append's target is
/// EITHER the marker file (`>> "$marker"`) or, on a `touch` failure,
/// `/dev/null` (`cat >/dev/null 2>&1`, which also drains stdin so the
/// hook's caller never blocks on a full pipe) — never the script's inherited
/// stdout. The gate is `touch` first (an ORDINARY command failure per the
/// reasoning above, safely reported and continued past) rather than a bare
/// `>>` open on a possibly-nonexistent parent dir (the SAME fatal-
/// redirection-error hazard `: > "$path"` had) — once `touch` proves the
/// path is openable, the single grouped `{ cat; printf '\n'; } >> "$marker"`
/// append is safe. Real-execution pins:
/// `promptsubmit_hook_script_sh_exits_zero_and_prints_nothing_when_the_
/// marker_dir_cant_be_created` (induced `mkdir` failure — exit 0 AND empty
/// stdout) and `promptsubmit_hook_script_sh_appends_stdin_verbatim_with_no_
/// stdout` (the happy path — two firings append, they don't overwrite, and
/// stdout stays empty on the successful path too). Mutation-verified (PR
/// #451 body has the exact commands/output): a stray unredirected `cat`
/// ahead of the touch-gate — a realistic typo that would leak the submitted
/// prompt straight into Claude's own context — reds both tests. **Honesty
/// note, found by review**: on this shell (git-bash `sh.exe`, and per POSIX
/// itself), the FATAL-on-redirection-error rule applies to *special
/// built-ins* like a bare `: >`, not to ordinary utilities or compound
/// groups — so `{ cat; printf '\n'; } >> "$marker"` alone, without the
/// `touch` gate, was verified NOT to abort this script on the induced
/// `mkdir` failure either (confirmed directly, not assumed). The `touch`
/// gate is kept as defense-in-depth / consistency with the rest of this
/// module's established style (never trust a shell's exact redirection-
/// error semantics across every `sh` implementation this might run under),
/// not because a mutation here demonstrates a live regression on THIS
/// shell — that would be a false claim, exactly the kind #451 round 1
/// review caught this doc comment making about a test that didn't exist.
///
/// **#993 S1's `statusline` arm is the one arm whose stdout is a PRODUCT.**
/// Claude Code runs a `statusLine` command on every assistant message and
/// "displays whatever the script prints to stdout" (code.claude.com/docs/en/
/// statusline). loomux's `--settings` entry outranks the human's own
/// `statusLine`, so this arm must (a) save the payload for
/// `modelstate::parse_statusline_snapshot` — whole-file, via `.tmp` + `mv -f`,
/// so the compact-nudge tick never reads a torn file — and (b) hand the SAME
/// payload to the human's own status-line command, passed as `$4` (resolved
/// once at spawn, `OrchRegistry::user_statusline`), so its output is what the
/// pane shows. The arm prints nothing of its own on any path: with no `$4`
/// the line stays blank. That is not identical to a claude pane with no
/// status line configured — the CLI hides most footer keyboard hints whenever
/// a `statusLine` exists (a disclosed residual, `docs/design/pane-model-state.md`
/// §S1). The payload is read into a variable FIRST, above the
/// group-dir check, so the chain gets it even when the snapshot write fails
/// (touch-gated, per the reasoning above) — a broken hooks dir costs loomux
/// its reading, never the human their status line. Still `exit 0` on every
/// path, the chain's own status ignored: the human decided the default on
/// #993. Real-execution pins: the `statusline_hook_*` tests.
///
/// The chain runs as `( eval "$chain" )`, not `sh -c "$4"`: a bare `sh` is a
/// PATH lookup, and on Windows the PATH a CLI hands its hooks routinely lacks
/// Git's `usr\bin` (#335 — the reason every hook command here invokes an
/// ABSOLUTE `sh`). `eval` runs the line in the interpreter already running
/// this script. The parentheses are load-bearing only on ksh and zsh:
/// `eval` is a special built-in, so a syntax error in the human's command is
/// fatal to the shell that runs it, and those shells run a pipeline's last
/// stage in the CURRENT shell, where it would skip the `exit 0`. dash and
/// bash already fork every stage, so no CI shell exercises the parentheses
/// and no test pins them — they are defence for an `sh` this suite never
/// runs. `set --` first, so the human's command sees no positional
/// parameters, as it would under `sh -c`.
///
/// **#413 S5's `postcompact` arm is existence-only, like `precompact`.** Claude
/// Code's hooks reference documents `PostCompact` as running "after Claude
/// Code completes a compact operation", with no decision control (a nonzero
/// exit only shows stderr), and its input as the common fields plus `trigger`
/// and `compact_summary` — "the conversation summary generated by the compact
/// operation". The arm touches `<agent>.postcompact.json` and never writes the
/// payload into it: the marker's mtime is the whole signal, and the one field
/// the payload adds is the conversation's summary, which loomux has no reason
/// to copy onto disk beside the group's state. It still DRAINS stdin to
/// `/dev/null` after the touch, unlike `precompact`, because this payload is
/// the one whose size is the conversation's — the `promptsubmit` arm's reason
/// for draining (the hook's caller never blocks on a full pipe). Exit 0 on
/// every path, as above. Real-execution pin:
/// `postcompact_hook_script_sh_touches_its_marker_drains_stdin_and_prints_nothing`.
///
/// **Naming, kept imprecise on purpose (#112):** this file is still named
/// `compact-hook.sh` (see `ensure_compact_hook_script`) and this constant is
/// still `COMPACT_HOOK_SCRIPT`, even though the `promptsubmit` arm below
/// means "compact" no longer describes everything this script does.
/// Renaming either would strand any already-spawned agent whose generated
/// `--settings`/`loomux-compact.json` still points at the OLD path/name —
/// so both stay, generic-lifecycle-hook-script-not-just-compact in
/// substance even though the names say otherwise.
#[doc(hidden)] // pub for integration tests: they run this script for real
pub const COMPACT_HOOK_SCRIPT: &str = "#!/bin/sh\n\
event=\"$1\"\n\
group_dir=\"$2\"\n\
agent_id=\"$3\"\n\
if [ \"$event\" = statusline ]; then\n\
  payload=$(cat)\n\
fi\n\
if [ -n \"$group_dir\" ] && [ -n \"$agent_id\" ]; then\n\
  mkdir -p \"$group_dir/hooks\" 2>/dev/null\n\
  case \"$event\" in\n\
    precompact)\n\
      touch \"$group_dir/hooks/$agent_id.precompact.json\" 2>/dev/null\n\
      ;;\n\
    postcompact)\n\
      touch \"$group_dir/hooks/$agent_id.postcompact.json\" 2>/dev/null\n\
      ;;\n\
    sessionstart-compact)\n\
      touch \"$group_dir/hooks/$agent_id.sessionstart-compact.json\" 2>/dev/null\n\
      ctx=\"[orrerix] Session resumed after a compact. Your durable role contract already rides in the system prompt (--agent, a generated custom-agent file) -- trust it over any summary above. Re-sync live state now: list_tasks, get_state, list_agents. Directive ledger (if any): ${group_dir}/ledger-${agent_id}.log\"\n\
      printf '{\"hookSpecificOutput\":{\"hookEventName\":\"SessionStart\",\"additionalContext\":\"%s\"}}\\n' \"$ctx\"\n\
      ;;\n\
    promptsubmit)\n\
      marker=\"$group_dir/hooks/$agent_id.promptsubmit.jsonl\"\n\
      if touch \"$marker\" 2>/dev/null; then\n\
        { cat; printf '\\n'; } >> \"$marker\" 2>/dev/null\n\
      else\n\
        cat >/dev/null 2>&1\n\
      fi\n\
      ;;\n\
    statusline)\n\
      snap=\"$group_dir/hooks/$agent_id.statusline.json\"\n\
      if touch \"$snap.tmp\" 2>/dev/null; then\n\
        printf '%s\\n' \"$payload\" > \"$snap.tmp\" 2>/dev/null && mv -f \"$snap.tmp\" \"$snap\" 2>/dev/null\n\
      fi\n\
      ;;\n\
  esac\n\
fi\n\
if [ \"$event\" = postcompact ]; then\n\
  cat >/dev/null 2>&1\n\
fi\n\
if [ \"$event\" = statusline ] && [ -n \"$4\" ]; then\n\
  chain=\"$4\"\n\
  set --\n\
  printf '%s\\n' \"$payload\" | ( eval \"$chain\" )\n\
fi\n\
exit 0\n\
";

/// What `OrchRegistry::compact_hook_settings` hands `write_hook_settings_file`:
/// the `hooks` object and the `statusLine` object (#993 S1), two TOP-LEVEL keys
/// of the one `--settings` file, built from the same script and `sh` so either
/// both exist or neither does.
pub(in crate::orchestration) struct ClaudeHookSettings {
    pub(in crate::orchestration) hooks: Value,
    pub(in crate::orchestration) status_line: Value,
}

/// Claude Code's permission mode for an (un)attended agent: its native `auto`
/// preset when unattended (what a human uses interactively), else `acceptEdits`.
pub fn claude_permission_mode(unattended: bool) -> &'static str {
    if unattended {
        "auto"
    } else {
        "acceptEdits"
    }
}

/// #465: a read-only agent (today: a planner) needs more than
/// `CLAUDE_EDIT_DENY_TOOLS` denying its KNOWN editing tools by name —
/// that list is fail-open to any editing tool Claude Code adds *after* it
/// was last written (nothing mismatches, nothing warns, the tool just
/// works). `--permission-mode dontAsk` closes that direction instead of
/// narrowing it: per the official
/// [permission modes reference](https://code.claude.com/docs/en/permission-modes.md)
/// ("Allow only pre-approved tools with dontAsk mode", fetched as raw
/// markdown, verified 2026-07-29): "If you set `dontAsk` mode, Claude Code
/// auto-denies every tool call that would otherwise prompt you. Claude runs
/// only actions matching your `permissions.allow` rules, read-only Bash
/// commands, and calls approved by a PreToolUse hook." A brand-new editing
/// tool released tomorrow is not in `--allowedTools` — nothing added it
/// there — so `dontAsk` denies it on first use with zero loomux code
/// change, which is exactly the property `auto` mode's background
/// safety-classifier fallback could never give: `auto` still lets Claude
/// **choose** any tool, including one #448/#465's literal deny lists don't
/// know about yet. See `docs/design/orchestration.md`'s `#465` section for
/// the full argument, the two rejected alternatives (Copilot's
/// `--available-tools`; listening for the CLI's own startup warning), and
/// why Copilot's side of this issue is documented open rather than closed
/// the same way.
///
/// `--disallowedTools` (`CLAUDE_EDIT_DENY_TOOLS`/`CLAUDE_READONLY_DENY_GIT`) stays emitted
/// alongside this, unchanged: a bare deny rule "removes the tool from
/// Claude's context entirely" (stronger than merely unapproved — Claude
/// never sees it as a choice at all) and `claude_edit_deny_tools_are_known_claude_tools`
/// still catches a stale/mistyped entry. The two layers are independent
/// defences for the two different directions (named-and-wrong vs.
/// unnamed-and-new); dropping either narrows the guarantee.
///
/// **Never call this for a non-read-only agent.** `dontAsk` auto-denies every
/// tool call outside `--allowedTools` — for a worker, the file edits it exists
/// to make; for a reviewer, the *shell* it exists to work through (running the
/// tests, `gh pr checkout --detach`, posting the review). That second case is
/// worth stating separately since #462, because a reviewer now carries deny
/// flags too and could otherwise look like a candidate for this mode: it is
/// not, and [`Containment::is_read_only`] — not `denies_edits()` — is the
/// predicate its callers pass. This is also why the branch below is `if
/// read_only`, not folded into `claude_permission_mode` itself: that function
/// is also used by `single_pane_autopilot_flags` for a *non-read-only* solo
/// pane, which must keep `auto`.
///
/// **The fail-open direction this closes stays open one rung down (#462).** The
/// argument above — a literal deny list cannot name an editing tool Claude Code
/// ships tomorrow — applies word for word to a reviewer's
/// [`Containment::NoEdits`]. The *remedy* does not transfer: `dontAsk` is
/// precisely the thing a reviewer cannot have. So a reviewer's editing-tool
/// denial is fail-open to a future tool by construction, and closing it would
/// need a mechanism that separates "new editing tool" from "shell command",
/// which neither CLI offers today. Recorded, not fixed — see
/// `docs/design/orchestration.md`'s reviewer-containment section.
pub fn claude_effective_permission_mode(unattended: bool, read_only: bool) -> &'static str {
    if read_only {
        "dontAsk"
    } else {
        claude_permission_mode(unattended)
    }
}

/// Claude's `--model` value with the block's context variant applied (#687), as
/// **one literal argv token**: the plain model when no variant is set, else the
/// documented `{model}[{variant}]` extended-context alias (`sonnet[1m]`).
///
/// Composed here, at emit, rather than stored in `Block::model`, because
/// `sanitize_model` strips brackets — a `sonnet[1m]` written as a model id
/// would silently become the broken `sonnet1m` — and widening that sanitizer to
/// admit brackets would put a shell glob pattern into every model string.
pub(crate) fn claude_model_token(model: &str, context: &str) -> String {
    if context.is_empty() {
        model.to_string()
    } else {
        format!("{model}[{context}]")
    }
}

/// [`claude_model_token`] for the SHELL form. Quoted when — and only when — a
/// variant is set: `[1m]` is a glob pattern to a POSIX shell and a bare bracket
/// starts an attribute in PowerShell, while quoting unconditionally would
/// change the emitted line for every group that pinned nothing.
pub(in crate::orchestration) fn claude_model_arg(model: &str, context: &str) -> String {
    if context.is_empty() {
        model.to_string()
    } else {
        format!("\"{}\"", claude_model_token(model, context))
    }
}
