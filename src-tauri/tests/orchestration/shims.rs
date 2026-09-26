//! The .cmd shim delegator and stale-shim pruning.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- #335: .cmd shim delegator bakes in an absolute sh path ----------
//
// CI cannot exercise an agent pane, so these tests target the generated
// artifact directly (as #335's own test-strategy note asks): render the
// `.cmd` delegator, invoke it with a PATH that carries none of sh.exe's
// directory (the exact PowerShell/cmd shape reported live), and assert the
// gate still fires — or, if sh genuinely can't be resolved, that the
// degraded-gate audit event fires instead of a silent bypass.

/// `where sh.exe`'s first hit, or `None` — used only to find an absolute path
/// to feed into `gh_shim_cmd` as the pre-resolved `sh_path` (mirroring what
/// `OrchRegistry::ensure_shims` would have baked in via `winpath::resolve_sh`
/// at shim-write time). Not a claim about the *test process's* own PATH.
#[cfg(windows)]
pub(crate) fn locate_sh_exe() -> Option<String> {
    let out = std::process::Command::new("where").arg("sh.exe").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(windows)]
#[test]
fn gh_cmd_shim_still_gates_a_merge_when_sh_is_stripped_from_the_invoking_path() {
    // The live bug (#335): a default Git for Windows install puts git.exe on
    // PATH but NOT sh.exe (`usr\bin` is off PATH) — a PowerShell/cmd
    // invocation of gh.cmd used to `for %%S in (sh.exe) do set ORRERIX_SH=...`
    // against the *invoking* shell's PATH, find nothing, and silently exec
    // the real gh with no gate and no audit. The fix bakes an ABSOLUTE
    // sh.exe path into the .cmd at shim-write time; this proves the
    // delegator still routes through the POSIX gate even when the invoking
    // process's PATH carries no trace of sh.exe's directory at all.
    use std::process::Command;
    let Some(sh_abs) = locate_sh_exe() else {
        eprintln!("SKIP gh_cmd_shim_still_gates_a_merge…: no sh.exe found via `where`");
        return;
    };

    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh(root, &log);
    let shim_dir = root.join("shim");
    fs::create_dir_all(&shim_dir).unwrap();

    // The same POSIX gh shim the `sh`-invoked harness tests already exercise.
    fs::write(shim_dir.join("gh"), gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    // The Windows delegator, with the absolute sh path baked in (#335's fix) —
    // exactly what `ensure_shims`/`write_shim` would have resolved via
    // `winpath::resolve_sh` from git.exe's own install layout.
    fs::write(
        shim_dir.join("gh.cmd"),
        gh_shim_cmd(&fake.display().to_string(), Some(&sh_abs.replace('\\', "/"))),
    )
    .unwrap();

    // A PATH with no trace of sh.exe's directory — the exact PowerShell/cmd
    // scenario #335 reports. Only bare Windows system dirs, so cmd.exe's own
    // built-ins still resolve — never the shim dir, never any Git install dir.
    let stripped_path = r"C:\Windows\System32;C:\Windows";

    let status = Command::new(shim_dir.join("gh.cmd"))
        .args(["pr", "merge", "5"])
        .env("PATH", stripped_path)
        .env("LOOMUX_GROUP_DIR", &group)
        .env("FAKE_BASE", "main")
        .env("FAKE_DEFAULT", "main")
        .env("FAKE_NUM", "5")
        .status()
        .unwrap();

    assert!(
        !status.success(),
        "the merge must still be BLOCKED (no grant, no markers) even with sh stripped from the \
         invoking PATH — a bypass would exit 0"
    );
    let audit = fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    assert!(
        audit.contains("merge-gate-blocked"),
        "must route through the real POSIX gate logic (not just fail some other way), got audit: {audit}"
    );
    let gh_log = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !gh_log.contains("FAKE-GH-RAN"),
        "must never fall through to running gh unrestricted (the args-consuming fallback branch \
         in the fake gh stub), got log: {gh_log}"
    );
}

