//! Tuning constants shared across concerns or used only by registry methods
//! (timeouts, poll intervals, byte caps). A constant with one free-code user
//! sits beside that user instead (#3498 P4).
//! Design note: `docs/design/orchestration.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. It calls no
//! sibling file.

use super::*;
/// Sliding window the spawn-rate guardrail counts spawns over.
pub(in crate::orchestration) const SPAWN_RATE_WINDOW_MS: u64 = 60 * 60 * 1000;
/// Arm the low-disk notice when free space on the workspace drive drops below
/// this (#134). A disk-full write is what destroyed the board in the incident
/// (#133); 5 GB leaves headroom for one more cold cargo build (~5–7 GB) to be
/// reclaimed before writes start failing at 0.
pub(in crate::orchestration) const LOW_DISK_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// Clear the low-disk latch only once free space recovers past this higher mark
/// (arming + 2 GB). The hysteresis stops a disk hovering at the threshold from
/// re-notifying every tick.
pub(in crate::orchestration) const LOW_DISK_CLEAR_BYTES: u64 = LOW_DISK_BYTES + 2 * 1024 * 1024 * 1024;
/// How long the merge-queue driver holds a group off after a tick whose next
/// attempt would cost the same external calls and reach the same answer
/// (#698 — `mqloop::DriveReport::backoff`).
///
/// Two shapes ask for it: an **external failure**, since §10's rows all end
/// "entries return to `queued`" and the very next tick would retry the same
/// thing against a remote that is simply down; and **nothing eligible to
/// batch**, which costs two `gh` round-trips per examined entry to establish
/// and can stay true for hours while a re-review is pending.
///
/// The bound is not a retry *limit* — §10 is explicit that the entries requeue
/// and a later batch re-derives the question — it is a **rate**, so a stuck
/// group costs a handful of `gh` calls and at most one notice per five minutes
/// instead of per wake. Five minutes because that is long enough to be quiet and
/// short enough that neither a transient auth blip nor a fresh review parks a
/// batch for a coffee break.
pub(in crate::orchestration) const MQ_DRIVE_BACKOFF_MS: u64 = 5 * 60_000;
/// #743 S4b: how long a computed group-usage summary may be re-served to the
/// POLLED command path before it is recomputed.
///
/// One second, chosen against the poll cadences it had to collapse: the group
/// view's 2 s batch fired `orch_group_usage` and `orch_autonomy` (whose budget
/// meter runs the same computation) concurrently in one `Promise.all`, and the
/// tab bar polled at 4 s per group-bound tab. Since #1608 neither of those is a
/// caller: the snapshot publisher runs the computation once per
/// [`views::VIEW_PUBLISH_INTERVAL`], which is deliberately this same second, so
/// the window still bounds exactly what it always bounded. A window of one
/// second is short enough that no tick ever *skips* a refresh — every pass still
/// recomputes
/// — and long enough that the several reads inside one tick share a single
/// computation. It is the freshness bound, not a cache lifetime: figures are a
/// cost meter refreshed twice a second at worst, and every non-polled caller
/// (the MCP tool, the autonomy anchor, the budget enforcer) passes
/// `Duration::ZERO` and never sees a stored value at all.
pub(in crate::orchestration) const USAGE_POLL_MAX_AGE: Duration = Duration::from_millis(1000);

