//! Pi's adapter: flags, the sessions store and session-file parsing, and its
//! MCP config and repo MCP exposure.
//! Design note: `docs/design/pi.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. IO: fs. Sibling
//! files it calls: `clis/mod.rs`.

use super::*;

// ── pi (#2126) ─────────────────────────────────────────────────────────────
//
// Every claim below is verified against the vendors' own source and docs at
// the version pins recorded in `docs/design/pi.md` — pi itself, and separately
// the community `pi-mcp-adapter` extension that gives pi MCP at all. That
// document carries the citations, the containment argument and the residuals.
// Same shape as the gemini and opencode blocks above: literals in constants,
// generated documents in pure functions, so what a pane is actually handed can
// be asserted without a spawn.

/// The two built-in tools a [`Containment::denies_edits`] pi pane is denied,
/// as one `--exclude-tools` value.
///
/// pi's built-ins are `read, bash, powershell (Windows), edit, write, grep,
/// find, ls`, and `--exclude-tools` "Disable specific built-in, extension, and
/// custom tools" is applied AFTER every allowlist — so this is the same
/// deny-beats-allow property claude's `--disallowedTools` and opencode's
/// `edit: deny` have, reached a third way.
///
/// A NAME list rather than a permission key, which is why the #448 hazard the
/// `*_EDIT_DENY_TOOLS` constants warn about applies here in full: a
/// file-modifying tool pi ships tomorrow under a third name is NOT denied by
/// this, and nothing goes red to say so. That is a stated property of pi's
/// containment ceiling, not an oversight — see `CLI_CAPS`' pi row.
pub const PI_EDIT_DENY_TOOLS: &str = "edit,write";

/// Trust the project folder for this run. Every group spawn carries this or
/// [`PI_NO_APPROVE_FLAG`], so pi's one boot dialog can never appear on a pane
/// loomux is about to type a kickoff into.
///
/// The dialog it forecloses ("Trust project folder?") is raised when the cwd
/// carries any of `.pi/{settings.json,extensions,skills,prompts,themes,
/// SYSTEM.md,APPEND_SYSTEM.md}`, or a `.agents/skills` directory in the cwd or
/// any parent — and only when no decision is already saved in
/// `~/.pi/agent/trust.json`. A repo with none of those raises no dialog at
/// all, which is why this flag's effect is invisible on most repos and
/// load-bearing on the one that has them.
///
/// The UNCONTAINED classes take this one: a repo's own extensions, skills and
/// prompts are legitimate worker material, exactly as opencode's project
/// config is left loaded for a worker.
pub const PI_APPROVE_FLAG: &str = "--approve";

/// Ignore project-local pi resources for this run — the CONTAINED classes'
/// half of the pair above.
///
/// The calculus flips for a reviewer for the reason
/// [`OPENCODE_DISABLE_PROJECT_CONFIG_ENV`] gives: a repo's `.pi/extensions`
/// could register a file-writing tool under a name `--exclude-tools edit,write`
/// does not mention, and "the repo's resources never load" is a much simpler
/// claim than "loomux's denials outrank them". A contained pane is the one
/// place worth paying a repo's custom skills for the simpler story.
///
/// User- and global-level extensions — the MCP adapter included — load either
/// way; this is a PROJECT-local switch only.
pub const PI_NO_APPROVE_FLAG: &str = "--no-approve";

/// pi's unattended posture: **nothing**, and that is a measured claim rather
/// than an unfilled blank.
///
/// pi has no permission prompts to bypass. Its own design principles say so
/// outright — "It intentionally does not include built-in MCP, sub-agents,
/// permission popups, plan mode…" — so there is no `--auto`, no `--yolo`, no
/// `--approval-mode` and nothing for an autopilot toggle to turn on. An
/// ATTENDED pi pane already runs every tool without asking, which makes the
/// attended and unattended launch lines byte-identical (pinned by
/// `pi_launch_flags_per_posture`) and makes the group's `auto_ops` toggle a
/// genuine no-op on this CLI.
///
/// Kept as a named atom anyway, for the #101 invariant the other three CLIs
/// rest on: the launcher toggle and the group spawn must mean the same thing
/// on every CLI loomux can spawn, and the only way to say "they agree on
/// nothing" without two independent empty strings is one shared empty one.
///
/// NOT to be confused with [`PI_APPROVE_FLAG`], which is about the folder-trust
/// dialog and rides EVERY group line regardless of posture.
pub const PI_UNATTENDED_FLAGS: &str = "";

