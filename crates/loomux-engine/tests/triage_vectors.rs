//! The RUST half of #3304 S2's cross-language pin.
//!
//! `test/fixtures/orchtriage/vectors.json` holds delivery cases with the
//! answers `triage::classify`, `triage::never_triaged` and `triage::decide`
//! must give. This file asserts the ENGINE agrees with that file;
//! `test/orchtriageeval.test.ts` asserts `scripts/orch-triage-eval.cjs`'s
//! mirror agrees with the same file. One fixture, two readers, one CI run — so
//! a behavioural divergence between the rule tier and the replay harness
//! reddens on whichever side moved, rather than silently making the eval a
//! measurement of a second, different classifier.
//!
//! **Why the harness mirrors the rules at all, rather than calling them.** The
//! rule table is hand-written prefix tests over `&str`; there is no data an
//! engine export could hand a Node script, and a `--dump-rules` flag would
//! export the class NAMES and leave the decisions behind — the half that can
//! diverge without anyone noticing. Shipping a Rust binary the script shells
//! out to would put a `cargo` build on the eval's path, which this repo's
//! workers cannot run at all. A mirror plus this pin is the seam that costs
//! one test file and no product surface.
//!
//! **This test is the SOURCE-OF-TRUTH side.** If it fails, the fixture is
//! wrong or `triage.rs` changed behaviour; the JS mirror is never the thing to
//! edit first.

use std::path::PathBuf;

use loomux_engine::triage::{self, Decision, DeliverReason, Input, Kind, Policy, Rule};
use serde_json::Value;

fn fixture() -> Value {
    // `CARGO_MANIFEST_DIR` is `<repo>/crates/loomux-engine`.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("test");
    p.push("fixtures");
    p.push("orchtriage");
    p.push("vectors.json");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", p.display()))
}

/// The `Decision` as the fixture spells it, so a mismatch prints both sides in
/// the fixture's own vocabulary rather than as a `Debug` dump a reader then has
/// to translate.
fn decision_json(d: Decision) -> Value {
    match d {
        Decision::Deliver(r) => serde_json::json!({ "action": "deliver", "reason": r.as_str() }),
        Decision::Defer(r) => serde_json::json!({ "action": "defer", "rule": r.as_str() }),
    }
}

#[test]
fn every_golden_vector_matches_the_engine() {
    let doc = fixture();
    let cases = doc["cases"].as_array().expect("cases[]");
    // Positive control: an empty fixture would make every assertion below
    // vacuous, and a zero-case run reads exactly like a clean one.
    assert!(cases.len() >= 20, "expected a real corpus, got {}", cases.len());

    for case in cases {
        let name = case["name"].as_str().expect("name");
        let text = case["text"].as_str().expect("text");
        let human_actor = case["human_actor"].as_bool().expect("human_actor");
        // `merge_queue_enabled` was read here until #3324 retired the rule
        // that consumed it. The fixture still carries the field on every case
        // and it is now inert; it is left in place rather than swept out of
        // 42 cases in a slice about the rule, and nothing reads it.

        let kinds: Vec<Kind> = case["policy"]["kinds"]
            .as_array()
            .expect("policy.kinds")
            .iter()
            .map(|k| {
                let s = k.as_str().expect("kind string");
                Kind::parse(s).unwrap_or_else(|| panic!("{name}: unknown kind {s:?}"))
            })
            .collect();
        let policy = Policy {
            enabled: case["policy"]["enabled"].as_bool().expect("policy.enabled"),
            kinds,
            max_defer_minutes: triage::TRIAGE_MAX_DEFER_MINUTES_DEFAULT,
        };

        assert_eq!(
            triage::classify(text).as_str(),
            case["expect"]["kind"].as_str().expect("expect.kind"),
            "{name}: classify",
        );

        let never = triage::never_triaged(text, human_actor).map(|r| r.as_str());
        let want_never = case["expect"]["never"].as_str();
        assert_eq!(never, want_never, "{name}: never_triaged");

        let got = decision_json(triage::decide(
            &Input { text, from: "w-1", human_actor },
            &policy,
        ));
        assert_eq!(got, case["expect"]["decision"], "{name}: decide");
    }
}

/// The vectors must EXERCISE the table, not merely agree with it on whatever
/// they happen to cover: a rule with no case is a rule the cross-language pin
/// is blind to, and the blindness is silent.
///
/// `test/orchtriageeval.test.ts` asserts the same coverage from the JS side, so
/// neither reader can be the only one that noticed.
#[test]
fn the_corpus_exercises_every_rule_and_every_deliver_reason() {
    let doc = fixture();
    let cases = doc["cases"].as_array().expect("cases[]");

    let mut rules_seen: Vec<String> = Vec::new();
    let mut reasons_seen: Vec<String> = Vec::new();
    for case in cases {
        let d = &case["expect"]["decision"];
        if let Some(r) = d["rule"].as_str() {
            rules_seen.push(r.to_string());
        }
        if let Some(r) = d["reason"].as_str() {
            reasons_seen.push(r.to_string());
        }
    }

    // There is no carve-out any more. `gate-satisfied` used to be the one rule
    // no vector could name — `decide` answered `TryEnqueue` and only a real,
    // impure merge-queue enqueue turned that into a defer — and #3324 retired
    // it, so every rule this enum has is now nameable by a vector and the list
    // below is the whole of `Rule`. The counterfactual gate notice is a vector
    // too, expecting DELIVER.
    let want_rules: Vec<&str> = vec![
        Rule::RunGreen.as_str(),
        Rule::ChecksGreen.as_str(),
        Rule::PlannerExited.as_str(),
        Rule::AgentExited.as_str(),
        Rule::DriveCancelled.as_str(),
        Rule::PlanChunk.as_str(),
    ];
    for r in &want_rules {
        assert!(rules_seen.iter().any(|s| s == r), "no vector exercises rule {r}");
    }
    // The retirement, from the fixture's side: no case may expect the retired
    // action or the retired rule, so re-adding either to the engine without
    // re-adding it here cannot pass, and re-adding a vector for it cannot pass
    // either.
    for c in cases {
        let d = &c["expect"]["decision"];
        assert_ne!(d["action"], "try-enqueue", "the try-enqueue action is retired (#3324)");
        assert_ne!(d["rule"], "gate-satisfied", "the gate-satisfied rule is retired (#3324)");
    }

    for r in [
        DeliverReason::Disabled.as_str(),
        DeliverReason::KindNotTriaged.as_str(),
        DeliverReason::NoRule.as_str(),
        triage::NeverReason::HumanActor.as_str(),
        triage::NeverReason::Regrounding.as_str(),
        triage::NeverReason::DriveHeld.as_str(),
        triage::NeverReason::DelegateBlocked.as_str(),
        triage::NeverReason::Watchdog.as_str(),
        triage::NeverReason::NeedsYou.as_str(),
    ] {
        assert!(reasons_seen.iter().any(|s| s == r), "no vector exercises deliver reason {r}");
    }
}