/// #743 S4a: how long the repo's resolved default-branch NAME may be re-served
/// before it is re-resolved from `git`.
///
/// Coarse — five minutes — because the answer changes only when a repo's
/// default branch is renamed or first created, and because resolving it costs
/// 2-4 blocking `git` spawns that `orch_workflow_status` was paying every 2 s
/// per open group view. The staleness this ADDS is bounded by this constant and
/// is strictly smaller than the staleness the value already carries by design:
/// [`crate::git::default_branch_name`] reads local refs only, so it is stale
/// until something happens to fetch, with no bound at all. Display-only — no
/// gate reads it (see that function's doc).
pub(in crate::orchestration) const DEFAULT_BRANCH_MAX_AGE: Duration = Duration::from_secs(5 * 60);
/// Autonomous mode (#83): hard backstop on idle ticks delivered per rolling hour,
/// independent of the one-notice latch — the analogue of `max_spawns_per_hour` for
/// the tick source. With a minutes-scale quiet gate the latch already bounds this
/// near ~one per window; the cap catches any pathological re-arm. 0 would disable it.
pub(in crate::orchestration) const MAX_IDLE_TICKS_PER_HOUR: u32 = 6;
/// #496: the audit `reason` recorded when `idle_tick_input_defer_max_minutes`
/// — not the ordinary quiet-window threshold — is why a tick fired: the
/// input fold was clamped against `AgentEntry.last_output_progress_ms` and
/// the bound elapsed. See `DEFAULT_IDLE_TICK_INPUT_DEFER_MAX_MINUTES` (`guardrails.rs`) and `idle_tick_tick`.
pub(in crate::orchestration) const IDLE_TICK_INPUT_DEFER_BOUND_REASON: &str = "idle-tick-input-defer-bound";
/// Compact-nudge (#287): hard backstop on `/compact` nudges delivered per
/// rolling hour, independent of the one-shot latch — same role as
/// `MAX_IDLE_TICKS_PER_HOUR` for the idle tick, reused via the same
/// `idle_tick_should_fire` gate (see `compact_nudge_tick`).
pub(in crate::orchestration) const MAX_COMPACT_NUDGES_PER_HOUR: u32 = 4;
/// Production bug fix (rev-42 delta, round 2): how long a fired reinjection
/// is given to confirm delivery before `compact_nudge_tick` treats it as
/// stuck and retries. Generous enough to clear `deliver_prompt`'s own
/// worst-case hold chain (two `USER_QUIET_MAX_HOLD` waits plus
/// `SUBMIT_MAX_WAIT` and the echo/retry window — comfortably under this)
/// without racing a delivery that is merely slow, not lost.
pub(in crate::orchestration) const REINJECT_CONFIRM_TIMEOUT_MS: u64 = 5 * 60 * 1000;
/// #535: how much longer past `REINJECT_CONFIRM_TIMEOUT_MS` a retry may be
/// deferred while the pane is still actively producing output. A retry pasted
/// into a live turn is the worst possible moment for it — it interrupts an
/// agent that is demonstrably working — so the timeout waits for a lull rather
/// than firing blind. Bounded rather than open-ended: a pane that never goes
/// quiet (an animated statusline above the group's `idle_activity_floor_bytes`,
/// a genuinely runaway turn) must not be able to suppress the retry forever,
/// or scope item 3's safety net for a genuinely LOST re-grounding dies with it.
///
/// Sized at one further `REINJECT_CONFIRM_TIMEOUT_MS`, i.e. a stuck-and-busy
/// reinjection reaches `MAX_REINJECT_ATTEMPTS` in at most 30 minutes instead of
/// 15. A constant, deliberately not a `Guardrails` knob: #518's bound made the
/// same call for the same reason — a knob with no correct second setting only
/// ever gets set wrong.
pub(in crate::orchestration) const REINJECT_BUSY_DEFER_MAX_MS: u64 = REINJECT_CONFIRM_TIMEOUT_MS;
/// #535 (rev-15 finding 2): how long after an attempt is DECIDED before an MCP
/// call may be read as an acknowledgment of it.
///
/// Without a floor, `attempted_ms` is the moment loomux decided to paste — but
/// the agent cannot have read anything until the paste is written AND the Enter
/// is pressed. So a tool call the agent had already decided on during its
/// *previous* turn, arriving milliseconds later, is indistinguishable from a
/// response and resolves the phase for a notice the agent provably had not yet
/// seen. That is a false landed-signal — the exact family #112 (the
/// output-burst version) and #522 (the idle-pane version) exist to kill, and
/// re-introducing it here through a different door would defeat the point.
///
/// **The ORDINARY SUBMIT PATH's worst case, computed from the constants that
/// define it** — every quantity referenced, none written down, so the floor
/// cannot drift out of step with the delivery code (rev-22 D2). "Ordinary" means
/// *unconditional*: every stage below runs on every delivery. `deliver_prompt`:
///
/// 1. types the paste under echo verification — up to `ECHO_ATTEMPTS` rounds,
///    each polling for the TUI's redraw for up to `ECHO_WINDOW` and then
///    sleeping `ECHO_RETRY_DELAY` before retyping (that sleep runs after the
///    final attempt too, before the loop condition ends it);
/// 2. sleeps `PASTE_SUBMIT_DELAY` between the paste and the Enter;
/// 3. waits for the pane to go quiet before pressing Enter, bounded by
///    `SUBMIT_MAX_WAIT`;
/// 4. watches for the submit's output burst for `SUBMIT_CONFIRM_WINDOW`;
/// 5. makes blind Enter retries by sleeping each `SUBMIT_RETRY_DELAYS` entry in
///    turn — **sequential sleeps, so the last one lands at their SUM**, not at
///    the last element.
///
/// **This constant has now been wrong twice, both times short and both times in
/// a way that read as rigorous** — first by treating `SUBMIT_RETRY_DELAYS` as
/// absolute offsets and omitting `SUBMIT_CONFIRM_WINDOW` (2.6s short, rev-22
/// D1), then by summing only stages 3-5 while claiming to cover the ordinary
/// path (11.2s short, rev-28 Q1). Both are recorded rather than quietly
/// corrected, because the failure mode is not arithmetic — it is a sum that
/// *looks* complete. **If you add a stage to `deliver_prompt` before the last
/// Enter, it belongs in the expression below.**
///
/// **What this does NOT cover, and is not claimed to** (rev-22 D3). Three
/// `wait_for_question_clear` checkpoints sit between the decision and the last
/// Enter, each bounded by `QUESTION_HOLD_MAX` (120s), and
/// `HUMAN_INPUT_BLOCK_BOUND_MS` (10 min) is a further pre-delivery hold. Any of
/// them can push the Enter well past this floor, leaving a residual window in
/// which a pre-paste call still reads as an acknowledgment. Folding them in is
/// not the fix: the floor would exceed `REINJECT_CONFIRM_TIMEOUT_MS` itself and
/// the mechanism would stop resolving anything. **That residual is tracked in
/// #546**, with the related question of what could actually prove the notice
/// was *read* rather than merely that the agent is executing. So this is a
/// judgement about the common path, not a proof about every path — the earlier
/// wording claimed the latter, which is exactly the kind of sentence that stops
/// the next reader from checking.
///
/// The cost is nothing the signal can feel: ~21% of
/// `REINJECT_CONFIRM_TIMEOUT_MS`, leaving ~236s in which a genuine ack still
/// resolves the phase. And a genuine ack landing *inside* the floor is not
/// lost, merely not counted yet — agents call loomux tools repeatedly, so the
/// next one resolves it; if none ever comes, the unchanged retry path runs,
/// which is exactly the pre-#535 behaviour.
///
/// Why not the delivery ledger's own `submit_sent_ms`, which would be exact?
/// Because that would re-couple the acknowledgment path to the delivery
/// bookkeeping this whole fix exists to stop depending on. A bound computed
/// from our own timing constants needs no sampler to be working.
pub(in crate::orchestration) const REINJECT_ACK_SETTLE_MS: u64 = {
    // Stage 5. `while` rather than an iterator: `Iterator::sum` is not const.
    let mut tail_ms = 0u64;
    let mut i = 0;
    while i < SUBMIT_RETRY_DELAYS.len() {
        tail_ms += SUBMIT_RETRY_DELAYS[i].as_millis() as u64;
        i += 1;
    }
    // Stage 1: every attempt can burn its full echo window AND its retry sleep.
    let echo_ms = ECHO_ATTEMPTS as u64
        * (ECHO_WINDOW.as_millis() as u64 + ECHO_RETRY_DELAY.as_millis() as u64);
    // `as_millis`, not `as_secs() * 1000`: the latter silently truncates a
    // sub-second change to any of these (rev-22 N1).
    echo_ms                                             // 1: echo-verified typing
        + PASTE_SUBMIT_DELAY.as_millis() as u64         // 2: paste → Enter gap
        + SUBMIT_MAX_WAIT.as_millis() as u64            // 3: wait for quiet
        + SUBMIT_CONFIRM_WINDOW.as_millis() as u64      // 4: submit-burst watch
        + tail_ms // 5: blind Enter retries
};

