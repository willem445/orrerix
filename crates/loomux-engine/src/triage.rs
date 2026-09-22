//! Delivery triage — the RULE tier (#3304 S1).
//!
//! WHAT THIS IS FOR. An orchestrator pane wakes once per delivery, and a wake
//! costs ~1.3 M cache-read tokens (#3304's census, measured over 368 wakes in
//! fifteen days). 64 % of those wakes close by a SHAPE rule with no judgement
//! in them at all: a review drive reporting `GATE SATISFIED`, a `notify_when`
//! run that came back green, a planner that posted and exited. This module is
//! the pure half of the gate that stops those reaching the pane — it reads a
//! delivery's LEADING SHAPE and answers deliver / defer.
//!
//! WHAT IT IS NOT. There is no model here and no network. The slice's config
//! accepts exactly one provider (`none`); #3304 S3 is where a provider adapter
//! would go, behind the seam this module's `Decision` already names. A rule
//! cannot be talked out of its answer by the text it is routing, which matters
//! because every string it reads is agent-authored.
//!
//! THE VOCABULARY IS INHERITED, AND IT IS NOT IDENTICAL.
//! `docs/design/orchestration-evals.md` §4.1 and `scripts/orch-scorecard.cjs`
//! already classify an orchestrator-bound prompt by its leading shape, and the
//! census that motivated this module was taken with that classifier.
//! [`classify`] reuses its shapes, its first-match-wins discipline and its
//! class names where they exist — the engine has no `regex` dependency, so the
//! patterns are hand-written prefix tests rather than regexes.
//!
//! **It is NOT the same table, and the four differences are named here rather
//! than left for a reader to find** (review round 1, B4). The scorecard answers
//! "what did this cost", over a log; this answers "must this wake the pane", on
//! the delivery path. The classes diverge where those questions do:
//!
//! 1. **No `verdict-notice`.** The scorecard's fifth wake class — whose
//!    tie-break §4.1 calls load-bearing, a verdict echo being a verdict notice
//!    rather than a system notice — folds into [`Kind::SystemNotice`] here.
//!    Nothing is lost, because no rule fires on either and both deliver. A rule
//!    written for one would have to bring the class back first.
//! 2. **`message-from` is split out of `delegate-blocked`.** The scorecard
//!    pools them — a "message from" arm sits inside its `delegate-blocked`
//!    regex — and here they are separate classes. The split is LOAD-BEARING:
//!    the plan-chunk rule exists only because `message-from` can be reasoned
//!    about apart from a blocked report, which is never triaged at all.
//! 3. **Three classes the scorecard has no row for**: [`Kind::PrChecks`],
//!    [`Kind::PlannerExited`] and [`Kind::AgentExited`]. The census counted
//!    those by hand (#3304 Q1), and a rule needs a class to hang on.
//! 4. **`run-completed` is matched the scorecard's way** — `run <digits>:
//!    completed`. An earlier draft accepted any token after `run `, which was
//!    looser than the table it claims to reuse.
//!
//! A second, divergent classifier is what this module exists not to be. A
//! documented, tested divergence is a different thing from an undisclosed one.
//!
//! FAIL-SAFE IS DELIVER. Every shape this module does not positively recognise
//! as closing by rule is delivered, so a new notice kind, a reworded one, and
//! a parse that falls through all read as "wake the orchestrator". The cost of
//! a spared wake is a wake; the cost of a swallowed decision is a stalled
//! group, so the two errors are not symmetric and the default is the cheap one.
//!
//! NOTHING IS EVER DROPPED. A deferred delivery is written to
//! `<group-dir>/deferred.json` and flushed as ONE framed delivery at the next
//! genuine wake, or at `max_defer_minutes` with no wake at all, or when the
//! store hits [`MAX_DEFERRED`]. Three bounds rather than one, because #496 /
//! #513's lesson is that a suppression waiting on a fallible signal must have
//! a clock under it: "the next genuine wake" is a signal that may never come.
//!
//! See `docs/design/delivery-triage.md`.

use serde::{Deserialize, Serialize};

/// The file, under the group directory, that holds what triage has deferred.
pub const DEFERRED_FILE: &str = "deferred.json";

/// The only `triage.provider` value this slice accepts. #3304 S3 adds the
/// second; until then a file naming one is REFUSED rather than silently run
/// with no provider, because a repo author who wrote `provider: typesafe`
/// believes text is being classified by a model and it is not.
pub const PROVIDER_NONE: &str = "none";

/// `triage.max_defer_minutes` — the clock under the deferral.
///
/// Its own closed range rather than the notify-TTL clamp family's, and
/// REFUSED outside it rather than clamped, on `merge_queue.max_batch`'s
/// argument: a repo asking for a four-hour ceiling has said something about
/// how long it is willing to lose sight of its own fleet, and quietly handing
/// it thirty minutes would leave the misunderstanding in place while the
/// behaviour changed underneath it.
pub const TRIAGE_MAX_DEFER_MINUTES_DEFAULT: u32 = 30;
pub const TRIAGE_MAX_DEFER_MINUTES_MIN: u32 = 1;
pub const TRIAGE_MAX_DEFER_MINUTES_MAX: u32 = 240;

/// The third bound on a deferral, beside "the next genuine wake" and
/// `max_defer_minutes`: how many notices may accumulate before the store
/// flushes regardless of either.
///
/// A CI storm can produce a hundred green-run notices inside one
/// `max_defer_minutes` window, and a framed delivery listing all of them is
/// no longer a summary — it is the wake it was meant to replace, plus a wall
/// of text. Flushing at the cap keeps every framed notice readable and keeps
/// the file bounded; nothing is dropped, the flush simply comes early.
pub const MAX_DEFERRED: usize = 40;

/// How much of one deferred notice the framed flush quotes.
///
/// The flush is a SUMMARY: its job is to tell the orchestrator what it slept
/// through in one screen, not to re-deliver N payloads it already decided not
/// to wake for. The full text of every deferred row is in `deferred.json` and
/// in the `delivery-triaged` audit rows, and `list_deferred` reads them back.
const QUOTE_BYTES: usize = 160;

// ---------------------------------------------------------------------------
// Kinds — §4.1's leading-shape table
// ---------------------------------------------------------------------------

/// The prefix every notice orrerix itself writes carries. A text without it
/// was typed by a human into the pane (the scorecard's `other` class), which
/// is the first member of the never-triaged set.
const PREFIX: &str = "[orrerix] ";

/// A delivery's class, decided by the LEADING SHAPE of its text — first match
/// wins, in the order [`classify`] tests them.
///
/// The variants START from `orch-scorecard.cjs`'s two tables (`WAKE_SHAPES`
/// plus `ORCH_PROMPT_SHAPES`), because the census this module implements used
/// both: the scorecard splits "what woke it" from "what the drive cost it",
/// and triage needs one table that names every row of #3304's Q1. It is **not
/// their union** — the module header enumerates the four places this table and
/// the scorecard's differ, and that list is the contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `X reports progress …`
    DelegateProgress,
    /// `X reports done …`
    DelegateDone,
    /// `X reports approved …` / `… request_changes …`
    ReviewerReport,
    /// `X reports blocked …`
    DelegateBlocked,
    /// `review drive PR #N: GATE SATISFIED …`
    DriveGateSatisfied,
    /// `review drive PR #N: HELD …`
    DriveHeld,
    /// `review drive PR #N: CANCELLED …`
    DriveCancelled,
    /// `run N: completed — conclusion: X …` (a `notify_when` `workflow_run`)
    RunCompleted,
    /// `PR #N checks: SUCCESS …` (a `notify_when` `pr_checks`)
    PrChecks,
    /// `message from X: …`
    MessageFrom,
    /// `planner X (id) posted its plan and exited …`
    PlannerExited,
    /// `agent X (id) exited (code …) …`
    AgentExited,
    /// `watchdog: agent X … 10+ min …`
    Watchdog,
    /// Any other `[orrerix]` line — a question answer, a needs-you close-out,
    /// a lock notice, a compact nudge, a relayed human line. Never a rule.
    SystemNotice,
    /// No `[orrerix]` prefix: a human typed it.
    Human,
}

