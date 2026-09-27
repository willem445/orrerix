//! Copilot CLI's adapter: unattended and autopilot flags, tool permissions
//! and MCP grants, the compact/prompt-submit hooks, the autopilot dialog
//! (detect and confirm), and the permissions file and folder trust.
//! Design note: `docs/design/harness-adapters.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): `PtyManager`,
//! `crate::pty`. IO: fs, threads/sleep. Sibling files it calls: `auditlog.rs`,
//! `clis/mod.rs`, `panetail.rs`, `pathkey.rs`.

use super::*;
/// Poll interval while watching for the consent dialog.
const AUTOPILOT_DIALOG_POLL: Duration = Duration::from_millis(250);
/// Pause after answering so the TUI dismisses the dialog and repaints / starts
/// the turn before the delivery's confirmation window measures the burst.
const AUTOPILOT_DIALOG_SETTLE: Duration = Duration::from_millis(700);
/// Keys that confirm the highlighted menu item in Copilot's consent dialog.
/// Focus-in report (`ESC[I`) + Enter (`\r`) — the SAME transport as
/// [`submit_sequence`]`("copilot")`. The dialog is answered after the kickoff
/// submit, by which point copilot's focus flag is already true (the submit's
/// own `ESC[I` set it); the prefix is kept so this stays consistent with the
/// other pane-write sites and self-sufficient if a stray blur ever intervened
/// (#98). The `\r` selects the default-highlighted "Enable all permissions"
/// (menu `initialIndex` 0, `code==="return"`, verified against the 1.0.69 TUI)
/// — no arrow keys needed.
#[doc(hidden)] // pub for integration tests
pub const COPILOT_AUTOPILOT_CONFIRM_KEYS: &[u8] = b"\x1b[I\r";

// ── Per-CLI unattended ("autopilot / allow all") permission flags ───────────
//
// The single source of truth for what "unattended" means on each agent CLI.
// Both the orchestration spawn path (`build_agent_command`) and the single-pane
// launcher (`single_pane_autopilot_flags`, exposed as the `agent_autopilot_flags`
// Tauri command) build from these atoms, so the two paths can't drift (#101).

/// Copilot's base "allow all" atom: pre-approve all tools and all paths so the
/// agent runs without per-tool / path-verification confirmation. Combined with
/// `--autopilot` to build [`COPILOT_GROUP_AUTOPILOT_FLAGS`] — kept as its own
/// constant purely so the two flag strings are built from one shared atom
/// instead of two independently-typed literals that could drift apart.
///
/// **Spellings re-verified against copilot 1.0.77's reference (#781, checked
/// 2026-08-03), with no drift.** The
/// [allow/deny reference](https://docs.github.com/en/copilot/how-tos/copilot-cli/use-copilot-cli/allowing-tools)
/// still lists `--allow-all-tools` ("Full access to the available tools") under
/// *Permissive options*, and the
/// [programmatic reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-programmatic-reference)
/// still lists `--allow-all-paths` ("Disable file-path verification entirely").
/// Neither page marks either flag deprecated or renamed, so both atoms stay
/// exactly as spelled.
///
/// **One documented limit loomux cannot flag its way past**, recorded here
/// because a work machine is where it bites: the same *Permissive options*
/// section states, verbatim, "If you have a Copilot Business or Copilot
/// Enterprise license, these commands may be blocked by an enterprise
/// administrator." An org that blocks bypass-permissions mode neuters these two
/// atoms while leaving `--autopilot` (not a permissive option) working, so the
/// pane runs unattended and prompts anyway. The targeted grants loomux emits
/// alongside them — `--allow-tool <server>` in the base command, `--add-dir` for
/// the group dir and workdir — are NOT permissive options and are what keeps
/// such a pane usable at all. Nothing here can override the policy, and loomux
/// must not pretend to.
pub const COPILOT_UNATTENDED_FLAGS: &str = "--allow-all-tools --allow-all-paths";

/// Copilot's unattended-autopilot flags: [`COPILOT_UNATTENDED_FLAGS`] plus
/// `--autopilot`, which puts an agent into true autopilot mode. Autopilot mode
/// is not just the idle auto-continue loop — it injects an autonomy directive
/// into the model's system prompt ("persist autonomously … continue executing
/// without waiting for user input … the user may not even be present"), which
/// is exactly right for an unattended agent.
///
/// Used by BOTH the group spawn path and, since #364, the single-pane launcher
/// (`single_pane_autopilot_flags`) — a single-pane copilot agent with the
/// Autopilot checkbox on now enters the same true autopilot mode a group
/// worker does, not just allow-all. (An earlier version of this flag kept the
/// single pane on allow-all-only, reasoning a human at the keyboard didn't need
/// true autopilot framing — the human's report on #364 was that the checkbox
/// should mean autopilot on single panes too.)
///
/// `--autopilot` triggers the "Enable autopilot mode" consent dialog at
/// startup (on the human's own first submit for a single pane, since it gets
/// no programmatic kickoff — see `Role::Solo`'s "never receives a kickoff"
/// doc). That dialog is answered deterministically on both paths:
/// [`copilot_autopilot_prompt_detected`] + `deliver_prompt`'s confirm step for
/// group kickoffs, and `OrchRegistry::confirm_solo_copilot_autopilot` (started
/// right after spawn) for single panes — so neither path leaves a human
/// staring at an unanswered blocking dialog.
///
/// Kept as a derived-but-pinned constant (`single_pane_flags_reuse_the_group_path_atoms`
/// asserts it equals `--autopilot ` + [`COPILOT_UNATTENDED_FLAGS`]) so the atoms
/// can't drift apart.
pub const COPILOT_GROUP_AUTOPILOT_FLAGS: &str = "--autopilot --allow-all-tools --allow-all-paths";

