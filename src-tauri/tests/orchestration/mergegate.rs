//! The enforced merge gate and the gh/git shims that carry it.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- enforced merge gate (#83) ----------

pub(crate) fn s(v: &str) -> String { v.to_string() }
pub(crate) fn args(a: &[&str]) -> Vec<String> { a.iter().map(|x| x.to_string()).collect() }

#[test]
fn gh_is_merge_invocation_detects_pr_merge_in_every_flag_arrangement() {
    let m = |a: &[&str]| gh_is_merge_invocation(&args(a));
    // The incident form and plain arrangements.
    assert!(m(&["pr", "merge", "123", "--squash"]));
    assert!(m(&["pr", "merge"]));
    assert!(m(&["pr", "merge", "--admin", "--merge"]));
    // rev-79 F1 BLOCKER: -R/--repo (and other globals) BEFORE the command.
    assert!(m(&["-R", "owner/repo", "pr", "merge", "123"]), "gh -R o/r pr merge must be gated");
    assert!(m(&["--repo", "owner/repo", "pr", "merge"]), "gh --repo o/r pr merge must be gated");
    assert!(m(&["--repo=owner/repo", "pr", "merge"]), "gh --repo=o/r must be gated");
    assert!(m(&["-Rowner/repo", "pr", "merge"]), "glued -Ro/r must be gated");
    assert!(m(&["--help", "pr", "merge"]) || true); // (--help would short-circuit gh; harmless here)
    // F1: -R/--repo BETWEEN the command and subcommand (cobra allows interspersing).
    assert!(m(&["pr", "-R", "owner/repo", "merge", "123"]), "gh pr -R o/r merge must be gated");
    assert!(m(&["pr", "--repo=owner/repo", "merge"]));
    // Value-taking merge flags before the selector must not fool detection.
    assert!(m(&["pr", "merge", "--body", "shipping it", "123"]));
    // Raw API merge shapes (the cheap-to-catch bypass), incl. -R before.
    assert!(m(&["api", "--method", "PUT", "repos/o/r/pulls/5/merge"]));
    assert!(m(&["api", "graphql", "-f", "query=mergePullRequest(...)"]));
    // Non-merge gh commands are NOT gated (even with -R before).
    assert!(!m(&["pr", "view", "123"]));
    assert!(!m(&["-R", "owner/repo", "pr", "view", "123"]), "gh -R o/r pr view is not a merge");
    assert!(!m(&["pr", "create", "--fill"]));
    assert!(!m(&["issue", "list"]));
    assert!(!m(&["api", "repos/o/r/pulls"]));
    assert!(!m(&[]));
}

#[test]
fn gh_positionals_and_repo_flag_parse_around_global_flags() {
    // Positionals skip -R/--repo (with value) and boolean flags, wherever they sit.
    let p = |a: &[&str]| gh_positionals(a);
    assert_eq!(p(&["-R", "o/r", "pr", "merge", "123"]), vec!["pr", "merge", "123"]);
    assert_eq!(p(&["pr", "-R", "o/r", "merge"]), vec!["pr", "merge"]);
    assert_eq!(p(&["--repo=o/r", "pr", "merge"]), vec!["pr", "merge"]);
    assert_eq!(p(&["pr", "merge", "--squash", "42"]), vec!["pr", "merge", "42"]);
    // The -R/--repo value is extracted in every accepted form (F2).
    assert_eq!(gh_repo_flag(&["-R", "o/r", "pr", "merge"]).as_deref(), Some("o/r"));
    assert_eq!(gh_repo_flag(&["--repo", "o/r", "pr", "merge"]).as_deref(), Some("o/r"));
    assert_eq!(gh_repo_flag(&["--repo=o/r", "pr", "merge"]).as_deref(), Some("o/r"));
    assert_eq!(gh_repo_flag(&["-Ro/r", "pr", "merge"]).as_deref(), Some("o/r"));
    assert_eq!(gh_repo_flag(&["pr", "-R", "o/r", "merge"]).as_deref(), Some("o/r"));
    assert_eq!(gh_repo_flag(&["pr", "merge", "123"]), None);
}

#[test]
fn gh_gate_decision_enforces_the_human_gate_on_the_default_branch() {
    // (base, default, autonomous, auto_merge, dangerous, grant) — merge invocation.
    let d = |base: Option<&str>, def, auto, am, dang, grant| gh_gate_decision(true, base, def, auto, am, dang, grant);
    // Non-merge → always pass.
    assert_eq!(gh_gate_decision(false, Some("main"), Some("main"), false, false, false, false), GhGate::PassThrough);
    // Merge onto a NON-default base (integration branch) → pass, regardless of markers/grant.
    assert_eq!(d(Some("feat/x"), Some("main"), false, false, false, false), GhGate::PassThrough);
    assert_eq!(d(Some("feat/x"), Some("main"), true, true, false, false), GhGate::PassThrough);
    // Merge onto the DEFAULT branch: blanket-allowed with autonomous+auto_merge.
    assert_eq!(d(Some("main"), Some("main"), true, true, false, false), GhGate::AllowMerge);
    // Supervised dangerous mode (NOT autonomous) → allowed, distinct path.
    assert_eq!(d(Some("main"), Some("main"), false, false, true, false), GhGate::AllowDangerous, "dangerous mode authorizes the merge");
    // dangerous is a NO-OP while autonomous (mutually exclusive; guard is defensive).
    assert_eq!(d(Some("main"), Some("main"), true, false, true, false), GhGate::Block, "dangerous ignored while autonomous, no auto_merge/grant → block");
    // Without blanket/dangerous, a valid one-time GRANT authorizes it (consumed).
    assert_eq!(d(Some("main"), Some("main"), false, false, false, true), GhGate::AllowGrant, "a human grant authorizes the merge");
    assert_eq!(d(Some("main"), Some("main"), true, false, false, true), GhGate::AllowGrant);
    // No markers and no grant → block.
    assert_eq!(d(Some("main"), Some("main"), true, false, false, false), GhGate::Block, "auto_merge off + no grant blocks");
    assert_eq!(d(Some("main"), Some("main"), false, true, false, false), GhGate::Block, "autonomous off + no grant blocks");
    assert_eq!(d(Some("main"), Some("main"), false, false, false, false), GhGate::Block);
    // Undeterminable base → fail-safe block (nothing overrides an unverifiable base).
    assert_eq!(d(None, Some("main"), true, true, true, true), GhGate::BlockUnverifiable);
    assert_eq!(d(Some("main"), None, true, true, true, true), GhGate::BlockUnverifiable);
    assert_eq!(d(Some(""), Some("main"), false, false, true, false), GhGate::BlockUnverifiable, "empty base is unverifiable even in dangerous mode");
}

#[test]
fn grant_helpers_normalize_and_expire() {
    // PR number extraction from every board `pr` form.
    assert_eq!(pr_number("7"), Some(7));
    assert_eq!(pr_number("#42"), Some(42));
    assert_eq!(pr_number("https://github.com/o/r/pull/123"), Some(123));
    assert_eq!(pr_number("PR 9"), Some(9));
    assert_eq!(pr_number("no-number-here"), None);
    // Grant segment sanitization (must match the shim's `tr -c`): path-escaping and
    // odd chars collapse to `_`, safe chars survive.
    assert_eq!(grant_segment("v1.2.3"), "v1.2.3");
    assert_eq!(grant_segment("release/../../etc"), "release_.._.._etc");
    assert_eq!(grant_segment("v1/beta"), "v1_beta");
    assert_eq!(grant_segment(""), "_");
    // TTL rule: unexpired iff a future expiry exists.
    assert!(grant_unexpired(Some(100), 99));
    assert!(!grant_unexpired(Some(100), 100), "exactly at expiry is expired");
    assert!(!grant_unexpired(Some(100), 200));
    assert!(!grant_unexpired(None, 0), "no grant file → not authorized");
}

#[test]
fn release_gate_allows_via_auto_release_dangerous_or_grant() {
    // (autonomous, auto_release, dangerous, grant). Parallel to merges.
    let d = |a, ar, dang, g| release_gate_decision(a, ar, dang, g);
    // Blanket: autonomous + auto_release → allowed (not grant-consumed).
    assert_eq!(d(true, true, false, false), GhGate::AllowMerge);
    // Supervised dangerous mode (NOT autonomous) → allowed, distinct path.
    assert_eq!(d(false, false, true, false), GhGate::AllowDangerous, "dangerous mode publishes");
    assert_eq!(d(true, false, true, false), GhGate::Block, "dangerous ignored while autonomous, no auto_release/grant → block");
    // auto_release OFF but a valid per-tag grant → allowed (consumed).
    assert_eq!(d(true, false, false, true), GhGate::AllowGrant);
    assert_eq!(d(false, false, false, true), GhGate::AllowGrant, "grant works even non-autonomous");
    // No blanket opt-in, no dangerous, no grant → blocked (conservative default).
    assert_eq!(d(true, false, false, false), GhGate::Block, "autonomous alone never publishes");
    assert_eq!(d(false, true, false, false), GhGate::Block, "auto_release without autonomous never publishes");
    assert_eq!(d(false, false, false, false), GhGate::Block);
}

#[test]
fn gh_release_action_detects_publish_subcommands_and_tag() {
    let a = |v: &[&str]| gh_release_action(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(a(&["release", "create", "v1.2.3"]), Some(("create".into(), "v1.2.3".into())));
    assert_eq!(a(&["release", "edit", "v1.2.3", "--draft=false"]), Some(("edit".into(), "v1.2.3".into())));
    assert_eq!(a(&["release", "delete", "v9"]), Some(("delete".into(), "v9".into())));
    // -R/--repo before the command is skipped, tag still found.
    assert_eq!(a(&["-R", "o/r", "release", "create", "v2"]), Some(("create".into(), "v2".into())));
    // rev-86 LOW: value-flags BEFORE the tag (title/notes/target…) must be consumed
    // so the tag positional isn't mis-read as the flag's value.
    assert_eq!(a(&["release", "create", "--title", "My Release", "v1.2.3"]),
        Some(("create".into(), "v1.2.3".into())), "--title value must not be mistaken for the tag");
    assert_eq!(a(&["release", "create", "-n", "some notes", "--target", "main", "v1.2.3"]),
        Some(("create".into(), "v1.2.3".into())));
    assert_eq!(a(&["release", "create", "--title=X", "v1.2.3"]), Some(("create".into(), "v1.2.3".into())));
    // Tag first, flags after (also fine).
    assert_eq!(a(&["release", "create", "v1.2.3", "--title", "X", "--generate-notes"]),
        Some(("create".into(), "v1.2.3".into())));
    // Read-only release subcommands and non-release commands are NOT publish actions.
    assert_eq!(a(&["release", "view", "v1"]), None);
    assert_eq!(a(&["release", "list"]), None);
    assert_eq!(a(&["pr", "merge", "1"]), None);
}

#[test]
fn git_tag_push_classifies_tag_pushes() {
    let g = |v: &[&str]| git_tag_push(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    // Explicit tag refs and the `tag <name>` form → gate that tag.
    assert_eq!(g(&["push", "origin", "refs/tags/v1.2.3"]), GitTagPush::Tag("v1.2.3".into()));
    assert_eq!(g(&["push", "origin", "tag", "v9"]), GitTagPush::Tag("v9".into()));
    assert_eq!(g(&["push", "origin", "+v1:refs/tags/v1"]), GitTagPush::Tag("v1".into()));
    // A bare refspec matching release.yml's `v*` trigger is a (candidate) tag; the
    // shim confirms it against real git. rev-86 BLOCKER: `v*` is ANY v-prefixed
    // ref, not just `v<digit>` — `vbeta`/`vRelease`/`vv1.0.0` would publish yet the
    // old `v[0-9]` pattern let them slip.
    assert_eq!(g(&["push", "origin", "v1.0.0"]), GitTagPush::Tag("v1.0.0".into()));
    assert_eq!(g(&["push", "origin", "vbeta"]), GitTagPush::Tag("vbeta".into()), "vbeta matches v*");
    assert_eq!(g(&["push", "origin", "vRelease"]), GitTagPush::Tag("vRelease".into()));
    assert_eq!(g(&["push", "origin", "vv1.0.0"]), GitTagPush::Tag("vv1.0.0".into()));
    // Non-`v*` refs never trigger release.yml, so they are NOT candidates — pinned
    // so the scope stays explicit (a `nightly` tag would not publish).
    assert_eq!(g(&["push", "origin", "nightly"]), GitTagPush::None, "non-v* ref is not a release candidate");
    assert_eq!(g(&["push", "origin", "release-1"]), GitTagPush::None);
    // Bulk tag pushes → Bulk (blocked; can't match one grant).
    assert_eq!(g(&["push", "--tags"]), GitTagPush::Bulk);
    assert_eq!(g(&["push", "origin", "--follow-tags"]), GitTagPush::Bulk);
    assert_eq!(g(&["push", "--mirror"]), GitTagPush::Bulk);
    // Plain branch pushes and non-push commands → None (fast passthrough). A
    // `v*`-prefixed branch is a candidate here but the shim confirms-away non-tags.
    assert_eq!(g(&["push", "origin", "feat/x"]), GitTagPush::None);
    assert_eq!(g(&["push", "-u", "origin", "HEAD"]), GitTagPush::None);
    assert_eq!(g(&["push", "origin", "main"]), GitTagPush::None);
    assert_eq!(g(&["-C", "/repo", "push", "origin", "main"]), GitTagPush::None, "git globals skipped");
    assert_eq!(g(&["status"]), GitTagPush::None);
    assert_eq!(g(&["commit", "-m", "x"]), GitTagPush::None);
}

#[test]
fn git_shim_script_bakes_real_git_and_gates_tag_push() {
    let sh = git_shim_sh("C:/Program Files/Git/cmd/git.exe", &shim_paths());
    assert!(sh.contains("REAL_GIT=\"C:/Program Files/Git/cmd/git.exe\""), "bakes the real git path");
    assert!(sh.starts_with("#!/bin/sh"));
    // Only `git push` is inspected; everything else execs immediately.
    assert!(sh.contains("if [ \"$cmd\" != \"push\" ]") && sh.contains("exec \"$REAL_GIT\" \"$@\""));
    assert!(sh.contains("--tags") && sh.contains("--follow-tags"), "blocks bulk tag pushes");
    assert!(sh.contains("refs/tags/"), "detects explicit tag refs");
    assert!(sh.contains("release_grants/"), "gates on a release grant");
    assert!(sh.contains("release-gate-blocked"), "audits refusals");
    // #438: the tag push checks the release grant and never spends it — a
    // release grant covers its tag's whole pipeline until its TTL. The #256/
    // #303/#315 bug class (a grant burned on a step that never landed) is
    // therefore structural here, not settle-dependent.
    assert!(sh.contains("loomux_release_grant_valid"),
        "the tag-push gate consults the shared release-grant validity check");
    assert!(!sh.contains("loomux_grant_settle"),
        "nothing in the git shim consumes a grant, so there is nothing to settle");
    assert!(!sh.contains("\r"), "the POSIX git shim must be LF-only");
}

/// #3202: `%3N` (milliseconds) is a GNU coreutils extension. BSD `date` — which
/// macOS ships as /bin/date — does not implement `%N` at all and does not fail
/// on it either: `+%s%3N` yields `<seconds>3N`, so every audit row the gh or
/// git shim wrote on macOS carried a garbage `ts_ms` that made the whole line
/// unparseable JSON. The `[ -z "$ts" ]` fallback the audit functions had is
/// exactly the check that cannot see this failure — it catches `date` printing
/// NOTHING, never `date` printing the WRONG thing — and the defect is invisible
/// on every platform the behavioural harness tests run on (Linux and Git-Bash
/// carry GNU `date`, whose `%s%3N` is true milliseconds), which is why those
/// tests stayed green. Every rendered POSIX shim must carry that ONE portable
/// form at every ts site — pinned as text so it is red on every platform, not
/// only where BSD `date` runs. (The .cmd shims never shell out to `date`: their
/// degraded rows hardcode `"ts_ms":0`, so there is no ts site to pin there.)
///
/// #3249 item 2: the guard used to accept ANY all-digit `%s%3N` result —
/// magnitude-blind, exactly the adversary the #3248 premortem flagged: a
/// `date` that answers `%s%3N` with plain seconds is all-digit, and trusting
/// it stamps ts_ms a thousandfold too small. The rendered block must carry a
/// 13-digit accept arm (the epoch-ms width for 2001–2286) and refuse every
/// other all-digit magnitude outright (`ts=0`) — a value that already
/// misbehaved is not re-consulted, so the whole-seconds rung runs only for
/// a non-digit or empty answer; the behavioural twin
/// (`gh_shim_audit_ts_rejects_a_seconds_magnitude_from_date`, below) runs
/// that adversary through the rendered shim.
///
/// #3259 items 1+4: the accept arms are also CANONICAL and SELF-CONTAINED.
/// A zero-padded answer (`0170000000`) is all-digit and the right WIDTH, so
/// only a non-zero leading digit spelled in the arm itself can refuse it —
/// a bare interpolation (`"ts_ms":0170000000`) stops the audit line being
/// JSON at all. And each arm spelling its whole accept shape (width AND
/// alphabet AND non-zero lead) is what removes the ordering from between
/// the ACCEPT arms and the junk arm: a reordered case still gives every
/// input the same verdict among those arms — the catch-all `*)` is the
/// exception and must stay last (see the happy-path pin). The behavioural
/// twins:
/// `gh_shim_audit_ts_refuses_a_leading_zero_timestamp` (both arms' zero-pad
/// adversary), `gh_shim_audit_ts_junk_of_the_accept_width_reaches_the_rung`
/// (the reorder adversary: a 13-character NON-digit answer must reach the
/// whole-seconds rung, not the 13-digit accept arm), and
/// `gh_shim_audit_ts_falls_back_to_whole_seconds_when_date_lacks_percent_n`.
#[test]
fn every_rendered_shim_timestamps_with_the_portable_ms_fallback() {
    // The array is BUILT from PINNED_SHIMS, not hand-enumerated beside it
    // (rev-final round 2 finding 1): a name added to the population const
    // without an arm here panics below, so the census's failure sequence —
    // add a renderer, bump the const, bump the SANCTIONED count — dead-ends
    // red here, where the ts sites are actually checked, instead of ending
    // green with a fourth shim pinned by nothing.
    let shims: Vec<(&str, String)> = PINNED_SHIMS
        .iter()
        .map(|name| match *name {
            "gh" => ("gh", gh_shim_sh("C:/Program Files/GitHub CLI/gh.exe", &shim_paths())),
            "git" => ("git", git_shim_sh("C:/Program Files/Git/cmd/git.exe", &shim_paths())),
            "loomux" => ("loomux", loomux_shim_sh()),
            other => panic!(
                "PINNED_SHIMS names `{other}` but this pin renders no shim for it — \
                 the census holds the population to the const, so a name added there \
                 without an arm above has its ts sites checked by nothing (#3249)"
            ),
        })
        .collect();
    for (name, sh) in &shims {
        // The `%s%3N` attempt itself MUST survive: GNU's all-digit result is
        // used as-is, so removing it would cost Linux and Git-Bash true
        // millisecond precision. What must be gone is a `%s%3N` value trusted
        // with only an emptiness check.
        const BROKEN: &str = "[ -z \"$ts\" ] && ts=0";
        // #3259 item 1: the accept arms are CANONICAL. A zero-padded answer
        // is all-digit and can carry the right magnitude, so neither the
        // junk arm nor a width check alone refuses it: `0170000000` used to
        // pass the ten-`?` arm and interpolate `"ts_ms":0170000000000`, a
        // line no JSON parser accepts. Every accept arm therefore requires
        // a non-zero leading digit — the first class is `[1-9]`, every later
        // position `[0-9]`, and the run ends in the arm's own `)`: an arm
        // one class wider reads `…[0-9])` where the const needs `)`, so the
        // same width-safety the ten-`?` consts carried holds (width 9 and
        // width 11 match 0 — measured, see the PR body's width table).
        const SECONDS_ARM: &str =
            "[1-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]) ts=\"${ts}000\" ;;";
        // #3249 item 2 + #3259 item 1: exactly a CANONICAL 13-digit value
        // (epoch-ms width AND a non-zero lead) is trusted as-is; every other
        // all-digit magnitude, and every zero-padded form, is refused.
        const MS_ARM: &str =
            "[1-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]) ;;";
        // #3259 item 4: each accept arm states its WHOLE accept shape —
        // width, alphabet and non-zero lead in the arm itself — so no
        // ACCEPT arm depends on the junk arm running before it; the
        // catch-all `*)` must stay LAST (only the happy-path pin sees a
        // demoted catch-all — every refusal pin expects 0). The reorder
        // adversary is run through the rendered shim by
        // gh_shim_audit_ts_junk_of_the_accept_width_reaches_the_rung.
        // The rung is only a SECOND CHANCE: every `%s%3N` site re-consults
        // `date` with plain `%s` exactly once — a `%s%3N` answer that
        // already produced digits is never re-consulted (#3249).
        const SECONDS_CONSULT: &str = "ts=$(date +%s 2>/dev/null)";
        // The bare reject arm appears TWICE per ts site once the whole-seconds
        // rung carries its own magnitude guard: the rung's reject and the
        // outer all-other-magnitude reject.
        const NOT_MS: &str = "*) ts=0 ;;";
        let sites = sh.matches("ts=$(date +%s%3N 2>/dev/null)").count();
        assert!(sites > 0, "the {name} shim must timestamp its audit rows (non-vacuity)");
        assert_eq!(
            sites,
            sh.matches(MS_ARM).count(),
            "the {name} shim has {sites} ts site(s) but {} canonical 13-digit accept arm(s) (thirteen classes, first `[1-9]`) — an all-digit guard is magnitude-blind AND a leading-zero digit string interpolated bare stops the audit line being JSON (#3248 premortem, #3249, #3259 item 1)",
            sh.matches(MS_ARM).count()
        );
        assert_eq!(
            sites,
            sh.matches(SECONDS_ARM).count(),
            "the {name} shim has {sites} ts site(s) but {} canonical whole-seconds arm(s) (ten classes, first `[1-9]`) — the whole-seconds rung must refuse a wrong-magnitude `%s` answer (9 or 11 digits) and a zero-padded one, not append `000` into a ts_ms no JSON parser accepts (#3249 residual 1, #3259 item 1)",
            sh.matches(SECONDS_ARM).count()
        );
        assert_eq!(
            sites,
            sh.matches(SECONDS_CONSULT).count(),
            "the {name} shim has {sites} `%s%3N` site(s) but {} whole-seconds re-consult(s) — every ts site must reuse the self-launch shim's portable form, one `date +%s` second chance per site and never for a `%s%3N` answer that already produced digits (#3202, #3249)",
            sh.matches(SECONDS_CONSULT).count()
        );
        assert_eq!(
            2 * sites,
            sh.matches(NOT_MS).count(),
            "the {name} shim has {sites} ts site(s) but {} bare reject arm(s) — there are two per site once the whole-seconds rung carries its own magnitude guard (#3249 residual 1): the rung's reject and the outer all-other-magnitude reject",
            sh.matches(NOT_MS).count()
        );
        assert!(!sh.contains(BROKEN),
            "the {name} shim still trusts `%s%3N` with only an emptiness check — on BSD `date` (macOS) that prints a literal `3N` tail and every audit row stops being JSON (#3202)");
    }
}

/// The POSIX shims this file pins, by name — one entry per renderer, the
/// population the census (`a_rendered_shim_renderer_cannot_hide_from_the_ts_pin`)
/// holds to a scan of every production source root, and the const the text
/// pin above BUILDS its entries from: a name added here without a rendering
/// arm in every_rendered_shim_timestamps_with_the_portable_ms_fallback
/// panics there, so the two populations cannot drift apart (rev-final
/// round 2 finding 1).
pub(crate) const PINNED_SHIMS: [&str; 3] = ["gh", "git", "loomux"];

/// #3249 item 2 — the behavioural twin of the text pin above, for the one
/// adversary the text pin cannot run: a `date` that answers `%s%3N` with a
/// plain seconds value (all-digit, 10 digits — the #3248 premortem's
/// magnitude-blind case). Fed through the PATH repair the shim itself
/// performs (the same fixture shape as
/// `gh_shim_refuses_when_tr_resolves_but_cannot_run`, so it arms on every
/// platform), the rendered gh shim's audit row must carry EXACTLY the ts=0
/// sentinel — the adversary refused, and the proof the fake really reached
/// the shim: a PATH repair that stopped putting the fixture ahead of the
/// system would let the real GNU `date` answer 13 digits and this exact-0
/// assertion reddens rather than passing (rev-final round 2 finding 3).
/// The 3N polarity (item 3) is pinned by
/// `gh_shim_audit_ts_falls_back_to_whole_seconds_when_date_lacks_percent_n`,
/// below; the whole-seconds rung's own magnitude by
/// `gh_shim_audit_ts_refuses_a_wrong_magnitude_whole_seconds_answer`; the
/// accept arms' canonicality and order-independence by
/// `gh_shim_audit_ts_refuses_a_leading_zero_timestamp` and
/// `gh_shim_audit_ts_junk_of_the_accept_width_reaches_the_rung`. All four
/// arm through `ts_pin_sh` — a sh-less host is a hard failure, not a skip
/// (#3259 item 3).
#[test]
fn gh_shim_audit_ts_rejects_a_seconds_magnitude_from_date() {
    let sh = ts_pin_sh("gh_shim_audit_ts_rejects_a_seconds_magnitude_from_date");
    let td = tempfile::tempdir().unwrap();
    // The premortem adversary: a `date` that prints plain seconds for every
    // format, so `%s%3N` is all-digit — and 10 digits, not 13.
    let (audit, marker) = run_gh_shim_audit_with_fake_date(
        &sh,
        td.path(),
        &[("+%s%3N", "1700000000"), ("+%s", "1700000000")],
        "1700000000",
    );
    // THE positive control (#3249 residual 3): the fake must actually have
    // run — a host that reaches NO `date` at all also lands on the exact-0
    // refusal, and without this control the test could not tell "the guard
    // refused a bad value" from "there was nothing to refuse". And exactly
    // one invocation, for the `%s%3N` attempt alone: a value that already
    // misbehaved is not re-consulted.
    assert_eq!(
        marker.lines().collect::<Vec<_>>(),
        ["+%s%3N"],
        "the fake date must have been invoked exactly once, for the `%s%3N` attempt \
         alone (positive control against a vacuous green when no date runs): {marker:?}"
    );
    assert_eq!(
        audit.lines().filter(|l| l.contains("merge-gate-blocked")).count(),
        1,
        "the refusal must be audited exactly once (non-vacuity): {audit}"
    );
    for line in audit.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value =
            serde_json::from_str(line).expect("each audit line is valid JSON");
        let ts = v["ts_ms"]
            .as_u64()
            .unwrap_or_else(|| panic!("ts_ms must be numeric: {line}"));
        // THE positive control (rev-final round 2 finding 3): exact `0` — the
        // refusal value — cannot be produced by the real GNU `date` (13
        // digits), so this pins not only that the adversary was refused but
        // that the fake `date` is the one the shim actually found.
        assert_eq!(
            ts, 0,
            "a date that answers %s%3N with plain seconds must be refused outright \
             (ts=0), not trusted as milliseconds: got {ts} in {line}"
        );
    }
}

/// One rendered-gh-shim audit run against a scripted fake `date`, shared by
/// the three ts behavioural pins. The fake logs every invocation to a
/// marker file — the positive control that separates "the guard refused a
/// bad value" from "no `date` was reached at all" (#3249 residual 3): the
/// ts=0 sentinel reads green either way, so a caller asserts on the marker
/// before asserting on ts. `date_answers` maps the formats the shim uses
/// (`+%s%3N`, `+%s`) to their scripted outputs; any other format gets
/// `other_answer`. Runs the no-grant refusal path (the one that audits)
/// exactly once and returns `(audit text, marker text)`.
fn run_gh_shim_audit_with_fake_date(
    sh: &str,
    root: &std::path::Path,
    date_answers: &[(&str, &str)],
    other_answer: &str,
) -> (String, String) {
    use std::process::Command;
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let utils = root.join("utils");
    std::fs::create_dir_all(&utils).unwrap();
    let marker = root.join("date-invocations.log");
    let fake_date = utils.join("date");
    let mut script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\ncase \"$1\" in\n",
        marker.display()
    );
    for (fmt, answer) in date_answers {
        script.push_str(&format!("  \"{fmt}\") printf '%s\\n' \"{answer}\" ;;\n"));
    }
    script
        .push_str(&format!("  *) printf '%s\\n' \"{other_answer}\" ;;\nesac\n"));
    std::fs::write(&fake_date, script).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh(root, &log);
    let shim = root.join("gh");
    std::fs::write(
        &shim,
        gh_shim_sh(
            &fake.display().to_string(),
            &ShimPaths { utils_dir: Some(msys_dir_for_fixture(&utils)), git_dir: None },
        ),
    )
    .unwrap();
    let _ = Command::new(sh)
        .arg("-c")
        .arg(format!("chmod +x '{}' '{}'", fake.display(), fake_date.display()))
        .status();
    // No merge grant: `pr merge` takes the refusal path — exactly the one
    // that audits — so the row's ts_ms is what the callers are about.
    let out = Command::new(sh)
        .arg(&shim)
        .args(["pr", "merge", "5"])
        .env("LOOMUX_GROUP_DIR", &group)
        .env("FAKE_BASE", "main")
        .env("FAKE_DEFAULT", "main")
        .env("FAKE_NUM", "5")
        .output()
        .unwrap();
    assert!(!out.status.success(), "no grant → blocked");
    (
        std::fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default(),
        std::fs::read_to_string(&marker).unwrap_or_default(),
    )
}

