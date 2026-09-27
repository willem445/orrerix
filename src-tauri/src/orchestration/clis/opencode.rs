//! OpenCode's adapter: permissions, config JSON, pane environment, its database
//! location, and its session-id shape.
//! Design note: `docs/design/opencode.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. Sibling files it
//! calls: `clis/mod.rs`.

use super::*;

// ── OpenCode (#722) ────────────────────────────────────────────────────────
//
// Every claim below is verified against the CLI's own source at the version
// pin recorded in `docs/design/opencode.md` (its published docs do not cover
// the load-bearing parts), and that document carries the citations and the
// full containment argument. Kept here in the same shape as the gemini block
// above: literals in constants, the generated documents in pure functions, so
// what a contained pane is actually handed can be asserted without a spawn.

/// The permission KEY every file-modifying tool asks under — one key, not a
/// list of tool names, because opencode's permission engine is keyed on the
/// *permission* a tool requests rather than the tool's own name. `edit`,
/// `write` and `apply_patch` all request `"edit"`, and the CLI's own read-only
/// `plan` agent is built from exactly this one denial ("Plan mode. Disallows
/// all edit tools.").
///
/// The opencode sibling of [`CLAUDE_EDIT_DENY_TOOLS`] /
/// [`COPILOT_EDIT_DENY_TOOLS`] / [`GEMINI_EDIT_DENY_TOOLS`], and singular for
/// a real reason rather than an oversight: a list would invite exactly the
/// #448 failure those constants' docs describe — a name that matches nothing
/// reading like containment — and here there is no per-tool name to get wrong.
/// It is also why this tier is genuinely stronger than a name list: a
/// file-modifying tool opencode ships *tomorrow* asks the same key, so it is
/// denied with no loomux change (the fail-open direction #465 could only close
/// for claude, closed here by construction).
pub const OPENCODE_EDIT_DENY_PERMISSION: &str = "edit";

/// The git-mutation command patterns a [`Containment::ReadOnly`] opencode
/// agent denies under the `bash` permission — a planner only, for the reason
/// [`CLAUDE_READONLY_DENY_GIT`] gives.
///
/// No space before the `*`: opencode's matcher treats `*` as zero-or-more
/// characters against the whole command string, so `git commit*` covers a bare
/// `git commit` as well as `git commit -m …`, where `git commit *` would miss
/// the bare form.
pub const OPENCODE_READONLY_DENY_GIT: &[&str] = &["git commit*", "git push*"];

/// The `bash` patterns an ATTENDED opencode pane pre-approves, so the
/// branch→commit→PR flow and `gh` don't stop a human at every step while
/// everything else still asks. Kept as an atom because both the emitted
/// posture and its test read it.
///
/// The direction here is the opposite of every other CLI's: opencode's own
/// default is `"*": "allow"` — *more* permissive than any loomux attended
/// pane — so the generated posture NARROWS it rather than widening a
/// restrictive default.
pub const OPENCODE_ATTENDED_BASH_ALLOW: &[&str] = &["git *", "gh *"];

/// opencode's unattended posture, as one atom shared by the group spawn path
/// and [`single_pane_autopilot_flags`], so the two can't drift (#101).
///
/// `--auto` is the documented spelling: "auto-approve permissions that are not
/// explicitly denied". Its `--yolo` and `--dangerously-skip-permissions`
/// aliases are marked hidden in the CLI's own option table and are deliberately
/// not used. It is not a policy setting — it replies "allow once" to a
/// permission that was already going to be *asked*, and a `deny` rule never
/// raises an ask at all, which is exactly why a contained pane stays contained
/// under it.
pub const OPENCODE_UNATTENDED_FLAGS: &str = "--auto";

/// The inline config document (JSON) loomux hands an opencode pane.
///
/// **Why an env var and not a file path.** opencode merges its config sources
/// in a documented order, later winning per leaf key, and `OPENCODE_CONFIG`
/// (the custom-file variable) loads *before* the project's own
/// `opencode.json` and its `.opencode/` directories — so a repo could re-allow
/// what loomux denies. `OPENCODE_CONFIG_CONTENT` loads *after* all of them.
pub const OPENCODE_CONFIG_CONTENT_ENV: &str = "OPENCODE_CONFIG_CONTENT";

/// The permission-only override, applied LAST — after the org, managed-config
/// and MDM ranks that land *after* the inline document above.
///
/// Set alongside [`OPENCODE_CONFIG_CONTENT_ENV`], never instead of it, and
/// that pairing is the containment guarantee rather than belt-and-braces for
/// its own sake. It closes two holes the config document alone cannot: an
/// account/managed/MDM config outranking the inline one, and — the sharper
/// case — an `--agent` that fails to resolve, which does **not** error but
/// prints a warning and falls back to `build`, the most permissive agent
/// there is. A reviewer degrading into a full-write agent over a typo, with
/// only a scrollback line to show for it, is precisely the silent failure
/// #462's guarantee cannot survive; this variable applies globally, so it
/// survives that fallback.
pub const OPENCODE_PERMISSION_ENV: &str = "OPENCODE_PERMISSION";

