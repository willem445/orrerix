//! Delivery holds: escalation, hold episodes, hold classes and channels, the
//! undeliverable cause and notice, the orchestrator notice inbox, and the
//! delivery-held events.
//! Design note: `docs/design/delivery-triage.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. Sibling files it
//! calls: `humaninput.rs`, `noticemask.rs`, `questionhold.rs`,
//! `strandedpolicy.rs`.

use super::*;

/// What the drainer should do about a pane it is not yet allowed to write to
/// (#532, extracted for rev-12 B1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldEscalation {
    /// Nothing to do: the hold has already been badged and nothing has changed
    /// (the chip raised earlier in the episode stays up — see `Chip`).
    None,
    /// #563: the pane is held RIGHT NOW and the human should see that right
    /// now. Raise the pane-header delivery-held chip naming this reason.
    ///
    /// This is the outcome that did not exist before #563, and its absence was
    /// the whole bug. `deliver_now` badges its own capped in-attempt waits
    /// (`emit_held`), but the hold a human actually sits in front of lives
    /// BETWEEN attempts, in `run_queue_drainer`'s poll loop — which used to
    /// `continue` on a blocked admission with no UI event of any kind. So the
    /// chip existed only for the seconds inside one attempt, and the pane went
    /// dark for the whole sustained hold, until `Badge` fired
    /// `QUESTION_HOLD_STALE_AFTER` (ten minutes, in practice up to ~19) later.
    ///
    /// Deliberately NOT one-shot in this function. The caller owns idempotence
    /// (it tracks whether the chip is up), for the same reason `Badge`'s
    /// one-shot is an INPUT: a decision that "the pane is held" is a fact
    /// about right now, whereas "have we said so already" is caller state.
    Chip(HeldReason),
    /// Raise the pane's attention badge, naming this blocker. Fires at most
    /// once per hold episode — the caller's one-shot is an INPUT here
    /// (`already_badged`), not a second decision made somewhere else.
    ///
    /// #563: the chip must be up here too. A hold that reaches the bound
    /// without ever having been chipped is reachable — a drainer that starts
    /// on an entry enqueued long ago evaluates its first poll already past the
    /// bound — so the caller raises the chip on this arm as well rather than
    /// assuming a prior `Chip` did.
    Badge(StrandedBlocker),
    /// The pane is writable again: drop whatever this escalation raised — the
    /// chip, the badge, or both.
    ///
    /// #563: this now fires on EVERY writable poll, not only when a badge was
    /// raised. With a chip that can be up without a badge ever having fired,
    /// "we badged, so there is something to clear" stopped being derivable
    /// here, and the alternative — a sixth parameter — would have made this
    /// function's contract depend on two separate pieces of caller state. The
    /// caller's clears are already guarded (`clear_stranded` audits only when
    /// a badge was really up; the chip clear is gated on the caller's own
    /// `chip_reason`), so a `Clear` for a pane with nothing up is a no-op, not
    /// a spurious audit line.
    Clear,
}

/// The one decision point for #532's escalation — pure, so that the whole
/// contract (when it fires, at most once, what it names, and when it comes
/// down) is directly pinnable.
///
/// **Why this is extracted rather than inline (rev-12 B1).** It shipped as an
/// `&&` chain at the drainer's call site, which meant the entire escalation
/// path could be deleted with the whole suite still green — on a PR whose
/// reason for existing is a hold that escalated to nobody. That is precisely
/// the failure `should_flush_before_paste_now`'s own doc argues against ("a
/// named function, not an inline `&&` at the call site, so the combination is
/// independently testable and can't be silently dropped by a future edit"); the
/// doctrine was stated in this file and then not followed one function over.
///
/// Precedence:
/// - Writable (`admission.go()`) ⇒ `Clear`. The pane recovering is what ends
///   the episode, not a timer. (#563: unconditional now — see `Clear`'s doc.)
/// - Inside the bound ⇒ `Chip(reason)` (#563). The escalation is not due, but
///   the pane is held and the human must be able to SEE that from the first
///   poll. This arm returned `None` before #563, and that silence — not a
///   misclassification — is what the issue reports.
/// - Already badged ⇒ `None`. The one-shot lives here so "at most once per
///   episode" is a property of this function rather than of whichever caller
///   remembers to check a set first. The chip stays up; only the badge is
///   one-shot.
/// - Otherwise ⇒ `Badge`, naming the gate that is actually blocking:
///   `HoldQuestion` ⇒ [`StrandedBlocker::QuestionStale`] (loomux may be reading
///   a question that is no longer on screen), `HoldBoxOccupied` ⇒
///   [`StrandedBlocker::HumanInput`].
///
/// That last arm is the NB3 fix in behavioural terms: a long hold is *always*
/// reported, and the only thing the blocked gate decides is which sentence the
/// human reads. Neither arm ever presses Enter.
///
/// **What `HumanInput`'s existing wording does and does not promise (rev-27
/// NB-E).** It reads *"stuck behind text you typed — press Enter or clear the
/// box"*, and reusing it here rather than minting a variant is deliberate — but
/// not because it is unconditionally true. Under the very stuck-true mode
/// [`hold_bound_elapsed`] refuses to trust, `input_pending` can be set over an
/// empty box, and then "text you typed" asserts content that is not there.
/// That is the same unbacked-claim class which justified minting
/// `QuestionStale` instead of reusing `Question`, so it is named rather than
/// glossed.
///
/// What makes reuse right anyway is the *action*, which is the part a human
/// acts on: `classify_human_input` reads `\r` as `Submit` and zeroes the
/// counter outright, so "press Enter" genuinely releases the hold in **both**
/// branches — real leftover text, or a stuck counter over an empty box. That is
/// the same both-branches-safe standard `QuestionStale`'s wording was held to,
/// and it is met; only the diagnosis, not the remedy, can be wrong.
///
/// **The one-shot freezes the wording, not just the count (rev-27 NB-F).** A
/// badge raised as `QuestionStale` keeps its "type a character and delete it"
/// advice even if the human then starts typing and the real blocker becomes
/// `HoldBoxOccupied`. The chip is then offering advice for the other branch.
/// This is the deliberate cost of suppressing re-badges: the pane is genuinely
/// held either way, and neither prescribed action is unsafe on the other's
/// branch (typing-and-deleting leaves the box as it found it; pressing Enter
/// submits a line the human owns and would have submitted anyway). Recorded so
/// the tradeoff is chosen rather than rediscovered.
#[doc(hidden)] // pub for integration tests
pub fn held_escalation(
    admission: WriteAdmission,
    held_since_ms: u64,
    now_ms: u64,
    bound_ms: u64,
    already_badged: bool,
) -> HeldEscalation {
    if admission.go() {
        return HeldEscalation::Clear;
    }
    // #563: held, and inside the bound — the escalation is not due yet, but the
    // VISIBILITY is due immediately. Returning `None` here (the pre-#563
    // behaviour) is precisely the invisible window the issue reports: nothing
    // at all reported the hold until the bound elapsed.
    if !hold_bound_elapsed(held_since_ms, now_ms, bound_ms) {
        return match admission.held_reason() {
            Some(reason) => HeldEscalation::Chip(reason),
            // Unreachable — `admission.go()` returned above, and every other
            // variant has a reason. Named rather than `unreachable!()` for the
            // same reason the `Go` arm below is: this runs on a detached
            // drainer thread, where a panic is a silently dead pane.
            None => HeldEscalation::None,
        };
    }
    if already_badged {
        // The badge is up and stays up; so does the chip the caller raised
        // earlier in this episode. Nothing to change.
        return HeldEscalation::None;
    }
    match admission {
        WriteAdmission::HoldQuestion => HeldEscalation::Badge(StrandedBlocker::QuestionStale),
        WriteAdmission::HoldBoxOccupied => HeldEscalation::Badge(StrandedBlocker::HumanInput),
        // Unreachable — `admission.go()` returned above. Named rather than
        // `unreachable!()`: this runs on a detached drainer thread, where a
        // panic is a silently dead pane (rev-19 N9's finding, same reasoning).
        WriteAdmission::Go => HeldEscalation::None,
    }
}

