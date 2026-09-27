//! Codex's adapter: the per-agent profile file (name, TOML, MCP exposure) and
//! the worktree git access it grants.
//! Design note: `docs/design/codex.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. IO: fs. It calls
//! no sibling file.

use super::*;

// ─────────────────────────── codex (#2515 C1) ───────────────────────────
//
// Everything loomux configures on a codex pane rides ONE generated file:
// `CODEX_HOME/<brand>-<agent>.config.toml`, selected with `-p/--profile`,
// documented "Layer $CODEX_HOME/<name>.config.toml on top of the base user
// config" (`utils/cli/src/shared_options.rs` at the pin in
// `docs/design/codex.md`; the name is built by `resolve_profile_v2_config_path`,
// `format!("{profile_name}{CONFIG_PROFILE_V2_SUFFIX}")` against `codex_home`).
// That is a whole `ConfigToml` document layered over the human's own — NOT the
// narrow `[profiles.<name>]` table, which is the legacy shape and cannot carry
// `projects`, `mcp_servers` or `developer_instructions` at all.
//
// The design note carries the argument for every key and for the three routes
// not taken (`-c` on argv, a project `.codex/config.toml`, a per-agent
// `CODEX_HOME`). What lives here is the mechanism.

/// The environment variable a codex GROUP pane carries its agent token in —
/// the value `env_http_headers` names, never the token itself (#2515 C1, D2).
///
/// A literal rather than a value built from [`brand::ENV_PREFIX`] for the
/// reason [`brand::MCP_TOOL_PREFIX`] is one: a `const` cannot be assembled
/// from another `const` string, and the pairing is kept honest by a test that
/// derives it from the prefix instead
/// (`the_codex_token_env_var_is_the_brand_prefix_plus_agent_token`).
///
/// **Deliberately NOT exported under the legacy prefix as well.** Every other
/// `ORRERIX_`/`LOOMUX_` pair in `agent_pane_env` exists because something
/// already on disk — a shim, an operator's wrapper — reads the old name. This
/// variable is read by exactly one thing, the profile file loomux wrote in the
/// same call, so a second spelling would be a second copy of a secret in the
/// pane's environment with no reader at all.
pub(in crate::orchestration) const CODEX_TOKEN_ENV: &str = "ORRERIX_AGENT_TOKEN";

/// The suffix a profile-v2 file takes under `CODEX_HOME` — the shape the orphan
/// sweep and `codex_profile_name_of_path` STRIP, as opposed to the one the
/// writer builds.
///
/// **The two directions are spelled differently on purpose, and the difference
/// is not a drift.** A const cannot appear inside a `format!` template's
/// extension position without making the site invisible to
/// `no_raw_identifier_is_interpolated_into_a_file_name`, whose trigger is an
/// extension LITERAL in the template (constraint 6). A file name loomux builds
/// from an identifier must be visible to that scan, so
/// [`codex_profile_file_name`] writes `.config.toml` literally and carries an
/// allowlist row arguing why it is safe. The readers cannot use a literal —
/// `strip_suffix` needs the value, not a template — so they take this const.
///
/// `the_codex_profile_suffix_and_its_file_name_builder_agree` pins the two
/// spellings equal, which is what keeps #502's "a delete path that re-derives a
/// write path's shape either misses files or matches too widely" from applying:
/// they are one fact checked in one place, not two literals maintained apart.
pub(in crate::orchestration) const CODEX_PROFILE_FILE_EXT: &str = ".config.toml";

/// The file name one agent's codex profile takes — **the one place an
/// identifier becomes a codex profile file name** (constraint 6).
///
/// Split out of its two callers so there is a single declared assembly point,
/// exactly as `claude_transcript_path` and `pi_session_file_in_dir` are, and it
/// takes the same proof at its signature: a `&PathSegment` cannot be handed a
/// raw `&str`, so the type is the guarantee and the scan's allowlist row pins
/// the type rather than the line.
///
/// The value interpolated is narrower still than the parameter — the name comes
/// from [`codex_profile_name`], which refuses anything outside codex's own
/// `[A-Za-z0-9_-]` alphabet — but the signature is what a textual scan can
/// check, so that is what the row names.
#[doc(hidden)] // pub for integration tests
pub fn codex_profile_file_name(agent: &PathSegment) -> Result<String, String> {
    let name = codex_profile_name(agent)?;
    Ok(format!("{name}.config.toml"))
}

