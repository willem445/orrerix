//! The audit log on disk: its rotation, `append_audit` and the durable append,
//! the per-agent ledger and usage-series lines, and the `AuditEntry` parser.
//! Design note: `docs/design/durability-and-disk.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): `lock_safe`
//! (`crate::obs`). IO: fs, threads/sleep. Sibling files it calls:
//! `grouppath.rs`.

use super::*;

/// Size cap after which the audit log rolls over to `audit.1.jsonl` (one
/// generation kept). Full prompt texts land in the audit, so it grows fast.
const AUDIT_ROTATE_BYTES: u64 = 8 * 1024 * 1024;

/// Serializes every in-process audit writer — appends *and* rotation — against
/// each other (#240). Two guarantees hang off it: no thread holds an append
/// handle across another thread's rotation rename, and two threads can't both
/// decide to rotate (the second rename would discard the generation the first
/// just created). Uncontended in practice — an append is a few hundred bytes
/// every few seconds — and held only for the open+write, never across
/// orchestration work, so it can't meaningfully block a pane. `lock_safe`
/// keeps a poisoned lock from turning best-effort auditing into a panic
/// cascade (see `obs::LockExt`).
static AUDIT_LOCK: std::sync::OnceLock<TrackedMutex<()>> = std::sync::OnceLock::new();

/// [`AUDIT_LOCK`], initialised on first use.
///
/// A getter rather than a `static TrackedMutex` because registering a lock with
/// the watchdog is not a `const` operation — and it must be registered, or the
/// one lock every refusal path takes under three other registry locks would be
/// the one lock a hold report cannot name. Same `OnceLock::get_or_init` shape
/// `obs::data_root` uses.
pub(in crate::orchestration) fn audit_lock() -> &'static TrackedMutex<()> {
    AUDIT_LOCK.get_or_init(|| TrackedMutex::new_ranked("audit", lockorder::AUDIT, ()))
}