/// #539: the settling floor `unconfirmed_disposition`'s activity input is
/// measured from — `REINJECT_ACK_SETTLE_MS`'s sibling, for the same reason and
/// with a deliberately different sum.
///
/// **Why not simply reuse `REINJECT_ACK_SETTLE_MS`.** That floor is measured
/// from the moment a re-grounding was *decided*, so it must cover the whole
/// decision→Enter chain (stages 1-5 of its expression). This one is measured
/// from `submit_sent_ms`, which `deliver_now` stamps immediately BEFORE the
/// first Enter is written — stages 1-3 (echo-verified typing, the paste→Enter
/// gap, the wait for quiet) are already spent by then, and charging for them
/// again would push the floor past the point where a genuine post-delivery
/// call still counts. What remains after that stamp is exactly the part of our
/// own submit machinery that can still be pressing Enter: the submit-burst
/// watch and the blind retry tail.
///
/// **What it does NOT bound, stated rather than implied.** An agent that was
/// mid-turn when our Enter landed can make loomux calls belonging to its
/// PREVIOUS turn for as long as that turn runs — minutes, not milliseconds —
/// and no constant computed from delivery timings can separate those from a
/// call made in response to us. That residual is #546's question (liveness is
/// not proof of a read), and it is why the activity input governs only the one
/// arm where the box was independently OBSERVED to no longer hold our text;
/// see `unconfirmed_disposition`'s precedence table.
///
/// `pub` so the integration test can assert the shipped value against its own
/// stage-by-stage derivation. The sibling constant is private and mirrored by
/// hand instead, which catches a drifting value only if someone remembers to
/// re-derive it; comparing against the real one catches it always.
pub const UNCONFIRMED_ACK_SETTLE_MS: u64 = {
    let mut tail_ms = 0u64;
    let mut i = 0;
    while i < SUBMIT_RETRY_DELAYS.len() {
        tail_ms += SUBMIT_RETRY_DELAYS[i].as_millis() as u64;
        i += 1;
    }
    SUBMIT_CONFIRM_WINDOW.as_millis() as u64 + tail_ms
};

