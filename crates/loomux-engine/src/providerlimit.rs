//! **Provider spend/usage limits, read off a pane's own text** (#2811 S5a).
//!
//! When the account behind a block's model runs out of budget, the CLI in that
//! pane prints the provider's refusal and then sits there. Nothing else in
//! loomux can tell that state apart from "the agent is thinking": the pane is
//! alive, the session is intact, the process has not exited, and no report ever
//! arrives. Before this module the only thing that eventually noticed was a
//! sixty-minute lane-stall timeout, once per affected drive.
//!
//! Everything here is data plus `match` — no I/O, no registry, no clock — the
//! same class as [`crate::model`], and for the same reason: the two consumers
//! sit on opposite sides of the engine/`src-tauri` seam. The attention scan
//! (`OrchRegistry::attention_tick`) raises the `provider-limit` chip from it;
//! #2811 S5b reads the same table to decide which review drives to hold.
//!
//! # Why the needles are matched against a REASSEMBLED line
//!
//! The subject is a terminal ring, and a terminal wraps. The captured
//! OpenRouter refusal in `src-tauri/tests/fixtures/attention/` is
//!
//! ```text
//!   |  This request would exceed your available credits given your current in-
//!   |  flight requests. Retry after in-flight requests settle, or add credits.
//! ```
//!
//! (with pi's `U+2503` box gutter where this doc draws `|`) — broken mid-word,
//! with the gutter re-drawn on the continuation. A plain `tail.contains(needle)`
//! over any needle longer than the first line silently returns `false` there,
//! and the same needle matches perfectly against the unwrapped copy someone
//! pastes into a test, so the guard reads green while being blind on exactly
//! the real subject (CLAUDE.md, the line-oriented-sweep blind spot).
//! [`limit_in_tail`] therefore strips each line's leading indent and gutter
//! glyph, drops the blank gutter rows, and tests each needle against a window
//! of up to [`WRAP_WINDOW`] consecutive stripped lines rejoined — with no space
//! where the break fell on a hyphen.
//!
//! # Why a match must be LINE-INITIAL
//!
//! A pane that merely QUOTES a provider's refusal is the false positive that
//! matters, and it is not hypothetical: the orchestrator's own pane types the
//! string into `ask_human` while asking the human to top the account up, and a
//! worker's kickoff brief can quote it too. Every captured refusal begins its
//! own rendered line (after the indent/gutter the strip removes); a quotation
//! of one normally sits mid-sentence, behind a `got "` or a `stopped at "`.
//! That is the discriminating axis, so the match is anchored to the start of a
//! reassembled line rather than searched for anywhere in the tail.
//! `negative-orchestrator-quotes-a-limit.txt` is the control, and the residual
//! the anchor does NOT close — prose that OPENS with a needle — is pinned by
//! `a_quotation_of_a_refusal_is_not_a_refusal` rather than left implied.
//!
//! # The table is data
//!
//! Adding a provider, or a second spelling of one provider's refusal, is one
//! row on [`LIMIT_PATTERNS`] — never a new branch in the scan (CLAUDE.md
//! constraint 8: no machine- or repo-specific knowledge in product code, and
//! nothing here is either — these are the vendors' own strings). Each row
//! carries its own [`PatternSource`] so a reader can tell a needle cut from a
//! captured pane apart from one taken on report.

/// Where a row's needle came from — provenance carried in the table rather
/// than in a comment, because the two classes are not equally trustworthy and
/// a reader is owed the difference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternSource {
    /// Cut from a pane tail captured in a real incident. The matching fixture
    /// under `src-tauri/tests/fixtures/attention/` is that capture, and
    /// `a_captured_pane_raises_its_providers_limit` is its positive control.
    Captured,
    /// The vendor's documented / widely-reported wording for the same state,
    /// with no capture in hand here. Pinned by an inline string in the tests,
    /// not by a fixture — the honest bound on what it is evidence of.
    Reported,
}

/// One provider whose spend/usage limit can stop a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Provider {
    /// Stable id used in audit rows, hold reasons and dedup keys. Lowercase,
    /// no spaces — #2811 S5b writes it to `review_drives.json`.
    pub id: &'static str,
    /// What the human is shown.
    pub display: &'static str,
    /// What the human can actually DO about it. A limit chip that does not say
    /// this is a chip that only reports bad news.
    pub remedy: &'static str,
}

/// The closed set of providers this module knows a refusal string for.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "anthropic",
        display: "Anthropic (Claude)",
        remedy: "add usage credits, run /limit-reset in one pane, or wait for the rolling window",
    },
    Provider {
        id: "openrouter",
        display: "OpenRouter",
        remedy: "raise the key's total limit or add credits at openrouter.ai",
    },
];

