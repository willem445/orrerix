//! Reading an agent pane's screen: prompt-wait and question detection, the
//! idle prompt and menu structure, and CLI readiness.
//! Design note: `docs/design/pty-input-path.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. Sibling files it
//! calls: `panetail.rs`, `questionhold.rs`, `tuning.rs`.

use super::*;
/// How far up from the bottom of a composed screen [`idle_prompt_row_rendered`]
/// looks for the CLI's own composer (#903).
///
/// A window, not the whole screen, because the reading has to mean "this is the
/// pane's CURRENT input affordance" and not "a composer was painted at some point
/// in the scrollback".
///
/// **Sized against the chrome BELOW the composer, not against a box at rest**
/// (rev-433). Six was the latter and was the wrong measurement: at the pre-Enter
/// checkpoint the composer is not one row, it is however many rows the brief
/// wrapped onto, and only its LAST rows need to be in range — but everything the
/// CLI paints underneath them does. The captures put that at one to three rows
/// (Claude Code: separator + mode footer; copilot: `@ files · # issues`, plus a
/// right-aligned session line and, on 1.0.71+, a scrollbar column), so eight
/// leaves a clear margin for a CLI that adds one more hint bar without letting a
/// stale composer row from further up license a release.
///
/// The authorship clause makes this far less load-bearing than it was: a
/// multi-row paste puts a claimed row on *every* row of the composer, so the
/// window only has to reach the bottom-most one. `h8` asserts that reach as a
/// precondition on each fixture, so a CLI that grows its footer fails loudly here
/// instead of silently reading "not a composer".
const IDLE_PROMPT_TAIL_ROWS: usize = 8;
/// How far up from the bottom of a composed screen [`menu_structure_rendered`]
/// reads its TOKEN lists (#903 rev-438).
///
/// **Smaller than [`IDLE_PROMPT_TAIL_ROWS`], and the two measure different
/// things.** That one has to *reach* the composer, so it is sized generously
/// against everything painted below it. This one has to *exclude the transcript*,
/// because the tokens it reads (`1. yes`, `use arrow keys`, `enter to select`)
/// are the ones #40 already found in ordinary finished-turn prose — the whole
/// reason `prompt_wait_match` windows them to `last_painted`.
///
/// Sized against what sits between the screen bottom and a dialog's own footer
/// when that dialog is the thing the composer is waiting under: one or two rows
/// of chrome below the composer (a hint bar, a right-aligned session line), the
/// composer's own last row, and the footer itself. Four.
///
/// **The residual, stated because the number is tight.** A composer holding a
/// MULTI-row paste consumes window slots, so a dialog footer above it can fall
/// out of reach. The pointer clause is unwindowed and still catches every dialog
/// that paints a pointer at content — every capture in the suite except Claude
/// Code's reverse-video `AskUserQuestion`, which is the one shape this residual
/// is about. The refinement that would close it is a window measured from the
/// composer's own top row rather than from the screen bottom; it is not taken
/// here because the composer's rows are exactly what the masked view no longer
/// has (see the design note's limits).
const MENU_TOKEN_TAIL_ROWS: usize = 4;

// Kickoff readiness: a fixed boot delay loses the race on a loaded machine
// (a CLI that boots slower than the delay flushes the pasted prompt along
// with its startup stdin buffer — observed live with a reviewer spawned
// while a worker ran cargo test). Instead, watch the pane's output ring and
// paste only once the CLI has painted its UI and gone quiet.
/// Minimum wait before even checking (lets the process start writing).
const READY_MIN_WAIT: Duration = Duration::from_millis(1500);
/// Output must be idle this long (UI finished painting) to count as ready.
const READY_QUIET: Duration = Duration::from_millis(1200);
/// Minimum bytes of output before a CLI can be considered painted.
const READY_MIN_OUTPUT: usize = 512;
/// Give up waiting and paste anyway after this long.
const READY_MAX_WAIT: Duration = Duration::from_secs(25);

/// Attention routing (#6): does a pane's ANSI-stripped output tail look like a
/// CLI parked on a prompt only the human can answer — a permission dialog, a
/// yes/no confirmation, or a numbered/selection menu? This is the "last output"
/// half of idle-with-prompt detection; the caller pairs it with an
/// output-quiet check (this alone can't tell a live prompt from the same words
/// scrolled past). So it errs toward recognizable interactive-prompt structure
/// rather than any mention of a question. Case-insensitive.
///
/// Two tiers of signal, by how prose-safe each is (#40 review):
/// - *Structured* signals (numbered y/n menu, explicit y/n tokens, stock
///   permission phrasings) don't occur in ordinary prose, so they're honored
///   across the last ~12 lines.
/// - *Prose-like* signals — a bare selection pointer, the plain-English menu
///   footer ("use arrow keys", "enter to select") and, since #903, the
///   permission phrasings that are ordinary sentences rather than punctuated UI
///   ([`PROSE_PERMISSION_PHRASES`]) — DO appear in finished-turn
///   agent output (agents describe keyboard UIs, paste shell prompts, echo
///   `a › b` breadcrumbs). A *live* menu paints these as the last thing on
///   screen, with its pointer *leading* an option line (after any box frame);
///   prose does neither. So the pointer must lead a de-framed line, and the
///   footer is only read from the last few non-empty lines — once the CLI
///   redraws its idle input box below the prose, the phrase falls out of range.
/// **Callers mask; this function only detects.** It answers "is this text
/// question-shaped", never "whose text is this" — so every caller reading a
/// live pane owes it [`mask_loomux_notices_with_record`] first — with that
/// pane's delivery record, so a notice that WRAPPED is masked over every row it
/// wrapped onto (#576 residual) — and, where the caller knows what it just
/// pasted, `mask_own_paste`. Masking is deliberately NOT folded
/// in here: the two are different questions, and `mask_own_paste` needs a
/// per-call argument this signature does not have. The cost of that split is
/// that a new consumer can forget — which is exactly what happened to the two
/// attention-chip readers in `attention_tick` / `plain_pane_attention`, found
/// in review after the delivery gate had already been fixed (#576 rev-126).
/// If you are adding a third reader of a live pane, mask it.
pub fn prompt_wait_detected(tail: &str) -> bool {
    prompt_wait_match(tail).is_some()
}

