//! Get_output grid reconstruction and per-CLI launch posture: permission flags, autopilot, gemini, OpenCode, session association and per-block CLI pinning.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------------------------------------------------------------------------
// #520: composed-grid reconstruction for `get_output`.
//
// The tests above pin the LINE-IDENTITY collapse, which handles a CLI that
// redraws whole lines. The fixture below is the shape that defeats it — and
// defeated it in production, at ~12k tokens per call.
// ---------------------------------------------------------------------------

/// The delivered prompt, sitting in the pane's input box and repainted on
/// every single frame — the single biggest contributor to the observed flood.
const TUI_PROMPT_LINE: &str = "> Read issue #520 and harden the get_output capture path end to end";

/// A synthesized capture of a Claude-Code-v2.1.x-shaped animated turn, as RAW
/// pty bytes (escape sequences intact — they are the whole point).
///
/// Synthesized, never recorded from a live CLI: CLAUDE.md constraint 3 rules
/// out spawning a real agent to produce a fixture, and a hand-built one pins
/// the *mechanism* rather than one session's incidental text. The mechanism,
/// straight from the issue:
///
/// - the live region is repainted in place — cursor parked, `ESC[J`, three
///   rows rewritten, cursor moved back up — so nothing scrolls and no two
///   frames are byte-identical;
/// - the status **verb cycles** and a token counter **ticks**, so a stable
///   "core" per frame doesn't exist either;
/// - most frames repaint only PART of a line (cursor re-addressed to one
///   column, digits overwritten), which is what makes the ANSI-stripped
///   stream concatenate into ever-different text;
/// - the queued prompt is inside the repainted region, so it multiplies.
fn claude_tui_animation_capture() -> Vec<u8> {
    const VERBS: [&str; 6] =
        ["Shenaniganing", "Roosting", "Creating", "Percolating", "Noodling", "Simmering"];
    const GLYPHS: [&str; 5] = ["✻", "✶", "✽", "●", "✢"];
    let mut cap: Vec<u8> = Vec::new();

    // Real, committed work output — what a triaging orchestrator is actually
    // looking for, and what the flood buried.
    for i in 1..=5 {
        cap.extend_from_slice(format!("real work line {i}\r\n").as_bytes());
    }

    // 300 frames of the animated turn: each one repaints the whole three-row
    // live region (spinner / blank / input box) and parks the cursor back at
    // its top-left, and most also tick the counter in place afterwards.
    for i in 1..=300u32 {
        cap.extend_from_slice(b"\r\x1b[J");
        cap.extend_from_slice(
            format!(
                "{} {}… (esc to interrupt · {i}s · ↓ {} tokens)\r\n",
                GLYPHS[(i % 5) as usize],
                VERBS[(i % 6) as usize],
                i * 7
            )
            .as_bytes(),
        );
        cap.extend_from_slice(b"\r\n");
        cap.extend_from_slice(TUI_PROMPT_LINE.as_bytes());
        // Back to the top-left of the live region for the next frame.
        cap.extend_from_slice(b"\r\x1b[2A");
        if i % 10 != 0 {
            // Partial-line repaint: one column re-addressed, digits rewritten.
            cap.extend_from_slice(format!("\x1b[40G{i}s").as_bytes());
        }
    }
    cap
}

#[test]
fn get_output_composes_a_tui_redraw_storm_down_to_one_screen() {
    // What the orchestrator asks for on a busy pane, and what it must get:
    // the screen as a human sees it — the real work lines, the current
    // spinner frame, the prompt once — not 300 frames of paint.
    let capture = claude_tui_animation_capture();
    let composed = loomux_lib::orchestration::termgrid::render_screen(&capture, 100, 30);
    let out = format_output_tail(&composed, 30);

    for i in 1..=5 {
        assert!(
            out.contains(&format!("real work line {i}")),
            "real content line {i} was buried by the animation, got:\n{out}"
        );
    }
    assert_eq!(
        out.matches(TUI_PROMPT_LINE).count(),
        1,
        "the delivered prompt is repainted every frame; it must appear ONCE in \
         the composed screen, not once per repaint. Got:\n{out}"
    );
    assert!(
        out.contains("300s"),
        "the freshest frame is the one that says whether the pane is still \
         alive — it must be what survives, got:\n{out}"
    );
    assert!(
        !out.contains("· 40s ·") && !out.contains("· 150s ·"),
        "frames that were painted over must be gone, not stacked, got:\n{out}"
    );
    // The real cost check. 300 repaints of a ~56-column status line and a
    // 67-character prompt is tens of KB of write stream; one screen of it is
    // a few hundred bytes.
    assert!(
        out.len() < 1_000,
        "a 30-line read of an animated pane must cost a few hundred bytes, \
         not kilobytes — got {} bytes:\n{out}",
        out.len()
    );
}

#[test]
fn get_output_grid_is_what_saves_it_not_the_line_collapse() {
    // Mutation pin, and the reason the fix is a grid replay rather than a
    // smarter dedup: run the SAME bytes through the old pipeline (delete the
    // escapes, keep the text) and the flood is fully reproduced here. If a
    // future change quietly routes `get_output` back to `strip_ansi`, the
    // first assertion below is what it looks like from the caller's side.
    let capture = claude_tui_animation_capture();
    let stripped = strip_ansi(&capture);
    assert!(
        stripped.matches(TUI_PROMPT_LINE).count() > 5,
        "sanity: the raw write stream really does multiply the prompt — if it \
         doesn't, this fixture no longer reproduces #520"
    );
    assert!(
        stripped.len() > 10_000,
        "sanity: the raw write stream really is kilobytes, got {}",
        stripped.len()
    );

    let composed = loomux_lib::orchestration::termgrid::render_screen(&capture, 100, 30);
    assert!(
        composed.len() * 10 < stripped.len(),
        "replaying the same bytes onto a screen must be an order of magnitude \
         smaller than keeping the write stream — got {} composed vs {} stripped",
        composed.len(),
        stripped.len()
    );
}

#[test]
fn get_output_grid_resolves_partial_line_repaints() {
    // The exact mechanism that defeats line-identity dedup, in miniature: one
    // row, one column re-addressed, digits overwritten. Nothing here is ever
    // a repeated LINE, so no dedup can help; only replaying the writes can.
    let mut raw = b"working: 0%".to_vec();
    for pct in [10, 20, 30, 40, 50, 60, 70, 80, 90] {
        raw.extend_from_slice(format!("\x1b[10G{pct}%").as_bytes());
    }
    assert!(
        strip_ansi(&raw).contains("0%10%20%"),
        "sanity: stripping escapes really does concatenate the repaints"
    );
    assert_eq!(
        loomux_lib::orchestration::termgrid::render_screen(&raw, 40, 5),
        "working: 90%",
        "a counter ticking in place is one line showing its latest value"
    );
}

#[test]
fn get_output_grid_leaves_plain_scrolling_output_alone() {
    // The other half of "no damage": a pane that just prints (a build log, a
    // test run) has no redraws to compose, and must come back line for line —
    // including the lines that scrolled off the top of the screen.
    let mut raw = Vec::new();
    for i in 1..=100 {
        raw.extend_from_slice(format!("cargo check line {i}\r\n").as_bytes());
    }
    let composed = loomux_lib::orchestration::termgrid::render_screen(&raw, 80, 24);
    let lines: Vec<&str> = composed.lines().collect();
    assert_eq!(lines.len(), 100, "every printed line must survive, got:\n{composed}");
    assert_eq!(lines[0], "cargo check line 1", "scrollback must survive the replay");
    assert_eq!(lines[99], "cargo check line 100");
}

#[test]
fn get_output_grid_resyncs_after_a_malformed_utf8_byte() {
    // Review finding 2 (#530). The ring is a byte TAIL: it routinely begins
    // mid-codepoint, so a malformed sequence at the head of a capture is the
    // NORMAL case, not an exotic one. A decoder that assumes the width from
    // the lead byte and skips that whole window on failure eats the valid
    // text sitting behind it.
    use loomux_lib::orchestration::termgrid::render_screen;

    // The discriminating case: `0xe0` claims three bytes, so a window skip
    // swallows "he" and yields "llo". Only a one-byte resync recovers it.
    assert_eq!(
        render_screen(b"\xe0hello", 40, 5),
        "hello",
        "a bad lead byte must cost exactly itself, never the valid text behind it"
    );
    // An orphaned continuation byte — what a tail that starts mid-codepoint
    // actually looks like.
    assert_eq!(render_screen(b"\x80hello", 40, 5), "hello");
    // `0xff` is never valid UTF-8 in any position, sitting in front of real
    // multibyte content.
    let mut mixed: Vec<u8> = vec![0xff];
    mixed.extend_from_slice("✻ ok".as_bytes());
    assert_eq!(render_screen(&mixed, 40, 5), "✻ ok");
    // Truncated multibyte at the very END of the capture (the ring boundary
    // cut it): dropped cleanly, and above all not a panic.
    let mut truncated = b"done ".to_vec();
    truncated.push(0xe2); // first byte of a 3-byte '…'; the rest never arrived
    assert_eq!(render_screen(&truncated, 40, 5), "done");
}

#[test]
fn get_output_grid_history_keeps_the_newest_rows_within_its_cap() {
    // Review finding 1 (#530) is a COMPLEXITY fix (the scrolled-off history
    // was drained one row at a time off the front of a Vec, so an adversarial
    // newline flood paid O(cap) per scroll). Behavior was already correct, so
    // there is no red to show for it — this test guards what the fix could
    // plausibly break instead: swapping the container must not change which
    // rows are retained. Deliberately structural, never wall-clock — timing
    // assertions on this repo are a recorded lesson (#514/#516).
    let mut raw = Vec::new();
    for i in 1..=6000 {
        raw.extend_from_slice(format!("row {i}\r\n").as_bytes());
    }
    let composed = loomux_lib::orchestration::termgrid::render_screen(&raw, 80, 24);
    let lines: Vec<&str> = composed.lines().collect();

    assert_eq!(
        lines.last(),
        Some(&"row 6000"),
        "the newest row must always survive — it is what a monitoring read is for"
    );
    assert!(
        lines.len() < 6000,
        "history must be capped, not unbounded — got {} rows",
        lines.len()
    );
    assert!(
        !lines.iter().any(|l| *l == "row 1"),
        "the OLDEST rows are the ones the cap must drop"
    );
    // Retention is a contiguous newest-first window: no gaps, no reordering,
    // no duplicated rows introduced by the eviction path.
    let first: usize = lines[0]
        .strip_prefix("row ")
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("unexpected first row {:?}", lines[0]));
    assert_eq!(
        lines.len(),
        6000 - first + 1,
        "retained rows must be one contiguous run ending at the newest, got {} rows starting at {first}",
        lines.len()
    );
}

#[test]
fn get_output_is_hard_capped_in_bytes_whatever_lines_asks_for() {
    // `lines` bounds lines; only a byte cap bounds the payload. 400 rows of
    // 200 wide, all distinct, nothing to collapse — the shape of a real wide
    // build log, and 80 KB into the caller's context before this cap.
    let mut text = String::new();
    for i in 1..=400 {
        text.push_str(&format!("{i:04} {}\n", "x".repeat(200)));
    }
    let out = format_output_tail(&text, 500);

    assert!(
        out.len() <= OUTPUT_TAIL_MAX_BYTES,
        "get_output must never return more than {OUTPUT_TAIL_MAX_BYTES} bytes, got {}",
        out.len()
    );
    assert!(
        out.len() * 4 < text.len(),
        "sanity: the cap must actually be binding on this fixture"
    );
    assert!(
        out.contains("truncated"),
        "truncation must be stated, never silent, got:\n{}",
        &out[..out.len().min(200)]
    );
    assert!(
        out.contains("0400 "),
        "the NEWEST content is what a monitoring read wants — it must be the \
         part that survives"
    );
    assert!(
        !out.contains("0001 "),
        "the oldest content is what should have been dropped"
    );
}

#[test]
fn get_output_byte_cap_never_splits_a_multibyte_character() {
    // The cap counts BYTES, and pane output is full of multibyte glyphs — box
    // drawing, arrows, spinner stars, ellipses. Cutting at a byte offset that
    // lands inside one panics the whole `get_output` call. Sibling of the cap
    // test above (the same "disable the cap" mutation reddens both on the
    // length assertion); this one additionally pins the boundary handling,
    // which a pure-ASCII fixture cannot reach.
    let mut text = String::new();
    for i in 1..=400 {
        text.push_str(&format!("│ {i:04} ↓ {}\n", "…".repeat(60)));
    }
    let out = format_output_tail(&text, 500);
    assert!(
        out.len() <= OUTPUT_TAIL_MAX_BYTES,
        "the cap must bind on multibyte content too, got {}",
        out.len()
    );
    assert!(out.contains("0400"), "the newest content must survive, got:\n{out}");
    assert!(
        out.chars().count() > 100,
        "the surviving text must still decode as characters, got:\n{out}"
    );
}

#[test]
fn claude_command_minimizes_init_approvals_without_bypass() {
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let cmd = reg.build_agent_command("claude", "sonnet", false, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::None, &PersonaInject::default());
    assert!(cmd.contains("--model sonnet"));
    assert!(cmd.contains("--permission-mode acceptEdits"));
    assert!(cmd.contains("--strict-mcp-config"), "workers must not see the user's other MCP servers");
    assert!(cmd.contains("--add-dir \"C:/data/group\""),
        "instructions dir must be a workspace so reading it never prompts");
    assert!(cmd.contains("--allowedTools mcp__orrerix"),
        "loomux tools must be pre-approved so report/list never prompt");
    assert!(!cmd.contains("Bash(git"), "git is not pre-approved for a non-auto_ops worker");
    let cmd = reg.build_agent_command("claude", "sonnet", true, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::None, &PersonaInject::default());
    assert!(cmd.contains("--permission-mode auto"),
        "the Auto preset must use Claude Code's native auto permission mode");
    assert!(cmd.contains("\"Bash(git *)\"") && cmd.contains("\"Bash(gh *)\""),
        "auto_ops pre-approves git + gh so the branch→commit→PR flow runs unattended");
    assert!(
        !cmd.contains("--dangerously-skip-permissions"),
        "bypass mode must never be used: its confirm dialog defaults to exit and kills the pane"
    );
    // A worker (`Containment::None`) has no write/commit denials.
    assert!(!cmd.contains("--disallowedTools"), "an uncontained agent gets no tool denials");
    // A planner (`Containment::ReadOnly`) is denied file writes + git commit/push
    // at the CLI level, even under Auto perms — but keeps gh for the plan comment.
    let plan = reg.build_agent_command("claude", "opus", true, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::ReadOnly, &PersonaInject::default());
    assert!(plan.contains("--disallowedTools"), "planner must deny tools structurally");
    for denied in CLAUDE_EDIT_DENY_TOOLS {
        assert!(plan.contains(denied), "planner must deny the {denied} tool");
    }
    assert!(!plan.contains("MultiEdit"),
        "MultiEdit matches no real Claude Code tool (#448) — must not reappear in the deny list");
    assert!(plan.contains("\"Bash(git commit *)\"") && plan.contains("\"Bash(git push *)\""),
        "planner must deny git commit/push using the canonical (space-form) rule spelling");
    assert!(plan.contains("\"Bash(gh *)\""), "gh stays allowed so the planner can post its plan comment");
    assert!(!plan.contains("\"Bash(git checkout"),
        "read-only git usage (checkout/log/diff) must not be denied");
    // The colon-mid wildcard is malformed: Claude Code ignores the rule AND
    // prints a startup warning (the "auto deny rule" flash on planner boot).
    // No rule may use it — that regression is what this pins down.
    assert!(!plan.contains("commit:*") && !plan.contains("push:*"),
        "no colon-mid wildcard rule (`Bash(git commit:*)`) — it is malformed and triggers the startup warning flash");
}

/// #462: the deny tier is a **property of the capability class**, and this is
/// the enumeration of it. Written as an exhaustive `match` on `Role` rather
/// than a list of `assert_eq!`s on purpose — a fifth class would make this test
/// fail to compile, where a list of asserts would keep passing while saying
/// nothing about the class nobody wrote a line for. (An enumeration is a claim
/// of completeness; this is the claim being made checkable.)
#[test]
fn every_capability_class_pins_its_deny_tier() {
    // #2519: the population is `Role::ALL`, not a hand-list. It was a literal
    // array, which is the completeness claim this test's own doc makes being
    // made by a list somebody had to remember to widen; `Role::ALL` is
    // compiler-enforced complete (`Role::all_index`), so the enumeration below
    // is the only thing left to write an arm in.
    for role in Role::ALL {
        let want = match role {
            Role::Orchestrator | Role::Worker => Containment::None,
            Role::Reviewer => Containment::NoEdits,
            Role::Planner => Containment::ReadOnly,
            // #1161. The reviewer's tier for the reviewer's reason: a manager
            // reads the codebase to ground its questions and never writes it.
            // `ReadOnly` would be the wrong rung, not merely a stricter one —
            // it forces unattended mode, and the whole point of this class is
            // that a human is sitting in front of it.
            Role::Manager => Containment::NoEdits,
            Role::Solo => Containment::None,
            // #2519. The ORCHESTRATOR's arm, on the orchestrator's argument and
            // not the solo pane's: a lead pane is a full working pane the human
            // drives — it edits their code and runs their commands — so a deny
            // tier here would take away the CLI they launched. Unlike
            // `Role::Solo`'s arm this one IS reached: `build_agent_command`
            // clamps a spawn loomux performs, and slice B opens a lead through
            // it. What bounds the fan-out is the cap and the spawn-rate
            // backstop, not containment.
            Role::Lead => Containment::None,
        };
        assert_eq!(role.containment(), want, "{role:?} changed deny tier — was that deliberate?");
    }
    // The ladder: each tier is the one below it plus more. What this rules out
    // is a tier that denies `git commit` while leaving `Edit` reachable, which
    // would be containment on paper only — the editing tool is the easier path.
    for c in [Containment::None, Containment::NoEdits, Containment::ReadOnly] {
        assert!(!c.denies_git_mutation() || c.denies_edits(), "{c:?} denies git but not edits");
        assert!(!c.forces_unattended() || c.denies_git_mutation(), "{c:?} is unattended below ReadOnly");
    }
    // `is_read_only()` must keep meaning the FULLY read-only class: it gates the
    // `allow:` ban (`parse_workflow`/`persona_inject`), which #462 deliberately
    // did not extend to reviewers. A reviewer picking up deny flags must not
    // silently pick that up with them.
    assert!(Role::Planner.is_read_only());
    assert!(!Role::Reviewer.is_read_only(), "a reviewer is contained, but it is NOT read-only");
    // #1161, and the reason it is worth stating rather than following from the
    // tier above: a manager IS banned from `allow:`, but by decision D1 (a
    // repo may not author the human's interface), NOT by `is_read_only()`. If
    // this ever flipped, the D1 ban would silently become a duplicate of the
    // read-only one and the two rules would stop being separable.
    assert!(!Role::Manager.is_read_only(), "a manager is contained, but it is NOT read-only");
}

/// #462: a reviewer's containment, asserted through the same selector the spawn
/// site uses (`Role::containment()`, passed straight into `build_agent_command`
/// by `spawn_agent_ex`) rather than a hand-picked tier — so this fails if the
/// *class* ever stops mapping to the deny flags, not merely if the flag-emitting
/// code changes.
///
/// The negative halves matter as much as the positive one. #462's whole risk is
/// that "make the reviewer safer" quietly becomes "make the reviewer a planner":
/// a reviewer that lost its shell could not run the tests, and one promoted to
/// unattended would gain pre-approved git/gh it never had.
#[test]
fn reviewer_is_denied_editing_tools_but_keeps_its_shell() {
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let tier = Role::Reviewer.containment();

    let rev = reg.build_agent_command("claude", "sonnet", true, cfg, None, gdir, Path::new("C:/repo"), None, false, tier, &PersonaInject::default());
    assert!(rev.contains("--disallowedTools"), "a reviewer must deny tools structurally, not by instruction: {rev}");
    for denied in CLAUDE_EDIT_DENY_TOOLS {
        assert!(rev.contains(denied), "a reviewer must deny the {denied} tool: {rev}");
    }
    // The job: run the tests, check the PR out, post the review.
    assert!(!rev.contains("Bash(git commit"), "a reviewer keeps git commit — its template hands it that in place of git stash (#299)");
    assert!(!rev.contains("Bash(git push"), "denying push here would be a claim the shell can't back; it stays instruction-tier");
    assert!(rev.contains("\"Bash(gh *)\""), "gh stays allowed so the reviewer can post its review");
    // #465 x #462, the seam where the two changes meet: a reviewer must NOT be
    // promoted to `dontAsk`. That mode auto-denies everything outside
    // `--allowedTools` — for a reviewer that is the shell it runs the tests
    // through, so the mode that hardens a planner would disable a reviewer.
    // `claude_effective_permission_mode` is fed `containment.is_read_only()`,
    // never `denies_edits()`, and this is what says so out loud.
    assert!(!rev.contains("dontAsk"),
        "a reviewer must never run under dontAsk (#465 names this case explicitly): {rev}");
    assert!(rev.contains("--permission-mode auto"), "{rev}");
    // …and no promotion: an auto_ops=false reviewer stays exactly as attended
    // as it was before #462.
    let manual = reg.build_agent_command("claude", "sonnet", false, cfg, None, gdir, Path::new("C:/repo"), None, false, tier, &PersonaInject::default());
    assert!(manual.contains("--permission-mode acceptEdits"),
        "a non-auto_ops reviewer must NOT inherit the planner's forced-unattended promotion: {manual}");
    assert!(!manual.contains("\"Bash(gh *)\""),
        "a non-auto_ops reviewer must not gain the unattended git/gh pre-approval either: {manual}");

    // Copilot spells the same denial its own way — pinned separately because a
    // fix applied to one adapter only is the drift #448 already caught once.
    let rev = reg.build_agent_command("copilot", "auto", true, cfg, None, gdir, Path::new("C:/repo"), None, false, tier, &PersonaInject::default());
    // #802: one `--deny-tool`, comma-separated — membership, not a flag per entry.
    let denied = copilot_tool_patterns(&rev, "--deny-tool");
    for t in COPILOT_EDIT_DENY_TOOLS {
        assert!(denied.iter().any(|d| d == t), "a reviewer must deny {t}: {rev}");
    }
    assert!(!rev.contains("shell(git commit)") && !rev.contains("shell(git push)"),
        "a copilot reviewer keeps its shell git, same as the claude one: {rev}");
}

/// #462 review finding: **the spawn SITE's own wiring**, which every other test
/// in this arc leaves uncovered.
///
/// `reviewer_is_denied_editing_tools_but_keeps_its_shell` and the snapshot rows
/// all call `build_agent_command` themselves, re-deriving the tier with the
/// *same expression* the spawn site uses. That is a pin that stops at the
/// extracted unit: a `spawn_agent_ex` refactor that hardcoded a literal —
/// exactly the shape this PR just removed from `register_orchestrator_pane` —
/// would keep all of them green while reviewers silently lost their flags
/// (#492). So this one goes through `spawn_agent` and reads what the SITE
/// built, via the request the no-frontend path keeps.
///
/// Three classes in one group, because a single-class assertion is defeated by
/// the opposite mutation: pinning only the reviewer passes if the site hardcodes
/// `NoEdits` for everyone, and pinning only "worker has no flags" passes if it
/// hardcodes `None`. What is actually being pinned is the *mapping*, so each
/// class must be wrong in its own direction.
#[test]
fn a_spawn_carries_the_deny_flags_of_the_class_it_spawned() {
    let (reg, _d) = test_registry();
    // Three live delegates at once, so the classes are compared within ONE
    // group's config rather than across three groups that could differ.
    let g = reg
        .create_group("C:/tmp/deny-site-repo", Guardrails { max_agents: 3, ..rails() })
        .unwrap();

    // Not `build_agent_command` — the real thing, the way the orchestrator
    // reaches it. `worktree: false` keeps the fixture off the filesystem; the
    // launch flags don't depend on it.
    let spawn = |role: Role, name: &str| {
        let a = reg.spawn_agent(&g.id, role, name, "t", false, None).unwrap();
        reg.spawn_request_for_test(&a.id).unwrap_or_else(|| panic!("no spawn request for {name}"))
    };

    let rev = spawn(Role::Reviewer, "rev");
    assert_eq!(rev.role, Role::Reviewer, "sanity: the request is the reviewer's");
    for denied in CLAUDE_EDIT_DENY_TOOLS {
        assert!(
            rev.command.contains(denied),
            "a REVIEWER SPAWN must carry the {denied} denial — the class-to-flags hop at the \
             spawn site is what this pins: {}",
            rev.command
        );
    }
    assert!(rev.command.contains("--disallowedTools"), "{}", rev.command);
    // The structured form the pane actually spawns is built from the same call,
    // and a site that fed one builder and not the other would be just as broken.
    assert!(
        rev.argv.windows(4).any(|w| w == ["--disallowedTools", "Edit", "Write", "NotebookEdit"]),
        "the direct-spawn argv must carry them too: {:?}",
        rev.argv
    );
    // Still a reviewer, not a planner: the shell it works through is intact.
    assert!(!rev.command.contains("Bash(git commit"), "{}", rev.command);
    assert!(!rev.command.contains("Bash(git push"), "{}", rev.command);
    // #946 Q4 / #1091 slice H (H7): this is a PLAIN reviewer — no
    // `role_hint: liaison` — so it must not pick up the question deny just
    // because it already carries a `--disallowedTools` list for #462.
    assert!(
        !rev.command.contains("AskUserQuestion"),
        "a non-liaison reviewer must not deny AskUserQuestion: {}",
        rev.command
    );

    // A worker off the SAME site must come out with nothing denied — this is the
    // half that fails if the site starts denying unconditionally.
    let w = spawn(Role::Worker, "w");
    assert!(
        !w.command.contains("--disallowedTools"),
        "a worker spawn must carry no denials — it exists to change the repo: {}",
        w.command
    );

    // …and a planner off the same site keeps the full read-only tier, so a site
    // that flattened every class onto the reviewer's tier fails here.
    let p = spawn(Role::Planner, "p");
    assert!(
        p.command.contains("\"Bash(git commit *)\"") && p.command.contains("\"Bash(git push *)\""),
        "a planner spawn must still deny git mutation: {}",
        p.command
    );
    // #946 Q4 / #1091 slice H: a planner is read-only already and has no
    // `role_hint`, so H7's predicate must not reach it either — the deny is
    // role-keyed (orchestrator/liaison), not "already contained at all".
    assert!(
        !p.command.contains("AskUserQuestion"),
        "a planner must not deny AskUserQuestion — H7 names only the \
         orchestrator and a liaison-hinted reviewer: {}",
        p.command
    );

    // The other spawn site (#462 review named both). `register_orchestrator_pane`
    // hands its request straight back, so no seam is needed to read it — and an
    // orchestrator that ever acquired deny flags would be a group that cannot
    // drive its own git/gh flow.
    // Through `test_registry`, never a raw `OrchRegistry::new` — #464's guard
    // (`no_registry_construction_bypasses_the_test_agent_dir_overrides`) is right:
    // an unrouted registry leaks a generated agent file into the real
    // `~/.claude`/`~/.copilot` agents dir on its first spawn. `Arc` wraps the
    // helper's registry rather than building a second one.
    let (reg2, _d2) = test_registry();
    let reg2 = std::sync::Arc::new(reg2);
    let repo = tempfile::tempdir().unwrap();
    let orch = create_orchestration_group(
        &reg2,
        &repo.path().to_string_lossy().replace('\\', "/"),
        rails(),
        SessionOrigin::Fresh,
        None,
        None,
    )
    .unwrap();
    assert_eq!(orch.role, Role::Orchestrator);
    // #946 Q4 / #1091 slice H flips this assertion (it used to read
    // `!orch.command.contains("--disallowedTools")`, back when the
    // orchestrator's tier — `Containment::None` — carried no deny flags at
    // all). This IS the site that opens a `--disallowedTools` flag where
    // none existed before: the orchestrator's `Containment` stays `None` (it
    // must still drive its own git/gh flow unrestricted — the block below
    // pins that), but it now denies the blocking AskUserQuestion dialog by
    // ROLE instead (`claude_denies_interactive_question`) — a held dialog on
    // this exact pane strands every delegate report queued behind it, since
    // the queue that holds them is bounded (`queue::QUEUE_MAX_PER_PANE`).
    for denied in CLAUDE_QUESTION_DENY_TOOLS {
        assert!(
            orch.command.contains(denied),
            "the orchestrator spawn must deny {denied} — a held dialog here \
             strands every delegate report queued behind it: {}",
            orch.command
        );
    }
    assert!(orch.command.contains("--disallowedTools"), "{}", orch.command);
    assert!(
        orch.argv.windows(2).any(|w| w == ["--disallowedTools", "AskUserQuestion"]),
        "the direct-spawn argv must carry it too: {:?}",
        orch.argv
    );
    // Still `Containment::None` in every other respect: the orchestrator must
    // keep driving git/gh unrestricted, and this new role-keyed deny must not
    // smuggle in the edit/git tiers meant only for a contained class.
    //
    // Checked against the TOKENIZED argv, not a raw substring of `command` —
    // `"Edit"` is a substring of `"acceptEdits"` (the `--permission-mode`
    // value every orchestrator command carries), so a naive `.contains("Edit")`
    // false-positives on every orchestrator spawn regardless of this deny.
    for untouched in CLAUDE_EDIT_DENY_TOOLS {
        assert!(
            !orch.argv.iter().any(|t| t == untouched),
            "the orchestrator must still keep {untouched} — Containment::None \
             is unchanged: {:?}",
            orch.argv
        );
    }
    assert!(!orch.command.contains("Bash(git commit"), "{}", orch.command);
    assert!(!orch.command.contains("Bash(git push"), "{}", orch.command);
}

