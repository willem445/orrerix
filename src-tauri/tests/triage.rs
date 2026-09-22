//! Integration tests for delivery triage (#3304 S1).
//!
//! Design note: `docs/design/delivery-triage.md`. The engine's own unit tests
//! (`loomux_engine::triage`) pin the DECISION — which shape closes by which
//! rule, and what the never-triaged set always answers. What can only be
//! pinned here is everything that makes the decision real:
//!
//! - a deferred notice never reaches the pane (no `prompt` row is written for
//!   it, which is the only durable evidence of "offered to a pane");
//! - it is FLUSHED, as one framed delivery, in front of the next genuine wake;
//! - it is flushed at the DEADLINE with no wake at all;
//! - it is never DROPPED — `deferred.json` survives a fresh registry over the
//!   same state root;
//! - `provider: none` is the only value that loads, and nothing on the
//!   delivery path can reach a network.
//!
//! # Why a new file rather than `tests/orchestration.rs`
//!
//! `tests/mergequeue.rs`'s reason, plus one of this feature's own: the design
//! note says every path that can hold a notice back lives in ONE file
//! (`src/orchestration/triagegate.rs`), and a test target that reads it as one
//! scope is the other half of that. `tests/smoke.rs` is untouched — CLAUDE.md
//! constraint 4 needs at least one integration-test target for the Windows
//! comctl32-v6 manifest link args, and this adds one more rather than
//! replacing it.
//!
//! # No network, no real CLIs
//!
//! CLAUDE.md constraint 3. Nothing here spawns an agent CLI, `git`, or `gh`.

use loomux_lib::orchestration::mcp::dispatch;
use loomux_lib::orchestration::triage;
use loomux_lib::orchestration::workflow::parse_workflow;
use loomux_lib::orchestration::{
    Caller, Delivery, GroupId, Guardrails, Launch, OrchRegistry, Role,
};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

// ── harness ─────────────────────────────────────────────────────────────────

fn relaunch_registry(dir: &Path) -> OrchRegistry {
    let reg = OrchRegistry::new(dir.to_path_buf());
    reg.set_port(45997);
    reg.set_claude_agents_dir_override(dir.join("claude-agents"));
    reg.set_copilot_agents_dir_override(dir.join("copilot-agents"));
    reg.set_compact_hook_dir_override(dir.join("compacthook"));
    reg.set_copilot_hooks_dir_override(dir.join("copilot-hooks"));
    reg
}

fn rails() -> Guardrails {
    Guardrails {
        max_agents: 6,
        agent_cli: "claude".into(),
        auto_ops: false,
        advanced_orchestrator: true,
        ..Guardrails::default()
    }
}

struct Repo {
    _root: tempfile::TempDir,
    repo: PathBuf,
}

impl Repo {
    fn with_workflow(yaml: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let dir = repo.join(".loomux");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("workflow.yml"), yaml).unwrap();
        Repo { _root: root, repo }
    }
    fn path(&self) -> String {
        self.repo.to_string_lossy().replace('\\', "/")
    }
}

/// A roster with one orchestrator block, plus whatever `extra` declares. The
/// blocks are what makes the file load at all; the point of every test below
/// is the `triage:` block.
fn workflow_yaml(extra: &str) -> String {
    format!(
        "version: 1\n\
         name: t\n\
         {extra}\n\
         blocks:\n\
         \x20 - id: orchestrator\n\
         \x20   kind: orchestrator\n\
         \x20   cli: claude\n\
         \x20 - id: worker\n\
         \x20   kind: worker\n\
         \x20   cli: claude\n"
    )
}

const TRIAGE_ON: &str = "triage:\n  enabled: true\n  provider: none";

/// A group with `triage: enabled: true`, a live orchestrator on a pty, and a
/// live worker on another.
struct Fixture {
    reg: OrchRegistry,
    dir: tempfile::TempDir,
    _repo: Repo,
    g: GroupId,
    orch: String,
    worker: String,
}

fn fixture_with(extra: &str) -> Fixture {
    let repo = Repo::with_workflow(&workflow_yaml(extra));
    let dir = tempfile::tempdir().unwrap();
    let reg = relaunch_registry(dir.path());
    let g = reg.create_group_ex(&repo.path(), rails(), Launch::Fresh).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let worker = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    reg.set_pty_for_test(&orch.id, 8801);
    reg.set_pty_for_test(&worker.id, 8802);
    Fixture { reg, dir, _repo: repo, g: g.id, orch: orch.id, worker: worker.id }
}

