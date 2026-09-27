//! Stranded prompts and hold episodes: pushing, actuating, re-wording and
//! dismissing a stranded entry, re-delivering a lost kickoff, the
//! provider-limit and question-held latches, and the drainer's hold episodes
//! and their escalation, as an `impl OrchRegistry` block (#3498). The janitor
//! that finds stranded prompts is in `delivery.rs`. The design is
//! `docs/design/orchestration.md`.

use super::*;

impl OrchRegistry {
    /// Push a `StrandedSubmit` marker to the FRONT of `pty_id`'s queue
    /// (#445 seam 3): the drainer's single-consumer discipline means only
    /// it ever pops the front, so this is safe to call whether the queue
    /// was empty (a fresh delivery's own seam-3 abort — the marker becomes
    /// the only entry) or non-empty (the drainer converting the item it's
    /// mid-replaying). Bounded by the SAME cap as a text enqueue — a marker
    /// still occupies a queue slot.
    ///
    /// **`reason` is the abort's, and there are two of them (#560).** The
    /// caller is `run_queue_drainer`'s `DeliverOutcome::AbortedPreEnter(reason)`
    /// arm, and since #532 that reason is `BoxOccupied` as often as it is
    /// `Question` — the pre-Enter gate has two causes. This took no reason at
    /// all and recorded `question` for both, so a marker left by a human's
    /// half-typed line was audited as a dialog that was never on screen: the
    /// same false claim #560 fixes one call site over, reached by dropping the
    /// information rather than by hardcoding over it. Both reasons are honest
    /// as they stand (see `EnqueueReason::BoxOccupied`'s doc, amended for the
    /// pre-Enter case), so this needs no new variant — only the parameter.
    #[doc(hidden)] // pub for integration tests
    pub fn enqueue_stranded_front(
        &self,
        group: &GroupId,
        agent_id: &str,
        from: &str,
        pty_id: u32,
        reason: queue::EnqueueReason,
    ) -> Result<(), String> {
        self.recover_persisted_queue(group); // before an id is minted — see `enqueue_text`
        let target = self.durable_target(agent_id);
        let pushed = self.queues.mutate(group, self, |queues| {
            let q = queues.entry(pty_id).or_default();
            // #560: the abort's own reason, forwarded — the marker is the
            // remainder of the delivery the pre-Enter gate declined, and which
            // of that gate's two causes declined it is the whole of what this
            // line records.
            let pushed =
                self.push_stranded_front_locked(q, agent_id, from, group, &target, reason);
            // A refused push (the pane is at cap) leaves the queue exactly
            // as it was — see `QueueDirty::nothing_persisted` for why the
            // `entry().or_default()` above does not count either.
            let dirty = if pushed.is_ok() {
                queuestate::QueueDirty::snapshot()
            } else {
                queuestate::QueueDirty::nothing_persisted()
            };
            (pushed, dirty)
        });
        let out = self.audit_stranded_push(group, agent_id, pushed, reason).map(|_id| ());
        // #563: either the marker took a slot (depth up) or it was refused at
        // cap — both are capacity facts, and the refusal is the one that
        // leaves pasted-but-unsubmitted text in the pane with nothing queued
        // to press it. `audit_stranded_push` records the loss; this makes it
        // visible on a pane whose in-band notice channel is suppressed.
        self.note_queue_capacity(group, pty_id, Some(agent_id));
        out
    }

    /// The push itself, performed on an ALREADY-HELD `queues` entry (#496
    /// PR-C rev-47 B1). Extracted so `admit_stranded_selfheal` can fuse its
    /// gate check and this push into ONE critical section without
    /// duplicating the push — the pre-fix shape called `enqueue_stranded_
    /// front`, which re-took the lock, and that gap was the race. Returns
    /// `Ok((id, depth))` or `Err(depth)` when the pane's queue is at cap (a
    /// marker occupies a slot like any other entry). Audits nothing: the
    /// caller is still holding a lock, and `audit` does file I/O.
    ///
    /// **#562: this used to be the one place in the mutation-site table
    /// where the mutation and its persistence obligation lived in different
    /// functions** — and, correspondingly, the row the table's own
    /// re-derivation dropped. It no longer is: both callers reach this from
    /// inside a `QueueMap::mutate` closure, so the write follows the
    /// mutation whether or not either caller remembers it exists.
    ///
    /// **`reason` is the caller's, never this function's (#560).** It shipped
    /// hardcoding `EnqueueReason::Question`, which is true of the drainer's
    /// push — a pre-Enter question really did decline that Enter — and false
    /// of `admit_stranded_selfheal`'s, whose trigger is a pane that went QUIET
    /// with our text stranded in it. A shared helper cannot know which, so it
    /// no longer guesses: same mislabel, and same fix, as `AbortedPreEnter`
    /// carrying its own reason since #532 rev-12 NB1.
    fn push_stranded_front_locked(
        &self,
        q: &mut VecDeque<queue::QueuedDelivery>,
        agent_id: &str,
        from: &str,
        group: &GroupId,
        target: &DurableTarget,
        reason: queue::EnqueueReason,
    ) -> Result<(u64, usize), usize> {
        if q.len() >= queue::QUEUE_MAX_PER_PANE {
            return Err(q.len());
        }
        let id = self.queue_seq.fetch_add(1, Ordering::SeqCst) + 1;
        q.push_front(queue::QueuedDelivery {
            id, agent_id: agent_id.to_string(), from: from.to_string(),
            payload: queue::QueuedPayload::StrandedSubmit,
            reason, enqueued_ms: now_ms(), coalesced: 0,
            // Stamped like any other entry even though a marker can never
            // be REPLAYED after a restart (`queue::split_recovered`): the
            // recovery path still has to name which group's snapshot it came
            // out of and which pane it was for, to audit and surface the
            // loss instead of dropping it quietly.
            group: Some(group.clone()),
            to_orchestrator: target.is_orchestrator,
            session_id: target.session_id.clone(),
            // #620: a marker is an Enter press against text ALREADY in the
            // pane's input box — the boot it might once have been part of is
            // long over by the time one is pushed, and there is no payload to
            // re-deliver if it is lost. `MidSession` is the only kind that
            // describes it.
            delivery_kind: Delivery::MidSession,
        });
        Ok((id, q.len()))
    }

