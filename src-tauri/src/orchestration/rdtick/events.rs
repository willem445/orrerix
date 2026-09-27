//! A driven delegate's events and the panes a drive owns (§7): `rd_ingest` and
//! `rd_consume`, the delegate lookup `rd_owner`, the owned-pane reads
//! (`rd_driven_panes*`, `rd_surviving_panes`), the delta-brief check
//! `rd_lane_briefed_verify`, and the driver's audit line.
//!
//! Design note: `docs/design/review-driver.md`. Part of `rdtick/`, the driver's
//! registry wiring, which `tests/reviewdrive.rs` scans whole (see `mod.rs`).

use super::*;

impl OrchRegistry {
    /// Record a driven delegate's event for the next tick (§7).
    ///
    /// **Consumed, not dropped.** The MCP arm audits `rd-consumed` before
    /// calling this, so traffic that stopped arriving as an orchestrator prompt
    /// is still on the record and still attributable.
    ///
    /// **In memory, and that is a bounded choice rather than an oversight.** A
    /// `report` is an *event*; the durable facts a drive turns on — the head,
    /// the verdict files — are re-read from GitHub and from disk every tick. An
    /// orrerix restart between a worker's `report(done)` and the next tick
    /// therefore loses only the body-only-fix shortcut (arc 8): a push is still
    /// seen as a head move (arc 7), and a drive that learns nothing degrades to
    /// `held(fix-stalled)`, which is bounded and named. Persisting an event
    /// queue would buy that one arc at the cost of a second write path into the
    /// state file, from the MCP thread.
    #[doc(hidden)] // pub for integration tests
    pub fn rd_ingest(&self, group: &GroupId, pr: u64, event: RdEvent) {
        let mut map = self.rd_signals.lock_safe();
        let sig = map.entry((group.clone(), pr)).or_default();
        match event {
            RdEvent::WorkerDone => sig.worker = reviewdrive::WorkerSignal::Done,
            // `blocked` outranks a `done` seen in the same window: the two can
            // only both be present if the worker said one and then the other,
            // and `blocked` is the one that needs a human.
            RdEvent::WorkerBlocked => sig.worker = reviewdrive::WorkerSignal::Blocked,
            // Its own field, never `sig.worker` — see [`RdEvent::WorkerProgress`].
            RdEvent::WorkerProgress => sig.worker_progress = true,
            RdEvent::Messaged { by } => {
                sig.messaged = true;
                sig.messaged_by = by;
            }
            RdEvent::Verdict => {}
        }
    }

    /// **What a terminal notice owes a reader: the panes this drive still
    /// holds, founding ones included** (#3250, review round 3).
    ///
    /// [`reviewdrive::DriveEntry::owned_panes`] is what the exit notices named
    /// before, and for a drive that never handed back its worker side is EMPTY
    /// — so the pane the orchestrator handed the drive appeared in no clause at
    /// all. That is invisible while the release takes the pane (nothing is left
    /// to name) and wrong in the two cases where it does not: a founding pane
    /// that is BUSY on the exit tick, which the barrier refuses and no later
    /// tick re-asks, and the reconcile's own cancel, which takes no tick and
    /// releases nothing. Both left the conversation recoverable by nothing the
    /// notice said.
    ///
    /// **Liveness is the filter, and it is what keeps the clause honest.**
    /// `panes_clause` promises panes that are still RUNNING, for the
    /// orchestrator to resume or dispose of; a founding pane this very tick
    /// released is dead, and naming it would be the false claim that clause
    /// exists to avoid. `release_pane` drops a released OWNED pane from the
    /// record, which is how the same promise is kept on that side — a founding
    /// pane is not in the record to drop, so the liveness read does the same
    /// work here. Asked of the registry, which is why this sits on this side
    /// rather than in the engine.
    pub(super) fn rd_surviving_panes(
        &self,
        entry: &reviewdrive::DriveEntry,
    ) -> Vec<(String, reviewdrive::DrivenRole)> {
        let mut panes = entry.owned_panes();
        for a in &entry.founding_panes {
            if a.trim().is_empty() || panes.iter().any(|(id, _)| id == a) {
                continue;
            }
            if self.agent(a).is_some_and(|x| x.status != AgentStatus::Dead) {
                panes.push((a.clone(), reviewdrive::DrivenRole::Worker));
            }
        }
        panes
    }