/// Production bug fix (#410, PR #329 round 6): how long a `compact_pending`
/// arm is given to reach a busy-then-quiet resolution (confirm or discard)
/// before the resolver forces an abandon — symmetric to `REINJECT_CONFIRM_
/// TIMEOUT_MS`'s bound on the delivery-confirmation phase. See `AgentEntry::
/// compact_pending_armed_ms`'s doc for the live incident this closes.
pub(in crate::orchestration) const ARM_PENDING_TIMEOUT_MS: u64 = 5 * 60 * 1000;
/// Production bug fix (PR #329 round 7): how long INFERENCE arms (banner,
/// manual detection) are suppressed after a loomux-authored delivery
/// confirms, or after this agent's own compact_pending resolves — see
/// `AgentEntry::compact_inference_guard_until_ms`'s doc. Matches `MANUAL_
/// COMPACT_DETECT_WINDOW_MS`'s existing 3-minute scale: long enough to cover
/// both a paste's echo settling into the pane AND the immediate post-
/// compact conversation ABOUT the compact-nudge feature itself (exactly the
/// live-demo scenario this closes), short enough that a genuinely NEW
/// signal minutes later still arms normally. Loomux-initiated (trusted)
/// arms are NEVER gated by this — they don't infer anything, so there is
/// nothing for a stale echo to fool.
pub(in crate::orchestration) const INFERENCE_ARM_COOLDOWN_MS: u64 = 3 * 60_000;
/// #413 S5: how long a fresh Claude `PostCompact` marker waits, measured on the
/// tick's own clock from when loomux first saw it
/// (`AgentEntry::compact_hook_postcompact_first_seen_ms`), before it resolves
/// the compaction by deciding loomux's own reinjection.
///
/// **Why wait at all.** The same compaction also fires `SessionStart(compact)`,
/// whose arm prints native `additionalContext` — and a compaction that marker
/// resolves must NOT also get loomux's reinjection (rev-4 N3: a duplicate
/// re-grounding spends exactly the tokens native delivery saves). The hooks
/// reference does not say which of the two fires first, and the tick can read
/// the directory between the two writes. So a `PostCompact` marker settles for
/// this long first; a `SessionStart(compact)` that lands meanwhile resolves the
/// compaction terminally and the `PostCompact` marker is then absorbed. Shorter
/// than `COMPACT_NUDGE_FAST_POLL_INTERVAL` on purpose: while an arm is open the
/// marker resolves on the tick after the one that first saw it, never later.
pub const POSTCOMPACT_SETTLE_MS: u64 = 5_000;
/// #413 S5: how close a `PostCompact` marker's mtime must sit to the last
/// consumed `SessionStart(compact)` marker's for the two to be read as ONE
/// compaction, whichever wrote first. Both hooks fire as that compaction
/// completes; a second compaction a minute later would need a second
/// summarisation call and a refilled context in between.
pub const POSTCOMPACT_SESSIONSTART_PAIR_MS: u64 = 60_000;
pub const DEFAULT_COMPACT_CONTEXT_THRESHOLD_PERCENT: u32 = 45;
// Compact-nudge (#328): the context window for `latest_context_tokens`-based
// percent calculations used to be a single flat constant here
// (`CLAUDE_CONTEXT_WINDOW_TOKENS`, 200K) — a documented assumption, not a
// measured fact, and PROVEN wrong in a live demo (PR #329 round 7): an agent
// actually running a much larger tier read as ~5x more full than the CLI's
// own `/context` reported. Replaced by `effective_context_window_tokens`
// (model-aware, with a human override and a conservative fallback) — see
// its doc and `usage::claude_context_window_tokens`'s.
/// Compact-nudge (#328): how recently the orchestrator's `set_state` call
/// must land for `request_compact` to skip its offload-checklist warning.
/// Not a hard gate (the issue is explicit: warn, never block) — a human-
/// readable proxy for "was durable state offloaded before compacting", which
/// loomux can see via the pane's own tool-call timeline (`AgentEntry.
/// last_state_write_ms`).
pub(in crate::orchestration) const SET_STATE_RECENCY_WINDOW_MS: u64 = 15 * 60_000;
/// Compact-nudge (#328): a human-typed `/compact` detected in a pane's output
/// tail only counts as fresh if the pane's `last_user_input_ms` falls within
/// this window of "now" — guards against the tail's bounded ring buffer still
/// holding an OLDER `/compact` echo re-triggering the post-compact
/// re-injection for a compaction that already completed and was already
/// handled. Generous relative to `IDLE_TICK_INTERVAL` (the tick cadence that
/// reads it) so a slow-typed command is never missed.
pub(in crate::orchestration) const MANUAL_COMPACT_DETECT_WINDOW_MS: u64 = 3 * 60_000; // 3x IDLE_TICK_INTERVAL's 60s
/// Directive ledger (#329 expansion): max bytes of a pane's ledger tail
/// embedded verbatim in its post-compact re-grounding notice. The file on
/// disk is never truncated — only what gets PASTED into the pane is capped,
/// the same relationship the reinjected instructions text already has to the
/// (much larger) conversation it's re-grounding. Sized generously relative to
/// a one-line diary entry so a normal session's ledger fits whole; the tail
/// (most recent entries) survives a cut, never the head, because the newest
/// directives are the ones most likely to still be live.
pub(in crate::orchestration) const DIRECTIVE_LEDGER_EMBED_CAP_BYTES: usize = 2048;
/// Directive ledger (#329 review, N2): hard cap on the LEDGER FILE itself —
/// distinct from `DIRECTIVE_LEDGER_EMBED_CAP_BYTES`, which only bounds what
/// gets pasted into a reinjection notice. Without this, an agent that notes
/// liberally and never curates (`replace: true`) grows the file (and the
/// full-file `fs::read_to_string` every reinjection does) without bound.
/// Generous relative to one line per human directive — curation via
/// `replace` stays the primary, deliberate mechanism; this is only the
/// backstop for a session that never uses it. See `ledger_capped`.
pub(in crate::orchestration) const DIRECTIVE_LEDGER_MAX_BYTES: usize = 64 * 1024;
/// Directive ledger (#329 review, N1): max characters kept from a single
/// `note_directive` append-mode entry after sanitization — mirrors
/// `OrchRegistry::CHANNEL_TEXT_CAP`'s role for `channel_send` (a directive is
/// closer to a message than to a terse notification field, hence this and
/// not the smaller `notify::NOTICE_FIELD_CAP`).
pub(in crate::orchestration) const DIRECTIVE_ENTRY_MAX_CHARS: usize = 2000;
/// #814: how stale the delivery-queue badge's own suppression may get before it
/// re-pushes anyway — the independent release for
/// [`OrchRegistry::queue_depth_push`]'s skip (`performance.md` §2 P4).
///
/// 30s, i.e. every tenth attention tick, chosen the way
/// [`STRANDED_JANITOR_EVERY_N_TICKS`] was: the skip's signal is a memory of an
/// emit, not an acknowledgement of one, so a listener that never received the
/// last push wears no badge and nothing in the reading will change to correct it
/// — least of all on a queue stalled for an hour, whose coarsened reading is
/// deliberately stable. **Three ways to be that listener, not one:** a webview
/// that reloaded, an emit that never landed, and — the case a first draft of this
/// paragraph missed (rev finding 3) — a PANE that was not there for it, i.e. one
/// restored or spawned after the last push, whose badge is therefore up to this
/// window late. All three are the same staleness with the same bound; none is a
/// missing badge, only a late one. Bounding the suppression costs at most two
/// emits a minute while anything is queued, and nothing at all when nothing is.
pub(in crate::orchestration) const QUEUE_DEPTH_REPUSH_MS: u64 = 30_000;
/// A pane's terminal output must be stable (unchanged) at least this long
/// before an idle-with-prompt is asserted — the CLI has stopped painting and
/// is genuinely parked on a prompt, not mid-render. Measured across ticks, so
/// it also debounces (needs a couple of consecutive quiet scans).
pub(in crate::orchestration) const ATTENTION_QUIET_MS: u64 = 4000;
/// If the human typed into a pane within this window it does not "need
/// attention" — they are already at the keyboard on it.
pub(in crate::orchestration) const ATTENTION_RECENT_INPUT_MS: u64 = 6000;
/// How long the frontend gets to open a pane and report its pty id.
pub(in crate::orchestration) const BIND_TIMEOUT: Duration = Duration::from_secs(20);
/// Gap between the bracketed paste and the Enter that submits it.
///
/// #535: an unconditional stage of the submit path, so it is also an input to
/// `REINJECT_ACK_SETTLE_MS` and tracks automatically. Omitting it from that sum
/// is one of the two ways that floor has already been wrong.
pub(in crate::orchestration) const PASTE_SUBMIT_DELAY: Duration = Duration::from_millis(500);

