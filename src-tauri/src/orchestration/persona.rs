//! Personas: how a persona's contract is carried and injected, the tools-gap
//! checks, the block contract text, and the generated per-CLI agent files.
//! Design note: `docs/design/orchestration.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. Sibling files it
//! calls: `clis/copilot.rs`, `clis/mod.rs`, `kickoff.rs`.

use super::*;

/// YAML double-quoted scalar for a generated Claude subagent file's
/// `description:` field (round #417 correction 6 — `write_claude_agent_
/// file`). Claude's own subagent-file schema REQUIRES `description`
/// (unlike loomux's generated Copilot file, which has none at all — see
/// `write_copilot_agent_file`'s doc), and the value can be a repo-authored
/// persona's free text, not guaranteed YAML-safe on its own — an unquoted
/// value containing `: ` would break the frontmatter's block-mapping parse.
/// Escapes `\` and `"` for a valid double-quoted YAML flow scalar, and
/// collapses newlines to spaces since a description is documented as one
/// line.
pub(in crate::orchestration) fn yaml_double_quoted(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"").replace(['\n', '\r'], " ");
    format!("\"{escaped}\"")
}

/// Round 8 review (N1/N2, rev-16): WHAT of a block's durable role material
/// actually rides an agent's CLI system-prompt layer — replaces the lossy
/// `contract_on_system_layer: bool` this PR carried through round 8's own
/// B1 fix. The bool collapsed a real THREE-state fact into two: "full
/// contract" and "kickoff only" were both real, distinct states before
/// round 8 (Claude vs. everything else), but round 8's Copilot slimming
/// introduced a genuine THIRD state — mechanics core + a pointer, durable
/// but not complete — and the bool forced it into `false`/"nothing durable"
/// alongside the true kickoff-only cases, which is what made every Copilot
/// compaction re-embed the full ~58-60KB instructions file via VERBOSE
/// reinjection: exactly the cost a compaction is supposed to reclaim, and
/// counter to round 5's own user-directed slimming. rev-16 also named the
/// staleness this caused as a 3-round pattern on this PR (the [`AgentEntry::
/// contract_carrier`] doc and the `to_reinject` processing comment both
/// described a binary fact a bool could still technically hold, even after
/// round 8 changed what was actually true) — an enum makes the docs
/// structurally unable to describe a state that doesn't exist.
///
/// Not persisted: `AgentEntry` (where this rides at runtime) derives only
/// `Clone, Debug` — no `Serialize`/`Deserialize` — and is never written to
/// disk. The one roster structure that IS persisted, [`AgentRecord`]
/// (`agents.json`), does not carry this fact at all; it is recomputed fresh
/// by `persona_inject` on every spawn and every resume. So there is no
/// serde migration surface for this field to get wrong — checked, not
/// assumed, before concluding no compat shim was needed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractCarrier {
    /// The FULL contract ([`block_contract_text`]: mechanics + the complete
    /// role template, persona folded in) rides the system-prompt layer
    /// verbatim. Every Claude block, either path (`--agent <generated
    /// file>` or the `--append-system-prompt-file` fallback — both are
    /// launch-time system-prompt construction, not a conversation-history
    /// artifact a compaction could touch).
    SystemLayerFull,
    /// Only the non-negotiable CORE (identity + `mechanics_core` + persona,
    /// if any, + a pointer to the full instructions file — see
    /// `copilot_agent_body`'s doc) rides the system-prompt layer. Copilot's
    /// generated-wrapper happy path, as of round 8: durable, but
    /// deliberately incomplete, because the full contract routinely blows
    /// GitHub's documented body cap.
    SystemLayerCore,
    /// Nothing loomux-authored is durable on the system-prompt layer for
    /// this agent — at most the kickoff prompt got a copy, once, of
    /// whatever a workflow persona supplied. A Copilot block on a
    /// user-authored native `.github/agents/*.md` persona (only the user's
    /// OWN file rides `--agent`, never loomux's contract — the documented
    /// #416 residual gap), the `~/.copilot/agents`-unwritable fallback, and
    /// an over-cap generated body the round-8 write guard refused to write.
    KickoffOnly,
}

impl Default for ContractCarrier {
    /// The safest assumption when nothing else is known: treat an agent as
    /// if it has NOTHING durable, same as the pre-enum bool's `false`
    /// default — a false "nothing durable" wastes a verbose re-embed at
    /// worst; a false "fully durable" would silently drop a contract.
    fn default() -> Self {
        Self::KickoffOnly
    }
}

