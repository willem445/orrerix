//! Masking the pane's own echo: the pasted prompt and orrerix's own notices
//! are masked out of the screen before it is read, from the delivered-notice
//! and delivered-prompt records.
//! Design note: `docs/design/question-gate-authorship.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): `OrchRegistry`.
//! Sibling files it calls: `screen.rs`.

use super::*;

/// Remove every line of `tail` that exactly matches one of `pasted_text`'s
/// own lines (rev-19 B-A). This is the self-echo exclusion for the
/// interactive-question guard — CONTENT-based, not byte-COUNT-based. A round
/// that gated on "has the pane's output total grown since a baseline
/// snapshot" was proven broken twice: the baseline is always ONE number, so
/// it can only mark a single point in time as "before" — a dialog that
/// renders WHILE the paste is still echoing (the canonical #420 timeline:
/// loomux pastes, Copilot processes it, paints a dialog, all before loomux's
/// own settle-wait finishes) gets baked into whichever snapshot the
/// checkpoint happened to take, and reads as "no growth" — invisible —
/// forever after. No amount of moving WHEN the snapshot is taken closes that
/// gap, because delta-vs-one-number can't distinguish "still my own paste
/// settling" from "their dialog, appeared in the same window" — both are
/// just "more bytes since X." Content can: `deliver_prompt` knows EXACTLY
/// what it pasted. Masking out only the lines that are OUR OWN text leaves
/// whatever the CLI itself painted — including a
/// dialog that rendered mid-paste — fully visible to the detector; a paste
/// whose own text happens to contain "(y/n)" or "do you want to run" masks
/// itself away to nothing, because every line of it IS a pasted line.
///
/// **#820: a CLI does not render our paste as our lines, and comparing whole
/// lines for equality is what made this mask miss.** The original rule was
/// `row.trim().to_lowercase() == pasted_line`, and every way a CLI paints our
/// text defeats it: copilot 1.0.7x prefixes its fallback composer row with
/// `❯ ` (U+276F — a [`POINTER_GLYPHS`] member), draws a `┃ ` border down
/// *every* row its framed composer wrapped our line onto, and echoes the
/// submitted prompt back into its transcript as `❯ <text>` with a right-aligned
/// `HH:mm`. Any one of those leaves a row of OUR text in front of the detector,
/// and it then takes only one such row leading with `❯`/`›`/`→` — which the
/// composer prefix supplies outright, and a wrap boundary supplies whenever a
/// brief's own prose wraps onto one (`red → green`, `main → beta10`) — for
/// `pointer-option` to fire on loomux's own prompt. Nothing repaints an
/// unsubmitted box, so the ring stays frozen on it and
/// [`pointer_rendered`] finds the same glyph among the rendered rows: the #727
/// latch exactly, re-entered through our own paste, and neither reading can
/// ever release it.
///
/// So the comparison is made against the row a CLI actually paints:
///
/// - **De-framed**, so a bordered composer (`┃ our text`) is compared on its
///   content — the same [`deframe`] every other reading here already uses.
/// - **Wrap-reconstructed**, so one pasted line spread over several rows is
///   claimed as one run, via the same [`reconstructs_to_end`] discipline
///   [`mask_loomux_notices_with_record`] uses: contiguous, in order, from the
///   line's own start, and to its END rather than a prefix. A run that does not
///   account for the whole line claims nothing, so the failure direction is
///   under-masking — a hold that stands, which is the cheap error.
/// - **Pointer-stripped, but only as a SECOND attempt and only above an
///   evidence floor.** See [`SELF_ECHO_MIN_POINTER_CHARS`]: a box border is
///   decoration by construction, whereas a pointer glyph is the entire
///   `pointer-option` signal, so stripping one is a claim that needs paying for.
///
/// What does NOT change is the reason this mask is allowed to be greedy about
/// our own text at all: `deliver_prompt` knows EXACTLY what it pasted. A paste
/// whose own text happens to contain `(y/n)` still masks itself away to
/// nothing, because every line of it IS a pasted line, while a dialog the CLI
/// painted mid-paste stays fully visible — that was always the point of a
/// CONTENT-based exclusion, and it is what a byte-COUNT baseline could never do
/// (the history above). #820 changes only HOW a row is recognised as ours,
/// never WHICH text counts as ours.
pub fn mask_own_paste(tail: &str, pasted_text: &str) -> String {
    let rows: Vec<&str> = tail.lines().collect();
    let mut norm: Vec<String> = rows.iter().map(|r| wrap_normalize(deframe(r))).collect();
    // The same rows read once more with a leading pointer glyph removed. Kept
    // beside the ordinary reading rather than replacing it, so the plain
    // comparison is always tried first and the pointer strip can only ever
    // claim MORE — never differently.
    let unpointed: Vec<Option<String>> =
        rows.iter().map(|r| strip_leading_pointer(r).map(wrap_normalize)).collect();
    let pasted: Vec<String> =
        pasted_text.lines().map(wrap_normalize).filter(|l| !l.is_empty()).collect();
    let mut keep = vec![true; rows.len()];
    let mut i = 0;
    while i < rows.len() {
        if norm[i].is_empty() {
            i += 1;
            continue;
        }
        let mut claimed = pasted.iter().find_map(|line| reconstructs_to_end(&norm, i, line, 0));
        if claimed.is_none() {
            if let Some(head) = unpointed[i].clone() {
                // Only this row is re-read without its glyph; the continuation
                // rows of the same wrap run stay on the ordinary reading,
                // because only the FIRST row of a composer's line carries the
                // prompt chevron.
                let framed = std::mem::replace(&mut norm[i], head);
                // #820 residual, the human's case: a SHORT steer. The floor
                // refuses to claim `❯ merge` (5 chars) because `❯ Overwrite` is
                // byte-identical whether it is our composer holding a one-word
                // paste or a live dialog's highlighted choice — and the row
                // cannot tell the difference. Its NEIGHBORHOOD can: every real
                // dialog's option list is headed by the question it is asking —
                // g4's `? Overwrite the existing file?`, Copilot's
                // `● Allow Copilot to run …?` and `● Which retry strategy …?`,
                // Claude's boxed `Which authentication …?` — and a composer
                // never paints one: its chrome above the input row is a
                // divider, the cwd, and `● Ready.`. So the floor is waived
                // exactly when no such question row heads the block above: a
                // short pointer row that is byte-for-byte one of our pasted
                // lines, under no dialog question, is our own composer, and
                // masking it cannot release an Enter into a dialog — there is
                // none there to select into. `dialog_header_above` is the
                // shape-tracking scan (see its doc); the one captured dialog
                // with no question row, the Claude MCP approval's prose, is
                // kept a question by its confirm footer — see g8's h3.
                let own_short_composer = !dialog_header_above(&rows, &norm, &keep, i);
                claimed = pasted
                    .iter()
                    .filter(|line| {
                        line.chars().count() >= SELF_ECHO_MIN_POINTER_CHARS || own_short_composer
                    })
                    .find_map(|line| reconstructs_to_end(&norm, i, line, 0));
                if claimed.is_none() {
                    norm[i] = framed;
                }
            }
        }
        // #871/#903: the CLI COLLAPSED this paste rather than echoing it, so
        // there is no row here that IS our text and every reading above has
        // nothing to match. Claude Code replaces a multi-line paste with a
        // placeholder of its own (`[Pasted text #1 +6 lines]` on the live
        // incident), and this file already knows that happens — the Tier 1
        // precondition declines to govern for exactly this reason. The composer
        // reading never learned it, which is why `idle_row` flips true to FALSE
        // between the pre-paste checkpoint and the pre-Enter one on an unchanged
        // screen, and the Enter is then withheld from a pane that is visibly
        // idle: the delivery aborts with the paste stranded in the box, and every
        // later delivery queues behind it.
        //
        // **The evidence this claims on is the CLI's, not a shape we invented.**
        // A pane that echoed our bytes into a free-text composer is a pane that
        // was not showing a modal — a dialog does not take a paste and render a
        // placeholder for it. What the row proves is authorship, which is all
        // this mask ever claims.
        //
        // **Two terms, both narrowing.** The paste must be MULTI-LINE, because a
        // single line is not one a CLI collapses, so a placeholder beside one is
        // not ours (a long single-line paste that some CLI collapses anyway is a
        // stated residual: it fails to match and the gate holds). And no dialog
        // question row may head the block above, the same term every record
        // claim now carries.
        //
        // Deliberately NOT keyed on the placeholder's `+N lines` count: the
        // exact text is the CLI's to change and this repo has no citable
        // specification of its arithmetic, so pinning one would be a guess
        // dressed as a check. The shape plus the multi-line term is what is
        // actually known.
        if claimed.is_none()
            && pasted.len() > 1
            && collapsed_paste_row(rows[i])
            && !dialog_header_above(&rows, &norm, &keep, i)
        {
            claimed = Some(i + 1);
        }
        match claimed {
            Some(end) => {
                keep[i..end].fill(false);
                i = end;
            }
            None => i += 1,
        }
    }
    rows.iter().zip(keep).filter_map(|(r, k)| k.then_some(*r)).collect::<Vec<_>>().join("\n")
}