fn fixture() -> Fixture {
    fixture_with(TRIAGE_ON)
}

impl Fixture {
    /// Deliver `text` to the orchestrator as a mid-session notice from
    /// `from`, the way every producer does. The `Result` is deliberately
    /// ignored: a headless registry has no app handle, so a delivery that got
    /// PAST triage fails at the drainer — after the `prompt` row is written,
    /// which is what every assertion below reads.
    fn notify(&self, text: &str) {
        let _ = self.reg.deliver_prompt(&self.orch, text, "w-1", Delivery::MidSession);
    }
    fn deferred_path(&self) -> PathBuf {
        self.reg.state_root().join(self.g.as_str()).join(triage::DEFERRED_FILE)
    }
    /// Every `prompt` row's text, in order — "what was offered to a pane".
    fn prompts(&self) -> Vec<String> {
        self.reg
            .audit_log(&self.g)
            .into_iter()
            .filter(|e| e.action == "prompt")
            .map(|e| e.detail["text"].as_str().unwrap_or_default().to_string())
            .collect()
    }
    fn triaged(&self) -> Vec<(String, String)> {
        self.reg
            .audit_log(&self.g)
            .into_iter()
            .filter(|e| e.action == "delivery-triaged")
            .map(|e| {
                (
                    e.detail["kind"].as_str().unwrap_or_default().to_string(),
                    e.detail["action"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }
}

/// Every `delivery-triaged` row's `(kind, action, text)`.
fn triaged_rows(f: &Fixture) -> Vec<(String, String, Option<String>)> {
    f.reg
        .audit_log(&f.g)
        .into_iter()
        .filter(|e| e.action == "delivery-triaged")
        .map(|e| {
            (
                e.detail["kind"].as_str().unwrap_or_default().to_string(),
                e.detail["action"].as_str().unwrap_or_default().to_string(),
                e.detail.get("text").and_then(|t| t.as_str()).map(str::to_string),
            )
        })
        .collect()
}

/// The other half of `tests/orchestration.rs`'s #464 allowlist row for this
/// file (`no_registry_construction_bypasses_the_test_agent_dir_overrides`).
///
/// That row permits exactly one raw `OrchRegistry::new` here, on the stated
/// grounds that it is [`relaunch_registry`] and that [`relaunch_registry`]
/// redirects every generated-agent-file destination away from the real
/// `~/.claude` / `~/.copilot`. A helper that quietly stopped applying one of
/// those overrides would leave the row true about the COUNT and false about the
/// property, and nothing over there could tell.
#[test]
fn its_registry_helper_applies_every_override_this_allowlist_row_assumes() {
    let src =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/triage.rs"))
            .expect("this file reads itself");

    // The helper's body: from its signature to the first line that closes it at
    // column 0 — narrow enough that an override applied by some OTHER function
    // in this file cannot satisfy the assertions below.
    let start = src
        .find("fn relaunch_registry(dir: &Path) -> OrchRegistry {")
        .expect("the sanctioned helper must exist, under the name the row names");
    let body = &src[start..];
    let end = body.find("\n}").expect("the helper must terminate") + 2;
    let body = &body[..end];

    for needed in [
        "set_claude_agents_dir_override",
        "set_copilot_agents_dir_override",
        "set_compact_hook_dir_override",
        "set_copilot_hooks_dir_override",
    ] {
        assert!(
            body.contains(needed),
            "the #464 allowlist row for tests/triage.rs assumes this helper applies every \
             override; it no longer applies {needed}, so a registry built through it can reach \
             the real agent dirs and the row's premise is gone"
        );
    }

    // The population control: the extraction really did isolate the helper, so
    // the four assertions above are about ITS body and not about the whole file.
    assert!(
        body.len() < 1_200,
        "the helper's body extraction ran away ({} chars); the assertions above would then be \
         satisfied by any other function in this file",
        body.len()
    );
    assert!(
        !body.contains("#[test]"),
        "the extraction swallowed a test, so it is no longer reading only the helper"
    );
}

// The real shapes, transcribed from their emitters rather than invented.
//
// The note is RED-ONLY and carries the quotes `notify::watch_fired_notice`
// writes around one. Both matter since #3324: the `run-green` rule now defers
// only when the registered note names no green-path action, and a specimen
// whose note said `green → next merge` (as this one did, unquoted) passed for
// the WRONG reason — the missing quotes put it outside `registered_note`'s
// slice, not outside the marker set.
const RUN_GREEN: &str = "[orrerix] run 17812: completed — conclusion: success. \
                         Note (registered): \"post-merge main; red → INVARIANT 6\". (watch n-1)";
const RUN_RED: &str = "[orrerix] run 17813: completed — conclusion: failure. (watch n-2)";
const HELD: &str = "[orrerix] review drive PR #1758: HELD (ci-red) — three attempts spent.";
const DONE: &str = "[orrerix] w-2902 reports done: #3304 — PR #3310, CI green.";
const PLANNER_EXIT: &str =
    "[orrerix] planner p-1 (plan-2890) posted its plan and exited — its delegate slot is free.";

// ── the deferral ────────────────────────────────────────────────────────────

#[test]
fn a_green_run_notice_is_recorded_instead_of_offered_to_the_pane() {
    let f = fixture();
    f.notify(RUN_GREEN);

    // The load-bearing assertion, and it is about the ABSENCE of a `prompt`
    // row rather than about any state: `prompt` means "offered to a pane"
    // (`front_door_refusals`' own contract), so a deferral that wrote one
    // would be indistinguishable in the log from a delivery.
    assert!(
        !f.prompts().iter().any(|t| t.contains("17812")),
        "the deferred notice must never be offered to the pane: {:?}",
        f.prompts()
    );
    // The positive control for that absence — the mechanism RAN and said so,
    // which an empty log would also satisfy.
    assert_eq!(
        f.triaged(),
        vec![("run-completed".to_string(), "rule:run-green".to_string())],
        "one delivery-triaged row, naming the rule that closed it"
    );
    // And it is on disk, in full, rather than gone.
    let list = f.reg.deferred_list(&f.g);
    assert_eq!(list["count"], Value::from(1));
    assert_eq!(list["enabled"], Value::from(true));
    assert_eq!(list["items"][0]["rule"], Value::from("run-green"));
    assert_eq!(list["items"][0]["text"], Value::from(RUN_GREEN));
    assert!(f.deferred_path().exists(), "deferred.json is written, not held in memory");
}

#[test]
fn the_held_notice_is_flushed_in_front_of_the_next_genuine_wake() {
    let f = fixture();
    f.notify(RUN_GREEN);
    f.notify(PLANNER_EXIT);
    assert_eq!(f.reg.deferred_list(&f.g)["count"], Value::from(2), "both held");

    f.notify(DONE); // a `done` report is never a rule in this slice

    let prompts = f.prompts();
    // Order is the whole point: the frame is admitted FIRST, so the pane
    // reads what it slept through before the thing that woke it.
    let frame = prompts
        .iter()
        .position(|t| t.starts_with("[orrerix] 2 notices deferred"))
        .expect(&format!("the framed flush must be offered to the pane: {prompts:?}"));
    let wake = prompts
        .iter()
        .position(|t| t.contains("reports done"))
        .expect("the genuine wake is delivered too");
    assert!(frame < wake, "the flush rides in front of the wake: {prompts:?}");
    assert!(prompts[frame].contains("17812"), "the frame names what was held: {prompts:?}");
    assert!(prompts[frame].contains("planner-exited"), "…both of them: {prompts:?}");
    // …and where to read them in full. The AUDIT LOG, not `list_deferred()`:
    // the store is cleared before this frame is delivered (review round 1,
    // B1a), which the next assertion is the other half of.
    assert!(
        prompts[frame].contains("audit log as delivery-triaged"),
        "the frame names a record that still exists: {prompts:?}"
    );

    assert_eq!(f.reg.deferred_list(&f.g)["count"], Value::from(0), "the store is cleared");
}

#[test]
fn the_deadline_flushes_with_no_wake_at_all() {
    let f = fixture();
    f.notify(RUN_GREEN);
    let held_at = f.reg.deferred_list(&f.g)["items"][0]["ts_ms"].as_u64().expect("ts");

    // One millisecond before the bound, nothing happens — which is what makes
    // the assertion after it about the DEADLINE rather than about the tick
    // flushing whatever it finds.
    let deadline = held_at + u64::from(triage::TRIAGE_MAX_DEFER_MINUTES_DEFAULT) * 60_000;
    assert!(f.reg.triage_flush_tick(deadline - 1).is_empty(), "not yet due");
    assert_eq!(f.reg.deferred_list(&f.g)["count"], Value::from(1));

    let flushed = f.reg.triage_flush_tick(deadline);
    assert_eq!(flushed, vec![f.g.clone()], "the deadline flushes this group");
    assert!(
        f.prompts().iter().any(|t| t.starts_with("[orrerix] 1 notice deferred")
            && t.contains("deferral deadline")),
        "the frame says WHY it came now: {:?}",
        f.prompts()
    );
    assert_eq!(f.reg.deferred_list(&f.g)["count"], Value::from(0));
}

#[test]
fn a_held_notice_survives_a_restart_rather_than_being_dropped() {
    let f = fixture();
    f.notify(RUN_GREEN);

    // A FRESH registry over the same state root — the restart, as far as
    // anything durable is concerned.
    let reg2 = relaunch_registry(f.dir.path());
    reg2.create_group_ex(&f._repo.path(), rails(), Launch::Resume).unwrap();
    let list = reg2.deferred_list(&f.g);
    assert_eq!(list["count"], Value::from(1), "the held notice is still there: {list}");
    assert_eq!(list["items"][0]["text"], Value::from(RUN_GREEN), "in full, not summarised");
}

/// Review round 1, B1. The deferral's audit row carries the notice's FULL
/// text, and that is the only permanent copy of it.
///
/// `deferred.json` holds the text only until the flush clears it, and the
/// deferrable classes' own emitters audit no text of their own — so without
/// this field, a flushed notice's words exist nowhere, which is what makes
/// "nothing is ever dropped" and the frame's own pointer true or false.
#[test]
fn a_deferrals_audit_row_carries_the_full_text_and_outlives_the_flush() {
    let f = fixture();
    f.notify(RUN_GREEN);

    let rows = triaged_rows(&f);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "run-completed");
    assert_eq!(rows[0].1, "rule:run-green");
    assert_eq!(rows[0].2.as_deref(), Some(RUN_GREEN), "the full text, not a quote of it");

    // Flush it, which empties the store — the state the frame is read in.
    f.notify(DONE);
    assert_eq!(f.reg.deferred_list(&f.g)["count"], Value::from(0));

    // The text survives the clear. This is the assertion the design note's
    // "the notices are in the audit log either way" rests on.
    let after = triaged_rows(&f);
    assert!(
        after.iter().any(|(_, a, t)| a == "rule:run-green" && t.as_deref() == Some(RUN_GREEN)),
        "the flushed notice's text must still be recoverable: {after:?}"
    );

    // A DELIVERED notice carries no `text` here — it already has a `prompt`
    // row, and duplicating it would be a second record to keep in step.
    assert!(
        after.iter().any(|(k, a, t)| k == "delegate-done" && a == "no-rule" && t.is_none()),
        "{after:?}"
    );
}

/// Review round 1, B1a. The frame sends the reader to a record that still
/// exists when they read it.
#[test]
fn the_frame_points_at_the_audit_log_not_at_an_emptied_tool() {
    let f = fixture();
    f.notify(RUN_GREEN);
    f.notify(DONE);
    let frame = f
        .prompts()
        .into_iter()
        .find(|t| t.starts_with("[orrerix] 1 notice deferred"))
        .expect("the frame is delivered");
    assert!(frame.contains("audit log as delivery-triaged"), "got: {frame}");
    assert!(
        !frame.contains("list_deferred"),
        "the store is empty by the time this is read: {frame}"
    );
}

/// Review round 1, B2. The deadline flush never targets a dead pane.
///
/// Without the status filter this cleared the store under the lock,
/// `deliver_prompt` refused `AgentDead`, the error was discarded, and every
/// held notice was gone — a relaunched orchestrator inheriting an empty store.
#[test]
fn the_deadline_flush_never_empties_the_store_at_a_dead_pane() {
    let f = fixture();
    f.notify(RUN_GREEN);
    let held_at = f.reg.deferred_list(&f.g)["items"][0]["ts_ms"].as_u64().expect("ts");
    let deadline = held_at + u64::from(triage::TRIAGE_MAX_DEFER_MINUTES_DEFAULT) * 60_000;

    // The orchestrator's pane exits.
    f.reg.on_pty_exit(8801, Some(0), "", 0, true);

    assert!(
        f.reg.triage_flush_tick(deadline).is_empty(),
        "a dead pane is not a flush target"
    );
    assert_eq!(
        f.reg.deferred_list(&f.g)["count"],
        Value::from(1),
        "the held notice waits for a live pane rather than being spent at a dead one"
    );
}

/// Review round 1, premortem 1. Turning triage OFF hands back what it is
/// holding, rather than stranding it.
///
/// All three bounds are gated on the same policy that created the entries, so
/// a store held by a policy since turned off had nothing that would ever
/// release it: `list_deferred` answered `enabled: false, count: N` forever.
#[test]
fn turning_triage_off_releases_what_it_was_holding() {
    let f = fixture();
    f.notify(RUN_GREEN);
    assert_eq!(f.reg.deferred_list(&f.g)["count"], Value::from(1));

    // The repo author edits the file: same group, same store, triage off.
    fs::write(
        f._repo.repo.join(".loomux").join("workflow.yml"),
        workflow_yaml("triage:\n  enabled: false\n  provider: none"),
    )
    .unwrap();
    assert_eq!(f.reg.deferred_list(&f.g)["enabled"], Value::from(false));

    // The very next tick releases it, whatever the clock says — the entry is
    // nowhere near its deadline.
    let flushed = f.reg.triage_flush_tick(0);
    assert_eq!(flushed, vec![f.g.clone()], "the stragglers are handed back");
    assert_eq!(f.reg.deferred_list(&f.g)["count"], Value::from(0));
    // …and they reach the pane saying WHY, which is the half review round 2
    // caught: the release reused the deadline's cause, so the frame told the
    // reader it had hit `max_defer_minutes` when it was nowhere near it (the
    // entry is seconds old and the default is 30 minutes).
    let frame = f
        .prompts()
        .into_iter()
        .find(|t| t.starts_with("[orrerix] 1 notice deferred"))
        .expect("the frame reaches the pane");
    assert!(frame.contains("flushed because triage was turned off"), "got: {frame}");
    assert!(!frame.contains("deadline"), "the cause was the policy, not the clock: {frame}");
}

// ── what is NEVER held ──────────────────────────────────────────────────────

#[test]
fn the_never_triaged_set_and_a_red_run_still_wake_the_pane() {
    let f = fixture();
    for text in [HELD, RUN_RED, DONE] {
        f.notify(text);
    }
    let prompts = f.prompts();
    for needle in ["HELD", "17813", "reports done"] {
        assert!(
            prompts.iter().any(|t| t.contains(needle)),
            "{needle} must reach the pane: {prompts:?}"
        );
    }
    assert_eq!(f.reg.deferred_list(&f.g)["count"], Value::from(0), "nothing was held");
    // Each one is still ACCOUNTED for, with the reason it was let through —
    // the audit row is what makes "delivered" and "never looked at" different.
    let actions: Vec<String> = f.triaged().into_iter().map(|(_, a)| a).collect();
    assert_eq!(actions, vec!["held", "no-rule", "no-rule"], "got {actions:?}");
}

#[test]
fn a_delegate_pane_is_not_triaged_at_all() {
    let f = fixture();
    let _ = f.reg.deliver_prompt(&f.worker, RUN_GREEN, "orch", Delivery::MidSession);
    assert!(
        f.prompts().iter().any(|t| t.contains("17812")),
        "triage is the ORCHESTRATOR pane's gate: {:?}",
        f.prompts()
    );
    assert!(f.triaged().is_empty(), "and not even a policy read happens for a delegate");
}

#[test]
fn with_no_triage_block_not_one_byte_of_delivery_behaviour_moves() {
    // The negative control for every test above, and the product default.
    let f = fixture_with("");
    for text in [RUN_GREEN, PLANNER_EXIT, HELD, DONE] {
        f.notify(text);
    }
    assert_eq!(f.prompts().len(), 4, "every notice is offered: {:?}", f.prompts());
    assert!(f.triaged().is_empty(), "and no triage row is written at all");
    assert!(!f.deferred_path().exists(), "no file is created for a repo that did not opt in");
    assert_eq!(f.reg.deferred_list(&f.g)["enabled"], Value::from(false));
}

#[test]
fn a_kind_the_repo_left_off_the_list_is_delivered() {
    let f = fixture_with("triage:\n  enabled: true\n  provider: none\n  kinds: [run-completed]");

    // The NAMED kind is held.
    f.notify(RUN_GREEN);
    assert_eq!(f.reg.deferred_list(&f.g)["count"], Value::from(1), "only the named kind");
    assert!(!f.prompts().iter().any(|t| t.contains("17812")), "{:?}", f.prompts());

    // The UNNAMED one is a genuine wake — so it both reaches the pane AND
    // carries the held notice out in front of it. Asserting the count is
    // still 1 here is what the first version of this test got wrong: it read
    // the flush, which is the feature working, as the narrowing failing.
    f.notify(PLANNER_EXIT);
    let prompts = f.prompts();
    let frame = prompts
        .iter()
        .position(|t| t.starts_with("[orrerix] 1 notice deferred"))
        .unwrap_or_else(|| panic!("the flush rides in front: {prompts:?}"));
    let wake = prompts
        .iter()
        .position(|t| t.contains("posted its plan"))
        .expect("the unnamed kind wakes the pane");
    assert!(frame < wake, "{prompts:?}");
    assert_eq!(f.reg.deferred_list(&f.g)["count"], Value::from(0), "the flush cleared it");
}

// ── the config refusals ─────────────────────────────────────────────────────

/// `parse_workflow` refuses rather than degrading, and the message names the
/// slice that would make the value legal.
#[test]
fn provider_none_is_the_only_value_this_build_accepts() {
    let errs = parse_workflow(&workflow_yaml(
        "triage:\n  enabled: true\n  provider: typesafe",
    ))
    .expect_err("a provider this build has no adapter for must refuse the file");
    let joined = errs.join("\n");
    assert!(joined.contains("triage.provider"), "got: {joined}");
    assert!(joined.contains("S3"), "the error names where the value becomes legal: {joined}");

    // The control: the value this build DOES have loads, and resolves to on.
    let ok = parse_workflow(&workflow_yaml(TRIAGE_ON))
        .expect("provider: none must load");
    assert!(ok.triage.enabled);
    assert_eq!(ok.triage.provider, triage::PROVIDER_NONE);
    assert_eq!(ok.triage.max_defer_minutes, triage::TRIAGE_MAX_DEFER_MINUTES_DEFAULT);
}

#[test]
fn an_out_of_range_deadline_and_an_unknown_kind_both_refuse_the_file() {
    for (block, needle) in [
        ("triage:\n  enabled: true\n  max_defer_minutes: 0", "triage.max_defer_minutes"),
        ("triage:\n  enabled: true\n  max_defer_minutes: 241", "triage.max_defer_minutes"),
        ("triage:\n  enabled: true\n  kinds: [run-complete]", "triage.kinds"),
        ("triage:\n  enabled: true\n  nope: 1", "unknown field"),
    ] {
        let errs = parse_workflow(&workflow_yaml(block))
            .expect_err(&format!("must refuse: {block}"));
        assert!(errs.join("\n").contains(needle), "{block} -> {errs:?}");
    }
    // The control: both bounds are INCLUSIVE, so the refusals above are about
    // the range rather than about the key existing.
    for m in [1u32, 240] {
        let ok = parse_workflow(&workflow_yaml(&format!(
            "triage:\n  enabled: true\n  max_defer_minutes: {m}"
        )))
        .unwrap_or_else(|e| panic!("{m} must load: {e:?}"));
        assert_eq!(ok.triage.max_defer_minutes, m);
    }
}

// ── the privacy claim, as a property of the build ───────────────────────────

/// What `provider: none` actually guarantees, stated as something CHECKABLE.
///
/// **The first draft of this test claimed the workspace has no HTTP client at
/// all, and that claim is false.** `tauri` brings `reqwest`, `hyper` and
/// `hyper-util` into `Cargo.lock` transitively — it is the webview host and
/// has done so since long before this feature. The claim survived being
/// written into three permanent surfaces and was caught only by RUNNING the
/// guard, which is CLAUDE.md's "a guard that REFUSES ships only after it has
/// run clean over known-good subjects" landing on its author.
///
/// So the guarantee is narrowed to what is true, and each half is pinned by
/// the assertion that can actually see it:
///
/// 1. **The crate triage lives in declares no HTTP client.**
///    `loomux-engine`'s dependency list is closed, short and individually
///    audited in its own manifest; a client added there would be a new direct
///    edge in a crate whose defining property is what it must never depend on.
/// 2. **Neither triage source names a network primitive**, default-deny over a
///    token list rather than over a binding's name.
///
/// What is NOT claimed: that the shipped binary links no HTTP stack. It does,
/// through `tauri`. What S1 guarantees is that no delivery can reach it —
/// there is no call, no client and no address anywhere on the path from
/// `deliver_prompt_as` to a decision. S3 adding a provider has to add a direct
/// edge or a socket, and either reddens this.
/// Every dependency NAME a Cargo manifest declares — the keys of every
/// `[dependencies]`-family table, and nothing from `[package]` or from the
/// comments.
///
/// Deliberately small and deliberately not a TOML parser: the one property it
/// needs is that a name matches as a NAME rather than as a substring of the
/// surrounding prose, which is the false block this exists to have fixed.
fn declared_deps(manifest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_deps = false;
    for line in manifest.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            // `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`
            // and every `[target.'cfg(...)'.dependencies]` variant.
            in_deps = t.contains("dependencies]");
            continue;
        }
        if !in_deps || t.starts_with('#') || t.is_empty() {
            continue;
        }
        if let Some((key, _)) = t.split_once('=') {
            let key = key.trim().trim_matches('"');
            if !key.is_empty() && !key.contains(char::is_whitespace) {
                out.push(key.to_string());
            }
        }
    }
    out
}

#[test]
fn neither_the_engine_nor_triage_can_reach_a_network() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();

