//! Unconfirmed deliveries: the disposition of one, the late monitor, and the
//! unconfirmed, eaten and confirmed-late notices.
//! Design note: `docs/design/delivery-triage.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. Sibling files it
//! calls: `refusals.rs`, `submit.rs`, `tier1.rs`.

use super::*;
/// #539 (rev-13 N3): how many delivery ids the coalesced notice names before
/// it summarizes the rest. Deliberately the SAME number as
/// `PAUSE_SUPPRESSION_LIST_MAX`, whose doc makes the argument this borrows: a
/// notice is itself a delivery pasted into a pane, so a list it cannot bound
/// is a paste it cannot bound. The audit log holds every id either way
/// (`delivery-unconfirmed-notice` records the full `delivery_ids` array), so
/// nothing is lost by capping the pasted copy.
pub const UNCONFIRMED_NOTICE_IDS_MAX: usize = PAUSE_SUPPRESSION_LIST_MAX;

/// What a `DeclareFailed` tick should actually DO about the pane (#522).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnconfirmedDisposition {
    /// The pane is structurally idle — nothing of ours is sitting in the box,
    /// nothing of the human's is either, AND the pane produced a turn's worth
    /// of output since our Enter (#585). Record it and stop; do NOT tell the
    /// orchestrator to go re-send something that is not stuck.
    IdleAuditOnly,
    /// #585: the same empty, quiet box — but with NO turn evidence behind it.
    /// The pane never ran a turn on our text and our text is not there to run,
    /// which is the eaten-paste signature, not a finished pane. Announce it
    /// (with `delivery_eaten_notice`'s wording, not the "sitting unsubmitted"
    /// one — the text is gone, not stuck) and let the caller fall through to
    /// the strand path so the badge and `kickoff_recovery_action` are reached.
    EatenNotify,
    /// #539: the box was observed to no longer hold our text AND the agent's
    /// own process reached loomux after this delivery settled. The pane is not
    /// idle — it is working (and #585's turn evidence agrees: the pane painted
    /// a turn's worth of output since our Enter) — and the only thing still
    /// arguing for an alarm is `box_pending`, a signal known to latch true
    /// over an empty box. Reachable ONLY from the `box_pending` cell: an empty
    /// box with no human text is #585's territory, and activity must never
    /// silence an eaten delivery there. Record
    /// it and stop, but under its own name: "we watched the pane go quiet with
    /// an empty box" and "we watched the agent act" are different facts, and a
    /// timeline that showed them as one could not tell whether #539's evidence
    /// is doing any work.
    ActiveAuditOnly,
    /// Our text is still in the box, or the reading is indeterminate. This is
    /// the genuine strand the notice exists for — announce it unchanged.
    Notify,
}

/// What the `DeclareFailed` arm does once it has classified the pane (#585) —
/// the arm's own PRECEDENCE, lifted out of control flow and into a value.
///
/// **Why this exists at all.** #585 was not a wrong decision; it was a
/// correctly-decided one that a `return` statement preempted. The idle-pane
/// arm stopped the monitor ~90 lines above the test gating #517's kickoff
/// recovery, so the recovery never ran — and the entire test suite was green
/// throughout, because the ordering existed only as control flow inside a
/// function no test can execute (`late_monitor` needs a live `AppHandle` and
/// a real `PtyManager`).
///
/// #517's own wiring test shows the failure mode exactly: it hand-composes
/// `stranded_selfheal_action` and then `kickoff_recovery_action` in the order
/// its author believed the monitor used, and passes — asserting the intended
/// composition rather than the shipped one. A test that IS the composition
/// cannot observe that the real composition differs. That is the
/// "unpinnable wiring nobody could assert a property against" hazard this file
/// names elsewhere, and it cost this project two releases of a dead feature.
///
/// Making the precedence a value does not make `late_monitor` executable in a
/// test, and this is deliberately not claimed to: the arm could still be
/// mis-wired to ignore the route it computes. What it does buy is that the
/// ORDERING — "an eaten paste reaches the recovery; an idle pane does not" —
/// is now a property a test reads off a pure function instead of a property a
/// reader has to re-derive by tracing `return`s. The one thing that silently
/// regressed is the one thing now pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedArmRoute {
    /// Record the idle pane and stop. Nothing is stranded, nothing is owed.
    QuietStop,
    /// Announce, badge, and consult `kickoff_recovery_action`. `eaten`
    /// selects the notice wording only (see `delivery_eaten_notice`); both
    /// values take the identical path, which is the point — an eaten delivery
    /// must not be routed anywhere a merely-unconfirmed one is not.
    Escalate { eaten: bool },
}