/// A block's durable role CONTRACT plus any workflow persona, compiled down to
/// what each agent CLI can actually consume (#222, restructured #416, moved
/// off argv onto a file for Claude in round #417 correction 6).
///
/// **Both CLIs are symmetric now, as of round 6** — each has its own native
/// `--agent <name>` flag resolving a NAME against a known user-level
/// directory (`~/.claude/agents` / `~/.copilot/agents`), so engaging one
/// needs either a file the user already wrote (`.github/agents/*.md`,
/// Copilot only — Claude has no equivalent repo-authored convention) or one
/// loomux generates itself into the CLI's OWN user-level agent directory
/// (never the repo's `.github/agents/`, which would dirty the user's git
/// tree). Before round 6, Claude was the odd one out — it took a definition
/// INLINE (`--agents '<json>' --agent <id>`), which is exactly what put the
/// full contract on argv and blew Windows `CreateProcessW`'s 32,767-character
/// limit once #416 widened that payload from a short persona to loomux's own
/// template prose. Hence:
///
/// | block persona | claude | copilot |
/// |---|---|---|
/// | none | contract → generated file + `--agent` | slim (mechanics core + pointer) → generated file + `--agent` |
/// | `prompt:` (inline) | contract+persona → generated file + `--agent` | slim+persona → generated file + `--agent` |
/// | `profile: .github/agents/x.md` | n/a — Claude has no repo-authored persona convention | `--agent x` (native, unwrapped — see `persona_inject`) |
///
/// **Round 8:** Copilot's row is no longer symmetric with Claude's — GitHub's
/// own custom-agents-configuration reference caps the generated file's BODY
/// at 30,000 characters (documented; Claude has no such cap), and the FULL
/// contract routinely exceeds it (the default roster's own orchestrator
/// measured 58,633 chars). Copilot's generated file now carries a SLIM
/// composition (`copilot_agent_body`: identity + `mechanics_core` + persona,
/// if any, via the same [`block_contract_text`] framing, + a pointer to the
/// full instructions file) instead of the raw contract — a per-CLI
/// composition decision, not a universal thinning; Claude's row is
/// unchanged. Copilot's row carries `ContractCarrier::SystemLayerCore` as
/// of this split — a real durable state, distinct from `KickoffOnly` (see
/// that enum's doc).
///
/// (Claude's write-failure fallback — `~/.claude/agents` unwritable — is
/// `--append-system-prompt-file <instructions file>`, not shown above; still
/// a file, never argv. Copilot's write-failure fallback is a kickoff-text
/// paste, the one case where the contract does NOT reach the system-prompt
/// layer at all — `ContractCarrier::KickoffOnly`.)
///
/// Before #416, the `none` row emitted nothing at all on either CLI (the
/// pre-#222 command, byte for byte) and the built-in mechanics/template body
/// never reached a system-prompt layer on ANY row — only a repo's own persona
/// text did. `contract` (see [`block_contract_text`]) closes that: it is
/// ALWAYS present and ALWAYS what rides on Claude's native custom-agent
/// mechanism (round 8 note above: Copilot's own generated file carries a
/// slim COMPOSITION derived from the same inputs, not `contract` directly),
/// with a configured persona folded in as an addendum rather than the sole
/// payload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PersonaInject {
    /// Claude `--agent <name>`: activates a loomux-generated
    /// `~/.claude/agents/<name>.md` FILE carrying [`block_contract_text`]'s
    /// output (round #417 correction 6; see `OrchRegistry::
    /// write_claude_agent_file`). **Never paired with `claude_agents_json`
    /// (removed this round) — the contract travels by FILE now, never argv.**
    /// The doc's own CLI reference confirms `--agent <name>` alone starts a
    /// session "where the main thread itself takes on that subagent's
    /// system prompt... replac[ing] the default Claude Code system prompt
    /// entirely" when the name resolves against `~/.claude/agents/` or
    /// `.claude/agents/` — the exact file-based mechanism Copilot's
    /// `~/.copilot/agents` precedent already established, now mirrored for
    /// Claude instead of the pre-round-6 inline-JSON flag. `None` only when
    /// `write_claude_agent_file` fails (directory unwritable) — see
    /// `claude_append_system_prompt_file` for that fallback.
    pub claude_agent: Option<String>,
    /// Claude `--append-system-prompt-file <path>`: the fallback ONLY when
    /// `write_claude_agent_file` fails. Points at the SAME instructions file
    /// `write_instruction_files` already reliably writes to the group's own
    /// state dir (never a second file loomux has to invent or clean up) —
    /// docs-confirmed to "load additional system prompt text from a file and
    /// append to the default prompt", so this stays system-prompt-layer
    /// durable (`ContractCarrier::SystemLayerFull`) even in this rare path,
    /// unlike Copilot's equivalent fallback (which has no such flag and
    /// degrades to a kickoff-only paste). Round 8 review (N3b, widened by
    /// rev-18): the instructions file this points at is the MECHANICS/
    /// template body only — a block's actual persona TEXT never lands
    /// there (only in `contract`, which is what failed to write) — true in
    /// EITHER persona mode, so this combination (write failure + any
    /// non-empty persona) is audited (`claude-fallback-persona-dropped`)
    /// at the `persona_inject` call site. Never set together with
    /// `claude_agent`.
    pub claude_append_system_prompt_file: Option<PathBuf>,
    /// Copilot `--agent <name>` — either a user-authored `.github/agents/*.md`
    /// (native, unwrapped) or a loomux-generated `~/.copilot/agents/*.agent.md`
    /// (#416; see `OrchRegistry::write_copilot_agent_file`) carrying a SLIM
    /// composition as of round 8 (`copilot_agent_body`), not the raw
    /// contract — see that function's doc for why. loomux never writes a
    /// generated file into the repo's own `.github/agents/` (see
    /// `profiles::is_copilot_native`).
    pub copilot_agent: Option<String>,
    /// opencode `--agent <handle>` (#722): selects the agent entry loomux
    /// declared in the config document it delivers by environment variable —
    /// the third "native custom-agent flag, file-backed" shape, differing from
    /// claude's and copilot's only in that the DEFINITION rides an env-borne
    /// document instead of a file in a CLI-owned directory, while the contract
    /// itself still rides a file ([`Self::opencode_prompt_file`]).
    ///
    /// `None` when that file could not be written — in which case the config
    /// document declares no agent either, because an `--agent` naming an entry
    /// that does not exist does not fail: it warns and falls back to `build`,
    /// the most permissive agent there is. Containment does not rest on this
    /// flag for exactly that reason (see [`OPENCODE_PERMISSION_ENV`]).
    pub opencode_agent: Option<String>,
    /// The file [`Self::opencode_agent`]'s entry points its `prompt` at, via
    /// opencode's `{file:…}` reference. Written under the group's own state
    /// dir — never into the repo's `.opencode/agents/`, which would dirty the
    /// user's git tree with files they did not write, and never into a
    /// per-user CLI directory, which would need an orphan sweep; a group's
    /// files go when the group does.
    pub opencode_prompt_file: Option<PathBuf>,
    /// pi `--append-system-prompt <path>` (#2126) — the block's full contract,
    /// by FILE.
    ///
    /// The fourth shape, and the only one of the four that is not a *named*
    /// custom agent: pi has no `--agent` equivalent at all, so there is
    /// nothing to name and nothing to resolve — the contract is appended to
    /// pi's own system prompt directly. That makes it the SIMPLEST of the four
    /// and still `ContractCarrier::SystemLayerFull`: the flag is launch-time
    /// system-prompt construction, not a conversation-history artifact a
    /// compaction could touch, exactly like claude's
    /// [`Self::claude_append_system_prompt_file`].
    ///
    /// A file rather than argv text, although pi's flag accepts either: the
    /// contract is many KB and Windows `CreateProcessW`'s 32,767-character
    /// command-line limit made that a real, demo-blocking bug once (#417).
    /// Only the path reaches argv.
    ///
    /// `None` when the file could not be written — the group dir is
    /// unwritable. The pane then launches with no flag (audited
    /// `pi-contract-file-unwritable`, carrier `KickoffOnly`) rather than with
    /// a flag pointing at a file that is not there.
    pub pi_append_system_prompt_file: Option<PathBuf>,
    /// codex `developer_instructions` (#2515 C1) — the block's full contract,
    /// **by VALUE**, and the fifth of the five persona shapes.
    ///
    /// It is the only one that is not a path, and that is forced rather than
    /// chosen: codex has no `--append-system-prompt`, no `--agent`, and no
    /// `developer_instructions_file`. `model_instructions_file` exists and is
    /// the wrong knob — it REPLACES codex's built-in prompt rather than adding
    /// to it, which would take the agent's own tool discipline away in order
    /// to give it a role. So the contract travels as a key in the profile file
    /// `write_codex_profile` writes, and the escaping that makes an arbitrary
    /// contract safe in TOML is `toml_basic_escape`'s job.
    ///
    /// Carrying KB of text in a struct field rather than a path costs nothing
    /// here: it never reaches argv (Windows `CreateProcessW`'s 32,767-character
    /// limit, #417, is what makes the other four paths), only a file loomux
    /// writes in the same call.
    ///
    /// `ContractCarrier::SystemLayerFull`, and that is measured rather than
    /// assumed. The key is documented as "inserted as a `developer` role
    /// message", which sounds like conversation history a compaction would
    /// eat — but codex reads it from CONFIG into every `TurnContext` and
    /// re-inserts it through `build_initial_context_with_world_state` on the
    /// compaction path itself (`start_new_context_window` →
    /// `replace_compacted_history`). So a compacted codex pane recovers its
    /// contract from this file with loomux doing nothing, which is exactly what
    /// the `SystemLayerFull` tier claims.
    ///
    /// `None` when there is no contract to deliver.
    pub codex_developer_instructions: Option<String>,
    /// Extra pre-approved tool patterns from the persona's `allow:`. Widens
    /// only *within* the capability class: deny rules beat allow rules on both
    /// CLIs, so this can never re-grant what the block's `kind` denies.
    pub extra_allow: Vec<String>,
    /// Persona body for the **kickoff-prompt fallback** — reached only when
    /// `~/.copilot/agents` is unwritable (the generated file above is the
    /// normal #416 path). Delivered as text in the kickoff, which every CLI
    /// reads. Claude has no equivalent: its own write-failure fallback
    /// (`claude_append_system_prompt_file`) never needs one.
    pub kickoff: Option<String>,
    /// WHAT of the block's durable role material rides this agent's
    /// system-prompt layer — see [`ContractCarrier`]'s own doc for the
    /// three states and round 8's rev-16 finding about why this used to be
    /// a lossy bool. `SystemLayerFull` for every Claude block (either
    /// generation path); `SystemLayerCore` for Copilot's generated-wrapper
    /// happy path (round 8); `KickoffOnly` for a Copilot native persona,
    /// an unwritable `~/.copilot/agents`, or an over-cap body the write
    /// guard refused. Consumed post-spawn by `compact_reinjection_notice`'s
    /// three-way shape choice (round #417 correction 5, revised round 8):
    /// `SystemLayerFull` → slim (nothing to re-embed or point at);
    /// `SystemLayerCore` → pointer (re-read the full instructions file, no
    /// embed — the compaction can only dilute what the core DOESN'T
    /// already durably hold); `KickoffOnly` → verbose (embed the full
    /// contract read back from the instructions file — the true fallback,
    /// nothing else is durable).
    pub contract_carrier: ContractCarrier,
    /// Human-facing notices this compilation produced (#802), surfaced in the
    /// `spawn_agent` reply as well as in the audit log.
    ///
    /// The audit alone was not enough, and that is the whole lesson of #802:
    /// the failure it describes — a persona's `tools:` filter silently
    /// stripping loomux's own MCP server from the delegate — cost three rounds
    /// precisely because nothing said it out loud at spawn time. Whoever asked
    /// for the spawn is the party that can fix the file, so the notice goes
    /// back to them and not only to a log nobody reads mid-incident.
    pub warnings: Vec<String>,
}

