//! The human-input guard on delivery: holding for a human who is typing,
//! classifying their input, box occupancy, and the paste gate.
//! Design note: `docs/design/pty-input-path.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): `PtyManager`,
//! `crate::pty`. IO: threads/sleep. Sibling files it calls: `submit.rs`.

use super::*;

// Human-typing backstop (#43, option A): even with the loomux compose strip,
// a human can still type directly into the terminal. Before the paste AND
// before the first Enter, hold delivery while the pane has seen recent
// keystrokes so a report can't land in — or submit — the human's half-typed
// line. Capped so a long compose session can't starve reports forever.
/// Treat the human as "still typing" if they hit a key within this window.
pub(in crate::orchestration) const USER_QUIET_HOLD: Duration = Duration::from_secs(4);
/// Deliver anyway once a single hold has waited this long (never starve).
pub(in crate::orchestration) const USER_QUIET_MAX_HOLD: Duration = Duration::from_secs(90);
/// Poll interval while holding for the human to go quiet.
const USER_QUIET_POLL: Duration = Duration::from_millis(250);

// Human-input paste guard (#111): the quiet backstop above only waits out
// active typing — it does NOT stop a paste landing on top of text a human
// typed and then LEFT sitting in the box (a half-written `/model`, say). Pasting
// there and pressing Enter merge-submits the human's line with the prompt (the
// live `Unknown command: /modelRun ...` collision). So before pasting, if the
// box still holds a human's unsubmitted line (tracked per keystroke as
// `input_pending`), hold for them to submit/clear it, and if it never clears,
// abort rather than blind-merge.
/// Bounded wait for the box to clear before aborting the delivery.
const HUMAN_INPUT_HOLD_MAX: Duration = Duration::from_secs(60);
/// Poll interval while holding for the box to clear.
const HUMAN_INPUT_POLL: Duration = Duration::from_millis(250);

/// Should prompt delivery keep holding for the human to stop typing? (#43,
/// option A). Returns true to keep waiting, false to proceed. Pure so the
/// hold/deadline decision is unit-testable without a live PTY.
///
/// - `last_input_ms` is the pane's last-keystroke time (0 = none recorded).
/// - `held` is how long THIS hold has already waited; once it reaches
///   `max_hold` we deliver anyway so a long compose session can't starve the
///   report queue.
pub(in crate::orchestration) fn should_hold_for_user(
    last_input_ms: u64,
    now_ms: u64,
    held: Duration,
    quiet_window: Duration,
    max_hold: Duration,
) -> bool {
    if held >= max_hold {
        return false; // cap reached — deliver anyway
    }
    if last_input_ms == 0 {
        return false; // nobody has typed in this pane
    }
    let since = now_ms.saturating_sub(last_input_ms);
    since < quiet_window.as_millis() as u64
}

/// Poll-and-hold loop that drives `should_hold_for_user`: block while
/// `last_input_ms()` reports recent keystrokes, until quiet or the hold hits
/// `max_hold`. Returns `Some(held_ms)` when it actually waited (so the caller
/// can audit the held duration), `None` when it was already quiet on entry.
///
/// Generic over the keystroke source and timings so the wiring — that the
/// loop consults the decision every `poll` and honours the starvation cap —
/// is integration-testable without a live PTY (see the #40 twice-bitten
/// lesson: the pure decision alone isn't enough; the loop that calls it must
/// be exercised too).
#[doc(hidden)] // pub for integration tests
pub fn hold_until_quiet<F: Fn() -> u64>(
    last_input_ms: F,
    quiet_window: Duration,
    max_hold: Duration,
    poll: Duration,
) -> Option<u64> {
    let start = std::time::Instant::now();
    let mut held = false;
    while should_hold_for_user(last_input_ms(), now_ms(), start.elapsed(), quiet_window, max_hold) {
        held = true;
        std::thread::sleep(poll);
    }
    held.then(|| start.elapsed().as_millis() as u64)
}

