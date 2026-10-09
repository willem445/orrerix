//! The per-CLI adapters (#3498 P4): one file per agent CLI, plus what they
//! share here — the MCP server name and token headers, `cli_extra_env`,
//! `AgentCliConfig`, the single-pane autopilot flags, the agent token, and the
//! session-learning polls (`SessionBaseline`, `SessionSearch`).
//! Design note: `docs/design/harness-adapters.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. It calls no
//! sibling file.

use super::*;

mod claude;
pub use claude::*;
mod codex;
pub use codex::*;
mod copilot;
pub use copilot::*;
mod gemini;
pub use gemini::*;
mod opencode;
pub use opencode::*;
mod pi;
pub use pi::*;
mod resume;
pub use resume::*;

// Copilot session tracking: unlike Claude, copilot can't be handed a session
// id up front — it mints one and writes `~/.copilot/session-state/<id>/` a
// few seconds into boot. After spawning a copilot pane we poll for the new
// session directory and bind its id to the pane's roster record.
/// How often to poll `session-state` for the pane's new session.
const COPILOT_SESSION_POLL: Duration = Duration::from_millis(1000);
/// Give up watching after this long (copilot never initialized, or crashed).
const COPILOT_SESSION_TIMEOUT: Duration = Duration::from_secs(90);

// OpenCode session tracking (#722): the same after-the-fact problem as
// copilot's, one store down — `--session` continues an existing session and
// nothing pre-assigns one, so loomux learns the id by watching this group's
// own store (`OPENCODE_DB`) for a row that was not there before the spawn.
/// How often to poll the group's store. Slower than copilot's tick because
/// each one is a SQLite open + query rather than a directory listing, and
/// because the deadline below is an order of magnitude longer.
const OPENCODE_SESSION_POLL: Duration = Duration::from_secs(2);
/// Give up watching after this long.
///
/// Far longer than copilot's 90s, and the reason is an honest gap rather than
/// caution: loomux cannot verify whether opencode writes the `session` row at
/// TUI boot or only when the first turn starts — checking would mean spawning
/// the real CLI, which constraint 3 forbids. A deadline picked for "at boot"
/// would silently leave every pane whose first turn was late (a kickoff queued
/// behind a busy pane, a human who walked away) permanently unidentified, and
/// the failure would look exactly like the CLI not being installed. Ten
/// minutes covers the first turn either way; the cost of being wrong in this
/// direction is one sleeping thread.
const OPENCODE_SESSION_TIMEOUT: Duration = Duration::from_secs(600);

// codex session tracking (#2515 C1): copilot's problem again, with codex's own
// reason for it. `resume_session_id` on the TUI's `Cli` is `#[clap(skip)]` —
// "Internal … not exposed as a public flag" — so nothing loomux can put on the
// line pre-assigns an id, and the pane's thread is learned by watching
// `$CODEX_HOME/sessions` for a rollout that was not there before the spawn.
/// How often to walk the rollout store for the pane's new thread.
///
/// The slowest of the three, and the reason is neither copilot's (one directory
/// listing) nor opencode's (one SQLite query). This walk is **three directory
/// levels deep over the human's entire codex history**: one `read_dir` per
/// year, per month, and per DAY they have ever used codex, plus one entry per
/// rollout. A year of daily use is ~365 day-directories and as many files, and
/// that whole listing is paid on every tick — the baseline set makes each
/// candidate cheap to REJECT (a set lookup, no file opened) but does nothing to
/// shorten the walk itself.
///
/// Multiplied by the ten-minute deadline below, a two-second tick would be 300
/// full walks per booting pane. Five seconds is 120, and nothing whatsoever
/// waits on the result: the id is bound in the background, and a pane that
/// identifies four seconds later is indistinguishable to every consumer. So the
/// tick is set by what the walk COSTS rather than by how soon the answer could
/// be had.
const CODEX_SESSION_POLL: Duration = Duration::from_secs(5);
/// Give up watching after this long.
///
/// opencode's ten minutes rather than copilot's 90 seconds, and the fact that
/// decides it is READ rather than guessed — which is the one respect in which
/// this differs from the constant above it.
/// `RolloutRecorder::new`'s own doc: *"For newly created sessions, this
/// precomputes path/metadata and **defers file creation/open until an explicit
/// `persist()` call**."* So a fresh codex pane has a rollout PATH from boot and
/// no rollout FILE, and nothing on this side can see a path that has not been
/// written. The file appears when the session first persists — that is, when
/// the pane does some work.
///
/// A 90-second deadline would therefore leave permanently unidentified every
/// pane whose first turn was late: a kickoff queued behind a busy pane, a human
/// who walked away from an attended group. The failure would look exactly like
/// codex not being installed. Ten minutes covers the first turn either way, and
/// the cost of being wrong in this direction is one sleeping thread.
///
/// The widened window does raise the chance that a DIFFERENT pane's new session
/// in the same directory is seen while this one is still waiting — which is
/// precisely why the answer there is `Contested` and never "take the newest".
const CODEX_SESSION_TIMEOUT: Duration = Duration::from_secs(600);

