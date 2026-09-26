//! Attention routing, the question hold, paste masking and stranded-delivery self-heal.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ---------- attention routing: surface which pane needs the human (#6) ----------

/// Group with an orchestrator and one working (tasked) worker; the watchdog is
/// off (irrelevant here). Returns (reg, tempdir, group, worker id).
pub(crate) fn attention_setup() -> (OrchRegistry, tempfile::TempDir, GroupId, String) {
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/repo", watchdog_rails(0)).unwrap();
    reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "do work", false, None).unwrap();
    (reg, dir, g.id, w.id)
}

pub(crate) fn no_tails() -> HashMap<String, String> {
    HashMap::new()
}

#[test]
fn prompt_wait_detected_spots_prompts_not_chatter() {
    // Claude Code permission menu.
    assert!(prompt_wait_detected(
        "Do you want to make this edit to lib.rs?\n❯ 1. Yes\n  2. No, tell Claude what to do"
    ));
    // Copilot-style allow prompt with a selection pointer.
    assert!(prompt_wait_detected("? Allow command? ›\n❯ Yes\n  No"));
    // A bare yes/no confirmation.
    assert!(prompt_wait_detected("Overwrite the file? (y/n)"));
    // Folder-trust dialog.
    assert!(prompt_wait_detected("Do you trust the files in this folder?"));
    // Normal streaming output is not a prompt.
    assert!(!prompt_wait_detected(
        "Running cargo test...\n   Compiling loomux v0.2.0\ntest result: ok. 42 passed"
    ));
    // A question merely mentioned mid-explanation is not enough on its own.
    assert!(!prompt_wait_detected(
        "I weighed whether to proceed with the refactor and decided it was fine."
    ));
    assert!(!prompt_wait_detected(""));
}

// Captured/synthetic terminal output (with real ANSI + box drawing) for the
// interactive-question repro from issue #40. Fed through `strip_ansi` exactly as
// the live attention scan does, so the fixtures exercise the whole detection
// pipeline, not a pre-cleaned string.
pub(crate) const FIX_CLAUDE_ASK: &str = include_str!("../fixtures/attention/claude-askuserquestion.txt");
pub(crate) const FIX_COPILOT_ASK: &str = include_str!("../fixtures/attention/copilot-question.txt");
// #420 rev-15 N3: a genuine multi-choice (non-yes/no) Copilot question, built
// to exercise a DIFFERENT detector signal than copilot-question.txt (whose
// pointer sits in the last painted lines and carries a footer, so it already
// matches `has_pointer_option`/`has_menu_footer` — signals `pos-pointer-last-
// line.txt` and `claude-askuserquestion.txt` already cover). Here the trailing
// "Copilot is thinking..." status line pushes the pointer+numbered option OUT
// of the last-3-lines window (no footer either), so ONLY the structured
// `has_numbered_menu` signal (the `❯ 1.` substring, read across the whole
// 12-line window, not just the last 3) can catch it — a rev-15-neutralized
// `has_numbered_menu` makes this specific fixture undetected while the other
// three positives keep passing.
//
// #420 rev-19 N11: this fixture (and copilot-question.txt above it) is a
// PLAUSIBLE, hand-built approximation of Copilot's TUI style, not a verified
// live capture — an exhaustive search of this repo's history and every
// group's audit-log archive on this machine turned up no real captured
// Copilot multi-choice menu, and CLAUDE.md constraint 3 forbids spawning a
// real Copilot CLI to obtain one in-session. `claude-mcp-approval.txt`
// (below) is what actually closes that gap: a REAL numbered-menu capture
// from a live session's own audit log, verbatim (respaced only — the
// `get_output` MCP tool's logged `text` field collapses whitespace runs, a
// pre-existing artifact of that logging path, not of the terminal itself).
// It's Claude Code's, not Copilot's, but `prompt_wait_detected` and
// `has_numbered_menu` are CLI-agnostic by construction (the whole guard is,
// per rev-15 N5) — a verified-real multi-choice menu proves the signal
// against reality regardless of which CLI painted it. This file remains
// useful for the DIFFERENT thing it isolates (the signal alone, footer-less,
// scrolled out of the prose-guard's last-3-line window) but is explicitly
// NOT the "proven against reality" fixture; that's the one below.
pub(crate) const FIX_COPILOT_MULTICHOICE: &str = include_str!("../fixtures/attention/copilot-multichoice-question.txt");
// #420 rev-19 N11: a REAL captured interactive-question menu — Claude Code's
// MCP-server approval dialog, verbatim from a live session's own audit log
// (respacing only, see the note above; wording, numbering, and footer are
// exactly as captured). rev-19 n2 correction: this capture's footer survived
// too, so it witnesses `has_numbered_menu` AND `has_menu_footer` TOGETHER,
// not `has_numbered_menu` in isolation — it is NOT a sole-signal witness.
// Sole-signal isolation for `has_numbered_menu` specifically is what
// `copilot-multichoice-question.txt` (above) is built for (synthetic, with
// the footer/pointer deliberately absent). What this fixture grounds is
// simpler and narrower: that the COMBINED shape `has_numbered_menu`/`has_
// menu_footer` already cover is something a real CLI genuinely paints, not
// only something imagined to test the detector's branches.
pub(crate) const FIX_CLAUDE_MCP_APPROVAL: &str = include_str!("../fixtures/attention/claude-mcp-approval.txt");
pub(crate) const FIX_STREAMING: &str = include_str!("../fixtures/attention/streaming-output.txt");
pub(crate) const FIX_IDLE_BOX: &str = include_str!("../fixtures/attention/idle-input-box.txt");
// Finished-turn agent output that *mentions* interactive-UI cues but is not a
// live prompt (#40 review false-positive repro): keyboard-nav prose, a pasted
// shell prompt whose glyph is `❯`, and a `›` UI breadcrumb — each followed by
// the CLI's redrawn idle input box.
pub(crate) const FIX_FP_PROSE: &str = include_str!("../fixtures/attention/fp-prose-arrow-keys.txt");
pub(crate) const FIX_FP_SHELL: &str = include_str!("../fixtures/attention/fp-shell-prompt-glyph.txt");
pub(crate) const FIX_FP_BREADCRUMB: &str = include_str!("../fixtures/attention/fp-breadcrumb-separator.txt");
// A leading ❯/›/→ in finished-turn prose above the idle box: repro steps that
// lead with a shell `❯` glyph, and a fenced ❯ command block. The pointer leads
// the line but is not in the last painted lines, so it must NOT flag (#40 review
// residual). Contrast with pos-pointer-last-line, where the pointer *is* the
// last thing painted — a genuine prompt-wait positive.
pub(crate) const FIX_FP_LEADING_PTR: &str = include_str!("../fixtures/attention/fp-leading-pointer-prose.txt");
pub(crate) const FIX_FP_FENCED_PTR: &str = include_str!("../fixtures/attention/fp-fenced-pointer-block.txt");
pub(crate) const FIX_POS_PTR_LAST: &str = include_str!("../fixtures/attention/pos-pointer-last-line.txt");
// #727, from the live repro: a RESUMED pane's restored screen — a finished
// turn's report, then the CLI's chrome. The input box is a bare `❯` on its own
// row (Claude Code's empty prompt, under a custom-agent rule), and it is the
// third-from-last non-empty row, so it lands squarely inside the detector's own
// last-3-painted-lines pointer window. Nothing on this screen is asking anyone
// anything; the pane is idle at an empty box. Captured shape, not imagined:
// this is `get_output`'s render of w-209 in group loomux-68435179, the pane the
// question gate held for 25 minutes.
pub(crate) const FIX_FP_RESUMED_IDLE: &str =
    include_str!("../fixtures/attention/fp-resumed-agent-idle-prompt.txt");
// #903, the two screens the issue reports: a resumed reviewer pane and the
// ORCHESTRATOR's own pane, both idling at an empty input box, both with a
// delivery queue behind "an interactive question is on screen" that never
// released (25 min / 30+ min, three panes killed by hand).
//
// **Provenance, split, because the two halves are not equally sourced.** The
// CHROME — separator, the bare `❯` box, separator, and the
// `⏵⏵ auto mode on (shift+tab to cycle) · ← for agents` footer — is byte-verbatim
// from `fp-resumed-agent-idle-prompt.txt`, itself a real `get_output` capture of
// #727's wedged pane; #903's own comments quote that footer line as present on
// both of its screens, which is what makes the same chrome the right chrome. The
// replayed TURN TEXT is reconstructed: the issue records the footer, the empty
// box and "no question anywhere on screen", not the body of the turn the resume
// replayed. So each fixture asserts its own preconditions (which detector signal
// fires, on which line, inside which window) rather than assuming them — the
// discipline `copilot-composer-holds-our-paste.txt` documents for the same
// reason.
//
// They are deliberately not variations on one shape:
//
//  - `fp-resumed-review-verdict-idle`: a rev-lead's replayed verdict, quoting the
//    detector it is reviewing. It carries THREE would-be signals — `(y/n)`
//    (structured tier), `yes/no` and `waiting for your` (#903 demoted both to the
//    last-painted tier) — so it exercises the token re-tiering AND the grid
//    reading, and shows why the re-tiering alone was never going to be enough.
//  - `fp-orchestrator-relay-idle`: the orchestrator relaying a worker's blocked
//    report, which quotes a permission phrase (`do you want to proceed`) that
//    stays in the wide structured tier on purpose. Nothing but the composed
//    screen can save this one.
pub(crate) const FIX_FP_RESUMED_VERDICT: &str =
    include_str!("../fixtures/attention/fp-resumed-review-verdict-idle.txt");
pub(crate) const FIX_FP_ORCH_RELAY: &str = include_str!("../fixtures/attention/fp-orchestrator-relay-idle.txt");
// #820: copilot 1.0.7x holding a loomux delivery that has been pasted into its
// composer and not yet submitted — the pane a human photographed for the issue
// ("the chip reads the question-gate wording and the screen holds only loomux's
// own prompt"). Two shapes, because copilot has two composers and BOTH put a
// row of our own text in front of the detector:
//
//  - `copilot-composer-holds-our-paste`: the chevron composer, whose input row
//    literally begins with `❯ ` — a `POINTER_GLYPHS` member, so our own paste
//    reads as a menu's highlighted choice;
//  - `copilot-framed-composer-wrap`: the 1.0.64+ prompt frame, where the `┃ `
//    border de-frames away and the WRAP is what leaves an arrow leading a row —
//    ordinary orchestrator prose (`red → green`) is full of them.
//
// **Reconstructed from cited sources, not captured** (CLAUDE.md constraint 3
// forbids spawning a real copilot to capture one): the `❯ ` input row is the
// shape pasted in github/copilot-cli#4070 (v1.0.69) and #4292 (v1.0.61), the
// per-row `┃ ` border is the verbatim composer paste in #4116 (v1.0.71-0), and
// the `@ files · # issues` hint bar is #4184 (v1.0.72-0). Anything a test needs
// to be true of the SHAPE — that the offending row lands inside the detector's
// own last-3-painted-lines window — is asserted as a precondition rather than
// assumed, the way #727's own fixture test does it.
pub(crate) const FIX_COPILOT_COMPOSER_PASTE: &str =
    include_str!("../fixtures/attention/copilot-composer-holds-our-paste.txt");
pub(crate) const FIX_COPILOT_FRAMED_WRAP: &str =
    include_str!("../fixtures/attention/copilot-framed-composer-wrap.txt");
/// The exact text loomux pasted into the pane `FIX_COPILOT_COMPOSER_PASTE`
/// shows — one logical line, as `deliver_prompt` would still be holding it at
/// the pre-Enter checkpoint.
pub(crate) const COMPOSER_PASTE_TEXT: &str =
    "Review requested changes on PR #814 and push fixes to the same branch.";
/// The same, for the framed fixture: one logical line the composer wrapped
/// across two rows, with one of the brief's own arrows landing at the start of
/// the second.
pub(crate) const FRAMED_PASTE_TEXT: &str = "Order of work: understand the failure first, then design the \
     fix, → then implement it and get CI green on all three platforms.";

#[test]
fn prompt_wait_detected_fires_on_interactive_question_fixtures() {
    // #40: a Claude Code AskUserQuestion menu highlights the active option with
    // reverse-video SGR (stripped away), leaving numbered options with arbitrary
    // labels and an "Enter to select" footer — no selection glyph survives.
    assert!(
        prompt_wait_detected(&strip_ansi(FIX_CLAUDE_ASK.as_bytes())),
        "AskUserQuestion menu must be recognized as needing the human"
    );
    // #40: a Copilot CLI question draws its `❯` pointer indented inside a box, so
    // the option line never *starts* with the pointer after trimming.
    assert!(
        prompt_wait_detected(&strip_ansi(FIX_COPILOT_ASK.as_bytes())),
        "Copilot boxed selection prompt must be recognized as needing the human"
    );
    // #40 review: a plain inquirer confirm whose `❯` pointer IS the last thing
    // painted (no footer / no y-n token) must still flag — proving the anchored
    // pointer signal fires when the menu really is on screen.
    assert!(
        prompt_wait_detected(&strip_ansi(FIX_POS_PTR_LAST.as_bytes())),
        "a pointer as the last painted line is a genuine prompt-wait"
    );
    // #420 rev-15 N3: a numbered/radio-select Copilot question whose pointer
    // has scrolled out of the last-3-lines window (a trailing status line
    // follows it) — only `has_numbered_menu`'s whole-window substring check
    // catches this, not the pointer/footer signals the other fixtures above
    // already exercise. This is the case a delivery landing mid-dialog would
    // silently answer by selecting whichever option the Enter lands on.
    assert!(
        prompt_wait_detected(&strip_ansi(FIX_COPILOT_MULTICHOICE.as_bytes())),
        "a numbered multi-choice Copilot question must be recognized as needing the human"
    );
    // #420 rev-19 N11: the REAL captured menu (see the fixture's own doc
    // comment for provenance and the n2 correction — it witnesses
    // `has_numbered_menu` combined with `has_menu_footer`, not either signal
    // alone) — proves that combined shape against an actual screen paint,
    // not an imagined one.
    assert!(
        prompt_wait_detected(&strip_ansi(FIX_CLAUDE_MCP_APPROVAL.as_bytes())),
        "a real captured multi-choice menu must be recognized as needing the human"
    );
}

// ---------- #420 rev-19 R1/B4: question_hold_predicate ----------
//
// `wait_for_question_clear`'s old production-bound form took a concrete
// `&PtyManager`, so nothing but a live pty could ever exercise it — a test
// could disable the whole guard and every existing test still passed
// (rev-15's neutralized-predicate finding). `question_hold_predicate` is the
// fix: fully generic over every external read, so these tests drive the REAL
// decision logic with scripted closures, no PtyManager, no real PTY.
//
// rev-19 R1: release is now STATE-based, not activity-based — the predicate
// no longer takes ANY human-input signal at all (the old `last_input_ms`/
// `input_pending`/`held_since_ms` params are gone from the signature), so an
// arrow key navigating the still-open menu or xterm's automatic OSC/DCS query
// replies (#179) structurally CANNOT release a hold anymore — there is no
// input left in the type to fool it with. Release requires the SAME detector
// that raised the hold to read false on two CONSECUTIVE polls.
//
// rev-19 B-A: self-echo exclusion is now CONTENT-based (`mask_own_paste`),
// not byte-COUNT-based — the predicate takes `pasted_text: Option<String>`
// instead of a growth baseline. A byte-count baseline can only mark ONE point
// in time as "before"; a dialog that renders WHILE the paste is still
// settling gets baked into whichever number a checkpoint happened to
// snapshot and reads as invisible forever. Masking doesn't have that blind
// spot: `deliver_prompt` knows exactly what it pasted, so removing those
// exact lines from the tail leaves whatever the CLI itself painted, no
// matter when it painted it relative to our own paste's timing.

#[test]
fn question_hold_predicate_ignores_self_echo_of_our_own_just_pasted_text() {
    // rev-15 N1: the pre-Enter/retry checkpoints read the tail AFTER loomux's
    // own paste — so a delivered prompt that happens to contain phrasing
    // `prompt_wait_detected` matches ("(y/n)", "do you want to run") sits,
    // echoed and unsubmitted, in that exact tail. Masked out (every line of
    // the tail IS a pasted line here), this must NOT read as a live question.
    let text = "Do you want to run npm test? (y/n)";
    let pred = question_hold_predicate(move || Some(text.as_bytes().to_vec()), Some(text.to_string()), Vec::new());
    assert!(!pred(), "our own pasted text, masked out, must read as self-echo, not a live question");
}

#[test]
fn question_hold_predicate_holds_for_a_real_dialog_appended_after_our_own_paste() {
    // The tail contains BOTH our own pasted text AND a genuinely separate
    // dialog painted after it — masking removes only our own lines, leaving
    // the dialog fully visible to the detector.
    let pasted = "please rerun the deploy script and let me know how it goes";
    let tail = format!("{pasted}\n\nDo you want to run npm test? (y/n)");
    let pred = question_hold_predicate(move || Some(tail.as_bytes().to_vec()), Some(pasted.to_string()), Vec::new());
    assert!(pred(), "a dialog appended after our own (masked-out) paste must still hold");
}

#[test]
fn question_hold_predicate_pre_paste_checkpoint_has_no_self_echo_risk() {
    // The pre-paste checkpoint runs before this delivery has written
    // anything — `pasted_text: None` — so a live question already on screen
    // must hold; there is nothing of OURS on screen yet to mask.
    let pred = question_hold_predicate(|| Some(b"Do you want to run npm test? (y/n)".to_vec()), None, Vec::new());
    assert!(pred(), "pre-paste checkpoint: no self-echo possible, a live question must hold");
}