/// This row with a leading menu-pointer glyph removed, after any box framing —
/// or `None` where it does not lead with one (#820).
///
/// Deliberately NOT folded into [`deframe`]. `deframe`'s strip set is
/// decoration: a border, a bullet, an indent, none of which any detector rule
/// keys on. A pointer glyph is the opposite — it *is* [`leads_with_pointer`]'s
/// whole signal — so a reading that stripped it globally would delete the
/// `pointer-option` signal outright rather than narrowing it. It is removed in
/// exactly one place, for exactly one question: *is this row our own text with
/// the CLI's prompt chevron in front of it.*
fn strip_leading_pointer(line: &str) -> Option<&str> {
    let d = deframe(line);
    POINTER_GLYPHS.iter().find_map(|g| d.strip_prefix(*g))
}
/// Is this rendered row a CLI's own placeholder for a paste it COLLAPSED
/// instead of echoing (#871/#903)?
///
/// Shape only, and loosely: a bracketed row that names a pasted text. The
/// bracket pair is what keeps it from matching prose — a CLI's placeholder is a
/// self-contained token on the composer's line, not a clause inside a sentence
/// — and the wording is matched case-insensitively and without reading the
/// `#N`/`+M lines` parts, because those are the CLI's to change and there is no
/// citable specification of them to pin. See the call site in
/// [`mask_own_paste`] for the terms that do the narrowing; this predicate is
/// deliberately not one of them.
///
/// A leading pointer glyph is stripped first: Claude Code's composer paints
/// `❯ [Pasted text #1 +6 lines]`, and it is that whole row — chevron included —
/// that must stop being read as a pointer at a menu option.
fn collapsed_paste_row(row: &str) -> bool {
    let inner = strip_leading_pointer(row).unwrap_or_else(|| deframe(row));
    let inner = inner.trim().trim_end_matches(is_frame_char);
    inner.starts_with('[')
        && inner.ends_with(']')
        && inner.to_lowercase().contains("pasted text")
}

/// How long a pasted line must be before [`mask_own_paste`] will claim a row
/// on the strength of a POINTER-stripped comparison (#820).
///
/// The floor exists because the two things being told apart can be
/// byte-identical. `❯ Overwrite` is our own composer holding a one-word paste,
/// and it is also a live dialog's highlighted choice; the only evidence
/// separating them is that we know what we pasted, and a short line is weak
/// evidence — a brief containing a bare `Yes` line would otherwise let
/// `❯ Yes` be masked out of a genuine permission dialog, which releases an
/// Enter into it. That is the #420 harm, and over-masking is the one direction
/// this file never chooses.
///
/// 24 characters, the same figure and the same argument as
/// [`R_TOP_MIN_ANCHOR_CHARS`]: it clears every stock menu option a CLI paints
/// (`Yes`, `No`, `Overwrite`, `Keep both`, `1. Yes`) while sitting far below
/// any line of an orchestrator brief or a steer. The cost is borne only by
/// short claims that were never good evidence.
///
/// **Stated residual:** a delivery whose every line is shorter than this, sitting
/// in a chevron composer, is still not masked and can still latch the gate. The
/// pointer strip is what answers the shape #820 reported; the floor is what
/// keeps that from being a hole, and buying the last case would need evidence
/// this mask does not have. The `matched` field added to
/// `delivery-held-in-queue` in the same change is what makes such a hold name
/// itself.
const SELF_ECHO_MIN_POINTER_CHARS: usize = 24;

/// Does this row look like a live dialog's question line?
///
/// `row` is expected ALREADY reduced — a `norm` entry ([`deframe`] +
/// [`wrap_normalize`]) — because the captured dialog fixtures head their
/// option lists four ways and that reduction is what unifies them: g4's
/// `? Overwrite the existing file?` (opens and closes with the glyph),
/// Copilot's `●`-bulleted `Allow Copilot to run the following command?` and
/// `Which retry strategy …?` (the bullet deframes away), and Claude's boxed
/// `Which authentication approach …?`. Every one of those is a row ending in
/// `?`; the trailing `trim_end_matches(is_frame_char)` additionally unwraps a
/// box that CLOSES its border (`│ Which …? │`) — the captured Claude fixture
/// happens not to paint one. A bare `?`, or a row that is only the glyph, has
/// no words and is not a question. The one captured dialog that asks no
/// question, the Claude MCP approval's prose preamble, has nothing here to
/// detect and is kept a question by its confirm footer instead. Used only as
/// a veto in [`mask_own_paste`], so the bar is set on the side of *seeing* a
/// dialog: a row that merely resembles a prompt keeps the pointer row below
/// it unmasked, which errs into a hold we can release, never into an Enter
/// into a live dialog.
fn is_dialog_header(row: &str) -> bool {
    let row = row.trim_end_matches(is_frame_char);
    let words = row.trim_matches('?').trim();
    !words.is_empty() && (row.starts_with('?') || row.ends_with('?'))
}

/// Is this RAW row part of a dialog's option block rather than its head?
///
/// The upward scan in [`dialog_header_above`] steps over exactly the rows a
/// dialog interposes between its question and the highlighted choice: blank
/// rows, sibling options (a row leading with a pointer glyph, or a box-framed
/// row still indented once the frame is peeled), and indented `$ command`
/// context under Copilot's command dialogs. [`deframe`] would not serve here —
/// it eats indentation, and indentation is the evidence — so the peel set is
/// only the box glyphs and pointers, leaving leading whitespace in place. A
/// row that is none of these ends the block and, once tested as a question,
/// stops the scan.
fn option_block_row(raw: &str) -> bool {
    if raw.trim().is_empty() {
        return true;
    }
    let peeled = raw.trim_start_matches(|c| matches!(c, '│' | '┃' | '|' | '❯' | '›' | '→'));
    peeled.starts_with(char::is_whitespace)
}

