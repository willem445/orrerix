//! Compaction: the compact-nudge tick (#287) and the context signals it
//! decides from, the cache-idle nudge, and an agent's or the human's own
//! `/compact` request, as an `impl OrchRegistry` block (#3498). The designs
//! are `docs/design/orchestration.md`, `docs/design/compaction-settings.md`
//! and `docs/design/cache-age.md`.

use super::*;

impl OrchRegistry {
    // ---------- compact-nudge (#287): periodic `/compact` at natural lulls ----------

    /// One compact-nudge pass. For each **non-paused** group's running agent
    /// whose role is in that group's `compact_nudge_roles` and whose CLI has a
    /// compact command (`compact_command_for`), fold in the latest
    /// pty output counter using the group's `idle_activity_floor_bytes` —
    /// meaningful growth (a real turn, not idle repaint noise) resets the
    /// quiet clock and this tick's own one-shot latch.
    ///
    /// This deliberately reads the SAME quiet-clock concept `idle_tick_tick`
    /// reads (guardrail #1: reuse the idleness signal, don't invent a second
    /// one) — but rebaselines against its OWN `compact_nudge_last_output_total`,
    /// not `idle_tick_tick`'s `last_output_total` (rev-24 review finding: idle-
    /// tick and compact-nudge can both be watching the SAME agent — default
    /// config targets the orchestrator for both — and an earlier revision
    /// shared one rebaselined counter between them, so whichever tick polled
    /// first each cycle consumed the growth and the other's `idle_output_is_
    /// activity` check permanently saw a zero delta: its latch, once set,
    /// could never clear again). Each tick keeping its own last-seen baseline
    /// on the same monotonic pty counter is the standard fix for two
    /// independent readers of one counter (a consumer-offset shape), not a
    /// second idleness mechanism — both still derive activity from the exact
    /// same source (the pty's `output_total`) via the exact same rule
    /// (`idle_output_is_activity` against the group's `idle_activity_floor_
    /// bytes`). `last_progress_ms` itself stays shared and is safe for both
    /// to advance: each writes it only after independently confirming real
    /// growth from its own baseline, so a write here can only move the quiet
    /// clock's timestamp closer to the truth, never corrupt the other tick's
    /// next comparison.
    ///
    /// An eligible pane quiet past its group's `compact_nudge_minutes`, not
    /// already latched, and under `MAX_COMPACT_NUDGES_PER_HOUR` (reusing
    /// `idle_tick_should_fire`'s gate — the identical shape, not a copy with a
    /// new name) gets `/compact` typed through `deliver_prompt`'s normal
    /// delivery path (`Delivery::MidSession`: a pane write of `/compact` + CR,
    /// no PTY resize, no new agent capability). #287 shipped an immediate,
    /// optional follow-up notice here; #328 replaces it with the MANDATORY
    /// `compact_reinjection_notice` below, delivered once compaction is
    /// actually observed to have finished rather than guessed at the moment
    /// of the paste.
    ///
    /// `deliver_prompt` owns never clobbering a human's unsubmitted line
    /// (#111/#171/#246): if the pane's input box is occupied, it holds up to
    /// the shipped cap and then silently aborts — a held compact is skipped,
    /// not queued, exactly like any other delivery to the orchestrator (no
    /// notice, since a notice about a delivery to the orchestrator would
    /// itself be a delivery to the orchestrator).
    ///
    /// `compact_nudge_minutes` 0 disables the group entirely (the conservative
    /// default). Paused groups are skipped wholesale — same reasoning as
    /// `watchdog_tick`/`idle_tick_tick`. Returns the nudged agent ids. Split
    /// from the pty read (`agent_output_totals`) so the gate/latch/cap/pause
    /// logic is testable with synthetic counters — the `watchdog_tick` shape.
    ///
    /// Two more independent pieces of state ride the same per-agent pass
    /// (#328), each gated so the group's existing behavior is unchanged when
    /// unused:
    /// - **Compaction-completion detection**, shared by all FOUR trigger
    ///   paths (a heuristic/requested fire below; a human typing `/compact`
    ///   manually, detected via `human_typed_compact_detected`; or the CLI's
    ///   own auto-compact banner, detected via `auto_compact_banner_detected`
    ///   — #329 expansion, see below). `AgentEntry.compact_pending` tracks "a
    ///   compact was initiated, not yet resolved"; `compact_seen_busy` is the
    ///   busy half of the busy-then-quiet edge that marks it finished — no
    ///   text-parsing of Claude's own completion output needed, since
    ///   `compact_nudge_tick` already has a busy/quiet detector for its own
    ///   idleness signal. Resolution delivers the MANDATORY
    ///   `compact_reinjection_notice` — slim for almost every agent since
    ///   #417 correction round 5 (the contract already rides the
    ///   system-prompt layer; only the ledger tail and re-sync pointers need
    ///   resending), verbose only for the one documented case where it
    ///   doesn't — superseding #287's optional post-fire notice.
    /// - **Auto-compact banner detection (#329 expansion)**: the CLI's own
    ///   emergency auto-compact never goes through `request_compact`, a
    ///   heuristic fire, or a human typing `/compact` — none of those three
    ///   paths ever set `compact_pending`, so without this the mandatory
    ///   re-injection above would simply never fire for it, which is the
    ///   exact incident that motivated this expansion. Detected from the same
    ///   output tail the manual-`/compact` detector already reads, gated on
    ///   `!compact_pending` (nothing else already tracking this pane) and
    ///   `!currently_quiet` — this tick's OWN fresh-growth signal, not a
    ///   duration-based window: `last_progress_ms` gets rewritten to `now` by
    ///   the growth check whenever the pane is busy for any reason, so a
    ///   window compared against it would read as "fresh" on every busy tick
    ///   regardless of why the tail holds the banner text. Requiring growth
    ///   on the SAME tick the banner is read is the strongest signal actually
    ///   available. Sets `compact_seen_busy` immediately (the banner IS the
    ///   busy signal, caught mid-compaction rather than inferred after the
    ///   fact from a separate growth observation).
    /// - **Context-usage escalation**: once `compact_context_threshold_percent`
    ///   is crossed (percent from `usage::latest_context_tokens`, an exact
    ///   transcript-recorded figure, not a byte proxy — see that fn's doc),
    ///   `compact_escalation_notice` fires once, giving the agent that tick
    ///   to self-request. If it's still over threshold on a LATER tick and
    ///   still hasn't called `request_compact`, loomux sets `compact_requested`
    ///   on its behalf, so a fire follows at the next idle moment regardless.
    pub fn compact_nudge_tick(
        &self,
        now: u64,
        outputs: &HashMap<String, u64>,
        manual_signals: &HashMap<String, (String, u64)>,
        context_percents: &HashMap<String, u32>,
        context_tokens: &HashMap<String, u64>,
        context_boundary_counts: &HashMap<String, u64>,
        delivery_confirmations: &HashMap<String, DeliveryConfirmation>,
    ) -> Vec<String> {
        let paused = self.paused.lock_safe().clone();
        // Snapshot per-group guardrails (Clone) rather than per-agent CLI
        // lookups while holding the agents lock — `Guardrails::cli_for_block`
        // needs the whole roster, and `cli_for_agent` would re-lock
        // `self.groups`. (The resolver was `cli_for` until #2167; the lock
        // reasoning is unchanged, both read `self.blocks`.)
        let groups: HashMap<GroupId, Guardrails> = self
            .groups
            .lock_safe()
            .iter()
            .map(|(id, g)| (id.clone(), g.guardrails.clone()))
            .collect();
        let tick_times = self.compact_nudge_times.lock_safe().clone();

        // The `&'static str` is the CLI's own compact command (`compact_command_for`),
        // read at the admission gate below and pasted verbatim (#413 S4).
        let mut to_fire: Vec<(String, GroupId, &'static str)> = Vec::new();
        // `u32` is the 1-indexed attempt number (see `AgentEntry::
        // compact_reinject_attempts`) — carried through so the audit line
        // distinguishes a first fire from a retry.
        let mut to_reinject: Vec<(String, GroupId, PathBuf, PathBuf, u32, ContractCarrier)> = Vec::new();
        let mut to_escalate: Vec<(String, GroupId, u32)> = Vec::new();
        // Production bug fix (D2/D3): a resolved-but-unconfirmed pending
        // state — audited for visibility (this exact gap in observability is
        // why the live incident took real forensic work to root-cause: no
        // record existed of WHICH detector armed `compact_pending`, or that
        // it had been discarded rather than genuinely resolved).
        let mut to_discard: Vec<(String, GroupId, Option<u64>, Option<u64>)> = Vec::new();
        // rev-42 delta (round 2): a reinjection whose delivery just confirmed
        // — audited for visibility, distinct from the initial fire above.
        // #535: the `&'static str` is the RESOLUTION SOURCE — `"delivery"` (our
        // own submit sampler saw it) or `"activity"` (the agent itself called a
        // loomux tool afterwards). Two different facts; a timeline that
        // conflated them could not show whether #535's fix is working.
        let mut to_reinject_confirmed: Vec<(String, GroupId, ReinjectAck)> = Vec::new();
        // #535: an attempt whose retry window expired while the pane was still
        // mid-turn — deferred rather than spent. Audited (once per attempt, see
        // `compact_reinject_busy_deferred`) because "no retry fired" must read
        // as a deliberate choice, not a silently dropped one — the same reason
        // `to_hook_native_skip` exists.
        let mut to_defer_busy: Vec<(String, GroupId, u32)> = Vec::new();
        // rev-42 delta (round 2): a reinjection stuck past `MAX_REINJECT_
        // ATTEMPTS` — the latch is released anyway (never wedge the state
        // machine), but this must be visible: it's a genuinely lost
        // re-grounding, not a harmless discard.
        let mut to_abandon: Vec<(String, GroupId, u32)> = Vec::new();
        // #410 (round 6): an arm forced open past `ARM_PENDING_TIMEOUT_MS`
        // without ever reaching a busy-then-quiet resolution — audited
        // distinctly from `to_abandon` (which is specifically a stuck
        // reinjection DELIVERY, past the arm phase). The `bool` (round 7) is
        // whether hook evidence was recorded for this arm before it timed
        // out — see the push site's own doc for why "no evidence" would
        // misreport a PreCompact-only arm that simply never went quiet.
        let mut to_arm_timeout: Vec<(String, GroupId, bool)> = Vec::new();
        // #417: audited separately from the pre-existing arm sites — visibility
        // for "this compaction's evidence came from a hook, not a guess" is the
        // whole point of the feature, not incidental.
        let mut to_hook_armed: Vec<(String, GroupId, &'static str)> = Vec::new();
        // rev-4 review (N3): a confirmed arm whose native additionalContext
        // ALREADY delivered the re-grounding — loomux's own reinjection is
        // skipped for it (see `compact_hook_native_notice_delivered`'s doc),
        // audited distinctly so "no reinjection fired" reads as a deliberate
        // choice, not a silently dropped one.
        let mut to_hook_native_skip: Vec<(String, GroupId)> = Vec::new();
        // #428 (round 9): an arm resolved by Copilot's own compaction-
        // completion paint rather than a busy-then-quiet observation —
        // audited distinctly (`compact-resolved-copilot-marker`) so the
        // timeline reads as "the CLI's own screen told us" rather than
        // "loomux inferred it from output going quiet", the same
        // provenance distinction `to_hook_armed` already draws for
        // marker-file evidence vs. inference.
        let mut to_copilot_marker_resolved: Vec<(String, GroupId)> = Vec::new();
        // #413 S5: an arm resolved by Claude's PostCompact marker — audited
        // distinctly (`compact-resolved-postcompact`) for the same provenance
        // reason as the Copilot marker above: the CLI's own hook said the
        // compaction finished, nothing was inferred.
        let mut to_postcompact_resolved: Vec<(String, GroupId)> = Vec::new();
        {
            let mut agents = self.agents.lock_safe();
            for a in agents.values_mut() {
                let Some(g) = groups.get(&a.group) else { continue };
                // One gate for loop admission and the paste (#417, #413 S4):
                // a pane whose CLI row carries no compact command is never
                // compacted, so nothing else in this loop applies to it.
                if a.status != AgentStatus::Running {
                    continue;
                }
                let Some(compact_command) = compact_command_for(g.cli_for_block(&a.block, a.role)) else {
                    continue;
                };

                // Lifecycle-panel surfacing (PR #329 round 6): cache this
                // tick's context-token reading (if any) so `group_summary`
                // can surface it without its own transcript read — see
                // `last_context_tokens`'s doc. Every agent in this loop
                // already cleared the CLI-support gate above, so every
                // reading here is for an agent the panel would want to show
                // usage for anyway.
                if let Some(&tokens) = context_tokens.get(&a.id) {
                    a.last_context_tokens = Some(tokens);
                }

                // Production bug fix (PR #329 round 7): provenance — a
                // CONFIRMED delivery whose `from` is this app's own extends this agent's
                // inference-arm cooldown, so loomux's OWN paste (a `/compact`
                // command, a reinjection/escalation notice) can never have
                // its own echo satisfy loomux's own banner/manual detectors.
                // `from` distinguishes this from a human's own message or
                // another agent's forwarded one reaching the same pane
                // through the same delivery pipeline — neither of those
                // should suppress detection. `.max` never SHORTENS an
                // already-later guard (e.g. one just set by a resolve below).
                if let Some(d) = delivery_confirmations.get(&a.id) {
                    if d.confirmed && brand::is_host_actor(&d.from) {
                        a.compact_inference_guard_until_ms =
                            a.compact_inference_guard_until_ms.max(d.submit_sent_ms + INFERENCE_ARM_COOLDOWN_MS);
                    }
                }

                // Rebaseline / meaningful-growth detection — own baseline
                // (`compact_nudge_last_output_total`, not idle-tick's
                // `last_output_total`) — see the fn doc for why sharing the
                // rebaselined counter with idle-tick was the bug.
                let mut currently_quiet = true;
                if let Some(&cur) = outputs.get(&a.id) {
                    let meaningful = idle_output_is_activity(
                        a.compact_nudge_last_output_total,
                        cur,
                        g.idle_activity_floor_bytes,
                    );
                    a.compact_nudge_last_output_total = cur; // rebaseline every observation
                    if meaningful {
                        a.last_progress_ms = now;
                        a.compact_nudge_notified = false;
                        currently_quiet = false;
                        if a.compact_pending {
                            a.compact_seen_busy = true;
                        }
                    }
                }

                // #417: hook-sourced evidence — a new TRUSTED arm/confirm
                // source, checked before any inference arm site so a
                // configured hook always wins over guessing. `read_hook_
                // marker_ts` reads the marker FILE's mtime (see its doc for
                // why not a timestamp inside the file), so "fresh" means
                // "newer than the last one this agent already consumed" — a
                // hook that never fires just leaves both reads `None`
                // forever, a total no-op for a hook-less setup.
                //
                // rev-4 review (B1, blocking): a marker file has no idea an
                // app can restart. `AgentEntry.compact_hook_*_seen_ms` is
                // in-memory, so it resets to `None` on every restart, and
                // agent ids are minted from an in-memory counter too — a
                // fresh boot can hand out an id a PREVIOUS process already
                // used. Combined: restart, get the same id back, and the
                // first tick reads a marker from the OLD process's hook fire
                // as "fresh" (`ts > None.unwrap_or(0)` is true for any real
                // mtime) — arming TRUSTED with `seen_busy = true`, bypassing
                // every evidence gate, and reinjecting "Context was
                // compacted" with no compaction having happened. Three
                // layers, belt-and-suspenders (this is the TRUSTED tier, so
                // a false positive here is worse than one in the inference
                // tiers below): (1) `ts >= a.started_ms` — a marker can only
                // be evidence for a compaction that happened during THIS
                // agent's own lifetime, which a stale cross-restart marker
                // structurally never was (`>=`, not `>`: `started_ms` and a
                // marker's mtime are both millisecond-resolution real wall
                // clock, so a marker written in the SAME millisecond an
                // agent started is still legitimately its own, not a false
                // rejection); (2) delete-on-consume — once a
                // marker is actually used to arm, it is removed from disk so
                // it can never be re-read as evidence again, independent of
                // whether the in-memory bookkeeping survives; (3) the
                // regression test simulating exactly this sequence (fresh
                // `AgentEntry`, same id, an old marker already on disk).
                let hooks_dir = self.group_dir(&a.group).join("hooks");
                let precompact_marker = hooks_dir.join(format!("{}.precompact.json", a.id));
                let sessionstart_marker = hooks_dir.join(format!("{}.sessionstart-compact.json", a.id));
                let precompact_ts = read_hook_marker_ts(&precompact_marker);
                let sessionstart_ts = read_hook_marker_ts(&sessionstart_marker);
                if let Some(ts) = precompact_ts {
                    if ts > a.compact_hook_precompact_seen_ms.unwrap_or(0)
                        && ts >= a.started_ms
                        && !a.compact_pending
                    {
                        // Direct proof a compaction just STARTED — arm exactly
                        // like the loomux-initiated path (`compact_pending_
                        // trusted = true`: no inference gate needed) and treat
                        // it as already busy, since the compaction itself is
                        // the busy half of busy-then-quiet, whether or not it
                        // grew the pane's own output past the activity floor.
                        a.compact_hook_precompact_seen_ms = Some(ts);
                        let _ = fs::remove_file(&precompact_marker);
                        a.compact_pending = true;
                        a.compact_pending_trusted = true;
                        a.compact_seen_busy = true;
                        a.compact_pending_baseline_tokens = context_tokens.get(&a.id).copied();
                        a.compact_pending_baseline_marker_count = context_boundary_counts.get(&a.id).copied();
                        a.compact_pending_armed_ms = Some(now);
                        a.compact_pending_evidence = Some("hook");
                        to_hook_armed.push((a.id.clone(), a.group.clone(), "precompact"));
                    }
                }
                if let Some(ts) = sessionstart_ts {
                    if ts > a.compact_hook_sessionstart_seen_ms.unwrap_or(0) && ts >= a.started_ms {
                        // Even STRONGER direct proof: Claude Code itself just
                        // told us a session started because of a compact.
                        // Unconditional on `compact_pending` (unlike the
                        // PreCompact marker above) — this covers a hook config
                        // that only has SessionStart, or a PreCompact marker
                        // this tick loop missed/raced.
                        a.compact_hook_sessionstart_seen_ms = Some(ts);
                        let _ = fs::remove_file(&sessionstart_marker);
                        to_hook_armed.push((a.id.clone(), a.group.clone(), "sessionstart-compact"));
                        // Round 7 (live-demo finding): this marker is a
                        // TERMINAL resolution, not just another arm — resolve
                        // RIGHT HERE rather than falling through to the
                        // busy-then-quiet resolver below. Unlike a PreCompact
                        // marker (proof compaction STARTED — genuinely still
                        // needs a later quiet observation to know when it's
                        // done), SessionStart(compact) firing IS proof the
                        // compaction has ALREADY FINISHED (Claude Code
                        // restarted the session because of it) AND that
                        // native `additionalContext` re-grounding already
                        // reached the agent (the ONLY script branch that
                        // emits it — see `COMPACT_HOOK_SCRIPT`'s
                        // `sessionstart-compact` case). There is nothing left
                        // to confirm: waiting on `currently_quiet` here just
                        // races `ARM_PENDING_TIMEOUT_MS` against however long
                        // the agent's own post-compact turn happens to run,
                        // and loses on any fast compact whose agent keeps
                        // working continuously afterward — the EXACT shape
                        // the live incident's audit timeline showed (both
                        // hook-evidence events landed, N3 suppression was the
                        // correct call, then `compact-arm-timeout` fired
                        // anyway at precisely `ARM_PENDING_TIMEOUT_MS` after
                        // the precompact arm, having never observed a quiet
                        // tick). This is the missing 6th terminal path
                        // alongside reinject-confirmed / reinject-abandoned /
                        // arm-timeout / discard / the old busy-then-quiet-
                        // gated native-skip (now folded into this one, since
                        // `compact_hook_native_notice_delivered` can no
                        // longer be `true` by the time that check would run).
                        //
                        // rev-10 review (B1, blocking): this site runs BEFORE
                        // the delivery-confirmation block below, every tick —
                        // so if a busy-then-quiet resolution had ALREADY
                        // decided a loomux reinjection (`compact_reinject_
                        // attempted_ms` set, `compact_pending` still `true`
                        // throughout that phase by design) before this
                        // SessionStart evidence arrived, leaving those two
                        // fields untouched here left the confirmation phase
                        // live against a `compact_pending` this block just
                        // set back to `false`. On a LATER tick that phase
                        // would still fire on its own: a late-confirmed
                        // delivery double-audits the same compaction as
                        // resolved twice, an unconfirmed one RETRIES the
                        // loomux reinjection after native `additionalContext`
                        // already re-grounded the agent (the exact double-
                        // delivery N3 exists to prevent), and an exhausted
                        // one falsely reports "reinjection-abandoned" for a
                        // compaction that in fact succeeded. Worse,
                        // `compact_pending = false` here let a fresh
                        // PreCompact marker re-arm a whole new cycle while
                        // that stale confirmation phase was still retrying —
                        // breaking the "one confirmation phase at a time"
                        // invariant the block below documents. This is a
                        // delivery-phase terminal path exactly like the two
                        // in that block, so it clears the same two fields.
                        let reinject_already_dispatched = a.compact_reinject_attempted_ms.is_some();
                        a.compact_pending = false;
                        a.compact_pending_baseline_tokens = None;
                        a.compact_pending_baseline_marker_count = None;
                        a.compact_pending_trusted = false;
                        a.compact_seen_busy = false;
                        a.compact_pending_armed_ms = None;
                        a.compact_pending_evidence = None;
                        a.compact_hook_native_notice_delivered = false;
                        a.compact_reinject_attempted_ms = None;
                        a.compact_reinject_attempts = 0;
                        // #535: this is a delivery-phase TERMINAL path, so it
                        // clears the deferral latch alongside the two fields
                        // above — the same "same phase, same field inventory"
                        // rule rev-21 review imposed on the sibling paths.
                        a.compact_reinject_busy_deferred = false;
                        a.compact_inference_guard_until_ms = now + INFERENCE_ARM_COOLDOWN_MS;
                        // A loomux reinjection prompt may already have been
                        // PASTED (fire-and-forget, on an earlier tick) before
                        // this native evidence arrived — that delivery
                        // already happened, so it was not "skipped", and
                        // auditing it as such would be false. The genuine
                        // skip case (no reinjection was ever dispatched for
                        // this compaction) is the only one that gets that
                        // audit line; `compact-hook-evidence sessionstart-
                        // compact` above already records that evidence was
                        // seen either way.
                        if !reinject_already_dispatched {
                            to_hook_native_skip.push((a.id.clone(), a.group.clone()));
                        }
                    }
                }

                // #413 S5: Claude's `PostCompact` marker — trusted proof the
                // compaction FINISHED, the resolution half #417's PreCompact
                // arm never had. Checked AFTER the SessionStart block so a
                // same-tick SessionStart(compact) always wins (it is terminal and
                // carries native re-grounding), and BEFORE every inference arm
                // and the busy-then-quiet resolver below, which is the point: an
                // arm this marker speaks for resolves on the hook's word, never
                // on a quiet tick's guess and never through the inference gate.
                //
                // The same three freshness layers as the markers above (`ts`
                // newer than the last consumed, `ts >= a.started_ms` against a
                // cross-restart marker, delete-on-consume). What differs is the
                // settle: the pure `postcompact_marker_disposition` holds a fresh
                // marker for `POSTCOMPACT_SETTLE_MS` so a SessionStart(compact)
                // written a moment later resolves the compaction natively rather
                // than racing loomux's own reinjection into a duplicate
                // re-grounding. While it settles, the open arm is upgraded to
                // trusted-and-busy (the hook has spoken: it can no longer be
                // DISCARDED for want of a token drop) and its resolution is held
                // for the tick — bounded by that window on the tick's own clock.
                let postcompact_marker = hooks_dir.join(format!("{}.postcompact.json", a.id));
                let mut postcompact_settling = false;
                match read_hook_marker_ts(&postcompact_marker)
                    .filter(|&ts| ts > a.compact_hook_postcompact_seen_ms.unwrap_or(0) && ts >= a.started_ms)
                    .filter(|_| false)
                {
                    None => a.compact_hook_postcompact_first_seen_ms = None,
                    Some(ts) => {
                        let first_sight = a.compact_hook_postcompact_first_seen_ms.is_none();
                        if first_sight {
                            to_hook_armed.push((a.id.clone(), a.group.clone(), "postcompact"));
                        }
                        match postcompact_marker_disposition(
                            ts,
                            a.compact_hook_sessionstart_seen_ms,
                            a.compact_reinject_attempted_ms.is_some(),
                            a.compact_hook_postcompact_first_seen_ms,
                            now,
                        ) {
                            PostCompactDisposition::Absorb => {
                                a.compact_hook_postcompact_seen_ms = Some(ts);
                                a.compact_hook_postcompact_first_seen_ms = None;
                                let _ = fs::remove_file(&postcompact_marker);
                            }
                            PostCompactDisposition::Settle => {
                                if first_sight {
                                    a.compact_hook_postcompact_first_seen_ms = Some(now);
                                }
                                if a.compact_pending {
                                    a.compact_pending_trusted = true;
                                    a.compact_seen_busy = true;
                                    a.compact_pending_evidence = Some("hook");
                                }
                                postcompact_settling = true;
                            }
                            PostCompactDisposition::Resolve => {
                                a.compact_hook_postcompact_seen_ms = Some(ts);
                                a.compact_hook_postcompact_first_seen_ms = None;
                                let _ = fs::remove_file(&postcompact_marker);
                                // Enter the delivery-confirmation phase with the
                                // SAME field inventory the busy-then-quiet
                                // "confirmed" branch and the Copilot marker branch
                                // use (rev-21: two paths into one phase reset the
                                // same fields). Unconditional on an arm being open,
                                // like SessionStart(compact): a compaction whose
                                // PreCompact this loop never saw still finished,
                                // and still needs re-grounding.
                                a.compact_pending = true;
                                a.compact_pending_evidence = Some("hook");
                                a.compact_pending_baseline_tokens = None;
                                a.compact_pending_baseline_marker_count = None;
                                a.compact_pending_trusted = false;
                                a.compact_seen_busy = false;
                                a.compact_pending_armed_ms = None;
                                a.compact_reinject_attempts = 1;
                                a.compact_reinject_attempted_ms = Some(now);
                                a.compact_reinject_busy_deferred = false;
                                let instructions = self.group_dir(&a.group).join(
                                    g.block(&a.block)
                                        .map(|b| b.instructions_file())
                                        .unwrap_or_else(|| role_instructions_file(a.role).to_string()),
                                );
                                // #925 — see the sibling sites below.
                                if let Ok(agent_seg) = PathSegment::parse(&a.id) {
                                    let ledger = self.ledger_path(&a.group, &agent_seg);
                                    to_reinject.push((
                                        a.id.clone(), a.group.clone(), instructions, ledger,
                                        1, a.contract_carrier,
                                    ));
                                }
                                to_postcompact_resolved.push((a.id.clone(), a.group.clone()));
                            }
                        }
                    }
                }

                // #428 (round 9): Copilot's own compaction-completion paint
                // as a fast terminal path. GitHub ships Copilot a `preCompact`
                // hook (arms exactly like Claude's, above) but NO post-
                // compact signal at all — no `postCompact` event, and its
                // `sessionStart` fires only on startup|resume|new, never
                // compact (both docs-confirmed in earlier #417 rounds). So
                // an armed Copilot cycle's ONLY resolution before this round
                // was busy-then-quiet — and a compact that finishes while the
                // pane is genuinely idle (nobody prompts it) produces no
                // busy-then-quiet of its own, so it rides the full
                // `ARM_PENDING_TIMEOUT_MS` to a FALSE timeout every time. The
                // live incident (#428): preCompact evidence at +0s, badge
                // stuck at "awaiting evidence" for 240s, resolved only
                // because the user happened to type a question into the
                // pane (busy-then-quiet, coincidentally, 60s before the
                // 300s false-timeout would have fired).
                //
                // RESOLUTION SEMANTICS differ from the SessionStart block
                // above: Copilot has no native `additionalContext` delivery
                // (unlike Claude, its own `--agent` custom-agent file is the
                // only durable channel, and compaction doesn't touch that),
                // so this marker proves the compaction FINISHED, never that
                // re-grounding was delivered. It therefore does NOT skip
                // reinjection (no `to_hook_native_skip`) — it converts the
                // ARM straight into a DECIDED reinjection, the exact same
                // action the busy-then-quiet "confirmed" branch below takes,
                // just without waiting for a quiet tick to observe it.
                //
                // ACCELERATOR, NOT REPLACEMENT: this is UI text Copilot
                // paints, not a documented API (see `copilot_compaction_
                // marker_detected`'s own doc for the fragility this
                // inherits and the exact strings, captured from #428's own
                // incident report, not reconstructed from memory) — busy-
                // then-quiet below still runs unconditionally as the
                // fallback, so a future Copilot release changing this
                // wording degrades back to today's (slower, but correct)
                // behavior, never to a hang.
                //
                // PROVENANCE (#424/#427 lesson, named explicitly in #428's
                // own comment): gated on `a.compact_pending` — an arm must
                // already be open, so a stale mention from a LONG-past,
                // already-resolved compaction sitting in scrollback can
                // never resurrect anything (no B1 risk: nothing here can
                // set `compact_pending` true from `false`, only consume an
                // ALREADY-true one) — and on `now >= a.compact_inference_
                // guard_until_ms`, the same cooldown gate `human_typed_
                // compact_detected`/`auto_compact_banner_detected` use to
                // keep loomux's own recent paste from satisfying its own
                // detector. That cooldown is belt-and-braces here: loomux
                // never writes either matched sentence into anything it
                // pastes (`compact_reinjection_notice`'s three shapes,
                // `compact_escalation_notice`, or the bare `/compact`
                // command) — checked, not assumed, against those exact
                // functions' bodies — so there is no loomux-authored echo
                // this could ever match in the first place.
                //
                // rev-10's B1 lesson, applied here too: this site runs
                // BEFORE the delivery-confirmation block immediately below,
                // every tick — so if a busy-then-quiet resolution (or an
                // earlier marker match) already decided a reinjection
                // (`compact_reinject_attempted_ms` is `Some`), that
                // confirmation phase is already live and must be left
                // completely alone: no re-deciding, no touching its fields.
                // Gating on `.is_none()` makes this a genuine no-op in that
                // case, not a reset — the exact ordering hazard B1 named.
                if a.compact_pending
                    && a.compact_reinject_attempted_ms.is_none()
                    && now >= a.compact_inference_guard_until_ms
                {
                    if let Some((tail, _)) = manual_signals.get(&a.id) {
                        if copilot_compaction_marker_detected(g.cli_for_block(&a.block, a.role), tail) {
                            // rev-21 review: match the SIBLING busy-then-
                            // quiet "confirmed" branch's exact field-reset
                            // inventory below — that branch does NOT clear
                            // `compact_pending_evidence`/`compact_hook_
                            // native_notice_delivered` (both fields stay
                            // meaningful through the delivery-confirmation
                            // phase; e.g. `compact_pending_evidence` is
                            // what keeps a hook-armed cycle's "hook" chip
                            // label through that phase). Two paths that
                            // both enter the SAME phase must reset the SAME
                            // fields, or which one resolved an arm silently
                            // changes what the phase looks like afterward.
                            a.compact_pending_baseline_tokens = None;
                            a.compact_pending_baseline_marker_count = None;
                            a.compact_pending_trusted = false;
                            a.compact_seen_busy = false;
                            a.compact_pending_armed_ms = None;
                            a.compact_reinject_attempts = 1;
                            a.compact_reinject_attempted_ms = Some(now);
                            // #535: fresh attempt — clear the busy-deferral
                            // anti-nag latch so this attempt's own deferral is
                            // still audited (see the field's doc).
                            a.compact_reinject_busy_deferred = false;
                            let instructions = self.group_dir(&a.group).join(
                                g.block(&a.block)
                                    .map(|b| b.instructions_file())
                                    .unwrap_or_else(|| role_instructions_file(a.role).to_string()),
                            );
                            // #925: the ledger is named after the agent, so the
                            // id is proven a single path component before it
                            // names a file. Unreachable for a minted `w-N`/
                            // `orch` id — fail-closed rather than trusted, and
                            // skipping is the safe direction: a path we cannot
                            // name is a file we must not write.
                            if let Ok(agent_seg) = PathSegment::parse(&a.id) {
                                let ledger = self.ledger_path(&a.group, &agent_seg);
                                to_reinject.push((
                                    a.id.clone(), a.group.clone(), instructions, ledger,
                                    1, a.contract_carrier,
                                ));
                            }
                            to_copilot_marker_resolved.push((a.id.clone(), a.group.clone()));
                        }
                    }
                }

                // rev-42 delta (round 2, PR #329 round-5 re-demo): while a
                // decided reinjection is waiting on its delivery to confirm,
                // skip straight to that check — do NOT re-run the busy/
                // confirm decision below (already decided; `compact_pending`
                // stays `true` throughout this phase so no other arm site can
                // re-fire in the meantime).
                //
                // Live re-demo evidence (round 5) showed the previous design
                // — clear the latch the INSTANT `deliver_prompt` is called,
                // regardless of outcome — has a real gap: `deliver_prompt` is
                // fire-and-forget (its `Result` reflects only whether the
                // spawn succeeded, never whether the paste actually landed),
                // so "decided to reinject" was being treated as "reinjected"
                // with no feedback loop if that delivery never confirmed
                // (held for a human typing, the input box never clearing,
                // or any other silent failure). The forensic timeline for
                // that re-demo actually showed something more fundamental —
                // the busy-then-quiet PRECONDITION below never fired at all
                // (see that comment) — but the confirmed-delivery gate is a
                // real, separate gap in its own right and the fix contract
                // requires it regardless of which mechanism this particular
                // re-demo hit.
                if let Some(attempted_ms) = a.compact_reinject_attempted_ms {
                    let confirmed_delivery = delivery_confirmations
                        .get(&a.id)
                        .is_some_and(|d| d.submit_sent_ms >= attempted_ms && d.confirmed);
                    // #535: the agent's OWN post-attempt MCP call — the
                    // acknowledgment this phase was missing. `confirmed_
                    // delivery` above is loomux watching its own paste, and it
                    // is wrong by omission often enough (~25 false unconfirmed
                    // alarms in one observed session) that a LANDED
                    // re-grounding was being re-pasted into a working agent.
                    // See `agent_acted_since` / `reinject_disposition`.
                    //
                    // rev-15 finding 2: the floor, NOT a bare `attempted_ms`.
                    // That stamp is when we decided to paste; the agent cannot
                    // have read anything until the Enter is actually pressed,
                    // so a call it had already decided on last turn would
                    // otherwise resolve this phase for a notice it had not yet
                    // seen. See `REINJECT_ACK_SETTLE_MS` for the derivation.
                    let acked = agent_acted_since(
                        a.last_mcp_activity_ms,
                        attempted_ms.saturating_add(REINJECT_ACK_SETTLE_MS),
                    );
                    // #535: `currently_quiet` is this tick's OWN floor-gated
                    // growth reading, computed above. `busy` is used ONLY to
                    // defer an attempt, never to claim one landed — output
                    // includes our own pasted notice echoing back and
                    // statusline/spinner repaints (#480). The echo cannot
                    // cause a spurious defer anyway: it lands while
                    // `elapsed < REINJECT_CONFIRM_TIMEOUT_MS`, where the
                    // disposition is `Wait` regardless of busy-ness.
                    match reinject_disposition(
                        confirmed_delivery,
                        acked,
                        !currently_quiet,
                        now.saturating_sub(attempted_ms),
                        REINJECT_CONFIRM_TIMEOUT_MS,
                        REINJECT_BUSY_DEFER_MAX_MS,
                    ) {
                        ReinjectDisposition::Resolved => {
                            a.compact_pending = false;
                            a.compact_reinject_attempted_ms = None;
                            a.compact_reinject_attempts = 0;
                            a.compact_reinject_busy_deferred = false;
                            a.compact_pending_evidence = None;
                            a.compact_hook_native_notice_delivered = false;
                            // #410/round-7: the immediate post-compact discussion
                            // is exactly the window an inference arm can misread
                            // as a new compaction — cool down.
                            a.compact_inference_guard_until_ms = now + INFERENCE_ARM_COOLDOWN_MS;
                            // #535: provenance, the same distinction
                            // `to_hook_armed` already draws — "the agent
                            // answered us" is a different fact from "our own
                            // submit sampler saw the Enter", and a timeline
                            // that conflates them cannot be used to tell
                            // whether this fix is working in the field.
                            //
                            // #546: one value, not a string chosen here and
                            // re-interpreted at every surface. It decides the
                            // audit ACTION as well as the `source` field —
                            // a liveness close is not written under a
                            // `-confirmed` action, because it confirmed
                            // nothing. See `ReinjectAck`.
                            let ack = ReinjectAck::from_evidence(confirmed_delivery);
                            // #546: the same provenance, kept on the entry so
                            // it can be SURFACED and not merely audited.
                            // `LivenessOnly` closes this phase on proof the
                            // agent is alive, never on proof it read the
                            // re-grounding, and the lifecycle panel is the
                            // last place a human can notice the difference.
                            // See `compact_last_ack`.
                            a.compact_last_ack = Some(ack);
                            a.compact_last_ack_ms = Some(now);
                            to_reinject_confirmed.push((a.id.clone(), a.group.clone(), ack));
                        }
                        // Still within the timeout, a delivery may be
                        // legitimately in flight (a long human-typing hold) —
                        // wait, don't re-fire yet.
                        ReinjectDisposition::Wait => {}
                        ReinjectDisposition::DeferBusy => {
                            // #535: the window is spent but this pane is
                            // mid-turn. Spend nothing — not the attempt, not
                            // the clock: `compact_reinject_attempted_ms` is
                            // left untouched so the deferral is bounded by
                            // `REINJECT_BUSY_DEFER_MAX_MS` measured from the
                            // ORIGINAL attempt, and cannot be extended a tick
                            // at a time by a pane that simply keeps painting.
                            //
                            // Audited once per attempt, not once per tick: the
                            // poll runs every 10s while any arm is open
                            // (`compact_nudge_poll_interval`), so an unlatched
                            // audit here would write ~30 identical lines per
                            // deferral. Same anti-nag latch shape as
                            // `watchdog_notified` / `compact_nudge_notified`.
                            if !a.compact_reinject_busy_deferred {
                                a.compact_reinject_busy_deferred = true;
                                to_defer_busy.push((
                                    a.id.clone(), a.group.clone(), a.compact_reinject_attempts,
                                ));
                            }
                        }
                        ReinjectDisposition::Retry => {
                            if a.compact_reinject_attempts < MAX_REINJECT_ATTEMPTS {
                                a.compact_reinject_attempts += 1;
                                a.compact_reinject_attempted_ms = Some(now);
                                // Fresh attempt — the previous attempt's
                                // deferral latch must not silence this one's.
                                a.compact_reinject_busy_deferred = false;
                                let instructions = self.group_dir(&a.group).join(
                                    g.block(&a.block)
                                        .map(|b| b.instructions_file())
                                        .unwrap_or_else(|| role_instructions_file(a.role).to_string()),
                                );
                                // #925 — see the sibling site above.
                                if let Ok(agent_seg) = PathSegment::parse(&a.id) {
                                    let ledger = self.ledger_path(&a.group, &agent_seg);
                                    to_reinject.push((
                                        a.id.clone(), a.group.clone(), instructions, ledger,
                                        a.compact_reinject_attempts, a.contract_carrier,
                                    ));
                                }
                            } else {
                                to_abandon.push((a.id.clone(), a.group.clone(), a.compact_reinject_attempts));
                                a.compact_pending = false;
                                a.compact_reinject_attempted_ms = None;
                                a.compact_reinject_attempts = 0;
                                a.compact_reinject_busy_deferred = false;
                                a.compact_pending_evidence = None;
                                a.compact_hook_native_notice_delivered = false;
                                a.compact_last_lost_reason = Some("reinjection-abandoned".to_string());
                                a.compact_last_lost_ms = Some(now);
                                a.compact_inference_guard_until_ms = now + INFERENCE_ARM_COOLDOWN_MS;
                            }
                        }
                    }
                } else if a.compact_pending && !postcompact_settling {
                    // Production bug fix (#410, PR #329 round 6): an arm that
                    // never reaches a busy-then-quiet resolution — a stalled
                    // agent, a compaction that never actually starts, or (the
                    // live incident this closes) a rapid discard/re-arm cycle
                    // that never leaves `compact_pending` open long enough
                    // for a queued `request_compact` to win the race — must
                    // not wedge the state machine forever. Checked BEFORE the
                    // `currently_quiet` gate below (unlike the delivery-
                    // confirmation phase's timeout, which only matters once
                    // already resolved, a stuck ARM might never go quiet at
                    // all, so this bound cannot wait for that).
                    let armed_too_long = a
                        .compact_pending_armed_ms
                        .is_some_and(|armed_ms| now.saturating_sub(armed_ms) >= ARM_PENDING_TIMEOUT_MS);
                    if armed_too_long {
                        // Round 7 (label-honesty finding): a PreCompact-only
                        // arm (no SessionStart wired — e.g. Copilot, or a
                        // Claude config missing that one hook) can still
                        // legitimately hit this bound if the agent's own
                        // post-compact turn simply never goes quiet within
                        // `ARM_PENDING_TIMEOUT_MS` — hook evidence WAS
                        // recorded (see the marker-consumption block above),
                        // it just wasn't enough on its own to auto-resolve.
                        // "no evidence" would misreport that case, so the
                        // reason carries what was actually seen.
                        let had_hook_evidence = a.compact_pending_evidence == Some("hook");
                        to_arm_timeout.push((a.id.clone(), a.group.clone(), had_hook_evidence));
                        a.compact_pending = false;
                        a.compact_seen_busy = false;
                        a.compact_pending_baseline_tokens = None;
                        a.compact_pending_baseline_marker_count = None;
                        a.compact_pending_trusted = false;
                        a.compact_pending_armed_ms = None;
                        a.compact_pending_evidence = None;
                        a.compact_hook_native_notice_delivered = false;
                        a.compact_last_lost_reason = Some(
                            if had_hook_evidence { "arm-timeout-with-evidence" } else { "arm-timeout" }.to_string(),
                        );
                        a.compact_last_lost_ms = Some(now);
                        a.compact_inference_guard_until_ms = now + INFERENCE_ARM_COOLDOWN_MS;
                    } else if currently_quiet {
                    // Compaction-completion: busy (compaction ran/rendered)
                    // then quiet again while pending — resolve regardless of
                    // pause state (a pause must not strand a pane mid-compact
                    // forever).
                    //
                    // Production bug fix (D2/D3, PR #329 delta review): busy-
                    // then-quiet is necessary but not SUFFICIENT — a live
                    // incident showed it can be satisfied by an agent's
                    // ordinary turn, with no compaction ever running.
                    // `compaction_confirmed` requires the context-token
                    // reading to have actually dropped since the baseline
                    // captured when `compact_pending` was set before the
                    // mandatory re-injection is trusted to fire.
                    //
                    // rev-42 delta (deadlock fix): that gate is only sound
                    // for INFERENCE arms (manual `/compact` typing, the
                    // auto-compact banner) — paths where loomux merely
                    // *believes* a compact happened. It deadlocks the
                    // LOOMUX-INITIATED arm (heuristic fallback /
                    // `request_compact`, the one that actually types
                    // `/compact` itself, below): `usage::
                    // latest_context_tokens`'s drop is a NEXT-TURN phenomenon
                    // (proved against a real transcript in `usage::tests::
                    // real_transcript_proves_the_token_drop_is_a_next_turn_
                    // phenomenon_rev42_q1`) and the only next turn available
                    // is the reinjection this gate is supposed to authorize —
                    // a reading taken before that turn is always still-high,
                    // so a uniform gate silently discards genuine
                    // compactions on the primary path forever.
                    // `compact_pending_trusted` (set at each arm site below)
                    // is the fix: trusted arms skip confirmation entirely —
                    // loomux has positive knowledge it pasted `/compact`
                    // itself, so busy-then-quiet IS the signal, same as it
                    // always was before D2/D3 (this was never the
                    // false-positive path that motivated the gate).
                    // Inference arms keep the hard gate, now widened to
                    // `inferred_compaction_confirmed` (token-drop OR the
                    // `compact_boundary` transcript marker).
                    //
                    // rev-42 delta (round 2, the round-5 re-demo's ACTUAL
                    // root cause): "busy" used to mean ONLY real terminal-
                    // output growth clearing `idle_activity_floor_bytes`
                    // (`compact_seen_busy`, set by the rebaseline block
                    // above). The re-demo's forensic timeline (audit.jsonl +
                    // breadcrumbs.log around the genuine, confirmed `/compact`
                    // paste) showed NO reinjection audit line, NO discard
                    // audit line — nothing — for over three minutes while
                    // the agent was demonstrably back to normal work; the
                    // resolver never even reached this branch. The floor-
                    // gated busy check depends on the compaction's OWN
                    // terminal rendering being large enough to clear a
                    // threshold tuned to filter ordinary repaint noise — a
                    // real but small/fast compaction can fail to clear it,
                    // and unlike the two INFERENCE arms (which set
                    // `compact_seen_busy = true` immediately at arm time,
                    // from the very evidence that armed them), the
                    // LOOMUX-INITIATED arm has no such alternate evidence —
                    // it waited PURELY on a later tick's byte-growth
                    // observation, which may simply never come. A rise in
                    // the `compact_boundary` marker is direct, floor-
                    // independent proof the compaction actually happened
                    // (same-turn, no next-turn dependency — same signal
                    // `inferred_compaction_confirmed` already uses), so it
                    // now counts as "seen busy" too, for every arm — closing
                    // this gap without weakening the "don't paste while the
                    // agent is visibly still busy" intent `currently_quiet`
                    // protects (a marker rise only ever WIDENS what counts as
                    // the busy half; the quiet requirement is untouched).
                    //
                    // `compact_pending`/`compact_seen_busy`/both baselines/
                    // the trust flag are ALWAYS cleared on a DISCARD — one
                    // resolution attempt per arming, so a repeatedly-false-
                    // positiving detector can re-arm the state machine as
                    // many times as it wants but can never cause more than
                    // one discarded, context-growth-free no-op per arming
                    // (audited with baseline/current tokens, Q3). A CONFIRM
                    // instead moves into the delivery-confirmation phase
                    // above rather than clearing immediately — see
                    // `compact_reinject_attempted_ms`'s doc.
                    let marker_baseline = a.compact_pending_baseline_marker_count;
                    let marker_current = context_boundary_counts.get(&a.id).copied();
                    let marker_rose = matches!((marker_baseline, marker_current), (Some(b), Some(c)) if c > b);
                    if a.compact_seen_busy || marker_rose {
                        let baseline_tokens = a.compact_pending_baseline_tokens;
                        let current_tokens = context_tokens.get(&a.id).copied();
                        let confirmed = a.compact_pending_trusted
                            || inferred_compaction_confirmed(
                                baseline_tokens,
                                current_tokens,
                                marker_baseline,
                                marker_current,
                            );
                        // rev-4 review (N3), moved to the SessionStart marker-
                        // consumption site in round 7: `a.compact_hook_native_
                        // notice_delivered` can never be `true` here anymore
                        // — that marker is now a TERMINAL resolution the
                        // instant it's consumed (see that block's own doc for
                        // why waiting on `currently_quiet` here was actively
                        // wrong: it raced `ARM_PENDING_TIMEOUT_MS` against
                        // however long the agent's post-compact turn ran, and
                        // lost on any fast compact whose agent kept working —
                        // the live incident this round fixes). What's left
                        // here is purely the INFERENCE-arm and PreCompact-
                        // only-arm path, which still genuinely needs a quiet
                        // observation to know the compaction (and whatever
                        // followed it) has settled.
                        if confirmed {
                            a.compact_pending_baseline_tokens = None;
                            a.compact_pending_baseline_marker_count = None;
                            a.compact_pending_trusted = false;
                            a.compact_seen_busy = false;
                            a.compact_pending_armed_ms = None;
                            a.compact_reinject_attempts = 1;
                            a.compact_reinject_attempted_ms = Some(now);
                            // #535: fresh attempt — clear the busy-deferral
                            // anti-nag latch so this attempt's own deferral is
                            // still audited (see the field's doc).
                            a.compact_reinject_busy_deferred = false;
                            let instructions = self.group_dir(&a.group).join(
                                g.block(&a.block)
                                    .map(|b| b.instructions_file())
                                    .unwrap_or_else(|| role_instructions_file(a.role).to_string()),
                            );
                            // #925 — see the sibling sites above.
                            if let Ok(agent_seg) = PathSegment::parse(&a.id) {
                                let ledger = self.ledger_path(&a.group, &agent_seg);
                                to_reinject.push((a.id.clone(), a.group.clone(), instructions, ledger, 1, a.contract_carrier));
                            }
                        } else {
                            a.compact_pending = false;
                            a.compact_seen_busy = false;
                            a.compact_pending_baseline_tokens = None;
                            a.compact_pending_baseline_marker_count = None;
                            a.compact_pending_trusted = false;
                            a.compact_pending_armed_ms = None;
                            a.compact_pending_evidence = None;
                            a.compact_hook_native_notice_delivered = false;
                            // #410/round-7: the same repeating-false-signal
                            // shape D4 pins can otherwise re-arm on the very
                            // next tick — a short cooldown after ANY discard
                            // gives the state machine room to actually settle
                            // rather than cycling discard-then-immediate-
                            // rearm indefinitely.
                            a.compact_inference_guard_until_ms = now + INFERENCE_ARM_COOLDOWN_MS;
                            to_discard.push((a.id.clone(), a.group.clone(), baseline_tokens, current_tokens));
                        }
                    }
                    }
                }

                // #413 S5: while a PostCompact marker settles, nothing else in
                // this pass acts on the pane — a compaction just FINISHED, so it
                // is no moment to paste another `/compact`, infer a new one from
                // its banner, or escalate on a reading from before it. Bounded by
                // `POSTCOMPACT_SETTLE_MS`, and everything above (token cache,
                // growth rebaseline, the hook markers) has already run.
                if postcompact_settling {
                    continue;
                }

                // A paused group's agents are deliberately quiet; never nudge,
                // escalate, or start a new manual-detection window, and never
                // burn the one-shot latch or the per-hour budget.
                if paused.contains(&a.group) {
                    continue;
                }

                // #410 (round 6): the loomux-initiated (heuristic/requested)
                // fire-check runs FIRST among the four arm sites — moved up
                // from after manual/banner detection/escalation. Live
                // evidence (the incident `ARM_PENDING_TIMEOUT_MS` above also
                // closes) showed a queued `request_compact` starved for 10+
                // minutes: a discard just above can clear `compact_pending`
                // on this SAME tick, and — in the OLD order — an inference
                // arm re-satisfying its own condition on this same tick (a
                // recurring banner mention, an ongoing conversation) would
                // re-arm `compact_pending` before this check ever ran,
                // repeatedly starving the EXPLICIT, already-queued request
                // of the brief window it needs. Running this deterministic,
                // already-decided check first means a fresh discard always
                // gives a queued request first refusal, before any inference
                // arm gets a chance to re-claim the pane this same tick.
                let times = tick_times.get(&a.group).map(Vec::as_slice).unwrap_or(&[]);
                // The loop's own admission gate above (`compact_command_for`)
                // already guarantees this CLI has a compact command to paste,
                // so the fire conditions below don't re-check it — see that
                // gate's doc.
                // Heuristic (role-gated, minutes-threshold, min-context-floor)
                // fallback fire. The floor is loomux's OWN judgment call
                // (never applied to `requested_fires` below — an agent that
                // asks for a compact is always honored regardless of context%).
                let heuristic_fires = compact_nudge_role_allowed(a.role, &g.compact_nudge_roles)
                    && idle_tick_should_fire(
                        a.last_progress_ms,
                        now,
                        g.compact_nudge_minutes,
                        a.compact_nudge_notified,
                        times,
                        MAX_COMPACT_NUDGES_PER_HOUR,
                    )
                    && compact_nudge_context_floor_met(
                        context_percents.get(&a.id).copied(),
                        // #413 S4: a reading with tokens and no window the
                        // panel would publish (codex/pi before a report,
                        // opencode always, no override) fails the floor
                        // CLOSED — see `context_window_unknown`. Read off this
                        // tick's reading as `run_compact_nudge` cached it above.
                        context_window_unknown(
                            context_tokens.get(&a.id).copied(),
                            g.context_window_tokens_override,
                            a.last_context_window,
                            a.last_context_window_rounded,
                            a.last_context_model.as_deref(),
                            a.last_context_source,
                        ),
                        g.compact_nudge_min_context_percent,
                        g.compact_nudge_minutes,
                    );
                // Agent-requested (via `request_compact`, or set on its
                // behalf by escalation BELOW, one tick later than before this
                // round's reorder — see the reorder note above) fire — no
                // role gate (self-initiated), no minutes threshold (the
                // request IS the trigger; `request_compact` itself already
                // refuses a caller whose CLI fails the gate above) and
                // rate-limited (shared budget).
                let requested_fires = a.compact_requested
                    && compact_request_should_fire(currently_quiet, times, now, MAX_COMPACT_NUDGES_PER_HOUR);
                // `!a.compact_pending` (rev-12 review finding): this is the
                // one place that actually types `/compact`, so it is the
                // authoritative choke point — a compact already pending for
                // this pane must never get a second one queued behind it,
                // regardless of how `heuristic_fires`/`requested_fires` got
                // set. A `request_compact` call that lands while already
                // pending is not lost: `compact_requested` just stays set and
                // is honored on a LATER tick, once `compact_pending` clears.
                if !a.compact_pending && (heuristic_fires || requested_fires) {
                    a.compact_nudge_notified = true;
                    a.compact_requested = false;
                    a.compact_pending = true;
                    a.compact_seen_busy = false;
                    a.compact_pending_baseline_tokens = context_tokens.get(&a.id).copied();
                    a.compact_pending_baseline_marker_count = context_boundary_counts.get(&a.id).copied();
                    // Loomux-initiated arm (rev-42 delta): THIS is the arm
                    // that pastes `/compact` itself below — loomux has
                    // positive knowledge the command was submitted, so
                    // busy-then-quiet is sufficient on its own; trusted arms
                    // skip `inferred_compaction_confirmed` entirely (see the
                    // resolver's doc for why a hard gate here deadlocks the
                    // primary path).
                    a.compact_pending_trusted = true;
                    a.compact_pending_armed_ms = Some(now);
                    to_fire.push((a.id.clone(), a.group.clone(), compact_command));
                }

                // Manual `/compact` detection (only while nothing is already
                // pending, and only on a FRESH human write — see
                // `MANUAL_COMPACT_DETECT_WINDOW_MS`'s doc for why the recency
                // gate matters).
                //
                // Production bug fix (PR #329 round 7): also gated on
                // `compact_inference_guard_until_ms` — recency of the
                // human's OWN last keystroke says nothing about whether the
                // `/compact` TOKEN this scan matches is itself fresh; a live
                // demo showed loomux's own earlier `/compact` paste (or the
                // reinjection notice that followed it) can sit in the
                // bounded tail for minutes, and an UNRELATED fresh keystroke
                // nearby was enough to satisfy the old recency check alone
                // and misread that stale echo as a new human-typed command.
                // See `compact_inference_guard_until_ms`'s doc.
                if !a.compact_pending && now >= a.compact_inference_guard_until_ms {
                    if let Some((tail, last_input_ms)) = manual_signals.get(&a.id) {
                        if now.saturating_sub(*last_input_ms) < MANUAL_COMPACT_DETECT_WINDOW_MS
                            && human_typed_compact_detected(tail)
                        {
                            // Inference arm (loomux only believes a human
                            // typed `/compact`) — untrusted, keeps the hard
                            // `inferred_compaction_confirmed` gate above.
                            a.compact_pending = true;
                            a.compact_seen_busy = false;
                            a.compact_pending_baseline_tokens = context_tokens.get(&a.id).copied();
                            a.compact_pending_baseline_marker_count =
                                context_boundary_counts.get(&a.id).copied();
                            a.compact_pending_trusted = false;
                            a.compact_pending_armed_ms = Some(now);
                        }
                    }
                }

                // Auto-compact banner detection (#329 expansion, 4th trigger
                // path): the CLI's OWN emergency auto-compact — no
                // `request_compact` call, no heuristic fire, no human typing
                // `/compact` — so none of the checks above ever see it start.
                // Reuses the same tail already read for manual detection;
                // gated on nothing already pending, plus `!currently_quiet`
                // (computed above, from THIS tick's real output growth) —
                // deliberately NOT a duration-based recency window like
                // `MANUAL_COMPACT_DETECT_WINDOW_MS`: `last_progress_ms` gets
                // rewritten to `now` by the growth check earlier in this same
                // iteration whenever the pane is busy for ANY reason, so
                // comparing "now" against it would trivially read as fresh on
                // every busy tick regardless of why the tail contains the
                // banner text — not a real recency signal at all. Requiring
                // fresh growth on THIS exact tick is the strongest signal
                // actually available: a banner sitting in the tail with NO
                // new output since is definitely stale (an already-resolved
                // compact, or scrollback that never left); still best-effort,
                // same as `human_typed_compact_detected`, if unrelated fresh
                // growth happens to land in the same observation window as
                // old banner text still inside the tail's bounded ring.
                // `compact_seen_busy` is set true immediately, not left for a
                // later tick to observe — the banner match IS the busy
                // signal, caught mid-compaction.
                //
                // Production bug fix (PR #329 round 7): also gated on
                // `compact_inference_guard_until_ms`, same reasoning as the
                // manual-detection gate above — the banner substring can
                // appear in loomux's OWN reinjection notice (it quotes the
                // role instructions verbatim, which may itself describe or
                // mention this feature) or in the immediate post-compact
                // discussion of the test that just ran.
                if !a.compact_pending && !currently_quiet && now >= a.compact_inference_guard_until_ms {
                    if let Some((tail, _)) = manual_signals.get(&a.id) {
                        if auto_compact_banner_detected(g.cli_for_block(&a.block, a.role), tail) {
                            // Inference arm (loomux only believes the CLI's
                            // own auto-compact banner appeared) — untrusted,
                            // keeps the hard `inferred_compaction_confirmed`
                            // gate above.
                            a.compact_pending = true;
                            a.compact_seen_busy = true;
                            a.compact_pending_baseline_tokens = context_tokens.get(&a.id).copied();
                            a.compact_pending_baseline_marker_count =
                                context_boundary_counts.get(&a.id).copied();
                            a.compact_pending_trusted = false;
                            a.compact_pending_armed_ms = Some(now);
                        }
                    }
                }

                // Context-usage escalation: latch on crossing, clear once back
                // under threshold (e.g. after a compact lands). The notice
                // fires ALONE on the crossing tick — giving the agent this
                // tick to self-request before loomux falls back on its
                // behalf on a LATER tick still over threshold, so the
                // fallback request never races the notice with an
                // interleaved `/compact`.
                //
                // #410 (round 6): the fire-check that consumes `compact_
                // requested` now runs ABOVE this block (moved up to fix the
                // request-starvation incident — see its own comment), so a
                // `compact_requested` set HERE can no longer be read by the
                // SAME tick's fire-check at all — it is always honored on a
                // LATER tick, one full tick later than the escalation-notice
                // split above already intended. Harmless: the notice-then-
                // fallback shape this comment describes only ever needed
                // "not the same tick as the notice," never "the very next
                // tick" specifically.
                //
                // Gated on `!a.compact_pending` (rev-12 review finding):
                // without this, a still-over-threshold reading on the tick
                // right after a fallback-triggered fire — before the pane has
                // visibly gone busy from loomux's point of view, so
                // `currently_quiet` is still `true` — re-armed
                // `compact_requested` (reset to `false` when the first
                // `/compact` fired) and the fire-check above would type a
                // SECOND `/compact` into a pane whose first compact hadn't
                // resolved yet. A compact already in flight is reason enough
                // to hold off on both re-notifying and re-arming; the
                // existing quiet-clock resolution above already clears
                // `compact_pending` the moment the first one is actually
                // done, at which point escalation resumes normally.
                if !a.compact_pending {
                    if let Some(&percent) = context_percents.get(&a.id) {
                        if !compact_nudge_role_allowed(a.role, &g.compact_nudge_roles)
                            || percent < g.compact_context_threshold_percent
                            || g.compact_context_threshold_percent == 0
                        {
                            a.compact_escalation_notified = false;
                        } else if compact_escalation_should_fire(
                            percent,
                            g.compact_context_threshold_percent,
                            a.compact_escalation_notified,
                        ) {
                            a.compact_escalation_notified = true;
                            to_escalate.push((a.id.clone(), a.group.clone(), percent));
                        } else if !a.compact_requested {
                            // Still over threshold on a later tick, notice
                            // already delivered, agent still hasn't asked —
                            // request on its behalf. Silent: the notice already
                            // told it what would happen.
                            a.compact_requested = true;
                        }
                    }
                }

            }
        }

        let mut nudged = Vec::new();
        for (id, group, command) in to_fire {
            {
                let mut tt = self.compact_nudge_times.lock_safe();
                let v = tt.entry(group.clone()).or_default();
                v.push(now);
                v.retain(|&t| now.saturating_sub(t) < SPAWN_RATE_WINDOW_MS);
            }
            self.audit(&group, brand::AUDIT_ACTOR, "compact-nudge", json!({ "agent": id, "command": command }));
            let _ = self.deliver_prompt(&id, command, brand::AUDIT_ACTOR, Delivery::MidSession);
            nudged.push(id);
        }
        for (id, group, baseline_tokens, current_tokens) in to_discard {
            self.audit(&group, brand::AUDIT_ACTOR, "compact-pending-discarded", json!({
                "agent": id,
                "reason": "quiet-after-busy without confirmed compaction (no token drop, no compact_boundary marker)",
                "baseline_tokens": baseline_tokens,
                "current_tokens": current_tokens,
            }));
        }
        for (id, group, percent) in to_escalate {
            self.audit(&group, brand::AUDIT_ACTOR, "compact-escalation", json!({ "agent": id, "percent": percent }));
            let _ = self.deliver_prompt(&id, &compact_escalation_notice(percent), brand::AUDIT_ACTOR, Delivery::MidSession);
        }
        for (id, group, instructions_path, ledger_path, attempt, contract_carrier) in to_reinject {
            // #417 correction round 5: `reinject_shape` only reads the
            // instructions file back at all for `ContractCarrier::
            // KickoffOnly` — see `compact_reinjection_notice`'s doc. The
            // other two states never pay for a read they wouldn't use.
            //
            // rev-10 review (N3, round 7), widened to a real three-way
            // choice by rev-16 review (N2, round 8): the path this reads
            // was already computed with a fallback to the class file if the
            // block that owns it is gone from the roster (same `g.block(&a.
            // block).map(..).unwrap_or_else(..)` shape `kickoff_prompt`
            // uses) — but a READ can still come back empty (or fail
            // outright) if that fallback file was itself never written for
            // this exact case, or is momentarily missing (e.g. the #423
            // sweep reclaiming a block's file out from under a still-live
            // agent). `unwrap_or_default()` used to turn that into a
            // verbose notice with an EMPTY contract body, silently — worse
            // than no notice at all, since it *looks* like re-grounding
            // happened. Loud instead: `reinject_shape` degrades a failed
            // `KickoffOnly` read to `Pointer`, never `Slim` (which would
            // falsely claim durability this agent doesn't have) — audited
            // here specifically for that degraded case, since a genuine
            // `SystemLayerCore` agent's `Pointer` shape is the CORRECT
            // outcome, not something to flag.
            let shape = reinject_shape(&instructions_path, contract_carrier);
            if contract_carrier == ContractCarrier::KickoffOnly && matches!(shape, ReinjectShape::Pointer) {
                self.audit(&group, brand::AUDIT_ACTOR, "compact-reinjection-contract-unreadable", json!({
                    "agent": id,
                    "path": instructions_path.display().to_string(),
                }));
            }
            // Missing/empty ledger reads as "" and `directive_ledger_embed`
            // turns that into `None` — nothing embeds for a pane that never
            // called `note_directive`, so this is a no-op for every session
            // until the directive-ledger feature is actually used.
            let ledger = fs::read_to_string(&ledger_path).unwrap_or_default();
            let ledger_path_str = ledger_path.display().to_string();
            let ledger_embed = directive_ledger_embed(&ledger, DIRECTIVE_LEDGER_EMBED_CAP_BYTES, &ledger_path_str);
            let shape_label = match &shape {
                ReinjectShape::Slim => "slim",
                ReinjectShape::Pointer => "pointer",
                ReinjectShape::Verbose(_) => "verbose",
            };
            self.audit(&group, brand::AUDIT_ACTOR, "compact-reinjection",
                json!({ "agent": id, "ledger_embedded": ledger_embed.is_some(), "attempt": attempt,
                        "shape": shape_label }));
            let instructions_path_str = instructions_path.display().to_string();
            let notice = compact_reinjection_notice(&shape, &instructions_path_str, &ledger_path_str, ledger_embed.as_deref());
            // `Delivery::Regrounding`, not `MidSession` (#1161 M2). This is the
            // ONE mid-session text orrerix may type into a `Role::Manager` pane
            // (decision D2), and it is spelled as its own delivery kind so that
            // carve-out is a property of the enum rather than a bypass at this
            // call site. It behaves identically to `MidSession` in every other
            // respect, which its own test pins — nothing about how a compacted
            // pane is re-grounded changes for any other class.
            let _ = self.deliver_prompt(&id, &notice, brand::AUDIT_ACTOR, Delivery::Regrounding);
        }
        // rev-42 delta (round 2): terminal-state audits for the confirmed-
        // delivery gate — visibility for both outcomes the fix contract
        // requires (exactly one delivered re-grounding, or a bounded, visible
        // give-up), neither of which existed before this round.
        //
        // #546: the ACTION NAME says what was proven. A phase closed on the
        // agent's own liveness is written as `compact-reinjection-liveness-
        // only`, never as `-confirmed` — nothing was confirmed, and
        // `audit.jsonl` is the surface that outlives the badge, so a reader
        // counting confirmations there was counting liveness closes among
        // them. `proves`/`does_not_prove` ride along so the record is
        // self-describing rather than requiring the reader to already know
        // which `source` is the weak one. See `ReinjectAck`.
        for (id, group, ack) in to_reinject_confirmed {
            self.audit(&group, brand::AUDIT_ACTOR, ack.audit_action(), json!({
                "agent": id,
                "source": ack.wire(),
                "proves": ack.proves(),
                "does_not_prove": ack.does_not_prove(),
            }));
        }
        // #535: a retry withheld because the pane was mid-turn. Distinct action
        // name from every other outcome above: this is neither a fire nor a
        // give-up, and reading it as either would misreport a healthy pane.
        for (id, group, attempt) in to_defer_busy {
            self.audit(&group, brand::AUDIT_ACTOR, "compact-reinjection-deferred-busy", json!({
                "agent": id,
                "attempt": attempt,
                "reason": "retry window elapsed while the pane was still producing output — \
                           deferring rather than interrupting a live turn",
                "bounded_by_ms": REINJECT_BUSY_DEFER_MAX_MS,
            }));
        }
        for (id, group, attempts) in to_abandon {
            self.audit(&group, brand::AUDIT_ACTOR, "compact-reinjection-abandoned", json!({ "agent": id, "attempts": attempts }));
        }
        for (id, group, had_hook_evidence) in to_arm_timeout {
            self.audit(&group, brand::AUDIT_ACTOR, "compact-arm-timeout", json!({
                "agent": id,
                "reason": "arm never reached a busy-then-quiet resolution within ARM_PENDING_TIMEOUT_MS",
                "had_hook_evidence": had_hook_evidence,
            }));
        }
        for (id, group, event) in to_hook_armed {
            self.audit(&group, brand::AUDIT_ACTOR, "compact-hook-evidence", json!({ "agent": id, "event": event }));
        }
        for (id, group) in to_hook_native_skip {
            self.audit(&group, brand::AUDIT_ACTOR, "compact-reinjection-skipped-native", json!({
                "agent": id,
                "reason": "SessionStart hook already delivered native additionalContext for this compaction",
            }));
        }
        for (id, group) in to_postcompact_resolved {
            self.audit(&group, brand::AUDIT_ACTOR, "compact-resolved-postcompact", json!({
                "agent": id,
                "reason": "Claude's PostCompact hook reported the compaction finished and no SessionStart(compact) re-grounding arrived within the settle window",
                "settle_ms": POSTCOMPACT_SETTLE_MS,
            }));
        }
        for (id, group) in to_copilot_marker_resolved {
            self.audit(&group, brand::AUDIT_ACTOR, "compact-resolved-copilot-marker", json!({
                "agent": id,
                "reason": "Copilot's own compaction-completion paint resolved the arm without waiting for busy-then-quiet",
            }));
        }
        nudged
    }

    /// Compact-nudge (#328): per-agent (ANSI-stripped output tail, last
    /// human-input Unix-ms) snapshot for the manual-`/compact`-detection
    /// pass. Impure (pty read); split from the decision so
    /// `human_typed_compact_detected` and the recency gate stay
    /// synthetic-input testable. A dead pty or no app handle simply omits
    /// that agent — a missing entry never counts as a detected manual
    /// compact.
    fn agent_compact_signals(&self) -> HashMap<String, (String, u64)> {
        let Some(app) = self.app.lock_safe().clone() else {
            return HashMap::new();
        };
        let ptys = app.state::<crate::pty::PtyManager>();
        self.compact_signals_from(&ptys)
    }

    /// [`Self::agent_compact_signals`] with the pty manager passed in — the
    /// EXACT body the compact-nudge cadence runs, minus the `AppHandle`
    /// lookup, so an integration test can drive it against a real
    /// fake-child-backed `PtyManager` (the `preenter_admission` /
    /// `flush_stranded_text` seam, and the same reason for it: nothing can
    /// resolve `self.app` headless, so the inline version was
    /// undeletable-by-test).
    ///
    /// **The snapshot is the point (#743 S7, `performance.md` INV-5/P3.)** The
    /// `agents` lock is taken to clone out `(id, pty_id)` and RELEASED before
    /// any pty is touched — the same map-lock→release→leaf shape
    /// `PtyManager::writer_handle` states for the pty side, and the same one
    /// the attention scan (#725) already uses. Iterating the guard directly
    /// held `agents` across N whole-ring clones and N `strip_ansi` passes, on
    /// a cadence that goes to 10 s whenever any agent anywhere has a compact
    /// arm open — so a single slow pane serialised every reader of the agent
    /// registry behind it, including the attention scan and the usage poll
    /// that runs on the webview thread.
    ///
    /// The tail read itself stays whole-ring, deliberately: three detectors
    /// read it with three different reaches — `auto_compact_banner_detected`
    /// wants only the last non-blank line, `human_typed_compact_detected`
    /// wants anything typed inside `MANUAL_COMPACT_DETECT_WINDOW_MS` (3 min),
    /// and the Copilot completion marker sits wherever the CLI last painted
    /// it. Narrowing the window would shorten the middle one's reach, which
    /// is a detection change, not a lock change; the cost it would save is
    /// already bounded and argued at [`Self::any_compact_pending`], and the
    /// cheaper lever named there (skip the read for agents that cannot
    /// possibly be mid-compact) does not cost reach at all. Deferred to that
    /// lever rather than done here — see performance.md §4 X5.
    #[doc(hidden)] // pub for integration tests
    pub fn compact_signals_from(
        &self,
        ptys: &crate::pty::PtyManager,
    ) -> HashMap<String, (String, u64)> {
        let panes: Vec<(String, u32)> = self
            .agents
            .lock_safe()
            .values()
            .filter_map(|a| Some((a.id.clone(), a.pty_id?)))
            .collect();
        panes
            .into_iter()
            .filter_map(|(id, pty_id)| {
                let tail = strip_ansi(&ptys.output_tail(pty_id)?);
                let last_input = ptys.last_user_input_ms(pty_id).unwrap_or(0);
                Some((id, (tail, last_input)))
            })
            .collect()
    }

    /// Production bug fix (PR #329 delta review, rev-42 Q4): ONE transcript
    /// tail-read per Running Claude-CLI agent with a resolvable session id —
    /// EVERY one, not gated on the group's escalation threshold, since the
    /// reading is load-bearing for CORRECTNESS (the busy-then-quiet
    /// resolver's confirmation gate), not just the opportunistic escalation
    /// notice. Replaces the former `agent_context_percents` +
    /// `agent_context_tokens` pair, which each did their own whole-file read
    /// against the same transcripts — `agent_context_percents` now derives
    /// its percents from this map instead of re-reading, closing that
    /// double-read. Via `usage::compaction_signal_in` (bounded tail-read),
    /// carrying both the context-token reading (percent/gating) and the
    /// `compact_boundary` marker count (`inferred_compaction_confirmed`'s
    /// no-next-turn-required signal). Impure (disk read); split from the
    /// decision so `compaction_confirmed`/`inferred_compaction_confirmed`
    /// stay synthetic-input testable.
    ///
    /// #993: select each running pane's reader from its resolved CLI. Claude
    /// starts with its transcript and enriches it from a matching status-line
    /// snapshot; Codex reads the newest rollout via the store lookup. The
    /// parsers and path resolvers stay separate so neither CLI's missing or
    /// compressed artifact can borrow another pane's reading.
    ///
    /// #993 S2b: pi reads its session file from the GROUP's own store
    /// (`pi_sessions_dir`, the `--session-dir` both launch forms hand every
    /// group pi pane — the store the usage meter's pi arm reads, never the
    /// per-user `sessions::pi_sessions_root`, which a group pane does not write
    /// to). Its window is looked up in the cached `--list-models` probe for the
    /// model the file names, and its effort falls back to the pane's block
    /// knob — the `--thinking` value it was launched with — when the tail holds
    /// no `thinking_level_change`. See `modelstate::pi_compaction_signal_in`.
    ///
    /// #993 S2c: opencode reads the group's own SQLite store
    /// (`opencode_db_path`) on one read-only connection: model and variant
    /// (as effort) from the session row, tokens from the newest counted
    /// assistant message, and no window — the store records none. See
    /// `modelstate::opencode_compaction_signal_in`.
    ///
    /// #413 S4: the codex rollout is found through `codex_rollout_path`'s
    /// per-session memo, not a store walk per tick.
    #[doc(hidden)] // pub for the codex, pi and opencode context-reader integration tests
    pub fn agent_context_signals(&self) -> HashMap<String, crate::usage::CompactionSignal> {
        self.agent_context_signals_for_group(None)
    }

    /// [`Self::agent_context_signals`], restricted to `only_group`'s running
    /// agents when one is named — the usage sampler's read (#993 S6), which
    /// wants one group's readings and would otherwise read every group's tails
    /// on each group's compute. `None` is every group (the compact-nudge tick).
    #[doc(hidden)] // pub so `tests/piusage.rs` can pin that the filter holds (#3571)
    pub fn agent_context_signals_for_group(
        &self,
        only_group: Option<&GroupId>,
    ) -> HashMap<String, crate::usage::CompactionSignal> {
        let rows: Vec<(String, String, GroupId, workflow::BlockId, Role)> = self
            .agents
            .lock_safe()
            .values()
            .filter(|agent| agent.status == AgentStatus::Running)
            .filter(|agent| only_group.map_or(true, |group| &agent.group == group))
            .filter_map(|agent| {
                Some((
                    agent.id.clone(),
                    agent.session_id.clone()?,
                    agent.group.clone(),
                    agent.block.clone(),
                    agent.role,
                ))
            })
            .collect();
        if rows.is_empty() {
            return HashMap::new();
        }
        let guardrails: HashMap<GroupId, Guardrails> = self
            .groups
            .lock_safe()
            .iter()
            .filter(|(id, _)| only_group.map_or(true, |group| *id == group))
            .map(|(id, group)| (id.clone(), group.guardrails.clone()))
            .collect();
        // The block's effort knob rides along for pi's fallback. Resolved the
        // way `cli_for_block` resolves the CLI — the agent's own block, else its
        // class default — so the two describe the same block. Already clamped
        // to what the CLI honors (`Guardrails::clamped`), so it is the value the
        // launch line passed as `--thinking`.
        let candidates: Vec<(String, String, GroupId, String, String)> = rows
            .into_iter()
            .filter_map(|(id, sid, group, block, role)| {
                let rails = guardrails.get(&group)?;
                let cli = rails.cli_for_block(&block, role).to_string();
                let effort = rails
                    .block(&block)
                    .or_else(|| rails.block_for(role))
                    .map(|b| b.effort.clone())
                    .unwrap_or_default();
                Some((id, sid, group, cli, effort))
            })
            .collect();
        let claude_root = self
            .claude_projects_dir
            .lock_safe()
            .clone()
            .or_else(crate::usage::default_claude_projects_root);
        let codex_root = crate::sessions::codex_sessions_root();

        candidates
            .into_iter()
            .filter_map(|(id, sid, group, cli, effort)| match cli.as_str() {
                "pi" => {
                    let session = PathSegment::parse(&sid).ok()?;
                    let signal = crate::modelstate::pi_compaction_signal_in(
                        &self.pi_sessions_dir(&group),
                        &session,
                        Some(effort.as_str()),
                        &|model| crate::modelstate::probe_window("pi", model),
                    )?;
                    Some((id, signal))
                }
                "codex" => {
                    let root = codex_root.as_ref()?;
                    let session = PathSegment::parse(&sid).ok()?;
                    let path = self.codex_rollout_path(root, &session, std::time::Instant::now())?;
                    Some((id, crate::modelstate::codex_compaction_signal_at(&path)?))
                }
                // #993 S2c: the group's own store, where every group opencode
                // pane's `OPENCODE_DB` points. The session id is a SQL
                // parameter here, never a path segment, so it is not parsed.
                "opencode" => Some((
                    id,
                    crate::modelstate::opencode_compaction_signal_in(&self.opencode_db_path(&group), &sid)?,
                )),
                "claude" => {
                    let root = claude_root.as_ref()?;
                    let signal = crate::usage::compaction_signal_in(root, &sid)?;
                    // #925: the id becomes a file name, so it is parsed first;
                    // a roster id always parses, and one that did not would
                    // simply get no enrichment.
                    let snapshot = PathSegment::parse(&id)
                        .ok()
                        .and_then(|seg| fs::read_to_string(statusline_snapshot_path(&self.root, &group, &seg)).ok())
                        .and_then(|text| crate::modelstate::parse_statusline_snapshot(&text));
                    let signal = crate::modelstate::enrich_with_statusline(signal, snapshot.as_ref(), &sid);
                    Some((id, signal))
                }
                _ => None,
            })
            .collect()
    }

    /// The rollout `session` resolves to under the codex store `root`: the
    /// remembered path while `modelstate::reuse_remembered_rollout` allows it
    /// (one stat), else a fresh `find_codex_session_file` walk whose answer is
    /// remembered at `now` (#3531, #413 S4). The walk and the stat both run
    /// outside `codex_rollout_paths`, so no file I/O happens under the lock.
    ///
    /// A walk that finds nothing is NOT remembered: a pane whose first rollout
    /// line has not been written yet is the ordinary case at launch, and
    /// caching its absence would hide the first reading for a whole interval.
    /// The residual is that such a session is walked on every tick until its
    /// rollout appears — the pre-memo cost, for exactly the panes that have no
    /// reading to lose.
    fn codex_rollout_path(&self, root: &Path, session: &PathSegment, now: std::time::Instant) -> Option<PathBuf> {
        use crate::modelstate::{reuse_remembered_rollout, RememberedRollout, CODEX_ROLLOUT_REVALIDATE_AFTER};
        let key = (root.to_path_buf(), session.as_str().to_string());
        let remembered = self.codex_rollout_paths.lock_safe().get(&key).cloned();
        if let Some(r) = remembered {
            if reuse_remembered_rollout(&r, now, CODEX_ROLLOUT_REVALIDATE_AFTER, r.path.is_file()) {
                return Some(r.path);
            }
        }
        let found = loomux_engine::sessions::find_codex_session_file(root, session);
        let mut memo = self.codex_rollout_paths.lock_safe();
        memo.retain(|_, r| now.saturating_duration_since(r.resolved) < CODEX_ROLLOUT_REVALIDATE_AFTER);
        match &found {
            Some(path) => {
                memo.insert(key, RememberedRollout { path: path.clone(), resolved: now });
            }
            None => {
                memo.remove(&key);
            }
        }
        found
    }

    /// Compact-nudge (#328): current context-window usage percent per agent
    /// whose group has escalation enabled (`compact_context_threshold_percent
    /// > 0`). Derived from `signals` (already read once for every running
    /// agent by `agent_context_signals`, rev-42 Q4) — no transcript read of its
    /// own. Split from the decision so `compact_escalation_should_fire` stays
    /// synthetic-input testable.
    ///
    /// #413 S4: a percent needs a window `modelstate::published_window`
    /// accepts — the ONE rule the lifecycle panel publishes by, so the panel
    /// never shows a percent the escalation refuses, or the reverse. A reading
    /// with tokens and no such window (opencode always; codex or pi before
    /// their CLI reports one, and with no group override) gets no percent, so
    /// it never escalates and reads as "unknown" to the idle-compact backstop.
    /// The lull floor refuses the same reading separately, through
    /// `context_window_unknown` over the agent's cached reading — it fails
    /// closed there, not open. Those readings come back as the second half,
    /// for `note_unwindowed_escalations` to audit.
    fn agent_context_percents(
        &self,
        signals: &HashMap<String, crate::usage::CompactionSignal>,
    ) -> (HashMap<String, u32>, Vec<UnwindowedReading>) {
        // Production bug fix (PR #329 round 7): threshold AND the override
        // both come from the same per-group guardrails snapshot, so the
        // escalation percent below is computed against the SAME window the
        // lifecycle panel shows (see `effective_context_window_tokens`) —
        // no more flat 200K assumption that fires the escalation ~5x too
        // early for a larger-context agent.
        let (thresholds, overrides): (HashMap<GroupId, u32>, HashMap<GroupId, Option<u64>>) = {
            let groups = self.groups.lock_safe();
            (
                groups.iter().map(|(id, g)| (id.clone(), g.guardrails.compact_context_threshold_percent)).collect(),
                groups.iter().map(|(id, g)| (id.clone(), g.guardrails.context_window_tokens_override)).collect(),
            )
        };
        let agent_groups: HashMap<String, GroupId> = self
            .agents
            .lock_safe()
            .iter()
            .map(|(id, a)| (id.clone(), a.group.clone()))
            .collect();
        let mut percents = HashMap::new();
        let mut unwindowed = Vec::new();
        for (id, sig) in signals {
            let Some(group) = agent_groups.get(id) else { continue };
            if thresholds.get(group).copied().unwrap_or(0) == 0 {
                continue;
            }
            let Some(tokens) = sig.tokens else { continue };
            let override_tokens = overrides.get(group).copied().flatten();
            let window = crate::modelstate::published_window(
                effective_context_window_tokens(
                    override_tokens,
                    sig.window_tokens,
                    sig.window_rounded,
                    sig.model.as_deref(),
                    Some(tokens),
                ),
                Some(sig.source),
                sig.window_tokens,
            );
            match window {
                Some((window, _)) => {
                    percents.insert(id.clone(), context_percent_used(tokens, window));
                }
                None => unwindowed.push(UnwindowedReading {
                    agent: id.clone(),
                    group: group.clone(),
                    tokens,
                    source: sig.source,
                }),
            }
        }
        (percents, unwindowed)
    }

    /// Audit, once per episode, each escalation-eligible agent whose reading
    /// has tokens but no window a percent may be computed against (#413 S4):
    /// the threshold is on for its group, its role is one the nudge serves and
    /// its CLI has a compact command, so a human reading the timeline would
    /// otherwise expect an escalation that can never come. `unwindowed` is this
    /// tick's list (`agent_context_percents`); the latch is
    /// `compact_unwindowed_noted`, rebuilt from it, so a reading that gains a
    /// window leaves the set and a later tokens-only stretch is audited anew.
    fn note_unwindowed_escalations(&self, unwindowed: &[UnwindowedReading]) {
        // Two sequential snapshots, never one lock held across the other.
        let rails: HashMap<GroupId, Guardrails> = {
            let groups = self.groups.lock_safe();
            unwindowed
                .iter()
                .filter_map(|r| Some((r.group.clone(), groups.get(&r.group)?.guardrails.clone())))
                .collect()
        };
        let seats: HashMap<String, (Role, workflow::BlockId)> = {
            let agents = self.agents.lock_safe();
            unwindowed
                .iter()
                .filter_map(|r| agents.get(&r.agent).map(|a| (r.agent.clone(), (a.role, a.block.clone()))))
                .collect()
        };
        let eligible: Vec<&UnwindowedReading> = unwindowed
            .iter()
            .filter(|r| {
                let (Some(rails), Some((role, block))) = (rails.get(&r.group), seats.get(&r.agent)) else { return false };
                compact_nudge_role_allowed(*role, &rails.compact_nudge_roles)
                    && compact_command_for(rails.cli_for_block(block, *role)).is_some()
            })
            .collect();
        let fresh: Vec<&UnwindowedReading> = {
            let mut noted = self.compact_unwindowed_noted.lock_safe();
            let fresh = eligible.iter().copied().filter(|r| !noted.contains(&r.agent)).collect();
            *noted = eligible.iter().map(|r| r.agent.clone()).collect();
            fresh
        };
        for r in fresh {
            self.audit(&r.group, brand::AUDIT_ACTOR, "compact-escalation-skipped", json!({
                "agent": r.agent,
                "reason": "context window unknown: the reading has tokens but no reported window and no group override, so no percent exists to escalate on",
                "tokens": r.tokens,
                "source": r.source.as_str(),
            }));
        }
    }

    /// rev-42 delta (round 2): per-agent snapshot of the most recent delivery
    /// to that agent's pane, resolved via its CURRENT `pty_id` — a fresh read
    /// every tick, so a pane that was rebound (a new pty after a resume)
    /// simply reads as "no delivery yet" rather than a stale one from a
    /// now-dead pty. Impure (locks `self.last_delivery`); split from the
    /// decision so `compact_nudge_tick`'s reinjection-confirmation resolver
    /// stays synthetic-input testable — see `DeliveryConfirmation`'s doc for
    /// why this can't just be read live off `deliver_prompt` itself.
    fn agent_last_deliveries(&self) -> HashMap<String, DeliveryConfirmation> {
        let pty_ids: Vec<(String, u32)> = self
            .agents
            .lock_safe()
            .values()
            .filter_map(|a| Some((a.id.clone(), a.pty_id?)))
            .collect();
        if pty_ids.is_empty() {
            return HashMap::new();
        }
        let last_delivery = self.last_delivery.lock_safe();
        pty_ids
            .into_iter()
            .filter_map(|(id, pty_id)| {
                let outcome = last_delivery.get(&pty_id)?;
                Some((id, DeliveryConfirmation {
                    submit_sent_ms: outcome.submit_sent_ms,
                    confirmed: outcome.confirmed,
                    from: outcome.from.clone(),
                }))
            })
            .collect()
    }

    /// Round 10 (#428 follow-up): whether ANY agent, in any group, currently
    /// has a compact arm open (`AgentEntry::compact_pending`) — the single
    /// input `compact_nudge_poll_interval` needs to pick the loop's next
    /// sleep. Deliberately registry-wide, not per-group/per-agent: the
    /// compact-nudge thread is one loop serving every group, so the fast
    /// cadence has to be "on" if it would help ANYONE waiting, off only when
    /// truly nobody is. A stale read is impossible by construction — this
    /// takes the same lock `compact_nudge_tick` itself locks, read fresh
    /// every call, never cached.
    ///
    /// rev-25 review, cost of that breadth named explicitly (deliberate, not
    /// overlooked): ONE pending arm anywhere upgrades the poll cadence for
    /// EVERY agent everywhere, not just the affected one — `agent_compact_
    /// signals` (what the faster cadence makes run more often) reads every
    /// agent's FULL pty tail (`PtyManager::output_tail`, capped at
    /// `OUTPUT_RING_CAP` = 256 KiB) and ANSI-strips it, gated only on having
    /// a pty at all, not on pending/eligibility. Measured bound: at 6
    /// wakes/min (the 10s fast cadence) that's ≤ 256 KiB × 6 = 1.5 MiB/min
    /// PER AGENT, sub-1 MiB/s in aggregate even for a large fleet (tens of
    /// agents); normal sessions see this for seconds, not minutes, since
    /// most arms resolve on the very next fast wake. The theoretical worst
    /// case for how long the elevated cadence can run at all is bounded by
    /// the state machine itself — `ARM_PENDING_TIMEOUT_MS` (5 min) if an arm
    /// never resolves, or up to `MAX_REINJECT_ATTEMPTS` (3) ×
    /// `REINJECT_CONFIRM_TIMEOUT_MS` (5 min) if it resolves just before
    /// timing out and then every retry stalls — under 20 minutes either way,
    /// never unbounded. If this bound ever needs tightening in practice, two
    /// cheap options, neither built speculatively here: (a) scope the fast
    /// cadence per-group instead of registry-wide (this fn becomes `any_
    /// compact_pending_in(group)`, and `start_compact_nudge` would need one
    /// loop iteration per group or a per-group sleep instead of one shared
    /// sleep); (b) have `agent_compact_signals` skip the tail read entirely
    /// for an agent that is neither `compact_pending` nor otherwise
    /// eligible for compact-nudge's inference detectors, so the elevated
    /// cadence's extra cost lands only on agents it could possibly matter
    /// for.
    ///
    /// #413 S5: a `PostCompact` marker in its settle window counts too. It may
    /// have arrived with no arm open (a compaction whose PreCompact this loop
    /// never saw), and without this its resolution would wait on the idle
    /// cadence rather than the tick after `POSTCOMPACT_SETTLE_MS`.
    pub fn any_compact_pending(&self) -> bool {
        self.agents
            .lock_safe()
            .values()
            .any(|a| a.compact_pending || a.compact_hook_postcompact_first_seen_ms.is_some())
    }

    /// One full compact-nudge cycle: read pty counters, then tick. Called on a
    /// timer by `start_compact_nudge`; `now` injected so tests drive it
    /// deterministically.
    pub fn run_compact_nudge(&self, now: u64) -> Vec<String> {
        let Some(_tick) = self.tick_gate("run_compact_nudge") else { return Vec::new() };
        let outputs = self.agent_output_totals();
        let manual_signals = self.agent_compact_signals();
        let signals = self.agent_context_signals();
        let (context_percents, unwindowed) = self.agent_context_percents(&signals);
        self.note_unwindowed_escalations(&unwindowed);
        let context_tokens: HashMap<String, u64> = signals
            .iter()
            .filter_map(|(id, s)| s.tokens.map(|t| (id.clone(), t)))
            .collect();
        let context_boundary_counts: HashMap<String, u64> = signals
            .iter()
            .map(|(id, s)| (id.clone(), s.compact_boundary_count))
            .collect();
        let delivery_confirmations = self.agent_last_deliveries();
        // Production bug fix (PR #329 round 7): cache each agent's model
        // from this SAME read (no extra transcript access) so `group_
        // summary` can compute a model-aware percent on its own, much more
        // frequent poll cadence — same "cache from the background tick,
        // don't re-read per poll" shape as `last_context_tokens`. Not
        // threaded through `compact_nudge_tick` itself: nothing in the
        // decision logic needs it, so keeping it out of that pure function's
        // parameter list means no churn to its ~50 existing test call sites.
        {
            let mut agents = self.agents.lock_safe();
            for (id, sig) in &signals {
                let Some(a) = agents.get_mut(id) else { continue };
                if let Some(model) = &sig.model {
                    a.last_context_model = Some(model.clone());
                }
                // #993 S1: unlike the model, the reported window FOLLOWS the
                // signal — including back to `None` when this tick's reading
                // carries no matching snapshot (a resumed session whose status
                // line has not run yet), so a previous session's window can
                // never outlive it. A tick with no signal at all (a transient
                // read miss) leaves it alone, like the model.
                a.last_context_window = sig.window_tokens;
                a.last_context_window_rounded = sig.window_rounded;
                // #993 S3: an observed effort only — a pi launch fallback is the
                // block's knob, already published as `declared.effort`, and
                // publishing it here too would pass configuration off as a
                // reading (the rule S6's samples follow).
                a.last_context_effort = sig.observed_effort().map(str::to_owned);
                a.last_context_source = Some(sig.source);
            }
        }
        let nudged = self.compact_nudge_tick(
            now,
            &outputs,
            &manual_signals,
            &context_percents,
            &context_tokens,
            &context_boundary_counts,
            &delivery_confirmations,
        );
        // #3407: AFTER the compact-nudge pass, so the quiet clock it reads has
        // already folded this tick's output growth, and so a compact that pass
        // just armed reads as `compact_busy` here rather than being nudged twice.
        self.cache_idle_nudge_tick(now, &context_percents);
        nudged
    }

    /// The orchestrator's idle-compact backstop (#3407 acceptance criterion 3):
    /// an orchestrator that has gone quiet with NOTHING in flight is told, once,
    /// to compact while its prompt cache is still warm — so the compaction
    /// request itself reads a warm cache and the next real wake re-reads a
    /// small context rather than the whole session cold.
    ///
    /// The resident prompt carries the same rule as one line; this is the
    /// orrerix-side backstop for an orchestrator that forgot it. Decision in
    /// [`loomux_engine::cacheage::idle_compact_should_fire`] (the band, the
    /// floor, the fail-closed context); what "in flight" means is gathered
    /// here, because it is registry state:
    ///
    /// - a live **delegate** — a worker, reviewer or planner not yet dead (a
    ///   manager or lead is the human's own pane and never "in flight");
    /// - a pending **watch** (`notify_when`) in the group. Every live watch
    ///   counts: orrerix knows when a watch EXPIRES, never when its target will
    ///   resolve, so "fires within the TTL" is not something it can rule out;
    /// - pending **intake** for the group, or a **queued delivery** for this
    ///   pane;
    /// - a live **review or plan drive** — read off the drive files last, only
    ///   for a pane that would otherwise fire, and an unreadable file counts as
    ///   in flight ("I could not look" is not "nothing there").
    ///
    /// Never mid-decision: `idle_ms` is the same output-quiet clock the
    /// compact-nudge and idle ticks read, and the notice goes through
    /// `deliver_prompt`, which holds for a human's occupied input box. Paused
    /// groups are skipped, as by every sibling tick. Returns the nudged ids.
    pub fn cache_idle_nudge_tick(&self, now: u64, context_percents: &HashMap<String, u32>) -> Vec<String> {
        // Three phases (review round 1, rev-std finding 1). GATHER takes every
        // registry snapshot, each on its own short lock, and holds none after.
        // DECIDE reads only that snapshot plus the per-pane queue and the drive
        // files, and writes nothing. APPLY is the only phase that mutates state:
        // latches, audit, delivery. So a decision can never observe its own
        // half-applied writes.
        let snap = self.cache_idle_gather(now);
        let plan = self.cache_idle_decide(snap, context_percents);
        self.cache_idle_apply(plan)
    }

    /// GATHER for [`Self::cache_idle_nudge_tick`]: the running orchestrators of
    /// unpaused groups, plus the group-level "in flight" sets, each read on its
    /// own short lock.
    fn cache_idle_gather(&self, now: u64) -> CacheIdleSnapshot {
        let paused = self.paused.lock_safe().clone();
        let groups: HashMap<GroupId, Guardrails> = self
            .groups
            .lock_safe()
            .iter()
            .map(|(id, g)| (id.clone(), g.guardrails.clone()))
            .collect();
        let watch_groups: HashSet<GroupId> =
            self.watches.lock_safe().values().map(|w| w.group.clone()).collect();
        let intake_groups: HashSet<GroupId> = self
            .intake_pending
            .lock_safe()
            .iter()
            .filter(|(_, p)| !p.is_empty())
            .map(|(g, _)| g.clone())
            .collect();
        let agents = self.agents.lock_safe();
        let delegate_groups = agents
            .values()
            .filter(|a| {
                matches!(a.role, Role::Worker | Role::Reviewer | Role::Planner)
                    && a.status != AgentStatus::Dead
            })
            .map(|a| a.group.clone())
            .collect();
        let candidates = agents
            .values()
            .filter(|a| {
                a.role == Role::Orchestrator
                    && a.status == AgentStatus::Running
                    && !paused.contains(&a.group)
            })
            .map(|a| CacheIdleCandidate {
                id: a.id.clone(),
                group: a.group.clone(),
                block: a.block.clone(),
                pty_id: a.pty_id,
                idle_ms: now.saturating_sub(a.last_progress_ms),
                latched: a.cache_idle_nudge_latched,
                compact_busy: a.compact_pending || a.compact_requested,
            })
            .collect();
        CacheIdleSnapshot { groups, watch_groups, intake_groups, delegate_groups, candidates }
    }

    /// DECIDE for [`Self::cache_idle_nudge_tick`]: for each candidate, either
    /// release its latch (evidence the idle stretch ended), fire (the pure
    /// [`loomux_engine::cacheage::idle_compact_should_fire`] plus the drive
    /// files), or do nothing. Writes nothing. The drive files are read last,
    /// and only for a pane that would otherwise fire.
    fn cache_idle_decide(&self, snap: CacheIdleSnapshot, context_percents: &HashMap<String, u32>) -> CacheIdlePlan {
        use loomux_engine::cacheage;
        let mut plan = CacheIdlePlan::default();
        for c in snap.candidates {
            let Some(g) = snap.groups.get(&c.group) else { continue };
            let cli = g.cli_for_block(&c.block, Role::Orchestrator);
            if compact_command_for(cli).is_none() {
                continue;
            }
            let ttl = cacheage::effective_ttl_minutes(
                g.block(&c.block).and_then(|b| b.cache_ttl_minutes),
                cli,
            );
            // The heuristic nudge's floor setting, resolved the way that feature
            // resolves it when it is ON — `None` is the smart default, `Some(0)`
            // an explicit "no floor" — and independent of whether the heuristic
            // lull timer itself is enabled: this backstop is always on.
            let floor = g
                .compact_nudge_min_context_percent
                .unwrap_or(DEFAULT_COMPACT_NUDGE_MIN_CONTEXT_PERCENT);
            let pct = context_percents.get(&c.id).copied();
            let cheap_in_flight = snap.delegate_groups.contains(&c.group)
                || snap.watch_groups.contains(&c.group)
                || snap.intake_groups.contains(&c.group)
                || c.pty_id.is_some_and(|p| self.queue_depth(p) > 0);
            if c.latched {
                if cheap_in_flight || pct.is_some_and(|p| p < floor) || self.drives_in_flight(&c.group) {
                    plan.release.push(c.id);
                }
                continue;
            }
            let inputs = cacheage::IdleCompactInputs {
                idle_ms: c.idle_ms,
                ttl_minutes: ttl,
                in_flight: cheap_in_flight,
                context_percent: pct,
                floor_percent: floor,
                latched: false,
                compact_busy: c.compact_busy,
            };
            if cacheage::idle_compact_should_fire(&inputs) && !self.drives_in_flight(&c.group) {
                let (Some(p), Some(t)) = (pct, ttl) else { continue };
                plan.fire.push((c, p, t));
            }
        }
        plan
    }

    /// APPLY for [`Self::cache_idle_nudge_tick`]: latches first (one `agents`
    /// lock), then the audit line and the delivery per fire, with no lock held.
    fn cache_idle_apply(&self, plan: CacheIdlePlan) -> Vec<String> {
        use loomux_engine::cacheage;
        if !plan.release.is_empty() || !plan.fire.is_empty() {
            let mut agents = self.agents.lock_safe();
            for id in &plan.release {
                if let Some(a) = agents.get_mut(id) {
                    a.cache_idle_nudge_latched = false;
                }
            }
            for (c, _, _) in &plan.fire {
                if let Some(a) = agents.get_mut(&c.id) {
                    a.cache_idle_nudge_latched = true;
                }
            }
        }
        let mut nudged = Vec::new();
        for (c, pct, ttl) in plan.fire {
            self.audit(&c.group, brand::AUDIT_ACTOR, "cache-idle-nudge", json!({
                "agent": c.id,
                "idle_ms": c.idle_ms,
                "ttl_minutes": ttl,
                "context_percent": pct,
            }));
            let _ = self.deliver_prompt(
                &c.id,
                &cacheage::idle_compact_notice(pct, ttl),
                brand::AUDIT_ACTOR,
                Delivery::MidSession,
            );
            nudged.push(c.id);
        }
        nudged
    }


    /// Whether a review or plan drive is live in `group` (#3407's backstop).
    /// An unreadable drive file answers `true`: the backstop may not tell an
    /// orchestrator "nothing is in flight" off a file it could not read.
    fn drives_in_flight(&self, group: &GroupId) -> bool {
        let dir = self.group_dir(group);
        let review = match reviewdrive::load_state(&dir) {
            Ok(st) => st.entries.iter().any(|e| e.state().is_live()),
            Err(_) => true,
        };
        review
            || match plandrive::load_state(&dir) {
                Ok(st) => st.entries.iter().any(|e| e.state().is_live()),
                Err(_) => true,
            }
    }

    /// Compact-nudge (#328): self-scoped agent request. Sets `compact_
    /// requested` on the CALLING agent's own entry ONLY — the token that
    /// resolves to `agent_id` is the entire trust surface, so there is no
    /// `group_id`-style path segment and no cross-pane power (mirrors
    /// `report`/`message_orchestrator`'s self-scoping). Returns a clear
    /// not-supported error — the CLI's `compact_note`, via
    /// `compact_unsupported_reason` — for a CLI with no compact command
    /// (`compact_command_for`) rather than silently flagging a request that
    /// can never fire. For an orchestrator caller, appends
    /// `compact_checklist_warning`'s soft nudge (never a block — the call
    /// always succeeds) if `set_state` hasn't landed recently.
    ///
    /// Deliberately does NOT check `compact_pending` itself (rev-12 review
    /// asked this be verified for consistency with `compact_nudge_tick`'s
    /// pending gate): a call landing while a compact is already in flight for
    /// this pane is not an error and not lost — `compact_requested` just sits
    /// set, and `compact_nudge_tick`'s fire-check (the ONE place that
    /// actually types `/compact`, and the sole authoritative gate on
    /// `compact_pending`) honors it on a later tick once the in-flight
    /// compact resolves, rather than firing a second one concurrently. The
    /// response says so rather than implying "at your next idle moment" is
    /// necessarily the very next one.
    pub fn request_compact(&self, agent_id: &str) -> Result<String, String> {
        let a = self.agent(agent_id).ok_or("unknown agent")?;
        let cli = self.cli_for_agent(&a);
        if compact_command_for(&cli).is_none() {
            return Err(format!("request_compact is not supported here — {}", compact_unsupported_reason(&cli)));
        }
        if let Some(e) = self.agents.lock_safe().get_mut(agent_id) {
            e.compact_requested = true;
        }
        self.audit(&a.group, agent_id, "compact-requested", json!({}));
        let warning = (a.role == Role::Orchestrator)
            .then(|| compact_checklist_warning(a.last_state_write_ms, now_ms(), SET_STATE_RECENCY_WINDOW_MS))
            .flatten();
        let when = if a.compact_pending {
            "queued — a compact is already in flight for this pane; yours will fire once it resolves"
        } else {
            "will fire at your next idle moment"
        };
        Ok(match warning {
            Some(w) => format!("compact requested — {when} ({w})"),
            None => format!("compact requested — {when}"),
        })
    }

    /// The human's "Compact now" (#3407): the cache-age chip's menu item, on
    /// any agent pane. Rides [`Self::request_compact`]'s exact path — it sets
    /// `compact_requested`, and `compact_nudge_tick`'s fire check is still the
    /// ONE place that types `/compact`, at the pane's next quiet observation,
    /// arming the post-compact re-grounding like any other trusted fire. So
    /// "now" means "the next idle moment", never mid-turn, and the response
    /// says which.
    ///
    /// Takes the group the chip was rendered under and refuses an agent that
    /// is not in it: holding a valid group id is not membership (CLAUDE.md
    /// constraint 6), and the webview names both. Audited with `by: human`, so
    /// the timeline tells a human's compact from the agent's own.
    pub fn human_request_compact(&self, group: &GroupId, agent_id: &str) -> Result<String, String> {
        let a = self.agent(agent_id).ok_or("unknown agent")?;
        if a.group != *group {
            return Err("that agent is not in this group".into());
        }
        if a.status == AgentStatus::Dead {
            return Err("that agent is no longer running".into());
        }
        let cli = self.cli_for_agent(&a);
        if compact_command_for(&cli).is_none() {
            return Err(compact_unsupported_reason(&cli));
        }
        if let Some(e) = self.agents.lock_safe().get_mut(agent_id) {
            e.compact_requested = true;
        }
        // What the fire check will actually do with the flag, read the way
        // `compact_nudge_tick` reads it: a paused group is skipped outright,
        // and a requested fire draws on the group's shared hourly budget.
        // The reply says which, instead of promising a paste that is not
        // coming (review round 1, N3).
        let paused = self.is_paused(group);
        let budget_spent = self
            .compact_nudge_times
            .lock_safe()
            .get(group)
            .is_some_and(|t| spawn_rate_exceeded(t, now_ms(), MAX_COMPACT_NUDGES_PER_HOUR, SPAWN_RATE_WINDOW_MS));
        let reply = human_compact_reply(paused, a.compact_pending, budget_spent);
        self.audit(group, "human", "compact-requested", json!({
            "agent": agent_id,
            "by": "human",
            "paused": paused,
            "budget_spent": budget_spent,
        }));
        Ok(reply)
    }
}
