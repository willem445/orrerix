//! The question hold: whether an agent's pane is showing a question, how that
//! is witnessed and re-read, and when a write is admitted anyway.
//! Design note: `docs/design/human-questions.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): `PtyManager`,
//! `crate::pty`. IO: threads/sleep. Sibling files it calls: `holds.rs`,
//! `humaninput.rs`, `noticemask.rs`, `panetail.rs`, `screen.rs`, `tuning.rs`.

use super::*;

// Interactive-question paste guard (#420): Copilot (and other CLIs) surface
// numbered/radio-select questions and y/n permission prompts as an interactive
// TUI, not as text sitting in the input box — so the box-occupied guard
// (`humaninput.rs`) doesn't see
// them. A programmatic paste+Enter landing there is worse than the box-occupied
// case: Enter doesn't merge text, it SELECTS the highlighted option (usually
// the first), silently steering the agent's turn in an answer nobody chose. So
// before pasting AND before the first Enter, hold delivery while
// `prompt_wait_detected` reads a live question off the pane's output tail —
// same detector attention routing already uses (#6/#40) — until it clears
// (the human answers) or the bound elapses, in which case abort rather than
// blind-select.
/// Bounded wait for the question to clear before aborting the delivery.
/// Longer than [`HUMAN_INPUT_HOLD_MAX`]: reading and deciding a substantive
/// question takes more of a human's attention than submitting an already-typed
/// line.
const QUESTION_HOLD_MAX: Duration = Duration::from_secs(120);
/// Poll interval while holding for the question to clear.
const QUESTION_HOLD_POLL: Duration = Duration::from_millis(250);
/// Non-empty rendered rows a composition must have before it is allowed to
/// say a question is gone (#534). Two, not a coherence proof: a replay that
/// began mid-stream and was never painted over composes to near-nothing, and
/// "the screen is blank" must not be mistaken for "the screen is clear". Any
/// CLI at rest paints more than this (an input box alone is three rows), so
/// the floor only ever catches the degenerate case it names.
const GRID_MIN_RENDERED_ROWS: usize = 2;
/// How many CONSECUTIVE polls `prompt_wait_detected` must read false before
/// the interactive-question guard releases (rev-19 R1): release is state-
/// based (is the menu still on screen?), not activity-based (did a keystroke
/// arrive?) — a single false read could be a transient mid-redraw miss, so
/// two in a row is the bar. Only applies once a hold has genuinely started
/// (`question_hold_predicate`'s `ever_shown` gate) — a checkpoint that was
/// never shown a question releases on its very first check, no extra delay.
const QUESTION_RELEASE_CONSECUTIVE_CLEAR_POLLS: u32 = 2;

/// The live interactive-question guard's hold decision (#420), generic over
/// the tail read so it's integration-tested with a scripted closure — no
/// `PtyManager`, no real PTY (rev-15 B4: the old production-bound version of
/// this predicate could never be exercised by a test that disabled it, since
/// nothing but a live pty could drive it; this generic form is what a test
/// drives directly, and the production wrapper below just supplies a real
/// closure over `ptys`).
///
/// - `tail()` — raw (ANSI-included) bytes of the pane's current output tail.
/// - `pasted_text` — `None` for a checkpoint that runs BEFORE this delivery
///   has written anything (nothing of ours is on screen yet to mask);
///   `Some(the exact text this delivery pasted)` for a checkpoint that runs
///   after it (pre-Enter, each retry) — `mask_own_paste` (`noticemask.rs`) strips our
///   own lines out of the tail before the detector ever sees it (rev-15 N1 /
///   rev-19 B-A).
///
/// **Release is STATE-based, not ACTIVITY-based (rev-19 R1).** An earlier cut
/// released the hold on a human keystroke (a submitted, non-sitting one). That
/// was wrong on both ends: not sufficient (an arrow key *navigating the still-
/// open menu* — `HumanInput::Neutral`, no text left sitting — satisfied the old
/// condition and let the freed Enter fire straight into the still-open dialog)
/// and not necessary (a question can clear with no local keystroke at all —
/// the CLI times its own prompt out, or answers itself from a recorded
/// consent). Worse, xterm's own automatic terminal-query replies (#179) can
/// stamp a pane's keystroke-recency clock with no human present at all. So
/// human activity is dropped from this decision ENTIRELY: the only thing that
/// gets to say the question is gone is the SAME detector that said it was
/// there — `prompt_wait_detected` reading false. A single false read is not
/// enough on its own (a redraw mid-flicker could transiently miss the menu),
/// so release requires it read false on two CONSECUTIVE polls; the very first
/// poll of a hold that was never actually shown a question releases
/// immediately (`ever_shown` gates the two-poll requirement to only kick in
/// once a real hold has genuinely started — see `wait_for_question_clear`'s
/// own fast-path for why a never-active hold must not pay any extra latency).
///
/// #496 PR-A made `PtyManager::note_user_input` gate the keystroke-recency
/// stamp itself on `classify_human_input`, so an xterm auto-reply no longer
/// refreshes it either — the clock this comment distrusts is materially more
/// trustworthy than it was when rev-19 R1 was written. That does NOT change
/// this decision: release still needs to be STATE-based, because the "not
/// sufficient" half of the finding above — an arrow key navigating a
/// still-open menu reads `HumanInput::Neutral` too, and #496 does not stamp
/// pure-Neutral input either — is untouched by tightening what the clock
/// tracks. This predicate stays exactly as-is.
///
/// #534 kept all of the above and added a second reading. The logic now lives
/// in [`question_hold_predicate_sampled`]; THIS function is the ring-only
/// entry point — the pre-#534 guard exactly, and the fallback whenever a
/// pane's screen cannot be composed. Everything the paragraphs above say about
/// what may and may not release a hold applies unchanged to both, because both
/// are the same closure: the grid narrows *when* the detector reads clear, it
/// does not touch the two-consecutive-poll rule that decides what a clear read
/// is worth. See [`question_shown`] for the one behaviour that differs.
pub fn question_hold_predicate<T>(
    tail: T,
    pasted_text: Option<String>,
    delivered: Vec<String>,
) -> impl Fn() -> bool
where
    T: Fn() -> Option<Vec<u8>>,
{
    // Ring only, no composition — exactly the reading this guard had before
    // #534, and the one it falls back to whenever a pane's screen cannot be
    // trusted. `visible: None` is not "nothing is rendered"; it is "we have no
    // rendered-rows evidence", which `question_shown` treats as the ring's
    // word being final.
    question_hold_predicate_sampled(
        move || QuestionSample { ring: tail(), visible: None },
        pasted_text,
        None,
        delivered,
    )
}