impl Kind {
    /// The wire spelling — what the `delivery-triaged` audit row's `kind`
    /// carries, what `triage.kinds:` names, and what `list_deferred` returns.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::DelegateProgress => "delegate-progress",
            Kind::DelegateDone => "delegate-done",
            Kind::ReviewerReport => "reviewer-report",
            Kind::DelegateBlocked => "delegate-blocked",
            Kind::DriveGateSatisfied => "drive-gate-satisfied",
            Kind::DriveHeld => "drive-held",
            Kind::DriveCancelled => "drive-cancelled",
            Kind::RunCompleted => "run-completed",
            Kind::PrChecks => "pr-checks",
            Kind::MessageFrom => "message-from",
            Kind::PlannerExited => "planner-exited",
            Kind::AgentExited => "agent-exited",
            Kind::Watchdog => "watchdog",
            Kind::SystemNotice => "system-notice",
            Kind::Human => "human",
        }
    }

    /// Every class, so a `kinds:` value can be validated against the set and
    /// the docs table can be generated from the same place the parser reads.
    ///
    /// Hand-listed, so [`Kind::all_index`] below is the tripwire that makes a
    /// sixteenth variant a compile error rather than a silently short list —
    /// the idiom `Role::ALL` and `Delivery::ALL` already use.
    pub const ALL: [Kind; 15] = [
        Kind::DelegateProgress,
        Kind::DelegateDone,
        Kind::ReviewerReport,
        Kind::DelegateBlocked,
        Kind::DriveGateSatisfied,
        Kind::DriveHeld,
        Kind::DriveCancelled,
        Kind::RunCompleted,
        Kind::PrChecks,
        Kind::MessageFrom,
        Kind::PlannerExited,
        Kind::AgentExited,
        Kind::Watchdog,
        Kind::SystemNotice,
        Kind::Human,
    ];

    /// Non-exhaustive-match tripwire for [`Kind::ALL`] — see its doc.
    #[allow(dead_code)]
    fn all_index(self) -> usize {
        match self {
            Kind::DelegateProgress => 0,
            Kind::DelegateDone => 1,
            Kind::ReviewerReport => 2,
            Kind::DelegateBlocked => 3,
            Kind::DriveGateSatisfied => 4,
            Kind::DriveHeld => 5,
            Kind::DriveCancelled => 6,
            Kind::RunCompleted => 7,
            Kind::PrChecks => 8,
            Kind::MessageFrom => 9,
            Kind::PlannerExited => 10,
            Kind::AgentExited => 11,
            Kind::Watchdog => 12,
            Kind::SystemNotice => 13,
            Kind::Human => 14,
        }
    }

    /// Parse a wire spelling back, for `triage.kinds:`.
    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// `text` after the `[orrerix] ` prefix, or `None` when it has none.
fn body(text: &str) -> Option<&str> {
    text.strip_prefix(PREFIX)
}

/// Is `rest` the tail of `<token> <word>`, where `<token>` is one non-space
/// run? The scorecard's `\S+ reports done` in std.
fn after_agent_token<'a>(rest: &'a str, verb: &str) -> Option<&'a str> {
    let (tok, tail) = rest.split_once(' ')?;
    if tok.is_empty() || tok.contains(char::is_whitespace) {
        return None;
    }
    tail.strip_prefix(verb)
}

/// `review drive PR #N: ` → `(N, tail)`.
fn drive_notice(rest: &str) -> Option<(u64, &str)> {
    let tail = rest.strip_prefix("review drive PR #")?;
    let (num, tail) = tail.split_once(would_end_number)?;
    let pr: u64 = num.parse().ok()?;
    Some((pr, tail))
}

fn would_end_number(c: char) -> bool {
    c == ':'
}

/// The class of one orchestrator-bound delivery (#3304 Q1).
///
/// First match wins, and the ORDER is the tie-break, exactly as
/// `orch-scorecard.cjs` documents for its own tables: `reports approved` is a
/// reviewer report rather than a generic delegate report, and a drive notice
/// is its own state rather than a `system-notice`.
pub fn classify(text: &str) -> Kind {
    let Some(rest) = body(text) else { return Kind::Human };
    if let Some((_, tail)) = drive_notice(rest) {
        // The `:` was consumed as the delimiter; the state follows one space.
        let tail = tail.trim_start();
        if tail.starts_with("GATE SATISFIED") {
            return Kind::DriveGateSatisfied;
        }
        if tail.starts_with("HELD") {
            return Kind::DriveHeld;
        }
        if tail.starts_with("CANCELLED") {
            return Kind::DriveCancelled;
        }
        return Kind::SystemNotice;
    }
    if after_agent_token(rest, "reports progress").is_some() {
        return Kind::DelegateProgress;
    }
    if after_agent_token(rest, "reports done").is_some() {
        return Kind::DelegateDone;
    }
    if after_agent_token(rest, "reports approved").is_some()
        || after_agent_token(rest, "reports request_changes").is_some()
    {
        return Kind::ReviewerReport;
    }
    if after_agent_token(rest, "reports blocked").is_some() {
        return Kind::DelegateBlocked;
    }
    if rest.starts_with("message from ") {
        return Kind::MessageFrom;
    }
    if rest.starts_with("watchdog: ") {
        return Kind::Watchdog;
    }
    // `run <digits>: completed` — the scorecard's own shape, not a looser
    // "any token after `run `" (review round 1, B4(4)).
    if run_completed(rest) {
        return Kind::RunCompleted;
    }
    if rest.starts_with("PR #") && rest.contains(" checks: ") {
        return Kind::PrChecks;
    }
    if rest.starts_with("planner ") && rest.contains("posted its plan and exited") {
        return Kind::PlannerExited;
    }
    if rest.starts_with("agent ") && rest.contains(" exited (code ") {
        return Kind::AgentExited;
    }
    Kind::SystemNotice
}

// ---------------------------------------------------------------------------
// The never-triaged set
// ---------------------------------------------------------------------------

/// Why a delivery is exempt from triage before any rule is consulted (#3304
/// Q2's bottom row, the 96 deliveries that are "the decision class by
/// construction").
///
/// The rule table is default-deliver, so most of this set would be delivered
/// anyway. It is stated explicitly regardless, for two reasons: the audit row
/// then says WHY a delivery was not triaged rather than leaving it to be read
/// off an absence, and S3's provider tier is gated on the same predicate —
/// so "a human's words are never sent to a classifier" is a property of this
/// function rather than of the rule table happening not to match them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeverReason {
    /// The text carries no `[orrerix]` prefix, or the sender is the human.
    HumanActor,
    /// A post-compact re-grounding notice, or an orchestration-restored one:
    /// the pane has just lost its context and this is what rebuilds it.
    Regrounding,
    /// `review drive PR #N: HELD` — the reason and the remedy ARE the
    /// decision (#3118 already deduped these).
    DriveHeld,
    /// `X reports blocked` — one in the whole census, and every one of them
    /// is a delegate that has stopped.
    DelegateBlocked,
    /// A watchdog stall notice: inspect-or-resend is a decision.
    Watchdog,
    /// The text names the orchestrator as the one who must act — a question,
    /// a needs-you close-out, or a report that says so in words.
    NeedsYou,
}

impl NeverReason {
    pub fn as_str(self) -> &'static str {
        match self {
            NeverReason::HumanActor => "human-actor",
            NeverReason::Regrounding => "regrounding",
            NeverReason::DriveHeld => "held",
            NeverReason::DelegateBlocked => "blocked",
            NeverReason::Watchdog => "watchdog",
            NeverReason::NeedsYou => "needs-you",
        }
    }
}

