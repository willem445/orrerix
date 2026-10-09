//! The next-prompt cost estimate's backend half (#3831): what the usage row
//! carries for `src/promptcost.ts` to compute from, and that the two fields it
//! adds to the persisted row are additive in both directions.
//!
//! One module of the `orchestration` integration-test target (`main.rs`).
//! Layout rules: docs/design/module-layout.md. The TTL ladder and the idle
//! backstop's half of #3831 are in `cacheage.rs`, beside the rest of the TTL.

use super::*;
use std::collections::{BTreeMap, BTreeSet};

const MIN: u64 = 60_000;

fn cost_row<'a>(usage: &'a Value, agent: &str) -> &'a Value {
    usage["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == agent)
        .unwrap_or_else(|| panic!("no usage row for {agent}: {usage}"))
}

/// Append one Claude assistant line for `sid`, with the cache write split into
/// the two buckets the API reports. Appends, because the usage tick reads
/// through a byte cursor and an append is the shape a live transcript takes.
#[allow(clippy::too_many_arguments)]
fn append_turn(proj: &Path, sid: &str, id: &str, model: &str, input: u64, w5: u64, w60: u64, read: u64) {
    use std::io::Write;
    let encoded = proj.join("C--tmp-repo");
    fs::create_dir_all(&encoded).unwrap();
    let line = json!({"type":"assistant","message":{"id":id,"model":model,
        "usage":{"input_tokens":input,"output_tokens":50,
                 "cache_creation_input_tokens":w5 + w60,"cache_read_input_tokens":read,
                 "cache_creation":{"ephemeral_5m_input_tokens":w5,"ephemeral_1h_input_tokens":w60}}}});
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(encoded.join(format!("{sid}.jsonl")))
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

/// The six keys of `prompt_cost` that come off the price table.
const PRICE_KEYS: [&str; 6] =
    ["price_model", "price_per_mtok", "price_long_prompt", "price_basis", "price_dated", "chars_per_token"];

#[test]
fn a_usage_row_carries_everything_the_next_prompt_estimate_reads() {
    // The whole path, off a real transcript under the projects-dir override:
    // fold -> snapshot -> merge -> row. Three turns on Sonnet 5.5, whose
    // version the family-only table could not tell from Sonnet 4.6.
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let sid = w.session_id.clone().unwrap();
    // A second pane on a version of a listed family the table does not list.
    let newer = reg.spawn_agent(&g.id, Role::Worker, "n", "task", false, None).unwrap();
    append_turn(proj.path(), newer.session_id.as_deref().unwrap(), "n1", "claude-sonnet-9", 10, 0, 0, 0);

    // The first turn: 40,000 tokens of context, all written to the hour cache.
    append_turn(proj.path(), &sid, "t1", "claude-sonnet-5-5", 10, 0, 39_990, 0);
    append_turn(proj.path(), &sid, "t2", "claude-sonnet-5-5", 5, 0, 2_000, 40_000);
    // The newest turn: 2 fresh + 300 written + 250,000 read.
    append_turn(proj.path(), &sid, "t3", "claude-sonnet-5-5", 2, 0, 300, 250_000);
    let usage = reg.group_usage(&g.id);
    let row = cost_row(&usage, &w.id);
    let cost = &row["prompt_cost"];

    assert_eq!(row["source"], "transcript", "control: the row really is read off the fixture: {row}");
    assert!(cost.is_object(), "a live row carries the estimate's inputs: {row}");
    // C: what the NEWEST turn was sent — not the session's running total.
    assert_eq!(cost["context_tokens"], json!(250_302));
    assert_ne!(row["tokens"]["total"], cost["context_tokens"]);
    // F: what the FIRST turn was sent.
    assert_eq!(cost["first_context_tokens"], json!(40_000));
    // p: Sonnet 5.5's own row, resolved here so the frontend keeps no table.
    assert_eq!(cost["price_model"], "claude-sonnet-5-5");
    assert_eq!(cost["price_basis"], "listed");
    assert_eq!(cost["price_per_mtok"]["input"], json!(2.0), "Sonnet 5.5 is $2 input, not Sonnet 4.6's $3");
    assert_eq!(cost["price_per_mtok"]["cache_read"], json!(0.10));
    assert_eq!(cost["price_per_mtok"]["cache_write"], json!(2.5));
    assert_eq!(cost["price_per_mtok"]["cache_write_1h"], json!(4.0));
    assert_eq!(cost["price_per_mtok"]["output"], json!(10.0));
    assert_eq!(cost["price_long_prompt"], Value::Null);
    assert_eq!(cost["price_dated"], "2026-10-09");
    assert_eq!(cost["chars_per_token"], json!(3.5 / 1.3));
    // T: every cache write in the session went to the hour cache, the block
    // declares nothing, so the DETECTED hour beats claude's default five.
    assert_eq!(row["cache_ttl_minutes"], json!(60));
    assert_eq!(row["cache_ttl_source"], "session");
    assert_eq!(row["cache_cooling_after_ms"], json!(48 * MIN));

    // The two fields the row persists are on disk, so a tick whose fresh read
    // comes back empty still has them.
    let disk = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("usage.json")).unwrap();
    assert!(disk.contains("\"first_context_tokens\": 40000"), "{disk}");
    assert!(disk.contains("\"detected_cache_ttl_minutes\": 60"), "{disk}");

    // An unlisted version of a listed family is priced at that family's
    // ceiling, and the row says the price is a ceiling. It wrote nothing to
    // the cache, so nothing is detected and it sits on claude's default five.
    let row = cost_row(&usage, &newer.id);
    assert_eq!(row["prompt_cost"]["price_basis"], "family-ceiling");
    assert_eq!(row["prompt_cost"]["price_per_mtok"]["input"], json!(3.0));
    assert_eq!(row["cache_ttl_minutes"], json!(5));
    assert_eq!(row["cache_ttl_source"], "cli");
}