    /// Which live drive, if any, this agent is a delegate of — §7's interception
    /// key, and the whole of it.
    ///
    /// **Keyed on the agent, never on text.** The id compared here is one
    /// orrerix minted at spawn and the driver recorded itself
    /// (`LaneRecord::agent`, `DriveEntry::worker_agent`, and the superseded
    /// panes beside each), and the caller's id comes from its MCP token rather
    /// than from `args`. So a delegate cannot choose whether its report reaches
    /// the orchestrator by naming a PR number, and cannot name someone else's to
    /// redirect theirs — which is the property §7 spends a paragraph on, because
    /// a delegate that could do either is a delegate that can route around the
    /// orchestrator.
    ///
    /// **Every pane the drive opened, not only the latest** (#1871 B2), and the
    /// answer says which: [`reviewdrive::DrivenPane::current`] is what keeps
    /// "this pane is mine" from being read as "take its word". See that type.
    ///
    /// **Only a LIVE drive owns anyone.** A `held` entry is parked: its
    /// delegates' traffic goes to the orchestrator exactly as it always did,
    /// which is what makes a hold a hand-back to a human rather than a quieter
    /// kind of drive. A terminal entry owns nobody for the same reason.
    ///
    /// Reads `review_drives.json` on every driven-or-not `report` and
    /// `review_verdict`. That is one small JSON read on a path that already
    /// writes a verdict file and delivers a pane prompt, and an absent file —
    /// the product default — costs a `stat` and answers `None`.
    pub fn rd_owner(
        &self,
        group: &GroupId,
        agent_id: &str,
    ) -> Option<(u64, reviewdrive::DrivenPane)> {
        let dir = self.group_dir(group);
        let _state_guard = self.rd_state_lock.lock_safe();
        let state = reviewdrive::load_state(&dir).ok()?;
        state
            .entries
            .iter()
            .filter(|e| e.state().is_live())
            .find_map(|e| e.driven_role(agent_id).map(|r| (e.pr, r)))
    }

