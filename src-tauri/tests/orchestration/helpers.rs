//! Shared fixtures for every topic module: `rails`, the registry constructors, `RealRepo`, `MqFake`, `scratch_dir`, the session-store fixtures, the three shared macros and `CAPTURE_SERIAL`.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

/// Wall-clock Unix-ms, mirroring the crate-private `now_ms` — the debounce
/// tests inject `now_ms() + window` into `flush_due_max_notices` to fire a
/// pending notice deterministically without sleeping out the real 3s window.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// The built-in 4-block roster on claude with the historic per-class models —
/// i.e. exactly what a plain launcher run produces (#222). Every block inherits
/// `agent_cli` and carries no persona, so nothing reaches a command line that
/// didn't before.
pub(crate) fn rails() -> Guardrails {
    Guardrails {
        max_agents: 2,
        agent_cli: "claude".into(),
        blocks: workflow::default_roster(&[
            (Role::Orchestrator, "", "opus"),
            (Role::Worker, "", "sonnet"),
            (Role::Reviewer, "", "sonnet"),
            (Role::Planner, "", "opus"),
        ]),
        auto_ops: false,
        idle_kill_minutes: 0,
        max_spawns_per_hour: 0,
        watchdog_stall_minutes: 0,
        ..Guardrails::default()
    }
}

/// Build a registry against `dir` with every test-only directory override
/// applied — the SAME seam every `OrchRegistry` construction in this file
/// must go through, not just the first one against a fresh temp root.
///
/// #464: a "relaunch" test (a second `OrchRegistry::new` pointed at the same
/// or a related state root, simulating loomux restarting) used to construct
/// the registry directly and skip this. `claude_agents_dir_override`/
/// `copilot_agents_dir_override` are in-memory fields on the `OrchRegistry`
/// instance, not persisted state, so a fresh instance that never calls
/// `set_claude_agents_dir_override` falls through to the REAL
/// `dirs::home_dir()/.claude/agents` — and a spawn or resume against it
/// (`create_orchestration_group`, `spawn_agent`, …) then writes a real
/// `loomux-<group>-<block>.md` file there. That is exactly how the orchestration
/// suite left 1,111 stray files under a dev's real `~/.claude/agents` and 161
/// under `~/.copilot/agents` (found live on this machine while fixing #464) —
/// disjoint from, and in addition to, the `%TEMP%` worktree-directory leak
/// (see `real_repo`). Route every registry construction through here so no
/// future one can reopen the gap.
pub(crate) fn relaunch_registry(dir: &Path) -> OrchRegistry {
    let reg = OrchRegistry::new(dir.to_path_buf());
    reg.set_port(45999); // fake port so config writing works
    reg.set_claude_agents_dir_override(dir.join("claude-agents"));
    reg.set_copilot_agents_dir_override(dir.join("copilot-agents"));
    reg.set_compact_hook_dir_override(dir.join("compacthook"));
    reg.set_copilot_hooks_dir_override(dir.join("copilot-hooks"));
    reg
}

pub(crate) fn test_registry() -> (OrchRegistry, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    (reg, dir)
}

/// #569: give `agent_id` a fake pane, then pause `group` — the pairing every
/// test that wants to observe a delivery's TEXT without a terminal now needs.
///
/// Pause was this suite's delivery mock for years: `deliver_prompt` destroyed
/// the payload, audited `prompt-suppressed-paused` carrying the full text, and
/// returned `Ok`, so pausing made a notice observable with no pty anywhere.
/// #569 option 2 replaced that discard with a queue admission — and a queue is
/// keyed by PANE, so the target needs a `pty_id` or the delivery fails with the
/// same "agent has no terminal yet" error an unpaused one would get. The text
/// is still recorded in full; it lands under the ordinary `prompt` action now,

