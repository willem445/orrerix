//! Question evidence from the composed grid and loomux's own notice rows.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- #534 / #513(c): question evidence from the COMPOSED GRID ----------
//
// The limitation this closes, as the design note states it (~4938-4966): *from
// an append-only byte ring, a live dialog and an answered one that has not
// scrolled away are byte-identical.* `prompt_wait_detected` reads such a ring,
// so on a quiet pane the hold re-asserts forever and the only bound available
// was a badge (#536), never a release. #513's live 27-minute orchestrator-
// inbound stall is that shape, and its trigger is still unknown because the
// abort record never said what the guard keyed on.
//
// Three pieces and the seams between them:
//   A. `termgrid::render_visible` exposes ONLY rendered rows. The design note
//      names the trap: a history-included read reproduces the bug, because an
//      answered question that scrolled off is still in the history half.
//   B. `prompt_wait_match` reports WHAT fired, with `prompt_wait_detected`
//      defined as `.is_some()` on it so the two cannot drift.
//   C/D. `question_shown` combines the readings ONE-DIRECTIONALLY — the ring
//      triggers, the grid may only release.

/// Raw pty bytes for a sequence of painted lines.
///
/// `\r\n`, not `\n`, and it is load-bearing rather than pedantic: a bare LF is
/// an INDEX — down one row, column untouched — so a fixture written with `\n`
/// staircases every line rightward across the grid and composes into something
/// no terminal would ever show. A pty emits CRLF; so does this.
pub(crate) fn painted(lines: &[&str]) -> Vec<u8> {
    lines.iter().flat_map(|l| format!("{l}\r\n").into_bytes()).collect()
}

/// A pane's two readings built from ONE raw stream, exactly as
/// `question_sample` does in production: the grid replays everything, the ring
/// is the trailing window the detector has always used.
pub(crate) fn sample_from_raw(raw: &[u8], cols: u16, rows: u16) -> QuestionSample {
    QuestionSample {
        ring: Some(raw[raw.len().saturating_sub(4096)..].to_vec()),
        visible: trustworthy_composition(
            loomux_lib::orchestration::termgrid::render_visible(raw, cols, rows),
        ),
    }
}

/// A question, then enough ordinary output to scroll it off a SHORT screen
/// while leaving it inside BOTH the ring window and the detector's own
/// last-12-non-empty-lines window. The #532 incident, minimally.
///
/// The trailing-line count is load-bearing in two directions at once and a
/// test built on this must not quietly lose either: too many and the question
/// falls out of the detector's 12-line window, so the ring stops matching and
/// the test passes for the wrong reason; too few and it never leaves a 6-row
/// screen, so there is nothing for the grid to notice. Nine lines total sits
/// in both windows.
fn scrolled_off_question() -> Vec<u8> {
    let mut lines = vec!["Do you want to run npm test? (y/n)".to_string()];
    lines.extend((0..8).map(|i| format!("running step {i}")));
    painted(&lines.iter().map(String::as_str).collect::<Vec<_>>())
}

// ---- A. render_visible: rendered rows only ----

#[test]
fn a1_render_visible_drops_the_scrolled_off_question_that_render_screen_keeps() {
    // THE bug, put to both readings of the same bytes. The byte ring still
    // holds the answered question, and so does `render_screen` — its history
    // half is exactly where scrolled rows go. Only `render_visible` agrees
    // with the human's eyes.
    let raw = scrolled_off_question();
    let history_included = loomux_lib::orchestration::termgrid::render_screen(&raw, 80, 6);
    let visible_only = loomux_lib::orchestration::termgrid::render_visible(&raw, 80, 6);

    assert!(
        history_included.contains("(y/n)"),
        "render_screen keeps scrolled-off rows — its job for get_output, and precisely \
         why pointing the detector at it would have reproduced the bug"
    );
    assert!(
        !visible_only.contains("(y/n)"),
        "render_visible must expose only what is ON SCREEN; the question scrolled away"
    );
    assert!(visible_only.contains("running step 7"), "the live rows are still there");
}

#[test]
fn a2_render_visible_excludes_the_primary_screen_parked_behind_an_alt_screen() {
    // `into_text` deliberately shows the parked primary (a `get_output` reader
    // wants the context a pager covers). It is by definition NOT displayed, so
    // a "still on screen" reading must not see it — a second exclusion, which
    // a naive `grid + parked` render would miss even with history dropped.
    let mut raw = painted(&["Do you want to proceed?"]);
    raw.extend_from_slice(b"\x1b[?1049h"); // enter alt screen
    raw.extend_from_slice(&painted(&["PAGER LINE ONE", "PAGER LINE TWO"]));

    let history_included = loomux_lib::orchestration::termgrid::render_screen(&raw, 80, 10);
    let visible_only = loomux_lib::orchestration::termgrid::render_visible(&raw, 80, 10);

    assert!(
        history_included.contains("Do you want to proceed?"),
        "the parked primary is kept by render_screen"
    );
    assert!(
        !visible_only.contains("Do you want to proceed?"),
        "the parked primary is behind the alt screen — not displayed, not evidence"
    );
    assert!(visible_only.contains("PAGER LINE TWO"), "the alt screen IS what is displayed");
}

#[test]
fn a3_render_visible_keeps_a_dialog_that_is_still_on_screen() {
    // The other half of A1: absence only means something if presence survives.
    let raw = painted(&["building...", "Do you want to run npm test? (y/n)"]);
    let visible = loomux_lib::orchestration::termgrid::render_visible(&raw, 80, 6);
    assert!(visible.contains("(y/n)"), "a dialog on screen must render as on screen");
}

#[test]
fn a4_render_visible_drops_a_dialog_the_cli_erased_in_place() {
    // The case a byte ring can NEVER see: the CLI answers its own question and
    // repaints over it without scrolling. Nothing leaves the ring; everything
    // leaves the screen.
    let mut raw = painted(&["Do you want to run npm test? (y/n)"]);
    raw.extend_from_slice(b"\x1b[2J\x1b[H"); // erase display, cursor home
    raw.extend_from_slice(&painted(&["> ", "(idle)"]));
    let visible = loomux_lib::orchestration::termgrid::render_visible(&raw, 80, 10);
    assert!(!visible.contains("(y/n)"), "an in-place erase removes it from the screen");
    assert!(visible.contains("(idle)"), "and leaves what the CLI painted instead");
}

// ---- B. prompt_wait_match: the detector names its own evidence ----

#[test]
fn b1_prompt_wait_match_and_prompt_wait_detected_can_never_disagree() {
    // `prompt_wait_detected` is DEFINED as `prompt_wait_match(..).is_some()`,
    // so this is true by construction — asserted anyway over every fixture the
    // guard is graded on, because the day someone re-inlines the boolean for
    // speed is the day the audit starts lying about a hold that happened. The
    // false-positive fixtures are included: parity has to hold on the
    // negatives too, or the match would be reporting phantom evidence.
    for (name, tail) in [
        ("claude-ask", FIX_CLAUDE_ASK),
        ("copilot-ask", FIX_COPILOT_ASK),
        ("pointer-last", FIX_POS_PTR_LAST),
        ("copilot-multichoice", FIX_COPILOT_MULTICHOICE),
        ("claude-mcp-approval", FIX_CLAUDE_MCP_APPROVAL),
        ("streaming", FIX_STREAMING),
        ("idle-box", FIX_IDLE_BOX),
        ("fp-prose", FIX_FP_PROSE),
        ("fp-shell", FIX_FP_SHELL),
        ("fp-breadcrumb", FIX_FP_BREADCRUMB),
        ("fp-leading-ptr", FIX_FP_LEADING_PTR),
        ("fp-fenced-ptr", FIX_FP_FENCED_PTR),
        ("fp-resumed-idle", FIX_FP_RESUMED_IDLE),
    ] {
        let stripped = strip_ansi(tail.as_bytes());
        assert_eq!(
            prompt_wait_match(&stripped).is_some(),
            prompt_wait_detected(&stripped),
            "{name}: the match and the boolean must be the same detector"
        );
    }
    assert!(prompt_wait_match("").is_none(), "empty tail: nothing matched, nothing to report");
}

#[test]
fn b2_prompt_wait_match_names_the_signal_and_the_line_it_fired_on() {
    // #513(c): the abort audit recorded `to`/`stage`/`held_ms` and nothing
    // about the trigger, which is why that incident is still unexplained.
    // These are the fields that end that.
    let m = prompt_wait_match("building...\nOverwrite the file? (y/n)").expect("y/n must match");
    assert_eq!(m.signal, "yes-no-token");
    assert_eq!(m.needle, QuestionNeedle::Token("(y/n)"));
    assert_eq!(m.line, "overwrite the file? (y/n)", "the detector's own normalization, not raw bytes");

    let m = prompt_wait_match("Do you trust the files in this folder?").expect("permission must match");
    assert_eq!(m.signal, "permission-phrase");
    assert_eq!(m.needle, QuestionNeedle::Token("do you trust"));

    let m = prompt_wait_match("pick one\n  option a\n> option b\n❯ option c").expect("pointer must match");
    assert_eq!(m.signal, "pointer-option");
    assert_eq!(
        m.needle,
        QuestionNeedle::LeadingPointer,
        "a leading glyph is a POSITION — re-read positionally, never by string search"
    );
    assert_eq!(m.line, "❯ option c");

    let m = prompt_wait_match("choose\n  a\n  b\nuse arrow keys to move").expect("footer must match");
    assert_eq!(m.signal, "menu-footer");
}

#[test]
fn b5_a_dialog_painting_both_footer_and_pointer_reports_the_token_bearing_one() {
    // #534 rev-13. Real menus routinely paint both, and the reported match
    // decides which needle the composed-screen re-read gets: the footer's is a
    // literal token, the pointer's is a position that is strictly harder to
    // re-find (and was round 2's blocking finding). Among two signals that
    // fired on the same dialog, prefer the evidence that survives
    // recomposition.
    let both = "which option?\n  Yes\n❯ No\n↑↓ to select, enter to confirm";
    let m = prompt_wait_match(both).expect("both signals are present");
    assert_eq!(m.signal, "menu-footer", "the token-bearing signal is reported");
    assert!(matches!(m.needle, QuestionNeedle::Token(_)));
    // And the boolean is untouched by the ordering — it is a disjunction.
    assert!(prompt_wait_detected(both));
}

#[test]
fn b3_prompt_wait_match_bounds_the_line_it_records() {
    // An audit field must not become a channel for whatever length of line a
    // pane felt like painting.
    let long = format!("{} (y/n)", "x".repeat(5000));
    let m = prompt_wait_match(&long).expect("still matches");
    assert_eq!(m.line.chars().count(), QuestionMatch::MAX_LINE, "the recorded line is capped");
}

#[test]
fn b4_witness_audit_says_null_rather_than_nothing_when_no_question_was_seen() {
    // `null` is a real answer: on a `delivery-aborted-question` record it means
    // the abort outcome and the detector disagree, which is itself the finding.
    // The old records were indistinguishable from that case in every direction.
    assert!(witness_audit(None).is_null(), "no match seen must be recorded, not omitted");

    let seen = QuestionWitnessed {
        matched: prompt_wait_match("Overwrite? (y/n)").expect("matches"),
        grid: GridEvidence::NotRendered,
        idle_row: false,
    };
    let v = witness_audit(Some(&seen));
    assert_eq!(v["signal"], "yes-no-token");
    assert_eq!(v["line"], "overwrite? (y/n)");
    assert_eq!(v["grid"], "not-rendered", "whether the screen agreed is the diagnostic half");
    // #903: the override's own term, in the record. A `delivery-question-override`
    // line a human cannot check the evidence of is not an audit.
    assert_eq!(v["idle_row"], false, "and whether the CLI's composer was on that screen");
}

// ---- C. question_shown: the truth table, one row of which is new ----

#[test]
fn c1_no_ring_match_never_consults_the_grid() {
    // The grid may only ever RELEASE. If the ring says nothing, the reading is
    // clear no matter what is composed — even a screen full of live dialog.
    // This is what keeps the change one-directional: no new false-positive
    // class can enter through the grid.
    let live_dialog = "Do you want to run npm test? (y/n)\n> ";
    assert!(
        !question_shown(None, Some(Composed::plain(live_dialog))),
        "the ring is the trigger; the grid must not invent a hold"
    );
}

#[test]
fn c2_an_unreadable_grid_leaves_the_ring_the_authority() {
    // `visible: None` is "no trustworthy composition", NOT "a blank screen" —
    // pty gone, geometry unknown, replay never painted over. Behaviour here
    // must be exactly what it was before #534.
    let m = prompt_wait_match("Overwrite? (y/n)").expect("matches");
    assert!(question_shown(Some(&m), None), "no evidence either way -> the ring's word stands");
    assert_eq!(grid_evidence_for(&m, None), GridEvidence::Unreadable);
}