/// The `DeclareFailed` arm's precedence (#585). See `FailedArmRoute`.
///
/// Total over `UnconfirmedDisposition`, so adding a variant without deciding
/// its route is a compile error rather than a silent fall-through into
/// whichever branch happens to sit first.
pub fn failed_arm_route(disposition: UnconfirmedDisposition) -> FailedArmRoute {
    match disposition {
        UnconfirmedDisposition::IdleAuditOnly => FailedArmRoute::QuietStop,
        // #539: silent for the same reason `IdleAuditOnly` is — the pane is
        // demonstrably not stranded — but reached on different evidence, so it
        // keeps its own audit action at the call site. The totality of this
        // match is what forced the new variant to be routed consciously
        // instead of falling through a wildcard into an escalation.
        UnconfirmedDisposition::ActiveAuditOnly => FailedArmRoute::QuietStop,
        UnconfirmedDisposition::EatenNotify => FailedArmRoute::Escalate { eaten: true },
        UnconfirmedDisposition::Notify => FailedArmRoute::Escalate { eaten: false },
    }
}

/// Should a late-confirmation failure raise the actionable "the prompt may be
/// sitting unsubmitted in its pane; get_output it and re-send" notice? (#522)
///
/// The notice was firing on panes that were simply DONE: a worker finished its
/// turn, went idle at the CLI's rest prompt, and the monitor — which only
/// knows that no `PromptSubmit` hook record ever showed up — announced a
/// strand that did not exist. Copilot has no such hook at all, and a claude
/// pane can miss one, so "no hook record" is a weak signal on its own. The
/// cost is not merely noise: every one of these tells the orchestrator to
/// `get_output` the pane (the #520 token flood on an animated pane) and tempts
/// a duplicate re-send — the double-delivery loop #451/#510 exist to prevent.
///
/// The disambiguation is STRUCTURAL, deliberately reusing the seams that
/// already exist rather than inferring anything from output bytes — the same
/// discipline #518's origin bit follows:
/// - `reading` — is OUR pasted text still identifiably at the box's
///   tail end? That, and only that, is what "sitting unsubmitted" means.
///   #559 made this a `BoxReading` rather than a bool: `Unverifiable` — the
///   tail we could read is shorter than the paste we are looking for — is
///   NOT an idle pane. It is no reading at all, and the whole point of this
///   function is to withhold an alarm only when the pane was actually
///   observed to be at rest. Treating a foregone `false` as an observation
///   is how a coalesced flush over the scan window went silent (see
///   `BoxReading`'s doc).
/// - `box_pending` — `PtyManager::input_pending`, whether any human-typed
///   characters are outstanding. A box with a person's half-written line in it
///   is not an idle pane, and the human still needs to hear about it.
/// - `turn_evidence` (#585) — did the pane produce `KICKOFF_TURN_EVIDENCE_
///   BYTES` of output since this delivery's own Enter? See below; this is the
///   input whose absence made every #517 recovery unreachable.
///
/// **#585: an empty box is two different panes, and #522 only modelled one.**
/// `NotHolding` on a quiet pane has two causes that are indistinguishable from
/// the reading alone:
/// - the CLI consumed our text, ran its turn, and came back to rest — #522's
///   case, where an alarm is noise; and
/// - the paste was never accepted at all (a blind paste into a CLI whose stdin
///   reader had not attached, or an Enter that landed on a busy CLI and was
///   dropped). The box is equally empty, and the delivery is equally gone —
///   except this one is a LOST message, and staying silent about it strands
///   the agent.
///
/// Live evidence (#585): across this project's whole recorded audit history,
/// `kickoff_recovery_action` — the #517/#526 recovery written specifically for
/// the second case — fired **zero** times and declined-with-a-reason zero
/// times, against 13 `delivery-unconfirmed-idle-pane` records and 3 fresh
/// kickoffs whose submit was never confirmed. One agent sat idle for 11
/// minutes on a kickoff nobody knew was lost. The reason is precedence, not
/// logic: this function's `IdleAuditOnly` arm returns from `DeclareFailed`
/// ~90 lines ABOVE the `NotHolding` test that gates the recovery, so the
/// recovery was reachable only when `NotHolding` coincided with a human's
/// half-typed line (`box_pending`) or with `Unverifiable`. It fired on a
/// coincidence, which is to say it did not fire. That is the same shape #559
/// fixed for `Unverifiable` — and #559 fixed only the lower half of it.
///
/// **The discriminator already existed; it was simply never consulted here.**
/// A pane that really did run a turn painted kilobytes since our Enter (the
/// monitor only looks after `PENDING_IDLE_QUIET` of total silence, so a turn
/// that happened has long since finished painting). An eaten paste leaves a
/// stray Enter on an empty box: a repaint, nothing more. That is exactly
/// `KICKOFF_TURN_EVIDENCE_BYTES`, and `kickoff_recovery_action` — 90 lines
/// below — already trusts it to gate the far more dangerous decision of
/// re-sending text into a live pane. **If the bar is good enough to authorise
/// a re-delivery, it is more than good enough to authorise a notice.** This is
/// not a weakening of #526's evidence bar and not a second mechanism beside
/// it: it is the same bar, newly applied to the path that was bypassing it.
///
/// **What this does NOT re-open.** #522's flood stays suppressed where #522
/// aimed it: a worker that finished a turn and went idle has turn evidence by
/// construction, so it still takes `IdleAuditOnly` and still says nothing. The
/// only deliveries that newly speak up are ones where the pane produced less
/// than a turn's output since our Enter AND the box no longer holds our
/// text — which is not a finished pane under any reading.
///
/// - `agent_acted` (#539) — `agent_acted_since(last_mcp_activity_ms,
///   submit_sent_ms + UNCONFIRMED_ACK_SETTLE_MS)`: the agent's OWN process
///   authenticated with its token and invoked a loomux tool after this
///   delivery settled. #535 built this clock and named this function as its
///   intended second consumer; until then the detector had no evidence about
///   the agent at all, only about the box, which is why panes that had
///   reported and pushed within the same minute still drew the alarm.
/// - `kickoff_recoverable` (#539) — whether this delivery is a fresh spawn's
///   kickoff that #517 could still re-deliver.
///
/// **The merged table (#585 + #539).** These two landed against the same
/// function within hours of each other and both split the `NotHolding` row, so
/// the composition is stated here in full rather than left to be inferred from
/// match order:
///
/// | `reading` | `box_pending` | `turn_evidence` | `agent_acted` | `kickoff_recoverable` | result |
/// |---|---|---|---|---|---|
/// | `Holds` | any | any | any | any | `Notify` |
/// | `Unverifiable` | any | any | any | any | `Notify` |
/// | `NotHolding` | `true` | `true` | `true` | `false` | `ActiveAuditOnly` (#539) |
/// | `NotHolding` | `true` | `true` | `true` | `true` | `Notify` (kickoff veto) |
/// | `NotHolding` | `true` | otherwise | | | `Notify` |
/// | `NotHolding` | `false` | `true` | — | — | `IdleAuditOnly` (#585) |
/// | `NotHolding` | `false` | `false` | — | — | `EatenNotify` (#585) |
///
/// The `—` are not shorthand for "any": `agent_acted` is **structurally
/// absent** from the `box_pending: false` arms, and that is the load-bearing
/// half of this merge.
///
/// **Why AND, not OR — correcting this doc's own earlier prediction.** #585
/// shipped anticipating that #539 would compose as *evidence-OR* ("any
/// independent sign that the agent acted on our delivery justifies silence").
/// That is not what shipped, and the difference matters in exactly the cell
/// #585 exists to protect. An MCP call is **not** a sign that the agent acted
/// on *our delivery* — it is a sign the agent's process is alive. An agent
/// whose paste was eaten still calls loomux: every role's instructions end
/// with "if you have no task yet, report progress and wait". So under OR, a
/// lost kickoff on a pane with no turn evidence would be silenced by the very
/// report that proves the agent never got its brief — re-deading the recovery
/// #585 had just un-deaded, from a different input.
///
/// Hence two rules, both restrictions rather than extensions:
/// - `agent_acted` is read **only** in the `box_pending` cell. The
///   `IdleAuditOnly`/`EatenNotify` split below is #585's alone.
/// - Inside that cell it must be accompanied by `turn_evidence`. Silence
///   requires the pane to have *painted a turn since our Enter* AND the agent
///   to have *reached loomux after it settled* — two independent post-submit
///   observations, neither of which the other can manufacture. The panes this
///   was built for (#539's ~28 false alarms, agents that "had reported and
///   pushed within the same minute") satisfy both by construction, so the fix
///   loses nothing it was aimed at.
///
/// The honest summary is that the two changes compose as evidence-AND within
/// one cell and as disjoint ownership across the rest of the row — which is
/// stricter than either author predicted alone, and strictly safer than both.
///
/// "CLI at rest / turn complete" is the third condition #522 names, and it is
/// already a PRECONDITION of ever reaching this decision rather than an input
/// here: `late_monitor_tick` returns `DeclareFailed` only when
/// `quiet_long_enough` (no `output_total` growth for `PENDING_IDLE_QUIET`) and
/// no question is on screen. A pane mid-turn keeps `output_total` creeping —
/// spinner and statusline frames included (#480) — so it never gets this far.
/// Taking it as an input anyway would let a caller pass `true` for a pane that
/// is demonstrably busy, inventing a state the surrounding code cannot
/// produce.
///
/// **Precedence, and what #539 deliberately does NOT touch.** Activity is
/// added as evidence; it never overrides evidence that points the other way:
///
/// | `reading` | `box_pending` | `agent_acted` | result |
/// |---|---|---|---|
/// | `Holds` | any | any | `Notify` |
/// | `Unverifiable` | any | any | `Notify` |
/// | `NotHolding` | `true` | `true` | `ActiveAuditOnly` (#539) |
/// | `NotHolding` | `true` | `false` | `Notify` |
/// | `NotHolding` | `false` | any | `IdleAuditOnly` |
///
/// - `Holds` is a direct observation that our text is sitting unsubmitted.
///   An agent can be busy for reasons that have nothing to do with our paste,
///   so liveness must not silence a strand we can see.
/// - `Unverifiable` is #559's honest-uncertainty arm and stays untouched. We
///   have no box reading at all there, and activity is not a reading of the
///   box: suppressing on it would re-create exactly the defect #559 fixed
///   (silence drawn from an answer the pane was never consulted about), just
///   sourced from a different signal.
/// - So activity governs precisely ONE cell — the one where the box was
///   observed to have lost our text and the only remaining argument for an
///   alarm is `box_pending`, i.e. `PtyManager::input_pending`, which is known
///   to latch >0 over an empty box (bare ESC, a TUI line-clear, a CLI
///   consuming the line — see the design note's input-origin section). Two
///   independent readings — the box no longer holds our text, and the agent
///   itself reached loomux afterwards — is the same "release on a second,
///   independent reading" shape #518 used, not a timer.
///
/// **The suppression is bounded by construction** (`.loomux/lessons.md`: any
/// suppression driven by a fallible signal must be BOUNDED). It is not a hold
/// that waits for a condition to clear, so there is no "what if the signal is
/// wrong and never clears" state to get stuck in: it is a one-shot decision
/// taken against a POSITIVE stamp that must already exist. No stamp, no
/// suppression — a pane with no activity signal keeps its pre-#539 behaviour
/// exactly. The activity evidence IS the bound.
///
/// **Why `kickoff_recoverable` vetoes the suppression.** #517's lost-kickoff
/// re-delivery is reached through the notify path on a `NotHolding` reading,
/// and a spawn whose brief never arrived is precisely an agent that will call
/// a loomux tool anyway: every role's instructions end with "if you have no
/// task yet, report progress and wait". So on a kickoff, an activity stamp is
/// as consistent with "the brief was eaten and the agent announced itself
/// idle" as with "the brief landed" — the one case where this evidence points
/// at nothing. Rather than let it silence the only recovery a fresh spawn has,
/// a recoverable kickoff always takes the unchanged path.
///
/// Note what this does NOT do: it does not mark the delivery confirmed. An
/// idle-pane reading is good enough to withhold an alarm, not to assert
/// something landed, and leaving the ledger `unconfirmed` keeps every
/// downstream behaviour exactly as it was — including the next delivery's
/// stranded flush, whose own doc already blesses this residual ("A false
/// 'unconfirmed' here is safe: the flush Enter lands on an already empty box
/// and is a no-op").
#[doc(hidden)] // pub for integration tests
pub fn unconfirmed_disposition(
    reading: BoxReading,
    box_pending: bool,
    turn_evidence: bool,
    agent_acted: bool,
    kickoff_recoverable: bool,
) -> UnconfirmedDisposition {
    match reading {
        // Our text is still in the box — the genuine strand.
        BoxReading::Holds => UnconfirmedDisposition::Notify,
        // #559: no reading. Silence here would be a claim ("the pane is
        // idle") drawn from an answer that was fixed before the pane was
        // consulted. #539 does not weaken this: activity is evidence about
        // the agent, never about the box.
        BoxReading::Unverifiable => UnconfirmedDisposition::Notify,
        // Observed empty of our text — idle only if it is empty of the
        // human's too.
        // #539 + #585, merged deliberately (see this function's doc). The
        // human-input cell is the ONLY one activity governs, and it now
        // requires turn evidence as well — see "Why AND, not OR" in the doc.
        BoxReading::NotHolding if box_pending => {
            if agent_acted && turn_evidence && !kickoff_recoverable {
                UnconfirmedDisposition::ActiveAuditOnly
            } else {
                UnconfirmedDisposition::Notify
            }
        }
        // #585: and only if the pane can show it actually ran a turn. This is
        // the cell that swallowed every lost kickoff this feature exists to
        // catch — see this function's doc. #539 does NOT reach past here:
        // `agent_acted` is deliberately absent from both arms below, because
        // an agent that calls a tool while our paste is missing is the
        // SYMPTOM of the eaten delivery, not evidence against it.
        BoxReading::NotHolding if turn_evidence => UnconfirmedDisposition::IdleAuditOnly,
        BoxReading::NotHolding => UnconfirmedDisposition::EatenNotify,
    }
}