/// Suppress the CLI's boot-time self-update. The copilot `--no-auto-update`
/// hazard exactly: a mid-boot update restarts the CLI and flushes anything
/// typed into the first instance, i.e. the kickoff.
///
/// The env var, not the config document's `autoupdate` key — an environment
/// variable cannot be overridden by a config rank loomux does not control.
pub const OPENCODE_DISABLE_AUTOUPDATE_ENV: &str = "OPENCODE_DISABLE_AUTOUPDATE";

/// Skip the repo's own `opencode.json` and `.opencode/` directories entirely
/// — set for a CONTAINED pane only ([`Containment::denies_edits`]).
///
/// For a worker the repo's config is legitimate material (its commands, its
/// own MCP servers) and loomux's document already wins the merge, so it stays
/// loaded. For a reviewer or a planner the calculus flips: "loomux's rules win
/// the merge" is a claim about merge order, while "the repo's rules never load"
/// is a claim about nothing at all — and a contained pane is the one place
/// worth paying a repo's custom commands for the simpler story.
pub const OPENCODE_DISABLE_PROJECT_CONFIG_ENV: &str = "OPENCODE_DISABLE_PROJECT_CONFIG";

/// Point this pane's session/message store at a database under the group's own
/// state dir instead of the shared per-user one.
///
/// Absolute paths are honored as-is (a relative value would resolve under the
/// CLI's own data root). Two reasons, both structural: the human's own
/// opencode sessions stay out of a group's store — and a group's store stays
/// out of theirs — and "the newest session in this database" becomes an
/// unambiguous question, which is what session identification needs on a CLI
/// that cannot be handed a session id up front. The reader itself is a later
/// slice; this is the seam it lands on.
pub const OPENCODE_DB_ENV: &str = "OPENCODE_DB";

/// Where [`OPENCODE_DB_ENV`] points, relative to a group's state dir. A
/// directory of its own because SQLite writes `-wal`/`-shm` siblings.
pub(in crate::orchestration) const OPENCODE_DB_SUBDIR: &str = "opencode";
pub(in crate::orchestration) const OPENCODE_DB_FILE: &str = "opencode.db";

/// Timeout (ms) on the loomux MCP entry, raised from the documented 5000ms
/// default: loomux's own tools do real work behind a call (`report` writes
/// state and audits, `notify_when` registers a watch), and a tool call timing
/// out reads to an agent as the tool being broken.
const OPENCODE_MCP_TIMEOUT_MS: u64 = 30_000;