// Submission discipline: copilot ignores Enter while its agent is running
// (the pasted text just sits in the input box — observed live with a worker
// report landing mid-turn), so before pressing Enter the pane must be quiet
// (turn finished). Enter on an empty box is a no-op in both CLIs, so a
// couple of spaced blind retries are safe and cover late busy-locks.
/// Output must be idle this long before Enter is pressed.
pub(in crate::orchestration) const SUBMIT_QUIET: Duration = Duration::from_millis(1000);
/// Max time to wait for quiet before pressing Enter anyway.
///
/// #535: also an input to `REINJECT_ACK_SETTLE_MS` — changing it moves the
/// window in which an agent's MCP call can be an acknowledgment of a pasted
/// re-grounding. That is computed from this constant, so it tracks
/// automatically; no second place to update.
pub(in crate::orchestration) const SUBMIT_MAX_WAIT: Duration = Duration::from_secs(45);
/// Spaced blind Enter retries after the first (no-ops once submitted).
///
/// **Each entry is a sequential `sleep` in the retry loop, not an absolute
/// offset** — with `[2500, 4500]` the retries land at +2.5s and +7.0s. Reading
/// this array as offsets is a live mistake, not a hypothetical: it is how
/// `REINJECT_ACK_SETTLE_MS` first came out 2.6s short (#535, rev-22 D1).
///
/// #535: also an input to `REINJECT_ACK_SETTLE_MS`, which sums the whole array,
/// so adding or lengthening a retry widens that floor automatically.
pub(in crate::orchestration) const SUBMIT_RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(2500), Duration::from_millis(4500)];