/// Strip a line's leading box border / bullet / indent so a menu pointer
/// inside a bordered dialog (`│ ❯ Yes`) is seen to *lead* its content.
///
/// Module-scope since #534 rev-13: the composed screen has to apply the SAME
/// rule the ring detector does when re-reading the pointer signal
/// ([`pointer_rendered`]), and two copies of a de-framing rule is exactly how
/// the two readings would drift apart.
pub(in crate::orchestration) fn deframe(l: &str) -> &str {
    l.trim_start_matches(is_frame_char)
}

/// What `deframe` treats as decoration rather than content.
///
/// Hoisted out of `deframe` (#727) so [`leads_with_pointer`] can ask the same
/// question of what FOLLOWS a pointer glyph as `deframe` asks of what precedes
/// it — one strip set, so "is there content here" means the same thing on both
/// sides of the glyph.
pub(in crate::orchestration) fn is_frame_char(c: char) -> bool {
    c == '│' || c == '┃' || c == '|' || c == '*' || c == '●' || c == '•' || c == '◆'
        || c.is_whitespace()
}

/// The glyphs that mark a menu's highlighted choice when they LEAD a line.
pub(in crate::orchestration) const POINTER_GLYPHS: [char; 3] = ['❯', '›', '→'];

/// Does this line LEAD with a menu pointer that POINTS AT SOMETHING, after any
/// box framing?
///
/// The single definition of the pointer rule (#534 rev-13). Both readings call
/// it — the ring detector over its last painted lines, [`pointer_rendered`]
/// over every rendered row — because a re-read that is looser than the detector
/// it stands in for would pin holds open on ordinary prose, and one that is
/// tighter would release into a live menu. Neither is allowed to drift.
///
/// **The glyph must lead content, and that is #727.** Claude Code's own input
/// box is a bare `❯` on its own row when empty — an idle pane's *prompt*, the
/// opposite of a question. Matching it made every idle Claude Code pane read as
/// "a menu is displayed" on BOTH readings at once: the ring matched whatever
/// `strip_ansi` had concatenated that glyph onto, and [`pointer_rendered`] then
/// found the same bare glyph among the rendered rows, so `grid_evidence_for`
/// answered `StillRendered` forever and the one release #534 added could never
/// fire. A resumed pane made it permanent — its replayed screen is static, so
/// nothing ever repaints the glyph away — and three panes were lost to a
/// 25-minute hold over a visibly idle input box.
///
/// This narrows the signal rather than the guard: a highlighted menu choice is
/// a pointer *at an option*, and there is no dialog whose selected option is
/// blank. A pointer with only framing after it (`│ ❯   │`) is likewise pointing
/// at nothing — hence [`is_frame_char`] on both sides rather than a bare
/// `trim`, so an empty prompt inside a box is read the same as one outside it.
fn leads_with_pointer(line: &str) -> bool {
    let d = deframe(line);
    POINTER_GLYPHS
        .iter()
        .any(|g| d.strip_prefix(*g).is_some_and(|rest| !rest.trim_matches(is_frame_char).is_empty()))
}

/// The glyphs a CLI paints to say "your turn to type" at its input box.
///
/// A superset of [`POINTER_GLYPHS`] in spirit but not in membership, and the
/// difference is the point: `❯`/`›` are used for BOTH a menu's highlighted
/// choice and an empty composer, which is why [`leads_with_pointer`] has to ask
/// what FOLLOWS the glyph (#727). `>` and `$` are only ever prompts — Claude
/// Code's older box and any shell — and `→` is only ever a pointer, so it is
/// absent here.
pub(in crate::orchestration) const PROMPT_GLYPHS: [char; 4] = ['❯', '›', '>', '$'];

/// Is this rendered row an EMPTY input prompt — a prompt glyph with nothing
/// after it but framing (#903)?
///
/// The exact complement of [`leads_with_pointer`]'s content requirement, over
/// the same de-framing rule, so "a pointer at an option" and "a prompt with
/// nothing in it" can never both be true of one row and can never drift apart.
fn is_empty_prompt_row(line: &str) -> bool {
    let d = deframe(line);
    PROMPT_GLYPHS
        .iter()
        .any(|g| d.strip_prefix(*g).is_some_and(|rest| rest.trim_matches(is_frame_char).is_empty()))
}

/// The last [`IDLE_PROMPT_TAIL_ROWS`] non-empty rendered rows — the pane's
/// current chrome, as opposed to whatever is still sitting above it.
fn bottom_rendered_rows(visible: &str, n: usize) -> Vec<&str> {
    let rows: Vec<&str> = visible.lines().filter(|l| !l.trim().is_empty()).collect();
    rows[rows.len().saturating_sub(n)..].to_vec()
}

