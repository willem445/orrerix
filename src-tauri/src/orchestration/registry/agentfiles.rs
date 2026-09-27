//! The per-CLI agent files on disk: the CLI home and agent-directory
//! overrides, the Codex profile and the Copilot folder trust, the generated
//! custom-agent files and their orphan sweep and per-group reclaim, and the
//! persona a block resolves to and the launch flags it compiles into
//! (`resolve_persona`, `persona_inject`), as an `impl OrchRegistry` block
//! (#3498). The designs are `docs/design/orchestration.md` and
//! `docs/design/harness-adapters.md`; personas are `docs/design/workflows.md`.

use super::*;

impl OrchRegistry {
    /// Point the usage reader at a specific Claude transcript root, instead of
    /// `~/.claude/projects`. Test-only seam (see `claude_projects_dir`).
    #[doc(hidden)]
    pub fn set_claude_projects_dir(&self, dir: PathBuf) {
        *self.claude_projects_dir.lock_safe() = Some(dir);
    }

    /// Point the generated Claude custom-agent file at a specific directory,
    /// instead of `~/.claude/agents`. Test-only seam (see
    /// `claude_agents_dir_override`).
    #[doc(hidden)]
    pub fn set_claude_agents_dir_override(&self, dir: PathBuf) {
        *self.claude_agents_dir_override.lock_safe() = Some(dir);
    }

    /// Point the generated Copilot custom-agent file at a specific directory,
    /// instead of `~/.copilot/agents`. Test-only seam (see
    /// `copilot_agents_dir_override`).
    #[doc(hidden)]
    pub fn set_copilot_agents_dir_override(&self, dir: PathBuf) {
        *self.copilot_agents_dir_override.lock_safe() = Some(dir);
    }

    /// Point codex's home at a specific directory, instead of `$CODEX_HOME` /
    /// `~/.codex`. Test-only seam (see `codex_home_override`).
    #[doc(hidden)]
    pub fn set_codex_home_override(&self, dir: PathBuf) {
        *self.codex_home_override.lock_safe() = Some(dir);
    }

    /// Take (and clear) the notices a spawn produced for `agent_id` (#802).
    ///
    /// Empty for the overwhelming majority of spawns. Read once, by the
    /// `spawn_agent` reply — see [`Self::spawn_notices`].
    #[doc(hidden)] // pub for mcp.rs and the integration tests
    pub fn take_spawn_notices(&self, agent_id: &str) -> Vec<String> {
        self.spawn_notices.lock_safe().remove(agent_id).unwrap_or_default()
    }

    /// Point the #417 compact-hook script directory at a specific directory,
    /// instead of the real per-machine one `compact_hook_dir` otherwise
    /// derives. Test-only seam (see `compact_hook_dir_override`'s doc for the
    /// cross-test race this exists to avoid).
    #[doc(hidden)]
    pub fn set_compact_hook_dir_override(&self, dir: PathBuf) {
        *self.compact_hook_dir_override.lock_safe() = Some(dir);
    }

    /// Point Copilot's user-level hooks directory at a specific directory,
    /// instead of `~/.copilot/hooks`. Test-only seam (see
    /// `copilot_hooks_dir_override`'s doc).
    #[doc(hidden)]
    pub fn set_copilot_hooks_dir_override(&self, dir: PathBuf) {
        *self.copilot_hooks_dir_override.lock_safe() = Some(dir);
    }

