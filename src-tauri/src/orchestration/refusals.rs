//! Deliveries refused at the front door or suppressed during a pause, and the
//! refusal roster and notices built from the audit log.
//! Design note: `docs/design/delivery-triage.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. Sibling files it
//! calls: `auditlog.rs`, `noticemask.rs`.

use super::*;

/// #569: WHY one delivery a pause window lost is gone — the two are not the
/// same event and must not be reported as one (review B2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuppressedCause {
    /// A build predating #569 option 2 destroyed it at the pause branch
    /// (`prompt-suppressed-paused`). Nothing writes that action now, so this
    /// variant is reachable only by pausing under an older loomux, upgrading,
    /// and resuming — a real sequence, and the reason the scan is kept rather
    /// than deleted: those payloads genuinely are gone and this notice is the
    /// only thing that will ever say so.
    LegacyDiscard,
    /// THIS build refused it: the target pane was already at
    /// `queue::QUEUE_MAX_PER_PANE` when it arrived, so `enqueue_text`'s
    /// `RejectFull` arm dropped the payload and returned `Err`
    /// (`delivery-dropped`, `enqueue_reason: group-paused`).
    ///
    /// **The claim this variant exists to stop being false.** Option 2 was
    /// documented — here, in `pause_suppression_notice`, in `resume_group` and
    /// in the design note — as making a pause incapable of destroying a
    /// payload. It is not: the per-pane cap is 8, the orchestrator's pane is
    /// where a whole fleet converges, and loomux's own advisories now queue
    /// there too, so a long pause can fill it and refuse a worker's
    /// `report("done")` ninth. The sender is told (`Err`), but pre-B2 the
    /// ORCHESTRATOR never was, which is the #569 stall arriving through the
    /// queue instead of around it.
    QueueFullDuringPause,
}

impl SuppressedCause {
    /// Stable audit/report token.
    pub fn as_str(self) -> &'static str {
        match self {
            SuppressedCause::LegacyDiscard => "legacy-discard",
            SuppressedCause::QueueFullDuringPause => "queue-full-during-pause",
        }
    }
}

/// #569: one delivery a pause window lost, recovered from the audit log.
///
/// **Why there is no id here.** Neither source has a usable one. The pre-#569
/// pause branch returned *before* the front door, so no id was ever minted;
/// `enqueue_text`'s `RejectFull` arm mints none either, for the reason #563
/// gave when it met the same problem — a rejected entry has nothing to join
/// against. Both are named by `{from, to, preview}`, which the record does
/// establish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuppressedDelivery {
    /// Whoever called `deliver_prompt` — the audit entry's actor.
    pub from: String,
    /// The agent the payload was headed for.
    pub to: String,
    /// One-line, bounded preview of the lost payload
    /// (`queue::dropped_payload_preview`, reused so a lost payload reads the
    /// same wherever it is reported).
    pub preview: String,
    /// Which of the two ways it was lost — see [`SuppressedCause`]. The notice
    /// groups on this, because "an older build threw it away" and "this build
    /// refused it, and would refuse the next one too" call for different
    /// actions from the reader.
    pub cause: SuppressedCause,
}

/// #569: everything ONE pause window swallowed, oldest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PauseSuppression {
    pub items: Vec<SuppressedDelivery>,
    /// Whether the `group-pause` line that OPENED this window was still in the
    /// entries handed to [`suppressed_during_pause`]. False means the scan ran
    /// off the start of a timeline that is both rotated on disk and capped at
    /// `AUDIT_VIEW_LIMIT` by `audit_log`, so the list may reach back into an
    /// EARLIER pause — a caveat the notice states rather than swallows, since
    /// an over-count presented as exact is the same unbacked claim
    /// `.loomux/lessons.md` catalogues.
    pub window_start_seen: bool,
}

/// #569: read a pause window's discarded deliveries out of `entries`
/// (oldest-first audit timeline), scanning BACKWARDS from the end.
///
/// **Two sources, one window** (review B2). `prompt-suppressed-paused` is the
/// legacy discard; `delivery-dropped` carrying `enqueue_reason: group-paused`
/// is THIS build refusing an admission because the target pane was already at
/// `queue::QUEUE_MAX_PER_PANE`. Both destroy a payload inside a pause and
/// neither is otherwise shown to anyone, so both belong in the one notice the
/// resume already sends. The `enqueue_reason` filter is what keeps this to the
/// pause: `delivery-dropped` is written for ordinary queue-full rejections too,
/// and those are the sender's own synchronous `Err` to deal with, not something
/// a resume should re-report.
///
/// **Why `group-pause` alone bounds the window.** A `prompt-suppressed-paused`
/// line can only be written while the group is paused, and a `delivery-dropped`
/// line whose `enqueue_reason` is `group-paused` likewise, so any such line
/// after the most recent `group-pause` belongs to the window that line opened —
/// there is no intervening resume it could have survived. `group-resume` is
/// deliberately NOT a boundary: `create_group` audits that same action name
/// for a group RESTORED from disk (a different event with the same string), so
/// stopping on it would silently truncate a window that spanned an app
/// restart. `group-pause` has no such collision.
///
/// Pure: takes entries, returns a summary, touches no registry state — so the
/// window arithmetic is testable without a paused group or a filesystem.
pub fn suppressed_during_pause(entries: &[AuditEntry]) -> PauseSuppression {
    let mut items = Vec::new();
    let mut window_start_seen = false;
    for e in entries.iter().rev() {
        if e.action == "group-pause" {
            window_start_seen = true;
            break;
        }
        if e.action == "prompt-suppressed-paused" {
            items.push(SuppressedDelivery {
                from: e.actor.clone(),
                to: e.detail["to"].as_str().unwrap_or("?").to_string(),
                preview: queue::dropped_payload_preview(e.detail["text"].as_str().unwrap_or("")),
                cause: SuppressedCause::LegacyDiscard,
            });
            continue;
        }
        // A queue-full refusal of a PAUSE-held admission. `enqueue_text` writes
        // this line with the preview already bounded, so it is taken verbatim
        // rather than re-truncated; `from` is on the detail here (the actor is
        // `loomux`, which did the dropping, not the sender who lost the work).
        if e.action == "delivery-dropped"
            && e.detail["enqueue_reason"] == json!(queue::EnqueueReason::GroupPaused.as_str())
        {
            items.push(SuppressedDelivery {
                from: e.detail["from"].as_str().unwrap_or("?").to_string(),
                to: e.detail["to"].as_str().unwrap_or("?").to_string(),
                preview: e.detail["preview"].as_str().unwrap_or("").to_string(),
                cause: SuppressedCause::QueueFullDuringPause,
            });
        }
    }
    items.reverse(); // scanned newest-first; report in the order they arrived
    PauseSuppression { items, window_start_seen }
}

