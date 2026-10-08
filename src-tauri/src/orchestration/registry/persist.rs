//! Durable registry state: the registry's wiring and path accessors
//! (`group_dir`, `next_group_id`, `ledger_path`, `attachments_dir`), the
//! group's `state.json`, the session digest, the durable agent-id high-water
//! mark, the durable roster and session-id learning (`persist_agent_record`,
//! `group_records`, `associate_session`, `session_*`), loading
//! `group.json` (`load_group_file`) and the on-disk group registry
//! (`existing_group_ids`), as an `impl OrchRegistry` block (#3498). The
//! designs are `docs/design/orchestration.md`,
//! `docs/design/session-id-learning.md` and
//! `docs/design/groupid-and-path-roots.md`.

use super::*;

impl OrchRegistry {
    /// Record the `Arc` the registry is stored behind so `&self` methods can
    /// spawn background work that outlives the current call. Call once, right
    /// after wrapping the registry in an `Arc`.
    pub fn set_self_arc(self: &Arc<Self>) {
        *self.self_arc.lock_safe() = Arc::downgrade(self);
    }

    /// Upgrade the stored weak self-handle. `None` in unit tests that build a
    /// bare registry without calling `set_self_arc` — background helpers then
    /// simply don't run.
    pub(in crate::orchestration) fn arc(&self) -> Option<Arc<Self>> {
        self.self_arc.lock_safe().upgrade()
    }

    /// Default persistent root: `<data root>/orchestration` (see `obs::data_root`).
    pub fn default_root() -> PathBuf {
        crate::obs::data_root().join("orchestration")
    }

    pub fn set_app(&self, app: AppHandle) {
        *self.app.lock_safe() = Some(app);
    }

    /// The process's declared-root registry (#1042). `lib.rs` `manage`s this
    /// `Arc` so commands reach the same instance this registry populates; the
    /// integration tests call it to assert what a group create declared.
    pub fn roots(&self) -> Arc<RootRegistry> {
        Arc::clone(&self.roots)
    }

    pub fn set_port(&self, port: u16) {
        self.port.store(port, Ordering::SeqCst);
    }