#[test]
fn c3_still_rendered_holds() {
    let m = prompt_wait_match("Overwrite the file? (y/n)").expect("matches");
    // #903 changed this screen, and the reason is that the old one contradicted
    // itself. It was `"some output\nOverwrite the file? (y/n)\n> "` — an inline
    // yes/no prompt sitting ABOVE the CLI's own empty composer, which is not a
    // shape any TUI paints: an inline prompt takes the cursor, it does not hand
    // the box back. `c3b` below pins what that screen now answers and why.
    // Here the prompt is the last thing painted, which is what a live one is.
    let screen = "some output\nOverwrite the file? (y/n)";
    assert_eq!(grid_evidence_for(&m, Some(Composed::plain(screen))), GridEvidence::StillRendered);
    assert!(question_shown(Some(&m), Some(Composed::plain(screen))), "still displayed -> still holding");
}

#[test]
fn c3b_still_rendered_loses_to_an_idle_composer_underneath_it() {
    // #903, at the seam: the matched text IS still rendered, and the hold ends
    // anyway. `StillRendered` was true and useless on the panes this issue was
    // filed for — question-shaped text on a screen whose own bottom row said the
    // CLI was waiting for free text, not for an answer.
    let m = prompt_wait_match("Overwrite the file? (y/n)").expect("matches");
    let screen = "some output\nOverwrite the file? (y/n)\n> ";
    assert!(
        match_still_rendered(screen, &m),
        "precondition: the match really is on this screen — the release is NOT 'it went away'"
    );
    assert_eq!(
        grid_evidence_for(&m, Some(Composed::plain(screen))),
        GridEvidence::IdlePrompt,
        "an empty composer is positive evidence that nothing is being asked"
    );
    assert!(!question_shown(Some(&m), Some(Composed::plain(screen))), "idle at an empty box -> release");
}

#[test]
fn c4_not_rendered_is_the_one_transition_this_change_adds() {
    // Ring matched, screen composed cleanly, and neither the match nor any
    // other question shape is on it. Before #534 no reading could reach this
    // conclusion at all — which is why the hold could only ever be badged.
    let m = prompt_wait_match("Overwrite the file? (y/n)").expect("matches");
    // No trailing `> ` row: #903's idle-composer reading is checked FIRST and
    // would answer `IdlePrompt` here, which is a true statement about the screen
    // but not the one this test is about. `c3b` covers that reading; this one
    // still has to pin #534's own transition on a screen where it is the only
    // reason to release.
    let screen = "running step 11\nrunning step 12\nrunning step 13";
    assert_eq!(grid_evidence_for(&m, Some(Composed::plain(screen))), GridEvidence::NotRendered);
    assert!(!question_shown(Some(&m), Some(Composed::plain(screen))), "no longer rendered -> answered -> release");
}

#[test]
fn c5_a_dialog_outside_the_detectors_own_window_still_counts_as_rendered() {
    // `prompt_wait_detected`'s last-12-lines rule is CHRONOLOGICAL — written
    // for a stream, where recent means last. On a screen it is spatial, and a
    // dialog can sit above 12 rows of statusline and input box. So the match's
    // own line is looked for anywhere on screen; without that, a tall pane
    // would release into a live dialog.
    let m = prompt_wait_match("Do you want to proceed? (y/n)").expect("matches");
    let mut screen = String::from("Do you want to proceed? (y/n)\n");
    for i in 0..20 {
        screen.push_str(&format!("status row {i}\n"));
    }
    assert!(
        !prompt_wait_detected(&screen),
        "precondition: the detector's own window cannot see it — this is the gap being covered"
    );
    assert_eq!(grid_evidence_for(&m, Some(Composed::plain(&screen))), GridEvidence::StillRendered);
    assert!(question_shown(Some(&m), Some(Composed::plain(&screen))), "displayed is displayed, wherever on screen");
}

#[test]
fn c6_a_matched_line_re_wrapped_across_rows_still_counts_as_rendered() {
    // A line the ring saw as one write is two rows on a narrower screen. A
    // row-by-row compare would miss it and call a plainly-visible dialog gone
    // — the one direction this must never be wrong in.
    let m = prompt_wait_match("do you want to make this edit to mod.rs").expect("matches");
    let wrapped = "do you want to make this\nedit to mod.rs\n> ";
    assert!(
        match_still_rendered(wrapped, &m),
        "wrap-insensitive: a row break is whitespace, not evidence of absence"
    );
    assert!(
        !match_still_rendered("edit the file\n> ", &m),
        "and sharing a word or two is not a match"
    );
}

// ---- C8-C11: the `pointer-option` class (#534 rev-13, round-2 blocking) ----
//
// This signal is the only one whose evidence is a POSITION rather than a
// substring. Round 1 gave it no needle and let `match_still_rendered` fall back
// to searching for the matched line — which comes from `strip_ansi` of the byte
// ring, cursor addressing deleted, so a redraw-fragmented row arrives as a
// concatenation that was never on screen and can never be found on a clean
// grid. The compensating check silently became a no-op, in the false-RELEASE
// direction, for exactly the layout it was added to compensate for.

/// The review's scenario, built as bytes rather than asserted about: a CLI
/// repaints one physical row by cursor address, so `strip_ansi` concatenates
/// the frames into a line that never existed on any screen.
fn redraw_fragmented_pointer_tail() -> String {
    // Two paints of the same row, cursor-addressed between them — exactly what
    // `termgrid`'s own header documents for Claude Code.
    let raw = b"\x1b[5;1H\xe2\x9d\xaf Yes, allow once\x1b[5;1H\xe2\x9d\xaf Yes, allow once".to_vec();
    strip_ansi(&raw)
}

#[test]
fn c8_a_redraw_fragmented_pointer_line_is_a_needle_that_can_never_be_found() {
    // The precondition the whole finding rests on. If this ever stops being
    // true the tests below would pass for the wrong reason, so pin it.
    let tail = redraw_fragmented_pointer_tail();
    let m = prompt_wait_match(&tail).expect("a leading pointer still matches on the ring");
    assert_eq!(m.signal, "pointer-option");
    assert!(
        m.line.matches("yes, allow once").count() > 1,
        "the recorded line is a multi-frame concatenation, not a screen row: {:?}",
        m.line
    );
    // A clean grid showing that menu does NOT contain the concatenation.
    let clean_screen = "❯ yes, allow once\n  no, and tell me why";
    assert!(
        !flat_contains_line(clean_screen, &m.line),
        "this is why a line search cannot be the only reading for this class"
    );
}

/// Does the composed screen contain the match's recorded line, whitespace
/// flattened — i.e. the round-1 check, isolated so C8 can show it failing on a
/// screen that plainly shows the menu.
fn flat_contains_line(visible: &str, line: &str) -> bool {
    let flat = visible.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    flat.contains(&line.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase())
}

#[test]
fn c9_a_live_pointer_menu_above_the_chrome_holds_though_every_other_reading_says_clear() {
    // The false-release path, end to end, at the decision that would have
    // pressed Enter into a live menu.
    //
    // Every ingredient is the defining case for this signal class: a bare
    // pointer menu matches none of the token rules (that is the only way
    // `pointer-option` is reached at all), and it sits ABOVE the input box and
    // status chrome, so the detector's own last-3-lines pointer window cannot
    // see it either.
    let m = prompt_wait_match(&redraw_fragmented_pointer_tail()).expect("matches on the ring");
    let screen = "❯ yes, allow once\n  no, and tell me why\n\nesc to cancel\n\n> \nready";

    // The two readings that were relied on in round 1, both genuinely clear:
    assert!(
        !prompt_wait_detected(screen),
        "precondition: the menu is more than 3 non-empty rows from the bottom, \
         so the detector's CHRONOLOGICAL window cannot see it on a SPATIAL layout"
    );
    assert!(
        !flat_contains_line(screen, &m.line),
        "precondition: the concatenated needle is not on the clean screen"
    );

    // ...and yet the menu is plainly displayed, so this must hold.
    assert!(
        match_still_rendered(screen, &m),
        "the pointer is re-read positionally across ALL rendered rows"
    );
    assert_eq!(grid_evidence_for(&m, Some(Composed::plain(screen))), GridEvidence::StillRendered);
    assert!(
        question_shown(Some(&m), Some(Composed::plain(screen))),
        "a live menu must never release — this is the #420 harm the whole change is fenced against"
    );
}

#[test]
fn c10_a_pointer_menu_genuinely_gone_still_releases() {
    // The other half: the fix must not degrade this class into "never
    // releases". With no pointer anywhere on the composed screen, the evidence
    // is as good as any token class's.
    let m = prompt_wait_match(&redraw_fragmented_pointer_tail()).expect("matches on the ring");
    let answered = "npm test running\nall good\n\n> \nready";
    assert!(!match_still_rendered(answered, &m));
    // #903: `> ` is an empty composer, so this screen now answers with the more
    // specific of the two release readings. The behaviour under test — a pointer
    // menu that is genuinely gone still releases — is `question_shown` below,
    // and it is unchanged.
    assert_eq!(grid_evidence_for(&m, Some(Composed::plain(answered))), GridEvidence::IdlePrompt);
    assert!(!question_shown(Some(&m), Some(Composed::plain(answered))), "answered and gone -> release");
}

#[test]
fn c11_the_pointer_re_read_is_spatial_not_chronological() {
    // The root mismatch, isolated. `prompt_wait_match` reads its pointer from
    // the last 3 non-empty lines because a stream's "recent" means "last"; a
    // screen's does not. Inheriting that window on the grid is what made the
    // menu in C9 invisible, so the re-read must scan every rendered row —
    // including one at the very top, under box framing.
    let m = prompt_wait_match(&redraw_fragmented_pointer_tail()).expect("matches on the ring");
    let mut screen = String::from("│ ❯ yes, allow once\n");
    for i in 0..20 {
        screen.push_str(&format!("status row {i}\n"));
    }
    assert!(!prompt_wait_detected(&screen), "precondition: outside the detector's own window");
    assert!(
        match_still_rendered(&screen, &m),
        "a deframed leading glyph anywhere on the screen is the menu, wherever it sits"
    );

    // Mid-line glyphs are NOT the signal — that is the false-positive rule the
    // ring detector already encodes, and the re-read must not be looser than
    // the detector it is standing in for, or ordinary prose would pin a hold
    // open forever.
    let prose = "run it with demo ❯ npm run dev\nsee Home › Prefs\n> \nready";
    assert!(
        !match_still_rendered(prose, &m),
        "a glyph mid-line is pervasive in ordinary output and is not a menu"
    );
}

#[test]
fn c7_a_blank_composition_is_unreadable_not_clear() {
    // A replay that began mid-stream and was never painted over composes to
    // near-nothing. "The screen is blank" must not be mistaken for "the screen
    // is clear" — that is the blind-start hole, closed by refusing to read
    // such a composition at all.
    assert_eq!(trustworthy_composition(String::new()), None);
    assert_eq!(trustworthy_composition("   \n\n  ".into()), None);
    assert_eq!(
        trustworthy_composition("> \n(idle)".into()).as_deref(),
        Some("> \n(idle)"),
        "a populated screen is evidence"
    );
}

// ---- D. the predicate, driven by faked composed-grid states ----

#[test]
fn d1_a_live_question_still_rendered_never_releases() {
    // Both readings agree; poll it as often as you like.
    let raw = painted(&["building the project", "Do you want to run npm test? (y/n)"]);
    let pred = question_hold_predicate_sampled(move || sample_from_raw(&raw, 80, 10), None, None, Vec::new());
    for i in 0..5 {
        assert!(pred(), "poll {i}: the dialog is on screen — must hold");
    }
}

#[test]
fn d2_a_question_that_scrolled_off_screen_releases_though_the_ring_still_holds_it() {
    // #532's empty-box incident, mechanism for mechanism: the question is
    // answered and gone from the screen, but it has NOT left the byte ring, so
    // the pre-#534 guard re-asserted the hold forever.
    let raw = scrolled_off_question();
    // Precondition — without it this test would pass for the wrong reason.
    assert!(
        prompt_wait_detected(&strip_ansi(&raw)),
        "the byte ring STILL matches: that is the bug being fixed, not an artifact of the fixture"
    );
    let pred = question_hold_predicate_sampled(move || sample_from_raw(&raw, 80, 6), None, None, Vec::new());
    assert!(!pred(), "the screen says answered — no hold ever starts");
}

#[test]
fn d3_an_answered_question_erased_in_place_releases_after_the_hysteresis() {
    // A hold that genuinely started, then the CLI repaints over its own
    // dialog. Release must STILL take two consecutive clear reads — the grid
    // is new evidence, not a shortcut past rev-19 R1's transient-redraw guard.
    let live = painted(&["Do you want to run npm test? (y/n)", "choose an option"]);
    let mut answered = live.clone();
    answered.extend_from_slice(b"\x1b[2J\x1b[H"); // erase display, cursor home
    answered.extend_from_slice(&painted(&["npm test running", "all good"]));

    let call = std::sync::atomic::AtomicU32::new(0);
    let pred = question_hold_predicate_sampled(
        move || {
            let n = call.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n == 0 { sample_from_raw(&live, 80, 10) } else { sample_from_raw(&answered, 80, 10) }
        },
        None,
        None,
        Vec::new(),
    );
    assert!(pred(), "poll 1: dialog on screen -> hold");
    assert!(pred(), "poll 2: screen now clear, but one clear read is not enough");
    assert!(!pred(), "poll 3: second consecutive clear read -> release");
}

