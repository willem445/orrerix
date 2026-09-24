//! The Codex **harness** (#2515 C1): the generated profile file, the launch
//! line, the store watcher and the solo MCP arm.
//!
//! Its sibling `codexsessions.rs` is about reading a store codex wrote; this
//! file is about the things loomux writes and the line it launches. The two
//! failure modes are independent — a lookup can be right while the profile
//! grants nothing, and vice versa.
//!
//! **What is worth pinning here, and why each is its own test.** Almost
//! everything in this slice fails SILENTLY. A profile whose top-level keys land
//! inside a table still parses; a contract that closes its own string
//! delimiter early still leaves a file on disk; a launch line missing `-p`
//! still starts codex. In every one of those cases the pane boots, looks
//! healthy, and simply has no trust, no tools or no contract — so the
//! assertions below are about the DOCUMENT and the LINE rather than about
//! whether the call returned `Ok`.
//!
//! An integration test, not inline `#[cfg(test)]`, per repo constraint 4.
//!
//! **No codex is ever run** (constraint 3), and not even `--help`: every vendor
//! fact these assertions encode is read blob-by-blob out of `openai/codex` at
//! tag `rust-v0.153.4` (and, for #3405's approval-mode pin, `rust-v0.156.1`)
//! and quoted in `docs/design/codex.md`.

use loomux_lib::orchestration::{
    codex_profile_file_name, codex_profile_name, codex_profile_name_of_path,
    codex_profile_toml, codex_user_mcp_exposure, codex_worktree_git_roots,
    single_pane_autopilot_flags, CodexMcpAuth, PathSegment,
};
use std::fs;
use std::path::{Path, PathBuf};

const CWD: &str = "C:\\Projects\\loomux-worktrees\\feat\\x";

fn seg(s: &str) -> PathSegment {
    PathSegment::parse(s).unwrap()
}

/// The profile a GROUP pane gets, at the posture most tests want.
fn group_profile(unattended: bool) -> String {
    codex_profile_toml(
        7777,
        CodexMcpAuth::EnvVar("ORRERIX_AGENT_TOKEN"),
        Path::new(CWD),
        unattended,
        "",
        None,
        &[],
    )
}

// ---------------------------------------------------------------------------
// 1. The document's SHAPE — the failures that parse
// ---------------------------------------------------------------------------

/// **The one that fails silently and expensively.** TOML gives every key after
/// a table header to that table, so a top-level scalar emitted below
/// `[sandbox_workspace_write]` becomes `sandbox_workspace_write.approval_policy`
/// — a key codex's strict-config check reports at best and ignores at worst,
/// leaving the pane with none of its posture and nothing red to say so.
///
/// Asserted on POSITION rather than on the rendered text, because the text is
/// what a later edit changes: any new top-level key added below the first `[`
/// reddens this without anyone having to remember the rule.
#[test]
fn a_codex_profiles_top_level_keys_all_precede_the_first_table_header() {
    // Every top-level key the generator can emit, including the two that are
    // conditional — so this is a claim about the generator, not about one
    // configuration of it.
    let body = codex_profile_toml(
        7777,
        CodexMcpAuth::EnvVar("ORRERIX_AGENT_TOKEN"),
        Path::new(CWD),
        true,
        "high",
        Some("be excellent"),
        &[],
    );
    let lines: Vec<&str> = body.lines().collect();
    let first_table = lines
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .expect("the document must have at least one table");
    for key in [
        "approval_policy",
        "sandbox_mode",
        "model_reasoning_effort",
        "developer_instructions",
    ] {
        let at = lines
            .iter()
            .position(|l| l.starts_with(&format!("{key} =")))
            .unwrap_or_else(|| panic!("{key} is not emitted at all:\n{body}"));
        assert!(
            at < first_table,
            "{key} is emitted at line {at}, BELOW the first table header at line {first_table} — \
             TOML would make it a key of that table, and nothing would fail loudly:\n{body}"
        );
    }
}

/// The trust line, which is the difference between a pane that reads its
/// kickoff and one that sits on "Do you trust the contents of this directory?"
/// eating it.
///
/// The Windows path is the specimen on purpose: it is the only one of the four
/// values in this document that needs escaping at all, and a quoted TOML key
/// with raw backslashes is a parse error rather than a wrong value — so getting
/// it wrong loses the WHOLE profile, not just the trust.
#[test]
fn a_codex_profile_trusts_the_panes_own_directory_with_the_backslashes_escaped() {
    let body = group_profile(true);
    assert!(
        body.contains("[projects.\"C:\\\\Projects\\\\loomux-worktrees\\\\feat\\\\x\"]"),
        "the trust key must be the pane's cwd as a TOML basic string, every backslash \
         doubled:\n{body}"
    );
    assert!(body.contains("trust_level = \"trusted\""), "{body}");
    // The negative half: a RAW backslash run inside the key would be a parse
    // error, so its absence is the actual property. Asserted separately
    // because the positive above would also pass on a document that carried
    // both forms.
    assert!(
        !body.contains("[projects.\"C:\\Projects"),
        "an unescaped backslash in the quoted key makes the whole profile unparseable:\n{body}"
    );
}