/// The tool-spec values `--deny-tool`/`--allow-tool` are documented to
/// accept, per the official
/// [CLI configuration guide](https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/configure-copilot-cli)
/// ("Specifying which tool you want to allow or deny", fetched as raw HTML
/// via curl and grepped, verified 2026-07-28): "To use the --deny-tool and
/// --allow-tool options, you must specify what type of tool you want to
/// allow or deny" followed by an exactly-three-item list — `shell(COMMAND)`,
/// the bare category `write` ("tools—other than shell commands—permission to
/// modify files"), and `MCP_SERVER_NAME(tool)`. This is used only by
/// `copilot_edit_deny_tools_are_known_copilot_categories`; Copilot's docs
/// (unlike Claude's) don't say whether an unmatched `--deny-tool` value
/// warns, errors, or is silently ignored, and CLAUDE.md forbids spawning a
/// real `copilot` session to find out (a `--help` probe, the kind
/// [`crate::cliprobe`] already does, can't settle this either — enumerating
/// recognized deny-tool values needs an actual permission-engine run). So
/// this list can only be pinned against what the docs enumerate, not against
/// a live registry the way [`KNOWN_CLAUDE_TOOLS`] would be if Claude exposed
/// one.
#[doc(hidden)] // pub for integration tests
pub const KNOWN_COPILOT_DENY_CATEGORIES: &[&str] = &["write"];

/// The tool names a Copilot agent denies via `--deny-tool` once its class is
/// contained at all ([`Containment::denies_edits`] — a planner or, since #462, a
/// reviewer); the Copilot sibling of [`CLAUDE_EDIT_DENY_TOOLS`], and one list
/// for the same reason. See [`KNOWN_COPILOT_DENY_CATEGORIES`] for the source.
/// Bare `write` is the only bare-category value the docs document at all.
///
/// `edit` was dropped from here in #448: it is not one of the three
/// documented `--deny-tool` value shapes (the enumeration above reads as
/// exhaustive — "you must specify what type of tool", then exactly three
/// bullets), so it is likely already the same class of inert leftover
/// `MultiEdit` was for Claude — and unlike Claude, Copilot's docs don't
/// describe a startup warning that would have surfaced the mistake. Keeping
/// an unconfirmed name here was worse than dropping it: it read as
/// containment while only `write` was actually confirmed to provide it,
/// which is the exact false-confidence failure #448 exists to eliminate.
/// The guarantee now rests on `write` alone, which the docs describe as
/// covering the *whole* non-shell file-modification category, not one named
/// tool among several — so this is not a narrower guarantee, only an
/// honestly-scoped one. This is a docs-only conclusion (no live copilot run
/// backs it); if that leaves the question open enough to want a live check,
/// that check is the human's to make, not an agent's (CLAUDE.md constraint 3).
pub const COPILOT_EDIT_DENY_TOOLS: &[&str] = &["write"];

/// The git-mutation shell commands a [`Containment::ReadOnly`] Copilot agent
/// denies (a planner only — see [`CLAUDE_READONLY_DENY_GIT`] for why a reviewer
/// keeps these), per the `shell(COMMAND)` spec documented alongside
/// [`COPILOT_EDIT_DENY_TOOLS`].
pub const COPILOT_READONLY_DENY_GIT: &[&str] = &["shell(git commit)", "shell(git push)"];

/// The git/gh pre-approval an **attended** copilot agent gets in place of the
/// unattended allow-all atoms, so the branch→commit→PR flow doesn't prompt on
/// every command. The `shell(COMMAND)` spec's `:*` suffix is documented to
/// match "the command stem followed by a space, preventing partial matches. For
/// example, `shell(git:*)` matches `git push` and `git pull` but does not match
/// `gitea`."
pub const COPILOT_ATTENDED_ALLOW: &[&str] = &["shell(git:*)", "shell(gh:*)"];

