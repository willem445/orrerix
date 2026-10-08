//! The group usage store (#3677): the in-memory image of `<group>/usage.json`
//! plus its overlay `<group>/usage-live.json`, the rule that folds one over the
//! other on load, the stamp that says whether the image still matches the disk,
//! and the decision of which file a merge has to write, if any.
//! Design note: `docs/design/usage-store.md`.
//!
//! Outbound edges (#888): `std::fs`, `serde_json`, `fsatomic::atomic_write` and
//! `budget::note_durable_write`. No `tauri`, `PtyManager` or `OrchRegistry`. It
//! reads `agentmodel` for the row type and calls no other sibling file; its one
//! caller is `registry/usage.rs`, which owns the lock every item here is used
//! under and the audit lines a load can owe.

use super::*;

/// A row for every key the group has, as one JSON array — the file an older
/// build wrote and still reads. Rewritten whole, and only when the SET of rows
/// changes or a row settles: a key seen for the first time, or a kill
/// snapshot. So it names every session the store knows, and holds each one's
/// figures as of the last such write.
pub const USAGE_FILE: &str = "usage.json";

/// The overlay: the rows whose figures have changed since [`USAGE_FILE`] was
/// last written whole, in the same row shape. It is what a usage tick writes
/// when an agent it already knows has spent, so that write is as large as the
/// rows that are moving and no larger. This store never adds a key to it that
/// [`USAGE_FILE`] lacks. An older build ignores it.
pub const USAGE_LIVE_FILE: &str = "usage-live.json";

/// What `fs::metadata` says about one of the store's two files — the cheap
/// evidence that the image in memory is still what the disk holds.
///
/// **What it can see, stated because a stamp is not a read.** It is the file's
/// length and modification time. A foreign rewrite that changes either is seen
/// on the next tick and the store reloads. One that preserves BOTH is not seen
/// at all, until the process restarts: that is the price of not reading the
/// file every tick, which is the cost this store exists to remove. No writer in
/// this repo produces such a rewrite, and `the_store_does_not_reread_a_file_whose_stamp_has_not_moved`
/// pins the blind spot so this paragraph cannot go stale quietly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::orchestration) enum FileStamp {
    Absent,
    Present { len: u64, modified: SystemTime },
}

/// The stamp of `path`, or `None` when it could not be taken. `None` never
/// equals anything, itself included (see [`UsageStore::matches_disk`]): "could
/// not look" must cost a reload, never pass for "unchanged".
fn stamp_of(path: &Path) -> Option<FileStamp> {
    match fs::metadata(path) {
        Ok(m) => m
            .modified()
            .ok()
            .map(|modified| FileStamp::Present { len: m.len(), modified }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(FileStamp::Absent),
        Err(_) => None,
    }
}

/// The stamp of a file this process has just written `len` bytes to. A length
/// that is not the one written means something else got there between the
/// write and this look, so the stamp is withheld and the next tick reloads.
fn stamp_written(path: &Path, len: usize) -> Option<FileStamp> {
    match stamp_of(path) {
        Some(FileStamp::Present { len: on_disk, modified }) if on_disk == len as u64 => {
            Some(FileStamp::Present { len: on_disk, modified })
        }
        _ => None,
    }
}

/// One group's usage rows, authoritative while its stamps match the disk.
///
/// **The invariant: loading the two files gives exactly `rows`**, apart from
/// the `updated_ms` of a row whose content has not changed since it was last
/// written (that field advances in memory on every tick and is persisted only
/// with a real change). Every path that could break it reloads instead: a
/// failed write drops the store, and a stamp that no longer matches the disk
/// is a reload before anything is merged.
pub(in crate::orchestration) struct UsageStore {
    /// Every row, live and historical, in file order. An `Arc` per row so the
    /// list a tick hands its caller is a vector of pointers, not a copy of
    /// every row the group has ever had.
    pub rows: Vec<Arc<UsageSnapshot>>,
    /// The keys whose row differs from what [`USAGE_FILE`] holds — the rows
    /// the next overlay write carries.
    pub overlay: HashSet<String>,
    base_stamp: Option<FileStamp>,
    live_stamp: Option<FileStamp>,
}

/// Every group's store, plus the test seam, behind `OrchRegistry::usage_lock`.
#[derive(Default)]
pub(in crate::orchestration) struct UsageStores {
    pub by_group: HashMap<GroupId, UsageStore>,
    /// Test-only: make every store write fail without touching the disk, so
    /// the failed-write path can be pinned on every platform. No portable
    /// filesystem trick fails a temp-file-plus-rename while leaving the reads
    /// beside it working. `false` in production.
    pub fail_writes: bool,
}

/// A file that held something other than usage rows and was moved aside.
pub(in crate::orchestration) struct UsagePreserved {
    pub file: &'static str,
    pub error: String,
    pub preserved: PathBuf,
}

pub(in crate::orchestration) struct UsageLoad {
    pub store: UsageStore,
    /// What the caller owes an audit line for.
    pub preserved: Vec<UsagePreserved>,
}

enum RowsRead {
    Absent,
    Rows(Vec<UsageSnapshot>),
    /// Read, and not a list of usage rows.
    Corrupt(String),
    /// Not read at all. Different from `Absent` and from `Corrupt`: nothing is
    /// known about what the file holds.
    Unreadable(String),
}

fn read_rows(path: &Path) -> RowsRead {
    match fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RowsRead::Absent,
        Err(e) => RowsRead::Unreadable(e.to_string()),
        // `from_slice`, so bytes that are not UTF-8 are a parse failure like
        // any other and take the preserve-as-`.bad` path below.
        Ok(bytes) => match serde_json::from_slice::<Vec<UsageSnapshot>>(&bytes) {
            Ok(rows) => RowsRead::Rows(rows),
            Err(e) => RowsRead::Corrupt(e.to_string()),
        },
    }
}

