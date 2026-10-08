//! The group usage store (#3677): what a usage tick writes, what it leaves
//! alone, and what survives a restart, a kill and an older build's file.
//!
//! One module of the `orchestration` integration-test target (`main.rs`).
//! Layout rules: docs/design/module-layout.md. Design:
//! docs/design/usage-store.md.
//!
//! **How "wrote" and "did not write" are observed.** Off the filesystem, never
//! off a counter in the code under test: a file's length, a digest of its bytes
//! and its modification time, before and after. Every store write is a temp file renamed into
//! place, so a write that happened is a file whose modification time moved
//! even where its bytes came out the same.

use super::*;

/// A row list exactly as a store written by v1.3.0 holds it: no
/// `current_model`, no `activity`. See `fixtures/usagestore/README.md`.
const USAGE_V1_3_0: &str = include_str!("../fixtures/usagestore/usage-v1.3.0.json");
/// The same rows as v1.3.1-beta7 wrote them, cut from a real store.
const USAGE_V1_3_1_BETA7: &str = include_str!("../fixtures/usagestore/usage-v1.3.1-beta7.json");
/// Summed off the fixture by a separate script, not by the code under test:
/// the four token counters of its seven rows.
const FIXTURE_TOKENS: u64 = 10_528_902;
const FIXTURE_COST: f64 = 2.5809207900000004;
const FIXTURE_ROWS: usize = 7;

/// What is on disk at a path. A digest rather than the bytes themselves so a
/// failed comparison prints three numbers, not a few hundred kilobytes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OnDisk {
    len: usize,
    digest: u64,
    modified: SystemTime,
}

/// The file at `path`, or `None` when nothing is there.
fn file_state(path: &Path) -> Option<OnDisk> {
    use std::hash::{Hash, Hasher};
    let bytes = fs::read(path).ok()?;
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    Some(OnDisk { len: bytes.len(), digest: h.finish(), modified })
}

/// Both of the store's files at once.
fn store_state(dir: &Path) -> (Option<OnDisk>, Option<OnDisk>) {
    (file_state(&dir.join("usage.json")), file_state(&dir.join("usage-live.json")))
}