/// One codex rollout under `<root>/<yyyy>/<mm>/<dd>/`, in the shape
/// `RolloutFileName::render` emits and with the `session_meta` first line
/// `RolloutRecorder::new` writes (#2515 C1).
///
/// The timestamp is a literal 19 characters because that is what the vendor's
/// own parser requires — it reads the id from offset 20 after checking for a
/// `-` at 19 — and `thread` is deliberately allowed to be a readable label
/// rather than a UUID: this repo's rule is looser than the vendor's about the
/// timestamp on purpose (`codex_rollout_thread_id`'s doc), and a fixture whose
/// ids read as words makes a contest assertion legible.
///
/// **No codex is ever run** (constraint 3): the format is read blob-by-blob out
/// of `openai/codex` at tag `rust-v0.153.4` and quoted in `docs/design/codex.md`.
pub(crate) fn write_codex_rollout(root: &Path, date: (&str, &str, &str), thread: &str, cwd: &str) -> PathBuf {
    let dir = root.join(date.0).join(date.1).join(date.2);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-09-05T10-00-00-{thread}.jsonl"));
    // `cwd` is interpolated into JSON, so a Windows path would need its
    // backslashes escaped. Every caller here passes a forward-slash path, and
    // that is not laziness: `norm_path` folds both separators, so the fixtures
    // exercise the comparison rather than the escaping — which is
    // `codex_profile_toml`'s job and is pinned in `tests/codexharness.rs`.
    assert!(!cwd.contains('\\'), "fixture cwds use forward slashes: {cwd}");
    let body = format!(
        "{{\"timestamp\":\"2026-09-05T10:00:00.000Z\",\"type\":\"session_meta\",\
         \"payload\":{{\"session_id\":\"{thread}\",\"id\":\"{thread}\",\
         \"timestamp\":\"2026-09-05T10:00:00.000Z\",\"cwd\":\"{cwd}\",\
         \"originator\":\"codex_cli_rs\",\"cli_version\":\"0.153.4\"}}}}\n"
    );
    std::fs::write(&path, body).unwrap();
    path
}

/// #904: the one constructor, in tests as in production — these suites drive
/// the refusal paths with ids that are deliberately not live groups.
pub(crate) fn parse_gid(s: &str) -> GroupId {
    GroupId::parse(s).unwrap()
}

/// which is what [`delivered_texts`] reads.
pub(crate) fn pause_with_pane(reg: &OrchRegistry, group: &GroupId, agent_id: &str, pty_id: u32) {
    reg.set_pty_for_test(agent_id, pty_id);
    reg.pause_group(group).unwrap();
}

/// Every prompt `group` delivered, in order, as text — the only way to observe
/// delivered wording in test mode, which has no real PTY to read back.
///
/// #569: the action filtered on is `prompt`, not `prompt-suppressed-paused`.
/// Pause queues rather than discards now, so a paused delivery audits exactly
/// what an unpaused one does; the pause is visible instead in the
/// `delivery-queued` line's `reason` (`group-paused`) and in the pane's queue.
/// Pausing still does not touch anything that happens BEFORE delivery (merge
/// grant minting, board writes), which is what makes it usable as a probe.
pub(crate) fn delivered_texts(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    reg.audit_log(group)
        .into_iter()
        .filter(|e| e.action == "prompt")
        .filter_map(|e| e.detail["text"].as_str().map(str::to_string))
        .collect()
}

/// A disposable git repo used as the worktree-cutting fixture's own root: the
/// repo lives one level BELOW a private temp root (`<root>/repo`), not AT the
/// temp root itself.
///
/// #464: `git_worktree_add` cuts a spawned agent's worktree to
/// `<repo's-parent>/<repo-name>-worktrees/<name>` — a directory SIBLING to
/// the repo, not inside it (see `git.rs::git_worktree_add`). When the repo
/// root itself was a bare `tempfile::tempdir()` (the pre-fix shape here),
/// that sibling landed directly under `%TEMP%`, outside the `TempDir`'s own
/// cleanup scope, and every worktree-cutting test that spawned a worker or
/// reviewer through the MCP surface (worktree defaults ON there, #338/#359)
/// leaked it on every passing run — not just on failure. 2,438 survivors
/// (growing to 2,702) were found in `%TEMP%` this way, and the disk they
/// filled crashed a live session.
///
/// Nesting the repo one level down means the sibling `-worktrees` directory
/// — and the `.git/worktrees/<name>` administrative registration inside the
/// repo's own `.git` that `git worktree add` also writes — are BOTH still
/// inside `_root`, so `_root`'s `Drop` (which runs on success, on assertion
/// failure, and through a panicking unwind alike) reclaims the whole tree in
/// one shot. No git-specific teardown call (`git worktree remove`, `git
/// worktree prune`) is needed, and none can be forgotten at a call site,
/// because there is nothing left outside the temp root for one to miss.
pub(crate) struct RealRepo {
    pub(crate) _root: tempfile::TempDir,
    pub(crate) repo: PathBuf,
}