/// Is a live dialog's question row in the block above the pointer row at
/// `from`?
///
/// Not a fixed window — a row-count budget would only be calibrated against
/// the captured fixtures, where the pointer sits on the FIRST option and the
/// question is one row away. Real dialogs violate both: arrowing down moves
/// `❯` through the option list, a two-line `$ command` block or a wrapped
/// question adds rows, and Copilot pads with blanks. The scan instead follows
/// the dialog's actual shape: step upward from the pointer, vetoing on the
/// first question row, stepping over the option block ([`option_block_row`]:
/// blanks, sibling options, indented command rows), and stopping at the first
/// row that is neither a question nor option block — a composer's divider or
/// cwd, a dialog's prose preamble. Rows already claimed as our own text
/// (`!keep[j]`) are skipped outright — a row proven to be our paste cannot be
/// a dialog's question, and Copilot echoes a submitted prompt into its
/// transcript as `❯ <text>`, the exact shape the waiver is for.
///
/// The evidence is NEGATIVE — absence of a dialog, not presence of a composer
/// — the one place in this file a claim is bought without paying with content.
/// It is accepted only because every real dialog's question row ends in `?`
/// and the composer chrome above its input row (a divider, the cwd,
/// `● Ready.`) never does, and because the cost of an error is a hold, not a
/// release. Residuals: a command row the CLI does NOT indent under the question
/// stops the scan as if it were chrome — a gap for the rare dialog that paints
/// one unindented (Copilot and Claude both indent theirs); a headerless
/// composer with no divider above its input row, holding agent prose that ends
/// in `?`, still vetoes a short steer — the #820 hold, best-effort; and the
/// ring read is emission order, so an option-block repaint that omits the
/// question line reads headerless until the next full repaint. Another: for
/// this scan, [`option_block_row`]'s peel set is narrower than [`deframe`]'s —
/// it strips only box glyphs and pointers, not the bullets (`*`, `●`, `•`,
/// `◆`) `deframe` also treats as decoration — so a dialog that BULLETS its
/// sibling options instead of indenting them stops the scan before the
/// question, and the waiver then engages on a live highlighted choice; no
/// captured fixture does this, so it is speculative rather than demonstrated.
/// This is unexercised scope, not a protective tradeoff: a composer's divider
/// row (`──…`, U+2500) is peeled by NEITHER set — it carries none of
/// `option_block_row`'s glyphs, none of `deframe`'s bullets — so widening the
/// peel set to match `deframe`'s would not touch g9 h3's divider protection at
/// all; the two are unrelated. The trust model is unchanged from the 24-char
/// floor (an agent that knows the pasted text can paint a header-less row to
/// induce masking); this does not widen it.
fn dialog_header_above(rows: &[&str], norm: &[String], keep: &[bool], from: usize) -> bool {
    let mut j = from;
    while j > 0 {
        j -= 1;
        // #903 B2: the header test runs BEFORE the `keep` check, and the order is
        // the whole finding. Testing `keep` first let a claimed row be stepped
        // over — so two recorded lines were enough to walk this scan past the
        // very question row it vetoes on: claim the dialog's question row with
        // the first, and the second's option row then reads as having no header
        // above it, masks, and the gate releases an Enter into a live dialog.
        //
        // "Is a dialog's question row above this one" is a fact about the
        // SCREEN, and consulting the mask to answer it let the mask decide its
        // own bound. A header now vetoes whether or not its row was claimed,
        // which is a term the record cannot buy at any number of claims.
        if is_dialog_header(&norm[j]) {
            return true;
        }
        // Claimed NON-header rows are still stepped over, unchanged: a loomux
        // notice interleaved above an option block must not end the scan, which
        // is what this clause was for before it was asked to do more.
        if !keep[j] {
            continue;
        }
        if option_block_row(rows[j]) {
            continue;
        }
        return false;
    }
    false
}

/// Remove the rows loomux itself wrote into this pane before the question
/// detector ever sees them (#576).
///
/// **The bug.** `prompt_wait_detected` asks "does this pane look parked on a
/// question", and loomux's own notices are text *about* questions: a relayed
/// `report` note or `message_orchestrator` text lands as
/// `[orrerix] w-7 reports blocked: Copilot asked "do you want to run npm test?
/// (y/n)"`. That satisfies two of the detector's structured signals, so the
/// gate latches — and because a held pane emits nothing new, no fresh output
/// ever pushes it back out of the scan window. An orchestrator pane is the most
/// exposed, since relayed worker prose is most of what gets written to it.
///
/// #534 does not cover this and could not: it lets the *grid* release a hold
/// the ring is still asserting, but our own text is genuinely **rendered**, so
/// both readings agree it is on screen. They are right — it is on screen. It is
/// simply not a question, and only the marker can say so.
///
/// **Exactly one row per marker, and never the rows around it.** The tempting
/// version masks the marker's whole wrap-run: a notice is one logical line
/// (`truncate_notice` strips control characters, so it cannot contain a
/// newline), a terminal wraps one logical line into a contiguous run of
/// non-blank rows, and only the first row carries the marker — so a run-mask
/// would also catch a `(y/n)` that wrapped onto row two. It is rejected because
/// the marker cannot support it. Since an agent can print a marker row itself
/// (see [`NOTICE_MARKER`]), a run-mask hands any pane the power to
/// delete the seven rows below an attacker-chosen row — and a genuine
/// permission dialog painted there would be masked into "no question", which
/// releases an Enter into it. That is the #420 harm, reachable from pane
/// output. Failing OPEN is the dangerous direction, so the mask claims only
/// the row the marker actually leads.
///
/// **A multi-row notice is the PRODUCER's problem, never this function's
/// (#632).** The rule above says one row per marker, so loomux text occupying
/// several rows is only fully maskable if every row it emits leads with the
/// marker once `deframe`d — which is a shape each producer owes, not a claim
/// this mask can widen to cover for them. Both directions of that are load
/// bearing: a producer that skips it hands the detector a row of loomux prose
/// (the #632 bug), and a mask that compensated by taking neighbouring rows
/// would be the run-mask rejected two paragraphs up. See
/// [`unmaskable_framing_rows`] for the shared invariant the producers assert
/// themselves against, and `deframe`'s strip set for why the bullet in a
/// continuation row must be `•` and never `-`.
///
/// **What the marker alone leaves, and what closes it (#576 residual).** A
/// notice that wraps keeps whatever tokens landed past the first row; a marker
/// row that has itself scrolled off leaves its continuation unmarked. Both are
/// under-masks — the gate holds when it might have cleared, which is the cheap
/// error the ten-minute `QuestionStale` badge already covers. Closing them
/// needs loomux to know *what* it wrote to a pane, not merely that a row claims
/// it did: that is [`DeliveredNotices`], and the mask that consults it is
/// [`mask_loomux_notices_with_record`]. THIS function is the record-free form —
/// the marker rule and nothing else — and it stays, because a pane whose record
/// was lost (a restart) and a producer asserting its own maskability at its
/// door ([`unmaskable_framing_rows`]) must both be answered by the marker rule
/// alone. See the record-aware function for why the record cannot widen a
/// producer's door.
///
/// The residual false-release surface is correspondingly one row wide: a pane
/// would have to paint a row that both leads with the marker and *is itself*
/// the live question. A CLI paints its dialog rows itself and does not prefix
/// them with our marker, and an agent printing the whole thing has not rendered
/// a dialog at all — it has printed prose about one.
pub fn mask_loomux_notices(tail: &str) -> String {
    mask_loomux_notices_with_record(tail, &[])
}

