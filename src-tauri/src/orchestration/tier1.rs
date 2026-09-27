//! The Tier-1 box scan: whether the input box still holds a paste, and the
//! scan census.
//! Design note: `docs/design/delivery-triage.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): `crate::pty`.
//! Sibling files it calls: `panetail.rs`, `screen.rs`, `tuning.rs`.

use super::*;

// #112 round-2 redesign: acceptance vs. processing. A queued prompt into a
// busy pane produces no PROCESSING evidence (no burst, no hook) for an
// arbitrarily long time — that's normal, correct CLI behavior, not a fault.
// Tier 1 (box consumption) answers a different, faster question — did the
// CLI ACCEPT the paste at all — and does so two-sidedly wherever it can
// verify its own precondition (see `box_holds_paste`'s doc). The extended
// "is this genuinely idle, not just busy" monitor below is what replaces a
// fixed timeout with an observed one for everything Tier 1 can't answer.
/// How much of the pane's tail (bytes, raw/pre-strip) Tier 1 reads to check
/// whether our own pasted text is still sitting at the box's tail end.
/// Same sizing philosophy as `QUESTION_SCAN_TAIL_BYTES` — reused directly
/// rather than inventing a second "how much tail matters" constant.
///
/// #559: this is Tier 1's FLOOR, not its budget. A scan window fixed at 4 KiB
/// while `QUEUE_FLUSH_MAX_BYTES` lets one coalesced flush paste 24 KiB makes
/// `box_holds_paste` structurally false for every large paste — the evidence
/// is not absent, it is unreachable — and two constants chosen for unrelated
/// reasons (question-scan cost vs. token budget) then jointly decide whether a
/// stranded delivery is ever noticed. `tier1_scan_bytes` derives the actual
/// read from the paste, with this as its lower bound; `TIER1_SCAN_MAX_BYTES`
/// is the ceiling, and it is derived from the flush cap rather than picked.
const BOX_TAIL_SCAN_BYTES: usize = QUESTION_SCAN_TAIL_BYTES;
/// Ceiling on one Tier 1 box read (#559) — the largest paste a single flush
/// may produce (`QUEUE_FLUSH_MAX_BYTES`) plus the same `BOX_TAIL_SCAN_BYTES`
/// of headroom the base window already allows for box framing, prompt symbol
/// and cursor chrome around our text. Derived, deliberately: the whole point
/// of this issue is that the scan budget and the flush cap were independent
/// numbers, so the relationship is expressed in the code rather than in a
/// comment asking a future reader to keep them in step. A single queue entry
/// larger than the flush cap still delivers alone (`plan_flush`'s "the cap is
/// a CEILING, never a floor"), so this bound can still be exceeded by the
/// paste — which is exactly the residual `BoxReading::Unverifiable` exists to
/// say out loud instead of reading as an idle pane.
const TIER1_SCAN_MAX_BYTES: usize = queue::QUEUE_FLUSH_MAX_BYTES + BOX_TAIL_SCAN_BYTES;
/// Ceiling on one WIDENED Tier 1 read (#685) — the pty output ring itself,
/// because that is the whole readable universe: a request past it cannot come
/// back with a byte the ring does not hold. Derived rather than picked, for the
/// same reason `TIER1_SCAN_MAX_BYTES` is. In practice the widening lands far
/// below this (it asks for what the measured retention says it needs, see
/// `Tier1Scan`); the cap only binds for a tail that is almost entirely escape
/// bytes.
const TIER1_SCAN_WIDEN_MAX_BYTES: usize = crate::pty::OUTPUT_RING_CAP;
/// How many times one Tier 1 read may re-request a wider tail (#685) before it
/// settles for the widest it got. Each round re-measures, so the estimate is
/// corrected rather than repeated; three is enough for a scaled request to
/// converge on a tail whose density is not uniform, and the bound exists so a
/// pathological one cannot spin. Running out is not a failure mode of its own:
/// the widest read still classifies, and a short one classifies as
/// `Unverifiable` exactly as it did before. `pub` so the bound is directly
/// testable rather than inferred from a loose "it terminated" assertion.
pub const TIER1_SCAN_WIDEN_ROUNDS: u32 = 3;
/// Extra slack (normalized characters) added around the pasted text's own
/// length when windowing "the tail end" for containment — covers box
/// framing/prompt-symbol/cursor characters immediately around our text
/// without widening the window enough to reach unrelated older output.
const BOX_TAIL_WINDOW_SLACK: usize = 200;
/// How much of one paste LINE [`paste_echo_probe`] samples for
/// [`box_reading`]'s "a partial match is not an absence" arm (#821).
///
/// Bounded above by what a single rendered row can comfortably hold. 48
/// characters is inside the narrowest pane anyone runs while being far past
/// coincidence for prose.
///
/// **"It must stay inside one row" is the conservative heuristic, not the
/// mechanism** (rev-305 Ground 4). The probe is matched against
/// [`normalize_deframed`] of the tail, where every row has already had its
/// LEADING decoration stripped and the whole thing flattened to
/// single-space-joined words — so a probe spanning a word-wrapped row boundary
/// matches perfectly well, which is the entire known copilot shape. Two things
/// actually break it, and a short probe only lowers the odds of meeting either:
/// a boundary carrying decoration `deframe` cannot reach (a trailing scrollbar
/// `┃`), or a HARD mid-word wrap, which splits one word into two tokens and so
/// inserts a space the needle does not have. Sizing this from the pane's real
/// width would be a new coupling to terminal geometry for a case that is
/// already strictly better than the status quo.
///
/// **The cost of firing too readily is real, and it is not the safe
/// direction.** `Unverifiable` falls through to `stranded_marker_action`'s
/// ordinary gates, so a probe that fires on everything erodes #813/#819's
/// repair back toward the deadlock it exists to break — a conditional deadlock
/// traded for a more frequent one. Too strict, and a decorated tail keeps
/// reading as a confident absence, which is the collision #819's licence exists
/// to prevent. The second is the expensive one, which is why the floor below
/// sits where it does rather than higher.
const PASTE_ECHO_PROBE_CHARS: usize = 48;
/// The evidence floor under [`paste_echo_probe`]: a line shorter than this
/// yields no probe at all (#821).
///
/// Same figure and same argument as `R_TOP_MIN_ANCHOR_CHARS` and
/// `SELF_ECHO_MIN_POINTER_CHARS` — short fragments are not evidence, and a
/// probe built from one would fire on coincidence, which here means declining
/// to retire a marker that should have retired. A paste with no line this long
/// simply keeps the pre-#821 reading.
const PASTE_ECHO_PROBE_MIN_CHARS: usize = 24;