/// #112 round 3 (rev-20 B2 + B3): the ONE decision point for whether Tier
/// 1's accumulated reading may become an authoritative veto
/// (`ConfirmSource::BoxVeto`) at the end of the confirm+retry window. Pure
/// so the polarity — arguably the single most important property in this
/// redesign — is directly pinnable rather than an inline `if` a future edit
/// could silently invert (exactly the shape round 1 shipped and failed
/// live: unpinnable wiring nobody could assert a property against).
///
/// A veto requires ALL of:
/// - nothing else already decided this delivery (`confirm_source ==
///   ConfirmSource::None`);
/// - Tier 1 governs this delivery at all (`tier1_governs`);
/// - Tier 1's own reading, at the end, was "still holding"
///   (`tier1_reading == Some(true)`);
/// - the confirm+retry window ran to NATURAL exhaustion —
///   `window_exhausted_naturally`. An early exit for a question on screen,
///   a human typing, or a failed retry write all mean the box's state
///   cannot be trusted as evidence of NON-acceptance: a question may be
///   intercepting Enter rather than refusing it, and a human mid-edit means
///   the box's contents aren't about our delivery either way. B2's fix:
///   ANY early exit leaves the delivery `Pending`, never `Failed` — the
///   question-guarded late monitor (`late_monitor_tick`) is what's allowed
///   to decide from there, because unlike this one-shot end-of-window
///   check, it re-observes the question state on every subsequent poll;
/// - Tier 1 is still trusted at the end (`tier1_trusted_at_end` —
///   B3's fix: no human input since our own submit).
///
/// Never returns anything OTHER than the input `confirm_source` unchanged
/// when any condition fails — this function can only ever produce
/// `BoxVeto`, never invent a different outcome or downgrade an existing
/// one.
#[doc(hidden)] // pub for integration tests
pub fn final_window_outcome(
    confirm_source: ConfirmSource,
    tier1_governs: bool,
    tier1_reading: Option<bool>,
    window_exhausted_naturally: bool,
    tier1_trusted_at_end: bool,
) -> ConfirmSource {
    if matches!(confirm_source, ConfirmSource::None)
        && tier1_governs
        && tier1_reading == Some(true)
        && window_exhausted_naturally
        && tier1_trusted_at_end
    {
        ConfirmSource::BoxVeto
    } else {
        confirm_source
    }
}