// **The generated profile deliberately sets NEITHER MCP timeout** (#2515 C1,
// review round 1 finding 2), and the absence is the decision rather than an
// omission — which is why it is written down where the keys would have been.
//
// The first version of this file wrote `tool_timeout_sec = 30.0` and
// `startup_timeout_sec = 20.0`, documented as RAISES over codex's "60s" and
// "10s" defaults. Both defaults were wrong: they came from #2515's slice plan,
// which took them from the published config reference, and I transcribed them
// onto a permanent surface instead of reading the source. At the pin codex
// resolves both keys in `codex-mcp/src/connection_manager.rs` with
// `.unwrap_or(DEFAULT_…)` against `codex-mcp/src/rmcp_client.rs`:
//
//     pub(crate) const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
//     pub(crate) const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);
//
// So the shipped values were REDUCTIONS — the tool timeout by 10× — carrying a
// rationale ("loomux's tools do real work behind a call, and one timing out
// reads to an agent as the tool being broken") that argues for the exact
// opposite. A `spawn_agent` behind a large group can legitimately run well past
// 30 seconds; stock codex would have waited 300.
//
// Writing a key whose only effect is to make the pane worse than the vendor's
// own default is indefensible, so both keys are gone and codex's defaults
// stand. Nothing is lost that pi's `PI_MCP_TIMEOUT_MS` buys: that constant
// raises the pi adapter's own shorter default TO 30s, and codex already gives
// ten times that.
//
// `a_codex_profile_sets_no_mcp_timeout_and_says_why` pins the absence, so a
// later edit that re-adds a number has to argue with a test rather than with a
// comment.

/// How a codex pane's profile presents this agent's token.
///
/// Two shapes, because a pane can only carry a secret on a channel it HAS, and
/// the two kinds of codex pane do not have the same one. This is an amendment
/// to #2515's plan D2, agreed before it was written and recorded on the issue:
/// D2 said the token rides the pane environment and is never in the file,
/// which is right for the pane D2 was describing and impossible for the other.
///
/// - [`Self::EnvVar`] — a GROUP pane. `write_mcp_config` returns the pane
///   environment alongside the file and the spawn applies it, so the file
///   names a variable and holds no token byte. D2 verbatim.
/// - [`Self::Literal`] — a SOLO pane. `solo_prepare` only ever appends a flag
///   string to a command line the human owns; it sets no environment at all
///   (its pi arm says so outright). A solo profile naming a variable nothing
///   sets would connect with no auth header — and the pane would still be
///   advertised `delivery_only: false`, which is exactly the
///   advertised-status-disagrees-with-reality defect `solo_prepare` guards
///   against at its own fallback arm. So the token goes in the file, as every
///   other CLI's generated config already carries it, in a directory that is
///   the human's own.
#[derive(Clone, Copy, Debug)]
#[doc(hidden)] // pub for integration tests
pub enum CodexMcpAuth<'a> {
    /// Name the environment variable; the value never touches the file.
    EnvVar(&'a str),
    /// Carry the token itself — a solo pane, which has no environment.
    Literal(&'a str),
}

/// Whether a TOML basic string keeps its newlines (a multi-line `"""` body) or
/// escapes them (a one-line `"…"` value, or a quoted key).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TomlNewlines {
    Escape,
    Keep,
}

