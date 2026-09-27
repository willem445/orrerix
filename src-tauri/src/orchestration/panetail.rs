//! A pane's output tail: ANSI stripping, spinner-frame collapsing and the
//! capped tail text, and the attention tail signals.
//! Design note: `docs/design/orchestration.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. It calls no
//! sibling file.

use super::*;
/// How many RAW bytes of a pane's output ring one attention tick reads (#717).
/// Everything the scan looks for (a prompt, a question, a menu) is the last
/// thing the CLI painted, so the tail has always been all it needed — but the
/// read used to fetch the whole (up to 256 KB) ring and slice these bytes off
/// the end, and that fetch happens under the global `ptys` mutex, the one
/// `write_pty`/`note_user_input` take on every keystroke and the one the pane's
/// own reader thread contends with on the ring behind it. Bounding the REQUEST
/// is the entire point: the stripped text handed to the detectors is
/// byte-identical either way (`strip_ansi(&ring[len-N..])` is
/// `strip_ansi(&last_N)`), so what changes is only how long the lock is held.
pub const ATTENTION_SCAN_BYTES: usize = 4096;

/// What `agent_output_tail` returns given whatever the live pty produced (if
/// still alive) and whatever was captured at exit (#281). Factored out as a
/// pure function so the fallback — the actual behavior change — is directly
/// unit-testable without a live pty/app handle, which `agent_output_tail`
/// itself can't be driven with in a unit test.
pub fn resolve_output_text(live: Option<String>, last_exit_tail: Option<&str>) -> Result<String, String> {
    if let Some(t) = live {
        return Ok(t);
    }
    match last_exit_tail {
        Some(t) if !t.is_empty() => Ok(t.to_string()),
        // The live pty is already gone (the agent exited) and nothing was
        // captured at exit time either — the pre-#281 behavior, kept as the
        // last resort rather than inventing content that was never seen.
        _ => Err("terminal already closed".to_string()),
    }
}

/// Everything `attention_tick`'s phase 2 reads out of ONE pane's masked tail.
///
/// A struct rather than two parallel `HashMap`s (#2811 S5a) because both
/// answers come from the same `mask_loomux_notices_with_record` call, and
/// keeping them together is what makes that literal: two maps built in one
/// closure invite a later edit to compute one of them somewhere else, off a
/// tail that was never masked — which is precisely the defect the mask exists
/// to prevent.
pub(in crate::orchestration) struct PaneTailSignals {
    /// The pane's tail looks like an interactive prompt awaiting an answer
    /// (`prompt_wait_detected`) — one input to the `waiting` reason.
    pub(in crate::orchestration) shaped: bool,
    /// The provider spend/usage limit this pane is sitting on, if any
    /// (`providerlimit::limit_in_tail`).
    pub(in crate::orchestration) limit: Option<&'static providerlimit::LimitPattern>,
}

/// One pane's attention-scan tail: a BOUNDED raw read of its output ring,
/// ANSI-stripped (#717).
///
/// `read` is the raw-byte reader — `PtyManager::output_tail_bounded` at every
/// production call site (`attention_inputs` for agent panes,
/// `pane_attention_inputs_from` for plain ones). A closure rather than the
/// manager itself for the same reason `Tier1Scan::read` takes one: the SIZE of
/// the request is the only thing an assertion can reach here. Slicing the last
/// `ATTENTION_SCAN_BYTES` off a whole-ring read produces a byte-identical
/// string, so a test that only looks at the returned text cannot tell a 4 KB
/// copy under the `ptys` mutex from a 256 KB one — and the copy is the defect.
///
/// The strip runs OUTSIDE the read (the reader has already released both locks
/// by the time it returns), so no scanning happens under the lock at all.
pub fn attention_tail(read: impl FnOnce(usize) -> Option<Vec<u8>>) -> Option<String> {
    read(ATTENTION_SCAN_BYTES).map(|raw| strip_ansi(&raw))
}