#[test]
fn d4_tui_redraw_churn_around_a_live_dialog_never_false_clears() {
    // The failure mode a naive rendered-rows read would have: a TUI repaints
    // its frame constantly, so the composed screen is built from different
    // bytes on every poll. What must not change is the answer, as long as the
    // dialog is still painted.
    let dialog = "Do you want to run npm test? (y/n)";
    let frames: Vec<Vec<u8>> = (0..6)
        .map(|i| {
            let mut f = b"\x1b[H\x1b[2J".to_vec(); // home, erase, repaint the frame
            f.extend_from_slice(&painted(&[dialog, "  Yes", "  No", &format!("working {i}")]));
            f
        })
        .collect();
    let call = std::sync::atomic::AtomicU32::new(0);
    let pred = question_hold_predicate_sampled(
        move || {
            let n = call.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as usize;
            sample_from_raw(&frames[n % frames.len()], 80, 10)
        },
        None,
        None,
        Vec::new(),
    );
    for i in 0..6 {
        assert!(pred(), "frame {i}: repaint churn is not an answer — the dialog is still there");
    }
}

#[test]
fn d4b_a_poll_landing_mid_repaint_is_survived_by_the_hysteresis_not_by_the_grid() {
    // #534 rev-13, review N1. `d4` only covers "the bytes change, the answer
    // does not" — every frame there repaints the dialog. It does NOT cover a
    // poll landing INSIDE a repaint: after the erase, before the dialog row is
    // painted. That composition genuinely reads clear, and pretending
    // otherwise would be the unbacked claim the lessons file calls a defect.
    //
    // What actually defends it is stated honestly here rather than in prose:
    // (a) a fully-erased screen composes to nothing and is `Unreadable`, not
    // clear; (b) a partly-painted one does read clear, so it takes TWO
    // consecutive such polls to release, which is the hysteresis doing the
    // work — not the grid.
    let dialog = "Do you want to run npm test? (y/n)";
    // CUMULATIVE, as a real ring is — otherwise the byte ring would lose the
    // dialog along with the screen and the test would prove nothing about the
    // grid, which is the whole point of these polls.
    let mut full = b"\x1b[H\x1b[2J".to_vec();
    full.extend_from_slice(&painted(&[dialog, "  Yes", "  No", "working"]));
    // Erased, chrome painted, dialog row not yet: the ring still holds the
    // dialog, the SCREEN genuinely does not.
    let mut mid = full.clone();
    mid.extend_from_slice(b"\x1b[H\x1b[2J");
    mid.extend_from_slice(&painted(&["  Yes", "  No"]));
    // Erased with nothing painted at all.
    let mut erased = full.clone();
    erased.extend_from_slice(b"\x1b[H\x1b[2J");
    let mut repainted = mid.clone();
    repainted.extend_from_slice(b"\x1b[H\x1b[2J");
    repainted.extend_from_slice(&painted(&[dialog, "  Yes", "  No", "working"]));

    assert!(
        prompt_wait_detected(&strip_ansi(&mid)),
        "precondition: the RING still matches through the repaint — only the screen went clear"
    );
    assert_eq!(
        trustworthy_composition(loomux_lib::orchestration::termgrid::render_visible(&erased, 80, 10)),
        None,
        "(a) a fully-erased screen is Unreadable, so it can never be read as 'the dialog is gone'"
    );

    // (b) one mid-repaint poll between two good frames must not release.
    let seq = [full, mid, repainted];
    let call = std::sync::atomic::AtomicU32::new(0);
    let pred = question_hold_predicate_sampled(
        move || {
            let n = call.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as usize;
            sample_from_raw(&seq[n.min(seq.len() - 1)], 80, 10)
        },
        None,
        None,
        Vec::new(),
    );
    assert!(pred(), "poll 1: dialog painted -> hold");
    assert!(pred(), "poll 2: mid-repaint reads clear, but one clear read is not enough");
    assert!(pred(), "poll 3: repaint completed -> the clear streak is broken, still holding");
}

#[test]
fn d5_the_predicate_records_what_it_held_for_and_whether_the_screen_agreed() {
    // #513(c)/F2 end to end: the field whose absence left a 27-minute stall
    // undiagnosable. Recorded on every poll the ring matched, INCLUDING the
    // ones the grid contradicted — "the ring kept matching but the screen said
    // gone" is the single most useful line such a diagnosis could have.
    let raw = scrolled_off_question();
    let witness: QuestionWitness = Default::default();
    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&raw, 80, 6),
        None,
        Some(std::rc::Rc::clone(&witness)),
        Vec::new(),
    );
    assert!(!pred(), "scrolled off the screen -> released");

    let seen = witness.borrow().clone().expect("the ring matched, so the witness must hold it");
    assert_eq!(seen.matched.signal, "yes-no-token");
    assert!(seen.matched.line.contains("(y/n)"));
    assert_eq!(
        seen.grid,
        GridEvidence::NotRendered,
        "the record has to name the disagreement, not just that it held"
    );
}

#[test]
fn d6_our_own_pasted_text_rendered_in_the_box_is_masked_on_the_grid_too() {
    // `mask_own_paste` has to run on BOTH readings or the guard contradicts
    // itself, and the grid is where it bites HARDEST: our own just-pasted,
    // not-yet-submitted text sits in the input box, where it is genuinely
    // RENDERED and stays rendered — unlike in the ring, where it scrolls out
    // of a 4 KiB window soon enough.
    //
    // The scenario that separates the two: a real dialog matched by the ring
    // has since scrolled off the screen (so this should RELEASE), while a
    // brief that happens to quote a permission phrase is still displayed in
    // the box. Unmasked, the grid would answer "still displayed" about our own
    // paste and strand the delivery (rev-15 N1 / rev-19 B-A, now owed by the
    // grid too).
    let pasted = "please confirm: do you want to proceed with the deploy";
    let mut lines = vec!["Overwrite the file? (y/n)".to_string()];
    lines.extend((0..6).map(|i| format!("running step {i}")));
    lines.push(pasted.to_string());
    let raw = painted(&lines.iter().map(String::as_str).collect::<Vec<_>>());

    // Preconditions, so a fixture that drifts fails loudly instead of passing
    // for a reason this test is not about.
    let visible = loomux_lib::orchestration::termgrid::render_visible(&raw, 80, 6);
    assert!(!visible.contains("(y/n)"), "the real dialog has scrolled off the screen");
    assert!(visible.contains(pasted), "our own paste is still rendered in the box");
    assert!(
        prompt_wait_detected(&strip_ansi(&raw)),
        "the ring still matches the scrolled-off dialog — that is what triggers the guard"
    );

    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&raw, 80, 6),
        Some(pasted.to_string()),
        None,
        Vec::new(),
    );
    assert!(
        !pred(),
        "the dialog is off screen and the only question-shaped text left rendered is OUR OWN — release"
    );
}

#[test]
fn d7_the_ring_only_predicate_is_unchanged_by_all_of_this() {
    // `question_hold_predicate` is the fallback path — `visible: None` on every
    // poll — and its behaviour is the pre-#534 guard exactly. The rev-19 tests
    // above pin its release rules; this one pins the *reason* it still holds
    // where the grid would have released, so nobody "simplifies" the fallback
    // into consulting a grid it does not have.
    let raw = scrolled_off_question();
    let pred = question_hold_predicate(move || Some(raw.clone()), None, Vec::new());
    assert!(pred(), "no composed screen to consult -> the byte ring's word is final");
}

// ---------- #576: loomux's own notice rows must not latch the question gate ----------

/// The exact line `mcp.rs` composes when it relays a worker's `report` note
/// into the orchestrator's pane (`[orrerix] {agent_id} reports {status}:
/// {note}`), carrying a note that is *about* a dialog.
///
/// Two of the detector's STRUCTURED signals live in this one row — `do you
/// want to run` (permission-phrase) and `(y/n)` (yes-no-token) — and neither
/// is a question anyone is asking this pane. Structured signals are honored
/// across the last twelve non-empty lines, so unlike the prose-like ones they
/// are not pushed out of range when the CLI redraws its input box underneath:
/// nothing a quiet pane does will clear them.
const LOOMUX_REPORT_RELAY: &str = "[orrerix] w-119 reports blocked: Copilot is asking \
\"Do you want to run npm test? (y/n)\" and I cannot answer it myself";

/// A pane at rest: the relayed notice, then the CLI's redrawn idle input box.
/// Modelled on `tests/fixtures/attention/idle-input-box.txt` — the box is what
/// a real CLI paints under a submitted message, and it is deliberately present
/// so these tests cannot pass merely because the pane was empty.
fn pane_with_relayed_notice() -> Vec<u8> {
    painted(&[
        LOOMUX_REPORT_RELAY,
        "",
        "╭──────────────────────────────────╮",
        "│ > Try \"fix the build\"            │",
        "╰──────────────────────────────────╯",
        "  ? for shortcuts",
    ])
}

#[test]
fn e1_a_relayed_report_note_must_not_latch_the_ring_only_gate() {
    // THE #576 bug, at the reading that triggers it. `pasted_text` is `None`
    // because that is what all three `question_active_now` call sites pass:
    // the checkpoint runs BEFORE the entry it is considering has written
    // anything, so `mask_own_paste` has nothing of its own to mask and
    // structurally cannot help here. The pane's ring nonetheless holds the
    // PREVIOUS delivery's text, which is loomux's own.
    let raw = pane_with_relayed_notice();
    let pred = question_hold_predicate(move || Some(raw.clone()), None, Vec::new());
    assert!(
        !pred(),
        "loomux's own relayed report note is text ABOUT a question, not a question being \
         asked of this pane — the gate must not latch on what loomux itself wrote (#576)"
    );
}

#[test]
fn e2_a_relayed_report_note_must_not_latch_the_two_reading_gate_either() {
    // The same notice put to BOTH readings, which is the production shape and
    // the reason #534 could not already have fixed this: our own text is
    // genuinely rendered, so the grid agrees it is on screen — and the grid is
    // right. It is simply not a question.
    let raw = pane_with_relayed_notice();

    // Preconditions, so a drifting fixture fails loudly instead of passing for
    // a reason these tests are not about. Both stay true after the fix:
    // `prompt_wait_detected` itself is untouched, and so is the composition.
    assert!(
        prompt_wait_detected(&strip_ansi(&raw)),
        "unmasked, the ring matches — that is the trigger #576 is about"
    );
    let visible = loomux_lib::orchestration::termgrid::render_visible(&raw, 120, 12);
    assert!(
        visible.contains("reports blocked"),
        "and the notice is genuinely RENDERED, which is exactly why #534's grid release \
         cannot reach this: both readings agree, and both are correct about the pixels"
    );

    let pred = question_hold_predicate_sampled(move || sample_from_raw(&raw, 120, 12), None, None, Vec::new());
    assert!(
        !pred(),
        "the only question-shaped text on this pane is loomux's own notice — release (#576)"
    );
}

#[test]
fn e3_a_quoted_marker_row_must_not_hide_a_genuine_dialog_below_it() {
    // The adversarial case, and the reason the mask claims ONE row.
    //
    // The `[orrerix]` marker is unforgeable only in the DELIVERY direction:
    // `notify::sanitize_gh_text` rewrites `[`/`]` in every untrusted field, so
    // nothing an agent sends THROUGH loomux can carry it. An agent's own pane
    // output is not sanitized at all, so an agent can print a marker row
    // itself — echoing a notice back, quoting one in a summary, or induced to
    // by a hostile prompt.
    //
    // If masking a marker row also swallowed the rows around it, that row
    // would become a way to hide a live permission dialog from the gate, and
    // the Enter the gate then released would answer it. That is the #420 harm,
    // reachable from pane output. Failing OPEN is the dangerous direction.
    let raw = painted(&[
        "[orrerix] w-3 reports done: PR #900 is green",
        "◆ Allow Copilot to run the following command?",
        "",
        "  $ rm -rf build",
        "",
        "│ ❯ Yes",
        "│   No, and tell Copilot what to do differently",
        "",
        "Use arrow keys · Enter to confirm · Esc to cancel",
    ]);
    let pred = question_hold_predicate_sampled(move || sample_from_raw(&raw, 120, 14), None, None, Vec::new());
    assert!(
        pred(),
        "a genuine dialog sharing a pane with a quoted [orrerix] row must STILL latch — \
         masking a marker row must never release an Enter into a live question (#576)"
    );
}

#[test]
fn e4_a_dialog_painted_directly_under_a_marker_row_still_latches() {
    // The same hazard with NO blank row between the marker row and the
    // question — precisely the shape a wrap-run mask ("mask the marker row and
    // every non-blank row after it") would have swallowed whole. A run-mask is
    // the tempting way to cover a notice that wrapped; this stands guard
    // against reintroducing it as a "wrap fix" later.
    let raw = painted(&[
        "[orrerix] w-3 reports done: PR #900 is green",
        "Do you want to run npm test? (y/n)",
    ]);
    let pred = question_hold_predicate_sampled(move || sample_from_raw(&raw, 120, 10), None, None, Vec::new());
    assert!(
        pred(),
        "only the row the marker LEADS is masked — the dialog row beneath it must survive (#576)"
    );
}

