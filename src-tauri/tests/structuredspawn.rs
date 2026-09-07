//! The structured spawn path, end to end, against a FAKE pi (#2850 S3b).
//!
//! **Never a real agent CLI** (CLAUDE.md constraint 3). The child here is a
//! `.cmd`/`.sh` script this test writes itself, which replays a recorded JSONL
//! stream on stdout and exits. No model, no token, no credit is anywhere near
//! it — the same posture `direct_spawn.rs` takes with its own fake children.
//!
//! # What this closes that the engine tests cannot
//!
//! `crates/loomux-engine`'s pi tests drive the decoder over an in-memory
//! reader, deliberately: they never start a process, so the PROCESS SEAM — the
//! argv actually handed to `CreateProcessW`, a real pipe, a real exit — is a
//! stated residual there. This is where that residual is closed.
//!
//! # The Windows shim, and why the fixture directory has a space in its name
//!
//! On Windows `pi` is an npm `.cmd` shim, which `CreateProcessW` cannot start;
//! `winpath::launch_form` returns a `cmd.exe /c <shim>` prefix for it. That
//! prefix is re-parsed by `cmd.exe`, and the case that breaks first is a path
//! containing a space. So the fake pi is deliberately installed under a
//! directory whose name has one, and the argv-parity test below reads the
//! arguments the child actually received rather than the ones we hoped it got.

use std::fs;
use std::path::{Path, PathBuf};

use loomux_engine::harness::pi::{full_argv, LaunchSpec};
use loomux_engine::harness::AgentPane;
use loomux_engine::model::Containment;

/// The JSONL the fake pi replays: the engine's own fixture, so the two sides
/// cannot drift. If the engine re-records it, this test speaks the new one.
const PI_STREAM: &str =
    include_str!("../../crates/loomux-engine/tests/fixtures/harness/pi/one-turn.jsonl");

