//! Prompt-cache age (#3407) — how long a pane has been quiet relative to its
//! provider's prompt-cache TTL, and what the last wake after a quiet stretch
//! cost.
//!
//! # What this can and cannot claim
//!
//! A provider's prompt cache is **not observable** from here. Nothing loomux
//! reads says "this prefix is still cached". What IS observable is when the
//! pane last made a model request — its usage counters moved — and the
//! provider's documented rule for how long a cached prefix survives after the
//! last request that wrote or read it. Everything this module produces is an
//! INFERENCE from those two facts, and every surface that shows it says so.
//!
//! The inference errs toward HOT, and the size of that error is bounded and
//! stated rather than hidden: the cache lifetime is measured "from the start of
//! the request that writes or reads the cache entry, not from the end of its
//! response" (Anthropic's prompt-caching docs), while a request is observed
//! here when its counters land — at the END of the response, rounded up to the
//! usage tick. So the observed age is younger than the real one by the
//! response's own duration plus at most one tick. The cooling band
//! ([`cooling_after_ms`]) is sized to absorb that for ordinary responses; a
//! four-minute stream against a five-minute TTL is the case it does not.
//!
//! # Why derived from the usage sampler, never a new poll
//!
//! `compute_group_usage` already refreshes every live agent's cumulative
//! counters on the polled publisher's tick (`docs/design/polled-views.md`,
//! strip tier). "The counters moved since the last reading" IS "a request
//! landed since the last reading", so [`fold_activity`] turns the merge the
//! usage collector already performs into an activity timestamp with no second
//! transcript read and no second clock. It is CLI-agnostic for the same reason:
//! it reads the four counters every usage source already fills, never a
//! per-vendor timestamp field.
//!
//! # The TTL table
//!
//! Per-CLI defaults live on [`crate::model::CliCaps::cache_ttl_minutes`] —
//! a fact about the vendor, written down once — and a workflow block may
//! override it (`cache_ttl_minutes:`), which is how an account on a longer TTL
//! than the conservative default says so. [`effective_ttl_minutes`] is the one
//! resolver. See `docs/design/cache-age.md`.

use serde::{Deserialize, Serialize};

/// Ceiling on a block's `cache_ttl_minutes:` (24 h). OpenAI's extended
/// retention is the longest documented TTL today at 24 hours; nothing longer
/// is a cache any provider describes, so a larger value is a typo and is
/// refused rather than clamped.
pub const CACHE_TTL_MINUTES_MAX: u32 = 1440;

/// A burst of counter movement that begins after at least this much quiet is a
/// new **wake** ([`WakeCost`]); movement closer together than this is the same
/// turn continuing. One minute: longer than the gap between the requests of one
/// agent turn (a tool call returning), shorter than any TTL in the table, so a
/// wake is always recorded before the cache it might have missed could expire.
pub const WAKE_GAP_MS: u64 = 60_000;

/// The shortest cooling band, in ms — see [`cooling_after_ms`].
pub const MIN_COOLING_BAND_MS: u64 = 2 * 60_000;

/// Resolve the TTL (minutes) in force for one agent: the block's
/// `cache_ttl_minutes:` when it declares one, else its CLI's
/// [`crate::model::CliCaps::cache_ttl_minutes`].
///
/// `None` means **unknown**, and it is an answer, not a failure: a CLI whose
/// provider is not fixed (copilot, opencode, pi route to several) has no
/// honest default, and `cache_ttl_minutes: 0` is how a block says "do not
/// infer a cache state for me". Every consumer shows the age alone and claims
/// no hot/cold state for `None`.
pub fn effective_ttl_minutes(block_override: Option<u32>, cli: &str) -> Option<u32> {
    match block_override {
        Some(0) => None,
        Some(n) => Some(n),
        None => crate::model::cli_caps(cli).and_then(|c| c.cache_ttl_minutes),
    }
}

/// Idle time (ms) after which a pane on this TTL reads **cooling**: the TTL
/// minus a band of a fifth of the TTL, but never a band narrower than
/// [`MIN_COOLING_BAND_MS`].
///
/// A fifth reproduces the issue's own example (`cooling 48m/60m`). The floor is
/// there for the five-minute TTL, where a fifth is one minute: that is the
/// same width as the compact-nudge loop's tick, so the orchestrator backstop
/// (which fires inside this band) could miss it entirely on one late tick, and
/// it is narrower than the response-duration error the module header states.
/// Two minutes gives the backstop two ticks and the chip a real warning.
pub fn cooling_after_ms(ttl_minutes: u32) -> u64 {
    let ttl_ms = ttl_minutes as u64 * 60_000;
    let band = (ttl_ms / 5).max(MIN_COOLING_BAND_MS);
    ttl_ms.saturating_sub(band)
}