/// #560: one pane's open hold episode — *this pane has not accepted a delivery
/// since `started_ms`* — and what has already been said about it.
///
/// **The episode, not the entry.** #532 measured its escalation from the front
/// queue entry's `enqueued_ms`, which is a fact about a *payload*, not about the
/// pane. The two come apart the moment the queue front changes under a pane that
/// never recovered, which is exactly what `enqueue_stranded_front` does after an
/// `AbortedPreEnter`: it pops the batch that was pasted and pushes a
/// `StrandedSubmit` marker carrying a fresh `now_ms()`, handing the bound a
/// brand-new clock for a pane that has been blocked continuously. Keying on the
/// pane instead makes "held since T" mean what both the badge's wording and the
/// one-shot already assumed it meant.
///
/// **In memory, and what a restart does.** This is not queue state and owes no
/// `persist_queues` call (#468) — the same argument its `question_stale_notified`
/// predecessor made. A restart therefore forgets any mid-flight episode: the
/// process that would have badged is gone, `queue.json` recovery re-admits the
/// payloads (#467), and the fresh drainer opens a new episode on its first
/// failed observation, so the ten-minute clock restarts from the restart. That
/// is the right direction to be wrong in — persisting it would badge instantly
/// on boot for a hold a human may well have resolved while loomux was down,
/// which is a false claim on the badge whose whole job is to be trustworthy —
/// and the loss is bounded by the restart already announcing itself through
/// recovery notices and `queue_orphans`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::orchestration) struct HoldEpisode {
    /// When this pane's current hold episode began — the drainer's first
    /// observation that the pane did not accept a delivery.
    pub(in crate::orchestration) started_ms: u64,
    /// Whether `delivery-held-in-queue` has been written for THIS episode. One
    /// line per episode, not per poll: the record has to show that loomux held
    /// and said so without becoming an entry every two seconds.
    ///
    /// #560: this replaces keying that one-shot on the drainer's pane-header
    /// chip being down. The chip is LIVE state (it follows the gate on every
    /// poll and comes down the instant the pane reads writable), so keying an
    /// episode-scoped audit line on it churned for the same reason the badge
    /// did — a flickering pane re-raised the chip and re-wrote the line.
    pub(in crate::orchestration) announced: bool,
    /// Whether the ten-minute escalation badge ([`StrandedBlocker::QuestionStale`]
    /// / [`StrandedBlocker::HumanInput`]) has fired for THIS episode — the
    /// `already_badged` input [`held_escalation`] takes.
    ///
    /// **Stated limit.** This says *we raised it*, not *it is still up*: another
    /// mechanism's `clear_stranded` (the late monitor's `Resolved` arm, or
    /// `attention_tick` pruning a dead agent) can take the badge down while this
    /// stays `true`, and the escalation will not re-raise until the episode
    /// ends. Not repaired here on purpose — a re-raise loop would fight whatever
    /// just cleared it, and the one realistic clearer (`Resolved`) means the
    /// delivery resolved, which produces the `Delivered` observation that ends
    /// the episode anyway. Pre-#560 the same latch existed for as long as no
    /// writable poll intervened; what changed is that a writable poll no longer
    /// resets it.
    pub(in crate::orchestration) badged: bool,
    /// #590 L2: whether `notice-undeliverable` has been reported for THIS
    /// episode ([`OrchRegistry::note_undeliverable_notice`]).
    ///
    /// **Its own flag rather than a read of `badged`**, which is the whole
    /// reason it exists as a field, and the two come apart in BOTH directions:
    ///
    /// - a badge RAISE sets `badged`, after which `held_escalation` returns
    ///   `None` for the rest of the episode (`if already_badged`), so sharing
    ///   the flag would mean never reporting after the instant the bound was
    ///   crossed;
    /// - a badge raise DECLINED — another mechanism already owns the badge —
    ///   leaves `badged` false while `Badge` comes back on every poll, so
    ///   sharing it would mean re-reporting every couple of seconds, on the
    ///   pane with the most wrong with it.
    ///
    /// **Set by the poll that REPORTS, never by one that merely looked**
    /// (rev-128's blocking finding). Claiming it at the top of
    /// [`OrchRegistry::note_undeliverable_notice`] spent an episode's only
    /// report on a poll that found nothing queued, which silenced the case
    /// where a notice joins a pane that is ALREADY held — one step from #590's
    /// own incident, and unbounded, since an episode ends only when the pane
    /// accepts a delivery.
    ///
    /// Same lifetime as the rest of the episode: cleared when the pane accepts
    /// a delivery, and forgotten across a restart, so the worst a restart costs
    /// is one repeated diagnosis of a pane that is still stuck.
    pub(in crate::orchestration) notice_reported: bool,
}