    /// **Every pane a live drive CURRENTLY owns**, agent id -> (PR, side)
    /// (#2811 S2) — the roster-wide form of the question [`rd_owner`] answers
    /// for one caller.
    ///
    /// # One ownership read, not a second definition
    ///
    /// The class: three surfaces that had to know whether a pane belongs to a
    /// drive — the roster row, the kill refusal and the cap-refusal roster —
    /// and no shared answer for them to read, so the orchestrator killed a
    /// drive's idle worker 42 s before the drive needed it (#3038). The answer
    /// is derived HERE, through
    /// [`reviewdrive::DriveEntry::driven_role`] — the same predicate `rd_owner`
    /// uses, asked of the panes [`reviewdrive::DriveEntry::owned_panes`] names —
    /// so "the drive owns this pane" has one definition and a fourth consumer
    /// gets it by calling this rather than by re-reading the file its own way.
    ///
    /// **#2555 item 1 is NOT closed by this, and an earlier draft of this note
    /// said it was** (rev round 1, B1). That item asks that
    /// [`OrchRegistry::release_driven_pane`] itself refuse a pane the drive's
    /// records do not name — or that a `ReleaseTicket` only the driver can mint
    /// replace the one-call-site source scan. This builds the shared read that
    /// fix needs and consumes it on three OTHER surfaces; nothing on the release
    /// path reads it, and `release_driven_pane`'s refusals are exactly what they
    /// were. Wiring it there is a different change — the release runs UNDER
    /// `rd_state_lock` (see *Locking*), so it would take the already-held form
    /// plus a proof that it is always called that way, and it moves a signature
    /// two source scans in `tests/reviewdrive.rs` pin. #2555 keeps item 1.
    ///
    /// **`current` panes only, and that is a narrowing with a reason.** A
    /// superseded pane is one the drive will never speak to again
    /// ([`reviewdrive::DrivenPane::current`] is exactly that distinction): its
    /// traffic is still intercepted, which is what `rd_owner` is for, but the
    /// drive needs nothing further from it, and refusing an orchestrator's kill
    /// on it would hold a slot the drive has finished with — the opposite of the
    /// cost #3038 is about. So a superseded pane is killable and unmarked.
    ///
    /// That narrowing is pinned as the `current` filter below and NOT
    /// behaviourally, and the residual is stated rather than implied: no fixture
    /// in `tests/reviewdrive.rs` can reach a live superseded pane, because a
    /// hand-back REUSES a live idle pane on the same session
    /// ([`Self::rd_reuse_pane`]) and [`reviewdrive::DriveEntry::forget_dead_panes`]
    /// drops the ones that are not. What IS pinned is the other filter,
    /// `is_live` — `cancel_review_drive` makes the same pane killable and
    /// unmarked again, which is the refusal's own stated remedy working.
    ///
    /// # Locking — TWO entry points, because two callers need opposite failures
    ///
    /// Takes `rd_state_lock` and NOTHING else, and every caller must have
    /// released the `agents` lock before asking. The driver's own release path
    /// holds `rd_state_lock` and then reaches `agents` (`release_driven_pane`),
    /// so `agents` -> `rd_state_lock` is an inversion of an ordering that
    /// already exists in production. Callers therefore take this snapshot FIRST
    /// and read the roster afterwards; nothing here needs the two to overlap,
    /// because a pane that dies between the two reads is reported as driven and
    /// refused, which is the safe direction.
    ///
    /// **The hazard that actually fired is RE-ENTRANCY, not that inversion**,
    /// and it is why [`Self::rd_driven_panes_now`] exists. `rd_drive_group_with`
    /// holds `rd_state_lock` across its whole read-modify-write (`rdtick/tick.rs`'s
    /// `let _state_guard` before `load_state`) and performs its spawns INSIDE
    /// it — §2.4's ordering, deliberate — so the driver's own lane spawn and
    /// hand-back reach `spawn_agent_bound`, and any cap refusal formatted there
    /// would re-take a mutex this thread already holds. `lockwatch` refuses that
    /// rather than self-deadlocking, so the whole `reviewdrive` suite panicked
    /// `lock-reentrant` on the first CI run of this slice.
    ///
    /// So the two cap-refusal roster sites use the non-blocking sibling and the
    /// two guard sites use this one, and the split is by FAILURE DIRECTION:
    ///
    /// - **This one blocks**, because [`Self::list_agents`] and the MCP
    ///   `kill_agent` arm must never fail open. A guard that skipped its check
    ///   because a lock was momentarily busy would let exactly the kill this
    ///   slice exists to refuse through, under contention, silently. Neither
    ///   runs under the tick: nothing in `rd_drive_group_with` calls either.
    /// - **[`Self::rd_driven_panes_now`] does not**, because its output is a
    ///   MESSAGE decoration and its `None` case is almost exactly the driver's
    ///   own spawn — for which the marker is redundant, since a cap refusal the
    ///   DRIVER gets becomes `held(cap-refused)` / `cap-full`, whose notice
    ///   already names this drive's own panes. Losing the marker there costs the
    ///   pre-#2811-S2 sentence; it can never produce a wrong one.
    ///
    /// One small JSON read per call, on the same file [`rd_owner`] reads on
    /// every `report` — and an absent `review_drives.json` (the product
    /// default, and every group with no driver at all) costs a `stat` and
    /// answers empty.
    ///
    /// # Two costs of that choice, disclosed rather than argued away
    ///
    /// Both were named in review round 1's premortem, and neither is closable by
    /// a test:
    ///
    /// - **The snapshot is not held across the caller's decision.** The MCP
    ///   `kill_agent` arm reads this and then calls `kill_agent`, so a pane that
    ///   BECOMES driven in between — the tick's `rd_reuse_pane` claiming an idle
    ///   pane concurrently — is killed with no refusal. That is the reverse of
    ///   the direction the paragraph above promises, it is sub-millisecond, and
    ///   `force` is the honest override for it. Closing it would mean holding
    ///   `rd_state_lock` across the kill, which is `release_driven_pane`'s own
    ///   `rd_state_lock` -> `agents` edge taken from the other side.
    /// - **The blocking form makes the orchestrator's most frequent read wait on
    ///   the tick.** `list_agents` now blocks on a lock a drive tick holds across
    ///   its whole read-modify-write, spawns included (§2.4) — worktree creation
    ///   among them. Nothing here measures that hold, and if it ever grows to
    ///   seconds the roster read stalls with it. The discipline is still the
    ///   right one — a guard may not fail open — and this is its price, said
    ///   out loud rather than discovered.
    pub(crate) fn rd_driven_panes(
        &self,
        group: &GroupId,
    ) -> Result<std::collections::BTreeMap<String, (u64, reviewdrive::DrivenRole)>, ()> {
        let _state_guard = self.rd_state_lock.lock_safe();
        self.rd_driven_panes_locked(group)
    }