/// Escape `s` for a TOML **basic** string — the double-quoted form, which is
/// the only one that can encode arbitrary text.
///
/// **Why not the `'''` literal form #2515's plan named.** A TOML literal string
/// admits no escapes at all: its content ends at the first closing run of three
/// apostrophes, and there is nothing one can write instead. "Escaping" it
/// therefore means REWRITING the role contract on its way to the agent — and a
/// contract that reaches the model altered is worse than one that is encoded,
/// because the alteration is invisible from both ends. The basic form encodes
/// losslessly, and the encoding is total: every `"` becomes `\"`, so a run of
/// three cannot appear in the body; every `\` becomes `\\`, so a trailing
/// backslash cannot become a line continuation; and every other control
/// character becomes `\uXXXX`, which the literal form forbids outright.
///
/// `Keep` leaves real newlines in place, for a `"""` body: that is what makes a
/// multi-KB contract readable when a human opens the file to debug a pane, and
/// it is safe precisely because the two characters that could close the
/// delimiter early are already escaped.
fn toml_basic_escape(s: &str, newlines: TomlNewlines) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' if newlines == TomlNewlines::Keep => out.push('\n'),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Every other C0 control, plus DEL. TOML permits none of them raw
            // in a basic string, and a role contract folded together from repo
            // prose is not guaranteed free of them.
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// The profile name for one agent — `<brand>-<agent id>`, and the ONE place
/// that shape is derived. Four sites need it: the writer, `solo_prepare` (which
/// spells it on argv), the per-agent removal, and the orphan sweep.
///
/// **Unreachable today, and kept anyway — with the reason stated, because an
/// unexplained unreachable check is indistinguishable from a forgotten one.**
///
/// The two alphabets are `ProfileV2Name`'s (a non-empty run of ASCII
/// alphanumerics, `_` and `-`) and [`check_segment`]'s (the same run, and then
/// two FURTHER refusals: no leading `-`, no Windows reserved device name). So
/// `PathSegment` is strictly NARROWER than what codex accepts, `brand::NAME` is
/// itself alphanumeric, and every `PathSegment` this is handed therefore
/// produces a name `-p` can select. The `Err` arm cannot be reached from a
/// valid `PathSegment` at all.
///
/// It stays because the relationship it depends on is somebody else's to
/// change, in both directions: `check_segment` serves four identifier families
/// (CLAUDE.md constraint 6) and could widen for one of them, and codex could
/// narrow `ProfileV2Name`. A file written under a name `-p` cannot select is a
/// pane that boots with no MCP, no trust and no contract, and NOTHING says so —
/// codex resolves the profile itself and simply does not find one. Failing the
/// spawn here, next to the write that would have been useless, is the cheap
/// side of that trade.
///
/// `a_codex_profile_name_is_valid_by_construction_and_the_check_is_the_backstop`
/// pins the subset relationship rather than pretending to exercise the `Err`
/// arm with an input that cannot exist — so a widening of either alphabet
/// reddens a test that tells you this check just became live.
#[doc(hidden)] // pub for integration tests
pub fn codex_profile_name(agent: &PathSegment) -> Result<String, String> {
    let name = format!("{}-{agent}", brand::NAME);
    if name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')) {
        return Ok(name);
    }
    Err(format!(
        "agent id {agent:?} cannot become a codex profile name: codex accepts only ASCII \
         alphanumerics, '_' and '-' in a --profile value, and {name:?} carries something else"
    ))
}

/// Recover a profile NAME from the profile FILE path — the inverse of
/// [`codex_profile_name`] plus [`CODEX_PROFILE_FILE_EXT`], and the one thing
/// the launch-line builders need that they are not handed.
///
/// They receive `cfg`, the generated file, exactly as every other adapter does;
/// `-p` wants the name that file is stored under. Deriving it from the path
/// rather than threading the agent id into two fourteen-argument builders keeps
/// the naming in ONE place — both directions go through the same constant — and
/// `a_codex_profile_path_round_trips_to_the_name_the_launch_line_spells` pins
/// the round trip so the pair cannot drift the way #502's write and delete
/// paths did.
///
/// `Path::file_stem` is deliberately not used: the extension has two dots, so a
/// stem would answer `orrerix-w-3.config` — a name `-p` would look for under a
/// file that is not there.
///
/// `None` is **unreachable on the codex path**, and it is worth being precise
/// about why rather than calling it a degrade. Every codex pane's `cfg` comes
/// from `write_mcp_config`'s codex branch, which returns the profile file it
/// just wrote — so the suffix always strips. If it somehow did not, the pane
/// would launch with the human's base config and no orrerix layer, which is
/// **not** a graceful outcome: no trust key means it boots into the trust
/// dialog and eats its kickoff. That is at least LOUD — a human sees the
/// dialog — where the alternative, emitting `-p` for a name codex cannot
/// resolve, is an error at startup nobody is watching for. Neither is good;
/// this is the less bad one, and the real defence is that the value comes from
/// the writer rather than from a caller.
#[doc(hidden)] // pub for integration tests
pub fn codex_profile_name_of_path(cfg: &Path) -> Option<&str> {
    cfg.file_name()?.to_str()?.strip_suffix(CODEX_PROFILE_FILE_EXT)
}