/// A block's persona after the `prompt:` / `profile:` sources have been
/// resolved to one body — the shared input to both the CLI flags
/// ([`PersonaInject`]) and the block's role-instruction file.
#[derive(Clone, Debug)]
#[doc(hidden)] // pub for integration tests: they compile a block exactly as spawn does
pub struct ResolvedPersona {
    /// The persona body (`sanitize_persona`d — see that function's doc for
    /// what still needs stripping now that no consumer is a raw shell line).
    pub text: String,
    /// The handle a native `--agent` flag names.
    pub name: String,
    /// One-line description, consumed by Claude's generated custom-agent
    /// file's frontmatter (required by its schema). Copilot's generated
    /// file has its OWN `description`, as of round 8 — but built
    /// deterministically from `group`/`block.id` by `write_copilot_agent_
    /// file` directly, never from this persona-sourced field.
    pub description: String,
    pub mode: profiles::ProfileMode,
    pub allow: Vec<String>,
    /// Set when the persona came from a user-authored `.github/agents/*.md`,
    /// which is the only thing Copilot's native `--agent` can resolve.
    pub copilot_native: bool,
    /// The persona file's own `tools:` list (#802), `None` when it declares
    /// none. Copilot's `tools:` is a FILTER over built-in AND MCP tools, so a
    /// list that omits loomux's server strips the delegate's report channel —
    /// see [`profiles::AgentProfile::tools`] and `persona_inject`.
    pub copilot_tools: Option<Vec<String>>,
    /// Whether the persona file declares its own `mcp-servers:` block (#802) —
    /// the one case where loomux must not repair a `tools:` gap by re-pointing
    /// `--agent` at a generated copy. See
    /// [`profiles::AgentProfile::has_mcp_servers`].
    pub copilot_has_mcp_servers: bool,
    /// The persona file's frontmatter block verbatim (#802), so a #802 repair
    /// can write a stand-in that keeps every key loomux does not own — see
    /// [`profiles::AgentProfile::frontmatter`] and
    /// [`profiles::carry_frontmatter`]. Empty for a persona that came from an
    /// inline `prompt:` or from `allow:` alone: neither has a file.
    pub copilot_frontmatter: String,
}

