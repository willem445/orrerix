//! Gemini CLI's adapter: tool lists, flags, and the settings and policy files.
//! Design note: `docs/design/harness-adapters.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. Sibling files it
//! calls: `clis/mod.rs`.

use super::*;

/// Gemini CLI's built-in tool names, per the official
/// [Tools reference](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/tools.md)
/// ("Available tools", fetched as raw markdown via the GitHub contents API and
/// read off the per-category tables), verified 2026-08-01.
///
/// Same job and same limits as [`KNOWN_CLAUDE_TOOLS`]: it catches a typo or a
/// stale name left in [`GEMINI_EDIT_DENY_TOOLS`] (via
/// `gemini_edit_deny_tools_are_known_gemini_tools`), and it does NOT catch a
/// future gemini release renaming a tool out from under an unrefreshed
/// snapshot. That half needs a human to re-run the refresh.
///
/// Only the stable, non-experimental tools are listed — the task-tracker family
/// is documented as "experimental ... Enable via `experimental.taskTracker`",
/// and pinning names that only exist behind a flag would make the pin weaker,
/// not stronger. `complete_task` is likewise omitted: the reference marks it
/// "not available to the user".
///
/// Refresh procedure: re-read `docs/reference/tools.md` in
/// `google-gemini/gemini-cli` and update this list and the verified-on date.
#[doc(hidden)] // pub for integration tests
pub const KNOWN_GEMINI_TOOLS: &[&str] = &[
    "activate_skill", "ask_user", "enter_plan_mode", "exit_plan_mode", "get_internal_docs",
    "glob", "google_web_search", "grep_search", "list_directory", "list_mcp_resources",
    "read_file", "read_many_files", "read_mcp_resource", "replace", "run_shell_command",
    "search_file_content", "web_fetch", "write_file", "write_todos",
];

/// The file-editing tool names a gemini agent is denied once its class is
/// contained at all ([`Containment::denies_edits`]) — the gemini sibling of
/// [`CLAUDE_EDIT_DENY_TOOLS`] / [`COPILOT_EDIT_DENY_TOOLS`], one list for the
/// same reason (see the Claude one's doc).
///
/// `write_file` and `replace` are the two tools the Tools reference marks
/// `Kind: Edit`; `run_shell_command` is deliberately left alone, which is what
/// makes this [`Containment::NoEdits`] and not something stricter — a reviewer
/// runs the tests through the shell (#462).
///
/// **Delivered twice, on purpose** (see `write_gemini_settings`): as
/// policy-engine `deny` rules in a generated admin-tier TOML, and as
/// `tools.exclude` entries in the generated settings file. Not belt-and-braces
/// for its own sake — each layer covers the other's documented failure mode.
/// Supplemental admin policies "are **ignored** if any `.toml` policy files are
/// found in the standard system location", so on a machine with enterprise
/// policies installed the TOML silently does nothing; `tools.exclude` is not
/// subject to that guard. In the other direction `tools.exclude` is documented
/// as "deprecated in favor of policy rules with a `deny` decision", so it is
/// the layer with an expiry date. Either alone would be a containment that
/// fails silently and open.
///
/// Both layers outrank `--approval-mode yolo`: gemini's own policy config
/// gives the YOLO allow-all rule priority `998` in the **default** tier
/// (`1.998` after the tier transform), against `4.4` for a `tools.exclude`
/// deny and `5.x` for an admin-tier TOML deny
/// ([`policy/config.ts`](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/policy/config.ts),
/// `EXCLUDE_TOOLS_FLAG_PRIORITY = USER_POLICY_TIER + 0.4`, `ADMIN_POLICY_TIER
/// = 5`). So an unattended gemini reviewer is still a contained one.
pub const GEMINI_EDIT_DENY_TOOLS: &[&str] = &["write_file", "replace"];

/// The git-mutation shell commands a [`Containment::ReadOnly`] gemini agent
/// denies (a planner only — see [`CLAUDE_READONLY_DENY_GIT`] for why a reviewer
/// keeps these).
///
/// Spelled as command PREFIXES, not tool names: the policy engine's
/// `commandPrefix` field is documented as "a string or array of strings that a
/// shell command must start with ... syntactic sugar for `toolName =
/// "run_shell_command"` and an `argsPattern`", and `tools.exclude` accepts the
/// same narrowing through its legacy `toolName(args)` form, where the args are
/// "treated as a command prefix for the shell tool". One list, two spellings,
/// both generated from here.
pub const GEMINI_READONLY_DENY_GIT: &[&str] = &["git commit", "git push"];

/// Gemini's unattended posture, as one atom shared by the group spawn path
/// ([`OrchRegistry::build_agent_command`]) and the single-pane launcher
/// ([`single_pane_autopilot_flags`]), so the two can't drift (#101).
///
/// `--approval-mode yolo`, not the `--yolo` alias: the
/// [CLI reference](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/cli-reference.md)
/// marks `--yolo` **deprecated** in favour of it. "yolo" names an approval
/// mode, not a bypass — a contained agent's deny rules still outrank it (see
/// [`GEMINI_EDIT_DENY_TOOLS`] for the priority arithmetic).
pub const GEMINI_UNATTENDED_FLAGS: &str = "--approval-mode yolo";

