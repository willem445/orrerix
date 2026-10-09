//! The next-prompt cost estimate's backend half (#3831): what the usage row
//! carries for `src/promptcost.ts` to compute from, and that the two fields it
//! adds to the persisted row are additive in both directions.
//!
//! One module of the `orchestration` integration-test target (`main.rs`).
//! Layout rules: docs/design/module-layout.md. The TTL ladder and the idle
//! backstop's half of #3831 are in `cacheage.rs`, beside the rest of the TTL.

use super::*;

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

    assert_eq!(row["source"], "transcript", "control: the row really is read off the fixture: {row}");
    // C: what the NEWEST turn was sent — not the session's running total.
    assert_eq!(row["context_tokens"], json!(250_302));
    assert_ne!(row["tokens"]["total"], row["context_tokens"]);
    // F: what the FIRST turn was sent.
    assert_eq!(row["first_context_tokens"], json!(40_000));
    // p: Sonnet 5.5's own row, resolved here so the frontend keeps no table.
    assert_eq!(row["price_model"], "claude-sonnet-5-5");
    assert_eq!(row["price_basis"], "listed");
    assert_eq!(row["price_per_mtok"]["input"], json!(2.0), "Sonnet 5.5 is $2 input, not Sonnet 4.6's $3");
    assert_eq!(row["price_per_mtok"]["cache_read"], json!(0.10));
    assert_eq!(row["price_per_mtok"]["cache_write"], json!(2.5));
    assert_eq!(row["price_per_mtok"]["cache_write_1h"], json!(4.0));
    assert_eq!(row["price_per_mtok"]["output"], json!(10.0));
    assert_eq!(row["price_long_prompt"], Value::Null);
    assert_eq!(row["price_dated"], "2026-10-09");
    assert_eq!(row["prompt_chars_per_token"], json!(3.5 / 1.3));
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
    assert_eq!(row["price_basis"], "family-ceiling");
    assert_eq!(row["price_per_mtok"]["input"], json!(3.0));
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
    assert_eq!(row["price_per_mtok"]["input"], json!(0.10));
    assert_eq!(row["price_long_prompt"]["over_tokens"], json!(100_000));
    assert_eq!(row["price_long_prompt"]["price"]["input"], json!(0.50));
    assert_eq!(row["price_long_prompt"]["price"]["cache_read"], json!(0.05));
    // Its writes went to the five-minute cache, and the row says so.
    assert_eq!(row["cache_ttl_minutes"], json!(5));
    assert_eq!(row["cache_ttl_source"], "session");

    // A model the table does not list is tokens-only: every price field is
    // null — never a zero — while the token readings are still there.
    let row = cost_row(&usage, &future.id);
    assert_eq!(row["source"], "transcript", "control: this row was read too: {row}");
    for key in ["price_model", "price_per_mtok", "price_long_prompt", "price_basis", "price_dated", "prompt_chars_per_token"] {
        assert_eq!(row[key], Value::Null, "{key} must be null for an unpriced model: {row}");
    }
    assert_eq!(row["cost_usd"], Value::Null);
    assert_eq!(row["context_tokens"], json!(510));
    assert_eq!(row["first_context_tokens"], json!(510));
}

#[test]
fn a_row_whose_cli_reports_its_own_dollars_is_never_priced_off_the_table() {
    // The gate is the row's own provenance, never its model id and never its
    // CLI's name. pi reports the dollars it paid, so its row is `estimated:
    // false` — and a pi model id that names a Claude family (`anthropic/…`
    // through pi) must not pull an Anthropic list price onto it.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let reported = UsageSnapshot {
        estimated: false,
        cli: "pi".to_string(),
        source: "pi-transcript".to_string(),
        model: Some("anthropic/claude-opus-4-8".to_string()),
        current_model: Some("anthropic/claude-opus-4-8".to_string()),
        first_context_tokens: Some(12_000),
        ..usage_snap("sess-reported", "w-reported", 0.4, 1_000, 10)
    };
    // The control: the SAME model id on a row whose dollars are ours.
    let estimated = UsageSnapshot { estimated: true, ..reported.clone() };
    let estimated = UsageSnapshot { key: "sess-estimated".to_string(), agent_id: "w-estimated".to_string(), ..estimated };
    reg.upsert_usage_snapshot(&g.id, reported);
    reg.upsert_usage_snapshot(&g.id, estimated);
    let usage = reg.group_usage(&g.id);

    let row = cost_row(&usage, "w-reported");
    assert_eq!(row["price_per_mtok"], Value::Null, "pi's dollars are pi's: {row}");
    assert_eq!(row["price_dated"], Value::Null);
    assert_eq!(row["prompt_chars_per_token"], Value::Null);
    // The token half of the estimate does not depend on a price.
    assert_eq!(row["first_context_tokens"], json!(12_000));
    // pi has no default TTL and nothing was detected: no state is claimed.
    assert_eq!(row["cache_ttl_minutes"], Value::Null);
    assert_eq!(row["cache_ttl_source"], Value::Null);

    let row = cost_row(&usage, "w-estimated");
    assert_eq!(row["price_per_mtok"]["input"], json!(5.0), "the same id IS priced where the dollars are ours: {row}");
    // Neither row is a live agent, so neither has a context reading.
    assert_eq!(row["context_tokens"], Value::Null);
}