/// Production wrapper: hold delivery to `pty_id` while its human is typing,
/// using the shipped window/cap/poll timings.
pub(in crate::orchestration) fn wait_for_user_quiet(ptys: &crate::pty::PtyManager, pty_id: u32) -> Option<u64> {
    hold_until_quiet(
        || ptys.last_user_input_ms(pty_id).unwrap_or(0),
        USER_QUIET_HOLD,
        USER_QUIET_MAX_HOLD,
        USER_QUIET_POLL,
    )
}

/// #518: how long the human-input block may stand on a keystroke TIMESTAMP
/// alone — no new keystroke evidence, and no human characters outstanding in
/// the box — before it is treated as stale. The delivery-hold sibling of
/// #500's `DEFAULT_IDLE_TICK_INPUT_DEFER_MAX_MINUTES`, and the same principle
/// a third time: a suppression driven by a fallible signal must be BOUNDED,
/// because nothing else will ever clear it.
///
/// Ten minutes, chosen against the delivery machinery's own longest legitimate
/// window rather than picked round: `REINJECT_CONFIRM_TIMEOUT_MS` (5 min) is
/// already documented as "comfortably" longer than `deliver_prompt`'s entire
/// worst-case hold chain (two `USER_QUIET_MAX_HOLD` waits plus
/// `SUBMIT_MAX_WAIT` and the echo/retry window), so 2x that cannot elapse
/// inside any single delivery attempt — this bound can only ever fire on a
/// pane that has genuinely been sitting still.
///
/// Deliberately NOT a per-group guardrail, unlike its #500 sibling. That one
/// is classification-BLIND by design (it caps the raw timestamp whatever
/// produced it), so a group whose humans really do sit typing for 20 minutes
/// has a legitimate reason to want it longer. This one releases only on
/// POSITIVE evidence that there is nothing of the human's to clobber
/// (`input_pending` false — see `human_input_block`), so there is no workflow
/// for which a longer value is more correct, and a knob with no correct second
/// setting is a knob that only ever gets set wrong.
pub const HUMAN_INPUT_BLOCK_BOUND_MS: u64 = 10 * 60 * 1000;

/// Whether the human-input block on a delivery still stands (#518).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HumanInputBlock {
    /// No keystroke evidence at all since our own submit — `tier1_trusted`.
    /// Nothing to block on; this is the ordinary case.
    None,
    /// Human keystroke evidence stands: either it is recent, or characters
    /// the human typed are still sitting in the box. Never bounded out.
    Blocked,
    /// #518: the evidence is a timestamp older than `bound_ms` AND the box
    /// holds no human-typed characters. The block is released, and — unlike
    /// `None` — the caller knows the bound is WHY, so it can say so in the
    /// audit instead of releasing silently.
    BoundedOut,
}

impl HumanInputBlock {
    /// The boolean the old inline `last_user_input_ms > submit_sent_ms`
    /// derivations produced — "a human typed since our submit, so do not
    /// touch this box". `BoundedOut` reads FALSE here: that is the whole
    /// point of the bound.
    pub fn holds(self) -> bool {
        matches!(self, HumanInputBlock::Blocked)
    }
}

