//! Opening a reviewer lane: `rd_open_lane`, the session a lane resumes on, the
//! live pane already holding a round, and reuse before spawn (#1960).
//!
//! Design note: `docs/design/review-driver.md`. Part of `rdtick/`, the driver's
//! registry wiring, which `tests/reviewdrive/guards.rs` scans whole (see `mod.rs`).

use super::*;

impl OrchRegistry {
    /// Brief one reviewer lane — a fresh spawn by block id, or a resume of the
    /// session this lane already has (§4: the driver does make the
    /// spawn-versus-reuse choice *within* a lane it was already told to run).
    ///
    /// **A resume that will not resolve falls back to a fresh spawn by block
    /// id**, which is §8's reaped-reviewer row: the entry stores the full
    /// resolved session so the next round resumes it, and a lane whose session
    /// no longer resolves respawns fresh. That respawn does **not** consume a
    /// `review_rounds` increment — that counter counts rounds of *findings*, and
    /// a reaped reviewer produced none.
    pub(super) fn rd_open_lane(
        &self,
        group: &GroupId,
        entry: &mut reviewdrive::DriveEntry,
        block: &str,
        brief: &RdBrief,
        limits: &reviewdrive::DriveLimits,
        now: u64,
        verify: bool,
        body_only: bool,
    ) -> Result<RdLaneOpen, String> {
        if block.is_empty() {
            return Err("no lane at that index".into());
        }
        // #2508: the scope is derived ONCE, here, at the same pre-record point
        // the brief renders from, and threaded both into the render and out on
        // [`RdLaneOpen`] for the `rd-lane-spawned` audit row — two
        // derivations would read the same state today and could drift the day
        // one of them moves after `open_lane` writes.
        let scope = rd_lane_scope(entry, block, brief, verify);
        let text = self.rd_lane_brief(entry, block, brief, limits, verify, &scope);
        // **The lane's session is the pane's session, and the record is only
        // one of the two places it is written** (#2109).
        //
        // `LaneRecord::session` is whatever the spawn RETURNED, and
        // `spawn_agent_bound` returns one only for `cli == "claude"`, which
        // pre-assigns a uuid. Copilot and opencode mint theirs after boot: the
        // session watcher discovers it and binds it to the pane and the roster
        // through `associate_session`, and nothing has ever written it back
        // onto the lane. So for every lane on those CLIs the recorded session
        // was permanently empty, this filter dropped it, and EVERY round opened
        // a fresh pane on a fresh conversation — measured on the dogfood as nine
        // reviewer panes for three PRs where six would have done, each new pane
        // briefed with what "its" previous verdict had been by a pane that had
        // never seen it.
        //
        // The pane the lane recorded answers the question the record could not,
        // and it is the same fact rather than a second one: the roster row for
        // that pane carries the session, the block and the cwd — the three
        // `spawn_agent(resume_session:)` needs.
        let prior = self.rd_lane_session(group, entry.lane(block));
        // #2163: the `lane-stalled` anchor this brief must carry. A brief that
        // is only REPLACING a dead pane at the same REVISION — the full
        // `(head, digest)` key, which is why the digest is passed — keeps the
        // anchor the original brief set; see `lane_stall_anchor` for why that
        // is both the honest reading of a silence clock and the thing that
        // bounds a pane which dies on every spawn. Computed here, before
        // `open_lane` replaces the record it reads.
        let pane_dead =
            entry.lane(block).is_some_and(|rec| self.rd_dead_lane_pane(rec).is_some());
        let anchor = reviewdrive::lane_stall_anchor(
            entry.lane(block),
            &brief.head,
            brief.body_digest_opt(),
            pane_dead,
            now,
        );
        // **The resume names the lane's own block** (#1961). A resume that
        // named neither `kind` nor `block` reached `spawn_agent_bound`, which
        // has no session-inheritance rule of its own and falls straight through
        // to `block_for(role)` — the roster's DEFAULT reviewer block. So a
        // `rev-final` lane came back as `rev-std` on its second round: wrong
        // persona, wrong model, and on a CLI that may not be able to open the
        // transcript at all. This lane's block is not something to look up — it
        // is the key the lane is filed under.
        // Reuse before spawn (#1960), the lane's half: a lane briefed a second
        // time whose reviewer is idle in a live pane is re-briefed IN that pane
        // rather than in a second one on the same conversation. Five of the six
        // panes holding the cap in the measured incident were idle lanes and
        // superseded workers the driver itself had opened.
        let fresh = |this: &Self| {
            this.rd_spawn(group, Role::Reviewer, Some(block.to_string()), None, &text)
        };
        // #2089: cloned out of `entry` because the reuse below borrows `self`
        // while `entry` is held `&mut` — the shape `rd_reconcile_with` and
        // `rd_step_entry` already use before their own `rd_audit` calls.
        let on_behalf = entry.on_behalf_of.clone();
        // **No second pane for a block that already has a live one at this
        // head** (#2109) — checked only where a spawn is about to happen, so
        // the reuse arm above keeps its chance first.
        //
        // A re-brief at an UNCHANGED head is §8's body-changed row: the digest
        // moved, the lane must be asked again, and where its pane is idle and
        // ready `rd_reuse_pane` types the delta into it and this never fires.
        // Where that pane is BUSY — still writing the review it was briefed for
        // — the reuse declines on readiness and the spawn used to mint a second
        // pane on the same conversation. Measured: `rev-1825` and `rev-1826`
        // both reviewed PR #2104's round 2, and `rev-1832` reported that "a
        // duplicate rev-std round-2 review from a parallel pane landed 41
        // seconds after mine with the same verdict and lead finding". Two paid
        // reviews for one verdict slot, and two panes against the cap.
        //
        // The refusal is an `Err`, which `OpenLane`'s arm already handles as a
        // back-off: the tick retries, and the moment that pane goes idle the
        // reuse arm delivers the delta into it.
        //
        // **What bounds the retry depends on whether that lane has ANSWERED at
        // this head**, and the two cases are genuinely different (#2109 review
        // 2 and review 4; §8's duplicate-refusal row states the same split).
        //
        // A lane that has NOT answered — the common case, a pane still writing
        // the review it was briefed for — is bounded by the clock this refusal
        // does not re-arm: `spawned_ms` stays where the original brief put it,
        // so a pane that never comes back is `held(lane-stalled)` naming it,
        // exactly as if it had gone quiet without a re-brief. That bound is only
        // REACHABLE because `decide_review_wait`'s stall check is keyed on the
        // head rather than on the full (head, digest) key; the two changes ship
        // together for that reason, and refusing without the re-key would swap a
        // duplicate reviewer for a hold hours away that names no lane.
        //
        // A lane that HAS answered here and whose pane then went busy on
        // something else — a human re-tasking it, another drive taking it over —
        // is the residual: the stall arm exempts it precisely because it
        // answered. An earlier version of this comment claimed that case could
        // not arise ("a pane still holding the round has by definition recorded
        // no verdict for it"), and that premise is false — answering and then
        // being given other work are not exclusive.
        //
        // **What bounds it moved in #2110, and only what bounds it.** That
        // issue built no per-lane refusal clock, so there is still nothing
        // per-lane here and the hold still names no lane; what it added is a
        // per-STATE bound, and `review-wait` is a state, so this composition
        // now leaves at its `review-wait` state bound as `held(state-stalled)`
        // rather than at twelve hours as `held(drive-stalled)` — four hours on a
        // one-lane gate at stock knobs, since #2117 review 2 made that bound the
        // constant PLUS one `lane_timeout_minutes` per required lane.
        // `an_answered_lane_whose_re_brief_is_refused_is_bounded_by_the_review_wait_state_bound`
        // pins the gap and that exit; closing it properly still wants the
        // per-lane clock.
        //
        // **#2162 narrowed this refusal and left that residual where it is.** A
        // pane the reuse arm DECLINED is idle by construction, and refusing a
        // replacement for it deadlocked the drive — so `rd_live_lane_pane` now
        // refuses only a pane that is still WORKING. The residual above is the
        // busy case, which is exactly what this refusal still covers.
        //
        // **A head change is deliberately not covered.** There the recorded pane
        // is reviewing a revision the drive has moved past, its verdict binds to
        // a head that is gone, and superseding it is what `prior_agents` exists
        // for.
        let dup = self.rd_live_lane_pane(entry.lane(block), &brief.head);
        let pr = entry.pr;
        let no_duplicate = |this: &Self| -> Result<(), String> {
            let Some(pane) = dup.as_deref() else { return Ok(()) };
            this.rd_audit(group, &on_behalf, rddrive::audit_action::LANE_DUPLICATE_REFUSED, json!({
                "pr": pr,
                "block": block,
                "head": brief.head,
                "pane": pane,
            }));
            Err(format!(
                "lane {block} already has a live pane ({pane}) working on this head; \
                 refusing to open a second reviewer for the same round"
            ))
        };
        // **`resumed` records what HAPPENED, not what was attempted.** A resume
        // that fell through to a fresh pane is a lane that lost its
        // conversation, and filing it as a resume would make the audit row say
        // the opposite of the thing `rd-lane-resume-failed` was added to
        // surface (#2109).
        let (agent, session_id, resumed) = match prior {
            Some(session) => match self.rd_reuse_pane(group, &on_behalf, &session, block, &text) {
                Some(a) => (a, session, true),
                None => {
                    no_duplicate(self)?;
                    let (sp, was_resume) = match self.rd_spawn(
                        group,
                        Role::Reviewer,
                        Some(block.to_string()),
                        Some(session.clone()),
                        &text,
                    ) {
                        Ok(sp) => (sp, true),
                        Err(why) => {
                            // **A fall-through to fresh is audited, never
                            // silent** (#2109), on `rd-reuse-declined`'s own
                            // argument one arm over: the only other visible
                            // effect of a failed resume is a fresh pane, and a
                            // fresh pane is what "this lane had no session to
                            // resume" looks like too. A reader chasing a
                            // reviewer that lost its conversation could not tell
                            // the two apart on this log.
                            //
                            // A cap refusal is NOT one of these. It refused the
                            // slot, not the session, and a fresh spawn would be
                            // refused identically — so it propagates, where
                            // `OpenLane` backs off and `cap_starved_since_ms`
                            // starts counting toward `held(cap-full)`. Falling
                            // through to `fresh` here would spend the retry on
                            // a second refusal and file it as a resume failure.
                            if super::is_live_cap_refusal(&why) {
                                return Err(why);
                            }
                            self.rd_audit(group, &on_behalf, rddrive::audit_action::LANE_RESUME_FAILED, json!({
                                "pr": pr,
                                "block": block,
                                "session": session,
                                "head": brief.head,
                                "detail": why,
                            }));
                            (fresh(self)?, false)
                        }
                    };
                    (sp.id, sp.session_id.unwrap_or_default(), was_resume)
                }
            },
            None => {
                no_duplicate(self)?;
                let sp = fresh(self)?;
                (sp.id, sp.session_id.unwrap_or_default(), false)
            }
        };
        entry.open_lane(
            block,
            &session_id,
            &agent,
            &brief.head,
            brief.body_digest_opt(),
            anchor,
            verify,
            // #2509. Carried from the step, never re-derived here — see
            // `LaneRecord::briefed_body_only`, and `briefed_verify` one
            // argument up for why a grant is derived exactly once.
            body_only,
        );
        Ok(RdLaneOpen { agent, session: session_id, resumed, scope })
    }

