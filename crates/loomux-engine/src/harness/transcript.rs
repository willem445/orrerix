//! The VT renderer: [`HarnessEvent`] in, terminal bytes out
//! (`doc/design/harness-adapters.md` §5).
//!
//! # Why a structured pane still writes to a terminal
//!
//! A structured pane has no PTY, so nothing produces bytes for its `OutputBuf`
//! ring on its own. loomux produces them: this module turns the event stream
//! into VT output, and those bytes go into that ring through the same coalescer
//! that feeds `pty-output` today.
//!
//! That is what keeps `get_output`, termgrid replay, thumbnails,
//! `last_exit_tail` and #888's replay-on-attach working with **no API change**.
//!
//! **This is one of §5.1's two projections, and it is not the one the human
//! looks at.** The visible surface is a DOM renderer fed the events themselves
//! (#2891); the ring is fed from the same log, which is why serving those five
//! consumers costs nothing there. Neither projection is derived from the other.
//!
//! # The one rule this module must not break
//!
//! > **The renderer never rewrites bytes it has already emitted.**
//!
//! Lines are wrapped to the pane's columns at emit time; a later width change
//! re-wraps nothing, and reflow of already-emitted output is xterm's own,
//! exactly as for a PTY pane.
//!
//! CLAUDE.md constraint 1 forbids resizing a PTY for a UI feature because the
//! repaint pollutes scrollback. A structured pane has no PTY to resize, so the
//! constraint cannot be violated the usual way — but a renderer that re-emitted
//! its transcript on a width change would produce **the same damage by a
//! different road**, and a worse one, because grepping for the resize call would
//! not find it. So this module emits forward only: no cursor movement, no
//! erase, and no **bare** carriage return — a `CR` that is not part of a `CRLF`
//! is the one that returns the cursor without advancing.
//!
//! That is true of the bytes loomux authors *and* of the bytes it passes
//! through, which is the harder half: a model's own text, a tool's name and a
//! rendered argument preview all reach the stream, and a JSON string carries
//! `CR` and `ESC` perfectly well. [`Renderer::wrapped`] neutralizes every
//! control character in them, at the one point all three cross — see its doc for
//! why the filter is there and not at the call sites.
//! [`no_output_can_rewrite_the_screen`] is the pin, and it bans that shape
//! specifically; an earlier version banned every `CR`, which forbade the line
//! ending this module is required to emit (see [`NEWLINE`]).
//!
//! # What it deliberately does not do
//!
//! Wrapping counts **characters, not display columns**. A CJK or emoji glyph
//! occupies two cells and is counted as one here, so a line of wide characters
//! wraps late by up to its own width. Correct wrapping needs a Unicode width
//! table, which is a dependency; this is an engine leaf whose dependency budget
//! is `serde_json` and `std` (§8.1). The residual is stated rather than hidden,
//! and it degrades to "a line wraps in a slightly different place", never to
//! corrupted output — the bytes are still a valid stream, and xterm reflows them
//! itself.

use super::{
    CompactTrigger, Cost, Decision, DecisionSource, Harness, HarnessEvent, NoteKind,
    StopReason, UiAnswer, UiMethod, Usage,
};

/// How much of a tool call's arguments is drawn.
///
/// A tool call is **one collapsed line**: the human is watching what the agent
/// is doing, not reading its arguments, and an unbounded `input` (a whole file
/// in a `Write`) would push everything else off the screen. The full value is in
/// the pane's event log either way.
pub const TOOL_PREVIEW_BYTES: usize = 120;

/// The line ending every emit site in this module uses — **`CRLF`, not a bare
/// `LF`**, and it is load-bearing rather than pedantic.
///
/// These bytes end at `this.term.write(chunk)` (`src/pane.ts`) on a `Terminal`
/// built without `convertEol`, which xterm.js defaults to `false`. With it
/// false a bare `LF` is an **INDEX** — down one row, column untouched — so
/// every line staircases rightward and the transcript composes into something
/// no terminal would show. A PTY never exposes this, because ConPTY and a POSIX
/// pty in `ONLCR` both deliver `CRLF`; a *synthesized* stream has to do it
/// itself. `painted()` in `src-tauri/tests/orchestration.rs` states the same
/// rule for the same reason on the fixture side, and the two existing
/// loomux-authored injections into a pane go through `term.writeln`, which
/// appends `CRLF` for them.
///
/// **This is not in tension with the forward-only rule above.** A `CR` that is
/// part of a `CRLF` is a newline; only a *bare* `CR` — one not followed by `LF`
/// — returns the cursor without advancing, which is what can repaint a line.
/// [`no_output_can_rewrite_the_screen`] bans exactly that, and an earlier
/// version of it banned every `CR`, which forbade this fix and would have left
/// a green suite asserting the defect as the contract.
///
/// Fixing this by setting `convertEol: true` on the shared `Terminal` was
/// rejected: that terminal is every pane's, so it would change how a real PTY
/// pane parses a lone `LF` too.
const NEWLINE: &str = "\r\n";

/// Dim, for loomux's own annotations.
const DIM: &str = "\x1b[2m";
/// Reset. Every sequence this module opens is closed on the same line, so a
/// truncated stream can never leave the pane's terminal in an attribute state.
const RESET: &str = "\x1b[0m";

/// Renders one pane's event stream.
///
/// Stateful in exactly one respect — the current column — because [`Text`] is a
/// **delta**: consecutive events continue a line rather than each starting one,
/// and wrapping is only correct if the renderer remembers how far along that
/// line it is.
///
/// [`Text`]: HarnessEvent::Text
#[derive(Debug)]
pub struct Renderer {
    cols: usize,
    col: usize,
    harness: Option<Harness>,
}

impl Renderer {
    /// `cols` is the pane's width at the time the pane was created. It is not
    /// updated on resize, and that is the rule above rather than an oversight.
    ///
    /// A `0` — which a UI really does report mid-layout — is treated as **1**,
    /// the narrowest real terminal, not as a guessed comfortable width. The
    /// floor exists only so the wrapper cannot be asked to fit a character into
    /// no columns; anything above it is the caller's number and is honoured.
    /// An earlier draft clamped to 20 and CI caught it immediately: that floor
    /// silently overrode every genuinely narrow pane, which is a made-up width
    /// rendered as though it were the pane's own.
    pub fn new(cols: u16) -> Self {
        Renderer {
            cols: (cols as usize).max(1),
            col: 0,
            harness: None,
        }
    }