/// Strip ANSI escape sequences (CSI, OSC, two-byte ESC) and carriage
/// returns so `get_output` returns readable text from raw terminal bytes.
pub fn strip_ansi(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            i += 1;
            match bytes.get(i) {
                Some(b'[') => {
                    // CSI: parameters/intermediates until a final byte 0x40-0x7E.
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                Some(b']') => {
                    // OSC: until BEL or ESC \.
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                Some(_) => i += 2 - 1, // two-byte escape: skip the introducer
                None => {}
            }
            continue;
        }
        if b == b'\r' || (b < 0x20 && b != b'\n' && b != b'\t') {
            i += 1;
            continue;
        }
        // Decode this UTF-8 unit; fall back to skipping the byte.
        let len = match b {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        if let Ok(s) = std::str::from_utf8(&bytes[i..(i + len).min(bytes.len())]) {
            out.push_str(s);
        }
        i += len;
    }
    out
}

/// #480/#496 PR-E: a redrawn spinner/statusline frame's stable "core", for
/// deciding whether two CONSECUTIVE lines are the same repaint. Strips
/// exactly one leading glyph run (the spinner symbol — braille dots, Claude's
/// `✻`/`✢`/`✽` star family, `*`, a box-drawing bullet, whatever a given CLI
/// redraws each tick) plus the space after it, and exactly one trailing
/// parenthesized group (the `(esc to interrupt · 8s · ↓ 172 tokens)` shape
/// documented at `auto_compact_banner_substrings` (`compactnudge.rs`) — elapsed time and
/// token counts that change every frame live here). What is left is the
/// stable prose a human actually wants to read once.
///
/// Deliberately NO fuzzy matching beyond that: two lines collapse only when
/// this core is byte-identical. A line that merely shares a leading glyph but
/// says something different keeps its own core and is never merged — see
/// `collapse_repeated_frames`'s doc for the conservatism this buys.
///
/// The trailing-paren strip is gated on the line actually having had a
/// leading glyph stripped (rev-29's #501 review finding, N1). Applying it to
/// *any* line ending in `)` over-collapses ordinary content that just happens
/// to differ only inside trailing parens — `fn parse(input: &str)` next to
/// `fn parse(input: &[u8])` is exactly the shape a worker-pane tail is full
/// of (code listings, rustc diagnostics), and both are real, distinct lines,
/// not a redraw. Every real spinner shape this repo has documented is
/// glyph-led, so gating on that costs nothing for the case this function
/// exists to catch while closing the false-merge class entirely.
fn spinner_frame_core(line: &str) -> &str {
    let trimmed = line.trim();
    let leads_with_glyph = trimmed.chars().next().is_some_and(|c| !c.is_alphanumeric() && !c.is_whitespace());
    let after_glyph = trimmed.trim_start_matches(|c: char| !c.is_alphanumeric() && !c.is_whitespace());
    let after_glyph = after_glyph.strip_prefix(' ').unwrap_or(after_glyph);
    if leads_with_glyph && after_glyph.ends_with(')') {
        match after_glyph.rfind('(') {
            Some(i) => after_glyph[..i].trim_end(),
            None => after_glyph,
        }
    } else {
        after_glyph
    }
}

/// A core shorter than this never collapses, even if repeated — guards short,
/// legitimately-repeated lines (a bare prompt character, several blank lines
/// in a row) from ever being read as a redrawn frame. Conservatism per
/// #480/#496 PR-E's brief: prefer under-collapsing to over-collapsing.
const SPINNER_FRAME_MIN_CORE_LEN: usize = 6;

/// #480/#496 PR-E: collapse a run of CONSECUTIVE lines that share a
/// `spinner_frame_core` into one — the freshest (last) line of the run,
/// verbatim, plus a `(N repeated frames collapsed)` marker so the elision is
/// visible rather than silent (this repo has a whole batch of lessons about
/// claims of completeness that turned out false; a silent drop here would be
/// exactly that). Only ADJACENT lines are ever compared: two identical lines
/// separated by other content are a real repeat in the transcript (e.g. the
/// same log line printed twice, far apart), not a redraw, and are left alone.
pub fn collapse_repeated_frames(lines: &[&str]) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let core = spinner_frame_core(lines[i]);
        if core.chars().count() >= SPINNER_FRAME_MIN_CORE_LEN {
            let mut j = i + 1;
            while j < lines.len() && spinner_frame_core(lines[j]) == core {
                j += 1;
            }
            let run = j - i;
            if run > 1 {
                out.push(lines[j - 1].to_string()); // freshest frame, verbatim
                out.push(format!("[... {} repeated frames collapsed ...]", run - 1));
                i = j;
                continue;
            }
        }
        out.push(lines[i].to_string());
        i += 1;
    }
    out
}