impl RealRepo {
    pub(crate) fn path(&self) -> &Path {
        &self.repo
    }
    /// The fixture's own private temp root (`repo`'s parent) — everything
    /// this fixture creates, including any worktree `spawn_agent` cuts, lives
    /// under here. Test-only seam for asserting the whole tree is gone after
    /// drop (see the leak-regression tests below); nothing else needs it.
    pub(crate) fn root(&self) -> &Path {
        self._root.path()
    }
}

/// A minimal real git repo (one commit on the default branch). Needed by
/// #338 tests: a worker spawn's worktree now defaults ON at the MCP surface,
/// so an MCP-dispatched worker spawn needs real git under it to succeed —
/// the fake `"C:/tmp/repo"` path used elsewhere is only safe for tests that
/// spawn workers through the direct Rust API (`OrchRegistry::spawn_agent`),
/// which is untouched by this default and still takes a plain bool.
pub(crate) fn real_repo() -> RealRepo {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .expect("git must be installed for this test");
        assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    fs::write(repo.join("f.txt"), "hi").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    RealRepo { _root: root, repo }
}

/// Std-based scratch dir under the OS temp root — not `tempfile` (#412's own
/// new tests use this pattern deliberately, matching `tests/workflowfile.rs`/
/// `tests/lessonsfile.rs`), keyed by a tag plus this process's id so parallel
/// test runs never collide. The caller is responsible for cleanup.
pub(crate) fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("loomux-orch-test-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Fixture a claude session store entry: `<root>/<project>/<id>.jsonl` with a
/// `cwd` field, exactly the shape `find_session_cwd`'s claude half scans
/// (#412). `project` only needs to be SOME directory name — the scan is by
/// filename, never by reversing the munged project-dir name back to a path.
pub(crate) fn fixture_claude_session(root: &std::path::Path, id: &str, cwd: &str) {
    let proj = root.join("Some-Project-Dir");
    fs::create_dir_all(&proj).unwrap();
    fs::write(
        proj.join(format!("{id}.jsonl")),
        format!("{{\"type\":\"user\",\"cwd\":{cwd:?},\"message\":{{\"content\":\"hi\"}}}}\n"),
    )
    .unwrap();
}

/// Fixture a copilot session-state entry: `<root>/<anything>/workspace.yaml`
/// carrying `id:`/`cwd:` fields — matched by the PARSED id, never the
/// directory name (#412, mirroring `find_session_cwd`'s copilot half).
pub(crate) fn fixture_copilot_session(root: &std::path::Path, id: &str, cwd: &str) {
    let dir = root.join("some-session-dir");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("workspace.yaml"), format!("id: {id}\nname: fixture\ncwd: {cwd}\n")).unwrap();
}

/// The `session` DDL an opencode store is built from in this file, verbatim
/// from the one `opencodedb`'s module docs quote — read off
/// `anomalyco/opencode@f67e80c2` (tag `v1.18.11`), recorded in full on #722.
///
/// Kept as a literal here rather than shared with `tests/opencodesessions.rs`
/// and `tests/opencodeusage.rs`, which each carry their own copy for the same
/// reason: Rust compiles every integration test as its own crate, so there is
/// no cheaper place to put it than beside the tests that use it.
///
/// No opencode is ever run to produce one (constraint 3).
pub(crate) const OPENCODE_SESSION_DDL: &str = "CREATE TABLE session (
    id text PRIMARY KEY, project_id text NOT NULL, parent_id text,
    slug text NOT NULL, directory text NOT NULL, title text NOT NULL,
    version text NOT NULL, share_url text, permission text,
    time_created integer NOT NULL, time_updated integer NOT NULL,
    time_compacting integer, time_archived integer, workspace_id text,
    path text, agent text, model text,
    cost real DEFAULT 0 NOT NULL,
    tokens_input integer DEFAULT 0 NOT NULL,
    tokens_output integer DEFAULT 0 NOT NULL,
    tokens_reasoning integer DEFAULT 0 NOT NULL,
    tokens_cache_read integer DEFAULT 0 NOT NULL,
    tokens_cache_write integer DEFAULT 0 NOT NULL,
    metadata text)";