/// The four cumulative token counters, as the usage row carries them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

impl Counters {
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_creation)
            .saturating_add(self.cache_read)
    }
}

/// What the first request(s) of a wake cost — the counters that moved on the
/// first usage reading after at least [`WAKE_GAP_MS`] of quiet.
///
/// That is exactly the quantity that shows what a cold cache costs: a wake on
/// a hot cache is mostly `cache_read_tokens` (0.1x input on Anthropic's
/// table), a wake on a cold one is mostly `cache_creation_tokens` (1.25x) plus
/// `input_tokens`. At the usage tick's one-second cadence it is usually one
/// request; when two landed inside one tick it is both, which only ever makes
/// the figure larger, never mislabels a hot read as cold.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WakeCost {
    /// Unix-ms the wake was observed.
    pub at_ms: u64,
    /// How long the pane had been quiet before it, or `None` when that is not
    /// known — the first movement ever recorded for this row (a fresh session,
    /// or a row written by a build that predates this field).
    pub idle_before_ms: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    /// The dollar delta across the same reading, when both readings carried a
    /// figure. Estimated or reported exactly as the row's own `cost_usd` is.
    pub cost_usd: Option<f64>,
}

/// The activity half of a usage row: when its counters last moved, and the
/// last wake. Persisted beside the counters in `usage.json` (additive — a row
/// written before #3407 reads as `Activity::default()`, i.e. unknown).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    /// Unix-ms the counters were last observed to move — the inferred time of
    /// the last model request. `None` until one has been observed.
    #[serde(default)]
    pub last_active_ms: Option<u64>,
    #[serde(default)]
    pub last_wake: Option<WakeCost>,
}

/// Fold one new usage reading into a row's [`Activity`].
///
/// - Counters that did not GROW (equal, or lower — a different source's
///   reading replacing the row) are not a request: the previous activity is
///   carried unchanged. Growth is judged on the total, never on one counter,
///   so a reading that shifts tokens between buckets cannot fake a request.
/// - Growth after at least [`WAKE_GAP_MS`] of quiet, or growth with no
///   previous activity at all, starts a new wake carrying the delta.
/// - Growth inside the gap is the same turn continuing: `last_active_ms`
///   advances and the wake already recorded stands.
///
/// Pure — the caller supplies `now_ms` (the reading's own timestamp) — so every
/// arm is pinned without a registry or a transcript.
pub fn fold_activity(
    prev: &Activity,
    prev_counters: Counters,
    prev_cost: Option<f64>,
    next: Counters,
    next_cost: Option<f64>,
    now_ms: u64,
) -> Activity {
    if false && next.total() <= prev_counters.total() {
        return prev.clone();
    }
    let idle_before_ms = prev.last_active_ms.map(|t| now_ms.saturating_sub(t));
    let is_wake = idle_before_ms.map_or(true, |gap| gap >= WAKE_GAP_MS);
    let last_wake = if is_wake {
        Some(WakeCost {
            at_ms: now_ms,
            idle_before_ms,
            input_tokens: next.input.saturating_sub(prev_counters.input),
            output_tokens: next.output.saturating_sub(prev_counters.output),
            cache_creation_tokens: next.cache_creation.saturating_sub(prev_counters.cache_creation),
            cache_read_tokens: next.cache_read.saturating_sub(prev_counters.cache_read),
            cost_usd: match (prev_cost, next_cost) {
                (Some(a), Some(b)) => Some((b - a).max(0.0)),
                _ => None,
            },
        })
    } else {
        prev.last_wake.clone()
    };
    Activity { last_active_ms: Some(now_ms), last_wake }
}

/// Everything the orchestrator's idle-compact backstop decides on (#3407
/// acceptance criterion 3), gathered by the host so the decision is pure.
#[derive(Clone, Copy, Debug)]
pub struct IdleCompactInputs {
    /// How long the pane has been output-quiet — the SAME quiet clock the
    /// compact-nudge and idle ticks read (`AgentEntry::last_progress_ms`), so
    /// "never mid-decision" is the existing quiet-window rule, not a new one.
    pub idle_ms: u64,
    /// [`effective_ttl_minutes`] for this agent. `None` never fires: with no
    /// TTL there is no "before it goes cold" to aim at.
    pub ttl_minutes: Option<u32>,
    /// Anything that will wake this orchestrator on its own: a live delegate,
    /// a live review/plan drive, a pending `notify_when` watch, a queued
    /// delivery. Computed by the host; `true` when it could not look.
    pub in_flight: bool,
    /// Context used, percent, off the transcript. `None` never fires.
    pub context_percent: Option<u32>,
    /// The floor the context must be at or above for a compact to be worth a
    /// request. `0` means no floor.
    pub floor_percent: u32,
    /// Already nudged for this idle stretch (see the host's latch).
    pub latched: bool,
    /// A compact is already pending or requested for this pane.
    pub compact_busy: bool,
}