#[test]
fn question_hold_predicate_is_false_for_ordinary_output_and_a_closed_pty() {
    let ordinary = question_hold_predicate(
        || Some(b"Running cargo test...\ntest result: ok. 42 passed".to_vec()),
        None,
        Vec::new(),
    );
    assert!(!ordinary(), "ordinary streaming output must not read as a question");

    let closed = question_hold_predicate(|| None, None, Vec::new());
    assert!(!closed(), "a closed/gone pane must never block delivery");
}

#[test]
fn question_hold_predicate_ignores_activity_the_menu_is_still_open() {
    // rev-19 R1: no input signal exists in the type anymore for an arrow key
    // (a `HumanInput::Neutral` write — no text left sitting) or an automatic
    // terminal-query reply to fool the release with. As long as the SAME
    // matching tail is observed, the predicate must hold no matter how many
    // times it's polled.
    let pred = question_hold_predicate(|| Some(FIX_COPILOT_ASK.as_bytes().to_vec()), None, Vec::new());
    for i in 0..5 {
        assert!(pred(), "poll {i}: menu still on screen — must never release regardless of poll count");
    }
}

#[test]
fn question_hold_predicate_requires_two_consecutive_clear_reads_to_release() {
    // rev-19 R1: a single false read could be a transient mid-redraw miss —
    // release requires the detector to read false TWICE in a row once a hold
    // has genuinely started.
    let call = std::sync::atomic::AtomicU32::new(0);
    let pred = question_hold_predicate(
        move || {
            let n = call.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n == 0 { Some(FIX_COPILOT_ASK.as_bytes().to_vec()) } else { Some(b"idle input box".to_vec()) }
        },
        None,
        Vec::new(),
    );
    assert!(pred(), "poll 1: question shown -> hold");
    assert!(pred(), "poll 2: first clear read -> still hold (only one so far)");
    assert!(!pred(), "poll 3: second consecutive clear read -> release");
}

#[test]
fn question_hold_predicate_releases_on_first_poll_when_never_shown_a_question() {
    // rev-19 N10: a checkpoint that starts already-clear (the question was
    // answered, or never existed, before this hold began) must release
    // IMMEDIATELY — the two-consecutive-clear requirement only engages once a
    // hold has actually observed a question at least once; it must never
    // penalize the overwhelmingly common "nothing to hold for" case with an
    // artificial delay.
    let pred = question_hold_predicate(|| Some(b"idle input box".to_vec()), None, Vec::new());
    assert!(!pred(), "never shown a question — the very first call must release immediately");
}

#[test]
fn question_hold_predicate_wired_into_the_generic_hold_loop_releases_immediately_when_already_clear() {
    // rev-19 N10, through the REAL hold_for_human_input loop (not just the
    // predicate in isolation): held_ms must be 0 — no cap ride — when the
    // question was already gone at hold start.
    let pred = question_hold_predicate(|| Some(b"idle input box".to_vec()), None, Vec::new());
    let out = hold_for_human_input(&pred, Duration::from_secs(5), HB_POLL);
    assert_eq!(out, PasteDecision::Paste { held_ms: 0 }, "already-clear must release on the very first check, no cap ride");
}

#[test]
fn question_hold_predicate_wired_into_the_generic_hold_loop_releases_once_the_menu_clears_twice() {
    // #40 lesson, applied to #420: exercise the LOOP, not just the pure
    // decision. `hold_for_human_input` is the same generic block-until-clear
    // loop the box-occupied guard already uses; here the predicate shows a
    // question for a few polls, then the detector itself genuinely clears —
    // proving the state-based release (rev-19 R1) composes correctly with
    // the real loop, no PtyManager involved.
    use std::sync::atomic::{AtomicU32, Ordering};
    let polls = AtomicU32::new(0);
    let pred = question_hold_predicate(
        move || {
            let n = polls.fetch_add(1, Ordering::Relaxed);
            if n < 3 { Some(FIX_COPILOT_ASK.as_bytes().to_vec()) } else { Some(b"idle input box".to_vec()) }
        },
        None,
        Vec::new(),
    );
    let out = hold_for_human_input(&pred, Duration::from_secs(5), HB_POLL);
    match out {
        PasteDecision::Paste { held_ms } => {
            assert!(held_ms < 4000, "must release once the menu clears, well under the cap, got {held_ms}ms");
        }
        other => panic!("expected Paste once the menu clears, got {other:?}"),
    }
}

// ---------- #420 rev-19 B-A: mask_own_paste + the exact real-PtyManager scratch shape rev-19 used ----------

#[test]
fn mask_own_paste_removes_exactly_our_own_lines() {
    let pasted = "please rerun the deploy script and let me know how it goes\nit failed last time (y/n) was never even asked";
    let tail = format!("{pasted}\n\nDo you want to run npm test? (y/n)");
    let masked = mask_own_paste(&tail, pasted);
    assert!(!masked.contains("rerun the deploy script"), "our own pasted lines must be removed: {masked}");
    assert!(masked.to_lowercase().contains("do you want to run npm test"), "the dialog's own line must survive: {masked}");
}

#[test]
fn mask_own_paste_leaves_nothing_when_the_tail_is_only_our_own_paste() {
    let pasted = "Do you want to run npm test? (y/n) — that's literally what my report says";
    let masked = mask_own_paste(pasted, pasted);
    assert!(!prompt_wait_detected(&masked), "a paste that IS only our own text must mask away to nothing question-shaped: {masked:?}");
}

#[test]
fn question_hold_predicate_on_a_real_ptymanager_holds_for_a_dialog_seeded_alongside_a_large_paste_but_not_for_the_pastes_own_matching_text() {
    // rev-19 B-A's own reproduction shape, pinned here against a REAL
    // PtyManager (not scripted closures) so the whole path — output_tail_
    // bounded, strip_ansi, mask_own_paste, prompt_wait_detected — is
    // exercised together, the same way rev-19's scratch test demonstrated
    // the round-3 regression.
    let large_paste = "Please review the attached report and let me know if the retry logic \
        looks right. The deploy failed twice yesterday and once the day before, and I want to \
        make sure we're not silently retrying into a broken state. Let me know your thoughts \
        whenever you get a chance — no rush, but I'd like this DONE before the next release."
        .to_string();

    // Case 1: a genuine dialog seeded ALONGSIDE the large paste (the
    // canonical #420 timeline — Copilot painted a dialog while/after loomux
    // pasted) — must hold.
    let pm = PtyManager::default();
    let seeded_dialog_tail = format!("{large_paste}\n\nDo you want to run npm test? (y/n)");
    pm.register_fake_for_test(10, seeded_dialog_tail.as_bytes());
    let pred1 = question_hold_predicate(
        || pm.output_tail_bounded(10, 4096),
        Some(large_paste.clone()),
        Vec::new(),
    );
    assert!(pred1(), "a dialog seeded alongside a large paste must still be detected as active");

    // Case 2: the large paste ALONE, on screen with nothing else — even
    // though its OWN prose carries a matching phrase ("(y/n)"), proving
    // masking, not luck.
    let self_matching_paste = format!("{large_paste}\nDo you want to run npm test? (y/n)");
    let pm2 = PtyManager::default();
    pm2.register_fake_for_test(11, self_matching_paste.as_bytes());
    let pred2 = question_hold_predicate(
        || pm2.output_tail_bounded(11, 4096),
        Some(self_matching_paste.clone()),
        Vec::new(),
    );
    assert!(!pred2(), "a large paste whose OWN text matches the detector must mask away to not-held");
}

// ---------- #420 rev-15 B1 / rev-19 N9: flush + retry gating ----------

#[test]
fn should_flush_before_paste_now_is_suppressed_by_a_live_question() {
    // rev-15 B1: the stranded-text flush is the FIRST write `deliver_prompt`
    // makes — before the interactive-question checkpoint that follows it
    // ever runs. Without this gate, a question already on screen from BEFORE
    // this delivery started would eat the flush's Enter.
    assert!(
        should_flush_before_paste(Some(false), false),
        "sanity: the base decision alone would flush here"
    );
    assert!(
        !should_flush_before_paste_now(Some(false), false, true, false),
        "a live question must suppress the flush even when the base decision says to flush"
    );
    assert!(
        should_flush_before_paste_now(Some(false), false, false, false),
        "with no question active, the base decision still governs"
    );
}

#[test]
fn retry_gate_holds_for_a_question_or_writes_once_clear() {
    // rev-15 B2: a question appearing in the retry window means HOLD, not
    // retry. rev-19 N9: `retry_gate` only ever sees the question hold's
    // outcome now — the caller's own human-typing check already `break`s
    // BEFORE this is ever called (see mod.rs's retry loop), so there is no
    // `SkipHumanTyping` arm left to be dead code with an `unreachable!()`
    // panic risk sitting in it on a detached thread.
    assert_eq!(
        retry_gate(PasteDecision::Abort { held_ms: 120_000 }),
        RetryGate::SkipQuestionPending { held_ms: 120_000 },
        "a question that never cleared within the cap must skip this retry, not blind-fire it"
    );
    assert_eq!(
        retry_gate(PasteDecision::Paste { held_ms: 0 }),
        RetryGate::Write { held_ms: 0 },
        "no question in the way — write the retry"
    );
    assert_eq!(
        retry_gate(PasteDecision::Paste { held_ms: 500 }),
        RetryGate::Write { held_ms: 500 },
        "a question that cleared within the cap still ends in a write, just held first"
    );
}

// ---------- #420 rev-15 B3 / rev-19 R3: pre-Enter abort must leave a traceable outcome ----------

#[test]
fn a_recorded_unconfirmed_outcome_makes_the_next_deliverys_flush_fire() {
    // rev-15 B3: the pre-Enter abort pastes text and then withholds the
    // Enter — the ONLY way the next delivery's stranded-text flush can see
    // that text is stranded is if THIS delivery recorded `confirmed: false`.
    // This test pins the DOWNSTREAM consequence: given that recorded
    // outcome, and no human typing since, the next delivery's flush decision
    // must fire. See `record_aborted_preenter_outcome_makes_the_next_
    // deliverys_flush_actually_fire` below for the end-to-end version that
    // also proves the recording itself happens (not just its consequence).
    let recorded_on_abort = false; // DeliveryOutcome::confirmed, as the abort now records it
    assert!(
        should_flush_before_paste_now(Some(recorded_on_abort), false, false, false),
        "a delivery aborted pre-Enter must leave a `confirmed: false` outcome that the NEXT \
         delivery's flush decision reads as stranded text to clear"
    );
}

// ---------- #532: the two safety signals, fired backwards ----------
//
// The human's report was a false positive followed by a missed true positive:
// "held: question pending" on a totally EMPTY box, and then — the moment they
// started typing — the queue submitting the held prompt over their in-progress
// input. These pin that both halves are the SAME event. `prompt_wait_detected`
// reads an append-only byte RING (`output_tail_bounded`), not a screen, so the
// human's own echo is what pushes a stale question out of its window: the
// keystroke that RELEASES the question gate is the keystroke that OCCUPIES the
// box. A straight line of checkpoints therefore walks into the paste with a
// green light it earned against a box that was empty two minutes earlier.

#[test]
fn write_admission_puts_human_content_ahead_of_a_question() {
    // #510 is the absolute: holding too long costs a badge, submitting over a
    // person's line costs everything. So `box_pending` outranks the question
    // even when BOTH are true — a caller must never badge "question pending"
    // (and, worse, resolve on the question clearing) while human characters
    // are outstanding.
    assert_eq!(write_admission(false, false), WriteAdmission::Go);
    assert_eq!(write_admission(false, true), WriteAdmission::HoldQuestion);
    assert_eq!(write_admission(true, false), WriteAdmission::HoldBoxOccupied);
    assert_eq!(
        write_admission(true, true),
        WriteAdmission::HoldBoxOccupied,
        "human content in the box outranks a question — reversing this precedence is the \
         mutation that lets a question-clearing event admit a write over a person's line"
    );
    assert!(write_admission(false, false).go());
    assert!(!write_admission(true, false).go());
    assert!(!write_admission(false, true).go());
}

#[test]
fn write_admission_badges_the_gate_that_actually_blocked() {
    // The badge and the enqueue reason are derived from the admission rather
    // than re-chosen at each call site, so a pane can never say "question
    // pending" while the thing blocking it is the human's own line — which is
    // exactly the mislabel #532's first half looked like from the outside.
    assert_eq!(write_admission(false, false).held_reason(), None);
    assert_eq!(
        write_admission(true, true).held_reason(),
        Some(HeldReason::BoxOccupied),
        "with both gates up the badge must name the one that actually governs"
    );
    assert_eq!(
        write_admission(false, true).held_reason(),
        Some(HeldReason::InteractiveQuestion)
    );
}

#[test]
fn the_queued_notice_names_the_gate_that_actually_blocked() {
    // rev-12 NB1. `AbortedPreEnter` used to hardcode `Question` in the notice
    // it sends, because that path had one cause. #532 gave it a second — the
    // pre-Enter occupancy gate — and the hardcode then told the orchestrator
    // "an interactive question is on screen" for a pane whose only blocker was
    // the human's own half-typed line, sending it to look for a dialog that
    // does not exist. `enqueue_reason` is what the call site now carries.
    assert_eq!(
        write_admission(true, false).enqueue_reason(),
        queue::EnqueueReason::BoxOccupied,
        "a box-occupied abort must not announce itself as a question"
    );
    assert_eq!(
        write_admission(false, true).enqueue_reason(),
        queue::EnqueueReason::Question
    );
    assert_eq!(
        write_admission(true, true).enqueue_reason(),
        queue::EnqueueReason::BoxOccupied,
        "with both gates up, the reason follows the same precedence as the badge"
    );
    // And the notice text a human/orchestrator actually reads differs by it —
    // pinning the mapping alone would not catch a caller that dropped it.
    assert!(
        queue::queued_notice("w-1", queue::EnqueueReason::BoxOccupied).contains("human input"),
        "got: {}",
        queue::queued_notice("w-1", queue::EnqueueReason::BoxOccupied)
    );
}

#[test]
fn the_keystroke_that_clears_a_stale_question_must_not_admit_a_write() {
    // THE SEQUENCE PIN, as pure state. This is the incident, step by step:
    //
    //   1. stale hold: box empty, detector still matching bytes in the ring
    //   2. the human types: their echo pushes the question out of the window,
    //      so `question_active` flips false — and their characters land in the
    //      box, so `box_pending` flips true, in the SAME event
    //   3. the drain must WAIT here. Before #532 the question gate was the
    //      last one consulted, so its clearing was read as a green light.
    //   4. the human submits or clears: box empties
    //   5. only NOW may the write go.
    let stale_hold = write_admission(false, true);
    assert_eq!(stale_hold, WriteAdmission::HoldQuestion, "step 1: held on the stale question");

    let human_starts_typing = write_admission(true, false);
    assert!(
        !human_starts_typing.go(),
        "step 3: the question clearing must NOT admit a write — the same keystroke that \
         cleared it put the human's characters in the box. This assertion is the whole bug: \
         reading only the question gate here returns Go and submits mid-typing."
    );
    assert_eq!(
        human_starts_typing,
        WriteAdmission::HoldBoxOccupied,
        "and the hold must now be attributed to the box, not still to the question"
    );

    let box_emptied = write_admission(false, false);
    assert!(box_emptied.go(), "step 5: box empty and no question — now, and only now, write");
}

#[test]
fn the_flush_press_reads_occupancy_not_just_the_keystroke_timestamp() {
    // `human_typed_since` is `last_user_input_ms > submit_sent_ms` — a
    // timestamp compare, which answers "did a keystroke land AFTER our own
    // submit". That is strictly narrower than "is there human content in this
    // box". A human who typed a line and left it sitting BEFORE our submit
    // stamps at or before `submit_sent_ms`, so `human_typed_since` is FALSE
    // and every pre-#532 gate waved the Enter through onto their line.
    assert!(
        should_flush_before_paste_now(Some(false), false, false, false),
        "sanity: the stranded-flush case this guard exists for still fires"
    );
    assert!(
        !should_flush_before_paste_now(Some(false), false, false, true),
        "human content in the box must suppress the flush Enter even though the keystroke \
         timestamp is older than our submit and no question is on screen — the exact hole \
         `human_typed_since` alone cannot see"
    );
    // ...and occupancy must not become a second way to suppress the flush the
    // guard exists FOR. Our own pasted text never moves `input_pending`
    // (`note_user_input` runs on human writes only, never on `write_bytes`),
    // so the stranded case reads `box_pending == false` and still flushes —
    // pinned by the first assertion above rather than merely asserted here.
    assert!(
        !should_flush_before_paste_now(Some(true), false, false, false),
        "a CONFIRMED previous delivery still means nothing to flush, unchanged"
    );
    assert!(
        !should_flush_before_paste_now(None, false, false, false),
        "the first delivery to a pane still never flushes, unchanged"
    );
}

#[test]
fn hold_bound_elapsed_is_the_clock_alone_and_disabled_at_zero() {
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let held_since = 1_000_000u64;

    assert!(
        !hold_bound_elapsed(held_since, held_since + bound - 1, bound),
        "inside the bound the hold is ordinary — nothing to report"
    );
    assert!(
        hold_bound_elapsed(held_since, held_since + bound, bound),
        "at the bound exactly, the hold is reportable"
    );
    assert!(
        !hold_bound_elapsed(held_since, held_since + bound * 10, 0),
        "`bound_ms == 0` disables the bound rather than firing instantly, so a mis-set \
         constant degrades to silence instead of badging every pane"
    );
    assert!(
        !hold_bound_elapsed(held_since, held_since.saturating_sub(5_000), bound),
        "a clock that reads backwards must not badge (saturating, not wrapping)"
    );
}