/// Where to look for the session a just-spawned pane is about to mint, plus
/// the snapshot of what was already there (#722).
///
/// Only the CLIs that mint their own id after boot have one. Claude and pi are
/// handed their id up front ([`CliCaps::premints_session_id`], both spelled
/// `--session-id`), so there is nothing to learn; gemini has no session store
/// loomux reads.
#[derive(Clone, Debug)]
#[doc(hidden)] // pub for integration tests
pub enum SessionBaseline {
    /// The ids under `~/.copilot/session-state`, and the root they came from —
    /// carried rather than re-resolved so the poll cannot end up reading a
    /// different directory than the baseline was taken from.
    Copilot { ids: HashSet<String>, root: PathBuf },
    /// The ids in **this group's** opencode store. The path is deliberately
    /// *not* carried: it is recomputed per poll from `opencode_db_path`, the
    /// one function the spawn that creates the store and the usage read that
    /// consumes it already share (#812).
    OpenCode { ids: HashSet<String> },
    /// The thread ids under `$CODEX_HOME/sessions`, and the root they came
    /// from (#2515 C1) — copilot's shape, and carried for copilot's reason:
    /// the poll must not end up reading a different directory than the
    /// baseline was taken from, which is exactly what would happen if a
    /// `CODEX_HOME` changed mid-session and the root were re-resolved.
    ///
    /// **NOT group-local**, unlike opencode's, and that is a decision rather
    /// than an omission: codex's only relocation knob is `CODEX_HOME`, which
    /// moves `auth.json` with it, so a per-group store would boot every pane
    /// logged out. `docs/design/codex.md` carries the argument. The practical
    /// consequence for this watcher is that the store it polls is shared with
    /// the human's own codex sessions, which is why a cwd match is required
    /// and a contest is refused rather than resolved.
    Codex { ids: HashSet<String>, root: PathBuf },
}

impl SessionBaseline {
    pub(in crate::orchestration) fn cli(&self) -> &'static str {
        match self {
            SessionBaseline::Copilot { .. } => "copilot",
            SessionBaseline::OpenCode { .. } => "opencode",
            SessionBaseline::Codex { .. } => "codex",
        }
    }
    pub(in crate::orchestration) fn poll(&self) -> Duration {
        match self {
            SessionBaseline::Copilot { .. } => COPILOT_SESSION_POLL,
            SessionBaseline::OpenCode { .. } => OPENCODE_SESSION_POLL,
            SessionBaseline::Codex { .. } => CODEX_SESSION_POLL,
        }
    }
    pub(in crate::orchestration) fn timeout(&self) -> Duration {
        match self {
            SessionBaseline::Copilot { .. } => COPILOT_SESSION_TIMEOUT,
            SessionBaseline::OpenCode { .. } => OPENCODE_SESSION_TIMEOUT,
            SessionBaseline::Codex { .. } => CODEX_SESSION_TIMEOUT,
        }
    }
}