/// Move a file that could not be parsed aside as `<name>.bad`, so the rows it
/// held can be inspected instead of being overwritten by the next write.
fn preserve_corrupt(path: &Path, file: &'static str, error: String) -> UsagePreserved {
    let bad = path.with_extension("json.bad");
    // A durable REPLACE — it moves the live file aside — and the caller's
    // audit line after it is a tracked acquisition, so this is the exact
    // write-then-acquire shape a budget unwind tears (#1609 review B2).
    // Sealing here is what makes that audit line reachable.
    budget::note_durable_write(file);
    let _ = fs::rename(path, &bad);
    UsagePreserved { file, error, preserved: bad }
}

/// Fold the overlay's rows over the base file's, giving the store's row list
/// and the set of keys the overlay still owns.
///
/// **A row in both files is decided by `updated_ms`, newest wins, and the
/// overlay wins a tie.** In the ordinary case the overlay is the newer of the
/// two by construction: it only ever carries rows that changed after the base
/// was written, and `merge_usage_entry` advances `updated_ms` strictly on every
/// change. The comparison is what makes the two cases where that is not so come
/// out right:
///
/// - a crash between the whole-file write and the removal of the overlay it
///   made redundant leaves an overlay OLDER than the base, and applying it
///   would walk every row in it backwards;
/// - a build from before the overlay existed ran in between, refreshed a row
///   in the base, and never touched the overlay.
///
/// A row only the overlay has is a session the base has not seen yet, and is
/// appended. Pure.
pub(in crate::orchestration) fn fold_usage_overlay(
    base: Vec<UsageSnapshot>,
    live: Vec<UsageSnapshot>,
) -> (Vec<Arc<UsageSnapshot>>, HashSet<String>) {
    let mut rows: Vec<Arc<UsageSnapshot>> = base.into_iter().map(Arc::new).collect();
    // First occurrence per key, which is the row `merge_usage_entry`'s own
    // `find` would update.
    let mut index: HashMap<String, usize> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        index.entry(row.key.clone()).or_insert(i);
    }
    let mut overlay = HashSet::new();
    for row in live {
        match index.get(&row.key).copied() {
            Some(i) => {
                if row.updated_ms >= rows[i].updated_ms {
                    overlay.insert(row.key.clone());
                    rows[i] = Arc::new(row);
                }
            }
            None => {
                index.insert(row.key.clone(), rows.len());
                overlay.insert(row.key.clone());
                rows.push(Arc::new(row));
            }
        }
    }
    (rows, overlay)
}