/// Normalize prompt text for a landed-signal comparison: trim, collapse every
/// run of whitespace (including CR/LF, so CRLF-vs-LF and any TUI/JSON
/// re-wrapping wash out) to a single space. Deliberately NOT case-folding —
/// unlike an on-screen echo check (rejected in the design note precisely for
/// rendering fragility), this compares the raw text loomux itself pasted
/// against the JSON payload's OWN copy of that same text; case should already
/// agree, and folding it would only widen false positives.
pub(in crate::orchestration) fn normalize_prompt_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// [`normalize_prompt_text`], with each LINE [`deframe`]d first — the form the
/// Tier 1 box comparison runs in (#821).
///
/// **The defect.** Flattening a rendered tail whole puts the CLI's per-row
/// decoration *inside* the haystack, interleaved through whatever it decorated.
/// Copilot's framed composer draws `┃ ` down **every** row it wrapped a paste
/// onto, so `normalize_prompt_text` of that tail is `"┃ line one ┃ line two"`
/// while the paste it is compared against is `"line one line two"`. Containment
/// fails on text that is plainly still in the box.
///
/// **Applied to BOTH sides, and that is load bearing rather than tidy.** The
/// tempting version de-frames only the tail, on the reasoning that our own
/// paste carries no decoration. It is wrong, and wrong in the dangerous
/// direction: [`is_frame_char`] counts `*`, `•`, `●`, `◆` and `|` as framing,
/// so a brief containing an ordinary **markdown bullet list or table** would
/// have its `*`/`|` stripped out of the tail and kept in the needle. Every such
/// paste — and orchestrator briefs are full of them — would fail containment,
/// land past the length guard (the tail being longer, exactly as in the gutter
/// case), and read as a confident `NotHolding`. That is the very failure this
/// change exists to remove, re-introduced by the change itself. De-framing both
/// sides cancels it: the two lose the same characters, so containment is
/// unaffected and only specificity is spent.
///
/// The read-SIZING sites (`tier1_scan_bytes`, `Tier1Scan::for_paste`)
/// deliberately keep `normalize_prompt_text`: they size a request rather than
/// compare, so measuring the needle un-de-framed only ever asks for MORE tail
/// than the comparison needs, which is the conservative direction and preserves
/// #559's "no read is ever narrower than it was".
///
/// **Why `deframe` and not #820's full reconstruction.** `mask_own_paste`
/// stitches a wrapped line back together with [`reconstructs_to_end`], which is
/// strictly more precise — it verifies row STRUCTURE, not just the character
/// sequence — and strictly more brittle: any decoration it cannot account for
/// (a trailing gutter, the scrollbar's own `┃` on the right edge, a re-indent)
/// ends the run. There, brittleness was free: a failed reconstruction
/// under-masks, and an under-mask only costs a hold. **Here it is not.** This
/// feeds [`BoxReading`], where the expensive error is a confident `NotHolding`,
/// so the recogniser wants to be permissive about `Holds` and the residual
/// imprecision is handled by refusing to call a partial match an absence (see
/// [`box_reading`]). Same shared notion of what decoration is — one
/// [`deframe`], not a second copy of the rule — but a different instrument
/// built on it, because the failure direction is inverted.
fn normalize_deframed(s: &str) -> String {
    s.lines().flat_map(|l| deframe(l).split_whitespace()).collect::<Vec<_>>().join(" ")
}

/// The slice of a normalized tail that a needle of `needle_len` is looked for
/// within — the last `needle_len + BOX_TAIL_WINDOW_SLACK` characters.
///
/// One definition, because [`box_holds_paste`] and [`box_reading`]'s partial
/// probe must search the *same* window: a probe that ranged wider than the
/// containment it is qualifying could call a match partial on evidence the
/// containment was never allowed to see.
fn box_tail_window(norm_tail: &str, needle_len: usize) -> &str {
    let start = norm_tail.len().saturating_sub(needle_len + BOX_TAIL_WINDOW_SLACK);
    norm_tail.get(start..).unwrap_or(norm_tail)
}

