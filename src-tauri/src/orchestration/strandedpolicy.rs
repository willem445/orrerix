//! Stranded deliveries' pure decisions: what blocks one, self-heal and kickoff
//! recovery, badge release and admission, the stranded marker, and flushing
//! stranded text before a paste.
//! Design note: `docs/design/delivery-triage.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): `PtyManager`,
//! `crate::pty`, `lock_safe` (`crate::obs`). Sibling files it calls:
//! `humaninput.rs`, `questionhold.rs`, `submit.rs`, `tier1.rs`.

use super::*;

// ────────────── #496 PR-C: actuation on a stranded delivery ──────────────
//
// #451 gave a delivery three honest states and #445/#470 made a held payload
// queued-never-destroyed — but nothing ACTS on `Failed`. For an
// orchestrator-target delivery both notices are suppressed by design (loop
// avoidance — `should_notify_unconfirmed`/`should_notify_paste_held`), and in
// an idle group there is no NEXT delivery whose own pre-paste flush would
// eventually press the withheld Enter. The prompt then sits in the box until
// a human notices and presses Enter by hand — the human being the recovery
// mechanism, which is the defect #496 is about (observed on Claude panes as
// well as copilot ones: the root cause is CLI-agnostic).
//
// The guarantee this section adds: **a delivery ends Confirmed, or a bounded
// self-heal fires, or a human-visible attention badge is raised. No path
// ends silent-and-wedged.**
//
// Two deliberate non-inventions:
// - The re-submit is NOT a new delivery and NOT a raw write from the monitor
//   thread. It is admitted as a `StrandedSubmit` marker at the FRONT of the
//   pane's queue (`enqueue_stranded_front`) and pressed by the drainer —
//   the same single-consumer path #470 made the only way anything reaches a
//   pane, and the same marker `AbortedPreEnter` already uses. Front, not
//   back, because the stranded text is physically in the box already:
//   anything queued behind it must not paste on top of it. A raw write here
//   would race the drainer mid-paste and re-open the ordering hole #470
//   closed.
// - The guardrails are re-used, not re-implemented: `drain_stranded_submit`
//   → `flush_stranded_text` re-derives `human_typed_since` from the ledger
//   and re-reads the live question state (#420) at the instant of the press.
//   `stranded_selfheal_action` below is the TRIGGER gate, deciding whether a
//   heal is worth admitting at all; it never becomes the last word on
//   whether the Enter is safe.

/// Why a stranded delivery could not be self-healed (#496 PR-C). Carried into
/// the attention badge so "this pane needs you" always says what is in the
/// way, rather than leaving the human to work it out from the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrandedBlocker {
    /// A human has typed into the pane since our own submit, so the box may
    /// hold THEIR line. Never merge-submit over human-typed content — the
    /// same rule `should_flush_before_paste` has enforced since #81/#84.
    HumanInput,
    /// A live interactive question owns the Enter key (#420): pressing it
    /// would answer the question, not submit our prompt.
    Question,
    /// Our pasted text is no longer at the box's tail end, so there is
    /// nothing identifiable to re-submit — the delivery still failed, and
    /// the human still needs to know, but loomux must not press Enter into
    /// a pane whose state it can no longer account for.
    NotHolding,
    /// #559: loomux could not read enough of the pane's tail to contain the
    /// text it pasted (`BoxReading::Unverifiable`), so whether the prompt is
    /// still sitting in the box is unknown. Deliberately NOT folded into
    /// `NotHolding`: that variant's wording tells the human their text is
    /// *gone*, which here would be a claim from a `false` that was arithmetic
    /// rather than observation. Same non-action as `NotHolding` — loomux
    /// never presses Enter into a pane it cannot account for — but an honest
    /// name for a different reason, so a badge, an audit line and a grep can
    /// all tell "we looked and it is gone" from "we could not look".
    Unverifiable,
    /// The per-delivery heal budget (`STRANDED_SELFHEAL_MAX_HEALS`) is spent.
    Exhausted,
    /// The pane's delivery queue is at cap, so the re-submit could not even
    /// be queued (rev-47 NB4). Deliberately NOT folded into `Exhausted`:
    /// that one's wording says a heal already fired and did not take, which
    /// would be a false claim here — nothing was attempted at all.
    QueueFull,
    /// #563: the pane's delivery queue has reached `queue::QUEUE_NEAR_FULL_AT`
    /// — nothing has been lost yet, and the point of the badge is that it says
    /// so *before* anything is.
    ///
    /// **Why this is a separate variant and not `QueueFull`.** `QueueFull`'s
    /// wording asserts that loomux *could not queue* something, which would be
    /// a false claim while the queue is still accepting — the same
    /// unbacked-claim class that made `QuestionStale` a separate variant from
    /// `Question` and `QueueFull` a separate one from `Exhausted`. The two
    /// also want different actions from the human: `QueueFull` says "work has
    /// already been dropped, go look"; this one says "release the pane and
    /// nothing will be".
    ///
    /// **Why a badge and not only a notice.** The in-band channel
    /// (`notify_queue`) is suppressed whenever the target is the group's own
    /// orchestrator, which is precisely the pane #563 was reported on. A badge
    /// has no such suppression, so this is the channel that survives the case
    /// that matters. Released on evidence — the depth falling back to
    /// `CapacityState::Normal` — never on a timer.
    QueueNearFull,
    /// #563: the pane's queue has reached `queue::QUEUE_MAX_PER_PANE`, read
    /// from the DEPTH and nothing else.
    ///
    /// **Why this is not `QueueFull` (rev-10 finding 1, applied consistently).**
    /// `QueueFull`'s wording says loomux *could not even queue a re-send* and
    /// tells the human to *press Enter in the pane*. Both are true where it is
    /// raised, from `actuate_stranded`: there a self-heal really was refused
    /// and stranded text really is sitting in the box. Neither is established
    /// by a depth reading — `note_queue_capacity` never consults hold state,
    /// and the transition INTO `Full` fires on the admission that took the last
    /// slot, before anything has been rejected at all. Reusing `QueueFull`
    /// there would send a human to press Enter on a pane with nothing to
    /// submit: the same false-claim class this issue exists to eliminate, and
    /// the same reason `QuestionStale` was minted rather than reusing
    /// `Question`.
    ///
    /// Says what depth establishes and then stops: the queue is at the cap, so
    /// arrivals are being dropped. Released on the same evidence
    /// `QueueNearFull` is — a depth that has actually come back down.
    QueueAtCapacity,
    /// #532: the interactive-question guard has held this pane's delivery for
    /// longer than [`QUESTION_HOLD_STALE_AFTER`], and loomux cannot tell
    /// whether the question it still detects is real.
    ///
    /// **Why this is a badge and never a release.** `prompt_wait_detected`
    /// reads `PtyManager::output_tail_bounded`, which is an append-only byte
    /// RING, not a screen: an answered question stays inside the last
    /// `QUESTION_SCAN_TAIL_BYTES` on a pane that has gone quiet, so the
    /// structured signals it matches on ("(y/n)", "do you want to proceed")
    /// keep matching indefinitely. That detector's own doc says as much — "this
    /// alone can't tell a live prompt from the same words scrolled past" — and
    /// the #420 hold is the one consumer that pairs it with nothing. From bytes
    /// alone the two states are identical, so there is no reading that could
    /// justify releasing.
    ///
    /// The asymmetry decides what to do about that. A prompt held too long is
    /// recoverable by a human who is *told* about it; an Enter that
    /// auto-answers a live consent dialog is not recoverable at all — it
    /// silently steers the agent, which is the exact harm #420 exists to
    /// prevent. So the bound stops the hold re-arming *silently* and names the
    /// staleness hypothesis to the human; it never converts into a write. See
    /// [`held_escalation`].
    ///
    /// **Which human, and when (rev-12 NB4).** Be exact about the channel this
    /// argument rests on, because it is narrower than "told". The telling is
    /// `mark_stranded` — the `attn_stranded` badge (a chip in the desktop
    /// window) plus an audit line, the established #496 PR-C channel. It is
    /// **not** an in-band notice to any agent: `notify_queue` returns early
    /// with `notice-suppressed` whenever `target_is_orchestrator`, which is
    /// exactly the pane in #532's own incident. So on an attended session the
    /// human sees the chip; on an unattended overnight run **nobody is told
    /// until someone next looks at the window**, which is what happened. That
    /// does not change the release/badge decision — an unattended run is
    /// precisely where a wrong Enter is least recoverable — but "a human is
    /// told" should not be read as "somebody is paged".
    ///
    /// The approach that could answer what this variant can only ask is
    /// [`termgrid`] (#530): rendered rows, where "still displayed" is a
    /// real reading. Note it is NOT a drop-in — `termgrid::render_screen`
    /// returns scrolled-off history rows *followed by* the on-screen rows, so
    /// pointing the detector at it unchanged would reproduce this same bug.
    /// See `docs/design/orchestration.md`'s #532 section for what the follow-up
    /// actually needs.
    QuestionStale,
    /// #569: deliveries aimed at this pane were DISCARDED while the group was
    /// paused, and the resume-time notice that would have said so could not be
    /// delivered to the group's orchestrator.
    ///
    /// **Two ways to earn it, and only one of them is history** (review B2).
    /// Option 2 (enqueue-while-paused) removed the pause branch's discard, so
    /// the LEGACY cause needs a group paused under an older loomux. But a pane
    /// already at `queue::QUEUE_MAX_PER_PANE` still has admissions refused for
    /// as long as a pause lasts, and that loses a payload on THIS build — see
    /// `SuppressedCause` and `announce_pause_suppression`.
    ///
    /// **Why this is a badge and not only the notice.** The notice
    /// (`announce_pause_suppression`) is an in-band delivery to the
    /// orchestrator, so it fails exactly when a paused group has been left long
    /// enough for its orchestrator to idle out or be killed — which is the
    /// longest, most damaging pause, not the mildest. A notice that goes
    /// missing precisely in the worst case is the same silent-loss shape #569
    /// exists to close, one level up. `mark_stranded` has no role suppression
    /// and needs no orchestrator at all, so it is the channel that survives
    /// (the #563 argument, applied to a different hold).
    ///
    /// **It claims only what the audit record establishes**: that something
    /// addressed to this pane was thrown away, and that nothing is queued to
    /// arrive. It does NOT say the pane is held, that text is sitting
    /// unsubmitted in its box, or that loomux is re-sending anything — none of
    /// which a suppression record shows, and each of which is some other
    /// variant's sentence (`HumanInput`, `Exhausted`, the `None` heal wording).
    /// Cleared like any other badge, by `clear_stranded`.
    PauseSuppressed,
}