/// The ONE derivation of "has a human typed into this pane since our own
/// submit?" (#518). Every delivery-path consumer used to inline
/// `ptys.last_user_input_ms(pty) > submit_sent_ms` — `tier1_trusted`'s
/// inverse — and that expression is an unbounded LATCH: a single stamp landing
/// after our submit pins it true for the entire life of the delivery and its
/// late monitor (up to `LATE_MONITOR_MAX_LIFETIME`, four hours). #496 PR-A
/// (#499) made a false stamp much rarer by gating the stamp itself on
/// keystroke evidence, but "rarer" is not "never": that gate is a byte-shape
/// classification over an OPEN set of terminal auto-reply shapes, and #496's
/// own plan §7 left "which copilot emission recurs mid-session" unresolved.
/// #518 is that residue firing — a copilot orchestrator's prompt sat
/// unsubmitted under a badge that no longer had any live fact behind it, and
/// only a human's physical Enter recovered the group.
///
/// So the latch gets an aggregate bound, and the bound releases on EVIDENCE,
/// not merely on elapsed time:
///
/// - `tier1_trusted` first: with no stamp after our submit there is nothing
///   to bound, and this returns `None` exactly as before.
/// - **`box_pending` outranks the bound.** If the pane's occupancy counter
///   (#111/#171, `PtyManager::input_pending`) says human-typed characters are
///   still sitting in the box, the block NEVER times out, however stale the
///   timestamp is. This is what keeps #510's absolute — never submit over
///   genuine human content — absolute: the bound cannot release while there
///   is any human content to submit over. `box_occupancy_delta`'s own doc
///   commits to the direction that makes this sound ("deliberately biased to
///   never UNDER-count real occupancy"), so a `false` reading here is the
///   trustworthy one.
/// - Only then does time matter: a timestamp older than `bound_ms`, with an
///   empty box, is a fact about the past and not about the pane now.
///
/// `bound_ms == 0` disables the bound (pre-#518 behaviour) rather than making
/// it fire instantly — a 0 that meant "always bounded out" would turn a
/// mis-set constant into the exact clobber this guard exists to prevent.
///
/// Pure, and returning a three-way rather than a bool, so the release is
/// auditable at the call sites that have a seam and directly pinnable by
/// tests — including that `box_pending` beats the bound, which is the one
/// property a future edit must not reorder.
#[doc(hidden)] // pub for integration tests
pub fn human_input_block(
    last_user_input_ms: u64,
    submit_sent_ms: u64,
    box_pending: bool,
    now_ms: u64,
    bound_ms: u64,
) -> HumanInputBlock {
    if tier1_trusted(last_user_input_ms, submit_sent_ms) {
        return HumanInputBlock::None;
    }
    if box_pending {
        return HumanInputBlock::Blocked;
    }
    if bound_ms == 0 {
        return HumanInputBlock::Blocked;
    }
    if now_ms.saturating_sub(last_user_input_ms) >= bound_ms {
        HumanInputBlock::BoundedOut
    } else {
        HumanInputBlock::Blocked
    }
}

/// Production wrapper: `human_input_block` against a live pane, with the
/// shipped bound. A closed pty reads `last_user_input_ms` as `0`, which
/// `tier1_trusted` already resolves to `None` before `box_pending` is
/// consulted at all — the `unwrap_or(true)` below is the fail-safe direction
/// for a reading we cannot take, not a case this can actually reach.
pub(in crate::orchestration) fn human_input_block_now(
    ptys: &crate::pty::PtyManager,
    pty_id: u32,
    submit_sent_ms: u64,
) -> HumanInputBlock {
    human_input_block(
        ptys.last_user_input_ms(pty_id).unwrap_or(0),
        submit_sent_ms,
        ptys.input_pending(pty_id).unwrap_or(true),
        now_ms(),
        HUMAN_INPUT_BLOCK_BOUND_MS,
    )
}

/// The audit `reason`/`release` token recorded when #518's bound — not an
/// absence of keystroke evidence — is why a human-input block was not
/// honoured. Its own string so a grep can tell "no human ever typed" apart
/// from "a human typed, and we decided that fact had gone stale".
pub(in crate::orchestration) const HUMAN_INPUT_BLOCK_BOUND_REASON: &str = "human-input-block-bound";

/// How a single human write into a pane's input changes box occupancy (#111).
/// Classified from the keystroke's *content*, which is what tells a line still
/// sitting in the box from one already submitted — an output-byte heuristic
/// can't (one keystroke's input-line redraw, or ambient agent streaming, can
/// exceed any fixed burst floor, and a sub-floor submit never clears it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HumanInput {
    /// Printable text was entered — a line now sits unsubmitted in the box.
    Content,
    /// The line was submitted (Enter) or explicitly cleared — the box is empty.
    Submit,
    /// Navigation/editing that neither adds visible text nor submits (arrows,
    /// backspace, bare escape sequences) — box occupancy is unchanged **by
    /// this coarse read**. Backspace/DEL does in fact remove a character;
    /// `box_occupancy_delta` (#171) is the finer-grained sibling that tracks
    /// that, for callers that need to notice a typed line getting backspaced
    /// all the way back out rather than submitted.
    Neutral,
}

