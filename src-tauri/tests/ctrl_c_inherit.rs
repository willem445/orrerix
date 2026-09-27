//! A pane's child must be able to take Ctrl+C however the app was launched
//! (#3595).
//!
//! `npx orrerix` launches the app with Node's `detached: true`, which on
//! Windows is `CREATE_NEW_PROCESS_GROUP`, and a process created that way starts
//! with its inheritable "ignore CTRL+C" attribute set. Every ConPTY child the
//! app then spawns inherits it, so ^C in a pane reaches ConPTY, ConPTY raises
//! CTRL_C_EVENT, and the running `npm run dev` ignores it.
//! `loomux_lib::pty::spawn_pane_child` clears the attribute before its
//! `CreateProcess`; this test drives that production path against a real
//! ConPTY slave, exactly as `spawn_pty` does, and reads the bit the child was
//! born with.
//!
//! THE INSTRUMENT. The attribute is `RTL_USER_PROCESS_PARAMETERS.ConsoleFlags`
//! bit 0 in the child's PEB — the flag `SetConsoleCtrlHandler(NULL, TRUE)` sets
//! and CreateProcess copies into a child. No documented API reads it back, so
//! the test reads the child's memory: the PEB address via
//! `NtQueryInformationProcess(ProcessBasicInformation)`, then
//! `PEB.ProcessParameters` (x64 offset 0x20) and `ConsoleFlags` (x64 offset
//! 0x18). Those two offsets are undocumented, which is why the test first
//! proves the reader on a child KNOWN to carry the bit (`CREATE_NEW_PROCESS_GROUP`,
//! documented to disable CTRL+C) — a reader pointed at the wrong bytes would
//! read "clear" everywhere and pass the real assertion vacuously.
//!
//! ONE test function, deliberately: the attribute is process-wide state, and
//! the phases below set it, observe it and have `spawn_pane_child` clear it.
//! Split across `#[test]`s they would race on it in parallel threads.
#![cfg(all(windows, target_pointer_width = "64"))]

use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use portable_pty::{native_pty_system, PtySize};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
/// `ConsoleFlags` bit 0: the process ignores CTRL+C.
const IGNORE_CTRL_C: u32 = 1;

/// `PROCESS_BASIC_INFORMATION`, spelled out here so the test needs no
/// `Win32_System_Kernel` feature just for the struct.
#[repr(C)]
#[derive(Default)]
struct BasicInfo {
    exit_status: i32,
    peb: usize,
    affinity_mask: usize,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from: usize,
}

/// The `ConsoleFlags` word of process `pid`, read out of its PEB.
fn console_flags(pid: u32) -> u32 {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid)
            .unwrap_or_else(|e| panic!("OpenProcess({pid}): {e}"));
        let mut info = BasicInfo::default();
        let mut len = 0u32;
        let status = NtQueryInformationProcess(
            h,
            ProcessBasicInformation,
            &mut info as *mut _ as *mut core::ffi::c_void,
            core::mem::size_of::<BasicInfo>() as u32,
            &mut len,
        );
        assert!(status.is_ok(), "NtQueryInformationProcess({pid}): {status:?}");
        let mut params: usize = 0;
        ReadProcessMemory(
            h,
            (info.peb + 0x20) as *const core::ffi::c_void,
            &mut params as *mut _ as *mut core::ffi::c_void,
            core::mem::size_of::<usize>(),
            None,
        )
        .unwrap_or_else(|e| panic!("read PEB.ProcessParameters of {pid}: {e}"));
        let mut flags: u32 = 0;
        ReadProcessMemory(
            h,
            (params + 0x18) as *const core::ffi::c_void,
            &mut flags as *mut _ as *mut core::ffi::c_void,
            core::mem::size_of::<u32>(),
            None,
        )
        .unwrap_or_else(|e| panic!("read ConsoleFlags of {pid}: {e}"));
        let _ = CloseHandle(h);
        flags
    }
}

/// A child that stays alive long enough to be read, then is killed.
fn ping_exe() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    format!(r"{root}\System32\PING.EXE")
}

/// Spawn ping through plain `CreateProcess` (std), read its flags, kill it.
fn plain_child_flags(creation_flags: u32) -> u32 {
    let mut child = Command::new(ping_exe())
        .args(["-n", "30", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(creation_flags)
        .spawn()
        .expect("spawn ping");
    let flags = console_flags(child.id());
    let _ = child.kill();
    let _ = child.wait();
    flags
}

#[test]
fn a_pane_child_is_born_able_to_take_ctrl_c_even_when_the_app_was_launched_ignoring_it() {
    // 1. The instrument sees a SET bit: CREATE_NEW_PROCESS_GROUP is documented
    //    to disable CTRL+C for the new process.
    let group = plain_child_flags(CREATE_NEW_PROCESS_GROUP);
    assert_eq!(
        group & IGNORE_CTRL_C,
        IGNORE_CTRL_C,
        "positive control: a CREATE_NEW_PROCESS_GROUP child must read as ignoring CTRL+C \
         (ConsoleFlags={group:#x}); if it does not, the PEB offsets are wrong and every \
         assertion below is vacuous"
    );

    // 2. Put this process where `npx orrerix` puts the app, and prove the state
    //    really is inherited: a plain child now carries the bit too.
    unsafe { SetConsoleCtrlHandler(None, true) }.expect("set ignore-CTRL+C on the test process");
    let inherited = plain_child_flags(0);
    assert_eq!(
        inherited & IGNORE_CTRL_C,
        IGNORE_CTRL_C,
        "premise: a child of a process ignoring CTRL+C inherits it (ConsoleFlags={inherited:#x})"
    );

    // 3. The fix: a pane child spawned through the production path on a real
    //    ConPTY is born NOT ignoring CTRL+C.
    let pair = native_pty_system()
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .expect("openpty");
    let argv = vec![ping_exe(), "-n".into(), "30".into(), "127.0.0.1".into()];
    let (mut child, direct) = loomux_lib::pty::spawn_pane_child(
        &*pair.slave,
        None,
        Some(&argv),
        None,
        &[],
        loomux_lib::pty::ShellKind::PowerShell,
    )
    .expect("spawn a pane child");
    let pid = child.process_id().expect("pane child pid");
    let pane = console_flags(pid);
    let _ = child.kill();
    drop(pair);
    assert!(direct, "ping.exe is a native image and must take the direct spawn path");
    assert_eq!(
        pane & IGNORE_CTRL_C,
        0,
        "a pane child inherited ignore-CTRL+C (ConsoleFlags={pane:#x}): Ctrl+C in that pane \
         would never interrupt a running process (#3595)"
    );
}