/// #112 round 3 (rev-20 B1): one tick's decision for the late-confirmation
/// monitor — pure, so the precedence is directly pinnable rather than an
/// inline `if`/`continue` chain a future edit could silently reorder.
///
/// - `Superseded` — a NEWER delivery to the same pane has recorded its own
///   `DeliveryOutcome` since this monitor started (`submit_sent_ms` no
///   longer matches what's in `last_delivery`): this monitor no longer owns
///   anything and must exit WITHOUT writing or notifying — that's precisely
///   the clobber/false-correction hazard (rev-20 B1) this variant exists to
///   prevent. Checked first: even a hook match arriving on a superseded
///   monitor's tick would be writing a confirmation for the WRONG delivery.
/// - `Confirm` — a `promptsubmit` match arrived. `correction: true` when
///   `already_failed` (this is upgrading an alarm that already fired, so
///   the orchestrator needs the correction notice, not a second success
///   notice); checked before `Expired` so a match on the very last tick
///   before the cap still resolves the delivery rather than timing out.
/// - `Expired` — the lifetime cap was hit with nothing resolved.
/// - `KeepWaiting` — nothing to act on: either already `failed` (only a
///   hook match, handled above, has anything left to do) or not yet quiet
///   long enough / a question is on screen.
/// - `DeclareFailed` — genuinely idle, no question, not yet failed: the
///   ONE point this delivery is ever declared `Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)] // pub for integration tests
pub enum MonitorAction {
    Superseded,
    Expired,
    Confirm { merged: bool, correction: bool },
    DeclareFailed,
    KeepWaiting,
}