/// A pane's composed screen in the TWO forms the question guard's readings need
/// (#903, rev-427 B1).
///
/// The guard has always read one screen: the masked one, because "is a question
/// displayed" must not be answered by loomux's own notices or its own
/// just-pasted text. The idleness reading added by #903 needs the other one too,
/// and the reason is mechanical rather than stylistic:
///
/// **`mask_own_paste` REMOVES the rows it claims.** At the pre-Enter checkpoint —
/// the one checkpoint that runs with a `pasted_text` — our brief is sitting in
/// the CLI's composer, so the mask deletes exactly the row that proves a composer
/// is on screen. Reading idleness from the masked screen alone therefore answers
/// "no composer" for a pane whose composer is not merely present but holding our
/// own text, and the pre-Enter gate re-asserts the very false positive the
/// pre-paste gate just released — aborting with the paste stranded in the box.
/// That is rev-427's blocking finding, and it is worse than the bug this PR
/// fixes, because a stranded paste wedges a second gate behind it.
///
/// Keeping both views is what lets the idleness reading ask the right question —
/// *is the CLI at its composer* — instead of the accidental one — *is the
/// composer empty right this instant*. And it asks it **without a second copy of
/// "is this row ours"**: the mask's own recognition (deframe, wrap
/// reconstruction, the #820 pointer-strip and its short-line floor) is reused
/// verbatim by comparing the two views, because a row present in `with_paste`
/// and absent from `masked` is precisely a row [`mask_own_paste`] claimed.
///
/// **The two views differ by the PASTE mask only** (rev-433). Both have already
/// had [`mask_loomux_notices_with_record`] applied, and that is not tidiness: the
/// notice mask also removes rows, so diffing against a wholly unmasked screen
/// would read one of loomux's own `[orrerix] …` notice rows sitting near the
/// bottom of a transcript as "the composer", and release on it. Authorship of the
/// *composer* is the signal; authorship of anything loomux ever wrote is not.
#[derive(Clone, Copy, Debug)]
pub struct Composed<'a> {
    /// Loomux's own notices AND this delivery's paste removed. Everything that
    /// asks "is a question displayed" reads THIS.
    pub masked: &'a str,
    /// Notices removed, this delivery's paste still on it. Read for exactly one
    /// question: is the CLI's composer on screen, holding our own text.
    pub with_paste: &'a str,
}

impl<'a> Composed<'a> {
    /// Both views the same — correct at every checkpoint that has no paste of its
    /// own to mask (`pasted_text: None`), and the shape every test without a
    /// paste wants. With the two equal, `idle_prompt_row_rendered`'s "the paste
    /// mask claimed this row" clause is vacuously false and the reading collapses
    /// to the empty-composer one.
    pub fn plain(s: &'a str) -> Self {
        Composed { masked: s, with_paste: s }
    }
}

/// Is the CLI's own input prompt on screen, holding nothing but what loomux put
/// there (#903)?
///
/// The weaker of this module's two idleness readings, and the one the last-resort
/// override keys on: it says a composer is on screen and **says nothing about
/// what else is**. [`idle_prompt_rendered`] is the stronger one, and the
/// difference between them is deliberate — see that function, and the design
/// note's override-safety argument, for why the last resort is allowed the weaker
/// bar and what actually stops it pressing Enter into a dialog.
///
/// **Two independent clauses, and only one of them knows what a prompt looks
/// like** (rev-433). The split is the CLI-agnosticism argument, so it is written
/// out rather than left to the code:
///
/// - **Authorship (the general clause).** A row present in `with_paste` and
///   absent from `masked` is a row [`mask_own_paste`] claimed — loomux's own text
///   rendered by the CLI. At a checkpoint holding a `pasted_text` that text is,
///   by construction, sitting unsubmitted in the composer. **No glyph, no frame,
///   no shape at all is required**, which is what makes this work on a composer
///   nobody here has ever seen: opencode has no capture in this repo (design note
///   `opencode.md`, constraint 3), and it does not need one.
/// - **Emptiness (the shape clause).** With no paste there is nothing to key
///   authorship on, so an empty composer has to be recognised by appearance, and
///   the only marker generic enough to try is a prompt glyph with nothing after
///   it. It is a genuine limit, not an oversight — see the design note: a
///   decoration-only row cannot be told from a dialog's blank framed row on shape
///   alone (`claude-askuserquestion.txt` contains exactly such a row inside this
///   window), so widening this clause would release a live dialog. A CLI whose
///   *empty* composer paints no glyph therefore gets no layer-2 release and falls
///   through to the bounded override, which is what a last resort is for.
///
/// rev-433's blocking finding was the first clause carrying the second's glyph
/// requirement: copilot's 1.0.64+ framed composer paints `┃ ` on every row and
/// **no** prompt glyph, so a multi-row brief pasted into it read as "not a
/// composer", the pre-Enter gate re-asserted on the still-rendered prose, and the
/// paste stranded — a regression this PR introduced for that pane class, since
/// before it nothing was ever pasted there at all.
pub fn idle_prompt_row_rendered(c: Composed<'_>) -> bool {
    // Rows the paste mask kept. `mask_own_paste` only ever DROPS rows — never
    // rewrites them — so surviving rows are byte-identical and set membership is
    // an exact test rather than a fuzzy one. A duplicate row text of which only
    // one copy was claimed still reads as "kept", which errs toward NOT idle: the
    // conservative direction, and the one every reading here defaults to.
    let survived: std::collections::HashSet<&str> = c.masked.lines().collect();
    bottom_rendered_rows(c.with_paste, IDLE_PROMPT_TAIL_ROWS)
        .iter()
        .any(|r| !survived.contains(*r) || is_empty_prompt_row(r))
}

/// Is this pane sitting at an idle input prompt with no menu selection anywhere
/// on screen (#903)?
///
/// **Positive evidence of idleness, not the absence of evidence of a question.**
/// The distinction is the safety argument: a screen loomux cannot read, or one
/// whose CLI paints a box shape this does not recognise, answers `false` and the
/// hold stands. Only a screen that actually shows a composer — and shows no
/// highlighted choice anywhere, so a dialog cannot be sitting above one — is
/// allowed to end a hold.
///
/// The conjunct reads the **masked** screen, and that is load-bearing rather than
/// incidental: on the `with_paste` view, our own brief in a chevron composer
/// (`❯ <our text>`) leads a pointer at content, which is #820's exact trap and
/// would veto the release on evidence loomux itself wrote.
///
/// **rev-433 widened the conjunct from "no pointer" to "no menu STRUCTURE",** and
/// the two changes are one change: making the composer reading CLI-agnostic means
/// it now recognises composers it never did (copilot's framed one), which in turn
/// exposes the conjunct to dialog shapes it was never asked about. The shape that
/// mattered is Claude Code's `AskUserQuestion` — reverse video, so `strip_ansi`
/// leaves **no pointer at all**, only numbered options and a selection footer.
/// With the old conjunct, that dialog plus a composer holding our paste would
/// have read as idle and released. It is the fixture suite's own
/// `claude-askuserquestion.txt`, so this was reachable, not theoretical.
///
/// The assumption, stated so a future TUI change is a known break rather than a
/// mystery (the same courtesy #727's note pays for its own): **a CLI does not
/// paint a modal question and a free-text composer at the same time.** Every
/// dialog the fixtures cover replaces the box while it is up. A CLI that asked a
/// question *above* a live composer would read as idle here — which is why the
/// menu-structure clause is a conjunct rather than a nicety, and why the fixtures
/// assert this reading is false for every positive dialog capture in the suite,
/// not merely true for the negatives.
pub fn idle_prompt_rendered(c: Composed<'_>) -> bool {
    idle_prompt_row_rendered(c) && !menu_structure_rendered(c)
}