thread_local! {
    /// Test-only seam (#240): how long *this thread* pauses between rotation's
    /// size check and its rename. Rotation is check-then-rename, and the window
    /// between the two is a few instructions wide — too narrow for a test to
    /// force a second rotator into it, which is why the lock's rotation-race
    /// protection would otherwise ship unverified. Widening the window on demand
    /// makes the race a real reproducer (see
    /// `concurrent_rotations_keep_the_retained_generation`).
    ///
    /// Zero in production, and read only when a rotation actually fires (an 8 MB
    /// rollover), so the production path pays one thread-local read per rollover
    /// and nothing else. Thread-local rather than a global so it can't leak into
    /// the other tests cargo runs in parallel in this process. Mirrors the
    /// existing `set_claude_projects_dir` test seam.
    static ROTATE_CHECK_PAUSE: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

/// Widen this thread's rotation check-to-rename window. Test-only (see
/// `ROTATE_CHECK_PAUSE`); production never calls it, so the window stays as
/// narrow as the code makes it.
#[doc(hidden)] // pub for integration tests
pub fn set_rotate_check_pause_for_test(pause: Duration) {
    ROTATE_CHECK_PAUSE.with(|p| p.set(pause));
}

/// Roll `audit.jsonl` over to `audit.1.jsonl` once it exceeds `cap`.
/// Factored out so the threshold behavior is testable with a tiny cap.
#[doc(hidden)] // pub for integration tests
pub fn rotate_audit_if_needed(dir: &Path, cap: u64) {
    let _guard = audit_lock().lock_safe();
    rotate_audit_locked(dir, cap);
}

/// Rotation body. Callers must already hold `AUDIT_LOCK` — `append_audit` takes
/// it once and covers rotate+append with a single acquisition (the lock is not
/// reentrant).
///
/// A *cross-process* writer (the gh/git shims' `>>`) can still open the log a
/// moment before this rename and write through the handle afterwards. That's
/// accepted, not a defect: the handle keeps pointing at the same file, so the
/// line lands at the tail of `audit.1.jsonl` instead of the fresh `audit.jsonl`
/// — never lost, and the viewer reads both generations (`audit_log`). Only its
/// position in the timeline shifts, and only for a record that raced an 8 MB
/// rollover.
fn rotate_audit_locked(dir: &Path, cap: u64) {
    let path = dir.join("audit.jsonl");
    if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > cap {
        // Check-then-rename: the size we just read is only still true because
        // `AUDIT_LOCK` is held. Without it a second rotator could pass this same
        // check, wait out the first one's rename, and then rename the *fresh*
        // log over `audit.1.jsonl` — discarding the generation the first just
        // retained. `ROTATE_CHECK_PAUSE` (zero outside tests) widens exactly
        // this window so that race can be reproduced rather than argued.
        let pause = ROTATE_CHECK_PAUSE.with(|p| p.get());
        if !pause.is_zero() {
            std::thread::sleep(pause);
        }
        let _ = fs::rename(&path, dir.join("audit.1.jsonl")); // replaces the old generation
    }
}

/// Append `bytes` to an append-only durable file and **fsync it** (#547).
///
/// Two differences from `append_audit`, both deliberate:
///
/// - **It fsyncs.** The audit log is best-effort history; the staged-orphan
///   archive holds the only remaining copy of payloads that were just removed
///   from `queue.json`, so it has to be at least as durable as the snapshot
///   it took them out of — which `atomic_write` fsyncs.
/// - **It reports failure** instead of swallowing it. A failed append means
///   the caller must NOT go on to remove the entries from staging; see
///   `archive_staged_overflow`'s ordering argument.
///
/// The single-`write_all` rule is `append_audit`'s rule 1 and applies here for
/// the same reason: append atomicity is per syscall, so a batch of records has
/// to reach the OS as one buffer or a concurrent writer can be scheduled into
/// the middle of it. The caller assembles the whole batch.
pub(in crate::orchestration) fn append_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Audit-log writer usable from background threads (delivery outcomes)
/// without holding a registry reference.
///
/// Appends are atomic *per record*, which the sibling `atomic_write` does not
/// give you — that one makes whole-file *replaces* crash-safe (#133), a
/// different failure mode. Two rules keep a record whole (#240):
///
/// 1. **One buffer, one `write_all`.** The record and its newline are serialized
///    up front and handed to the OS in a single call. Append-mode atomicity is
///    per write *syscall*, so a record emitted as many writes is a record other
///    writers can be scheduled into the middle of. The old code wrote
///    `writeln!(f, "{line}")` with `line` a `serde_json::Value`: `Display` walks
///    the tree and emits a write per token, and concurrent writers (mass
///    agent-exit at shutdown, delivery threads) spliced each other character by
///    character — real logs ended up with
///    `{{""actionaction""::""agent-exitagent-exit""`.
///
///    Precisely: `write_all` *loops* on a short write, and each iteration is its
///    own append — so the atomicity rests on the file not short-writing, not on
///    a contract. For a regular file on our baselines (Windows, Linux) a
///    blocking write of a record-sized buffer is issued as one write and returns
///    complete or fails; short writes are a pipe/socket/`ENOSPC` behavior. That
///    is the practice this relies on, and it is worth restating rather than
///    claiming a guarantee the API doesn't make: audit records can be large
///    (full prompt texts land here).
/// 2. **`AUDIT_LOCK` for in-process writers**, so appends don't race rotation.
///
/// The *other* writers are the gh/git shims (`gh_shim_sh`, `git_shim_sh`), in
/// other processes and beyond any mutex of ours. They rely on rule 1 alone, and
/// satisfy it the same way: one `printf` of one whole line, appended with `>>`.
/// Any shim audit line must stay a single `printf`; building a line across two
/// redirections would reintroduce exactly this bug across processes.
///
/// #904: the group id arrives here **already validated** — the parameter is a
/// [`GroupId`], and the path is built by [`group_dir_at`], the one assembly
/// point. There is no check in this function and there is deliberately nothing
/// to check: a caller cannot construct an id that would escape.
///
/// It was worth the type. This function is `root.join(group)` plus
/// `create_dir_all`, so before #904 a `..` component wrote the audit log
/// outside the orchestration root entirely — demonstrated on CI, not
/// theorized. Auditing is best-effort by contract (see the doc's opening
/// line) and returns `()`, so there was nowhere to put a refusal even once one
/// was possible; making the argument unforgeable is what removed the need.
pub(in crate::orchestration) fn append_audit(root: &Path, group: &GroupId, actor: &str, action: &str, detail: Value) {
    let dir = group_dir_at(root, group);
    let record = json!({ "ts_ms": now_ms(), "actor": actor, "action": action, "detail": detail });
    let mut line = record.to_string();
    line.push('\n'); // newline in the same buffer — a separate write could be split off
    // Serialize before taking the lock: JSON formatting is the expensive part
    // and no other writer cares about it.
    let _ = fs::create_dir_all(&dir);
    let _guard = audit_lock().lock_safe(); // covers rotate + append as one unit
    rotate_audit_locked(&dir, AUDIT_ROTATE_BYTES);
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(dir.join("audit.jsonl")) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Directive ledger (#329 expansion): append one line to a pane's ledger
/// file. Same single-`write_all` rule as `append_audit` (rule 1 in its doc)
/// is what makes the append atomic — but unlike `audit.jsonl`, a ledger file
/// has exactly one writer (the owning agent, self-scoped via `note_directive`)
/// and no rotation, so there is no second process or rotate-vs-append race to
/// guard against and no need for `AUDIT_LOCK`-style serialization.
pub(in crate::orchestration) fn append_ledger_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut buf = line.to_string();
    buf.push('\n');
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(&buf.into_bytes())
}

/// The usage series' file name inside a group directory (#2011 slice B).
///
/// A **persisted schema** — `docs/design/token-charts.md` is its contract. It is
/// append-only, has **exactly one writer at a time** (see
/// [`OrchRegistry::series_sample`]: the usage tick, whichever thread is running
/// it, serialized per group by the usage memo cell), is **never rotated** and
/// is **never rebuilt** from `audit.jsonl` or a
/// transcript. Rotation is what the audit log needs and what this file must not
/// have: a rotated series loses the baseline every reader differences against,
/// and the whole point of cumulative rows is that a reader can lose one and
/// still be right.
pub const USAGE_SERIES_FILE: &str = "usage-series.jsonl";

/// The size at which reading `usage-series.jsonl` whole stops being obviously
/// cheap, and the read starts saying so (#2941 review).
///
/// **A revisit trigger, not a limit.** Nothing rotates or compacts this file,
/// so it grows with the calendar: at roughly 288 rows per day per moving key
/// and ~300 bytes a row, a busy group is single-digit MB per month — fine at a
/// 30 s poll — and a group left open for a year is not. The failure mode of
/// leaving that unstated is that nobody can tell when it has arrived, because
/// the read gets gradually slower and never says anything.
///
/// 32 MB is deliberately well past where anything hurts (it is four times the
/// `AUDIT_ROTATE_BYTES` the audit log rotates at) and well short of where a
/// 30 s poll would stall. Crossing it sets `oversize` on the payload; it never
/// shortens the answer. When it does start firing, the fix is one of the two
/// this slice consciously deferred: seek to `since_ms` rather than filter, or
/// compact. See `docs/design/token-charts.md`.
pub const SERIES_REVISIT_BYTES: u64 = 32 * 1024 * 1024;

/// The hard ceiling on a `usage-series.jsonl` read (#3469): four times the
/// revisit trigger above. Unlike that trigger this one **is** a refusal — past
/// it the chart read returns its degrade (`Null`, a skipped tick) and audits
/// `poll-read-failed` rather than buffering the file — because a file four
/// times past "revisit this" is the case where holding it whole on a 30 s poll
/// is the hazard, not the answer. The fail-soft mechanism for every size under
/// it is the fallible reservation in [`loomux_engine::boundedread`].
pub const SERIES_READ_LIMIT_BYTES: u64 = 4 * SERIES_REVISIT_BYTES;

/// Append one row to a group's `usage-series.jsonl`.
///
/// Delegates to [`append_ledger_line`] rather than reimplementing its
/// single-`write_all` append: the properties are the same and the reasons are
/// the same. One writer, no rotation, so no `AUDIT_LOCK`-style serialization —
/// see that function's doc for why that combination is what makes the append
/// atomic without a lock.
pub(in crate::orchestration) fn append_series_line(dir: &Path, row: &usageseries::SeriesRow) -> std::io::Result<()> {
    let line = serde_json::to_string(row)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    append_ledger_line(&dir.join(USAGE_SERIES_FILE), &line)
}

/// One parsed audit-log line, for the in-app timeline viewer. Mirrors the
/// shape written by `append_audit`; `detail` stays an opaque JSON value so the
/// frontend can render per-action without the backend knowing every schema.
#[derive(Clone, Debug, Serialize)]
pub struct AuditEntry {
    pub ts_ms: u64,
    pub actor: String,
    pub action: String,
    pub detail: Value,
}

/// Parse audit JSONL text into entries, in file order (oldest first), skipping
/// malformed lines. Pure so ordering/robustness is testable without touching
/// the filesystem or a registry.
#[doc(hidden)] // pub for integration tests
pub fn parse_audit_lines(text: &str) -> Vec<AuditEntry> {
    parse_audit_lines_counted(text).0
}

/// Same, but also reports how many non-blank lines failed to parse. Skipping
/// silently is how #240 stayed invisible for so long: a corrupt log read as a
/// slightly shorter timeline, with nothing anywhere saying lines had been
/// dropped. Blank lines don't count — a torn tail or a trailing newline is
/// normal; unparseable *content* is not.
#[doc(hidden)] // pub for integration tests
pub fn parse_audit_lines_counted(text: &str) -> (Vec<AuditEntry>, usize) {
    let mut skipped = 0usize;
    let entries = text
        .lines()
        .filter_map(|line| match parse_audit_line(line)? {
            Ok(e) => Some(e),
            Err(()) => {
                skipped += 1;
                None
            }
        })
        .collect();
    (entries, skipped)
}

/// One audit line: `None` for a blank one (not a fault — a trailing newline is
/// normal), `Some(Err(()))` for one that will not parse, which the callers
/// count. Shared by the whole-text parser above and the windowed reader, so
/// the two cannot disagree about what a line means.
pub(in crate::orchestration) fn parse_audit_line(line: &str) -> Option<Result<AuditEntry, ()>> {
    if line.trim().is_empty() {
        return None;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Some(Err(()));
    };
    Some(Ok(AuditEntry {
        ts_ms: v["ts_ms"].as_u64().unwrap_or(0),
        actor: v["actor"].as_str().unwrap_or("").to_string(),
        action: v["action"].as_str().unwrap_or("").to_string(),
        detail: v.get("detail").cloned().unwrap_or(Value::Null),
    }))
}

/// Upper bound on entries returned to the viewer: the audit grows fast (full
/// prompt texts) and only the most recent slice is worth rendering. Keeps the
/// payload bounded even against a rotated + current pair near the 8 MB cap.
#[doc(hidden)] // pub for integration tests
pub const AUDIT_VIEW_LIMIT: usize = 5000;

/// Per-file ceiling on an audit-window read (#3469): four rotations' worth.
/// Rotation keeps each generation near [`AUDIT_ROTATE_BYTES`], so a file past
/// this means rotation itself is broken — and the window read then reports
/// that rather than buffering an unbounded file on a poll path. A **sanity
/// cap**, not the fail-soft mechanism: that is the fallible reservation in
/// [`loomux_engine::boundedread`], which covers every size under it too.
pub const AUDIT_READ_LIMIT_BYTES: u64 = 4 * AUDIT_ROTATE_BYTES;