/// Classify one human write for the delivery paste guard (#111). Pure so the
/// rule is testable; `write_pty` calls it to maintain the per-pane
/// `input_pending` flag.
///
/// - A write carrying **bracketed-paste markers** (`ESC[200~` / `ESC[201~`) is
///   pasted text held UNSUBMITTED in the box → `Content`, even if it ends in a
///   newline: under bracketed-paste mode (Claude Code and most modern TUIs) a
///   pasted newline is literal, not a submit — the human's separate Enter
///   afterwards is the submit. Checked first so an interior/trailing newline
///   can't misread the paste as submitted (the #111 loss otherwise).
/// - Otherwise a carriage return / newline submits the current line, UNLESS
///   printable text follows the last newline (then that trailing text is a fresh
///   unsubmitted line → `Content`).
/// - Ctrl-U (kill-line) / Ctrl-C (interrupt) empty the box → `Submit`.
/// - Any remaining printable/graphic character (after skipping escape sequences)
///   → `Content`.
/// - Otherwise (arrows, backspace, lone escape sequences) → `Neutral`.
///
/// Erring toward `Content`/`Neutral` on ambiguous input keeps the guard biased
/// to the safe hold: a real sitting line is never misread as empty. This
/// three-way read alone can't recognize a line that was typed and then fully
/// backspaced back out (every backspace reads as `Neutral`, individually
/// indistinguishable from an arrow key) — that was issue #171: the box read
/// as occupied forever, holding every subsequent delivery until the 60s abort.
/// `write_pty` closes that gap by pairing this call with `box_occupancy_delta`,
/// which *does* count backspace/DEL as a removal. Other residual clears that
/// leave the flag stuck — Esc-to-clear, Ctrl-W/Ctrl-K, and soft-newline
/// editors — still need true box-occupancy detection and remain open.
pub fn classify_human_input(data: &str) -> HumanInput {
    // Bracketed paste: the text lands in the box unsubmitted regardless of any
    // newline it contains, so never read it as a submit.
    if data.contains(BRACKETED_PASTE_START) || data.contains(BRACKETED_PASTE_END) {
        return HumanInput::Content;
    }
    if let Some(pos) = data.rfind(['\r', '\n']) {
        // `\r`/`\n` are single-byte, so `pos + 1` is a valid char boundary.
        let after = &data[pos + 1..];
        return if input_has_printable(after) { HumanInput::Content } else { HumanInput::Submit };
    }
    // Line-clear controls empty the box even without a newline.
    const KILL_LINE: char = '\u{15}'; // Ctrl-U
    const INTERRUPT: char = '\u{03}'; // Ctrl-C
    if !data.is_empty() && data.chars().all(|c| c == KILL_LINE || c == INTERRUPT) {
        return HumanInput::Submit;
    }
    if input_has_printable(data) {
        HumanInput::Content
    } else {
        HumanInput::Neutral
    }
}

/// xterm bracketed-paste bracket sequences: the terminal wraps pasted text in
/// these so an app can tell a paste from typing (and hold pasted newlines soft).
const BRACKETED_PASTE_START: &str = "\u{1b}[200~";
const BRACKETED_PASTE_END: &str = "\u{1b}[201~";