/// A path as opencode's config document must spell it: forward slashes.
///
/// `{file:…}` references are substituted **textually, on the raw string,
/// before the document is parsed** — so a Windows path's backslashes are not
/// JSON-unescaped on the way through, and a JSON-escaped `C:\\Users\\…` would
/// reach the filesystem with its separators doubled. Windows accepts forward
/// slashes everywhere loomux needs them, so this side-steps the question
/// entirely rather than betting on doubled separators resolving.
fn opencode_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// The **global** permission posture for one opencode pane — the object that
/// rides both the config document and [`OPENCODE_PERMISSION_ENV`].
///
/// Two rules govern its shape, and both come from how the engine evaluates:
///
/// - **Last matching rule wins**, matching by wildcard on the permission key
///   as well as the pattern. So this never emits a `"*"` KEY: a `"*"` key
///   matches every permission, and — since rules are emitted in key order —
///   one sitting after the specific denials would silently re-allow them.
///   Narrow keys only, and the denials are ordered after the allows within
///   their own key (`"*"` sorts before `"git *"` sorts before `"git commit*"`).
/// - **A `deny` short-circuits before any prompt**, so no ask is ever raised
///   for it and `--auto` — which only answers asks — cannot reach it.
///
/// The `edit: "deny"` scalar is the strong form on purpose: it becomes a rule
/// with the `*` pattern, and the CLI drops a tool from the model's toolset
/// entirely when the last matching rule for its permission is a `*`-pattern
/// deny. So a contained pane does not merely fail to edit; it is never offered
/// the tool.
#[doc(hidden)] // pub for integration tests
pub fn opencode_permission_json(containment: Containment, unattended: bool) -> Value {
    let mut bash = serde_json::Map::new();
    if unattended {
        bash.insert("*".into(), json!("allow"));
    } else {
        // Attended: narrow opencode's allow-everything default back to "ask",
        // then pre-approve the flows a human would otherwise be interrupted
        // for on every step.
        bash.insert("*".into(), json!("ask"));
        for pat in OPENCODE_ATTENDED_BASH_ALLOW {
            bash.insert((*pat).to_string(), json!("allow"));
        }
    }
    if containment.denies_git_mutation() {
        for pat in OPENCODE_READONLY_DENY_GIT {
            bash.insert((*pat).to_string(), json!("deny"));
        }
    }
    let mut perm = serde_json::Map::new();
    perm.insert("bash".into(), Value::Object(bash));
    perm.insert(
        OPENCODE_EDIT_DENY_PERMISSION.to_string(),
        if containment.denies_edits() { json!("deny") } else if unattended { json!("allow") } else { json!("ask") },
    );
    // The group's state dir — where every agent's role-instruction file lives
    // — is outside the pane's worktree, and this key defaults to `ask`, so
    // without it an unattended pane would stall on its own instructions.
    //
    // **The blanket form, not `{"*": "ask", "<group dir>/*": "allow"}`, and
    // the narrow one would be a REGRESSION rather than a tightening.** Rules
    // are concatenated defaults-then-user and the last match wins, so a user
    // `"*": "ask"` lands AFTER opencode's own default whitelist for this key
    // (its temp dir, its skill dirs, its reference dirs) and demotes every one
    // of them to `ask` — turning the CLI's own internal machinery into
    // prompts. Re-listing those dirs here would mean hardcoding another
    // vendor's internal paths, which ages exactly as badly as a model table
    // (#329). What the breadth actually costs is one prompt on an ATTENDED
    // pane before the agent reads outside its worktree; `read` is not a tier
    // loomux contains at on any CLI (see [`Containment`]: these are denials of
    // named tools, never a filesystem sandbox), so no guarantee rests on it.
    perm.insert("external_directory".into(), json!("allow"));
    // Restore the `question` tool for a pane that HAS a human in it, and only
    // then. opencode's built-in defaults deny it and its default `build` agent
    // re-allows it — a config-declared agent (which is what every loomux pane
    // runs) inherits the defaults, so without this an attended worker's
    // "should I do X?" would come back as a permission error instead of a
    // question. The other direction is just as deliberate: an unattended pane
    // has nobody to answer, and a question it cannot get an answer to is a
    // stall, which is the deadlock `forces_unattended` exists to prevent.
    perm.insert("question".into(), if unattended { json!("deny") } else { json!("allow") });
    Value::Object(perm)
}

/// The **agent-level** denials for a contained class, or `None` for an
/// uncontained one (there is nothing to deny).
///
/// A separate, strictly-later ruleset than the global posture above — the
/// engine concatenates a config-declared agent's rules after the global ones
/// — which is what makes containment independent of key order *between* the
/// two objects. Same denials, said twice, for the same reason gemini's are
/// (`GEMINI_EDIT_DENY_TOOLS`): each layer covers the other's failure mode.
/// This one survives a global posture an outranking config rank overwrote;
/// the global one ([`OPENCODE_PERMISSION_ENV`]) survives `--agent` failing to
/// resolve this entry at all.
fn opencode_agent_permission_json(containment: Containment) -> Option<Value> {
    if !containment.denies_edits() {
        return None;
    }
    let mut perm = serde_json::Map::new();
    if containment.denies_git_mutation() {
        // DENIALS ONLY — no `"*": "allow"` alongside them. The rest of the
        // shell is already allowed by the global posture this ruleset is
        // concatenated after, so restating it here would only risk stating it
        // differently, and a rule that widens is not what an agent-level block
        // is for.
        let mut bash = serde_json::Map::new();
        for pat in OPENCODE_READONLY_DENY_GIT {
            bash.insert((*pat).to_string(), json!("deny"));
        }
        perm.insert("bash".into(), Value::Object(bash));
    }
    perm.insert(OPENCODE_EDIT_DENY_PERMISSION.to_string(), json!("deny"));
    Some(Value::Object(perm))
}