#[test]
fn e9_a_multi_row_resume_notice_no_longer_latches_the_gate_it_is_reported_into() {
    // #632, behaviourally rather than at the string level: the SAME producer,
    // painted onto a pane, put to the real question-hold predicate.
    //
    // Before this, only the header row of `pause_suppression_notice` carried
    // the marker. Its `  - w-2 -> orch-1 (…): <preview>` item rows landed
    // unmasked in the tail of an orchestrator pane, carrying whatever
    // question-shaped tokens the lost payloads happened to contain — and the
    // structured signals (`do you want to run`, `(y/n)`) are honoured across
    // the last twelve non-empty lines, so a redraw never pushes them out. The
    // pane then held on a question nobody asked, and a held pane emits nothing
    // fresh to clear itself: the #576 self-latch, one row further down.
    let s = PauseSuppression {
        items: vec![SuppressedDelivery {
            from: "w-2".into(),
            to: "orch-1".into(),
            preview: "Do you want to run npm test? (y/n)".into(),
            cause: SuppressedCause::QueueFullDuringPause,
        }],
        window_start_seen: true,
    };
    let notice = pause_suppression_notice(&s);
    let rows: Vec<&str> = notice.lines().collect();
    assert!(rows.len() >= 2, "the item row is what this test is about: {notice}");

    let raw = painted(&rows);
    // Preconditions, in e2's style, so a drifting fixture fails loudly rather
    // than passing for a reason this test is not about: unmasked this pane DOES
    // match, and the item row is genuinely rendered rather than scrolled off.
    assert!(
        prompt_wait_detected(&strip_ansi(&raw)),
        "unmasked, the item row matches — that is the trigger #632 is about: {notice}"
    );
    let visible = loomux_lib::orchestration::termgrid::render_visible(&raw, 200, 12);
    assert!(
        visible.contains("w-2 -> orch-1"),
        "and the item row is genuinely RENDERED, so this is the #576 shape one row down: {visible}"
    );

    let pred = question_hold_predicate_sampled(move || sample_from_raw(&raw, 200, 12), None, None, Vec::new());
    assert!(
        !pred(),
        "loomux's own resume notice must not park the pane it is delivered into, item rows \
         included (#632): {notice}"
    );
}

#[test]
fn e10_a_real_dialog_under_a_reframed_notice_block_still_latches() {
    // The safety half of e9, and the one that would break first if #632 were
    // ever "fixed" by widening the mask instead of reframing the rows.
    //
    // Same notice, but a genuine permission dialog is painted directly beneath
    // it — no blank row, the tightest shape. Every row of the notice masks
    // away; not one row of the dialog may go with them. A block-form mask
    // ("from a marker row until the next blank line", "the marker row and its
    // wrap run") swallows this and releases an Enter into a live question,
    // which is #420 reached from pane output — the exact reason #621 scoped the
    // mask to one row per marker and the reason this PR reframes rows rather
    // than widening the claim.
    let s = PauseSuppression {
        items: vec![SuppressedDelivery {
            from: "w-2".into(),
            to: "orch-1".into(),
            preview: "report: done, PR #123 is green".into(),
            cause: SuppressedCause::LegacyDiscard,
        }],
        window_start_seen: true,
    };
    let notice = pause_suppression_notice(&s);
    let mut rows: Vec<&str> = notice.lines().collect();
    rows.extend([
        "◆ Allow Copilot to run the following command?",
        "",
        "  $ rm -rf build",
        "",
        "│ ❯ Yes",
        "│   No, and tell Copilot what to do differently",
        "",
        "Use arrow keys · Enter to confirm · Esc to cancel",
    ]);

    let raw = painted(&rows);
    let pred = question_hold_predicate_sampled(move || sample_from_raw(&raw, 200, 20), None, None, Vec::new());
    assert!(
        pred(),
        "a live dialog sharing a pane with a fully-masked notice block must STILL latch — \
         reframing rows must never become a way to hide the rows around them (#621/#632)"
    );
}

#[test]
fn e11_a_coalesced_flush_payload_still_latches_the_gate_by_design() {
    // The documented CONSERVATIVE residual, pinned as deliberate rather than
    // left to be discovered as a bug.
    //
    // A coalesced flush's framing masks away (see
    // `every_framing_row_of_a_coalesced_flush_is_maskable_but_the_payload_is_left_alone`),
    // but each constituent's text rides verbatim and is NOT masked. So a
    // delivery whose own body is question-shaped still parks the pane — exactly
    // as it would have done pasted on its own, pre-#533, with no banner
    // anywhere near it. That equivalence is the argument: masking it would give
    // the coalesced path a blindness no other delivery path has, and the only
    // rule that could reach it is content- or position-based, which is
    // forgeable from pane output.
    //
    // Under-masking is the safe error (a hold that clears late, escalated by
    // `QuestionStale` at ten minutes); over-masking releases an Enter into a
    // live dialog. This asserts we are on the safe side.
    let items = [queue::FlushConstituent {
        id: 1,
        from: "w-7",
        enqueued_ms: 0,
        coalesced: 0,
        text: "Please check the build.\nDo you want to run npm test? (y/n)",
    }];
    let flush = queue::coalesced_flush_text(&items, 0, 1_000, queue::FlushCause::PaneBlocked);
    let rows: Vec<&str> = flush.lines().collect();
    let raw = painted(&rows);
    let pred = question_hold_predicate_sampled(move || sample_from_raw(&raw, 200, 14), None, None, Vec::new());
    assert!(
        pred(),
        "a constituent payload is agent text and keeps its tokens — the documented \
         conservative residual of #632, not a miss: {flush}"
    );
}

/// The two rows `e6` has always painted: one notice, wrapped, with the
/// detector's tokens landing on the row the marker does NOT lead.
const E6_WRAP_ROW_1: &str = "[orrerix] w-119 reports blocked: Copilot is asking";
const E6_WRAP_ROW_2: &str = "\"Do you want to run npm test? (y/n)\" and I cannot answer it";

#[test]
fn e6_a_wrapped_notice_no_longer_latches_once_the_record_says_we_wrote_it() {
    // #576's WRAP RESIDUAL, and a deliberately FLIPPED expectation: this test
    // used to pin the latch as a known limit ("a wrapped notice's later rows
    // are unmarked and still latch"). Same two rows, opposite answer, because
    // the thing that was missing now exists.
    //
    // What changed is not the mask's appetite but its evidence. The old rule
    // could only ask "does this row LOOK like a notice", which one row can
    // answer and the next row cannot — and widening it on the marker alone was
    // rejected (e3/e4: an agent can print a marker row, and a run-mask would
    // let it hide a live dialog). The record answers a different question —
    // "did loomux WRITE this text here" — which a pane cannot forge, and the
    // continuation is claimed only because it reconstructs, verbatim and to the
    // end, a line the record holds.
    let raw = painted(&[E6_WRAP_ROW_1, E6_WRAP_ROW_2]);

    // Precondition: unmasked, this pane matches — the trigger is real and the
    // fixture has not drifted into something the detector ignores.
    assert!(
        prompt_wait_detected(&strip_ansi(&raw)),
        "unmasked, the wrapped row matches — that is the residual #576 is about"
    );

    // The degradation, pinned in the same test rather than assumed: with no
    // record (a pane loomux never wrote to, or one whose record a restart
    // dropped) the answer is the pre-#576 one. Losing the record costs a hold
    // that clears late, never a release into a live question.
    let no_record = painted(&[E6_WRAP_ROW_1, E6_WRAP_ROW_2]);
    let without =
        question_hold_predicate_sampled(move || sample_from_raw(&no_record, 120, 10), None, None, Vec::new());
    assert!(
        without(),
        "without the record the marker rule is all there is, and it still latches — the \
         fail-CLOSED degradation this change is allowed to fall back to (#576)"
    );

    // The line the two rows above are the wrap of — what `deliver_now` recorded
    // when it pasted this notice.
    let delivered = vec![format!("{E6_WRAP_ROW_1} {E6_WRAP_ROW_2}")];
    let pred =
        question_hold_predicate_sampled(move || sample_from_raw(&raw, 120, 10), None, None, delivered);
    assert!(
        !pred(),
        "a notice that wrapped is still loomux's own writing on the row it wrapped onto — \
         with the delivery record, the gate must release (#576)"
    );
}

/// Split one logical line into rows the way a terminal word-wraps it, so a
/// wrap fixture cannot silently drift from the line it claims to be a wrap of
/// — the point of `e6b` is that the rows and the recorded line are the same
/// text, and hand-typing both is exactly how that stops being true.
fn wrapped_rows(line: &str, cols: usize) -> Vec<String> {
    let mut rows: Vec<String> = vec![String::new()];
    for word in line.split(' ') {
        let cur = rows.last_mut().expect("seeded with one row");
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.chars().count() + 1 + word.chars().count() <= cols {
            cur.push(' ');
            cur.push_str(word);
        } else {
            rows.push(word.to_string());
        }
    }
    rows
}

#[test]
fn e6b_a_real_relay_notice_wrapped_across_three_rows_masks_whole() {
    // e6 at a width that wraps the REAL relayed line (`LOOMUX_REPORT_RELAY`,
    // the exact shape `mcp.rs` composes) onto more than two rows, with the rows
    // derived from the line rather than typed beside it. Three rows matter: the
    // claim walks a run, so a two-row fixture cannot tell "continues the line"
    // from "is the second half of it".
    let line = LOOMUX_REPORT_RELAY;
    let rows = wrapped_rows(line, 46);
    assert!(rows.len() >= 3, "fixture must actually wrap more than once: {rows:?}");
    let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
    let raw = painted(&refs);
    assert!(
        prompt_wait_detected(&strip_ansi(&raw)),
        "precondition: unmasked, the wrapped relay matches"
    );

    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&raw, 120, 12),
        None,
        None,
        vec![line.to_string()],
    );
    assert!(!pred(), "every row of a wrapped notice is loomux's own writing (#576)");
}

#[test]
fn e12_a_notice_whose_marker_row_scrolled_off_still_masks_from_the_top() {
    // The second under-mask #576 names: the marker row has scrolled off the top
    // of the reading, so the rows that survive are a headless middle of a
    // notice — unmarked, and carrying the tokens.
    //
    // Every reading here is truncated at the TOP (the ring keeps the last
    // bytes, the grid the last rows), so the first non-empty row is the one
    // place a line can legitimately appear headless — and the only place a
    // mid-line anchor is allowed. `e14` pins that it is not allowed anywhere
    // else.
    let line = LOOMUX_REPORT_RELAY;
    let rows = wrapped_rows(line, 46);
    assert!(rows.len() >= 3, "fixture must wrap: {rows:?}");
    // Drop the marker row: what is left is exactly what a pane shows once the
    // notice has scrolled halfway off.
    let headless: Vec<&str> = rows[1..].iter().map(String::as_str).collect();
    let raw = painted(&headless);
    assert!(
        prompt_wait_detected(&strip_ansi(&raw)),
        "precondition: the headless continuation still matches — that is the residual"
    );
    assert!(
        !mask_loomux_notices(&strip_ansi(&raw)).trim().is_empty(),
        "precondition: the marker rule alone claims none of these rows — there is no marker \
         left on screen to claim them by"
    );

    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&raw, 120, 12),
        None,
        None,
        vec![line.to_string()],
    );
    assert!(
        !pred(),
        "a notice whose marker row scrolled off is still a notice — the record says what the \
         screen no longer can (#576)"
    );
}

#[test]
fn e13_a_dialog_under_a_recorded_marker_row_still_latches() {
    // e4's safety case with the record POPULATED, which is the version that
    // matters now: the marker row is genuine (loomux really did deliver it) and
    // a live permission dialog is painted directly beneath, no blank row.
    //
    // The recorded line is consumed WHOLE by the marker row itself — it never
    // wrapped — so there is no continuation to claim and the run stops there.
    // A mask that widened on "this row is in the record" rather than on "these
    // rows reconstruct the recorded line to its end" would swallow the dialog
    // and release an Enter into it: the #420 harm.
    let notice = "[orrerix] w-3 reports done: PR #900 is green";
    let raw = painted(&[notice, "Do you want to run npm test? (y/n)"]);
    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&raw, 120, 10),
        None,
        None,
        vec![notice.to_string()],
    );
    assert!(
        pred(),
        "the record ends where the notice ends — the dialog row beneath a REAL notice must \
         survive exactly as it does beneath a quoted one (#576/#420)"
    );
}

#[test]
fn e14_an_agent_printed_marker_row_widens_nothing() {
    // The #420 harm restated as the rule the record-aware mask obeys: masking
    // keys off the RECORD, never off the marker, so a row an agent printed
    // itself — one loomux never wrote — claims exactly the one row the old
    // marker rule always claimed and not a character more.
    //
    // The pane here is the adversarial shape: a forged marker row that LOOKS
    // like the head of a wrapped notice, with the live question on the row that
    // would be its continuation. The record holds a different notice entirely
    // (loomux did write something to this pane — that is what makes the test
    // about the record's CONTENT rather than about it being empty).
    let raw = painted(&[
        "[orrerix] w-119 reports blocked: Copilot is asking",
        "Do you want to run npm test? (y/n)",
    ]);
    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&raw, 120, 10),
        None,
        None,
        vec!["[orrerix] w-3 reports done: PR #900 is green".to_string()],
    );
    assert!(
        pred(),
        "a marker row loomux never wrote must widen nothing — the mask claims a continuation \
         only when it reconstructs a line the record actually holds (#576/#420)"
    );
}

