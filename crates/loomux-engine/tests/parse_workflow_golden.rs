//! Characterisation goldens for [`parse_workflow`] (#3498 P8b).
//!
//! P8b restructures `parse_workflow` — the tree's highest-cognitive-complexity
//! function — into per-section sub-parsers, and the human's standing condition
//! on the #3498 campaign is that every slice PROVES behaviour is unchanged. This
//! file is that proof's instrument, and it landed in a commit of its own BEFORE
//! the restructure, green against the unchanged function. That order is what
//! makes it evidence: a golden written after the refactor could only ever pin
//! whatever the refactor happened to produce.
//!
//! Two halves:
//!
//! - **Success**: the hand-authored subjects in
//!   `tests/fixtures/parse_workflow/*.yml` (enumerated at run time), each
//!   REQUIRED to parse, compared to `<stem>.golden.txt` as the WHOLE `Result`
//!   via `{:#?}`. Between them they declare every section and use every block
//!   key. Debug rather than serde because a derived `Debug` prints every
//!   field, while a `Serialize` impl is free to skip one. Default-deny both
//!   ways: a subject with no golden fails, and a golden with no subject fails.
//!
//!   **The repo's own `.orrerix/workflow.yml` and `.orrerix/workflows/*.yml`
//!   are deliberately NOT subjects.** They are live operational config, edited
//!   on a model swap or a new block several times a month, and a golden over
//!   them would turn every such edit into a red engine test and a CI-log
//!   re-bless, although none of those edits changes parse behaviour. P8b's
//!   parity over those files was measured once, while the restructure landed
//!   (PR #3673's body names the runs). That the main file parses at all is
//!   `src-tauri/tests/workflow/dogfood.rs`'s job.
//! - **Refusal**: [`cases`] holds one malformed input per error branch the
//!   function has, plus the ORDER cases (errors across sections, within a gate,
//!   across blocks). Their full error lists — strings and order — are compared
//!   to `errors.golden.txt`. Each case also names a `needle`: a substring of the
//!   format string of the branch it was written to reach, hand-copied from the
//!   source. The golden alone would happily characterise whatever branch an
//!   input ACTUALLY hits; the needle is what says it hits the one its name
//!   claims. "Per branch" means per branch of `parse_workflow` and the
//!   section parsers it calls, NOT of the helpers those call
//!   (`validate_knob`, `gate_reviewer_error`, `sanitize_intake_label`, …):
//!   each helper is reached, but its own branches are sampled, not enumerated.
//!
//! **These goldens are snapshots of the base behaviour, on purpose.** For a
//! refactor the property under test is parity with the code as it was, so the
//! expectation is the unchanged function's own output, blessed from a CI run of
//! the base (workers do not build Rust locally). A mismatch prints the actual
//! text between `=====BEGIN`/`=====END` markers for exactly that purpose. Once
//! blessed, a golden is edited only by a change that MEANS to change parse
//! behaviour — never by a restructure. To re-bless, copy the `BEGIN`/`END`
//! block from the failing CI log over the golden, then read the diff.
//!
//! **What else moves these goldens.** They pin OUTPUT, so they also pin text
//! and values `parse.rs` does not own. `errors.golden.txt` quotes messages
//! written elsewhere: the `CliCaps` containment, effort and context notes,
//! `SUPPORTED_CLIS` and the knob vocabularies (`model.rs`), `vocab.rs`'s
//! kind/hint/driver vocabularies and refusals, `resolve_profile_path`
//! (`sanitize.rs`), `gate_reviewer_error` and `sanitize_intake_label`
//! (`schema.rs`), `pathseg::check_segment`, `triage::Kind::ALL` and serde's
//! own unknown-field messages. `every-section.golden.txt` and
//! `minimal.golden.txt` carry the policy defaults and clamp ceilings, as the
//! notify-TTL and drive-timeout clamps resolve them. Rewording one of those,
//! or retuning a default, reddens a golden here with no parse change: re-bless
//! it in that same PR.
//!
//! Line endings: inputs and goldens are both normalised to LF before use, so a
//! Windows checkout's CRLF (`core.autocrlf=true` is this project's baseline) is
//! not a difference. Parity is a question about the function, not the checkout.

use std::fs;
use std::path::{Path, PathBuf};

use loomux_engine::workflow::{parse_workflow, sanitize_id};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parse_workflow")
}