/// Shared walk over a human write, skipping terminal escape sequences, that
/// backs both `input_has_printable` and `box_occupancy_delta`. Skips CSI
/// (`ESC [ … final`, e.g. arrow keys, bracketed-paste markers) AND the string
/// sequences a terminal emits in *reply* to a program's query — OSC (`ESC ]`)
/// and DCS/SOS/PM/APC (`ESC P`/`X`/`^`/`_`) — plus other short `ESC`-led
/// sequences, so none of their printable bytes read as typed content.
///
/// The OSC/DCS skip is #179: GitHub Copilot queries the terminal's colors
/// (`ESC]10;?`, `ESC]11;?`, `ESC]4;n;?`) and version (`ESC[>q`) at boot; the
/// webview's xterm auto-answers, and those answers reach us through `write_pty`
/// exactly like a keystroke. Their bodies are printable (`11;rgb:0d0d/1111/1717`),
/// so without skipping the whole string they were misread as a human's line,
/// wedging `input_pending` true and stalling the fresh-copilot kickoff paste in
/// the #111 box-clear hold (up to its 60s abort) — the "prompt never delivered"
/// symptom. Claude Code issues no such query, so only copilot tripped it.
///
/// #496: the same auto-replies reach `write_pty` with NO human present at
/// all, and until PR-A they *also* unconditionally re-stamped
/// `user_input_ms` — the keystroke-recency clock everything from the
/// autonomous idle tick to the stranded-text flush reads. This scan is now
/// consulted for THAT gate too (`PtyManager::note_user_input`): a write
/// classifies `Neutral` with a zero `occupancy_delta` — this skip is exactly
/// why — and `note_user_input` treats that combination as "not a keystroke",
/// so the timestamp isn't touched either. One classifier, two consumers; see
/// `note_user_input` for the gate and the tradeoff it accepts.
///
/// Returns `(has_printable, occupancy_delta)`: the first is "did this write
/// put visible text in the box at all" (`input_has_printable`'s job); the
/// second is the signed change to box occupancy — one Unicode CHARACTER
/// added counts `+1` regardless of its UTF-8 byte width, backspace/DEL
/// (`\x08`/`\x7f`) removes `1` (#171) — that `box_occupancy_delta` exposes so
/// a run of backspaces emptying a typed line is recognized even though no
/// single write in the run looks like a submit.
fn scan_box_units(s: &str) -> (bool, i32) {
    let b = s.as_bytes();
    let mut i = 0;
    let mut printable = false;
    let mut delta: i32 = 0;
    while i < b.len() {
        if b[i] == 0x1b {
            i += 1;
            match b.get(i) {
                // CSI: `ESC [` … final byte in 0x40..=0x7e.
                Some(b'[') => {
                    i += 1;
                    while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                        i += 1;
                    }
                    i += 1; // consume the CSI final byte
                }
                // OSC / DCS / SOS / PM / APC: a string sequence whose body is
                // arbitrary (often printable) text, terminated by BEL (0x07) or
                // ST (`ESC \`). Skip the whole thing — it's a query reply, not
                // typed input (#179).
                Some(b']') | Some(b'P') | Some(b'X') | Some(b'^') | Some(b'_') => {
                    i += 1;
                    while i < b.len() {
                        if b[i] == 0x07 {
                            i += 1; // BEL terminator
                            break;
                        }
                        if b[i] == 0x1b && b.get(i + 1) == Some(&b'\\') {
                            i += 2; // ST terminator (ESC \)
                            break;
                        }
                        i += 1;
                    }
                }
                // Any other 2-byte / lone ESC sequence (charset select, `ESC=`, …).
                _ => {
                    i += 1;
                }
            }
            continue;
        }
        // Backspace / DEL: one character removed from the box (#171).
        if b[i] == 0x08 || b[i] == 0x7f {
            delta -= 1;
            i += 1;
            continue;
        }
        // Printable ASCII, or a UTF-8 multibyte LEAD byte (0xC0..=0xFF): count
        // one occupancy unit per character, not per byte. A 3-byte CJK
        // character or a 4-byte emoji is still exactly one keystroke and one
        // backspace, so counting raw bytes here (+3/+4 on type, -1 on the one
        // backspace that removes it) over-counted occupancy for any non-ASCII
        // typer and reproduced #171's exact stuck-occupied symptom for them —
        // the counter never gets back to `<= 0` because the single removal
        // can't cancel a multi-byte addition. Continuation bytes (0x80..=0xBF)
        // are still consumed (and still mark the write as printable) but add
        // no further delta — they're part of the character its lead byte
        // already counted.
        if (0x20..0x7f).contains(&b[i]) || b[i] >= 0xc0 {
            printable = true;
            delta += 1;
            i += 1;
            continue;
        }
        if b[i] >= 0x80 {
            printable = true;
            i += 1; // continuation byte — already counted via its lead byte
            continue;
        }
        i += 1; // other C0 control (tab, etc.)
    }
    (printable, delta)
}