// ---------- #532 rev-12 B1/NB3: the escalation path itself ----------
//
// These exist because rev-12 proved the whole escalation could be DELETED with
// the suite still green — on a PR whose reason for existing is a hold that
// escalated to nobody. `held_escalation` is the extracted decision; every
// property below is one a future edit could otherwise drop silently.

#[test]
fn no_signal_can_veto_the_escalation_however_stuck_it_is() {
    // rev-12 NB3, and the most important assertion in this PR after the
    // sequence pin. The first cut let `box_pending` return "not stale", so a
    // pane whose occupancy counter was stuck true held forever AND never
    // reported it: the pre-paste loop holds, the pre-Enter gate declines, the
    // flush declines, and the badge that exists to report exactly that never
    // fires. `input_pending` has a reachable stuck-true mode — it is a counter
    // only human writes move, zeroed on only \r/\n, Ctrl-U and Ctrl-C, so a
    // bare ESC or the CLI consuming the line leaves it above zero over an empty
    // box (see `note_user_input`). An escalation a broken signal can silence is
    // not a bound.
    let bound = QUESTION_HOLD_STALE_AFTER.as_millis() as u64;
    let held = 1_000_000u64;
    let long_past = held + bound * 10;

    assert_eq!(
        held_escalation(WriteAdmission::HoldBoxOccupied, held, long_past, bound, false),
        HeldEscalation::Badge(StrandedBlocker::HumanInput),
        "a box-occupied hold past the bound MUST still escalate — this is the arm the stuck \
         counter used to silence entirely"
    );
    assert_eq!(
        held_escalation(WriteAdmission::HoldQuestion, held, long_past, bound, false),
        HeldEscalation::Badge(StrandedBlocker::QuestionStale),
        "and a question hold escalates naming the staleness hypothesis"
    );
}

#[test]
fn the_escalation_names_the_gate_that_is_actually_blocking() {
    // The blocked gate decides which sentence the human reads — never whether
    // they hear anything at all. Swapping these two would tell someone to go
    // answer a question when the real blocker is their own half-typed line.
    let bound = 1_000u64;
    let (held, now) = (0u64, 5_000u64);
    assert_eq!(
        held_escalation(WriteAdmission::HoldQuestion, held, now, bound, false),
        HeldEscalation::Badge(StrandedBlocker::QuestionStale)
    );
    assert_eq!(
        held_escalation(WriteAdmission::HoldBoxOccupied, held, now, bound, false),
        HeldEscalation::Badge(StrandedBlocker::HumanInput)
    );
}

#[test]
fn the_escalation_is_one_shot_and_comes_down_when_the_pane_recovers() {
    let bound = 1_000u64;
    let (held, now) = (0u64, 5_000u64);

    assert_eq!(
        held_escalation(WriteAdmission::HoldQuestion, held, now, bound, true),
        HeldEscalation::None,
        "already badged — must not re-badge. `mark_stranded` audits on every call and the \
         drainer polls every QUEUE_DRAIN_POLL, so a lost one-shot is an audit line every two \
         seconds for as long as the hold stands"
    );
    assert_eq!(
        held_escalation(WriteAdmission::Go, held, now, bound, true),
        HeldEscalation::Clear,
        "the pane becoming writable is what ends the episode — the badge must come down"
    );
    // #563 changed both of the assertions below, and it is worth being exact
    // about why rather than just editing the expectations: as written they
    // PINNED the invisible window this issue is about. The first said a
    // writable pane has nothing to clear (true only while the chip did not
    // exist); the second said a hold inside the bound reports nothing at all,
    // which is the defect stated as a property. Neither guarantee is lost —
    // "no spurious audit" is now enforced at the caller's guarded clear, and
    // "the BADGE is for a hold that outlasted the bound" is asserted directly
    // above and in `a_held_pane_is_reported_from_the_first_poll...`.
    assert_eq!(
        held_escalation(WriteAdmission::Go, held, now, bound, false),
        HeldEscalation::Clear,
        "a writable pane clears whether or not it was ever badged — a chip raised inside the \
         bound has to come down too, and only the caller knows what it put up"
    );
    assert_eq!(
        held_escalation(WriteAdmission::HoldQuestion, held, held + bound - 1, bound, false),
        HeldEscalation::Chip(HeldReason::InteractiveQuestion),
        "a young hold is ordinary — but ordinary is not invisible: it chips, and only the \
         escalation waits for the bound"
    );
}

#[test]
fn the_preenter_gate_declines_over_human_content_and_passes_an_empty_box() {
    // rev-12 B1: the single most safety-critical line this PR adds. Inline in
    // `deliver_now` it was undeletable-by-test — nothing can construct
    // `deliver_now` headless (it needs a concrete `Wry` AppHandle), so removing
    // the gate kept the suite green. Extracted, it drives against a real
    // fake-child-backed `PtyManager` with occupancy moved through the real
    // `note_user_input` path.
    let pm = PtyManager::default();
    let pty_id = 534;
    pm.register_fake_for_test(pty_id, b"idle input box, nothing pending");

    assert!(
        preenter_admission(&pm, pty_id).go(),
        "an empty box must not block the Enter — otherwise every delivery strands"
    );

    pm.note_user_input(pty_id, "mid-thought", true);
    assert_eq!(
        preenter_admission(&pm, pty_id),
        WriteAdmission::HoldBoxOccupied,
        "human characters outstanding at the moment of Enter must withhold it — this Enter \
         would submit THEIR line, which is #510's absolute"
    );

    // Their own Enter empties the box; ours becomes safe again.
    pm.note_user_input(pty_id, "\r", true);
    assert!(
        preenter_admission(&pm, pty_id).go(),
        "the gate is a wait, not a permanent strand"
    );
}

#[test]
fn a_closed_pane_does_not_strand_the_preenter_gate() {
    // `unwrap_or(false)`, deliberately the opposite of `flush_stranded_text`'s
    // `unwrap_or(true)`: a closed pane has no box and nobody typing into it, so
    // there is nothing to protect, and the write immediately after fails and
    // audits `prompt-failed` on its own — the pre-existing, more informative
    // behaviour. Flipping this to fail-safe would convert a dead pane into an
    // endless enqueue/retry cycle.
    let pm = PtyManager::default();
    assert!(
        preenter_admission(&pm, 9_999).go(),
        "an unregistered/closed pty must not be treated as an occupied box"
    );
}

#[test]
fn the_stale_question_badge_never_claims_to_know_which_state_the_pane_is_in() {
    // The whole argument for badging instead of releasing is that loomux
    // CANNOT tell a live dialog from an answered one that has not scrolled
    // away. The wording must therefore name both branches and give an action
    // that is safe under either — a badge that asserted "this question is
    // stale" would be exactly the unbacked claim the repo's own lessons file
    // calls a defect.
    let d = stranded_detail("w-3", Some(StrandedBlocker::QuestionStale));
    assert!(d.contains("w-3"), "got: {d}");
    assert!(
        d.contains("answer it if one is on screen"),
        "must stay correct for the LIVE-dialog branch, got: {d}"
    );
    assert!(
        d.contains("stale"),
        "must name the staleness hypothesis for the other branch, got: {d}"
    );
    assert_eq!(
        StrandedBlocker::QuestionStale.as_str(),
        "question-hold-stale",
        "its own audit token, greppable apart from the live-question blocker"
    );
    assert_ne!(
        StrandedBlocker::QuestionStale.as_str(),
        StrandedBlocker::Question.as_str(),
        "a stale-reading badge and a genuine live-question badge are different facts"
    );
}

// ---------- #420 rev-19 R3: real-PTY wiring tests ----------
//
// `flush_stranded_text`/`record_aborted_preenter_outcome` are the EXACT
// functions `deliver_prompt` calls (not reimplementations of their logic) —
// these tests drive them against a REAL `PtyManager` backed by a real ConPTY
// child (`PtyManager::register_fake_for_test`, pty.rs), with no real Tauri
// AppHandle (unavailable headless) and no real agent CLI (CLAUDE.md
// constraint 3: the fake's child is a bare `cmd`/`sh` no-op). Mutating either
// function to bypass its check reds one of these.

#[test]
fn flush_stranded_text_does_not_enter_when_a_question_is_showing() {
    let pm = PtyManager::default();
    let captured = pm.register_fake_for_test(1, FIX_COPILOT_ASK.as_bytes());

    let fired = flush_stranded_text(&pm, 1, Some(false), false, b"\r", Vec::new());

    assert!(!fired, "must not press Enter while a question is on screen");
    assert!(
        captured.lock().unwrap().is_empty(),
        "no bytes should have reached the pty: {:?}",
        captured.lock().unwrap()
    );
}

#[test]
fn flush_stranded_text_enters_when_no_question_is_showing() {
    let pm = PtyManager::default();
    let captured = pm.register_fake_for_test(2, b"ordinary streaming output, nothing pending");

    let fired = flush_stranded_text(&pm, 2, Some(false), false, b"\r", Vec::new());

    assert!(fired, "no question on screen — the stranded flush must fire");
    assert_eq!(&*captured.lock().unwrap(), b"\r");
}

// ---------- #532: the drain must WAIT for the box, then flush ----------
//
// Same real-PTY discipline as the block above, and the same reason for it:
// these drive the EXACT functions the drainer calls, and occupancy is moved
// through `PtyManager::note_user_input` — the real path a keystroke takes from
// the frontend's `write_pty` — rather than by poking a counter, so
// `input_pending` behaves here exactly as it does in production. Mutating the
// occupancy check out of `should_flush_before_paste_now` reds both.

#[test]
fn the_drain_press_declines_over_a_line_the_human_left_before_our_submit() {
    let pm = PtyManager::default();
    let pty_id = 532;
    let captured = pm.register_fake_for_test(pty_id, b"idle input box, nothing pending");
    let last_delivery: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());

    // ORDERING IS THE POINT. The human types a line and leaves it sitting
    // BEFORE our delivery records its submit, so `human_typed_since`
    // (`last_user_input_ms > submit_sent_ms`) reads FALSE — the timestamp
    // compare is structurally blind to content that predates our own submit.
    // No question is on screen either. Every pre-#532 gate says "flush".
    pm.note_user_input(pty_id, "half a thought", true);
    record_aborted_preenter_outcome(&last_delivery, pty_id, "w-1".to_string(), None);
    assert_eq!(
        pm.input_pending(pty_id),
        Some(true),
        "precondition: the human's characters are outstanding in the box"
    );

    let action = drain_stranded_submit(&pm, &last_delivery, "w-1".to_string(), pty_id, b"\r", Vec::new());

    assert_eq!(
        action,
        StrandedMarkerAction::Retry(queue::EnqueueReason::BoxOccupied),
        "the queue's press must decline while human content sits in the box — this is the \
         #510 rule read at the moment of Enter rather than inherited from a timestamp. #813: \
         it must also NAME the gate that declined, instead of the hardcoded question the \
         drainer used to report for every decline"
    );
    assert!(
        captured.lock().unwrap().is_empty(),
        "no bytes should have reached the pty: {:?}",
        captured.lock().unwrap()
    );
}

#[test]
fn the_drain_press_fires_once_the_box_is_empty_again() {
    // The other half of "wait, THEN flush": declining must not become a new
    // way to strand a delivery forever. Once the human's line is gone the same
    // press fires, unchanged.
    //
    // `human_typed_since` is passed as the caller computes it. On this branch
    // that caller still inlines the unbounded timestamp latch #518 found, so a
    // human keystroke after our submit pins it true; #528's bounded
    // `human_input_block` is what releases it again on positive evidence, and
    // THIS PR is what makes that release safe — the press re-reads occupancy at
    // the moment it fires rather than trusting the bound alone.
    let pm = PtyManager::default();
    let pty_id = 533;
    let captured = pm.register_fake_for_test(pty_id, b"idle input box, nothing pending");

    pm.note_user_input(pty_id, "half a thought", true);
    assert_eq!(pm.input_pending(pty_id), Some(true));
    assert!(
        !flush_stranded_text(&pm, pty_id, Some(false), false, b"\r", Vec::new()),
        "still occupied — still declining"
    );

    // The human submits their own line. `classify_human_input` reads this as
    // `Submit`, which zeroes the occupancy counter outright (#111/#171).
    pm.note_user_input(pty_id, "\r", true);
    assert_eq!(
        pm.input_pending(pty_id),
        Some(false),
        "precondition: the box is empty again"
    );

    assert!(
        flush_stranded_text(&pm, pty_id, Some(false), false, b"\r", Vec::new()),
        "box empty, no question, previous delivery unconfirmed — the withheld Enter must \
         finally land, or the occupancy gate would be a permanent strand rather than a wait"
    );
    assert_eq!(&*captured.lock().unwrap(), b"\r");
}

#[test]
fn record_aborted_preenter_outcome_makes_the_next_deliverys_flush_actually_fire() {
    // rev-19 R3(b): chains the REAL recorder (`record_aborted_preenter_
    // outcome`), the REAL reader pattern `deliver_prompt` uses
    // (`prev.as_ref().map(|o| o.confirmed)`, mirrored here as `recorded_
    // confirmed`), and the REAL flush function — not a literal `false` a
    // test fabricates itself (rev-19's "tautology" finding against the
    // previous round's version of this test). What this DOES pin: that
    // `record_aborted_preenter_outcome`'s own insert is correctly read back
    // by `recorded_confirmed` and correctly drives `flush_stranded_text` to
    // fire — i.e. the recorder's behavior and the downstream consequence,
    // exercised through the real extraction rather than a hand-picked bool.
    // rev-19 n1 correction: what this does NOT pin is that `deliver_prompt`
    // itself calls `record_aborted_preenter_outcome` at its one call site —
    // deleting THAT line reds no test (confirmed by trying it and
    // reverting); that gap is the disposition table's own honest residual,
    // not something this test closes.
    let last_delivery: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
    let pty_id = 3;

    record_aborted_preenter_outcome(&last_delivery, pty_id, "w-1".to_string(), None);

    let prev_confirmed = recorded_confirmed(&last_delivery, pty_id);
    assert_eq!(prev_confirmed, Some(false), "an aborted pre-Enter delivery must record itself as unconfirmed");

    let pm = PtyManager::default();
    let captured = pm.register_fake_for_test(pty_id, b"idle input box, nothing pending");
    let fired = flush_stranded_text(&pm, pty_id, prev_confirmed, false, b"\r", Vec::new());

    assert!(
        fired,
        "the outcome recorded on a pre-Enter abort must make the NEXT delivery's flush actually press Enter"
    );
    assert_eq!(&*captured.lock().unwrap(), b"\r");
}

// ---------- #454: supersession during a re-send's in-flight window ----------
//
// #451 B1's supersession rule fires off the NEWER delivery's outcome insert,
// which lands at the END of its confirm window (under a second when its hook
// confirms in-window, ~9s worst case). The newer delivery's own `promptsubmit`
// record exists almost immediately after its Enter, so a stale monitor could
// read "still mine" from the ledger and then resolve ITSELF off the newer
// delivery's record -- a misattributed `delivery-confirmed-late`, plus a "no
// re-send needed" correction about the re-send that is the only reason
// anything landed.
//
// The fix is an ordering: `record_inflight_delivery` claims the pane in the
// ledger BEFORE the Enter, and the monitor takes its ledger observation AFTER
// its hook read (`observe_ledger`). These tests drive those REAL functions,
// chained into `late_monitor_tick` -- the same real decision function the
// monitor thread calls -- through #454's own interleaving. The exhaustive
// proof that no OTHER interleaving misattributes is `queue.rs`'s
// `supersession_race_property`, with its own mutation controls.
//
// Honest residual, stated the way rev-19 n1 stated the same one for
// `record_aborted_preenter_outcome`: these pin the recorder, the observer and
// the decision, not that `deliver_now` calls the recorder at its one call
// site. Deleting THAT line is caught by the property model's
// `claiming_the_ledger_only_at_the_outcome_reproduces_the_454_race`, which
// models exactly that deletion.

#[test]
fn a_resends_inflight_claim_supersedes_the_stale_monitor_before_its_own_record_can_be_seen() {
    // #454's exact interleaving, in order.
    let pty = 4541u32;
    let d1_ms = 1_000u64;
    let d2_ms = 9_000u64;
    let last_delivery = ledger_with!(pty, d1_ms);

    // (1) Before the re-send: D1's monitor owns the pane, and a hook record
    // matching ITS baseline is genuinely its own -- it must be free to
    // resolve. This is the behavior #454's fix must not break.
    let before = observe_ledger(&last_delivery, pty, d1_ms);
    assert_eq!(
        before,
        LedgerView { superseded: false, outstanding: true, newer_confirmed: false },
        "an unresolved delivery still owns its pane"
    );
    assert_eq!(
        late_monitor_tick(before.superseded, PromptLandedMatch::Existence, true, false, false, false),
        MonitorAction::Confirm { merged: false, correction: true },
        "before any newer delivery, a late record is this delivery's own and corrects its Failed verdict",
    );

    // (2) The re-send presses Enter -- but claims the pane FIRST. This is the
    // single line `deliver_now` gained: everything the Enter causes,
    // including the record D1's monitor would otherwise match, happens after
    // this point.
    record_inflight_delivery(&last_delivery, pty, d2_ms, "orrerix".to_string(), None);

    // (3) D1's monitor's next tick, taken in the real order: hook first, then
    // ledger. Any record it can see now is checked against a ledger that
    // already names the re-send.
    let after = observe_ledger(&last_delivery, pty, d1_ms);
    assert_eq!(
        after,
        LedgerView { superseded: true, outstanding: false, newer_confirmed: false },
        "the re-send owns the pane from its Enter, not from its outcome insert -- and it is not \
         confirmed yet, so nothing may be reported as landed on its behalf",
    );
    assert_eq!(
        late_monitor_tick(after.superseded, PromptLandedMatch::Existence, true, false, false, false),
        MonitorAction::Superseded,
        "the stale monitor must exit writing and notifying nothing -- no misattributed \
         delivery-confirmed-late, no \"no re-send needed\" correction about the re-send itself",
    );
}

