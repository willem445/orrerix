//! Per-session token usage and dollar cost, read from each agent CLI's own
//! transcript records rather than scraped from the pane statusline.
//!
//! # Source of truth per CLI (and its limits)
//!
//! **Claude Code** writes one JSONL line per message into
//! `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`. Each assistant
//! message carries an exact `usage` object (`input_tokens`, `output_tokens`,
//! `cache_creation_input_tokens`, `cache_read_input_tokens`) and the `model`
//! that produced it. We sum those — deduplicating by message id so a resumed
//! or replayed transcript isn't double-counted — and derive dollars from a
//! small, dated price table (`price_for`). Token counts are therefore *exact*;
//! the dollar figure is an *estimate* (subscription/Max accounts pay no
//! marginal dollar cost at all, so the statusline shows `$0.00` regardless of
//! real usage — tokens are the honest metric there).
//!
//! **Copilot CLI** keeps only `session-state/<id>/workspace.yaml`, which
//! records no token counts we can read today. So copilot sessions have no
//! transcript usage source; the orchestration layer falls back to the
//! last-resort statusline parse for them. If a future copilot build writes a
//! usage record, add a `copilot_session_usage` reader here and it slots in
//! ahead of the fallback with no other changes.
//!
//! **OpenCode** writes no transcript file at all: sessions, messages and parts
//! live in one SQLite database, and the `session` row itself already carries
//! the dollar cost OpenCode computed *and* five token counters. So
//! `opencode_session_usage` reads one row instead of folding message records —
//! and the dollars are **reported by the CLI, not estimated here**, which is
//! why OpenCode needs no `price_for` entry. The read itself lives in
//! `crate::opencodedb`; this module only maps its shape onto
//! [`SessionUsage`], which is the lossy half and belongs where the other CLIs'
//! mappings are.
//!
//! Everything here is best-effort and pure where it matters: the parser
//! (`parse_claude_transcript`) takes text and is exercised by fixture tests,
//! never a live CLI.

use loomux_engine::obs::LockExt;
use loomux_engine::pathseg::PathSegment;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use loomux_engine::lockwatch::TrackedMutex;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Exact token counts for a session, split by kind so the UI can show tokens
/// even when no dollar figure is available.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
}

impl TokenUsage {
    /// Every token the session touched — the headline "tokens" figure.
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_creation_tokens + self.cache_read_tokens
    }
}

/// One session's usage, tokens plus a best-effort dollar estimate.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SessionUsage {
    pub tokens: TokenUsage,
    /// Dollar cost derived from the price table, or `None` when no message in
    /// the transcript used a model we have a price for (token display only).
    pub cost_usd: Option<f64>,
    /// The model the cost was priced against (the one with the most output
    /// tokens), for display and debugging. `None` when unpriced.
    ///
    /// On claude this is a PRICING pick, not "the model this pane is on": a
    /// session that switched models keeps naming the old one until a message
    /// on the new one out-writes the largest message on the old — possibly
    /// never. pi, codex and opencode fill it with the latest turn's model, so
    /// the two readings coincide there. Anything asking "which model now?"
    /// reads [`SessionUsage::current_model`] instead (#3415).
    pub model: Option<String>,
    /// The model of the LATEST counted turn — "which model is this pane on",
    /// priced or not. This is what the usage series samples carry, so a
    /// token chart can split spend by model and mark a mid-session switch at
    /// the tick it happened (#3415). Equal to `model` on every CLI whose
    /// `model` is already last-turn; differs from it only on claude, where
    /// `model` is the pricing pick described above.
    pub current_model: Option<String>,
    /// The context the session's FIRST counted turn was sent — its fresh
    /// input, cache-written and cache-read tokens together (#3831). That is
    /// what starting this agent cost before it had any history: the CLI's
    /// system prompt and tools, the repo's instruction files, and the first
    /// prompt, measured on this machine, repo and model rather than assumed.
    /// The next-prompt estimate prices "the same prompt in a fresh agent" off
    /// it. `None` where the source has no per-turn record (opencode's session
    /// row is a running total) or no turn has been counted yet.
    pub first_context_tokens: Option<u64>,
    /// The prompt-cache lifetime (minutes) this session's own records show its
    /// LAST cache write was made with, where the CLI records one (#3831).
    /// `None` on every source that does not — which is every one but claude's
    /// transcript today. The middle rung of
    /// `loomux_engine::cacheage::resolve_ttl`.
    pub detected_cache_ttl_minutes: Option<u32>,
}

/// The cache-write buckets a Claude Code transcript's `usage.cache_creation`
/// object carries, with the lifetime each one names, SHORTEST FIRST.
///
/// The field names are the Messages API's own (prompt-caching reference,
/// fetched 2026-10-09): the response `usage` carries
/// `"cache_creation": { "ephemeral_5m_input_tokens": …,
/// "ephemeral_1h_input_tokens": … }`, and "the current
/// `cache_creation_input_tokens` field equals the sum of the values in the
/// `cache_creation` object". That Claude Code copies the object into its
/// transcript unchanged is an OBSERVATION, not a documented contract: read off
/// 1,219 assistant records written by Claude Code 2.1.284 to 2.1.295 on this
/// machine, every one of which carried both keys, summing to the total.
///
/// Shortest first because that is the order [`claude_write_ttl`] decides in.
const CLAUDE_CACHE_BUCKETS: [(&str, u32); 2] =
    [("ephemeral_5m_input_tokens", 5), ("ephemeral_1h_input_tokens", 60)];

/// The cache lifetime (minutes) ONE request wrote with, or `None` when it
/// wrote nothing to the cache or its record does not say into which bucket.
///
/// **Within one request the shorter bucket wins.** A request may write both:
/// the API allows it on the condition that "cache entries with longer TTL must
/// appear before shorter TTLs", so in a mixed request the long-lived entry is
/// the front of the prompt and the short-lived one is the conversation's tail.
/// When the tail expires, the next request re-writes it whatever the front
/// did, and the chip's one forbidden answer is `hot` over a cache that is
/// gone — so the request's lifetime is its shortest.
fn claude_write_ttl(usage: &Value) -> Option<u32> {
    let buckets = usage.get("cache_creation")?;
    CLAUDE_CACHE_BUCKETS.iter().rev().find(|(key, _)| u64_field(buckets, key) > 0).map(|(_, minutes)| *minutes)
}

/// How many of one request's cache-written tokens went to the 1-hour cache —
/// what [`cost_of_split`] prices at the 1-hour rate. Zero when the record has
/// no bucket object, which prices the whole write at the 5-minute rate exactly
/// as before #3831.
fn claude_write_1h_tokens(usage: &Value) -> u64 {
    usage.get("cache_creation").map(|b| u64_field(b, "ephemeral_1h_input_tokens")).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Price table
// ---------------------------------------------------------------------------

/// USD per **one million** tokens for one model, at one prompt-length tier.
///
/// Field order here is not the vendor table's; [`mp`] takes the five numbers in
/// the table's own column order so a row can be checked against the page by
/// eye.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    /// Writing a prefix to the **5-minute** cache.
    pub cache_write: f64,
    /// Writing a prefix to the **1-hour** cache (#3831). Before this field
    /// existed a session's cost was priced at the 5-minute rate whichever
    /// cache it wrote to, which under-reported every 1-hour account.
    pub cache_write_1h: f64,
    /// A cache hit, which also refreshes the entry.
    pub cache_read: f64,
}

/// The day [`PRICE_ROWS`] was read off [`PRICE_TABLE_SOURCE`]. It travels on
/// every usage row that carries a price, so no surface can show a dollar
/// figure without the date it was true on.
pub const PRICE_TABLE_DATED: &str = "2026-10-09";

/// Where [`PRICE_ROWS`] came from: Anthropic's published pricing page, "Model
/// pricing" table. To update: re-read the page, change the rows and
/// [`PRICE_TABLE_DATED`] together, and re-read the three rules stated under the
/// table (the cache multipliers, the Haiku 5.5 prompt-length tier, and which
/// models the newer tokenizer covers) — `docs/design/prompt-cost.md` quotes
/// them as fetched.
pub const PRICE_TABLE_SOURCE: &str = "https://platform.claude.com/docs/en/about-claude/pricing";

/// A model family, as the id spells it. The match is a substring of the id —
/// `claude-opus-4-8`, `us.anthropic.claude-opus-4-8-v1:0` and
/// `claude-opus-4-8[1m]` are all Opus — which is the matching this table has
/// always done; what #3831 adds is the VERSION beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Fable,
    Mythos,
    Opus,
    Sonnet,
    Haiku,
}

const FAMILIES: [(&str, Family); 5] = [
    ("fable", Family::Fable),
    ("mythos", Family::Mythos),
    ("opus", Family::Opus),
    ("sonnet", Family::Sonnet),
    ("haiku", Family::Haiku),
];

/// One row of the vendor's table.
struct PriceRow {
    family: Family,
    version: (u32, u32),
    /// The page marks the row retired. A retired row still prices its own
    /// exact version — an old transcript can carry it — but is left out of the
    /// family ceiling an UNKNOWN version takes ([`family_ceiling`]).
    retired: bool,
    price: ModelPrice,
    /// A second, higher price for a request whose prompt is longer than the
    /// threshold (tokens). One model has one today.
    long_prompt: Option<(u64, ModelPrice)>,
}

/// One price, the five numbers in the vendor table's column order: base input,
/// 5-minute cache write, 1-hour cache write, cache hit, output.
const fn mp(input: f64, cache_write: f64, cache_write_1h: f64, cache_read: f64, output: f64) -> ModelPrice {
    ModelPrice { input, output, cache_write, cache_write_1h, cache_read }
}

const fn row(family: Family, major: u32, minor: u32, price: ModelPrice) -> PriceRow {
    PriceRow { family, version: (major, minor), retired: false, price, long_prompt: None }
}

const fn retired(family: Family, major: u32, minor: u32, price: ModelPrice) -> PriceRow {
    PriceRow { family, version: (major, minor), retired: true, price, long_prompt: None }
}

/// Model prices in USD per 1M tokens, transcribed from [`PRICE_TABLE_SOURCE`]
/// on [`PRICE_TABLE_DATED`], one row per row of that table and in its order.
///
/// Every row is the page's own five numbers rather than a base price and a
/// multiplier, so a row is checked by reading across. The multipliers the page
/// states — 5-minute write 1.25x, 1-hour write 2x, and a cache hit at 0.1x, or
/// 0.05x on Opus 5.5 and Sonnet 5.5, or 0.025x on Fable 5.1 and Mythos 5.1 —
/// are asserted over the whole table by
/// `every_price_row_obeys_the_multipliers_the_vendor_states`, which is what
/// catches a mistyped digit in one of the three derived columns.
const PRICE_ROWS: &[PriceRow] = &[
    row(Family::Fable, 5, 1, mp(10.0, 12.50, 20.0, 0.25, 50.0)),
    row(Family::Mythos, 5, 1, mp(10.0, 12.50, 20.0, 0.25, 50.0)),
    row(Family::Fable, 5, 0, mp(10.0, 12.50, 20.0, 1.0, 50.0)),
    row(Family::Mythos, 5, 0, mp(10.0, 12.50, 20.0, 1.0, 50.0)),
    row(Family::Opus, 5, 5, mp(4.0, 5.0, 8.0, 0.20, 20.0)),
    row(Family::Opus, 5, 0, mp(5.0, 6.25, 10.0, 0.50, 25.0)),
    row(Family::Opus, 4, 8, mp(5.0, 6.25, 10.0, 0.50, 25.0)),
    row(Family::Opus, 4, 7, mp(5.0, 6.25, 10.0, 0.50, 25.0)),
    row(Family::Opus, 4, 6, mp(5.0, 6.25, 10.0, 0.50, 25.0)),
    row(Family::Opus, 4, 5, mp(5.0, 6.25, 10.0, 0.50, 25.0)),
    retired(Family::Opus, 4, 1, mp(15.0, 18.75, 30.0, 1.50, 75.0)),
    retired(Family::Opus, 4, 0, mp(15.0, 18.75, 30.0, 1.50, 75.0)),
    row(Family::Sonnet, 5, 5, mp(2.0, 2.50, 4.0, 0.10, 10.0)),
    // The page's footnote 3: the $2/$10 launch price "is now the standard
    // price"; the increase to $3/$15 "will not occur".
    row(Family::Sonnet, 5, 0, mp(2.0, 2.50, 4.0, 0.20, 10.0)),
    row(Family::Sonnet, 4, 6, mp(3.0, 3.75, 6.0, 0.30, 15.0)),
    row(Family::Sonnet, 4, 5, mp(3.0, 3.75, 6.0, 0.30, 15.0)),
    retired(Family::Sonnet, 4, 0, mp(3.0, 3.75, 6.0, 0.30, 15.0)),
    // Two rows on the page: "for prompts up to 100,000 tokens" and "over".
    PriceRow {
        family: Family::Haiku,
        version: (5, 5),
        retired: false,
        price: mp(0.10, 0.125, 0.20, 0.01, 0.50),
        long_prompt: Some((100_000, mp(0.50, 0.625, 1.0, 0.05, 2.50))),
    },
    row(Family::Haiku, 4, 5, mp(1.0, 1.25, 2.0, 0.10, 5.0)),
    retired(Family::Haiku, 3, 5, mp(0.80, 1.0, 1.60, 0.08, 4.0)),
];

/// English characters one token stands for on the tokenizer the models before
/// Claude 4.7 use. The vendor's glossary, "Tokens", as fetched on
/// [`PRICE_TABLE_DATED`]: on earlier models "a token represents approximately
/// 3.5 English characters".
const CHARS_PER_TOKEN_EARLIER: f64 = 3.5;

/// How many more tokens the newer tokenizer produces for the same text. Same
/// glossary entry: "Claude 4.7 and later models and Claude Mythos Preview use a
/// newer tokenizer that produces approximately 30 percent more tokens for the
/// same text than earlier models".
const NEWER_TOKENIZER_FACTOR: f64 = 1.3;

/// The first version on the newer tokenizer.
const NEWER_TOKENIZER_FROM: (u32, u32) = (4, 7);

/// How a [`PriceQuote`] was reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PriceBasis {
    /// The id names a family AND a version the table lists.
    Listed,
    /// The id names a family the table knows at a version it does not, so the
    /// quote is that family's ceiling ([`family_ceiling`]) and may be high.
    FamilyCeiling,
}

/// A request longer than `over_tokens` pays `price` instead of the base price.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct LongPromptPrice {
    pub over_tokens: u64,
    pub price: ModelPrice,
}

/// Everything the table says about one model id.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PriceQuote {
    pub price: ModelPrice,
    pub long_prompt: Option<LongPromptPrice>,
    pub basis: PriceBasis,
    /// English characters per token on this model's tokenizer — what turns
    /// typed text into an approximate token count without a tokenizer
    /// (`src/promptcost.ts`). A fact about the model, so it is resolved here
    /// beside the price and the frontend keeps no model table of its own.
    pub chars_per_token: f64,
}

impl PriceQuote {
    /// The price a request of `prompt_tokens` pays — where a prompt's length is
    /// "all of its input tokens, including cache reads and cache writes" (the
    /// page's "Long context pricing"), and "over" is strict.
    pub fn at(&self, prompt_tokens: u64) -> ModelPrice {
        match self.long_prompt {
            Some(long) if prompt_tokens > long.over_tokens => long.price,
            _ => self.price,
        }
    }
}

