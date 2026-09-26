//! The merge queue's registry seam, guardrails, spawn expiry, max_agents, exit surfacing, identity durability and delivery-queue bookkeeping.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

#[test]
fn kickoff_readiness_waits_for_painted_and_quiet_cli() {
    let s = Duration::from_secs;
    let ms = Duration::from_millis;
    // A slow-booting CLI (no output yet) is not ready no matter the elapsed
    // time inside the window — this is the race that ate a reviewer kickoff.
    assert!(!cli_ready(0, s(5), s(5)));
    // Output present but still actively painting (not quiet) → not ready.
    assert!(!cli_ready(4096, ms(100), s(5)));
    // Too early to judge, even if output looks settled.
    assert!(!cli_ready(4096, s(1), ms(800)));
    // Painted + quiet + past the minimum wait → ready.
    assert!(cli_ready(4096, s(2), s(3)));
}


/// **The three merge-queue tools are orchestrator-only, at the DISPATCH gate**
/// (#581 §11.1).
///
/// The role-filtered listing is cosmetic — a tool omitted from a listing is
/// still callable — so what matters is that a worker's *call* is refused, not
/// merely that a worker's *listing* omits it. `queue_merge` can make the backend
/// push a ref and open a PR, so this is the check that has to hold. Both halves
/// are asserted, following the `review_verdict` / `queue_orphans` precedent.
#[test]
fn the_merge_queue_tools_are_orchestrator_only_and_the_dispatch_check_is_the_gate() {
    let (reg, _d, co, cw) = setup_mcp();
    const TOOLS: [&str; 3] = ["queue_merge", "merge_queue_status", "cancel_queued_merge"];

    // The real gate: a worker calling any of them is refused.
    for name in TOOLS {
        let args = if name == "merge_queue_status" { json!({}) } else { json!({ "pr": "612" }) };
        let denied =
            dispatch(&reg, &cw, "tools/call", &json!({ "name": name, "arguments": args })).unwrap();
        assert_eq!(denied["isError"], true, "{name} must refuse a worker at dispatch");
    }

    // …and the cosmetic half: the orchestrator sees them, a worker does not.
    let listed = dispatch(&reg, &co, "tools/list", &json!({})).unwrap();
    let names: Vec<&str> =
        listed["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    let worker_listed = dispatch(&reg, &cw, "tools/list", &json!({})).unwrap();
    let worker_names: Vec<&str> =
        worker_listed["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    for name in TOOLS {
        assert!(names.contains(&name), "the orchestrator must SEE {name}: {names:?}");
        assert!(!worker_names.contains(&name), "a worker must not be offered {name}");
    }
}

/// `rails()` with the advanced orchestrator ON — the toggle that puts a repo's
/// `.loomux/workflow.yml` in force at all. Plain `rails()` leaves it off, which
/// is the default and is exactly the third case the queue test exercises.
pub(crate) fn advanced_rails() -> Guardrails {
    Guardrails { advanced_orchestrator: true, ..rails() }
}

/// Write a repo dir whose `.loomux/workflow.yml` declares a merge queue.
pub(crate) fn repo_with_merge_queue(tag: &str, enabled: bool) -> std::path::PathBuf {
    let repo = scratch_dir(tag);
    fs::create_dir_all(repo.join(".loomux")).unwrap();
    // `version:` is REQUIRED (no serde default) and must equal
    // `workflow::SCHEMA_VERSION` — a file without it fails the parse, which is
    // how the first cut of this test ended up asserting presence against a
    // workflow that never loaded.
    fs::write(
        repo.join(".loomux").join("workflow.yml"),
        // A workflow needs at least one block to be valid, so the file carries a
        // minimal roster it does not otherwise use. The queue's own gating is on
        // `merge_queue.enabled`, not on the roster's shape.
        format!(
            "version: {}\n\
             blocks:\n  - id: w\n    name: Worker\n    kind: worker\n    cli: claude\n    model: sonnet\n\
             merge_queue:\n  enabled: {enabled}\n  max_batch: 3\n",
            workflow::SCHEMA_VERSION
        ),
    )
    .unwrap();
    // The fixture asserts its OWN validity. Without this, a malformed fixture
    // fails the caller's `contains(...)` assertion instead — which reads as "the
    // feature is broken" when the truth is "this file never loaded", and that is
    // exactly how the first cut of this test misdiagnosed a missing `version:`.
    let loaded = workflow::load_workflow(repo.to_str().unwrap());
    assert!(
        matches!(&loaded, Ok(Some(wf)) if wf.merge_queue.enabled == enabled),
        "fixture must parse with merge_queue.enabled={enabled}, got {loaded:?}"
    );
    repo
}

/// **The `{{MERGE_QUEUE}}` fragment reaches only a group that actually runs the
/// queue** — and "runs the queue" means BOTH the block and the
/// advanced-orchestrator toggle.
///
/// The absence half is already pinned hard by `tests/workflow.rs`'s blessed
/// golden and its never-names-the-gate-machinery rule — which is how this got
/// caught: the first cut of slice E put this guidance in the base template and
/// those tests went red. What nothing pinned was the **presence** half: a
/// substitution that was always empty would satisfy every one of them, which is
/// the door-connected-to-nothing shape again.
///
/// The toggle case is the one that found a defect. With the toggle off,
/// `create_group_ex` deliberately clears the merge gate so the default merge
/// path stays pre-#222 — so a queue keyed on the block alone would be live in a
/// group that opted out of the whole workflow, with its own gate cleared
/// underneath it.
#[test]
fn the_merge_queue_note_reaches_only_a_group_that_actually_runs_the_queue() {
    // #1683 moved the merge-gate section — and the `{{MERGE_QUEUE}}` fragment
    // with it — into the rendered playbook, so this reads
    // orchestrator-playbook.md where it used to read orchestrator.md.
    let note_marker = "queue_merge(pr, target?)";
    let playbook = |reg: &OrchRegistry, gid: &str| {
        fs::read_to_string(reg.state_root().join(gid).join("orchestrator-playbook.md")).unwrap()
    };

    // Block on + toggle on: the guidance is there.
    let (reg, _d) = test_registry();
    let repo = repo_with_merge_queue("mq-on", true);
    let g = reg.create_group(repo.to_str().unwrap(), advanced_rails()).unwrap();
    let orch = playbook(&reg, g.id.as_str());
    assert!(
        orch.contains(note_marker),
        "a group whose repo enables the queue must be told the queue exists"
    );
    assert!(orch.contains("base-not-target"), "…including the refusal vocabulary");
    assert!(
        orch.contains("Routing is yours"),
        "…and that loomux does not brief the culprit's worker (§9)"
    );

    // Block explicitly off: nothing.
    let (reg, _d) = test_registry();
    let repo = repo_with_merge_queue("mq-off", false);
    let g = reg.create_group(repo.to_str().unwrap(), advanced_rails()).unwrap();
    let orch = playbook(&reg, g.id.as_str());
    assert!(!orch.contains(note_marker), "enabled:false must read exactly like no block at all");
    assert!(!orch.contains("{{MERGE_QUEUE}}"), "the placeholder is substituted, not left raw");

    // Block ON but the advanced orchestrator OFF: still nothing. The workflow
    // file is not in force, so neither is the queue.
    let (reg, _d) = test_registry();
    let repo = repo_with_merge_queue("mq-toggle-off", true);
    let g = reg.create_group(repo.to_str().unwrap(), rails()).unwrap(); // toggle OFF
    let orch = playbook(&reg, g.id.as_str());
    assert!(
        !orch.contains(note_marker),
        "with the advanced orchestrator off the workflow file is not in force, so the queue \
         is not running and its guidance must not appear"
    );
}

/// **With no `merge_queue:` block, every tool refuses `queue-disabled`** — the
/// product default (§12), and the state every repo is in until it opts in.
///
/// Driven through the real `dispatch()`, so this covers the JSON shim, the
/// registry method and the driver's refusal together.
#[test]
fn the_queue_tools_refuse_queue_disabled_until_the_repo_opts_in() {
    let (reg, d, co, _cw) = setup_mcp();

    let out = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "queue_merge", "arguments": { "pr": "612" } })).unwrap();
    assert_eq!(out["isError"], false, "a refusal is a RESULT, not a protocol error");
    let v: Value = serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(v["refused"], json!("queue-disabled"));
    assert_eq!(v["queued"], Value::Null, "a refusal never also reports success");

    // Status is readable with the feature off — it reports `enabled: false`
    // rather than erroring, so an orchestrator can tell "off" from "broken".
    let out = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "merge_queue_status", "arguments": {} })).unwrap();
    let v: Value = serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(v["enabled"], json!(false));
    assert_eq!(v["entries"], json!([]));

    // And the read did NOT create the state file — a status call that conjured
    // one would collapse slice F's `absent` state for every group.
    assert!(!d.path().join(co.group.as_str()).join("merge_queue.json").exists());
}

/// **#710 (rev N2): a REFUSED enqueue still releases a stale target — on disk.**
///
/// The queue-core half is pinned in `tests/mergequeue.rs`; what only this file
/// can reach is the registry path around it. `queue_merge_with`'s refusal arm
/// did not write at all, so a released target would have lived in a state object
/// that was dropped on the way out and `merge_queue_status` — which reads the
/// file — would have gone on naming a branch the queue is not landing on until
/// the next restart's reconcile.
///
/// The second half is the trap that write could have sprung: writing
/// unconditionally would create a `merge_queue.json` for any group whose first
/// `queue_merge` is refused, collapsing slice F's `absent` state (§12) for every
/// repo that ever mistyped a PR number.
#[test]
fn a_refused_enqueue_releases_a_stale_target_without_conjuring_a_queue_file() {
    /// A PR whose base IS the default branch: constraint 7 refuses it before any
    /// target can be established, so the only state change available to this
    /// enqueue is the release.
    struct BaseIsDefault;
    impl loomux_lib::orchestration::mqdriver::MqRunner for BaseIsDefault {
        fn git(
            &self,
            args: &[&str],
        ) -> Result<loomux_lib::orchestration::mqdriver::CmdOut, String> {
            panic!("constraint 7 refuses before any git call: {args:?}")
        }
        fn gh(
            &self,
            args: &[&str],
        ) -> Result<loomux_lib::orchestration::mqdriver::CmdOut, String> {
            let joined = args.join(" ");
            let stdout = if joined.contains("repo view") {
                "main\n".to_string()
            } else {
                json!({ "baseRefName": "main", "headRefOid": "a".repeat(40), "body": "b" })
                    .to_string()
            };
            Ok(loomux_lib::orchestration::mqdriver::CmdOut {
                code: Some(0),
                stdout,
                stderr: String::new(),
            })
        }
    }

    let (reg, d) = test_registry();
    let repo = repo_with_merge_queue("mq-stale-target", true);
    let g = reg.create_group(repo.to_str().unwrap(), advanced_rails()).unwrap();
    let qfile = d.path().join(g.id.as_str()).join("merge_queue.json");

    // The field state of #710: an empty queue still pointing at the branch a
    // cancelled batch left behind.
    fs::write(&qfile, r#"{"version":1,"target":"integration/batch3","entries":[]}"#).unwrap();

    let v = reg.queue_merge_with(&g.id, 705, None, &BaseIsDefault);
    assert_eq!(v["refused"], json!("base-is-default"), "the refusal itself is unchanged");

    let after: Value = serde_json::from_str(&fs::read_to_string(&qfile).unwrap()).unwrap();
    assert_eq!(after["target"], json!(""), "the release reached disk: {after}");
    assert_eq!(
        reg.merge_queue_status(&g.id)["target"],
        json!(""),
        "so status stops naming it at the first touch, not at the next restart"
    );

    // A group that never queued anything has no file, and a refused enqueue must
    // not conjure one — `merge_queue_view` distinguishes "never enqueued" from
    // "empty queue", and an always-present file collapses the two.
    let repo2 = repo_with_merge_queue("mq-stale-target-absent", true);
    let g2 = reg.create_group(repo2.to_str().unwrap(), advanced_rails()).unwrap();
    let qfile2 = d.path().join(g2.id.as_str()).join("merge_queue.json");
    assert!(!qfile2.exists(), "fixture precondition");

    let v = reg.queue_merge_with(&g2.id, 705, None, &BaseIsDefault);
    assert_eq!(v["refused"], json!("base-is-default"));
    assert!(
        !qfile2.exists(),
        "a refused enqueue must not create merge_queue.json — the default state is already \
         drained, so the release is a no-op and nothing changed to write"
    );
}

/// A PR that is not in the queue cannot be cancelled, and is told so — rather
/// than being given a success that means nothing (§11.1's `not-queued`).
#[test]
fn cancelling_a_pr_that_is_not_queued_is_refused_not_silently_successful() {
    let (reg, _d, co, _cw) = setup_mcp();
    let out = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "cancel_queued_merge", "arguments": { "pr": "#999" } })).unwrap();
    let v: Value = serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(v["refused"], json!("not-queued"));
    assert_eq!(v["cancelled"], Value::Null);
}