/// Suppress pi's boot-time latest-version request to `pi.dev`.
///
/// The narrow variable, not `PI_OFFLINE`: the broad one would also cut off
/// whatever else pi reaches the network for, and the only thing loomux wants
/// gone is a startup request whose failure or slowness sits between the pane
/// appearing and the kickoff landing.
pub const PI_SKIP_VERSION_CHECK_ENV: &str = "PI_SKIP_VERSION_CHECK";

/// Where a group's pi sessions live, relative to the group's state dir — a
/// sibling of opencode's `opencode/`.
///
/// Per-GROUP rather than per-agent, and that is enough precisely because ids
/// are pre-minted: a session is located by an exact `_<id>.jsonl` filename
/// suffix in one directory, so a second agent's file in the same directory is
/// not an ambiguity. Per-agent would buy nothing and cost a directory per
/// pane.
///
/// The point of moving it off the human's own store at all is the same one
/// [`OPENCODE_DB_ENV`] makes: a group's sessions stay out of the human's
/// `pi --resume` list, and their sessions stay out of the group's.
const PI_SESSIONS_SUBDIR: &str = "pi";
const PI_SESSIONS_LEAF: &str = "sessions";

/// [`OrchRegistry::pi_sessions_dir`] against an already-resolved group state
/// directory — the one derivation of this path in the codebase.
///
/// Free rather than a method because the launch-line builders receive a
/// `group_dir: &Path` and have no [`GroupId`] in scope, and two independent
/// spellings of the directory a pane WRITES its session to and the directory
/// a resume LOOKS in is the disagreement that would present as "this pane
/// cannot be resumed" with nothing red to say why.
pub fn pi_sessions_in(group_dir: &Path) -> PathBuf {
    group_dir.join(PI_SESSIONS_SUBDIR).join(PI_SESSIONS_LEAF)
}

/// Whether a CLI's session store, as loomux configures it, lives under the
/// GROUP's own state dir rather than in a per-user location shared by every
/// group on the machine.
///
/// Two consumers, and they are the reason this is a function rather than a
/// condition written twice: [`session_cwd_in_store`] needs the group's store
/// path handed to it for these CLIs and would otherwise silently search a
/// per-user root, and `orch_list_recorded` must keep them OFF [`StoreIndex`],
/// whose whole premise (#1592) is amortising ONE enumeration of a big shared
/// store across many groups. A group-local store is already O(1) per group and
/// has nothing to share, so indexing it would be strictly more work and would
/// answer for the wrong group besides.
///
/// A by-name match, deliberately, and it is not the "re-derived at a call
/// site" shape `CliCaps` warns about: it is derived ONCE, here, and asked by
/// everything that cares. It is not a `CliCaps` field because it is a fact
/// about how LOOMUX points the CLI (`OPENCODE_DB`, `--session-dir`), not a
/// capability of the vendor's.
pub fn group_local_session_store(cli: &str) -> bool {
    matches!(cli, "opencode" | "pi")
}

/// The file a pi session wrote itself to inside a group's own pi store
/// (#2126) — the ONE derivation of "which file is this session", shared by the
/// resume path ([`pi_session_cwd_in_dir`]) and the usage meter
/// ([`crate::usage::pi_session_usage_in`]).
///
/// `dir` is [`OrchRegistry::pi_sessions_dir`]: one flat directory holding
/// `<timestamp>_<uuid>.jsonl` for every pane in the group, because pi writes
/// directly under a `--session-dir` with no per-cwd subdirectory of its own.
///
/// **Matched on an exact `_<id>.jsonl` filename SUFFIX, never a prefix or a
/// `contains`.** pi's ids are loomux-minted UUIDs, so a prefix match would be
/// harmless today and wrong the first time an id is a prefix of another — and
/// the leading `_` is what makes the suffix unambiguous against a timestamp
/// that happens to end in the same digits.
///
/// **`Ok(None)` for an absent directory**, matching opencode's `Absent` rule:
/// a group whose panes have not written a session yet has no such session,
/// which is "not found in the history" rather than a store failure worth
/// telling the caller to investigate. That case is ordinary rather than
/// exotic — pi defers creating the file to the first assistant response, so a
/// pane that was spawned and never prompted has no file at all.
///
/// Takes a [`PathSegment`], not a `&str`, for the reason `claude_transcript_path`
/// does (#925): the id is interpolated into a filename this process then matches
/// against a directory it did not choose, so proof belongs at the signature
/// rather than in each caller's memory. `src-tauri/tests/pathseg.rs` names this
/// as the declared assembly point for a pi session file.
#[doc(hidden)] // pub for integration tests
pub fn pi_session_file_in_dir(
    dir: &Path,
    session: &PathSegment,
) -> Result<Option<PathBuf>, String> {
    let suffix = format!("_{session}.jsonl");
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(&suffix) {
            continue;
        }
        return Ok(Some(entry.path()));
    }
    Ok(None)
}