/// How many discarded deliveries the resume notice names individually before
/// it summarizes the rest. A long pause can swallow an unbounded number of
/// them and this notice is itself a delivery — one pasted into a pane — so the
/// list is capped and the remainder is pointed at the audit log, which holds
/// every payload IN FULL (`prompt-suppressed-paused` carries `text`, not a
/// preview).
pub const PAUSE_SUPPRESSION_LIST_MAX: usize = 8;

/// #569: the resume-time notice naming what a pause LOST — from either cause.
///
/// Reworked by option 2 rather than deleted, and corrected by review B2. The
/// wording has to do two jobs the audit lines cannot:
///
/// - **Say which pause it is talking about.** Both behaviors now coexist in a
///   group's history, and a reader who takes "will not be replayed" as the
///   current rule re-requests work that is already on its way — the duplicate
///   `queued_notice`'s "do NOT re-send" exists to prevent, arriving from the
///   opposite direction.
/// - **Not claim a pause can no longer destroy anything.** It can: a pane at
///   `queue::QUEUE_MAX_PER_PANE` refuses further admissions for as long as the
///   pause lasts, and a refusal is a payload that is gone. That half is not
///   history — it is a live property of this build, and it is the half a reader
///   can still act on, so it is stated in the present tense and its sentence
///   says what to do about it.
///
/// Pure so the copy is unit-testable, matching `queue::dropped_notice` /
/// `delivery_held_detail`.
pub fn pause_suppression_notice(s: &PauseSuppression) -> String {
    let n = s.items.len();
    let refused = s.items.iter().filter(|i| i.cause == SuppressedCause::QueueFullDuringPause).count();
    let legacy = n - refused;
    let (count, verb) = if n == 1 {
        ("1 delivery".to_string(), "was")
    } else {
        (format!("{n} deliveries"), "were")
    };
    let mut out = format!(
        "[orrerix] Group resumed — {count} {verb} LOST while this group was paused. Anything the \
         pause merely HELD is delivering on its own right now; do not re-request that. What is \
         listed below is not held, it is gone."
    );
    // Each cause gets its own sentence, and only the causes actually present:
    // a notice that explains a failure mode this window did not have is one
    // more paragraph between the reader and the one that matters.
    if legacy > 0 {
        out.push_str(
            " Some were DISCARDED by an EARLIER loomux version that did not queue while paused — \
             those are history and cannot recur on this build.",
        );
    }
    if refused > 0 {
        out.push_str(
            " Some were REFUSED by this build because the target pane's delivery queue was \
             already full (8 deep) — that is not history: a pane at capacity keeps refusing for \
             as long as a pause lasts, so if this group is paused again for a long stretch, \
             expect it again.",
        );
    }
    out.push_str(" Whatever you are still waiting on from these has to be re-requested. Lost:");
    for it in s.items.iter().take(PAUSE_SUPPRESSION_LIST_MAX) {
        // The cause is per-item because a single window can mix them, and
        // "which of these can happen to me again" is the reader's next question.
        let why = match it.cause {
            SuppressedCause::LegacyDiscard => "discarded by an earlier loomux",
            SuppressedCause::QueueFullDuringPause => "refused, queue full",
        };
        // #632: `  • ` + the marker, never the old bare `  - `. This notice is
        // an in-band DELIVERY — it rides the pty into an orchestrator's pane —
        // and every row loomux writes there has to be one
        // `mask_loomux_notices` can claim, or the item rows are text ABOUT a
        // question sitting in the tail of the pane most exposed to #576's
        // self-latch. `deframe` strips whitespace and `│ ┃ | * ● • ◆` but NOT
        // `-`, which is exactly why the bullet changed (the #624 convention).
        //
        // The preview is bounded AGENT text, and it is masked here rather than
        // left to latch because this row is loomux's own framing QUOTING a
        // payload — the #576 relay case exactly — not a rendered dialog. It is
        // re-collapsed through `dropped_payload_preview` (idempotent on
        // well-formed input) rather than trusted: it is read back out of a
        // durable `audit.jsonl` that an EARLIER loomux version may have
        // written, and a preview carrying a newline would split this into two
        // rows with only the first marker-led — #632 reintroduced from disk.
        out.push_str(&format!(
            "\n  • {NOTICE_MARKER} {} -> {} ({why}): {}",
            it.from,
            it.to,
            queue::dropped_payload_preview(&it.preview),
        ));
    }
    if n > PAUSE_SUPPRESSION_LIST_MAX {
        out.push_str(&format!(
            "\n  • {NOTICE_MARKER} ...and {} more — every lost payload is in this group's \
             audit log in full (actions `prompt-suppressed-paused` and `delivery-dropped`).",
            n - PAUSE_SUPPRESSION_LIST_MAX
        ));
    }
    if !s.window_start_seen {
        out.push_str(&format!(
            "\n  • {NOTICE_MARKER} (The `group-pause` line that opened this window is no \
             longer in the readable audit log, so this list may reach back into an earlier pause.)"
        ));
    }
    // The door for this producer (#632), the way `OrchNoticeInbox::park` is the
    // door for #624's single-line notices: asserted through the real mask, so a
    // later edit that adds an unmarked row fails where it is introduced rather
    // than in a pane. Debug only — CI's test builds are debug, and a release
    // build must never panic a live session over it; the degraded outcome is a
    // gate that holds too long, which `QuestionStale` already reports.
    debug_assert!(
        unmaskable_framing_rows(&out, &[]).is_empty(),
        "every row of the pause-suppression notice must be maskable (#632) — got leftovers \
         {:?} from {out:?}",
        unmaskable_framing_rows(&out, &[])
    );
    out
}