#[test]
fn a_tiered_model_carries_its_long_prompt_price_and_an_unpriced_one_carries_none() {
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let haiku = reg.spawn_agent(&g.id, Role::Worker, "h", "task", false, None).unwrap();
    let future = reg.spawn_agent(&g.id, Role::Worker, "f", "task", false, None).unwrap();
    append_turn(proj.path(), haiku.session_id.as_deref().unwrap(), "h1", "claude-haiku-5-5", 10, 500, 0, 0);
    append_turn(proj.path(), future.session_id.as_deref().unwrap(), "f1", "some-future-model-9", 10, 500, 0, 0);
    let usage = reg.group_usage(&g.id);

    // The one model with a prompt-length tier carries both prices and the
    // threshold between them.
    let row = cost_row(&usage, &haiku.id);
    let cost = &row["prompt_cost"];
    assert_eq!(cost["price_per_mtok"]["input"], json!(0.10));
    assert_eq!(cost["price_long_prompt"]["over_tokens"], json!(100_000));
    assert_eq!(cost["price_long_prompt"]["price"]["input"], json!(0.50));
    assert_eq!(cost["price_long_prompt"]["price"]["cache_read"], json!(0.05));
    // Its writes went to the five-minute cache, and the row says so.
    assert_eq!(row["cache_ttl_minutes"], json!(5));
    assert_eq!(row["cache_ttl_source"], "session");

    // A model the table does not list is tokens-only: every price field is
    // null — never a zero — while the token readings are still there.
    let row = cost_row(&usage, &future.id);
    let cost = &row["prompt_cost"];
    assert_eq!(row["source"], "transcript", "control: this row was read too: {row}");
    assert!(cost.is_object(), "the object is there; it is the PRICE that is unknown: {row}");
    for key in PRICE_KEYS {
        assert_eq!(cost[key], Value::Null, "{key} must be null for an unpriced model: {row}");
    }
    assert_eq!(row["cost_usd"], Value::Null);
    assert_eq!(cost["context_tokens"], json!(510));
    assert_eq!(cost["first_context_tokens"], json!(510));
}