fn rows_in(path: &Path) -> Vec<UsageSnapshot> {
    serde_json::from_slice(&fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .unwrap_or_else(|e| panic!("{} is not a row list: {e}", path.display()))
}

fn row_tokens(r: &UsageSnapshot) -> u64 {
    r.input_tokens + r.output_tokens + r.cache_creation_tokens + r.cache_read_tokens
}

/// One more assistant turn on a claude transcript, APPENDED the way the CLI
/// writes one. Each turn needs its own `id`: the reader counts a message once.
fn spend(proj: &Path, sid: &str, id: &str, input: u64, output: u64) {
    use std::io::Write;
    let encoded = proj.join("C--tmp-repo");
    fs::create_dir_all(&encoded).unwrap();
    let line = json!({"type":"assistant","message":{"id":id,"model":"claude-opus-4-8",
        "usage":{"input_tokens":input,"output_tokens":output,
                 "cache_creation_input_tokens":0,"cache_read_input_tokens":0}}});
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(encoded.join(format!("{sid}.jsonl")))
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

/// A group with one live claude worker whose session has spent 1,500 tokens,
/// ticked until its row has stopped changing.
///
/// TWO ticks, and the second is not padding: the first sights the row, and the
/// second records the baseline the cache-age fold measures growth from (#3407)
/// — a real change to what persists. Only from the third tick on is "nothing
/// moved" true of the row.
fn settled_worker(reg: &OrchRegistry, proj: &Path) -> (GroupId, String, String, PathBuf) {
    reg.set_claude_projects_dir(proj.to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let sid = w.session_id.clone().expect("a claude worker gets a session id");
    spend(proj, &sid, "m1", 1000, 500);
    assert_eq!(reg.group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(1500));
    assert_eq!(reg.group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(1500));
    let dir = reg.state_root().join(g.id.as_str());
    (g.id, w.id, sid, dir)
}

/// #3677 acceptance 1. The store used to be rewritten whole on every tick,
/// because `updated_ms` — which moves whenever the tick runs — was part of
/// what it wrote.
#[test]
fn an_unchanged_usage_tick_writes_nothing_and_a_changed_one_does() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, d) = test_registry();
    let (g, _w, sid, dir) = settled_worker(&reg, proj.path());

    let before = store_state(&dir);
    assert!(
        before.0.is_some() || before.1.is_some(),
        "positive control: the row is on disk, so `unchanged` below is a statement about a \
         file that exists rather than about one nothing ever wrote"
    );

    // Let the wall clock move. A tick that still wrote would now write a
    // different `updated_ms`, so it cannot hide behind identical bytes.
    std::thread::sleep(Duration::from_millis(30));
    for _ in 0..3 {
        let u = reg.group_usage(&g);
        assert_eq!(u["lifetime_tokens"].as_u64(), Some(1500), "the tick still answers");
    }
    assert_eq!(
        store_state(&dir),
        before,
        "#3677: a tick on which no figure moved wrote the usage store anyway"
    );

    // The positive control the absence above needs: a row that DID move is
    // written, and what was written is the new figure.
    spend(proj.path(), &sid, "m2", 1000, 500);
    assert_eq!(reg.group_usage(&g)["lifetime_tokens"].as_u64(), Some(3000));
    assert_ne!(store_state(&dir), before, "a changed row must be written");
    let restarted = relaunch_registry(d.path());
    assert_eq!(
        restarted.group_usage(&g)["lifetime_tokens"].as_u64(),
        Some(3000),
        "and a process that only has the disk reads the new figure off it"
    );
}

/// #3677 acceptance 2. One agent spending must not cost a rewrite of every
/// row the group has ever had.
#[test]
fn a_tick_with_one_agent_spending_does_not_rewrite_the_historical_rows() {
    const DEAD: usize = 400;
    let proj = tempfile::tempdir().unwrap();
    let (reg, d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(&dir).unwrap();
    // The historical rows, the way an older build left them: one pretty-printed
    // `usage.json` and nothing else.
    let dead: Vec<UsageSnapshot> = (0..DEAD)
        .map(|i| usage_snap(&format!("sess-dead-{i}"), &format!("w-dead-{i}"), 0.5, 1000, 0))
        .collect();
    fs::write(dir.join("usage.json"), serde_json::to_string_pretty(&dead).unwrap()).unwrap();
    let historical = file_state(&dir.join("usage.json")).unwrap();

    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let sid = w.session_id.clone().unwrap();
    let mut live = 0u64;
    for turn in 1..=5u64 {
        spend(proj.path(), &sid, &format!("m{turn}"), 100 * turn, 50);
        live += 100 * turn + 50;
        assert_eq!(
            reg.group_usage(&g.id)["lifetime_tokens"].as_u64(),
            Some(DEAD as u64 * 1000 + live),
            "positive control: the tick counts every historical row AND the live row's new \
             figure, so the store it did not rewrite is one it really is serving"
        );
    }

    assert_eq!(
        file_state(&dir.join("usage.json")).as_ref(),
        Some(&historical),
        "#3677: a tick rewrote the file holding the {DEAD} historical rows"
    );
    let overlay = rows_in(&dir.join("usage-live.json"));
    assert_eq!(
        overlay.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
        vec![sid.as_str()],
        "what a tick writes is the rows that moved: the one live agent's, and none of the \
         {DEAD} it did not touch"
    );
    assert_eq!(row_tokens(&overlay[0]), live);

    // Nothing is lost by the figures being split across two files.
    let restarted = relaunch_registry(d.path());
    let after = restarted.group_usage(&g.id);
    assert_eq!(after["lifetime_tokens"].as_u64(), Some(DEAD as u64 * 1000 + live));
    assert_eq!(after["agents"].as_array().unwrap().len(), DEAD + 1);
}

/// #3677 acceptance 2, the PARSE half, and the store's one blind spot pinned
/// as a blind spot.
///
/// The store trusts the rows it holds while each file's length and
/// modification time stand still, and does not open the file to check. So a
/// rewrite that preserves both is not seen — which is the same fact as "a tick
/// no longer reads and parses the file". The design note states this residual;
/// this is what fails if it stops being true in either direction.
#[test]
fn the_store_does_not_reread_a_file_whose_stamp_has_not_moved() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.upsert_usage_snapshot(&g.id, usage_snap("sess-a", "w-1", 0.50, 100, 200));
    assert_eq!(reg.group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(300));

    let path = reg.state_root().join(g.id.as_str()).join("usage.json");
    let stamp = fs::metadata(&path).unwrap().modified().unwrap();
    let text = fs::read_to_string(&path).unwrap();
    let forged = text.replace("\"input_tokens\": 100", "\"input_tokens\": 999");
    assert_ne!(forged, text, "the edit landed");
    assert_eq!(forged.len(), text.len(), "at the same length");
    fs::write(&path, &forged).unwrap();
    fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(stamp).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().modified().unwrap(),
        stamp,
        "and under the same modification time"
    );

    assert_eq!(
        reg.group_usage(&g.id)["lifetime_tokens"].as_u64(),
        Some(300),
        "the file was read again on a tick (1199 is the forged figure)"
    );
}