/// #579: what a front-door refusal was carrying — the two shapes
/// `queue-full-at-call` is written for, which need different advice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusedPayload {
    /// `enqueue_text`'s `RejectFull` arm: a text delivery. Its bytes are
    /// recoverable from the paired `prompt` line — see
    /// [`front_door_refusals`].
    Prompt,
    /// `audit_stranded_push`'s rejection: a `StrandedSubmit` marker, which
    /// never carried text at all (the bytes were already pasted into the
    /// pane; only the Enter was queued). Nothing to re-send — the pane needs
    /// an Enter, or its text re-pasted by hand.
    StrandedSubmit,
}

impl RefusedPayload {
    pub fn as_str(self) -> &'static str {
        match self {
            RefusedPayload::Prompt => "prompt",
            RefusedPayload::StrandedSubmit => "stranded-submit",
        }
    }
}

/// #633: WHY a delivery was refused at the front door — the discriminator that
/// turns [`front_door_refusals`] from a queue-full list into a refusal list.
///
/// **Why this exists at all.** #630 scanned for exactly one `delivery-dropped`
/// reason (`queue-full-at-call`) because it was the only refusal that wrote an
/// audit line. `deliver_prompt`'s two PRE-admission refusals — the target is
/// dead, the target has no terminal bound — wrote nothing, so no derivation
/// could ever surface them; #615 created the second of those by turning a silent
/// `Ok` into a silent `Err`, which is a strictly better contract for the sender
/// and no better at all for anyone reading the log afterwards. A refusal that
/// leaves no record cannot be enumerated by anything, which is the whole #579
/// class. So every refusal now writes a line, each under its own reason string,
/// and this enum is what a reader joins on.
///
/// **Every arm is a real, distinct instruction to whoever reads the row**, which
/// is why this is a typed discriminator rather than the raw string carried
/// through: a queue-full refusal says "the pane is busy, this may be worth
/// re-sending later"; a dead-target refusal says "re-target it, that pane is
/// gone"; a no-terminal refusal says "it was too early, send it again once the
/// pane binds". Collapsing them into one row shape would make the list
/// enumerable and still not actionable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalReason {
    /// `enqueue_text`'s `RejectFull` arm (#563/#579): the target pane's queue
    /// was already at `queue::QUEUE_MAX_PER_PANE`.
    QueueFull,
    /// `deliver_prompt_as` (#633): the target agent's status was
    /// [`AgentStatus::Dead`]. Refused BEFORE admission, so no id was minted and
    /// no queue was touched.
    AgentDead,
    /// `deliver_prompt_as` (#633): the target agent had no `pty_id` bound yet —
    /// a queue is keyed by pane, so there was nowhere to hold the payload.
    /// Created as an `Err` by #615 (it was a silent `Ok` before), audited by
    /// #633.
    NoTerminal,
    /// `withdraw_unprocessable` (#470): the admission succeeded and was then
    /// UNDONE because no `AppHandle` existed to ever drain it. Unreachable in
    /// production — see [`front_door_refusals`] for why it is surfaced anyway.
    NoAppHandle,
    /// `withdraw_unprocessable` (#470), the sibling case: an `AppHandle`
    /// existed but the registry was not held in an `Arc`, so no drainer could
    /// be spawned. Pre-#633 this wrote the `no-app-handle` reason too — one
    /// line claiming a cause that was not the one that fired.
    RegistryNotShared,
    /// `deliver_prompt_as` (#1161 M2): the target was this group's MANAGER and
    /// the delivery was not one of the two the no-injection guarantee permits.
    /// Refused BEFORE admission, like the two above, so no id was minted and no
    /// queue was touched.
    ///
    /// **Unlike every other reason here, this one is a POLICY refusal rather
    /// than a resource one**, and the difference matters to whoever reads the
    /// row: the other five say loomux could not deliver, this one says it will
    /// not. Nothing about the pane changes that — resuming it, freeing its
    /// queue and binding it a terminal all leave the answer the same.
    ManagerPane,
}

impl RefusalReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RefusalReason::QueueFull => "queue-full-at-call",
            RefusalReason::AgentDead => "agent-dead-at-call",
            RefusalReason::NoTerminal => "no-terminal-at-call",
            RefusalReason::NoAppHandle => "no-app-handle",
            RefusalReason::RegistryNotShared => "registry-not-shared",
            RefusalReason::ManagerPane => "manager-pane",
        }
    }

    /// Parse a `delivery-dropped` line's `reason` back into an arm — `None` for
    /// every reason that is NOT a front-door refusal, which is what makes
    /// [`front_door_refusals`]'s filter an enumeration rather than a guess.
    pub fn from_audit(reason: &str) -> Option<Self> {
        match reason {
            "queue-full-at-call" => Some(RefusalReason::QueueFull),
            "agent-dead-at-call" => Some(RefusalReason::AgentDead),
            "no-terminal-at-call" => Some(RefusalReason::NoTerminal),
            "no-app-handle" => Some(RefusalReason::NoAppHandle),
            "registry-not-shared" => Some(RefusalReason::RegistryNotShared),
            "manager-pane" => Some(RefusalReason::ManagerPane),
            _ => None,
        }
    }

    /// What this refusal means for the payload, in the reader's terms — carried
    /// on the audit line itself (`consequence`) so one string serves every
    /// channel, the same discipline the stranded-marker refusal already
    /// follows.
    ///
    /// `None` for [`RefusalReason::QueueFull`], and deliberately: that row
    /// already says what happened in fields the reader has (`queue_depth`,
    /// `enqueue_reason`) and at length in `queue_orphans`' own description, and
    /// its ONE case with a consequence — a refused `StrandedSubmit` marker,
    /// which leaves pasted-but-unsubmitted text in the pane — writes its own
    /// string from `audit_stranded_push` (#579). Restating that prose here
    /// would be a second copy of a sentence already maintained elsewhere, which
    /// is the failure mode `consequence` was introduced to avoid.
    pub(in crate::orchestration) fn consequence(self) -> Option<&'static str> {
        Some(match self {
            RefusalReason::QueueFull => return None,
            RefusalReason::AgentDead => {
                "the target agent was already dead — nothing was queued, and that pane will \
                 never take it; re-target this to a live or resumed agent"
            }
            RefusalReason::NoTerminal => {
                "the target agent had no terminal bound yet, so there was no queue to hold \
                 this — nothing was queued; re-send once the pane binds"
            }
            RefusalReason::NoAppHandle => {
                "loomux had no app handle to process this pane's queue, so the admission was \
                 withdrawn rather than left to strand — nothing is queued"
            }
            RefusalReason::RegistryNotShared => {
                "the registry was not shared, so no drainer could be started — the admission \
                 was withdrawn rather than left to strand; nothing is queued"
            }
            RefusalReason::ManagerPane => {
                "the target is the group's manager — the human's own pane, which takes no \
                 delivery from any agent; nothing was queued and nothing will be. Post status to \
                 message_manager, or put a decision to the human with ask_human"
            }
        })
    }
}