/// #3249 item 3 — the pin for the polarity the text pin cannot see: a
/// `date` with no `%N` support (BSD's polarity, the whole #3202 defect)
/// answers `%s%3N` with the epoch seconds plus a literal `3N` tail — a
/// NON-digit answer — so the outer case's junk arm must route it to the
/// whole-seconds rung, and the rung's answer (a known 10-digit epoch) must
/// land ts_ms on an all-digit 13-DIGIT millisecond value. A polarity edit to
/// the outer case arm (junk accepted, digits refused) sends the `3N` junk
/// straight into `ts_ms` or the value to the refuse arm, and either flip
/// reddens here — the text pin's fixed strings stay untouched by it.
/// The rung's ACCEPT side (exactly 10 digits) is pinned by this same run.
#[test]
fn gh_shim_audit_ts_falls_back_to_whole_seconds_when_date_lacks_percent_n() {
    let sh = ts_pin_sh("gh_shim_audit_ts_falls_back_to_whole_seconds_when_date_lacks_percent_n");
    let td = tempfile::tempdir().unwrap();
    // BSD polarity: `%s%3N` is the epoch with a literal `3N` glued on;
    // `%s` is a known 10-digit epoch second.
    let (audit, marker) = run_gh_shim_audit_with_fake_date(
        &sh,
        td.path(),
        &[("+%s%3N", "17000000003N"), ("+%s", "1700000000")],
        "2025-01-01 00:00:00",
    );
    // Positive control (#3249 residual 3) and the fallback rung's receipt:
    // the fake answered `%s%3N` AND the junk arm consulted `date +%s` —
    // a shim that never reached `date` could produce neither line.
    assert_eq!(
        marker.lines().collect::<Vec<_>>(),
        ["+%s%3N", "+%s"],
        "the fake date must have been invoked for both rungs (positive control \
         against a vacuous green when no date runs): {marker:?}"
    );
    assert_eq!(
        audit.lines().filter(|l| l.contains("merge-gate-blocked")).count(),
        1,
        "the refusal must be audited exactly once (non-vacuity): {audit}"
    );
    for line in audit.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value =
            serde_json::from_str(line).expect("each audit line is valid JSON");
        let ts = v["ts_ms"]
            .as_u64()
            .unwrap_or_else(|| panic!("ts_ms must be numeric: {line}"));
        assert_eq!(
            ts, 1700000000000,
            "a date without %N must fall through the junk arm to the whole-seconds \
             rung and stamp 13-digit epoch-ms: got {ts} in {line}"
        );
    }
}

/// #3249 residual 1 — the whole-seconds rung's own magnitude guard. Its
/// `%s` answer is a DIFFERENT adversary from the 13-digit accept arm: all-
/// digit by construction here, so only a magnitude check can refuse it, and
/// without one a 9-digit (pre-2001 epoch) or 11-digit (year ~2286+) answer
/// becomes a 12- or 14-digit ts_ms. Both wrong sides are run through the
/// rendered shim and must land on the ts=0 sentinel; the rung's accept side
/// (exactly 10 digits) is pinned by
/// `gh_shim_audit_ts_falls_back_to_whole_seconds_when_date_lacks_percent_n`.
#[test]
fn gh_shim_audit_ts_refuses_a_wrong_magnitude_whole_seconds_answer() {
    let sh = ts_pin_sh("gh_shim_audit_ts_refuses_a_wrong_magnitude_whole_seconds_answer");
    for (label, seconds) in [("9-digit", "170000000"), ("11-digit", "17000000000")] {
        let td = tempfile::tempdir().unwrap();
        // The `3N` junk answer is what sends the guard down the
        // whole-seconds rung — the rung is the adversary's second chance,
        // so its own answer is what must be magnitude-checked.
        let (audit, marker) = run_gh_shim_audit_with_fake_date(
            &sh,
            td.path(),
            &[("+%s%3N", "17000000003N"), ("+%s", seconds)],
            "3N",
        );
        assert_eq!(
            marker.lines().collect::<Vec<_>>(),
            ["+%s%3N", "+%s"],
            "the fake date must have been invoked for both rungs (positive control, \
             #3249 residual 3): {marker:?}"
        );
        assert_eq!(
            audit.lines().filter(|l| l.contains("merge-gate-blocked")).count(),
            1,
            "the {label} refusal must be audited exactly once (non-vacuity): {audit}"
        );
        for line in audit.lines().filter(|l| !l.trim().is_empty()) {
            let v: serde_json::Value =
                serde_json::from_str(line).expect("each audit line is valid JSON");
            let ts = v["ts_ms"]
                .as_u64()
                .unwrap_or_else(|| panic!("ts_ms must be numeric: {line}"));
            assert_eq!(
                ts, 0,
                "a {label} whole-seconds answer must be refused outright (ts=0), not \
                 appended `000` into a wrong-magnitude ts_ms: got {ts} in {line}"
            );
        }
    }
}

/// #3259 item 1 — canonicality, on both accept arms. A zero-padded answer
/// is all-digit and the right WIDTH, so neither the junk arm nor the width
/// alone refuses it: a PATH-rogue `date` answering `0170000000` to `+%s`
/// used to pass the ten-`?` arm and interpolate `"ts_ms":0170000000000` —
/// a bare leading-zero literal, which no JSON parser accepts — and the
/// same defect on the outer arm (`0170000000000` from `+%s%3N`) never even
/// reached the rung. Both adversaries are run through the rendered shim:
/// the row must land on the ts=0 sentinel AND parse (the pre-fix shim
/// emits a line serde_json refuses, which is the red this pin is cut for).
/// The reorder twin (a NON-digit answer of the accept width) is pinned by
/// `gh_shim_audit_ts_junk_of_the_accept_width_reaches_the_rung`.
#[test]
fn gh_shim_audit_ts_refuses_a_leading_zero_timestamp() {
    let sh = ts_pin_sh("gh_shim_audit_ts_refuses_a_leading_zero_timestamp");
    for (label, answers, want_marker) in [
        // The outer arm's adversary: `%s%3N` itself answers zero-padded —
        // refused outright, so the fake is never consulted a second time.
        (
            "13-digit",
            &[("+%s%3N", "0170000000000")] as &[(&str, &str)],
            &["+%s%3N"] as &[&str],
        ),
        // The rung's adversary: the `3N` junk answer is what routes the
        // guard to the whole-seconds rung, whose own answer is zero-padded.
        (
            "10-digit",
            &[("+%s%3N", "3N"), ("+%s", "0170000000")] as &[(&str, &str)],
            &["+%s%3N", "+%s"] as &[&str],
        ),
    ] {
        let td = tempfile::tempdir().unwrap();
        let (audit, marker) = run_gh_shim_audit_with_fake_date(&sh, td.path(), answers, "3N");
        // Positive control (#3249 residual 3): the fake must actually have
        // run — a host that reaches NO `date` also lands on ts=0.
        assert_eq!(
            marker.lines().collect::<Vec<_>>(),
            want_marker,
            "the {label} scenario's fake date must have been invoked for exactly \
             the rungs it reaches (positive control): {marker:?}"
        );
        assert_eq!(
            audit.lines().filter(|l| l.contains("merge-gate-blocked")).count(),
            1,
            "the {label} refusal must be audited exactly once (non-vacuity): {audit}"
        );
        for line in audit.lines().filter(|l| !l.trim().is_empty()) {
            let v: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| {
                panic!(
                    "ts_ms must stay JSON-parseable — a zero-padded answer \
                     interpolated bare is a leading-zero literal, invalid JSON \
                     (#3259 item 1): {e} in {line}"
                )
            });
            assert_eq!(
                v["ts_ms"].as_u64(),
                Some(0),
                "a zero-padded {label} answer must be refused outright (ts=0), not \
                 interpolated into the audit line: got {v} in {line}"
            );
        }
    }
}

/// #3259 item 4 — the accept shape is in the arm, so no ACCEPT arm depends
/// on the junk arm running before it. The adversary the OLD ladder could
/// not survive reordered: a 13-character NON-digit answer would have
/// matched the bare ten-`?`-run accept arm (`?????????????` matches any 13
/// characters) the moment the junk arm stopped running first, and landed
/// in ts_ms unquoted. With the accept shape spelled in the arm (`[1-9]`
/// then digit classes), the same input fails the arm on its ALPHABET and
/// reaches the whole-seconds rung whatever order the arms are in — this
/// pin runs it through the rendered shim in the shipping order and
/// demands the rung's verdict. SCOPE, stated because the claim was
/// over-broad once: this pins accept-arm-vs-junk-arm only. The catch-all
/// `*) ts=0 ;;` is NOT order-independent — moved above the 13-digit
/// accept arm it silently zero-stamps every audit row while every
/// refusal pin stays green — which is why the catch-all must stay last,
/// and why `gh_shim_audit_ts_trusts_a_good_millisecond_answer` (the
/// happy path, below) exists.
#[test]
fn gh_shim_audit_ts_junk_of_the_accept_width_reaches_the_rung() {
    let sh = ts_pin_sh("gh_shim_audit_ts_junk_of_the_accept_width_reaches_the_rung");
    let td = tempfile::tempdir().unwrap();
    // 13 characters, none a digit — the outer accept arm's width exactly.
    // `%s` is a known 10-digit epoch second, so the rung's accept side is
    // what must answer.
    let (audit, marker) = run_gh_shim_audit_with_fake_date(
        &sh,
        td.path(),
        &[("+%s%3N", "abcdefghijklm"), ("+%s", "1700000000")],
        "3N",
    );
    assert_eq!(
        marker.lines().collect::<Vec<_>>(),
        ["+%s%3N", "+%s"],
        "the junk-width answer must be routed to the rung (positive control): {marker:?}"
    );
    assert_eq!(
        audit.lines().filter(|l| l.contains("merge-gate-blocked")).count(),
        1,
        "the refusal must be audited exactly once (non-vacuity): {audit}"
    );
    for line in audit.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("audit line must be JSON ({e}): {line}"));
        assert_eq!(
            v["ts_ms"].as_u64(),
            Some(1700000000000),
            "a 13-character non-digit answer must reach the whole-seconds rung \
             (the accept arm matches on shape, not on luck of the ordering): \
             got {v} in {line}"
        );
    }
}

/// The HAPPY PATH — the pin the refusal pins cannot be: a good, canonical
/// 13-digit `%s%3N` answer (what GNU `date` answers on Linux and Git Bash)
/// must be trusted AS-IS — `ts_ms` is the value itself, not the ts=0
/// sentinel. Every other ts pin feeds an adversary, so every one of them
/// expects 0; that polarity is why demoting the catch-all `*) ts=0 ;;`
/// above the 13-digit accept arm greens the whole suite while silently
/// zero-stamping every audit row (rev round 1, finding 1's premortem —
/// the reviewer ran the permutation). This run's exact-value assertion is
/// what makes that demotion red: the permuted ladder answers 0.
#[test]
fn gh_shim_audit_ts_trusts_a_good_millisecond_answer() {
    let sh = ts_pin_sh("gh_shim_audit_ts_trusts_a_good_millisecond_answer");
    let td = tempfile::tempdir().unwrap();
    let (audit, marker) = run_gh_shim_audit_with_fake_date(
        &sh,
        td.path(),
        &[("+%s%3N", "1700000000000")],
        "3N",
    );
    // Positive control (#3249 residual 3): the fake must actually have run
    // — a host that reaches NO `date` also lands on ts=0, and this pin's
    // whole point is to separate the trusted answer from the sentinel.
    assert_eq!(
        marker.lines().collect::<Vec<_>>(),
        ["+%s%3N"],
        "the fake date must have been invoked exactly once, for the `%s%3N` \
         attempt (positive control): {marker:?}"
    );
    assert_eq!(
        audit.lines().filter(|l| l.contains("merge-gate-blocked")).count(),
        1,
        "the refusal must be audited exactly once (non-vacuity): {audit}"
    );
    for line in audit.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("audit line must be JSON ({e}): {line}"));
        assert_eq!(
            v["ts_ms"].as_u64(),
            Some(1700000000000),
            "a good canonical 13-digit answer must be trusted as-is — a demoted \
             catch-all or a broken accept arm zero-stamps every audit row \
             silently: got {v} in {line}"
        );
    }
}

/// #3259 item 3 — the hard-fail itself. A host that CANNOT resolve a `sh`
/// must panic, naming the decision, never skip: `pin_could_not_arm`'s skip
/// path is exactly the silent un-arming this closes. The None case is fed
/// BY HAND (`ts_pin_sh_resolved`) because no CI leg can produce it without
/// going red — that is the point. (Round 2 finding: the first cut of this
/// pin called `ts_pin_sh` for the None case too, which on CI consults a
/// host that HAS a sh and so never reached the guard — the fixed run reds
/// on all three legs with exactly this panic, and the parameter split is
/// what makes the guard's red reachable at all.)
#[test]
fn ts_pins_hard_fail_when_posix_sh_cannot_be_resolved() {
    // Positive controls: an ARMED host answers with a sh, never the panic,
    // through BOTH layers — the live one really consults the resolution,
    // and the parameterized one passes Some through unchanged.
    let sh = ts_pin_sh("positive control");
    assert!(!sh.is_empty(), "an armed host's sh path is non-empty: {sh:?}");
    assert_eq!(
        ts_pin_sh_resolved("positive control", Some("/bin/sh".to_string())),
        "/bin/sh",
        "a Some resolution is passed through, not second-guessed"
    );
    // The guard: None → panic (caught here; a panic that ESCAPED would be
    // a legitimate red on a genuinely sh-less host).
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ts_pin_sh_resolved("sh-less host", None)
    }));
    let Err(payload) = outcome else {
        panic!(
            "ts_pin_sh skipped instead of hard-failing on a sh-less host — the \
             silent degradation #3259 item 3 closes is still open"
        );
    };
    let msg = payload
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .unwrap_or_default()
        });
    assert!(
        msg.contains("#3259"),
        "the panic must name the decision it encodes, so a reader of a red log \
         can tell a broken host from a broken pin: {msg:?}"
    );
}

/// #815: the launcher block is a refusal, not a gate — the properties worth
/// pinning are the ones whose absence would quietly turn it back into a launch.
#[test]
fn loomux_shim_refuses_outright_with_no_path_that_runs_the_launcher() {
    let sh = loomux_shim_sh();
    assert!(sh.starts_with("#!/bin/sh"));
    assert!(sh.trim_end().ends_with("exit 1"), "the only way out is a refusal");
    assert!(sh.contains("self-launch-blocked"), "audits the refusal");
    assert!(!sh.contains("\r"), "the POSIX loomux shim must be LF-only");
    // No delegation and no grant path: a real launcher is never resolved, so there
    // is nothing to exec, and no marker/grant can authorize one.
    assert!(!sh.contains("exec "), "nothing is ever exec'd — that would be the launch");
    assert!(!sh.contains("grant") && !sh.contains("dangerous_mode") && !sh.contains("autonomous"),
        "no marker or grant may unlock the launcher; there is no authorized agent use");

    let cmd = loomux_shim_cmd();
    assert!(cmd.trim_end().ends_with("exit /b 1"), "the .cmd refuses too");
    assert!(cmd.contains("self-launch-blocked"), "the .cmd audits the refusal");
    // Deliberately unlike gh.cmd/git.cmd: no sh delegation, and therefore no
    // `:orrerix_no_sh` fallback that would run the real launcher when sh is absent.
    assert!(!cmd.contains("ORRERIX_SH") && !cmd.contains("orrerix_no_sh"),
        "a refusal must not have a degraded path that falls through to the launcher");
    // cmd.exe re-parses an echo's text after expanding it, so the message itself
    // (past the `>&2 echo ` redirect, which is meant to be parsed) must carry no
    // metacharacter — one stray `(` and the refusal dies as a syntax error instead
    // of printing, leaving an agent with a bare non-zero exit and no explanation.
    let msg = cmd.lines().find_map(|l| l.strip_prefix(">&2 echo ")).expect("an echoed refusal");
    assert!(!msg.contains(['(', ')', '<', '>', '|', '&', '^', '%', '"']),
        "the echoed message must carry no cmd.exe metacharacter: {msg}");
}

/// #845: the refusal's text is a *claim about what the launcher does*, and an
/// agent that hits the shim reads it as the authoritative description. The
/// pre-#845 wording said the launcher "reinstalls the desktop app whenever its
/// version differs from its own" — the autoupdate behaviour #845 removed. The
/// structural test above and the harness below both stay green on either
/// wording, so nothing else here notices when the message goes stale.
#[test]
fn loomux_shim_messages_describe_the_post_845_command_surface() {
    for (name, text) in [("sh", loomux_shim_sh()), ("cmd", loomux_shim_cmd())] {
        // The one command that can still reinstall is named, so the human the
        // agent escalates to knows what they are being asked to run. The EMITTED
        // spelling only (#1153 phase 5): this is text we write, not text we read,
        // and telling an agent to escalate `loomux update` names a command the
        // human may no longer have.
        assert!(text.contains("orrerix update reinstalls it"),
            "the {name} refusal must name `orrerix update` as the reinstall path: {text}");
        // ...and plain `orrerix` is described as installing only a MISSING app.
        assert!(text.contains("when it is missing"),
            "the {name} refusal must say plain orrerix only installs a missing app: {text}");
        // The pre-rename launcher is blocked by the same shim, and the message
        // has to say so: an agent that reaches this refusal after typing `loomux`
        // otherwise reads a message about a command it did not run and concludes
        // the block was for something else.
        assert!(text.contains("loomux"),
            "the {name} refusal must still name the pre-rename launcher it also blocks: {text}");
        // The removed autoupdate claim must not survive in either shim.
        assert!(!text.contains("version differs"),
            "the {name} refusal still carries the pre-#845 autoupdate claim: {text}");
    }
}