/// Tier 1 (#112 round 2): does `stripped_tail` (ANSI-stripped current pane
/// output) still hold `pasted` at its TAIL END? Deliberately "at the tail
/// end", not "anywhere" — a CLI that echoes an ACCEPTED prompt into
/// scrollback/transcript history would make "appears anywhere" trivially
/// true forever and say nothing about acceptance. Both sides normalized the
/// same way `prompt_landed` already does (trim + collapse whitespace,
/// washing out line-wrap/CRLF noise); containment is checked only within
/// the last `pasted`-length-plus-slack window of the normalized tail
/// (`BOX_TAIL_WINDOW_SLACK`), so text that's scrolled up into older output
/// because something else happened since reads as gone even though it's
/// technically still present somewhere earlier in the buffer.
///
/// This same function serves BOTH of Tier 1's uses: checked once, on the
/// pre-Enter tail, it verifies Tier 1's own precondition (`deliver_prompt`'s
/// `tier1_governs`); polled after Enter, it's the two-sided signal itself
/// (`true` => still pending/vetoable, `false` => consumed/accepted).
#[doc(hidden)] // pub for integration tests
pub fn box_holds_paste(stripped_tail: &str, pasted: &str) -> bool {
    // #821: two routes, and the disjunction is the whole safety property —
    // de-framing is a SUPERSET of the flat comparison, never a replacement
    // (rev-306 B1). Either route finding our text is our text being present.
    //
    // The de-framed route is what #821 adds: it sees through a per-row gutter
    // the flat comparison cannot. Both sides are de-framed there — see
    // `normalize_deframed` for why the tail alone would be worse than the bug.
    //
    // The flat route is the pre-#821 comparison, kept because de-framing can
    // otherwise LOSE a match it used to find. A wrap that pushes a mid-line
    // `|`/`*` to a row START strips it from the tail (row-leading) and not from
    // the needle (mid-line, and `deframe` is leading-only), so `ps aux | grep`
    // wrapped before the pipe fails de-framed containment on text that is
    // verbatim on screen — and the probe, sampling that same line, carries the
    // same `|` and fails with it. That is a new route to `NotHolding`, the one
    // reading this whole change exists to make expensive, and it needs only a
    // pane under 48 columns rather than anything degenerate.
    //
    // Keeping both makes the property structural rather than case-analytic:
    // #821 can only ever ADD `Holds` readings. A narrower `deframe` (box-drawing
    // glyphs only, sparing `*` and `|`) would dodge today's two known shapes
    // and would still be a case analysis — and it would mint a second notion of
    // decoration, which is exactly what sharing `deframe` exists to avoid.
    holds_paste_under(stripped_tail, pasted, normalize_deframed)
        || holds_paste_under(stripped_tail, pasted, normalize_prompt_text)
}

/// One route of [`box_holds_paste`]: is `pasted` inside the tail-end window of
/// `stripped_tail`, with both sides put through `norm` (#821)?
///
/// Taking the normalizer as a parameter rather than spelling the windowing
/// twice is the point — the two routes must agree about what "the tail end"
/// means, or the disjunction would be comparing different questions.
fn holds_paste_under(
    stripped_tail: &str,
    pasted: &str,
    norm: fn(&str) -> String,
) -> bool {
    let norm_pasted = norm(pasted);
    if norm_pasted.is_empty() {
        return false; // nothing to still be holding
    }
    box_tail_window(&norm(stripped_tail), norm_pasted.len()).contains(&norm_pasted)
}

/// How many raw tail bytes Tier 1 must ask for to have any chance of finding
/// `pasted` in the box (#559) — the paste's own normalized length plus
/// `BOX_TAIL_SCAN_BYTES` of headroom for the framing/prompt/cursor chrome
/// around it, capped at `TIER1_SCAN_MAX_BYTES`.
///
/// **Why the read is derived from the paste rather than fixed.** Containment
/// needs the tail to be at least as long as what we are looking for, so a
/// window that is smaller than the paste cannot return `true` no matter what
/// the pane contains. The *semantic* window was already paste-relative —
/// `box_holds_paste` checks only the last `pasted`-length-plus-slack of the
/// normalized tail — so sizing the READ this way widens nothing about what
/// counts as "the tail end"; it only stops the read from truncating the
/// window the comparison already uses. A paste of 4 KiB or less asks for
/// exactly what it always did, so nothing changes for the ordinary delivery
/// (only pastes past the old fixed window move at all).
///
/// **Why the cost is acceptable.** This is proportional to a paste we have
/// already paid to write into the pane, and it only exceeds the old constant
/// for deliveries that are themselves multi-KiB. `LATE_MONITOR_QUESTION_SCAN_
/// BYTES` (32 KiB) is the standing precedent — and its own doc already
/// contemplates "Tier 1 governing a multi-KB brief", a state that until this
/// change could not actually occur.
///
/// Best-effort, not a guarantee: a heavily-ANSI-escaped tail strips down to
/// far fewer characters than the bytes read, so even this size can come back
/// too short. #583 measured how often (routinely, from ~1.5 KiB up) and #685
/// turned that residual into a re-read — see `Tier1Scan`, which is what every
/// call site now goes through. This function keeps the FLOOR: the first request
/// any Tier 1 read makes, unchanged, so no read is ever narrower than it was.
#[doc(hidden)] // pub for integration tests
pub fn tier1_scan_bytes(pasted: &str) -> usize {
    normalize_prompt_text(pasted)
        .len()
        .saturating_add(BOX_TAIL_SCAN_BYTES)
        .min(TIER1_SCAN_MAX_BYTES)
}