/// The other side of the blind spot, and the reason the store may be trusted
/// at all: every reader it replaced re-read the file each tick, so a change
/// made by anything else — a second process, a hand edit, a restore — was seen
/// within one. It still is, whenever the stamp moves.
#[test]
fn a_rewrite_from_outside_the_store_is_picked_up_when_the_stamp_moves() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.upsert_usage_snapshot(&g.id, usage_snap("sess-a", "w-1", 0.50, 100, 200));
    assert_eq!(reg.group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(300));
    let dir = reg.state_root().join(g.id.as_str());
    let path = dir.join("usage.json");

    // A different LENGTH: seen whatever the clock's resolution is.
    let mut rows = rows_in(&path);
    rows.push(usage_snap("sess-foreign", "w-foreign", 0.25, 4000, 0));
    fs::write(&path, serde_json::to_string_pretty(&rows).unwrap()).unwrap();
    assert_eq!(
        reg.group_usage(&g.id)["lifetime_tokens"].as_u64(),
        Some(4300),
        "a row another writer added must be counted on the next read"
    );

    // The same length, a later modification time: a counter that changed
    // digits without changing width, which is what a second writer's ordinary
    // tick looks like.
    std::thread::sleep(Duration::from_millis(50));
    let text = fs::read_to_string(&path).unwrap();
    let edited = text.replace("\"input_tokens\": 4000", "\"input_tokens\": 5000");
    assert_ne!(edited, text);
    assert_eq!(edited.len(), text.len());
    fs::write(&path, &edited).unwrap();
    assert_eq!(reg.group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(5300));

    // And the overlay is watched the same way, though nothing had written one.
    let foreign = vec![usage_snap("sess-overlay", "w-overlay", 0.1, 10_000, 0)];
    fs::write(dir.join("usage-live.json"), serde_json::to_string(&foreign).unwrap()).unwrap();
    assert_eq!(reg.group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(15_300));
}

/// #3677 acceptance 3. A killed agent has no later tick, so its final figures
/// go into `usage.json` itself — the file every build reads — and not into the
/// overlay.
#[test]
fn a_kill_snapshot_lands_in_usage_json_and_survives_a_restart() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, d) = test_registry();
    let (g, w, sid, dir) = settled_worker(&reg, proj.path());
    spend(proj.path(), &sid, "m2", 1000, 500);
    assert_eq!(reg.group_usage(&g)["lifetime_tokens"].as_u64(), Some(3000));
    assert!(
        rows_in(&dir.join("usage-live.json")).iter().any(|r| r.key == sid),
        "fixture: before the kill the live row is in the overlay, so the assertions below \
         are about a row that had to be MOVED"
    );

    // Spend no tick ever saw: the kill snapshot is the only thing that can
    // capture it.
    spend(proj.path(), &sid, "m3", 2000, 1000);
    reg.mark_dead(&w, Some(0));

    let base = rows_in(&dir.join("usage.json"));
    let row = base.iter().find(|r| r.key == sid).expect("the dead agent's row is in usage.json");
    assert_eq!(row_tokens(row), 6000, "with its FINAL figures, not the last tick's");
    assert_eq!(
        file_state(&dir.join("usage-live.json")),
        None,
        "and the overlay, now redundant, is gone rather than left holding a dead agent's row"
    );

    let restarted = relaunch_registry(d.path());
    assert_eq!(restarted.group_usage(&g)["lifetime_tokens"].as_u64(), Some(6000));
}