/// #560: what one drainer iteration observed about a pane, as a closed set, and
/// the single place that says which observations END its hold episode.
///
/// **Why a type rather than four `if`s at the call sites.** The rule this
/// encodes — *a momentary writable reading is not the end of an episode; a
/// delivery landing is* — is the entire fix for #560's first symptom, and the
/// pre-#560 code expressed the opposite rule implicitly, by clearing the badge
/// one-shot inside the drainer's `HeldEscalation::Clear` arm. Nothing named it,
/// so nothing could test it and nothing failed when it was wrong. Matching
/// exhaustively in [`ends_hold_episode`] means a future observation cannot be
/// added without deciding, in writing and in one place, what it does to the
/// clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldObservation {
    /// A poll found the pane not writable (`write_admission` held).
    HeldPoll,
    /// A poll found the pane writable. **Provisional**, not proof: the drainer
    /// goes straight on to `deliver_now`, which re-reads both gates at the
    /// paste and at the Enter and can abort on either — so "writable at the
    /// instant of one poll" is precisely the reading a flickering pane produces
    /// between two aborted attempts.
    WritablePoll,
    /// An attempt ran and did not deliver (`AbortedPrePaste`/`AbortedPreEnter`).
    /// OPENS an episode if none is open: a hold that lives entirely inside
    /// `deliver_now` (every poll reads writable, every attempt then aborts)
    /// would otherwise never start a clock at all, and that pane is exactly as
    /// stuck as one that fails at the poll.
    Aborted,
    /// A delivery LANDED (`DeliverOutcome::Done`) — the pane accepted a write.
    Delivered,
    /// #813: loomux gave up on a `StrandedSubmit` repair it could not safely
    /// perform (`DeliverOutcome::Retired`). Ends the episode exactly as
    /// `Delivered` does — loomux is no longer holding this pane on that entry,
    /// and the next entry clocks and badges afresh — but says so without
    /// claiming the pane accepted a write.
    Retired,
}

/// #560: does this observation end the pane's hold episode?
///
/// Only evidence that the pane actually **accepted** a delivery does. The
/// alternative — ending an episode on a writable reading — is what #532
/// effectively did, and it fails in both directions at once:
///
/// - *Churn.* A badged pane that reads writable for one poll and then aborts
///   drops and re-raises its badge (`stranded-attention` / `stranded-cleared`
///   per flicker), which is the audit flood the one-shot exists to prevent.
/// - *Suppression, which is worse.* If the clock restarted on every writable
///   poll, a pane whose occupancy toggles faster than the bound — a human
///   typing with a delivery queued behind them, #560's own reported scenario —
///   would never badge **at all**. That re-creates the #532 bug class
///   ("an escalation that never fires") and violates the rule
///   [`hold_bound_elapsed`]'s doc states outright: an escalation must not be
///   suppressible by the signals it exists to report on.
///
/// The queue emptying ends an episode too, but not through here: `commit_exit`
/// drops the record in the same generation-guarded step that deregisters the
/// drainer, because that removal has to be atomic with the queue check.
pub fn ends_hold_episode(observation: HoldObservation) -> bool {
    match observation {
        // #813: a retired marker ends the episode for the same reason a
        // delivery does — whatever loomux was holding this pane for is over.
        HoldObservation::Delivered | HoldObservation::Retired => true,
        HoldObservation::HeldPoll | HoldObservation::WritablePoll | HoldObservation::Aborted => false,
    }
}

/// #560: does this observation OPEN a hold episode (start the clock) if none is
/// open yet? Both of the drainer's ways of failing to deliver do; a writable
/// poll neither opens nor closes one.
pub fn opens_hold_episode(observation: HoldObservation) -> bool {
    match observation {
        HoldObservation::HeldPoll | HoldObservation::Aborted => true,
        HoldObservation::WritablePoll
        | HoldObservation::Delivered
        | HoldObservation::Retired => false,
    }
}

/// Why a prompt delivery is currently being held for human input (#246): the
/// UI-facing counterpart to the audit-log holds above. `Typing` is
/// `wait_for_user_quiet`'s "human is actively typing" hold (#43); `BoxOccupied`
/// is `wait_for_box_clear`'s "an unsubmitted line sits in the box" hold (#111,
/// backed by `box_occupancy_delta`/#171). Two reasons because they read
/// differently to a human watching the pane — one is "wait, I'm still typing",
/// the other is "wait, I left something in the box" — even though both boil
/// down to "loomux won't paste over you".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldReason {
    Typing,
    BoxOccupied,
    /// A question/permission TUI is on screen (#420): the pane's own agent is
    /// mid-dialog (Copilot's numbered/radio-select question, a y/n permission
    /// prompt, Claude's `AskUserQuestion`), and a programmatic paste+Enter would
    /// land ON that dialog — worse than the other two reasons, because Enter
    /// there doesn't just merge text, it SELECTS an option (the highlighted
    /// default, usually the first) and silently steers the agent. See
    /// `prompt_wait_detected` for the detector and `confirm_copilot_autopilot_dialog`
    /// for why the autopilot-consent dialog specifically is exempt from this hold.
    InteractiveQuestion,
}