/// Markers that make a text a decision whatever its shape says (#3304 Q1's
/// "a report that NAMES the orchestrator" column).
///
/// Matched case-insensitively on the whole text, not on its leading shape,
/// because that is what they are: an escape hatch a delegate can reach for
/// from inside any notice. They can only ever move a delivery toward
/// DELIVER, so an agent that writes one gets a wake it might not have needed
/// — never a suppression it did not ask for. That asymmetry is why a marker
/// list is safe here and a rule keyed on agent-authored prose would not be.
///
/// **`is yours` is the one marker orrerix's OWN text triggers** (#3324). Every
/// `GATE SATISFIED` notice ends `Disposition is yours (INVARIANT 3):
/// list_verdicts("N")` — an orchestrator action named in the notice — and the
/// S2 replay (#3304) measured 32 of 40 false defers on exactly that line. It
/// replaces the narrower `decision is yours`, which it strictly subsumes;
/// keeping both would leave a dead entry. Because a marker can only DELIVER,
/// widening one cannot create a suppression — the cost is bounded above by a
/// wake that was not needed.
const NEEDS_YOU_MARKERS: [&str; 6] = [
    "blocking on you",
    "is yours",
    "needs you",
    "needs your",
    "you must rule",
    "your call",
];

/// Markers for the re-grounding / restored class, which arrives as a
/// `system-notice` and must never be held back: a pane that has just lost its
/// context is the one pane a delayed notice hurts most.
const REGROUNDING_MARKERS: [&str; 3] =
    ["context was compacted", "orchestration restored", "re-grounding"];

/// Markers that say the `Note (registered)` on a watch notice names a
/// GREEN-path action (#3324).
///
/// `run-green` and `checks-green` were written on the reading that a green
/// verdict is news to nobody; the S2 replay (#3304) found 7 of 40 false defers
/// where it was news, because the registrant had written down what to do WHEN
/// it went green — `if green: tag v1.3.0 and push`, `Green -> spawn
/// process-pro`. The rule is subordinate to the note, exactly as it is to
/// [`NEEDS_YOU_MARKERS`], and for the same reason: the match can only move the
/// delivery toward DELIVER, so a note that mentions green buys at worst a wake
/// that was not needed. A red-only note (`red-main rule`, `Red -> INVARIANT
/// 6`) matches nothing here and still defers.
///
/// Matched against the NOTE alone, not the whole notice — see
/// [`registered_note`] for why the scope is load-bearing.
const GREEN_PATH_MARKERS: [&str; 5] =
    ["if green", "on success", "once green", "green ->", "green →"];

/// Markers on an `agent ... exited` notice that say the pane produced nothing
/// (#3324).
///
/// `agent-exited` defers on the reading that the roster already reflects the
/// exit, so the notice is an FYI. A pane that exited before printing anything
/// is not a roster update — it is a LOST kickoff, and the orchestrator has to
/// decide whether to respawn. This text is built by orrerix itself
/// (`src-tauri/src/orchestration/mod.rs`), not by an agent, so the match is
/// not reachable by anything a delegate writes.
const SILENT_EXIT_MARKERS: [&str; 1] = ["produced no output before exiting"];

fn contains_ci(hay_lower: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay_lower.contains(n))
}

/// Is this delivery exempt from triage entirely? `None` means the rule table
/// decides.
pub fn never_triaged(text: &str, human_actor: bool) -> Option<NeverReason> {
    if human_actor || body(text).is_none() {
        return Some(NeverReason::HumanActor);
    }
    let lower = text.to_ascii_lowercase();
    if contains_ci(&lower, &REGROUNDING_MARKERS) {
        return Some(NeverReason::Regrounding);
    }
    match classify(text) {
        Kind::DriveHeld => return Some(NeverReason::DriveHeld),
        Kind::DelegateBlocked => return Some(NeverReason::DelegateBlocked),
        Kind::Watchdog => return Some(NeverReason::Watchdog),
        _ => {}
    }
    if contains_ci(&lower, &NEEDS_YOU_MARKERS) {
        return Some(NeverReason::NeedsYou);
    }
    None
}

// ---------------------------------------------------------------------------
// The rule table
// ---------------------------------------------------------------------------

/// A rule that closed a delivery without waking the pane. The wire spelling
/// is what the audit row's `action` carries as `rule:<name>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
///
/// **There is no `GateSatisfied` (#3324).** The rule shipped on the reading
/// that the merge queue's own gate re-check was the decision; it is not. A
/// `GATE SATISFIED` notice ends `Disposition is yours (INVARIANT 3)` because
/// under that invariant the disposition IS an orchestrator decision, so the
/// notice can never be an FYI and the rule was wrong on its own terms. It is
/// retired rather than reworded: the marker in [`NEEDS_YOU_MARKERS`] already
/// delivers every one of these, and a rule that can only ever be shadowed is
/// a rule with no behaviour to test.
pub enum Rule {
    /// A `notify_when` `workflow_run` that came back `conclusion: success`
    /// AND whose registered note names no green-path action (#3324).
    RunGreen,
    /// A `notify_when` `pr_checks` that came back `SUCCESS`, same proviso.
    ChecksGreen,
    /// A planner that posted its plan and exited — the plan drive (#3040)
    /// consumes this; the notice is a slot-free FYI.
    PlannerExited,
    /// A pane that exited having printed SOMETHING; the roster already
    /// reflects it. One that printed nothing is a lost kickoff and is
    /// delivered (#3324).
    AgentExited,
    /// `review drive PR #N: CANCELLED` — the drive ended because something
    /// already decided it should.
    DriveCancelled,
    /// One `---BEGIN PLAN k/n---` chunk that is not the last: held so the
    /// whole plan arrives as one wake instead of n.
    PlanChunk,
}

impl Rule {
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::RunGreen => "run-green",
            Rule::ChecksGreen => "checks-green",
            Rule::PlannerExited => "planner-exited",
            Rule::AgentExited => "agent-exited",
            Rule::DriveCancelled => "drive-cancelled",
            Rule::PlanChunk => "plan-chunk",
        }
    }
}

/// Why a delivery is being delivered rather than deferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliverReason {
    /// `triage.enabled: false`, or no `triage:` block at all.
    Disabled,
    /// The never-triaged set claimed it.
    Never(NeverReason),
    /// Its kind is not on `triage.kinds:`.
    KindNotTriaged,
    /// No rule matched — the fail-safe default, and the commonest answer.
    NoRule,
}

impl DeliverReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DeliverReason::Disabled => "disabled",
            DeliverReason::Never(r) => r.as_str(),
            DeliverReason::KindNotTriaged => "kind-not-triaged",
            DeliverReason::NoRule => "no-rule",
        }
    }
}

/// What the rule tier decided about one delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Wake the pane, for this reason. A flush of anything already deferred
    /// rides in front of it — that is what "the next genuine wake" means.
    Deliver(DeliverReason),
    /// Hold it; audit only.
    ///
    /// Two variants, not three: the `TryEnqueue` arm that let the caller
    /// resolve a satisfied gate by attempting a merge-queue enqueue went with
    /// the rule it justified (#3324). Every decision this module makes is
    /// now pure and final.
    Defer(Rule),
}

/// One delivery, as the rule tier sees it.
pub struct Input<'a> {
    pub text: &'a str,
    /// The `from` the audit row carries — an agent id, or loomux's own actor.
    pub from: &'a str,
    /// True when the sender is the human rather than the fleet.
    pub human_actor: bool,
}

/// What a repo declares about triage. Mirrors `workflow::TriagePolicy`, which
/// is where the parse and the refusals live; this is the shape the decision
/// reads, so the pure function takes no dependency on the whole workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub enabled: bool,
    /// The kinds triage may act on. Empty means every kind the rule table
    /// covers — see `workflow::TriagePolicy::kinds`.
    pub kinds: Vec<Kind>,
    pub max_defer_minutes: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            enabled: false,
            kinds: Vec::new(),
            max_defer_minutes: TRIAGE_MAX_DEFER_MINUTES_DEFAULT,
        }
    }
}

impl Policy {
    fn covers(&self, kind: Kind) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&kind)
    }
}

