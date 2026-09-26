//! Declined flushes, named lock resources and what counts as a question or a composer.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- #824: a declined flush is not permission to paste ----------
//
// `flush_stranded_text` is the mechanism that clears a previous delivery's
// stranded text before this one pastes, and it can DECLINE — a human typed
// since our submit, a question is live, human characters are outstanding.
// `deliver_now` ignored that and carried on to the paste.
//
// Nothing between the flush and the paste can catch it, and the reason is
// structural rather than an oversight: the pre-paste guard is
// `wait_for_box_clear` → `PtyManager::input_pending` → `input_box_len`, a
// counter written by `note_user_input` and by nothing else — and
// `note_user_input` is reached from exactly ONE place, `write_from_frontend`.
// Orchestration's own typing goes out through `write_bytes`, which touches no
// counter at all. So `!input_pending` means "no HUMAN characters outstanding",
// never "the box is empty", and loomux's own pasted prompt is invisible to
// every guard on the path.
//
// The incident shape is ordinary: a delivery strands, the human presses Enter
// (or types a character and backspaces it out), the flush declines on the human
// block, `input_pending` reads false, and the next delivery pastes ON TOP of
// the stranded prompt — which the pre-Enter quiet wait then submits as one
// merged prompt. That is the #81/#84/#111 collision, reached through the
// flush's own decline.
//
// The guard is gated on a POSITIVE `Holds` and nothing weaker. `Unverifiable`
// keeps today's behaviour on purpose (common near the Tier 1 scan cap per
// #583/#685 — aborting on it would trade a rare merge for a routine stall), and
// `NotHolding` is precisely when pasting is safe.

#[test]
fn a_declined_flush_over_our_own_stranded_text_aborts_instead_of_pasting() {
    // THE incident cell. The flush declined, the ledger says the previous
    // delivery is unconfirmed, and Tier 1 can see our text still in the box.
    assert_eq!(
        stranded_paste_guard(Some(false), false, Some(BoxReading::Holds)),
        StrandedPasteGuard::AbortStranded
    );
}

#[test]
fn a_flush_that_actually_pressed_never_blocks_the_paste() {
    // `flushed` is the box having been cleared by our OWN Enter, so there is
    // nothing left to collide with — and the reading is stale by construction
    // (it describes the pane before the press). Checked first for that reason.
    assert_eq!(
        stranded_paste_guard(Some(false), true, Some(BoxReading::Holds)),
        StrandedPasteGuard::Paste,
        "a successful flush is the box being cleared; blocking after it would deadlock every \
         delivery into a pane that had ever stranded one"
    );
}

#[test]
fn only_a_positive_holds_reading_aborts_the_paste() {
    // The blast-radius bound, stated as a matrix. `Unverifiable` is the one that
    // matters commercially: it is routine near the scan cap, and aborting on it
    // would convert a rare merge into a routine stall on ordinary traffic.
    for reading in [Some(BoxReading::NotHolding), Some(BoxReading::Unverifiable), None] {
        assert_eq!(
            stranded_paste_guard(Some(false), false, reading),
            StrandedPasteGuard::Paste,
            "reading {reading:?} is not evidence of a collision, so it must not abort"
        );
    }
}

#[test]
fn a_pane_with_nothing_stranded_is_untouched_by_this_guard() {
    // The overwhelming majority of deliveries. `Some(true)` is a pane whose last
    // delivery confirmed; `None` is a pane with no ledger entry at all. Neither
    // may pay anything here, whatever the tail happens to look like.
    for prev in [Some(true), None] {
        assert_eq!(
            stranded_paste_guard(prev, false, Some(BoxReading::Holds)),
            StrandedPasteGuard::Paste,
            "prev_confirmed={prev:?} has nothing stranded, so a Holds reading is about \
             somebody else's text"
        );
    }
}

#[test]
fn the_wired_declined_flush_leaves_the_pane_untouched_when_our_text_is_still_there() {
    // The WIRED half, driving the real `flush_stranded_text` against a real
    // `PtyManager` — the function `deliver_now` actually calls. A pure test
    // cannot see the part that made this reachable: that the human's Enter
    // stamps the keystroke clock AND empties the occupancy counter in one
    // write, so the flush declines on the human block while `input_pending`
    // simultaneously reads clear. Only the real `note_user_input` does both.
    let pm = PtyManager::default();
    let pty = 8241u32;
    let captured = pm.register_fake_for_test(pty, STRANDED_TAIL.as_bytes());
    let last_delivery: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
    record_stranded_outcome_at_for_test(
        &last_delivery,
        pty,
        "orrerix".to_string(),
        now_ms() - 60_000,
        Some(STRANDED_PROMPT.to_string()),
    );

    // The human touches the pane: Enter classifies `Submit`, which stamps the
    // clock and zeroes the counter in the same write.
    pm.note_user_input(pty, "\r", true);
    assert_eq!(
        pm.input_pending(pty),
        Some(false),
        "precondition: the ONLY box guard on this path now reads clear — which is the defect"
    );

    let human_block = human_input_block(
        pm.last_user_input_ms(pty).unwrap_or(0),
        now_ms() - 60_000,
        pm.input_pending(pty).unwrap_or(true),
        now_ms(),
        HUMAN_INPUT_BLOCK_BOUND_MS,
    );
    assert_eq!(human_block, HumanInputBlock::Blocked, "precondition: the human-block route");

    let flushed = flush_stranded_text(
        &pm,
        pty,
        recorded_confirmed(&last_delivery, pty),
        human_block.holds(),
        b"\r",
        Vec::new(),
    );
    assert!(!flushed, "precondition: the flush declines, and pre-#824 that was the end of it");
    assert!(
        captured.lock().unwrap().is_empty(),
        "a declining flush writes nothing, so the stranded text is still sitting there"
    );

    // Tier 1 on our own recorded text — the reading `deliver_now` now takes.
    let mut scan = Tier1Scan::for_paste(STRANDED_PROMPT);
    let read = scan.read(|n| pm.output_tail_bounded(pty, n));
    let reading = box_reading(read.as_ref().map(|r| r.stripped.as_str()), STRANDED_PROMPT);
    assert_eq!(reading, BoxReading::Holds, "precondition: the pane still shows our prompt");

    assert_eq!(
        stranded_paste_guard(recorded_confirmed(&last_delivery, pty), flushed, Some(reading)),
        StrandedPasteGuard::AbortStranded,
        "so the delivery must abort pre-paste — pasting here merges two prompts into one (#824)"
    );
}

// ── named lock resources (#858) ────────────────────────────────────────────
//
// The engine's own state machine is unit-tested in
// `crates/loomux-engine/src/locks.rs` (#888 A2 batch 5 moved it there; it is
// still reachable as `orchestration::locks`); these drive the WIRED path — the real
// `dispatch()`, the real registry, the real audit log — because that is where
// the parts that can silently do nothing live: the tool listing, the role
// gate, the pane notice, the `mark_dead` reclaim, and the sweep folded into
// the poll tick.

/// A repo whose `.loomux/workflow.yml` declares `resources:`.
fn repo_with_resources(tag: &str, body: &str) -> std::path::PathBuf {
    let repo = scratch_dir(tag);
    fs::create_dir_all(repo.join(".loomux")).unwrap();
    fs::write(
        repo.join(".loomux").join("workflow.yml"),
        format!(
            "version: {}\n\
             blocks:\n  - id: w\n    name: Worker\n    kind: worker\n    cli: claude\n    model: sonnet\
             \n  - id: p\n    name: Planner\n    kind: planner\n    cli: claude\n    model: sonnet\n\
             {body}",
            workflow::SCHEMA_VERSION
        ),
    )
    .unwrap();
    // The fixture asserts its own validity — a file that never parsed would
    // otherwise fail the caller's assertion as "the feature is broken".
    let loaded = workflow::load_workflow(repo.to_str().unwrap());
    assert!(
        matches!(&loaded, Ok(Some(wf)) if !wf.resources.is_empty()),
        "fixture must parse with a non-empty resources block, got {loaded:?}"
    );
    repo
}

/// A group on a repo declaring `body`, plus two worker callers.
fn setup_locks(tag: &str, body: &str) -> (OrchRegistry, tempfile::TempDir, GroupId, Caller, Caller) {
    let (reg, dir) = test_registry();
    let repo = repo_with_resources(tag, body);
    let g = reg.create_group(repo.to_str().unwrap(), lock_rails()).unwrap();
    let w1 = reg.spawn_agent(&g.id, Role::Worker, "w1", "task", false, None).unwrap();
    let w2 = reg.spawn_agent(&g.id, Role::Worker, "w2", "task", false, None).unwrap();
    let c1 = reg.resolve_token(&w1.token).unwrap();
    let c2 = reg.resolve_token(&w2.token).unwrap();
    (reg, dir, g.id, c1, c2)
}

fn lock_rails() -> Guardrails {
    Guardrails { max_agents: 4, ..advanced_rails() }
}

const BUILD_ONE_SLOT: &str = "resources:\n  build:\n    slots: 1\n    max_hold_minutes: 45\n";
const BUILD_ONE_MINUTE: &str = "resources:\n  build:\n    slots: 1\n    max_hold_minutes: 1\n";

fn lock_call(reg: &OrchRegistry, c: &Caller, name: &str, args: Value) -> (bool, String) {
    let r = dispatch(reg, c, "tools/call", &json!({ "name": name, "arguments": args })).unwrap();
    (
        r["isError"].as_bool().unwrap_or(false),
        r["content"][0]["text"].as_str().unwrap_or_default().to_string(),
    )
}