/// #3677 acceptance 3. A process that stops without killing its agents (a
/// crash, or simply closing the app) leaves the live rows in the overlay
/// only. The next process must count them.
#[test]
fn lifetime_totals_after_a_restart_equal_what_they_were_before_it() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, d) = test_registry();
    let (g, _w, sid, dir) = settled_worker(&reg, proj.path());
    // History that IS in usage.json, so the total spans both files.
    reg.upsert_usage_snapshot(&g, usage_snap("sess-old", "w-old", 1.25, 1000, 2000));
    spend(proj.path(), &sid, "m2", 700, 300);
    let before = reg.group_usage(&g);
    assert_eq!(before["lifetime_tokens"].as_u64(), Some(1500 + 3000 + 1000));

    let in_base: u64 = rows_in(&dir.join("usage.json"))
        .iter()
        .filter(|r| r.key == sid)
        .map(row_tokens)
        .sum();
    assert_eq!(
        in_base, 1500,
        "fixture: usage.json holds the live row as of the last whole write, 1,000 tokens \
         behind — so the equality below holds only if the overlay is loaded over it"
    );

    let restarted = relaunch_registry(d.path());
    let after = restarted.group_usage(&g);
    assert_eq!(after["lifetime_tokens"], before["lifetime_tokens"]);
    assert_eq!(after["agents"].as_array().unwrap().len(), before["agents"].as_array().unwrap().len());
    let cost = |v: &Value| v["lifetime_cost_usd"].as_f64().expect("a dollar total");
    assert!(
        (cost(&after) - cost(&before)).abs() < 1e-9,
        "dollars too: {} before, {} after",
        cost(&before),
        cost(&after)
    );
}

/// How a row present in BOTH files is decided when the store is loaded: the
/// newer `updated_ms` wins, and the overlay wins a tie.
///
/// The fixture is the collision — one key, two different figures — in each
/// direction. Three wrong rules give three different totals here, which is
/// what makes the pin fail-able: the overlay always winning reads 3,700 in the
/// first case, the overlay being ignored reads 5,000, and the correct fold
/// reads 5,700.
#[test]
fn a_row_in_both_files_is_decided_by_which_is_newer() {
    let stamped = |key: &str, input: u64, at: u64| UsageSnapshot {
        updated_ms: at,
        ..usage_snap(key, "w-1", 0.5, input, 0)
    };
    let load = |base: Vec<UsageSnapshot>, overlay: Vec<UsageSnapshot>| -> u64 {
        let (reg, _d) = test_registry();
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        let dir = reg.state_root().join(g.id.as_str());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("usage.json"), serde_json::to_string_pretty(&base).unwrap()).unwrap();
        fs::write(dir.join("usage-live.json"), serde_json::to_string(&overlay).unwrap()).unwrap();
        reg.group_usage(&g.id)["lifetime_tokens"].as_u64().unwrap()
    };

    // A STALE overlay: the process died between writing usage.json whole and
    // removing the overlay that write made redundant — or an older build
    // refreshed the row in usage.json and never knew the overlay was there.
    assert_eq!(
        load(
            vec![stamped("sess-k", 5000, 200)],
            vec![stamped("sess-k", 3000, 100), stamped("sess-only-overlay", 700, 50)],
        ),
        5700,
        "an overlay row older than usage.json's must not walk the row backwards, and a row \
         only the overlay has is still counted"
    );
    // The ordinary case: the overlay is what moved since the whole write.
    assert_eq!(load(vec![stamped("sess-k", 3000, 100)], vec![stamped("sess-k", 5000, 200)]), 5000);
    // A tie goes to the overlay, which in the ordinary case is the later write.
    assert_eq!(load(vec![stamped("sess-k", 3000, 100)], vec![stamped("sess-k", 5000, 100)]), 5000);
}