#[test]
fn e14b_a_notice_carrying_the_panes_own_agents_words_is_never_recordable() {
    // rev-163 B1, at the door that closes it. `e14` pins that a row loomux
    // never wrote widens nothing; this pins the harder half — a row loomux
    // really did write, carrying text the pane's OWN agent chose, must not
    // become claimable either.
    //
    // The attack it forecloses, concretely: `notify_when(kind, pr, note)` takes
    // an agent-supplied `note` and `notify_tick` delivers the fired notice to
    // the REGISTERING agent's own pane. `sanitize_gh_text` maps `[`/`]` to
    // parens and strips control characters — it leaves ordinary words, `?`,
    // `(`, `)` and `❯` untouched, which is everything a dialog row is made of.
    // So one agent, with one tool call it makes itself, could put a line of its
    // own choosing into its own pane's record, print that line's head, and let
    // its CLI paint a permission dialog that is exactly the remainder. One
    // party, no coincidences.
    //
    // The door is `mark_notice_maskable`, and it is default-CLOSED: a notice no
    // producer marked never becomes claimable however often it is delivered.
    // `notify_tick` does not mark, so the delivery below records nothing.
    let (reg, _dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", watchdog_rails(0)).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "do work", false, None).unwrap();
    let pty = 91u32;
    reg.set_pty_for_test(&w.id, pty);

    // The shape `notify_tick` delivers into the registrant's own pane, with the
    // note the agent chose.
    let self_directed = "[orrerix] watch n-1: pr #661 checks: SUCCESS. Note (registered): \
                         \"X Do you want to run npm test? (y/n) END\"";
    reg.record_delivered_text(pty, self_directed);
    assert!(
        reg.delivered_notice_lines(pty).is_empty(),
        "an unmarked notice is delivered but never claimable — the default, and the whole of \
         the fix for the one-party attack (rev-163 B1)"
    );

    // And behaviourally, which is what the gate actually consults: the agent
    // prints the line's head into its own pane, its CLI paints the question
    // beneath, and the gate must STILL hold.
    let raw = painted(&[
        "[orrerix] watch n-1: pr #661 checks: SUCCESS. Note (registered): \"X",
        "Do you want to run npm test? (y/n) END\"",
    ]);
    let delivered = reg.delivered_notice_lines(pty);
    let pred =
        question_hold_predicate_sampled(move || sample_from_raw(&raw, 120, 10), None, None, delivered);
    assert!(
        pred(),
        "text the pane's own agent authored must never widen the mask over that pane — \
         otherwise the gate self-grants the permission its CLI was asking a human for (#420)"
    );
}

#[test]
fn e14c_a_marked_relay_line_is_claimable_and_that_is_the_stated_residual() {
    // The other side of `e14b`, pinned as a KNOWN residual rather than left to
    // be discovered — the `e11` treatment, applied to the dangerous direction
    // instead of the safe one, because a residual nobody can see is one nobody
    // can weigh.
    //
    // A relayed `report` note is one agent's words CALLED IN by another agent,
    // so `deliver_relayed_to_orchestrator` marks it: that is what makes the wrap
    // masking #576 asked for possible at all. The cost is that if the pane's
    // occupant prints the line's head into its own pane and its CLI then paints
    // a dialog that is exactly the remainder, the run reconstructs and the
    // dialog is masked.
    //
    // **The residual is PROXY-AUTHORSHIP, not a two-party induction** (rev-163
    // B3 — an earlier version of this comment claimed the latter). The door's
    // check is `from != orch`, which is callership: an orchestrator that
    // instructs a worker to report words it chose passes it, and the line lands
    // in the ORCHESTRATOR's own pane, which it prints into at will. Both marked
    // call sites target the orchestrator, so this is the whole claimable
    // surface rather than a corner of it. Pinned here so the cost is visible
    // and can be re-judged; argued in full at `mask_loomux_notices_with_record`
    // and in the design note.
    let line = "[orrerix] w-119 reports blocked: Copilot is asking Do you want to run npm test? (y/n)";
    let raw = painted(&[
        "[orrerix] w-119 reports blocked: Copilot is asking",
        "Do you want to run npm test? (y/n)",
    ]);
    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&raw, 120, 10),
        None,
        None,
        vec![line.to_string()],
    );
    assert!(
        !pred(),
        "documented residual: a MARKED line whose remainder a dialog reproduces exactly is \
         claimed — the proxy-authorship surface the human accepts knowingly (#576/#420)"
    );
}

#[test]
fn e20_a_real_report_relay_opts_its_notice_into_the_record() {
    // The opt-in wired to a REAL producer, through the actual MCP `report`
    // path — not `mark_notice_maskable` called by hand. Without this, the door
    // could be perfectly built and connected to nothing, and every mask test
    // above would still pass on hand-built records.
    //
    // This is also the #576 motivating case end to end: a worker's note, which
    // is the worker's own words, landing in the ORCHESTRATOR's pane. Cross-pane
    // authorship is exactly what `deliver_relayed_to_orchestrator` checks
    // before marking.
    let (reg, _d, co, cw) = setup_mcp();
    let orch_pty = 501u32;
    // A pane for the orchestrator (the relay's target) plus a pause, which is
    // how this suite observes a delivery's TEXT with no terminal anywhere.
    pause_with_pane(&reg, &cw.group, &co.agent_id, orch_pty);

    let r = dispatch(&reg, &cw, "tools/call", &json!({
        "name": "report",
        "arguments": {
            "outcome": "blocked",
            "note": "Copilot is asking Do you want to run npm test? (y/n) and I cannot answer it",
            "ref": "#661",
        }
    }))
    .unwrap();
    assert_ne!(r["isError"], json!(true), "the report itself must succeed: {r}");

    let relayed = delivered_texts(&reg, &cw.group)
        .into_iter()
        .find(|t| t.contains("reports blocked"))
        .expect("the relay is delivered to the orchestrator's pane");

    // Phase 1 only so far: the producer marked it, but nothing has been written
    // to that pane, so nothing is claimable yet.
    assert!(
        reg.delivered_notice_lines(orch_pty).is_empty(),
        "marked is not written — a record of text nobody painted would claim rows nobody wrote"
    );

    // The write `deliver_now` performs, which is the half a paused group does
    // not do for us.
    reg.record_delivered_text(orch_pty, &relayed);
    assert_eq!(
        reg.delivered_notice_lines(orch_pty),
        vec![relayed.trim().to_string()],
        "a real cross-pane relay is claimable once delivered — the case #576 is about"
    );
}