#[test]
fn without_the_inflight_claim_the_resends_own_record_resolves_the_wrong_delivery() {
    // The mutation control at the real-code level: the pre-#454 shape, where
    // the re-send claims the pane only when its confirm window resolves. The
    // ledger is untouched between its Enter and that insert, so the identical
    // tick that exits cleanly above instead confirms -- and, because this
    // monitor had already declared `Failed`, sends the correction notice.
    //
    // If this test ever stops reproducing the confirm, the test above has
    // stopped depending on the claim and is passing vacuously.
    let pty = 4542u32;
    let d1_ms = 1_000u64;
    let last_delivery = ledger_with!(pty, d1_ms);

    // The re-send's Enter went out; its outcome insert has not landed yet.
    // Nothing wrote the ledger, so the stale monitor still reads "mine".
    let view = observe_ledger(&last_delivery, pty, d1_ms);
    assert!(!view.superseded, "the pre-fix ledger cannot see a delivery that is merely in flight");
    assert_eq!(
        late_monitor_tick(view.superseded, PromptLandedMatch::Existence, true, false, false, false),
        MonitorAction::Confirm { merged: false, correction: true },
        "this is the #454 defect: the re-send's own record resolves the OLD delivery and announces \
         a correction -- exactly backwards, since the re-send is why anything landed",
    );
}

#[test]
fn an_inflight_claim_reads_as_unconfirmed_so_the_next_delivery_still_flushes() {
    // The claim is written before anything is known about the outcome, so it
    // must read as UNCONFIRMED -- the conservative value. If the pane dies or
    // the app exits mid-window, the ledger's residue then still tells the next
    // delivery to flush a possibly-stranded box rather than pasting on top of
    // it. Chained through the real reader and the real flush, the same way
    // `record_aborted_preenter_outcome`'s own test is.
    let pty = 4543u32;
    let last_delivery = ledger_with!(pty, 7_000);

    let prev_confirmed = recorded_confirmed(&last_delivery, pty);
    assert_eq!(prev_confirmed, Some(false), "an in-flight delivery has not landed yet");

    let pm = PtyManager::default();
    let captured = pm.register_fake_for_test(pty, b"idle input box, nothing pending");
    assert!(
        flush_stranded_text(&pm, pty, prev_confirmed, false, b"\r", Vec::new()),
        "an in-flight claim left behind by a dead delivery must still arm the next delivery's flush"
    );
    assert_eq!(&*captured.lock().unwrap(), b"\r");
}

#[test]
fn a_stranded_badge_survives_an_inflight_claim_and_drops_on_a_confirmed_one() {
    // `newer_confirmed` is what the monitor's `Superseded` arm drops the #496
    // attention badge on, and it must stay FALSE while the newer delivery is
    // merely in flight: an unconfirmed successor is not evidence the pane is
    // unwedged. It flips true once that delivery actually lands -- here
    // through `drain_stranded_submit`, the real replay path, which records a
    // confirmed outcome under its own fresh `submit_sent_ms`.
    let pty = 4544u32;
    let d1_ms = 1_000u64;
    let last_delivery = ledger_with!(pty, d1_ms);

    record_inflight_delivery(&last_delivery, pty, 9_000, "orrerix".to_string(), None);
    assert_eq!(
        observe_ledger(&last_delivery, pty, d1_ms),
        LedgerView { superseded: true, outstanding: false, newer_confirmed: false },
        "a badge must not come down on a delivery that has not landed yet"
    );

    let pm = PtyManager::default();
    pm.register_fake_for_test(pty, STRANDED_TAIL.as_bytes());
    assert_eq!(
        drain_stranded_submit(&pm, &last_delivery, "orrerix".to_string(), pty, b"\r", Vec::new()),
        StrandedMarkerAction::Press
    );
    assert!(
        observe_ledger(&last_delivery, pty, d1_ms).newer_confirmed,
        "once a newer delivery is recorded confirmed, the pane is demonstrably unwedged and the \
         stale monitor drops the badge on its way out"
    );
}

// ---------- #496 PR-A: the phantom-input-stamp fix ----------
//
// `write_pty` used to stamp `user_input_ms = now` UNCONDITIONALLY, before
// `classify_human_input` ever ran. xterm answers a program's terminal
// queries — colour probes, focus reports, device-attribute/cursor-position
// replies — through that exact same write path with NO human present at all
// (#179's boot-time instance; copilot also emits these mid-session on
// redraw/focus churn). Those auto-replies stamped the clock just like a
// keystroke, and that one clock feeds the autonomous idle tick's quiet
// window, the stranded-text flush, the submit retries, and Tier-1 box
// confirmation — so a copilot pane that only ever emits auto-replies could
// wedge all four forever with the human never having touched the keyboard
// (#496's reported symptom: autonomous mode halts until a human presses
// Enter). `PtyManager::note_user_input` (the exact code `write_pty` now
// calls) gates the stamp on `classify_human_input`/`box_occupancy_delta`
// reading actual keystroke evidence, reusing the #179 scanner rather than
// adding a second classifier.

#[test]
fn terminal_query_replies_do_not_stamp_user_input() {
    let pm = PtyManager::default();
    let id = 496_01;
    pm.register_fake_for_test(id, b"");
    assert_eq!(pm.last_user_input_ms(id), Some(0), "a fresh fake pty stamps nothing at all");

    for (name, reply) in [
        ("OSC 11 colour query reply (BEL-terminated)", "\x1b]11;rgb:0d0d/1111/1717\x07"),
        ("OSC 10 colour query reply (ST-terminated)", "\x1b]10;rgb:f0f6/f0f6/fcfc\x1b\\"),
        ("xterm focus-in report", "\x1b[I"),
        ("DA (device attributes) reply", "\x1b[?64;1;2;6;9;15;18;21;22c"),
        ("CPR (cursor position) reply", "\x1b[24;80R"),
    ] {
        // `human_origin: true` deliberately — this test is about the BYTE-SHAPE
        // gate (#496 PR-A) and must keep failing if that gate is removed, so
        // #518's origin bit is handed the least helpful value here. Both
        // conditions are ANDed; each gets its own test.
        pm.note_user_input(id, reply, true);
        assert_eq!(
            pm.last_user_input_ms(id),
            Some(0),
            "{name} is an xterm auto-reply with no human present — must NOT stamp user_input_ms \
             (this is the #496 regression: base stamps unconditionally, so this assertion fails there)"
        );
    }
}

#[test]
fn real_keystrokes_still_stamp_user_input() {
    // Green companions for the test above: a fix that stopped the clock
    // updating for genuine keystrokes would be far worse than #496's bug —
    // it would make every human-typing signal in the delivery pipeline
    // (the #111 paste guard, the question hold, the idle tick) blind.
    for (name, data) in [("plain text", "a"), ("Enter / CR", "\r"), ("backspace", "\u{7f}")] {
        let pm = PtyManager::default();
        let id = 496_02;
        pm.register_fake_for_test(id, b"");
        assert_eq!(pm.last_user_input_ms(id), Some(0));

        pm.note_user_input(id, data, true);

        assert_ne!(
            pm.last_user_input_ms(id),
            Some(0),
            "{name} is a real keystroke — user_input_ms must still be stamped"
        );
    }
}

// ---------- #518: the structural half of the keystroke-evidence gate ----------
//
// #496 PR-A (above) classifies by BYTE SHAPE. That is a pattern match against
// an OPEN set of terminal auto-reply shapes — it covers everything #179
// catalogued, but #496's own plan §7 closed with "which copilot emission
// recurs mid-session" unanswered, which is why `phantom-input-gated` exists as
// a breadcrumb rather than an answer. #518 is that residue firing in
// production: a copilot orchestrator's prompt sat unsubmitted behind a
// "held: user typing" state with nobody at the keyboard.
//
// The frontend already solved this exact problem for its own `firstInputMs`
// (#440 B2-R) and explicitly did NOT do it with a better `onData` filter — it
// took the signal from `term.onKey` and the two `term.paste()` sites, which
// are unreachable by anything the terminal manufactures for itself. The origin
// bit carries that same guarantee across the IPC boundary. The two conditions
// are ANDed, so neither test below can pass by the other's mechanism.

#[test]
fn a_non_human_origin_write_never_stamps_however_it_reads() {
    // The property the byte-shape gate CANNOT provide: this write is
    // indistinguishable from typed content by shape — plain printable text,
    // `HumanInput::Content`, a positive occupancy delta — and it still must
    // not stamp, because the frontend says no key was pressed and no paste
    // happened. This is what makes the gate closed over an open set rather
    // than over a catalogue: it needs no prediction of what copilot's TUI
    // might emit next.
    //
    // Mutation this is here to catch: drop `human_origin &&` from
    // `note_user_input`'s `keystroke_like` and this reddens immediately.
    let pm = PtyManager::default();
    let id = 518_01;
    pm.register_fake_for_test(id, b"");

    for (name, data) in [
        ("printable body of an unmodelled auto-reply", "11;rgb:0d0d/1111/1717"),
        ("a reply fragmented across writes, tail half in isolation (#496 N3)", "rgb:f0f6/f0f6/fcfc\x1b\\"),
        ("a bare CR the terminal emitted itself", "\r"),
    ] {
        pm.note_user_input(id, data, false);
        assert_eq!(
            pm.last_user_input_ms(id),
            Some(0),
            "{name} did not come from a key or a paste — must NOT stamp user_input_ms, \
             whatever its bytes look like"
        );
    }
}

#[test]
fn a_human_origin_keystroke_still_stamps() {
    // The green companion, and the reason the origin bit is ANDed rather than
    // substituted for the shape gate: a fix that made the clock stop tracking
    // real typing would blind every human-protection guard in the delivery
    // pipeline, which is far worse than the bug #518 is about.
    let pm = PtyManager::default();
    let id = 518_02;
    pm.register_fake_for_test(id, b"");

    pm.note_user_input(id, "please hold on", true);

    assert_ne!(
        pm.last_user_input_ms(id),
        Some(0),
        "a genuine keystroke, from a real key event, must still stamp the clock"
    );
}

#[test]
fn an_origin_bit_alone_does_not_resurrect_a_shape_gated_write() {
    // Both halves are necessary, and this pins the direction the other tests
    // cannot: an arrow key IS a genuine key event (`human_origin: true`) and
    // still must not stamp, because #496 PR-A's tradeoff — pure-`Neutral`,
    // zero-delta input does not defer anything — is deliberate and untouched
    // by #518. A future edit that replaced the shape gate with the origin bit
    // instead of ANDing them would pass every other test in this section.
    let pm = PtyManager::default();
    let id = 518_03;
    pm.register_fake_for_test(id, b"");

    pm.note_user_input(id, "\x1b[A", true); // up-arrow: a real key, no content

    assert_eq!(
        pm.last_user_input_ms(id),
        Some(0),
        "#496 PR-A's Neutral/zero-delta tradeoff must survive #518 — the two gates are ANDed"
    );
}

#[test]
fn phantom_gate_tick_throttles_repeated_gated_writes_per_pane() {
    // #496 N1 review: `phantom-input-gated` used to fire on EVERY gated
    // write — a human wheel-scrolling or holding an arrow key could hit tens
    // per second, each a breadcrumb file-open, and the flood would dilute
    // (and eventually rotate away) the mid-session copilot signal the
    // breadcrumb exists to capture. `phantom_gate_tick` is the pure decision
    // this throttles with — no filesystem involved (#496 N4 review: this is
    // the one seam that COULD be tested purely, unlike the breadcrumb's own
    // disk write).
    const INTERVAL_MS: u64 = 5_000; // must match pty.rs's PHANTOM_GATE_BREADCRUMB_MIN_INTERVAL_MS

    // First tick for a pane always emits (nothing to throttle against yet):
    // `last_emit_ms` is `None` ("never emitted"), deliberately not a `0`
    // sentinel — see `phantom_gate_tick`'s doc comment for why a bare `0`
    // would only work by coincidence of real epoch-ms magnitudes, which
    // small/synthetic test timestamps like this one don't have.
    let (emit, state) = phantom_gate_tick((0, None), 1_000);
    assert_eq!(emit, Some(1), "the first gated write for a pane must emit immediately");
    assert_eq!(state, (0, Some(1_000)), "an emission resets the counter and records the emit time");

    // A second tick milliseconds later (well inside the interval) must NOT
    // emit — it just accumulates.
    let (emit, state) = phantom_gate_tick(state, 1_010);
    assert_eq!(emit, None, "a gated write inside the throttle interval must not emit");
    assert_eq!(state, (1, Some(1_000)), "the count accumulates; last_emit_ms is unchanged");

    // A third tick, still inside the interval, accumulates further.
    let (emit, state) = phantom_gate_tick(state, 2_500);
    assert_eq!(emit, None);
    assert_eq!(state, (2, Some(1_000)), "count keeps accumulating across multiple throttled writes");

    // A fourth tick, now past the interval, emits — and reports EVERY gated
    // write coalesced since the last emission (this one plus the two that
    // were throttled), not just itself, so the log line still carries the
    // rate.
    let (emit, state) = phantom_gate_tick(state, 1_000 + INTERVAL_MS);
    assert_eq!(emit, Some(3), "the write that crosses the interval emits, counting every write since the last emission");
    assert_eq!(state, (0, Some(1_000 + INTERVAL_MS)), "the counter resets and the clock re-bases on this emission");

    // Exactly AT the interval boundary counts as due (>=, not >) — a
    // deliberate off-by-one check.
    let (emit, _) = phantom_gate_tick((0, Some(1_000)), 1_000 + INTERVAL_MS);
    assert_eq!(emit, Some(1), "exactly at the interval boundary must emit, not wait for one more tick");
    let (emit, _) = phantom_gate_tick((0, Some(1_000)), 1_000 + INTERVAL_MS - 1);
    assert_eq!(emit, None, "one millisecond short of the interval must still throttle");

    // Different panes are independent: seeding a fresh (0, None) state (as a
    // brand-new pty id would have) emits immediately regardless of what
    // another pane's clock is doing.
    let (emit, _) = phantom_gate_tick((0, None), 1_000 + INTERVAL_MS);
    assert_eq!(emit, Some(1), "a different pane's state is independent of this one's throttle");
}

// ---------- #496 PR-C: stranded-delivery self-heal + attention badge ----------
//
// The wedge these pin: a delivery ends `Failed` (60s output-quiet, no
// question, nothing confirmed) with the pasted prompt still sitting in the
// box. Before this change nothing ACTED on that — for an orchestrator target
// both notices are suppressed by design, and an idle group has no next
// delivery whose pre-paste flush would press the withheld Enter, so a human
// had to notice and press it. The guarantee now: self-heal through the
// ordered queue, or a visible badge, never silence.

/// The pane tail a stranded delivery leaves behind: our prompt echoed into
/// the input box, nothing after it. `box_holds_paste` reads this as "still
/// holding" — Tier 1's own signal, used here exactly as `deliver_prompt`
/// uses it.
pub(crate) const STRANDED_TAIL: &str =
    "  ⎿  done\n\n> please post the status roll-up on #496\n";

#[test]
fn stranded_selfheal_action_precedence_puts_human_content_first() {
    // The whole decision table, in precedence order. Written as a table
    // because the ORDER is the property: a future edit that moves the
    // human-content check below the "is our text still there" check would
    // make loomux press Enter on a line a person is mid-way through typing.
    // #559: `holds` stays a bool here — every row of this table is about an
    // INFORMATIVE box read, which is what the parameter meant before the
    // three-state `BoxReading` existed. The third state has its own tests
    // below; keeping this table's shape means the precedence property it
    // pins is compared against the same rows it always was.
    let heal = |ledger, typed, question, holds: bool, used| {
        stranded_selfheal_action(
            ledger,
            typed,
            question,
            if holds { BoxReading::Holds } else { BoxReading::NotHolding },
            used,
            STRANDED_SELFHEAL_MAX_HEALS,
        )
    };

    // The ledger — the durable artifact — is consulted before any pane
    // reading: an outcome that is no longer this delivery's, or is already
    // confirmed, means there is nothing to actuate at all.
    assert_eq!(heal(false, false, false, true, 0), StrandedAction::Resolved);
    assert_eq!(
        heal(false, true, true, true, 9),
        StrandedAction::Resolved,
        "a resolved delivery never badges, whatever the pane looks like"
    );

    // The one absolute: never submit over human-typed content. It outranks
    // even the heal budget and the question guard.
    assert_eq!(
        heal(true, true, false, true, 0),
        StrandedAction::Attention(StrandedBlocker::HumanInput),
        "a human keystroke since our submit must badge, never heal"
    );
    assert_eq!(
        heal(true, true, true, false, 9),
        StrandedAction::Attention(StrandedBlocker::HumanInput),
        "human content outranks every other blocker"
    );

    // #420's question guard: a live question owns the Enter key.
    assert_eq!(
        heal(true, false, true, true, 0),
        StrandedAction::Attention(StrandedBlocker::Question)
    );

    // Nothing identifiable left in the box → badge, don't guess.
    assert_eq!(
        heal(true, false, false, false, 0),
        StrandedAction::Attention(StrandedBlocker::NotHolding)
    );

    // The bound.
    assert_eq!(
        heal(true, false, false, true, STRANDED_SELFHEAL_MAX_HEALS),
        StrandedAction::Attention(StrandedBlocker::Exhausted),
        "the heal budget must be spent-able, and spending it badges instead of re-pressing"
    );

    // Everything clear → heal.
    assert_eq!(heal(true, false, false, true, 0), StrandedAction::SelfHeal);
}