/// One poll's worth of readings for the question guard (#534): the same
/// instant seen two ways.
pub struct QuestionSample {
    /// Raw (ANSI-included) bytes from the pane's append-only output ring —
    /// what the guard has always read.
    pub ring: Option<Vec<u8>>,
    /// The pane's currently-rendered rows, composed by [`question_visible`]
    /// ([`termgrid::render_visible`]'s rows, less the input box's faint
    /// placeholder, #3426). `None` means *no trustworthy
    /// composition*, never *a blank screen* — the two must not collapse,
    /// because one licenses a release and the other must not.
    pub visible: Option<String>,
}

/// What the composed screen says about a match the byte ring made (#534).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridEvidence {
    /// The match — or some other question shape — is among the rendered rows.
    StillRendered,
    /// The screen composed cleanly and holds neither. Releases the hold.
    NotRendered,
    /// The screen composed cleanly and shows the CLI sitting at an EMPTY input
    /// prompt with no menu selection anywhere on it (#903) — whatever the ring
    /// matched, and whatever of it is still rendered, is text on an idle pane
    /// rather than a question anybody can answer. Releases the hold.
    ///
    /// Its own variant rather than folding into `NotRendered` because the two
    /// are different findings and the audit is read by humans: `not-rendered`
    /// says the dialog went away, `idle-prompt` says there was never a dialog —
    /// which is the whole of #903 and the single most useful line in the log
    /// when the detector fires on prose again.
    IdlePrompt,
    /// No composition worth reading (pty gone, geometry unknown, nothing
    /// painted). Proves nothing in either direction; the ring's word stands.
    Unreadable,
}

impl GridEvidence {
    /// Audit vocabulary — stable, kebab-case, read by humans diagnosing the
    /// next #513-shaped incident.
    pub fn as_str(&self) -> &'static str {
        match self {
            GridEvidence::StillRendered => "still-rendered",
            GridEvidence::NotRendered => "not-rendered",
            GridEvidence::IdlePrompt => "idle-prompt",
            GridEvidence::Unreadable => "unreadable",
        }
    }
}

/// Read the composed screen for evidence about `m`.
///
/// Two independent ways to answer "still displayed", OR'd, because a false
/// `NotRendered` is the expensive error (it releases an Enter toward a dialog
/// that may be live) and a false `StillRendered` is the cheap one (a hold the
/// human is already badged about at ten minutes):
///
/// - [`prompt_wait_detected`] over the rendered rows — catches a dialog the
///   CLI repainted with different text than the ring matched.
/// - [`match_still_rendered`] — catches a dialog sitting outside the
///   detector's own last-12-lines window, which is a *chronological* rule
///   applied here to a *spatial* layout and so cannot be relied on alone.
///
/// **#903 puts one reading ahead of both of them**, and the ordering is the
/// change: [`idle_prompt_rendered`] is consulted FIRST, so a pane sitting at an
/// empty composer answers `IdlePrompt` even when the matched text is plainly
/// still on the screen. That inverts the asymmetry above for exactly one case,
/// deliberately. The asymmetry is right when the question is "did the dialog go
/// away" — absence of evidence is weak, so weight it toward holding. It is
/// wrong when the question is "was this ever a dialog", because there the
/// screen is offering *positive* evidence that it was not: a CLI showing an
/// empty free-text box is not blocked on an answer. #903's two lost panes had a
/// matched line, still rendered, forever, on a pane nobody was being asked
/// anything by — `StillRendered` was true and useless.
pub fn grid_evidence_for(m: &QuestionMatch, visible: Option<Composed<'_>>) -> GridEvidence {
    match visible {
        None => GridEvidence::Unreadable,
        Some(c) if idle_prompt_rendered(c) => GridEvidence::IdlePrompt,
        // Both of these read the MASKED view, unchanged: "is a question
        // displayed" was never allowed to be answered by loomux's own rows.
        // `Composed` widens what the guard can see, not what counts as a
        // question.
        Some(c) if prompt_wait_detected(c.masked) || match_still_rendered(c.masked, m) => {
            GridEvidence::StillRendered
        }
        Some(_) => GridEvidence::NotRendered,
    }
}

/// The question guard's reading for one poll, from both signals (#534).
///
/// **The ring is the trigger; the grid can only ever release.** Written out,
/// because the asymmetry is the safety argument and not an implementation
/// detail:
///
/// | ring | grid | reading | |
/// |---|---|---|---|
/// | no match | (not consulted) | clear | |
/// | match | `Unreadable` | hold | |
/// | match | `StillRendered` | hold | |
/// | match | `NotRendered` | **clear** | #534's one change |
/// | match | `IdlePrompt` | **clear** | #903's one change |
///
/// Every row but the last is today's behaviour, so the entire behavioural
/// surface of this change is a single transition, in a single direction, and
/// the design note argues exactly that one. Notably the grid is never allowed
/// to *create* a hold the ring did not: that would be a new false-positive
/// class (screen content the ring had already scrolled past), and #420/#427
/// forbid weakening the guard but nothing asks us to strengthen it here.
///
/// A release still is not a write. This reading feeds
/// `question_hold_predicate`, which requires
/// [`QUESTION_RELEASE_CONSECUTIVE_CLEAR_POLLS`] consecutive clear reads; the
/// write that follows is admitted by `write_admission`, which re-reads box
/// occupancy at the instant of the write (#532). "The question is gone AND the
/// box is empty" is therefore a conjunction the caller already enforces — see
/// the design note for why it is not re-derived here.
pub fn question_shown(ring: Option<&QuestionMatch>, visible: Option<Composed<'_>>) -> bool {
    match ring {
        None => false,
        // Spelled as the two readings that HOLD rather than as `!= NotRendered`
        // (#903): a sixth `GridEvidence` added later must decide which side it
        // is on at the compiler's insistence, instead of inheriting "hold" — or,
        // worse, "release" — from whichever way the comparison happened to be
        // written.
        Some(m) => matches!(
            grid_evidence_for(m, visible),
            GridEvidence::StillRendered | GridEvidence::Unreadable
        ),
    }
}

/// Is a composed screen worth reading as evidence, or is it a replay that
/// began blind and was never painted over (#534)?
///
/// A composition with essentially nothing on it is not a clear screen; it is
/// an absent one, and the distinction is the whole difference between "the
/// dialog is gone" and "we did not see the dialog". Returning `None` costs a
/// hold that was already going to happen; the alternative is treating a blank
/// grid as proof.
///
/// It is not a coherence check and does not pretend to be one — a garbled but
/// well-populated composition passes here. See the design note's limits
/// section for what that leaves open.
pub fn trustworthy_composition(visible: String) -> Option<String> {
    (visible.lines().filter(|l| !l.trim().is_empty()).count() >= GRID_MIN_RENDERED_ROWS)
        .then_some(visible)
}