impl StrandedBlocker {
    /// Stable audit token — the string that lands in the audit log, kept
    /// separate from the human-facing badge text so one can change without
    /// silently breaking greps over the other.
    pub fn as_str(self) -> &'static str {
        match self {
            StrandedBlocker::HumanInput => "human-input",
            StrandedBlocker::Question => "question",
            StrandedBlocker::NotHolding => "not-holding",
            // #559: its own token so a grep tells "we read the box and our
            // text is gone" from "we could not read enough of the box to
            // tell" — the distinction the whole change exists to preserve.
            StrandedBlocker::Unverifiable => "box-unverifiable",
            StrandedBlocker::Exhausted => "heal-budget-spent",
            StrandedBlocker::QueueFull => "queue-full",
            StrandedBlocker::QueueNearFull => "queue-near-full",
            StrandedBlocker::QueueAtCapacity => "queue-at-capacity",
            StrandedBlocker::QuestionStale => "question-hold-stale",
            StrandedBlocker::PauseSuppressed => "pause-suppressed",
        }
    }
}

/// What to do about a delivery the late monitor just declared `Failed`
/// (#496 PR-C).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrandedAction {
    /// The ledger says this delivery is no longer outstanding (confirmed, or
    /// superseded by one that was). Nothing to heal and nothing to badge.
    Resolved,
    /// Safe: admit a bounded re-submit through the delivery queue.
    SelfHeal,
    /// Not safe (or budget spent): raise the pane's attention badge instead
    /// of waiting silently.
    Attention(StrandedBlocker),
}

/// How many self-heal submits loomux may fire for ONE stranded delivery
/// (#496 PR-C). One, deliberately: the heal is an Enter into a pane whose
/// state we can only observe indirectly, and the failure mode of pressing it
/// twice (a second Enter landing on whatever the first one opened) is worse
/// than the failure mode of not pressing it again (a badge the human
/// already has in front of them). The bound is also structural today —
/// `late_monitor_tick` returns `DeclareFailed` at most once per monitor,
/// since every later tick sees `already_failed` — but that is the *caller's*
/// precedence, and a future edit could change it without noticing this;
/// counting explicitly means the cap survives that edit and is directly
/// testable rather than inferred.
pub const STRANDED_SELFHEAL_MAX_HEALS: u32 = 1;

/// The one decision point for #496 PR-C: given everything known about a
/// just-`Failed` delivery, self-heal or badge? Pure, so the precedence — and
/// in particular that the human-content guard is checked FIRST — is directly
/// pinnable rather than an inline `if` chain a future edit could reorder
/// (the argument `final_window_outcome` and `late_monitor_tick` already make
/// in `unconfirmed.rs`).
///
/// Inputs, in the order they are consulted:
/// - `ledger_outstanding` — the DURABLE artifact, not a pane heuristic: this
///   pane's recorded `DeliveryOutcome` is still THIS delivery's and still
///   unconfirmed. Anything else means the ledger already moved on (a heal
///   that landed, a newer delivery that confirmed) and this monitor has
///   nothing to actuate. Checked first so no pane reading can ever override
///   what the ledger says about whether a delivery is still outstanding.
/// - `human_typed_since` — a human keystroke landed after our submit
///   (`tier1_trusted`'s inverse). The one absolute: loomux never submits
///   over human-typed content, so this outranks every other input including
///   the heal budget.
/// - `question_on_screen` — #420's guard. False by construction at today's
///   only call site (`late_monitor_tick` will not declare failure while a
///   question is up), and re-checked authoritatively at press time by
///   `flush_stranded_text`; taken as a parameter anyway so this function
///   does not silently depend on the caller's precedence staying that way.
/// - `reading` — what Tier 1's box read established (`BoxReading`). #559 made
///   this a three-state rather than a bool: `Unverifiable` badges as its own
///   blocker instead of borrowing `NotHolding`'s "its text is gone" claim.
///   Both refuse to self-heal, so this widens nothing about when loomux
///   presses Enter — the ONE thing that could make a badge-only distinction
///   dangerous — it only stops the badge from asserting what loomux does not
///   know.
/// - `heals_used` / `max_heals` — the bound.
pub fn stranded_selfheal_action(
    ledger_outstanding: bool,
    human_typed_since: bool,
    question_on_screen: bool,
    reading: BoxReading,
    heals_used: u32,
    max_heals: u32,
) -> StrandedAction {
    if !ledger_outstanding {
        return StrandedAction::Resolved;
    }
    if human_typed_since {
        return StrandedAction::Attention(StrandedBlocker::HumanInput);
    }
    if question_on_screen {
        return StrandedAction::Attention(StrandedBlocker::Question);
    }
    match reading {
        BoxReading::Unverifiable => {
            return StrandedAction::Attention(StrandedBlocker::Unverifiable);
        }
        BoxReading::NotHolding => return StrandedAction::Attention(StrandedBlocker::NotHolding),
        BoxReading::Holds => {}
    }
    if heals_used >= max_heals {
        return StrandedAction::Attention(StrandedBlocker::Exhausted);
    }
    StrandedAction::SelfHeal
}

// ───────────── #517: a fresh spawn's kickoff that never reached the pane ─────────────
//
// #496 PR-C (above) recovers a delivery whose text IS in the box and whose
// Enter was withheld. A fresh spawn's kickoff fails a DIFFERENT way: the
// paste itself is swallowed by a CLI whose stdin reader has not attached yet
// (`ECHO_WINDOW`'s own comment: "observed live with copilot, whose input
// attaches well after its UI paints"), so nothing ever lands in the box.
// `stranded_selfheal_action` correctly refuses that state — `NotHolding`,
// "there is nothing identifiable to re-submit" — and today the story ends
// there: a badge, a notice, and a worker sitting idle with no brief until a
// human or the orchestrator re-sends by hand. That is #517: six instances in
// one day, and the reason the agent's own recovery ("re-send with
// send_prompt") is a person's job.
//
// The recovery for THIS shape is not an Enter, it is the brief again — and
// it goes through `deliver_prompt`'s own front door (#470), not a write from
// the monitor thread, so it inherits ordered admission, the byte-identical
// coalesce, the audit trail, and every paste guard. The kickoff was never
// outside that machinery; only its FAILURE was.
//
// **Why this cannot double-deliver a kickoff that actually landed.** Three
// independent layers, in the order they are consulted:
//   1. A landed kickoff resolves before it ever gets here — `late_monitor_
//      tick` returns `Confirm` on the `promptsubmit` hook record (installed
//      for every spawned agent, both CLIs, at spawn) and the monitor exits.
//      `DeclareFailed` is unreachable for it.
//   2. `ledger_outstanding` — the durable artifact, not a pane heuristic.
//   3. `output_since_submit` — a kickoff that landed makes the agent run its
//      whole first turn; one that was eaten leaves the pane exactly as boot
//      left it. This is the observed discriminator from the live data: two
//      fresh spawns lost their brief and sat silent while a third received
//      its brief and worked normally.
// Plus the queue's own coalesce, which collapses a re-admission of
// byte-identical text into an entry still waiting, and a budget of one.

/// How many re-deliveries loomux may fire for ONE lost fresh kickoff (#517).
/// One, for the same reason `STRANDED_SELFHEAL_MAX_HEALS` is one: the
/// recovery acts on a pane whose state we can only observe indirectly, and
/// the cost of a second wrong re-delivery (a duplicate brief the agent has
/// to reconcile) is worse than the cost of stopping (a badge and a notice
/// the human already has in front of them). Counted explicitly rather than
/// relied on structurally, so the bound survives an edit to the caller's
/// precedence and is directly testable.
pub const KICKOFF_REDELIVERY_MAX: u32 = 1;

/// Output growth since a kickoff's own submit baseline that counts as "this
/// agent really did receive its brief and start a turn" (#517) — the guard
/// that makes re-delivery safe.
///
/// Sized to sit far above the residual an EATEN kickoff can produce and far
/// below what a LANDED one produces. A landed kickoff makes the CLI echo the
/// brief into its transcript and begin a reply, and the monitor only ever
/// looks after `PENDING_IDLE_QUIET` of total silence — so a turn that
/// happened has painted kilobytes by then. (Review F3: an earlier version of
/// this comment also claimed a kickoff brief is itself larger than this. It
/// is not — real briefs run ~1-2 KB — and the discriminator never rested on
/// that; the claim is deleted rather than softened.) An eaten kickoff leaves
/// a stray Enter on an empty box: a repaint, nothing more. The asymmetry is
/// deliberate — this bar being too HIGH only declines a recovery that was
/// needed (falling back to today's badge, the pre-#517 behavior), while too
/// LOW would re-deliver a brief that landed. Bias toward the reversible
/// mistake.
pub const KICKOFF_TURN_EVIDENCE_BYTES: u64 = 4096;