#[test]
fn loomux_shim_harness_blocks_the_launcher_and_audits_it() {
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP loomux_shim_harness…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let (root, group) = (td.path(), td.path().join("group"));
    std::fs::create_dir_all(&group).unwrap();
    let shim = root.join("loomux");
    std::fs::write(&shim, loomux_shim_sh()).unwrap();

    // Every shape an agent might reach for — bare, a flag, the launcher's
    // `update` command, or the pre-#845 `--reinstall` flag — refuses
    // identically. There is no allowed argv.
    for argv in [vec![], vec!["--version"], vec!["--reinstall"], vec!["update"]] {
        let out = Command::new("sh").arg(&shim).args(&argv)
            .env("LOOMUX_GROUP_DIR", &group).env("LOOMUX_AGENT_ID", "w-1")
            .output().unwrap();
        assert!(!out.status.success(), "`loomux {argv:?}` must refuse");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("blocked") && err.contains("MCP"),
            "the refusal must say what to do instead: {err}");
    }
    // One audit line per attempt, naming the agent that tried.
    let audit = std::fs::read_to_string(group.join("audit.jsonl")).unwrap();
    assert_eq!(audit.lines().filter(|l| l.contains("self-launch-blocked")).count(), 4);
    assert!(audit.contains("\"agent\":\"w-1\""), "the audit names the agent: {audit}");
    for line in audit.lines() {
        let v: serde_json::Value =
            serde_json::from_str(line).expect("each audit line is valid JSON");
        // ts_ms must be a NUMBER, not merely present: `date +%s%3N` yields a
        // literal `3N` tail on BSD date (macOS), which silently corrupts the whole
        // line. Parseability alone wouldn't catch a quoted or truncated stamp.
        assert!(v["ts_ms"].is_number(), "ts_ms must be numeric: {line}");
    }
}

#[test]
fn grant_merge_writes_a_consumable_grant_file_and_audits() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // A PR URL is normalized to the number; the grant is keyed pr-<N>.
    let num = reg.grant_merge(&g.id, "https://github.com/o/r/pull/42", Some("bump the changelog first"), "human").unwrap();
    assert_eq!(num, 42);
    let grant = reg.state_root().join(g.id.as_str()).join("merge_grants").join("pr-42");
    assert!(grant.is_file(), "the grant file must exist for the shim to consult");
    // Line 1 is a future unix-seconds expiry.
    let body = std::fs::read_to_string(&grant).unwrap();
    let exp: u64 = body.lines().next().unwrap().parse().unwrap();
    assert!(exp > now_ms() / 1000, "grant expiry must be in the future");
    assert_eq!(audit_count(&reg, &g.id, "merge-grant-written"), 1);
    // A bad PR ref is rejected, no grant written.
    assert!(reg.grant_merge(&g.id, "not-a-pr", None, "human").is_err());
}

#[test]
fn approve_task_writes_a_merge_grant_for_the_prs_number() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Ship it"), None, None)).unwrap();
    let mut p = patch(None, Some("pr"), None);
    p.pr = Some("#7".into());
    reg.upsert_task(&g.id, "orch-1", Some(&t.id), p).unwrap();
    // Clicking Approve (with an optional comment) must mint the one-time grant for
    // that PR — otherwise the enforced gate leaves the orchestrator unable to merge.
    reg.approve_task(&g.id, &t.id, Some("also tag the release note")).unwrap();
    assert!(reg.state_root().join(g.id.as_str()).join("merge_grants").join("pr-7").is_file(),
        "Approve must write the merge grant for the task's PR");
    assert_eq!(audit_count(&reg, &g.id, "merge-grant-written"), 1);
}

// --- bulk board approvals (#507) ---------------------------------------------------------------

/// A merge-gate task with the given title and optional PR ref, ready to approve.
fn gate_task(reg: &OrchRegistry, group: &GroupId, title: &str, pr: Option<&str>) -> Task {
    let t = reg.upsert_task(group, "orch-1", None, patch(Some(title), None, None)).unwrap();
    let mut p = patch(None, Some("pr"), None);
    p.pr = pr.map(String::from);
    reg.upsert_task(group, "orch-1", Some(&t.id), p).unwrap()
}

/// The grant file's nonce (line 2) — distinct per grant, so two grants written
/// by one bulk approval are provably two grants, not one shared authorization.
fn grant_nonce(reg: &OrchRegistry, group: &GroupId, pr: u64) -> String {
    let body = fs::read_to_string(
        reg.state_root().join(group.as_str()).join("merge_grants").join(format!("pr-{pr}")),
    )
    .unwrap();
    body.lines().nth(1).unwrap().to_string()
}

#[test]
fn bulk_approve_mints_a_grant_per_pr_and_delivers_one_consolidated_notice() {
    // The whole point of #507: approving N items at once must keep issuing N
    // ordinary per-PR grants (authority unchanged) while costing the
    // orchestrator ONE prompt instead of N. Both halves are asserted here,
    // because either alone is the bug: N notices is the spam this fixes, and
    // one grant covering N PRs would be a bulk authority nobody granted.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let a = gate_task(&reg, &g.id, "Ship the parser", Some("#7"));
    let b = gate_task(&reg, &g.id, "Ship the writer", Some("https://github.com/o/r/pull/9"));
    let c = gate_task(&reg, &g.id, "Docs pass", None);
    pause_with_pane(&reg, &g.id, &orch.id, 7001);

    let approved = reg
        .approve_tasks(
            &g.id,
            &[
                ApproveItem { id: a.id.clone(), comment: Some("  squash it  ".into()) },
                ApproveItem { id: b.id.clone(), comment: Some("   ".into()) },
                ApproveItem { id: c.id.clone(), comment: Some("close the issue too".into()) },
            ],
        )
        .unwrap();

    // Authority: one real, separately-nonced grant per PR — and none for the
    // item that had no PR to key one on.
    let grants = reg.state_root().join(g.id.as_str()).join("merge_grants");
    assert!(grants.join("pr-7").is_file() && grants.join("pr-9").is_file(),
        "each selected PR must get its own grant file (a PR URL normalized to its number)");
    assert_eq!(audit_count(&reg, &g.id, "merge-grant-written"), 2,
        "one grant minted per PR — no bulk grant object, and nothing minted for the PR-less item");
    assert_ne!(grant_nonce(&reg, &g.id, 7), grant_nonce(&reg, &g.id, 9),
        "two grants, not one authorization shared across PRs");
    assert_eq!(fs::read_dir(&grants).unwrap().count(), 2, "no grant beyond the two selected PRs");
    for t in &approved {
        assert_eq!(t.status, "done", "every approved item is the human's sign-off");
    }

    // Delivery: exactly one prompt, naming every granted PR, carrying the
    // per-task notes (trimmed; a whitespace-only note is no note), and calling
    // out the approved item that got no grant rather than folding it in.
    let sent = delivered_texts(&reg, &g.id);
    assert_eq!(sent.len(), 1, "a bulk approval is ONE prompt, not one per task: {sent:?}");
    assert_eq!(
        sent[0],
        format!(
            "[orrerix] the human GRANTED one-time merges of PRs #7, #9 (valid ~30 min each). \
             You may now merge EACH of THOSE PRs once (only #7, #9), one grant per PR; report when done.\n\
             Also APPROVED at the merge gate, with no PR number to grant — merge and close out by hand: {} \"Docs pass\".\n\
             Note from the human on #7: squash it\n\
             Note from the human on {}: close the issue too",
            c.id, c.id
        )
    );
}

#[test]
fn a_bulk_approval_of_one_delivers_exactly_what_a_single_approve_delivers() {
    // #507 changes delivery, not authority — and the proof a human can check
    // is that the wording did not move under the single-Approve path they
    // already know. Same task shape, same note, through both entry points:
    // byte-identical text, and identical to the pinned #83 sentence, so a
    // future edit to the shared builder cannot drift both together silently.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let single = gate_task(&reg, &g.id, "Ship it", Some("#7"));
    let bulk = gate_task(&reg, &g.id, "Ship it", Some("#7"));
    pause_with_pane(&reg, &g.id, &orch.id, 7002);

    reg.approve_task(&g.id, &single.id, Some("bump the changelog first")).unwrap();
    reg.approve_tasks(
        &g.id,
        &[ApproveItem { id: bulk.id.clone(), comment: Some("bump the changelog first".into()) }],
    )
    .unwrap();

    let sent = delivered_texts(&reg, &g.id);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1], "a bulk of one must deliver the single-Approve text verbatim");
    assert_eq!(
        sent[0],
        "[orrerix] the human GRANTED a one-time merge of PR #7 (valid ~30 min). \
         Note from the human: bump the changelog first\n\
         You may now merge THAT PR once (only #7); report when done."
    );

    // And with no note, the other half of the single-Approve wording.
    let plain = gate_task(&reg, &g.id, "Ship it", Some("#8"));
    reg.approve_tasks(&g.id, &[ApproveItem { id: plain.id, comment: None }]).unwrap();
    assert_eq!(
        delivered_texts(&reg, &g.id)[2],
        "[orrerix] the human GRANTED a one-time merge of PR #8 (valid ~30 min). \
         You may now merge THAT PR once (only #8); report when done."
    );
}

#[test]
fn a_bulk_approval_is_all_or_nothing_and_mints_nothing_when_it_refuses() {
    // The board can shift under a human's selection between render and click.
    // For a delete that is harmless (the row is already gone); for an approval
    // it is authority, so the batch refuses as a whole rather than issuing
    // some of the grants the human asked for and silently dropping the rest.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let at_gate = gate_task(&reg, &g.id, "Ship it", Some("#7"));
    let queued = reg.upsert_task(&g.id, "orch-1", None, patch(Some("Not ready"), Some("queued"), None)).unwrap();
    pause_with_pane(&reg, &g.id, &orch.id, 7003);

    let err = reg
        .approve_tasks(
            &g.id,
            &[
                ApproveItem { id: at_gate.id.clone(), comment: None },
                ApproveItem { id: queued.id.clone(), comment: None },
            ],
        )
        .unwrap_err();
    assert!(err.contains("merge gate"), "the refusal must name why: {err}");
    assert!(!reg.state_root().join(g.id.as_str()).join("merge_grants").join("pr-7").exists(),
        "a refused batch mints NO grant, not the ones that happened to be valid");
    assert_eq!(audit_count(&reg, &g.id, "merge-grant-written"), 0);
    assert!(delivered_texts(&reg, &g.id).is_empty(), "a refused batch tells the orchestrator nothing");
    let statuses: Vec<String> = reg.tasks(&g.id).into_iter().map(|t| t.status).collect();
    assert!(!statuses.contains(&"done".to_string()), "no item is signed off by a refused batch");

    // An unknown id refuses the same way, and a repeated one is refused rather
    // than minting the same PR's grant (and listing it) twice.
    assert!(reg.approve_tasks(&g.id, &[ApproveItem { id: "t-nope".into(), comment: None }]).is_err());
    let twice = reg
        .approve_tasks(
            &g.id,
            &[
                ApproveItem { id: at_gate.id.clone(), comment: None },
                ApproveItem { id: at_gate.id.clone(), comment: None },
            ],
        )
        .unwrap_err();
    assert!(twice.contains("twice"), "a repeated id is refused, not double-granted: {twice}");
    assert_eq!(audit_count(&reg, &g.id, "merge-grant-written"), 0);

    // An empty selection is a no-op: nothing written, nothing said.
    assert!(reg.approve_tasks(&g.id, &[]).unwrap().is_empty());
    assert!(delivered_texts(&reg, &g.id).is_empty());
}

#[test]
fn a_bulk_approval_with_no_prs_at_all_says_so_instead_of_naming_grants() {
    // Approving PR-less items in bulk must not produce a grant sentence with an
    // empty PR list — the orchestrator has to hear "nothing was authorized".
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let a = gate_task(&reg, &g.id, "Docs pass", None);
    // Carrying a `pr` field that yields no NUMBER is the same case as carrying
    // none, and is why the notice says "no PR number could be resolved" rather
    // than "no PR is linked" — the latter would be false here and would send
    // the orchestrator looking for the wrong problem.
    let b = gate_task(&reg, &g.id, "Spike", Some("TBD"));
    pause_with_pane(&reg, &g.id, &orch.id, 7004);

    reg.approve_tasks(
        &g.id,
        &[
            ApproveItem { id: a.id.clone(), comment: None },
            ApproveItem { id: b.id.clone(), comment: None },
        ],
    )
    .unwrap();

    let sent = delivered_texts(&reg, &g.id);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0],
        format!(
            "[orrerix] the human APPROVED {} \"Docs pass\", {} \"Spike\" at the merge gate and marked \
             them done. No PR number could be resolved for them, so nothing was authorized — \
             merge and close out by hand.",
            a.id, b.id
        )
    );
    assert!(!reg.state_root().join(g.id.as_str()).join("merge_grants").exists(),
        "nothing to grant means no grant directory at all");
    assert_eq!(audit_count(&reg, &g.id, "task-approve-bulk"), 1);
}

#[test]
fn a_failed_grant_write_fails_the_call_instead_of_reclassifying_the_item() {
    // #507 review B1. `mint_merge_grant` fails for two very different reasons —
    // the ref carries no PR number (the intended plain-approval path) and the
    // grant file could not be WRITTEN (a full disk; this repo has lost a board
    // to one). Collapsing both with `.ok()` made a write failure announce "no
    // PR number could be resolved", which is false: a number WAS resolved, and
    // the orchestrator would be told to close out by hand a PR whose merge the
    // shim then refuses. The classification must come from the ref alone.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let a = gate_task(&reg, &g.id, "Ship the parser", Some("#7"));
    let b = gate_task(&reg, &g.id, "Ship the writer", Some("#9"));
    // Injected I/O failure, portable and deterministic: a regular FILE where the
    // grant DIRECTORY has to go, so `atomic_write`'s `create_dir_all` fails.
    fs::write(reg.state_root().join(g.id.as_str()).join("merge_grants"), b"not a directory").unwrap();
    pause_with_pane(&reg, &g.id, &orch.id, 7005);

    let err = reg
        .approve_tasks(&g.id, &[ApproveItem { id: a.id.clone(), comment: None }])
        .unwrap_err();
    assert!(
        !err.contains("no PR") && !err.contains("merge gate"),
        "the failure must surface as the write error it is, not a reclassification: {err}"
    );
    assert!(
        delivered_texts(&reg, &g.id).is_empty(),
        "a failed grant write announces NOTHING — an unannounced grant expires unused, but a \
         confident notice misdescribing what was authorized does not un-say itself"
    );
    assert_eq!(audit_count(&reg, &g.id, "merge-grant-written"), 0);

    // The single-Approve path had the same swallow and gets the same treatment.
    assert!(reg.approve_task(&g.id, &b.id, None).is_err());
    assert!(
        delivered_texts(&reg, &g.id).is_empty(),
        "a single Approve whose grant write failed must not fall through to the plain-approval \
         notice, which claims the item simply had no PR"
    );
}

#[test]
fn two_selected_tasks_naming_the_same_pr_are_refused_not_double_granted() {
    // #507 review N1. Dedup by task id alone let two board rows for the SAME PR
    // (a duplicate filing) through: both mint `pr-7`, the second overwriting the
    // first, and the notice then reads "#7, #7 … one grant per PR" — two grants
    // claimed, one file on disk. No authority is widened, but the notice lies
    // about what it authorized, so refuse it like a repeated id.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    // Two different REFS, one number — proving the check is on the resolved PR
    // number, not on the string the human happened to paste.
    let a = gate_task(&reg, &g.id, "Ship it", Some("#7"));
    let b = gate_task(&reg, &g.id, "Ship it (dup filing)", Some("https://github.com/o/r/pull/7"));
    pause_with_pane(&reg, &g.id, &orch.id, 7006);

    let err = reg
        .approve_tasks(
            &g.id,
            &[
                ApproveItem { id: a.id.clone(), comment: None },
                ApproveItem { id: b.id.clone(), comment: None },
            ],
        )
        .unwrap_err();
    assert!(err.contains("PR #7 appears twice"), "the refusal must name the PR: {err}");
    assert!(!reg.state_root().join(g.id.as_str()).join("merge_grants").exists(),
        "refused in pre-flight — nothing minted");
    assert!(delivered_texts(&reg, &g.id).is_empty());
    assert!(
        reg.tasks(&g.id).into_iter().all(|t| t.status != "done"),
        "and no item is signed off by a refused batch"
    );
}

#[test]
fn grants_are_not_writable_by_any_mcp_tool() {
    // SECURITY BOUNDARY: grants are human-only (Tauri commands). No MCP tool an
    // agent can call may write under the group dir at a grant path.
    let (reg, _d, co, cw) = setup_mcp();
    let tool_names = |c: &Caller| -> Vec<String> {
        dispatch(&reg, c, "tools/list", &Value::Null).unwrap()["tools"].as_array().unwrap()
            .iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    };
    for c in [&co, &cw] {
        for n in tool_names(c) {
            assert!(!n.contains("grant"), "no MCP tool may write grants, found: {n}");
            // Supervised dangerous mode (#83) is likewise Tauri-only — no MCP surface.
            assert!(!n.contains("dangerous"), "no MCP tool may enable dangerous mode, found: {n}");
        }
    }
    // Exercise the file-writing MCP tools an agent CAN call; none may create a
    // grant dir/file or the dangerous_mode marker under the group.
    let _ = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "set_state", "arguments": { "state": "{\"x\":1}" } }));
    let _ = dispatch(&reg, &co, "tools/call",
        &json!({ "name": "upsert_task", "arguments": { "title": "t", "status": "pr" } }));
    let gdir = reg.state_root().join(co.group.as_str());
    assert!(!gdir.join("merge_grants").exists(), "no MCP tool may create merge_grants");
    assert!(!gdir.join("release_grants").exists(), "no MCP tool may create release_grants");
    assert!(!gdir.join("dangerous_mode").exists(), "no MCP tool may create the dangerous_mode marker");
}

/// The shim paths the PRODUCT bakes in (#509), resolved the same way
/// `ensure_shims` does. Every harness test builds its shim with these so it
/// pins what actually ships — and so the shim's own dependency self-check finds
/// the coreutils on a runner whose PATH carries `Git\bin` but not `Git\usr\bin`.
pub(crate) fn shim_paths() -> ShimPaths {
    resolve_shim_toolchain().1
}

/// Fake gh recording args and answering pr/repo view from env; anything else
/// "succeeds". Returns its path. Shared by the harness tests.
pub(crate) fn write_fake_gh(root: &std::path::Path, log: &std::path::Path) -> std::path::PathBuf {
    let p = root.join("fakegh");
    // The `api … --jq .tag_name` arm answers the shim's #437 release-id → tag
    // lookup from `$FAKE_REL_MAP` ("<id>=<tag> <id>=<tag> …"). An id that is not
    // in the map exits NON-ZERO with no output, which is what a 404 / no-such-
    // release / offline gh looks like to the shim — i.e. the fail-closed input.
    // Unset FAKE_REL_MAP therefore means "no release id resolves", which is the
    // pre-#437 world and keeps every existing expectation in this file honest.
    std::fs::write(&p, format!(
        "#!/bin/sh\n\
         echo \"ARGS: $*\" >> \"{log}\"\n\
         if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then printf '%s %s\\n' \"$FAKE_BASE\" \"$FAKE_NUM\"; exit 0; fi\n\
         if [ \"$1\" = \"repo\" ] && [ \"$2\" = \"view\" ]; then printf '%s\\n' \"$FAKE_DEFAULT\"; exit 0; fi\n\
         if [ \"$1\" = \"api\" ]; then\n\
           case \"$*\" in\n\
             *--jq*.tag_name*)\n\
               _p=\"\"\n\
               for a in \"$@\"; do case \"$a\" in *releases/*) _p=$a ;; esac; done\n\
               _id=${{_p##*releases/}}\n\
               for e in $FAKE_REL_MAP; do\n\
                 case \"$e\" in \"$_id=\"*) printf '%s\\n' \"${{e#*=}}\"; exit 0 ;; esac\n\
               done\n\
               exit 1 ;;\n\
           esac\n\
         fi\n\
         printf 'FAKE-GH-RAN\\n'; exit 0\n",
        log = log.display()
    )).unwrap();
    p
}

#[test]
fn gh_shim_harness_grant_authorizes_one_merge_and_releases_are_gated() {
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP gh_shim_harness_grant…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();

    let run = |argv: &[&str], num: &str| -> bool {
        Command::new("sh").arg(&shim).args(argv)
            .env("LOOMUX_GROUP_DIR", &group)
            .env("FAKE_BASE", "main").env("FAKE_DEFAULT", "main").env("FAKE_NUM", num)
            .status().unwrap().success()
    };
    let write_grant = |dir: &str, name: &str| {
        let d = group.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        // far-future expiry (unix seconds)
        std::fs::write(d.join(name), b"99999999999\n1\n").unwrap();
    };

    // No grant, no markers → blocked.
    assert!(!run(&["pr", "merge", "5"], "5"), "no grant → blocked");
    // A grant for pr-5 authorizes exactly one merge, then is consumed.
    write_grant("merge_grants", "pr-5");
    assert!(run(&["pr", "merge", "5"], "5"), "valid grant → allowed");
    assert!(!group.join("merge_grants/pr-5").exists(), "grant must be consumed");
    assert!(!run(&["pr", "merge", "5"], "5"), "consumed grant → second merge blocked");
    // A grant for pr-5 must NOT authorize merging pr-7.
    write_grant("merge_grants", "pr-5");
    assert!(!run(&["pr", "merge", "7"], "7"), "a pr-5 grant cannot merge pr-7");
    // An expired grant does not authorize (and is cleaned up).
    std::fs::create_dir_all(group.join("merge_grants")).unwrap();
    std::fs::write(group.join("merge_grants/pr-9"), b"1\n1\n").unwrap();
    assert!(!run(&["pr", "merge", "9"], "9"), "expired grant → blocked");

    // Releases: blocked without a grant even though markers would allow a MERGE.
    std::fs::write(group.join("autonomous"), b"").unwrap();
    std::fs::write(group.join("auto_merge"), b"").unwrap();
    assert!(!run(&["release", "create", "v1.2.3"], "0"), "release blocked even in autonomous+auto_merge");
    write_grant("release_grants", "v1.2.3");
    assert!(run(&["release", "create", "v1.2.3"], "0"), "release grant → allowed");
    // #438: NOT consumed — a release grant covers its tag's whole pipeline for
    // its TTL. (Merge grants above are still one-time; that asymmetry is the
    // point, since a merge is one action and a release is several.)
    assert!(group.join("release_grants/v1.2.3").exists(), "release grant stays live for the rest of the pipeline");
    // Read-only release subcommand passes through.
    assert!(run(&["release", "view", "v1.2.3"], "0"), "release view is not gated");
    // rev-86 LOW: value-flags BEFORE the tag must not misparse it — a granted
    // release with --title still resolves tag v1.2.3 and is allowed.
    assert!(run(&["release", "create", "--title", "My Release", "v1.2.3"], "0"),
        "granted release with --title before the tag must be allowed, not misparsed");
    assert!(!run(&["release", "create", "--title", "My Release", "v9.9.9"], "0"),
        "a release with --title and no grant is still blocked (tag parsed correctly)");
    // auto_release opt-in: autonomous + auto_release blanket-allows any release —
    // and does NOT consume a grant (no per-tag file needed). auto_merge alone did
    // NOT (asserted above), proving the two toggles are independent.
    std::fs::write(group.join("auto_release"), b"").unwrap();
    assert!(run(&["release", "create", "v3.0.0"], "0"), "autonomous+auto_release blanket-allows a release");
    assert!(run(&["release", "create", "v3.0.1"], "0"), "blanket auto_release is repeatable (not a one-time grant)");

    // Supervised dangerous mode (#83): the human is present, NOT autonomous. Clear
    // the autonomous markers, set dangerous_mode → both a default-branch MERGE and a
    // RELEASE are allowed (no grant), each with its DISTINCT audit marker.
    for m in ["autonomous", "auto_merge", "auto_release"] { let _ = std::fs::remove_file(group.join(m)); }
    std::fs::write(group.join("dangerous_mode"), b"").unwrap();
    std::fs::write(group.join("audit.jsonl"), b"").unwrap();
    assert!(run(&["pr", "merge", "5"], "5"), "dangerous mode → default-branch merge allowed");
    assert!(run(&["release", "create", "v4.0.0"], "0"), "dangerous mode → release allowed");
    let audit = std::fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    assert!(audit.contains("merge-gate-dangerous"), "distinct merge audit marker, got: {audit}");
    assert!(audit.contains("release-gate-dangerous"), "distinct release audit marker");
    // dangerous is a NO-OP while autonomous (mutually exclusive; defensive guard):
    // with autonomous back on but no auto_merge/auto_release/grant, both are blocked.
    // Use PR 8 (no leftover grant) so only the dangerous path could have allowed it.
    std::fs::write(group.join("autonomous"), b"0").unwrap();
    assert!(!run(&["pr", "merge", "8"], "8"), "dangerous is ignored while autonomous → merge blocked");
    assert!(!run(&["release", "create", "v4.0.1"], "0"), "dangerous is ignored while autonomous → release blocked");
}