    /// [`Self::rd_driven_panes`] for a caller that may already be holding
    /// `rd_state_lock` — the two cap-refusal roster sites (#2811 S2).
    ///
    /// **Empty when the lock is not free RIGHT NOW**, which is the driver's own
    /// tick nine times in ten (`try_lock_safe` answers `None` for a mutex this
    /// thread already holds, without the `lock-reentrant` refusal a blocking
    /// acquire would take) and another group's tick the rest — **and empty when
    /// the record is unreadable**, which the guards refuse on and this does not.
    /// All three of those fail toward an UNMARKED roster — the message this repo shipped
    /// before S2 — never toward a row falsely marked driven, and never toward a
    /// kill going through: no guard reads this. The argument for why the driver
    /// does not need the marker is on [`Self::rd_driven_panes`], under
    /// *Locking*.
    pub(crate) fn rd_driven_panes_now(
        &self,
        group: &GroupId,
    ) -> std::collections::BTreeMap<String, (u64, reviewdrive::DrivenRole)> {
        let Some(_state_guard) = self.rd_state_lock.try_lock_safe() else {
            return std::collections::BTreeMap::new();
        };
        self.rd_driven_panes_locked(group).unwrap_or_default()
    }

    /// The read itself, with `rd_state_lock` already held by the caller — ONE
    /// body, so the three acquisition disciplines above cannot become three
    /// answers.
    ///
    /// **`Err` is "orrerix could not read this group's drive record", and it is
    /// a different fact from `Ok` of an empty map** (rev round 1, N1). A
    /// `review_drives.json` that is absent is the product default and genuinely
    /// means no pane is driven; one that is present and unparseable — the
    /// downgrade case, where a newer build wrote a schema this one reads as
    /// `Unsupported` — means orrerix does not KNOW, and this repo's settled
    /// posture for that fact is to refuse rather than to read it as "undriven"
    /// (the `Err` is `()` because there is nothing for a caller to branch on —
    /// "could not read" is the whole of it, and the caller's answer is a refusal
    /// either way):
    /// `queue_merge` answers rd-state-unreadable on the same input, pinned by
    /// `a_torn_drive_record_refuses_the_enqueue_instead_of_reading_as_undriven`,
    /// and §2.4 says the tick refuses too. Collapsing the two here would have
    /// made the guard admit exactly the kill it exists to refuse, on the one
    /// input where nothing else can tell.
    fn rd_driven_panes_locked(
        &self,
        group: &GroupId,
    ) -> Result<std::collections::BTreeMap<String, (u64, reviewdrive::DrivenRole)>, ()> {
        let dir = self.group_dir(group);
        let mut out = std::collections::BTreeMap::new();
        let Ok(state) = reviewdrive::load_state(&dir) else { return Err(()) };
        for e in state.entries.iter().filter(|e| e.state().is_live()) {
            for (agent, _) in e.owned_panes() {
                match e.driven_role(&agent) {
                    Some(p) if p.current => {
                        out.insert(agent, (e.pr, p.role));
                    }
                    _ => {}
                }
            }
        }
        Ok(out)
    }