/// Does this row LEAD with the notice marker, after any box framing?
///
/// The single definition of the marker rule (the `leads_with_pointer`
/// precedent): [`mask_loomux_notices_with_record`] and
/// [`loomux_authored_lines`] both call it, so what the record REMEMBERS and
/// what the mask CLAIMS cannot drift apart — a record holding lines the mask
/// would not have recognised is a record that widens nothing, and the reverse
/// is a mask reaching for lines that were never kept.
///
/// De-framed so a notice echoed inside a box UI (`│ [orrerix] …`) is still seen
/// to LEAD its row — the same rule, and the same reason, as
/// `leads_with_pointer`. Lowercasing is [`brand::leading_notice_marker`]'s own
/// contract, so a re-cased echo still matches.
///
/// **Every accepted spelling, not just today's** (#1153 phase 3). A pane's
/// scrollback is written once and read for as long as the pane lives: rows an
/// agent captured before the rename lead with the legacy marker, and a mask
/// that stopped recognising them would quietly start leaking pre-rename
/// notices into the very run-mask this function exists to feed.
fn leads_with_notice_marker(line: &str) -> bool {
    brand::leading_notice_marker(deframe(line)).is_some()
}

/// One row (or one recorded line) reduced to what a wrap cannot change:
/// case-folded, with every whitespace run collapsed to a single space and the
/// ends trimmed.
///
/// A terminal wrapping one logical line re-distributes it across rows, and a
/// CLI re-rendering it may re-indent the continuations; neither alters the
/// sequence of words. Comparing on this normal form is therefore what lets a
/// run of rows be checked against the line it came from without the check
/// depending on the pane's width — which the mask deliberately does not know
/// (see [`mask_loomux_notices_with_record`]).
fn wrap_normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Do `rows[from..]` reconstruct `line` from byte offset `at` all the way to
/// its END, one row at a time?
///
/// Returns the row index just past the run, or `None` if the rows do not
/// account for the whole remainder. Both halves are load bearing:
///
/// - **Contiguous, in order, from the anchor.** Each row must continue exactly
///   where the previous one stopped, optionally across the single space a word
///   wrap eats at the boundary (a hard wrap mid-word eats nothing, so the space
///   is optional rather than required). A blank row ends the run: a wrapped
///   logical line has none.
/// - **To the end, never a prefix.** A run that merely *starts* a recorded line
///   proves nothing about the rows after it, and claiming them would be the
///   run-mask [`mask_loomux_notices`] rejects. Requiring the remainder to be
///   consumed exactly means the last claimed row is the line's last row, so
///   whatever a pane painted below it is untouched. The direction of the error
///   is right too: a notice the CLI truncated with an ellipsis, or one cut off
///   by the bottom of the reading, simply fails to reconstruct and the gate
///   keeps holding.
fn reconstructs_to_end(rows: &[String], from: usize, line: &str, at: usize) -> Option<usize> {
    let mut rest = &line[at..];
    let mut i = from;
    while i < rows.len() && !rest.is_empty() {
        let row = rows[i].as_str();
        if row.is_empty() {
            break;
        }
        let candidate = rest.strip_prefix(' ').unwrap_or(rest);
        match candidate.strip_prefix(row) {
            Some(next) => {
                rest = next;
                i += 1;
            }
            None => break,
        }
    }
    (rest.is_empty() && i > from).then_some(i)
}