#[test]
fn stranded_selfheal_admits_a_submit_through_the_queue_ahead_of_pending_text() {
    // The ordering property: the stranded text is ALREADY in the box, so the
    // re-submit must drain before anything queued behind it — otherwise the
    // next payload pastes on top of an unsubmitted prompt and the two merge.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 496u32;

    reg.enqueue_text(&g.id, &w.id, "orrerix", "the next brief", pty, queue::EnqueueReason::Arrival)
        .unwrap();

    let healed = reg.actuate_stranded(&g.id, &w.id, "orrerix", pty, StrandedAction::SelfHeal);
    assert!(healed, "a clear self-heal decision must actually admit a submit");

    let snap = reg.queue_snapshot(pty);
    assert_eq!(snap.len(), 2, "the heal is an extra queue entry, not a replacement");
    assert_eq!(
        snap[0].payload,
        queue::QueuedPayload::StrandedSubmit,
        "the re-submit must drain FIRST — nothing may paste over stranded text"
    );
    assert_eq!(snap[1].payload.text(), Some("the next brief"), "queued text keeps its place behind it");

    // Audited, so a self-heal is never a silent write into someone's pane.
    let audits = reg.audit_log(&g.id);
    assert_eq!(
        audits.iter().filter(|e| e.action == "stranded-selfheal-submit").count(),
        1,
        "every self-heal attempt must be audited exactly once"
    );
    // ...and loud: the human sees the pane wedged even though loomux is
    // recovering it (the badge clears once the ledger confirms).
    let note = reg.stranded_note(&w.id).expect("a self-heal must still raise the badge");
    assert_eq!(note.blocker, None, "an in-flight heal badges as 'loomux is re-sending it'");
}

#[test]
fn a_selfheal_marker_is_audited_as_a_selfheal_and_never_as_a_question() {
    // #560's residual. BOTH marker pushes go through the one helper
    // (`push_stranded_front_locked`), which hardcoded
    // `EnqueueReason::Question` — true of the drainer's push (a pre-Enter
    // question really did decline that Enter) and FALSE of this one, whose
    // trigger is a pane that has gone QUIET with our text stranded in its box.
    //
    // The stakes are the audit line, and they are not cosmetic: a self-heal is
    // the one write loomux makes on its OWN initiative into somebody's pane,
    // and `delivery-queued` is the only record that it happened. Claiming
    // `question` sent the human reconstructing that wedge to look for a dialog
    // that was never on screen — the same mislabel #532 rev-12 NB1 fixed on
    // the notice, on the one path that had no honest variant to name.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5601u32;

    assert!(
        reg.actuate_stranded(&g.id, &w.id, "orrerix", pty, StrandedAction::SelfHeal),
        "a clear self-heal decision must actually admit a marker to audit"
    );

    let queued: Vec<_> =
        reg.audit_log(&g.id).into_iter().filter(|e| e.action == "delivery-queued").collect();
    assert_eq!(queued.len(), 1, "one marker push, one line: {queued:?}");
    assert_eq!(
        queued[0].detail["marker"], json!("stranded-submit"),
        "the line under test is the marker's, not a text delivery's: {:?}", queued[0].detail
    );
    assert_eq!(
        queued[0].detail["reason"], json!("stranded-self-heal"),
        "a self-heal must name ITSELF — nothing was on screen to call a question: {:?}",
        queued[0].detail
    );

    // The same fact on the entry, which IS the on-disk record (#468): the
    // audit line and `queue.json` must never disagree about one push.
    let snap = reg.queue_snapshot(pty);
    assert_eq!(snap.len(), 1, "the heal is one queue entry");
    assert_eq!(snap[0].payload, queue::QueuedPayload::StrandedSubmit);
    assert_eq!(
        snap[0].reason.as_str(), "stranded-self-heal",
        "the persisted reason is the audited one, or forensics reads two stories"
    );
}

#[test]
fn a_marker_left_by_an_occupied_box_is_audited_as_box_occupied_not_a_question() {
    // #560's second call site, and the same defect reached the other way: the
    // self-heal push HARDCODED a reason it did not have, while the drainer's
    // push (`enqueue_stranded_front`) DROPPED one it did. `AbortedPreEnter`
    // has carried its gate's own reason since #532 rev-12 NB1 — and that gate
    // has two causes, a dialog and a human's half-typed line — but the marker
    // it produces was recorded as `question` either way.
    //
    // What that costs is a contradiction inside one event: the orchestrator's
    // notice for this abort already says "pane has human input"
    // (`queued_notice(BoxOccupied)`), so the audit line said dialog while the
    // notice said keyboard, about the same delivery, at the same instant. The
    // reader with only the log is the one who loses.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5603u32;

    // The value `run_queue_drainer`'s `AbortedPreEnter(reason)` arm forwards
    // when #532's occupancy gate is what declined the Enter. The drainer loop
    // itself needs an `AppHandle` and so is not test-drivable (this file's
    // standing convention — drive the methods it calls); this is that method.
    reg.enqueue_stranded_front(&g.id, &w.id, "orrerix", pty, queue::EnqueueReason::BoxOccupied)
        .unwrap();

    let queued: Vec<_> =
        reg.audit_log(&g.id).into_iter().filter(|e| e.action == "delivery-queued").collect();
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert_eq!(
        queued[0].detail["reason"], json!("box-occupied"),
        "a marker left by a human's own line must not be filed as a dialog: {:?}",
        queued[0].detail
    );
    assert_eq!(
        reg.queue_snapshot(pty)[0].reason.as_str(), "box-occupied",
        "and the on-disk record agrees with the audit line about the same push"
    );
}

#[test]
fn the_drainers_own_marker_still_says_question() {
    // The control for both tests above, and the reason every reason here is
    // THREADED rather than simply changed: a question gate really can be what
    // declined the Enter, and for that marker `question` was true all along. A
    // fix that made the shared helper say `stranded-self-heal` — or the
    // drainer say `box-occupied` — for everyone would just move the false
    // claim next door.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5602u32;

    reg.enqueue_stranded_front(&g.id, &w.id, "orrerix", pty, queue::EnqueueReason::Question)
        .unwrap();

    let queued: Vec<_> =
        reg.audit_log(&g.id).into_iter().filter(|e| e.action == "delivery-queued").collect();
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert_eq!(
        queued[0].detail["reason"], json!("question"),
        "the drainer's marker keeps the reason that was always true of it: {:?}",
        queued[0].detail
    );
    assert_eq!(reg.queue_snapshot(pty)[0].reason.as_str(), "question");
}

#[test]
fn a_marker_reason_survives_the_snapshot_and_an_unknown_one_costs_only_its_entry() {
    // `EnqueueReason` is a field of `QueuedDelivery`, which IS the on-disk
    // record (#468) — so adding a variant is a persisted-format change, and
    // both directions need an answer rather than a hope (the `QueuedDelivery`
    // precedent from #620/#654).
    //
    // Forward (old file, this build): every reason ever written is still
    // spelled, so nothing on disk stops parsing.
    let legacy = json!({
        "version": queue::SNAPSHOT_VERSION,
        "written_ms": 1,
        "entries": [{
            "pty_id": 9, "id": 1, "agent_id": "w-1", "from": "loomux",
            "payload": { "kind": "stranded-submit" },
            "reason": "question", "enqueued_ms": 5, "coalesced": 0,
            "group": "g-1", "to_orchestrator": false, "session_id": null,
        }],
    });
    let (back, skipped) = queue::parse_snapshot(&legacy.to_string());
    assert_eq!((back.len(), skipped), (1, 0), "a marker written before #560 must still parse: {back:?}");
    assert_eq!(back[0].delivery.reason.as_str(), "question");

    // Round trip: what this build writes for a self-heal, this build reads
    // back as the same reason — the property the tolerance below is the
    // fallback FOR, not a substitute for.
    let current = json!({
        "version": queue::SNAPSHOT_VERSION,
        "written_ms": 1,
        "entries": [{
            "pty_id": 9, "id": 2, "agent_id": "w-1", "from": "loomux",
            "payload": { "kind": "stranded-submit" },
            "reason": "stranded-self-heal", "enqueued_ms": 5, "coalesced": 0,
            "group": "g-1", "to_orchestrator": false, "session_id": null,
        }],
    });
    let (back, skipped) = queue::parse_snapshot(&current.to_string());
    assert_eq!((back.len(), skipped), (1, 0), "{back:?}");
    assert_eq!(back[0].delivery.reason.as_str(), "stranded-self-heal");

    // Backward (this build's file, an OLDER build) — the case a new variant
    // cannot fix in the past, only bound. An unknown reason has no safe
    // default (`#[serde(other)]` mapping it onto some existing reason would
    // make a downgrade silently MISLABEL the entry, which is the exact defect
    // this variant removes), so `parse_snapshot`'s per-entry tolerance skips
    // that entry, counts it, and leaves every sibling intact. Simulated with a
    // reason no build knows, because that is precisely what "stranded-self-
    // heal" looks like to a build that predates it.
    let from_the_future = json!({
        "version": queue::SNAPSHOT_VERSION,
        "written_ms": 1,
        "entries": [
            {
                "pty_id": 9, "id": 3, "agent_id": "w-1", "from": "loomux",
                "payload": { "kind": "stranded-submit" },
                "reason": "a-reason-no-build-here-knows", "enqueued_ms": 5, "coalesced": 0,
                "group": "g-1", "to_orchestrator": false, "session_id": null,
            },
            {
                "pty_id": 9, "id": 4, "agent_id": "w-1", "from": "loomux",
                "payload": { "kind": "text", "text": "the sibling that must survive" },
                "reason": "arrival", "enqueued_ms": 6, "coalesced": 0,
                "group": "g-1", "to_orchestrator": false, "session_id": null,
            },
        ],
    });
    let (back, skipped) = queue::parse_snapshot(&from_the_future.to_string());
    assert_eq!(skipped, 1, "the unreadable entry is counted, never silently absorbed");
    assert_eq!(back.len(), 1, "and it costs only itself: {back:?}");
    assert_eq!(back[0].delivery.payload.text(), Some("the sibling that must survive"));
}

#[test]
fn a_queued_stranded_submit_presses_enter_through_the_real_replay() {
    // The other half of "re-flushed through the queue": what the drainer
    // actually does with the marker this PR admits. Drives the REAL replay
    // function (`drain_stranded_submit`, the one `run_queue_drainer` calls)
    // against a real PtyManager backed by a fake child — no AppHandle, no
    // agent CLI (CLAUDE.md constraint 3).
    let pm = PtyManager::default();
    let pty = 4961u32;
    let captured = pm.register_fake_for_test(pty, STRANDED_TAIL.as_bytes());
    let last_delivery: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());
    // The ledger state a stranded delivery leaves: recorded, unconfirmed.
    record_aborted_preenter_outcome(&last_delivery, pty, "orrerix".to_string(), None);

    let action = drain_stranded_submit(&pm, &last_delivery, "orrerix".to_string(), pty, b"\r", Vec::new());

    assert_eq!(
        action,
        StrandedMarkerAction::Press,
        "the queued marker must press Enter on a quiet, question-free pane"
    );
    assert_eq!(&*captured.lock().unwrap(), b"\r", "exactly one submit, nothing else");
    assert_eq!(
        recorded_confirmed(&last_delivery, pty),
        Some(true),
        "a landed re-submit must close the ledger entry the badge clears off"
    );
}

#[test]
fn stranded_human_content_badges_and_never_submits() {
    // The not-safe path, both halves: the decision refuses to heal, and the
    // real press path refuses too if it is somehow reached. Human text in
    // the box is the one thing loomux must never merge-submit over.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 4962u32;

    let action = stranded_selfheal_action(
        true,              // ledger: still outstanding
        true,              // a human typed after our submit
        false,             // no question on screen
        BoxReading::Holds, // our text is still in the box
        0,
        STRANDED_SELFHEAL_MAX_HEALS,
    );
    let healed = reg.actuate_stranded(&g.id, &w.id, "orrerix", pty, action);

    assert!(!healed, "human content in the box must never produce a submit");
    assert_eq!(reg.queue_depth(pty), 0, "nothing may be queued for a pane loomux must not press Enter on");
    let note = reg.stranded_note(&w.id).expect("the human must be told, not silently waited on");
    assert_eq!(note.blocker, Some(StrandedBlocker::HumanInput));
    assert!(
        reg.audit_log(&g.id).iter().any(|e| e.action == "stranded-attention"),
        "the badge must be audited too — the record is how a wedge is reconstructed later"
    );

    // And the press path itself declines, so the guard does not rely on the
    // decision function being the only caller.
    let pm = PtyManager::default();
    let captured = pm.register_fake_for_test(pty, STRANDED_TAIL.as_bytes());
    let fired = flush_stranded_text(&pm, pty, Some(false), true, b"\r", Vec::new());
    assert!(!fired, "flush_stranded_text must refuse while a human has typed since our submit");
    assert!(captured.lock().unwrap().is_empty(), "no bytes may reach a pane holding human text");
}

#[test]
fn a_selfheal_never_cuts_in_front_of_a_drainer_that_owns_an_entry() {
    // The hazard this closes, and the reason the gate is a safety rule
    // rather than an optimization: a drainer inside `deliver_now` has
    // already peeked its front entry and will finish by calling
    // `pop_front_dequeued(that id)`, which pops ONLY on an id match. A
    // marker slipped in front of it makes that pop match nothing, leaving an
    // ALREADY-DELIVERED entry queued for a second, duplicate delivery.
    assert_eq!(stranded_admission_gate(true, false), Some("drainer-active"));
    assert_eq!(
        stranded_admission_gate(true, true),
        Some("drainer-active"),
        "the safety rule is checked before the idempotence one"
    );
    assert_eq!(stranded_admission_gate(false, true), Some("submit-already-queued"));
    assert_eq!(stranded_admission_gate(false, false), None, "an idle pane's queue may be fronted");

    // Wired: against the REAL registry state the real check reads.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 4964u32;
    reg.enqueue_text(&g.id, &w.id, "orrerix", "mid-delivery", pty, queue::EnqueueReason::Arrival)
        .unwrap();
    let _generation = reg.register_drainer_for_test(pty).expect("nothing holds this pty yet");
    assert!(reg.drainer_active(pty));

    let healed = reg.actuate_stranded(&g.id, &w.id, "orrerix", pty, StrandedAction::SelfHeal);

    assert!(!healed, "no heal may be admitted while a drainer owns the queue front");
    assert_eq!(reg.queue_depth(pty), 1, "the queue must be exactly as the drainer left it");
    assert_eq!(
        reg.queue_snapshot(pty)[0].payload.text(),
        Some("mid-delivery"),
        "the drainer's own entry must still be the front — a marker here would strand it"
    );
    assert!(
        reg.audit_log(&g.id).iter().any(|e| e.action == "stranded-selfheal-skipped"),
        "the skip is audited, with its reason"
    );
    // Still loud: declining to heal never means declining to tell the human.
    assert!(reg.stranded_note(&w.id).is_some(), "the badge is raised whichever mechanism presses Enter");
}

#[test]
fn stranded_selfheal_is_bounded_and_never_double_queues() {
    // Two independent bounds, because either one alone leaves a flush loop
    // available: the per-delivery heal budget, and the refusal to stack a
    // second marker behind one that has not fired yet.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 4963u32;

    assert!(reg.actuate_stranded(&g.id, &w.id, "orrerix", pty, StrandedAction::SelfHeal));
    assert_eq!(reg.queue_depth(pty), 1);

    // Budget spent: the decision no longer even asks for a heal.
    assert_eq!(
        stranded_selfheal_action(true, false, false, BoxReading::Holds, 1, STRANDED_SELFHEAL_MAX_HEALS),
        StrandedAction::Attention(StrandedBlocker::Exhausted),
        "one heal per stranded delivery — no infinite flush loop"
    );

    // Belt and braces: even a caller that ignores the budget cannot stack a
    // second Enter behind the first, and the un-fired marker does not burn
    // budget it never used.
    let healed_again = reg.actuate_stranded(&g.id, &w.id, "orrerix", pty, StrandedAction::SelfHeal);
    assert!(!healed_again, "a marker already queued must not count as a fresh heal");
    assert_eq!(reg.queue_depth(pty), 1, "a second Enter must never be stacked behind an unfired one");
    let audits = reg.audit_log(&g.id);
    assert_eq!(
        audits.iter().filter(|e| e.action == "stranded-selfheal-submit").count(),
        1,
        "only the admission that really happened is audited as a submit"
    );
    assert!(
        audits.iter().any(|e| e.action == "stranded-selfheal-skipped"),
        "the declined attempt is audited too, with its reason"
    );
}

// ---------- #517: a fresh spawn's kickoff the pane never received ----------
//
// The failure these pin is NOT the one above. #496 PR-C rescues a delivery
// whose text IS in the box with its Enter withheld. A fresh kickoff fails the
// other way: a CLI whose stdin reader has not attached yet swallows the paste
// outright, so nothing ever reaches the box, `stranded_selfheal_action`
// correctly refuses (`NotHolding` — there is no Enter to press), and before
// this change the story ended at a badge. The worker then idles with no brief
// until a human re-sends it by hand: six instances in one day, two of three
// fresh spawns on v1.1.0-beta, while a third received its brief normally.
//
// The guarantee added: a lost FRESH kickoff is re-delivered through the same
// front door — bounded, audited, and never twice.