/// What the guard held for, recorded for the abort audit (#513(c)/F2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionWitnessed {
    /// The detector's match — signal class and the line it fired on.
    pub matched: QuestionMatch,
    /// What the composed screen said about it on that same poll.
    pub grid: GridEvidence,
    /// Did that same screen show the CLI's own input prompt — empty, or holding
    /// nothing but loomux's paste ([`idle_prompt_row_rendered`], the WEAK
    /// reading)? #903.
    ///
    /// It rides the witness rather than a fifth out-parameter for two reasons.
    /// It is genuinely diagnostic — "the guard held while a composer was on
    /// screen" is the #903 finding in one bit, and it is emitted by
    /// [`witness_audit`] so the next narrowing has it. And it is only ever
    /// *needed* when the ring matched, which is exactly when a witness exists:
    /// the last-resort override is defined over a pane whose question gate is
    /// holding, and a gate cannot hold without a match.
    pub idle_row: bool,
}

/// Slot the hold predicate writes its last observed match into, so the abort
/// site can say WHY it held. Single-threaded by construction (the predicate is
/// a closure polled by one delivery thread), hence `Rc`/`RefCell` rather than
/// a lock.
pub type QuestionWitness = std::rc::Rc<std::cell::RefCell<Option<QuestionWitnessed>>>;

/// The `matched` field every question-guard audit record now carries
/// (#513(c)/F2) — or `null` where the guard genuinely never saw a question.
///
/// `null` is a real answer and is why this returns a value rather than
/// omitting the key: for a `delivery-aborted-question` record it means the
/// abort outcome and the detector disagree, which is itself the finding. The
/// old records were indistinguishable from that case in every direction, which
/// is why #513's live 27-minute incident is still unexplained.
///
/// Four fields, each earning its place in a log that rotates at 8 MiB:
/// `signal` says which detector rule fired (the fastest way to spot a rule
/// misfiring on prose), `line` says what it fired on (bounded by
/// [`QuestionMatch::MAX_LINE`]), `grid` says whether the composed screen
/// agreed — the one that turns "the guard held" into "the guard held and the
/// screen backed it up", or into the opposite — and `idle_row` (#903) says
/// whether the CLI's own composer was on that screen, which is the term the
/// last-resort override is decided on and therefore the one a human has to be
/// able to audit after it fires.
pub fn witness_audit(seen: Option<&QuestionWitnessed>) -> Value {
    match seen {
        None => Value::Null,
        Some(w) => json!({
            "signal": w.matched.signal,
            "line": w.matched.line,
            "grid": w.grid.as_str(),
            "idle_row": w.idle_row,
        }),
    }
}

/// [`question_hold_predicate`], generalized over a sample that may carry
/// composed-screen evidence, and able to record what it saw (#534).
pub fn question_hold_predicate_sampled<T>(
    sample: T,
    pasted_text: Option<String>,
    witness: Option<QuestionWitness>,
    // #576: what loomux knows it wrote to this pane
    // (`OrchRegistry::delivered_mask_lines` — the per-pane notice record plus
    // the per-session prompt record, #576/#903). A REQUIRED argument rather than
    // a defaulted one: every production caller has it, and a call site that
    // silently got an empty record would be a gate quietly running on the
    // pre-#576 rule with nothing to say so (the #544 "never acquired by
    // omission" shape). Empty is a legitimate value — a pane loomux has written
    // nothing into, or one whose record a restart dropped — and it means
    // exactly the marker rule.
    delivered: Vec<String>,
) -> impl Fn() -> bool
where
    T: Fn() -> QuestionSample,
{
    let ever_shown = std::cell::Cell::new(false);
    let consecutive_clear = std::cell::Cell::new(0u32);
    move || {
        let s = sample();
        // `mask_own_paste` runs on BOTH readings or the guard would be
        // inconsistent with itself: our own just-pasted text sits in the box
        // and is therefore *rendered*, so an unmasked grid read would answer
        // "still displayed" about our own paste (rev-15 N1 / rev-19 B-A).
        // Two masks, both on BOTH readings. `mask_own_paste` needs this
        // delivery's text and so is `None` at every checkpoint that runs before
        // one (`question_active_now`'s call sites — four since #819 added
        // `stranded_marker_action`'s); the notice mask needs
        // no `pasted_text` at all, which is exactly why it closes #576 — the
        // outer drainer gate has no `pasted_text` to mask with, yet the pane is
        // full of the PREVIOUS delivery's notices. Since #576's residual it
        // reads `delivered` as well, so a notice that WRAPPED is masked over
        // every row it wrapped onto rather than only its first.
        let mask = |t: &str| {
            let t = mask_loomux_notices_with_record(t, &delivered);
            match &pasted_text {
                Some(p) => mask_own_paste(&t, p),
                None => t,
            }
        };
        let ring_match =
            s.ring.as_deref().and_then(|out| prompt_wait_match(&mask(&strip_ansi(out))));
        // #903 rev-427 B1: BOTH views are kept, and the fully-masked one is still
        // the only thing "is a question displayed" is asked of. The other answers
        // one narrow question the masked one structurally cannot — is the CLI's
        // composer on screen — because `mask_own_paste` deletes the very row that
        // proves it.
        //
        // rev-433: the pair differs by the PASTE mask ALONE. Both have had the
        // notice mask applied first, so the authorship diff cannot mistake one of
        // loomux's own `[orrerix] …` notice rows near the bottom of a transcript
        // for the composer. See [`Composed`].
        let with_paste =
            s.visible.as_deref().map(|v| mask_loomux_notices_with_record(v, &delivered));
        let masked = with_paste.as_deref().map(|v| match &pasted_text {
            Some(p) => mask_own_paste(v, p),
            None => v.to_string(),
        });
        let composed = match (with_paste.as_deref(), masked.as_deref()) {
            (Some(with_paste), Some(masked)) => Some(Composed { masked, with_paste }),
            _ => None,
        };
        let shown = question_shown(ring_match.as_ref(), composed);
        if let (Some(w), Some(m)) = (&witness, ring_match) {
            // Recorded on every poll the ring matched, INCLUDING the polls
            // that read clear on the grid — the abort audit wants the last
            // thing seen, and "the ring kept matching but the screen said
            // gone" is the single most useful line a #513 diagnosis could
            // have. Never cleared on a no-match poll: an abort is preceded by
            // whatever the hold was about, and blanking it would leave the
            // audit saying nothing again.
            let grid = grid_evidence_for(&m, composed);
            // #903: the override's own term, recorded on the SAME poll as the
            // decision it will be used for — never re-read at the override site,
            // for the reason this whole witness exists.
            let idle_row = composed.is_some_and(idle_prompt_row_rendered);
            *w.borrow_mut() = Some(QuestionWitnessed { matched: m, grid, idle_row });
        }
        if shown {
            ever_shown.set(true);
            consecutive_clear.set(0);
            return true;
        }
        if !ever_shown.get() {
            return false; // never actually holding — no artificial delay
        }
        let n = consecutive_clear.get() + 1;
        consecutive_clear.set(n);
        n < QUESTION_RELEASE_CONSECUTIVE_CLEAR_POLLS
    }
}