/// **A loomux fault must read as a fault, never as a policy refusal**
/// (rev-163 NB).
///
/// `queue-disabled` and `not-queued` are *decisions*: they say the queue looked
/// at the request and declined, and each tells the caller something true they
/// can act on. An unreadable state file is neither — and labelling it
/// `queue-disabled` would tell an orchestrator its repo never opted in, so it
/// would **stop**, which is the one wrong move when the truth is a torn write.
/// A wrong label does not merely under-inform; it sends the reader elsewhere.
///
/// Shape borrowed from `queue_orphans`' `no-app-handle` / `registry-not-shared`,
/// which are documented as "should never appear in a running build, so treat one
/// as a loomux defect worth reporting".
#[test]
fn a_loomux_fault_is_labelled_as_one_and_never_as_a_policy_refusal() {
    let (reg, d, co, _cw) = setup_mcp();
    let qfile = d.path().join(co.group.as_str()).join("merge_queue.json");

    // (1) queue_merge with an unreadable state file.
    fs::write(&qfile, "{ this is not json").unwrap();
    let out = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "queue_merge", "arguments": { "pr": "612" } })).unwrap();
    let v: Value = serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        v["refused"], json!("queue-state-unreadable"),
        "an unreadable queue must not read as 'the repo never opted in'"
    );

    // (2) cancel_queued_merge with the same file. `not-queued` would assert the
    //     PR is absent, which loomux cannot know from a file it could not read.
    let out = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "cancel_queued_merge", "arguments": { "pr": "612" } })).unwrap();
    let v: Value = serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(v["refused"], json!("queue-state-unreadable"));

    // (3) an unresolvable group — a fault, not a queue state. Called on the
    //     registry directly: `dispatch` always uses `caller.group`, so there is
    //     no agent-reachable path that names another group (which is itself the
    //     reason there is no cross-group check to test).
    let v = reg.queue_merge(&parse_gid("no-such-group"), 612, None);
    assert_eq!(v["refused"], json!("queue-unavailable"));

    // And all four are flagged as faults by the predicate the tool descriptions
    // and any future caller branch on — so the distinction cannot be lost by
    // someone re-listing the strings and missing one.
    for label in [
        "queue-state-unreadable",
        "queue-state-unwritable",
        "queue-unavailable",
        "gate-unreadable",
    ] {
        assert!(loomux_lib::orchestration::mqloop::refusal::is_loomux_fault(label), "{label}");
    }
    for label in ["queue-disabled", "not-queued", "gate-not-met", "base-is-default"] {
        assert!(
            !loomux_lib::orchestration::mqloop::refusal::is_loomux_fault(label),
            "{label} is a policy decision, not a fault"
        );
    }
}

