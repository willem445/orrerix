//! Source-scanning guards: the quick drive never builds an external command
//! and never ends a pane, and this target's registry helper applies the
//! overrides its #464 allowlist row assumes.

use super::*;

// ── the never-merges scan ───────────────────────────────────────────────────

/// The files that ARE the quick drive, and the whole of it: the pure core, the
/// step and the interception, and the human-side lifecycle.
///
/// **A file scope rather than a name scope**, which is CLAUDE.md's
/// source-scanning-guard convention and `tests/reviewdrive/guards.rs`'s own
/// choice: a scope keyed on a name — an `qd_` prefix, a module — is stepped
/// over by a landing verb added in a function that does not carry it. A file
/// is not a name.
const QUICK_FILES: [&str; 3] = [
    "../crates/loomux-engine/src/quickdrive.rs",
    "src/orchestration/qdtick.rs",
    "src/orchestration/registry/quick.rs",
];

/// Registry capabilities the quick drive may never reach, matched as CALLS.
///
/// The review driver's own denylist, plus the ones that driver is ALLOWED and
/// this one is not: it may release a pane it is done with and enqueue a clean
/// PR; a quick run ends no pane and lands nothing, ever. Each row is
/// self-verifying — the identifier must still be defined under the
/// orchestration sources, so a rename fails here instead of disarming the row.
const FORBIDDEN_CALLS: [(&str, &str); 13] = [
    ("grant_merge", "a quick run never merges and never grants one"),
    ("queue_merge", "nor queues one"),
    ("queue_merge_with", "nor queues one through the gh-only runner the review driver may use"),
    ("record_verdict", "its reviewer's verdict travels by report, never as a gate-counted verdict file"),
    ("kill_agent", "the panes are left for the human — ending one is theirs to do"),
    ("kill_agent_as", "nor by the initiator-taking half"),
    ("mark_dead", "nor by the primitive under every kill"),
    ("reap_idle_agents", "nor by the reaper"),
    ("release_driven_pane", "the review driver's one permitted kill is not this driver's"),
    ("post_issue_comment", "a quick run needs no issue and writes to none"),
    ("upsert_task", "it has no board"),
    ("drive_review", "handing a run to the review driver is not in this build"),
    ("drive_plan", "nor to the plan driver"),
];

/// Ways of starting a process or reaching `gh`/`git`, matched as text. The
/// quick drive needs none of them: it reads a pane's status, types into a
/// pane, and writes files in its own group directory.
const FORBIDDEN_TEXT: [(&str, &str); 6] = [
    ("Command::new", "it starts no child process"),
    ("RdRunner", "it holds no gh runner"),
    ("MqRunner", "nor the merge queue's git-carrying one"),
    ("runner_for(", "nor builds one"),
    ("gh_capture", "nor shells out to gh by the capture helper"),
    ("run_git(", "nor to git"),
];

/// One quick-drive file as the scan reads it: production source only —
/// `#[cfg(test)]` onward and every line comment are cut, for
/// `tests/reviewdrive/guards.rs`'s two stated reasons (a test may legitimately
/// name a forbidden thing to prove it is refused, and the design is quoted at
/// length in `///` blocks). The comment cut is textual and is fooled by a `//`
/// inside a string literal on a line with an odd number of quotes before it;
/// the population floor below is what would notice if it started eating code.
fn production_source(rel: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let src = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    strip(&src)
}