    /// The same renderer, told which harness it is rendering.
    ///
    /// **This exists because #2850 made a literal false.** The `Booted` line
    /// used to open `"claude · <model> · <session>"` — correct while there was
    /// one harness, and a lie on the pi pane this slice adds. The fix is not a
    /// branch at the render site: CLAUDE.md's rule is that a per-CLI identity
    /// string is READ off the source and never branched on, because
    /// `if claude { "claude" } else { "pi" }` hands the third harness the
    /// else-branch and reports the wrong CLI with nothing red to say so. So the
    /// name comes from [`Harness::as_str`], the one table.
    ///
    /// [`Renderer::new`] stays, and a renderer built that way names no CLI at
    /// all rather than guessing one — the PTY adapter (#888 A4-18′) has no
    /// [`Harness`] to give.
    pub fn for_harness(harness: Harness, cols: u16) -> Self {
        Renderer {
            harness: Some(harness),
            ..Renderer::new(cols)
        }
    }

    /// Render one event. Returns the bytes to append to the pane's ring — empty
    /// when the event draws nothing.
    pub fn render(&mut self, ev: &HarnessEvent) -> Vec<u8> {
        let mut out = String::new();
        match ev {
            HarnessEvent::Booted {
                session,
                model,
                capabilities: _,
            } => {
                let model = model.as_deref().unwrap_or("model unknown");
                let session = session.as_deref().unwrap_or("session not yet known");
                let who = match self.harness {
                    Some(h) => format!("{} · ", h.as_str()),
                    None => String::new(),
                };
                self.meta(&mut out, &format!("{who}{model} · {session}"));
            }
            HarnessEvent::TurnStarted { .. } => {
                // A blank line between turns and nothing else. The turn number
                // is loomux's own counter (see `TurnId`); printing it would put
                // an internal identity on the human's screen.
                self.newline(&mut out);
                self.newline(&mut out);
            }
            HarnessEvent::Text { delta, .. } => self.text(&mut out, delta),
            HarnessEvent::ToolCall { name, input, .. } => {
                let preview = preview_input(input);
                self.meta(&mut out, &format!("> {name}({preview})"));
            }
            HarnessEvent::ToolResult { ok, .. } => {
                self.meta(&mut out, if *ok { "  ok" } else { "  failed" });
            }
            HarnessEvent::PermissionRequest { tool, input, .. } => {
                let preview = preview_input(input);
                // Not dim: this is the one line that is waiting for a human, and
                // dimming it would put the thing the pane is blocked on in the
                // least visible style on the screen.
                self.newline(&mut out);
                self.line(&mut out, &format!("[permission] {tool}({preview})"));
            }
            HarnessEvent::PermissionSettled { decision, by, .. } => {
                let d = match decision {
                    Decision::Allow => "allowed",
                    Decision::Deny => "denied",
                };
                let by = match by {
                    DecisionSource::Policy => "by policy",
                    DecisionSource::Human => "by the human",
                    DecisionSource::PaneExited => "— the pane exited first",
                };
                self.meta(&mut out, &format!("  {d} {by}"));
            }
            HarnessEvent::TurnEnded {
                usage, cost, stop, ..
            } => {
                self.meta(&mut out, &turn_summary(usage.as_ref(), cost.as_ref(), stop));
            }
            HarnessEvent::Compacted {
                trigger,
                pre_tokens,
            } => {
                let t = match trigger {
                    CompactTrigger::Manual => "manual",
                    CompactTrigger::Auto => "auto",
                };
                let before = pre_tokens
                    .map(|n| format!(", {n} tokens before"))
                    .unwrap_or_default();
                self.meta(&mut out, &format!("-- compacted ({t}{before})"));
            }
            HarnessEvent::Exited { code } => {
                let code = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into());
                self.meta(&mut out, &format!("-- exited ({code})"));
            }
            // A structured pane never emits one, and a PTY pane's bytes are its
            // own — rendering inferred evidence into a transcript would put a
            // heuristic on screen in the same style as a reported fact, which is
            // the confusion `ObservedEvent` exists to prevent.
            HarnessEvent::Observed(_) => {}

            // ── #2850: the additive variants ────────────────────────────
            //
            // Two of the five draw NOTHING here, and that is a decision with a
            // reason rather than an omission.
            //
            // **Where the streamed bulk IS visible today, stated plainly: the
            // per-pane event log and machine consumers, and no human surface.**
            // This renderer is the only human surface a structured pane has
            // (§5.1) — an earlier draft of this comment justified the silence by
            // pointing at a DOM renderer of `HarnessEvent`, and no such thing
            // exists in this repo. That paragraph was written against #2850
            // S1a's PROPOSED "two projections" amendment, which is unmerged; on
            // `main`, §5.1 rejects a DOM transcript view outright. The decision
            // below stands on its own grounds; the projection it appealed to did
            // not exist.
            //
            // `Thinking` follows the rule the Claude decoder already applies to a
            // `thinking` block (`claude::Decoder::assistant`): reasoning is not
            // the transcript of what the agent did. #2891 asks for it to be
            // quietable, which a VT stream cannot offer — so the choice is
            // between always showing it and never showing it, and never is the
            // one that matches every other pane in the app. Rendering it would
            // also put the highest-volume thing pi produces into a thumbnail.
            HarnessEvent::Thinking { .. } => {}
            // `ToolOutput` is the same argument by volume, and by parity: a
            // `ToolCall` line and its `  ok`/`  failed` are what this projection
            // has always shown for a tool on BOTH harnesses — #2850 asks for tool
            // executions as collapsed blocks, which is what that already is — and
            // streaming a tool's bytes into the ring on pi alone would make the
            // same session look different by harness for no reported difference.
            //
            // The cost is real and is not hidden: a pi tool's streamed output has
            // no human surface today. Giving it one is a rendering decision for
            // the slice that owns the human surface, and it has the event log to
            // build from.
            HarnessEvent::ToolOutput { .. } => {}

            // The other three DO draw, because each is something a human
            // reading a pane has to see happen.
            HarnessEvent::UiRequest {
                method,
                title,
                message,
                options,
                ..
            } => {
                let m = match method {
                    UiMethod::Select => "select",
                    UiMethod::Confirm => "confirm",
                    UiMethod::Input => "input",
                    UiMethod::Editor => "editor",
                };
                let what = title
                    .as_deref()
                    .or(message.as_deref())
                    .unwrap_or("(no title)");
                let opts = if options.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", options.join(", "))
                };
                // Not dim, for `PermissionRequest`'s reason: this is a line the
                // pane is BLOCKED on, and dimming it would put the thing a human
                // has to answer in the least visible style on the screen.
                self.newline(&mut out);
                self.line(&mut out, &format!("[dialog {m}] {what}{opts}"));
            }
            HarnessEvent::UiSettled { answer, by, .. } => {
                let a = match answer {
                    UiAnswer::Value(v) => format!("answered {v:?}"),
                    UiAnswer::Confirmed(true) => "confirmed".to_string(),
                    UiAnswer::Confirmed(false) => "declined".to_string(),
                    UiAnswer::Cancelled => "cancelled".to_string(),
                };
                let by = match by {
                    DecisionSource::Policy => "by policy",
                    DecisionSource::Human => "by the human",
                    DecisionSource::PaneExited => "— the pane exited first",
                };
                self.meta(&mut out, &format!("  {a} {by}"));
            }
            // COUNTS, never the queued text. The strings are another pane's
            // prompts and can be arbitrarily long; what a human needs from this
            // event is that a delivered turn is waiting rather than running.
            HarnessEvent::QueueChanged {
                steering,
                follow_up,
            } => {
                if steering.is_empty() && follow_up.is_empty() {
                    // The queue draining to empty is the normal end of every
                    // queued turn; a line for it would be one per delivery.
                    return Vec::new();
                }
                self.meta(
                    &mut out,
                    &format!(
                        "-- queued: {} steering, {} follow-up",
                        steering.len(),
                        follow_up.len()
                    ),
                );
            }
            // A note DOES draw here, unlike `Thinking` and `ToolOutput`, and the
            // difference is volume against value: there are three kinds, each is
            // rare, and each is something a human staring at a stalled pane needs
            // to see. A retry loop that drew nothing is the failure this variant
            // was added for (#2850 S1b).
            HarnessEvent::Note { note, text, .. } => {
                let tag = match note {
                    NoteKind::Retry => "retry",
                    NoteKind::Error => "error",
                    NoteKind::Ui => "note",
                };
                self.meta(&mut out, &format!("-- [{tag}] {text}"));
            }
        }
        out.into_bytes()
    }

    /// A loomux annotation: its own line, dimmed, wrapped.
    fn meta(&mut self, out: &mut String, s: &str) {
        self.newline(out);
        out.push_str(DIM);
        self.wrapped(out, s);
        out.push_str(RESET);
        self.newline(out);
    }

    /// A line of loomux's own, undimmed.
    fn line(&mut self, out: &mut String, s: &str) {
        self.newline(out);
        self.wrapped(out, s);
        self.newline(out);
    }

    /// Model text: continues the current line, wraps, honours its own newlines.
    fn text(&mut self, out: &mut String, s: &str) {
        for (i, part) in s.split('\n').enumerate() {
            if i > 0 {
                self.hard_newline(out);
            }
            self.wrapped(out, part);
        }
    }

    /// Emit `s`, breaking at `cols`.
    ///
    /// Breaks at the column, not at a word boundary. Word wrapping would need to
    /// buffer a word before deciding, and this renderer is fed **deltas** — a
    /// word routinely arrives split across two events, so a word-wrapper would
    /// either hold text back (latency the human sees as stalling) or break in
    /// the wrong place anyway.
    /// **Every byte that reaches this function is CONTENT, and content is
    /// neutralized.** That is what makes the forward-only rule in this module's
    /// header true rather than aspirational, and it is why the filter lives here
    /// instead of at the call sites.
    ///
    /// Three callers feed this text loomux did not author: [`Self::text`] (a
    /// `HarnessEvent::Text` delta, handed through by the decoder verbatim) and
    /// [`Self::meta`]/[`Self::line`], whose format strings embed a tool's
    /// `name`, a permission's `tool` and a rendered `preview`. A JSON string
    /// carries `CR` and `ESC` perfectly well, so without this an agent's own
    /// output reaches `term.write` unchanged and can repaint its line
    /// (`…approved\rDENIED`), reach onto a line already drawn (`ESC[A`), or
    /// **erase the whole pane** (`ESC[2J`) — loomux's own header line with it.
    /// That is CLAUDE.md constraint 1's damage arriving by exactly the road the
    /// design note's §5.2 says it cannot.
    ///
    /// loomux's own control bytes never pass through here: [`Self::meta`] pushes
    /// `DIM`/`RESET` around this call, and [`NEWLINE`] is emitted by the wrap
    /// below and by [`Self::newline`]. So the rule "everything this function
    /// receives is untrusted" holds with no exception to remember, which a
    /// per-call-site filter would not give — [`preview_input`] is the proof of
    /// that: it stripped `CR`/`LF` from a tool's arguments and let `ESC`
    /// through, and model text got neither.
    ///
    /// A control becomes a **space** rather than being dropped: the column
    /// arithmetic below counts what it emits, and a dropped character would let
    /// a line silently exceed `cols`.
    fn wrapped(&mut self, out: &mut String, s: &str) {
        for c in s.chars() {
            if self.col >= self.cols {
                out.push_str(NEWLINE);
                self.col = 0;
            }
            out.push(if c.is_control() { ' ' } else { c });
            self.col += 1;
        }
    }

    /// Start a new line unless already at column 0.
    fn newline(&mut self, out: &mut String) {
        if self.col > 0 {
            out.push_str(NEWLINE);
            self.col = 0;
        }
    }

    /// Start a new line even at column 0 — a blank line the model asked for.
    fn hard_newline(&mut self, out: &mut String) {
        out.push_str(NEWLINE);
        self.col = 0;
    }
}