fn lock_tool_names(reg: &OrchRegistry, c: &Caller) -> Vec<String> {
    dispatch(reg, c, "tools/list", &Value::Null).unwrap()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

fn lock_audit_actions(reg: &OrchRegistry, group: &GroupId) -> Vec<String> {
    reg.audit_log(group).into_iter().map(|e| e.action).collect()
}

fn lock_json(reg: &OrchRegistry, c: &Caller) -> Value {
    let (err, text) = lock_call(reg, c, "list_locks", json!({}));
    assert!(!err, "list_locks must not fail: {text}");
    serde_json::from_str(&text).expect("list_locks returns JSON")
}

/// **A repo that declares nothing gets no lock tools at all** — the "an absent
/// block means the feature is off" posture, enforced in the listing rather
/// than only in a refusal, so the three descriptions cost no context in every
/// group that never asked for them.
#[test]
fn the_lock_tools_are_listed_only_where_a_repo_declares_resources() {
    let (reg, _d, _g, c1, _c2) = setup_locks("locks-listing", BUILD_ONE_SLOT);
    let names = lock_tool_names(&reg, &c1);
    for t in ["acquire_lock", "release_lock", "list_locks"] {
        assert!(names.contains(&t.to_string()), "declared repo must offer {t}: {names:?}");
    }
    // The declared menu reaches the agent IN the description — an agent that
    // cannot see what exists guesses names and collects refusals.
    let listed = dispatch(&reg, &c1, "tools/list", &Value::Null).unwrap();
    let desc = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "acquire_lock")
        .expect("acquire_lock is listed")["description"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(desc.contains("'build' (1 slot, max hold 45 min)"), "{desc}");

    // …and a group whose repo declares none sees none of the three.
    let (reg2, _d2, co, cw) = setup_mcp();
    for c in [&co, &cw] {
        let names = lock_tool_names(&reg2, c);
        for t in ["acquire_lock", "release_lock", "list_locks"] {
            assert!(!names.contains(&t.to_string()), "undeclared repo must not offer {t}");
        }
    }
}

/// The contention path end to end: the second caller is QUEUED with its
/// position, and the call RETURNS — it is not an error and it does not block.
#[test]
fn a_second_worker_is_queued_with_its_position_rather_than_blocked_or_refused() {
    let (reg, _d, g, c1, c2) = setup_locks("locks-contend", BUILD_ONE_SLOT);

    let (err, text) =
        lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build", "note": "cargo test" }));
    assert!(!err, "{text}");
    assert!(text.contains("is YOURS"), "{text}");

    let (err, text) = lock_call(&reg, &c2, "acquire_lock", json!({ "name": "build" }));
    assert!(!err, "contention is not an error: {text}");
    assert!(text.contains("position 1"), "{text}");
    // The reply has to talk the caller out of the deadlock shape (#590): a
    // worker told only "queued" invents a sleep loop, and the grant notice is
    // typed into the very pane that loop would be blocking.
    assert!(text.contains("END YOUR TURN"), "{text}");

    let state = lock_json(&reg, &c1);
    assert_eq!(state["resources"][0]["holders"][0]["agent"], json!(c1.agent_id));
    assert_eq!(state["resources"][0]["holders"][0]["note"], json!("cargo test"));
    assert_eq!(state["resources"][0]["queue"][0]["agent"], json!(c2.agent_id));

    let actions = lock_audit_actions(&reg, &g);
    assert!(actions.contains(&"lock-acquire".to_string()), "{actions:?}");
    assert!(actions.contains(&"lock-queued".to_string()), "{actions:?}");
}

/// Releasing hands the slot to the head of the queue **and tells that agent in
/// its own pane** — the half a state-only test would miss entirely, and the
/// only thing that makes a non-blocking acquire usable at all.
#[test]
fn releasing_grants_the_slot_to_the_head_of_the_queue_and_notifies_it() {
    let (reg, _d, g, c1, c2) = setup_locks("locks-handoff", BUILD_ONE_SLOT);
    lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build" }));
    lock_call(&reg, &c2, "acquire_lock", json!({ "name": "build" }));
    // A pane to deliver into, and a pause so the text is observable without a
    // real terminal (this suite's standard delivery mock).
    pause_with_pane(&reg, &g, &c2.agent_id, 4201);

    let (err, text) = lock_call(&reg, &c1, "release_lock", json!({ "name": "build" }));
    assert!(!err, "{text}");
    assert!(text.contains(&c2.agent_id), "the releaser is told who took it: {text}");

    let notices = delivered_texts(&reg, &g);
    assert!(
        notices.iter().any(|t| t.contains("[orrerix] lock 'build' is yours")),
        "the new holder must be told, in its own pane: {notices:?}"
    );
    let state = lock_json(&reg, &c2);
    assert_eq!(state["resources"][0]["holders"][0]["agent"], json!(c2.agent_id));
    assert!(state["resources"][0]["queue"].as_array().unwrap().is_empty());
}

/// A holder whose pane dies must not strand the slot. This is the reclaim that
/// matters most: nothing else will ever release it.
#[test]
fn a_dead_holders_slot_is_reclaimed_and_handed_on_immediately() {
    let (reg, _d, g, c1, c2) = setup_locks("locks-death", BUILD_ONE_SLOT);
    lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build" }));
    lock_call(&reg, &c2, "acquire_lock", json!({ "name": "build" }));

    reg.mark_dead(&c1.agent_id, Some(1));

    let state = lock_json(&reg, &c2);
    assert_eq!(
        state["resources"][0]["holders"][0]["agent"],
        json!(c2.agent_id),
        "the queued worker takes the dead holder's slot, without waiting for the sweep"
    );
    let reclaim = reg
        .audit_log(&g)
        .into_iter()
        .find(|e| e.action == "lock-reclaim")
        .expect("the reclaim is audited");
    assert_eq!(reclaim.detail["agent"], json!(c1.agent_id));
    assert_eq!(reclaim.detail["why"], json!("agent-gone"));
}

/// The max-hold backstop, on the wired sweep: a holder that never releases
/// loses the lock, is told so, and the queue moves.
#[test]
fn an_overrun_hold_is_reclaimed_by_the_sweep_audited_and_reported_to_its_holder() {
    let (reg, _d, g, c1, c2) = setup_locks("locks-expiry", BUILD_ONE_MINUTE);
    lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build" }));
    lock_call(&reg, &c2, "acquire_lock", json!({ "name": "build" }));
    // The ex-holder needs a pane for the "your lock is gone" notice to be
    // observable; the group is resumed again immediately because a paused
    // group's locks are deliberately frozen (see the pause test below).
    pause_with_pane(&reg, &g, &c1.agent_id, 4301);
    reg.resume_group(&g).unwrap();

    // Nothing has expired yet — the sweep must not reclaim early.
    reg.locks_tick(now_ms());
    assert!(
        !lock_audit_actions(&reg, &g).contains(&"lock-expired".to_string()),
        "a hold inside its window is not reclaimed"
    );

    // Two minutes on, past the declared one-minute max hold.
    reg.locks_tick(now_ms() + 2 * 60_000);
    let expired = reg
        .audit_log(&g)
        .into_iter()
        .find(|e| e.action == "lock-expired")
        .expect("the expiry is audited");
    assert_eq!(expired.detail["agent"], json!(c1.agent_id));
    assert!(
        delivered_texts(&reg, &g).iter().any(|t| t.contains("RECLAIMED")),
        "the ex-holder is told its work is no longer serialized"
    );
    let state = lock_json(&reg, &c2);
    assert_eq!(state["resources"][0]["holders"][0]["agent"], json!(c2.agent_id));
}

/// A queued request has its own clock, and giving up is announced rather than
/// silent — otherwise a worker that ended its turn on "you are queued" waits
/// forever for a notice that will never come.
#[test]
fn a_queued_request_times_out_on_its_own_clock_and_says_so() {
    let (reg, _d, g, c1, c2) = setup_locks("locks-waittimeout", BUILD_ONE_SLOT);
    lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build" }));
    // 5 is the floor `clamp_expires_minutes` enforces; anything lower is
    // clamped up to it, so this is the shortest wait a caller can actually ask
    // for.
    lock_call(&reg, &c2, "acquire_lock", json!({ "name": "build", "wait_minutes": 5 }));
    pause_with_pane(&reg, &g, &c2.agent_id, 4401);
    reg.resume_group(&g).unwrap();

    reg.locks_tick(now_ms() + 6 * 60_000);
    let timeout = reg
        .audit_log(&g)
        .into_iter()
        .find(|e| e.action == "lock-wait-timeout")
        .expect("the timeout is audited");
    assert_eq!(timeout.detail["agent"], json!(c2.agent_id));
    assert!(
        delivered_texts(&reg, &g).iter().any(|t| t.contains("wait TIMED OUT")),
        "a dropped waiter is told, never left waiting on a notice that will not come"
    );
    // The holder is untouched: its own 45-minute window is nowhere near.
    let state = lock_json(&reg, &c1);
    assert_eq!(state["resources"][0]["holders"][0]["agent"], json!(c1.agent_id));
    assert!(state["resources"][0]["queue"].as_array().unwrap().is_empty());
}

/// A paused group is frozen solid: a pause is not a reason to take a running
/// build's lock away, and the pause span is credited on the tick that observes
/// it unpaused rather than charged to every hold at once.
#[test]
fn a_pause_does_not_expire_a_hold_the_pause_itself_froze() {
    let (reg, _d, g, c1, _c2) = setup_locks("locks-pause", BUILD_ONE_MINUTE);
    lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build" }));
    let t0 = now_ms();

    // Paused for two minutes — twice the max hold.
    reg.pause_group(&g).unwrap();
    reg.locks_tick(t0);
    reg.locks_tick(t0 + 2 * 60_000);
    assert!(
        !lock_audit_actions(&reg, &g).contains(&"lock-expired".to_string()),
        "a paused group's holds do not expire"
    );

    // The tick that observes it resumed credits the pause span, so the hold
    // survives that tick too rather than expiring the instant work resumes.
    reg.resume_group(&g).unwrap();
    reg.locks_tick(t0 + 2 * 60_000);
    assert!(
        !lock_audit_actions(&reg, &g).contains(&"lock-expired".to_string()),
        "the pause span is credited, not charged"
    );
    // …and the deadline still exists: past the credited window, it goes.
    reg.locks_tick(t0 + 4 * 60_000);
    assert!(
        lock_audit_actions(&reg, &g).contains(&"lock-expired".to_string()),
        "the credit shifts the deadline, it does not remove it"
    );
}

/// Two refusals at the dispatch gate: a lock you never took, and a resource
/// nobody declared.
///
/// The planner refusal used to ride along here and no longer does — it now has
/// its own test covering all three tools, because the plural in this one's
/// former name was a coverage claim it did not make (rev-lead, PR #859
/// finding 1). See `every_lock_tool_refuses_a_planner_and_says_why_in_lock_terms`.
#[test]
fn the_lock_tools_refuse_a_non_holder_and_an_undeclared_name() {
    let (reg, _d, _g, c1, c2) = setup_locks("locks-refusals", BUILD_ONE_SLOT);

    let (err, text) = lock_call(&reg, &c2, "release_lock", json!({ "name": "build" }));
    assert!(err, "releasing a lock you never took must fail: {text}");
    assert!(text.contains("do not hold"), "{text}");

    let (err, text) = lock_call(&reg, &c1, "acquire_lock", json!({ "name": "buld" }));
    assert!(err, "an undeclared resource must be refused, never created: {text}");
    assert!(text.contains("build"), "the refusal names what IS declared: {text}");
}

/// Re-asking is safe in both states — the property a worker relies on after a
/// compact, when it cannot remember whether its own call landed.
#[test]
fn re_asking_never_extends_a_hold_or_costs_a_waiter_its_place() {
    let (reg, _d, _g, c1, c2) = setup_locks("locks-idempotent", BUILD_ONE_SLOT);
    lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build" }));
    let deadline = lock_json(&reg, &c1)["resources"][0]["holders"][0]["expires_ms"].clone();
    assert!(deadline.is_number(), "precondition: a hold carries a deadline");

    let (err, text) = lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build" }));
    assert!(!err, "{text}");
    assert!(text.contains("already hold"), "{text}");
    assert!(text.contains("NOT extended"), "{text}");
    // The reply's WORDS are prose and would survive a refresh untouched; this
    // is the assertion that actually pins it. A deadline that moved on every
    // "still mine" would make max_hold_minutes bound nothing at all.
    assert_eq!(
        lock_json(&reg, &c1)["resources"][0]["holders"][0]["expires_ms"],
        deadline,
        "a re-ask must not move the reclaim deadline"
    );

    lock_call(&reg, &c2, "acquire_lock", json!({ "name": "build" }));
    let (err, text) = lock_call(&reg, &c2, "acquire_lock", json!({ "name": "build" }));
    assert!(!err, "{text}");
    assert!(
        text.contains("position 1"),
        "a re-ask keeps its place, never goes to the back: {text}"
    );

    let state = lock_json(&reg, &c1);
    assert_eq!(state["resources"][0]["queue"].as_array().unwrap().len(), 1, "not queued twice");
}

/// A waiter that no longer needs the resource can leave, instead of being
/// handed a slot it would then sit on for a full max-hold.
#[test]
fn a_waiter_can_withdraw_and_the_next_agent_moves_up() {
    let (reg, _d, g, c1, c2) = setup_locks("locks-withdraw", BUILD_ONE_SLOT);
    let w3 = reg.spawn_agent(&g, Role::Worker, "w3", "task", false, None).unwrap();
    let c3 = reg.resolve_token(&w3.token).unwrap();
    lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build" }));
    lock_call(&reg, &c2, "acquire_lock", json!({ "name": "build" }));
    lock_call(&reg, &c3, "acquire_lock", json!({ "name": "build" }));

    let (err, text) = lock_call(&reg, &c2, "release_lock", json!({ "name": "build" }));
    assert!(!err, "{text}");
    assert!(text.contains("withdrawn"), "{text}");

    let (_, text) = lock_call(&reg, &c1, "release_lock", json!({ "name": "build" }));
    assert!(text.contains(&c3.agent_id), "w3 moves up rather than waiting behind a ghost: {text}");
}

/// Group isolation, on the lock surface: one group's contention is invisible to
/// another's, even on the same resource name.
#[test]
fn locks_are_scoped_to_one_group() {
    let (reg, _d, _g, c1, _c2) = setup_locks("locks-iso-a", BUILD_ONE_SLOT);
    let repo_b = repo_with_resources("locks-iso-b", BUILD_ONE_SLOT);
    let gb = reg.create_group(repo_b.to_str().unwrap(), lock_rails()).unwrap();
    let wb = reg.spawn_agent(&gb.id, Role::Worker, "wb", "task", false, None).unwrap();
    let cb = reg.resolve_token(&wb.token).unwrap();

    lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build" }));
    // Same NAME, different group — and therefore a different lock.
    let (err, text) = lock_call(&reg, &cb, "acquire_lock", json!({ "name": "build" }));
    assert!(!err, "{text}");
    assert!(text.contains("is YOURS"), "another group's hold must not queue this one: {text}");

    let state = lock_json(&reg, &cb);
    let holders = state["resources"][0]["holders"].as_array().unwrap();
    assert_eq!(holders.len(), 1);
    assert_eq!(holders[0]["agent"], json!(cb.agent_id));
}

/// With the advanced orchestrator OFF, `.loomux/workflow.yml` is not the
/// group's config at all — so the resources it declares are not in force, and
/// the lock surface is byte-for-byte what it was before this feature.
#[test]
fn a_non_advanced_group_gets_no_locks_even_from_a_repo_that_declares_them() {
    let (reg, _d) = test_registry();
    let repo = repo_with_resources("locks-basic", BUILD_ONE_SLOT);
    let g = reg.create_group(repo.to_str().unwrap(), rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let c = reg.resolve_token(&w.token).unwrap();

    assert!(!lock_tool_names(&reg, &c).contains(&"acquire_lock".to_string()));
    let (err, text) = lock_call(&reg, &c, "acquire_lock", json!({ "name": "build" }));
    assert!(err, "{text}");
    assert!(text.contains("declares no lock resources"), "{text}");
}

/// A `workflow.yml` mid-save is unparseable for a moment, and the lock config
/// is re-read on every lock call AND on the group view's 2s poll. Treating
/// "cannot read it" as "declares nothing" would drop every live hold in the
/// group — and audit that it had — because an author was halfway through
/// typing a line.
#[test]
fn a_momentarily_unreadable_workflow_file_does_not_drop_a_live_hold() {
    let (reg, _d) = test_registry();
    let repo = repo_with_resources("locks-unreadable", BUILD_ONE_SLOT);
    let g = reg.create_group(repo.to_str().unwrap(), lock_rails()).unwrap();
    let w1 = reg.spawn_agent(&g.id, Role::Worker, "w1", "task", false, None).unwrap();
    let c1 = reg.resolve_token(&w1.token).unwrap();
    lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build" }));

    // Halfway through an edit: valid YAML, invalid workflow.
    fs::write(repo.join(".loomux").join("workflow.yml"), "version: 99\n").unwrap();

    let state = lock_json(&reg, &c1);
    assert_eq!(
        state["resources"][0]["holders"][0]["agent"],
        json!(c1.agent_id),
        "an unreadable config must not revoke a lock somebody is holding"
    );
    assert!(
        !lock_audit_actions(&reg, &g.id).contains(&"lock-undeclared".to_string()),
        "…nor claim in the audit log that the resource was undeclared"
    );
    // The tool listing survives it too — otherwise the holder loses the very
    // tool it needs to release what it is holding.
    assert!(lock_tool_names(&reg, &c1).contains(&"release_lock".to_string()));

    // A file that parses and genuinely drops the resource IS honoured, and
    // says so — this is the case the guard above must not swallow.
    fs::write(
        repo.join(".loomux").join("workflow.yml"),
        format!(
            "version: {}\nblocks:\n  - id: w\n    name: Worker\n    kind: worker\n    cli: claude\n    model: sonnet\n",
            workflow::SCHEMA_VERSION
        ),
    )
    .unwrap();
    let state = lock_json(&reg, &c1);
    assert!(state["resources"].as_array().unwrap().is_empty());
    let dropped = reg
        .audit_log(&g.id)
        .into_iter()
        .find(|e| e.action == "lock-undeclared")
        .expect("dropping a declared resource with a live holder is audited");
    assert_eq!(dropped.detail["holders"], json!([c1.agent_id]));
}

/// **The `{{LOCKS}}` / `{{LOCKS_ORCH}}` fragments reach only a group whose repo
/// actually declares resources** — and, like the merge-queue fragment, "actually
/// declares" means BOTH the block and the advanced-orchestrator toggle.
///
/// The absence half is pinned hard by `tests/workflow.rs`'s blessed goldens and
/// its `the_default_rendering_never_names_the_gate_machinery` rule — which is
/// how this shape got caught here too: the first cut put these bullets in the
/// base templates and that test went red, naming `.loomux/workflow.yml` in a
/// rendering for a group that has no workflow file. What nothing pins is the
/// **presence** half: a substitution that was always empty would satisfy every
/// one of those, and the agents would simply never be told the tools exist.
#[test]
fn the_lock_notes_reach_only_a_group_whose_repo_declares_resources() {
    let worker_marker = "Queued means END YOUR TURN";
    let orch_marker = "why is that worker quiet";
    let read = |reg: &OrchRegistry, g: &str, file: &str| {
        fs::read_to_string(reg.state_root().join(g).join(file)).unwrap()
    };

    // Declared + toggle on: every role that can hold a lock is told, and each
    // is told ITS OWN half.
    let (reg, _d) = test_registry();
    let repo = repo_with_resources("locks-note-on", BUILD_ONE_SLOT);
    let g = reg.create_group(repo.to_str().unwrap(), lock_rails()).unwrap();
    let orch = read(&reg, &g.id, "orchestrator.md");
    assert!(orch.contains(orch_marker), "the orchestrator is told what list_locks is FOR");
    assert!(orch.contains("name the lock in the brief"), "…and that briefing it is its job");
    assert!(
        !orch.contains(worker_marker),
        "…and not the agent-side half: an orchestrator does not take build locks"
    );
    for file in ["worker.md", "reviewer.md"] {
        let text = read(&reg, &g.id, file);
        assert!(text.contains(worker_marker), "{file} must carry the end-your-turn rule");
        assert!(!text.contains(orch_marker), "{file} must not carry the orchestrator's half");
    }
    // planner.md is given neither fragment. That is a claim about the
    // TEMPLATE only — the refusal itself is enforced at dispatch and pinned by
    // `every_lock_tool_refuses_a_planner_and_says_why_in_lock_terms`, not here
    // (rev-lead, PR #859 finding 1: this comment used to assert the refusal
    // while testing the absence of a placeholder).
    assert!(!read(&reg, &g.id, "planner.md").contains("acquire_lock"));

    // A workflow with no `resources:` block: nothing, and no raw placeholder.
    let (reg, _d) = test_registry();
    let repo = repo_with_merge_queue("locks-note-none", false); // a workflow, no resources
    let g = reg.create_group(repo.to_str().unwrap(), advanced_rails()).unwrap();
    for file in ["orchestrator.md", "worker.md", "reviewer.md"] {
        let text = read(&reg, &g.id, file);
        assert!(!text.contains("acquire_lock"), "{file}: no resources declared, no lock prose");
        assert!(!text.contains("{{LOCKS"), "{file}: the placeholder is substituted, not left raw");
    }

    // Declared, but the advanced orchestrator OFF: still nothing. The workflow
    // file is not in force, so neither are its resources — and that is exactly
    // the group whose rendering must stay byte-for-byte pre-#222.
    let (reg, _d) = test_registry();
    let repo = repo_with_resources("locks-note-toggle-off", BUILD_ONE_SLOT);
    let g = reg.create_group(repo.to_str().unwrap(), rails()).unwrap();
    for file in ["orchestrator.md", "worker.md", "reviewer.md"] {
        let text = read(&reg, &g.id, file);
        assert!(!text.contains("acquire_lock"), "{file}: toggle off means the block is not in force");
    }
}

/// **The sweep's only production seam.** `gh_poll_tick` is what makes holds and
/// waits expire in the shipped app; every other lock test calls `locks_tick`
/// directly, so deleting the call inside `gh_poll_tick` would leave all of them
/// green while nothing but the `mark_dead` fast path ever reclaimed anything
/// (rev-lead, PR #859 finding 3 — the lessons-file rule that a subsystem is not
/// done until a production path calls it). Starts at `gh_poll_tick` for exactly
/// the reason its merge-queue and intake siblings do.
///
/// No subprocess: this group is not autonomous, so the intake half returns at
/// its no-due-groups guard before any `gh` call.
#[test]
fn the_gh_poll_tick_drives_the_lock_sweep() {
    let (reg, _d, g, c1, c2) = setup_locks("locks-seam", BUILD_ONE_MINUTE);
    lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build" }));
    lock_call(&reg, &c2, "acquire_lock", json!({ "name": "build" }));

    // Two minutes on, past the declared one-minute max hold — reached through
    // the wake the app actually runs, not through locks_tick.
    reg.gh_poll_tick(now_ms() + 2 * 60_000, &HashMap::new());

    assert!(
        lock_audit_actions(&reg, &g).contains(&"lock-expired".to_string()),
        "the poll tick must run the sweep: without that call nothing here expires"
    );
    let state = lock_json(&reg, &c2);
    assert_eq!(
        state["resources"][0]["holders"][0]["agent"],
        json!(c2.agent_id),
        "…and the queue must move on that same wake"
    );
}

/// **All three lock tools refuse a planner, at the dispatch gate.** The plural
/// in the older test's name was a coverage claim it did not make: it exercised
/// `acquire_lock` alone, so `release_lock`'s check could have been deleted and
/// `list_locks` had none at all while it stayed green (rev-lead, PR #859
/// finding 1). This is the test that makes the claim true.
#[test]
fn every_lock_tool_refuses_a_planner_and_says_why_in_lock_terms() {
    let (reg, _d, g, _c1, _c2) = setup_locks("locks-planner", BUILD_ONE_SLOT);
    let planner = reg.spawn_agent(&g, Role::Planner, "p", "plan", false, None).unwrap();
    let cp = reg.resolve_token(&planner.token).unwrap();

    for (tool, args) in [
        ("acquire_lock", json!({ "name": "build" })),
        ("release_lock", json!({ "name": "build" })),
        ("list_locks", json!({})),
    ] {
        let (err, text) = lock_call(&reg, &cp, tool, args);
        assert!(err, "{tool} must refuse a planner at dispatch, not merely omit it from the listing");
        // #859 finding 4: the refusal has to be about the thing the caller
        // asked for. The shared gate used to answer every planner with the
        // notification tools' reason, so a planner refused a LOCK was told
        // about watches.
        assert!(
            text.contains("cannot take or read locks"),
            "{tool}'s refusal must be in lock terms: {text}"
        );
        assert!(!text.contains("register notifications"), "{tool}: wrong family's reason: {text}");
    }
    // …and the cosmetic half stays cosmetic: a planner sees none of them.
    let names = lock_tool_names(&reg, &cp);
    for tool in ["acquire_lock", "release_lock", "list_locks"] {
        assert!(!names.contains(&tool.to_string()), "a planner must not be offered {tool}");
    }
    // The notification family keeps its own reason — one gate, two answers.
    let (err, text) = lock_call(&reg, &cp, "notify_when", json!({ "kind": "pr_checks", "pr": "1" }));
    assert!(err);
    assert!(text.contains("register notifications"), "{text}");
}

/// `wait_minutes` that is present but not a whole number is REJECTED, never
/// silently defaulted — the `notify_when` rule, which has this same pin at
/// `expires_minutes`. Without it the caller that wrote `"30"` waits an hour and
/// is never told (rev-lead, PR #859 finding 5).
#[test]
fn a_non_integer_wait_minutes_is_refused_rather_than_defaulted() {
    let (reg, _d, _g, c1, c2) = setup_locks("locks-waitarg", BUILD_ONE_SLOT);
    lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build" }));

    for bad in [json!("30"), json!(30.5), json!(-5), json!(true)] {
        let (err, text) =
            lock_call(&reg, &c2, "acquire_lock", json!({ "name": "build", "wait_minutes": bad }));
        assert!(err, "wait_minutes {bad} must be refused: {text}");
        assert!(text.contains("whole number of minutes"), "{text}");
    }
    // Refused means refused: nothing was queued on the way to the error.
    let state = lock_json(&reg, &c1);
    assert!(state["resources"][0]["queue"].as_array().unwrap().is_empty());

    // Omitted entirely is the one case that legitimately defaults, and it is
    // the clamp's default rather than an invented one.
    let (err, text) = lock_call(&reg, &c2, "acquire_lock", json!({ "name": "build" }));
    assert!(!err, "{text}");
    assert!(text.contains("60 min"), "the documented default: {text}");
}

/// **A multi-slot resource, end to end through `workflow.yml`.** `slots: 2` was
/// covered in the engine's unit tests (from a hand-built `ResourcePolicy`) and
/// at the parse layer, but the `workflow.yml -> LockTable.slots` chain was never
/// exercised above 1, so a wiring bug that pinned every resource to one slot
/// would have been invisible (rev-lead, PR #859 finding 14).
#[test]
fn a_two_slot_resource_admits_two_holders_through_the_wired_path() {
    let (reg, _d, g, c1, c2) =
        setup_locks("locks-multislot", "resources:\n  gpu:\n    slots: 2\n    max_hold_minutes: 45\n");
    let w3 = reg.spawn_agent(&g, Role::Worker, "w3", "task", false, None).unwrap();
    let c3 = reg.resolve_token(&w3.token).unwrap();

    for c in [&c1, &c2] {
        let (err, text) = lock_call(&reg, c, "acquire_lock", json!({ "name": "gpu" }));
        assert!(!err && text.contains("is YOURS"), "both slots are grantable: {text}");
    }
    let (err, text) = lock_call(&reg, &c3, "acquire_lock", json!({ "name": "gpu" }));
    assert!(!err, "{text}");
    assert!(text.contains("position 1"), "the third contender queues: {text}");

    let state = lock_json(&reg, &c1);
    assert_eq!(state["resources"][0]["slots"], json!(2));
    assert_eq!(state["resources"][0]["holders"].as_array().unwrap().len(), 2);

    // And the tool description pluralizes — the menu is how an agent learns
    // what exists, and "2 slot" would be the only place it ever reads it.
    let listed = dispatch(&reg, &c1, "tools/list", &Value::Null).unwrap();
    let desc = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "acquire_lock")
        .expect("acquire_lock is listed")["description"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(desc.contains("'gpu' (2 slots, max hold 45 min)"), "{desc}");
}

/// **Ending a group releases every lock its agents held, and records it.**
///
/// The first cut of this test asserted that `locks_tick` no longer names the
/// ended group — which cannot fail (rev-lead, round 2): `end_group` marks every
/// member dead first, so `cleanup_agent_locks` has already emptied the table by
/// the time the removal line runs, and a sweep over an empty table returns an
/// empty `Sweep` and is never pushed onto `acted`. Deleting the removal line
/// left it green.
///
/// So this pins the property that IS observable and DOES redden: the audit
/// record. `lock-reclaim` is written by `cleanup_agent_locks` on the way
/// through `mark_dead`, and it is durable — remove that call and this test
/// fails, which is the regression worth catching, because a group whose agents
/// exit without their holds being released is the stranded-slot case the whole
/// reclaim design exists to prevent.
///
/// **What this deliberately does NOT pin, and why.** The `self.locks` entry
/// removal itself (finding 8) is invisible from every public surface: `locks`
/// is private, `lock_state` re-creates the entry through `or_default()` before
/// it can report on it, and `locks_tick` returns only the groups it *acted on*,
/// which an empty table never is. Pinning it needs a `#[doc(hidden)] pub`
/// accessor on `OrchRegistry` — product code, which this round is not touching
/// (the human is demoing from this branch). Named here rather than left as a
/// silent gap: that one line ships untested, and the accessor is the follow-up.
#[test]
fn ending_a_group_releases_and_records_the_locks_its_agents_held() {
    let (reg, _d, g, c1, c2) = setup_locks("locks-endgroup", BUILD_ONE_SLOT);
    lock_call(&reg, &c1, "acquire_lock", json!({ "name": "build" }));
    lock_call(&reg, &c2, "acquire_lock", json!({ "name": "build" }));
    assert_eq!(
        lock_json(&reg, &c1)["resources"][0]["holders"][0]["agent"],
        json!(c1.agent_id),
        "precondition: w-1 holds it and w-2 is queued behind"
    );

    reg.end_group(&g, false).unwrap();

    let reclaims: Vec<Value> = reg
        .audit_log(&g)
        .into_iter()
        .filter(|e| e.action == "lock-reclaim" || e.action == "lock-wait-cleanup")
        .map(|e| e.detail)
        .collect();
    assert!(
        reclaims.iter().any(|d| d["agent"] == json!(c1.agent_id) && d["why"] == json!("agent-gone")),
        "the holder's lock must be released and recorded on the way out: {reclaims:?}"
    );
    assert!(
        reclaims.iter().any(|d| d["agent"] == json!(c2.agent_id)),
        "…and so must the queued request behind it: {reclaims:?}"
    );
}

// ---------- #903: question-SHAPED text is not a question ----------
//
// The live incidents: three resumed reviewer panes (all resumes of one session,
// deterministic on demand) and the ORCHESTRATOR's own pane, each with deliveries
// parked behind "an interactive question is on screen" that never released — 25
// minutes, then 30+ minutes and four deliveries, ending with panes killed by
// hand. The human's report is the whole characterization: *"there were no
// interactive questions asked that I could tell."*
//
// **What non-question content satisfies the predicate.** Everything the detector
// keys on is TEXT, and half of it is ordinary English:
//
//  - the wide (12-line) tiers match `(y/n)`, `do you want to proceed`, `yes/no`,
//    `waiting for your` anywhere in the last twelve painted lines — which is
//    exactly where a finished turn's report sits;
//  - the grid then *agrees*, because that text really is rendered. #534's
//    release is `NotRendered`, and the text has not gone anywhere.
//
// A resumed pane makes it permanent, the same way #727 and #820 did: the
// restored screen is static, so nothing ever repaints the prose away, and the
// drainer's uncapped poll re-reads the identical screen every two seconds
// forever. An agent whose job is reviewing THIS detector writes `(y/n)` into its
// own verdict, which is why one session tripped it every single time.
//
// Three layers, each with its own tests below:
//  h1/h2  the detector is quieter — prose-shaped permission phrases join the
//         last-painted tier, and a rendered empty composer overrides
//         `StillRendered`;
//  h3/h4  neither narrowing touches a real dialog (the #518/#532 hostage class
//         this gate exists for);
//  h5-h7  the bounded last-resort override, for a false positive shaped in a way
//         nobody has characterized yet.

/// A fixture's bytes as a pty would actually have emitted them: CRLF, always.
///
/// **This is not tidying — it is the difference between a screen and a
/// staircase, and it is platform-dependent.** `painted()`'s doc states the rule
/// for synthetic fixtures: a bare LF is an INDEX (down one row, column
/// untouched), so text written with LFs marches rightward across the grid and
/// composes into something no terminal would ever show. A file-backed fixture is
/// subject to the same rule *and* to `core.autocrlf`, which normalizes these
/// files to LF in the git blob — so `include_str!` hands a Windows checkout CRLF
/// and a Linux one bare LF, and the same test composes two different screens.
/// It cost this PR a CI round: h1's rendered-precondition passed on Windows and
/// failed on ubuntu/macos with the fixture's own text shredded across columns.
fn pty_bytes_903(fixture: &str) -> Vec<u8> {
    fixture.replace("\r\n", "\n").replace('\n', "\r\n").into_bytes()
}

/// The composed screen of a #903 fixture, at the geometry #727's tests use.
fn composed_903(fixture: &str) -> String {
    trustworthy_composition(loomux_lib::orchestration::termgrid::render_visible(
        &pty_bytes_903(fixture),
        100,
        12,
    ))
    .expect("precondition: these screens compose to a readable grid")
}

#[test]
fn h1_the_repro_screens_match_the_detector_and_release_anyway() {
    for (name, fixture, signal) in [
        ("resumed reviewer verdict", FIX_FP_RESUMED_VERDICT, "yes-no-token"),
        ("orchestrator relay", FIX_FP_ORCH_RELAY, "permission-phrase"),
    ] {
        // Precondition 1 — this IS the false positive, not a screen that passes
        // for some unrelated reason. The ring matches, and by a WIDE-tier signal
        // that #903 deliberately did not demote, so the release below can only
        // be the composed-screen reading.
        let m = prompt_wait_match(&strip_ansi(fixture.as_bytes()))
            .unwrap_or_else(|| panic!("{name}: precondition: the ring must still match"));
        assert_eq!(m.signal, signal, "{name}: and by the signal the incident is about");

        // Precondition 2 — the matched text is still ON the screen. #534's
        // release cannot fire here; if it could, this fixture would be testing
        // the wrong thing.
        let visible = composed_903(fixture);
        assert!(
            match_still_rendered(&visible, &m),
            "{name}: precondition: the match is genuinely rendered — the release is NOT \
             'the dialog went away': {visible:?}"
        );
        assert!(
            visible.contains("auto mode on"),
            "{name}: precondition: this is a live screen, not an absent one"
        );

        // The fix: the pane's own bottom row says it is waiting for free text.
        assert!(
            idle_prompt_rendered(Composed::plain(&visible)),
            "{name}: an empty composer is on screen and no menu selection is: {visible:?}"
        );
        assert_eq!(
            grid_evidence_for(&m, Some(Composed::plain(visible.as_str()))),
            GridEvidence::IdlePrompt,
            "{name}: and that reading beats `StillRendered`"
        );

        // End to end, through the production predicate, on both readings taken
        // from the one raw stream exactly as `question_sample` takes them.
        let raw = pty_bytes_903(fixture);
        let pred = question_hold_predicate_sampled(
            move || sample_from_raw(&raw, 100, 12),
            None,
            None,
            Vec::new(),
        );
        assert!(!pred(), "{name}: a pane idling at an empty box must take its delivery");
    }
}

#[test]
fn h2_permission_phrasings_that_are_sentences_are_read_only_from_the_last_lines() {
    // The token half of the fix, isolated from the grid half. These three were
    // in the WIDE tiers, whose justification has always been "these don't occur
    // in ordinary prose" — and all three are ordinary prose.
    let prose = "the reviewer asked for a yes/no confirmation before merging\n\
                 and is waiting for your call on the shape\n\
                 press enter to continue past the pager, then re-run it\n\
                 build finished\n\
                 all tests pass\n\
                 > ";
    assert!(
        prompt_wait_match(prose).is_none(),
        "a paragraph mentioning three permission phrasings, with the CLI's box redrawn \
         underneath it, is a finished turn: {:?}",
        prompt_wait_match(prose)
    );

    // ...and a LIVE one still fires, because a live one is the last thing
    // painted. That is the same rule the pointer and the menu footer have used
    // since #40, and the reason demoting costs nothing a real prompt needs.
    for (name, token) in [
        ("yes/no", "Continue? yes/no"),
        ("waiting for your", "Waiting for your answer"),
        ("press enter to continue", "Press Enter to continue"),
    ] {
        let live = format!("running the migration\nit touched 4 tables\n{token}");
        let m = prompt_wait_match(&live)
            .unwrap_or_else(|| panic!("{name}: a live prompt paints its phrase LAST"));
        assert_eq!(
            m.signal, "prose-permission-phrase",
            "{name}: and reports itself as the tier it now belongs to"
        );
    }
}

#[test]
fn h3_every_real_dialog_still_holds_and_none_of_them_reads_as_idle() {
    // The fail-SAFE direction, and the floor #727/#820 set: the captured and
    // reconstructed dialogs must be untouched by both narrowings. #518/#532 are
    // what a regression here costs — a delivery pasted into a live dialog does
    // not merge text, it SELECTS whatever is highlighted.
    for (name, fixture) in [
        ("claude-askuserquestion", FIX_CLAUDE_ASK),
        ("copilot-question", FIX_COPILOT_ASK),
        ("copilot-multichoice", FIX_COPILOT_MULTICHOICE),
        ("claude-mcp-approval", FIX_CLAUDE_MCP_APPROVAL),
        ("pointer-last", FIX_POS_PTR_LAST),
    ] {
        assert!(
            prompt_wait_match(&strip_ansi(fixture.as_bytes())).is_some(),
            "{name}: the token re-tiering must not lose a real dialog"
        );
        let visible = composed_903(fixture);
        assert!(
            !idle_prompt_rendered(Composed::plain(&visible)),
            "{name}: a screen with a dialog on it must never read as an idle composer: \
             {visible:?}"
        );
        let raw = pty_bytes_903(fixture);
        let pred = question_hold_predicate_sampled(
            move || sample_from_raw(&raw, 100, 12),
            None,
            None,
            Vec::new(),
        );
        assert!(pred(), "{name}: a real dialog must still hold the delivery (#420)");
    }
}

#[test]
fn h4_an_empty_box_beside_a_live_menu_does_not_release_it() {
    // The conjunct, and the reason it is a conjunct — **for layer 2**.
    // `idle_prompt_row_rendered` alone says "a composer is on screen somewhere";
    // a CLI that painted a dialog ABOVE a live composer would satisfy it while a
    // human was very much being asked something. Requiring that NO row anywhere
    // holds a highlighted choice is what closes that for the immediate release,
    // and it is the stated assumption that reading rests on.
    //
    // **Layer 3 deliberately does NOT have this conjunct** (rev-427 B2), which is
    // exactly why this screen matters twice: `h9` pins that the override's own
    // term is the weak reading, and the design note argues why the strong one
    // would make the override dead code and what withholds the Enter instead.
    let both = "❯ 1. Yes, allow once\n  2. No, and tell me why\n─────────\n❯\n─────────";
    assert!(
        idle_prompt_row_rendered(Composed::plain(both)),
        "precondition: the weaker reading DOES fire here — that is the hazard"
    );
    assert!(
        !idle_prompt_rendered(Composed::plain(both)),
        "…and the highlighted choice is what stops it becoming a release"
    );
    let m = prompt_wait_match("❯ 1. Yes, allow once\n  2. No, and tell me why")
        .expect("a numbered menu with a pointer is a question");
    assert_eq!(grid_evidence_for(&m, Some(Composed::plain(both))), GridEvidence::StillRendered);
    assert!(question_shown(Some(&m), Some(Composed::plain(both))), "a live menu holds, empty box or not");
}

#[test]
fn h5_the_last_resort_override_needs_every_term_on_the_same_poll() {
    let bound = QUESTION_HOLD_OVERRIDE_AFTER.as_millis() as u64;
    let t0 = 5_000_000u64;
    let overdue = t0 + bound;

    // All four terms — question hold, an open episode, the bound elapsed, and a
    // fresh idle streak — and only then.
    assert!(
        question_override_admits(WriteAdmission::HoldQuestion, Some(t0), overdue, bound, 2),
        "held past the bound with the pane's own screen reading idle: deliver"
    );

    // #510's absolute is untouched: a box with a human's line in it is never
    // overridden, at any age.
    assert!(
        !question_override_admits(WriteAdmission::HoldBoxOccupied, Some(t0), overdue, bound, 9),
        "a box-occupied hold is a person's half-typed line — never overridden"
    );
    assert!(
        !question_override_admits(WriteAdmission::Go, Some(t0), overdue, bound, 9),
        "nothing to override when the gates are already clear"
    );

    // One fresh read is not enough: this reading licenses a WRITE, and a single
    // composition can catch a mid-redraw instant.
    assert!(
        !question_override_admits(WriteAdmission::HoldQuestion, Some(t0), overdue, bound, 1),
        "one idle read is not a streak"
    );
    // Not yet overdue.
    assert!(
        !question_override_admits(WriteAdmission::HoldQuestion, Some(t0), overdue - 1, bound, 2),
        "one millisecond short of the bound is still inside it"
    );
    // No open hold episode: nothing has been measured, so nothing is overdue.
    assert!(
        !question_override_admits(WriteAdmission::HoldQuestion, None, overdue, bound, 2),
        "no episode, no clock, no override"
    );
    // The escape hatch's own escape hatch: a mis-set constant degrades to
    // today's holding, never to delivering into every dialog on screen.
    assert!(
        !question_override_admits(WriteAdmission::HoldQuestion, Some(t0), overdue, 0, 9),
        "bound 0 disables the override, the same convention every bound here uses"
    );
}

#[test]
fn h6_the_override_bound_sits_between_the_badge_and_the_still_queued_notice() {
    // Not a restatement of the constant — an ORDERING, and the argument for the
    // number. A human must be badged before loomux acts on their behalf, and the
    // queue must move before the generic "still queued" notice is the first
    // anyone hears of it. #903's incidents ran 25 and 30+ minutes, so the bound
    // has to land inside a human's patience rather than merely inside infinity.
    assert!(
        QUESTION_HOLD_OVERRIDE_AFTER > QUESTION_HOLD_STALE_AFTER,
        "the human is badged (and given time to look) before the override fires"
    );
    assert!(
        QUESTION_HOLD_OVERRIDE_AFTER < queue::QUEUE_STILL_QUEUED_NOTICE_AFTER,
        "…and the queue moves before the 'still queued' notice would report it stuck"
    );
}

#[test]
fn h7_an_unreadable_screen_still_holds_and_that_is_the_stated_limit() {
    // The one class neither layer covers, pinned so it is a known limit rather
    // than a surprise. With no trustworthy composition there is no idleness to
    // observe: the ring's word stands, the veto cannot fire, and the override
    // has nothing to count a streak of. Releasing on no evidence at all is
    // precisely what this guard must never do — see the design note's limits.
    let m = prompt_wait_match("Do you want to proceed? (y/n)").expect("matches");
    assert!(question_shown(Some(&m), None), "no evidence either way -> the ring's word stands");
    assert_eq!(grid_evidence_for(&m, None), GridEvidence::Unreadable);
    assert!(
        !question_override_admits(
            WriteAdmission::HoldQuestion,
            Some(0),
            QUESTION_HOLD_OVERRIDE_AFTER.as_millis() as u64 * 10,
            QUESTION_HOLD_OVERRIDE_AFTER.as_millis() as u64,
            0,
        ),
        "an unreadable screen never produces an idle streak, however long the hold runs"
    );
}

/// The repro fixture with a delivery sitting unsubmitted in its composer — the
/// screen `deliver_now`'s PRE-ENTER checkpoint actually reads (#903 rev-427 B1).
///
/// Built by replacing the bare `❯` row with `❯ <brief>`, which is what the CLI
/// paints once loomux's bracketed paste lands: same screen, same replayed prose,
/// composer now occupied by our own text.
fn with_paste_in_composer(fixture: &str, brief: &str) -> String {
    let out: Vec<String> = fixture
        .replace("\r\n", "\n")
        .lines()
        .map(|l| {
            // The prompt row is the one whose only content is the glyph; the SGR
            // wrapper around it is preserved so the fixture still exercises
            // `strip_ansi` and the grid the way the untouched one does.
            if l.replace("\u{1b}[38;5;153m", "").replace("\u{1b}[0m", "").trim() == "❯" {
                l.replace('❯', &format!("❯ {brief}"))
            } else {
                l.to_string()
            }
        })
        .collect();
    out.join("\n")
}

#[test]
fn h8_the_pre_enter_checkpoint_releases_our_own_paste_and_still_holds_a_real_dialog() {
    // **rev-427's blocking finding, both directions.**
    //
    // Layer 2 releases the pre-paste gate and the paste lands. `deliver_now` then
    // runs `wait_for_question_clear` again with `Some(&pasted_text)` — and
    // `mask_own_paste` DELETES the composer row it claims, so the reading that
    // just released ("an empty prompt is on screen") finds nothing, the replayed
    // `(y/n)` prose is still rendered, and the gate re-asserts the very false
    // positive it had released. The delivery then aborts `AbortedPreEnter` with
    // the text stranded in the box, and the stranded-submit marker's own question
    // check reads the same screen forever: #903's wedge, plus an unsubmitted
    // paste, on a path the override does not cover.
    //
    // `h1` cannot see this — it passes `pasted_text: None`, which is the
    // pre-PASTE shape only.
    const BRIEF: &str = "Rebase onto main and re-read the review findings on PR #909.";

    for (name, fixture) in [
        ("resumed reviewer verdict", FIX_FP_RESUMED_VERDICT),
        ("orchestrator relay", FIX_FP_ORCH_RELAY),
    ] {
        let screen = with_paste_in_composer(fixture, BRIEF);
        // Precondition 1: the composer really is occupied by our text — the whole
        // premise of the finding. If this row were still bare the test would pass
        // for `h1`'s reason, not this one.
        assert!(
            screen.contains(&format!("❯ {BRIEF}")),
            "{name}: precondition: the brief must be IN the composer row"
        );
        let raw = pty_bytes_903(&screen);

        // Precondition 2: with the paste masked out, no EMPTY prompt row survives
        // — this is the mechanism, asserted rather than assumed.
        let composed = loomux_lib::orchestration::termgrid::render_visible(&raw, 100, 12);
        let masked = mask_own_paste(&composed, BRIEF);
        assert!(
            !idle_prompt_row_rendered(Composed::plain(&masked)),
            "{name}: precondition: the masked screen has no empty prompt row left — reading \
             idleness from it alone is exactly what re-asserted the hold: {masked:?}"
        );
        // ...and the prose the ring matched IS still on that masked screen, so
        // `NotRendered` cannot rescue it either.
        let m = prompt_wait_match(&mask_own_paste(&strip_ansi(&raw), BRIEF))
            .unwrap_or_else(|| panic!("{name}: precondition: the ring still matches"));
        assert!(
            match_still_rendered(&masked, &m),
            "{name}: precondition: the matched prose is still rendered after masking"
        );

        // The fix: the two views together say "the CLI is at its composer,
        // holding our own text" — which is a composer, not a dialog.
        assert!(
            idle_prompt_rendered(Composed { masked: masked.as_str(), with_paste: composed.as_str() }),
            "{name}: our own brief in the composer is still a composer"
        );

        // End to end through the production predicate, in the pre-Enter shape.
        let raw_for_pred = raw.clone();
        let pred = question_hold_predicate_sampled(
            move || sample_from_raw(&raw_for_pred, 100, 12),
            Some(BRIEF.to_string()),
            None,
            Vec::new(),
        );
        assert!(
            !pred(),
            "{name}: the pre-Enter gate must not re-assert the hold layer 2 just released — \
             that strands the paste and wedges a second gate behind it"
        );
    }

    // **The other direction, and it is the one that must not regress.** A REAL
    // dialog on screen while our paste sits in the composer still holds: the
    // Enter would SELECT a highlighted option, which no dedup rule can undo, and
    // this checkpoint is what withholds it (#518/#532, and the first line of the
    // override's own safety argument).
    let composer_row = format!("❯ {BRIEF}");
    let live = painted(&[
        "● Allow the command to run?",
        "",
        "│ ❯ Yes",
        "│   No, and tell me why",
        "",
        composer_row.as_str(),
    ]);
    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&live, 100, 12),
        Some(BRIEF.to_string()),
        None,
        Vec::new(),
    );
    assert!(
        pred(),
        "a live dialog above our own occupied composer must still withhold the Enter (#420)"
    );
}

