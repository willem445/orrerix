//! The pane queues' write side: enqueueing text, the delivered-text records
//! the notice mask reads, the drainer's lifecycle, dropping, popping and
//! notifying, persisting and recovering the queues, and the unconfirmed
//! deliveries, as an `impl OrchRegistry` block (#3498), plus the
//! `QueueSnapshotWriter` forward to `persist_queues`. The read side and the
//! delivery path are `delivery.rs`. The queue's design is #445 in
//! `docs/design/orchestration.md`; its locking is
//! `docs/design/pty-input-path.md`.

use super::*;

impl OrchRegistry {
    /// Offer a NEW text payload to `pty_id`'s queue (#445/#470) — the front
    /// door (EVERY delivery, `reason: Arrival`, #470) is the only real-code
    /// caller as of #470 (pre-#470, a direct delivery's own hold-cap abort
    /// also called this with `BoxOccupied`/`Question`; that call site no
    /// longer exists — every entry is admitted up front, before any
    /// attempt, so there is nothing left to enqueue ON an abort). Applies
    /// `queue::admit`'s policy (FIFO / cap-8-reject-newest / byte-identical
    /// coalesce), audits the outcome, and returns the synchronous
    /// `queue_full_error` on `RejectFull` so a front-door caller can
    /// propagate it to the ORIGINAL MCP caller. Never called for a
    /// `StrandedSubmit` marker — see `enqueue_stranded_front`. Still takes
    /// an explicit `reason` (rather than hardcoding `Arrival`) because
    /// integration tests use this directly to seed arbitrary queue states.
    ///
    /// **`was_first` (#470).** Whether `pty_id`'s queue was empty
    /// immediately before this admission — computed under the SAME lock
    /// acquisition as the push, so it is the one atomic fact that decides
    /// arrival order: the ONLY admission whose push observes an empty queue
    /// wins the right to kick off processing (`deliver_prompt`'s front
    /// door); every other admission, no matter how it got here, lands
    /// behind whatever's already there. This is what closes the 3+-contender
    /// ordering gap (see `docs/design/orchestration.md`'s Ordering
    /// subsection): there is no longer a separate "race a raw mutex for an
    /// empty queue" path for a later arrival to bypass.
    #[doc(hidden)] // pub for integration tests
    pub fn enqueue_text(
        &self,
        group: &GroupId,
        agent_id: &str,
        from: &str,
        text: &str,
        pty_id: u32,
        reason: queue::EnqueueReason,
    ) -> Result<AdmitOutcome, String> {
        // #620: every caller but the front door admits a payload that is a
        // plain prompt by the time it gets here — a recovery re-admission
        // (`readmit_recovered`, whose entry's original kind describes a pane
        // that no longer exists), #517's own bounded re-delivery
        // (`redeliver_lost_kickoff`, which must NOT re-arm the kickoff flags —
        // see `redelivery_treatment`), and the integration tests that seed
        // arbitrary queue states. Those callers say so by construction here
        // rather than each passing a kind they would all pass the same.
        self.enqueue_text_as(group, agent_id, from, text, pty_id, reason, Delivery::MidSession)
    }

    /// [`enqueue_text`](Self::enqueue_text) with the admitting delivery's KIND
    /// recorded on the entry (#620) — `deliver_prompt`'s front door is the
    /// only caller, because it is the only one that knows.
    ///
    /// The kind is stamped on BOTH of that function's admission paths, paused
    /// and not, so the entry's record of what it is does not depend on which
    /// branch happened to admit it. Only `flush_paused_queues` reads it back
    /// (see `paused_flush_treatment`); an unpaused admission threads the same
    /// facts to its drainer directly, on the stack, as it always has.
    ///
    /// A COALESCE keeps the surviving (older) entry's kind rather than
    /// overwriting it: the treatment belongs to the entry that will actually
    /// drain, and a byte-identical repeat adds no information about it — the
    /// same argument `queue::admit`'s coalesce rests on.
    #[doc(hidden)] // pub for integration tests
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_text_as(
        &self,
        group: &GroupId,
        agent_id: &str,
        from: &str,
        text: &str,
        pty_id: u32,
        reason: queue::EnqueueReason,
        kind: Delivery,
    ) -> Result<AdmitOutcome, String> {
        // #467, and it must be FIRST: recovery seeds `queue_seq` past every
        // id the previous process left on disk, so it has to run before this
        // call mints one. See `recover_persisted_queue`'s id-collision note.
        // A no-op (one `HashSet` probe) on every call after the first.
        self.recover_persisted_queue(group);
        // Resolved BEFORE the `queues` lock, never under it: this reads the
        // agents map, and every other site in this file that needs both
        // takes agents first (`deliver_prompt`'s own `self.agent(...)`), so
        // reversing it here would be the one inversion that deadlocks.
        let target = self.durable_target(agent_id);
        // #562: what the admission DID, carried out of the critical section
        // so every audit line, notice and `Err` below still runs with no
        // lock held — exactly as the pre-#562 `drop(queues)` in each arm
        // achieved, now by construction rather than by remembering to drop.
        enum Admitted {
            Rejected { depth: usize },
            Coalesced { id: u64, coalesced: u32, depth: usize },
            Queued { id: u64, depth: usize, was_first: bool },
        }
        let admitted = self.queues.mutate(group, self, |queues| {
            let q = queues.entry(pty_id).or_default();
            let was_first = q.is_empty();
            match queue::admit(q, text, reason) {
                queue::AdmitDecision::RejectFull => {
                    // Nothing mutated `queues` on this arm, so no snapshot
                    // is owed (see docs/design/orchestration.md's
                    // persistence table). The `entry().or_default()` above
                    // can INTERN an empty deque, which is a mutation of the
                    // map but not of anything the file carries:
                    // `group_queue_entries` iterates each pane's
                    // deliveries, so an empty deque contributes no entry.
                    (Admitted::Rejected { depth: q.len() }, queuestate::QueueDirty::nothing_persisted())
                }
                queue::AdmitDecision::Coalesce => {
                    // #445 rev-35 NB4: `id`/`coalesced` were missing from
                    // this audit line, contradicting `QueuedDelivery::id`'s
                    // own doc ("carried in every audit line ... -coalesced
                    // ... so one payload's whole history is
                    // reconstructible"). `admit` already guarantees a match
                    // exists (that's the definition of `Coalesce`), so
                    // `expect` here documents an invariant rather than
                    // masking a real failure with a fake default.
                    let (id, coalesced) = q
                        .iter_mut()
                        .rev()
                        .find(|e| e.payload.text() == Some(text))
                        .map(|e| {
                            e.coalesced += 1;
                            (e.id, e.coalesced)
                        })
                        .expect("admit() returned Coalesce, so a byte-identical entry must exist");
                    // A coalesce target, by definition, matched an entry
                    // ALREADY in the queue — `was_first` (computed before
                    // `admit` ran) can therefore never be true here;
                    // asserting it documents that invariant rather than
                    // trusting it silently.
                    debug_assert!(!was_first, "a coalesce match implies a pre-existing entry");
                    // A coalesce mutates the queue too (the surviving
                    // entry's `coalesced` counter), so the snapshot has to
                    // move with it or a recovery would under-report the
                    // flush header's de-duplication count.
                    (
                        Admitted::Coalesced { id, coalesced, depth: q.len() },
                        queuestate::QueueDirty::snapshot(),
                    )
                }
                queue::AdmitDecision::Admit => {
                    let id = self.queue_seq.fetch_add(1, Ordering::SeqCst) + 1;
                    q.push_back(queue::QueuedDelivery {
                        id, agent_id: agent_id.to_string(), from: from.to_string(),
                        payload: queue::QueuedPayload::Text(text.to_string()),
                        reason, enqueued_ms: now_ms(), coalesced: 0,
                        group: Some(group.clone()),
                        to_orchestrator: target.is_orchestrator,
                        session_id: target.session_id,
                        delivery_kind: kind,
                    });
                    (
                        Admitted::Queued { id, depth: q.len(), was_first },
                        queuestate::QueueDirty::snapshot(),
                    )
                }
            }
        });
        // Durability (#468) has already run by here, with the lock released
        // and BEFORE these audit lines, so a crash between the two leaves a
        // snapshot with no `delivery-queued` line rather than the reverse.
        // Both orders lose one record; only this one loses the *forensic*
        // record instead of the *payload*. That ordering is now
        // `QueueMap::mutate`'s, not this function's to get right.
        match admitted {
            Admitted::Rejected { depth } => {
                // #563: NAME WHAT DROPPED. The pre-#563 line recorded only
                // `{to, reason, depth}` — enough to know something was lost,
                // never enough to know what, and no id is minted for a
                // rejected entry so there is nothing else in the log to join
                // against. A sender can re-send a delivery it can identify;
                // it cannot re-send an anonymous one. `audit.jsonl` rotates,
                // so this line is the only record that will ever exist.
                self.audit(group, brand::AUDIT_ACTOR, "delivery-dropped", json!({
                    "to": agent_id, "reason": "queue-full-at-call", "depth": depth,
                    "from": from, "enqueue_reason": reason.as_str(),
                    "bytes": text.len(), "preview": queue::dropped_payload_preview(text),
                }));
                // The pane IS at capacity, and on an orchestrator target the
                // `Err` below is the ONLY other signal and goes to the
                // calling agent, never to the human.
                self.note_queue_capacity(group, pty_id, Some(agent_id));
                Err(queue::queue_full_error(agent_id, depth, reason.as_str()))
            }
            Admitted::Coalesced { id, coalesced, depth } => {
                self.audit(group, brand::AUDIT_ACTOR, "delivery-coalesced",
                    json!({ "to": agent_id, "id": id, "coalesced": coalesced, "depth": depth }));
                Ok(AdmitOutcome { id, was_first: false, coalesced: true })
            }
            Admitted::Queued { id, depth, was_first } => {
                self.audit(group, brand::AUDIT_ACTOR, "delivery-queued",
                    json!({ "to": agent_id, "id": id, "reason": reason.as_str(), "depth": depth }));
                // #563: the admission that takes a pane to `Approaching` is
                // the last moment a warning can still be a warning.
                self.note_queue_capacity(group, pty_id, Some(agent_id));
                Ok(AdmitOutcome { id, was_first, coalesced: false })
            }
        }
    }

    /// Undo an admission that turned out to have nowhere to go (#470): only
    /// reachable when `pty_id`'s queue had NO app handle available at all to
    /// ever process it. In real operation `self.app` is set once at startup
    /// and this never fires; it exists purely so a bare test registry keeps
    /// the exact "no app handle, nothing queued" contract the pre-#470
    /// direct path gave callers
    /// (`deliver_prompt_front_door_is_a_noop_when_the_queue_is_empty`) even
    /// though #470's front door now admits BEFORE it knows whether anything
    /// can ever drain the entry. Pops `id` off the front IF it's still
    /// there (the only place it could be — nothing else has had a chance to
    /// pop yet) and audits the withdrawal so it's never silently missing.
    ///
    /// #633 gave it the refusal's own `reason` (the two call sites both used to
    /// withdraw under `no-app-handle`, so one of them named a cause that had not
    /// fired) and the `{to, from, bytes, preview}` a refusal line needs to be
    /// actionable — the same fields #563 added to the queue-full refusal, for
    /// the same reason: a sender can re-send a delivery it can identify and
    /// cannot re-send an anonymous one. The payload itself stays recoverable by
    /// #579's verified pairing rather than inline, because by this point
    /// `deliver_prompt` HAS written the `prompt` line carrying the full text.
    pub(in crate::orchestration) fn withdraw_unprocessable(
        &self,
        group: &GroupId,
        agent_id: &str,
        from: &str,
        text: &str,
        pty_id: u32,
        id: u64,
        reason: RefusalReason,
    ) {
        let removed = self.queues.mutate(group, self, |queues| {
            let removed = match queues.get_mut(&pty_id) {
                Some(q) if q.front().is_some_and(|f| f.id == id) => {
                    q.pop_front();
                    true
                }
                _ => false,
            };
            // The pop is conditional on the id still being the front, so
            // the write is too: a rollback that found nothing to roll back
            // changed nothing the snapshot carries.
            let dirty = if removed {
                queuestate::QueueDirty::snapshot()
            } else {
                queuestate::QueueDirty::nothing_persisted()
            };
            (removed, dirty)
        });
        if removed {
            self.audit(group, brand::AUDIT_ACTOR, "delivery-dropped", json!({
                "id": id, "reason": reason.as_str(), "to": agent_id, "from": from,
                "bytes": text.len(), "preview": queue::dropped_payload_preview(text),
                "consequence": reason.consequence(),
            }));
        } else {
            // #633: the branch that used to write NOTHING. The pop is
            // conditional (the id may no longer be the front), and when it does
            // not fire the caller still returns `Err` to its sender while the
            // entry stays queued — a delivery reported as failed and a payload
            // still sitting in a pane's queue, with no line joining the two.
            //
            // A DIFFERENT action on purpose, and not `delivery-dropped`:
            // `queue::orphaned_queue_entries` CLOSES an id on that action, so
            // writing one here would tell the orphan derivation that a still-
            // queued entry had been resolved — turning a silent gap into a
            // false all-clear, which is worse. This action closes nothing, the
            // entry keeps being reported as an orphan by id (correctly: it is
            // still there), and `front_door_refusals` skips it for that same
            // reason — one loss, one row.
            self.audit(group, brand::AUDIT_ACTOR, "delivery-withdraw-missed", json!({
                "queued_id": id, "reason": reason.as_str(), "to": agent_id, "from": from,
                "pty": pty_id,
                "consequence": "the sender was told this delivery failed, but the entry was no \
                                longer at the front of the queue and is STILL QUEUED — it is \
                                reported as an orphan by id, not as a refusal",
            }));
        }
    }

    /// #633: audit a delivery `deliver_prompt_as` refused BEFORE it ever
    /// reached the queue — a dead target, or one with no terminal bound yet.
    ///
    /// Deliberately shaped like `enqueue_text`'s `queue-full-at-call` line so
    /// `front_door_refusals` reads one row shape, with two differences that are
    /// facts rather than omissions: no `id` (nothing was minted, so nothing for
    /// `queue::orphaned_queue_entries` to open or close) and no `depth`/
    /// `enqueue_reason` (admission was never attempted — a `0` there would be a
    /// measurement nobody took, reading as "the pane was empty").
    ///
    /// It carries `text` in full, unlike the queue-full line, because it is
    /// written BEFORE `deliver_prompt`'s own `prompt` line and so has nothing to
    /// pair with. See `front_door_refusals`'s doc for why the `prompt` write is
    /// not simply moved above these refusals instead.
    pub(in crate::orchestration) fn audit_delivery_refused(
        &self,
        group: &GroupId,
        agent_id: &str,
        from: &str,
        text: &str,
        reason: RefusalReason,
    ) {
        self.audit(group, brand::AUDIT_ACTOR, "delivery-dropped", json!({
            "to": agent_id, "reason": reason.as_str(), "from": from,
            "bytes": text.len(), "preview": queue::dropped_payload_preview(text),
            "text": text,
            "consequence": reason.consequence(),
        }));
    }