/// A tool call's arguments, on one line, bounded.
///
/// A `command` string is shown as itself, because for `Bash` it is the whole
/// point and the surrounding JSON is noise. Anything else is compact JSON.
fn preview_input(input: &serde_json::Value) -> String {
    let raw = match input.get("command").and_then(serde_json::Value::as_str) {
        Some(cmd) => cmd.to_string(),
        None => match input {
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        },
    };
    // The "one collapsed line each" property is held by [`Renderer::wrapped`],
    // which turns every control character — `LF` and `CR` included — into a
    // space at the one point all pass-through text crosses.
    //
    // This function used to carry its own `replace(['\n', '\r'], " ")`. It is
    // gone rather than kept as belt-and-braces, and that is the point of the
    // finding that put the filter in `wrapped`: a second, weaker copy of one
    // rule is how the two drift, and this copy WAS the weaker one — it stripped
    // `CR`/`LF` from a tool's arguments while letting `ESC` through, and model
    // text got neither. A round that removed it stopped reddening once `wrapped`
    // covered the same ground, which is what a subsumed rule looks like.
    let (cut, truncated) = super::truncate_on_char_boundary(&raw, TOOL_PREVIEW_BYTES);
    if truncated {
        format!("{cut}…")
    } else {
        cut.to_string()
    }
}