/// Where a pi session recorded that it ran, read from a group's own pi store
/// (#2126) — the pi half of [`session_cwd_in_store`].
///
/// A matched file with no readable header is `Ok(Some(""))`, the same
/// "recorded, but recorded no working directory" answer `resolve_resume_cwd`
/// already distinguishes from "not found"; an absent file or an absent
/// directory is `Ok(None)`, per [`pi_session_file_in_dir`].
#[doc(hidden)] // pub for integration tests
pub fn pi_session_cwd_in_dir(dir: &Path, session_id: &str) -> Result<Option<String>, String> {
    // The same admission `find_session_cwd` applies before it will touch a
    // store at all (#925): an id that is not a single path component cannot
    // name a file here, so it is "not found" rather than a lookup.
    let Ok(seg) = PathSegment::parse(session_id) else { return Ok(None) };
    let Some(path) = pi_session_file_in_dir(dir, &seg)? else { return Ok(None) };
    // The header is pi's FIRST line — `{"type":"session","version":3,
    // "id":…,"cwd":…}` — and only a BOUNDED PREFIX of the file is read to
    // get it, never the whole transcript.
    //
    // The bound is load-bearing rather than tidy, because of WHO calls
    // this. `orch_list_recorded`'s `resumable` check asks it once per pi
    // group on every session-browser refresh — the boot prefetch, each
    // sidebar open, each resume settle — and a pi transcript is
    // append-only and unbounded (one line per turn). Reading the whole
    // file to answer a question the first line answers made the cost of a
    // listing scale with how much WORK a group had done, which is the
    // wrong axis entirely (rev-std round 1, finding 2). The earlier
    // version's doc comment claimed this bound while `read_to_string`
    // quietly took the lot.
    //
    // The read itself is `loomux_engine::sessions::bounded_first_line` (#2515
    // C2), which is where those mechanics and that bound's rationale now live.
    // It is shared with codex's header read rather than copied, because the two
    // CLIs are asking the identical question of an identical file shape — an
    // append-only JSONL whose first line is the metadata — and two copies of
    // "read one bounded line" is two places for the bound to be forgotten.
    //
    // A file that exists but is empty (created and not yet written
    // through) reads as "no recorded cwd", not as absent: the session id
    // is real, its workspace is merely unknown.
    let cwd = loomux_engine::sessions::bounded_first_line(&path, PI_SESSION_HEADER_MAX_BYTES)
        .and_then(|line| serde_json::from_str::<Value>(line.trim_end()).ok())
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("session"))
        .and_then(|v| v.get("cwd").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    Ok(Some(cwd))
}

/// How much of a pi session file [`pi_session_cwd_in_dir`] will read looking
/// for its header line.
///
/// The header is the first line and is a few hundred bytes in practice, so
/// this is not a budget anyone is expected to spend — it is the ceiling for
/// the one case where `read_line` would otherwise not stop: a corrupt or
/// mid-write file with no newline in it at all, where "the first line" and
/// "the whole transcript" are the same thing.
///
/// Generous on purpose (constraint 8 — nothing here is tuned to this repo's
/// own transcripts). It fails toward "no recorded cwd" rather than toward a
/// wrong one: a truncated read cannot parse as JSON, so the session still
/// lists and still says its workspace is unknown, which is the answer a
/// header-less file already gets.
const PI_SESSION_HEADER_MAX_BYTES: u64 = 64 * 1024;

/// The filename extension of the contract file a pi block's
/// `--append-system-prompt` points at.
///
/// `.pi.md` rather than opencode's bare `.md` so the two never collide in the
/// one `configs/` directory they share: `generated_agent_handle` is
/// `loomux-<group>-<block>`, which is the SAME handle for a block whose `cli:`
/// changed between two launches of one group.
pub(in crate::orchestration) const PI_CONTRACT_FILE_EXT: &str = ".pi.md";

/// Timeout (ms) on the loomux MCP entry in a pi config, raised from the
/// adapter's own default for the reason [`OPENCODE_MCP_TIMEOUT_MS`] gives:
/// loomux's tools do real work behind a call, and one timing out reads to an
/// agent as the tool being broken.
const PI_MCP_TIMEOUT_MS: u64 = 30_000;