    /// #576: mark `text` as safe for the question mask to claim in `to_agent`'s
    /// pane — the OPT-IN, without which a delivered line widens nothing.
    ///
    /// **The promise a caller makes, and it is per FIELD (rev-163 B1).** Every
    /// span of this text was either composed by loomux or *called in* by an
    /// agent other than `to_agent`. Nothing weaker will do, and "the notice
    /// leads with the marker" is much weaker: loomux's framing routinely carries
    /// a field somebody chose — a `notify_when` note, an agent NAME the
    /// orchestrator picked at spawn, a task title, a PR title from GitHub.
    ///
    /// **"Called in by", not "authored by" — the promise is weaker than it
    /// sounds and the difference is a known residual (rev-163 B3).** What a
    /// caller can check is which agent made the tool call. Who *dictated the
    /// words* is not knowable here and arguably not knowable at all: an
    /// orchestrator telling a worker what to report is the orchestrator's
    /// ordinary channel, not an attack signature. So a recipient that can
    /// instruct another agent can put text of its own choosing into its own
    /// pane's record through that agent — proxy-authorship. See
    /// [`mask_loomux_notices_with_record`]'s residual note for the scenario,
    /// its scope, and why it is accepted rather than mechanised away.
    ///
    /// **The attack this exists to stop**, because it is not obvious and it was
    /// found in review rather than in design. `notify_when(kind, pr, note)`
    /// takes an agent-supplied `note` (up to `NOTICE_FIELD_CAP`), and
    /// `notify_tick` delivers the fired notice **to the registering agent's own
    /// pane**. `sanitize_gh_text` maps `[`/`]` to parens and strips control
    /// characters — it does not touch ordinary words, `?`, `(`, `)` or `❯`,
    /// which is everything a dialog row is made of. So one agent, with one tool
    /// call it makes itself, can put a line of its own choosing into its own
    /// pane's record; then print that line's head into its own pane; then let
    /// its CLI paint a permission dialog whose rows are exactly that line's
    /// remainder. The run reconstructs, the dialog rows are masked away, the
    /// gate reads clear and loomux presses Enter into a live question. That is
    /// the #420 harm from ONE party, deterministically — which is why entry to
    /// the record is an explicit promise a producer makes and not a property of
    /// the text.
    ///
    /// **Default-closed, and omission is the safe direction.** A producer that
    /// never calls this leaves its notices exactly as they were before #576's
    /// residual work: the marker rule alone, one row per marker, a wrapped
    /// notice latching until `QuestionStale` reports it. That is the cheap
    /// error, so forgetting to opt in costs a hold, never a release.
    ///
    /// Marking is phase one only — see [`DeliveredNotices`]. Nothing becomes
    /// claimable until [`OrchRegistry::record_delivered_text`] observes the
    /// same line actually going to the pane.
    #[doc(hidden)] // pub for integration tests
    pub fn mark_notice_maskable(&self, to_agent: &str, text: &str) {
        if loomux_authored_lines(text).is_empty() {
            return;
        }
        let Some(pty_id) = self.agents.lock_safe().get(to_agent).and_then(|a| a.pty_id) else {
            // No pane bound yet: nothing can be on its screen, so there is
            // nothing for a mask to claim. The delivery still happens; it just
            // never becomes claimable, which is the default.
            return;
        };
        let mut map = self.delivered_notices.lock_safe();
        let seq = Self::next_record_seq(&map);
        map.entry(pty_id).or_default().note_pending(text, seq);
        Self::evict_stalest_panes(&mut map);
    }

    /// #576: these bytes just went to `pty_id`. A line its producer marked
    /// ([`OrchRegistry::mark_notice_maskable`]) becomes claimable by the
    /// question gate's mask; anything else is ignored.
    ///
    /// **Called after the write, never before it.** A record of text that never
    /// reached the pane would let the mask claim rows nobody wrote — the
    /// fail-OPEN direction. Recording after `write_bytes` returns `Ok` keeps
    /// the record a statement about bytes that went out.
    #[doc(hidden)] // pub for integration tests
    pub fn record_delivered_text(&self, pty_id: u32, text: &str) {
        if loomux_authored_lines(text).is_empty() {
            return;
        }
        let mut map = self.delivered_notices.lock_safe();
        let seq = Self::next_record_seq(&map);
        // `get_mut`, never `entry(..).or_default()`: a pane with no record has
        // nothing marked, and creating one here would be a map entry that can
        // only ever hold unclaimable lines.
        if let Some(rec) = map.get_mut(&pty_id) {
            rec.note_written(text, seq);
        }
    }

    /// The next write stamp: max over panes + 1, so the pane just written is
    /// always the newest and can never be the eviction victim.
    fn next_record_seq(map: &HashMap<u32, DeliveredNotices>) -> u64 {
        map.values().map(|d| d.seq).max().unwrap_or(0) + 1
    }

    /// Evict the least-recently-written pane, so the map cannot outgrow the
    /// fleet actually being delivered to. Only a pane that has taken nothing
    /// since `DELIVERED_NOTICE_PANES` others did can be chosen, and losing its
    /// record costs it the wrap masking until its next delivery — fail-closed,
    /// like every other loss here.
    fn evict_stalest_panes(map: &mut HashMap<u32, DeliveredNotices>) {
        while map.len() > DELIVERED_NOTICE_PANES {
            let Some(&stalest) = map.iter().min_by_key(|(_, d)| d.seq).map(|(k, _)| k) else {
                break;
            };
            map.remove(&stalest);
        }
    }

    /// #576: what loomux knows it wrote into `pty_id`, as the mask's input.
    /// Empty for a pane loomux has never written a notice into — which is the
    /// record-free marker rule, i.e. today's behaviour.
    #[doc(hidden)] // pub for integration tests
    pub fn delivered_notice_lines(&self, pty_id: u32) -> Vec<String> {
        self.delivered_notices.lock_safe().get(&pty_id).map(|d| d.claimable()).unwrap_or_default()
    }

    /// #903: the CLI session id of whichever agent currently holds `pty_id`.
    ///
    /// Through `by_pty`, the reverse index this registry already maintains
    /// beside every `pty_id` write — not a fresh scan of `agents`, because a
    /// second way of answering "who holds this pane" is a second thing that can
    /// drift from the first.
    ///
    /// **This TAKES `by_pty` and then `agents`** — stated as what it acquires
    /// rather than as an internal ordering, because the ordering is not the fact
    /// a caller needs. An earlier version of this doc read "`by_pty` is taken
    /// and RELEASED before `agents`, and neither is held while reaching for
    /// `delivered_prompts`". Every word of that was true of this body and
    /// useless to its caller: `attention_tick` was holding `agents` when it
    /// called in, so the second acquisition here was a re-entrant one on a
    /// `parking_lot::Mutex`, and the tick parked forever. That is #1702, and it
    /// was live for four betas.
    ///
    /// So the rule a caller reads off this doc is the one a lock-order note
    /// could not give it: **never call this while holding a registry lock**, and
    /// prefer not calling it at all — a caller that already has an agent
    /// snapshot has the session in [`AgentEntry::session_id`] and should pass
    /// that to [`OrchRegistry::delivered_mask_lines`] instead.
    #[doc(hidden)] // pub for integration tests
    pub fn session_for_pty(&self, pty_id: u32) -> Option<String> {
        // Through the EXISTING reverse index, never a fresh scan of `agents`:
        // `by_pty` is already maintained beside every `pty_id` write, and a
        // second way of answering "who holds this pane" is a second thing that
        // can drift from the first.
        //
        // A STALE entry — a pane whose agent has since been replaced by a resume
        // of the same session — resolves to the same session id, which is the
        // answer this function wants anyway.
        let agent = self.by_pty.lock_safe().get(&pty_id).cloned()?;
        self.agents.lock_safe().get(&agent).and_then(|a| a.session_id.clone())
    }

    /// #903: these bytes just went to `pty_id` as a PROMPT. Record the
    /// loomux-originated lines against the pane's SESSION, so a later resume of
    /// that session can recognise them replayed on its own screen.
    ///
    /// **Called after the write, never before it**, for
    /// [`OrchRegistry::record_delivered_text`]'s reason: a record of text that
    /// never reached a pane would let the mask claim rows nobody wrote, which is
    /// the fail-OPEN direction.
    ///
    /// A pane with no session — one loomux never assigned or resolved an id for
    /// — records nothing. That is the same "nothing known" an untouched pane
    /// has, and it costs a hold, never a release.
    #[doc(hidden)] // pub for integration tests
    pub fn record_delivered_prompt(&self, pty_id: u32, text: &str, kind: Delivery) {
        // #903 B1: both terms, and the kind is checked FIRST so a refused kind
        // never pays for the line scan.
        if !prompt_record_admits_kind(kind) {
            return;
        }
        if delivered_prompt_lines(text).is_empty() {
            return;
        }
        let Some(session) = self.session_for_pty(pty_id) else {
            return;
        };
        let mut map = self.delivered_prompts.lock_safe();
        let seq = map.values().map(|d| d.seq).max().unwrap_or(0) + 1;
        map.entry(session).or_default().note_written(text, seq);
        while map.len() > DELIVERED_PROMPT_SESSIONS {
            let Some(stalest) = map.iter().min_by_key(|(_, d)| d.seq).map(|(k, _)| k.clone()) else {
                break;
            };
            map.remove(&stalest);
        }
    }

    /// #903: the prompt lines loomux has delivered into `session` — including
    /// ones delivered to a DIFFERENT pane of the same session, which is the
    /// whole reason this exists.
    ///
    /// **Takes the session id, not a pty** (#1702). Resolving a pty in here
    /// meant every caller silently paid `by_pty` + `agents` inside a function
    /// whose name promises a record read, and the caller that could least afford
    /// it — `attention_tick`, which was holding `agents` and already had the
    /// session in its own roster — is the one that deadlocked on it. The
    /// resolution now happens at the call site, where whether a lock is held is
    /// something a reader can see.
    ///
    /// `None` is the same "nothing known" an unresolvable pane always produced:
    /// the marker rule alone, which is the fail-CLOSED direction.
    #[doc(hidden)] // pub for integration tests
    pub fn delivered_prompt_record(&self, session: Option<&str>) -> Vec<String> {
        let Some(session) = session else {
            return Vec::new();
        };
        self.delivered_prompts.lock_safe().get(session).map(|d| d.claimable()).unwrap_or_default()
    }

    /// #903: everything the question gate's mask may claim in `pty_id` — the
    /// per-pane notice record (#576/#661, opt-in per producer) plus the
    /// per-session prompt record (#903, admitted by provenance).
    ///
    /// The two classes are unioned here and NOT distinguished downstream,
    /// because [`mask_loomux_notices_with_record`] applies one rule to both and
    /// that rule is the stricter of the two it replaces: reconstruct-to-end AND
    /// no dialog question row heading the block above the anchor. A single rule
    /// is what stops "which record did this line come from" becoming a thing the
    /// mask has to get right on every row.
    /// **`session` is supplied by the caller** (#1702) — the pane record is
    /// keyed by pty and the prompt record by session, and only the caller knows
    /// whether it already holds the latter. A caller with an agent snapshot
    /// passes [`AgentEntry::session_id`] and takes NO lock to get it; one with
    /// only a pty passes [`OrchRegistry::session_for_pty`] at its own call site.
    #[doc(hidden)] // pub for integration tests
    pub fn delivered_mask_lines(&self, pty_id: u32, session: Option<&str>) -> Vec<String> {
        let mut lines = self.delivered_notice_lines(pty_id);
        lines.extend(self.delivered_prompt_record(session));
        lines
    }

    /// Whether a drainer thread is currently registered for `pty_id` — i.e.
    /// whether something may be mid-`deliver_now` on this pane's queue front
    /// (#496 PR-C's admission gate; see `stranded_admission_gate`). Also the
    /// test seam for #445's bounded-lifecycle property (`ensure_drainer`
    /// never double-spawns; a drainer removes itself on exit) — rev-47 NB3
    /// folded the identical `queue_draining()` accessor into this one.
    ///
    /// NOTE for callers inside a `queues` critical section: `admit_stranded_
    /// selfheal` must NOT call this (it would be a second, non-atomic read);
    /// it reads `queue_draining` directly while already holding `queues`.
    #[doc(hidden)] // pub for integration tests
    pub fn drainer_active(&self, pty_id: u32) -> bool {
        self.queue_draining.is_registered(pty_id)
    }