/// The family and version a model id names.
///
/// The version is the one or two SHORT numeric segments beside the family
/// word: after it in every id since Claude 4 (`claude-opus-4-8`,
/// `claude-sonnet-4-5-20250929`), before it in the older spelling
/// (`claude-3-5-haiku-20241022`). A segment longer than two digits is a date,
/// never a version, so `claude-opus-4-20250514` is 4.0 and not 4.20250514; a
/// suffix such as `[1m]` or `-v1:0` is not all digits and ends the version.
/// `None` for the version when the id carries none this can read.
fn parse_model(model: &str) -> Option<(Family, Option<(u32, u32)>)> {
    let m = model.to_ascii_lowercase();
    let segments: Vec<&str> =
        m.split(|c: char| !c.is_ascii_alphanumeric()).filter(|s| !s.is_empty()).collect();
    // A family word is matched as a substring of a segment, as it always has
    // been, so an id that glues it to something else still resolves.
    let (at, family) = segments.iter().enumerate().find_map(|(i, seg)| {
        FAMILIES.iter().find(|(word, _)| seg.contains(*word)).map(|(_, f)| (i, *f))
    })?;
    fn short_number(s: &str) -> Option<u32> {
        if s.len() <= 2 && s.chars().all(|c| c.is_ascii_digit()) {
            s.parse().ok()
        } else {
            None
        }
    }
    let after: Vec<u32> = segments[at + 1..].iter().map_while(|s| short_number(s)).take(2).collect();
    let version = if let Some(major) = after.first() {
        Some((*major, after.get(1).copied().unwrap_or(0)))
    } else {
        let mut before: Vec<u32> =
            segments[..at].iter().rev().map_while(|s| short_number(s)).take(2).collect();
        before.reverse();
        before.first().map(|major| (*major, before.get(1).copied().unwrap_or(0)))
    };
    Some((family, version))
}

/// The price an UNKNOWN version of a known family takes: column by column, the
/// highest figure among that family's rows the page does not mark retired.
///
/// **Why the highest.** The estimate's posture has always been that it never
/// under-reports, and an id this table has not caught up with is far likelier
/// a model released since [`PRICE_TABLE_DATED`] than an old one.
///
/// **Why not the retired rows.** Opus 4.1 lists at three times any current
/// Opus. Folding it in would triple the figure for every new Opus until
/// someone updated this table — a wrong number in the other direction, and one
/// that would read as a real cost. A retired version an old transcript really
/// does carry is still priced exactly, by its own row.
///
/// **Per column, not per row**, because no single row is highest everywhere:
/// Fable 5 and 5.1 share an input price and differ fourfold on a cache hit.
///
/// The long-prompt tier is NOT carried up: its threshold is a fact about one
/// model, and the ceiling is already above both of that model's tiers.
fn family_ceiling(family: Family) -> Option<ModelPrice> {
    PRICE_ROWS.iter().filter(|r| r.family == family && !r.retired).map(|r| r.price).reduce(|a, b| {
        ModelPrice {
            input: a.input.max(b.input),
            output: a.output.max(b.output),
            cache_write: a.cache_write.max(b.cache_write),
            cache_write_1h: a.cache_write_1h.max(b.cache_write_1h),
            cache_read: a.cache_read.max(b.cache_read),
        }
    })
}

/// Everything the price table says about `model`, or `None` for a model whose
/// family it does not list — which falls back to token-only display on every
/// surface. A listed version gets its own row; an unlisted version of a listed
/// family gets [`family_ceiling`] and says so in `basis`.
///
/// **A model id from another vendor's CLI is not this function's to price.**
/// It answers for the id it is handed, and an id such as
/// `openrouter/anthropic/claude-sonnet-4.5` does name a family. Whether a row
/// is priced off this table at all is decided by the caller, on the row's own
/// provenance — see `OrchRegistry::compute_group_usage`.
pub fn price_quote(model: &str) -> Option<PriceQuote> {
    let (family, version) = parse_model(model)?;
    let listed = version.and_then(|v| PRICE_ROWS.iter().find(|r| r.family == family && r.version == v));
    // An unreadable or unlisted version is assumed to be on the newer
    // tokenizer: more tokens for the same text, so the typed-prompt estimate
    // errs high like the price beside it.
    let newer_tokenizer = match listed {
        Some(r) => r.version >= NEWER_TOKENIZER_FROM,
        None => true,
    };
    let chars_per_token = if newer_tokenizer {
        CHARS_PER_TOKEN_EARLIER / NEWER_TOKENIZER_FACTOR
    } else {
        CHARS_PER_TOKEN_EARLIER
    };
    match listed {
        Some(r) => Some(PriceQuote {
            price: r.price,
            long_prompt: r.long_prompt.map(|(over_tokens, price)| LongPromptPrice { over_tokens, price }),
            basis: PriceBasis::Listed,
            chars_per_token,
        }),
        None => family_ceiling(family).map(|price| PriceQuote {
            price,
            long_prompt: None,
            basis: PriceBasis::FamilyCeiling,
            chars_per_token,
        }),
    }
}

/// The base-tier price for `model` — [`price_quote`] for a caller with no
/// prompt length in hand. A caller pricing one REQUEST uses
/// [`PriceQuote::at`] with that request's own prompt length instead.
pub fn price_for(model: &str) -> Option<ModelPrice> {
    price_quote(model).map(|q| q.price)
}

/// Dollar cost of a token bundle at a given price (per-1M rates), with every
/// cache write at the 5-minute rate.
fn cost_of(t: &TokenUsage, p: &ModelPrice) -> f64 {
    cost_of_split(t, 0, p)
}

/// [`cost_of`] with `write_1h` of the bundle's cache-creation tokens priced at
/// the 1-hour rate and the rest at the 5-minute one. `write_1h` is clamped to
/// the bundle's own cache-creation count, so a record whose buckets disagree
/// with its total can move tokens between the two rates and never invent any.
fn cost_of_split(t: &TokenUsage, write_1h: u64, p: &ModelPrice) -> f64 {
    let write_1h = write_1h.min(t.cache_creation_tokens);
    let write_5m = t.cache_creation_tokens - write_1h;
    (t.input_tokens as f64 * p.input
        + t.output_tokens as f64 * p.output
        + write_5m as f64 * p.cache_write
        + write_1h as f64 * p.cache_write_1h
        + t.cache_read_tokens as f64 * p.cache_read)
        / 1_000_000.0
}

// ---------------------------------------------------------------------------
// Claude Code transcript parsing
// ---------------------------------------------------------------------------

/// Pull a u64 usage field, tolerating absent/null.
fn u64_field(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Parse a Claude Code session transcript (JSONL text) into summed usage plus
/// a token-derived cost estimate. Pure and fixture-testable.
///
/// Rules mirroring how Claude Code writes transcripts:
/// - Only `assistant` messages carry a `usage` object; user/summary lines are
///   skipped.
/// - The same assistant message can appear more than once (streaming replays,
///   `--resume` re-emits); dedupe by `message.id` so tokens aren't
///   double-counted. Lines without an id are always counted (can't dedupe).
/// - Synthetic messages (`model` == `"<synthetic>"`) are not billable and
///   never contribute a model/price.
/// - Cost accumulates per message at its own model's price, so a session that
///   switched models is priced correctly; if no message used a priced model,
///   `cost_usd` is `None`.
pub fn parse_claude_transcript(text: &str) -> SessionUsage {
    parse_claude_transcript_lines(text.lines())
}

/// [`parse_claude_transcript`] over a LINE ITERATOR instead of one `&str`.
///
/// Same rules, same result — the `&str` form is literally this function fed
/// `text.lines()`, so the two cannot drift. It exists because the caller that
/// reads a transcript off disk must never hold the whole file: see
/// [`claude_session_usage_in`] and #1218.
///
/// The fold is per-line and lives in [`TranscriptFold`]; nothing but the
/// source of the lines changes between callers. Memory is bounded by the
/// longest single line plus the dedupe set of message ids — not by the file.
pub fn parse_claude_transcript_lines<I, S>(lines: I) -> SessionUsage
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut fold = TranscriptFold::default();
    for line in lines {
        fold.push(line.as_ref());
    }
    fold.usage()
}

/// The running state of a transcript fold: the four token totals, the accrued
/// dollar cost, the message-id dedupe set, and the best-priced model so far.
///
/// **Why it is a struct (#1239).** The fold was already per-line, but it lived
/// inside `parse_claude_transcript_lines` as six locals, which meant it could
/// only ever run to completion in one call. The incremental reader
/// ([`TranscriptCursors`]) keeps one of these alive ACROSS polls and pushes
/// only the newly-appended lines into it, so a tick's work is proportional to
/// what the agent just wrote rather than to the whole session.
///
/// Extracting it is also what keeps the whole-file parse and the incremental
/// one from drifting: there is exactly one implementation of the rules
/// documented on [`parse_claude_transcript`], and both paths are that
/// implementation fed different lines. Two folds could not have been kept in
/// step by review.
#[derive(Default)]
struct TranscriptFold {
    totals: TokenUsage,
    cost: f64,
    any_priced: bool,
    seen: HashSet<String>,
    /// The priced model with the most output tokens, for display.
    best_model: Option<(String, u64)>,
    /// The model of the LAST counted assistant message, priced or not
    /// ([`SessionUsage::current_model`], #3415). Kept beside `best_model`
    /// rather than replacing it: the usage panel's "priced against" label is
    /// a pricing question, the chart's split is a which-model-now question,
    /// and on a switched session they have different answers. A re-emitted
    /// (deduped) message never moves it — an old line replayed by `--resume`
    /// must not rewind the pane to the model it has since left.
    last_model: Option<String>,
    /// The first counted real turn's context ([`SessionUsage::first_context_tokens`]).
    /// Set once and never moved: a re-emitted line is dropped by the dedupe
    /// above before it gets here, and a later turn is not the first.
    first_context: Option<u64>,
    /// The lifetime of the last cache write seen
    /// ([`SessionUsage::detected_cache_ttl_minutes`]).
    ///
    /// **The last cache-WRITING request decides.** A request that only read
    /// the cache leaves this alone: a hit refreshes an entry for the lifetime
    /// it was written with (the pricing page's duration for a cache hit is
    /// "Same duration as the preceding write"), so it is no evidence of a
    /// change. A later write on a different lifetime replaces it, which is
    /// what follows an account whose cache lifetime changes mid-session.
    write_ttl: Option<u32>,
}

impl TranscriptFold {
    /// Fold ONE transcript line in, per the rules on
    /// [`parse_claude_transcript`]. A line that is blank, unparseable, not an
    /// assistant turn, carries no `usage`, or repeats an already-seen message
    /// id contributes nothing — which is what makes the fold safe to feed the
    /// same line twice.
    fn push(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let Some(msg) = v.get("message") else { return };
        let Some(usage) = msg.get("usage") else { return };

        // Dedupe by message id when present.
        if let Some(id) = msg.get("id").and_then(Value::as_str) {
            if !self.seen.insert(id.to_string()) {
                return;
            }
        }

        let t = TokenUsage {
            input_tokens: u64_field(usage, "input_tokens"),
            output_tokens: u64_field(usage, "output_tokens"),
            cache_creation_tokens: u64_field(usage, "cache_creation_input_tokens"),
            cache_read_tokens: u64_field(usage, "cache_read_input_tokens"),
        };
        self.totals.input_tokens += t.input_tokens;
        self.totals.output_tokens += t.output_tokens;
        self.totals.cache_creation_tokens += t.cache_creation_tokens;
        self.totals.cache_read_tokens += t.cache_read_tokens;

        let model = msg.get("model").and_then(Value::as_str).unwrap_or("");
        if model.is_empty() || model == "<synthetic>" {
            return;
        }
        self.last_model = Some(model.to_string());
        // This request's prompt length: everything it was sent, which is the
        // same sum `latest_context_tokens` reads off the newest turn.
        let prompt = t.input_tokens + t.cache_creation_tokens + t.cache_read_tokens;
        self.first_context.get_or_insert(prompt);
        if let Some(ttl) = claude_write_ttl(usage) {
            self.write_ttl = Some(ttl);
        }
        if let Some(q) = price_quote(model) {
            // Priced per REQUEST, on two axes a session-wide price cannot
            // carry (#3831): the prompt-length tier this request's own prompt
            // falls in, and which cache its writes went to.
            self.cost += cost_of_split(&t, claude_write_1h_tokens(usage), &q.at(prompt));
            self.any_priced = true;
            let out = t.output_tokens;
            match &mut self.best_model {
                Some((_, best_out)) if *best_out >= out => {}
                _ => self.best_model = Some((model.to_string(), out)),
            }
        }
    }

    /// The session usage as of everything folded in so far. Cheap enough to
    /// call on every poll — it clones one model id and copies four counters.
    fn usage(&self) -> SessionUsage {
        SessionUsage {
            tokens: self.totals,
            cost_usd: self.any_priced.then_some(self.cost),
            model: self.best_model.as_ref().map(|(m, _)| m.clone()),
            current_model: self.last_model.clone(),
            first_context_tokens: self.first_context,
            detected_cache_ttl_minutes: self.write_ttl,
        }
    }
}

/// Approximate current context-window usage from a Claude Code transcript
/// (#328): the LATEST assistant message's `input_tokens +
/// cache_creation_input_tokens + cache_read_input_tokens` — the size of
/// everything sent as context for that turn. This is a materially different
/// question from `parse_claude_transcript`'s cumulative totals (which sum
/// every turn's input across the WHOLE session, for cost/billing purposes);
/// the context window's current fullness is what the MOST RECENT turn sent,
/// not the running lifetime sum. Self-correcting after a compaction — the
/// next turn's input tokens drop back down, exactly reflecting the freshly
/// summarized context. `output_tokens` is excluded: it's what the turn
/// PRODUCED, not what was IN context going in. `None` if no real (non-
/// synthetic) assistant `usage` line is found. Exact (an API-reported figure
/// from the CLI's own transcript), not a byte-count proxy — see
/// `docs/design/orchestration.md`'s Compact-nudge section for why this beats
/// inventing one.
pub fn latest_context_tokens(text: &str) -> Option<u64> {
    let v = latest_real_assistant_turn(text)?;
    let usage = v.get("message")?.get("usage")?;
    let input = u64_field(usage, "input_tokens");
    let cache_creation = u64_field(usage, "cache_creation_input_tokens");
    let cache_read = u64_field(usage, "cache_read_input_tokens");
    Some(input + cache_creation + cache_read)
}

/// Production bug fix (PR #329 round 7): the model id the LATEST real turn
/// ran on — the exact `"model"` field `latest_context_tokens` already reads
/// past on its way to the token count, now also surfaced so a caller can
/// derive the ACTUAL context-window size for this session
/// (`claude_context_window_tokens`) instead of assuming a flat one. Shares
/// `latest_real_assistant_turn`'s "which turn is latest" definition with
/// `latest_context_tokens`, so the two can never disagree about which turn
/// they're each reading.
pub fn latest_context_model(text: &str) -> Option<String> {
    let v = latest_real_assistant_turn(text)?;
    v.get("message")?.get("model")?.as_str().map(str::to_string)
}