/// A fake `gh` that mirrors the REAL `gh`'s split behavior the #294 bug hinged on:
/// `pr view` accepts `-R`, but `repo view` rejects it exactly like the real CLI
/// ("unknown shorthand flag: 'R' in -R") — any `-R`/`--repo` token reaching `repo
/// view` here fails, so this only passes if the shim stops forwarding `$rf` to it.
fn write_fake_gh_rejecting_dash_r_on_repo_view(root: &std::path::Path, log: &std::path::Path) -> std::path::PathBuf {
    let p = root.join("fakegh_norepo_r");
    std::fs::write(&p, format!(
        "#!/bin/sh\n\
         echo \"ARGS: $*\" >> \"{log}\"\n\
         if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then printf '%s %s\\n' \"$FAKE_BASE\" \"$FAKE_NUM\"; exit 0; fi\n\
         if [ \"$1\" = \"repo\" ] && [ \"$2\" = \"view\" ]; then\n\
         \x20 shift 2\n\
         \x20 for a in \"$@\"; do case \"$a\" in -R|--repo|-R?*|--repo=*) printf 'unknown shorthand flag: '\\''R'\\'' in -R\\n' >&2; exit 1 ;; esac; done\n\
         \x20 printf '%s\\n' \"$FAKE_DEFAULT\"; exit 0\n\
         fi\n\
         printf 'MERGED\\n'; exit 0\n",
        log = log.display()
    )).unwrap();
    p
}

#[test]
fn gh_shim_harness_granted_merge_with_dash_r_repo_is_allowed_not_blocked_as_unverifiable_base() {
    // #294 live incident: a granted `gh pr merge N -R owner/repo` blocked as
    // "unverifiable-base" because the shim forwarded -R to `gh repo view`, which
    // rejects it. Proven against the REAL generated shim + a fake gh that rejects
    // -R on `repo view` exactly like the real CLI (see helper above) — this test
    // fails on the pre-fix shim (default-branch lookup comes back empty → block)
    // and passes once `repo view` is called with the repo positionally.
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP gh_shim_harness_granted_merge_with_dash_r…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh_rejecting_dash_r_on_repo_view(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();

    let run = |argv: &[&str], num: &str| -> (bool, String) {
        let out = Command::new("sh").arg(&shim).args(argv)
            .env("LOOMUX_GROUP_DIR", &group)
            .env("FAKE_BASE", "main").env("FAKE_DEFAULT", "main").env("FAKE_NUM", num)
            .output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let write_grant = |name: &str| {
        let d = group.join("merge_grants");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(name), b"99999999999\n1\n").unwrap();
    };

    // A granted -R merge for an UNRELATED pr (no grant for pr-12) still blocks —
    // and a different PR's grant is untouched by that blocked attempt (the other
    // saving grace #294 calls out: a block never consumes a grant it didn't use).
    write_grant("pr-11");
    let (ok, err) = run(&["pr", "merge", "12", "-R", "owner/repo"], "12");
    assert!(!ok, "no grant for pr-12 → still blocked even with -R");
    assert!(err.contains("human gate"), "refusal message, got: {err}");
    assert!(group.join("merge_grants/pr-11").exists(), "an unrelated blocked -R attempt must not consume pr-11's grant");

    // The granted -R merge is now allowed — this is the line the #294 bug broke.
    let (ok, err) = run(&["pr", "merge", "11", "-R", "owner/repo"], "11");
    assert!(ok, "granted -R merge must be allowed, got stderr: {err}");
    assert!(!group.join("merge_grants/pr-11").exists(), "the used grant is consumed");

    // repo view was in fact invoked WITHOUT -R (proving the fix, not just luck).
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(logged.lines().any(|l| l.contains("repo view") && !l.contains("-R") && !l.contains("--repo")),
        "repo view must be called without -R/--repo, log: {logged}");
}

/// A fake `gh` whose `pr merge` exits `$FAKE_MERGE_EXIT` (default 0/success) —
/// lets a test simulate GitHub refusing the merge (draft PR, branch
/// protection, …) while `pr view`/`repo view` resolve normally.
fn write_fake_gh_with_merge_exit(root: &std::path::Path, log: &std::path::Path) -> std::path::PathBuf {
    let p = root.join("fakegh_merge_exit");
    std::fs::write(&p, format!(
        "#!/bin/sh\n\
         echo \"ARGS: $*\" >> \"{log}\"\n\
         if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then printf '%s %s\\n' \"$FAKE_BASE\" \"$FAKE_NUM\"; exit 0; fi\n\
         if [ \"$1\" = \"repo\" ] && [ \"$2\" = \"view\" ]; then printf '%s\\n' \"$FAKE_DEFAULT\"; exit 0; fi\n\
         if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"merge\" ]; then exit \"${{FAKE_MERGE_EXIT:-0}}\"; fi\n\
         printf 'MERGED\\n'; exit 0\n",
        log = log.display()
    )).unwrap();
    p
}

#[test]
fn gh_shim_harness_a_merge_that_fails_at_github_does_not_burn_the_one_time_grant() {
    // #256 live incident: a granted `gh pr merge` was let through, GitHub
    // refused it (draft PR), and the interceptor had already deleted the
    // grant on interception — the human had to re-Approve. Proven against
    // the REAL generated shim: a merge that exits non-zero must leave the
    // grant usable for a retry; only a merge that exits 0 may consume it.
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP gh_shim_harness_a_merge_that_fails_at_github…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh_with_merge_exit(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();

    let run = |num: &str, merge_exit: &str| -> bool {
        Command::new("sh").arg(&shim).args(["pr", "merge", num])
            .env("LOOMUX_GROUP_DIR", &group)
            .env("FAKE_BASE", "main").env("FAKE_DEFAULT", "main").env("FAKE_NUM", num)
            .env("FAKE_MERGE_EXIT", merge_exit)
            .status().unwrap().success()
    };
    let write_grant = |name: &str| {
        let d = group.join("merge_grants");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(name), b"99999999999\n1\n").unwrap();
    };
    let grant_path = |name: &str| group.join("merge_grants").join(name);

    // A merge GitHub refuses (draft PR, say) must NOT consume the grant.
    write_grant("pr-5");
    assert!(!run("5", "1"), "a failed merge must fail (surface GitHub's refusal)");
    assert!(grant_path("pr-5").exists(), "a failed merge must NOT consume the grant — this is the #256 bug");
    assert!(!grant_path("pr-5.claimed").exists(), "no orphaned .claimed file after a resolved failure");

    // The SAME grant authorizes the retry once the PR is fixed (e.g. `gh pr
    // ready`) and the merge actually succeeds — then it IS consumed.
    assert!(run("5", "0"), "retry with the still-usable grant must succeed");
    assert!(!grant_path("pr-5").exists(), "a successful merge consumes the grant");
    assert!(!grant_path("pr-5.claimed").exists(), "no orphaned .claimed file after a resolved success");

    // The now-consumed grant cannot authorize a second merge.
    assert!(!run("5", "0"), "a consumed grant must not authorize another merge");

    // Expired grants are still cleaned up (never claimed, never left behind).
    std::fs::write(group.join("merge_grants/pr-9"), b"1\n1\n").unwrap();
    assert!(!run("9", "0"), "expired grant → blocked");
    assert!(!grant_path("pr-9").exists(), "expired grant is cleaned up, not left claimable");

    // Crash-between semantics: a `.claimed` file with no matching grant (as
    // if the process died between claim and resolve) must NOT be treated as
    // a usable grant by a later attempt — the bare grant file is gone, so
    // the next merge sees "no grant" and fails closed, requiring a fresh one.
    std::fs::create_dir_all(group.join("merge_grants")).unwrap();
    std::fs::write(group.join("merge_grants/pr-13.claimed"), b"99999999999\n1\n").unwrap();
    assert!(!run("13", "0"), "an orphaned .claimed file with no live grant must not authorize a merge");
}

/// A fake `gh` whose `release` subcommand exits `$FAKE_RELEASE_EXIT` (default
/// 0/success) — lets a test simulate GitHub refusing a release publish (tag
/// already exists, a transient API error, …).
fn write_fake_gh_with_release_exit(root: &std::path::Path, log: &std::path::Path) -> std::path::PathBuf {
    let p = root.join("fakegh_release_exit");
    std::fs::write(&p, format!(
        "#!/bin/sh\n\
         echo \"ARGS: $*\" >> \"{log}\"\n\
         if [ \"$1\" = \"release\" ]; then exit \"${{FAKE_RELEASE_EXIT:-0}}\"; fi\n\
         printf 'PUBLISHED\\n'; exit 0\n",
        log = log.display()
    )).unwrap();
    p
}

#[test]
fn gh_shim_harness_a_release_publish_that_fails_at_github_does_not_burn_the_grant() {
    // #303 (same bug class as #256): a granted `gh release create` was let
    // through, GitHub refused it (tag already exists, a transient API error,
    // …), and the interceptor had already deleted the release grant on
    // interception — the human would have had to re-grant. #438 makes that
    // property structural rather than settle-dependent: a release grant is a
    // pipeline grant, checked and never spent, so NO outcome burns it. This
    // still executes the REAL generated shim, and now pins BOTH halves — a
    // refused publish is retryable (#303) AND a successful one leaves the
    // grant live for the next step of the same release (#438).
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP gh_shim_harness_a_release_publish_that_fails…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh_with_release_exit(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();

    let run = |tag: &str, release_exit: &str| -> bool {
        Command::new("sh").arg(&shim).args(["release", "create", tag])
            .env("LOOMUX_GROUP_DIR", &group)
            .env("FAKE_RELEASE_EXIT", release_exit)
            .status().unwrap().success()
    };
    let write_grant = |name: &str| {
        let d = group.join("release_grants");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(name), b"99999999999\n1\n").unwrap();
    };
    let grant_path = |name: &str| group.join("release_grants").join(name);

    // A publish GitHub refuses must NOT consume the grant.
    write_grant("v1.2.3");
    assert!(!run("v1.2.3", "1"), "a failed publish must fail (surface GitHub's refusal)");
    assert!(grant_path("v1.2.3").exists(), "a failed publish must NOT consume the grant — this is the #303 bug");
    assert!(!grant_path("v1.2.3.claimed").exists(), "the release path claims nothing, so it can leave no .claimed file");

    // The SAME grant authorizes a retry — and a SUCCESSFUL publish leaves it
    // live too (#438). This assertion is the inverse of what it was before this
    // fix ("a successful publish consumes the grant"), deliberately: the human
    // authorized the release of v1.2.3, and `gh release create` is only the
    // first step of it. Burning the grant here is what forced a second human
    // grant for the notes write that follows.
    assert!(run("v1.2.3", "0"), "retry with the still-usable grant must succeed");
    assert!(grant_path("v1.2.3").exists(), "a successful publish must leave the pipeline grant live for the next step");
    assert!(run("v1.2.3", "0"), "the same grant covers a further step of the same release");

    // …but only until it expires. An expired grant refuses and is cleaned up,
    // which is the ONLY thing now bounding a release grant's lifetime, so it is
    // the assertion that matters most in this file.
    std::fs::write(group.join("release_grants/v1.2.3"), b"1\n1\n").unwrap();
    assert!(!run("v1.2.3", "0"), "expired grant → blocked, even for a tag that was mid-pipeline");
    assert!(!grant_path("v1.2.3").exists(), "expired grant is cleaned up, not left usable");

    // And only for its own tag: a live v1.2.3 grant is not a release grant.
    write_grant("v1.2.3");
    assert!(!run("v9.9.9", "0"), "a v1.2.3 grant must not authorize publishing v9.9.9");
}

#[test]
fn release_grant_covers_one_tags_whole_pipeline_and_resolves_release_ids_fail_closed() {
    // The headline pin for #437 + #438, executed end to end against BOTH real
    // generated shims sharing one group dir — because the incident spanned them:
    // the v1.1.0-beta6 grant was spent by the GIT shim's tag push, and the notes
    // write that the GH shim then refused was part of the same release the human
    // had already authorized. It was refused twice over, in fact: once because
    // the grant was gone, and once because the notes call is addressed by
    // canonical release id (`gh api -X PATCH …/releases/<id>`, which the release
    // skill MANDATES over tag lookup, #282) and the shim could not extract a tag
    // from it at all — the refusal literally read "release/tag ()".
    //
    // Four properties, and the last three are the security envelope that makes
    // the first one safe to want:
    //   (a) ONE grant covers tag push → release create/edit → notes-by-id.
    //   (b) A different release id resolves to ITS OWN tag and is REFUSED.
    //   (c) Expiry still refuses — the TTL is the wall, not the first use.
    //   (d) An id loomux cannot resolve is REFUSED (fail-closed), never assumed
    //       to belong to the granted tag.
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP release_grant_covers_one_tags_whole_pipeline…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");

    let fake_gh = write_fake_gh(root, &log);
    let gh = root.join("gh");
    std::fs::write(&gh, gh_shim_sh(&fake_gh.display().to_string(), &shim_paths())).unwrap();
    // Fake git: `rev-parse` confirms $FAKE_TAG is a real tag; anything else "succeeds".
    let fake_git = root.join("fakegit");
    std::fs::write(&fake_git,
        "#!/bin/sh\n\
         if [ \"$1\" = \"rev-parse\" ]; then\n\
           for a in \"$@\"; do case \"$a\" in refs/tags/*) [ \"$a\" = \"refs/tags/$FAKE_TAG\" ] && exit 0 ;; esac; done\n\
           exit 1\n\
         fi\n\
         printf 'FAKE-GIT-RAN\\n'; exit 0\n").unwrap();
    let git = root.join("git");
    std::fs::write(&git, git_shim_sh(&fake_git.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!(
        "chmod +x '{}' '{}' '{}' '{}'",
        fake_gh.display(), gh.display(), fake_git.display(), git.display())).status();

    // Release 555 is v1.2.3's (the granted one); 777 belongs to a different
    // release entirely; every other id is unknown to gh (exits non-zero).
    const REL_MAP: &str = "555=v1.2.3 777=v0.0.9";
    let run_gh = |argv: &[&str]| -> bool {
        Command::new("sh").arg(&gh).args(argv)
            .env("LOOMUX_GROUP_DIR", &group).env("FAKE_REL_MAP", REL_MAP)
            .status().unwrap().success()
    };
    let run_git = |argv: &[&str]| -> bool {
        Command::new("sh").arg(&git).args(argv)
            .env("LOOMUX_GROUP_DIR", &group).env("FAKE_TAG", "v1.2.3")
            .status().unwrap().success()
    };
    let grant_path = group.join("release_grants").join("v1.2.3");
    let write_grant = |expiry: &str| {
        std::fs::create_dir_all(group.join("release_grants")).unwrap();
        std::fs::write(&grant_path, format!("{expiry}\n1\n").as_bytes()).unwrap();
    };
    let audit = || std::fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    let clear_audit = || { std::fs::write(group.join("audit.jsonl"), b"").unwrap(); };

    // Baseline: with no grant at all, every step of the release is refused. If
    // this ever passes, nothing below is measuring what it claims to.
    let notes: &[&str] = &["api", "-X", "PATCH", "repos/o/r/releases/555", "-F", "body=@notes.md"];
    assert!(!run_git(&["push", "origin", "refs/tags/v1.2.3"]), "no grant → tag push refused");
    assert!(!run_gh(notes), "no grant → notes write refused");

    // ---- (a) ONE grant, the whole pipeline, in the order a real release runs.
    write_grant("99999999999");
    assert!(run_git(&["push", "origin", "refs/tags/v1.2.3"]), "granted tag push must be allowed");
    assert!(grant_path.exists(), "the tag push must NOT spend the grant — the release has more steps");
    assert!(run_gh(&["release", "edit", "v1.2.3", "--notes", "x"]), "same grant covers the release edit");
    clear_audit();
    assert!(run_gh(notes), "same grant covers the release-NOTES write addressed by release id (#437)");
    let a = audit();
    assert!(a.contains("release-id-resolved") && a.contains("v1.2.3"),
        "the id → tag resolution must be in the audit trail, got: {a}");
    assert!(grant_path.exists(), "the grant is still live after the pipeline — only its TTL ends it");

    // ---- (b) A DIFFERENT release is not covered, however it is addressed.
    // 777 resolves to v0.0.9, so the v1.2.3 grant does not match it — this is
    // the `make_latest` flip on somebody else's release, refused.
    assert!(!run_gh(&["api", "-X", "PATCH", "repos/o/r/releases/777", "-f", "make_latest=true"]),
        "a v1.2.3 grant must not authorize writing to another tag's release");
    assert!(!run_gh(&["api", "-X", "DELETE", "repos/o/r/releases/777"]),
        "a v1.2.3 grant must not authorize deleting another tag's release");
    assert!(!run_gh(&["release", "create", "v0.0.9"]), "…nor publishing another tag by name");
    assert!(!run_git(&["push", "origin", "refs/tags/v9.9.9"]), "…nor pushing another tag");

    // A URL that names a release loomux cannot pin down — a traversal, or a
    // sub-resource of one — resolves to nothing and is refused. (What makes
    // that *safe* rather than merely conservative is that the resolving GET
    // uses the write's own path verbatim, so the two can never address
    // different releases; these shapes are refused because identity can't be
    // established, and `an_id_addressed_release_write_never_takes_its_identity_
    // from_a_caller_supplied_tag` is where it matters that the argv tag doesn't
    // get to answer instead.)
    assert!(!run_gh(&["api", "-X", "PATCH", "repos/o/r/releases/555/../777", "-f", "make_latest=true"]),
        "a traversal inside a releases URL is not identifiable → refused");
    assert!(!run_gh(&["api", "-X", "POST", "repos/o/r/releases/555/assets"]),
        "a sub-resource of the granted release resolves to nothing → stays fail-closed");

    // ---- (d) Fail-closed on an id gh cannot resolve. 999 is in no map entry,
    // so the lookup exits non-zero with no output — a 404, a network failure, a
    // gh that isn't authenticated. None of those may be read as "close enough".
    clear_audit();
    assert!(!run_gh(&["api", "-X", "PATCH", "repos/o/r/releases/999", "-F", "body=@notes.md"]),
        "an unresolvable release id must be REFUSED, not assumed to be the granted one");
    let a = audit();
    assert!(a.contains("release-id-unresolved"), "the failed resolution must be audited, got: {a}");
    assert!(a.contains("release-gate-blocked"), "…and the call blocked as a release-gate event, got: {a}");
    assert!(grant_path.exists(), "a refusal must not consume the grant either");

    // ---- (c) Expiry is the wall. Everything above stops the instant it passes.
    write_grant("1");
    assert!(!run_gh(notes), "expired grant → the notes write is refused");
    assert!(!grant_path.exists(), "an expired grant is deleted, not left usable");
    write_grant("1");
    assert!(!run_git(&["push", "origin", "refs/tags/v1.2.3"]), "expired grant → the tag push is refused");
    assert!(!run_gh(&["release", "edit", "v1.2.3", "--notes", "x"]), "expired grant → the release edit is refused");

    // A read-only GET of the same release is not a publish and never was gated —
    // pinned here so the id-resolution work can't quietly start gating reads.
    assert!(run_gh(&["api", "repos/o/r/releases/555"]), "a read-only release GET still passes through");
}

#[test]
fn release_id_addressed_calls_resolve_to_their_tag_before_the_grant_is_checked() {
    // #437 ON ITS OWN. The pipeline test above covers this too, but it reaches
    // it only *after* asserting #438's retained-grant behaviour — so on the
    // pre-fix shim it panics on a #438 line and #437 is never witnessed red at
    // all. This test's FIRST assertion is the #437 one, against a grant no
    // earlier step has touched: it fails on the old shim because an
    // id-addressed release write carries no tag to key a grant on, and for no
    // other reason. Two halves of one PR need two witnesses.
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP release_id_addressed_calls_resolve…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();

    // 555 is the granted tag's release; 777 is somebody else's; nothing else resolves.
    let run = |argv: &[&str]| -> bool {
        Command::new("sh").arg(&shim).args(argv)
            .env("LOOMUX_GROUP_DIR", &group).env("FAKE_REL_MAP", "555=v1.2.3 777=v0.0.9")
            .status().unwrap().success()
    };
    let write_grant = || {
        std::fs::create_dir_all(group.join("release_grants")).unwrap();
        std::fs::write(group.join("release_grants/v1.2.3"), b"99999999999\n1\n").unwrap();
    };

    // THE #437 ASSERTION. `gh api -X PATCH repos/O/R/releases/<id> -F body=@…`
    // is how the release skill mandates notes be applied — by canonical release
    // id, never by tag lookup, because a tag can resolve to more than one
    // release (#282). Under a live grant for the tag that release belongs to,
    // it must be ALLOWED.
    write_grant();
    assert!(run(&["api", "-X", "PATCH", "repos/o/r/releases/555", "-F", "body=@notes.md"]),
        "an id-addressed write to the GRANTED tag's release must resolve and be allowed (#437)");

    // …and the resolution is what authorizes it, not the mere presence of a
    // grant: the same shape against a release belonging to another tag is
    // refused, with the same grant still live.
    assert!(!run(&["api", "-X", "PATCH", "repos/o/r/releases/777", "-F", "body=@notes.md"]),
        "the id must be resolved to ITS tag — another release is not covered");
    // An id gh cannot resolve at all (404 / network / unauthenticated) is
    // refused, never charitably read as the granted one.
    assert!(!run(&["api", "-X", "PATCH", "repos/o/r/releases/424242", "-F", "body=@notes.md"]),
        "an unresolvable release id fails closed");
    // No grant at all → refused even for the resolvable id, so the assertion
    // above is measuring the grant match and not a hole in the gate.
    let _ = std::fs::remove_dir_all(group.join("release_grants"));
    assert!(!run(&["api", "-X", "PATCH", "repos/o/r/releases/555", "-F", "body=@notes.md"]),
        "resolving an id must not by itself authorize anything");
}

#[test]
fn a_releases_tags_url_takes_its_identity_from_the_url_not_from_a_body_field() {
    // Review round 2. `…/releases/tags/<tag>` names its release by TAG, right in
    // the URL — but the gate read that shape as "no locus" and fell through to
    // the ordinary tag path, where a body field answered for it:
    //
    //   gh api -X PATCH repos/o/r/releases/tags/v0.0.9 -f tag_name=<granted>
    //
    // was allowed while addressing somebody else's release. It is B1's shape
    // one endpoint over, and it survived B1's fix because that fix keyed on
    // "the URL carries a numeric id" and this URL carries a tag instead.
    //
    // GitHub exposes no write on that endpoint today, so this was unreachable
    // rather than exploitable — which is exactly why it is worth closing: the
    // gate would have been resting on the shape of someone else's API surface,
    // and a surface that grows a write later does not send us a note. The tag is
    // right there in the URL, so identity is derived from it with NO lookup.
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP a_releases_tags_url_takes_its_identity…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();

    let run = |argv: &[&str]| -> bool {
        Command::new("sh").arg(&shim).args(argv)
            .env("LOOMUX_GROUP_DIR", &group)
            .env("FAKE_REL_MAP", "555=v1.2.3 777=v0.0.9")
            .status().unwrap().success()
    };
    let grant = |tag: &str| {
        let d = group.join("release_grants");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(tag), b"99999999999\n1\n").unwrap();
    };

    // The decoys: URL names v0.0.9's release, body claims the granted tag.
    grant("v1.2.3");
    for decoy in [
        &["api", "-X", "PATCH", "repos/o/r/releases/tags/v0.0.9", "-f", "tag_name=v1.2.3"] as &[&str],
        &["api", "-X", "DELETE", "repos/o/r/releases/tags/v0.0.9", "-f", "tag_name=v1.2.3"],
        &["api", "-X", "PATCH", "repos/o/r/releases/tags/v0.0.9", "-F", "ref=refs/tags/v1.2.3"],
        // A traversal in the tag position names no identifiable release either.
        &["api", "-X", "PATCH", "repos/o/r/releases/tags/../v9", "-f", "tag_name=v1.2.3"],
    ] {
        assert!(!run(decoy), "the URL's tag segment is the locus, not a body field: {decoy:?}");
    }

    // …and the legitimate direction still works: the URL naming the GRANTED tag
    // is allowed, and does it WITHOUT an API call, because the answer is in the
    // URL. (`write_fake_gh` logs every invocation; the shim's id→tag lookup is
    // the only thing that would put `tag_name` in that log for this argv.)
    std::fs::write(&log, b"").unwrap();
    assert!(run(&["api", "-X", "PATCH", "repos/o/r/releases/tags/v1.2.3", "-F", "body=@notes.md"]),
        "a write to the granted tag's own releases/tags URL is allowed");
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(!logged.contains("tag_name"),
        "the URL already carries the tag — resolving it must cost no API call, got: {logged}");

    // Tag CASE is part of a tag, so the value is taken from the ORIGINAL path
    // even though the shape test runs on the lowercased copy: a grant for
    // `vRelease` must still be found when the URL says `vRelease`.
    //
    // Deliberately NOT asserted here: that `vrelease` is refused under a
    // `vRelease` grant. It is refused on Linux and ALLOWED on Windows and macOS,
    // and neither is this change's doing — a grant is looked up by opening
    // `release_grants/<segment>`, so tag matching inherits the host filesystem's
    // case sensitivity. That predates this PR (the same lookup keys merge grants
    // and the git shim's tag push on the base branch) and is disclosed in the PR
    // body rather than papered over with a test that would be red on two of the
    // three CI platforms. What this pin does catch, on any case-sensitive host,
    // is the value being lowercased on its way to the grant lookup.
    grant("vRelease");
    assert!(run(&["api", "-X", "PATCH", "repos/o/r/releases/tags/vRelease", "-F", "body=@notes.md"]),
        "the tag is taken from the original path, so its case survives to the grant lookup");
}

#[test]
fn an_id_addressed_release_write_never_takes_its_identity_from_a_caller_supplied_tag() {
    // rev B1. Widening the grant to a whole pipeline is paid for by the claim
    // that it still reaches exactly ONE tag — and that claim was false. The
    // resolution was conditional on the argv naming no tag, so one extra flag
    // suppressed it and handed a live grant the run of every release in the repo:
    //
    //   gh api -X PATCH repos/o/r/releases/777 -f tag_name=v1.2.3 -f make_latest=true
    //
    // keyed the gate on v1.2.3, found the human's grant for it, and then retagged
    // release 777 and took `latest`. `-X DELETE … -f tag_name=v1.2.3` deleted an
    // arbitrary release the same way (`tag_name` is ignored by the API on a
    // DELETE — it existed only to satisfy the gate).
    //
    // This is the MIRROR of the decoys in
    // `gh_shim_harness_gates_raw_api_tag_ref_by_locus_defeating_decoys`: those
    // try to LOOSEN the gate with a cosmetic `refs/heads`, these SATISFY it with
    // a cosmetic tag. The suite had no case for that direction, which is why the
    // hole survived a green run. The rule pinned here: for a URL that names one
    // release, identity comes from the ID, never from a body field the caller
    // wrote — including when the id cannot be resolved, where the argv tag must
    // not get to answer instead.
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP an_id_addressed_release_write_never_takes…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();

    // 555 → v1.2.3 (granted). 777 → v0.0.9 (someone else's). 888 → a tag_name
    // that is not a plausible ref name at all. Nothing else resolves.
    let run = |argv: &[&str]| -> bool {
        Command::new("sh").arg(&shim).args(argv)
            .env("LOOMUX_GROUP_DIR", &group)
            .env("FAKE_REL_MAP", "555=v1.2.3 777=v0.0.9 888=has;semi")
            .status().unwrap().success()
    };
    let grant = |tag: &str| {
        let d = group.join("release_grants");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(tag), b"99999999999\n1\n").unwrap();
    };
    let audit = || std::fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    let clear_audit = || { std::fs::write(group.join("audit.jsonl"), b"").unwrap(); };

    // Control: the grant genuinely works for the release it is for, so every
    // refusal below is the decoy being rejected and not the gate being shut.
    grant("v1.2.3");
    assert!(run(&["api", "-X", "PATCH", "repos/o/r/releases/555", "-F", "body=@notes.md"]),
        "control: the granted release's own notes write is allowed");

    // THE B1 SHAPES. Each names release 777 — which belongs to v0.0.9 — while
    // claiming the granted tag in a field the caller chose.
    for decoy in [
        &["api", "-X", "PATCH", "repos/o/r/releases/777", "-f", "tag_name=v1.2.3", "-f", "make_latest=true"] as &[&str],
        &["api", "-X", "DELETE", "repos/o/r/releases/777", "-f", "tag_name=v1.2.3"],
        &["api", "-X", "PATCH", "repos/o/r/releases/777", "-F", "ref=refs/tags/v1.2.3"],
        // …and the two shapes where the id CANNOT be resolved, where falling back
        // to the argv tag would reopen the hole in a different dress.
        &["api", "-X", "POST", "repos/o/r/releases/777/assets", "-f", "tag_name=v1.2.3"],
        &["api", "-X", "PATCH", "repos/o/r/releases/../777", "-f", "tag_name=v1.2.3"],
    ] {
        assert!(!run(decoy), "a caller-supplied tag must not authorize an id-addressed write: {decoy:?}");
    }

    // The inverse direction: the URL names the GRANTED release, but the body
    // would move it onto another tag. That publishes a tag nobody authorized, so
    // it is refused rather than allowed on the strength of the resolved tag.
    clear_audit();
    assert!(!run(&["api", "-X", "PATCH", "repos/o/r/releases/555", "-f", "tag_name=v9.9.9"]),
        "retagging the granted release onto an ungranted tag must be refused");
    let a = audit();
    assert!(a.contains("release-id-tag-mismatch"), "the two-tags-named case is audited distinctly, got: {a}");

    // A tag_name read back from the API that is not a plausible ref name is
    // rejected before it can be path-sanitized into some OTHER grant's segment
    // (`has;semi` would sanitize to `has_semi`).
    grant("has_semi");
    assert!(!run(&["api", "-X", "PATCH", "repos/o/r/releases/888", "-F", "body=@notes.md"]),
        "an implausible resolved tag_name must be rejected, not sanitized into a grant match");

    // Unchanged: for `POST …/releases` (create) there is no id in the URL, so
    // tag_name IS the locus and still keys the grant. This is the case the B1
    // fix must not break.
    grant("v1.2.3");
    assert!(run(&["api", "-X", "POST", "repos/o/r/releases", "-f", "tag_name=v1.2.3"]),
        "creating a release still keys on tag_name — there is no id to resolve");
    assert!(!run(&["api", "-X", "POST", "repos/o/r/releases", "-f", "tag_name=v8"]),
        "…and still refuses another tag");
}