// Submit confirmation + stranded-text flush (#81/#84). A submit that landed
// clears the input box and the CLI repaints / starts its turn — a burst of
// output. An Enter that was ignored (focus-gated pre-#99, still busy, or an
// empty box) produces effectively none. We watch for that burst in a short
// window after the first Enter and record the outcome, so the NEXT delivery to
// the same pane can tell whether the previous prompt is still stranded in the
// box (and would otherwise merge with the new paste).
/// How long after the first Enter to watch for the submit's output burst.
///
/// #535: also an input to `REINJECT_ACK_SETTLE_MS` — it sits between the first
/// Enter and the blind-retry loop, so it is part of the worst-case time before
/// the last Enter can land. Computed from this constant, so it tracks.
pub(in crate::orchestration) const SUBMIT_CONFIRM_WINDOW: Duration = Duration::from_millis(600);
/// After flushing a previous delivery's stranded text, let the CLI settle
/// (box clears, turn starts) before the new paste lands.
pub(in crate::orchestration) const FLUSH_SETTLE: Duration = Duration::from_millis(400);
/// How long the extended post-window monitor polls before each check —
/// cheap (a tail read + a marker-file read), so this can be fairly tight
/// without real cost.
pub(in crate::orchestration) const LATE_MONITOR_POLL: Duration = Duration::from_secs(5);
/// #539: how long a pane's first unconfirmed-delivery alarm waits for
/// stragglers before it is delivered as ONE notice naming every buffered
/// delivery (`OrchRegistry::unconfirmed_pending`).
///
/// Sized from `LATE_MONITOR_POLL`, not picked round: alarms that need
/// coalescing are ones raised by *different* monitors on the same pane, and a
/// monitor only re-reads the ledger (and so only notices it has been
/// superseded) once per poll. Three polls is enough for a straggler that was
/// two ticks behind the first alarm to still land inside the window, and
/// stops well short of the point where a genuine strand's alarm feels late.
///
/// The latency it adds is paid on a condition that already took
/// `PENDING_IDLE_QUIET` (60s) of observed silence to establish, and whose
/// recovery is a human or an orchestrator reading the pane back — never a
/// deadline. Coalescing is a pure win against the cost it targets, which is
/// orchestrator turns, not milliseconds.
pub(in crate::orchestration) const UNCONFIRMED_NOTICE_COALESCE_WINDOW: Duration = Duration::from_secs(15);
/// How long the pane's own output must stay quiet — checked ONLY once the
/// normal confirm/retry window has already closed without a decision —
/// before "no evidence yet" is treated as "genuinely done and still no
/// trace, so it really is lost" rather than "still busy, ask again later".
/// Deliberately its OWN constant, not `SUBMIT_QUIET`: that 1-second bar is
/// tuned for "safe to press Enter", an unrelated question with an unrelated
/// false-positive cost. This one needs enough margin that an ordinary gap
/// in a CLI's own streaming cadence (a pause between tool calls, a spinner
/// tick) can never be mistaken for "finished and idle" — minutes, not
/// seconds. Named plainly in the design note as a byte-count proxy (the
/// SAME category of signal `watchdog_tick`'s stall detection already
/// trusts at this timescale) rather than a semantic "I see an idle prompt"
/// observation, because that's what it actually is.
pub(in crate::orchestration) const PENDING_IDLE_QUIET: Duration = Duration::from_secs(60);
/// #112 round 3 (rev-20 B1): a hard cap on how long a single late-
/// confirmation monitor thread stays alive with nothing resolved.
/// Deliberately generous — the live episode this design responds to needed
/// 38+ minutes — but not infinite: an abandoned pane (a killed agent whose
/// pty somehow never reports closed, or a `Pending` delivery that never
/// resolves and never goes idle because it keeps seeing brief unrelated
/// output) must not hold a poller forever. Combined with the supersession
/// check (`late_monitor_tick`'s doc), this bounds live monitors to at most
/// one per pane most of the time, for at most this long.
pub(in crate::orchestration) const LATE_MONITOR_MAX_LIFETIME: Duration = Duration::from_secs(4 * 3600);
/// Bytes of the pane's live output tail scanned for a pending question
/// (rev-15 N4): `prompt_wait_detected` only ever reads the last ~12 non-empty
/// lines, so a bounded trailing slice is enough — matches the attention
/// path's own trailing window (`ATTENTION_SCAN_BYTES`, the same size for the
/// identical "the prompt is at the end" reason — and since #717 the size of
/// that path's READ, not just of the slice it took afterwards), instead of
/// cloning the whole (up to 256 KB) output ring on every
/// 250ms poll for up to two minutes.
pub(in crate::orchestration) const QUESTION_SCAN_TAIL_BYTES: usize = 4096;
/// Bytes replayed onto a composed grid for the "is it still DISPLAYED"
/// reading (#534). Sized by one requirement and one budget:
///
/// - It must be **at least** `QUESTION_SCAN_TAIL_BYTES`, or the guard could
///   match text in the ring that the replay never saw painted and then read
///   its absence from the screen as evidence it was answered. Being a strict
///   superset is what makes "the paint that caused this hold is inside the
///   window we replayed" true, which is the whole argument for trusting a
///   negative (see the design note).
/// - Bigger is better for fidelity — the replay starts blind, so more bytes
///   means more of the screen genuinely painted rather than left blank — and
///   the cost is a linear byte scan plus one grid, no history retained. 16x
///   the ring window, half of `LATE_MONITOR_QUESTION_SCAN_BYTES`'s cadence
///   argument at 20x its poll rate.
pub(in crate::orchestration) const QUESTION_GRID_REPLAY_BYTES: usize = 64 * 1024;
/// #112 round 3 (rev-20 N1): the late-confirmation monitor's OWN question
/// scan window — deliberately larger than `QUESTION_SCAN_TAIL_BYTES`. That
/// constant was sized for the 250ms-cadence pre-Enter/retry checkpoints,
/// where a bigger read on every poll adds up; the monitor polls once every
/// `LATE_MONITOR_POLL` (5s), so a much larger read costs nothing measurable
/// there. It needs the extra room: a dialog rendered above a long,
/// literally-rendered paste (Tier 1 governing a multi-KB brief) sits
/// chronologically BEFORE that paste in the tail, so a 4KB window could be
/// entirely paste-plus-chrome and miss the dialog entirely — exactly the
/// wrong direction to be wrong in, since missing a live question is what
/// lets the idle trigger fire while the human is still expected to answer.
pub(in crate::orchestration) const LATE_MONITOR_QUESTION_SCAN_BYTES: usize = 32 * 1024;
/// Poll interval for the readiness check.
pub(in crate::orchestration) const READY_POLL: Duration = Duration::from_millis(250);