impl HeldReason {
    pub fn as_str(self) -> &'static str {
        match self {
            HeldReason::Typing => "typing",
            HeldReason::BoxOccupied => "box-occupied",
            HeldReason::InteractiveQuestion => "question",
        }
    }
}

/// #563: every way loomux can withhold a delivery, as a closed set.
///
/// **Why this exists as a type rather than a paragraph.** #563 is an
/// *invisible* hold: a delivery held against an empty-looking pane, with no
/// warning, until the queue filled and work was dropped. The reason it could
/// happen is that "which holds are visible" was never written down anywhere a
/// compiler or a test could check — the pane-header chip covered the holds
/// inside one delivery attempt (`deliver_now`) and simply did not exist for
/// the hold BETWEEN attempts (`run_queue_drainer`'s poll loop), and nothing
/// anywhere said so. A prose table would have rotted the same way; this one
/// fails the build.
///
/// The enforcement is two-part and neither half is decorative:
/// - [`hold_channels`] matches exhaustively, so a new hold classification
///   cannot be added without declaring how a human learns about it;
/// - `every_hold_class_reaches_a_human` (integration tests) asserts every
///   classification has at least one channel that
///   [`HoldChannel::survives_orchestrator_target`] — because the in-band
///   notice channel is suppressed on exactly the pane #563 was reported on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldClass {
    /// #43, pre-paste: the human is actively typing in the pane.
    PrePasteTyping,
    /// #111/#510, pre-paste: a human line is sitting unsubmitted in the box.
    PrePasteBoxOccupied,
    /// #420, pre-paste: a question/permission TUI owns the Enter key.
    PrePasteQuestion,
    /// #532, pre-paste: the two gates kept re-arming for
    /// `PREPASTE_RECHECK_ROUNDS`, so the attempt gave up without pasting.
    PrePasteRecheckExhausted,
    /// #43, pre-Enter: the human started typing during the paste settle.
    PreEnterTyping,
    /// #420, pre-Enter: a dialog appeared while our paste was settling.
    PreEnterQuestion,
    /// #532, pre-Enter: human-typed content is in the box at submit time.
    PreEnterBoxOccupied,
    /// #563: the drainer's poll found the box occupied BETWEEN attempts. This
    /// is where a sustained hold actually lives, and it is the classification
    /// that had no channel at all before #563.
    QueuePollBoxOccupied,
    /// #563: as above, blocked on the interactive-question gate.
    QueuePollQuestion,
    /// #532: a poll hold that has outlived `QUESTION_HOLD_STALE_AFTER`.
    QueueStaleEscalation,
    /// #563: the pane's queue has reached `queue::QUEUE_NEAR_FULL_AT` —
    /// nothing lost yet, and the warning exists to keep it that way.
    QueueNearFull,
    /// #445/#563: the queue is at `queue::QUEUE_MAX_PER_PANE`; further
    /// arrivals are rejected outright.
    QueueFull,
    /// The human paused the group, so loomux delivers nothing (`deliver_prompt`).
    GroupPaused,
    /// #590 L2: a hold that has outlived `QUESTION_HOLD_STALE_AFTER` **on a
    /// pane whose queue holds one of loomux's own `[orrerix]` notices**
    /// (`queue::is_loomux_notice`).
    ///
    /// A strict subset of [`HoldClass::QueueStaleEscalation`]'s panes, split
    /// out because the two are not the same event to a reader. A stale hold on
    /// a kickoff is work waiting; a stale hold on a NOTICE means an agent is
    /// being kept from something loomux decided it needed to know — and in
    /// #590's incident that agent was blocked *on the very condition the
    /// undelivered notice reported*, so the pane could not clear itself and no
    /// channel reached anyone who could. Hence the extra two channels: this is
    /// the only hold classification whose harm lands on an AGENT rather than
    /// on a human's attention.
    UndeliverableNotice,
}

impl HoldClass {
    /// Every classification. Maintained against [`HoldClass::ordinal`]'s
    /// exhaustive match and the length assertion in
    /// `every_hold_class_reaches_a_human`.
    pub const ALL: &'static [HoldClass] = &[
        HoldClass::PrePasteTyping,
        HoldClass::PrePasteBoxOccupied,
        HoldClass::PrePasteQuestion,
        HoldClass::PrePasteRecheckExhausted,
        HoldClass::PreEnterTyping,
        HoldClass::PreEnterQuestion,
        HoldClass::PreEnterBoxOccupied,
        HoldClass::QueuePollBoxOccupied,
        HoldClass::QueuePollQuestion,
        HoldClass::QueueStaleEscalation,
        HoldClass::QueueNearFull,
        HoldClass::QueueFull,
        HoldClass::GroupPaused,
        HoldClass::UndeliverableNotice,
    ];

    /// Dense index into [`HoldClass::ALL`]. Exhaustive on purpose: a new
    /// variant cannot compile without being given an index, and the test then
    /// fails until `ALL` lists it — so the completeness of `ALL` is enforced
    /// rather than trusted.
    pub fn ordinal(self) -> usize {
        match self {
            HoldClass::PrePasteTyping => 0,
            HoldClass::PrePasteBoxOccupied => 1,
            HoldClass::PrePasteQuestion => 2,
            HoldClass::PrePasteRecheckExhausted => 3,
            HoldClass::PreEnterTyping => 4,
            HoldClass::PreEnterQuestion => 5,
            HoldClass::PreEnterBoxOccupied => 6,
            HoldClass::QueuePollBoxOccupied => 7,
            HoldClass::QueuePollQuestion => 8,
            HoldClass::QueueStaleEscalation => 9,
            HoldClass::QueueNearFull => 10,
            HoldClass::QueueFull => 11,
            HoldClass::GroupPaused => 12,
            HoldClass::UndeliverableNotice => 13,
        }
    }
}