/// The separator between two patterns inside ONE `--allow-tool`/`--deny-tool`
/// value.
///
/// **One occurrence per option — never a repeated flag (#802).** Per the CLI
/// command reference (raw-fetched via `curl -sL "https://docs.github.com/api/\
/// article/body?pathname=/en/copilot/reference/copilot-cli-reference/\
/// cli-command-reference"`, per the `agent-cli-reference` skill's raw-fetch
/// recipe, checked 2026-08-03), `--allow-tool=TOOL ...` is "Tools the CLI has
/// permission to use. Will not prompt for permission. **For multiple tools, use
/// a quoted, comma-separated list.**", and `--deny-tool=TOOL ...` says the same.
/// Neither is documented as repeatable — and that table is demonstrably careful
/// about the distinction: `--add-dir`, `--attachment`, `--add-github-mcp-tool`,
/// `--add-github-mcp-toolset` and `--disable-mcp-server` each carry "(can be
/// used multiple times)", and `--secret-env-vars` carries BOTH annotations at
/// once ("(can be used multiple times). For multiple variables, use a quoted,
/// comma-separated list."). The allow/deny how-to agrees from the other side:
/// "The value for each of these options is a comma-separated list of tool
/// kinds".
///
/// So the repeated form loomux emitted before #802 is a form the docs never
/// describe, and its failure mode — if a later occurrence *replaces* an earlier
/// one instead of accumulating — is exactly the #802 report: the base
/// `--allow-tool <server>` grant silently dropped by the git/gh pair or by a
/// block's `allow:` patterns emitted after it. Those extra occurrences are, on
/// copilot, the ONLY argv difference a workflow block introduces over the
/// built-in roster.
///
/// **This does not claim to have caught the parser in the act.** Whether
/// copilot accumulates or last-wins is undocumented, and CLAUDE.md constraint 3
/// rules out spawning a real copilot to find out. The point is that the single
/// comma-separated occurrence is correct under *either* reading, so it needs no
/// theory of the parser — the repeated form did. (The same undocumented
/// question is already flagged, and deliberately not relied on, at the solo-pane
/// MCP join site in `spawn_solo_agent`.)
pub const COPILOT_TOOL_LIST_SEP: &str = ",";

/// The `--allow-tool` and `--deny-tool` values a copilot agent launches with,
/// as ordered pattern lists ready to join with [`COPILOT_TOOL_LIST_SEP`].
/// Shared by the string and argv builders so the two can't drift, and pure so
/// the whole posture is assertable without a spawn (CLAUDE.md constraint 3).
///
/// The allow list always leads with [`MCP_SERVER`] — the bare
/// server-name form, which the reference's "Tool permission patterns" table
/// documents as a permission kind in its own right (`SERVER-NAME | MCP server
/// tool invocation | MyMCP(create_issue), MyMCP`) with a worked example: "#
/// Allow all tools from a server / `copilot --allow-tool='MyMCP'`". #802's
/// first hypothesis was that copilot 1.0.77 had stopped matching a bare server
/// name and now required a per-tool or pattern spec; the reference says
/// otherwise, so the *value* is unchanged and only its packaging moved.
///
/// Deny is separate rather than folded in because deny is not a stronger allow:
/// "Deny rules always take precedence over allow rules, even when `--allow-all`
/// is set", which is what lets a planner/reviewer stay contained under the
/// unattended allow-all atoms.
#[doc(hidden)] // pub for integration tests
pub fn copilot_tool_permissions(
    unattended: bool,
    containment: Containment,
    extra_allow: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut allow = vec![MCP_SERVER.to_string()];
    if !unattended {
        // An unattended agent already has `--allow-all-tools`; the git/gh pair
        // is the attended tier's substitute for it, not an addition to it.
        allow.extend(COPILOT_ATTENDED_ALLOW.iter().map(|s| (*s).to_string()));
    }
    // A block's own `allow:` patterns come last — the capability-closure rule
    // in `persona_inject` has already emptied this for a read-only class.
    allow.extend(extra_allow.iter().cloned());

    let mut deny: Vec<String> = Vec::new();
    if containment.denies_edits() {
        deny.extend(COPILOT_EDIT_DENY_TOOLS.iter().map(|s| (*s).to_string()));
    }
    if containment.denies_git_mutation() {
        // Nested the same way the claude arm nests its git denials: the tiers
        // are a ladder (see `Containment`).
        deny.extend(COPILOT_READONLY_DENY_GIT.iter().map(|s| (*s).to_string()));
    }
    // #803 review: a block that re-declares a pattern loomux already grants
    // (`allow: [<server>]`, or `shell(git:*)` on an attended block) would
    // otherwise repeat it inside the one value. Harmless to copilot, but it
    // makes the command line say something loomux does not mean, and this
    // value is now the single place a reader checks to see what an agent was
    // granted — so it should read as the grant, not as a concatenation.
    // First occurrence wins, so the documented order above is preserved.
    dedupe_preserving_order(&mut allow);
    dedupe_preserving_order(&mut deny);
    (allow, deny)
}

/// Drop repeat entries, keeping each value's FIRST position. Exact string
/// equality — these are tool patterns, not paths, so none of
/// [`same_path_key`]'s separator/case latitude applies.
fn dedupe_preserving_order(v: &mut Vec<String>) {
    let mut seen: HashSet<String> = HashSet::new();
    v.retain(|s| seen.insert(s.clone()));
}