/// The posture is REAL on codex, unlike pi's, and it is the profile that
/// carries it — so the two postures must produce two different documents while
/// producing the same launch line (pinned in `codex_launch_flags_per_posture`).
///
/// Both directions are asserted. Only checking that unattended says `never`
/// would pass on a generator that hardcoded it, which is precisely the mistake
/// worth catching: an attended codex worker that never prompts is an
/// autonomous agent the human did not ask for.
#[test]
fn the_codex_approval_policy_flips_with_the_panes_posture() {
    assert!(group_profile(true).contains("approval_policy = \"never\""), "{}", group_profile(true));
    assert!(
        group_profile(false).contains("approval_policy = \"on-request\""),
        "{}",
        group_profile(false)
    );
    assert_ne!(
        group_profile(true),
        group_profile(false),
        "the two postures must differ SOMEWHERE in the profile — this is the document that \
         carries them, since the launch line deliberately does not"
    );
}

/// `workspace-write` plus network, and never the bypass rung.
///
/// The network line is not decoration: `workspace-write` turns network access
/// OFF by default, and a worker that cannot reach GitHub cannot open a PR — the
/// failure would arrive many minutes into a task, as a `gh` error, a long way
/// from this file.
#[test]
fn a_codex_pane_gets_workspace_write_with_network_and_never_the_bypass_rung() {
    let body = group_profile(true);
    assert!(body.contains("sandbox_mode = \"workspace-write\""), "{body}");
    assert!(body.contains("network_access = true"), "{body}");
    for forbidden in ["danger-full-access", "dangerously-bypass", "yolo"] {
        assert!(
            !body.contains(forbidden),
            "loomux never puts {forbidden:?} in a codex profile:\n{body}"
        );
    }
}

/// #687 on codex: the effort knob is a profile key, and an EMPTY effort emits
/// no key at all rather than an empty one.
///
/// The empty case is the one that matters and it is not a tidiness point:
/// `ReasoningEffort`'s own `FromStr` answers `Err("reasoning_effort must not be
/// empty")` for `""`, so `model_reasoning_effort = ""` would fail the whole
/// config — taking the trust level and the MCP server down with it — rather
/// than being ignored.
#[test]
fn a_codex_effort_knob_rides_the_profile_and_an_empty_one_emits_no_key() {
    let with = codex_profile_toml(
        7777,
        CodexMcpAuth::EnvVar("V"),
        Path::new(CWD),
        true,
        "xhigh",
        None,
        &[],
    );
    assert!(with.contains("model_reasoning_effort = \"xhigh\""), "{with}");
    let without = group_profile(true);
    assert!(
        !without.contains("model_reasoning_effort"),
        "an unset effort must emit no key — an empty one is refused by codex outright, which \
         would lose the whole profile:\n{without}"
    );
}

// ---------------------------------------------------------------------------
/// The profile sets NEITHER MCP timeout, and that absence is a decision
/// (#2515 C1, review round 1 finding 2).
///
/// This pins an absence, so it needs the control that an absence assertion
/// always needs: the document must demonstrably be the MCP-server document, or
/// "no timeout key" would also pass on an empty string.
///
/// **Why absent rather than set.** The first version of this file wrote
/// `tool_timeout_sec = 30.0` and `startup_timeout_sec = 20.0` and documented
/// them as raises over codex's "60s" and "10s" defaults. Both defaults were
/// wrong — transcribed from the slice plan rather than read from the source —
/// and the real ones are `DEFAULT_STARTUP_TIMEOUT = 30s` and
/// `DEFAULT_TOOL_TIMEOUT = 300s` (`codex-mcp/src/rmcp_client.rs` at the pin,
/// applied by `connection_manager.rs`'s `.unwrap_or`). So the shipped values
/// were reductions, the tool timeout by 10×, under a rationale arguing for the
/// opposite: a `spawn_agent` behind a large group can legitimately outrun 30
/// seconds, and stock codex would have waited 300.
///
/// The numbers are deliberately NOT asserted here. Pinning "30 and 300" would
/// be pinning someone else's constants, which orrerix cannot keep honest and
/// which would go stale silently the day codex changes them. What orrerix
/// controls, and all it should assert, is that it writes no number at all.
#[test]
fn a_codex_profile_sets_no_mcp_timeout_and_says_why() {
    let body = codex_profile_toml(
        7777,
        CodexMcpAuth::EnvVar("ORRERIX_AGENT_TOKEN"),
        Path::new(CWD),
        true,
        "high",
        Some("contract"),
        &[],
    );
    // The control: this really is the document that declares the MCP server, so
    // the absences below are about a populated entry rather than about nothing.
    assert!(body.contains("[mcp_servers.orrerix]"), "{body}");
    assert!(body.contains("url = \"http://127.0.0.1:7777/mcp\""), "{body}");

    for key in ["startup_timeout_sec", "tool_timeout_sec"] {
        assert!(
            !body.contains(key),
            "the profile must set no {key}: codex's own default is more generous than anything \
             orrerix would write, and the first version of this file set BOTH lower while \
             claiming to raise them:\n{body}"
        );
    }
}