    /// Register a fake drainer for `pty_id` so an integration test can drive
    /// the `drainer-active` admission gate against the REAL registry state
    /// the real check reads — a headless test has no `AppHandle`, so it
    /// cannot spawn an actual drainer to produce this state (same shape as
    /// `set_rotate_check_pause_for_test` / `register_fake_for_test`).
    ///
    /// Returns the minted generation (`None` if one was already registered) so
    /// a test can hand it back to [`Self::release_drainer_for_test`] and model
    /// a drainer's whole lifetime, not only its middle.
    #[doc(hidden)] // test-only seam
    pub fn register_drainer_for_test(&self, pty_id: u32) -> Option<u64> {
        // Through the same `claim` the real `ensure_drainer` uses (#497):
        // there is no raw insert to reach for, and a test seam that wrote
        // this map some other way would be a second way in — exactly what
        // the newtype exists to not have.
        self.queue_draining.claim(pty_id, || self.drainer_gen.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// Deregister the fake drainer `register_drainer_for_test` claimed — the
    /// `commit_exit` half of the same seam (#825 M3), so a test can assert what
    /// changes at the moment a drainer *exits* rather than only what is true
    /// while one runs.
    ///
    /// Takes the generation, because the newtype's ONE removal takes the
    /// generation (#497/#470 B1 round 2) and a test seam that reached past that
    /// would be the ungenerationed removal the type exists to make unwritable.
    #[doc(hidden)] // test-only seam
    pub fn release_drainer_for_test(&self, pty_id: u32, generation: u64) -> bool {
        self.queue_draining.release(pty_id, generation)
    }

    /// Spawn `run_queue_drainer` for `pty_id` unless one is already running
    /// — bounded thread lifecycle (#445): at most one drainer per pane,
    /// answering the same unbounded-thread-accumulation concern raised
    /// against #451's late monitors. `self: &Arc<Self>` (not `&self`) so it
    /// can hand the drainer thread its own owned registry handle.
    ///
    /// `fresh_first` (#470) carries a just-admitted FRESH delivery's own
    /// kickoff behavior through to the drainer's first pass IF this call is
    /// the one that actually wins the idempotent spawn below — every OTHER
    /// caller (including this same delivery's own front-door caller, on the
    /// rare NB2-style race where a `None`-passing caller's `ensure_drainer`
    /// call happens to win instead) passes `None` and gets the always-safe
    /// `false, false` on iteration 1. Purely theoretical for the reason
    /// `FreshFirstAttempt`'s doc gives: a fresh kickoff's pty has no other
    /// deliveries to race against yet.
    pub(in crate::orchestration) fn ensure_drainer(self: &Arc<Self>, app: AppHandle, group: GroupId, pty_id: u32, fresh_first: Option<FreshFirstAttempt>) {
        // #470 B1 review round 2: mint a fresh generation for this spawn —
        // see `queue_draining`'s and `DrainerGuard`'s docs for why a bare
        // membership check isn't enough. `contains_key` (not `insert`)
        // because insertion now needs the freshly-minted value, not `()`.
        let Some(generation) =
            self.queue_draining.claim(pty_id, || self.drainer_gen.fetch_add(1, Ordering::SeqCst) + 1)
        else {
            return; // already running for this pane
        };
        let reg = self.clone();
        std::thread::spawn(move || run_queue_drainer(reg, app, group, pty_id, fresh_first, generation));
    }

    /// #470 B1 — the ONLY way `run_queue_drainer` may decide it's done with
    /// `pty_id`: atomically, in ONE critical section on `queues`, checks
    /// whether exiting is safe and, if so, removes `pty_id` from
    /// `queue_draining` BEFORE releasing `queues`'s lock.
    ///
    /// **The race this closes.** A plain "peek the queue, see it's empty,
    /// `return` and let the `DrainerGuard`'s `Drop` deregister" — the
    /// pre-fix shape — leaves an OS-scheduling-width window between that
    /// peek and the guard actually running. A `deliver_prompt` admission
    /// landing in that window (`enqueue_text` observes the queue was empty
    /// → `was_first: true` → the sender is told `Ok`) calls `ensure_drainer`,
    /// which finds `pty_id` STILL registered in `queue_draining` and
    /// no-ops — believing a drainer will get to it. That drainer is
    /// already committed to exiting and never re-reads the queue. The
    /// delivery is stranded forever: not destroyed (still audited, still
    /// sitting in `queues`), but never delivered either, with the sender
    /// already told success — exactly the "queued means safe" contract
    /// #445/#451 exist to guarantee, broken by the very PR meant to
    /// strengthen it. Fusing the emptiness check and the `queue_draining`
    /// removal into ONE lock scope on `queues` closes it: whichever of "a
    /// push" or "this exit" the OS schedules first is fully visible to the
    /// other by the time it runs, because they contend for the same lock.
    /// See `unified_admission_property`'s `drainer_lifecycle` model
    /// (queue.rs) for the exhaustive proof.
    ///
    /// **Generation-checked, not unconditional (review round 2).** This
    /// removal races the SAME `DrainerGuard::drop` that runs when THIS
    /// call's own thread eventually returns — see that type's doc for why
    /// an unconditional second removal (the round-1 shape) could erase a
    /// SUCCESSOR drainer's live registration. `generation` is the token
    /// `ensure_drainer` minted for the CALLING thread's spawn; the removal
    /// here (and the guard's) only fires if the currently stored value is
    /// still exactly that token, so whichever of "this call" or "the
    /// guard's later drop" runs first performs the real removal and the
    /// other is inherently a no-op — never a race to avoid double-firing,
    /// because double-firing a generation-checked removal is harmless by
    /// construction.
    ///
    /// Also clears `pty_id` from `queue_still_notified` and (#532/#560) its
    /// `hold_episodes` record in the SAME
    /// generation-checked step (review round 2, N3) — folded in here
    /// rather than left as a separate post-return call at each exit site,
    /// which had the identical stale-clear shape (a committed-but-not-yet-
    /// returned drainer's cleanup running after a successor had already
    /// re-armed the flag) just for a purely cosmetic flag instead of a
    /// mutual-exclusion invariant.
    ///
    /// `force`: the pty-closed/agent-dead paths must exit AND discard
    /// whatever is queued regardless of content — always returns
    /// `Some(entries)` (possibly empty). The normal "nothing left to
    /// drain" exit (`force: false`) must NOT commit while the queue is
    /// non-empty at this exact instant — returns `None`, and the caller
    /// (`run_queue_drainer`) loops again to pick up whatever raced in,
    /// rather than exiting and relying on a NEW drainer to notice it.
    pub(in crate::orchestration) fn commit_exit(&self, group: &GroupId, pty_id: u32, generation: u64, force: bool) -> Option<Vec<queue::QueuedDelivery>> {
        self.queues.mutate(group, self, |queues| {
            let is_empty = queues.get(&pty_id).map(|q| q.is_empty()).unwrap_or(true);
            if !force && !is_empty {
                // Nothing was taken out, so nothing is owed the file.
                return (None, queuestate::QueueDirty::nothing_persisted());
            }
            let entries: Vec<queue::QueuedDelivery> =
                queues.remove(&pty_id).map(Vec::from).unwrap_or_default();
            // Still holding `queues`'s lock here — this is the atomic step.
            if self.queue_draining.release(pty_id, generation) {
                self.queue_still_notified.lock_safe().remove(&pty_id);
                // #532/#560: cleared with its sibling, for the same reason —
                // this pane's queue is done, so nothing is being held on it any
                // more. This is the "the queue emptied" end of a hold episode
                // ([`ends_hold_episode`]'s doc names it), and it lives HERE
                // rather than going through `note_hold` because it has to be
                // atomic with the emptiness check and the `queue_draining`
                // removal above. In-memory state, NOT queue state: it owes no
                // `persist_queues` call (#468), and clearing it inside the same
                // generation guard is what stops a successor drainer's
                // freshly-opened episode being stripped by a predecessor's exit.
                self.hold_episodes.lock_safe().remove(&pty_id);
                // #563: same lifecycle, same reasoning — this pane's queue is
                // done, so its remembered capacity edge is about a queue that
                // no longer exists. Left behind, it would make a successor
                // drainer's first reading look like a fall from `Full` and
                // emit a release for a pane that never recovered.
                self.queue_pressure.lock_safe().remove(&pty_id);
            }
            // #468, and `QueueMap::mutate` runs it OUTSIDE the critical
            // section above — persisting is file I/O and the atomicity this
            // function exists for is between `queues` and `queue_draining`,
            // not between either and the disk. Only a force-exit that
            // actually discarded something changes the snapshot; the normal
            // exit removes an already-empty deque, which the group-filtered
            // snapshot never contained anything for.
            let dirty = if entries.is_empty() {
                queuestate::QueueDirty::nothing_persisted()
            } else {
                queuestate::QueueDirty::snapshot()
            };
            (Some(entries), dirty)
        })
    }

    /// The audit+notify tail shared by `drop_queue` (a direct, standalone
    /// drop — used by tests and any future caller that already holds a
    /// batch of removed entries) and `commit_exit`'s force-exit callers in
    /// `run_queue_drainer`. Never touches `queues` or `queue_draining`
    /// itself — `entries` is always what a PRIOR removal already took out.
    pub(in crate::orchestration) fn announce_dropped(&self, group: &GroupId, entries: Vec<queue::QueuedDelivery>, reason: queue::DropReason) {
        if entries.is_empty() {
            return;
        }
        for e in &entries {
            self.audit(group, brand::AUDIT_ACTOR, "delivery-dropped",
                json!({ "to": e.agent_id, "id": e.id, "reason": reason.as_str() }));
        }
        // In practice every entry in one pane's queue shares the same
        // target — named from the last (most recent) entry.
        if let Some(last) = entries.last() {
            let target_is_orchestrator =
                self.agent(&last.agent_id).map(|a| a.role == Role::Orchestrator).unwrap_or(false);
            self.notify_queue(group, &last.agent_id, target_is_orchestrator,
                &queue::dropped_notice(&last.agent_id, entries.len(), reason));
        }
    }

    /// Drop every entry currently queued for `pty_id` (#445): the pane's
    /// agent died or its pty closed while entries were waiting. Audits ONE
    /// `delivery-dropped` per entry (so each payload's own id closes out —
    /// see `queue::orphaned_queue_entries`) plus a single coalesced notice
    /// naming the count. Today this case is silent; this makes it loud.
    /// Standalone (does not touch `queue_draining`) — `run_queue_drainer`'s
    /// own force-exit paths use `commit_exit` instead so the removal is
    /// atomic with deregistering (#470 B1); this is for callers (tests,
    /// direct MCP-adjacent drop paths) with no drainer lifecycle to keep
    /// in sync.
    #[doc(hidden)] // pub for integration tests
    pub fn drop_queue(&self, group: &GroupId, pty_id: u32, reason: queue::DropReason) {
        let entries: Vec<queue::QueuedDelivery> = self.queues.mutate(group, self, |queues| {
            let entries: Vec<queue::QueuedDelivery> =
                queues.remove(&pty_id).map(Vec::from).unwrap_or_default();
            // Removing a pane with nothing queued removes nothing the
            // group-filtered snapshot ever carried.
            let dirty = if entries.is_empty() {
                queuestate::QueueDirty::nothing_persisted()
            } else {
                queuestate::QueueDirty::snapshot()
            };
            (entries, dirty)
        });
        // #560: the queue is gone, so nothing is being held on this pane — the
        // same end-of-episode `commit_exit` performs for the drainer's own exits
        // (there it must be atomic with the emptiness check, which is why it is
        // written out there rather than shared with this line). Left behind, the
        // record would hand a future drainer on this pty a clock from a queue
        // that no longer exists — the mirror of the stale `queue_pressure` entry
        // #563 had to clear for exactly the same reason.
        self.hold_episodes.lock_safe().remove(&pty_id);
        // #563: the queue is now empty, so any pressure badge this pane was
        // carrying is no longer true. Before `announce_dropped`, so the drop
        // notice is the last word rather than being followed by a release.
        self.note_queue_capacity(group, pty_id, None);
        self.announce_dropped(group, entries, reason);
    }

    /// Best-effort notice for a queue event (#445) — same suppression
    /// discipline as every other delivery notice: never to an
    /// orchestrator-target pane (a notice about a delivery to the
    /// orchestrator is itself a delivery to the orchestrator — a loop) or a
    /// paused group, and every suppression leaves a `notice-suppressed`
    /// audit line so it's discoverable after the fact instead of silently
    /// vanishing (the routed #451 finding this issue also owns).
    ///
    /// **#578: suppressed as a delivery is no longer discarded as
    /// information.** The orchestrator-target branch parks the notice in
    /// [`OrchRegistry::orch_notice_inbox`] and the text goes into the audit
    /// line, so the same fact reaches the orchestrator on its next MCP tool
    /// result ([`HoldChannel::OrchestratorInbox`]) without anything ever
    /// being typed into the blocked pane. The suppression itself is
    /// unchanged — that is the point: the loop the early return prevents is
    /// real, and the fix is a channel that is not a delivery, not a relaxed
    /// gate.
    #[doc(hidden)] // pub for integration tests
    pub fn notify_queue(&self, group: &GroupId, agent_id: &str, target_is_orchestrator: bool, text: &str) {
        if target_is_orchestrator {
            self.orch_notice_inbox
                .lock_safe()
                .entry(group.clone())
                .or_default()
                .park(text);
            self.audit(group, brand::AUDIT_ACTOR, "notice-suppressed",
                json!({
                    "kind": "queue", "to": agent_id, "reason": "target-is-orchestrator",
                    // #578: the two fields that make this line a RECORD of the
                    // notice rather than a record that one existed — the relay
                    // is in memory, so after a restart this is all there is.
                    "parked": true,
                    "text": text.chars().take(NOTICE_AUDIT_TEXT_CAP).collect::<String>(),
                }));
            return;
        }
        if self.is_paused(group) {
            // **Reached only by a non-orchestrator target**, because the branch
            // above returns first, and that order is deliberate (review NB2).
            // An orchestrator-target notice raised while the group is paused —
            // `note_queue_capacity` on the orchestrator's own pane is the live
            // path — IS parked, and should be: a pause suppresses DELIVERIES,
            // and the relay is not one. A paused orchestrator can still call
            // tools, and its own pane's queue pressure is precisely what it
            // should learn about while everything else is held.
            //
            // What follows is therefore about a WORKER target during a pause.
            // Deliberately NOT parked (#578), and #615 sharpened the reason
            // rather than weakening it. Since option 2 (enqueue-while-paused)
            // a pause is a DELAY, not a discard: the deliveries behind it sit
            // in the pane's durable queue and are flushed in arrival order at
            // resume, with `flush_header_text` announcing them then. So a
            // queue notice suppressed here describes an event whose payload is
            // safe and which the resume flush reports IN TIME and accurately;
            // relaying it minutes later, out of a parked buffer, would say
            // "queued" about a delivery that has since landed.
            //
            // The one loss a pause on this build can still cause — a pane
            // already at `queue::QUEUE_MAX_PER_PANE` refusing admissions for as
            // long as the pause lasts — has its own channel in
            // `announce_pause_suppression` (and its badge fallback), so parking
            // here would be a second report of a fact that already has one.
            self.audit(group, brand::AUDIT_ACTOR, "notice-suppressed",
                json!({ "kind": "queue", "to": agent_id, "reason": "group-paused" }));
            return;
        }
        let _ = self.deliver_to_orchestrator(group, text, brand::AUDIT_ACTOR);
    }

    /// #578: drain `group`'s parked orchestrator-target queue notices as the
    /// text of an extra MCP content block, or `None` when there is nothing
    /// parked (the ordinary case).
    ///
    /// **Drains, and drains once.** The entry is removed under the lock, so
    /// two concurrent `tools/call`s cannot both relay the same notice and a
    /// relayed notice cannot repeat on the next call. Losing the relay if the
    /// caller then fails is acceptable and audited: `notice-relayed` is
    /// written here, and every constituent notice is already in the log as a
    /// `notice-suppressed` line carrying its own text.
    #[doc(hidden)] // pub for integration tests
    pub fn take_orchestrator_notices(&self, group: &GroupId) -> Option<String> {
        let inbox = self.orch_notice_inbox.lock_safe().remove(group)?;
        let text = orch_notice_relay_text(&inbox.notices, inbox.elided)?;
        self.audit(group, brand::AUDIT_ACTOR, "notice-relayed", json!({
            "kind": "queue", "count": inbox.notices.len(), "elided": inbox.elided,
        }));
        Some(text)
    }

    /// #563: re-read `pty_id`'s queue depth and, if its capacity state has
    /// CHANGED since the last reading, say so — loudly, and through a channel
    /// that survives an orchestrator target.
    ///
    /// **Why this exists.** Before #563 a queue filling up was observable at
    /// exactly one instant: the moment a payload was already being thrown away.
    /// There was no "approaching" state, so nothing could warn in advance; and
    /// the drop itself was announced only through `notify_queue`, which is
    /// suppressed whenever the target is the group's own orchestrator — the
    /// pane the incident was reported on. So a queue could fill completely and
    /// silently. Both halves are fixed here: a threshold below the cap, and an
    /// attention badge (no role suppression) alongside the notice.
    ///
    /// **Edge-triggered, and released on evidence.** Only a *transition* acts,
    /// so a pane sitting at 7/8 for an hour produces one audit line and one
    /// badge rather than one per arrival. The release condition is the depth
    /// genuinely coming back down (`CapacityState::Normal`), never elapsed time
    /// — a queue that is still full after ten minutes is more urgent, not less.
    ///
    /// **Never stomps another mechanism's badge**, matching
    /// `run_queue_drainer`'s escalation: if some other subsystem has already
    /// flagged this pane, that flag is telling the human the same thing this
    /// one would (go look at this pane) and it is not ours to overwrite. Our
    /// own badge IS upgraded, so `Approaching` → `Full` re-words rather than
    /// sticking at the softer sentence.
    ///
    /// Takes no lock across the notice/badge calls, and re-reads depth rather
    /// than accepting it as a parameter, so every caller — admission,
    /// rejection, dequeue, drop — states the same fact the same way.
    /// `hint` is the target a caller already knows (the admission paths do);
    /// every other caller passes `None` and the agent is resolved from the
    /// queue, then from the remembered pressure record, then from `by_pty`.
    #[doc(hidden)] // pub for integration tests
    pub fn note_queue_capacity(&self, group: &GroupId, pty_id: u32, hint: Option<&str>) {
        let (depth, front_agent) = {
            let queues = self.queues.read();
            let q = queues.get(&pty_id);
            (
                q.map(|q| q.len()).unwrap_or(0),
                q.and_then(|q| q.front()).map(|e| e.agent_id.clone()),
            )
        };
        let state = queue::capacity_state(depth);
        // Resolution order, most-authoritative first. `remembered` is what
        // makes the release transition work at all: by then the queue may be
        // empty, so nothing else can name whose badge is coming down.
        let (prev, remembered) = {
            let m = self.queue_pressure.lock_safe();
            match m.get(&pty_id) {
                Some((s, who)) => (*s, Some(who.clone())),
                None => (queue::CapacityState::Normal, None),
            }
        };
        let agent_id = hint
            .map(str::to_string)
            .or(front_agent)
            .or(remembered)
            .or_else(|| self.by_pty.lock_safe().get(&pty_id).cloned());
        {
            let mut m = self.queue_pressure.lock_safe();
            match (&agent_id, state) {
                // Nothing to remember once the pane is back to normal.
                (_, queue::CapacityState::Normal) => {
                    m.remove(&pty_id);
                }
                (Some(who), _) => {
                    m.insert(pty_id, (state, who.clone()));
                }
                // Under pressure with nobody to attribute it to (teardown
                // ordering, or a bare registry with no agent at all). Not
                // recorded, so a later call with a resolvable target still
                // sees the transition rather than having it swallowed here.
                (None, _) => {}
            }
        }
        if state == prev {
            return;
        }
        let Some(agent_id) = agent_id else {
            return;
        };
        self.audit(group, brand::AUDIT_ACTOR, "delivery-queue-pressure", json!({
            "to": agent_id, "pty": pty_id, "depth": depth, "cap": queue::QUEUE_MAX_PER_PANE,
            "state": state.as_str(), "was": prev.as_str(),
        }));
        let existing = self.stranded_note(&agent_id).and_then(|n| n.blocker);
        // Ours to upgrade and to clear: exactly the two badges THIS function
        // raises. `QueueFull` is deliberately absent — `actuate_stranded`
        // raises it about a refused re-send, which a depth falling back does
        // not resolve, so clearing it here would drop another mechanism's
        // still-true badge.
        let ours = matches!(
            existing,
            Some(StrandedBlocker::QueueNearFull) | Some(StrandedBlocker::QueueAtCapacity)
        );
        if state == queue::CapacityState::Normal {
            if ours {
                self.clear_stranded(group, &agent_id, "queue-pressure-released");
            }
        } else {
            let (blocker, text) = if state == queue::CapacityState::Full {
                (
                    // rev-10 finding 1: NOT `QueueFull` — that one's wording
                    // asserts a refused re-send and stranded text in the box,
                    // neither of which a depth reading establishes.
                    StrandedBlocker::QueueAtCapacity,
                    queue::at_capacity_notice(&agent_id, queue::QUEUE_MAX_PER_PANE),
                )
            } else {
                (
                    StrandedBlocker::QueueNearFull,
                    queue::pressure_notice(&agent_id, depth, queue::QUEUE_MAX_PER_PANE),
                )
            };
            if existing.is_none() || ours {
                self.mark_stranded(group, &agent_id, Some(blocker));
            }
            let target_is_orchestrator =
                self.agent(&agent_id).map(|a| a.role == Role::Orchestrator).unwrap_or(false);
            // Suppressed on an orchestrator target (and audited as such) — which is
            // exactly why the badge above is raised first and unconditionally.
            // Terminates: the only notice this can generate is one delivery to the
            // orchestrator's own pane, and a pressure reading on THAT pane
            // suppresses here rather than sending a third.
            self.notify_queue(group, &agent_id, target_is_orchestrator, &text);
        }
        // #658: this ONE transition — the pane was at its cap and its depth has
        // now come back down — is the first moment loomux can tell the pane
        // what it refused while it was full. Everything else on that path
        // already existed and is unchanged: the refusals were audited (#563 /
        // #633), the senders were told synchronously, and #578's rider told the
        // orchestrator its queue was FULL. What none of them did was name WHO
        // was refused to the pane that refused them, which left a mid-session
        // refusal reachable only by an orchestrator that happened to poll
        // `queue_orphans` (#658's live instance).
        //
        // **Last in this function, deliberately.** The roster is delivered, so
        // it re-enters here through the enqueue — and everything above has
        // already committed this transition's pressure record, badge and
        // notice, so the nested call sees the state THIS call settled on rather
        // than racing it. The recursion terminates by construction: a delivery
        // can only push depth UP, and the edge below is the one edge an
        // increase cannot produce.
        if prev == queue::CapacityState::Full && state != queue::CapacityState::Full {
            self.announce_refusal_roster(group, &agent_id);
        }
    }

    /// #658: tell `agent_id`'s pane what it refused while its queue was full,
    /// once per drain, in one bounded coalesced line.
    ///
    /// **The channel is chosen the way #578 chose it.** An orchestrator's own
    /// pane cannot be told by a delivery — a prompt about that pane's blocked
    /// deliveries queues behind the very block it reports — so the roster is
    /// parked in [`OrchNoticeInbox`] and rides back on the orchestrator's next
    /// MCP tool result. Every other pane is told by an ordinary delivery. Both
    /// carry the identical string, so the recipient reads the same words
    /// whichever way it arrived.
    ///
    /// **It obeys the cap it reports on.** The delivery goes in under
    /// [`queue::EnqueueReason::RefusalRoster`], whose `cap_headroom` is zero —
    /// unlike the pause-loss notice, this one fires on the edge where depth just
    /// came DOWN, so it has room by construction and needs no exemption. If a
    /// concurrent arrival takes that slot anyway the roster is refused like
    /// anything else, the audit line says `delivered: false`, the watermark does
    /// NOT advance, and the same refusals are re-derived at the next drain.
    ///
    /// **Not marked maskable, and that is a decision (#661).**
    /// `mark_notice_maskable` is a per-field producer promise that every span of
    /// a notice was composed by loomux or called in by an agent OTHER than the
    /// recipient. This line fails that on its face: its previews are payload
    /// text authored by whoever sent the refused delivery, echoed back into the
    /// recipient's own pane. Claiming them would let a sender place chosen text
    /// into another pane's masked record — the injection surface the
    /// default-closed rule exists to keep shut — so the roster keeps the
    /// default, exactly as the queue notices next to it do
    /// (`deliver_relayed_to_root`'s why-comment). The cost of not
    /// opting in is a gate that holds slightly too long, never a release.
    /// Distinct, despite the shared vocabulary, from the single-line rule
    /// [`OrchNoticeInbox::park`] asserts: that is #576's pane-tail masking of
    /// loomux's OWN framing, which this line satisfies by leading with
    /// [`NOTICE_MARKER`] and never wrapping to a second line.
    ///
    /// Reads the audit window rather than any in-memory tally — see
    /// [`refusal_roster`] for why the log is the record. That read happens on
    /// this edge only, never per delivery.
    fn announce_refusal_roster(&self, group: &GroupId, agent_id: &str) {
        let (entries, window_truncated) = self.audit_log_windowed(group);
        let roster = refusal_roster(&entries, agent_id, window_truncated);
        // Nothing was refused since the last roster — the ordinary case, and
        // the one that must add nothing to a pane and no line to the log. Same
        // early return, for the same reason, as `announce_pause_suppression`'s.
        let Some(text) = refusal_roster_notice(&roster) else { return };
        let target_is_orchestrator =
            self.agent(agent_id).map(|a| a.role == Role::Orchestrator).unwrap_or(false);
        let outcome = if target_is_orchestrator {
            self.orch_notice_inbox
                .lock_safe()
                .entry(group.clone())
                .or_default()
                .park(&text);
            Ok(())
        } else {
            self.deliver_prompt_as(
                agent_id, &text, brand::AUDIT_ACTOR, Delivery::MidSession,
                queue::EnqueueReason::RefusalRoster,
            )
        };
        self.audit(group, brand::AUDIT_ACTOR, REFUSAL_ROSTER_ACTION, json!({
            "to": agent_id,
            "count": roster.items.len(),
            "total": roster.total,
            "omitted": roster.omitted,
            "resent": roster.items.iter().filter(|i| i.resent).count(),
            // The watermark itself (#658). Read back by the NEXT roster, and
            // only off a line that also says `delivered: true`.
            "through_ms": roster.through_ms,
            "at_through": roster.at_through,
            "window_truncated": roster.window_truncated,
            "channel": if target_is_orchestrator { "orchestrator-inbox" } else { "pane-delivery" },
            "delivered": outcome.is_ok(),
            "error": outcome.as_ref().err(),
            // #578's rule: a relay held in memory is one a restart loses, so
            // the line carries the text it relayed rather than a record that
            // one existed. Capped like every other notice the log quotes.
            "text": text.chars().take(NOTICE_AUDIT_TEXT_CAP).collect::<String>(),
        }));
    }

    /// Pop `id` off the FRONT of `pty_id`'s queue if it's still there (the
    /// normal case — see `run_queue_drainer`'s single-consumer doc for why
    /// it always is) and audit `delivery-dequeued`. Called by the drainer
    /// once an entry's delivery attempt reaches `DeliverOutcome::Done`.
    #[doc(hidden)] // pub for integration tests
    pub fn pop_front_dequeued(&self, group: &GroupId, pty_id: u32, id: u64, enqueued_ms: u64) {
        self.queues.mutate(group, self, |queues| {
            if let Some(q) = queues.get_mut(&pty_id) {
                if q.front().is_some_and(|f| f.id == id) {
                    q.pop_front();
                }
            }
            // #468: the delivered entry has to leave the snapshot before
            // anything else, or a restart in the next instant replays
            // something that already landed in the pane — the one
            // double-delivery this design can actually produce. It is still
            // a window, not a guarantee: a crash between the pop and this
            // write re-delivers. That residual is bounded by the queue's
            // existing byte-identical coalesce on re-admission and stated
            // plainly in the design note rather than claimed away.
            //
            // Unconditional, matching the pre-#562 code exactly: the
            // mismatched-front case (nothing popped) is the one
            // `run_queue_drainer`'s single-consumer doc calls impossible,
            // and writing a redundant snapshot is the cheap direction to be
            // wrong in.
            ((), queuestate::QueueDirty::snapshot())
        });
        self.audit(group, brand::AUDIT_ACTOR, "delivery-dequeued",
            json!({ "id": id, "queued_ms": now_ms().saturating_sub(enqueued_ms) }));
        // #563: the depth just came down — the evidence the pressure badge
        // releases on. Release is here, on a real reading, and nowhere on a
        // timer.
        self.note_queue_capacity(group, pty_id, None);
    }

    /// Close out EVERY constituent of one coalesced flush (#533-A) — the
    /// confirmation-transfer half.
    ///
    /// One combined paste means one submit, one `submit_sent_ms`, and
    /// therefore ONE #451 three-state outcome in `last_delivery` — so that
    /// outcome is the outcome of all `n` constituents, and they must all
    /// close out together or the queue would replay text the pane already
    /// received. Each is popped from the front in batch order (the drainer
    /// is the single consumer, so each is the front in turn) and audited
    /// with the batch it was submitted in, so one payload's history stays
    /// reconstructible from `audit.jsonl` alone — `combined_ids` is what
    /// tells a later reader that three `delivery-dequeued` lines at the same
    /// millisecond are one paste, not three.
    ///
    /// Called ONLY on `DeliverOutcome::Done`. A batch that aborted
    /// pre-paste closes out nothing: every constituent stays exactly where
    /// it was, in order, for the next attempt.
    #[doc(hidden)] // pub for integration tests
    pub fn pop_batch_dequeued(&self, group: &GroupId, pty_id: u32, batch: &[(u64, u64)]) {
        if batch.len() == 1 {
            // Unchanged from pre-#533 for the uncontended case: one entry,
            // one plain `delivery-dequeued` line with no batch fields.
            let (id, enqueued_ms) = batch[0];
            self.pop_front_dequeued(group, pty_id, id, enqueued_ms);
            return;
        }
        let ids: Vec<u64> = batch.iter().map(|(id, _)| *id).collect();
        for (id, enqueued_ms) in batch {
            self.queues.mutate(group, self, |queues| {
                if let Some(q) = queues.get_mut(&pty_id) {
                    if q.front().is_some_and(|f| f.id == *id) {
                        q.pop_front();
                    }
                }
                // #468, and NOT inherited from the single-entry branch
                // above: that one delegates to `pop_front_dequeued`, which
                // persists; this one pops directly, so it owes its own
                // write. Per entry rather than once per batch, matching
                // `pop_front_dequeued` exactly — it is what keeps the
                // design note's residual "one entry wide" instead of "one
                // flush batch wide", and this loop has already paid for N
                // audit appends, so N small snapshot writes are noise
                // beside the paste that preceded them.
                //
                // #562: this is the site whose missing write was invisible
                // for a whole PR cycle. Under `QueueMap::mutate` the write
                // is no longer something this branch could fail to
                // inherit — reaching the `&mut` is what schedules it.
                ((), queuestate::QueueDirty::snapshot())
            });
            self.audit(group, brand::AUDIT_ACTOR, "delivery-dequeued", json!({
                "id": id,
                "queued_ms": now_ms().saturating_sub(*enqueued_ms),
                "combined_ids": ids,
                "combined": ids.len(),
            }));
        }
        // #563: once, after the whole batch — a batch pops several entries at
        // one instant, and reporting a capacity transition per pop would
        // narrate a fall the pane never actually sat in.
        self.note_queue_capacity(group, pty_id, None);
    }

    /// Remove constituents that `queue::plan_flush` ruled superseded
    /// (#533-A) — by id, anywhere in the queue, not just the front — and
    /// MOVE each one's coalesce count onto the entry that supersedes it.
    ///
    /// **The transfer (rev-13 F4).** `admit`'s admission-time coalesce
    /// bumps the survivor's `coalesced` counter, which is what the flush
    /// header reports per constituent. A drain-time drop that only removed
    /// the duplicate would under-report for precisely the case this
    /// re-check exists to catch. The survivor absorbs the duplicate itself
    /// (`+1`) plus whatever that duplicate had already absorbed, so the
    /// total is conserved no matter which entry a repeat happened to land
    /// on.
    ///
    /// **Audited, deliberately WITHOUT a `notify_queue` notice** — unlike
    /// `announce_dropped` and the queue-full case in `AbortedPreEnter`,
    /// which do both. Those two lose a payload: nobody receives that text.
    /// This one does not — the identical payload still delivers, via the
    /// surviving earlier entry, in its original queue position. There is no
    /// loss to announce, and announcing one anyway would spend exactly the
    /// orchestrator turn this whole PR exists to save. The `audit.jsonl`
    /// line is the record; `superseded_by` and `folded_coalesced` on it
    /// make the transfer reconstructible after the fact.
    #[doc(hidden)] // pub for integration tests
    pub fn drop_superseded(&self, group: &GroupId, pty_id: u32, superseded: &[queue::Superseded]) {
        if superseded.is_empty() {
            return;
        }
        let removed: Vec<(queue::QueuedDelivery, u64)> = self.queues.mutate(group, self, |queues| {
            let removed: Vec<(queue::QueuedDelivery, u64)> = match queues.get_mut(&pty_id) {
                Some(q) => {
                    let (gone, kept): (Vec<_>, Vec<_>) =
                        q.drain(..).partition(|e| superseded.iter().any(|s| s.id == e.id));
                    q.extend(kept);
                    // Transfer under the SAME lock as the removal: a header
                    // rendered from a queue where the duplicate is gone but
                    // its count hasn't landed yet would under-report just as
                    // badly as never transferring it.
                    let mut out = Vec::with_capacity(gone.len());
                    for e in gone {
                        let by = superseded
                            .iter()
                            .find(|s| s.id == e.id)
                            .map(|s| s.by)
                            .expect("partition matched on this exact list");
                        if let Some(survivor) = q.iter_mut().find(|k| k.id == by) {
                            survivor.coalesced = survivor.coalesced.saturating_add(1 + e.coalesced);
                        }
                        out.push((e, by));
                    }
                    out
                }
                None => Vec::new(),
            };
            // #468: this removes entries from the live queue AND moves
            // coalesce counts onto their survivors, so the snapshot has to
            // move with it — otherwise a restart resurrects a delivery that
            // was deliberately superseded, and under-reports the survivor's
            // fold count. One write for the whole set, not per entry:
            // supersession removes byte-identical duplicates as a group,
            // and a crash before this lands resurrects duplicates, which is
            // exactly what the queue's own byte-identical coalesce absorbs
            // on re-admission.
            //
            // #562: the other site whose missing write shipped silently.
            let dirty = if removed.is_empty() {
                queuestate::QueueDirty::nothing_persisted()
            } else {
                queuestate::QueueDirty::snapshot()
            };
            (removed, dirty)
        });
        for (e, by) in removed {
            self.audit(group, brand::AUDIT_ACTOR, "delivery-dropped", json!({
                "to": e.agent_id,
                "id": e.id,
                "reason": "superseded",
                "superseded_by": by,
                "folded_coalesced": 1 + e.coalesced,
            }));
        }
        // #563: supersession is the other way a depth falls, and a pane whose
        // pressure was relieved by dedup rather than by delivery must release
        // its badge just the same.
        self.note_queue_capacity(group, pty_id, None);
    }

    /// Test-only: bind `agent_id` to `pty_id` directly, bypassing the real
    /// spawn/bind handshake (which needs a live Tauri window). #445's
    /// front-door tests need SOME agent with a `pty_id` to key a queue by,
    /// without the app-handle round trip every other pty-bound path needs.
    ///
    /// #533-B: also registers the REVERSE mapping (`by_pty`), which the real
    /// bind writes and which `on_pty_exit` looks the agent up through — so
    /// the exit path is drivable in a test at all, instead of returning at
    /// its first line. Additive: every pre-#533 caller only ever read
    /// `pty_id`, and `mark_dead` cleans this entry up exactly as it does for
    /// a real bind.
    #[doc(hidden)] // pub for integration tests
    pub fn set_pty_for_test(&self, agent_id: &str, pty_id: u32) {
        if let Some(a) = self.agents.lock_safe().get_mut(agent_id) {
            a.pty_id = Some(pty_id);
        }
        self.by_pty.lock_safe().insert(pty_id, agent_id.to_string());
    }
    /// Test-only: record an outcome for the last delivery to `pty_id`, in the
    /// two shapes `deliver_now` writes (#2089).
    ///
    /// The reuse-readiness predicate reads this map, and a headless test cannot
    /// fill it the production way: `deliver_now` needs a live pty and an
    /// `AppHandle` to run its confirm window at all — the same obstacle
    /// `reviewdrive.rs`'s `make_delivery_land` documents from the other side.
    /// `confirmed: false` goes through production's own writer
    /// ([`record_inflight_delivery`]) rather than a second insert here, so the
    /// unconfirmed shape cannot drift; `true` is the one shape no `pub` writer
    /// exists for, because `DeliveryOutcome` is deliberately private
    /// (`DeliveryConfirmation`'s doc).
    #[doc(hidden)] // pub for integration tests
    pub fn set_last_delivery_for_test(&self, pty_id: u32, confirmed: bool) {
        if !confirmed {
            record_inflight_delivery(
                &self.last_delivery,
                pty_id,
                now_ms(),
                brand::AUDIT_ACTOR.to_string(),
                None,
            );
            return;
        }
        self.last_delivery.lock_safe().insert(pty_id, DeliveryOutcome {
            confirmed: true,
            submit_sent_ms: now_ms(),
            from: brand::AUDIT_ACTOR.to_string(),
            // Mirrors the confirmed branch of `deliver_now`'s own write: #813,
            // our text went in, so there is nothing stranded to look for.
            stranded_text: None,
        });
    }

    /// Test-only companion to [`OrchRegistry::set_pty_for_test`]: give an agent
    /// the CLI session id a real spawn would have assigned it, so the
    /// session-keyed prompt record (#903) is reachable from a headless test.
    #[doc(hidden)] // pub for integration tests
    pub fn set_session_for_test(&self, agent_id: &str, session_id: &str) {
        if let Some(a) = self.agents.lock_safe().get_mut(agent_id) {
            a.session_id = Some(session_id.to_string());
        }
    }

    /// Write `group`'s live queues to disk (#468). Called after EVERY
    /// mutation of `queues` that changes what is pending, always with no
    /// lock held — see `queue_persist`'s doc for the lock order and for why
    /// holding the persist lock across read-then-write is what stops a
    /// stale snapshot landing after a fresh one.
    ///
    /// **#562: "called after EVERY mutation" is no longer a sentence asking
    /// to be believed.** This is what `QueueMap::mutate` runs, through the
    /// `QueueSnapshotWriter` impl below, the moment it releases the queue
    /// lock — so the only remaining direct callers are the ones with no
    /// mutation of their own (`readmit_recovered`'s `put_back`, which makes
    /// a `recovered_queue` change durable). The old arrangement — a table
    /// in `docs/design/orchestration.md` listing who owed this call — is
    /// what let #533's two new mutators merge clean while owing a write
    /// nobody knew about.
    ///
    /// Best-effort by construction: a failed write leaves the previous good
    /// snapshot (that is `atomic_write`'s whole contract) and is audited
    /// rather than propagated, because the alternative — failing the
    /// delivery that triggered it — would turn a durability degradation
    /// into an outage of the thing being made durable.
    fn persist_queues(&self, group: &GroupId) {
        // **Never overwrite a snapshot nobody has read yet.** Recovery is
        // lazy (see `recover_persisted_queue`), and the first thing that
        // touches a group after a restart is not guaranteed to be a bind or
        // a `queue_orphans` call — it can be an admission, which would
        // rewrite the file with live state and destroy the previous
        // process's backlog before anything looked at it. Hanging the
        // read off the WRITE closes that by construction rather than by
        // relying on call-site ordering. Idempotent and cheap after the
        // first pass (one `HashSet` probe).
        //
        // Deliberately BEFORE `queue_persist` is acquired: recovery can send
        // an `[orrerix]` notice, which is itself a delivery, which persists —
        // and `std::sync::Mutex` is not reentrant, so doing this under the
        // writer lock would deadlock. The recursive call re-enters
        // `recover_persisted_queue` as a no-op (the group is marked before
        // any I/O runs) and takes the writer lock while this frame still
        // holds nothing.
        self.recover_persisted_queue(group);
        let _writer = self.queue_persist.lock_safe();
        // #547: roll the staged tail off the hot path BEFORE measuring what
        // the snapshot must carry, so this write pays for what is still
        // live-ish rather than for every orphan the install has accumulated.
        // Under `queue_persist`, which is what serializes it against the
        // archive's other writer (`readmit_archived`'s rewrite).
        self.archive_staged_overflow(group);
        let entries = self.group_queue_entries(group);
        let body = queue::serialize_snapshot(now_ms(), entries);
        if let Err(e) = atomic_write(&self.queue_snapshot_path(group), body.as_bytes()) {
            self.audit(group, brand::AUDIT_ACTOR, "queue-persist-failed", json!({ "error": e.to_string() }));
        }
    }

    /// Move staged orphans that no longer belong on the hot write path into
    /// `queue-orphans-archive.jsonl` (#547). Called by `persist_queues` alone,
    /// with `queue_persist` already held.
    ///
    /// **Why the snapshot needed a bound at all.** `queue.json` is rewritten
    /// and fsynced on every admission, and it carries the staged set (#523).
    /// Staging is never cleared, and worker panes do not survive a restart, so
    /// each restart permanently adds that restart's unbindable backlog: the
    /// per-delivery write cost grows with the age of the install. The bound is
    /// stated in `queue::ArchivePolicy` — age, then an entry backstop, then a
    /// byte backstop — and argued at each constant.
    ///
    /// **This is a move, not an eviction, and the distinction is the whole
    /// design.** #547 rules out a cap and rules out an audited eviction: the
    /// staged set exists to report work nobody received, so dropping from it
    /// reintroduces one level down the silent loss #523 exists to eliminate.
    /// Everything rolled here is still on disk, still surfaced by
    /// `queue_orphans` (with `source: "archive"`), still re-admitted by
    /// `readmit_archived` when its pane comes back, and named individually in
    /// the audit. The only thing that changes is which file it is fsynced from.
    ///
    /// **Ordering: append, then remove — never the reverse.** The archive
    /// append is fsynced first and staging is only edited once it returned
    /// `Ok`. A crash or a failed append therefore leaves the entry in BOTH
    /// stores (or in staging alone), never in neither; `queue::parse_archive`
    /// dedupes an id that ended up twice, and `queue_orphans` skips an
    /// archived id that is still staged. The opposite order buys nothing and
    /// costs a lost payload in exactly the failure this file exists for.
    ///
    /// **Its audit lines are deliberately NOT terminal for the orphan scan**,
    /// the same rule `recover_persisted_queue`'s staging lines follow:
    /// `queue::orphaned_queue_entries` closes an id only on
    /// `delivery-dequeued`/`-dropped`/`-recovered`, and an archived entry's
    /// disposition has not changed — writing a terminal line here would tell
    /// the one derivation that needs no snapshot that the work was resolved.
    fn archive_staged_overflow(&self, group: &GroupId) {
        let now = now_ms();
        let policy = queue::ArchivePolicy::default();
        // Plan under the staging locks over a PROJECTION (id/age/bytes), not
        // over clones: this runs on every admission, and cloning the staged
        // set to ask whether any of it should move would be its own version of
        // the cost being removed. Lock order is `recovered_queue` →
        // `recovered_markers`, matching `group_queue_entries`.
        let to_roll: Vec<(queue::PersistedEntry, queue::ArchiveReason)> = {
            let staged = self.recovered_queue.lock_safe();
            let markers = self.recovered_markers.lock_safe();
            let mut costs: Vec<queue::StagedCost> = Vec::new();
            for list in [staged.get(group), markers.get(group)].into_iter().flatten() {
                costs.extend(list.iter().map(|e| queue::StagedCost {
                    id: e.delivery.id,
                    enqueued_ms: e.delivery.enqueued_ms,
                    bytes: e.delivery.payload.text().map_or(0, str::len),
                }));
            }
            let plan = queue::plan_archive(&costs, now, &policy);
            if plan.is_empty() {
                return;
            }
            let why: HashMap<u64, queue::ArchiveReason> = plan.into_iter().collect();
            let mut picked: Vec<(queue::PersistedEntry, queue::ArchiveReason)> = Vec::new();
            for list in [staged.get(group), markers.get(group)].into_iter().flatten() {
                for e in list.iter() {
                    if let Some(r) = why.get(&e.delivery.id) {
                        picked.push((e.clone(), *r));
                    }
                }
            }
            picked.sort_by_key(|(e, _)| e.delivery.id);
            picked
        };
        if to_roll.is_empty() {
            return;
        }
        let mut buf = String::new();
        for (e, why) in &to_roll {
            let rec = queue::new_archive_record(now, *why, e.clone());
            let line = queue::archive_line(&rec);
            if line.is_empty() {
                // `archive_line`'s serialize fallback. An unserializable entry
                // must stay staged rather than be removed against a line that
                // says nothing — the safe direction is "still costs a write",
                // not "gone".
                continue;
            }
            buf.push_str(&line);
            buf.push('\n');
        }
        let path = self.queue_archive_path(group);
        if buf.is_empty() {
            return;
        }
        if let Err(e) = append_durable(&path, buf.as_bytes()) {
            // Nothing has left staging yet, so this degrades to "the snapshot
            // stays fat" — a cost, not a loss. Named so a disk that has
            // started refusing writes is visible rather than inferred from a
            // file that keeps growing.
            self.audit(group, brand::AUDIT_ACTOR, "queue-archive-failed",
                json!({ "error": e.to_string(), "entries": to_roll.len() }));
            return;
        }
        let rolled: HashSet<u64> = to_roll.iter().map(|(e, _)| e.delivery.id).collect();
        {
            let mut staged = self.recovered_queue.lock_safe();
            let mut markers = self.recovered_markers.lock_safe();
            for list in [staged.get_mut(group), markers.get_mut(group)].into_iter().flatten() {
                list.retain(|e| !rolled.contains(&e.delivery.id));
            }
        }
        // One line per entry, with the payload's size rather than its bytes
        // (the bytes are in the archive, and the audit log already carries
        // them on that delivery's own `prompt` line). "What and why", per
        // entry, is the standard a silent roll would fail.
        for (e, why) in &to_roll {
            self.audit(group, brand::AUDIT_ACTOR, "queue-orphan-archived", json!({
                "to": e.delivery.agent_id,
                "id": e.delivery.id,
                "reason": why.as_str(),
                "staged_ms": now.saturating_sub(e.delivery.enqueued_ms),
                "bytes": e.delivery.payload.text().map_or(0, str::len),
                "file": "queue-orphans-archive.jsonl",
            }));
        }
    }

    /// The archived half of this group's orphan view (#547) — the same rows
    /// `queue_orphans` derives from staging, for entries that have rolled off
    /// the hot snapshot.
    ///
    /// Read on demand and never cached: `queue_orphans` is a once-per-session
    /// call by design (see its tool description), so a file read there is
    /// cheaper than any structure kept live for it — which would put the cost
    /// back on the hot path this change exists to clear.
    pub(in crate::orchestration) fn archived_orphans(&self, group: &GroupId) -> Vec<queue::OrphanedQueueEntry> {
        let Ok(text) = fs::read_to_string(self.queue_archive_path(group)) else {
            return Vec::new();
        };
        let (rows, skipped) = queue::parse_archive(&text);
        if skipped > 0 {
            self.audit(group, brand::AUDIT_ACTOR, "queue-archive-skipped", json!({ "entries": skipped }));
        }
        rows.into_iter()
            .map(|a| queue::OrphanedQueueEntry {
                id: a.entry.delivery.id,
                agent_id: a.entry.delivery.agent_id.clone(),
                enqueued_ms: a.entry.delivery.enqueued_ms,
                // A marker reports WHY IT CANNOT BE REPLAYED, exactly as it
                // does from staging — the archive changed where it lives, not
                // what it is, and `STRANDED_ORPHAN_REASON` is the one string
                // every channel uses for it.
                reason: match a.entry.delivery.payload {
                    queue::QueuedPayload::StrandedSubmit => queue::STRANDED_ORPHAN_REASON.to_string(),
                    _ => a.entry.delivery.reason.as_str().to_string(),
                },
                text: a.entry.delivery.payload.text().map(str::to_string),
                source: queue::OrphanSource::Archive,
            })
            .collect()
    }

    /// Re-admit archived entries for the pane that just bound (#547) — the
    /// half of the archive design that keeps it a MOVE rather than a
    /// downgrade.
    ///
    /// Without this, rolling an entry off the snapshot would quietly cost it
    /// its automatic rebind, and "nothing recoverable is destroyed" would hold
    /// only for the report. Matching is `queue::rebinds_to`, the same rule
    /// `readmit_recovered` uses, and admission goes through `enqueue_text` —
    /// the same front door — so an archived entry rejoins the queue by the
    /// ordinary rules.
    ///
    /// **Archived entries are re-admitted BEFORE staged ones** (the caller
    /// orders it), because they are strictly older: ids are monotonic per
    /// group and an entry only reaches the archive by being among the oldest
    /// staged. Arrival order across the two stores is the property #523's N5
    /// sort exists to protect, and it would break the other way round.
    ///
    /// **Ordering: admit, then unarchive.** `enqueue_text` persists the fresh
    /// live entry into `queue.json` before this removes the archived line, so
    /// a crash in between leaves the payload in both stores rather than in
    /// neither. Its cost is the double-delivery window #467 already bounds:
    /// the replay is byte-identical, so `queue::admit`'s exact-equality
    /// coalesce collapses it into the live entry.
    ///
    /// The rewrite re-reads the file under `queue_persist` rather than writing
    /// back what this function read minutes earlier — `archive_staged_overflow`
    /// may have appended in between, and rewriting a stale copy would delete
    /// those appends. Returns how many were re-admitted and the oldest
    /// `enqueued_ms` among them (`u64::MAX` when none), which the caller folds
    /// into the recovery notice's age.
    fn readmit_archived(
        &self,
        group: &GroupId,
        agent_id: &str,
        pty_id: u32,
        target: &DurableTarget,
    ) -> (usize, u64) {
        let path = self.queue_archive_path(group);
        let Ok(text) = fs::read_to_string(&path) else {
            return (0, u64::MAX);
        };
        let (rows, _skipped) = queue::parse_archive(&text);
        let mut candidates: Vec<queue::PersistedEntry> = rows
            .into_iter()
            .map(|a| a.entry)
            // A marker is never replayable (`queue::split_recovered`'s
            // judgement): the pane whose input box gave it meaning is gone.
            // It stays archived and stays reported.
            .filter(|e| {
                e.delivery.payload.text().is_some()
                    && queue::rebinds_to(&e.delivery, target.is_orchestrator, target.session_id.as_deref())
            })
            .collect();
        if candidates.is_empty() {
            return (0, u64::MAX);
        }
        candidates.sort_by_key(|e| e.delivery.id);
        let mut done: HashSet<u64> = HashSet::new();
        let mut oldest_ms = u64::MAX;
        for e in candidates {
            let Some(body) = e.delivery.payload.text().map(str::to_string) else { continue };
            let from = e.delivery.from.clone();
            match self.enqueue_text(group, agent_id, &from, &body, pty_id, queue::EnqueueReason::Recovered) {
                Ok(fresh) => {
                    oldest_ms = oldest_ms.min(e.delivery.enqueued_ms);
                    done.insert(e.delivery.id);
                    // Same closing line staging's re-admission writes, for the
                    // same reason (the old id's disposition is now durable
                    // under a fresh one), plus which store it came out of.
                    self.audit(group, brand::AUDIT_ACTOR, "delivery-recovered", json!({
                        "to": agent_id, "id": e.delivery.id, "readmitted_as": fresh.id,
                        "source": "archive",
                        "queued_ms": now_ms().saturating_sub(e.delivery.enqueued_ms),
                    }));
                }
                // The pane is at `QUEUE_MAX_PER_PANE`. It stays archived and
                // stays reported — the same "back where it was, never dropped"
                // answer `readmit_recovered`'s rejection path gives.
                Err(_) => {}
            }
        }
        if done.is_empty() {
            return (0, u64::MAX);
        }
        {
            let _writer = self.queue_persist.lock_safe();
            let current = fs::read_to_string(&path).unwrap_or_default();
            // **Over RAW lines, never over parsed records** (#547 review B1).
            // `parse_archive` drops a line whose version this build does not
            // know — correct for acting on records, fatal for rewriting the
            // file: rebuilding it from parsed output deletes exactly the
            // lines the per-line `v` exists to protect, so a rollback after a
            // `v: 2` writer would lose them on the next bind. Parsing here
            // answers one question only — "is this the record I am
            // deliberately removing?" — and everything else, including a line
            // that is not JSON at all, is carried through byte for byte.
            let mut body = String::new();
            for line in queue::scan_archive(&current) {
                if line.id.is_some_and(|id| done.contains(&id)) {
                    continue;
                }
                body.push_str(line.raw);
                body.push('\n');
            }
            if let Err(e) = atomic_write(&path, body.as_bytes()) {
                // The entries are already live and delivering; a failed
                // rewrite only means the archive still lists them, which
                // surfaces as an orphan row for work that is in flight. Named
                // rather than silent, because that is a confusing row to read.
                self.audit(group, brand::AUDIT_ACTOR, "queue-archive-rewrite-failed",
                    json!({ "error": e.to_string(), "entries": done.len() }));
            }
        }
        (done.len(), oldest_ms)
    }

    /// Everything `group` still owes a pane: the live queues, plus whatever a
    /// previous restart staged and has not re-bound yet.
    ///
    /// Live entries come first, in exact drain order (per pane front-first,
    /// panes by ascending pty id so the file is stable), filtered on the
    /// entry's OWN `group` stamp rather than by re-deriving each pane's group
    /// from the agents map — see `QueuedDelivery::group`.
    ///
    /// **Staged entries are included, and that is what makes recovery
    /// survive a SECOND restart** (review round 1, finding 1). Writing live
    /// queues only meant the first admission after a restart rewrote the file
    /// without the backlog it had just staged into memory — so a process that
    /// died before the orchestrator's session-start `queue_orphans` call took
    /// the only remaining copy with it. Restarts cluster (a crash loop, or a
    /// fleet restart followed by another), and the exposed set was precisely
    /// the case this feature exists for: entries whose pane did not come
    /// back. Re-admitted entries leave staging and reappear here as LIVE
    /// under fresh ids, so nothing is written twice.
    ///
    /// Lock order is `queues` → `recovered_queue` → `recovered_markers`. The
    /// only other site touching staging (`readmit_recovered`) takes and
    /// releases `recovered_queue` before it ever calls into `queues`, so the
    /// two never overlap in the opposite order.
    fn group_queue_entries(&self, group: &GroupId) -> Vec<queue::PersistedEntry> {
        let mut out = Vec::new();
        {
            let queues = self.queues.read();
            let mut ptys: Vec<u32> = queues.keys().copied().collect();
            ptys.sort_unstable();
            for pty_id in ptys {
                let Some(q) = queues.get(&pty_id) else { continue };
                for d in q.iter().filter(|d| d.group.as_ref() == Some(group)) {
                    out.push(queue::PersistedEntry { pty_id, delivery: d.clone() });
                }
            }
        }
        if let Some(staged) = self.recovered_queue.lock_safe().get(group) {
            out.extend(staged.iter().cloned());
        }
        if let Some(markers) = self.recovered_markers.lock_safe().get(group) {
            out.extend(markers.iter().cloned());
        }
        // **Sorted by id, which for this group IS arrival order** (review
        // round 2, N5). Appending staged after live is not restart-invariant:
        // a cap-rejected entry from restart 1 would be written behind
        // deliveries queued after it, and a SECOND restart would then re-admit
        // them in that inverted order — silently breaking the per-pane arrival
        // order this feature exists to preserve. `queue_seq` is monotonic per
        // group and seeded past the snapshot on every recovery, so an old
        // staged id always sorts ahead of a fresh live one.
        //
        // The one thing this costs, stated because the file is also a human
        // forensic artifact: a `StrandedSubmit` marker is pushed to the FRONT
        // of its pane's queue but minted with a HIGHER id, so its position in
        // the file no longer mirrors its drain position. That is acceptable
        // here and nowhere else — a marker is never replayed across a restart
        // (`split_recovered`), so no recovery decision reads its file order,
        // and its `pty_id` + id still identify it exactly.
        out.sort_by_key(|e| e.delivery.id);
        out
    }

    /// The durable identity of a delivery target (#467), resolved at
    /// admission because that is the only moment the agent is guaranteed to
    /// still exist. See `QueuedDelivery::to_orchestrator`/`session_id` for
    /// why neither `pty_id` nor `agent_id` can serve. An unknown agent
    /// (integration tests seeding a queue for a pane with no roster entry)
    /// resolves to "no durable identity", which surfaces as an orphan —
    /// the safe direction.
    pub(in crate::orchestration) fn durable_target(&self, agent_id: &str) -> DurableTarget {
        match self.agent(agent_id) {
            Some(a) => DurableTarget {
                is_orchestrator: a.role == Role::Orchestrator,
                session_id: a.session_id.clone(),
            },
            None => DurableTarget { is_orchestrator: false, session_id: None },
        }
    }

    /// Read `group`'s `queue.json` back after a restart (#467), exactly once
    /// per process.
    ///
    /// **Lazy, not startup-wired, and that is deliberate.** There is no
    /// single "a group was restored" callback in this registry — a group
    /// comes back when its orchestrator pane rejoins, which is one of
    /// several paths — so recovery hangs off first touch instead: a pane
    /// binding (`readmit_recovered`), the orchestrator's session-start
    /// `queue_orphans` call, or an admission (`persist_queues` runs it
    /// before it can overwrite the file). `recovered_groups` makes it
    /// idempotent: a second pass would re-stage entries the first already
    /// re-admitted, and deliver them twice.
    ///
    /// **The `recovered_groups` guard is held across the whole critical
    /// phase, not just the check (review round 1, finding 2).** It used to
    /// be published in a temporary guard that was released before the file
    /// read, the `queue_seq` seed and the staging insert — so a SECOND
    /// thread arriving in that window saw "already recovered", returned, and
    /// minted an id the snapshot still held. That is not a theoretical race:
    /// MCP dispatch is thread-per-request, and the moment right after a
    /// restart is exactly when every agent in a group reports at once (what
    /// #524's fleet restart produced). Holding the guard makes a concurrent
    /// first touch WAIT for the seed instead of racing it, and closes the
    /// same window for `readmit_recovered`, which would otherwise see empty
    /// staging mid-recovery and deliver a kickoff ahead of the backlog.
    ///
    /// **Hard invariant for anything added inside that critical section: it
    /// may not deliver, enqueue, or otherwise re-enter recovery.**
    /// `std::sync::Mutex` is not reentrant, so a re-entry on THIS thread
    /// deadlocks rather than looping. Auditing is fine (file I/O, no
    /// callback); notices are not, which is why every notice is collected
    /// and sent in phase 2, after the guard drops.
    ///
    /// The snapshot file is NOT deleted here, and `persist_queues` writes
    /// staged entries back out alongside the live ones — so recovery is
    /// re-runnable across any number of restarts (review round 1, finding 1).
    pub(in crate::orchestration) fn recover_persisted_queue(&self, group: &GroupId) {
        // ---- phase 1: under the guard, everything another thread must not
        // observe half-done (the seed, and staging itself) ----
        let notices: Vec<String> = {
            let mut recovered = self.recovered_groups.lock_safe();
            if !recovered.insert(group.clone()) {
                return;
            }
            // Publishing the mark here rather than at the end is safe
            // BECAUSE the guard is held for the rest of the phase: no other
            // thread can observe it until this scope ends, and every early
            // return below (no file, unparseable file) still needs the group
            // marked so recovery is not retried on every later call.
            let Ok(text) = fs::read_to_string(self.queue_snapshot_path(group)) else {
                return;
            };
            let (entries, skipped) = queue::parse_snapshot(&text);
            if skipped > 0 {
                // A partially-unreadable snapshot recovers what it can; the
                // rest is named rather than silently short.
                self.audit(group, brand::AUDIT_ACTOR, "queue-recover-skipped", json!({ "entries": skipped }));
            }
            // **Push `queue_seq` past every id this snapshot holds, before
            // any id is minted for this group in the new process.** The
            // counter is in-memory and restarts at zero, so without this a
            // fresh admission after a restart gets id 1 — the same id a
            // recovered entry already carries. Two live consequences, both
            // silent: `queue_orphans`'s live-id filter would hide a real
            // orphan behind an unrelated fresh delivery that happened to
            // reuse its number, and the audit scan would see the NEW id's
            // `delivery-dequeued` close out the OLD id's `delivery-queued`.
            // Every id-minting path calls recovery before it mints (see
            // `enqueue_text`), and the guard above is what makes "before"
            // true for OTHER threads too. Ids stay unique only WITHIN a
            // group; a collision across groups is harmless because every
            // consumer of an id — the live filter, the audit scan, the
            // snapshot — is group-scoped.
            // #547: the archive holds ids the snapshot no longer does, so the
            // seed has to read BOTH or it regresses exactly when archiving has
            // done its job. A group whose whole staged set has rolled off has
            // an EMPTY snapshot and a non-empty archive; seeding from the
            // snapshot alone would restart the counter at zero and hand a
            // fresh delivery an id an archived orphan still carries — the same
            // two silent consequences this seed exists to prevent, reintroduced
            // by the fix for a different problem. One file read, once per
            // group per process, inside the guard: no delivery, no re-entry.
            //
            // Over RAW lines, for the same reason the rewrite is (#547 review
            // B1): reading ids through `parse_archive` would skip every line
            // this build cannot interpret, so a `v: 2` record written before a
            // rollback would carry an id the seed never saw — which is this
            // hazard again, arriving through the compatibility mechanism meant
            // to prevent it.
            let archive_text =
                fs::read_to_string(self.queue_archive_path(group)).unwrap_or_default();
            let archive_lines = queue::scan_archive(&archive_text);
            let archived_high = archive_lines.iter().filter_map(|l| l.id).max().unwrap_or(0);
            // A line whose id cannot be read even as raw JSON is the one case
            // the seed genuinely cannot account for — there is no number to
            // account. Said out loud rather than left as a silent shortfall,
            // because the consequence (a re-minted id) is itself silent.
            let opaque = archive_lines.iter().filter(|l| l.id.is_none()).count();
            if opaque > 0 {
                self.audit(group, brand::AUDIT_ACTOR, "queue-archive-id-unreadable", json!({ "lines": opaque }));
            }
            let high = entries.iter().map(|e| e.delivery.id).max().unwrap_or(0).max(archived_high);
            let _ = self.queue_seq.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                (cur < high).then_some(high)
            });
            let split = queue::split_recovered(entries);
            let mut notices = Vec::new();
            // **These audit lines are deliberately NOT terminal for the
            // orphan scan** (review round 1, finding 1). The previous
            // version closed each recovered id at STAGE time — which made
            // the audit derivation, the one view that needs no snapshot,
            // blind to entries that were only staged. Combined with a
            // snapshot that held live entries only, a second restart lost
            // them with no trace anywhere: strictly worse than not having
            // persisted at all. An id is now closed exactly when its
            // disposition becomes durable somewhere else — `readmit_
            // recovered` writes `delivery-recovered` when the entry leaves
            // staging under a fresh id — so until then BOTH views report it
            // and `queue::merge_orphans` dedupes them.
            for e in &split.replayable {
                self.audit(group, brand::AUDIT_ACTOR, "queue-recovered", json!({
                    "to": e.delivery.agent_id, "id": e.delivery.id,
                    "queued_ms": now_ms().saturating_sub(e.delivery.enqueued_ms),
                }));
            }
            for m in &split.markers {
                self.audit(group, brand::AUDIT_ACTOR, "queue-stranded-unreplayable", json!({
                    "to": m.delivery.agent_id, "id": m.delivery.id,
                    "reason": queue::STRANDED_ORPHAN_REASON,
                }));
            }
            if !split.markers.is_empty() {
                notices.push(queue::stranded_lost_notice(split.markers.len()));
                self.recovered_markers.lock_safe().insert(group.clone(), split.markers);
            }
            if !split.replayable.is_empty() {
                self.recovered_queue.lock_safe().insert(group.clone(), split.replayable);
            }
            notices
        };
        // ---- phase 2: guard released. A notice is a delivery, a delivery
        // enqueues, an enqueue persists, and persisting calls back into this
        // function — which is exactly why it cannot happen above. ----
        for n in notices {
            let _ = self.deliver_to_orchestrator(group, &n, brand::AUDIT_ACTOR);
        }
    }