/// Whether `s` contains a graphic character once terminal escape sequences are
/// skipped — the test for "this write put visible text in the box". See
/// `scan_box_units` for what's skipped and why.
fn input_has_printable(s: &str) -> bool {
    scan_box_units(s).0
}

/// The signed change to box occupancy from a single human write: `+1` per
/// Unicode CHARACTER (a UTF-8 lead byte, not a raw byte — a 3-byte CJK
/// character or a 4-byte emoji is one keystroke, so it counts as one, not
/// three or four), `-1` per backspace/DEL, `0` for anything else (arrows,
/// bare escape sequences, an OSC/DCS query-reply echo — #179). Used alongside
/// `classify_human_input` to track real occupancy rather than a bare
/// pending/not flag: `write_pty` applies this to a running per-pane counter
/// (clamped at zero) so a typed line that gets fully backspaced out reads
/// back to empty even though no single write in the run looks like a submit
/// (#171) — `classify_human_input`'s `Submit` still resets the counter
/// directly to zero, which is exact where it applies (Enter, Ctrl-U,
/// Ctrl-C).
///
/// Counting by character rather than by byte matters for correctness, not
/// just cosmetics: byte-counting added 3/4 for one CJK/emoji character typed
/// but only subtracted 1 for the single backspace that removes it, so the
/// counter never returned to zero and a non-ASCII typer hit this exact
/// stuck-occupied symptom (a prior revision of this fix had that bug).
///
/// The counter is deliberately biased to never UNDER-count real occupancy
/// (never read `0`/empty while a human's line is in fact still sitting in the
/// box — that's the clobber hazard the whole #111 guard exists to prevent).
/// It may OVER-count, and that direction is safe: if some CLI's line editor
/// ever deletes more than one character for a single backspace/DEL byte (a
/// multi-codepoint grapheme cluster, say), this still only subtracts `1`,
/// leaving the counter higher than the box's real contents — worst case, a
/// delivery holds a little longer than strictly necessary before pasting,
/// never earlier. Only backspace/DEL is counted as removal at all: Ctrl-W
/// (delete word), Ctrl-K (kill to end) and Esc-to-clear editors still net `0`
/// here — full box-occupancy tracking for those remains open (see
/// `classify_human_input`).
pub fn box_occupancy_delta(s: &str) -> i32 {
    scan_box_units(s).1
}

/// One tick of the pre-paste human-input hold (#111): given whether a human's
/// line is still sitting in the box, decide whether to paste, keep holding, or
/// abort. Pure so the hold/abort rule is testable without a live PTY;
/// `hold_for_human_input` drives it. Mirrors `should_flush_before_paste` — a
/// small, total gate.
///
/// - `box_pending`: does the box still hold a human's unsubmitted line?
/// - `held` / `max_hold`: the bounded wait; at the cap we abort rather than
///   paste onto a line the human never cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteGate {
    /// Box is clear (or was never dirty) — paste the prompt.
    Paste,
    /// Human's line is still in the box — keep waiting.
    Hold,
    /// The box never cleared within the bound — do not paste; notify instead.
    Abort,
}

pub fn resolve_paste_gate(box_pending: bool, held: Duration, max_hold: Duration) -> PasteGate {
    if !box_pending {
        return PasteGate::Paste; // box is empty — paste the prompt
    }
    if held >= max_hold {
        return PasteGate::Abort; // bounded wait elapsed and the line never cleared
    }
    PasteGate::Hold
}