#[test]
fn h9_the_override_keys_on_the_weak_reading_and_layer_two_on_the_strong_one() {
    // rev-427 non-blocking 1: nothing pinned WHICH idleness reading licenses the
    // write, so flipping the wiring moved no test — on the one term that decides
    // whether loomux types into a pane it was told not to.
    //
    // `h4`'s screen is the separator between the two readings: an empty composer
    // under a highlighted menu choice. Layer 2 must hold (the conjunct); the
    // override's term must be true (no conjunct). Both, on one screen, from the
    // production predicate's own witness — not from calling the two helpers
    // directly, which is what left the wiring untested.
    let both = "❯ 1. Yes, allow once\n  2. No, and tell me why\n─────────\n❯\n─────────";
    let raw = painted(&both.lines().collect::<Vec<_>>());

    let witness: QuestionWitness = Default::default();
    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&raw, 100, 12),
        None,
        Some(std::rc::Rc::clone(&witness)),
        Vec::new(),
    );
    assert!(pred(), "layer 2 holds: a highlighted choice is on screen");

    let seen = witness.borrow().clone().expect("the ring matched, so a witness exists");
    assert_eq!(
        seen.grid,
        GridEvidence::StillRendered,
        "layer 2's reading — the menu-absent conjunct refuses to call this idle"
    );
    assert!(
        seen.idle_row,
        "layer 3's reading — the composer IS on screen, and this is the term \
         `question_override_admits` counts a streak of. If this ever reads false, the override \
         is dead code: the strong reading would already have released the hold."
    );

    // And the reverse pairing on the plain repro screen: both readings agree, so
    // layer 2 releases and layer 3 never gets a hold to override.
    let idle = pty_bytes_903(FIX_FP_RESUMED_VERDICT);
    let w2: QuestionWitness = Default::default();
    let pred2 = question_hold_predicate_sampled(
        move || sample_from_raw(&idle, 100, 12),
        None,
        Some(std::rc::Rc::clone(&w2)),
        Vec::new(),
    );
    assert!(!pred2(), "no menu anywhere: layer 2 releases on its own");
    let seen2 = w2.borrow().clone().expect("the ring matched here too");
    assert_eq!(seen2.grid, GridEvidence::IdlePrompt);
    assert!(seen2.idle_row, "and the weak reading agrees, as it must whenever the strong one does");
}