/// The unwritable half, on the one platform where a directory's permissions
/// portably stop a write.
///
/// `#[cfg(unix)]` deliberately: on Windows the read-only *directory* attribute
/// does not prevent file creation, so there is no portable way to force
/// `atomic_write` to fail — and a test that silently does nothing on a platform
/// is worse than one that states where it runs. The other four relabeled paths
/// are covered on all three platforms above.
/// Driven through **cancel** rather than enqueue, deliberately. Enqueue only
/// reaches the write after resolving a target and satisfying the merge gate, so
/// forcing a write failure there would mean standing up a whole gated repo — and
/// a cheaper setup that refused earlier would produce an assertion that passes
/// for the wrong reason. Cancel needs nothing but a readable queue file, so the
/// write is genuinely the step under test.
///
/// `#[cfg(unix)]` deliberately: on Windows the read-only *directory* attribute
/// does not prevent file creation, so there is no portable way to force
/// `atomic_write` to fail. Stated rather than silently no-op'd on a platform.
/// (Also inert if run as root, which CI is not.)
#[cfg(unix)]
#[test]
fn a_queue_change_that_cannot_be_persisted_is_reported_as_unwritable() {
    use std::os::unix::fs::PermissionsExt;
    let (reg, d, co, _cw) = setup_mcp();
    let gdir = d.path().join(co.group.as_str());
    fs::write(gdir.join("merge_queue.json"), IN_FLIGHT_QUEUE_JSON).unwrap();

    // Readable, so `load_state` succeeds; not writable, so `store_state` fails.
    fs::set_permissions(&gdir, fs::Permissions::from_mode(0o555)).unwrap();
    let v = reg.cancel_queued_merge(&co.group, 612);
    // Restored before asserting, so a failure cannot leave an undeletable dir.
    fs::set_permissions(&gdir, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(
        v["refused"],
        json!("queue-state-unwritable"),
        "a cancel the next restart would forget is not a cancel — and it is certainly \
         not 'that PR was never queued'"
    );
    assert_eq!(v["cancelled"], Value::Null, "a fault never also reports success");
}

/// The gate half of #681: a `merge_gate` file that is ON DISK but an I/O error
/// (here, permission denied) keeps loomux from reading it must refuse
/// `gate-unreadable` — a loomux FAULT — never a policy refusal, which asserts
/// something read a well-formed answer out of the file (present-and-declared,
/// present-and-empty, or absent) rather than failing to read it at all.
///
/// This fixture's own repo has no `merge_queue:` block (`setup_mcp()`'s fake
/// repo carries no `workflow.yml`), so `merge_queue_enabled` is false and the
/// pre-fix label here is `queue-disabled`, not literally `gate-not-configured`
/// — that one only fires once the queue is enabled and enqueue reaches the
/// gate check. The property this test actually pins is the more general one
/// #681 asked for: **a gate-read fault preempts every policy question**,
/// `queue-disabled` included, the same way `STATE_UNREADABLE` already
/// preempts it above — not merely the one policy label the issue happened to
/// name. A malformed (but readable) gate file is NOT this case — it still
/// parses fine as far as `fs::read_to_string` is concerned and correctly
/// stays `gate-not-met` via `GateSpec::Malformed`, covered by the existing
/// gate tests in mergequeue.rs.
///
/// `#[cfg(unix)]` for the same reason as the unwritable test above: Windows
/// has no portable way to make an existing file's own read fail.
#[cfg(unix)]
#[test]
fn an_unreadable_gate_file_is_labelled_as_a_fault_never_a_policy_refusal() {
    use std::os::unix::fs::PermissionsExt;
    let (reg, d, co, _cw) = setup_mcp();
    let gate_file = d.path().join(co.group.as_str()).join("merge_gate");
    fs::write(&gate_file, "require all-pass\nreviewer rev-a\n").unwrap();

    // Readable state (none written — `load_state` defaults cleanly), but the
    // gate file itself cannot be opened for reading.
    fs::set_permissions(&gate_file, fs::Permissions::from_mode(0o000)).unwrap();
    let v = reg.queue_merge(&co.group, 612, None);
    // Restored before asserting, so a failure cannot leave an unreadable file
    // behind for the tempdir's own cleanup.
    fs::set_permissions(&gate_file, fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(
        v["refused"],
        json!("gate-unreadable"),
        "an unreadable gate file must never masquerade as a policy refusal — \
         here, pre-fix, it would read as 'queue-disabled' ('the repo never opted \
         in'), when the truth is a loomux fault"
    );
    assert!(
        loomux_lib::orchestration::mqloop::refusal::is_loomux_fault("gate-unreadable"),
        "gate-unreadable must be flagged as a loomux fault, not a policy refusal"
    );
}

/// A malformed `pr` argument is rejected before anything is resolved — the
/// tools take "PR number, #n, or URL" like every other PR-taking tool here, and
/// an unparseable one must not become PR 0.
#[test]
fn a_malformed_pr_argument_is_rejected_rather_than_coerced() {
    let (reg, _d, co, _cw) = setup_mcp();
    for bad in ["", "not-a-pr", "#", "abc123"] {
        let out = dispatch(&reg, &co, "tools/call",
            &json!({ "name": "queue_merge", "arguments": { "pr": bad } })).unwrap();
        assert_eq!(out["isError"], true, "{bad:?} must be rejected, not coerced");
    }
}

/// A `merge_queue.json` with one entry inside an in-flight batch. Used twice in
/// the strand test — once to set it up, once to **re-arm** it so the once-only
/// guard has something it must actually stop.
const IN_FLIGHT_QUEUE_JSON: &str = r#"{"version":1,"target":"integration",
    "entries":[{"pr":612,"head":"aaaaaaa","state":"ci-wait","enqueued_ms":0,"batch":"mq-1"}],
    "batch":{"id":"mq-1","prs":[612],"scratch_sha":"abc","draft_pr":640,
             "state":"ci-wait","started_ms":0}}"#;

/// **The production reconcile path, end to end through the registry** (§4).
///
/// A batch is recorded as in-flight and the world does not match it (the scratch
/// ref is gone). The registry must strand the entries, audit it, rewrite the
/// snapshot rather than delete it, and hand back exactly one phase-2 notice —
/// and it must do all of that only **once** per group per process.
#[test]
fn merge_queue_reconcile_strands_a_batch_the_world_no_longer_matches() {
    let (reg, d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // The group state dir is the registry root plus the group id.
    let qfile = d.path().join(g.id.as_str()).join("merge_queue.json");
    fs::write(&qfile, IN_FLIGHT_QUEUE_JSON).unwrap();

    // `ls-remote --exit-code` exits 2 = the scratch ref is gone from the remote.
    let fake = MqFake {
        git: (2, String::new()),
        gh: (0, r#"{"state":"OPEN"}"#.into()),
        calls: std::sync::Mutex::new(0),
    };
    let notices = reg.merge_queue_reconcile_with(&g.id, &fake);

    assert_eq!(notices.len(), 1, "one phase-2 notice, collected not sent");
    assert!(notices[0].contains("could NOT be resumed"), "{}", notices[0]);

    // The snapshot is REWRITTEN, never deleted — recovery stays re-runnable.
    let after = fs::read_to_string(&qfile).expect("the queue file still exists");
    assert!(after.contains("kicked-back"), "the entry was stranded on disk: {after}");
    assert!(!after.contains("\"batch\":{"), "the dead batch record was cleared: {after}");

    // The transition reached the audit log, which is the durable record even
    // when the notice cannot be delivered.
    let audit = reg.audit_log(&g.id);
    assert!(
        audit.iter().any(|e| e.action == "mq-stranded"),
        "expected an mq-stranded audit event, saw: {:?}",
        audit.iter().map(|e| e.action.clone()).collect::<Vec<_>>()
    );

    // Once-only: a second call must do nothing, so a restart cannot re-strand
    // or re-notify entries a previous pass already resolved.
    //
    // **The file is re-armed first, and that is the whole point.** The first
    // pass leaves the queue resolved — the entry terminal, the batch cleared —
    // so a second pass is naturally a no-op *whether or not the guard exists*.
    // The original version of this assertion did not re-arm, and mutation I2
    // (guard removed) left it GREEN: it asserted a property it did not test,
    // which is exactly the decoration this practice exists to catch. Re-arming
    // gives the second call real work, so only the guard can stop it.
    fs::write(&qfile, IN_FLIGHT_QUEUE_JSON).unwrap();
    let again = reg.merge_queue_reconcile_with(&g.id, &fake);
    assert!(again.is_empty(), "the once-only guard stops a second reconcile");
    let untouched = fs::read_to_string(&qfile).expect("still there");
    assert!(
        untouched.contains("ci-wait"),
        "the re-armed queue was not touched by a second pass: {untouched}"
    );
}

/// The default path every group is on today: **no `merge_queue.json` at all**.
///
/// It must be a clean no-op — no notice, no strand, and **nothing spawned**,
/// which is what makes it safe to call from a bind site on every group before
/// slice E exists. Driven through the real `merge_queue_reconcile` (not the
/// `_with` seam) so the production entry point itself is executed.
#[test]
fn merge_queue_reconcile_is_a_silent_no_op_when_nothing_was_ever_queued() {
    let (reg, d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // The group state dir is the registry root plus the group id.
    let qfile = d.path().join(g.id.as_str()).join("merge_queue.json");
    assert!(!qfile.exists(), "precondition: nothing has ever been queued");

    // The real entry point — the one `readmit_recovered` calls.
    reg.merge_queue_reconcile(&g.id);

    // **The file must still not exist.** This is the property, not a detail:
    // slice F's `merge_queue_view` reports `absent` — "never enqueued", the
    // product default (§12) — by the file's absence, so a reconcile that wrote
    // an empty queue on every bind would silently collapse `absent` into
    // "empty queue" for every group in the product.
    assert!(
        !qfile.exists(),
        "reconciling an empty queue must not conjure a merge_queue.json"
    );

    let audit = reg.audit_log(&g.id);
    assert!(
        !audit.iter().any(|e| e.action.starts_with("mq-")),
        "an empty queue produces no merge-queue audit noise, saw: {:?}",
        audit.iter().map(|e| e.action.clone()).collect::<Vec<_>>()
    );
}

// ---------- registry: guardrails, isolation, persistence, audit ----------

#[test]
fn guardrail_caps_live_agents() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w1", "t", false, None).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w2", "t", false, None).unwrap();
    let err = reg.spawn_agent(&g.id, Role::Worker, "w3", "t", false, None).unwrap_err();
    assert!(err.contains("guardrail"), "expected guardrail rejection, got: {err}");
    // A dead agent frees its slot.
    let id = reg.list_agents(&g.id)[0]["id"].as_str().unwrap().to_string();
    reg.mark_dead(&id, Some(0));
    reg.spawn_agent(&g.id, Role::Worker, "w3", "t", false, None).unwrap();
}

// ---------- #106: timed-out spawns must not resurrect as zombie panes -------

#[test]
fn spawn_request_expiry_decision() {
    // The rule the backend stamps and the frontend enforces: a request is stale
    // once wall-clock passes the deadline of the backend's own bind wait.
    let now = 1_000_000u64;
    // Future deadline (frontend recovered in time) → serviceable.
    assert!(!spawn_request_expired(now + 20_000, now));
    // Past deadline (stalled-then-recovered — the incident) → drop.
    assert!(spawn_request_expired(now - 5_000, now));
    // Boundary is a strict `>`: exactly at the deadline is still live.
    assert!(!spawn_request_expired(now, now));
    assert!(spawn_request_expired(now, now + 1));
    // Deadline 0 = unstamped (legacy payload) → never expires.
    assert!(!spawn_request_expired(0, u64::MAX));
}

#[test]
fn late_bind_on_torn_down_spawn_is_rejected() {
    // The frontend's zombie-pane guard leans on bind_agent ERRORING for a spawn
    // whose bind wait already timed out (the backend removed the pending bind on
    // timeout). Assert bind returns an error for an agent with no pending bind —
    // this is exactly the rejection the recovered frontend now catches and turns
    // into "close the stale pane" instead of an unhandled toast. (The 20s bind
    // wait itself only runs with a live frontend, so it can't be driven here;
    // in this headless registry no pending bind is ever registered.)
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let err = reg.bind(&w.id, 7).unwrap_err();
    assert!(
        err.contains("no pending bind"),
        "a bind with no pending spawn must be rejected so the frontend can discard the pane, got: {err}"
    );
    // A bind for a never-known agent is likewise rejected, never a silent success.
    assert!(reg.bind("w-999", 7).is_err());
}

#[test]
fn list_agents_truncates_task_to_compact_excerpt() {
    // #851: list_agents returned every roster row's full task brief
    // verbatim — a live row kept the whole multi-hundred-word spawn brief,
    // and #106 (the prior fix, superseded here) only trimmed a DEAD row
    // down to omitting `task` altogether. A session with a dozen dead
    // agents, each still carrying a live neighbor's full brief, pushed one
    // group's list_agents payload to ~4k tokens. Every row's `task` is now
    // capped to 140 chars + an ellipsis, alive or dead alike, so a dead row
    // keeps a hint of what it was doing instead of nothing — the full brief
    // stays durable in the audit log/task board; this adds no new retrieval
    // surface for it.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let long = "x".repeat(4096);
    let short = "short brief, well under the cap";
    // `rails()` caps this group at 2 live delegates, so `dead` is killed
    // before the next spawn — otherwise three concurrent live workers would
    // trip the guardrail this test isn't about.
    let dead = reg.spawn_agent(&g.id, Role::Worker, "dead", &long, false, None).unwrap();
    reg.mark_dead(&dead.id, Some(0));
    let live = reg.spawn_agent(&g.id, Role::Worker, "live", &long, false, None).unwrap();
    let live_short =
        reg.spawn_agent(&g.id, Role::Worker, "live-short", short, false, None).unwrap();

    let roster = reg.list_agents(&g.id);
    let arr = roster.as_array().unwrap();
    let dead_row = arr.iter().find(|a| a["id"] == json!(dead.id)).unwrap();
    let live_row = arr.iter().find(|a| a["id"] == json!(live.id)).unwrap();
    let short_row = arr.iter().find(|a| a["id"] == json!(live_short.id)).unwrap();

    // Long brief: both the alive and the dead row get the SAME excerpt —
    // capped at 140 chars plus a trailing ellipsis, never the 4096-char
    // original, and never simply dropped for the dead row.
    let expected_excerpt: String = long.chars().take(140).chain(std::iter::once('…')).collect();
    assert_eq!(
        dead_row["task"],
        json!(expected_excerpt),
        "dead row must still carry a bounded task excerpt, not omit it"
    );
    assert_eq!(
        live_row["task"],
        json!(expected_excerpt),
        "live row's task must be capped the same way as a dead row's"
    );

    // Identity is unaffected — a dead row still resumes.
    assert_eq!(dead_row["status"], json!("dead"));
    assert_eq!(dead_row["name"], json!("dead"));
    assert_eq!(dead_row["role"], json!("worker"));
    assert!(dead_row.get("session").is_some(), "session kept for resume");
    assert!(dead_row.get("cwd").is_some());

    // Short brief: well under the cap, comes back byte-for-byte, no ellipsis.
    assert_eq!(short_row["task"], json!(short));

    // The 4096-char brief itself never appears anywhere in the payload —
    // only the capped excerpt does, for either row.
    assert_eq!(
        roster.to_string().matches(&long).count(),
        0,
        "the full task brief must not appear in the roster — only the capped excerpt"
    );
}

#[test]
fn task_excerpt_is_utf8_safe_at_the_char_boundary() {
    // #851 review NB1: `task_excerpt` MUST cut on a char boundary, not a
    // byte offset — task briefs are full of multi-byte glyphs (em dashes,
    // arrows, emoji), and a byte-offset cut like `&s[..140]` panics the
    // instant it lands inside one instead of between two. This pins that
    // by building a brief whose 140th CHAR is a 4-byte emoji: a byte-offset
    // cut at byte 140 would land one byte into that emoji (139 ASCII bytes
    // in), while a char-counted cut does not — so a regression to
    // byte-slicing fails this test with a slice-boundary panic, not a
    // quiet wrong answer.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let mut task = "a".repeat(139);
    task.push('😀'); // the 140th char, 4 bytes wide — straddles byte offset 140
    task.push_str(" and more text past the cap that must be dropped");
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", &task, false, None).unwrap();

    let roster = reg.list_agents(&g.id);
    let row = roster.as_array().unwrap().iter().find(|a| a["id"] == json!(w.id)).unwrap();
    let excerpt = row["task"].as_str().unwrap();

    // 140 kept chars (139 'a' + the intact emoji) plus the ellipsis marker —
    // counted in chars, not bytes, so the emoji is never split.
    assert_eq!(
        excerpt.chars().count(),
        141,
        "140-char cap + ellipsis, counted in chars not bytes; got {excerpt:?}"
    );
    assert!(excerpt.starts_with(&"a".repeat(139)));
    assert!(excerpt.contains('😀'), "the multi-byte char at the boundary must survive intact, got {excerpt:?}");
    assert!(excerpt.ends_with('…'));
}

#[test]
fn guardrail_clamps_and_sanitizes() {
    let g = Guardrails {
        max_agents: 99,
        agent_cli: "definitely-not-a-cli".into(),
        blocks: workflow::default_roster(&[
            (Role::Orchestrator, "", "opus"),
            (Role::Worker, "", "sonnet; rm -rf /"),
            (Role::Reviewer, "", ""),
            (Role::Planner, "", ""),
        ]),
        auto_ops: true,
        idle_kill_minutes: 99999,
        max_spawns_per_hour: 9999,
        watchdog_stall_minutes: 99999,
        ..Guardrails::default()
    }
    .clamped();
    assert_eq!(g.max_agents, 12, "cap must clamp to the hard ceiling");
    assert_eq!(g.idle_kill_minutes, 1440, "idle-kill timeout clamps to 24h");
    assert_eq!(g.max_spawns_per_hour, 240, "spawn-rate cap clamps to the ceiling");
    assert_eq!(g.watchdog_stall_minutes, 1440, "watchdog stall timeout clamps to 24h");
    assert_eq!(g.agent_cli, "claude", "unknown group CLIs fall back to claude explicitly");
    // #722 widened `sanitize_model` to admit `/` (opencode ids are
    // `provider_id/model_id`), so the fixture's trailing slash now survives
    // where it used to be dropped incidentally. The property this line exists
    // for is unchanged and is asserted below rather than left to the literal:
    // `/` is not a shell metacharacter — it is glob-inert in a POSIX shell and
    // not an operator in PowerShell — so what a crafted model could actually
    // smuggle (the `;` and the spaces) is still gone.
    let worker = g.model_for(Role::Worker);
    assert_eq!(worker, "sonnetrm-rf/", "shell metacharacters must be stripped");
    for bad in [' ', ';', '&', '|', '$', '`', '(', ')', '[', ']', '*', '?', '"', '\''] {
        assert!(!worker.contains(bad), "{bad:?} must never survive into a model id: {worker:?}");
    }
    assert_eq!(g.model_for(Role::Reviewer), "sonnet", "empty model falls back to default");
    // Reasoning classes (orchestrator, planner) default to the strong tier on Claude.
    assert_eq!(g.model_for(Role::Planner), "opus", "empty planner model falls back to the reasoning tier");
    // Copilot's fallback model is "auto" (it picks the best itself).
    let g = Guardrails {
        max_agents: 4,
        agent_cli: "copilot".into(),
        auto_ops: false,
        idle_kill_minutes: 0,
        max_spawns_per_hour: 0,
        watchdog_stall_minutes: 0,
        ..Guardrails::default()
    }
    .clamped();
    assert_eq!(g.model_for(Role::Worker), "auto");
    assert_eq!(g.model_for(Role::Orchestrator), "auto");
    assert_eq!(g.model_for(Role::Planner), "auto");
    assert_eq!(g.blocks.len(), 4, "an empty roster is filled with the built-in 4 blocks");
    // A per-block CLI overrides the group default (issue #4, now a block field);
    // the model fallback follows the block's *effective* CLI.
    let g = Guardrails {
        max_agents: 4,
        agent_cli: "copilot".into(),
        blocks: workflow::default_roster(&[
            (Role::Orchestrator, "", ""),
            (Role::Worker, "claude", ""),
            (Role::Reviewer, "", ""),
            (Role::Planner, "", ""),
        ]),
        ..Guardrails::default()
    }
    .clamped();
    assert_eq!(g.cli_for(Role::Worker), "claude", "a block's CLI overrides the group default");
    assert_eq!(g.cli_for(Role::Reviewer), "copilot", "an empty block CLI inherits the group default");
    assert_eq!(g.model_for(Role::Worker), "sonnet", "worker model fallback follows the worker block's claude CLI");
    assert_eq!(g.model_for(Role::Reviewer), "auto", "reviewer model fallback follows the inherited copilot CLI");
}

// ---------- #56: adjustable max_agents on the fly ----------

#[test]
fn set_max_agents_validates_bounds() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // Below the floor and above the ceiling are refused; the cap is unchanged.
    assert!(reg.set_max_agents(&g.id, 0, "human").is_err());
    assert!(reg.set_max_agents(&g.id, 13, "human").is_err());
    assert_eq!(reg.group(&g.id).unwrap().guardrails.max_agents, 2, "a rejected change must not mutate the cap");
    // The inclusive bounds 1..=12 are accepted.
    assert_eq!(reg.set_max_agents(&g.id, 1, "human").unwrap(), 1);
    assert_eq!(reg.set_max_agents(&g.id, 12, "human").unwrap(), 12);
    // An unknown group is an error, not a panic.
    assert!(reg.set_max_agents(&parse_gid("no-such-group"), 3, "human").is_err());
}

#[test]
fn set_max_agents_enforcement_reads_live_value() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap(); // cap 2
    reg.spawn_agent(&g.id, Role::Worker, "w1", "t", false, None).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w2", "t", false, None).unwrap();
    // At the cap: a third is refused.
    assert!(reg.spawn_agent(&g.id, Role::Worker, "w3", "t", false, None).is_err());
    // Raise the cap live → spawn_agent reads the new value, so the next spawn
    // succeeds (nothing cached the creation-time number).
    assert_eq!(reg.set_max_agents(&g.id, 3, "human").unwrap(), 3);
    reg.spawn_agent(&g.id, Role::Worker, "w3", "t", false, None).unwrap();
    // Lower it below the live count → new spawns blocked again immediately.
    assert_eq!(reg.set_max_agents(&g.id, 1, "human").unwrap(), 1);
    let err = reg.spawn_agent(&g.id, Role::Worker, "w4", "t", false, None).unwrap_err();
    assert!(err.contains("guardrail"), "a lowered cap must block new spawns, got: {err}");
}