fn read_lf(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn golden_path(stem: &str) -> PathBuf {
    let mut name = stem.to_string();
    name.push_str(".golden.txt");
    fixture_dir().join(name)
}

/// The `.yml` files directly inside `dir`, sorted by name.
fn yml_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("listing {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "yml"))
        .collect();
    out.sort();
    out
}

fn stem_of(p: &Path) -> String {
    p.file_stem().expect("file stem").to_string_lossy().into_owned()
}

/// Every success subject as `(golden stem, input path)` — the fixture
/// directory's `.yml` files only (see the module doc for why the repo's own
/// workflow files are not among them).
fn subjects() -> Vec<(String, PathBuf)> {
    yml_files(&fixture_dir()).into_iter().map(|p| (stem_of(&p), p)).collect()
}

/// Compare `actual` to the golden at `path`; on a mismatch print the actual
/// text between markers (the bless channel) and return false.
fn matches_golden(path: &Path, actual: &str) -> bool {
    let expected = fs::read_to_string(path).map(|s| s.replace("\r\n", "\n")).ok();
    if expected.as_deref() == Some(actual) {
        return true;
    }
    if let Some(expected) = &expected {
        // The first differing line, so a reader of the failure need not diff
        // two dumps by eye.
        let (mut e, mut a) = (expected.lines(), actual.lines());
        for n in 1.. {
            let (el, al) = (e.next(), a.next());
            if el != al {
                eprintln!("first difference in {} at line {n}:\n  golden: {el:?}\n  actual: {al:?}", path.display());
                break;
            }
            if el.is_none() {
                break;
            }
        }
    }
    eprintln!(
        "golden mismatch for {} ({}):\n=====BEGIN {}=====\n{actual}=====END {}=====",
        path.display(),
        if expected.is_some() { "differs" } else { "missing" },
        path.file_name().unwrap().to_string_lossy(),
        path.file_name().unwrap().to_string_lossy(),
    );
    false
}

#[test]
fn every_fixture_workflow_parses_to_its_golden() {
    let subjects = subjects();
    // Positive control on the population: both hand-authored fixtures. A
    // subject list that came back short would otherwise pass by comparing
    // nothing.
    let stems: Vec<&str> = subjects.iter().map(|(s, _)| s.as_str()).collect();
    for want in ["every-section", "minimal"] {
        assert!(stems.contains(&want), "subject {want} missing from {stems:?}");
    }

    let mut ok = true;
    for (stem, path) in &subjects {
        let text = read_lf(path);
        let parsed = parse_workflow(&text);
        // The fixtures exist to walk the SUCCESS paths: a refusal is a broken
        // subject, not a characterisation.
        if let Err(e) = &parsed {
            panic!("{} ({stem}) must parse, got {e:#?}", path.display());
        }
        let actual = format!("{parsed:#?}\n");
        ok &= matches_golden(&golden_path(stem), &actual);
    }
    assert!(ok, "at least one parse golden differs — see the BEGIN/END blocks above");
}

#[test]
fn no_parse_golden_is_orphaned() {
    // The other direction of default-deny: a golden whose subject was renamed or
    // deleted would otherwise sit here pinning nothing, reading as coverage.
    let mut stems: Vec<String> = subjects().into_iter().map(|(s, _)| s).collect();
    stems.push("errors".to_string());
    let mut goldens = 0;
    for entry in fs::read_dir(fixture_dir()).expect("fixture dir") {
        let name = entry.expect("dir entry").file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".golden.txt") else { continue };
        goldens += 1;
        assert!(stems.iter().any(|s| s == stem), "golden {name} has no subject");
    }
    assert!(goldens >= 3, "expected at least three goldens, found {goldens}");
}

/// One malformed input and the branch it is written to reach.
struct Case {
    name: &'static str,
    /// A substring of the reached branch's format string, copied by hand from
    /// the source — see the module doc.
    needle: &'static str,
    yaml: String,
}

fn case(name: &'static str, needle: &'static str, yaml: impl Into<String>) -> Case {
    Case { name, needle, yaml: yaml.into() }
}

/// `version: 1` plus a flow-style block list plus any further sections.
fn wf(blocks: &str, rest: &str) -> String {
    format!("version: 1\nblocks: [{blocks}]\n{rest}")
}

/// A valid roster the gate cases name reviewers from: one of every kind a gate
/// reviewer check distinguishes (a worker, two plain reviewers, a manager, a
/// liaison).
const ROSTER: &str = "{id: w, kind: worker}, {id: r1, kind: reviewer}, {id: r2, kind: reviewer}, \
                      {id: m, kind: manager}, {id: li, kind: reviewer, role_hint: liaison}";