/// H7 (#1091 plan-783): the liaison — a `kind: reviewer` block carrying
/// `role_hint: liaison` (#891) — is named in the deny predicate explicitly,
/// not merely swept in by already being contained. This spawns a liaison
/// block through the REAL site (`spawn_agent_ex`, the way an orchestrator
/// actually dispatches one) and asserts its command/argv carry BOTH the
/// pre-existing #462 edit denial AND the new #946 Q4 one, in the SAME
/// `--disallowedTools` list — never two occurrences of the flag, since Claude
/// Code does not merge two `--disallowedTools` flags on one command line (see
/// `claude_denies_interactive_question`'s doc: a second occurrence would
/// silently drop the edit denial already emitted).
#[test]
fn a_liaison_hinted_reviewer_denies_both_edits_and_the_question_dialog() {
    // `liaison_group` (below, #891) already builds exactly this shape: a
    // roster with a `human` block carrying `kind: reviewer` +
    // `role_hint: liaison`. Reused rather than re-declared so this test can
    // never drift from what the #891 liaison tests actually spawn.
    let (reg, _d, _repo, gid) = liaison_group();
    let a = reg
        .spawn_agent_ex(&gid, Role::Reviewer, Some("human".into()), "liaison", "t", false, None, None, None, None, None)
        .unwrap();
    let req = reg
        .spawn_request_for_test(&a.id)
        .unwrap_or_else(|| panic!("no spawn request for the liaison"));
    assert_eq!(req.role, Role::Reviewer, "sanity: the liaison block is still Role::Reviewer");

    // ONE --disallowedTools flag, both denials inside it.
    assert_eq!(
        req.command.matches("--disallowedTools").count(),
        1,
        "exactly one --disallowedTools flag must carry both denials: {}",
        req.command
    );
    // Tokenized, not a raw substring of `req.command` — `.contains("Edit")`
    // is satisfied by `--permission-mode acceptEdits`, which every liaison
    // command also carries, so a naive check here would pass whether or not
    // `Edit` is actually in the deny list (the exact bug `6f2bc23e` fixed on
    // the absence side, left live here on the presence side).
    let command_tokens = shell_tokenize(&req.command);
    for denied in CLAUDE_EDIT_DENY_TOOLS.iter().chain(CLAUDE_QUESTION_DENY_TOOLS.iter()) {
        assert!(
            command_tokens.iter().any(|t| t == denied),
            "a liaison-hinted reviewer must deny {denied}: {:?}",
            command_tokens
        );
    }
    assert_eq!(
        req.argv.iter().filter(|t| t.as_str() == "--disallowedTools").count(),
        1,
        "the argv form must agree — one flag, not two: {:?}",
        req.argv
    );
    for denied in CLAUDE_EDIT_DENY_TOOLS.iter().chain(CLAUDE_QUESTION_DENY_TOOLS.iter()) {
        assert!(req.argv.iter().any(|t| t == denied), "{denied} missing from argv: {:?}", req.argv);
    }
    // A liaison is still `NoEdits`, not `ReadOnly`: git commit/push stay
    // reachable through the shell — #891 gives it no reason to touch git at
    // all, but #462's own guarantee for a reviewer must not narrow here.
    assert!(!req.command.contains("Bash(git commit"), "{}", req.command);
    assert!(!req.command.contains("Bash(git push"), "{}", req.command);
}

/// The predicate itself (#946 Q4 / #1091 H7), independent of any spawn
/// plumbing: exactly orchestrator OR liaison-hinted, nothing else — the unit
/// `a_liaison_hinted_reviewer_denies_both_edits_and_the_question_dialog` and
/// `a_spawn_carries_the_deny_flags_of_the_class_it_spawned` exercise through
/// real spawns.
#[test]
fn claude_denies_interactive_question_is_exactly_orchestrator_or_liaison() {
    assert!(claude_denies_interactive_question(Role::Orchestrator, None));
    assert!(claude_denies_interactive_question(Role::Orchestrator, Some("liaison")));
    assert!(claude_denies_interactive_question(Role::Reviewer, Some("liaison")));
    // Every other (role, hint) pairing this codebase can actually produce
    // (`role_hint_requires` pins `liaison` to `Role::Reviewer` alone) reads false.
    assert!(!claude_denies_interactive_question(Role::Worker, None));
    assert!(!claude_denies_interactive_question(Role::Reviewer, None));
    assert!(!claude_denies_interactive_question(Role::Planner, None));
    assert!(!claude_denies_interactive_question(Role::Reviewer, Some("advisor")));
    assert!(!claude_denies_interactive_question(Role::Reviewer, Some("process")));
}

/// #448-style drift pin, sibling of `claude_edit_deny_tools_are_known_claude_tools`:
/// `CLAUDE_QUESTION_DENY_TOOLS` names a real Claude Code tool, so a typo or an
/// upstream rename fails CI instead of silently denying nothing.
///
/// Mutation evidence (red before green): temporarily adding a typo'd entry
/// (e.g. "AskUserQuestions") fails this assertion immediately; the real
/// spelling passes.
#[test]
fn claude_question_deny_tools_are_known_claude_tools() {
    for t in CLAUDE_QUESTION_DENY_TOOLS {
        assert!(
            KNOWN_CLAUDE_TOOLS.contains(t),
            "{t:?} in CLAUDE_QUESTION_DENY_TOOLS is not a known Claude Code tool per \
             the Tools reference (https://code.claude.com/docs/en/tools-reference) — \
             typo, or was the tool renamed/removed upstream? (#448 discipline)"
        );
    }
}

/// #448: the editing-tool guarantee is spelled as string literals
/// matched against an external tool registry that drifts underneath it (the
/// CLI's own registry, which loomux does not control and cannot query live —
/// see `KNOWN_CLAUDE_TOOLS`'s doc). This pins `CLAUDE_EDIT_DENY_TOOLS`
/// against the Claude Code Tools reference snapshot so a typo or a stale name
/// breaks CI instead of silently widening what a contained class may do —
/// since #462 that is a reviewer as well as a planner, which is precisely why
/// the list stayed ONE list: a second copy would be a second thing to keep
/// pinned, and a stale entry reads as containment while providing none. Exactly
/// the failure mode that let `MultiEdit` (folded into `Edit` upstream) sit
/// in the deny list matching nothing until a human read the CLI's own
/// startup warning by hand.
///
/// Mutation evidence (red before green): temporarily re-adding "MultiEdit" —
/// or any other typo — to `CLAUDE_EDIT_DENY_TOOLS` fails this assertion
/// immediately, with a message naming the bad entry; removing it passes.
/// This is exactly the property #448 asked for.
#[test]
fn claude_edit_deny_tools_are_known_claude_tools() {
    for t in CLAUDE_EDIT_DENY_TOOLS {
        assert!(
            KNOWN_CLAUDE_TOOLS.contains(t),
            "{t:?} in CLAUDE_EDIT_DENY_TOOLS is not a known Claude Code tool per \
             the Tools reference (https://code.claude.com/docs/en/tools-reference) — \
             typo, or was the tool renamed/removed upstream? (#448)"
        );
    }
}

/// Sibling of `claude_edit_deny_tools_are_known_claude_tools` for the
/// Copilot adapter — the SAME pin, against `KNOWN_COPILOT_DENY_CATEGORIES`
/// (the exactly-three `--deny-tool` value shapes Copilot's CLI configuration
/// guide documents). This is the test that would have caught `edit` before
/// it shipped: `edit` was never one of the documented shapes, so a version
/// of `COPILOT_EDIT_DENY_TOOLS` that includes it fails here exactly the
/// way a `MultiEdit`-style typo fails the Claude pin above — a validator
/// that catches one and not the other is not doing its job (#448 review).
///
/// Mutation evidence (red before green): temporarily adding "edit" back to
/// `COPILOT_EDIT_DENY_TOOLS` fails this assertion immediately; removing
/// it passes.
#[test]
fn copilot_edit_deny_tools_are_known_copilot_categories() {
    for t in COPILOT_EDIT_DENY_TOOLS {
        assert!(
            KNOWN_COPILOT_DENY_CATEGORIES.contains(t),
            "{t:?} in COPILOT_EDIT_DENY_TOOLS is not one of the three documented \
             --deny-tool value shapes (shell(COMMAND) / write / MCP_SERVER(tool)) per \
             https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/configure-copilot-cli \
             — this is exactly how `edit` should have been caught (#448)"
        );
    }
}

/// The consistency test (`build_agent_argv_matches_command_line`) already
/// pins the string-vs-argv *shapes* against each other; this pins the
/// **content** of the git-mutation denials against the exact CLI-documented
/// spellings, so `CLAUDE_READONLY_DENY_GIT` / `COPILOT_READONLY_DENY_GIT`
/// can't quietly grow a malformed entry (e.g. the colon-mid wildcard
/// regression already covered above) or lose one.
#[test]
fn readonly_deny_git_lists_are_exactly_commit_and_push() {
    assert_eq!(CLAUDE_READONLY_DENY_GIT, ["Bash(git commit *)", "Bash(git push *)"]);
    assert_eq!(COPILOT_READONLY_DENY_GIT, ["shell(git commit)", "shell(git push)"]);
}

#[test]
fn planner_runs_unattended_regardless_of_auto_ops() {
    // A planner has no human in its pane, so it must reach gh + the loomux MCP
    // and explore read-only WITHOUT prompting — even when the group is NOT in
    // auto_ops. Otherwise it deadlocks on the first approval no one can give,
    // which is exactly why claude's `plan` permission mode can't be used here.
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    // auto_ops = FALSE, read_only = TRUE (a planner in a manual-ops group).
    let plan = reg.build_agent_command("claude", "opus", false, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::ReadOnly, &PersonaInject::default());
    assert!(plan.contains("--permission-mode dontAsk"),
        "a planner runs unattended (dontAsk: pre-approved tools only, #465) even when \
         the group is not auto_ops — else it deadlocks");
    assert!(plan.contains("\"Bash(gh *)\""),
        "a non-auto_ops planner must still have gh pre-approved so `gh issue comment` (its plan) never prompts");
    assert!(plan.contains("\"Bash(git *)\""),
        "a non-auto_ops planner must still have read-only git pre-approved for exploration");
    assert!(plan.contains("--disallowedTools") && plan.contains("Write"),
        "writes/commit/push stay denied structurally — Auto perms don't loosen the read-only contract");
    // By contrast a non-auto_ops WORKER (read_only=false) is unchanged: it
    // stays in acceptEdits with no pre-approved git/gh (the human gates ops).
    let worker = reg.build_agent_command("claude", "sonnet", false, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::None, &PersonaInject::default());
    assert!(worker.contains("--permission-mode acceptEdits"),
        "a non-auto_ops worker is unaffected: it still gates ops through acceptEdits");
    assert!(!worker.contains("\"Bash(gh *)\""),
        "a non-auto_ops worker gets no pre-approved gh — only planners run unattended");
}

/// Read a copilot command line's `--allow-tool` / `--deny-tool` patterns —
/// and, in doing so, assert the #802 invariant every caller below depends on:
/// **the option appears AT MOST ONCE**, carrying a comma-separated list.
///
/// Copilot's CLI reference documents these options as taking "a quoted,
/// comma-separated list" and never as repeatable — while annotating five
/// neighbouring options "(can be used multiple times)". A repeated occurrence
/// is therefore a form the docs don't describe, and if copilot resolves it
/// last-wins rather than by accumulating, every pattern but the last is
/// silently dropped — which is #802's report ("the CLI lists the loomux MCP
/// server as available, but the agent has no permission to use its tools"),
/// since `--allow-tool orrerix` was emitted FIRST.
///
/// Returns the patterns in order, or an empty vec when the option is absent.
fn copilot_tool_patterns(cmd: &str, flag: &str) -> Vec<String> {
    let occurrences = cmd.matches(&format!("{flag} ")).count();
    assert!(
        occurrences <= 1,
        "{flag} must appear at most once on a copilot command line (#802) — every pattern \
         belongs in ONE comma-separated value; found {occurrences} occurrences in: {cmd}"
    );
    let needle = format!("{flag} \"");
    let Some(i) = cmd.find(&needle) else { return Vec::new() };
    let after = &cmd[i + needle.len()..];
    let end = after.find('"').expect("an unterminated tool-permission value");
    after[..end].split(',').map(str::to_string).collect()
}

/// #802 — the invariant the fix exists for, stated once, in the form a
/// regression would break: every allow pattern loomux grants a copilot agent
/// (the loomux MCP server, the attended git/gh pair, and a workflow block's own
/// `allow:` patterns) rides ONE `--allow-tool`, and every denial ONE
/// `--deny-tool`.
///
/// This is the copilot sibling of
/// `claude_allow_patterns_are_not_severed_from_the_allowedtools_flag`: same
/// failure mode (a grant that reads as present on the command line but isn't in
/// effect), reached through a different CLI's parser. The persona patterns are
/// what makes it bite in practice — they are the ONLY copilot-argv difference a
/// workflow block introduces over the built-in roster, which is why #802 was
/// reported against custom workflows specifically.
#[test]
fn copilot_tool_permission_flags_ride_one_occurrence_each() {
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let persona = PersonaInject {
        extra_allow: vec!["shell(npm:*)".into(), "shell(cargo:*)".into()],
        ..Default::default()
    };
    let cmd = |auto_ops, containment, p: &PersonaInject| {
        reg.build_agent_command(
            "copilot", "auto", auto_ops, cfg, None, gdir, Path::new("C:/repo"), None, false,
            containment, p,
        )
    };

    // The attended tier stacks the most allow patterns, so it is where a
    // repeated-flag regression shows first.
    let attended = cmd(false, Containment::None, &persona);
    assert_eq!(
        copilot_tool_patterns(&attended, "--allow-tool"),
        ["orrerix", "shell(git:*)", "shell(gh:*)", "shell(npm:*)", "shell(cargo:*)"],
        "every allow pattern must sit in one comma-separated value, MCP server first: {attended}"
    );

    // An unattended block with a persona: the loomux grant must survive the
    // block's own patterns, which used to be emitted as later occurrences.
    let unattended = cmd(true, Containment::None, &persona);
    assert_eq!(
        copilot_tool_patterns(&unattended, "--allow-tool"),
        ["orrerix", "shell(npm:*)", "shell(cargo:*)"],
        "an unattended agent drops the git/gh pair (it has --allow-all-tools) but never the \
         MCP grant: {unattended}"
    );

    // A planner's denials — three patterns that used to be three flags.
    let planner = cmd(true, Containment::ReadOnly, &PersonaInject::default());
    assert_eq!(
        copilot_tool_patterns(&planner, "--deny-tool"),
        ["write", "shell(git commit)", "shell(git push)"],
        "every denial must sit in one comma-separated value: {planner}"
    );
    // …and the read-only class still grants loomux, which is the whole point:
    // a planner with no MCP tools cannot post its plan or report.
    assert_eq!(copilot_tool_patterns(&planner, "--allow-tool"), ["orrerix"]);

    // An uncontained agent emits no `--deny-tool` at all rather than an empty
    // value — a flag with nothing after it is a parse hazard, not a no-op.
    assert!(
        !unattended.contains("--deny-tool"),
        "no denials means no flag: {unattended}"
    );
}

#[test]
fn copilot_command_uses_copilot_adapter_flags() {
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let cmd = reg.build_agent_command("copilot", "auto", true, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::None, &PersonaInject::default());
    assert!(cmd.starts_with("copilot "), "selected CLI must actually be launched, not claude");
    assert!(
        cmd.contains("--additional-mcp-config \"@C:/x/cfg.json\""),
        "the @ file marker must be inside the quotes — a bare @\" opens a PowerShell here-string, got: {cmd}"
    );
    assert!(cmd.contains("--model auto"));
    // The bare MCP server name is copilot's documented "all tools from this
    // server" form (`SERVER-NAME` row of the tool-permission-patterns table);
    // #802 changed only its packaging, never the value.
    assert!(copilot_tool_patterns(&cmd, "--allow-tool").contains(&"orrerix".to_string()), "got: {cmd}");
    assert!(cmd.contains("--add-dir \"C:/data/group\""));
    assert!(
        cmd.contains("--add-dir \"C:/repo\""),
        "the workspace must be pre-trusted so panes don't stall on a trust prompt"
    );
    assert!(
        cmd.contains("--no-auto-update"),
        "a mid-boot self-update restarts copilot and eats the kickoff"
    );
    // Auto preset = copilot's group autopilot posture: all tools + all paths +
    // --autopilot (true autopilot mode). The startup "Enable autopilot mode"
    // dialog is answered by the kickoff path before the brief is pasted (#101).
    assert!(cmd.contains("--allow-all-tools") && cmd.contains("--allow-all-paths"));
    assert!(cmd.contains("--autopilot"),
        "group copilot workers run in true autopilot mode; the kickoff confirms the consent dialog");
    // Conservative preset keeps the explicit allowlist instead.
    let cmd = reg.build_agent_command("copilot", "auto", false, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::None, &PersonaInject::default());
    assert!(!cmd.contains("--allow-all-tools") && !cmd.contains("--autopilot"));
    let allowed = copilot_tool_patterns(&cmd, "--allow-tool");
    assert!(allowed.iter().any(|p| p == "shell(git:*)") && allowed.iter().any(|p| p == "shell(gh:*)"),
        "the attended tier keeps its git/gh pre-approval: {cmd}");
    // Resume reopens a tracked session via --resume; copilot has no
    // pre-assignable id, so a session without resume adds no session flag.
    let sid = "aabbccdd-1122-4334-8556-77889900aabb";
    let cmd = reg.build_agent_command("copilot", "auto", true, cfg, None, gdir, Path::new("C:/repo"), Some(sid), true, Containment::None, &PersonaInject::default());
    // #458/#781: the `=` form, and NEVER the space form. Copilot documents
    // `--resume[=VALUE]` as optional-value, so `--resume <id>` risks parsing as
    // a bare `--resume` (the interactive picker, or an outright error under a
    // non-TTY) plus a stray positional — in a pane loomux is about to type a
    // kickoff into. See the builder's own comment for the quoted reference.
    assert!(cmd.contains(&format!("--resume={sid}")), "copilot resume must use --resume=<id>, got: {cmd}");
    assert!(!cmd.contains("--resume "), "the space form is the #458 hazard, got: {cmd}");
    let cmd = reg.build_agent_command("copilot", "auto", true, cfg, None, gdir, Path::new("C:/repo"), Some(sid), false, Containment::None, &PersonaInject::default());
    assert!(!cmd.contains("--resume") && !cmd.contains("--session-id"),
        "a fresh copilot spawn cannot pin a session id");
    // A non-planner copilot agent gets no deny-tool flags.
    assert!(!cmd.contains("--deny-tool"), "non-planner copilot agents get no tool denials");
    // A planner (read_only=true) denies writes + git commit/push even under
    // --allow-all-tools (deny wins in Copilot); gh stays reachable.
    let plan = reg.build_agent_command("copilot", "auto", true, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::ReadOnly, &PersonaInject::default());
    let plan_deny = copilot_tool_patterns(&plan, "--deny-tool");
    assert!(plan_deny.iter().any(|d| d == "write"),
        "planner must deny copilot's write category (the documented file-modification category), got: {plan}");
    // #448: `edit` is not one of Copilot's three documented --deny-tool value
    // shapes (shell(COMMAND) / write / MCP_SERVER(tool)) — it was likely
    // already as inert as MultiEdit was for Claude, so it was dropped rather
    // than kept as an unverified entry that only looked like containment.
    assert!(!plan_deny.iter().any(|d| d == "edit"),
        "edit is not a documented copilot deny-tool value — must not reappear (#448), got: {plan}");
    assert!(plan_deny.iter().any(|d| d == "shell(git commit)") && plan_deny.iter().any(|d| d == "shell(git push)"),
        "planner must deny git commit/push");
    assert!(!plan_deny.iter().any(|d| d.starts_with("shell(gh")), "gh stays allowed for the plan comment");
}

#[test]
fn copilot_planner_runs_unattended_regardless_of_auto_ops() {
    // Mirror of the claude fix on the copilot adapter: a planner has no human
    // in its pane, so a NON-auto_ops copilot planner must still take copilot's
    // group autopilot preset (all tools/paths + --autopilot) — the conservative
    // interactive preset would stall it on approvals no one can give. Deny
    // rules keep it read-only (deny wins over --allow-all-tools in Copilot).
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    // auto_ops = FALSE, read_only = TRUE (a planner in a manual-ops group).
    let plan = reg.build_agent_command("copilot", "auto", false, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::ReadOnly, &PersonaInject::default());
    assert!(plan.contains("--allow-all-tools") && plan.contains("--allow-all-paths"),
        "a non-auto_ops copilot planner must run unattended (all tools/paths), else it deadlocks: {plan}");
    assert!(plan.contains("--autopilot"),
        "a group planner runs in true autopilot mode; the kickoff answers the consent dialog for it");
    let plan_deny = copilot_tool_patterns(&plan, "--deny-tool");
    assert!(plan_deny.iter().any(|d| d == "write") && plan_deny.iter().any(|d| d == "shell(git commit)"),
        "writes/commit stay denied — the unattended preset doesn't loosen the read-only contract");
    assert!(!plan_deny.iter().any(|d| d.starts_with("shell(gh")),
        "gh stays allowed so the copilot planner can post its plan comment unattended");
    // A non-auto_ops copilot WORKER (read_only=false) is unchanged: it keeps
    // the conservative interactive preset (no allow-all).
    let worker = reg.build_agent_command("copilot", "auto", false, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::None, &PersonaInject::default());
    assert!(!worker.contains("--allow-all-tools") && !worker.contains("--autopilot"),
        "a non-auto_ops copilot worker stays interactive — only planners run unattended");
}

// ── Single-pane autopilot flags (#101) ─────────────────────────────────────