/// Does this composed screen carry menu STRUCTURE anywhere on it — a highlighted
/// choice, a numbered option, or a selection footer (#903 rev-433)?
///
/// Deliberately the *structural* signals only, never the prose-shaped ones. The
/// whole of #903 is that question-shaped **prose** must stop holding panes, so a
/// conjunct that keyed on `prompt_wait_detected` would veto every release this PR
/// exists to make — the prose is on the masked screen by definition. What this
/// asks instead is whether the CLI has painted something a human could *select*,
/// which no finished turn's report does.
///
/// **Two reaches, and the split is #40's, not a new invention** (rev-438). The
/// pointer keeps the whole screen; the TOKEN lists are read from the bottom rows
/// only.
///
/// - **Pointer: whole screen**, like [`pointer_rendered`] and for its reason — a
///   live menu can sit above rows of statusline and composer, and a leading
///   glyph at content is not something prose produces.
/// - **Tokens: [`MENU_TOKEN_TAIL_ROWS`] bottom rows.** `prompt_wait_match` has
///   windowed exactly these two lists to `last_painted` since #40, because they
///   *do* occur in ordinary finished-turn output — `fp-prose-arrow-keys.txt` is
///   the fixture that pins it, and it is prose describing a file picker. Reading
///   them over the whole screen made a pane that merely QUOTES "use arrow keys"
///   or "1. yes" carry menu structure forever, which vetoed the #903 release for
///   the exact class of transcript this issue is about. The conjunct now sees
///   only structure sitting near the composer, which is also what
///   `docs/orchestration.md` has always described.
///
/// **Position from `with_paste`, evidence from `masked`.** Where a row *is* on
/// screen is a fact about the render, so the window is measured on the view that
/// still has the composer in it — otherwise deleting the composer's rows pulls
/// transcript prose down into the window and undoes the split. What may *count*
/// as evidence is still only what loomux did not write, so rows the paste mask
/// claimed are filtered out before any token is looked for.
///
/// **Row-wise, not flattened** (rev-438's option 2, folded in because the row
/// filter above already gives us rows): a flatten joins neighbours, so
/// "…press Enter" ending one row and "to select…" beginning the next would
/// manufacture `enter to select` out of two unrelated lines. Erring toward "a
/// menu is up" is the cheap direction in general, but not when the invented
/// evidence is what keeps a pane wedged.
fn menu_structure_rendered(c: Composed<'_>) -> bool {
    if pointer_rendered(c.masked) {
        return true;
    }
    let survived: std::collections::HashSet<&str> = c.masked.lines().collect();
    bottom_rendered_rows(c.with_paste, MENU_TOKEN_TAIL_ROWS)
        .into_iter()
        .filter(|r| survived.contains(*r))
        .any(|r| {
            let row = r.trim().to_lowercase();
            NUMBERED_MENU_TOKENS.iter().chain(MENU_FOOTER_TOKENS).any(|t| row.contains(t))
        })
}

/// Does any rendered row lead with a menu pointer (#534 rev-13)?
///
/// The composed-screen re-reading of the `pointer-option` signal, and the fix
/// for a false-RELEASE path review found in round 2. That signal is the only
/// one whose evidence is a **position** rather than a substring, so it has no
/// token to look for again; the round-1 code fell back to looking for the
/// matched *line*, and that line comes from `strip_ansi` of the raw ring, which
/// deletes cursor-address sequences and so can concatenate several repaints of
/// one physical row into a string that was never on screen
/// (`termgrid`'s own header documents that exact behaviour for Claude Code).
/// Such a needle can never be found on a clean grid, so the check silently
/// became a no-op — in the one direction this change must never be wrong in.
///
/// This looks for the signal itself instead of a stringly artifact of it, and
/// **spatially**: every rendered row, not the last three. That window is
/// `prompt_wait_match`'s, and it is CHRONOLOGICAL — correct for a stream where
/// recent means last, wrong for a screen where a live menu can sit above rows
/// of statusline and input box. Inheriting it here is what let step 3 of the
/// review's scenario read "no question on screen" with the menu plainly on it.
fn pointer_rendered(visible: &str) -> bool {
    visible.lines().any(|l| leads_with_pointer(&l.trim().to_lowercase()))
}

