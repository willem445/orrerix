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
//! THE VOCABULARY IS NOT NEW. `doc/design/orchestration-evals.md` §4.1 and
//! `scripts/orch-scorecard.cjs` already classify an orchestrator-bound prompt
//! by its leading shape, and the census that motivated this module was taken
//! with that classifier. [`classify`] is that same table in Rust — the engine
//! has no `regex` dependency, so the patterns are hand-written prefix tests,
//! but the CLASSES and their order are the scorecard's. A second, divergent
//! classifier is exactly what this module exists not to be.
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
//! See `doc/design/delivery-triage.md`.

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
/// The variants are `orch-scorecard.cjs`'s two tables unioned (`WAKE_SHAPES`
/// plus `ORCH_PROMPT_SHAPES`), because the census this module implements used
/// both: the scorecard splits "what woke it" from "what the drive cost it",
/// and triage needs one table that names every row of #3304's Q1.
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
    if rest.starts_with("run ") && rest.contains(": completed") {
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
const NEEDS_YOU_MARKERS: [&str; 6] = [
    "blocking on you",
    "needs you",
    "needs your",
    "you must rule",
    "your call",
    "decision is yours",
];

/// Markers for the re-grounding / restored class, which arrives as a
/// `system-notice` and must never be held back: a pane that has just lost its
/// context is the one pane a delayed notice hurts most.
const REGROUNDING_MARKERS: [&str; 3] =
    ["context was compacted", "orchestration restored", "re-grounding"];

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
pub enum Rule {
    /// `GATE SATISFIED` on a repo whose merge queue is on: the queue's own
    /// gate re-check is the decision, and the orchestrator's next move was
    /// `queue_merge` anyway.
    GateSatisfied,
    /// A `notify_when` `workflow_run` that came back `conclusion: success`.
    RunGreen,
    /// A `notify_when` `pr_checks` that came back `SUCCESS`.
    ChecksGreen,
    /// A planner that posted its plan and exited — the plan drive (#3040)
    /// consumes this; the notice is a slot-free FYI.
    PlannerExited,
    /// A pane that exited; the roster already reflects it.
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
            Rule::GateSatisfied => "gate-satisfied",
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
    Defer(Rule),
    /// `GATE SATISFIED` on PR N with the merge queue ENABLED. The caller
    /// attempts the enqueue and turns this into `Defer(Rule::GateSatisfied)`
    /// on success, or `Deliver(DeliverReason::NoRule)` on any refusal.
    ///
    /// It is a third variant rather than a `Defer` the caller may undo
    /// because the enqueue is the whole of the rule's justification: the
    /// orchestrator's next step for a satisfied gate is `queue_merge`, so a
    /// notice suppressed WITHOUT one having happened is a PR nobody is
    /// driving. The impure attempt cannot live in this module, and the
    /// decision must not be readable as "deferred" until it has succeeded.
    TryEnqueue { pr: u64 },
}

/// One delivery, as the rule tier sees it.
pub struct Input<'a> {
    pub text: &'a str,
    /// The `from` the audit row carries — an agent id, or loomux's own actor.
    pub from: &'a str,
    /// True when the sender is the human rather than the fleet.
    pub human_actor: bool,
    /// Whether this repo's `merge_queue:` block is on. Read by the
    /// `GATE SATISFIED` rule and by nothing else.
    pub merge_queue_enabled: bool,
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
        Kind::DriveGateSatisfied => match (input.merge_queue_enabled, pr_of(input.text)) {
            // The queue is the rule's justification, so no queue means no
            // rule — a satisfied gate on a repo that merges by hand is a
            // thing the orchestrator has to act on.
            (true, Some(pr)) => Decision::TryEnqueue { pr },
            _ => Decision::Deliver(DeliverReason::NoRule),
        },
        Kind::RunCompleted if run_is_green(input.text) => Decision::Defer(Rule::RunGreen),
        Kind::PrChecks if checks_are_green(input.text) => Decision::Defer(Rule::ChecksGreen),
        Kind::PlannerExited => Decision::Defer(Rule::PlannerExited),
        Kind::AgentExited => Decision::Defer(Rule::AgentExited),
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
}