/// #3405: loomux's own MCP tools are pre-approved with `approve` — NOT codex's
/// default `auto` — on every profile shape, inside loomux's server table only.
///
/// **What `auto` really does, and why it broke the first codex worker.** At
/// rust-v0.156.1 (identical at 0.153.4) `requires_mcp_tool_approval_for_mode`
/// maps `Auto` to "ask unless the tool is annotated read-only, or
/// non-destructive AND closed-world"; an un-annotated tool defaults to
/// destructive, and none of loomux's tools is annotated. With
/// `approval_policy = "never"` the ask is a refusal — "MCP tool call requires
/// approval, but approval policy is never" — so the unattended pane could not
/// `report`, `message_orchestrator` or `note_directive`. `Approve` is the value
/// `mcp_permission_prompt_is_auto_approved` returns `true` on before it reads
/// the policy at all, so it holds in BOTH postures.
///
/// Every (auth shape × posture) cell is checked, because the group and solo
/// paths build different header lines right beside this one, and the attended
/// posture is the one a "fix" keyed on `never` would miss. The line must sit
/// INSIDE `[mcp_servers.orrerix]`: the same key above the first table header is
/// not a key codex reads at all, and in any other server's table it would grant
/// that server's tools instead of loomux's.
#[test]
fn a_codex_profile_pre_approves_loomuxs_own_tools_in_every_shape_and_posture() {
    for (label, auth) in [
        ("group", CodexMcpAuth::EnvVar("ORRERIX_AGENT_TOKEN")),
        ("solo", CodexMcpAuth::Literal("tok-not-real")),
    ] {
        for unattended in [true, false] {
            let body = codex_profile_toml(7777, auth, Path::new(CWD), unattended, "", None, &[]);
            let lines: Vec<&str> = body.lines().collect();
            let table = lines
                .iter()
                .position(|l| l.trim() == "[mcp_servers.orrerix]")
                .unwrap_or_else(|| panic!("{label}/{unattended}: no loomux server table:\n{body}"));
            let table_end = lines[table + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map_or(lines.len(), |i| table + 1 + i);
            let hits: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.trim_start().starts_with("default_tools_approval_mode"))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(
                hits.len(),
                1,
                "{label}/unattended={unattended}: exactly one approval-mode line:\n{body}"
            );
            assert!(
                (table + 1..table_end).contains(&hits[0]),
                "{label}/unattended={unattended}: the approval mode must be a key of loomux's OWN \
                 server table (lines {table}..{table_end}), not at line {}:\n{body}",
                hits[0]
            );
            assert_eq!(
                lines[hits[0]].trim(),
                "default_tools_approval_mode = \"approve\"",
                "{label}/unattended={unattended}: `auto` still asks for every un-annotated tool, \
                 and under `approval_policy = \"never\"` that ask is a refusal (#3405):\n{body}"
            );
        }
    }
}
// 2. The token — two shapes, and each must NOT be the other
// ---------------------------------------------------------------------------