/// The `tools:` entries a generated Copilot agent file adds so loomux's own MCP
/// server survives a persona's tool filter (#802).
///
/// `<server>/*` is the documented spelling: the [custom-agents configuration
/// reference](https://docs.github.com/en/copilot/reference/custom-agents-configuration)'s
/// *Tools* section says *"You can also explicitly enable all tools from a
/// specific MCP server using `some-mcp-server/*`"*, alongside *"Tool names from
/// specific MCP servers can be prefixed with the server name followed by a
/// `/`"*.
///
/// The bare server name alongside it is a **deliberate hedge, and free**. The same
/// section states *"All unrecognized tool names are ignored"*, so an entry the
/// CLI does not understand costs nothing — while the failure it insures against
/// is #802 recurring silently for a fourth round because the one documented
/// spelling turned out not to be what the 1.0.x CLI matches on (the CLI's own
/// `--allow-tool` takes the bare server name, so both spellings are live in
/// Copilot's vocabulary). Drop the bare form once a live run confirms which one
/// Copilot honors — the answer is a live observation loomux cannot make for
/// itself (CLAUDE.md constraint 3).
pub const COPILOT_MCP_TOOL_GRANTS: [&str; 2] = ["orrerix/*", MCP_SERVER];


/// A persona's `tools:` list with [`COPILOT_MCP_TOOL_GRANTS`] appended —
/// the user's own scoping intent preserved verbatim, widened by exactly the one
/// server loomux needs the delegate to be able to call (#802).
///
/// Order matters only for readability; duplicates are dropped so re-rendering a
/// file that already grants the server is idempotent.
pub(in crate::orchestration) fn copilot_tools_with_loomux(tools: &[String]) -> Vec<String> {
    let mut out: Vec<String> = tools.to_vec();
    for grant in COPILOT_MCP_TOOL_GRANTS {
        if !out.iter().any(|t| t == grant) {
            out.push(grant.to_string());
        }
    }
    out
}

/// #417 (Copilot correction): the `bash` half of loomux's `preCompact` hook
/// entry (see `OrchRegistry::ensure_copilot_compact_hook`) — inline, per the
/// docs' own `"bash": "YOUR_BASH_COMMAND"` convention, rather than a separate
/// script file: Copilot invokes this string directly (POSIX shell), so
/// there's no `sh.exe`-resolution dance to do (unlike Claude's hook command,
/// which invokes an absolute interpreter path itself). No-ops silently
/// (never a nonzero exit) when this Copilot session isn't a loomux pane at
/// all (the group-dir/agent-id variables unset under BOTH spellings) — the discrimination a
/// GLOBAL, machine-wide hook file needs, since it applies to every Copilot
/// session on the box. Same exit-0-no-matter-what safety requirement as
/// Claude's script (rev-4 review round 3) — Copilot's own hooks reference
/// (docs.github.com/en/copilot/reference/hooks-reference) documents
/// `preCompact` as a blocking event there too.
///
/// Writes the marker with `touch`, not `: > "$path"` — the same real bug
/// `COMPACT_HOOK_SCRIPT`'s doc explains in full: a bare `>` redirect whose
/// target can't be opened (the parent directory doesn't exist, as it
/// wouldn't if `mkdir -p` above also failed) is FATAL for a non-interactive
/// POSIX shell, aborting before the trailing `exit 0` and before the SAME
/// line's own `2>/dev/null` can even apply. `touch`'s failure is an ordinary
/// command failure the shell just reports and continues past. The `: >`
/// version of this failed a real-execution test
/// (`copilot_precompact_hook_bash_exits_zero_when_the_marker_dir_cant_be_
/// created`) before this fix.
#[doc(hidden)] // pub for integration tests: they run this command for real
pub const COPILOT_PRECOMPACT_HOOK_BASH: &str =
    "ORX_GD=\"${ORRERIX_GROUP_DIR:-$LOOMUX_GROUP_DIR}\"; ORX_AID=\"${ORRERIX_AGENT_ID:-$LOOMUX_AGENT_ID}\"; \
     if [ -n \"$ORX_GD\" ] && [ -n \"$ORX_AID\" ]; then \
     mkdir -p \"$ORX_GD/hooks\" 2>/dev/null; \
     touch \"$ORX_GD/hooks/$ORX_AID.precompact.json\" 2>/dev/null; \
     fi; exit 0";

/// The `powershell` half of the same hook entry — Windows' native shell, so
/// Copilot never has to fall back to (or loomux resolve) a POSIX `sh` on a
/// machine that may not have Git for Windows installed at all.
///
/// Wrapped in `try`/`catch` with `-ErrorAction Stop` on both `New-Item`
/// calls (rev-4 review round 3): unlike POSIX `sh`, PowerShell's DEFAULT
/// error preference (`Continue`) leaves whether a given cmdlet failure is
/// terminating or not somewhat provider-dependent — `-ErrorAction Stop`
/// makes every failure here explicitly terminating, and the surrounding
/// `catch {}` swallows it unconditionally, so the trailing `exit 0` (outside
/// the `try`/`catch`, always reached) is deterministic regardless of which
/// failure mode a given filesystem error takes. Pinned (Windows-only) by
/// `copilot_precompact_hook_powershell_exits_zero_when_the_marker_dir_cant_
/// be_created`.
#[doc(hidden)] // pub for integration tests: they run this command for real
pub const COPILOT_PRECOMPACT_HOOK_POWERSHELL: &str =
    "$gd = if ($env:ORRERIX_GROUP_DIR) { $env:ORRERIX_GROUP_DIR } else { $env:LOOMUX_GROUP_DIR }; $aid = if ($env:ORRERIX_AGENT_ID) { $env:ORRERIX_AGENT_ID } else { $env:LOOMUX_AGENT_ID }; \
     try { if ($gd -and $aid) { \
     $hooksDir = Join-Path $gd 'hooks'; \
     New-Item -ItemType Directory -Force -Path $hooksDir -ErrorAction Stop | Out-Null; \
     $marker = Join-Path $hooksDir ($aid + '.precompact.json'); \
     New-Item -ItemType File -Force -Path $marker -ErrorAction Stop | Out-Null \
     } } catch {}; exit 0";

