//! Source-scanning guards: §3.1's never-merges scan, the #464 helper proof, and #2509's one-writer grace scan.
//!
//! One module of the `reviewdrive` integration-test target (`main.rs`),
//! split out of the former single-file `tests/reviewdrive.rs` by #3498 P6
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── §3.1's never-merges scan ────────────────────────────────────────────────

/// The two files and one directory that are the review driver, and the whole of
/// it.
///
/// **A file scope rather than a name scope, and that is the point.** CLAUDE.md's
/// source-scanning-guard convention forbids deciding from a binding's name, and
/// the design note says so about this scan in particular: "any scope keyed on a
/// name — a module, an `rd_*` prefix — is stepped over by a landing verb added
/// in a function that does not carry it". A file is not a name; the driver's
/// registry wiring was moved into `rdtick.rs` precisely so this list could be
/// files rather than a prefix, and #3498 P5 split that file into the
/// `rdtick/` directory, which [`driver_production_source`] reads whole.
const DRIVER_FILES: [&str; 3] = [
    "../crates/loomux-engine/src/reviewdrive.rs",
    "../crates/loomux-engine/src/rddrive.rs",
    "src/orchestration/rdtick",
];

// ── §3.1's never-merges scan, decided on shape rather than on names ─────────

/// Every `gh` subcommand the driver is permitted to issue, with the reason each
/// is permitted — **default-deny**: an argv whose first two tokens are not on
/// this list fails, and a row that stops matching anything fails too.
///
/// This is the list that matters. §3.1 item 1 says the driver may never build a
/// merge or any other landing verb; item 3 adds the edit and relabel verbs. Both
/// are statements about **what it asks `gh` to do**, and the driver asks `gh` for
/// exactly one thing per argv — so enumerating the permitted asks denies every
/// forbidden one at once, including the ones nobody thought to name. A denylist
/// of verbs would have to anticipate `gh api`, `gh pr comment`, `gh pr close`
/// and whatever `gh` ships next; this does not.
const ALLOWED_ASKS: [(&str, &str, &str); 2] = [
    ("pr", "view", "§2.1: the PR's state, head, base, body, mergeability and size"),
    ("pr", "checks", "§2.1: the PR's checks, which is how CI green/red is learned"),
];

/// Registry capabilities the driver may never reach, matched as CALLS.
///
/// Each row is **self-verifying**: the identifier must still be defined
/// somewhere under the two source roots. A denylist row naming a function that
/// has since been renamed denies nothing while still reporting green, which is
/// the failure mode this whole guard is about — so a rename fails here loudly
/// instead of disarming the row.
const FORBIDDEN_CALLS: [(&str, &str); 7] = [
    ("grant_merge", "item 2: no barrier exists on that function; this is the barrier"),
    ("kill_agent", "item 5: the orchestrator's kill tool is not the driver's to call"),
    ("kill_agent_as", "item 5: nor its initiator-taking half, which #2501 does not widen"),
    ("mark_dead", "item 5: the primitive under every kill — reachable only via release_driven_pane"),
    ("reap_idle_agents", "item 5: the reaper is not the driver's to call"),
    ("record_verdict", "item 7: the driver reads verdicts and can never write one"),
    ("queue_merge", "§8.1: the runner-less form builds a real git-carrying runner — the driver's one enqueue is PERMITTED_ENQUEUE, through with_git_denied"),
];

/// **The ONE way the driver may enqueue a PR** (#3367 item 5), and the count of
/// its call sites — [`PERMITTED_RELEASE`]'s shape, for the same reason.
///
/// §8.1 used to say the driver never queues. Item 5 narrows it to the CLEAN
/// case — every required lane passed at the live head declaring
/// `open_findings: 0`, CI green, `merge_queue.enabled` — and the narrowing is a
/// capability rather than a licence: `queue_merge_with` re-enforces the gate
/// from the verdict files and the live PR (merge-queue.md §6), refuses the
/// default branch structurally (§7), and is handed the driver's `gh`-only
/// runner, so what may be queued is still the queue's to decide. A second call
/// site is a second place that narrowing can be broken, so the count is the pin.
///
/// What the count cannot see is WHEN it is called — that is pinned
/// behaviourally, by the clean-case tests below and their omitted-declaration
/// control, which must enqueue nothing.
const PERMITTED_ENQUEUE: (&str, &str, usize, &str) = (
    "queue_merge_with",
    "src/orchestration/rdtick",
    1,
    "#3367 item 5: the clean case's enqueue, through the queue's own gate re-check and with_git_denied",
);