/// What `get_output` (`agent_output_tail`) actually returns for an already-
/// `strip_ansi`'d pane render: repeated spinner/statusline frames collapsed
/// (`collapse_repeated_frames`), then the last `n_lines` of THAT, `n_lines`
/// clamped to `[1, 500]` exactly like `agent_output_tail` always has. Factored
/// out pure, same reasoning as `resolve_output_text` above, so `get_output`'s
/// behavior is directly testable without a live pty/app handle.
///
/// This is `get_output`'s OWN path only, strictly after the shared
/// `strip_ansi` this function's caller already applied — nothing here changes
/// what `strip_ansi` itself returns to its other callers (`box_holds_paste`,
/// `prompt_wait_detected`, the compact/menu detectors in `compactnudge.rs` and
/// `screen.rs`); they never call
/// this function, and never see collapsed text.
pub fn format_output_tail(text: &str, n_lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let collapsed = collapse_repeated_frames(&all);
    let n = n_lines.clamp(1, 500);
    let start = collapsed.len().saturating_sub(n);
    cap_output_bytes(collapsed[start..].join("\n"))
}

/// Hard ceiling on what one `get_output` call can put into the caller's
/// context, in bytes, whatever `lines` was asked for (#520).
///
/// `lines` bounds distinct content *lines*; nothing bounded the *payload*. A
/// pane rendering a 200-column TUI can put several KB on a single line, and
/// 500 lines of that is a six-figure token bill delivered to an orchestrator
/// that asked a small question. The two limits are independent on purpose:
/// whichever binds first wins.
///
/// 8 KB is roughly two full screens of a wide pane — enough to answer "what
/// is this agent doing right now", which is what the tool is for. Anything
/// larger is a job for the agent's own report, not for monitoring.
pub const OUTPUT_TAIL_MAX_BYTES: usize = 8 * 1024;

/// Headroom reserved for the truncation marker so the *returned* string —
/// marker included — is always within [`OUTPUT_TAIL_MAX_BYTES`]. A cap that
/// the cap's own announcement can push you over is not a cap.
const OUTPUT_TAIL_MARKER_RESERVE: usize = 96;

/// Trim `text` to [`OUTPUT_TAIL_MAX_BYTES`], keeping the **newest** end —
/// a monitoring read wants what the pane is doing now, not how it started —
/// and saying so on the line it dropped.
///
/// The marker states plainly that bytes were dropped and how many. It does
/// NOT characterise them ("animation residue" was the phrasing #520 proposed):
/// by the time this runs the composed-grid replay has already removed the
/// redraw churn, so anything still here and still over budget is as likely to
/// be a legitimately enormous build log. Labelling real output as residue
/// would be a claim the code can't back — the thing this repo keeps writing
/// lessons about — so the marker reports the fact (bytes dropped, cap hit)
/// and leaves the interpretation to the reader.
fn cap_output_bytes(text: String) -> String {
    if text.len() <= OUTPUT_TAIL_MAX_BYTES {
        return text;
    }
    let budget = OUTPUT_TAIL_MAX_BYTES - OUTPUT_TAIL_MARKER_RESERVE;
    let mut keep_from = text.len() - budget;
    // Char boundary FIRST: pane output is full of multibyte glyphs (box
    // drawing, arrows, spinner stars), and `text[keep_from..]` panics on an
    // offset that lands inside one. Only then look for a line boundary, so
    // the first surviving line isn't a fragment.
    while keep_from < text.len() && !text.is_char_boundary(keep_from) {
        keep_from += 1;
    }
    if let Some(i) = text[keep_from..].find('\n') {
        keep_from += i + 1;
    }
    format!(
        "[... truncated {} bytes: over get_output's {} KB cap ...]\n{}",
        keep_from,
        OUTPUT_TAIL_MAX_BYTES / 1024,
        &text[keep_from..]
    )
}