// Copilot autopilot consent (#101/#179): a group copilot agent is launched with
// `--autopilot`, which makes copilot open an "Enable autopilot mode" dialog the
// first time a message is submitted (NOT at boot — verified live on 1.0.69: a
// fresh pane paints a normal input box). The kickoff path answers it
// deterministically (Enter on the default "Enable all permissions") right AFTER
// the first submit, which both enables autopilot and lets the just-submitted
// brief proceed. Fail-soft: if the dialog never appears, delivery proceeds.
/// How long to watch for the consent dialog after the kickoff submit before
/// giving up and letting the submit retries carry on. Tuned to the GROUP
/// path, where the dialog-triggering submit is loomux's own kickoff Enter —
/// fired within milliseconds of this watch starting.
#[doc(hidden)] // pub for integration tests
pub const AUTOPILOT_DIALOG_WAIT: Duration = Duration::from_secs(12);
/// How long a SOLO pane's watcher watches for the consent dialog (#364) before
/// giving up. Far longer than [`AUTOPILOT_DIALOG_WAIT`] because the
/// dialog-triggering submit here is the HUMAN's own first message — there is
/// no programmatic Enter to key the wait off, so it must tolerate however long
/// a person takes to read a freshly booted pane and type into it, not just the
/// group path's near-instant follow-through.
#[doc(hidden)] // pub for integration tests
pub const SOLO_AUTOPILOT_DIALOG_WAIT: Duration = Duration::from_secs(600);