/// One delivery's Tier 1 scan window, counted in the unit the comparison
/// actually runs in (#685).
///
/// #559 derived the read from the paste; #583 measured what that bought. A live
/// tail retains roughly half its raw bytes through `strip_ansi` +
/// `normalize_prompt_text`, and the slack (`BOX_TAIL_SCAN_BYTES`) is a
/// CONSTANT, so the shortfall grows with the paste and crosses the needle's own
/// length while the paste is still only a couple of KiB. From there up
/// `box_reading` answered `Unverifiable` out of arithmetic, having learned
/// nothing about the pane: 58% of live deliveries in the 1.5-2 KiB band, and
/// every one from 2.5 KiB. That is the SAFE direction and it stayed safe — it
/// is simply not verification.
///
/// So the budget is counted in POST-STRIP characters and the raw request is
/// whatever it takes to deliver them: read, measure what survived stripping in
/// THIS pane, and — only when that came up short of the window the comparison
/// uses — re-request scaled by the retention this pane's own tail just
/// demonstrated. Scaling by a measured ratio rather than by a chosen inflation
/// constant is the point: density is a property of a CLI's repaint stream
/// (#583's whole finding), so any constant is right for one CLI and wrong for
/// the next.
///
/// **What bounds it.**
/// - `target_chars` is the containment window `box_holds_paste` compares
///   within — the paste plus `BOX_TAIL_WINDOW_SLACK` — and nothing past it is
///   ever looked at. An ordinary delivery's first read clears that target with
///   room to spare and widens not at all; only a read that is truncating the
///   comparison widens, which is also the only case that costs anything.
/// - The `TIER1_SCAN_MAX_BYTES` ceiling still caps the target, so a paste past
///   it stays `Unverifiable` at any density. Raising that ceiling — what
///   verification should even claim for a paste larger than a whole flush — is
///   a separate decision (#685's posture half) and is deliberately not taken
///   here.
/// - A read the ring UNDER-FILLS ends the widening on the spot: fewer bytes
///   back than asked for means the ring holds no more, so no wider request can
///   add one. This is #583's short-*pane* confounder, and it is exactly the
///   regime where widening buys nothing.
/// - `TIER1_SCAN_WIDEN_ROUNDS` bounds the re-reads regardless, and running out
///   returns the widest read taken rather than no read at all.
///
/// **Why this cannot manufacture a confirm.** #559's invariant was that every
/// box read for a delivery uses the same size, because a precondition verified
/// against a wide tail and then polled against a NARROW one could see the box
/// cut mid-paste, read `NotHolding`, and confirm a delivery nothing observed.
/// Two things rule that out here, and it is worth being exact about which:
/// - **Within one of these**, `request_floor` only ever rises, so the confirm
///   loop and the retry loop can never poll narrower than the precondition read
///   they revisit. That is the case the invariant was written for, kept in a
///   strictly stronger form — monotone, not merely equal.
/// - **Across the two that exist** — `deliver_now`'s and the late monitor's,
///   independent and on different threads — the monitor's floor starts at
///   `tier1_scan_bytes` again, so its first REQUEST can be narrower than a
///   widened one `deliver_now` already made. What makes that safe is not the
///   request size but that every read widens ITSELF before anything classifies
///   it: a reading is decided from a tail that covers the containment window
///   whenever the pane's density allows it, and when it does not the tail is
///   shorter than the needle, which is `Unverifiable` and never `NotHolding`.
///   A narrower first request costs a re-read, not a confirm.
///
/// The load-bearing property, so an edit preserves the right one: what makes a
/// `NotHolding` trustworthy is that the read COVERED THE WINDOW, not that it
/// matched an earlier read's byte count. And every read here is at least as
/// wide as the one this call site took before this change, so nothing that was
/// sound before became less so.
#[doc(hidden)] // pub for integration tests
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier1Scan {
    /// Where the NEXT read starts. Seeded with `tier1_scan_bytes` and raised
    /// (never lowered) by a widening, so a delivery that has already learned
    /// its pane is dense pays the discovery once instead of on every poll —
    /// and so successive reads are monotone, which is what makes the
    /// same-size invariant above hold in its stronger form.
    request_floor: usize,
    /// Post-strip characters a read must deliver to cover the containment
    /// window whole.
    target_chars: usize,
}

/// One Tier 1 tail read, as taken (#685) — the request that produced it beside
/// what came back, so the census records the size actually asked for rather
/// than the size the delivery started from.
#[doc(hidden)] // pub for integration tests
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier1TailRead {
    /// The FINAL raw request — `Tier1Scan`'s floor when nothing widened.
    pub requested_bytes: usize,
    /// Raw bytes the ring returned for it.
    pub tail_bytes: usize,
    /// Those bytes after `strip_ansi` — the haystack `box_reading` gets.
    pub stripped: String,
}

impl Tier1Scan {
    /// The scan window for one delivery's `pasted` text. Built once per
    /// delivery (and once per late monitor), never per read: `pasted_text` is
    /// fixed for a delivery's whole life, and normalizing a multi-KiB paste is
    /// not something a 100ms poll should repeat.
    pub fn for_paste(pasted: &str) -> Self {
        Self {
            // Deliberately the same function the old call sites called, rather
            // than a second copy of its arithmetic: one definition of the floor
            // is worth one extra normalize per delivery.
            request_floor: tier1_scan_bytes(pasted),
            target_chars: normalize_prompt_text(pasted)
                .len()
                .saturating_add(BOX_TAIL_WINDOW_SLACK)
                .min(TIER1_SCAN_MAX_BYTES),
        }
    }