    /// Re-admit whatever a restart left staged for the pane that just bound
    /// (#467). Called from every bind site, BEFORE that pane's own kickoff
    /// is delivered — an entry queued before the restart arrived before the
    /// kickoff, and admission order is delivery order, so re-admitting after
    /// the kickoff would silently reorder them.
    ///
    /// Matching is `queue::rebinds_to`'s rule (the group's orchestrator, or
    /// the same CLI session id) — never `agent_id`, which is re-minted at
    /// restore. Entries that match are re-admitted in their original order
    /// through `enqueue_text`, the SAME front door every other delivery
    /// uses: they take their place in the live queue by the ordinary
    /// admission rules rather than being spliced in beside it, which is also
    /// what makes the existing byte-identical coalesce cover the one
    /// double-delivery window this design has (see `pop_front_dequeued`).
    /// Entries that do not match stay staged for `queue_orphans`.
    ///
    /// Returns how many were re-admitted.
    #[doc(hidden)] // pub for integration tests
    pub fn readmit_recovered(&self, group: &GroupId, agent_id: &str, pty_id: u32) -> usize {
        self.recover_persisted_queue(group);
        // #581 §4: the merge queue's own restart reconcile, beside the delivery
        // queue's. Same shape (once-only guard, two phases), same reason for
        // being on a BIND site rather than on group creation: phase 2 delivers a
        // notice, and at group-creation time no pane exists to receive one.
        // A no-op (one `HashSet` probe) on every call after the first, and a
        // no-op over an absent `merge_queue.json` on every group that has never
        // enqueued — which is all of them until slice E ships the MCP tools.
        self.merge_queue_reconcile(group);
        let target = self.durable_target(agent_id);
        // #547: the archive first, because everything in it is older than
        // everything still staged (an entry reaches it by being among the
        // oldest) and admission order is delivery order. See
        // `readmit_archived`.
        let (from_archive, archive_oldest_ms) = self.readmit_archived(group, agent_id, pty_id, &target);
        let matches = |e: &queue::PersistedEntry| {
            queue::rebinds_to(&e.delivery, target.is_orchestrator, target.session_id.as_deref())
        };

        // **Exactly one entry is ever out of both durable stores at a time,
        // on every path** (review round 2 N4; review round 3 blocking 1).
        //
        // Round 2's shape partitioned the whole matching set out of staging
        // up front, so every entry after the one being admitted lived ONLY in
        // a local vector and a crash mid-loop dropped the remainder. Taking
        // them one at a time fixed the SUCCESS path but not the rejection
        // path: a rejected entry was parked in a local vector until the loop
        // ended, so a recovered backlog meeting a pane already at
        // `QUEUE_MAX_PER_PANE` still had every matching entry outside both
        // stores simultaneously — precisely the case hazard 5 covers. That
        // left the design note asserting an invariant this branch of the code
        // did not honour, which is the defect, not the window itself.
        //
        // So a rejected entry goes straight back into staging and the
        // snapshot is rewritten before the next one is taken. `parked` is
        // what stops the loop re-offering it forever, and it holds ids rather
        // than entries so nothing is ever held outside a durable store to
        // track it. The cost is one extra snapshot write per rejection, in a
        // path that only runs when a full pane meets a recovered backlog.
        //
        // The loop re-locks per iteration rather than hoisting the guard
        // because holding `recovered_queue` across `enqueue_text` would
        // invert this file's lock order (see `group_queue_entries`).
        // Seeded from the archive pass, not zeroed: an entry re-admitted out of
        // the archive was offered, was re-admitted, and is as old as it says —
        // and the `offered == 0` early return below would otherwise swallow a
        // bind whose whole backlog had rolled off (no `delivery-requeued` line,
        // no drainer nudge, no notice, and a return of 0 for work that IS
        // queued).
        let mut readmitted = from_archive;
        let mut offered = from_archive;
        let mut rejected = 0usize;
        let mut oldest_ms = archive_oldest_ms;
        let mut parked: HashSet<u64> = HashSet::new();
        // Return `entry` to staging and make that durable before anything
        // else is taken out. Takes the id back so the caller can mark it
        // parked without holding the entry.
        let put_back = |entry: queue::PersistedEntry| -> u64 {
            let id = entry.delivery.id;
            self.recovered_queue.lock_safe().entry(group.clone()).or_default().push(entry);
            self.persist_queues(group);
            id
        };
        loop {
            let next = {
                let mut staged = self.recovered_queue.lock_safe();
                let Some(list) = staged.get_mut(group) else { break };
                match list.iter().position(|e| matches(e) && !parked.contains(&e.delivery.id)) {
                    Some(i) => list.remove(i),
                    None => break,
                }
            };
            offered += 1;
            oldest_ms = oldest_ms.min(next.delivery.enqueued_ms);
            // A payload-less entry cannot be here (markers stage into
            // `recovered_markers`, never `recovered_queue`) — but put it back
            // rather than `continue`ing past it, so an impossible case that
            // becomes possible later degrades to "still an orphan" instead of
            // to a silent drop out of the staging area.
            let Some(text) = next.delivery.payload.text().map(str::to_string) else {
                rejected += 1;
                parked.insert(put_back(next));
                continue;
            };
            let from = next.delivery.from.clone();
            match self.enqueue_text(group, agent_id, &from, &text, pty_id, queue::EnqueueReason::Recovered) {
                Ok(fresh) => {
                    readmitted += 1;
                    // Close the OLD id out here and nowhere else (review
                    // round 1, finding 1): this is the moment its
                    // disposition becomes durable somewhere else — a fresh
                    // `delivery-queued` line, and a live queue entry the
                    // snapshot now carries. Written before recovery stages,
                    // it blinded the audit derivation to entries that only
                    // ever got staged.
                    self.audit(group, brand::AUDIT_ACTOR, "delivery-recovered", json!({
                        "to": agent_id, "id": next.delivery.id, "readmitted_as": fresh.id,
                        "queued_ms": now_ms().saturating_sub(next.delivery.enqueued_ms),
                    }));
                }
                // Re-admission can legitimately fail — the pane's queue is
                // capped at `QUEUE_MAX_PER_PANE` like any other, and a
                // recovered backlog can meet a queue that already has entries
                // in it. Losing it here would be the exact silent loss this
                // whole feature exists to prevent — worse than the original
                // bug, because the sender was told long ago it was queued.
                // Back into staging and onto disk immediately; see the
                // invariant comment above the loop.
                Err(_) => {
                    rejected += 1;
                    parked.insert(put_back(next));
                }
            }
        }
        if offered == 0 {
            return 0;
        }
        if rejected > 0 {
            self.audit(group, brand::AUDIT_ACTOR, "delivery-requeue-rejected", json!({
                "to": agent_id, "pty": pty_id, "count": rejected, "reason": "queue-full",
            }));
        }
        self.audit(group, brand::AUDIT_ACTOR, "delivery-requeued", json!({
            "to": agent_id, "pty": pty_id, "count": readmitted, "offered": offered,
        }));
        if readmitted > 0 {
            // Best-effort nudge: the entries are in the queue either way,
            // but nothing has spawned a drainer for a pane that was empty
            // until this moment.
            if let (Some(app), Some(reg)) = (self.app.lock_safe().clone(), self.arc()) {
                reg.ensure_drainer(app, group.clone(), pty_id, None);
            }
            let minutes = now_ms().saturating_sub(oldest_ms) / 60_000;
            let target_is_orchestrator = target.is_orchestrator;
            self.notify_queue(group, agent_id, target_is_orchestrator,
                &queue::recovered_notice(agent_id, readmitted, minutes));
        }
        readmitted
    }