#[test]
fn single_pane_autopilot_flags_per_cli() {
    // Claude: native Auto permission mode + the git/gh pre-approval, so a
    // standalone pane skips the interactive prompt-on-everything default.
    let claude = single_pane_autopilot_flags("claude");
    assert!(claude.contains("--permission-mode auto"),
        "claude autopilot must use the native Auto mode, got: {claude}");
    assert!(claude.contains("--allowedTools"), "claude autopilot must pre-approve tools");
    assert!(claude.contains("\"Bash(git *)\"") && claude.contains("\"Bash(gh *)\""),
        "claude autopilot must pre-approve git + gh, got: {claude}");
    assert!(!claude.contains("--dangerously-skip-permissions"),
        "autopilot must never use bypass mode");

    // Copilot: all tools + all paths pre-approved, PLUS --autopilot (#364) —
    // single panes now enter true autopilot mode too, same as the group path.
    // The resulting consent dialog is answered by a dedicated solo watcher
    // (`confirm_solo_copilot_autopilot`), not left unattended.
    let copilot = single_pane_autopilot_flags("copilot");
    assert!(copilot.contains("--allow-all-tools") && copilot.contains("--allow-all-paths"),
        "copilot autopilot must pass its unattended flags, got: {copilot}");
    assert!(copilot.contains("--autopilot"),
        "copilot autopilot must now use --autopilot (#364) — the checkbox should mean true autopilot \
         mode on single panes too, not just allow-all: {copilot}");

    // Hermes: --yolo bypasses dangerous-command approval prompts, and docs
    // show no startup consent dialog (unlike copilot's --autopilot).
    let hermes = single_pane_autopilot_flags("hermes");
    assert_eq!(hermes, "--yolo", "hermes autopilot must pass --yolo, got: {hermes}");

    // Ante: --yolo executes all tools automatically, no rule evaluation or
    // prompts (docs: configuration/permission.mdx, usage/approvals.mdx) — same
    // shape as Hermes's --yolo, no documented startup dialog either.
    let ante = single_pane_autopilot_flags("ante");
    assert_eq!(ante, "--yolo", "ante autopilot must pass --yolo, got: {ante}");

    // Case-insensitive on the program name.
    assert_eq!(single_pane_autopilot_flags("Claude"), claude);
    assert_eq!(single_pane_autopilot_flags("COPILOT"), copilot);
    assert_eq!(single_pane_autopilot_flags("Hermes"), hermes);
    assert_eq!(single_pane_autopilot_flags("Ante"), ante);

    // Gemini (#267): `--approval-mode yolo` — the documented spelling, since
    // its own CLI reference marks the bare `--yolo` alias deprecated. Built
    // from the SAME atom the group spawn path uses (#101), asserted below in
    // `gemini_launcher_and_group_paths_share_one_unattended_atom`.
    let gemini = single_pane_autopilot_flags("gemini");
    assert_eq!(
        gemini, "--approval-mode yolo",
        "gemini autopilot must use the non-deprecated approval-mode spelling, got: {gemini}"
    );
    assert_eq!(single_pane_autopilot_flags("Gemini"), gemini);

    // OpenCode (#722): `--auto` — "auto-approve permissions that are not
    // explicitly denied". The `--yolo` / `--dangerously-skip-permissions`
    // aliases are hidden in its own option table, so the documented spelling
    // is the one loomux emits. Same atom as the group path
    // (`OPENCODE_UNATTENDED_FLAGS`), asserted in
    // `opencode_launcher_and_group_paths_share_one_unattended_atom`.
    let opencode = single_pane_autopilot_flags("opencode");
    assert_eq!(
        opencode, "--auto",
        "opencode autopilot must use the documented spelling, got: {opencode}"
    );
    assert_eq!(single_pane_autopilot_flags("OpenCode"), opencode);

    // No flags on the LINE. `opencode` left this class in #722 — replaced by
    // `custom`, the placeholder this function's own doc names, so the specimen
    // count and the property both survive.
    //
    // **`codex` is in this loop for a different reason from the other three,
    // and since #2515 C1 the difference matters.** `custom`, `aider` and `""`
    // are here because nothing is known about their unattended surface, so
    // inventing flags would be guessing. codex has a real approval policy and
    // loomux really does set it — as `approval_policy` in the generated
    // profile — so its empty answer is a statement about WHERE the posture
    // lives, not about whether it exists. Reaching for `-a/--ask-for-approval`
    // here would give a pane a posture its own profile disagrees with.
    for other in ["codex", "custom", "aider", ""] {
        assert_eq!(single_pane_autopilot_flags(other), "",
            "{other:?} puts no unattended flag on the command line — must return empty");
    }
}

#[test]
fn single_pane_flags_reuse_the_group_path_atoms() {
    // The whole point of #101: the single-pane flags are built from the SAME
    // per-CLI atoms as build_agent_command, so the two paths can't drift. If
    // build_agent_command's unattended flags change, these must change with it.
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");

    // Claude: permission mode + the shared git/gh allowlist constant.
    let group_claude =
        reg.build_agent_command("claude", "sonnet", true, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::None, &PersonaInject::default());
    let single_claude = single_pane_autopilot_flags("claude");
    assert!(single_claude.contains(&format!("--permission-mode {}", claude_permission_mode(true))));
    assert!(group_claude.contains(&format!("--permission-mode {}", claude_permission_mode(true))));
    assert!(single_claude.contains(CLAUDE_UNATTENDED_ALLOW) && group_claude.contains(CLAUDE_UNATTENDED_ALLOW),
        "both paths must use the shared CLAUDE_UNATTENDED_ALLOW constant");

    // Copilot (#364): single-pane and group now share the SAME posture — both
    // enter true autopilot mode, so single_pane_autopilot_flags reuses the
    // group constant directly rather than inventing a divergent string.
    let group_copilot =
        reg.build_agent_command("copilot", "auto", true, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::None, &PersonaInject::default());
    let single_copilot = single_pane_autopilot_flags("copilot");
    assert_eq!(single_copilot, COPILOT_GROUP_AUTOPILOT_FLAGS,
        "single-pane copilot must reuse the group autopilot atom verbatim, not a divergent string");
    assert!(group_copilot.contains(COPILOT_GROUP_AUTOPILOT_FLAGS),
        "the group path must use the shared COPILOT_GROUP_AUTOPILOT_FLAGS constant");
    // The atoms still can't drift apart: group == "--autopilot " + allow-all.
    assert_eq!(COPILOT_GROUP_AUTOPILOT_FLAGS, format!("--autopilot {COPILOT_UNATTENDED_FLAGS}"),
        "the group autopilot atom must be the allow-all atom plus --autopilot");
}

#[test]
fn claude_permission_mode_maps_unattended() {
    assert_eq!(claude_permission_mode(true), "auto");
    assert_eq!(claude_permission_mode(false), "acceptEdits");
}

/// #465: `read_only` must win regardless of `unattended` — a planner is
/// BOTH unattended and read_only, but the mode it needs is the closed one
/// (`dontAsk`), not `auto`. And `dontAsk` must never reach a non-read-only
/// agent (worker/reviewer): it denies file edits that aren't pre-approved,
/// which is exactly what those roles exist to make.
#[test]
fn claude_effective_permission_mode_is_dontask_only_for_read_only() {
    assert_eq!(claude_effective_permission_mode(true, true), "dontAsk");
    assert_eq!(claude_effective_permission_mode(false, true), "dontAsk");
    assert_eq!(claude_effective_permission_mode(true, false), claude_permission_mode(true));
    assert_eq!(claude_effective_permission_mode(false, false), claude_permission_mode(false));
}

/// #465, the property the whole fix rests on: `dontAsk` only closes the
/// fail-open direction (a NEW Claude Code editing tool silently working
/// because nothing named it in a deny list) if the allow list `dontAsk`
/// checks against never contains a BARE tool grant. A bare `Bash` or a bare
/// `Edit`-class name would pre-approve Claude to use that WHOLE tool freely
/// — present and future capabilities of it — which defeats
/// "everything not pre-approved is denied" just as surely as the old
/// per-name deny list did, only relocated to the allow side. This pins the
/// actual list a read-only agent's `--allowedTools` resolves to: every
/// token must be either the literal `mcp__orrerix` or a SCOPED `Name(...)`
/// pattern, never a bare tool name.
///
/// Mutation evidence (red before green): with `CLAUDE_UNATTENDED_ALLOW`
/// locally changed from `"Bash(git *)" "Bash(gh *)"` to a bare `"Bash"`
/// (simulating exactly the class of regression this test exists to catch),
/// this test fails with `"Bash" in the read-only --allowedTools list is a
/// BARE tool grant...`; restoring the scoped form passes.
#[test]
fn claude_readonly_allowed_tools_contain_no_unscoped_grant() {
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let plan = reg.build_agent_command(
        "claude", "opus", false, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::ReadOnly,
        &PersonaInject::default(),
    );
    let tokens = shell_tokenize(&plan);
    let start = tokens
        .iter()
        .position(|t| t == "--allowedTools")
        .expect("a read-only claude command must set --allowedTools")
        + 1;
    let end = tokens[start..]
        .iter()
        .position(|t| t.starts_with("--"))
        .map(|i| start + i)
        .unwrap_or(tokens.len());
    let allowed = &tokens[start..end];
    assert!(!allowed.is_empty(), "read-only --allowedTools must not be empty under dontAsk");
    for t in allowed {
        assert!(
            t == "mcp__orrerix" || (t.contains('(') && t.ends_with(')')),
            "{t:?} in the read-only --allowedTools list is a BARE tool grant — under \
             dontAsk (#465) a bare grant re-opens the exact fail-open direction this \
             mode exists to close (Claude would be pre-approved to use ALL of that \
             tool, present and future capabilities alike). Every entry must be \
             `mcp__orrerix` or a scoped `Name(...)` pattern."
        );
    }
}

#[test]
fn copilot_single_pane_now_shares_the_group_autopilot_posture() {
    // #364: single-pane and group copilot agents now enter the SAME true
    // autopilot mode (--autopilot, for both the autonomy system-prompt framing
    // AND so the checkbox actually means autopilot on a single pane). The
    // resulting "Enable autopilot mode" dialog is answered on both paths — the
    // group path's kickoff delivery, and a dedicated solo watcher for single
    // panes (`OrchRegistry::confirm_solo_copilot_autopilot`), since a solo pane
    // never receives a programmatic kickoff to hang the confirm off of.
    assert!(COPILOT_GROUP_AUTOPILOT_FLAGS.contains("--autopilot"),
        "group posture enters autopilot mode");
    assert!(COPILOT_UNATTENDED_FLAGS.contains("--allow-all-tools")
        && COPILOT_GROUP_AUTOPILOT_FLAGS.contains("--allow-all-tools"),
        "both postures pre-approve all tools");

    // Single-pane path: --autopilot now included, verbatim the group constant.
    assert!(single_pane_autopilot_flags("copilot").contains("--autopilot"),
        "single-pane copilot must now pass --autopilot when the checkbox is on (#364)");
    assert_eq!(single_pane_autopilot_flags("copilot"), COPILOT_GROUP_AUTOPILOT_FLAGS);

    // Group spawn path: unattended worker + planner both get --autopilot.
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let worker = reg.build_agent_command("copilot", "auto", true, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::None, &PersonaInject::default());
    let planner = reg.build_agent_command("copilot", "auto", false, cfg, None, gdir, Path::new("C:/repo"), None, false, Containment::ReadOnly, &PersonaInject::default());
    assert!(worker.contains("--autopilot") && planner.contains("--autopilot"),
        "group-mode unattended copilot spawns enter true autopilot mode");
}

#[test]
fn copilot_autopilot_prompt_is_detected_only_on_the_real_dialog() {
    // Positive: the exact strings the 1.0.68 TUI paints (title + enable option),
    // ANSI stripped, possibly with a box frame / numbering around them.
    let dialog = "\
        ┌ Enable autopilot mode ─────────────────────────────┐\n\
        │ Autopilot mode works best with all permissions.     │\n\
        │ ❯ 1. Enable all permissions (recommended)           │\n\
        │   2. Continue with limited permissions              │\n\
        │   3. Cancel (Esc)                                    │\n\
        └─────────────────────────────────────────────────────┘";
    assert!(copilot_autopilot_prompt_detected(dialog),
        "the real consent dialog must be recognized");
    // Case-insensitivity (the recognizer lowercases).
    assert!(copilot_autopilot_prompt_detected("ENABLE AUTOPILOT MODE ... Enable All Permissions"));

    // Absent: ordinary agent output must NOT trip it (no stray Enter into a
    // working pane). Each half-phrase alone is insufficient.
    assert!(!copilot_autopilot_prompt_detected(""));
    assert!(!copilot_autopilot_prompt_detected("copilot ready; waiting for your task"));
    assert!(!copilot_autopilot_prompt_detected(
        "I could enable autopilot mode later if you want — say the word."),
        "the title phrase alone (prose) must not match without the enable option");
    assert!(!copilot_autopilot_prompt_detected(
        "run /allow-all to enable all permissions"),
        "the option phrase alone must not match without the dialog title");

    // A DIFFERENT boot-time dialog must not be mistaken for the autopilot one
    // (rev-41): Copilot's folder-trust dialog is also a boxed menu shown at
    // startup and it even mentions "permissions", but neither anchor phrase
    // appears — so the two-anchor detector must reject it (verbatim strings from
    // the 1.0.68 bundle's "Confirm folder trust" dialog).
    let folder_trust = "\
        ┌ Confirm folder trust ───────────────────────────────────────────────┐\n\
        │ C:\\Projects\\loomux                                                   │\n\
        │ Copilot can read files in this folder and, with your permission, edit │\n\
        │ them or run code and shell commands. It will remember your            │\n\
        │ permissions for the rest of this session.                             │\n\
        │ Do you trust the files in this folder?                                │\n\
        │ ❯ 1. Yes                                                              │\n\
        │   2. Yes, and remember this folder                                    │\n\
        │   3. No, exit (Esc)                                                    │\n\
        └───────────────────────────────────────────────────────────────────────┘";
    assert!(!copilot_autopilot_prompt_detected(folder_trust),
        "the folder-trust boot dialog must never be read as the autopilot consent dialog");
    // A login/auth prompt is likewise not the autopilot dialog.
    assert!(!copilot_autopilot_prompt_detected(
        "Your GitHub token may be invalid, expired, or lacking the required permissions — sign in again."),
        "an auth prompt must not match");
}

#[test]
fn autopilot_confirm_gates_to_a_kickoff_of_an_unattended_copilot() {
    // #364: the confirm (and its fail-soft watch) must run on EVERY kickoff of
    // an unattended copilot agent — fresh boot AND resume both show (or must
    // re-show) the "Enable autopilot mode" dialog; only a mid-session delivery
    // (long past boot, no kickoff at all) skips it. `is_kickoff=true` stands in
    // for either Delivery::FreshKickoff or Delivery::ResumeKickoff here — see
    // `delivery_confirms_autopilot_dialog_on_both_fresh_and_resumed_kickoffs`
    // for the enum-level mapping that used to get this wrong (resume was
    // wired to skip it entirely).
    assert!(should_confirm_copilot_autopilot("copilot", true, true));
    // Mid-session (is_kickoff=false) → never, even for copilot.
    assert!(!should_confirm_copilot_autopilot("copilot", true, false),
        "a mid-session delivery must skip the confirm — it's long past boot, no dialog to catch");
    // Attended copilot (no --autopilot passed) shows no dialog → never.
    assert!(!should_confirm_copilot_autopilot("copilot", false, true),
        "an attended copilot agent has no --autopilot, so no dialog to confirm");
    // Claude never shows this dialog → never, regardless of the other flags.
    assert!(!should_confirm_copilot_autopilot("claude", true, true),
        "only copilot has the autopilot consent dialog");
}

#[test]
fn delivery_confirms_autopilot_dialog_on_both_fresh_and_resumed_kickoffs() {
    // #364 root cause: the gate used to be wired off `Delivery::is_fresh_boot`
    // (FreshKickoff only), so a RESUMED copilot session never got its "Enable
    // autopilot mode" dialog answered — the human's report that resume leaves
    // the dialog unanswered (or autopilot unrestored). Both kickoff deliveries
    // must confirm; only a mid-session delivery (already past boot) must not.
    assert!(Delivery::FreshKickoff.confirms_autopilot_dialog());
    assert!(Delivery::ResumeKickoff.confirms_autopilot_dialog(),
        "#364: a resumed copilot kickoff must also answer the autopilot consent dialog");
    assert!(!Delivery::MidSession.confirms_autopilot_dialog());
}

#[test]
fn solo_autopilot_wait_is_far_more_generous_than_the_group_path() {
    // #364: the group path's 12s wait is tuned to loomux's OWN kickoff Enter —
    // the dialog-triggering submit lands within milliseconds of the watch
    // starting. A solo pane's dialog-triggering submit is the HUMAN's own
    // first message, with no lower bound on how long that takes, so the solo
    // watcher needs a far more generous window or it will just miss the human
    // every time.
    assert!(SOLO_AUTOPILOT_DIALOG_WAIT > AUTOPILOT_DIALOG_WAIT,
        "solo must wait meaningfully longer than the group path's near-instant-Enter window");
}

#[test]
fn confirm_solo_copilot_autopilot_gates_on_cli_before_touching_the_app_handle() {
    // #364: a solo pane's autopilot watcher must re-check `cli` itself rather
    // than blindly trusting the caller — a non-copilot pane must no-op WITHOUT
    // even reaching for the app handle (so a claude/hermes/etc. solo launch
    // never pays for or risks a watcher it has no business starting).
    let (reg, _d) = test_registry();
    assert_eq!(reg.confirm_solo_copilot_autopilot(1, "claude"), Ok(()),
        "a non-copilot cli must no-op cleanly, never touching the (here, unset) app handle");
    assert_eq!(reg.confirm_solo_copilot_autopilot(1, "hermes"), Ok(()));

    // A copilot pane clears the gate and DOES try to act — proven here by the
    // fact that it goes on to need the app handle, which this test registry
    // never sets, so it surfaces that error instead of silently no-opping.
    let err = reg.confirm_solo_copilot_autopilot(1, "copilot").unwrap_err();
    assert!(err.contains("no app handle"),
        "a copilot cli must clear the gate and attempt to start the watcher: {err}");
}

#[test]
fn autopilot_confirm_and_stranded_flush_never_both_fire_on_a_fresh_boot() {
    // #99/#179 interaction: the autopilot confirm (Enter on the consent dialog)
    // now runs AFTER the kickoff submit, while #99's stranded-text flush (an
    // Enter to clear a previous prompt still in the box) runs before the paste.
    // Neither can fire on a fresh boot without the other being a no-op: on a
    // freshly booted pane there is no prior delivery, so the flush's own guard
    // (`should_flush_before_paste(None, _)`) is false, regardless of whether the
    // confirm itself gates on fresh-boot-only or (post-#364) fresh-or-resume.
    // This pins that composition — if either guard's contract changes, this
    // fails.
    // Fresh boot ⇒ confirm may run …
    assert!(should_confirm_copilot_autopilot("copilot", true, true));
    // … but the flush cannot: no previous delivery to key off (prev = None).
    assert!(!should_flush_before_paste(None, false),
        "a fresh-boot pane has no prior delivery, so the flush never fires alongside the confirm");
    assert!(!should_flush_before_paste(None, true));
}

#[test]
fn copilot_autopilot_confirm_reuses_the_copilot_submit_transport() {
    // #179: the confirm answers the "Enable autopilot mode" dialog copilot opens
    // in response to the kickoff submit. The dialog default "Enable all
    // permissions" is selected with Enter (menu initialIndex 0). The keys carry
    // the focus-in prefix so this stays identical to every other copilot pane
    // write (#98) — pin the two together so they can't silently drift apart.
    assert_eq!(COPILOT_AUTOPILOT_CONFIRM_KEYS, b"\x1b[I\r");
    assert!(COPILOT_AUTOPILOT_CONFIRM_KEYS.ends_with(b"\r"),
        "the selection key is Enter (menu initialIndex 0 = Enable all permissions)");
    assert_eq!(COPILOT_AUTOPILOT_CONFIRM_KEYS, submit_sequence("copilot"),
        "the autopilot confirm reuses copilot's focus-in+Enter transport");
}

#[test]
fn terminal_query_replies_are_not_read_as_human_input() {
    // #179 root cause: a fresh copilot pane queries the terminal's colors and
    // version at boot (`ESC]10;?`, `ESC]11;?`, `ESC]4;n;?`, `ESC[>q`); the
    // webview's xterm auto-answers, and those answers reach `classify_human_input`
    // through `write_pty` exactly like a keystroke. Their bodies are printable, so
    // when the classifier only skipped CSI they were read as a human's line —
    // wedging `input_pending` true and stalling the kickoff paste in the #111
    // box-clear hold until it aborted ("prompt never delivered"). A terminal
    // reply must classify Neutral (it changes no box occupancy), never Content.

    // OSC 11 (background color) reply — BEL-terminated, as xterm sends it.
    assert_eq!(classify_human_input("\x1b]11;rgb:0d0d/1111/1717\x07"), HumanInput::Neutral,
        "an OSC color-query reply is a terminal answer, not typed input");
    // OSC 10 (foreground) reply — ST-terminated (ESC \) form.
    assert_eq!(classify_human_input("\x1b]10;rgb:f0f6/f0f6/fcfc\x1b\\"), HumanInput::Neutral);
    // OSC 4 palette-entry reply.
    assert_eq!(classify_human_input("\x1b]4;1;rgb:ffff/0000/0000\x07"), HumanInput::Neutral);
    // DCS reply to XTVERSION (`ESC[>q`) — `ESC P > | xterm(...) ESC \`.
    assert_eq!(classify_human_input("\x1bP>|xterm(370)\x1b\\"), HumanInput::Neutral,
        "a DCS version reply is a terminal answer, not typed input");
    // Several batched into one write (xterm can coalesce replies) — still Neutral.
    assert_eq!(
        classify_human_input("\x1b]10;rgb:f0f6/f0f6/fcfc\x07\x1b]11;rgb:0d0d/1111/1717\x07"),
        HumanInput::Neutral,
    );
    // A CSI DA/DSR reply was already Neutral — keep it that way.
    assert_eq!(classify_human_input("\x1b[?64;1;2;6;9;15;18;21;22c"), HumanInput::Neutral);
    assert_eq!(classify_human_input("\x1b[24;80R"), HumanInput::Neutral);

    // Guardrail: a real typed line that merely *follows* a query reply in the same
    // write must still register as Content — skipping the reply must not swallow
    // the human's text after it.
    assert_eq!(classify_human_input("\x1b]11;rgb:0d0d/1111/1717\x07hello"), HumanInput::Content,
        "typed text after a query reply is still a human's unsubmitted line");
}

#[test]
fn build_agent_command_full_line_snapshots() {
    // Snapshot the ENTIRE command line for a representative matrix (rev-33
    // note). The other build_agent_command tests inspect the refactor with
    // `.contains()`, which can't catch a stray space, a dropped flag, or a
    // reordered fragment; asserting the full string pins the exact output so
    // any future drift in the shared flag atoms fails loudly here. Fixed paths
    // (no session/resume) keep the strings deterministic.
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");
    // signature: (cli, model, auto_ops, cfg, group_dir, workdir, session, resume, containment)
    let cmd = |cli, model, auto_ops, containment| {
        reg.build_agent_command(cli, model, auto_ops, cfg, None, gdir, wd, None, false, containment, &PersonaInject::default())
    };

    // Claude worker, auto_ops ON → native Auto mode + git/gh pre-approval.
    // No `hook_settings` passed here (`None`) ⇒ no `--settings` at all — see
    // `claude_worker_gets_a_settings_flag_only_when_hook_settings_is_some`
    // below for the #417 flag itself, split from `--mcp-config`'s file per
    // rev-4 review N2.
    assert_eq!(
        cmd("claude", "sonnet", true, Containment::None),
        "claude --mcp-config \"C:/x/cfg.json\" --strict-mcp-config \
         --model sonnet --permission-mode auto --add-dir \"C:/data/group\" --allowedTools mcp__orrerix \
         \"Bash(git *)\" \"Bash(gh *)\""
    );

    // Claude worker, auto_ops OFF → acceptEdits, no git/gh, no denials.
    assert_eq!(
        cmd("claude", "sonnet", false, Containment::None),
        "claude --mcp-config \"C:/x/cfg.json\" --strict-mcp-config \
         --model sonnet --permission-mode acceptEdits --add-dir \"C:/data/group\" --allowedTools mcp__orrerix"
    );

    // Copilot worker, auto_ops ON → group autopilot: --autopilot + all tools/paths.
    assert_eq!(
        cmd("copilot", "auto", true, Containment::None),
        "copilot --additional-mcp-config \"@C:/x/cfg.json\" --model auto \
         --add-dir \"C:/data/group\" --add-dir \"C:/repo\" --no-auto-update \
         --autopilot --allow-all-tools --allow-all-paths --allow-tool \"orrerix\""
    );

    // Copilot worker, auto_ops OFF → the conservative git/gh allowlist branch.
    assert_eq!(
        cmd("copilot", "auto", false, Containment::None),
        "copilot --additional-mcp-config \"@C:/x/cfg.json\" --model auto \
         --add-dir \"C:/data/group\" --add-dir \"C:/repo\" --no-auto-update \
         --allow-tool \"orrerix,shell(git:*),shell(gh:*)\""
    );

    // Claude planner (read_only) in a NON-auto_ops group → unattended anyway,
    // running `dontAsk` (#465 — pre-approved tools only, so an editing tool
    // Claude adds tomorrow is denied by construction) rather than `auto`,
    // plus the write/commit/push denials, gh still reachable. Literals per
    // CLAUDE_EDIT_DENY_TOOLS/_GIT (#448 — `MultiEdit` dropped, it matched
    // no real Claude Code tool).
    assert_eq!(
        cmd("claude", "opus", false, Containment::ReadOnly),
        "claude --mcp-config \"C:/x/cfg.json\" --strict-mcp-config \
         --model opus --permission-mode dontAsk --add-dir \"C:/data/group\" --allowedTools mcp__orrerix \
         \"Bash(git *)\" \"Bash(gh *)\" --disallowedTools Edit Write NotebookEdit \
         \"Bash(git commit *)\" \"Bash(git push *)\""
    );

    // Copilot planner (read_only) in a NON-auto_ops group → group autopilot
    // (--autopilot + all tools/paths) + deny rules; gh not denied. Literals
    // per COPILOT_EDIT_DENY_TOOLS/_GIT (#448 — `edit` dropped, it is not
    // one of Copilot's three documented --deny-tool value shapes).
    assert_eq!(
        cmd("copilot", "auto", false, Containment::ReadOnly),
        "copilot --additional-mcp-config \"@C:/x/cfg.json\" --model auto \
         --add-dir \"C:/data/group\" --add-dir \"C:/repo\" --no-auto-update \
         --autopilot --allow-all-tools --allow-all-paths --allow-tool \"orrerix\" \
         --deny-tool \"write,shell(git commit),shell(git push)\""
    );

    // #462 — a reviewer (`NoEdits`), the tier between the two above. Every
    // difference from the worker row on the SAME auto_ops setting must be the
    // deny flags and nothing else: same permission mode, same allow list. The
    // git denials are absent by design (a reviewer's shell is its job), and
    // `--permission-mode` is NOT promoted to `auto` the way a planner's is —
    // `NoEdits` narrows a reviewer, it must never widen one.
    assert_eq!(
        cmd("claude", "sonnet", true, Containment::NoEdits),
        "claude --mcp-config \"C:/x/cfg.json\" --strict-mcp-config \
         --model sonnet --permission-mode auto --add-dir \"C:/data/group\" --allowedTools mcp__orrerix \
         \"Bash(git *)\" \"Bash(gh *)\" --disallowedTools Edit Write NotebookEdit"
    );
    assert_eq!(
        cmd("claude", "sonnet", false, Containment::NoEdits),
        "claude --mcp-config \"C:/x/cfg.json\" --strict-mcp-config \
         --model sonnet --permission-mode acceptEdits --add-dir \"C:/data/group\" --allowedTools mcp__orrerix \
         --disallowedTools Edit Write NotebookEdit"
    );
    assert_eq!(
        cmd("copilot", "auto", true, Containment::NoEdits),
        "copilot --additional-mcp-config \"@C:/x/cfg.json\" --model auto \
         --add-dir \"C:/data/group\" --add-dir \"C:/repo\" --no-auto-update \
         --autopilot --allow-all-tools --allow-all-paths --allow-tool \"orrerix\" \
         --deny-tool \"write\""
    );
    assert_eq!(
        cmd("copilot", "auto", false, Containment::NoEdits),
        "copilot --additional-mcp-config \"@C:/x/cfg.json\" --model auto \
         --add-dir \"C:/data/group\" --add-dir \"C:/repo\" --no-auto-update \
         --allow-tool \"orrerix,shell(git:*),shell(gh:*)\" --deny-tool \"write\""
    );

    // Unknown CLI falls back to the claude adapter byte-for-byte (never a
    // silent half-built command) — same string as the claude worker case.
    assert_eq!(
        cmd("totally-unknown-cli", "sonnet", true, Containment::None),
        cmd("claude", "sonnet", true, Containment::None),
        "an unrecognized CLI must build the exact claude fallback command"
    );
}