    /// **Was this lane's outstanding brief a body-verification delta at exactly
    /// this revision?** (#2168 E2.) `review_verdict` asks before it writes, and
    /// the answer becomes
    /// [`workflow::ReviewVerdict::verified_body`](crate::orchestration::workflow::ReviewVerdict::verified_body).
    ///
    /// **A capability grant, so every comparison here is exact and every
    /// unknown is a no.** `lane_open_for`'s digest rule is deliberately
    /// unknown-TOLERANT — one transient `gh` failure to read a PR body must not
    /// re-brief every open lane — and that tolerance is right for "is this lane
    /// still open" and wrong for "may this verdict discharge the gate's
    /// `body-unchanged` clause for the lanes it supersedes". So the brief's
    /// recorded head and digest must be non-empty and must equal the ones the
    /// verdict is being bound to; anything else grants nothing, and the verdict
    /// is written exactly as it would have been before this existed.
    ///
    /// Only a LIVE drive grants: a held or terminal entry is not driving this
    /// lane, and its old brief is not a standing instruction. Same read as
    /// [`rd_owner`](Self::rd_owner), and the same reason.
    pub fn rd_lane_briefed_verify(
        &self,
        group: &GroupId,
        pr: u64,
        block: &str,
        head: &str,
        body_digest: &str,
    ) -> bool {
        if head.is_empty() || body_digest.is_empty() {
            return false;
        }
        let dir = self.group_dir(group);
        let _state_guard = self.rd_state_lock.lock_safe();
        let Ok(state) = reviewdrive::load_state(&dir) else { return false };
        state
            .entry(pr)
            .filter(|e| e.state().is_live())
            .and_then(|e| e.lane(block))
            .is_some_and(|rec| {
                rec.briefed_verify
                    && rec.briefed_head == head
                    && rec.briefed_digest == body_digest
            })
    }

    /// Record that a driven delegate's event was **consumed** by the driver
    /// rather than delivered to the orchestrator (§7).
    ///
    /// **Nothing is silent.** Every consumed event is audited with its kind, the
    /// agent and the PR, so traffic that stopped arriving as a prompt is still
    /// on the record and still attributable. "Consumed" is a different word from
    /// "dropped" and §5.4's vocabulary keeps them different.
    ///
    /// `event` is `None` for traffic that is consumed but carries no signal at
    /// all — a LANE's `report`, whose word to the drive is its verdict file, and
    /// any report from a superseded pane.
    ///
    /// A current WORKER's `report(progress)` used to be in that set and is not
    /// any more (#1959): it still moves nothing — a drive advances on the head,
    /// the checks and the verdict files, not on a delegate saying it is still
    /// going — but it arrives as [`RdEvent::WorkerProgress`], on a field
    /// `reviewdrive::decide` cannot read, so the tick can answer it in the
    /// worker's own pane instead of waiting out `fix-stalled`.
    pub fn rd_consume(
        &self,
        group: &GroupId,
        pr: u64,
        agent: &str,
        kind: &str,
        event: Option<RdEvent>,
    ) {
        let on_behalf = {
            let dir = self.group_dir(group);
            let _state_guard = self.rd_state_lock.lock_safe();
            reviewdrive::load_state(&dir)
                .ok()
                .and_then(|s| s.entry(pr).map(|e| e.on_behalf_of.clone()))
                .unwrap_or_default()
        };
        self.rd_audit(
            group,
            &on_behalf,
            rddrive::audit_action::CONSUMED,
            json!({ "pr": pr, "agent": agent, "kind": kind }),
        );
        if let Some(e) = event {
            self.rd_ingest(group, pr, e);
        }
    }

    /// This drive's pending delegate signals, **without clearing them**.
    ///
    /// Cleared only once an arc has been taken, because a tick can decline to
    /// act for reasons that have nothing to do with the signal — an unresolved
    /// head, a runner failure — and a signal consumed by a tick that then did
    /// nothing is a hand-back the drive never learns about.
    pub(super) fn rd_signal(&self, group: &GroupId, pr: u64) -> RdSignal {
        self.rd_signals.lock_safe().get(&(group.clone(), pr)).cloned().unwrap_or_default()
    }

    /// One audit line in the driver's vocabulary, carrying §3's `on_behalf_of`.
    ///
    /// The **actor** stays `brand::AUDIT_ACTOR`, so it is this detail key — not
    /// the actor — that distinguishes a driver action from any other host
    /// action, and it is what an audit reader filters on.
    pub(super) fn rd_audit(&self, group: &GroupId, on_behalf_of: &str, action: &str, mut detail: Value) {
        if let Some(obj) = detail.as_object_mut() {
            obj.insert(rddrive::ON_BEHALF_OF.to_string(), Value::from(on_behalf_of));
        }
        self.audit(group, brand::AUDIT_ACTOR, action, detail);
    }
}