#[doc(hidden)] // pub for integration tests
pub fn late_monitor_tick(
    superseded: bool,
    hook_match: PromptLandedMatch,
    already_failed: bool,
    expired: bool,
    quiet_long_enough: bool,
    showing_question: bool,
) -> MonitorAction {
    if superseded {
        return MonitorAction::Superseded;
    }
    if !matches!(hook_match, PromptLandedMatch::None) {
        let merged = matches!(hook_match, PromptLandedMatch::Content { merged: true });
        return MonitorAction::Confirm { merged, correction: already_failed };
    }
    if expired {
        return MonitorAction::Expired;
    }
    if already_failed {
        return MonitorAction::KeepWaiting;
    }
    if quiet_long_enough && !showing_question {
        return MonitorAction::DeclareFailed;
    }
    MonitorAction::KeepWaiting
}

/// Whether an unconfirmed delivery should raise a one-shot notice to the group's
/// orchestrator so it can close the loop (#103). Fires only for a delivery to a
/// NON-orchestrator agent whose submit went unconfirmed: the prompt may be
/// sitting unsubmitted in the pane while the orchestrator, believing it landed,
/// is none the wiser. Suppressed when the target IS the orchestrator — a notice
/// about a delivery to the orchestrator would itself be a delivery to the
/// orchestrator, an endless loop; those rely on #99's stranded-text flush on the
/// next delivery instead. Suppressed when confirmed: the prompt landed, nothing
/// to chase. Pure so the gate is testable; emission (exactly once per delivery,
/// past the submit retries) lives in `deliver_prompt`'s delivery thread.
pub fn should_notify_unconfirmed(target_is_orchestrator: bool, confirmed: bool) -> bool {
    !target_is_orchestrator && !confirmed
}