#[test]
fn lowering_max_agents_kills_nobody() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap(); // cap 2
    let w1 = reg.spawn_agent(&g.id, Role::Worker, "w1", "t", false, None).unwrap();
    let w2 = reg.spawn_agent(&g.id, Role::Worker, "w2", "t", false, None).unwrap();
    // Drop the cap under the live count: attrition-only, no kills.
    reg.set_max_agents(&g.id, 1, "human").unwrap();
    let sum = reg.group_summary(&g.id);
    assert_eq!(sum["live_agents"].as_u64().unwrap(), 2, "both live workers survive a lowered cap");
    assert_eq!(sum["max_agents"].as_u64().unwrap(), 1, "summary reflects the new cap");
    assert_eq!(sum["live_delegates"].as_u64().unwrap(), 2, "summary exposes the count that would block spawns");
    // Still present and not dead in the roster.
    for w in [&w1, &w2] {
        let alive = reg.list_agents(&g.id).as_array().unwrap().iter().any(|a| {
            a["id"] == json!(w.id) && a["status"] != json!("dead")
        });
        assert!(alive, "worker {} must stay alive", w.id);
    }
}

#[test]
fn max_agents_change_survives_launcher_relaunch() {
    let dir = tempfile::tempdir().unwrap();
    let gid;
    let path;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap(); // launcher cap 2
        gid = g.id.clone();
        path = reg.state_root().join(g.id.as_str()).join("group.json");
        reg.set_max_agents(&g.id, 9, "human").unwrap();
    }
    // group.json carries the new cap; unrelated fields are preserved (the
    // update patches the field in place rather than rewriting the file).
    let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(v["guardrails"]["max_agents"].as_u64().unwrap(), 9);
    assert!(v["created_ms"].as_u64().is_some(), "created_ms must survive the patch");
    // The in-place patch must not disturb the block roster (#222) — it is the
    // group's whole agent identity, and set_max_agents rewrites one integer.
    let worker = v["guardrails"]["blocks"]
        .as_array()
        .expect("the roster must survive the patch")
        .iter()
        .find(|b| b["id"] == "worker")
        .expect("the worker block must survive the patch");
    assert_eq!(worker["model"], json!("sonnet"), "other guardrails must survive the patch");
    // A fresh registry (app restart) + a real launcher relaunch on the same
    // repo: this drives create_group's actual resume path, not a hand-fed
    // group.json. The launcher hardcodes its default cap (rails() = 2), but the
    // persisted adjustment (9) must win — otherwise the relaunch silently
    // reverts 9→2. Other guardrails still come from the launch.
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    assert_eq!(g.id, gid, "restart resumes the same group");
    assert_eq!(g.guardrails.max_agents, 9, "the persisted cap wins over the launcher default on resume");
    // ...and it's the value the resumed group actually holds + re-persists.
    assert_eq!(reg.group(&gid).unwrap().guardrails.max_agents, 9);
    let v: Value =
        serde_json::from_str(&fs::read_to_string(reg.state_root().join(gid.as_str()).join("group.json")).unwrap()).unwrap();
    assert_eq!(v["guardrails"]["max_agents"].as_u64().unwrap(), 9, "resume re-persists the honored cap");
}

#[test]
fn intake_gate_config_survives_launcher_relaunch() {
    // rev-33 N4: `create_group_ex`'s "honor the persisted value on a relaunch"
    // block already covers `max_agents` (see the test above), `idle_tick_
    // minutes`, `compact_nudge_*` — but `intake_poll_minutes`/`idle_tick_
    // fallback_minutes` were missing from it entirely. A launcher relaunch
    // (Launch::Fresh, the launcher's bare caller Guardrails — no UI field for
    // either) silently reset a hand-edited `Some(0)` opt-out back to `None`
    // (the gate switching back ON) and a custom fallback cadence back to the
    // default, every single time. Mirrors `max_agents_change_survives_
    // launcher_relaunch`'s exact restart-a-fresh-registry shape.
    let dir = tempfile::tempdir().unwrap();
    let gid;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg
            .create_group(
                "C:/tmp/repo",
                Guardrails { intake_poll_minutes: Some(0), idle_tick_fallback_minutes: 45, ..rails() },
            )
            .unwrap();
        gid = g.id.clone();
    }
    // A fresh registry (app restart) + a real launcher relaunch on the same
    // repo, with the LAUNCHER'S bare defaults (it has no field for either
    // knob): the persisted opt-out/cadence must win, not the launcher's.
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    assert_eq!(g.id, gid, "restart resumes the same group");
    assert_eq!(g.guardrails.intake_poll_minutes, Some(0),
        "a hand-edited opt-out must survive a launcher relaunch, not silently reset to the smart default");
    assert_eq!(g.guardrails.idle_tick_fallback_minutes, 45,
        "a custom fallback cadence must likewise survive a launcher relaunch");
}

#[test]
fn intake_gate_absent_config_still_smart_defaults_after_relaunch() {
    // The other direction of the same fix: a group that never set an explicit
    // value (`None` on disk, `rails()`'s bare default) must still resolve to
    // the smart default after a relaunch — the N4 fix reads the persisted
    // value through unchanged, it must never coerce `None` into `Some(0)` (or
    // vice versa) along the way.
    let dir = tempfile::tempdir().unwrap();
    let gid;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        gid = g.id.clone();
    }
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    assert_eq!(g.id, gid, "restart resumes the same group");
    assert_eq!(g.guardrails.intake_poll_minutes, None,
        "absent must stay absent across a relaunch, not get pinned to some resolved value");
    reg.set_autonomous(&gid, true).unwrap();
    assert_eq!(
        intake::effective_intake_poll_minutes(reg.group(&gid).unwrap().guardrails.intake_poll_minutes, true),
        5, // DEFAULT_INTAKE_POLL_MINUTES's current value, pinned by literal here rather than imported
        "the smart default must still apply post-relaunch once autonomous mode is on"
    );
}

#[test]
fn new_group_still_honors_the_launcher_cap() {
    // The resume-prefers-persisted rule must not leak into genuinely new
    // groups: a first launch on a repo uses the caller's cap verbatim.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/fresh-repo", Guardrails { max_agents: 5, ..rails() }).unwrap();
    assert_eq!(g.guardrails.max_agents, 5, "a new group takes the launcher's cap");
}

#[test]
fn max_agents_change_audits_and_notifies_orchestrator() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap(); // cap 2
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    // Pause so the notice is QUEUED-and-audited rather than pasted (test mode
    // has no pane to type into) — this lets us observe the exact notice text.
    pause_with_pane(&reg, &g.id, &orch.id, 6001);
    reg.set_max_agents(&g.id, 4, "human").unwrap();
    // The audit is immediate (per-click); the notice is debounced (#79), so it
    // is delivered only when its window has elapsed — drive the flush past the
    // 3s debounce deterministically (no sleep) to observe the notice text.
    reg.flush_due_max_notices(now_ms() + 4_000);
    let log = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    let events: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(
        events.iter().any(|e|
            e["action"] == json!("max-agents-set")
            && e["detail"]["from"] == json!(2) && e["detail"]["to"] == json!(4)
            && e["actor"] == json!("human")),
        "the cap change must be audited with from/to and the actor"
    );
    assert!(
        events.iter().any(|e|
            e["action"] == json!("prompt")
            && e["detail"]["text"].as_str().unwrap_or("").contains("max live agents changed 2→4")),
        "the orchestrator must receive the cap-change re-plan notice"
    );
}

// Helper: read the audit log and count the coalesced re-plan notices (visible
// as `prompt` entries — the group is paused in these tests, so #569 queues the
// delivery rather than pasting it, and the text is audited either way).
fn replan_notices(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    delivered_texts(reg, group)
        .into_iter()
        .filter(|t| t.contains("max live agents changed"))
        .collect()
}

#[test]
fn rapid_max_agents_clicks_coalesce_to_one_notice() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap(); // cap 2
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    pause_with_pane(&reg, &g.id, &orch.id, 6002);
    // A burst of stepper clicks 2→4→6→3, all within the debounce window (test
    // calls land in the same few ms). Each persists + enforces + audits per
    // click, but the notice is held.
    reg.set_max_agents(&g.id, 4, "human").unwrap();
    reg.set_max_agents(&g.id, 6, "human").unwrap();
    reg.set_max_agents(&g.id, 3, "human").unwrap();
    // Before the window elapses, nothing has been delivered.
    reg.flush_due_max_notices(now_ms());
    assert!(replan_notices(&reg, &g.id).is_empty(), "no notice fires mid-burst");
    // Every click is audited (enforcement/persist stay per-click).
    let sets = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| l.contains("max-agents-set"))
        .count();
    assert_eq!(sets, 3, "each click is audited immediately");
    // Once the window passes: exactly ONE notice, spanning the whole burst
    // (2→3), never the intermediate 2→4 / 4→6 values.
    reg.flush_due_max_notices(now_ms() + 4_000);
    let notices = replan_notices(&reg, &g.id);
    assert_eq!(notices.len(), 1, "a burst yields one coalesced notice, got: {notices:?}");
    assert!(notices[0].contains("2→3"), "notice spans the whole burst, got: {}", notices[0]);
}

#[test]
fn spaced_max_agents_changes_notify_separately() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap(); // cap 2
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    pause_with_pane(&reg, &g.id, &orch.id, 6003);
    // First change flushes fully before the second is recorded → two notices.
    reg.set_max_agents(&g.id, 5, "human").unwrap();
    reg.flush_due_max_notices(now_ms() + 4_000);
    reg.set_max_agents(&g.id, 2, "human").unwrap();
    reg.flush_due_max_notices(now_ms() + 8_000);
    let notices = replan_notices(&reg, &g.id);
    assert_eq!(notices.len(), 2, "spaced changes stay separate, got: {notices:?}");
    assert!(notices[0].contains("2→5"), "first notice: {}", notices[0]);
    assert!(notices[1].contains("5→2"), "second notice: {}", notices[1]);
}

#[test]
fn planners_count_toward_live_delegates_summary() {
    // #47 makes planners count against the cap (live_delegate_count includes
    // them); the summary's live_delegates must agree so the UI's "cap below N
    // live" warning stays honest.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", Guardrails { max_agents: 5, ..rails() }).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w1", "t", false, None).unwrap();
    reg.spawn_agent(&g.id, Role::Reviewer, "r1", "t", false, None).unwrap();
    reg.spawn_agent(&g.id, Role::Planner, "p1", "t", false, None).unwrap();
    let sum = reg.group_summary(&g.id);
    assert_eq!(
        sum["live_delegates"].as_u64().unwrap(),
        3,
        "worker + reviewer + planner all count against the cap"
    );
}

#[test]
fn setting_same_max_agents_is_a_noop() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap(); // cap 2
    assert_eq!(reg.set_max_agents(&g.id, 2, "human").unwrap(), 2);
    let log = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    assert!(!log.contains("max-agents-set"), "a no-op change must not audit or notify");
}

#[test]
fn set_max_agents_fails_soft_on_corrupt_group_file() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // A valid-JSON but non-object root (e.g. from corruption) must error rather
    // than panic on the in-place field assignment.
    fs::write(reg.state_root().join(g.id.as_str()).join("group.json"), "null").unwrap();
    let err = reg.set_max_agents(&g.id, 5, "human").unwrap_err();
    assert!(err.contains("not a JSON object"), "non-object root must fail soft, got: {err}");
}

#[test]
fn max_agents_notice_reads_naturally() {
    assert_eq!(
        max_agents_notice(4, 2),
        "[orrerix] max live agents changed 4→2 — re-plan accordingly"
    );
}

#[test]
fn workflow_mode_notice_reads_naturally() {
    let all_pass_gate = workflow::Gate {
        require: workflow::GateRequire::AllPass,
        reviewers: vec!["rev-orch".into(), "rev-ui".into(), "rev-tests".into()],
        also: vec!["ci-green".into()],
        max_diff_lines: None,
        routing: Vec::new(),
    };
    assert_eq!(
        workflow_mode_notice(true, "loomux", Some(&all_pass_gate)),
        "[orrerix] workflow mode changed: 'loomux' active, merge gate requires all of \
         [rev-orch, rev-ui, rev-tests] · ci-green — re-plan your spawn/review strategy."
    );
    let threshold_gate = workflow::Gate {
        require: workflow::GateRequire::Threshold(2),
        reviewers: vec!["a".into(), "b".into(), "c".into()],
        also: vec![],
        max_diff_lines: None,
        routing: Vec::new(),
    };
    assert_eq!(
        workflow_mode_notice(true, "focused-review", Some(&threshold_gate)),
        "[orrerix] workflow mode changed: 'focused-review' active, merge gate requires 2 of \
         [a, b, c] — re-plan your spawn/review strategy."
    );
    assert_eq!(
        workflow_mode_notice(true, "loomux", None),
        "[orrerix] workflow mode changed: 'loomux' active, no merge gate declared — re-plan \
         your spawn/review strategy."
    );
    assert_eq!(
        workflow_mode_notice(false, "loomux", None),
        "[orrerix] workflow mode changed: built-in roster, no merge gate — re-plan your \
         spawn/review strategy."
    );
    // `off` ignores whatever gate/name it is (mis)called with — off has neither,
    // by construction, whatever the caller passes.
    assert_eq!(
        workflow_mode_notice(false, "loomux", Some(&all_pass_gate)),
        workflow_mode_notice(false, "", None)
    );
}