#[test]
fn claude_gets_a_settings_flag_only_when_hook_settings_is_some() {
    // #417, split from `--mcp-config`'s file per rev-4 review N2: `--settings`
    // is a SEPARATE file/flag, present only when the caller actually has one
    // (Claude with a resolvable hook script) — never emitted just because
    // the agent is Claude, and never pointed at `cfg` (the mcp-config file).
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let hooks = Path::new("C:/x/cfg-hooks.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");

    let cmd = reg.build_agent_command(
        "claude", "sonnet", true, cfg, Some(hooks), gdir, wd, None, false, Containment::None, &PersonaInject::default(),
    );
    assert!(cmd.contains("--settings \"C:/x/cfg-hooks.json\""), "{cmd}");
    assert!(!cmd.contains("--settings \"C:/x/cfg.json\""), "must never point --settings at the mcp-config file: {cmd}");

    let argv = reg.build_agent_argv(
        "claude", "sonnet", true, cfg, Some(hooks), gdir, wd, None, false, Containment::None, &PersonaInject::default(),
    );
    assert!(argv.windows(2).any(|w| w == ["--settings", "C:/x/cfg-hooks.json"]), "{argv:?}");
    assert_eq!(shell_tokenize(&cmd), argv, "the two forms must never drift on the new flag either");

    // Copilot never gets a --settings flag, `hook_settings` or not — it isn't
    // wired through the copilot branch at all (copilot has no hook config).
    let copilot_cmd = reg.build_agent_command(
        "copilot", "auto", true, cfg, Some(hooks), gdir, wd, None, false, Containment::None, &PersonaInject::default(),
    );
    assert!(!copilot_cmd.contains("--settings"), "{copilot_cmd}");
}

/// The `--allowedTools` value list emitted for a claude spawn, as argv tokens:
/// everything between the flag and the next `--`-prefixed token. Claude's
/// [CLI reference](https://code.claude.com/docs/en/cli-reference) documents
/// `--allowedTools` as taking space-separated values in ONE occurrence
/// (its own example: `"Bash(git log *)" "Bash(git diff *)" "Read"`), so any
/// other flag emitted mid-list terminates the list — everything after it is a
/// stray positional, not an allow rule.
pub(crate) fn claude_allowed_tools_values(tokens: &[String]) -> Vec<String> {
    claude_flag_values(tokens, "--allowedTools")
}

/// The same extraction for any one flag's value list. Shared so the deny side
/// (#614 review N1) is pinned by the same rule as the allow side rather than
/// by a second, subtly different reimplementation of "where does a value list
/// end".
fn claude_flag_values(tokens: &[String], flag: &str) -> Vec<String> {
    let start = tokens
        .iter()
        .position(|t| t == flag)
        .unwrap_or_else(|| panic!("a claude command must set {flag}: {tokens:?}"))
        + 1;
    let end = tokens[start..]
        .iter()
        .position(|t| t.starts_with("--"))
        .map(|i| start + i)
        .unwrap_or(tokens.len());
    tokens[start..end].to_vec()
}

/// #610, the root cause: `--settings` (#417) is emitted BETWEEN
/// `--allowedTools`'s first value and the git/gh patterns that follow it, so
/// on every spawn that actually has a hook-settings file — i.e. every real
/// Claude spawn — the value list Claude parses is just `mcp__orrerix` and the
/// patterns land as stray positional arguments. That is invisible on a worker
/// (`--permission-mode auto` approves git/gh without consulting the allow
/// list) and total on a planner (`dontAsk` denies everything not
/// pre-approved), which is exactly the reported symptom: three planners with
/// `gh` entirely denied while read-only `git` — Claude's own built-in
/// read-only Bash carve-out, no allow rule involved — kept working.
///
/// The one token that survived the truncation, `mcp__orrerix`, is also the one
/// capability planners demonstrably kept (`report`/`message_orchestrator`
/// worked throughout), which is what rules OUT "dontAsk ignores
/// `--allowedTools` entirely" as the explanation.
///
/// So this pins contiguity, not mere presence: every allow pattern must be
/// inside the flag's own value list, for both spawn forms and every tier.
#[test]
fn claude_allow_patterns_are_not_severed_from_the_allowedtools_flag() {
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let hooks = Path::new("C:/x/cfg-hooks.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");
    let persona = PersonaInject { extra_allow: vec!["Bash(make:*)".into()], ..PersonaInject::default() };

    for containment in [Containment::None, Containment::NoEdits, Containment::ReadOnly] {
        // A read-only block never carries `extra_allow` (#222's capability
        // closure empties it) — pass it only where it can legitimately appear.
        let p = if containment == Containment::ReadOnly { PersonaInject::default() } else { persona.clone() };
        let mut want = vec!["mcp__orrerix", "Bash(git *)", "Bash(gh *)"];
        if containment != Containment::ReadOnly {
            want.push("Bash(make:*)");
        }
        let line = reg.build_agent_command(
            "claude", "opus", true, cfg, Some(hooks), gdir, wd, None, false, containment, &p,
        );
        let argv = reg.build_agent_argv(
            "claude", "opus", true, cfg, Some(hooks), gdir, wd, None, false, containment, &p,
        );
        for (form, tokens) in [("command", shell_tokenize(&line)), ("argv", argv)] {
            let allowed = claude_allowed_tools_values(&tokens);
            for w in &want {
                assert!(
                    allowed.iter().any(|t| t == w),
                    "#610: {w:?} is missing from the {form} form's --allowedTools value list \
                     for {containment:?} — it was emitted AFTER another flag (--settings), which \
                     terminates the list, so Claude never receives it as an allow rule at all. \
                     Value list seen: {allowed:?}\n  line: {line}"
                );
            }
        }
    }
}

/// #614 review N1: the same contiguity property, for the DENY list — the
/// direction that fails **open**.
///
/// The deny list is contiguous today, so this pins no live defect. What it
/// closes is the coverage gap that let #610 live for two releases: every
/// existing pin on the deny list's ordering (the full-line goldens, the
/// `windows(4)` argv check) is built with `hook_settings: None`, and
/// `--settings` is a flag that appears ONLY on a real spawn. A future flag with
/// that same "only present sometimes" shape, emitted between
/// `--disallowedTools` and its values, would sever the git denials with every
/// other test green — and unlike a severed allow, a severed deny is silent: the
/// planner just quietly gains `git commit`.
#[test]
fn claude_deny_patterns_are_not_severed_from_the_disallowedtools_flag() {
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let hooks = Path::new("C:/x/cfg-hooks.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");

    for (containment, want) in [
        (Containment::ReadOnly, [CLAUDE_EDIT_DENY_TOOLS, CLAUDE_READONLY_DENY_GIT].concat()),
        (Containment::NoEdits, CLAUDE_EDIT_DENY_TOOLS.to_vec()),
    ] {
        let line = reg.build_agent_command(
            "claude", "opus", true, cfg, Some(hooks), gdir, wd, None, false, containment,
            &PersonaInject::default(),
        );
        let argv = reg.build_agent_argv(
            "claude", "opus", true, cfg, Some(hooks), gdir, wd, None, false, containment,
            &PersonaInject::default(),
        );
        for (form, tokens) in [("command", shell_tokenize(&line)), ("argv", argv)] {
            let denied = claude_flag_values(&tokens, "--disallowedTools");
            for w in &want {
                assert!(
                    denied.iter().any(|t| t == w),
                    "#614/N1: {w:?} is missing from the {form} form's --disallowedTools value \
                     list for {containment:?} — a flag emitted mid-list would terminate it and \
                     silently drop the denial. Value list seen: {denied:?}\n  line: {line}"
                );
            }
        }
    }
}

/// #610: `dontAsk` is documented against `permissions.allow` — per the
/// [permission modes reference](https://code.claude.com/docs/en/permission-modes)
/// ("Allow only pre-approved tools with dontAsk mode"), Claude "runs only
/// actions matching your `permissions.allow` rules, read-only Bash commands,
/// and calls approved by a PreToolUse hook". That is a SETTINGS-file concept,
/// and the `--settings` file loomux already writes (#417) is the layer loomux
/// controls, so a read-only pane's allow rules belong there and not only on
/// argv.
///
/// Both halves are pinned here, because the second is the one the old code
/// couldn't do at all: the settings file used to be written ONLY when there
/// were hooks to put in it (no `sh` resolvable ⇒ `None` ⇒ no `--settings`
/// flag), which would have left a planner with no allow rules on exactly the
/// machines where hook provisioning fails.
#[test]
fn readonly_pane_settings_carry_permissions_allow() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let p = reg.spawn_agent(&g.id, Role::Planner, "p", "plan it", false, None).unwrap();

    let path = reg.state_root().join(g.id.as_str()).join("configs").join(format!("{}-hooks.json", p.id));
    let cfg: Value = serde_json::from_str(
        &fs::read_to_string(&path).unwrap_or_else(|e| panic!("#610: no settings file at {path:?}: {e}")),
    )
    .unwrap();
    let allow: Vec<String> = cfg["permissions"]["allow"]
        .as_array()
        .unwrap_or_else(|| panic!("#610: read-only pane's settings file has no permissions.allow: {cfg}"))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();

    // The capabilities a planner's own job needs: `gh` for `gh issue view` and
    // its `gh issue comment` deliverable, `git` for read-only exploration AND
    // `git fetch` (a git subcommand, so `Bash(git *)` covers it — the ReadOnly
    // deny list carves out only `commit`/`push`, and deny beats allow), and
    // the loomux MCP server for `report`/`message_orchestrator`.
    for want in ["mcp__orrerix", "Bash(git *)", "Bash(gh *)"] {
        assert!(allow.iter().any(|r| r == want), "#610: {want:?} missing from {allow:?}");
    }
    // The research capability the same issue decided on: a planner must be
    // able to ground a plan in a vendor's own reference docs (the
    // `agent-cli-reference` skill requires exactly that), which `gh` cannot
    // reach and the built-in read-only Bash set does not include.
    for want in ["WebFetch", "WebSearch"] {
        assert!(allow.iter().any(|r| r == want), "#610: {want:?} missing from {allow:?}");
    }

    // #465's invariant, restated for this layer: an allow list a `dontAsk`
    // pane is judged against must never contain a bare grant of a
    // MUTATION-capable tool — that re-opens the fail-open direction (a new
    // editing tool nothing denies by name) from the allow side. `WebFetch`/
    // `WebSearch` are the two deliberate bare entries, and they are bare
    // honestly: per the permissions reference `WebFetch(domain:*)` "is
    // equivalent to a bare `WebFetch` rule", so a scoped-looking spelling
    // would be decoration, not a narrowing.
    const RESEARCH_TOOLS: [&str; 2] = ["WebFetch", "WebSearch"];
    for rule in &allow {
        let scoped = rule.contains('(') && rule.ends_with(')');
        assert!(
            rule == "mcp__orrerix" || scoped || RESEARCH_TOOLS.contains(&rule.as_str()),
            "#610/#465: {rule:?} is a BARE tool grant in a dontAsk pane's permissions.allow. \
             Only `mcp__orrerix`, a scoped `Name(...)` pattern, or one of the enumerated \
             non-mutating research tools {:?} may appear here.",
            RESEARCH_TOOLS
        );
    }
    for denied in CLAUDE_EDIT_DENY_TOOLS {
        assert!(!allow.iter().any(|r| r == denied), "#610: {denied:?} must never be allowed: {allow:?}");
    }
    // #448's drift pin, applied to the allow side: a mistyped or upstream-
    // renamed BARE tool name in an allow rule matches nothing and silently
    // grants nothing — the same invisible-failure shape a dead deny entry had,
    // and worse here, since the symptom is a planner that mysteriously can't
    // fetch rather than a startup warning. (Scoped `Name(...)` patterns and
    // `mcp__*` rules are exempt for the same reason they are on the deny side.)
    for rule in &allow {
        if rule.starts_with("mcp__") || rule.contains('(') {
            continue;
        }
        assert!(
            KNOWN_CLAUDE_TOOLS.contains(&rule.as_str()),
            "#610/#448: {rule:?} is not a known Claude Code tool name — an allow rule that \
             matches no tool grants nothing, silently. See KNOWN_CLAUDE_TOOLS' refresh procedure."
        );
    }

    // #614 review B1: `Bash(git *)` is a PREFIX pattern, so as a live
    // `permissions.allow` rule it positively matches `git commit -m …` and
    // `git push`. The carve-out must therefore be reachable from inside this
    // same object, not only from `--disallowedTools` in another layer —
    // otherwise the read-only tier's one guarantee (no mutation) rests on a
    // cross-mechanism precedence that this PR's own reasoning refuses to
    // assume for allows. Deny is derived from the same predicates argv uses,
    // so a new Containment tier moves both layers or neither.
    let deny: Vec<String> = cfg["permissions"]["deny"]
        .as_array()
        .unwrap_or_else(|| panic!("#614/B1: read-only pane's settings carry no permissions.deny: {cfg}"))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    for want in CLAUDE_EDIT_DENY_TOOLS.iter().chain(CLAUDE_READONLY_DENY_GIT.iter()) {
        assert!(
            deny.iter().any(|r| r == want),
            "#614/B1: {want:?} missing from the settings deny list {deny:?} — the argv \
             --disallowedTools value alone would leave it in a different precedence layer \
             from the allow rule it has to beat."
        );
    }

    // The file is only useful if the pane is actually pointed at it — asserted
    // on the SPAWN REQUEST, the command line the pane is really launched with.
    let req = reg.spawn_request_for_test(&p.id).expect("no spawn request for the planner");
    assert!(req.command.contains("--settings"), "{}", req.command);
    assert!(
        req.argv.windows(2).any(|w| w[0] == "--settings" && w[1] == path.display().to_string()),
        "the planner's argv must point --settings at its own settings file: {:?}",
        req.argv
    );

    // #614 review N4: the two layers must be the SAME LIST, not merely two
    // lists that each happen to contain today's constants. The loop above
    // pins presence, which covers the dangerous direction for the two deny
    // constants that exist now — but a future third constant added to the
    // argv branch alone would be forced onto the argv side by the exact-
    // equality full-line goldens and forced onto the settings side by
    // nothing, leaving a denial in one layer only while `Bash(git *)` stays
    // live in `permissions.allow`. That is this round's B1 finding again, one
    // constant later. Equality, taken off the real spawn request, is what
    // actually states the property the design claims. Sorted rather than
    // order-sensitive: the two layers are sets of rules, and the ORDER within
    // `--disallowedTools` is already pinned by the full-line goldens.
    let mut argv_deny = claude_flag_values(&req.argv, "--disallowedTools");
    let mut settings_deny = deny.clone();
    argv_deny.sort();
    settings_deny.sort();
    assert_eq!(
        argv_deny, settings_deny,
        "#614/N4: a read-only pane's --disallowedTools values and its settings \
         permissions.deny must be the same list — one layer gained or lost a denial the \
         other didn't. argv={argv_deny:?} settings={settings_deny:?}"
    );
}

/// #610, the half that could not work before: the settings file must be
/// written for a read-only pane even when there is NOTHING to put in the
/// `hooks` key — `write_hook_settings_file` used to return `None` in that
/// case and the caller then omitted `--settings` entirely, which is a
/// fail-open policy that is right for hooks (a missing compact nudge) and
/// wrong for permissions (a planner with no allow rules under `dontAsk` can
/// do nothing at all).
///
/// The no-hooks state is forced the way it happens in the wild — hook
/// provisioning fails — by pointing the hook-script directory at a path under
/// a regular FILE, so `create_dir_all` cannot succeed on any platform.
#[test]
fn readonly_pane_gets_a_settings_file_even_when_hook_provisioning_fails() {
    let (reg, d) = test_registry();
    let blocker = d.path().join("not-a-directory");
    fs::write(&blocker, b"x").unwrap();
    reg.set_compact_hook_dir_override(blocker.join("compacthook"));

    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let p = reg.spawn_agent(&g.id, Role::Planner, "p", "plan it", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "do it", false, None).unwrap();

    let settings = |id: &str| reg.state_root().join(g.id.as_str()).join("configs").join(format!("{id}-hooks.json"));
    let cfg: Value = serde_json::from_str(
        &fs::read_to_string(settings(&p.id))
            .unwrap_or_else(|e| panic!("#610: a read-only pane must still get a settings file: {e}")),
    )
    .unwrap();
    assert!(cfg.get("hooks").is_none(), "no hooks were resolvable, so no hooks key may be written: {cfg}");
    assert!(
        cfg["permissions"]["allow"].as_array().is_some_and(|a| !a.is_empty()),
        "#610: permissions.allow must be written even with no hooks: {cfg}"
    );
    let planner_cmd = reg.spawn_request_for_test(&p.id).expect("no spawn request for the planner").command;
    assert!(planner_cmd.contains("--settings"), "{planner_cmd}");

    // The fail-open policy is unchanged for a pane with nothing to put in the
    // file: a worker has no permissions block of its own, so with hooks
    // unavailable there is still no settings file and no flag.
    let worker_cmd = reg.spawn_request_for_test(&w.id).expect("no spawn request for the worker").command;
    assert!(!settings(&w.id).exists(), "a non-read-only pane with no hooks must get no settings file");
    assert!(!worker_cmd.contains("--settings"), "{worker_cmd}");
}