/// Gemini's attended posture — the explicit spelling of its own default, so a
/// non-`auto_ops` group's flags say what they mean instead of relying on an
/// absent flag (the "explicit, not silent" convention the rest of this module
/// follows).
pub const GEMINI_ATTENDED_FLAGS: &str = "--approval-mode default";

/// The environment variable a gemini pane is spawned with so it reads the
/// per-agent settings file loomux generated for it — gemini's
/// *system*-settings path override.
///
/// Why this variable and not `GEMINI_CLI_HOME`: the home override relocates the
/// whole user-level config *and state* root, which includes the credentials the
/// human logged in with (`.gemini/oauth_creds.json`, `google_accounts.json`), so
/// a per-agent home would meet every agent with a login prompt. This one moves
/// exactly one file. It is documented for precisely this use — the
/// [enterprise guide](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/enterprise.md)
/// describes a wrapper script exporting it so "the enterprise settings are
/// always loaded with the highest precedence", which is the same shape as a
/// loomux-spawned pane, and it is not subject to the ownership checks the
/// standard system directory is.
///
/// System settings are the top settings tier (Default < User < Project <
/// System), so what loomux puts there wins over the user's own `~/.gemini`.
pub const GEMINI_SETTINGS_ENV: &str = "GEMINI_CLI_SYSTEM_SETTINGS_PATH";

/// The per-agent gemini settings file: the loomux MCP server, plus (for a
/// contained class) the `tools.exclude` layer of its deny rules and a pointer
/// at the admin-tier policy file that carries the other layer.
///
/// Pure and `pub` so the whole generated document is testable without a spawn —
/// this file *is* a gemini reviewer's containment and its MCP identity, so
/// "does it say what we think it says" must be assertable directly.
///
/// The MCP server is declared with `httpUrl` + `headers`, the streamable-HTTP
/// shape in gemini's `mcpServers` schema, carrying the same
/// agent token header every other CLI's config does — one server contract,
/// three spellings.
#[doc(hidden)] // pub for integration tests
pub fn gemini_settings_json(
    port: u16,
    token: &str,
    containment: Containment,
    policy: Option<&Path>,
) -> String {
    let mut cfg = json!({
        "mcpServers": one_server_map(json!({
            "httpUrl": format!("http://127.0.0.1:{port}/mcp"),
            "headers": agent_token_headers(token),
        }))
    });
    if containment.denies_edits() {
        let mut exclude: Vec<String> =
            GEMINI_EDIT_DENY_TOOLS.iter().map(|t| t.to_string()).collect();
        if containment.denies_git_mutation() {
            // The legacy `toolName(args)` narrowing form, where the args are
            // read as a shell command prefix — the `tools.exclude` spelling of
            // the same denial the policy file writes as `commandPrefix`.
            exclude.extend(
                GEMINI_READONLY_DENY_GIT.iter().map(|p| format!("run_shell_command({p})")),
            );
        }
        cfg["tools"] = json!({ "exclude": exclude });
    }
    if let Some(p) = policy {
        cfg["adminPolicyPaths"] = json!([p.display().to_string()]);
    }
    serde_json::to_string_pretty(&cfg).unwrap()
}

/// The per-agent gemini policy-engine document (#267) — the admin-tier deny
/// rules for a contained class. Pure and `pub` for the same reason as
/// [`gemini_settings_json`].
///
/// Hand-written TOML rather than a serializer: this is four fields in a
/// repeated table, every value comes from a loomux constant (no user text, so
/// nothing to escape), and adding a TOML dependency to `src-tauri` for it would
/// need the getrandom audit CLAUDE.md constraint 2 demands. Its shape is pinned
/// by tests against the documented schema.
///
/// `priority = 100` is arbitrary *within* the admin tier and does no work
/// against loomux's own rules (there is only one file); the tier is what makes
/// these beat the user's policies and `--approval-mode yolo`.
#[doc(hidden)] // pub for integration tests
pub fn gemini_policy_toml(containment: Containment) -> String {
    let mut out = String::from(
        "# Generated by loomux for one agent pane — do not edit.\n\
         # Loaded as a SUPPLEMENTAL ADMIN policy via the adminPolicyPaths setting\n\
         # in this agent's generated settings file, so these denials outrank the\n\
         # user's own policies and the yolo approval mode alike.\n",
    );
    if !containment.denies_edits() {
        return out;
    }
    for tool in GEMINI_EDIT_DENY_TOOLS {
        out.push_str(&format!(
            "\n[[rule]]\ntoolName = \"{tool}\"\ndecision = \"deny\"\npriority = 100\n"
        ));
    }
    if containment.denies_git_mutation() {
        for prefix in GEMINI_READONLY_DENY_GIT {
            out.push_str(&format!(
                "\n[[rule]]\ntoolName = \"run_shell_command\"\ncommandPrefix = \"{prefix}\"\n\
                 decision = \"deny\"\npriority = 100\n"
            ));
        }
    }
    out
}