#[test]
fn agent_config_carries_token_and_server_url() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo2", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let cfg = fs::read_to_string(
        reg.state_root().join(g.id.as_str()).join("configs").join(format!("{}.json", w.id)),
    )
    .unwrap();
    assert!(cfg.contains(&w.token), "config must carry the agent token");
    assert!(cfg.contains("127.0.0.1:45999/mcp"));
}

#[test]
fn token_resolution_and_group_isolation() {
    let (reg, _d) = test_registry();
    let ga = reg.create_group("C:/tmp/repo-a", rails()).unwrap();
    let gb = reg.create_group("C:/tmp/repo-b", rails()).unwrap();
    let wa = reg.spawn_agent(&ga.id, Role::Worker, "wa", "t", false, None).unwrap();
    let wb = reg.spawn_agent(&gb.id, Role::Worker, "wb", "t", false, None).unwrap();
    let ca = reg.resolve_token(&wa.token).unwrap();
    assert_eq!(ca.group, ga.id);
    // Group A's roster never shows group B's agents.
    let roster = reg.list_agents(&ga.id).to_string();
    assert!(roster.contains(wa.id.as_str()) && !roster.contains(wb.id.as_str()));
    // Dead agents lose their token entirely.
    reg.mark_dead(&wa.id, Some(1));
    assert!(reg.resolve_token(&wa.token).is_none());
    assert!(reg.resolve_token("no-such-token").is_none());
}

#[test]
fn state_persists_across_registry_instances() {
    let dir = tempfile::tempdir().unwrap();
    let gid;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        gid = g.id.clone();
        reg.set_state(&g.id, r#"{"queue":[12,13]}"#).unwrap();
    }
    // Fresh instance (app restart) + same repo → same group id and state.
    let reg = relaunch_registry(dir.path());
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    assert_eq!(g.id, gid, "group id must be stable per repo for resume");
    assert_eq!(reg.get_state(&g.id), r#"{"queue":[12,13]}"#);
}

#[test]
fn group_id_normalizes_path_but_separates_repos() {
    let (reg, _d) = test_registry();
    let a = reg.create_group("C:\\Tmp\\Repo", rails()).unwrap();
    let b = reg.create_group("c:/tmp/repo/", rails()).unwrap();
    let c = reg.create_group("C:/tmp/other", rails()).unwrap();
    assert_eq!(a.id, b.id, "case/separator/trailing-slash variants are the same repo");
    assert_ne!(a.id, c.id, "different repos must not share state");
}

#[test]
fn state_rejects_invalid_input() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    assert!(reg.set_state(&g.id, "not json").is_err());
    let huge = format!("{{\"x\":\"{}\"}}", "a".repeat(512 * 1024));
    assert!(reg.set_state(&g.id, &huge).is_err());
    assert_eq!(reg.get_state(&g.id), "{}", "failed writes must not corrupt state");
}

#[test]
fn audit_log_records_lifecycle_as_json_lines() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w", "do a thing", false, None).unwrap();
    reg.set_state(&g.id, "{}").unwrap();
    let log = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("audit.jsonl")).unwrap();
    let events: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(
        events.iter().any(|e| e["detail"]["task"] == "do a thing"),
        "spawn audit must capture the task brief"
    );
    let kinds: Vec<&str> = events.iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"group-create"));
    assert!(kinds.contains(&"agent-spawn"));
    assert!(kinds.contains(&"state-write"));
    assert!(events.iter().all(|e| e["ts_ms"].as_u64().is_some()));
}

#[test]
fn bracketed_paste_frames_text_and_normalizes_crlf() {
    let p = bracketed_paste("line1\r\nline2");
    let s = String::from_utf8(p).unwrap();
    assert!(s.starts_with("\x1b[200~") && s.ends_with("\x1b[201~"));
    assert!(
        s.contains("line1\nline2") && !s.contains('\r'),
        "CR must not leak inside a paste — it would submit early"
    );
}

#[test]
fn submit_sequence_claude_is_a_bare_cr() {
    // Claude's submit must stay byte-identical: a single carriage return, no
    // focus prefix, no CRLF. Any drift here would regress the tuned TUI path.
    assert_eq!(submit_sequence("claude"), b"\r");
}

#[test]
fn submit_sequence_unknown_cli_falls_back_to_bare_cr() {
    // An unrecognized / future CLI gets the safe default (bare CR), never the
    // Copilot-specific focus prefix.
    assert_eq!(submit_sequence("aider"), b"\r");
    assert_eq!(submit_sequence(""), b"\r");
}

#[test]
fn submit_sequence_copilot_prefixes_focus_in_before_enter() {
    // #98: Copilot drops non-paste keystrokes on an unfocused pane, so a bare
    // CR after the paste never submits. The fix prefixes a focus-in report
    // (CSI I) that flips Copilot's focus flag true, so the CR that follows is
    // accepted. Order matters: focus-in MUST come before the CR.
    let seq = submit_sequence("copilot");
    assert_eq!(seq, b"\x1b[I\r");
    assert!(seq.starts_with(b"\x1b[I"), "focus-in must precede the Enter");
    assert!(seq.ends_with(b"\r"), "the Enter itself is still a bare CR");
    // The focus report is exactly CSI I (no params/intermediates) — that is
    // what Copilot's CSI parser maps to a focus event; a stray param would
    // parse as something else and not flip the flag.
    assert_eq!(&seq[..seq.len() - 1], b"\x1b[I");
}

#[test]
fn flush_reuses_the_per_cli_submit_sequence() {
    // The stranded-text flush presses submit once — it must use the SAME
    // per-CLI sequence as a normal submit, so Copilot's flush also carries the
    // focus-in prefix (a bare CR would be ignored on an unfocused pane, and the
    // stranded text would never clear).
    assert_eq!(submit_sequence("copilot"), b"\x1b[I\r");
    assert_eq!(submit_sequence("claude"), b"\r");
}

#[test]
fn should_flush_only_on_the_stranded_text_signature() {
    // First delivery to a pane (no prior outcome): never flush.
    assert!(!should_flush_before_paste(None, false));
    assert!(!should_flush_before_paste(None, true));
    // Previous delivery confirmed as submitted: box is empty, never flush.
    assert!(!should_flush_before_paste(Some(true), false));
    assert!(!should_flush_before_paste(Some(true), true));
    // Previous delivery unconfirmed BUT a human has typed since: their line may
    // be in the box — never blind-submit it.
    assert!(!should_flush_before_paste(Some(false), true));
    // The one case that flushes: previous unconfirmed, no human input since.
    assert!(should_flush_before_paste(Some(false), false));
}

#[test]
fn submit_confirmed_needs_a_real_output_burst() {
    // Pane reached quiet before Enter (reached_quiet = true):
    // No / trivial growth after Enter -> not confirmed (an ignored key, or idle
    // cursor-blink noise, must not read as a landed submit).
    assert!(!submit_confirmed(true, 1000, 1000));
    assert!(!submit_confirmed(true, 1000, 1010));
    // A burst clearing the threshold -> confirmed.
    assert!(submit_confirmed(true, 1000, 1024));
    assert!(submit_confirmed(true, 1000, 100_000));
    // Totals never go backwards, but a wrapped/garbage reading must not panic
    // or false-confirm.
    assert!(!submit_confirmed(true, 1000, 500));
}

#[test]
fn submit_never_confirmed_when_quiet_was_not_reached() {
    // rev-32: on a busy pane the submit-wait hits SUBMIT_MAX_WAIT without ever
    // reaching quiet, so the Enter lands mid-stream. Even a large burst is that
    // ongoing stream, not the submit — it must NOT confirm, else the prompt is
    // stranded but recorded confirmed and the next delivery skips the flush.
    assert!(!submit_confirmed(false, 1000, 100_000));
    assert!(!submit_confirmed(false, 1000, 1024));
    assert!(!submit_confirmed(false, 1000, 1000));
}

#[test]
fn unconfirmed_notice_fires_only_for_a_stranded_worker_delivery() {
    // The one case that notifies: a delivery to a non-orchestrator agent whose
    // submit went unconfirmed — the prompt may be sitting unsubmitted in the box.
    assert!(should_notify_unconfirmed(false, false));
    // Confirmed submit: the prompt landed, nothing to chase.
    assert!(!should_notify_unconfirmed(false, true));
    // Target IS the orchestrator: a notice to it would itself be a delivery to
    // the orchestrator — an endless loop. Never notify, confirmed or not; those
    // rely on #99's stranded-text flush on the next delivery instead.
    assert!(!should_notify_unconfirmed(true, false));
    assert!(!should_notify_unconfirmed(true, true));
}

#[test]
fn unconfirmed_notice_text_names_the_agent_and_the_recovery_move() {
    let msg = unconfirmed_delivery_notice("w-3", &[4_242]);
    assert!(msg.starts_with("[orrerix] "), "notice is a loomux system message: {msg}");
    assert!(msg.contains("w-3"), "notice must name the stranded agent: {msg}");
    assert!(msg.contains("unconfirmed"), "notice must state the condition: {msg}");
    // Points the orchestrator at the recovery move from the template.
    assert!(msg.contains("get_output"), "notice must point at reading the pane: {msg}");
    assert!(msg.contains("re-send"), "notice must point at re-sending: {msg}");
}

// ---------- #281: surfacing a silent early exit ----------

#[test]
fn exit_diagnostic_names_the_silent_death_when_nothing_was_ever_printed() {
    // The #281 signature: a resumed CLI that exits before printing a single
    // byte. A bare exit code can't distinguish this from "did real work, then
    // failed" — the notice must say so explicitly.
    let msg = exit_diagnostic("", 0);
    assert!(msg.contains("no output"), "must name the zero-output case: {msg}");
    assert!(
        msg.contains("session") || msg.contains("cwd") || msg.contains("flag"),
        "must suggest plausible causes so the orchestrator has somewhere to look: {msg}"
    );
}

#[test]
fn exit_diagnostic_shows_the_tail_when_the_process_actually_produced_output() {
    // A crash mid-work is a different failure than a silent DOA death — the
    // notice must carry what the CLI actually printed, not the zero-output
    // wording, and must never invent content that wasn't captured.
    let msg = exit_diagnostic("Error: something broke\npanic at line 9", 42);
    assert!(!msg.contains("no output"), "must not claim silence when bytes were produced: {msg}");
    assert!(msg.contains("something broke"), "must carry the real captured output: {msg}");
}

#[test]
fn exit_diagnostic_snippet_is_bounded_not_the_whole_captured_tail() {
    // A saturated ring can be large; the orchestrator notice is a diagnostic
    // hint, not a full transcript dump.
    let huge = "x".repeat(10_000);
    let msg = exit_diagnostic(&huge, 10_000);
    assert!(msg.len() < 1000, "snippet must be bounded, got {} chars", msg.len());
}

#[test]
fn exit_cause_never_misdiagnoses_an_expected_kill_of_a_productive_agent() {
    // The bug: `PtyManager::kill` (pty.rs) removes the pty handle from the
    // live map BEFORE the waiter thread can snapshot it, so an idle-kill or
    // kill_agent of a delegate that produced plenty of real output STILL
    // arrives here with tail="" and total_bytes==0 — indistinguishable, by
    // the numbers alone, from a genuine silent death. `expected` is the only
    // thing that tells them apart, and it must win: an expected exit is never
    // reported as "produced no output" / "missing/corrupt session", however
    // little the (unreliable, in this case) tail/total say was captured.
    let msg = exit_cause(true, "", 0);
    assert!(!msg.contains("no output"), "an expected kill must never be misdiagnosed: {msg}");
    assert!(!msg.contains("corrupt"), "must not blame a corrupt session on a deliberate stop: {msg}");
    assert!(msg.contains("stopped"), "must say loomux stopped it, got: {msg}");

    // An UNEXPECTED exit with the exact same (tail="", total=0) numbers is the
    // real #281 signature and must still get the full diagnostic.
    let msg = exit_cause(false, "", 0);
    assert!(msg.contains("no output"), "an unexpected silent exit must still be diagnosed: {msg}");
}