/// #579: one delivery REFUSED at the front door — the target pane's queue was
/// already at `queue::QUEUE_MAX_PER_PANE`, so nothing was ever queued.
///
/// **Why this is a separate type from [`queue::OrphanedQueueEntry`] rather than
/// that struct with an optional id.** A refused delivery never reached
/// `queue_seq.fetch_add`, so it has no id — and `OrphanedQueueEntry.id: u64` is
/// the wire shape of the `queue_orphans` MCP tool as well as the join key both
/// orphan derivations run on (`queue::merge_orphans` dedupes on it, the audit
/// scan opens and closes on it). Widening it to `Option<u64>` would make every
/// existing row's id nullable for the benefit of rows that can never have one,
/// and a synthetic id would be a number that joins against nothing while
/// looking like one that does. So refusals are surfaced as their own list, on
/// their own key — `{from, to, preview}`, the same naming [`SuppressedDelivery`]
/// settled on for the same reason. See `docs/design/orchestration.md`'s
/// "Front-door refusals (#579)".
///
/// The other half of that argument is behavioral: an orphan is a payload
/// loomux still HOLDS (staged in `recovered_queue`, re-admitted the moment its
/// pane rebinds), while a refusal was explicitly declined and the sender told
/// so synchronously. Keeping them in one list would put refusals within reach
/// of `readmit_recovered`, and silently re-admitting a declined delivery later
/// would reorder it against everything the pane accepted in the meantime.
/// Being audit-derived and read-only, this list structurally cannot do that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusedDelivery {
    /// Whoever called `deliver_prompt` — off the audit line's `from` detail,
    /// not its actor (the actor is `loomux`, which did the refusing, not the
    /// sender who lost the work). `"?"` for a pre-#563 line that recorded
    /// only `{to, reason, depth}`.
    pub from: String,
    /// The pane the payload was headed for.
    pub to: String,
    pub refused_ms: u64,
    /// Why loomux refused it (#633) — the discriminator the reader acts on.
    pub reason: RefusalReason,
    /// The pane's queue depth at the moment of refusal — `QUEUE_MAX_PER_PANE`
    /// in every real `queue-full-at-call` case, carried through rather than
    /// assumed.
    ///
    /// `None` (#633) for a refusal that never reached the queue at all: a
    /// dead-target or no-terminal refusal returns before admission, so there is
    /// no depth to report and reporting `0` would be a measurement nobody took
    /// dressed as one that says the pane was empty.
    pub depth: Option<usize>,
    /// Which [`EnqueueReason`](queue::EnqueueReason) the refused admission was
    /// made under, when the line recorded one. `None` for a marker refusal
    /// (never admitted under a reason) and for a pre-#563 line.
    pub enqueue_reason: Option<String>,
    pub payload: RefusedPayload,
    /// `text.len()` as the refusal recorded it, so the true size is known even
    /// when the bytes are not recoverable. `None` on a pre-#563 line.
    pub bytes: Option<usize>,
    /// The bounded one-line preview the refusal line carries
    /// (`queue::dropped_payload_preview`) — empty for a marker refusal and a
    /// pre-#563 line.
    pub preview: String,
    /// The full payload, recovered from the paired `prompt` audit line and
    /// VERIFIED against this refusal's own record — see
    /// [`front_door_refusals`]. `None` when it could not be verified, which is
    /// a different fact from an empty payload and asks for different handling
    /// (re-derive rather than re-send verbatim).
    pub text: Option<String>,
    /// The consequence a marker refusal states in its own audit line, carried
    /// verbatim rather than re-worded here — one string, every channel.
    pub consequence: Option<String>,
}

/// #579: every front-door refusal the readable audit window holds, plus how
/// many there were in total — see [`front_door_refusals`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontDoorRefusals {
    /// Oldest first, capped at [`REFUSED_LIST_MAX`] — the MOST RECENT that
    /// many, since a pane at capacity keeps refusing and the newest refusals
    /// are the ones still likely to matter.
    pub items: Vec<RefusedDelivery>,
    /// How many refusals the scan found, before the cap. Never implied by
    /// `items.len()`: a caller that reports a capped list as complete is the
    /// silent-truncation defect `.loomux/lessons.md` names.
    pub total: usize,
    /// Whether the audit window the scan ran over had itself been cut at
    /// `AUDIT_VIEW_LIMIT` (#579 review NB1) — i.e. whether `total` is a count
    /// of ALL this group's refusals or only of the readable tail.
    ///
    /// **Why this is load-bearing and not a nicety.** `total` and the
    /// `refused_omitted` derived from it are honest about the *list* cap and
    /// silent about the *window* cap, and the two compose badly: a group with
    /// 6000 audit entries and four refusals among the oldest thousand reports
    /// `refused_count: 0, refused_omitted: 0` — which reads as "nothing was
    /// refused", the strongest possible claim, from a scan that never saw the
    /// evidence. One bounded flag turns that into "nothing was refused in what
    /// I could read," which is the true statement and the one the reader can
    /// act on (go read `audit.jsonl`). Same job as #569's
    /// `PauseSuppression::window_start_seen`, in the same lineage, for the same
    /// reason: a scan that ran off the start of its timeline has to say so.
    pub window_truncated: bool,
}

/// #579: how many refusals `queue_orphans` lists individually. Deliberately
/// the same number as [`PAUSE_SUPPRESSION_LIST_MAX`], and for a related but
/// not identical reason: that cap bounds a notice PASTED into a pane, this one
/// bounds a tool result READ INTO an orchestrator's context — and each row here
/// can carry up to `queue::ORPHAN_TEXT_CAP_BYTES` of recovered payload, so an
/// uncapped list is an unbounded read. Unlike the orphan list, which the
/// per-pane cap of 8 already bounds, refusals accumulate without limit: a pane
/// held at capacity refuses every arrival for as long as it stays there.
/// Everything past the cap stays in `audit.jsonl`, and `FrontDoorRefusals::
/// total` says how much was left there.
pub const REFUSED_LIST_MAX: usize = 8;