/// What [`prompt_wait_detected`] matched, not merely *that* it matched (#534,
/// closing #513(c)/F2).
///
/// Two callers need this and neither could be served by the bare `bool`:
///
/// - **The abort audit.** `delivery-aborted-question` recorded `to`/`stage`/
///   `held_ms`/`recheck_round` — everything except the one thing a diagnosis
///   starts from. The 27-minute live incident on #513 held the orchestrator's
///   inbound queue five times over and its trigger is *still* unknown, because
///   the record never said what the guard keyed on. It does now.
/// - **The grid evidence.** "Is the question still displayed" is only a real
///   question if you can name the question. [`match_still_rendered`] looks for
///   THIS match on the composed screen rather than re-running a detector whose
///   windowing is chronological, not spatial.
///
/// [`prompt_wait_detected`] is defined as `this.is_some()`, so there is exactly
/// one detector and the two can never drift. Which signal is *reported* when
/// several fire is a stable order — structured signals first, prose-like ones
/// last — chosen so the audit names the most diagnostic evidence available;
/// the boolean is a disjunction and is unaffected by the order.
pub fn prompt_wait_match(tail: &str) -> Option<QuestionMatch> {
    let lines: Vec<String> = tail
        .lines()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    let recent = &lines[lines.len().saturating_sub(12)..];

    // The last few non-empty lines — "the last thing the CLI painted". Both
    // prose-like signals (pointer, footer) are read only from here (#40 review):
    // a live menu paints its pointer/footer last, whereas finished-turn prose
    // that happens to lead a line with `❯`/`›`/`→` (a `❯ npm run dev` shell
    // example, a fenced repro block) is followed by the CLI's redrawn idle input
    // box, which pushes it out of this window.
    let last_painted = &recent[recent.len().saturating_sub(3)..];

    // Find the line a token fired on, so the match can name its own evidence.
    // Equivalent to the `recent.join("\n").contains(token)` this replaces —
    // no token spans a newline, so a per-line search matches exactly the same
    // set of tails.
    fn token_in<'a>(lines: &'a [String], tokens: &[&'static str]) -> Option<(&'static str, &'a str)> {
        tokens.iter().find_map(|t| {
            lines.iter().find(|l| l.contains(*t)).map(|l| (*t, l.as_str()))
        })
    }

    // Explicit yes/no confirmation tokens. Reported first: the least
    // ambiguous evidence there is, and the most useful line to see in an audit.
    if let Some((token, line)) = token_in(recent, YES_NO_TOKENS) {
        return Some(QuestionMatch::token("yes-no-token", token, line));
    }
    // Stock permission / trust / continue phrasings from Claude Code & Copilot.
    if let Some((token, line)) = token_in(recent, PERMISSION_PHRASES) {
        return Some(QuestionMatch::token("permission-phrase", token, line));
    }
    // A numbered yes/no menu even without the pointer glyph.
    if let Some((token, line)) = token_in(recent, NUMBERED_MENU_TOKENS) {
        return Some(QuestionMatch::token("numbered-menu", token, line));
    }
    // Interactive selection-menu footer (AskUserQuestion / Copilot / inquirer).
    // Claude Code's AskUserQuestion highlights the active option with reverse
    // video (an ANSI attribute stripped before we see it), so no glyph survives
    // and this footer is the only durable signal (#40). Like the pointer it's
    // read only from the last painted lines. NOTE: matched on single lines, so a
    // footer wrapped across rows in a very narrow pane, or a localized / reworded
    // footer, won't match — a known gap (see design doc).
    //
    // #534 rev-13: reported BEFORE the pointer, which is a change from round 1.
    // Real menus routinely paint both, and when they do the reported match
    // decides which needle the composed-screen re-read gets. The footer's is a
    // literal token; the pointer's is a position, which is strictly harder to
    // re-find and was the round-2 blocking finding. So among two signals that
    // fired on the same dialog, prefer the one whose evidence survives
    // recomposition. This cannot change the boolean — that is a disjunction —
    // only which evidence gets recorded and re-read.
    if let Some((token, line)) = token_in(last_painted, MENU_FOOTER_TOKENS) {
        return Some(QuestionMatch::token("menu-footer", token, line));
    }
    // #903: permission-shaped phrasings that are ALSO ordinary English, read
    // from the last painted lines only — the same two-tier rule the pointer and
    // the footer have always used, applied to the tokens it never covered. A
    // live prompt paints these last; an agent's finished turn writes them in the
    // middle of a paragraph, above the CLI's redrawn box. See
    // `PROSE_PERMISSION_PHRASES` for why exactly these three and not the rest.
    if let Some((token, line)) = token_in(last_painted, PROSE_PERMISSION_PHRASES) {
        return Some(QuestionMatch::token("prose-permission-phrase", token, line));
    }
    // Selection pointer marking the highlighted choice. A `❯`/`›`/`→` that
    // *leads* a line's content (after any box frame) is menu-shaped; the same
    // glyph mid-line is pervasive in ordinary output — pasted shell prompts
    // (`demo ❯ npm run dev`), UI breadcrumbs (`Home › Prefs`), diff/log arrows.
    // Requiring it to lead rules those out; requiring it in the last painted
    // lines also rules out a *leading* glyph in finished prose above the idle box.
    if let Some(line) = last_painted.iter().find(|l| leads_with_pointer(l.as_str())) {
        // A POSITION, not a substring — re-read on the grid by
        // `pointer_rendered`, never by searching for `line` (see the needle's
        // own doc for what happens when that distinction is left implicit).
        return Some(QuestionMatch::new("pointer-option", QuestionNeedle::LeadingPointer, line));
    }
    None
}

/// The literal substrings [`prompt_wait_match`] keys on, hoisted out of the
/// function so the match can report the one that fired and
/// [`match_still_rendered`] can look for that same one on the composed screen.
/// Contents and order are exactly what the inline disjunctions were.
const YES_NO_TOKENS: &[&str] = &["(y/n)", "[y/n]", "y/n)", "[y/n]?"];
const PERMISSION_PHRASES: &[&str] = &[
    "do you want to proceed",
    "do you want to make this edit",
    "do you want to create",
    "do you want to run",
    "do you trust",
    "trust the files",
    "allow this",
    "allow command",
    "grant access",
];
/// #903: the three signals that were **sentences**, not UI structure — moved
/// out of the two wide-window tiers above and read only from the last painted
/// lines.
///
/// The wide tiers are justified by prose-safety ("these don't occur in ordinary
/// prose", per this function's own header) and these three never met that bar:
///
/// - `yes/no` — the spelled-out English phrase, sitting in `YES_NO_TOKENS`
///   beside the punctuated `(y/n)`/`[y/n]` forms that genuinely are structure.
///   "a yes/no question", "a yes/no confirmation" is how anyone writes about
///   one.
/// - `waiting for your` — a bare sentence fragment. In an orchestration pane it
///   is the *normal* thing to read: "waiting for your review", "waiting for your
///   merge decision".
/// - `press enter to continue` — an instruction documentation and agent prose
///   quote constantly; `fp-prose-arrow-keys.txt` has pinned that exact shape as
///   a false positive for the footer tier since #40.
///
/// Demoting costs nothing a real dialog needs: a live one paints its phrase as
/// the last thing on screen (that is what "waiting" means), and a dialog that
/// pushes its title above the window still carries its options — every
/// structured tier above is untouched.
const PROSE_PERMISSION_PHRASES: &[&str] =
    &["yes/no", "waiting for your", "press enter to continue"];