/// `---BEGIN PLAN k/n---` → `(k, n)`, wherever in the text it appears.
///
/// Planners split a long plan across several `message_orchestrator` calls and
/// frame each piece with this marker; the census found 17 wakes carrying four
/// plans. Nothing in orrerix emits the marker — it is a planner convention —
/// so the parse is deliberately strict: both numbers, both parsed, `k <= n`,
/// `n >= 2`. Anything else falls through to the fail-safe.
pub fn plan_chunk(text: &str) -> Option<(u32, u32)> {
    let at = text.find("---BEGIN PLAN ")?;
    let rest = &text[at + "---BEGIN PLAN ".len()..];
    let end = rest.find("---")?;
    let (k, n) = rest[..end].trim().split_once('/')?;
    let k: u32 = k.trim().parse().ok()?;
    let n: u32 = n.trim().parse().ok()?;
    if n < 2 || k == 0 || k > n {
        return None;
    }
    Some((k, n))
}

/// Does a `run N: completed` notice report a green conclusion?
///
/// **Keyed on the conclusion alone, and NOT on the branch**, which is a
/// deliberate narrowing of #3304 Q1's "success on the default branch": a
/// `watch_fired_notice` for a `workflow_run` carries the run id and the
/// conclusion and no branch at all, so a branch clause here would be a
/// condition this module cannot evaluate. It costs nothing: a run that went
/// green is news to nobody whichever branch it was on, and the census's
/// single red run — the one delivery of the 65 that mattered — is caught by
/// the conclusion, not by the branch.
fn run_is_green(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    match lower.find("conclusion: ") {
        Some(at) => lower[at + "conclusion: ".len()..].trim_start().starts_with("success"),
        None => false,
    }
}

/// The agent's `Note (registered): "..."` on a watch notice, if it carries one.
///
/// Scoped DELIBERATELY to the note. The rest of a watch notice is
/// GitHub-derived (`conclusion: success`, `checks: SUCCESS`, a head SHA) and a
/// green-path marker matched against the whole text would be reading the
/// verdict, not the registrant's intent. The note's own delimiters are
/// backend-built by `notify::watch_fired_notice` and the note is sanitized of
/// control characters before it is interpolated, so an agent cannot forge the
/// closing delimiter's position; the worst it can do is include a quote, which
/// truncates the slice this reads — in the DELIVER direction only.
fn registered_note(text: &str) -> Option<&str> {
    const OPEN: &str = "Note (registered): \"";
    let at = text.find(OPEN)?;
    let rest = &text[at + OPEN.len()..];
    let end = rest.rfind('"')?;
    Some(&rest[..end])
}

/// Does the registered note name something to do when the verdict is GREEN?
fn note_names_green_path(text: &str) -> bool {
    match registered_note(text) {
        Some(note) => contains_ci(&note.to_ascii_lowercase(), &GREEN_PATH_MARKERS),
        None => false,
    }
}

/// Did the pane exit without printing anything at all?
fn exited_silently(text: &str) -> bool {
    contains_ci(&text.to_ascii_lowercase(), &SILENT_EXIT_MARKERS)
}

/// Does a `PR #N checks:` notice report a green result?
fn checks_are_green(text: &str) -> bool {
    match text.find(" checks: ") {
        Some(at) => text[at + " checks: ".len()..].trim_start().starts_with("SUCCESS"),
        None => false,
    }
}

/// THE DECISION, in the order #3304 Q5 states it: never-triaged set, then the
/// rule table, then fail-safe deliver.
///
/// Pure and total. Every path returns a [`Decision`]; there is no error case,
/// because there is nothing here that can fail — a shape that does not parse
/// is a shape that does not match, which is a delivery.
pub fn decide(input: &Input<'_>, policy: &Policy) -> Decision {
    if !policy.enabled {
        return Decision::Deliver(DeliverReason::Disabled);
    }
    if let Some(reason) = never_triaged(input.text, input.human_actor) {
        return Decision::Deliver(DeliverReason::Never(reason));
    }
    let kind = classify(input.text);
    if !policy.covers(kind) {
        return Decision::Deliver(DeliverReason::KindNotTriaged);
    }
    match kind {
        // `Kind::DriveGateSatisfied` has no arm: it falls to the fail-safe
        // below, and never reaches even that in practice because the marker
        // claims it first (#3324).
        //
        // Both green rules are subordinate to the registered note: a note that
        // names a green-path action is the registrant saying the green verdict
        // is the trigger for their next move (#3324).
        Kind::RunCompleted if run_is_green(input.text) && !note_names_green_path(input.text) => {
            Decision::Defer(Rule::RunGreen)
        }
        Kind::PrChecks if checks_are_green(input.text) && !note_names_green_path(input.text) => {
            Decision::Defer(Rule::ChecksGreen)
        }
        Kind::PlannerExited => Decision::Defer(Rule::PlannerExited),
        // A pane that printed nothing is a lost kickoff, not a roster update.
        Kind::AgentExited if !exited_silently(input.text) => Decision::Defer(Rule::AgentExited),
        Kind::DriveCancelled => Decision::Defer(Rule::DriveCancelled),
        // A plan's LAST chunk is a genuine wake and carries its siblings out
        // of the store with it (the flush rides in front of every delivery),
        // so one plan costs one wake instead of n.
        Kind::MessageFrom => match plan_chunk(input.text) {
            Some((k, n)) if k < n => Decision::Defer(Rule::PlanChunk),
            _ => Decision::Deliver(DeliverReason::NoRule),
        },
        _ => Decision::Deliver(DeliverReason::NoRule),
    }
}

/// `run <digits>: completed` — at least one digit, and the digits are the
/// whole of the run id.
fn run_completed(rest: &str) -> bool {
    let Some(tail) = rest.strip_prefix("run ") else { return false };
    let Some((id, after)) = tail.split_once(':') else { return false };
    !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) && after.starts_with(" completed")
}

/// The PR a drive notice names.
pub fn pr_of(text: &str) -> Option<u64> {
    drive_notice(body(text)?).map(|(pr, _)| pr)
}

// ---------------------------------------------------------------------------
// The deferred store
// ---------------------------------------------------------------------------

/// One held delivery. The text is kept in full: `list_deferred` reads these
/// back, and a notice nobody can re-read is a notice that was dropped with
/// extra steps.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub ts_ms: u64,
    pub from: String,
    pub kind: String,
    pub rule: String,
    pub text: String,
}

/// `<group-dir>/deferred.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deferred {
    #[serde(default)]
    pub entries: Vec<Entry>,
}

/// Why a store is being flushed now. Carried into the framed notice so the
/// orchestrator can tell "you were about to be woken anyway" from "the clock
/// ran out on these".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushCause {
    /// A genuine wake is about to be delivered; these ride in front of it.
    Wake,
    /// `max_defer_minutes` elapsed with no wake.
    Deadline,
    /// [`MAX_DEFERRED`] entries accumulated.
    Full,
    /// Triage was turned OFF (or its file stopped parsing) while this store
    /// still held notices, so the feature hands back what it was holding.
    ///
    /// **Its own variant rather than a reuse of [`FlushCause::Deadline`]**
    /// (review round 2): the cause is rendered into the frame a human-supervised
    /// pane reads, and "flushed on the deferral deadline" is simply untrue of a
    /// release that happened because the policy went away — often, as the test
    /// pins, nowhere near the deadline. A wrong value on a user-facing surface
    /// is a defect rather than a tidiness point.
    PolicyOff,
}

impl FlushCause {
    pub fn as_str(self) -> &'static str {
        match self {
            FlushCause::Wake => "wake",
            FlushCause::Deadline => "deadline",
            FlushCause::Full => "full",
            FlushCause::PolicyOff => "policy-off",
        }
    }
}

impl Deferred {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The oldest held entry's timestamp — the clock `max_defer_minutes` runs
    /// against.
    pub fn oldest_ms(&self) -> Option<u64> {
        self.entries.first().map(|e| e.ts_ms)
    }

