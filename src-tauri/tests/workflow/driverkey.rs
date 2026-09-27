//! The per-block `driver:` key (#2850 S3a) and `cache_ttl_minutes`.
//!
//! One module of the `workflow` integration-test target (`main.rs`),
//! split out of the former single-file `tests/workflow.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ───────────────────── #2850 S3a: the `driver:` block key ───────────────────
//
// `driver: structured` says HOW loomux drives the block's agent: over the
// CLI's structured-protocol surface (harness adapters) instead of a scraped
// PTY. The VALUE is loomux's own closed vocabulary (`workflow::DRIVER_MODES`);
// whether the block's CLI can carry it is capability data
// (`CliCaps::structured_driver`). Both refusals are parse errors — the same
// load-time posture `cli_can_host` established for the containment question,
// so a repo learns from its own file rather than from a spawn that fails
// hours later.

/// The block id every `driver_block` fixture uses. Deliberately a string no
/// refusal message could contain by accident — see the note in the helper.
const DRIVER_BLOCK_ID: &str = "zephyr-drv";

/// One worker block on `cli`, with the optional `driver:` key.
fn driver_block(cli: &str, mode: Option<&str>) -> String {
    // A DISTINCTIVE id, not "w": the refusal text carries ordinary English
    // ("loomux drives over its structured protocol"), so asserting the block
    // id with `contains("w")` matched the prose rather than the id — it would
    // have passed against a refusal that never named the block at all.
    let mut text = format!(
        "version: 1\nblocks:\n  - id: {DRIVER_BLOCK_ID}\n    kind: worker\n    cli: {cli}\n"
    );
    if let Some(mode) = mode {
        text.push_str(&format!("    driver: {mode}\n"));
    }
    text
}

#[test]
fn driver_structured_parses_on_a_cli_whose_row_carries_a_driver() {
    let parsed = workflow::parse_workflow(&driver_block("pi", Some("structured")))
        .expect("pi is the one CLI with a structured driver — the key must parse");
    let w = parsed.blocks.iter().find(|b| b.id == DRIVER_BLOCK_ID).unwrap();
    assert_eq!(
        w.driver.as_deref(),
        Some("structured"),
        "the normalized value reaches the parsed block"
    );

    // The negative control: a block WITHOUT the key is the absent behavior —
    // a PTY pane — and parses exactly as it did before the key existed.
    let parsed = workflow::parse_workflow(&driver_block("pi", None)).unwrap();
    let w = parsed.blocks.iter().find(|b| b.id == DRIVER_BLOCK_ID).unwrap();
    assert_eq!(w.driver, None, "no driver: key is a PTY pane");
}

#[test]
fn driver_structured_is_refused_on_a_cli_whose_row_carries_none() {
    // Every CLI whose `structured_driver` is None — claude's decoder exists
    // but its spawn wiring is #84's R2, so its row (and this loop) refuse it
    // today. R2 flips its row and amends this loop in the same PR.
    for cli in ["opencode", "claude", "codex", "copilot", "gemini"] {
        let text = driver_block(cli, Some("structured"));
        let errs = workflow::parse_workflow(&text).err().unwrap_or_else(|| {
            panic!("{cli}: a CLI with no structured driver must refuse the key")
        });
        assert!(
            errs.iter()
            .any(|e| e.contains(DRIVER_BLOCK_ID)
                && e.contains(cli)
                && e.contains("no structured driver")),
            "{cli}: the refusal must name the block and the CLI: {errs:?}"
        );
    }
}

#[test]
fn driver_unknown_value_is_refused_before_the_cli_question() {
    // The vocabulary is loomux's own, so a value outside it is refused even on
    // the CLI that HAS a driver — the same order `validate_knob` checks in,
    // and the same "a typo is never coerced" posture.
    let text = driver_block("pi", Some("pty"));
    let errs = workflow::parse_workflow(&text)
        .err()
        .expect("an unknown driver value must be refused");
    assert!(
        errs.iter()
            .any(|e| e.contains("unknown driver") && e.contains("pty") && e.contains("structured")),
        "the refusal names the value and the closed set: {errs:?}"
    );
}

#[test]
fn list_blocks_rows_carry_the_driver_key() {
    let wf = "version: 1\nname: driven\n\
              blocks:\n\
              \x20 - id: w\n    kind: worker\n    cli: pi\n    driver: structured\n\
              \x20 - id: p\n    kind: worker\n";
    let repo = Repo::new().workflow(wf);
    let (reg, _dir) = test_registry();
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    let v = reg.list_blocks(&g.id);
    let rows = v["blocks"].as_array().unwrap();
    let w = rows.iter().find(|b| b["id"] == json!("w")).unwrap();
    assert_eq!(
        w["driver"],
        json!("structured"),
        "the declared value is on the row: {v}"
    );

    // Additive, in both directions: a block without the key still carries the
    // KEY (null), so a reader can tell "no driver" from "a build that never
    // heard of the key".
    let p = rows.iter().find(|b| b["id"] == json!("p")).unwrap();
    assert!(
        p.as_object().unwrap().contains_key("driver"),
        "the key is present even on a block that omits it: {p}"
    );
}