const NUMBERED_MENU_TOKENS: &[&str] = &["1. yes", "❯ 1."];
const MENU_FOOTER_TOKENS: &[&str] =
    &["enter to select", "enter to confirm", "use arrow", "arrow keys", "↑↓", "↑/↓"];

/// How a match can be looked for AGAIN on a composed screen (#534 rev-13).
///
/// An enum rather than the `Option<&'static str>` this replaces, because the
/// round-1 shape encoded "no token" as `None` and left what to do about it to
/// a comment — and the comment was wrong. A signal whose evidence is a
/// position has to be re-read positionally; falling back to the matched *line*
/// made the check a silent no-op (see [`pointer_rendered`]). Spelling the two
/// kinds out means [`match_still_rendered`] must handle each explicitly, and a
/// sixth signal class added later cannot inherit the hole by defaulting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionNeedle {
    /// A literal substring. Short and repaint-stable, so it survives both
    /// re-wrapping at a different width and a partial redraw.
    Token(&'static str),
    /// A pointer glyph LEADING a line — a position, not a substring. Re-read
    /// with [`pointer_rendered`], never by string search.
    LeadingPointer,
}

/// One question-shaped thing [`prompt_wait_match`] found, and where.
///
/// `line` is the detector's OWN normalization of the screen line (trimmed,
/// lowercased) rather than the raw bytes — that is the form the detector
/// reasons in, so it is the form the audit should see. Bounded at
/// [`QuestionMatch::MAX_LINE`] characters: an audit field must not become a
/// channel for an arbitrarily long line a pane happened to paint.
///
/// **`line` is diagnostic, not evidence.** It comes from `strip_ansi` of the
/// byte ring, which deletes cursor addressing, so a redraw-fragmented row can
/// arrive here as a concatenation that was never on screen. It is exactly what
/// an incident reviewer wants to read and exactly what a screen search must
/// not depend on — that dependency is the round-2 blocking finding. Use
/// `needle` for anything that decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionMatch {
    /// Stable kebab-case name of the signal class that fired. This is audit
    /// vocabulary — renaming one silently breaks reading old logs against new.
    pub signal: &'static str,
    /// What to look for when asking whether this is still on screen.
    pub needle: QuestionNeedle,
    /// The normalized line the signal was found on, truncated.
    pub line: String,
}

impl QuestionMatch {
    /// Cap on the retained line. Generous enough to hold any real dialog line
    /// (a wrapped one is already split by the terminal), short enough that a
    /// pane spraying a single enormous line cannot bloat the audit.
    pub const MAX_LINE: usize = 200;

    fn new(signal: &'static str, needle: QuestionNeedle, line: &str) -> Self {
        QuestionMatch { signal, needle, line: line.chars().take(Self::MAX_LINE).collect() }
    }

    pub(in crate::orchestration) fn token(signal: &'static str, token: &'static str, line: &str) -> Self {
        Self::new(signal, QuestionNeedle::Token(token), line)
    }
}