    /// Post-strip characters a read has to deliver to cover the whole window
    /// `box_holds_paste` compares within.
    pub fn target_chars(&self) -> usize {
        self.target_chars
    }

    /// Where the next read starts — `tier1_scan_bytes` until a widening raises
    /// it.
    pub fn request_floor(&self) -> usize {
        self.request_floor
    }

    /// The next, WIDER raw request after a read of `requested` bytes came back
    /// as `tail_bytes` raw / `tail_chars` post-strip, or `None` to stop.
    ///
    /// Pure, and the whole of the sizing decision — the reader below is just
    /// the loop around it. Each `None` is a different reason to stop, and they
    /// are all "a wider request cannot change the answer":
    /// the window is already covered; the ring under-filled this one; or the
    /// ratio is undefined because nothing survived stripping (the same
    /// zero-denominator `retained_pct` refuses to report, and for the same
    /// reason — a guess dressed as a measurement is worse than the honest
    /// `Unverifiable` that follows).
    pub fn widen(&self, requested: usize, tail_bytes: usize, tail_chars: usize) -> Option<usize> {
        if tail_chars >= self.target_chars || tail_bytes < requested || tail_chars == 0 {
            return None;
        }
        // Scale the RAW request by the shortfall the read just measured:
        // `requested` bytes bought `tail_chars`, so `target_chars` needs this
        // many. Rounded up, so a read one character short still grows.
        let next = (requested as u64)
            .saturating_mul(self.target_chars as u64)
            .div_ceil(tail_chars as u64)
            .min(TIER1_SCAN_WIDEN_MAX_BYTES as u64) as usize;
        (next > requested).then_some(next)
    }

    /// Take one Tier 1 tail read, widening until it covers the window or until
    /// one of `widen`'s stopping conditions says nothing wider would help.
    /// `None` is no read at all (the pty is gone) — never a short read, which
    /// is a different fact and is `Some` with the shortfall visible in it.
    ///
    /// `read` is the raw-byte reader — `PtyManager::output_tail_bounded` at
    /// every call site. A closure rather than the manager itself so the loop is
    /// drivable by a test that counts the requests and sizes them, which is the
    /// only way to pin "never narrower" and "bounded" on the real code rather
    /// than on a re-implementation of it.
    pub fn read(&mut self, mut read: impl FnMut(usize) -> Option<Vec<u8>>) -> Option<Tier1TailRead> {
        let mut requested = self.request_floor;
        let mut rounds = 0u32;
        loop {
            let raw = read(requested)?;
            let stripped = strip_ansi(&raw);
            // #821: measured through the SAME normalization the containment
            // runs on. Counting the un-de-framed length credits the read with
            // the CLI's own gutters — characters `box_holds_paste` will never
            // search — so the widening would stop early believing it had
            // covered a window it had not.
            let chars = normalize_deframed(&stripped).len();
            let wider = (rounds < TIER1_SCAN_WIDEN_ROUNDS)
                .then(|| self.widen(requested, raw.len(), chars))
                .flatten();
            let Some(next) = wider else {
                return Some(Tier1TailRead {
                    requested_bytes: requested,
                    tail_bytes: raw.len(),
                    stripped,
                });
            };
            requested = next;
            // Monotone by construction (`widen` only returns a larger request),
            // and asserted as `max` rather than assignment so the floor cannot
            // be walked backwards by a future edit to `widen`.
            self.request_floor = self.request_floor.max(next);
            rounds += 1;
        }
    }

    /// The census for one read of this scan (#583's record, #685's sizes).
    /// `None` is a read that never happened, and the request recorded then is
    /// the one that was about to be made — built here rather than at each
    /// `json!` site for the same reason `Tier1ScanCensus::to_json` is.
    pub fn census(&self, read: Option<&Tier1TailRead>, pasted: &str) -> Tier1ScanCensus {
        Tier1ScanCensus::measure(
            read.map_or(self.request_floor, |r| r.requested_bytes),
            read.map(|r| (r.tail_bytes, r.stripped.as_str())),
            pasted,
        )
    }
}

/// What a Tier 1 box read actually ESTABLISHED (#559) — the three-state
/// replacement for the bare `box_holds_paste` bool every consumer used to
/// pass around.
///
/// The bool conflated two states that mean opposite things. `false` because
/// the tail was read and our text is not in it is evidence about the pane.
/// `false` because the tail we could read is SHORTER than our own paste is
/// arithmetic: containment cannot hold when the haystack is smaller than the
/// needle, so the answer was fixed before the pane was ever consulted. Every
/// consumer that treated the second as the first — `unconfirmed_disposition`
/// reading it as "the box holds nothing, so this pane is simply idle",
/// `stranded_selfheal_action` reading it as "its text is gone" — was drawing
/// a conclusion from a foregone `false`.
///
/// This is the same lesson #536 landed for escalation and #445 for suppressed
/// notices: a path that quietly opts out of governing must not be
/// indistinguishable from one that governed and found nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxReading {
    /// Our pasted text is identifiably at the box's tail end.
    Holds,
    /// The tail was long enough to have contained our paste, and does not.
    /// An informative absence: the CLI consumed it, collapsed it to a
    /// placeholder, or it never landed.
    NotHolding,
    /// No usable read: the pty is gone, or the tail we got back is shorter
    /// than our own paste. `box_holds_paste` is false either way, and that
    /// `false` says nothing at all about the pane.
    Unverifiable,
}