/// Why a lost-kickoff re-delivery was declined (#517) — carried into the
/// audit so "loomux did not re-send" always says which condition stopped it,
/// the same discipline `StrandedBlocker` gives the badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickoffDecline {
    /// Not a fresh spawn's kickoff at all (a mid-session delivery, or a
    /// resume re-sync). Only a FRESH kickoff's payload is unrecoverable
    /// when it never lands — see `kickoff_recovery_action`'s doc.
    NotAKickoff,
    /// The ledger says this delivery is no longer outstanding.
    Resolved,
    /// A human typed into the pane after our submit.
    HumanInput,
    /// A live interactive question owns the pane (#420).
    Question,
    /// The pane produced a turn's worth of output after our submit, on a
    /// pane we watched go ready before pasting — the brief landed after all,
    /// whatever the confirmation tiers saw.
    TurnStarted,
    /// The pane produced a turn's worth of output after our submit, but this
    /// delivery pasted BLIND (`ReadyWait::TimedOut` — the boot wait hit
    /// `READY_MAX_WAIT` with the CLI still painting), so that growth cannot
    /// be attributed to a turn rather than to the boot paint that was
    /// already running (review F5). Declines exactly like `TurnStarted` —
    /// the conservative fallback is the same — but must not CLAIM a turn the
    /// evidence does not support: `.loomux/lessons.md`, "a claim is a
    /// deliverable". The delivery's own `prompt-typed` record carries
    /// `ready_observed: false` alongside, so the two facts are greppable
    /// together.
    OutputUnattributable,
    /// The per-delivery re-delivery budget is spent.
    Exhausted,
}

impl KickoffDecline {
    /// Stable audit token, kept separate from any human-facing wording so
    /// one can change without silently breaking greps over the other.
    pub fn as_str(self) -> &'static str {
        match self {
            KickoffDecline::NotAKickoff => "not-a-kickoff",
            KickoffDecline::Resolved => "resolved",
            KickoffDecline::HumanInput => "human-input",
            KickoffDecline::Question => "question",
            KickoffDecline::TurnStarted => "turn-started",
            KickoffDecline::OutputUnattributable => "output-unattributable",
            KickoffDecline::Exhausted => "redelivery-budget-spent",
        }
    }
}

/// What to do about a fresh kickoff the late monitor just declared `Failed`
/// with nothing left in the box (#517).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickoffRecovery {
    /// Re-admit the brief through the delivery queue's front door.
    Redeliver,
    /// Leave it to the attention badge `stranded_selfheal_action` already
    /// raised, and audit why.
    Decline(KickoffDecline),
}

/// The one decision point for #517. Pure, so the precedence is directly
/// pinnable rather than an inline `if` chain a future edit could reorder —
/// the argument `stranded_selfheal_action` and `late_monitor_tick` already
/// make (the second in `unconfirmed.rs`).
///
/// Consulted ONLY after `stranded_selfheal_action` has returned
/// `Attention(StrandedBlocker::NotHolding)`: that is the eaten-paste
/// signature (the delivery failed AND its text is not in the box), and it is
/// the only state where re-sending the text is the right recovery rather
/// than pressing Enter. Every other `StrandedAction` keeps its pre-#517
/// behavior untouched.
///
/// Inputs, in the order they are consulted:
/// - `is_fresh_kickoff` — a FRESH spawn's brief, not a mid-session delivery
///   and not a resume re-sync. Narrow on purpose: a fresh kickoff is the one
///   payload with no other route to the agent (it is the agent's entire
///   reason to exist, and nothing will re-send it), whereas a mid-session
///   prompt has a sender who is still around and a resume notice is
///   re-derivable from durable state.
/// - `ledger_outstanding` — the DURABLE artifact: this pane's recorded
///   `DeliveryOutcome` is still THIS delivery's and still unconfirmed.
///   Checked before any pane reading, exactly as `stranded_selfheal_action`
///   checks it first.
/// - `human_typed_since` / `question_on_screen` — the same two absolutes the
///   self-heal honors. A re-delivery pastes rather than pressing Enter, so
///   it is less dangerous than a heal here, but "loomux does not act on a
///   pane a person is using" is a rule this feature has no business
///   weakening.
/// - `output_since_submit` vs `turn_evidence_bytes` — the anti-double-
///   delivery guard (see the section comment above). `ready_observed`
///   (review F5) only ever changes which DECLINE this reports, never whether
///   it declines: growth on a pane we never watched go ready is
///   `OutputUnattributable` rather than `TurnStarted`, because the boot paint
///   that was still running is an equally good explanation for it.
/// - `redeliveries_used` / `max_redeliveries` — the bound.
#[allow(clippy::too_many_arguments)]
pub fn kickoff_recovery_action(
    is_fresh_kickoff: bool,
    ledger_outstanding: bool,
    human_typed_since: bool,
    question_on_screen: bool,
    output_since_submit: u64,
    turn_evidence_bytes: u64,
    ready_observed: bool,
    redeliveries_used: u32,
    max_redeliveries: u32,
) -> KickoffRecovery {
    if !is_fresh_kickoff {
        return KickoffRecovery::Decline(KickoffDecline::NotAKickoff);
    }
    if !ledger_outstanding {
        return KickoffRecovery::Decline(KickoffDecline::Resolved);
    }
    if human_typed_since {
        return KickoffRecovery::Decline(KickoffDecline::HumanInput);
    }
    if question_on_screen {
        return KickoffRecovery::Decline(KickoffDecline::Question);
    }
    if output_since_submit >= turn_evidence_bytes {
        return KickoffRecovery::Decline(if ready_observed {
            KickoffDecline::TurnStarted
        } else {
            KickoffDecline::OutputUnattributable
        });
    }
    if redeliveries_used >= max_redeliveries {
        return KickoffRecovery::Decline(KickoffDecline::Exhausted);
    }
    KickoffRecovery::Redeliver
}

/// The kickoff treatment a lost-kickoff RE-delivery's own drain runs under
/// (#517, review F4) — `None` when it must run under none at all.
///
/// **The finding.** The recovery used to nudge the drainer with `None`, so
/// the re-delivery pasted with `wait_ready: false`. That is the one place
/// this feature could have reproduced the bug it exists to fix: a CLI whose
/// stdin reader has still not attached would eat the re-delivered brief too.
/// It is NOT impossible by construction — merely unlikely, since the monitor
/// only declares failure after `READY_MAX_WAIT` plus `PENDING_IDLE_QUIET`,
/// by which point a healthy CLI has long been reading — and "unlikely" is
/// not the bar for the ghost of the original defect. So the re-delivery gets
/// the SAME boot wait the original kickoff had. It costs `READY_MIN_WAIT`
/// (1.5s) on a pane that is already ready, which is nothing against a lost
/// brief.
///
/// The other two flags are deliberately NOT copied from the original:
/// - `confirm_autopilot: false` — copilot's consent dialog is triggered by
///   the FIRST submit and has either been answered already or will never
///   appear; re-arming that watcher would put a stray Enter into a pane
///   whose state we are trying to recover.
/// - `fresh_kickoff: false` — this is what bounds the whole feature at ONE
///   recovery. A re-delivery that is itself eaten degrades to the loud
///   pre-#517 badge instead of triggering another recovery, so there is no
///   re-send loop even if every budget check were removed.
///
/// `None` when the re-delivery did NOT land alone at the front of the queue:
/// kickoff treatment belongs to whichever entry the drainer's first pass
/// actually picks up, and mirroring `deliver_prompt`'s own `was_first` rule
/// keeps that decision in one place rather than relying on
/// `FreshFirstAttempt`'s id guard to catch a mismatch after the fact.
pub fn redelivery_treatment(was_first: bool) -> Option<KickoffTreatment> {
    was_first.then_some(KickoffTreatment {
        wait_ready: true,
        confirm_autopilot: false,
        fresh_kickoff: false,
    })
}

/// The kickoff treatment a delivery HELD THROUGH A PAUSE runs under when
/// `flush_paused_queues` finally starts a drainer for its pane (#620).
///
/// **The finding.** Nothing guards `spawn_agent` against a paused group, and
/// since #569 a spawn during a pause routes its `Delivery::FreshKickoff`
/// through `deliver_prompt`'s pause branch like any other delivery. The
/// resume then flushed it with `None` — `wait_ready: false`,
/// `confirm_autopilot: false`, `fresh_kickoff: false` — because the queue
/// entry carried no kind. For a copilot pane under `--autopilot` that is a
/// wedge rather than a timing nit: per `confirm_copilot_autopilot_dialog`'s
/// own note the consent dialog appears AFTER the kickoff Enter, so nobody
/// dismisses it, and #517's late-kickoff recovery is unarmed because the
/// drainer was never told this was a kickoff.
///
/// So, unlike `redelivery_treatment`, this copies the ORIGINAL delivery's own
/// flags rather than a narrowed set — the pause changed when the brief lands,
/// not what it is. `confirm_autopilot` is the one flag that cannot come from
/// the kind alone (it also depends on the group's CLI and posture); the caller
/// passes `should_confirm_copilot_autopilot`'s verdict, computed exactly as
/// `deliver_prompt`'s front door computes it.
///
/// `None` for `MidSession`, which is every ordinary held prompt and needs no
/// treatment at all — the same `None` this path has always passed, now said
/// rather than assumed.
pub fn paused_flush_treatment(kind: Delivery, confirm_autopilot: bool) -> Option<KickoffTreatment> {
    (kind != Delivery::MidSession).then_some(KickoffTreatment {
        wait_ready: kind.wait_ready(),
        confirm_autopilot,
        fresh_kickoff: kind.recovers_lost_kickoff(),
    })
}

/// The three flags `redelivery_treatment` and `paused_flush_treatment` decide,
/// named rather than a bare `(bool, bool, bool)`: they are the same type and
/// mean opposite things, so a positional tuple is one transposed edit away
/// from arming the autopilot watcher on a recovery, or making a re-delivery
/// recoverable and unbounding the feature. The public mirror of the private
/// `FreshFirstAttempt` fields they populate.
///
/// #620 renamed this from `RedeliveryTreatment`: it is no longer the shape of
/// one producer's answer, and each producer's own doc — not this struct's —
/// is where the values it chooses are argued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KickoffTreatment {
    /// Hold the paste until the CLI has painted.
    pub wait_ready: bool,
    /// Watch for copilot's autopilot-consent dialog (#101/#364).
    pub confirm_autopilot: bool,
    /// Whether THIS delivery may itself be re-delivered by the late monitor
    /// if it turns out never to have landed (#517).
    pub fresh_kickoff: bool,
}