    /// Claude's own custom-agent directory (`~/.claude/agents`, per its own
    /// CLI reference — user-level scope, priority 4 in ITS `--agent`
    /// resolution order, below `.claude/agents/` project-level and the now-
    /// unused `--agents` CLI flag), or the test override. Never the repo's
    /// `.claude/agents/`, for the SAME reason `copilot_agents_dir` never
    /// writes into `.github/agents/`: a generated file there would dirty the
    /// user's git tree with something they didn't author. `None` only if the
    /// home directory can't be resolved at all.
    fn claude_agents_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = self.claude_agents_dir_override.lock_safe().clone() {
            return Some(dir);
        }
        self.user_cli_dir(".claude", "agents")
    }

    /// #502, the RE-MINT root cause: a directory inside the user's home is
    /// resolved ONLY for the registry that owns the user's live
    /// orchestration state — the one rooted at [`default_root`]
    /// (Self::default_root), which `lib.rs` constructs exactly once. Any
    /// registry rooted elsewhere gets a directory inside its OWN root
    /// instead, and can never write into the user's real CLI configuration.
    ///
    /// This was observed live, not theorized. After the 1,210 orphans of
    /// #502 were deleted by hand, files for long-dead groups reappeared
    /// within minutes — and the names said where from: group ids are
    /// `<repo-dir-name>-<hash>` (`group_id_for_repo`), and the reappearing
    /// ones were `loomux-tmp<random>-<hash>-*` and `loomux-repo-<hash>-*`,
    /// i.e. `tempfile::tempdir()` directory names and the fake `C:/tmp/repo`
    /// path — loomux's OWN test suite. It constructs throwaway registries
    /// 50+ times and only its shared `test_registry` helper set the agents-
    /// dir override, so every OTHER test that spawned an agent minted a real
    /// multi-KB file in the developer's `~/.claude/agents` that nothing ever
    /// deleted. That is where 67MB came from; the group-end reclaim and the
    /// startup sweep are the belt, this is the cause.
    ///
    /// Fixed HERE rather than by adding the override to each of those call
    /// sites: "every test remembers to opt out of writing to your home
    /// directory" is not a property anyone can maintain, and the next test
    /// added would silently reintroduce it. Deliberately not `cfg!(test)`-
    /// gated either — integration tests link the lib as a normal dependent,
    /// so `cfg(test)` is FALSE in them (which is exactly why the leak
    /// survived this long), and the rule is sound on its own terms for any
    /// caller: a registry that is not managing the user's live state has no
    /// business writing the user's live CLI config.
    ///
    /// The failure direction is safe: if the app's root ever stops being
    /// `default_root()`, generated agent files land in that root instead of
    /// the user's home, and `persona_inject`'s existing write-failure
    /// fallbacks (`--append-system-prompt-file` on Claude, kickoff text on
    /// Copilot) still deliver the contract. Nothing silently writes where it
    /// shouldn't.
    pub(in crate::orchestration) fn user_cli_dir(&self, cli_dir: &str, sub: &str) -> Option<PathBuf> {
        if self.is_live_registry() {
            return dirs::home_dir().map(|h| h.join(cli_dir).join(sub));
        }
        Some(self.root.join(format!("{}-{sub}", cli_dir.trim_start_matches('.'))))
    }

    /// #502 (rev-38 review, B1): the containment PREDICATE, in one place —
    /// is this the registry that owns the user's live orchestration state?
    ///
    /// Every path that would otherwise reach into the user's home asks this
    /// one question: [`user_cli_dir`](Self::user_cli_dir) for the generated
    /// agent dirs, and [`copilot_home_dir`](Self::copilot_home_dir) for
    /// Copilot's hook config and its `trustedFolders` config. Factored out
    /// because rev-38 found a third home-writing path that the first two had
    /// each open-coded the rule for and this one had simply never been given
    /// it — a rule copied per site is a rule the next site forgets.
    fn is_live_registry(&self) -> bool {
        self.root == Self::default_root()
    }

    /// Copilot's user-level home (`$COPILOT_HOME`, else `~/.copilot`) — or,
    /// for a registry that is not the user's live one, a contained stand-in
    /// inside its own root.
    ///
    /// #502 (rev-38 review, B1): `COPILOT_HOME` is checked only for the live
    /// registry, deliberately. That variable names the user's REAL Copilot
    /// home, which is precisely what a throwaway registry must not touch —
    /// honoring it first would reopen the leak for anyone who has it set.
    pub(in crate::orchestration) fn copilot_home_dir(&self) -> Option<PathBuf> {
        if !self.is_live_registry() {
            return Some(self.root.join("copilot-home"));
        }
        if let Ok(home) = std::env::var("COPILOT_HOME") {
            if !home.trim().is_empty() {
                return Some(PathBuf::from(home));
            }
        }
        dirs::home_dir().map(|h| h.join(".copilot"))
    }

    /// codex's user-level home (`$CODEX_HOME`, else `~/.codex`) — or, for a
    /// registry that is not the user's live one, a contained stand-in inside
    /// its own root (#2515 C1).
    ///
    /// Same three rules as [`Self::copilot_home_dir`], and here for the same
    /// #502 reason: this is the third directory in the user's home loomux
    /// WRITES into, and a throwaway registry that reached the real one would
    /// leave profile files in a human's `~/.codex` that nothing ever sweeps.
    /// The test override is checked first so a test never depends on the
    /// developer's own environment; `CODEX_HOME` is checked only for the live
    /// registry, because that variable names the user's REAL codex home, which
    /// is precisely what a throwaway registry must not touch.
    ///
    /// **`is_empty()`, not `trim()`, and that is a deliberate disagreement with
    /// the neighbour above.** codex's own `find_codex_home` filters on
    /// `!val.is_empty()`, so a `CODEX_HOME` of one space is a real (and
    /// hopeless) path to codex itself. Reading it as unset here would put the
    /// profile in `~/.codex` while the pane's codex looked in `" "` — the two
    /// would disagree about which store this pane is configured from, which is
    /// worse than agreeing on a path that does not exist. `sessions.rs`'s
    /// `codex_sessions_root_from` mirrors the vendor for the same reason.
    fn codex_home_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = self.codex_home_override.lock_safe().clone() {
            return Some(dir);
        }
        if !self.is_live_registry() {
            return Some(self.root.join("codex-home"));
        }
        if let Ok(home) = std::env::var("CODEX_HOME") {
            if !home.is_empty() {
                return Some(PathBuf::from(home));
            }
        }
        dirs::home_dir().map(|h| h.join(".codex"))
    }

    /// Write one codex pane's profile file and return its path plus the pane
    /// environment that pane needs — `write_mcp_config`'s codex branch, split
    /// out because it is the only branch whose file lands OUTSIDE loomux's own
    /// state root.
    ///
    /// That is the one genuinely new cross-layer fact in #2515 and it is why
    /// three things travel with it: the name is loomux-branded so a sweep can
    /// recognise it, the file is removed with the agent that owns it
    /// ([`Self::remove_codex_profile`]), and anything the removal misses is
    /// reclaimed at startup ([`Self::sweep_orphaned_agent_files`], which keys on
    /// the LIVE AGENT MAP — see the comment in its codex arm for why the durable
    /// roster is the wrong oracle and made that claim false once already). A file
    /// in a vendor's user directory that nothing cleans up is #502 by another
    /// route.
    ///
    /// `Err` rather than best-effort, and for the same reason
    /// `write_gemini_policy` is: this file is not a convenience the pane can do
    /// without. Without it the pane has no MCP server (no report channel), no
    /// recorded trust (it boots into the trust dialog and eats its kickoff),
    /// and no contract. A codex pane that cannot have its profile is not a
    /// degraded agent, it is a pane sitting on a dialog.
    pub(in crate::orchestration) fn write_codex_profile(
        &self,
        group: &GroupId,
        agent: &PathSegment,
        auth: CodexMcpAuth<'_>,
        workdir: &Path,
        unattended: bool,
        effort: &str,
        developer_instructions: Option<&str>,
        // #3456: `codex_worktree_git_access`'s answer for a group pane, empty
        // for a solo one — see the solo caller for why.
        git: &CodexGitAccess,
    ) -> Result<(PathBuf, String), String> {
        let home = self
            .codex_home_dir()
            .ok_or_else(|| "cannot resolve codex's home directory (no CODEX_HOME and no user home)".to_string())?;
        let file = codex_profile_file_name(agent)?;
        // codex itself requires `CODEX_HOME` to EXIST — it errors out with
        // "CODEX_HOME points to {val:?}, but that path is not a directory" —
        // so creating it is not loomux overstepping: a pane whose home is
        // missing does not boot at all.
        fs::create_dir_all(&home).map_err(|e| format!("{}: {e}", home.display()))?;
        let path = home.join(&file);
        let port = self.port();
        let body = codex_profile_toml(
            port,
            auth,
            workdir,
            unattended,
            effort,
            developer_instructions,
            git,
        );
        fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))?;
        // Measure-and-warn, once per spawn, and NEVER a refusal — see
        // `codex_user_mcp_exposure`. `None` for the human who declares no MCP
        // servers of their own, which is the ordinary case and writes no row.
        if let Some(exposure) = codex_user_mcp_exposure(&home, MCP_SERVER) {
            let mut row = exposure;
            row["agent"] = json!(agent.as_str());
            self.audit(group, brand::AUDIT_ACTOR, "codex-user-mcp-merged", row);
        }
        Ok((path, codex_profile_name(agent)?))
    }

    /// `GH_TOKEN` for a codex GROUP pane, read from the human's own `gh`
    /// (`gh auth token`) — or nothing, with an audit row saying why (#3405).
    ///
    /// **Why codex needs this and claude/copilot/pi do not.** Those CLIs run
    /// `gh` as the human, so `gh` finds its token wherever `gh auth login` put
    /// it. codex runs shell commands inside its own sandbox, and on Windows the
    /// `elevated` sandbox runs them as a DIFFERENT local account
    /// (`CodexSandboxOnline`, logged on with `LogonUserW` —
    /// windows-sandbox-rs/src/identity.rs at rust-v0.156.1). `gh`'s default
    /// token store is the per-account system keyring (Windows Credential
    /// Manager), which that account cannot see; `hosts.yml` still names the
    /// login, so `gh` sends its request with no token and GitHub answers
    /// `HTTP 401: Requires authentication`. What DOES cross the sandbox is the
    /// environment: codex's `shell_environment_policy` defaults to
    /// `inherit = all` with `ignore_default_excludes = true`
    /// (config/src/shell_environment_policy.rs at the same tag), and `gh`
    /// prefers `GH_TOKEN` over any stored credential.
    ///
    /// **The pane env, never the profile**, per the design note's rule for a
    /// group pane's secrets: the profile lives in the human's `CODEX_HOME` and
    /// outlives a crash until the sweep; the env dies with the pane. And codex
    /// GROUP panes only: a solo pane has no environment loomux sets
    /// (`solo_prepare` only appends flags to the human's own line), and a
    /// non-codex pane already reaches the keyring as the human.
    ///
    /// **Accepted residual: the credential now crosses codex's sandbox
    /// boundary.** Under `sandbox = "elevated"` a codex pane could NOT run
    /// `gh auth token` — that is the bug — so this is a real widening for that
    /// pane, not a no-op: the token is handed across the account boundary codex
    /// drew, and every process the pane runs (a dependency's install script, a
    /// test binary) can read it from its environment. Accepted because it is
    /// the credential a claude/copilot/pi peer in the same group already holds
    /// as the human, and it is the remedy #3405 asked for. It is also read ONCE,
    /// at spawn: a later `gh auth refresh`, logout or revocation is not seen by
    /// a pane already running — it goes back to `401` with no audit row (the
    /// read itself succeeded), and only a respawn picks up the new token.
    ///
    /// **Degrades, never refuses.** A pane without `gh` access is still a pane
    /// that can `report` — failing its spawn would turn one broken tool into
    /// zero working ones. So a failed or empty read writes a
    /// `codex-gh-token-unavailable` audit row (the reason, never a token) and
    /// the pane starts without the variable. Runs through [`Self::gh_capture`],
    /// the one bounded place the backend spawns `gh`; on the orchestrator and
    /// lead paths that is inside the `creation` mutex, which fires once per
    /// group launch (`performance.md` X6), for one local keyring read.
    pub(in crate::orchestration) fn codex_gh_token_env(
        &self,
        group: &GroupId,
        agent_id: &str,
        workdir: &Path,
    ) -> Option<(String, String)> {
        // #502's containment rule, asked through its one predicate: a registry
        // that is not the user's live one never reads the human's credential.
        // Without this every codex spawn in the test suite would run the REAL
        // `gh auth token` on the developer's machine and carry their token in
        // a test `SpawnRequest`. A test that means to exercise this path says
        // so explicitly by installing the `gh_exec_override` fake.
        if !self.is_live_registry() && self.gh_exec_override.lock_safe().is_none() {
            return None;
        }
        let why = match self.gh_capture(&workdir.display().to_string(), &["auth", "token"]) {
            // One token, one line. Anything with interior whitespace is not a
            // token `gh` printed, and exporting it would present garbage as a
            // credential rather than presenting none.
            Ok(out) => {
                let tok = out.trim();
                if !tok.is_empty() && !tok.contains(char::is_whitespace) {
                    return Some(("GH_TOKEN".to_string(), tok.to_string()));
                }
                "gh auth token printed no token".to_string()
            }
            Err(e) => e,
        };
        self.audit(
            group,
            brand::AUDIT_ACTOR,
            "codex-gh-token-unavailable",
            json!({
                "agent": agent_id,
                "why": notify::sanitize_gh_text(&why, notify::NOTICE_FIELD_CAP),
                "effect": "gh inside this codex pane's sandbox will be unauthenticated",
            }),
        );
        None
    }

    /// Remove one agent's codex profile file. Best-effort and idempotent: the
    /// agent may have been on another CLI, the file may already be gone, or
    /// `CODEX_HOME` may have moved since it was written — none of which is
    /// worth failing a kill or a group teardown over, and all of which the
    /// startup sweep catches later.
    #[doc(hidden)] // pub for integration tests
    pub fn remove_codex_profile(&self, agent_id: &str) {
        let Ok(seg) = PathSegment::parse(agent_id) else { return };
        let Ok(file) = codex_profile_file_name(&seg) else { return };
        let Some(home) = self.codex_home_dir() else { return };
        let path = home.join(file);
        if fs::remove_file(&path).is_ok() {
            crate::obs::breadcrumb(
                "codex-profile",
                &format!("removed {} with its agent", path.display()),
            );
        }
    }

    /// Pre-trust an agent's workspace in copilot's config so its pane doesn't
    /// boot into a folder-trust dialog — which eats the kickoff paste and gets
    /// blind-answered by the submit retries. Best-effort: on any failure the
    /// dialog simply appears as before.
    ///
    /// Pre-trust an agent's workspace in copilot's config so its pane doesn't
    /// boot into a folder-trust dialog — which eats the kickoff paste and gets
    /// blind-answered by the submit retries. Best-effort: on any failure the
    /// dialog simply appears as before.
    ///
    /// **The only entry point** (rev-38 round 3). It briefly had a `pub` free
    /// sibling that did its own home resolution, which left containment
    /// depending on every caller picking this one — the same
    /// convention-instead-of-mechanism shape that let the leak below happen
    /// in the first place. There is now nothing else to call.
    ///
    /// #502 (rev-38 review, B1): the free function was the third uncontained
    /// write into the user's home, and the worst of them on two counts the
    /// other two don't share — `trustedFolders` is a SECURITY setting (it
    /// suppresses Copilot's folder-trust prompt), and its entries are
    /// per-workspace PATHS, so a suite spawning into fresh temp dirs appends
    /// a new one every run and grows without bound. The agent-file
    /// incident's exact shape, one directory over.
    ///
    /// #475's thread-local seam still wins when a test sets it: fixturing it
    /// is an explicit statement about where this write should go, and that
    /// beats a structural default. Everything else — including the 50+ tests
    /// that build a registry without fixturing anything — lands inside the
    /// registry's own root via `copilot_home_dir`.
    #[doc(hidden)] // pub for integration tests (#475's seam drives this path)
    /// `location` is the repo whose git root scopes the grant; `folder` is the
    /// directory this agent actually works in (its worktree, or the repo
    /// itself). They differ for a worker in a dedicated worktree — see
    /// [`copilot_permissions_grant`] for why the key is the repo and the
    /// worktree rides `allowed_directories`.
    pub fn pre_trust_copilot_folder(&self, location: &str, folder: &str) {
        if let Some(home) = COPILOT_TRUST_HOME_OVERRIDE.with(|c| c.borrow().clone()) {
            pre_trust_copilot_folder_in(&home, location, folder);
            return;
        }
        let Some(home) = self.copilot_home_dir() else { return };
        pre_trust_copilot_folder_in(&home, location, folder);
    }

    /// Copilot's own custom-agent directory (`~/.copilot/agents`, per its
    /// docs — highest-precedence in ITS OWN `--agent` resolution order), or
    /// the test override. Never the repo's `.github/agents/`: writing a
    /// generated file there would dirty the user's git tree with a file they
    /// didn't author (`profiles::is_copilot_native`'s reasoning). `None` only
    /// if the home directory can't be resolved at all.
    fn copilot_agents_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = self.copilot_agents_dir_override.lock_safe().clone() {
            return Some(dir);
        }
        // #502: same containment rule as the Claude side — see `user_cli_dir`.
        self.user_cli_dir(".copilot", "agents")
    }

    /// Reclaim generated custom-agent files (`loomux-<group>-<block>.md`,
    /// see `write_claude_agent_file`/`write_copilot_agent_file`) left behind
    /// under `~/.claude/agents`/`~/.copilot/agents` by a group this registry
    /// no longer has any record of.
    ///
    /// `end_group` already reclaims a group's own files the moment it ends
    /// (#416/round-6, widened round #417 correction 6) — this exists for the
    /// groups that never reach `end_group` at all: a crash, a `kill -9`, or
    /// (the #464 case) an orchestration-suite test registry that spawns
    /// agents through the exact same write path and then simply exits.
    /// 1,111 and 161 such stray files were found under a real dev machine's
    /// `~/.claude/agents` and `~/.copilot/agents` respectively while fixing
    /// that issue — accumulated one test run at a time, nothing to do with
    /// this registry's OWN in-memory state, which is why a from-disk sweep,
    /// not an in-memory teardown hook, is what closes the gap. Called once
    /// at launch (`lib.rs`'s `setup`), matching `start_disk_monitor` et al.
    ///
    /// Conservative by construction, not merely by intent:
    /// - Only entries literally named `loomux-<something>.md` (which also
    ///   covers Copilot's `loomux-<something>.agent.md`) are ever
    ///   considered — loomux's own naming convention (see `write_claude_
    ///   agent_file`'s and `write_copilot_agent_file`'s `handle` formats,
    ///   the only two shapes either ever writes), nothing else in either
    ///   directory is touched (a hand-authored `loomux-notes.txt` is
    ///   invisible to this sweep, not merely spared by the group check
    ///   below), and this is never gated on any repo-, toolchain-, or
    ///   machine-specific path.
    /// - A candidate is reclaimed ONLY when NO directory under this
    ///   registry's own `state_root()` matches it as a `loomux-<group>[-…]`
    ///   prefix — i.e. `<group>` resolves to nothing this registry has any
    ///   record of at all. A still-live, merely-paused, or simply
    ///   not-yet-ended group's files are left alone even if that group has
    ///   not spawned anything recently. Matched by prefix rather than by
    ///   splitting the filename on `-`, because both a group id
    ///   (`tmpocm2wt-ddf1fc50`) and a block id (`rev-security`) can contain
    ///   a `-` themselves, so there is no unambiguous single split point.
    /// - Every reclaim is breadcrumbed by path, so a sweep is diagnosable
    ///   after the fact, never silent — and a removal failure (permissions,
    ///   a concurrent process) is collected, not swallowed.
    /// - If the `state_root()` enumeration itself fails (a transient AV/EDR
    ///   lock, a permissions error, or — note this issue's own trigger —
    ///   disk exhaustion, a condition under which `new()`'s own best-effort
    ///   `create_dir_all` can fail too), this sweep does NOT fall through to
    ///   "no known groups" and treat every generated file as an orphan
    ///   (#464 review B1): that reads an "I don't know what's live" moment
    ///   as "nothing is", which is exactly backwards and could delete a
    ///   LIVE group's file — breaking that group's persona injection on its
    ///   next spawn/restore, precisely during the kind of post-crash
    ///   relaunch this sweep runs at. If unsure, delete nothing: an
    ///   enumeration failure skips the sweep entirely and reports the error.
    pub fn sweep_orphaned_agent_files(&self) -> Value {
        let known_groups: Vec<String> = match fs::read_dir(self.state_root()) {
            Ok(rd) => rd
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect(),
            Err(e) => {
                let msg = format!(
                    "sweep skipped: could not enumerate groups under {} ({e}) — \
                     refusing to guess which generated agent files are orphaned",
                    self.state_root().display()
                );
                crate::obs::breadcrumb("fixture-sweep", &msg);
                return json!({
                    "reclaimed": [],
                    "errors": [json!({ "path": self.state_root().to_string_lossy(), "error": e.to_string() })],
                });
            }
        };
        let mut reclaimed = Vec::new();
        let mut errors = Vec::new();
        for dir in [self.claude_agents_dir(), self.copilot_agents_dir()] {
            let Some(dir) = dir else { continue };
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
                // #464 round-2 review N2: `write_claude_agent_file`/`write_
                // copilot_agent_file` only ever write `<handle>.md` or
                // `<handle>.agent.md` — never anything else. Requiring that
                // suffix (both end in `.md`) keeps a hand-authored
                // `loomux-notes.txt`/similar out of consideration entirely,
                // on top of (not instead of) the `loomux-` prefix check
                // below.
                if !name.ends_with(".md") {
                    continue;
                }
                let Some(rest) = name.strip_prefix("loomux-") else { continue };
                let known = known_groups.iter().any(|g| {
                    rest == g.as_str()
                        || rest.starts_with(&format!("{g}-"))
                        || rest.starts_with(&format!("{g}."))
                });
                if known {
                    continue;
                }
                match fs::remove_file(&path) {
                    Ok(()) => {
                        crate::obs::breadcrumb(
                            "fixture-sweep",
                            &format!("reclaimed orphaned generated-agent file {}", path.display()),
                        );
                        reclaimed.push(path.to_string_lossy().into_owned());
                    }
                    Err(e) => {
                        errors.push(json!({ "path": path.to_string_lossy(), "error": e.to_string() }));
                    }
                }
            }
        }
        // #2515 C1: codex's profile files, which are keyed on the AGENT rather
        // than on the group and live in a directory the two loops above never
        // look at. Same #464 B1 rule, applied to a different roster: if the
        // set of live agents cannot be established, delete NOTHING — reading
        // "I could not find out" as "nothing is live" is exactly backwards and
        // would strip a running pane of its trust, its MCP server and its
        // contract on the next respawn.
        // **The oracle is the LIVE AGENT MAP, not the durable roster**
        // (review round 3, B1). The first version unioned every
        // `AgentRecord::id` in every group's `agents.json`, which made this
        // arm dead code: `persist_agent_record` is an upsert and never
        // removes a row, `end_group` does not delete the group directory, and
        // an agent id is never re-minted (#524) — so that union is "every
        // agent id ever spawned in this root", and the skip below matched
        // every profile orrerix had ever written. The backstop three surfaces
        // promised did not exist.
        //
        // `self.agents` is the only thing that knows which panes are actually
        // running, and at this function's one production call site
        // (`lib.rs`, the setup block, before the reapers start and before any
        // spawn) it is EMPTY — which is exactly right: a profile is read by
        // codex at launch and is useless afterwards, so every one present at
        // startup is stale by construction. Keying on the live map also keeps
        // this correct if the sweep is ever called later, where the roster
        // could not have been.
        //
        // The claude/copilot loops above key on GROUPS because their file
        // names carry a group and "no such group in this root" is a real
        // orphan signal. A codex profile carries an AGENT, and the roster it
        // would have to be checked against is never pruned — which is why the
        // same shape does not transfer.
        // Named for what it IS: every agent this PROCESS holds, live or
        // recently dead (`mark_dead` sets the status and leaves the entry in the
        // map). That is deliberately conservative within a running process — a
        // dead agent's profile is already removed by `mark_dead` itself — and at
        // the startup call site the map is empty, which is the case that matters.
        let agents_this_process_holds: HashSet<String> =
            self.agents.lock_safe().values().map(|a| a.id.clone()).collect();
        {
            if let Some(home) = self.codex_home_dir() {
                // The one refusal that survives, and it is the half that was
                // right: if this directory cannot be listed, delete nothing.
                // "I could not look" is not "there was nothing there".
                if let Ok(entries) = fs::read_dir(&home) {
                    let prefix = format!("{}-", brand::NAME);
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if !path.is_file() {
                            continue;
                        }
                        let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
                        // Both halves are required, and the SUFFIX half is what
                        // keeps a human's own `orrerix-notes.md` out of
                        // consideration: `write_codex_profile` only ever writes
                        // `<brand>-<agent>.config.toml`, so anything else in
                        // this directory is not loomux's to delete.
                        let Some(stem) = name.strip_suffix(CODEX_PROFILE_FILE_EXT) else { continue };
                        let Some(agent) = stem.strip_prefix(&prefix) else { continue };
                        if agents_this_process_holds.contains(agent) {
                            continue;
                        }
                        match fs::remove_file(&path) {
                            Ok(()) => {
                                crate::obs::breadcrumb(
                                    "fixture-sweep",
                                    &format!("reclaimed orphaned codex profile {}", path.display()),
                                );
                                reclaimed.push(path.to_string_lossy().into_owned());
                            }
                            Err(e) => errors.push(
                                json!({ "path": path.to_string_lossy(), "error": e.to_string() }),
                            ),
                        }
                    }
                }
            }
        }
        json!({ "reclaimed": reclaimed, "errors": errors })
    }

    /// #502: reclaim the generated custom-agent files belonging to ONE
    /// group, from the CLIs' own user-level agent directories — the delete
    /// half of [`end_group`](Self::end_group).
    ///
    /// Scoped deliberately to a group, because the ORPHAN half already
    /// shipped separately: [`sweep_orphaned_agent_files`]
    /// (Self::sweep_orphaned_agent_files) (#464) reclaims files no live
    /// group claims, at startup. An earlier revision of this change carried
    /// its own second sweep; two implementations of one mechanism is a
    /// defect in waiting, so this keeps #464's and adds only what it does
    /// not do — the per-group teardown path.
    ///
    /// This replaces a per-MEMBER name list, which structurally could not
    /// see a file whose block a roster change retired, or whose member entry
    /// was pruned while the file stayed on disk. Ownership is a property of
    /// the FILE, not of who is still in the roster, so the directory is
    /// scanned and each candidate asked who owns it.
    ///
    /// A file is eligible only when ALL of these hold:
    ///
    /// 1. it sits at the TOP LEVEL of the CLI's agent dir (`read_dir`, not
    ///    recursive — Claude Code's own doc notes it scans `~/.claude/
    ///    agents/` recursively, so a user may well have subfolders there;
    ///    loomux writes flat and so reads flat);
    /// 2. its name is `loomux-<group>-<block><ext>` — the exact shape
    ///    [`generated_agent_handle`] writes, per CLI, with a non-empty block
    ///    part;
    /// 3. [`owning_group`] does not hand it to a MORE specific live group
    ///    (see that function: group ids can prefix one another, and
    ///    first-match logic would let `end_group("X")` delete live `X-extra`
    ///    files).
    ///
    /// Every failure direction keeps the file: an unreadable agent dir, an
    /// unreadable candidate, and an unreadable orchestration root — the last
    /// aborting the whole pass, because the group list is what ownership is
    /// decided from and an empty one would say "nothing claims anything".
    /// That is the same rule #464's sweep already applies to itself, and it
    /// is deliberately the same rule here rather than a second opinion.
    ///
    /// Returns the filenames actually removed. Best-effort throughout: a
    /// reclaim failure must never fail a group teardown.
    #[doc(hidden)] // pub for integration tests (the enumeration-failure rule is pinned on this)
    pub fn reclaim_group_agent_files(&self, group: &GroupId) -> Vec<String> {
        let Some(groups) = self.existing_group_ids() else { return Vec::new() };
        let mut removed = Vec::new();
        for (dir, ext) in [
            (self.claude_agents_dir(), CLAUDE_AGENT_FILE_EXT),
            (self.copilot_agents_dir(), COPILOT_AGENT_FILE_EXT),
        ] {
            let Some(dir) = dir else { continue };
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
                let Some(stem) = name.strip_suffix(ext) else { continue };
                let is_ours = stem
                    .strip_prefix(&format!("{GENERATED_AGENT_PREFIX}{group}-"))
                    .is_some_and(|block| !block.is_empty());
                // `owning_group` can only answer with a group at least as
                // long as this one (the prefix test above already proves
                // this group matches), so a different answer means a MORE
                // specific live group claims the file — leave it alone.
                if !is_ours || owning_group(stem, &groups).is_some_and(|o| o != group) {
                    continue;
                }
                if fs::remove_file(&path).is_ok() {
                    removed.push(name.to_string());
                }
            }
        }
        removed
    }

    /// Resolve a block's persona from its `prompt:` (inline) or `profile:` (a
    /// repo file) — `Ok(None)` when the block declares neither, which is every
    /// block of the default roster.
    ///
    /// A broken `profile:` is an **error the caller audits and swallows**, not a
    /// failed spawn: a repo file must never be able to stop an agent from
    /// starting. Re-read on every spawn, so editing a persona applies to the
    /// next agent without restarting the group.
    #[doc(hidden)] // pub for integration tests
    pub fn resolve_persona(
        &self,
        group: &GroupInfo,
        block: &workflow::Block,
    ) -> Result<Option<ResolvedPersona>, String> {
        // The orchestrator block is loomux-owned: a repo may pin its cli/model,
        // never author its persona or pre-approve its tools. `parse_workflow`
        // rejects that outright and says why — but a hand-edited `group.json`
        // never meets the parser, so the persona is dropped here too, and audited.
        //
        // Neutralizing it *here* (rather than only in `persona_inject`) is what
        // makes it total: this is the single point both the CLI flags and the
        // block's instruction file are resolved through, so a `mode: replace`
        // orchestrator persona cannot rewrite `orchestrator.md` either. See
        // `parse_workflow` for why the trust root is not a customization surface.
        //
        // #1161: `persona_allowed` now answers `false` for a MANAGER block too
        // (decision D1), and it drops through this same door. The audit ACTION
        // keeps its historic name — it is a durable record key other tooling
        // and tests read, and renaming it would rewrite the meaning of every
        // record already on disk — while the `why` field names the block's own
        // class, so a record says which rule fired.
        if !workflow::persona_allowed(block) && (block.has_persona() || !block.allow.is_empty()) {
            self.audit(&group.id, brand::AUDIT_ACTOR, "workflow-orchestrator-persona-denied", json!({
                "block": block.id,
                "kind": block.kind,
                "prompt": block.prompt.is_some(),
                "profile": block.profile,
                "allow": block.allow,
                "why": if block.kind == Role::Manager {
                    "a manager speaks to the human and relays their direction into the trust root \
                     — a repo file may not author its prompt or pre-approve its tools"
                } else {
                    "the orchestrator is loomux's trust root — a repo file may not author its \
                     prompt or pre-approve its tools"
                },
            }));
            return Ok(None);
        }
        if let Some(rel) = block.profile.as_deref() {
            let p = profiles::load_block_profile(&group.repo, rel, block.kind)?;
            let handle = p.copilot_agent.clone().unwrap_or_else(|| p.name.clone());
            // Copilot's `--agent` resolves names against `.github/agents/`, so
            // only a persona that actually lives there can use the native flag —
            // AND the name must resolve back to the file loomux actually read.
            //
            // The handle comes from the file's frontmatter `name:`, not from its
            // path. So `.github/agents/security-review.md` whose frontmatter says
            // `name: worker` would make loomux emit `--agent worker`, and Copilot
            // would go and load whichever file declares `name: worker` — the
            // *worker* persona. loomux would have kind-checked one file and
            // launched another, with the audit line insisting all was well.
            //
            // So: only take the native path when the handle unambiguously names
            // this file. Otherwise fall back to kickoff injection, which delivers
            // the persona loomux actually read.
            let native = profiles::is_copilot_native(rel)
                && profiles::handle_resolves_to(&group.repo, &handle, rel);
            if profiles::is_copilot_native(rel) && !native {
                self.audit(&group.id, brand::AUDIT_ACTOR, "copilot-agent-handle-ambiguous", json!({
                    "block": block.id, "profile": rel, "handle": handle,
                    "action": "using kickoff injection — `--agent` would resolve to a different file",
                }));
            }
            return Ok(Some(ResolvedPersona {
                text: workflow::sanitize_persona(&p.instructions),
                name: handle,
                description: workflow::sanitize_persona(&p.description),
                mode: p.mode,
                allow: p.allow.iter().chain(block.allow.iter()).cloned().collect(),
                copilot_native: native,
                // #802: carried, not applied here — `persona_inject` is where
                // the CLI is known, and this only means anything on copilot.
                copilot_tools: p.tools.clone(),
                copilot_has_mcp_servers: p.has_mcp_servers,
                copilot_frontmatter: p.frontmatter.clone(),
            }));
        }
        if let Some(prompt) = block.prompt.as_deref() {
            return Ok(Some(ResolvedPersona {
                text: workflow::sanitize_persona(prompt),
                name: block.id.clone(),
                // `sanitize_display` keeps a name readable — it strips control
                // characters and braces, and nothing else — so it can still
                // contain an apostrophe, and the description rides into the
                // single-quoted `--agents` token. Persona-sanitize it too, or a
                // block named `Bob's review` would close that quote.
                description: workflow::sanitize_persona(&block.name),
                // An inline `prompt:` is an addendum to the built-in role
                // contract. Only a persona FILE can declare `mode: replace` —
                // replacing loomux's role body is a deliberate, reviewable act,
                // not something a one-liner in a workflow file falls into.
                mode: profiles::ProfileMode::Append,
                allow: block.allow.clone(),
                copilot_native: false,
                // An inline `prompt:` is not a copilot agent file and has no
                // frontmatter of its own to declare either of these.
                copilot_tools: None,
                copilot_has_mcp_servers: false,
                copilot_frontmatter: String::new(),
            }));
        }
        // No persona, but a block may still carry `allow:` patterns.
        if !block.allow.is_empty() {
            return Ok(Some(ResolvedPersona {
                text: String::new(),
                name: block.id.clone(),
                description: workflow::sanitize_persona(&block.name),
                mode: profiles::ProfileMode::Append,
                allow: block.allow.clone(),
                copilot_native: false,
                copilot_tools: None,
                copilot_has_mcp_servers: false,
                copilot_frontmatter: String::new(),
            }));
        }
        Ok(None)
    }

    /// Compile a resolved persona into the launch flags of `cli` — the table in
    /// [`PersonaInject`]. `None` in, `PersonaInject::default()` out: no persona,
    /// no flags, pre-#222 command line.
    /// Audit a repo-authored `allow:` that was refused because the block's class
    /// is read-only. Silently dropping it would leave an author wondering why
    /// their pattern does nothing; honoring it would break capability closure.
    fn audit_allow_denied(&self, group: &GroupId, block: &workflow::Block, allow: &[String]) {
        self.audit(group, brand::AUDIT_ACTOR, "workflow-allow-denied", json!({
            "block": block.id,
            "kind": block.kind.as_str(),
            "allow": allow,
            "why": "a read-only capability class may not pre-approve tool patterns — \
                    an allow pattern could hand it a shell that writes files",
        }));
    }

    /// Generate (or refresh) a loomux-owned Claude custom-agent file carrying
    /// `contract` (round #417 correction 6, replacing the pre-round-6
    /// inline `--agents '<json>'` payload — see `PersonaInject::claude_
    /// agent`'s doc for why: Windows `CreateProcessW` has a hard
    /// 32,767-character command-line limit, the full role contract is many
    /// KB, and it can no longer ride argv on EITHER spawn path). Written
    /// into Claude's OWN user-level agent directory (`~/.claude/agents`,
    /// see [`claude_agents_dir`](Self::claude_agents_dir)), mirroring
    /// `write_copilot_agent_file` exactly — never the repo's
    /// `.claude/agents/`, same "don't dirty the user's git tree" reasoning.
    /// Its handle is unique per group+block so concurrent groups never
    /// collide, and it is rewritten on every spawn (like `write_mcp_
    /// config`) so an edited template/persona applies to the next agent
    /// without restarting the group. Returns the `--agent` handle, or
    /// `None` when the directory can't be created/written (fail-open — the
    /// caller falls back to `--append-system-prompt-file` rather than
    /// failing the spawn).
    ///
    /// Round 8 audit (rev-10 lineage), against Claude Code's own sub-agents
    /// doc (code.claude.com/docs/en/sub-agents, "Supported frontmatter
    /// fields"): "Only `name` and `description` are required" — both
    /// written below, so this was already schema-compliant, not merely
    /// lenient (unlike Copilot's generated file before this same round —
    /// see `write_copilot_agent_file`'s doc for that fix). One documented
    /// difference from Copilot worth naming: the doc states outright "The
    /// filename doesn't have to match" `name` for Claude — `--agent <name>`
    /// resolves against the frontmatter field, not the filename stem — the
    /// opposite of Copilot's filename-keyed resolution. `handle` here is
    /// both, so the distinction is inert for loomux either way, but it is
    /// why the Claude and Copilot functions must never be assumed to share
    /// a resolution rule just because they share a `handle` shape.
    ///
    /// The SAME doc's "Supported frontmatter fields" table and its
    /// surrounding prose name no maximum length for a subagent's markdown
    /// body anywhere — unlike Copilot's own custom-agents-configuration
    /// reference, which caps the equivalent body at 30,000 characters (see
    /// `copilot_agent_body`'s doc). That asymmetry is why `contract` is
    /// written here VERBATIM, full size, while Copilot's generated file
    /// gets a composed, deliberately-slimmer text instead of this same
    /// `contract` value — a per-CLI decision the docs justify, not an
    /// oversight to reconcile.
    fn write_claude_agent_file(&self, group: &GroupId, block: &workflow::Block, contract: &str, description: &str) -> Option<String> {
        if !self.group_state_exists(group) {
            return None;
        }
        let dir = self.claude_agents_dir()?;
        fs::create_dir_all(&dir).ok()?;
        let handle = generated_agent_handle(group, &block.id)?;
        // `contract` is already control-character-clean (any persona text
        // folded into it was already sanitized at resolution time — see
        // `resolve_persona`'s `sanitize_persona` call) — and unlike the
        // pre-round-6 `--agents` JSON payload, this is a FILE, not a shell
        // token, so no apostrophe-mangling/ASCII-escaping pass is needed
        // here either: loomux's own template prose keeps its real
        // apostrophes. `description` IS quoted (`yaml_double_quoted`),
        // since — unlike `contract`, loomux's own trusted text — it can
        // carry a repo-authored persona's free-text description, which is
        // not guaranteed YAML-safe (a bare colon would break the mapping).
        //
        // Round 8 review (N3a): `compaction_self_check_clause` mirrors the
        // same delivery-independent self-check Copilot's generated file
        // carries — see that function's doc. Claude's own docs confirm
        // compaction never touches the system prompt, so this is belt-
        // and-braces on top of an already-reliable channel, not a
        // response to a known gap on Claude's side specifically.
        //
        // #502: `description` is length-clamped — see
        // `GENERATED_AGENT_DESCRIPTION_MAX_CHARS` for the
        // aggregate-description budget this protects.
        let instructions_path = self.group_dir(group).join(block.instructions_file());
        let body = format!(
            "---\nname: {handle}\ndescription: {}\n---\n{contract}{}\n",
            yaml_double_quoted(&clamp_agent_description(description)),
            compaction_self_check_clause(&instructions_path),
        );
        let path = dir.join(format!("{handle}{CLAUDE_AGENT_FILE_EXT}"));
        fs::write(&path, body).ok()?;
        Some(handle)
    }

    /// Generate (or refresh) the file a block's durable CONTRACT is written
    /// to under the group's own state dir, returning `(handle, path)` or `None`
    /// if it can't be written.
    ///
    /// **Two CLIs, one file, two ways of naming it** (#722, generalised for
    /// #2126). opencode's generated agent entry points its `prompt` at this
    /// file through a `{file:...}` reference in the config document; pi points
    /// `--append-system-prompt` straight at it on argv. `ext` is the whole
    /// difference, and it exists so the two never collide in the one
    /// `configs/` directory they share — `generated_agent_handle` is
    /// `loomux-<group>-<block>`, which is the SAME handle for a block whose
    /// `cli:` changed between two launches of one group.
    ///
    /// **Written under the GROUP's own state dir**, unlike its claude and
    /// copilot siblings, and for two reasons rather than one. opencode has no
    /// user-level agents directory loomux could write a *definition* into
    /// without also owning its lifecycle — the definition lives in the config
    /// document instead — so all that needs a home here is the contract text;
    /// and a file under the group dir is reclaimed when the group is, so this
    /// needs no orphan sweep of the kind `~/.claude/agents` and
    /// `~/.copilot/agents` do (#464/#502). The repo's own `.opencode/agents/`
    /// is never written for the reason copilot's `.github/agents/` isn't:
    /// loomux does not dirty a user's git tree with files they did not write.
    ///
    /// Beside the config document in `configs/`, deliberately: the two are one
    /// artifact split across a file and an env var, and the config's `{file:}`
    /// reference is what joins them.
    ///
    /// Written on every spawn, like `write_mcp_config`, so an edited
    /// template/persona applies to the next agent without restarting the
    /// group. Named for the handle so the reference and its target can't drift.
    ///
    /// The whole contract goes in VERBATIM — no frontmatter, no wrapper: this
    /// file is not an agent definition (the config document is), it is the
    /// value of that definition's `prompt`. opencode inserts it JSON-escaped,
    /// so quotes, newlines and backslashes in a persona cannot break the
    /// document around it, and it `.trim()`s the content, so nothing may
    /// depend on the trailing newline.
    fn write_contract_file(
        &self,
        group: &GroupId,
        block: &workflow::Block,
        contract: &str,
        ext: &str,
    ) -> Option<(String, PathBuf)> {
        if !self.group_state_exists(group) {
            return None;
        }
        let dir = self.group_dir(group).join("configs");
        fs::create_dir_all(&dir).ok()?;
        let handle = generated_agent_handle(group, &block.id)?;
        let path = dir.join(format!("{handle}{ext}"));
        let instructions_path = self.group_dir(group).join(block.instructions_file());
        let body =
            format!("{contract}{}\n", compaction_self_check_clause(&instructions_path));
        fs::write(&path, body).ok()?;
        Some((handle, path))
    }

    /// Generate (or refresh) a loomux-owned Copilot custom-agent file carrying
    /// `contract` (#416) — written into Copilot's OWN user-level agent
    /// directory (`~/.copilot/agents`, see [`copilot_agents_dir`]
    /// (Self::copilot_agents_dir)), never the repo's `.github/agents/`. Its
    /// handle is unique per group+block so concurrent groups never collide,
    /// and it is rewritten on every spawn (like `write_mcp_config`) so an
    /// edited template/persona applies to the next agent without restarting
    /// the group. Returns the `--agent` handle, or `None` when the directory
    /// can't be created/written (fail-open — the caller falls back to the
    /// pre-#416 kickoff-text path rather than failing the spawn).
    ///
    /// Round 8 (live-demo blocker, rev-10 lineage): a Copilot launch failed
    /// with `CustomAgentLoadFailedError: ... description: Required`. GitHub's
    /// own custom-agents-configuration reference (docs.github.com/en/copilot/
    /// reference/custom-agents-configuration) lists exactly two frontmatter
    /// fields with any required/optional distinction that matters here —
    /// `description` (**required**) and `name` (optional, defaults to the
    /// filename if omitted) — everything else (`tools`, `model`,
    /// `disable-model-invocation`, `user-invocable`, `mcp-servers`,
    /// `metadata`) is optional and left unset on the ordinary path, so nothing
    /// silently narrows what the agent can do. **The one exception is the #802
    /// repair path** (`repair`), where this file stands in for a user's own
    /// agent file: there it reproduces that file's keys verbatim and extends
    /// only `tools:`. See `repair`'s doc.
    ///
    /// That this directory is a real, documented `--agent` source is not
    /// inferred from loomux's own precedent: the CLI how-to
    /// (docs.github.com/en/copilot/how-tos/use-copilot-agents/use-copilot-cli)
    /// lists `~/.copilot/agents` as the **user-level** agent location, "All
    /// projects", and shows the flag taking a NAME
    /// (`copilot --agent=refactor-agent --prompt "…"`) — which is exactly what
    /// `handle` supplies as both the filename stem and the frontmatter `name`.
    /// Its conflict rule ("a system-level agent overrides a repository-level
    /// agent") cannot bite either way, because `generated_agent_handle` is
    /// `loomux-<group>-<block>` and nothing else in the world claims it. `description` never needs to
    /// be distinctive prose — it is loomux's own bookkeeping label, not
    /// something a human browses — so it is built deterministically from
    /// `group`/`block.id` alone: same inputs, byte-identical description,
    /// every render, forever (no timestamp, no persona text, nothing that
    /// would make two renders of the same block disagree).
    ///
    /// Filename convention, confirmed against the same reference and the
    /// CLI how-to (docs.github.com/en/copilot/how-tos/copilot-cli/customize-
    /// copilot/create-custom-agents-for-cli): `--agent <value>` resolves
    /// against the FILENAME stem (minus `.agent.md`), never the frontmatter
    /// `name` field — the opposite of Claude's `--agent`, which resolves
    /// against frontmatter `name` and documents "the filename doesn't have
    /// to match" (see `write_claude_agent_file`'s doc). `handle` here is
    /// simultaneously the `--agent` value, the filename stem, and the
    /// frontmatter `name` — correct for Copilot's filename-keyed resolution,
    /// and harmless extra information for a field the docs say is optional.
    ///
    /// Round 8 review (B1, blocking): the SAME custom-agents-configuration
    /// reference also caps the agent BODY (the markdown after the
    /// frontmatter, i.e. the prompt) at 30,000 characters — "The prompt can
    /// be a maximum of 30,000 characters" — while Claude's own sub-agents
    /// doc states no such limit (see `write_claude_agent_file`'s doc). The
    /// FULL block contract (`block_contract_text`: mechanics core + the
    /// complete built-in role template) routinely exceeds that for the
    /// default roster's own orchestrator — measured 58,633 chars, ~1.95x
    /// over — before this fix; an over-cap write is either silently
    /// truncated or rejected by Copilot, either of which is role
    /// degradation presenting as a successful launch. So the body written
    /// here is `copilot_agent_body`'s SLIM composition, never the raw
    /// `contract` (see that function's doc for what it keeps vs. defers to
    /// the instructions file) — a per-CLI decision; Claude's rendition is
    /// unchanged. The caller sets `ContractCarrier::SystemLayerCore` on
    /// success (rev-16 review, round 8 delta): a real durable state, NOT
    /// "nothing durable" — a real compaction routes to `ReinjectShape::
    /// Pointer` (re-read the file, no re-embed), never the full verbose
    /// embed a `KickoffOnly` agent gets. See `ContractCarrier`'s doc.
    ///
    /// Belt-and-braces, mirroring `command_line_length_guard`'s shape: a
    /// composed body over `COPILOT_AGENT_BODY_SAFE_CHARS` fails LOUDLY here
    /// (audited, naming the block and the measured size) rather than ever
    /// writing a file Copilot might truncate or refuse — `None` routes the
    /// caller to the existing write-failure fallback (kickoff delivery,
    /// `ContractCarrier::KickoffOnly`, verbose re-grounding), exactly like
    /// an unwritable directory already does.
    fn write_copilot_agent_file(
        &self,
        group: &GroupId,
        block: &workflow::Block,
        persona: Option<&ResolvedPersona>,
        // #802: `true` only on the tools-gap repair path, where this file is a
        // STAND-IN for a user's `.github/agents/*.md` that Copilot would
        // otherwise have loaded whole. Then, and only then, it reproduces that
        // file's frontmatter verbatim (minus the three keys loomux re-authors)
        // and extends `tools:` with the loomux grant.
        //
        // `false` is every other generated file — the default roster, an inline
        // `prompt:`, an ambiguous handle — and those are byte-identical to
        // before this change (rev-lead N1). A non-native persona's `tools:` was
        // never in force, because Copilot never loaded its file; emitting it
        // here would narrow such a block for the first time, which is a
        // capability change #802 never asked for.
        repair: bool,
    ) -> Option<String> {
        if !self.group_state_exists(group) {
            return None;
        }
        let dir = self.copilot_agents_dir()?;
        fs::create_dir_all(&dir).ok()?;
        let handle = generated_agent_handle(group, &block.id)?;
        let instructions_path = self.group_dir(group).join(block.instructions_file());
        let body_text = copilot_agent_body(block, persona, &instructions_path);
        // `.chars().count()`, not `.len()`: the documented cap is in
        // CHARACTERS, and loomux's own trusted prose is ASCII, but a
        // repo-authored persona folded in via `block_contract_text` is not
        // guaranteed to be — a multi-byte character must count once, not
        // once per UTF-8 byte, or this guard would trip early on non-ASCII
        // text that is nowhere near the real cap.
        let body_chars = body_text.chars().count();
        if body_chars > COPILOT_AGENT_BODY_SAFE_CHARS {
            self.audit(group, brand::AUDIT_ACTOR, "copilot-agent-body-oversized", json!({
                "block": block.id,
                "chars": body_chars,
                "safe_limit": COPILOT_AGENT_BODY_SAFE_CHARS,
                "documented_cap": COPILOT_AGENT_BODY_DOCUMENTED_CAP_CHARS,
            }));
            return None;
        }
        // `group`/`block.id` are both loomux-internal identifiers
        // (sanitized elsewhere), never repo-authored free text, but
        // `description` is still quoted the same way the Claude path
        // quotes ITS (persona-sourced) description — cheap, uniform
        // defense against a stray YAML-significant character either way.
        // The frontmatter split in `profiles::parse_profile` only ever
        // looks at the FIRST `\n---` after the opening one, so a `---`
        // line anywhere inside `body_text` itself is inert.
        let description = format!("orrerix {} agent for group {group}", block.id);
        // #802, and ONLY on the repair path (see `repair`'s doc). Two things
        // happen here, and they are one idea: this file is standing in for a
        // file the user wrote, so it must not quietly become a different
        // persona.
        //
        // 1. `carried` reproduces every frontmatter key except the three loomux
        //    re-authors (`LOOMUX_OWNED_FRONTMATTER_KEYS`). `model:` is the one
        //    with real teeth — the reference documents it as *"Model to use when
        //    this custom agent executes"*, and dropping it on the way through
        //    would change which model the persona runs on, silently, as a side
        //    effect of a permissions fix. Everything else (`infer`, `target`,
        //    `disable-model-invocation`, and whatever Copilot documents next)
        //    rides along for the same reason: loomux has no opinion about those
        //    keys, and "no opinion" must mean "carried", not "deleted".
        // 2. `tools:` is the user's list verbatim plus
        //    `COPILOT_MCP_TOOL_GRANTS` — the only edit loomux makes, and the
        //    reason the file exists.
        //
        // Omitting `tools:` entirely (the `false` path) is Copilot's documented
        // "all available tools", which is what every generated file has always
        // relied on and what makes the built-in roster work.
        let (carried, tools_line) = match persona.filter(|_| repair) {
            Some(p) => (
                profiles::carry_frontmatter(&p.copilot_frontmatter),
                match p.copilot_tools.as_deref() {
                    Some(tools) => format!(
                        "tools: [{}]\n",
                        copilot_tools_with_loomux(tools)
                            .iter()
                            .map(|t| yaml_double_quoted(t))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    None => String::new(),
                },
            ),
            None => (String::new(), String::new()),
        };
        let body = format!(
            "---\nname: {handle}\ndescription: {}\n{tools_line}{carried}---\n{body_text}\n",
            yaml_double_quoted(&clamp_agent_description(&description))
        );
        let path = dir.join(format!("{handle}{COPILOT_AGENT_FILE_EXT}"));
        fs::write(&path, body).ok()?;
        Some(handle)
    }

    /// Compile a resolved persona into the launch flags of `cli` — the table in
    /// [`PersonaInject`]. `contract` is the block's full durable role CONTRACT
    /// (see [`block_contract_text`]) — computed once at the spawn site from
    /// the SAME [`render_block_instructions`](Self::render_block_instructions)
    /// call that wrote the instructions file, so the system-prompt payload and
    /// the file can never disagree.
    ///
    /// #416: unlike the pre-#416 behavior (a repo persona compiled onto
    /// `--agents`/a native Copilot file, nothing at all otherwise), `contract`
    /// reaches the CLI's native custom-agent mechanism for EVERY block, workflow-
    /// customized or not — the durable-role-contract gap this closes was never
    /// about *whether* a persona could ride the system-prompt layer, it was
    /// that loomux's OWN mechanics/template body never did, on any block.
    #[doc(hidden)] // pub for integration tests
    pub fn persona_inject(
        &self,
        group: &GroupId,
        block: &workflow::Block,
        cli: &str,
        persona: Option<&ResolvedPersona>,
        contract: &str,
    ) -> PersonaInject {
        // CAPABILITY CLOSURE, enforced at the last possible moment (#222).
        //
        // `workflow::parse_workflow` already refuses `allow:` on a read-only
        // block, but that is not the only way a pattern gets here: a
        // `.github/agents/*.md` persona can carry its own `allow:` frontmatter,
        // and a hand-edited group.json never sees the parser at all. A read-only
        // class is read-only by denying a *fixed list* of tools — so an allow
        // pattern that names something not on that list (`Bash(python *)`,
        // `Bash(tee *)`, …) would hand a planner a pre-approved shell that writes
        // files, with no human in its pane to say no.
        //
        // Nobody can enumerate every write-capable program. So a read-only block
        // simply gets no allow patterns, from any source, ever.
        let allow: Vec<String> = match persona {
            Some(p) if block.kind.is_read_only() => {
                if !p.allow.is_empty() {
                    self.audit_allow_denied(group, block, &p.allow);
                }
                Vec::new()
            }
            Some(p) => p.allow.clone(),
            None => Vec::new(),
        };
        let mut out = PersonaInject { extra_allow: allow, ..Default::default() };

        if cli == "copilot" {
            // #802: a comma SEPARATES patterns inside a `--allow-tool` value on
            // copilot ("a quoted, comma-separated list"), so a pattern that
            // contains one is unrepresentable on this CLI — copilot would read
            // `shell(git log --format=a,b)` as the two fragments
            // `shell(git log --format=a` and `b)`, neither of which is the
            // grant that was written, and one of which is a prefix pattern
            // nobody authored. There is no documented escape, so loomux refuses
            // the pattern rather than inventing one or shipping the fragments.
            // Audited, never silent — the block still launches, one grant
            // narrower, and the record says which and why. Claude's
            // `--allowedTools` is space-separated and unaffected, which is why
            // this lives in the copilot branch rather than in the shared
            // capability-closure filter above.
            let (keep, refused): (Vec<String>, Vec<String>) =
                out.extra_allow.drain(..).partition(|p| !p.contains(','));
            if !refused.is_empty() {
                self.audit(group, brand::AUDIT_ACTOR, "copilot-allow-pattern-refused", json!({
                    "block": block.id,
                    "allow": refused,
                    "why": "a comma separates patterns inside copilot's --allow-tool value, \
                            so a pattern containing one cannot be expressed on this CLI",
                }));
            }
            out.extra_allow = keep;
            // #802, THE ROOT CAUSE. Copilot's custom-agent `tools:` frontmatter
            // is a FILTER, not an addition — the custom-agents configuration
            // reference's *Tools processing* section: "The `tools` list filters
            // the set of tools that are made available to the agent - whether
            // built-in or sourced from MCP servers", with "If no tools are
            // specified, all available tools are enabled".
            //
            // So a `.github/agents/*.md` persona carrying `tools: [read, edit]`
            // strips EVERY loomux MCP tool from the delegate `--agent` launches:
            // the server still loads (it rides `--additional-mcp-config`, so the
            // CLI lists it as available) and none of its tools survive the
            // filter. That is #802's report word for word — "MCP visible but not
            // usable" — and it is why only workflow blocks with a `profile:`
            // were affected while the built-in roster was fine: loomux's OWN
            // generated agent file has never had a `tools:` key, so it inherits
            // the documented all-tools default.
            //
            // Neither `--allow-tool <server>` nor the `permissions-config.json`
            // approval (#803) can undo it: those grant PERMISSION over what is
            // available, and this decides what is available at all. The only
            // documented repair is an agent file whose own list carries the
            // grant — and loomux may never write into the user's
            // `.github/agents/` (#222). So it re-points `--agent` at a
            // loomux-owned copy in `~/.copilot/agents/` carrying the user's list
            // PLUS `COPILOT_MCP_TOOL_GRANTS`, which is the generated-file
            // path that already exists a few lines below.
            // **Only a NATIVE persona can have this bug at all** (rev-lead N1).
            // The filter bites when Copilot loads the USER's file, which happens
            // only on the native path. A non-native persona — a `profile:`
            // outside `.github/agents/`, or one whose handle does not resolve
            // back to its own file — is delivered by a loomux-generated copy
            // that never carried a `tools:` key and still doesn't, so its list
            // was never in force and reproducing it now would *narrow* that
            // block for the first time. Gating here keeps this change to
            // exactly the blocks #802 describes, and keeps the warning free of
            // false positives on blocks that were never broken.
            let tools_gap = persona.filter(|p| p.copilot_native && !p.grants_loomux_tools());
            if let Some(p) = tools_gap {
                let refusal = tools_gap_refusal(p);
                self.audit(group, brand::AUDIT_ACTOR, "copilot-persona-tools-gap", json!({
                    "block": block.id,
                    "persona": p.name,
                    "tools": p.copilot_tools.clone().unwrap_or_default(),
                    "missing": COPILOT_MCP_TOOL_GRANTS,
                    "per_tool_scope": p.scopes_mcp_server_per_tool(),
                    "names_server_as": p.mcp_server_named_in_tools(),
                    "action": match refusal {
                        None => "re-pointed --agent at a loomux-generated stand-in carrying every \
                                 frontmatter key verbatim, with the loomux grant added to tools:",
                        Some(ToolsGapAction::KeptNativeForMcpServers) =>
                            "left as written — the persona declares its own mcp-servers, which a stand-in would drop",
                        Some(ToolsGapAction::KeptNativeForExplicitEmptyList) =>
                            "left as written — `tools: []` is a deliberate no-tools decision, not an omission",
                        Some(ToolsGapAction::KeptNativeForPerToolScope) =>
                            "left as written — the list scopes the loomux server per-tool on purpose",
                        Some(other) => {
                            debug_assert!(false, "not a refusal reason: {other:?}");
                            "left as written"
                        }
                    },
                }));
                // loomux repairs an OMISSION, never a DECISION — see
                // `tools_gap_refusal`. Each refusal launches the user's file
                // exactly as they wrote it, with the warning saying which
                // decision was honored and what it costs them.
                if let Some(action) = refusal {
                    out.warnings.push(copilot_tools_gap_warning(&block.id, p, action));
                    // THE GUARD (rev-lead B1). Every site that puts a persona's
                    // OWN frontmatter name on `--agent` must first know that
                    // name resolves back to the file loomux read — that is the
                    // whole job of `handle_resolves_to`, and a refusal path is
                    // not an exemption from it. Without this, an ambiguous
                    // handle (`security-review.md` declaring `name: worker`)
                    // would make a REFUSAL emit `--agent worker` and launch a
                    // different persona than the one loomux kind-checked.
                    //
                    // `tools_gap` above already requires `copilot_native`, so
                    // this is belt-and-braces today — kept because the invariant
                    // must live at the site that depends on it, not in a filter
                    // ten lines up that a later edit could loosen without ever
                    // looking here.
                    if persona.is_some_and(|p| p.copilot_native) {
                        out.copilot_agent = persona.map(|p| p.name.clone());
                        return out;
                    }
                    // Not native: fall through to the generated copy (which is
                    // where a non-native persona was always headed), never to a
                    // `--agent` naming a file loomux did not verify.
                }
                // Repairable: fall through to the generated-file path below,
                // deliberately skipping the native early-return. The stand-in
                // carries this persona's frontmatter verbatim with only
                // `tools:` extended, so the user's scoping intent — and their
                // `model:`, and every key loomux has no opinion about — survives
                // the substitution.
            } else if persona.is_some_and(|p| p.copilot_native) {
                // Unchanged (#222): a user-authored `.github/agents/*.md`
                // persona reaches Copilot by pointing `--agent` at the exact
                // file loomux read and kind-checked. Synthesizing a wrapper
                // around it would trade that carefully-resolved trust
                // property (`profiles::handle_resolves_to`) for mechanics-
                // core coverage this one case still lacks — a residual,
                // documented gap (docs/design/orchestration.md's #416 note),
                // not something worth reaching across that boundary for.
                // `contract_carrier` stays its default (`KickoffOnly`) —
                // only the user's OWN file rides `--agent`, never anything
                // loomux authored.
                out.copilot_agent = persona.map(|p| p.name.clone());
                return out;
            }
            // #416: every OTHER copilot block — the default roster, an inline
            // `prompt:` persona, or an ambiguous native handle — now gets a
            // durable (round 8: a SLIM core, not the full contract — see
            // `copilot_agent_body`'s doc) contract via a loomux-generated
            // custom-agent file, where before it got nothing (default
            // roster) or a kickoff-prompt paste (inline `prompt:`).
            match self.write_copilot_agent_file(group, block, persona, tools_gap.is_some()) {
                Some(handle) => {
                    out.copilot_agent = Some(handle);
                    // A real durable state (rev-16 review, round 8 delta) —
                    // NOT `KickoffOnly`. See `ContractCarrier::
                    // SystemLayerCore`'s doc for what's guaranteed vs. what
                    // a real compaction still has to re-read from disk.
                    out.contract_carrier = ContractCarrier::SystemLayerCore;
                }
                None => {
                    // `~/.copilot/agents` unwritable, OR the composed body
                    // blew the round-8 size guard — fall back to the
                    // pre-#416 kickoff-text path rather than silently losing
                    // a configured persona. An allow-only/no-persona block
                    // has no text to fall back to, so this is a true no-op
                    // for it, same as before. `contract_carrier` stays its
                    // default (`KickoffOnly`) — nothing loomux authored is
                    // durable here.
                    if let Some(p) = persona {
                        if !p.text.trim().is_empty() {
                            out.kickoff = Some(p.text.clone());
                        }
                    }
                }
            }
            // #802: the gap notice is worded from what actually happened, not
            // from what was intended — the write above can still fail, and a
            // warning claiming a repair that never landed is worse than none.
            if let Some(p) = tools_gap {
                out.warnings.push(copilot_tools_gap_warning(
                    &block.id,
                    p,
                    if out.copilot_agent.is_some() {
                        ToolsGapAction::Repaired
                    } else {
                        ToolsGapAction::RepairFailed
                    },
                ));
            }
            return out;
        }

        // OpenCode (#722): the same "native custom-agent flag, file-backed"
        // shape as the two above, with the definition in the config document
        // `write_mcp_config` generates (which is why this must run BEFORE it
        // at every spawn site) and the contract itself in a file the entry's
        // `prompt` references. `contract` is used whole — the full instructions
        // body plus any persona — because opencode documents no cap on it,
        // matching claude rather than copilot (see `copilot_agent_body`).
        //
        // A repo persona resolved from a `.github/agents/*.md` file
        // (`copilot_native`) is irrelevant here: that flag records what
        // COPILOT's `--agent` can resolve, and opencode resolves neither the
        // file nor its frontmatter. Its text is already folded into `contract`
        // by `block_contract_text`, so nothing is lost by ignoring the flag.
        if cli == "opencode" {
            match self.write_contract_file(group, block, contract, OPENCODE_AGENT_FILE_EXT) {
                Some((handle, path)) => {
                    out.opencode_agent = Some(handle);
                    out.opencode_prompt_file = Some(path);
                    out.contract_carrier = ContractCarrier::SystemLayerFull;
                }
                None => {
                    // The group dir is unwritable. Emit NO `--agent` and no
                    // agent entry: a `--agent` naming an entry that isn't
                    // there does not fail, it falls back to `build` — the most
                    // permissive agent — so a broken flag would be strictly
                    // worse than no flag. Containment is unaffected either way
                    // (it rides `OPENCODE_PERMISSION`, which applies globally);
                    // what is lost is the durable contract, so fall back to the
                    // kickoff-text path exactly as copilot does, and audit it
                    // for the same reason the claude fallback is audited.
                    if persona.is_some_and(|p| !p.text.trim().is_empty()) {
                        out.kickoff = persona.map(|p| p.text.clone());
                    }
                    self.audit(group, brand::AUDIT_ACTOR, "opencode-agent-file-unwritable", json!({
                        "block": block.id,
                        "reason": "could not write the block's contract file under the group dir; \
                                   the pane launches with no --agent (an unresolvable one would \
                                   silently fall back to the default `build` agent) and the \
                                   contract reaches it through the kickoff only",
                    }));
                }
            }
            return out;
        }

        // pi (#2126): the same "contract in a file under the group dir" shape
        // as opencode's above, and SIMPLER at the flag end — pi has no
        // `--agent` equivalent, so there is no handle to emit and nothing that
        // could resolve to the wrong definition. `--append-system-prompt`
        // names the file and pi appends it to its own system prompt.
        //
        // `contract` is used whole — the full instructions body plus any
        // persona — matching claude and opencode rather than copilot, whose
        // slim composition exists solely because GitHub documents a 30,000
        // character body cap. pi documents no cap on this flag.
        //
        // A `copilot_native` persona is as irrelevant here as it is in the
        // opencode branch: that flag records what COPILOT's `--agent` can
        // resolve, and pi resolves neither the file nor its frontmatter. The
        // text is already folded into `contract` by `block_contract_text`.
        if cli == "pi" {
            match self.write_contract_file(group, block, contract, PI_CONTRACT_FILE_EXT) {
                Some((_handle, path)) => {
                    out.pi_append_system_prompt_file = Some(path);
                    out.contract_carrier = ContractCarrier::SystemLayerFull;
                }
                None => {
                    // The group dir is unwritable. Emit NO flag: pointing
                    // `--append-system-prompt` at a file that is not there
                    // would at best be silently ignored and at worst put the
                    // literal path into the system prompt as TEXT, since the
                    // flag documents "text or file contents". Fall back to the
                    // kickoff-text path exactly as copilot and opencode do,
                    // and audit it for the reason the claude fallback is
                    // audited: a dropped persona must be findable in the trail
                    // rather than inferred from its absence.
                    //
                    // Nothing about containment rests on this flag — pi's
                    // denial rides `--exclude-tools` on the same line — so the
                    // loss here is the durable contract and nothing else.
                    if persona.is_some_and(|p| !p.text.trim().is_empty()) {
                        out.kickoff = persona.map(|p| p.text.clone());
                    }
                    self.audit(group, brand::AUDIT_ACTOR, "pi-contract-file-unwritable", json!({
                        "block": block.id,
                        "reason": "could not write the block's contract file under the group dir; \
                                   the pane launches with no --append-system-prompt and the \
                                   contract reaches it through the kickoff only",
                    }));
                }
            }
            return out;
        }

        // codex (#2515 C1): the same whole-`contract` delivery as claude,
        // opencode and pi, and the only one of the five that needs no file of
        // loomux's own — the contract IS a key in the profile
        // `write_mcp_config` is about to write, so there is no second artifact
        // to create, no handle that could resolve to the wrong definition, and
        // no write that could fail independently.
        //
        // That last point is why this branch has no fallback arm while its
        // three predecessors all do. Their failure mode is "the contract file
        // could not be written, so emit no flag and fall back to the kickoff";
        // here the contract and the MCP server and the trust level are one
        // file, and a codex pane that cannot have it is not a degraded agent —
        // it is a pane sitting on the trust dialog with no report channel. So
        // the failure belongs to `write_codex_profile`, which returns `Err` and
        // fails the spawn visibly, rather than to a silent degrade here.
        //
        // A `copilot_native` persona is as irrelevant here as in the two
        // branches above: that flag records what COPILOT's `--agent` can
        // resolve, and codex resolves neither the file nor its frontmatter.
        // The text is already folded into `contract` by `block_contract_text`.
        if cli == "codex" {
            if !contract.trim().is_empty() {
                out.codex_developer_instructions = Some(contract.to_string());
                out.contract_carrier = ContractCarrier::SystemLayerFull;
            }
            return out;
        }

        // Claude (and the fallback adapter): the durable contract ALWAYS
        // rides the system-prompt layer now — every block, persona or not
        // — but as of round #417 correction 6, by FILE, never argv. The
        // pre-round-6 design put the whole contract inline in `--agents
        // '<json>'`; a live demo hit Windows CreateProcessW's hard
        // 32,767-character command-line limit, because the full role
        // contract (mechanics core + template, many KB) is categorically
        // bigger than the short repo personas `--agents` was designed to
        // carry before #416 widened its payload to loomux's own template
        // text on every block. See `PersonaInject::claude_agent`'s doc for
        // the mechanism and citation. `description` is required by the
        // file schema either way.
        let description = persona
            .map(|p| p.description.trim())
            .filter(|d| !d.is_empty())
            .unwrap_or(block.id.as_str());
        match self.write_claude_agent_file(group, block, contract, description) {
            Some(handle) => out.claude_agent = Some(handle),
            None => {
                // `~/.claude/agents` unwritable — fall back to `--append-
                // system-prompt-file` pointed at the instructions file
                // `write_instruction_files` ALREADY reliably wrote to this
                // group's own state dir (no second file to invent or
                // clean up). Still system-prompt-layer durable either way
                // — see `claude_append_system_prompt_file`'s doc — so this
                // fallback, unlike Copilot's kickoff-text one, never needs
                // `contract_carrier` to drop below `SystemLayerFull`.
                //
                // Round 8 review (N3b, widened by rev-18): that fallback
                // file is mechanics/template ONLY — a block's actual
                // persona TEXT lives in `contract` (which is what just
                // failed to write), never in the instructions file, in
                // EITHER mode — `render_block_instructions`'s append
                // branch only ever writes a short "adopt your persona"
                // pointer note (`block_note`), never the persona's own
                // words. The audit isn't scoped to `mode: replace` (no
                // design cost to covering both — same file, same gap,
                // same missing text) — audited rather than silently
                // dropped. Not fixed by appending the persona into the
                // instructions file itself — that file has other writers
                // (`write_instruction_files`) and a per-fallback mutation
                // of shared state is a bigger change than this gap
                // warrants.
                if persona.is_some_and(|p| !p.text.trim().is_empty()) {
                    self.audit(group, brand::AUDIT_ACTOR, "claude-fallback-persona-dropped", json!({
                        "block": block.id,
                        "mode": persona.map(|p| p.mode.as_str()),
                        "reason": "~/.claude/agents unwritable; the --append-system-prompt-file \
                                    fallback carries mechanics only, never a persona's own text",
                    }));
                }
                out.claude_append_system_prompt_file =
                    Some(self.group_dir(group).join(block.instructions_file()));
            }
        }
        out.contract_carrier = ContractCarrier::SystemLayerFull;
        out
    }
}