/// The directories a codex pane must be able to write to COMMIT from a linked
/// git worktree (#3456): its own gitdir, plus the three directories of the shared
/// `.git` a commit and a push write — `objects`, `refs`, `logs`. `Ok(vec![])`
/// for a pane that is not in a linked worktree (a main clone, whose `.git` is a
/// directory, or no repo at all): nothing extra, codex's own posture stands.
///
/// **Why a worktree pane cannot commit without this.** Under `workspace-write`
/// codex keeps every writable root's `.git` read-only, and for a `.git` FILE it
/// resolves the `gitdir:` pointer and protects that directory too
/// (`default_read_only_subpaths_for_writable_root`, protocol/src/permissions.rs
/// at rust-v0.156.1) — on Windows as DENY ACEs, which is what made
/// `<repo>/.git/worktrees/<name>/index.lock` unwritable. An explicit `write`
/// entry for the SAME path suppresses that default (`has_explicit_resolved_path_entry`
/// in `get_writable_roots_with_cwd_impl`, and `append_default_read_only_path_if_no_explicit_rule`
/// on the legacy path — both compare the two paths with `==`), so the gitdir is
/// spelled here exactly as codex resolves it:
/// the pointer file's own text, joined onto the pane's directory and folded
/// lexically, the way `resolve_gitdir_from_file` does. Asking `git rev-parse
/// --git-dir` instead would risk a differently spelled path, and a spelling that
/// misses by one component leaves the DENY in place with nothing to say so.
///
/// **Why not the whole `.git`.** `hooks/`, `config` and `info/` are where a
/// write turns into code the HUMAN'S unsandboxed git runs (a hook,
/// `core.fsmonitor`, `core.hooksPath`) — the reason codex protects `.git` at
/// all. A commit and a push need none of them; `docs/design/codex.md`
/// (§Committing from a worktree) has the measurement and what the narrow set
/// costs.
///
/// **`Err` is a refusal with a reason, never a guess.** A `.git` file whose
/// layout is not git's own linked-worktree shape — the gitdir not sitting at
/// `<common>/worktrees/<name>`, or its `commondir` naming a different directory
/// — grants nothing. The pane cannot rewrite `commondir` itself
/// ([`codex_worktree_git_access`] seals it read-only), but an unsandboxed peer or
/// the human can, and the layout check is what stops such a file from pointing a
/// spawn's write entries at an arbitrary `objects`/`refs`/`logs` elsewhere.
#[doc(hidden)] // pub for integration tests
pub fn codex_worktree_git_roots(workdir: &Path) -> Result<Vec<PathBuf>, String> {
    let dot_git = workdir.join(".git");
    if !dot_git.is_file() {
        return Ok(Vec::new());
    }
    // Every message below LEADS with what was wrong and trails the paths: the
    // caller caps the audit row's reason to `NOTICE_FIELD_CAP` characters, and a
    // temp-dir path alone can be longer than that.
    let text = codex_read_git_meta(&dot_git, ".git")?;
    // codex's own parse: the FIRST colon, a `gitdir` prefix, a trimmed value.
    let raw = match text.trim().split_once(':') {
        Some((prefix, raw)) if prefix.trim() == "gitdir" && !raw.trim().is_empty() => raw.trim(),
        _ => return Err(format!(".git is not a `gitdir: <path>` pointer: {}", dot_git.display())),
    };
    let gitdir = codex_fold_path(&workdir.join(raw));
    if !gitdir.is_dir() {
        return Err(format!("gitdir is not a directory: {}", gitdir.display()));
    }
    let parent = gitdir.parent().filter(|p| p.file_name() == Some(std::ffi::OsStr::new("worktrees")));
    let Some(common) = parent.and_then(Path::parent) else {
        return Err(format!("gitdir is not at <common>/worktrees/<name>: {}", gitdir.display()));
    };
    let commondir = gitdir.join("commondir");
    let named = codex_read_git_meta(&commondir, "commondir")?;
    if codex_fold_path(&gitdir.join(named.trim())).as_path() != common {
        return Err(format!(
            "commondir names {:?}, not the common dir {} its location implies",
            named.trim(),
            common.display()
        ));
    }
    let mut roots = vec![gitdir.clone()];
    // Only the ones that exist: codex skips a missing root on Windows anyway,
    // and a path that is not there is not one to hand another platform's
    // sandbox to bind.
    roots.extend(["objects", "refs", "logs"].iter().map(|d| common.join(d)).filter(|p| p.is_dir()));
    Ok(roots)
}

/// The name of the permissions profile a worktree pane's codex profile defines
/// and selects. Branded so a human reading their own merged config can tell
/// whose it is; not `:`-prefixed, which codex reserves for built-ins
/// (`validate_user_permission_profile_names`).
pub const CODEX_WORKTREE_PERMISSIONS: &str = "orrerix-worktree";

/// What a codex pane's sandbox is told about git: directories it may write, and
/// files inside them it may NOT. Empty for every pane that is not in a linked
/// worktree, and then the profile keeps codex's legacy `workspace-write` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexGitAccess {
    pub write: Vec<PathBuf>,
    pub read_only: Vec<PathBuf>,
}