impl ResolvedPersona {
    /// Whether this persona's own `tools:` filter would leave this app's MCP
    /// tools available to the agent Copilot launches from it (#802). Always
    /// true for a persona that declares no `tools:` — Copilot's documented
    /// default is every tool.
    ///
    /// **The CURRENT server name only, and that asymmetry is argued rather
    /// than overlooked** (rev-967 B1). Everywhere else in #1153 phase 3 a
    /// reader accepts every spelling, because it is reading a record this app
    /// wrote and cannot rewrite. A persona's `tools:` list is not that: it is
    /// the USER's statement of intent, and a stale `loomux/*` in it grants
    /// access to a server no longer declared to Copilot. Treating that as a
    /// live grant would take the native path and hand the delegate a filter
    /// matching nothing — it would launch with no orchestration tools at all.
    /// Treating it as a gap sends it to the repair path, which adds
    /// `orrerix/*`: the author's own intent, spelled the way the server is
    /// spelled now. So the stale whole-server grant is deliberately a GAP.
    pub fn grants_loomux_tools(&self) -> bool {
        profiles::tools_grant_mcp_server(self.copilot_tools.as_deref(), MCP_SERVER)
    }

    /// Does the `tools:` list scope this app's MCP server to NAMED TOOLS —
    /// under any spelling a repo author could have written (#802, rev-967 B1)?
    ///
    /// `loomux/report` is a decision, not an omission, and it stays one after
    /// the rename: the author asked for exactly one tool. Before this arm
    /// existed the pre-rename spelling read as "never mentions the server",
    /// so [`tools_gap_refusal`] called it repairable and the repair appended
    /// the full-server grant — this app widening a deliberate narrowing,
    /// which is the exact move #222's capability closure forbids.
    ///
    /// Per spelling, `mentions && !grants`: that is what makes `loomux/*`
    /// (whole-server, stale) fall through to the repair path while
    /// `loomux/report` (one tool, stale) does not. See
    /// [`grants_loomux_tools`](Self::grants_loomux_tools) for why those two
    /// want opposite answers.
    pub fn scopes_mcp_server_per_tool(&self) -> bool {
        brand::MCP_SERVERS.into_iter().any(|s| {
            profiles::tools_mention_mcp_server(self.copilot_tools.as_deref(), s)
                && !profiles::tools_grant_mcp_server(self.copilot_tools.as_deref(), s)
        })
    }