/// The notice delivered to the orchestrator for the unconfirmed deliveries to
/// `agent_id` buffered by one coalescing window (#103, coalesced in #539).
///
/// **Why it names ids.** Pre-#539 this was one notice per delivery and the
/// orchestrator could tell them apart only by arrival order. Coalescing makes
/// that impossible — several alarms arrive as one line — so each constituent
/// is named. `delivery_ids` are `submit_sent_ms` stamps: the identity the
/// delivery ledger and `late_monitor_tick`'s supersession check already key on
/// (no two deliveries to a pane share one), and the only id EVERY delivery
/// has, queued or straight through the front door. The same value is written
/// as `delivery_id` on this pane's `delivery-failed-idle` /
/// `delivery-unconfirmed-*` audit lines, so a reader can resolve any id in
/// this notice back to the record it came from.
///
/// The verb agrees with the count, per #533's `flush_header_text` — every one
/// of these lines is read by an agent as an instruction, and one that doesn't
/// parse is one more reason to skim it. The plural also changes the ASK: with
/// several deliveries in play the recovery is ONE `get_output`, not one per
/// id, which is the whole point of coalescing.
///
/// **The id list is capped** (rev-13 N3) at `UNCONFIRMED_NOTICE_IDS_MAX`, with
/// the remainder pointed at the audit log — which holds every id, since
/// `delivery-unconfirmed-notice` records the whole `delivery_ids` array. This
/// notice is itself a delivery pasted into a pane, and an uncapped list is the
/// shape `PAUSE_SUPPRESSION_LIST_MAX` and `dropped_payload_preview` were both
/// written for. Realistic batches are far below the cap; the cap exists so the
/// worst case is bounded rather than because the common case needs it.
///
/// An empty slice cannot reach here (`flush_unconfirmed_notices` returns
/// early on an empty bucket); it degrades to the plural wording with no ids
/// rather than panicking.
pub fn unconfirmed_delivery_notice(agent_id: &str, delivery_ids: &[u64]) -> String {
    let ids = notice_id_list(delivery_ids);
    if delivery_ids.len() == 1 {
        format!(
            "[orrerix] delivery to {agent_id} unconfirmed (id {ids}) — the prompt may be sitting \
             unsubmitted in its pane; get_output it and re-send if needed"
        )
    } else {
        format!(
            "[orrerix] {n} deliveries to {agent_id} unconfirmed (ids {ids}) — one or more prompts \
             may be sitting unsubmitted in its pane; get_output it ONCE and re-send whichever \
             did not land",
            n = delivery_ids.len()
        )
    }
}