// ---------- #903 rev-433: the composer is whatever THAT CLI paints ----------
//
// rev-433's blocking finding, and the audit it asked for. `h8` proved the
// pre-Enter release on Claude Code's chevron composer — with a reading that
// required a **prompt glyph**, which is a fact about Claude Code, not about
// composers. Copilot's 1.0.64+ prompt frame paints `┃ ` on every row it wrapped a
// paste onto and **no** glyph at all, so on that pane class a multi-row brief read
// as "not a composer", the pre-Enter gate re-asserted on still-rendered prose, and
// the paste stranded — which then wedges the box gate, where the override refuses
// to help. A regression introduced by THIS PR: before it, the pre-paste gate held
// and nothing was ever pasted there.
//
// **The audit, across every composer render this repo has evidence for:**
//
// | CLI / composer | how it paints a held paste | glyph? | mask claims it? |
// | --- | --- | --- | --- |
// | Claude Code box | `❯ <text>` | yes | yes |
// | copilot chevron | `❯ <text>` | yes | yes |
// | copilot framed 1.0.64+ | `┃ <text>` every row | **no** | yes |
// | copilot framed + scrollbar | `┃ <text>  ┃` | **no** | **no** (#821 residual) |
// | opencode | **no capture exists in this repo** | unknown | yes, by construction |
//
// Two rows settle the design between them. **opencode** is sourced from upstream
// docs under constraint 3 — no `opencode` process has ever been run by an agent
// here — so its composer render is genuinely unknown and a shape-based reading
// could only be guessed at. Authorship is not guessable: a row the paste mask
// claimed is loomux's own text, whoever painted it and however. That is why the
// general clause of `idle_prompt_row_rendered` asks nothing about shape, and why
// this suite can make a claim about a CLI it has never seen.
//
// **The scrollbar row is the honest limit** and `h12` pins it: there the mask
// itself cannot claim the rows (the trailing `┃` defeats reconstruct-to-end —
// #821's stated residual), so authorship has nothing to key on either. Not a
// regression, because nothing was ever pasted into that pane class before this PR
// either; not fixed, and said so rather than left to be discovered.