    /// The server spelling this persona's `tools:` list actually names, if
    /// any — so a warning can tell a human their file names the PRE-RENAME
    /// server rather than leaving them to wonder why a scope they can see in
    /// their own file matches nothing.
    pub fn mcp_server_named_in_tools(&self) -> Option<&'static str> {
        brand::MCP_SERVERS
            .into_iter()
            .find(|s| profiles::tools_mention_mcp_server(self.copilot_tools.as_deref(), *s))
    }
}

/// The spawn-time notice for a persona whose `tools:` filter drops loomux's MCP
/// server (#802).
///
/// It names the block, the persona, what the list says, and the exact line to
/// add — because the failure this describes is indistinguishable, from inside
/// the delegate's pane, from "loomux is broken". Three rounds of #802 went by
/// on that ambiguity.
/// What loomux actually did about a persona's `tools:` gap (#802) — the
/// outcomes [`copilot_tools_gap_warning`] has to be able to tell apart.
///
/// The three `KeptNative*` variants are all "the file is left exactly as the
/// user wrote it, and the delegate really is missing loomux until a human edits
/// it". They are separate variants rather than one, because the *reason* is the
/// only actionable part of that message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc(hidden)] // pub for integration tests
pub enum ToolsGapAction {
    /// `--agent` was re-pointed at a loomux-generated copy carrying the same
    /// list plus the loomux grant. The spawn works; the repo file still needs
    /// the one-line fix.
    Repaired,
    /// The persona declares its own `mcp-servers:`, and a generated copy would
    /// drop them.
    KeptNativeForMcpServers,
    /// `tools: []` — documented as *"disables all tools"*. An explicit,
    /// deliberate "nothing", which loomux does not get to overrule into
    /// "nothing except me".
    KeptNativeForExplicitEmptyList,
    /// The list already names loomux per-tool (`loomux/report`). The user scoped
    /// the server deliberately; widening that to `orrerix/*` would be this app
    /// granting itself more than it was given.
    KeptNativeForPerToolScope,
    /// The copy could not be written (unwritable agents dir, or a body over the
    /// documented size cap), so no `--agent` file is in play at all.
    RepairFailed,
}

/// Whether a persona's `tools:` gap is one loomux may repair by re-pointing
/// `--agent` at a stand-in, or one it must only report (#802).
///
/// `None` = repairable. `Some(reason)` = leave the user's file alone and say
/// why. Every refusal here is the same principle: **loomux repairs an omission,
/// never a decision.** A list that simply never mentions loomux is an omission
/// — nobody writes `tools: [read, edit]` *meaning* "and loomux must not work".
/// An empty list and a per-tool `loomux/<tool>` scope are decisions, stated in
/// the file, and silently widening either would make loomux the thing that
/// granted itself capability — the exact move #222's capability closure exists
/// to forbid, and the opposite of what this PR claims to do.
pub(in crate::orchestration) fn tools_gap_refusal(p: &ResolvedPersona) -> Option<ToolsGapAction> {
    if p.copilot_has_mcp_servers {
        return Some(ToolsGapAction::KeptNativeForMcpServers);
    }
    if p.copilot_tools.as_deref().is_some_and(|t| t.is_empty()) {
        return Some(ToolsGapAction::KeptNativeForExplicitEmptyList);
    }
    if p.scopes_mcp_server_per_tool() {
        return Some(ToolsGapAction::KeptNativeForPerToolScope);
    }
    None
}

