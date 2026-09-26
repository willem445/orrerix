//! The source-scanning guards: tests that read the repo's own source (via `CARGO_MANIFEST_DIR` or `include_str!`) rather than drive behaviour, kept together per docs/design/module-layout.md.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

/// Every `.md` at ANY depth under `dir` (#1845, review round 1 finding 1).
///
/// Recursive because the `.gitattributes` patterns it checks are `**`: a flat
/// `read_dir` would police a NARROWER population than the rule, so a template
/// added in a subdirectory would be pinned by git, invisible to the guard, and
/// still over the floor. No subdirectory exists in either tree today, which is
/// exactly why the flat version read as correct.
fn markdown_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("{} is not readable: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("a readable directory yields readable entries").path();
        if path.is_dir() {
            markdown_files_recursive(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
}

/// The checkout is the thing under test here (#1845).
///
/// `include_str!` embeds ON-DISK bytes, so each of these files ships its
/// checkout's line endings straight into the running product: the resident
/// prompt the orchestrator pays for on every model call, the goldens that pin
/// it byte-for-byte, and the driver briefs. `.gitattributes` pins both trees to
/// `eol=lf` so those bytes are identical on every platform — but changing a
/// gitattribute does NOT rewrite files already in a worktree, so an agent
/// worktree cut before the rule landed, or a checkout whose `.gitattributes`
/// line was later removed, still carries CRLF with nothing else to say so.
///
/// **Why this sits beside the budget assertion rather than inside it.** The
/// budget test does notice a CRLF checkout today, but only because the CR bytes
/// happen to exceed the margin left under `RESIDENT_CORE_BUDGET` — 8 B of it,
/// against 509 CR bytes, measured on `orchestrator.md` at blob ef585635 while
/// that constant is 35,000. Shorten the template
/// past that and the budget goes quiet again on exactly the platform that pays
/// more, which is the issue's own "the guard is worse than none" case (#1845).
/// This test is margin-independent and covers every template, not only the one
/// with a budget.
///
/// **Default-deny over the directory, not over a list of consts.** Two template
/// consts (`WORKFLOW_TPL`, `BLOCK_TPL`) are private to the lib and unreachable
/// from an integration test, and a template added tomorrow would be on no list
/// at all — so the population is whatever the directories hold, walked
/// RECURSIVELY because the attribute patterns match at any depth and a flat walk
/// would police a narrower population than the rule. The floors below are the
/// vacuity control (an unreadable or empty walk passes a `!contains` check
/// trivially) and are deliberately looser than the counts observed when this was
/// written — 11 templates and 7 fixture files, no subdirectories — so an
/// ordinary add or removal does not touch this test.
///
/// **The `.md` filter is the RULE's scope, not a shortcut.** `.gitattributes`
/// pins `**/*.md` rather than `**` on purpose: `text` is an explicit override,
/// so a bare `**` would force EOL conversion on a future non-`.md` fixture — a
/// rendered-bytes golden, anything genuinely binary — and corrupt it at
/// checkout, while this walk would not police it either. Rule and guard cover
/// the same set, and a non-`.md` file added to either tree is a deliberate act
/// that decides its own treatment in both places (review round 2, N2/N3).
///
/// **Do not delete this as redundant with the budget assertion.** It reads as
/// redundant only while the margin happens to be smaller than the CR count, and
/// that is the one condition under which it is NOT redundant that a reader can
/// check. Shorten `orchestrator.md` by more than the margin and the budget
/// assertion goes green on a CRLF checkout, at which point this is the only
/// thing standing between a stale worktree and a silently platform-dependent
/// resident prompt — the #1683 number, measured in a unit nobody pays. The
/// PR's own one-mechanism argument is about not adding a SECOND budget, not
/// about dropping the checkout guard.
///
/// **Residual:** the walk reads the tree the test binary is RUN against, which
/// need not be the tree it was COMPILED against. The final assertion on
/// `ORCHESTRATOR_TPL` closes that for the one file whose bytes the budget
/// measures; every other file in both trees is covered by the walk alone.
#[test]
fn every_prompt_template_is_checked_out_with_lf_endings() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // (the `.gitattributes` pattern, the dir under `src-tauri/`, the floor)
    let trees = [
        (
            "src-tauri/src/orchestration/templates/**/*.md",
            "src/orchestration/templates",
            8usize,
        ),
        ("src-tauri/tests/fixtures/pre222/**/*.md", "tests/fixtures/pre222", 5usize),
    ];
    let mut scanned = 0usize;
    for (pattern, rel, floor) in trees {
        let dir = root.join(rel);
        let mut files = Vec::new();
        markdown_files_recursive(&dir, &mut files);
        let here = files.len();
        for path in &files {
            let bytes = fs::read(path)
                .unwrap_or_else(|e| panic!("{} is not readable: {e}", path.display()));
            assert!(
                !bytes.contains(&b'\r'),
                "{} carries a CR, so this checkout is CRLF and `include_str!` embeds one \
                 extra byte per line — the resident-prompt budget would be measuring the \
                 checkout instead of the content (#1845). `.gitattributes` pins `{pattern}` \
                 to `eol=lf`, but an attribute never rewrites files already on disk. FIX: \
                 delete these files and check them out again — `rm` them, then \
                 `git checkout -- <dir>`. `git add --renormalize .` will NOT do it: it \
                 rewrites the index only, silently, so you get a clean `git status` beside \
                 a still-CRLF file. A plain `git checkout --` without deleting first is a \
                 no-op too, because git considers the file up to date. AND IF A CR SURVIVES \
                 ALL THAT, it is in the BLOB, not the checkout: git's `text` filter converts \
                 CRLF pairs and leaves a LONE CR untouched at both ends, so `eol=lf` cannot \
                 strip one and no amount of re-checking-out will either — fix the content.",
                path.display()
            );
        }
        assert!(
            here >= floor,
            "{} yielded {here} markdown files, under the floor of {floor} — a walk that \
             finds nothing passes the CR check vacuously, so an empty one is a broken guard, \
             not a clean tree",
            dir.display()
        );
        scanned += here;
    }
    // Not implied by the floors above: an emptied `trees` array runs neither of
    // them, and every assertion in this test would then be unreached.
    assert!(scanned > 0, "the walk must reach files in at least one tree");

    // The walk is the population; this is the wiring half — the bytes the
    // COMPILER embedded, which is what the budget assertion above measures and
    // what the product ships.
    assert!(
        !ORCHESTRATOR_TPL.contains('\r'),
        "the EMBEDDED resident core carries a CR: this test binary was compiled against a \
         CRLF checkout, whatever the directory walk above found (#1845)"
    );
}

/// **The GUI cannot edit a field it has never heard of** (#880). `allow:` has
/// been a `RawBlock` field since #222 and the workflow pane never grew a
/// control for it — or even a name for it — so a workflow that declared one
/// looked, in the pane, exactly like a workflow that didn't. Nothing was
/// broken on either side; the two sides simply had no way to disagree out
/// loud.
///
/// `src/workflow-schema.json` is the shared statement of the wire schema, and
/// this is the engine's half of enforcing it: every section's field set, as
/// serde actually defines it (`workflow::workflow_schema_keys()`, derived from
/// the `Raw*` types rather than hand-listed), against the same section in the
/// committed manifest. **Both directions are errors** — a field added to a
/// `Raw*` struct with no manifest entry is a field the GUI will never offer,
/// and a manifest entry with no `Raw*` field is a control that would write a
/// key the engine rejects the whole file over (`deny_unknown_fields`).
///
/// The frontend's half of the same contract is `test/workflowschema.test.ts`
/// (the pane's parser must know each field, and its serializer must emit it).
/// Deliberately two tests and not one: they pin different sides, and a repo
/// where only one is green is a repo where the pane and the engine disagree
/// about what a workflow file is — which is exactly what a human then gets lied
/// to about.
#[test]
fn the_workflow_schema_manifest_matches_the_engines_raw_types() {
    // Relative to THIS file: src-tauri/tests/ -> repo root -> src/.
    const MANIFEST: &str = include_str!("../../../src/workflow-schema.json");
    let manifest: Value = serde_json::from_str(MANIFEST).expect("the manifest must be valid JSON");
    let sections = manifest["sections"].as_object().expect("manifest.sections must be a mapping");

    let engine = workflow::workflow_schema_keys();
    assert_eq!(
        sections.keys().cloned().collect::<std::collections::BTreeSet<String>>(),
        engine.keys().cloned().collect::<std::collections::BTreeSet<String>>(),
        "the manifest and the engine must describe the same set of sections"
    );

    for (section, fields) in &engine {
        let declared: std::collections::BTreeSet<String> = sections[section.as_str()]["fields"]
            .as_array()
            .unwrap_or_else(|| panic!("manifest section {section:?} needs a fields array"))
            .iter()
            .map(|f| {
                f["name"]
                    .as_str()
                    .unwrap_or_else(|| panic!("every field in {section:?} needs a name"))
                    .to_string()
            })
            .collect();
        let actual: std::collections::BTreeSet<String> = fields.iter().cloned().collect();
        assert_eq!(
            declared, actual,
            "section {section:?}: src/workflow-schema.json and the engine's Raw* types disagree. \
             A field the engine accepts but the manifest omits is one no GUI control will ever \
             be generated for; a field the manifest declares but the engine refuses is a control \
             that writes a key `deny_unknown_fields` rejects the whole file over."
        );
        // A section with no fields would satisfy the equality above only if the
        // engine's type were empty too — impossible for these, but say so, so a
        // future refactor that hands back an empty map fails loudly instead of
        // passing vacuously.
        assert!(!actual.is_empty(), "section {section:?} must have fields");
    }
}

/// **A manifest row is data slice C generates a control from, so a wrong row is
/// a wrong form** (#880 review finding 1). The test above pins field NAMES; this
/// one pins what those fields may CONTAIN — every `values`, `default`, `min`,
/// `max` and `max_entries` in `src/workflow-schema.json` against
/// `workflow::workflow_schema_field_facts()`, which derives them from the
/// engine's own accessors and constants rather than from a second hand-written
/// list.
///
/// **Both directions, like the name pin.** A fact the engine states and the
/// manifest omits is a control generated without a bound the engine enforces; a
/// fact the manifest states and the engine never did is decoration a reviewer
/// cannot check — the falsification that motivated this test (a reviewer changed
/// `block.cli`'s whole `values` to `["notacli", "alsofake"]` and `max_batch`'s
/// default 3 → 99, and both suites stayed green) is the second kind.
///
/// What is deliberately NOT pinned here, so the manifest's own note can be
/// trusted: `title`/`help` prose, and — named in `workflow_schema_field_facts`'s
/// docblock — `gate.require`'s value set, which the engine states only as match
/// arms.
#[test]
fn the_workflow_schema_manifest_matches_the_engines_values_defaults_and_bounds() {
    const MANIFEST: &str = include_str!("../../../src/workflow-schema.json");
    // `maxLength` (#1457) is a LENGTH bound on a string field, distinct from
    // `max` — which this manifest documents as "highest accepted number".
    const PINNED: [&str; 6] =
        ["values", "default", "min", "max", "max_entries", "maxLength"];

    let manifest: Value = serde_json::from_str(MANIFEST).expect("the manifest must be valid JSON");
    let sections = manifest["sections"].as_object().expect("manifest.sections must be a mapping");
    let facts = workflow::workflow_schema_field_facts();

    // Every fact the manifest declares must be one the engine states, and match.
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (section, body) in sections {
        for field in body["fields"].as_array().expect("fields must be an array") {
            let name = field["name"].as_str().expect("a field needs a name");
            let key = format!("{section}.{name}");
            let stated = facts.get(&key);
            for k in PINNED {
                let declared = &field[k];
                if declared.is_null() {
                    continue;
                }
                seen.insert(format!("{key}.{k}"));
                let engine = stated.map(|f| &f[k]).filter(|v| !v.is_null()).unwrap_or_else(|| {
                    panic!(
                        "{key}: the manifest declares {k} = {declared}, but the engine states no \
                         {k} for this field. Either it is not a real constraint — in which case a \
                         generated control would enforce something the engine never asked for — \
                         or `workflow_schema_field_facts()` needs to say where it comes from."
                    )
                });
                assert_eq!(
                    declared, engine,
                    "{key}: the manifest says {k} = {declared}, the engine says {engine}. \
                     Slice C generates this field's control from the manifest, so this is the \
                     difference between a form a human can use and one that writes a file the \
                     engine refuses."
                );
            }
        }
    }

    // …and every fact the engine states must be declared. This is the direction
    // that catches a NEW bound (a `max` added to the parse, a fifth CLI) rather
    // than a wrong one.
    for (key, stated) in &facts {
        for k in PINNED {
            if stated[k].is_null() {
                continue;
            }
            assert!(
                seen.contains(&format!("{key}.{k}")),
                "{key}: the engine enforces {k} = {}, and src/workflow-schema.json does not say \
                 so — a control generated from this row would not know about it.",
                stated[k]
            );
        }
    }
    assert!(!seen.is_empty(), "this test must actually compare something");
}

/// Every enum value the manifest declares must be one the ENGINE accepts —
/// checked by feeding each through `parse_workflow` rather than by comparing two
/// lists, because a list can be copied wrong in both places at once.
///
/// The empty values are the point: `block.cli: ""` (inherit the group's CLI) and
/// `intake.source: ""` (the built-in source) are the states most files are
/// actually in, and a manifest that omitted them — as this one did until the
/// review — gives slice C a `<select>` that cannot represent a legal file.
#[test]
fn every_enum_value_the_manifest_declares_is_one_the_engine_accepts() {
    const MANIFEST: &str = include_str!("../../../src/workflow-schema.json");
    let manifest: Value = serde_json::from_str(MANIFEST).expect("valid JSON");

    let value_of = |section: &str, field: &str| -> Vec<String> {
        manifest["sections"][section]["fields"]
            .as_array()
            .expect("fields")
            .iter()
            .find(|f| f["name"] == field)
            .unwrap_or_else(|| panic!("{section}.{field} must exist"))["values"]
            .as_array()
            .unwrap_or_else(|| panic!("{section}.{field} must declare values"))
            .iter()
            .map(|v| v.as_str().expect("an enum value is a string").to_string())
            .collect()
    };

    for kind in value_of("block", "kind") {
        // The four class names are reserved as ids for their own class, so the
        // block is named for the kind under test rather than a fixed `w`.
        let text = format!("version: 1\nblocks:\n  - id: {kind}\n    kind: {kind}\n");
        assert!(
            workflow::parse_workflow(&text).is_ok(),
            "block.kind: the manifest declares {kind:?}, which the engine refuses"
        );
    }
    for cli in value_of("block", "cli") {
        let text = format!("version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: \"{cli}\"\n");
        assert!(
            workflow::parse_workflow(&text).is_ok(),
            "block.cli: the manifest declares {cli:?}, which the engine refuses"
        );
    }
    for hint in value_of("block", "role_hint") {
        // A hint requires its own kind — that pairing is the hint's whole rule.
        let kind = match hint.as_str() {
            "advisor" => "planner",
            "process" => "worker",
            "liaison" => "reviewer",
            other => panic!("block.role_hint declares {other:?}, which this test has no pairing for — if the engine grew a hint, pair it here"),
        };
        let text =
            format!("version: 1\nblocks:\n  - id: h\n    kind: {kind}\n    role_hint: {hint}\n");
        assert!(
            workflow::parse_workflow(&text).is_ok(),
            "block.role_hint: the manifest declares {hint:?}, which the engine refuses on kind {kind}"
        );
    }
    for source in value_of("intake", "source") {
        let text = format!(
            "version: 1\nblocks:\n  - id: w\n    kind: worker\nintake:\n  source: \"{source}\"\n"
        );
        assert!(
            workflow::parse_workflow(&text).is_ok(),
            "intake.source: the manifest declares {source:?}, which the engine refuses"
        );
    }
    for require in value_of("gate", "require") {
        let threshold = if require == "threshold" { "    threshold: 1\n" } else { "" };
        let text = format!(
            "version: 1\nblocks:\n  - id: rev\n    kind: reviewer\ngates:\n  merge:\n    require: {require}\n{threshold}    reviewers: [rev]\n"
        );
        assert!(
            workflow::parse_workflow(&text).is_ok(),
            "gate.require: the manifest declares {require:?}, which the engine refuses"
        );
    }

    // And the sets are CLOSED: a value outside them is refused, so `enum` in the
    // manifest means what its legend says it does.
    for text in [
        "version: 1\nblocks:\n  - id: w\n    kind: superuser\n",
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: notacli\n",
        "version: 1\nblocks:\n  - id: w\n    kind: worker\n    role_hint: supervisor\n",
        "version: 1\nblocks:\n  - id: w\n    kind: worker\nintake:\n  source: githublabels\n",
        "version: 1\nblocks:\n  - id: rev\n    kind: reviewer\ngates:\n  merge:\n    require: most\n    reviewers: [rev]\n",
    ] {
        assert!(
            workflow::parse_workflow(text).is_err(),
            "a value outside a declared enum must be refused, never coerced:\n{text}"
        );
    }
}

/// Every `[orrerix] …` notice composed in the MCP surface either scrubs the text
/// it interpolates or is on this list, with a reason.
///
/// Default-deny, keyed on the notice's own leading literal rather than on a line
/// number or a binding's name (CLAUDE.md's guard convention: a rename must not
/// step over it, and a hand-derived position is valid only at the commit it was
/// derived on). A new arm that composes a notice out of agent text is a RED here
/// on the day it is written, which is what rev-2 asked for: the fix for one path
/// must not leave a fourth path discoverable only by a reviewer.
const NOTICE_SCRUB_EXEMPT: [(&str, &str); 2] = [
    (
        "\\n[orrerix] {g}",
        "the gate clause: `gate_status_line_with` composes loomux-owned text, and the one \
         untrusted half it can carry (gh stderr) is scrubbed at source by `gh_failure_text` \
         — pinned by `gh_stderr_reaching_the_gate_line_cannot_forge_a_loomux_notice`",
    ),
    (
        "[orrerix] {} ({}) recorded verdict",
        "the verdict POINTER (#3040 N2): every field it interpolates is loomux-owned — a \
         minted agent id, a workflow block id, a `Verdict` enum, the `u64` PR twice — and \
         the gate clause it nests is the row above. The summary was the one \
         delegate-authored field and it is GONE from this notice rather than scrubbed, \
         which is why the row reads as an exemption instead of a scrub. Pinned by \
         `an_undriven_verdict_copy_is_a_pointer_not_a_summary` and by the forgery sweep's \
         `review_verdict pointer` arm, which asserts no delegate text reaches the pane in \
         any spelling. Adding an agent-authored field back here means scrubbing it AND \
         withdrawing this row.",
    ),
];

#[test]
fn every_loomux_notice_composed_in_the_mcp_surface_scrubs_what_it_interpolates() {
    // The structural half of rev-2 F1b (INV4): the behavioural sweep above
    // proves today's paths are closed, and this proves a NEW one cannot open
    // quietly. It scans `mcp.rs` — the file where every delegate-callable tool
    // composes its own notice — for string literals that begin an `[orrerix] …`
    // line, and requires each one's `format!` call to mention a scrub.
    //
    // Stated blind spots, because a scan that does not enumerate its own limits
    // is a claim rather than a guard:
    //
    //   - a notice composed in ANOTHER module and merely delivered from here
    //     (today `report::structured_notice` is the only one, and it scrubs
    //     internally — pinned by its own unit tests in `report.rs`);
    //   - a scrub applied further up the call chain rather than inside the
    //     `format!` (that reads as a violation here, which is the safe
    //     direction: it must be argued into the exemption list above);
    //   - a notice assembled by `push_str` rather than `format!`;
    //   - **a new FIELD added beside a scrubbed one on an existing site.** This
    //     checks that a scrub is named in the call, not that every interpolated
    //     argument passes through one, so a second agent-authored field added
    //     to a call that already scrubs its first reads as compliant here.
    //
    // The behavioural sweep covers the first three: it drives the tools the
    // delegate is offered, however their text is assembled. It covers the
    // FOURTH only for a tool whose arguments it can satisfy — see its own doc
    // for that boundary, which is the one genuinely reviewer-checked gap
    // between these two guards.
    let src = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/orchestration/mcp.rs"),
    )
    .expect("mcp.rs must be readable");
    const SCRUBS: [&str; 3] = ["relay_payload", "relay_payload_keeping_lines", "sanitize_gh_text"];

    let mut sites = 0usize;
    let mut i = 0usize;
    while let Some(rel) = src[i..].find("format!(") {
        let at = i + rel;
        // BYTE offsets throughout, never char indices: `find` returns bytes, and
        // an earlier revision indexed a `Vec<char>` with one of them — which is
        // a panic exactly as far into the file as its non-ASCII prose has
        // accumulated (this file's comments are full of em dashes), not a
        // failure of the rule being guarded.
        let after = at + "format!(".len();
        i = after;
        // The literal is usually on the NEXT line — `format!(⏎    "[orrerix] …"` —
        // which is why this skips whitespace instead of matching `format!("`.
        // That earlier spelling found exactly one of the four sites in this file
        // and reported itself healthy, which is why the floor assertion below
        // exists: a scan that quietly stops matching is worse than no scan.
        let lit_start = match src[after..].char_indices().find(|(_, c)| !c.is_whitespace()) {
            Some((off, '"')) => after + off + 1,
            _ => continue,
        };
        let lit = &src[lit_start..];
        if !(lit.starts_with("[orrerix]") || lit.starts_with("\\n[orrerix]")) {
            continue;
        }
        sites += 1;
        // The call's own extent, by paren depth — not a fixed window, so a long
        // argument list cannot push the scrub out of view and pass.
        //
        // Parens are counted only in CODE. A naive walker counts them inside
        // string literals and comments too, and both appear here for real: the
        // verdict notice's own literal contains `({})`, and this file's comments
        // are prose full of parentheses. Today those happen to be balanced, so a
        // naive walk lands on the right byte by luck — one unbalanced `)` in a
        // literal or a comment and the extent silently becomes some other span
        // of the file, which is a tripwire that reports on the wrong thing while
        // looking healthy. Skipping both is the whole fix (rev-2 disposition,
        // item 2); nothing else about the scan changes.
        let mut depth = 0i32;
        let mut end = at;
        let mut chars = src[at..].char_indices().peekable();
        while let Some((k, c)) = chars.next() {
            match c {
                // A string literal: run to its unescaped close.
                '"' => {
                    while let Some((_, d)) = chars.next() {
                        if d == '\\' {
                            chars.next(); // the escaped char, whatever it is
                        } else if d == '"' {
                            break;
                        }
                    }
                }
                // A CHAR literal is `'x'` or `'\n'`. A lone `'` is a lifetime
                // (`&'a str`), and treating that as an opening quote would run
                // the skip to the next apostrophe anywhere in the file —
                // swallowing the `)` this walk exists to find. So the shape is
                // checked before anything is consumed.
                '\'' => {
                    let mut probe = chars.clone();
                    match (probe.next(), probe.next()) {
                        (Some((_, '\\')), _) => {
                            for (_, d) in chars.by_ref() {
                                if d == '\'' {
                                    break;
                                }
                            }
                        }
                        (Some(_), Some((_, '\''))) => {
                            chars.next();
                            chars.next();
                        }
                        _ => {} // a lifetime — ordinary code, not a literal
                    }
                }
                '/' if matches!(chars.peek(), Some((_, '/'))) => {
                    for (_, d) in chars.by_ref() {
                        if d == '\n' {
                            break;
                        }
                    }
                }
                '/' if matches!(chars.peek(), Some((_, '*'))) => {
                    chars.next();
                    let mut prev = '\0';
                    for (_, d) in chars.by_ref() {
                        if prev == '*' && d == '/' {
                            break;
                        }
                        prev = d;
                    }
                }
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = at + k;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(depth == 0, "unterminated `format!(` at byte {at} — the walker lost the file");
        let call = &src[at..=end];
        // Matched on THIS site's own opening literal, never on the call text:
        // the verdict notice nests the exempt gate clause inside itself, so a
        // `contains` here would have exempted the very site rev-2 F1b is about
        // — the enclosing call, whose summary field is agent-authored — because
        // one of its arguments is on the list. A guard that can be switched off
        // by what it encloses is not a guard.
        if NOTICE_SCRUB_EXEMPT.iter().any(|(anchor, _)| lit.starts_with(anchor)) {
            continue;
        }
        assert!(
            SCRUBS.iter().any(|s| call.contains(s)),
            "an `[orrerix] …` notice in mcp.rs interpolates text without naming a scrub, and is \
             not on NOTICE_SCRUB_EXEMPT. If its fields are all loomux-owned, add it there WITH \
             the reason; if any of them is agent-authored, scrub it (#891 rev-2 F1b).\n\n{call}"
        );
    }
    assert!(
        sites >= 4,
        "the scan found only {sites} notice sites — it has stopped matching the file it guards, \
         which is the way a source scan dies silently"
    );
}

/// rev-368 F2: the `gh` contract part 1 rests on, pinned at the source.
///
/// The failure mode this exists for is a **silent no-op**, which is why no
/// other test can catch it. `comments`/`reviews` are `#[serde(default)]` and
/// their timestamps are `Option`, so dropping `comments,reviews` from
/// `poll_intake`'s argv — or GitHub renaming `createdAt`/`submittedAt` —
/// degrades part 1 to "no PR has ever had any discussion": no parse error, no
/// log line, every other test still green, and
/// `parse_pr_list_without_comment_fields_reads_as_no_activity` positively
/// blesses that shape (correctly — it is the degradation contract). The
/// consequence is worse than a plain regression because part 2 rests on part
/// 1: a parked group would decay to its 24h ceiling while blind to the one
/// thing that happens to a parked group, a human commenting.
///
/// Verified against the real CLI at review time: `gh pr list --json
/// number,title,statusCheckRollup,comments,reviews` populates both arrays with
/// `createdAt`/`submittedAt` keys.
///
/// **The first half is asked of the VALUE, not of the source text (#778
/// landing).** #795 moved the argv out of `poll_intake` and into
/// `intake::pr_list_argv()` so its `--limit` could be pinned, which left the
/// original spelling of this assertion — a scan for the literal field string in
/// `mod.rs` — red on a poller that was still perfectly correct. Calling the
/// builder is the stronger claim anyway: it survives any reformatting of the
/// list. What the source scan still owns is the second half, the link the value
/// cannot see: that `poll_intake` reaches `gh` THROUGH that builder rather than
/// hand-rolling an argv beside it, which is how the fields could go missing
/// while `pr_list_argv` stayed green.
#[test]
fn poll_intake_still_asks_gh_for_comment_and_review_activity() {
    let argv = intake::pr_list_argv();
    for field in ["comments", "reviews"] {
        assert!(
            argv.iter().any(|a| a.split(',').any(|f| f == field)),
            "the `gh pr list` argv must keep asking for `{field}` (#864): without those two fields \
             every PR silently reads as having no discussion, and a parked group decays to its \
             ceiling while blind to a human commenting — with no parse error and no other failing \
             test. Got: {argv:?}"
        );
    }

    let src = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/orchestration/mod.rs"))
        .expect("read src/orchestration/mod.rs");
    assert!(
        src.contains("intake::pr_list_argv()"),
        "poll_intake must build its `gh pr list` argv through `intake::pr_list_argv` — an argv \
         spelled inline beside it would bypass both this pin and the fetch bound (#795), with \
         every other test still green"
    );

    // #888 A3 batch 11 moved `intake.rs` into `loomux-engine`; the scan follows
    // the file, because what it pins is a serde rename inside THAT file and no
    // other. Spelled the way `tests/groupid.rs` spells its second source root.
    let intake = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/loomux-engine/src/intake.rs"
    ))
    .expect("read crates/loomux-engine/src/intake.rs");
    for field in ["createdAt", "submittedAt"] {
        assert!(
            intake.contains(&format!("rename = \"{field}\"")),
            "intake.rs must keep deserializing `{field}` — the field name is GitHub's, not ours, \
             and a rename degrades to permanent silence rather than to an error"
        );
    }
}