/// The composer captures whose rows `mask_own_paste` can actually claim, each
/// with the brief it was given. Reused rather than invented — these are the exact
/// renders #820/#821 already had to reason about.
fn claimable_composer_cases() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("copilot framed multi-row", FIX_BOX_GUTTER_MULTIROW, GUTTER_PASTE_TEXT),
        ("copilot framed wrap", FIX_COPILOT_FRAMED_WRAP, FRAMED_PASTE_TEXT),
        ("copilot chevron", FIX_COPILOT_COMPOSER_PASTE, COMPOSER_PASTE_TEXT),
    ]
}

/// The tail window `idle_prompt_row_rendered` reads, spelled out here so the
/// tests can assert reach as a precondition rather than trusting the constant.
fn tail_rows_903(visible: &str) -> Vec<&str> {
    let rows: Vec<&str> = visible.lines().filter(|l| !l.trim().is_empty()).collect();
    rows[rows.len().saturating_sub(8)..].to_vec()
}

#[test]
fn h10_a_composer_holding_our_paste_reads_idle_with_no_glyph_anywhere() {
    // Each capture gets #903's own trigger painted above it — a finished turn's
    // report quoting `(y/n)`. Without it these panes would release for the
    // trivial reason that nothing matched at all, and the test would pass while
    // saying nothing about the composer reading.
    //
    // **The second sentence is rev-438's** and it is not decoration. `use arrow
    // keys` is a `MENU_FOOTER_TOKENS` member, and #40 put those tokens in the
    // windowed tier precisely because agents write them in ordinary prose
    // (`fp-prose-arrow-keys.txt` is that fixture). Read over the whole screen —
    // which is what the rev-433 conjunct did — one such sentence anywhere in a
    // transcript carried "menu structure" forever and vetoed the #903 release for
    // exactly the class of pane this issue is about.
    const PROSE: &str = "● Finding 1: the gate reads any line carrying (y/n) as a live prompt.\n\
         ● It behaves like a picker where you use arrow keys to move between entries.";

    for (name, fixture, brief) in claimable_composer_cases() {
        let screen =
            format!("{PROSE}\n{}", fixture.replace("\r\n", "\n"));
        let raw = pty_bytes_903(&screen);
        let with_paste = loomux_lib::orchestration::termgrid::render_visible(&raw, 100, 14);
        let masked = mask_own_paste(&with_paste, brief);

        // Precondition 0 — the ring genuinely matches on the masked screen, so
        // the release below is the composer reading and not an absence.
        assert!(
            prompt_wait_match(&mask_own_paste(&strip_ansi(&raw), brief)).is_some(),
            "{name}: precondition: the ring must still match after masking our paste"
        );

        // Precondition 1 — the mask really does claim rows here. Without it the
        // test asserts something about a screen the mask never touched, which is
        // how a vacuous pass looks.
        let survived: std::collections::HashSet<&str> = masked.lines().collect();
        let claimed: Vec<&str> = with_paste
            .lines()
            .filter(|l| !l.trim().is_empty() && !survived.contains(l))
            .collect();
        assert!(
            !claimed.is_empty(),
            "{name}: precondition: `mask_own_paste` must claim at least one row"
        );

        // Precondition 2 — a claimed row is inside the tail window. This is
        // `IDLE_PROMPT_TAIL_ROWS`'s reach asserted per fixture, so a CLI that
        // grows its footer fails HERE, loudly, instead of silently reading "not a
        // composer" in production (rev-433's second path).
        assert!(
            tail_rows_903(&with_paste).iter().any(|r| !survived.contains(*r)),
            "{name}: precondition: a claimed row must be within the tail window — if this \
             fails, IDLE_PROMPT_TAIL_ROWS is too small for this CLI's chrome"
        );

        // Precondition 3 — and for the framed shapes, there is genuinely no
        // prompt glyph to key on. This is the finding, made executable: the old
        // reading asked for one.
        if name.starts_with("copilot framed") {
            assert!(
                !claimed.iter().any(|r| {
                    let t = r.trim_start_matches(|c: char| {
                        c == '│' || c == '┃' || c == '|' || c.is_whitespace()
                    });
                    t.starts_with('❯') || t.starts_with('›') || t.starts_with('>')
                }),
                "{name}: precondition: the framed composer paints NO prompt glyph — that is \
                 why a glyph-keyed reading missed it: {claimed:?}"
            );
        }

        assert!(
            idle_prompt_row_rendered(Composed {
                masked: masked.as_str(),
                with_paste: with_paste.as_str()
            }),
            "{name}: a composer holding our own paste is a composer, glyph or no glyph"
        );

        // End to end at the checkpoint that strands: the pre-Enter shape.
        let raw2 = raw.clone();
        let pred = question_hold_predicate_sampled(
            move || sample_from_raw(&raw2, 100, 14),
            Some(brief.to_string()),
            None,
            Vec::new(),
        );
        assert!(
            !pred(),
            "{name}: the pre-Enter gate must not hold on a pane whose only occupant is our \
             own brief — that aborts with the paste stranded"
        );
    }
}

#[test]
fn h11_a_real_dialog_still_holds_on_every_cli_composer_shape() {
    // The fail-safe half of the audit, and the one #518/#532 are about. Same
    // composer shapes, each with a genuine numbered menu painted above them: the
    // Enter would SELECT an option, so every one must hold.
    //
    // A numbered menu rather than a bare pointer on purpose — `❯ 1.` is read
    // across the detector's whole 12-line window, so the assertion is about the
    // composer reading and not about where the CLI's own hint bar happens to push
    // a prose-tier signal.
    for (name, fixture, brief) in claimable_composer_cases() {
        let mut rows: Vec<String> = vec![
            "● Allow the command to run?".into(),
            "".into(),
            "│ ❯ 1. Yes".into(),
            "│   2. No, and tell me why".into(),
            "".into(),
        ];
        rows.extend(fixture.replace("\r\n", "\n").lines().map(|l| l.to_string()));
        let raw = pty_bytes_903(&rows.join("\n"));

        let pred = question_hold_predicate_sampled(
            move || sample_from_raw(&raw, 100, 18),
            Some(brief.to_string()),
            None,
            Vec::new(),
        );
        assert!(
            pred(),
            "{name}: a live dialog above the composer must still withhold the Enter (#420/#532)"
        );
    }
}

#[test]
fn h12_the_two_shapes_the_audit_does_not_close_are_pinned_as_limits() {
    // **Where the audit stops, demonstrated rather than asserted.** Both of these
    // are behaviours a future reader could mistake for bugs; both are decisions.

    // 1. The copilot scrollbar shape. `mask_own_paste` cannot claim these rows at
    //    all — the trailing `┃` defeats reconstruct-to-end, which is #821's own
    //    stated residual — so authorship has nothing to key on and the composer
    //    is not recognised. Not a regression (nothing was pasted into this pane
    //    class before this PR either); not fixed.
    let raw = pty_bytes_903(FIX_BOX_GUTTER_SCROLLBAR);
    let with_paste = loomux_lib::orchestration::termgrid::render_visible(&raw, 100, 12);
    let masked = mask_own_paste(&with_paste, GUTTER_PASTE_TEXT);
    assert_eq!(
        masked, with_paste,
        "precondition: the mask claims NOTHING on the scrollbar shape (#821's residual) — if \
         this ever starts claiming rows, the limit below is closed and should be deleted"
    );
    assert!(
        !idle_prompt_row_rendered(Composed {
            masked: masked.as_str(),
            with_paste: with_paste.as_str()
        }),
        "known limit: authorship cannot see a composer the mask cannot claim"
    );

    // 2. The emptiness clause keeps its glyph requirement, and cannot drop it.
    //    The tempting generalisation is "a row made of nothing but decoration",
    //    which would finally cover a copilot framed composer sitting EMPTY (`┃`
    //    alone, no glyph, no paste to key authorship on). It cannot be taken: a
    //    dialog's own blank framed row is byte-identical to it, and one sits
    //    inside the tail window of a REAL captured dialog.
    let dialog = composed_903(FIX_CLAUDE_ASK);
    assert!(
        tail_rows_903(&dialog).iter().any(|r| {
            let t = r.trim();
            !t.is_empty() && t.chars().all(|c| c == '│' || c == '┃' || c.is_whitespace())
        }),
        "precondition: a real dialog capture has a decoration-only row in the tail window — \
         exactly what a 'decoration means composer' rule would match"
    );
    assert!(
        !idle_prompt_row_rendered(Composed::plain(&dialog)),
        "…and today's clause refuses it, which is what keeps h3 green"
    );

    // The cost of that refusal, pinned so it is a known behaviour rather than a
    // surprise: an empty, glyph-less composer reads as not idle, so layer 2
    // cannot release it and the pane falls through to the bounded override. A
    // degradation, not a wedge.
    let empty_framed = "● Ready.\n\n┃\n  @ files · # issues";
    assert!(
        !idle_prompt_row_rendered(Composed::plain(empty_framed)),
        "known limit (design note): an empty glyph-less composer is not recognised — the \
         bounded override is the channel for that pane"
    );
}