/// Open (creating) an opencode store at `db` in **WAL** — the journal mode
/// opencode opens its own database in (`SOURCE`, `database/database.ts`).
///
/// WAL is not a detail of the fixture: it is what makes
/// [`an_opencode_store_left_by_a_killed_process_still_resolves_the_session`]
/// able to leave a dirty `-wal` behind at all, and what
/// `opencodedb::open_readonly`'s "not `immutable=1`" decision is about.
pub(crate) fn open_opencode_store(db: &Path) -> rusqlite::Connection {
    fs::create_dir_all(db.parent().expect("an opencode db path always has a parent")).unwrap();
    let conn = rusqlite::Connection::open(db).unwrap();
    let mode: String =
        conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).unwrap();
    assert_eq!(mode, "wal", "the fixture store must really be in WAL, got {mode:?}");
    conn
}

/// One root `session` row: the columns these tests vary, and realistic
/// constants for the rest.
///
/// `directory` is spelled with forward slashes because that is how opencode
/// writes the column (`LOCAL-OBSERVED`), and `project_id` is the real sha1 for
/// this repo — every worktree of one repo shares it, which is exactly why
/// identification cannot key on the project (see `docs/design/opencode.md`).
pub(crate) fn insert_opencode_session(conn: &rusqlite::Connection, id: &str, directory: &str) {
    conn.execute(
        "INSERT INTO session (id, project_id, parent_id, slug, directory, title, version,
                              time_created, time_updated)
         VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?7)",
        rusqlite::params![
            id,
            "f9dd9fcdf18a51fa9de041f787210d1ce5e0d1e7",
            "loomux",
            directory,
            "a title",
            "1.18.11",
            1_785_703_307_950i64,
        ],
    )
    .unwrap();
}

/// A group's opencode store at `db` holding one root session per
/// `(id, directory)`, closed cleanly — the ordinary case.
pub(crate) fn fixture_opencode_store(db: &Path, rows: &[(&str, &str)]) {
    let conn = open_opencode_store(db);
    conn.execute_batch(OPENCODE_SESSION_DDL).unwrap();
    for (id, directory) in rows {
        insert_opencode_session(&conn, id, directory);
    }
}

/// `db` with a SQLite sidecar suffix appended (`-wal`, `-shm`) — appended to
/// the whole path, extension included, which is how SQLite names them.
pub(crate) fn sqlite_sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Copy one SQLite file through `read`+`write` rather than [`fs::copy`].
///
/// Deliberate on Windows: `fs::copy` is `CopyFileExW`, which opens the source
/// with a sharing mode that a live SQLite `-shm` mapping can refuse, while
/// `File::open` (what `fs::read` uses) asks for share-read/write/delete and
/// gets the bytes. The fixture below copies files out from under an OPEN
/// connection on purpose, so this is the difference between a fixture and a
/// sharing violation.
pub(crate) fn copy_sqlite_file(from: &Path, to: &Path) {
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::write(to, fs::read(from).unwrap()).unwrap();
}

// ---------- the merge queue's registry seam (#581 slice D2) ----------

/// A canned `MqRunner` for the reconcile seam. Minimal on purpose — the queue's
/// own behaviour is covered in `tests/mergequeue.rs`; what this file exists to
/// reach is the **registry path** around it (the once-only guard, `load_state`,
/// the audit emission, `store_state`), which no test touched until rev-163
/// pointed out that the door was connected to nothing.
///
/// Constraint 3: canned replies, so reconciling spawns no `git` and no `gh`.
pub(crate) struct MqFake {
    pub(crate) git: (i32, String),
    pub(crate) gh: (i32, String),
    pub(crate) calls: std::sync::Mutex<usize>,
}

impl loomux_lib::orchestration::mqdriver::MqRunner for MqFake {
    fn git(&self, _args: &[&str]) -> Result<loomux_lib::orchestration::mqdriver::CmdOut, String> {
        *self.calls.lock().unwrap() += 1;
        Ok(loomux_lib::orchestration::mqdriver::CmdOut {
            code: Some(self.git.0),
            stdout: self.git.1.clone(),
            stderr: String::new(),
        })
    }
    fn gh(&self, _args: &[&str]) -> Result<loomux_lib::orchestration::mqdriver::CmdOut, String> {
        *self.calls.lock().unwrap() += 1;
        Ok(loomux_lib::orchestration::mqdriver::CmdOut {
            code: Some(self.gh.0),
            stdout: self.gh.1.clone(),
            stderr: String::new(),
        })
    }
}
/// The pane's ledger state a `Pending` delivery leaves behind: recorded under
/// its own `submit_sent_ms`, unconfirmed. `record_inflight_delivery` is the
/// real writer for both that and the in-flight claim -- one shape, one
/// function, which is why the claim needs no new state. Written out at each
/// call site rather than behind a helper because `DeliveryOutcome` is private
/// (see `record_aborted_preenter_outcome`'s doc): the map's value type is only
/// ever reachable by inference from these functions' own signatures.
macro_rules! ledger_with {
    ($pty:expr, $submit_sent_ms:expr) => {{
        let m: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
        record_inflight_delivery(&m, $pty, $submit_sent_ms, "orrerix".to_string(), None);
        m
    }};
}