/// #3249 item 1: the census the #3248 review round added counted renderer
/// functions in ONE hard-named file (`orchestration/mod.rs`) by ONE name
/// suffix — a shim renderer added in another module of `orchestration/`, or
/// in `loomux-engine`, escaped both. This census scans every production
/// source root the workspace has — `src-tauri/src`,
/// `crates/loomux-engine/src` and `crates/loomux-server/src` (#3249
/// residual 2: the server leaf had no shim code, but a renderer added there
/// would have gone unseen) — default-deny in the shape `tests/groupid.rs`
/// uses: the
/// anchor is name-independent — a rendered POSIX shim IS a `#!/bin/sh`
/// script, so every shebang on a code line must be a declared template on
/// SANCTIONED (exact whitespace-collapsed line + expected count + reason,
/// the ROOT_USES convention) — and the population must agree three ways:
/// template sites found == renderer functions found == entries in
/// PINNED_SHIMS.
///
/// Residuals, stated because a guard that implies completeness it does not
/// have is how the previous two versions of the join guard came to be
/// trusted while broken (tests/groupid.rs's identical honesty): a shim
/// script built by `format!` or `include_str!` carries no template-const
/// shebang line, so the shebang axis cannot see it — the name-based census
/// below is the SUPPLEMENT that catches it there (a #922-labelled
/// supplement, not the load-bearing axis). Two more, one per axis: the
/// shebang axis matches the literal `#!/bin/sh`, so a template opening
/// `#!/bin/bash` or `#!/usr/bin/env sh` escapes it; and the supplement
/// reads only `pub fn`/`pub(crate) fn` lines whose name sits on the
/// declaration line, so a private `fn` or a wrapped signature hides from
/// it (rev-final round 2 findings 2 and 4; rev-std round 1 finding 2).
///
/// Positive control (round 1): the scratch red added `fake_shim_sh` (with
/// a template const) under `loomux-engine` — outside every path the old
/// one-file census read — and this pin went red on the template-count
/// check. Round 2 closes the reviewer's follow-on: with the census lists
/// bumped alongside the fake renderer, THIS census goes green and the text
/// pin is what reddens (its array is built from PINNED_SHIMS), so the
/// bump-the-counts path dead-ends where the ts sites are checked. The same
/// control works from the third root: a fake renderer under
/// `crates/loomux-server/src` reds this pin's template-count check the
/// same way (#3249 residual 2).
///
/// #3259 item 2 closes the next abstraction up: the ROOTS list is no longer
/// the census's own word. The equality against the root manifest's
/// `[workspace] members` reddens when a crate joins the workspace without a
/// ROOTS entry — and when a ROOTS entry outlives its crate — so the
/// hand-maintained list can only drift with a red to say so.
#[test]
fn a_rendered_shim_renderer_cannot_hide_from_the_ts_pin() {
    /// Sanctioned shebang-bearing lines, exact text after whitespace
    /// collapse, each with its expected count and the reason it is allowed.
    const SANCTIONED: &[(&str, usize, &str)] = &[
        (
            "const TPL: &str = r#\"#!/bin/sh",
            3,
            "the gh/git/loomux shim templates — every pinned renderer renders from one",
        ),
        (
            "pub const COMPACT_HOOK_SCRIPT: &str = \"#!/bin/sh\\n\\",
            1,
            "the compact hook — event-driven marker files, no audit rows, no ts site",
        ),
    ];
    fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    // #3259 item 2: this list is no longer hand-maintained alone — the
    // equality below holds it to the Cargo workspace's own `[workspace]`
    // members. Adding a crate means editing both, and the equality fails
    // if either side moves alone.
    const ROOTS: &[(&str, &str)] = &[
        ("src-tauri", concat!(env!("CARGO_MANIFEST_DIR"), "/src")),
        (
            "loomux-engine",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../crates/loomux-engine/src"),
        ),
        (
            "loomux-server",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../crates/loomux-server/src"),
        ),
    ];
    // #3259 item 2: ROOTS must agree with the workspace's own member list —
    // the third root was missed for exactly this reason once (`crates/
    // loomux-server/src` had no shim code, so nothing forced the list to
    // name it), and a FOURTH crate hosting a shim renderer would escape the
    // same way, one abstraction up. The members are read from the root
    // manifest — the workspace's own declaration — and both sides are
    // canonicalized, so the comparison is between directories, not between
    // two spellings of one (`src-tauri` is a member whose ROOTS entry is
    // spelled `CARGO_MANIFEST_DIR/src`, not `…/../src-tauri/src`). If the
    // parser silently under-read, the equality would fail loudly (members
    // ≠ ROOTS), so it fails toward red, never toward green.
    let manifest =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("Cargo.toml");
    let manifest_src = std::fs::read_to_string(&manifest).unwrap_or_else(|e| {
        panic!(
            "cannot read the workspace root manifest ({}): {e}",
            manifest.display()
        )
    });
    let members = workspace_members(&manifest_src);
    let ws_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let canon = |p: &std::path::Path, what: &str| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|e| {
                panic!(
                    "{what} ({}) does not resolve as a directory ({e}) — the census \
                     scans real source roots, and a member with no `src` cannot \
                     host a renderer", p.display()
                )
            })
            .to_string_lossy()
            .replace('\\', "/")
    };
    let mut member_roots: Vec<String> = members
        .iter()
        .map(|m| canon(&ws_root.join(m).join("src"), &format!("workspace member `{m}`'s src")))
        .collect();
    let mut census_roots: Vec<String> = ROOTS
        .iter()
        .map(|(_, p)| canon(std::path::Path::new(*p), &format!("census root `{p}`")))
        .collect();
    member_roots.sort();
    census_roots.sort();
    assert_eq!(
        member_roots, census_roots,
        "the census ROOTS list and the Cargo workspace members have drifted — a \
         member with no ROOTS entry hosts shim renderers the census cannot see \
         (the exact miss #3249 residual 2 recorded), and a ROOTS entry with no \
         member scans a crate that no longer exists. Add the missing side (or \
         argue beside the per-root tripwire below why a member is deliberately \
         unscanned). members: {member_roots:?}; ROOTS: {census_roots:?}"
    );
    let mut files: Vec<(&str, std::path::PathBuf)> = Vec::new();
    for (label, root) in ROOTS {
        let mut found = Vec::new();
        collect_rs_files(std::path::Path::new(root), &mut found);
        // Asserted PER ROOT rather than on the total: a mistyped or stale
        // root contributes nothing and would hide behind the other root's
        // file count (the tests/groupid.rs rule).
        assert!(
            !found.is_empty(),
            "no `.rs` found under the {label} source root ({root}) — a root that scans \
             nothing is a tripwire that cannot fire"
        );
        files.extend(found.into_iter().map(|p| (*label, p)));
    }
    assert!(
        files.len() > 5,
        "the source scan found almost nothing — check the paths"
    );
    // The anchor, content-matched like tests/groupid.rs's GroupId check: a
    // file count cannot tell "both roots scanned" from "one root scanned
    // twice", so the scan must demonstrably reach the file that DEFINES the
    // renderers.
    assert!(
        files.iter().any(|(_, p)| std::fs::read_to_string(p)
            .is_ok_and(|s| s.contains("pub fn gh_shim_sh("))),
        "the scan never reached the file that defines the shim renderers — wherever \
         that file lives is a root this census must scan; add it to the ROOTS list"
    );
    let normalize = |line: &str| line.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut offenders: Vec<String> = Vec::new();
    let mut seen = vec![0usize; SANCTIONED.len()];
    let mut renderers: Vec<String> = Vec::new();
    for (label, path) in &files {
        let src = std::fs::read_to_string(path).unwrap();
        let name = format!("{label}/{}", path.file_name().unwrap().to_string_lossy());
        for (i, line) in src.lines().enumerate() {
            let trimmed = line.trim_start();
            // A comment may spell the shebang literally — several do.
            if trimmed.starts_with("//") {
                continue;
            }
            // The name-based SUPPLEMENT (a labelled heuristic, per #922): a
            // `pub fn`/`pub(crate) fn` whose name ends in `_shim_sh` is a
            // renderer candidate for the population checks below.
            for kw in ["pub fn ", "pub(crate) fn "] {
                if let Some(rest) = trimmed.strip_prefix(kw) {
                    let end = rest
                        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                        .unwrap_or(rest.len());
                    if rest[..end].ends_with("_shim_sh") {
                        renderers.push(rest[..end].to_string());
                    }
                }
            }
            if trimmed.contains("#!/bin/sh") {
                match SANCTIONED.iter().position(|(text, _, _)| *text == normalize(line)) {
                    Some(idx) => seen[idx] += 1,
                    None => offenders.push(format!("{name}:{}: {trimmed}", i + 1)),
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a `#!/bin/sh` script must be a declared template on the SANCTIONED list \
         (default-deny, the tests/groupid.rs convention). Found {} unplanned site(s):\n{}\n\n\
         If one is legitimate, add its exact whitespace-collapsed line to SANCTIONED with \
         an expected count and a reason — that argument is the point of this test.",
        offenders.len(),
        offenders.join("\n")
    );
    for (idx, (text, expected, whose)) in SANCTIONED.iter().enumerate() {
        assert_eq!(
            seen[idx],
            *expected,
            "expected exactly {expected} occurrence(s) of `{text}` — {whose} — found {}. \
             Zero means the declared template was renamed or deleted and this scan is \
             watching nothing; more than the expected count means a new shebang site grew \
             without being argued in.",
            seen[idx]
        );
    }
    // The population must agree three ways — templates, renderer functions,
    // and the entries the text pin above covers.
    assert_eq!(
        seen[0],
        PINNED_SHIMS.len(),
        "the pinned shim templates must equal the {} entries the text pin's array carries",
        PINNED_SHIMS.len()
    );
    assert_eq!(
        renderers.len(),
        PINNED_SHIMS.len(),
        "the renderer census found {renderers:?} but the pin covers {} — every renderer \
         must render from a template this pin knows about, AND its ts sites must be \
         checked by the text pin above: add the renderer there too (the pin's array is \
         built from PINNED_SHIMS, so a name with no rendering arm reds \
         every_rendered_shim_timestamps_with_the_portable_ms_fallback). A \
         format!-built or include_str!-built shim script carries no shebang line and \
         hides from the shebang axis, which is why this name-based supplement exists \
         (#922, #3249).",
        PINNED_SHIMS.len()
    );
    for name in PINNED_SHIMS {
        assert!(
            renderers.contains(&format!("{name}_shim_sh")),
            "the pin covers `{name}` but no renderer named `{name}_shim_sh` is declared — \
             the census and the renderers have drifted"
        );
    }
}

/// #3259 item 2 — the `[workspace] members` array from the ROOT manifest,
/// hand-parsed: reading one array out of one file is not worth a TOML
/// dependency, and a new dependency is an audit argument the
/// src-tauri/Cargo.toml notes would have to carry. The parser is
/// deliberately narrow and fails LOUD on anything it was not written for —
/// a multi-line array, a glob member, a missing section — because a silent
/// under-read would feed the equality above an empty list that then fails
/// loudly rather than green (it fails toward red, never toward green).
fn workspace_members(manifest: &str) -> Vec<String> {
    let mut in_workspace = false;
    for line in manifest.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_workspace = t == "[workspace]";
            continue;
        }
        if !in_workspace {
            continue;
        }
        let Some(rest) = t.strip_prefix("members") else { continue };
        let Some(rest) = rest.trim_start().strip_prefix('=') else { continue };
        let arr = rest.trim();
        let (Some(open), Some(close)) = (arr.find('['), arr.rfind(']')) else {
            panic!(
                "`members` in the root [workspace] is not a single-line array — \
                 extend workspace_members to read the new shape: {t:?}"
            );
        };
        let inner = &arr[open + 1..close];
        if inner.contains('*') || inner.contains('?') {
            panic!(
                "`members` carries a glob — a glob cannot be tied to census ROOTS \
                 without resolving it; spell the members out: {t:?}"
            );
        }
        let parsed: Vec<String> = inner
            .split(',')
            .map(|m| m.trim().trim_matches('"').trim_matches('\''))
            .filter(|m| !m.is_empty())
            .map(|m| m.to_string())
            .collect();
        assert!(
            !parsed.is_empty(),
            "`members` parsed to an empty list — the census cannot tie ROOTS to \
             a workspace that declares none: {t:?}"
        );
        return parsed;
    }
    panic!(
        "no `[workspace] members` in the root manifest — the census cannot tie \
         ROOTS to a workspace that declares none"
    );
}