#[test]
fn a_row_whose_cli_reports_its_own_dollars_is_never_priced_off_the_table() {
    // The gate is the row's own provenance, never its model id and never its
    // CLI's name. pi reports the dollars it paid, so its row is `estimated:
    // false` — and a pi model id that names a Claude family (`anthropic/…`
    // through pi) must not pull an Anthropic list price onto it.
    //
    // Both rows are LIVE: each is stored under a spawned agent's own key, and
    // with no transcript on disk the tick's fresh read is empty, so the merge
    // keeps the stored row. The two differ in `estimated` and nothing else.
    let proj = tempfile::tempdir().unwrap();
    let (reg, _d) = test_registry();
    reg.set_claude_projects_dir(proj.path().to_path_buf());
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let a = reg.spawn_agent(&g.id, Role::Worker, "a", "task", false, None).unwrap();
    let b = reg.spawn_agent(&g.id, Role::Worker, "b", "task", false, None).unwrap();
    let stored = |agent: &str, key: &str, estimated: bool| UsageSnapshot {
        estimated,
        model: Some("anthropic/claude-opus-4-8".to_string()),
        current_model: Some("anthropic/claude-opus-4-8".to_string()),
        first_context_tokens: Some(12_000),
        ..usage_snap(key, agent, 0.4, 1_000, 10)
    };
    reg.upsert_usage_snapshot(&g.id, stored(&a.id, a.session_id.as_deref().unwrap(), false));
    reg.upsert_usage_snapshot(&g.id, stored(&b.id, b.session_id.as_deref().unwrap(), true));
    let usage = reg.group_usage(&g.id);

    let row = cost_row(&usage, &a.id);
    assert_eq!(row["live"], json!(true), "control: the row is live, so it has an estimate object: {row}");
    assert_eq!(row["estimated"], json!(false), "control: the stored row survived the tick: {row}");
    for key in PRICE_KEYS {
        assert_eq!(row["prompt_cost"][key], Value::Null, "{key}: a reported row's dollars are its CLI's: {row}");
    }
    // The token half of the estimate does not depend on a price.
    assert_eq!(row["prompt_cost"]["first_context_tokens"], json!(12_000));

    // The control: the SAME model id, priced, where the dollars are ours.
    let row = cost_row(&usage, &b.id);
    assert_eq!(row["estimated"], json!(true));
    assert_eq!(row["prompt_cost"]["price_per_mtok"]["input"], json!(5.0), "{row}");
    assert_eq!(row["prompt_cost"]["price_model"], "anthropic/claude-opus-4-8");
}

#[test]
fn a_row_whose_agent_is_gone_carries_no_estimate_object() {
    // The estimate is about a pane's NEXT prompt. A historical row has none,
    // and it is also the row the MCP `group_usage` tool hands an agent ten at
    // a time — so it carries one `null`, not eight null keys.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.upsert_usage_snapshot(
        &g.id,
        UsageSnapshot { first_context_tokens: Some(9_000), ..usage_snap("sess-gone", "w-gone", 0.4, 1_000, 10) },
    );
    let usage = reg.group_usage(&g.id);
    let row = cost_row(&usage, "w-gone");
    assert_eq!(row["live"], json!(false));
    assert_eq!(row["prompt_cost"], Value::Null, "{row}");
    assert!(row.as_object().unwrap().contains_key("prompt_cost"), "the key is present and null, not absent");
    // The TTL fields are not part of the estimate and are still there.
    assert_eq!(row["cache_ttl_minutes"], json!(5));
    // Nothing leaks out flat beside it.
    for key in PRICE_KEYS.iter().chain(["context_tokens", "first_context_tokens"].iter()) {
        assert!(!row.as_object().unwrap().contains_key(*key), "{key} must live under prompt_cost: {row}");
    }
}

/// A `usage.json` as the build before #3831 wrote it: seven rows cut out of a
/// real store, one per `source`. The file is named for v1.3.1-beta7, and its
/// field set is the one `UsageSnapshot` still had at v1.3.1-beta8 — the
/// release this change is cut from — so it is that build's shape too. See
/// `fixtures/usagestore/README.md`.
const USAGE_BEFORE_3831: &str = include_str!("../fixtures/usagestore/usage-v1.3.1-beta7.json");

/// The row as a build BEFORE #3831 declares it — the same seventeen fields,
/// and like `UsageSnapshot` no `deny_unknown_fields`. Reading today's file
/// through this is what "an older build still reads what this one writes"
/// means.
#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct RowBefore3831 {
    key: String,
    agent_id: String,
    name: String,
    role: String,
    source: String,
    #[serde(default)]
    block: String,
    #[serde(default)]
    cli: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    cost_usd: Option<f64>,
    estimated: bool,
    model: Option<String>,
    #[serde(default)]
    current_model: Option<String>,
    updated_ms: u64,
    #[serde(default)]
    activity: Value,
}

/// The key set of every row in a `usage.json`, by the row's `key`.
fn row_keys(text: &str) -> BTreeMap<String, BTreeSet<String>> {
    let rows: Vec<Value> = serde_json::from_str(text).expect("a row list");
    rows.iter()
        .map(|r| {
            let obj = r.as_object().expect("a row object");
            (obj["key"].as_str().unwrap().to_string(), obj.keys().cloned().collect())
        })
        .collect()
}