/// #579: read a group's front-door refusals out of `entries` (an oldest-first
/// audit timeline, as `audit_log` returns them).
///
/// A delivery refused at the cap is the one loss with NO queue entry to its
/// name: `enqueue_text`'s `RejectFull` arm returns before `queue_seq.fetch_add`,
/// so it can never join the id-keyed orphan derivations, which is exactly why
/// #563 split this out (#579) instead of folding it into #572's visibility fix.
/// The audit line is the only record that will ever exist, and since #572 it
/// carries enough to act on: `from`, `bytes` and a bounded `preview`.
///
/// **Every refusal, not just the capped one (#633).** #579 shipped scanning one
/// reason because one reason was all that wrote a line. `deliver_prompt_as`
/// refuses two ways BEFORE admission — the target is dead, the target has no
/// terminal bound — and both wrote nothing at all, so this derivation, the
/// orphan derivations and a human reading `audit.jsonl` were equally blind to
/// them; #615 made the no-terminal case an `Err` instead of a silent `Ok`,
/// which fixed the sender's contract and left the record exactly as empty. Each
/// now writes its own [`RefusalReason`], and the filter above is an enumeration
/// over that enum rather than an equality test against one string.
///
/// **Where the payload comes from differs by reason, and it has to.** A
/// queue-full refusal happens AFTER `deliver_prompt` has audited `prompt` with
/// the full text, so #579 recovers the bytes by pairing (below). The two
/// pre-admission refusals happen BEFORE that line is written — and moving the
/// `prompt` write earlier was rejected, because `prompt` is what the whole
/// suite (and `delivered_texts`) reads as "this was offered to a pane", and a
/// delivery to a dead agent was never offered to anything. So those lines carry
/// their own `text` inline, and this reads it verbatim: it is the same line and
/// the same write, so there is no join to get wrong and nothing to verify
/// against. That is a rare line (a refusal), not a per-delivery cost.
///
/// **The no-app-handle drop is SURFACED, not excluded (#633's other half).**
/// `withdraw_unprocessable`'s undo is unreachable in production — `set_app` and
/// `set_self_arc` both run in `lib.rs`'s `setup` block, before the MCP server
/// thread that is the only way an agent can call `deliver_prompt` at all — and
/// #630 excluded it on exactly that argument, silently. Two things decided it
/// the other way here. First, once the list is reason-discriminated the cost is
/// one enum arm, so the exclusion was buying nothing. Second, "unreachable in
/// production" is a claim about today's startup order that nothing enforces,
/// and the failure mode of it going stale is precisely the #579 class: a loss
/// nothing can enumerate. Surfacing it means a broken assumption shows up as a
/// row instead of as silence. It does not double-report: the withdrawal's own
/// line carries the `id`, which CLOSES that id for
/// `queue::orphaned_queue_entries`, so the entry is gone from the orphan
/// derivation by the time it appears here — one loss, one row, the same rule
/// the `recovered` exclusion below enforces from the other side.
///
/// **Recovering the payload, verified rather than assumed.** `deliver_prompt`
/// audits `prompt` — with the FULL text — immediately before it admits, on both
/// the paused and unpaused paths, so the bytes a refusal lost are still in the
/// log. This pairs a refusal with the most recent `prompt` line from the SAME
/// sender to the SAME target, and accepts it only if BOTH of that line's
/// fingerprints match the refusal's own record: `text.len() == bytes`, and
/// `queue::dropped_payload_preview(text) == preview` recomputed. Two checks
/// rather than positional adjacency, because audit writes from concurrent
/// delivery threads interleave and "the line just before" is not a guarantee.
/// If either check fails the row reports `text: None` and the reader falls back
/// to the preview — the safe direction, since the failure mode of guessing here
/// is handing an orchestrator the wrong bytes to paste into somebody's terminal.
/// The residual it does not close: a second `prompt` line for the same
/// (sender, target) pair, written by another thread inside the window between
/// this delivery's own `prompt` line and its refusal, whose text ALSO matches
/// both fingerprints — i.e. the same payload, or one that agrees on length and
/// on its whitespace-collapsed first `queue::DROPPED_PREVIEW_MAX` chars.
///
/// **`recovered` refusals are deliberately excluded.** A recovery re-admission
/// refused at the cap (`readmit_recovered`, `EnqueueReason::Recovered`) writes
/// this same line, but that entry is put straight back into staging and keeps
/// being reported as an ORPHAN — with its payload, and by an id. Listing it here
/// too would show one lost payload twice in one tool result, and the documented
/// response to both lists is to re-send.
///
/// `window_truncated` is passed IN rather than guessed at from `entries.len()`
/// — see [`OrchRegistry::audit_log_windowed`], which is where the cut happens
/// and therefore the only place that knows. It is a required parameter, not a
/// field a caller fills in afterwards, so a future call site cannot forget it
/// and silently re-introduce a complete-looking count over a partial window
/// (#579 review NB1).
///
/// One audit entry read as a front-door refusal, or `None` if it is not one
/// (#658, extracted from [`front_door_refusals`]).
///
/// **Extracted rather than re-spelled.** #658's drain-time roster
/// ([`refusal_roster`]) needs exactly this classification — which
/// `delivery-dropped` lines are refusals, which reason each carries, which
/// shape (`prompt` vs marker) it is, and which are excluded — over a different
/// window and for a single target. Re-deriving it there would mean two filters
/// that must agree forever about what counts as a refusal, and the failure mode
/// of them drifting is the #579 class again: a loss one channel enumerates and
/// the other silently does not.
///
/// `text` is filled ONLY from an inline `text` field (the #633 pre-admission
/// refusals, which write their own payload because no `prompt` line exists to
/// pair with). Recovering a queue-full refusal's bytes needs the timeline
/// either side of this line, so that stays in `front_door_refusals`, which has
/// it.
///
/// **The two exclusions are here, not at the call sites**, so both consumers
/// inherit them:
/// - An unmodelled `delivery-dropped` reason. `delivery-dropped` is written for
///   reasons that are not front-door refusals at all: `agent-died` and
///   `queue-full` (a whole queue dropped at once) carry an `id` and are already
///   reported by the id-keyed orphan derivations, so listing them here too
///   would show one loss twice in one tool result. Anything
///   [`RefusalReason::from_audit`] does not know is skipped by the same rule —
///   an unmodelled reason is not silently folded into a list whose documented
///   response is "re-send".
/// - A `recovered` re-admission refused at the cap. That entry is put straight
///   back into staging and keeps being reported as an ORPHAN, with its payload
///   and by an id.
fn refusal_row(e: &AuditEntry) -> Option<RefusedDelivery> {
    if e.action != "delivery-dropped" {
        return None;
    }
    let reason = e.detail["reason"].as_str().and_then(RefusalReason::from_audit)?;
    let to = e.detail["to"].as_str().unwrap_or("?").to_string();
    let depth = e.detail["depth"].as_u64().map(|d| d as usize);
    if e.detail["payload"] == json!(RefusedPayload::StrandedSubmit.as_str()) {
        return Some(RefusedDelivery {
            // A marker push is loomux's own act, and its line records no
            // `from` — the actor is the honest answer here, unlike on a
            // prompt refusal where a sender lost the work.
            from: e.actor.clone(),
            to,
            refused_ms: e.ts_ms,
            reason,
            depth,
            enqueue_reason: None,
            payload: RefusedPayload::StrandedSubmit,
            bytes: None,
            preview: String::new(),
            text: None,
            consequence: e.detail["consequence"].as_str().map(str::to_string),
        });
    }
    let enqueue_reason = e.detail["enqueue_reason"].as_str().map(str::to_string);
    if enqueue_reason.as_deref() == Some(queue::EnqueueReason::Recovered.as_str()) {
        return None;
    }
    Some(RefusedDelivery {
        from: e.detail["from"].as_str().unwrap_or("?").to_string(),
        to,
        refused_ms: e.ts_ms,
        reason,
        depth,
        enqueue_reason,
        payload: RefusedPayload::Prompt,
        bytes: e.detail["bytes"].as_u64().map(|b| b as usize),
        preview: e.detail["preview"].as_str().unwrap_or("").to_string(),
        text: e.detail["text"].as_str().map(str::to_string),
        consequence: e.detail["consequence"].as_str().map(str::to_string),
    })
}