/// The files in a linked worktree's gitdir that change what git — including
/// the HUMAN'S unsandboxed git — does in that worktree, and that a codex pane
/// therefore must not write even though the gitdir itself is writable (#3456,
/// the human's call in review round 1):
///
/// - `commondir` — which `.git` is the common dir, and so which `config` and
///   `hooks/` apply. Rewritten, the next `git status` the human runs there can
///   execute a hook or a `core.fsmonitor` the pane chose.
/// - `config.worktree` — per-worktree config, read when the repo sets
///   `extensions.worktreeConfig`; a config key is code by the same route.
/// - `gitdir` — the back-pointer `git worktree repair` WRITES a `.git` file
///   through: rewritten, the human's next repair writes where the pane chose.
///
/// `hooks`, `config`, `info` and `objects` resolve to the COMMON dir for a
/// linked worktree (git's `common_list`), and `HEAD`, `index`, `ORIG_HEAD`,
/// `FETCH_HEAD` and `logs/` are what a commit must write.
///
/// **Not sealed, and a real route (residual, #3460 review N4):** the rebase
/// state. `rebase-merge/git-rebase-todo` is per-worktree and must stay writable
/// for the pane's own rebase, and an `exec` line planted there runs on the
/// HUMAN'S next `git rebase --continue` in that worktree. It cannot be sealed
/// here: the directory is created per rebase, so there is nothing to deny at
/// spawn, and denying it would break the rebase the pane needs. The user doc
/// tells the human not to continue a rebase in a pane's worktree they did not
/// start.
const CODEX_GITDIR_SEALED: [&str; 3] = ["commondir", "config.worktree", "gitdir"];

/// [`codex_worktree_git_roots`] plus the sealed files, for the profile.
///
/// **One side effect, deliberately:** an absent `config.worktree` is created
/// EMPTY. A deny can only be set on a path that exists (codex's Windows
/// `compute_allow_paths_for_permissions` skips a missing one), so without it the
/// pane could create the file the seal exists to stop. An empty
/// `config.worktree` means nothing to git: it is read only under
/// `extensions.worktreeConfig`, and then it sets nothing. It is created with
/// `create_new`, so a file already there is never touched. `commondir` and
/// `gitdir` are NOT created: git writes both for every linked worktree, so a
/// gitdir missing either is refused rather than repaired, and an empty `gitdir`
/// is one `git worktree prune` would read as broken.
#[doc(hidden)] // pub for integration tests
pub fn codex_worktree_git_access(workdir: &Path) -> Result<CodexGitAccess, String> {
    let write = codex_worktree_git_roots(workdir)?;
    let Some(gitdir) = write.first().cloned() else {
        return Ok(CodexGitAccess::default());
    };
    let mut read_only = Vec::new();
    for name in CODEX_GITDIR_SEALED {
        let path = gitdir.join(name);
        if name == "config.worktree" {
            match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("config.worktree cannot be sealed: {}: {e}", path.display())),
            }
        } else if !path.is_file() {
            return Err(format!("{name} missing, cannot be sealed: {}", path.display()));
        }
        read_only.push(path);
    }
    Ok(CodexGitAccess { write, read_only })
}

/// The most of a `.git` pointer or a `commondir` [`codex_worktree_git_roots`]
/// will read. Both are one path and a newline when git writes them; a real
/// one is a few hundred bytes at most.
const CODEX_GIT_META_CAP: u64 = 4096;

/// Read one of those small git metadata files, BOUNDED (#3456 review N1).
/// `commondir` sits in the gitdir, which this PR makes writable by the pane, so
/// its size is the pane's choice: an unbounded read would let it put an
/// arbitrarily large read on the next spawn's path. So the read stops at the
/// cap plus one byte, and an oversized file is refused by its size alone,
/// never echoed.
fn codex_read_git_meta(path: &Path, what: &str) -> Result<String, String> {
    use std::io::Read;
    let file = fs::File::open(path).map_err(|e| format!("{what} unreadable: {}: {e}", path.display()))?;
    let mut buf = Vec::new();
    file.take(CODEX_GIT_META_CAP + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("{what} unreadable: {}: {e}", path.display()))?;
    if buf.len() as u64 > CODEX_GIT_META_CAP {
        return Err(format!("{what} is larger than {CODEX_GIT_META_CAP} bytes: {}", path.display()));
    }
    String::from_utf8(buf).map_err(|_| format!("{what} is not UTF-8: {}", path.display()))
}