    pub fn port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }

    /// **The one place a group-scoped path is assembled** (#904). Every
    /// group-scoped path in the process descends from this join.
    ///
    /// It takes a [`GroupId`], not a `&str`, and that is the whole point:
    /// CLAUDE.md hard constraint 6 used to say this join *trusts* its caller,
    /// which was never a claim about the id but about the transport — only our
    /// own in-process webview can invoke a `#[tauri::command]`. That is a fact
    /// #888's remote engine dissolves. The proof now travels with the value, so
    /// the join cannot be reached with anything unvalidated, from any caller,
    /// over any transport.
    ///
    /// `GroupId` deliberately does not implement `AsRef<Path>`, so this is not
    /// merely the conventional place to build a group path — it is the only
    /// expressible one.
    pub(in crate::orchestration) fn group_dir(&self, group: &GroupId) -> PathBuf {
        group_dir_at(&self.root, group)
    }

    /// The group id a launch on `repo` gets: repo-derived so a relaunch resumes
    /// the same state directory, plus a `-{n}` tail because one repo can host
    /// several *concurrent* orchestrations and those must never share a group
    /// (their orchestrators would receive each other's worker reports). Takes
    /// the first candidate with no live agents.
    ///
    /// #904 folded the two byte-identical copies of this scan
    /// (`create_group_ex` and `promote_orchestrator_cli`) into one definition
    /// while giving it a validated return type — the same argument
    /// `opencode_db_path`'s doc makes: two call sites deriving one identity
    /// independently is a disagreement waiting to happen, and this one decides
    /// which directory a group's whole state lives in.
    ///
    /// `None` is unreachable in practice — `group_id_for_repo`'s output is
    /// pinned parseable by `the_minter_can_never_produce_an_id_its_own_validator_rejects`,
    /// and a `-{n}` tail preserves the alphabet — but it is returned rather
    /// than `expect`ed, because both callers already have an error path and a
    /// panic here would take down a launch.
    pub(in crate::orchestration) fn next_group_id(&self, repo: &str) -> Option<GroupId> {
        let base = group_id_for_repo(repo);
        let id = (1..)
            .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
            // #3679: and not a group whose QUICK run a human can still resume
            // or stop. Such a run may have no live pane at all — it is parked,
            // or its first pane has not opened yet — and liveness is the only
            // thing this used to read, so the next launch on the repo would
            // have been handed its group and overwritten its record.
            .find(|candidate| {
                !self.group_is_live(candidate)
                    && !GroupId::parse(candidate).is_ok_and(|id| self.qd_holds_group(&id))
            })?;
        GroupId::parse(&id).ok()
    }

    /// This group's OpenCode session store — the file `OPENCODE_DB` points
    /// every opencode pane in the group at (#722).
    ///
    /// One function rather than two `join`s at the two call sites, because the
    /// spawn that CREATES this path and the usage read that CONSUMES it
    /// disagreeing would not fail loudly: the reader would simply find no
    /// database and report an agent as having spent nothing.
    #[doc(hidden)] // pub for integration tests
    pub fn opencode_db_path(&self, group: &GroupId) -> PathBuf {
        self.group_dir(group).join(OPENCODE_DB_SUBDIR).join(OPENCODE_DB_FILE)
    }

    /// This group's pi session store — the directory `--session-dir` points
    /// every pi pane in the group at (#2126).
    ///
    /// One function rather than a `join` at each call site, for the reason
    /// [`Self::opencode_db_path`]'s doc gives: the spawn that CREATES this
    /// path and the reads that CONSUME it disagreeing would not fail loudly —
    /// the reader would simply find no session and report the pane as
    /// unresumable.
    ///
    /// **A directory, not a file, and its layout is pi's rather than
    /// loomux's.** With `--session-dir <dir>` pi writes the session file
    /// DIRECTLY under `<dir>` — `SessionManager.create` uses the given
    /// directory with no per-cwd subdirectory, unlike the default store, whose
    /// `--<cwd with every separator and colon replaced by ->--` segment is
    /// what makes the default layout cwd-keyed. So one flat directory per
    /// group holds `<timestamp>_<uuid>.jsonl` for every pane in it, and a
    /// lookup by id is an exact `_<id>.jsonl` filename-suffix match.
    /// Delegates to [`pi_sessions_in`] rather than joining, because the launch
    /// line is built from a `group_dir: &Path` that has no [`GroupId`] in
    /// scope — so the two would otherwise be two independent spellings of one
    /// directory, which is precisely what this function exists to prevent.
    #[doc(hidden)] // pub for integration tests
    pub fn pi_sessions_dir(&self, group: &GroupId) -> PathBuf {
        pi_sessions_in(&self.group_dir(group))
    }

    /// Scratch dir holding images pasted/attached into the steering strip (#72).
    /// A subdir of the group state dir, so it's naturally per-group and swept
    /// on group end alongside the worktrees.
    pub(in crate::orchestration) fn attachments_dir(&self, group: &GroupId) -> PathBuf {
        self.group_dir(group).join("attachments")
    }

    /// Directive ledger (#329 expansion): one pane's ledger file, directly in
    /// the group dir alongside `audit.jsonl` — human-inspectable the same
    /// way, per the #240 precedent. Keyed by agent id, not by role/block
    /// (unlike the instructions files): a pane's own directives are its own
    /// regardless of which block it's running, and a resume should still see
    /// them. See `note_directive`, the sole writer.
    /// Takes a validated agent id (#925) — see `promptsubmit_marker_path` for
    /// the argument; this is the same family and the same reasoning.
    pub(in crate::orchestration) fn ledger_path(&self, group: &GroupId, agent_id: &PathSegment) -> PathBuf {
        self.group_dir(group).join(format!("ledger-{agent_id}.log"))
    }

    /// The CLI the group's orchestrator runs (`claude`/`copilot`/…) — a CLASS
    /// question with no agent in hand, so `cli_for` is the right resolver here
    /// and this is the one call site #2167 deliberately left alone. (A group has
    /// exactly one orchestrator, so its class default cannot diverge from the
    /// pane the way a two-block worker class can.) Used to format image references the
    /// way that CLI consumes them (#72). Falls back to the default `claude`
    /// wording if the group isn't loaded (a save always follows a live steer, so
    /// this is just a safety net).
    pub fn orchestrator_cli(&self, group: &GroupId) -> String {
        self.group(group)
            .map(|g| g.guardrails.cli_for(Role::Orchestrator).to_string())
            .unwrap_or_else(|| "claude".into())
    }

    /// Persist a steered image to the group's `attachments/` scratch dir and
    /// return its absolute path (#72). The steering strip can't hand binary to
    /// a CLI prompt, but Claude Code and Copilot both *read image files from
    /// paths* — so a pasted screenshot is written here and the steer text gains
    /// an "Attached image: <path>" line pointing at it. Bytes are written
    /// verbatim: the image arrives as a browser Blob and we never decode it
    /// (no image crate, no `getrandom` deps) — only size and extension are
    /// validated. Files are reclaimed when the group ends (see `end_group`).
    pub fn save_attachment(&self, group: &GroupId, ext: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        // Membership guard: only ever write under a real, known group id (#72
        // review). This is NOT the traversal check any more — #904 made that the
        // `GroupId` parameter's job, and a traversal id is no longer expressible
        // here. What survives is the other half, and it is the half a type
        // cannot state: a well-formed id is not the same as a group that EXISTS,
        // and this writes bytes to disk. Validity is not membership.
        if self.group(group).is_none() {
            return Err("unknown group".into());
        }
        if bytes.is_empty() {
            return Err("empty attachment".into());
        }
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(format!(
                "attachment too large ({} bytes, max {MAX_ATTACHMENT_BYTES})",
                bytes.len()
            ));
        }
        let ext = sanitize_attachment_ext(ext)
            .ok_or_else(|| format!("unsupported attachment type: {ext:?}"))?;
        let dir = self.attachments_dir(group);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        // `<ms>-<seq>.<ext>`: wall-clock time keeps names sortable/legible while
        // the process-local sequence disambiguates a same-millisecond burst.
        let name = format!("{}-{}.{ext}", now_ms(), ATTACH_SEQ.fetch_add(1, Ordering::Relaxed));
        let path = dir.join(name);
        fs::write(&path, bytes).map_err(|e| e.to_string())?;
        self.audit(group, "human", "attachment-save",
            json!({ "path": path.display().to_string(), "bytes": bytes.len() }));
        Ok(path)
    }

    // ---------- durable state ----------

    pub fn get_state(&self, group: &GroupId) -> String {
        fs::read_to_string(self.group_dir(group).join("state.json"))
            .unwrap_or_else(|_| "{}".to_string())
    }

    pub fn set_state(&self, group: &GroupId, state: &str) -> Result<(), String> {
        if state.len() > MAX_STATE_BYTES {
            return Err(format!("state too large ({} bytes, max {MAX_STATE_BYTES})", state.len()));
        }
        serde_json::from_str::<Value>(state).map_err(|e| format!("state must be valid JSON: {e}"))?;
        let dir = self.group_dir(group);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        // Atomic replace so a failed write (disk-full, crash) leaves the last
        // good snapshot intact (#133). set_state holds no lock — the unique
        // temp name in `atomic_write` keeps concurrent writers from clobbering
        // one another's scratch file, and the rename makes it last-writer-wins
        // rather than a torn file.
        atomic_write(&dir.join("state.json"), state.as_bytes()).map_err(|e| e.to_string())?;
        self.audit(group, brand::AUDIT_ACTOR, "state-write", json!({ "bytes": state.len() }));
        Ok(())
    }

    // ---------- session digest (#250/#324 slice B) ----------

    /// Resolve a session's normalized transcript and reduce it to friction
    /// windows + anchors — the shared backend behind the `session_digest` MCP
    /// tool. Looks up the caller's group only (`merged_records`/`get_task`/
    /// `tasks` are all group-scoped already), so a task/agent/pr id from
    /// another group simply isn't found — same "unknown X" shape
    /// `require_in_group` uses, no separate cross-group check needed.
    ///
    /// `merged_records` (not `agent()`) is used deliberately: this tool's
    /// whole point is reading a session *after* it finished, when the worker
    /// is very often already dead/reaped — a live-only lookup would return
    /// "unknown agent" for the exact case the process-pro persona spawns
    /// into (#324: "spawned at merge-gate resolution").
    pub fn session_digest(&self, group: &GroupId, lookup: DigestLookup) -> Result<digest::SessionDigest, String> {
        let (session_id, cli, final_diff_ref, outcome, title) = match lookup {
            DigestLookup::Agent(agent_id) => {
                let rec = self
                    .merged_records(group)
                    .into_iter()
                    .filter(|r| r.id == agent_id)
                    .max_by_key(|r| r.updated_ms)
                    .ok_or_else(|| format!("unknown agent: {agent_id}"))?;
                let session_id = rec
                    .session
                    .clone()
                    .ok_or_else(|| format!("agent {agent_id} has no recorded session transcript"))?;
                let cli = self.cli_for_record(group, &rec);
                (session_id, cli, rec.branch.clone(), None, Some(rec.task.clone()))
            }
            DigestLookup::Task(task_id) => {
                let task = self.get_task(group, &task_id).ok_or_else(|| format!("unknown task: {task_id}"))?;
                let session_id = task
                    .session
                    .clone()
                    .ok_or_else(|| format!("task {task_id} has no recorded session"))?;
                let cli = task
                    .assignee
                    .as_deref()
                    .and_then(|aid| {
                        self.merged_records(group).into_iter().filter(|r| r.id == aid).max_by_key(|r| r.updated_ms)
                    })
                    .map(|rec| self.cli_for_record(group, &rec))
                    .unwrap_or_else(|| "claude".to_string());
                (session_id, cli, task.pr.clone(), Some(task.status.clone()), Some(task.title.clone()))
            }
            DigestLookup::Pr(pr) => {
                let want = pr_number(&pr).ok_or_else(|| format!("no PR number found in {pr:?}"))?;
                let task = self
                    .tasks(group)
                    .into_iter()
                    .find(|t| t.pr.as_deref().and_then(pr_number) == Some(want))
                    .ok_or_else(|| format!("no task found for PR {pr}"))?;
                return self.session_digest(group, DigestLookup::Task(task.id));
            }
        };
        let events = self.read_session_transcript_events(group, &cli, &session_id)?;
        let mut digest = digest::build_digest(&events, final_diff_ref, outcome, title);
        // The #324 recurrence pass. Everything above this line describes ONE
        // session, which is exactly as much as the process-pro's durability
        // filter ("would a fresh worker on a different task in this repo hit
        // the same wall?") cannot be answered from. This reads the group's
        // other recorded sessions and answers it mechanically.
        let (others, capped) = self.corroborating_session_keys(group, &session_id);
        digest.sessions_scanned = others.len();
        digest.corroboration_capped = capped;
        digest::apply_recurrence(&mut digest, &others);
        Ok(digest)
    }

    /// How many OTHER sessions one `session_digest` call reads to compute
    /// recurrence. The cost of this feature is linear in it — each one is a
    /// transcript read plus a friction-extraction pass — and it is charged
    /// per call, because nothing is cached (see
    /// `docs/design/supervisor-skills.md`, "Recurrence is derived on read").
    /// Small on purpose: recurrence is a yes/no-ish signal ("did anyone else
    /// hit this?"), and the difference between scanning 8 sessions and 40 is
    /// a much larger bill for a marginally better answer.
    const MAX_CORROBORATION_SESSIONS: usize = 8;

    /// Read the group's other recorded sessions and return each one's deduped
    /// friction keys, plus whether the scan was capped (#324).
    ///
    /// Newest first and one entry per SESSION id: an agent that rejoined its
    /// own session appears once, so a resumed pane cannot corroborate itself.
    /// The target session is excluded for the same reason — a session is not
    /// evidence about itself, which is the whole cold-read premise this
    /// feature inherits from #324's issue body.
    ///
    /// A session whose transcript can't be read (reaped, a CLI whose
    /// transcripts loomux can't parse, a `Task.session` pointing at nothing)
    /// is skipped, not fatal: corroboration is a bonus signal on top of a
    /// digest that must still be returned. `sessions_scanned` on the digest
    /// reports how many were ACTUALLY read, so a skip is visible as a smaller
    /// denominator rather than silently inflating confidence.
    fn corroborating_session_keys(&self, group: &GroupId, target_session: &str) -> (Vec<(String, Vec<String>)>, bool) {
        let mut recs = self.merged_records(group);
        recs.sort_by_key(|r| std::cmp::Reverse(r.updated_ms));
        let mut seen: HashSet<String> = HashSet::new();
        let mut candidates: Vec<(String, String, String)> = Vec::new();
        for r in &recs {
            let Some(sid) = r.session.clone() else { continue };
            if sid == target_session || !seen.insert(sid.clone()) {
                continue;
            }
            candidates.push((r.id.clone(), self.cli_for_record(group, r), sid));
        }
        let capped = candidates.len() > Self::MAX_CORROBORATION_SESSIONS;
        candidates.truncate(Self::MAX_CORROBORATION_SESSIONS);
        let out = candidates
            .into_iter()
            .filter_map(|(label, cli, sid)| {
                let events = self.read_session_transcript_events(group, &cli, &sid).ok()?;
                Some((label, digest::session_friction_keys(&events)))
            })
            .collect::<Vec<_>>();
        (out, capped)
    }

    /// The CLI a roster row's block/role resolves to, dead-agent-safe (unlike
    /// `cli_for_agent`, which needs a live `AgentEntry`). Mirrors its
    /// resolution — the row's OWN block first (#2167) — and its fallback:
    /// unresolvable role/group both fall back to `"claude"`. A row persisted
    /// before #222 carries an empty `block`, which `cli_for_block` resolves to
    /// the class default, exactly as this did for every row before.
    fn cli_for_record(&self, group: &GroupId, rec: &AgentRecord) -> String {
        let role = workflow::kind_from_str(&rec.role).unwrap_or(Role::Worker);
        self.group(group)
            .map(|g| g.guardrails.cli_for_block(&rec.block, role).to_string())
            .unwrap_or_else(|| "claude".to_string())
    }

    /// Read+parse a session's transcript into normalized digest events.
    /// Claude: resolved via the same `claude_projects_dir` test seam
    /// `group_usage` uses. Copilot: `session-state/<id>/` carries no
    /// per-turn transcript today (see `digest::parse_copilot_session_events`'s
    /// doc) — best-effort from what's on disk, never an error just because
    /// the richer files are absent. OpenCode: the group's own SQLite store
    /// (#722 slice B2), which is why this takes `group` at all — the store is
    /// per-group by construction (`OPENCODE_DB`), unlike the two
    /// machine-global session roots above.
    ///
    /// `session_id` reaches a filesystem path join on the claude and copilot
    /// branches. It's usually system-assigned (Claude's own session uuid), but
    /// `Task.session` can be set by an agent through `upsert_task`'s
    /// free-form `session` field — reject anything that isn't a plain path
    /// component before it ever reaches `Path::join` (review finding NB4,
    /// #250/#324 slice B follow-up). The opencode branch binds it as a SQL
    /// parameter instead, so it is not a path there; the check runs first for
    /// all three regardless, because an id this registry would refuse to look
    /// up on disk is not one to go looking for in a database either.
    ///
    /// **#925 replaced the predicate that used to stand here with the type.**
    /// It was `digest::is_safe_session_id`, which rejected `/`, `\`, `.` and
    /// `..` and nothing else — so `"C:"`, `"CON"`, a leading `-`, a 5000-byte
    /// id, a NUL byte and every non-ASCII byte all passed it and reached the
    /// join. Parsing once here and threading a [`PathSegment`] to both path
    /// arms means the refusal is no longer a check a future arm could forget to
    /// call: neither `claude_transcript_path` nor `copilot_session_dir_at` will
    /// accept anything else.
    fn read_session_transcript_events(
        &self,
        group: &GroupId,
        cli: &str,
        session_id: &str,
    ) -> Result<Vec<digest::TranscriptEvent>, String> {
        // Error PREFIX unchanged (`invalid session id: …`) — that prefix is
        // what callers and both tests match on. The message itself now appends
        // the `SegmentError`, so the refusal is diagnosable from a log line
        // without echoing the id twice.
        let session = PathSegment::parse(session_id)
            .map_err(|e| format!("invalid session id: {session_id:?} ({e})"))?;
        match cli {
            "claude" => {
                let root = self
                    .claude_projects_dir
                    .lock_safe()
                    .clone()
                    .or_else(crate::usage::default_claude_projects_root)
                    .ok_or("cannot resolve the Claude projects root")?;
                let path = crate::usage::claude_transcript_path(&root, &session)
                    .ok_or_else(|| format!("no Claude transcript found for session {session_id}"))?;
                // Line-by-line via BufReader, not `fs::read_to_string` (review
                // finding NB3): mirrors `usage::claude_session_usage_in`. A
                // transcript is append-only and can be read mid-write — one
                // malformed byte sequence on a line-in-progress fails
                // `read_to_string`'s whole-file UTF-8 validation outright,
                // where `BufReader::lines().map_while(Result::ok)` keeps
                // everything read up to that point instead.
                let file = fs::File::open(&path).map_err(|e| e.to_string())?;
                let mut text = String::new();
                for line in BufReader::new(file).lines().map_while(Result::ok) {
                    text.push_str(&line);
                    text.push('\n');
                }
                Ok(digest::parse_claude_transcript_events(&text))
            }
            "copilot" => {
                let root = crate::sessions::copilot_session_state_root()
                    .ok_or("cannot resolve the Copilot session-state root")?;
                let dir = crate::sessions::copilot_session_dir_at(&root, &session);
                let workspace = fs::read_to_string(dir.join("workspace.yaml")).unwrap_or_default();
                let checkpoints = fs::read_to_string(dir.join("checkpoints").join("index.md")).unwrap_or_default();
                Ok(digest::parse_copilot_session_events(&workspace, &checkpoints))
            }
            "opencode" => {
                // The group's store, through the one read-only path
                // `opencodedb` exposes — never a second connection route to
                // the same file. `Unavailable` is a degrade for the polled
                // usage meter, but a digest is a single deliberate call whose
                // whole product is this transcript, so it surfaces as an error
                // carrying the reason (absent store / unopenable / schema
                // drift) instead of an empty digest that would read as "this
                // worker hit no friction". `corroborating_session_keys`
                // already drops a session whose transcript won't read, so a
                // missing store shrinks `sessions_scanned` rather than failing
                // somebody else's digest.
                let db = self.opencode_db_path(group);
                let rows = crate::opencodedb::session_transcript(&db, session_id)
                    .map_err(|e| format!("no opencode transcript for session {session_id}: {e}"))?;
                Ok(digest::parse_opencode_transcript_events(&rows))
            }
            other => Err(format!("session_digest does not support agent CLI {other:?}")),
        }
    }

    // ---------- durable agent-id high-water mark (#524) ----------

    /// Path of the durable agent-id high-water mark (#524).
    ///
    /// At the orchestration ROOT, beside the group dirs rather than inside
    /// one, because [`seq`](Self::seq) is registry-global: the same counter
    /// mints `w-3` in one group and `rev-4` in the next. A per-group file (the
    /// issue's literal option (a)) would need a max-across-every-group read
    /// before the first mint, and would still LOSE the mark whenever a group
    /// dir is swept — while the artifacts keyed by the ids it handed out do
    /// not go with it. A stale `agent/rev-8` branch in the human's repo,
    /// colliding with a freshly re-minted `rev-8`, is the collision #227
    /// actually hit.
    ///
    /// A plain file here is invisible to both root scans: `session_roles`
    /// skips any entry without a `group.json`, and `existing_group_ids`
    /// filters on `is_dir()`.
    fn agent_seq_path(&self) -> PathBuf {
        self.root.join("agent-seq.json")
    }

    /// Seed [`seq`](Self::seq) from durable state, exactly once. **Caller must
    /// hold `agent_seq_persist`.**
    ///
    /// Two sources, in order of authority:
    ///
    /// 1. `agent-seq.json` — written by every mint since #524, so it is by
    ///    construction at least as high as any id this install handed out.
    /// 2. Failing that (a fresh install has no file; so does the first launch
    ///    after upgrading to a build that has this), the **durable roster** —
    ///    `agents.json` in every group dir. That file already exists and
    ///    already records every id, so an install that predates the counter
    ///    file does not have to re-mint its way through a collision before
    ///    the protection starts working. This is the "derive it from state you
    ///    already keep" alternative the issue asks to weigh, used where it is
    ///    genuinely better (migration) rather than as the mechanism, because
    ///    as the mechanism it fails in two directions the file does not: it
    ///    misses an agent whose spawn crashed between the mint and the roster
    ///    write, and it forgets everything when a group dir is swept.
    fn seed_agent_seq_locked(&self) {
        if self.agent_seq_seeded.swap(true, Ordering::SeqCst) {
            return;
        }
        let from_file = fs::read_to_string(self.agent_seq_path())
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v["high_water"].as_u64())
            .map(|n| n.min(u32::MAX as u64) as u32);
        // **Both sources, always the higher** — not the file with the roster
        // as a fallback (rev-13's blocking finding on #604). Consulting the
        // roster only when the file is missing trusts a file that is known to
        // be able to LIE: `mint_agent_seq` audits a failed write and does not
        // propagate it, so the id it just returned goes on to be worktreed,
        // rostered and audited while the mark that reserved it never reached
        // disk. The file then parses to a value BELOW the roster, the old
        // `match` took it at face value, and the next restart reissued a live
        // id — #524's own bug, back through #524's own degraded path.
        //
        // The floor's two weaknesses (it misses a spawn that died before its
        // roster write; it forgets a swept group dir) are arguments against
        // the roster as the ONLY source, which is what they were written for.
        // A max inherits each source's strength and neither's weakness, and
        // costs one directory scan per process — which the migration path
        // already paid for.
        let mark = from_file.unwrap_or(0).max(self.agent_seq_floor_from_rosters());
        // `fetch_max`, not `store`: a seed may never move the counter
        // BACKWARDS, which is the one direction that reintroduces the bug.
        self.seq.fetch_max(mark, Ordering::SeqCst);
    }

    /// Highest agent-id suffix recorded in any group's durable roster — one of
    /// the two sources `seed_agent_seq_locked` takes the max of, never a
    /// fallback consulted only when the other is missing (see its comment).
    /// Best-effort by construction: an unreadable root, group dir, or
    /// `agents.json` contributes nothing rather than failing a spawn.
    fn agent_seq_floor_from_rosters(&self) -> u32 {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return 0;
        };
        let mut high = 0u32;
        for e in entries.flatten().filter(|e| e.path().is_dir()) {
            let Ok(text) = fs::read_to_string(e.path().join("agents.json")) else {
                continue;
            };
            let Ok(list) = serde_json::from_str::<Vec<AgentRecord>>(&text) else {
                continue;
            };
            for r in list {
                if let Some(n) = agent_id_suffix(&r.id) {
                    high = high.max(n);
                }
            }
        }
        high
    }

    /// Take the next agent-id suffix, durably (#524) — the ONLY way a value
    /// leaves [`seq`](Self::seq).
    ///
    /// Seed, `fetch_add` and persist all happen under one lock, so the file
    /// can never end up below an id already handed out (see
    /// `agent_seq_persist`'s doc). The write lands BEFORE the id is returned,
    /// which is what makes the guarantee survive a crash mid-spawn: the
    /// caller has not yet cut a worktree, written a roster row, or emitted an
    /// audit line, so there is no window where an artifact outlives the mark
    /// that reserved its id.
    ///
    /// A failed write is audited to `group` and not propagated — the same
    /// rule `persist_queues` follows, for the same reason: failing the spawn
    /// would turn a durability degradation into an outage of the thing being
    /// made durable.
    ///
    /// A spawn that mints and then fails (a guardrail refusal, a worktree that
    /// won't cut) leaves its id spent, which is unchanged from before this and
    /// deliberately not "fixed": returning an id to the pool is exactly the
    /// reuse this closes. The sequence has gaps; that is what a gap means.
    pub(in crate::orchestration) fn mint_agent_seq(&self, group: &GroupId) -> u32 {
        let _writer = self.agent_seq_persist.lock_safe();
        self.seed_agent_seq_locked();
        // `saturating_add` under the lock, not `fetch_add` (rev-13 N1). A
        // stored `high_water` at `u32::MAX` — implausible, but the one input
        // where the parse-side clamp does NOT fail safe — made the atomic wrap
        // to 0 and the `+ 1` overflow, and `[profile.release]` sets no
        // `overflow-checks`, so the shipped binary would wrap silently where
        // `cargo test` panics. Wrapping is the worst possible failure here: it
        // reissues from `w-1` and collides with EVERY live id at once.
        // Saturating instead pins the counter at the ceiling — still broken,
        // but broken in the direction that hands out no id twice until the
        // next mint, and it says so out loud rather than looking healthy.
        // Load/store is safe because every mint holds this lock and nothing
        // else touches `seq`.
        let prev = self.seq.load(Ordering::SeqCst);
        let seq = prev.saturating_add(1);
        self.seq.store(seq, Ordering::SeqCst);
        if seq == prev {
            self.audit(group, brand::AUDIT_ACTOR, "agent-seq-exhausted", json!({
                "seq": seq,
                "detail": "agent-id counter is at u32::MAX — ids are no longer unique",
            }));
        }
        let body = serde_json::to_string_pretty(&json!({
            "high_water": seq,
            "updated_ms": now_ms(),
        }))
        .unwrap();
        if let Err(e) = atomic_write(&self.agent_seq_path(), body.as_bytes()) {
            self.audit(group, brand::AUDIT_ACTOR, "agent-seq-persist-failed", json!({
                "seq": seq, "error": e.to_string(),
            }));
        }
        seq
    }

    // ---------- durable roster (session ↔ role mapping, resume) ----------

    /// Upsert an agent into the group's `agents.json`. Best-effort like the
    /// audit log; shares the file lock with the task board.
    pub(in crate::orchestration) fn persist_agent_record(&self, entry: &AgentEntry, status: &str) {
        let _guard = self.tasks_lock.lock_safe();
        let path = self.group_dir(&entry.group).join("agents.json");
        let mut list: Vec<AgentRecord> = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let record = AgentRecord {
            id: entry.id.clone(),
            role: entry.role.as_str().into(),
            block: entry.block.clone(),
            name: entry.name.clone(),
            name_source: entry.name_source,
            session: entry.session_id.clone(),
            cwd: entry.cwd.clone(),
            status: status.to_string(),
            updated_ms: now_ms(),
            task: entry.task.clone(),
            branch: entry.branch.clone(),
            pane_kind: entry.pane_kind.clone(),
            forked_from: entry.forked_from.clone(),
        };
        // Match by (id, session). Since #524 an id is never re-minted, so a
        // bare-id match can no longer overwrite a DIFFERENT run's record —
        // but the pair stays, and not merely out of caution: rosters written
        // before #524 are still on disk and do contain repeated ids, and the
        // session half is what keeps a copilot placeholder (session `None`)
        // upgradable in place. A session-bearing record also supersedes
        // this run's placeholder for the same id — copilot writes an entry
        // with no session at spawn, then upgrades it once its session id is
        // discovered (only placeholders have session == None).
        match list.iter_mut().find(|r| {
            r.id == record.id && (r.session == record.session || r.session.is_none())
        }) {
            Some(r) => *r = record,
            None => list.push(record),
        }
        let _ = fs::create_dir_all(self.group_dir(&entry.group));
        // Atomic replace so a failed write can't wipe the agent roster (#133).
        // Holds `tasks_lock` (taken above), so writes are serialized.
        let body = serde_json::to_string_pretty(&list).unwrap();
        let _ = atomic_write(&path, body.as_bytes());
        // #2985: the same roster, projected into the flat form the `gh` shim
        // can read from POSIX `sh` — written from THIS list, under THIS lock,
        // in the same atomic-replace style, so the close gate can never be
        // deciding from a roster that disagrees with `agents.json`. A failed
        // write leaves the previous file intact and the gate then refuses a
        // close it cannot attribute, which is the safe direction.
        let rows: Vec<(String, String, Option<String>)> = list
            .iter()
            .filter(|r| r.status != "dead")
            .map(|r| (r.id.clone(), r.role.clone(), r.branch.clone()))
            .collect();
        let _ = atomic_write(
            &self.group_dir(&entry.group).join(OWNER_ROSTER_FILE),
            render_owner_roster(&rows).as_bytes(),
        );
    }

    fn group_records(&self, group: &GroupId) -> Vec<AgentRecord> {
        fs::read_to_string(self.group_dir(group).join("agents.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Snapshot the sessions a CLI's store already holds, immediately before a
    /// pane is spawned, so the one that pane mints can be told apart later.
    /// `None` for a CLI with nothing to learn (claude and pi are handed their
    /// id — [`CliCaps::premints_session_id`]; gemini has no store loomux
    /// reads), and `None` when the snapshot itself could not be taken — see
    /// below.
    ///
    /// **A baseline that cannot be read is not an empty baseline.** An empty
    /// one says "the store held nothing", which makes every session in it a
    /// candidate; if the truth was "the store could not be opened this
    /// instant", that turns another pane's existing session into this pane's
    /// answer. So a real degrade refuses to watch at all — the pane stays
    /// unidentified, exactly as an opencode pane is today — and says so once
    /// in the audit log. A *missing* store is the opposite case and genuinely
    /// is an empty baseline: opencode has not created the group's file yet,
    /// which is the ordinary state of the first pane in a group.
    #[doc(hidden)] // pub for integration tests
    pub fn capture_session_baseline(
        &self,
        cli: &str,
        group: &GroupId,
    ) -> Option<SessionBaseline> {
        // #2126: asked as a CAPABILITY, and asked FIRST. "loomux hands this
        // CLI its id" and "there is nothing for a store watcher to learn" are
        // one fact, and before this they were two — the mint sites named
        // claude and this function omitted it, so a CLI that gained a
        // `--session-id` had to be remembered in two places or it would both
        // be handed an id AND have a thread watching a store for the id it was
        // never going to invent. `CLI_CAPS` decides it once for both.
        //
        // Ahead of the match rather than folded into it, so a row landing here
        // with `premints_session_id: true` and a `SessionBaseline` variant of
        // its own cannot end up with the variant silently winning.
        if premints_session_id(cli) {
            return None;
        }
        match cli {
            "copilot" => crate::sessions::copilot_session_state_root().map(|root| {
                let ids = crate::sessions::copilot_session_ids(&root);
                SessionBaseline::Copilot { ids, root }
            }),
            // codex (#2515 C1): copilot's shape — a store that is the HUMAN's,
            // not this group's, so an absent root is "codex has never run
            // here" and is a real (empty) baseline, while an unresolvable one
            // is a degrade that refuses to watch. `codex_sessions_root()`
            // answers `None` only for the second: it resolves a path without
            // touching the disk, so `None` means there is no home to resolve
            // AT ALL, which is not the same fact as an empty store.
            "codex" => match crate::sessions::codex_sessions_root() {
                Some(root) => {
                    let ids = crate::sessions::codex_session_ids(&root);
                    Some(SessionBaseline::Codex { ids, root })
                }
                None => {
                    self.audit(group, brand::AUDIT_ACTOR, "session-untracked", json!({
                        "cli": "codex",
                        "reason": "could not resolve codex's session store before spawning, so a \
                                   new session cannot be told from an existing one",
                    }));
                    None
                }
            },
            "opencode" => match crate::opencodedb::session_ids(&self.opencode_db_path(group)) {
                Ok(ids) => Some(SessionBaseline::OpenCode { ids }),
                Err(crate::opencodedb::Unavailable::Absent) => {
                    Some(SessionBaseline::OpenCode { ids: HashSet::new() })
                }
                Err(e) => {
                    self.audit(group, brand::AUDIT_ACTOR, "session-untracked", json!({
                        "cli": "opencode",
                        "reason": format!(
                            "could not snapshot the session store before spawning, so a new \
                             session cannot be told from an existing one: {e}"
                        ),
                    }));
                    None
                }
            },
            _ => None,
        }
    }

    /// Poll the CLI's own session store for the session this just-spawned pane
    /// created — the one absent from `baseline` — and bind its id to the pane.
    /// Runs on its own thread; both CLIs write the session some seconds into
    /// boot. Gives up after the baseline's own deadline.
    ///
    /// One watcher for both CLIs (#722), not a copilot one and an opencode
    /// twin: the shape is identical — snapshot before the spawn, poll for what
    /// appeared, stop on a dead or already-identified pane, audit once on
    /// giving up — and only *where to look* differs, which is what
    /// [`SessionBaseline`] carries.
    pub(in crate::orchestration) fn spawn_session_watcher(
        self: Arc<Self>,
        agent_id: String,
        group_id: GroupId,
        cwd: String,
        baseline: SessionBaseline,
    ) {
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + baseline.timeout();
            // Why the LAST outcome and not the first: the interesting reason a
            // watch times out is the one still true at the deadline. A store
            // that was briefly unreadable and then fine, or a contest that
            // resolved, must not be what the audit line blames.
            // Declared without an initial value on purpose: every path that
            // reaches the timeout check below has already assigned one this
            // tick, so a placeholder here could only ever be dead.
            let mut last;
            loop {
                std::thread::sleep(baseline.poll());
                // Stop if the pane died or was already associated (a resume
                // re-spawn, or a manual edit) — nothing left to track.
                match self.agent(&agent_id) {
                    Some(a) if a.status == AgentStatus::Dead => return,
                    Some(a) if a.session_id.is_some() => return,
                    Some(_) => {}
                    None => return,
                }
                last = self.search_for_session(&group_id, &agent_id, &cwd, &baseline);
                if let SessionSearch::Found(sid) = &last {
                    if self.associate_session(&group_id, &agent_id, sid) {
                        return;
                    }
                    // Lost the race: another pane bound this id between our
                    // search and our write. Keep watching rather than giving
                    // up — the winner's claim now excludes that id from our
                    // search, so the next tick looks for a session of our own,
                    // which is also exactly what the timeout should report if
                    // one never appears.
                    last = SessionSearch::Waiting;
                }
                if std::time::Instant::now() >= deadline {
                    self.audit(&group_id, brand::AUDIT_ACTOR, "session-untracked", json!({
                        "agent": agent_id,
                        "cli": baseline.cli(),
                        "reason": last.untracked_reason(),
                    }));
                    return;
                }
            }
        });
    }

    /// One poll of the CLI's session store. Split out of the watcher loop so
    /// the decision is testable without a thread, a clock, or a live pane.
    #[doc(hidden)] // pub for integration tests
    pub fn search_for_session(
        &self,
        group_id: &GroupId,
        agent_id: &str,
        cwd: &str,
        baseline: &SessionBaseline,
    ) -> SessionSearch {
        match baseline {
            SessionBaseline::Copilot { ids, root } => {
                match crate::sessions::newest_new_copilot_session(root, ids, cwd) {
                    Some(sid) => SessionSearch::Found(sid),
                    None => SessionSearch::Waiting,
                }
            }
            SessionBaseline::Codex { ids, root } => {
                // Same claim exclusion as opencode's below, and for a reason
                // that is stronger here rather than weaker: codex's store is
                // the HUMAN's, shared by every group AND by their own
                // terminal sessions, so "a new rollout under this directory"
                // separates far less than it does in a group-local store.
                // Directory plus baseline plus claims is the whole of what
                // distinguishes two panes in one worktree, and when that is
                // not enough the answer is `Contested`, never a guess.
                let claimed = self.claimed_sessions(group_id, agent_id);
                match crate::sessions::newest_new_codex_session(root, ids, cwd, &claimed) {
                    crate::sessions::CodexIdentified::One(sid) => SessionSearch::Found(sid),
                    crate::sessions::CodexIdentified::None => SessionSearch::Waiting,
                    crate::sessions::CodexIdentified::Contested(n) => SessionSearch::Contested(n),
                }
            }
            SessionBaseline::OpenCode { ids } => {
                // The store is the group's, resolved through the ONE function
                // the spawn that creates it and the usage read that consumes it
                // already share (#812) — three callers agreeing by
                // construction rather than by three matching literals.
                let db = self.opencode_db_path(group_id);
                // Sessions other panes in this group have already taken. A
                // group's store is shared by every pane in it, and several of
                // those panes run in the SAME directory (the orchestrator, the
                // reviewer, any worker without its own worktree), so directory
                // plus baseline alone does not separate them — see
                // `opencodedb::identify_session_on`.
                let claimed = self.claimed_sessions(group_id, agent_id);
                match crate::opencodedb::identify_session(&db, cwd, ids, &claimed) {
                    Ok(crate::opencodedb::Identified::One(sid)) => SessionSearch::Found(sid),
                    Ok(crate::opencodedb::Identified::None) => SessionSearch::Waiting,
                    Ok(crate::opencodedb::Identified::Contested(n)) => SessionSearch::Contested(n),
                    // Absent is not a degrade here, it is the ordinary state of
                    // a store opencode has not created yet — the same reading
                    // `note_opencode_db_degrade` gives it.
                    Err(crate::opencodedb::Unavailable::Absent) => SessionSearch::Waiting,
                    Err(e) => SessionSearch::Unreadable(e.to_string()),
                }
            }
        }
    }

    /// Session ids already bound to OTHER panes in this group.
    ///
    /// Live agents only, deliberately: a pane that died before this one
    /// spawned had its session created before the baseline snapshot, so the
    /// baseline already excludes it — reading `agents.json` every poll would
    /// buy nothing and put a file read on a polled path.
    fn claimed_sessions(&self, group_id: &GroupId, except_agent: &str) -> HashSet<String> {
        self.agents
            .lock_safe()
            .values()
            .filter(|a| a.group == group_id && a.id != except_agent)
            .filter_map(|a| a.session_id.clone())
            .collect()
    }

    /// Bind a discovered session id to a live pane: update the agent map, the
    /// durable roster (`agents.json`), and any task board item this agent owns
    /// — the same session trail Claude gets at spawn. Best-effort. Public for
    /// the session watcher and its tests.
    ///
    /// Returns whether this call bound the id. `false` — a no-op — when the
    /// pane is gone, when it already carries an id, or when **another pane in
    /// this group is already bound to this session**. That last one is not a
    /// formality: see the comment inside, and note that a caller which treats
    /// `false` as "done" turns a lost race into a permanently unidentified
    /// pane. The watcher keeps polling instead.
    pub fn associate_session(&self, group_id: &GroupId, agent_id: &str, session_id: &str) -> bool {
        // The lock scope DECIDES; everything else happens after the guard
        // drops — same shape, and same reason, as `note_opencode_db_degrade`:
        // `audit` takes its own locks, and holding an unrelated one across it
        // is how lock-order bugs start. Keeping the two sites identical also
        // means this file has one pattern for "record a refusal" rather than
        // two a reader has to tell apart (rev-306 F1). What must stay inside
        // the guard is only the check-and-write pair below, which is the whole
        // point of NB3.
        let (entry, taken_by_another) = {
            let mut agents = self.agents.lock_safe();
            // The claim exclusion `search_for_session` applies is READ under
            // this lock and then released before the store is queried, so two
            // watchers can both be inside that window at once: with one
            // candidate visible and neither pane bound yet, both searches
            // legitimately answer `Found(same id)`. Refusing here — under the
            // one lock that also performs the write — is what makes the
            // exclusion hold, rather than merely usually holding. Without it
            // the refusal policy protects only the two-candidate case and
            // leaks exactly the harm it exists to prevent (one conversation
            // bound to two panes) in the one-candidate case (rev-306 NB3).
            if agents.values().any(|a| {
                a.group == group_id
                    && a.id != agent_id
                    && a.session_id.as_deref() == Some(session_id)
            }) {
                (None, true)
            } else {
                match agents.get_mut(agent_id) {
                    // Don't clobber an id set in the meantime (e.g. a resume).
                    Some(a) if a.session_id.is_none() => {
                        a.session_id = Some(session_id.to_string());
                        (Some(a.clone()), false)
                    }
                    // Pane gone, or already carries an id: a no-op either way.
                    _ => (None, false),
                }
            }
        };
        if taken_by_another {
            self.audit(group_id, brand::AUDIT_ACTOR, "session-claim-refused", json!({
                "agent": agent_id,
                "session": session_id,
                "reason": "another pane in this group is already bound to this session",
            }));
            return false;
        }
        let Some(entry) = entry else { return false };
        let status = match entry.status {
            AgentStatus::Dead => "dead",
            _ => "running",
        };
        self.persist_agent_record(&entry, status);
        // Mirror onto the task board: any item this agent owns (by id or
        // display name) that lacks a session gets it, so the orchestrator can
        // resume the task later without hunting the id out of list_agents.
        {
            let _guard = self.tasks_lock.lock_safe();
            let mut tasks = self.tasks(group_id);
            let mut changed = false;
            for t in tasks.iter_mut() {
                let owner = t.assignee.as_deref().unwrap_or("");
                if t.session.is_none() && (owner == entry.id || owner == entry.name) {
                    t.session = Some(session_id.to_string());
                    t.updated_ms = now_ms();
                    changed = true;
                }
            }
            if changed {
                let _ = self.write_tasks(group_id, &tasks);
            }
        }
        // One event for one fact (#722): this fires for copilot and opencode
        // alike, so a reader asking "when did this pane get its session" does
        // not have to know which CLI it was to know which line to look for.
        // The CLI itself is not repeated here — this agent's `agent-spawn`
        // line already records it, and a second copy could only ever disagree.
        self.audit(group_id, brand::AUDIT_ACTOR, "session-learned",
            json!({ "agent": agent_id, "session": session_id }));
        // The frontend's copy of that same fact (#1563), emitted here rather
        // than at the watcher so every path that binds an id reports it, and
        // last so that the two early returns above — a claim another pane
        // holds, and a pane that is gone or already bound — emit nothing.
        self.emit_session_learned(group_id, agent_id, session_id);
        true
    }

    /// Tell the frontend an id the BACKEND learned (#1563).
    ///
    /// copilot and opencode accept no pre-minted session id, so their session
    /// is bound after boot by [`Self::spawn_session_watcher`] →
    /// `associate_session`, and until this event existed the pane was never
    /// told: `Pane.capture()` wrote `sessionId: null` into `tabs.json` and the
    /// dormant-group card then offered no resume for a session `agents.json`
    /// had recorded all along. The roster knew; the webview had no way to be
    /// told. A PREMINTING CLI is unaffected — claude and pi both have their id
    /// minted onto the command line at spawn, so neither ever reaches this path
    /// at all ([`Self::capture_session_baseline`] answers `None` for them, so no
    /// watcher is started and nothing is ever learned to emit).
    ///
    /// One event per binding, on the same `AppHandle` seam `orch-focus` and
    /// `orch-spawn-request` use.
    ///
    /// The frontend locates the pane by **agent id alone** and never reads
    /// `group_id` — sound because agent ids are minted from one registry-global
    /// counter (see [`Self::seq`]), so an id names at most one pane anywhere.
    /// `group_id` rides along for audit legibility, for symmetry with every
    /// other `orch-*` event, and for a future consumer that wants to filter by
    /// group — not because this event's own listener needs it.
    fn emit_session_learned(&self, group_id: &GroupId, agent_id: &str, session_id: &str) {
        // Built ONCE, above the branch, so the payload a test observes is the
        // same value production emits rather than a second construction of it.
        let payload = json!({
            "group_id": group_id,
            "agent_id": agent_id,
            "session_id": session_id,
        });
        // Bound BEFORE the match: a match scrutinee's temporaries live for the
        // whole match, so branching on `self.app.lock_safe().clone()` directly
        // would hold that guard across both `emit` and a second registry lock —
        // the lock-order hazard `associate_session` above is explicitly shaped to
        // avoid. `spawn_agent_ex` binds first for the same reason.
        let app = self.app.lock_safe().clone();
        match app {
            // Best-effort: a webview that has gone away must not fail the
            // binding, which is already durable in `agents.json` by now.
            Some(app) => { let _ = app.emit("orch-session-learned", &payload); }
            None => self.test_session_learned.lock_safe().push(payload),
        }
    }

    /// The substring a line must contain before it is worth handing to
    /// `serde_json` at all (#1592). Conservative by construction: `action` is
    /// compared against this exact literal below, so a line that does not carry
    /// these bytes anywhere cannot possibly match. Every writer of this file is
    /// `serde_json::to_string` in this process (see `audit`), and serde_json's
    /// serializer never escapes an ASCII letter or a hyphen — so the raw bytes
    /// of an `agent-spawn` action are always literally present. The residual is
    /// stated rather than assumed away: a hand-edited audit line spelling the
    /// action with `\u` escapes would be skipped here where the old
    /// parse-everything loop would have matched it.
    const AUDIT_SPAWN_MARKER: &'static str = "agent-spawn";

    /// Roster entries derived from `agent-spawn` audit lines, oldest first.
    /// Backfill for groups created before agents.json existed — their
    /// session-to-role mapping lives only in the audit log. That is WHY the
    /// audit is read at all, and it is the half of this comment that is
    /// load-bearing: without it the streaming note below reads as an
    /// optimisation of something with no stated purpose.
    ///
    /// **Streamed, not slurped** (#1592). This used to `read_to_string` BOTH
    /// generations into one `String` and then build a full `serde_json::Value`
    /// for every line of it. An install with a long orchestration history keeps
    /// tens of megabytes per group there, and `session_roles` calls this once
    /// per group — so peak memory was the whole corpus at once, and every line
    /// of it was parsed whether or not it was a spawn. Reading line by line
    /// bounds the buffer at one line, and the marker prefilter above keeps the
    /// `Value` allocation for the rows that can actually match.
    ///
    /// **The rows returned, and their order, are unchanged on every well-formed
    /// line** — and the two degrade paths are strictly WIDER, never narrower,
    /// which is a behaviour change rather than a pure refactor (#1592 review
    /// N2). No record is lost either way:
    ///
    ///  1. A per-line IO or UTF-8 error stops THAT generation and moves on,
    ///     where `read_to_string` failed the whole file and contributed
    ///     nothing.
    ///  2. An `audit.1.jsonl` with no trailing newline no longer has its last
    ///     line concatenated onto `audit.jsonl`'s first. Both used to be lost
    ///     to the one parse failure that splice caused; both now parse.
    fn records_from_audit(&self, group: &GroupId) -> Vec<AgentRecord> {
        let mut out: Vec<AgentRecord> = Vec::new();
        // Oldest first so newer spawns win the (id, session) upsert; the
        // rotated generation holds the older entries.
        for name in ["audit.1.jsonl", "audit.jsonl"] {
            let Ok(file) = fs::File::open(self.group_dir(group).join(name)) else { continue };
            for line in BufReader::new(file).lines() {
                // A read error (I/O, or invalid UTF-8 in one line) stops THIS
                // generation and moves to the next, where the pre-streaming
                // `read_to_string` failed the whole file and contributed
                // nothing. The degrade is therefore strictly WIDER, not
                // narrower — partial rows where there used to be none — which
                // is the direction a best-effort listing wants, and it is
                // stated because it IS a behaviour change rather than a pure
                // refactor.
                let Ok(line) = line else { break };
                Self::push_audit_spawn(&line, &mut out);
            }
        }
        out
    }

    /// One audit line's contribution to [`Self::records_from_audit`]. Split out
    /// so the streaming loop above stays a loop and this stays the parse (#1592);
    /// the body is byte-for-byte the pre-streaming one.
    fn push_audit_spawn(line: &str, out: &mut Vec<AgentRecord>) {
        if !line.contains(Self::AUDIT_SPAWN_MARKER) {
            return;
        }
        {
            let Ok(v) = serde_json::from_str::<Value>(line) else { return };
            if v["action"] != "agent-spawn" {
                return;
            }
            let d = &v["detail"];
            let Some(session) = d["session"].as_str() else { return };
            let role = d["role"].as_str().unwrap_or("worker").to_string();
            // Mirror `spawn_agent_ex`'s persisted_branch rule (#1): the spawn
            // audit always records a `branch` value (even a fallback name for
            // roles that never use one), so only trust it where that role
            // could actually have a real branch — a worker (worktree or
            // shared-repo), or a reviewer that got a worktree.
            let worktree = d["worktree"].as_bool().unwrap_or(false);
            let branch = (role == "worker" || (role == "reviewer" && worktree))
                .then(|| d["branch"].as_str())
                .flatten()
                .map(String::from);
            let task = d["task"].as_str().unwrap_or("").to_string();
            let record = AgentRecord {
                id: d["agent"].as_str().unwrap_or("").to_string(),
                name: d["name"]
                    .as_str()
                    .unwrap_or(if role == "orchestrator" { "orchestrator" } else { "agent" })
                    .to_string(),
                // The spawn audit predates the name-tier field; backfilled
                // sessions restore at the default tier (#95r).
                name_source: NameSource::default(),
                // Blocks (#222) are recorded in the spawn audit; an audit line
                // from an older build has none, and the rejoin then falls back
                // to the class's default block.
                block: d["block"].as_str().unwrap_or("").to_string(),
                role,
                session: Some(session.to_string()),
                cwd: d["cwd"].as_str().unwrap_or("").to_string(),
                // The audit alone can't tell liveness; group_live covers it.
                status: "unknown".into(),
                updated_ms: v["ts_ms"].as_u64().unwrap_or(0),
                task,
                branch,
                // The spawn audit does not record the pane kind, and this
                // rebuild is reconstructing a roster from it. `None` is the
                // honest answer — absent means `pty`, which is what every
                // agent this path can see was, since a structured pane is
                // #2850 and newer than every audit line it reads.
                pane_kind: None,
                forked_from: None,
            };
            match out.iter_mut().find(|r| r.id == record.id && r.session == record.session) {
                Some(r) => *r = record,
                None => out.push(record),
            }
        }
    }

    /// Roster + audit backfill, deduped by session (roster wins). Sessions
    /// are the stable key: ids stopped recycling in #524, but rosters written
    /// before it are still on disk and do repeat them, so dedup by id would
    /// still merge two different agents' records on existing installs.
    pub(in crate::orchestration) fn merged_records(&self, group: &GroupId) -> Vec<AgentRecord> {
        // One group's roster + full audit log read and parsed — the unit
        // `GROUP_RECORD_SCANS` counts, so a test can pin how MANY groups a
        // lookup touches instead of how long it took (#514).
        GROUP_RECORD_SCANS.with(|c| c.set(c.get().saturating_add(1)));
        let mut records = self.group_records(group);
        for r in self.records_from_audit(group) {
            let dup = records.iter().any(|x| match (&x.session, &r.session) {
                (Some(a), Some(b)) => a == b,
                _ => x.id == r.id,
            });
            if !dup {
                records.push(r);
            }
        }
        records
    }

    /// **The roster row that says what CAPABILITY CLASS a session is** — the
    /// one lookup every resume path asks, so they cannot disagree (#1961).
    ///
    /// # Why the FIRST row and not the last-touched one
    ///
    /// A session is minted by one pane, running one block, under one CLI, and
    /// that is not a property later panes get to revise: a Claude transcript
    /// resumed under an opencode block does not come back as a different
    /// persona, it fails to open at all (`Invalid session ID`). So the question
    /// this answers — *whose conversation is this* — has exactly one true
    /// answer for the life of the session, and it is the one the roster wrote
    /// first.
    ///
    /// `agents.json` is a `Vec` that `persist_agent_record` **appends** to and
    /// then updates in place, so its order is spawn order and the first row
    /// naming a session is the pane that originally ran it. `updated_ms` cannot
    /// answer this: it is last-*touched*, not created, so a long-lived original
    /// pane sorts AFTER a resume opened minutes ago, and `max_by_key` returns
    /// the newest identity rather than the real one. That is #1961's amplifier
    /// exactly — the driver wrote one wrong row for a session and every later
    /// bare `spawn_agent(resume_session:)` inherited the wrong block from it,
    /// including the orchestrator's own hand recovery.
    ///
    /// The session browser's rejoin already resolved the block with `find`
    /// (this rule) while the MCP arm used `max_by_key` (the other one), which
    /// is why the human's click-to-rejoin recovered the pane that the tool
    /// call could not. This function is that rule, named once.
    ///
    /// **The rule survives the roster being lost, and it is worth saying why**
    /// (rev-std round 2, premortem 2). `merged_records` falls through to
    /// audit-derived rows for a session no roster row names, so "the first
    /// record" becomes the earliest SURVIVING one — and the concern is that it
    /// could then be a resume row carrying the resumed block rather than the
    /// minting pane. It cannot, because `records_from_audit` walks the audit
    /// generations oldest-first and pushes in file order, and the audit is
    /// append-only: the first `agent-spawn` line naming a session is the spawn
    /// that minted it. What would break the rule is a TRUNCATED audit, not a
    /// lost roster, and that is a state in which the group has lost the record
    /// of its own spawns rather than one this function can rule out.
    ///
    /// **Not the same question as "where does its work live".** A workspace
    /// legitimately MOVES over a session's life (a worktree re-cut, a resume
    /// placed elsewhere), so cwd inheritance keeps reading the LAST-touched
    /// record and is deliberately left alone. Identity is immutable; location
    /// is not.
    pub(in crate::orchestration) fn session_identity_record(&self, group: &GroupId, session: &str) -> Option<AgentRecord> {
        self.merged_records(group).into_iter().find(|r| r.session.as_deref() == Some(session))
    }

    /// **The branch a resumed pane records** (#3442): the one the session was
    /// minted against, read off [`session_identity_record`](Self::session_identity_record).
    ///
    /// # Why the roster, and not the worktree's `HEAD`
    ///
    /// The recorded branch is a CAPABILITY — the `gh` shim's close gate reads
    /// it out of `agent_owners` to decide which PRs this pane may close — so it
    /// must come from something the backend wrote, never from something the
    /// agent controls. A worktree's `HEAD` is the agent's to move: a worker
    /// that ran `git switch <another-branch>` before its pane ended would, on
    /// resume, be handed ownership of that branch and every `-scratchN` under
    /// it. The roster row is written only by `spawn_agent_full` at the moment
    /// it cut (or named) the branch. Reading `HEAD` would also be wrong in the
    /// benign cases: a reviewer's worktree is `--detach`ed (#359) and has no
    /// branch to read, and a spawn-path git subprocess is a cost and a failure
    /// mode this answer does not need.
    ///
    /// # Why the FIRST row, not the last-touched one
    ///
    /// Only the pane that MINTED a session ever assigns it a branch: a resume
    /// always arrives with a `cwd_override`, and that arm cuts nothing. Every
    /// later row is therefore a copy, and rows written before this fix are
    /// copies of `None` — so the last-touched row would hand every session
    /// already resumed once (i.e. every worker past round 1 of a driven
    /// review) exactly the empty branch this fixes. The minting row is the one
    /// fact; it is the same row #1961 made the answer to "which block".
    ///
    /// # Fail-closed arms
    ///
    /// `None` — owning nothing, which the close gate refuses — when no row
    /// names the session, when that row carries no branch (an orchestrator,
    /// planner, manager or worktree-less reviewer, which never had one), or
    /// when the pane being opened is a different CLASS from the one that
    /// minted the session: an explicit `kind`/`block` that resumes a worker's
    /// conversation as some other role is not that worker continuing its own
    /// work, and does not inherit its branch.
    pub(in crate::orchestration) fn resumed_session_branch(&self, group: &GroupId, session: &str, role: Role) -> Option<String> {
        let rec = self.session_identity_record(group, session)?;
        if rec.role != role.as_str() {
            return None;
        }
        rec.branch.map(|b| b.trim().to_string()).filter(|b| !b.is_empty())
    }

    /// Every recorded session across all groups on disk, with role identity
    /// — drives the session browser's ORCH/W/REV badges and restore flow.
    pub fn session_roles(&self) -> Vec<SessionRole> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return out;
        };
        for e in entries.flatten() {
            // #904: a directory name is an id from outside this process — parse it.
            let Ok(group_id) = GroupId::parse(&e.file_name().to_string_lossy()) else { continue };
            if !e.path().join("group.json").is_file() {
                continue;
            }
            let live = self.group_is_live(&group_id);
            // Resolved once per group (#1), not per session: the repo for the
            // "repo/branch" identity, and the board's session→pr map so a PR
            // set AFTER a session ended (or by a different agent entirely)
            // still surfaces against it. Best-effort — an unreadable
            // group.json degrades to `repo: None`, never a fabricated guess.
            let repo = self.load_group_file(&group_id).map(|(repo, _)| repo);
            let pr_by_session: HashMap<String, String> = self
                .tasks(&group_id)
                .into_iter()
                .filter_map(|t| Some((t.session?, t.pr?)))
                .collect();
            for r in self.merged_records(&group_id) {
                if let Some(session) = r.session {
                    let pr = pr_by_session.get(&session).cloned();
                    out.push(SessionRole {
                        session_id: session,
                        group_id: group_id.clone(),
                        role: r.role,
                        agent_name: r.name,
                        group_live: live,
                        task: r.task,
                        branch: r.branch,
                        repo: repo.clone(),
                        pr,
                        forked_from: r.forked_from,
                    });
                }
            }
        }
        out
    }

    /// Every orchestration group loomux has a record of on disk, for the
    /// session browser's "Orchestrations" section (#1563).
    ///
    /// **Why loomux's own record and not a CLI's.** The sidebar's session
    /// scan reads each CLI's own store, and for opencode that is deliberately
    /// the human's GLOBAL store only (`docs/design/opencode.md`): a group's
    /// opencode sessions live in `<group>/opencode/opencode.db`, which is
    /// excluded on purpose because a bare `--session` pane spawned from such a
    /// row would be powerless. Before #1563 that left a fresh opencode
    /// orchestrator with no UI route to `resume_recorded_session` AT ALL. This
    /// reads `group.json` + `agents.json` instead, so every recorded group has
    /// a route regardless of which CLI ran it — and one shape for every CLI,
    /// so claude groups reach it the same way.
    ///
    /// **Not the only route now, and the distinction is the point.** #1563
    /// slice A persists a learned id to `tabs.json`, so a dormant-group card
    /// can carry an opencode id too. That route needs the pane to have been
    /// open when the watcher bound the id, and that tab set to survive; this
    /// one reads the group's own roster and needs neither, so it still reaches
    /// a group whose card was never captured or whose tab set this window does
    /// not have.
    ///
    /// **What it deliberately does NOT read.** No transcript scan and no
    /// CLI-store enumeration, and in particular NOT [`Self::merged_records`],
    /// which parses both audit generations (bounded at ~16 MB per group).
    /// `orch_session_roles` already pays that fan-out per group; a second one
    /// on the same surface is what the #1563 plan forbids. (Until #1592 that
    /// command was also SYNC and #743's F4 row owned converting it. Both are
    /// now false — it is `async` over `run_blocking`, and that debt row is
    /// deleted — so the cost argument above stands on the fan-out alone, which
    /// is what it always rested on. What #749 still owns is narrower: an index
    /// or live-groups filter, so `session_roles` stops scaling with groups
    /// EVER created. See `docs/design/performance.md` §5.) The cost here is two
    /// small JSON reads per group, plus a store membership test per group with
    /// a recorded orchestrator session.
    ///
    /// **That membership test used to be a per-group store enumeration, and
    /// that is what #1592 fixed** (the paragraph this replaces described the
    /// pre-#1592 shape, and is preserved as history rather than deleted
    /// because the hazard it names is real and returns the moment anyone puts
    /// a per-group lookup back). Claude's was a filename probe per project
    /// dir (`<id>.jsonl`); opencode's is one indexed `SELECT`. But
    /// `find_copilot_session_cwd` has no filename-is-the-id shortcut — a
    /// copilot session's directory name is not guaranteed to equal its id, so
    /// only `workspace.yaml`'s own `id:` field is authoritative — and a MISS
    /// therefore parsed every session directory in the store, FOR EACH GROUP
    /// that missed. A stale group is precisely the one that misses, so the
    /// listing cost grew as groups × store. [`StoreIndex`] now enumerates each
    /// file-backed store at most once per listing; opencode keeps its own
    /// per-group `SELECT`, which has nothing to share across groups. Being off
    /// the webview thread and coalesced by the sidebar's `RefreshGate` bounds
    /// how often this runs — it never bounded how much it did.
    ///
    /// **What that exclusion COSTS, not only what it saves.** Reading
    /// `agents.json` alone means this list can resolve strictly LESS than
    /// `session_roles` can (#1568 review N2): a group whose orchestrator
    /// session survives only in `audit.jsonl` — a roster write lost to a
    /// crash, or a build predating the roster — reports `session_id: None`
    /// here ("session not yet identified") while the session list still
    /// offers its `ORCH` row and restores it fine. `last_seen_ms` is `0` for
    /// the same group, sorting it last within its liveness class. That is a
    /// deliberate trade and not merely a cost decision: the audit fallback is
    /// what the fan-out above buys, and it is bounded — the roster is written
    /// on every spawn and every `associate_session`, so an audit-only
    /// orchestrator means a lost write, not an ordinary state.
    ///
    /// **A damaged group is listed, not hidden.** A directory with a
    /// `group.json` that will not parse yields a row with `repo: None` and an
    /// empty `cli` rather than being skipped — the human whose group.json got
    /// torn is precisely the one who needs to see that the group is still
    /// there. The gate is the same one [`Self::session_roles`] uses (a
    /// `group.json` FILE must exist), so the two agree on what a group is.
    pub fn recorded_orchestrations(&self) -> Vec<RecordedOrchestration> {
        let mut out = Vec::new();
        // One enumeration per STORE for the whole listing, not one per group
        // (#1592) — see [`StoreIndex`]. Built lazily, so a root with no claude
        // group never lists claude's projects at all.
        let mut store = StoreIndex::default();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return out;
        };
        for e in entries.flatten() {
            // #904: a directory name is an id from outside this process — parse it.
            let Ok(group_id) = GroupId::parse(&e.file_name().to_string_lossy()) else { continue };
            if !e.path().join("group.json").is_file() {
                continue;
            }
            let loaded = self.load_group_file(&group_id);
            let repo = loaded.as_ref().map(|(repo, _)| repo.clone());
            // Resolved exactly as `resume_recorded_session`'s orchestrator
            // branch resolves it (the orchestrator BLOCK's cli, falling back
            // to the group's default), because `resumable` below has to ask
            // that path's question, not a similar one.
            let cli = loaded
                .as_ref()
                .map(|(_, g)| {
                    g.block_for(Role::Orchestrator)
                        .map(|b| workflow::cli_of(b, &g.agent_cli).to_string())
                        .unwrap_or_else(|| g.agent_cli.clone())
                })
                .unwrap_or_default();
            let records = self.group_records(&group_id);
            let last_seen_ms = records.iter().map(|r| r.updated_ms).max().unwrap_or(0);
            // A group can hold several orchestrator rows — every resume mints
            // a new agent id and upserts a new one. Prefer a row that actually
            // carries a session id (a later row with none is not evidence the
            // earlier session is gone), then the most recently updated.
            let session_id = records
                .iter()
                .filter(|r| r.role == "orchestrator")
                .max_by_key(|r| (r.session.is_some(), r.updated_ms))
                .and_then(|r| r.session.clone())
                .filter(|s| !s.trim().is_empty());
            // The SAME question the resume path asks, with the SAME per-group
            // opencode store (#722) — so a row that offers Resume is one
            // `resume_recorded_session` will accept, and one that cannot be
            // resumed says so instead of showing a button that fails on click.
            // An unreadable store degrades to `false`, never to an error: this
            // is a listing, and one broken group must not blank the whole list.
            //
            // An EMPTY `cli` (unreadable group.json) short-circuits to `false`
            // rather than asking a store, and that is load-bearing rather than
            // tidy (#1568 review N1). `session_cwd_in_store`'s non-opencode
            // branch routes to `find_session_cwd`, whose default arm is
            // CLAUDE's — so `""` would ask claude's projects directory, and a
            // torn group.json over a roster still naming a real claude session
            // would report `resumable: true` while `resume_recorded_session`
            // refuses at `load_group_file` ("group.json is missing for this
            // orchestration") without reaching any store at all. Asking a
            // DIFFERENT store's question is exactly what the parity claim above
            // forbids, so the answer is to ask none.
            let resumable = !cli.is_empty()
                && session_id.as_deref().is_some_and(|sid| {
                    // A GROUP-LOCAL store is already cheap — one indexed
                    // SELECT against this group's own opencode db, or one
                    // `read_dir` of its own pi directory — and there is nothing
                    // to share across groups, so those keep asking the resume
                    // path's own function. The per-user stores are the shared
                    // ones, and only those go through the index (#1592).
                    if group_local_session_store(&cli) {
                        matches!(
                            session_cwd_in_store(
                                &cli,
                                sid,
                                Some(&self.opencode_db_path(&group_id)),
                                Some(&self.pi_sessions_dir(&group_id)),
                            ),
                            Ok(Some(_))
                        )
                    } else {
                        store.contains(&cli, sid)
                    }
                });
            out.push(RecordedOrchestration {
                group_live: self.group_is_live(&group_id),
                group_id,
                repo,
                cli,
                session_id,
                resumable,
                last_seen_ms,
            });
        }
        out
    }

    /// Single-group fast path for the lookup `session_roles()` does (#479):
    /// resuming a session whose group is already known (every dormant-group
    /// Resume click names it via `hint` — see `resume_recorded_session`)
    /// doesn't need every OTHER group's group.json/tasks.json/full audit log
    /// read and parsed just to throw away every row but one. This reads and
    /// merges exactly the one group `session_roles()` would eventually find
    /// it in — same fields, same last-match tie-break, WITHIN that group.
    ///
    /// **Not proven equivalent to the full scan in general** (review round on
    /// #479's PR, correcting this comment's own prior overclaim): when a
    /// session id has an `agent-spawn` audit row in MORE than one group —
    /// reachable today via #485 (a delegate rejoined into the wrong group
    /// writes that row into it too) — a hit here resolves to the HINTED
    /// group deterministically, where the full scan's `.last()` resolves to
    /// whichever group `fs::read_dir(&self.root)` happened to enumerate last
    /// (arbitrary iteration order, not a considered choice either). The two
    /// can disagree in that corner. Hinted-group-wins is arguably the more
    /// defensible rule — it's the group the clicked tab is actually bound
    /// to — but it is a DIFFERENT tie-break, not a proof of sameness, and
    /// this must not be sold as one. #485 closed the way NEW two-group rows
    /// were created (`resume_recorded_session` now refuses a rejoin whose
    /// hint disagrees with the session's own record, and the dormant-group
    /// Resume click hints with the placeholder's OWN captured group rather
    /// than the tab's), but rows written before that fix are still on disk,
    /// so the corner remains reachable for existing data. A miss (stale/wrong
    /// hint, or the id genuinely isn't in that group) falls through to the
    /// unchanged full scan in the caller.
    ///
    /// Measured (`resume_recorded_session_group_hint_avoids_scanning_every_
    /// other_group`, tests/orchestration/, #479): with 200 decoy groups on
    /// disk (300 audit lines each — the axis this test exercises), resuming
    /// via a correct group hint took 419ms on `resume_recorded_session`'s
    /// prior unconditional `session_roles()` call; with this fast path in
    /// its place, 42ms. Both numbers are debug-build, one dev machine — the
    /// point is the O(other groups) term this removes, not the absolute
    /// figures, and this axis (many groups, each modest) is not the only one
    /// that matters: this fast path still reads and parses the HINTED
    /// group's OWN full audit log (`merged_records` → `records_from_audit`,
    /// both rotation generations, bounded at ~16 MB by the 8 MB rotate cap)
    /// — a real, long-lived group with a near-cap audit log still pays that
    /// cost every resume, and the 42ms figure will not reproduce for it.
    /// What this fast path removes is strictly the OTHER groups' scans, not
    /// the resumed group's own. Those millisecond figures are a historical
    /// measurement, not what the test asserts: since #514 it pins the group
    /// COUNT (`group_record_scans_for_test`) and only prints the durations,
    /// because the wall-clock margin went flaky on loaded CI runners while
    /// the count never can.
    pub(in crate::orchestration) fn session_role_in_group(&self, group_id: &GroupId, session_id: &str) -> Option<SessionRole> {
        if !self.group_dir(group_id).join("group.json").is_file() {
            return None;
        }
        let live = self.group_is_live(group_id);
        let repo = self.load_group_file(group_id).map(|(repo, _)| repo);
        let pr_by_session: HashMap<String, String> = self
            .tasks(group_id)
            .into_iter()
            .filter_map(|t| Some((t.session?, t.pr?)))
            .collect();
        self.merged_records(group_id)
            .into_iter()
            .filter(|r| r.session.as_deref() == Some(session_id))
            .last()
            .map(|r| {
                let session = r.session.clone().unwrap_or_default();
                let pr = pr_by_session.get(&session).cloned();
                SessionRole {
                    session_id: session,
                    group_id: group_id.clone(),
                    role: r.role,
                    agent_name: r.name,
                    group_live: live,
                    task: r.task,
                    branch: r.branch,
                    repo,
                    pr,
                    forked_from: r.forked_from,
                }
            })
    }

    /// Load a group's persisted identity (repo + guardrails) from group.json.
    ///
    /// See [`read_blocks`] for how a pre-#222 group.json (flat per-role fields,
    /// no `blocks` array) is migrated to the block roster on read.
    #[doc(hidden)] // pub for integration tests (the #222 migration is asserted on this)
    pub fn load_group_file(&self, group: &GroupId) -> Option<(String, Guardrails)> {
        let v: Value =
            serde_json::from_str(&fs::read_to_string(self.group_dir(group).join("group.json")).ok()?).ok()?;
        let repo = v["repo"].as_str()?.to_string();
        let g = &v["guardrails"];
        let s = |k: &str, fb: &str| g[k].as_str().unwrap_or(fb).to_string();
        Some((
            repo,
            Guardrails {
                max_agents: g["max_agents"].as_u64().unwrap_or(4) as u32,
                agent_cli: s("agent_cli", "claude"),
                // The roster (#222). A group.json written by an older loomux has
                // no `blocks` array — only the eight flat per-role fields — so
                // rebuild the equivalent 4-block roster from those. That is the
                // whole migration: a pre-#222 group rejoins with exactly the CLIs
                // and models it was launched with.
                blocks: read_blocks(g),
                // The advanced-orchestrator toggle (#222). Absent → false: a
                // group launched before the toggle existed ran the built-in
                // roster, so that is what it rejoins as. This is also what makes
                // the toggle durable — a resumed orchestration (session browser)
                // rebuilds its guardrails from here, not from a launcher form.
                advanced_orchestrator: g["advanced_orchestrator"].as_bool().unwrap_or(false),
                // #1689: which workflow file this group runs. Absent (every
                // group.json written before named workflows) or empty → the
                // `default` name, which resolves to `.orrerix/workflow.yml` —
                // exactly the file that group has always been running.
                //
                // A value that is not a usable name falls back to `default`
                // rather than failing the load. That is the same posture
                // `agent_cli` takes two lines up and `clamped()` takes for a
                // hand-edited roster: a group.json somebody edited by hand must
                // still rejoin. The refusal that matters is the one at
                // `WorkflowName::parse`, which is what stops the string from
                // reaching a path join; nothing here is reachable without it.
                workflow: g["workflow"]
                    .as_str()
                    .and_then(|s| workflow::WorkflowName::parse(s).ok())
                    .unwrap_or_default(),
                auto_ops: g["auto_ops"].as_bool().unwrap_or(true),
                idle_kill_minutes: g["idle_kill_minutes"].as_u64().unwrap_or(0) as u32,
                max_spawns_per_hour: g["max_spawns_per_hour"].as_u64().unwrap_or(0) as u32,
                watchdog_stall_minutes: g["watchdog_stall_minutes"].as_u64().unwrap_or(0) as u32,
                // Autonomous token budget (#83) is a durable human choice, like
                // max_agents: absent in older group.json → 0 (no cap).
                autonomy_budget_tokens: g["autonomy_budget_tokens"].as_u64().unwrap_or(0),
                // Idle-tick window (#83): absent → 0 → clamped() maps to the default.
                idle_tick_minutes: g["idle_tick_minutes"].as_u64().unwrap_or(0) as u32,
                // Idle-tick activity floor (#83): absent → 0 → clamped() → default.
                idle_activity_floor_bytes: g["idle_activity_floor_bytes"].as_u64().unwrap_or(0),
                // #496 input-defer bound: absent → 0 → clamped() → default (15m).
                idle_tick_input_defer_max_minutes:
                    g["idle_tick_input_defer_max_minutes"].as_u64().unwrap_or(0) as u32,
                // Compact-nudge (#287): absent → 0 → off, exactly the conservative
                // default a group.json written before this field existed means.
                compact_nudge_minutes: g["compact_nudge_minutes"].as_u64().unwrap_or(0) as u32,
                compact_nudge_roles: g["compact_nudge_roles"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
                // Min-context floor (benchtest finding): absent → 0 → off.
                // Tri-state: absent/null → None (unset — the smart default
                // resolves at gate-evaluation time, not here).
                compact_nudge_min_context_percent:
                    g["compact_nudge_min_context_percent"].as_u64().map(|v| v as u32),
                // Context escalation: absent on a legacy group means the new 45% default;
                // a stored 0 remains the human's explicit off choice.
                compact_context_threshold_percent: g["compact_context_threshold_percent"]
                    .as_u64()
                    .unwrap_or(DEFAULT_COMPACT_CONTEXT_THRESHOLD_PERCENT as u64) as u32,
                // Context-window override (PR #329 round 7): absent → None →
                // defer entirely to the model-based guess, the conservative
                // default for a group.json written before this field existed.
                context_window_tokens_override: g["context_window_tokens_override"].as_u64(),
                // The resolved intake profile (#382 P1). Absent → the
                // built-in default, same migration guarantee as `blocks`.
                intake: read_intake(g),
                // Idle-tick intake gate (#332/#429): absent/null → None — the
                // smart default resolves at gate-evaluation time (not here),
                // matching `compact_nudge_min_context_percent`'s tri-state load
                // just above.
                intake_poll_minutes: g["intake_poll_minutes"].as_u64().map(|v| v as u32),
                // The fallback backstop: absent → 0 → clamped() maps to the default.
                idle_tick_fallback_minutes: g["idle_tick_fallback_minutes"].as_u64().unwrap_or(0) as u32,
                // #864: the backoff ceiling; absent → 0 → clamped() maps to the
                // default, so a pre-#864 group.json gets the backoff rather
                // than a 0 that would pin it to the base forever.
                idle_tick_fallback_max_minutes: g["idle_tick_fallback_max_minutes"].as_u64().unwrap_or(0) as u32,
            },
        ))
    }

    // ---------- groups & agents ----------

    /// Record that a resumed group's pinned roster no longer matches what the
    /// repo's workflow file now says (#222, rev-11 F2). Audit only — the pinned
    /// roster is what runs, deliberately.
    ///
    /// The comparison itself is [`roster_drift`], shared with the live `drift`
    /// field `workflow_status` publishes (#1689 slice B): a badge and an audit
    /// row that disagreed about whether a group had drifted would be two
    /// answers to one question, and the human would have no way to tell which
    /// was stale.
    pub(in crate::orchestration) fn audit_workflow_drift(&self, id: &GroupId, repo: &str, g: &Guardrails) {
        let Some(drift) = roster_drift(&load_active_workflow(repo, g), g) else { return };
        self.audit(id, brand::AUDIT_ACTOR, "workflow-changed-since-launch", json!({
            "path": active_workflow_path(repo, g),
            "note": drift.note,
            "running": g.blocks.iter().map(|b| b.id.clone()).collect::<Vec<_>>(),
            "on_disk": drift.on_disk,
            // Mirrors `running`/`on_disk` for the intake profile — null
            // `intake_on_disk` means the file is gone/broken, same as an
            // empty `on_disk` for blocks in those arms.
            "intake_running": intake_json(&g.intake),
            "intake_on_disk": drift.intake_on_disk.as_ref().map(intake_json),
            "action": "keeping the roster this group was launched with — relaunch to pick up the new one",
        }));
    }

    /// Every group id loomux currently has state for: the subdirectory names
    /// of the orchestration root, which IS the group registry on disk
    /// (`group_dir` = `root/<group>`).
    ///
    /// `None` means **the registry could not be read**, which is NOT the
    /// same as "there are no groups" and must never be flattened into it.
    /// Ownership in [`reclaim_group_agent_files`]
    /// (Self::reclaim_group_agent_files) is decided by asking which live
    /// group CLAIMS a file, so an empty list says "nothing claims anything"
    /// — which would dissolve the protection that stops `end_group("X")`
    /// deleting a live `X-extra` group's files. That is failing toward
    /// deletion, the one direction this must never fail in.
    pub(in crate::orchestration) fn existing_group_ids(&self) -> Option<Vec<GroupId>> {
        match fs::read_dir(&self.root) {
            Ok(entries) => Some(
                entries
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    // #904: a directory name is an id that arrives from OUTSIDE
                    // this process — anything at all can create a directory
                    // under the root — so it is parsed, not trusted, and an
                    // unparseable one is skipped rather than fed back into a
                    // join.
                    //
                    // Not "not a group loomux made" (rev-440 N3 corrected an
                    // earlier version of this comment that said so): the minter
                    // used to emit a leading-dash id for a repo directory named
                    // `-…`, so such a directory IS one of ours. The consequence
                    // is bounded and one-directional — the only consumer here is
                    // `reclaim_group_agent_files`, which is per-group and whose
                    // `None` path deletes nothing, so a skipped id loses its
                    // reclaim, never someone else's files. The same skip in
                    // `session_roles` costs such a group its rows in the session
                    // browser. Both are preferable to joining an id the
                    // validator refuses; neither is silent data loss.
                    .filter_map(|e| GroupId::parse(e.file_name().to_str()?).ok())
                    .collect(),
            ),
            // Any failure at all, missing root included — #464's own review
            // (B3) settled this rule for its sweep and it is adopted here
            // verbatim rather than second-guessed: "I could not list my
            // groups" must never be read as "I have no groups", because the
            // second widens the blast radius instead of narrowing it. One
            // rule, one behavior, both delete paths.
            Err(_) => None,
        }
    }
}