/// **Whether a pane's session watch must wait for the pane's own first tool
/// call** rather than start at the spawn (#3723).
///
/// Every freshly spawned pane but one is typed a kickoff, so its CLI starts a turn — and
/// writes its session — within seconds of the spawn, and a watch that starts
/// then is looking at a store in which the pane's own session is about to be
/// the new one. An idle quick root is typed nothing. Its CLI may not write a
/// session until the human's first message, which can be an hour away, and
/// for all of that time a watch would be looking for "a new session in this
/// directory" with the pane's own not there to find.
///
/// That only matters where somebody else's session can turn up in the same
/// store and the same directory, which is one variant:
///
/// - [`SessionBaseline::Codex`] — the store is the HUMAN's, shared with their
///   own terminal sessions, and a quick root's directory is their checkout. A
///   `codex` they start there while the root waits is new, in that directory
///   and unclaimed: the watch would bind it, usage would be read from it, and
///   a Resume would re-open their conversation as the root. Deferred. Once
///   the root has made a tool call its own session exists, so the search
///   finds that one — or finds two and answers `Contested`, never a guess.
/// - [`SessionBaseline::OpenCode`] — the store is this GROUP's own. No
///   session of the human's is ever in it. Not deferred.
/// - [`SessionBaseline::Copilot`] — the store is the human's, but copilot
///   writes its session a few seconds into boot whether or not it is typed
///   anything, so the window is what it always was. Deferring would widen it:
///   copilot's search takes the NEWEST new session, and a later one of the
///   human's would outrank the root's. Not deferred.
///
/// A pane that was typed a kickoff is never deferred, whatever its CLI.
#[doc(hidden)] // pub for integration tests
pub fn defers_session_watch(idle_start: bool, baseline: &SessionBaseline) -> bool {
    idle_start && false && matches!(baseline, SessionBaseline::Codex { .. })
}

/// The outcome of one poll of a session store.
///
/// `Contested` and `Unreadable` exist so that giving up can say *why*: a pane
/// that never identified because two sessions matched needs a different answer
/// from a human than one whose CLI never wrote a session at all, and both
/// otherwise surface only as "no session id".
#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)] // pub for integration tests
pub enum SessionSearch {
    /// Nothing new yet — keep polling. The normal answer for a booting pane.
    Waiting,
    Found(String),
    /// More than one candidate, carrying how many. Refused, never guessed —
    /// see `opencodedb::identify_session_on`.
    Contested(usize),
    Unreadable(String),
}

impl SessionSearch {
    /// What to record when the watch gives up in this state.
    pub(in crate::orchestration) fn untracked_reason(&self) -> String {
        match self {
            // `Found` cannot reach here (the watcher returns on it), but a
            // reason string is not the place to `unreachable!()` a background
            // thread over.
            SessionSearch::Waiting | SessionSearch::Found(_) => {
                "no new session appeared before timeout".to_string()
            }
            SessionSearch::Contested(n) => format!(
                "{n} candidate sessions matched this pane's directory and none could be told \
                 apart — refused rather than bind the wrong conversation"
            ),
            SessionSearch::Unreadable(e) => format!("session store unreadable: {e}"),
        }
    }
}

/// The MCP server name this app declares to every agent CLI. Gemini needs it as
/// a value (`--allowed-mcp-server-names`, its analogue of claude's
/// `--strict-mcp-config`: an allowlist rather than a "only my file" switch), so
/// the string that was a literal in three places is now one constant — and
/// since #1153 phase 3 it is [`brand::MCP_SERVER`], the same word the data dir
/// and the audit actor use, because a rename that moved one of them and not the
/// others is a group whose allowlist denies its own tools.
pub const MCP_SERVER: &str = brand::MCP_SERVER;