#[test]
fn git_shim_harness_gates_tag_pushes() {
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP git_shim_harness…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    // Fake git: rev-parse confirms a tag iff it matches $FAKE_TAG; push "succeeds".
    let fake = root.join("fakegit");
    std::fs::write(&fake,
        "#!/bin/sh\n\
         if [ \"$1\" = \"rev-parse\" ]; then\n\
           for a in \"$@\"; do case \"$a\" in refs/tags/*) [ \"$a\" = \"refs/tags/$FAKE_TAG\" ] && exit 0 ;; esac; done\n\
           exit 1\n\
         fi\n\
         printf 'FAKE-GIT-RAN\\n'; exit 0\n").unwrap();
    let shim = root.join("git");
    std::fs::write(&shim, git_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();
    let run = |argv: &[&str], fake_tag: &str| -> bool {
        Command::new("sh").arg(&shim).args(argv)
            .env("LOOMUX_GROUP_DIR", &group).env("FAKE_TAG", fake_tag)
            .status().unwrap().success()
    };
    let grant = |name: &str| {
        let d = group.join("release_grants");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(name), b"99999999999\n1\n").unwrap();
    };

    // Branch push → untouched (fast passthrough).
    assert!(run(&["push", "origin", "main"], ""), "branch push is never gated");
    // Explicit tag ref → blocked without a grant, allowed (and consumed) with one.
    assert!(!run(&["push", "origin", "refs/tags/v1.2.3"], ""), "tag push blocked without grant");
    grant("v1.2.3");
    assert!(run(&["push", "origin", "refs/tags/v1.2.3"], ""), "tag push allowed with grant");
    // #438: the tag push is the FIRST step of the release, not the whole of it —
    // the grant stays live so the `gh release`/notes steps ride the same one.
    assert!(group.join("release_grants/v1.2.3").exists(), "the tag push must not spend the pipeline grant");
    let _ = std::fs::remove_file(group.join("release_grants/v1.2.3"));
    // Bulk tag push → always blocked.
    assert!(!run(&["push", "--tags"], ""), "--tags is blocked");
    assert!(!run(&["push", "origin", "--follow-tags"], ""), "--follow-tags is blocked");
    // Bare v* refspec confirmed as a tag by real git → gated.
    assert!(!run(&["push", "origin", "v2.0.0"], "v2.0.0"), "confirmed bare v* tag is gated");
    // rev-86 BLOCKER: v* is ANY v-prefixed tag, matching release.yml — a `vbeta`
    // tag (v + letter) MUST be gated, not slip through the old `v[0-9]` pattern.
    grant("vbeta");
    assert!(run(&["push", "origin", "vbeta"], "vbeta"), "granted vbeta tag push allowed");
    std::fs::write(group.join("release_grants/vbeta"), b"1\n1\n").unwrap(); // expire it
    assert!(!run(&["push", "origin", "vbeta"], "vbeta"), "vbeta tag push blocked once the grant expires");
    assert!(!run(&["push", "origin", "vRelease"], "vRelease"), "vRelease (v* tag) is gated");
    // A non-v* ref never triggers release.yml, so it is NOT gated even if it's a tag.
    assert!(run(&["push", "origin", "nightly"], "nightly"), "a non-v* tag is not a release → not gated");
    // Bare v* that is NOT a tag (a branch) → not gated (rev-parse fails to confirm).
    assert!(run(&["push", "origin", "v2-feature"], "nope"), "a v*-looking branch is not gated");
    // auto_release opt-in blanket-allows a v* tag push with NO grant (repeatable).
    std::fs::write(group.join("autonomous"), b"").unwrap();
    std::fs::write(group.join("auto_release"), b"").unwrap();
    assert!(run(&["push", "origin", "refs/tags/v5.0.0"], ""), "autonomous+auto_release blanket-allows a tag push");
    assert!(run(&["push", "origin", "refs/tags/v5.0.1"], ""), "blanket auto_release tag push is repeatable");
    // Supervised dangerous mode (not autonomous): a v* tag push is allowed with the
    // distinct audit marker.
    for m in ["autonomous", "auto_release"] { let _ = std::fs::remove_file(group.join(m)); }
    std::fs::write(group.join("dangerous_mode"), b"").unwrap();
    std::fs::write(group.join("audit.jsonl"), b"").unwrap();
    assert!(run(&["push", "origin", "refs/tags/v6.0.0"], ""), "dangerous mode → tag push allowed");
    let audit = std::fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    assert!(audit.contains("release-gate-dangerous"), "distinct dangerous audit marker, got: {audit}");
    // No-op while autonomous.
    std::fs::write(group.join("autonomous"), b"").unwrap();
    assert!(!run(&["push", "origin", "refs/tags/v6.0.1"], ""), "dangerous ignored while autonomous → tag push blocked");
}

/// A fake `git` whose `push` exits `$FAKE_PUSH_EXIT` (default 0/success) and
/// whose `rev-parse` still confirms `$FAKE_TAG` as a real tag — lets a test
/// simulate git/GitHub refusing a tag push (network, remote hook, protected
/// ref, …) independently of the shim's tag-detection logic.
fn write_fake_git_with_push_exit(root: &std::path::Path) -> std::path::PathBuf {
    let p = root.join("fakegit_push_exit");
    std::fs::write(&p,
        "#!/bin/sh\n\
         if [ \"$1\" = \"rev-parse\" ]; then\n\
           for a in \"$@\"; do case \"$a\" in refs/tags/*) [ \"$a\" = \"refs/tags/$FAKE_TAG\" ] && exit 0 ;; esac; done\n\
           exit 1\n\
         fi\n\
         exit \"${FAKE_PUSH_EXIT:-0}\"\n").unwrap();
    p
}

#[test]
fn git_shim_harness_a_tag_push_that_fails_does_not_burn_the_grant() {
    // #315 (same bug class as #256/#303): the tag-push grant was consumed on
    // interception, before the real `git push` ran — a push git/GitHub
    // refuses (network, remote hook, protected ref, …) burned the one-time
    // grant on a push that never landed, leaving the human to re-grant.
    // #438 makes that unconditional: a release grant is never spent by any
    // outcome, so a refused push has nothing to burn. Proven against the REAL
    // generated shim — and the failure path is the interesting one to keep,
    // because a future re-introduction of consume-on-use would land here first.
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP git_shim_harness_a_tag_push_that_fails…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let fake = write_fake_git_with_push_exit(root);
    let shim = root.join("git");
    std::fs::write(&shim, git_shim_sh(&fake.display().to_string(), &shim_paths())).unwrap();
    let _ = Command::new("sh").arg("-c").arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();

    let run = |push_exit: &str| -> bool {
        Command::new("sh").arg(&shim).args(["push", "origin", "refs/tags/v1.2.3"])
            .env("LOOMUX_GROUP_DIR", &group)
            .env("FAKE_TAG", "v1.2.3")
            .env("FAKE_PUSH_EXIT", push_exit)
            .status().unwrap().success()
    };
    let write_grant = |name: &str| {
        let d = group.join("release_grants");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(name), b"99999999999\n1\n").unwrap();
    };
    let grant_path = |name: &str| group.join("release_grants").join(name);

    // A push git/GitHub refuses must NOT consume the grant.
    write_grant("v1.2.3");
    assert!(!run("1"), "a failed push must fail (surface git's refusal)");
    assert!(grant_path("v1.2.3").exists(), "a failed push must NOT consume the grant — this is the #315 bug");
    // The release path claims nothing, so it can never strand a `.claimed`
    // sibling either — the crash-between-claim-and-settle hole simply has no
    // shape here any more (the merge gate still claims, and still pins it).
    assert!(!grant_path("v1.2.3.claimed").exists(), "the release path must not create a .claimed file at all");

    // The SAME grant authorizes a retry, and a successful push leaves it live
    // for the rest of THIS release's pipeline (#438).
    assert!(run("0"), "retry with the still-usable grant must succeed");
    assert!(grant_path("v1.2.3").exists(), "a successful push leaves the pipeline grant live");
    assert!(!grant_path("v1.2.3.claimed").exists(), "still no .claimed file after a success");
    assert!(run("0"), "the same grant covers a re-push of the same tag inside its window");

    // Expired grants are refused and cleaned up — the one and only stop.
    std::fs::write(group.join("release_grants/v1.2.3"), b"1\n1\n").unwrap();
    assert!(!run("0"), "expired grant → blocked");
    assert!(!grant_path("v1.2.3").exists(), "expired grant is cleaned up, not left usable");

    // A stray `.claimed` file (left over from a pre-#438 shim, or hand-made)
    // is NOT a grant path and must authorize nothing.
    std::fs::create_dir_all(group.join("release_grants")).unwrap();
    std::fs::write(group.join("release_grants/v1.2.3.claimed"), b"99999999999\n1\n").unwrap();
    assert!(!run("0"), "a stray .claimed file with no live grant must not authorize a push");
}

/// Extract a named shell function's body — everything from the line after its
/// `name() {` header through the matching top-level `}` — out of a generated
/// shim script. `loomux_grant_claim`/`loomux_grant_settle` are straight-line
/// and `case`/`esac` shell (no brace-using constructs), so a plain "next line
/// that is exactly `}`" scan is exact here, not a heuristic.
fn extract_shell_fn(script: &str, name: &str) -> String {
    let marker = format!("{name}() {{");
    let start = script.find(&marker).unwrap_or_else(|| panic!("{name} not found in script"));
    let body_start = start + script[start..].find('\n').unwrap() + 1;
    let rest = &script[body_start..];
    let end = rest.find("\n}\n").unwrap_or_else(|| panic!("{name} has no closing brace"));
    rest[..end].to_string()
}

#[test]
fn gh_and_git_shim_release_grant_check_stays_byte_identical() {
    // #315 review NB1, carried forward to #438's replacement: the release-grant
    // validity check is inlined into two separately generated scripts with no
    // shared shell lib, and it is the single place deciding what a release grant
    // is worth. A one-sided edit — a TTL comparison relaxed in one shim, an
    // expiry cleanup dropped from the other — must go red here, not drift.
    //
    // It is now substituted from ONE Rust const, so this asserts a property the
    // build already guarantees rather than reconciling two hand-kept copies;
    // that is deliberate belt-and-braces on a security boundary, and it is what
    // goes red if someone "helpfully" re-inlines a second copy.
    let gh = gh_shim_sh("C:/Program Files/GitHub CLI/gh.exe", &shim_paths());
    let git = git_shim_sh("C:/Program Files/Git/cmd/git.exe", &shim_paths());
    let a = extract_shell_fn(&gh, "loomux_release_grant_valid");
    let b = extract_shell_fn(&git, "loomux_release_grant_valid");
    assert_eq!(a, b, "loomux_release_grant_valid has drifted between the gh shim and the git shim");
    // It must genuinely gate on the expiry, not merely exist.
    assert!(a.contains("head -n1") && a.contains("date +%s") && a.contains("-ge"),
        "the release-grant check must compare the grant's expiry against the clock, got: {a}");
    assert!(a.contains("rm -f"), "an expired release grant must be deleted, not left on disk");
    // The one-time claim/settle machinery now serves the MERGE gate only — it
    // must be gone from the git shim, which has no one-time grants left.
    assert!(gh.contains("loomux_grant_claim") && gh.contains("loomux_grant_settle"),
        "the gh shim's merge gate still claims/settles its one-time per-PR grant");
    assert!(!git.contains("loomux_grant_claim") && !git.contains("loomux_grant_settle"),
        "the git shim gates only releases, which are never consumed — no dead claim/settle");
}

/// Record that a #509 pin could not arm — visibly, and in a way that cannot let
/// coverage silently reach zero.
///
/// **rev-21 B2.** The first cut was `eprintln!` + `return`. A `return` from a
/// `#[test]` IS a pass, and `ci.yml` runs `cargo test --locked` with no
/// `--nocapture`, so libtest captures a passing test's stderr and prints
/// nothing — grep all three #525 build jobs and there is not one SKIP line. The
/// stated rationale, "skip loudly or run armed, nothing between", did not ship:
/// on macOS and Windows these pins reported `... ok`, indistinguishable from
/// armed. That is the same fail-open-while-looking-healthy shape as the bug this
/// PR fixes, one level up, and it is why the mechanism — not the intent — had to
/// change.
///
/// Two mechanisms, because visibility alone is not enough:
///
/// 1. **Visible.** The skip is appended to `$GITHUB_STEP_SUMMARY` when set —
///    plain file IO that no test harness intercepts, rendered on the run page.
///    A skip taken on CI is now readable rather than merely intended to be.
/// 2. **Cannot reach zero.** On Linux this is not a skip at all, it FAILS.
///    ubuntu-22.04 is the only job that arms these pins (run 30631911457
///    observed the headline pin red there against the mutated shim), so an
///    unarmed pin on Linux *is* #509 coverage at zero with CI green. Making it
///    red is what turns "coverage should not silently vanish" from an
///    aspiration into a property.
fn pin_could_not_arm(test: &str, why: &str) {
    use std::io::Write;
    let msg = format!("SKIP {test}: {why}");
    // Straight to fd 2, NOT `eprintln!`. libtest's capture is a thread-local
    // consulted by the `print!`/`eprintln!` machinery; a direct write to the
    // `Stderr` handle does not go through it, so this lands in the CI log of a
    // PASSING test — which `eprintln!` demonstrably did not (zero SKIP lines
    // across three #525 build jobs, rev-21 B2). Verified by reading the log of
    // the run that introduced it, not by assuming: see the PR body.
    let _ = writeln!(std::io::stderr(), "{msg}");
    // Belt: the run-page summary, which survives even if a future libtest does
    // capture fd 2.
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "- :warning: **#509 pin did not arm** — `{msg}`");
        }
    }
    assert!(
        !cfg!(target_os = "linux"),
        "{msg}\n\nOn Linux the #509 stripped-PATH pins MUST arm: ubuntu-22.04 is the only job \
         that arms them, so an unarmed pin here means #509 coverage is at ZERO while CI is \
         green — precisely the failure this guard exists to make impossible. If a runner or \
         image change made this legitimate, fix the harness or move the guard deliberately. Do \
         not delete it to get back to green."
    );
}

/// The MSYS form of a path, for baking into a shim fixture. A separate
/// implementation from `winpath::to_msys_dir`, not a call to it — the product's
/// own conversion is pinned independently by
/// `winpath::tests::msys_dir_rewrites_a_drive_letter_and_leaves_posix_paths_alone`.
fn msys_dir_for_fixture(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut c = s.chars();
    match (c.next(), c.next()) {
        (Some(d), Some(':')) if d.is_ascii_alphabetic() => {
            format!("/{}/{}", d.to_ascii_lowercase(), s[2..].trim_start_matches('/'))
        }
        _ => s,
    }
}