/// #112's `userPromptSubmitted` hook (bash half) — an EXISTENCE-only marker,
/// same shape and same reasoning as `COPILOT_PRECOMPACT_HOOK_BASH` above
/// ("no payload parsing needed", per that constant's own doc): Copilot's
/// hooks reference (docs.github.com/en/copilot/reference/hooks-reference)
/// documents `userPromptSubmitted`'s field names (`prompt`, camelCase, or
/// `hook_event_name`/`prompt`, the VS-Code-compatible shape) but does NOT
/// document the payload's TRANSPORT for this event the way it nails down
/// Claude's stdin-JSON contract — so this arm never attempts to read or
/// capture the prompt text at all; it appends a single non-JSON marker line
/// (`.`) to the SAME per-agent `.promptsubmit.jsonl` file the Claude script
/// writes real JSON records into. `promptsubmit_records_since` treats any
/// non-empty, non-JSON line as an existence record (`text: None` —
/// `PromptSubmitRecord`'s doc), so the Copilot and Claude tiers share one
/// reader; Copilot just degrades to `PromptLandedMatch::Existence` rather
/// than ever reaching `Content`. Also notification-only per the reference
/// ("No" output is processed for this event) — there is no exit-2-erases-
/// the-prompt hazard here the way there is for Claude's `UserPromptSubmit`,
/// but the same touch-gate discipline is kept anyway for consistency with
/// every other marker write in this module and because Copilot's OWN docs
/// still document `preCompact` as blocking (the precedent this constant
/// already follows).
pub const COPILOT_PROMPTSUBMIT_HOOK_BASH: &str =
    "ORX_GD=\"${ORRERIX_GROUP_DIR:-$LOOMUX_GROUP_DIR}\"; ORX_AID=\"${ORRERIX_AGENT_ID:-$LOOMUX_AGENT_ID}\"; \
     if [ -n \"$ORX_GD\" ] && [ -n \"$ORX_AID\" ]; then \
     mkdir -p \"$ORX_GD/hooks\" 2>/dev/null; \
     marker=\"$ORX_GD/hooks/$ORX_AID.promptsubmit.jsonl\"; \
     if touch \"$marker\" 2>/dev/null; then printf '.\\n' >> \"$marker\" 2>/dev/null; fi; \
     fi; exit 0";

/// The `powershell` half of the same hook entry — see `COPILOT_PROMPTSUBMIT_
/// HOOK_BASH`'s doc for why this is existence-only (no payload capture).
/// `try`/`catch` + `-ErrorAction Stop` mirrors `COPILOT_PRECOMPACT_HOOK_
/// POWERSHELL` exactly: PowerShell has no POSIX-style fatal-redirection-error
/// hazard, so the touch-gate here is belt-and-braces consistency, not a
/// correctness requirement the way it is for the `sh` arm above.
pub const COPILOT_PROMPTSUBMIT_HOOK_POWERSHELL: &str =
    "$gd = if ($env:ORRERIX_GROUP_DIR) { $env:ORRERIX_GROUP_DIR } else { $env:LOOMUX_GROUP_DIR }; $aid = if ($env:ORRERIX_AGENT_ID) { $env:ORRERIX_AGENT_ID } else { $env:LOOMUX_AGENT_ID }; \
     try { if ($gd -and $aid) { \
     $hooksDir = Join-Path $gd 'hooks'; \
     New-Item -ItemType Directory -Force -Path $hooksDir -ErrorAction Stop | Out-Null; \
     $marker = Join-Path $hooksDir ($aid + '.promptsubmit.jsonl'); \
     Add-Content -Path $marker -Value '.' -ErrorAction Stop \
     } } catch {}; exit 0";

/// Does the pane's ANSI-stripped output tail show Copilot's "Enable autopilot
/// mode" consent dialog? (#101). Copilot opens this dialog at startup when
/// launched with `--autopilot` (the group posture); the kickoff path answers it
/// deterministically before pasting the brief so the two can't collide.
///
/// Anchored on BOTH the dialog title and its enable option — the exact strings
/// the 1.0.68 TUI paints (`title:"Enable autopilot mode"`, item label
/// `"Enable all permissions (recommended)"`). Requiring both rules out ordinary
/// prose that merely mentions autopilot or permissions, so a false positive
/// can't make loomux fire a stray Enter into a working pane. Case-insensitive;
/// tolerant of the parenthetical and of line wrapping (each substring is matched
/// independently against the whole tail).
pub fn copilot_autopilot_prompt_detected(tail: &str) -> bool {
    let t = tail.to_lowercase();
    t.contains("enable autopilot mode") && t.contains("enable all permissions")
}