#[cfg(windows)]
#[test]
fn git_cmd_shim_still_gates_a_tag_push_when_sh_is_stripped_from_the_invoking_path() {
    // Same shape as the gh test above, for the git shim's release-gate path
    // (`git push` of a `v*` tag, #83): #335's bug and fix are identical
    // across both `.cmd` delegators, since they share `shim_cmd_delegator`.
    use std::process::Command;
    let Some(sh_abs) = locate_sh_exe() else {
        eprintln!("SKIP git_cmd_shim_still_gates_a_tag_push…: no sh.exe found via `where`");
        return;
    };

    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    fs::create_dir_all(&group).unwrap();
    let fake = root.join("fakegit");
    fs::write(
        &fake,
        "#!/bin/sh\n\
         if [ \"$1\" = \"rev-parse\" ]; then exit 1; fi\n\
         printf 'FAKE-GIT-RAN\\n'; exit 0\n",
    )
    .unwrap();
    let shim_dir = root.join("shim");
    fs::create_dir_all(&shim_dir).unwrap();
    fs::write(shim_dir.join("git"), git_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    fs::write(
        shim_dir.join("git.cmd"),
        git_shim_cmd(&fake.display().to_string(), Some(&sh_abs.replace('\\', "/"))),
    )
    .unwrap();

    let stripped_path = r"C:\Windows\System32;C:\Windows";
    let status = Command::new(shim_dir.join("git.cmd"))
        .args(["push", "origin", "refs/tags/v1.2.3"])
        .env("PATH", stripped_path)
        .env("LOOMUX_GROUP_DIR", &group)
        .status()
        .unwrap();

    assert!(
        !status.success(),
        "the tag push must still be BLOCKED (no grant) even with sh stripped from the invoking PATH"
    );
    let audit = fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    assert!(audit.contains("release-gate-blocked"), "must route through the real gate logic, got audit: {audit}");
}

#[cfg(windows)]
#[test]
fn gh_cmd_shim_audits_loudly_instead_of_silently_bypassing_when_no_sh_is_resolvable() {
    // #335's other half: if shim-write time genuinely cannot find `sh`
    // anywhere on the machine, the fallback to the real binary must be LOUD
    // (an audit event saying the gate is degraded), never a silent bypass.
    // `sh_path: None` is exactly the shape `ensure_shims` bakes in for that
    // case.
    use std::process::Command;
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");

    // A real-binary stand-in that `cmd.exe` can execute directly (no `sh`
    // involved at all in this branch) — a minimal batch stub.
    let fake_bat = root.join("fakegh.cmd");
    fs::write(&fake_bat, format!("@echo off\r\necho FAKE-GH-RAN>>\"{}\"\r\nexit /b 0\r\n", log.display()))
        .unwrap();

    let shim_dir = root.join("shim");
    fs::create_dir_all(&shim_dir).unwrap();
    fs::write(shim_dir.join("gh.cmd"), gh_shim_cmd(&fake_bat.display().to_string(), None)).unwrap();

    let status = Command::new(shim_dir.join("gh.cmd"))
        .args(["pr", "merge", "5"])
        .env("LOOMUX_GROUP_DIR", &group)
        .status()
        .unwrap();

    assert!(status.success(), "the degraded fallback still runs the real binary, so this exits 0");
    let gh_log = fs::read_to_string(&log).unwrap_or_default();
    assert!(gh_log.contains("FAKE-GH-RAN"), "the real binary must have actually run: {gh_log}");
    let audit = fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    assert!(
        audit.contains("gate-degraded-no-sh"),
        "a fallback with no sh resolvable must be audited LOUDLY, not silent — got audit: {audit:?}"
    );
}

#[cfg(windows)]
#[test]
fn gh_cmd_shim_ignores_an_inherited_sh_override_in_the_degraded_no_sh_case() {
    // rev-10 review finding on #335: `setlocal` makes the .cmd's OWN variable
    // changes revertible, but it does NOT clear an sh-override the invoking
    // shell already exported. Before the fix, the degraded (`sh_path: None`)
    // template never `set` the sh-override at all — so an inherited value (stray
    // env, or an adversarial agent priming its own shell) would satisfy `if
    // not defined ORRERIX_SH` and route through THAT untrusted binary instead
    // of taking the audited degraded-fallback branch, defeating the "never a
    // silent bypass" guarantee for exactly the case this delegator exists to
    // protect. The fix unconditionally clears it before the check;
    // this pins it by simulating the attack: an inherited ORRERIX_SH pointing
    // at a stand-in binary that must NEVER run.
    use std::process::Command;
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");

    let fake_bat = root.join("fakegh.cmd");
    fs::write(&fake_bat, format!("@echo off\r\necho FAKE-GH-RAN>>\"{}\"\r\nexit /b 0\r\n", log.display()))
        .unwrap();

    // A stand-in for a hijacked/inherited "sh.exe" — if the .cmd ever invokes
    // it, that alone proves the inherited variable shadowed the baked-in
    // (empty) resolution.
    let malicious_log = root.join("malicious.log");
    let malicious = root.join("malicious_sh.cmd");
    fs::write(
        &malicious,
        format!("@echo off\r\necho MALICIOUS-SH-RAN>>\"{}\"\r\nexit /b 0\r\n", malicious_log.display()),
    )
    .unwrap();

    let shim_dir = root.join("shim");
    fs::create_dir_all(&shim_dir).unwrap();
    // sh_path: None — shim-write time found no sh anywhere on the machine.
    fs::write(shim_dir.join("gh.cmd"), gh_shim_cmd(&fake_bat.display().to_string(), None)).unwrap();

    let status = Command::new(shim_dir.join("gh.cmd"))
        .args(["pr", "merge", "5"])
        .env("LOOMUX_GROUP_DIR", &group)
        // The variable the .cmd ACTUALLY consults. #1153 phase 3 renamed it,
        // and setting the old name here would leave this test green while
        // simulating nothing: the malicious stand-in cannot run if the shim
        // never looks at the variable pointing to it. (The group dir above
        // stays on the LEGACY spelling on purpose — the .cmd resolves the
        // current one from it, so that arm of the dual-accept is exercised
        // here rather than only asserted.)
        .env("ORRERIX_SH", &malicious) // simulates an inherited/adversarial value
        .status()
        .unwrap();

    assert!(status.success(), "the degraded fallback still runs the real binary, so this exits 0");
    assert!(
        !malicious_log.exists(),
        "an inherited ORRERIX_SH must NEVER be trusted when shim-write time resolved no sh — the \
         malicious stand-in must not have run"
    );
    let gh_log = fs::read_to_string(&log).unwrap_or_default();
    assert!(gh_log.contains("FAKE-GH-RAN"), "the real binary must have run via the degraded fallback: {gh_log}");
    let audit = fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    assert!(
        audit.contains("gate-degraded-no-sh"),
        "the degraded fallback must still be audited even with an inherited ORRERIX_SH present — got audit: {audit:?}"
    );
}

// ───────── #3477: the `.cmd` delegator finds its OWN POSIX shim ─────────
//
// cmd.exe expands a top-level `%~dp0` against the CURRENT DIRECTORY when the
// batch was started by a QUOTED name resolved through PATH — the shape npm's
// `.cmd` wrappers use. The delegator then handed sh `<cwd>\gh`, so a file named
// `gh` in the agent's worktree ran in place of the merge gate.

#[cfg(windows)]
#[test]
fn gh_cmd_shim_never_runs_a_gh_file_in_the_cwd_when_invoked_by_quoted_name_through_path() {
    use std::process::Command;
    let Some(sh_abs) = locate_sh_exe() else {
        eprintln!("SKIP gh_cmd_shim_never_runs_a_gh_file_in_the_cwd…: no sh.exe found via `where`");
        return;
    };

    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh(root, &log);
    let shim_dir = root.join("shim");
    fs::create_dir_all(&shim_dir).unwrap();
    fs::write(shim_dir.join("gh"), gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    fs::write(
        shim_dir.join("gh.cmd"),
        gh_shim_cmd(&fake.display().to_string(), Some(&sh_abs.replace('\\', "/"))),
    )
    .unwrap();

    // The agent's worktree, holding a POSIX script named `gh` that "merges" and
    // exits 0. An extensionless file does not shadow cmd's own PATHEXT lookup, so
    // `gh.cmd` from the shim dir is still what cmd starts — the question is only
    // which `gh` the delegator hands to sh.
    let worktree = root.join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    let decoy_log = root.join("decoy.log");
    fs::write(
        worktree.join("gh"),
        format!("#!/bin/sh\necho DECOY-GH-RAN >> \"{}\"\nexit 0\n", decoy_log.display().to_string().replace('\\', "/")),
    )
    .unwrap();

    // The caller: a batch that names `gh` QUOTED and lets cmd resolve it through
    // PATH — npm's cmd-shim shape (`"%_prog%" …`). Invoking gh.cmd by absolute
    // path, as the tests above do, never reaches the bug.
    let caller = root.join("caller.cmd");
    fs::write(&caller, "@echo off\r\n\"gh\" pr merge 5\r\nexit /b %errorlevel%\r\n").unwrap();
    let path = format!(r"{};C:\Windows\System32;C:\Windows", shim_dir.display());

    let status = Command::new("cmd")
        .arg("/d")
        .arg("/c")
        .arg(&caller)
        .current_dir(&worktree)
        .env("PATH", path)
        .env("LOOMUX_GROUP_DIR", &group)
        .env("FAKE_BASE", "main")
        .env("FAKE_DEFAULT", "main")
        .env("FAKE_NUM", "5")
        .status()
        .unwrap();

    assert!(
        !decoy_log.exists(),
        "the delegator ran the worktree's own `gh` in place of the merge gate — %~dp0 resolved to the cwd"
    );
    assert!(!status.success(), "the merge must be BLOCKED (no grant, no markers); a bypass exits 0");
    let audit = fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    assert!(
        audit.contains("merge-gate-blocked"),
        "the real POSIX gate must be what ran, got audit: {audit}"
    );
}

#[test]
fn every_cmd_delegator_reads_its_own_dir_only_inside_a_called_label() {
    // The property the Windows test above relies on, pinned on every platform
    // and for BOTH delegators (they share `shim_cmd_delegator`): the only `%~dp0`
    // in the file sits in the `:orrerix_self_dir` label, which the main flow
    // reaches via `call` — never a top-level `%~dp0`, which is the #3477 bug.
    for (name, text) in [
        ("gh", gh_shim_cmd("C:/real/gh.exe", Some("C:/Git/usr/bin/sh.exe"))),
        ("git", git_shim_cmd("C:/real/git.exe", Some("C:/Git/usr/bin/sh.exe"))),
        ("gh (no sh)", gh_shim_cmd("C:/real/gh.exe", None)),
    ] {
        let lines: Vec<&str> = text.split("\r\n").collect();
        let label = lines
            .iter()
            .position(|l| *l == ":orrerix_self_dir")
            .unwrap_or_else(|| panic!("{name}: no :orrerix_self_dir label in\n{text}"));
        let dp0: Vec<usize> = (0..lines.len()).filter(|&i| lines[i].contains("%~dp0")).collect();
        assert!(!dp0.is_empty(), "{name}: the delegator must still locate its own dir");
        for i in dp0 {
            assert!(
                i > label && !lines[label..i].iter().any(|l| l.starts_with("exit /b")),
                "{name}: `%~dp0` on line {i} is outside the called label: {:?}",
                lines[i]
            );
        }
        let call = lines.iter().position(|l| *l == "call :orrerix_self_dir");
        let first_use = lines.iter().position(|l| l.contains("%ORRERIX_SHIM_DIR%"));
        assert!(
            matches!((call, first_use), (Some(c), Some(u)) if c < u),
            "{name}: the label must be CALLED before the shim dir is used\n{text}"
        );
        assert!(
            label > lines.iter().position(|l| l.starts_with("exit /b")).unwrap(),
            "{name}: the main flow must exit before falling into the label"
        );
    }
}

// ───────── #3477: stale generated shims are pruned, nothing else is ─────────

/// The two orphan headers measured on a real machine: the `node`/`npm` shims a
/// #322 build wrote (POSIX and `.cmd`), which shadowed the real programs on
/// every agent pane's PATH and broke `npm run`.
const ORPHAN_SH_HEAD: &str = "#!/bin/sh\n# loomux resource-guard shim (#318) — restrict-only: delay, never deny.\n\
                              # Generated by loomux; do not edit. Guards invocations of: node\n";
const ORPHAN_CMD_HEAD: &str = "@echo off\r\nrem loomux resource-guard shim (#318) — delegate to the POSIX shim; run real node if no sh.\r\n";

#[test]
fn a_stale_generated_shim_is_recognised_and_a_file_the_product_did_not_write_is_not() {
    // Orphans: generated header, name not in GENERATED_SHIM_NAMES.
    assert!(is_stale_generated_shim("node", ORPHAN_SH_HEAD));
    assert!(is_stale_generated_shim("npm.cmd", ORPHAN_CMD_HEAD));
    assert!(is_stale_generated_shim("cargo", "#!/bin/sh\n# orrerix resource-guard shim (#318)\n"));
    // A shim this build writes is never stale — bare or `.cmd`.
    let gh_sh = gh_shim_sh("C:/real/gh.exe", &shim_paths());
    assert!(!is_stale_generated_shim("gh", &gh_sh));
    assert!(!is_stale_generated_shim("gh.cmd", &gh_shim_cmd("C:/real/gh.exe", None)));
    assert!(!is_stale_generated_shim("git", &git_shim_sh("C:/real/git.exe", &shim_paths())));
    assert!(!is_stale_generated_shim("loomux.cmd", &loomux_shim_cmd()));
    // Fail-safe direction: no product header → never deleted, whatever the name.
    assert!(!is_stale_generated_shim("node", "#!/bin/sh\nexec /usr/bin/node \"$@\"\n"));
    assert!(!is_stale_generated_shim("node", "#!/bin/sh\n# my own node shim (#1)\n"));
    assert!(!is_stale_generated_shim("tool", "#!/bin/sh\n# loomuxish shim (#1)\n"));
    assert!(!is_stale_generated_shim("tool", "#!/bin/sh\n# loomux helper, not a generated file\n"));
    // The header must be in the first lines, where every generator puts it.
    assert!(!is_stale_generated_shim("tool", "a\nb\nc\nd\n# loomux x shim (#318)\n"));
    // A multi-byte character at the `rem ` probe boundary must not panic.
    assert!(!is_stale_generated_shim("tool", "ré—x loomux shim (#1)\n"));
}

#[test]
fn the_merge_and_release_gate_shims_are_never_pruned_whatever_a_spawn_resolved() {
    // #3481 B1: the kept set must not depend on whether THIS spawn found the real
    // gh/git — a transient miss (an upgrade uninstalls, then reinstalls) would
    // otherwise delete the merge gate from the dir every live pane of every group
    // has first on PATH. `prune_stale_shims` takes no list from its caller, so the
    // only thing that can drop a gate name is this constant.
    for gate in ["gh", "git"] {
        assert!(
            GENERATED_SHIM_NAMES.contains(&gate),
            "`{gate}` must always be kept — its shim IS the gate, and pruning it on a spawn \
             that missed the real binary ungates every live pane"
        );
    }
    // A gh shim whose real gh is not installed at all: still kept, header and all.
    let td = tempfile::tempdir().unwrap();
    let dir = td.path();
    fs::write(dir.join("gh"), gh_shim_sh("C:/gone/gh.exe", &shim_paths())).unwrap();
    fs::write(dir.join("gh.cmd"), gh_shim_cmd("C:/gone/gh.exe", None)).unwrap();
    fs::write(dir.join("git"), git_shim_sh("C:/gone/git.exe", &shim_paths())).unwrap();
    fs::write(dir.join("git.cmd"), git_shim_cmd("C:/gone/git.exe", None)).unwrap();
    fs::write(dir.join("node"), ORPHAN_SH_HEAD).unwrap(); // positive control: the prune ran
    prune_stale_shims(dir);
    let mut left: Vec<String> =
        fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(left, ["gh", "gh.cmd", "git", "git.cmd"], "the gates stay; only the orphan goes");
}

#[test]
fn prune_stale_shims_deletes_the_orphans_and_keeps_everything_else() {
    let td = tempfile::tempdir().unwrap();
    let dir = td.path();
    let write = |n: &str, body: &str| fs::write(dir.join(n), body).unwrap();
    write("node", ORPHAN_SH_HEAD);
    write("node.cmd", ORPHAN_CMD_HEAD);
    write("npm", &ORPHAN_SH_HEAD.replace("of: node", "of: npm"));
    write("npm.cmd", &ORPHAN_CMD_HEAD.replace("real node", "real npm"));
    write("gh", &gh_shim_sh("C:/real/gh.exe", &shim_paths()));
    write("gh.cmd", &gh_shim_cmd("C:/real/gh.exe", None));
    write("orrerix.cmd", &loomux_shim_cmd());
    write("mytool", "#!/bin/sh\necho mine\n");
    fs::create_dir(dir.join("loomux-subdir-shim (#1)")).unwrap();

    prune_stale_shims(dir);

    let mut left: Vec<String> =
        fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(
        left,
        ["gh", "gh.cmd", "loomux-subdir-shim (#1)", "mytool", "orrerix.cmd"],
        "exactly the four orphans go; the kept shims, a user file and a directory stay"
    );
}