/// `{ "<name>": <server> }` — the one-entry server map every CLI's generated
/// config wraps its server entry in, keyed by [`MCP_SERVER`] so the name
/// on argv (`--allowed-mcp-server-names <server>`, `--allow-tool <server>`,
/// `--allowedTools mcp__<server>`) and the name in the file can't drift
/// apart — every one of those argv spellings is now built from this const or
/// from [`brand::MCP_TOOL_PREFIX`], which a unit test derives from it.
/// `{ "<agent token header>": <token> }` — the auth block every CLI's
/// generated MCP config carries, minted from [`brand::AGENT_TOKEN_HEADER`] so
/// the name a config PRESENTS cannot drift from the set `mcp.rs` accepts.
/// Written under one spelling; read under every spelling (#1153 phase 3).
pub(in crate::orchestration) fn agent_token_headers(token: &str) -> Value {
    let mut m = serde_json::Map::new();
    m.insert(brand::AGENT_TOKEN_HEADER.to_string(), Value::String(token.to_string()));
    Value::Object(m)
}

pub(in crate::orchestration) fn one_server_map(server: Value) -> Value {
    let mut m = serde_json::Map::new();
    m.insert(MCP_SERVER.to_string(), server);
    Value::Object(m)
}

/// The extra, CLI-specific environment a *group* agent's pane needs on top of
/// [`OrchRegistry::agent_pane_env`]'s shims — today, gemini's settings-path
/// override and nothing else.
///
/// Separate from `agent_pane_env` on purpose: that one returns nothing at all
/// when the gh/git shims can't be written, and a gemini agent silently losing
/// its MCP identity because git wasn't installed would be a confusing failure a
/// long way from its cause. Pure, so the mapping is testable without a spawn.
///
/// `token` is read by the codex arm alone (#2515 C1), and it is why this
/// function grew a parameter rather than codex getting its own: codex is the
/// one adapter whose pane environment carries this agent's *identity* instead
/// of merely pointing at the file that does. Passing it here keeps the
/// "one mapping, testable without a spawn" property the doc above claims —
/// deriving the variable at the call site would put the token's spelling in a
/// place no test can reach.
pub fn cli_extra_env(cli: &str, cfg: &Path, token: &str) -> Vec<(String, String)> {
    match cli {
        "gemini" => vec![(GEMINI_SETTINGS_ENV.to_string(), cfg.display().to_string())],
        // codex (#2515 C1): `cfg` is SELECTED on argv by name (`-p`), so the
        // path itself needs no variable — what rides here is the agent token,
        // which the generated profile names in `env_http_headers` and never
        // contains. That is plan D2, and it is the half of the amendment that
        // survives verbatim: a GROUP pane has a pane environment, so its secret
        // goes there. A SOLO pane has none, and `solo_prepare` puts the token
        // in the file instead — see `CodexMcpAuth`.
        //
        // An empty token means the caller minted none (`solo_prepare`'s
        // no-seam path). Exporting an empty variable would leave the pane
        // presenting a blank auth header, which the MCP server refuses in a way
        // that reads like a bug rather than like an absence — so emit nothing.
        "codex" if !token.is_empty() => {
            vec![(CODEX_TOKEN_ENV.to_string(), token.to_string())]
        }
        // pi (#2126): `cfg` is named on ARGV (`--mcp-config`), so none of this
        // pane's MCP identity rides the environment — the one variable here is
        // the boot-time version check, and it is unrelated to `cfg`.
        //
        // **`PI_MCP_CONFIG_MODE=exclusive` is deliberately NOT set, and the
        // reason is a fact about the adapter rather than a preference.** In
        // exclusive mode the adapter discards the `--mcp-config` override —
        // `getEffectivePiGlobalConfigPath` passes `undefined` for it — and
        // reads one fixed per-user file instead. So exclusivity and a
        // per-agent config are mutually exclusive at the pin, loomux takes the
        // per-agent config, and the merge that leaves is measured by
        // `pi_repo_mcp_exposure` and documented as an open residual in
        // `docs/design/pi.md`. Setting the variable here would not harden this
        // pane; it would point it at somebody else's file.
        "pi" => vec![(PI_SKIP_VERSION_CHECK_ENV.to_string(), "1".to_string())],
        _ => Vec::new(),
    }
}