#[doc(hidden)] // pub for integration tests
pub fn copilot_tools_gap_warning(
    block_id: &str,
    persona: &ResolvedPersona,
    action: ToolsGapAction,
) -> String {
    let listed = persona.copilot_tools.clone().unwrap_or_default();
    let shown = if listed.is_empty() {
        "an empty list (which Copilot documents as \"disables all tools\")".to_string()
    } else {
        format!("[{}]", listed.join(", "))
    };
    let partial = match persona.mcp_server_named_in_tools() {
        // The pre-rename spelling. Say so plainly: from inside the file this
        // looks like a working scope, and nothing else here would tell the
        // author that the name itself is what stopped matching (rev-967 B1).
        Some(named) if named != MCP_SERVER => format!(
            " It names the PRE-RENAME server `{named}` — that scope matches nothing now that \
             the server is `{MCP_SERVER}`, and it is left exactly as written rather than \
             widened, so edit the file to scope `{MCP_SERVER}` instead."
        ),
        Some(_) if persona.scopes_mcp_server_per_tool() => format!(
            " It grants some {MCP_SERVER} tools but not all of them, so the delegate \
             would be missing whichever ones it did not name."
        ),
        _ => String::new(),
    };
    let tail = match action {
        ToolsGapAction::Repaired => format!(
            "orrerix launched it from a generated copy of that persona carrying the same list \
             plus \"{}\" instead, so this spawn works — but the repo file is still the one to \
             fix.",
            COPILOT_MCP_TOOL_GRANTS.join("\", \""),
        ),
        ToolsGapAction::KeptNativeForMcpServers => {
            "orrerix did NOT rewrite it, because the persona declares its own `mcp-servers:` \
             block and a generated copy would drop those servers — so THIS DELEGATE CANNOT \
             CALL orrerix until the file is fixed."
                .to_string()
        }
        ToolsGapAction::KeptNativeForExplicitEmptyList => {
            "orrerix did NOT rewrite it: an empty list is a deliberate \"no tools at all\", not \
             an omission, and orrerix does not overrule it into \"none except orrerix\" — so THIS \
             DELEGATE CANNOT CALL orrerix until the file is fixed."
                .to_string()
        }
        ToolsGapAction::KeptNativeForPerToolScope => {
            // rev-967 N7. This tail is per-ACTION, so it lands directly after the
            // `partial` sentence — and for a stale spelling that sentence has just
            // said the scope matches nothing. "CAN CALL ONLY the tools that list
            // names" then reads as though some of them still work, when the set is
            // empty. The refusal is identical in both cases; only what it promises
            // differs, so each says its own consequence.
            let stale = persona.mcp_server_named_in_tools().is_some_and(|n| n != MCP_SERVER);
            let consequence = if stale {
                "so THIS DELEGATE CAN CALL NONE OF THEM: the tools that list names belong to a \
                 server this app no longer declares, and the scope is left as written rather \
                 than widened — a human has to edit the file"
            } else {
                "so THIS DELEGATE CAN CALL ONLY the orrerix tools that list names, until a \
                 human widens it"
            };
            format!(
                "orrerix did NOT rewrite it: the list scopes the orrerix server to named tools \
                 on purpose, and widening that to the whole server would be orrerix granting \
                 itself more than it was given — {consequence}."
            )
        }
        ToolsGapAction::RepairFailed => {
            "orrerix could not write its generated copy, so this delegate launched with no \
             agent file at all: it keeps its orrerix tools, but the persona reached it as \
             kickoff text instead of its system prompt."
                .to_string()
        }
    };
    format!(
        "block {block_id}: copilot persona \"{}\" declares tools: {shown}, which does not grant \
         orrerix's MCP server. Copilot's `tools:` is a FILTER over built-in AND MCP tools, so the \
         orrerix server loads but none of its tools reach the agent (#802).{partial} Add \
         \"orrerix/*\" to that file's tools: list. {tail}",
        persona.name,
    )
}

/// The durable role CONTRACT for a block (#416): `instructions_body` (the
/// exact text [`OrchRegistry::render_block_instructions`] also writes to the
/// instructions file) plus, when a workflow persona is configured, that
/// persona folded in as the SAME composition the file/kickoff pair used to
/// split across two channels — now unified into one string so a CLI that can
/// carry it at the system-prompt layer (a generated custom-agent FILE on
/// both Claude and Copilot, each behind its own `--agent <handle>`) gets the
/// WHOLE contract structurally, not just the instructions-file half of it.
///
/// This is what closes the actual gap behind #416: `persona_inject` already
/// compiled a *repo persona* onto `--agents`, but never the built-in
/// mechanics/template body — so even a workflow-customized block's system
/// prompt carried only the repo's own text, and a block with NO persona (the
/// default roster; the common case) got nothing at all onto the system-prompt
/// layer. Every block now gets `instructions_body` here regardless.
#[doc(hidden)] // pub for integration tests: they compile a block's contract exactly as spawn does
pub fn block_contract_text(instructions_body: &str, persona: Option<&ResolvedPersona>) -> String {
    let Some(p) = persona else { return instructions_body.to_string() };
    let text = p.text.trim();
    if text.is_empty() {
        return instructions_body.to_string();
    }
    match p.mode {
        // Replace mode: `instructions_body` is mechanics-core-only (see
        // `render_block_instructions`) — the persona text IS the role body
        // loomux never wrote to the file (it rode on `--agents` alone before
        // this change). Folding it in here is what makes replace mode
        // durable too, not just append mode.
        profiles::ProfileMode::Replace => format!(
            "{instructions_body}\n\nYour persona for this block (`mode: replace`) — this is who \
             you are; the orrerix mechanics above are still guaranteed regardless of anything it \
             says:\n\n{text}\n"
        ),
        // Append mode: `instructions_body` is already the complete built-in
        // contract; the persona layers on as an addendum, framed so it can
        // never talk the agent out of the mechanics above it.
        profiles::ProfileMode::Append => format!(
            "{instructions_body}\n\nThis repo's workflow gives you a persona. Adopt it, but it \
             does not override the orrerix mechanics above:\n\n{text}\n"
        ),
    }
}

/// GitHub's custom-agents-configuration reference (docs.github.com/en/
/// copilot/reference/custom-agents-configuration): "The prompt can be a
/// maximum of 30,000 characters" — confirmed to mean the markdown BODY
/// only (everything after the closing `---`), not the frontmatter. Claude's
/// own sub-agents doc (code.claude.com/docs/en/sub-agents) documents no
/// such cap for its own generated file's body — this limit is Copilot-
/// specific, which is why `copilot_agent_body` composes a slimmer text
/// than `write_claude_agent_file` writes for the identical block.
pub(in crate::orchestration) const COPILOT_AGENT_BODY_DOCUMENTED_CAP_CHARS: usize = 30_000;