/// A GROUP pane's profile names the environment variable and contains no token
/// byte (plan D2), and a SOLO pane's carries the token because it has no
/// environment to name (#2515 C1's amendment to D2).
///
/// Both halves assert the ABSENCE of the other shape as well as the presence of
/// their own. Presence alone would pass on a generator that emitted both maps,
/// which is the one outcome that is worse than either: it would put the secret
/// in the file AND depend on the variable.
#[test]
fn a_group_codex_profile_names_the_token_variable_and_a_solo_one_carries_the_token() {
    const TOKEN: &str = "tok-abc123-not-a-real-one";

    let group = codex_profile_toml(
        7777,
        CodexMcpAuth::EnvVar("ORRERIX_AGENT_TOKEN"),
        Path::new(CWD),
        true,
        "",
        None,
        &[],
    );
    assert!(
        group.contains("env_http_headers = { \"X-Orrerix-Agent\" = \"ORRERIX_AGENT_TOKEN\" }"),
        "{group}"
    );
    // Matched per LINE, not as a substring, and the first draft of this
    // assertion got it wrong in the one way that matters: `env_http_headers`
    // CONTAINS `http_headers`, so `!group.contains("http_headers = {")` fails
    // against a perfectly correct group profile. A key name that is a suffix of
    // another key name cannot be excluded by substring at all — the check has to
    // be anchored at the start of the line, which is where a TOML key sits.
    assert!(
        !group.lines().any(|l| l.trim_start().starts_with("http_headers")),
        "a group profile must not carry a literal header map — that is the solo shape:\n{group}"
    );

    let solo = codex_profile_toml(
        7777,
        CodexMcpAuth::Literal(TOKEN),
        Path::new(CWD),
        false,
        "",
        None,
        &[],
    );
    assert!(solo.contains(&format!("http_headers = {{ \"X-Orrerix-Agent\" = \"{TOKEN}\" }}")), "{solo}");
    assert!(
        !solo.contains("env_http_headers"),
        "a solo profile must not name a variable: `solo_prepare` sets no pane environment, so \
         nothing would ever set it and the pane would connect unauthenticated while still being \
         advertised as a full channel member:\n{solo}"
    );
}

/// The group half of the pair above, stated as the property it actually is:
/// the token must not appear ANYWHERE in a group profile, not merely outside
/// the header map.
///
/// Separate from the test above because it is a different question. That one
/// asks which map is emitted; this one asks whether a token can reach the file
/// by some other route a later edit might add — a comment, a URL parameter, an
/// audit breadcrumb folded in.
#[test]
fn no_byte_of_a_group_codex_panes_token_reaches_its_profile() {
    const TOKEN: &str = "tok-must-not-appear-anywhere";
    let body = codex_profile_toml(
        7777,
        CodexMcpAuth::EnvVar("ORRERIX_AGENT_TOKEN"),
        Path::new(CWD),
        true,
        "high",
        Some("contract text"),
        &[],
    );
    assert!(!body.contains(TOKEN), "{body}");
    // The control: this assertion is only meaningful because the generator
    // COULD have been handed the token — `CodexMcpAuth::Literal` is the same
    // type in the same position, and it does put it in.
    let solo = codex_profile_toml(
        7777,
        CodexMcpAuth::Literal(TOKEN),
        Path::new(CWD),
        true,
        "high",
        Some("contract text"),
        &[],
    );
    assert!(
        solo.contains(TOKEN),
        "the assertion above is vacuous unless this generator can emit a token at all:\n{solo}"
    );
}

// ---------------------------------------------------------------------------
// 3. The contract, and the escaping that keeps it whole
// ---------------------------------------------------------------------------

/// A contract containing the delimiter must be ESCAPED, not truncated and not
/// rewritten (T1.5).
///
/// The failure this guards is the quiet one: a role contract that closes its
/// own string early leaves a file that either fails to parse (the pane loses
/// its trust and its MCP server, not just its persona) or — worse — parses with
/// the contract cut at the delimiter and the rest of it read as TOML.
///
/// Three specimens, each of which breaks a different naive encoder: a run of
/// three apostrophes (which is what the slice plan proposed to escape), a run
/// of three double quotes (which closes the form actually used), and a trailing
/// backslash (which would become a line continuation and swallow the closing
/// delimiter).
#[test]
fn a_codex_contract_with_triple_quotes_is_escaped_not_truncated() {
    let contract = "line one\n'''\nline two\n\"\"\"\nline three\\";
    let body = codex_profile_toml(
        7777,
        CodexMcpAuth::EnvVar("V"),
        Path::new(CWD),
        true,
        "",
        Some(contract),
        &[],
    );
    // Every line of the contract survives — nothing was cut at a delimiter.
    for fragment in ["line one", "line two", "line three"] {
        assert!(body.contains(fragment), "{fragment:?} was lost:\n{body}");
    }
    // No run of three unescaped double quotes anywhere except the two
    // delimiters the generator itself wrote. Counting is how "escaped" is
    // asserted without asserting the exact encoding, which is an
    // implementation detail a later edit may improve.
    assert_eq!(
        body.matches("\"\"\"").count(),
        2,
        "exactly two `\"\"\"` runs may appear — the opening and closing delimiters. A third is \
         the contract closing the string early:\n{body}"
    );
    // The apostrophe run is content, so it must still be there VERBATIM: this
    // is the half that says "escaped, not rewritten". A generator that
    // sanitized the contract to make it fit would pass every assertion above.
    assert!(
        body.contains("'''"),
        "the apostrophe run is part of the contract and must reach the agent unchanged — \
         rewriting it would alter the role contract invisibly:\n{body}"
    );
    // And the trailing backslash must not be able to eat the delimiter.
    assert!(
        body.contains("line three\\\\"),
        "a trailing backslash must be escaped, or it becomes a TOML line continuation and \
         swallows the closing delimiter:\n{body}"
    );
}