/// Read a group's store off the disk.
///
/// `Err` means a file is there and could not be READ. That is not the same
/// fact as the file being absent, and the caller must not treat it as one: a
/// store defaulted to empty on a read that failed would be written back over
/// every row the group has on its next change (CLAUDE.md, "a multi-tenant
/// whole-file store never publishes from a handle it has not read"). So no
/// store comes back, nothing is cached, and the caller declines its write.
///
/// A file that reads and does not PARSE is a different case with a different
/// answer, unchanged from before this store existed: it is moved aside as
/// `.bad` and the store starts without it.
pub(in crate::orchestration) fn load_usage_store(dir: &Path) -> Result<UsageLoad, String> {
    let base_path = dir.join(USAGE_FILE);
    let live_path = dir.join(USAGE_LIVE_FILE);
    let mut preserved = Vec::new();

    // Each stamp is taken BEFORE its read. A write that lands between the two
    // then leaves the store holding newer rows under an older stamp, which the
    // next tick sees as a mismatch and reloads. The other order would record a
    // newer stamp over older rows, and nothing would ever notice.
    let mut base_stamp = stamp_of(&base_path);
    let base = match read_rows(&base_path) {
        RowsRead::Absent => Vec::new(), // normal: no usage yet
        RowsRead::Rows(rows) => rows,
        RowsRead::Unreadable(e) => return Err(format!("{USAGE_FILE}: {e}")),
        RowsRead::Corrupt(e) => {
            preserved.push(preserve_corrupt(&base_path, USAGE_FILE, e));
            base_stamp = stamp_of(&base_path);
            Vec::new()
        }
    };
    let mut live_stamp = stamp_of(&live_path);
    let live = match read_rows(&live_path) {
        RowsRead::Absent => Vec::new(), // normal: nothing has moved since the last whole write
        RowsRead::Rows(rows) => rows,
        RowsRead::Unreadable(e) => return Err(format!("{USAGE_LIVE_FILE}: {e}")),
        RowsRead::Corrupt(e) => {
            preserved.push(preserve_corrupt(&live_path, USAGE_LIVE_FILE, e));
            live_stamp = stamp_of(&live_path);
            Vec::new()
        }
    };

    let (rows, overlay) = fold_usage_overlay(base, live);
    Ok(UsageLoad {
        store: UsageStore { rows, overlay, base_stamp, live_stamp },
        preserved,
    })
}

/// What merging one reading did to the store's rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::orchestration) enum RowMerge {
    /// The row is there and nothing that persists moved.
    Unchanged,
    /// The row is there and its persisted content moved.
    Changed,
    /// No row had this key: one was added.
    Added,
}

/// Which file a merge has to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::orchestration) enum UsageWrite {
    /// Nothing that persists has changed.
    Nothing,
    /// The overlay alone: rows the base already has, with newer figures.
    Live,
    /// The whole base file, after which the overlay is redundant and removed.
    Whole,
}

/// Decide what a merge writes. Pure.
///
/// - `changed`: some row's persisted content moved in this merge (a row that
///   was added counts).
/// - `added`: the merge added a row for a key the store did not have.
/// - `settle`: the merge is a write from outside the usage tick (a kill
///   snapshot), so whatever is waiting in the overlay goes into the base now.
///   A dead agent's row has no later tick to carry it there.
/// - `overlay_waiting`: the overlay holds at least one row, AFTER the merge.
///
/// **A new key is a whole write, and that is what keeps `usage.json` complete**
/// (#3680 review B1). Were a first sighting written to the overlay like any
/// other change, a row would exist in `usage-live.json` alone until the next
/// agent ended — so `usage.json`, the one file an older build and any outside
/// reader open, would be missing every agent spawned since, and would not
/// exist at all in a group where nobody had ended yet. The cost is one whole
/// write per key that appears, the same cadence as one per agent that ends.
///
/// **Nothing else makes a tick write the whole file.** In particular a row
/// waiting in the overlay that this tick did not carry — one a previous
/// process left, or one a second instance of the app owns — stays there. An
/// earlier version folded such a row into the base on sight, and two instances
/// each with an agent spending in the same group then rewrote the whole file
/// against each other once a tick (#3680 review N1). The overlay cannot grow
/// without bound for the lack of that rule: it only ever takes keys the base
/// has, and the next new key or ended agent empties it.
pub(in crate::orchestration) fn plan_usage_write(
    changed: bool,
    added: bool,
    settle: bool,
    overlay_waiting: bool,
) -> UsageWrite {
    if !changed && !(settle && overlay_waiting) {
        return UsageWrite::Nothing;
    }
    if settle || (added && false) {
        UsageWrite::Whole
    } else {
        UsageWrite::Live
    }
}