    /// The session a lane must be RESUMED on, or `None` when this build cannot
    /// establish one (#2109).
    ///
    /// **Two sources for one fact, in the order of how directly they observed
    /// it.** [`reviewdrive::LaneRecord::session`] is what the spawn returned,
    /// and `spawn_agent_bound` returns a session id only for `cli == "claude"`
    /// — it pre-assigns a uuid there, while copilot and opencode mint theirs
    /// after boot. For those the field is empty at `open_lane` time and nothing
    /// has ever written it back, so it is empty for the life of the lane: the
    /// session watcher's discovery lands on the PANE (`associate_session`
    /// updates the agent map and the roster row) and never on the drive's own
    /// record. Reading the record alone therefore answered "this lane has no
    /// session" for every non-claude reviewer, and `rd_open_lane` opened a
    /// fresh pane on a fresh conversation every round — #2109's first ask, and
    /// the reason nine reviewer panes were paid for where six were needed.
    ///
    /// The live agent map is asked before the roster because it is the one the
    /// watcher writes first; the roster is what survives a pane that has since
    /// exited, which is exactly the lane a resume is FOR.
    ///
    /// **A blank is never a value.** An empty or whitespace-only id is dropped
    /// at every step rather than passed to `rd_spawn`, where
    /// `sanitize_session` would refuse it and the refusal would be filed as a
    /// resume failure — a claim about a session that was never recorded.
    pub(super) fn rd_lane_session(
        &self,
        group: &GroupId,
        lane: Option<&reviewdrive::LaneRecord>,
    ) -> Option<String> {
        let lane = lane?;
        let keep = |s: String| Some(s).filter(|s| !s.trim().is_empty());
        if let Some(s) = keep(lane.session.clone()) {
            return Some(s);
        }
        if lane.agent.trim().is_empty() {
            return None;
        }
        if let Some(s) = self.agent(&lane.agent).and_then(|a| a.session_id).and_then(keep) {
            return Some(s);
        }
        self.merged_records(group)
            .into_iter()
            .find(|r| r.id == lane.agent)
            .and_then(|r| r.session)
            .and_then(keep)
    }