/// Whether the late monitor's live re-check should RE-WORD a badge that
/// currently reads "loomux is re-sending it" (`blocker: None`), and to what.
/// `None` = leave the in-flight wording alone.
///
/// Two reasons to leave it alone, and they are different facts:
/// - `Exhausted` (#496 rev-47 NB1) — the budget arm firing on our OWN
///   already-queued marker, which IS the "still re-sending" state the badge
///   already shows. Only a REAL blocker is worth re-wording for.
/// - `NotHolding` while a lost-kickoff re-delivery is in flight (#517,
///   review F2) — "its text is gone — check the pane" is *true* here and
///   still wrong to say: the text is gone precisely because the paste was
///   eaten, which is why a re-delivery is queued. Without this arm the badge
///   flips back to telling the human to act on a pane loomux is actively
///   recovering, on the very next 5s tick, for as long as the re-delivery
///   waits behind a busy drain or an occupied box. It self-corrects on the
///   re-delivery's confirm or supersede, but a badge that says "your problem"
///   about loomux's own in-flight work is exactly the honesty failure the
///   NB1 re-check exists to prevent, pointed the other way.
///
/// Every other blocker re-words as before: `HumanInput` and `Question` are
/// real, human-clearable states that outrank an in-flight recovery, and
/// `QueueFull` means nothing was queued at all.
///
/// #559 adds `Unverifiable` to the second arm for the identical reason. It is
/// the same delivery shape — a paste with no confirming trace, which is why a
/// re-delivery was queued — reached because the paste was too large to verify
/// rather than because it was verifiably gone. Telling the human to go check a
/// pane loomux is actively recovering is the same honesty failure whichever of
/// the two readings produced it.
pub fn stranded_reword(live: StrandedBlocker, redelivery_in_flight: bool) -> Option<StrandedBlocker> {
    match live {
        StrandedBlocker::Exhausted => None,
        StrandedBlocker::NotHolding | StrandedBlocker::Unverifiable if redelivery_in_flight => None,
        other => Some(other),
    }
}

/// What one live box reading is allowed to do to a badge that is already up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeRelease {
    /// Take the chip down, with this `clear_stranded` reason.
    Clear(&'static str),
    /// Keep the chip up and say something truer about it. Never an insert —
    /// see [`OrchRegistry::reword_stranded`].
    Reword(StrandedBlocker),
    /// This reading establishes nothing that changes the chip.
    Keep,
}

/// The badge-honesty matrix (#825 M2): given a raised badge and one live
/// reading of the pane, may the chip come down, and if not, does it still say
/// the right thing?
///
/// # Why this is a function and not two pieces of inline logic
///
/// The check itself is not new. `run_late_confirmation_monitor`'s `KeepWaiting`
/// arm has kept a raised badge honest every `LATE_MONITOR_POLL` since #496
/// PR-C — but only for as long as that monitor lives, which is
/// `LATE_MONITOR_MAX_LIFETIME` (four hours) at the outside and usually far
/// less. So the badge latch has never really been two regimes of *logic*, only
/// two regimes of *observation*: while a monitor is alive the chip
/// self-corrects, and after it exits nothing looks at the pane again, ever.
/// That is the whole of #825's "indefinite latch" for the three classes below.
///
/// The fix is therefore a hoist, not a second opinion. This function is the
/// decision, extracted; the monitor's arm and `OrchRegistry::stranded_janitor_
/// pass` are two *observers* that ask it the same question. Two honesty checks
/// that could drift is the failure mode a copy would have shipped — the one
/// where a chip clears under a live monitor and re-latches an hour later, or
/// the reverse, and no reader could say which was right.
///
/// # What each caller can and cannot see
///
/// `saw_text_in_box` is the one input the two observers genuinely differ on,
/// and it is a fact about the *observer*, not the pane. The monitor watches one
/// delivery continuously, so it can witness a genuine present→absent
/// TRANSITION: our text was in that box, and now it is not, so whoever pressed
/// Enter, the pane is no longer wedged. The janitor arrives with no such
/// memory — it starts, by construction, on panes whose monitor is already gone
/// — so it can never pass anything but `false` here, and needs strictly
/// stronger evidence to clear on. Hence the two clear arms.
///
/// # The evidence bar for a clear without a transition
///
/// Exactly #819's `HumanResolved` bar, extended from the queued-marker path to
/// badge-only panes: a positive [`BoxReading::NotHolding`] on the text the
/// ledger still records **and** a human keystroke on this pane since our own
/// submit. Both halves are load-bearing.
///
/// - `NotHolding` alone is [`StrandedRetireReason::TextGone`], which #819
///   deliberately declined to clear on: our text left the box with nobody at
///   the keyboard is precisely [`StrandedBlocker::NotHolding`]'s own sentence
///   ("never confirmed and its text is gone — check the pane"). Clearing there
///   would answer a question loomux cannot answer.
/// - The keystroke alone is #518's phantom — a terminal auto-reply
///   misclassified as a person — and is why the stamp only ever *names* a
///   release that a box reading has already licensed. It never licenses one.
///
/// Requiring both also contains the two residuals this inherits rather than
/// absorbing them silently:
///
/// 1. **#828's remaining false `NotHolding`** (a hard mid-word wrap). A lone
///    false reading only ever re-words here; it takes two independent failures
///    — a misread box AND a phantom keystroke — to reach a clear, and that
///    conjunction is the same one #819 already accepts on the marker path.
/// 2. **#518's phantom stamp.** Same bar, same path, same acceptance. A
///    phantom over a genuinely-gone text is a clear on `TextGone`, which is why
///    nothing below may clear on `NotHolding` alone.
///
/// # Reading the matrix
///
/// Nothing but a positive `NotHolding` establishes anything at all. `Holds`
/// (our text is demonstrably still sitting there), `Unverifiable` (we looked
/// and could not tell) and `None` (the ledger records no text to look for) are
/// three different facts, and all three keep the chip: this is the direction
/// where being wrong hides a prompt nobody will re-send.
///
/// Pure and total, so every cell is directly assertable.
#[doc(hidden)] // pub for integration tests
pub fn stranded_badge_release(
    // The chip that is currently up. `None` is the in-flight-heal wording
    // ("loomux is re-sending it"), which is not this matrix's business — the
    // monitor re-derives that one live from `stranded_selfheal_action`.
    blocker: Option<StrandedBlocker>,
    // What Tier 1 says about OUR OWN stranded text, or `None` when there is no
    // recorded text to look for. Kept distinct for the reason
    // `stranded_marker_action` keeps them distinct: "nothing to look for" and
    // "looked and could not tell" are different facts that happen to take the
    // same conservative branch.
    reading: Option<BoxReading>,
    // A human keystroke is on record for this pane since our submit —
    // `tier1_trusted`'s inverse, the same derivation `drain_stranded_submit`
    // takes. Names a release; never licenses one.
    human_stamped_since: bool,
    // The observer WATCHED our text sit in this box earlier in its own life, so
    // a `NotHolding` now is a transition rather than a standing state. Only the
    // late monitor can ever pass `true`; see the header.
    saw_text_in_box: bool,
) -> BadgeRelease {
    // Nothing weaker than a positive absence is evidence. Ordered first so no
    // arm below can be reached on a reading that never happened.
    if reading != Some(BoxReading::NotHolding) {
        return BadgeRelease::Keep;
    }
    if saw_text_in_box {
        // #496 PR-C's own clear, unchanged and still the strongest reading
        // available: present→absent, whoever pressed the Enter. It outranks the
        // keystroke-named clear below because it is about the text rather than
        // about who was at the keyboard, and it holds for EVERY class — a
        // transition unwedges the pane whatever the chip happened to say.
        return BadgeRelease::Clear("text-left-the-box");
    }
    match blocker {
        // The three classes #825 leaves with no release: each one's badge is a
        // claim about where our text is, so each one is answerable by a later
        // reading of the same box. (`QueueFull` is a retry, not a clear — M3.
        // `PauseSuppressed` describes a loss that already happened and can
        // never become untrue, so no pane reading may release it; the human's
        // explicit dismiss is its release, M1. Every hold class already has
        // one.)
        Some(StrandedBlocker::NotHolding)
        | Some(StrandedBlocker::Unverifiable)
        | Some(StrandedBlocker::Exhausted) => {
            if human_stamped_since {
                // The one name in this vocabulary for "a person dealt with it",
                // taken from #819's enum rather than spelled again, so the
                // drainer's retirement and this clear can never come to mean
                // two different things in the audit.
                BadgeRelease::Clear(StrandedRetireReason::HumanResolved.as_str())
            } else if blocker != Some(StrandedBlocker::NotHolding) {
                // The honesty upgrade. `Unverifiable` says "we could not read
                // the box" and `Exhausted` says "a heal fired and did not
                // take, and the text read `Holds`" — both are now stale
                // sentences about a box we CAN read and which does not hold
                // our text. The chip stays up (nobody has shown a human dealt
                // with it) but it stops claiming more than we know.
                BadgeRelease::Reword(StrandedBlocker::NotHolding)
            } else {
                // Already the truest thing we can say.
                BadgeRelease::Keep
            }
        }
        _ => BadgeRelease::Keep,
    }
}

/// Whether a raised chip is worth a janitor pane read at all (#825 M2).
///
/// **Derived from the matrix rather than restated as a list.** The classes the
/// janitor watches are, by definition, exactly the ones where some reading it
/// can actually take would change the chip — so this asks
/// [`stranded_badge_release`] instead of naming `NotHolding` / `Unverifiable` /
/// `Exhausted` a second time. A hand-written list is a second copy of the
/// matrix's domain, and it goes stale the first time a class is added to one
/// and not the other: too narrow silently drops a class from the janitor with
/// nothing red, too wide only costs a read. Neither can happen if there is one
/// list and it is the function.
///
/// `NotHolding` is the only reading that ever produces a verdict (every other
/// arm keeps the chip), and `saw_text_in_box` is always `false` for this
/// observer, so the two keystroke states are the whole space to probe.
pub(in crate::orchestration) fn janitor_watches(blocker: Option<StrandedBlocker>) -> bool {
    [true, false].into_iter().any(|stamped| {
        stranded_badge_release(blocker, Some(BoxReading::NotHolding), stamped, false)
            != BadgeRelease::Keep
    })
}