/// [`mask_loomux_notices`], plus the rows a per-pane record of delivered text
/// proves loomux wrote (#576 — the wrap residual and the scrolled-off marker).
///
/// **What `delivered` is, and why it is the only thing allowed to widen the
/// mask.** It is the marker-led lines loomux has actually written into THIS
/// pane ([`DeliveredNotices`]) that a producer has ALSO promised the pane's own
/// agent did not author any span of ([`OrchRegistry::mark_notice_maskable`]).
/// A pane cannot add to it: the record is written on the delivery side, from
/// the text loomux pasted, so pane output — the direction
/// [`NOTICE_MARKER`] is forgeable in — cannot reach it. That is the
/// property #576 asked for and the reason the widening below is keyed off the
/// record and never off the marker: **an agent-printed marker row still widens
/// nothing**, because a row that merely looks like a notice matches no recorded
/// line (#420's harm, restated as the rule this function obeys).
///
/// The second half of that — the producer's promise — is what stops the
/// unforgeability being hollow: a line loomux wrote can still be a line an
/// agent CHOSE, and the record must not hand an agent the mask over its own
/// pane. See the residual note at the bottom for the review finding that
/// established this.
///
/// **#903 widened what `delivered` may contain, and narrowed what a claim
/// costs.** It now also carries the PROMPT bodies loomux delivered into this
/// pane's CLI **session** ([`DeliveredPrompts`], unioned in by
/// [`OrchRegistry::delivered_mask_lines`]), because the rows this mask could
/// not claim on a resumed pane were loomux's own previous deliveries replayed
/// by the CLI — not notices at all. Two things keep the widening from being a
/// weakening:
///
/// - **Admission is by provenance and excludes notice text.**
///   [`delivered_prompt_lines`] drops every marker-led line, so the one-party
///   route [`OrchRegistry::mark_notice_maskable`] documents — an agent putting a
///   line of its own choosing into its own pane's record via
///   `notify_when(note:)` — stays exactly as closed as it is today. What is
///   admitted is a kickoff brief or an orchestrator's `send_prompt` body, and no
///   agent can address one of those to itself — and that is now ENFORCED rather
///   than observed: `send_prompt` refuses `a.id == caller.agent_id` outright
///   ("cannot send a prompt to yourself"), and a kickoff's target is an agent
///   `spawn_agent` has just created, which the caller cannot be. The two routes
///   that DID let an agent reach its own pane — the post-compact re-grounding
///   notice and `resume_kickoff_notice`, both of which paste the agent's own
///   directive ledger — are refused by [`prompt_record_admits_kind`] and by
///   [`delivered_prompt_lines`]'s first-line rule respectively.
/// - **Every record claim now owes `dialog_header_above`**, not only the
///   pointer-stripped ones. That is a strictly smaller set of claims than the
///   pre-#903 rule made, so the widening cannot reach a row the old rule would
///   have refused.
///
/// The residual that remains — an orchestrator delivering rows that coincide
/// with a real dialog's, a dialog with no question row of its own, a header
/// whose text the same party controls — is argued in
/// `docs/design/question-gate-authorship.md` and is bounded, not closed.
///
/// **Two rules, both anchored in the record:**
///
/// - *A wrapped notice (`R-wrap`).* A run of rows starting at a row that is the
///   START of a recorded line and reconstructing that line to its end is
///   claimed whole. The anchor is the recorded line's own beginning, so the
///   marker plays no part in the decision — a recorded line leads with the
///   marker by construction, which makes its first row one
///   [`mask_loomux_notices`] would have claimed anyway; the new part is only
///   the continuation, and only where the continuation *is* the rest of that
///   line.
/// - *A marker that scrolled off (`R-top`).* Every reading here is truncated at
///   the TOP (the byte ring keeps the last bytes; the grid keeps the last
///   rows), so the one place a line can legitimately appear headless is the
///   first non-empty row of the reading. There, and only there, a run may
///   anchor MID-line — at any offset of a recorded line — provided it still
///   reconstructs that line to its end. Anywhere else a mid-line anchor would
///   be a claim about rows loomux cannot show it wrote.
///
/// **Why the width is not consulted.** Predicting the wrap points would need
/// the pane's geometry AND the CLI's own re-rendering of the text (prefixes,
/// re-indents, hard vs word wrap), and being wrong about them fails in the
/// dangerous direction — a wrongly-predicted run would claim a row that is not
/// ours. Reconstruction asks the safe question instead: not "where would this
/// have wrapped" but "is this row, verbatim, the next thing we wrote".
///
/// **The residual, restated accurately after review (rev-163 B1).** The record's
/// CONTENT is partly agent-authored — a relayed `report` note is the worker's
/// own words inside loomux's framing — so an agent that can (a) get a
/// marker-led line delivered into a pane, (b) print that line's head into that
/// same pane, and (c) have its CLI paint a live dialog whose rows are
/// byte-identical to that line's remainder, in order, hides the dialog.
///
/// An earlier version of this paragraph called that "two parties and three
/// coincidences". **That was wrong**, and the way it was wrong is the reason
/// entry to the record is now an explicit producer promise: via `notify_when`'s
/// agent-supplied `note`, all three are available to ONE agent in its OWN pane,
/// deterministically — the note is a tool argument it writes itself, the
/// delivered line is typed into its own pane so it knows the bytes, and the
/// "coincidence" in (c) is a choice it makes when authoring the note. See
/// [`OrchRegistry::mark_notice_maskable`], which is what closes it: a line is
/// claimable only where its producer has promised the pane's own occupant did
/// not author any span of it.
///
/// **What remains is PROXY-AUTHORSHIP by the recipient, and it is
/// orchestrator-shaped (rev-163 B3).** An earlier version of this paragraph
/// called the remainder a two-party case needing the author to induce the
/// recipient. That is also too kind, for the same reason as before: the check
/// [`OrchRegistry::deliver_relayed_to_root`] performs is CALLERSHIP
/// (`from != root`), not authorship. An orchestrator that tells a worker
/// "report with this exact note" gets its own chosen text marked — `from` is
/// the worker, so the check passes — and delivered into its OWN pane. The
/// worker complying is ordinary compliance, not a compromise, and the
/// orchestrator then prints the head row into its own pane at will, needing no
/// inducement at all.
///
/// The scope is the whole surface, not a corner: both marked call sites target
/// the orchestrator, so **every claimable line in the system lands in the pane
/// of the one agent best placed to dictate its content**. What it buys that
/// agent is precisely the #420 harm — loomux pressing Enter on a permission
/// dialog its own CLI was asking a HUMAN to answer.
///
/// **Accepted knowingly, not mitigated.** Authorship is not enforceable here:
/// an orchestrator instructing a worker is loomux's ordinary traffic and has
/// no attack signature to key on. The alternatives are the ones already
/// rejected — refusing question-shaped rows is vacuous, and refusing
/// agent-supplied fields deletes the motivating case. Two levers exist if this
/// is judged too expensive, and both are subtractions rather than fixes: drop
/// `R-top`'s mid-line anchor for marked lines (shrinks the surface, does not
/// close it, and gives up #576's scrolled-off half), or stop marking
/// altogether (the pre-record behaviour — the wrap residual simply stays open).
///
/// Under-masking stays the cheap error and this function keeps choosing it: any
/// mismatch, any missing tail, any unmarked line, any lost record ends the run.
pub fn mask_loomux_notices_with_record(tail: &str, delivered: &[String]) -> String {
    let rows: Vec<&str> = tail.lines().collect();
    let mut norm: Vec<String> = rows.iter().map(|r| wrap_normalize(deframe(r))).collect();
    let recorded: Vec<String> = delivered.iter().map(|d| wrap_normalize(d)).collect();
    let first_row = norm.iter().position(|n| !n.is_empty());
    let mut keep = vec![true; rows.len()];
    let mut i = 0;
    while i < rows.len() {
        if norm[i].is_empty() {
            i += 1;
            continue;
        }
        // The marker rule: one row, no record needed. Everything below only ever
        // claims MORE rows, never this one differently.
        //
        // #903 reads it through a leading POINTER glyph as well, and the reason is
        // the same replay that made the session record necessary. A resumed CLI
        // renders a replayed user turn with a `❯` in front of it, and `deframe`
        // does not treat a pointer as framing — so a notice loomux delivered and
        // the CLI replayed comes back as `❯ [orrerix] …` and the marker rule
        // stops seeing its own marker. Fifteen of the thirty-nine
        // `pointer-option` holds in the group's audit log were marker-led notices
        // wearing exactly that chevron.
        //
        // It claims no more than the marker rule already claims: ONE row, the one
        // the marker leads. The #420 objection to widening this — that an agent
        // can print a marker row itself, so a RUN-mask hands a pane the power to
        // delete the rows beneath an attacker-chosen row — is untouched, because
        // a pane that can print `[orrerix] …` can already print it and get that
        // single row claimed. Adding a chevron in front of it buys the pane
        // nothing it did not have.
        if leads_with_notice_marker(rows[i])
            || strip_leading_pointer(rows[i]).is_some_and(leads_with_notice_marker)
        {
            keep[i] = false;
        }
        let mut claimed = record_claim(&norm, &recorded, first_row, i);
        if claimed.is_none() {
            // #903: the same POINTER-stripped second reading `mask_own_paste`
            // has, and here for a reason that is not symmetry. A resumed CLI
            // renders a replayed USER turn with a leading `❯`, so the row that
            // starts a recorded prompt on a resumed pane's screen is the
            // recorded line with a chevron in front of it and matches nothing.
            // That row is the one this whole record exists to claim.
            //
            // Only the anchor row is re-read: a wrap run's continuations carry
            // no chevron, so stripping one off them would be inventing evidence.
            // And only lines that clear [`SELF_ECHO_MIN_POINTER_CHARS`] may
            // claim on the strength of a stripped pointer, for that constant's
            // own reason — `❯ Yes` is byte-identical whether it is a replayed
            // one-word turn of ours or a live dialog's highlighted choice.
            if let Some(head) = strip_leading_pointer(rows[i]).map(wrap_normalize) {
                if !head.is_empty() {
                    let long: Vec<String> = recorded
                        .iter()
                        .filter(|l| l.chars().count() >= SELF_ECHO_MIN_POINTER_CHARS)
                        .cloned()
                        .collect();
                    let framed = std::mem::replace(&mut norm[i], head);
                    claimed = record_claim(&norm, &long, first_row, i);
                    if claimed.is_none() {
                        norm[i] = framed;
                    }
                }
            }
        }
        // #903: a record claim is REFUSED when a live dialog's own question row
        // heads the block above the anchor.
        //
        // This narrows every record claim, not only the pointer-stripped ones,
        // and the uniformity is deliberate: one rule for the whole record path
        // means "which record did this line come from" is never something the
        // mask has to get right per row. It can only ever refuse a claim the
        // pre-#903 rule would have made — the fail-CLOSED direction, which costs
        // a hold the ten-minute `QuestionStale` badge already reports.
        //
        // It is also the term the widened record is bounded by. A recorded line
        // that happens to coincide with a dialog's rows cannot delete them out
        // from under the detector while the dialog's own question row is still
        // above them; `dialog_header_above` is the same shape-tracking scan
        // `mask_own_paste` uses for its short-pointer case. What it does NOT
        // bound is stated in `docs/design/question-gate-authorship.md`: a dialog
        // with no question row of its own, and a header whose text the same
        // party controls.
        if claimed.is_some() && dialog_header_above(&rows, &norm, &keep, i) {
            claimed = None;
            // N3: put the row back the way the pointer-stripped attempt found
            // it. That attempt rewrites `norm[i]` in place and only restores it
            // on ITS own miss, so a claim it made and this veto then nulled left
            // the stripped form behind for every later reader of `norm` — the
            // upward scans above included. It was inert only by coincidence
            // before B2; now that those scans test `is_dialog_header(&norm[j])`
            // on rows regardless of `keep`, a row left stripped of its leading
            // glyph is a row this function reads differently than the screen
            // shows it.
            norm[i] = wrap_normalize(deframe(rows[i]));
        }
        match claimed {
            Some(end) => {
                keep[i..end].fill(false);
                i = end;
            }
            None => i += 1,
        }
    }
    rows.iter()
        .zip(keep)
        .filter_map(|(r, k)| k.then_some(*r))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One record-anchored claim attempt at row `i`: `R-wrap` first, then `R-top`.
///
/// Extracted from [`mask_loomux_notices_with_record`] (#903) so the row can be
/// re-read with its leading pointer glyph stripped without the two rules being
/// written out twice — a second spelling of `R-top`'s floor or of
/// reconstruct-to-end is exactly the drift this file's guards keep finding.
fn record_claim(
    norm: &[String],
    recorded: &[String],
    first_row: Option<usize>,
    i: usize,
) -> Option<usize> {
    recorded.iter().find_map(|line| {
        reconstructs_to_end(norm, i, line, 0).or_else(|| {
            // `R-top`: a mid-line anchor, allowed at the first non-empty
            // row of the reading and nowhere else. `at > 0` because offset
            // zero is `R-wrap` above — a headless run is by definition
            // missing something.
            (first_row == Some(i) && norm[i].chars().count() >= R_TOP_MIN_ANCHOR_CHARS)
                .then(|| {
                    line.match_indices(norm[i].as_str())
                        .find_map(|(at, _)| {
                            (at > 0).then(|| reconstructs_to_end(norm, i, line, at)).flatten()
                        })
                })
                .flatten()
        })
    })
}

/// The lines of a delivery that [`mask_loomux_notices`] would claim — i.e. the
/// part of what loomux just pasted that is loomux's OWN writing, and the only
/// part [`DeliveredNotices`] can ever hold.
///
/// **Why the filter, rather than recording the whole payload.** Most of what
/// `deliver_now` pastes is agent text: a kickoff brief, an orchestrator's
/// prompt, the verbatim constituent payloads of a coalesced flush. Recording
/// those would give the record-aware mask the power to blind the gate to
/// ordinary agent content — the exact blindness `e11` pins as deliberately NOT
/// taken (a queued delivery whose own body is question-shaped must park the
/// pane exactly as it would have unqueued). Marker-led lines are loomux's
/// framing, so the record holds only what the mask was always allowed to claim,
/// and the record's job is limited to the rows those lines WRAPPED onto.
///
/// **A necessary condition, never a sufficient one (rev-163 B1).** A line
/// passing this filter is still not recordable: it must ALSO have been marked
/// by its producer through [`OrchRegistry::mark_notice_maskable`]. See that
/// method for the attack this ordering exists to stop — a marker-led line is
/// loomux's *framing*, and framing routinely carries a field some agent chose.
///
/// De-framed on the way in, so what the record holds is what
/// [`mask_loomux_notices_with_record`] compares against: the mask normalizes
/// rows as `wrap_normalize(deframe(row))`, and a payload line that arrived
/// framed would otherwise be recognised here (`leads_with_notice_marker` itself
/// de-frames), stored WITH the frame, and then match nothing (rev-163 N2 — a
/// fail-closed drift, but a drift, and `leads_with_notice_marker`'s doc claims
/// there is none).
pub fn loomux_authored_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| leads_with_notice_marker(l))
        .map(|l| deframe(l).trim().chars().take(DELIVERED_NOTICE_CHARS).collect())
        .collect()
}
/// The lines of a delivery that are loomux-ORIGINATED **prompt** text — a
/// kickoff brief or a `send_prompt` body — as opposed to a notice (#903).
///
/// The complement of [`loomux_authored_lines`], and the complement is the whole
/// safety argument rather than a tidy split. A notice is text loomux *writes*,
/// and the record of one is deliberately opt-in per producer, because
/// `notify_when(kind, pr, note)` takes an agent-supplied `note` and delivers the
/// fired notice into the **registering agent's own pane** — so admitting notice
/// text by provenance alone would let one agent, with one tool call it makes
/// itself, put a line of its choosing into its own pane's record, print that
/// line's head, and have its CLI paint a dialog whose rows are that line's
/// remainder. That is [`OrchRegistry::mark_notice_maskable`]'s documented
/// one-party attack, and this filter is what keeps it closed: marker-led lines
/// are excluded here and stay on the opt-in rule, unchanged.
///
/// What is left is text that reached a pane because loomux was asked to deliver
/// a PROMPT there — a kickoff brief, or an orchestrator's `send_prompt` body
/// (a coalesced flush's constituent payloads arrive here ONE AT A TIME, as
/// their own prompt bodies — the drainer splits the framing off before calling,
/// which is why this function never has to parse a flush apart; see
/// `record_contributions` on [`deliver_now`], and #632 for the split it reuses).
/// No agent can address one of those to itself — `send_prompt` refuses
/// a caller that names its own id, and a kickoff goes to an agent that did not
/// exist when the caller asked for it. `send_prompt` is the
/// orchestrator's tool, so the residual this admits is chiefly an orchestrator
/// writing into somebody else's pane — a party that already holds spawn and prompt
/// powers over that pane. The design note argues that asymmetry at length; it is
/// bounded, not closed.
///
/// Lines longer than [`DELIVERED_PROMPT_CHARS`] are DROPPED, never truncated —
/// see that constant for why a prefix would be a claim whose meaning depends on
/// the pane's width.
pub fn delivered_prompt_lines(text: &str) -> Vec<String> {
    // #903 B1 — TERM 1, and the finding that forced it. Filtering marker-led
    // lines ONE AT A TIME is not the same rule as excluding notices: a loomux
    // notice is one marker-led first line followed by a body that is not, so
    // every continuation row of a multi-line notice was entering this record.
    //
    // That is a one-party hole, not a cosmetic one. `note_directive(text,
    // replace: true)` writes the ledger RAW — no sanitize, no cap, no `[ts]`
    // prefix, and it is self-scoped by construction — and both
    // `compact_reinjection_notice` (via the self-callable `request_compact`) and
    // `resume_kickoff_notice` paste that ledger VERBATIM into the SAME agent's
    // pane, below their marker-led first line. So one agent, with two tool calls
    // it makes itself, could put a line of its own choosing into its own
    // session's record — exactly the capability
    // [`OrchRegistry::mark_notice_maskable`] is default-closed to withhold.
    //
    // The rule is therefore about the authorship of the DELIVERY, not of each
    // line: if loomux wrote the first non-empty line as a notice, loomux is
    // relaying whatever follows rather than originating it. That generalises to
    // every future embed, because a loomux notice is marker-led by producer
    // convention (#624/#632 enforce it at the producer), where a content
    // blocklist would need editing for each new one.
    //
    // Unforgeable in the direction that matters: this text is what loomux
    // pasted, and an agent controls only the block BELOW that first line. An
    // orchestrator prefixing its own `send_prompt` with the marker only
    // downgrades its own masking, which is the safe direction.
    if text.lines().find(|l| !l.trim().is_empty()).is_some_and(leads_with_notice_marker) {
        return Vec::new();
    }
    text.lines()
        .filter(|l| !leads_with_notice_marker(l))
        .map(|l| deframe(l).trim())
        .filter(|l| !l.is_empty() && l.chars().count() <= DELIVERED_PROMPT_CHARS)
        .map(str::to_string)
        .collect()
}