/// A `usage.json` in the shape 1.3.1-beta8 writes: every field `UsageSnapshot`
/// has at that release, in its order, and neither of the two #3831 adds.
const BETA8_USAGE_JSON: &str = r#"[
  {
    "key": "sess-old",
    "agent_id": "w-old",
    "name": "w-old",
    "role": "worker",
    "source": "transcript",
    "block": "worker",
    "cli": "claude",
    "input_tokens": 1200,
    "output_tokens": 340,
    "cache_creation_tokens": 5000,
    "cache_read_tokens": 910000,
    "cost_usd": 1.25,
    "estimated": true,
    "model": "claude-opus-4-8",
    "current_model": "claude-opus-4-8",
    "updated_ms": 1759900000000,
    "activity": {
      "last_active_ms": 1759900000000,
      "last_wake": null,
      "baseline": {
        "source": "transcript",
        "counters": {
          "input": 1200,
          "output": 340,
          "cache_creation": 5000,
          "cache_read": 910000
        },
        "cost_usd": 1.25
      }
    }
  }
]"#;

/// The row as a build BEFORE #3831 declares it — the same fields, and like
/// `UsageSnapshot` no `deny_unknown_fields`. Reading today's file through this
/// is what "an older build still reads what this one writes" means.
#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct Beta8Row {
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

#[test]
fn a_usage_file_from_before_the_prompt_cost_fields_loads_unchanged() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("usage.json");
    fs::write(&path, BETA8_USAGE_JSON).unwrap();

    // It loads: the row is there with its figures, and the two new fields
    // read as unknown rather than failing the file or defaulting to a number.
    let usage = reg.group_usage(&g.id);
    let row = cost_row(&usage, "w-old");
    assert_eq!(row["tokens"]["total"], json!(1200 + 340 + 5000 + 910_000));
    assert_eq!(row["cost_usd"], json!(1.25));
    assert_eq!(row["last_active_ms"], json!(1_759_900_000_000u64));
    assert_eq!(row["first_context_tokens"], Value::Null);
    assert_eq!(row["cache_ttl_minutes"], json!(5), "nothing detected on an old row: the CLI default");
    assert_eq!(row["cache_ttl_source"], "cli");
    // And reading it did not rewrite it: the bytes are the ones beta8 wrote.
    assert_eq!(fs::read_to_string(&path).unwrap(), BETA8_USAGE_JSON, "a read is not a migration");
    assert_eq!(audit_count(&reg, &g.id, "usage-corrupt"), 0);

    // A row with nothing to say in the new fields persists in beta8's own
    // shape: the keys are absent, not `null`.
    reg.upsert_usage_snapshot(&g.id, usage_snap("sess-plain", "w-plain", 0.1, 10, 1));
    let disk = fs::read_to_string(&path).unwrap();
    assert!(disk.contains("\"sess-plain\""), "control: the write happened: {disk}");
    assert!(!disk.contains("first_context_tokens"), "{disk}");
    assert!(!disk.contains("detected_cache_ttl_minutes"), "{disk}");

    // A row that DOES carry them is written with them, and the file an older
    // build then meets still parses as that build's row list — all three rows,
    // old figures intact, the unknown keys ignored.
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
    let as_beta8: Vec<Beta8Row> = serde_json::from_str(&disk).expect("an older build reads the new file");
    assert_eq!(as_beta8.len(), 3);
    let old = as_beta8.iter().find(|r| r.key == "sess-old").expect("the beta8 row survived");
    assert_eq!((old.input_tokens, old.cache_read_tokens, old.cost_usd), (1200, 910_000, Some(1.25)));
    // ...and this build reads them back.
    let row = cost_row(&reg.group_usage(&g.id), "w-new").clone();
    assert_eq!(row["first_context_tokens"], json!(40_000));
    assert_eq!(row["cache_ttl_minutes"], json!(60));
}