#[test]
fn agent_output_tail_prefers_live_output_but_falls_back_to_the_captured_exit_tail() {
    // Live output (the pty is still alive) always wins over whatever was
    // captured at a PAST exit.
    assert_eq!(
        resolve_output_text(Some("live text".to_string()), Some("stale exit tail")).unwrap(),
        "live text"
    );
    // The live pty is gone (the agent exited) — #281's fallback answers with
    // what was captured at exit time instead of failing outright.
    assert_eq!(
        resolve_output_text(None, Some("captured at exit")).unwrap(),
        "captured at exit"
    );
    // Nothing live AND nothing captured (a plain pane, or one that exited
    // before #281 shipped) — the original "terminal already closed" error,
    // not a fabricated answer.
    let err = resolve_output_text(None, None).unwrap_err();
    assert!(err.contains("already closed"), "must keep the original error, got: {err}");
    // An empty captured tail is the same as nothing captured — never "answer"
    // with an empty string as if that were meaningful output.
    let err = resolve_output_text(None, Some("")).unwrap_err();
    assert!(err.contains("already closed"), "empty capture must not be treated as an answer: {err}");
}

#[test]
fn classify_human_input_reads_box_occupancy_from_keystroke_content() {
    // Printable text → a line now sits in the box.
    assert_eq!(classify_human_input("a"), HumanInput::Content);
    assert_eq!(classify_human_input("/model"), HumanInput::Content);
    assert_eq!(classify_human_input("dfgdsfg"), HumanInput::Content);
    // Enter (any newline form) submits — the box clears. This is the fix's crux:
    // a sub-"burst" submit (empty Enter, short command) is still positively a
    // submit, so the pending flag can't get stuck (finding #2).
    assert_eq!(classify_human_input("\r"), HumanInput::Submit);
    assert_eq!(classify_human_input("\n"), HumanInput::Submit);
    assert_eq!(classify_human_input("\r\n"), HumanInput::Submit);
    assert_eq!(classify_human_input("ls\r"), HumanInput::Submit); // typed + submitted in one write
    // Text AFTER the last newline is a fresh unsubmitted line → still Content.
    assert_eq!(classify_human_input("done\rmore"), HumanInput::Content);
    // Explicit line-clear controls empty the box.
    assert_eq!(classify_human_input("\u{15}"), HumanInput::Submit); // Ctrl-U
    assert_eq!(classify_human_input("\u{03}"), HumanInput::Submit); // Ctrl-C
    // Navigation / editing that adds no visible text leaves occupancy unchanged —
    // a stray arrow or backspace must NOT mark an empty box as pending (else a
    // delivery to an idle pane would wedge).
    assert_eq!(classify_human_input("\u{1b}[C"), HumanInput::Neutral); // right arrow
    assert_eq!(classify_human_input("\u{1b}[A"), HumanInput::Neutral); // up arrow
    assert_eq!(classify_human_input("\u{7f}"), HumanInput::Neutral); // backspace/DEL
    assert_eq!(classify_human_input(""), HumanInput::Neutral);
    // A bracketed paste is text sitting UNSUBMITTED in the box → Content.
    assert_eq!(classify_human_input("\u{1b}[200~hello\u{1b}[201~"), HumanInput::Content);
    // The finding-#1 shape: a paste ENDING IN A NEWLINE. The pasted newline is
    // literal under bracketed-paste mode (the CLI holds it unsubmitted), so the
    // marker — checked before the trailing-newline rule — must keep this Content,
    // NOT Submit. Reading it as submitted would let the next delivery merge-submit
    // the human's paste (the exact #111 loss).
    assert_eq!(classify_human_input("\u{1b}[200~foo\n\u{1b}[201~"), HumanInput::Content);
    assert_eq!(classify_human_input("\u{1b}[200~foo\r\n\u{1b}[201~"), HumanInput::Content);
    // A multi-line paste (interior newlines) is likewise held unsubmitted → Content.
    assert_eq!(classify_human_input("\u{1b}[200~a\nb\nc\u{1b}[201~"), HumanInput::Content);
    // Even an empty bracketed paste carries the markers → Content (pending, the
    // safe-hold direction), never a spurious Submit.
    assert_eq!(classify_human_input("\u{1b}[200~\u{1b}[201~"), HumanInput::Content);
}

#[test]
fn paste_gate_holds_until_clear_then_pastes_or_aborts_at_the_cap() {
    let cap = Duration::from_secs(60);
    // Box empty → paste immediately (the normal delivery path).
    assert_eq!(resolve_paste_gate(false, Duration::ZERO, cap), PasteGate::Paste);
    assert_eq!(resolve_paste_gate(false, cap, cap), PasteGate::Paste);
    // Human's line still sitting, within the bound → keep holding.
    assert_eq!(resolve_paste_gate(true, Duration::from_secs(1), cap), PasteGate::Hold);
    // Bound elapsed and the line never cleared → abort (never blind-merge).
    assert_eq!(resolve_paste_gate(true, cap, cap), PasteGate::Abort);
    assert_eq!(resolve_paste_gate(true, cap + Duration::from_millis(1), cap), PasteGate::Abort);
}

// ---------- #445: delivery queue notice vocabulary ----------
//
// The old `paste_held_notice`/`question_held_notice`/`held_delivery_notice`
// (and their gate `should_notify_paste_held`) said "held: ... re-send when
// clear" — the exact honesty defect #445 exists to fix: a delivery that hit
// its hold cap was DESTROYED, not held, and re-sending was actively harmful
// once queueing made it unnecessary. They are deleted; `queue::queued_notice`
// (below) replaces them. See `queue.rs`'s own unit tests for the full
// coverage of the new notice text — these two just pin the property that
// specifically motivated the deletion.

#[test]
fn queued_notice_replaces_the_deleted_re_send_wording() {
    let msg = queue::queued_notice("w-9", queue::EnqueueReason::BoxOccupied);
    assert!(msg.starts_with("[orrerix] "), "notice is a loomux system message: {msg}");
    assert!(msg.contains("w-9"), "notice must name the target agent: {msg}");
    assert!(!msg.to_lowercase().contains("re-send when clear"), "the old misleading phrasing must be gone: {msg}");
    assert!(msg.contains("do NOT re-send"), "must warn against re-sending a queued payload: {msg}");

    let msg = queue::queued_notice("w-9", queue::EnqueueReason::Question);
    assert!(msg.contains("interactive question"), "must name the condition: {msg}");
}

#[test]
fn orchestrator_template_no_longer_instructs_a_re_send_on_a_held_delivery() {
    // #445 plan step 5: delete every "re-send when clear" instruction from
    // the live text. This pins ORCHESTRATOR_TPL and, since #1683 moved the
    // delivery-notice procedure into the playbook, the playbook template with
    // it — the concatenation is the text an orchestrator actually reads. (The
    // pre222 golden is separate by design: it tracks the live default-group
    // text and is re-blessed whenever that text deliberately changes
    // (`a_workflow_placeholder_must_sit_at_the_end_of_a_line_it_shares` in
    // tests/workflow.rs enforces the re-bless; see
    // tests/fixtures/pre222/README.md's changelog for the entry).)
    let both = format!("{ORCHESTRATOR_TPL}{ORCHESTRATOR_PLAYBOOK_TPL}");
    assert!(
        !both.to_lowercase().contains("re-send when clear"),
        "the live orchestrator text must not instruct re-sending a queued delivery"
    );
    assert!(
        both.contains("do NOT"),
        "the live orchestrator text must explicitly warn against re-sending a queued payload"
    );
    assert!(
        both.contains("queued"),
        "the live orchestrator text must describe the new hold-means-queued behavior"
    );
}

// ---------- #590: no role may block a turn waiting on CI ----------
//
// Live deadlock, #577: a worker registered a `notify_when` CI watch (right)
// AND ALSO blocked its own turn on a shell-level wait for the same checks
// (fatal). The PR had gone CONFLICTING under two merges, so GitHub was never
// going to create the check-suites that wait was blocked on — and the watch's
// own CONFLICTING notice (#337, built for exactly this) is delivered by TYPING
// INTO THE PANE, which a pane mid-turn cannot accept. The turn was waiting on
// a resolution queued behind itself; a host watchdog plus a human reading the
// pane by hand broke it, 20+ minutes later.
//
// `orchestrator.md` had carried this rule for weeks ("never sit in a wait
// loop, never `sleep`"); the DELEGATE templates never got it, which is the
// whole gap #590 names. The two tests below pin it where it can actually
// regress — the live templates — rather than only in the pre222 golden, which
// fails as "re-bless me" and teaches nobody which rule went missing.

/// Every role that can register a CI watch is told what the watch is for and
/// which case it exists to catch. Concepts, not sentences: the tool to call,
/// the kind to call it with, and `CONFLICTING` — the state whose notice is the
/// ONLY thing that will ever arrive, because no check-suite is ever created
/// for it. Prose may be rewritten freely; a version missing any of these is no
/// longer telling the reader how to stop waiting.
#[test]
fn every_role_template_names_the_ci_watch_and_the_conflicting_case() {
    // #1683 moved the orchestrator's CI-gate / monitoring procedure into the
    // playbook, so the orchestrator's surface is core + playbook — the
    // concatenation is the text it actually reads.
    let orch = format!("{ORCHESTRATOR_TPL}{ORCHESTRATOR_PLAYBOOK_TPL}");
    for (role, tpl) in
        [("orchestrator", orch.as_str()), ("worker", WORKER_TPL), ("reviewer", REVIEWER_TPL)]
    {
        for concept in ["notify_when", "pr_checks", "CONFLICTING"] {
            assert!(
                tpl.contains(concept),
                "{role}.md no longer names `{concept}` — a role that cannot name the watch, \
                 or the one PR state that never produces checks, has been left to wait (#590)"
            );
        }
    }
}

/// The delegate half, and the one #590 actually filed. A worker or reviewer
/// wakes ONLY when something is typed into its pane, so for them "don't poll"
/// is not enough advice — the rule has to say **end the turn**, and it has to
/// carry its own why (the deadlock), or it reads as a performance preference
/// and loses to "I'll just wait for this one run".
#[test]
fn delegate_templates_forbid_blocking_a_turn_on_ci() {
    for (role, tpl) in [("worker", WORKER_TPL), ("reviewer", REVIEWER_TPL)] {
        let lower = tpl.to_lowercase();
        assert!(
            lower.contains("end the turn"),
            "{role}.md must tell the delegate to END THE TURN after registering the watch — \
             a delegate only wakes on a pane delivery, and a mid-turn pane takes none (#590)"
        );
        assert!(
            lower.contains("deadlock"),
            "{role}.md must state the deadlock mechanism, not just the prohibition: a rule \
             with no why is a rule a delegate talks itself out of once (#590)"
        );
        assert!(
            lower.contains("sleep"),
            "{role}.md must name `sleep` (and its relatives) as banned — the rule has to name \
             the thing it bans or it is advice about polling frequency (#590)"
        );
    }
}

// ---------- #596: a PR body's claims are about a SHA and a scope ----------
//
// One batch, two claim families, both of them a sentence that quietly stopped
// being true while the text stayed put.
//
// (a) STALE GREEN. A run citation is a fact about a SHA, not about a PR: any
// push or rebase invalidates it and the body survives untouched. Three
// instances, two workers, one batch — #571 cited a run three commits behind
// head; #588 cited a pre-rebase run at review 1 and then the SAME pre-rebase
// run again after the rebase at review 2. Every one was caught by a reviewer,
// none by the worker who wrote it, which is why the fix has to be structural
// (re-derive) rather than an exhortation to be careful.
//
// (b) STALE SCOPE. `Closes #N` is a claim about scope, and a squash merge
// honors it out of the squashed commit message however partial the change
// actually was — #569 and #590 were both auto-closed this session with real
// scope still open and had to be reopened by hand.
//
// Pinned on the LIVE worker template rather than the pre222 golden, for the
// same reason as #590's pair above: the golden fails as "re-bless me", which
// names no rule and teaches nobody which one went missing.

/// `worker.md` as a worker actually reads it — the template with `{{DOD}}`
/// substituted (#3040 P2).
///
/// The definition of done is ONE copy (`templates/dod.md`) and `worker.md`
/// carries a placeholder where the section used to be, so `WORKER_TPL` alone is
/// no longer the worker's contract: it is the contract minus its DoD. Same
/// composition `every_role_template_names_the_ci_watch_and_the_conflicting_case`
/// above already does for the orchestrator's core + playbook, and for the same
/// reason — a pin whose subject moved behind a substitution must read the text
/// the agent is held to, never the source file that happens to hold most of it.
///
/// The `assert!` is the vacuity control, and it is not decoration: without it a
/// renamed or unregistered placeholder makes this return the template unchanged,
/// and every pin below then passes or fails for a reason that has nothing to do
/// with the rule it names.
fn worker_contract_text() -> String {
    let composed = WORKER_TPL.replace("{{DOD}}", loomux_lib::orchestration::brief::dod_body());
    assert!(
        !composed.contains("{{DOD}}") && composed.len() > WORKER_TPL.len(),
        "the DoD substitution did not happen — these pins would be reading worker.md WITHOUT \
         its definition of done, which is the one section they are about"
    );
    composed
}