/// Whether a self-heal may admit its `StrandedSubmit` marker right now
/// (#496 PR-C), and if not, the audited reason. `None` = admit.
///
/// **`drainer-active` is a safety rule, not a nicety.** Pushing to the FRONT
/// of a pane's queue is only safe while no drainer OWNS an entry: a drainer
/// sitting inside `deliver_now` has already peeked its front entry and will,
/// on completion, call `pop_front_dequeued(that entry's id)` — which pops
/// ONLY on an id match. Slip a marker in front of it and that pop matches
/// nothing, leaving an ALREADY-DELIVERED text entry queued for a second,
/// duplicate delivery. Pre-#496 nothing could hit this (the front door
/// pushes to the BACK; the drainer's own `AbortedPreEnter` marker is pushed
/// after it pops), and the self-heal must not become the first thing that
/// does.
///
/// **This predicate is only half the rule; the other half is WHERE it is
/// evaluated.** Deciding from `queue_draining` and then pushing is not
/// enough on its own — a drainer registering between the two re-opens the
/// very hazard above, which is what this PR first shipped and review rev-47
/// B1 caught. The caller (`admit_stranded_selfheal`) must therefore consult
/// this while HOLDING `queues`, and push in that same critical section, so
/// the observation cannot go stale before it is acted on. What makes that
/// sufficient: any drainer that could ever peek this front must register
/// before it peeks (`ensure_drainer`) and must take `queues` TO peek, so it
/// is either already registered when the fused section reads
/// `queue_draining` (→ decline) or it peeks strictly after the push (→ finds
/// the marker at the front, the safe case — it drains the submit first and
/// pops it by a matching id). See `admit_stranded_selfheal`'s doc for the
/// lock order and `queue.rs`'s `stranded_admission_property` for the
/// exhaustive proof, whose unfused variant is the mutation control that
/// keeps the fused one from passing vacuously.
///
/// **Nothing is lost by declining, in either case.** A live drainer means a
/// delivery is already queued for this pane, and THAT delivery's own
/// pre-paste `flush_stranded_text` is the pre-existing mechanism which
/// presses exactly the Enter this heal wanted pressed — the self-heal exists
/// for the case where no such next delivery exists (an idle group), which is
/// precisely when no drainer is running. A marker already at the front is
/// the same submit, already pending, retried by the drainer with no cap.
/// Either way the badge is still raised: the human is told regardless of
/// which mechanism does the pressing.
pub fn stranded_admission_gate(drainer_active: bool, front_is_marker: bool) -> Option<&'static str> {
    if drainer_active {
        return Some("drainer-active");
    }
    if front_is_marker {
        return Some("submit-already-queued");
    }
    None
}

/// Whether a raised [`StrandedBlocker::QueueFull`] chip's refused re-send may be
/// admitted **now** (#825 M3), and if not, the reason it is not yet time.
/// `None` = go.
///
/// `QueueFull` is the one unreleased class whose release is a **retry rather
/// than a reading**. Its badge does not claim anything about where our text is
/// — [`stranded_badge_release`]'s matrix has nothing to say about it — it says
/// loomux could not even *queue* the re-send. The honest answer to that is to
/// queue it once there is room, and let the confirm/retire machinery that owns
/// every other marker own this one too.
///
/// # Why this is not a hook on the drain edge
///
/// plan-312's M3 put the retry at `note_queue_capacity`'s `Full` → not-`Full`
/// transition, beside `announce_refusal_roster` (#658). That edge cannot admit
/// anything, and the reason is a runtime fact a read-only plan could not see:
/// every production caller able to produce it — [`OrchRegistry::pop_front_dequeued`],
/// [`OrchRegistry::pop_batch_dequeued`] and [`OrchRegistry::drop_superseded`] —
/// runs on the drainer thread, which holds its `queue_draining` registration
/// from `ensure_drainer` right through to `commit_exit`. So
/// [`stranded_admission_gate`] would answer `Some("drainer-active")` at every
/// real drain edge, every time, forever. (The one other caller,
/// `OrchRegistry::drop_queue`, is destroying the pane's queue; re-admitting
/// there would be queueing for a pane that is going away.)
///
/// It is also the wrong moment **on the merits**, which is what makes this a
/// relocation rather than a workaround. `stranded_admission_gate`'s own doc
/// says what a live drainer means: a delivery is already queued for this pane,
/// and THAT delivery's pre-paste `flush_stranded_text` presses exactly the
/// Enter this marker wants pressed. The moment nothing else will press it is
/// the moment the queue has gone quiet and the drainer has exited — which is
/// also the only moment the marker can be admitted at all. So M3 observes the
/// pane *after* the drain rather than during it: the same "hoist the work out
/// of the lifetime of the thread that dies" M2 applied to the late monitor,
/// pointed at the drainer instead.
///
/// *Rejected:* teaching the gate that the drainer may admit its own marker
/// between a `pop_front_dequeued` and its next peek. It is true that the
/// drainer owns no entry at that instant — but it re-opens #496 PR-C rev-47
/// B1 by construction, replacing a fused check-and-push with a parameter every
/// future caller of `note_queue_capacity` would have to get right, for a repair
/// the very next queued delivery's pre-paste flush already performs.
///
/// Pure and total, so every cell is directly assertable. The depth check
/// mirrors `push_stranded_front_locked`'s own refusal condition rather than
/// `queue::capacity_state`'s badge classification: this is a pre-check that
/// exists to keep a still-full pane from attempting-and-failing (and auditing a
/// `delivery-dropped` line) on every pass, so it has to ask the same question
/// the push will ask. The real check is still the one inside
/// [`OrchRegistry::admit_stranded_selfheal`], taken under the `queues` lock.
pub fn queuefull_readmit_gate(
    blocker: Option<StrandedBlocker>,
    depth: usize,
    drainer_active: bool,
) -> Option<&'static str> {
    // Only `QueueFull` names a re-send that was REFUSED and never queued. Every
    // other chip is somebody else's release — M1's dismiss, M2's matrix, or the
    // hold and depth classes' own conditions — and the in-flight wording
    // (`None`) is a submit that is already pending.
    if blocker != Some(StrandedBlocker::QueueFull) {
        return Some("not-queue-full");
    }
    // The condition that raised the chip is still true, so the chip is still
    // right and there is nothing to retry.
    if depth >= queue::QUEUE_MAX_PER_PANE {
        return Some("still-full");
    }
    // Same word as `stranded_admission_gate`'s, and the same fact — pushing in
    // front of a drainer that owns the front entry is the rev-47 B1 race. Named
    // here as well so a caller can decline BEFORE attempting, which is what
    // keeps a declined pass silent instead of writing a skip line every 30s.
    if drainer_active {
        return Some("drainer-active");
    }
    None
}

/// One pane's stranded-delivery state (#496 PR-C), held in
/// `OrchRegistry::attn_stranded` and rendered by `attention_tick`. Only the
/// facts are stored; the human-facing wording is built at render time by
/// `stranded_detail`, so the badge text lives in exactly one place next to
/// every other attention reason's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrandedNote {
    /// `None` — a self-heal is in flight for this pane. `Some(blocker)` —
    /// loomux cannot act and the human must.
    pub blocker: Option<StrandedBlocker>,
    /// When the delivery was declared stranded (Unix-ms).
    pub since_ms: u64,
}

/// #569: whether the resume-time suppression notice's FALLBACK badge fires
/// for one pane that lost deliveries to a pause.
///
/// Pure, and separate from `announce_pause_suppression`, because the branch
/// that matters most cannot be reached in a headless integration test: with no
/// `AppHandle`, `deliver_prompt` always ends in `Err("no app handle")` after
/// admitting, so "the notice landed, therefore no badge" is unobservable there
/// (the same limitation `delivered_texts`' doc in the test file describes from
/// the other side). Extracting the decision makes all three rules assertable
/// directly, which is the only way "delivered ⇒ never badge" gets tested at
/// all rather than asserted in a comment.
///
/// - `notice_delivered` — the orchestrator has the full list, so a chip on
///   every pane that missed something would be noise on top of a channel that
///   already worked.
/// - `target_alive` — a dead pane's badge is a chip nobody can act on.
/// - `existing` — never stomp another mechanism's badge (`note_queue_capacity`
///   follows the same rule): whatever raised it is telling the human to look at
///   this same pane, and a *held* pane's badge is the more urgent claim because
///   it names something still fixable. An in-flight heal (`Some(note)` whose
///   own `blocker` is `None`) counts as somebody else's badge for the same
///   reason. Our OWN badge is re-stamped rather than skipped, so a second pause
///   that loses more does not go quiet.
pub fn pause_badge_decision(
    notice_delivered: bool,
    target_alive: bool,
    existing: Option<StrandedNote>,
) -> bool {
    if notice_delivered || !target_alive {
        return false;
    }
    match existing {
        None => true,
        Some(n) => matches!(n.blocker, Some(StrandedBlocker::PauseSuppressed)),
    }
}