/// The LATEST real (non-synthetic, `usage`-bearing) assistant turn in a
/// transcript, scanning newest-to-oldest — shared scan behind both `latest_
/// context_tokens` and `latest_context_model`.
fn latest_real_assistant_turn(text: &str) -> Option<Value> {
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(msg) = v.get("message") else { continue };
        if msg.get("usage").is_none() {
            continue;
        }
        let model = msg.get("model").and_then(Value::as_str).unwrap_or("");
        if model.is_empty() || model == "<synthetic>" {
            continue; // not a real turn's context
        }
        return Some(v);
    }
    None
}

/// Production bug fix (PR #329 round 7): the standard Claude context window
/// (tokens) — the conservative fallback `claude_context_window_tokens` uses
/// for an absent/unrecognized model id. UNDER-estimating the window (reading
/// a HIGHER percent than reality) nudges toward compacting SOONER, never
/// later — the safe direction when genuinely unsure, since the alternative
/// (silently assuming a bigger window than reality) risks the escalation
/// threshold firing too LATE and letting the CLI's own emergency auto-
/// compact land with no offload.
pub const DEFAULT_CLAUDE_CONTEXT_WINDOW_TOKENS: u64 = 200_000;

/// Context-window size (tokens) for a Claude model, matched the SAME way
/// `price_for` matches (substring of the transcript's own model id) — real
/// evidence (a live demo, PR #329 round 7) showed a flat 200K denominator
/// reads badly wrong for a model actually running with a much larger window:
/// the CLI's own `/context` reported ~5% for a token count loomux read as
/// ~26% under the flat assumption. Opus is the one family with concrete,
/// user-reported evidence of a 1M-token tier; everything else (and an
/// absent/unrecognized model id) falls back to the documented default. This
/// is a best-effort GUESS, not a guarantee — Claude's actual context tier is
/// ultimately a per-request API setting this transcript field doesn't fully
/// pin down — so callers needing certainty should prefer an explicit
/// human-set override over this function's return value; see `docs/design/
/// orchestration.md`'s Compact-nudge section.
pub fn claude_context_window_tokens(model: Option<&str>) -> u64 {
    let Some(model) = model else { return DEFAULT_CLAUDE_CONTEXT_WINDOW_TOKENS };
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        1_000_000
    } else {
        DEFAULT_CLAUDE_CONTEXT_WINDOW_TOKENS
    }
}