/// A pane carrying a stuck-prompt chip, wired the way the janitor actually
/// finds one: a real `PtyManager` with a fake child painting `$tail`, a ledger
/// record of the text we stranded there, and **no monitor thread anywhere**.
/// That absence is the fixture's whole point — every test below is the state a
/// pane is in after `LATE_MONITOR_MAX_LIFETIME` has taken its monitor away, and
/// before #825 M2 nothing looked at such a pane again, ever.
///
/// The record is back-dated a minute because `tier1_trusted` compares with
/// `<=`: a record and a keystroke stamped in the same millisecond would read as
/// "no human has typed since our submit", which is a fact about the test's
/// clock rather than about the pane.
///
/// Yields `(reg, tempdir, group, agent id, PtyManager, pty id, captured writes,
/// ledger)`. `captured` is kept by every caller so the safety line — this
/// mechanism only ever takes chips down or re-words them, and never writes to a
/// pane — is asserted rather than asserted-about.
macro_rules! janitor_pane {
    ($blocker:expr, $tail:expr) => {{
        let (reg, dir, g, wid) = attention_setup();
        let pty = 8251u32;
        let pm = PtyManager::default();
        let captured = pm.register_fake_for_test(pty, $tail.as_bytes());
        reg.set_pty_for_test(&wid, pty);
        reg.mark_stranded(&g, &wid, Some($blocker));
        let ledger: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
        record_stranded_outcome_at_for_test(
            &ledger,
            pty,
            "orrerix".to_string(),
            now_ms() - 60_000,
            Some(STRANDED_PROMPT.to_string()),
        );
        (reg, dir, g, wid, pm, pty, captured, ledger)
    }};
}

/// A pane whose stuck-prompt chip says the re-send was **refused**, produced the
/// way production produces one: fill the pane's queue to its cap, then let
/// `actuate_stranded`'s admission be rejected. Nothing is faked — the chip, its
/// audit trail and the queue depth are all the real ones, which is what makes
/// the drain below a real drain.
///
/// The ledger record is back-dated a minute for the reason `janitor_pane!`'s is,
/// and carries the sender so the re-admitted marker can be checked against it:
/// a marker that invented its own `from` would put a lie in the one line a human
/// greps to find out who sent the prompt that wedged (#560).
///
/// Yields `(reg, tempdir, group, agent id, pty id, ledger)`.
macro_rules! queuefull_pane {
    () => {{
        let (reg, dir, g, wid) = attention_setup();
        let pty = 8261u32;
        reg.set_pty_for_test(&wid, pty);
        for i in 0..queue::QUEUE_MAX_PER_PANE {
            reg.enqueue_text(
                &g, &wid, "orch", &format!("d-{i}"), pty, queue::EnqueueReason::BehindQueue,
            )
            .expect("the cap is the point — every one of these has to be admitted");
        }
        let healed = reg.actuate_stranded(&g, &wid, "orch", pty, StrandedAction::SelfHeal);
        assert!(!healed, "precondition: a marker refused at cap is not a heal");
        assert_eq!(
            reg.stranded_note(&wid).and_then(|n| n.blocker),
            Some(StrandedBlocker::QueueFull),
            "precondition: the refusal is what raises the chip this slice releases"
        );
        let ledger: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
        record_stranded_outcome_at_for_test(
            &ledger,
            pty,
            "orch".to_string(),
            now_ms() - 60_000,
            Some(STRANDED_PROMPT.to_string()),
        );
        (reg, dir, g, wid, pty, ledger)
    }};
}

/// The reader backlog is process-wide state, and this binary runs its tests
/// in parallel — so every test that consults or fills it takes this first.
/// Without it, the ceiling test's seeding would make a concurrent capture
/// test fail with the backlog error, and the failure would be real but
/// meaningless. (`obs.rs`'s own `static SERIAL` precedent.)
static CAPTURE_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn capture_lock() -> std::sync::MutexGuard<'static, ()> {
    CAPTURE_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}