impl BoxReading {
    /// Stable audit token, so every record of a Tier 1 reading spells the
    /// three states the same way — kept next to the enum rather than
    /// stringified at each `json!` site, which is how two records of the same
    /// fact drift into two vocabularies.
    pub fn as_str(self) -> &'static str {
        match self {
            BoxReading::Holds => "holds",
            BoxReading::NotHolding => "not-holding",
            BoxReading::Unverifiable => "unverifiable",
        }
    }
}

/// Classify one Tier 1 box read (#559). `stripped_tail` is `None` when the
/// tail could not be read at all (pty gone).
///
/// Ordering matters: a positive `box_holds_paste` is decided FIRST, so a
/// reading that did find our text can never be downgraded by the length
/// arithmetic below it (it cannot be — `Holds` implies the tail was at least
/// as long as the paste — but the order makes that obvious rather than
/// inferred).
#[doc(hidden)] // pub for integration tests
pub fn box_reading(stripped_tail: Option<&str>, pasted: &str) -> BoxReading {
    let Some(tail) = stripped_tail else { return BoxReading::Unverifiable };
    if box_holds_paste(tail, pasted) {
        return BoxReading::Holds;
    }
    // An empty paste is `NotHolding`, matching `box_holds_paste`'s own
    // "nothing to still be holding" — the answer is false for a reason that
    // is about the paste, not about how much tail we could see. Both routes
    // must agree it is empty: a paste that is ONLY framing (`* * *`) de-frames
    // to nothing while still being text under the flat route.
    if normalize_deframed(pasted).is_empty() && normalize_prompt_text(pasted).is_empty() {
        return BoxReading::NotHolding;
    }
    // **Every arm below is asked of BOTH routes** (#821, rev-307). Once
    // `box_holds_paste` became a disjunction, an arm that consults one
    // normalization is asking about one of the two comparisons that could have
    // found our text — and answering for both. That asymmetry is this file's
    // recurring defect, now in its third instance:
    //
    // - the LENGTH arm. De-framing shrinks a frame-heavy needle (a markdown
    //   table row) more than it shrinks an un-gutted tail, so the de-framed
    //   test can decline to fire exactly where the flat one would have. On a
    //   truncated read that lands on `NotHolding` where the pre-#821 code said
    //   `Unverifiable` — the dangerous direction, on a narrower trigger than
    //   the containment defect but the same kind.
    // - the PROBE arm, found by the rev-307 sweep rather than reported. The
    //   probe carries whatever frame characters are MID-line in our own text,
    //   and a wrap that pushed one to a row start removes it from the de-framed
    //   tail — so a de-framed probe can miss where a flat probe matches. It
    //   only bites once both containments have failed, which a truncated read
    //   supplies.
    //
    // Asking each arm per route and OR-ing makes the property structural
    // instead of a claim to re-verify every time this function grows an arm:
    // any route that could have found the text gets to say "I could not tell".
    // `Unverifiable` is the safe answer here, so a union is the safe shape.
    for norm in [normalize_deframed as fn(&str) -> String, normalize_prompt_text] {
        let norm_pasted = norm(pasted);
        if norm_pasted.is_empty() {
            continue; // this route has nothing to look for; the other may.
        }
        let norm_tail = norm(tail);
        if norm_tail.len() < norm_pasted.len() {
            return BoxReading::Unverifiable;
        }
        // **A PARTIAL match is not an absence.** The length test above only
        // catches a tail too SHORT to have held our paste; per-row decoration
        // ADDS characters, so the tail that defeated containment is typically
        // LONGER than the paste it failed to contain and sails past it. What
        // is left would be `NotHolding`: not an absence of evidence, but
        // counterfeit evidence of an absence, which is the single input
        // `stranded_marker_action`'s retirement licence does not defend
        // against.
        //
        // So: if our paste's own line is still sitting in the window while the
        // whole of it will not match, the text is evidently there and the
        // rendering is what we failed to read. That covers decoration nobody
        // has catalogued — a trailing gutter, the scrollbar's `┃` on the right
        // edge, a re-indent, whatever the next TUI release paints — without
        // modelling any of it.
        if let Some(probe) = paste_echo_probe(pasted, norm) {
            if box_tail_window(&norm_tail, norm_pasted.len()).contains(&probe) {
                return BoxReading::Unverifiable;
            }
        }
    }
    BoxReading::NotHolding
}