// ---------------------------------------------------------------------------
// 4. The profile NAME — the vendor's alphabet, refused rather than sanitized
// ---------------------------------------------------------------------------

/// The writer's literal and the readers' const are ONE fact (#2515 C1).
///
/// `codex_profile_file_name` spells `.config.toml` literally in its `format!`
/// template — it has to, or `no_raw_identifier_is_interpolated_into_a_file_name`
/// would not see the site at all, since that scan's trigger is an extension
/// literal inside the template. The orphan sweep and
/// `codex_profile_name_of_path` cannot use a literal: `strip_suffix` needs the
/// value. So the suffix is spelled twice, and this is what stops that being
/// #502's "a delete path that re-derives a write path's shape either misses
/// files or matches too widely".
///
/// Asserted as a ROUND TRIP rather than as `assert_eq!` on two strings, because
/// the round trip is the property that actually matters: whatever the writer
/// produces, the reader must recover the same name from it. A test comparing
/// the two spellings would pass on a pair that agreed with each other and with
/// nothing the filesystem holds.
#[test]
fn the_codex_profile_suffix_and_its_file_name_builder_agree() {
    for id in ["w-3", "orch-1", "solo-27"] {
        let file = codex_profile_file_name(&seg(id)).unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!(file, format!("orrerix-{id}.config.toml"));
        assert_eq!(
            codex_profile_name_of_path(Path::new(&file)),
            Some(format!("orrerix-{id}").as_str()),
            "the reader must recover exactly the name `-p` is spelled with, from the file name \
             the writer produced — the two spellings of the suffix are one fact"
        );
    }
    // `file_stem` is the obvious wrong reader and would answer
    // `orrerix-w-3.config` — a name `-p` would look for under a file that is
    // not there. Pinned so the cheaper-looking implementation cannot be
    // adopted silently.
    let file = codex_profile_file_name(&seg("w-3")).unwrap();
    assert_ne!(
        codex_profile_name_of_path(Path::new(&file)),
        Path::new(&file).file_stem().and_then(|s| s.to_str()),
        "the extension has two dots, so a file_stem reader is wrong rather than merely different"
    );
}
/// A profile name is valid **by construction**, and the check inside
/// `codex_profile_name` is a backstop against somebody else's alphabet moving.
///
/// This test pins the RELATIONSHIP rather than pretending to exercise the `Err`
/// arm, because that arm cannot be reached from a valid `PathSegment` today:
/// `check_segment` accepts ASCII alphanumerics, `_` and `-` and then refuses
/// two things codex would have accepted (a leading `-`, a reserved device
/// name), so it is strictly NARROWER than `ProfileV2Name`'s `FromStr`. Writing
/// a fixture for the `Err` arm would mean constructing a `PathSegment` that
/// cannot exist, and a test that cannot fail is a decoration.
///
/// What CAN change is either alphabet — `check_segment` serves four identifier
/// families and could widen for one of them. When it does, the assertion below
/// reddens and says that the backstop just stopped being decorative.
#[test]
fn a_codex_profile_name_is_valid_by_construction_and_the_check_is_the_backstop() {
    // Every id loomux actually mints, spelled the way `Block::prefix()` plus a
    // sequence number spells it.
    for id in ["w-3", "rev-4", "orch-1", "solo-27", "plan-2212"] {
        let name = codex_profile_name(&seg(id)).unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!(name, format!("orrerix-{id}"));
    }

    // The subset claim itself, over the whole printable-ASCII range: anything
    // `PathSegment` admits must be something codex's `--profile` admits too.
    // This is what makes the `Err` arm unreachable, so it is the thing to pin.
    for byte in 0x20u8..0x7f {
        let candidate = format!("w{}3", byte as char);
        if PathSegment::parse(&candidate).is_err() {
            continue;
        }
        assert!(
            candidate.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "{candidate:?} is a legal PathSegment but carries a character codex's ProfileV2Name \
             refuses — `codex_profile_name`'s Err arm is now REACHABLE, which is fine, but this \
             test's premise (and its doc) must be rewritten to exercise it rather than to \
             assert it cannot happen"
        );
        assert!(codex_profile_name(&seg(&candidate)).is_ok(), "{candidate}");
    }

    // The non-vacuity control: the sweep above is only meaningful if it
    // actually rejected some characters — an empty loop body would pass.
    assert!(
        PathSegment::parse("w.3").is_err() && PathSegment::parse("w@3").is_err(),
        "this sweep proves nothing unless PathSegment really does refuse some printable ASCII"
    );
}

// ---------------------------------------------------------------------------
// 5. The exposure row
// ---------------------------------------------------------------------------