/// Flatten composed rows into one wrap-insensitive haystack: lowercased, every
/// run of whitespace (row breaks included) collapsed to a single space.
///
/// The row break is the whole reason this exists. A dialog line the byte ring
/// saw as one write is TWO rows on a screen narrower than it, so a row-by-row
/// `contains` would miss text that is plainly displayed — and missing it here
/// means concluding "not on screen" and releasing, the one direction this
/// change must never be wrong in. Joining rows makes unrelated neighbours
/// concatenate and can therefore produce a spurious hit; that errs toward
/// "still displayed", which costs a hold the human is already told about.
fn flatten_rendered(visible: &str) -> String {
    visible.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Is the specific thing the byte ring matched still among the pane's RENDERED
/// rows (#534)?
///
/// This is the narrow question, and narrow is the point. It does not ask "is
/// some question on screen" — [`prompt_wait_detected`] answers that, with
/// windowing rules written for a chronological stream. It asks whether *this
/// match*, the one holding *this* delivery, is still displayed. That keeps
/// unrelated screen content — a `❯ npm run dev` in prose the CLI has not
/// scrolled away yet — from either causing or preventing a release.
///
/// The needle is re-read in its OWN terms, and that is the round-2 fix: a
/// `Token` by substring, a `LeadingPointer` by position. Round 1 had only a
/// substring path and fell back to searching for `m.line`, which comes from
/// `strip_ansi` of the byte ring — cursor addressing deleted, so a
/// redraw-fragmented row arrives as a concatenation that was never on screen
/// and can never be found on a clean grid. For `pointer-option`, the only
/// signal with no token, that made this whole check a silent no-op in the
/// false-RELEASE direction. See [`pointer_rendered`].
///
/// The line search is kept as an ADDITIONAL disjunct for both kinds, never the
/// only one: when the line is clean it is the most specific evidence available,
/// and when it is a repaint artifact it simply fails to match — it can add a
/// hold, never remove one.
///
/// A negative here is a genuine "not on this screen"; there is no third answer,
/// because "the composition could not be trusted" is decided before this is
/// called (see [`question_shown`]).
pub fn match_still_rendered(visible: &str, m: &QuestionMatch) -> bool {
    let flat = flatten_rendered(visible);
    let by_needle = match m.needle {
        QuestionNeedle::Token(t) => flat.contains(t),
        QuestionNeedle::LeadingPointer => pointer_rendered(visible),
    };
    if by_needle {
        return true;
    }
    let line = flatten_rendered(&m.line);
    !line.is_empty() && flat.contains(&line)
}

/// Decide whether a freshly spawned CLI is ready to receive typed input,
/// from its output volume and how long that output has been stable. Pure so
/// the thresholds are testable; the polling loop lives in `deliver_now`.
///
/// `output_total` is the pane's MONOTONIC byte counter
/// (`PtyManager::output_total`), and `quiet_for` must be derived from that
/// same counter — never from the output ring's current length (#517). The
/// ring saturates at `OUTPUT_RING_CAP`, where its length stops changing
/// while output keeps arriving (`OutputBuf`'s own doc), so a length-based
/// caller reports a still-booting CLI as quiet and this function then
/// declares it ready. Below saturation the two readings are equal, which is
/// why the bug only showed on CLIs with a large boot paint.
pub fn cli_ready(output_total: usize, quiet_for: Duration, elapsed: Duration) -> bool {
    elapsed >= READY_MIN_WAIT && output_total >= READY_MIN_OUTPUT && quiet_for >= READY_QUIET
}

/// The full readiness test (#1591): [`cli_ready`]'s painted-and-quiet base
/// AND, for a CLI whose [`CliCaps::ready_marker`] row declares one, having
/// SEEN that marker in the pane's output.
///
/// **The base test is unchanged and still required.** A marker is an
/// additional obligation, never a substitute: it says the CLI has finished
/// coming up, and says nothing about whether it is mid-repaint at this
/// instant. Anding them means a marker can only ever DELAY a paste, which is
/// what makes this safe to add to a path whose failure mode is a lost brief —
/// the worst a marker can do is spend the caller's `READY_MAX_WAIT` ceiling,
/// after which the caller pastes anyway exactly as it does today.
///
/// `marker_seen` is decided by the caller FRESH on the tick it is used, and
/// is never latched across ticks (#1591 review N3). An earlier draft carried it
/// forward on the reasoning that "this CLI's servers have connected" is a
/// one-way fact; that is true of the SERVERS and false of the EVIDENCE, and the
/// difference is the whole finding — a decoy that matched once would have
/// released every later tick's paste on a screen that no longer showed
/// anything. Since the marker is read only on a tick where the base test
/// already holds, a positive is consumed by the very next line; there is
/// nothing a latch could have bought.
///
/// Pure, so the composition is assertable without a pty (`cli_ready`'s own
/// reason).
pub fn cli_ready_with_marker(
    output_total: usize,
    quiet_for: Duration,
    elapsed: Duration,
    marker: Option<ReadyMarker>,
    marker_seen: bool,
) -> bool {
    cli_ready(output_total, quiet_for, elapsed) && (marker.is_none() || marker_seen)
}

/// How many raw tail bytes the readiness gate replays to compose a screen
/// (#1591) — [`QUESTION_GRID_REPLAY_BYTES`], reused because it answers the same
/// question (how much history must a blind VT replay eat before the composed
/// grid is the real screen) and re-derived here at THIS loop's cadence rather
/// than inherited (#1591 review D4).
///
/// **The envelope, stated because the constant's own doc argues its size at a
/// different poll rate.** The gate composes a screen only on a tick where the
/// base painted-and-quiet test already holds, so the first possible
/// composition is at `READY_MIN_WAIT` + `READY_QUIET` and the last at
/// `READY_MAX_WAIT`: at `READY_POLL` = 250 ms that is at most
/// `(25_000 - 2_700) / 250` ≈ **89 compositions**, each one tail copy of up to
/// 64 KiB plus one linear replay and one `size()` read. Worst case ≈ 5.7 MB of
/// copying spread over 25 s, per pane, and only on a pane whose marker never
/// arrives — i.e. it coincides with something already being wrong, and it
/// multiplies by the number of agents spawning at once.
///
/// That is judged affordable at this cadence for the same reason the question
/// guard affords it at its own: the work is a linear scan with no history
/// retained, it is bounded by `READY_MAX_WAIT` rather than open-ended, and it
/// buys the one reading that can see a cursor-positioned footer at all. If
/// `READY_MAX_WAIT` or `READY_POLL` ever move, this paragraph is the thing to
/// re-derive.
pub(in crate::orchestration) const READY_GRID_REPLAY_BYTES: usize = QUESTION_GRID_REPLAY_BYTES;

/// Compose the screen the readiness marker is looked for in (#1591) — the pure
/// half of `deliver_now`'s sampler, split out so all three of its branches are
/// assertable without a pty (#1591 review D1).
///
/// The split is at this boundary and not one level up on purpose: `PtyManager`
/// has no geometry test hook, so a helper taking `&PtyManager` would see
/// `size() == None` in every test and only ever exercise the fallback — which
/// is precisely the branch this function exists to stop being the only one
/// anybody has run. Same reason `cli_ready` was extracted from the loop.
///
/// **Preferred reading: the rendered rows.** A status footer is the most
/// cursor-positioned region of a TUI, and the byte ring cannot say where a
/// repaint put the count relative to its label. Composed exactly the way
/// `question_sample` composes one (#534) — same replay, same
/// [`trustworthy_composition`] gate.
///
/// **Fallback: the ANSI-stripped ring**, taken on EITHER of two triggers, both
/// named because the surfaces used to state only the first (#1591 review D3):
///
/// 1. the pane reports no geometry, or
/// 2. the replay composed fewer than `GRID_MIN_RENDERED_ROWS` non-empty rows,
///    so [`trustworthy_composition`] declined it.
///
/// **This is a deliberate DIVERGENCE from `question_sample`, not a copy of
/// it.** That guard refuses to substitute — "no size, no grid evidence" — and
/// its [`QuestionSample`] doc says the two readings "must not collapse, because
/// one licenses a release and the other must not". The asymmetry that makes
/// collapsing them right here and wrong there is the direction of the
/// consequence: the question guard's grid evidence RELEASES a hold, so a
/// substituted reading could let a delivery land on a live dialog. This
/// reading only ever un-blocks a paste that the base painted-and-quiet test has
/// ALREADY approved, and refusing to substitute would price every
/// geometry-less pane at `READY_MAX_WAIT` on every kickoff. The trade is a
/// wider decoy surface (the ring carries scrolled-off history the screen does
/// not) for not being inert; the ring slice is therefore narrowed to
/// [`QUESTION_SCAN_TAIL_BYTES`], so the fallback never reads MORE history than
/// the pre-#1591 reading it replaces.
pub fn ready_screen(raw: &[u8], size: Option<(u16, u16)>) -> String {
    size.and_then(|(cols, rows)| trustworthy_composition(termgrid::render_visible(raw, cols, rows)))
        .unwrap_or_else(|| {
            let from = raw.len().saturating_sub(QUESTION_SCAN_TAIL_BYTES);
            strip_ansi(&raw[from..])
        })
}

/// How the fresh-boot readiness wait ended (#517).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyWait {
    /// The CLI was observed painted and quiet — safe to paste.
    Ready,
    /// `READY_MAX_WAIT` expired with the CLI still not looking ready. The
    /// caller pastes anyway (a visible prompt the human can re-submit beats
    /// one silently withheld), but this is a materially different state from
    /// `Ready` and is audited as such.
    TimedOut,
    /// The pty went away while waiting.
    PaneClosed,
}