/// Pure: entries in, summary out, no registry and no filesystem — the same split
/// [`suppressed_during_pause`] follows, and for the same reason (this reads
/// `AuditEntry`, which lives here rather than in `queue.rs`).
pub fn front_door_refusals(entries: &[AuditEntry], window_truncated: bool) -> FrontDoorRefusals {
    // (sender, target) -> the full text of the last `prompt` line between them.
    let mut offered: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    let mut items: Vec<RefusedDelivery> = Vec::new();
    for e in entries {
        if e.action == "prompt" {
            if let (Some(to), Some(text)) = (e.detail["to"].as_str(), e.detail["text"].as_str()) {
                offered.insert((e.actor.clone(), to.to_string()), text.to_string());
            }
            continue;
        }
        let Some(mut row) = refusal_row(e) else { continue };
        // #633: a pre-admission refusal carries its own payload, because no
        // `prompt` line was ever written for it to pair with — see this
        // function's doc. When the line has one, `refusal_row` has already
        // taken it: same line, same write, nothing to join and so nothing to
        // verify against. Otherwise fall back to #579's verified pairing.
        if row.text.is_none() {
            row.text = row
                .bytes
                .and_then(|bytes| {
                    offered.get(&(row.from.clone(), row.to.clone())).filter(|t| {
                        t.len() == bytes
                            && queue::dropped_payload_preview(t.as_str()) == row.preview
                    })
                })
                .cloned();
        }
        items.push(row);
    }
    let total = items.len();
    if total > REFUSED_LIST_MAX {
        items.drain(..total - REFUSED_LIST_MAX);
    }
    FrontDoorRefusals { items, total, window_truncated }
}

/// #658: the audit action the drain-time refusal roster writes — and the
/// watermark [`refusal_roster`] reads back, which is why it is a constant
/// rather than a literal at each end.
pub const REFUSAL_ROSTER_ACTION: &str = "refusal-roster";

/// #658: the roster's opening sentence, held as a constant because it is load
/// bearing twice over: [`refusal_roster_notice`] writes it, and
/// [`refusal_roster`] recognises a refused roster BY it. Sharing one literal is
/// what makes the second use impossible to drift from the first.
pub const REFUSAL_ROSTER_OPENER: &str =
    "[orrerix] your pane's delivery queue has drained back below its cap.";

/// #658: how many refusals one roster names individually.
///
/// Smaller than [`REFUSED_LIST_MAX`] (8) on purpose, and the difference is the
/// CHANNEL, not the fact. That cap bounds a JSON list an orchestrator pulls
/// deliberately with `queue_orphans`; this one bounds a SINGLE LINE that is
/// either pasted into a pane or ridden back on a tool result — single because
/// [`OrchNoticeInbox::park`] requires it (every row of the relay block has to
/// stay maskable). Four entries at [`ROSTER_PREVIEW_MAX`] each keeps that line
/// in the same order of magnitude as [`NOTICE_AUDIT_TEXT_CAP`], which is what
/// every other loomux notice is sized against. Everything past the cap is
/// counted, said out loud, and still in `audit.jsonl`.
pub const ROSTER_LIST_MAX: usize = 4;

/// #658: how much of each refusal's preview one roster row carries. Tighter
/// than [`queue::DROPPED_PREVIEW_MAX`] (160) because a roster carries up to
/// [`ROSTER_LIST_MAX`] of them on ONE line — see that constant. Long enough
/// that the recipient can tell WHICH delivery it was, which is the whole job:
/// the payload itself is not re-sendable from here and is not offered as if it
/// were (`queue_orphans` has the verified bytes).
pub const ROSTER_PREVIEW_MAX: usize = 80;

/// #658: one refused delivery as the drain-time roster reports it — sender,
/// bounded preview, reason, and whether the sender has since got it through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RosterEntry {
    /// The sender who lost the work — who the recipient has to ask.
    pub from: String,
    pub reason: RefusalReason,
    /// Re-clamped to [`ROSTER_PREVIEW_MAX`] from the refusal line's own
    /// preview, never re-derived from the payload.
    pub preview: String,
    /// **Marked, not suppressed** (#658's own wording). A refusal whose sender
    /// has since re-sent the same payload successfully is still listed —
    /// because the recipient cannot tell from the outside which of its
    /// arriving deliveries was a re-send, and a list that silently omitted them
    /// would read as "these are all still missing" while being short. See
    /// [`refusal_roster`] for what "successfully" is derived from.
    pub resent: bool,
}