/// Outcome of the pre-paste human-input hold (#111): either the box is clear and
/// delivery may paste, or it never cleared and the delivery must abort. Carries
/// the held duration so the caller can audit how long it waited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteDecision {
    Paste { held_ms: u64 },
    Abort { held_ms: u64 },
}

/// Poll-and-hold loop that drives `resolve_paste_gate`: if a human's line is
/// sitting in the box (`box_pending`), block until they submit/clear it (the
/// flag flips false) or the bounded wait elapses, then return `Paste`/`Abort`.
/// Returns `Paste { held_ms: 0 }` immediately when the box is already clear.
///
/// Generic over the occupancy source and timings so the wiring — that the loop
/// re-reads the flag each poll and honours the abort cap — is integration-
/// testable without a live PTY (the #40 lesson: exercise the loop, not just the
/// pure decision).
#[doc(hidden)] // pub for integration tests
pub fn hold_for_human_input<P: Fn() -> bool>(
    box_pending: P,
    max_hold: Duration,
    poll: Duration,
) -> PasteDecision {
    if !box_pending() {
        return PasteDecision::Paste { held_ms: 0 };
    }
    let start = std::time::Instant::now();
    loop {
        let held = start.elapsed();
        match resolve_paste_gate(box_pending(), held, max_hold) {
            PasteGate::Paste => return PasteDecision::Paste { held_ms: held.as_millis() as u64 },
            PasteGate::Abort => return PasteDecision::Abort { held_ms: held.as_millis() as u64 },
            PasteGate::Hold => std::thread::sleep(poll),
        }
    }
}

/// Production wrapper: hold prompt delivery to `pty_id` while a human's line is
/// sitting in its input box, using the shipped cap / poll. A closed pty reads as
/// "not pending" so a dead pane never blocks the thread.
pub(in crate::orchestration) fn wait_for_box_clear(ptys: &crate::pty::PtyManager, pty_id: u32) -> PasteDecision {
    hold_for_human_input(
        || ptys.input_pending(pty_id).unwrap_or(false),
        HUMAN_INPUT_HOLD_MAX,
        HUMAN_INPUT_POLL,
    )
}

#[cfg(test)]
mod hold_tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(4);
    const CAP: Duration = Duration::from_secs(90);

    #[test]
    fn holds_while_human_typed_recently() {
        // Typed 1s ago (< 4s window), well under the cap: keep holding.
        assert!(should_hold_for_user(9_000, 10_000, Duration::from_secs(5), WINDOW, CAP));
    }

    #[test]
    fn proceeds_once_human_is_quiet() {
        // Last keystroke was 5s ago (> 4s window): deliver.
        assert!(!should_hold_for_user(5_000, 10_000, Duration::from_secs(2), WINDOW, CAP));
    }

    #[test]
    fn proceeds_when_nobody_typed() {
        // 0 == no keystroke ever recorded for this pane.
        assert!(!should_hold_for_user(0, 10_000, Duration::ZERO, WINDOW, CAP));
    }

    #[test]
    fn cap_forces_delivery_even_if_still_typing() {
        // Human is still typing (0ms ago) but the hold hit the 90s cap:
        // deliver anyway so reports aren't starved forever.
        assert!(!should_hold_for_user(10_000, 10_000, CAP, WINDOW, CAP));
        // One tick over the cap also delivers.
        assert!(!should_hold_for_user(10_000, 10_000, CAP + Duration::from_millis(1), WINDOW, CAP));
    }

    #[test]
    fn boundary_at_exactly_the_window_proceeds() {
        // `since == window` is not "< window", so it proceeds (quiet enough).
        assert!(!should_hold_for_user(6_000, 10_000, Duration::from_secs(1), WINDOW, CAP));
    }

    #[test]
    fn future_timestamp_does_not_underflow() {
        // A clock skew where last_input is "after" now must not panic or wrap;
        // saturating_sub yields 0 → within window → hold.
        assert!(should_hold_for_user(11_000, 10_000, Duration::from_secs(1), WINDOW, CAP));
    }
}