    /// The pane that already holds this lane's round, when opening another
    /// would be a DUPLICATE (#2109) — this lane's recorded pane, positively
    /// live, and briefed at the head about to be briefed again.
    ///
    /// **All three conditions, and the middle one fails toward spawning.** A
    /// pane this registry has no record of is "we could not check", not "it is
    /// alive" — the same asymmetry [`reviewdrive::DriveEntry::forget_dead_panes`]
    /// states — but the safe direction is the opposite one here. Refusing on an
    /// unknown pane would wedge every drive in the group after a restart, whose
    /// agent map is empty and whose panes are genuinely gone; spawning on one
    /// costs at worst the duplicate this exists to prevent, in the one case
    /// where orrerix cannot see the pane at all. So only `Some(non-Dead)`
    /// refuses.
    ///
    /// The head condition is what keeps this from blocking a legitimate
    /// supersede: after a push the recorded pane is reviewing a revision the
    /// drive has moved past, and opening its successor is what
    /// [`reviewdrive::LaneRecord::prior_agents`] exists for.
    ///
    /// **And live is not enough: the pane must be WORKING** (#2162). #2109's own
    /// subject is "a pane still writing the review it was briefed for", and
    /// `idle_since_ms` is the registry's answer to exactly that — the same half
    /// of `idle_pane_on_session`'s conjunction, which asks "does this agent have
    /// work". An IDLE pane has none: it took the brief, finished its turn and
    /// reported, and there is no second review for a second pane to duplicate.
    ///
    /// Without this the guard composed with `rd_reuse_pane`'s readiness decline
    /// into a hard deadlock, and the two are about ONE pane. Reuse only ever
    /// declines a pane that is idle (`idle_pane_on_session` filters on
    /// `idle_since_ms` before the readiness test, so an `rd-reuse-declined` row
    /// PROVES the pane was idle), and this refusal then named that same pane as
    /// live and briefed at this head — which it was, because a body-only fix
    /// cannot move the head. Too unconfirmed to reuse and too live to replace:
    /// measured on PR #2140 as 38 minutes with no lane open, the same three rows
    /// every tick, ended by a human killing the pane. Since a body-only fix is
    /// #1875's whole class, that was the common case rather than a corner.
    ///
    /// **The two conditions do not overlap, which is why this costs #2109
    /// nothing.** The reuse arm considers only idle panes; this refusal now
    /// protects only busy ones — and busy is what the measured duplicate was
    /// (`rev-1825` and `rev-1826` both reviewing PR #2104's round 2, the second
    /// spawned while the first was still writing). An idle pane that is
    /// delivery-READY never reaches here at all: the reuse arm types the delta
    /// into it and `rd_open_lane` returns before this is asked.
    ///
    /// **What is left over is unchanged**: a lane whose pane went busy on
    /// something else — a human re-tasking it, another drive taking it over —
    /// is still refused, still with no per-lane clock, and still bounded by
    /// `review-wait`'s state bound where it has answered and by `lane-stalled`
    /// where it has not.
    fn rd_live_lane_pane(
        &self,
        lane: Option<&reviewdrive::LaneRecord>,
        head: &str,
    ) -> Option<String> {
        let lane = lane?;
        if lane.agent.trim().is_empty() || head.is_empty() || lane.briefed_head != head {
            return None;
        }
        match self.agent(&lane.agent) {
            Some(a) if a.status != AgentStatus::Dead && a.idle_since_ms.is_none() => {
                Some(lane.agent.clone())
            }
            _ => None,
        }
    }