#[test]
fn e18_the_record_needs_both_a_producers_mark_and_a_write() {
    // The two phases, each pinned as necessary. Marking without a write claims
    // text nobody painted (fail-open); writing without a mark claims text an
    // agent may have chosen (rev-163 B1). Neither alone may produce a claimable
    // line.
    let (reg, _dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", watchdog_rails(0)).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "do work", false, None).unwrap();
    let pty = 77u32;
    reg.set_pty_for_test(&w.id, pty);
    let line = "[orrerix] w-1 reports done: PR #900 is green";

    reg.mark_notice_maskable(&w.id, line);
    assert!(
        reg.delivered_notice_lines(pty).is_empty(),
        "marked but never written: nothing is on that screen yet, so nothing may be claimed"
    );

    reg.record_delivered_text(pty, "[orrerix] some OTHER notice nobody marked");
    assert!(
        reg.delivered_notice_lines(pty).is_empty(),
        "written but never marked: the default, and the one-party attack's door"
    );

    reg.record_delivered_text(pty, &format!("{line}\nplease review it"));
    assert_eq!(
        reg.delivered_notice_lines(pty),
        vec![line.to_string()],
        "marked AND written is the only combination that claims anything — and the delivery's \
         agent-authored body still never enters the record"
    );
}

#[test]
fn e19_the_record_evicts_the_stalest_pane_and_never_the_one_just_written() {
    // rev-163 N3: the per-PANE bound, and the subtlety that keeps it correct —
    // `seq` is max-over-panes + 1, so the pane just written is always the
    // newest and can never be its own eviction victim. A regression here
    // degrades silently to the marker rule, which is exactly the class of
    // failure that is invisible without a test.
    let (reg, _dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", watchdog_rails(0)).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();

    // The record is keyed by PANE; the agent is only how a producer names one.
    // So one agent re-pointed at each pty in turn seeds as many pane records as
    // this needs, without asking the fleet guardrail (`max_agents`) for 66
    // workers it would rightly refuse.
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "do work", false, None).unwrap();
    let notice_for = |i: usize| format!("[orrerix] notice for pane {i}");
    let pty_for = |i: usize| 1000 + i as u32;
    let panes = DELIVERED_NOTICE_PANES + 2;

    for i in 0..panes {
        reg.set_pty_for_test(&w.id, pty_for(i));
        reg.mark_notice_maskable(&w.id, &notice_for(i));
        reg.record_delivered_text(pty_for(i), &notice_for(i));
    }

    assert!(
        reg.delivered_notice_lines(pty_for(0)).is_empty(),
        "the pane that has taken nothing while {DELIVERED_NOTICE_PANES} others did is evicted"
    );
    assert_eq!(
        reg.delivered_notice_lines(pty_for(panes - 1)),
        vec![notice_for(panes - 1)],
        "and the pane just written is never the victim — `seq` is max-over-panes + 1"
    );

    // Re-writing an evicted pane recovers it rather than staying broken.
    reg.set_pty_for_test(&w.id, pty_for(0));
    reg.mark_notice_maskable(&w.id, &notice_for(0));
    reg.record_delivered_text(pty_for(0), &notice_for(0));
    assert_eq!(
        reg.delivered_notice_lines(pty_for(0)),
        vec![notice_for(0)],
        "eviction costs a pane its record until its next delivery, not permanently"
    );
}

#[test]
fn e15_the_record_claims_only_a_run_that_reconstructs_a_delivered_line() {
    // The unit-level statement of what e13/e14 defend behaviourally, and of
    // where a mid-line anchor is allowed.
    let line = "[orrerix] w-1 reports blocked: waiting on the reviewer to answer the question";

    // A run that reconstructs the line to its end is claimed whole.
    let wrapped = "[orrerix] w-1 reports blocked: waiting on\nthe reviewer to answer the question\nplain agent output";
    assert_eq!(
        mask_loomux_notices_with_record(wrapped, &[line.to_string()]).trim(),
        "plain agent output",
        "both rows of the wrap are ours; the row after the run is not"
    );

    // A run that DIVERGES keeps everything from the divergence on — including
    // the marker row's own continuation, which is no longer evidence of
    // anything once it stops matching what we wrote.
    let diverged = "[orrerix] w-1 reports blocked: waiting on\nDo you want to run npm test? (y/n)";
    assert_eq!(
        mask_loomux_notices_with_record(diverged, &[line.to_string()]).trim(),
        "Do you want to run npm test? (y/n)",
        "the marker row still masks (the marker rule), but a row that is not the rest of what \
         we wrote is never taken with it"
    );

    // A run that reconstructs only a PREFIX of the line proves nothing about
    // the rows after it, so it claims none of them.
    let truncated = "[orrerix] w-1 reports blocked: waiting on\nthe reviewer to";
    assert_eq!(
        mask_loomux_notices_with_record(truncated, &[line.to_string()]).trim(),
        "the reviewer to",
        "a run must consume the recorded line to its END — a prefix leaves the rows below \
         unexplained, which is the run-mask #621 rejected"
    );

    // A mid-line anchor is honoured at the first row of the reading (the head
    // scrolled off) and NOWHERE else.
    assert_eq!(
        mask_loomux_notices_with_record("the reviewer to answer the question", &[line.to_string()])
            .trim(),
        "",
        "the top row of a top-truncated reading may anchor mid-line"
    );
    assert_eq!(
        mask_loomux_notices_with_record(
            "plain agent output\nthe reviewer to answer the question",
            &[line.to_string()]
        )
        .trim(),
        "plain agent output\nthe reviewer to answer the question",
        "a headless fragment in the MIDDLE of a reading is not a scrolled-off marker — nothing \
         was truncated there, so the rows above it would have shown the head if we had written it"
    );

    // rev-163 N1: and a mid-line anchor needs real evidence. A short fragment
    // that happens to appear in a recorded line is not proof that the head
    // scrolled off, and without a floor it would carry a claim over everything
    // below it that completes the line — a one-character "proof".
    assert_eq!(
        mask_loomux_notices_with_record("the question", &[line.to_string()]).trim(),
        "the question",
        "a fragment below the anchor floor claims nothing, even though it does complete the \
         recorded line — the scrolled-off case this rule serves always has a full row of it"
    );
}

#[test]
fn e16_the_record_holds_loomux_framing_and_never_agent_payload() {
    // What may enter the record at all, which is the ceiling on everything the
    // record-aware mask can ever claim. Marker-led lines are loomux's own
    // framing; a delivery's agent-authored body is not recorded, so the mask
    // cannot be handed the power to blind the gate to ordinary pane content
    // (the residual `e11` pins as deliberate).
    let items = [queue::FlushConstituent {
        id: 1,
        from: "w-7",
        enqueued_ms: 0,
        coalesced: 0,
        text: "Please check the build.\nDo you want to run npm test? (y/n)",
    }];
    let flush = queue::coalesced_flush_text(&items, 0, 1_000, queue::FlushCause::PaneBlocked);
    let recorded = loomux_authored_lines(&flush);
    assert!(!recorded.is_empty(), "the flush's own framing is loomux's writing: {flush}");
    assert!(
        !recorded.iter().any(|l| l.contains("Do you want to run npm test?")),
        "a constituent's payload is agent text and must never enter the record: {recorded:?}"
    );

    // And behaviourally: the same flush, painted, with its own framing recorded
    // — the payload still parks the pane exactly as `e11` pins it does.
    let rows: Vec<&str> = flush.lines().collect();
    let raw = painted(&rows);
    let pred =
        question_hold_predicate_sampled(move || sample_from_raw(&raw, 200, 14), None, None, recorded);
    assert!(
        pred(),
        "recording a flush's framing must not start masking the payload it frames (#576/#632)"
    );
}

#[test]
fn e17_the_registry_records_what_it_delivered_bounded_and_deduped() {
    // The record as the delivery path actually fills it, through the registry
    // seams a producer and `deliver_now` call — not a hand-built `Vec`.
    let (reg, _dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", watchdog_rails(0)).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "do work", false, None).unwrap();
    let pty = 7u32;
    reg.set_pty_for_test(&w.id, pty);
    assert!(
        reg.delivered_notice_lines(pty).is_empty(),
        "a pane loomux has never written to knows nothing — which is the marker rule"
    );
    // Both phases for every line: marked by a producer, then written. `e18`
    // pins that each is necessary; this test is about what survives the pair.
    let deliver = |text: &str| {
        reg.mark_notice_maskable(&w.id, text);
        reg.record_delivered_text(pty, text);
    };

    // A delivery is framing plus body: only the framing is kept.
    deliver("[orrerix] w-1 reports done: PR #900 is green\nplease review it");
    assert_eq!(
        reg.delivered_notice_lines(pty),
        vec!["[orrerix] w-1 reports done: PR #900 is green".to_string()],
        "marker-led lines only"
    );

    // `deliver_now`'s echo-verified typing loop can write the same payload more
    // than once; a retype must not evict the pane's earlier notices.
    deliver("[orrerix] w-1 reports done: PR #900 is green\nplease review it");
    assert_eq!(reg.delivered_notice_lines(pty).len(), 1, "a repeat of the newest line is dropped");

    // Bounded, drop-oldest.
    for i in 0..DELIVERED_NOTICES_PER_PANE + 5 {
        deliver(&format!("[orrerix] notice number {i}"));
    }
    let lines = reg.delivered_notice_lines(pty);
    assert_eq!(lines.len(), DELIVERED_NOTICES_PER_PANE, "the record is capped per pane");
    assert!(
        lines.last().is_some_and(|l| l.ends_with(&format!("{}", DELIVERED_NOTICES_PER_PANE + 4))),
        "the newest line is kept — it is the one still on screen: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("PR #900")),
        "and the oldest are dropped: {lines:?}"
    );

    // Panes do not share a record.
    assert!(
        reg.delivered_notice_lines(8).is_empty(),
        "the record is per pane — another pane's notices are not evidence about this one"
    );
}

#[test]
fn e5_mask_loomux_notices_drops_exactly_the_marker_led_rows() {
    // The unit-level statement of the scope decision e3/e4 defend
    // behaviourally: LED BY the marker, not merely containing it, and one row
    // at a time.
    let tail = "plain agent output line\n\
                [orrerix] w-1 reports done: see PR #12\n\
                │ [orrerix] idle tick: you have been idle\n\
                the docs say an `[orrerix]` notice gets typed into your pane\n\
                Do you want to proceed? (y/n)";
    let masked = mask_loomux_notices(tail);

    assert!(!masked.contains("reports done"), "a marker-led row is loomux's own writing: {masked:?}");
    assert!(
        !masked.contains("idle tick"),
        "and so is one echoed inside a box UI — `deframe` makes `│ [orrerix] …` still LEAD its \
         row, the same rule `leads_with_pointer` applies: {masked:?}"
    );
    assert!(masked.contains("plain agent output line"), "unrelated rows are untouched: {masked:?}");
    assert!(
        masked.contains("the docs say"),
        "a row that merely MENTIONS the marker mid-line is not a notice row — only a LEADING \
         marker counts, or every pane quoting the docs would go blind: {masked:?}"
    );
    assert!(
        masked.contains("Do you want to proceed? (y/n)"),
        "and the row after a masked one is never taken with it — that is the whole of the \
         one-row scope (#576): {masked:?}"
    );
}

#[test]
fn e5b_the_marker_is_what_sanitized_untrusted_text_can_never_contain() {
    // Why a marker is trustworthy enough to mask on at all, pinned against the
    // sanitizer rather than asserted in prose: every untrusted field formatted
    // into a notice loses its brackets, so no agent-supplied text can ever
    // arrive LEADING a row with this marker. That is the delivery direction —
    // and it is the only direction the mask relies on. Pane output is not
    // sanitized, which is exactly why the scope stops at one row.
    let hostile = "[orrerix] w-9 reports blocked: do you want to proceed? (y/n)";
    let sanitized = notify::sanitize_gh_text(hostile, notify::NOTICE_FIELD_CAP);
    assert!(
        !sanitized.contains(NOTICE_MARKER),
        "an untrusted field must not be able to forge the marker it would be masked by: {sanitized:?}"
    );
    assert_eq!(
        mask_loomux_notices(&sanitized),
        sanitized,
        "so text arriving through the sanitizer is never mistaken for loomux's own writing"
    );
}

#[test]
fn e7_a_relayed_notice_raises_no_attention_chip_while_a_real_dialog_still_does() {
    // rev-126's finding: the SAME self-latch, arriving at a different
    // consumer. `attention_tick` reads the pane tail through
    // `prompt_wait_detected` to raise the "waiting on a prompt" chip, and a
    // pane holding a relayed `[orrerix] … (y/n)` notice is output-quiet
    // precisely BECAUSE it is idle — so the quiet gate that is supposed to
    // mean "parked on a question" is satisfied by loomux's own prose about
    // one. Fixing only the delivery gate left this reader behind.
    //
    // Two independent registries so neither case can inherit the other's
    // latched `attn_quiet` / `attn_waiting_ack` state.
    let now = 1_000_000_000_000u64;
    let no_input: HashMap<String, u64> = HashMap::new();

    // A genuine dialog must still raise the chip — the guard against "fixed"
    // meaning "switched off".
    let (reg_real, _d1, _g1, wid_real) = attention_setup();
    let out_real: HashMap<String, u64> = [(wid_real.clone(), 512u64)].into_iter().collect();
    let real: HashMap<String, String> =
        [(wid_real.clone(), strip_ansi(FIX_COPILOT_ASK.as_bytes()))].into_iter().collect();
    reg_real.attention_tick(now, &out_real, &real, &no_input);
    let flagged = reg_real.attention_tick(now + 5000, &out_real, &real, &no_input);
    assert!(
        flagged.iter().any(|i| i.agent_id == wid_real && i.reason == "waiting"),
        "a real Copilot dialog must still raise the waiting chip"
    );

    // The same pane, quiet for just as long, holding only loomux's own
    // relayed notice: no chip.
    let (reg_notice, _d2, _g2, wid_notice) = attention_setup();
    let out_notice: HashMap<String, u64> = [(wid_notice.clone(), 512u64)].into_iter().collect();
    let notice: HashMap<String, String> =
        [(wid_notice.clone(), format!("{LOOMUX_REPORT_RELAY}\n  ? for shortcuts"))]
            .into_iter()
            .collect();
    reg_notice.attention_tick(now, &out_notice, &notice, &no_input);
    let quiet = reg_notice.attention_tick(now + 5000, &out_notice, &notice, &no_input);
    assert!(
        quiet.iter().all(|i| !(i.agent_id == wid_notice && i.reason == "waiting")),
        "loomux's own relayed notice must not raise a `waiting` chip — a wrong chip trains \
         the human to ignore chips, and unlike the gate's latch nothing reports it (#576): {quiet:?}"
    );
}

#[test]
fn e8_a_plain_pane_holding_a_relayed_notice_raises_no_chip_either() {
    // `plain_pane_attention` is the second unmasked reader rev-126 found. A
    // plain pane is never delivered to, so it takes a human pasting a notice
    // in to read it — but the two readings must not disagree about what counts
    // as a question, and a shell pane wrongly flagged as "waiting on your
    // input" is the same false chip.
    let now = 1_000_000_000_000u64;
    let no_input: HashMap<u32, u64> = HashMap::new();
    let no_agents: HashSet<u32> = HashSet::new();
    let out: HashMap<u32, u64> = [(7u32, 256u64)].into_iter().collect();

    let (reg_real, _d1, _g1, _w1) = attention_setup();
    let real: HashMap<u32, String> =
        [(7u32, strip_ansi(FIX_COPILOT_ASK.as_bytes()))].into_iter().collect();
    reg_real.plain_pane_attention(now, &out, &real, &no_input, &no_agents);
    let flagged = reg_real.plain_pane_attention(now + 5000, &out, &real, &no_input, &no_agents);
    assert!(
        flagged.iter().any(|i| i.pty_id == Some(7) && i.reason == "waiting"),
        "a real dialog on a plain pane must still raise the chip"
    );

    let (reg_notice, _d2, _g2, _w2) = attention_setup();
    let notice: HashMap<u32, String> =
        [(7u32, format!("{LOOMUX_REPORT_RELAY}\n$ "))].into_iter().collect();
    reg_notice.plain_pane_attention(now, &out, &notice, &no_input, &no_agents);
    let quiet = reg_notice.plain_pane_attention(now + 5000, &out, &notice, &no_input, &no_agents);
    assert!(
        quiet.is_empty(),
        "a plain pane showing only loomux's own notice is not waiting on anything: {quiet:?}"
    );
}

// ---------- #727: an empty prompt glyph is not a menu pointer ----------
//
// The live incident: a pane resumed from a previous session sat at a visibly
// idle, empty input box, and the question gate held every delivery to it for
// 25 minutes — twice, deterministically, on the same session. Three panes were
// lost to it and two queued deliveries were dropped with them.
//
// The phantom question was the CLI's own prompt. Claude Code paints an empty
// input box as a bare `❯` on its own row, and `leads_with_pointer` read that as
// a menu's highlighted choice. That poisons BOTH readings at once, which is why
// nothing could clear it: the ring matched (the glyph is in the last painted
// lines, and `strip_ansi` concatenates it onto whatever was addressed next), and
// `pointer_rendered` then found the same bare glyph among the rendered rows, so
// the grid answered `StillRendered` and #534's release — the only thing that
// ends a hold without a human — could never fire. A resumed pane makes it
// permanent: its restored screen is static, so the glyph is never repainted
// away.

#[test]
fn f1_the_clis_own_empty_prompt_glyph_is_not_a_menu_pointer() {
    let tail = strip_ansi(FIX_FP_RESUMED_IDLE.as_bytes());

    // Precondition, and the reason this fixture is not a duplicate of the other
    // `❯` negatives: those are safe because the CLI's redrawn box pushes the
    // glyph out of the last-3-painted-lines window. This glyph IS that box, so
    // it is inside the window by construction and nothing can push it out.
    let last_painted: Vec<String> = tail
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .rev()
        .take(3)
        .collect();
    assert!(
        last_painted.iter().any(|l| l == "❯"),
        "precondition: the bare prompt glyph must be inside the detector's own pointer \
         window, or this test would pass for the reason the other fixtures pass: {last_painted:?}"
    );

    assert!(
        prompt_wait_match(&tail).is_none(),
        "a pointer with nothing after it points at no option — it is an empty prompt, \
         and an idle pane is the opposite of a pane parked on a question: {:?}",
        prompt_wait_match(&tail)
    );
}

#[test]
fn f2_a_resumed_panes_restored_screen_does_not_hold_a_delivery() {
    // The repro, end to end at the gate that queued the delivery: both
    // readings of the restored screen, through the production predicate.
    let raw = FIX_FP_RESUMED_IDLE.as_bytes().to_vec();

    // The grid must be worth reading, or a release here would be the blind-start
    // hole (`Unreadable` -> the ring's word stands) rather than the fix.
    let visible = trustworthy_composition(loomux_lib::orchestration::termgrid::render_visible(
        &raw, 100, 12,
    ))
    .expect("precondition: the restored screen composes to a readable grid");
    assert!(
        visible.contains("auto mode on"),
        "precondition: the CLI's chrome is genuinely rendered — this is a live screen, \
         not an absent one: {visible:?}"
    );

    let pred =
        question_hold_predicate_sampled(move || sample_from_raw(&raw, 100, 12), None, None, Vec::new());
    assert!(
        !pred(),
        "a pane idling at an empty input box must take its delivery immediately (#727)"
    );
}

#[test]
fn f3_a_pointer_leading_a_real_option_still_matches_and_still_holds() {
    // The fail-SAFE direction, which this narrowing must not touch: a pointer
    // that leads an actual choice is still a menu, on both readings.
    let m = prompt_wait_match(&strip_ansi(FIX_POS_PTR_LAST.as_bytes()))
        .expect("a highlighted menu choice is still a question");
    assert_eq!(m.signal, "pointer-option", "and still THIS signal class");

    for screen in [
        "❯ Overwrite\n  Keep both",
        // Framed on BOTH sides — the boxed dialog `deframe` exists for. The
        // trailing border must not be mistaken for "points at nothing".
        "│ ❯ Overwrite   │\n│   Keep both   │",
        // A different glyph, and one sitting far above the chrome: the grid
        // re-read is spatial (C11), and stays so.
        "→ 1. retry\n  2. abort\nstatus\nstatus\n> \nready",
    ] {
        assert!(
            match_still_rendered(screen, &m),
            "a pointer leading an option is still the menu, framed or not: {screen:?}"
        );
    }

    // ...and the empty prompt is the ONLY thing that stops being one.
    for empty in ["❯", "  ❯  ", "│ ❯   │", "›"] {
        assert!(
            !match_still_rendered(empty, &m),
            "a glyph with only framing after it is a prompt, not a choice: {empty:?}"
        );
    }

    // End to end on a fully composed screen, so the hold is the production
    // decision and not just the predicate's opinion about a string.
    let live = painted(&[
        "Overwrite the existing file?",
        "❯ Overwrite",
        "  Keep both",
        "",
        "esc to cancel",
    ]);
    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&live, 100, 10),
        None,
        None,
        Vec::new(),
    );
    assert!(pred(), "a live menu must still hold the delivery (#420)");
}

