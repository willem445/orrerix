//! Launching an agent pane: the `gh` shims and the per-pane environment
//! (#83), the MCP config, the Gemini policy and the Claude `--settings` file
//! with its compaction and statusline hooks, and the agent's command line and
//! argv (`build_agent_command`, `build_agent_argv`), as an `impl OrchRegistry`
//! block (#3498). The designs are `docs/design/orchestration.md` and
//! `docs/design/shim-path-integrity.md`.

use super::*;

impl OrchRegistry {
    // ---------- enforced merge gate (#83): gh shim + per-pane env ----------

    /// The shared directory holding the `gh` + `git` interceptor shims, prepended
    /// to every *agent* pane's PATH so a default-branch merge (`gh pr merge`) or a
    /// release/tag publish (`gh release …`, `git push` of a `v*` tag) is
    /// structurally gated (a live incident showed template guidance alone fails).
    /// One shim set for all groups — it reads the pane's group-dir variable at runtime to find
    /// the group's markers/grants — under the loomux data dir, beside the per-group
    /// orchestration state.
    fn shim_dir(&self) -> PathBuf {
        self.root.parent().unwrap_or(&self.root).join("ghshim")
    }

    /// Write one shim script (POSIX + a Windows `.cmd` delegator) for `program`,
    /// resolving the real binary and baking its absolute path in so the shim never
    /// re-resolves to itself. No-op returning `false` when the real program isn't
    /// installed (nothing to intercept). `sh` builds the POSIX body from the
    /// forward-slashed real path; `cmd` builds the `.cmd` delegator. `sh_path` is
    /// the absolute `sh.exe` path (#335, resolved once by the caller — see
    /// `ensure_shims`) baked into the `.cmd` delegator; `None` means no `sh` was
    /// found anywhere on the machine, in which case `cmd` renders a delegator that
    /// audits the degraded gate before falling through to the real binary.
    fn write_shim(
        &self,
        dir: &Path,
        program: &str,
        sh: impl Fn(&str, &ShimPaths) -> String,
        cmd: impl Fn(&str, Option<&str>) -> String,
        sh_path: Option<&str>,
        paths: &ShimPaths,
    ) -> bool {
        let Some(real) = crate::winpath::resolve_program(
            program,
            &crate::winpath::launch_path(),
            &crate::winpath::launch_pathext(),
        ) else {
            return false;
        };
        // Forward-slash so the path is safe inside the POSIX shim (Git Bash accepts
        // `C:/…`); still valid for the `.cmd` wrapper.
        let real_fwd = real.to_string_lossy().replace('\\', "/");
        let script = dir.join(program);
        let _ = fs::write(&script, sh(&real_fwd, paths).as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o755));
        }
        #[cfg(target_os = "windows")]
        {
            // cmd/pwsh panes resolve `<program>` to `<program>.cmd` (`.PS1` isn't
            // on the default PATHEXT). It delegates to the POSIX shim so the gate
            // logic lives in one place, via the absolute `sh_path` resolved at
            // shim-write time (#335) rather than the invoking shell's PATH.
            let _ = fs::write(dir.join(format!("{program}.cmd")), cmd(&real_fwd, sh_path).as_bytes());
        }
        true
    }

    /// Write a shim that refuses outright (#815) — no real binary resolved, nothing
    /// delegated to. Separate from `write_shim` because that helper is built around
    /// intercepting a program that *exists* and handing off to it: it no-ops when
    /// the real binary is absent, which is exactly backwards for a refusal. This one
    /// is unconditional, so the block holds on a machine where the launcher merely
    /// isn't installed *yet* — an agent can `npm i -g` one mid-session.
    fn write_refusal_shim(&self, dir: &Path, program: &str, sh: String, cmd: String) {
        let script = dir.join(program);
        let _ = fs::write(&script, sh.as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o755));
        }
        #[cfg(target_os = "windows")]
        {
            // cmd/pwsh panes resolve `<program>` to `<program>.cmd` (`.PS1` isn't on
            // the default PATHEXT), so the refusal needs both forms — see `write_shim`.
            let _ = fs::write(dir.join(format!("{program}.cmd")), cmd.as_bytes());
        }
        #[cfg(not(target_os = "windows"))]
        let _ = cmd;
    }

    /// Write (idempotently) the `gh` + `git` shim scripts and return the shim dir,
    /// or `None` when neither real binary is installed. Cheap (a few small file
    /// writes); called per spawn so a freshly-installed gh/git is picked up by the
    /// next pane.
    fn ensure_shims(&self) -> Option<PathBuf> {
        let dir = self.shim_dir();
        if fs::create_dir_all(&dir).is_err() {
            return None;
        }
        // #335: resolve an absolute `sh.exe` ONCE, from git's own install layout
        // (not the invoking shell's PATH — see `winpath::resolve_sh`), and bake it
        // into both `.cmd` delegators. `None` when no `sh` exists anywhere on the
        // machine; the delegator then audits the degraded gate on every fallback
        // rather than bypassing it silently.
        //
        // #509 rides on the same resolution: `sh` inherits the CALLER's PATH,
        // which off a PowerShell/cmd pane carries neither Git for Windows'
        // coreutils (so the shim's `tr` vanished and the `gh api` gate fell
        // OPEN) nor a native `git.exe` ahead of our own `git.cmd` (so gh's
        // internal git calls were mangled by cmd.exe's re-parse). Both are
        // derived here from the same install layout `sh` was found in — never
        // hardcoded (constraint 8) — and baked into the POSIX shims.
        let (sh_path, shim_paths) = resolve_shim_toolchain();

        let gh = self.write_shim(&dir, "gh", gh_shim_sh, gh_shim_cmd, sh_path.as_deref(), &shim_paths);
        let git = self.write_shim(&dir, "git", git_shim_sh, git_shim_cmd, sh_path.as_deref(), &shim_paths);
        // #815: the launcher block rides the same dir and the same per-spawn refresh,
        // but resolves nothing and gates nothing — see `loomux_shim_sh`. It does not
        // participate in the return below: whether an agent pane gets a PATH at all
        // stays a gh/git question, and a machine with neither has no shim dir on PATH
        // for this to live on anyway.
        // NOT the audit actor: this argument is the FILE NAME the refusal shim
        // is written under, so it must be the name of a launcher an agent could
        // actually type, and a shim written under any other name blocks nothing
        // while leaving the real launcher runnable — #815's guard defeated,
        // silently.
        //
        // BOTH names, because after #1153 phase 5 two of them can be on PATH:
        // `orrerix` is what the launcher package installs now, and `loomux` is
        // what a global install of the old `loomux-desktop` left behind. npm
        // does not uninstall the old package for you, and this is a refusal —
        // covering a name nobody has costs an unused file, while missing one
        // somebody does have costs the whole group. Ordered current-first so
        // `tests/rebrand.rs` reads the live spelling out of the first call.
        //
        // `tests/pathseg.rs` pins a call site verbatim as the proof that
        // `program` is a literal here; a bulk sweep took this argument once and
        // that guard is what caught it. Two literal calls, not a loop over a
        // slice, so that property stays true by inspection.
        self.write_refusal_shim(&dir, "orrerix", loomux_shim_sh(), loomux_shim_cmd());
        self.write_refusal_shim(&dir, "loomux", loomux_shim_sh(), loomux_shim_cmd());
        // #3477: anything else in this dir shadows a real program on every agent
        // pane's PATH, so drop the shims an earlier build wrote and this one no
        // longer does — marker-gated, see `is_stale_generated_shim`. Takes no
        // list from here: what this spawn resolved must never decide what is kept
        // (`GENERATED_SHIM_NAMES`, #3481 B1).
        prune_stale_shims(&dir);
        (gh || git).then_some(dir)
    }

    /// The extra environment injected into an *agent* pane (never a human's plain
    /// shell): the gh + git shims prepended to PATH so the merge/release gates are
    /// enforced, and `{ORRERIX,LOOMUX}_GROUP_DIR`/`_AGENT_ID` — both spellings,
    /// #1153 phase 3 — so a shim or hook script
    /// finds this group's markers/grants and knows which agent it's running for.
    ///
    /// the agent-id variable (#417 Copilot correction) exists specifically for
    /// Copilot's compact-hook script: unlike Claude's `--settings` file (baked
    /// per-agent with the id in its own argv), Copilot's hook config is a
    /// GLOBAL, machine-wide file (`~/.copilot/hooks/*.json`) shared by every
    /// Copilot session on the box, loomux-launched or not — so the script has
    /// no argv to read an id from and must recover both facts from its own
    /// inherited environment instead. Absence of these two vars (a human's own
    /// non-loomux Copilot session) is precisely the signal the script uses to
    /// no-op — see `COPILOT_COMPACT_HOOK_SCRIPT`'s doc.
    ///
    /// Only ever computed when at least the shims exist (gh or git installed);
    /// empty when neither is, which would also mean an agent pane never gets
    /// these two vars — an acceptable joint fate today since every environment
    /// that installs Claude/Copilot for loomux-managed development is assumed
    /// to have git.
    pub(in crate::orchestration) fn agent_pane_env(&self, group: &GroupId, agent_id: &str) -> Vec<(String, String)> {
        let Some(shim) = self.ensure_shims() else {
            return Vec::new();
        };
        let sep = if cfg!(windows) { ';' } else { ':' };
        let base = crate::winpath::fresh_path()
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        let path = format!("{}{sep}{base}", shim.display());
        // #1153 phase 3: BOTH spellings, same values. Not a nicety — an
        // operator's wrapper scripts, a repo persona, and this app's OWN shims
        // already on disk in a live group all read the legacy names, and none
        // of those is something an upgrade may rewrite. The new name is what
        // everything written from here on reads; the old one is what everything
        // already written keeps reading. Both are exported for as long as
        // `brand::LEGACY_ENV_PREFIX` exists, and dropping either is a
        // deliberate, separately-argued break (see `brand`).
        let dir = self.group_dir(group).display().to_string();
        let mut env = vec![("PATH".to_string(), path)];
        for prefix in [brand::ENV_PREFIX, brand::LEGACY_ENV_PREFIX] {
            env.push((format!("{prefix}GROUP_DIR"), dir.clone()));
            env.push((format!("{prefix}AGENT_ID"), agent_id.to_string()));
        }
        env
    }

    /// Write the per-agent MCP config the agent CLI connects with, and return
    /// it together with the pane environment that CLI needs to read it (see
    /// [`AgentCliConfig`]). Claude and Copilot share the same core schema;
    /// Copilot additionally expects a `tools` allowlist inside the server entry.
    ///
    /// `unattended` and `persona` are opencode's (#722): its single document
    /// carries the permission posture (which differs for an attended pane) and
    /// the block's agent entry (which `persona_inject` must have produced
    /// first — hence the call order at both spawn sites). Both are ignored by
    /// every other adapter.
    ///
    /// rev-4 review (N2): this file used to ALSO carry the #417 hook config
    /// (folded into a top-level `hooks` key, passed a second time via
    /// `--settings`) — one generated file, two flags. That only stays safe
    /// while `--mcp-config`'s reader and `--settings`'s reader each happen to
    /// ignore the other's top-level keys, which is a schema assumption
    /// neither this file nor Claude Code's own docs pin down; a future
    /// Claude Code release tightening either reader's validation would be a
    /// silent breakage with no test able to catch it locally. Split instead
    /// (see `write_hook_settings_file`): two small generated files, one per
    /// flag, at the cost of one extra `fs::write` per Claude spawn.
    pub(in crate::orchestration) fn write_mcp_config(
        &self,
        group: &GroupId,
        agent_id: &str,
        token: &str,
        cli: &str,
        // The directory this pane will be launched in — read by the pi branch
        // alone, to MEASURE what the repo declares that pi's MCP adapter will
        // merge into the pane's tool surface (`pi_repo_mcp_exposure`). Passed
        // rather than re-derived so the file that is written and the tree that
        // is scanned are the same spawn's, and so the check cannot drift to a
        // different directory than the one the pane gets.
        workdir: &Path,
        containment: Containment,
        unattended: bool,
        // #2515 C1: read by the codex branch alone. codex has no effort FLAG —
        // `model_reasoning_effort` is a key in the profile file this function
        // writes — so a knob that is argv-borne on claude and pi is
        // config-borne here, and the two builders emit nothing for it. Passed
        // as the whole `ModelKnobs` rather than a bare `&str` so this parameter
        // means the same thing as the builders' does; `context` is unread,
        // because codex has no context-variant key (`CliCaps` says so).
        knobs: workflow::ModelKnobs<'_>,
        persona: &PersonaInject,
    ) -> Result<AgentCliConfig, String> {
        let port = self.port();
        if port == 0 {
            return Err("loomux MCP server is not running".into());
        }
        // #925: the agent id becomes three file names below this point
        // (`{id}.json`, `{id}-gemini-policy.toml`, `{id}-hooks.json`). Parse
        // once, here, and thread the proof — the same parse-at-the-boundary
        // shape `command_group` gives a group id.
        let agent_seg = PathSegment::parse(agent_id)
            .map_err(|e| format!("invalid agent id {agent_id:?}: {e}"))?;
        let dir = self.group_dir(group).join("configs");
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(format!("{agent_seg}.json"));
        // Gemini's file is a different schema entirely (its own settings.json,
        // not the claude/copilot MCP-config shape) and carries this agent's
        // containment as well as its MCP server — see `gemini_settings_json`.
        if cli == "gemini" {
            let policy = self.write_gemini_policy(&dir, &agent_seg, containment)?;
            let cfg = gemini_settings_json(port, token, containment, policy.as_deref());
            fs::write(&path, cfg).map_err(|e| e.to_string())?;
            let env = cli_extra_env(cli, &path, token);
            return Ok(AgentCliConfig { path, env });
        }
        // OpenCode (#722): the config document is delivered by environment
        // variable, so the file written here is the AUDIT copy — what the pane
        // was handed, readable after the fact — while the env carries the
        // authoritative bytes. Both come from one call to one generator, so
        // they cannot disagree.
        if cli == "opencode" {
            let agent = persona
                .opencode_agent
                .as_deref()
                .zip(persona.opencode_prompt_file.as_deref());
            let cfg = opencode_config_json(port, token, containment, unattended, agent);
            fs::write(&path, &cfg).map_err(|e| e.to_string())?;
            // SQLite creates the database file but not its parent directory,
            // and a pane whose store cannot be opened does not boot — so this
            // is an error, not a best-effort mkdir.
            let db = self.opencode_db_path(group);
            let db_dir = db.parent().expect("opencode_db_path always has a parent").to_path_buf();
            fs::create_dir_all(&db_dir).map_err(|e| e.to_string())?;
            let env = opencode_pane_env(&cfg, containment, unattended, &db);
            return Ok(AgentCliConfig { path, env });
        }
        // codex (#2515 C1): the only branch whose file lands OUTSIDE loomux's
        // own state root, because codex's `-p` names a profile in `CODEX_HOME`
        // rather than a path anywhere. `path` below is therefore that file, not
        // one under `configs/` — the `path` computed above is unused on this
        // branch, and `AgentCliConfig.path` documents itself as "the generated
        // file", which this is.
        //
        // It carries far more than MCP: trust, approval posture, sandbox mode,
        // network access, the effort knob and the role contract all ride it.
        // That is why the function's name undersells this branch, and why
        // `write_codex_profile` is where the argument for each key lives.
        if cli == "codex" {
            // #3456: the git metadata a commit writes, which a linked worktree
            // keeps OUTSIDE the pane's directory. A layout this does not
            // recognise grants nothing and says why in the audit log — the
            // pane still spawns (it can still edit and `report`), but its first
            // `git commit` would otherwise fail with no visible cause.
            let git_access = match codex_worktree_git_access(workdir) {
                Ok(access) => access,
                Err(why) => {
                    self.audit(
                        group,
                        brand::AUDIT_ACTOR,
                        "codex-worktree-gitdir-unrecognised",
                        // Capped like `codex_gh_token_env`'s reason: the
                        // text can quote `commondir`, which the pane can
                        // write (#3456 review N1).
                        json!({
                            "agent": agent_id,
                            "why": notify::sanitize_gh_text(&why, notify::NOTICE_FIELD_CAP),
                        }),
                    );
                    CodexGitAccess::default()
                }
            };
            let (path, _name) = self.write_codex_profile(
                group,
                &agent_seg,
                // A GROUP pane, so the token rides the pane environment and
                // the file names the variable — plan D2. `solo_prepare` is the
                // caller that cannot do this, and it passes `Literal`; see
                // `CodexMcpAuth`.
                CodexMcpAuth::EnvVar(CODEX_TOKEN_ENV),
                workdir,
                unattended,
                knobs.effort,
                persona.codex_developer_instructions.as_deref(),
                &git_access,
            )?;
            let mut env = cli_extra_env(cli, &path, token);
            // #3405: the human's `gh` credential, which codex's sandbox cannot
            // reach on its own — see `codex_gh_token_env`. Pane environment
            // only, never the profile: the file above names no GitHub token.
            env.extend(self.codex_gh_token_env(group, agent_id, workdir));
            return Ok(AgentCliConfig { path, env });
        }
        // pi (#2126): the file written here IS the file named on
        // `--mcp-config` — audit copy and authoritative bytes are one
        // artifact, as claude's are — and the pane environment carries no part
        // of this agent's identity (see `cli_extra_env`'s pi arm).
        if cli == "pi" {
            let cfg = pi_mcp_config_json(port, token, &pi_server_name(&agent_seg));
            fs::write(&path, cfg).map_err(|e| e.to_string())?;
            // pi WOULD create this directory itself — `SessionManager`'s
            // constructor `mkdirSync`s it when persisting — so this is not
            // load-bearing for the pane's boot, and claiming otherwise would
            // be a claim the vendor's source contradicts.
            //
            // It is an error rather than a best-effort mkdir for a different
            // and smaller reason: an unwritable group dir is a real fault, and
            // finding it HERE fails the spawn visibly, next to the config
            // write that would fail for the same cause, instead of surfacing
            // several turns later as a session pi could not persist. The
            // resume lookup is separately tolerant of the directory being
            // absent (`pi_session_cwd_in_dir` answers "not found"), so nothing
            // downstream depends on this having run.
            fs::create_dir_all(self.pi_sessions_dir(group)).map_err(|e| e.to_string())?;
            // Measure-and-warn, once per spawn, and NEVER a refusal — see
            // `pi_repo_mcp_exposure`. Best-effort by construction: it returns
            // `None` for the repo that declares nothing, which is nearly every
            // repo, so the ordinary spawn writes no row at all.
            if let Some(exposure) = pi_repo_mcp_exposure(
                workdir,
                &pi_server_name(&agent_seg),
                &crate::orchestration::mcp::every_tool_name(),
            ) {
                let mut row = exposure;
                row["agent"] = json!(agent_id);
                self.audit(group, brand::AUDIT_ACTOR, "pi-repo-mcp-merged", row);
            }
            let env = cli_extra_env(cli, &path, token);
            return Ok(AgentCliConfig { path, env });
        }
        let mut server = json!({
            "type": "http",
            "url": format!("http://127.0.0.1:{port}/mcp"),
            "headers": agent_token_headers(token),
        });
        if cli == "copilot" {
            server["tools"] = json!(["*"]);
        }
        let cfg = json!({ "mcpServers": one_server_map(server) });
        fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).map_err(|e| e.to_string())?;
        let env = cli_extra_env(cli, &path, token);
        Ok(AgentCliConfig { path, env })
    }

    /// Write this agent's gemini policy-engine file — the admin-tier half of a
    /// contained gemini agent's deny rules (#267) — returning its path, or
    /// `None` for an uncontained class (there is nothing to deny).
    ///
    /// Admin tier is the point: gemini's policy tiers are Default(1) <
    /// Extension(2) < Workspace(3) < User(4) < Admin(5), and a supplemental
    /// admin policy — one named by `adminPolicyPaths` in the settings file this
    /// sits beside — gets the same Admin base as a system-installed one. So the
    /// user's own `~/.gemini/policies/*.toml` cannot re-allow what loomux
    /// denies, and neither can `--approval-mode yolo` (whose allow-all rule is
    /// Default-tier). Same property Claude's `--disallowedTools` has by
    /// documented precedence, reached a different way.
    ///
    /// Failing to write this is an ERROR, not a fail-open: every other
    /// generated-file failure in this module is best-effort because the cost is
    /// a missed hook or a stale breadcrumb, and the cost here is an
    /// uncontained reviewer. `write_mcp_config`'s `?` sends it to the spawn's
    /// own error path.
    fn write_gemini_policy(
        &self,
        dir: &Path,
        agent_id: &PathSegment,
        containment: Containment,
    ) -> Result<Option<PathBuf>, String> {
        if !containment.denies_edits() {
            return Ok(None);
        }
        let path = dir.join(format!("{agent_id}-gemini-policy.toml"));
        fs::write(&path, gemini_policy_toml(containment)).map_err(|e| e.to_string())?;
        Ok(Some(path))
    }

    /// Claude's `--settings` file (#417, split from `--mcp-config`'s file per
    /// rev-4 review N2 — see `write_mcp_config`'s doc): nothing shared with the
    /// MCP config's schema. Three keys, each written only when this agent
    /// actually has one:
    ///
    /// - `hooks` — the #417/#112 compact-lifecycle config, absent when
    ///   `compact_hook_settings` has nothing to write (no `sh` resolvable).
    /// - `statusLine` — #993 S1, present exactly when `hooks` is (same script,
    ///   same `sh`); chains to the human's own status line resolved from `cwd`
    ///   (see `user_statusline`).
    /// - `permissions` — #610, for a [`Containment::is_read_only`] pane only:
    ///   [`CLAUDE_READONLY_SETTINGS_ALLOW`] under `allow` (the surface `dontAsk`
    ///   is documented to consult — see that constant for the full argument and
    ///   the WebFetch/`git fetch` decisions) and, since #614's review, the
    ///   matching `deny` from `readonly_settings_deny` so the rule that must
    ///   beat `Bash(git *)`'s prefix match sits in the same object as the allow
    ///   itself.
    ///
    /// **The name is now imprecise, deliberately** (same reasoning as
    /// `COMPACT_HOOK_SCRIPT`'s "Naming, kept imprecise on purpose"): this
    /// function and its `{agent_id}-hooks.json` output can carry a `permissions`
    /// block and no hooks at all. Renaming would strand every already-spawned
    /// agent whose `--settings` argv bakes in the old path, which is a worse
    /// trade than an imprecise name one comment can fix.
    ///
    /// **The `None` policy changed with #610, and only for hooks.** This used
    /// to return `None` whenever hooks were unavailable, and the caller then
    /// omitted `--settings` entirely. Fail-open is right for a hook (the cost
    /// is a missed compact nudge) and wrong for permissions: a `dontAsk` pane
    /// with no allow rules can do nothing at all, so a read-only pane now
    /// always gets a file. `None` survives for the case that is still genuinely
    /// empty — a non-read-only agent with no hooks — because pointing
    /// `--settings` at an empty object would be a flag with no content.
    ///
    /// Keys that would be empty are OMITTED rather than written as `{}`:
    /// `--settings` values "override the same keys in your `settings.json`
    /// files for this session" per the CLI reference, so an empty `hooks` key
    /// is not an inert placeholder — it is a claim about hooks that could
    /// displace the user's own.
    /// `agent_id` is a [`PathSegment`] (#925): its `{agent_id}-hooks.json`
    /// output is a file name, so the id must be proven a single component
    /// before it gets there.
    pub(in crate::orchestration) fn write_hook_settings_file(
        &self,
        group: &GroupId,
        agent_id: &PathSegment,
        containment: Containment,
        cwd: &Path,
    ) -> Option<PathBuf> {
        let (hooks, status_line) = match self.compact_hook_settings(group, agent_id, cwd) {
            Some(s) => (Some(s.hooks), Some(s.status_line)),
            None => (None, None),
        };
        let permissions = containment.is_read_only().then(|| {
            let mut p = serde_json::Map::new();
            p.insert("allow".into(), json!(CLAUDE_READONLY_SETTINGS_ALLOW));
            // #614 review B1: the denials ride the SAME object as the allow
            // they have to beat, so `Bash(git *)`'s prefix match over
            // `git commit`/`git push` is carved out inside one documented
            // precedence domain ("Rules are evaluated in order: deny, then
            // ask, then allow") instead of across two mechanisms.
            // `--disallowedTools` stays emitted too — see
            // `CLAUDE_READONLY_SETTINGS_ALLOW`'s doc for the citation that
            // makes the cross-layer direction sound as well.
            let deny = readonly_settings_deny(containment);
            if !deny.is_empty() {
                p.insert("deny".into(), json!(deny));
            }
            Value::Object(p)
        });
        if hooks.is_none() && permissions.is_none() {
            return None;
        }
        let mut cfg = serde_json::Map::new();
        if let Some(hooks) = hooks {
            cfg.insert("hooks".into(), hooks);
        }
        if let Some(status_line) = status_line {
            cfg.insert("statusLine".into(), status_line);
        }
        if let Some(permissions) = permissions {
            cfg.insert("permissions".into(), permissions);
        }
        let dir = self.group_dir(group).join("configs");
        fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{agent_id}-hooks.json"));
        fs::write(&path, serde_json::to_string_pretty(&Value::Object(cfg)).unwrap()).ok()?;
        Some(path)
    }

    /// The shared directory holding the generic compact-lifecycle hook
    /// script (#417) — a sibling of `shim_dir()`, one copy for every group:
    /// the script itself carries no group- or agent-specific text, only argv
    /// (the group's state dir + the agent's id, baked into each agent's own
    /// `command` string below). Test-overridable (see
    /// `compact_hook_dir_override`'s doc) — every real deployment gets the
    /// derived path.
    fn compact_hook_dir(&self) -> PathBuf {
        if let Some(dir) = self.compact_hook_dir_override.lock_safe().clone() {
            return dir;
        }
        self.root.parent().unwrap_or(&self.root).join("compacthook")
    }

    /// Write (or refresh) the generic hook script, returning its path or
    /// `None` if it can't be written. Idempotent/always-rewritten, like
    /// `write_shim` — cheap, and keeps it current if loomux itself updated.
    ///
    /// Still named `compact-hook.sh` even though #112 added a `promptsubmit`
    /// arm that has nothing to do with compaction — see `COMPACT_HOOK_
    /// SCRIPT`'s doc ("Naming, kept imprecise on purpose") for why the name
    /// stays: an already-spawned agent's `--settings`/hooks-config command
    /// bakes in this exact path, and renaming would strand it.
    fn ensure_compact_hook_script(&self) -> Option<PathBuf> {
        let dir = self.compact_hook_dir();
        fs::create_dir_all(&dir).ok()?;
        let path = dir.join("compact-hook.sh");
        fs::write(&path, COMPACT_HOOK_SCRIPT).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o755));
        }
        Some(path)
    }

    /// The `sh` interpreter to run the hook script with, resolved the same
    /// way `ensure_shims`'s Windows `.cmd` delegators bake in an absolute
    /// `sh.exe` (#335) rather than trusting a bare `sh` on PATH (not
    /// guaranteed on Windows) or Claude Code's own shell resolution, which
    /// loomux has no visibility into. `None` when no `sh` exists anywhere
    /// (essentially POSIX-only risk; Windows resolves via git's own install
    /// layout) — hook provisioning is then skipped entirely for this spawn,
    /// same fail-open policy as a missing gh/git shim.
    fn resolve_hook_sh(&self) -> Option<PathBuf> {
        #[cfg(target_os = "windows")]
        {
            let git = crate::winpath::resolve_program(
                "git",
                &crate::winpath::launch_path(),
                &crate::winpath::launch_pathext(),
            )?;
            crate::winpath::resolve_sh(&git, &crate::winpath::launch_path(), &crate::winpath::launch_pathext())
        }
        #[cfg(not(target_os = "windows"))]
        {
            Some(PathBuf::from("/bin/sh"))
        }
    }

    /// Claude's PreCompact + SessionStart(compact) hook config (#417) — the
    /// `hooks` object `write_hook_settings_file` writes, which rides on
    /// `--settings` (an ADDITIVE settings layer Claude Code composes over
    /// the user's own `.claude/settings.json`, never a hand merge of it —
    /// "never clobber the user's hooks" is a property of the CLI flag
    /// itself here, not something loomux re-implements by
    /// parsing their JSON).
    ///
    /// The `command` for each event explicitly invokes the resolved `sh`
    /// (never a bare `sh` on PATH) against the generic script, with the
    /// event name + this group's state dir + this agent's id as literal argv
    /// — so the script itself carries zero repo/group-specific text (#8's
    /// generic-product constraint) and no new env var is needed to tell it
    /// which agent it's running for.
    ///
    /// `None` when no `sh` is resolvable — the agent then has no hooks
    /// configured at all, same as before this feature existed (fail-open, not
    /// a spawn failure).
    ///
    /// #112 adds `UserPromptSubmit`, the real prompt-landed signal
    /// (`deliver_prompt`'s confirm phase — see the `resolve_submit_
    /// confirmation` doc). Per the hooks reference, `UserPromptSubmit`
    /// "does NOT support matchers and always fires on every prompt
    /// submission" — unlike `SessionStart` above, no `matcher` key is
    /// written at all (one was silently ignored anyway, per the docs; this
    /// mirrors reality rather than adding a key the CLI would discard).
    ///
    /// #413 S5 adds `PostCompact` — "after context compaction completes", per
    /// the same reference — with no `matcher`, so it fires after a manual
    /// `/compact` and an auto-compact alike, as `PreCompact` above does. Its
    /// marker is the trusted compaction-DONE signal `compact_nudge_tick` settles
    /// and resolves on (see `POSTCOMPACT_SETTLE_MS`). It is written only for a
    /// Claude Code the probe knows is `2.1.76` or later
    /// (`claude_supports_postcompact`): an older one would ignore the whole file.
    ///
    /// #993 S1 adds the `statusLine` entry, returned beside `hooks` because it
    /// is a TOP-LEVEL settings key, not a hook event — and derived from the same
    /// script and `sh`, so a machine that cannot run the hooks gets no status
    /// line either (never a `statusLine` pointing at a script that was not
    /// written). Unlike the hooks it is NOT additive: `--settings` outranks the
    /// human's own `statusLine`, which is why its command chains to theirs
    /// (`user_statusline`) and carries their `padding`/`refreshInterval`.
    fn compact_hook_settings(&self, group: &GroupId, agent_id: &str, cwd: &Path) -> Option<ClaudeHookSettings> {
        let script = self.ensure_compact_hook_script()?;
        let sh = self.resolve_hook_sh()?;
        // Forward-slashed so the path is safe inside the POSIX script's own
        // argv handling, mirroring `write_shim`'s identical reasoning.
        let group_dir = self.group_dir(group).display().to_string().replace('\\', "/");
        let script_fwd = script.display().to_string().replace('\\', "/");
        let sh_fwd = sh.display().to_string().replace('\\', "/");
        let cmd = |event: &str| format!("\"{sh_fwd}\" \"{script_fwd}\" {event} \"{group_dir}\" \"{agent_id}\"");
        let user = self.user_statusline(cwd);
        let mut status_line = serde_json::Map::new();
        status_line.insert("type".into(), json!("command"));
        status_line.insert(
            "command".into(),
            json!(crate::modelstate::with_chained_command(
                &cmd("statusline"),
                user.as_ref().map(|u| u.command.as_str()),
            )),
        );
        if let Some(p) = user.as_ref().and_then(|u| u.padding) {
            status_line.insert("padding".into(), json!(p));
        }
        if let Some(r) = user.as_ref().and_then(|u| u.refresh_interval) {
            status_line.insert("refreshInterval".into(), json!(r));
        }
        let mut hooks = json!({
            "PreCompact": [{ "hooks": [{ "type": "command", "command": cmd("precompact") }] }],
            "SessionStart": [{ "matcher": "compact", "hooks": [{ "type": "command", "command": cmd("sessionstart-compact") }] }],
            "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": cmd("promptsubmit") }] }],
        });
        // #413 S5 review r2: only for a Claude Code known to have the event — an
        // older one would ignore this WHOLE file (see
        // `claude_supports_postcompact`). Unknown, including a pane spawned
        // before the startup probe landed, gets none.
        if claude_supports_postcompact(claude_cached_version().as_deref()) {
        }
        Some(ClaudeHookSettings { hooks, status_line: Value::Object(status_line) })
    }

    /// #993 S1: the human's own Claude status line, read ONCE at spawn, that
    /// loomux's `statusline` hook chains to — so an orrerix pane shows the line
    /// a plain `claude` in the same directory would.
    ///
    /// The layers are read in Claude Code's own settings precedence
    /// (code.claude.com/docs/en/settings, "Settings precedence": managed, then
    /// command-line arguments, then local project, then shared project, then
    /// user), skipping the two loomux cannot or must not read as the human's:
    /// managed settings outrank `--settings` anyway, so a managed `statusLine`
    /// replaces loomux's entry and this chain never runs (the snapshot is then
    /// simply absent and the transcript reader carries on); and the
    /// command-line layer IS loomux's own file. What remains, highest first:
    /// `<cwd>/.claude/settings.local.json`, `<cwd>/.claude/settings.json`, and
    /// the user file. `cwd` is the pane's working directory — the directory
    /// claude resolves its project settings from.
    ///
    /// The user file goes through [`Self::user_cli_dir`], so a registry that is
    /// not the human's live one reads a contained stand-in inside its own root
    /// (#502) rather than the developer's real `~/.claude/settings.json`.
    /// Unreadable files are simply absent layers: a status line is cosmetic,
    /// and a spawn never fails over one.
    fn user_statusline(&self, cwd: &Path) -> Option<crate::modelstate::UserStatusLine> {
        let mut paths = vec![cwd.join(".claude").join("settings.local.json"), cwd.join(".claude").join("settings.json")];
        if let Some(user) = self.user_cli_dir(".claude", "settings.json") {
            paths.push(user);
        }
        let texts: Vec<String> = paths.iter().filter_map(|p| fs::read_to_string(p).ok()).collect();
        let layers: Vec<&str> = texts.iter().map(String::as_str).collect();
        crate::modelstate::resolve_user_statusline(&layers)
    }

    /// #417 (Copilot correction): Copilot's own user-level hook config
    /// directory — `$COPILOT_HOME/hooks` if that env var is set, else
    /// `~/.copilot/hooks` per GitHub's docs
    /// (docs.github.com/en/copilot/reference/hooks-reference). Test-
    /// overridable (`copilot_hooks_dir_override`), mirroring
    /// `copilot_agents_dir`. Deliberately NEVER the repo's `.github/hooks/`
    /// — same reasoning as never writing into the repo's `.github/agents/`
    /// (#416): a generated file there would dirty the user's git tree with
    /// something they didn't author. The user-level directory is Copilot's
    /// OWN convention for machine-wide config, and — confirmed by the docs
    /// (constraint: "When the same event appears in multiple sources, all
    /// hook entries from all sources are run") — genuinely ADDITIVE: adding
    /// a file here is proven never to clobber the user's own hooks, unlike
    /// Claude's `--settings`, whose merge semantics this repo could not
    /// verify (see `compact_hook_settings`'s doc).
    fn copilot_hooks_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = self.copilot_hooks_dir_override.lock_safe().clone() {
            return Some(dir);
        }
        // #502: one of three uncontained writes into the user's home this
        // issue closed — same defect class `user_cli_dir` closes for the
        // agent dirs. Blast radius here is the smallest of the three: ONE
        // idempotent, always-rewritten file the real app writes anyway
        // (`loomux-compact.json`), never one per group, so it never
        // accumulated the way the agent files did — but a throwaway registry
        // still had no business rewriting the user's real Copilot hook
        // config, and it was observed doing exactly that during a test run.
        //
        // rev-38 review (B1) corrected the record here: an earlier revision
        // of this comment claimed this was "the LAST uncontained write" after
        // auditing every `dirs::home_dir()` in the module. That was WRONG —
        // `pre_trust_copilot_folder` was a third one, and the worst of them
        // (a security setting, growing without bound). The audit missed it
        // because it was a free function with no registry to ask, which is
        // exactly why the containment rule now lives in ONE predicate
        // (`is_live_registry`) that every home-resolving path consults,
        // rather than being open-coded per site. `compact_hook_dir` was and
        // remains root-relative already.
        Some(self.copilot_home_dir()?.join("hooks"))
    }

    /// Write (or refresh) loomux's `PreCompact` hook config into Copilot's
    /// user-level hooks directory — a single small, GENERIC, machine-wide
    /// file (`loomux-compact.json`, idempotent/always-rewritten, like the
    /// gh/git shims), never one generated per group or per agent: unlike
    /// Claude's `--settings` (baked per-agent with the agent's id in its own
    /// argv), Copilot's hook config is a single GLOBAL surface shared by
    /// every Copilot session on the machine — loomux-launched or not — so
    /// there is exactly one file to maintain, and the hook script recovers
    /// which (if any) loomux agent it's running for from its OWN inherited
    /// environment at invocation time (the group-dir/agent-id variables
    /// — see `agent_pane_env`'s doc), never from anything baked into this
    /// file. Absent either var (a human's own, non-loomux Copilot session on
    /// this machine), the inline command below is a silent no-op — the
    /// discrimination the review round required, done via the SAME
    /// env-var-presence idiom the gh/git shims already use (`LOOMUX_GROUP_
    /// DIR` unset ⇒ "not a loomux pane, do nothing"), rather than matching
    /// the payload's `cwd` against a live list of loomux worktrees: that
    /// would need either a registration file the script re-reads on every
    /// invocation (extra I/O and a staleness/race surface) or a static list
    /// baked in at generation time (stale the moment a new group spawns
    /// after this file was last written) — env-var inheritance has neither
    /// problem, since it is always current for the actual process the hook
    /// runs inside.
    ///
    /// The command itself only WRITES A MARKER FILE, matching Claude's
    /// `precompact` case exactly — no payload parsing needed (the marker's
    /// mere existence, at `read_hook_marker_ts`'s mtime, is the whole
    /// signal), so the script never needs to read the JSON Copilot pipes in.
    /// Both `bash` and `powershell` fields are always provided (per the
    /// docs' own two-variant `command` hook shape) — Copilot picks the one
    /// for its host OS itself, so loomux never needs its own `sh.exe`
    /// resolution dance the way Claude's hook command does.
    ///
    /// Returns `false` (never fatal — fail-open, same as a missing gh/git
    /// shim) when the hooks directory can't be created/written.
    ///
    /// #112 adds `userPromptSubmitted` alongside `preCompact` in the SAME
    /// file (still one small global file, same additive-merge guarantee) —
    /// existence-only, per `COPILOT_PROMPTSUBMIT_HOOK_BASH`'s doc: the
    /// payload transport for this event isn't documented, so the command
    /// never attempts to read it. The file stays named `loomux-compact.json`
    /// for the same reason `compact-hook.sh` stays named that (see
    /// `COMPACT_HOOK_SCRIPT`'s doc): this is a rewrite-in-place, single
    /// well-known path every Copilot session on the box already resolves —
    /// renaming it would just mean the OLD file (with the OLD hook set)
    /// sits there stale until the next write, not a clean cutover.
    pub(in crate::orchestration) fn ensure_copilot_compact_hook(&self) -> bool {
        let Some(dir) = self.copilot_hooks_dir() else { return false };
        if fs::create_dir_all(&dir).is_err() {
            return false;
        }
        let cfg = json!({
            "version": 1,
            "hooks": {
                "preCompact": [{
                    "type": "command",
                    "bash": COPILOT_PRECOMPACT_HOOK_BASH,
                    "powershell": COPILOT_PRECOMPACT_HOOK_POWERSHELL,
                    "timeoutSec": 10,
                }],
                "userPromptSubmitted": [{
                    "type": "command",
                    "bash": COPILOT_PROMPTSUBMIT_HOOK_BASH,
                    "powershell": COPILOT_PROMPTSUBMIT_HOOK_POWERSHELL,
                    "timeoutSec": 10,
                }],
            },
        });
        fs::write(dir.join("loomux-compact.json"), serde_json::to_string_pretty(&cfg).unwrap()).is_ok()
    }

    /// Build an agent's launch command for the group's CLI. Baseline
    /// permissions minimize the approvals needed just to *initialize*: the
    /// group state dir is added as a workspace (so reading the instructions
    /// file never prompts) and the orrerix MCP tools are pre-approved (so
    /// `report` etc. never prompt). `auto_ops` additionally pre-approves
    /// git/gh commands so the branch→commit→PR flow runs unattended;
    /// everything else still asks the human. A [`Containment::ReadOnly`] planner
    /// is *always* treated as unattended (Auto perms + git/gh allowlist)
    /// regardless of `auto_ops`: it never mutates and has no human in its
    /// pane, so gating it would only deadlock it (see below).
    ///
    /// `containment` hardens a class's contract at the CLI level (#47, #462):
    /// where the CLI supports tool denial, the file-editing tools are denied
    /// outright for every contained class, and a `ReadOnly` one additionally
    /// loses the git mutation subcommands (`commit`/`push`) — so a planner
    /// cannot write code or create branches/commits/pushes even under Auto
    /// perms, and a reviewer cannot reach for an editing tool at all. `gh` stays
    /// available throughout (the planner posts its plan as an issue comment; the
    /// reviewer posts its review). Deny rules take precedence over the allow
    /// list on both CLIs. NOTE: these are real, structural denials of the *tools
    /// named*; they are deliberately NOT a sandbox (`gh pr create` stays
    /// reachable, and so does every write a shell command can perform), so the
    /// complete contract still rests partly on each class's instructions — see
    /// [`Containment`] for the exact size of each tier's guarantee.
    ///
    /// `persona` compiles a workflow block's persona down to the CLI's **native**
    /// custom-agent flag (#222) — see [`PersonaInject`]. A block with no persona
    /// passes `PersonaInject::default()`, which adds nothing: that is what makes
    /// a group with no `.loomux/workflow.yml` byte-for-byte identical to
    /// pre-#222 loomux (pinned by `default_roster_command_lines_match_legacy`).
    ///
    /// Ordering matters and is not cosmetic, in BOTH directions (#610). Claude's
    /// `--allowedTools` takes space-separated values in one occurrence, so its
    /// value list runs from the flag to the next flag — and every allow pattern
    /// must sit inside it:
    /// - *After the flag's first value, before any other flag.* A flag emitted
    ///   mid-list ends it, demoting every pattern after it to a stray positional
    ///   argument. `--settings` (#417) did exactly that to the whole git/gh
    ///   allowlist, silently, from the release that added it (#610).
    /// - *Before `--disallowedTools`* — otherwise the same patterns would be
    ///   parsed as *denials*.
    ///
    /// So a new flag added to the claude branch goes after the allow values, not
    /// between them and their flag. `claude_allow_patterns_are_not_severed_from_
    /// the_allowedtools_flag` pins the property rather than the current order.
    ///
    /// This form pins **no model knobs** (#687) — it is
    /// [`Self::build_agent_command_ex`] with [`workflow::ModelKnobs::default`],
    /// i.e. the pre-#687 command line byte for byte. Every spawn path calls the
    /// `_ex` form with its block's knobs (the `spawn_agent`/`spawn_agent_ex`
    /// shape); this one survives because "a block that pinned nothing produces
    /// exactly today's line" is a property worth keeping directly assertable.
    ///
    /// Nor does it carry a role (#946 Q4 / #1091 slice H) — it always builds
    /// as `Role::Worker` with no `role_hint`, the one combination
    /// [`claude_denies_interactive_question`] is guaranteed to answer `false`
    /// for, so this form's long-standing callers (most of this suite) see no
    /// behavior change from that predicate existing. A caller that wants the
    /// question-deny predicate evaluated for a real role calls
    /// [`Self::build_agent_command_ex`] directly, the way both real spawn
    /// sites do.
    #[allow(clippy::too_many_arguments)]
    #[doc(hidden)] // pub for integration tests
    pub fn build_agent_command(
        &self,
        cli: &str,
        model: &str,
        auto_ops: bool,
        cfg: &Path,
        hook_settings: Option<&Path>,
        group_dir: &Path,
        workdir: &Path,
        session: Option<&str>,
        resume: bool,
        containment: Containment,
        persona: &PersonaInject,
    ) -> String {
        // Never a fork, so the infallible body directly — see
        // `agent_command_line` for why there is no `unwrap` here.
        self.agent_command_line(
            cli,
            model,
            workflow::ModelKnobs::default(),
            auto_ops,
            cfg,
            hook_settings,
            group_dir,
            workdir,
            session,
            resume,
            containment,
            persona,
            Role::Worker,
            None,
            None,
        )
    }

    /// [`Self::build_agent_command`] with the block's model knobs (#687):
    /// `knobs.effort` becomes claude's `--effort <level>` and `knobs.context`
    /// composes the `{model}[{variant}]` alias. Both are **only-when-set**, and
    /// neither is emitted on copilot or gemini — see [`CliCaps`] for why (their
    /// seams are a user-owned settings file and an interactive control), which
    /// is also why an unset knob is not merely a default but the whole
    /// back-compat guarantee: a group on a CLI build predating `--effort` is
    /// unaffected unless a human opts in.
    ///
    /// `role`/`role_hint` (#946 Q4 / #1091 slice H) feed
    /// [`claude_denies_interactive_question`] alone — a role-keyed deny
    /// orthogonal to `containment`, never a substitute for it. See that
    /// function's doc for the predicate and [`CLAUDE_QUESTION_DENY_TOOLS`]
    /// for what gets denied.
    ///
    /// `fork_of` (#3318) asks the CLI to **fork** the session it names — open
    /// a new session that starts as a copy of that one's conversation, leaving
    /// the original untouched. Each arm spells it from its own
    /// [`model::CliCaps::fork`] row (claude and opencode a flag, codex a
    /// subcommand, pi a flag naming the parent beside `--session-id <child>`).
    /// **A CLI whose row is [`ForkSeam::None`] is REFUSED** with the row's own
    /// note — an `Err`, never a line (#3331 item 2; see [`fork_line`] for why
    /// F1's silent drop stopped being safe once F2's tool could reach it).
    ///
    /// **`fork_of` overrides `resume`**, because the two ask incompatible
    /// things of one session argument: `--resume <id>` alone continues that
    /// session, and a fork branches off it. The `session` argument then names
    /// the CHILD rather than the session being opened, and is read only on a
    /// seam that pre-mints one ([`ForkSeam::premints_child`]).
    #[allow(clippy::too_many_arguments)]
    #[doc(hidden)] // pub for integration tests
    pub fn build_agent_command_ex(
        &self,
        cli: &str,
        model: &str,
        knobs: workflow::ModelKnobs<'_>,
        auto_ops: bool,
        cfg: &Path,
        hook_settings: Option<&Path>,
        group_dir: &Path,
        workdir: &Path,
        session: Option<&str>,
        resume: bool,
        containment: Containment,
        persona: &PersonaInject,
        role: Role,
        role_hint: Option<&str>,
        fork_of: Option<&str>,
    ) -> Result<String, String> {
        let fork = fork_line(cli, fork_of)?;
        Ok(self.agent_command_line(
            cli, model, knobs, auto_ops, cfg, hook_settings, group_dir, workdir, session, resume,
            containment, persona, role, role_hint, fork,
        ))
    }

    /// The infallible body of [`Self::build_agent_command_ex`], taking a fork
    /// request [`fork_line`] has already admitted. Split out so the wrappers
    /// that never fork ([`Self::build_agent_command`]) need no `unwrap` — a
    /// panic on a spawn path is a process abort (constraint 10).
    #[allow(clippy::too_many_arguments)]
    fn agent_command_line(
        &self,
        cli: &str,
        model: &str,
        knobs: workflow::ModelKnobs<'_>,
        auto_ops: bool,
        cfg: &Path,
        // Claude's `--settings` file (#417) — `write_hook_settings_file`'s
        // output, a SEPARATE file from `cfg` (rev-4 review N2: one file
        // serving both `--mcp-config` and `--settings` is a schema-drift
        // time bomb — see `write_mcp_config`'s doc). `None` omits
        // `--settings` entirely (a non-Claude CLI, or an agent with neither
        // hooks nor a permissions block — #610).
        hook_settings: Option<&Path>,
        group_dir: &Path,
        workdir: &Path,
        session: Option<&str>,
        resume: bool,
        containment: Containment,
        persona: &PersonaInject,
        role: Role,
        role_hint: Option<&str>,
        fork: Option<ForkLine<'_>>,
    ) -> String {
        // A planner never mutates and has no human in its pane, so there is
        // nothing for `auto_ops` to gate: it must explore, post its plan
        // comment, and report with zero prompts, or it would stall waiting on
        // an approval no one is there to give. So a planner (`ReadOnly`)
        // always runs unattended on BOTH CLIs; workers/reviewers follow the
        // group's `auto_ops` — including a reviewer, whose #462 deny flags
        // must never come bundled with a promotion to unattended. (This is
        // also why claude's `plan` permission mode / copilot's `--plan` can't
        // be used here — both hold the plan for interactive human sign-off.)
        let unattended = auto_ops || containment.forces_unattended();
        match cli {
            "copilot" => {
                // Copilot has `--resume` but no way to pre-assign an id, so an
                // id is discovered after boot (`spawn_copilot_session_watcher`)
                // rather than minted here the way claude's is.
                //
                // **The `=` is required, not cosmetic (#458, re-verified for
                // #781).** Per the CLI command reference (raw-fetched via
                // `curl -sL "https://docs.github.com/api/article/body?pathname=\
                // /en/copilot/reference/copilot-cli-reference/cli-command-reference"`,
                // per the `agent-cli-reference` skill's no-WebFetch rule, checked
                // 2026-08-03) the flag is optional-value — `-r`, `--resume[=VALUE]`
                // — and the page now says outright: "Bare `--resume` (no value)
                // shows an interactive session picker, which requires a TTY. If
                // multiple sessions exist and the picker can't be shown … the CLI
                // exits with an error instead of silently starting a new session —
                // pass an explicit `--resume=SESSION-ID` or use `--continue`."
                //
                // **What is documented, and what is not.** Documented: the
                // `[=VALUE]` notation, and the instruction to pass
                // `--resume=SESSION-ID` explicitly. NOT documented: whether the
                // space form actually mis-parses — the same page writes
                // `--resume <TASK-ID>` in prose (describing resuming a remote
                // task, not demonstrating the parse), so the two readings are not
                // settled by the docs, and CLAUDE.md constraint 3 rules out
                // spawning a real copilot to settle it. So this is "use the form
                // the reference tells you to use", not "fixed a confirmed
                // mis-parse" — the failure it would avoid (a TTY picker, or an
                // outright exit, in a pane loomux is about to type a kickoff
                // into) is severe enough that the free option wins without
                // needing the stronger claim.
                // #458 fixed this on the Sessions-tab command (`scan_copilot`) and
                // left the spawn path alone because nothing routed a copilot
                // orchestration session here; #781 does, so it moves too.
                //
                // NOT `--session-id <id>`, which is also documented and exact:
                // it "creates a new session when the value is a valid UUID" and
                // nothing matches — and copilot's ids ARE UUIDs, so a stale id
                // would silently open a blank session wearing the right name.
                // `--resume=` fails loudly on a missing id instead, which is the
                // behavior a rejoin wants.
                let resume_flag = match (session, resume) {
                    (Some(s), true) => format!("--resume={s} "),
                    _ => String::new(),
                };
                // NOTE: the @ (copilot's file-path marker) must sit INSIDE
                // the quotes — the pane shell is PowerShell, where a bare
                // `@"` opens a here-string and the whole line dies with a
                // ParserError before copilot ever runs.
                // --no-auto-update: a mid-boot self-update restarts the
                // CLI and flushes anything typed into the first instance.
                // --add-dir <workdir>: pre-trusts the agent's workspace so
                // panes don't stall on a folder-trust prompt.
                //
                // #802: the tool-permission flags are NOT built inline here any
                // more — they are collected and emitted ONCE at the end of this
                // arm, as a single comma-separated value each. See
                // `COPILOT_TOOL_LIST_SEP` for why a repeated `--allow-tool` was
                // a form the docs never described.
                let mut cmd = format!(
                    "copilot {resume_flag}--additional-mcp-config \"@{}\" --model {model} \
                     --add-dir \"{}\" --add-dir \"{}\" --no-auto-update",
                    cfg.display(),
                    group_dir.display(),
                    workdir.display()
                );
                if unattended {
                    // Group workers/planners get true autopilot mode: all tools +
                    // all paths pre-approved AND --autopilot (the autonomy
                    // system-prompt framing). The resulting "Enable autopilot
                    // mode" startup dialog is answered deterministically by the
                    // kickoff path (see COPILOT_GROUP_AUTOPILOT_FLAGS /
                    // confirm_copilot_autopilot_dialog) before the brief is
                    // pasted. A planner (ReadOnly) always takes this path even
                    // in a non-auto_ops group — interactive mode would stall it on
                    // a human that isn't there; the deny rules below keep it
                    // read-only, and deny takes precedence over --allow-all-tools
                    // in Copilot.
                    cmd.push(' ');
                    cmd.push_str(COPILOT_GROUP_AUTOPILOT_FLAGS);
                }
                // Copilot's native custom agent (#222). `--agent <name>` resolves
                // the name against `.github/agents/` — it CANNOT take an inline
                // definition, so this is set only when the block's `profile:`
                // points at a file the *user* authored there. loomux never writes
                // a generated persona into `.github/agents/` to make this flag
                // work: that would dirty the user's git tree with files they did
                // not write. A block with an inline `prompt:` instead reaches
                // Copilot through the kickoff prompt (`PersonaInject::kickoff`).
                if let Some(agent) = &persona.copilot_agent {
                    cmd.push_str(&format!(" --agent {agent}"));
                }
                // #802 — ONE `--allow-tool` and at most one `--deny-tool`, each
                // carrying a comma-separated list, per the documented value
                // form (`COPILOT_TOOL_LIST_SEP`). The whole value is quoted
                // because a `shell(git commit)` pattern contains a space, and
                // because the docs' own examples quote it.
                //
                // The deny value denies file writes even under
                // `--allow-all-tools` ("Deny rules always take precedence over
                // allow rules"). `gh` and the shell stay allowed — the planner
                // posts its plan comment, the reviewer (#462) runs the tests.
                // Literals live in COPILOT_EDIT_DENY_TOOLS /
                // COPILOT_READONLY_DENY_GIT (see the former for what's verified
                // vs. UNVERIFIED, #448).
                let (allow, deny) =
                    copilot_tool_permissions(unattended, containment, &persona.extra_allow);
                cmd.push_str(&format!(
                    " --allow-tool \"{}\"",
                    allow.join(COPILOT_TOOL_LIST_SEP)
                ));
                if !deny.is_empty() {
                    cmd.push_str(&format!(
                        " --deny-tool \"{}\"",
                        deny.join(COPILOT_TOOL_LIST_SEP)
                    ));
                }
                cmd
            }
            // Gemini (#267 stage 2) — the cross-model reviewer's CLI.
            //
            // The flags here are *only* the ones gemini takes on argv. Its two
            // loomux-critical seams are not argv at all:
            //
            // - **MCP.** Gemini declares MCP servers in `settings.json`
            //   (`mcpServers`) with no CLI-flag equivalent, so the loomux
            //   server rides the generated system-settings file
            //   `write_mcp_config` writes, delivered by the
            //   `GEMINI_CLI_SYSTEM_SETTINGS_PATH` environment variable set on
            //   the pane (`cli_extra_env`). `cfg` is that file — it is NOT
            //   named on this command line, and that is why
            //   [`CliCaps::mcp_argv_seam`] is false for gemini.
            // - **Containment.** Likewise a settings/policy-file concern (see
            //   `write_gemini_settings`), because the policy engine is the
            //   only surface that can deny a built-in tool by name;
            //   `--allowed-tools` is documented as deprecated ("Use Policy
            //   Engine instead") and is deliberately not used.
            //
            // So a reviewer's deny flags being *absent from this string* is
            // the design, not an omission — the assertion that they are
            // present lives on the generated files instead.
            //
            // Two persona surfaces are deliberately NOT wired, and both are
            // stated rather than silently dropped (see `docs/design/workflows.md`):
            //
            // - **No native custom-agent flag.** Gemini has no `--agent`
            //   equivalent, so a gemini block's persona reaches it through the
            //   kickoff prompt (`PersonaInject::kickoff`), exactly like an
            //   inline-`prompt:` copilot block.
            // - **`allow:` patterns do not apply.** `persona.extra_allow`
            //   holds Claude/Copilot *tool-pattern* strings (`Bash(make:*)`,
            //   `shell(npm:*)`); gemini's tool namespace and matcher are
            //   different, so translating them would be inventing semantics.
            //   Passing them through gemini's own `--allowed-tools` is not an
            //   option either — that flag is documented deprecated. A gemini
            //   block widens nothing; it only ever gets its class's baseline.
            "gemini" => {
                // Gemini mints its own session ids (`--list-sessions`) and has
                // no way to be handed one up front, so sessions aren't tracked
                // for it — same shape as copilot, and `--resume` only ever
                // appears on an explicit resume.
                let resume_flag = match (session, resume) {
                    (Some(s), true) => format!("--resume {s} "),
                    _ => String::new(),
                };
                let approval =
                    if unattended { GEMINI_UNATTENDED_FLAGS } else { GEMINI_ATTENDED_FLAGS };
                // `--include-directories` adds the group state dir to the
                // workspace (gemini's `--add-dir`); the agent's own workdir is
                // already the pane's cwd. `--allowed-mcp-server-names` is the
                // `--strict-mcp-config` analogue: whatever else the user's own
                // settings declare, this agent talks to loomux only.
                //
                // Both take arrays, so both are emitted with a single value
                // and are always followed by another flag or the end of the
                // line — never by a bare token that a greedy array parse could
                // swallow (#610's lesson, applied to a different CLI's
                // list-valued flags).
                format!(
                    "gemini {resume_flag}--model {model} {approval} \
                     --include-directories \"{}\" --allowed-mcp-server-names {MCP_SERVER}",
                    group_dir.display()
                )
            }
            // OpenCode (#722). The shortest arm of the four, and deliberately:
            // its MCP server, its containment and its persona DEFINITION are
            // all keys in a config document delivered by pane environment
            // (`write_mcp_config`'s opencode branch + `opencode_pane_env`) —
            // there is no `--mcp-config` analogue, no deny flag, and no
            // `--add-dir`. `cfg` is that document's audit copy and is
            // deliberately not named here, which is why
            // [`CliCaps::mcp_argv_seam`] is false for opencode too.
            //
            // So a contained pane's denials being ABSENT from this string is
            // the design, not an omission: they are asserted on the generated
            // document and on the environment instead.
            //
            // The TUI is the surface, not `opencode run`, and that is load-
            // bearing rather than incidental: `run` answers a permission it
            // cannot ask a human about by REJECTING it outright, so the
            // attended posture (`edit: ask`) would silently refuse every edit
            // instead of prompting. Anything that moves an opencode pane onto
            // `run` inherits that.
            "opencode" => {
                let mut cmd = String::from("opencode");
                // No flag pre-assigns a session id — `--session` continues an
                // existing one — so, like copilot and gemini, it appears only
                // on an explicit resume.
                //
                // #3318 F2: a FORK names the parent with the same `--session`
                // and adds the row's token right after it — "use with
                // `--continue` or `--session`" — so the fork line is the resume
                // line plus exactly that token. `session` is not read: opencode
                // cannot pre-mint the child, whose id the store watcher learns.
                match fork {
                    Some(f) => {
                        cmd.push_str(&format!(" --session {}", f.parent));
                        if let Some(token) = f.seam.token() {
                            cmd.push_str(&format!(" {token}"));
                        }
                    }
                    None => {
                        if let (Some(s), true) = (session, resume) {
                            cmd.push_str(&format!(" --session {s}"));
                        }
                    }
                }
                // Omitted entirely when empty: `default_model("opencode", …)`
                // is empty on purpose (see its doc), and a blank `--model`
                // would be an argument, not a silence.
                if !model.is_empty() {
                    cmd.push_str(&format!(" --model {model}"));
                }
                if let Some(agent) = &persona.opencode_agent {
                    cmd.push_str(&format!(" --agent {agent}"));
                }
                if unattended {
                    cmd.push(' ');
                    cmd.push_str(OPENCODE_UNATTENDED_FLAGS);
                }
                // `persona.extra_allow` holds claude/copilot tool-pattern
                // strings (`Bash(make:*)`, `shell(npm:*)`); opencode's
                // permission keys and matcher are a different namespace, so
                // translating them would be inventing semantics — the same
                // decision, for the same reason, as gemini's arm. An opencode
                // block widens nothing; it only ever gets its class's baseline.
                cmd
            }
            // pi (#2126). Everything loomux configures on pi rides argv — its
            // MCP config, its session identity, its session store, its
            // contract and its containment — which is what makes this the
            // longest of the non-claude arms and what makes its assertions
            // land here rather than on a generated document.
            "codex" => {
                // The whole line is four things, and three of them are one
                // thing: `-C` (where), `-p` (everything loomux configured),
                // an optional `-m`, and — on a resume — the `resume`
                // SUBCOMMAND. No `-s`, no `-a`, no `--full-auto` (which does
                // not exist at the pin; its only occurrence in the vendor is a
                // test, and the docs call it "a deprecated compatibility
                // alias"), and never the bypass flag. Posture rides the
                // profile, which is what `codex_launch_flags_per_posture`
                // pins: the attended and unattended LINES are byte-identical
                // and the two PROFILES are not.
                //
                // `-C` on BOTH directions, and on the resume it is doing real
                // work rather than being symmetric for its own sake. Resuming
                // a thread whose recorded cwd differs from the launch dir
                // makes the TUI PROMPT ("resume here or there?") when
                // `tui.resume_cwd` is unset, and a prompt on a pane loomux is
                // about to type into is a lost kickoff.
                //
                // Root options are inherited by the subcommand
                // (`SharedCliOptions::inherit_exec_root_options`), so writing
                // them before `resume` is correct and is the only order that
                // works — codex's usage is `codex [OPTIONS] <COMMAND> [ARGS]`.
                let mut cmd = String::from("codex");
                cmd.push_str(&format!(" -C \"{}\"", workdir.display()));
                // Unquoted: `codex_profile_name` refuses anything outside
                // codex's `[A-Za-z0-9_-]` alphabet rather than sanitizing it,
                // so there is nothing here a shell could split. `cfg` is the
                // profile FILE; `-p` wants its name, which is the file stem
                // minus `.config.toml`.
                if let Some(profile) = codex_profile_name_of_path(cfg) {
                    cmd.push_str(&format!(" -p {profile}"));
                }
                // Omitted entirely when empty, for the reason opencode's and
                // pi's arms give: `default_model("codex", …)` is empty on
                // purpose so the human's own `config.toml` model wins, and a
                // blank `-m` would be an argument, not a silence.
                if !model.is_empty() {
                    cmd.push_str(&format!(" -m {model}"));
                }
                // #687: codex has no effort FLAG — `model_reasoning_effort` is
                // a profile key, written by `write_codex_profile` from the
                // SAME `knobs` this function is handed. Read here and
                // deliberately not emitted, so the omission is a statement
                // rather than a gap; `a_codex_effort_knob_rides_the_profile_
                // and_never_the_line` pins both halves at once.
                let _ = knobs.effort;
                // Last, and a SUBCOMMAND rather than a flag — the only session
                // identity among loomux's adapters that is not a flag. `resume`
                // is read here, unlike pi's arm: codex has no
                // opens-or-creates flag, so a fresh spawn names nothing at all
                // and learns its id from the store afterwards
                // (`SessionBaseline::Codex`).
                //
                // #3318 F2: a FORK is the same slot with the row's word in
                // place of `resume` — `codex [OPTIONS] fork <SESSION_ID>` — so
                // everything before it (`-C`, `-p`, `-m`) is the resume line's,
                // byte for byte. The child is a new thread the store watcher
                // learns, exactly as a fresh spawn's is.
                match fork {
                    Some(f) => {
                        let word = f.seam.token().unwrap_or("fork");
                        cmd.push_str(&format!(" {word} {}", f.parent));
                    }
                    None => {
                        if let (Some(s), true) = (session, resume) {
                            cmd.push_str(&format!(" resume {s}"));
                        }
                    }
                }
                // `persona.extra_allow` holds claude/copilot tool-pattern
                // strings; codex has no allow mechanism a launch line can
                // reach (its rules engine loads only from `CODEX_HOME/rules`
                // and the project's `.codex/rules`, neither per-agent), so
                // there is nothing to translate them into and a codex block
                // widens nothing — the same decision as gemini's, opencode's
                // and pi's arms.
                //
                // `containment` is deliberately unread, and it is NOT asserted
                // either. codex tops out at `Containment::None`, so no contained
                // class can reach a real codex spawn — but that is `cli_can_host`
                // holding at parse and spawn time, not a property of this
                // function. These builders are pure and are driven directly over
                // EVERY tier by `string_and_argv_forms_agree_for_every_adapter`,
                // so a `debug_assert!` here would panic a debug test build on a
                // call that is entirely legitimate. Reading the parameter and
                // emitting nothing is the honest shape: there is no flag codex
                // could carry a denial on, whatever tier it is handed.
                let _ = containment;
                cmd
            }
            "pi" => {
                // ONE flag for both directions, and that is the whole reason
                // pi needs no session watcher, no baseline and no contest
                // refusal: `--session-id <id>` is documented "use exact
                // project session ID, creating it if missing", so the same
                // token opens an existing session and creates a missing one.
                // A resume of a pane that was never prompted therefore starts
                // a fresh session under the id it was always going to have,
                // rather than failing — pi does not create the session FILE
                // until the first assistant response.
                //
                // `resume` is deliberately not read. That is not a dropped
                // case: it is the fact, and `pi_launch_flags_per_posture`
                // pins the fresh and resumed lines EQUAL so a later edit
                // that splits them has to argue for it.
                let mut cmd = String::from("pi");
                if let Some(s) = session {
                    cmd.push_str(&format!(" --session-id {s}"));
                }
                // #3318 F2: a FORK keeps `--session-id`, which now names the
                // CHILD, and adds `--fork <parent>` beside it. The installed pi
                // (0.85.1) accepts exactly that pair and writes the child under
                // the given id (see pi's `CliCaps` row), so the fork line is the
                // fresh line for the child plus that one flag and its value.
                if let Some(f) = fork {
                    if let Some(token) = f.seam.token() {
                        cmd.push_str(&format!(" {token} {}", f.parent));
                    }
                }
                // The group's own store, so a group's sessions stay out of the
                // human's `pi --resume` list and theirs stay out of the
                // group's — `OPENCODE_DB`'s argument, reached by a flag.
                cmd.push_str(&format!(
                    " --session-dir \"{}\"",
                    pi_sessions_in(group_dir).display()
                ));
                // The MCP seam pi has only through the adapter extension the
                // human installs. pi's own parser files an unknown
                // `--flag value` into `unknownFlags` rather than erroring, so
                // this is inert — not fatal — on a machine where the adapter
                // is missing, and the pane then boots with no orrerix tools.
                // That failure is invisible to loomux and visible to the human
                // in `/mcp`; a launcher preflight for it is a follow-up.
                cmd.push_str(&format!(" --mcp-config \"{}\"", cfg.display()));
                if let Some(path) = &persona.pi_append_system_prompt_file {
                    // BY FILE, never as argv text: the flag takes "text or
                    // file contents", and a role contract is many KB against
                    // Windows CreateProcessW's 32,767-character command-line
                    // limit (#417). Only the path ever reaches argv.
                    cmd.push_str(&format!(" --append-system-prompt \"{}\"", path.display()));
                }
                // Exactly one of the pair, on EVERY group line, so pi's one
                // boot dialog ("Trust project folder?") can never appear on a
                // pane loomux is about to type a kickoff into. Which one is
                // the containment question — see the two constants.
                cmd.push(' ');
                cmd.push_str(if containment.denies_edits() {
                    PI_NO_APPROVE_FLAG
                } else {
                    PI_APPROVE_FLAG
                });
                if containment.denies_edits() {
                    // Applied AFTER every allowlist, so this is the denial and
                    // there is nothing for it to lose to. A ReadOnly class
                    // never reaches here — `cli_can_host` refuses a planner on
                    // pi outright, because pi has no bash-command deny for
                    // `Containment::denies_git_mutation` to be expressed as.
                    cmd.push_str(&format!(" --exclude-tools {PI_EDIT_DENY_TOOLS}"));
                }
                // Omitted entirely when empty, for the reason opencode's arm
                // gives: `default_model("pi", …)` is empty on purpose, and a
                // blank `--model` would be an argument, not a silence.
                if !model.is_empty() {
                    cmd.push_str(&format!(" --model {model}"));
                }
                // #687: a member of a closed enum by the time it reaches here
                // (parser + `clamped_knob`), so it needs no quoting — the same
                // argument `model` rests on. pi's own level vocabulary is a
                // superset of loomux's five.
                if !knobs.effort.is_empty() {
                    cmd.push_str(&format!(" --thinking {}", knobs.effort));
                }
                // `unattended` is READ and deliberately changes nothing — pi
                // has no permission prompts to bypass, so the attended and
                // unattended lines are byte-identical. See
                // `PI_UNATTENDED_FLAGS` for why that is a measured claim, and
                // `docs/orchestration.md` for what it means for a human
                // running an attended pi worker.
                debug_assert!(PI_UNATTENDED_FLAGS.is_empty());
                let _ = unattended;
                // `persona.extra_allow` holds claude/copilot tool-pattern
                // strings; pi has no allow mechanism at all (no permission
                // engine, no prompts), so there is nothing to translate them
                // into and a pi block widens nothing — the same decision, for
                // a stronger reason, as gemini's and opencode's arms.
                cmd
            }
            // "claude" and the explicit fallback for anything unrecognized.
            _ => {
                // Assigning the session id up front is what makes per-task
                // sessions resumable later: loomux never has to fish the id
                // out of the CLI.
                //
                // #3318 F1 adds a third shape. A FORK names the parent with
                // `--resume` and asks for a new id with `--fork-session`
                // (appended below, with the other trailing flags). Which id
                // the child ends up with is the one thing the vendor's docs do
                // not settle, so BOTH arms are built here and the choice is
                // made by `model::CLAUDE_FORK_PREMINTS_CHILD_ID` — READ below,
                // not inferred from what the caller passed:
                //
                //  - constant true — the PRE-MINT arm, and the default loomux
                //    ships: `--session-id <child> --resume <parent>
                //    --fork-session`, where `<child>` is the `session`
                //    argument. If claude honours the given id for the child, a
                //    fork keeps the exact-id property every claude pane
                //    already has and nothing has to be learned.
                //  - constant false — the LEARNED arm: `--resume <parent>
                //    --fork-session`, and the child's id is whatever claude
                //    mints. loomux cannot read it (claude takes no session
                //    baseline — `premints_session_id` is true for it), so such a
                //    pane is honestly unrecorded; `fork_agent` records no id
                //    for it rather than one the CLI is not running under.
                //
                // Live check L1 on #3318 is what decides between them, and it
                // is the human's to run: constraint 3 forbids loomux spawning
                // a real claude to find out.
                //
                // This arm serves claude AND every unrecognized CLI. An
                // unrecognized CLI can no longer reach here WITH a fork (#3331
                // item 2): `fork_line` refuses it before any line is built,
                // which is what closes rev-std r1's hazard — the fork's id shape
                // on a line whose fork flag the table withheld — at the source
                // rather than by gating it here.
                let session_flag = match fork {
                    // A FORK. Which of the two arms is built is decided HERE, by
                    // the row's `premints_child` — which on claude's row IS
                    // `CLAUDE_FORK_PREMINTS_CHILD_ID` — not by whether the caller
                    // happened to pass a child id. That distinction is the whole
                    // point: the constant is what the human flips after running
                    // live check L1, and a caller convention would leave that
                    // flip inert on this side while the frontend's mirror moved.
                    Some(f) => {
                        let child = if f.seam.premints_child() { session } else { None };
                        match child {
                            Some(child) => format!("--session-id {child} --resume {} ", f.parent),
                            None => format!("--resume {} ", f.parent),
                        }
                    }
                    // Not a fork: exactly the pre-#3318 line, byte for byte.
                    None => match (session, resume) {
                        (Some(s), true) => format!("--resume {s} "),
                        (Some(s), false) => format!("--session-id {s} "),
                        (None, _) => String::new(),
                    },
                };
                // "Auto" preset = Claude Code's native auto permission mode
                // (what the human uses interactively); otherwise acceptEdits.
                // A planner (`ReadOnly`) is always `unattended` (see above),
                // but runs under `dontAsk`, not Auto — see
                // `claude_effective_permission_mode`'s doc (#465).
                //
                // `is_read_only()`, NOT `denies_edits()`: #465's own doc says
                // never to hand this `true` for a non-read-only agent, and
                // names the reviewer as the case it must not cover. `dontAsk`
                // auto-denies anything outside `--allowedTools`, which is the
                // reviewer's whole shell — the tests it runs, the `gh` it
                // reviews through. A reviewer's containment is the deny list
                // alone; see `Containment::NoEdits` for what that leaves open,
                // including the fail-open direction #465 closes for a planner
                // and cannot close here.
                let perm = claude_effective_permission_mode(unattended, containment.is_read_only());
                let model_arg = claude_model_arg(model, knobs.context);
                let mut cmd = format!(
                    "claude {session_flag}--mcp-config \"{}\" --strict-mcp-config \
                     --model {model_arg} --permission-mode {perm} --add-dir \"{}\" --allowedTools {tools}",
                    cfg.display(),
                    group_dir.display(),
                    tools = brand::MCP_TOOL_PREFIX
                );
                if unattended {
                    // Pre-approve git + gh so the unattended flow runs without
                    // prompts (workers: branch→commit→PR; planners: read-only
                    // explore + `gh issue comment` for the plan). `Bash(git *)`
                    // matches every git subcommand; a planner's denials below
                    // carve commit/push back out.
                    cmd.push(' ');
                    cmd.push_str(CLAUDE_UNATTENDED_ALLOW);
                }
                // Persona `allow:` patterns extend the SAME `--allowedTools`
                // list, so they must land before `--disallowedTools` opens the
                // deny list below. For a NON-read-only block (worker/reviewer)
                // they can only widen within the capability class: on Claude,
                // `--disallowedTools` beats the allow list, so a worker persona
                // still cannot allow itself back into a git-mutation pattern
                // this same block denies. For a read-only block (planner),
                // `persona.extra_allow` is always EMPTY by the time it reaches
                // here — `persona_inject`'s capability closure (#222) drops
                // every `allow:` pattern for `Role::is_read_only()` from every
                // source (workflow-declared, `.github/agents` frontmatter, a
                // hand-edited `group.json`), unconditionally, with no opt-in.
                // This loop is not where a planner's read-only guarantee is
                // enforced; it is simply never handed anything to iterate.
                for pat in &persona.extra_allow {
                    cmd.push_str(&format!(" \"{pat}\""));
                }
                // #417: Claude Code's own ADDITIVE settings layer — never
                // clobbers the user's own `.claude/settings.json` (hooks, and
                // since #610 the read-only pane's `permissions.allow`).
                // Omitted entirely (not pointed at an empty file) when this
                // agent has nothing to put in one — see
                // `write_hook_settings_file`.
                //
                // **Emitted HERE, after the last `--allowedTools` value, not
                // before the first (#610).** A space-separated value list ends
                // at the next flag, so a `--settings` sitting mid-list demoted
                // every pattern after it to a stray positional argument: the
                // whole git/gh allowlist on every real spawn, invisible under
                // `auto`/`acceptEdits` and total under `dontAsk`. Any new flag
                // added to this branch belongs below this line too, never
                // between `--allowedTools` and its values.
                if let Some(hs) = hook_settings {
                    cmd.push_str(&format!(" --settings \"{}\"", hs.display()));
                }
                // #687: the block's thinking level, ONLY when one is set —
                // and emitted here, below `--settings`, for exactly the #610
                // reason stated above it: any flag added to this branch goes
                // after the last `--allowedTools` value, never between the
                // flag and its values. `knobs.effort` is a member of a closed
                // enum by the time it reaches here (parser + `clamped_knob`),
                // so it needs no quoting — the same argument `model` rests on.
                if !knobs.effort.is_empty() {
                    cmd.push_str(&format!(" --effort {}", knobs.effort));
                }
                // #3318 F1: the fork token, read from the capability table
                // rather than spelled here — `--fork-session` on claude's row.
                // Positioned with `--settings` and `--effort` above, for the
                // same #610 reason they are: any flag added to this branch goes
                // AFTER the last `--allowedTools` value, never between the flag
                // and its values.
                if let Some(token) = fork.and_then(|f| f.seam.token()) {
                    cmd.push(' ');
                    cmd.push_str(token);
                }
                if containment.denies_edits() {
                    // Deny the file-editing tools — and, for a ReadOnly class,
                    // the git mutation subcommands — outright
                    // (--disallowedTools overrides the permission mode AND the
                    // allow list in Claude Code), so a planner can't write code
                    // or commit/push and a reviewer (#462) can't edit. `gh`
                    // (incl. `gh issue comment` / `gh pr review`) stays
                    // reachable for the plan comment and the review.
                    //
                    // Spelling matters. `:*` is a valid wildcard only as a
                    // TRAILING suffix (`Bash(gh:*)` is fine); a colon in the
                    // MIDDLE of the command (`Bash(git commit:*)`) is not —
                    // Claude Code discards that rule as malformed AND prints a
                    // startup warning, the "auto deny rule" flash a human
                    // caught on planner boot. So the enforcing denial rests on
                    // the space form `Bash(git commit *)`: it is the canonical
                    // spelling and actually blocks commit/push, with no
                    // warning. (An earlier draft passed both spellings; the
                    // colon-mid one added nothing but the warning.)
                    // Literals live in CLAUDE_EDIT_DENY_TOOLS /
                    // CLAUDE_READONLY_DENY_GIT (see their docs for the
                    // drift-pin test, #448).
                    cmd.push_str(" --disallowedTools");
                    for t in CLAUDE_EDIT_DENY_TOOLS {
                        cmd.push_str(&format!(" {t}"));
                    }
                    // Nested, not a sibling `if`: the tiers are a ladder, so
                    // git denial can never appear without the edit denial that
                    // opened `--disallowedTools` above.
                    if containment.denies_git_mutation() {
                        for t in CLAUDE_READONLY_DENY_GIT {
                            cmd.push_str(&format!(" \"{t}\""));
                        }
                    }
                }
                // #946 Q4 / #1091 slice H: role-keyed, not containment-keyed
                // (see `claude_denies_interactive_question`'s doc) — so this
                // sits OUTSIDE the `containment.denies_edits()` block above,
                // not nested inside it the way the git-mutation denial is.
                // Two cases:
                // - The block above already opened `--disallowedTools`
                //   (a liaison-hinted reviewer, `Containment::NoEdits`) — EXTEND
                //   that SAME value list. Claude Code does not merge two
                //   `--disallowedTools` flags on one command line (the second
                //   occurrence would win, silently dropping the edit/git
                //   denial already emitted), so opening a second one here
                //   would be a regression, not an addition.
                // - Nothing opened it yet (the orchestrator, `Containment::None`
                //   — #465/#462 never gave it a tier) — open it fresh. This is
                //   the one case where a `--disallowedTools` flag appears on an
                //   orchestrator's command line at all; the #610/#417
                //   flag-severing pins (`claude_allow_patterns_are_not_severed_
                //   from_the_allowedtools_flag` and friends) are the regression
                //   net that would catch this landing in the wrong place.
                if claude_denies_interactive_question(role, role_hint) {
                    if !containment.denies_edits() {
                        cmd.push_str(" --disallowedTools");
                    }
                    for t in CLAUDE_QUESTION_DENY_TOOLS {
                        cmd.push_str(&format!(" {t}"));
                    }
                }
                // Claude's native custom agent, by FILE (round #417
                // correction 6, replacing the pre-round-6 inline `--agents
                // '<json>' --agent <id>` pair — see `PersonaInject::
                // claude_agent`'s doc: the inline JSON payload put the
                // whole role contract on argv, and Windows CreateProcessW's
                // hard 32,767-character command-line limit made that a
                // real, demo-blocking bug once the contract grew past a
                // short persona's size). `--agent <handle>` alone now
                // activates a loomux-generated `~/.claude/agents/<handle>.md`
                // file — the SAME "native custom-agent flag, file-backed"
                // shape Copilot's `--agent` already used, just newly true
                // for Claude too. Only a short handle ever reaches argv;
                // the contract itself never does, on either spawn path.
                if let Some(agent) = &persona.claude_agent {
                    cmd.push_str(&format!(" --agent {agent}"));
                } else if let Some(path) = &persona.claude_append_system_prompt_file {
                    // Fallback (round #417 correction 6): `~/.claude/agents`
                    // was unwritable — append the group's own instructions
                    // file to Claude's system prompt instead. Still a
                    // FILE, never argv, so the length bug can't resurface
                    // here either.
                    cmd.push_str(&format!(" --append-system-prompt-file \"{}\"", path.display()));
                }
                cmd
            }
        }
    }

    /// The **structured** form of [`build_agent_command`] — the same invocation
    /// as a program + literal-argument vector instead of a shell command line
    /// (issue #78). Direct-CLI pane spawn hands this to `spawn_pty` so the agent
    /// executable becomes the ConPTY child with no pwsh/sh wrapper; the string
    /// form is still emitted alongside it as the shell fallback (shim CLIs,
    /// unresolved programs, or the `LOOMUX_NO_DIRECT_SPAWN` escape hatch).
    ///
    /// Each element is a literal argv token: no surrounding shell quotes, spaces
    /// inside a token preserved (`Bash(git *)` is ONE element). Built from the
    /// same flag atoms as `build_agent_command`; a consistency test
    /// (`build_agent_argv_matches_command_line`) tokenizes the string form and
    /// asserts it equals this vector across the full matrix, so the two can't
    /// drift.
    #[allow(clippy::too_many_arguments)]
    #[doc(hidden)] // pub for integration tests
    pub fn build_agent_argv(
        &self,
        cli: &str,
        model: &str,
        auto_ops: bool,
        cfg: &Path,
        hook_settings: Option<&Path>,
        group_dir: &Path,
        workdir: &Path,
        session: Option<&str>,
        resume: bool,
        containment: Containment,
        persona: &PersonaInject,
    ) -> Vec<String> {
        // See `build_agent_command`'s doc: same inert sentinel
        // (`Role::Worker`, no hint) so this form's long-standing callers see
        // no change from the #946 Q4 / #1091 slice H predicate existing.
        // Never a fork, so the infallible body directly (`agent_argv`).
        self.agent_argv(
            cli,
            model,
            workflow::ModelKnobs::default(),
            auto_ops,
            cfg,
            hook_settings,
            group_dir,
            workdir,
            session,
            resume,
            containment,
            persona,
            Role::Worker,
            None,
            None,
        )
    }

    /// [`Self::build_agent_argv`] with the block's model knobs (#687) — the
    /// structured twin of [`Self::build_agent_command_ex`], and pinned equal to
    /// it (knobs included) by `build_agent_argv_matches_command_line`.
    ///
    /// `role`/`role_hint`: see [`Self::build_agent_command_ex`]'s doc — same
    /// meaning, same [`claude_denies_interactive_question`] predicate. So is
    /// `fork_of`, and so is its refusal: the same [`fork_line`] answers both
    /// forms, so they cannot disagree about which CLI may fork.
    #[allow(clippy::too_many_arguments)]
    #[doc(hidden)] // pub for integration tests
    pub fn build_agent_argv_ex(
        &self,
        cli: &str,
        model: &str,
        knobs: workflow::ModelKnobs<'_>,
        auto_ops: bool,
        cfg: &Path,
        hook_settings: Option<&Path>,
        group_dir: &Path,
        workdir: &Path,
        session: Option<&str>,
        resume: bool,
        containment: Containment,
        persona: &PersonaInject,
        role: Role,
        role_hint: Option<&str>,
        fork_of: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let fork = fork_line(cli, fork_of)?;
        Ok(self.agent_argv(
            cli, model, knobs, auto_ops, cfg, hook_settings, group_dir, workdir, session, resume,
            containment, persona, role, role_hint, fork,
        ))
    }

    /// The infallible body of [`Self::build_agent_argv_ex`] — the twin of
    /// [`Self::agent_command_line`], split out for the same reason.
    #[allow(clippy::too_many_arguments)]
    fn agent_argv(
        &self,
        cli: &str,
        model: &str,
        knobs: workflow::ModelKnobs<'_>,
        auto_ops: bool,
        cfg: &Path,
        hook_settings: Option<&Path>,
        group_dir: &Path,
        workdir: &Path,
        session: Option<&str>,
        resume: bool,
        containment: Containment,
        persona: &PersonaInject,
        role: Role,
        role_hint: Option<&str>,
        fork: Option<ForkLine<'_>>,
    ) -> Vec<String> {
        let unattended = auto_ops || containment.forces_unattended();
        let mut a: Vec<String> = Vec::new();
        let push = |a: &mut Vec<String>, s: &str| a.push(s.to_string());
        match cli {
            "copilot" => {
                push(&mut a, "copilot");
                if let (Some(s), true) = (session, resume) {
                    // ONE argv element, `=`-joined — the form the reference
                    // tells you to pass (see the string builder's comment).
                    //
                    // Kept in step with the string form rather than argued
                    // separately, because what an optional-value flag does with
                    // a SEPARATE argv element is exactly what the docs do not
                    // say. The reasoning that it would be read as a positional
                    // is an inference from the `[=VALUE]` notation, and the same
                    // page writes `--resume <TASK-ID>` in prose elsewhere, so
                    // the inference is not even unopposed. Following the
                    // documented instruction costs nothing and needs no theory
                    // of the parser; the theory is what would need proving.
                    a.push(format!("--resume={s}"));
                }
                push(&mut a, "--additional-mcp-config");
                // The @ marker rides on the path as a single argv element (no
                // shell here-string hazard once it's not a shell string at all).
                a.push(format!("@{}", cfg.display()));
                push(&mut a, "--model");
                push(&mut a, model);
                push(&mut a, "--add-dir");
                a.push(group_dir.display().to_string());
                push(&mut a, "--add-dir");
                a.push(workdir.display().to_string());
                push(&mut a, "--no-auto-update");
                if unattended {
                    // Reuse the atom directly: no quotes/embedded spaces, so the
                    // whitespace split yields exactly the argv tokens.
                    for t in COPILOT_GROUP_AUTOPILOT_FLAGS.split_whitespace() {
                        push(&mut a, t);
                    }
                }
                if let Some(agent) = &persona.copilot_agent {
                    push(&mut a, "--agent");
                    push(&mut a, agent);
                }
                // #802 — one occurrence each, same order and same atoms as the
                // string form (which quotes the joined value; there is no shell
                // here, so it is one literal argv element instead).
                let (allow, deny) =
                    copilot_tool_permissions(unattended, containment, &persona.extra_allow);
                push(&mut a, "--allow-tool");
                a.push(allow.join(COPILOT_TOOL_LIST_SEP));
                if !deny.is_empty() {
                    push(&mut a, "--deny-tool");
                    a.push(deny.join(COPILOT_TOOL_LIST_SEP));
                }
            }
            // Gemini (#267) — see the string form for why this arm is short:
            // its MCP and containment seams are generated files, not flags.
            "gemini" => {
                push(&mut a, "gemini");
                if let (Some(s), true) = (session, resume) {
                    push(&mut a, "--resume");
                    push(&mut a, s);
                }
                push(&mut a, "--model");
                push(&mut a, model);
                // == GEMINI_UNATTENDED_FLAGS / GEMINI_ATTENDED_FLAGS, as
                // literal tokens: no quotes or embedded spaces inside a value,
                // so the whitespace split yields exactly these argv elements.
                for t in
                    if unattended { GEMINI_UNATTENDED_FLAGS } else { GEMINI_ATTENDED_FLAGS }
                        .split_whitespace()
                {
                    push(&mut a, t);
                }
                push(&mut a, "--include-directories");
                a.push(group_dir.display().to_string());
                push(&mut a, "--allowed-mcp-server-names");
                push(&mut a, MCP_SERVER);
            }
            // OpenCode (#722) — see the string form for why this arm is short:
            // its MCP, containment and persona-definition seams are one
            // env-delivered document, not flags.
            "opencode" => {
                push(&mut a, "opencode");
                // #3318 F2 — same shape as the string form; see it.
                match fork {
                    Some(f) => {
                        push(&mut a, "--session");
                        push(&mut a, f.parent);
                        if let Some(token) = f.seam.token() {
                            push(&mut a, token);
                        }
                    }
                    None => {
                        if let (Some(s), true) = (session, resume) {
                            push(&mut a, "--session");
                            push(&mut a, s);
                        }
                    }
                }
                if !model.is_empty() {
                    push(&mut a, "--model");
                    push(&mut a, model);
                }
                if let Some(agent) = &persona.opencode_agent {
                    push(&mut a, "--agent");
                    push(&mut a, agent);
                }
                if unattended {
                    // == OPENCODE_UNATTENDED_FLAGS, as a literal token.
                    for t in OPENCODE_UNATTENDED_FLAGS.split_whitespace() {
                        push(&mut a, t);
                    }
                }
            }
            // pi (#2126) — the same atoms and the same order as the string
            // form above; see it for why each flag is there. Every path is one
            // literal argv element (no shell here, so no quotes), and
            // `build_agent_argv_matches_command_line` tokenizes the string
            // form and asserts it equals this vector across the matrix.
            "codex" => {
                push(&mut a, "codex");
                push(&mut a, "-C");
                a.push(workdir.display().to_string());
                if let Some(profile) = codex_profile_name_of_path(cfg) {
                    push(&mut a, "-p");
                    push(&mut a, profile);
                }
                if !model.is_empty() {
                    push(&mut a, "-m");
                    push(&mut a, model);
                }
                // == the string form's silence: the effort knob is a profile
                // key on codex, not a flag. Stated so the two forms make the
                // same claim about it rather than one simply omitting it.
                let _ = knobs.effort;
                // The subcommand goes LAST, after every root option, because
                // codex's usage is `codex [OPTIONS] <COMMAND> [ARGS]` and the
                // subcommand inherits what precedes it. A fork takes the same
                // slot with the row's word (#3318 F2) — see the string form.
                match fork {
                    Some(f) => {
                        push(&mut a, f.seam.token().unwrap_or("fork"));
                        push(&mut a, f.parent);
                    }
                    None => {
                        if let (Some(s), true) = (session, resume) {
                            push(&mut a, "resume");
                            push(&mut a, s);
                        }
                    }
                }
                // Read and deliberately not asserted — see the string form's
                // arm for why a `debug_assert!` here would panic a debug test
                // build on a legitimate call.
                let _ = containment;
            }
            "pi" => {
                push(&mut a, "pi");
                // `resume` unread on purpose — `--session-id` opens-or-creates.
                if let Some(s) = session {
                    push(&mut a, "--session-id");
                    push(&mut a, s);
                }
                // #3318 F2 — `--fork <parent>` beside the child's id; see the
                // string form.
                if let Some(f) = fork {
                    if let Some(token) = f.seam.token() {
                        push(&mut a, token);
                        push(&mut a, f.parent);
                    }
                }
                push(&mut a, "--session-dir");
                a.push(pi_sessions_in(group_dir).display().to_string());
                push(&mut a, "--mcp-config");
                a.push(cfg.display().to_string());
                if let Some(path) = &persona.pi_append_system_prompt_file {
                    push(&mut a, "--append-system-prompt");
                    a.push(path.display().to_string());
                }
                push(
                    &mut a,
                    if containment.denies_edits() { PI_NO_APPROVE_FLAG } else { PI_APPROVE_FLAG },
                );
                if containment.denies_edits() {
                    push(&mut a, "--exclude-tools");
                    // ONE element: the value is a comma-joined list with no
                    // spaces, so it is a single token on both forms.
                    push(&mut a, PI_EDIT_DENY_TOOLS);
                }
                if !model.is_empty() {
                    push(&mut a, "--model");
                    push(&mut a, model);
                }
                if !knobs.effort.is_empty() {
                    push(&mut a, "--thinking");
                    push(&mut a, knobs.effort);
                }
                // == PI_UNATTENDED_FLAGS (empty), stated so the two forms make
                // the same claim about the posture rather than one of them
                // simply omitting it.
                debug_assert!(PI_UNATTENDED_FLAGS.is_empty());
            }
            // "claude" and the explicit fallback for anything unrecognized.
            _ => {
                push(&mut a, "claude");
                // #3318 F1 — the same three shapes as the string form, in the
                // same order; see that arm's comment for the pre-mint/learned
                // split and live check L1.
                // An unrecognized CLI never reaches here with a fork —
                // `fork_line` refused it (#3331 item 2). See the string form.
                match fork {
                    // Same row field, same decision, same reason as the string
                    // form — see its comment. Read here too rather than passed
                    // in, so the two forms cannot disagree about which arm a
                    // flip of the constant selects.
                    Some(f) => {
                        let child = if f.seam.premints_child() { session } else { None };
                        if let Some(child) = child {
                            push(&mut a, "--session-id");
                            push(&mut a, child);
                        }
                        push(&mut a, "--resume");
                        push(&mut a, f.parent);
                    }
                    None => match (session, resume) {
                        (Some(s), true) => {
                            push(&mut a, "--resume");
                            push(&mut a, s);
                        }
                        (Some(s), false) => {
                            push(&mut a, "--session-id");
                            push(&mut a, s);
                        }
                        (None, _) => {}
                    },
                }
                push(&mut a, "--mcp-config");
                a.push(cfg.display().to_string());
                push(&mut a, "--strict-mcp-config");
                push(&mut a, "--model");
                // The [1m] suffix as ONE literal token — no shell here, so no
                // quotes (the string form quotes it because `[1m]` is a glob
                // pattern to a POSIX shell).
                a.push(claude_model_token(model, knobs.context));
                push(&mut a, "--permission-mode");
                // Same tier predicate as the string form (#465 + #462) — see
                // that call for why it is is_read_only(), not denies_edits().
                push(&mut a, claude_effective_permission_mode(unattended, containment.is_read_only()));
                push(&mut a, "--add-dir");
                a.push(group_dir.display().to_string());
                push(&mut a, "--allowedTools");
                push(&mut a, brand::MCP_TOOL_PREFIX);
                if unattended {
                    // == CLAUDE_UNATTENDED_ALLOW, as literal (unquoted) tokens.
                    push(&mut a, "Bash(git *)");
                    push(&mut a, "Bash(gh *)");
                }
                // Still inside --allowedTools' value list — before the deny list.
                for pat in &persona.extra_allow {
                    push(&mut a, pat);
                }
                // #417: a SEPARATE file from --mcp-config's (rev-4 review
                // N2) — omitted entirely when this agent has nothing to put in
                // one. After the allow values, never before them: see the
                // string form's comment for the #610 defect that ordering fixes.
                if let Some(hs) = hook_settings {
                    push(&mut a, "--settings");
                    a.push(hs.display().to_string());
                }
                // #687 — same position as the string form, for the same reason.
                if !knobs.effort.is_empty() {
                    push(&mut a, "--effort");
                    push(&mut a, knobs.effort);
                }
                // #3318 F1 — same position and same table read as the string
                // form; `build_agent_argv_matches_command_line` is what keeps
                // the two from drifting.
                if let Some(token) = fork.and_then(|f| f.seam.token()) {
                    push(&mut a, token);
                }
                if containment.denies_edits() {
                    push(&mut a, "--disallowedTools");
                    for t in CLAUDE_EDIT_DENY_TOOLS {
                        push(&mut a, t);
                    }
                    // Nested for the same reason as in `build_agent_command`:
                    // the tiers are a ladder (see `Containment`).
                    if containment.denies_git_mutation() {
                        for t in CLAUDE_READONLY_DENY_GIT {
                            push(&mut a, t);
                        }
                    }
                }
                // #946 Q4 / #1091 slice H — same predicate and same
                // extend-vs-open choice as the string form; see that arm's
                // comment for why this must never open a SECOND
                // `--disallowedTools`.
                if claude_denies_interactive_question(role, role_hint) {
                    if !containment.denies_edits() {
                        push(&mut a, "--disallowedTools");
                    }
                    for t in CLAUDE_QUESTION_DENY_TOOLS {
                        push(&mut a, t);
                    }
                }
                if let Some(agent) = &persona.claude_agent {
                    push(&mut a, "--agent");
                    push(&mut a, agent);
                } else if let Some(path) = &persona.claude_append_system_prompt_file {
                    push(&mut a, "--append-system-prompt-file");
                    a.push(path.display().to_string());
                }
            }
        }
        a
    }
}