    /// The audit tail shared by both marker-push call sites — run AFTER the
    /// `queues` lock is released, never under it.
    ///
    /// `reason` is the one the push was admitted under, and is echoed rather
    /// than re-decided here (#560): this function writes the `delivery-queued`
    /// line a human greps to reconstruct a wedge, and a second hardcoded
    /// `"question"` at the point of RECORDING would put the lie back the
    /// threading through `push_stranded_front_locked` just took out. The
    /// refusal line's `blocked_reason` takes it for the same reason — a
    /// marker refused at cap leaves text pasted in a pane with nothing to
    /// submit it, and which trigger got that far is the first thing the reader
    /// of that line needs.
    fn audit_stranded_push(
        &self,
        group: &GroupId,
        agent_id: &str,
        pushed: Result<(u64, usize), usize>,
        reason: queue::EnqueueReason,
    ) -> Result<u64, String> {
        match pushed {
            Err(depth) => {
                // #563: names what was lost, like the front door's rejection
                // does. A marker carries no text — what it stands for IS the
                // fact recorded here, and without `payload` a reader cannot
                // tell this line apart from a rejected prompt.
                self.audit(group, brand::AUDIT_ACTOR, "delivery-dropped", json!({
                    "to": agent_id, "reason": "queue-full-at-call", "depth": depth,
                    "payload": "stranded-submit",
                    "consequence": "text is pasted in the pane with nothing queued to submit it",
                }));
                Err(queue::queue_full_error(agent_id, depth, reason.as_str()))
            }
            Ok((id, depth)) => {
                self.audit(group, brand::AUDIT_ACTOR, "delivery-queued", json!({
                    "to": agent_id, "id": id, "reason": reason.as_str(),
                    "depth": depth, "marker": "stranded-submit",
                }));
                Ok(id)
            }
        }
    }