/// Production wrapper: hold prompt delivery to `pty_id` while a live
/// interactive question is on screen (#420), reusing `hold_for_human_input`'s
/// generic block-until-clear-or-capped loop exactly like `wait_for_box_clear`
/// does, just with `question_hold_predicate` (bound to this pane, via
/// `output_tail_bounded` — rev-15 N4) as the "still occupied" predicate.
/// `pasted_text` is threaded straight through — see `question_hold_predicate`'s
/// doc for what `None` vs `Some` means at each call site. Owns the
/// delivery-held badge around the wait (the SAME shape every other hold in
/// this function uses: pre-check with zero elapsed hold, so the badge only
/// fires for a hold that actually happens) so every caller — the pre-paste
/// checkpoint, the pre-Enter checkpoint, and each spaced retry (rev-15 B2) —
/// gets identical badge/hold behavior from one place instead of re-deriving
/// it. A closed pty or unreadable tail reads as "no question" so a dead/gone
/// pane never blocks the thread.
///
/// Since #534 it reads both signals per poll (see [`question_sample`]) and
/// returns, alongside the decision, the LAST thing the detector matched —
/// `None` when the guard never saw a question at all. Callers that abort put
/// it in the audit; that field is the whole of #513(c), and the reason it is
/// returned rather than re-derived at the abort site is that a fresh read
/// there would describe a different instant than the one that decided.
pub(in crate::orchestration) fn wait_for_question_clear(
    ptys: &crate::pty::PtyManager,
    pty_id: u32,
    pasted_text: Option<&str>,
    delivered: Vec<String>,
    emit_held: &impl Fn(HeldReason),
    emit_held_cleared: &impl Fn(),
) -> (PasteDecision, Option<QuestionWitnessed>) {
    let witness: QuestionWitness = Default::default();
    let predicate = question_hold_predicate_sampled(
        || question_sample(ptys, pty_id),
        pasted_text.map(str::to_string),
        Some(std::rc::Rc::clone(&witness)),
        delivered,
    );
    let will_hold = predicate();
    if will_hold {
        emit_held(HeldReason::InteractiveQuestion);
    }
    let decision = hold_for_human_input(&predicate, QUESTION_HOLD_MAX, QUESTION_HOLD_POLL);
    if will_hold {
        emit_held_cleared();
    }
    let seen = witness.borrow().clone();
    (decision, seen)
}

/// Both of the question guard's readings for one pane, taken from ONE tail
/// read (#534).
///
/// One read, not two, and the reason is correctness rather than cost: a second
/// `output_tail_bounded` call would sample a *different instant*, so the ring
/// could match text the composition — taken microseconds later, after the CLI
/// erased it — legitimately no longer holds. The guard would then be comparing
/// two screens and calling the difference evidence. Slicing the ring window out
/// of the tail the grid replays keeps both readings answering about the same
/// bytes.
///
/// The ring slice is the LAST [`QUESTION_SCAN_TAIL_BYTES`], preserving the
/// pre-#534 detector window exactly — including its habit of starting
/// mid-codepoint, which `strip_ansi` has always absorbed.
fn question_sample(ptys: &crate::pty::PtyManager, pty_id: u32) -> QuestionSample {
    let raw = ptys.output_tail_bounded(pty_id, QUESTION_GRID_REPLAY_BYTES);
    // Geometry is required, never defaulted. `get_output` may fall back to
    // 80x24 because a wrong width there only re-wraps prose in something a
    // human reads; here a wrong width changes which cells hold which
    // characters, and a wrapped `(y/n)` that fails to match would read as
    // "not displayed" — the one direction this must never be wrong in. No
    // size, no grid evidence.
    let visible = match (raw.as_deref(), ptys.size(pty_id)) {
        (Some(bytes), Some((cols, rows))) => question_visible(bytes, cols, rows),
        _ => None,
    };
    let ring = raw.map(|b| b[b.len().saturating_sub(QUESTION_SCAN_TAIL_BYTES)..].to_vec());
    QuestionSample { ring, visible }
}

/// The composed screen the question guard reads (#534), with the input box's
/// PLACEHOLDER removed (#3426).
///
/// Split out of [`question_sample`] so the one decision it adds is drivable from
/// raw bytes by a test — `question_sample` itself needs a live `PtyManager`.
///
/// **Why the placeholder has to go.** After a turn, Claude Code writes a guess
/// at the human's next prompt into its empty input box as placeholder text
/// (`❯ main is green now — rebase onto origin/main and re-run CI`). As TEXT
/// that row is a prompt glyph leading content, which is two things this guard
/// reads as NOT idle: a `pointer-option` row (the ring's trigger, and
/// [`pointer_rendered`]'s veto on the grid), and a composer that is not empty,
/// so [`idle_prompt_row_rendered`] is false. The second is what wedged #3426: it
/// blocks #903's idle-composer release AND starves
/// [`question_override_admits`] of the idle reads it counts, so a hold on
/// prose that the idle release exists for held for thirty minutes and the
/// fifteen-minute override never fired.
///
/// **What tells it apart, and why nothing else would.** Text cannot: a
/// suggestion and a line the human typed are the same characters in the same
/// place. The attribute can. Claude Code paints its placeholder with chalk
/// `dim` — SGR 2, faint — with at most the first character in inverse video (its
/// block cursor, drawn only while the terminal has focus); typed input is
/// painted at normal intensity. So [`placeholder_blanked`] clears a row's
/// content only when EVERY content cell is faint, allowing just that one
/// leading inverse cell. The CLI's idle SIGNAL was the alternative the issue
/// named and it is not used: the one idleness signal this guard has is the
/// rendered composer, and a hook- or transcript-based "turn ended" is
/// CLI-specific plumbing that would still not say whether a dialog is up now.
///
/// **Scoped to one row: the LOWEST row that leads with a prompt glyph** — the
/// composer, since a CLI's input box sits below its transcript. A faint row
/// higher up is transcript, and clearing it could manufacture an "empty
/// composer" above a live dialog. A dialog's highlighted choice (`❯ 1. Yes`) is
/// painted at normal intensity, so the rule leaves it — and every question row
/// on screen — exactly as it was. Every other row is `render_visible`'s,
/// unchanged.
///
/// **The rule does not check that the lowest glyph-led row IS the composer.**
/// With a glyph-less dialog up (reverse-video `AskUserQuestion`), the lowest
/// such row is whatever sits above it. Any faint row led by a prompt glyph, such
/// as a past prompt or a dim hint or tool-output line starting with `$` or `>`,
/// is cleared, and the WEAK idle reading turns true. Facts about today's screens
/// keep that closed, not this rule (Claude Code paints past prompts at normal
/// intensity). The residual is argued in `docs/design/orchestration.md`'s #3426
/// section and pinned by
/// `residual_a_faint_prompt_row_above_a_glyphless_dialog_reads_as_an_idle_composer`.
#[doc(hidden)] // pub for integration tests
pub fn question_visible(bytes: &[u8], cols: u16, rows: u16) -> Option<String> {
    let styled = termgrid::render_visible_styled(bytes, cols, rows);
    let mut text: Vec<String> =
        styled.iter().map(|r| r.iter().map(|c| c.ch).collect()).collect();
    let composer = text.iter().rposition(|t| {
        let d = deframe(t);
        PROMPT_GLYPHS.iter().any(|g| d.starts_with(*g))
    });
    if let Some(i) = composer {
        if let Some(blanked) = placeholder_blanked(&styled[i]) {
            text[i] = blanked;
        }
    }
    trustworthy_composition(text.join("\n"))
}