fn strip(src: &str) -> String {
    let src = match src.find("\n#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    };
    src.lines()
        .map(|line| {
            let mut quotes = 0usize;
            let b: Vec<char> = line.chars().collect();
            for k in 0..b.len() {
                if b[k] == '"' && (k == 0 || b[k - 1] != '\\') {
                    quotes += 1;
                }
                if b[k] == '/' && b.get(k + 1) == Some(&'/') && quotes % 2 == 0 {
                    return b[..k].iter().collect::<String>();
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How many times `src` calls `ident` — the identifier immediately followed by
/// `(`, with no identifier character before it. A `contains` check is not
/// this: `killed_by` is not `kill_agent`, and `record_verdict_seen` is not
/// `record_verdict`.
fn count_calls(src: &str, ident: &str) -> usize {
    let needle = format!("{ident}(");
    let mut from = 0usize;
    let mut n = 0usize;
    while let Some(rel) = src[from..].find(&needle) {
        let at = from + rel;
        if at == 0
            || !src[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            n += 1;
        }
        from = at + needle.len();
    }
    n
}

/// Every bracketed list of string literals in `src`, as its ordered tokens —
/// the SHAPE an argv has, never the name of whatever built it.
fn argv_literals(src: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for opener in ["vec![", "&["] {
        let mut from = 0usize;
        while let Some(rel) = src[from..].find(opener) {
            let start = from + rel + opener.len();
            let Some(len) = src[start..].find(']') else { break };
            let toks: Vec<String> = src[start..start + len]
                .split('"')
                .skip(1)
                .step_by(2)
                .map(|s| s.to_string())
                .collect();
            if !toks.is_empty() {
                out.push(toks);
            }
            from = start + len;
        }
    }
    out
}

/// Whether an argv literal is an ask of `gh` or `git` — its first token is one
/// of their nouns or the program itself. The quick drive may make NONE, so
/// unlike the review driver's scan there is no allowlist to check it against.
fn is_external_ask(argv: &[String]) -> bool {
    matches!(
        argv.first().map(String::as_str),
        Some("pr" | "issue" | "api" | "release" | "repo" | "run" | "gh" | "git" | "push" | "merge" | "tag")
    )
}

/// Everything the scan finds wrong with `src`, as sentences.
fn findings(rel: &str, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for argv in argv_literals(src) {
        if is_external_ask(&argv) {
            out.push(format!("{rel}: builds an external command line {argv:?}"));
        }
    }
    for (bad, why) in FORBIDDEN_CALLS {
        if count_calls(src, bad) > 0 {
            out.push(format!("{rel}: calls {bad} — {why}"));
        }
    }
    for (bad, why) in FORBIDDEN_TEXT {
        if src.contains(bad) {
            out.push(format!("{rel}: names {bad} — {why}"));
        }
    }
    out
}

/// **The quick drive never builds a landing verb, never starts a process, and
/// never ends a pane.**
///
/// Stricter than the review driver's scan, and simpler for it: that driver
/// reads GitHub and may ask `gh` for two things, so its scan is default-deny
/// against an allowlist. This driver asks for nothing, so every external ask
/// is a finding.
///
/// # The residual, stated because a scan must state one
///
/// A landing verb reached through a helper in ANOTHER file is invisible here.
/// The three the quick drive does call across that line are the review
/// driver's delivery helpers (`rd_reuse_pane`, `rd_take_over_pane`,
/// `rd_spawn`), which type text into a pane or open one and are inside that
/// driver's own scan. What an AGENT does in its pane is not this scan's
/// subject at all: that is bounded by the role's containment and the `gh`
/// shim's human grant, and by the brief telling it never to merge.
#[test]
fn the_quick_drive_builds_no_external_command_and_ends_no_pane() {
    let mut all = Vec::new();
    let mut lines = 0usize;
    for rel in QUICK_FILES {
        let src = production_source(rel);
        lines += src.lines().count();
        all.extend(findings(rel, &src));
    }
    // The population control: a scan that read nothing reports clean, which is
    // byte-identical to one that found nothing forbidden. Each file is pinned
    // by a function it must hold, so a file that moved fails here loudly.
    assert!(lines > 1_500, "only {lines} production lines read across the quick drive's files");
    for (rel, marker) in [
        (QUICK_FILES[0], "pub fn decide("),
        (QUICK_FILES[1], "pub fn qd_drive_group("),
        (QUICK_FILES[2], "pub fn quick_start("),
    ] {
        assert!(production_source(rel).contains(marker), "{rel} no longer holds `{marker}`");
    }
    assert!(all.is_empty(), "quick-orchestration.md, \"what a run can never do\":\n  {}", all.join("\n  "));

    // Every denylist row must still name something that EXISTS, or it denies a
    // function that has been renamed and reports green while doing it.
    let orchestration = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/orchestration");
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let mut dirs = vec![orchestration];
    while let Some(d) = dirs.pop() {
        for path in std::fs::read_dir(&d).unwrap().flatten().map(|e| e.path()) {
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|x| x == "rs") {
                files.push(path);
            }
        }
    }
    let haystack: String =
        files.iter().map(|p| std::fs::read_to_string(p).unwrap_or_default()).collect::<Vec<_>>().join("\n");
    for (bad, _) in FORBIDDEN_CALLS {
        assert!(
            haystack.contains(&format!("fn {bad}(")),
            "the denylist row for `{bad}` names nothing that exists any more — it was probably \
             renamed, and this row has been denying nothing since"
        );
    }
}

/// The positive control, and the one this guard most needs: **it reddens on a
/// planted landing verb, a planted process and a planted kill — and not on the
/// lookalikes the real source is full of.**
#[test]
fn the_scan_fires_on_a_planted_merge_and_not_on_a_lookalike() {
    // A quick drive that landed, shelled out and killed. One finding each.
    for (hostile, expect) in [
        (r##"fn land(pr: u64) -> Vec<String> { vec!["pr".into(), "merge".into(), pr.to_string()] }"##, "external command"),
        (r##"fn push() { let argv = &["git", "push", "origin", "HEAD"]; run(argv); }"##, "external command"),
        (r##"fn shell() { let _ = std::process::Command::new("gh").arg("pr").status(); }"##, "Command::new"),
        ("fn tidy(reg: &OrchRegistry) { let _ = reg.kill_agent(&pane); }", "kill_agent"),
        ("fn tidy(reg: &OrchRegistry) { let _ = reg.mark_dead(&pane, Some(0)); }", "mark_dead"),
        ("fn land(reg: &OrchRegistry) { reg.grant_merge(group, pr, \"human\"); }", "grant_merge"),
        ("fn board(reg: &OrchRegistry) { let _ = reg.upsert_task(g, a, None, patch); }", "upsert_task"),
    ] {
        let found = findings("planted.rs", &strip(hostile));
        assert!(
            found.iter().any(|f| f.contains(expect)),
            "the scan missed a planted violation ({expect}): {hostile}\n  found: {found:?}"
        );
    }

    // The lookalikes: what this driver's real source says and does.
    let benign = r##"
        let how = match a.killed_by { Some(who) => who.as_str(), None => "exited" };
        let _ = "Nothing was merged; its panes are still open";
        let _ = "there is no merge gate";
        render_template(QUICK_FIX_TPL, &[("WHY", &why), ("TASK", &task)]);
        const QUICK_ACTIONS: [&str; 5] = ["step", "stop", "resume", "handoff", "note"];
        self.qd_mem.lock_safe().working.remove(group);
        cur.record_verdict_seen(&block);
        // a comment may say gh pr merge, kill_agent(x) and Command::new freely
    "##;
    let found = findings("benign.rs", &strip(benign));
    assert!(found.is_empty(), "the scan fired on source that only LOOKS like a violation: {found:?}");
}

// ── #464: the registry helper ───────────────────────────────────────────────

/// The proof `tests/orchestration/guards.rs`'s allowlist row for this target
/// names (`no_registry_construction_bypasses_the_test_agent_dir_overrides`).
///
/// That row permits exactly one raw `OrchRegistry::new` in
/// `quickdrive/helpers.rs`, on the grounds that it is `relaunch_registry` and
/// that `relaunch_registry` redirects every generated-agent-file destination
/// away from the real `~/.claude` / `~/.copilot`. A helper that quietly
/// stopped applying one of those overrides would leave the row true about the
/// COUNT and false about the property.
#[test]
fn its_registry_helper_applies_every_override_this_allowlist_row_assumes() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/quickdrive/helpers.rs"),
    )
    .expect("the helpers file reads");
    let start = src
        .find("pub(crate) fn relaunch_registry(dir: &std::path::Path) -> OrchRegistry {")
        .expect("the sanctioned helper must exist, under the name the row names");
    let body = &src[start..];
    let end = body.find("\n}").expect("the helper must terminate") + 2;
    let body = &body[..end];
    for needed in [
        "set_claude_agents_dir_override",
        "set_copilot_agents_dir_override",
        "set_compact_hook_dir_override",
        "set_copilot_hooks_dir_override",
    ] {
        assert!(
            body.contains(needed),
            "the #464 allowlist row for tests/quickdrive/helpers.rs assumes this helper applies \
             every override; it no longer applies {needed}"
        );
    }
    // The population control: the extraction isolated the helper, so the four
    // assertions above are about ITS body and not about the whole file.
    assert!(body.len() < 1_200, "the helper's body extraction ran away ({} chars)", body.len());
}