    /// Admit a self-heal re-submit for a stranded delivery (#496 PR-C) — the
    /// admission half of `actuate_stranded`, split out so the ordering
    /// property ("the marker drains BEFORE anything queued behind it, and
    /// nothing is pasted on top of the stranded text") is testable without
    /// an `AppHandle`.
    ///
    /// **One critical section, not three (rev-47 B1).** The gate check and
    /// the push happen under a SINGLE acquisition of `queues`, with
    /// `queue_draining` consulted while that lock is held. The pre-fix shape
    /// — peek `queues`, release; read `queue_draining`, release; re-take
    /// `queues` to push — was check-then-act across three scopes, and a
    /// drainer registering in the gap re-opened the very duplicate-delivery
    /// hazard the gate exists to close: gate sees no drainer, a delivery is
    /// admitted and its drainer spawns/peeks the Text entry as front, the
    /// marker is then pushed in front of it, and the drainer's completing
    /// `pop_front_dequeued(text id)` finds a mismatched front, pops nothing,
    /// and leaves an ALREADY-DELIVERED entry queued for a second delivery.
    /// That window is not exotic: the delivery most likely to arrive at that
    /// instant is the idle tick's nudge to the same wedged pane, and the
    /// idle tick and `DeclareFailed` key off the same quiet condition, so
    /// they are correlated rather than independent.
    ///
    /// Fusing them closes it: any drainer that could ever peek this front
    /// must register BEFORE it peeks (`ensure_drainer`) and must take
    /// `queues` TO peek — so relative to this critical section it is either
    /// already registered (the fused gate sees it and declines) or it peeks
    /// strictly after the push (and finds the marker at the front, which is
    /// the safe case: it drains the submit first, popping it by a matching
    /// id). The lock ORDER — `queues`, then `queue_draining` — is exactly
    /// `commit_exit`'s, the only other place both are held at once, so this
    /// introduces no new deadlock edge. `queue.rs`'s
    /// `stranded_admission_property` proves the fused rule over every
    /// interleaving, with the unfused variant as its mutation control.
    ///
    /// See `stranded_admission_gate` for both decline reasons and why
    /// declining loses nothing.
    #[doc(hidden)] // pub for integration tests
    pub fn admit_stranded_selfheal(
        &self,
        group: &GroupId,
        agent_id: &str,
        from: &str,
        pty_id: u32,
    ) -> Result<bool, String> {
        enum Admission {
            Declined(&'static str),
            Pushed(Result<(u64, usize), usize>),
        }
        self.recover_persisted_queue(group); // before an id is minted — see `enqueue_text`
        let target = self.durable_target(agent_id);
        let admission = self.queues.mutate(group, self, |queues| {
            let front_is_marker = queues
                .get(&pty_id)
                .and_then(|q| q.front())
                .is_some_and(|e| matches!(e.payload, queue::QueuedPayload::StrandedSubmit));
            // Nested INSIDE `queues`, matching `commit_exit`'s lock order.
            let drainer_active = self.queue_draining.is_registered(pty_id);
            match stranded_admission_gate(drainer_active, front_is_marker) {
                // A decline touches nothing at all — the gate read the
                // queue and left it exactly as it found it.
                Some(reason) => (Admission::Declined(reason), queuestate::QueueDirty::nothing_persisted()),
                None => {
                    let q = queues.entry(pty_id).or_default();
                    // #560: the self-heal's own reason. Nothing is on screen
                    // here — the trigger is a QUIET pane holding our stranded
                    // text — so the `Question` this shared helper used to
                    // hardcode sent a human reading `audit.jsonl` looking for a
                    // dialog that never existed.
                    let pushed = self.push_stranded_front_locked(
                        q, agent_id, from, group, &target, queue::EnqueueReason::StrandedSelfHeal);
                    let dirty = if pushed.is_ok() {
                        queuestate::QueueDirty::snapshot()
                    } else {
                        queuestate::QueueDirty::nothing_persisted()
                    };
                    (Admission::Pushed(pushed), dirty)
                }
            }
        });
        match admission {
            Admission::Declined(reason) => {
                self.audit(group, brand::AUDIT_ACTOR, "stranded-selfheal-skipped",
                    json!({ "to": agent_id, "reason": reason }));
                Ok(false)
            }
            Admission::Pushed(pushed) => {
                // #563: before the `?` — a rejected marker is exactly the case
                // whose capacity state most needs reporting, and an early
                // return would skip it.
                self.note_queue_capacity(group, pty_id, Some(agent_id));
                self.audit_stranded_push(
                    group, agent_id, pushed, queue::EnqueueReason::StrandedSelfHeal)?;
                self.audit(group, brand::AUDIT_ACTOR, "stranded-selfheal-submit",
                    json!({ "to": agent_id, "via": "queue-front-marker" }));
                Ok(true)
            }
        }
    }

    /// Act on one `stranded_selfheal_action` decision (#496 PR-C): admit the
    /// re-submit through the delivery queue, or raise the pane's attention
    /// badge — and audit either way, so every self-heal attempt and every
    /// "loomux could not act" is in the record.
    ///
    /// Returns whether a heal was actually admitted, which is what the late
    /// monitor counts against `STRANDED_SELFHEAL_MAX_HEALS`: a declined
    /// admission (a marker already queued, or a full queue) must not burn
    /// the budget for a submit that never happened.
    ///
    /// The badge is raised for a heal too, not only for the blocked cases —
    /// a delivery that needed healing at all already burned its whole
    /// confirm window plus `PENDING_IDLE_QUIET`, and the human is entitled
    /// to know their group wedged even when loomux recovers it. It clears on
    /// the monitor's next tick once the ledger confirms (see
    /// `clear_stranded`), so a successful heal is a brief badge, not a
    /// sticky one.
    #[doc(hidden)] // pub for integration tests
    pub fn actuate_stranded(
        &self,
        group: &GroupId,
        agent_id: &str,
        from: &str,
        pty_id: u32,
        action: StrandedAction,
    ) -> bool {
        let blocker = match action {
            StrandedAction::Resolved => {
                self.clear_stranded(group, agent_id, "resolved");
                return false;
            }
            StrandedAction::Attention(b) => Some(b),
            StrandedAction::SelfHeal => None,
        };
        let mut healed = false;
        let blocker = match blocker {
            Some(b) => Some(b),
            None => match self.admit_stranded_selfheal(group, agent_id, from, pty_id) {
                Ok(true) => {
                    healed = true;
                    // Best-effort nudge, the same shape `deliver_prompt`'s
                    // behind-the-queue path uses: a drainer may already be
                    // running for this pane, and `ensure_drainer` is
                    // idempotent. Without an app handle (headless tests)
                    // nothing can drain — the marker stays queued, which is
                    // the honest state, and the badge below still fires.
                    if let (Some(app), Some(reg)) = (self.app.lock_safe().clone(), self.arc()) {
                        reg.ensure_drainer(app, group.clone(), pty_id, None);
                    }
                    None
                }
                // A marker is already queued — the submit this decision
                // wanted is pending, so present it as an in-flight heal
                // (`None`) without counting it against the budget.
                Ok(false) => None,
                Err(e) => {
                    // rev-47 NB4: its OWN blocker, not `Exhausted` — no heal
                    // was attempted here, so the badge must not say one was.
                    self.audit(group, brand::AUDIT_ACTOR, "stranded-selfheal-skipped",
                        json!({ "to": agent_id, "reason": "queue-full", "error": e }));
                    Some(StrandedBlocker::QueueFull)
                }
            },
        };
        self.mark_stranded(group, agent_id, blocker);
        healed
    }

    /// Re-admit a lost fresh kickoff's brief through the delivery queue
    /// (#517) — the actuation half of `kickoff_recovery_action`, split out so
    /// the admission property ("the brief is re-delivered through the same
    /// front door, exactly once") is testable without an `AppHandle`.
    ///
    /// **Why the front door and not a write.** The kickoff was never outside
    /// #470's ordered admission; only its failure was. Re-admitting here
    /// means the re-delivery is ordered against anything else queued for the
    /// pane, inherits every paste guard (`deliver_now`'s human-input,
    /// question and stranded-text checkpoints), and is confirmed by the same
    /// three-state machinery (#451) as any other prompt — so a re-delivery
    /// that ALSO fails is loud rather than a second silent loss. A raw write
    /// from the monitor thread would race the drainer mid-paste and re-open
    /// the ordering hole #470 closed, exactly as #496 PR-C's own note says.
    ///
    /// **Idempotence, at this layer.** `queue::admit`'s byte-identical
    /// coalesce is consulted in the SAME critical section as the push
    /// (`AdmitOutcome::coalesced`), so a brief still waiting in the queue can
    /// never be queued a second time — and a coalesce is reported as NOT
    /// admitted, so it does not burn the caller's re-delivery budget for a
    /// send that never happened. This is the same "a declined admission must
    /// not burn the budget" rule `actuate_stranded` follows.
    ///
    /// Returns whether a re-delivery was really admitted.
    #[doc(hidden)] // pub for integration tests
    pub fn redeliver_lost_kickoff(
        &self,
        group: &GroupId,
        agent_id: &str,
        from: &str,
        pty_id: u32,
        text: &str,
    ) -> bool {
        let admitted = match self.enqueue_text(
            group, agent_id, from, text, pty_id, queue::EnqueueReason::KickoffRecovery,
        ) {
            Ok(a) => a,
            Err(e) => {
                // The pane's queue is at cap. Nothing was sent and nothing
                // may claim otherwise — the badge `actuate_stranded` already
                // raised stays up and says so.
                self.audit(group, brand::AUDIT_ACTOR, "kickoff-redelivery-skipped",
                    json!({ "to": agent_id, "reason": "queue-full", "error": e }));
                return false;
            }
        };
        if admitted.coalesced {
            self.audit(group, brand::AUDIT_ACTOR, "kickoff-redelivery-skipped",
                json!({ "to": agent_id, "reason": "already-queued", "id": admitted.id }));
            return false;
        }
        // Review F4: the re-delivery runs under the SAME boot wait the
        // original kickoff had — see `redelivery_treatment` for why the
        // other two flags are deliberately not copied with it, and why
        // `was_first` gates it exactly as `deliver_prompt`'s front door does.
        let treatment = redelivery_treatment(admitted.was_first)
            .map(|t| FreshFirstAttempt::from((admitted.id, t)));
        self.audit(group, brand::AUDIT_ACTOR, "kickoff-redelivered", json!({
            "to": agent_id, "id": admitted.id, "via": "queue-front-door",
            // The F4 fix, visible in the record: a re-delivery that skipped
            // the boot wait would be the original defect's ghost, so whether
            // it got one is a fact the audit states rather than implies.
            "boot_wait": treatment.as_ref().is_some_and(|t| t.wait_ready),
        }));
        // Best-effort nudge, the same shape `deliver_prompt`'s
        // behind-the-queue path and `actuate_stranded` both use;
        // `ensure_drainer` is idempotent. Without an app handle (headless
        // tests) nothing can drain — the entry stays queued, which is the
        // honest state, and the caller's badge still fires.
        if let (Some(app), Some(reg)) = (self.app.lock_safe().clone(), self.arc()) {
            reg.ensure_drainer(app, group.clone(), pty_id, treatment);
        }
        true
    }

    /// Raise (or update) a pane's stranded attention badge (#496 PR-C) and
    /// audit it. Keeps `since_ms` from an existing note so a badge that
    /// changes reason (heal in flight → blocked) still reports how long the
    /// pane has been stranded.
    #[doc(hidden)] // pub for integration tests
    pub fn mark_stranded(&self, group: &GroupId, agent_id: &str, blocker: Option<StrandedBlocker>) {
        let since_ms = {
            let mut m = self.attn_stranded.lock_safe();
            let since_ms = m.get(agent_id).map(|n| n.since_ms).unwrap_or_else(now_ms);
            m.insert(agent_id.to_string(), StrandedNote { blocker, since_ms });
            since_ms
        };
        self.audit_stranded_attention(group, agent_id, blocker, since_ms);
    }

    /// Re-word a chip that is **already up** (#825 M2), returning whether one
    /// was. Never inserts, which is the entire difference from
    /// [`Self::mark_stranded`] and the reason it exists.
    ///
    /// **An absent entry means "nothing to do", and specifically not "raise
    /// one".** Since #825 M1 a missing note can mean a human dismissed the
    /// chip, so a re-word that inserted would hand them back the warning they
    /// just took down — the live complaint, reopened by the mechanism meant to
    /// fix it. Every re-word runs on a reading taken moments earlier, so
    /// "moments earlier there was a chip here" is exactly the stale premise a
    /// dismissal invalidates, and the map's own `get_mut` is the only check
    /// that cannot race it: absent stays absent.
    ///
    /// `expected` guards the other half of the same gap — the chip is still up
    /// but now says something else, because another mechanism re-raised it
    /// between the read and this write. Re-wording *that* would overwrite a
    /// fresher diagnosis with a verdict about a chip that is gone. Passing the
    /// blocker the caller actually judged makes the update a compare-and-set.
    ///
    /// Audited as `stranded-attention`, identically to a re-word through
    /// `mark_stranded` (which is what the late monitor's live re-check has
    /// always been): the badge's history stays one grep, not two.
    #[doc(hidden)] // pub for integration tests
    pub fn reword_stranded(
        &self,
        group: &GroupId,
        agent_id: &str,
        expected: Option<StrandedBlocker>,
        to: Option<StrandedBlocker>,
    ) -> bool {
        let Some(since_ms) = ({
            let mut m = self.attn_stranded.lock_safe();
            match m.get_mut(agent_id) {
                Some(note) if note.blocker == expected => {
                    note.blocker = to;
                    Some(note.since_ms)
                }
                _ => None,
            }
        }) else {
            return false;
        };
        self.audit_stranded_attention(group, agent_id, to, since_ms);
        true
    }

    /// Apply one [`stranded_badge_release`] verdict to a pane's chip (#825 M2),
    /// returning whether the chip came down.
    ///
    /// The single WRITER, as that function is the single decider. Both
    /// observers — the late monitor's `KeepWaiting` arm and
    /// [`Self::stranded_janitor_pass`] — go through here, so the same verdict
    /// cannot come to mean two things depending on which of them reached it
    /// first. In particular a `Reword` is a re-word from both, never a raise:
    /// see [`Self::reword_stranded`] for why an absent entry has to stay
    /// absent.
    ///
    /// `judged` is the blocker the verdict was decided from, carried through so
    /// the write can check the chip is still that one.
    #[doc(hidden)] // pub for integration tests
    pub fn apply_stranded_verdict(
        &self,
        group: &GroupId,
        agent_id: &str,
        judged: Option<StrandedBlocker>,
        verdict: BadgeRelease,
    ) -> bool {
        match verdict {
            BadgeRelease::Clear(why) => {
                self.clear_stranded(group, agent_id, why);
                true
            }
            BadgeRelease::Reword(to) => {
                self.reword_stranded(group, agent_id, judged, Some(to));
                false
            }
            BadgeRelease::Keep => false,
        }
    }

    /// The one `stranded-attention` audit line, shared by every writer of the
    /// badge so a raise and a re-word cannot drift into two vocabularies for
    /// the same event (the reason `BoxReading::as_str` lives next to its enum).
    fn audit_stranded_attention(
        &self,
        group: &GroupId,
        agent_id: &str,
        blocker: Option<StrandedBlocker>,
        since_ms: u64,
    ) {
        self.audit(group, brand::AUDIT_ACTOR, "stranded-attention", json!({
            "to": agent_id,
            "blocker": blocker.map(|b| b.as_str()).unwrap_or("self-healing"),
            "stranded_ms": now_ms().saturating_sub(since_ms),
        }));
    }

    /// Drop a pane's stranded badge (#496 PR-C). Audited ONLY when a badge
    /// was actually up, so the log records state changes rather than every
    /// poll of a healthy pane.
    #[doc(hidden)] // pub for integration tests
    pub fn clear_stranded(&self, group: &GroupId, agent_id: &str, why: &str) {
        let had = self.attn_stranded.lock_safe().remove(agent_id);
        if let Some(note) = had {
            self.audit(group, brand::AUDIT_ACTOR, "stranded-cleared", json!({
                "to": agent_id, "why": why,
                "stranded_ms": now_ms().saturating_sub(note.since_ms),
            }));
        }
    }

    /// #946 Q4 / #1091 slice H — the latched-attention belt's sole writer.
    /// Called only from `deliver_now`'s `emit_held` closure, only for the
    /// ORCHESTRATOR's own pane (see `attn_question_held`'s doc for why the
    /// belt is orchestrator-only, unlike the Q4 CLI deny which also covers
    /// the liaison). No audit entry: the hold itself is already audited
    /// (`delivery-held-for-question`) at the call site that decided to hold;
    /// this only mirrors that decision into the attention set so
    /// `attention_tick` can surface it.
    /// The [`providerlimit`] provider whose refusal `agent_id`'s pane is
    /// showing, as of the last attention scan (#2811 S5b).
    ///
    /// The driver's read of the ONE pane-text classifier. `None` covers both
    /// "that pane is fine" and "the scan has not run" — see
    /// [`Self::attn_provider_limit`] for why those are deliberately the same
    /// answer, and `DriveFacts::provider_limited` for the fail direction.
    pub fn provider_limit_for_agent(&self, agent_id: &str) -> Option<String> {
        self.attn_provider_limit.lock_safe().get(agent_id).cloned()
    }

    /// Test seam: publish a provider limit for `agent_id` without running a
    /// real attention scan, which needs pty tails a fake runner has none of.
    /// Production writes this map ONLY from `attention_tick`.
    pub fn set_provider_limit_for_test(&self, agent_id: &str, provider: &str) {
        self.attn_provider_limit
            .lock_safe()
            .insert(agent_id.to_string(), provider.to_string());
    }

    /// Test seam: the pane recovered. Production has no such call — the scan
    /// rewrites the map WHOLE, so a pane that is no longer showing a refusal
    /// simply stops appearing in it. This exists so a test can reach the same
    /// state without running a scan.
    pub fn clear_provider_limit_for_test(&self, agent_id: &str) {
        self.attn_provider_limit.lock_safe().remove(agent_id);
    }

    pub fn latch_question_held(&self, agent_id: &str) {
        self.attn_question_held.lock_safe().insert(agent_id.to_string());
    }

    /// Releases [`Self::latch_question_held`]. Called unconditionally from
    /// `deliver_now`'s `emit_held_cleared` closure whenever ANY hold in that
    /// function clears (Typing/BoxOccupied included) — safe because removing
    /// an id the latch was never holding for is a no-op; see
    /// `attn_question_held`'s doc for why that can't race a different hold.
    pub fn unlatch_question_held(&self, agent_id: &str) {
        self.attn_question_held.lock_safe().remove(agent_id);
    }

    /// The human explicitly dismissed a pane's stuck-prompt chip (#825 M1).
    ///
    /// **Valid for every [`StrandedBlocker`]**, including
    /// [`StrandedBlocker::PauseSuppressed`] — whose loss is *historical* and so
    /// can never be released by any reading of the pane — because the release
    /// evidence here is not an inference about the pane at all. It is the one
    /// reader the badge exists for saying "seen", aimed at the chip itself, so
    /// loomux fabricates no claim by obeying it.
    ///
    /// **It takes the CHIP down and nothing else.** No hold is released, no
    /// queue entry is admitted, and no Enter is ever pressed — a dismissal
    /// changes exactly one thing: whether a warning is on screen. If the pane
    /// really is still wedged, everything that would have re-raised the badge
    /// still can, with the one stated exception below.
    ///
    /// **Deliberately NOT the focus ack** ([`Self::ack_attention`]). Focusing a
    /// pane is what the human does all day in order to type into it, so
    /// treating it as "seen" would take down chips nobody read — and for the
    /// `Unverifiable` / `Exhausted` classes the chip may be the only trace of a
    /// prompt still sitting unsubmitted in a box loomux could not read. This
    /// gesture is unambiguous; that one is not. (Nor is it an expiry: elapsed
    /// time clears the notice precisely on the unattended machine where nobody
    /// saw it.)
    ///
    /// **Audited as its own event, not only as a clear.** [`Self::clear_stranded`]
    /// records the reason but not the *class*, and for the classes that claim
    /// the box may still hold text the class IS the diagnosis. So a dismissal
    /// writes `stranded-dismissed` — actor `human`, carrying the blocker token
    /// and how long the chip stood — ahead of the clear. A dismissed badge is
    /// therefore fully reconstructible from the audit log; it is never a silent
    /// disappearance, which is the whole reason a human is allowed to take one
    /// down on their own say-so.
    ///
    /// **The one thing a dismissal does not get back.** For `QuestionStale` /
    /// `HumanInput` the badge is raised through a hold episode's `badged`
    /// one-shot ([`held_escalation`]), which has already fired — so within that
    /// same episode the chip will not re-raise. That is accepted for an
    /// explicit dismissal (the human just said they had seen it) rather than
    /// papered over by re-arming the one-shot, which would turn one gesture
    /// into a chip that comes straight back.
    ///
    /// Returns whether a badge was actually up. `false` — an unknown agent, or
    /// one with no chip — is a no-op that audits nothing: there is no state
    /// change to record.
    pub fn dismiss_stranded(&self, agent_id: &str) -> bool {
        // The group is loomux's own fact about the pane, so it is read from the
        // registry rather than taken from the caller: a command COULD accept a
        // group id and parse it (#904), but not accepting one at all is strictly
        // narrower. An id with no agent record cannot have a chip on
        // screen either — `attention_tick` builds its items from `agents` — so
        // this rejects only ids that could never have been clicked.
        let Some(group) = self.agent(agent_id).map(|a| a.group) else { return false };
        let Some(note) = self.stranded_note(agent_id) else { return false };
        self.audit(&group, "human", "stranded-dismissed", json!({
            "to": agent_id,
            "blocker": note.blocker.map(|b| b.as_str()).unwrap_or("self-healing"),
            "stranded_ms": now_ms().saturating_sub(note.since_ms),
        }));
        // Read-then-clear, the shape the late monitor's arms already use. A
        // clear that lands in between leaves a `stranded-dismissed` line whose
        // `stranded-cleared` partner names the other mechanism's reason — which
        // is the honest record of what happened, not a lost one.
        self.clear_stranded(&group, agent_id, "human-dismissed");
        true
    }

    /// #560: record one drainer observation against `pty_id`'s hold episode,
    /// returning when the (possibly just-opened) episode began, or `None` when
    /// no episode is open.
    ///
    /// The only place [`ends_hold_episode`]/[`opens_hold_episode`] are
    /// consulted, so the lifecycle rule — *what starts and what ends an
    /// episode* — is applied identically to every observation instead of being
    /// re-spelled per call site, which is how #532's clock and its one-shot
    /// came to disagree in the first place.
    ///
    /// **It is NOT the only writer of [`OrchRegistry::hold_episodes`], and an
    /// earlier version of this doc said it was** (#599 review, rev-10). Four
    /// functions write the map, at six points, every one of them deliberate —
    /// enumerated here so the safety argument invites the audit rather than
    /// ending it:
    ///
    /// | writer | writes | why not here |
    /// | --- | --- | --- |
    /// | `note_hold` | inserts (episode opens) | — |
    /// | `note_hold` | removes (episode ends) | — |
    /// | [`OrchRegistry::hold_escalation_step`] | sets `announced` | needs the audit-line decision it is making |
    /// | [`OrchRegistry::hold_escalation_step`] | sets `badged` | gated on `stranded_note`, which the lifecycle rule knows nothing about |
    /// | `commit_exit` | removes | must be ATOMIC with the emptiness check + `queue_draining` removal |
    /// | `drop_queue` | removes | the pane's queue is gone; no observation describes it |
    ///
    /// What IS true, and is the invariant worth relying on: the two fields that
    /// have to agree — `started_ms` and `badged` — are only ever created and
    /// destroyed together, because they live in one value that is inserted whole
    /// and removed whole. Every writer above either creates the record, destroys
    /// it, or sets a one-shot flag inside a record that already exists. None can
    /// reset the clock while leaving the one-shot armed, or vice versa, which is
    /// the exact divergence #560 exists to make unrepresentable.
    ///
    /// Ending an episode also drops the escalation badge it raised, if that
    /// badge is still ours: the blocker check is the same one the pre-#560
    /// `Clear` arm made, moved to the point where the pane has *proved* it can
    /// accept a delivery rather than merely read writable for one poll.
    /// Another mechanism's badge is never ours to drop (#532 rev-12 NB5).
    #[doc(hidden)] // pub for integration tests
    pub fn note_hold(
        &self,
        group: &GroupId,
        agent_id: &str,
        pty_id: u32,
        observation: HoldObservation,
        now_ms: u64,
    ) -> Option<u64> {
        if ends_hold_episode(observation) {
            let had = self.hold_episodes.lock_safe().remove(&pty_id);
            // Only clear the badge if this episode is the one that raised it.
            // A `Delivered` on a pane that was never escalated must not reach
            // in and drop some other mechanism's badge.
            //
            // #813: and only a DELIVERY may clear it here at all. `Retired`
            // ends the episode without the pane having accepted anything, so
            // it establishes nothing about whether the pane still needs a
            // human — the whole distinction `HoldObservation::Retired` exists
            // to keep out of the audit trail. The retire arm owns that
            // decision per reason (`StrandedRetireReason::resolves_the_pane`),
            // and clearing here as well would take a `TextGone` or
            // `NothingStranded` badge down behind that decision's back, which
            // is precisely the badge story those two variants exist to avoid.
            if observation == HoldObservation::Delivered
                && had.is_some_and(|e| e.badged)
                && self
                    .stranded_note(agent_id)
                    .is_some_and(|n| matches!(n.blocker, Some(StrandedBlocker::QuestionStale) | Some(StrandedBlocker::HumanInput)))
            {
                self.clear_stranded(group, agent_id, "delivery-hold-released");
            }
            return None;
        }
        let mut episodes = self.hold_episodes.lock_safe();
        match episodes.get(&pty_id) {
            Some(open) => Some(open.started_ms),
            None if opens_hold_episode(observation) => {
                episodes.insert(pty_id, HoldEpisode {
                    started_ms: now_ms,
                    announced: false,
                    badged: false,
                    notice_reported: false,
                });
                Some(now_ms)
            }
            // A writable poll on a pane with no open episode: nothing to open
            // (it is not held) and nothing to close.
            None => None,
        }
    }

    /// #560: when `pty_id`'s open hold episode began, if one is open.
    ///
    /// Read-only itself. It does NOT imply that every mutation goes through
    /// [`OrchRegistry::note_hold`] — an earlier version of this doc claimed
    /// that and it was false (#599 review, rev-10); see `note_hold`'s own doc
    /// for the enumeration of all four writers and why each is where it is.
    #[doc(hidden)] // pub for integration tests
    pub fn hold_episode_since(&self, pty_id: u32) -> Option<u64> {
        self.hold_episodes.lock_safe().get(&pty_id).map(|e| e.started_ms)
    }

    /// #560: has `pty_id`'s open hold episode already raised its escalation
    /// badge? The `already_badged` input [`held_escalation`] takes. Read-only
    /// seam.
    #[doc(hidden)] // pub for integration tests
    pub fn hold_episode_badged(&self, pty_id: u32) -> bool {
        self.hold_episodes.lock_safe().get(&pty_id).is_some_and(|e| e.badged)
    }

    /// #532/#560/#563: ONE held drainer poll's escalation step — open (or
    /// continue) the pane's hold episode, write the once-per-episode audit
    /// line, decide via [`held_escalation`], and raise the badge if it is due.
    ///
    /// **Why this is a registry method and not eight lines in the drainer**
    /// (the `flush_stranded_text` argument, one function over). `run_queue_
    /// drainer` needs a real `AppHandle`, which a headless integration test
    /// cannot build — so anything left inline there is unreachable by every
    /// test in this repo, and #560's first symptom is precisely a defect that
    /// lived in those inline lines while `held_escalation`'s own contract stayed
    /// green and pinned. Everything that needs no `AppHandle` lives here; the
    /// caller is left with the pane-header chip, which is genuinely drainer-
    /// local UI state.
    ///
    /// The returned [`HeldEscalation`] is `held_escalation`'s verdict verbatim,
    /// so the caller can still drive the chip off it. A `Clear` deliberately
    /// does NOT end the episode — see [`ends_hold_episode`].
    #[doc(hidden)] // pub for integration tests
    #[allow(clippy::too_many_arguments)]
    pub fn hold_escalation_step(
        &self,
        group: &GroupId,
        agent_id: &str,
        pty_id: u32,
        admission: WriteAdmission,
        depth: usize,
        now_ms: u64,
        bound_ms: u64,
        last_user_input_ms: Option<u64>,
        // #820: what the question gate matched on THIS poll, or `None` where
        // it matched nothing (including every poll blocked on the box rather
        // than a question). Threaded in for the same reason
        // `last_user_input_ms` is — this method has no `PtyManager`, the
        // caller does, and re-reading the pane here would annotate the
        // decision with a different instant's evidence.
        matched: Option<&QuestionWitnessed>,
    ) -> HeldEscalation {
        // #590 L2: threaded in rather than read here because this method has no
        // `PtyManager` — the caller does. It is the same class of parameter as
        // `depth`: one live pane reading the drainer already holds, passed so
        // the whole decision downstream of it stays reachable by a headless
        // test. What it feeds ([`undeliverable_cause`]) is pure and pinned
        // separately.
        if admission.go() {
            // Provisional recovery. The chip comes down (the caller's job) but
            // the episode stands until a delivery actually lands.
            self.note_hold(group, agent_id, pty_id, HoldObservation::WritablePoll, now_ms);
            return HeldEscalation::Clear;
        }
        let started_ms = self
            .note_hold(group, agent_id, pty_id, HoldObservation::HeldPoll, now_ms)
            .unwrap_or(now_ms);
        // One audit line per hold EPISODE, not per poll and not per reason
        // change — the record has to show that loomux held and said so, without
        // becoming an entry every two seconds for as long as the hold stands
        // (the same reasoning `mark_stranded`'s one-shot rests on). The CHIP
        // re-words when the blocking gate changes (#563 rev-10 finding 3); the
        // audit deliberately does not, because an alternating pane would then
        // flood the log with the flapping this line exists to summarise.
        let announce = {
            let mut episodes = self.hold_episodes.lock_safe();
            match episodes.get_mut(&pty_id) {
                Some(e) if !e.announced => {
                    e.announced = true;
                    true
                }
                _ => false,
            }
        };
        if announce {
            self.audit(group, brand::AUDIT_ACTOR, "delivery-held-in-queue", json!({
                "to": agent_id, "stage": "queue-poll",
                "blocked_on": admission.enqueue_reason().as_str(),
                "held_ms": now_ms.saturating_sub(started_ms),
                "depth": depth,
                // #820: the same `matched` shape every capped hold and every
                // abort has carried since #513(c)/F2 — signal class, the
                // bounded line it fired on, and what the composed screen said.
                // `null` here means the gate matched nothing, which for a
                // `blocked_on: "question"` row is itself the finding.
                "matched": witness_audit(matched),
            }));
        }
        let escalation = held_escalation(
            admission,
            started_ms,
            now_ms,
            bound_ms,
            self.hold_episode_badged(pty_id),
        );
        if let HeldEscalation::Badge(blocker) = escalation {
            // #532 rev-12 NB5: guard the RAISE, not just the clear.
            // `mark_stranded` overwrites unconditionally, so badging over
            // another mechanism's badge would make it ours — and our clear
            // would then remove it. Declining to overwrite costs nothing: that
            // badge is already telling the human this pane needs them, which is
            // all this escalation exists to achieve.
            if self.stranded_note(agent_id).is_none() {
                if let Some(e) = self.hold_episodes.lock_safe().get_mut(&pty_id) {
                    e.badged = true;
                }
                self.mark_stranded(group, agent_id, Some(blocker));
            }
        }
        // #590 L2. Evaluated on the BOUND, not on `escalation`, and outside the
        // `Badge` arm entirely — rev-128's blocking finding, plus the half its
        // remedy did not reach.
        //
        // The badge is the human's channel and this is the orchestrator agent's,
        // and the two arms of `held_escalation` they need are not the same one:
        //
        // - a RAISED badge sets `HoldEpisode::badged`, after which
        //   `held_escalation` returns `None` for the rest of the episode
        //   (`if already_badged`), so there is never a second `Badge` poll;
        // - a DECLINED raise (another mechanism owns the badge) leaves `badged`
        //   false, so `Badge` comes back every poll.
        //
        // Nested in that arm, this fires exactly once and only at the instant
        // the bound is crossed — which is the whole defect: an episode that
        // crosses the bound holding only work would never look again, and the
        // notice that arrives afterwards (the watch firing behind a held
        // follow-up — one step from #590's own incident) stays silent for as
        // long as the pane stays held. Keyed on the bound instead, every later
        // poll re-asks, and `HoldEpisode::notice_reported` — claimed by the poll
        // that REPORTS, never by one that merely looked — keeps it to one per
        // episode.
        if hold_bound_elapsed(started_ms, now_ms, bound_ms) {
            self.note_undeliverable_notice(
                group,
                agent_id,
                pty_id,
                admission,
                started_ms,
                bound_ms,
                last_user_input_ms,
            );
        }
        escalation
    }

    /// #590 L2: at the escalation bound, is one of loomux's OWN notices sitting
    /// in this pane's queue — and if so, say so on a channel that is not the
    /// blocked pane. Once per hold episode.
    ///
    /// **Why this is worth reporting when a plain stale hold is not.** Every
    /// other payload waiting on a busy pane is work, and a busy pane finishing
    /// its turn delivers it; the chip and the badge are the right channels and
    /// the human at the window is the right reader. A *notice* is the one
    /// payload whose non-delivery can be the reason the pane stays busy:
    /// #590's worker was blocked on a CI condition that
    /// `notify::watch_conflicting_notice` had already resolved, in a notice it
    /// could not be told. That is not a hold a human's attention fixes — it is
    /// a hold whose only readers are elsewhere.
    ///
    /// **The queue is re-read here rather than taken as a parameter**, matching
    /// `note_queue_capacity`'s rule for the same reason: every caller then
    /// states the same fact the same way, and the whole decision stays
    /// reachable by a headless test (`run_queue_drainer` needs an `AppHandle`,
    /// so anything left inline there is testable by nothing — #560's first
    /// symptom lived in exactly those lines).
    ///
    /// **Called on every poll past the bound, and the order of the three steps
    /// below is the fix for rev-128's blocking finding.** It reads the flag,
    /// then the queue, and claims the one-shot only when it is about to report:
    ///
    /// 1. `notice_reported` first, so the steady state after a report is one
    ///    flag read per poll and nothing else;
    /// 2. the queue snapshot, taken only while this episode is still unreported
    ///    — bounded work (one lock read plus a clone of at most
    ///    `queue::QUEUE_MAX_PER_PANE` entries) on a thread that is already
    ///    polling a pty every `queue::QUEUE_DRAIN_POLL`;
    /// 3. the claim, by the poll that REPORTS, never by one that merely looked.
    ///
    /// Claiming before the zero-notice gate — the original order — spent the
    /// episode's only report on a poll that had nothing to say, so an episode
    /// that crossed the bound holding only work never looked again and the
    /// notice that arrived afterwards was silent for as long as the pane stayed
    /// held. There was no tension to trade off here: the alternative the old
    /// comment weighed this against (a one-shot that re-arms on queue content
    /// and fires repeatedly for one hold) is not what this order produces —
    /// `notice_reported` is set once and never cleared inside an episode, so
    /// re-asking on each poll costs a re-read, never a second report.
    fn note_undeliverable_notice(
        &self,
        group: &GroupId,
        agent_id: &str,
        pty_id: u32,
        admission: WriteAdmission,
        started_ms: u64,
        bound_ms: u64,
        last_user_input_ms: Option<u64>,
    ) {
        // Step 1. Cheap, and the common case once a pane has been reported.
        // An episode that is gone entirely is treated as reported rather than
        // as a reason to report with no one-shot to hold it — the failure
        // direction that floods.
        let unreported = self
            .hold_episodes
            .lock_safe()
            .get(&pty_id)
            .is_some_and(|e| !e.notice_reported);
        if !unreported {
            return;
        }
        // Step 2. Resolved BEFORE the queue read, never after: `enqueue_text`
        // states the house rule for the two maps — agents first, then queues —
        // because every site that needs both takes them in that order and one
        // inversion is all a deadlock needs. `queue_snapshot` does release its
        // guard before returning, so this is discipline rather than a live bug,
        // which is exactly when it is cheap to keep.
        let target_is_orchestrator = self
            .agent(agent_id)
            .is_some_and(|a| a.role == Role::Orchestrator);
        let entries = self.queue_snapshot(pty_id);
        let notices = queue::queued_notice_count(&entries);
        if notices == 0 {
            // The gate, and what keeps this from being a second, noisier copy
            // of `HoldClass::QueueStaleEscalation`. Nothing has been spent: the
            // next poll asks again, which is what lets a notice joining an
            // already-held pane still be reported.
            return;
        }
        // Step 3. Claim, and re-check under the lock so the flag is read and
        // written in one critical section rather than across the queue read
        // above. One drainer per pane makes a race impossible today; the
        // re-check means this stays correct if that ever stops being true.
        let claimed = {
            let mut episodes = self.hold_episodes.lock_safe();
            match episodes.get_mut(&pty_id) {
                Some(e) if !e.notice_reported => {
                    e.notice_reported = true;
                    true
                }
                _ => false,
            }
        };
        if !claimed {
            return;
        }
        let cause =
            undeliverable_cause(admission.held_reason(), last_user_input_ms, started_ms);
        let minutes = bound_ms / 60_000;
        // The durable half, and the one a test can read: `notify_queue`'s own
        // delivery can be refused with no record at all when the orchestrator
        // pane is dead or unbound (#633), so the audit line is written FIRST
        // and unconditionally.
        self.audit(group, brand::AUDIT_ACTOR, "notice-undeliverable", json!({
            "to": agent_id,
            "pty": pty_id,
            "minutes": minutes,
            "notices": notices,
            "depth": entries.len(),
            "cause": cause.as_str(),
            "blocked_on": admission.enqueue_reason().as_str(),
        }));
        // Through `notify_queue`, which is what makes the orchestrator's OWN
        // stuck pane work: that branch parks the notice for #578's inbox
        // instead of typing it into the pane it is about. Nothing is enqueued
        // for the blocked pane on any path, so the notice cannot queue behind
        // the block it reports — and a parked notice about the orchestrator's
        // pane cannot re-trigger this either, since parking creates no queue
        // entry and the one-shot above is already spent.
        self.notify_queue(
            group,
            agent_id,
            target_is_orchestrator,
            &undeliverable_notice(agent_id, notices, entries.len(), minutes, cause),
        );
    }
}