    // (1) the engine crate's own declared dependencies, compared by NAME.
    //
    // **Not `manifest.contains(name)`**, which is how the first version of
    // this assertion falsely blocked a known-good manifest: `surf` is a
    // substring of `surface`, and that word appears twice in the engine's own
    // dependency-audit prose. A substring scan over a file that is mostly
    // COMMENTARY reports the commentary. Parse the declaration instead — a
    // dependency is a `key = …` line inside a `[dependencies]` table — and
    // compare the keys by equality.
    let manifest = fs::read_to_string(root.join("crates/loomux-engine/Cargo.toml"))
        .expect("the engine manifest");
    let declared = declared_deps(&manifest);
    // The positive control FIRST: a scan whose success shape is "no match" is
    // byte-identical to a scan that read the wrong file, or that parsed it into
    // nothing.
    assert!(
        declared.iter().any(|d| d == "serde_norway"),
        "the engine manifest parsed to {declared:?} — the assertions below would be vacuous"
    );
    for banned in
        ["reqwest", "hyper", "ureq", "isahc", "curl", "attohttpc", "surf", "typesafe-sdk"]
    {
        assert!(
            !declared.iter().any(|d| d == banned),
            "{banned} is a declared dependency of loomux-engine — the crate triage lives in. \
             #3304 S1 ships no provider, and a classifier that sends agent-authored text off \
             the machine is S3's decision to argue with the human's say-so, not a dependency \
             that arrives quietly. Declared: {declared:?}"
        );
    }