/// The strongest fragment of `pasted` that a single rendered ROW can be
/// expected to hold intact — [`box_reading`]'s partial-match probe — or `None`
/// where no line is long enough to be evidence of anything (#821).
///
/// **Why a line and not the flattened paste.** The obvious probe is "the first
/// N characters of the needle", and it is wrong for exactly the reason the
/// needle itself failed: as soon as N runs past the first LOGICAL line it spans
/// a `\n` the tail never had, so it can only match by accident. A probe that
/// dies to the same thing it is meant to detect is not a backstop. Taking it
/// from one line keeps the probe a contiguous run of the pane's own words.
///
/// Note this is about logical lines, not rendered rows: a long line the CLI
/// wrapped is still one contiguous run in [`normalize_deframed`]'s output (see
/// [`PASTE_ECHO_PROBE_CHARS`] for what does and does not survive a row
/// boundary).
///
/// The LONGEST line, because that is the strongest evidence available: the more
/// of our own prose a fragment carries, the less it can be something the CLI
/// happened to paint. Truncated to [`PASTE_ECHO_PROBE_CHARS`] so a long line
/// that WRAPS still probes only its first row, and floored at
/// [`PASTE_ECHO_PROBE_MIN_CHARS`] so a paste of short lines yields no probe at
/// all rather than a coincidence-prone one — that paste keeps the pre-#821
/// behaviour, which is the honest outcome when there is no evidence to be had.
///
/// **Residual, stated accurately** (rev-305 Ground 4 corrected an earlier,
/// more pessimistic version of this): the probe fails only where its span
/// crosses a row boundary that carries decoration `deframe` cannot reach, or a
/// hard mid-word wrap — a narrow pane raises the odds of crossing a boundary
/// at all, it does not itself break anything. Where that happens the reading
/// falls through to `NotHolding`.
///
/// **Why that is still not a regression** (rev-306 F1). An earlier wording
/// justified this with "containment is wrap-agnostic", and that does not
/// survive the second cause above: a hard mid-word wrap splits one word into
/// two tokens, so it defeats CONTAINMENT too, not merely the probe. The
/// conclusion holds on a different argument — that case is **unchanged by
/// #821, not introduced by it**. `normalize_prompt_text` flattened the break
/// into a space the needle lacked in exactly the same way, so a hard mid-word
/// wrap read `NotHolding` before this change and reads `NotHolding` after it.
/// What #821 moves is elsewhere, and both directions are away from the
/// dangerous reading: de-framing turns the gutter case from `NotHolding` into
/// `Holds`, and the probe turns residual failures into `Unverifiable`.
///
/// **This probe cannot be the thing that guarantees no regression, and an
/// earlier version of this doc wrongly implied it could** (rev-306 B1). A wrap
/// that pushes a mid-line `|`/`*` to a row start strips it from the tail and
/// not from the needle — and this probe samples that same needle line, so it
/// carries the same character and fails for the same reason. A backstop that
/// shares the failure mode of what it backstops adds nothing there. Worse, the
/// bound previously stated here (a paste whose longest line is under
/// [`PASTE_ECHO_PROBE_MIN_CHARS`]) governs only whether a probe is FORMED and
/// says nothing about whether a formed probe MATCHES: a wrap boundary lands
/// inside the 48-character sample whenever the composer is under 48 columns, so
/// the exposure was an ordinary 25-47 column split, not a degenerate pane.
///
/// The guarantee lives in [`box_holds_paste`] instead, where it is structural:
/// the de-framed route is a SUPERSET of the flat one, so #821 can only add
/// `Holds` readings. This probe's job is narrower — turning a residual
/// containment failure into `Unverifiable` rather than a confident absence.
///
/// **`norm` is a parameter for the same reason (rev-307 sweep).** A probe built
/// under one normalization and searched in a tail under that same one is a
/// probe for ONE of [`box_holds_paste`]'s two routes; run alone it answers for
/// both, and can therefore miss where the other route's probe would have hit.
/// [`box_reading`] runs it once per route and takes the union, so no route's
/// evidence is silently spent on the other's behalf.
fn paste_echo_probe(pasted: &str, norm: fn(&str) -> String) -> Option<String> {
    pasted
        .lines()
        .map(norm)
        .max_by_key(|l| l.chars().count())
        .filter(|l| l.chars().count() >= PASTE_ECHO_PROBE_MIN_CHARS)
        .map(|l| l.chars().take(PASTE_ECHO_PROBE_CHARS).collect())
}

/// One Tier 1 box read, MEASURED (#583) — the numbers the `BoxReading` beside
/// it was decided from, so a live group's audit log can say whether the read
/// is routinely too short rather than only that this one was.
///
/// #559 sized the read from the paste and accepted a residual: the slack it
/// adds (`BOX_TAIL_SCAN_BYTES`) is counted in RAW bytes, while the containment
/// it protects runs on the tail AFTER `strip_ansi` and `normalize_prompt_text`.
/// A repaint-heavy TUI tail can therefore strip below the length of the very
/// paste it is meant to contain, and near-cap deliveries land in
/// `Unverifiable` — the SAFE direction (stated, notified, never a false
/// confirm), but delivering none of the verification coverage the derived read
/// was for. Whether that is the norm or the exception turns on one number
/// nothing recorded: how many characters survive per raw byte of a live pane's
/// tail.
///
/// So this is instrumentation, and ONLY instrumentation — nothing here decides
/// anything, deliberately. #583's question is what the distribution is; the
/// slack is a tuning decision that should follow the measurement rather than a
/// guess about it. `tier1_decline` already says WHICH reading was reached;
/// this says how close it ran, in the terms that would size a fix:
/// `retained_pct` is the density the slack has to buy through, and
/// `margin_chars` is the headroom this read had over its own needle.
#[doc(hidden)] // pub for integration tests
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier1ScanCensus {
    /// What the ring was asked for — since #685 the FINAL request of a read
    /// that may have widened itself, not the `tier1_scan_bytes` floor it
    /// started from. Both jq recipes in the design note still read it the same
    /// way: it remains "the window this reading was decided against", which is
    /// what `tail_bytes == requested_bytes` (the short-ring confounder) and
    /// every breakeven computed from it depend on.
    pub requested_bytes: usize,
    /// Raw bytes the ring actually returned — short of `requested_bytes`
    /// whenever the pane has simply not produced that much output yet, which
    /// is the confounder any reading of `tail_chars` has to rule out first.
    /// `None` is no read AT ALL (pty gone), never `Some(0)`: those are
    /// different facts, and not conflating them is the whole of #559.
    pub tail_bytes: Option<usize>,
    /// What those bytes came to after `strip_ansi` + `normalize_deframed` —
    /// the haystack the containment check actually gets (#821).
    pub tail_chars: Option<usize>,
    /// The needle's own length under that SAME normalization (#821). Not the
    /// same number as `tier1_paste_bytes` (which is raw), nor as the
    /// `normalize_prompt_text` length the read-sizing sites use, and it is
    /// this one the arithmetic compares against.
    pub paste_chars: usize,
}