/// This composer row with its placeholder cleared, or `None` when the row holds
/// anything a human could have typed (#3426).
///
/// Reads the cells after the prompt glyph, less any trailing frame (the `│` of a
/// boxed composer). `Some` only when there is content, at least one content cell
/// is faint, and every content cell is faint — except the FIRST, which may
/// instead be inverse, because that is where the CLI draws its cursor over the
/// placeholder. A typed character at normal intensity anywhere refuses, and
/// refusing leaves the row as it was: the guard's pre-#3426 reading, which is
/// the direction it is always allowed to err in.
///
/// Whitespace is never evidence either way: an inverse SPACE is the cursor
/// after typed text, and a faint space is indistinguishable from any other.
fn placeholder_blanked(row: &[termgrid::StyledCell]) -> Option<String> {
    let glyph = row.iter().position(|c| !is_frame_char(c.ch))?;
    if !PROMPT_GLYPHS.contains(&row[glyph].ch) {
        return None;
    }
    let end = row.iter().rposition(|c| !is_frame_char(c.ch)).map_or(glyph + 1, |e| e + 1);
    let content: Vec<(usize, &termgrid::StyledCell)> = row
        .iter()
        .enumerate()
        .take(end)
        .skip(glyph + 1)
        .filter(|(_, c)| !c.ch.is_whitespace())
        .collect();
    let first = content.first()?.0;
    let placeholder = content.iter().all(|(i, c)| c.faint || (*i == first && c.inverse))
        && content.iter().any(|(_, c)| c.faint);
    if !placeholder {
        return None;
    }
    let s: String = row
        .iter()
        .enumerate()
        .map(|(i, c)| if i > glyph && i < end { ' ' } else { c.ch })
        .collect();
    Some(s.trim_end().to_string())
}

/// One-shot "is a question on screen right now" snapshot (#420 rev-15 B1) —
/// for a checkpoint that just needs to know NOW, not hold-and-wait: the
/// stranded-text flush must not blind-Enter into a live dialog, but it's not
/// itself the guard responsible for holding — the pre-paste checkpoint that
/// immediately follows it owns that. A single call to the hold predicate is
/// exactly a one-shot read: its two-consecutive-clear release requirement only
/// engages once a hold has actually observed a question at least once
/// (`ever_shown`), so a lone call just answers "is it shown right now".
///
/// #534: this reads the composed screen too. It has to — this is the gate
/// `write_admission` consults at the instant of the write, so a checkpoint
/// still keyed on the byte ring alone would re-assert, one call later, the
/// hold the guard had just released on grid evidence. No witness: nothing
/// downstream of a one-shot read has an abort record to put one in.
pub(in crate::orchestration) fn question_active_now(
    ptys: &crate::pty::PtyManager,
    pty_id: u32,
    pasted_text: Option<&str>,
    delivered: Vec<String>,
) -> bool {
    question_active_witnessed(ptys, pty_id, pasted_text, delivered).active
}

/// One one-shot reading of a pane's question gate (#903): the decision, the
/// evidence, and — for the last-resort override — whether that same instant's
/// composed screen showed the CLI's own input prompt.
///
/// A struct rather than a widening tuple because the third field is easy to
/// mistake for the second's negation and is not: `active` is the gate,
/// `idle_prompt` is a fact about the CURRENT render that
/// [`question_override_admits`] is allowed to act on only after a bound has
/// elapsed. They disagree exactly in the case #903 exists for.
pub struct QuestionReading {
    /// Is the question gate holding right now?
    pub active: bool,
    /// What the detector matched on this poll, for the audit.
    pub witnessed: Option<QuestionWitnessed>,
    /// Did this poll's composed screen show the CLI's input prompt — empty, or
    /// holding nothing but loomux's own paste ([`idle_prompt_row_rendered`], the
    /// WEAK reading, without [`idle_prompt_rendered`]'s menu-absent conjunct)?
    ///
    /// **The weak one, deliberately, and it is the whole of the override's
    /// design** (rev-427 B2). Feeding this from the strong reading would make the
    /// override unreachable: the strong reading is what `grid_evidence_for`
    /// already releases on, so a pane satisfying it is not holding, and there
    /// would be nothing left to override. The override IS the time-bounded
    /// downgrade of that one conjunct — see [`question_override_admits`] and the
    /// design note for what stops it pressing Enter into a live menu.
    ///
    /// `false` for an unreadable screen, and `false` when the ring matched
    /// nothing: no composition (or no hold) means no idleness claim.
    pub idle_prompt: bool,
}

/// [`question_active_now`], plus WHAT the detector matched (#820).
///
/// The one-shot read is where a *sustained* hold lives. `deliver_now`'s own
/// holds are individually capped and every one of them already audits its
/// match (`witness_audit`, #513(c)/F2); the queue drainer re-arms this read
/// every `QUEUE_DRAIN_POLL` with **no** cap, so the record a human actually
/// diagnoses a strand from is the drainer's — and that record said
/// `blocked_on: "question"` and nothing whatever about which shape, on which
/// line, with what the screen said about it. #513's blind spot, one hold
/// class over, and the reason #820's false positive could not name itself:
/// the pane was held for the whole of a session by a signal no record
/// identified.
///
/// A witness rather than a re-read at the audit site, for the same reason
/// `wait_for_question_clear` returns one: a fresh read there would describe a
/// different instant than the one that decided.
pub(in crate::orchestration) fn question_active_witnessed(
    ptys: &crate::pty::PtyManager,
    pty_id: u32,
    pasted_text: Option<&str>,
    delivered: Vec<String>,
) -> QuestionReading {
    let witness: QuestionWitness = Default::default();
    let active = question_hold_predicate_sampled(
        || question_sample(ptys, pty_id),
        pasted_text.map(str::to_string),
        Some(std::rc::Rc::clone(&witness)),
        delivered,
    )();
    let seen = witness.borrow().clone();
    // #903: taken off the witness rather than re-derived here, so the term the
    // override is decided on and the record that explains it are the same poll's
    // reading of the same screen — and so there is exactly ONE place
    // (`question_hold_predicate_sampled`) that decides what "the composer is on
    // screen" means. `None` — the ring matched nothing, so no witness — reads
    // `false`, which is right twice over: there is no hold to override, and a
    // streak must not be built out of polls that were never holding.
    let idle_prompt = seen.as_ref().is_some_and(|w| w.idle_row);
    QuestionReading { active, witnessed: seen, idle_prompt }
}