    // (2) the two triage sources name no network primitive. Default-deny over
    // TOKENS, never over a binding's name (CLAUDE.md's source-scanning-guard
    // convention): a rename cannot step over `https://` or `TcpStream`.
    for rel in ["crates/loomux-engine/src/triage.rs", "src-tauri/src/orchestration/triagegate.rs"]
    {
        let src = fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert!(src.contains("fn "), "{rel} was not read as expected");
        for token in
            ["https://", "http://", "TcpStream", "reqwest", "hyper::", "Client::new", "::connect("]
        {
            assert!(
                !src.contains(token),
                "{rel} names {token} — nothing on the delivery path may reach a network in S1"
            );
        }
    }
}

// ── the MCP surface ─────────────────────────────────────────────────────────

fn caller(f: &Fixture, id: &str, role: Role) -> Caller {
    Caller { agent_id: id.to_string(), group: f.g.clone(), role, role_hint: None }
}

fn call_tool(f: &Fixture, c: &Caller, name: &str) -> Value {
    dispatch(&f.reg, c, "tools/call", &json!({ "name": name, "arguments": {} }))
        .unwrap_or_else(|e| panic!("{name}: {e:?}"))
}

#[test]
fn list_deferred_is_the_orchestrators_read_and_nobody_elses() {
    let f = fixture();
    f.notify(RUN_GREEN);

    let orch = caller(&f, &f.orch, Role::Orchestrator);
    let out = call_tool(&f, &orch, "list_deferred");
    assert_eq!(out["isError"], json!(false), "{:?}", out["content"][0]["text"]);
    let v: Value =
        serde_json::from_str(out["content"][0]["text"].as_str().expect("text")).expect("json");
    assert_eq!(v["count"], Value::from(1));
    assert_eq!(v["items"][0]["kind"], Value::from("run-completed"));

    let worker = caller(&f, &f.worker, Role::Worker);
    let denied = call_tool(&f, &worker, "list_deferred");
    assert_eq!(
        denied["isError"],
        json!(true),
        "a delegate has no business reading another pane's held notices"
    );
    let msg = denied["content"][0]["text"].as_str().unwrap_or_default().to_lowercase();
    assert!(msg.contains("orchestrator"), "got: {msg}");
}