/// #903 B1': what a paste may contribute to the session prompt record.
///
/// One rule for both shapes, which is why it is three lines: the entries' OWN
/// texts under their OWN kinds, never the string that was pasted. A coalesced
/// flush pastes loomux's framing wrapped around N constituent payloads (#533-A),
/// and even a lone delivery can carry a flush header on its front — so the bytes
/// on the wire are a mixture whose parts have different authors and, in the flush
/// case, different [`Delivery`] kinds. The queue still has them apart at this
/// point; #632 already owns the framing/payload split
/// ([`unmaskable_framing_rows`]), and nothing is gained by making the record
/// re-derive it from a rendered string.
///
/// **Pure, and that is the point of extracting it** — the same argument
/// [`override_enter_admits`] carries. Welded into the drainer this rule would
/// need an `AppHandle` to exercise, so nothing in this repo could drive it, and
/// the round that introduced it shipped a regression no test could see: term 1
/// excluded the framed whole, which took the constituent payloads #903 needs
/// masked out of the record with the header.
///
/// Each pair is still admitted on its own merits downstream — both #903 B1 terms
/// run per contribution — so a re-grounding notice riding in a batch is refused
/// by its kind and a `resume_kickoff_notice` by its marker-led first line,
/// whatever they are flushed alongside.
#[doc(hidden)] // pub for integration tests
pub fn record_contributions_for(batch: &[queue::QueuedDelivery]) -> Vec<(String, Delivery)> {
    batch
        .iter()
        .filter_map(|e| e.payload.text().map(|t| (t.to_string(), e.delivery_kind)))
        .collect()
}