impl FlushCause {
    pub fn as_str(self) -> &'static str {
        match self {
            FlushCause::Wake => "wake",
            FlushCause::Deadline => "deadline",
            FlushCause::Full => "full",
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
        };
        let mut s = format!(
            "{PREFIX}{n} {plural} deferred over the last {mins} min ({tail}). \
             Each closed by a shape rule, none of them a decision; full text via \
             list_deferred():"
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
    const RUN_GREEN: &str =
        "[orrerix] run 17812: completed — conclusion: success. Note (registered): green → \
         next merge; red → fix forward. (watch n-1)";
    const RUN_RED: &str = "[orrerix] run 17813: completed — conclusion: failure. (watch n-2)";
    const CHECKS_GREEN: &str =
        "[orrerix] PR #241 checks: SUCCESS — all 6 checks passed (watch n-3)";
    const CHECKS_RED: &str = "[orrerix] PR #241 checks: FAILURE — 1 of 6 failed (watch n-3)";
    const PLANNER_EXIT: &str =
        "[orrerix] planner p-1 (plan-2890) posted its plan and exited — its delegate slot is free.";
    const AGENT_EXIT: &str =
        "[orrerix] agent w-2902 (worker-adv) exited (code Some(0)) 91000ms after its last output.";
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
        for (text, want) in cases {
            assert_eq!(classify(text), *want, "classifying: {text}");
        }
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

    // ---------- the never-triaged set: these ALWAYS deliver

    fn on() -> Policy {
        Policy { enabled: true, ..Policy::default() }
    }

    fn input<'a>(text: &'a str) -> Input<'a> {
        Input { text, from: "w-1", human_actor: false, merge_queue_enabled: true }
    }

    #[test]
    fn the_never_triaged_set_always_delivers() {
        let cases: &[(&str, NeverReason)] = &[
            (HELD, NeverReason::DriveHeld),
            (BLOCKED, NeverReason::DelegateBlocked),
            (WATCHDOG, NeverReason::Watchdog),
            ("merge it when you get a chance", NeverReason::HumanActor),
            (
                "[orrerix] Context was compacted — re-read your instructions.",
                NeverReason::Regrounding,
            ),
            (
                "[orrerix] w-4 reports done: PR #9 is up but BLOCKING ON YOU: A or B?",
                NeverReason::NeedsYou,
            ),
        ];
        for (text, why) in cases {
            assert_eq!(
                decide(&input(text), &on()),
                Decision::Deliver(DeliverReason::Never(*why)),
                "must never be triaged: {text}"
            );
        }
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
    fn a_satisfied_gate_asks_the_caller_to_enqueue_rather_than_deferring_on_its_own() {
        // NOT a `Defer`: the rule is justified by the enqueue, so the decision
        // stays unresolved until the registry has actually made one.
        assert_eq!(decide(&input(GATE), &on()), Decision::TryEnqueue { pr: 1758 });
    }

    #[test]
    fn a_red_run_and_red_checks_are_delivered() {
        assert_eq!(decide(&input(RUN_RED), &on()), Decision::Deliver(DeliverReason::NoRule));
        assert_eq!(decide(&input(CHECKS_RED), &on()), Decision::Deliver(DeliverReason::NoRule));
    }

    #[test]
    fn a_satisfied_gate_with_the_queue_disabled_is_delivered() {
        let mut i = input(GATE);
        i.merge_queue_enabled = false;
        assert_eq!(decide(&i, &on()), Decision::Deliver(DeliverReason::NoRule));
    }

    #[test]
    fn a_delegate_done_is_never_a_rule_in_this_slice() {
        // #3304 Q2: the `done` class is where the residual JUDGEMENT lives,
        // and S1 ships no provider — so all 90 of them deliver.
        assert_eq!(decide(&input(DONE), &on()), Decision::Deliver(DeliverReason::NoRule));
        assert_eq!(decide(&input(APPROVED), &on()), Decision::Deliver(DeliverReason::NoRule));
    }

    #[test]
    fn triage_off_delivers_everything_the_rules_would_have_held() {
        let off = Policy::default();
        assert!(!off.enabled, "the product default is off");
        for text in [RUN_GREEN, CHECKS_GREEN, PLANNER_EXIT, AGENT_EXIT, CANCELLED, GATE] {
            assert_eq!(
                decide(&input(text), &off),
                Decision::Deliver(DeliverReason::Disabled),
                "off must change nothing: {text}"
            );
        }
    }

    #[test]
    fn a_kind_left_off_the_kinds_list_is_delivered() {
        let narrow = Policy { enabled: true, kinds: vec![Kind::RunCompleted], ..Policy::default() };
        assert_eq!(decide(&input(RUN_GREEN), &narrow), Decision::Defer(Rule::RunGreen));
        assert_eq!(
            decide(&input(PLANNER_EXIT), &narrow),
            Decision::Deliver(DeliverReason::KindNotTriaged)
        );
    }

    // ---------- plan chunks

    #[test]
    fn plan_chunks_coalesce_and_the_last_one_wakes() {
        let c = |k: u32, n: u32| {
            format!("[orrerix] message from p-1: ---BEGIN PLAN {k}/{n}--- slices: …")
        };
        assert_eq!(decide(&input(&c(1, 4)), &on()), Decision::Defer(Rule::PlanChunk));
        assert_eq!(decide(&input(&c(3, 4)), &on()), Decision::Defer(Rule::PlanChunk));
        assert_eq!(
            decide(&input(&c(4, 4)), &on()),
            Decision::Deliver(DeliverReason::NoRule),
            "the last chunk is the wake that carries the others out"
        );
    }

    #[test]
    fn a_malformed_plan_marker_falls_through_to_deliver() {
        for bad in [
            "[orrerix] message from p-1: ---BEGIN PLAN 1/1--- one chunk is not a split",
            "[orrerix] message from p-1: ---BEGIN PLAN 5/4--- past the end",
            "[orrerix] message from p-1: ---BEGIN PLAN 0/4--- zero",
            "[orrerix] message from p-1: ---BEGIN PLAN one/four--- words",
            "[orrerix] message from p-1: ---BEGIN PLAN 1/4 no closing marker",
        ] {
            assert_eq!(
                decide(&input(bad), &on()),
                Decision::Deliver(DeliverReason::NoRule),
                "must fail safe: {bad}"
            );
        }
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
        assert!(n.contains("list_deferred()"), "the read-back is named: {n}");
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