impl Tier1ScanCensus {
    /// Measure one read. `read` is `None` when no read happened at all;
    /// otherwise `(raw bytes returned, the stripped text those bytes made)` —
    /// ONE option rather than two, so a call site cannot answer "was there a
    /// read" two ways.
    pub fn measure(requested_bytes: usize, read: Option<(usize, &str)>, pasted: &str) -> Self {
        Self {
            requested_bytes,
            tail_bytes: read.map(|(raw, _)| raw),
            // The same normalization `box_reading` compares through, not a
            // char count of the stripped text: whitespace collapse is a real
            // part of the shrink (a TUI pads every box row out to the terminal
            // width), and a census that measured only the ANSI half would
            // under-report the loss that actually decides the reading.
            //
            // #821: "the same normalization" is now `normalize_deframed`, and
            // BOTH lines move with it. `margin_chars` is a DIFFERENCE, so it
            // is only meaningful while its two terms measure the same kind of
            // string — the first cut of this moved `tail_chars` alone and left
            // `paste_chars` on `normalize_prompt_text` one line below, which
            // differenced two normalizations and understated the margin by
            // exactly the framing characters a brief's own markdown bullets
            // and table rows carry (rev-305 B1). That is the shape `h5` exists
            // for, so the bias was systematic, one-directional, and pointed at
            // over-reporting the very arm this number is read to count.
            //
            // Contrast the read-SIZING sites (`tier1_scan_bytes`,
            // `Tier1Scan::for_paste`), which deliberately keep
            // `normalize_prompt_text`: those size a REQUEST, where an
            // un-de-framed needle only ever asks for more tail and larger is
            // the safe direction. These two MEASURE, and a measurement has to
            // match the comparison it describes.
            tail_chars: read.map(|(_, stripped)| normalize_deframed(stripped).len()),
            paste_chars: normalize_deframed(pasted).len(),
        }
    }

    /// Characters of headroom the read had over the text it was looking for.
    /// **Negative is exactly the LENGTH `Unverifiable` arm** for every paste
    /// `deliver_prompt` can actually make (a non-empty one): `box_reading`
    /// compares the same two normalized lengths, so a histogram of this over a
    /// live group IS the distribution #583 asks for rather than a proxy for
    /// it. An empty paste is `NotHolding` for its own reason and never reaches
    /// that arm, so the equivalence holds there vacuously rather than by
    /// exception. `None` is the one `Unverifiable` this cannot speak for — no
    /// read happened, so there is no margin, which is itself the reading.
    ///
    /// **#821 added a SECOND `Unverifiable` arm this cannot see.** A read with
    /// ample margin can still be unreadable — the partial-match arm fires on a
    /// tail that is *longer* than the paste, which is the whole shape of the
    /// gutter defect. So a non-negative margin no longer implies the reading
    /// was decisive, and a histogram of this measures short reads only. Said
    /// plainly rather than left for a reader to discover, because "negative is
    /// exactly the arm" was true when written and quietly stopped being so.
    ///
    /// **And since the rev-307 sweep there are two LENGTH arms, of which this
    /// measures one.** `box_reading` runs its length test once per
    /// normalization and takes the union, so a read can be `Unverifiable` on
    /// the flat route's arithmetic while this de-framed margin is comfortably
    /// positive. The implication (negative margin ⇒ `Unverifiable`) still
    /// holds — a union only ever adds — but the converse is now false twice
    /// over rather than once.
    pub fn margin_chars(&self) -> Option<i64> {
        self.tail_chars.map(|chars| chars as i64 - self.paste_chars as i64)
    }

    /// Characters surviving per 100 raw bytes read — the ANSI/whitespace
    /// density #583 exists to measure, since it is what a raw-byte slack has
    /// to buy through. `None` when nothing was read, and also for a zero-byte
    /// read, where the ratio is undefined rather than 0%.
    pub fn retained_pct(&self) -> Option<u32> {
        let raw = self.tail_bytes.filter(|n| *n > 0)?;
        let chars = self.tail_chars?;
        Some(((chars as u64 * 100) / raw as u64) as u32)
    }

    /// The audit shape, built once here rather than at each `json!` site — the
    /// same argument `BoxReading::as_str` makes next to its own enum: two
    /// records of one fact in two vocabularies cannot be aggregated, and this
    /// record exists only to be aggregated. The derived fields are carried
    /// rather than left to the reader for the same reason.
    pub fn to_json(&self) -> Value {
        json!({
            "requested_bytes": self.requested_bytes,
            "tail_bytes": self.tail_bytes,
            "tail_chars": self.tail_chars,
            "paste_chars": self.paste_chars,
            "margin_chars": self.margin_chars(),
            "retained_pct": self.retained_pct(),
        })
    }
}