/// Split a shell command line into argv, honoring the two quotings
/// `build_agent_command` emits. Double quotes wrap paths and tool patterns
/// (`--add-dir "C:/a b"`, `"Bash(git *)"`, `@"C:/x"` → `@C:/x`); single quotes
/// wrap the `--agents` JSON payload, whose body is full of double quotes
/// (#222) — which is exactly why that one is single-quoted, in both PowerShell
/// and POSIX sh.
pub(crate) fn shell_tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false; // distinguishes "" (empty token) from whitespace
    for c in line.chars() {
        match c {
            '"' | '\'' if quote.is_none() => {
                quote = Some(c);
                started = true;
            }
            _ if quote == Some(c) => {
                quote = None;
                started = true;
            }
            ' ' | '\t' if quote.is_none() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            _ => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

#[test]
fn build_agent_argv_snapshots() {
    // The structured (direct-spawn) form, pinned per CLI (issue #78, #102-style).
    // A native-exe agent pane spawns exactly program=argv[0] with these literal
    // args — no shell, no quoting. Fixed paths keep it deterministic.
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");
    let argv =
        |cli, model, auto_ops, containment| reg.build_agent_argv(cli, model, auto_ops, cfg, None, gdir, wd, None, false, containment, &PersonaInject::default());

    // Claude worker, auto_ops ON. Note the quote-free literal tool tokens.
    // No `hook_settings` (`None`) ⇒ no `--settings` — see
    // `claude_gets_a_settings_flag_only_when_hook_settings_is_some` for that.
    assert_eq!(
        argv("claude", "sonnet", true, Containment::None),
        vec![
            "claude", "--mcp-config", "C:/x/cfg.json", "--strict-mcp-config", "--model", "sonnet",
            "--permission-mode", "auto", "--add-dir", "C:/data/group", "--allowedTools",
            "mcp__orrerix", "Bash(git *)", "Bash(gh *)",
        ]
    );

    // #462 — the reviewer tier, structurally: the SAME tokens as the worker row
    // above plus the deny pair, and (unlike a planner) nothing removed and no
    // git denials. Pinned on both CLIs because each spells the denial its own
    // way — a regression that dropped one adapter's flags would still pass the
    // other's row.
    assert_eq!(
        argv("claude", "sonnet", true, Containment::NoEdits),
        vec![
            "claude", "--mcp-config", "C:/x/cfg.json", "--strict-mcp-config", "--model", "sonnet",
            "--permission-mode", "auto", "--add-dir", "C:/data/group", "--allowedTools",
            "mcp__orrerix", "Bash(git *)", "Bash(gh *)", "--disallowedTools", "Edit", "Write",
            "NotebookEdit",
        ]
    );
    assert_eq!(
        argv("copilot", "auto", true, Containment::NoEdits),
        vec![
            "copilot", "--additional-mcp-config", "@C:/x/cfg.json", "--model", "auto", "--add-dir",
            "C:/data/group", "--add-dir", "C:/repo", "--no-auto-update",
            "--autopilot", "--allow-all-tools", "--allow-all-paths",
            "--allow-tool", "orrerix", "--deny-tool", "write",
        ]
    );

    // Copilot planner (ReadOnly): group autopilot + deny rules; @ rides the cfg.
    // #448: `edit` dropped — not one of Copilot's three documented --deny-tool
    // value shapes (shell(COMMAND) / write / MCP_SERVER(tool)).
    assert_eq!(
        argv("copilot", "auto", false, Containment::ReadOnly),
        vec![
            "copilot", "--additional-mcp-config", "@C:/x/cfg.json", "--model", "auto", "--add-dir",
            "C:/data/group", "--add-dir", "C:/repo", "--no-auto-update",
            "--autopilot", "--allow-all-tools", "--allow-all-paths",
            "--allow-tool", "orrerix",
            "--deny-tool", "write,shell(git commit),shell(git push)",
        ]
    );

    // The program is always argv[0] — what the pane spawns directly.
    assert_eq!(argv("claude", "sonnet", false, Containment::None)[0], "claude");
    assert_eq!(argv("copilot", "auto", false, Containment::None)[0], "copilot");
    // Unknown CLI → claude adapter, structurally too.
    assert_eq!(argv("totally-unknown-cli", "sonnet", true, Containment::None)[0], "claude");
}

#[test]
fn build_agent_argv_matches_command_line() {
    // Drift guard: the structured argv must be exactly the tokenization of the
    // shell command line across the full matrix, session/resume included. This
    // is what lets both forms coexist (direct spawn + shell fallback) without
    // ever describing a different invocation.
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");
    let sid = "11111111-2222-3333-4444-555555555555";
    let sessions: [(Option<&str>, bool); 3] =
        [(None, false), (Some(sid), false), (Some(sid), true)];
    // The matrix now includes the #222 persona flags. Round #417 correction
    // 6 replaced the inline `--agents '<json>'` token (the one loomux used
    // to single-quote, stuffed with double quotes/spaces/escapes — by far
    // the likeliest place for the two forms to drift) with a short
    // `--agent <handle>` naming a generated FILE, or (the write-failure
    // fallback) `--append-system-prompt-file "<path>"` — a path can still
    // contain spaces, so it's still worth its own matrix entry.
    let personas: [PersonaInject; 6] = [
        PersonaInject::default(),
        PersonaInject { claude_agent: Some("loomux-g-1-rev-sec".into()), ..PersonaInject::default() },
        PersonaInject {
            claude_append_system_prompt_file: Some(PathBuf::from("C:/data/group/rev sec.md")),
            ..PersonaInject::default()
        },
        PersonaInject { copilot_agent: Some("repo-worker".into()), ..PersonaInject::default() },
        PersonaInject {
            extra_allow: vec!["Bash(make:*)".into(), "mcp__probe".into()],
            ..PersonaInject::default()
        },
        // #722: opencode's own `--agent <handle>`. Its prompt FILE never
        // reaches either form (it is referenced from the generated config
        // document, not from argv), so the handle is the whole surface.
        PersonaInject {
            opencode_agent: Some("loomux-g-1-rev-sec".into()),
            opencode_prompt_file: Some(PathBuf::from("C:/data/group/configs/loomux-g-1-rev sec.md")),
            ..PersonaInject::default()
        },
    ];
    // #610: `hook_settings` was fixed at `None` across this whole matrix, so
    // the ONE flag that only ever appears on a real spawn — `--settings`, and
    // with it every ordering question its position raises — was never
    // tokenized here at all. Both states now ride the matrix.
    let hooks = PathBuf::from("C:/x/cfg-hooks.json");
    // #687: the model knobs ride the matrix too, and the `[1m]` case is the one
    // that earns its place — it is the only value either form QUOTES
    // differently (`--model "sonnet[1m]"` in the shell form, one bare token in
    // argv), so a drift in that composition is invisible to every other entry.
    let knob_sets = [
        workflow::ModelKnobs::default(),
        workflow::ModelKnobs { effort: "xhigh", context: "" },
        workflow::ModelKnobs { effort: "", context: "1m" },
        workflow::ModelKnobs { effort: "low", context: "1m" },
    ];
    // #946 Q4 / #1091 slice H: the role dimension this PR added.
    // `Role::Worker, None` stays first — inert for
    // `claude_denies_interactive_question`, i.e. byte-identical to the
    // pre-Q4 matrix — with the two denying combinations swept alongside it
    // so the total string-vs-argv equivalence this matrix exists to pin
    // actually covers the new dimension instead of fixing it away.
    let roles: [(Role, Option<&str>); 3] =
        [(Role::Worker, None), (Role::Orchestrator, None), (Role::Reviewer, Some("liaison"))];
    // Every adapter, not just the two the matrix started with: an arm only one
    // of the two builders emits is precisely the drift this exists to catch,
    // and a CLI absent from the list is an arm nobody is checking.
    // codex joined with #2515 C1. Its arms are the ones this matrix is most
    // worth running over: they are the only pair where the two forms differ in
    // SHAPE rather than only in quoting — the string form appends
    // ` resume <id>` while the argv form pushes two tokens — so a divergence
    // here is a real possibility rather than a formality.
    for cli in ["claude", "codex", "copilot", "gemini", "opencode", "pi", "totally-unknown-cli"] {
        for auto_ops in [false, true] {
            // Every tier, not just the two that existed before #462 — a
            // middle tier that only one of the two forms emits is exactly the
            // drift this test exists to catch.
            for containment in [Containment::None, Containment::NoEdits, Containment::ReadOnly] {
                for hook_settings in [None, Some(hooks.as_path())] {
                    for (session, resume) in sessions {
                        for persona in &personas {
                            for knobs in knob_sets {
                                for (role, role_hint) in roles {
                                    let line = reg.build_agent_command_ex(
                                        cli, "m", knobs, auto_ops, cfg, hook_settings, gdir, wd, session, resume, containment, persona,
                                        role, role_hint,
                                    None,
                                    ).unwrap();
                                    let argv = reg.build_agent_argv_ex(
                                        cli, "m", knobs, auto_ops, cfg, hook_settings, gdir, wd, session, resume, containment, persona,
                                        role, role_hint,
                                    None,
                                    ).unwrap();
                                    assert_eq!(
                                        shell_tokenize(&line),
                                        argv,
                                        "argv must equal the tokenized command line for \
                                         cli={cli} auto_ops={auto_ops} containment={containment:?} \
                                         hook_settings={hook_settings:?} knobs={knobs:?} \
                                         session={session:?} resume={resume} persona={persona:?} \
                                         role={role:?} role_hint={role_hint:?}\n  line: {line}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // #722: the one input the matrix above cannot carry — an EMPTY model. It
    // is opencode's normal state (`default_model` returns empty so the pane
    // inherits the human's own choice), and it is the case where the two forms
    // could most easily disagree: a string builder that emits `--model ` with
    // nothing after it tokenizes to one token where argv has two.
    for containment in [Containment::None, Containment::NoEdits, Containment::ReadOnly] {
        for (session, resume) in sessions {
            for persona in &personas {
                let line = reg.build_agent_command(
                    "opencode", "", true, cfg, None, gdir, wd, session, resume, containment, persona,
                );
                let argv = reg.build_agent_argv(
                    "opencode", "", true, cfg, None, gdir, wd, session, resume, containment, persona,
                );
                assert_eq!(shell_tokenize(&line), argv, "line: {line}");
                assert!(
                    !argv.iter().any(|t| t == "--model"),
                    "an empty model emits no flag at all: {argv:?}"
                );
            }
        }
    }
}

// ── #267 stage 2: gemini as a reviewer-capable orchestration CLI ───────────

/// #267 stage 2. `SUPPORTED_CLIS` is the gate every other layer keys off — the
/// workflow parser (`parse_workflow`), the spawn guardrail (`spawn_agent_ex`)
/// and the launcher roster all consult it — so "a workflow.yml can declare a
/// gemini-backed reviewer" is, first, this membership.
///
/// Stage 1 (PR #355) shipped the docs/template nudge to run one reviewer on a
/// different CLI than the worker; with only claude and copilot in this list,
/// that nudge could only ever buy a *second Claude-family opinion*. Gemini is
/// the first genuinely different model family loomux can spawn.
#[test]
fn gemini_is_a_supported_orchestration_cli() {
    assert!(
        SUPPORTED_CLIS.contains(&"gemini"),
        "#267 stage 2: gemini must be a spawnable orchestration CLI so a workflow.yml can \
         declare a gemini-backed reviewer — SUPPORTED_CLIS is {SUPPORTED_CLIS:?}"
    );
}

/// The gemini adapter builds a *gemini* invocation, not the claude fallback.
///
/// Pins the three seams a reviewer block actually needs, each of which is a
/// different CLI's spelling of something loomux already does for claude and
/// copilot:
///
/// - **`--allowed-mcp-server-names <server>`** — gemini's analogue of claude's
///   `--strict-mcp-config`: the reviewer reaches `review_verdict` and nothing
///   the user's own settings might have added. (The server itself is declared
///   in the generated settings file, not on argv — gemini has no
///   `--mcp-config`-equivalent flag; see `write_mcp_config`.)
/// - **`--approval-mode`** — the unattended posture, gemini's `yolo` against
///   claude's `--permission-mode`.
/// - **`--include-directories <group_dir>`** — the group state dir, gemini's
///   `--add-dir`.
///
/// It also asserts the two spawn forms agree, the same drift guard
/// `build_agent_argv_matches_command_line` applies to every other CLI.
#[test]
fn gemini_reviewer_command_is_a_gemini_invocation() {
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");
    let cmd = reg.build_agent_command(
        "gemini", "pro", true, cfg, None, gdir, wd, None, false, Containment::NoEdits,
        &PersonaInject::default(),
    );
    assert!(
        cmd.starts_with("gemini "),
        "a gemini block must spawn gemini, not fall through to the claude adapter: {cmd}"
    );
    for want in [
        "--model pro",
        "--approval-mode yolo",
        "--allowed-mcp-server-names orrerix",
    ] {
        assert!(cmd.contains(want), "gemini command is missing {want:?}: {cmd}");
    }
    assert!(
        cmd.contains(&format!("--include-directories \"{}\"", gdir.display())),
        "the group state dir must be in gemini's workspace: {cmd}"
    );
    // Claude's flags are claude's. A half-built command that mixes the two
    // would be worse than the fallback it replaced.
    for never in ["--mcp-config", "--permission-mode", "--disallowedTools", "--add-dir"] {
        assert!(!cmd.contains(never), "gemini command must not carry claude's {never:?}: {cmd}");
    }
    let argv = reg.build_agent_argv(
        "gemini", "pro", true, cfg, None, gdir, wd, None, false, Containment::NoEdits,
        &PersonaInject::default(),
    );
    assert_eq!(argv[0], "gemini", "argv[0] is what the pane spawns directly: {argv:?}");
    assert_eq!(shell_tokenize(&cmd), argv, "the two spawn forms must not drift: {cmd}");
}

/// A gemini block's default model is gemini's *reasoning* tier, not claude's
/// model names leaking through `default_model`'s claude branch.
///
/// `pro` for every class, deliberately, unlike claude's strong/mid split:
/// gemini's documented alias set is `pro` (complex reasoning) vs `flash`
/// (speed), and the entire point of a cross-model reviewer (#267) is a second
/// *strong* opinion — a reviewer defaulted onto a speed tier would be the
/// weakened reviewer this stage exists to avoid. A block may still pin its own
/// `model:`.
#[test]
fn gemini_blocks_default_to_geminis_reasoning_tier() {
    let g = Guardrails { agent_cli: "gemini".into(), ..Guardrails::default() };
    for role in [Role::Orchestrator, Role::Worker, Role::Reviewer, Role::Planner] {
        assert_eq!(
            g.model_for(role),
            "pro",
            "a gemini {role:?} must default to a gemini model alias, not a claude one"
        );
    }
}

/// #101's invariant, extended to gemini: the launcher's autopilot toggle and
/// the group spawn path must mean the SAME thing, built from one atom, so a
/// human's "autopilot on" and a group's `auto_ops` can't quietly diverge.
#[test]
fn gemini_launcher_and_group_paths_share_one_unattended_atom() {
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");
    let unattended = reg.build_agent_command(
        "gemini", "pro", true, cfg, None, gdir, wd, None, false, Containment::None,
        &PersonaInject::default(),
    );
    assert!(
        unattended.contains(&single_pane_autopilot_flags("gemini")),
        "the group path must carry the launcher's own gemini atom verbatim: {unattended}"
    );
    // And the attended posture is spelled out rather than left to an absent
    // flag, so a non-auto_ops group's command line says what it means.
    let attended = reg.build_agent_command(
        "gemini", "pro", false, cfg, None, gdir, wd, None, false, Containment::None,
        &PersonaInject::default(),
    );
    assert!(attended.contains("--approval-mode default"), "{attended}");
    assert!(!attended.contains("yolo"), "an attended gemini agent must not be in yolo: {attended}");
}

/// The capability table is the *reason* a CLI is or isn't in `SUPPORTED_CLIS`,
/// so the two must not drift: a CLI marked `orchestration` with no spawn
/// adapter would panic somewhere downstream, and one with an adapter but no row
/// would have no recorded containment ceiling — i.e. `cli_can_host` would wave
/// it through for every class, which is exactly the fail-open direction #462's
/// guarantee cannot survive.
#[test]
fn supported_clis_match_the_capability_table() {
    let from_table: Vec<&str> =
        CLI_CAPS.iter().filter(|c| c.orchestration).map(|c| c.cli).collect();
    assert_eq!(
        from_table,
        SUPPORTED_CLIS.to_vec(),
        "CLI_CAPS' orchestration rows and SUPPORTED_CLIS describe the same set, in the same order"
    );
    for cli in SUPPORTED_CLIS {
        assert!(cli_caps(cli).is_some(), "{cli} is spawnable but has no capability row");
    }
}

/// **The gate this whole stage turns on.** A reviewer runs under
/// `Containment::NoEdits` (#462) and a planner under `ReadOnly`; a CLI that
/// cannot enforce that tier must not be allowed to host one, because the
/// failure is silent — the agent spawns, reviews, and simply is not contained.
///
/// Codex is the case that made this concrete (#267): its only containment axis
/// is `sandbox_mode`, whose `read-only` rung also blocks running commands and
/// network access (no tests, no `gh`) and whose `workspace-write` rung denies
/// nothing at all. So it tops out at `Containment::None` — fine for a worker or
/// an orchestrator, structurally disqualified for every class orrerix denies
/// the editing tools to — a reviewer, a planner, and a MANAGER, which sits at
/// the reviewer's tier for the same purpose (#1161: it reads the codebase to
/// ground its questioning and must not write it). Three classes, and none of
/// them had to be remembered: `cli_can_host` is one comparison against an
/// ordered ladder.
#[test]
fn a_cli_may_not_host_a_class_whose_containment_it_cannot_enforce() {
    // The uncontained classes are open to every evaluated CLI.
    for role in [Role::Orchestrator, Role::Worker] {
        for cli in ["claude", "copilot", "gemini", "codex"] {
            assert!(cli_can_host(cli, role).is_ok(), "{cli} must be able to host a {role:?}");
        }
    }
    // The contained ones are not. `Role::Manager` joins them here rather than
    // by anyone noticing: it is `NoEdits`, so the ladder puts it on this side.
    for role in [Role::Reviewer, Role::Planner, Role::Manager] {
        for cli in ["claude", "copilot", "gemini"] {
            assert!(
                cli_can_host(cli, role).is_ok(),
                "{cli} denies editing tools by name — it must be able to host a {role:?}"
            );
        }
        let err = cli_can_host("codex", role)
            .expect_err("codex has no tool-level edit deny — it must not host a contained class");
        assert!(
            err.contains("codex") && err.contains("containment"),
            "the refusal must name the CLI and what it is missing, not just say 'unsupported': {err}"
        );
    }
}

/// The gate is enforced where a hand-edited `group.json` has to get past it,
/// not only where a well-formed workflow file is parsed. `parse_workflow`
/// refuses the same pairing at load time (`workflow.rs`), but a persisted
/// roster never goes back through the parser.
#[test]
fn a_workflow_file_cannot_declare_a_reviewer_on_an_uncontainable_cli() {
    let errs = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: rev-codex\n    kind: reviewer\n    cli: codex\n",
    )
    .expect_err("a codex reviewer must not parse");
    let joined = errs.join("\n");
    assert!(joined.contains("rev-codex"), "the finding must name the offending block: {joined}");
    // Naming the block is not enough to prove the *containment* gate fired —
    // `codex` is also outside `SUPPORTED_CLIS`, so a generic "unknown cli"
    // rejection would satisfy the line above while the gate did nothing. The
    // reason has to be the reason: the parser checks containment FIRST for
    // exactly this, so a CLI loomux has evaluated is told what it is missing
    // rather than that it does not exist.
    assert!(
        joined.contains("containment"),
        "the parser must refuse this pairing for its containment gap, not merely as an \
         unknown CLI — otherwise this test passes with the gate removed: {joined}"
    );
}

/// A contained gemini agent's deny rules are not on its command line — they are
/// the two generated files. This asserts the *containment itself*, which for
/// gemini means: both layers name both editing tools, and the settings file
/// points at the policy file so the admin tier is actually loaded.
///
/// Two layers, not redundancy — each covers the other's documented failure
/// mode (see `GEMINI_EDIT_DENY_TOOLS`): supplemental admin policies are ignored
/// outright on a machine that has system-wide gemini policies installed, and
/// `tools.exclude` is documented as deprecated.
#[test]
fn a_contained_gemini_agents_deny_rules_ride_its_generated_files() {
    let policy_path = Path::new("C:/data/group/configs/rev-1-gemini-policy.toml");

    for (tier, want_git) in [(Containment::NoEdits, false), (Containment::ReadOnly, true)] {
        let settings: Value = serde_json::from_str(&gemini_settings_json(
            5123,
            "tok-abc",
            tier,
            Some(policy_path),
        ))
        .expect("the generated settings must be valid JSON");
        let toml = gemini_policy_toml(tier);

        // Layer 1: tools.exclude.
        let exclude = settings["tools"]["exclude"]
            .as_array()
            .unwrap_or_else(|| panic!("{tier:?} must carry a tools.exclude list: {settings}"))
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        for t in GEMINI_EDIT_DENY_TOOLS {
            assert!(exclude.iter().any(|e| e == t), "{tier:?} tools.exclude is missing {t}: {exclude:?}");
            // Layer 2: the admin-tier policy rule for the same tool.
            assert!(
                toml.contains(&format!("toolName = \"{t}\"")),
                "{tier:?} policy file is missing a deny rule for {t}:\n{toml}"
            );
        }
        assert!(
            toml.contains("decision = \"deny\""),
            "the policy rules must be denials, not asks:\n{toml}"
        );
        // The shell survives BOTH layers — that is what makes this NoEdits and
        // not something stricter: a reviewer runs the tests (#462).
        assert!(
            !exclude.iter().any(|e| e == "run_shell_command"),
            "{tier:?} must not deny the shell outright: {exclude:?}"
        );

        // The git-mutation denials are the ReadOnly rung only — a reviewer
        // keeps `git commit` (see CLAUDE_READONLY_DENY_GIT's doc for why).
        for prefix in GEMINI_READONLY_DENY_GIT {
            let in_exclude = exclude.iter().any(|e| e == &format!("run_shell_command({prefix})"));
            let in_toml = toml.contains(&format!("commandPrefix = \"{prefix}\""));
            assert_eq!(in_exclude, want_git, "{tier:?} tools.exclude / {prefix}: {exclude:?}");
            assert_eq!(in_toml, want_git, "{tier:?} policy / {prefix}:\n{toml}");
        }

        // And the admin tier is actually reachable: an unreferenced policy
        // file is a file gemini never reads.
        assert_eq!(
            settings["adminPolicyPaths"][0].as_str(),
            Some(policy_path.display().to_string().as_str()),
            "the settings file must point at the policy file: {settings}"
        );
    }

    // An uncontained class gets neither layer — nothing to deny, and an empty
    // `tools.exclude` would be a claim about tools loomux isn't making.
    let open: Value =
        serde_json::from_str(&gemini_settings_json(5123, "tok-abc", Containment::None, None))
            .expect("valid JSON");
    assert!(open.get("tools").is_none(), "an uncontained agent needs no exclusions: {open}");
    assert!(open.get("adminPolicyPaths").is_none(), "{open}");
}

/// Every agent CLI gets the same MCP identity, in its own spelling. Gemini's is
/// the `mcpServers` settings key with `httpUrl` + `headers` — there is no
/// `--mcp-config` equivalent flag, which is the whole reason its config travels
/// by environment variable.
#[test]
fn a_gemini_agent_reaches_the_same_loomux_mcp_server_every_other_cli_does() {
    let settings: Value =
        serde_json::from_str(&gemini_settings_json(5123, "tok-abc", Containment::NoEdits, None))
            .expect("valid JSON");
    let server = &settings["mcpServers"]["orrerix"];
    assert_eq!(
        server["httpUrl"].as_str(),
        Some("http://127.0.0.1:5123/mcp"),
        "gemini must be pointed at THIS loomux instance's port: {settings}"
    );
    assert_eq!(
        server["headers"]["X-Orrerix-Agent"].as_str(),
        Some("tok-abc"),
        "the per-agent token is what makes `review_verdict` attributable: {settings}"
    );
}

/// The settings file only reaches gemini if the pane is spawned with the
/// environment variable naming it — that env entry IS gemini's MCP delivery,
/// so its absence would be a reviewer with no `review_verdict` at all.
#[test]
fn a_gemini_panes_env_carries_its_settings_path_and_no_other_clis_does() {
    let cfg = Path::new("C:/data/group/configs/rev-1.json");
    assert_eq!(
        cli_extra_env("gemini", cfg, "tok-abc"),
        vec![(
            "GEMINI_CLI_SYSTEM_SETTINGS_PATH".to_string(),
            "C:/data/group/configs/rev-1.json".to_string()
        )]
    );
    // gemini's env names a PATH and never a token — asserted explicitly,
    // because the codex arm below put a secret into this function's output and
    // "no other CLI's env carries one" is now a property worth stating rather
    // than an obvious one.
    assert!(
        !cli_extra_env("gemini", cfg, "tok-abc").iter().any(|(_, v)| v.contains("tok-abc")),
        "gemini's settings file carries its own token; the pane env must not"
    );
    // `codex` was in this list until #2515 C1, as a member of the class
    // "delivers its MCP config on argv, so it needs no env". It has left that
    // class: its config is SELECTED on argv (`-p`) and its token rides the
    // environment. Per CLAUDE.md the specimen is relocated rather than the
    // assertion relaxed — the loop below keeps the witnesses that are still
    // inside the class, and codex gets its own assertions.
    for other in ["claude", "copilot", ""] {
        assert!(
            cli_extra_env(other, cfg, "tok-abc").is_empty(),
            "{other} names its config on argv and carries nothing in the pane environment"
        );
    }
    // pi is deliberately NOT in that loop, and was not before #2515 C1
    // either: it gets exactly one variable, and the point is that the
    // variable is not IDENTITY. Asserted rather than omitted, so "pi's env
    // carries no token" is a checked claim rather than an absence.
    let pi = cli_extra_env("pi", cfg, "tok-abc");
    assert_eq!(pi.len(), 1, "pi gets the boot-time version-check skip and nothing else: {pi:?}");
    assert!(
        !pi.iter().any(|(_, v)| v.contains("tok-abc")),
        "pi names its config on argv — its one variable must not carry the token: {pi:?}"
    );
}

/// A codex GROUP pane's token rides the environment, and the variable is the
/// one its profile names (#2515 C1).
///
/// Two halves, and the second is the one with teeth. The profile written by
/// `write_codex_profile` says `env_http_headers = { … = "ORRERIX_AGENT_TOKEN" }`
/// and holds no token; this function is what puts the value there. If the two
/// spellings drift, nothing fails at spawn — the pane boots, connects with no
/// auth header, and every tool call is refused, which reads to an agent as the
/// orchestration tools being broken.
#[test]
fn a_codex_panes_env_carries_its_token_under_the_name_its_profile_expects() {
    let cfg = Path::new("C:/Users/x/.codex/orrerix-w-3.config.toml");
    assert_eq!(
        cli_extra_env("codex", cfg, "tok-abc"),
        vec![("ORRERIX_AGENT_TOKEN".to_string(), "tok-abc".to_string())]
    );
    // The name is derived from the brand prefix rather than hard-coded twice —
    // the pairing `CODEX_TOKEN_ENV`'s doc claims.
    assert_eq!(
        cli_extra_env("codex", cfg, "tok-abc")[0].0,
        format!("{}AGENT_TOKEN", brand::ENV_PREFIX)
    );
    // The profile's own spelling of that variable, asserted against the same
    // expression — this is the drift the test exists for.
    let profile = codex_profile_toml(
        7777,
        CodexMcpAuth::EnvVar("ORRERIX_AGENT_TOKEN"),
        Path::new("C:/repo"),
        true,
        "",
        None,
        &CodexGitAccess::default(),
    );
    assert!(
        profile.contains(&format!("\"{}AGENT_TOKEN\"", brand::ENV_PREFIX)),
        "the profile must name the variable this function sets:\n{profile}"
    );
    // The path is NOT in the env: codex's config reaches it by `-p <name>` on
    // argv, so a variable naming the file would be a second, unread channel.
    assert!(
        !cli_extra_env("codex", cfg, "tok-abc").iter().any(|(_, v)| v.contains(".config.toml")),
        "codex's config is selected on argv, not named by an environment variable"
    );
    // No token, no variable. `solo_prepare`'s no-seam path mints none, and an
    // empty variable would leave the pane presenting a blank auth header —
    // which the server refuses in a way that reads like a bug rather than an
    // absence.
    assert!(cli_extra_env("codex", cfg, "").is_empty());
}

/// The [`GEMINI_EDIT_DENY_TOOLS`] pin, same shape and same limits as
/// `claude_edit_deny_tools_are_known_claude_tools` (#448): it catches a typo or
/// a stale name in loomux's own deny list — a deny rule matching no tool reads
/// exactly like containment — and does NOT catch gemini renaming a tool out
/// from under the snapshot.
#[test]
fn gemini_edit_deny_tools_are_known_gemini_tools() {
    for t in GEMINI_EDIT_DENY_TOOLS {
        assert!(
            KNOWN_GEMINI_TOOLS.contains(t),
            "{t:?} is not a known gemini tool name — a deny rule for a tool that does not exist \
             is not containment. Known: {KNOWN_GEMINI_TOOLS:?}"
        );
    }
    // The shell is deliberately absent: denying it would deny the tests.
    assert!(
        !GEMINI_EDIT_DENY_TOOLS.contains(&"run_shell_command"),
        "NoEdits leaves the shell whole (#462)"
    );
}

// ── OpenCode (#722 slice A) ────────────────────────────────────────────────
//
// Every assertion below is written against the BEHAVIOR the adapter must
// have, not its internals, so each one compiles — and fails — on the tree
// that predates the adapter. That is the red half of this slice's
// red-before-green evidence; the white-box pins on the generated config
// document (`opencode_config_json`) sit further down and can only exist once
// the function does.

/// The group whose CLI is opencode, on the built-in roster — models left
/// empty so `default_model("opencode", …)` decides, which for opencode means
/// "say nothing and inherit whatever the human configured".
fn opencode_rails() -> Guardrails {
    Guardrails {
        max_agents: 4,
        agent_cli: "opencode".into(),
        blocks: workflow::default_roster(&[
            (Role::Orchestrator, "", ""),
            (Role::Worker, "", ""),
            (Role::Reviewer, "", ""),
            (Role::Planner, "", ""),
        ]),
        auto_ops: false,
        idle_kill_minutes: 0,
        max_spawns_per_hour: 0,
        watchdog_stall_minutes: 0,
        ..Guardrails::default()
    }
}

/// **The latent mangle bug (#722).** OpenCode model ids are
/// `provider_id/model_id` — `opencode/deepseek-v4-flash-free` — and
/// `sanitize_model`/`sanitize_model_opt` dropped every `/`, so that id
/// silently became `opencodedeepseek-v4-flash-free`: a model that does not
/// exist, delivered without a word of complaint.
///
/// Widened deliberately and pinned here, exactly as #709 widened it for
/// brackets: `/` is inert to both emitted forms (it is not a glob
/// metacharacter in a POSIX shell and not an operator in PowerShell), so
/// admitting it costs nothing that the second half of this test doesn't still
/// refuse.
#[test]
fn a_model_id_may_carry_a_provider_prefix_but_never_shell_syntax() {
    let parsed = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\n    model: opencode/deepseek-v4-flash-free\n",
    )
    .expect("a provider-prefixed model id must parse");
    let worker = parsed.blocks.iter().find(|b| b.id == "worker").expect("the worker block");
    assert_eq!(
        worker.model, "opencode/deepseek-v4-flash-free",
        "the provider prefix must survive sanitizing — dropping the slash yields a model id \
         that does not exist, and the CLI is handed it without any error"
    );

    // The same widening, one layer down: `clamped()` normalizes a hand-edited
    // group.json's models through `sanitize_model` (the fallback-carrying
    // twin), and it must agree.
    let rails = Guardrails {
        agent_cli: "claude".into(),
        blocks: workflow::default_roster(&[(Role::Worker, "", "opencode/deepseek-v4-flash-free")]),
        ..rails()
    }
    .clamped();
    assert_eq!(
        rails.model_for(Role::Worker),
        "opencode/deepseek-v4-flash-free",
        "clamped() must not mangle a provider-prefixed model either"
    );

    // …and the sanitizer is still a sanitizer. Anything that could smuggle an
    // argument onto the command line the model is interpolated into stays out.
    let hostile = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: worker\n    kind: worker\n    model: \"sonnet; rm -rf / && echo\"\n",
    )
    .expect("the hostile model is sanitized, not rejected");
    let worker = hostile.blocks.iter().find(|b| b.id == "worker").expect("the worker block");
    for bad in [' ', ';', '&', '|', '$', '`', '(', ')', '[', ']', '"', '\''] {
        assert!(
            !worker.model.contains(bad),
            "{bad:?} must never survive into a model id: {:?}",
            worker.model
        );
    }
}

/// opencode joins the spawnable set, and — unlike codex — with a real
/// containment ceiling, so it may host a reviewer and a planner.
///
/// The ceiling is `ReadOnly` because opencode's permission engine denies by
/// permission KEY (`edit`), and `edit` is the key every file-modifying tool
/// asks under — the vendor's own read-only `plan` agent is built from exactly
/// that rule. See `docs/design/opencode.md` for the citations.
#[test]
fn opencode_is_a_spawnable_cli_with_a_containment_ceiling() {
    assert!(
        SUPPORTED_CLIS.contains(&"opencode"),
        "opencode must be group-spawnable: {SUPPORTED_CLIS:?}"
    );
    let caps = cli_caps("opencode").expect("opencode must have a capability row");
    assert!(caps.orchestration, "the row and SUPPORTED_CLIS must agree");
    assert!(
        !caps.mcp_argv_seam,
        "opencode's MCP config is an env-delivered document, not an argv flag — a solo launch \
         cannot set env, so it stays delivery-only (#288)"
    );
    assert_eq!(
        caps.max_containment,
        Containment::ReadOnly,
        "opencode denies the edit permission and the git-mutation shell prefixes"
    );
    for role in [Role::Orchestrator, Role::Worker, Role::Reviewer, Role::Planner] {
        assert!(
            cli_can_host("opencode", role).is_ok(),
            "opencode must be able to host a {role:?}"
        );
    }
}

/// The launch line itself. Four properties, each of which a wrong adapter
/// gets wrong in its own direction: the program, the unattended posture, the
/// resume flag, and the omit-when-empty model.
#[test]
fn opencode_launch_flags_per_posture() {
    let (reg, _d) = test_registry();
    let cfg = Path::new("C:/x/cfg.json");
    let gdir = Path::new("C:/data/group");
    let wd = Path::new("C:/repo");
    let cmd = |model: &str, auto: bool, session: Option<&str>, resume: bool, c: Containment| {
        reg.build_agent_command(
            "opencode", model, auto, cfg, None, gdir, wd, session, resume, c,
            &PersonaInject::default(),
        )
    };

    let attended = cmd("opencode/deepseek-v4-flash-free", false, None, false, Containment::None);
    assert!(
        attended.starts_with("opencode "),
        "the program must be opencode, not the claude fallback: {attended}"
    );
    assert!(
        attended.contains("--model opencode/deepseek-v4-flash-free"),
        "the provider-prefixed model must reach argv intact: {attended}"
    );
    assert!(
        !attended.contains("--auto"),
        "an attended pane must not auto-approve permission asks: {attended}"
    );

    // Unattended (`auto_ops`) — `--auto` is opencode's documented spelling;
    // `--yolo`/`--dangerously-skip-permissions` are undocumented aliases and
    // are deliberately not used.
    let unattended = cmd("opencode/deepseek-v4-flash-free", true, None, false, Containment::None);
    assert!(unattended.contains("--auto"), "{unattended}");
    assert!(
        !unattended.contains("--yolo") && !unattended.contains("skip-permissions"),
        "only the documented spelling: {unattended}"
    );

    // A planner is `ReadOnly`, hence always unattended — a human it could ask
    // is not there, and gating it would only deadlock it.
    let planner = cmd("", false, None, false, Containment::ReadOnly);
    assert!(planner.contains("--auto"), "a ReadOnly class runs unattended: {planner}");

    // An empty model means "inherit the human's configured default" — loomux
    // has no vendor-neutral alias pair for opencode and refuses to hardcode a
    // model table (#329), so the flag is omitted entirely rather than emitted
    // with an empty value.
    assert!(
        !planner.contains("--model"),
        "an empty model must omit the flag, not emit a blank one: {planner}"
    );

    // Resume: opencode's `--session <id>` continues an existing session. There
    // is no way to pre-assign one, so it appears on a resume and never
    // otherwise.
    let fresh = cmd("", false, Some("ses_03bd2d53dffeiBvu9PvuCPjxT7"), false, Containment::None);
    assert!(!fresh.contains("--session"), "a cold start pre-assigns nothing: {fresh}");
    let resumed = cmd("", false, Some("ses_03bd2d53dffeiBvu9PvuCPjxT7"), true, Containment::None);
    assert!(
        resumed.contains("--session ses_03bd2d53dffeiBvu9PvuCPjxT7"),
        "a resume must continue the recorded session: {resumed}"
    );

    // The structured form the pane actually spawns is built from the same
    // atoms; `build_agent_argv_matches_command_line` pins the whole matrix,
    // this pins the one token that decides which program runs.
    let argv = reg.build_agent_argv(
        "opencode", "", true, cfg, None, gdir, wd, None, false, Containment::None,
        &PersonaInject::default(),
    );
    assert_eq!(argv.first().map(String::as_str), Some("opencode"), "{argv:?}");
    assert!(argv.iter().any(|a| a == "--auto"), "{argv:?}");
    assert!(!argv.iter().any(|a| a == "--model"), "{argv:?}");
}

/// A workflow file may declare opencode blocks, including the contained
/// classes — the parser consults the same capability table the spawn path
/// does, so this is the load-time half of the gate.
#[test]
fn a_workflow_file_may_declare_opencode_blocks() {
    let wf = workflow::parse_workflow(
        "version: 1\nblocks:\n  - id: w-oc\n    kind: worker\n    cli: opencode\n\
         \n  - id: rev-oc\n    kind: reviewer\n    cli: opencode\n\
         \n  - id: plan-oc\n    kind: planner\n    cli: opencode\n",
    )
    .expect("opencode blocks must parse for every class it can contain");
    assert_eq!(wf.blocks.iter().filter(|b| b.cli == "opencode").count(), 3, "{wf:?}");
}

/// **`refuse` and `clamp` are not the same control** (#880 review finding 4): a
/// bound the engine rejects the whole file over has to stop the submit, while
/// one it silently pulls into range may accept and coerce. The manifest says
/// which each field is; nothing but behavior can confirm it, so this drives the
/// real `parse_workflow` with an out-of-range value and looks at what happened.
///
/// Cardinality (`max_entries` on `resources:`) is checked the same way, because
/// it is the one bound a form can violate without any single field being wrong —
/// slice C's "add a resource" affordance writing a 33rd entry.
#[test]
fn the_manifests_bounds_are_the_ones_parse_workflow_actually_enforces() {
    let block = "blocks:\n  - id: w\n    kind: worker\n";

    // refuse: the file does not load at all.
    for (what, text) in [
        ("gate.threshold", format!("version: 1\n{block}gates:\n  merge:\n    require: threshold\n    threshold: 0\n    reviewers: [w]\n")),
        // #1174. Declared over a REVIEWER block, so the refusal is attributable to the
        // bound: `gate.threshold` above names a worker as its reviewer, which the parser
        // also refuses, and a row that would fail for two reasons pins neither. The
        // `err.contains` below is the other half of making it attributable.
        ("gate.max_diff_lines", "version: 1\nblocks:\n  - id: r\n    kind: reviewer\ngates:\n  merge:\n    reviewers: [r]\n    max_diff_lines: 0\n".to_string()),
        ("merge_queue.max_batch", format!("version: 1\n{block}merge_queue:\n  max_batch: 0\n")),
        ("driver.max_review_rounds", format!("version: 1\n{block}driver:\n  max_review_rounds: 0\n")),
        ("driver.max_review_rounds-above-max", format!("version: 1\n{block}driver:\n  max_review_rounds: 4\n")),
        ("driver.max_ci_attempts", format!("version: 1\n{block}driver:\n  max_ci_attempts: 0\n")),
        ("driver.max_ci_attempts-above-max", format!("version: 1\n{block}driver:\n  max_ci_attempts: 4\n")),
        // `driver.max_rebase_attempts` has no below-range row: its floor is 0,
        // and 0 is a LEGAL value there (#1778 §5.3 - a repo may refuse the
        // driver any rebase). The floor is exercised in tests/workflow.rs.
        ("driver.max_rebase_attempts-above-max", format!("version: 1\n{block}driver:\n  max_rebase_attempts: 2\n")),
        ("resource.slots", format!("version: 1\n{block}resources:\n  build:\n    slots: 0\n")),
        ("resource.slots-above-max", format!("version: 1\n{block}resources:\n  build:\n    slots: 65\n")),
        ("resource.max_hold_minutes", format!("version: 1\n{block}resources:\n  build:\n    max_hold_minutes: 0\n")),
        ("resource.max_hold_minutes-above-max", format!("version: 1\n{block}resources:\n  build:\n    max_hold_minutes: 481\n")),
        // #1457 N10. Declared with an explicit `cli: claude`, and for the same
        // reason the `gate.max_diff_lines` row above declares a reviewer: a
        // remote block with no `cli:` is refused for THAT, and both refusals
        // mention "remote", so a row that could fail for two reasons pins
        // neither. With the cli spelled out the only rule left to break is the
        // length, which is what this row is about — 65 characters against a
        // cap of 64.
        ("block.remote-above-max", format!(
            "version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: claude\n    remote: {}\n",
            "b".repeat(65)
        )),
    ] {
        let err = workflow::parse_workflow(&text)
            .err()
            .unwrap_or_else(|| panic!("{what}: the manifest says on_out_of_range=refuse, but the engine accepted an out-of-range value"));
        assert!(
            !err.is_empty(),
            "{what}: a refusal must say something — a generated control quotes this back to the human"
        );
        // …and it must say which FIELD, or the human is told their file is bad and
        // left to find out where. (`what` carries a `-above-max` suffix on the rows
        // that drive the ceiling rather than the floor; the field name is the part
        // before it, and before the section prefix.)
        let field = what.split('.').nth(1).unwrap().split('-').next().unwrap();
        assert!(
            err.iter().any(|e| e.contains(field)),
            "{what}: the refusal must name the field it is about, and {err:?} does not mention {field:?}"
        );
    }

    // clamp: the file loads, and the value is quietly pulled into range. Both
    // ends, because a control that refuses one and coerces the other is wrong
    // half the time.
    for (given, want) in [(1_u32, 5_u32), (9999, 240)] {
        let wf = workflow::parse_workflow(&format!(
            "version: 1\n{block}merge_queue:\n  enabled: true\n  checks_timeout_minutes: {given}\n"
        ))
        .unwrap_or_else(|e| {
            panic!("merge_queue.checks_timeout_minutes: the manifest says on_out_of_range=clamp, so {given} must LOAD: {e:?}")
        });
        assert_eq!(
            wf.merge_queue.checks_timeout_minutes, want,
            "merge_queue.checks_timeout_minutes: {given} must clamp to {want}"
        );
    }

    // Two of the driver's three backstops ride the same notify-TTL clamp
    // family (#1778 §5.3), so the same both-ends check over each of them;
    // `drive_timeout_minutes` carries its own range since #2110 and is checked
    // against the ceiling the manifest publishes for it.
    for field in ["lane_timeout_minutes", "fix_timeout_minutes", "drive_timeout_minutes"] {
        let ceiling = if field == "drive_timeout_minutes" { 1440 } else { 240 };
        for (given, want) in [(1_u32, 5_u32), (9999, ceiling)] {
            let wf = workflow::parse_workflow(&format!(
                "version: 1\n{block}driver:\n  enabled: true\n  {field}: {given}\n"
            ))
            .unwrap_or_else(|e| {
                panic!("driver.{field}: the manifest says on_out_of_range=clamp, so {given} must LOAD: {e:?}")
            });
            let got = match field {
                "lane_timeout_minutes" => wf.driver.lane_timeout_minutes,
                "fix_timeout_minutes" => wf.driver.fix_timeout_minutes,
                _ => wf.driver.drive_timeout_minutes,
            };
            assert_eq!(got, want, "driver.{field}: {given} must clamp to {want}");
        }
    }

    // cardinality: RESOURCES_MAX + 1 entries is a load error, so the manifest's
    // max_entries is a real ceiling and not a suggestion.
    let mut many = format!("version: 1\n{block}resources:\n");
    for i in 0..=32 {
        many.push_str(&format!("  r{i}:\n    slots: 1\n"));
    }
    assert!(
        workflow::parse_workflow(&many).is_err(),
        "workflow.resources: the manifest declares max_entries, so one entry past it must refuse the file"
    );
    // …and exactly at the cap it still loads: a ceiling that refused its own
    // limit would make the manifest's number wrong by one in the other direction.
    let mut at_cap = format!("version: 1\n{block}resources:\n");
    for i in 0..32 {
        at_cap.push_str(&format!("  r{i}:\n    slots: 1\n"));
    }
    assert!(
        workflow::parse_workflow(&at_cap).is_ok(),
        "workflow.resources: max_entries entries must still load"
    );
}

/// **The delivery seam.** opencode names no config file on argv: its MCP
/// server, its containment and its persona all ride environment variables set
/// on the pane loomux spawns. So this goes through the real spawn site and
/// reads what that site built — a command line alone could look perfect while
/// the agent booted with no loomux MCP server and no denials at all.
#[test]
fn an_opencode_spawn_delivers_its_config_and_containment_by_env() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/opencode-repo", opencode_rails()).unwrap();
    let spawn = |role: Role, name: &str| {
        let a = reg.spawn_agent(&g.id, role, name, "t", false, None).unwrap();
        (a.id.clone(), reg.spawn_request_for_test(&a.id).expect("no spawn request"))
    };

    let (_wid, worker) = spawn(Role::Worker, "w");
    let env: HashMap<&str, &str> =
        worker.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    // The config document — the only channel opencode's MCP server has.
    let cfg: Value = serde_json::from_str(
        env.get("OPENCODE_CONFIG_CONTENT").expect("the generated config must ride the pane env"),
    )
    .expect("OPENCODE_CONFIG_CONTENT must be valid JSON");
    assert_eq!(cfg["mcp"]["orrerix"]["type"].as_str(), Some("remote"), "{cfg}");
    assert!(
        cfg["mcp"]["orrerix"]["url"].as_str().unwrap_or_default().contains("/mcp"),
        "the loomux MCP endpoint: {cfg}"
    );
    assert!(
        cfg["mcp"]["orrerix"]["headers"]["X-Orrerix-Agent"].is_string(),
        "the per-agent token is what makes every MCP call attributable: {cfg}"
    );
    assert_eq!(
        cfg["mcp"]["orrerix"]["oauth"], json!(false),
        "OAuth auto-detection is on by default and loomux authenticates by header — a 401 \
         during discovery must not start a flow this server never speaks: {cfg}"
    );
    assert_eq!(
        cfg["share"].as_str(), Some("disabled"),
        "a group agent's session is never published: {cfg}"
    );

    // Autoupdate is suppressed by ENV, not by the config key: a mid-boot
    // self-update restarts the CLI and flushes the kickoff (the copilot
    // `--no-auto-update` hazard), and an env var cannot be overridden by the
    // org/managed/MDM config ranks that land after the inline document.
    assert_eq!(env.get("OPENCODE_DISABLE_AUTOUPDATE"), Some(&"1"), "{env:?}");

    // The per-group database: absolute, under this group's own state dir, so
    // a group's sessions are separable from the human's own and from every
    // other group's.
    let db = env.get("OPENCODE_DB").expect("the per-group db path must be set");
    assert!(Path::new(db).is_absolute(), "OPENCODE_DB must be absolute: {db}");
    assert!(
        db.replace('\\', "/").contains(g.id.as_str()),
        "the database must live under THIS group's dir: {db}"
    );

    // A worker is uncontained — nothing to deny, and the repo's own project
    // config is left loading.
    assert!(
        env.get("OPENCODE_DISABLE_PROJECT_CONFIG").is_none(),
        "a worker keeps the repo's own opencode config: {env:?}"
    );

    // **The two copies of the posture must BE the same posture.** The document
    // and the override are produced by two calls that each derive from
    // `(containment, unattended)`; today one call site passes the same locals
    // to both, but nothing in the signatures forces that, and a future arm
    // passing a stale pair would give a pane a document saying one thing and
    // an override — the layer that wins — saying another. Assert the equality
    // rather than rely on call-site discipline.
    let wperm: Value =
        serde_json::from_str(env.get("OPENCODE_PERMISSION").expect("worker permission")).unwrap();
    assert_eq!(
        cfg["permission"], wperm,
        "the document's posture and the OPENCODE_PERMISSION override must be one object"
    );

    // The reviewer is the class the #462 guarantee lives or dies on.
    let (_rid, rev) = spawn(Role::Reviewer, "rev");
    let renv: HashMap<&str, &str> = rev.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let rperm: Value = serde_json::from_str(
        renv.get("OPENCODE_PERMISSION")
            .expect("containment must ALSO ride OPENCODE_PERMISSION, which no config rank can \
                     override and which survives an --agent resolution failure"),
    )
    .expect("OPENCODE_PERMISSION must be valid JSON");
    assert_eq!(
        rperm["edit"], json!("deny"),
        "a reviewer may not edit: `edit` is the permission every file-modifying tool asks \
         under, and denying it with the `*` pattern removes them from the model's tools \
         entirely: {rperm}"
    );
    // The group is not `auto_ops`, so a reviewer is ATTENDED — its shell asks
    // rather than being pre-approved. What matters for #462 is that it is
    // reachable at all (a reviewer runs the tests), and that the flows it works
    // through are not the ones being interrupted.
    assert!(
        rperm["bash"].is_object() && rperm["bash"]["*"] == json!("ask"),
        "a reviewer's shell is intact — it runs the tests (#462): {rperm}"
    );
    assert_eq!(rperm["bash"]["git *"], json!("allow"), "{rperm}");
    assert_eq!(
        rperm["bash"]["git push*"], Value::Null,
        "a reviewer keeps git: only the ReadOnly tier loses commit/push (#462): {rperm}"
    );
    assert_eq!(
        renv.get("OPENCODE_DISABLE_PROJECT_CONFIG"), Some(&"1"),
        "a contained pane does not load the repo's own opencode config at all — a repo cannot \
         then contribute permission rules to the merge: {renv:?}"
    );
    // Same equality as the worker's, on the class the #462 guarantee rests on:
    // a contained pane is exactly where a stale override would matter most.
    let rcfg: Value =
        serde_json::from_str(renv.get("OPENCODE_CONFIG_CONTENT").expect("reviewer doc")).unwrap();
    assert_eq!(rcfg["permission"], rperm, "document and override must be one object");

    // …and a planner additionally loses the git-mutation subcommands.
    let (_pid, planner) = spawn(Role::Planner, "p");
    let penv: HashMap<&str, &str> =
        planner.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let pperm: Value =
        serde_json::from_str(penv.get("OPENCODE_PERMISSION").expect("planner permission")).unwrap();
    assert_eq!(pperm["bash"]["git commit*"], json!("deny"), "{pperm}");
    assert_eq!(pperm["bash"]["git push*"], json!("deny"), "{pperm}");
    // A planner is ReadOnly, hence always unattended even in this non-auto_ops
    // group — so the shell is open apart from the two denials above, which is
    // what leaves `gh` reachable for the plan comment.
    assert_eq!(pperm["bash"]["*"], json!("allow"), "{pperm}");

    // The persona/contract reaches opencode as a native agent, selected on
    // argv and defined in the config document, with the handle namespaced so a
    // repo's own `.opencode/agents/*.md` cannot deep-merge into it.
    let pcfg: Value =
        serde_json::from_str(penv.get("OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
    let agents = pcfg["agent"].as_object().expect("the block's agent entry");
    let (handle, entry) = agents.iter().next().expect("exactly one generated agent");
    assert!(handle.starts_with("loomux-"), "generated handles are namespaced: {handle}");
    assert!(
        planner.command.contains(&format!("--agent {handle}")),
        "the pane must actually select it — an unresolvable --agent falls back to `build`, the \
         most permissive agent there is: {}",
        planner.command
    );
    assert_eq!(
        entry["mode"].as_str(), Some("primary"),
        "a subagent-mode entry is refused by --agent and silently falls back to `build`: {entry}"
    );
    let prompt = entry["prompt"].as_str().expect("the contract rides a file reference");
    assert!(
        prompt.starts_with("{file:") && prompt.ends_with('}'),
        "the role contract travels by FILE, never on argv: {prompt}"
    );
    let path = prompt.trim_start_matches("{file:").trim_end_matches('}');
    assert!(
        !path.contains('\\'),
        "file references are substituted textually BEFORE the JSON is parsed, so a Windows \
         backslash path arrives with its separators doubled — emit forward slashes: {path}"
    );
    assert!(
        fs::read_to_string(path).is_ok_and(|t| !t.trim().is_empty()),
        "a missing file reference is fatal to config load — it must be written before the \
         pane spawns: {path}"
    );
}

/// The body of a `"bash": { … }` object as it is EMITTED, one per occurrence.
/// A bash object's values are all scalars, so the next `}` closes it.
///
/// The needle carries the space `to_string_pretty` emits, so a switch to the
/// compact serializer would make this return nothing — and a caller that only
/// loops over the result would then assert nothing at all while staying green.
/// Every caller must therefore assert the COUNT before looping; see the one
/// below.
fn emitted_bash_objects(doc: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = doc;
    while let Some(i) = rest.find("\"bash\": {") {
        let body = &rest[i + "\"bash\": {".len()..];
        let end = body.find('}').expect("an unterminated bash object");
        out.push(body[..end].to_string());
        rest = &body[end..];
    }
    out
}

/// **Rule order is the containment.** opencode evaluates permission rules by
/// taking the LAST one that matches — matching by wildcard on the permission
/// KEY as well as on the pattern — and it emits them in the order the config
/// document spells them. Two ways that turns a denial into nothing, both
/// pinned here:
///
/// - a `"*"` KEY (not pattern) sitting after the specific denials matches
///   every permission and silently re-allows them;
/// - a `"git *": "allow"` sitting after `"git commit*": "deny"` re-allows the
///   commit.
#[test]
fn an_opencode_denial_is_never_emitted_before_something_that_re_allows_it() {
    for unattended in [false, true] {
        let doc = loomux_lib::orchestration::opencode_config_json(
            5123,
            "tok-abc",
            Containment::ReadOnly,
            unattended,
            Some(("loomux-g-plan", Path::new("C:/g/configs/loomux-g-plan.md"))),
        );
        let cfg: Value = serde_json::from_str(&doc).expect("the generated document must be JSON");

        for (what, perm) in
            [("global", &cfg["permission"]), ("agent", &cfg["agent"]["loomux-g-plan"]["permission"])]
        {
            let keys: Vec<&String> =
                perm.as_object().expect("a permission object").keys().collect();
            assert!(
                !keys.iter().any(|k| k.as_str() == "*"),
                "the {what} permission block must never carry a `*` KEY — it matches every \
                 permission, and one emitted after the denials re-allows them: {keys:?}"
            );
        }

        let objects = emitted_bash_objects(&doc);
        // Without this the loop below can go VACUOUS and stay green: the
        // helper's needle carries the space `to_string_pretty` emits, so a
        // switch to the compact serializer would return an empty vec and
        // silently retire the whole rule-order pin. A ReadOnly pane with an
        // agent entry emits exactly two — the global posture and the
        // agent-level denials.
        assert_eq!(
            objects.len(),
            2,
            "expected the global and agent-level bash objects; a count of 0 means the helper's \
             needle stopped matching, not that the document stopped carrying them: {doc}"
        );
        for body in objects {
            let deny = body
                .find("\"deny\"")
                .unwrap_or_else(|| panic!("a ReadOnly pane denies git mutation: {body}"));
            for allow in body.match_indices("\"allow\"") {
                assert!(
                    allow.0 < deny,
                    "every allow must be emitted BEFORE the first deny — last match wins, so an \
                     allow after a deny silently reopens it: {body}"
                );
            }
        }
    }
}

/// The two postures, and the one property that separates them: an attended
/// pane must ASK, because opencode's own default is to allow everything —
/// louder than any other CLI loomux spawns, where the default is restrictive
/// and loomux widens it.
#[test]
fn an_attended_opencode_pane_narrows_the_clis_allow_everything_default() {
    use loomux_lib::orchestration::opencode_permission_json;

    let attended = opencode_permission_json(Containment::None, false);
    assert_eq!(attended["edit"], json!("ask"), "{attended}");
    assert_eq!(attended["bash"]["*"], json!("ask"), "{attended}");
    for pat in ["git *", "gh *"] {
        assert_eq!(
            attended["bash"][pat], json!("allow"),
            "the branch→commit→PR flow is pre-approved so a human isn't stopped at every \
             step: {attended}"
        );
    }

    let unattended = opencode_permission_json(Containment::None, true);
    assert_eq!(unattended["edit"], json!("allow"), "{unattended}");
    assert_eq!(unattended["bash"]["*"], json!("allow"), "{unattended}");

    // Both postures reach outside the worktree: the pane's own role
    // instructions live under the group's state dir, and this key defaults to
    // `ask`, so without it an unattended pane would stall on its own contract.
    for posture in [&attended, &unattended] {
        assert_eq!(posture["external_directory"], json!("allow"), "{posture}");
    }

    // `question` follows the HUMAN, not containment. opencode's defaults deny
    // it and only its built-in `build` agent re-allows it, so a loomux pane —
    // which always runs a config-declared agent — would otherwise answer an
    // attended worker's "should I do X?" with a permission error instead of a
    // question. Unattended it must stay denied: nobody is there to answer, and
    // a question with no answer is a stall.
    assert_eq!(attended["question"], json!("allow"), "{attended}");
    assert_eq!(unattended["question"], json!("deny"), "{unattended}");

    // Containment overrides the posture in the one direction that matters, and
    // never the other way: a contained pane denies regardless of `unattended`.
    for unattended in [false, true] {
        assert_eq!(
            opencode_permission_json(Containment::NoEdits, unattended)["edit"],
            json!("deny")
        );
        assert_eq!(
            opencode_permission_json(Containment::ReadOnly, unattended)["bash"]["git push*"],
            json!("deny")
        );
        // A reviewer keeps git: only the ReadOnly tier loses commit/push.
        assert_eq!(
            opencode_permission_json(Containment::NoEdits, unattended)["bash"]["git push*"],
            Value::Null
        );
    }
}

/// The env mapping, asserted without a spawn — including the one entry that is
/// conditional, and the one that must never be relative.
#[test]
fn opencode_pane_env_carries_the_document_the_override_and_the_group_database() {
    use loomux_lib::orchestration::opencode_pane_env;

    let db = Path::new("C:/state/groups/g-1/opencode/opencode.db");
    let env: HashMap<String, String> =
        opencode_pane_env("{\"share\":\"disabled\"}", Containment::None, false, db)
            .into_iter()
            .collect();
    assert_eq!(env["OPENCODE_CONFIG_CONTENT"], "{\"share\":\"disabled\"}");
    assert_eq!(env["OPENCODE_DISABLE_AUTOUPDATE"], "1");
    assert_eq!(
        env["OPENCODE_DB"], "C:/state/groups/g-1/opencode/opencode.db",
        "forward slashes, and absolute — a relative value resolves under the CLI's own data \
         root, i.e. NOT under the group dir"
    );
    // The permission override is the layer that survives an --agent resolution
    // failure, so it ships on every pane, contained or not.
    assert!(env.contains_key("OPENCODE_PERMISSION"), "{env:?}");
    assert!(
        !env.contains_key("OPENCODE_DISABLE_PROJECT_CONFIG"),
        "an uncontained pane keeps the repo's own opencode config: {env:?}"
    );

    for c in [Containment::NoEdits, Containment::ReadOnly] {
        let env: HashMap<String, String> = opencode_pane_env("{}", c, true, db).into_iter().collect();
        assert_eq!(
            env["OPENCODE_DISABLE_PROJECT_CONFIG"], "1",
            "a contained pane does not load the repo's config at all, so the repo cannot \
             contribute a rule to the permission merge: {env:?}"
        );
    }
}

/// The #101 invariant on the fourth CLI: the launcher toggle and the group
/// spawn path must mean the same thing, built from one atom rather than two
/// independently-typed literals.
#[test]
fn opencode_launcher_and_group_paths_share_one_unattended_atom() {
    use loomux_lib::orchestration::OPENCODE_UNATTENDED_FLAGS;
    let (reg, _d) = test_registry();
    let group = reg.build_agent_command(
        "opencode", "m", true, Path::new("C:/x/cfg.json"), None, Path::new("C:/data/group"),
        Path::new("C:/repo"), None, false, Containment::None, &PersonaInject::default(),
    );
    assert!(group.contains(OPENCODE_UNATTENDED_FLAGS), "{group}");
    assert_eq!(single_pane_autopilot_flags("opencode"), OPENCODE_UNATTENDED_FLAGS);
}

/// The mirror of `gemini_blocks_default_to_geminis_reasoning_tier`, and it
/// asserts the opposite thing on purpose: an opencode block has NO default
/// model, so the pane inherits whatever the human configured.
///
/// Empty is a decision, not a hole. opencode ids are `provider_id/model_id`
/// against a catalog of dozens of providers with no vendor-neutral alias, so
/// any default loomux picked would be a hardcoded model table (#329) that also
/// silently overrode a human who had already chosen. What a wrong
/// implementation does instead is fall through to `default_model`'s claude
/// branch and hand opencode `sonnet`/`opus` — a model its providers do not
/// serve — which is exactly what this pins against.
#[test]
fn opencode_blocks_default_to_no_model_at_all() {
    let g = Guardrails { agent_cli: "opencode".into(), ..Guardrails::default() };
    for role in [Role::Orchestrator, Role::Worker, Role::Reviewer, Role::Planner] {
        assert_eq!(
            g.model_for(role),
            "",
            "an opencode {role:?} must inherit the human's own model, not a claude tier"
        );
    }
    // …and a block that DOES pin one keeps it, provider prefix and all.
    let pinned = Guardrails {
        agent_cli: "opencode".into(),
        blocks: workflow::default_roster(&[(
            Role::Worker,
            "opencode",
            "opencode/deepseek-v4-flash-free",
        )]),
        ..Guardrails::default()
    }
    .clamped();
    assert_eq!(pinned.model_for(Role::Worker), "opencode/deepseek-v4-flash-free");
}

/// The [`OPENCODE_EDIT_DENY_PERMISSION`] pin — the opencode sibling of
/// `claude_edit_deny_tools_are_known_claude_tools`, and the same limits: it
/// catches loomux naming a key opencode does not evaluate (a rule matching
/// nothing reads exactly like containment), not opencode renaming the key out
/// from under an unrefreshed snapshot.
///
/// It is a single key rather than a list because the engine denies by the
/// permission a tool REQUESTS, and `edit`, `write` and `apply_patch` all
/// request `edit` — the same one key the CLI's own read-only `plan` agent is
/// built from. See `docs/design/opencode.md` for the source citations.
#[test]
fn opencode_denies_the_permission_key_every_editing_tool_asks_under() {
    use loomux_lib::orchestration::{OPENCODE_EDIT_DENY_PERMISSION, OPENCODE_READONLY_DENY_GIT};
    assert_eq!(OPENCODE_EDIT_DENY_PERMISSION, "edit");
    // The shell is deliberately untouched by the edit denial: denying it would
    // deny the tests a reviewer exists to run (#462).
    assert_ne!(OPENCODE_EDIT_DENY_PERMISSION, "bash");
    // No space before the `*`: the matcher is zero-or-more against the whole
    // command, so `git commit*` covers a bare `git commit` and `git commit *`
    // would not.
    for pat in OPENCODE_READONLY_DENY_GIT {
        assert!(pat.ends_with('*') && !pat.ends_with(" *"), "{pat:?}");
    }
}

#[test]
fn shell_tokenize_handles_quotes_and_at_marker() {
    assert_eq!(
        shell_tokenize(r#"claude --add-dir "C:/a b" --allowedTools "Bash(git *)""#),
        vec!["claude", "--add-dir", "C:/a b", "--allowedTools", "Bash(git *)"]
    );
    assert_eq!(
        shell_tokenize(r#"copilot --additional-mcp-config "@C:/x/cfg.json""#),
        vec!["copilot", "--additional-mcp-config", "@C:/x/cfg.json"]
    );
}

#[test]
fn copilot_mcp_config_includes_tools_allowlist() {
    let (reg, _d) = test_registry();
    let mut rails = rails();
    rails.agent_cli = "copilot".into();
    let g = reg.create_group("C:/tmp/copilot-repo", rails).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let cfg = fs::read_to_string(
        reg.state_root().join(g.id.as_str()).join("configs").join(format!("{}.json", w.id)),
    )
    .unwrap();
    assert!(cfg.contains("\"tools\""), "copilot expects a tools allowlist in the server entry");
    assert!(cfg.contains(&w.token));
    // #417: copilot has no PreCompact-equivalent hook (confirmed absent
    // upstream) — it stays on the pre-existing inference tier, never faked.
    assert!(!cfg.contains("\"hooks\""), "copilot must not get a hooks block it can't use: {cfg}");
}

#[test]
fn claude_agent_spawn_provisions_the_compact_lifecycle_hooks() {
    // #417: PreCompact + SessionStart(compact) hook config rides in its own
    // generated file (`write_hook_settings_file`), a SEPARATE file from
    // `--mcp-config`'s (rev-4 review N2 — see `write_mcp_config`'s doc for
    // why sharing one file between the two was a schema-drift risk), passed
    // via `--settings` — never a hand-merge of the user's own
    // `.claude/settings.json`. Skipped (not failed) when no `sh` interpreter
    // is resolvable at all, mirroring the #335 shim tests' "skip, don't
    // fail" precedent for an environment where `sh.exe` genuinely can't be
    // found — `resolve_hook_sh`'s own fail-open policy.
    #[cfg(windows)]
    if locate_sh_exe().is_none() {
        eprintln!("SKIP claude_agent_spawn_provisions_the_compact_lifecycle_hooks: no sh.exe found via `where`");
        return;
    }
    let (reg, d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    let mcp_cfg_path = reg.state_root().join(g.id.as_str()).join("configs").join(format!("{}.json", w.id));
    let mcp_cfg: Value = serde_json::from_str(&fs::read_to_string(&mcp_cfg_path).unwrap()).unwrap();
    assert!(mcp_cfg.get("hooks").is_none(), "the mcp-config file must carry no hooks key at all: {mcp_cfg}");
    assert!(mcp_cfg.get("mcpServers").is_some());

    let hooks_path = reg.state_root().join(g.id.as_str()).join("configs").join(format!("{}-hooks.json", w.id));
    let hooks_cfg: Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
    let precompact = hooks_cfg["hooks"]["PreCompact"][0]["hooks"][0]["command"].as_str().unwrap().to_string();
    let sessionstart = hooks_cfg["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap().to_string();
    assert!(precompact.contains("precompact"), "{precompact}");
    assert!(sessionstart.contains("sessionstart-compact"), "{sessionstart}");
    assert_eq!(hooks_cfg["hooks"]["SessionStart"][0]["matcher"], json!("compact"), "only compact-sourced starts are hooked");
    // The command embeds this agent's group dir + id as literal argv — the
    // generic script itself carries none of it (constraint #8).
    assert!(precompact.contains(w.id.as_str()), "{precompact}");
    assert!(precompact.contains(g.id.as_str()), "{precompact}");

    // #112: the real prompt-landed signal rides the SAME `--settings` file,
    // one more entry alongside PreCompact/SessionStart.
    let promptsubmit_entry = &hooks_cfg["hooks"]["UserPromptSubmit"][0];
    let promptsubmit = promptsubmit_entry["hooks"][0]["command"].as_str().unwrap().to_string();
    assert!(promptsubmit.contains("promptsubmit"), "{promptsubmit}");
    assert!(promptsubmit.contains(w.id.as_str()) && promptsubmit.contains(g.id.as_str()), "{promptsubmit}");
    // Per the hooks reference, `UserPromptSubmit` "does NOT support matchers
    // and always fires on every prompt submission" — no `matcher` key at all,
    // unlike `SessionStart` above (which needs one to scope to compact-only
    // starts).
    assert!(promptsubmit_entry.get("matcher").is_none(), "UserPromptSubmit takes no matcher: {promptsubmit_entry}");
    // The other two events must be completely untouched by this addition.
    assert!(precompact.contains("precompact") && !precompact.contains("promptsubmit"));
    assert!(sessionstart.contains("sessionstart-compact") && !sessionstart.contains("promptsubmit"));

    // The generic script itself was written to the test's isolated
    // `compact_hook_dir_override` (`test_registry()`'s doc: real deployments
    // use `root.parent()/compacthook`, a machine-wide shared path this test
    // deliberately avoids so it can't read a stale script some OTHER test
    // left behind at that shared location — see #112 PR discussion; the
    // pre-#112 version of this assertion read from the WRONG, non-overridden
    // path and only ever passed by accident of whatever another test had
    // most recently written there).
    let script = d.path().join("compacthook").join("compact-hook.sh");
    let script_text = fs::read_to_string(&script).expect("the hook script must be written");
    assert!(script_text.contains("precompact") && script_text.contains("sessionstart-compact") && script_text.contains("promptsubmit"));
    assert!(!script_text.contains(g.id.as_str()), "the script itself must stay generic — group specifics ride on argv only");
}

pub(crate) fn copilot_rails() -> Guardrails {
    let mut r = rails();
    r.agent_cli = "copilot".into();
    r
}

#[test]
fn copilot_agents_spawn_without_a_preassigned_session() {
    // Copilot has no `--session-id`; a fresh copilot pane starts untracked
    // and is associated later once its session-state appears on disk.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/copilot-repo", copilot_rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    assert!(w.session_id.is_none(), "copilot cannot pre-assign a session id");
}

#[test]
fn associating_a_copilot_session_records_it_on_roster_and_task_board() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/copilot-repo", copilot_rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "builder", "do the thing", false, None).unwrap();
    // The orchestrator has put a task on the board assigned to this worker.
    let t = reg.upsert_task(&g.id, "orch-1", None, patch(Some("build feature"), None, None)).unwrap();
    reg.upsert_task(
        &g.id,
        "orch-1",
        Some(&t.id),
        TaskPatch { assignee: Some(w.id.clone()), ..Default::default() },
    )
    .unwrap();

    // The watcher discovered copilot's session id and binds it to the pane.
    let sid = "0f9e8d7c-1234-4abc-8def-0011223344ff";
    reg.associate_session(&g.id, &w.id, sid);

    // Agent map now carries the id (so list_agents/resume can use it).
    let agents = reg.list_agents(&g.id);
    let entry = agents.as_array().unwrap().iter().find(|a| a["id"] == w.id.as_str()).unwrap();
    assert_eq!(entry["session"], sid);

    // Durable roster records exactly one session row for this pane — the
    // placeholder was upgraded, not duplicated — and the session browser
    // surfaces it as a worker chip in this group.
    let roles: Vec<_> = reg.session_roles().into_iter().filter(|r| r.session_id == sid).collect();
    assert_eq!(roles.len(), 1, "one roster/session-browser entry per pane, got {}", roles.len());
    assert_eq!(roles[0].role, "worker");
    assert_eq!(roles[0].group_id, g.id);

    // Task board mirrors the session so the orchestrator can resume the task.
    let task = reg.tasks(&g.id).into_iter().find(|x| x.id == t.id).unwrap();
    assert_eq!(task.session.as_deref(), Some(sid));

    // Idempotent: a second (late) discovery must not clobber the bound id.
    reg.associate_session(&g.id, &w.id, "ffffffff-0000-4000-8000-000000000000");
    let agents = reg.list_agents(&g.id);
    let entry = agents.as_array().unwrap().iter().find(|a| a["id"] == w.id.as_str()).unwrap();
    assert_eq!(entry["session"], sid, "an already-tracked pane keeps its first session id");
}

#[test]
fn associating_a_session_emits_one_learned_event() {
    // #1563: copilot and opencode bind their session AFTER boot, so the pane
    // was never told about an id `agents.json` had recorded all along —
    // `Pane.capture()` wrote `sessionId: null` into tabs.json and the
    // dormant-group card then offered no resume for a live session. The event
    // this pins is the missing consumer of that fact.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/copilot-repo", copilot_rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "builder", "t", false, None).unwrap();
    // Absence assertion, and its positive control is the `len() == 1` below:
    // nothing has been learned yet, and the mechanism demonstrably does fire.
    assert!(
        reg.session_learned_events_for_test().is_empty(),
        "a spawn alone learns nothing — copilot cannot pre-assign an id"
    );

    let sid = "0f9e8d7c-1234-4abc-8def-0011223344ff";
    assert!(reg.associate_session(&g.id, &w.id, sid), "the binding must take");

    let events = reg.session_learned_events_for_test();
    assert_eq!(events.len(), 1, "exactly one event per binding, got {events:?}");
    assert_eq!(events[0]["group_id"], g.id.as_str(), "names the group");
    assert_eq!(events[0]["agent_id"], w.id.as_str(), "names the pane's agent");
    assert_eq!(events[0]["session_id"], sid, "carries the id that was learned");

    // A LATE second discovery for a pane that already carries an id is a no-op
    // in the roster, and must be one on the wire too: the frontend's own
    // `adoptSessionId` refuses a second id, but an event that never arrives is
    // the stronger guarantee and the one this site owes (plan §7).
    assert!(!reg.associate_session(&g.id, &w.id, "ffffffff-0000-4000-8000-000000000000"));
    assert_eq!(
        reg.session_learned_events_for_test().len(),
        1,
        "an already-bound pane emits no second event"
    );

    // The other refusal — a session another pane in the group already claims —
    // returns from a branch ABOVE `persist_agent_record`, so it exercises a
    // different placement of the emit than the one above does.
    let b = reg.spawn_agent(&g.id, Role::Worker, "second", "t", false, None).unwrap();
    assert!(!reg.associate_session(&g.id, &b.id, sid), "the claim is excluded");
    assert_eq!(
        reg.session_learned_events_for_test().len(),
        1,
        "a refused claim emits nothing"
    );
}

#[test]
fn copilot_orchestration_session_gets_a_chip_and_restores() {
    use loomux_lib::orchestration::resume_recorded_session;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap(); // must exist for restore
    let repo_path = repo.path().to_string_lossy().into_owned();
    let sid = "0a1b2c3d-4e5f-4a6b-8c7d-8e9f00112233";
    let gid;
    {
        let reg = relaunch_registry(dir.path());
        reg.set_port(45999);
        let g = reg.create_group(&repo_path, copilot_rails()).unwrap();
        gid = g.id.clone();
        // A copilot orchestrator spawns untracked, then its session is bound
        // once it appears on disk (here, driven directly).
        let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
        assert!(orch.session_id.is_none());
        reg.associate_session(&g.id, &orch.id, sid);
        // Session browser now has an ORCH chip for this copilot session.
        let roles: Vec<_> = reg.session_roles().into_iter().filter(|r| r.session_id == sid).collect();
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].role, "orchestrator");
    }
    // "App restart": a new registry restores the whole copilot orchestration
    // from the recorded session, resuming its conversation via `copilot
    // --resume`. The orchestrator branch now pre-checks the session actually
    // resolves in copilot's OWN store before opening a pane for it (#412
    // review B1) — fixture it there, thread-local so no other concurrently
    // running test's copilot lookups are affected.
    let store = scratch_dir("copilot-orch-restore");
    fixture_copilot_session(&store, sid, &repo_path);
    loomux_lib::sessions::set_copilot_session_state_root_for_test(Some(store.clone()));
    let reg = Arc::new(relaunch_registry(dir.path()));
    reg.set_port(45999);
    let req = resume_recorded_session(&reg, sid, None, false).unwrap().expect("orchestrator pane spec");
    loomux_lib::sessions::set_copilot_session_state_root_for_test(None);
    let _ = fs::remove_dir_all(&store);
    assert_eq!(req.group_id, gid);
    assert!(req.command.starts_with("copilot "), "must relaunch copilot, got: {}", req.command);
    assert!(req.command.contains(&format!("--resume={sid}")), "must resume the recorded session");
    assert!(!req.command.contains("--resume "), "#458/#781: never the space form on copilot");
}

/// #1563 slice C1: the **opencode** half of the orchestrator restore, mirroring
/// `copilot_orchestration_session_gets_a_chip_and_restores` one store down.
///
/// **A regression pin, not a behaviour change — GREEN on the base commit.** The
/// backend resume path has been CLI-aware since #722 and this half already
/// works; what #1563 found broken is everything ABOVE it (an opencode
/// orchestrator's learned id never reaches `tabs.json`, and the session browser
/// reads the human's global store by design, never a group's), and that is
/// slices A and B. This pins the half those two must not silently break while
/// they land, and it is the half nothing covered: `copilot_orchestration_…`
/// pinned copilot, and no test drove an opencode orchestrator through
/// `resume_recorded_session` at all.
///
/// Three things it holds down that a command-line-only assertion would miss:
///
/// - the store the resume pre-check consults is **this group's**
///   (`opencode_db_path`), not the human's global one — the #722 bug that made
///   an opencode group unreopenable was reading somewhere else entirely;
/// - the relaunch is `--session`, opencode's *continue* flag, and neither
///   claude's `--session-id` nor copilot's `--resume`;
/// - the MCP wiring rides the pane **environment**, because opencode names no
///   config file on argv (`CliCaps::mcp_argv_seam` is false for it). A resumed
///   orchestrator that came back without it would be a pane with its
///   conversation intact and no loomux MCP server at all — resumed and mute.
#[test]
fn opencode_orchestration_restores_from_recorded_session() {
    use loomux_lib::orchestration::resume_recorded_session;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap(); // must exist for restore
    let repo_path = repo.path().to_string_lossy().into_owned();
    // opencode's own id shape: `ses_` + 12 hex + 14 base62 (a real id from the
    // #722 slice-V memo). A claude-shaped UUID would not exercise the widening
    // `sanitize_session` needed for opencode at all.
    let sid = "ses_1508a391dffext5Xb0UUF2UDjk";
    let gid;
    let db;
    {
        let reg = relaunch_registry(dir.path());
        let g = reg.create_group(&repo_path, opencode_rails()).unwrap();
        gid = g.id.clone();
        db = reg.opencode_db_path(&g.id);
        // No flag pre-assigns an opencode session id, so the orchestrator pane
        // spawns untracked and the watcher binds its id once the `session` row
        // appears. Driven directly here — constraint 3, no opencode is run.
        let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
        assert!(orch.session_id.is_none(), "opencode cannot pre-assign a session id");
        reg.associate_session(&g.id, &orch.id, sid);
        let roles: Vec<_> =
            reg.session_roles().into_iter().filter(|r| r.session_id == sid).collect();
        assert_eq!(roles.len(), 1, "one roster row for the orchestrator pane");
        assert_eq!(roles[0].role, "orchestrator");
        assert_eq!(roles[0].group_id, gid);
    }
    // The store the resume pre-check reads is THIS GROUP's — `OPENCODE_DB`
    // points every pane in the group at it (#722) — so that is where the
    // orchestrator's session has to be for the resume to find it.
    fixture_opencode_store(&db, &[(sid, repo_path.as_str())]);

    // "App restart": a new registry restores the whole orchestration from the
    // recorded session.
    let reg = Arc::new(relaunch_registry(dir.path()));
    let req =
        resume_recorded_session(&reg, sid, None, false).unwrap().expect("orchestrator pane spec");
    assert_eq!(req.group_id, gid);
    assert_eq!(req.role, Role::Orchestrator);
    assert_eq!(
        Path::new(&req.cwd),
        repo.path(),
        "an orchestrator's launch cwd is the group's repo, never a worktree"
    );
    assert!(req.command.starts_with("opencode"), "must relaunch opencode, got: {}", req.command);
    assert!(
        req.command.contains(&format!("--session {sid}")),
        "must continue the recorded session, got: {}",
        req.command
    );
    assert!(
        !req.command.contains("--session-id"),
        "`--session-id` is claude's spelling and opencode has no such flag: {}",
        req.command
    );
    assert!(
        !req.command.contains("--resume"),
        "`--resume` is copilot's/claude's spelling: {}",
        req.command
    );

    // The MCP wiring, which for opencode is NOT on the command line.
    let env: HashMap<&str, &str> = req.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let cfg: Value = serde_json::from_str(
        env.get("OPENCODE_CONFIG_CONTENT")
            .expect("the generated config must ride the RESUMED pane's env, not only a fresh one"),
    )
    .expect("OPENCODE_CONFIG_CONTENT must be valid JSON");
    assert_eq!(
        cfg["mcp"][brand::MCP_SERVER]["type"].as_str(),
        Some("remote"),
        "the loomux MCP server is the only channel a resumed orchestrator has: {cfg}"
    );
    assert!(
        cfg["mcp"][brand::MCP_SERVER]["headers"]["X-Orrerix-Agent"].is_string(),
        "the per-agent token is what makes every MCP call attributable: {cfg}"
    );
    assert_eq!(
        env.get("OPENCODE_DB").map(|p| p.replace('\\', "/")),
        Some(db.display().to_string().replace('\\', "/")),
        "the resumed pane must be pointed back at the SAME store its session lives in, or its \
         next turn writes a session nothing can find again: {env:?}"
    );
}

/// #1563 slice C2: an opencode store left mid-flight by a **killed** process —
/// the crash the human was actually recovering from — must still resolve its
/// orchestrator's session.
///
/// **The fixture is the artifacts, not a flag.** A hard kill leaves three
/// files: the main database holding whatever was last checkpointed, a `-wal`
/// holding everything committed since, and a `-shm` wal-index nobody got to
/// remove. They are built here by copying a live store's three files out from
/// under an OPEN connection and then letting that connection close the
/// *original* — so the copy has a dirty WAL and no living handle anywhere,
/// which is what `TerminateProcess` leaves and what a clean quit never does.
/// (One honest limit: the `-shm` copied this way is coherent, where a real
/// kill can leave a torn one. That is the easier case, and it is the one whose
/// success is claimed below — a torn index forces SQLite's recovery path, which
/// no test can produce deterministically.)
///
/// **The finding this test records rather than fixes.**
/// `opencodedb::open_readonly` falls back to an `immutable=1` open when the
/// plain read-only open fails, and an immutable connection reads the main
/// database file ALONE — it never consults the WAL. The last two assertions
/// prove that is not theoretical on this very store: the fallback returns
/// `Ok(None)` for a session plainly in it, with a positive control (the
/// checkpointed row it CAN see) so the miss cannot be mistaken for a store it
/// simply failed to open. Nothing in the two live arms engages that fallback,
/// because a read-only open succeeds against both artifact sets — so the
/// blindness is real and not reachable *from these fixtures*, which is why it
/// is recorded rather than papered over by rigging the fixture around it. The
/// fix, if a reachable case is ever found, is a `-shm`/`-wal` presence check
/// before falling back; see `docs/design/opencode.md`, Session identification.
#[test]
fn an_opencode_store_left_by_a_killed_process_still_resolves_the_session() {
    use loomux_lib::opencodedb;
    use loomux_lib::orchestration::session_cwd_in_store;
    let scratch = scratch_dir("opencode-killed-store");
    let live = scratch.join("live").join("opencode.db");
    let killed = scratch.join("killed").join("opencode.db");
    let swept = scratch.join("swept").join("opencode.db");
    // Written before the crash and checkpointed into the main file.
    let checkpointed = "ses_03bd2d53dffeiBvu9PvuCPjxT7";
    // Committed after it and never checkpointed — the orchestrator's own.
    let in_wal = "ses_1508a391dffext5Xb0UUF2UDjk";

    {
        let conn = open_opencode_store(&live);
        conn.execute_batch(OPENCODE_SESSION_DDL).unwrap();
        insert_opencode_session(&conn, checkpointed, "C:/Projects/loomux");
        // Everything so far lands in the main database file...
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get::<_, i64>(0)).unwrap();
        // ...and everything after it stays in the -wal, which is the state a
        // kill freezes. (Autocheckpoint would not have fired on one row at its
        // 1000-page default either; off is the claim, not the coincidence.)
        conn.execute_batch("PRAGMA wal_autocheckpoint=0").unwrap();
        insert_opencode_session(&conn, in_wal, "C:/Projects/loomux-worktrees/test/x");
        for suffix in ["", "-wal", "-shm"] {
            let from = sqlite_sidecar(&live, suffix);
            if from.is_file() {
                copy_sqlite_file(&from, &sqlite_sidecar(&killed, suffix));
            }
        }
        // The same store as a reboot or a sweeper leaves it: the -shm is
        // regenerable and routinely goes missing, the -wal is not.
        for suffix in ["", "-wal"] {
            copy_sqlite_file(&sqlite_sidecar(&live, suffix), &sqlite_sidecar(&swept, suffix));
        }
    } // the LIVE store closes cleanly here — the two copies never did

    // The fixture really did leave a dirty WAL behind. Without this, every
    // assertion below would pass just as well against a store that quietly
    // checkpointed, i.e. against no crash at all.
    let wal = sqlite_sidecar(&killed, "-wal");
    assert!(wal.is_file(), "a killed store keeps its -wal: {}", wal.display());
    assert!(
        wal.metadata().unwrap().len() > 0,
        "an empty -wal holds no uncheckpointed row and would make this test vacuous"
    );
    assert!(
        sqlite_sidecar(&killed, "-shm").is_file(),
        "and the -shm index the dead process never got to remove"
    );

    // ARM 1 — the whole kill artifact set, which is what a resume actually
    // meets. `open_readonly` is deliberately NOT immutable precisely so this
    // works: a read-only connection consults the WAL and sees the last
    // committed state.
    assert_eq!(
        session_cwd_in_store("opencode", in_wal, Some(&killed), None).unwrap().as_deref(),
        Some("C:/Projects/loomux-worktrees/test/x"),
        "a session committed but never checkpointed must still resolve, or every orchestrator \
         whose app was killed comes back unresumable"
    );

    // ARM 2 — the same store after the -shm was swept. SQLite recreates the
    // wal-index from the -wal given write access to the directory, so the row
    // survives this too: the read-only flag is about the DATABASE, not the
    // sidecars.
    assert_eq!(
        session_cwd_in_store("opencode", in_wal, Some(&swept), None).unwrap().as_deref(),
        Some("C:/Projects/loomux-worktrees/test/x"),
        "a missing -shm is regenerated, not a lost session"
    );

    // ARM 3 — what the immutable fallback would cost if it ever engaged here.
    let conn = opencodedb::open_immutable(&killed).expect("an immutable open of a real store");
    assert_eq!(
        opencodedb::session_directory_on(&conn, checkpointed).unwrap().as_deref(),
        Some("C:/Projects/loomux"),
        "positive control: this connection DID open the store and can read what was \
         checkpointed — the miss below is not a failed open"
    );
    assert_eq!(
        opencodedb::session_directory_on(&conn, in_wal).unwrap(),
        None,
        "and it silently loses everything the dead process committed but never checkpointed. \
         `open_readonly`'s immutable fallback is blind to exactly the rows a crash leaves in \
         the -wal — recorded here (#1563 C2), not fixed, because nothing on these fixtures \
         reaches it: see docs/design/opencode.md, Session identification"
    );
    drop(conn);
    let _ = fs::remove_dir_all(&scratch);
}

#[test]
fn copilot_group_resumes_a_recorded_session() {
    // Resume parity: a copilot group accepts resume_session (its ids are
    // hex+dashes, so they pass sanitization) and reuses it on the pane.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/copilot-repo", copilot_rails()).unwrap();
    let dir = tempfile::tempdir().unwrap(); // an existing cwd for the resume
    let sid = "aabbccdd-1122-4334-8556-77889900aabb".to_string();
    let w = reg
        .spawn_agent_ex(
            &g.id,
            Role::Worker,
            None,
            "resumed",
            "follow-up",
            false,
            None,
            None,
            Some(sid.clone()),
            Some(dir.path().to_string_lossy().into_owned()),
            None,
        )
        .unwrap();
    assert_eq!(w.session_id.as_deref(), Some(sid.as_str()));
    // A mangled id is rejected rather than silently resuming the wrong one.
    assert!(reg
        .spawn_agent_ex(
            &g.id, Role::Worker, None, "bad", "", false, None, None,
            Some("../../etc/passwd".into()),
            Some(dir.path().to_string_lossy().into_owned()),
            None,
        )
        .is_err());
}

/// #479 (restore UX): resuming a session whose group is already known — every
/// dormant-group Resume click names it via `hint` — used to pay for
/// `session_roles()`'s full scan regardless: every OTHER group's group.json,
/// tasks.json, and full audit log read and parsed just to find one row. This
/// pins the group-hint fast path (`session_role_in_group`) that makes a
/// correct hint touch only the named group, by surrounding the resumed
/// session with many unrelated decoy groups each carrying a realistic audit
/// tail, and asserting the resume reads ONE group's records, not one per
/// group on disk.
///
/// WHAT IS PINNED IS A COUNT, NOT A DURATION (#514). Two earlier versions of
/// this test asserted on wall-clock and both went flaky on CI:
///
///  1. A fixed 200ms bound — red at 211ms on a slower runner, against an
///     otherwise-genuinely-fixed fast path.
///  2. A same-run ratio against a bare `session_roles()` baseline (`elapsed *
///     2 < baseline`) — red three times in one day on windows-latest
///     (175ms/311ms, 188ms/272ms, and a third of the same shape). The hint
///     path was faster than the baseline on every one of those runs; it just
///     missed the 2x margin under runner load, because a loaded runner does
///     not slow both sides by the same factor when one side is I/O-bound
///     across 200 directories and the other is not.
///
/// Both were measuring a proxy. What #479 actually changed is how many groups
/// get their roster + full audit log read to resolve one session — 201 before,
/// 1 after — so that is what this pins, via `group_record_scans_for_test()`.
/// The count is identical on any hardware under any load, and it is red for
/// exactly the regression that matters: a hint path that scans every group
/// again. Same structural-pin convention as #493/#511's `ScanStats` tests.
///
/// The durations are still measured and printed (`--nocapture`) because they
/// are useful to a human investigating this path — ~420-540ms before the fix
/// vs ~40-55ms after, debug build, one dev machine — but nothing asserts on
/// them. Do not re-add a timing assertion here in any form.
#[test]
fn resume_recorded_session_group_hint_avoids_scanning_every_other_group() {
    use loomux_lib::orchestration::{group_record_scans_for_test, resume_recorded_session};
    use std::sync::Arc;
    use std::time::Instant;

    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);

    const DECOY_GROUPS: usize = 200;
    const NOISE_LINES: usize = 300;
    for i in 0..DECOY_GROUPS {
        let g = reg.create_group(&format!("C:/tmp/decoy-{i}"), rails()).unwrap();
        let mut audit = String::new();
        for n in 0..NOISE_LINES {
            audit.push_str(&format!(
                "{{\"ts_ms\":{n},\"actor\":\"loomux\",\"action\":\"noise\",\"detail\":{{\"n\":{n}}}}}\n"
            ));
        }
        fs::write(reg.state_root().join(g.id.as_str()).join("audit.jsonl"), audit).unwrap();
    }

    // The target session, in its own group — fixtured into claude's store
    // (thread-local test seam) exactly like the other orchestrator-resume
    // tests, so the existence pre-check passes and the resume actually
    // proceeds instead of failing on an unrelated lookup.
    let repo = tempfile::tempdir().unwrap(); // must exist for restore
    let repo_path = repo.path().to_string_lossy().into_owned();
    let g = reg.create_group(&repo_path, rails()).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let sid = orch.session_id.unwrap();
    reg.mark_dead(&orch.id, Some(0));

    // Control: the full scan this fast path replaces, still reachable
    // directly (it's the unchanged fallback `resume_recorded_session` itself
    // uses on a hint miss). Counted, not timed. This is also the fixture's
    // own proof — it asserts the decoys are really on disk and really would
    // be read, without which a hinted count of 1 below would pass just as
    // happily against an empty state root.
    let before = group_record_scans_for_test();
    let baseline_start = Instant::now();
    let _ = reg.session_roles();
    let baseline = baseline_start.elapsed();
    let full_scan_groups = group_record_scans_for_test() - before;
    assert_eq!(
        full_scan_groups,
        DECOY_GROUPS + 1,
        "fixture check: a full session_roles() scan must read every decoy group's \
         records plus the target's — if this is not {} the decoys aren't where \
         this test thinks they are, and the pin below proves nothing",
        DECOY_GROUPS + 1
    );

    let store = scratch_dir("resume-hint-fastpath");
    fixture_claude_session(&store, &sid, &repo_path);
    loomux_lib::sessions::set_claude_projects_root_for_test(Some(store.clone()));
    let before = group_record_scans_for_test();
    let started = Instant::now();
    let req = resume_recorded_session(&reg, &sid, Some((g.id.clone(), "orchestrator".into())), false);
    let elapsed = started.elapsed();
    let hinted_groups = group_record_scans_for_test() - before;
    loomux_lib::sessions::set_claude_projects_root_for_test(None);
    let _ = fs::remove_dir_all(&store);

    let req = req.unwrap().expect("must relaunch the recorded orchestrator session");
    assert_eq!(req.group_id, g.id);
    // Printed for a human reading `--nocapture`; asserted on by nothing. The
    // assertion below is the whole pin.
    eprintln!(
        "resume_recorded_session with a correct group hint read {hinted_groups} group(s) \
         in {elapsed:?}; a full session_roles() scan read {full_scan_groups} in {baseline:?} \
         (across {DECOY_GROUPS} decoy groups x {NOISE_LINES} audit lines each)"
    );

    // THE PIN. A correct hint resolves the session inside the ONE named group
    // (`session_role_in_group`) and reads nothing else: the orchestrator
    // branch relaunches from `load_group_file` and returns before the
    // worker/reviewer path's own `merged_records` roster pull, so this
    // measures 1 today.
    //
    // The ceiling is a deliberately loose absolute constant rather than that
    // exact figure. What must stay true is that the count does not grow with
    // how many groups exist — not that it is exactly 1: a delegate rejoin
    // legitimately reads the same single group twice, and a refactor adding
    // another single-group read shouldn't have to touch this line. A
    // regression that drops the fast path reads all 201 and misses this by two
    // orders of magnitude, which is the failure this test exists to catch.
    const HINTED_GROUP_CEILING: usize = 4;
    assert!(
        hinted_groups <= HINTED_GROUP_CEILING,
        "resuming via a correct group hint must read a fixed handful of groups' \
         records, not one per group on disk — read {hinted_groups} group(s) with \
         {DECOY_GROUPS} decoys present (ceiling {HINTED_GROUP_CEILING}; a full scan \
         reads {full_scan_groups})"
    );
}

#[test]
fn concurrent_groups_on_one_repo_stay_separate_but_resume_when_free() {
    let (reg, _d) = test_registry();
    // First orchestration on the repo.
    let g1 = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g1.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    reg.set_state(&g1.id, r#"{"queue":[1]}"#).unwrap();
    // Second concurrent orchestration on the SAME repo must get its own
    // group (otherwise its orchestrator would receive g1's worker reports)
    // and must not inherit g1's state.
    let g2 = reg.create_group("C:/tmp/repo", rails()).unwrap();
    assert_ne!(g1.id, g2.id);
    assert_eq!(reg.get_state(&g2.id), "{}");
    // Once g1 has no live agents, a new launch resumes g1's id and state.
    for a in reg.list_agents(&g1.id).as_array().unwrap() {
        reg.mark_dead(a["id"].as_str().unwrap(), Some(0));
    }
    let g3 = reg.create_group("C:/tmp/repo", rails()).unwrap();
    assert_eq!(g3.id, g1.id, "freed base group id must be reused for resume");
    assert_eq!(reg.get_state(&g3.id), r#"{"queue":[1]}"#);
}

#[test]
fn kickoff_prompt_references_instructions_and_task() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "fix-auth", "Fix issue #7", false, None).unwrap();
    let k = reg.kickoff_prompt(&w, &g, "note", None);
    assert!(k.contains("worker.md"));
    assert!(k.contains("Fix issue #7"));
    let idle = reg.spawn_agent(&g.id, Role::Worker, "idle", "", false, None).unwrap();
    let k = reg.kickoff_prompt(&idle, &g, "", None);
    assert!(k.contains("No task is assigned yet"));
}

#[test]
fn planner_kickoff_references_planner_instructions() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let p = reg
        .spawn_agent(&g.id, Role::Planner, "plan-47", "Plan issue #47", false, None)
        .unwrap();
    let k = reg.kickoff_prompt(&p, &g, "note", None);
    assert!(k.contains("planner.md"), "planner kickoff must reference its instructions file");
    assert!(k.contains("a planner agent"), "kickoff must name the planner role");
    assert!(k.contains("Plan issue #47"));
}

#[test]
fn planner_explores_read_only_and_never_gets_a_worktree() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    // Even with worktree requested, a planner runs in the repo itself — it
    // never branches, worktrees, commits, or PRs (#47).
    let p = reg
        .spawn_agent(&g.id, Role::Planner, "plan", "Plan it", true, Some("plan/x".into()))
        .unwrap();
    assert_eq!(p.cwd, "C:/tmp/repo", "a planner must not get a dedicated worktree");
    assert!(
        reg.list_agents(&g.id).as_array().unwrap().iter().any(|a| a["role"] == "planner"),
        "planner must appear in the roster with its role"
    );
    // The planner's spawn-time read-only note (PLANNER_READONLY_NOTE) is threaded
    // verbatim into its kickoff, communicating the no-code/branches/PRs contract.
    let k = reg.kickoff_prompt(&p, &g, PLANNER_READONLY_NOTE, None);
    assert!(
        k.contains("never create branches, worktrees, commits, or PRs"),
        "planner kickoff must carry the read-only containment note, got: {k}"
    );
    assert!(
        PLANNER_READONLY_NOTE.contains("read-only") && PLANNER_READONLY_NOTE.contains("issue comment"),
        "the containment note must state the read-only contract and the issue-comment deliverable"
    );
}

#[test]
fn planner_counts_toward_the_live_agent_cap() {
    let (reg, _d) = test_registry();
    // Cap is 2 (rails). A worker + a planner fill it; the next delegate is refused.
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w1", "t", false, None).unwrap();
    reg.spawn_agent(&g.id, Role::Planner, "p1", "plan", false, None).unwrap();
    let err = reg
        .spawn_agent(&g.id, Role::Reviewer, "rev1", "t", false, None)
        .unwrap_err();
    assert!(err.contains("guardrail"), "a planner must count against the delegate cap: {err}");
}

#[test]
fn per_block_cli_is_pinned_at_spawn_and_persisted() {
    let (reg, _d) = test_registry();
    // Group default is copilot, but the reviewer BLOCK overrides to claude
    // (#4's per-role CLI, now a block field — #222).
    let rails = Guardrails {
        agent_cli: "copilot".into(),
        blocks: workflow::default_roster(&[
            (Role::Orchestrator, "", ""),
            (Role::Worker, "", ""),
            (Role::Reviewer, "claude", ""),
            (Role::Planner, "", ""),
        ]),
        max_agents: 4,
        ..rails()
    };
    let g = reg.create_group("C:/tmp/mixed-repo", rails).unwrap();
    // Observable per-block effect: claude agents get a pre-assigned session id;
    // copilot agents mint their own later, so start without one.
    let worker = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let reviewer = reg.spawn_agent(&g.id, Role::Reviewer, "rev", "t", false, None).unwrap();
    assert!(worker.session_id.is_none(), "worker inherits the copilot group default (no pre-assigned session)");
    assert!(reviewer.session_id.is_some(), "the reviewer block's claude CLI pre-assigns a session id");
    // The roster is persisted to group.json as the block array.
    let gj = fs::read_to_string(reg.state_root().join(g.id.as_str()).join("group.json")).unwrap();
    let v: Value = serde_json::from_str(&gj).unwrap();
    assert_eq!(v["guardrails"]["agent_cli"], "copilot");
    let blocks = v["guardrails"]["blocks"].as_array().expect("blocks array persisted");
    let rev = blocks.iter().find(|b| b["id"] == "reviewer").expect("reviewer block persisted");
    assert_eq!(rev["cli"], "claude");
    assert_eq!(rev["kind"], "reviewer");
    assert!(blocks.iter().any(|b| b["id"] == "planner"), "every block is persisted, not just the overridden one");
}

#[test]
fn unknown_block_cli_is_rejected_at_spawn() {
    let (reg, _d) = test_registry();
    // A hand-edited group.json could pin an unsupported CLI to a block; the
    // spawn must reject it rather than silently downgrade (#4).
    let rails = Guardrails {
        blocks: workflow::default_roster(&[
            (Role::Orchestrator, "", ""),
            (Role::Worker, "aider", ""),
            (Role::Reviewer, "", ""),
            (Role::Planner, "", ""),
        ]),
        max_agents: 4,
        ..rails()
    };
    let g = reg.create_group("C:/tmp/bad-cli-repo", rails).unwrap();
    let err = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap_err();
    assert!(err.contains("unsupported agent CLI"), "unknown block CLI must be rejected: {err}");
    // Blocks that inherit the (valid) group default still spawn fine.
    reg.spawn_agent(&g.id, Role::Reviewer, "rev", "t", false, None).unwrap();
}

#[test]
fn instruction_files_rendered_with_group_facts() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/myrepo", rails()).unwrap();
    let dir = reg.state_root().join(g.id.as_str());
    let orch = fs::read_to_string(dir.join("orchestrator.md")).unwrap();
    assert!(orch.contains("C:/tmp/myrepo"));
    assert!(orch.contains("at most 2 live delegates"), "guardrails must be rendered into the doc");
    // The rename_agent tool and its delegation guidance are rendered (#95r).
    assert!(orch.contains("rename_agent(agent_id, name)"),
        "orchestrator instructions must document rename_agent");
    assert!(orch.contains("Name the pane for its work"),
        "orchestrator instructions must guide renaming a worker to its task");
    // The unconfirmed-delivery recovery guidance is rendered (#103): read the
    // pane back, re-send once, and flag the human on a repeat. #1683 moved
    // that procedure into the rendered playbook; the pin follows it.
    let pb = fs::read_to_string(dir.join("orchestrator-playbook.md")).unwrap();
    assert!(pb.contains("delivery to <id> unconfirmed"),
        "orchestrator instructions must explain the unconfirmed-delivery notice");
    assert!(pb.contains("flag the human"),
        "unconfirmed-delivery guidance must escalate a repeat to the human");
    assert!(!pb.contains("{{"), "no unrendered placeholders in the playbook");
    let worker = fs::read_to_string(dir.join("worker.md")).unwrap();
    assert!(worker.contains("Never merge"), "merge gatekeeping must be in worker instructions");
    // The planner instructions are rendered alongside the other roles (#47).
    let planner = fs::read_to_string(dir.join("planner.md")).unwrap();
    assert!(planner.contains("planner"), "planner instructions must be written to the group dir");
    assert!(
        planner.contains("never write code") || planner.contains("You never write code"),
        "planner instructions must forbid writing code"
    );
    assert!(!planner.contains("{{"), "no unrendered placeholders in planner.md");
}

/// #1187: `spawn_agent_ex`'s per-spawn instruction-file refresh ("Refresh the
/// block's instruction file so a `mode: replace` swap is reflected in what the
/// kickoff points at") used to render with its OWN short var list — `REPO,
/// GROUP_ID, MAX_AGENTS, *_MODEL, HOLD_LABEL, LESSONS_PATH` — while the
/// group-level render (`write_instruction_files`, above) renders with the full
/// one, adding `WORKFLOW, ADVISOR_CONSULT_NOTE, POST_MERGE_WORKFLOW_HOOK,
/// MERGE_QUEUE, LOCKS, LOCKS_ORCH`. `render_template` leaves an unlisted
/// `{{KEY}}` LITERAL rather than substituting empty, so every spawn silently
/// regressed a freshly-rendered file back to raw placeholders — live evidence
/// on a real group's `worker-deep.md` was `{{ADVISOR_CONSULT_NOTE}}` and
/// `{{LOCKS}}` sitting verbatim in a file a spawned worker was handed to read.
///
/// The fix is `OrchRegistry::instruction_vars` — one builder both paths render
/// from — so this both proves the regression is gone and guards against a
/// third, independently-drifting list ever being added back.
///
/// The roster also declares its OWN orchestrator block (#1187 review round 1
/// N4), and the test spawns it too: `spawn_agent_ex(role: Orchestrator, block:
/// None)`. This shape is test-only today, and deliberately so — the MCP
/// `spawn_agent` tool refuses `kind: "orchestrator"` (`mcp.rs`, the `if kind
/// == Some(Role::Orchestrator)` guard in `call_tool`), `spawn_agent_bound`
/// itself refuses a *named* orchestrator block (`mod.rs`, the `if
/// block.kind == Role::Orchestrator && named.is_some()` guard in
/// `spawn_agent_bound`), and `resume_recorded_session` short-circuits an
/// orchestrator record (`if record.role == "orchestrator"`) before it ever
/// reaches `spawn_agent_ex` — so no production caller takes this path (see
/// `spawn_agent_bound`'s own comment on the test-only registration use, just
/// above its orchestrator-block-name refusal). Anchored by symbol rather than
/// line number, since a symbol survives a rebase that moves the file and a
/// bare line number does not.
/// The test exercises `instruction_vars`/`pairs()` directly through the one
/// caller that *can* reach it, so `{{WORKFLOW}}` — the placeholder this
/// issue's own body names as the worst case, since it carries the entire
/// declared-roster section — has a witness at the spawn site the shared
/// builder now serves, in case a production path ever does reach it.
///
/// Derived, not measured: these two assertions were added on top of the fix
/// (commit `deb0873d`+), so they were never run red against the pre-fix short
/// var list. The pre-fix list demonstrably lacked `WORKFLOW` entirely (it is
/// not among `REPO, GROUP_ID, MAX_AGENTS, *_MODEL, HOLD_LABEL,
/// LESSONS_PATH`), so a re-render would have left `{{WORKFLOW}}` literal and
/// both assertions would have failed the same way the worker-block
/// `ADVISOR_CONSULT_NOTE` assertion earlier in this test actually did fail —
/// but that failure was never actually observed for these two.
#[test]
fn spawn_ex_rerender_carries_the_full_var_list_no_placeholder_survives() {
    let (reg, _d) = test_registry();
    let repo = scratch_dir("spawn-ex-varlist");
    fs::create_dir_all(repo.join(".loomux")).unwrap();
    // A roster carrying every conditional fragment `orchestrator.md` and
    // `worker.md` can hold: an explicit `orchestrator` block (`{{WORKFLOW}}`,
    // `{{LOCKS_ORCH}}`), `role_hint: advisor` (`{{ADVISOR_CONSULT_NOTE}}`) and
    // a declared `resources:` block (`{{LOCKS}}`) — the same two placeholders
    // the live evidence in #1187 named, plus the orchestrator-only one its
    // body calls out by name. `advanced_orchestrator` must be on for
    // `load_workflow` to gate LOCKS/LOCKS_ORCH in (`locks_declared` in
    // `instruction_vars`).
    fs::write(
        repo.join(".loomux").join("workflow.yml"),
        format!(
            "version: {}\n\
             blocks:\n\
             \x20 - id: orchestrator\n    kind: orchestrator\n\
             \x20 - id: w\n    kind: worker\n\
             \x20 - id: adv\n    kind: planner\n    role_hint: advisor\n\
             resources:\n  build:\n    slots: 1\n    max_hold_minutes: 45\n",
            workflow::SCHEMA_VERSION
        ),
    )
    .unwrap();
    // The fixture asserts its own validity, per this file's convention: a
    // workflow file that never parsed would otherwise fail the assertions
    // below as "the feature is broken" rather than "the fixture is broken".
    let loaded = workflow::load_workflow(repo.to_str().unwrap());
    assert!(
        matches!(&loaded, Ok(Some(wf)) if !wf.resources.is_empty()),
        "fixture must parse with a non-empty resources block, got {loaded:?}"
    );

    let g = reg
        .create_group(
            repo.to_str().unwrap(),
            Guardrails { advanced_orchestrator: true, max_agents: 4, ..rails() },
        )
        .unwrap();
    let dir = reg.state_root().join(g.id.as_str());

    // Positive control: the group-level render (`create_group` calls
    // `write_instruction_files`) must have actually substituted both
    // fragments into `w.md` — otherwise this fixture isn't exercising the
    // vars this test cares about, and a clean re-render below would prove
    // nothing.
    let before = fs::read_to_string(dir.join("w.md")).unwrap();
    assert!(
        before.contains("consult the advisor (`adv`)"),
        "fixture setup: ADVISOR_CONSULT_NOTE must be substituted by the group-level render \
         before the spawn-time re-render is even exercised: {before}"
    );
    assert!(
        before.contains("acquire_lock(name, note?, wait_minutes?)"),
        "fixture setup: LOCKS must be substituted by the group-level render before the \
         spawn-time re-render is even exercised: {before}"
    );
    assert!(!before.contains("{{"), "fixture setup: group-level render must leave no placeholder: {before}");

    // The regression site: `spawn_agent_ex` re-renders `w.md` on every spawn.
    let a = reg
        .spawn_agent_ex(&g.id, Role::Worker, Some("w".into()), "w", "task", false, None, None, None, None, None)
        .unwrap();
    assert_eq!(a.block, "w");

    let after = fs::read_to_string(dir.join("w.md")).unwrap();
    assert!(
        after.contains("consult the advisor (`adv`)"),
        "spawn_agent_ex's re-render must carry ADVISOR_CONSULT_NOTE through, not regress it \
         to a literal placeholder: {after}"
    );
    assert!(
        after.contains("acquire_lock(name, note?, wait_minutes?)"),
        "spawn_agent_ex's re-render must carry LOCKS through, not regress it to a literal \
         placeholder: {after}"
    );
    assert!(
        !after.contains("{{"),
        "spawn_agent_ex's re-render must leave no unrendered {{{{PLACEHOLDER}}}} in the file \
         a spawned agent is told to read (#1187): {after}"
    );

    // #1187 review round 1 N4: the orchestrator-block case named in the issue
    // and the body as the worst one — `{{WORKFLOW}}` carries the entire
    // declared-roster section, not a one-line note. Same shape as the worker
    // case above: positive control on the group-level render, then the
    // spawn-time re-render must carry it through.
    let orch_before = fs::read_to_string(dir.join("orchestrator.md")).unwrap();
    assert!(
        orch_before.contains("Spawn by block, not by kind."),
        "fixture setup: WORKFLOW must be substituted by the group-level render before the \
         spawn-time re-render is even exercised: {orch_before}"
    );
    assert!(!orch_before.contains("{{"), "fixture setup: group-level render must leave no placeholder: {orch_before}");

    // `block: None` — no production caller reaches this shape today (see the
    // doc comment above this test); it is exactly the "register the group's
    // own orchestrator in tests" use `spawn_agent_bound`'s own comment names
    // (just above its `named.is_some()` orchestrator-block refusal),
    // exercised directly to reach the shared builder.
    let orch_agent = reg
        .spawn_agent_ex(&g.id, Role::Orchestrator, None, "orch", "task", false, None, None, None, None, None)
        .unwrap();
    assert_eq!(orch_agent.block, "orchestrator");

    let orch_after = fs::read_to_string(dir.join("orchestrator.md")).unwrap();
    assert!(
        orch_after.contains("Spawn by block, not by kind."),
        "spawn_agent_ex's re-render must carry WORKFLOW through for an orchestrator block, not \
         regress it to a literal placeholder (#1187 N4): {orch_after}"
    );
    assert!(
        !orch_after.contains("{{"),
        "spawn_agent_ex's re-render of an orchestrator block must leave no unrendered \
         {{{{PLACEHOLDER}}}} (#1187 N4): {orch_after}"
    );
}