/// The overlay holds live rows only. A row in it that no live agent owns — here
/// one a previous process left behind when it stopped — is folded into
/// `usage.json` on the next tick that writes at all, so the overlay cannot
/// grow by a row per restart.
#[test]
fn an_overlay_row_with_no_live_owner_is_folded_into_usage_json() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(&dir).unwrap();
    let orphan = vec![usage_snap("sess-gone", "w-gone", 0.5, 9000, 0)];
    fs::write(dir.join("usage-live.json"), serde_json::to_string(&orphan).unwrap()).unwrap();

    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let sid = w.session_id.clone().unwrap();
    spend(proj.path(), &sid, "m1", 1000, 500);
    assert_eq!(reg.group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(10_500));

    let mut keys: Vec<String> = rows_in(&dir.join("usage.json")).into_iter().map(|r| r.key).collect();
    keys.sort();
    let mut want = vec!["sess-gone".to_string(), sid.clone()];
    want.sort();
    assert_eq!(keys, want, "the orphaned row moved into usage.json, with the live one");
    assert_eq!(file_state(&dir.join("usage-live.json")), None, "and the overlay was emptied");

    // From here the overlay is live rows only again.
    assert_eq!(reg.group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(10_500));
    spend(proj.path(), &sid, "m2", 1000, 500);
    assert_eq!(reg.group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(12_000));
    assert_eq!(
        rows_in(&dir.join("usage-live.json")).iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
        vec![sid.as_str()]
    );
}

/// A store that could not be READ is not an empty store. Before #3677 a read
/// that failed was taken for "no usage yet", and the tick's own rows were then
/// written over whatever the unread file held.
///
/// `usage.json` is made a directory because that is the one obstacle that
/// fails a read the same way on every platform CI runs.
#[test]
fn a_usage_store_that_cannot_be_read_is_not_written_over() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(dir.join("usage.json")).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let sid = w.session_id.clone().unwrap();
    spend(proj.path(), &sid, "m1", 1000, 500);

    for _ in 0..2 {
        assert_eq!(
            reg.group_usage(&g.id)["lifetime_tokens"].as_u64(),
            Some(1500),
            "the tick still reports its own reading"
        );
    }
    assert!(dir.join("usage.json").is_dir(), "the unread store was left exactly as it was");
    assert_eq!(file_state(&dir.join("usage-live.json")), None, "and nothing was written beside it");
    let reported: Vec<AuditEntry> = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "poll-read-failed" && e.detail["reader"] == "usage-store")
        .collect();
    assert_eq!(reported.len(), 1, "said once, not once per tick");

    // The obstacle goes away and the store works again, with nothing to undo.
    fs::remove_dir(dir.join("usage.json")).unwrap();
    assert_eq!(reg.group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(1500));
    assert_eq!(relaunch_registry(d.path()).group_usage(&g.id)["lifetime_tokens"].as_u64(), Some(1500));
}

/// A `usage.json` whose bytes are not UTF-8 is corrupt, and gets the same
/// treatment as one that is not JSON: preserved as `.bad`. It used to fail the
/// READ instead, which the loader took for "absent" — so the next write
/// replaced it and nothing kept a copy.
#[test]
fn a_usage_json_that_is_not_utf8_is_preserved_not_overwritten() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(&dir).unwrap();
    let garbage: &[u8] = &[0xff, 0xfe, b'[', b'{', 0xc3, 0x28, b']'];
    fs::write(dir.join("usage.json"), garbage).unwrap();

    reg.upsert_usage_snapshot(&g.id, usage_snap("sess-b", "w-2", 1.25, 300, 400));

    assert_eq!(
        fs::read(dir.join("usage.json.bad")).ok().as_deref(),
        Some(garbage),
        "the unreadable bytes must be kept for inspection"
    );
    let rows = rows_in(&dir.join("usage.json"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, "sess-b");
}