    pub fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
    }

    /// Is this store due a flush on its own, with no wake to ride?
    ///
    /// Two bounds, and BOTH are the #496/#513 lesson rather than one being
    /// belt-and-braces: the deadline bounds how long one notice may sit, and
    /// the cap bounds how many may pile up inside one deadline window. A
    /// store with neither would be an unbounded wait on "some other delivery
    /// arrives", which is a signal that may never come.
    pub fn due(&self, now_ms: u64, max_defer_minutes: u32) -> Option<FlushCause> {
        if self.entries.is_empty() {
            return None;
        }
        if self.entries.len() >= MAX_DEFERRED {
            return Some(FlushCause::Full);
        }
        let deadline_ms = u64::from(max_defer_minutes) * 60_000;
        match self.oldest_ms() {
            // A clock that reads backwards (a store written before a system
            // clock moved) flushes rather than latching: the fail-safe
            // direction here is the wake.
            Some(oldest) if now_ms < oldest => Some(FlushCause::Deadline),
            Some(oldest) if now_ms.saturating_sub(oldest) >= deadline_ms => {
                Some(FlushCause::Deadline)
            }
            _ => None,
        }
    }

    /// The ONE framed delivery that replaces N wakes.
    ///
    /// Returns `None` for an empty store, so a caller cannot deliver a frame
    /// around nothing.
    pub fn flush_notice(&self, now_ms: u64, cause: FlushCause) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let oldest = self.oldest_ms().unwrap_or(now_ms);
        let mins = now_ms.saturating_sub(oldest) / 60_000;
        let n = self.entries.len();
        let plural = if n == 1 { "notice" } else { "notices" };
        let tail = match cause {
            FlushCause::Wake => "flushed because something did need you",
            FlushCause::Deadline => "flushed on the deferral deadline",
            FlushCause::Full => "flushed because the deferred store filled",
            FlushCause::PolicyOff => "flushed because triage was turned off",
        };
        // The frame points at the audit log, NOT at `list_deferred()` (review
        // round 1, B1a): the store is emptied BEFORE this frame is delivered,
        // so by the time the orchestrator could call that tool it answers
        // `count: 0`. `delivery-triaged` carries each deferral's full text and
        // is permanent, so it is the record that is actually there to read.
        // `list_deferred()` answers a DIFFERENT question — what is held right
        // now — and the tool's own description is where that is said.
        let mut s = format!(
            "{PREFIX}{n} {plural} deferred over the last {mins} min ({tail}). \
             Each closed by a shape rule, none of them a decision; full text is on \
             this group's audit log as delivery-triaged:"
        );
        for e in &self.entries {
            s.push_str(&format!("\n- [{}] {} from {}: {}", e.rule, e.kind, e.from, quote(&e.text)));
        }
        Some(s)
    }
}