/// #563: how a human can learn that a delivery is being withheld.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldChannel {
    /// The pane-header `⏸ held` chip (`orch-delivery-held`). A UI event keyed
    /// on `pty_id` alone — no role or pause suppression anywhere in its path
    /// (`src/orchestration.ts`'s listener, `heldbadge.ts`'s mapping).
    HeldChip,
    /// The pane's attention badge (`mark_stranded` → `stranded_detail`). Also
    /// a UI channel, also unsuppressed by role.
    AttentionBadge,
    /// An in-band `[orrerix] …` notice delivered to the group's orchestrator
    /// (`notify_queue`).
    ///
    /// **Never sufficient on its own.** It is suppressed outright when the
    /// held pane IS the orchestrator's (a notice about the orchestrator's
    /// blocked pane would queue behind the very block it reports) and again
    /// while the group is paused. #563 was reported on an orchestrator pane;
    /// classifications whose only channel was this one were, on that pane,
    /// silent.
    OrchestratorNotice,
    /// #578: the notice [`HoldChannel::OrchestratorNotice`] could NOT deliver
    /// to an orchestrator target, parked by `notify_queue` and handed back on
    /// that orchestrator's next MCP tool result
    /// (`OrchRegistry::take_orchestrator_notices`).
    ///
    /// **Why this one survives an orchestrator target.** It is not a delivery.
    /// It consumes no slot in the target pane's queue and types nothing into
    /// the pane, so it cannot queue behind the very block it reports — the
    /// loop that makes the in-band notice structurally undeliverable here.
    /// It rides back on a call the orchestrator itself made, which is also the
    /// proof that the orchestrator is running and reading at that instant.
    ///
    /// **And so it needs no cap exemption**, which is the sharpest way to see
    /// the difference from the other orchestrator-facing notice on this list.
    /// #615's pause-loss notice IS a delivery, so on a pane at
    /// `queue::QUEUE_MAX_PER_PANE` — exactly the pane it has the most to report
    /// about — it was certain to be destroyed by the same cap it was reporting,
    /// and needed `queue::EnqueueReason::PauseLossNotice`'s one entry of
    /// headroom to survive. This channel never meets that problem: a full queue
    /// is the CONDITION it reports, not an obstacle to reporting it.
    ///
    /// **A pull, not a push, and listed ALONGSIDE the badge rather than
    /// instead of it.** An orchestrator that never calls another tool never
    /// reads its inbox: this channel reaches the orchestrator *agent* on its
    /// next turn, while the badge/chip reach the *human* regardless. The two
    /// cover different failures (a wedged human-facing UI vs. an unattended
    /// overnight run with nobody at the window) and neither subsumes the
    /// other.
    OrchestratorInbox,
    /// A synchronous `Err` returned to the agent that called the MCP tool
    /// (`queue::queue_full_error`). Reaches the *sender*, never the human and
    /// never the held pane's own agent.
    CallerError,
    /// The group's paused state, which the human set and the group UI shows.
    /// The one classification a human cannot be surprised by.
    PausedGroupUi,
}

impl HoldChannel {
    /// Whether this channel still reaches a human when the held pane IS the
    /// group's own orchestrator — the #563 case, and the property the
    /// completeness test asserts.
    ///
    /// Matched exhaustively rather than written as a `!matches!` negation
    /// (#578): a channel added later must state its own answer here instead
    /// of defaulting to "survives", which is the optimistic half of the
    /// answer and the one that would make the completeness test pass by
    /// accident.
    pub fn survives_orchestrator_target(self) -> bool {
        match self {
            HoldChannel::OrchestratorNotice => false,
            HoldChannel::HeldChip
            | HoldChannel::AttentionBadge
            | HoldChannel::OrchestratorInbox
            | HoldChannel::CallerError
            | HoldChannel::PausedGroupUi => true,
        }
    }
}