    /// Every delivery this group REFUSED at the front door (#579) — the target
    /// pane's queue was at `queue::QUEUE_MAX_PER_PANE`, so nothing was queued
    /// and there is no id to report it under. Derived from `audit.jsonl` alone
    /// (`front_door_refusals`, pure), because the audit line is the only record
    /// a refusal ever produces.
    ///
    /// Read-only by construction: nothing here touches `recovered_queue`, so a
    /// refused delivery can never be re-admitted as a side effect of being
    /// reported — the one thing #579 asked to be sure of, since a delivery
    /// declined an hour ago would reorder against everything the pane has
    /// accepted since.
    #[doc(hidden)] // pub for integration tests
    pub fn front_door_refusals(&self, group: &GroupId) -> FrontDoorRefusals {
        // `audit_log_windowed`, not `audit_log`: the derivation has to know
        // whether its own timeline was cut, or a count over a partial window
        // reads as a count over all of history (#579 review NB1).
        let (entries, window_truncated) = self.audit_log_windowed(group);
        front_door_refusals(&entries, window_truncated)
    }

    /// The pre-#468 orphan derivation, unchanged: scan `group`'s audit log
    /// for `delivery-queued` ids with no matching `delivery-dequeued`/
    /// `delivery-dropped`. Split out of `queue_orphans` so the snapshot
    /// derivation can be merged over it (and so this one stays directly
    /// testable on its own).
    pub(in crate::orchestration) fn audit_derived_orphans(&self, group: &GroupId) -> Vec<queue::OrphanedQueueEntry> {
        let entries = self.audit_log(group);
        let lines: Vec<queue::QueueAuditLine> = entries
            .iter()
            .map(|e| queue::QueueAuditLine {
                action: e.action.as_str(),
                id: e.detail.get("id").and_then(Value::as_u64),
                agent_id: e.detail.get("to").and_then(Value::as_str),
                reason: e.detail.get("reason").and_then(Value::as_str),
                ts_ms: e.ts_ms,
            })
            .collect();
        queue::orphaned_queue_entries(&lines)
    }