#[test]
fn h13_a_pointerless_dialog_above_our_composer_still_holds() {
    // The gap the CLI-agnostic widening would have opened, closed by widening the
    // conjunct in the same change (rev-433).
    //
    // Making the composer reading glyph-free means it now recognises composers it
    // never did — which exposes the conjunct to dialog shapes it was never asked
    // about. Claude Code's `AskUserQuestion` is the one that matters: it
    // highlights with reverse video, so `strip_ansi` leaves **no pointer at
    // all**. Under the old `!pointer_rendered` conjunct, that dialog plus a
    // composer holding our paste would have read idle and released an Enter into
    // a live selection.
    let brief = "Rebase onto main and re-read the findings.";
    let screen = format!("{}\n┃ {brief}\n  @ files · # issues", FIX_CLAUDE_ASK.replace("\r\n", "\n"));
    let raw = pty_bytes_903(&screen);
    let with_paste = loomux_lib::orchestration::termgrid::render_visible(&raw, 100, 16);
    let masked = mask_own_paste(&with_paste, brief);
    let c = Composed { masked: masked.as_str(), with_paste: with_paste.as_str() };

    // Precondition 1 — the composer IS recognised. Without this the test would
    // pass because the widening failed, not because the conjunct held.
    assert!(
        idle_prompt_row_rendered(c),
        "precondition: the framed composer holding our brief is recognised (the widening works)"
    );
    // Precondition 2 — and this dialog genuinely has no pointer, which is why the
    // old conjunct could not see it.
    assert!(
        !loomux_lib::orchestration::prompt_wait_match(&masked)
            .is_some_and(|m| m.needle == QuestionNeedle::LeadingPointer),
        "precondition: reverse video leaves no pointer glyph on this capture"
    );

    // The conjunct: numbered options and a selection footer are menu STRUCTURE.
    assert!(!idle_prompt_rendered(c), "a pointer-less dialog is still a dialog");

    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&raw, 100, 16),
        Some(brief.to_string()),
        None,
        Vec::new(),
    );
    assert!(pred(), "…and the pre-Enter gate withholds the Enter (#420/#532)");
}


// ═══════════════════════════════════════════════════════════════════════════
// J. #903 / #871: loomux's OWN delivered text, replayed onto a resumed pane
//
// The live wedge, reconstructed from group `loomux-68435179`'s own
// `audit.jsonl` rather than imagined. Pane `rev-1277` resumed session
// `39b611be`; the transcript `claude --resume` replayed ended with loomux's own
// kickoff prompt to `rev-1262` — the SAME session, the same app run, delivered
// 1.5 h earlier — which the CLI renders with a leading `❯`. Every poll for the
// next thirty minutes audited `signal:"pointer-option"`, `grid:"still-rendered"`
// and `idle_row:true`: loomux saw the empty composer and refused to act on it,
// because `menu_structure_rendered`'s whole-screen pointer clause was reading a
// row loomux itself had written.
//
// **Fixture provenance, and it is measured at both ends rather than asserted.**
// The twelve message rows are the 853-character prompt body recorded verbatim
// in the audit (`fp-resumed-prior-delivery-echo.prompt.txt`), word-wrapped at
// 78 columns:
//
//  - against the SCREEN: those rows' first six reproduce, byte for byte, the six
//    the orchestrator's own `get_output` captured off that pane (the capture
//    stops at six only because `get_output` caps at 500 characters);
//  - against the RING: padded to an 80-column pane and concatenated, they are a
//    byte-exact prefix of the `matched.line` the audit recorded — which is also
//    the evidence for how `echo_raw` below encodes the pane, and for why the
//    detector saw a pointer at all.
//
// The chrome below them — separator, bare `❯`, separator, `⏵⏵ auto mode on …` —
// is lifted unchanged from `fp-resumed-agent-idle-prompt.txt`, itself a real
// capture.
const FIX_FP_RESUMED_ECHO: &str =
    include_str!("../fixtures/attention/fp-resumed-prior-delivery-echo.txt");
const FIX_RESUMED_ECHO_PROMPT: &str =
    include_str!("../fixtures/attention/fp-resumed-prior-delivery-echo.prompt.txt");

/// The incident pane's geometry, both figures measured (see the section header).
const ECHO_COLS: u16 = 80;
/// Taller than the sixteen rows the fixture paints, so the replayed prompt's own
/// head does not scroll off — otherwise this would be exercising `R-top` rather
/// than the case the section is about.
const ECHO_ROWS: u16 = 20;

/// The fixture as the PANE emits it, which is not the same thing as the fixture.
///
/// A static replayed screen is painted by ADDRESSING rows, not by emitting
/// newlines, and `strip_ansi` deletes cursor-address sequences — so the byte ring
/// the detector reads is every row concatenated into ONE line, each padded to the
/// pane width. That is not a guess about the CLI: the audit's own `matched.line`
/// for this incident is a byte-exact prefix of exactly that concatenation at 80
/// columns, gaps and all (`(run` + 12 columns of padding + the next row's 2-space
/// indent = the 14 spaces the record shows).
///
/// Feeding the fixture through `pty_bytes_903` instead would put a newline
/// between every row, which moves the pointer row out of `prompt_wait_match`'s
/// last-three-painted window and makes the detector read CLEAR — a fixture that
/// reproduces the screen and not the bug.
fn echo_raw() -> Vec<u8> {
    let mut out = Vec::new();
    for (i, row) in FIX_FP_RESUMED_ECHO.replace("\r\n", "\n").lines().enumerate() {
        out.extend_from_slice(format!("\x1b[{};1H", i + 1).as_bytes());
        out.extend_from_slice(row.as_bytes());
        let painted = strip_ansi(row.as_bytes()).chars().count();
        for _ in painted..ECHO_COLS as usize {
            out.push(b' ');
        }
    }
    out
}

fn echo_visible(raw: &[u8]) -> String {
    loomux_lib::orchestration::termgrid::render_visible(raw, ECHO_COLS, ECHO_ROWS)
}

/// The prompt body as the session record holds it.
fn echo_record() -> Vec<String> {
    delivered_prompt_lines(FIX_RESUMED_ECHO_PROMPT.trim_end_matches(['\r', '\n']))
}

#[test]
fn j1_a_resumed_panes_replayed_delivery_latches_the_pointer_signal() {
    // The false positive itself, with no record — the behaviour every pane in
    // the incident had. The assertions run in the order the audit recorded them,
    // so a future reader can line this test up against the log.
    let raw = echo_raw();
    let m = prompt_wait_match(&strip_ansi(&raw)).expect("the ring matches — it did, for 30 min");
    assert_eq!(
        m.signal, "pointer-option",
        "and by the signal the audit named: the replayed row leads with a chevron"
    );
    assert!(
        m.line.contains("[orch] round 3"),
        "the row it matched is loomux's OWN delivery, replayed: {:?}",
        m.line
    );

    let visible = echo_visible(&raw);
    let c = Composed::plain(&visible);
    assert!(
        idle_prompt_row_rendered(c),
        "precondition — the composer IS on screen and empty. loomux saw this every poll \
         (`idle_row:true`) and it is what makes the hold indefensible: {visible:?}"
    );
    assert!(
        !idle_prompt_rendered(c),
        "…and the release is vetoed anyway, by the menu-structure conjunct reading a \
         pointer row loomux itself wrote — THE bug"
    );
    assert_eq!(
        grid_evidence_for(&m, Some(c)),
        GridEvidence::StillRendered,
        "which the grid reports exactly as the audit did"
    );

    let raw2 = echo_raw();
    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&raw2, ECHO_COLS, ECHO_ROWS),
        None,
        None,
        Vec::new(),
    );
    assert!(pred(), "so with no record of our own delivery, the gate holds — the wedge");
}

#[test]
fn j2_the_session_record_of_our_own_prompt_releases_the_resumed_pane() {
    // The fix, and the whole of it: the ONLY difference from `j1` is that loomux
    // remembers having delivered this text into this session.
    //
    // **The ring still matches, and that is the design working rather than the
    // fix falling short.** The mask cannot claim anything in the ring here — the
    // ring is the whole screen concatenated into one line, so no row of it is a
    // wrapped run of a recorded line — and it does not need to: #534's rule is
    // that the ring TRIGGERS and only the grid may RELEASE. What the record
    // changes is the grid's answer, from `StillRendered` to `IdlePrompt`, by
    // removing from the composed screen the rows that were vetoing it.
    let record = echo_record();
    assert_eq!(
        record.len(),
        1,
        "the incident's prompt is ONE logical line — 853 chars the CLI wrapped over twelve \
         rows, which is why `DELIVERED_PROMPT_CHARS` is not `DELIVERED_NOTICE_CHARS`"
    );
    assert!(
        record[0].chars().count() > DELIVERED_NOTICE_CHARS,
        "and it is longer than the notice cap, so a record sized for notices would hold a \
         PREFIX and reconstruct against nothing"
    );
    assert!(record[0].chars().count() <= DELIVERED_PROMPT_CHARS);

    let raw = echo_raw();
    let visible = echo_visible(&raw);
    let masked = mask_loomux_notices_with_record(&visible, &record);

    // The MECHANISM, not just the outcome: exactly the twelve rows of the
    // replayed prompt are claimed, and nothing else on the screen is.
    let rows_of = |s: &str| s.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(
        rows_of(&visible) - rows_of(&masked),
        12,
        "the whole wrapped run is claimed and only it — the chrome, the composer and the \
         footer all survive:\nBEFORE {visible:?}\nAFTER {masked:?}"
    );

    let m = prompt_wait_match(&strip_ansi(&raw)).expect("the ring matches, exactly as in j1");
    let c = Composed { masked: masked.as_str(), with_paste: masked.as_str() };
    assert_eq!(
        grid_evidence_for(&m, Some(c)),
        GridEvidence::IdlePrompt,
        "and THAT is the one transition this change makes: still-rendered -> idle-prompt"
    );

    let raw2 = echo_raw();
    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&raw2, ECHO_COLS, ECHO_ROWS),
        None,
        None,
        record.clone(),
    );
    assert!(!pred(), "so the delivery flushes, in seconds, at the FIRST poll");
}

#[test]
fn j3_a_recorded_line_that_is_a_dialog_option_is_refused_under_a_question_row() {
    // `h4`'s authorship twin, and the term that bounds what a widened record can
    // reach. `h4` pins that a live menu above an empty composer holds; this pins
    // that it still holds when the menu's highlighted option is byte-identical to
    // a line loomux really did deliver — the shape an orchestrator choosing its
    // own `send_prompt` text could construct.
    let option = "1. Yes, and do not ask again for git commit commands in this project";
    let record = vec![option.to_string()];

    // CONTROL first, so the assertion below cannot pass because the claim failed
    // for some unrelated reason (the length floor, the wrap rule, a typo).
    // Same rows, no question heading them: the claim IS made.
    let headless = format!("some ordinary transcript line\n❯ {option}\n  2. No\n─────────\n❯");
    assert!(
        !mask_loomux_notices_with_record(&headless, &record).contains(option),
        "control: with no dialog question above it, a recorded line IS claimed — so the \
         refusal below is `dialog_header_above` and not an accident"
    );

    let dialog =
        format!("? Do you want to proceed with this command\n❯ {option}\n  2. No\n─────────\n❯");
    let masked = mask_loomux_notices_with_record(&dialog, &record);
    assert!(
        masked.contains(option),
        "the dialog's highlighted choice survives the mask: {masked:?}"
    );
    let m = prompt_wait_match(&masked).expect("and the detector still sees the dialog");
    assert!(
        question_shown(Some(&m), Some(Composed::plain(&masked))),
        "…so the Enter is still withheld from a live question (#420)"
    );
}

/// A finished turn whose prose satisfies the detector, with **no composer row of
/// its own** (#871's tests).
///
/// The captured false-positive fixtures all end in an idle `❯`, which is what
/// makes them false positives — and which would make every assertion below pass
/// on the fixture's own composer rather than on the paste reading under test.
/// This screen is built without one on purpose, so `idle_prompt_row_rendered` can
/// only ever be true because of the row the test appends.
fn prose_without_a_composer() -> String {
    "⏺ Rebased and pushed. The reviewer's round-2 note asks: do you want to proceed\n  \
     with the merge, or hold for the human? I have not, pending that call."
        .to_string()
}