/// One deferred notice, cut to [`QUOTE_BYTES`] on a char boundary and with
/// its own newlines flattened, so one entry is always one line of the frame.
fn quote(text: &str) -> String {
    let one_line: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.len() <= QUOTE_BYTES {
        return one_line;
    }
    let mut cut = QUOTE_BYTES;
    while cut > 0 && !one_line.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &one_line[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shapes, transcribed from the emitters rather than invented:
    /// `rddrive::gate_satisfied_notice`, `notify::watch_fired_notice`,
    /// `OrchRegistry`'s planner-exit and agent-exit notices, and the
    /// `report`/`message_orchestrator` lines the census counted.
    const GATE: &str = "[orrerix] review drive PR #1758: GATE SATISFIED at df6a73d0 (body 4a1c) \
                        — rev-lead pass; 1 rounds, 1 CI, 0 rebases. Disposition is yours \
                        (INVARIANT 3): list_verdicts(\"1758\").";
    const HELD: &str = "[orrerix] review drive PR #1758: HELD (ci-red) — three attempts spent.";
    const CANCELLED: &str = "[orrerix] review drive PR #3: CANCELLED — the human merged it.";
    // Transcribed from `notify::watch_fired_notice`, INCLUDING the quotes it
    // writes around a registered note — an earlier draft dropped them, which
    // made the "transcribed rather than invented" claim above not byte-honest
    // for this specimen (review round 1, N2).
    // #3324 split this specimen in two. The note orrerix's agents actually
    // write frequently names what to do WHEN the run goes green, and the
    // `run-green` rule is subordinate to that — so the deferring specimen
    // needs a RED-ONLY note, and the old one is kept beside it under its own
    // name as the witness for the new arm. Both are real shapes off the live
    // log; neither is invented.
    const RUN_GREEN: &str =
        "[orrerix] run 17812: completed — conclusion: success. Note (registered): \"post-merge \
         main abc1234; red → INVARIANT 6\". (watch n-1)";
    const RUN_GREEN_ACT: &str =
        "[orrerix] run 17812: completed — conclusion: success. Note (registered): \"green → \
         next merge; red → fix forward\". (watch n-1)";
    const CHECKS_GREEN_ACT: &str =
        "[orrerix] PR #241 checks: SUCCESS — all 6 checks passed. Note (registered): \"1.3.0 \
         bump PR — if green, ask the human to merge\" (watch n-3)";
    const RUN_RED: &str = "[orrerix] run 17813: completed — conclusion: failure. (watch n-2)";
    const CHECKS_GREEN: &str =
        "[orrerix] PR #241 checks: SUCCESS — all 6 checks passed (watch n-3)";
    const CHECKS_RED: &str = "[orrerix] PR #241 checks: FAILURE — 1 of 6 failed (watch n-3)";
    const PLANNER_EXIT: &str =
        "[orrerix] planner p-1 (plan-2890) posted its plan and exited — its delegate slot is free.";
    // Transcribed from the registry's agent-exit notice, including the clause
    // an earlier draft invented a different ending for (review round 1, N2).
    const AGENT_EXIT: &str = "[orrerix] agent w-2902 (worker-adv) exited (code Some(0)) 91000ms \
                              after spawn — idle. Update your plan and state accordingly.";
    // A pane that exited before the CLI printed anything: a LOST KICKOFF, which
    // the roster does not already reflect (#3324).
    const AGENT_EXIT_SILENT: &str = "[orrerix] agent w-2902 (worker-adv) exited (code Some(1)) \
                                     91000ms after spawn — produced no output before exiting — it \
                                     likely exited before the CLI printed anything at all. Update \
                                     your plan and state accordingly.";
    // `GATE` with its closing sentence removed. Kept after the gate rule was
    // retired (#3324) as the NEGATIVE control for that retirement: it carries
    // no marker, so nothing in the never-triaged set claims it, and it must
    // STILL deliver — off the fail-safe rather than off a rule. A gate notice
    // reworded to stop naming an orchestrator action would look like this, and
    // this is the test that says it would still wake the pane.
    const GATE_NO_MARKER: &str = "[orrerix] review drive PR #1758: GATE SATISFIED at df6a73d0 \
                                  (body 4a1c) — rev-lead pass; 1 rounds, 1 CI, 0 rebases.";
    const DONE: &str = "[orrerix] w-2902 reports done: #3304 — PR #3310, CI green.";
    const WATCHDOG: &str = "[orrerix] watchdog: agent w-1 (rev-lead) has been silent 10+ min.";
    const BLOCKED: &str = "[orrerix] w-7 reports blocked: needs a human call on #42.";
    const APPROVED: &str = "[orrerix] rev-1 reports approved: PR #3310 — no findings.";

    // ---------- classification: the positive control, one specimen per rule

    #[test]
    fn every_real_shape_lands_in_its_own_class() {
        let cases: &[(&str, Kind)] = &[
            (GATE, Kind::DriveGateSatisfied),
            (HELD, Kind::DriveHeld),
            (CANCELLED, Kind::DriveCancelled),
            (RUN_GREEN, Kind::RunCompleted),
            (RUN_RED, Kind::RunCompleted),
            (CHECKS_GREEN, Kind::PrChecks),
            (PLANNER_EXIT, Kind::PlannerExited),
            (AGENT_EXIT, Kind::AgentExited),
            (DONE, Kind::DelegateDone),
            (APPROVED, Kind::ReviewerReport),
            (BLOCKED, Kind::DelegateBlocked),
            (WATCHDOG, Kind::Watchdog),
            ("[orrerix] message from w-2: heads-up, rebasing.", Kind::MessageFrom),
            ("[orrerix] w-3 reports progress: half way.", Kind::DelegateProgress),
            ("[orrerix] q-4 was answered: yes, option A.", Kind::SystemNotice),
            ("merge it when you get a chance", Kind::Human),
        ];
        // Collected and compared once, for `decide_all`'s reason: a rename or
        // a reordering of the shape table breaks several rows at once, and a
        // loop would report the first and hide the rest.
        let got: Vec<(&str, Kind)> = cases.iter().map(|(t, _)| (*t, classify(t))).collect();
        assert_eq!(got, cases.to_vec());
    }

    #[test]
    fn the_kind_table_is_a_bijection_on_its_wire_spellings() {
        // `ALL` is hand-listed (see its doc); this is what makes a duplicate
        // or a missing spelling fail rather than silently shrink `kinds:`.
        let mut seen: Vec<&str> = Kind::ALL.iter().map(|k| k.as_str()).collect();
        seen.sort_unstable();
        let n = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), n, "two kinds share a wire spelling");
        for k in Kind::ALL {
            assert_eq!(Kind::parse(k.as_str()), Some(k), "round-trip: {}", k.as_str());
        }
        assert_eq!(Kind::parse("no-such-kind"), None);
    }

    /// Decide a whole TABLE and compare the answers in one assertion.
    ///
    /// Every table-driven test below goes through this rather than asserting
    /// inside a loop, and the reason is a measured one rather than a style
    /// preference. A loop of `assert_eq!` stops at its first mismatch, so a
    /// mutation that breaks N rows reddens with evidence about ONE of them —
    /// which is how #3304 S1's own red-before-green run left five of six rules
    /// unevidenced (CLAUDE.md: "a red evidences only the assertion it REACHED
    /// and MOVED"). Comparing the collected vectors reddens ONCE, with every
    /// row that moved printed side by side.
    fn decide_table<'a>(
        cases: &'a [(&'a str, Decision)],
        policy: &Policy,
    ) -> (Vec<(&'a str, Decision)>, Vec<(&'a str, Decision)>) {
        let got = cases.iter().map(|(text, _)| (*text, decide(&input(text), policy))).collect();
        (got, cases.to_vec())
    }

    // ---------- the never-triaged set: these ALWAYS deliver

    fn on() -> Policy {
        Policy { enabled: true, ..Policy::default() }
    }

    fn input<'a>(text: &'a str) -> Input<'a> {
        Input { text, from: "w-1", human_actor: false }
    }

    #[test]
    fn the_never_triaged_set_always_delivers() {
        // One assertion over the whole set, so a mutation that breaks several
        // members reddens with all of them rather than with the first — this
        // is the safety-critical half of the decision, and "which members
        // moved" is the question a red here has to answer.
        let cases: &[(&str, Decision)] = &[
            (HELD, Decision::Deliver(DeliverReason::Never(NeverReason::DriveHeld))),
            (BLOCKED, Decision::Deliver(DeliverReason::Never(NeverReason::DelegateBlocked))),
            (WATCHDOG, Decision::Deliver(DeliverReason::Never(NeverReason::Watchdog))),
            (
                "merge it when you get a chance",
                Decision::Deliver(DeliverReason::Never(NeverReason::HumanActor)),
            ),
            (
                "[orrerix] Context was compacted — re-read your instructions.",
                Decision::Deliver(DeliverReason::Never(NeverReason::Regrounding)),
            ),
            (
                "[orrerix] w-4 reports done: PR #9 is up but BLOCKING ON YOU: A or B?",
                Decision::Deliver(DeliverReason::Never(NeverReason::NeedsYou)),
            ),
        ];
        let (got, want) = decide_table(cases, &on());
        assert_eq!(got, want, "none of these may ever be triaged");
    }

    #[test]
    fn a_human_typed_line_is_exempt_even_when_it_wears_the_prefix() {
        // A human CAN type a line beginning `[orrerix] ` into the pane, and
        // two did in the census. The actor decides, not the prefix.
        let mut i = input(RUN_GREEN);
        i.human_actor = true;
        assert_eq!(
            decide(&i, &on()),
            Decision::Deliver(DeliverReason::Never(NeverReason::HumanActor))
        );
    }

    // ---------- the rule table: one specimen per rule, and its negative

    // ONE TEST PER RULE, rather than one test asserting all six.
    //
    // The six were a single test until the red-before-green run said why that
    // is worth six `#[test]` lines: a mutation removing the whole rule table
    // reddened it on its FIRST assertion and stopped, so the red evidenced the
    // `run-green` rule and told us nothing whatever about the other five
    // (CLAUDE.md: "a red evidences only the assertion it REACHED and MOVED").
    // Split, every rule's red is its own and the mutation table has six rows
    // instead of one.

    #[test]
    fn the_run_green_rule_matches_its_own_specimen() {
        assert_eq!(decide(&input(RUN_GREEN), &on()), Decision::Defer(Rule::RunGreen));
    }

    #[test]
    fn the_checks_green_rule_matches_its_own_specimen() {
        assert_eq!(decide(&input(CHECKS_GREEN), &on()), Decision::Defer(Rule::ChecksGreen));
    }

    #[test]
    fn the_planner_exited_rule_matches_its_own_specimen() {
        assert_eq!(decide(&input(PLANNER_EXIT), &on()), Decision::Defer(Rule::PlannerExited));
    }

    #[test]
    fn the_agent_exited_rule_matches_its_own_specimen() {
        assert_eq!(decide(&input(AGENT_EXIT), &on()), Decision::Defer(Rule::AgentExited));
    }

    #[test]
    fn the_drive_cancelled_rule_matches_its_own_specimen() {
        assert_eq!(decide(&input(CANCELLED), &on()), Decision::Defer(Rule::DriveCancelled));
    }

    #[test]
    fn no_gate_satisfied_notice_is_ever_held_back_by_any_rule() {
        // The retirement (#3324), pinned from BOTH sides so that neither half
        // can come back alone.
        //
        // The line orrerix really emits is claimed by the marker, before the
        // rule table is consulted at all:
        assert_eq!(
            decide(&input(GATE), &on()),
            Decision::Deliver(DeliverReason::Never(NeverReason::NeedsYou))
        );
        // And one with the marker removed — the shape a reword would produce —
        // delivers too, off the FAIL-SAFE, because no rule matches its class
        // any more. This is the assertion the retirement would break if the
        // rule were reinstated, and the reason it is not just a duplicate of
        // the test above.
        assert_eq!(decide(&input(GATE_NO_MARKER), &on()), Decision::Deliver(DeliverReason::NoRule));
        assert_eq!(never_triaged(GATE_NO_MARKER, false), None, "nothing claims it before the table");
        assert_eq!(classify(GATE_NO_MARKER), Kind::DriveGateSatisfied, "the CLASS survives the rule");
        // That the wire vocabulary no longer carries the rule at all is pinned
        // where it can be: the mirror's vocabulary scan in
        // `test/orchtriageeval.test.ts`, which reads `Rule::as_str`'s arms out of
        // this file and asserts set-equality with the JS `RULES` table.
    }

    #[test]
    fn the_verbatim_gate_satisfied_line_names_an_orchestrator_action_and_is_delivered() {
        // #3324's core. `rddrive::gate_satisfied_notice` ends every GATE
        // SATISFIED line `Disposition is yours (INVARIANT 3): list_verdicts("N")`
        // — an orchestrator action named in orrerix's OWN text — and the S2
        // replay (#3304) measured 32 of 40 false defers on exactly it. It is a
        // needs-you DELIVER before the rule table is ever consulted.
        assert!(GATE.contains("Disposition is yours (INVARIANT 3): list_verdicts(\"1758\")."));
        assert_eq!(never_triaged(GATE, false), Some(NeverReason::NeedsYou));
        assert_eq!(
            decide(&input(GATE), &on()),
            Decision::Deliver(DeliverReason::Never(NeverReason::NeedsYou))
        );
        // And it outranks the rule table wherever the class would have led: a
        // marker is read before `covers` and before `classify`'s answer is
        // acted on, so narrowing `kinds` to this very class changes nothing.
        let narrow =
            Policy { enabled: true, kinds: vec![Kind::DriveGateSatisfied], ..Policy::default() };
        assert_eq!(
            decide(&input(GATE), &narrow),
            Decision::Deliver(DeliverReason::Never(NeverReason::NeedsYou))
        );
    }

    #[test]
    fn a_green_run_or_green_checks_whose_note_names_a_green_path_action_is_delivered() {
        // #3324's second remedy. 7 of the 40 false defers were a green verdict
        // the registrant had written down a next move for.
        let cases: [(&str, Decision); 2] = [
            (RUN_GREEN_ACT, Decision::Deliver(DeliverReason::NoRule)),
            (CHECKS_GREEN_ACT, Decision::Deliver(DeliverReason::NoRule)),
        ];
        let (got, want) = decide_table(&cases, &on());
        assert_eq!(got, want);
        // The DISCRIMINATOR, not just the two positives: the deferring
        // specimens differ from these ONLY in their note, so a rule that read
        // anything else — the conclusion, the watch id, the presence of a note
        // at all — would answer the same for both pairs and this table would
        // still pass without it.
        assert!(note_names_green_path(RUN_GREEN_ACT) && !note_names_green_path(RUN_GREEN));
        assert!(note_names_green_path(CHECKS_GREEN_ACT) && !note_names_green_path(CHECKS_GREEN));
    }

    #[test]
    fn the_green_path_scan_reads_the_note_and_not_the_verdict_around_it() {
        // The SCOPE is load-bearing: `conclusion: success` and `checks: SUCCESS`
        // are GitHub-derived, so a marker matched against the whole notice would
        // be reading the verdict rather than the registrant's intent. A notice
        // carrying no note at all must be untouched however green it reads.
        let no_note = "[orrerix] run 17812: completed — conclusion: success. (watch n-1)";
        assert_eq!(registered_note(no_note), None);
        assert!(!note_names_green_path(no_note));
        assert_eq!(decide(&input(no_note), &on()), Decision::Defer(Rule::RunGreen));
        // `on success` placed OUTSIDE the note does not reach the rule either —
        // the negative control for the scope, which a whole-text scan fails.
        let outside = "[orrerix] run 17812: completed — conclusion: success, on success nothing. \
                       Note (registered): \"red → INVARIANT 6\" (watch n-1)";
        assert_eq!(decide(&input(outside), &on()), Decision::Defer(Rule::RunGreen));
    }

    #[test]
    fn a_pane_that_printed_nothing_is_a_lost_kickoff_and_is_delivered() {
        // #3324's third remedy. The `agent-exited` rule reads "the roster
        // already reflects it"; a pane that died before printing anything is
        // not a roster update, it is a kickoff that has to be respawned.
        assert_eq!(
            decide(&input(AGENT_EXIT_SILENT), &on()),
            Decision::Deliver(DeliverReason::NoRule)
        );
        // The discriminator again: the two specimens share their class and
        // differ only in this clause.
        assert_eq!(classify(AGENT_EXIT_SILENT), classify(AGENT_EXIT));
        assert!(exited_silently(AGENT_EXIT_SILENT) && !exited_silently(AGENT_EXIT));
    }

    #[test]
    fn a_run_notice_needs_a_numeric_id_to_be_one() {
        // The scorecard requires `run <digits>: completed`, and so does this
        // (review round 1, B4(4)). A looser `starts_with("run ")` classified
        // any `run <token>: completed` line as a CI run — and since the rule
        // DEFERS a green one, a line merely shaped like that would have been
        // suppressed rather than delivered.
        let cases: [(&str, Kind); 4] = [
            (RUN_GREEN, Kind::RunCompleted),
            ("[orrerix] run 7: completed — conclusion: success.", Kind::RunCompleted),
            // Not a run id, so not the class, so no rule: DELIVERED.
            ("[orrerix] run away: completed — conclusion: success.", Kind::SystemNotice),
            ("[orrerix] run 17a: completed — conclusion: success.", Kind::SystemNotice),
        ];
        let got: Vec<(&str, Kind)> = cases.iter().map(|(t, _)| (*t, classify(t))).collect();
        assert_eq!(got, cases.to_vec());
        // The consequence that actually matters, not just the class.
        assert_eq!(
            decide(&input("[orrerix] run away: completed — conclusion: success."), &on()),
            Decision::Deliver(DeliverReason::NoRule)
        );
    }

    #[test]
    fn a_red_run_and_red_checks_are_delivered() {
        // The NEGATIVE controls for the two green rules, and the pair that
        // proves a deletion mutation is not enough: both already expect
        // `Deliver`, so only inverting `run_is_green` / `checks_are_green`
        // can move them (#1487 N2/N4).
        let cases: [(&str, Decision); 2] = [
            (RUN_RED, Decision::Deliver(DeliverReason::NoRule)),
            (CHECKS_RED, Decision::Deliver(DeliverReason::NoRule)),
        ];
        let (got, want) = decide_table(&cases, &on());
        assert_eq!(got, want);
    }

    #[test]
    fn a_delegate_done_is_never_a_rule_in_this_slice() {
        // #3304 Q2: the `done` class is where the residual JUDGEMENT lives,
        // and S1 ships no provider — so all 90 of them deliver.
        let cases: [(&str, Decision); 2] = [
            (DONE, Decision::Deliver(DeliverReason::NoRule)),
            (APPROVED, Decision::Deliver(DeliverReason::NoRule)),
        ];
        let (got, want) = decide_table(&cases, &on());
        assert_eq!(got, want);
    }

    #[test]
    fn triage_off_delivers_everything_the_rules_would_have_held() {
        let off = Policy::default();
        assert!(!off.enabled, "the product default is off");
        let cases: Vec<(&str, Decision)> =
            [RUN_GREEN, CHECKS_GREEN, PLANNER_EXIT, AGENT_EXIT, CANCELLED, GATE]
                .into_iter()
                .map(|t| (t, Decision::Deliver(DeliverReason::Disabled)))
                .collect();
        let (got, want) = decide_table(&cases, &off);
        assert_eq!(got, want, "off must change nothing");
    }

    #[test]
    fn a_kind_left_off_the_kinds_list_is_delivered() {
        let narrow = Policy { enabled: true, kinds: vec![Kind::RunCompleted], ..Policy::default() };
        let cases: [(&str, Decision); 2] = [
            (RUN_GREEN, Decision::Defer(Rule::RunGreen)),
            (PLANNER_EXIT, Decision::Deliver(DeliverReason::KindNotTriaged)),
        ];
        let (got, want) = decide_table(&cases, &narrow);
        assert_eq!(got, want);
    }

    // ---------- plan chunks

    #[test]
    fn plan_chunks_coalesce_and_the_last_one_wakes() {
        let c = |k: u32, n: u32| {
            format!("[orrerix] message from p-1: ---BEGIN PLAN {k}/{n}--- slices: …")
        };
        let (first, middle, last) = (c(1, 4), c(3, 4), c(4, 4));
        // The table is a `let` binding rather than a temporary: `decide_table`
        // borrows it and `got`/`want` outlive the call, so an inline `&[…]`
        // is dropped at the end of the statement (E0716).
        let cases: [(&str, Decision); 3] = [
            (first.as_str(), Decision::Defer(Rule::PlanChunk)),
            (middle.as_str(), Decision::Defer(Rule::PlanChunk)),
            // The last chunk is the wake that carries the others out.
            (last.as_str(), Decision::Deliver(DeliverReason::NoRule)),
        ];
        let (got, want) = decide_table(&cases, &on());
        assert_eq!(got, want);
    }

    #[test]
    fn a_malformed_plan_marker_falls_through_to_deliver() {
        let cases: Vec<(&str, Decision)> = [
            "[orrerix] message from p-1: ---BEGIN PLAN 1/1--- one chunk is not a split",
            "[orrerix] message from p-1: ---BEGIN PLAN 5/4--- past the end",
            "[orrerix] message from p-1: ---BEGIN PLAN 0/4--- zero",
            "[orrerix] message from p-1: ---BEGIN PLAN one/four--- words",
            "[orrerix] message from p-1: ---BEGIN PLAN 1/4 no closing marker",
        ]
        .into_iter()
        .map(|t| (t, Decision::Deliver(DeliverReason::NoRule)))
        .collect();
        let (got, want) = decide_table(&cases, &on());
        assert_eq!(got, want, "every malformed marker must fail safe");
    }

    #[test]
    fn an_ordinary_message_from_is_delivered() {
        // A marker moves the row into the never-triaged set, which is a
        // DIFFERENT reason for the same answer — and the reason is what the
        // audit row carries, so the two are pinned apart rather than together.
        assert_eq!(
            decide(&input("[orrerix] message from w-2: the rebase call is YOUR CALL."), &on()),
            Decision::Deliver(DeliverReason::Never(NeverReason::NeedsYou))
        );
        // A decision the markers do NOT catch still delivers, because
        // `message-from` is not a rule in this slice at all. The markers are
        // an escape hatch toward delivery, never the thing that decides it —
        // which is why missing one costs nothing.
        assert_eq!(
            decide(&input("[orrerix] message from w-2: design call before I code?"), &on()),
            Decision::Deliver(DeliverReason::NoRule)
        );
        assert_eq!(
            decide(&input("[orrerix] message from w-2: heads-up, rebasing onto main."), &on()),
            Decision::Deliver(DeliverReason::NoRule)
        );
    }

    // ---------- the store

    fn entry(ts_ms: u64) -> Entry {
        Entry {
            ts_ms,
            from: "w-1".into(),
            kind: Kind::RunCompleted.as_str().into(),
            rule: Rule::RunGreen.as_str().into(),
            text: RUN_GREEN.into(),
        }
    }

    #[test]
    fn an_empty_store_is_never_due_and_frames_nothing() {
        let d = Deferred::default();
        assert_eq!(d.due(u64::MAX, 30), None);
        assert_eq!(d.flush_notice(0, FlushCause::Wake), None);
    }

    #[test]
    fn the_deadline_bounds_a_deferral_with_no_wake() {
        let mut d = Deferred::default();
        d.push(entry(1_000_000));
        let deadline = 1_000_000 + 30 * 60_000;
        assert_eq!(d.due(deadline - 1, 30), None, "one ms early is not due");
        assert_eq!(d.due(deadline, 30), Some(FlushCause::Deadline), "the bound is inclusive");
    }

    #[test]
    fn the_cap_bounds_a_deferral_inside_one_deadline_window() {
        let mut d = Deferred::default();
        for _ in 0..MAX_DEFERRED - 1 {
            d.push(entry(1_000_000));
        }
        assert_eq!(d.due(1_000_001, 240), None);
        d.push(entry(1_000_000));
        assert_eq!(d.due(1_000_001, 240), Some(FlushCause::Full));
    }

    #[test]
    fn a_backwards_clock_flushes_rather_than_latching() {
        let mut d = Deferred::default();
        d.push(entry(2_000_000));
        assert_eq!(d.due(1_000_000, 30), Some(FlushCause::Deadline));
    }

    #[test]
    fn the_frame_names_every_held_notice_on_one_line_each() {
        let mut d = Deferred::default();
        d.push(entry(1_000_000));
        d.push(Entry {
            ts_ms: 1_060_000,
            from: "loomux".into(),
            kind: Kind::PlannerExited.as_str().into(),
            rule: Rule::PlannerExited.as_str().into(),
            text: PLANNER_EXIT.into(),
        });
        let n = d.flush_notice(1_120_000, FlushCause::Wake).expect("two entries frame");
        assert!(n.starts_with("[orrerix] 2 notices deferred over the last 2 min"), "got: {n}");
        // The frame names the record that still EXISTS when it is read. It
        // must not name `list_deferred()`, which answers `count: 0` by then
        // (review round 1, B1a) — pinned in both directions so the wording
        // cannot drift back.
        assert!(n.contains("audit log as delivery-triaged"), "got: {n}");
        assert!(!n.contains("list_deferred"), "the frame must not send a reader to an empty tool: {n}");
        // The header is ONE paragraph. A `\` line-continuation that collapsed
        // in authoring would ship this literal's own source indentation to the
        // reader — CLAUDE.md, "A user-facing message is ONE paragraph", whose
        // second shape is a run of spaces with no newline before it.
        assert!(!n.lines().next().unwrap().contains("  "), "got: {n}");
        // One line per entry, plus the header — a frame that folded two
        // notices onto one line would be a frame that lost one.
        assert_eq!(n.lines().count(), 3, "got: {n}");
        assert!(n.contains("[run-green] run-completed from w-1:"), "got: {n}");
        assert!(n.contains("[planner-exited] planner-exited from loomux:"), "got: {n}");
    }

    #[test]
    fn a_long_notice_is_quoted_not_replayed_and_never_breaks_a_char() {
        let mut d = Deferred::default();
        d.push(Entry { text: "é".repeat(400), ..entry(0) });
        let n = d.flush_notice(0, FlushCause::Deadline).expect("one entry frames");
        assert_eq!(n.lines().count(), 2, "a multi-line payload stays one line: {n}");
        assert!(n.ends_with('…'), "the quote is cut and marked: {n}");
        assert!(
            !n.contains(&"é".repeat(200)),
            "the frame summarises rather than replaying the payload: {n}"
        );
    }

    #[test]
    fn every_flush_cause_renders_its_own_clause() {
        // Review round 2: `PolicyOff` reused `Deadline`'s variant, so a release
        // caused by triage being switched off told the reader it had hit the
        // deferral deadline — a wrong value on a surface a human reads. Pinned
        // as a SET, so a fifth cause cannot quietly share a fourth's words.
        let mut d = Deferred::default();
        d.push(entry(0));
        // The parenthesised clause and nothing else: cut at the CLOSING paren
        // as well as the opening one. Taking the whole tail swept the header
        // and the entry list in with it.
        let clause = |c: FlushCause| {
            let n = d.flush_notice(0, c).expect("one entry frames");
            let after = n.split_once(" (").expect("the cause is parenthesised").1.to_string();
            after.split_once(')').expect("the clause closes").0.to_string()
        };
        let got: Vec<(&str, String)> = [
            FlushCause::Wake,
            FlushCause::Deadline,
            FlushCause::Full,
            FlushCause::PolicyOff,
        ]
        .into_iter()
        .map(|c| (c.as_str(), clause(c)))
        .collect();
        assert_eq!(
            got,
            vec![
                ("wake", "flushed because something did need you".to_string()),
                ("deadline", "flushed on the deferral deadline".to_string()),
                ("full", "flushed because the deferred store filled".to_string()),
                ("policy-off", "flushed because triage was turned off".to_string()),
            ]
        );
        // The wire spellings are distinct too — they land in an audit row.
        let mut names: Vec<&str> = got.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "two causes share a wire spelling");
    }

    #[test]
    fn the_frame_flattens_a_multi_line_payload() {
        let mut d = Deferred::default();
        d.push(Entry { text: "first\nsecond\r\nthird".into(), ..entry(0) });
        let n = d.flush_notice(0, FlushCause::Full).expect("one entry frames");
        assert_eq!(n.lines().count(), 2, "got: {n}");
        assert!(n.ends_with("first second third"), "got: {n}");
    }

    #[test]
    fn the_store_round_trips_through_its_wire_form() {
        // `deferred.json` survives a loomux restart; a store that did not
        // round-trip would drop exactly the notices this module promises
        // never to drop.
        let mut d = Deferred::default();
        d.push(entry(1_000_000));
        let json = serde_json::to_string(&d).expect("serialise");
        assert_eq!(serde_json::from_str::<Deferred>(&json).expect("parse"), d);
        // An absent `entries` key reads as an empty store rather than failing
        // the parse — a file truncated by a crash must not wedge delivery.
        assert_eq!(serde_json::from_str::<Deferred>("{}").expect("parse"), Deferred::default());
    }
}