/// The badge/tooltip wording for a stranded pane (#496 PR-C) — pure, and
/// deliberately phrased as what the HUMAN should do, since the entire point
/// of the badge is that a wedged pane is never discovered by accident.
pub fn stranded_detail(name: &str, blocker: Option<StrandedBlocker>) -> String {
    match blocker {
        None => format!("{name}'s prompt was never submitted — loomux is re-sending it"),
        Some(StrandedBlocker::HumanInput) => {
            format!("{name}'s prompt is stuck behind text you typed — press Enter or clear the box")
        }
        Some(StrandedBlocker::Question) => {
            format!("{name}'s prompt is stuck behind a question on screen — answer it")
        }
        Some(StrandedBlocker::NotHolding) => {
            format!("{name}'s prompt was never confirmed and its text is gone — check the pane")
        }
        // #559: names the uncertainty rather than resolving it, and gives an
        // action that is safe under either branch — if the prompt is sitting
        // there, Enter sends it; if it is not, the human has looked and lost
        // nothing. Never says the text is gone: loomux does not know that.
        Some(StrandedBlocker::Unverifiable) => {
            format!(
                "{name}'s prompt was never confirmed and is too large for loomux to verify in the \
                 pane — check the pane and press Enter if it is still sitting there unsent"
            )
        }
        Some(StrandedBlocker::Exhausted) => {
            format!("{name}'s prompt is still unsubmitted after a self-heal — press Enter in the pane")
        }
        Some(StrandedBlocker::QueueFull) => {
            format!("{name}'s pane has a full delivery queue, so loomux could not even queue a re-send — press Enter in the pane")
        }
        // #563: says what is true NOW (nothing lost) and what happens if the
        // pane is left alone (deliveries start being dropped). Deliberately
        // does not claim a re-send failed — that is `QueueFull`'s sentence,
        // and it would be false here.
        //
        // rev-10 finding 1: nor does it claim the pane is HELD.
        // `note_queue_capacity` raises this badge on depth alone and never
        // reads any hold state, so a pane whose senders simply outrun a healthy
        // drainer lands here with nothing to release. The hold is offered as a
        // condition for the human to check — "if the pane is held" — because
        // that is the shape of what loomux actually knows. See
        // `queue::pressure_notice`'s doc for the full argument.
        Some(StrandedBlocker::QueueNearFull) => {
            format!(
                "{name}'s delivery queue is nearly full and still filling — deliveries are \
                 arriving faster than that pane accepts them. If it is held (unsubmitted text, \
                 or a question on screen), releasing it drains the backlog; at the cap further \
                 deliveries are dropped"
            )
        }
        // #563 / rev-10 finding 1: the depth-derived counterpart to
        // `QueueFull`. States the consequence (arrivals are being dropped) and
        // the same conditional check, and claims nothing about a re-send that
        // this badge's caller never attempted.
        Some(StrandedBlocker::QueueAtCapacity) => {
            format!(
                "{name}'s delivery queue is FULL — further deliveries to that pane are being \
                 DROPPED, not queued. If it is held (unsubmitted text, or a question on screen), \
                 releasing it drains the backlog"
            )
        }
        // #532: this wording must stay honest about WHICH state loomux is in,
        // because it does not know. It names both branches and gives an action
        // that is safe under either — answering a real question, or typing and
        // deleting a character, which both moves the pane's output past the
        // stale bytes and leaves the box empty again.
        Some(StrandedBlocker::QuestionStale) => {
            format!(
                "{name}'s prompt has been held for minutes on a question loomux still detects in this pane — \
                 answer it if one is on screen; if the pane looks clear, that reading is stale: type a \
                 character and delete it to release the hold"
            )
        }
        // #569: past tense, and no action for the PANE — there is nothing to
        // release here. What was addressed to this pane is gone, and the only
        // repair is upstream: whoever sent it has to send it again. Says so,
        // and claims nothing about the pane's own state (see the variant's doc).
        Some(StrandedBlocker::PauseSuppressed) => {
            format!(
                "{name} was sent deliveries that were DISCARDED while this group was paused, and \
                 loomux could not reach the orchestrator to say so — nothing is queued for that \
                 pane. Whatever it is waiting on has to be sent again"
            )
        }
    }
}

/// Why a queued `StrandedSubmit` marker was retired instead of pressed
/// (#813). Each variant is a distinct audit token, because "there was nothing
/// left to submit", "a human submitted it for us" and "our text left the box
/// with nobody at the keyboard" are three different facts about the same pane,
/// and a shared string could not tell a reader which one happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrandedRetireReason {
    /// The pane's delivery ledger no longer says anything is stranded — the
    /// last delivery confirmed, or there is no record at all (a restart drops
    /// the in-memory ledger while the queue is persisted). Pressing Enter
    /// against that is a blind press with nothing behind it.
    NothingStranded,
    /// #813's incident cell. Our stranded text is verifiably **gone from the
    /// box**, and a human keystroke landed on this pane after our own submit —
    /// so a person dealt with it, which is exactly what the queue was waiting
    /// for and exactly what it could not see.
    HumanResolved,
    /// Our stranded text is verifiably gone from the box, and **no** human
    /// keystroke is on record since our submit — the CLI consumed it, replaced
    /// it with a placeholder, or it never really landed. Nothing left to press
    /// Enter against either way, but deliberately NOT `HumanResolved`: that
    /// name would put a person in the audit trail who was never there, and it
    /// is the one of these two that licenses taking the badge down.
    TextGone,
}

impl StrandedRetireReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StrandedRetireReason::NothingStranded => "nothing-stranded",
            StrandedRetireReason::HumanResolved => "human-resolved",
            StrandedRetireReason::TextGone => "text-gone",
        }
    }

    /// Whether this retirement is evidence that the pane no longer needs a
    /// human, i.e. whether it takes the `attn_stranded` chip down.
    ///
    /// Only `HumanResolved`. `TextGone` says our text left the box with nobody
    /// at the keyboard, which is precisely [`StrandedBlocker::NotHolding`]'s
    /// situation — "never confirmed and its text is gone — check the pane" —
    /// and clearing on it would answer a question loomux cannot answer.
    /// `NothingStranded` establishes nothing about the pane at all.
    pub fn resolves_the_pane(self) -> bool {
        matches!(self, StrandedRetireReason::HumanResolved)
    }
}

/// What the drainer should do with the `StrandedSubmit` marker at the front of
/// a pane's queue (#813) — the precedence as a VALUE, for the reason
/// `failed_arm_route` gives for the same shape: a `return`-ordered version of
/// this lives only as control flow inside a function no test in this repo can
/// construct, and #585 is the precedent for that being how a wrong ordering
/// survives review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrandedMarkerAction {
    /// Press the Enter the stranded delivery never got.
    Press,
    /// Drop the marker and let the queue move on.
    Retire(StrandedRetireReason),
    /// A live gate is in the way, or our text is still sitting there and
    /// cannot be pressed yet — leave the marker queued and retry, carrying the
    /// gate that actually declined so the caller's notice and audit name it
    /// (the mislabel #532 rev-12 NB1 closed on `AbortedPreEnter`, arriving
    /// here).
    Retry(queue::EnqueueReason),
}

/// **A marker is a repair, not a payload** — and that is the whole argument for
/// #813 (see `docs/design/orchestration.md`).
///
/// Before this, a marker that could not fire stayed at the FRONT of the pane's
/// queue and was retried "next tick, no cap, exactly like every other queued
/// entry". But it is not like every other queued entry: every other one carries
/// text that exists nowhere else, whereas a marker carries an **Enter**. So the
/// marker was the one queue entry whose failure to fire cost *other* work, and
/// nothing bounded it.
///
/// Worse, its release condition was ANTI-CORRELATED with the human's own
/// recovery. `human_input_block` re-arms for `HUMAN_INPUT_BLOCK_BOUND_MS` from
/// the human's LAST keystroke, so a human who does the sane thing — click into
/// the wedged pane, press Enter to submit the stranded prompt, then keep typing
/// to talk to the CLI — pins the marker at the head of the queue for as long as
/// they stay engaged, and every steering prompt behind it silently never
/// delivers. That is #813's live incident, step for step.
///
/// # The evidence a retirement rests on, and why it is about the TEXT
///
/// The first cut of this fix retired on "a keystroke landed after our submit
/// **and** `input_pending` is false", and that was wrong twice over.
///
/// **It could not tell a person from a phantom.** #518 exists because a
/// terminal auto-reply can be misclassified as a keystroke, and that is exactly
/// the shape this reading has: a stamp, an empty occupancy counter. Retiring on
/// it fired inside `HUMAN_INPUT_BLOCK_BOUND_MS`, i.e. strictly before
/// `BoundedOut` — which is only reachable with `!box_pending` — could ever be
/// reached, so it did not merely coexist with #518's bound, it made that bound
/// **unreachable from this path**, and wrote `human-resolved` into the audit
/// trail for a pane no human had touched.
///
/// **And `!box_pending` is not "the box is empty".** `input_box_len` counts
/// characters the HUMAN typed; loomux's own paste never touches it. So the
/// first cut retired while our own prompt could still be sitting in that box —
/// and the next queue entry, whose `deliver_now` does NOT abort when
/// `flush_stranded_text` declines, would then paste **on top of it** and submit
/// both prompts merged as one. That is the #81/#84/#111 collision the stranded
/// flush exists to prevent, re-opened by the repair meant to help.
///
/// Both hazards have one root — reasoning about *who typed* instead of *where
/// our text is* — so both close with one change. A retirement now requires a
/// positive [`BoxReading::NotHolding`]: the tail was long enough to have
/// contained our paste and does not. Then, and only then, the keystroke record
/// picks the NAME (`HumanResolved` vs `TextGone`), which is all it was ever fit
/// to decide.
///
/// `Holds`, `Unverifiable` and "no text on record" all fall through to the
/// ordinary gates. Nothing retires on an absence of evidence — the one
/// direction that could re-open either hazard.
///
/// # What retiring cannot lose
///
/// A retirement means our text is not in the box. There is therefore no Enter
/// left for this marker to press that could submit it, and nothing for a later
/// paste to collide with. The case where our Enter WOULD still have been right
/// is precisely `Holds`, and that is the case this function keeps retrying.
///
/// Pure and total so every cell of the matrix is directly assertable.
#[doc(hidden)] // pub for integration tests
pub fn stranded_marker_action(
    prev_confirmed: Option<bool>,
    // What Tier 1 says about OUR OWN stranded text, or `None` when the ledger
    // carries no text to look for. `None` and `Unverifiable` are different
    // facts ("we have nothing to look for" vs "we looked and could not tell")
    // and both take the same conservative branch, which is why neither is
    // collapsed into the other.
    text_reading: Option<BoxReading>,
    // Whether a human keystroke is on record for this pane since our own
    // submit — `tier1_trusted`'s inverse. Names a retirement; never licenses
    // one.
    human_stamped_since: bool,
    human_block: HumanInputBlock,
    box_pending: bool,
    // A CLOSURE, not a bool, for the reason `question_hold_predicate_sampled`
    // takes a sampler: answering it costs a 64 KiB grid recomposition, and most
    // of the decisions below never need to ask. Called at most once, and only
    // on the path that actually consults it.
    question_active: impl FnOnce() -> bool,
) -> StrandedMarkerAction {
    // Nothing stranded to submit. `should_flush_before_paste`'s own condition,
    // read as a retire rather than as a decline: a decline here retried forever
    // against a ledger that can never say `Some(false)` again.
    if !matches!(prev_confirmed, Some(false)) {
        return StrandedMarkerAction::Retire(StrandedRetireReason::NothingStranded);
    }
    // The one positive reading that licenses a retirement: our text is gone
    // from the box. See the header for why nothing weaker will do.
    if text_reading == Some(BoxReading::NotHolding) {
        return StrandedMarkerAction::Retire(if human_stamped_since {
            StrandedRetireReason::HumanResolved
        } else {
            StrandedRetireReason::TextGone
        });
    }
    // #510's absolute, unchanged: human-typed characters are outstanding in the
    // box, so never press Enter over them.
    if box_pending {
        return StrandedMarkerAction::Retry(queue::EnqueueReason::BoxOccupied);
    }
    // #518, unchanged and now genuinely reachable: a human typed here since our
    // submit, so hold — but only until the bound, after which `holds()` reads
    // false and this falls through to the press. That bound is the ONLY thing
    // that releases a stamp which may have been a phantom, which is why nothing
    // above may retire on the stamp alone.
    //
    // `BoxOccupied` is the honest reason: the box is occupied — by OUR text,
    // which is the entire premise of a marker existing. Whose text it is lives
    // in the `stranded-marker-*` audit, not in the queue's reason vocabulary.
    if human_block.holds() {
        return StrandedMarkerAction::Retry(queue::EnqueueReason::BoxOccupied);
    }
    // #420, unchanged: a live question owns the Enter key.
    if question_active() {
        return StrandedMarkerAction::Retry(queue::EnqueueReason::Question);
    }
    StrandedMarkerAction::Press
}