/// What [`OrchRegistry::write_mcp_config`] produced for one agent: the file it
/// wrote, and the pane environment that CLI needs in order to read it.
///
/// The two travel together because for two of the four adapters they are one
/// artifact. Claude and Copilot name the file on argv and need no env at all;
/// gemini's file is named by an environment variable; and opencode's document
/// does not reach the CLI as a file at all — the file is the audit copy and the
/// env carries the bytes (see [`OPENCODE_CONFIG_CONTENT_ENV`]). Returning the
/// path alone would leave every caller to re-derive the env from the CLI name,
/// which is the "re-derived at a call site that then silently disagrees"
/// failure [`CliCaps`]' own doc argues against.
pub struct AgentCliConfig {
    /// The generated file. Named on argv by the CLIs that can (`--mcp-config`,
    /// `--additional-mcp-config`); an audit artifact for the ones that can't.
    pub path: PathBuf,
    /// Extra pane environment, on top of [`OrchRegistry::agent_pane_env`]'s
    /// shims. Empty for an argv-seam CLI.
    pub env: Vec<(String, String)>,
}

/// Per-CLI flags that put a *standalone* single-pane agent into the same
/// unattended "autopilot / allow all" posture group workers get — minus the
/// MCP/session/workspace wiring only a managed agent needs. Built from the SAME
/// atoms as `build_agent_command` (#101) so the launcher and the orchestration
/// path can't drift.
///
/// Returns an empty string for CLIs with no known unattended flag surface
/// (custom, and anything not in the table): the toggle is a no-op there rather
/// than inventing flags that may not exist.
///
/// **Empty does not always mean "no posture"** (#2515 C1). Two rows return an
/// empty string for two different reasons, and the arms say which: pi has no
/// permission prompts to bypass at all, while codex has a real approval policy
/// that loomux sets in the profile file rather than on the line. A reader who
/// takes an empty return as "loomux does nothing about this CLI's posture"
/// would be right about pi and wrong about codex.
///
/// Ante (#292) is launcher-only here, same Tier-A shape as Hermes (#284):
/// this function and the `AGENTS` catalog entry (`src/agents.ts`) are the only
/// changes. `SUPPORTED_CLIS`, `build_agent_command`, and `write_mcp_config`
/// deliberately do NOT gain an ante arm — Ante's MCP servers are configured
/// exclusively via `~/.ante/settings.json` (no `--mcp-config`-equivalent CLI
/// flag exists per docs). Ante stays a delivery-only channel member. (It used
/// to say "like codex"; #2515 C1 gave codex a spawn adapter and a real seam,
/// so Ante is now alone in that class and the comparison would send a reader
/// to a row that says the opposite.) (PR #323 recorded that as "adding ante to `SUPPORTED_CLIS`
/// would hit `solo_prepare`'s `unreachable!` arm"; #267 removed that trap by
/// deriving the solo seam from [`CliCaps::mcp_argv_seam`] instead of
/// `SUPPORTED_CLIS` — but the rest of the argument, that loomux has no ante
/// spawn adapter, stands unchanged.)
pub fn single_pane_autopilot_flags(program: &str) -> String {
    match program.trim().to_lowercase().as_str() {
        "claude" => format!(
            "--permission-mode {} --allowedTools {CLAUDE_UNATTENDED_ALLOW}",
            claude_permission_mode(true)
        ),
        // #364: a single-pane copilot agent now enters the SAME true autopilot
        // mode as a group agent — reuses COPILOT_GROUP_AUTOPILOT_FLAGS verbatim
        // rather than inventing a divergent string, so the two postures can't
        // drift apart. The resulting "Enable autopilot mode" dialog (triggered
        // by the human's own first submit, since a solo pane gets no
        // programmatic kickoff) is answered by a dedicated watcher —
        // `OrchRegistry::confirm_solo_copilot_autopilot`, started right after
        // spawn — not left for the human to stare at unanswered.
        "copilot" => COPILOT_GROUP_AUTOPILOT_FLAGS.to_string(),
        // Hermes: "Bypass dangerous-command approval prompts", and docs show no
        // startup consent dialog (unlike copilot's --autopilot) — safe for a
        // single-pane launch. cli-commands.md:
        // https://github.com/NousResearch/hermes-agent/blob/main/website/docs/reference/cli-commands.md
        "hermes" => "--yolo".to_string(),
        // Ante: yolo permission mode runs "all tools execute automatically
        // without rule evaluation or prompts" (docs/configuration/permission.mdx);
        // usage/approvals.mdx confirms it's an all-or-nothing bypass with no
        // documented startup dialog, same shape as Hermes's --yolo above.
        // https://github.com/AntigmaLabs/ante-preview/blob/main/docs-site/docs/configuration/permission.mdx
        // https://github.com/AntigmaLabs/ante-preview/blob/main/docs-site/docs/usage/approvals.mdx
        //
        // NOTE (#292): Ante is documented "macOS and Linux only" (README) — no
        // Windows binary ships in any release. This arm is correct-per-docs but
        // unreachable on loomux's Windows-only target until Ante ships Windows
        // support (or a WSL bridge is built as a follow-up); wired now so the
        // launcher entry (src/agents.ts) doesn't silently no-op the toggle.
        "ante" => "--yolo".to_string(),
        // Gemini (#267): the same atom the group path builds — see
        // `build_agent_command`'s gemini arm. `--approval-mode yolo` is the
        // documented spelling (`--yolo` is deprecated in favour of it per the
        // CLI reference), and unlike copilot's `--autopilot` no startup consent
        // dialog is documented for it, so a solo pane needs no answering
        // watcher. Wired here for the #101 invariant: the launcher toggle and
        // the group spawn must mean the same thing on every CLI loomux can
        // spawn, and gemini is now one.
        "gemini" => GEMINI_UNATTENDED_FLAGS.to_string(),
        // OpenCode (#722): the same atom the group path builds — see
        // `OPENCODE_UNATTENDED_FLAGS` for why `--auto` and not one of its two
        // hidden aliases. No startup consent dialog is documented or present
        // for it (unlike copilot's `--autopilot`), so a solo pane needs no
        // answering watcher. Wired here for the #101 invariant: the launcher
        // toggle and the group spawn must mean the same thing on every CLI
        // loomux can spawn, and opencode is now one.
        "opencode" => OPENCODE_UNATTENDED_FLAGS.to_string(),
        // pi (#2126): the same atom the group path builds, and it is EMPTY —
        // see `PI_UNATTENDED_FLAGS` for why that is a measured claim about pi
        // rather than an unfilled arm. Wired explicitly, sharing the atom,
        // for the #101 invariant: the launcher toggle and the group spawn
        // must mean the same thing on every CLI loomux can spawn, and "they
        // agree that there is nothing to turn on" is a claim two independent
        // `String::new()`s could not make. Falling through to the `_` arm
        // would produce the same string and evidence nothing.
        "pi" => PI_UNATTENDED_FLAGS.to_string(),
        // codex (#2515 C1): EMPTY, and unlike pi's empty this one is a
        // statement about WHERE the posture lives rather than about whether it
        // exists. codex's approval policy is real and loomux does set it — as
        // `approval_policy` in the generated profile — but a solo pane's
        // profile is written by `solo_prepare`, which has already decided the
        // posture by the time this function is asked, and there is no codex
        // FLAG that means the same thing. `-a/--ask-for-approval` exists on the
        // TUI, and emitting it here would give a solo pane a posture its own
        // profile disagrees with; the bypass flag is never emitted by loomux at
        // all.
        //
        // Wired explicitly rather than falling through to `_`, for the #101
        // invariant and for the reason pi's arm gives: "they agree there is
        // nothing to put on the LINE" is a claim, and two independent
        // `String::new()`s could not make it.
        "codex" => String::new(),
        _ => String::new(),
    }
}

/// 128-bit hex token from std's OS-seeded `RandomState` (each instance draws
/// fresh OS entropy) mixed with time. Deliberately not getrandom-based: see
/// the Cargo.toml note on bcryptprimitives/ProcessPrng. Tokens authenticate
/// same-user localhost agents; that adversary can read the config files
/// anyway, so this strength is proportionate.
pub(in crate::orchestration) fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(32);
    for i in 0..2u64 {
        let mut h = std::hash::RandomState::new().build_hasher();
        h.write_u64(now_ms());
        h.write_u64(i);
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}