/// #563: the channels each hold classification actually fires. Exhaustive, so
/// a hold added later cannot be silent by omission.
///
/// This is a description of what the code does, not an aspiration — every arm
/// is backed by a call site, and the arms that changed in #563
/// (`QueuePoll*`, `QueueNearFull`, `QueueFull`) changed because the call sites
/// did.
///
/// **#578 and the [`HoldChannel::OrchestratorInbox`] arms.** The inbox is a
/// property of `notify_queue` specifically — it is that function's suppressed
/// branch, parked instead of discarded — so it is listed on exactly the
/// classifications whose notice goes through `notify_queue`, and nowhere else.
/// [`HoldClass::GroupPaused`] is the one that looks like it should have it and
/// must not: its `OrchestratorNotice` is `announce_pause_suppression`, a
/// different call on a different path that `notify_queue` never sees — and
/// since #615 an actual in-band DELIVERY, admitted past the cap with
/// `queue::EnqueueReason::PauseLossNotice`. Listing an inbox it never parks
/// into would make this table say something untrue.
pub fn hold_channels(class: HoldClass) -> &'static [HoldChannel] {
    match class {
        // `deliver_now`'s in-attempt holds all badge via `emit_held`.
        HoldClass::PrePasteTyping
        | HoldClass::PrePasteBoxOccupied
        | HoldClass::PrePasteQuestion
        | HoldClass::PreEnterTyping
        | HoldClass::PreEnterQuestion => &[HoldChannel::HeldChip],
        // These two abort the attempt without ever having badged (the recheck
        // loop's gates alternated; the pre-Enter box gate is a single reading,
        // not a hold). Neither leaves the pane unattended: the entry stays at
        // the front of the queue and the drainer's poll — which now chips
        // immediately — is the next thing to look at it.
        HoldClass::PrePasteRecheckExhausted | HoldClass::PreEnterBoxOccupied => {
            &[HoldChannel::HeldChip, HoldChannel::OrchestratorNotice, HoldChannel::OrchestratorInbox]
        }
        // #563's fix: the poll hold chips from the first blocked tick and
        // escalates to a badge at `QUESTION_HOLD_STALE_AFTER`.
        HoldClass::QueuePollBoxOccupied | HoldClass::QueuePollQuestion => {
            &[HoldChannel::HeldChip, HoldChannel::AttentionBadge]
        }
        HoldClass::QueueStaleEscalation => &[HoldChannel::AttentionBadge, HoldChannel::HeldChip],
        // #563's fix: a badge (orchestrator-safe) alongside the notice.
        // #578 adds the inbox: the badge tells a human who is looking, the
        // inbox tells the orchestrator agent on its next turn — which is the
        // only one of the two that fires on an unattended overnight run.
        HoldClass::QueueNearFull => &[
            HoldChannel::AttentionBadge,
            HoldChannel::OrchestratorNotice,
            HoldChannel::OrchestratorInbox,
        ],
        HoldClass::QueueFull => &[
            HoldChannel::AttentionBadge,
            HoldChannel::OrchestratorNotice,
            HoldChannel::OrchestratorInbox,
            HoldChannel::CallerError,
        ],
        // #569, and re-stated for option 2 (enqueue-while-paused), which
        // changed what this hold DOES: the payload is now held in the pane's
        // durable queue and flushed on resume, so a pause is a delay, no
        // longer the one hold that destroys what it withholds.
        //
        //  - `PausedGroupUi` is the hold itself: the human set the pause, the
        //    group UI shows it for as long as it lasts, and the deliveries
        //    behind it are safe. The one classification a human cannot be
        //    surprised by.
        //  - `OrchestratorNotice` / `AttentionBadge` are
        //    `announce_pause_suppression` and its no-live-orchestrator
        //    fallback (`StrandedBlocker::PauseSuppressed`), and they are NOT
        //    vestigial (review B2). Besides the legacy discard, they carry the
        //    one loss a pause on this build can still cause: a pane at
        //    `queue::QUEUE_MAX_PER_PANE` refuses admissions for as long as the
        //    pause lasts, and the refused sender's `Err` reaches the sender,
        //    never the human. That is precisely what this table exists to
        //    catch, so all three arms are live.
        //
        // The resume flush itself is not a channel: it is a delivery to the
        // agent that was waiting, not a way for a HUMAN to learn about a hold,
        // which is the only question this table answers.
        HoldClass::GroupPaused => &[
            HoldChannel::PausedGroupUi,
            HoldChannel::OrchestratorNotice,
            HoldChannel::AttentionBadge,
        ],
        // #590 L2. A separate classification from `QueueStaleEscalation`
        // rather than two more channels bolted onto it, because the two say
        // different things and only one of them is unconditionally true.
        // `QueueStaleEscalation` fires for ANY pane held past the bound and
        // reaches the human only; this one fires for the subset where the held
        // payload is loomux's OWN notice, and reaches the orchestrator agent
        // too. Folding the extra channels into that row would make the table
        // claim an orchestrator-facing channel for a held kickoff, which
        // nothing sends.
        HoldClass::UndeliverableNotice => &[
            HoldChannel::AttentionBadge,
            HoldChannel::HeldChip,
            HoldChannel::OrchestratorNotice,
            HoldChannel::OrchestratorInbox,
        ],
    }
}

/// #590 L2: what a held pane's own readings say about WHO is holding it, at
/// the moment loomux concludes that its own notice cannot be delivered there.
///
/// **Why the diagnosis is worth a type.** The channel this feeds is read by
/// the orchestrator *agent*, and the two hold reasons the drainer can report
/// (`box-occupied`, `question`) do not distinguish the cases that need
/// opposite responses: a human's half-written line (wait — a person is right
/// there) from a CLI's own turn state (do not wait — the pane will not clear
/// until the agent's turn ends, and the notice sitting undelivered may be the
/// very thing that would end it). #590's incident is the second one and read
/// on the wire exactly like the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndeliverableCause {
    /// The box is occupied and **no human keystroke has landed in this pane
    /// since the hold episode opened** (`PtyManager::last_user_input_ms`, which
    /// #496 gates on `classify_human_input` so a terminal's own query replies
    /// never stamp it). So whatever occupies the box is not a person's — it is
    /// the pane's own CLI, mid-turn.
    ///
    /// **What this claim is, exactly.** It is *negative* evidence — "not a
    /// human" — plus the box reading, and that is the actionable half. It is
    /// not proof that a turn is running: `input_pending` is documented to latch
    /// over an already-empty box (a bare ESC, a TUI line-clear, a CLI consuming
    /// the line), so a quiescent pane with a latched counter reads the same. The
    /// wording therefore states the evidence it has rather than the inference
    /// alone, and every response it invites (look at the pane, do not wait for
    /// it) is correct under both readings.
    PaneMidTurn,
    /// The box is occupied and a human HAS typed in this pane since the episode
    /// opened. #510's absolute is doing exactly what it exists to do; the badge
    /// and the chip are the right channels and the orchestrator should not
    /// treat this as a stall.
    HumanTyping,
    /// #420: a question/permission dialog owns the pane's Enter key. Checked
    /// BEFORE the keystroke evidence because it is a direct observation of the
    /// pane rather than an inference from its absence — and because it covers
    /// that evidence's documented blind spot: a human navigating a dialog with
    /// arrow keys classifies `Neutral` with a zero occupancy delta and never
    /// stamps `last_user_input_ms` (#496's stated tradeoff), so a
    /// keystroke-first order would report a human mid-menu as a pane mid-turn.
    QuestionOnScreen,
    /// No reading to classify from — a closed pty, or a hold with no gate
    /// attributed to it. Named rather than folded into one of the others so a
    /// notice never asserts a cause it did not observe.
    Unknown,
}