/// The one-line summary a turn ends on.
///
/// Reads [`Usage::call_cumulative`] and says **cumulative**, because that figure
/// is the running total for the whole call and not this turn's spend — a summary
/// that printed it as a per-turn number would be the exact misreading §7's traps
/// exist to prevent, on the most-read surface there is.
fn turn_summary(usage: Option<&Usage>, cost: Option<&Cost>, stop: &StopReason) -> String {
    let stop = match stop {
        StopReason::Completed => "done".to_string(),
        StopReason::MaxTurns => "stopped: turn limit".to_string(),
        StopReason::Aborted => "stopped: aborted".to_string(),
        StopReason::Error => "stopped: error".to_string(),
        StopReason::Other(s) if s.is_empty() => "stopped".to_string(),
        StopReason::Other(s) => format!("stopped: {s}"),
    };
    let mut parts = vec![format!("-- {stop}")];
    if let Some(u) = usage {
        parts.push(format!("{} tokens (call total)", u.call_cumulative.total()));
    }
    if let Some(c) = cost {
        // "est." is not decoration: the vendor calls this figure a client-side
        // estimate and says not to make financial decisions from it, so the
        // screen says estimate too.
        parts.push(format!("${:.4} est.", c.usd));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{ObservedEvent, RequestId, Tokens, ToolUseId, TurnId};

    fn render_all(cols: u16, evs: &[HarnessEvent]) -> String {
        let mut r = Renderer::new(cols);
        let mut out = Vec::new();
        for ev in evs {
            out.extend(r.render(ev));
        }
        String::from_utf8(out).expect("the renderer must emit valid UTF-8")
    }

    #[test]
    fn no_output_can_rewrite_the_screen() {
        // §5.2's rule, as the assertion the module header names. The renderer is
        // forward-only: any cursor movement, erase, or bare carriage return
        // would let it repaint, which is CLAUDE.md constraint 1's damage
        // arriving without a ConPTY resize to grep for.
        let out = render_all(
            40,
            &[
                HarnessEvent::Booted {
                    session: Some("s".into()),
                    model: Some("opus".into()),
                    capabilities: vec![],
                },
                HarnessEvent::TurnStarted { turn: TurnId(0) },
                HarnessEvent::Text {
                    turn: TurnId(0),
                    delta: "a very long line that certainly wraps past forty columns".into(),
                },
                // The subject the assertions below need, and without which this
                // test's population could not violate any of them: text loomux
                // did not author, carrying the three shapes it bans. Every
                // event above is loomux-authored and contains no CR and no ESC,
                // so the loops passed over a string that could not have held
                // the shape — an absence assertion over the wrong population.
                HarnessEvent::Text {
                    turn: TurnId(0),
                    delta: "approved\rDENIED \x1b[2m\x1b[A\x1b[2J\x1b[K done".into(),
                },
                // The same bytes by the OTHER route into `wrapped`: a tool's
                // name and its rendered argument preview are interpolated into
                // a loomux-authored line, so a filter applied only to `Text`
                // would leave this one open.
                HarnessEvent::ToolCall {
                    turn: TurnId(0),
                    id: ToolUseId("t2".into()),
                    name: "Ba\x1b[Ash".into(),
                    input: serde_json::json!({"command": "echo hi\rrm -rf /\x1b[2J"}),
                },
                HarnessEvent::ToolCall {
                    turn: TurnId(0),
                    id: ToolUseId("t".into()),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "git status"}),
                },
                HarnessEvent::TurnEnded {
                    turn: TurnId(0),
                    usage: None,
                    cost: None,
                    stop: StopReason::Completed,
                },
            ],
        );
        // A BARE carriage return — one not followed by `LF` — is what can
        // repaint a line, and it is what this bans. An earlier version banned
        // every `\r`, which is the wrong rule stated with the right words: it
        // forbade the CRLF this module must emit (see `NEWLINE`), so it would
        // have reddened on the fix and left a green suite asserting the defect
        // as the contract.
        let bytes: Vec<char> = out.chars().collect();
        for (i, c) in bytes.iter().enumerate() {
            if *c == '\r' {
                assert_eq!(
                    bytes.get(i + 1),
                    Some(&'\n'),
                    "a bare CR at {i} can return the cursor without advancing, \
                     which repaints the line: {out:?}"
                );
            }
        }
        // The other direction, and the one that actually broke: a lone `LF` is
        // an INDEX on a terminal without `convertEol`, so every line after it
        // staircases. Every newline this module emits must be a full CRLF.
        for (i, c) in bytes.iter().enumerate() {
            if *c == '\n' {
                assert_eq!(
                    if i == 0 { None } else { bytes.get(i - 1) },
                    Some(&'\r'),
                    "a lone LF at {i} — the transcript staircases from here: {out:?}"
                );
            }
        }
        // Positive control for the pair above: this output really does contain
        // newlines, so neither loop is passing over a string with none.
        assert!(
            out.matches("\r\n").count() >= 3,
            "the two assertions above must have had CRLFs to inspect: {out:?}"
        );
        for seq in ["\x1b[A", "\x1b[B", "\x1b[C", "\x1b[D", "\x1b[H", "\x1b[J", "\x1b[K", "\x1b[s", "\x1b[u"] {
            assert!(
                !out.contains(seq),
                "the renderer emitted {seq:?}, which can rewrite what it already \
                 drew: {out:?}"
            );
        }
        // The positive control: it did emit SOMETHING, so the absences above are
        // about the content and not about an empty string.
        assert!(out.contains("git status"), "{out:?}");
        // And the filter NEUTRALIZES rather than deletes: the pass-through text
        // is still legible with its controls turned into spaces. An assertion
        // that only checked the absences would pass just as well on a renderer
        // that dropped the delta entirely.
        assert!(
            out.contains("approved DENIED"),
            "a control must become a space, not vanish with its neighbours: {out:?}"
        );
        // `\x1b[Ash` is ESC + the printable `[Ash`, so only the ESC is
        // neutralized — the bracket and letters are ordinary text and stay.
        assert!(
            out.contains("Ba [Ash("),
            "the same rule applies to an interpolated tool name: {out:?}"
        );
        // Every attribute it opens, it closes on the same pass.
        assert_eq!(
            out.matches(DIM).count(),
            out.matches(RESET).count(),
            "an unbalanced SGR leaves the pane's terminal in an attribute state: {out:?}"
        );
    }

    #[test]
    fn text_deltas_continue_one_line_and_wrap_at_the_column() {
        // The property that makes `Text` a delta rather than a line: three
        // events, one paragraph. A renderer that started a line per event would
        // produce three lines here and pass any test that only checked content.
        let out = render_all(
            10,
            &[
                HarnessEvent::Text {
                    turn: TurnId(0),
                    delta: "abcde".into(),
                },
                HarnessEvent::Text {
                    turn: TurnId(0),
                    delta: "fghij".into(),
                },
                HarnessEvent::Text {
                    turn: TurnId(0),
                    delta: "klmno".into(),
                },
            ],
        );
        assert_eq!(
            out, "abcdefghij\r\nklmno",
            "wrapped at 10, not per event, and the break is a CRLF: {out:?}"
        );
    }

    #[test]
    fn a_newline_inside_model_text_is_honoured_and_a_wrap_is_not_a_paragraph() {
        let out = render_all(
            80,
            &[HarnessEvent::Text {
                turn: TurnId(0),
                delta: "one\n\ntwo".into(),
            }],
        );
        assert_eq!(out, "one\r\n\r\ntwo");
    }

    #[test]
    fn a_tool_call_is_one_line_however_long_its_input_is() {
        // The collapsed-line property, tested on the input that actually breaks
        // it: a `command` string carrying REAL newlines. A structured argument
        // would not — `Value::to_string` escapes newlines to `\n`, so that
        // fixture is already one line and the test would pass with the flattening
        // deleted. Length alone would not either; truncation hides it.
        let out = render_all(
            200,
            &[HarnessEvent::ToolCall {
                turn: TurnId(0),
                id: ToolUseId("t".into()),
                name: "Bash".into(),
                input: serde_json::json!({"command": "echo one\necho two\necho three"}),
            }],
        );
        let body = out.replace(DIM, "").replace(RESET, "");
        // Split on the CRLF this module emits, not on a bare `\n`: splitting on
        // `\n` leaves a `\r` on every part, so an empty line reads as non-empty
        // and the count below silently stops meaning "lines drawn".
        let drawn: Vec<&str> = body.split(NEWLINE).filter(|l| !l.is_empty()).collect();
        assert_eq!(drawn.len(), 1, "a tool call must draw exactly one line: {drawn:?}");
        assert!(drawn[0].starts_with("> Bash("), "{drawn:?}");
        assert!(
            drawn[0].contains("echo one echo two"),
            "the newlines must become spaces, not line breaks — held by \
             `wrapped`'s control filter, which is why this test reddens with \
             that filter removed rather than with a preview-local strip: {drawn:?}"
        );
    }

    #[test]
    fn a_long_tool_preview_is_truncated_and_says_so() {
        let long = "x".repeat(TOOL_PREVIEW_BYTES + 50);
        let out = render_all(
            500,
            &[HarnessEvent::ToolCall {
                turn: TurnId(0),
                id: ToolUseId("t".into()),
                name: "Bash".into(),
                input: serde_json::json!({ "command": long }),
            }],
        );
        assert!(out.contains('…'), "truncation must be visible: {out:?}");
        let xs = out.matches('x').count();
        assert_eq!(xs, TOOL_PREVIEW_BYTES, "truncated to the cap, not to a guess");
    }

    #[test]
    fn a_permission_request_is_the_one_line_that_is_not_dimmed() {
        // It is what the pane is blocked on, so it must not be drawn in the
        // least visible style on the screen. Asserted as a CONTRAST against a
        // neighbouring meta line, so a change that dimmed everything fails here
        // rather than passing because nothing is dim.
        let out = render_all(
            80,
            &[
                HarnessEvent::ToolCall {
                    turn: TurnId(0),
                    id: ToolUseId("t".into()),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "rm -rf /"}),
                },
                HarnessEvent::PermissionRequest {
                    id: RequestId("r".into()),
                    tool: "Bash".into(),
                    input: serde_json::json!({"command": "rm -rf /"}),
                },
            ],
        );
        let perm_line = out
            .lines()
            .find(|l| l.contains("[permission]"))
            .expect("the request must be drawn");
        assert!(
            !perm_line.contains(DIM),
            "the blocked-on line must not be dimmed: {perm_line:?}"
        );
        let tool_line = out
            .lines()
            .find(|l| l.contains("> Bash("))
            .expect("the tool call must be drawn");
        assert!(
            tool_line.contains(DIM),
            "the control: an ordinary meta line IS dimmed, so the assertion \
             above is about contrast rather than about nothing being dim: {tool_line:?}"
        );
    }

    #[test]
    fn the_turn_summary_says_the_token_figure_is_a_call_total_and_the_cost_an_estimate() {
        // §7's two traps on the most-read surface there is. A summary that
        // printed the cumulative figure as this turn's spend, or a vendor
        // estimate as a bill, would be wrong in a way only prose catches.
        let out = render_all(
            120,
            &[HarnessEvent::TurnEnded {
                turn: TurnId(0),
                usage: Some(Usage {
                    call_cumulative: Tokens {
                        input: 100,
                        output: 20,
                        cache_read: 5,
                        cache_creation: 1,
                    },
                    this_turn_main_loop: Some(Tokens {
                        input: 10,
                        output: 2,
                        cache_read: 0,
                        cache_creation: 0,
                    }),
                    per_model: vec![],
                }),
                cost: Some(Cost {
                    usd: 0.0731,
                    basis: crate::harness::CostBasis::HarnessEstimate,
                }),
                stop: StopReason::Completed,
            }],
        );
        assert!(out.contains("126 tokens (call total)"), "{out:?}");
        assert!(
            !out.contains("12 tokens"),
            "the per-turn figure must not be the one drawn: {out:?}"
        );
        assert!(out.contains("$0.0731 est."), "{out:?}");
    }

    #[test]
    fn an_unknown_stop_reason_reaches_the_screen_instead_of_being_flattened() {
        let out = render_all(
            120,
            &[HarnessEvent::TurnEnded {
                turn: TurnId(0),
                usage: None,
                cost: None,
                stop: StopReason::Other("rapid_refill_breaker".into()),
            }],
        );
        assert!(
            out.contains("rapid_refill_breaker"),
            "a reason this build does not know is exactly what a human \
             debugging a stuck pane needs: {out:?}"
        );
    }

    #[test]
    fn inferred_evidence_is_never_drawn() {
        // A structured pane emits none of these; the pin is that if one ever
        // reached this renderer it would not be painted in the same style as a
        // reported fact.
        let out = render_all(
            80,
            &[
                HarnessEvent::Observed(ObservedEvent::ReadyMarker),
                HarnessEvent::Observed(ObservedEvent::QuestionSuspected {
                    matched: "Do you want to proceed?".into(),
                }),
                HarnessEvent::Observed(ObservedEvent::Quiet),
            ],
        );
        assert!(out.is_empty(), "inferred evidence must draw nothing: {out:?}");
    }

    #[test]
    fn a_zero_width_pane_terminates_and_is_not_widened_to_a_guess() {
        // Two properties, and the second is the one an earlier draft got wrong.
        // A zero must not make the wrapper unable to place a character — and it
        // must not be silently replaced by a comfortable width either, because
        // that renders a number the pane never reported.
        let out = render_all(
            0,
            &[HarnessEvent::Text {
                turn: TurnId(0),
                delta: "abc".into(),
            }],
        );
        assert_eq!(
            out, "a\r\nb\r\nc",
            "zero is treated as one column, not as twenty"
        );
        // The control: a real narrow width is honoured exactly, so the floor
        // above cannot be creeping upward unnoticed.
        let out = render_all(
            3,
            &[HarnessEvent::Text {
                turn: TurnId(0),
                delta: "abcdef".into(),
            }],
        );
        assert_eq!(out, "abc\r\ndef");
    }

    // ── #2850 ───────────────────────────────────────────────────────────────

    fn render_for(h: Harness, evs: &[HarnessEvent]) -> String {
        let mut r = Renderer::for_harness(h, 200);
        let mut s = Vec::new();
        for e in evs {
            s.extend(r.render(e));
        }
        String::from_utf8(s).unwrap()
    }

    #[test]
    fn the_boot_line_names_the_harness_it_is_rendering_and_never_a_literal() {
        // The regression #2850 would otherwise have shipped: this line read
        // `"claude · {model} · {session}"` as a LITERAL, which was true while
        // there was one harness and false on the pi pane this slice adds. No test
        // pinned it, which is why it survived — so it is pinned now, on both
        // harnesses at once so a name table with one right row cannot pass.
        let booted = HarnessEvent::Booted {
            session: Some("s-1".into()),
            model: Some("m-1".into()),
            capabilities: vec![],
        };
        let c = render_for(Harness::Claude, std::slice::from_ref(&booted));
        let p = render_for(Harness::Pi, std::slice::from_ref(&booted));
        assert!(c.contains("claude · m-1 · s-1"), "{c:?}");
        assert!(p.contains("pi · m-1 · s-1"), "{p:?}");
        assert!(!p.contains("claude"), "a pi pane must not name claude: {p:?}");

        // And the constructor that has no harness to name: it names NONE rather
        // than guessing one, because the PTY adapter has no `Harness` to give.
        let plain = render_all(200, std::slice::from_ref(&booted));
        assert!(plain.contains("m-1 · s-1"), "{plain:?}");
        assert!(!plain.contains("claude"), "{plain:?}");
        assert!(!plain.contains("pi ·"), "{plain:?}");
    }

    #[test]
    fn reasoning_and_tool_bytes_stay_out_of_the_ring_and_the_other_three_do_not() {
        // This is the RING — behind `get_output`, replay and thumbnails, and
        // (§5.1) the only human surface a structured pane has. `Thinking` and
        // `ToolOutput` are deliberately silent in it; see the render arms for the
        // grounds, and for the fact that their bulk therefore reaches the event
        // log and machine consumers but no human surface today.
        //
        // The pair matters — an assertion that only checked the two silent
        // variants would pass just as well on a renderer that drew nothing at
        // all, which is the vacuity control here.
        let silent = render_all(
            200,
            &[
                HarnessEvent::Thinking {
                    turn: TurnId(0),
                    delta: "the user is asking about".into(),
                },
                HarnessEvent::ToolOutput {
                    turn: TurnId(0),
                    id: ToolUseId("t".into()),
                    delta: "total 48".into(),
                    is_error: false,
                    replaces: false,
                },
                HarnessEvent::QueueChanged {
                    steering: vec![],
                    follow_up: vec![],
                },
            ],
        );
        assert_eq!(
            silent, "",
            "reasoning, tool bytes and an emptied queue draw nothing in the ring"
        );

        // The positive control: the renderer IS running, and the three variants
        // that must draw do.
        let drawn = render_all(
            200,
            &[
                HarnessEvent::UiRequest {
                    id: RequestId("u1".into()),
                    method: UiMethod::Select,
                    title: Some("Allow dangerous command?".into()),
                    message: None,
                    options: vec!["Allow".into(), "Block".into()],
                    timeout_ms: Some(10_000),
                },
                HarnessEvent::UiSettled {
                    id: RequestId("u1".into()),
                    answer: UiAnswer::Value("Allow".into()),
                    by: DecisionSource::Human,
                },
                HarnessEvent::QueueChanged {
                    steering: vec!["focus on errors".into()],
                    follow_up: vec!["then summarize".into(), "and stop".into()],
                },
            ],
        );
        assert!(
            drawn.contains("[dialog select] Allow dangerous command? [Allow, Block]"),
            "{drawn:?}"
        );
        assert!(drawn.contains("answered \"Allow\" by the human"), "{drawn:?}");
        assert!(drawn.contains("queued: 1 steering, 2 follow-up"), "{drawn:?}");
        // COUNTS, not the text: another pane's prompts are unbounded, and the
        // ring is a fixed-size buffer.
        assert!(
            !drawn.contains("focus on errors"),
            "the queued TEXT must not reach the ring: {drawn:?}"
        );

        // And the one line that is waiting for a human is NOT dimmed, for
        // `PermissionRequest`'s reason.
        let dialog_line = drawn
            .lines()
            .find(|l| l.contains("[dialog select]"))
            .expect("the dialog line is there");
        assert!(
            !dialog_line.contains(DIM),
            "the line the pane is blocked on must not be the dimmest on screen: \
             {dialog_line:?}"
        );
    }

    #[test]
    fn no_additive_variant_can_rewrite_the_screen_either() {
        // `no_output_can_rewrite_the_screen` above is scoped to the variants that
        // existed when it was written, and #2850 added five — including two whose
        // content is a harness-supplied string (a dialog title, a queued prompt)
        // that can carry `CR` and `ESC` perfectly well. This runs the same ban
        // over the new population, with the attacker-controlled bytes in it.
        let out = render_all(
            40,
            &[
                HarnessEvent::Thinking {
                    turn: TurnId(0),
                    delta: "\r\x1b[2Kwiped".into(),
                },
                HarnessEvent::ToolOutput {
                    turn: TurnId(0),
                    id: ToolUseId("t".into()),
                    delta: "\r\x1b[2Kwiped".into(),
                    is_error: true,
                    replaces: true,
                },
                HarnessEvent::UiRequest {
                    id: RequestId("u".into()),
                    method: UiMethod::Confirm,
                    title: Some("\r\x1b[2Kwiped".into()),
                    message: None,
                    options: vec!["\x1b[31mred".into()],
                    timeout_ms: None,
                },
                HarnessEvent::UiSettled {
                    id: RequestId("u".into()),
                    answer: UiAnswer::Value("\r\x1b[2Kwiped".into()),
                    by: DecisionSource::Policy,
                },
                HarnessEvent::QueueChanged {
                    steering: vec!["\r\x1b[2K".into()],
                    follow_up: vec![],
                },
            ],
        );
        let bytes = out.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\r' {
                assert_eq!(
                    bytes.get(i + 1),
                    Some(&b'\n'),
                    "a bare CR returns the cursor without advancing: {out:?}"
                );
            }
        }
        // The ESC ban is on ESCs this module did not author. Every sequence it
        // does author is `DIM`/`RESET`, so stripping those must leave none.
        let stripped = out.replace(DIM, "").replace(RESET, "");
        assert!(
            !stripped.contains('\x1b'),
            "a harness-supplied ESC reached the terminal: {stripped:?}"
        );
    }

    // ── the two projections, kept honest against one record (#2891 S4) ──────
    //
    // `doc/design/harness-adapters.md` §5.1 makes the event log the record and
    // gives it two projections: this one, into VT bytes for the pane's
    // `OutputBuf` ring, and the DOM renderer the human actually reads
    // (`src/structuredview.ts` -> `src/structuredpane.ts`). It states the risk
    // that shape creates in one line — "two projections of one log can
    // disagree" — and assigns the control to the slice that builds the DOM
    // renderer. These two tests are the Rust half of it; the TypeScript half is
    // `test/structuredrows.test.ts`, and both read the same two files.

    /// The fixture the frontend's tests and its replay page run on, and this
    /// crate's own path to it. It lives under `test/` because that is where the
    /// frontend can reach it; the point of reading it from HERE is that the
    /// contract it claims to encode is defined in THIS crate.
    const FRONTEND_FIXTURE: &str =
        include_str!("../../../../test/fixtures/structuredview/session.harness.jsonl");
    const PARITY_RECORD: &str =
        include_str!("../../../../test/fixtures/structuredview/parity.json");

    /// Split on `\n` only and strip an optional trailing `\r` — pi's own framing
    /// rule, and what makes this reader correct on a CRLF checkout, which this
    /// file is (`git ls-files --eol` says `w/crlf`).
    fn fixture_lines() -> Vec<&'static str> {
        FRONTEND_FIXTURE
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .filter(|l| !l.is_empty())
            .collect()
    }

    /// The one line that is deliberately NOT a `HarnessEvent`: orrerix's own
    /// delivery, which rides the same batch tagged distinctly so that a harness
    /// cannot forge one (`structuredview.ts`'s `LocalEvent`).
    fn is_local(line: &str) -> bool {
        line.contains("\"kind\":\"delivery\"")
    }

    #[test]
    fn the_frontend_fixture_is_byte_for_byte_what_serde_emits() {
        // WHAT THIS CLOSES, and it is a CLASS rather than three instances.
        // `test/fixtures/structuredview/session.harness.jsonl` calls itself
        // "HarnessEvent as it serializes" and the frontend reads it through an
        // unchecked `as` cast, so nothing on either side had ever checked the
        // claim. Three lines were wrong when this test was written (#2891 S4):
        // a `ui_settled` answer spelled `{"Value":…}` where `UiAnswer` carries
        // `rename_all = "snake_case"` and the wire is `{"value":…}`, the
        // frontend type that agreed with it, and a `compacted` trigger of
        // `"threshold"`, which is not a `CompactTrigger` at all. Every reader
        // of a settled dialog matched the capitalised keys, so a REAL
        // settlement would have fallen through to "cancelled" on every dialog —
        // with a green suite on both sides, because the fixture carried the
        // same error.
        //
        // ROUND-TRIP RATHER THAN A HAND-BUILT SEQUENCE. The ask was for a test
        // that serialises a representative sequence and compares it byte for
        // byte. This does the same work over EVERY line instead of over the
        // ones somebody chose, and it cannot drift from the fixture the way a
        // second hand-written copy of it would: deserialize (which refuses an
        // unknown variant spelling outright) and re-serialize (which pins the
        // key spelling and the field ORDER, since serde emits declaration
        // order). A serde attribute changed in this crate reddens here, and
        // here is the file the frontend runs on.
        //
        // ONE CONSEQUENCE WORTH NAMING, because it caught a fourth wrong line.
        // `serde_json` is built here without `preserve_order`, so a `Value`
        // object is a `BTreeMap` and its keys come back SORTED. A tool call`s
        // `input` is a `Value`, so the bytes the engine really emits for
        // `Grep{pattern, path}` are `{"path":…,"pattern":…}` — which is not the
        // order the fixture was written in, and not something any amount of
        // reading the type would have told you.
        let lines = fixture_lines();
        assert_eq!(
            lines.len(),
            28,
            "the fixture is 28 lines; a silent truncation would pass every assertion below"
        );

        let mut locals = 0;
        let mut checked = 0;
        for (i, line) in lines.iter().enumerate() {
            if is_local(line) {
                locals += 1;
                continue;
            }
            let ev: HarnessEvent = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("fixture line {} is not a HarnessEvent: {e}\n{line}", i + 1));
            let round = serde_json::to_string(&ev)
                .expect("a HarnessEvent this crate just parsed must serialize");
            assert_eq!(
                round,
                *line,
                "fixture line {} is not what serde emits for the event it decodes to",
                i + 1
            );
            checked += 1;
        }
        assert_eq!(locals, 1, "exactly one line is a local (non-HarnessEvent) event");
        assert_eq!(checked, 27, "and the other 27 were really round-tripped");
    }

    #[test]
    fn the_two_projections_diverge_only_where_the_record_says_they_do() {
        // The parity control §5.1 asks for. NOT an equality: this projection
        // deliberately draws nothing for several kinds — the arms above carry
        // the argument for each — so a "both draw the same set" assertion would
        // have to be weakened until it caught nothing. The record is per-kind,
        // every divergence carries its reason, and both sides assert against
        // it, so a NEW divergence cannot appear silently and an existing one
        // cannot be closed without saying so in the record.
        //
        // ONE RENDERER, IN FIXTURE ORDER, because that is how this projection
        // is really driven — and because it is stateful in a way that decides
        // this very question. `Renderer::newline` is a no-op at column 0, so
        // `TurnStarted` (whose whole output is two of them) emits bytes only
        // when the previous event left the cursor mid-line. A fresh renderer
        // per event would answer "does this kind draw" with a fact about the
        // instrument rather than about the projection. The DOM half runs one
        // `State` over the same lines in the same order, which is what makes
        // the two columns comparable at all.
        //
        // The whole table is built BEFORE anything is asserted, so one run
        // reports every wrong row instead of stopping at the first.
        let record: serde_json::Value =
            serde_json::from_str(PARITY_RECORD).expect("parity.json must be valid JSON");
        let kinds = record["kinds"]
            .as_object()
            .expect("parity.json must carry a `kinds` object");

        let mut renderer = Renderer::new(80);
        // (kind, drew) in first-seen order. A kind that appears twice must
        // agree with itself, which is asserted rather than last-write-wins.
        let mut actual: Vec<(String, bool)> = Vec::new();
        let mut drew_something = false;
        for (i, line) in fixture_lines().iter().enumerate() {
            if is_local(line) {
                continue;
            }
            let tag = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v["kind"].as_str().map(str::to_owned))
                .unwrap_or_else(|| panic!("fixture line {} has no `kind`", i + 1));
            let ev: HarnessEvent = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("fixture line {} is not a HarnessEvent: {e}", i + 1));

            let drew = !renderer.render(&ev).is_empty();
            if drew {
                drew_something = true;
            }
            match actual.iter_mut().find(|(k, _)| *k == tag) {
                // `drew` may legitimately differ between two events of one kind
                // (a `TurnStarted` at column 0 draws nothing, one mid-line
                // does), so the record's claim is "this kind draws AT LEAST
                // ONCE over this log" — the honest reading of a conditional
                // separator, and it is stated in parity.json's own header.
                Some((_, seen)) => *seen |= drew,
                None => actual.push((tag, drew)),
            }
        }

        let mut wrong: Vec<String> = Vec::new();
        for (tag, drew) in &actual {
            match kinds.get(tag).and_then(|r| r["vt"].as_bool()) {
                None => wrong.push(format!("  {tag}: parity.json has no boolean `vt` row for it")),
                Some(expected) if expected != *drew => wrong.push(format!(
                    "  {tag}: the VT projection {}, parity.json says it {}",
                    if *drew { "DRAWS" } else { "draws nothing" },
                    if expected { "does" } else { "does not" }
                )),
                Some(_) => {}
            }
        }
        assert!(
            wrong.is_empty(),
            "the two projections no longer diverge where the record says they do:\n{}\n\
             \nThe record is `test/fixtures/structuredview/parity.json`. A row that moved is a \
             DESIGN change (this projection started or stopped drawing a kind), not a test fix — \
             update the record and say why in its `why`.",
            wrong.join("\n")
        );

        // Every divergence carries an argument. A divergence is allowed; an
        // UNARGUED one is the drift §5.1 warns about.
        for (tag, _) in &actual {
            let row = &kinds[tag];
            if row["vt"].as_bool() != row["dom"].as_bool() {
                let why = row["why"].as_str().unwrap_or("");
                assert!(
                    why.len() > 40,
                    "`{tag}` diverges between the two projections and parity.json gives no reason \
                     worth the name"
                );
            }
        }

        // The controls. An absence-only run over an empty fixture, or one whose
        // every row happened to be `vt: false`, would pass everything above.
        assert!(
            drew_something,
            "positive control: no event drew anything, so the renderer never ran"
        );
        assert_eq!(
            actual.len(),
            16,
            "the fixture covers 16 of the 17 HarnessEvent kinds (all but `observed`, which is \
             PTY-only) — it now covers {}, so the scan above is narrower than it reads",
            actual.len()
        );
        assert_eq!(
            kinds.len(),
            actual.len(),
            "parity.json describes kinds the fixture does not exercise, so those rows are \
             asserted by nothing"
        );
    }
}