/// Whether a delivery should attempt the copilot autopilot-consent confirm.
/// A *kickoff* (fresh boot OR resume, #364) of an *unattended copilot* agent
/// can show the "Enable autopilot mode" dialog — a mid-session delivery is
/// long past boot and never shows it — so only mid-session skips the
/// (fail-soft, up to `AUTOPILOT_DIALOG_WAIT`) watch. Pure so the gate is
/// unit-testable without a live pty.
pub fn should_confirm_copilot_autopilot(cli: &str, unattended: bool, is_kickoff: bool) -> bool {
    is_kickoff && unattended && cli == "copilot"
}

/// For a copilot pane launched with `--autopilot` (group OR, since #364,
/// single-pane): watch for the "Enable autopilot mode" consent dialog and
/// answer it (Enter selects the default "Enable all permissions"). Copilot
/// 1.0.69 opens this dialog in response to the FIRST message submit, not at
/// boot (#179) — a group kickoff runs this right AFTER loomux's own kickoff
/// Enter, so selecting the default both enables autopilot and lets the
/// just-submitted brief proceed (the pending message is not discarded); a
/// solo pane has no programmatic kickoff, so `confirm_solo_copilot_autopilot`
/// just watches from spawn for whenever the HUMAN's own first submit triggers
/// it. Fail-soft: returns `false` without acting if the dialog does not appear
/// within `max_wait` (e.g. copilot changed the flow, consent was already
/// recorded, or — solo path — the human never got that far), and the caller's
/// own retries/no-op carry on.
///
/// Returns `true` iff it detected and answered the dialog. `max_wait` differs
/// by caller (a short fail-soft window after loomux's own deterministic Enter
/// for the group path vs. `SOLO_AUTOPILOT_DIALOG_WAIT`'s much longer,
/// human-paced window for solo); poll interval and keys come from module
/// constants so the wiring is testable, and the pure recognizer is
/// [`copilot_autopilot_prompt_detected`].
pub(in crate::orchestration) fn confirm_copilot_autopilot_dialog(
    ptys: &crate::pty::PtyManager,
    pty_id: u32,
    root: &Path,
    group: &GroupId,
    agent: &str,
    max_wait: Duration,
) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < max_wait {
        let Some(out) = ptys.output_tail(pty_id) else {
            return false; // terminal closed — let the caller's own checks report
        };
        if copilot_autopilot_prompt_detected(&strip_ansi(&out)) {
            let _ = ptys.write_bytes(pty_id, COPILOT_AUTOPILOT_CONFIRM_KEYS);
            append_audit(root, group, brand::AUDIT_ACTOR, "copilot-autopilot-confirmed", json!({
                "to": agent,
                "waited_ms": start.elapsed().as_millis() as u64,
            }));
            // Let the TUI dismiss the dialog and repaint before the brief pastes.
            std::thread::sleep(AUTOPILOT_DIALOG_SETTLE);
            return true;
        }
        std::thread::sleep(AUTOPILOT_DIALOG_POLL);
    }
    false
}

/// The JSONC-ish split both copilot config editors below share: leading `//`
/// comment lines, then a JSON object. Returns `(comments, body)`.
fn split_jsonc_comment_header(text: &str) -> (&str, &str) {
    let mut comment_len = 0;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with("//") || t.is_empty() {
            comment_len += line.len();
        } else {
            break;
        }
    }
    text.split_at(comment_len)
}

/// The name of copilot's saved-permissions file, and its legacy extensionless
/// spelling. Per the configuration-directory reference: "Older builds used an
/// extensionless file named `permissions-config`. If `permissions-config.json`
/// doesn't exist but the extensionless file does, the CLI still honors the
/// legacy file. Use `permissions-config.json` for new edits."
///
/// That fallback is why [`copilot_permissions_file`] exists rather than a
/// hardcoded join: creating `permissions-config.json` beside a user's existing
/// legacy file would make the CLI stop honouring the legacy one, silently
/// discarding every approval they had saved.
const COPILOT_PERMISSIONS_FILE: &str = "permissions-config.json";
const COPILOT_PERMISSIONS_FILE_LEGACY: &str = "permissions-config";

/// Which permissions file to edit inside `home` — see
/// [`COPILOT_PERMISSIONS_FILE`] for why the legacy name can't just be ignored.
fn copilot_permissions_file(home: &Path) -> PathBuf {
    let modern = home.join(COPILOT_PERMISSIONS_FILE);
    if !modern.exists() {
        let legacy = home.join(COPILOT_PERMISSIONS_FILE_LEGACY);
        if legacy.exists() {
            return legacy;
        }
    }
    modern
}