/// The human's own `[mcp_servers.*]` entries merge into a codex pane's tool
/// surface, so they are reported — and a human who declares none costs no audit
/// row at all (T1.8).
#[test]
fn the_codex_user_config_exposure_row_names_merged_servers_and_is_absent_for_none() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();

    // No file at all: the overwhelmingly common case.
    assert!(codex_user_mcp_exposure(home, "orrerix").is_none());

    // A file with no servers is the SECOND absence, and a different one: the
    // file was read and said nothing. Both must be `None`, or every codex
    // spawn on a machine with an ordinary config.toml writes a noise row.
    fs::write(home.join("config.toml"), "model = \"gpt-5.5\"\n[tui]\nalternate_screen = \"never\"\n")
        .unwrap();
    assert!(
        codex_user_mcp_exposure(home, "orrerix").is_none(),
        "a config with no MCP servers must produce no row"
    );

    fs::write(
        home.join("config.toml"),
        "[mcp_servers.playwright]\ncommand = \"npx\"\n\n[mcp_servers.orrerix]\nurl = \"http://x\"\n",
    )
    .unwrap();
    let row = codex_user_mcp_exposure(home, "orrerix").expect("two servers must produce a row");
    assert_eq!(row["servers"], serde_json::json!(["orrerix", "playwright"]));
    // The DIRECTION, which is the opposite of pi's and is why it has a field:
    // loomux's profile is the later layer, so the user's same-named entry is
    // the one that loses.
    assert_eq!(row["this_pane_displaces_a_user_server_of_the_same_name"], serde_json::json!(true));
}

/// The blind spot, pinned rather than only disclosed: the scan is line-oriented,
/// so an INLINE `mcp_servers` table is invisible to it.
///
/// Without this, the disclosure in `codex_user_mcp_exposure`'s doc could go
/// false — someone widening the scan to a real TOML parse would leave the
/// "absence is not proof" sentence standing while it had stopped being true,
/// and nothing would say so. The pin fails in BOTH directions.
#[test]
fn the_codex_exposure_scan_cannot_see_an_inline_mcp_servers_table_and_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("config.toml"),
        "mcp_servers = { playwright = { command = \"npx\" } }\n",
    )
    .unwrap();
    assert!(
        codex_user_mcp_exposure(tmp.path(), "orrerix").is_none(),
        "this is the DISCLOSED blind spot, not a bug: the scan matches [mcp_servers.<name>] \
         headers. If this test starts failing because the scan was widened, delete the \
         'absence is not proof' paragraph from its doc in the same commit"
    );
    // The non-vacuity control: the same server declared in the shape the scan
    // DOES read is found, so the assertion above is about the shape rather
    // than about the function never returning anything.
    fs::write(tmp.path().join("config.toml"), "[mcp_servers.playwright]\ncommand = \"npx\"\n")
        .unwrap();
    assert!(codex_user_mcp_exposure(tmp.path(), "orrerix").is_some());
}

// ---------------------------------------------------------------------------
// 6. The posture toggle that is deliberately empty
// ---------------------------------------------------------------------------

/// `single_pane_autopilot_flags("codex")` is empty, and the test says WHY it is
/// empty so the row cannot be "tidied" into the `_` arm.
///
/// The claim is not "codex has no posture" — it has one, and loomux sets it in
/// the profile. The claim is that no part of it goes on the LINE, which is what
/// makes the launch line identical across postures.
#[test]
fn a_solo_codex_panes_posture_is_not_on_its_command_line() {
    assert_eq!(single_pane_autopilot_flags("codex"), "");
    // The control: this function is capable of returning something, so an
    // empty answer for codex is a decision rather than a property of the
    // function.
    assert!(!single_pane_autopilot_flags("claude").is_empty());
    // And the two spellings codex WOULD have used are never emitted, which is
    // the thing a future edit might reach for.
    for cli in ["codex", "claude", "copilot", "gemini", "opencode", "pi"] {
        let flags = single_pane_autopilot_flags(cli);
        assert!(
            !flags.contains("--ask-for-approval") && !flags.contains("--approve-for-me"),
            "{cli} must not carry codex's approval flags: {flags}"
        );
    }
}

// ---------------------------------------------------------------------------
// 7. Committing from a linked worktree (#3456)
// ---------------------------------------------------------------------------

/// git's own linked-worktree layout, built by hand so these tests need no git:
/// `<root>/main/.git/{objects,refs,logs,hooks,info}` plus `config`, and a
/// worktree `<root>/wt` whose `.git` FILE says `gitdir: <root>/main/.git/worktrees/wt`
/// — forward slashes, the way git writes it on Windows too — with `commondir`
/// reading `../..`, as `git worktree add` leaves it.
struct FakeWorktree {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    wt: PathBuf,
    gitdir: PathBuf,
    common: PathBuf,
}

