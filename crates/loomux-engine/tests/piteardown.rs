//! The pi pane's process teardown, against a real short-lived child.
//!
//! **Not a constraint-3 violation.** The rule is never to spawn a real AGENT
//! CLI (`claude`, `copilot`, `pi`) to test or demo anything, because that burns
//! the human's credits. These children are `cmd.exe` / `sh` exiting
//! immediately
//! — the same thing `src-tauri/tests/direct_spawn.rs` already does — and no
//! agent, model or token is anywhere near them.
//!
//! The property under test is the REAP (#3067 item 2). `Child::kill` signals
//! and returns; without a `wait` the process-table entry survives until orrerix
//! exits. An exit status can be obtained in exactly one way — by waiting for it
//! — so `Departure` carrying one IS the evidence the child was collected, and
//! deleting the `wait` makes it unproducible.

use std::process::{Command, Stdio};
use std::time::Duration;

use loomux_engine::harness::pi::{shutdown_child, Departure};

/// A child that exits on its own, immediately, with a code we choose.
fn quick_exit(code: i32) -> Command {
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd.exe");
        c.args(["/c", &format!("exit {code}")]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", &format!("exit {code}")]);
        c
    };
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    c
}

/// A child that ignores the grace entirely, so the kill path is the one taken.
fn sleeper() -> Command {
    let mut c = if cfg!(windows) {
        // `pause` blocks on stdin, which is piped and never written.
        let mut c = Command::new("cmd.exe");
        c.args(["/c", "pause"]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 30"]);
        c
    };
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    c
}

#[test]
fn a_child_that_leaves_on_its_own_is_collected_on_the_graceful_path() {
    let mut child = quick_exit(7).spawn().expect("spawn");
    let d = shutdown_child(&mut child, Duration::from_secs(5));

    // The status is the receipt: only a wait can produce one.
    match d {
        Departure::Graceful(code) => assert_eq!(
            code,
            Some(7),
            "the graceful path reported a status it did not read from the child"
        ),
        other => panic!("a child that exits immediately must leave gracefully, got {other:?}"),
    }
}

#[test]
fn a_child_that_overstays_is_killed_and_then_collected() {
    let mut child = sleeper().spawn().expect("spawn");
    let pid = child.id();

    // A grace short enough that the sleeper cannot possibly beat it, so this
    // test pins the KILL path rather than racing the graceful one.
    let d = shutdown_child(&mut child, Duration::from_millis(50));

    assert!(
        matches!(d, Departure::Killed(_)),
        "a child that outlives its grace must take the kill path, got {d:?}"
    );
    assert_ne!(
        d,
        Departure::Unreaped,
        "the child was killed but never collected — this is the zombie #3067 item 2 is about"
    );

    // `try_wait` is NOT the discriminating observation and is not used as one:
    // a killed-but-unreaped child answers `Ok(Some(_))` there too, because the
    // process really has exited. The only place the difference is visible is
    // the OS process table.
    //
    // Linux is where this test can actually look. `/proc/<pid>` survives a
    // kill for exactly as long as nobody has collected the child, with state
    // `Z`; after the reap the entry is gone. So this assertion FAILS if the
    // `wait` in `shutdown_child` is deleted, which is the whole point of
    // #3067 item 2 — and it is a real check rather than a restatement of the
    // return value.
    //
    // Residual, stated rather than papered over: Windows and macOS have no
    // equally cheap observation, so on those platforms the reap rests on
    // `Departure::Killed` being unproducible without a successful `wait`.
    // Linux is in the CI matrix, so the property is covered there on every
    // run.
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        if let Ok(stat) = stat {
            // `comm` can itself contain spaces, so the state field is read
            // after the LAST ")" rather than by splitting on whitespace.
            let after = stat.rsplit_once(')').map(|(_, r)| r.trim()).unwrap_or("");
            let state = after.split_whitespace().next().unwrap_or("");
            assert_ne!(
                state, "Z",
                "the killed child is a ZOMBIE — shutdown_child did not reap it (#3067 item 2)"
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
}

#[test]
fn the_grace_is_a_bound_and_not_a_sleep() {
    // A child that exits at once must not cost the caller the whole grace:
    // `PiPane::drop` runs on the teardown path of every structured pane, and a
    // 2 s sleep per pane would be felt when a group shuts down.
    let mut child = quick_exit(0).spawn().expect("spawn");
    let t0 = std::time::Instant::now();
    let _ = shutdown_child(&mut child, Duration::from_secs(30));
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "shutdown_child waited out the grace for a child that had already left ({:?})",
        t0.elapsed()
    );
}