#[test]
fn j4_a_collapsed_paste_is_still_our_own_text_at_the_pre_enter_checkpoint() {
    // #871's shape, and the one that turns a hold into a STRAND. The pre-paste
    // checkpoint releases (the composer is empty, so `idle_row` is true), the
    // paste lands, and the CLI collapses it to a placeholder — at which point
    // `mask_own_paste` has nothing to match, `idle_row` flips to false on an
    // otherwise unchanged screen, and the Enter is withheld from a visibly idle
    // pane. The audit recorded exactly that flip: `idle_row:true` at the drainer
    // gate, `idle_row:false` at `stage:"pre-enter"`.
    let brief = "Rebase onto main and re-read the findings.\nThen report when CI is green.";
    let transcript = prose_without_a_composer();

    // Precondition: this screen's ring matches, and NOT on a pointer — this test
    // is about the paste reading, so the screen must not also carry `j1`'s signal.
    let m = prompt_wait_match(&strip_ansi(transcript.as_bytes()))
        .expect("precondition: question-shaped prose the detector fires on");
    assert_ne!(
        m.needle,
        QuestionNeedle::LeadingPointer,
        "precondition: by a TOKEN signal, so this test isolates the paste reading"
    );

    let compose = |composer: &str, pasted: Option<&str>| {
        let raw = pty_bytes_903(&format!("{transcript}\n{composer}"));
        let with_paste =
            loomux_lib::orchestration::termgrid::render_visible(&raw, ECHO_COLS, ECHO_ROWS);
        let masked = match pasted {
            Some(p) => mask_own_paste(&with_paste, p),
            None => with_paste.clone(),
        };
        (with_paste, masked)
    };

    // Control 1: the CLI echoed our brief literally. That path has always worked,
    // and it fixes what "recognised" means here.
    let (echoed_vis, echoed_masked) = compose(
        "❯ Rebase onto main and re-read the findings.\n❯ Then report when CI is green.",
        Some(brief),
    );
    assert!(
        idle_prompt_row_rendered(Composed {
            masked: echoed_masked.as_str(),
            with_paste: echoed_vis.as_str()
        }),
        "control: a literally-echoed paste is recognised as ours"
    );

    // Control 2: an ORDINARY composer row that is neither empty nor ours reads
    // false — so the assertion below is about the placeholder and not about the
    // screen simply having a chevron on it.
    let (other_vis, other_masked) = compose("❯ something the human typed", Some(brief));
    assert!(
        !idle_prompt_row_rendered(Composed {
            masked: other_masked.as_str(),
            with_paste: other_vis.as_str()
        }),
        "control: a composer holding somebody else's text is not evidence of ours"
    );

    // The case: the CLI collapsed our paste instead of echoing it.
    let (with_paste, masked) = compose("❯ [Pasted text #1 +6 lines]", Some(brief));
    let c = Composed { masked: masked.as_str(), with_paste: with_paste.as_str() };
    assert!(
        idle_prompt_row_rendered(c),
        "the placeholder row is OUR paste — the CLI took our bytes into its composer, which \
         is the evidence, not a shape we invented: {with_paste:?}"
    );
    assert!(
        idle_prompt_rendered(c),
        "…and with no menu structure on this screen that is a release, so the Enter goes"
    );

    let raw2 = pty_bytes_903(&format!("{transcript}\n❯ [Pasted text #1 +6 lines]"));
    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&raw2, ECHO_COLS, ECHO_ROWS),
        Some(brief.to_string()),
        None,
        Vec::new(),
    );
    assert!(!pred(), "so the pre-Enter gate releases instead of stranding the paste (#871)");
}

#[test]
fn j5_a_placeholder_with_no_matching_delivery_is_not_ours() {
    // The correspondence term. A placeholder row is claimed only where THIS
    // delivery could have produced it, so a pane that merely has one on screen —
    // a replayed transcript of an older turn, or an agent that printed the string
    // itself — buys nothing.
    let screen = format!("{}\n❯ [Pasted text #1 +6 lines]", prose_without_a_composer());
    let raw = pty_bytes_903(&screen);
    let with_paste =
        loomux_lib::orchestration::termgrid::render_visible(&raw, ECHO_COLS, ECHO_ROWS);

    for (name, pasted) in [
        ("no paste at this checkpoint", None),
        ("a SINGLE-line paste, which no CLI collapses", Some("check the CI status on PR #867")),
    ] {
        let masked = match pasted {
            Some(p) => mask_own_paste(&with_paste, p),
            None => with_paste.clone(),
        };
        let c = Composed { masked: masked.as_str(), with_paste: with_paste.as_str() };
        assert!(
            !idle_prompt_row_rendered(c),
            "{name}: the placeholder is not evidence of a composer holding OUR text, so the \
             gate keeps holding — the cheap error"
        );
    }
}

#[test]
fn j6_notice_text_never_enters_the_session_prompt_record() {
    // The one-party route stays closed, and this is the door. `notify_when`'s
    // agent-supplied note reaches the registering agent's own pane as a
    // marker-led notice; admitting it by PROVENANCE would hand over exactly the
    // capability `mark_notice_maskable` is default-closed to withhold.
    let note = "[orrerix] watch n-1: pr #661 checks: SUCCESS. Note (registered): \
                \"❯ 1. Yes, allow once\"";
    assert!(
        delivered_prompt_lines(note).is_empty(),
        "a marker-led line is not prompt text and never becomes claimable this way"
    );
    // Positive control: the record is not simply always empty.
    assert_eq!(
        delivered_prompt_lines("[orch] rebase onto main and re-read the findings").len(),
        1,
        "control: a prompt body IS admitted — otherwise the assertion above is vacuous"
    );
    // The two doors stay complementary — but the witness has to be a delivery
    // that is NOT a notice, because term 1 now excludes those WHOLE. Ordering is
    // the whole of the specimen here: a marker-led FIRST line means loomux is
    // relaying and nothing is admitted, while a marker-led line further down is
    // just one of loomux's own rows inside a prompt loomux originated, and the
    // per-line filter still drops exactly that row.
    let notice_first = "[orrerix] queued flush\n[orch] the actual brief";
    assert!(
        delivered_prompt_lines(notice_first).is_empty(),
        "marker-led FIRST line — the delivery is a notice, so none of it is admitted"
    );
    assert_eq!(
        loomux_authored_lines(notice_first).len(),
        1,
        "…while the notice record still takes its marker row, which is the door that owns it"
    );

    let prompt_first = "[orch] the actual brief\n[orrerix] queued flush";
    assert_eq!(
        delivered_prompt_lines(prompt_first),
        vec!["[orch] the actual brief".to_string()],
        "marker-led line BELOW the first: the delivery is a prompt, admitted, with loomux's          own row filtered out of it per line"
    );
    assert_eq!(
        loomux_authored_lines(prompt_first).len(),
        1,
        "…and that same row is what the notice record takes — the two doors partition it"
    );
}