/// Any POSIX `sh` this host can run a shim under — the product's own resolution
/// on Windows, `/bin/sh` elsewhere. Unlike `stripped_path_harness` this asks for
/// nothing but a shell, so a test that supplies its own `utils_dir` can run on
/// every platform.
fn any_posix_sh() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        resolve_shim_toolchain().0
    }
    #[cfg(not(target_os = "windows"))]
    {
        Some("/bin/sh".to_string())
    }
}

/// The `sh` the ts behavioural pins run under — `any_posix_sh`, but a host
/// that cannot resolve one is a HARD failure, not a skip (#3259 item 3).
///
/// The #3249 ts pins arm on EVERY CI leg today: Linux and macOS get
/// `/bin/sh` unconditionally (`any_posix_sh`'s non-Windows arm never
/// answers None), and windows-latest ships Git for Windows — the shim
/// feature's own dependency, since the product's `resolve_shim_toolchain`
/// is exactly what `any_posix_sh` consults here. The only leg that can
/// silently lose `sh` is Windows, and there the absence is a runner
/// defect, not a legitimate host property: #509's lesson was that a
/// green-while-unarmed pin is indistinguishable from coverage, so the
/// degradation path must not exist. Counting armed-vs-skipped across legs
/// was the alternative and is declined: the count lands in per-job step
/// summaries nobody is forced to read, while a panic is red CI nobody can
/// miss. The #509 stripped-PATH pins are deliberately NOT switched — they
/// CAN legitimately skip (macOS splits the coreutils), so they keep
/// `pin_could_not_arm`. Pinned by
/// `ts_pins_hard_fail_when_posix_sh_cannot_be_resolved`.
fn ts_pin_sh(test: &str) -> String {
    ts_pin_sh_resolved(test, any_posix_sh())
}

/// `ts_pin_sh` with the resolution as a PARAMETER, so the None case is
/// drivable: no CI leg can produce it without going red — that is the
/// point — so the pin feeds the None by hand.
fn ts_pin_sh_resolved(test: &str, sh: Option<String>) -> String {
    match sh {
        Some(sh) => sh,
        None => panic!(
            "{test}: no POSIX sh on this host — the ts behavioural pins MUST arm \
             (every CI leg has one; windows-latest ships Git Bash, the shim \
             feature's declared dependency), so this hard-fails instead of \
             silently un-arming the ts coverage (#3259 item 3). If a runner \
             or image change made this legitimate, fix the harness or move \
             the guard deliberately — do not delete it to get back to green."
        ),
    }
}

/// The POSIX tools the shims' dependency preamble asserts (#509). One list, two
/// consumers: `stripped_path_harness` needs them all present in a single
/// directory before it can arm a pin, and
/// `gh_and_git_shim_deps_preamble_stays_byte_identical` pins that the generated
/// script really covers exactly these. Mirrors the list in `shim_deps_preamble`
/// — if that one grows a tool, this goes with it or the byte-identical test says
/// so.
const SHIM_DEP_TOOLS: [&str; 7] = ["tr", "head", "tail", "date", "cat", "rm", "mv"];

/// The `(sh, ShimPaths)` a stripped-PATH harness should use on THIS host, or
/// `None` if it cannot assemble one (#509).
///
/// On Windows this is the product's own resolution, so the pin exercises what
/// actually ships. Off Windows `resolve_shim_toolchain` deliberately bakes
/// nothing — there is no `.cmd` layer and the coreutils are simply on PATH —
/// but the dependency preamble is the *same generated script*, so the test
/// supplies the equivalent itself: `/bin/sh` plus the directory `tr` really
/// lives in. That keeps the headline pin ARMED on the unix runners, where a
/// POSIX `sh` honors a stripped PATH, instead of leaving it skipped on every
/// platform CI builds.
fn stripped_path_harness() -> Option<(String, ShimPaths)> {
    #[cfg(target_os = "windows")]
    {
        let (sh, paths) = resolve_shim_toolchain();
        if paths.utils_dir.is_none() {
            return None;
        }
        sh.map(|s| (s, paths))
    }
    #[cfg(not(target_os = "windows"))]
    {
        // EVERY dependency has to live in the ONE directory `utils_dir` names.
        // The shim takes a single directory, not a list, because Git for Windows
        // ships all of them in one `usr\bin` — so a host that splits them cannot
        // supply a sufficient one and must SKIP rather than hand the shim a
        // directory that was never going to work. macOS is exactly that host
        // (`/usr/bin/tr` but `/bin/date`), and the first cut of this helper —
        // "find `tr`, use its directory" — sent the macOS runner red: the shim
        // correctly refused, naming `date`, for a reason with nothing to do with
        // #509. Checking the whole set is what makes the skip honest.
        for cand in ["/usr/bin", "/bin"] {
            let dir = std::path::Path::new(cand);
            if SHIM_DEP_TOOLS.into_iter().all(|d| dir.join(d).is_file()) {
                return Some((
                    "/bin/sh".to_string(),
                    ShimPaths { utils_dir: Some(dir.display().to_string()), git_dir: None },
                ));
            }
        }
        None
    }
}

/// A wrapper that reproduces what a PowerShell/cmd pane hands the shim (#509):
/// a PATH carrying no Git for Windows coreutils. It runs whatever script it is
/// given under `sh_abs`, so the same wrapper drives both the shim and the
/// precondition probe.
///
/// The PATH is assigned **inside the shell**, not in the child's environment,
/// and that is the whole point. Setting it via `Command::env_clear()` +
/// `.env("PATH", …)` is not something a test can be built on: the MSYS runtime
/// re-seeds PATH when a native Windows parent launches `sh.exe`, and on the
/// GitHub windows runner it put the coreutils back — the un-repaired shim then
/// found `tr`, behaved perfectly correctly, and the fail-closed pin failed for
/// a reason with nothing to do with the code under test (#509, CI attempts 1
/// and 2, identically). An assignment in the script runs after all of that —
/// and it is verified, not assumed: see
/// `assert_wrapper_really_hides_the_coreutils`.
///
/// `sh_abs` is baked in and the target is passed to it as a script argument
/// rather than `exec`d directly: with PATH stripped there is no `sh` left to
/// find, and a Windows checkout has no executable bit to rely on either.
fn write_stripped_path_wrapper(root: &std::path::Path, sh_abs: &str) -> std::path::PathBuf {
    let p = root.join("strip_path.sh");
    std::fs::write(&p, format!(
        "#!/bin/sh\nPATH=\"/loomux-nonexistent-509\"\nexport PATH\nexec \"{sh_abs}\" \"$@\"\n"
    )).unwrap();
    p
}

/// Run the generated gh shim the way a `.cmd` delegator does — `sh.exe` by
/// ABSOLUTE path (#335) — through the coreutils-free wrapper above.
/// Returns (success, both output streams for the diagnostic).
fn run_shim_with_stripped_path(
    sh_abs: &str,
    wrapper: &std::path::Path,
    shim: &std::path::Path,
    group: &std::path::Path,
    argv: &[&str],
) -> (bool, String) {
    let out = std::process::Command::new(sh_abs)
        .arg(wrapper)
        .arg(shim)
        .args(argv)
        .env("LOOMUX_GROUP_DIR", group)
        .env("FAKE_BASE", "main")
        .env("FAKE_DEFAULT", "main")
        .env("FAKE_NUM", "1")
        .output()
        .unwrap();
    // Both streams in the diagnostic: a shim that wrongly passes through says so
    // on stdout (the real gh's output), and one that refuses says so on stderr.
    (
        out.status.success(),
        format!(
            "[stderr] {} [stdout] {}",
            String::from_utf8_lossy(&out.stderr).trim(),
            String::from_utf8_lossy(&out.stdout).trim()
        ),
    )
}

/// Whether this host can actually express #509's condition — the shim's own
/// `tr` failing — under the stripped-PATH wrapper.
///
/// **A functional capability probe, deliberately, not platform detection.** It
/// runs `printf x | tr x y` and checks the output, rather than asking
/// `command -v` or branching on the OS. What the gate depends on is `tr`
/// *working*; a `tr` that resolves but cannot run and a `tr` that runs despite
/// a stripped PATH are both answered correctly by executing it. Sniffing the
/// platform would encode today's runner image as a fact about Windows, which it
/// is not — this same host arms the pin when its `sh` honors the strip.
///
/// `false` means the environment refuses to be stripped, and the caller must
/// then emit a LOUD, NAMED skip. That is not hypothetical: on the GitHub
/// windows runner the Git-for-Windows coreutils stay reachable however PATH is
/// set — via the child environment (#509 CI attempts 1–2) or assigned inside
/// the shell before `exec` (attempt 3, where this probe reported `FOUND`).
///
/// The rule is: **skip loudly or run armed, never anything between.** Softening
/// the check into a pass would leave a test that looks like it guards #509 and
/// silently does not — the exact failure class `.loomux/lessons.md` calls "a
/// claim is a deliverable". A host that cannot hide the coreutils is also a
/// host on which #509 cannot reproduce, so skipping there loses no coverage
/// that was ever available; it just has to say so out loud.
fn stripped_path_hides_the_coreutils(sh_abs: &str, wrapper: &std::path::Path, root: &std::path::Path) -> bool {
    let probe = root.join("probe_tr.sh");
    std::fs::write(&probe, "#!/bin/sh\nprintf x | tr x y\n").unwrap();
    let out = std::process::Command::new(sh_abs).arg(wrapper).arg(&probe).output().unwrap();
    // A working `tr` prints exactly "y". Anything else — not found, broken, no
    // output at all — means the condition IS expressible here.
    String::from_utf8_lossy(&out.stdout).trim() != "y"
}

/// **rev-21 N2 — the invariant, and the one #509 pin that arms EVERYWHERE.**
///
/// A `tr` that RESOLVES but cannot RUN reproduces #509 exactly: empty
/// substitution, gate matches nothing, tag deletion straight through. The
/// startup self-check is structurally blind to it, because `command -v` proves
/// resolution and nothing more — which is the gap rev-21 N1 names. The
/// point-of-use guard (`loomux_norm_guard`) is what closes it, and this pins
/// that it does.
///
/// Unlike the two stripped-PATH pins, this one needs the environment to hide
/// nothing: it bakes a `utils_dir` of its own holding a deliberately broken
/// `tr`, so the shim's own PATH repair puts that tool first on every platform.
/// No capability probe, no skip, no `#[cfg]` — it arms on all four CI jobs,
/// which is also why it is the pin that does not depend on ubuntu staying the
/// only armed one.
#[test]
fn gh_shim_refuses_when_tr_resolves_but_cannot_run() {
    use std::process::Command;
    let Some(sh) = any_posix_sh() else {
        pin_could_not_arm("gh_shim_refuses_when_tr_resolves_but_cannot_run", "no POSIX sh on this host");
        return;
    };
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let utils = root.join("utils");
    std::fs::create_dir_all(&utils).unwrap();
    // Resolvable — `command -v tr` finds it, so the startup probe is satisfied —
    // and broken: it prints nothing and exits non-zero, exactly what a corrupt
    // install, an arch mismatch or a fork failure looks like at the call site.
    let broken_tr = utils.join("tr");
    std::fs::write(&broken_tr, "#!/bin/sh\nexit 1\n").unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh(root, &log);
    let shim = root.join("gh");
    // Only `tr` is shadowed: every other dependency still resolves from the
    // inherited PATH, so the self-check passes and the ONLY thing wrong is that
    // one normalizer cannot run.
    std::fs::write(&shim, gh_shim_sh(
        &fake.display().to_string(),
        &ShimPaths { utils_dir: Some(msys_dir_for_fixture(&utils)), git_dir: None },
    )).unwrap();
    let _ = Command::new(&sh).arg("-c")
        .arg(format!("chmod +x '{}' '{}'", fake.display(), broken_tr.display())).status();

    let out = Command::new(&sh).arg(&shim)
        .args(["api", "-X", "DELETE", "repos/o/r/git/refs/tags/v1.2.3"])
        .env("LOOMUX_GROUP_DIR", &group)
        .env("FAKE_BASE", "main").env("FAKE_DEFAULT", "main").env("FAKE_NUM", "1")
        .output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(!out.status.success(),
        "a published tag ref must not be deletable when `tr` cannot run, stderr: {err}");
    assert_eq!(std::fs::read_to_string(&log).unwrap_or_default(), "",
        "the real gh must never be reached");
    assert!(err.contains("could not normalize"),
        "the refusal must name the failed normalization, got: {err}");
    // THE POINT: the startup dependency check did NOT save us here — `tr`
    // resolved fine. If this ever starts firing instead, the guard below has
    // stopped being the thing under test.
    assert!(!err.contains("cannot find the POSIX tool"),
        "this must be caught at the point of use, not by the resolution probe: {err}");
    let audit = std::fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    assert!(audit.contains("gate-degraded-normalize-failed"),
        "and it must be audited distinctly from a missing tool, got: {audit}");
}

/// **The #509 pin.** Invoked from a PowerShell/cmd pane the shim's `sh` inherits
/// a PATH with no Git for Windows coreutils, and every `tr` in it died with
/// "command not found" — leaving its command substitution EMPTY and carrying on.
/// Empty is not a safe default: an empty `path_low`/`low` matched none of the
/// `gh api` release/merge arms, so `gh api -X DELETE …/git/refs/tags/v1.2.3` and
/// a graphql `mergePullRequest` — both refused under Git Bash — sailed straight
/// through to the real gh. This runs the REAL generated shim under exactly that
/// PATH and pins that they are refused, and that an ordinary read still works.
///
/// MUTATION CHECK: take the PATH repair away (`ShimPaths::default()`) and the
/// last assertion here goes red — see
/// `gh_shim_without_its_coreutils_fails_closed_not_open`, which pins what the
/// un-repaired shim does instead (refuse loudly, never pass through).
#[test]
fn gh_shim_gates_the_api_shapes_when_the_callers_path_lacks_git_coreutils() {
    let Some((sh_abs, paths)) = stripped_path_harness() else {
        pin_could_not_arm("gh_shim_gates_the_api_shapes…",
            "no POSIX sh, or the 7 dependencies do not share one directory on this host");
        return;
    };
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &paths)).unwrap();
    // The shim `exec`s the fake gh, which needs an executable bit on the unix
    // runners. No-op on Windows.
    let _ = std::process::Command::new(&sh_abs).arg("-c")
        .arg(format!("chmod +x '{}'", fake.display())).status();
    let wrapper = write_stripped_path_wrapper(root, &sh_abs);
    if !stripped_path_hides_the_coreutils(&sh_abs, &wrapper, root) {
        pin_could_not_arm("gh_shim_gates_the_api_shapes…",
            "this host's sh cannot be made to hide the coreutils, so #509's condition is inexpressible here");
        return;
    }

    let ran = || std::fs::read_to_string(&log).unwrap_or_default();
    // Every one of these is a publish-to-the-world or merge shape that only the
    // `tr`-normalized `gh api` arms catch. Each must be REFUSED, and the real gh
    // must never be reached.
    //
    // The graphql queries use VARIABLES rather than inline string literals on
    // purpose, and it is not cosmetic: an argument carrying a `"` is escaped by
    // Rust's Windows spawner as `\"`, and the MSYS runtime that parses `sh.exe`'s
    // command line splits on that — the argument arrives truncated at the first
    // quote and the test stops testing what it says it does. (Measured: the
    // literal form reached the shim as `query=mutation` and nothing else.)
    // Variables are also the shape #196 r6 says a text scan must not be beaten
    // by, so this is the harder case anyway.
    for (argv, what) in [
        (vec!["api", "-X", "DELETE", "repos/o/r/git/refs/tags/v1.2.3"], "DELETE of a published tag ref"),
        (vec!["api", "-X", "POST", "repos/o/r/releases", "-f", "tag_name=v9.9.9"], "POST of a release"),
        (vec!["api", "graphql", "-f", "query=mutation M($id:ID!){mergePullRequest(input:{pullRequestId:$id}){clientMutationId}}"], "graphql mergePullRequest"),
        (vec!["api", "graphql", "-f", "query=mutation M($t:String!){createRelease(input:{tagName:$t}){clientMutationId}}"], "graphql createRelease"),
    ] {
        let before = ran();
        let (ok, err) = run_shim_with_stripped_path(&sh_abs, &wrapper, &shim, &group, &argv);
        assert!(!ok, "{what} must be refused under a PowerShell-style PATH, {err}; fake gh saw: {}", ran());
        assert_eq!(ran(), before, "{what} must never reach the real gh (it did)");
        assert!(!err.contains("command not found"),
            "the gate must not be normalizing with tools it cannot find, stderr: {err}");
    }
    // …and the shim is still USABLE: an ordinary read passes through. This is the
    // assertion the mutation kills — without the PATH repair the dependency
    // self-check refuses this too.
    let before = ran();
    let (ok, err) = run_shim_with_stripped_path(&sh_abs, &wrapper, &shim, &group, &["api", "repos/o/r"]);
    assert!(ok, "an ordinary GET must still pass through, stderr: {err}");
    assert_ne!(ran(), before, "an ordinary GET must reach the real gh");
}

/// The other half of the #509 mutation check: with NO coreutils dir baked in
/// (the pre-fix state, and the state on a machine where shim-write time could
/// not find one), the shim must refuse LOUDLY rather than run a gate that
/// silently skips its own normalization. Fail closed, never open — including for
/// the innocuous read that the repaired shim lets through above.
#[test]
fn gh_shim_without_its_coreutils_fails_closed_not_open() {
    let Some((sh_abs, _)) = stripped_path_harness() else {
        pin_could_not_arm("gh_shim_without_its_coreutils…",
            "no POSIX sh, or the 7 dependencies do not share one directory on this host");
        return;
    };
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    let log = root.join("gh.log");
    let fake = write_fake_gh(root, &log);
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(&fake.display().to_string(), &ShimPaths::default())).unwrap();
    let _ = std::process::Command::new(&sh_abs).arg("-c")
        .arg(format!("chmod +x '{}'", fake.display())).status();
    let wrapper = write_stripped_path_wrapper(root, &sh_abs);
    if !stripped_path_hides_the_coreutils(&sh_abs, &wrapper, root) {
        pin_could_not_arm("gh_shim_without_its_coreutils…",
            "this host's sh cannot be made to hide the coreutils, so #509's condition is inexpressible here");
        return;
    }

    for argv in [
        vec!["api", "-X", "DELETE", "repos/o/r/git/refs/tags/v1.2.3"],
        vec!["api", "repos/o/r"],
        vec!["pr", "merge", "1"],
    ] {
        let (ok, err) = run_shim_with_stripped_path(&sh_abs, &wrapper, &shim, &group, &argv);
        assert!(!ok, "{argv:?} must be refused when the shim cannot normalize, stderr: {err}");
    }
    assert_eq!(std::fs::read_to_string(&log).unwrap_or_default(), "",
        "a shim that cannot normalize must never reach the real gh");
    let (_, err) = run_shim_with_stripped_path(&sh_abs, &wrapper, &shim, &group, &["api", "repos/o/r"]);
    assert!(err.contains("cannot find the POSIX tool"),
        "the refusal must SAY what is missing, not just fail, got: {err}");
    let audit = std::fs::read_to_string(group.join("audit.jsonl")).unwrap_or_default();
    assert!(audit.contains("gate-degraded-missing-dep"), "and it must be audited, got: {audit}");
}