/// The rule has to be a *procedure*, not "keep the body accurate". These three
/// are what make a citation checkable rather than trusted: the command that
/// lists a run with its commit, the field that carries that commit
/// (`headSha` — the whole point, since a run id alone says nothing about which
/// tree it ran on), and the local head to compare it against (`rev-parse`).
/// Prose may be rewritten freely; a version missing any of them no longer
/// tells a worker how to tell a live citation from a dead one.
#[test]
fn worker_template_requires_re_deriving_run_citations_after_a_push() {
    let tpl = worker_contract_text();
    for concept in ["gh run list", "headSha", "rev-parse"] {
        assert!(
            tpl.contains(concept),
            "worker.md no longer names `{concept}` — without the command and the field that \
             tie a run to a commit, 're-derive your citations' is a wish, not a procedure (#596)"
        );
    }
    let lower = tpl.to_lowercase();
    assert!(
        lower.contains("stale"),
        "worker.md must say what a citation BECOMES after a push (stale) — a rule that only \
         says 'check your links' is one a worker reads as already satisfied (#596)"
    );
}

/// The scope half. `Closes #N` was already in this template as the one way to
/// link an issue, so the fix is not "mention Part of" — it is that the
/// alternative exists AND that the mechanism is named: a squash merge honors
/// `Closes` regardless of scope, so hedging elsewhere in the body does not
/// save the issue. Without the mechanism this reads as a style preference,
/// which is exactly how #569 and #590 got closed.
#[test]
fn worker_template_reserves_closes_for_a_pr_that_finishes_the_issue() {
    let tpl = worker_contract_text();
    for concept in ["Closes #N", "Part of #N"] {
        assert!(
            tpl.contains(concept),
            "worker.md no longer offers `{concept}` — a partial-scope PR with no keyword of \
             its own goes on writing `Closes` and auto-closing live issues (#596)"
        );
    }
    assert!(
        tpl.to_lowercase().contains("squash merge"),
        "worker.md must name the squash merge as the mechanism that honors `Closes` whatever \
         the scope — unstated, the keyword choice reads as a style preference (#596)"
    );
}

// ---------- #625: choosing the right keyword is not enough ----------
//
// The rider on #596's scope half above, and the reason that half needed one:
// #569 was auto-closed by a squash a SECOND time, an hour after the first,
// through a PR (#615) that had deliberately linked `Part of #569` and argued the
// choice at length. What closed it was that argument's own last sentence, which
// asked a human to close the issue by hand and named it right after the verb.
//
// GitHub's scan is textual and context-blind: `close`/`fix`/`resolve` next to an
// issue reference, anywhere in the PR body AND in every commit message a squash
// aggregates — blockquotes, caveats, and sentences arguing against closing
// included. So #596's pin above (`Closes #N` / `Part of #N` / `squash merge`)
// could stay green while the rider that actually catches this case was
// refactored away: those three tokens survive a template that has lost it. That
// is the hole these two tests close.
//
// Pinned on the LIVE templates for the reason `:1210-1212` gives — the pre222
// golden fails as "re-bless me" and names no rule. Split by role because the two
// halves are performed by different agents: the worker writes the body, and only
// the orchestrator ever performs a merge.

/// The authoring side. Concepts, not sentences: the mechanism's name (the coined
/// term does the work `deadlock` does in #590's pin above — it is what a reader
/// has to have met to predict the failure), the channel a worker would otherwise
/// never think to check (the commit messages a squash aggregates, not just the
/// body it is looking at), and the instruction as a COMMAND rather than an
/// exhortation — `grep`, over `git log`, before posting. A version missing any of
/// these is telling a worker to be careful, which #615's author already was.
#[test]
fn worker_template_says_the_closing_keyword_scan_is_context_blind() {
    let lower = worker_contract_text().to_lowercase();
    for concept in ["context-blind", "commit message", "grep", "git log"] {
        assert!(
            lower.contains(concept),
            "worker.md no longer names `{concept}` — picking `Part of #N` is not enough on its \
             own, and #615 proves it: the scan reads prose that ARGUES AGAINST closing, so the \
             rule has to be grep-before-posting across both channels, not keyword choice (#625)"
        );
    }
}

/// The merge side, where the ordering is itself the assertion. `scrub` names the
/// check that runs BEFORE the squash, and what the template says about reopening
/// has to come after it, because that is the half that runs when the scrub missed
/// something — a fallible check needs its own backstop (the *bounded suppression*
/// lesson, from the other end).
///
/// `reopen` is deliberately NOT pinned on its own: it appears elsewhere in this
/// template for unrelated reasons (the red-main revert, the re-sync sweep), so a
/// bare `contains` for it would pass against a template that never gained this
/// section at all — a green assertion proving nothing, which is the failure mode
/// `:1210-1212` is about.
#[test]
fn orchestrator_template_scrubs_the_squashed_message_and_rechecks_after() {
    // #1683 moved the squash procedure into the playbook; the pin follows its
    // specimen, never relaxes (a test specimen must stay a member of the
    // class it witnesses).
    let lower = ORCHESTRATOR_PLAYBOOK_TPL.to_lowercase();
    for concept in ["squash", "context-blind", "scrub"] {
        assert!(
            lower.contains(concept),
            "the playbook no longer names `{concept}` — the aggregated squash message is the \
             text GitHub reads its keywords out of, and the only agent that can read it before \
             the merge is the one performing the merge (#625; procedure moved to the playbook \
             in #1683)"
        );
    }
    let after_scrub = &lower[lower.find("scrub").expect("asserted present above")..];
    assert!(
        after_scrub.contains("reopen"),
        "the playbook scrubs the aggregated message before merging but no longer says to \
         reopen what closed anyway — the scrub is a fallible check with nothing behind it, and \
         #569 needed that backstop twice in one session (#625)"
    );
}

// ---------- #524 / #455: identity durability ----------
//
// Two halves of one property, which is why they share a section: an agent id
// that is never re-minted is what makes the kickoff's delivery id (#455) unique
// and durable without persisting anything else.
//
// Nothing below names a function this PR introduced. The restart tests go
// through `spawn_agent` and the delivery-id tests build the expected token as a
// literal `format!` — the same independence argument `tests/fixtures/pre222`
// makes about the golden templates: an expectation derived from the code's own
// helper moves with it and pins nothing. It also means this exact test source
// compiles and runs against the base commit, which is what made the red-before-
// green evidence a behavior failure rather than a build failure.

#[test]
fn agent_ids_are_never_re_minted_after_a_restart() {
    // #524: the ids were minted from an in-memory counter that restarted at 0
    // with the process, so the first worker of every launch was `w-1` — and
    // every persisted artifact keyed by that id (a board `assignee`, an audit
    // row, an `agent/w-1` branch left in the human's repo) silently belonged to
    // someone else. #227 hit exactly that with a stale `agent/rev-8`.
    let dir = tempfile::tempdir().unwrap();
    let before: Vec<String> = {
        let reg = relaunch_registry(dir.path());
        let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
        vec![
            reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap().id,
            reg.spawn_agent(&g.id, Role::Reviewer, "r", "t", false, None).unwrap().id,
        ]
    };

    // ---- the restart: a brand-new registry over the same state root ----
    let reg = relaunch_registry(dir.path());
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let after = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap().id;

    assert!(
        !before.contains(&after),
        "a restart re-minted {after:?}, an id the previous run already handed out ({before:?}) \
         — every artifact still referencing it now points at the wrong agent (#524)"
    );
    assert!(
        id_suffix(&after) > before.iter().map(|id| id_suffix(id)).max().unwrap(),
        "the counter must RESUME above the previous run's high-water mark, not merely differ: \
         {after:?} against {before:?}"
    );
}

#[test]
fn a_missing_counter_file_reseeds_from_the_durable_roster() {
    // The install that already exists. Nothing wrote `agent-seq.json` before
    // this change, so on the first launch after upgrading the only record of
    // which ids are spent is `agents.json` — the durable roster loomux has
    // always kept. Without this fallback the protection would start working
    // only AFTER the very collision it exists to prevent.
    let dir = tempfile::tempdir().unwrap();
    // Two, not more: `rails()` caps a group at `max_agents: 2` and a third
    // spawn is refused by the guardrail, which would fail this test on an
    // unwrap instead of on the property it is about (caught by the red run —
    // the first cut asked for three).
    let before = ids_from_a_previous_run(dir.path(), 2);
    // Removing a file that does not exist is the same no-op on either side of
    // this change, so the two runs differ only in whether ids repeat.
    let _ = std::fs::remove_file(dir.path().join("agent-seq.json"));
    assert!(
        !dir.path().join("agent-seq.json").exists(),
        "the fixture must really have taken the counter file away"
    );

    let after = id_after_restart(dir.path());
    assert!(
        !before.contains(&after),
        "with the counter file gone the roster is the only durable record of spent ids, and \
         {after:?} collides with {before:?} (#524)"
    );
}

/// The numeric tail of an agent id, re-derived here rather than imported: see
/// this section's header for why no test below names a function the change
/// introduced.
fn id_suffix(id: &str) -> u32 {
    id.rsplit_once('-').and_then(|(_, n)| n.parse().ok()).unwrap_or_else(|| {
        panic!("{id:?} is not a minted `<prefix>-<seq>` agent id")
    })
}

/// Spawn `n` workers into a fresh group on `dir` and return their ids, letting
/// the registry drop — the "a previous run happened here" fixture the durability
/// tests below all start from. Capped by `rails()`'s `max_agents: 2`.
fn ids_from_a_previous_run(dir: &Path, n: usize) -> Vec<String> {
    let reg = relaunch_registry(dir);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    (0..n).map(|_| reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap().id).collect()
}

/// Restart over `dir` and mint one more id.
fn id_after_restart(dir: &Path) -> String {
    let reg = relaunch_registry(dir);
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap().id
}