#[test]
fn driver_structured_is_refused_at_spawn_when_the_block_inherits_its_cli() {
    // #2850 S3b, and the case `parse_workflow` structurally cannot reach.
    //
    // The block spells no `cli:` of its own, so the parser computes
    // `caps: None` and skips the CLI half of the driver check entirely. The
    // file is therefore ACCEPTED — asserted here, because if a later change
    // made the parse refuse it, this test would pass for a different reason
    // and stop covering the spawn path at all.
    let (reg, _dir) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n  - id: inherits\n    kind: worker\n    driver: structured\n",
    );
    // `rails()` carries `agent_cli: "claude"`, whose CliCaps row has no
    // structured driver — so THAT is the inherited value that must be caught.
    let g = reg
        .create_group(&repo.path(), rails())
        .expect("the parser cannot judge an inherited cli, so the file must parse");
    assert_eq!(
        g.guardrails.block("inherits").unwrap().driver.as_deref(),
        Some("structured"),
        "the key survived the parse — which is exactly why spawn has to ask again"
    );

    let err = reg
        .spawn_agent_ex(
            &g.id, Role::Worker, Some("inherits".into()), "", "t", false,
            None, None, None, None, None,
        )
        .expect_err("a structured block on an inherited claude must be refused at spawn");
    assert!(
        err.contains("inherits") && err.contains("claude") && err.contains("no structured driver"),
        "the spawn refusal must name the block and the RESOLVED cli: {err}"
    );
    // The remedy names a CLI that would work, derived from the table.
    assert!(err.contains("pi"), "the refusal must name a CLI that has one: {err}");
}

#[test]
fn an_inherited_cli_that_does_carry_a_driver_passes_the_driver_gate() {
    // The positive control for the test above, and the half that keeps it from
    // passing by refusing everything: without this, "refuse every structured
    // block" would satisfy that assertion just as well as "refuse the wrong
    // CLI".
    //
    // It asks the GATE rather than driving a spawn, and that is a constraint-3
    // requirement rather than a convenience. Once #2850 S3b wired the
    // structured arm, a spawn on an inherited `pi` stops being inert: it
    // resolves a program and starts it. On CI that fails with "pi is not on
    // PATH" — which is how this test was caught — but on a developer machine
    // with pi installed it would START A REAL AGENT CLI. A test that passes
    // everywhere and spawns a paid agent on one machine is worse than one that
    // fails, so the spawn half of this control is deliberately not taken.
    //
    // The refusal case above is unaffected and still drives the real spawn
    // path: it is REFUSED before anything is resolved or started.
    let want = workflow::structured_harness_for(Some("structured"), "pi")
        .expect("pi has a structured driver, so the gate must admit it");
    assert_eq!(
        want,
        Some(loomux_engine::harness::Harness::Pi),
        "the gate admitted the block but resolved the wrong harness"
    );

    // And the same question asked of the inherited value the refusal test uses,
    // so the two halves are provably about the same predicate rather than two
    // spellings that happen to agree.
    let refused = workflow::structured_harness_for(Some("structured"), "claude")
        .expect_err("claude carries no structured driver yet (#84 R2)");
    assert!(
        refused.to_string().contains("no structured driver"),
        "the refusal must say WHY: {refused}"
    );
}

#[test]
fn a_group_json_driver_outside_the_vocabulary_is_dropped_and_never_coerced() {
    // The one input `parse_workflow` never sees, pinned at its CURRENT
    // behaviour so a future change that starts COERCING a typo reddens here
    // instead of silently spawning a structured pane the operator never asked
    // for. `read_blocks` drops an out-of-vocabulary value because there is no
    // human at that layer to show a parse error to.
    let (reg, _dir) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n  - id: typo\n    kind: worker\n    cli: pi\n    driver: structured\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();
    let path = reg.state_root().join(g.id.as_str()).join("group.json");

    let mut edited: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    for b in edited["guardrails"]["blocks"].as_array_mut().unwrap() {
        if b["id"] == "typo" {
            // The operator meant `structured` and mistyped it.
            b["driver"] = Value::String("structrued".into());
        }
    }
    fs::write(&path, serde_json::to_string_pretty(&edited).unwrap()).unwrap();

    let (_, back) = reg.load_group_file(&g.id).expect("the edited group file must still load");
    assert_eq!(
        back.block("typo").unwrap().driver,
        None,
        "a driver outside DRIVER_MODES must be DROPPED, never coerced to the near miss"
    );
    // The control that stops the assertion above passing for the wrong reason:
    // the block is still there and still a worker. A dropped block would
    // satisfy it just as well.
    assert_eq!(back.block("typo").unwrap().kind, Role::Worker);
    assert_eq!(back.block("typo").unwrap().cli, "pi");
}