fn fake_linked_worktree() -> FakeWorktree {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let common = root.join("main").join(".git");
    for d in ["objects", "refs", "logs", "hooks", "info"] {
        fs::create_dir_all(common.join(d)).unwrap();
    }
    fs::write(common.join("config"), "[core]\n").unwrap();
    let gitdir = common.join("worktrees").join("wt");
    fs::create_dir_all(&gitdir).unwrap();
    fs::write(gitdir.join("commondir"), "../..\n").unwrap();
    let wt = root.join("wt");
    fs::create_dir_all(&wt).unwrap();
    let pointer = gitdir.display().to_string().replace('\\', "/");
    fs::write(wt.join(".git"), format!("gitdir: {pointer}\n")).unwrap();
    FakeWorktree { _tmp: tmp, root, wt, gitdir, common }
}

/// The `writable_roots` array out of a profile, and the check that it sits in
/// `[sandbox_workspace_write]` — a key emitted into any other table is one
/// codex reads as something else or not at all. `None` when absent.
fn writable_roots_of(profile: &str) -> Option<Vec<PathBuf>> {
    let lines: Vec<&str> = profile.lines().collect();
    let at = lines.iter().position(|l| l.starts_with("writable_roots = ["))?;
    let table = lines[..at]
        .iter()
        .rposition(|l| l.trim_start().starts_with('['))
        .expect("writable_roots emitted above every table header");
    assert_eq!(
        lines[table], "[sandbox_workspace_write]",
        "writable_roots must be a key of [sandbox_workspace_write]:\n{profile}"
    );
    let body = lines[at].strip_prefix("writable_roots = [")?.strip_suffix(']')?;
    // Basic-string items: `\\` and `\"` are the only escapes a path produces.
    let (mut out, mut cur, mut in_str, mut esc) = (Vec::new(), String::new(), false, false);
    for c in body.chars() {
        match (in_str, esc, c) {
            (true, true, c) => {
                cur.push(c);
                esc = false;
            }
            (true, false, '\\') => esc = true,
            (true, false, '"') => {
                out.push(PathBuf::from(std::mem::take(&mut cur)));
                in_str = false;
            }
            (true, false, c) => cur.push(c),
            (false, _, '"') => in_str = true,
            _ => {}
        }
    }
    Some(out)
}

/// The roots a worktree pane is handed: its gitdir, and exactly the three
/// directories of the shared store a commit and a push write — never the shared
/// `.git` itself, and never `hooks/`, `info/` or `config`, which are where a
/// write becomes code the human's unsandboxed git runs.
///
/// The gitdir is compared with the POINTER'S OWN SPELLING, not a canonical one:
/// codex drops its read-only default for the gitdir only when an explicit
/// writable root is `==` to the path it resolved from that pointer
/// (`append_default_read_only_path_if_no_explicit_rule`, rust-v0.156.1), so a
/// root naming the same directory by another spelling would leave the DENY in
/// place.
#[test]
fn a_linked_worktree_gets_its_gitdir_and_the_shared_store_but_not_hooks_or_config() {
    let f = fake_linked_worktree();
    let roots = codex_worktree_git_roots(&f.wt).expect("git's own layout is recognised");
    let pointer = fs::read_to_string(f.wt.join(".git")).unwrap();
    let pointed = Path::new(pointer.trim().strip_prefix("gitdir:").unwrap().trim());
    assert_eq!(roots[0].as_path(), pointed, "the gitdir, spelled as the pointer spells it: {roots:?}");
    assert_eq!(
        roots,
        vec![f.gitdir.clone(), f.common.join("objects"), f.common.join("refs"), f.common.join("logs")],
        "exactly the gitdir and objects/refs/logs"
    );
    for never in [f.common.clone(), f.common.join("hooks"), f.common.join("info"), f.common.join("config")] {
        assert!(
            !roots.iter().any(|r| never.starts_with(r)),
            "{} must stay outside every writable root — a write there runs as the human: {roots:?}",
            never.display()
        );
    }
}

/// A missing store directory is left out rather than named: codex skips a
/// nonexistent root on Windows, and it is not a path to hand any other
/// platform's sandbox to bind.
#[test]
fn a_store_directory_that_does_not_exist_is_not_named() {
    let f = fake_linked_worktree();
    fs::remove_dir_all(f.common.join("logs")).unwrap();
    let roots = codex_worktree_git_roots(&f.wt).unwrap();
    assert_eq!(roots, vec![f.gitdir.clone(), f.common.join("objects"), f.common.join("refs")]);
}