/// #532: the guards a delivery must find satisfied **at one instant**, on the
/// pass that actually commits to a write.
///
/// A straight line of checkpoints is what inverted both safety signals in
/// #532. `deliver_now` checked box occupancy (#111/#171) FIRST and the
/// interactive question (#420) SECOND — and the second one *blocks*, for up to
/// `QUESTION_HOLD_MAX` (two minutes). Nothing re-read occupancy afterwards, so
/// the delivery pasted on a green light earned two minutes earlier against a
/// box that was empty *then*.
///
/// That is not a rare interleaving; it is the ordinary one, because the same
/// event resolves the second gate and violates the first. A human typing is
/// what pushes a stale question out of `prompt_wait_detected`'s window (their
/// echo shifts the byte ring), so on a stale hold the keystroke that *releases*
/// the question gate is the same keystroke that *occupies* the box. The
/// delivery then pasted onto their half-written line and the pre-Enter quiet
/// wait submitted the merged result the moment they paused — the human's
/// report, mechanism for mechanism.
///
/// So every gate is re-read here, together, and a checkpoint's answer is never
/// carried across a wait that can outlive it. `box_pending` is checked first
/// because #510 is the absolute: human-typed content in the box outranks even a
/// question, since the cost of holding too long is a badge and the cost of
/// submitting over a person's line is unrecoverable.
///
/// Pure and three-way (rather than the `bool` this replaces at three call
/// sites) so the precedence is directly pinnable and so a caller can badge and
/// enqueue with the reason that actually blocked instead of re-deriving it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteAdmission {
    /// Every gate is clear right now — paste, or press.
    Go,
    /// Human-typed characters are outstanding in the box (#111/#171, #510).
    HoldBoxOccupied,
    /// An interactive question/permission TUI is on screen (#420).
    HoldQuestion,
}

impl WriteAdmission {
    /// Whether this admission permits a write right now.
    pub fn go(self) -> bool {
        matches!(self, WriteAdmission::Go)
    }
    /// The delivery-held badge reason for the gate that blocked, so a caller
    /// never picks one that disagrees with the gate it actually stopped on.
    /// `None` for `Go` — there is no hold to badge.
    pub fn held_reason(self) -> Option<HeldReason> {
        match self {
            WriteAdmission::Go => None,
            WriteAdmission::HoldBoxOccupied => Some(HeldReason::BoxOccupied),
            WriteAdmission::HoldQuestion => Some(HeldReason::InteractiveQuestion),
        }
    }
    /// The queue's reason for enqueueing when a re-verify round gives up.
    /// `Go` maps to `BoxOccupied` only because the type demands a value; no
    /// caller enqueues on `Go` (see `deliver_now`, which returns early).
    pub fn enqueue_reason(self) -> queue::EnqueueReason {
        match self {
            WriteAdmission::HoldQuestion => queue::EnqueueReason::Question,
            WriteAdmission::Go | WriteAdmission::HoldBoxOccupied => queue::EnqueueReason::BoxOccupied,
        }
    }
}

#[doc(hidden)] // pub for integration tests
pub fn write_admission(box_pending: bool, question_active: bool) -> WriteAdmission {
    if box_pending {
        return WriteAdmission::HoldBoxOccupied;
    }
    if question_active {
        return WriteAdmission::HoldQuestion;
    }
    WriteAdmission::Go
}

/// The pre-Enter write gate as its own STEP (#532, extracted for rev-12 B1) —
/// the last reading `deliver_now` takes before pressing Enter, against the live
/// pane.
///
/// Extracted for exactly the reason `flush_stranded_text`'s doc gives for the
/// same shape: inline at the call site, this — the single most safety-critical
/// line the PR adds — could be deleted with the whole suite still green,
/// because nothing can construct `deliver_now` (it needs a concrete `Wry`
/// `AppHandle`, unavailable headless). As a named function it is drivable
/// against a real fake-child-backed `PtyManager`, and `deliver_now` has nothing
/// left to get wrong beyond calling it.
///
/// Only the box is consulted. The question gate is a *blocking hold*
/// (`wait_for_question_clear`) that runs immediately before this and has
/// already resolved by the time we get here; re-reading it would either be
/// redundant or would re-open a hold this delivery already paid for. So this
/// answers the one question no other pre-Enter check asks: is there human-typed
/// content in the box *right now*, which this Enter would submit.
///
/// `unwrap_or(false)` for a closed pty is deliberate, and differs from
/// `flush_stranded_text`'s `unwrap_or(true)` for a stated reason rather than by
/// oversight: there, the decision is a blind Enter with no downstream net, so an
/// unreadable pane must decline; here the write immediately below fails on its
/// own and audits `prompt-failed`, which is both the pre-existing behaviour and
/// the more informative one. A closed pane has no box and nobody typing into
/// it, so there is nothing for this guard to protect either way.
#[doc(hidden)] // pub for integration tests
pub fn preenter_admission(ptys: &crate::pty::PtyManager, pty_id: u32) -> WriteAdmission {
    write_admission(ptys.input_pending(pty_id).unwrap_or(false), false)
}

/// #532: how many times `deliver_now`'s pre-paste checkpoints may re-verify
/// each other before giving up and letting the queue hold the delivery.
///
/// Small on purpose. Each round already contains the full, individually
/// capped holds (`HUMAN_INPUT_HOLD_MAX` + `QUESTION_HOLD_MAX`), so the rounds
/// bound *re-arming*, not waiting — and re-arming more than a couple of times
/// means the two gates are genuinely alternating, which is a pane a human is
/// actively working in. Giving up there is not a loss: the entry stays at the
/// front of its queue and the drainer retries it with no cap, which is both
/// the pre-existing recovery and the correct one. Raising this would make
/// loomux hold the delivery mutex longer against a busy human for no extra
/// chance of success.
pub(in crate::orchestration) const PREPASTE_RECHECK_ROUNDS: u32 = 3;