/// #509 (second, separate cause): `gh pr create` died with
/// `failed to run git: 'merge' is not recognized` because gh shells out to
/// `git config --get-regexp ^branch\.<b>\.(remote|merge)$`, that resolved to our
/// `git.cmd`, and the `cmd.exe /c` layer Windows needs to run a `.cmd` re-parses
/// the command line — splitting it at the unquoted `|`. The shim answers by
/// handing the real gh a PATH whose `git` is a native `git.exe`, for gh
/// BUILT-IN subcommands only. Pinned behaviorally: the real gh sees the git dir
/// first for `gh pr …`, and does NOT for a token that could be an alias or an
/// extension (which run agent-authored code and keep the gated git).
#[test]
fn gh_shim_gives_the_real_gh_a_native_git_only_for_builtin_subcommands() {
    use std::process::Command;
    if Command::new("sh").arg("-c").arg("exit 0").status().map(|s| !s.success()).unwrap_or(true) {
        eprintln!("SKIP gh_shim_gives_the_real_gh_a_native_git…: no POSIX sh");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();
    // A fake gh that reports the PATH it was handed, so we can see what its own
    // `git` lookup would find.
    let log = root.join("path.log");
    let fake = root.join("fakegh_path");
    std::fs::write(&fake, format!(
        "#!/bin/sh\nprintf '%s\\n' \"$PATH\" > \"{}\"\nexit 0\n", log.display()
    )).unwrap();
    let gitdir = "/c/loomux-test-real-git";
    let shim = root.join("gh");
    std::fs::write(&shim, gh_shim_sh(
        &fake.display().to_string(),
        &ShimPaths { utils_dir: shim_paths().utils_dir, git_dir: Some(gitdir.into()) },
    )).unwrap();
    let _ = Command::new("sh").arg("-c")
        .arg(format!("chmod +x '{}' '{}'", fake.display(), shim.display())).status();

    let path_seen = |argv: &[&str]| -> String {
        let _ = std::fs::remove_file(&log);
        Command::new("sh").arg(&shim).args(argv)
            .env("LOOMUX_GROUP_DIR", &group)
            .status().unwrap();
        std::fs::read_to_string(&log).unwrap_or_default()
    };
    let p = path_seen(&["pr", "view", "1"]);
    assert!(p.starts_with(&format!("{gitdir}:")),
        "`gh pr view` must hand the real gh a native git first on PATH, got: {p}");
    let p = path_seen(&["repo", "view"]);
    assert!(p.starts_with(&format!("{gitdir}:")), "…and `gh repo view` likewise, got: {p}");
    // An alias or an extension runs agent-authored code: the GATED git stays.
    for argv in [vec!["some-alias"], vec!["extension", "exec", "x"]] {
        let p = path_seen(&argv);
        assert!(!p.starts_with(&format!("{gitdir}:")),
            "{argv:?} must NOT get the ungated git — it can run agent-authored code, got: {p}");
    }
}

/// Normalizer sites deliberately NOT wrapped in `loomux_norm_guard`, each with
/// the reason its emptiness is safe (#509 rev-32 NB2, re-keyed by rev-36 NBa,
/// text-pinned by #564 O2).
///
/// `(variable, a substring identifying THAT SITE, the site's EXACT source line,
/// reason)`.
///
/// **The context substring is load-bearing, not decoration.** Keyed by variable
/// name alone — as this list shipped — a NEW unguarded fail-open site that
/// happened to reuse a generic name would inherit the exemption and pass
/// silently; `rest` is the obvious candidate, and it appears twice already. That
/// made this list's own "cannot rot into a blanket permission" claim false on
/// the day it landed, which is worse than a wrong site: the list is the
/// mechanism the entire guarded/exempt split rests on, so a false claim ABOUT it
/// undermines every claim made THROUGH it. The pin also asserts each context
/// matches exactly one site, so an entry cannot silently widen later either.
///
/// **The third field closes #564 O2 — the exempted LINE, verbatim.** Context +
/// count catch an exemption that stops matching, or matches too much. Neither
/// can catch an exempted line REWRITTEN IN PLACE while keeping its context: same
/// variable, same surrounding shape, different behaviour, still exactly one
/// match. The reason recorded here was decided about a specific line, so the
/// pin holds that line still: any edit to it — even a harmless one — reddens and
/// forces the reason to be re-decided rather than inherited. Update this text
/// only together with the reason beside it, never to make the assertion quiet.
const UNGUARDED_TR_SITES: &[(&str, &str, &str, &str)] = &[
    (
        "cur_head",
        "\"$cur_head\" | tr",
        r#"cur_head=$(printf '%s' "$cur_head" | tr '[:upper:]' '[:lower:]')"#,
        "the workflow gate's head oid: empty ⇒ the `[ -n \"$cur_head\" ] ||` on the very next          line blocks with unresolved-head, so the gate refuses rather than proceeds",
    ),
    (
        "rest",
        "*tagName:*)",
        r#"*tagName:*)   rest=${a_query#*tagName:}; rest=$(printf '%s' "$rest" | tr -d ' "'); rtag=${rest%%,*}; rtag=${rtag%%\}*}; rtag=${rtag%%)*} ;;"#,
        "tag EXTRACTION for grant keying, not gate matching: empty ⇒ empty rtag ⇒          loomux_release_gate is still entered and can then only allow via a blanket marker or          a grant file it cannot name — it never decides WHETHER to gate",
    ),
    (
        "rest",
        "*refs/tags/*)",
        r#"*refs/tags/*) rest=${a_query#*refs/tags/}; rest=$(printf '%s' "$rest" | tr -d ' "'); rtag=${rest%%,*}; rtag=${rtag%%\}*}; rtag=${rtag%%)*} ;;"#,
        "the same extraction, the refs/tags literal arm",
    ),
    (
        "a_head",
        "\"$c_head\" | tr",
        r#"a_head=$(printf '%s' "$c_head" | tr -d '"\\')"#,
        "#2985: an AUDIT-ONLY copy of the PR's head ref. The ownership DECISION a few lines below reads the unsanitised `$c_head`, which this line does not touch, so an empty result here cannot widen or narrow what the gate allows - it blanks one FIELD of one audit row. That is the inverse of #509's hazard, where an empty normalizer output matched every gate pattern. It is sanitised at all because a git ref may contain a quote, and an unescaped one FORGES a JSON field rather than merely corrupting it",
    ),
    (
        "a_branch",
        "\"$c_branch\" | tr",
        r#"a_branch=$(printf '%s' "$c_branch" | tr -d '"\\')"#,
        "#2985: an AUDIT-ONLY copy of the CALLER's own branch, from the roster. The ownership comparison reads the unsanitised `$c_branch`; this copy reaches only the `own` field of a blocked-close row, so emptiness costs a blank field and never a decision",
    ),
    (
        "a_role",
        "\"$c_role\" | tr",
        r#"a_role=$(printf '%s' "$c_role" | tr -d '"\\')"#,
        "#2985: an AUDIT-ONLY copy of the caller's role. The orchestrator test that can ALLOW a close reads `$c_role`, not this, so a failed `tr` here cannot promote or demote anyone - it blanks the `role` field of the record",
    ),
    (
        "a_pr",
        "\"$c_pr\" | tr",
        r#"a_pr=$(printf '%s' "$c_pr" | tr -d '"\\')"#,
        "#2985: an AUDIT-ONLY copy of the PR number. The refusal TEXT the agent reads interpolates `$c_pr`, and the gh call itself carries the caller's own argv, so emptiness here loses the `pr` field of one row and nothing else",
    ),
    (
        "a_owner",
        "\"$c_owner\" | tr",
        r#"a_owner=$(printf '%s' "$c_owner" | tr -d '"\\')"#,
        "#2985: an AUDIT-ONLY copy of the owning agent id. The refusal TEXT names the owner via `$c_owner_clause`, built from the unsanitised `$c_owner`; this copy is only the `owner` field of a blocked-close row. It is also the one field whose emptiness is already MEANINGFUL (nobody on the roster owns that branch), so a failed `tr` degrades to a value the reader already handles",
    ),
];

/// Is `text` a `tr` PIPELINE — a `|` whose command WORD is `tr`?
///
/// Indifferent to the spacing between the two (`| tr`, `|tr`, `|  tr` all
/// count), so the site count the pin asserts is a tripwire for a genuinely new
/// shape rather than for formatting — but `tr` has to be the whole word, which
/// the first cut got wrong in both directions:
///
/// * `… || true` counted as a site (#564 O3): the second `|` is followed by
///   `tr`, and a substring test cannot tell that from a pipeline. Same for
///   `| truncate`. Over-counting is the fail-safe direction, so it was never a
///   gate risk — it is fixed because a scan with known false positives is one
///   somebody eventually loosens to make quiet, and the loosening is where the
///   risk enters.
/// * The first fix for that squashed the whitespace out and then demanded a word
///   boundary — which the squashing had just destroyed. `| tr A a` becomes
///   `|trAa` and stops counting: a real site, missed. Nothing in the shipped
///   shims is written that way (every one passes `-c`, `-d` or a `'[:class:]'`
///   next), so the count did not move and the mistake would have sat there until
///   somebody wrote the plainest possible `tr` call. Caught by simulating the
///   scanner against the real templates before pushing, not by the count.
///
/// So: find the pipe, skip the spacing, and require `tr` not to run on into a
/// longer word.
fn is_tr_pipeline(text: &str) -> bool {
    text.char_indices().filter(|(_, c)| *c == '|').any(|(i, _)| {
        text[i + 1..]
            .trim_start()
            .strip_prefix("tr")
            .is_some_and(|after| {
                after.chars().next().map_or(true, |c| !c.is_ascii_alphanumeric() && c != '_')
            })
    })
}

/// Which variables does this generated-shim line assign from a `tr` pipeline?
/// The per-line scan behind
/// `every_shim_normalizer_is_guarded_or_explicitly_exempted`, lifted out of that
/// test's body so the scanner is testable in its own right (#564 O3) instead of
/// only observable through the pin it drives.
///
/// **Every `=$(…)` on the line, not the first** (#564 rev-1 B2). The first cut
/// took `line.find("=$(")` and asked whether the LINE contained a `tr` pipeline
/// anywhere, which is wrong twice over: a second normalizer appended beside an
/// existing site was neither counted nor checked (invisible, and leaving the
/// exact count correct — the O1 shape inside the shape the scan claims to
/// recognise), and a `tr` pipeline in the *second* substitution was attributed to
/// the *first* substitution's variable. The shim already writes several
/// statements per line — the two exempted `rest=…; rest=$(… | tr …); rtag=…`
/// arms are exactly that shape — so this is the natural next edit, not a
/// hypothetical. Each substitution is now delimited by its own matching `)` and
/// tested for a `tr` pipeline on its own.
///
/// A comment is never a site: the dependency preamble quotes
/// `x=$(printf … | tr …)` while explaining #509, and would otherwise be reported
/// as an unguarded one.
///
/// Still line-shaped, and that residual is real: an assignment split across two
/// lines is invisible here, which is what the count assertion's hint names.
fn tr_pipeline_assignment_vars(line: &str) -> Vec<String> {
    if line.trim_start().starts_with('#') {
        return Vec::new();
    }
    let mut vars = Vec::new();
    let mut from = 0usize;
    while let Some(off) = line[from..].find("=$(") {
        let eq = from + off; // the `=`
        let open = eq + 2; // the `(`
        // The substitution ends at ITS matching `)`. An unbalanced `)` inside
        // quotes would end it early and could only ADD sites (the tail is then
        // rescanned), which reddens the count rather than hiding a site — the
        // fail-safe direction for a scan whose job is to notice new ones.
        let mut depth = 0usize;
        let mut close = line.len();
        for (k, c) in line[open..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = open + k;
                        break;
                    }
                }
                _ => {}
            }
        }
        if is_tr_pipeline(&line[open..close]) {
            let var: String = line[..eq]
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<Vec<char>>()
                .into_iter()
                .rev()
                .collect();
            if !var.is_empty() {
                vars.push(var);
            }
        }
        from = eq + 3;
    }
    vars
}