/// Whether two rows would persist identically apart from `updated_ms` — the
/// question "did this tick change anything worth a write".
///
/// Decided on the SERIALIZED rows rather than field by field, so it reads
/// every field the file carries by construction: a field added to
/// `UsageSnapshot` later is compared from the commit that adds it, with no
/// second list to keep in step. A row that will not serialize answers `false`,
/// which costs a write rather than skipping one.
pub(in crate::orchestration) fn usage_rows_persist_alike(a: &UsageSnapshot, b: &UsageSnapshot) -> bool {
    fn shape(row: &UsageSnapshot) -> Option<Value> {
        let mut v = serde_json::to_value(row).ok()?;
        v.as_object_mut()?.remove("updated_ms");
        Some(v)
    }
    matches!((shape(a), shape(b)), (Some(x), Some(y)) if x == y)
}

impl UsageStore {
    /// Whether both files still carry the stamps this store last saw. A stamp
    /// that could not be taken — then or now — is a mismatch.
    pub fn matches_disk(&self, dir: &Path) -> bool {
        let same = |known: Option<FileStamp>, file: &str| {
            known.is_some() && known == stamp_of(&dir.join(file))
        };
        same(self.base_stamp, USAGE_FILE) && same(self.live_stamp, USAGE_LIVE_FILE)
    }

    /// Replace the overlay with the rows waiting in it. Compact JSON: nothing
    /// reads this file but a loader, and it is rewritten on every tick an
    /// agent spends in.
    pub fn write_live(&mut self, dir: &Path) -> std::io::Result<()> {
        let waiting: Vec<&UsageSnapshot> = self
            .rows
            .iter()
            .filter(|r| self.overlay.contains(&r.key))
            .map(|r| &**r)
            .collect();
        let body = serde_json::to_vec(&waiting)?;
        let path = dir.join(USAGE_LIVE_FILE);
        // Crash-safe: a crash mid-write leaves the previous overlay intact,
        // never a half-written one (#133).
        atomic_write(&path, &body)?;
        self.live_stamp = stamp_written(&path, body.len());
        Ok(())
    }

    /// Replace the base file with every row, then remove the overlay it has
    /// made redundant.
    ///
    /// **In that order, and the order is the crash argument.** A crash after
    /// the base write and before the removal leaves an overlay whose rows are
    /// all in the base already, at an `updated_ms` no newer than the base's —
    /// which [`fold_usage_overlay`] reads as the base. The other order would
    /// have a window in which the rows waiting in the overlay were in neither
    /// file. A removal that fails outright is the same state as that crash.
    pub fn write_whole(&mut self, dir: &Path) -> std::io::Result<()> {
        let all: Vec<&UsageSnapshot> = self.rows.iter().map(|r| &**r).collect();
        // Pretty-printed, as this file always has been: it is the one a human
        // opens, and it is no longer written on a cadence.
        let body = serde_json::to_vec_pretty(&all)?;
        let base_path = dir.join(USAGE_FILE);
        // Crash-safe: a crash mid-write leaves the old (valid) file intact,
        // never a half-written usage.json (#133).
        atomic_write(&base_path, &body)?;
        self.base_stamp = stamp_written(&base_path, body.len());
        self.overlay.clear();
        let live_path = dir.join(USAGE_LIVE_FILE);
        let _ = fs::remove_file(&live_path);
        self.live_stamp = stamp_of(&live_path);
        Ok(())
    }
}