/// The `list_deferred` TOOL DESCRIPTION is a rule-table claim, and it goes
/// stale like any other (#3324 review round 3).
///
/// It is the surface an orchestrator reads at RUNTIME to decide whether a
/// notice it has not seen was held or never arrived, and three rounds of this
/// PR corrected the rule table on three other surfaces — the design note, the
/// user docs, the playbook template — while leaving this one naming a retired
/// rule. Nothing was red, because nothing reads prose.
///
/// So: a DEFAULT-DENY scan of the description for the vocabulary of rules that
/// no longer hold, plus the markers that must be listed because an agent
/// reaching for one needs to know it works. Decided on the retired ENTITY, not
/// on a phrasing, so a reworded description that still promises the old
/// behaviour is still caught.
#[test]
fn the_list_deferred_description_does_not_promise_a_retired_rule() {
    let f = fixture();
    let orch = caller(&f, &f.orch, Role::Orchestrator);
    let listed = dispatch(&f.reg, &orch, "tools/list", &json!({})).expect("tools/list");
    let tools = listed["tools"].as_array().expect("tools[]");

    // POSITIVE CONTROL. Every assertion below is about the CONTENT of one
    // description; a lookup that found no tool, or a tool with no description,
    // would pass all of them vacuously.
    assert!(tools.len() >= 5, "tools/list returned {} tools", tools.len());
    let desc = tools
        .iter()
        .find(|t| t["name"] == "list_deferred")
        .and_then(|t| t["description"].as_str())
        .expect("list_deferred is not on tools/list, or carries no description");
    assert!(desc.len() > 400, "description is {} bytes — did it lose its body?", desc.len());

    // 1. The RETIRED rule. `GATE SATISFIED` may still be NAMED — the fixed
    //    description names it to say it is never held, which is the useful
    //    thing to say — so the scan is on the CLAIM, not the string: the
    //    description must not put it on the held side.
    let held_side = desc.split("Everything else is DELIVERED").next().unwrap_or(desc);
    assert!(
        !held_side.contains("GATE SATISFIED"),
        "the held-classes sentence still names GATE SATISFIED, whose rule is retired (#3324): {held_side}"
    );

    // 2. Every marker in `NEEDS_YOU_MARKERS` that an agent could reach for
    //    deliberately must be listed. Derived from the engine array rather
    //    than from a literal list here, so adding a marker without mentioning
    //    it reddens — which is how `is yours` went unlisted in the first place.
    for marker in triage::needs_you_markers() {
        assert!(
            desc.contains(marker),
            "`{marker}` is a needs-you marker and the tool description does not name it"
        );
    }

    // 3. The two rules that GAINED a proviso must not be stated flat. Both
    //    used to read as unconditional and both now depend on the registered
    //    note / on what the pane printed.
    assert!(
        desc.contains("registered note"),
        "the green rules are conditional on the registered note and the description does not say so"
    );
    assert!(
        desc.contains("printed something"),
        "`agent-exited` is conditional on the pane having printed and the description does not say so"
    );
}
/// There is deliberately no write tool at any tier: nothing on the MCP surface
/// can defer a notice, flush one, or clear the store. Default-deny, so a
/// future slice cannot add one quietly.
#[test]
fn the_mcp_surface_has_no_path_to_deferring_or_flushing_anything() {
    let f = fixture();
    let orch = caller(&f, &f.orch, Role::Orchestrator);
    for name in ["defer_delivery", "flush_deferred", "clear_deferred", "triage_delivery"] {
        let out = call_tool(&f, &orch, name);
        assert_eq!(
            out["isError"],
            json!(true),
            "{name} must not exist — deferring is a rule's answer about a shape, never an ask"
        );
    }
    // The positive control: this harness CAN reach a tool, so the four
    // refusals above are about those names rather than about the call shape.
    assert_eq!(call_tool(&f, &orch, "list_deferred")["isError"], json!(false));
}