#[test]
fn a_counter_file_behind_the_roster_heals_instead_of_reissuing() {
    // rev-13's blocking finding on #604, and the case the first cut got wrong.
    //
    // `mint_agent_seq` audits a failed persist and deliberately does NOT
    // propagate it — so the id it just returned goes on to be worktreed,
    // rostered and audited while the mark that reserved it never reached disk.
    // The file then parses to a value BELOW the roster. Reading the file as
    // authoritative whenever it parses takes that lie at face value, and the
    // next restart reissues a live id: #524's own bug, through #524's own
    // degraded path.
    //
    // Not a tail risk on this repo. `atomic_write` fails on disk-full — its
    // own `sync_all()` comment calls that "the disk-full guard" — and there
    // are three recorded disk-exhaustion incidents here (#134, #320, #488),
    // one of which crashed loomux itself. "Failed write, then a restart" is
    // close to the modal way this fires.
    let dir = tempfile::tempdir().unwrap();
    let spent = ids_from_a_previous_run(dir.path(), 2);

    // Exactly the on-disk state a mint whose write failed leaves behind: the
    // roster records both agents, the counter file is a valid JSON document
    // that lags one behind.
    let stale = format!(r#"{{"high_water":{},"updated_ms":1}}"#, id_suffix(&spent[0]));
    std::fs::write(dir.path().join("agent-seq.json"), stale).unwrap();

    let after = id_after_restart(dir.path());
    assert!(
        !spent.contains(&after),
        "a counter file behind the roster must heal UP against it, not be believed: minted \
         {after:?} again after {spent:?} (rev-13 blocking, #524)"
    );
}

#[test]
fn a_counter_at_the_ceiling_saturates_instead_of_wrapping_to_one() {
    // rev-13 N1. A stored `high_water` at `u32::MAX` is the one input where
    // the parse-side clamp does not fail in the safe direction: `fetch_add`
    // wrapped the atomic to 0 and reissued from `w-1`, colliding with every
    // live id at once — and with no `overflow-checks` in `[profile.release]`
    // the shipped binary did it silently where `cargo test` would panic.
    //
    // Saturating is still a broken state, but it is broken in the direction
    // that reuses nothing, and `agent-seq-exhausted` says so in the audit.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("agent-seq.json"),
        format!(r#"{{"high_water":{},"updated_ms":1}}"#, u32::MAX),
    )
    .unwrap();

    let id = id_after_restart(dir.path());
    assert_ne!(
        id, "w-1",
        "a counter at the ceiling must never wrap to zero and reissue from the start — that \
         collides with every live id at once (rev-13 N1)"
    );
    assert_eq!(id, format!("w-{}", u32::MAX), "it should pin at the ceiling instead");
}

#[test]
fn an_unusable_counter_file_falls_back_to_the_roster() {
    // rev-13 N3: the corrupt-file paths were correct but asserted only by
    // inspection. Stated plainly, since it bears on how this evidence reads —
    // **these cases pass on the parent commit too.** Their behavior does not
    // change in this round; they are the coverage the blocking finding was
    // hiding in, and they now also pin that the `unwrap_or(0)` half of the new
    // max cannot swallow the roster when the file is unusable.
    for (label, body) in [
        ("empty", ""),
        ("not json at all", "high_water = 9"),
        ("wrong value type", r#"{"high_water":"five"}"#),
        ("negative", r#"{"high_water":-3}"#),
        ("field missing", r#"{"updated_ms":1}"#),
        ("truncated mid-write", r#"{"high_wat"#),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let spent = ids_from_a_previous_run(dir.path(), 2);
        std::fs::write(dir.path().join("agent-seq.json"), body).unwrap();

        let after = id_after_restart(dir.path());
        assert!(
            !spent.contains(&after),
            "an unusable counter file ({label}) must fall through to the roster, not read as \
             zero and reissue: minted {after:?} again after {spent:?}"
        );
    }
}

#[test]
fn every_kickoff_carries_a_delivery_id_that_is_stable_across_rebuilds() {
    // #455: nothing stamped a delivery, so a receiver had no way to recognise
    // "I have already acted on this one" — the mitigation the issue asks for
    // even before the CLI-side root cause is known.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "Fix #7", false, None).unwrap();
    let o = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();

    let k = reg.kickoff_prompt(&w, &g, "note", None);
    assert!(
        k.contains(&format!("Delivery id: {}/{}/k1", g.id, w.id)),
        "a delegate kickoff must carry its delivery id: {k}"
    );
    assert!(
        k.contains("ALREADY ACTED ON"),
        "the header must carry the rule's operative test, not just an opaque token — an agent \
         whose instructions file failed to read still has to be able to act on it: {k}"
    );

    // Every kickoff, not three roles out of four: an orchestrator that
    // re-processes its own kickoff re-runs a whole session start.
    let ok = reg.kickoff_prompt(&o, &g, "", None);
    assert!(
        ok.contains(&format!("Delivery id: {}/{}/k1", g.id, o.id)),
        "an orchestrator kickoff must carry one too: {ok}"
    );

    // THE property #517/#585's re-delivery depends on. That recovery re-admits
    // the brief BYTE-IDENTICALLY, and `queue::admit`'s byte-identical coalesce
    // is one of its duplicate-protection layers — an id minted per paste would
    // break the coalesce and hand the receiver a fresh id for work it may
    // already have done.
    assert_eq!(
        k,
        reg.kickoff_prompt(&w, &g, "note", None),
        "a kickoff's delivery id must be a property of the delivery, not minted per build"
    );
    assert_ne!(
        reg.kickoff_prompt(&w, &g, "note", None),
        ok,
        "two agents must not share a delivery id"
    );
}

#[test]
fn the_kickoff_header_points_at_a_section_every_template_actually_has() {
    // A header that names a section the reader's own instructions file does not
    // contain is worse than no header: it tells the agent a rule exists and
    // then hides it.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let k = reg.kickoff_prompt(&w, &g, "note", None);
    assert!(k.contains("Duplicate deliveries"), "the header must name the section: {k}");
    for (role, tpl) in
        [("orchestrator", ORCHESTRATOR_TPL), ("worker", WORKER_TPL), ("reviewer", REVIEWER_TPL),
         ("planner", PLANNER_TPL)]
    {
        assert!(
            tpl.contains("## Duplicate deliveries"),
            "{role}.md has no `## Duplicate deliveries` section for the kickoff header to point at"
        );
    }
}

/// The rule itself, pinned on the LIVE templates rather than only in the
/// `pre222` golden — which fails on any template edit at all, as *"re-bless
/// me"*, and so teaches nobody which rule went missing (#594's placement
/// argument, same file).
///
/// Three concepts, not sentences. The prose may be rewritten; a version missing
/// any of these has stopped stating the rule:
///
/// - the token exists and is called a **delivery id**;
/// - the test is **already acted on** — not "already seen", which would make an
///   agent no-op a brief it never got to act on;
/// - a **re-delivery is not a duplicate**, which is what keeps this composable
///   with #517/#585's deliberate re-send of a lost kickoff.
#[test]
fn every_role_template_distinguishes_a_duplicate_paste_from_a_re_delivery() {
    for (role, tpl) in
        [("orchestrator", ORCHESTRATOR_TPL), ("worker", WORKER_TPL), ("reviewer", REVIEWER_TPL),
         ("planner", PLANNER_TPL)]
    {
        let lower = tpl.to_lowercase();
        for concept in ["delivery id", "already acted on", "re-delivery is not a duplicate"] {
            assert!(
                lower.contains(concept),
                "{role}.md no longer says {concept:?} — without all three the rule either has no \
                 token to key on, keys on having SEEN the bytes (so a lost kickoff's re-delivery \
                 gets dropped as a duplicate), or forbids the re-delivery outright (#455/#585)"
            );
        }
    }
}

// ---------- #445: delivery queue — registry-level bookkeeping ----------
//
// `deliver_now`'s pipeline and the drainer thread cannot be exercised here
// (no live pty/app handle in test mode, and CLAUDE.md forbids spawning a
// real agent CLI) — see `queue.rs`'s module doc and the plan's own
// "Stated honestly" section. What IS testable, and pinned below: the FIFO
// admit/cap/coalesce bookkeeping, the front-door check (bypasses the
// app-handle requirement precisely because it needs no live pane), the
// dequeue/drop audit trail, and the orphan-scan derivation. Live wiring
// (drainer actually flushing a real pane) is a hand-validation item.

#[test]
fn enqueue_text_preserves_fifo_order_across_mixed_senders() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 101u32;

    reg.enqueue_text(&g.id, &w.id, "orch-1", "here is context", pty, queue::EnqueueReason::Question).unwrap();
    reg.enqueue_text(&g.id, &w.id, "orch-2", "now go", pty, queue::EnqueueReason::BehindQueue).unwrap();
    reg.enqueue_text(&g.id, &w.id, "orch-1", "a third, different ask", pty, queue::EnqueueReason::BehindQueue).unwrap();

    let snap = reg.queue_snapshot(pty);
    assert_eq!(snap.len(), 3);
    // "here is context" must never be overtaken by "now go" — the exact
    // inversion failure mode the front-door check exists to prevent.
    assert_eq!(snap[0].payload.text(), Some("here is context"));
    assert_eq!(snap[0].from, "orch-1");
    assert_eq!(snap[1].payload.text(), Some("now go"));
    assert_eq!(snap[2].payload.text(), Some("a third, different ask"));
    // Ids are strictly increasing (the monotonic AtomicU64 — no getrandom).
    assert!(snap[0].id < snap[1].id, "ids must increase monotonically");
    assert!(snap[1].id < snap[2].id, "ids must increase monotonically");
}

#[test]
fn enqueue_text_was_first_is_true_only_for_the_admission_that_finds_an_empty_queue() {
    // #470 review N2: pin `AdmitOutcome::was_first` directly by name —
    // it's the ONE fact `deliver_prompt`'s front door hangs the whole
    // unified-admission fix on (whoever observes an empty queue owns
    // spawning the drainer; everyone else best-effort nudges an existing
    // one). Previously only pinned indirectly through front-door tests.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 112u32;

    let first = reg.enqueue_text(&g.id, &w.id, "orrerix", "one", pty, queue::EnqueueReason::Arrival).unwrap();
    assert!(first.was_first, "the first admission to an empty queue must observe was_first: true");

    let second = reg.enqueue_text(&g.id, &w.id, "orrerix", "two", pty, queue::EnqueueReason::Arrival).unwrap();
    assert!(!second.was_first, "landing behind an existing entry must never report was_first: true");
    assert!(second.id > first.id, "ids still increase monotonically regardless of was_first");

    let coalesced = reg.enqueue_text(&g.id, &w.id, "orrerix", "one", pty, queue::EnqueueReason::Arrival).unwrap();
    assert!(!coalesced.was_first, "a coalesce match must never report was_first: true");
    assert_eq!(coalesced.id, first.id, "a coalesce reports the id of the entry it merged into");
    assert_eq!(reg.queue_depth(pty), 2, "the coalesced duplicate must not grow the queue");
}

#[test]
fn enqueue_text_rejects_newest_at_cap_never_evicts_oldest() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 102u32;

    for i in 0..queue::QUEUE_MAX_PER_PANE {
        reg.enqueue_text(&g.id, &w.id, "orrerix", &format!("distinct-{i}"), pty, queue::EnqueueReason::BehindQueue)
            .unwrap();
    }
    assert_eq!(reg.queue_depth(pty), queue::QUEUE_MAX_PER_PANE);

    let err = reg
        .enqueue_text(&g.id, &w.id, "orrerix", "one-too-many", pty, queue::EnqueueReason::Question)
        .unwrap_err();
    assert!(err.contains("NOT queued"), "must be a synchronous, truthful rejection: {err}");
    assert_eq!(reg.queue_depth(pty), queue::QUEUE_MAX_PER_PANE, "depth must not change on rejection");
    // The head is still the FIRST item ever queued — never evicted to make
    // room, because the head may be the kickoff everything after depends on.
    assert_eq!(reg.queue_snapshot(pty)[0].payload.text(), Some("distinct-0"));

    let dropped = reg
        .audit_log(&g.id)
        .into_iter()
        .filter(|e| e.action == "delivery-dropped" && e.detail["reason"] == json!("queue-full-at-call"))
        .count();
    assert_eq!(dropped, 1, "the rejection itself must be audited");
}

#[test]
fn enqueue_text_coalesces_a_byte_identical_repeat_and_bumps_the_counter() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 103u32;

    reg.enqueue_text(&g.id, &w.id, "orrerix", "give me a status update", pty, queue::EnqueueReason::Question).unwrap();
    reg.enqueue_text(&g.id, &w.id, "orrerix", "give me a status update", pty, queue::EnqueueReason::BehindQueue).unwrap();
    reg.enqueue_text(&g.id, &w.id, "orrerix", "give me a status update", pty, queue::EnqueueReason::BehindQueue).unwrap();

    assert_eq!(reg.queue_depth(pty), 1, "byte-identical repeats must collapse, not accumulate");
    assert_eq!(reg.queue_snapshot(pty)[0].coalesced, 2, "two duplicates coalesced into the original");

    // A DIFFERENT ask must still be admitted — coalescing never guesses at
    // semantic staleness, only exact-byte repeats.
    reg.enqueue_text(&g.id, &w.id, "orrerix", "give me a status update now", pty, queue::EnqueueReason::BehindQueue).unwrap();
    assert_eq!(reg.queue_depth(pty), 2);

    let coalesced_audits =
        reg.audit_log(&g.id).into_iter().filter(|e| e.action == "delivery-coalesced").count();
    assert_eq!(coalesced_audits, 2, "each coalesce must be individually audited");
}

#[test]
fn enqueue_stranded_front_lands_ahead_of_already_queued_text() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 104u32;

    reg.enqueue_text(&g.id, &w.id, "orrerix", "queued behind", pty, queue::EnqueueReason::BehindQueue).unwrap();
    reg.enqueue_stranded_front(&g.id, &w.id, "orrerix", pty, queue::EnqueueReason::Question)
        .unwrap();

    let snap = reg.queue_snapshot(pty);
    assert_eq!(snap.len(), 2);
    assert_eq!(snap[0].payload, queue::QueuedPayload::StrandedSubmit, "the marker must drain FIRST");
    assert_eq!(snap[1].payload.text(), Some("queued behind"));
}

#[test]
fn enqueue_stranded_front_respects_the_same_cap() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 105u32;

    for i in 0..queue::QUEUE_MAX_PER_PANE {
        reg.enqueue_text(&g.id, &w.id, "orrerix", &format!("d-{i}"), pty, queue::EnqueueReason::BehindQueue).unwrap();
    }
    let err = reg
        .enqueue_stranded_front(&g.id, &w.id, "orrerix", pty, queue::EnqueueReason::Question)
        .unwrap_err();
    assert!(err.contains("NOT queued"), "got: {err}");
}

#[test]
fn pop_front_dequeued_removes_only_a_matching_front_id_and_audits_queued_ms() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 106u32;

    reg.enqueue_text(&g.id, &w.id, "orrerix", "first", pty, queue::EnqueueReason::Question).unwrap();
    reg.enqueue_text(&g.id, &w.id, "orrerix", "second", pty, queue::EnqueueReason::BehindQueue).unwrap();
    let first_id = reg.queue_snapshot(pty)[0].id;
    let first_enqueued_ms = reg.queue_snapshot(pty)[0].enqueued_ms;

    // A stale/mismatched id must NOT pop the front (single-owner discipline
    // — see `run_queue_drainer`'s doc).
    reg.pop_front_dequeued(&g.id, pty, first_id + 999, first_enqueued_ms);
    assert_eq!(reg.queue_depth(pty), 2, "a non-matching id must not pop anything");

    reg.pop_front_dequeued(&g.id, pty, first_id, first_enqueued_ms);
    assert_eq!(reg.queue_depth(pty), 1);
    assert_eq!(reg.queue_snapshot(pty)[0].payload.text(), Some("second"));

    let dequeued = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "delivery-dequeued" && e.detail["id"] == json!(first_id))
        .expect("a real pop must be audited");
    assert!(dequeued.detail["queued_ms"].as_u64().is_some(), "must record how long it waited");
}
