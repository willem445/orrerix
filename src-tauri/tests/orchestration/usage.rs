//! Workflow selection at launch, the usage series, tuning fingerprints and bounded reads.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── #1689 slice D1: the launcher pins WHICH workflow a group runs ───────────

/// A repo declaring two workflows: `default` (`.orrerix/workflow.yml`) and one named
/// file under `.orrerix/workflows/`. Each declares a reviewer block nothing else
/// declares, so which file was read is decided by the roster rather than by a name
/// that could have been copied around without the file ever being opened.
fn repo_with_two_workflows(repo: &Path) -> String {
    let cfg = repo.join(".orrerix");
    fs::create_dir_all(cfg.join("workflows")).unwrap();
    fs::write(
        cfg.join("workflow.yml"),
        "version: 1\nblocks:\n  - id: rev-from-default\n    kind: reviewer\n",
    )
    .unwrap();
    fs::write(
        cfg.join("workflows").join("b.yml"),
        "version: 1\nblocks:\n  - id: rev-from-b\n    kind: reviewer\n",
    )
    .unwrap();
    repo.to_string_lossy().replace('\\', "/")
}

/// `create_orchestration`'s argument list with everything but the two fields under test
/// held at the launcher's own defaults, so a call site reads as the one thing it varies.
#[allow(clippy::too_many_arguments)]
pub(crate) fn launch_with_workflow(
    reg: &std::sync::Arc<OrchRegistry>,
    repo: &str,
    advanced: bool,
    workflow: Option<&str>,
) -> Result<SpawnRequest, String> {
    create_orchestration_sync(
        reg,
        repo.to_string(),
        None,
        2,
        "claude".into(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        false,
        0,
        0,
        0,
        advanced,
        workflow.map(str::to_string),
        None,
    )
}

#[test]
fn the_launcher_pins_which_workflow_a_group_runs_and_persists_the_name() {
    use std::sync::Arc;
    // The consent moment is the launch, and #1689 gives the human a second thing to
    // consent to: not only "run this repo's roster" but "run THIS ONE of its rosters".
    // The pin has to reach both the live roster and `group.json`, or a resume comes back
    // on a different workflow than the one that was launched.
    let state = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let repo_path = repo_with_two_workflows(repo.path());
    let reg = Arc::new(relaunch_registry(state.path()));

    let launched = launch_with_workflow(&reg, &repo_path, true, Some("b")).expect("launch");
    let gid = launched.group_id.clone();
    let rails = reg.group(&gid).unwrap().guardrails;
    assert!(
        rails.block("rev-from-b").is_some(),
        "the named file is the one that was read"
    );
    assert!(
        rails.block("rev-from-default").is_none(),
        "and the default file is NOT — a pin that reads both files pins nothing"
    );
    assert_eq!(rails.workflow.as_str(), "b");

    // Persisted beside the roster it produced, in the same write, so a resume cannot come
    // back on a name that disagrees with the blocks.
    let disk: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(reg.state_root().join(gid.as_str()).join("group.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(disk["guardrails"]["workflow"], serde_json::json!("b"));
}

#[test]
fn a_launch_that_names_no_workflow_runs_default_exactly_as_it_always_did() {
    use std::sync::Arc;
    // The whole compatibility claim, as a test: an omitted argument is `default`, which is
    // `.orrerix/workflow.yml` — the pre-#1689 launch, for every caller that has not learned
    // to ask and for every repo that declares one file.
    let state = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let repo_path = repo_with_two_workflows(repo.path());
    let reg = Arc::new(relaunch_registry(state.path()));

    let launched = launch_with_workflow(&reg, &repo_path, true, None).expect("launch");
    let rails = reg.group(&launched.group_id).unwrap().guardrails;
    assert!(rails.block("rev-from-default").is_some(), "no name means the default file");
    assert!(rails.block("rev-from-b").is_none());
    assert_eq!(rails.workflow.as_str(), "default");

    // Explicitly naming `default` is the SAME answer, not a second code path — the
    // property that makes the omission a default rather than a special case.
    let state2 = tempfile::tempdir().unwrap();
    let reg2 = Arc::new(relaunch_registry(state2.path()));
    let named = launch_with_workflow(&reg2, &repo_path, true, Some("default")).expect("launch");
    let rails2 = reg2.group(&named.group_id).unwrap().guardrails;
    assert!(rails2.block("rev-from-default").is_some());
    assert_eq!(rails2.workflow.as_str(), "default");
}

#[test]
fn a_workflow_name_that_is_not_one_refuses_the_launch_and_creates_nothing() {
    use std::sync::Arc;
    // CLAUDE.md constraint 6: the name becomes `<name>.yml` under a directory, so it is a
    // path component and is validated as one — refused, never rewritten. Refused rather
    // than defaulted, too: a LAUNCH has a human in front of it, and silently running some
    // other workflow than the one they picked is the failure this check exists to avoid.
    let state = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let repo_path = repo_with_two_workflows(repo.path());
    let reg = Arc::new(relaunch_registry(state.path()));

    for bad in ["../b", "a/b", "CON", "-b", "", "b.yml"] {
        let err = launch_with_workflow(&reg, &repo_path, true, Some(bad))
            .expect_err(&format!("{bad:?} must be refused"));
        assert!(
            err.contains("not a usable workflow name"),
            "{bad:?} refused with the wrong message: {err}"
        );
    }

    // The negative control the refusal is worth nothing without: nothing was created. A
    // group dir would mean the check ran BELOW the point where state is minted, which is
    // the shape a later refactor could reintroduce without any assertion above noticing.
    let created: Vec<String> = fs::read_dir(reg.state_root())
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(created.is_empty(), "a refused launch created {created:?}");

    // …and the positive control for THAT: the same call with a usable name does create one,
    // so "creates nothing" cannot pass by the launch never working at all.
    launch_with_workflow(&reg, &repo_path, true, Some("b")).expect("a usable name still launches");
    assert!(fs::read_dir(reg.state_root()).unwrap().flatten().any(|e| e.path().is_dir()));
}

#[test]
fn the_workflow_name_is_recorded_with_the_toggle_off_and_is_inert_until_it_is_on() {
    use std::sync::Arc;
    // The two are different questions and the docs say so: the toggle is the CONSENT to run
    // a repo-authored roster at all, the name answers only WHICH file. Recording the name
    // with the toggle off is what makes turning it on live come back to the workflow the
    // human chose at launch rather than to `default`.
    let state = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let repo_path = repo_with_two_workflows(repo.path());
    let reg = Arc::new(relaunch_registry(state.path()));

    let launched = launch_with_workflow(&reg, &repo_path, false, Some("b")).expect("launch");
    let rails = reg.group(&launched.group_id).unwrap().guardrails;
    assert_eq!(rails.workflow.as_str(), "b", "the name is recorded");
    assert!(
        rails.block("rev-from-b").is_none(),
        "and is inert: the toggle is off, so no repo-authored roster runs"
    );
    assert!(rails.block("orchestrator").is_some(), "the built-in roster runs instead");
}

// ===================================================================
// #2011 slice B — the persisted usage series, its sampler, its read
// command and the tuning fingerprint behind the marks.
// ===================================================================

/// Rails with `n` agents allowed and an extra worker block running `cli`.
fn rails_with_second_worker_cli(max_agents: u32, block_id: &str, cli: &str) -> Guardrails {
    let mut r = rails();
    r.max_agents = max_agents;
    let mut extra = r
        .blocks
        .iter()
        .find(|b| b.kind == Role::Worker)
        .expect("the built-in roster has a worker")
        .clone();
    extra.id = block_id.to_string();
    extra.name = block_id.to_string();
    extra.cli = cli.to_string();
    r.blocks.push(extra);
    r
}

/// Write a Claude transcript for `sid` under `proj`, with those token counts.
fn write_claude_transcript(proj: &Path, sid: &str, input: u64, output: u64) {
    let encoded = proj.join("C--tmp-repo");
    fs::create_dir_all(&encoded).unwrap();
    let text = format!(
        "{}\n{}\n",
        json!({"type":"user","message":{"content":"hi"}}),
        json!({"type":"assistant","message":{"id":format!("m{input}-{output}"),
            "model":"claude-opus-4-8",
            "usage":{"input_tokens":input,"output_tokens":output,
                     "cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}),
    );
    fs::write(encoded.join(format!("{sid}.jsonl")), text).unwrap();
}

fn series_lines(reg: &OrchRegistry, g: &GroupId) -> Vec<String> {
    let p = reg.state_root().join(g.as_str()).join("usage-series.jsonl");
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

#[test]
fn a_usage_tick_appends_one_series_row_per_moved_key() {
    // The whole sampler, driven through the real tick: `group_usage` refreshes
    // each live agent's snapshot, merges it, and samples. Nothing here calls
    // the sampler directly — a test that did would pass with the call site
    // deleted, which is the one defect that matters most about a hook on a
    // hot path.
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    // A zero bucket means "the spacing has always elapsed", so the MOVED half
    // of the predicate is the only one left deciding — which is exactly what
    // this test is about. The spacing half is pinned in the engine's own unit
    // tests, where five minutes can pass for free.
    reg.set_series_bucket_ms(0);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let sid = w.session_id.clone().unwrap();

    // Tick 1: nothing has been counted yet, so there is no data point.
    reg.group_usage(&g.id);
    assert!(
        series_lines(&reg, &g.id).is_empty(),
        "a zero-usage agent must not seed the series with a row"
    );

    // Tick 2: the transcript now has usage.
    write_claude_transcript(proj.path(), &sid, 1000, 500);
    reg.group_usage(&g.id);
    let after_first = series_lines(&reg, &g.id);
    assert_eq!(after_first.len(), 1, "exactly one row for the one moved key: {after_first:?}");
    let row: serde_json::Value = serde_json::from_str(&after_first[0]).unwrap();
    assert_eq!(row["kind"], "sample");
    assert_eq!(row["key"], sid.as_str(), "keyed by the CLI session, as usage.json is");
    assert_eq!(row["agent"], w.id.as_str());
    assert_eq!(row["block"], "worker", "the block, not just the capability class");
    assert_eq!(row["cli"], "claude");
    assert_eq!(row["in"].as_u64(), Some(1000));
    assert_eq!(row["out"].as_u64(), Some(500));
    assert_eq!(row["source"], "transcript");

    // Tick 3: nothing moved. The bucket has elapsed (it is zero), so ONLY the
    // moved half can refuse this row — and it must.
    reg.group_usage(&g.id);
    assert_eq!(
        series_lines(&reg, &g.id).len(),
        1,
        "an idle tick must append nothing, however long the bucket has been up"
    );

    // Tick 4: it moved again. Cumulative, not a delta — a reader differences
    // them, so a row that never landed costs resolution and never spend.
    write_claude_transcript(proj.path(), &sid, 4000, 900);
    reg.group_usage(&g.id);
    let rows = series_lines(&reg, &g.id);
    assert_eq!(rows.len(), 2, "the moved tick appends: {rows:?}");
    let second: serde_json::Value = serde_json::from_str(&rows[1]).unwrap();
    assert_eq!(second["in"].as_u64(), Some(4000), "cumulative, not the 3000 delta");
    assert_eq!(second["out"].as_u64(), Some(900));
}

/// Append one Claude assistant line for `sid` under `proj`, on `model`.
///
/// Appends rather than rewrites: the usage tick reads through a byte cursor,
/// and an append is the shape a live transcript actually takes.
fn append_claude_turn(proj: &Path, sid: &str, id: &str, model: &str, input: u64, output: u64) {
    use std::io::Write;
    let encoded = proj.join("C--tmp-repo");
    fs::create_dir_all(&encoded).unwrap();
    let line = json!({"type":"assistant","message":{"id":id,"model":model,
        "usage":{"input_tokens":input,"output_tokens":output,
                 "cache_creation_input_tokens":0,"cache_read_input_tokens":0}}});
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(encoded.join(format!("{sid}.jsonl")))
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

#[test]
fn a_claude_series_sample_carries_the_current_model_not_the_priced_one() {
    // #3415: the token chart splits spend by model and marks a switch off each
    // sample's `model`. On claude the usage snapshot's `model` is a PRICING
    // pick — the model of the single largest message — so a pane that switched
    // after a long first answer would keep sampling the old model. This drives
    // the real tick (as the test above does) through exactly that session and
    // asserts the row written AT the switch tick names the new model, while the
    // usage panel's "priced against" figure is left as it was.
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    reg.set_series_bucket_ms(0);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let sid = w.session_id.clone().unwrap();

    // Tick 1: the largest message of the whole session, on fable.
    append_claude_turn(proj.path(), &sid, "f1", "claude-fable-5-1", 10, 900);
    reg.group_usage(&g.id);
    // Tick 2: the switch. Opus never out-writes fable's 900.
    append_claude_turn(proj.path(), &sid, "o1", "claude-opus-5-5", 10, 100);
    let usage = reg.group_usage(&g.id);

    let rows = series_lines(&reg, &g.id);
    assert_eq!(rows.len(), 2, "one row per moved tick: {rows:?}");
    let first: serde_json::Value = serde_json::from_str(&rows[0]).unwrap();
    let second: serde_json::Value = serde_json::from_str(&rows[1]).unwrap();
    assert_eq!(first["model"], "claude-fable-5-1", "before the switch");
    assert_eq!(
        second["model"], "claude-opus-5-5",
        "the sample at the switch tick names the model the pane is now on"
    );

    let agent = usage["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == w.id.as_str())
        .expect("the live agent has a usage row");
    assert_eq!(
        agent["model"], "claude-fable-5-1",
        "the usage panel's priced-against model is still the pricing pick"
    );
}

#[test]
fn a_dead_agents_frozen_snapshot_is_never_resampled() {
    // `merge_usage_snapshots` returns live AND historical rows, so the sampler
    // is handed snapshots whose counters can never move again. Without the
    // live-key filter each of those would look "moved" to a fresh process
    // (there is no previous row in memory) and write one duplicate row per app
    // restart, forever.
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    reg.set_series_bucket_ms(0);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let sid = w.session_id.clone().unwrap();
    write_claude_transcript(proj.path(), &sid, 1000, 500);
    reg.group_usage(&g.id);
    assert_eq!(series_lines(&reg, &g.id).len(), 1, "the live tick sampled");

    reg.mark_dead(&w.id, Some(0));
    // The snapshot survives in usage.json — that is the #42 guarantee — and is
    // still returned by the merge on every later tick.
    let usage = reg.group_usage(&g.id);
    assert_eq!(usage["lifetime_tokens"].as_u64(), Some(1500), "the spend is still counted");
    assert_eq!(
        series_lines(&reg, &g.id).len(),
        1,
        "but it is not re-sampled: a frozen counter is not a new data point"
    );

    // **The discriminating half, and it needs a RESTART.** Within one process
    // the sampler's in-memory `last` row already refuses the dead key, because
    // its counters have not moved — so everything above holds with the
    // live-key filter deleted (measured: that mutation came back GREEN against
    // the assertions above alone). A fresh registry over the same state root
    // has no `last` for that key, `usage.json` still hands it back on the
    // merge, and the filter is then the ONLY thing between a dead agent and one
    // duplicate row per app start, forever.
    let reg2 = relaunch_registry(_d.path());
    reg2.set_claude_projects_dir(proj.path().to_path_buf());
    reg2.set_series_bucket_ms(0);
    let after = reg2.group_usage(&g.id);
    assert_eq!(
        after["lifetime_tokens"].as_u64(),
        Some(1500),
        "the restarted process still reads the historical snapshot — the fixture that \
         makes the next assertion discriminating rather than vacuous"
    );
    assert_eq!(
        series_lines(&reg2, &g.id).len(),
        1,
        "and still appends nothing for it across a restart"
    );
}

#[test]
fn a_row_written_before_block_existed_loads_with_empty_block_and_cli() {
    // The additive half of the schema change, and the reason it is additive:
    // a `usage.json` written by an older build must keep counting, and the two
    // new fields must read as UNKNOWN rather than being guessed from `role` —
    // which cannot tell `worker-std` from `worker-adv` and would therefore
    // file real spend under a block that never spent it.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let legacy = json!([{
        "key": "sess-legacy",
        "agent_id": "w-legacy",
        "name": "w",
        "role": "worker",
        "source": "transcript",
        "input_tokens": 1200u64,
        "output_tokens": 300u64,
        "cache_creation_tokens": 0u64,
        "cache_read_tokens": 0u64,
        "cost_usd": 0.25f64,
        "estimated": true,
        "model": "claude-opus-4-8",
        "updated_ms": 1u64,
    }]);
    let dir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("usage.json"), serde_json::to_string(&legacy).unwrap()).unwrap();

    let usage = reg.group_usage(&g.id);
    let row = usage["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "w-legacy")
        .expect("the legacy row still loads");
    assert_eq!(row["block"], "", "unknown, deliberately not guessed from role");
    assert_eq!(row["cli"], "");
    assert_eq!(
        usage["lifetime_tokens"].as_u64(),
        Some(1500),
        "and the spend it carries is not lost by the schema change"
    );
}

#[test]
fn two_worker_blocks_on_different_clis_are_labelled_by_block_not_by_role() {
    // `role` is the capability CLASS — four values — so it cannot tell two
    // worker blocks apart, and every cost question here is about exactly that
    // split. The second half is the one a shortcut would get wrong: `cli` is
    // NOT derivable from `source`, because a block with no readable usage yet
    // reports `source: "none"`, off which no CLI can be read at all.
    let (reg, _d) = test_registry();
    let proj = tempfile::tempdir().unwrap();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg
        .create_group("C:/tmp/repo", rails_with_second_worker_cli(4, "worker-oc", "opencode"))
        .unwrap();

    let a = reg
        .spawn_agent_ex(&g.id, Role::Worker, None, "wa", "t", false, None, None, None, None, None)
        .unwrap();
    let b = reg
        .spawn_agent_ex(
            &g.id,
            Role::Worker,
            Some("worker-oc".to_string()),
            "wb",
            "t",
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
    write_claude_transcript(proj.path(), a.session_id.as_deref().unwrap(), 1000, 500);

    let usage = reg.group_usage(&g.id);
    let rows = usage["agents"].as_array().unwrap();
    let ra = rows.iter().find(|r| r["id"] == a.id.as_str()).expect("block worker");
    let rb = rows.iter().find(|r| r["id"] == b.id.as_str()).expect("block worker-oc");

    assert_eq!(ra["role"], rb["role"], "one capability class, which is the point");
    assert_eq!(ra["block"], "worker");
    assert_eq!(rb["block"], "worker-oc", "the blocks are what tell them apart");
    assert_eq!(ra["cli"], "claude");
    assert_eq!(
        rb["cli"], "opencode",
        "resolved from the block's own cli, per cli_for_block"
    );
    assert_eq!(
        rb["source"], "none",
        "and that agent has no readable usage yet — the fixture that makes the next \
         assertion discriminating"
    );
    assert_ne!(
        rb["cli"], rb["source"],
        "so `cli` cannot have been read off `source`"
    );
}

#[test]
fn the_series_read_filters_by_since_ms_and_reports_its_coverage_floor() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(&dir).unwrap();

    // An absent file is an empty series, never an error: a group that has not
    // spent yet is a normal state, and the floor for it is `null` rather than
    // a fabricated timestamp.
    let empty = reg.usage_series(&g.id, 0);
    assert_eq!(empty["rows"].as_array().unwrap().len(), 0);
    assert!(empty["first_ts_ms"].is_null(), "no rows, no floor: {empty}");
    assert_eq!(empty["skipped"].as_u64(), Some(0));

    let row = |ts: u64, key: &str| {
        json!({"ts_ms":ts,"kind":"sample","key":key,"agent":"w-1","block":"worker",
               "cli":"claude","role":"worker","in":ts,"out":0,"cache_w":0,"cache_r":0,
               "cost_usd":null,"estimated":false,"source":"transcript","model":null})
        .to_string()
    };
    fs::write(
        dir.join("usage-series.jsonl"),
        format!("{}\n{}\n{{ not json\n{}\n", row(100, "s1"), row(200, "s1"), row(300, "s1")),
    )
    .unwrap();

    let all = reg.usage_series(&g.id, 0);
    assert_eq!(all["rows"].as_array().unwrap().len(), 3, "{all}");
    assert_eq!(all["skipped"].as_u64(), Some(1), "the bad line is COUNTED, not hidden");

    let since = reg.usage_series(&g.id, 200);
    let rows = since["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "since_ms is inclusive of its own bucket: {since}");
    assert_eq!(rows[0]["ts_ms"].as_u64(), Some(200));
    assert_eq!(
        since["first_ts_ms"].as_u64(),
        Some(100),
        "the coverage floor is the file's own oldest row, NOT the filtered window's — \
         it is what tells the panel how far back the history really goes"
    );
}

#[test]
fn the_series_read_labels_its_agents_and_a_dead_ones_cli_survives_in_its_rows() {
    // The `agents` half of the payload: the projection attributes rows by
    // agent, so a row whose agent has since exited must still label. The CLI
    // for a dead agent is read off the ROWS it wrote, never reversed out of its
    // persisted role string — `workflow::kind_from_str` is a capability
    // vocabulary with no arm for two of the classes, so that reversal is both
    // lossy and a widening of a grant.
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    reg.set_series_bucket_ms(0);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "brief text", false, None).unwrap();
    write_claude_transcript(proj.path(), w.session_id.as_deref().unwrap(), 1000, 500);
    reg.group_usage(&g.id);

    let live = reg.usage_series(&g.id, 0);
    let entry = live["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == w.id.as_str())
        .expect("the live agent is listed")
        .clone();
    assert_eq!(entry["block"], "worker");
    assert_eq!(entry["cli"], "claude");
    assert_eq!(entry["role"], "worker");
    assert_eq!(entry["task"], "brief text");
    assert_eq!(entry["session"], w.session_id.as_deref().unwrap());

    // **The discriminating half needs a RESTART, not a kill.** `mark_dead`
    // leaves the entry in the in-memory agent map with a dead STATUS, so the
    // live lookup still answers for it and the row fallback is never reached.
    // (Measured: the round-13 scratch mutation, which deletes that fallback,
    // came back GREEN against a kill alone.) A fresh registry over the same
    // state root has no agent map at all, and the rows the agent wrote while
    // alive are then the only surviving record of which CLI it ran.
    reg.mark_dead(&w.id, Some(0));
    let reg2 = relaunch_registry(_d.path());
    let after = reg2.usage_series(&g.id, 0);
    let gone = after["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == w.id.as_str())
        .expect("a restarted process still lists the agent, off the persisted roster")
        .clone();
    assert_eq!(gone["cli"], "claude", "recovered from the rows it wrote while alive");
    assert_eq!(gone["block"], "worker");
}

#[test]
fn changing_one_byte_of_workflow_yml_changes_only_that_component() {
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join(".orrerix")).unwrap();
    fs::write(repo.path().join(".orrerix").join("workflow.yml"), "blocks: []\n").unwrap();
    fs::write(repo.path().join("CLAUDE.md"), "rules\n").unwrap();
    fs::create_dir_all(repo.path().join(".github").join("agents")).unwrap();
    fs::write(repo.path().join(".github").join("agents").join("worker.md"), "persona\n").unwrap();

    let before = tuningfp::fingerprint(repo.path());
    assert!(!before.partial, "nothing here trips a cap");
    fs::write(repo.path().join(".orrerix").join("workflow.yml"), "blocks: [ ]\n").unwrap();
    let after = tuningfp::fingerprint(repo.path());

    assert_eq!(
        usageseries::fp_changed(&before.components, &after.components),
        vec!["workflow".to_string()],
        "one edit, one component: before={:?} after={:?}",
        before.components,
        after.components
    );
    // The discriminating control: an unrelated edit moves a DIFFERENT single
    // component, so "only workflow" above is not just "workflow is the only
    // one this ever reports".
    fs::write(repo.path().join(".github").join("agents").join("worker.md"), "persona2\n").unwrap();
    let third = tuningfp::fingerprint(repo.path());
    assert_eq!(
        usageseries::fp_changed(&after.components, &third.components),
        vec!["agents".to_string()]
    );
}

#[test]
fn an_absent_surface_hashes_as_absent_not_empty() {
    // "The file is gone" and "the file is empty" are different events, and a
    // fingerprint that conflates them draws no vertical for a deleted
    // lessons.md. The key is always PRESENT either way — a missing key would
    // read to `fp_changed` as a component the other build did not know about.
    let repo = tempfile::tempdir().unwrap();
    let absent = tuningfp::fingerprint(repo.path());
    for name in tuningfp::COMPONENTS {
        assert!(
            absent.components.contains_key(*name),
            "component {name} must always be present: {:?}",
            absent.components
        );
    }
    assert_eq!(absent.components["lessons"], tuningfp::ABSENT);
    assert_eq!(absent.components["workflow"], tuningfp::ABSENT);
    assert_eq!(absent.components["skills"], tuningfp::ABSENT);
    assert_eq!(
        absent.components["version"],
        env!("CARGO_PKG_VERSION"),
        "the build's own version stands in for the templates compiled into it"
    );

    fs::create_dir_all(repo.path().join(".orrerix")).unwrap();
    fs::write(repo.path().join(".orrerix").join("lessons.md"), "").unwrap();
    let empty = tuningfp::fingerprint(repo.path());
    assert_ne!(
        empty.components["lessons"],
        tuningfp::ABSENT,
        "an EMPTY file hashes to its own digest, which is not the absent sentinel"
    );
    assert_eq!(
        usageseries::fp_changed(&absent.components, &empty.components),
        vec!["lessons".to_string()],
        "so creating it is a mark"
    );
    assert!(!empty.partial);
}

#[test]
fn the_walk_cap_sets_fp_partial() {
    // A repo is caller-supplied, so the walk is capped in two directions and
    // each cap is DISCLOSED rather than silently narrowing what a mark means.
    let repo = tempfile::tempdir().unwrap();
    let skills = repo.path().join(".claude").join("skills");
    fs::create_dir_all(&skills).unwrap();
    fs::write(skills.join("a.md"), "small\n").unwrap();
    let shallow = tuningfp::fingerprint(repo.path());
    assert!(!shallow.partial, "the control: a small shallow tree is complete");

    // Depth: one directory past the cap.
    let mut deep = skills.clone();
    for i in 0..(tuningfp::MAX_DEPTH + 2) {
        deep = deep.join(format!("d{i}"));
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("b.md"), "deep\n").unwrap();
    let capped_depth = tuningfp::fingerprint(repo.path());
    assert!(capped_depth.partial, "a tree deeper than the cap is a PARTIAL answer");

    // Size: one file over the cap, in an otherwise shallow tree.
    let repo2 = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo2.path().join(".orrerix")).unwrap();
    let big = vec![b'x'; (tuningfp::MAX_FILE_BYTES + 1) as usize];
    fs::write(repo2.path().join(".orrerix").join("lessons.md"), &big).unwrap();
    let capped_size = tuningfp::fingerprint(repo2.path());
    assert!(capped_size.partial, "a file over the size cap is a PARTIAL answer");
    assert_eq!(
        capped_size.components["lessons"],
        tuningfp::ABSENT,
        "a skipped file is not hashed, and says so with the same sentinel"
    );
}

#[test]
fn a_mark_is_written_when_the_repo_tuning_changes_and_not_before() {
    // The mark half of the sampler, driven through the real tick. The first
    // look SEEDS rather than marks: every app restart would otherwise stamp an
    // "everything changed" vertical onto a plot where nothing had.
    let proj = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join(".orrerix")).unwrap();
    fs::write(repo.path().join(".orrerix").join("workflow.yml"), "blocks: []\n").unwrap();

    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    reg.set_series_bucket_ms(0);
    let g = reg.create_group(&repo.path().to_string_lossy(), rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let sid = w.session_id.clone().unwrap();
    // The transcript lives under the encoded-cwd folder the reader scans; the
    // folder name is irrelevant to it.
    write_claude_transcript(proj.path(), &sid, 1000, 500);

    reg.group_usage(&g.id); // seeds the fingerprint, samples one row
    reg.group_usage(&g.id); // unchanged tuning, unchanged counters
    let quiet = series_lines(&reg, &g.id);
    assert!(
        quiet.iter().all(|l| !l.contains("\"kind\":\"mark\"")),
        "an unchanged repo writes no mark: {quiet:?}"
    );

    fs::write(repo.path().join(".orrerix").join("workflow.yml"), "blocks: [ ]\n").unwrap();
    reg.group_usage(&g.id);
    let marks: Vec<serde_json::Value> = series_lines(&reg, &g.id)
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["kind"] == "mark")
        .collect();
    assert_eq!(marks.len(), 1, "exactly one mark for one change: {marks:?}");
    assert_eq!(
        marks[0]["changed"].as_array().unwrap(),
        &vec![json!("workflow")],
        "and it names what moved"
    );
    assert_eq!(marks[0]["fp_partial"], false);
    assert_ne!(
        marks[0]["prev"]["workflow"], marks[0]["fp"]["workflow"],
        "the row carries both sides, so a reader can diff them without the file"
    );

    // And it does not repeat on every later tick.
    reg.group_usage(&g.id);
    let again = series_lines(&reg, &g.id)
        .iter()
        .filter(|l| l.contains("\"kind\":\"mark\""))
        .count();
    assert_eq!(again, 1, "a mark is written on the CHANGE, not on the state");
}

#[test]
fn an_unreadable_surface_is_capped_not_silently_absent() {
    // #2941 review W3. `ABSENT` is a real value `fp_changed` compares like any
    // other, so a surface that exists but cannot be read THIS INSTANT flips its
    // component to absent for one bucket and back on the next: two mark rows
    // asserting a tuning change that never happened. `fp_partial` is the only
    // signal a reader has that a `changed` list may be a failed read rather
    // than an edit, so every arm that TRANSIENTLY cannot see a surface must set
    // it. Stably-not-there is a different case and is pinned at the end.
    //
    // The unreadable DIRECTORY is provoked portably by putting a regular file
    // where the walk expects a directory: `read_dir` fails on it everywhere,
    // and `try_exists` still answers `Ok(true)`, which is exactly the
    // "exists but I could not read it" state a lock or an AV scan produces.
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir_all(repo.path().join(".claude")).unwrap();
    fs::write(repo.path().join(".claude").join("skills"), "not a directory\n").unwrap();

    let unreadable = tuningfp::fingerprint(repo.path());
    assert!(
        unreadable.partial,
        "a directory that exists and could not be read is a CAP: {:?}",
        unreadable.components
    );

    // The control that makes the assertion above discriminating: with the
    // skills tree simply ABSENT, the digest is the same sentinel and `partial`
    // is false. The two states are byte-identical in `components` — which is
    // the whole reason the flag has to carry the difference.
    let gone = tempfile::tempdir().unwrap();
    let absent = tuningfp::fingerprint(gone.path());
    assert!(!absent.partial, "a missing tree is a real answer, not a cap");
    assert_eq!(
        absent.components["skills"], unreadable.components["skills"],
        "both read as the same sentinel, so `partial` is the ONLY thing that \
         separates 'there is no skills tree' from 'I could not look'"
    );
    assert_eq!(absent.components["skills"], tuningfp::ABSENT);

    // **The boundary of the rule, pinned so nobody widens it by symmetry.**
    // What forces `partial` is a TRANSIENT failure — one that can clear on the
    // next bucket and flip the component back, which is what makes a false
    // mark PAIR. A path that exists and is stably not a file (a directory
    // where `lessons.md` belongs) hashes as absent and is NOT capped: the
    // answer is the same on every bucket, so it cannot flip and cannot
    // manufacture a mark. Asserting `partial` here instead would be asserting
    // a behaviour the code is right not to have.
    let f = tempfile::tempdir().unwrap();
    fs::create_dir_all(f.path().join(".orrerix").join("lessons.md")).unwrap();
    let not_a_file = tuningfp::fingerprint(f.path());
    assert_eq!(not_a_file.components["lessons"], tuningfp::ABSENT);
    assert!(
        !not_a_file.partial,
        "a STABLE non-file is a real answer, not a cap — only a transient \
         failure can flip a component and produce a false mark pair"
    );
    // And it really is stable: the same input answers the same way twice, which
    // is the property the sentence above rests on.
    assert_eq!(tuningfp::fingerprint(f.path()).components, not_a_file.components);
}

#[test]
fn an_oversize_series_is_reported_not_truncated() {
    // #2941 review finding 2. Nothing rotates or compacts this file, so the
    // whole-file read has a size at which it stops being cheap — and a
    // residual with no number in it is one nobody can tell has been reached.
    // The ceiling is a REPORT: crossing it must never shorten the answer,
    // because a chart that silently truncates its own history is worse than a
    // slow one.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(&dir).unwrap();
    let row = |ts: u64| {
        json!({"ts_ms":ts,"kind":"sample","key":"s1","agent":"w-1","block":"worker",
               "cli":"claude","role":"worker","in":ts,"out":0,"cache_w":0,"cache_r":0,
               "cost_usd":null,"estimated":false,"source":"transcript","model":null})
        .to_string()
    };
    let body = format!("{}\n{}\n{}\n", row(100), row(200), row(300));
    fs::write(dir.join("usage-series.jsonl"), &body).unwrap();

    // Under the ceiling: not oversize, and `bytes` is the file's real size
    // rather than a figure derived from the parsed rows.
    let under = reg.usage_series(&g.id, 0);
    assert_eq!(under["oversize"], false);
    assert_eq!(
        under["bytes"].as_u64(),
        Some(body.len() as u64),
        "the size is read off the file: {under}"
    );
    assert_eq!(under["rows"].as_array().unwrap().len(), 3);

    // Over it: the flag flips and NOTHING ELSE CHANGES. Same row count, same
    // coverage floor — that is what makes it a report rather than a limit.
    reg.set_series_revisit_bytes((body.len() as u64) - 1);
    let over = reg.usage_series(&g.id, 0);
    assert_eq!(over["oversize"], true, "the ceiling is crossed: {over}");
    assert_eq!(
        over["rows"].as_array().unwrap().len(),
        3,
        "every row is still returned — a report, never a truncation"
    );
    assert_eq!(over["first_ts_ms"].as_u64(), Some(100), "and the floor is unmoved");
    assert_eq!(over["bytes"], under["bytes"]);

    // Exactly AT the ceiling is not over it: the comparison is strict, so a
    // file the size of the trigger does not report itself.
    reg.set_series_revisit_bytes(body.len() as u64);
    assert_eq!(reg.usage_series(&g.id, 0)["oversize"], false);
}

/// The `poll-read-failed` rows in a group's audit log, read with the poll
/// ceiling lifted — the audit read is itself one of the bounded readers, so a
/// test that lowered the ceiling has to lift it before looking.
fn poll_read_failures(reg: &OrchRegistry, g: &GroupId) -> Vec<AuditEntry> {
    reg.set_poll_read_limit(None);
    reg.audit_log(g).into_iter().filter(|e| e.action == "poll-read-failed").collect()
}

#[test]
fn a_series_read_over_its_limit_fails_soft_and_reports_once() {
    // #3469. orrerix died on `handle_alloc_error` because a refused
    // allocation had no way to be anything but an abort. The chart's read is
    // polled every 30 s, so the property is: a read that cannot be served
    // costs THAT TICK — the command's existing `Null` degrade — plus one
    // audited report, and the next read that can be served is served in full.
    // The refusal is injected through the reader's own limit, never by
    // exhausting memory; `boundedread`'s own tests pin that a refused
    // reservation takes the same `Err` path.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(&dir).unwrap();
    let row = |ts: u64| {
        json!({"ts_ms":ts,"kind":"sample","key":"s1","agent":"w-1","block":"worker",
               "cli":"claude","role":"worker","in":ts,"out":0,"cache_w":0,"cache_r":0,
               "cost_usd":null,"estimated":false,"source":"transcript","model":null})
        .to_string()
    };
    let body = format!("{}\n{}\n{}\n", row(100), row(200), row(300));
    fs::write(dir.join("usage-series.jsonl"), &body).unwrap();
    let len = body.len() as u64;

    // Positive control: at exactly the limit the read is served, whole.
    reg.set_poll_read_limit(Some(len));
    let at = reg.usage_series(&g.id, 0);
    assert_eq!(at["rows"].as_array().map(Vec::len), Some(3), "a file AT the limit is read: {at}");
    assert!(poll_read_failures(&reg, &g.id).is_empty(), "and nothing is reported for it");

    // One byte over: the tick is skipped, not the process.
    reg.set_poll_read_limit(Some(len - 1));
    assert_eq!(reg.usage_series(&g.id, 0), Value::Null, "an over-limit read degrades to Null");
    // Still failing: still Null, and the report is NOT repeated per poll.
    reg.set_poll_read_limit(Some(len - 1));
    assert_eq!(reg.usage_series(&g.id, 0), Value::Null);
    let rows = poll_read_failures(&reg, &g.id);
    assert_eq!(rows.len(), 1, "one report per failing episode, not one per poll: {rows:?}");
    assert_eq!(rows[0].detail["reader"], json!("usage-series"));
    let err = rows[0].detail["error"].as_str().unwrap_or_default();
    assert!(err.contains("read limit"), "the row says WHY the read failed: {err}");

    // Recovered: the next servable read is whole again, and it re-arms the
    // report, so a later episode is reported rather than swallowed.
    let back = reg.usage_series(&g.id, 0);
    assert_eq!(back["rows"].as_array().map(Vec::len), Some(3), "recovery serves every row: {back}");
    reg.set_poll_read_limit(Some(len - 1));
    assert_eq!(reg.usage_series(&g.id, 0), Value::Null);
    assert_eq!(poll_read_failures(&reg, &g.id).len(), 2, "a second episode is a second report");
}

#[test]
fn an_audit_window_over_its_limit_fails_soft_as_a_truncated_window() {
    // #3469, the audit half. The window is read by the viewer's follow poll
    // and by derivations. The degrade is an EMPTY window marked TRUNCATED, so
    // the two that read the flag (`front_door_refusals`, `refusal_roster`) see
    // a partial window rather than a confident empty timeline. `audit_log`'s
    // callers drop the flag and see it as empty, which is what the §1c table
    // in crash-observability.md says.
    let (reg, d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let text = "a refused report";
    let lines = [
        audit_jsonl_line(&prompt_line(1_000, "w-1", &orch.id, text)),
        audit_jsonl_line(&refusal_line(1_001, "w-1", &orch.id, text, "arrival")),
    ];
    let body = lines.join("\n") + "\n";
    fs::write(d.path().join(g.id.as_str()).join("audit.jsonl"), &body).unwrap();

    // Positive control: under the limit the refusal is seen, untruncated.
    let seen = reg.front_door_refusals(&g.id);
    assert_eq!((seen.total, seen.window_truncated), (1, false), "the fixture is readable: {:?}", seen.items);

    reg.set_poll_read_limit(Some(body.len() as u64 - 1));
    let err = reg.try_audit_log_windowed(&g.id).expect_err("an over-limit generation is an Err");
    assert!(err.contains("audit.jsonl") && err.contains("read limit"), "the error names the file and why: {err}");
    let (entries, truncated) = reg.audit_log_windowed(&g.id);
    assert!(entries.is_empty() && truncated, "the degrade is an empty window marked truncated");
    let r = reg.front_door_refusals(&g.id);
    assert!(r.window_truncated, "a derivation over the degrade reports a partial window, not a complete zero");

    let rows = poll_read_failures(&reg, &g.id);
    assert_eq!(rows.len(), 1, "reported once: {rows:?}");
    assert_eq!(rows[0].detail["reader"], json!("audit"));
    // And the read is whole again once it can be served.
    assert_eq!(reg.front_door_refusals(&g.id).total, 1);
}

#[test]
fn the_bounded_audit_window_matches_the_whole_log_trim_across_both_generations() {
    // #3469 changed HOW the window is built — per generation, into a deque
    // capped at `AUDIT_VIEW_LIMIT`, instead of concatenating both files and
    // trimming a Vec of every entry — and the answer must not move. The
    // fixture straddles the rotation boundary so the order across the two
    // files is part of what is pinned, and the rotated file has no trailing
    // newline (the case the old concatenation had a guard for).
    let (reg, d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let dir = d.path().join(g.id.as_str());
    let entry = |seq: usize| {
        audit_jsonl_line(&AuditEntry {
            ts_ms: seq as u64,
            actor: "loomux".into(),
            action: "seeded".into(),
            detail: json!({ "seq": seq }),
        })
    };
    let write = |total: usize| {
        let split = total / 2;
        let old: Vec<String> = (0..split).map(entry).collect();
        let new: Vec<String> = (split..total).map(entry).collect();
        fs::write(dir.join("audit.1.jsonl"), old.join("\n")).unwrap(); // no trailing newline
        fs::write(dir.join("audit.jsonl"), new.join("\n") + "\n").unwrap();
    };

    write(AUDIT_VIEW_LIMIT);
    let (all, cut) = reg.audit_log_windowed(&g.id);
    assert!(!cut, "exactly the limit is not a cut");
    assert_eq!(all.len(), AUDIT_VIEW_LIMIT);
    assert_eq!(all[0].detail["seq"], json!(0));

    write(AUDIT_VIEW_LIMIT + 1);
    let (w, cut) = reg.audit_log_windowed(&g.id);
    assert!(cut, "one over the limit is a cut");
    assert_eq!(w.len(), AUDIT_VIEW_LIMIT);
    let seqs: Vec<u64> = w.iter().map(|e| e.detail["seq"].as_u64().unwrap()).collect();
    let want: Vec<u64> = (1..=AUDIT_VIEW_LIMIT as u64).collect();
    assert!(seqs == want, "the OLDEST entry is the one dropped, and order holds across the rotation");
    // #3493 review N2: the window never holds more than the limit's worth of
    // slots. `Vec::from(VecDeque)` keeps the deque's buffer, so the returned
    // capacity IS the window's reservation.
    assert!(w.capacity() <= AUDIT_VIEW_LIMIT, "a full window is capped at the limit: {}", w.capacity());

    // And a short log reserves a short window, not all 5000 slots up front.
    write(10);
    let (short, cut) = reg.audit_log_windowed(&g.id);
    assert!(!cut);
    assert_eq!(short.len(), 10);
    assert!(short.capacity() < 100, "a 10-entry log must not reserve the whole window: {}", short.capacity());
}

#[test]
fn an_mcp_group_usage_call_is_a_writer_to_the_series() {
    // #2941 review W1. Four permanent surfaces used to say the sampler runs on
    // "the view publisher thread". It does not: `compute_group_usage` has one
    // caller (`group_usage_memoed`) and three ways in, and the MCP
    // `group_usage` tool is one of them — it asks with `Duration::ZERO`, so it
    // never serves the memo, always recomputes, and therefore always samples.
    //
    // This pins the PROPERTY the corrected docs now claim, rather than the
    // prose: a caller that is not the publisher writes rows. `group_usage` is
    // that call's own entry point (`mcp.rs`'s tool arm calls exactly this), so
    // driving it here exercises the same chain without standing up an MCP
    // server.
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    reg.set_series_bucket_ms(0);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    write_claude_transcript(proj.path(), w.session_id.as_deref().unwrap(), 1000, 500);

    assert!(series_lines(&reg, &g.id).is_empty(), "nothing sampled yet");
    // The un-memoed entry point, which is what the MCP tool arm calls.
    reg.group_usage(&g.id);
    assert_eq!(
        series_lines(&reg, &g.id).len(),
        1,
        "a non-publisher caller of group_usage appends to the series"
    );
}

#[test]
fn the_coverage_floor_is_the_oldest_ts_not_the_first_row_appended() {
    // #2941 review round 2 premortem. Rows land in WRITE order, and
    // `should_sample` deliberately treats a backwards clock as "elapsed" so a
    // wall-clock correction cannot wedge a key — so after one correction the
    // first row in the file is not the oldest one in it. The floor is a claim
    // about how far back the history goes ("series since …"), so reading it off
    // the first row would print a floor LATER than the panel's own data, which
    // is the one thing that number exists to prevent.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(&dir).unwrap();
    let row = |ts: u64| {
        json!({"ts_ms":ts,"kind":"sample","key":"s1","agent":"w-1","block":"worker",
               "cli":"claude","role":"worker","in":ts,"out":0,"cache_w":0,"cache_r":0,
               "cost_usd":null,"estimated":false,"source":"transcript","model":null})
        .to_string()
    };
    // Written in this order; the clock went backwards between the first and the
    // second, which is exactly the case `should_sample` keeps sampling through.
    fs::write(
        dir.join("usage-series.jsonl"),
        format!("{}\n{}\n{}\n", row(5_000), row(1_000), row(9_000)),
    )
    .unwrap();

    let view = reg.usage_series(&g.id, 0);
    assert_eq!(
        view["first_ts_ms"].as_u64(),
        Some(1_000),
        "the floor is the oldest ts in the file, not the first row appended: {view}"
    );
    // The discriminating control: the first row's ts is a DIFFERENT value, so
    // this cannot pass under an implementation that reads `all.first()`.
    assert_ne!(view["first_ts_ms"].as_u64(), Some(5_000));
    assert_eq!(view["rows"].as_array().unwrap().len(), 3, "and nothing is dropped");
}