    /// Human steering from the loomux compose strip (#43, option C): enqueue
    /// `text` to the group's orchestrator through the SAME per-pane serialized
    /// delivery path worker reports use. Rejects empty text and a paused group
    /// up front so the strip can tell the human why nothing was sent.
    ///
    /// #569 changed that guard's reason, not its answer: a paused group's
    /// delivery is no longer discarded, it is queued until resume. But this is
    /// a human typing into the compose strip *now*, and a message that silently
    /// waits an unknown number of hours is not what the strip's Send button
    /// promises — an immediate error the human can act on (resume, then send)
    /// beats deferring their sentence to a moment they haven't chosen yet.
    /// `start_task` rejects for the same reason. A dead/absent orchestrator
    /// surfaces as the "no live orchestrator" error from delivery.
    #[doc(hidden)] // pub for integration tests
    pub fn steer_orchestrator(&self, group: &GroupId, text: &str) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("empty steering message".into());
        }
        if self.is_paused(group) {
            return Err("group is paused — resume it before steering".into());
        }
        self.deliver_to_orchestrator(group, text, "human")
    }

    /// Close the delivery feedback loop (#103): when a delivery to a
    /// non-orchestrator agent finishes with its submit unconfirmed, tell the
    /// group's orchestrator once so it can `get_output` the pane and re-send if
    /// the prompt is stranded unsubmitted. No-op for orchestrator-target or
    /// confirmed deliveries (`should_notify_unconfirmed`), and — like the
    /// watchdog nudge — a paused group is skipped entirely: delivery is
    /// suppressed there anyway, so we must not spend the notice budget while
    /// paused. Best-effort (a dead orchestrator just drops it) and audited. The
    /// notice is itself a delivery TO the orchestrator, so it can never trigger a
    /// notice of its own — no loops.
    #[doc(hidden)] // pub for integration tests
    /// `eaten` (#585) selects the wording only — every suppression rule above
    /// it is identical for both. An eaten delivery is not a different KIND of
    /// unconfirmed delivery to the orchestrator-loop or paused-group gates; it
    /// is the same event described accurately (see `delivery_eaten_notice`).
    pub fn notify_unconfirmed_delivery(
        &self,
        group: &GroupId,
        agent_id: &str,
        target_is_orchestrator: bool,
        confirmed: bool,
        eaten: bool,
        delivery_id: u64,
    ) {
        if !should_notify_unconfirmed(target_is_orchestrator, confirmed) {
            // #445: every suppression path leaves a trace — a suppressed
            // notification must be discoverable after the fact, never just
            // vanish (the routed #451 finding this issue also owns).
            self.audit(group, brand::AUDIT_ACTOR, "notice-suppressed", json!({
                "kind": "unconfirmed-delivery", "to": agent_id, "delivery_id": delivery_id,
                "eaten": eaten,
                "reason": if target_is_orchestrator { "target-is-orchestrator" } else { "already-confirmed" },
            }));
            return;
        }
        if self.is_paused(group) {
            self.audit(group, brand::AUDIT_ACTOR, "notice-suppressed", json!({
                "kind": "unconfirmed-delivery", "to": agent_id, "delivery_id": delivery_id,
                "eaten": eaten, "reason": "group-paused",
            }));
            return;
        }
        // #539: buffer, don't send. The alarm is emitted by
        // `flush_unconfirmed_notices` at the end of a short window so that N
        // alarms on one pane cost the orchestrator one turn instead of N.
        if !self.buffer_unconfirmed_delivery(group, agent_id, eaten, delivery_id) {
            return; // an armed window already owns this bucket
        }
        // Exactly one timer per bucket — armed by the alarm that CREATED it,
        // never re-armed by the ones that join it, so the window cannot be
        // extended a delivery at a time by a pane that keeps failing (the
        // same "bounded from the ORIGINAL attempt" rule #535's busy deferral
        // follows). The thread is the whole deferral: it sleeps, then calls
        // the same flush a caller can call directly.
        //
        // A registry with no `self_arc` (a bare/headless construction — see
        // `set_self_arc`) has no owner to hand the window to, so it flushes
        // inline and behaves exactly as it did pre-#539. That is the honest
        // degrade rather than a bucket nothing would ever drain.
        let Some(me) = self.arc() else {
            self.flush_unconfirmed_notices(group, agent_id, eaten);
            return;
        };
        let (g, a) = (group.clone(), agent_id.to_string());
        std::thread::spawn(move || {
            std::thread::sleep(UNCONFIRMED_NOTICE_COALESCE_WINDOW);
            me.flush_unconfirmed_notices(&g, &a, eaten);
        });
    }

    /// Add one alarm to a pane's coalescing bucket (#539), returning whether
    /// this call OPENED the bucket (and therefore owes it a flush).
    ///
    /// Split out of `notify_unconfirmed_delivery` so the two halves of the
    /// coalescer — accumulate, then emit — are separately callable, and so a
    /// test can compose them exactly as production does with the only
    /// difference being that it does not sleep out the window. The gates
    /// (`should_notify_unconfirmed`, paused) stay in the caller: they are
    /// per-delivery facts evaluated when the alarm is raised, and the one
    /// pane-wide gate that can change during the window is re-checked at
    /// flush time per #532.
    #[doc(hidden)] // pub for integration tests
    pub fn buffer_unconfirmed_delivery(&self, group: &GroupId, agent_id: &str, eaten: bool, delivery_id: u64) -> bool {
        let opened = {
            let mut pending = self.unconfirmed_pending.lock_safe();
            let bucket =
                pending.entry((group.to_string(), agent_id.to_string(), eaten)).or_default();
            bucket.push(delivery_id);
            bucket.len() == 1
        };
        self.audit(group, brand::AUDIT_ACTOR, "delivery-unconfirmed-buffered", json!({
            "to": agent_id, "delivery_id": delivery_id, "opened_window": opened, "eaten": eaten,
            "window_ms": UNCONFIRMED_NOTICE_COALESCE_WINDOW.as_millis() as u64,
        }));
        opened
    }

    /// Withdraw ONE delivery's buffered alarm before the window closes (#539,
    /// rev-13 finding A) — the delivery turned out to have landed after all.
    ///
    /// **The defect this closes is one the coalescing delay itself created.**
    /// `late_monitor_tick` checks `hook_match` before `already_failed`, so a
    /// late `promptsubmit` record arriving AFTER a failure was declared is a
    /// first-class outcome (`Confirm { correction: true }` exists for exactly
    /// it) — and the monitor keeps polling every `LATE_MONITOR_POLL` (5s), so
    /// two or three of those ticks land inside a 15s window. Pre-#539 the
    /// alarm had already gone out before any correction could exist, so the
    /// orchestrator could only ever see "unconfirmed" then "correction, it
    /// landed". With a buffer in between, the correction can overtake the
    /// alarm and the orchestrator gets them backwards: "it landed" at t+5s,
    /// then "unconfirmed … id N …" at t+15s — a `get_output` probe plus the
    /// tempting re-send that #451/#510 exist to prevent. A change whose whole
    /// purpose is removing false unconfirmed notices must not manufacture one.
    ///
    /// **Retract, rather than re-derive at flush.** The flush has nothing to
    /// re-derive FROM: `last_delivery` holds the most recent `DeliveryOutcome`
    /// per pty, not one per delivery id, so for a multi-id batch no ledger can
    /// answer "is id N specifically still unconfirmed". The path that learns
    /// the delivery resolved is the one that knows which id resolved, so the
    /// withdrawal belongs there.
    ///
    /// Called unconditionally on `MonitorAction::Confirm`, not only when
    /// `correction` is set: an id that was never buffered is simply absent and
    /// this is a no-op, which is a cheaper thing to be sure of than the exact
    /// equivalence between "an alarm fired" and "`already_failed` is true".
    ///
    /// If this empties the bucket the key is dropped, and an already-armed
    /// timer will fire into nothing (harmless — `flush_unconfirmed_notices`
    /// returns early on an empty bucket). A LATER alarm for the same pane then
    /// opens a fresh bucket and arms its own timer, so the worst case is a
    /// notice delivered sooner than a full window — less coalescing, never a
    /// lost or a spurious notice.
    ///
    /// **The invariant this rests on:** a flush REMOVES the whole bucket, so
    /// an id still in it has provably not been announced yet. "Withdrawn" and
    /// "the orchestrator never heard about it" are therefore the same fact —
    /// which is what lets the caller skip the correction notice for a
    /// withdrawn id (a correction refers to an alarm the reader has already
    /// seen; for one that never went out it would be a turn spent describing
    /// a message that does not exist).
    ///
    /// Returns whether anything was actually withdrawn, so the audit can say
    /// so and a test can assert it.
    #[doc(hidden)] // pub for integration tests
    pub fn retract_unconfirmed_delivery(&self, group: &GroupId, agent_id: &str, delivery_id: u64) -> bool {
        // Both kinds: a correction resolves the DELIVERY, and which flavour
        // of alarm it happened to raise is not something the confirming tick
        // knows, or should have to.
        let (withdrawn, remaining) = {
            let mut pending = self.unconfirmed_pending.lock_safe();
            let mut withdrawn = false;
            let mut remaining = 0usize;
            for eaten in [false, true] {
                let key = (group.to_string(), agent_id.to_string(), eaten);
                let Some(bucket) = pending.get_mut(&key) else { continue };
                let before = bucket.len();
                bucket.retain(|id| *id != delivery_id);
                remaining += bucket.len();
                withdrawn |= bucket.len() < before;
                if bucket.is_empty() {
                    pending.remove(&key);
                }
            }
            (withdrawn, remaining)
        };
        if withdrawn {
            self.audit(group, brand::AUDIT_ACTOR, "delivery-unconfirmed-retracted", json!({
                "to": agent_id, "delivery_id": delivery_id, "remaining": remaining,
                "reason": "a prompt-landed signal arrived for this delivery before the \
                           coalescing window closed — it must not be announced as unconfirmed",
            }));
        }
        withdrawn
    }

    /// Emit the one coalesced unconfirmed-delivery notice for a pane's
    /// buffered alarms (#539) and clear the bucket.
    ///
    /// Called by the timer `notify_unconfirmed_delivery` arms, and directly by
    /// tests so the coalescing is assertable without sleeping on a real
    /// window. Idempotent: the bucket is TAKEN, so a second call for the same
    /// pane finds nothing and returns without delivering an empty notice.
    ///
    /// The paused check is repeated here rather than trusted from arm time
    /// (#532): a group can be paused during the window, and delivery to a
    /// paused group is suppressed anyway — spending the notice budget into one
    /// would be a write whose gate was verified against a state that no longer
    /// holds. The suppression is audited with every id it swallowed, so the
    /// deliveries are still discoverable (#445).
    #[doc(hidden)] // pub for integration tests
    pub fn flush_unconfirmed_notices(&self, group: &GroupId, agent_id: &str, eaten: bool) {
        let ids = self
            .unconfirmed_pending
            .lock_safe()
            .remove(&(group.to_string(), agent_id.to_string(), eaten))
            .unwrap_or_default();
        if ids.is_empty() {
            return;
        }
        if self.is_paused(group) {
            self.audit(group, brand::AUDIT_ACTOR, "notice-suppressed", json!({
                "kind": "unconfirmed-delivery", "to": agent_id, "delivery_ids": ids,
                "eaten": eaten, "reason": "group-paused",
            }));
            return;
        }
        // rev-13 N4: the record follows the attempt, and says whether it
        // landed. Auditing first and discarding the result asserts a notice
        // that a dead/unbound orchestrator never received — and coalescing
        // makes that one false record stand for every id in the batch at once.
        // `deliver_to_orchestrator`'s "no live orchestrator" branch audits
        // nothing of its own, so this is the only place it can be recorded.
        // Same shape as #569's `pause-suppression-notice`.
        // #585: the wording follows the bucket, so "was LOST, re-send it" can
        // only ever be said about ids actually read as eaten. The audit keeps
        // `eaten` too, so "how often is a delivery actually eaten?" stays
        // greppable apart from "how often is one merely unconfirmed?".
        let text = if eaten {
            delivery_eaten_notice(agent_id, &ids)
        } else {
            unconfirmed_delivery_notice(agent_id, &ids)
        };
        let outcome = self.deliver_to_orchestrator(group, &text, brand::AUDIT_ACTOR);
        self.audit(group, brand::AUDIT_ACTOR, "delivery-unconfirmed-notice", json!({
            "to": agent_id, "delivery_ids": ids, "coalesced": ids.len(), "eaten": eaten,
            "delivered": outcome.is_ok(), "error": outcome.as_ref().err(),
        }));
    }

    /// #112 round 2 — additive to the #445 seam (a NEW notice path;
    /// `should_notify_unconfirmed` and every `PasteGate::Abort` path are
    /// untouched by this function's existence). Tells the orchestrator a
    /// delivery it was already told was unconfirmed has since been proven
    /// to have landed — see `delivery_confirmed_late_notice`'s doc for why
    /// this is framed as a correction. Same suppression posture as
    /// `notify_unconfirmed_delivery`: never sent for an orchestrator-target
    /// delivery (a notice about a delivery to the orchestrator is itself a
    /// delivery to the orchestrator — the same loop that function's doc
    /// already names) and never sent to a paused group — both suppressions
    /// audited (#445), matching the routed #451 finding this issue owns:
    /// review found this correction notice left NO audit trace when
    /// suppressed, structurally the same silent-suppression class as this
    /// issue's core defect. Best-effort and audited; called at most once
    /// per delivery, from the late-confirmation monitor, only when a
    /// `failed` alarm had actually fired for this exact delivery.
    #[doc(hidden)] // pub for integration tests
    pub fn notify_delivery_confirmed_late(&self, group: &GroupId, agent_id: &str, target_is_orchestrator: bool) {
        if target_is_orchestrator {
            self.audit(group, brand::AUDIT_ACTOR, "notice-suppressed",
                json!({ "kind": "delivery-confirmed-late", "to": agent_id, "reason": "target-is-orchestrator" }));
            return;
        }
        if self.is_paused(group) {
            self.audit(group, brand::AUDIT_ACTOR, "notice-suppressed",
                json!({ "kind": "delivery-confirmed-late", "to": agent_id, "reason": "group-paused" }));
            return;
        }
        self.audit(group, brand::AUDIT_ACTOR, "delivery-confirmed-late-notice", json!({ "to": agent_id }));
        let _ = self.deliver_to_orchestrator(group, &delivery_confirmed_late_notice(agent_id), brand::AUDIT_ACTOR);
    }
}

/// #562 — what `QueueMap::mutate` runs once it has released the queue lock.
///
/// One line, and it is the entire coupling between "the queues changed" and
/// "the file changed". Every `queues` mutation in this file goes through
/// `mutate`, and `mutate` ends here; there is no second route to either
/// half. Deliberately a thin forward rather than the write itself, so
/// `persist_queues` stays the single place the lock order, the lazy
/// recovery and the best-effort failure policy are reasoned about.
impl queuestate::QueueSnapshotWriter for OrchRegistry {
    fn write_queue_snapshot(&self, group: &GroupId) {
        self.persist_queues(group);
    }
}