/// A fresh spawn's task brief. Long and multi-line like the real thing, which
/// matters: a brief this size is collapsed by the CLI's input box into a
/// `[Pasted text …]` placeholder, so Tier 1 never governs a kickoff and the
/// pane-reading signals a mid-session delivery relies on are unavailable.
const KICKOFF_BRIEF: &str = "You are w-11, a worker agent.\n\nYour task:\nIssue #517 — \
     spawn-time kickoff briefs are lost intermittently. Read it, fix it, open a PR.\n";

#[test]
fn kickoff_recovery_declines_before_it_ever_re_delivers() {
    // The whole decision table, in precedence order — the ORDER is the
    // property. Two arms carry the weight: `NotAKickoff` first (nothing but
    // a fresh spawn's brief may ever be re-sent by this path) and
    // `TurnStarted` ahead of the budget (a brief that landed must not be
    // re-sent even when budget remains).
    // `ready_observed: true` throughout except where it is the subject —
    // the ordinary case, where the boot wait watched the CLI go ready.
    let act = |kickoff, ledger, typed, question, growth, used| {
        kickoff_recovery_action(
            kickoff, ledger, typed, question, growth,
            KICKOFF_TURN_EVIDENCE_BYTES, true, used, KICKOFF_REDELIVERY_MAX,
        )
    };

    // Not a fresh kickoff — checked first, so no mid-session prompt or
    // resume re-sync can ever be re-pasted by this path however lost it looks.
    assert_eq!(
        act(false, true, false, false, 0, 0),
        KickoffRecovery::Decline(KickoffDecline::NotAKickoff)
    );

    // The durable ledger outranks every pane reading, exactly as it does in
    // `stranded_selfheal_action`.
    assert_eq!(
        act(true, false, false, false, 0, 0),
        KickoffRecovery::Decline(KickoffDecline::Resolved)
    );

    // Loomux does not act on a pane a person is using.
    assert_eq!(
        act(true, true, true, false, 0, 0),
        KickoffRecovery::Decline(KickoffDecline::HumanInput)
    );
    assert_eq!(
        act(true, true, false, true, 0, 0),
        KickoffRecovery::Decline(KickoffDecline::Question)
    );

    // The anti-double-delivery guard, and its precedence over the budget: a
    // pane that produced a turn's worth of output after our submit received
    // the brief, whatever the confirmation tiers saw. This is the live
    // counter-example (rev-9) expressed as a rule.
    assert_eq!(
        act(true, true, false, false, KICKOFF_TURN_EVIDENCE_BYTES, 0),
        KickoffRecovery::Decline(KickoffDecline::TurnStarted),
        "a kickoff that visibly started a turn must never be re-delivered"
    );

    // The bound.
    assert_eq!(
        act(true, true, false, false, 0, KICKOFF_REDELIVERY_MAX),
        KickoffRecovery::Decline(KickoffDecline::Exhausted),
        "the re-delivery budget must be spent-able — no re-send loop"
    );

    // Everything clear: an eaten kickoff on a silent pane → re-deliver.
    assert_eq!(act(true, true, false, false, 0, 0), KickoffRecovery::Redeliver);

    // And the kind gate itself: FRESH only. A resume re-sync is re-derivable
    // from durable state; a mid-session prompt has a sender still around.
    assert!(Delivery::FreshKickoff.recovers_lost_kickoff());
    assert!(
        !Delivery::ResumeKickoff.recovers_lost_kickoff(),
        "a resume notice is re-derivable — it must not be re-pasted by this path"
    );
    assert!(!Delivery::MidSession.recovers_lost_kickoff());

    // Review F5: growth on a pane we never watched go ready declines the
    // same way, but must not CLAIM a turn — the boot paint that was still
    // running explains it equally well, and an audit token that says
    // "turn-started" about output nobody can attribute is a claim the
    // evidence does not support.
    assert_eq!(
        kickoff_recovery_action(
            true, true, false, false, KICKOFF_TURN_EVIDENCE_BYTES,
            KICKOFF_TURN_EVIDENCE_BYTES, false, 0, KICKOFF_REDELIVERY_MAX,
        ),
        KickoffRecovery::Decline(KickoffDecline::OutputUnattributable),
        "a blind paste's growth is unattributable, not evidence of a turn"
    );
    assert_eq!(
        KickoffDecline::OutputUnattributable.as_str(),
        "output-unattributable",
        "the two growth declines must be distinguishable in the audit log"
    );
    // ...and `ready_observed` changes ONLY the label, never the outcome:
    // both are declines, and neither is reachable without growth.
    assert_eq!(
        kickoff_recovery_action(
            true, true, false, false, 0, KICKOFF_TURN_EVIDENCE_BYTES, false, 0, KICKOFF_REDELIVERY_MAX,
        ),
        KickoffRecovery::Redeliver,
        "a blind paste with no growth is still the eaten-kickoff case — recover it"
    );
}

#[test]
fn a_re_delivery_gets_the_same_boot_wait_the_original_kickoff_had() {
    // Review F4 — the one place this feature could reproduce the bug it
    // exists to fix. The recovery used to nudge the drainer with `None`, so
    // the re-delivered brief pasted with no boot wait at all; a CLI whose
    // stdin reader still had not attached would eat it again. That is not
    // impossible by construction — only unlikely — so the re-delivery is
    // given the same wait, and this pins all three flags.
    let t = redelivery_treatment(true).expect("a re-delivery alone at the front gets kickoff treatment");

    assert!(
        t.wait_ready,
        "a re-delivery pasting into a still-booting CLI is the original defect's ghost"
    );
    assert!(
        !t.confirm_autopilot,
        "copilot's consent dialog answers the FIRST submit — re-arming it would put a stray \
         Enter into the pane we are recovering"
    );
    assert!(
        !t.fresh_kickoff,
        "the re-delivery must NOT be recoverable itself — this is what bounds the feature at \
         one recovery even if every budget check were removed"
    );
    // All three at once, by name: they are the same type and mean opposite
    // things, so a transposed edit is exactly the failure this pins.
    assert_eq!(
        t,
        KickoffTreatment { wait_ready: true, confirm_autopilot: false, fresh_kickoff: false }
    );

    // And the front-door rule it mirrors: kickoff treatment belongs to the
    // entry the drainer's first pass actually picks up.
    assert_eq!(
        redelivery_treatment(false),
        None,
        "a re-delivery admitted behind an existing queue is not the drainer's first entry"
    );
}

#[test]
fn a_badge_never_tells_the_human_to_act_on_a_re_delivery_in_flight() {
    // Review F2. The monitor keeps a raised badge honest on every
    // `KeepWaiting` tick by re-running the live decision (#496 rev-47 NB1).
    // For an eaten kickoff that live decision is permanently `NotHolding` —
    // the text is gone, which is WHY a re-delivery is queued — so without
    // this arm the badge flips back from "loomux is re-sending it" to "check
    // the pane" on the very next 5s tick, for as long as the re-delivery
    // waits behind a busy drain.
    assert_eq!(
        stranded_reword(StrandedBlocker::NotHolding, true),
        None,
        "loomux must not tell the human to act on work it is actively doing"
    );
    assert_eq!(
        stranded_reword(StrandedBlocker::NotHolding, false),
        Some(StrandedBlocker::NotHolding),
        "with no recovery in flight the badge must still name the real state"
    );

    // The pre-existing rev-47 rule, unchanged: the budget arm firing on our
    // own queued marker IS the in-flight state the badge already shows.
    assert_eq!(stranded_reword(StrandedBlocker::Exhausted, false), None);
    assert_eq!(stranded_reword(StrandedBlocker::Exhausted, true), None);

    // Everything a human can actually clear still outranks an in-flight
    // recovery — those are real blockers, not loomux's own pending work.
    for blocker in [StrandedBlocker::HumanInput, StrandedBlocker::Question, StrandedBlocker::QueueFull] {
        assert_eq!(
            stranded_reword(blocker, true),
            Some(blocker),
            "{blocker:?} is human-clearable and must be said out loud even mid-recovery"
        );
    }
}

#[test]
fn a_re_delivery_supersedes_the_monitor_that_triggered_it() {
    // Review F1: the #519/#454 compose point, composed rather than asserted.
    // The design note claims "the recovery cannot be raced by the monitor
    // that triggered it". That rests on the re-delivery's own drain calling
    // `record_inflight_delivery` before its Enter — which mints a FRESH
    // `submit_sent_ms` — flipping the triggering monitor to `Superseded`.
    // Drives the real functions in the real order.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5174u32;
    let last_delivery: TrackedMutex<HashMap<u32, _>> = TrackedMutex::new("test_last_delivery", HashMap::new());

    // The lost kickoff's own ledger entry: in flight, unconfirmed. Its
    // monitor is watching under this `submit_sent_ms`.
    let kickoff_ms = 1_000u64;
    record_inflight_delivery(&last_delivery, pty, kickoff_ms, "orrerix".to_string(), None);
    assert_eq!(
        observe_ledger(&last_delivery, pty, kickoff_ms),
        LedgerView { superseded: false, outstanding: true, newer_confirmed: false },
        "before the recovery, the monitor still owns this pane"
    );

    // The recovery admits the brief...
    assert!(reg.redeliver_lost_kickoff(&g.id, &w.id, "orrerix", pty, KICKOFF_BRIEF));

    // ...and the drain that picks it up records its own in-flight delivery
    // before pressing Enter (#454's ordering — `deliver_now` mints a fresh
    // `submit_sent_ms` for every delivery, so a re-delivery can never reuse
    // the original's identity).
    let redelivery_ms = kickoff_ms + 1;
    record_inflight_delivery(&last_delivery, pty, redelivery_ms, "orrerix".to_string(), None);

    let view = observe_ledger(&last_delivery, pty, kickoff_ms);
    assert!(view.superseded, "the re-delivery must take ownership of the pane from the old monitor");
    assert!(!view.outstanding, "the old monitor no longer has an outstanding delivery to act on");
    assert_eq!(
        late_monitor_tick(view.superseded, PromptLandedMatch::None, false, false, true, false),
        MonitorAction::Superseded,
        "the monitor that triggered the recovery exits without writing or notifying — it \
         cannot race the re-delivery it asked for"
    );

    // The new delivery's own monitor owns the pane from here, and reads it
    // as outstanding under its own identity — nothing is left unwatched.
    assert_eq!(
        observe_ledger(&last_delivery, pty, redelivery_ms),
        LedgerView { superseded: false, outstanding: true, newer_confirmed: false },
    );
}

#[test]
fn a_lost_fresh_kickoff_is_re_delivered_through_the_queue_front_door() {
    // The recovery, wired: the brief goes back through the SAME admission
    // every other delivery uses, so it inherits ordering, the paste guards
    // and the three-state confirmation — a raw write from the monitor thread
    // would race the drainer and re-open the ordering hole #470 closed.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5170u32;

    let sent = reg.redeliver_lost_kickoff(&g.id, &w.id, "orrerix", pty, KICKOFF_BRIEF);

    assert!(sent, "a lost fresh kickoff must actually be re-admitted, not merely badged");
    let snap = reg.queue_snapshot(pty);
    assert_eq!(snap.len(), 1, "exactly one re-delivery");
    assert_eq!(
        snap[0].payload.text(),
        Some(KICKOFF_BRIEF),
        "the brief itself is re-delivered — verbatim, not a notice about it"
    );
    assert_eq!(
        snap[0].reason,
        queue::EnqueueReason::KickoffRecovery,
        "a re-delivery and a first delivery are different facts in the record"
    );

    // Loud, never silent: the recovery is in the audit log with the brief's
    // own queue id, so one payload's whole history stays reconstructible.
    let audits = reg.audit_log(&g.id);
    assert_eq!(
        audits.iter().filter(|e| e.action == "kickoff-redelivered").count(),
        1,
        "every re-delivery is audited exactly once"
    );
}

#[test]
fn a_re_delivery_never_double_sends_a_brief_that_is_still_queued() {
    // Idempotence layer 3 (after the hook confirming, and the ledger): the
    // queue's own byte-identical coalesce. A brief still waiting to drain
    // must not be queued a second time — and the collapsed attempt must not
    // burn the budget for a send that never happened, or one wrong reading
    // would cost the only re-delivery this delivery gets.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5171u32;

    reg.enqueue_text(&g.id, &w.id, "orrerix", KICKOFF_BRIEF, pty, queue::EnqueueReason::Arrival)
        .unwrap();

    let sent = reg.redeliver_lost_kickoff(&g.id, &w.id, "orrerix", pty, KICKOFF_BRIEF);

    assert!(!sent, "a coalesced re-delivery is not a send, and must not burn the budget");
    assert_eq!(reg.queue_depth(pty), 1, "the same brief must never occupy two queue slots");
    let audits = reg.audit_log(&g.id);
    assert!(
        audits.iter().any(|e| e.action == "kickoff-redelivery-skipped"),
        "the declined re-delivery is audited too, with its reason"
    );
    assert_eq!(
        audits.iter().filter(|e| e.action == "kickoff-redelivered").count(),
        0,
        "nothing may claim a re-delivery happened when the queue collapsed it"
    );
}

#[test]
fn the_boot_wait_never_calls_a_still_painting_cli_ready() {
    // The mechanism that eats a kickoff paste in the first place, driven
    // through the REAL wait loop against a REAL PtyManager. A CLI whose boot
    // output exceeds the output ring's cap keeps producing bytes while the
    // ring's LENGTH stops changing — so a length-based stability check reads
    // "gone quiet" mid-boot and the paste lands in a stdin buffer nobody is
    // reading yet. The counter cannot produce that reading.
    let pm = PtyManager::default();
    let pty = 5172u32;
    pm.register_fake_for_test(pty, b"");

    // A CLI still painting: every poll of the wait delivers another chunk of
    // boot output, all the way past the ring's capacity. The clock advances
    // one `READY_POLL` (250ms) per poll, so it also runs well past
    // `READY_MIN_WAIT` and `READY_QUIET` — nothing here is "not enough time
    // has passed", the pane is genuinely never quiet.
    let chunk = vec![b'x'; 64 * 1024];
    let clock = std::cell::Cell::new(Duration::ZERO);
    let outcome = await_cli_ready(
        // #1591: no marker — this test is about the SAMPLER, and a marker
        // would add a second reason for its TimedOut that it does not mean.
        None,
        || {
            pm.append_fake_output_for_test(pty, &chunk);
            pm.output_total(pty)
        },
        || -> Option<String> { unreachable!("a None marker must never compose a screen") },
        || clock.set(clock.get() + Duration::from_millis(250)),
        || clock.get(),
    );

    assert!(
        pm.output_total(pty).unwrap() > 256 * 1024,
        "the pane must actually have saturated its ring — otherwise this proves nothing"
    );
    assert_eq!(
        pm.output_tail(pty).unwrap().len(),
        256 * 1024,
        "the ring's length is pinned at the cap: the frozen signal the old wait trusted"
    );
    assert_eq!(
        outcome,
        ReadyWait::TimedOut,
        "a CLI that never stops painting must never be declared ready — it must hit \
         the cap and be pasted into knowingly, which is a state the audit can see"
    );

    // The bug itself, as a control: the SAME loop, the SAME pane, sampling
    // the ring's length instead of the counter — the substitution the
    // production call site used to make. It declares a still-painting CLI
    // ready, which is how a kickoff paste reached a stdin reader that had
    // not attached yet. Written out rather than described, so a future edit
    // that reverts the call site to `output_tail().len()` is reverting to
    // something this file states the consequence of.
    let pm_len = PtyManager::default();
    pm_len.register_fake_for_test(3172, b"");
    let clock_len = std::cell::Cell::new(Duration::ZERO);
    let via_ring_len = await_cli_ready(
        None,
        || {
            pm_len.append_fake_output_for_test(3172, &chunk);
            pm_len.output_tail(3172).map(|t| t.len() as u64)
        },
        || -> Option<String> { unreachable!("a None marker must never compose a screen") },
        || clock_len.set(clock_len.get() + Duration::from_millis(250)),
        || clock_len.get(),
    );
    assert_eq!(
        via_ring_len,
        ReadyWait::Ready,
        "the frozen signal reads as quiet on a pane that is still painting — this is \
         the defect, and the reason the wait must sample `output_total`"
    );

    // The control, so the assertion above is about the SIGNAL and not merely
    // about time: a pane that really does go quiet is declared ready.
    let pm2 = PtyManager::default();
    pm2.register_fake_for_test(4172, &vec![b'y'; 4096]);
    let clock2 = std::cell::Cell::new(Duration::ZERO);
    let ready = await_cli_ready(
        None,
        || pm2.output_total(4172),
        || -> Option<String> { unreachable!("a None marker must never compose a screen") },
        || clock2.set(clock2.get() + Duration::from_millis(250)),
        || clock2.get(),
    );
    assert_eq!(ready, ReadyWait::Ready, "a painted, quiet pane is ready — the wait still ends");
}