#[test]
fn f4_a_pointer_hold_releases_once_the_menu_gives_way_to_an_idle_prompt() {
    // The latch itself, at the reading that is supposed to break it.
    //
    // The ring matched a genuine pointer menu and, being append-only, goes on
    // matching it forever; #534 gave the grid the power to end that hold when
    // the screen no longer shows it. An idle Claude Code pane's own `❯` made
    // the grid answer `StillRendered` on EVERY screen, so for this signal class
    // that release was dead code — which is what turned a transient hold into a
    // 25-minute one.
    let m = prompt_wait_match(&redraw_fragmented_pointer_tail()).expect("matches on the ring");
    assert_eq!(m.needle, QuestionNeedle::LeadingPointer, "the class this is about");

    let idle = loomux_lib::orchestration::termgrid::render_visible(
        FIX_FP_RESUMED_IDLE.as_bytes(),
        100,
        12,
    );
    assert!(
        idle.contains('❯'),
        "precondition: the prompt glyph IS on the screen — the fix is that it is not a \
         menu, never that it stopped being rendered: {idle:?}"
    );
    assert!(
        !flat_contains_line(&idle, &m.line),
        "precondition: the concatenated ring needle is not on this screen either"
    );

    assert_eq!(
        grid_evidence_for(&m, Some(Composed::plain(&idle))),
        // #903 refines the answer on this exact screen: the menu is not among
        // the rendered rows AND the pane is visibly idle at an empty box, which
        // is the more specific of the two release readings. #727's property —
        // an empty prompt is not a stand-in for a menu — is the assertion below.
        GridEvidence::IdlePrompt,
        "the menu is not among the rendered rows, and an empty prompt is not a stand-in for it"
    );
    assert!(
        !question_shown(Some(&m), Some(Composed::plain(&idle))),
        "the dialog cleared — the hold must end without a human (#534's one transition, \
         which #727 had made unreachable)"
    );
}

// ---------- #820: a pointer glyph leading OUR OWN text is not a menu either ----------
//
// #727 narrowed `leads_with_pointer` so a pointer pointing at NOTHING — a CLI's
// empty input box — stops reading as a menu. #820 is the same latch, through
// the same signal, with the box FULL. Copilot 1.0.7x paints its chevron
// composer's input row as `❯ <text>`, so from the instant loomux pastes a
// delivery into that box, loomux's own prompt LEADS a line with a
// `POINTER_GLYPHS` member; and where the framed composer is used instead, the
// `┃ ` border de-frames away and a wrap boundary drops one of the brief's own
// arrows at the start of a row. Either way the pane reads as parked on a menu
// while the only thing on screen is what loomux itself just wrote.
//
// The self-echo exclusion was supposed to make that a non-event and did not,
// because it compared whole rendered lines to whole pasted lines for EQUALITY —
// and a CLI does not render our paste as our lines. It frames it, wraps it, and
// prefixes it.
//
// The latch that follows is #727's verbatim: nothing repaints an unsubmitted
// box, so the ring stays frozen on the row, `pointer_rendered` finds the same
// glyph among the rendered rows, and `GridEvidence::NotRendered` — the one
// reading that ends a hold without a human — is unreachable.

#[test]
fn g1_copilots_own_composer_holding_our_paste_is_not_a_menu_pointer() {
    let tail = strip_ansi(FIX_COPILOT_COMPOSER_PASTE.as_bytes());

    // Precondition, and the same one #727's f1 asserts for the same reason: a
    // composer row outside the detector's own last-3-painted-lines window would
    // make this pass on WINDOWING rather than on the exclusion it is about.
    let last_painted: Vec<String> = tail
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .rev()
        .take(3)
        .collect();
    assert!(
        last_painted.iter().any(|l| l.starts_with('❯')),
        "precondition: the composer's `❯ ` input row must sit inside the pointer window, or \
         this test proves nothing: {last_painted:?}"
    );
    // ...and the row must genuinely fire the signal without the exclusion, or
    // the assertion below would be celebrating a signal that never fired.
    let unmasked = prompt_wait_match(&tail).expect("the raw screen IS pointer-shaped");
    assert_eq!(unmasked.signal, "pointer-option", "and by THIS signal class");

    let masked = mask_own_paste(&tail, COMPOSER_PASTE_TEXT);
    assert!(
        prompt_wait_match(&masked).is_none(),
        "a pointer leading text loomux itself just pasted is loomux's own prompt, not a menu's \
         highlighted choice (#820): {:?}",
        prompt_wait_match(&masked)
    );
}

#[test]
fn g2_a_delivery_is_not_held_by_its_own_text_sitting_in_the_copilot_composer() {
    // The repro end to end, at the checkpoint that actually held it: both
    // readings of the live screen, through the production predicate, with the
    // `pasted_text` a pre-Enter checkpoint really does have.
    let raw = FIX_COPILOT_COMPOSER_PASTE.as_bytes().to_vec();

    // The grid must be worth reading, or a release here would be the
    // blind-start hole (`Unreadable` → the ring's word stands) rather than the
    // fix — #727's f2 makes the same check for the same reason.
    let visible = trustworthy_composition(loomux_lib::orchestration::termgrid::render_visible(
        &raw, 100, 10,
    ))
    .expect("precondition: the composer screen composes to a readable grid");
    assert!(
        visible.contains("@ files"),
        "precondition: the CLI's own chrome is genuinely rendered — a live screen, not an \
         absent one: {visible:?}"
    );

    let pred = question_hold_predicate_sampled(
        move || sample_from_raw(&raw, 100, 10),
        Some(COMPOSER_PASTE_TEXT.to_string()),
        None,
        Vec::new(),
    );
    assert!(
        !pred(),
        "a delivery must not be held behind the delivery's own text (#820) — and this is the \
         hold that never ends, because an unsubmitted box repaints nothing"
    );
}

#[test]
fn g3_a_wrapped_paste_row_that_opens_with_an_arrow_is_still_our_own_text() {
    // The framed composer, where no prompt chevron is involved at ALL: the
    // `┃ ` border de-frames away and the WRAP is what does the damage, dropping
    // one of the brief's own `→`s at the start of a row. This is why the fix is
    // wrap reconstruction and not merely a chevron strip — a mask that only
    // stripped the glyph would still miss every framed pane.
    let tail = strip_ansi(FIX_COPILOT_FRAMED_WRAP.as_bytes());
    let unmasked = prompt_wait_match(&tail).expect("the raw screen IS pointer-shaped");
    assert_eq!(unmasked.signal, "pointer-option");
    assert!(
        unmasked.line.contains("then implement it"),
        "precondition: the row that fired is the WRAPPED continuation of our own brief, not \
         its first row: {:?}",
        unmasked.line
    );

    let masked = mask_own_paste(&tail, FRAMED_PASTE_TEXT);
    assert!(
        prompt_wait_match(&masked).is_none(),
        "one pasted line wrapped across rows is still one pasted line (#820): {:?}",
        prompt_wait_match(&masked)
    );
}

#[test]
fn g4_a_short_pasted_line_never_claims_a_menu_option_row() {
    // The fail-SAFE direction, and the whole reason `SELF_ECHO_MIN_POINTER_CHARS`
    // exists. `❯ Overwrite` is our own composer holding a one-word paste, and it
    // is also a live dialog's highlighted choice — byte for byte the same row.
    // Claiming it on that evidence would mask a genuine dialog into "no
    // question" and release an Enter into it, which is the #420 harm.
    let tail = strip_ansi(FIX_POS_PTR_LAST.as_bytes());
    let masked = mask_own_paste(&tail, "Overwrite\nKeep both");
    let m = prompt_wait_match(&masked)
        .expect("a real dialog whose options are short must still be a question (#420)");
    assert_eq!(m.needle, QuestionNeedle::LeadingPointer, "and still by the pointer");

    // ...and the floor is about the EVIDENCE, not about pointers as such: the
    // same shape, with a line long enough to be recognisably ours, IS claimed.
    // Both halves are pinned, because a floor set to zero and a floor set to
    // infinity are each a different bug.
    let long = "Overwrite the existing file and keep the previous one as a backup";
    let long_tail = format!("? Are you sure?\n❯ {long}\n  Cancel");
    let long_masked = mask_own_paste(&long_tail, long);
    assert!(
        prompt_wait_match(&long_masked).is_none(),
        "a pointer leading a line long enough to be recognisably ours is our own echo: {:?}",
        prompt_wait_match(&long_masked)
    );
}

#[test]
fn g5_every_captured_real_dialog_still_holds_with_a_delivery_on_screen() {
    // #727's suite is the floor, and the argument that CHANGED is the mask —
    // so each captured shape is re-checked THROUGH the new exclusion with a
    // delivery's worth of our own text supplied, rather than by re-asserting a
    // bare detector call that no longer covers the risk.
    let paste = "Please rebase onto main and re-read the review findings on PR #814 \
                 before pushing anything else.";
    for (name, fixture) in [
        ("claude-askuserquestion", FIX_CLAUDE_ASK),
        ("copilot-question", FIX_COPILOT_ASK),
        ("copilot-multichoice", FIX_COPILOT_MULTICHOICE),
        ("claude-mcp-approval", FIX_CLAUDE_MCP_APPROVAL),
        ("pointer-last", FIX_POS_PTR_LAST),
    ] {
        let masked = mask_own_paste(&strip_ansi(fixture.as_bytes()), paste);
        assert!(
            prompt_wait_match(&masked).is_some(),
            "{name}: a real dialog must still hold the delivery — #727's suite is the floor"
        );
    }
    // ...and every false positive #40 and #727 already closed must stay closed.
    for (name, fixture) in [
        ("fp-resumed-idle", FIX_FP_RESUMED_IDLE),
        ("fp-prose-arrow-keys", FIX_FP_PROSE),
        ("fp-shell-prompt-glyph", FIX_FP_SHELL),
        ("fp-breadcrumb-separator", FIX_FP_BREADCRUMB),
    ] {
        let masked = mask_own_paste(&strip_ansi(fixture.as_bytes()), paste);
        assert!(
            prompt_wait_match(&masked).is_none(),
            "{name}: must still take its delivery immediately: {:?}",
            prompt_wait_match(&masked)
        );
    }
}