/// One spelling of one provider's refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitPattern {
    /// The [`Provider::id`] this refusal belongs to.
    pub provider: &'static str,
    /// The needle, matched at the START of a reassembled line (see the module
    /// doc). Deliberately stops short of any character a terminal or a font may
    /// render differently: the captured Claude line ends `what you`+U+2019+`re
    /// working on.` with a CURLY apostrophe, so the needle stops at `what you`
    /// rather than carrying an ASCII `'` — which is what the orchestrator's
    /// PARAPHRASE of the same message uses, and so would have matched the
    /// paraphrase and not the pane.
    pub needle: &'static str,
    pub source: PatternSource,
}

/// Every refusal spelling, in one table. **Extending this is a row, not a
/// branch.**
pub const LIMIT_PATTERNS: &[LimitPattern] = &[
    // Claude Code, out of usage credits. Captured:
    // `fixtures/attention/claude-usage-limit.txt`.
    LimitPattern {
        provider: "anthropic",
        needle: "/usage-credits to finish what you",
        source: PatternSource::Captured,
    },
    // The headline the same state prints when the whole message is on screen.
    LimitPattern {
        provider: "anthropic",
        needle: "Claude usage limit reached",
        source: PatternSource::Reported,
    },
    // The Anthropic API's own refusal for an exhausted balance, which a
    // wrapper CLI surfaces verbatim.
    LimitPattern {
        provider: "anthropic",
        needle: "Your credit balance is too low",
        source: PatternSource::Reported,
    },
    // OpenRouter, key spend cap reached. Captured:
    // `fixtures/attention/openrouter-key-limit.txt`. pi and opencode both
    // surface OpenRouter's text verbatim, which is why the row is keyed on the
    // PROVIDER and not on the CLI that printed it.
    LimitPattern {
        provider: "openrouter",
        needle: "Key limit exceeded",
        source: PatternSource::Captured,
    },
    // OpenRouter, account credits exhausted. Captured (and WRAPPED mid-word):
    // `fixtures/attention/openrouter-credits-exhausted.txt`.
    LimitPattern {
        provider: "openrouter",
        needle: "This request would exceed your available credits",
        source: PatternSource::Captured,
    },
];

/// How many consecutive rendered lines a needle may be reassembled across.
///
/// Three, not two: the longest needle here is 48 characters, which fits on one
/// line of any pane wide enough to run an agent CLI, so two would already do —
/// the third is headroom for a narrow pane and for a longer needle a later row
/// adds, and it costs one more string join per candidate line.
pub const WRAP_WINDOW: usize = 3;

/// The glyphs a TUI draws as a left gutter in front of wrapped content. pi and
/// opencode use `U+2503`; the others are here so a future CLI's box style does
/// not need a code change to be seen through.
const GUTTER: &[char] = &['\u{2503}', '\u{2502}', '\u{2506}', '\u{254E}', '\u{2551}', '|'];

/// One rendered line with its indent and gutter removed. `\r` goes with the
/// trailing whitespace, so a fixture checked out CRLF — which is every text
/// fixture in this repo, since `.gitattributes` gives the attention fixtures no
/// `eol=lf` rule — reads exactly like the LF blob.
fn strip_gutter(line: &str) -> &str {
    line.trim_matches(|c: char| c.is_whitespace() || GUTTER.contains(&c))
}

/// Rejoin `lines[i..]` into one logical line, taking up to [`WRAP_WINDOW`]
/// non-empty stripped lines. A break that fell on a hyphen is joined with NO
/// separator (`in-` + `flight` -> `in-flight`); every other break becomes one
/// space.
fn rejoined(lines: &[&str], i: usize) -> String {
    let mut out = String::new();
    let mut taken = 0usize;
    for line in &lines[i..] {
        let s = strip_gutter(line);
        if s.is_empty() {
            // A blank gutter row is padding, not a break inside a paragraph:
            // skip it without spending a window slot once we already have
            // content, and never START a candidate on one.
            if taken > 0 {
                continue;
            }
            break;
        }
        if taken > 0 && !out.ends_with('-') {
            out.push(' ');
        }
        out.push_str(s);
        taken += 1;
        if taken == WRAP_WINDOW {
            break;
        }
    }
    out
}