/// #1591: one fake-opencode boot, driven through the REAL wait loop against a
/// REAL `PtyManager`, with the marker taken from the REAL capability table.
///
/// The shape this reproduces is the one the human measured: the TUI paints its
/// banner and input box (well past `READY_MIN_OUTPUT`) and goes quiet while its
/// MCP servers and provider auth are still connecting, then prints the status
/// footer carrying the MCP count once they are up. The base painted-and-quiet
/// gate is satisfied during that gap, and pasting there is the lost kickoff.
#[test]
fn an_opencode_boot_is_not_ready_until_its_mcp_footer_appears() {
    let marker = cli_caps("opencode").expect("opencode has a capability row").ready_marker;
    assert!(marker.is_some(), "the table is what wires this — a None row makes the rest vacuous");

    // The footer as observed live, coloured the way a TUI colours a count: the
    // digit and the label are adjacent only AFTER the ANSI strip.
    let footer = "\u{1b}[36m\u{2299} \u{1b}[1m2\u{1b}[0m MCP\u{1b}[0m /status    1.18.25\r\n";
    // When (in synthetic elapsed time) the footer lands. Chosen past the point
    // the base gate alone fires, which is what makes this test able to fail;
    // the human measured the real gap at about three seconds.
    let footer_at = Duration::from_millis(4500);

    // `reads` counts every screen composition the loop asks for. It is what
    // makes the NO-LATCH property observable (#1591 review N3): a loop that
    // cached either answer would stop asking.
    let run = |m: Option<ReadyMarker>, pty: u32, with_footer: bool| {
        let pm = PtyManager::default();
        pm.register_fake_for_test(pty, b"");
        // The banner and input box: one paint, then silence.
        pm.append_fake_output_for_test(pty, &vec![b'#'; 4096]);
        let clock = std::cell::Cell::new(Duration::ZERO);
        let printed = std::cell::Cell::new(false);
        let reads = std::cell::Cell::new(0usize);
        let outcome = await_cli_ready(
            m,
            || {
                if with_footer && !printed.get() && clock.get() >= footer_at {
                    pm.append_fake_output_for_test(pty, footer.as_bytes());
                    printed.set(true);
                }
                pm.output_total(pty)
            },
            || {
                reads.set(reads.get() + 1);
                pm.output_tail_bounded(pty, 4096).map(|raw| strip_ansi(&raw))
            },
            || clock.set(clock.get() + Duration::from_millis(250)),
            || clock.get(),
        );
        (outcome, clock.get(), printed.get(), reads.get())
    };

    // 1. The control that makes the rest discriminating: the SAME pane and the
    //    SAME clock with no marker required. It is declared ready inside the
    //    gap — i.e. the base gate really is satisfied there, so the assertion
    //    below is about the marker and not about a fixture that never settles.
    let (base, base_at, _, base_reads) = run(None, 6591, true);
    assert_eq!(base_reads, 0, "a None row must never compose a screen at all");
    assert_eq!(base, ReadyWait::Ready, "the base gate is satisfied during the gap — that is the bug");
    assert!(
        base_at < footer_at,
        "the control must land BEFORE the footer or it proves nothing: ready at {base_at:?}"
    );

    // 2. opencode's row: ready only AFTER the footer.
    let (marked, marked_at, printed, marked_reads) = run(marker, 6592, true);
    assert_eq!(marked, ReadyWait::Ready, "the footer is the go signal — the wait must end on it");
    // The PROPERTY, asserted before the fixture control below it. Order matters
    // here and is not stylistic: with the marker check removed this wait ends
    // at the quiet point, which makes BOTH this and `printed` false — and a red
    // that lands on the fixture control reports "the fixture must actually have
    // printed the footer", which reads as a broken test rather than as the
    // defect. A red evidences only the assertion it reached.
    assert!(
        marked_at > footer_at,
        "readiness must not be declared before the MCP footer: ready at {marked_at:?}, footer at {footer_at:?}"
    );
    // The fixture control, kept: it is what rules out a footer that never
    // landed at all (which would reach the assertion above via the ceiling).
    assert!(printed, "the fixture must actually have printed the footer");

    // 3. A boot that never prints it waits out the ceiling and is pasted into
    //    knowingly — the TimedOut path, unchanged by this issue. This is the
    //    one direction the marker is allowed to fail in.
    let (never, never_at, printed_never, never_reads) = run(marker, 6593, false);
    assert!(!printed_never, "this arm must never print the footer");
    assert_eq!(
        never,
        ReadyWait::TimedOut,
        "an opencode boot whose footer never arrives must reach the ceiling rather than be \
         declared ready — and TimedOut is what the audit records"
    );
    assert!(
        never_at >= Duration::from_secs(25),
        "and it must be the CEILING it reached, not an early exit: {never_at:?}"
    );

    // NO LATCH (#1591 review N3): the screen is re-read on every tick that
    //    reaches it. The marked arm reads more than once — the loop kept asking
    //    across the whole quiet-to-footer window instead of caching the first
    //    negative — and the never-footer arm below keeps asking to the ceiling.
    //    A loop that latched EITHER answer would stop at one.
    assert!(
        marked_reads > 1,
        "the marker must be re-read each tick, not cached: {marked_reads} composition(s)"
    );
    assert!(
        never_reads > 10,
        "a negative must never be cached either — the loop must keep asking to the \
         ceiling: {never_reads} composition(s)"
    );

    // 4. Every other CLI is untouched — read from the table rather than
    //    asserted as a literal, so a row that grows a marker fails HERE instead
    //    of silently changing a claude pane's boot.
    for cli in ["claude", "copilot", "gemini"] {
        assert_eq!(
            cli_caps(cli).unwrap().ready_marker,
            None,
            "{cli} must keep the pre-#1591 gate: no marker in its row"
        );
    }
}

/// #1591: the marker is a SHAPE — a count — and neither a particular number nor
/// a bare label.
///
/// Both halves matter and fail in opposite directions. Requiring the digit is
/// what stops a line that merely NAMES MCP — a menu row, a `/mcp` help line,
/// the word sitting in a brief the pane is echoing — from releasing the paste
/// early. Not requiring a PARTICULAR digit is what stops loomux waiting on a
/// number it would have to keep in step with the CLI's own bookkeeping: the
/// footer counts the servers connected so far, so it moves during the
/// handshake.
#[test]
fn the_ready_marker_matches_a_count_not_a_label() {
    let m = ReadyMarker::CountThen(" MCP");

    // The observed footer, after the strip.
    assert!(m.matches("\u{2299} 2 MCP /status    1.18.25"));
    // Any count will do — one server connected is the CLI being up.
    assert!(m.matches("\u{2299} 1 MCP"));
    // Including a number loomux never configured, which is the point of not
    // comparing against one.
    assert!(m.matches("\u{2299} 17 MCP /status"));

    // The label alone is not the marker: pasting on it is the bug.
    assert!(!m.matches("\u{2299} MCP /status    1.18.25"));
    assert!(!m.matches("no MCP servers configured"));
    // A digit that is not adjacent does not count either.
    assert!(!m.matches("2 servers, MCP pending"));
    // Nothing at all.
    assert!(!m.matches(""));
    assert!(!m.matches("\u{2299} opencode  /help    1.18.25"));

    // Every occurrence is examined, not just the first — the row carries other
    // text, and a leading un-counted copy must not veto a counted one.
    assert!(m.matches("MCP: starting ...  \u{2299} 2 MCP"));

    // #1591 review N2: ZERO counts, deliberately. A printed count is a
    // COMPLETED handshake, which is the thing the gate actually needs to know;
    // it is not a claim that any server came up. The one loomux configures can
    // fail (port taken, token rejected, timeout) and the pane is still reading.
    // Pinned so the intent is a decision rather than an accident of the digit
    // test — the surfaces used to say "the first connected server", which is
    // not what this does.
    assert!(m.matches("\u{2299} 0 MCP /status    1.18.25"));

    // #1591 review N3, the EARLY-match direction: a boot line that carries the
    // shape before the input box is live. Without the prose rule these release
    // the paste into exactly the gap #1591 exists to close, and the gate
    // silently degrades to its pre-#1591 behaviour with every test still green.
    assert!(!m.matches("1 MCP server connecting..."));
    assert!(!m.matches("2 MCP servers starting"));
    assert!(!m.matches("\u{2299} 3 MCP servers failed to connect"));
    // opencode's own /status dialog, which a human can summon at any time:
    // `{Object.keys(sync.data.mcp).length} MCP Servers` renders the CONFIGURED
    // count -- a different number under a label-shaped caption. Rejected by the
    // word rule reading EITHER case, which is why it is not lowercase-only.
    assert!(!m.matches("2 MCP Servers"));
    // The rule is about PROSE, not about length: a count followed by anything
    // that is not a lowercase word still reads as a label.
    assert!(m.matches("\u{2299} 2 MCP  /status"));
    assert!(m.matches("\u{2299} 2 MCP | 1.18.25"));

    // THE RESIDUAL, pinned rather than merely disclosed (see
    // `docs/design/opencode.md`). The prose rule separates a label from a
    // sentence; it cannot separate the FOOTER's label from a label-shaped
    // string anywhere else on the rendered screen. This is the blind spot, and
    // it is asserted so the disclosure cannot go stale silently: if a later
    // narrowing closes it, this assertion reddens and the note gets corrected
    // in the same commit.
    // The real instance, not a hypothetical: opencode ships a second
    // `N MCP`-shaped string whose number means something ELSE — the session
    // footer's connected count, rendered with no trailing word
    // (`routes/session/footer.tsx`, `{mcp()} MCP`). It is a genuine
    // readiness signal too, so matching it is right; it is pinned here because
    // the note claims the marker is the HOME footer and this says what else
    // satisfies it.
    assert!(m.matches("\u{2022} 2 LSP  \u{2022} 2 MCP"));
    // And the residual the word rule cannot reach: any label-shaped count that
    // is not followed by a word. Asserted so the disclosure in
    // `docs/design/opencode.md` cannot go stale silently — a later narrowing
    // that closes this reddens here and the note is corrected in the same
    // commit.
    assert!(
        m.matches("Loaded 3 MCP"),
        "the word rule separates a label from a sentence, never the FOOTER's \
         label from any other label — the note says so, and this is that residual"
    );

    // The raw wire form, which is why the caller strips: the same bytes, the
    // opposite answer.
    let raw = "\u{1b}[36m\u{2299} \u{1b}[1m2\u{1b}[0m MCP\u{1b}[0m /status";
    assert!(!m.matches(raw), "unstripped, the digit and the label are not adjacent");
    assert!(m.matches(&strip_ansi(raw.as_bytes())), "stripped, they are");
}

/// #1591: the composition — a marker ADDS an obligation and never removes one.
///
/// Written as all four crossings of {base satisfied} x {marker seen}, plus the
/// no-marker column, so "require everything" and "require nothing" each fail
/// here rather than passing one arm apiece.
#[test]
fn a_ready_marker_can_only_ever_delay_a_paste() {
    let s = Duration::from_secs;
    let ms = Duration::from_millis;
    let m = Some(ReadyMarker::CountThen(" MCP"));

    // No marker in the row: exactly `cli_ready`, both ways.
    assert!(cli_ready_with_marker(4096, s(2), s(3), None, false));
    assert!(!cli_ready_with_marker(4096, ms(100), s(3), None, true));

    // With a marker: base AND marker.
    assert!(cli_ready_with_marker(4096, s(2), s(3), m, true));
    assert!(!cli_ready_with_marker(4096, s(2), s(3), m, false), "marker outstanding, so not ready");
    assert!(
        !cli_ready_with_marker(4096, ms(100), s(3), m, true),
        "a seen marker must NOT excuse a pane that is still painting — the base test stands"
    );
    assert!(!cli_ready_with_marker(0, s(5), s(5), m, true), "nor an unpainted one");
    assert!(!cli_ready_with_marker(4096, ms(100), s(3), m, false));
}

/// #1591 review D1: the production sampler's own decisions, on rendered-screen
/// fixtures.
///
/// The headline change of the previous round — read the SCREEN, not the ring —
/// lived entirely in a closure inside `deliver_now`, so reverting it reddened
/// nothing: every loop test injects its own sampler. `ready_screen` is that
/// closure's pure half, and this is the test that makes the choice falsifiable.
///
/// **Arms 1 and 2 are also premortem 2's pin** (#1591 review round 4). A
/// separate `the_marker_is_read_off_the_rendered_screen_not_the_byte_ring` used
/// to assert the same property one level lower — `render_visible` and
/// `strip_ansi` called directly, on a byte-identical fixture. Those are exactly
/// the two branches `ready_screen` dispatches between, so that test could not
/// fail unless one of these arms already had, and no mutation reddened it. A
/// test nothing can redden is a decoration, so it was folded in here rather
/// than kept: the property it named is arms 1-2, and the D1 mutation
/// (`ready_screen` ignoring geometry) is what witnesses it.
///
/// The fixture is deliberately painted LABEL-FIRST with an absolute cursor move
/// between the segments: a TUI repaints segments in whatever order its layout
/// walks them, so the byte log preserves that order and the screen does not.
/// Kept ASCII so the assertion is about adjacency rather than glyph width.
#[test]
fn ready_screen_prefers_the_grid_and_falls_back_only_when_it_must() {
    let m = ReadyMarker::CountThen(" MCP");
    // The footer painted label-first with a cursor move between the segments:
    // adjacent on the screen, not in the stream.
    let split_paint = concat!(
        "\u{1b}[2J\u{1b}[H",
        "opencode\r\n",
        "\u{1b}[2;2H MCP /status",
        "\u{1b}[2;1H2",
    )
    .as_bytes();

    // 1. With geometry, the grid is used — and it is the reading that finds the
    //    marker. This is the assertion that reddens if production reverts to
    //    stripping the ring.
    let grid = ready_screen(split_paint, Some((40, 4)));
    assert!(m.matches(&grid), "the composed screen shows the footer: {grid:?}");

    // 2. Without geometry there is no grid to compose, so the ring is read.
    //    Asserted as a DIFFERENT string, not merely as a non-panic: the two
    //    branches must be distinguishable or arm 1 proves nothing about which
    //    one ran.
    let ring = ready_screen(split_paint, None);
    assert_ne!(grid, ring, "the two branches must not produce the same reading");
    assert!(
        !m.matches(&ring),
        "the fallback cannot see this footer — that is the cost it is chosen \
         despite, and the reason geometry is preferred: {ring:?}"
    );

    // 3. The SECOND fallback trigger, which the surfaces used not to name: the
    //    replay composed too few non-empty rows to be trustworthy, so geometry
    //    being present is not on its own enough.
    let one_row = b"only one row";
    let thin = ready_screen(one_row, Some((40, 4)));
    assert_eq!(
        thin,
        ready_screen(one_row, None),
        "an untrustworthy composition must read exactly as no composition"
    );

    // 4. A screen with no marker on it stays unmatched through both branches —
    //    the control that stops arms 1-3 passing on a matcher that says yes to
    //    everything.
    let bare = concat!("\u{1b}[2J\u{1b}[H", "opencode\r\n", "ready\r\n").as_bytes();
    assert!(!m.matches(&ready_screen(bare, Some((40, 4)))));
    assert!(!m.matches(&ready_screen(bare, None)));

    // 5. A model name carrying the literal is not a count, at either branch.
    let modelname = concat!("\u{1b}[2J\u{1b}[H", "model: gpt-4 MCPx\r\nready\r\n").as_bytes();
    assert!(!m.matches(&ready_screen(modelname, Some((40, 4)))));
}

/// #1591 review D2: a footer that WRAPS between the count and its label.
///
/// The composed screen right-trims rows and joins with `\n`, so a row-wise
/// search sees ` MCP` preceded by a newline and misses a footer the human
/// plainly reads. It happens at a pane width, and loomux tiles panes, so the
/// width is a continuum a human drags through — this must not be a silent
/// 25-second ceiling on every kickoff at some widths.
#[test]
fn a_footer_that_wraps_between_the_count_and_its_label_still_matches() {
    let m = ReadyMarker::CountThen(" MCP");
    // One footer, two widths. Wide enough and it sits on one row; at exactly
    // ten columns the same bytes break between the count and its label, which
    // is the case this test exists for. The prefix is sized so the break lands
    // there — a width picked without checking splits the LABEL instead, and the
    // two sanity assertions below are what caught exactly that.
    let footer =
        concat!("\u{1b}[2J\u{1b}[H", "boot\r\n", "opencode 2 MCP /status\r\n").as_bytes();

    let wide = ready_screen(footer, Some((40, 6)));
    assert!(m.matches(&wide), "unwrapped, on one row: {wide:?}");

    let narrow = ready_screen(footer, Some((10, 8)));
    // TWO sanity assertions, because "it wrapped" and "it wrapped HERE" are
    // different claims and only the second makes this test about anything.
    assert!(
        narrow.lines().any(|l| l.starts_with(" MCP")),
        "the label must begin a row — i.e. the break fell between the count and \
         the label: {narrow:?}"
    );
    assert!(
        !narrow.lines().any(|l| l.contains("2 MCP")),
        "and no single row may hold both, or a row-wise search would find it and \
         the join would be untested: {narrow:?}"
    );
    assert!(
        m.matches(&narrow),
        "a wrapped footer is still a footer — the row pair must be joined: {narrow:?}"
    );

    // The control: joining pairs must not turn the WORD rule off. The same
    // wrap, with prose after the label, still refuses — the word it is judged
    // on rides along inside the joined pair.
    let prose =
        concat!("\u{1b}[2J\u{1b}[H", "boot\r\n", "opencode 2 MCP servers up\r\n").as_bytes();
    let prose_narrow = ready_screen(prose, Some((10, 8)));
    assert!(
        !m.matches(&prose_narrow),
        "the wrap handling must not smuggle a boot line past the word rule: \
         {prose_narrow:?}"
    );

    // And the control that the pair join is not simply matching everything:
    // two adjacent rows that do not form the shape stay unmatched.
    let unrelated = concat!("\u{1b}[2J\u{1b}[H", "loaded 3\r\nsomething else\r\n").as_bytes();
    assert!(!m.matches(&ready_screen(unrelated, Some((40, 4)))));
}