/// Loomux's grant in copilot's **documented** permission store,
/// `~/.copilot/permissions-config.json` — returning the new file content, or
/// `None` when nothing needs writing (already granted, or the existing file is
/// unparseable and must not be clobbered).
///
/// **Why this file, when [`add_trusted_folder`] already writes one (#802).**
/// The `config.json` / `trustedFolders` pair is the pre-1.0.77 surface, and it
/// appears in no page of copilot's current reference. Per the configuration-
/// directory reference (raw-fetched via `curl -sL "https://docs.github.com/api/\
/// article/body?pathname=/en/copilot/reference/copilot-cli-reference/\
/// cli-config-dir-reference"`, per the `agent-cli-reference` skill's raw-fetch
/// recipe, checked 2026-08-03), `config.json` "Stores internal application
/// state that is managed automatically by the CLI, including authentication
/// data … You should not normally need to edit this file", and "Any user
/// settings in `config.json` at startup are automatically migrated to
/// `settings.json`". `trustedFolders` is absent from that file's description,
/// from `settings.json`'s full settings table, and from this file's full
/// schema. What the docs DO name is this file — "Saved tool and directory
/// permissions per project" — and the allow/deny how-to
/// (`…/how-tos/copilot-cli/use-copilot-cli/allowing-tools`) says outright: "Any
/// directories you grant access to are saved to the same file."
///
/// That is an ABSENCE claim, so it is sourced rather than assumed — but an
/// absence across three reference pages is not proof the old key was *removed*,
/// only that it is no longer described. So the legacy write stays alongside
/// this one (see [`OrchRegistry::pre_trust_copilot_folder`]): loomux cannot
/// observe which copilot build a machine runs, and dropping the old key would
/// trade a confirmed gap for a possible regression.
///
/// **Two grants, both straight off the documented schema:**
/// - `locations.<key>.allowed_directories` — "Extra directories that the path
///   gate can access for this location."
/// - `locations.<key>.tool_approvals` `{ kind: "mcp", serverName, toolName:
///   null }` — "Approves one MCP tool, or every tool on the server when
///   `toolName` is `null`", with "`serverName` must match the configured MCP
///   server name exactly. Use the raw server name from your MCP configuration,
///   not a sanitized tool-name prefix." This is the durable form of the grant
///   `--allow-tool <server>` already makes on argv, and "the CLI lists the
///   loomux MCP server as available, but the agent has no permission to use its
///   tools" (#802) is a report of its absence. It grants nothing to a non-loomux
///   session: an approval names a server, and a session that never loads
///   loomux's `--additional-mcp-config` has no such server to invoke.
///
/// **`location` is the git root, not the agent's workdir.** The reference keys
/// this map by "the Git root used for permission scoping" and states "Linked
/// worktrees resolve to the main repository root, so they share permissions
/// with the main worktree" — so a worker in a dedicated worktree looks its
/// approvals up under the repo it was cut from. The worktree still needs the
/// *path* gate, which is what `folder` adds to `allowed_directories`.
///
/// An existing location key naming the same directory is REUSED rather than
/// duplicated: "The key must match the location where Copilot CLI is running.
/// If the key doesn't match, the saved approvals won't apply", so two keys for
/// one repo would leave the user's own approvals stranded under whichever one
/// copilot didn't pick. "Same directory" is decided by [`same_path_key`], whose
/// latitude is **platform-correct, not universal** (#803 review B1): a
/// case-variant spelling is the same key on Windows and a DIFFERENT one
/// everywhere else, because that is what copilot itself does.
pub fn copilot_permissions_grant(
    config_text: &str,
    location: &str,
    folder: &str,
    mcp_server: &str,
) -> Option<String> {
    let (comments, body) = split_jsonc_comment_header(config_text);
    let mut v: Value = if body.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(body).ok()?
    };
    let locations = v
        .as_object_mut()?
        .entry("locations")
        .or_insert_with(|| json!({}))
        .as_object_mut()?;
    // Reuse an equivalent key rather than adding a second spelling of it.
    let key = locations
        .keys()
        .find(|k| same_path_key(k.as_str(), location))
        .cloned()
        .unwrap_or_else(|| normalize_path_key(location));
    let entry = locations.entry(key).or_insert_with(|| json!({})).as_object_mut()?;

    let mut changed = false;

    let dirs = entry
        .entry("allowed_directories")
        .or_insert_with(|| json!([]))
        .as_array_mut()?;
    if !dirs.iter().any(|e| e.as_str().is_some_and(|s| same_path_key(s, folder))) {
        dirs.push(json!(folder));
        changed = true;
    }

    let approval = json!({ "kind": "mcp", "serverName": mcp_server, "toolName": null });
    let approvals = entry
        .entry("tool_approvals")
        .or_insert_with(|| json!([]))
        .as_array_mut()?;
    // A server-wide approval is `toolName: null`; a pre-existing approval for
    // ONE tool on the same server is not the same grant and must not satisfy
    // this check, or a hand-approved `report` would suppress the rest.
    let already = approvals.iter().any(|e| {
        e.get("kind").and_then(Value::as_str) == Some("mcp")
            && e.get("serverName").and_then(Value::as_str) == Some(mcp_server)
            && e.get("toolName").is_some_and(Value::is_null)
    });
    if !already {
        approvals.push(approval);
        changed = true;
    }

    if !changed {
        return None;
    }
    Some(format!("{comments}{}\n", serde_json::to_string_pretty(&v).ok()?))
}