// Echo verification: a paste that landed makes the TUI redraw its input box
// (observable as output bytes). A paste that produced no output within the
// window was flushed by a CLI whose stdin reader wasn't attached yet
// (observed live with copilot, whose input attaches well after its UI
// paints) — wait and retype.
/// How long a paste has to produce echo output before it counts as eaten.
///
/// #535: with `ECHO_ATTEMPTS`/`ECHO_RETRY_DELAY`, an input to
/// `REINJECT_ACK_SETTLE_MS` — the echo loop runs on every delivery, before the
/// Enter, so it is part of the worst case for "could the agent have read this
/// yet". Lengthening any of the three widens that floor automatically.
pub(in crate::orchestration) const ECHO_WINDOW: Duration = Duration::from_millis(2000);
/// Minimum output growth that counts as the input box echoing the paste.
pub(in crate::orchestration) const ECHO_MIN_BYTES: u64 = 8;
/// Pause before retyping after an eaten paste (input attach may be close).
///
/// #535: an input to `REINJECT_ACK_SETTLE_MS` — see `ECHO_WINDOW`. Note this
/// sleep also runs after the FINAL attempt, before the loop condition ends it,
/// so the worst case is `ECHO_ATTEMPTS` of it, not `ECHO_ATTEMPTS - 1`.
pub(in crate::orchestration) const ECHO_RETRY_DELAY: Duration = Duration::from_millis(1500);
/// Total attempts before typing blind and letting the human see the result.
///
/// #535: an input to `REINJECT_ACK_SETTLE_MS` — see `ECHO_WINDOW`.
pub(in crate::orchestration) const ECHO_ATTEMPTS: u32 = 3;
/// Upper bound for `set_state` payloads.
pub(in crate::orchestration) const MAX_STATE_BYTES: usize = 512 * 1024;

/// Cap on a single steered image attachment (#72), in decoded bytes. Sized to
/// comfortably hold a full-screen PNG screenshot while bounding the per-group
/// `attachments/` scratch dir. The steering strip enforces the same limit and
/// toasts on overflow; this is the backstop against a hostile/oversize IPC.
pub const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;

/// Cap on the base64 payload the save-attachment command will decode. Rejecting
/// oversize *before* decode keeps a giant string from ballooning memory — same
/// discipline as the OSC 52 clipboard path. base64 is 4 bytes per 3 input, plus
/// slack for padding/whitespace.
pub const MAX_ATTACHMENT_B64_LEN: usize = MAX_ATTACHMENT_BYTES / 3 * 4 + 16;

/// Merge grants (#83) are one-time and short-lived: a human sign-off authorizes
/// exactly one privileged action within this window, then the grant is consumed
/// or expires. 30 minutes is long enough for CI to finish and the merge to run,
/// short enough that a forgotten grant can't linger as a standing opening.
pub(in crate::orchestration) const GRANT_TTL_SECS: u64 = 30 * 60;

/// Release grants (#438) get their own, longer window, because unlike a merge
/// they are **not** one action: the human's "release vX.Y.Z" authorizes a
/// pipeline — push the tag, wait out the `release.yml` matrix (four platform
/// build legs; 25-40 min is normal), then write the release notes against the
/// release that run created. At the merge grant's 30 minutes the notes step
/// lands *after* expiry on a perfectly ordinary release, and the human gets
/// asked a second time for something they already authorized — which is the
/// complaint #438 was filed about, just moved rather than fixed.
///
/// 90 minutes is a deliberate number and the one worth arguing with: it covers a
/// slow matrix plus a re-run of one failed leg with margin, and it is still a
/// **hard wall** — the shim re-checks it on every step, so a forgotten grant
/// stops authorizing anything the moment it passes, and it never authorizes more
/// than its own tag. The alternative #438 floats — auto-extending the window
/// while pipeline steps keep succeeding — was rejected: a window that any
/// authorized step renews has no bound at all, and "the agent kept working" is
/// not evidence the human still consents.
pub(in crate::orchestration) const RELEASE_GRANT_TTL_SECS: u64 = 90 * 60;