/// #1591 review N3: a boot line carrying the marker's shape, on a pane that has
/// gone quiet before its input loop is live, must NOT release the kickoff.
///
/// This is the failure mode the whole fix would otherwise reproduce through its
/// own new mechanism: match a decoy, treat the pane as ready, and paste into
/// exactly the gap #1591 describes. The fixture is deliberately the hardest
/// version — the decoy is on screen AT the quiet point, which is the only
/// moment the gate ever looks.
#[test]
fn a_decoy_boot_line_does_not_release_the_kickoff() {
    let marker = cli_caps("opencode").expect("opencode has a capability row").ready_marker;
    let decoy = "1 MCP server connecting...\r\n";
    let footer = "\u{2299} 2 MCP /status    1.18.25\r\n";
    let footer_at = Duration::from_millis(6000);

    let pm = PtyManager::default();
    let pty = 6594u32;
    pm.register_fake_for_test(pty, b"");
    // Paint, then the decoy, then silence — so the base gate is satisfied with
    // the decoy sitting in the pane and nothing else happening.
    pm.append_fake_output_for_test(pty, &vec![b'#'; 4096]);
    pm.append_fake_output_for_test(pty, decoy.as_bytes());

    let clock = std::cell::Cell::new(Duration::ZERO);
    let printed = std::cell::Cell::new(false);
    let outcome = await_cli_ready(
        marker,
        || {
            if !printed.get() && clock.get() >= footer_at {
                pm.append_fake_output_for_test(pty, footer.as_bytes());
                printed.set(true);
            }
            pm.output_total(pty)
        },
        || pm.output_tail_bounded(pty, 4096).map(|raw| strip_ansi(&raw)),
        || clock.set(clock.get() + Duration::from_millis(250)),
        || clock.get(),
    );

    assert_eq!(outcome, ReadyWait::Ready, "the REAL footer still releases the paste");
    // The PROPERTY first, the fixture control below it. Removing the word rule
    // makes BOTH false — the decoy releases at the quiet point, so the run ends
    // before the real footer is ever printed — and a red landing on `printed`
    // reports "the fixture must have reached the real footer", which reads as a
    // broken test rather than as the defect. A red evidences only the assertion
    // it reached. (The same ordering, for the same reason, as the arm in
    // `an_opencode_boot_is_not_ready_until_its_mcp_footer_appears`.)
    assert!(
        clock.get() > footer_at,
        "the decoy released the kickoff: ready at {:?}, real footer at {footer_at:?}",
        clock.get()
    );
    assert!(printed.get(), "the fixture must have reached the real footer");

    // The control that makes this test about the DECOY rather than about the
    // clock: the identical fixture with the decoy replaced by a real footer is
    // ready as soon as it goes quiet, far below `footer_at`.
    let pm2 = PtyManager::default();
    pm2.register_fake_for_test(6595, b"");
    pm2.append_fake_output_for_test(6595, &vec![b'#'; 4096]);
    pm2.append_fake_output_for_test(6595, footer.as_bytes());
    let clock2 = std::cell::Cell::new(Duration::ZERO);
    let early = await_cli_ready(
        marker,
        || pm2.output_total(6595),
        || pm2.output_tail_bounded(6595, 4096).map(|raw| strip_ansi(&raw)),
        || clock2.set(clock2.get() + Duration::from_millis(250)),
        || clock2.get(),
    );
    assert_eq!(early, ReadyWait::Ready);
    assert!(
        clock2.get() < footer_at,
        "a real footer at the quiet point IS the go signal — otherwise the test \
         above would pass on a gate that never releases anything: {:?}",
        clock2.get()
    );
}

#[test]
fn an_eaten_kickoff_badges_and_then_re_delivers_the_brief() {
    // #585 CORRECTION. This comment used to claim "the whole #517 path
    // composed in the monitor's own order". It is not: the steps below are
    // hand-composed HERE, in the order this test's author believed the monitor
    // used. The shipped monitor returned before ever reaching step 2, and this
    // test passed anyway for two releases — a test that IS the composition
    // cannot observe that the real composition differs (`late_monitor` needs a
    // live `AppHandle`, so no test executes it). What it pins is that each
    // decision is individually correct, which is worth having and is all it
    // ever pinned. The ORDERING is pinned by `failed_arm_route` instead — see
    // `an_eaten_paste_is_routed_to_the_recovery_not_past_it`.
    //
    // The faked pane readings: a fresh kickoff swallowed by a booting CLI
    // leaves the ledger outstanding, no human input, no question, our text NOT
    // in the box, and a pane that never produced a turn.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 5173u32;

    // Step 1 — #496 PR-C's decision, unchanged: there is no Enter to press,
    // so the self-heal refuses and raises the badge. This is exactly where
    // the story ended before #517, and it is why the worker sat idle.
    let stranded = stranded_selfheal_action(
        true,                   // ledger: still outstanding
        false,                  // no human typed
        false,                  // no question
        BoxReading::NotHolding, // read the box; our text is NOT there — eaten
        0,
        STRANDED_SELFHEAL_MAX_HEALS,
    );
    assert_eq!(
        stranded,
        StrandedAction::Attention(StrandedBlocker::NotHolding),
        "the eaten-paste signature — an Enter would be a guess, so the self-heal must refuse"
    );
    reg.actuate_stranded(&g.id, &w.id, "orrerix", pty, stranded);
    assert_eq!(
        reg.stranded_note(&w.id).expect("the human is told either way").blocker,
        Some(StrandedBlocker::NotHolding)
    );
    assert_eq!(reg.queue_depth(pty), 0, "the self-heal alone queues nothing — this is the gap #517 closes");

    // Step 2 — #517: inside that one outcome, and only for a fresh kickoff,
    // the brief is re-delivered instead of the story ending at the badge.
    let recovery = kickoff_recovery_action(
        Delivery::FreshKickoff.recovers_lost_kickoff(),
        true,  // ledger: still outstanding
        false, // no human typed
        false, // no question
        0,     // the pane produced nothing after our submit — no turn started
        KICKOFF_TURN_EVIDENCE_BYTES,
        true,  // the boot wait did watch this CLI go ready
        0,
        KICKOFF_REDELIVERY_MAX,
    );
    assert_eq!(recovery, KickoffRecovery::Redeliver);
    assert!(reg.redeliver_lost_kickoff(&g.id, &w.id, "orrerix", pty, KICKOFF_BRIEF));
    reg.mark_stranded(&g.id, &w.id, None);

    assert_eq!(
        reg.queue_snapshot(pty)[0].payload.text(),
        Some(KICKOFF_BRIEF),
        "the brief the agent never received is back in the pane's queue"
    );
    assert_eq!(
        reg.stranded_note(&w.id).expect("still loud while loomux recovers").blocker,
        None,
        "the badge must stop telling the human to act once loomux is re-sending"
    );
    // Review F2: and it STAYS that way while the re-delivery waits to drain.
    // The live re-check on every later tick still reads `NotHolding` (the
    // text is gone — that is why a recovery is queued), so without the
    // in-flight arm the badge would flip back within one 5s tick.
    assert_eq!(
        stranded_reword(StrandedBlocker::NotHolding, /* redelivery in flight */ true),
        None,
        "a queued-but-undrained recovery must not re-raise 'check the pane'"
    );

    // And the counter-example, on the same pane state but with the one fact
    // that differs for a kickoff that DID land: the agent ran a turn.
    assert_eq!(
        kickoff_recovery_action(
            Delivery::FreshKickoff.recovers_lost_kickoff(),
            true, false, false,
            KICKOFF_TURN_EVIDENCE_BYTES * 4,
            KICKOFF_TURN_EVIDENCE_BYTES,
            true,
            0,
            KICKOFF_REDELIVERY_MAX,
        ),
        KickoffRecovery::Decline(KickoffDecline::TurnStarted),
        "the live counter-example: a reviewer that received its brief normally must \
         not be handed a duplicate"
    );
}

#[test]
fn stranded_badge_outranks_waiting_and_clears_when_the_delivery_resolves() {
    // The badge as the human sees it: it must beat `waiting` (a wedged pane
    // is not merely parked on a prompt), carry the pty the header chip and
    // dock dot key off, and come DOWN when the delivery resolves — a badge
    // that never clears trains the human to ignore it.
    let (reg, _d, g, wid) = attention_setup();
    let now = 1_000_000_000_000u64;
    let out: HashMap<String, u64> = [(wid.clone(), 512u64)].into_iter().collect();
    let prompt: HashMap<String, String> =
        [(wid.clone(), strip_ansi(FIX_COPILOT_ASK.as_bytes()))].into_iter().collect();
    let no_input = HashMap::new();

    // Park the pane on a question long enough that `waiting` would fire.
    reg.attention_tick(now, &out, &prompt, &no_input);
    let waiting = reg.attention_tick(now + 5000, &out, &prompt, &no_input);
    assert!(
        waiting.iter().any(|i| i.agent_id == wid && i.reason == "waiting"),
        "precondition: this pane would otherwise read as `waiting`"
    );

    reg.mark_stranded(&g, &wid, Some(StrandedBlocker::HumanInput));
    let flagged = reg.attention_tick(now + 6000, &out, &prompt, &no_input);
    let item = flagged
        .iter()
        .find(|i| i.agent_id == wid)
        .expect("a stranded pane must surface in the attention scan");
    assert_eq!(item.reason, "stranded", "a wedged prompt outranks `waiting`");
    assert_eq!(
        item.pty_id,
        reg.agent(&wid).unwrap().pty_id,
        "the badge must carry the pty the pane header chip and dock dot key off"
    );
    assert!(
        item.detail.contains("press Enter or clear the box"),
        "the detail must tell the human what to DO: {}",
        item.detail
    );

    // Resolved (the ledger confirmed the delivery, or the text left the box).
    reg.clear_stranded(&g, &wid, "confirmed-late");
    let cleared = reg.attention_tick(now + 7000, &out, &prompt, &no_input);
    assert!(
        cleared.iter().all(|i| !(i.agent_id == wid && i.reason == "stranded")),
        "a resolved delivery must drop its badge"
    );
    assert!(
        reg.audit_log(&g).iter().any(|e| e.action == "stranded-cleared"),
        "the clear is audited so the wedge's whole lifetime is reconstructible"
    );
}

/// #946 Q4 / #1091 slice H — the latched-attention belt. `latch_question_held`/
/// `unlatch_question_held` are `deliver_now`'s own write path (`emit_held`/
/// `emit_held_cleared` when `HeldReason::InteractiveQuestion` fires on the
/// orchestrator's pane); this test exercises the SAME public methods rather
/// than re-deriving a fake PTY dialog, so it pins `attention_tick`'s reading
/// of the latch without needing a real held delivery.
#[test]
fn held_question_dialog_outranks_blocked_and_clears_with_the_hold() {
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/held-dialog-repo", watchdog_rails(0)).unwrap();
    let orch = reg.spawn_agent(&g.id, Role::Orchestrator, "orch", "", false, None).unwrap();
    let now = 1_000_000_000_000u64;
    let no_out = HashMap::new();
    let no_tails = HashMap::new();
    let no_input = HashMap::new();

    // Precondition: an orchestrator that also reported "blocked" would
    // normally read as `blocked` — the reason this belt must OUTRANK it,
    // not merely coexist with it (a held dialog strands every OTHER agent's
    // report too, not just this one's own status).
    reg.note_report_attention(&orch.id, "blocked");
    let blocked_only = reg.attention_tick(now, &no_out, &no_tails, &no_input);
    assert!(
        blocked_only.iter().any(|i| i.agent_id == orch.id && i.reason == "blocked"),
        "precondition: without the latch this pane reads as blocked"
    );

    reg.latch_question_held(&orch.id);
    let held = reg.attention_tick(now + 1000, &no_out, &no_tails, &no_input);
    let item = held
        .iter()
        .find(|i| i.agent_id == orch.id)
        .expect("a held dialog must surface in the attention scan");
    assert_eq!(item.reason, "held-dialog", "the held dialog must outrank the plain `blocked` report");
    assert_eq!(item.pty_id, reg.agent(&orch.id).unwrap().pty_id);

    // Cleared the instant the hold clears (deliver_now's emit_held_cleared) —
    // latched though it is, it must not survive past the hold that raised it.
    reg.unlatch_question_held(&orch.id);
    let cleared = reg.attention_tick(now + 2000, &no_out, &no_tails, &no_input);
    assert!(
        cleared.iter().all(|i| !(i.agent_id == orch.id && i.reason == "held-dialog")),
        "the belt must release once the hold itself clears"
    );
    // The underlying `blocked` report is untouched by the latch's own
    // lifecycle — it re-emerges once the belt steps aside, exactly the
    // priority-ladder property `stranded_badge_outranks_waiting_and_clears_
    // when_the_delivery_resolves` pins for `stranded`/`waiting`.
    assert!(cleared.iter().any(|i| i.agent_id == orch.id && i.reason == "blocked"));
}

/// `attention_tick` itself has NO role check on `attn_question_held` — the
/// belt's "orchestrator only" scope (narrower than the #946 Q4 CLI deny,
/// which also covers the liaison) is enforced entirely at the ONE writer,
/// `deliver_now`'s `target_is_orchestrator` gate (see `attn_question_held`'s
/// doc). This test proves that by doing what only the writer should ever do
/// in production — latching a WORKER id directly, bypassing
/// `target_is_orchestrator` (which this test cannot reach without a real
/// PTY) — and confirming `attention_tick` reads it exactly the way it would
/// for an orchestrator: `held-dialog`, outranking `stranded`. If a future
/// change adds a second, redundant role check inside `attention_tick`, this
/// test is the one that catches the drift between "gated at the write side"
/// (the design) and "gated nowhere in particular" (two enforcement points
/// that can silently disagree).
#[test]
fn attention_tick_reads_the_held_set_with_no_role_check_of_its_own() {
    let (reg, _d, g, wid) = attention_setup();
    let now = 1_000_000_000_000u64;
    let no_out = HashMap::new();
    let no_tails = HashMap::new();
    let no_input = HashMap::new();

    reg.mark_stranded(&g, &wid, Some(StrandedBlocker::HumanInput));
    let before = reg.attention_tick(now, &no_out, &no_tails, &no_input);
    let before_reason =
        before.iter().find(|i| i.agent_id == wid).map(|i| i.reason).unwrap_or("");
    assert_eq!(before_reason, "stranded");

    reg.latch_question_held(&wid);
    let after = reg.attention_tick(now + 1000, &no_out, &no_tails, &no_input);
    let after_reason = after.iter().find(|i| i.agent_id == wid).map(|i| i.reason).unwrap_or("");
    assert_eq!(
        after_reason, "held-dialog",
        "attention_tick must have no role opinion of its own — production code never \
         calls latch_question_held for a non-orchestrator id (deliver_now's \
         target_is_orchestrator gate is the only enforcement point), but if attention_tick \
         ever DID special-case role here too, the two checks could silently disagree"
    );
}

#[test]
fn stranded_detail_names_the_blocker_the_human_must_clear() {
    // The badge text is the entire user-facing surface of this feature: each
    // blocker must say something different and actionable, or the human
    // learns nothing from the badge that raised it.
    let details: Vec<String> = [
        None,
        Some(StrandedBlocker::HumanInput),
        Some(StrandedBlocker::Question),
        Some(StrandedBlocker::NotHolding),
        Some(StrandedBlocker::Exhausted),
        Some(StrandedBlocker::QueueFull),
        // #563: appended (never inserted) — the index-based assertions below
        // name specific rows, and shifting them would silently re-point them
        // at the wrong blocker while still passing.
        Some(StrandedBlocker::QueueNearFull),
        Some(StrandedBlocker::QueueAtCapacity),
        Some(StrandedBlocker::QuestionStale),
        // #569: appended for the same reason — see the note above.
        Some(StrandedBlocker::PauseSuppressed),
    ]
    .into_iter()
    .map(|b| stranded_detail("w-1", b))
    .collect();

    assert!(details.iter().all(|d| d.starts_with("w-1")), "every detail names the pane");
    let unique: std::collections::HashSet<&String> = details.iter().collect();
    assert_eq!(unique.len(), details.len(), "no two blockers may read identically: {details:?}");
    assert!(
        details[0].contains("loomux is re-sending it"),
        "an in-flight heal must say loomux is handling it, not ask the human to act"
    );
    assert!(
        details[4].contains("press Enter"),
        "a spent heal budget hands the pane back to the human explicitly"
    );
    // rev-47 NB4: a claim is a deliverable, even in a tooltip. `QueueFull`
    // means no heal was ever attempted, so its wording must not borrow
    // `Exhausted`'s "after a self-heal".
    assert!(
        !details[5].contains("after a self-heal"),
        "the queue-full badge must not claim a heal fired: {}",
        details[5]
    );
    assert!(details[5].contains("queue"), "it must name the real obstacle: {}", details[5]);
}

#[test]
fn a_queue_full_pane_badges_queue_full_not_a_spent_heal() {
    // rev-47 NB4, wired: the `Err` arm of `actuate_stranded`'s admission.
    // Fill the pane's queue to the cap so the marker push is rejected, then
    // assert the badge reports the truth (nothing was attempted) rather than
    // `Exhausted`'s "a heal fired and did not take".
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let pty = 4965u32;
    for i in 0..queue::QUEUE_MAX_PER_PANE {
        reg.enqueue_text(&g.id, &w.id, "orrerix", &format!("d-{i}"), pty, queue::EnqueueReason::BehindQueue)
            .unwrap();
    }

    let healed = reg.actuate_stranded(&g.id, &w.id, "orrerix", pty, StrandedAction::SelfHeal);

    assert!(!healed, "a rejected admission is not a heal");
    assert_eq!(
        reg.stranded_note(&w.id).expect("the human must still be told").blocker,
        Some(StrandedBlocker::QueueFull),
        "the badge must name the real obstacle, not a heal that never ran"
    );
    assert!(
        reg.audit_log(&g.id).iter().any(|e| e.action == "stranded-selfheal-skipped"),
        "and the skip is audited with its reason"
    );
}