/// #532: how long the interactive-question guard may keep holding ONE pane's
/// delivery before loomux stops re-arming that hold silently and badges the
/// pane for a human ([`StrandedBlocker::QuestionStale`]).
///
/// The hold this bounds is per-pane and aggregate, not per-attempt. A single
/// `wait_for_question_clear` is already capped at `QUESTION_HOLD_MAX` (two
/// minutes) — but capping out only aborts *that* attempt, leaving the entry at
/// the front of the queue for `run_queue_drainer` to retry with no cap, which
/// re-arms the same hold every `QUEUE_DRAIN_POLL` forever. That is the same
/// unbounded-latch shape #518 found in the human-input block, one guard over:
/// the per-attempt cap was never the bound, because nothing bounds the
/// attempts.
///
/// Ten minutes, and **what that is and is not sized against** (rev-12 NB2).
/// It is comfortably shorter than `QUEUE_STILL_QUEUED_NOTICE_AFTER` (30 min),
/// so the specific diagnosis lands before the generic "still queued" one. It is
/// NOT, as this doc previously claimed, longer than `deliver_prompt`'s
/// worst-case hold chain: `PREPASTE_RECHECK_ROUNDS` re-arms the pre-paste pair
/// up to three times, so that chain is now `3 x (HUMAN_INPUT_HOLD_MAX +
/// QUESTION_HOLD_MAX)` plus the pre-Enter waits — roughly 13 minutes, past this
/// bound rather than inside it.
///
/// The bound cannot fire mid-delivery anyway, but for a *structural* reason
/// rather than an arithmetic one, and the difference matters to anyone editing
/// either constant: the drainer thread that evaluates this bound is the same
/// thread that blocks inside `deliver_now`, so while a delivery is holding,
/// nothing is polling. The consequence to know is on the other side — time to
/// badge is measured from the pane's hold-episode start ([`HoldEpisode`], #560;
/// `enqueued_ms` before that) but only *sampled* between attempts, so a pane
/// that keeps entering `deliver_now` can take up to roughly 19 minutes
/// to badge, not 10. Sizing this constant against the hold chain would be
/// reasoning about a race that cannot happen; sizing it against how long a
/// human will accept silence is the real constraint.
///
/// Unlike #518's bound this one does **not** release a write — see
/// [`StrandedBlocker::QuestionStale`] for why a byte ring cannot justify one —
/// so it is not a per-group guardrail either. There is no workflow for which a
/// human wants to be told *later* that loomux may be stuck.
pub const QUESTION_HOLD_STALE_AFTER: Duration = Duration::from_secs(10 * 60);

/// #532: has this pane's delivery been held long enough to be escalated to a
/// human? Pure, and deliberately a function of the CLOCK ALONE.
///
/// **rev-12 NB3 — why no signal may veto this.** The first cut took
/// `box_pending` and returned `false` whenever it was set, on the theory that a
/// hold explained by the human's own line needs no report. That was the same
/// mistake this PR exists to fix, one level up. `input_pending` is not a
/// reading of the box; it is `input_box_len > 0`, a running counter that only
/// human writes move and that `classify_human_input` zeroes on **only**
/// `\r`/`\n`, Ctrl-U and Ctrl-C (see `pty.rs`'s `note_user_input`). Every other
/// route to an empty box — a bare `ESC`, a TUI clearing the line in response to
/// a key with no occupancy delta, or the CLI consuming the line itself, which
/// loomux never observes at all — leaves the counter stuck above zero with
/// nothing in the box.
///
/// So the counter has a reachable stuck-true mode, and letting it veto the
/// escalation meant a pane could hold forever *and never tell anyone*: the
/// pre-paste loop holds, the pre-Enter gate declines, the flush declines, and
/// the badge that exists to report exactly that never fires. A staleness check
/// that a stuck flag can silence is not a bound. **An escalation must not be
/// suppressible by any of the signals it exists to report on** — which is why
/// this takes no signal at all, and [`held_escalation`] decides only what to
/// *call* the blocker, never whether to speak.
///
/// `bound_ms == 0` disables the bound (pre-#532 behaviour) rather than making
/// it fire instantly, so a mis-set constant degrades to silence rather than to
/// a badge on every pane.
///
/// `held_since_ms` is the PANE's hold-episode start ([`HoldEpisode`], #560) —
/// the moment this pane last failed to accept a delivery and has not accepted
/// one since — so the clock measures the thing the human experienced (a pane
/// that has not moved), not the lifetime of any one attempt and not the age of
/// whichever entry happens to be at the queue front. It was `front.enqueued_ms`
/// until #560; see [`ends_hold_episode`] for why an entry-scoped clock could be
/// restarted by a `StrandedSubmit` marker on a pane that never recovered.
#[doc(hidden)] // pub for integration tests
pub fn hold_bound_elapsed(held_since_ms: u64, now_ms: u64, bound_ms: u64) -> bool {
    if bound_ms == 0 {
        return false;
    }
    now_ms.saturating_sub(held_since_ms) >= bound_ms
}

/// #903: how long a pane's delivery may be held by the QUESTION gate alone
/// before loomux stops believing its own detector and **pastes** anyway.
///
/// Pasted, and — since #903 B2 — delivered: a granted override carries to the
/// Enter rather than stopping at the paste. The earlier version of this doc said
/// the pre-Enter checkpoint is not overridden and rested the safety argument on
/// that; it is no longer true and the argument now rests elsewhere. What
/// withholds the Enter from a live dialog is [`override_enter_admits`]: fresh
/// re-reads, every one of which must show this pane's own composer holding this
/// delivery's paste. What that does NOT bound is written up in
/// `docs/design/question-gate-authorship.md` rather than left implicit here. See
/// [`question_override_admits`] for the grant itself.
///
/// **Sized between the two clocks that already exist**, which is the whole of
/// the choice: longer than [`QUESTION_HOLD_STALE_AFTER`] (10 min), so the human
/// is badged and given five minutes to look before loomux acts on their behalf;
/// shorter than `QUEUE_STILL_QUEUED_NOTICE_AFTER` (30 min), so a queue moves
/// before the generic "still queued" notice is the first anyone hears of it.
/// #903's own incidents ran 25 and 30+ minutes and ended with panes killed by
/// hand — the bound has to land inside a human's patience, not merely inside
/// infinity.
///
/// **What a wrong override actually costs, in the right order** (rev-427 B2 —
/// the earlier version of this paragraph led with delivery-id dedup, which is
/// the *second* line of defence and does not cover the expensive failure):
///
/// 1. **The Enter is still gated.** An override skips the PRE-PASTE question
///    checkpoint only. `deliver_now`'s pre-Enter `wait_for_question_clear` runs
///    unskipped, against a screen masked with this delivery's own paste, and it
///    is what withholds the Enter from a live dialog. That matters because the
///    unrecoverable harm here is not a stray paste — it is an Enter *selecting*
///    a highlighted option, which no dedup rule can undo.
/// 2. **Then dedup covers the rest.** A paste that lands in an open dialog's lap
///    is recoverable: the human answers the dialog, and if the paste was eaten
///    the drainer re-sends under the same delivery id, which every receiver
///    treats as a duplicate and drops.
///
/// The cost of never overriding is what this issue is: a queue that never moves
/// and a pane a human has to kill. The design note argues both sides at length,
/// including the residual this leaves — see `idle_prompt` on [`QuestionReading`]
/// for why the term below is the WEAK idleness reading and why the strong one
/// would make this function dead code.
///
/// `0` disables the override (pre-#903 behaviour), the same convention
/// [`hold_bound_elapsed`] gives every bound here, so a mis-set constant degrades
/// to today's holding rather than to delivering into every dialog on screen.
pub const QUESTION_HOLD_OVERRIDE_AFTER: Duration = Duration::from_secs(15 * 60);