/// Hold a fresh boot's paste until the CLI has painted its UI and gone quiet
/// — the loop `deliver_now` runs before a kickoff, with its clock and its
/// output sampler injected so the loop itself is testable without a live
/// pty or a real `AppHandle`.
///
/// **`sample_total` MUST be `PtyManager::output_total`, never the length of
/// `output_tail` (#517).** That was the bug: `OutputBuf`'s own doc says the
/// monotonic counter exists precisely because the ring saturates at
/// `OUTPUT_RING_CAP` (256 KB), "where lengths stop changing". A CLI whose
/// boot output exceeds the cap — a TUI repainting a splash while it loads
/// config and MCP servers — froze the length at the moment of saturation, so
/// the "output has been stable for `READY_QUIET`" test passed while the CLI
/// was still painting and its stdin reader still unattached. loomux then
/// pasted the kickoff into a startup buffer nobody was reading, and the
/// brief was swallowed with no trace in the box for anything downstream to
/// recover. Every other progress loop in this file (the echo check, the
/// submit-quiet wait, the late monitor) already reads the counter; this one
/// was the outlier. Below saturation the two readings are identical, which
/// is why this only ever bit CLIs with a large boot paint — and why it read
/// as intermittent rather than deterministic.
///
/// `poll_tick` performs one `READY_POLL` wait; `elapsed` reports time since
/// the wait began (both real in production, synthetic in tests).
///
/// **`marker`** is the target CLI's [`CliCaps::ready_marker`] (#1591) — an
/// extra obligation on top of the base test for a CLI that paints its UI
/// before it can read. `None` (every CLI but opencode) makes this loop behave
/// exactly as it did before that issue, byte for byte.
///
/// `sample_screen` supplies the pane's RENDERED rows — a VT replay, not the
/// ANSI-stripped byte ring (see [`ReadyMarker::matches`] for why a footer's
/// count and its label need never be adjacent in the stream). It is called
/// ONLY on a tick where the base test already holds, so a `None` row never
/// composes a screen at all, and a marked one pays only across the window
/// between "painted and quiet" and "actually up" — precisely the window this
/// exists to cover.
///
/// **Nothing is latched** (#1591 review N3). The marker is re-read on every
/// tick that reaches it, so a decoy that satisfied it once cannot release a
/// later paste, and a negative is never cached either.
///
/// `READY_MAX_WAIT` is untouched as the ceiling, and that is the whole safety
/// argument: a CLI that renames its footer, or one whose marker never reaches
/// the read window, waits out the ceiling and is pasted into blind — the
/// pre-#1591 behaviour for an unrecognised boot, audited as `TimedOut` rather
/// than `Ready`. The gate fails toward a slow delivery, never toward a lost
/// one.
pub fn await_cli_ready(
    marker: Option<ReadyMarker>,
    mut sample_total: impl FnMut() -> Option<u64>,
    mut sample_screen: impl FnMut() -> Option<String>,
    mut poll_tick: impl FnMut(),
    mut elapsed: impl FnMut() -> Duration,
) -> ReadyWait {
    let mut last_total = 0u64;
    let mut last_change = Duration::ZERO;
    loop {
        poll_tick();
        let Some(total) = sample_total() else { return ReadyWait::PaneClosed };
        let now = elapsed();
        if total != last_total {
            last_total = total;
            last_change = now;
        }
        let quiet_for = now.saturating_sub(last_change);
        let base = cli_ready(last_total as usize, quiet_for, now);
        // Compose the screen only when the base test is otherwise satisfied.
        // Not an optimisation dressed as a rule: a marker cannot rescue a pane
        // that is still painting (the `&&` below would reject it anyway), so
        // reading earlier would buy nothing and would charge every ordinary
        // delivery for a VT replay it has never needed. The short-circuit is
        // also what keeps the read FRESH rather than latched — see the doc.
        let marker_seen = match marker {
            None => true,
            Some(m) => base && sample_screen().is_some_and(|screen| m.matches(&screen)),
        };
        if cli_ready_with_marker(last_total as usize, quiet_for, now, marker, marker_seen) {
            return ReadyWait::Ready;
        }
        if now >= READY_MAX_WAIT {
            return ReadyWait::TimedOut;
        }
    }
}