impl UndeliverableCause {
    /// The audit-line token.
    pub fn as_str(self) -> &'static str {
        match self {
            UndeliverableCause::PaneMidTurn => "pane-mid-turn",
            UndeliverableCause::HumanTyping => "human-typing",
            UndeliverableCause::QuestionOnScreen => "question-on-screen",
            UndeliverableCause::Unknown => "unknown",
        }
    }

    /// The clause [`undeliverable_notice`] renders, which is where the
    /// diagnosis becomes an instruction. Each one names the evidence behind it,
    /// so a reader can check the claim instead of trusting it.
    fn phrase(self) -> &'static str {
        match self {
            UndeliverableCause::PaneMidTurn => {
                "pane mid-turn (no human keystroke since the hold began), so nothing but that \
                 agent's own turn ending will clear it — read the pane, do not wait on it"
            }
            UndeliverableCause::HumanTyping => {
                "a human has typed in this pane since the hold began, so it is waiting behind \
                 their line and will clear when they submit it"
            }
            UndeliverableCause::QuestionOnScreen => {
                "a question/permission dialog owns the pane and needs an answer before anything \
                 can be delivered into it"
            }
            UndeliverableCause::Unknown => {
                "loomux could not read the pane's input state, so it cannot say what is holding it"
            }
        }
    }
}

/// #590 L2: classify a held pane at the escalation bound.
///
/// `held` is the blocking gate the drainer's own `write_admission` reported
/// (`WriteAdmission::held_reason`), `last_user_input_ms` the pane's
/// human-keystroke stamp (`None` when the pty is gone), and
/// `episode_started_ms` the hold episode's start ([`HoldEpisode`]).
///
/// **The comparison is against the episode, not against a fresh window**, and
/// that is why this needs no new constant. The question worth answering is not
/// "did a human type recently" — recently against what? — but "has a human
/// touched this pane at any point in the whole time it has been refusing our
/// delivery". A stamp older than the episode is a fact about some earlier
/// session at this pane and says nothing about what is in the box now.
///
/// Pure, so the precedence (dialog beats keystroke evidence beats its absence)
/// is directly pinnable — it is the one ordering a future edit must not swap.
pub fn undeliverable_cause(
    held: Option<HeldReason>,
    last_user_input_ms: Option<u64>,
    episode_started_ms: u64,
) -> UndeliverableCause {
    match held {
        None => UndeliverableCause::Unknown,
        Some(HeldReason::InteractiveQuestion) => UndeliverableCause::QuestionOnScreen,
        Some(HeldReason::Typing) | Some(HeldReason::BoxOccupied) => match last_user_input_ms {
            None => UndeliverableCause::Unknown,
            Some(ms) if ms >= episode_started_ms => UndeliverableCause::HumanTyping,
            Some(_) => UndeliverableCause::PaneMidTurn,
        },
    }
}

/// #590 L2: the orchestrator-facing notice for a pane that has been holding
/// loomux's own notice past the escalation bound.
///
/// **One line, marker-led**, like every other text that can reach
/// [`OrchNoticeInbox::park`] — see that method's `debug_assert`, and #621 for
/// why a row loomux writes must be one `mask_loomux_notices` can claim.
///
/// **What it says that `queue::still_queued_notice` does not**, which is the
/// whole reason it exists as a second notice rather than a reworded first one:
/// that the stuck payload is loomux's OWN notice (so an agent is waiting on
/// something it will never be told), and a cause for the hold. The still-queued
/// notice fires at 30 minutes and reports depth and elapsed time; #590's live
/// incident was diagnosed and cleared by a human at ~20, so that notice never
/// fired at all, and had it fired it would have said "nothing lost, delivers
/// automatically once clear" — true, and precisely the wrong thing to read
/// about a pane that cannot clear itself.
pub fn undeliverable_notice(
    agent_id: &str,
    notices: usize,
    depth: usize,
    minutes: u64,
    cause: UndeliverableCause,
) -> String {
    let subject = if notices == 1 {
        format!("1 of {depth} deliveries queued for {agent_id} is this app's own notice")
    } else {
        format!("{notices} of {depth} deliveries queued for {agent_id} are loomux's own notices")
    };
    format!(
        "{NOTICE_MARKER} notice undeliverable {minutes} min: {subject}, and the pane has \
         accepted nothing since — {}",
        cause.phrase()
    )
}

/// #578: how many parked notices one group's orchestrator inbox holds before
/// the oldest start being elided.
///
/// Sized for a burst, not a backlog. The inbox drains on the orchestrator's
/// very next MCP tool call, so a group that is past this number has an
/// orchestrator that has not called a tool in a long time — at which point the
/// twentieth "your queue is backed up" tells a reader nothing the first one
/// did, and every one of them is in `audit.jsonl` verbatim regardless. Bounded
/// because this map is written by loomux's own delivery paths and read by an
/// agent whose context is the scarce resource: an unbounded relay would be a
/// memory leak at one end and a context bomb at the other.
pub const ORCH_NOTICE_INBOX_MAX: usize = 20;

/// #578: how much of a suppressed notice's text the `notice-suppressed` audit
/// line carries. Matches the `tool-result` line's own cap — the notices are
/// one-liners, and the cap exists so a future caller passing something large
/// cannot bloat the log.
pub(in crate::orchestration) const NOTICE_AUDIT_TEXT_CAP: usize = 500;

/// #578: one group's parked orchestrator-target queue notices — the durable
/// half of [`HoldChannel::OrchestratorInbox`].
#[derive(Default, Debug, Clone)]
pub struct OrchNoticeInbox {
    /// Oldest first, capped at [`ORCH_NOTICE_INBOX_MAX`].
    pub notices: Vec<String>,
    /// How many the cap pushed out. **Counted, not forgotten**: a relay that
    /// silently held back N notices would read as complete while being
    /// short — the exact "looks complete, isn't" defect the whole
    /// #445/#467/#563 lineage exists to eliminate.
    pub elided: usize,
}