/// The provider limit this pane tail is showing, if any.
///
/// `tail` is the ANSI-stripped pane tail the attention scan already reads
/// (`attention_tail` then `strip_ansi`); pass the LOOMUX-MASKED form where one
/// is available, so a relayed `[orrerix]` notice quoting a refusal cannot raise
/// the chip on the pane that merely received it.
///
/// Returns the FIRST matching row in [`LIMIT_PATTERNS`] order, scanning lines
/// from the top: which of two simultaneous refusals wins is not a question
/// worth a rule, and a stable answer is worth more than a clever one.
pub fn limit_in_tail(tail: &str) -> Option<&'static LimitPattern> {
    let lines: Vec<&str> = tail.lines().collect();
    for i in 0..lines.len() {
        if strip_gutter(lines[i]).is_empty() {
            continue;
        }
        let logical = strip_gutter(lines[i]).to_string(); // MUTATION M4: no reassembly
        if let Some(p) = LIMIT_PATTERNS.iter().find(|p| logical.starts_with(p.needle)) {
            return Some(p);
        }
    }
    None
}

/// The [`Provider`] row for an id, or `None` for an id no table row declares.
pub fn provider(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pattern_names_a_declared_provider() {
        // The two tables are a closed pair: a needle whose provider has no row
        // would raise a chip nothing can word.
        for p in LIMIT_PATTERNS {
            assert!(
                provider(p.provider).is_some(),
                "pattern {:?} names provider {:?}, which PROVIDERS does not declare",
                p.needle,
                p.provider
            );
        }
        // Non-vacuity: the loop really did see both providers, so an empty or
        // one-provider table cannot satisfy the assertion above.
        assert!(LIMIT_PATTERNS.iter().any(|p| p.provider == "anthropic"));
        assert!(LIMIT_PATTERNS.iter().any(|p| p.provider == "openrouter"));
        assert!(
            LIMIT_PATTERNS.len() >= 5,
            "only {} patterns scanned",
            LIMIT_PATTERNS.len()
        );
        // Every declared provider is reachable from the table, so a provider
        // row whose needles were all deleted fails here rather than becoming
        // dead vocabulary.
        for p in PROVIDERS {
            assert!(
                LIMIT_PATTERNS.iter().any(|l| l.provider == p.id),
                "provider {:?} has no pattern that can ever raise it",
                p.id
            );
        }
    }

    #[test]
    fn a_needle_reassembles_across_a_wrapped_gutter() {
        // The real shape, from `fixtures/attention/openrouter-credits-
        // exhausted.txt`: broken mid-word with pi's gutter re-drawn on the
        // continuation.
        let wrapped = "  \u{2503}\n  \u{2503}  This request would exceed your available credits given your current in-\n  \u{2503}  flight requests. Retry after in-flight requests settle, or add credits.\n  \u{2503}\n";
        assert_eq!(
            limit_in_tail(wrapped).map(|p| p.provider),
            Some("openrouter"),
            "a wrapped refusal must still be seen"
        );
        // The instrument's own control: the SAME text unwrapped. A `contains`
        // implementation passes this one and fails the one above, which is the
        // whole reason the reassembly exists.
        let flat = "This request would exceed your available credits given your current in-flight requests.";
        assert_eq!(limit_in_tail(flat).map(|p| p.provider), Some("openrouter"));
        // And the hyphen join really did rebuild the broken word rather than
        // leaving `in- flight`.
        let lines: Vec<&str> = wrapped.lines().collect();
        assert!(
            rejoined(&lines, 1).contains("in-flight requests"),
            "the wrap join must not leave a space inside the broken word: {}",
            rejoined(&lines, 1)
        );
    }

    #[test]
    fn a_quotation_of_a_refusal_is_not_a_refusal() {
        // The false positive that matters, in the orchestrator's own words
        // (this group's q-45). Mid-sentence, so not line-initial.
        let quoted_mid_line = "rev-2313 (PR #2677) got \"This request would exceed your available credits given your current in-flight requests.\"";
        assert_eq!(
            limit_in_tail(quoted_mid_line),
            None,
            "a refusal quoted mid-sentence must not raise a limit"
        );
        // THE RESIDUAL, pinned rather than implied: the line-initial anchor
        // cannot separate a pane's refusal from prose that OPENS with the same
        // words. This is what the loomux-notice mask (the caller's job) and the
        // once-per-group-per-provider dedup bound; the anchor alone does not.
        let prose_opening_with_a_needle =
            "Claude usage limit reached is what stopped every delegate — please top it up.";
        assert!(
            limit_in_tail(prose_opening_with_a_needle).is_some(),
            "disclosed blind spot: prose OPENING with a needle is indistinguishable here"
        );
    }

    #[test]
    fn ordinary_pane_chatter_raises_nothing() {
        assert_eq!(limit_in_tail(""), None);
        assert_eq!(limit_in_tail("Running cargo test...\n   Compiling loomux v0.2.0\n"), None);
        assert_eq!(
            limit_in_tail("I checked the key limit and it is fine; credits are healthy."),
            None
        );
        // A needle's own words, scattered but never opening a line.
        assert_eq!(
            limit_in_tail("The key limit exceeded nothing today.\nCredits: fine.\n"),
            None
        );
    }
}