#[test]
fn a_bare_driver_key_is_the_absent_key_and_the_value_is_case_normalised() {
    // Two of the deliberate-but-untested paths S3a shipped.
    //
    // A bare `driver:` line parses as an empty value, which means the absent
    // key — a PTY pane — rather than a refusal about nothing.
    let bare = workflow::parse_workflow(&format!(
        "version: 1\nblocks:\n  - id: {DRIVER_BLOCK_ID}\n    kind: worker\n    cli: pi\n    driver:\n"
    ))
    .expect("a bare driver: key must parse, not refuse");
    assert_eq!(
        bare.blocks.iter().find(|b| b.id == DRIVER_BLOCK_ID).unwrap().driver,
        None,
        "a bare driver: is the absent key"
    );

    // And the value is normalised rather than matched literally, so a file
    // written in caps is accepted and stored in ONE spelling.
    for spelled in ["STRUCTURED", "Structured", "  structured  "] {
        let parsed = workflow::parse_workflow(&driver_block("pi", Some(spelled)))
            .unwrap_or_else(|e| panic!("{spelled:?} must normalise, got {e:?}"));
        assert_eq!(
            parsed.blocks.iter().find(|b| b.id == DRIVER_BLOCK_ID).unwrap().driver.as_deref(),
            Some("structured"),
            "{spelled:?} must reach the block in ONE normalised spelling"
        );
    }
}

#[test]
fn the_driver_key_survives_a_group_json_round_trip() {
    // `blocks_json` rewrites the whole roster on every change, so a field it
    // forgot would vanish on the next resume with nothing to see — the same
    // hazard `remote` is pinned against.
    let (reg, _dir) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n  - id: driven\n    kind: worker\n    cli: pi\n    driver: structured\n  - id: plain\n    kind: worker\n    cli: pi\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    let path = reg.state_root().join(g.id.as_str()).join("group.json");
    let gj: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let on_disk = gj["guardrails"]["blocks"].as_array().unwrap();
    assert_eq!(
        on_disk.iter().find(|b| b["id"] == "driven").unwrap()["driver"],
        json!("structured"),
        "the value is on disk as a plain string"
    );

    let (_, back) = reg.load_group_file(&g.id).expect("the group file must load");
    assert_eq!(back.block("driven").unwrap().driver.as_deref(), Some("structured"));
    assert_eq!(
        back.block("plain").unwrap().driver,
        None,
        "a block that never declared one must not acquire one on the way back"
    );
}

#[test]
fn the_structured_driver_refusal_renders_as_one_paragraph() {
    // The #1426 B2 shape, on a message a HUMAN reads: once as a parse error in
    // the workflow surface, and again prefixed `guardrail:` at spawn.
    //
    // This exists because the review round found the message shipping CR + LF
    // + 13 spaces, and NOTHING went red -- the pins on this refusal all assert
    // `.contains(<substring>)`, and no asserted substring straddles a line
    // break, so a fully green suite said nothing about its shape. That is the
    // gap `is_one_paragraph` was written for on the manager refusals; the same
    // gap, on a message added later.
    //
    // Shape BESIDE content, not instead of it: the content assertions below
    // would pass on a message broken across three lines.
    let refusal = workflow::structured_harness_for(Some("structured"), "claude")
        .expect_err("claude carries no structured driver yet");
    let msg = refusal.to_string();

    assert!(
        !msg.contains('\n'),
        "the refusal ships a hard line break: {msg:?}"
    );
    assert!(
        !msg.contains('\r'),
        "the refusal ships a carriage return -- the `\r` escape at a source line \
         end, which also stops the newline being a continuation: {msg:?}"
    );
    assert!(
        !msg.contains("          "),
        "the refusal leaked its source indentation: {msg:?}"
    );

    // And it still says the three things it is for.
    assert!(msg.contains("claude"), "{msg}");
    assert!(msg.contains("no structured driver"), "{msg}");
    assert!(msg.contains("pi"), "the remedy must name a CLI that has one: {msg}");
}