/// #564 O3 — the scan that drives the completeness pin, pinned itself.
///
/// The pin's whole value is that its count is exact, so what the scan counts has
/// to be what a reader would call a normalizer site. `|| true` was counted as
/// one (it only escaped the site tally because the audit line it appears on has
/// no `=$(` in it as well), and a known-false-positive scan is a scan someone
/// later relaxes to silence it.
#[test]
fn the_tr_site_scan_reads_pipelines_and_not_shell_operators() {
    // Every shape the pin claims to see, including a real site from each shim.
    for (line, want) in [
        (r#"  path_low=$(printf '%s' "$a_path" | tr '[:upper:]' '[:lower:]')"#, "path_low"),
        (r#"  path_low=$(printf '%s' "$a_path" |tr '[:upper:]' '[:lower:]')"#, "path_low"),
        (r#"  path_low=$(printf '%s' "$a_path" |  tr '[:upper:]' '[:lower:]')"#, "path_low"),
        (r#"safe=$(printf '%s' "$tag" | tr -c 'A-Za-z0-9._-' '_')"#, "safe"),
        // The plainest possible `tr` call, whose next token starts with a LETTER.
        // No shipped site is written this way, which is exactly why it belongs
        // here: the first fix for the `|| true` false positive squashed the
        // whitespace out before demanding a word boundary, and silently stopped
        // seeing this shape without moving the count (#564 rev-1).
        (r#"  low=$(printf '%s' "$x" | tr A a)"#, "low"),
        (r#"  low=$(printf '%s' "$x" |tr A a)"#, "low"),
    ] {
        assert_eq!(
            tr_pipeline_assignment_vars(line),
            vec![want.to_string()],
            "a `tr` pipeline assignment must be seen as a site: {line}"
        );
    }
    let none: Vec<String> = Vec::new();
    // THE O3 CASE: a shell OR, on a line that also assigns from a substitution.
    // Squashed it contains `|tr` — inside `||true` — and it is not a site.
    assert_eq!(
        tr_pipeline_assignment_vars(r#"  ts=$(date +%s) >> "$f" 2>/dev/null || true"#),
        none,
        "`|| true` is a shell OR, not a `tr` pipeline (#564 O3)"
    );
    // Nor is any other word that merely starts with `tr`.
    assert_eq!(
        tr_pipeline_assignment_vars(r#"  x=$(cat f | truncate-tool)"#),
        none,
        "`|truncate-tool` is not `| tr`"
    );
    // A comment quoting the shape (the #509 preamble does) is not a site.
    assert_eq!(
        tr_pipeline_assignment_vars(r#"  # Before #509: `x=$(printf … | tr …)` set x EMPTY"#),
        none,
        "a comment is not a site"
    );
    // A pipeline with nothing assigned is not a site either.
    assert_eq!(tr_pipeline_assignment_vars(r#"  printf '%s' "$x" | tr a b"#), none);
}

/// **#564 rev-1 B2 — one line can hold more than one normalizer, and the scan
/// has to see all of them.**
///
/// The first cut took the FIRST `=$(` on the line and asked whether the LINE
/// contained a `tr` pipeline anywhere. Both halves of that are wrong in the same
/// direction: a second normalizer appended beside an existing site is neither
/// counted nor guard-checked, while the exact count — the property that is
/// supposed to catch "a site was ADDED without a decision" — stays correct,
/// because the line was already counted once. That is O1's shape hiding inside
/// the shape the scan claims to recognise, and the shim already writes lines like
/// it (the two exempted `rest=…; rest=$(… | tr …); rtag=…` arms).
#[test]
fn the_tr_site_scan_sees_every_assignment_on_a_line() {
    // The B2 shape: a guarded site with a second, unguarded normalizer appended.
    // Both must come back — the caller checks each one for a guard separately.
    assert_eq!(
        tr_pipeline_assignment_vars(
            r#"  path_low=$(printf '%s' "$a_path" | tr '[:upper:]' '[:lower:]'); mth=$(printf '%s' "$a_method" | tr '[:upper:]' '[:lower:]')"#
        ),
        vec!["path_low".to_string(), "mth".to_string()],
        "a second normalizer on the same line is a second site (#564 rev-1 B2)"
    );
    // Each substitution is judged on its OWN contents: a `tr` pipeline in the
    // second one must not be attributed to the first one's variable, which is
    // what a whole-line match did.
    assert_eq!(
        tr_pipeline_assignment_vars(r#"  n=$(date +%s); low=$(printf '%s' "$x" | tr A a)"#),
        vec!["low".to_string()],
        "the `tr` belongs to the substitution it is IN, not to the first assignment on the line"
    );
    // …and the mirror: a `tr` in the first, something else in the second.
    assert_eq!(
        tr_pipeline_assignment_vars(r#"  low=$(printf '%s' "$x" | tr A a); n=$(date +%s)"#),
        vec!["low".to_string()]
    );
    // The real exempted shape — a `${…}` expansion (`=${`, not `=$(`) before the
    // substitution, and `)` characters after it — still yields exactly one site.
    assert_eq!(
        tr_pipeline_assignment_vars(
            r#"          *tagName:*)   rest=${a_query#*tagName:}; rest=$(printf '%s' "$rest" | tr -d ' "'); rtag=${rest%%,*}; rtag=${rtag%%\}*}; rtag=${rtag%%)*} ;;"#
        ),
        vec!["rest".to_string()],
        "the exempted extraction arm is one site, not zero and not two"
    );
}

/// **rev-32 NB2 / rev-36 NBa+NBb — nothing enforced guard COMPLETENESS, which is
/// what let NB1 through.** NB1 was one unguarded fail-open normalizer (`ql`)
/// sitting unnoticed among eleven `tr` sites, reachable only because a different
/// guard happened to exit first. A human found it by reading; that is not a
/// mechanism.
///
/// So: enumerate every `VAR=$(… | tr …)` in both generated shims and require each
/// to be guarded within a few lines, or named in `UNGUARDED_TR_SITES` with its
/// reason. Four properties make the pin itself trustworthy, each added because
/// the previous cut quietly lacked it:
///
/// 1. **Exemptions are keyed by site, not by name** (NBa) — see the list above.
/// 2. **The site count is asserted exactly** (NBb). `sites >= 1` only caught the
///    scan dying completely. An exact count also catches the scan going
///    half-blind: a site written `|tr` with no space, or split across two lines,
///    drops out of the count and reddens here instead of silently ceasing to be
///    checked. It catches a site ADDED without a decision **to the extent the
///    scanner sees it** — which is every `=$(… | tr …)` on a line, since #564
///    rev-1 B2, but still one LINE at a time. A `tr` site split across two lines
///    is invisible AND leaves the count correct; so is any normalizer not built
///    from `tr`. Neither is a hypothetical the count covers — the paragraph
///    below names where that class is actually caught.
/// 3. **The pipe match tolerates whitespace but respects word boundaries** —
///    `| tr`, `|tr`, `|  tr` all count so property 2 is a tripwire for a
///    genuinely new shape rather than for formatting, while `|| true` and
///    `| truncate` do not (#564 O3, `is_tr_pipeline`).
/// 4. **Each exempted site's line is pinned verbatim** (#564 O2). Properties
///    1–2 catch an exemption that drifts off its site; none of them catches the
///    site being rewritten UNDERNEATH a still-matching exemption.
///
/// **What this pin does NOT prove, stated because a pin that overclaims is worse
/// than one that doesn't exist** (#552, and #564 O1): it proves *every site it
/// can see is guarded*, never *every site is one it can see*. It is a text scan
/// for one shape — a normalizer written another way (`sed`, backticks, an
/// assignment split across lines) is invisible to it AND leaves its count
/// correct, so both halves pass. That residual is why
/// `no_single_broken_text_tool_can_let_a_gated_command_through` exists: it asks
/// the question behaviourally, of the running shim, where the shape a normalizer
/// is written in does not matter.
#[test]
fn every_shim_normalizer_is_guarded_or_explicitly_exempted() {
    let paths =
        ShimPaths { utils_dir: Some("/c/git/usr/bin".into()), git_dir: Some("/c/git/cmd".into()) };
    // The expected counts are part of the contract, not a snapshot: changing one
    // is how you record "I added/removed a normalizer on purpose".
    let shims = [
        // 11 → 12 with #565's `b_norm` (the PR body, canonicalized for the
        // `also: body-unchanged` digest) — a deliberate addition, and guarded.
        // 12 → 13 with #437's `_tag` (the tag a release id resolves to, CR-
        // stripped before it is matched against a grant) — also deliberate, also
        // guarded: an empty result there would key the grant lookup on nothing.
        // 13 → 14 with #2168 E2's `loomux_verdict_line5`, the one helper that
        // reproduces `parse_verdict_file`'s line-5 split and `sanitize_digest`
        // for every site that reads a verdict's digest. Its one `tr` is the
        // case fold, and it is GUARDED rather than exempted: an empty result
        // there feeds the digest comparison the `body-unchanged` clause decides
        // on, so #509's "empty matches every pattern" is exactly the hazard, not
        // an exception to it. The CR strip briefly added a second site and no
        // longer does — `str::lines()` drops one TRAILING `\r` and keeps the
        // rest, which is a parameter expansion, not a normalizer.
        // #2985 added five: the close gate's audit-only sanitisers, which delete
        // `"` and `\` from the values it writes into audit.jsonl so a git ref name
        // cannot forge a field in the row. All five are EXEMPT rather than guarded,
        // and the list above carries the reason per site: they feed the audit, never
        // the decision.
        ("gh", gh_shim_sh("C:/gh.exe", &paths), 19usize),
        ("git", git_shim_sh("C:/git.exe", &paths), 1usize),
    ];
    let mut all_sites: Vec<(String, String)> = Vec::new();
    for (name, script, expected) in &shims {
        let lines: Vec<&str> = script.lines().collect();
        let mut sites = 0usize;
        for (i, line) in lines.iter().enumerate() {
            // One scanner, pinned separately (`tr_pipeline_assignment_vars` /
            // `the_tr_site_scan_reads_pipelines_and_not_shell_operators` +
            // `…_sees_every_assignment_on_a_line`). A line can hold more than one
            // site, and each is counted and checked on its own (#564 rev-1 B2).
            for var in tr_pipeline_assignment_vars(line) {
                sites += 1;
                all_sites.push((var.clone(), (*line).to_string()));
                // `path_low`'s guard sits two lines below it, after `ref_low`.
                let guarded = lines[i..(i + 4).min(lines.len())]
                    .iter()
                    .any(|l| l.contains("loomux_norm_guard") && l.contains(&format!("\"${var}\"")));
                let exempt = UNGUARDED_TR_SITES
                    .iter()
                    .any(|(v, ctx, _, _)| *v == var.as_str() && line.contains(*ctx));
                assert!(
                    guarded || exempt,
                    "{name} shim line {}: `{var}` is normalized through `tr` but is neither guarded                  by loomux_norm_guard nor listed in UNGUARDED_TR_SITES with a reason. An                  unguarded normalizer returns EMPTY when the tool fails, and empty matches no                  gate pattern — that is #509. Guard it, or add it WITH a context substring                  identifying this site and the reason its emptiness is safe.
  {}",
                    i + 1,
                    line.trim()
                );
            }
        }
        assert_eq!(
            sites, *expected,
            "{name} shim: expected {expected} `| tr` sites, scanned {sites}. If you added or              removed a normalizer, update the count deliberately. If you did not, the SCAN has              gone blind to one — and knowing HOW it can go blind is part of the contract: it              reads one LINE at a time, so an assignment split across two lines is invisible to              it (every `=$(…)` WITHIN a line is seen, since #564 rev-1 B2). It is also blind to              any normalizer not built from `tr`; that class is covered behaviourally instead, by              `no_single_broken_text_tool_can_let_a_gated_command_through`."
        );
    }
    // NBa: every exemption must identify EXACTLY ONE real site. Zero means the
    // entry is stale and would sit there waiting to cover an unrelated future
    // site; more than one means the context is not specific enough to be an
    // identity, which is the blanket-permission failure in a different dress.
    for (var, ctx, pinned, reason) in UNGUARDED_TR_SITES {
        let hits: Vec<&String> = all_sites
            .iter()
            .filter(|(v, l)| v.as_str() == *var && l.contains(*ctx))
            .map(|(_, l)| l)
            .collect();
        assert_eq!(
            hits.len(), 1,
            "UNGUARDED_TR_SITES entry ({var:?}, {ctx:?}) matches {} `| tr` sites; it must match              exactly one. Zero: the site is gone — delete the exemption rather than leaving it              to cover something else later. More than one: the context is a pattern, not a site              identity, and is exempting more than it names.",
            hits.len()
        );
        // #564 O2: …and it must still be the SAME line. An exemption records a
        // reason decided about specific code; rewriting that code in place, with
        // the same variable and the same context, leaves the count and the
        // context match untouched while the reason quietly stops being true of
        // what it now exempts.
        assert_eq!(
            hits[0].trim(), *pinned,
            "UNGUARDED_TR_SITES entry ({var:?}, {ctx:?}) still matches exactly one site, but that              site has been REWRITTEN. Its exemption reason — {reason} — was decided about the              pinned line, not this one. Re-read the reason against the new line and decide again:              if emptiness is still safe here, update the pinned text WITH the reason; if it is              not, guard the site instead. Do not update the text alone to get back to green."
        );
    }
}

/// The external text tools a normalizer in these shims could plausibly be
/// written with (#564 O1). The first seven are the ones the dependency preamble
/// asserts today (`SHIM_DEP_TOOLS`); the rest are what a future normalizer is
/// most likely to reach for instead.
///
/// Sabotaging a tool nothing uses is a no-op today, and that is the point: the
/// day something *does* use one in front of a gate decision without a guard, the
/// sweep below is what says so — whatever shape the line is written in, and
/// whether or not anybody remembered to add the tool to the preamble's list.
///
/// `grep` earns its place ahead of the rest of that tier (#564 rev-1 N2): a gate
/// is a MATCHER, and `grep` is what a shell author reaches for the moment `case`
/// stops being expressive enough — so it is the likeliest tool for a future gate
/// decision to consume. `wc`, `sort`, `uniq`, `basename` and `dirname` are the
/// next tier and are deliberately NOT here, on RELEVANCE rather than cost: none
/// of them is a plausible matcher. Add one when a normalizer plausibly reaches
/// for it, not pre-emptively. (Cost is not the reason: adding `grep` and a sixth
/// shape together — 18 more shim invocations — moved the windows suite 86.51s →
/// 86.74s, which is inside run-to-run variance. libtest runs tests in parallel,
/// so the sweep's marginal cost is not its invocation count times anything.)
const CANDIDATE_NORMALIZER_TOOLS: &[&str] = &[
    "tr", "head", "tail", "date", "cat", "rm", "mv", "sed", "awk", "cut", "rev", "expr", "grep",
];

/// **#564 O1 — the completeness question asked BEHAVIOURALLY, of the running
/// shim.**
///
/// `every_shim_normalizer_is_guarded_or_explicitly_exempted` is a text scan for
/// one shape, so it proves "every site I can see is guarded" and never "every
/// site is one I can see": a normalizer written with `sed`, in backticks, or
/// split across two lines is invisible to it *and* leaves its exact site count
/// correct, so both halves of that pin pass while an unguarded fail-open site
/// sits in the gate. #509 was exactly that failure — an empty normalization in
/// front of a gate pattern — and shape is not what makes it dangerous.
///
/// So ask the question the way the bug arrives: break ONE text tool at a time —
/// resolvable, so the startup `command -v` probe is satisfied, and silently
/// producing nothing, which is what a corrupt install, an arch mismatch or a
/// fork failure looks like at the call site — and require every gated command to
/// still be REFUSED and never to reach the real binary. A new normalizer that
/// consumes a broken tool's empty output in front of a gate decision reddens
/// this test whatever it looks like in the source.
///
/// Arms on every platform: it bakes its own `utils_dir` holding the stub, so the
/// shim's own PATH repair puts it first (same technique as
/// `gh_shim_refuses_when_tr_resolves_but_cannot_run`, which pins the single-tool
/// `tr` case in depth — this generalizes it across tools and gated shapes).
///
/// **What it still does not prove**, since a pin that overclaims is the thing
/// #552 is about: it enumerates COMMAND SHAPES and TOOLS, so a normalizer on a
/// code path no shape below reaches, or one built from a tool not listed above,
/// is outside it. It is a **different** net from the text scan — neither one
/// contains the other, and neither is complete; both are kept because they are
/// blind to different things (#564 rev-1 B1). An unguarded site inside a code
/// path no shape below reaches reddens the scan and NOT this test; a normalizer
/// in a shape the scan cannot see reddens this test and NOT the scan. Reading
/// either as subsuming the other is how one of them gets deleted in a tidy-up.
/// What no listed shape can hide from it is the class that matters: a gated
/// command reaching the real binary because something in front of the gate
/// normalized to nothing.
///
/// Concretely unswept today, so nobody has to re-derive it: the `also: ci-green`
/// condition, the malformed-gate arms, and the `--input`-body ref parse. The
/// group fixture below deliberately covers the workflow-verdict gate itself
/// (#564 rev-1 N1) — the highest-value gate in the file, and the home of the
/// exempted `cur_head` normalizer, whose exemption reason is a claim about
/// behaviour and so belongs here rather than only in a comment.
#[test]
fn no_single_broken_text_tool_can_let_a_gated_command_through() {
    use std::process::Command;
    let Some(sh) = any_posix_sh() else {
        pin_could_not_arm("no_single_broken_text_tool_can_let_a_gated_command_through", "no POSIX sh on this host");
        return;
    };
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let group = root.join("group");
    std::fs::create_dir_all(&group).unwrap();

    // ── The group fixture. An EMPTY group dir left the highest-value gate in the
    // file entirely unswept (#564 rev-1 N1): the workflow-verdict gate is behind
    // `[ -f $LOOMUX_GROUP_DIR/merge_gate ]`, so with no such file none of these
    // invocations entered it — and `cur_head`, one of the three exempted
    // normalizers, lives inside it. Its exemption's stated reason is that the
    // `[ -n "$cur_head" ] ||` on the very next line blocks; that is a claim about
    // BEHAVIOUR, so the sweep should be the thing holding it.
    //
    // `autonomous` + `auto_merge` blanket-open the HUMAN gate deliberately, so
    // that for `gh pr merge` the workflow gate is the only thing left between the
    // command and the real gh. Without that the merge is refused by the human
    // gate whatever the verdicts say, and the sweep would be pinning the wrong
    // refusal. (The release/tag shapes are unaffected: they need `auto_release`.)
    for marker in ["autonomous", "auto_merge"] {
        std::fs::write(group.join(marker), b"").unwrap();
    }
    std::fs::write(group.join("merge_gate"), "require all-pass\nreviewer rev-1\n").unwrap();
    // A PASS whose head line is EMPTY — a hand-written or truncated verdict file.
    // With every tool working this is STALE (it cannot cover the current head) and
    // the merge is refused. It is chosen because it is the shape that turns a
    // failed `cur_head` normalization into a bypass: an empty `cur_head` compares
    // equal to an empty recorded head, and the stale verdict silently becomes a
    // pass for any revision — #197's failure class, reached through #509's.
    std::fs::create_dir_all(group.join("verdicts/pr-1")).unwrap();
    std::fs::write(group.join("verdicts/pr-1/rev-1"), "pass\n\n").unwrap();

    // Fake gh: as `write_fake_gh`, plus an answer for the workflow gate's
    // `--json headRefOid` lookup, which that shared fixture has no notion of.
    let log = root.join("gh.log");
    let fake_gh = root.join("fakegh_sweep");
    std::fs::write(&fake_gh, format!(
        "#!/bin/sh\n\
         echo \"ARGS: $*\" >> \"{log}\"\n\
         if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then\n\
        \x20 for a in \"$@\"; do case \"$a\" in *headRefOid*) printf '%s\\n' \"$FAKE_HEAD\"; exit 0 ;; esac; done\n\
        \x20 printf '%s %s\\n' \"$FAKE_BASE\" \"$FAKE_NUM\"; exit 0\n\
         fi\n\
         if [ \"$1\" = \"repo\" ] && [ \"$2\" = \"view\" ]; then printf '%s\\n' \"$FAKE_DEFAULT\"; exit 0; fi\n\
         printf 'FAKE-GH-RAN\\n'; exit 0\n",
        log = log.display()
    )).unwrap();
    // Fake git: confirms no bare v* as a tag (every gated shape below names
    // `refs/tags/…` outright), and announces itself for anything else.
    let fake_git = root.join("fakegit_sweep");
    std::fs::write(&fake_git,
        "#!/bin/sh\nif [ \"$1\" = \"rev-parse\" ]; then exit 1; fi\nprintf 'FAKE-GIT-RAN\\n'; exit 0\n").unwrap();
    let chmod = |p: &std::path::Path| {
        let _ = Command::new(&sh).arg("-c").arg(format!("chmod +x '{}'", p.display())).status();
    };
    chmod(&fake_gh);
    chmod(&fake_git);

    // A shim pair built against `utils` — the directory the generated script
    // prepends to PATH, and so the one whose contents win every tool lookup.
    let build = |label: &str, utils: &std::path::Path| -> (std::path::PathBuf, std::path::PathBuf) {
        let paths = ShimPaths { utils_dir: Some(msys_dir_for_fixture(utils)), git_dir: None };
        let gh = root.join(format!("gh_{label}"));
        std::fs::write(&gh, gh_shim_sh(&fake_gh.display().to_string(), &paths)).unwrap();
        let git = root.join(format!("git_{label}"));
        std::fs::write(&git, git_shim_sh(&fake_git.display().to_string(), &paths)).unwrap();
        chmod(&gh);
        chmod(&git);
        (gh, git)
    };
    // (success, stdout, stderr). The real binary announces itself on STDOUT —
    // that, not the exit code, is what says a gated command actually ran.
    let run = |shim: &std::path::Path, argv: &[&str]| -> (bool, String, String) {
        let out = Command::new(&sh).arg(shim).args(argv)
            .env("LOOMUX_GROUP_DIR", &group)
            .env("FAKE_BASE", "main").env("FAKE_DEFAULT", "main").env("FAKE_NUM", "1")
            .env("FAKE_HEAD", "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c")
            .output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };

    // The gated shapes swept per tool. Each is a publish-to-the-world or a merge,
    // and each reaches its decision through a different part of the shim. The
    // graphql query uses a VARIABLE rather than an inline string literal for the
    // measured quoting reason in `gh_shim_gates_the_api_shapes_…` (an argument
    // carrying a `"` is truncated on the way into MSYS `sh.exe`).
    let gated_gh: [(&[&str], &str); 5] = [
        (&["api", "-X", "DELETE", "repos/o/r/git/refs/tags/v1.2.3"], "DELETE of a published tag ref"),
        (&["api", "-X", "POST", "repos/o/r/releases", "-f", "tag_name=v9.9.9"], "POST of a release"),
        (&["api", "graphql", "-f", "query=mutation M($t:String!){createRelease(input:{tagName:$t}){clientMutationId}}"], "graphql createRelease"),
        (&["api", "-X", "PUT", "repos/o/r/pulls/1/merge"], "REST merge of a PR"),
        // #564 rev-1 N1. NOT the REST shape above, which is refused earlier as
        // `api-merge` and never reaches the workflow gate: only `gh pr merge`
        // resolves the base/number and then enters the `merge_gate` block, where
        // the exempted `cur_head` normalizer lives. The human gate is blanket-open
        // here (see the fixture), so a refusal of this shape IS the workflow gate.
        (&["pr", "merge", "1"], "merge of a PR under an unsatisfied workflow gate"),
    ];
    let gated_git: [(&[&str], &str); 1] =
        [(&["push", "origin", "refs/tags/v1.2.3"], "push of a release tag")];

    // ── The control: nothing sabotaged (an EMPTY utils dir, so every tool
    // resolves from the inherited PATH as usual). It has to establish two things
    // before the sweep means anything — that these shapes are refused HERE too
    // (so the sweep is not asserting a refusal that has nothing to do with the
    // sabotage), and that the shim otherwise WORKS (so a sweep run refusing
    // everything is a decision, not a broken harness).
    let control_utils = root.join("utils_control");
    std::fs::create_dir_all(&control_utils).unwrap();
    let (gh_ctl, git_ctl) = build("control", &control_utils);
    let (ok, out, err) = run(&gh_ctl, &["api", "repos/o/r"]);
    if !ok || !out.contains("FAKE-GH-RAN") {
        // The host's own PATH cannot even run the shim's dependency check — the
        // sweep would then "pass" by refusing everything for the wrong reason.
        pin_could_not_arm(
            "no_single_broken_text_tool_can_let_a_gated_command_through",
            &format!("this host's PATH cannot satisfy the shim's own dependencies: [stdout] {out} [stderr] {err}"),
        );
        return;
    }
    let (ok, out, _) = run(&git_ctl, &["push", "origin", "main"]);
    assert!(ok && out.contains("FAKE-GIT-RAN"), "control: an ordinary branch push must pass through, got: {out}");
    for (argv, what) in gated_gh.iter().chain(gated_git.iter()) {
        let shim = if argv[0] == "push" { &git_ctl } else { &gh_ctl };
        let (ok, out, err) = run(shim, argv);
        assert!(!ok && !out.contains("FAKE-"),
            "control: {what} must be refused with every tool intact — [stdout] {out} [stderr] {err}");
    }

    // ── The sweep.
    for tool in CANDIDATE_NORMALIZER_TOOLS {
        let utils = root.join(format!("utils_{tool}"));
        std::fs::create_dir_all(&utils).unwrap();
        // Resolvable (so `command -v` is satisfied and the startup probe is NOT
        // what refuses) and silently empty (exit 0, no output) — the harder half
        // of #509's condition, since nothing is printed to hint at it.
        let stub = utils.join(tool);
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
        chmod(&stub);
        let (gh, git) = build(tool, &utils);
        for (argv, what) in gated_gh.iter().chain(gated_git.iter()) {
            let is_git = argv[0] == "push";
            let shim = if is_git { &git } else { &gh };
            let ran = if is_git { "FAKE-GIT-RAN" } else { "FAKE-GH-RAN" };
            let (ok, out, err) = run(shim, argv);
            assert!(!out.contains(ran),
                "with `{tool}` broken, {what} REACHED the real binary — a normalizer in front of \
                 this gate consumed an empty result (#509/#564 O1). [stdout] {out} [stderr] {err}");
            assert!(!ok,
                "with `{tool}` broken, {what} must be refused — [stdout] {out} [stderr] {err}");
            // …and refused BY THE GATE. Without this the sweep would pass just as
            // happily on a shim that died of a shell error, which proves nothing.
            assert!(err.contains("orrerix:"),
                "with `{tool}` broken, {what} was refused but not by the gate (no gate refusal \
                 on stderr) — [stdout] {out} [stderr] {err}");
        }
    }
}

/// #509: the dependency preamble is one gate-integrity guarantee, not two
/// copies that can drift. Same reasoning as the grant-fragment pin above — the
/// gh and git shims are separate generated scripts with no shared shell lib, so
/// a tool dropped from one list and not the other would silently leave one shim
/// able to run with a normalizer it does not have.
#[test]
fn gh_and_git_shim_deps_preamble_stays_byte_identical() {
    let paths = ShimPaths { utils_dir: Some("/c/git/usr/bin".into()), git_dir: Some("/c/git/cmd".into()) };
    let gh = gh_shim_sh("C:/gh.exe", &paths);
    let git = git_shim_sh("C:/git.exe", &paths);
    let slice = |s: &str| -> String {
        let start = s.find("# ── The gate's own toolchain").expect("preamble present");
        let end = start + s[start..].find("\nunset _dep\n").expect("preamble terminator") + "\nunset _dep\n".len();
        s[start..end].to_string()
    };
    assert_eq!(slice(&gh), slice(&git), "the #509 dependency preamble has drifted between the two shims");
    // And it really does both halves: repair, then assert.
    let p = slice(&gh);
    assert!(p.contains("PATH=\"$ORX_UTILS:$PATH\""), "prepends the resolved coreutils dir");
    // rev-21 N5: match the LIST LINE, not the preamble text. `p.contains(" tr ")`
    // was also satisfied by the prose above it, so a tool named only in a comment
    // would have passed — the assertion would not have caught the drift it exists
    // to catch. Compare the actual `for _dep in …` operand, exactly and in order.
    let dep_line = p.lines().find(|l| l.trim_start().starts_with("for _dep in "))
        .expect("the self-check's dependency list line");
    let listed: Vec<&str> = dep_line
        .trim()
        .trim_start_matches("for _dep in ")
        .trim_end_matches("; do")
        .split_whitespace()
        .collect();
    assert_eq!(listed, SHIM_DEP_TOOLS.to_vec(),
        "the shim's dependency list must be exactly SHIM_DEP_TOOLS; got {listed:?}");
    assert!(p.contains("exit 1"), "a missing dependency must fail CLOSED");
    // With nothing resolvable at write time there is no repair to emit — but the
    // assertion still ships, so the shim refuses rather than half-normalizing.
    let bare = gh_shim_sh("C:/gh.exe", &ShimPaths::default());
    assert!(!bare.contains("LOOMUX_UTILS="), "no coreutils dir resolved → nothing to prepend");
    assert!(bare.contains("gate-degraded-missing-dep"), "the self-check ships regardless");
}

/// #509: the gh shim only re-points `git` for gh's BUILT-IN commands. An alias
/// (`gh myalias`, which for a `!`-alias runs a shell) and `gh extension …` run
/// agent-authored programs, and those must keep the gated `git` on PATH. This
/// pins the property structurally; the behavioral half is
/// `gh_shim_gives_the_real_gh_a_native_git_only_for_builtin_subcommands`.
#[test]
fn gh_shim_git_plumbing_case_lists_only_builtin_subcommands() {
    let sh = gh_shim_sh("C:/gh.exe", &ShimPaths { utils_dir: None, git_dir: Some("/c/git/cmd".into()) });
    let arm = sh.lines().find(|l| l.contains("PATH=\"/c/git/cmd:$PATH\""))
        .expect("the git-plumbing arm is emitted when a git dir resolved");
    for builtin in ["pr", "repo", "issue", "release", "api"] {
        assert!(arm.contains(builtin), "{builtin} is a gh built-in that shells out to git: {arm}");
    }
    for never in ["extension", "ext", "alias"] {
        assert!(!arm.split('|').any(|t| t.trim_matches(|c: char| !c.is_ascii_alphanumeric()) == never),
            "'{never}' must NOT re-point git — it runs agent-authored code: {arm}");
    }
    // No git dir resolved → no arm at all, rather than a `PATH=":$PATH"` that
    // would silently put the current directory first.
    let none = gh_shim_sh("C:/gh.exe", &ShimPaths::default());
    assert!(!none.contains("The real gh's own git plumbing"), "nothing to prepend → nothing emitted");
}

#[test]
fn gh_shim_script_bakes_real_gh_and_enforces_the_guards() {
    // The security-critical shim: pin that it bakes the real gh path, gates only
    // merges, checks BOTH markers, fails safe on an unverifiable base, and audits.
    let sh = gh_shim_sh("C:/Program Files/GitHub CLI/gh.exe", &shim_paths());
    assert!(sh.contains("REAL_GH=\"C:/Program Files/GitHub CLI/gh.exe\""), "bakes the real gh path");
    assert!(sh.starts_with("#!/bin/sh"), "POSIX shebang so Git Bash runs it");
    // Only merges are gated; everything else execs the real gh immediately.
    assert!(sh.contains("exec \"$REAL_GH\" \"$@\""), "non-merge passthrough");
    assert!(sh.contains("pr") && sh.contains("merge"), "detects gh pr merge");
    // Base determined via the REAL gh, compared to the default branch.
    assert!(sh.contains("baseRefName"), "resolves the PR base branch");
    assert!(sh.contains("defaultBranchRef"), "resolves the repo default branch");
    // BOTH markers required for a default-branch merge; fail-safe otherwise.
    assert!(sh.contains("$ORX_GD/autonomous") && sh.contains("$ORX_GD/auto_merge"),
        "checks both consent markers");
    assert!(sh.contains("unverifiable-base"), "fail-safe block on an undeterminable base");
    assert!(sh.contains("merge-gate-blocked") && sh.contains("audit.jsonl"), "audits refusals");
    assert!(!sh.contains("\r"), "the POSIX shim must be LF-only (a CRLF #!/bin/sh is broken)");
    // rev-79 F1/F2: the shim parses positionals around global flags and honors the
    // caller's -R/--repo when resolving base + default branch.
    assert!(sh.contains("--repo") && sh.contains("-R"), "recognizes -R/--repo global flag");
    assert!(sh.contains("rf=\"-R $repo\""), "passes the caller's repo through to the lookups");
    // #294: `pr view` accepts -R, but `gh repo view` takes the repo POSITIONALLY —
    // passing `-R` there is a hard `gh` error, not a no-op. Pin the two lookups use
    // DIFFERENT forms so this regression can't come back silently.
    assert!(sh.contains("pr view $rf"), "pr view honors -R");
    assert!(sh.contains("repo view $repo") && !sh.contains("repo view $rf"),
        "repo view takes the repo positionally, never via $rf (-R)");
}

#[test]
fn set_auto_merge_requires_autonomous_mode() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // Autonomous off → enabling auto-merge is REJECTED (the enforced dependency).
    let err = reg.set_auto_merge(&g.id, true).unwrap_err();
    assert!(err.to_lowercase().contains("autonomous"), "the rejection must name the dependency, got: {err}");
    assert!(!reg.is_auto_merge(&g.id), "auto-merge must not be enabled without autonomous mode");
    // With autonomous on, enabling works.
    reg.set_autonomous(&g.id, true).unwrap();
    reg.set_auto_merge(&g.id, true).unwrap();
    assert!(reg.is_auto_merge(&g.id));
}

#[test]
fn disabling_autonomous_force_disables_auto_merge() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let am_marker = reg.state_root().join(g.id.as_str()).join("auto_merge");
    reg.set_autonomous(&g.id, true).unwrap();
    reg.set_auto_merge(&g.id, true).unwrap();
    assert!(reg.is_auto_merge(&g.id) && am_marker.is_file());
    // Turning autonomous OFF must force auto-merge off too — the pair can never be
    // auto_merge-on/autonomous-off (the combo the enforced gate keys on).
    reg.set_autonomous(&g.id, false).unwrap();
    assert!(!reg.is_auto_merge(&g.id), "auto-merge must be force-disabled when autonomous turns off");
    assert!(!am_marker.is_file(), "the auto_merge marker must be removed");
    assert_eq!(audit_count(&reg, &g.id, "auto-merge-off"), 1, "the forced disable is audited");
    assert_eq!(s(reg.autonomy_state(&g.id)["auto_merge"].to_string().as_str()), "false");
}

#[test]
fn stale_auto_merge_without_autonomous_is_reconciled_on_read() {
    // Migration: a group dir carrying an `auto_merge` marker but no `autonomous`
    // marker (older group predating the dependency, or a hand-edited state dir)
    // must be reconciled OFF on the next load, so the enforced gate never sees the
    // forbidden combo.
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let gdir = reg.state_root().join(g.id.as_str());
    // Simulate the stale on-disk combo directly.
    std::fs::write(gdir.join("auto_merge"), b"").unwrap();
    assert!(!gdir.join("autonomous").is_file());
    // Reload the group in a fresh registry (restart) → reconcile.
    let reg2 = relaunch_registry(dir.path());
    reg2.set_port(45999);
    reg2.create_group("C:/tmp/repo", rails()).unwrap();
    assert!(!reg2.is_auto_merge(&g.id), "stale auto-merge must be reconciled off without autonomous");
    assert!(!gdir.join("auto_merge").is_file(), "the stale marker must be removed");
    assert_eq!(audit_count(&reg2, &g.id, "auto-merge-off"), 1, "the reconcile is audited");
}

#[test]
fn set_auto_release_mirrors_auto_merge_dependency_and_is_independent() {
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let am = reg.state_root().join(g.id.as_str()).join("auto_merge");
    let ar = reg.state_root().join(g.id.as_str()).join("auto_release");
    // Dependency: enabling auto-release without autonomous is rejected.
    let err = reg.set_auto_release(&g.id, true).unwrap_err();
    assert!(err.to_lowercase().contains("autonomous"), "must name the dependency, got: {err}");
    assert!(!reg.is_auto_release(&g.id));
    // With autonomous on, the two toggles are INDEPENDENT: auto_merge on + auto_release
    // off, and vice versa.
    reg.set_autonomous(&g.id, true).unwrap();
    reg.set_auto_merge(&g.id, true).unwrap();
    assert!(reg.is_auto_merge(&g.id) && !reg.is_auto_release(&g.id), "auto_merge on must not enable auto_release");
    assert!(am.is_file() && !ar.is_file());
    reg.set_auto_release(&g.id, true).unwrap();
    assert!(reg.is_auto_merge(&g.id) && reg.is_auto_release(&g.id));
    reg.set_auto_merge(&g.id, false).unwrap();
    assert!(!reg.is_auto_merge(&g.id) && reg.is_auto_release(&g.id), "disabling auto_merge must not touch auto_release");
    assert!(!am.is_file() && ar.is_file());
    assert_eq!(reg.autonomy_state(&g.id)["auto_release"].as_bool(), Some(true));
    // Turning autonomous OFF force-disables auto_release too (money-stop), audited.
    reg.set_autonomous(&g.id, false).unwrap();
    assert!(!reg.is_auto_release(&g.id), "autonomous-off must force-disable auto_release");
    assert!(!ar.is_file());
    assert_eq!(audit_count(&reg, &g.id, "auto-release-off"), 1);
    // Restart survival + stale reconcile: a stale auto_release without autonomous
    // is reconciled off on read.
    reg.set_autonomous(&g.id, true).unwrap();
    reg.set_auto_release(&g.id, true).unwrap();
    std::fs::remove_file(reg.state_root().join(g.id.as_str()).join("autonomous")).unwrap(); // hand-edit: drop autonomous, leave auto_release
    let reg2 = relaunch_registry(dir.path());
    reg2.set_port(45999);
    reg2.create_group("C:/tmp/repo", rails()).unwrap();
    assert!(!reg2.is_auto_release(&g.id), "stale auto_release without autonomous reconciled off");
    assert!(!reg2.state_root().join(g.id.as_str()).join("auto_release").is_file());
}

#[test]
fn budget_suspension_force_disables_auto_release() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.set_autonomous(&g.id, true).unwrap();
    reg.set_auto_release(&g.id, true).unwrap();
    assert!(reg.is_auto_release(&g.id));
    seed_usage(&reg, &g.id, "spend", 5_000);
    reg.set_autonomy_budget(&g.id, 100).unwrap();
    assert_eq!(reg.enforce_autonomy_budgets(now_ms()), vec![g.id.clone()]);
    assert!(!reg.is_autonomous(&g.id));
    assert!(!reg.is_auto_release(&g.id), "budget suspension must drop auto_release (gate closed)");
}