/// #903: how many CONSECUTIVE drainer polls must read an idle prompt before the
/// override is allowed to fire.
///
/// The same reasoning as [`QUESTION_RELEASE_CONSECUTIVE_CLEAR_POLLS`], and the
/// same number: one reading of a composed screen can catch a mid-redraw
/// instant, and this reading licenses a WRITE. Two polls is four seconds
/// against a bound measured in quarter-hours, so it costs nothing that matters.
#[doc(hidden)] // pub for integration tests
pub const QUESTION_OVERRIDE_CONSECUTIVE_READS: u32 = 2;

/// #903: may this poll deliver despite the question gate saying no?
///
/// Pure, and every term is load-bearing:
///
/// - **`admission` must be `HoldQuestion`.** A box-occupied hold is never
///   overridden — #510's absolute (human-typed content in the box outranks
///   everything) is untouched by this, and `write_admission` checks the box
///   first precisely so that a pane with both blockers reports the box one and
///   lands here ineligible.
/// - **`held_since_ms` is the PANE's hold-episode start** ([`HoldEpisode`]),
///   never an entry's `enqueued_ms` — the same clock [`hold_bound_elapsed`]
///   already measures the badge against, so "badged at 10, overridden at 15"
///   describes one continuous thing a human watched rather than two unrelated
///   timers. `None` — no open episode — is never eligible: nothing has been
///   measured, so nothing can be overdue.
/// - **`idle_streak` is a count of FRESH reads**, taken by the caller on the
///   same polls that produced `admission`. Not a latched flag and not a
///   historical one: the override re-proves the pane is idle every time it
///   fires, which is the property #903 asks for and the reason this takes a
///   streak rather than a bool the caller could have set minutes ago.
///
/// Every term must hold on the SAME poll. A pane that reads idle for ten
/// minutes and then paints a dialog is ineligible on the very next poll, with no
/// memory of having been eligible.
#[doc(hidden)] // pub for integration tests
pub fn question_override_admits(
    admission: WriteAdmission,
    held_since_ms: Option<u64>,
    now_ms: u64,
    bound_ms: u64,
    idle_streak: u32,
) -> bool {
    if admission != WriteAdmission::HoldQuestion {
        return false;
    }
    if idle_streak < QUESTION_OVERRIDE_CONSECUTIVE_READS {
        return false;
    }
    held_since_ms.is_some_and(|since| hold_bound_elapsed(since, now_ms, bound_ms))
}

/// One fresh pre-Enter re-read, reduced to the two bits the decision uses (#903
/// B2).
///
/// A named pair rather than a tuple because both bits are booleans and swapping
/// them silently inverts the safety argument — `active` says the gate is still
/// holding, `idle_prompt` says the pane's own screen shows its composer holding
/// this paste.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuestionReread {
    /// Did the question gate still read as holding on this poll?
    pub active: bool,
    /// Did that same screen show the CLI's composer holding this delivery's
    /// paste — [`idle_prompt_row_rendered`], the WEAK reading, which is the
    /// override's own standard?
    pub idle_prompt: bool,
}

impl QuestionReread {
    /// Does THIS one read permit the Enter?
    ///
    /// `!active` counts, and it is not a widening: a poll where the gate has
    /// simply gone clear is a poll the ordinary checkpoint would have released
    /// on. Without it a screen that repainted between the abort and this re-read
    /// would strand the paste for having got BETTER.
    pub fn admits(&self) -> bool {
        !self.active || self.idle_prompt
    }
}

/// #903 B2: do these fresh pre-Enter re-reads carry a granted override's Enter?
///
/// [`question_override_admits`] decides the PASTE, minutes earlier, on the
/// drainer's poll. This decides the ENTER, here, now — and the split is the whole
/// of what makes the grant worth anything. Skipping only the pre-paste gate left
/// the pre-Enter checkpoint to re-read an unchanged screen, reach the same false
/// positive the grant was issued because of, and abort with the text already in
/// the box.
///
/// Pure, and separated from the pane reading for the reason
/// [`question_hold_predicate_sampled`]'s own doc gives about rev-15 B4: a
/// decision welded to a live `PtyManager` is one no test in this repo can drive,
/// so the rule ends up pinned by nothing. The GRANT itself is not a term here —
/// it is the caller's `question_overridden`, decided by the drainer poll that
/// observed the pane, and re-deriving it at this site would describe a different
/// instant than the one that admitted the write.
///
/// Two terms, both narrowing:
///
/// - **Enough reads.** [`QUESTION_OVERRIDE_CONSECUTIVE_READS`], for its own
///   reason: one reading of a composed screen can catch a mid-redraw instant,
///   and this one licenses an Enter. An empty slice is never enough, so a caller
///   that took no reads at all cannot pass by omission.
/// - **EVERY read admits.** Not a majority and not the last one — a pane that
///   painted a dialog on any of these polls is ineligible, with no memory of
///   having been eligible on the others.
#[doc(hidden)] // pub for integration tests
pub fn override_enter_admits(reads: &[QuestionReread]) -> bool {
    reads.len() as u32 >= QUESTION_OVERRIDE_CONSECUTIVE_READS
        && reads.iter().all(QuestionReread::admits)
}

/// The production half of [`override_enter_admits`]: take the fresh reads this
/// pane owes, then let that function decide.
///
/// Stops early on the first read that does not admit — there is nothing for a
/// second poll to rescue, and the delivery is aborting anyway, so the sleep
/// would be latency spent on a decision already made.
pub(in crate::orchestration) fn preenter_override_admits(
    ptys: &crate::pty::PtyManager,
    pty_id: u32,
    pasted_text: &str,
    delivered: Vec<String>,
) -> bool {
    let mut reads: Vec<QuestionReread> = Vec::new();
    for round in 0..QUESTION_OVERRIDE_CONSECUTIVE_READS {
        if round > 0 {
            std::thread::sleep(QUESTION_HOLD_POLL);
        }
        let r = question_active_witnessed(ptys, pty_id, Some(pasted_text), delivered.clone());
        let read = QuestionReread { active: r.active, idle_prompt: r.idle_prompt };
        reads.push(read);
        if !read.admits() {
            break;
        }
    }
    override_enter_admits(&reads)
}