/// #658: everything one drain's roster says, before it is worded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusalRoster {
    /// The pane this roster is FOR — the one that refused these deliveries.
    pub to: String,
    /// Oldest first, capped at [`ROSTER_LIST_MAX`] — the most recent that many,
    /// since a pane held at capacity keeps refusing and the newest refusals are
    /// the ones still likely to matter.
    pub items: Vec<RosterEntry>,
    /// Refusals in the window before the cap. Never implied by `items.len()`.
    pub total: usize,
    /// `total - items.len()` — counted and said out loud, never silently cut.
    pub omitted: usize,
    /// The newest `refused_ms` this roster covers, and how many of the covered
    /// refusals share it. Written to the roster's own audit line and read back
    /// as the next roster's start point — see [`refusal_roster`]'s watermark
    /// note for why a bare timestamp is not enough.
    pub through_ms: u64,
    pub at_through: usize,
    /// Whether the audit window this was derived from had itself been cut at
    /// `AUDIT_VIEW_LIMIT` — same job as [`FrontDoorRefusals::window_truncated`],
    /// and said in the notice rather than swallowed.
    pub window_truncated: bool,
}

/// #658: every delivery `to_agent` refused that it has not been told about
/// yet — derived from `entries` (an oldest-first audit timeline) and nothing
/// else.
///
/// **Why the audit log is the record and there is no second bookkeeping
/// structure.** Every front-door refusal already writes a `delivery-dropped`
/// line carrying the four things a roster says (`from`, `to`, `reason`,
/// `preview`) — #563 put the preview there and #633 made every refusal reason
/// write one. A parallel in-memory list of "refusals not yet relayed" would be
/// a second copy of that, one that a restart empties and that can disagree with
/// the log a human reads. The one thing the log does NOT hold is how far the
/// last roster got, so that — and only that — is what this writes back, as a
/// line of the same log (see below).
///
/// **The watermark is a timestamp AND a count, and both are needed.** A roster
/// records the newest `refused_ms` it covered plus how many of the covered
/// refusals carried exactly that millisecond; the next scan skips everything
/// older and the first `at_through` at that same millisecond. A bare timestamp
/// with `>` would DROP a refusal stamped in the same millisecond as the last
/// one reported (a report burst into one pane is precisely when that happens),
/// and with `>=` it would repeat one forever. The count is exact instead:
/// audit entries are appended in write order, so "the first N at that
/// millisecond" names the same N on every re-read, and a same-millisecond
/// refusal appended AFTER the roster ran is the (N+1)th and is picked up next
/// time.
///
/// **Only a DELIVERED roster moves the watermark** (`delivered: true` on its
/// audit line). A roster that was itself refused reports nothing to anybody, so
/// letting it advance the mark would lose exactly the payloads it was written
/// to name.
///
/// **Exclusions beyond [`refusal_row`]'s own**, each load-bearing:
/// - A roster that was itself refused. Including it would put the previous
///   roster's text inside the next roster's preview, and that one inside the one
///   after: the recursion the issue asks this mechanism not to have. Recognised
///   two ways because a refusal has two shapes and neither test covers both — a
///   queue-full refusal records the [`queue::EnqueueReason::RefusalRoster`] the
///   admission was attempted under, while the #633 pre-admission refusals never
///   reach an admission and so record no reason at all, and are caught by
///   [`REFUSAL_ROSTER_OPENER`] leading the preview loomux itself wrote.
/// - A [`RefusedPayload::StrandedSubmit`] marker refusal. It has no sender to
///   ask and no payload to re-send; what it means is "there is unsubmitted text
///   in this pane's box", which is a different instruction with its own
///   `consequence` string already surfaced verbatim by `queue_orphans`.
///   Folding it into a list whose every other row means "ask this agent to
///   send it again" would misdirect the reader.
///
/// Pure — entries in, roster out — for the same reason
/// [`front_door_refusals`] is.
pub fn refusal_roster(
    entries: &[AuditEntry],
    to_agent: &str,
    window_truncated: bool,
) -> RefusalRoster {
    let (mut through_ms, mut at_through) = (0u64, 0usize);
    for e in entries {
        if e.action == REFUSAL_ROSTER_ACTION
            && e.detail["to"] == json!(to_agent)
            && e.detail["delivered"] == json!(true)
        {
            through_ms = e.detail["through_ms"].as_u64().unwrap_or(0);
            at_through = e.detail["at_through"].as_u64().unwrap_or(0) as usize;
        }
    }
    let mut skipped_at_through = 0usize;
    // (index into `entries`, the row) — the index is what the re-send scan
    // below needs, since "already re-sent" is a fact about what came AFTER.
    let mut covered: Vec<(usize, RefusedDelivery)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let Some(row) = refusal_row(e) else { continue };
        if row.to != to_agent || row.payload != RefusedPayload::Prompt {
            continue;
        }
        if row.enqueue_reason.as_deref() == Some(queue::EnqueueReason::RefusalRoster.as_str())
            || row.preview.starts_with(REFUSAL_ROSTER_OPENER)
        {
            continue;
        }
        if row.refused_ms < through_ms {
            continue;
        }
        if row.refused_ms == through_ms && skipped_at_through < at_through {
            skipped_at_through += 1;
            continue;
        }
        covered.push((i, row));
    }
    let total = covered.len();
    let new_through_ms = covered.last().map(|(_, r)| r.refused_ms).unwrap_or(through_ms);
    // Counted over EVERYTHING covered, not over the capped list: the cap drops
    // the oldest rows from the wording, never from what this roster is
    // answerable for, and a watermark that stopped short of them would re-report
    // them at every future drain.
    let new_at_through =
        covered.iter().filter(|(_, r)| r.refused_ms == new_through_ms).count();
    if total > ROSTER_LIST_MAX {
        covered.drain(..total - ROSTER_LIST_MAX);
    }
    let items = covered
        .into_iter()
        .map(|(i, r)| RosterEntry {
            resent: refusal_was_resent(entries, i, &r),
            preview: queue::clamp_preview(&r.preview, ROSTER_PREVIEW_MAX),
            from: r.from,
            reason: r.reason,
        })
        .collect::<Vec<_>>();
    RefusalRoster {
        to: to_agent.to_string(),
        omitted: total - items.len(),
        items,
        total,
        through_ms: new_through_ms,
        at_through: new_at_through,
        window_truncated,
    }
}

