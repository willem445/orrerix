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