/// The two repo-authored files pi's MCP adapter merges into a pane's tool
/// surface, relative to the pane's own working directory.
///
/// Named as data rather than matched inline because [`pi_repo_mcp_exposure`]
/// reports them and its fixture pins them; a third one the adapter grows is
/// then a row here.
const PI_REPO_MCP_FILES: &[&str] = &[".mcp.json", ".pi/mcp.json"];

/// The MCP server name in ONE pi agent's generated config — `<loomux>-<agent
/// id>` rather than the bare [`MCP_SERVER`] every other CLI uses.
///
/// **Per-agent on purpose, and free only here.** pi's adapter merges its
/// config sources and resolves a collision by server NAME, later source
/// winning — and the repo's own `.mcp.json` / `.pi/mcp.json` are LATER than
/// the file loomux names on `--mcp-config` (`docs/design/pi.md`, "Why the
/// bridge is not exclusive"). A repo declaring a server called `orrerix`
/// would therefore REPLACE loomux's entry outright, and the pane would boot
/// with no orrerix tools, or with something else's. An id a repo cannot guess
/// removes that route.
///
/// It costs nothing on pi and would cost something on every other CLI, which
/// is why `one_server_map`'s "one name, so the file and the argv cannot drift"
/// argument is untouched: claude, copilot and gemini all SPELL the server name
/// on argv (`--allowedTools mcp__<server>`, `--allow-tool <server>`,
/// `--allowed-mcp-server-names <server>`), and pi spells it nowhere. With
/// `toolPrefix: "none"` the name does not reach a tool name either, so the
/// role templates' bare `report(...)` is unaffected.
///
/// Side effect worth naming: the adapter's direct-tool metadata cache is keyed
/// by server name, so a per-agent name also means a pane never inherits the
/// tool list a DIFFERENT role's pane cached under one shared name.
///
/// `agent` is a [`PathSegment`] (#925) — the same proof `write_mcp_config`
/// already takes before the id becomes a file name — so the result cannot
/// carry a separator or a character JSON would have to escape.
#[doc(hidden)] // pub for integration tests
pub fn pi_server_name(agent: &PathSegment) -> String {
    format!("{MCP_SERVER}-{agent}")
}

/// The generated MCP config document for one pi pane — the file named on
/// `--mcp-config`, read by the `pi-mcp-adapter` extension the human installs.
///
/// Pure and `pub` so the whole document is assertable without a spawn: this
/// file IS a pi agent's orrerix identity, so "does it say what we think it
/// says" must be answerable directly.
///
/// Every key is checked against the adapter's own `readValidatedConfig` /
/// `validateConfig` at the pin in `docs/design/pi.md`: the document is
/// `{ mcpServers, imports?, settings? }`, `mcpServers` is a name→entry map and
/// an entry is accepted as any JSON object, so the per-entry keys below are
/// read by the runtime rather than the validator.
///
/// - `url` + `headers` — StreamableHTTP with an SSE fallback, carrying the
///   same agent-token header every other CLI's config does. A reconnect
///   re-`initialize`s with the same token, so it comes back as the same
///   `Caller` and the same pane identity.
/// - `lifecycle: "keep-alive"` — connect at startup and stay connected,
///   instead of the lazy default, so the kickoff's first `report` pays no
///   connect latency and the direct-tool registry is reconciled from a live
///   `tools/list` before the first status snapshot.
/// - `directTools: true` — register every tool of this server individually,
///   rather than behind the adapter's own proxy tool. loomux's largest role
///   surface is well under the adapter's 75-tool advisory.
/// - `toolPrefix: "none"` — bare tool names, which is what the role templates
///   spell (`report(...)`, never a prefixed form). `brand::MCP_TOOL_PREFIX` is
///   a claude ALLOWLIST fact, not a template fact.
/// - `settings.disableProxyTool` / `scriptMode` — drop the adapter's own two
///   tools once direct tools are up; nothing loomux does asks for them.
///
/// No `type` key: that is a claude-shaped field the adapter reads only through
/// its compatibility importer, and stating it here would be a claim about a
/// schema this document is not written in. `protocolVersion` is left at the
/// adapter's default, which is the handshake `mcp.rs` already echoes back.
#[doc(hidden)] // pub for integration tests
pub fn pi_mcp_config_json(port: u16, token: &str, server_name: &str) -> String {
    let mut servers = serde_json::Map::new();
    servers.insert(
        server_name.to_string(),
        json!({
            "url": format!("http://127.0.0.1:{port}/mcp"),
            "headers": agent_token_headers(token),
            "lifecycle": "keep-alive",
            "requestTimeoutMs": PI_MCP_TIMEOUT_MS,
            "directTools": true,
            "toolPrefix": "none",
        }),
    );
    let cfg = json!({
        "mcpServers": Value::Object(servers),
        "settings": {
            "disableProxyTool": true,
            "scriptMode": false,
            "notifyOnStartupConnect": true,
            "mcpFooterStatus": "full",
        },
    });
    serde_json::to_string_pretty(&cfg).unwrap()
}