/// Two sender names, out of two audit rows, that mean the same sender.
///
/// `audit.jsonl` spans the #1153 phase 3 flag day: one row can carry the
/// pre-rename host actor and the next the current one, for deliveries this app
/// sent minutes apart. A bare `==` between two such rows is a comparison of
/// spellings where the question is identity, and it answers wrong in both
/// directions — see the two call sites, which fail opposite ways.
///
/// Agent ids are unaffected: they are never host actors, so the second arm
/// cannot make two different agents compare equal.
fn same_audit_sender(a: &str, b: &str) -> bool {
    a == b || (brand::is_host_actor(a) && brand::is_host_actor(b))
}

/// #658: did `row`'s sender get this same payload through after `entries[at]`
/// refused it?
///
/// **Derived from the timeline, not assumed from silence.** `deliver_prompt`
/// audits `prompt` with the full text immediately before every admission, and a
/// refusal writes its own `delivery-dropped` line — so a re-send of the same
/// payload by the same sender to the same target is a later `prompt` line, and
/// a re-send that was refused AGAIN is that line followed by another refusal.
/// This walks forward counting the first and spending the second; anything left
/// over is a `prompt` that no refusal accounts for, i.e. one that was admitted
/// (or coalesced onto an entry already queued, which is the same fact for the
/// recipient: the payload is in the queue).
///
/// **Only a queue-full-shaped refusal spends a credit.** The #633 pre-admission
/// refusals write no `prompt` line at all — they carry their payload inline
/// instead, which is exactly what [`refusal_row`] surfaces as `text: Some(_)` —
/// so charging them for one would consume a re-send that really did land.
///
/// Matched on the same two fingerprints [`front_door_refusals`] pairs on
/// (`text.len() == bytes` and a recomputed
/// [`queue::dropped_payload_preview`]), never on positional adjacency, because
/// audit writes from concurrent delivery threads interleave. It inherits that
/// pairing's residual too: a DIFFERENT payload from the same sender to the same
/// target that agrees on length and on its collapsed first
/// [`queue::DROPPED_PREVIEW_MAX`] chars would be counted as this one's re-send.
/// The cost of being wrong is one row reading "already re-sent" instead of "ask
/// for it again" — which is why the answer is `false` whenever it cannot be
/// established at all (a refusal line too old to carry `bytes`): the roster
/// would rather ask for a delivery twice than tell a pane that a lost report is
/// already handled.
fn refusal_was_resent(entries: &[AuditEntry], at: usize, row: &RefusedDelivery) -> bool {
    let Some(bytes) = row.bytes else { return false };
    let mut credit: i64 = 0;
    for e in entries.iter().skip(at + 1) {
        if e.action == "prompt" {
            // rev-967 N5: across the flag day a bare `==` here fails to
            // credit a genuine re-send, and the roster tells an orchestrator
            // to ask again for a delivery that landed.
            if same_audit_sender(&e.actor, &row.from) && e.detail["to"] == json!(row.to) {
                if let Some(t) = e.detail["text"].as_str() {
                    if t.len() == bytes && queue::dropped_payload_preview(t) == row.preview {
                        credit += 1;
                    }
                }
            }
            continue;
        }
        let Some(later) = refusal_row(e) else { continue };
        if later.text.is_none()
            && later.to == row.to
            // The SECOND site of rev-967 N5's class, found by sweeping for it
            // rather than named in the review. It fails the OTHER way: an
            // unmatched spelling skips this `credit -= 1`, so a delivery that
            // was refused again reads as one that got through and drops off
            // the roster entirely — a loss, where the site above only causes
            // a duplicate ask.
            && same_audit_sender(&later.from, &row.from)
            && later.bytes == row.bytes
            && later.preview == row.preview
        {
            credit -= 1;
        }
    }
    credit > 0
}

/// #658: word a roster as the ONE line it has to be, or `None` when there is
/// nothing to say — the ordinary case, and the one that must cost a pane
/// nothing at all.
///
/// **One line is a hard requirement, not a style choice.** On an orchestrator
/// target this text is parked in [`OrchNoticeInbox`], whose `park` asserts
/// every notice is a single [`NOTICE_MARKER`]-led line so that every row
/// of the relay block stays maskable (#576/#621). Using the same string on the
/// pane-delivery path too means the recipient reads identical words whichever
/// channel carried it.
///
/// **It tells the reader what to DO, and never overstates.** Every row names
/// the sender to ask, because loomux does not and will not re-send these
/// itself: the payloads were declined synchronously and their senders were
/// told. Rows the sender has already got through say so rather than being
/// dropped from the list — see [`RosterEntry::resent`].
pub fn refusal_roster_notice(r: &RefusalRoster) -> Option<String> {
    if r.items.is_empty() {
        return None;
    }
    let n = r.total;
    // Opens with the shared constant — which itself leads with
    // `NOTICE_MARKER`, the single-line maskability requirement above.
    let mut out = format!(
        "{REFUSAL_ROSTER_OPENER} While it was full, {n} deliver{y} to you {was} REFUSED and \
         never queued — loomux does NOT re-send them, so anything below that is not marked \
         re-sent is still missing:",
        y = if n == 1 { "y" } else { "ies" },
        was = if n == 1 { "was" } else { "were" },
    );
    for (i, it) in r.items.iter().enumerate() {
        out.push_str(if i == 0 { " " } else { " | " });
        let clause = if it.resent {
            format!("{} has since re-sent it — nothing to do", it.from)
        } else {
            format!("NOT re-sent — ask {} for it", it.from)
        };
        out.push_str(&format!(
            "{} \"{}\" ({}; {clause})",
            it.from,
            it.preview,
            it.reason.as_str()
        ));
    }
    if r.omitted > 0 {
        out.push_str(&format!(
            " | plus {} earlier refusal{s} not listed here (this roster names {ROSTER_LIST_MAX}) \
             — every one is in this group's audit.jsonl as a `delivery-dropped` line",
            r.omitted,
            s = if r.omitted == 1 { "" } else { "s" }
        ));
    }
    if r.window_truncated {
        out.push_str(
            " | the audit window this was read from was itself cut, so there may be older \
             refusals it could not see",
        );
    }
    Some(out)
}