#[test]
fn a_usage_file_from_before_the_prompt_cost_fields_loads_unchanged() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("usage.json");
    fs::write(&path, USAGE_BEFORE_3831).unwrap();
    let before = row_keys(USAGE_BEFORE_3831);
    assert_eq!(before.len(), 7, "control: the fixture's seven rows were read");
    assert!(before.values().all(|k| k.len() == 17), "control: each in the seventeen-field shape");

    // It loads: every row is there with its figures, and a row with nothing
    // detected reads as its CLI's default TTL rather than failing the file.
    let usage = reg.group_usage(&g.id);
    assert_eq!(usage["agents"].as_array().unwrap().len(), 7);
    let claude = cost_row(&usage, "rev-3694");
    assert_eq!(claude["tokens"]["total"].as_u64(), Some(1_579_929));
    assert_eq!(claude["cache_ttl_minutes"], json!(5), "nothing detected on an old row: the CLI default");
    assert_eq!(claude["cache_ttl_source"], "cli");
    let pi = cost_row(&usage, "rev-2658");
    assert_eq!(pi["cache_ttl_minutes"], Value::Null, "and a CLI with no default still has none");
    // Reading it did not rewrite it: the bytes are the ones it was given.
    assert_eq!(fs::read_to_string(&path).unwrap(), USAGE_BEFORE_3831, "a read is not a migration");
    assert_eq!(audit_count(&reg, &g.id, "usage-corrupt"), 0);

    // The first write this build makes re-serializes all seven. Each comes
    // back with EXACTLY the keys it had — neither new key is added as `null`
    // — and so does a new row that has nothing to say in them.
    reg.upsert_usage_snapshot(&g.id, usage_snap("sess-plain", "w-plain", 0.1, 10, 1));
    let disk = fs::read_to_string(&path).unwrap();
    let after = row_keys(&disk);
    assert_eq!(after.len(), 8, "control: the write happened, and kept every row");
    for (key, keys) in &before {
        assert_eq!(after.get(key), Some(keys), "{key}: a re-serialized older row keeps its own key set");
    }
    assert_eq!(after["sess-plain"].len(), 17, "a row with nothing to say persists in the older shape");
    assert!(!disk.contains("first_context_tokens") && !disk.contains("detected_cache_ttl_minutes"), "{disk}");

    // A row that DOES carry them is written with them, and the file an older
    // build then meets still parses as that build's row list — all nine rows,
    // old figures intact, the two unknown keys ignored.
    reg.upsert_usage_snapshot(
        &g.id,
        UsageSnapshot {
            first_context_tokens: Some(40_000),
            detected_cache_ttl_minutes: Some(60),
            ..usage_snap("sess-new", "w-new", 0.2, 20, 2)
        },
    );
    let disk = fs::read_to_string(&path).unwrap();
    assert!(disk.contains("\"first_context_tokens\": 40000"), "{disk}");
    assert!(disk.contains("\"detected_cache_ttl_minutes\": 60"), "{disk}");
    assert_eq!(row_keys(&disk)["sess-new"].len(), 19);
    let older: Vec<RowBefore3831> = serde_json::from_str(&disk).expect("an older build reads the new file");
    assert_eq!(older.len(), 9);
    let kept = older.iter().find(|r| r.agent_id == "rev-3694").expect("the fixture's row survived");
    assert_eq!(
        kept.input_tokens + kept.output_tokens + kept.cache_creation_tokens + kept.cache_read_tokens,
        1_579_929
    );
    // ...and this build reads them back, each row with its own answer.
    let now: Vec<UsageSnapshot> = serde_json::from_str(&disk).expect("this build reads its own file");
    let field = |agent: &str| {
        let r = now.iter().find(|r| r.agent_id == agent).unwrap_or_else(|| panic!("{agent} is missing"));
        (r.first_context_tokens, r.detected_cache_ttl_minutes)
    };
    assert_eq!(field("w-new"), (Some(40_000), Some(60)));
    assert_eq!(field("rev-3694"), (None, None), "an old row reads as unknown, never as a number");
    assert_eq!(cost_row(&reg.group_usage(&g.id), "w-new")["cache_ttl_minutes"], json!(60));
}