#[test]
fn a_structured_spawn_that_fails_after_the_insert_leaves_no_ghost_row() {
    // Review round 1, finding 2. Both structured failure paths happen AFTER
    // `agents.insert` + `persist_agent_record(.., "running")`, so a bare `?`
    // does not fail a spawn -- it leaks a slot.
    //
    // The leak is silent and permanent: the row counts against `max_agents`
    // (the cap counts every row that is not Dead), `kill_agent` refuses it
    // because it has no `pty_id`, and nothing expires it -- the spawn-expiry
    // paths key on `orch-spawn-request`, which the structured arm never emits.
    // The trigger is the ORDINARY case: a machine with no `pi` on PATH, where
    // every attempt would leave another ghost until the group could spawn
    // nothing at all.
    //
    // Driven through the teardown itself rather than through a failing spawn,
    // and that is a deliberate trade with its cost stated. Reaching the real
    // Err arm needs `resolve_program` to fail, which needs an empty `PATH`: a
    // PROCESS-global mutation, in a binary whose other tests run in parallel
    // and shell out to git. That test would pass and break its neighbours
    // intermittently, which is a worse defect than the one it covers. So this
    // pins the teardown deterministically, and that the two Err arms route
    // through it is held by reading rather than by execution.
    let (reg, _dir) = test_registry();
    let repo = Repo::new().workflow(
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: claude\n",
    );
    let g = reg.create_group(&repo.path(), rails()).unwrap();

    // A real, live roster row -- the state a structured spawn is in at the
    // moment its program fails to resolve.
    let a = reg
        .spawn_agent_ex(
            &g.id, Role::Worker, Some("w".into()), "", "t", false,
            None, None, None, None, None,
        )
        .expect("a PTY block spawns in test mode");

    let status_of = |id: &str| -> String {
        reg.list_agents(&g.id)
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == json!(id))
            .map(|r| r["status"].as_str().unwrap_or("?").to_string())
            .unwrap_or_else(|| "(absent)".into())
    };
    assert_ne!(
        status_of(&a.id),
        "dead",
        "the row must start alive, or this test proves nothing"
    );
    assert!(
        reg.resolve_token(&a.token).is_some(),
        "the token must start resolvable, or the assertion below is vacuous"
    );

    let err = reg.abandon_structured_spawn(&g.id, &a.id, &a.token, "pi is not on PATH".into());

    assert!(err.contains("not on PATH"), "the error is handed back to be returned: {err}");
    assert_eq!(
        status_of(&a.id),
        "dead",
        "the abandoned row must be Dead -- a Starting ghost counts against max_agents and \
         kill_agent refuses it"
    );
    // The token is what an agent authenticates its MCP calls with. A row that
    // is Dead but still resolvable by token is a worse leak than the slot.
    assert!(
        reg.resolve_token(&a.token).is_none(),
        "the abandoned row is still reachable by its token"
    );
}

/// #3407: one worker block with an optional `cache_ttl_minutes:` value.
fn ttl_block(ttl: Option<&str>) -> String {
    let mut text = String::from("version: 1\nblocks:\n  - id: quokka-ttl\n    kind: worker\n    cli: claude\n");
    if let Some(v) = ttl {
        text.push_str(&format!("    cache_ttl_minutes: {v}\n"));
    }
    text
}

#[test]
fn cache_ttl_minutes_parses_across_its_range_and_absent_stays_absent() {
    for (raw, want) in [("60", Some(60)), ("0", Some(0)), ("1440", Some(1440))] {
        let parsed = workflow::parse_workflow(&ttl_block(Some(raw)))
            .unwrap_or_else(|e| panic!("cache_ttl_minutes: {raw} must parse: {e:?}"));
        let b = parsed.blocks.iter().find(|b| b.id == "quokka-ttl").unwrap();
        assert_eq!(b.cache_ttl_minutes, want, "{raw}");
    }
    // The negative control: no key is the CLI default, never a pinned number.
    let parsed = workflow::parse_workflow(&ttl_block(None)).unwrap();
    let b = parsed.blocks.iter().find(|b| b.id == "quokka-ttl").unwrap();
    assert_eq!(b.cache_ttl_minutes, None);
}

#[test]
fn cache_ttl_minutes_above_a_day_is_refused_naming_the_block() {
    let errs = workflow::parse_workflow(&ttl_block(Some("1441")))
        .err()
        .expect("a TTL above a day is a typo and must be refused, not clamped");
    assert!(
        errs.iter().any(|e| e.contains("quokka-ttl") && e.contains("cache_ttl_minutes")),
        "the refusal names the block and the key: {errs:?}"
    );
    // A value of the wrong TYPE is serde's refusal — the key is a number.
    assert!(workflow::parse_workflow(&ttl_block(Some("soon"))).is_err());
}