/// Add a folder to copilot's `trustedFolders` config, returning the new
/// file content — or None when nothing should be written (already trusted,
/// or the existing config is unparseable and must not be clobbered). The
/// file is JSONC-ish: leading `//` comment lines before a JSON object;
/// comments and unknown fields are preserved.
///
/// The pre-1.0.77 surface — see [`copilot_permissions_grant`], which is the
/// documented one and is written alongside this (#802).
pub fn add_trusted_folder(config_text: &str, folder: &str) -> Option<String> {
    let (comments, body) = split_jsonc_comment_header(config_text);
    let mut v: Value = if body.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(body).ok()?
    };
    let arr = v
        .as_object_mut()?
        .entry("trustedFolders")
        .or_insert_with(|| json!([]))
        .as_array_mut()?;
    if arr.iter().any(|e| e.as_str().is_some_and(|s| same_path_key(s, folder))) {
        return None;
    }
    arr.push(json!(folder));
    Some(format!("{comments}{}\n", serde_json::to_string_pretty(&v).ok()?))
}

thread_local! {
    /// Test seam for `pre_trust_copilot_folder`'s home resolution (#475), same
    /// contract and thread-scoping rationale as
    /// `COPILOT_SESSION_STATE_ROOT_OVERRIDE` in `sessions.rs`: a value set here
    /// can never leak into a concurrently-running test the way a process-wide
    /// `std::env::set_var` could. Checked BEFORE `COPILOT_HOME` — that's a
    /// genuine (pre-existing, non-test) production override, so a test using
    /// this hook must still win over a `COPILOT_HOME` the developer happens to
    /// have set locally. `None` (the default) means "use the real
    /// `COPILOT_HOME`/`~/.copilot`", so production behavior is unchanged.
    pub(in crate::orchestration) static COPILOT_TRUST_HOME_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only seam: fixture the directory `pre_trust_copilot_folder` reads and
/// writes its trust-grant `config.json` into, for the calling thread only.
/// Without this, a test that reached the trust-granting write would modify
/// the developer's REAL `~/.copilot/config.json` — see
/// `set_copilot_session_state_root_for_test` in `sessions.rs` for why a
/// thread-local beats a process-wide env var here.
#[doc(hidden)] // pub for integration tests
pub fn set_copilot_trust_home_for_test(home: Option<PathBuf>) {
    COPILOT_TRUST_HOME_OVERRIDE.with(|c| *c.borrow_mut() = home);
}

/// The write half of the trust grant, split from home RESOLUTION (#502) so
/// there is exactly one place that decides WHERE the config lives:
/// [`OrchRegistry::pre_trust_copilot_folder`], the sole entry point.
///
/// rev-38 round 3: this used to have a `pub` free sibling that resolved the
/// home directory itself, leaving "production must call the contained one"
/// to call-site discipline. Convention is not a mechanism — the sibling is
/// gone, and the containment rule is now unavoidable rather than merely
/// documented. `#475`'s tests drive the registry method, whose thread-local
/// seam still gives them the exact resolution they fixture.
///
/// Module-private on purpose: nothing outside this module should be able to
/// name a "write the trust grant HERE" primitive.
/// **Both writes go through [`atomic_write`], not `fs::write` (#803 review
/// B2).** These are the USER's files, not loomux's: `permissions-config.json`
/// holds every tool and directory approval they have ever granted copilot, in
/// any repo, and `config.json` holds copilot's own authentication state. A
/// plain `fs::write` truncates the destination before it writes, so a
/// disk-full or a crash mid-write leaves an empty or half-written file — and
/// the user silently loses the lot. That is the #133 failure verbatim (a
/// disk-full `fs::write` truncated `tasks.json` and destroyed the live board),
/// and it is strictly worse here because the data is not loomux's to lose and
/// loomux cannot regenerate it. `atomic_write` writes a same-directory temp,
/// fsyncs it, and renames it over the destination, so a failure leaves the
/// previous good file intact.
///
/// Still best-effort at the policy level: a failed grant means copilot prompts
/// as it did before, never a failed spawn. What changes is that a failure can
/// no longer be *destructive*.
pub(in crate::orchestration) fn pre_trust_copilot_folder_in(home: &Path, location: &str, folder: &str) {
    // The documented store (#802) — see `copilot_permissions_grant`.
    let perms_path = copilot_permissions_file(home);
    let perms_text = fs::read_to_string(&perms_path).unwrap_or_default();
    if let Some(updated) =
        copilot_permissions_grant(&perms_text, location, folder, MCP_SERVER)
    {
        // `atomic_write` creates the destination directory itself.
        let _ = atomic_write(&perms_path, updated.as_bytes());
    }
    // The pre-1.0.77 surface, kept because the current docs' silence about
    // `trustedFolders` is an absence, not a documented removal.
    let path = home.join("config.json");
    let text = fs::read_to_string(&path).unwrap_or_default();
    if let Some(updated) = add_trusted_folder(&text, folder) {
        let _ = atomic_write(&path, updated.as_bytes());
    }
}