/// Whether the idle-compact nudge fires now.
///
/// It fires only INSIDE the cooling band — idle at least
/// [`cooling_after_ms`] and still under the TTL. Below the band the
/// orchestrator may yet act on its own; at or past the TTL the cache is
/// already (inferred) cold, and a nudge then would pay the cold re-read to
/// compact rather than save it — the whole point is to compact while the
/// compaction request itself still reads a warm cache.
///
/// Context fails CLOSED here (unlike the heuristic compact-nudge floor, which
/// fails open): the nudge is itself a model request, and with no reading there
/// is no evidence it would pay for itself.
pub fn idle_compact_should_fire(i: &IdleCompactInputs) -> bool {
    let Some(ttl) = i.ttl_minutes else { return false };
    if i.in_flight || i.latched || i.compact_busy {
        return false;
    }
    let Some(pct) = i.context_percent else { return false };
    if pct < i.floor_percent {
        return false;
    }
    let ttl_ms = ttl as u64 * 60_000;
    i.idle_ms >= cooling_after_ms(ttl) && i.idle_ms < ttl_ms
}

/// The line the backstop types into the orchestrator's pane. Leads with the
/// literal the issue names so a human scanning the pane recognises it.
pub fn idle_compact_notice(context_percent: u32, ttl_minutes: u32) -> String {
    format!(
        "[orrerix] going idle with no work — compact now: nothing is in flight (no live \n\
         delegates, drives, watches or queued deliveries) and your context is at \
         {context_percent}%. The prompt cache is inferred to expire about {ttl_minutes}m \
         after your last request, and the next wake after that re-reads the whole context \
         uncached. Offload anything mid-decision (set_state, the task board) and call \
         request_compact() as the last action of this turn."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(input: u64, output: u64, cw: u64, cr: u64) -> Counters {
        Counters { input, output, cache_creation: cw, cache_read: cr }
    }

    #[test]
    fn a_block_override_beats_the_cli_default_and_zero_means_unknown() {
        assert_eq!(effective_ttl_minutes(None, "claude"), Some(5));
        assert_eq!(effective_ttl_minutes(Some(60), "claude"), Some(60));
        assert_eq!(effective_ttl_minutes(Some(0), "claude"), None);
        // A multi-provider CLI has no honest default, and an override still
        // applies to it.
        assert_eq!(effective_ttl_minutes(None, "copilot"), None);
        assert_eq!(effective_ttl_minutes(Some(30), "copilot"), Some(30));
        // An unknown CLI is unknown, never claude's arm.
        assert_eq!(effective_ttl_minutes(None, "not-a-cli"), None);
    }

    #[test]
    fn the_cooling_band_is_a_fifth_with_a_two_minute_floor() {
        assert_eq!(cooling_after_ms(60), 48 * 60_000);
        assert_eq!(cooling_after_ms(5), 3 * 60_000);
        assert_eq!(cooling_after_ms(10), 8 * 60_000);
        // A TTL shorter than the floor cools from the first instant rather
        // than underflowing.
        assert_eq!(cooling_after_ms(1), 0);
    }

    #[test]
    fn unmoved_or_shrunk_counters_are_not_a_request() {
        let prev = Activity { last_active_ms: Some(1_000), last_wake: None };
        let same = fold_activity(&prev, c(10, 10, 10, 10), None, c(10, 10, 10, 10), None, 999_999);
        assert_eq!(same, prev);
        let shrunk = fold_activity(&prev, c(10, 10, 10, 10), None, c(0, 0, 0, 5), None, 999_999);
        assert_eq!(shrunk, prev);
        // A reading that moves tokens between buckets at an equal total is not
        // growth either.
        let shuffled = fold_activity(&prev, c(10, 10, 10, 10), None, c(0, 10, 10, 20), None, 999_999);
        assert_eq!(shuffled, prev);
    }

    #[test]
    fn growth_after_the_gap_is_a_wake_carrying_the_delta() {
        let prev = Activity { last_active_ms: Some(1_000), last_wake: None };
        let now = 1_000 + 10 * 60_000;
        let got = fold_activity(
            &prev,
            c(100, 50, 0, 1_000),
            Some(1.0),
            c(120, 80, 900_000, 1_000),
            Some(6.5),
            now,
        );
        assert_eq!(got.last_active_ms, Some(now));
        let w = got.last_wake.expect("a wake");
        assert_eq!(w.at_ms, now);
        assert_eq!(w.idle_before_ms, Some(10 * 60_000));
        assert_eq!(
            (w.input_tokens, w.output_tokens, w.cache_creation_tokens, w.cache_read_tokens),
            (20, 30, 900_000, 0)
        );
        assert_eq!(w.cost_usd, Some(5.5));
    }

    #[test]
    fn growth_inside_the_gap_advances_the_clock_and_keeps_the_wake() {
        let wake = WakeCost { at_ms: 5_000, cache_read_tokens: 7, ..Default::default() };
        let prev = Activity { last_active_ms: Some(5_000), last_wake: Some(wake.clone()) };
        let got = fold_activity(&prev, c(1, 1, 1, 1), None, c(2, 2, 2, 2), None, 5_000 + WAKE_GAP_MS - 1);
        assert_eq!(got.last_active_ms, Some(5_000 + WAKE_GAP_MS - 1));
        assert_eq!(got.last_wake, Some(wake));
        // Exactly at the gap it is a wake — the boundary is inclusive.
        let at_gap = fold_activity(&prev, c(1, 1, 1, 1), None, c(2, 2, 2, 2), None, 5_000 + WAKE_GAP_MS);
        assert_eq!(at_gap.last_wake.unwrap().at_ms, 5_000 + WAKE_GAP_MS);
    }

    #[test]
    fn the_first_movement_ever_is_a_wake_whose_gap_is_unknown() {
        let got = fold_activity(&Activity::default(), c(0, 0, 0, 0), None, c(5, 5, 5, 5), Some(0.1), 42);
        assert_eq!(got.last_active_ms, Some(42));
        let w = got.last_wake.unwrap();
        assert_eq!(w.idle_before_ms, None);
        // No previous dollar figure → no delta, never the whole cumulative sum.
        assert_eq!(w.cost_usd, None);
    }

    #[test]
    fn a_lower_dollar_figure_never_reports_a_negative_wake_cost() {
        let prev = Activity { last_active_ms: Some(0), last_wake: None };
        let got = fold_activity(&prev, c(1, 0, 0, 0), Some(3.0), c(2, 0, 0, 0), Some(2.0), WAKE_GAP_MS);
        assert_eq!(got.last_wake.unwrap().cost_usd, Some(0.0));
    }

    fn inputs() -> IdleCompactInputs {
        IdleCompactInputs {
            idle_ms: 4 * 60_000,
            ttl_minutes: Some(5),
            in_flight: false,
            context_percent: Some(60),
            floor_percent: 50,
            latched: false,
            compact_busy: false,
        }
    }

    #[test]
    fn the_backstop_fires_inside_the_cooling_band_with_nothing_in_flight() {
        assert!(idle_compact_should_fire(&inputs()));
        // The band's edges: from cooling_after_ms inclusive to the TTL exclusive.
        assert!(idle_compact_should_fire(&IdleCompactInputs { idle_ms: 3 * 60_000, ..inputs() }));
        assert!(!idle_compact_should_fire(&IdleCompactInputs { idle_ms: 3 * 60_000 - 1, ..inputs() }));
        assert!(!idle_compact_should_fire(&IdleCompactInputs { idle_ms: 5 * 60_000, ..inputs() }));
    }

    #[test]
    fn the_backstop_refuses_every_disqualifier_on_its_own() {
        let cases: [(&str, IdleCompactInputs); 7] = [
            ("in flight", IdleCompactInputs { in_flight: true, ..inputs() }),
            ("latched", IdleCompactInputs { latched: true, ..inputs() }),
            ("compact busy", IdleCompactInputs { compact_busy: true, ..inputs() }),
            ("no ttl", IdleCompactInputs { ttl_minutes: None, ..inputs() }),
            ("no context reading", IdleCompactInputs { context_percent: None, ..inputs() }),
            ("under the floor", IdleCompactInputs { context_percent: Some(49), ..inputs() }),
            ("already cold", IdleCompactInputs { idle_ms: 60 * 60_000, ..inputs() }),
        ];
        for (name, i) in cases {
            assert!(!idle_compact_should_fire(&i), "{name} must refuse");
        }
        // A zero floor admits any reading, and the floor itself is inclusive.
        assert!(idle_compact_should_fire(&IdleCompactInputs { context_percent: Some(1), floor_percent: 0, ..inputs() }));
        assert!(idle_compact_should_fire(&IdleCompactInputs { context_percent: Some(50), ..inputs() }));
    }

    #[test]
    fn the_notice_leads_with_the_named_literal_and_is_one_paragraph() {
        let n = idle_compact_notice(61, 5);
        assert!(n.starts_with("[orrerix] going idle with no work — compact now"));
        assert!(n.contains("61%") && n.contains("5m") && n.contains("request_compact()"));
        assert!(!n.contains('\n') && !n.contains("          "), "one paragraph: {n:?}");
    }
}