/// **The ONE way the driver may end a pane's life** (#2501), and the count of
/// its call sites.
///
/// §3.1 item 5 used to be a closed sentence, and the rows above were the whole
/// of its enforcement. #2501 narrows it to a lane whose verdict is recorded at
/// the drive's current head and a worker whose `report` the drive has consumed;
/// #2811 S1 adds either of them at the step that ENDS the drive. The narrowing
/// is a capability rather than a licence: the rows above still deny every kill
/// primitive inside the driver's files, and this permits exactly one call to the
/// one barrier, which lives outside them (`OrchRegistry::release_driven_pane`,
/// in `registry/agents.rs`, beside `kill_agent_as` and `mark_dead`).
///
/// **The COUNT is the pin, not the presence.** A second call site is a second
/// place the release rule can be broken, and a scan that only asked "is it
/// called" would pass a driver that released a pane from anywhere in the tick.
/// So the site count is stated here and asserted, and a slice that genuinely
/// needs a second one argues it onto this row — which is the same discipline
/// `ALLOWED_ASKS` applies to a `gh` verb.
///
/// **What this scan does NOT enforce, stated because it is the interesting
/// half.** Which states may release is a property of a state machine, and no
/// source scan can see one. It is pinned behaviourally instead, by
/// `reviewdrive::releasable`'s own unit tests and by the integration tests in
/// this file that drive a real tick over a real registry and assert what
/// survives — including the negative controls, which are the ones that matter: a
/// briefed-but-silent lane, a stale verdict, a `blocked` worker and a parking
/// STEP all keep their panes — plus the positive counterpart, a `fail`-route
/// hand-back at the cap where the lane IS released and the drive does not park.
const PERMITTED_RELEASE: (&str, &str, usize, &str) = (
    "release_driven_pane",
    "src/orchestration/rdtick",
    1,
    "#2501/#2811 S1: §3.1 item 5's narrowed states, through the one barrier in mod.rs",
);

/// One driver file as the scan reads it: **production source only**.
///
/// Two things are removed, and each removal is a stated blind spot rather than a
/// convenience. `#[cfg(test)]` onward is cut, because a test in one of these
/// files may legitimately build a landing verb — one deliberately does, to prove
/// the `GitDenied` bridge REFUSES `git push`, and a scan that fired on it would
/// be a scan the fix for is to delete the proof. Line comments are cut, because
/// the design note is quoted at length in these files and a `///` block naming
/// `queue_merge` or a `gh pr merge` example is prose, not a capability.
///
/// The comment cut is textual and is fooled by a `//` inside a string literal on
/// a line with an odd number of quotes before it. No such line exists here, and
/// the population floor asserted in the scan is what would notice if the cut
/// ever started eating real code.
///
/// **A directory entry is read whole** (#3498 P5): every `.rs` under it,
/// recursively and in path order, each cut as above, joined into one source. So
/// `rdtick/` is one scope however it is divided — a file added to it is in scope
/// with no edit here, and every count below is the directory's, not a file's.
fn driver_production_source(rel: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    if p.is_dir() {
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        let mut dirs = vec![p.clone()];
        while let Some(d) = dirs.pop() {
            let entries = std::fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display()));
            for path in entries.flatten().map(|e| e.path()) {
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|x| x == "rs") {
                    files.push(path);
                }
            }
        }
        files.sort();
        assert!(!files.is_empty(), "{}: a driver directory holding no .rs file", p.display());
        return files.iter().map(|f| production_source_at(f)).collect::<Vec<_>>().join("\n");
    }
    production_source_at(&p)
}