/// What a pane's own repo declares that pi's MCP adapter will MERGE into that
/// pane's tool surface — measured, reported once, and never refused.
///
/// **Why this exists at all.** pi has no exclusive-config seam loomux can
/// reach: the adapter's `PI_MCP_CONFIG_MODE=exclusive` DISCARDS the
/// `--mcp-config` override and reads one fixed per-user file instead, so a
/// per-agent config and exclusivity are mutually exclusive at the pin (see
/// `docs/design/pi.md`, which cites the two lines). loomux takes the per-agent
/// config, which means the repo's own MCP files are merged in — repo-authored
/// input, in a threat model where the repo is the thing under review.
///
/// **Measure and warn, never refuse** (the generic-product rule, constraint
/// 8): a repo declaring its own MCP servers is a legitimate, common thing, and
/// loomux is not in a position to adjudicate it. What loomux CAN do is say, in
/// the audit trail, exactly what it saw — so a human debugging a pane whose
/// `report` went somewhere strange has the row rather than a mystery.
///
/// **What it can and cannot see, stated because the gap matters.** A server
/// NAME collision is fully detectable and is the sharp case: the later source
/// wins, so a repo entry named the same as this agent's server REPLACES it. A
/// TOOL-name collision is only detectable where the repo pins `directTools` to
/// an explicit list of names; an entry with `directTools: true` advertises its
/// names only at connect time, and loomux never connects to a repo's server to
/// find out. So an empty `tools` list here is "nothing statically visible",
/// never "nothing there" — the same absence-is-not-proof line the residual in
/// the design note is written on.
///
/// `None` when the repo declares nothing — the overwhelmingly common case, and
/// the one that must cost no audit row at all.
#[doc(hidden)] // pub for integration tests
pub fn pi_repo_mcp_exposure(
    workdir: &Path,
    server_name: &str,
    loomux_tools: &std::collections::BTreeSet<String>,
) -> Option<Value> {
    let mut files = Vec::new();
    for rel in PI_REPO_MCP_FILES {
        let path = workdir.join(rel);
        let Ok(body) = fs::read_to_string(&path) else { continue };
        // A file that is present but unparsable is still REPORTED: the adapter
        // would skip it with a console warning, so "loomux saw a file here and
        // could not read it" is the honest row — and silently dropping it is
        // how a real exposure comes to look like an absent one.
        let parsed: Option<Value> = serde_json::from_str(&body).ok();
        let servers = parsed
            .as_ref()
            .and_then(|v| v.get("mcpServers").or_else(|| v.get("mcp-servers")))
            .and_then(Value::as_object);
        let mut names: Vec<String> = Vec::new();
        let mut tools: Vec<String> = Vec::new();
        if let Some(map) = servers {
            for (name, entry) in map {
                names.push(name.clone());
                // Only an explicit NAME LIST is readable here — see this
                // function's doc for why `directTools: true` is not.
                if let Some(list) = entry.get("directTools").and_then(Value::as_array) {
                    for t in list.iter().filter_map(Value::as_str) {
                        if loomux_tools.contains(t) {
                            tools.push(t.to_string());
                        }
                    }
                }
            }
        }
        names.sort();
        tools.sort();
        tools.dedup();
        files.push(json!({
            "file": rel,
            "parsed": parsed.is_some(),
            "servers": names.clone(),
            // The sharp case: later source wins by NAME, so this entry would
            // replace loomux's outright.
            "shadows_this_agents_server": names.iter().any(|n| n == server_name),
            "shadows_loomux_tool_names": tools,
        }));
    }
    if files.is_empty() {
        return None;
    }
    Some(json!({
        "files": files,
        "why": "pi's MCP adapter MERGES its config sources and cannot be made exclusive to \
                loomux's per-agent file, so these repo-authored declarations are part of this \
                pane's tool surface. Reported, not refused. A tool-name overlap is visible only \
                where an entry pins directTools to explicit names — an entry with directTools \
                true advertises its names at connect time, and loomux never connects to one.",
    }))
}