impl OrchNoticeInbox {
    /// Park one notice, evicting the oldest (and counting it) past the cap.
    /// Oldest-first eviction on purpose: the newest notice is the one whose
    /// claim is still true — a `dropped_notice` from ten minutes ago has been
    /// superseded by whatever the queue did since.
    ///
    /// **The maskability invariant is checked at the door** (#576/#621, review
    /// NB3). [`orch_notice_relay_text`] can only keep every row maskable if
    /// every notice it is given is itself one marker-led line; a caller passing
    /// a multi-line or unprefixed string would produce rows
    /// [`mask_loomux_notices`] cannot claim, and the failure would surface in
    /// the rendered block, far from the call site that caused it. Asserted
    /// through `mask_loomux_notices` itself rather than by re-deriving the
    /// rule, so the check cannot drift from what it is standing in for.
    /// `debug_assert` because CI's test builds are debug (so it fires where a
    /// mistake is introduced) while a release build never panics a live session
    /// over it — the degraded outcome is a gate that holds too long, which
    /// `QuestionStale` already reports.
    pub fn park(&mut self, text: &str) {
        debug_assert!(
            mask_loomux_notices(text).is_empty(),
            "a parked queue notice must be a single {NOTICE_MARKER}-led line or the relay \
             block stops being maskable (#576/#621) — got {text:?}"
        );
        self.notices.push(text.to_string());
        if self.notices.len() > ORCH_NOTICE_INBOX_MAX {
            let overflow = self.notices.len() - ORCH_NOTICE_INBOX_MAX;
            self.notices.drain(..overflow);
            self.elided += overflow;
        }
    }
}

/// #578: render a group's parked notices as the extra MCP content block an
/// orchestrator's next tool result carries back. `None` when there is nothing
/// to say — the ordinary case, and the one that must add not a single byte to
/// a tool result.
///
/// Pure so the copy is testable without a dispatch harness, the way every
/// other notice string in this codebase is.
///
/// **The wording has one job beyond informing.** It must stop the orchestrator
/// from "helpfully" re-sending anything: the payloads these notices describe
/// are either already queued and delivering (`queued_notice`) or already gone
/// (`dropped_notice`), and a re-send is a duplicate in the first case and a
/// guess in the second. It also says plainly that this is its OWN pane, since
/// every other `[orrerix]` notice an orchestrator reads is about somebody else.
///
/// **Every row stays maskable (#576/#621).** This block rides an MCP tool
/// result, never the pty, so no reader of a live pane sees it directly. But
/// [`mask_loomux_notices`]'s own argument covers the path that puts it in a
/// pane anyway — an agent can print marker text itself — and an orchestrator
/// quoting its relay back into a summary would leave text *about* a question
/// in the tail of the pane most exposed to #576's self-latch. So the header
/// leads with [`NOTICE_MARKER`], every constituent notice already does,
/// and the two rows that would not (the bullet and the elision line) are
/// shaped so `deframe` still finds the marker leading them. A `-` bullet is
/// the specific thing that breaks this, since `deframe` does not strip it.
pub fn orch_notice_relay_text(notices: &[String], elided: usize) -> Option<String> {
    if notices.is_empty() {
        return None;
    }
    let n = notices.len();
    let mut out = format!(
        "[orrerix] {n} queue notice{s} about YOUR OWN pane could not be delivered to you as a \
         prompt — a delivery announcing your pane's blocked delivery would queue behind the very \
         block it reports (#578). Relayed here instead, riding back on a call you just made. \
         Nothing needs acknowledging and nothing needs re-sending:",
        s = if n == 1 { "" } else { "s" }
    );
    for t in notices {
        // `•`, not `-`, and the elision line below carries the marker rather
        // than opening with prose (#576/#621). Every row of this block has to
        // stay maskable by `mask_loomux_notices`, which drops a row that LEADS
        // with `NOTICE_MARKER` once `deframe`d — and `deframe` strips
        // whitespace and `│ ┃ | * ● • ◆`, but NOT `-`. This block never reaches
        // a pane on its own, but #621's own argument covers the path that puts
        // it there: an agent can print marker text itself, and an orchestrator
        // quoting its own relay into a summary would otherwise leave text
        // ABOUT a question sitting in the tail of the pane most exposed to
        // #576's self-latch.
        out.push_str("\n  • ");
        out.push_str(t);
    }
    if elided > 0 {
        out.push_str(&format!(
            "\n  • {NOTICE_MARKER} plus {elided} earlier notice{s} elided (this relay \
             holds {ORCH_NOTICE_INBOX_MAX}) — every one of them is in this group's audit.jsonl \
             as a `notice-suppressed` line.",
            s = if elided == 1 { "" } else { "s" }
        ));
    }
    Some(out)
}

/// Human-readable "what's held and why" line for a delivery-held badge/toast
/// (#246). Pure so the copy is unit-testable; callers pair it with
/// `delivery_held_event`'s payload.
pub fn delivery_held_detail(agent_id: &str, reason: HeldReason) -> String {
    match reason {
        HeldReason::Typing => format!(
            "Prompt delivery to {agent_id} is paused — you're typing in this pane."
        ),
        HeldReason::BoxOccupied => format!(
            "Prompt delivery to {agent_id} is paused — submit or clear the text in this pane's box to continue."
        ),
        HeldReason::InteractiveQuestion => format!(
            "Prompt delivery to {agent_id} is paused — an interactive question is on screen, answer it to release delivery."
        ),
    }
}

/// Build the `orch-delivery-held` event payload (#246): pushed the moment a
/// delivery to `agent_id` starts waiting on human input in `pty_id`'s box.
/// Pure so the shape is unit-testable without a harness that can capture an
/// actually-emitted Tauri event (see `channel_connected_event`'s doc for why
/// this codebase always factors payload construction out this way).
pub fn delivery_held_event(agent_id: &str, group: &GroupId, pty_id: u32, reason: HeldReason) -> Value {
    json!({
        "agent_id": agent_id, "group": group, "pty_id": pty_id,
        "reason": reason.as_str(), "detail": delivery_held_detail(agent_id, reason),
    })
}

/// Build the `orch-delivery-held-cleared` event payload (#246): pushed the
/// instant a hold resolves, whether the prompt then delivered or the delivery
/// aborted — either way nothing is held on `pty_id` anymore, so the frontend
/// badge for it drops.
pub fn delivery_held_cleared_event(pty_id: u32) -> Value {
    json!({ "pty_id": pty_id })
}