/// Production bug fix (PR #329, rev-42 delta): count of `type: "system",
/// subtype: "compact_boundary"` lines in a Claude transcript — the CLI's own
/// structural marker for "a compaction just completed here", written by the
/// CLI the INSTANT compaction finishes, carrying the exact `preTokens`/
/// `postTokens` it measured. Unlike `latest_context_tokens`'s drop, this
/// needs no following turn to observe: real transcript evidence (a genuine
/// dogfood session on this repo, `1aadeb3f-e8a1-4d29-88d4-7cf4b44ddf2a.jsonl`)
/// shows the boundary line lands 20 lines before the next real assistant
/// `usage` line — several non-assistant bookkeeping lines (a synthetic
/// continuation summary, attachment deltas, a `last-prompt` marker) sit in
/// between with no `type: "assistant"` at all. `latest_context_tokens`
/// genuinely cannot see a compact happened until that next turn exists; this
/// function can, immediately. Monotonically non-decreasing across a growing
/// transcript (more compactions only ever ADD boundary lines), so comparing a
/// later count against a baseline captured earlier is a clean "did a NEW
/// compaction happen since then" signal — see `orchestration::
/// inferred_compaction_confirmed`, its consumer.
pub fn compact_boundary_count(text: &str) -> u64 {
    text.lines()
        .filter(|line| {
            let line = line.trim();
            if line.is_empty() {
                return false;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else { return false };
            v.get("type").and_then(Value::as_str) == Some("system")
                && v.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        })
        .count() as u64
}

/// Both compaction-confirmation signals from a single transcript read
/// (rev-42 Q4: the two separate whole-file reads `claude_context_tokens_in`
/// and `agent_context_percents` each did are replaced by callers sharing
/// this one bounded read).
pub struct CompactionSignal {
    pub tokens: Option<u64>,
    pub compact_boundary_count: u64,
    /// Production bug fix (PR #329 round 7): the model the latest real turn
    /// ran on (`latest_context_model`) — lets a caller derive the ACTUAL
    /// context-window size (`claude_context_window_tokens`) for the percent
    /// this reading feeds, instead of assuming a flat one.
    pub model: Option<String>,
    /// #993 S1: the context-window size the CLI itself REPORTED (Claude's
    /// status-line `context_window.context_window_size`), when a snapshot for
    /// this session exists. `None` from the transcript alone, which records no
    /// window — the ladder then falls to the model table.
    pub window_tokens: Option<u64>,
    /// #993 S2b: `window_tokens` is a LOWER BOUND the CLI printed rounded (a
    /// pi `--list-models` spelling such as `262.1K`), so the ladder labels it
    /// `reported-rounded` rather than `reported`. Always `false` when
    /// `window_tokens` is `None`, and for every reader but pi's.
    pub window_rounded: bool,
    /// The reasoning effort this reading carries: Claude's status-line
    /// `effort.level` (#993 S1), codex's `turn_context.effort` (S2a), pi's
    /// newest `thinking_level_change` — else its launch knob, see
    /// `effort_is_launch_fallback` (S2b) — or opencode's session `variant`
    /// (S2c). Read it through [`Self::observed_effort`], never directly.
    pub effort: Option<String>,
    /// True only when `effort` is pi's launch knob (`--thinking`) standing in
    /// for a level the session tail does not name — configuration, not a
    /// reading. Every consumer that publishes an effort reads it through
    /// [`Self::observed_effort`], which drops it: the usage-series sample
    /// (#993 S6) and `group_summary`'s `context.effort` (S3), where the knob
    /// is published once, as `context.declared.effort`.
    pub effort_is_launch_fallback: bool,
    /// #993 S1: whether a status-line snapshot contributed to this reading.
    pub source: crate::modelstate::ContextSource,
}

impl CompactionSignal {
    /// The effort this reading OBSERVED: `effort` unless it is the pi launch
    /// fallback, which is the block's configured knob rather than anything the
    /// session reported. The one rule both the usage-series sample (#993 S6)
    /// and `group_summary`'s `context.effort` (S3) publish by — the knob
    /// itself is already published as `context.declared.effort`.
    pub fn observed_effort(&self) -> Option<&str> {
        if self.effort_is_launch_fallback {
            None
        } else {
            self.effort.as_deref()
        }
    }
}

// ---------------------------------------------------------------------------
// Transcript location
// ---------------------------------------------------------------------------

/// Default root under which Claude Code keeps per-project transcript folders.
/// Callers can override it (see `claude_session_usage_in`) so tests point at a
/// fixture tree without a real `~/.claude` and without touching global state.
pub fn default_claude_projects_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

/// Locate a session's transcript file under `root` by scanning the project
/// folders for `<session-id>.jsonl`. Claude encodes the cwd into the folder
/// name, so the file could be under any of them; a direct scan avoids
/// re-deriving that encoding. `None` if no transcript exists yet.
///
/// `pub(crate)`: `orchestration::digest` reuses this resolver rather than
/// re-deriving the same project-folder scan (#250/#324 slice B).
///
/// **The single declared assembly point for a claude transcript path (#925),
/// and it takes a [`PathSegment`] rather than a `&str` for the same reason
/// `group_dir_at` takes a `GroupId`.** The id is interpolated into a file name
/// that is then joined onto a directory this process did not choose, so a caller
/// holding an unvalidated string has no business reaching here: `..`, a
/// separator, or a Windows drive prefix in that id walks the join out of the
/// projects root. Requiring proof at the signature is what makes "validated
/// exactly once, at the boundary" a fact the compiler keeps rather than a
/// convention every future caller has to remember.
pub(crate) fn claude_transcript_path(root: &Path, session: &PathSegment) -> Option<PathBuf> {
    let name = format!("{session}.jsonl");
    let projects = fs::read_dir(root).ok()?;
    for project in projects.flatten() {
        let candidate = project.path().join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Read and sum a Claude session's usage from a transcript under the default
/// `~/.claude/projects` root. `None` when the root can't be resolved or the
/// transcript can't be found/opened.
pub fn claude_session_usage(session_id: &str) -> Option<SessionUsage> {
    let root = default_claude_projects_root()?;
    claude_session_usage_in(&root, session_id)
}

/// Read and sum a Claude session's usage from a transcript under an explicit
/// projects `root`, parsing the file from byte zero. `None` when the
/// transcript can't be found or opened — or when `session_id` is not a usable
/// path component (#925).
///
/// The refusal shares the existing `None` channel deliberately. This is a
/// polled usage meter reading an id off a persisted index entry; an entry
/// written by an older build is not evidence of anything, and "no usage for
/// that id" is the same answer it already gives for a transcript that has not
/// been written yet. Same fail-closed-into-an-existing-degrade shape #904 used
/// for `promptsubmit_marker_path`.
///
/// **This is no longer what the poll calls.** Since #1239 the polled path is
/// [`TranscriptCursors::session_usage`], which reaches the same totals by
/// folding only the bytes appended since the previous tick. This function
/// remains the whole-file read: `claude_session_usage`'s implementation, the
/// answer a fresh cursor computes on its first tick, and the thing the
/// incremental result is checked against. Both go through [`fold_appended`],
/// so #1218's streaming property is shared by construction rather than by two
/// readers being kept in step.
pub fn claude_session_usage_in(root: &Path, session_id: &str) -> Option<SessionUsage> {
    let session = PathSegment::parse(session_id).ok()?;
    let path = claude_transcript_path(root, &session)?;
    let mut cursor = TranscriptCursor::new(TranscriptKind::Claude, path);
    fold_appended(&mut cursor, false).ok()?;
    Some(cursor.fold.usage())
}

// ---------------------------------------------------------------------------
// pi transcript parsing (#2126)
// ---------------------------------------------------------------------------

/// Map one pi `Usage` object onto loomux's four token buckets.
///
/// **`reasoning` is deliberately NOT folded into `output_tokens`, and that is
/// the opposite of what [`opencode_session_usage`] does with the same-named
/// field.** pi's own type says why: *"Reasoning/thinking tokens, when the
/// provider reports them. This is a subset of `output`: `output` already
/// includes these tokens."* (`@earendil-works/pi-ai` `dist/types.d.ts`,
/// `interface Usage`, read off the 0.84.4 install). Adding it would
/// double-count. pi agrees with itself elsewhere: its `totalTokens` is
/// `input + output + cacheRead + cacheWrite` and its `calculateCost` prices
/// exactly those four, so `reasoning` contributes to neither. OpenCode's fifth
/// bucket is genuinely disjoint from its `output`, which is why that mapping
/// folds and this one must not.
///
/// A caveat recorded rather than smoothed over: on the local install, real
/// openrouter transcripts carry turns where `reasoning` EXCEEDS `output`, which
/// the vendor's own subset invariant cannot allow. Whichever way that
/// provider's numbers are wrong, taking pi's documented contract and pi's own
/// cost basis is what keeps our tokens and our dollars describing the same
/// spend.
///
/// `cacheWrite` -> `cache_creation_tokens` is the same rename opencode's
/// mapping makes: tokens written INTO the cache, which is what Claude's
/// `cache_creation_input_tokens` counts.
fn pi_tokens(usage: &Value) -> TokenUsage {
    TokenUsage {
        input_tokens: u64_field(usage, "input"),
        output_tokens: u64_field(usage, "output"),
        cache_creation_tokens: u64_field(usage, "cacheWrite"),
        cache_read_tokens: u64_field(usage, "cacheRead"),
    }
}

/// The running state of a pi transcript fold — the pi counterpart of
/// [`TranscriptFold`], and the second arm of [`TranscriptFolder`].
///
/// **No message-id dedupe, unlike claude's fold.** claude's exists because a
/// `--resume` re-emits assistant messages that were already on disk; pi's
/// session file is a TREE whose entries each get a fresh id, and re-folding the
/// same bytes is prevented by the cursor's own guards rather than by a set
/// here. Every entry carrying a `usage` is spend that happened, including
/// entries on branches the current leaf has navigated away from — the tokens
/// were bought either way, so the fold is over the FILE, not over the active
/// path.
#[derive(Default)]
struct PiFold {
    totals: TokenUsage,
    cost: f64,
    /// Any folded `usage` carried a `cost.total`. Distinguishes a genuinely
    /// free session (`Some(0.0)`) from one pi never priced (`None`) — the same
    /// distinction `a_free_models_zero_cost_is_reported_as_zero_not_as_unknown`
    /// pins for opencode.
    any_cost: bool,
    /// `provider/model` of the LAST assistant entry seen. Not the
    /// best-priced-by-output model claude's fold tracks: pi REPORTS its dollars
    /// rather than having them derived from a price table here, so this field
    /// carries no pricing decision to explain — it answers "which model is this
    /// pane on", and for that the latest turn is the truth.
    last_model: Option<String>,
    /// The first ASSISTANT turn's context
    /// ([`SessionUsage::first_context_tokens`]). Only an assistant entry can
    /// set it: a tool's own usage, a compaction and a branch summary are spend
    /// under this session, but none of them is the prompt the agent was sent.
    first_context: Option<u64>,
}

impl PiFold {
    /// Fold ONE pi session-file line in.
    ///
    /// The four entry shapes that can carry spend, per pi's session-entry union
    /// (`dist/core/session-manager.d.ts` at 0.84.4):
    ///
    /// - `{"type":"message","message":{"role":"assistant",...,"usage":{...}}}` —
    ///   the ordinary turn, and the only shape carrying `provider`/`model`.
    /// - the same with `"role":"toolResult"`, whose `usage` is optional and
    ///   documented as *"Usage from the tool execution itself"* — a tool that
    ///   called a model of its own. pi excludes it from context accounting; it
    ///   is still money spent under this session, which is what this meter
    ///   counts.
    /// - `{"type":"compaction",...,"usage":{...}}` — *"Usage from the LLM
    ///   call(s) that generated this summary, if available"*.
    /// - `{"type":"branch_summary",...,"usage":{...}}` — likewise, for a branch.
    ///
    /// Everything else (`session`, `model_change`, `thinking_level_change`,
    /// `custom`, `label`, user messages) carries no usage and contributes
    /// nothing. A blank or unparseable line contributes nothing either, which
    /// is what makes a torn last line harmless the moment the cursor holds it
    /// back.
    fn push(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let mut is_turn = false;
        let usage = match v.get("type").and_then(Value::as_str) {
            Some("message") => {
                let Some(msg) = v.get("message") else { return };
                match msg.get("role").and_then(Value::as_str) {
                    Some("assistant") => {
                        is_turn = true;
                        // Recorded even when the turn spent nothing — pi writes
                        // an all-zero `usage` on an errored turn — because this
                        // field is "which model is this pane on" rather than
                        // "which model did the spending".
                        if let (Some(p), Some(m)) = (
                            msg.get("provider").and_then(Value::as_str),
                            msg.get("model").and_then(Value::as_str),
                        ) {
                            if !p.is_empty() && !m.is_empty() {
                                self.last_model = Some(format!("{p}/{m}"));
                            }
                        }
                        msg.get("usage")
                    }
                    Some("toolResult") => msg.get("usage"),
                    _ => None,
                }
            }
            Some("compaction") | Some("branch_summary") => v.get("usage"),
            _ => None,
        };
        let Some(usage) = usage else { return };

        let t = pi_tokens(usage);
        // pi writes an all-zero `usage` on an errored turn; that is not a
        // prompt anyone was sent, so it does not get to be the first.
        let prompt = t.input_tokens + t.cache_creation_tokens + t.cache_read_tokens;
        if is_turn && prompt > 0 {
            self.first_context.get_or_insert(prompt);
        }
        self.totals.input_tokens += t.input_tokens;
        self.totals.output_tokens += t.output_tokens;
        self.totals.cache_creation_tokens += t.cache_creation_tokens;
        self.totals.cache_read_tokens += t.cache_read_tokens;

        if let Some(total) = usage.get("cost").and_then(|c| c.get("total")).and_then(Value::as_f64)
        {
            self.cost += total;
            self.any_cost = true;
        }
    }

    /// The session usage as of everything folded in so far.
    fn usage(&self) -> SessionUsage {
        SessionUsage {
            tokens: self.totals,
            cost_usd: self.any_cost.then_some(self.cost),
            model: self.last_model.clone(),
            current_model: self.last_model.clone(),
            first_context_tokens: self.first_context,
            // pi's `usage` has one `cacheWrite` count and no lifetime beside
            // it, so there is nothing to read a TTL off.
            detected_cache_ttl_minutes: None,
        }
    }
}

/// Parse a pi session file (JSONL text) into summed usage plus the dollar
/// figure **pi itself reported**. Pure and fixture-testable; no pi is ever run
/// (constraint 3).
pub fn parse_pi_transcript(text: &str) -> SessionUsage {
    parse_pi_transcript_lines(text.lines())
}

/// [`parse_pi_transcript`] over a LINE ITERATOR — the pi twin of
/// [`parse_claude_transcript_lines`], and for the same reason: the on-disk
/// reader must never hold the whole file (#1218).
pub fn parse_pi_transcript_lines<I, S>(lines: I) -> SessionUsage
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut fold = PiFold::default();
    for line in lines {
        fold.push(line.as_ref());
    }
    fold.usage()
}

/// Read and sum a pi session's usage from the group's own pi store at `dir`
/// (`crate::orchestration::pi_sessions_in`), parsing the file from byte zero.
/// `None` when no file in `dir` carries that session id, when it cannot be
/// opened, or when `session_id` is not a usable path component (#925).
///
/// The whole-file counterpart of the polled path, exactly as
/// [`claude_session_usage_in`] is for claude: both go through
/// [`fold_appended`], so #1218's streaming property and the partial-last-line
/// rule are shared by construction rather than by two readers being kept in
/// step.
pub fn pi_session_usage_in(dir: &Path, session_id: &str) -> Option<SessionUsage> {
    let session = PathSegment::parse(session_id).ok()?;
    let path = crate::orchestration::pi_session_file_in_dir(dir, &session).ok().flatten()?;
    let mut cursor = TranscriptCursor::new(TranscriptKind::Pi, path);
    fold_appended(&mut cursor, false).ok()?;
    Some(cursor.fold.usage())
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// codex rollout parsing (#2515 slice C3)
// ---------------------------------------------------------------------------

/// One codex `TokenUsage` object mapped onto loomux's four buckets.
///
/// **The buckets here are DISJOINT and codex's are not**, which is the whole of
/// this function and the one thing to get right. Read at `rust-v0.153.4`
/// (`codex-api/src/sse/responses.rs`, `impl From<ResponseCompletedUsage> for
/// TokenUsage`), codex copies the Responses API's own shape straight through:
/// `input_tokens` is the WHOLE prompt count, and `cached_tokens` /
/// `cache_write_tokens` are fields of `input_tokens_details` -- details OF that
/// number, not additions to it. Its own vendor test pins the arithmetic:
/// `input_tokens: 100` with `cached_tokens: 40` and `cache_write_tokens: 60`,
/// `output_tokens: 10`, and `total_tokens: 110` -- i.e. input + output, with
/// the two cache figures already inside the input half.
///
/// loomux's [`TokenUsage::total`] sums all four buckets, so mapping
/// `input_tokens` across whole would count the cached and written halves twice
/// and report 210 for a turn codex itself calls 110. So the fresh-input bucket
/// is `input_tokens` MINUS both details, and the identity that falls out is the
/// one to check a fixture against: this mapping's `total()` equals codex's own
/// `total_tokens`. The plan's D8 named only the two cache mappings and was
/// silent on the subtraction; the correction is argued in
/// `docs/design/codex.md` under Usage.
///
/// `saturating_sub` rather than a plain one, and it is what bounds the residual:
/// if a future codex ever made `cache_write_input_tokens` genuinely disjoint
/// from `input_tokens`, this would under-report fresh input rather than
/// underflow to a colossal number. Failing toward a smaller figure is the right
/// direction for a meter whose one refusal is a wrong total.
///
/// **`reasoning_output_tokens` is NOT added to output** -- pi's rule, on codex's
/// facts. It is `output_tokens_details.reasoning_tokens`, a detail of
/// `output_tokens` exactly as the cache figures are of input, so adding it would
/// double-count for the same reason. That is the opposite of the OpenCode
/// mapping, whose fifth bucket is genuinely disjoint.
///
/// Every field is read as a `u64` with a zero default, so a negative -- codex
/// types these `i64` and `non_cached_input` clamps at zero for that reason --
/// or an absent key contributes nothing rather than poisoning the sum.
fn codex_tokens(usage: &Value) -> TokenUsage {
    let cache_read = u64_field(usage, "cached_input_tokens");
    let cache_write = u64_field(usage, "cache_write_input_tokens");
    TokenUsage {
        input_tokens: u64_field(usage, "input_tokens")
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        output_tokens: u64_field(usage, "output_tokens"),
        cache_creation_tokens: cache_write,
        cache_read_tokens: cache_read,
    }
}

/// The running state of a codex rollout fold -- the third arm of
/// [`TranscriptFolder`].
///
/// **`payload.usage` per record, never `thread_token_usage`.** A
/// `token_usage_record` line carries THREE `TokenUsage` objects
/// (`protocol/src/protocol.rs`, `struct TokenUsageRecord`): `usage` is this one
/// response's, `turn_token_usage` is the running total for the turn, and
/// `thread_token_usage` is the running total for the whole thread. Summing any
/// of the latter two over a file sums a series of prefixes -- a thread of N
/// responses would report something on the order of N times its real spend, and
/// it would look plausible. Reading the LAST `thread_token_usage` instead of
/// summing would be arithmetically right and is still refused: it would break
/// the incremental cursor's contract, which is that folding the appended region
/// on top of a partial total is the same as folding the file whole, and only a
/// per-record SUM has that property.
///
/// No dedupe, like [`PiFold`] and unlike claude's: codex appends one record per
/// completed response and never re-emits, so re-folding the same bytes is
/// prevented by the cursor's own guards rather than by a set here.
///
/// No dollars: codex records tokens only, so `cost_usd` is left to a
/// [`price_for`] lookup and labelled an ESTIMATE -- the claude posture, not
/// opencode's or pi's.
#[derive(Default)]
struct CodexFold {
    totals: TokenUsage,
    /// The model of the LAST `turn_context` line seen. Not a best-priced-by-
    /// output choice: the question this answers is "which model is this pane
    /// on", and codex records a fresh `turn_context` per user turn, so the
    /// latest one is the truth. `turn_context.payload.model` is a required
    /// `String` at the pin (`struct TurnContextItem`), so a line that parses at
    /// all either has it or is not a `turn_context`.
    last_model: Option<String>,
    /// The first response's context
    /// ([`SessionUsage::first_context_tokens`]).
    first_context: Option<u64>,
}

impl CodexFold {
    /// Fold ONE rollout line in.
    ///
    /// A rollout line is `RolloutItemWire` (`history/src/rollout_payload.rs`,
    /// `#[serde(tag = "type", rename_all = "snake_case")]`) plus a `timestamp`,
    /// so the two shapes that matter here are
    /// `{"type":"token_usage_record","payload":{...,"usage":{...}}}` and
    /// `{"type":"turn_context","payload":{...,"model":"..."}}`.
    ///
    /// Everything else -- `session_meta`, `response_item`, `compacted`,
    /// `event_msg`, `world_state`, and the rest of the wire enum -- carries no
    /// spend this meter counts and contributes nothing. In particular
    /// `event_msg`/`token_count` is deliberately ignored even though it carries
    /// a `total_token_usage`: it is the same cumulative figure
    /// `thread_token_usage` is, written for the TUI's own display, and summing
    /// it would multiply the total exactly as described above.
    ///
    /// A blank or unparseable line contributes nothing, which is what makes a
    /// torn last line harmless the moment the cursor holds it back.
    fn push(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("token_usage_record") => {
                let Some(usage) = v.pointer("/payload/usage") else { return };
                let t = codex_tokens(usage);
                // The three input-side buckets add back up to codex's own
                // whole-prompt `input_tokens` (see `codex_tokens`).
                let prompt = t.input_tokens + t.cache_creation_tokens + t.cache_read_tokens;
                if prompt > 0 {
                    self.first_context.get_or_insert(prompt);
                }
                self.totals.input_tokens += t.input_tokens;
                self.totals.output_tokens += t.output_tokens;
                self.totals.cache_creation_tokens += t.cache_creation_tokens;
                self.totals.cache_read_tokens += t.cache_read_tokens;
            }
            Some("turn_context") => {
                if let Some(m) = v.pointer("/payload/model").and_then(Value::as_str) {
                    if !m.is_empty() {
                        self.last_model = Some(m.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    /// The session usage as of everything folded in so far.
    ///
    /// `cost_usd` is a price-table estimate or `None`: codex writes no dollars
    /// at all, and no codex model is in [`price_for`]'s table today, so this is
    /// `None` in practice and the row is tokens-only. That is deliberate --
    /// inventing an OpenAI price column here would make the group total a guess
    /// nobody dated -- and the arm still sets `estimated: true`, which is what
    /// keeps a later priced row honest.
    fn usage(&self) -> SessionUsage {
        let price = self.last_model.as_deref().and_then(price_for);
        SessionUsage {
            tokens: self.totals,
            cost_usd: price.map(|p| cost_of(&self.totals, &p)),
            model: self.last_model.clone(),
            current_model: self.last_model.clone(),
            first_context_tokens: self.first_context,
            // A rollout's `cache_write_input_tokens` carries no lifetime.
            detected_cache_ttl_minutes: None,
        }
    }
}

/// Parse a codex rollout (JSONL text) into summed usage. Pure and
/// fixture-testable; no codex is ever run (constraint 3).
pub fn parse_codex_transcript(text: &str) -> SessionUsage {
    parse_codex_transcript_lines(text.lines())
}

/// [`parse_codex_transcript`] over a LINE ITERATOR -- the codex twin of
/// [`parse_pi_transcript_lines`], and for the same reason: the on-disk reader
/// must never hold the whole file (#1218).
pub fn parse_codex_transcript_lines<I, S>(lines: I) -> SessionUsage
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut fold = CodexFold::default();
    for line in lines {
        fold.push(line.as_ref());
    }
    fold.usage()
}

/// Read and sum a codex session's usage from the human's own store at `root`
/// (`sessions::codex_sessions_root`), parsing the rollout from byte zero.
/// `None` when no readable rollout in `root` carries that thread id -- which
/// includes the compressed case, per `find_codex_session_file`'s doc.
///
/// The whole-file counterpart of the polled path, exactly as
/// [`pi_session_usage_in`] is for pi.
pub fn codex_session_usage_in(root: &Path, session_id: &str) -> Option<SessionUsage> {
    let session = PathSegment::parse(session_id).ok()?;
    let path = loomux_engine::sessions::find_codex_session_file(root, &session)?;
    let mut cursor = TranscriptCursor::new(TranscriptKind::Codex, path);
    fold_appended(&mut cursor, false).ok()?;
    Some(cursor.fold.usage())
}
// Incremental transcript reading (#1239)
// ---------------------------------------------------------------------------

/// Bytes of ALREADY-FOLDED content a cursor re-reads and compares before it
/// folds anything new on top.
///
/// **What it proves, exactly: that the LAST `ANCHOR_BYTES` of the consumed
/// region are still what was folded there.** Not that the consumed region is
/// intact — that is a strictly stronger statement this check does not make,
/// and reading it as the stronger one is the mistake #1361's review caught.
///
/// It buys the cases `len`+`mtime` cannot see: an in-place rewrite landing on
/// the same length, or one inside a single coarse-`mtime` tick. A wholesale
/// replacement shifts the content, and a rewrite of the most recent record
/// lands inside the window, so both change what sits there and the cursor
/// throws itself away.
///
/// **What it does NOT see**, stated here because three other passages used to
/// imply otherwise: an in-place edit to a consumed byte at an offset BELOW
/// `offset - ANCHOR_BYTES`, on a file that goes on being appended to normally.
/// `len` grew, mtime moved forward, the creation time is unchanged and the
/// anchor window is untouched, so every guard agrees and the edited bytes are
/// never re-read. Nothing in the stat or the anchor will ever notice; what
/// bounds it is [`CURSOR_REVALIDATE_AFTER`], which throws the whole cursor
/// away on a timer so the error is corrected within one interval instead of
/// persisting for the life of the session.
///
/// 64 bytes keeps the work bound this whole change exists for intact, and a
/// transcript line is hundreds of bytes at minimum, so the anchor never spans
/// more than the tail of one record.
const ANCHOR_BYTES: usize = 64;

/// How long a cursor may keep folding incrementally before it is discarded and
/// the transcript re-parsed from byte zero, whatever the stat and the anchor
/// say.
///
/// **This is what makes "fails toward slow, never toward wrong" true rather
/// than nearly true** (#1361 review B1). The stat arms and the anchor between
/// them catch replacement, truncation, rotation, a backwards mtime, and any
/// rewrite touching the last [`ANCHOR_BYTES`] of the consumed region. They do
/// NOT catch an in-place edit further back on a still-appending file — see the
/// note on `ANCHOR_BYTES`. Without a timer that case is wrong forever, which
/// is a different kind of claim from "one poll window of staleness"; with one
/// it is wrong for at most this long.
///
/// Five minutes against a 1 s poll means the incremental path still does
/// roughly 1/300th of the work the whole-file re-parse did, so the guarantee
/// costs almost none of the win. It is deliberately shorter than
/// [`CURSOR_TTL`], so a cursor that survives eviction has revalidated at least
/// once in between.
const CURSOR_REVALIDATE_AFTER: Duration = Duration::from_secs(300);

/// A cursor untouched for this long is dropped, so the map is bounded by
/// transcripts being POLLED rather than by every transcript this process ever
/// read. There is no lifecycle event to hang eviction on — a cursor outlives
/// its agent's pane on purpose (`mark_dead` reads usage after teardown) — so a
/// TTL is the honest bound. Ten minutes is far longer than the 1 s poll, so a
/// live agent never loses its cursor and a gone one costs at most one more
/// full re-parse than it had to.
const CURSOR_TTL: Duration = Duration::from_secs(600);

/// What one cursor read actually cost and what it decided.
///
/// The whole point of #1239 is a WORK bound, and a work bound is not
/// observable from the totals — the old whole-file re-parse produced exactly
/// the same numbers. So the reader reports its own cost, incremented at the
/// single place bytes leave the disk, and the tests assert on that. Production
/// ignores it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorWork {
    /// Bytes pulled off the disk by this read: the appended region, plus the
    /// anchor re-read, plus (on a reset or a first read) the whole file.
    pub bytes_read: u64,
    /// An existing cursor was discarded and the transcript re-folded from byte
    /// zero. False on a first read, which has no cursor to discard.
    pub reset: bool,
    /// The file's `len` and `mtime` were unchanged, so the cached totals were
    /// served without opening it at all.
    pub served_cached: bool,
    /// The projects root was scanned for the transcript file. True on a first
    /// read and whenever the remembered path stops being a file.
    pub scanned_root: bool,
    /// The cursor was discarded because it had been folding incrementally for
    /// [`CURSOR_REVALIDATE_AFTER`], not because any guard objected. Always
    /// accompanied by `reset`; this only says WHY.
    pub revalidated: bool,
}

/// Which CLI's transcript records a cursor is folding (#2126).
///
/// **The generalisation is a per-CLI LINE FOLDER, not a second cursor cache**,
/// and that is the whole design decision. Everything the cursor does that is
/// hard to get right — the stat verdict, the anchor re-read through the same
/// handle, holding back a partial trailing line, the revalidation timer, the
/// TTL eviction, the streaming read — is a fact about an append-only JSONL file
/// and about nothing else. A second cache would have been a second copy of all
/// of it, kept in step by review; one cache with a per-CLI [`TranscriptFolder`]
/// means a fix to any of those guards lands for every harness at once, and a
/// new harness supplies a `push`/`usage` pair and inherits the rest.
///
/// The two arms differ in exactly two places: what a line MEANS (the folder)
/// and where the file LIVES ([`transcript_path`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TranscriptKind {
    /// `~/.claude/projects/<encoded-cwd>/<session>.jsonl`, folded by
    /// [`TranscriptFold`].
    Claude,
    /// `<group>/pi/sessions/<timestamp>_<session>.jsonl`, folded by [`PiFold`].
    Pi,
    /// `<codex home>/sessions/YYYY/MM/DD/rollout-<ts>-<thread>[_<rollout>].jsonl`
    /// in the HUMAN's own store, folded by [`CodexFold`] (#2515 C3).
    ///
    /// The odd one out in this enum, and worth saying where the variants are
    /// listed: the other two roots hold one file per session at a name a
    /// caller can spell, and codex's holds a date tree whose file names carry
    /// a timestamp nobody can re-derive. So its [`transcript_path`] arm is a
    /// LOOKUP over the store rather than a join, and the answer is remembered
    /// on the cursor exactly as claude's scan result is.
    Codex,
}

/// The per-CLI half of a cursor: how one line of THIS harness's transcript
/// changes the running totals, and what those totals currently say.
///
/// An enum rather than a `Box<dyn Fold>` because a cursor has to be able to
/// build a FRESH folder of its own kind on every reset, and a trait object
/// cannot do that without a second factory to carry alongside it. Both arms are
/// concrete, both are in this module, and the match is exhaustive — a new
/// harness that forgets an arm does not compile.
enum TranscriptFolder {
    Claude(TranscriptFold),
    Pi(PiFold),
    Codex(CodexFold),
}

impl TranscriptFolder {
    fn new(kind: TranscriptKind) -> Self {
        match kind {
            TranscriptKind::Claude => TranscriptFolder::Claude(TranscriptFold::default()),
            TranscriptKind::Pi => TranscriptFolder::Pi(PiFold::default()),
            TranscriptKind::Codex => TranscriptFolder::Codex(CodexFold::default()),
        }
    }

    fn push(&mut self, line: &str) {
        match self {
            TranscriptFolder::Claude(f) => f.push(line),
            TranscriptFolder::Pi(f) => f.push(line),
            TranscriptFolder::Codex(f) => f.push(line),
        }
    }

    fn usage(&self) -> SessionUsage {
        match self {
            TranscriptFolder::Claude(f) => f.usage(),
            TranscriptFolder::Pi(f) => f.usage(),
            TranscriptFolder::Codex(f) => f.usage(),
        }
    }
}

/// Locate the transcript file a session wrote, for either harness.
///
/// The one place the two stores' shapes differ, and both arms delegate to the
/// module that already owns that shape rather than re-deriving it here: a
/// second spelling of "which file is this session's" is exactly the
/// disagreement `pi_sessions_in`'s own doc warns about.
///
/// pi's locator returns a `Result` whose `Err` is a store-level read failure;
/// this reader has one degrade channel (`None` — no usage for that id), which
/// is the same answer it already gives for a transcript that has not been
/// written yet, so both non-answers collapse into it.
///
/// codex's arm is a LOOKUP and cannot be anything else: a rollout is named
/// `rollout-<ts>-<thread>[_<rollout>].jsonl` under a date tree, so no
/// `format!("{session}.jsonl")` could ever name it — the timestamp is not
/// derivable and the revert suffix is not predictable. It answers `None` for a
/// COMPRESSED rollout, which is a decision rather than a miss; see
/// [`loomux_engine::sessions::find_codex_session_file`].
fn transcript_path(kind: TranscriptKind, root: &Path, session: &PathSegment) -> Option<PathBuf> {
    match kind {
        TranscriptKind::Claude => claude_transcript_path(root, session),
        TranscriptKind::Pi => {
            crate::orchestration::pi_session_file_in_dir(root, session).ok().flatten()
        }
        TranscriptKind::Codex => loomux_engine::sessions::find_codex_session_file(root, session),
    }
}

/// One transcript's parse position and everything needed to resume from it.
struct TranscriptCursor {
    /// The transcript file this cursor is bound to.
    ///
    /// Remembered rather than re-resolved: `claude_transcript_path` scans
    /// every project folder under the root, and doing that once a second per
    /// live agent is the same class of waste as the re-parse. It is
    /// re-validated with one `is_file()` per tick and re-scanned when that
    /// fails. The scan picks whichever project folder yields a match first,
    /// which is directory order and was already arbitrary when it ran every
    /// tick; pinning it makes that choice stable instead of stable-by-luck.
    path: PathBuf,
    /// The fold this cursor resumes.
    fold: TranscriptFolder,
    /// Bytes CONSUMED. Always sits immediately after a `\n`, so resuming from
    /// it can never land mid-record.
    offset: u64,
    /// File length as of the stat this cursor last acted on. May exceed
    /// `offset` when the tail is a line whose newline has not arrived yet.
    len: u64,
    modified: Option<SystemTime>,
    /// Creation time where the platform reports one (`Err` on filesystems
    /// without a birth time — Linux ext4 without statx, for instance — and
    /// then simply not a signal).
    ///
    /// It is a weaker signal on Windows than it looks: NTFS **file tunneling**
    /// deliberately restores the ORIGINAL creation timestamp when a name is
    /// deleted and recreated in the same directory inside a ~15 s window, so a
    /// quick rotation through the same path can present as the same file. That
    /// is why this arm is defence-in-depth over the anchor and the length
    /// rather than the thing relied on, and why the test for it probes the
    /// real before/after times instead of assuming they differ.
    created: Option<SystemTime>,
    /// The last [`ANCHOR_BYTES`] of the consumed region.
    anchor: Vec<u8>,
    /// When this cursor last started folding from byte zero — the clock
    /// [`CURSOR_REVALIDATE_AFTER`] runs against.
    built: Instant,
}

/// The stat-level decision for a cursor, before any bytes are read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatVerdict {
    /// Nothing changed: serve the cached totals, open nothing.
    Serve,
    /// The file grew (or its mtime moved): fold the appended region on.
    Extend,
    /// The file was replaced, truncated or rewritten: throw the cursor away.
    Reset,
}

impl TranscriptCursor {
    /// A cursor at byte zero for `kind`'s record shape. The kind is consumed
    /// here — it selects the folder and is then no longer a fact the cursor
    /// needs, because every caller reaches a cursor through
    /// [`TranscriptCursors`]'s map, whose KEY carries it.
    fn new(kind: TranscriptKind, path: PathBuf) -> Self {
        TranscriptCursor {
            path,
            fold: TranscriptFolder::new(kind),
            offset: 0,
            len: 0,
            modified: None,
            created: None,
            anchor: Vec::new(),
            built: Instant::now(),
        }
    }

    /// Classify what the file's current stat means for this cursor.
    ///
    /// The three reset arms, and why each is a reset rather than an extend:
    ///
    /// - **A different creation time** is a different FILE at the same path —
    ///   a rotation, or a delete-and-rewrite. Nothing about the old offset
    ///   refers to anything in it.
    /// - **A shorter file** cannot be the one we consumed: a transcript only
    ///   ever grows, so a shrink means it was truncated or rewritten. This is
    ///   the arm the issue calls out as the one to get right, and it is
    ///   deliberately `len < self.len` (shrank AT ALL) rather than
    ///   `len < self.offset` (shrank past what we consumed): a truncation that
    ///   still leaves our offset addressable tells us just as loudly that the
    ///   file is not the one we were reading.
    /// - **An mtime that moved BACKWARDS** is a file restored from elsewhere —
    ///   a copy, a sync, a checkout — over the one we were reading.
    ///
    /// `Serve` demands BOTH an unchanged length and an unchanged mtime, so a
    /// same-length rewrite reaches [`StatVerdict::Extend`] and then has to get
    /// past the anchor.
    ///
    /// **What no arm here detects** (#1361 review B1, corrected from a much
    /// narrower claim): an in-place edit to a consumed byte earlier than
    /// `offset - ANCHOR_BYTES`, on a file that keeps being appended to. It
    /// does not shrink the file, does not move the mtime backwards, does not
    /// change the creation time, and does not touch the anchor window — so
    /// every arm below agrees, the edited bytes are never re-read, and no
    /// later append re-decides anything. It is NOT "one poll window of stale
    /// totals": nothing in this function or the anchor will ever notice it.
    /// [`CURSOR_REVALIDATE_AFTER`] is what bounds it, by discarding the cursor
    /// on a timer regardless of what these arms say.
    fn stat_verdict(
        &self,
        len: u64,
        modified: Option<SystemTime>,
        created: Option<SystemTime>,
    ) -> StatVerdict {
        if let (Some(now), Some(then)) = (created, self.created) {
            if now != then {
                return StatVerdict::Reset;
            }
        }
        if len < self.len {
            return StatVerdict::Reset;
        }
        if let (Some(now), Some(then)) = (modified, self.modified) {
            if now < then {
                return StatVerdict::Reset;
            }
        }
        if len == self.len && modified == self.modified {
            return StatVerdict::Serve;
        }
        StatVerdict::Extend
    }

    /// Extend the anchor with bytes just consumed, keeping only the last
    /// [`ANCHOR_BYTES`].
    fn push_anchor(&mut self, bytes: &[u8]) {
        self.anchor.extend_from_slice(bytes);
        let excess = self.anchor.len().saturating_sub(ANCHOR_BYTES);
        if excess > 0 {
            self.anchor.drain(..excess);
        }
    }
}

/// What an attempt to advance a cursor produced.
enum Advance {
    /// The appended complete lines were folded on; the payload is the bytes
    /// read off disk.
    Folded(u64),
    /// The anchor re-read did not match what the cursor folded there, so the
    /// consumed region has changed under us and NOTHING was folded. The
    /// caller discards the cursor and re-parses from zero.
    AnchorMismatch(u64),
}

/// Fold every COMPLETE line from `cursor.offset` to end-of-file into the
/// cursor, advancing its offset and anchor.
///
/// **`verify_anchor` reads the proof through the SAME handle as the fold, and
/// that is a correctness requirement, not a saved syscall.** Checking the
/// anchor through a handle of its own would leave a window in which the file
/// is replaced between the proof and the read — the cursor would verify one
/// file and then resume into another, which is the one way this design could
/// produce a WRONG total rather than merely a slow tick. Reading the anchor
/// also leaves the handle sitting at exactly `offset`, so the check costs one
/// seek and [`ANCHOR_BYTES`].
///
/// An I/O failure on the anchor read is a mismatch, not an error: a cursor
/// that cannot prove its position is not one to resume from. A cursor with no
/// anchor yet (nothing consumed) has nothing to disagree with and never
/// reports one.
///
/// **A partial trailing line is read but NOT consumed.** A JSONL writer
/// appends a record and its newline as separate bytes, so a poll can land
/// between them; the offset therefore only ever advances past a `\n`. The
/// alternative — folding a torn record — is not "one tick early", it is
/// permanently wrong: a truncated line either fails to parse (and is then
/// skipped forever, losing that message's tokens) or, worse, parses with
/// truncated numbers. Holding back costs at most one poll window of freshness
/// on the newest message, and the bytes are re-read next tick.
///
/// **A line that is not valid UTF-8 is skipped, not fatal.** The reader this
/// replaced used `.lines().map_while(Result::ok)`, which STOPS at the first
/// such line — one bad byte silently truncated a whole session's usage to
/// whatever preceded it. Skipping is both the strictly better answer and the
/// only one a cursor can hold: stalling at a line forever would freeze the
/// offset there.
///
/// Streaming, per #1218: one reusable line buffer, never the file. Peak live
/// bytes are the longest single line plus whatever state the folder carries —
/// [`TranscriptFold`]'s message-id dedupe set on a `Claude` cursor, and four
/// counters plus one model id on a `Pi` or `Codex` one, neither of which has
/// such a set (see [`PiFold`] and [`CodexFold`] for why neither needs one).
fn fold_appended(cursor: &mut TranscriptCursor, verify_anchor: bool) -> std::io::Result<Advance> {
    let mut file = fs::File::open(&cursor.path)?;
    let mut bytes_read = 0u64;
    if verify_anchor && !cursor.anchor.is_empty() {
        let start = cursor.offset.saturating_sub(cursor.anchor.len() as u64);
        let mut got = vec![0u8; cursor.anchor.len()];
        let read = file.seek(SeekFrom::Start(start)).is_ok() && file.read_exact(&mut got).is_ok();
        bytes_read += got.len() as u64;
        if !read || got != cursor.anchor {
            return Ok(Advance::AnchorMismatch(bytes_read));
        }
        // The handle now sits at exactly `cursor.offset`.
    } else if cursor.offset > 0 {
        // Reached only when the anchor branch above did NOT run, which is sound
        // because a non-zero offset always carries a non-empty anchor:
        // `push_anchor` runs on every consumed line, so `anchor.len()` is
        // `min(offset, ANCHOR_BYTES)` by construction. If that ever stopped
        // holding, a verify_anchor tick with a non-empty offset would skip the
        // seek and re-fold the whole file from byte zero onto a cursor that
        // already holds it (#1361 review, latent-coupling note).
        file.seek(SeekFrom::Start(cursor.offset))?;
    }
    let mut reader = BufReader::new(file);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break; // end of file
        }
        bytes_read += n as u64;
        if buf.last() != Some(&b'\n') {
            break; // partial trailing line — leave it for a later tick
        }
        if let Ok(line) = std::str::from_utf8(&buf) {
            cursor.fold.push(line);
        }
        cursor.offset += n as u64;
        cursor.push_anchor(&buf);
    }
    Ok(Advance::Folded(bytes_read))
}

/// Per-transcript parse cursors, so a usage poll parses only what an agent
/// has WRITTEN since the previous poll (#1239).
///
/// **The problem.** `compute_group_usage` runs on the app's hottest poll, at
/// most once per `USAGE_POLL_MAX_AGE` (1 s), and re-read and re-parsed every
/// live agent's ENTIRE transcript on each tick — tens of MiB per agent on a
/// multi-day session, `serde_json` over every line of it, the message-id
/// dedupe set rebuilt from scratch — to advance four totals by a few lines.
/// #1218/#1237 bounded the MEMORY of that read (it streams); they did not
/// remove the work. The minidump behind #1239 records 1,701,161,634 page
/// faults on the process, and this loop is that churn.
///
/// **The contract.** A cursor holds the byte offset it has consumed up to, the
/// fold resumed from there — a [`TranscriptFolder`], whose per-CLI arm decides
/// what that state IS: `Claude` carries totals, cost, best model and the
/// message-id dedupe set that makes resuming safe against a `--resume`
/// re-emit; `Pi` carries totals, pi's own summed `cost.total` and the last
/// assistant turn's model, and needs no dedupe set because pi appends fresh
/// entry ids rather than re-emitting — and the stat it last acted on. Per
/// tick:
///
/// - unchanged `len` and `mtime` → the cached totals, with the file not
///   opened at all;
/// - grown → through ONE handle: re-read the anchor, and if it still matches,
///   fold the appended complete lines on from where that read left off;
/// - replaced, truncated or rewritten → discard the cursor and re-parse from
///   zero, which is exactly the pre-#1239 cost. The failure mode of every
///   guard here is "as slow as it used to be", never a wrong total.
///
/// **`Arc<TrackedMutex<..>>` per transcript, not one map lock** — the same
/// map-lock → release → leaf-lock rule the usage memo follows. The outer lock
/// is held only long enough to clone one cell out, so a full re-parse for one
/// agent never blocks another agent's tick.
///
/// **Both are `TrackedMutex` (#1601, #1605 review N1), and this is the pair
/// that most needed to be.** They are reachable from `OrchRegistry` (through
/// the `usage_cursors` field), they are on `orch_group_usage` — one of the ten
/// polled reads — and the per-transcript cell is held across `fs::metadata`
/// and `fold_appended`, which is real file IO inside the critical section. A
/// registry-wide scan could not see them, because it reads each field's
/// declared TYPE and this field's type is a struct that OWNS its mutexes;
/// `every_lock_on_the_registry_is_a_tracked_one` now names that blind spot
/// instead of claiming the population is complete.
///
/// Tracking makes that hold VISIBLE. It does not make it short — bounding IO
/// under a polled path's guard is Phase 1/2 of
/// `docs/plans/responsiveness-root-cause.md`, not this change. What #1601 buys
/// here is that if this IS the hold that wedges a build, the breadcrumb names
/// it instead of a human having to guess.
pub struct TranscriptCursors {
    /// Keyed by (harness, store root, session id). The `kind` is in the key
    /// rather than merely inside the cursor because the harnesses' roots are
    /// different directories today and nothing here should depend on that
    /// staying true — a cache that answered a pi read out of a claude cursor
    /// because the paths collided would produce a WRONG total, which is the one
    /// failure this whole design refuses.
    ///
    /// That is not hypothetical for codex: its store layout is a date TREE
    /// (`<root>/YYYY/MM/DD/`) and claude's is a folder per encoded cwd, so the
    /// two can share one root directory without either noticing.
    /// `a_codex_cursor_and_a_claude_cursor_never_serve_each_others_totals`
    /// (`tests/codexusage.rs`) builds exactly that fixture — one root, one
    /// session id, two stores — so the key's `kind` is pinned by a case where
    /// dropping it really does collide, rather than by two disjoint paths that
    /// would hold either way.
    cursors: TrackedMutex<HashMap<(TranscriptKind, PathBuf, String), CursorEntry>>,
    /// How long any one cursor may fold incrementally before it is discarded
    /// and re-parsed from zero. [`CURSOR_REVALIDATE_AFTER`] in production;
    /// a parameter only so the tests can exercise the timer without waiting
    /// on a real clock.
    revalidate_after: Duration,
}

impl Default for TranscriptCursors {
    fn default() -> Self {
        TranscriptCursors::with_revalidate_after(CURSOR_REVALIDATE_AFTER)
    }
}

struct CursorEntry {
    /// Last time this cursor was asked for — drives [`CURSOR_TTL`] eviction.
    used: Instant,
    cursor: Arc<TrackedMutex<Option<TranscriptCursor>>>,
}

impl TranscriptCursors {
    /// A cache whose cursors revalidate on the given interval instead of the
    /// production [`CURSOR_REVALIDATE_AFTER`].
    ///
    /// `#[doc(hidden)] pub` for the integration tests. The timer is the whole
    /// of what bounds the blind spot documented on [`ANCHOR_BYTES`], so it has
    /// to be testable; waiting five real minutes in a test is not a test.
    /// `Duration::ZERO` revalidates on every tick. Production constructs this
    /// type through `Default` and therefore never takes another value.
    #[doc(hidden)] // pub for integration tests
    pub fn with_revalidate_after(revalidate_after: Duration) -> Self {
        TranscriptCursors { cursors: TrackedMutex::new("usage_cursors", HashMap::new()), revalidate_after }
    }

    /// One session's usage, parsing only what was appended since this cache
    /// last read the same transcript. Same totals as the whole-file readers
    /// ([`claude_session_usage_in`], [`pi_session_usage_in`]), same `None`
    /// cases.
    ///
    /// `root` is the store `kind` names: claude's projects root, or a group's
    /// pi sessions directory.
    pub fn session_usage(
        &self,
        kind: TranscriptKind,
        root: &Path,
        session_id: &str,
    ) -> Option<SessionUsage> {
        self.session_usage_measured(kind, root, session_id).map(|(u, _)| u)
    }

    /// [`Self::session_usage`], also reporting what the read cost and what it
    /// decided ([`CursorWork`]).
    ///
    /// `#[doc(hidden)] pub` for the integration tests: the work bound is the
    /// property this whole type exists for, and it is invisible in the totals.
    #[doc(hidden)] // pub for integration tests
    pub fn session_usage_measured(
        &self,
        kind: TranscriptKind,
        root: &Path,
        session_id: &str,
    ) -> Option<(SessionUsage, CursorWork)> {
        // Same refusal-into-the-existing-`None` channel as
        // `claude_session_usage_in` (#925): an id that is not a usable path
        // component never reaches the join.
        let session = PathSegment::parse(session_id).ok()?;

        let cell = {
            let mut map = self.cursors.lock_safe();
            map.retain(|_, e| e.used.elapsed() < CURSOR_TTL);
            let entry = map
                .entry((kind, root.to_path_buf(), session_id.to_string()))
                .or_insert_with(|| CursorEntry {
                    used: Instant::now(),
                    cursor: Arc::new(TrackedMutex::new("usage_cursor_cell", None)),
                });
            entry.used = Instant::now();
            entry.cursor.clone()
        };
        let mut slot = cell.lock_safe();
        let mut work = CursorWork::default();
        // Captured BEFORE the path re-resolution below, which drops a cursor of
        // its own when the remembered file has gone: `reset` means "a cursor was
        // discarded", and reading it off `slot` afterwards would miss that one.
        let had_cursor = slot.is_some();

        // Is this cursor due for revalidation? Asked HERE, above the path
        // step, because the answer decides whether the remembered path may be
        // reused at all — see the block below.
        let due = slot.as_ref().is_some_and(|c| c.built.elapsed() >= self.revalidate_after);

        // ONE stat per tick, and it does double duty: the same `metadata` call
        // that answers `len`/mtime/creation is what validates the remembered
        // path. Asking `is_file()` first would be a second stat on the app's
        // hottest poll and would falsify the "one stat, then" claim this
        // design is documented with (#1361 review N4).
        //
        // **A due cursor drops its remembered PATH too, not just its fold**
        // (#2515 C3 review round 1, finding 1). The remembered path is
        // re-validated by that stat alone, so it survives for as long as the
        // file it names exists — and "the file still exists" is not the same
        // question as "this is still the session's file". codex makes the two
        // come apart on an ordinary gesture: `thread/revert` keeps the thread
        // id, starts a NEW rollout and switches the thread to it, leaving the
        // old file on disk and perfectly readable. Without this the cursor goes
        // on folding the superseded file for the life of the process — a live
        // pane whose usage silently stops moving — because nothing it checks
        // ever objects. Re-resolving on the revalidation tick bounds that the
        // same way the timer already bounds the anchor's blind spot, and costs
        // one lookup per `CURSOR_REVALIDATE_AFTER` per live agent.
        //
        // It is not codex-only on purpose: claude's transcript can be moved
        // between project folders and pi's store rewritten, and a second,
        // per-harness path-refresh policy would be one more thing to keep in
        // step. The other harnesses simply re-resolve to the path they already
        // had.
        let remembered = if due { None } else { slot.as_ref().map(|c| c.path.clone()) };
        let (path, meta) = match remembered.and_then(|p| {
            let m = fs::metadata(&p).ok()?;
            m.is_file().then_some((p, m))
        }) {
            Some(found) => found,
            None => {
                work.scanned_root = true;
                *slot = None;
                let p = transcript_path(kind, root, &session)?;
                let m = fs::metadata(&p).ok()?;
                (p, m)
            }
        };
        let (len, modified, created) = (meta.len(), meta.modified().ok(), meta.created().ok());

        // Decide from the stat; the anchor then gets to overrule an `Extend`,
        // from inside the same read that would have folded onto it.
        //
        // The age check comes FIRST and overrules both. It is not a guard that
        // detected anything — it is the timer that bounds what the guards
        // structurally cannot detect (see `ANCHOR_BYTES` and
        // `CURSOR_REVALIDATE_AFTER`), so it has to fire even on a tick where
        // every other signal is content.
        // `revalidated` says WHY a reset happened, and the re-resolution above
        // has already cleared `slot` when `due`, so it is recorded from `due`
        // rather than re-derived from a cursor that is no longer there.
        work.revalidated = due;
        let mut verdict = match slot.as_ref() {
            None => StatVerdict::Reset,
            Some(c) => match c.stat_verdict(len, modified, created) {
                StatVerdict::Serve => {
                    work.served_cached = true;
                    return Some((c.fold.usage(), work));
                }
                other => other,
            },
        };

        if verdict == StatVerdict::Extend {
            match fold_appended(slot.as_mut()?, true).ok()? {
                Advance::Folded(n) => work.bytes_read += n,
                Advance::AnchorMismatch(n) => {
                    work.bytes_read += n;
                    verdict = StatVerdict::Reset;
                }
            }
        }

        if verdict == StatVerdict::Reset {
            // `reset` is "a cursor was thrown away", so a first-ever read —
            // which also parses from zero — is not one.
            work.reset = had_cursor;
            *slot = Some(TranscriptCursor::new(kind, path));
            // A fresh cursor has no anchor, so this call cannot report a
            // mismatch; both arms mean the same thing here.
            work.bytes_read += match fold_appended(slot.as_mut()?, false).ok()? {
                Advance::Folded(n) | Advance::AnchorMismatch(n) => n,
            };
        }

        let cursor = slot.as_mut()?;
        cursor.len = len;
        cursor.modified = modified;
        cursor.created = created;
        Some((cursor.fold.usage(), work))
    }
}

// ---------------------------------------------------------------------------
// OpenCode session store (#722)
// ---------------------------------------------------------------------------

/// Read an OpenCode session's usage from the SQLite store at `db` — the
/// group's own database, per `OPENCODE_DB` (see `docs/design/opencode.md`).
///
/// `Ok(None)` is "readable store, no such session yet"; `Err` is a degrade the
/// caller reports as zero usage (`crate::opencodedb::Unavailable`).
///
/// **Two mapping decisions, both lossy, both deliberate:**
///
/// - **Reasoning tokens are folded into `output_tokens`.** OpenCode counts
///   them in a fifth bucket loomux has no column for, and dropping them would
///   under-report — a real session on this machine spent 1193 reasoning
///   tokens against 1115 output ones, so the fold is the difference between
///   roughly right and roughly half. `output` is also where they already sit
///   for the CLI loomux compares against: Claude's transcript counts thinking
///   inside `output_tokens`, so folding makes the two CLIs' `output` bucket
///   mean the same thing rather than two different things under one label.
/// - **`cache_write` maps to `cache_creation_tokens`.** Same quantity under
///   two vendors' names — tokens written INTO the cache, which is what
///   Claude's `cache_creation_input_tokens` counts.
///
/// The dollar figure passes through untouched and is **reported**, not
/// estimated: OpenCode priced it against its own provider table, so no
/// `price_for` lookup happens and callers must not label it an estimate.
/// `Some(0.0)` is a real answer on a free model, not a missing one.
pub fn opencode_session_usage(
    db: &Path,
    session_id: &str,
) -> Result<Option<SessionUsage>, crate::opencodedb::Unavailable> {
    Ok(crate::opencodedb::session_usage(db, session_id)?.map(|t| SessionUsage {
        tokens: TokenUsage {
            input_tokens: t.input,
            output_tokens: t.output + t.reasoning,
            cache_creation_tokens: t.cache_write,
            cache_read_tokens: t.cache_read,
        },
        cost_usd: Some(t.cost_usd),
        // opencode rewrites the root `session.model` on every prompt whose
        // model differs from the stored one (`setAgentModel`, called from
        // `SessionPrompt` at the `v1.18.11` pin), so the column is the
        // current model, not the one the session was created with.
        current_model: t.model.clone(),
        model: t.model,
        // The session row is a running total with no per-turn record, so
        // there is no first turn to read and no cache lifetime either.
        first_context_tokens: None,
        detected_cache_ttl_minutes: None,
    }))
}

/// Bytes read from the END of a transcript or rollout for tail-based context
/// signals: the current context reading and compaction boundaries relevant to
/// the pane's current arm state, never the full session history, which can
/// reach many MB over a long-lived orchestrator. The bound is generous relative
/// to a handful of lines, even with a large tool-output turn.
const TRANSCRIPT_TAIL_READ_BYTES: u64 = 256 * 1024;

/// Read the last `TRANSCRIPT_TAIL_READ_BYTES` of `path`, discarding a
/// possibly-truncated leading partial line (unless the read reached the true
/// start of the file, in which case there's nothing to truncate). `None` on
/// any I/O failure. Shared by the Claude transcript, Codex rollout and pi
/// session readers.
pub(crate) fn read_transcript_tail(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TRANSCRIPT_TAIL_READ_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    if start == 0 {
        return Some(text);
    }
    match text.find('\n') {
        Some(idx) => Some(text[idx + 1..].to_string()),
        None => Some(String::new()), // the whole read was one truncated line
    }
}

/// Read a Claude session's CURRENT context-window usage (#328) — see
/// `latest_context_tokens` — from a transcript under an explicit projects
/// `root`. `None` when the transcript can't be found/opened or carries no
/// real assistant turn yet. A thin convenience wrapper over
/// `compaction_signal_in` for callers that only need the token half.
pub fn claude_context_tokens_in(root: &Path, session_id: &str) -> Option<u64> {
    compaction_signal_in(root, session_id)?.tokens
}

/// Read BOTH compaction-confirmation signals (`latest_context_tokens` and
/// `compact_boundary_count`) from a single bounded tail read of a Claude
/// session's transcript. `None` when the transcript can't be found/opened;
/// `tokens` is separately `None` within a `Some(CompactionSignal)` when no
/// real assistant turn has landed in the tail window (matching `latest_
/// context_tokens`'s own `None` case) — `compact_boundary_count` still
/// reports 0 in that case rather than failing the whole read, since a
/// boundary marker's absence is itself a meaningful, distinct fact.
pub fn compaction_signal_in(root: &Path, session_id: &str) -> Option<CompactionSignal> {
    // Same refusal-into-the-existing-`None` channel as `claude_session_usage_in`
    // (#925): an id that is not a usable path component never reaches the join.
    let session = PathSegment::parse(session_id).ok()?;
    let path = claude_transcript_path(root, &session)?;
    let text = read_transcript_tail(&path)?;
    Some(CompactionSignal {
        tokens: latest_context_tokens(&text),
        compact_boundary_count: compact_boundary_count(&text),
        model: latest_context_model(&text),
        window_tokens: None,
        window_rounded: false,
        effort: None,
        effort_is_launch_fallback: false,
        source: crate::modelstate::ContextSource::Transcript,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One assistant transcript line with the given usage + model.
    fn line(id: &str, model: &str, input: u64, output: u64, cw: u64, cr: u64) -> String {
        serde_json::json!({
            "type": "assistant",
            "requestId": format!("req_{id}"),
            "message": {
                "id": id,
                "model": model,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_creation_input_tokens": cw,
                    "cache_read_input_tokens": cr,
                }
            }
        })
        .to_string()
    }

    #[test]
    fn sums_tokens_and_prices_by_model() {
        let text = [
            line("msg-1", "claude-opus-4-8", 100, 200, 50, 1000),
            line("msg-2", "claude-opus-4-8", 10, 20, 0, 500),
        ]
        .join("\n");
        let u = parse_claude_transcript(&text);
        assert_eq!(u.tokens.input_tokens, 110);
        assert_eq!(u.tokens.output_tokens, 220);
        assert_eq!(u.tokens.cache_creation_tokens, 50);
        assert_eq!(u.tokens.cache_read_tokens, 1500);
        assert_eq!(u.tokens.total(), 110 + 220 + 50 + 1500);
        // Opus: (110*5 + 220*25 + 50*6.25 + 1500*0.5) / 1e6
        let expect = (110.0 * 5.0 + 220.0 * 25.0 + 50.0 * 6.25 + 1500.0 * 0.5) / 1_000_000.0;
        assert!((u.cost_usd.unwrap() - expect).abs() < 1e-12, "got {:?}", u.cost_usd);
        assert_eq!(u.model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn dedupes_repeated_message_ids() {
        // A resumed transcript re-emits msg-1; it must be counted once.
        let text = [
            line("msg-1", "claude-sonnet-5", 100, 200, 0, 0),
            line("msg-1", "claude-sonnet-5", 100, 200, 0, 0),
            line("msg-2", "claude-sonnet-5", 5, 5, 0, 0),
        ]
        .join("\n");
        let u = parse_claude_transcript(&text);
        assert_eq!(u.tokens.input_tokens, 105, "duplicate id must not double-count");
        assert_eq!(u.tokens.output_tokens, 205);
    }

    #[test]
    fn skips_non_assistant_and_synthetic_and_malformed() {
        let text = [
            r#"{"type":"summary","summary":"a title"}"#.to_string(),
            r#"{"type":"user","message":{"content":"hi"}}"#.to_string(),
            "not json at all".to_string(),
            line("real", "claude-haiku-4-5", 40, 60, 0, 0),
            // Synthetic: contributes tokens but no model/price.
            line("synth", "<synthetic>", 1, 1, 0, 0),
        ]
        .join("\n");
        let u = parse_claude_transcript(&text);
        assert_eq!(u.tokens.input_tokens, 41);
        assert_eq!(u.tokens.output_tokens, 61);
        // Priced only off the haiku line.
        let expect = (40.0 * 1.0 + 60.0 * 5.0) / 1_000_000.0;
        assert!((u.cost_usd.unwrap() - expect).abs() < 1e-12);
        assert_eq!(u.model.as_deref(), Some("claude-haiku-4-5"));
    }

    #[test]
    fn unknown_model_yields_tokens_but_no_cost() {
        let text = line("m", "some-future-model-9", 100, 100, 0, 0);
        let u = parse_claude_transcript(&text);
        assert_eq!(u.tokens.total(), 200);
        assert_eq!(u.cost_usd, None, "unknown model must fall back to token-only");
        assert_eq!(u.model, None);
    }

    #[test]
    fn empty_transcript_is_zero_not_a_panic() {
        let u = parse_claude_transcript("");
        assert_eq!(u.tokens.total(), 0);
        assert_eq!(u.cost_usd, None);
    }

    #[test]
    fn price_table_matches_known_families() {
        assert!(price_for("claude-opus-4-8").is_some());
        assert!(price_for("claude-sonnet-5").is_some());
        assert!(price_for("claude-haiku-4-5").is_some());
        assert!(price_for("claude-fable-5").is_some());
        assert!(price_for("gpt-4o").is_none());
    }

    // ---------- latest_context_tokens (#328) ----------

    #[test]
    fn latest_context_tokens_reads_the_last_real_turn_not_the_cumulative_sum() {
        // The whole point of this fn vs `parse_claude_transcript`: context
        // fullness is what the MOST RECENT turn sent, not the running total
        // across the session.
        let text = [
            line("t1", "claude-sonnet-5", 50_000, 500, 0, 0),
            line("t2", "claude-sonnet-5", 80_000, 500, 0, 20_000),
        ]
        .join("\n");
        // Cumulative sum (what parse_claude_transcript reports) would be
        // 130_000 input tokens; the LATEST turn's context is 80_000 + 20_000
        // (cache read) = 100_000, a materially different figure.
        assert_eq!(latest_context_tokens(&text), Some(100_000));
        let cumulative = parse_claude_transcript(&text);
        assert_eq!(cumulative.tokens.input_tokens, 130_000, "sanity: cumulative really does differ");
    }

    #[test]
    fn latest_context_model_reads_the_same_latest_turn_tokens_does() {
        // PR #329 round 7: the two must never disagree about which turn is
        // "latest" — they share `latest_real_assistant_turn`.
        let text = [
            line("t1", "claude-sonnet-5", 50_000, 500, 0, 0),
            line("t2", "claude-opus-4-8", 80_000, 500, 0, 20_000),
        ]
        .join("\n");
        assert_eq!(latest_context_model(&text).as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn latest_context_model_skips_synthetic_and_non_assistant_lines_same_as_tokens() {
        let text = [
            r#"{"type":"user","message":{"content":"hi"}}"#.to_string(),
            line("synth", "<synthetic>", 1, 1, 0, 0),
            line("real", "claude-opus-4-8", 10, 10, 0, 0),
        ]
        .join("\n");
        assert_eq!(latest_context_model(&text).as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn latest_context_model_none_when_no_real_turn_exists() {
        assert_eq!(latest_context_model(""), None);
        assert_eq!(latest_context_model("not json\n{\"type\":\"user\"}"), None);
    }

    #[test]
    fn claude_context_window_tokens_defaults_conservative_and_widens_only_for_opus() {
        // PR #329 round 7: live evidence — a hardcoded 200K flat assumption
        // read a 1M-context Opus session's usage as ~5x too full (26% vs the
        // CLI's own reported ~5%). Opus is the one family with concrete
        // evidence of a larger tier; everything else, and an absent/
        // unrecognized model, falls back to the documented conservative
        // default (the safe direction when unsure: NEVER assume a bigger
        // window than reality, which would delay a needed compaction).
        assert_eq!(claude_context_window_tokens(Some("claude-opus-4-8")), 1_000_000);
        assert_eq!(claude_context_window_tokens(Some("claude-opus-4-7")), 1_000_000, "matches by family, like price_for");
        assert_eq!(claude_context_window_tokens(Some("claude-sonnet-5")), DEFAULT_CLAUDE_CONTEXT_WINDOW_TOKENS);
        assert_eq!(claude_context_window_tokens(Some("claude-haiku-4-5")), DEFAULT_CLAUDE_CONTEXT_WINDOW_TOKENS);
        assert_eq!(claude_context_window_tokens(Some("some-future-model-nobody-has-heard-of")), DEFAULT_CLAUDE_CONTEXT_WINDOW_TOKENS,
            "an unrecognized model must never silently widen the window — conservative fallback, not a guess in the unsafe direction");
        assert_eq!(claude_context_window_tokens(None), DEFAULT_CLAUDE_CONTEXT_WINDOW_TOKENS);
    }

    #[test]
    fn latest_context_tokens_self_corrects_after_a_compact() {
        // A compact's next turn sends far less context — the figure must
        // reflect that drop, not stay pinned to the pre-compact peak.
        let text = [
            line("before", "claude-sonnet-5", 180_000, 500, 0, 0),
            line("after-compact", "claude-sonnet-5", 8_000, 500, 0, 0),
        ]
        .join("\n");
        assert_eq!(latest_context_tokens(&text), Some(8_000));
    }

    /// A REAL (structurally trimmed, numbers untouched) excerpt from an actual
    /// dogfood session on this repo — `1aadeb3f-e8a1-4d29-88d4-7cf4b44ddf2a.jsonl`,
    /// `~/.claude/projects/C--Projects-loomux/`, 2026-07-15 — captured specifically
    /// to settle the rev-42 delta review's Q1: does `latest_context_tokens` see a
    /// compaction's drop before the next real assistant turn, or only after?
    /// Synthetic injection can't answer this (it assumes the very timing in
    /// question); this is the actual CLI's own transcript shape. Only the huge,
    /// parser-irrelevant fields (`preservedSegment`/`preCompactDiscoveredTools`
    /// arrays, the multi-paragraph summary prose) were elided for fixture size —
    /// every field either `latest_context_tokens` or `compact_boundary_count`
    /// reads is verbatim, including the exact token counts.
    const REAL_DOGFOOD_COMPACT_EXCERPT_PRE: &str =
        r#"{"type":"assistant","message":{"model":"claude-fable-5","usage":{"input_tokens":2,"output_tokens":1305,"cache_creation_input_tokens":48,"cache_read_input_tokens":516543}}}"#;
    const REAL_DOGFOOD_COMPACT_EXCERPT_BOUNDARY: &str =
        r#"{"type":"system","subtype":"compact_boundary","content":"Conversation compacted","level":"info","compactMetadata":{"trigger":"manual","preTokens":518258,"postTokens":7716,"cumulativeDroppedTokens":510542},"timestamp":"2026-07-15T01:46:54.839Z"}"#;
    // Interstitial bookkeeping lines the REAL transcript has between the
    // boundary and the next assistant turn — a synthetic continuation summary,
    // then (in the real file) several attachment-delta lines omitted here as
    // pure repetition, then a last-prompt marker. None are `type: "assistant"`.
    const REAL_DOGFOOD_COMPACT_EXCERPT_SUMMARY: &str =
        r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"[summary text elided for fixture size — real content is a multi-paragraph session recap]"}}"#;
    const REAL_DOGFOOD_COMPACT_EXCERPT_LASTPROMPT: &str =
        r#"{"type":"last-prompt","lastPrompt":"/compact"}"#;
    const REAL_DOGFOOD_COMPACT_EXCERPT_POST: &str =
        r#"{"type":"assistant","message":{"model":"claude-fable-5","usage":{"input_tokens":2,"output_tokens":1568,"cache_creation_input_tokens":15688,"cache_read_input_tokens":28268}}}"#;

    #[test]
    fn real_transcript_proves_the_token_drop_is_a_next_turn_phenomenon_rev42_q1() {
        // The window `compact_nudge_tick`'s resolver actually reads at: a
        // compact just completed (the boundary line exists), but the CLI
        // hasn't produced a new real assistant turn yet — only the synthetic
        // continuation summary and a last-prompt marker sit after it, exactly
        // as the real transcript shows.
        let before_next_turn = [
            REAL_DOGFOOD_COMPACT_EXCERPT_PRE,
            REAL_DOGFOOD_COMPACT_EXCERPT_BOUNDARY,
            REAL_DOGFOOD_COMPACT_EXCERPT_SUMMARY,
            REAL_DOGFOOD_COMPACT_EXCERPT_LASTPROMPT,
        ]
        .join("\n");
        // Real pre-compact figure: 2 + 48 + 516_543 = 516_593. Confirms rev-42's
        // Q1 empirically: `latest_context_tokens` is STILL pinned to the
        // pre-compact peak here — it has no way to know a compaction happened.
        assert_eq!(
            latest_context_tokens(&before_next_turn),
            Some(516_593),
            "before any new assistant turn, the reading must still show the STALE pre-compact value — this is the deadlock"
        );
        // But the boundary marker is ALREADY visible — no next turn required.
        assert_eq!(compact_boundary_count(&before_next_turn), 1,
            "compact_boundary_count sees the compaction immediately, unlike the token reading");

        // Now the next real assistant turn lands (the reinjection's own
        // response, in production) — only THEN does the token reading correct.
        let after_next_turn = format!("{before_next_turn}\n{REAL_DOGFOOD_COMPACT_EXCERPT_POST}");
        assert_eq!(
            latest_context_tokens(&after_next_turn),
            Some(2 + 15_688 + 28_268),
            "only once a new assistant turn exists does the drop become visible — confirms it's a next-turn phenomenon, not an at-compaction one"
        );
        assert_eq!(compact_boundary_count(&after_next_turn), 1, "still just the one real compaction");
    }

    #[test]
    fn compact_boundary_count_is_zero_when_absent_and_counts_every_real_boundary() {
        assert_eq!(compact_boundary_count(""), 0);
        assert_eq!(compact_boundary_count("not json\n{\"type\":\"user\"}"), 0);
        assert_eq!(compact_boundary_count(r#"{"type":"system","subtype":"other_thing"}"#), 0,
            "a different system subtype must not be mistaken for a compaction");
        let two_compactions = [
            REAL_DOGFOOD_COMPACT_EXCERPT_BOUNDARY,
            REAL_DOGFOOD_COMPACT_EXCERPT_POST,
            REAL_DOGFOOD_COMPACT_EXCERPT_BOUNDARY,
        ]
        .join("\n");
        assert_eq!(compact_boundary_count(&two_compactions), 2, "monotonically counts every boundary seen");
    }

    #[test]
    fn latest_context_tokens_skips_synthetic_and_non_assistant_lines() {
        let text = [
            r#"{"type":"summary","summary":"a title"}"#.to_string(),
            line("real", "claude-sonnet-5", 42_000, 100, 0, 1_000),
            // A trailing synthetic line (no real usage) must not be read as
            // "the latest turn" and mask the real one before it.
            line("synth", "<synthetic>", 999_999, 1, 0, 0),
        ]
        .join("\n");
        assert_eq!(latest_context_tokens(&text), Some(43_000));
    }

    #[test]
    fn latest_context_tokens_none_when_no_real_turn_exists() {
        assert_eq!(latest_context_tokens(""), None);
        assert_eq!(latest_context_tokens("not json\n{\"type\":\"user\"}"), None);
    }

    // ---------- the version-aware price table (#3831) ----------

    /// The five numbers of one vendor row, in the page's column order.
    fn cols(p: ModelPrice) -> [f64; 5] {
        [p.input, p.cache_write, p.cache_write_1h, p.cache_read, p.output]
    }

    fn listed(model: &str) -> [f64; 5] {
        let q = price_quote(model).unwrap_or_else(|| panic!("{model} is unpriced"));
        assert_eq!(q.basis, PriceBasis::Listed, "{model} should be a listed version");
        cols(q.price)
    }

    #[test]
    fn price_for_reads_the_version_and_not_only_the_family() {
        // The pair the family-only table could not tell apart: one family, two
        // input prices.
        assert_eq!(listed("claude-sonnet-5-5"), [2.0, 2.50, 4.0, 0.10, 10.0]);
        assert_eq!(listed("claude-sonnet-4-6"), [3.0, 3.75, 6.0, 0.30, 15.0]);
        // Same input price, different cache hit: 5 and 5.5 differ on one column.
        assert_eq!(listed("claude-sonnet-5"), [2.0, 2.50, 4.0, 0.20, 10.0]);
        assert_eq!(listed("claude-opus-5-5"), [4.0, 5.0, 8.0, 0.20, 20.0]);
        assert_eq!(listed("claude-opus-4-8"), [5.0, 6.25, 10.0, 0.50, 25.0]);
        assert_eq!(listed("claude-fable-5-1"), [10.0, 12.50, 20.0, 0.25, 50.0]);
        assert_eq!(listed("claude-fable-5"), [10.0, 12.50, 20.0, 1.0, 50.0]);
        assert_eq!(listed("claude-mythos-5-1"), listed("claude-fable-5-1"));
        assert_eq!(listed("claude-haiku-4-5"), [1.0, 1.25, 2.0, 0.10, 5.0]);
        // `price_for` is the same answer's base tier.
        assert_eq!(price_for("claude-sonnet-5-5").map(cols), Some([2.0, 2.50, 4.0, 0.10, 10.0]));
    }

    #[test]
    fn the_version_is_read_off_every_spelling_an_id_takes() {
        // A date is not a minor version, a suffix ends the version, a provider
        // prefix is ignored, and the pre-4 spelling puts the version first.
        assert_eq!(listed("claude-sonnet-4-5-20250929"), listed("claude-sonnet-4-5"));
        assert_eq!(listed("claude-opus-4-8[1m]"), listed("claude-opus-4-8"));
        assert_eq!(listed("us.anthropic.claude-opus-4-8-v1:0"), listed("claude-opus-4-8"));
        assert_eq!(listed("CLAUDE-OPUS-5-5"), listed("claude-opus-5-5"));
        assert_eq!(listed("claude-sonnet-4.6"), listed("claude-sonnet-4-6"));
        // A RETIRED version is still priced exactly, by its own row: an old
        // transcript really does carry it, at three times a current Opus.
        assert_eq!(listed("claude-opus-4-20250514"), [15.0, 18.75, 30.0, 1.50, 75.0]);
        assert_eq!(listed("claude-opus-4-1-20250805"), [15.0, 18.75, 30.0, 1.50, 75.0]);
        assert_eq!(listed("claude-3-5-haiku-20241022"), [0.80, 1.0, 1.60, 0.08, 4.0]);
    }

    #[test]
    fn an_unknown_version_takes_its_familys_highest_current_price() {
        let ceiling = |model: &str| {
            let q = price_quote(model).unwrap_or_else(|| panic!("{model} is unpriced"));
            assert_eq!(q.basis, PriceBasis::FamilyCeiling, "{model} is not a listed version");
            assert_eq!(q.long_prompt, None, "a ceiling carries no tier: {model}");
            cols(q.price)
        };
        // Sonnet's current rows run $2 to $3: the unknown version takes $3.
        assert_eq!(ceiling("claude-sonnet-9"), [3.0, 3.75, 6.0, 0.30, 15.0]);
        // Opus: the highest CURRENT row, never the retired $15 one — that
        // would triple the figure for every Opus newer than this table.
        assert_eq!(ceiling("claude-opus-9"), [5.0, 6.25, 10.0, 0.50, 25.0]);
        // Per column: Fable 5 and 5.1 share an input price and differ on a
        // cache hit, and the ceiling takes the dearer hit.
        assert_eq!(ceiling("claude-fable-9"), [10.0, 12.50, 20.0, 1.0, 50.0]);
        // Haiku: 4.5 is above both of 5.5's tiers.
        assert_eq!(ceiling("claude-haiku-9"), [1.0, 1.25, 2.0, 0.10, 5.0]);
        // An id with a family and no version at all.
        assert_eq!(ceiling("claude-mythos-preview"), [10.0, 12.50, 20.0, 1.0, 50.0]);
        assert_eq!(ceiling("opus"), [5.0, 6.25, 10.0, 0.50, 25.0]);
        // An unknown FAMILY is not priced at all, whatever it is.
        for unpriced in ["gpt-5.1-codex-max", "gpt-4o", "gemini-3-pro", "some-future-model-9", ""] {
            assert_eq!(price_quote(unpriced), None, "{unpriced:?} must stay tokens-only");
            assert!(price_for(unpriced).is_none());
        }
    }

    #[test]
    fn haiku_5_5_switches_tier_one_token_past_the_threshold() {
        let q = price_quote("claude-haiku-5-5").unwrap();
        assert_eq!(q.long_prompt.map(|l| l.over_tokens), Some(100_000));
        assert_eq!(cols(q.at(100_000)), [0.10, 0.125, 0.20, 0.01, 0.50], "\"up to\" is inclusive");
        assert_eq!(cols(q.at(100_001)), [0.50, 0.625, 1.0, 0.05, 2.50], "\"over\" starts one past");
        // A model with no tier answers its one price at any length.
        let opus = price_quote("claude-opus-5-5").unwrap();
        assert_eq!(opus.long_prompt, None);
        assert_eq!(opus.at(900_000), opus.price);
    }

    #[test]
    fn every_price_row_obeys_the_multipliers_the_vendor_states() {
        // The page states three multipliers on the base input price. Each row
        // above is five transcribed numbers, so this is what catches a typo in
        // one of the three derived columns.
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        let mut checked = 0;
        for r in PRICE_ROWS {
            let hit = match (r.family, r.version) {
                (Family::Fable | Family::Mythos, (5, 1)) => 0.025,
                (Family::Opus | Family::Sonnet, (5, 5)) => 0.05,
                _ => 0.1,
            };
            let tiers = std::iter::once(r.price).chain(r.long_prompt.map(|(_, p)| p));
            for p in tiers {
                let at = format!("{:?} {:?}", r.family, r.version);
                assert!(near(p.cache_write, p.input * 1.25), "5m write is 1.25x input: {at}");
                assert!(near(p.cache_write_1h, p.input * 2.0), "1h write is 2x input: {at}");
                assert!(near(p.cache_read, p.input * hit), "a hit is {hit}x input: {at}");
                assert!(p.output > p.input, "output is dearer than input: {at}");
                checked += 1;
            }
        }
        // The page's table has 21 rows; one model's two rows are one entry here.
        assert_eq!(checked, 21, "every row of the vendor table, and no more");
        // No two entries answer for the same (family, version).
        for (i, a) in PRICE_ROWS.iter().enumerate() {
            for b in &PRICE_ROWS[i + 1..] {
                assert!(a.family != b.family || a.version != b.version, "{:?} {:?} twice", a.family, a.version);
            }
        }
    }

    #[test]
    fn the_newer_tokenizer_starts_at_4_7() {
        let cpt = |model: &str| price_quote(model).unwrap().chars_per_token;
        let (earlier, newer) = (3.5, 3.5 / 1.3);
        assert_eq!(cpt("claude-sonnet-4-6"), earlier);
        assert_eq!(cpt("claude-opus-4-6"), earlier);
        assert_eq!(cpt("claude-haiku-4-5"), earlier);
        assert_eq!(cpt("claude-opus-4-7"), newer, "the first version on the newer tokenizer");
        assert_eq!(cpt("claude-opus-5-5"), newer);
        assert_eq!(cpt("claude-haiku-5-5"), newer);
        assert_eq!(cpt("claude-fable-5"), newer);
        // An unlisted version errs toward MORE tokens, like its price.
        assert_eq!(cpt("claude-sonnet-9"), newer);
        assert!(newer < earlier, "fewer characters per token is more tokens");
    }

    // ---------- per-request pricing, first turn, detected TTL (#3831) ----------

    /// An assistant line whose cache write is split into the two buckets the
    /// API reports: `w5` tokens to the 5-minute cache, `w60` to the 1-hour one.
    fn bucket_line(id: &str, model: &str, input: u64, w5: u64, w60: u64, cr: u64) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "id": id,
                "model": model,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": w5 + w60,
                    "cache_read_input_tokens": cr,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": w5,
                        "ephemeral_1h_input_tokens": w60,
                    },
                }
            }
        })
        .to_string()
    }

    fn cost(lines: &[String]) -> f64 {
        parse_claude_transcript(&lines.join("\n")).cost_usd.expect("priced")
    }

    #[test]
    fn a_cache_write_is_priced_by_the_cache_it_went_to() {
        let m = 1_000_000;
        // Opus 5.5: $5 a million to the 5-minute cache, $8 to the 1-hour one.
        assert!((cost(&[bucket_line("a", "claude-opus-5-5", 0, m, 0, 0)]) - 5.0).abs() < 1e-9);
        assert!((cost(&[bucket_line("a", "claude-opus-5-5", 0, 0, m, 0)]) - 8.0).abs() < 1e-9);
        assert!((cost(&[bucket_line("a", "claude-opus-5-5", 0, m, m, 0)]) - 13.0).abs() < 1e-9);
        // A record with no bucket object is priced as it always was: the
        // whole write at the 5-minute rate.
        assert!((cost(&[line("a", "claude-opus-5-5", 0, 0, m, 0)]) - 5.0).abs() < 1e-9);
        // Buckets that disagree with the total move tokens between the two
        // rates and never invent any: 1M written, 3M claimed for the hour.
        let lying = serde_json::json!({"type":"assistant","message":{"id":"a","model":"claude-opus-5-5",
            "usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":m,
                     "cache_read_input_tokens":0,
                     "cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":3 * m}}}})
        .to_string();
        assert!((cost(&[lying]) - 8.0).abs() < 1e-9);
    }

    #[test]
    fn haiku_5_5_is_priced_per_request_by_that_requests_own_prompt_length() {
        // Two requests, each priced on its own: 100,000 tokens of prompt pays
        // the lower price, 100,001 the higher. The prompt length is ALL the
        // input side, so the second one crosses on a cache read.
        let at = bucket_line("a", "claude-haiku-5-5", 100_000, 0, 0, 0);
        let over = bucket_line("b", "claude-haiku-5-5", 1, 0, 0, 100_000);
        let want_at = 100_000.0 * 0.10 / 1e6;
        let want_over = (1.0 * 0.50 + 100_000.0 * 0.05) / 1e6;
        assert!((cost(&[at.clone()]) - want_at).abs() < 1e-12);
        assert!((cost(&[over.clone()]) - want_over).abs() < 1e-12);
        assert!((cost(&[at, over]) - (want_at + want_over)).abs() < 1e-12, "earlier requests keep their price");
        // The lower tier would have charged the same read a fifth as much.
        assert!(want_over > 4.0 * (1.0 * 0.10 + 100_000.0 * 0.01) / 1e6);
    }

    #[test]
    fn first_context_tokens_is_the_first_counted_turn_and_survives_a_resume_re_emit() {
        assert_eq!(parse_claude_transcript("").first_context_tokens, None);
        let text = [
            // A synthetic line is not a turn anyone was sent.
            line("synth", "<synthetic>", 999_999, 1, 0, 0),
            // The first real turn: 10 fresh + 20 written + 30 read.
            line("m1", "claude-opus-5-5", 10, 500, 20, 30),
            line("m2", "claude-opus-5-5", 1, 500, 2, 80_000),
            // `--resume` re-emits m1. Different numbers on purpose: if the fold
            // read this line at all, the answer below would move.
            line("m1", "claude-opus-5-5", 7_000, 500, 7_000, 7_000),
        ]
        .join("\n");
        let u = parse_claude_transcript(&text);
        assert_eq!(u.first_context_tokens, Some(60), "input + cache-written + cache-read, output excluded");
        // Control: the newest turn's context is a different, much larger figure.
        assert_eq!(latest_context_tokens(&text), Some(21_000));
    }

    // The detected TTL's three rules, one test each so that each can fail on
    // its own (`docs/design/cache-age.md`, "Detecting the TTL").

    fn detected(lines: &[String]) -> Option<u32> {
        parse_claude_transcript(&lines.join("\n")).detected_cache_ttl_minutes
    }
    /// A request that wrote only to the 5-minute cache.
    fn w5(id: &str) -> String {
        bucket_line(id, "claude-opus-5-5", 1, 100, 0, 0)
    }
    /// A request that wrote only to the 1-hour cache.
    fn w60(id: &str) -> String {
        bucket_line(id, "claude-opus-5-5", 1, 0, 100, 0)
    }
    /// A request that wrote to BOTH.
    fn w_both(id: &str) -> String {
        bucket_line(id, "claude-opus-5-5", 1, 100, 100, 0)
    }
    /// A request that read the cache and wrote nothing to it.
    fn read_only(id: &str) -> String {
        bucket_line(id, "claude-opus-5-5", 1, 0, 0, 900_000)
    }

    #[test]
    fn the_last_cache_writing_request_decides_the_detected_ttl() {
        // Nothing written, nothing detected; each bucket names its own lifetime.
        assert_eq!(detected(&[]), None);
        assert_eq!(detected(&[w5("a")]), Some(5));
        assert_eq!(detected(&[w60("a")]), Some(60));
        // The LAST write decides, in both directions — so neither "first write
        // wins" nor "the longer one sticks" passes.
        assert_eq!(detected(&[w60("a"), w5("b")]), Some(5));
        assert_eq!(detected(&[w5("a"), w60("b")]), Some(60));
        assert_eq!(detected(&[w5("a"), w60("b"), w5("c"), w60("d")]), Some(60));
        // A re-emitted line is deduped before it can rewind the reading.
        assert_eq!(detected(&[w5("a"), w60("b"), w5("a")]), Some(60));
    }

    #[test]
    fn within_one_request_the_shorter_cache_bucket_wins() {
        // A request that wrote to both caches is on the SHORTER one: its
        // conversational tail is what expires first.
        assert_eq!(detected(&[w_both("a")]), Some(5));
        // ...against a history of hour-long writes too.
        assert_eq!(detected(&[w60("a"), w60("b"), w_both("c")]), Some(5));
        // It is only that request's answer: a later hour-only write replaces
        // it like any other, so this rule and the last-write rule compose.
        assert_eq!(detected(&[w_both("a"), w60("b")]), Some(60));
        // Control: an hour-only request is the hour, so the 5 above came from
        // the mixed request's short bucket and not from a default.
        assert_eq!(detected(&[w60("a")]), Some(60));
    }

    #[test]
    fn a_read_only_request_leaves_the_detected_ttl_alone() {
        // A cache hit refreshes an entry for the lifetime it was written with,
        // so it is no evidence of a change — on either lifetime.
        assert_eq!(detected(&[w60("a"), read_only("b"), read_only("c")]), Some(60));
        assert_eq!(detected(&[w5("a"), read_only("b")]), Some(5));
        // And it detects nothing on its own.
        assert_eq!(detected(&[read_only("a")]), None);
        // Neither does a write whose record names no bucket: it leaves the
        // reading where it was rather than guessing one.
        let unbucketed = |id: &str| line(id, "claude-opus-5-5", 1, 0, 100, 0);
        assert_eq!(detected(&[unbucketed("a")]), None);
        assert_eq!(detected(&[w60("a"), unbucketed("b")]), Some(60));
        // Control: a WRITING request after the reads does move it.
        assert_eq!(detected(&[w60("a"), read_only("b"), w5("c")]), Some(5));
    }
}