fn gate(body: &str) -> String {
    wf(ROSTER, &format!("gates:\n  merge: {body}\n"))
}

const W: &str = "{id: w, kind: worker}";

fn cases() -> Vec<Case> {
    let long_id = "a".repeat(49);
    let many_rules = (0..33)
        .map(|n| format!("{{paths: [\"p{n}/**\"], reviewers: [r2]}}"))
        .collect::<Vec<_>>()
        .join(", ");
    let many_paths = (0..33).map(|n| format!("\"p{n}/**\"")).collect::<Vec<_>>().join(", ");
    let many_resources = (0..33).map(|n| format!("r{n}: {{}}")).collect::<Vec<_>>().join(", ");

    vec![
        // ── top level ──────────────────────────────────────────────────────
        case("version-unsupported", "is not supported (this build understands version", "version: 2\nblocks: [{id: w, kind: worker}]"),
        case("serde-unknown-top-level-key", "unknown field", "version: 1\nblocks: [{id: w, kind: worker}]\nblokcs: []"),
        // The serde refusal returns before any validation runs: the version
        // error this document also carries must NOT appear.
        case("serde-error-returns-before-validation", "unknown field", "version: 2\nblocks: [{id: w, kind: worker, promt: x}]"),
        case("no-blocks", "no blocks declared", "version: 1\nblocks: []"),
        // "no blocks" is reported only when nothing else was: this list holds the
        // version error alone.
        case("no-blocks-suppressed-by-an-earlier-error", "is not supported", "version: 2\n"),
        // ── blocks: identity ───────────────────────────────────────────────
        case("block-id-too-long", "is longer than", wf(&format!("{{id: {long_id}, kind: worker}}"), "")),
        case("block-id-no-usable-characters", "has no usable characters", wf("{id: \"!!!\", kind: worker}", "")),
        case("block-id-path-traversal-refused", "contains characters that are not allowed", wf("{id: \"../x\", kind: worker}", "")),
        case("block-id-space-refused", "contains characters that are not allowed", wf("{id: \"rev security\", kind: worker}", "")),
        case("block-id-duplicate", "duplicate block id", wf("{id: w, kind: worker}, {id: w, kind: worker}", "")),
        case("block-kind-unknown", "unknown kind", wf("{id: w, kind: boss}", "")),
        case("block-id-reserved-for-another-kind", "is reserved for", wf("{id: planner, kind: reviewer}", "")),
        // ── blocks: cli ────────────────────────────────────────────────────
        case("block-cli-cannot-host-class", "cannot host a planner block", wf("{id: p, kind: planner, cli: pi}", "")),
        case("block-cli-unknown", "unknown cli", wf("{id: w, kind: worker, cli: vim}", "")),
        // ── blocks: persona and capability closure ─────────────────────────
        case("block-prompt-and-profile", "set either prompt:", wf("{id: w, kind: worker, prompt: hi, profile: p.md}", "")),
        case("block-profile-escapes-repo", "must stay inside the repo", wf("{id: w, kind: worker, profile: \"../p.md\"}", "")),
        case("block-orchestrator-persona", "loomux's trust root", wf("{id: orchestrator, kind: orchestrator, prompt: hi, allow: [\"Bash(ls)\"]}", "")),
        case("block-manager-persona", "a manager speaks to the human", wf("{id: m, kind: manager, profile: m.md}", "")),
        case("block-read-only-allow", "its class is read-only", wf("{id: p, kind: planner, allow: [\"Bash(ls)\"]}", "")),
        // ── blocks: role_hint ──────────────────────────────────────────────
        case("block-role-hint-unknown", "unknown role_hint", wf("{id: w, kind: worker, role_hint: Boss}", "")),
        case("block-role-hint-wrong-kind", "requires kind:", wf("{id: w, kind: worker, role_hint: \" Liaison \"}", "")),
        // ── blocks: knobs ──────────────────────────────────────────────────
        case("block-effort-unknown", "unknown effort", wf("{id: w, kind: worker, effort: extreme}", "")),
        case("block-effort-cli-cannot-set", "cannot set effort", wf("{id: w, kind: worker, cli: copilot, effort: high}", "")),
        case("block-context-unknown", "unknown context", wf("{id: w, kind: worker, context: 2m}", "")),
        case("block-context-cli-cannot-set", "cannot set context", wf("{id: w, kind: worker, cli: pi, context: 1m}", "")),
        // ── blocks: driver ─────────────────────────────────────────────────
        case("block-driver-unknown", "unknown driver", wf("{id: w, kind: worker, driver: pty}", "")),
        case("block-driver-cli-has-none", "has no structured driver", wf("{id: w, kind: worker, cli: claude, driver: structured}", "")),
        // ── blocks: remote ─────────────────────────────────────────────────
        case("block-remote-unusable-label", "is not a usable label", wf("{id: w, kind: worker, cli: claude, remote: \"../box\"}", "")),
        case("block-remote-on-orchestrator", "the orchestrator is the trust root, and orchestration state", wf("{id: orchestrator, kind: orchestrator, cli: claude, remote: box}", "")),
        case("block-remote-on-manager", "a manager pane is the human's own interface", wf("{id: m, kind: manager, cli: claude, remote: box}", "")),
        case("block-remote-cli-inherited", "spelled out on the block", wf("{id: w, kind: worker, remote: box}", "")),
        case("block-remote-cli-not-claude", "remote: requires cli: claude", wf("{id: w, kind: worker, cli: copilot, remote: box}", "")),
        // ── blocks: cache_ttl_minutes ──────────────────────────────────────
        case("block-cache-ttl-above-ceiling", "is above the", wf("{id: w, kind: worker, cache_ttl_minutes: 1441}", "")),
        // ── blocks: ordering ───────────────────────────────────────────────
        // First refusal wins per block: `kind` fails before `cli` is looked at.
        case("block-first-refusal-wins", "unknown kind", wf("{id: w, kind: boss, cli: vim}", "")),
        // The id is recorded as seen BEFORE its kind is checked, so the second
        // `x` is a duplicate even though the first `x` was refused.
        case("block-id-seen-before-kind-check", "duplicate block id", wf("{id: x, kind: boss}, {id: x, kind: worker}", "")),
        case("block-errors-accumulate-in-block-order", "has no usable characters", wf("{id: \"!!!\", kind: worker}, {id: w, kind: worker, cli: vim}, {id: ok, kind: worker}", "")),
        // ── roster ─────────────────────────────────────────────────────────
        case("roster-two-managers", "manager blocks declared", wf("{id: m1, kind: manager}, {id: m2, kind: manager}", "")),
        // ── edges ──────────────────────────────────────────────────────────
        case("edge-from-names-no-block", "'from' names no block", wf(W, "edges: [{from: ghost, to: w}]\n")),
        case("edge-to-names-no-block-each-reported", "'to' names no block", wf(W, "edges: [{from: w, to: [g1, w, g2]}]\n")),
        // ── gates: require ─────────────────────────────────────────────────
        case("gate-threshold-zero", "threshold must be a positive number", gate("{threshold: 0, reviewers: [r1]}")),
        case("gate-require-threshold-without-n", "needs a threshold: N", gate("{require: threshold, reviewers: [r1]}")),
        case("gate-all-pass-with-threshold", "all-pass takes no threshold", gate("{require: all-pass, threshold: 1, reviewers: [r1]}")),
        case("gate-require-unknown", "unknown require", gate("{require: most, reviewers: [r1]}")),
        // A `require` refusal skips the rest of that gate: no "no reviewers".
        case("gate-require-refusal-short-circuits", "unknown require", gate("{require: most, reviewers: []}")),
        // ── gates: reviewers ───────────────────────────────────────────────
        case("gate-reviewer-duplicate", "is named more than once", gate("{reviewers: [r1, r1]}")),
        case("gate-reviewer-names-no-block", "names no block", gate("{reviewers: [ghost]}")),
        case("gate-reviewer-not-a-reviewer", "not a reviewer", gate("{reviewers: [w]}")),
        case("gate-reviewer-is-manager", "is a manager", gate("{reviewers: [m]}")),
        case("gate-reviewer-is-liaison", "is a liaison", gate("{reviewers: [li]}")),
        case("gate-no-reviewers", "a gate with no reviewers gates nothing", gate("{reviewers: []}")),
        case("gate-threshold-exceeds-reviewers", "it could never pass", gate("{threshold: 3, reviewers: [r1, r2]}")),
        // ── gates: also / max_diff_lines ───────────────────────────────────
        case("gate-condition-unusable", "is not a usable name", gate("{reviewers: [r1], also: [\"ci green\"]}")),
        case("gate-max-diff-lines-zero", "max_diff_lines must be a positive number", gate("{reviewers: [r1], max_diff_lines: 0}")),
        // ── gates: routing ─────────────────────────────────────────────────
        case("gate-routing-with-threshold", "routing: and require: threshold cannot both be declared", gate("{threshold: 1, reviewers: [r1], routing: [{paths: [\"src/**\"], reviewers: [r2]}]}")),
        case("gate-routing-too-many-rules", "routing rules — at most", gate(&format!("{{reviewers: [r1], routing: [{many_rules}]}}"))),
        case("gate-routing-rule-no-paths", "declares no paths", gate("{reviewers: [r1], routing: [{reviewers: [r2]}]}")),
        case("gate-routing-rule-too-many-paths", "paths — at most", gate(&format!("{{reviewers: [r1], routing: [{{paths: [{many_paths}], reviewers: [r2]}}]}}"))),
        case("gate-routing-rule-no-reviewers", "names no reviewers", gate("{reviewers: [r1], routing: [{paths: [\"src/**\"]}]}")),
        case("gate-routing-path-duplicate", "lists the path",gate("{reviewers: [r1], routing: [{paths: [\"src/**\", \"src/**\"], reviewers: [r2]}]}")),
        case("gate-routing-path-unusable", "is not a usable path glob", gate("{reviewers: [r1], routing: [{paths: [\"src/\"], reviewers: [r2]}]}")),
        case("gate-routing-reviewer-duplicate", "names reviewer", gate("{reviewers: [r1], routing: [{paths: [\"src/**\"], reviewers: [r2, r2]}]}")),
        case("gate-routing-reviewer-names-no-block", "routing rule 1 reviewer", gate("{reviewers: [r1], routing: [{paths: [\"src/**\"], reviewers: [ghost]}]}")),
        // ── gates: ordering ────────────────────────────────────────────────
        // Every non-`require` gate refusal accumulates, in source order.
        case(
            "gate-errors-accumulate-in-order",
            "it could never pass",
            gate("{threshold: 5, reviewers: [r1, r1, ghost], also: [\"a b\"], max_diff_lines: 0, routing: [{paths: [], reviewers: []}]}"),
        ),
        // Gates are read in NAME order (a map), not file order.
        case("gates-reported-in-name-order", "no reviewers", wf(ROSTER, "gates:\n  zeta: {reviewers: []}\n  alpha: {reviewers: []}\n")),
        // ── intake ─────────────────────────────────────────────────────────
        case("intake-source-unknown", "unknown source", wf(W, "intake: {source: jira}\n")),
        case(
            "intake-errors-in-field-order",
            "is not a usable label",
            wf(W, "intake: {source: jira, labels: {ready: \"a b\", investigate: \"-x\", owned: \"!\", prototype: \"p q\", hold: \"h h\"}}\n"),
        ),
        // ── merge_queue ────────────────────────────────────────────────────
        case("merge-queue-max-batch-zero", "a batch of no PRs could never land anything", wf(W, "merge_queue: {max_batch: 0}\n")),
        // ── driver ─────────────────────────────────────────────────────────
        case(
            "driver-counters-out-of-range-in-field-order",
            "must be",
            wf(W, "driver: {max_review_rounds: 4, max_ci_attempts: 0, max_rebase_attempts: 2, plan_review_minutes: 121, planner_timeout_minutes: 14, fix_nonblocking_rounds: 4}\n"),
        ),
        // ── resources ──────────────────────────────────────────────────────
        case("resources-too-many", "are allowed (every name is listed", wf(W, &format!("resources: {{{many_resources}}}\n"))),
        case("resource-name-too-long", "is longer than", wf(W, &format!("resources: {{{long_id}: {{}}}}\n"))),
        case("resource-name-no-usable-characters", "has no usable characters", wf(W, "resources: {\"!!!\": {}}\n")),
        case("resource-name-disallowed-characters", "contains characters that are not allowed", wf(W, "resources: {\"heavy build\": {}}\n")),
        case("resource-slots-zero", "a resource with no slots", wf(W, "resources: {build: {slots: 0}}\n")),
        case("resource-slots-above-max", ".slots: ", wf(W, "resources: {build: {slots: 65}}\n")),
        case("resource-max-hold-zero", "a hold that expires", wf(W, "resources: {build: {max_hold_minutes: 0}}\n")),
        case("resource-max-hold-above-max", "a hold on a scarce resource has to be", wf(W, "resources: {build: {max_hold_minutes: 481}}\n")),
        case("resource-first-refusal-wins", "a resource with no slots", wf(W, "resources: {b: {slots: 0, max_hold_minutes: 0}}\n")),
        // ── board ──────────────────────────────────────────────────────────
        case("board-wip-zero", "a cap of 0 is a", wf(W, "board: {wip: {review: 0, queued: 0}}\n")),
        // ── triage ─────────────────────────────────────────────────────────
        case("triage-provider-not-none", "this build ships the RULE tier only", wf(W, "triage: {provider: typesafe}\n")),
        case("triage-kind-unknown", "is not a delivery kind", wf(W, "triage: {kinds: [delegate-done, bogus]}\n")),
        case("triage-max-defer-out-of-range", "triage.max_defer_minutes: must be", wf(W, "triage: {max_defer_minutes: 0}\n")),
        // ── the order ACROSS sections ──────────────────────────────────────
        case(
            "order-across-sections",
            "is not supported",
            "version: 2\n\
             blocks: [{id: w, kind: boss}, {id: r1, kind: reviewer}]\n\
             edges: [{from: ghost, to: r1}]\n\
             gates: {merge: {reviewers: []}}\n\
             intake: {source: jira}\n\
             merge_queue: {max_batch: 0}\n\
             driver: {max_ci_attempts: 9}\n\
             resources: {\"a b\": {}}\n\
             board: {wip: {pr: 0}}\n\
             triage: {provider: x}\n",
        ),
    ]
}