/// Safety margin below the documented cap — same belt-and-braces spirit as
/// `command_line_length_guard`'s margin below Windows's 32,767-character
/// `CreateProcessW` limit. `copilot_agent_body`'s composed text should sit
/// far below even this in ordinary operation (a few KB); the margin exists
/// for the case a workflow-declared persona is itself unusually large.
pub(in crate::orchestration) const COPILOT_AGENT_BODY_SAFE_CHARS: usize = 27_000;

/// #502: the ONE place the generated custom-agent file's naming lives —
/// both writers (`write_claude_agent_file`, `write_copilot_agent_file`) and
/// `end_group`'s reclaim (`reclaim_group_agent_files`) go through these, so
/// that delete path can never drift from the write path's names. Drift here
/// is not cosmetic: a delete path that re-derives the shape independently
/// either misses files (they accumulate — #502's incident) or matches too
/// widely (it deletes something loomux never wrote).
///
/// #464's `sweep_orphaned_agent_files` matches the same shape with its own
/// literals rather than these constants. Left as it shipped deliberately:
/// unifying it would be a behavior change to a reviewed, merged sweep for a
/// cosmetic gain, and its rule is pinned by its own tests.
///
/// The shape is `loomux-<group>-<block>` plus a per-CLI extension, because
/// a handle must be unique per group+block so concurrent groups never
/// collide — see `write_claude_agent_file`'s doc.
pub(in crate::orchestration) const GENERATED_AGENT_PREFIX: &str = "loomux-";
pub(in crate::orchestration) const CLAUDE_AGENT_FILE_EXT: &str = ".md";
pub(in crate::orchestration) const COPILOT_AGENT_FILE_EXT: &str = ".agent.md";
/// opencode's contract file (#722). Not an agent definition — the definition
/// lives in the generated config document — so this names only where that
/// document's `{file:}` reference points, and it lives under the group dir,
/// which is why it is out of `reclaim_group_agent_files`' scope entirely.
pub(in crate::orchestration) const OPENCODE_AGENT_FILE_EXT: &str = ".md";

/// `loomux-<group>-<block>` — and the **most dangerous** group-id
/// interpolation in the tree (#904), because unlike every other one it does not
/// stay inside loomux's own root: the handle becomes a FILE NAME under the
/// user's `~/.claude/agents` and `~/.copilot/agents` (see
/// `write_claude_agent_file`, `write_copilot_agent_file`). A group id carrying
/// a separator would not merely traverse loomux's state directory — it would
/// write into an arbitrary location under the user's CLI configuration, and
/// `end_group`'s reclaim would then delete by the same shape.
///
/// So the id is validated before it is interpolated, and `None` — never a
/// sanitized rewrite — is the answer for one that isn't a path segment. All
/// three callers already return `Option`, so the refusal is a `?`.
///
/// `block` needs no check here: block ids come from `workflow::sanitize_id`
/// and are pinned to the same alphabet at parse time.
#[doc(hidden)] // pub for integration tests
pub fn generated_agent_handle(group: &GroupId, block: &str) -> Option<String> {
    let group = GroupId::parse(group).ok()?;
    Some(format!("{GENERATED_AGENT_PREFIX}{group}-{block}"))
}

/// #502: upper bound on a generated agent file's `description:` value.
///
/// Claude Code loads EVERY agent definition's description into the parent
/// session's roster and caps the aggregate (observed: "Agent descriptions
/// are over the 15.0k-token limit"), so description length is a shared
/// budget, not a per-file concern. loomux's own descriptions are already
/// terse (the block id — `worker`, `orchestrator`), but a repo-authored
/// persona's `description:` frontmatter is free text of unbounded length,
/// and it flows straight into the generated file. Clamping it keeps one
/// repo's verbose persona from spending everyone's budget. 160 characters
/// is comfortably more than the one-line "when to delegate to this agent"
/// the field is documented to hold.
const GENERATED_AGENT_DESCRIPTION_MAX_CHARS: usize = 160;

/// Clamp a description to [`GENERATED_AGENT_DESCRIPTION_MAX_CHARS`],
/// counting CHARACTERS (a repo persona's text is not guaranteed ASCII, and
/// truncating mid-codepoint would panic on a byte slice).
pub(in crate::orchestration) fn clamp_agent_description(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= GENERATED_AGENT_DESCRIPTION_MAX_CHARS {
        return s.to_string();
    }
    let kept: String = s.chars().take(GENERATED_AGENT_DESCRIPTION_MAX_CHARS - 3).collect();
    format!("{}...", kept.trim_end())
}

/// #502: which live orchestration group a generated agent file's stem
/// belongs to — the **longest** group id whose `loomux-<group>-` prefix the
/// stem carries, or `None` when no live group claims it (an orphan).
///
/// Longest-match, not first-match, because nothing stops one group id from
/// being a prefix of another: with groups `g` and `g-x` alive, the stem
/// `loomux-g-x-worker` is `g-x`'s worker, not a `g` file for a block named
/// `x-worker`. First-match would let `end_group("g")` delete a LIVE group's
/// file. The ambiguity is unresolvable from the filename alone (both
/// readings are well-formed), so ownership is decided against the group
/// registry — loomux's own source of truth — and resolved toward the more
/// specific claim.
///
/// Its one caller, `end_group`'s `reclaim_group_agent_files`, takes its
/// conservative direction from this: it deletes only what THIS group owns,
/// so a stem a more specific live group claims is never touched.
pub(in crate::orchestration) fn owning_group<'a>(stem: &str, groups: &'a [GroupId]) -> Option<&'a GroupId> {
    groups
        .iter()
        .filter(|g| {
            stem.strip_prefix(&format!("{GENERATED_AGENT_PREFIX}{g}-"))
                .is_some_and(|block| !block.is_empty())
        })
        .max_by_key(|g| g.len())
}