/// Replay a queued `StrandedSubmit` marker (#445 seam 3): the text is already
/// sitting in the box from an earlier paste whose Enter was withheld — press it
/// via the SAME `flush_stranded_text` logic a normal delivery's own pre-paste
/// step already uses (guarded by `human_typed_since`, so a person's own line is
/// never blind-submitted).
///
/// #813: returns `stranded_marker_action`'s three-way rather than a bool. A
/// `Retry` is the old `false` (leave it queued); a `Retire` is new and is the
/// fix — see that function for the evidence a retirement rests on and why it
/// has to be about our text rather than about who typed.
#[doc(hidden)] // pub for integration tests (#496 PR-C drives the REAL replay)
pub fn drain_stranded_submit(
    ptys: &crate::pty::PtyManager,
    last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>,
    delivery_from: String,
    pty_id: u32,
    submit: &[u8],
    // #576: threaded through to `flush_stranded_text`'s question gate — see
    // there for why this reader gets the record too.
    delivered: Vec<String>,
) -> StrandedMarkerAction {
    let prev = last_delivery.lock_safe().get(&pty_id).cloned();
    let prev_confirmed = prev.as_ref().map(|o| o.confirmed);
    // #518: the SAME derivation the self-heal's trigger used
    // (`human_input_block`), not a second opinion. rev-47 NB1 is the failure
    // this avoids: a marker admitted on one rule and pressed under another
    // never fires, and the badge quietly goes on claiming loomux is handling a
    // re-send it will decline forever.
    let human_block = prev
        .as_ref()
        .map(|o| human_input_block_now(ptys, pty_id, o.submit_sent_ms))
        .unwrap_or(HumanInputBlock::None);
    // The same `tier1_trusted` reading `human_input_block` takes, kept separate
    // because it answers a different question: the block decides whether to
    // WRITE, this decides what to CALL a retirement that has already been
    // licensed by the box reading below.
    let human_stamped_since = prev.as_ref().is_some_and(|o| {
        !tier1_trusted(ptys.last_user_input_ms(pty_id).unwrap_or(0), o.submit_sent_ms)
    });
    // #532: taken HERE, at the press, never inherited. `unwrap_or(true)` on a
    // closed pty is the fail-safe direction, matching `flush_stranded_text`.
    let box_pending = ptys.input_pending(pty_id).unwrap_or(true);
    // #813: Tier 1, on our OWN text — the same `Tier1Scan` widening read and
    // the same `box_reading` the late-confirmation monitor takes, not a second
    // implementation of either.
    //
    // **Cost, declared (performance.md §3 INV-4: cadenced work says what it
    // costs).** This runs on the drainer's `QUEUE_DRAIN_POLL` (2 s), so it is
    // fixed-cadence work and owes a bound. Three things bound it:
    //
    // 1. **Scope.** It runs only while a `StrandedSubmit` marker is at the
    //    FRONT of a pane's queue — not per pane, not per poll of an ordinary
    //    queue, and not at all on a pane that has never stranded a delivery.
    //    A marker is rare and self-limiting: this very reading is what retires
    //    it, so the work ends the condition that schedules it.
    // 2. **Per call.** One `Tier1Scan::for_paste` normalize of the recorded
    //    paste, then at most `TIER1_SCAN_WIDEN_ROUNDS` ring reads capped at
    //    `TIER1_SCAN_WIDEN_MAX_BYTES` — `Tier1Scan::widen`'s own bound, not a
    //    fresh one. No IPC and no lock beyond the ring's own.
    // 3. **Precedent.** `run_late_confirmation_monitor` takes the identical
    //    read every `LATE_MONITOR_POLL` (5 s) for the whole of a delivery's
    //    unconfirmed life, so this is a shape the app already pays at a
    //    comparable cadence, on the same panes, for the same question.
    //
    // Deliberately gated on the ledger rather than taken unconditionally: a
    // marker whose pane reports anything but `Some(false)` retires on that
    // alone (see `stranded_marker_action`'s first cell), so paying for a box
    // read there would be buying an answer nothing consults. Not hoisted out
    // of the poll the way the late monitor hoists its `Tier1Scan` — this
    // function is called fresh per pass and owns no state between them, and
    // #559 rev-13 N2's hoist is available to a future caller that does.
    let text_reading = matches!(prev_confirmed, Some(false))
        .then(|| prev.as_ref().and_then(|o| o.stranded_text.clone()))
        .flatten()
        .map(|text| {
            let mut scan = Tier1Scan::for_paste(&text);
            let read = scan.read(|n| ptys.output_tail_bounded(pty_id, n));
            box_reading(read.as_ref().map(|r| r.stripped.as_str()), &text)
        });
    let action = stranded_marker_action(
        prev_confirmed,
        text_reading,
        human_stamped_since,
        human_block,
        box_pending,
        // #576: the record reaches BOTH readers of the question gate — this one
        // and `flush_stranded_text`'s own. Fixing one and leaving the other was
        // rev-126's finding (`e7`).
        || question_active_now(ptys, pty_id, None, delivered.clone()),
    );
    if action != StrandedMarkerAction::Press {
        return action;
    }
    // The press itself still goes through `flush_stranded_text`, which
    // re-derives its own gates: this function decides, that one writes, and
    // neither trusts the other's reading.
    if flush_stranded_text(ptys, pty_id, prev_confirmed, human_block.holds(), submit, delivered) {
        last_delivery.lock_safe().insert(
            pty_id,
            DeliveryOutcome {
                confirmed: true,
                submit_sent_ms: now_ms(),
                from: delivery_from,
                // #813: pressed, so nothing is stranded any more.
                stranded_text: None,
            },
        );
        StrandedMarkerAction::Press
    } else {
        // A decline AFTER we decided `Press` means a gate flipped between the
        // two reads (or the write itself failed on a pty that has since
        // closed). Re-read the cheap half rather than asserting a reason:
        // `flush_stranded_text` consults exactly two gates, so a box that is
        // now occupied names itself, and the question is the only thing left it
        // could have been. That inference is stated rather than hardcoded —
        // the hardcoded `Question` here was itself the mislabel this PR removes
        // everywhere else.
        StrandedMarkerAction::Retry(if ptys.input_pending(pty_id).unwrap_or(true) {
            queue::EnqueueReason::BoxOccupied
        } else {
            queue::EnqueueReason::Question
        })
    }
}

/// Whether to flush a previous delivery's stranded text (a single submit press)
/// before pasting the next prompt (#81/#84).
///
/// Flush only on the exact stranded-text signature: the previous delivery to
/// this pane was NOT confirmed as submitted, AND no human has typed into the
/// pane since (so the box holds the earlier *agent* prompt, not a person's
/// half-written line — which must never be blind-submitted). Never flushes on
/// the first delivery to a pane (`prev_confirmed == None`) or after a confirmed
/// one. A false "unconfirmed" here is safe: the flush Enter lands on an already
/// empty box and is a no-op.
pub fn should_flush_before_paste(prev_confirmed: Option<bool>, human_typed_since: bool) -> bool {
    matches!(prev_confirmed, Some(false)) && !human_typed_since
}