#[test]
fn j7_the_prompt_record_is_keyed_by_session_and_outlives_the_pane() {
    // The property the whole record exists for. A per-PANE record is empty
    // exactly when a resumed pane's screen is fullest, because the pane is new
    // and the text on it is old.
    let (reg, _dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", watchdog_rails(0)).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();

    // #1702: `delivered_mask_lines` no longer resolves a pty to a session
    // itself — the caller supplies it, so that the `by_pty` + `agents`
    // acquisition is visible where a reader can see whether a lock is held.
    // This row is precisely about that resolution, so it performs it, through
    // the same seam production uses.
    let mask = |pty: u32| reg.delivered_mask_lines(pty, reg.session_for_pty(pty).as_deref());
    let session = "39b611be-f2af-4316-8efd-2917faf8f790";
    let brief = "[orch] Round 3 (cap) re-record for #1429 at new head 82875938";

    let first = reg.spawn_agent(&g.id, Role::Reviewer, "rev", "review", false, None).unwrap();
    reg.set_session_for_test(&first.id, session);
    reg.set_pty_for_test(&first.id, 101);
    reg.record_delivered_prompt(101, brief, Delivery::MidSession);
    assert!(
        mask(101).iter().any(|l| l == brief),
        "the pane it was delivered into can see it"
    );

    // The resume: a NEW agent in a NEW pane, replaying the SAME session.
    let resumed = reg.spawn_agent(&g.id, Role::Reviewer, "rev2", "review", false, None).unwrap();
    reg.set_session_for_test(&resumed.id, session);
    reg.set_pty_for_test(&resumed.id, 102);
    assert!(
        mask(102).iter().any(|l| l == brief),
        "and so can the pane that replays it — which the per-pane notice record cannot do"
    );
    assert!(
        reg.delivered_notice_lines(102).is_empty(),
        "control: the per-PANE record is empty here, so the line above came from the session \
         record and not from the one #661 already had"
    );

    // #903 review B1, at the registry door rather than only in the predicate:
    // a refused KIND writes nothing, so the two terms are wired and not merely
    // written. Without this, changing the recorder's signature is the only thing
    // that would have caught a caller passing the wrong kind — which is exactly
    // what the compiler had to tell me on the round that added it.
    reg.record_delivered_prompt(101, "[orch] a regrounding-borne line", Delivery::Regrounding);
    assert!(
        !mask(101).iter().any(|l| l.contains("regrounding-borne")),
        "a Regrounding delivery contributes nothing, however ordinary its text looks"
    );

    // A different session sees nothing: the key is doing work.
    let other = reg.spawn_agent(&g.id, Role::Reviewer, "rev3", "review", false, None).unwrap();
    reg.set_session_for_test(&other.id, "9038509b-0000-0000-0000-000000000000");
    reg.set_pty_for_test(&other.id, 103);
    assert!(
        mask(103).is_empty(),
        "a pane resuming some OTHER session inherits nothing"
    );
}

#[test]
fn j8_a_replayed_notice_row_is_still_a_notice_under_a_chevron() {
    // The complement of `j2`, and the other half of the resume class. The session
    // record deliberately excludes marker-led lines (`j6`), so a NOTICE loomux
    // delivered and the CLI replayed is not covered by it — it is covered by the
    // marker rule, which stopped recognising its own marker the moment a resumed
    // CLI painted a `❯` in front of the row. Fifteen of the thirty-nine
    // `pointer-option` holds in the group's audit log are exactly this shape.
    let notice = "[orrerix] pr #1408 checks: success — 5 of 6 checks passed (1 skipped)";

    // Precondition: as a plain row the marker rule has always claimed it, so the
    // assertion below is about the chevron and nothing else.
    assert!(
        !mask_loomux_notices(notice).contains("checks: success"),
        "precondition: the marker rule claims a marker-led row"
    );

    let replayed = format!("❯ {notice}");
    assert!(
        !mask_loomux_notices(&replayed).contains("checks: success"),
        "and it claims the same row when a resumed CLI replays it under a chevron"
    );

    // ONE row, still — the widening reads through the glyph, it does not become a
    // run-mask. A live dialog painted directly beneath a replayed notice must
    // survive, which is #420's objection to widening this rule at all.
    let with_dialog = format!("{replayed}\n? Do you want to proceed\n❯ 1. Yes\n  2. No");
    let masked = mask_loomux_notices(&with_dialog);
    assert!(
        masked.contains("1. Yes") && masked.contains("Do you want to proceed"),
        "the rows below it are untouched: {masked:?}"
    );
    let m = prompt_wait_match(&masked).expect("so the dialog is still a question");
    assert!(
        question_shown(Some(&m), Some(Composed::plain(&masked))),
        "…and still holds the Enter"
    );
}

#[test]
fn j9_a_granted_override_carries_its_enter_only_on_fresh_proof() {
    // B2's decision, which is the one term standing between "the override moves
    // the queue" and "the override strands a paste on a pane with a live dialog
    // on it". `question_override_admits` decides the PASTE minutes earlier; this
    // decides the ENTER at the moment it is pressed.
    //
    // Written in terms of `QUESTION_OVERRIDE_CONSECUTIVE_READS` throughout, so a
    // future edit to that constant cannot leave this test asserting a rule the
    // code no longer has.
    let n = QUESTION_OVERRIDE_CONSECUTIVE_READS as usize;
    let idle = QuestionReread { active: true, idle_prompt: true };
    let dialog = QuestionReread { active: true, idle_prompt: false };
    let cleared = QuestionReread { active: false, idle_prompt: false };

    // The one shape that does NOT admit, and the three that do.
    assert!(!dialog.admits(), "a gate still holding with no composer on screen is a dialog");
    assert!(idle.admits(), "…and one whose screen shows our own paste in the composer is not");
    assert!(cleared.admits(), "…nor is a gate that has simply gone clear since the abort");

    // Enough reads, and never fewer.
    assert!(
        !override_enter_admits(&[]),
        "no reads at all must not pass by omission — the caller that took none is the \
         one this term exists to catch"
    );
    assert!(
        !override_enter_admits(&vec![idle; n - 1]),
        "one short of the bar is short of the bar: a single reading of a composed screen \
         can catch a mid-redraw instant, and this one licenses an Enter"
    );
    assert!(override_enter_admits(&vec![idle; n]), "and at the bar, with every read idle, it carries");
    assert!(
        override_enter_admits(&vec![cleared; n]),
        "a pane that repainted itself clear between the abort and this re-read must not be \
         stranded for having got BETTER"
    );

    // EVERY read, not the last one and not a majority — pinned in both orders, so
    // an implementation that looked at either end would fail here.
    let mut first_bad = vec![idle; n];
    first_bad[0] = dialog;
    let mut last_bad = vec![idle; n];
    last_bad[n - 1] = dialog;
    assert!(
        !override_enter_admits(&first_bad),
        "a dialog on the FIRST read is disqualifying, however the rest read"
    );
    assert!(
        !override_enter_admits(&last_bad),
        "and so is one on the LAST — there is no memory of having been eligible"
    );

    // A longer run does not dilute it either: the rule is universal, not a count.
    let mut long_run = vec![idle; n + 3];
    long_run[2] = dialog;
    assert!(
        !override_enter_admits(&long_run),
        "one dialog read anywhere in a longer run still refuses the Enter"
    );
}

#[test]
fn j10_a_collapsed_paste_under_a_dialog_header_is_not_claimed() {
    // The term the OPERATOR-SET rule found unpinned. `j2`/`j3`/`j5` between them
    // cover the collapsed-paste claim's shape term and its multi-line term, and
    // `j3` covers `dialog_header_above` on the RECORD path — but the same guard
    // on the collapsed-paste claim in `mask_own_paste` was pinned by nothing, so
    // "this claim is bounded by a dialog header" was a residual derived from
    // deletion mutations that never touched it.
    //
    // It matters in the one direction that costs something: if a dialog really is
    // up, a placeholder row inside its option block must not be read as "the
    // composer is holding our paste", because that reading is what releases the
    // Enter.
    let brief = "Rebase onto main and re-read the findings.\nThen report when CI is green.";
    let placeholder = "❯ [Pasted text #1 +6 lines]";

    let read = |screen: &str| {
        let raw = pty_bytes_903(screen);
        let with_paste =
            loomux_lib::orchestration::termgrid::render_visible(&raw, ECHO_COLS, ECHO_ROWS);
        let masked = mask_own_paste(&with_paste, brief);
        (with_paste, masked)
    };

    // CONTROL: the same option block with no question row heading it. The claim
    // IS made, so the refusal below is the guard and not the screen.
    let (open_vis, open_masked) =
        read(&format!("a finished turn's last line\n  $ git push --force\n{placeholder}"));
    assert!(
        idle_prompt_row_rendered(Composed {
            masked: open_masked.as_str(),
            with_paste: open_vis.as_str()
        }),
        "control: with no dialog question above it, the placeholder is claimed as ours"
    );

    // The case: a dialog's own question row heads the block.
    let (dlg_vis, dlg_masked) = read(&format!(
        "? Do you want to proceed with this command\n  $ git push --force\n{placeholder}"
    ));
    assert!(
        !idle_prompt_row_rendered(Composed {
            masked: dlg_masked.as_str(),
            with_paste: dlg_vis.as_str()
        }),
        "a placeholder inside a live dialog's option block is not evidence of an idle \
         composer: {dlg_vis:?}"
    );
}

#[test]
fn j11_a_loomux_notice_contributes_nothing_to_the_prompt_record() {
    // B1 TERM 1, and the hole it closes. Filtering marker-led lines one at a
    // time is not the same rule as excluding notices: a notice is one marker-led
    // first line followed by a body that is not, so every continuation row was
    // entering the prompt record — and the body of the two re-grounding notices
    // is the agent's OWN directive ledger, written raw by
    // `note_directive(replace: true)` and pasted back into that same agent's
    // pane on a self-callable `request_compact`. One party, on demand.
    let ledger_notice = "[orrerix] Context was compacted. Re-grounding you in your role \
                         instructions before you continue.\n\
                         Your directive ledger:\n\
                         ? Do you want to proceed with this command\n\
                         1. Yes, and do not ask again for git commit commands in this project";

    // Precondition — this text really is multi-line and really does carry rows a
    // per-line filter would have admitted, or the assertion below is vacuous.
    assert!(
        ledger_notice.lines().skip(1).any(|l| !l.trim().is_empty()),
        "precondition: the notice has a body below its marker-led first line"
    );
    let body_only = ledger_notice.lines().skip(1).collect::<Vec<_>>().join("\n");
    assert_eq!(
        delivered_prompt_lines(&body_only).len(),
        3,
        "precondition: those body lines are EXACTLY what the per-line filter admitted, so the \
         assertion below is about the notice's first line excluding them"
    );

    assert!(
        delivered_prompt_lines(ledger_notice).is_empty(),
        "a delivery whose FIRST non-empty line is a loomux notice contributes NOTHING — \
         not its marker row, and not one line of the body it is relaying"
    );

    // The same body, delivered as an ordinary prompt, IS admitted — so the
    // exclusion is keyed on the delivery being a notice, not on the body's
    // content, and this test cannot pass by the record being empty for everyone.
    let as_a_prompt = "[orch] please re-read the findings\nthen report when CI is green";
    assert_eq!(
        delivered_prompt_lines(as_a_prompt).len(),
        2,
        "control: an orchestrator's own multi-line prompt still enters the record whole"
    );
}

#[test]
fn j12_the_delivery_kind_gate_is_default_closed() {
    // B1 TERM 2. Independent of term 1, and neither is redundant: `ResumeKickoff`
    // carries BOTH an orchestrator brief (admitted here) and, at the
    // promoted-orchestrator call site, `resume_kickoff_notice`'s ledger embed —
    // which only term 1 refuses.
    assert!(
        !prompt_record_admits_kind(Delivery::Regrounding),
        "the post-compact re-grounding delivery exists to paste the agent's own ledger"
    );
    for kind in [Delivery::FreshKickoff, Delivery::ResumeKickoff, Delivery::MidSession] {
        assert!(
            prompt_record_admits_kind(kind),
            "{kind:?} carries a brief the orchestrator wrote, and ResumeKickoff in particular \
             is the incident's OWN payload — refusing it would close the door by regressing #903"
        );
    }

    // The set, not the list: exactly one variant is refused. A fifth variant
    // added later fails the `match` at compile time AND this count, so it cannot
    // acquire admission by being forgotten in either place.
    let all = [
        Delivery::FreshKickoff,
        Delivery::ResumeKickoff,
        Delivery::MidSession,
        Delivery::Regrounding,
    ];
    assert_eq!(
        all.iter().filter(|k| !prompt_record_admits_kind(**k)).count(),
        1,
        "exactly one delivery kind is refused; if that changed, the design note's argument did too"
    );
}

#[test]
fn j13_excluding_notices_does_not_uncover_their_own_wrapped_rows() {
    // The mirror risk of term 1, closed here rather than asserted in prose: if
    // the prompt record had been the only thing masking a notice's continuation
    // rows, excluding notices from it would trade a leak for a false-positive
    // gap on loomux's own wrapped text.
    //
    // It was not — but the honest scope is narrower than "the notice record
    // covers it", and the difference matters to anyone reading this as a
    // guarantee. A multi-row notice's wrap is masked by the NOTICE record
    // (#576/#661) only for a producer that OPTED IN via `mark_notice_maskable`,
    // which today is one call site: the orchestrator relay behind `report` /
    // `message_orchestrator`. The ledger-bearing notices do NOT opt in, so
    // nothing masks their wrap — they latch until `QuestionStale` badges it at
    // ten minutes, which is the pre-#903 fail-closed baseline and not a
    // regression term 1 introduced.
    //
    // What term 1 changed for them is the other direction: as first shipped it
    // ADMITTED their body to the prompt record, which is the hole B1 reported.
    // So "nothing is traded away" is a claim about coverage that existed BEFORE
    // this PR, not about coverage the round-1 code had.
    // The token must live on the CONTINUATION, not on the marker-led row: this
    // test is about what covers a notice's wrapped rows, and a fixture whose
    // token sits on row one is cleared by the marker rule alone — the claim below
    // would then hold for a reason that has nothing to do with the record. The
    // first version of this test had exactly that shape and its precondition
    // caught it.
    let notice = "[orrerix] w-7 reports blocked: the CLI is waiting on do you want to \
                  proceed (y/n) before it will run the suite";
    let wrapped = "[orrerix] w-7 reports blocked: the CLI is waiting on\n\
                   do you want to proceed (y/n) before it will run the suite";

    // Precondition — the continuation alone is what the detector fires on, so
    // masking only the first row would leave the pane latched.
    assert!(
        prompt_wait_match(&mask_loomux_notices(wrapped)).is_some(),
        "precondition: the marker rule alone leaves this notice's continuation matching"
    );

    // With the NOTICE record — the mechanism that actually covers this — the run
    // masks whole and the gate reads clear.
    let record = loomux_authored_lines(notice);
    assert_eq!(record.len(), 1, "precondition: the notice is one recordable line");
    assert!(
        prompt_wait_match(&mask_loomux_notices_with_record(wrapped, &record)).is_none(),
        "for an OPTED-IN producer the notice record masks the whole wrap run — that is \
         #661's coverage, it is one call site today, and term 1 does not touch it"
    );

    // And term 1 confirms the prompt record was never the mechanism: this notice
    // cannot enter it at all.
    assert!(
        delivered_prompt_lines(wrapped).is_empty(),
        "so nothing is traded away relative to the pre-#903 baseline. Round 1's code DID \
         hold these rows — that was B1 — and taking them back out restores the baseline \
         rather than removing coverage this PR had shipped"
    );
}

#[test]
fn j14_a_claimed_question_row_still_vetoes_the_row_below_it() {
    // B2. The upward scan used to consult `keep` BEFORE testing for a header, so
    // a row that had already been claimed was stepped over — and two recorded
    // lines were therefore enough to walk the scan past the very question row it
    // vetoes on. `j3` could not see it: its record holds ONE line, so nothing
    // above the option row was ever claimed.
    let header = "? Do you want to proceed with this command";
    let option = "1. Yes, and do not ask again for git commit commands in this project";
    let screen = format!("{header}\n❯ {option}\n  2. No\n─────────\n❯");

    // ONE recorded line — `j3`'s shape. The header is not claimed, so it vetoes.
    let single = vec![option.to_string()];
    assert!(
        mask_loomux_notices_with_record(&screen, &single).contains(option),
        "precondition (j3's case): with the header unclaimed, the option survives"
    );

    // TWO recorded lines, chained: the first claims the header row itself.
    let chained = vec![header.to_string(), option.to_string()];
    let masked = mask_loomux_notices_with_record(&screen, &chained);
    assert!(
        masked.contains(option),
        "a claimed question row still vetoes the claim below it — the mask must not be \
         allowed to decide its own bound: {masked:?}"
    );
    let m = prompt_wait_match(&masked).expect("and the dialog is still a question");
    assert!(
        question_shown(Some(&m), Some(Composed::plain(&masked))),
        "…so the Enter is still withheld (#420)"
    );

    // The control that keeps this honest: the chain's FIRST line really is
    // claimable, so the assertion above is the veto working and not the header
    // simply failing to match.
    let header_only = format!("{header}\nsome ordinary transcript row");
    assert!(
        !mask_loomux_notices_with_record(&header_only, &chained).contains(header),
        "control: with nothing below it to protect, the recorded header row IS claimed"
    );
}

#[test]
fn j15_a_coalesced_flush_contributes_its_constituents_and_not_its_framing() {
    // Review round 2, B1'. Term 1 excludes a delivery whose first non-empty line
    // is marker-led — and a coalesced flush's first line is exactly that, so the
    // framed whole took its constituent PAYLOADS out of the record with it. Those
    // payloads are the orchestrator-authored briefs #903 needs masked, so the
    // exclusion was fail-closed but a real coverage regression.
    //
    // The split is not inferred from the text here or in production: #632 already
    // owns it, the drainer already has the parts separately, and the fix passes
    // them down as `record_contributions` rather than asking the record to parse
    // a flush apart. This test builds the REAL flush through
    // `queue::coalesced_flush_text` — nothing in the suite did before — and pins
    // all three halves of the rule.
    let brief = "[orch] Round 3 (cap) re-record for #1429 at new head 82875938";
    let ledger_notice = "[orrerix] Context was compacted. Re-grounding you in your role \
                         instructions.\nYour directive ledger:\n1. Yes, allow once";
    let items = [
        queue::FlushConstituent {
            id: 7,
            from: "orch-1156",
            enqueued_ms: 1_000,
            coalesced: 0,
            text: brief,
        },
        queue::FlushConstituent {
            id: 8,
            from: "orrerix",
            enqueued_ms: 2_000,
            coalesced: 0,
            text: ledger_notice,
        },
    ];
    let rendered = queue::coalesced_flush_text(&items, 0, 9_000, queue::FlushCause::PaneBlocked);

    // Precondition 1 — this really is the shape term 1 refuses.
    assert!(
        delivered_prompt_lines(&rendered).is_empty(),
        "the FRAMED whole contributes nothing — which is why the drainer must hand the record \
         the constituents instead of this string: {rendered:?}"
    );
    // Precondition 2 — and the payload rows really are rendered inside it, so
    // recording them is a statement about rows that are on the pane's screen.
    assert!(rendered.contains(brief), "precondition: the brief is rendered verbatim in the flush");

    // #632's own invariant, reused rather than restated: everything loomux FRAMED
    // is maskable and the only survivors are payload rows. The fix rests on that
    // split, so the test asserts it holds for this fixture instead of trusting it.
    let payloads: Vec<&str> = items.iter().map(|c| c.text).collect();
    assert!(
        unmaskable_framing_rows(&rendered, &payloads).is_empty(),
        "precondition (#632): every framing row this flush emits is maskable"
    );

    // The rule, per constituent. The brief is admitted…
    assert_eq!(
        delivered_prompt_lines(brief),
        vec![brief.to_string()],
        "an orchestrator-authored constituent enters the record on its own merits"
    );
    // …and the ledger-bearing notice is NOT, however it is flushed alongside.
    // This is the half that keeps B1 closed: constituents are admitted
    // individually, so riding in a batch buys a notice nothing.
    assert!(
        delivered_prompt_lines(ledger_notice).is_empty(),
        "a re-grounding notice riding in the same batch is still refused by its own first line"
    );
    // And by kind, independently — either term alone refuses it.
    assert!(!prompt_record_admits_kind(Delivery::Regrounding));
}

#[test]
fn j16_the_record_takes_each_entrys_own_text_never_the_framed_paste() {
    // B1', at the decision rather than at the rule. `j15` pins what
    // `delivered_prompt_lines` does with a flush; this pins what the drainer
    // HANDS it, which is the half that regressed — and the half no test could
    // reach while the decision was welded into a function needing an `AppHandle`
    // (the same argument `override_enter_admits` carries).
    let entry = |id: u64, text: &str, kind| queue::QueuedDelivery {
        id,
        agent_id: "rev-1277".into(),
        from: "orch-1156".into(),
        payload: queue::QueuedPayload::Text(text.to_string()),
        reason: queue::EnqueueReason::Question,
        enqueued_ms: 1_000 + id,
        coalesced: 0,
        group: Some("g-1".try_into().unwrap()),
        to_orchestrator: false,
        session_id: None,
        delivery_kind: kind,
    };

    let brief = "[orch] Round 3 (cap) re-record for #1429 at new head 82875938";
    let ledger_notice = "[orrerix] Context was compacted.\nYour directive ledger:\n1. Yes, allow once";

    // A FLUSH: every constituent contributes, under its own kind.
    let batch = vec![
        entry(7, brief, Delivery::ResumeKickoff),
        entry(8, ledger_notice, Delivery::Regrounding),
    ];
    let got = record_contributions_for(&batch);
    assert_eq!(
        got,
        vec![
            (brief.to_string(), Delivery::ResumeKickoff),
            (ledger_notice.to_string(), Delivery::Regrounding),
        ],
        "both constituents are handed over as their OWN texts under their OWN kinds — the \
         framing is never among them, and the second entry's kind is what refuses it \
         downstream rather than anything about the batch it rode in"
    );

    // A LONE delivery is the same rule, not a special case — which matters
    // because `header_pending` can prepend a flush header to a single delivery,
    // and recording the framed string would lose it exactly as a flush lost its
    // constituents.
    let lone = vec![entry(9, brief, Delivery::MidSession)];
    assert_eq!(
        record_contributions_for(&lone),
        vec![(brief.to_string(), Delivery::MidSession)],
        "one entry, its own text, its own kind"
    );

    // A marker entry carries no text and contributes nothing — the `filter_map`
    // is load-bearing, not defensive.
    let marker = vec![queue::QueuedDelivery {
        payload: queue::QueuedPayload::StrandedSubmit,
        ..entry(10, "", Delivery::MidSession)
    }];
    assert!(
        record_contributions_for(&marker).is_empty(),
        "a StrandedSubmit marker has no payload text to contribute"
    );
}


// ═══════════════════════════════════════════════════════════════════════════
// The human-question registry (#946 slice Q1)
//
// The incident these exist for: a blocking orchestrator question halts the
// whole fleet, because a pane showing its CLI's own interactive dialog cannot
// take ANY delivery. `ask_human` makes asking asynchronous — and the property
// that makes the feature worth anything, tested hardest below, is that an
// agent can ASK but can never ANSWER.
// ═══════════════════════════════════════════════════════════════════════════