/// Round 8 review (N3a): a short, CLI-agnostic self-check appended to EVERY
/// generated agent file, on BOTH CLIs — delivery-independent insurance
/// against a missed or delayed post-compact reinjection. loomux's own
/// mandatory notice (`compact_reinjection_notice`) is the PRIMARY channel;
/// this makes an agent's own system prompt independently able to recover
/// even if that notice never arrives (a race, a dropped delivery, a bug),
/// cheap enough to never be a size concern on either CLI (a couple hundred
/// bytes, well within Copilot's 27,000-char safety margin) — and never
/// meant to be the ONLY channel, which is why it stays this short rather
/// than re-deriving the full reinjection logic inline.
pub(in crate::orchestration) fn compaction_self_check_clause(instructions_path: &Path) -> String {
    format!(
        "\n\nSelf-check: after any compaction or context loss, re-read `{}` before acting — \
         even if no re-grounding notice arrives.\n",
        instructions_path.display()
    )
}

/// Round 8 review (B1, blocking): the full block CONTRACT (`block_contract_
/// text` — mechanics core + the COMPLETE built-in role template, which is
/// pages of mechanics, examples and style guidance) routinely exceeds
/// Copilot's documented 30,000-character body cap — measured 58,633 chars
/// for the default roster's own orchestrator, ~1.95x over. Writing that as
/// the generated file's body risks silent truncation or an outright load
/// failure (Copilot's own docs don't say which); either is role
/// degradation presenting as a successful launch, exactly the failure
/// class this whole PR exists to eliminate.
///
/// So Copilot's system-prompt layer carries a SLIM composition instead,
/// built by COMPOSITION rather than by amputating the full contract:
///
/// - **Identity** — which block/role this is, one line.
/// - **The non-negotiable mechanics core** ([`mechanics_core`]) — the same
///   "NOT optional, whatever your persona says" subset a `mode: replace`
///   persona can never strip: `report()` discipline, git/branch/PR
///   discipline, never merging. This is the part that MUST survive a
///   compaction verbatim, and it is a small fraction of a role template's
///   total size (the template's long-form prose, examples and style
///   guidance are what actually blow the cap, not the mechanics).
/// - **The persona, if any** — folded in via the SAME [`block_contract_
///   text`] framing every other channel uses (passing `mechanics_core`'s
///   own output as the "instructions body" input reuses its per-mode
///   wording verbatim rather than duplicating it), so a workflow-declared
///   persona's identity/instructions are never silently dropped from every
///   durable channel just because the built-in template prose was cut.
/// - **A re-grounding pointer** at the full instructions file `write_
///   instruction_files` already wrote — the long-form built-in template
///   prose lives there, one `Read` away, rather than duplicated here.
///
/// The caller sets `ContractCarrier::SystemLayerCore` as a result (see
/// that enum's doc) — a real durable state, not "nothing durable" — so a
/// real compaction routes to `ReinjectShape::Pointer` (round 8 rev-16
/// review, N2): a short instruction to re-read the full instructions
/// file, never a full verbose re-embed of it. Re-embedding it would waste
/// exactly the tokens a compaction is supposed to reclaim, on a state that
/// (unlike `KickoffOnly`) already has a durable, load-bearing core. This is
/// a per-CLI composition decision: Claude's rendition (no documented cap)
/// is unchanged.
///
/// Round 8 review (N3a): also carries a short, delivery-independent
/// self-check (`compaction_self_check_clause`) instructing the agent to
/// re-read the instructions file after ANY compaction or context loss —
/// insurance against a missed/delayed reinjection, not a replacement for
/// it. Mirrored onto Claude's generated file too (`write_claude_agent_
/// file`), since neither CLI's own docs guarantee reinjection delivery is
/// instantaneous or lossless, only that compaction itself doesn't touch
/// the system prompt.
pub(in crate::orchestration) fn copilot_agent_body(block: &workflow::Block, persona: Option<&ResolvedPersona>, instructions_path: &Path) -> String {
    let mechanics_only = mechanics_core(block.kind, block.role_hint.as_deref());
    let slim_contract = block_contract_text(&mechanics_only, persona);
    format!(
        "You are `{}` (block id `{}`) — the {} for this loomux-orchestrated group.\n\n\
         {slim_contract}\n\n\
         This is a SLIM system-prompt copy, kept well under Copilot's documented agent-body \
         limit. Your FULL role instructions — the complete built-in role template, and this \
         block's own notes — live in `{}`. Read that file for anything beyond the mechanics \
         above; treat it as authoritative whenever this summary and it disagree.{}\n",
        block.name,
        block.id,
        block.kind.as_str(),
        instructions_path.display(),
        compaction_self_check_clause(instructions_path),
    )
}