#[test]
fn g6_the_queue_polls_hold_record_names_what_the_question_gate_matched() {
    // #820, finding 1. Every capped hold and every abort has recorded `matched`
    // since #513(c)/F2 — but the hold a human actually sits in front of is the
    // drainer's, which re-arms with NO cap, and it recorded `blocked_on:
    // "question"` and nothing whatever else. A false positive that parks a pane
    // for a whole session is precisely the record that has to name its own
    // shape, and #820 could not be diagnosed from `audit.jsonl` at all.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 8200u32;
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let t0 = 1_000_000u64;

    // The witness the drainer's own poll produced — a real detector match off a
    // real screen, not a hand-built one, so the audit is pinned to the
    // vocabulary the detector actually emits.
    let matched = prompt_wait_match(&strip_ansi(FIX_COPILOT_ASK.as_bytes()))
        .expect("the captured copilot dialog matches");
    let seen =
        QuestionWitnessed { matched: matched.clone(), grid: GridEvidence::StillRendered, idle_row: false };

    reg.hold_escalation_step(
        &g.id, &w.id, pty, WriteAdmission::HoldQuestion, 1, t0, bound, None, Some(&seen),
    );

    let rows = audit_entries(&reg, &g.id, "delivery-held-in-queue");
    assert_eq!(rows.len(), 1, "one line per hold EPISODE, unchanged");
    assert_eq!(rows[0]["detail"]["blocked_on"], "question");
    assert_eq!(
        rows[0]["detail"]["matched"]["signal"], matched.signal,
        "the record has to say WHICH rule fired — the fastest way to spot one misfiring on \
         our own prose, and the field whose absence left #820 undiagnosable"
    );
    assert_eq!(rows[0]["detail"]["matched"]["line"], matched.line, "and the line it fired on");
    assert_eq!(
        rows[0]["detail"]["matched"]["grid"], "still-rendered",
        "and whether the screen agreed"
    );

    // `null`, not a missing key, where the gate matched nothing — for a
    // question-blocked row that disagreement is itself the finding, and an
    // absent key is indistinguishable from an older build.
    let pty2 = 8201u32;
    reg.hold_escalation_step(
        &g.id, &w.id, pty2, WriteAdmission::HoldBoxOccupied, 1, t0, bound, None, None,
    );
    let rows = audit_entries(&reg, &g.id, "delivery-held-in-queue");
    assert_eq!(rows.len(), 2);
    assert!(
        rows[1]["detail"]["matched"].is_null(),
        "no match seen must be RECORDED, not omitted: {:?}",
        rows[1]
    );
}

#[test]
fn g7_a_short_steer_in_the_chevron_composer_is_still_our_own_prompt() {
    // #820's stated residual, now reported for real: a SHORT steering line
    // (`merge`, `ok`, `fix the build`) from loomux's compose strip sits in
    // Copilot's `❯ ` chevron composer below `SELF_ECHO_MIN_POINTER_CHARS`, so
    // the pointer strip refused it and the pre-Enter question gate latched on
    // our own text for up to `QUESTION_HOLD_MAX`. The evidence that separates
    // our composer from a live dialog is neighbourhood, not length: a dialog
    // paints a `? ...` prompt row above its options (g4's fixture, every
    // captured claude/copilot question); a composer paints none. This pins
    // both directions of the waiver that buys the short case.
    let tail = "● Ready.\nC:\\Projects\\loomux [⎇ main]\n──────────────────────\n❯ merge\n──────────────────────\n  @ files · # issues · Session: 4.2 AIC used";

    // Precondition: the `❯ ` row fires the pointer signal without the
    // exclusion — the same check g1 makes, and the one that proves the tail is
    // a shape the waiver is even about.
    let unmasked = prompt_wait_match(tail).expect("the raw screen IS pointer-shaped");
    assert_eq!(unmasked.signal, "pointer-option", "and by THIS signal class");

    // h1: the short line, with no dialog header above it, IS our own prompt.
    let masked = mask_own_paste(tail, "merge");
    assert!(
        prompt_wait_match(&masked).is_none(),
        "a short line in a composer with no dialog header above it is our own steer, not a \
         menu's choice (#820 residual): {:?}",
        prompt_wait_match(&masked)
    );

    // h2: the SAME shape is a live dialog's choice when a `? ...` header sits
    // above the pointer row — g4's fail-safe must survive the waiver.
    let dialog = "? Overwrite the existing file?\n❯ Overwrite\n  Keep both";
    let dialog_masked = mask_own_paste(dialog, "Overwrite");
    let m = prompt_wait_match(&dialog_masked)
        .expect("a real dialog with a short highlighted choice must still be a question (#420)");
    assert_eq!(m.needle, QuestionNeedle::LeadingPointer, "and still by the pointer");

    // h3: end to end, at the same pre-Enter checkpoint that held the pane.
    let pred = question_hold_predicate(
        move || Some(tail.as_bytes().to_vec()),
        Some("merge".to_string()),
        Vec::new(),
    );
    assert!(
        !pred(),
        "a short steer under no dialog header must not hold the delivery (#820 residual)"
    );
}

#[test]
fn g8_short_pastes_cannot_claim_short_options_of_real_dialogs() {
    // The waiver's fail-safe, tested from the side the fixtures actually
    // exhibit. g4 pins only the `? Overwrite …` shape (which the veto always
    // recognized) and g5's paste is a single line above the floor, so neither
    // ever ENGAGES the short-line waiver — but the moment a short paste can be
    // claimed, the veto is the only thing standing between it and a live
    // dialog's `Yes` (#420). The captured dialogs do not all head their option
    // lists with `?`: Copilot's command dialog is `●`-bulleted, Claude's
    // question is boxed, and both end the question row with `?`. These pin
    // that the veto reads those shapes too, and that the one dialog with no
    // question row at all stays a question by its footer.

    // h1: the real copilot-question fixture, a short paste byte-matching its
    // own highlighted `❯ Yes`. Must stay a question — fixture-survival only,
    // NOT veto coverage: the detector reports the fixture's footer token
    // ahead of the pointer, so what is asserted here is the hold surviving,
    // not the needle; h2 removes the footer to pin the veto's protection of
    // the pointer row itself.
    let tail = strip_ansi(FIX_COPILOT_ASK.as_bytes());
    let masked = mask_own_paste(&tail, "Yes");
    assert!(
        prompt_wait_match(&masked).is_some(),
        "a live Copilot command dialog must survive a short `Yes` paste (#420): {:?}",
        prompt_wait_match(&masked)
    );

    // h2: the same shape with the footer REMOVED — here the veto, and only the
    // veto, keeps `❯ Yes` out of the mask. This is the footerless dialog the
    // #420 harm is about; a footer-token answer would leave it open. The
    // command line is indented exactly as Copilot paints it, so the scan steps
    // over it to the question (an unindented one would stop the scan — the
    // documented residual).
    let footerless = "● Allow Copilot to run the following command?\n  $ npm test\n\n❯ Yes\n  \
                      No, and tell Copilot what to do differently";
    let footerless_masked = mask_own_paste(footerless, "Yes");
    let m2 = prompt_wait_match(&footerless_masked)
        .expect("a footerless Copilot dialog's short highlighted choice must still be a question");
    assert_eq!(m2.needle, QuestionNeedle::LeadingPointer, "and still by the pointer");

    // h3: the Claude MCP approval asks no question — prose preamble, then the
    // numbered options. The veto cannot name it (nothing ends in `?`), so its
    // protection is the confirm footer plus options no paste plausibly matches;
    // pin that the one short paste that COULD byte-match its highlighted row
    // still leaves the dialog a question.
    let mcp = strip_ansi(FIX_CLAUDE_MCP_APPROVAL.as_bytes());
    let mcp_masked = mask_own_paste(&mcp, "1. Use this MCP server");
    assert!(
        prompt_wait_match(&mcp_masked).is_some(),
        "an MCP approval dialog must hold even when its highlighted row matches our paste: {:?}",
        prompt_wait_match(&mcp_masked)
    );
}

#[test]
fn g9_the_header_scan_tracks_the_option_block_not_a_fixed_window() {
    // The calibration gap a fixed lookback budget would carry: the captured
    // fixtures put the pointer on the FIRST option with the question one row
    // away, but a live dialog has the pointer wherever the user last arrowed
    // it, a multi-line `$` command under the question, and Copilot's generous
    // blank padding. A 6-row budget loses the question when the pointer is on
    // option 3 of a command dialog whose question owns a two-line `$` block —
    // and then the waiver masks the live `❯ Yes`, which is the #420 harm.
    // `dialog_header_above` now follows the option block upward instead of
    // counting rows, so the question is found at ANY distance as long as only
    // option/blank/indented-command rows sit between it and the pointer.

    // h1: the reviewer's shape — pointer on option 3, two-line indented
    // command block, no footer. The question is six rows up; a fixed budget of
    // 6 would veto nothing and the `❯ Yes` row would be masked. The scan
    // steps past the two options, the two command rows and the blanks, finds
    // the question row, and keeps the dialog a question by the pointer.
    let tail = "● Allow Copilot to run the following command?\n\n  $ npm test\n  --watch\n\n\
                │   Yes, and don't ask again this session\n\
                │   No, and tell Copilot what to do differently\n\
                │ ❯ Yes";
    let masked = mask_own_paste(tail, "Yes");
    let m = prompt_wait_match(&masked)
        .expect("a footerless dialog with the pointer on a LATER option must still be a question");
    assert_eq!(m.needle, QuestionNeedle::LeadingPointer, "and by the pointer, not a token");

    // h2: the wrapped-question variant — the `?` lands on the question's
    // wrapped continuation row, which is indented and therefore would read as
    // an option row to a shape scan that did not test rows for question-ness
    // before classifying them. It is tested first, so the veto still fires.
    let wrapped = "? Overwrite the existing file and keep a\n  copy of the previous one elsewhere?\n\n\
                   ❯ Overwrite\n  Keep both";
    let wrapped_masked = mask_own_paste(wrapped, "Overwrite");
    let m3 = prompt_wait_match(&wrapped_masked)
        .expect("a dialog whose question wraps across rows must still be a question (#420)");
    assert_eq!(m3.needle, QuestionNeedle::LeadingPointer, "and still by the pointer");

    // h3: and the composer's own chrome must still read as headerless with
    // more scrollback above it — the divider stops the scan on its very
    // first upward step, so `earlier output`/`keeps scrolling here` are
    // never examined; this pins that a taller tail doesn't change the
    // divider's veto, NOT that those rows individually fail the question
    // test (g7 pins the minimal-tail case; the scan behaves identically here
    // because it never reads past the divider).
    let tall = "──────────────────────\n  earlier output\n  keeps scrolling here\n\
                ──────────────────────\n❯ merge\n──────────────────────\n  @ files · # issues";
    let tall_masked = mask_own_paste(tall, "merge");
    assert!(
        prompt_wait_match(&tall_masked).is_none(),
        "composer chrome above the divider is still not a dialog question: {:?}",
        prompt_wait_match(&tall_masked)
    );
}

#[test]
fn a_re_grounding_closed_on_liveness_is_never_audited_as_confirmed() {
    // #546, the honest-labeling half. #535 made the re-grounding phase resolve
    // on the agent's own MCP call instead of on loomux's delivery sampler, and
    // #588 put that provenance in the lifecycle badge. What both left standing
    // is the DURABLE record: every resolution, whichever signal closed it, was
    // written under one action — `compact-reinjection-confirmed`.
    //
    // On this path nothing was confirmed. No delivery confirmation is supplied
    // anywhere below; the phase closes because the agent called a loomux tool,
    // which proves it is alive and executing and says nothing whatever about
    // our paste. The uncovered case #546 filed — a genuinely LOST re-grounding
    // on a pane that was busy for some other reason — lands here, and an
    // operator counting confirmations in `audit.jsonl` counts it as a
    // re-grounding that landed.
    //
    // `audit.jsonl` is the surface that outlives the badge, the session and the
    // pane, so the claim it makes has to be the one the evidence supports. Two
    // actions, not one action with a field — the same argument #539 made for
    // `delivery-unconfirmed-agent-active` being its own name rather than a
    // second flavour of `delivery-unconfirmed-idle-pane`.
    let (reg, _d, gid, oid) = compact_nudge_setup(0);
    let grew: HashMap<String, u64> = [(oid.clone(), 50_000u64)].into_iter().collect();
    let attempted = reinject_awaiting_confirmation(&reg, &oid, &grew);

    // The agent answers past the settling floor — #535's acknowledgment, and
    // the ONLY evidence in play here.
    reg.set_last_mcp_activity_ms_for_test(&oid, attempted + REINJECT_ACK_SETTLE_MS + 1_000);
    let _ = reg.compact_nudge_tick(attempted + REINJECT_TIMEOUT_MS, &grew, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());

    assert!(!reg.agent(&oid).unwrap().compact_pending, "the phase did resolve — #535 is not regressed");
    assert_eq!(
        audit_count(&reg, &gid, "compact-reinjection-confirmed"), 0,
        "nothing confirmed this re-grounding — the agent's own liveness is not a confirmation, \
         and a record that says otherwise is the overclaim #546 is filed against"
    );

    let liveness = audit_entries(&reg, &gid, "compact-reinjection-liveness-only");
    assert_eq!(liveness.len(), 1, "the resolution must still be recorded, under a name that fits it");
    assert_eq!(liveness[0]["detail"]["source"], "activity", "the evidence source, unchanged from #535/#588");
    // The record is self-describing: a reader must not have to already know
    // which of the two `source` values is the weak one.
    let proves = liveness[0]["detail"]["proves"].as_str().unwrap_or_default();
    assert!(proves.contains("alive"), "the record must state what the evidence establishes, got: {proves:?}");
    let residual = liveness[0]["detail"]["does_not_prove"].as_str().unwrap_or_default();
    assert!(
        residual.contains("read") && residual.contains("delivered"),
        "and what it does not — liveness proves neither that the re-grounding was delivered nor \
         that it was read, got: {residual:?}"
    );
}