/// [`driver_production_source`] for one file.
fn production_source_at(p: &std::path::Path) -> String {
    let src = std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let src = match src.find("\n#[cfg(test)]") {
        Some(i) => src[..i].to_string(),
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

/// Whether `src` names `ident` as a **call** — the identifier immediately
/// followed by `(`, with no identifier character before it.
///
/// **A `contains` check is not this, and the difference was a live defect.**
/// `DriveEntry::record_verdict_seen` records what the driver READ and cannot
/// write a verdict file; under a substring match it reported the driver as
/// writing verdicts. A guard that cannot tell a forbidden name from a longer
/// name starting with it enforces a prefix rather than the rule it states.
fn names_call(src: &str, ident: &str) -> bool {
    count_calls(src, ident) > 0
}

/// How many times `src` calls `ident` — [`names_call`]'s own rule, counted
/// rather than answered yes/no.
///
/// #2501 needs the number: the driver is permitted exactly one call to
/// `release_driven_pane` and a second one is a second place §3.1 item 5's
/// release rule can be broken, so a boolean would pass the thing the row is
/// written to catch. `names_call` delegates here rather than the two matching in
/// parallel — a guard and its counter that can disagree are two guards.
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

/// Every argv literal in `src`, as its ordered string tokens.
///
/// **The extraction is the guard**, so it is worth saying what it keys on: an
/// argv reaching `RdRunner::gh` is a list of string literals, and this collects
/// every `vec![…]` and `&[…]` list whose elements are string literals. It keys
/// on the SHAPE — a bracketed list of quoted tokens — and never on the name of
/// the function that builds it, which is the axis CLAUDE.md forbids deciding on
/// because a rename steps over it.
fn argv_literals(src: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for opener in ["vec![", "&["] {
        let mut from = 0usize;
        while let Some(rel) = src[from..].find(opener) {
            let start = from + rel + opener.len();
            let Some(len) = src[start..].find(']') else { break };
            let block = &src[start..start + len];
            let toks: Vec<String> = block
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

/// §3.1's enforcement, prescribed on this slice: **the driver may never build a
/// merge or any other landing verb** — plus items 2, 3, 5 and 7.
///
/// # Why this is default-deny on the ASK rather than a denylist of verbs
///
/// The driver reaches the outside world through exactly one method — `gh`, with
/// an arg vector — so every external action it can take is an argv, and every
/// argv's first two tokens are the ask. Enumerating what it MAY ask denies every
/// forbidden ask at once, including `gh api`, `gh pr comment`, `gh pr close` and
/// whatever `gh` ships next. A denylist would have to have anticipated each.
///
/// It is also the axis CLAUDE.md requires: the decision is the argv's SHAPE and
/// its first tokens, never the name of the function that built it. Renaming
/// `pr_facts_argv` changes nothing here; adding a new argv builder fails until
/// its ask is argued onto `ALLOWED_ASKS`.
///
/// # The residual, stated because a scan must state one
///
/// **A landing verb the driver reaches through a shared helper it does not
/// own.** The `git` half is closed structurally — `rddrive::RdRunner` has no
/// `git` method, and the one bridge to the wider trait answers `git` with a
/// refusal — so the compiler, not this scan, enforces that half. The `gh` half
/// is bounded but not closed: an argv assembled at runtime from a config value,
/// or one built inside a helper in another module and handed to the driver, is
/// invisible here. None exists today.
#[test]
fn the_driver_never_builds_a_landing_verb_and_never_grants_a_merge() {
    let mut findings: Vec<String> = Vec::new();
    let mut asks_seen = 0usize;
    let mut unmatched: Vec<&str> = ALLOWED_ASKS.iter().map(|(_, v, _)| *v).collect();

    for rel in DRIVER_FILES {
        let src = driver_production_source(rel);

        // 1. Default-deny on what the driver ASKS `gh` for.
        for argv in argv_literals(&src) {
            // Only argvs that look like a `gh` ask are judged: a two-token-plus
            // list whose first token is a `gh` noun. Anything else here is a
            // template key list or an audit detail, not an external action.
            let Some(first) = argv.first() else { continue };
            if !matches!(first.as_str(), "pr" | "issue" | "api" | "release" | "repo" | "run") {
                continue;
            }
            asks_seen += 1;
            let verb = argv.get(1).map(String::as_str).unwrap_or("");
            match ALLOWED_ASKS.iter().find(|(n, v, _)| n == first && *v == verb) {
                Some((_, v, _)) => unmatched.retain(|u| u != v),
                None => findings.push(format!(
                    "{rel}: the driver asks `gh {first} {verb}`, which is not on the permitted \
                     list — §3.1 items 1 and 3 deny every ask that is not argued onto it"
                )),
            }
        }

        // 2. Registry capabilities, matched as calls.
        for (bad, why) in FORBIDDEN_CALLS {
            if names_call(&src, bad) {
                findings.push(format!("{rel}: calls {bad} — {why}"));
            }
        }

        // 3. #2501's one permitted kill and #3367 item 5's one permitted
        // enqueue, counted. Every file that is not the one named on the row
        // must call it zero times; the named file must call it exactly as often
        // as the row says. Both directions fail: a call from a second site, and
        // a row whose count has gone stale because the site was removed.
        for (call, home, sites, why) in [PERMITTED_RELEASE, PERMITTED_ENQUEUE] {
            let found = count_calls(&src, call);
            let want = if rel == home { sites } else { 0 };
            if found != want {
                findings.push(format!(
                    "{rel}: calls {call} {found} time(s), and the permitted count for this \
                     file is {want} — {why}. §3.1 permits ONE site; a second is a second place \
                     the rule can be broken, and must be argued onto its PERMITTED_ row."
                ));
            }
        }
    }

    // The population control. A scan that extracted no argv reports clean, which
    // is byte-identical to one that found nothing forbidden.
    assert!(
        asks_seen >= 3,
        "only {asks_seen} `gh` asks extracted across the driver's files — the extraction read \
         (almost) nothing, which is not the same as finding nothing"
    );
    assert!(findings.is_empty(), "review-driver.md §3.1:\n  {}", findings.join("\n  "));
    assert!(
        unmatched.is_empty(),
        "permitted asks that match nothing any more — a row nobody re-checked: {unmatched:?}"
    );

    // Every denylist row must still name something that EXISTS, or it denies a
    // function that has been renamed and reports green while doing it.
    //
    // The registry's methods live in `mod.rs` and, since #3498 P3, in the
    // `impl OrchRegistry` files under `registry/` (`grant_merge`,
    // `record_verdict`, `queue_merge` and `queue_merge_with` are in
    // `registry/merge.rs`). The directory is read whole, in path order, so a
    // later slice moving a row's subject into it needs no edit here.
    let orchestration = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/orchestration");
    let mut registry: Vec<std::path::PathBuf> = std::fs::read_dir(orchestration.join("registry"))
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "rs")).collect())
        .unwrap_or_default();
    registry.sort();
    let haystack = [orchestration.join("mod.rs"), orchestration.join("mcp.rs")]
        .into_iter()
        .chain(registry)
        .map(|p| std::fs::read_to_string(p).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    for (bad, _) in FORBIDDEN_CALLS {
        assert!(
            haystack.contains(&format!("fn {bad}(")),
            "the denylist row for `{bad}` names nothing that exists any more — it was probably \
             renamed, and this row has been denying nothing since"
        );
    }
    // The allowlist row is self-verifying the same way, and it has a second
    // half the denylist rows do not: the barrier must live OUTSIDE the driver's
    // own files. A `release_driven_pane` defined in `rdtick/` would be the
    // driver writing its own barrier, which is not a barrier.
    for (call, ..) in [PERMITTED_RELEASE, PERMITTED_ENQUEUE] {
        assert!(
            haystack.contains(&format!("fn {call}(")),
            "the permitted row for {call} names nothing that exists any more"
        );
        for rel in DRIVER_FILES {
            assert!(
                !driver_production_source(rel).contains(&format!("fn {call}(")),
                "{rel} DEFINES {call} — a capability the driver has must be a barrier it \
                 passes, not one it writes"
            );
        }
    }
}

/// The positive control, and it is the one this guard most needs: **it reddens
/// on a real merge call, and not on a string that merely contains one.**
///
/// The distinction is the whole difference between a guard that enforces the
/// rule and one that enforces a prefix — which is what the previous version of
/// this scan did, and what its own suite caught.
#[test]
fn the_landing_verb_scan_fires_on_a_real_merge_and_not_on_a_lookalike() {
    // A driver that landed. Both halves: the ask, and the registry call.
    let hostile = r##"
        pub fn land_argv(pr: u64) -> Vec<String> {
            vec!["pr".into(), "merge".into(), pr.to_string(), "--delete-branch".into()]
        }
        fn land(reg: &OrchRegistry) { reg.grant_merge(group, pr, "human"); }
    "##;
    let asks = argv_literals(hostile);
    assert!(
        asks.iter().any(|a| a.first().map(String::as_str) == Some("pr")
            && a.get(1).map(String::as_str) == Some("merge")),
        "the extraction missed a landing argv: {asks:?}"
    );
    assert!(
        !ALLOWED_ASKS.iter().any(|(n, v, _)| *n == "pr" && *v == "merge"),
        "`gh pr merge` must not be a permitted ask"
    );
    assert!(names_call(hostile, "grant_merge"));

    // …and the lookalikes the driver's real source is full of, which a guard
    // that fired on them would be deleted within a day.
    let benign = r##"
        let gate = self.merge_gate(group);
        let spec = mergeq::GateSpec::Absent;
        let _ = "merge-base";
        let _ = "merge_queue.json";
        entry.record_verdict_seen(&block, v, &head);
        vec!["pr".into(), "view".into(), "--json".into(), "state,headRefOid".into()]
    "##;
    assert!(
        !names_call(benign, "record_verdict"),
        "a substring match would report the driver as writing verdicts"
    );
    assert!(
        !names_call(benign, "grant_merge"),
        "`merge_gate` and `merge_queue.json` are not `grant_merge`"
    );
    let benign_asks = argv_literals(benign);
    assert!(
        benign_asks.iter().any(|a| a.first().map(String::as_str) == Some("pr")
            && a.get(1).map(String::as_str) == Some("view")),
        "…while a permitted ask is still extracted, so the extraction is not simply blind"
    );
    // The real call still fires, so narrowing to call-shape did not disarm it.
    assert!(names_call("reg.record_verdict(&g, &a, 1, \"pass\", \"s\");", "record_verdict"));
}

/// #2501's half of the positive control: **the narrowing did not open a hole.**
///
/// The interesting failure is not "the scan stopped catching `kill_agent`" — it
/// is a driver that reaches a kill by some OTHER name now that one route is
/// permitted, or one that releases a pane from a second site the release rule
/// was never argued for. Both are checked by performing them.
#[test]
fn the_kill_scan_permits_exactly_one_release_site_and_no_other_route_to_a_kill() {
    // A driver that killed by another name. Each of these is a real function on
    // `OrchRegistry` and none of them is `release_driven_pane`.
    for hostile in [
        "fn tidy(reg: &OrchRegistry) { let _ = reg.kill_agent(&lane); }",
        "fn tidy(reg: &OrchRegistry) { reg.kill_agent_as(&lane, ExitInitiator::IdleTimeout); }",
        "fn tidy(reg: &OrchRegistry) { let _ = reg.mark_dead(&lane, Some(0)); }",
        "fn tidy(reg: &OrchRegistry) { reg.reap_idle_agents(now); }",
    ] {
        assert!(
            FORBIDDEN_CALLS.iter().any(|(bad, _)| names_call(hostile, bad)),
            "a kill route no denylist row catches: {hostile}"
        );
    }
    // …and the lookalikes the real source carries, which a guard that fired on
    // them would be deleted within a day.
    let benign = r##"
        let dead = self.rd_dead_lane_pane(rec);
        let killed_by = a.killed_by.map(|who| who.as_str());
        let _ = "agent-kill";
        entry.forget_dead_panes(&|id| self.agent(id).is_some());
    "##;
    for (bad, _) in FORBIDDEN_CALLS {
        assert!(
            !names_call(benign, bad),
            "`{bad}` fired on source that only NAMES a kill — `killed_by`, an audit string and \
             `forget_dead_panes` are not kills"
        );
    }

    // The count, in both directions. One site is what the row permits; two is
    // the thing the count exists to catch, and zero is a row gone stale.
    let one = "fn tick(&self) { let _ = self.release_driven_pane(&agent); }";
    let two = "fn tick(&self) { self.release_driven_pane(&a); self.release_driven_pane(&b); }";
    assert_eq!(count_calls(one, PERMITTED_RELEASE.0), 1);
    assert_eq!(count_calls(two, PERMITTED_RELEASE.0), 2);
    assert_ne!(
        count_calls(two, PERMITTED_RELEASE.0),
        PERMITTED_RELEASE.2,
        "two release sites must not satisfy a row that permits {}",
        PERMITTED_RELEASE.2
    );
    assert_eq!(
        count_calls("fn tick(&self) {}", PERMITTED_RELEASE.0),
        0,
        "…and a file that never releases must count zero, so the row cannot go stale unnoticed"
    );
    // The live one, read the way the scan reads it: the permitted site really is
    // in the file the row names, so the row is not permitting an empty set.
    assert_eq!(
        count_calls(&driver_production_source(PERMITTED_RELEASE.1), PERMITTED_RELEASE.0),
        PERMITTED_RELEASE.2,
        "the permitted release site is not where the row says it is"
    );
}

/// **The proof the #464 allowlist row names**, so that row can go stale rather
/// than merely be trusted.
///
/// `no_registry_construction_bypasses_the_test_agent_dir_overrides` in
/// `tests/orchestration/` default-denies raw `OrchRegistry::new` across every
/// `tests/*.rs`, because a registry built without the agent-dir overrides falls
/// back to the user's REAL `~/.claude/agents` on its first spawn — the gap that
/// left 1,111 stray files on a live dev machine. This file has a row in that
/// allowlist for its own `relaunch_registry`, which is a decision to widen a
/// default-deny surface by one entry.
///
/// A row that says only "this file has a helper" is trusted, not checked: the
/// helper could stop applying an override tomorrow and the row would still read
/// correct. So this asserts the property the row actually depends on — that the
/// helper applies **every** override — by reading the helper's own source, which
/// is the only way to see what it does rather than what a registry happens to
/// report.
///
/// The `expected` list is written out here rather than derived from the source,
/// because deriving it from the thing under test is how a pin agrees with
/// whatever the code currently does. Adding a fifth override to the helper
/// without adding it here is meant to be silent; **removing** one is what this
/// catches, and removal is the direction that reopens #464.
#[test]
fn its_registry_helper_applies_every_override_this_allowlist_row_assumes() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reviewdrive/helpers.rs"),
    )
    .expect("this file reads itself");

    // The helper's body: from its signature to the first line that closes it at
    // column 0 — narrow enough that an override applied by some OTHER function
    // in this file cannot satisfy the assertion below.
    let start = src
        .find("fn relaunch_registry(dir: &std::path::Path) -> OrchRegistry {")
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
            "the #464 allowlist row for tests/reviewdrive/helpers.rs assumes this helper applies every \
             override; it no longer applies {needed}, so a registry built through it can reach \
             the real agent dirs and the row's premise is gone"
        );
    }

    // The population control: the extraction really did isolate the helper, so
    // the four assertions above are about ITS body and not about the whole file
    // — which contains those same names in other functions and in prose.
    assert!(
        body.len() < 1_200,
        "the helper's body extraction ran away ({} chars); the assertions above would then be \
         satisfied by any other function in this file",
        body.len()
    );
    assert!(
        !body.contains("#[test]"),
        "the extraction swallowed a test, so it is no longer reading only the helper"
    );
}