/// `.`/`..` folded lexically, as codex's `AbsolutePathBuf` normalization does —
/// deliberately not canonicalized: the point is to spell the gitdir the way
/// codex spells it, and codex does not resolve symlinks here either. Duplicated
/// per module, house style (see `fileedit::lexical_normalize`).
fn codex_fold_path(p: &Path) -> PathBuf {
    let mut out: Vec<std::path::Component<'_>> = Vec::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if matches!(out.last(), Some(std::path::Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// The generated profile document for one codex pane — the whole of what loomux
/// configures on codex.
///
/// Pure and `pub` so the document is assertable without a spawn, for the reason
/// `gemini_settings_json`'s doc gives: this file IS a codex agent's trust, its
/// posture, its identity and its contract, so "does it say what we think it
/// says" must be answerable directly.
///
/// **Key order is load-bearing, not cosmetic.** In TOML every key after a table
/// header belongs to that table, so the top-level scalars — `approval_policy`,
/// `sandbox_mode`, `model_reasoning_effort`, `developer_instructions` — must
/// all be emitted BEFORE the first `[…]` line, or they silently become keys of
/// whichever table happens to precede them. That would not fail loudly: at best
/// codex's strict-config check reports an unknown key, and at worst the pane
/// boots with none of its posture and no obvious cause.
/// `a_codex_profiles_top_level_keys_all_precede_the_first_table_header` pins it.
///
/// Every key, and why:
///
/// - `approval_policy` — `never` for an unattended pane (`AskForApproval::Never`,
///   "Never ask the user to approve commands"), `on-request` otherwise. Unlike
///   pi, whose posture toggle is a measured no-op, this one is real on codex,
///   and the attention scan catches the overlay an `on-request` pane raises.
/// - `sandbox_mode = "workspace-write"` — the only rung a working agent can
///   use. `read-only` blocks running commands and the network as well as edits
///   (see `CliCaps`' codex row), and the bypass rung is never emitted by loomux
///   at all.
/// - `[sandbox_workspace_write] network_access = true` — off by default under
///   `workspace-write`, and a worker that cannot reach GitHub is not a worker.
/// - `default_permissions` + `[permissions.<name>]` INSTEAD of `sandbox_mode`
///   and `[sandbox_workspace_write]` — only when `git` grants something, which
///   is a GROUP pane in a linked worktree (#3456): `:workspace` plus the git
///   metadata a commit writes (which lives outside the worktree) as `write`,
///   and the gitdir files that redirect git as `read`. The caller derives both
///   lists with [`codex_worktree_git_access`], where the argument for the exact
///   sets lives; this function only spells what it is handed.
/// - `[projects."<cwd>"] trust_level = "trusted"` — the single most important
///   line here. `should_show_trust_screen(config)` is
///   `config.active_project.trust_level.is_none()`, rendering "Do you trust the
///   contents of this directory?", and a fresh worktree is always a new project
///   root (its `.git` is a FILE, so `find_project_root` stops there rather than
///   walking on to the main clone). Without this line every pane would boot into
///   that dialog and eat its kickoff. The profile layer reaches the decision
///   because the loader folds EVERY layer into `merged_so_far` before computing
///   the project trust context.
/// - `[mcp_servers.<server>]` — loomux's own server over streamable HTTP,
///   carrying the same agent-token header every other CLI's config does. One
///   server contract, six spellings.
/// - `developer_instructions` — the block's role contract, and a durable
///   carrier rather than a first-turn paste. codex reads it from CONFIG into
///   each `TurnContext` and re-inserts it through
///   `build_initial_context_with_world_state` on the COMPACTION path
///   (`start_new_context_window` → `replace_compacted_history`), so a compacted
///   codex pane gets its contract back from this file without loomux doing
///   anything. That is what makes `ContractCarrier::SystemLayerFull` honest
///   here even though the key is documented as "inserted as a `developer` role
///   message" rather than as a system prompt.
///
/// **Residual, stated because a reader will look for it:** the key is the
/// pane's `cwd`, and codex looks the trust up under the PROJECT ROOT
/// (`find_project_root` walks ancestors for a marker). Every pane loomux
/// launches is AT a root — a worker's worktree, or the group's repo — so the
/// two coincide today. A pane launched in a subdirectory of a repo would find
/// no entry and see the dialog. Writing the ancestor chain instead would mean
/// re-implementing the vendor's root discovery here; writing the repo root
/// would trust more of the human's tree than this pane asked for.
///
/// **Not written, deliberately:** `model` (the human's own `config.toml` key is
/// the inherit row — see `default_model`), `[windows] sandbox` (elevated setup
/// is the human's, and getting it wrong costs them a private desktop their
/// credentials are not on), and `tui.alternate_screen` (a real key, settable
/// here — a profile-v2 file is strict-validated as a whole `ConfigToml` — and a
/// live judgement call about scrollback that #2515 §7.6 leaves to the human).
#[doc(hidden)] // pub for integration tests
pub fn codex_profile_toml(
    port: u16,
    auth: CodexMcpAuth<'_>,
    cwd: &Path,
    unattended: bool,
    effort: &str,
    developer_instructions: Option<&str>,
    git: &CodexGitAccess,
) -> String {
    // A worktree pane's sandbox is a NAMED permissions profile rather than the
    // legacy `sandbox_mode` block, because it needs a read-only file INSIDE a
    // writable directory (the gitdir's `commondir`), which `[sandbox_workspace_write]`
    // cannot say. Every other pane keeps the legacy block, byte for byte.
    let profiles = !git.write.is_empty();
    let mut s = String::new();
    s.push_str(&format!(
        "# Generated by {} — do not edit; this file is rewritten on every spawn and\n\
         # removed with the agent that owns it. See docs/design/codex.md.\n\n",
        brand::NAME
    ));

    // ── top-level scalars, all of them, before any table header ──
    s.push_str(&format!(
        "approval_policy = \"{}\"\n",
        if unattended { "never" } else { "on-request" }
    ));
    if profiles {
        // Not beside `sandbox_mode`: codex picks the permission syntax per
        // layer and a layer naming both is read as profiles anyway
        // (`resolve_permission_config_syntax`, core/src/config/mod.rs at
        // rust-v0.156.1). Naming only the one that applies leaves no ambiguity.
        s.push_str(&format!("default_permissions = \"{CODEX_WORKTREE_PERMISSIONS}\"\n"));
    } else {
        s.push_str("sandbox_mode = \"workspace-write\"\n");
    }
    // Omitted entirely when empty, for the reason the model flag is omitted:
    // a blank value is an argument, not a silence — and `ReasoningEffort`
    // refuses the empty string outright ("reasoning_effort must not be empty"),
    // so an empty key would fail the whole config rather than be ignored.
    if !effort.is_empty() {
        s.push_str(&format!("model_reasoning_effort = \"{effort}\"\n"));
    }
    if let Some(contract) = developer_instructions {
        // A multi-line basic string. TOML trims the newline immediately after
        // the opening delimiter, so the body starts exactly at `contract`'s
        // first character and nothing is added to what the agent reads.
        s.push_str(&format!(
            "developer_instructions = \"\"\"\n{}\"\"\"\n",
            toml_basic_escape(contract, TomlNewlines::Keep)
        ));
    }
    s.push('\n');

    // ── tables ──
    if profiles {
        // `:workspace` is codex's own `workspace-write` as a profile parent: `:root`
        // readable, the project roots (the pane's cwd) and temp writable, and the
        // project root's `.git` read-only (`extensible_builtin_parent_profile`,
        // core/src/config/permissions.rs). The child adds exact paths; codex
        // resolves each path to its DEEPEST matching entry
        // (`FileSystemSandboxPolicy::resolve_access`), so a `read` file inside a
        // `write` directory stays read-only. On Windows it becomes a deny-write
        // ACE, and codex grants `DELETE` per descendant rather than
        // `FILE_DELETE_CHILD` on the parent precisely so that such a deny holds
        // (`WRITE_ALLOW_MASK`, windows-sandbox-rs/src/acl.rs).
        let p = CODEX_WORKTREE_PERMISSIONS;
        s.push_str(&format!("[permissions.{p}]\nextends = \":workspace\"\n\n"));
        s.push_str(&format!("[permissions.{p}.filesystem]\n"));
        // Each path is a quoted key, every backslash escaped: a raw Windows
        // backslash in a basic string is a parse error that loses the WHOLE
        // profile, not just this entry.
        for (paths, access) in [(&git.write, "write"), (&git.read_only, "read")] {
            for path in paths {
                s.push_str(&format!(
                    "\"{}\" = \"{access}\"\n",
                    toml_basic_escape(&path.display().to_string(), TomlNewlines::Escape)
                ));
            }
        }
        // The legacy block's `network_access = true`, in the profile's words.
        s.push_str(&format!("\n[permissions.{p}.network]\nenabled = true\n\n"));
    } else {
        s.push_str("[sandbox_workspace_write]\nnetwork_access = true\n\n");
    }
    s.push_str(&format!(
        "[projects.\"{}\"]\ntrust_level = \"trusted\"\n\n",
        toml_basic_escape(&cwd.display().to_string(), TomlNewlines::Escape)
    ));
    s.push_str(&format!("[mcp_servers.{MCP_SERVER}]\n"));
    s.push_str(&format!("url = \"http://127.0.0.1:{port}/mcp\"\n"));
    // No `startup_timeout_sec`, no `tool_timeout_sec` — see the block where
    // those constants used to be. codex's own defaults (30s and 300s) are more
    // generous than anything loomux would set, and the first version of this
    // function set both LOWER while claiming to raise them.
    // `approve` — NOT codex's default `auto`, which is what this line wrote
    // until #3405 on the false premise that `auto` means "never ask". It does
    // not: `requires_mcp_tool_approval_for_mode` (core/src/mcp_tool_call.rs,
    // identical at rust-v0.153.4 and rust-v0.156.1) maps `Auto` to "ask unless
    // the tool's annotations say read-only, or non-destructive AND
    // closed-world", and an un-annotated tool counts as destructive — every
    // one of loomux's tools. Under `approval_policy = "never"` that ask is a
    // refusal ("MCP tool call requires approval, but approval policy is
    // never"), so an unattended pane could not `report` at all. `Approve` is
    // the one value `mcp_permission_prompt_is_auto_approved`
    // (codex-mcp/src/mcp/mod.rs) short-circuits on BEFORE it reads the policy,
    // so it holds for both postures — an attended pane's `report` is no more
    // the human's to click through than an unattended one's. It is scoped to
    // this one server, and the profile layer wins over the user layer, so a
    // human's own setting can neither re-impose a prompt here nor be widened
    // by this line anywhere else.
    s.push_str("default_tools_approval_mode = \"approve\"\n");
    let header = brand::AGENT_TOKEN_HEADER;
    match auth {
        CodexMcpAuth::EnvVar(var) => {
            s.push_str(&format!("env_http_headers = {{ \"{header}\" = \"{var}\" }}\n"));
        }
        CodexMcpAuth::Literal(token) => s.push_str(&format!(
            "http_headers = {{ \"{header}\" = \"{}\" }}\n",
            toml_basic_escape(token, TomlNewlines::Escape)
        )),
    }
    s
}

/// What the human's OWN codex config declares that loomux's profile layer does
/// NOT displace — measured, reported once per spawn, and never refused.
///
/// **Why this exists.** codex has no exclusive-MCP switch the TUI can reach:
/// `--ignore-user-config` is a `codex exec` flag, and `--profile` names a LAYER
/// over `~/.codex/config.toml` rather than a replacement for it. Layering
/// merges maps, so every `[mcp_servers.*]` the human declared is part of this
/// pane's tool surface — user-authored input, in a threat model where the agent
/// is autonomous. pi's residual, one vendor over, measured the same way
/// (`pi_repo_mcp_exposure`).
///
/// **Measure and warn, never refuse** (constraint 8): a human declaring their
/// own MCP servers is legitimate and common, and loomux is not in a position to
/// adjudicate it. What loomux CAN do is put what it saw in the audit trail, so
/// a human debugging a pane whose `report` went somewhere strange has a row
/// rather than a mystery.
///
/// **What it can and cannot see, stated because the gap matters.** A NAME
/// collision is the sharp case and is fully visible — but its DIRECTION is the
/// opposite of pi's, which is why it gets a field of its own: loomux's profile
/// is the LATER layer, so a user entry sharing loomux's server name is
/// displaced rather than displacing. The dangerous case here is the quiet one
/// (the human loses their server on this pane), not loomux losing its own.
/// What is invisible is everything about the servers themselves: this is a line
/// scan for `[mcp_servers.<name>]` headers, not a TOML parse, so it cannot see
/// an inline `mcp_servers = { … }` table, a server's tool list, or whether an
/// entry is `enabled = false`. An empty result is therefore "nothing statically
/// visible in the shape loomux looks for", never "nothing there" — the same
/// absence-is-not-proof line `pi_repo_mcp_exposure` is written on, and it is
/// pinned by a test rather than only disclosed here.
///
/// `None` when the file is absent or declares nothing — the common case, and
/// the one that must cost no audit row at all.
#[doc(hidden)] // pub for integration tests
pub fn codex_user_mcp_exposure(codex_home: &Path, our_server: &str) -> Option<Value> {
    let path = codex_home.join("config.toml");
    let body = fs::read_to_string(&path).ok()?;
    let mut names: Vec<String> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        // `[mcp_servers.<name>]` only. A dotted name (`[mcp_servers.a.b]`) is
        // taken whole: this reports what it SAW, and narrowing to the first
        // segment would report a server that is not what is written.
        let Some(rest) = line.strip_prefix("[mcp_servers.") else { continue };
        let Some(name) = rest.strip_suffix(']') else { continue };
        let name = name.trim().trim_matches('"');
        if !name.is_empty() {
            names.push(name.to_string());
        }
    }
    if names.is_empty() {
        return None;
    }
    names.sort();
    names.dedup();
    let displaced = names.iter().any(|n| n == our_server);
    Some(json!({
        "file": path.to_string_lossy(),
        "servers": names,
        // Its own field because the direction is the surprising half: loomux's
        // layer is LATER, so a same-named user entry is the one that loses.
        "this_pane_displaces_a_user_server_of_the_same_name": displaced,
        "why": "codex's --profile file is a LAYER over ~/.codex/config.toml, not a replacement \
                for it, and the TUI has no exclusive-config switch — so these user-declared \
                servers are part of this pane's tool surface. Reported, not refused. This is a \
                line scan for [mcp_servers.<name>] headers, so an inline mcp_servers table is \
                invisible to it and an empty result is not proof of absence.",
    }))
}