/// The capped, comma-joined id list both coalesced notices render (#539,
/// rev-13 N3). Shared so `delivery_eaten_notice` and
/// `unconfirmed_delivery_notice` cannot drift into different caps — they are
/// two wordings of the same batch shape, and a cap that applied to one of them
/// would be a bound that looked enforced and was not.
fn notice_id_list(delivery_ids: &[u64]) -> String {
    let shown = delivery_ids.len().min(UNCONFIRMED_NOTICE_IDS_MAX);
    let mut ids = delivery_ids[..shown].iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
    if delivery_ids.len() > shown {
        ids.push_str(&format!(", and {} more — see the audit log", delivery_ids.len() - shown));
    }
    ids
}

/// #585: the notice for a delivery whose text was EATEN — the box was read and
/// observed to hold neither our paste nor anything of the human's, and the
/// pane produced no turn's worth of output since our Enter.
///
/// Deliberately worded apart from `unconfirmed_delivery_notice`, which tells
/// the orchestrator the prompt "may be sitting unsubmitted in its pane". Here
/// it demonstrably is not: we looked, and it is gone. Telling an orchestrator
/// to go find text that no longer exists sends it to `get_output`, where it
/// sees an idle pane and — reasonably — concludes nothing is wrong. That is
/// not hypothetical: it is exactly how #585's two live losses were misread,
/// and the pre-emptive re-sends it invites are the #455 duplicate-kickoff
/// class. `.loomux/lessons.md`, "a claim is a deliverable": the notice states
/// what was observed, and names the idle pane the orchestrator is about to see
/// so an idle pane is not mistaken for a refutation.
///
/// #539 coalesces these per pane like the unconfirmed ones, so it names every
/// id it stands for and agrees with its own count. The parenthetical is kept
/// verbatim in both forms: it is the part that stops an idle pane being read
/// as a refutation, and it is no less needed when several were lost at once.
pub fn delivery_eaten_notice(agent_id: &str, delivery_ids: &[u64]) -> String {
    let ids = notice_id_list(delivery_ids);
    if delivery_ids.len() == 1 {
        format!(
            "[orrerix] delivery to {agent_id} was LOST (id {ids}) — its text never reached the \
             pane's box and the pane ran no turn on it. Re-send it. (get_output will show an \
             idle pane: that is the symptom, not evidence the delivery landed.)"
        )
    } else {
        format!(
            "[orrerix] {n} deliveries to {agent_id} were LOST (ids {ids}) — their text never \
             reached the pane's box and the pane ran no turn on them. Re-send them. (get_output \
             will show an idle pane: that is the symptom, not evidence they landed.)",
            n = delivery_ids.len()
        )
    }
}

/// #112 round 2: the correction notice for a delivery that already drew
/// `unconfirmed_delivery_notice` but has since been proven to have landed
/// after all — a late `promptsubmit` hook record arrived after the `failed`
/// alarm fired (see `DeliveryConfirmState`'s doc: `Failed` is reachable via
/// `ConfirmSource::BoxVeto`, itself not infallible — the rejection/error
/// residual named in the design note — or via the idle-without-evidence
/// trigger, which by construction can never rule out a coverage gap in the
/// hook itself). Named as a correction, not a re-confirmation, so the
/// orchestrator reads it as "stand down, the earlier alarm was wrong" rather
/// than a second, redundant success notice.
pub fn delivery_confirmed_late_notice(agent_id: &str) -> String {
    format!(
        "[orrerix] correction: the earlier \"delivery to {agent_id} unconfirmed\" alarm was wrong — \
         a prompt-landed signal for that same delivery has now arrived. It landed; no re-send needed."
    )
}