/// Whether the stranded-text flush should ACTUALLY press its Enter right now
/// (#420 rev-15 B1) — `should_flush_before_paste`'s decision, additionally
/// gated on there being no live interactive question on screen. The flush's
/// Enter is the FIRST write `deliver_prompt` makes; without this gate it fires
/// unconditionally, before the interactive-question checkpoint that follows
/// it ever runs — so a question already on screen from BEFORE this delivery
/// even started would eat that Enter and select whatever's highlighted, on
/// the exact path this guard exists to close. A named function (not an inline
/// `&&` at the call site) so the combination is independently testable and
/// can't be silently dropped by a future edit to either input.
///
/// **`box_pending` (#532) — the #510 rule, read STRUCTURALLY at the press.**
/// `human_typed_since` is a TIMESTAMP compare (`last_user_input_ms >
/// submit_sent_ms`), and a timestamp answers "did a keystroke land after our
/// own submit", which is a strictly narrower question than "is there human
/// content in this box right now". The gap is reachable and was live in #532:
/// a human who typed a line and left it sitting BEFORE our submit stamps
/// `last_user_input_ms` at or before `submit_sent_ms`, so `human_typed_since`
/// reads FALSE and this gate used to fire an Enter that submitted their line.
/// `input_pending` (#111/#171 — the per-pane occupancy counter, moved by writes
/// arriving through `write_pty`/`note_user_input`, never by loomux's own
/// `write_bytes`) is a **much closer** reading of the box than a keystroke
/// timestamp, so it is consulted here rather than inferred.
///
/// It is not, however, the box itself, and rev-12 was right to flag an earlier
/// version of this doc for saying so. It is a running count that
/// `classify_human_input` zeroes on only `\r`/`\n`, Ctrl-U and Ctrl-C, so it
/// has a reachable **stuck-true** mode — see [`hold_bound_elapsed`], which is
/// deliberately not allowed to consult it for exactly that reason. Stuck-true
/// is the safe direction *here* (a withheld Enter, recoverable) and the unsafe
/// direction *there* (a suppressed escalation, not), which is why the same
/// signal is trusted at this gate and refused at that one.
///
/// It does NOT suppress the legitimate case this flush exists for: our own
/// stranded paste never moves that counter.
pub fn should_flush_before_paste_now(
    prev_confirmed: Option<bool>,
    human_typed_since: bool,
    question_active: bool,
    box_pending: bool,
) -> bool {
    should_flush_before_paste(prev_confirmed, human_typed_since)
        && !question_active
        && !box_pending
}

/// What `deliver_now` does about a stranded-text flush that **declined** (#824).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrandedPasteGuard {
    /// Nothing of ours is provably in the box — paste, exactly as before.
    Paste,
    /// Our own prompt is still sitting in that box and the flush could not
    /// clear it. Do not paste on top of it.
    AbortStranded,
}

/// #824: the missing half of `flush_stranded_text`'s contract.
///
/// The flush is the mechanism that clears a previous delivery's stranded text
/// before this one pastes. It can DECLINE — `should_flush_before_paste_now`
/// returns false whenever a human typed since our submit, a question is live,
/// or human characters are outstanding — and `deliver_now` used to ignore that,
/// carrying straight on to the paste.
///
/// **Why no existing guard catches it.** The pre-paste guard between the flush
/// and the paste is `wait_for_box_clear`, which reads
/// `PtyManager::input_pending`, i.e. `input_box_len`. That counter is written by
/// `note_user_input` and by nothing else, and `note_user_input` is reached from
/// exactly one place — `write_from_frontend`. Orchestration's own typing goes
/// out through `write_bytes`, which touches no counter at all. So
/// `!input_pending` means "no HUMAN characters are outstanding", **never** "the
/// box is empty", and our own pasted prompt is invisible to every guard on this
/// path. The sequence that follows is ordinary rather than exotic: a delivery
/// strands, a human presses Enter (or types a character and backspaces it out),
/// the flush declines on the human block, `input_pending` reads false, and the
/// next delivery pastes ON TOP of the stranded prompt — which the pre-Enter
/// quiet wait then submits as one merged prompt. That is the #81/#84/#111
/// collision the flush exists to prevent, reached through the flush's own
/// decline.
///
/// **Gated on a POSITIVE reading, never on the decline.** `Holds` is the only
/// answer that aborts. `Unverifiable` keeps today's behaviour deliberately: it
/// is common near the Tier 1 scan cap (#583/#685's census), and aborting on it
/// would hold ordinary traffic for a reading that says nothing — trading a rare
/// merge for a routine stall. `NotHolding` means our text is gone, which is
/// exactly when pasting is safe. So this can only ever abort a delivery loomux
/// can SEE the collision coming for, which bounds the blast radius to the
/// evidence.
///
/// **#828 is what makes that reading load-bearing.** Before it,
/// `box_holds_paste` could answer a confident `NotHolding` for text that was on
/// screen behind per-row gutter decoration — so this guard would have been
/// silently absent precisely under the copilot rendering it is for. Post-#828
/// the reading is a structural superset whose safe answer is `Unverifiable`,
/// which is the branch that keeps today's behaviour here.
///
/// Pure and total, so every cell is directly assertable — `deliver_now` needs a
/// concrete `Wry` `AppHandle` and is unreachable by every test in this repo,
/// which is the same reason `preenter_admission` and `flush_stranded_text` were
/// extracted rather than left inline.
#[doc(hidden)] // pub for integration tests
pub fn stranded_paste_guard(
    prev_confirmed: Option<bool>,
    // Whether `flush_stranded_text` actually pressed. `true` means the box was
    // cleared by our own Enter, so there is nothing left to collide with.
    flushed: bool,
    // Tier 1 on the PREVIOUS delivery's own recorded text, or `None` when the
    // ledger carries none to look for.
    text_reading: Option<BoxReading>,
) -> StrandedPasteGuard {
    if flushed {
        return StrandedPasteGuard::Paste;
    }
    if !matches!(prev_confirmed, Some(false)) {
        return StrandedPasteGuard::Paste;
    }
    if text_reading == Some(BoxReading::Holds) {
        return StrandedPasteGuard::AbortStranded;
    }
    StrandedPasteGuard::Paste
}

/// The stranded-text flush STEP (#81/#84, #420 rev-15 B1) — decides via
/// `should_flush_before_paste_now` (reading the pane's live question state
/// through `question_active_now`) and, if it says to, presses `submit`.
/// Extracted out of `deliver_prompt`'s body specifically so an integration
/// test can drive this EXACT logic — the one `deliver_prompt` actually calls,
/// not a reimplementation of it — against a real (fake-child-backed, see
/// `PtyManager::register_fake_for_test`) `PtyManager`, without needing a real
/// Tauri `AppHandle` (unavailable headless — `tauri::test`'s `MockRuntime`
/// isn't the concrete `Wry` runtime the rest of `deliver_prompt`'s setup
/// requires) or a real agent CLI (CLAUDE.md constraint 3). rev-19 R3: a test
/// that only calls `should_flush_before_paste_now` directly proves the
/// *decision* is right but not that `deliver_prompt` acts on it — this
/// closes that gap, since `deliver_prompt` has nothing left to get wrong
/// here beyond calling this one function. Returns whether it actually wrote.
#[doc(hidden)] // pub for integration tests
pub fn flush_stranded_text(
    ptys: &crate::pty::PtyManager,
    pty_id: u32,
    prev_confirmed: Option<bool>,
    human_typed_since: bool,
    submit: &[u8],
    // #576: this reader gets the delivery record for the same reason the
    // drainer gate does — it is a blind Enter decided off `question_active_now`
    // with no `pasted_text`, and a wrapped notice of ours left it declining
    // forever. Fixing one reader of the gate and leaving another was rev-126's
    // finding (`e7`), so both move together.
    delivered: Vec<String>,
) -> bool {
    let question_active = question_active_now(ptys, pty_id, None, delivered);
    // #532: BOTH live readings are taken here, at the press, and neither is
    // inherited from a check some earlier stage passed. `unwrap_or(true)` is
    // the fail-safe direction for an occupancy reading we cannot take (a
    // closed pty): decline the Enter rather than press blind. It matches
    // `human_input_block_now`'s treatment of the same unreadable case.
    let box_pending = ptys.input_pending(pty_id).unwrap_or(true);
    should_flush_before_paste_now(prev_confirmed, human_typed_since, question_active, box_pending)
        && ptys.write_bytes(pty_id, submit).is_ok()
}

/// What a spaced submit retry should do this iteration (#420 rev-15 B2):
/// `deliver_prompt`'s retry loop used to press Enter unconditionally once the
/// human-typing check passed — exactly the window (a few seconds after the
/// first submit) Copilot most often paints a permission/question dialog. A
/// question appearing here means HOLD, not retry, so a fresh
/// `PasteDecision` (already gated on `prompt_wait_detected` and capped, same
/// as every other checkpoint) is threaded through as its own case rather than
/// being collapsed into a bool — the caller's audit text differs by WHY a
/// retry didn't fire, same as every other checkpoint in this function.
///
/// rev-19 N9: the human-typing check is NOT a variant here. It used to be
/// (`SkipHumanTyping`), but the caller already checks it and `break`s BEFORE
/// ever calling this function — meaning that arm could never actually be
/// reached, and its match arm carried an `unreachable!()` in code that runs
/// on a detached thread: a latent panic waiting for some future refactor to
/// reorder the caller and make it reachable for real. Dropping the variant
/// (and the parameter) removes the dead arm entirely instead of trusting
/// nobody ever reaches it — this function's only job now is "given the
/// question hold's outcome, write or don't", which is also all it can be
/// asked, since human-typing precedence is enforced by the caller's own
/// control flow, not by anything this function could get wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryGate {
    /// No question is in the way — press Enter. Carries the hold duration
    /// (0 if it was never active) straight through so the caller never needs
    /// to re-destructure the original `PasteDecision` to audit it (rev-19
    /// N9: no `unreachable!()` fallback needed for a value this enum already
    /// carries).
    Write { held_ms: u64 },
    /// A live question was still on screen when the hold for it capped out.
    SkipQuestionPending { held_ms: u64 },
}

/// Decide `RetryGate` for one spaced retry from the question hold's outcome.
pub fn retry_gate(question_decision: PasteDecision) -> RetryGate {
    match question_decision {
        PasteDecision::Paste { held_ms } => RetryGate::Write { held_ms },
        PasteDecision::Abort { held_ms } => RetryGate::SkipQuestionPending { held_ms },
    }
}