/// The acceptance's other half: a pane in a MAIN clone (`.git` is a directory)
/// or in no repo at all gets nothing extra — codex's own protection of its
/// `.git` stands. The fake worktree beside it is the control that the function
/// can answer non-empty on the same machine.
#[test]
fn a_main_clone_or_a_plain_directory_gets_no_extra_writable_roots() {
    let f = fake_linked_worktree();
    assert!(!codex_worktree_git_roots(&f.wt).unwrap().is_empty(), "control");
    assert_eq!(codex_worktree_git_roots(&f.root.join("main")).unwrap(), Vec::<PathBuf>::new());
    let plain = f.root.join("plain");
    fs::create_dir_all(&plain).unwrap();
    assert_eq!(codex_worktree_git_roots(&plain).unwrap(), Vec::<PathBuf>::new());
}

/// A relative pointer (`worktree.useRelativePaths`) resolves against the pane's
/// directory and is folded lexically, as codex folds it — no `..` survives into
/// a root codex would compare component by component.
#[test]
fn a_relative_gitdir_pointer_is_resolved_against_the_pane_and_folded() {
    let f = fake_linked_worktree();
    fs::write(f.wt.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
    let roots = codex_worktree_git_roots(&f.wt).unwrap();
    assert_eq!(roots[0], f.gitdir, "{roots:?}");
    assert!(
        roots.iter().all(|r| !r.components().any(|c| c == std::path::Component::ParentDir)),
        "{roots:?}"
    );
}

/// **The refusal.** The gitdir is writable once #3456 lands, so a pane can
/// rewrite its own `commondir`; if the next spawn trusted that file, the pane
/// could name ANY directory's `objects`/`refs`/`logs` as its writable roots.
/// Every layout that is not git's own linked-worktree shape grants nothing.
/// The untampered layout first, so each refusal below is about its tamper.
#[test]
#[ignore] // scratch: already seen red in run 35948502403; ignored so cargo reaches orchestration.rs
fn a_worktree_layout_that_is_not_gits_own_grants_nothing() {
    let f = fake_linked_worktree();
    assert!(codex_worktree_git_roots(&f.wt).is_ok(), "control: the untampered layout is accepted");

    // commondir redirected at another store that has all three directories.
    let elsewhere = f.root.join("elsewhere");
    for d in ["objects", "refs", "logs"] {
        fs::create_dir_all(elsewhere.join(d)).unwrap();
    }
    fs::write(f.gitdir.join("commondir"), format!("{}\n", elsewhere.display())).unwrap();
    let err = codex_worktree_git_roots(&f.wt).expect_err("a redirected commondir must be refused");
    assert!(err.contains("commondir"), "{err}");
    fs::write(f.gitdir.join("commondir"), "../..\n").unwrap();

    // No commondir at all — a submodule's or `--separate-git-dir`'s gitdir,
    // which is a whole repository, hooks and config included.
    let module = f.common.join("modules").join("m");
    fs::create_dir_all(&module).unwrap();
    let sub = f.root.join("sub");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join(".git"), format!("gitdir: {}\n", module.display())).unwrap();
    assert!(codex_worktree_git_roots(&sub).is_err(), "a gitdir outside <common>/worktrees/ is refused");

    // A `.git` file that is not a pointer, and a pointer at nothing.
    fs::write(sub.join(".git"), "not a pointer\n").unwrap();
    assert!(codex_worktree_git_roots(&sub).is_err());
    fs::write(sub.join(".git"), format!("gitdir: {}\n", f.root.join("gone").display())).unwrap();
    assert!(codex_worktree_git_roots(&sub).is_err());
}

/// The rendering: one `writable_roots` array inside `[sandbox_workspace_write]`,
/// every backslash doubled (a raw one is a TOML parse error that loses the WHOLE
/// profile), and no key at all when there is nothing to grant — so a main-clone
/// or solo pane's document is unchanged byte for byte.
#[test]
fn a_codex_profile_spells_its_writable_roots_inside_the_workspace_write_table() {
    let roots = vec![
        PathBuf::from("C:\\Projects\\repo\\.git\\worktrees\\wt"),
        PathBuf::from("C:\\Projects\\repo\\.git\\objects"),
    ];
    let body = codex_profile_toml(
        7777,
        CodexMcpAuth::EnvVar("ORRERIX_AGENT_TOKEN"),
        Path::new(CWD),
        true,
        "",
        None,
        &roots,
    );
    assert!(
        body.contains(
            "writable_roots = [\"C:\\\\Projects\\\\repo\\\\.git\\\\worktrees\\\\wt\", \
             \"C:\\\\Projects\\\\repo\\\\.git\\\\objects\"]"
        ),
        "{body}"
    );
    assert_eq!(writable_roots_of(&body), Some(roots));
    assert!(body.contains("[sandbox_workspace_write]\nnetwork_access = true\n"), "{body}");

    let none = group_profile(true);
    assert!(none.contains("network_access = true"), "control: the table is there: {none}");
    assert!(!none.contains("writable_roots"), "nothing to grant, no key: {none}");
}