#[test]
fn every_error_branch_produces_its_golden_error_list() {
    let cases = cases();
    let mut names: Vec<&str> = cases.iter().map(|c| c.name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), cases.len(), "case names must be unique");
    // Population floor: the branch census in the PR body counts one case per
    // branch; a table that shrank below it would read as the same coverage.
    assert!(cases.len() >= 80, "expected at least 80 cases, got {}", cases.len());

    let mut actual = String::new();
    let mut missed: Vec<String> = Vec::new();
    for c in &cases {
        actual.push_str("## ");
        actual.push_str(c.name);
        actual.push('\n');
        // A case that PARSES is recorded, not panicked on, so it shows up as a
        // difference in the golden like any other change — one run then shows
        // every case that moved, rather than stopping at the first.
        let errs = match parse_workflow(&c.yaml) {
            Ok(_) => {
                actual.push_str("(parsed — no refusal)\n");
                missed.push(format!("{}: must be refused, but it parsed", c.name));
                continue;
            }
            Err(errs) => errs,
        };
        if errs.is_empty() {
            actual.push_str("(refused with an EMPTY error list)\n");
        }
        // The golden holds each error Debug-escaped, so an embedded newline or a
        // run of spaces is visible there rather than hidden in a line break.
        if !errs.iter().any(|e| e.contains(c.needle)) {
            missed.push(format!("{}: no error contains {:?} in {errs:#?}", c.name, c.needle));
        }
        for e in &errs {
            actual.push_str(&format!("- {e:?}\n"));
        }
    }
    let golden_ok = matches_golden(&golden_path("errors"), &actual);
    assert!(golden_ok, "the refusal golden differs — see the BEGIN/END block above");
    assert!(missed.is_empty(), "cases that did not reach their named branch:\n{}", missed.join("\n"));
}