/// The whole generated config document for one opencode pane: its loomux MCP
/// server, its share posture, its permission posture, and — when the block's
/// contract was written to a file — its persona as a native agent entry.
///
/// Pure and `pub` so the document can be asserted directly: this file *is* an
/// opencode agent's MCP identity and half of its containment, so "does it say
/// what we think it says" must be answerable without a spawn.
///
/// `agent` is `(handle, prompt_file)`. The handle is namespaced
/// ([`generated_agent_handle`], `loomux-<group>-<block>`) because the merge
/// that lets loomux's entry win a same-name collision with a repo's own
/// `.opencode/agents/*.md` is a DEEP merge: a colliding file would keep every
/// key loomux left unset. A handle a repo cannot guess makes the collision
/// impossible rather than survivable — and every field loomux cares about is
/// emitted explicitly regardless.
#[doc(hidden)] // pub for integration tests
pub fn opencode_config_json(
    port: u16,
    token: &str,
    containment: Containment,
    unattended: bool,
    agent: Option<(&str, &Path)>,
) -> String {
    let mut cfg = json!({
        // No CLI flag exists for MCP servers; this key is the only channel.
        // `oauth: false` because OAuth auto-detection is on by default and the
        // loomux server authenticates by header — a 401 during discovery must
        // not send opencode down a flow this server does not speak.
        "mcp": one_server_map(json!({
            "type": "remote",
            "url": format!("http://127.0.0.1:{port}/mcp"),
            "enabled": true,
            "headers": agent_token_headers(token),
            "oauth": false,
            "timeout": OPENCODE_MCP_TIMEOUT_MS,
        })),
        // A group agent's session is never published to a share link.
        "share": "disabled",
        "permission": opencode_permission_json(containment, unattended),
    });
    if let Some((handle, prompt)) = agent {
        let mut entry = json!({
            // ALWAYS explicit: an entry that resolved to a subagent mode would
            // be refused by `--agent` and fall back to `build` with only a
            // warning line — the same silent widening
            // `OPENCODE_PERMISSION_ENV` exists to survive.
            "mode": "primary",
            // The contract by FILE, never on argv: it is many KB, and Windows
            // CreateProcessW's 32,767-character command-line limit is a real,
            // demo-blocking bug once a role contract rides a flag (#417).
            "prompt": format!("{{file:{}}}", opencode_path(prompt)),
        });
        if let Some(perm) = opencode_agent_permission_json(containment) {
            entry["permission"] = perm;
        }
        let mut agents = serde_json::Map::new();
        agents.insert(handle.to_string(), entry);
        cfg["agent"] = Value::Object(agents);
    }
    serde_json::to_string_pretty(&cfg).unwrap()
}

/// The pane environment that delivers everything above — opencode names none
/// of it on argv.
///
/// Pure, so the mapping is assertable without a spawn, and separate from
/// [`OrchRegistry::agent_pane_env`] for the reason [`cli_extra_env`] is: that
/// one returns nothing at all when the gh/git shims can't be written, and an
/// opencode agent silently losing its MCP identity and its containment because
/// git wasn't installed would be a confusing failure a long way from its cause.
///
/// **Caller contract:** `containment`/`unattended` must be the same pair that
/// produced `config_json`. The posture is deliberately restated here rather
/// than lifted out of the document (a parse that could fail would have to fail
/// *open*, dropping the very override that exists to survive everything else),
/// so the agreement is a call-site obligation — pinned end-to-end by
/// `an_opencode_spawn_delivers_its_config_and_containment_by_env`, which
/// asserts the document's `permission` and this override are one object.
#[doc(hidden)] // pub for integration tests
pub fn opencode_pane_env(
    config_json: &str,
    containment: Containment,
    unattended: bool,
    db: &Path,
) -> Vec<(String, String)> {
    let mut env = vec![
        (OPENCODE_CONFIG_CONTENT_ENV.to_string(), config_json.to_string()),
        (
            OPENCODE_PERMISSION_ENV.to_string(),
            opencode_permission_json(containment, unattended).to_string(),
        ),
        (OPENCODE_DISABLE_AUTOUPDATE_ENV.to_string(), "1".to_string()),
        (OPENCODE_DB_ENV.to_string(), opencode_path(db)),
    ];
    if containment.denies_edits() {
        env.push((OPENCODE_DISABLE_PROJECT_CONFIG_ENV.to_string(), "1".to_string()));
    }
    env
}

/// Is `s` an opencode session id in full (#722)?
///
/// `ses_` + 12 lowercase hex (a 6-byte timestamp) + 14 base62, 30 characters
/// in all (`SOURCE`, `id.ts`). Recognized by *shape* because opencode's ids are
/// shorter than a claude UUID: without this, [`is_full_session_id`] would read
/// a complete opencode id as a truncated prefix and refuse it as an unknown
/// session — see there for why that matters.
///
/// Compared character by character rather than by byte-slicing at 12: a
/// caller-supplied string is not guaranteed ASCII, and `&rest[..12]` on one
/// that is not would panic on a char boundary. This is fed by an MCP argument,
/// so "no caller can get here with such a string" is not a claim worth betting
/// a panic on.
pub(in crate::orchestration) fn is_opencode_session_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("ses_") else {
        return false;
    };
    let mut chars = rest.chars();
    let stamp_ok = chars
        .by_ref()
        .take(12)
        .filter(|c| c.is_ascii_digit() || ('a'..='f').contains(c))
        .count()
        == 12;
    let tail: Vec<char> = chars.collect();
    stamp_ok && tail.len() == 14 && tail.iter().all(|c| c.is_ascii_alphanumeric())
}