/// #903 B1 — TERM 2: may a delivery of this KIND contribute to the session
/// prompt record at all?
///
/// Spelled as an exhaustive match with **no wildcard arm**, for the reason
/// [`question_shown`] spells its `GridEvidence` reading the same way: a fifth
/// [`Delivery`] variant added later must decide which side it is on at the
/// compiler's insistence, instead of inheriting admission from a `_ => true`
/// nobody re-reads.
///
/// - `FreshKickoff` / `ResumeKickoff` — the brief an orchestrator wrote for a
///   spawn. `ResumeKickoff` in particular is the incident's OWN payload: the
///   `[orch] Round 3 (cap) re-record…` that wedged `rev-1277` reached `rev-1262`
///   on this kind, so refusing it would close the door by regressing #903.
/// - `MidSession` — a `send_prompt` body, orchestrator-authored. The accepted
///   two-party residual, argued in `docs/design/question-gate-authorship.md`.
/// - `Regrounding` — REFUSED. Its entire payload is the post-compact notice
///   whose body is the agent's own directive ledger.
///
/// This is the SECOND of two independent terms, and neither is redundant:
/// `ResumeKickoff` carries both an orchestrator brief (admitted here) and, at
/// the promoted-orchestrator call site, `resume_kickoff_notice`'s ledger embed —
/// which only TERM 1 refuses. Kind alone would let that through; content alone
/// would let a future non-marker-led embed through.
#[doc(hidden)] // pub for integration tests
pub fn prompt_record_admits_kind(kind: Delivery) -> bool {
    match kind {
        Delivery::FreshKickoff | Delivery::ResumeKickoff | Delivery::MidSession => true,
        Delivery::Regrounding => false,
    }
}

/// One pane's delivery record, or empty when there is no registry to ask.
///
/// `deliver_now`'s `reg` is `Option` for the headless wiring that has no
/// registry at all; an absent record is the same legitimate "nothing known"
/// an untouched pane has, and means the marker rule (#576).
pub(in crate::orchestration) fn delivered_lines(reg: &Option<Arc<OrchRegistry>>, pty_id: u32) -> Vec<String> {
    // #1702: this helper is the one place the delivery path resolves a pty to a
    // session, and it does so with no registry guard held — `deliver_now` and
    // the gates that call it take the per-pane delivery lock, never a registry
    // map. A caller that DOES hold one must pass its own session instead; see
    // [`OrchRegistry::session_for_pty`].
    reg.as_ref()
        .map(|r| r.delivered_mask_lines(pty_id, r.session_for_pty(pty_id).as_deref()))
        .unwrap_or_default()
}

/// The evidence floor a MID-LINE anchor must clear (rev-163 N1).
///
/// `R-top` claims a run whose first row is a fragment of a recorded line, on
/// the argument that the head scrolled off. Without a floor the fragment could
/// be one character, so any short first row that happens to appear somewhere in
/// a recorded line would carry a claim over everything below it that completes
/// the line — a one-character "proof" that the head scrolled off.
///
/// 24 chars is chosen from what the case being served actually looks like: a
/// scrolled-off wrap continuation is a full terminal row, so it is tens of
/// characters at minimum, and the narrowest pane anyone runs still leaves a
/// continuation far above this. The cost of the floor is therefore borne only
/// by fragments that were never good evidence.
const R_TOP_MIN_ANCHOR_CHARS: usize = 24;

/// Longest recorded line, in chars. `notify::NOTICE_TOTAL_CAP` already caps a
/// composed notice at 400; 512 leaves room for the framing a producer adds
/// around one without ever letting an unbounded payload in. A line longer than
/// this is truncated, which costs the tail of that one notice its wrap masking
/// (it can no longer reconstruct to its end) and nothing else — fail-closed, in
/// the direction everything here fails.
pub const DELIVERED_NOTICE_CHARS: usize = 512;

/// Lines remembered per pane. The mask can only ever use a line that is still
/// RENDERED, and `prompt_wait_detected` honours its structured signals across
/// the last twelve non-empty lines, so a notice that twenty-three later notices
/// have already pushed past is long out of every reading that consults this.
/// Drop-oldest, the `PendingIntake`/`OrchNoticeInbox::park` shape.
pub const DELIVERED_NOTICES_PER_PANE: usize = 24;
/// #903: how many characters of one delivered PROMPT line the session record
/// below keeps.
///
/// Four times [`DELIVERED_NOTICE_CHARS`], and the figure is measured rather
/// than picked: the live wedge this record exists for replayed an
/// **853-character** orchestrator prompt as ONE logical line, and
/// [`reconstructs_to_end`] can only claim a run that accounts for a recorded
/// line *to its end*. A cap sized for a notice's one sentence would therefore
/// record a prefix that reconstructs against nothing, and the gate would keep
/// holding with the record looking populated.
///
/// A line LONGER than this is dropped rather than truncated. Recording a prefix
/// would leave a line that can only ever be claimed by accident — a run whose
/// rows happen to end exactly where the truncation did — so the record would
/// carry entries whose meaning depends on a pane's width. Dropping degrades to
/// the same cheap error every other loss here does: the gate holds.
pub const DELIVERED_PROMPT_CHARS: usize = 2048;

/// #903: prompt lines remembered per SESSION, drop-oldest.
///
/// A resumed CLI replays its transcript's tail, so what can be on a resumed
/// pane's screen is the last few user turns — not the session's whole history.
/// Sixteen covers that with room to spare while keeping the record small enough
/// that its worst case is arithmetic rather than a hope.
pub const DELIVERED_PROMPT_LINES_PER_SESSION: usize = 16;

/// #903: sessions tracked at once, evicting the least-recently-written.
///
/// The ceiling this and the two constants above fix is
/// 2048 x 16 x 32 = 1 MiB, argued the same way [`DELIVERED_NOTICE_PANES`]
/// argues its own: a group is a handful of agents and a long session resumes
/// some of them, so this is generous for the live fleet while bounding a map
/// that would otherwise grow with every session id a run ever saw.
pub const DELIVERED_PROMPT_SESSIONS: usize = 32;

/// Panes tracked at once, evicting the least-recently-written. A group is a
/// handful of panes and a long session respawns some, so this is generous for
/// the live fleet while bounding the map that would otherwise grow with every
/// pty id a session ever used (`last_delivery`'s map does the same and is never
/// pruned, but its entries are three fields — these are up to
/// 24 × 512 chars, so the ceiling has to be stated: 64 panes ≈ 786 KiB worst
/// case).
pub const DELIVERED_NOTICE_PANES: usize = 64;