/// Whether `line` contains a REAL (live-code) call to `OrchRegistry::new(`,
/// as opposed to one merely mentioned inside a comment or a string literal
/// (a doc comment explaining this very guard, or this guard's own assertion
/// message, both name the construct literally).
///
/// #464 B2 review: the first cut of this guard filtered out any line
/// containing ANY `"` at all — which also, wrongly, excluded a perfectly
/// live construction such as `OrchRegistry::new(d.path().join("state"))`,
/// where the quote belongs to an ARGUMENT, not to something wrapping the
/// construct itself. Mutation-verified to catch exactly that shape: compare
/// where the construct text starts against where the line's first `"`
/// starts. If the construct starts first (or there is no quote at all on
/// the line), it's live code; if a quote opens before it, the construct
/// text is inside a string/doc-comment payload, not actually calling
/// anything.
fn is_live_registry_construction(line: &str) -> bool {
    if line.trim_start().starts_with("//") {
        return false; // a full-line comment can still literally spell the construct
    }
    // #502: a construction the test's own SUBJECT requires the helper not to
    // make — either a registry built the unguarded way precisely to prove
    // containment now holds without the helper, or one rooted somewhere the
    // helper cannot produce (a path that is not a directory, to force a real
    // enumeration failure).
    //
    // This pin's premise is that a raw `OrchRegistry::new(...)` silently
    // falls back to the user's REAL agent dirs. #502 removed that fallback
    // (`is_live_registry`: only the registry rooted at `default_root()` ever
    // resolves a home directory), so the premise no longer holds for these
    // few — but the pin is kept, and kept strict, because it also catches
    // the accidental case the containment rule is not the only defense
    // against. Marking a line is deliberately noisy and greppable so this
    // stays a handful of reviewed exceptions rather than a habit: a test
    // that is not ABOUT bare construction still routes through the helper.
    if line.contains(BARE_REGISTRY_CONTAINMENT_PIN) {
        return false;
    }
    match (line.find("OrchRegistry::new("), line.find('"')) {
        (Some(construct_at), Some(quote_at)) => construct_at < quote_at,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// The opt-out token for [`is_live_registry_construction`]. Written split so
/// this declaration is not itself a match when the pin scans this file.
const BARE_REGISTRY_CONTAINMENT_PIN: &str = concat!("containment", "-pin (#502)");

/// #464 B2: `claude_agents_dir_override`/`copilot_agents_dir_override` are
/// in-memory fields on an `OrchRegistry` instance, not persisted state, so a
/// registry built with a bare `OrchRegistry::new(...)` — skipping whichever
/// file's `relaunch_registry`/`test_registry` helper applies them — silently
/// falls back to the REAL `dirs::home_dir()/.claude/agents` /
/// `.../.copilot/agents` on its first spawn. That gap is exactly how the
/// orchestration suite's many "relaunch" tests (a second `OrchRegistry::new`
/// pointed at the same or a related state root) left 1,111 stray
/// `loomux-<group>-*.md` files under a real dev machine's `~/.claude/agents`
/// and 161 under `~/.copilot/agents` — found live on this machine while
/// fixing this issue. That can't be safely re-proven at runtime (probing it
/// would mean either touching the real home directory or faking `HOME`,
/// which only proves the fallback exists, not that nothing here still uses
/// it) — so this pins the invariant statically instead.
///
/// Reads every `tests/*.rs` file from disk at runtime (not `include_str!`,
/// which only sees the ONE file it's compiled into) so this covers the
/// whole integration-test suite in one place, not just this file: review
/// found `tests/lessonsfile.rs` and `tests/prompts.rs` each construct their
/// own `OrchRegistry` too (their own `test_registry()` helper, currently
/// safe but entirely unpinned) — a future edit to either, or a brand new
/// test file, reopened the leak with zero guard.
#[test]
fn no_registry_construction_bypasses_the_test_agent_dir_overrides() {
    // Recurse into subdirectories, not just the top level of `tests/`: the
    // standard `tests/common/mod.rs`-style shared-helper layout (or any
    // other subdirectory Rust's test harness recognizes) would otherwise
    // sit outside this pin's scan entirely (#464 round-2 review N1) — the
    // exact "partial enforcement, next one slips through the gap" shape
    // this whole ticket has been about. `walkdir`-free: `tests/` is a
    // handful of files, not worth a dependency for.
    fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    let tests_dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests"));
    // Exactly one file legitimately needs a raw `OrchRegistry::new(...)` per
    // registry-construction helper it defines. Add a file here ONLY when it
    // gains a helper of its own — every other construction, in every file
    // at every depth (including brand new ones this list has never heard
    // of), must be zero. Keyed by path RELATIVE to `tests/` (forward
    // slashes), not bare filename, so a same-named file in a subdirectory
    // can never collide with a top-level one.
    let sanctioned: &[(&str, usize)] = &[
        ("groupid.rs", 1),       // registry_at
        ("orchestration.rs", 1), // relaunch_registry
        ("workflow.rs", 1),      // relaunch_registry
        ("lessonsfile.rs", 1),   // test_registry
        ("prompts.rs", 1),       // test_registry
        ("perf_leaflocks.rs", 1), // test_registry
        ("opencodeusage.rs", 1), // test_registry
        // #2126 P3, the pi twin of the opencode row above. Same reason it is its
        // own binary (helpers do not cross integration-test targets), and it
        // carries the #1778 row convention rather than the older bare one: its
        // own `its_registry_helper_applies_every_override_this_allowlist_row_assumes`
        // asserts the helper really applies all four agent/hook dir overrides,
        // so this row's premise fails loudly in that binary if the helper ever
        // stops.
        ("piusage.rs", 1),       // test_registry (#2126 P3) — proof test above
        ("opencodesessions.rs", 1), // registry_at
        ("opencodedigest.rs", 1), // test_registry
        ("rootreg.rs", 1),        // registry_at
        ("manager_lifecycle.rs", 1), // relaunch_registry
        ("manager_prose.rs", 1),  // test_registry (#1161 M4)
        ("selfwatch.rs", 1),      // test_registry (#1601 Phase 0)
        ("liveness.rs", 1),       // test_registry (#1607, epic #1600)
        ("e2ehold_guard.rs", 1), // registry_at (#1603)
        ("views.rs", 1),          // test_registry (#1608)
        ("liveness.rs", 1),       // test_registry (#1608 L0/L1)
        // #1778 S3/S4. One row, and it is a DECISION to widen a default-deny
        // surface, so it carries the reason a later reader would need to
        // re-check it rather than only the helper's name.
        //
        // **Why this file needs its own helper at all**, which is the part that
        // could stop being true: a `tests/*.rs` file is its own integration-test
        // BINARY, and helpers do not cross binaries — `reviewdrive.rs` cannot
        // call `orchestration.rs`'s `relaunch_registry` any more than
        // `workflow.rs` can, which is why both of those already have a row here.
        // It is a separate target rather than more of `orchestration.rs` for
        // CLAUDE.md constraint 4's reason (an integration-test target is what
        // carries the comctl32-v6 manifest link args) and to stay off that
        // file's end-of-file append-conflict surface.
        //
        // **The proof this row names**, so the row can go stale rather than
        // merely be trusted: that file's own
        // `its_registry_helper_applies_every_override_this_allowlist_row_assumes`
        // asserts the helper really does apply all four agent/hook dir
        // overrides — which is the whole property #464 is about. If the helper
        // ever stops applying one, that test fails in its own binary and this
        // row's premise is gone with it.
        //
        // The per-line `containment-pin (#502)` marker is deliberately NOT used
        // here: that opt-out means "this test's subject requires the unguarded
        // construction", which is false of this file. The construction here is
        // the helper itself, which is exactly what this allowlist is for.
        ("reviewdrive.rs", 1),    // relaunch_registry (#1778 S3/S4) — see above
        // #2519 slice A. Its own binary for `manager_lifecycle.rs`'s two
        // reasons (one subject; off this file's end-of-file append-conflict
        // surface), and therefore its own helper: helpers do not cross
        // integration-test binaries. The helper applies all four agent/hook dir
        // overrides — which is the property #464 is about — and the row is
        // checked against that by this scan alone, so a reader re-checking it
        // reads `lead.rs`'s `relaunch_registry` itself.
        ("lead.rs", 1),           // relaunch_registry (#2519 slice A)
        // #3304 S1, delivery triage. Its own binary for the `reviewdrive.rs`
        // row's two reasons (helpers do not cross integration-test targets, and
        // CLAUDE.md constraint 4 makes the target KIND what matters), plus one
        // of this feature's own: `docs/design/delivery-triage.md` says every
        // path that can hold a notice back lives in one file, and a test target
        // that reads it as one scope is the other half of that.
        //
        // **The proof this row names**, so it can go stale rather than merely be
        // trusted: `tests/triage.rs`'s own
        // `its_registry_helper_applies_every_override_this_allowlist_row_assumes`
        // asserts the helper really does apply all four agent/hook dir
        // overrides — the property #464 is about. If it ever stops applying one,
        // that test fails in its own binary and this row's premise is gone.
        ("triage.rs", 1),         // relaunch_registry (#3304 S1) — proof test above
        // #3040 P3a, the plan driver's twin of the `reviewdrive.rs` row above.
        // Its own binary for that row's two reasons (helpers do not cross
        // integration-test targets, and CLAUDE.md constraint 4 makes the target
        // KIND what matters), and therefore its own helper.
        //
        // **The proof this row names**, so it can go stale rather than merely be
        // trusted: `tests/plandrive.rs`'s own
        // `its_registry_helper_applies_every_override_this_allowlist_row_assumes`
        // asserts the helper really does apply all four agent/hook dir
        // overrides. If it ever stops applying one, that test fails in its own
        // binary and this row's premise is gone with it.
        ("plandrive.rs", 1),      // test_registry (#3040 P3a) — see above
        // #2515 C3, the codex twin of the `piusage.rs` row above. Same reason it
        // is its own binary (helpers do not cross integration-test targets),
        // and it carries the #1778 row convention: its own
        // `its_registry_helper_applies_every_override_this_allowlist_row_assumes`
        // asserts the helper really applies all four agent/hook dir overrides,
        // so this row's premise fails loudly in that binary if it ever stops.
        ("codexusage.rs", 1),     // test_registry (#2515 C3) — proof test in that file
        // #3263 S2. `tests/todo.rs` was S1's own binary (a new file rather than
        // an append to this one, for the end-of-file-append conflict class
        // CLAUDE.md's git section names), and S1 needed no registry at all: it
        // drove `apply_to`/`snapshot_at` against explicit paths. The MCP tools
        // cannot — they go through `OrchRegistry::todo_apply` — so that binary
        // gains a registry, and therefore its own helper, since helpers do not
        // cross integration-test targets.
        //
        // **The proof this row names**, so it can go stale rather than merely
        // be trusted: that file's own
        // `its_registry_helper_applies_every_override_this_allowlist_row_assumes`
        // spawns through the helper's registry and asserts all four agent/hook
        // dir overrides really took — which is the property #464 is about. If
        // the helper ever stops applying one, that test fails in its own binary
        // and this row's premise is gone with it.
        ("todo.rs", 1),           // relaunch_registry (#3263 S2) — proof test in that file
    ];
    let mut files = Vec::new();
    collect_rs_files(tests_dir, &mut files);
    let mut checked = 0;
    for path in &files {
        let rel = path
            .strip_prefix(tests_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let sites: Vec<&str> = src.lines().filter(|l| is_live_registry_construction(l)).collect();
        let expected = sanctioned.iter().find(|(f, _)| *f == rel).map(|(_, n)| *n).unwrap_or(0);
        assert_eq!(
            sites.len(),
            expected,
            "tests/{rel}: expected exactly {expected} raw `OrchRegistry::new(...)` (its own \
             sanctioned registry-construction helper, if any) — found {}. Every OTHER \
             construction must route through that file's `relaunch_registry`/`test_registry` \
             helper, or it leaks a generated agent file into the real ~/.claude or \
             ~/.copilot agents dir on its first spawn (#464). If this file is meant to gain its \
             own helper, add it to `sanctioned` above with its count (using its path relative to \
             `tests/`). All live sites found: {sites:?}",
            sites.len(),
        );
        checked += 1;
    }
    assert!(
        checked >= sanctioned.len(),
        "expected to check at least {} tests/**/*.rs files, only found {checked} — did the tests/ dir move?",
        sanctioned.len()
    );
}

/// The registration invariant #406 exists to hold: app setup starts ONE
/// `gh`-polling loop, and neither of the two entry points it replaced.
/// Parsed out of `src/lib.rs` itself — the actual setup block, not a hand
/// transcription of it (the `generate_handler_matches_app_commands`
/// precedent in `tests/acl_manifest.rs`) — because "how many threads call
/// `gh`" is a property of that call list and of nothing else: any future
/// `start_*_poller` added beside it would restore exactly the unaccounted
/// second budget-spender this issue closed, and no other test in this suite
/// would notice.
#[test]
fn app_setup_starts_exactly_one_gh_polling_loop() {
    let src = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs")).expect("read src/lib.rs");
    let calls = |needle: &str| src.matches(needle).count();
    assert_eq!(
        calls("start_gh_poller("),
        1,
        "src/lib.rs must start the unified gh poller exactly once (#406)"
    );
    assert_eq!(
        calls("start_notify_poller("),
        0,
        "the notify-only poller is gone — its work is a half of `start_gh_poller`'s tick (#406)"
    );
    assert_eq!(
        calls("start_intake_poller("),
        0,
        "the intake-only poller is gone — a second gh-polling thread is the coupling #406 closed"
    );
}

#[test]
fn every_shim_name_ensure_shims_writes_is_one_the_prune_keeps() {
    // A shim written under a name missing from GENERATED_SHIM_NAMES would be
    // deleted by the prune on the very spawn that wrote it — silently, every
    // spawn. Read off the write calls' shape (the program argument is a literal
    // at every site; `tests/pathseg.rs` and `tests/rebrand.rs` rely on the same).
    let src = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/orchestration/mod.rs")).unwrap();
    let mut written: Vec<String> = Vec::new();
    for call in ["self.write_shim(&dir, \"", "self.write_refusal_shim(&dir, \""] {
        for chunk in src.split(call).skip(1) {
            written.push(chunk.chars().take_while(|c| *c != '"').collect());
        }
    }
    written.sort();
    written.dedup();
    // Population control: the four names ensure_shims writes today — and the
    // equality makes a stale constant entry fail as loudly as a missing one.
    let mut names: Vec<String> = GENERATED_SHIM_NAMES.iter().map(|s| s.to_string()).collect();
    names.sort();
    assert_eq!(written, names, "the shim names written and the names the prune keeps must be one set");
}

/// Every `#[tauri::command]` site in `src/orchestration/mod.rs`, as
/// `(name, is_async, body)`.
///
/// A source scan, like `gh.rs`'s own `every_tauri_command_in_this_module_is_
/// async_and_delegates` (#724) — same mechanics, applied to the orchestration
/// module. Read from `CARGO_MANIFEST_DIR` rather than `include_str!` so a
/// tens-of-thousands-of-lines module is not baked into the test binary.
///
/// A chunk counts only when the first non-attribute line after the marker is
/// the `pub fn` / `pub async fn` itself, so the marker appearing inside a doc
/// comment (it does — `run_blocking`'s doc names it) is skipped rather than
/// mis-attributed to whatever function happens to follow.
fn orchestration_command_sites() -> Vec<(String, bool, String)> {
    let src = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/orchestration/mod.rs"),
    )
    .expect("src/orchestration/mod.rs must be readable from the manifest dir");
    // Split so this test's own source never matches the marker it scans for.
    let marker = concat!("#[tauri::", "command]");
    let mut out = Vec::new();
    for chunk in src.split(marker).skip(1) {
        let mut sig: Option<&str> = None;
        for line in chunk.lines() {
            let t = line.trim_start();
            if t.is_empty() || t.starts_with("#[") || t.starts_with("#!") {
                continue;
            }
            sig = Some(t);
            break;
        }
        let Some(sig) = sig else { continue };
        let is_async = sig.starts_with("pub async fn ");
        let Some(after) = sig
            .strip_prefix("pub async fn ")
            .or_else(|| sig.strip_prefix("pub fn "))
        else {
            continue; // not a command signature (a doc-comment mention)
        };
        let Some(paren) = after.find('(') else { continue };
        let name = after[..paren].trim().to_string();
        // The body runs from the signature to the first closing brace in
        // column 0 — top-level items always close there.
        let from_sig = &chunk[chunk.find(sig).unwrap_or(0)..];
        let body = match from_sig.find("\n}") {
            Some(end) => from_sig[..end].to_string(),
            None => from_sig.to_string(),
        };
        out.push((name, is_async, body));
    }
    out
}

/// The polled orchestration commands #743 S4c moves off the webview main
/// thread: each must be an `async fn` whose body actually delegates.
///
/// Both halves matter and #724's mutation round is why: `async` alone does not
/// move the work, because Tauri polls a command's future on the main thread —
/// an `async fn` that calls its sync body inline is exactly as blocking as the
/// sync command it replaced.
///
/// **Scope: an allow-list of eight, NOT set equality over the module** — and
/// that is a smaller guarantee than the `gh.rs`/`git.rs` scans this borrows its
/// mechanics from. Those assert over every command they contain, so a new
/// offender forces a deliberate update. This one pins that these eight *stay*
/// converted and says nothing about a ninth polled sync command landing later.
/// Refusing a new unargued sync command anywhere in the crate is **E1's** job
/// (`perf_dispatch.rs` + its `SYNC_COMMANDS` manifest, performance.md INV-1),
/// which is slice **S2** — so a reader who wants that half should look there
/// and not mistake this test for it (rev-231 finding 1).
#[test]
fn the_polled_orchestration_commands_are_async_and_delegate_off_thread() {
    const OFF_THREAD: &[&str] = &[
        "orch_pause_group",
        "orch_resume_group",
        "orch_group_usage",
        "orch_autonomy",
        "orch_workflow_status",
        "orch_tasks",
        "orch_audit",
        "orch_merge_queue",
    ];
    let sites = orchestration_command_sites();

    // Vacuity guards. A marker that stopped matching, or a module that moved,
    // must fail loudly rather than pass over nothing.
    assert!(
        sites.len() >= 60,
        "the scan found only {} command sites in orchestration/mod.rs — the marker or the \
         signature shape has drifted, and every assertion below would be vacuous",
        sites.len()
    );
    let (_, autopilot_async, _) = sites
        .iter()
        .find(|(n, _, _)| n == "agent_autopilot_flags")
        .cloned()
        .expect("agent_autopilot_flags is a command site");
    assert!(
        !autopilot_async,
        "the scan must be able to tell a sync command from an async one; \
         `agent_autopilot_flags` is a static table lookup with nothing to delegate, \
         so it is the counter-specimen"
    );

    for want in OFF_THREAD {
        let (_, is_async, body) = sites
            .iter()
            .find(|(n, _, _)| n == want)
            .cloned()
            .unwrap_or_else(|| panic!("{want} must exist as a command site"));
        assert!(
            is_async,
            "#743 S4c: `{want}` is polled on a fixed cadence and does blocking I/O, so it must \
             be an `async fn` — a sync command is dispatched directly on the webview thread"
        );
        assert!(
            body.contains("run_blocking("),
            "#743 S4c: `{want}` is `async` but its body never reaches `run_blocking(` — Tauri \
             polls the future on the main thread, so an inline body is still main-thread work \
             (#724's mutation finding). Body:\n{body}"
        );
    }
}

/// The source half of the same guarantee: `mcp.rs` — the whole of what an
/// agent can reach — contains no call to the answer entry point and never
/// names the source type. A behavioural sweep can only drive tools that exist
/// today; this is what a slice adding one tomorrow trips over.
#[test]
fn the_mcp_surface_has_no_path_to_the_answer_entry_point() {
    let mcp = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/orchestration/mcp.rs"))
        .expect("read mcp.rs");
    assert!(
        !mcp.contains("answer_question("),
        "mcp.rs calls answer_question — the MCP surface is agent-reachable, so an answer tool \
         there would let an agent settle the question a HUMAN was asked (#946). Answers enter \
         only through trusted surfaces; add an AnswerSource variant and its own entry point."
    );
    assert!(
        !mcp.contains("AnswerSource"),
        "mcp.rs names AnswerSource — see above; the agent-reachable surface must not be able to \
         construct an answer provenance."
    );

    // Nothing else in the backend may become an answering surface without this
    // test noticing: the type is defined in `humanq.rs` and used in `mod.rs`
    // (the trusted `orch_question_answer` command), and nowhere else.
    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    collect_rs(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")), &mut files);
    assert!(files.len() > 10, "the source scan found almost nothing — did the tree move?");
    let mut mentions: Vec<String> = files
        .iter()
        .filter(|p| fs::read_to_string(p).is_ok_and(|s| s.contains("AnswerSource")))
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .collect();
    mentions.sort();
    assert_eq!(
        mentions,
        vec!["humanq.rs".to_string(), "mod.rs".to_string()],
        "AnswerSource escaped its two homes (humanq.rs defines it; mod.rs's \
         orch_question_answer supplies it). A new file naming it is a new answering surface — \
         which may be right (#947's bridge is planned), but is never accidental: read \
         humanq.rs's trust-boundary section, then update this list deliberately."
    );

    // ---- The closed SET of answer sources ----
    //
    // The two assertions above pin WHERE the type may be named. Neither pins
    // WHAT it can spell, and that is the boundary's actual load-bearing claim:
    // "there is no variant for an agent, and adding one would defeat the
    // feature rather than extend it." Without this, `AnswerSource::Agent` could
    // be added inside `humanq.rs` — a legitimate home — and every check above
    // would stay green while the trust boundary quietly acquired an
    // agent-shaped source.
    //
    // Read off the declaration rather than matched in Rust on purpose: an
    // exhaustive `match` would fail to COMPILE on a new variant, and a compile
    // error is the one failure that tells a reader nothing about behaviour.
    // This fails with a sentence instead.
    let humanq_src =
        fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/orchestration/humanq.rs"))
            .expect("read humanq.rs");
    let body = humanq_src
        .split_once("pub enum AnswerSource {")
        .and_then(|(_, rest)| rest.split_once('}'))
        .map(|(body, _)| body.to_string())
        .expect(
            "AnswerSource's declaration is no longer findable — it was renamed, moved, or grew a \
             struct variant with its own braces. Whichever it is, this pin must be re-aimed \
             deliberately rather than left silently matching nothing.",
        );
    let mut variants: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//"))
        .map(|l| l.trim_end_matches(',').to_string())
        .collect();
    variants.sort();
    assert_eq!(
        variants,
        vec!["Webview".to_string()],
        "the set of spellable answer sources changed. EVERY variant of AnswerSource is a way an \
         answer can enter the registry, so this list IS the trust boundary — adding one grants a \
         new party the power to settle a question the human was asked. A trusted surface (#947's \
         paired chat bridge) is a legitimate addition; anything an AGENT can reach is not, and \
         no variant may ever be constructible from mcp.rs. Update this list only alongside \
         humanq.rs's trust-boundary section."
    );
}

/// The same source half for DISMISSAL (#2137) — a second power over the same
/// registry, so a second guard rather than a widened one.
///
/// The two are kept apart deliberately. `AnswerSource` and `DismissSource` are
/// separate closed sets precisely so that a surface trusted to answer does not
/// silently acquire the right to clear the human's queue, and a test that
/// folded them together would be the place that difference stopped being
/// checkable.
#[test]
fn the_mcp_surface_has_no_path_to_the_dismiss_entry_point() {
    let mcp = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/orchestration/mcp.rs"))
        .expect("read mcp.rs");
    assert!(
        !mcp.contains("dismiss_question("),
        "mcp.rs calls dismiss_question — the MCP surface is agent-reachable, so a dismiss tool \
         there would let an agent clear a question the HUMAN was asked, on the human's behalf \
         (#2137). An agent whose own question went stale has withdraw_question, which settles it \
         visibly as `withdrawn`."
    );
    assert!(
        !mcp.contains("DismissSource"),
        "mcp.rs names DismissSource — see above; the agent-reachable surface must not be able to \
         construct a dismissal provenance."
    );

    // Nothing else in the backend may become a dismissing surface without this
    // test noticing: the type is defined in `humanq.rs` and used in `mod.rs`
    // (the trusted `orch_question_dismiss` command), and nowhere else.
    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    collect_rs(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")), &mut files);
    assert!(files.len() > 10, "the source scan found almost nothing — did the tree move?");
    let mut mentions: Vec<String> = files
        .iter()
        .filter(|p| fs::read_to_string(p).is_ok_and(|s| s.contains("DismissSource")))
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .collect();
    mentions.sort();
    assert_eq!(
        mentions,
        vec!["humanq.rs".to_string(), "mod.rs".to_string()],
        "DismissSource escaped its two homes (humanq.rs defines it; mod.rs's \
         orch_question_dismiss supplies it). A new file naming it is a new dismissing surface — \
         never accidental: read humanq.rs's trust-boundary section, then update this list \
         deliberately."
    );

    // ---- The closed SET of dismiss sources ----
    //
    // Read off the declaration rather than matched in Rust, for the answer
    // guard's reason: an exhaustive `match` fails to COMPILE on a new variant,
    // and a compile error is the one failure that tells a reader nothing about
    // behaviour. This fails with a sentence instead.
    let src =
        fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/orchestration/humanq.rs"))
            .expect("read humanq.rs");
    let body = src
        .split_once("pub enum DismissSource {")
        .and_then(|(_, rest)| rest.split_once('}'))
        .map(|(body, _)| body.to_string())
        .expect(
            "DismissSource's declaration is no longer findable — it was renamed, moved, or grew \
             a struct variant with its own braces. Whichever it is, this pin must be re-aimed \
             deliberately rather than left silently matching nothing.",
        );
    let mut variants: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//"))
        .map(|l| l.trim_end_matches(',').to_string())
        .collect();
    variants.sort();
    assert_eq!(
        variants,
        vec!["Webview".to_string()],
        "the set of spellable dismiss sources changed. Every variant is a party empowered to \
         clear a question the human was asked WITHOUT answering it, which is a different power \
         from answering and is why this list is not AnswerSource's. Nothing an agent can reach \
         belongs here."
    );
}

/// The backend's demo-gate set is a MIRROR of the board's, and this is what
/// stops the two spellings from drifting.
///
/// Scanned rather than duplicated: the frontend list is the one the board's own
/// chips and pickers read, so a status added there and not here would leave a
/// task parked on a human with no item ever raised. The scan resolves the
/// `PROTOTYPE_STATUS` identifier the TS list is written with, and PANICS rather
/// than passing if it cannot find either declaration — a guard that silently
/// watches nothing is worse than no guard.
#[test]
fn the_backend_demo_gate_set_matches_the_boards() {
    const TASKBOARD: &str = include_str!("../../../src/taskboard.ts");

    fn const_string(src: &str, name: &str) -> String {
        let decl = format!("export const {name} = \"");
        let start = src
            .find(&decl)
            .unwrap_or_else(|| panic!("src/taskboard.ts no longer declares `{name}` as a string \
                                       literal — this guard cannot resolve the demo-gate set"))
            + decl.len();
        let rest = &src[start..];
        rest[..rest.find('"').expect("unterminated string literal")].to_string()
    }

    let prototype = const_string(TASKBOARD, "PROTOTYPE_STATUS");
    let decl = "export const DEMO_STATUSES = [";
    let start = TASKBOARD
        .find(decl)
        .expect("src/taskboard.ts no longer declares `DEMO_STATUSES` — this guard watches nothing")
        + decl.len();
    let rest = &TASKBOARD[start..];
    let body = &rest[..rest.find(']').expect("unterminated DEMO_STATUSES array")];

    let front: Vec<String> = body
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|tok| match tok.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
            Some(lit) => lit.to_string(),
            // The only identifier the list is written with today. A NEW one
            // panics rather than being skipped, so the guard cannot quietly
            // start comparing a shorter list.
            None if tok == "PROTOTYPE_STATUS" => prototype.clone(),
            None => panic!(
                "DEMO_STATUSES member {tok:?} is neither a string literal nor a known constant — \
                 teach this guard to resolve it rather than letting it compare a short list"
            ),
        })
        .collect();

    assert!(!front.is_empty(), "the parsed frontend set must not be empty");
    let mut back: Vec<String> =
        loomux_lib::orchestration::DEMO_GATED_STATUSES.iter().map(|s| s.to_string()).collect();
    let mut front_sorted = front.clone();
    back.sort();
    front_sorted.sort();
    assert_eq!(
        back, front_sorted,
        "DEMO_GATED_STATUSES (src-tauri) and DEMO_STATUSES (src/taskboard.ts) have drifted — a \
         status parked on the human in one and not the other is a demo whose needs-you item is \
         never raised (#1151)"
    );
    // And the predicate agrees with its own set, in both directions.
    for status in &front {
        assert!(loomux_lib::orchestration::is_demo_gated(status), "{status} must be gated");
    }
    assert!(!loomux_lib::orchestration::is_demo_gated("in-progress"));
    assert!(!loomux_lib::orchestration::is_demo_gated(""));
}

/// The source half of the same guarantee: `mcp.rs` — the whole of what an agent
/// can reach — contains no call to the resolve entry point and never names the
/// source type. A behavioural sweep can only drive tools that exist today; this
/// is what a slice adding one tomorrow trips over.
#[test]
fn the_mcp_surface_has_no_path_to_the_item_resolve_entry_point() {
    let mcp = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/orchestration/mcp.rs"))
        .expect("read mcp.rs");
    assert!(
        !mcp.contains("resolve_needs_you"),
        "mcp.rs names resolve_needs_you — the MCP surface is agent-reachable, so a resolve tool \
         there would let an agent certify that the HUMAN had looked at something (#1151). \
         Resolves enter only through trusted surfaces; add a ResolveSource variant and its own \
         entry point. (Prose does not get an exemption: see the note on the needs-you arms.)"
    );
    assert!(
        !mcp.contains("ResolveSource"),
        "mcp.rs names ResolveSource — see above; the agent-reachable surface must not be able to \
         construct a resolve provenance."
    );
    // #2137. A SEPARATE assertion rather than a widened one: the two entry
    // points have different names, so `!contains("resolve_needs_you")` says
    // nothing at all about the dismiss one, and a slice wiring a dismiss tool
    // in would leave every check above green.
    assert!(
        !mcp.contains("dismiss_needs_you"),
        "mcp.rs names dismiss_needs_you — the MCP surface is agent-reachable, so a dismiss tool \
         there would let an agent clear the human's own queue on their behalf (#2137). An agent \
         whose ask went stale has withdraw_attention, which settles it visibly as a withdrawal."
    );

    // Nothing else in the backend may become a resolving surface without this
    // test noticing: the type is defined in `needsyou.rs` and used in `mod.rs`
    // (the trusted `orch_needs_you_resolve` command), and nowhere else.
    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    collect_rs(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")), &mut files);
    assert!(files.len() > 10, "the source scan found almost nothing — did the tree move?");
    let mut mentions: Vec<String> = files
        .iter()
        .filter(|p| fs::read_to_string(p).is_ok_and(|s| s.contains("ResolveSource")))
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .collect();
    mentions.sort();
    assert_eq!(
        mentions,
        vec!["mod.rs".to_string(), "needsyou.rs".to_string()],
        "ResolveSource escaped its two homes (needsyou.rs defines it; mod.rs's \
         orch_needs_you_resolve supplies it). A new file naming it is a new resolving surface — \
         which may one day be right, but is never accidental: read needsyou.rs's resolve-boundary \
         section, then update this list deliberately."
    );

    // ---- The closed SET of resolve sources ----
    //
    // The two assertions above pin WHERE the type may be named. Neither pins
    // WHAT it can spell, and that is the boundary's actual load-bearing claim:
    // there is no variant for an agent, and adding one would defeat the feature
    // rather than extend it. Without this, `ResolveSource::Agent` could be added
    // inside `needsyou.rs` — a legitimate home — and every check above would stay
    // green while the trust boundary quietly acquired an agent-shaped source.
    //
    // Read off the declaration rather than matched in Rust on purpose: an
    // exhaustive `match` would fail to COMPILE on a new variant, and a compile
    // error is the one failure that tells a reader nothing about behaviour. This
    // fails with a sentence instead.
    let src =
        fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/orchestration/needsyou.rs"))
            .expect("read needsyou.rs");
    let decl = src
        .split_once("pub enum ResolveSource {")
        .expect("ResolveSource's declaration moved — find it before weakening this test")
        .1
        .split_once('}')
        .expect("unterminated enum body")
        .0;
    let variants: Vec<&str> = decl
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//") && !l.starts_with("///"))
        .map(|l| l.trim_end_matches(','))
        .collect();
    assert_eq!(
        variants,
        vec!["Webview", "WebviewDismiss"],
        "ResolveSource gained or lost a variant. Every one of them is an entry point loomux \
         itself controls, and there is deliberately no agent-shaped one — a `board` or `agent` \
         variant would make a weaker settle indistinguishable from the human's acknowledgement, \
         which is the one thing `resolved_by` exists to keep unambiguous. The board's \
         auto-resolve and an agent's withdraw write their own tags for exactly this reason. \
         `WebviewDismiss` (#2137) is the human again, in the same webview, saying something \
         DIFFERENT — it widens no boundary, it adds a second thing the surface loomux already \
         trusted can say, and it writes its own tag so the two stay apart."
    );
}
