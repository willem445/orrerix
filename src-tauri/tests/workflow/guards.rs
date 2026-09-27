//! Source-scanning guards: the argued residuals that still read the default workflow file directly (#1689).
//!
//! One module of the `workflow` integration-test target (`main.rs`),
//! split out of the former single-file `tests/workflow.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

/// **No call site in `src-tauri/src` reads the repo's `default` workflow
/// without asking which one a GROUP runs** — the residual is closed (#2663),
/// and this scan is what keeps it closed.
///
/// The one `RESIDUALS` row left is not a residual at all: it is the NAMED
/// sibling `workflow_file_named(`, which the `workflow::workflow_file` prefix
/// necessarily catches, and it is listed rather than excluded by a narrower
/// pattern because a pattern tuned to miss it would also miss
/// `workflow_file_exists(`, the function this is guarding against.
///
/// The plan predicted two residuals and both are gone.
/// `orch_workflow_preview_sync` was the second, and it stopped being one when
/// it gained its optional `name` — its no-name arm goes through
/// `load_workflow_named` at `default` like everything else. `gh.rs::hold_label`
/// was the first, and #2663 closed it the same way: it takes an
/// `Option<&Guardrails>` now, and its no-group arm reads
/// `load_active_workflow(repo, &Guardrails::default())` rather than
/// `workflow::load_workflow(repo)`. Both times this scan is what noticed the
/// row had gone stale: the assertion below refuses to carry a row that matches
/// nothing, rather than letting the count go quietly wrong.
///
/// **Three spellings, not one**, and the counts below are measured at both ends
/// rather than recalled — the first version of this paragraph guessed them and
/// got all four wrong (rev-std round 2, finding 1; the widening itself was
/// rev-final round 2, finding 2).
///
/// `git grep -c -F <spelling> <base> -- src-tauri/src` at base `34f92793`, and
/// the same count at head:
///
/// | spelling | base | head | rerouted |
/// | --- | --- | --- | --- |
/// | `workflow::load_workflow(` | 13 (`gh.rs` 1, `mod.rs` 11, `rdtick.rs` 1) | 1 | 12 |
/// | `workflow::workflow_path(` | 12 (all `mod.rs`) | 0 | 12 |
/// | `workflow::workflow_file_exists(` | 2 (`mod.rs`) | 0 | 2 |
///
/// So **26 sites moved, and 14 of them — every `workflow_path` and every
/// `workflow_file_exists` — were invisible to a trigger watching
/// `load_workflow` alone.** All three functions are still `pub` and still
/// answer only for `default`. Adding an audit line or a template var as
/// `workflow::workflow_path(&g.repo)` type-checks, reads naturally, and is
/// exactly what twelve lines in `mod.rs` said one commit ago; a group running
/// `b.yml` would then be told in its own audit trail that it runs
/// `.orrerix/workflow.yml`.
///
/// **#2663 moved the 27th**, the `head` 1 in the first row above — `gh.rs`'s
/// `hold_label`. Same instrument, its own base and head:
///
/// | spelling | base `a1a0714e` | head | rerouted |
/// | --- | --- | --- | --- |
/// | `workflow::load_workflow(` | 1 (`gh.rs`) | 0 | 1 |
/// | `workflow::workflow_path(` | 0 | 0 | 0 |
/// | `workflow::workflow_file` | 1 (`mod.rs`, the NAMED sibling) | 1 | 0 |
///
/// The last row is the `RESIDUALS` entry below and is expected to stay 1: it is
/// `workflow_file_named(`, matched only because the trigger is a prefix.
///
/// A textual scan, with the limits that implies — it reads call TEXT, so a
/// caller that bound the function to a local first would be invisible. **So
/// would an UNQUALIFIED call**: `use loomux_engine::workflow::load_workflow;`
/// followed by a bare `load_workflow(&g.repo)` matches none of the three
/// patterns, and this is worth naming concretely rather than leaving inside
/// "textual", because widening the trigger from one spelling to three makes the
/// scan look stronger against exactly that shape than it is (rev-std round 2,
/// premortem 1). Nothing in `src-tauri/src` imports any of the three today, and
/// nothing here enforces that.
///
/// What it IS defence in depth over is a NEW group-scoped reader being added
/// against one of those three in the spelling the rest of the file uses, which
/// is how the reroute would realistically erode; the compiler cannot help
/// there, because all of them exist and all of them type-check.
#[test]
fn only_the_argued_residuals_still_read_the_default_workflow_directly() {
    /// The default-only spellings this watches. Each is matched with its
    /// trailing `(` so a NAMED sibling — `workflow_path_named(`,
    /// `workflow_file_named(` — does not collide with its default-only parent;
    /// `workflow_file` is deliberately the prefix rather than `workflow_file(`,
    /// so `workflow_file_exists(` is caught too, at the cost of matching
    /// `workflow_file_named(`, which the allowlist then answers for.
    const DEFAULT_ONLY: &[&str] = &[
        "workflow::load_workflow(",
        "workflow::workflow_path(",
        "workflow::workflow_file",
    ];

    /// `(file, call text, why it is not group-scoped)`.
    const RESIDUALS: &[(&str, &str, &str)] = &[
        (
            // `orch_workflow_preview_sync`, in `orchestration/commands/` since #3498 P2.
            "launch.rs",
            "workflow::workflow_file_named(&repo, n).is_file()",
            "not a residual at all — the NAMED sibling, caught only because the trigger matches \
             the `workflow::workflow_file` prefix so that `workflow_file_exists(` cannot slip \
             past it. Listed rather than excluded by a narrower pattern, because a pattern \
             tuned to miss this line would also miss the function it is guarding against",
        ),
    ];

    // Positive control, run against the SAME predicate the sweep below uses:
    // `found.is_empty()` is also what a trigger matching nothing produces, and
    // widening it from one pattern to three (rev-final round 2) is exactly when
    // a typo would go unnoticed.
    let triggers = |line: &str| DEFAULT_ONLY.iter().any(|s| line.contains(s));
    for must in [
        "(\"WORKFLOW_PATH\", workflow::workflow_path(&g.repo)),",
        "if workflow::workflow_file_exists(repo) {",
        "match workflow::load_workflow(&g.repo) {",
    ] {
        assert!(triggers(must), "the trigger must catch a default-only reader: {must}");
    }
    // The near-miss half: a pattern loose enough to flag every NAMED sibling
    // would flag the whole reroute and be switched off within a week. Two of the
    // three are excluded by their trailing `(`.
    for sibling in [
        "workflow::workflow_path_named(repo, name)",
        "workflow::load_workflow_named(repo, name)",
    ] {
        assert!(!triggers(sibling), "a NAMED sibling must not read as default-only: {sibling}");
    }
    // `workflow_file_named` is the third, and it DOES trigger — stated rather
    // than excused. `workflow::workflow_file` is deliberately a prefix so that
    // `workflow_file_exists(` cannot slip past, and that catches the named
    // sibling too; its `RESIDUALS` row is what answers for it, which is why
    // that row exists at all.
    assert!(
        triggers("workflow::workflow_file_named(&repo, n).is_file()"),
        "the prefix that catches workflow_file_exists( necessarily catches this too — if it \
         stops doing so, the RESIDUALS row for it goes stale and this test says which"
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&root, &mut files);
    assert!(!files.is_empty(), "a scan over an empty root is a tripwire that cannot fire");

    let mut found: Vec<String> = Vec::new();
    let mut seen = vec![0usize; RESIDUALS.len()];
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for (i, line) in fs::read_to_string(path).unwrap().lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            if !DEFAULT_ONLY.iter().any(|s| t.contains(s)) {
                continue;
            }
            match RESIDUALS.iter().position(|(f, call, _)| *f == name && t.contains(call)) {
                Some(j) => seen[j] += 1,
                None => found.push(format!("{name}:{}: {t}", i + 1)),
            }
        }
    }
    assert!(
        found.is_empty(),
        "a GROUP-scoped reader must go through `load_active_workflow` (#1689), or it reads \
         `.orrerix/workflow.yml` for a group that runs something else. Found {} site(s):\n{}\n\n\
         If one is genuinely repo-scoped, add it to `RESIDUALS` with the argument — writing the \
         argument down is the point.",
        found.len(),
        found.join("\n")
    );
    for (j, (f, call, _)) in RESIDUALS.iter().enumerate() {
        assert!(
            seen[j] > 0,
            "`RESIDUALS` row {f} / `{call}` matched nothing — the site moved or was rerouted, so \
             this row is stale. Re-point it or drop it."
        );
    }
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}