/// What loomux knows it wrote into one pane (#576): a bounded, drop-oldest
/// record of the marker-led lines it has pasted there.
///
/// **In memory only, and deliberately so.** The record's single consumer is a
/// mask over a LIVE pane's rendered tail, so it is worth exactly as long as the
/// rows it explains are still on screen. Persisting it would put agent-authored
/// note text in a second on-disk place with its own schema-version burden
/// (`queue.json` is versioned; a second file or a new field in that one is a
/// contract change), to buy masking for rows that a restarted pane has almost
/// always redrawn past. Losing it degrades to exactly the pre-#576 behaviour —
/// the marker rule alone, so a wrapped notice latches the gate until the
/// ten-minute `QuestionStale` badge surfaces it — which is the cheap error, and
/// the same one every other failure path here chooses.
/// **Two phases, and both are required (rev-163 B1).** A line becomes
/// claimable only if its PRODUCER marked it ([`OrchRegistry::mark_notice_maskable`],
/// which is the authorship promise) and a WRITE then delivered it
/// ([`OrchRegistry::record_delivered_text`], which is the "it really is on that
/// screen" half). Marking without a write claims text nobody painted; writing
/// without a mark claims text an agent may have chosen. Neither alone is
/// enough, and the default — a line nobody marked — is never claimable no
/// matter how often it is delivered.
#[derive(Debug, Default)]
pub struct DeliveredNotices {
    lines: VecDeque<RecordedLine>,
    /// Monotonic write stamp, for evicting the least-recently-written PANE.
    /// Not a clock: eviction only needs an order, and `now_ms()` would make the
    /// record's bound depend on the wall clock.
    pub(in crate::orchestration) seq: u64,
}

/// One line of [`DeliveredNotices`], and which of the two phases it has
/// reached. Only `written` lines are handed to the mask.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedLine {
    text: String,
    written: bool,
}

impl DeliveredNotices {
    /// Phase 1: a producer promises this text's spans are safe to claim in this
    /// pane. Nothing is claimable yet — the line is parked until a write
    /// delivers it.
    pub(in crate::orchestration) fn note_pending(&mut self, text: &str, seq: u64) {
        self.seq = seq;
        for text in loomux_authored_lines(text) {
            if self.lines.iter().any(|l| l.text == text) {
                continue;
            }
            self.lines.push_back(RecordedLine { text, written: false });
            while self.lines.len() > DELIVERED_NOTICES_PER_PANE {
                self.lines.pop_front();
            }
        }
    }

    /// Phase 2: these bytes just went to the pane. A line that was marked
    /// becomes claimable; a line that was NOT marked is ignored — deliberately
    /// silently, since most deliveries carry unmarked loomux framing and that
    /// is the ordinary case, not an error.
    pub(in crate::orchestration) fn note_written(&mut self, text: &str, seq: u64) {
        self.seq = seq;
        for delivered in loomux_authored_lines(text) {
            if let Some(l) = self.lines.iter_mut().find(|l| l.text == delivered) {
                l.written = true;
            }
        }
    }

    /// The claimable lines: marked by a producer AND delivered.
    pub(in crate::orchestration) fn claimable(&self) -> Vec<String> {
        self.lines.iter().filter(|l| l.written).map(|l| l.text.clone()).collect()
    }
}

/// #903: the prompt bodies loomux has delivered into one CLI **session**.
///
/// Keyed by session and not by pane, which is the entire point. A resumed pane
/// is a NEW pty replaying an OLD transcript, so a per-pane record is empty
/// exactly when the screen is fullest — and the rows on it are loomux's own
/// previous deliveries, rendered by the CLI with a leading pointer glyph, which
/// is what latched `prompt_wait_match`'s `pointer-option` signal for the whole
/// life of the pane. The record has to outlive the pane because the text does.
///
/// **One phase, not two** — unlike [`DeliveredNotices`], which parks a line
/// until a producer promises it. There is no producer to ask here: admission is
/// by PROVENANCE, and [`delivered_prompt_lines`] is the promise, made once,
/// structurally, about what may enter at all. Every line in here is text loomux
/// put on the wire as a prompt, recorded after the write succeeded.
///
/// **Not verbatim, and the difference is load-bearing rather than sloppy**: each
/// line is stored [`deframe`]d and trimmed, because that is the form the mask
/// compares against. A rendered row reaches `record_claim` as
/// `wrap_normalize(deframe(row))`, so a record holding the raw line would fail to
/// match any row whose leading glyph the CLI painted — and a brief line beginning
/// `* ` or `● ` is exactly such a row. Storing the compared form is what keeps the
/// two sides of that comparison from drifting.
#[derive(Debug, Default)]
pub struct DeliveredPrompts {
    lines: VecDeque<String>,
    /// Monotonic write stamp for evicting the least-recently-written SESSION —
    /// an order, not a clock, for [`DeliveredNotices`]'s reason.
    pub(in crate::orchestration) seq: u64,
}

impl DeliveredPrompts {
    /// These bytes just went to a pane bound to this session.
    pub(in crate::orchestration) fn note_written(&mut self, text: &str, seq: u64) {
        self.seq = seq;
        for line in delivered_prompt_lines(text) {
            if self.lines.iter().any(|l| *l == line) {
                continue;
            }
            self.lines.push_back(line);
            while self.lines.len() > DELIVERED_PROMPT_LINES_PER_SESSION {
                self.lines.pop_front();
            }
        }
    }

    pub(in crate::orchestration) fn claimable(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }
}

/// The rows of `rendered` that loomux WROTE and [`mask_loomux_notices`] could
/// not claim (#632). Empty is the invariant every multi-row notice owes.
///
/// **Why a helper rather than an assertion spelled out at each producer.**
/// [`mask_loomux_notices`] claims exactly one row per marker, so a notice that
/// occupies several rows is only fully maskable if *every* row it emits leads
/// with the marker once `deframe`d. #624 established that convention for
/// single-line notices and enforced it at `OrchNoticeInbox::park`; the two
/// pre-existing multi-row producers (`pause_suppression_notice` and
/// `queue::coalesced_flush_text`) build their own blocks, so their door is
/// their own last line. Both call this, and so do their tests — written
/// *through* `mask_loomux_notices` rather than re-deriving its rule, so the
/// check cannot drift from what it stands in for (the `park` precedent).
///
/// **`payloads` is the deliberate exemption, and it is narrow.** A coalesced
/// flush carries each constituent delivery's text VERBATIM, and those rows are
/// agent-authored by design: they are byte-identical to what the same delivery
/// would have painted on its own, so masking them would blind the gate to
/// ordinary pane content that no other delivery path hides. They are passed in
/// here and excused; everything else in the block is loomux's own framing and
/// must mask away. See the #632 section of `docs/design/orchestration.md` for
/// why leaving them to latch is the conservative direction.
///
/// Comparison is on trimmed, non-empty rows: blank rows carry no tokens for
/// `prompt_wait_detected` to match, and a framing row that were ever
/// byte-identical to a payload row would be excused — a false negative in an
/// assertion, which is the harmless direction for a guard.
pub fn unmaskable_framing_rows(rendered: &str, payloads: &[&str]) -> Vec<String> {
    let payload_rows: HashSet<&str> = payloads
        .iter()
        .flat_map(|p| p.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    mask_loomux_notices(rendered)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !payload_rows.contains(l))
        .map(str::to_string)
        .collect()
}