/// How many times `src` **assigns** to `field` — `x.field = …`. A comparison
/// (`x.field == …`) does not match, because the needle requires `= ` and a
/// comparison has `==` there; nor does a struct-literal `field: …` or a
/// declaration. See the caller for the residual this leaves.
///
/// A second counter beside [`count_calls`] rather than a generalisation of it:
/// the two match different shapes, and a guard that cannot say which shape it
/// matched is two guards.
fn count_assignments(src: &str, field: &str) -> usize {
    src.matches(&format!(".{field} = ")).count()
}

/// **#2509's one-shot grace has exactly one writer and one proposal site**, and
/// this is a default-deny scan that says so.
///
/// The whole safety argument for granting a round past INVARIANT 9's bound is
/// that it is granted ONCE. That is not a property of a `bool` — it is a
/// property of there being a single writer, in `advance`, on an arc `decide`
/// refuses to propose twice. A second assignment anywhere in the driver, or a
/// second site proposing `Counter::BodyOnlyGrace`, is a second place the "never
/// stacking" rule can be broken, and neither would redden any behavioural test:
/// each would look locally correct where it stood.
///
/// **Decided on shape, never on a name** (CLAUDE.md's source-scanning-guard
/// convention). The two things counted are a field ASSIGNMENT and an enum
/// VARIANT path — neither can be spelled another way and still compile, so a
/// rename moves the whole guard rather than stepping over it. The scan runs over
/// [`DRIVER_FILES`], a file scope rather than an `rd_*` prefix, for the reason
/// that list already carries.
///
/// **Per file, and each count is a different fact.** `reviewdrive.rs` names the
/// variant twice — once where `decide_review_wait` proposes the arc, once in
/// `advance`'s bump arm — and `rdtick/` names it once, where the tick reads
/// the step to decide whether to write `rd-round-grace`. Collapsing the three
/// into one total would let a second proposal site hide behind a deleted read.
///
/// # What this scan cannot see, stated rather than implied
///
/// It bounds explicit assignment. It does NOT see a whole-`Counters` write —
/// `entry.counters = Counters::seeded(n)` — which re-grants the grace along with
/// the three counts. That is `drive_review(reset_counters: true)`, an explicit,
/// audited orchestrator decision to spend a fresh budget (§2.3), and it is right
/// that it restores this one too; a scan refusing it would be refusing the
/// documented resume. It equally cannot see a struct literal
/// (`Counters { body_only_grace: false, .. }`), which nothing in the driver
/// writes today. The compiler is what keeps the field's writers inside the
/// crate; this is what keeps them countable.
#[test]
fn the_one_shot_grace_has_one_writer_and_one_proposal_site() {
    let expected = |rel: &str| -> (usize, usize, &'static str) {
        if rel.ends_with("reviewdrive.rs") {
            (1, 2, "the engine proposes the arc and spends the grace on it")
        } else if rel.ends_with("rdtick") {
            (0, 1, "the tick only READS the step, to decide whether to audit")
        } else {
            (0, 0, "nothing else in the driver touches the grace at all")
        }
    };
    for rel in DRIVER_FILES {
        let src = driver_production_source(rel);
        let (w, p, why) = expected(rel);
        assert_eq!(
            count_assignments(&src, "body_only_grace"),
            w,
            "{rel}: {why} — assignments to `body_only_grace`"
        );
        assert_eq!(
            src.matches("Counter::BodyOnlyGrace").count(),
            p,
            "{rel}: {why} — mentions of `Counter::BodyOnlyGrace`"
        );
    }

    // **Self-verifying**: both counters must fire on a planted specimen, in the
    // SAME shape the scan uses — a control run in another shape certifies
    // nothing, and a scan that silently matched nothing reads exactly like a
    // clean one.
    assert_eq!(
        count_assignments("fn f() { self.counters.body_only_grace = true; }", "body_only_grace"),
        1,
        "the assignment counter must see a real assignment"
    );
    assert_eq!(
        count_assignments("fn f() -> bool { c.body_only_grace == true }", "body_only_grace"),
        0,
        "…and must not mistake a comparison for one"
    );
    assert_eq!(
        count_assignments("struct C { body_only_grace: bool }", "body_only_grace"),
        0,
        "…nor a field declaration"
    );
    assert_eq!(
        "DriveStep::spend(x, Counter::BodyOnlyGrace)".matches("Counter::BodyOnlyGrace").count(),
        1,
        "the variant counter must see a real proposal"
    );
}