/// CLAUDE.md constraint 6 names this refusal as the thing that BOUNDS
/// `sanitize_id`'s weakness: a block id becomes `<id>.md` in the group dir, and
/// `sanitize_id` rewrites rather than refuses (`../x` → `x`), so two strings
/// could name one file were it not that `parse_workflow` rejects any id
/// `sanitize_id` had to change. Pinned by its exact message, beside a positive
/// control that the rewrite really happens — without it, a `sanitize_id` that
/// stopped rewriting would leave this test passing for a reason nobody chose.
#[test]
fn constraint_6_an_id_sanitize_id_had_to_change_is_refused_not_rewritten() {
    assert_eq!(sanitize_id("../x").as_deref(), Some("x"), "positive control: sanitize_id rewrites");
    // The second block IS `x`: had the first been rewritten rather than refused,
    // this would be a duplicate-id error (or, worse, two blocks writing x.md).
    let errs = parse_workflow("version: 1\nblocks: [{id: \"../x\", kind: worker}, {id: x, kind: worker}]\n")
        .expect_err("an id sanitize_id had to change must be refused");
    assert_eq!(
        errs,
        vec![
            "blocks[0]: id \"../x\" contains characters that are not allowed (letters, digits, '-', '_')"
                .to_string()
        ]
    );
}