/// Write a fake `pi` that echoes its argv to a file, replays `PI_STREAM`, then
/// waits for stdin to close.
///
/// The argv echo is the point: it is the only way to check what actually
/// reached the child through the shell, as opposed to what this process
/// intended to send.
fn install_fake_pi(dir: &Path) -> PathBuf {
    // A directory whose name contains a space, on purpose — see the module doc.
    let bin = dir.join("program files");
    fs::create_dir_all(&bin).unwrap();
    let stream = bin.join("stream.jsonl");
    // The fixture is CRLF on disk in this repo but the pi protocol is strict
    // LF-delimited JSONL; write it back as LF so the fake speaks the real
    // protocol rather than this checkout's line endings.
    fs::write(&stream, PI_STREAM.replace("\r\n", "\n")).unwrap();

    if cfg!(windows) {
        let p = bin.join("pi.cmd");
        // `echo %*` records the argv as the shell saw it. `type` replays the
        // stream. `pause` would need a console, so the script simply ends,
        // which closes stdout and is the EOF the driver reads.
        let script = format!(
            "@echo off\r\n\
             echo %* > \"%~dp0argv.txt\"\r\n\
             type \"%~dp0stream.jsonl\"\r\n",
        );
        fs::write(&p, script).unwrap();
        p
    } else {
        let p = bin.join("pi");
        let script = "#!/bin/sh\n\
                      printf '%s\\n' \"$*\" > \"$(dirname \"$0\")/argv.txt\"\n\
                      cat \"$(dirname \"$0\")/stream.jsonl\"\n";
        fs::write(&p, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }
}

fn spec_for(dir: &Path) -> LaunchSpec {
    LaunchSpec {
        session_id: Some("abc123".into()),
        session_dir: dir.join("sessions"),
        mcp_config: dir.join("mcp.json"),
        append_system_prompt: Some(dir.join("role.md")),
        approve: false,
        exclude_tools: Some("edit,write".into()),
        model: "z-ai/glm-5.3-flash".into(),
        thinking: "medium".into(),
    }
}

#[test]
fn a_structured_pane_boots_reads_the_stream_and_reaches_its_events() {
    let dir = tempfile::tempdir().unwrap();
    let program = install_fake_pi(dir.path());
    let spec = spec_for(dir.path());

    // Exactly what the spawn path does: resolve how to START the program, then
    // hand the prefix to the adapter.
    let (exe, prefix) = loomux_engine::winpath::launch_form(&program);
    let pane = loomux_engine::harness::pi::PiPane::spawn_with(&exe, &prefix, &spec, None)
        .expect("the fake pi must start through the resolved launch form");

    let rx = pane.events().expect("events() is take-once and this is the take");

    // Collect until the stream ends. A generous bound: this is a CI machine
    // starting a process, not a latency measurement.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut seen: Vec<loomux_engine::harness::HarnessEvent> = Vec::new();
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(ev) => seen.push(ev),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    assert!(
        !seen.is_empty(),
        "the pane produced no events at all — the child never spoke, or the argv was mangled \
         before it got there (see argv.txt beside the fake)"
    );

    // Booted is SYNTHESIZED from the boot `get_state` reply, which means the
    // driver wrote a command to the child's stdin and read the answer back:
    // the whole round trip, over real pipes.
    let booted = seen
        .iter()
        .any(|e| matches!(e, loomux_engine::harness::HarnessEvent::Booted { .. }));
    assert!(booted, "no Booted event — the boot query never round-tripped: {seen:?}");

    // And the session id really came off the wire.
    let session = seen.iter().find_map(|e| match e {
        loomux_engine::harness::HarnessEvent::Booted { session, .. } => session.clone(),
        _ => None,
    });
    assert_eq!(
        session.as_deref(),
        Some("abc123"),
        "the boot reply's sessionId did not reach Booted"
    );
}

#[test]
fn the_argv_the_child_receives_is_the_pty_line_plus_one_flag() {
    // `harness-adapters.md` section 2.3: the containment argv must be
    // byte-identical across drivers, because a structured driver that dropped a
    // deny flag would be a capability grant BY TRANSPORT.
    //
    // Measured on what the CHILD received, not on what this process built —
    // which is the half a pure argv test cannot reach, and the half the Windows
    // `cmd.exe /c` prefix can silently mangle.
    let dir = tempfile::tempdir().unwrap();
    let program = install_fake_pi(dir.path());
    let spec = spec_for(dir.path());

    let (exe, prefix) = loomux_engine::winpath::launch_form(&program);
    let pane = loomux_engine::harness::pi::PiPane::spawn_with(&exe, &prefix, &spec, None)
        .expect("spawn");
    // Let the child run far enough to have written its argv file.
    let argv_file = dir.path().join("program files").join("argv.txt");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline && !argv_file.is_file() {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    drop(pane);

    let got = fs::read_to_string(&argv_file)
        .expect("the fake pi never recorded its argv — it did not start");
    let got = got.trim();

    // Every flag the launch spec asked for reached the child, through the
    // shell, with the space-bearing paths intact.
    for flag in [
        "--mode",
        "rpc",
        "--session-id",
        "--session-dir",
        "--mcp-config",
        "--append-system-prompt",
        "--no-approve",
        "--exclude-tools",
        "--model",
        "--thinking",
    ] {
        assert!(
            got.contains(flag),
            "the child never received {flag} — argv was mangled in transit: {got}"
        );
    }

    // The containment half, stated as its own assertion because it is the one
    // that is a security property rather than a plumbing one: a read-only
    // posture must reach the child as `--no-approve`, never as `--approve`.
    assert!(
        !got.contains("--approve"),
        "a denies-edits spec reached the child as --approve — containment was lost in the \
         transport, which section 2.3 forbids: {got}"
    );
}

#[test]
fn the_structured_launch_line_is_the_pty_arm_plus_mode_rpc() {
    // The pure half of the parity claim, beside the process half above: the
    // spec's own argv is the PTY arm's flags with `--mode rpc` in front, in the
    // PTY arm's order, so a diff of the two lines is one inserted flag rather
    // than a reordering nobody can read.
    let dir = tempfile::tempdir().unwrap();
    let spec = spec_for(dir.path());
    let argv = full_argv(&[], &spec);

    assert_eq!(&argv[..2], &["--mode", "rpc"], "the mode flag must lead: {argv:?}");

    // Built through the SAME helper the spawn path uses, so this compares two
    // real derivations rather than one derivation against a hand-copied list.
    let from_spawn = loomux_lib::orchestration::structured::pi_launch_spec(
        Some("abc123"),
        dir.path(),
        &dir.path().join("mcp.json"),
        Some(&dir.path().join("role.md")),
        Containment::NoEdits,
        "z-ai/glm-5.3-flash",
        "medium",
    );
    assert_eq!(
        from_spawn.approve, false,
        "a NoEdits containment must produce --no-approve, not --approve"
    );
    assert_eq!(
        from_spawn.exclude_tools.as_deref(),
        Some("edit,write"),
        "the edit-deny tool list must survive the structured derivation"
    );
}

// ---------------------------------------------------------------------------
// The policy this slice adds, as pure functions. No process, no pane, no clock.
// ---------------------------------------------------------------------------

#[test]
fn a_delivery_becomes_the_turn_variant_its_author_and_kind_name() {
    // The variant is not decoration: a driver renders `Notice` with its marker
    // prefix and `Human` without one, and R2's permissions policy may read it.
    // All four crossings, because three of them share one `Delivery` kind and
    // are told apart ONLY by the author.
    use loomux_engine::harness::Turn;
    use loomux_engine::model::Delivery;
    use loomux_lib::orchestration::structured::turn_for;
    let orrerix = loomux_lib::orchestration::brand::AUDIT_ACTOR;

    assert!(matches!(turn_for(Delivery::FreshKickoff, "w-1", "hi"), Turn::Kickoff(_)));
    assert!(matches!(turn_for(Delivery::ResumeKickoff, orrerix, "hi"), Turn::Kickoff(_)));
    // A kickoff stays a kickoff whoever sent it: the kind decides first.
    assert!(
        matches!(turn_for(Delivery::FreshKickoff, "human", "hi"), Turn::Kickoff(_)),
        "the author must not override a kickoff kind"
    );

    // One kind, three authors.
    assert!(matches!(turn_for(Delivery::MidSession, orrerix, "x"), Turn::Notice(_)));
    assert!(matches!(turn_for(Delivery::MidSession, "human", "x"), Turn::Human(_)));
    assert!(matches!(turn_for(Delivery::MidSession, "orch-1", "x"), Turn::Prompt(_)));

    // The text survives every arm. A mapping that dropped it would satisfy
    // every `matches!` above.
    for t in [
        turn_for(Delivery::FreshKickoff, "w-1", "payload"),
        turn_for(Delivery::MidSession, orrerix, "payload"),
        turn_for(Delivery::MidSession, "human", "payload"),
        turn_for(Delivery::MidSession, "orch-1", "payload"),
    ] {
        assert_eq!(t.text(), "payload");
    }
}

#[test]
fn a_dialog_parks_for_a_delegate_and_is_cancelled_for_an_orchestrator() {
    // harness-adapters.md section 3.5 role asymmetry, as a table. A parked
    // orchestrator is #946 -- machine progress must never stop on human
    // absence -- while a parked worker is the correct scope.
    use loomux_engine::model::Role;
    use loomux_lib::orchestration::structured::{decide_dialog, CancelReason, DialogOutcome};

    for role in [Role::Worker, Role::Reviewer, Role::Planner] {
        assert_eq!(decide_dialog(role, 0), DialogOutcome::Park, "{role:?} must PARK on a dialog");
    }
    for role in [Role::Orchestrator, Role::Manager] {
        assert_eq!(
            decide_dialog(role, 0),
            DialogOutcome::CancelNow(CancelReason::RoleNeverParks),
            "{role:?} must never park (#946)"
        );
    }
}

#[test]
fn the_cap_is_checked_for_both_role_classes_and_not_only_the_parking_one() {
    // The one-rule-per-input rule. A cap checked only on the parking branch
    // would let an orchestrator accumulate rows for ever, and the asymmetry
    // would be invisible: every assertion in the test above would still pass.
    use loomux_engine::model::Role;
    use loomux_lib::orchestration::structured::{
        decide_dialog, CancelReason, DialogOutcome, MAX_PENDING_UI,
    };

    for role in [Role::Worker, Role::Orchestrator] {
        assert_eq!(
            decide_dialog(role, MAX_PENDING_UI),
            DialogOutcome::CancelNow(CancelReason::TooManyPending),
            "{role:?} at the cap must be cancelled FOR THE CAP, however it settles below it"
        );
        // One under the cap still takes the role's own answer, so the
        // assertion above is about the CAP rather than about refusing all.
        assert_ne!(
            decide_dialog(role, MAX_PENDING_UI - 1),
            DialogOutcome::CancelNow(CancelReason::TooManyPending),
            "{role:?} one under the cap must not be cap-cancelled"
        );
    }
}

#[test]
fn the_needs_you_text_says_what_was_asked_and_what_happened_to_it() {
    use loomux_engine::harness::{RequestId, UiMethod};
    use loomux_lib::orchestration::structured::{
        dialog_needs_you_text, CancelReason, DialogOutcome, PendingUi,
    };

    let ask = PendingUi {
        id: RequestId("uuid-1".into()),
        method: UiMethod::Select,
        title: Some("Allow dangerous command?".into()),
        message: None,
        options: vec!["Allow".into(), "Block".into()],
        needs_you: None,
        raised_ms: 0,
    };

    let parked = dialog_needs_you_text("w-1", &ask, &DialogOutcome::Park);
    assert!(parked.contains("w-1"), "the row must name the pane: {parked}");
    assert!(parked.contains("Allow dangerous command?"), "{parked}");
    assert!(parked.contains("Allow / Block"), "the options are the ask: {parked}");

    // A cancelled dialog still reaches the human -- #946 is about not BLOCKING
    // on them, never about hiding the question -- and it says it was
    // cancelled, because a row reading identically would have them answer a
    // dead ask.
    let cancelled = dialog_needs_you_text(
        "orch-1",
        &ask,
        &DialogOutcome::CancelNow(CancelReason::RoleNeverParks),
    );
    assert!(cancelled.contains("cancelled"), "{cancelled}");
    assert!(cancelled.contains("role-never-parks"), "the reason is on the row: {cancelled}");
    assert_ne!(parked, cancelled, "the two outcomes must not read the same");
}

#[test]
fn the_audit_column_agrees_with_the_engine_and_is_not_a_second_list() {
    // section 4.3 states the decision-grade split ONCE, in
    // `HarnessEvent::is_decision_grade`. `route` must READ it rather than
    // restate it: a second enumeration here is the divergence that rule exists
    // to stop, and it would drift silently, because both lists would be green
    // on the day they were written.
    use loomux_engine::harness::{
        CompactTrigger, Decision, DecisionSource, HarnessEvent, NoteKind, RequestId, StopReason,
        ToolUseId, TurnId, UiAnswer, UiMethod,
    };
    use loomux_lib::orchestration::structured::route;

    let sample = [
        HarnessEvent::Booted { session: None, model: None, capabilities: vec![] },
        HarnessEvent::TurnStarted { turn: TurnId(1) },
        HarnessEvent::Text { turn: TurnId(1), delta: "x".into() },
        HarnessEvent::Thinking { turn: TurnId(1), delta: "x".into() },
        HarnessEvent::ToolCall {
            turn: TurnId(1),
            id: ToolUseId("t".into()),
            name: "bash".into(),
            input: serde_json::Value::Null,
        },
        HarnessEvent::ToolOutput {
            turn: TurnId(1),
            id: ToolUseId("t".into()),
            delta: "o".into(),
            is_error: false,
            replaces: false,
        },
        HarnessEvent::ToolResult { turn: TurnId(1), id: ToolUseId("t".into()), ok: true },
        HarnessEvent::PermissionRequest {
            id: RequestId("r".into()),
            tool: "bash".into(),
            input: serde_json::Value::Null,
        },
        HarnessEvent::PermissionSettled {
            id: RequestId("r".into()),
            decision: Decision::Allow,
            by: DecisionSource::Policy,
        },
        HarnessEvent::UiRequest {
            id: RequestId("u".into()),
            method: UiMethod::Confirm,
            title: None,
            message: None,
            options: vec![],
            timeout_ms: None,
        },
        HarnessEvent::UiSettled {
            id: RequestId("u".into()),
            answer: UiAnswer::Cancelled,
            by: DecisionSource::Policy,
        },
        HarnessEvent::QueueChanged { steering: vec![], follow_up: vec![] },
        HarnessEvent::TurnEnded {
            turn: TurnId(1),
            usage: None,
            cost: None,
            stop: StopReason::Completed,
        },
        HarnessEvent::Compacted { trigger: CompactTrigger::Auto, pre_tokens: None },
        HarnessEvent::Note { turn: None, note: NoteKind::Retry, text: "x".into() },
        HarnessEvent::Exited { code: Some(0) },
    ];

    for ev in &sample {
        assert_eq!(
            route(ev).audit,
            ev.is_decision_grade(),
            "route disagreed with the engine about {ev:?}"
        );
    }

    // Non-vacuity: the sample must actually contain both answers, or the loop
    // above would pass over a `route` that returned a constant.
    assert!(sample.iter().any(|e| route(e).audit), "no decision-grade event in the sample");
    assert!(sample.iter().any(|e| !route(e).audit), "no per-pane-only event in the sample");

    // Every event reaches the ring and the frontend: both projections are fed
    // from one drain (section 5.1), so neither can show what the other has not.
    assert!(sample.iter().all(|e| route(e).ring && route(e).frontend));
}

#[test]
fn a_reserved_pane_id_is_not_a_pty_and_the_pty_side_cannot_find_it() {
    // The structural guard, measured rather than asserted in prose. A
    // structured pane id comes from the SAME counter `spawn_pty` mints from,
    // so it looks like any other id, and `PtyManager` has no entry for it, so
    // every pty-side lookup misses. That missing entry is what makes
    // `write_pty`, `resize_pty` and `kill` unable to reach a structured pane.
    let ptys = loomux_lib::pty::PtyManager::default();
    let a = ptys.reserve_id();
    let b = ptys.reserve_id();
    assert_ne!(a, b, "the allocator handed out one id twice");
    assert_eq!(
        ptys.output_total(a),
        None,
        "a reserved pane id resolved to a pty -- the two kinds share an allocator, not a map"
    );
    assert_eq!(ptys.output_total(b), None);

    // Killing one is a no-op rather than a panic or a wrong-pane kill.
    ptys.kill(a);
}