    /// **Reuse before spawn** (#1960): resume `session` into the live idle pane
    /// already running it **under `block`** and **ready to be typed into**
    /// (#2089), by typing the brief into that pane, and answer which pane took
    /// it. `None` = there is none, so the caller opens one — which includes the
    /// case where the only live idle pane on this session is running the WRONG
    /// block, and reusing it would be #1961 one arm over.
    /// `idle_pane_on_session` carries both arguments and the inputs that produce
    /// such a pane.
    ///
    /// **A candidate refused on readiness is audited, not dropped** (#2089).
    /// `rd-reuse-declined` (§5.4) names the pane and why, because the only other
    /// visible effect of the refusal is a fresh pane, and a fresh pane is what
    /// "there was no candidate at all" looks like too.
    ///
    /// # Why this rather than releasing what a new pane supersedes
    ///
    /// The driver opened a NEW pane per resume and closed none, so every round
    /// cost net +1 or +2 live panes and three concurrent drives exhausted the
    /// six-delegate cap in a round and a half — on the driver's own panes, five
    /// of the six idle. §3.1's other candidate fix was to kill the pane a new
    /// one supersedes, and this is chosen over it for two reasons.
    ///
    /// It does not need §3.1 item 5 narrowed the way that candidate would have —
    /// into "never kills a pane it did not open", which is a guarantee a reader
    /// has to hold a second fact to evaluate — and the exception would have been
    /// useless for the pane that actually squatted the cap in the measured
    /// incident: the ORIGINAL worker pane, opened by the orchestrator, which a
    /// release-what-you-superseded rule may not touch. Reuse takes that pane over
    /// on the first hand-back and never creates the second one at all.
    ///
    /// **#2501 did narrow item 5, and this argument is why it narrowed it on a
    /// different axis.** The release rule is keyed on the pane's STATE in the
    /// drive — a lane whose verdict is recorded at this head, a worker whose
    /// report the drive consumed — never on who opened it, so it reaches that
    /// same original worker pane and a reader evaluates it from the drive's own
    /// record. Reuse and release are not alternatives now: reuse is what happens
    /// while a pane is still needed, release is what happens when it is not, and
    /// a released pane's session is what the next round resumes into.
    ///
    /// And it is what an orchestrator driving by hand does: it types the next
    /// instruction into the worker's pane. A drive whose worker is idle in a
    /// live pane has nothing to gain from a second pane on the same
    /// conversation, and #338/#359 makes the second one actively worse — two
    /// panes, one session, one worktree.
    ///
    /// A failed delivery falls through to the spawn rather than failing the
    /// hand-back: `deliver_prompt` can refuse for reasons that say nothing
    /// about the drive (a pane that died between the lookup and the write),
    /// and the spawn is the path that already existed.
    pub(in crate::orchestration) fn rd_reuse_pane(
        &self,
        group: &GroupId,
        on_behalf_of: &str,
        session: &str,
        block: &str,
        text: &str,
    ) -> Option<String> {
        let found = self.idle_pane_on_session(group, session, block);
        for (pane, why) in &found.declined {
            self.rd_audit(group, on_behalf_of, "rd-reuse-declined", json!({
                "pane": pane,
                "session": session,
                "block": block,
                "reason": why.as_str(),
            }));
        }
        let agent = found.agent?;
        self.deliver_prompt(&agent, text, brand::AUDIT_ACTOR, Delivery::MidSession).ok()?;
        Some(agent)
    }
}