/// #3677 acceptance 4. The file an older build wrote is the file this build
/// reads: `usage.json`'s shape did not change. Loading it loses nothing, does
/// not rewrite it, and a later write keeps every row it held.
fn an_older_builds_usage_json_loads_whole(fixture: &str, has_activity: bool) {
    let (reg, d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("usage.json"), fixture).unwrap();
    let before = store_state(&dir);

    let usage = reg.group_usage(&g.id);
    let agents = usage["agents"].as_array().unwrap();
    assert_eq!(agents.len(), FIXTURE_ROWS, "every row loads");
    assert_eq!(usage["lifetime_tokens"].as_u64(), Some(FIXTURE_TOKENS));
    let cost = usage["lifetime_cost_usd"].as_f64().unwrap();
    assert!((cost - FIXTURE_COST).abs() < 1e-9, "lifetime dollars: {cost}");
    assert_eq!(usage["lifetime_cost_basis"], "mixed", "estimated and reported rows are both there");

    // The row that carries a wake in the beta7 file, and none in v1.3.0's.
    let row = agents.iter().find(|a| a["id"] == "rev-3694").expect("the claude reviewer's row");
    assert_eq!(row["tokens"]["total"].as_u64(), Some(1_579_929));
    assert_eq!(row["block"], "rev-std");
    if has_activity {
        assert_eq!(row["last_active_ms"].as_u64(), Some(1_791_426_230_688));
        assert_eq!(row["last_wake"]["at_ms"].as_u64(), Some(1_791_426_211_516));
        assert_eq!(row["last_wake"]["cache_read_tokens"].as_u64(), Some(111_536));
    } else {
        assert_eq!(row["last_active_ms"], Value::Null, "a row from before #3407 reads as unknown");
        assert_eq!(row["last_wake"], Value::Null);
    }
    // A row written before `block`/`cli` existed.
    let old = agents.iter().find(|a| a["id"] == "w-4").expect("the pre-block worker row");
    assert_eq!(old["block"], "");
    assert_eq!(old["tokens"]["total"].as_u64(), Some(1_586_156));

    assert_eq!(store_state(&dir), before, "reading an older build's file does not rewrite it");

    // The first write the new build makes keeps everything the file held.
    reg.upsert_usage_snapshot(&g.id, usage_snap("sess-new", "w-new", 1.0, 40, 60));
    let rows = rows_in(&dir.join("usage.json"));
    assert_eq!(rows.len(), FIXTURE_ROWS + 1);
    assert_eq!(rows.iter().map(row_tokens).sum::<u64>(), FIXTURE_TOKENS + 100);
    let kept = rows.iter().find(|r| r.agent_id == "rev-3694").unwrap();
    assert_eq!(
        kept.activity.last_active_ms,
        has_activity.then_some(1_791_426_230_688),
        "a row the write did not touch goes back out as it came in"
    );
    assert_eq!(
        relaunch_registry(d.path()).group_usage(&g.id)["lifetime_tokens"].as_u64(),
        Some(FIXTURE_TOKENS + 100)
    );
}

#[test]
fn a_usage_json_written_by_v1_3_0_loads_whole_and_is_not_rewritten_by_the_read() {
    assert!(
        !USAGE_V1_3_0.contains("activity") && !USAGE_V1_3_0.contains("current_model"),
        "fixture: this is the shape from BEFORE those two fields existed"
    );
    an_older_builds_usage_json_loads_whole(USAGE_V1_3_0, false);
}

#[test]
fn a_usage_json_written_by_v1_3_1_beta7_loads_whole_and_is_not_rewritten_by_the_read() {
    assert!(
        USAGE_V1_3_1_BETA7.contains("\"baseline\"") && USAGE_V1_3_1_BETA7.contains("current_model"),
        "fixture: this is the shape that carries both"
    );
    an_older_builds_usage_json_loads_whole(USAGE_V1_3_1_BETA7, true);
}

/// What an OLDER build sees of a store this build wrote, which is the other
/// direction of acceptance 4: `usage.json` is still a plain row list it can
/// parse, holding every row, with a live row as of the last whole write.
#[test]
fn usage_json_stays_a_row_list_an_older_build_can_read() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    let (g, _w, sid, dir) = settled_worker(&reg, proj.path());
    reg.upsert_usage_snapshot(&g, usage_snap("sess-old", "w-old", 1.25, 1000, 2000));
    spend(proj.path(), &sid, "m2", 700, 300);
    assert_eq!(reg.group_usage(&g)["lifetime_tokens"].as_u64(), Some(5500));

    // Read the way v1.3.0 read it: `usage.json` alone, as a flat array of
    // objects with these keys.
    let text = fs::read_to_string(dir.join("usage.json")).unwrap();
    let rows: Vec<Value> = serde_json::from_str(&text).expect("a JSON array");
    assert_eq!(rows.len(), 2, "the settled row and the live one");
    for key in [
        "key", "agent_id", "name", "role", "source", "input_tokens", "output_tokens",
        "cache_creation_tokens", "cache_read_tokens", "cost_usd", "estimated", "model", "updated_ms",
    ] {
        assert!(rows.iter().all(|r| r.get(key).is_some()), "every row carries `{key}`");
    }
    let seen: u64 = rows_in(&dir.join("usage.json")).iter().map(row_tokens).sum();
    assert_eq!(
        seen, 4500,
        "an older build is behind by exactly what moved since the last whole write (1,000 \
         here), never by a whole row"
    );
}
