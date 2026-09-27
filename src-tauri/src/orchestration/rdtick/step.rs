//! `rd_step_entry`: one entry, one tick, at most one advance (§2.4).
//!
//! Design note: `docs/design/review-driver.md`. Part of `rdtick/`, the driver's
//! registry wiring, which `tests/reviewdrive.rs` scans whole (see `mod.rs`).

use super::*;

impl OrchRegistry {
    /// One entry, one tick, **at most one advance** (§2.4).
    ///
    /// Runs with `rd_state_lock` held, and that includes the spawn — §2.4 says
    /// so in as many words ("the load-decide-store spans a spawn, and a
    /// `drive_review` landing inside that window would otherwise read the
    /// pre-spawn file and write it back, erasing the entry"). It is safe to span
    /// one here and not a *notice*: no site that takes `rd_state_lock` is
    /// reachable from a pane delivery, so a spawn's own kickoff cannot cycle
    /// back onto it. The orchestrator notices this produces are still delivered
    /// by the caller, outside the lock, for the #467/#468 reason. The full site
    /// list, and why the two interception helpers do not break it, is on
    /// [`Registry::rd_drive_group_with`].
    fn rd_step_entry(
        &self,
        group: &GroupId,
        runner: &dyn rddrive::RdRunner,
        state: &mut reviewdrive::ReviewDrivesState,
        pr: u64,
        limits: &reviewdrive::DriveLimits,
        now: u64,
        base_green_memo: &mut std::collections::HashMap<String, Option<bool>>,
    ) -> Option<RdOut> {
        let resting =
            state.entry(pr).map(|e| e.state().is_parked() || e.state().is_terminal())?;
        if resting {
            // §2.1's `held` row: "nothing; the tick does not advance it". Bailing
            // BEFORE the reads rather than after — `decide` would answer `Wait`
            // anyway, but a parked drive can sit here for days, and spending
            // `gh` round-trips per parked entry per tick to be told so is the
            // cost §2.4's one-group bound exists to keep down.
            return None;
        }
        let obs = rddrive::observe_pr(runner, pr);
        // **Only the states that READ these facts pay for them.** `decide` reads
        // `required_lanes` in `review-wait` and `gate-check` and `gate` in
        // `gate-check` alone; `ci-wait` and `fix-wait` read neither. Resolving
        // them unconditionally would spend a `pr view --json files` on every
        // routing gate and a pair of `gh` reads on every `base-green` gate, per
        // entry, per tick, for answers nothing consults — on the loop that also
        // delivers every `notify_when` notice in the fleet (§2.4). The queue's
        // own driver gates the same two reads on the same principle
        // (`declares_base_green`: a value nothing consults is not worth a round
        // trip, and an unfetched value is `None`, which refuses).
        let here = state.entry(pr).map(|e| e.state())?;
        let want_lanes = matches!(
            here,
            reviewdrive::DriveState::ReviewWait | reviewdrive::DriveState::GateCheck
        );
        let want_gate = here == reviewdrive::DriveState::GateCheck;
        let (gate, required, lane_notices) = if want_lanes {
            self.rd_gate_facts(group, runner, pr, &obs, want_gate, base_green_memo)
        } else {
            // `NotEvaluated` is the honest value here and not a stand-in for
            // "satisfied": §2.1's `gate-check` row treats it as "the tick reached
            // this state without evaluating the gate", which is `Wait`. Neither
            // of the two states that land here can read it at all.
            (reviewdrive::GateOutcome::NotEvaluated, None, Vec::new())
        };
        // **The lane-side twin of `worker_exit` below** (#2163). A pane exit was
        // observed only for the worker and only in `fix-wait`, on the argument
        // that "`review-wait` has `lane-stalled` for its own panes" — which is
        // true and is an HOUR away, measured from the brief rather than from the
        // death. A reviewer pane killed twelve minutes into its round left the
        // drive silent for forty-eight more with no rd-* row at all.
        //
        // Read here rather than in `rd_gate_facts` because it is a fact about
        // the ENTRY (which pane this lane recorded) and the agent map, and that
        // function is given neither on purpose. `_` on a lane the entry has no
        // record for: there is no pane to be dead.
        //
        // **Filled for EVERY required lane, though `decide_review_wait` consults
        // only `first_stale_lane`'s `k`** (#2169 review 2, premortem 1). That is
        // deliberate rather than an oversight: `LaneFact` is a per-lane reading
        // of the world, and one whose fields were populated only for whichever
        // lane happened to be selected would be a struct whose meaning depends
        // on its index — so a later change that reads another lane's entry
        // would silently read a `false` nobody wrote. The cost is one agent-map
        // lookup per required lane per tick. Lanes open strictly sequentially
        // today, so nothing else can reach a non-`k` lane; this keeps that a
        // property of the DECISION rather than of the facts it is handed.
        let required = required.map(|mut lanes| {
            for l in lanes.iter_mut() {
                l.pane_dead = state
                    .entry(pr)
                    .and_then(|e| e.lane(&l.block))
                    .is_some_and(|rec| self.rd_dead_lane_pane(rec).is_some());
            }
            lanes
        });
        let signal = self.rd_signal(group, pr);
        let messaged_by = signal.messaged_by.clone();
        // **The driver watches the pane it resumed** (#1961), and only where
        // the answer can mean anything: in `fix-wait`, on the CURRENT worker
        // pane, and only while that pane has said nothing.
        //
        // A `done` or a `blocked` already in hand outranks it — a worker that
        // reported and then exited is a worker that finished, and reading its
        // exit as a failure would throw away the arc its own report earned.
        // Nor is this asked in any other state: a worker pane exiting outside a
        // hand-back is not this drive's business.
        //
        // **This used to add "`review-wait` has `lane-stalled` for its own
        // panes", and that was the whole of #2163.** It is true and it is an
        // HOUR away, anchored at the brief rather than at the death, so a
        // reviewer pane killed twelve minutes in cost forty-eight more of
        // silence. `LaneFact::pane_dead` above is the lane-side observation
        // that answers it on the next tick instead.
        let worker_exit = (here == reviewdrive::DriveState::FixWait
            && signal.worker == reviewdrive::WorkerSignal::Silent)
            .then(|| state.entry(pr).map(|e| e.worker_agent.clone()).unwrap_or_default())
            .filter(|a| !a.is_empty())
            .and_then(|a| self.rd_pane_exit(&a));
        // **The restart marks, read ONCE** (#3196 review 2, #3225, #3226) —
        // each is SPENT below by the arm that acted on it, never here.
        let restart = self.rd_restart_mark(group, pr);
        let facts = reviewdrive::DriveFacts {
            now_ms: now,
            pr_open: obs.open,
            head: obs.head.clone(),
            body_digest: obs.body_digest.clone(),
            required_lanes: required.clone(),
            ci: obs.ci,
            // A dead resumed pane IS `worker-unresumable`, learned one tick
            // after the hand-back instead of one fix timeout after it. It rides
            // the existing signal rather than a new arc because §2.1 already
            // routes `Unresumable` from `fix-wait` to exactly this hold; what
            // was missing was anything that could ever produce it after the
            // hand-back itself had succeeded.
            worker: match worker_exit {
                Some(_) => reviewdrive::WorkerSignal::Unresumable,
                None => signal.worker,
            },
            gate,
            messaged: signal.messaged,
            // #2811 S10, and READ here — the mark is SPENT below, by the tick
            // that acts on it (#3196 review 2, rev-final finding 1).
            //
            // Taking it here was wrong, and wrong in the direction that revives
            // the incident this slice exists to remove. Several things decide
            // above `decide_fix_wait` and none of them re-brief anybody: the
            // empty-head guard returns `Wait` whenever `observe_pr` could not
            // read the PR — a runner error, a rate limit, an unparseable
            // response, which is a routine first-tick condition when a restart
            // sends a burst of `gh` calls at once — and the age and state
            // backstops park the drive. The reconcile runs once per registry
            // instance, so a mark spent by a tick that did nothing is never
            // re-issued: the drive keeps its dead pane, is never re-briefed, and
            // waits out `fix_timeout_minutes` into exactly the
            // `held(fix-stalled)` this slice is about.
            //
            // So the mark now survives every such tick and is discharged only by
            // one that RESOLVED the restart question — see the spend below.
            restart_handback: restart.handback,
            // #3225: the sibling one state later — a push whose worker pane
            // died with the process is the fix delivered, not a report to
            // keep waiting for. Set by the same reconcile, on the same
            // roster reading, and spent by the arc it produces.
            restart_push_delivered: restart.push_delivered,
            // #2811 S5b: the union over every pane this drive owns — lanes
            // AND the worker — which is why the fact is drive-level and not
            // on `LaneFact`: a drive in `fix-wait` owns a worker pane and no
            // open lane at all, and that is exactly a drive the hold covers.
            //
            // Read from the attention scan's published map, never by reading
            // pane text here: plan-2504 keeps ONE pane-text classifier, and a
            // second would be a second answer that can disagree with the chip
            // the human is looking at.
            //
            // `prior_*` agents come with `owned_panes()` and are harmless:
            // a pane that is gone is not in the map, so it contributes
            // nothing, and one still alive on a limited provider is a pane
            // this drive really did leave stopped.
            provider_limited: state
                .entry(pr)
                .map(|e| e.owned_panes())
                .unwrap_or_default()
                .into_iter()
                .find_map(|(agent, _)| self.provider_limit_for_agent(&agent)),
        };
        let mut out = RdOut::new(pr);
        out.backoff = obs.runner_failed;
        if let Some(why) = &worker_exit {
            out.refusal = why.clone();
        }
        let brief = RdBrief {
            pr,
            head: obs.head.clone(),
            base: obs.base.clone(),
            body_digest: obs.body_digest.clone().unwrap_or_default(),
            ci: obs.ci,
            failing_jobs: obs.failing_jobs.clone(),
            required: required
                .as_ref()
                .map(|r| r.iter().map(|l| l.block.clone()).collect())
                .unwrap_or_default(),
            lane_notices,
            // The index `decide_review_wait` walks to, computed with the same
            // pure function it uses rather than guessed from the verdicts.
            deciding_lane: required.as_deref().and_then(|r| {
                r.get(reviewdrive::first_stale_lane(
                    r,
                    &obs.head,
                    obs.body_digest.as_deref(),
                ))
                .map(|l| l.block.clone())
            }),
        };
        // **Every pane ANOTHER live drive owns**, read here because this is the
        // last point at which the whole state is borrowable immutably — the
        // next line takes this entry mutably (review round 1, finding 1).
        //
        // `already-driven` is keyed on the PR and not on the session
        // (`rd_refuse(RESUME_...)` below), so two live drives may legally name
        // one worker session: the same worker pushed two PRs. Without this, the
        // terminal release of drive A would take a pane drive B's record still
        // names and is still going to speak to — the one claim the session
        // widening rests on ("nobody is going to speak to it again"), broken by
        // the widening itself. Computed for every tick rather than only for the
        // terminal ones: it is a walk over a handful of entries, and a
        // conditional read here would have to be re-derived below where the
        // borrow is gone.
        let owned_elsewhere: Vec<String> = state
            .entries
            .iter()
            .filter(|e| e.pr != pr && e.state().is_live())
            .flat_map(|e| e.owned_panes().into_iter().map(|(a, _)| a))
            .collect();
        let entry = state.entry_mut(pr)?;
        // What each lane's verdict file said this tick, recorded onto that lane
        // BEFORE the decision — not as an input to it (nothing decides from a
        // recorded verdict; the live file is re-read every tick through the
        // gate's own parser) but because `at_head` is what tells a lane that has
        // ANSWERED from one that has only been ASKED. Without it a re-briefed
        // lane looks like a first-time lane forever and §5.5's delta template is
        // unreachable, which is the defect this line closes.
        for l in &brief.lane_notices {
            if entry.record_verdict_seen(&l.block, l.verdict, &l.at_head) {
                out.changed = true;
            }
        }
        let step = reviewdrive::decide(entry, &facts, limits);
        // **Read BEFORE the arc is taken, applied after it** (#2501). Both
        // halves matter. `releasable`'s worker rule is "a hand-back is
        // outstanding and the report it was waiting for arrived", which stops
        // being true the instant `entry.take` takes the arc out of that wait;
        // its terminal rule reads the entry the step is about to END, which
        // `take` equally destroys (#2811 S1); and the
        // lane rule must not see a lane this very tick re-briefed, which the
        // `OpenLane` arm below is about to do. Computed here, the answer is
        // about the world the decision was made in.
        let releases = reviewdrive::releasable(entry, &facts, &step);
        let on_behalf = entry.on_behalf_of.clone();
        // §2.1's `review-wait` row writes "the current lane index", and the
        // current lane is the DECIDING one — the first whose pass does not stand
        // — whether or not this tick had to open it. Writing it only on a
        // successful spawn leaves it lagging every time the drive waits on a
        // lane that is already open, which is most ticks of most rounds. It is
        // only a display and last-resort-fallback field today, so the lag was
        // not reachable as a defect; a field that is silently wrong is how the
        // next reader is misled.
        if entry.state() == reviewdrive::DriveState::ReviewWait {
            if let Some(k) = brief
                .deciding_lane
                .as_deref()
                .and_then(|b| brief.required.iter().position(|r| r == b))
            {
                if entry.lane_index != k {
                    entry.lane_index = k;
                    out.changed = true;
                }
            }
        }
        // **The releases (#2501)** — §3.1 item 5's narrowing, performed.
        //
        // **BEFORE the step's own arm, and that placement is the whole of
        // rev-final W1.** The obvious spot is below the `match`, after the arc
        // has been taken; it is wrong, and wrong in exactly the case this
        // feature exists for. On the `review-wait -> fix-wait` fail route the
        // arm calls `rd_handback`, and since #2501 released the previous
        // worker's pane there is usually no live pane to reuse — so it SPAWNS,
        // under the live-delegate cap, while the lane pane this tick is about to
        // release is still `status != Dead` and still counted. At the cap the
        // spawn is refused, the arm parks the drive `held(cap-refused)` on a
        // notice asking an orchestrator to free a slot, and only then would the
        // release free one. A parked drive does not self-advance, so that is an
        // orchestrator wake — the exact cost #2501 measured, reintroduced by
        // #2501's own worker rule. Releasing first means `mark_dead` has already
        // dropped the pane out of `live_delegate_count` before `rd_spawn` asks.
        //
        // Nothing else depends on the old order. The candidates were computed
        // pre-arc either way (see `releasable` above); reading each pane id here
        // reads the record BEFORE `rd_open_lane` could replace it, which is
        // strictly safer than after; and the exit notices are still built below
        // the whole `match`, off `entry.owned_panes()`, so they name what is
        // actually left. The one thing that moves is the ordering against
        // `entry.take`: a transition `take` refuses (unreachable through
        // `decide`) would now follow a release rather than skip it, which is
        // still correct — a lane that answered at this head is finished with
        // this round whether or not the drive managed to move.
        //
        // Three things happen per candidate, in this order and for these
        // reasons.
        //
        // **The session is resolved FIRST**, through `rd_lane_session`'s three
        // sources, and an empty answer skips the candidate. The promise this
        // whole mechanism rests on is that the conversation survives, and a lane
        // whose session cannot be named is one whose conversation the kill would
        // end. Failing closed here costs a slot; failing open costs the review.
        //
        // **Then the kill**, through the one capability the driver has for it.
        // `release_driven_pane` is the barrier — idle, alive, bound to a
        // terminal, not a manager — and it answers `Err` for every pane that is
        // none of those, which is skipped rather than recorded.
        //
        // **Then the record**, and only on the kill having succeeded. The two
        // are under ONE hold of `rd_state_lock`, which is what keeps the pane
        // from ever being live and unowned: `mark_dead` has already made
        // `resolve_token` refuse that caller by the time its id leaves the
        // record, so §7 has no window to leak through. Doing the kill out in the
        // caller's side-effect loop, the way a kick-back delivery is done, would
        // open exactly that window — and a delivery is safe there because losing
        // one costs a line, while losing this ordering costs an unowned pane.
        //
        // The audit row is written last, on the fact rather than the intent:
        // §5.4 says a release row means a pane went, and a reader counting freed
        // slots must be able to trust the count.
        // **The session of the worker pane this tick released, for the terminal
        // notice** (#2811 S1). Read out of the loop rather than recomputed below,
        // because by the time the notice is built `release_pane` has taken the
        // pane out of the record and the entry no longer names it — and because
        // only a release that the barrier actually PERFORMED may be reported as
        // one, which is the same honesty `out.releases` keeps.
        // **#3176's other half: a lane the release CANNOT take is told to stop
        // instead.**
        //
        // `release_driven_pane` refuses a pane that is not idle, and §3 is why —
        // the driver does not kill a reviewer mid-turn. But a reviewer mid-turn
        // on a PR that does not merge is precisely the case #3176 is about: it
        // is spending a paid round reading a head the rebase is about to
        // replace. So the two arms are complementary rather than alternatives,
        // and exactly one of them applies to a lane on a tick — the idle test
        // below is the same question `release_driven_pane` asks, asked first so
        // the two cannot both fire.
        //
        // **A queued delivery, never an interrupt** — `Delivery::MidSession`,
        // the same mechanism `rd_reuse_pane` types a re-brief with. It lands on
        // the pane's own queue and is pasted when the CLI next takes input;
        // nothing is cancelled, no signal is sent, and a reviewer mid-thought
        // finishes it and then reads this. That is the only kind of text orrerix
        // puts into a delegate's pane, and this adds no second kind.
        //
        // **Once per revision, not once per tick.** The rule is a standing
        // property of the facts — that is what makes the release arm work at all
        // — so `stopped_head` marks the lane, and the mark is written on the
        // delivery SUCCEEDING rather than on the intent, so a line that did not
        // reach the pane is one the next tick still owes.
        //
        // A lane with no pane on record, and one whose stop line has already
        // landed at this head, are skipped. So is a lane the candidate list does
        // not carry, which is the carve-out doing its work one function over: a
        // reviewer whose verdict is already on record is not mid-review and has
        // nothing to stand down from.
        for cand in &releases {
            if cand.reason != reviewdrive::ReleaseReason::Conflict {
                continue;
            }
            let reviewdrive::DrivenRole::Lane(block) = &cand.role else { continue };
            let agent = entry.lane(block).map(|r| r.agent.clone()).unwrap_or_default();
            if agent.trim().is_empty() {
                continue;
            }
            // **Which panes this arm is for, decided once** (rev-std round 1
            // finding 1).
            //
            // IDLE is the release arm's: telling a pane to stop and then killing
            // it on the same tick would be two events for one decision, and
            // `idle_since_ms` is the same signal `release_driven_pane` refuses
            // on, so the two arms cannot both fire.
            //
            // DEAD, or unknown to the registry, is NEITHER arm's — and the
            // reason is the retry. The declined row below earns its retry from
            // the queue-full case, where the next tick can succeed. For a pane
            // that died mid-review (a human kill, the idle reaper) while the PR
            // is CONFLICTING, `deliver_prompt` answers `Err` on every tick of
            // the whole conflict window, the mark is never written and the
            // release barrier refuses the same pane — so a futile retry would
            // put one row per tick on the very surface §5.4 asks a reader to
            // count from. Bounded and truthful, but noise, and noise on that
            // surface is what `rd-hold-repeated` exists to stop one arm over.
            //
            // Nothing else changes: the lane keeps its dead pane on record and
            // `decide_review_wait`'s `pane_dead` arm re-opens it once the rebase
            // clears the conflict, which is #2163's existing path and not this
            // slice's to alter.
            match self.agent(&agent) {
                Some(a) if a.idle_since_ms.is_some() => continue,
                Some(a) if a.status == AgentStatus::Dead => continue,
                Some(_) => {}
                None => continue,
            }
            if entry.lane_stopped_at(block, &brief.head) {
                continue;
            }
            let text = self.rd_lane_stop_brief(&brief);
            // **A refusal is AUDITED, not swallowed** (#3176, aligned with
            // #3203's `rd_take_over_pane`). `deliver_prompt` can refuse for
            // reasons that say nothing about the drive, but one is reachable
            // with nothing wrong at all — a pane at `QUEUE_MAX_PER_PANE` — and
            // on a silent `continue` that is indistinguishable from "there was
            // no busy lane to tell", which is the exact indistinguishability
            // `rd-reuse-declined` and `rd-takeover-declined` were both added to
            // remove. The mark is not written on this path, so the next tick
            // tries again and these rows say how many ticks it took.
            if let Err(why) = self.deliver_prompt(
                &agent,
                &text,
                brand::AUDIT_ACTOR,
                Delivery::MidSession,
            ) {
                out.audits.push((
                    rddrive::audit_action::LANE_STOP_DECLINED,
                    json!({ "pr": pr, "block": block, "agent": agent,
                            "head": brief.head, "reason": why }),
                ));
                continue;
            }
            if entry.mark_lane_stopped(block, &brief.head) {
                out.changed = true;
            }
            out.audits.push((
                rddrive::audit_action::LANE_STOPPED,
                json!({ "pr": pr, "block": block, "agent": agent,
                        "head": brief.head, "why": "conflict" }),
            ));
        }
        let mut released_worker_session = String::new();
        for cand in &releases {
            // **A WORKER candidate names every pane this drive owns on that
            // session, not only the current one** (#3203). `releasable` decides
            // per ROLE, and the worker role is one conversation that may have
            // more than one pane sitting on it: a hand-back that superseded a
            // pane left the old one alive and owned, and before #3203's other
            // half a second hand-back minted one more. Releasing only
            // `worker_agent` left those behind idle and counted against the
            // cap for the rest of the drive — measured on PR #3198, where the
            // ORIGINAL worker pane sat idle through two hand-backs and two
            // releases.
            //
            // Nothing here decides WHETHER a pane may go: `release_driven_pane`
            // is still the barrier (idle, alive, bound to a terminal, not a
            // manager), applied per pane, so a superseded pane that is somehow
            // busy is skipped exactly as the current one would be. What widens
            // is only the population the barrier is asked about.
            //
            // Ordered oldest-first, so the audit rows read as the history
            // they are: `owned_panes`'s own order for an owned-only population,
            // and `reviewdrive::release_population`'s sort wherever the founding
            // panes widen it.
            //
            // **And at a TERMINAL exit it also names the panes the drive was
            // STARTED ON** (#3250). `owned_panes` is written by a hand-back, so
            // it is EMPTY for a drive that never took one — which is not a
            // corner: on PRs #3243 and #3248 the audit log runs `rd-started` ->
            // `rd-satisfied` with no `rd-handback` row at all, and the worker
            // pane the orchestrator named at `start_review_drive` sat idle
            // through the exit holding its worktree.
            // `ReleaseCandidate::include_founding` carries the decision and the
            // bound — it is the engine's to make, so this reads the flag rather
            // than re-deriving the step.
            //
            // Still nothing here decides WHETHER a pane may go: the barrier is
            // applied per pane below, unchanged, so a founding pane that is busy
            // or not a driven delegate's role is skipped exactly as an owned
            // one is.
            let (agents, session) = match &cand.role {
                reviewdrive::DrivenRole::Worker => {
                    let mut agents: Vec<String> = entry
                        .owned_panes()
                        .into_iter()
                        .filter(|(_, role)| *role == reviewdrive::DrivenRole::Worker)
                        .map(|(agent, _)| agent)
                        .collect();
                    if cand.include_founding {
                        // **The merge, the narrowing and the ORDER are the
                        // engine's** (review rounds 1 and 2), so each is stated
                        // and tested in one place instead of being spelled as
                        // loop conditions here. All this side supplies is the
                        // registry's own fact: how old a pane is.
                        agents = reviewdrive::release_population(
                            agents,
                            &entry.founding_panes,
                            &owned_elsewhere,
                            &|a: &str| self.agent(a).map(|x| x.started_ms),
                        );
                    }
                    (agents, entry.worker_session.clone())
                }
                reviewdrive::DrivenRole::Lane(block) => {
                    let rec = entry.lane(block);
                    let agent = rec.map(|r| r.agent.clone()).unwrap_or_default();
                    let session = self.rd_lane_session(group, rec).unwrap_or_default();
                    (vec![agent], session)
                }
            };
            if session.trim().is_empty() {
                continue;
            }
            for agent in agents {
                if agent.trim().is_empty() {
                    continue;
                }
                if self.release_driven_pane(&agent).is_err() {
                    continue;
                }
                // **The record drop, and why a superseded pane needs none.**
                // `release_pane` clears the CURRENT pane out of the entry, which
                // is what keeps a live pane from ever being unowned (§7). A
                // superseded id is already on a list bounded by LIVENESS, and
                // `release_driven_pane` has just made this one dead, so
                // `forget_dead_panes` drops it — writing it out here as well
                // would be a write whose only effect is to be undone, which is
                // the argument `release_pane`'s own doc makes about the lane it
                // does not push.
                let freed = if entry.worker_agent == agent {
                    match entry.release_pane(&cand.role, &session) {
                        Some(freed) => freed,
                        None => continue,
                    }
                } else if cand.role == reviewdrive::DrivenRole::Worker {
                    agent.clone()
                } else {
                    // **A conflict release also forgets the revision the lane
                    // was briefed at** (#3176) — see
                    // [`reviewdrive::DriveEntry::reseed_lane`] for why a plain
                    // `release_pane` here leaves `review-wait` waiting on a lane
                    // it can see is open and cannot see has no pane. Every other
                    // reason releases a lane that has ANSWERED, where the field
                    // is inert.
                    //
                    // This is the LANE arm: the two above it are the worker's
                    // (#3203's per-pane widening), and a lane candidate names
                    // exactly one pane, so the reseed cannot reach a superseded
                    // id it was not decided for.
                    let freed = match (&cand.role, cand.reason) {
                        (
                            reviewdrive::DrivenRole::Lane(block),
                            reviewdrive::ReleaseReason::Conflict,
                        ) => entry.reseed_lane(block, &session),
                        _ => entry.release_pane(&cand.role, &session),
                    };
                    match freed {
                        Some(freed) => freed,
                        None => continue,
                    }
                };
                if cand.role == reviewdrive::DrivenRole::Worker {
                    released_worker_session = session.clone();
                }
                out.changed = true;
                let (action, mut detail) = match &cand.role {
                    reviewdrive::DrivenRole::Worker => (
                        rddrive::audit_action::WORKER_RELEASED,
                        json!({ "pr": pr, "agent": freed, "session": session,
                                "reason": cand.reason.as_str() }),
                    ),
                    reviewdrive::DrivenRole::Lane(block) => (
                        rddrive::audit_action::LANE_RELEASED,
                        json!({ "pr": pr, "block": block, "agent": freed, "session": session,
                                "reason": cand.reason.as_str() }),
                    ),
                };
                detail["head"] = Value::String(brief.head.clone());
                out.audits.push((action, detail));
                out.releases.push((cand.role.clone(), freed));
            }
        }
        // #2811 S10: set by the `Rehandback` arm, read by the spend below.
        let mut re_briefed = false;
        match &step {
            reviewdrive::DriveStep::Wait => {
                // **#1959: a worker's `report(progress)` in `fix-wait` is
                // answered, in the worker's own pane.**
                //
                // The drive does not move on it and must not — a drive advances
                // on the head, the checks and the verdict files, and treating
                // "still going" as "the fix is in" would brief a reviewer over
                // unfinished work. But swallowing it is #1857's shape one arm
                // over: the measured round was a BODY-ONLY fix, so there was
                // nothing to push and no new checks, the worker read the
                // brief's "report when the checks are green" literally and sent
                // `progress`, and the drive sat for ten minutes until the idle
                // watchdog woke the ORCHESTRATOR — the turn the driver exists to
                // remove. One line back into the worker's pane costs that turn
                // nothing.
                //
                // Under `Wait` alone, so it can never displace an arc: a tick
                // that has something to DO does it, and a worker whose
                // `report(done)` arrived in the same window is advanced rather
                // than lectured.
                if signal.worker_progress && entry.kickback_owed() {
                    let agent = entry.worker_agent.clone();
                    if !agent.is_empty() {
                        // Marked BEFORE the delivery, which happens outside this
                        // lock. A delivery that fails therefore costs the line —
                        // the same asymmetry §5.2 already draws for a hold's
                        // notice, and the right direction here: the drive stays
                        // bounded by `fix-stalled` either way, while a mark
                        // written only on success would re-emit on every tick
                        // for as long as a pane stayed unreachable.
                        entry.record_kickback(now);
                        out.changed = true;
                        out.kickback = Some((agent.clone(), rddrive::fix_kickback_notice(pr)));
                        out.audits.push((
                            rddrive::audit_action::KICKBACK,
                            json!({ "pr": pr, "agent": agent }),
                        ));
                    }
                }
            }
            // #2811 S10: the restart re-hand-back. **No arc, no counter.**
            //
            // The drive stays in `fix-wait`; what was lost across the process
            // boundary is the worker's PANE, not its session and not anything
            // it had been told. So this re-renders the same fix brief and
            // resumes the recorded session, exactly as the arc into `fix-wait`
            // does — and charges nothing for it, because no review round, CI
            // run or rebase happened. A restart is not a round.
            //
            // `grace: false`: #2509's one-shot grace is granted by an arc that
            // spends it, and this takes no arc. Passing `true` here would print
            // a grant the entry's counters do not record.
            reviewdrive::DriveStep::Rehandback => {
                re_briefed = true;
                match self.rd_handback(group, entry, &brief, limits, false) {
                    Ok((agent, pane)) => {
                        // The clock moves only once the worker has actually
                        // been reached, so a hand-back that failed leaves
                        // `held(fix-stalled)`'s bound where it was rather than
                        // silently extending it by a whole `fix_timeout_minutes`
                        // on every restart.
                        entry.restamp_fix_handback(now);
                        self.rd_handback_fails.lock_safe().remove(&(group.clone(), pr));
                        out.changed = true;
                        out.handback = Some(agent.clone());
                        out.audits.push((
                            rddrive::audit_action::HANDBACK,
                            json!({ "pr": pr, "agent": agent, "head": brief.head,
                                    "why": rddrive::handback_why::RESTART,
                                    // #3203: written on THIS arm too. A restart
                                    // took every pane with the old process, so
                                    // this is normally `spawned` — but the arm
                                    // runs whenever a restart MARK is standing,
                                    // and the orchestrator may already have
                                    // reopened that session by hand, in which
                                    // case the take-over arm reaches that pane
                                    // rather than opening a second one beside it.
                                    "pane": pane }),
                        ));
                    }
                    Err(why) => {
                        // A session that will not resume really is
                        // `worker-unresumable`, and it is learned HERE rather
                        // than assumed at reconcile: the mark says the process
                        // restarted, and only the attempt can say whether the
                        // session survived it. Held through the same arc every
                        // other failed hand-back takes, so the notice, the
                        // second-failure wording and `rd-held` are one path.
                        self.rd_handback_failed(group, entry, pr, &why, &mut out, now);
                    }
                }
            }
            reviewdrive::DriveStep::OpenLane { index, verify, body_only } => {
                // `decide` only ever names an index into the list it was handed,
                // so `None` is unreachable — and it is handled by falling
                // THROUGH rather than returning, because an early return here
                // would skip the head persistence below, which is the one write
                // this function exists to get right. An unreachable branch that
                // skips a load-bearing write is how the reachable one gets
                // broken later.
                let block = brief.required.get(*index).cloned().unwrap_or_default();
                // #2163: read BEFORE the open, which replaces the record this
                // asks about — and turned into a row only on the `Ok` arm,
                // because a re-open the cap refused has re-opened nothing.
                let replaced = entry.lane(&block).and_then(|rec| self.rd_dead_lane_pane(rec));
                match self
                    .rd_open_lane(group, entry, &block, &brief, limits, now, *verify, *body_only)
                {
                    Ok(RdLaneOpen { agent, session, resumed, scope }) => {
                        entry.lane_index = *index;
                        // **#3226: why this lane was re-briefed, where the
                        // answer is not derivable from the row.** A lane
                        // whose pane died with the process is reseeded by
                        // the reconcile, so what arrives here is an
                        // ordinary first-brief-of-a-round — same shape,
                        // same resumed session, same pane kind — and on
                        // #3226s incident the only way to tell a restart
                        // recovery from a normal round was to notice that
                        // no head had moved between them.
                        //
                        // Spent HERE rather than at the end of the tick:
                        // this lane has been re-briefed, and the other
                        // lanes of the same drive have not.
                        let mut why_restart = false;
                        self.rd_spend_restart_mark(group, pr, |mark| {
                            if let Some(k) = mark.lanes.iter().position(|b| b == &block) {
                                mark.lanes.remove(k);
                                why_restart = true;
                            }
                        });
                        if let Some((pane, killed_by)) = replaced {
                            out.audits.push((
                                rddrive::audit_action::LANE_REOPENED,
                                json!({ "pr": pr, "block": block, "head": brief.head,
                                        "pane": pane, "killed_by": killed_by,
                                        "agent": agent, "resumed": resumed }),
                            ));
                        }
                        // #2109: a lane opened is the refusal run ending. Not
                        // folded into `advance` — opening a lane is not an arc
                        // (§2.1), so there is no transition here to hang it on.
                        // #2110: it takes the clock, because ending a run is
                        // what charges its cost to the exclusion totals the
                        // two age bounds subtract.
                        entry.clear_cap_starvation(now);
                        out.changed = true;
                        out.lanes_opened.push((block.clone(), agent.clone()));
                        let mut spawned =
                            // #2109: `head`, `session` and `resumed` on the
                            // row. `resumed` is the fact the issue is about and
                            // the one nothing else records — a resumed lane and
                            // a fresh one produce the same shape of row, the
                            // same kind of pane id, and the same brief, so the
                            // only pre-#2109 way to tell nine panes from six was
                            // to count them by hand across three PRs.
                            // #2508: `scope` beside them, the same string the
                            // brief rendered — a reader counting whole-diff
                            // rounds for a beta's before/after reads it here
                            // rather than re-deriving it from round numbers.
                            json!({ "pr": pr, "block": block, "agent": agent,
                                    "head": brief.head, "session": session,
                                    "resumed": resumed, "scope": scope,
                                    "round": entry.counters.review_rounds + 1 });
                        // #3226, and ABSENT on an ordinary round rather than
                        // null: `why` is a claim about a restart, and a key
                        // present on every row would say nothing on the ones
                        // it is really about. Same shape as `rd-handback`s
                        // own `why`.
                        if why_restart {
                            spawned["why"] = Value::String("restart".to_string());
                        }
                        out.audits.push((rddrive::audit_action::LANE_SPAWNED, spawned));
                    }
                    Err(why) => {
                        // §8's live-delegate-cap row: a refused spawn is a
                        // runner-class outcome. Back off and retry on a later
                        // tick — and NEVER kill a pane to make room (§3.1
                        // item 5).
                        let cap = super::is_live_cap_refusal(&why);
                        out.backoff = true;
                        // **The retry is bounded by something that names the
                        // cap** (#2109). It used to be bounded only by
                        // `drive_timeout_minutes`, whose notice says nothing
                        // about slots: the measured drive sat in `review-wait`
                        // with `lanes: []` for three hours, emitting one of
                        // these rows per tick and no §2.2 exit at all. The stamp
                        // is written on the FIRST cap refusal of a run and left
                        // alone by the cap refusals after it, so `decide` reads
                        // a duration rather than the age of the newest tick.
                        // What keeps it a RUN is the three clears: the Ok arm
                        // below, every state arc, and — since review 4 — the
                        // `else` on this very branch, for any refusal that is
                        // not the cap's.
                        //
                        // **Only a CAP refusal stamps it**, because the hold it
                        // leads to names the cap: a persistent non-cap refusal
                        // (an unknown block, a workspace that will not resolve,
                        // this drive's own duplicate refusal) would park as
                        // `held(cap-full)` on a notice telling an orchestrator to
                        // free a slot that is not the problem — the
                        // diagnosis-for-observation swap #1961 fixed one hold
                        // over. Those keep the bound they have,
                        // `drive_timeout_minutes`, which asserts nothing.
                        //
                        // **And only a cap refusal may LET it stand** (#2109
                        // review 4). Guarding the write alone made the stamp a
                        // latch: nothing on a non-cap refusal re-stamped it and
                        // nothing cleared it, so a single early cap refusal
                        // aged into `held(cap-full)` behind a run of refusals
                        // that were nothing of the kind — the exact outcome the
                        // paragraph above says is prevented, reached from the
                        // read edge instead of the write edge. Worse, the stamp
                        // is the ENTRY's while `first_stale_lane` re-picks the
                        // lane every tick, so a two-lane drive could park on a
                        // stamp left by a lane that is no longer the subject,
                        // while the lane that IS the subject sits in a live pane
                        // the reuse arm could have delivered into for free.
                        //
                        // Clearing on every non-cap refusal closes both, and it
                        // closes the second WITHOUT keying the stamp on a block,
                        // which would be worse than the defect: a drive that
                        // cannot open ANY lane is starved whichever lane the
                        // tick happens to select, and a per-block clock would
                        // restart every time the selection moved — letting a
                        // genuinely starved multi-lane drive evade the bound by
                        // alternating. The cost is that a mixed run restarts the
                        // window at each cap refusal after a non-cap one, which
                        // is the fail-safe direction and is what the word
                        // "continuously" on `HeldReason::CapFull` promises.
                        // #2110: the clear takes the clock, because ending a
                        // run is what CHARGES its cost to the exclusion totals
                        // both age bounds subtract. That matters most on this
                        // site rather than least: a mixed run restarts the
                        // window here, and a restart that forgot what the
                        // previous stretch cost would hand the drive back time
                        // it never had.
                        let moved = if cap {
                            entry.note_cap_starvation(now)
                        } else {
                            entry.clear_cap_starvation(now)
                        };
                        if moved {
                            out.changed = true;
                        }
                        out.audits.push((
                            rddrive::audit_action::REFUSED,
                            json!({ "pr": pr, "block": block, "reason": "lane-spawn-refused",
                                    // #2109: how long the cap has been refusing
                                    // this drive, on the row a reader is already
                                    // looking at. `cap: true` says a slot was
                                    // the problem on THIS tick; a reader chasing
                                    // the starved drive wants the run.
                                    "starved_ms": entry.cap_starved_for(now),
                                    // #1960: whether the CAP is what refused,
                                    // on the row itself. A reader chasing a
                                    // drive that opened no lane for twenty
                                    // minutes was reading a free-text `detail`
                                    // to find out, and the answer decides
                                    // whether anything is wrong at all — a
                                    // capped lane retries and clears itself.
                                    "cap": cap,
                                    "detail": why }),
                        ));
                    }
                }
            }
            reviewdrive::DriveStep::Advance { to, held_reason, .. } => {
                // The CI observation that caused this arc is audited BEFORE the
                // arc is taken, so a green and a red are separate actions in the
                // order they were observed (§5.4: a filter looking for the thing
                // that happened must not match the thing that did not).
                match (entry.state(), obs.ci) {
                    (reviewdrive::DriveState::CiWait, reviewdrive::CiObservation::Green) => {
                        out.audits.push((
                            rddrive::audit_action::CI_GREEN,
                            json!({ "pr": pr, "head": brief.head }),
                        ))
                    }
                    (reviewdrive::DriveState::CiWait, reviewdrive::CiObservation::Red) => {
                        out.audits.push((
                            rddrive::audit_action::CI_RED,
                            json!({ "pr": pr, "head": brief.head, "failing": brief.failing_jobs }),
                        ))
                    }
                    // **Every state the engine lets act on a conflict**, not
                    // `ci-wait` alone (#2311): `decide` reads mergeability above the
                    // per-state logic, so `gate-check` and `review-wait` take the
                    // same arc 3 and owe the same row. It accounts for whichever
                    // step the same tick takes: the `rd-handback` `why:conflict`
                    // while `rebase_attempts` lasts, or the `rd-held`
                    // `rebase-limit` park once it is spent. Either way, an
                    // `rd-handback` `why:conflict` with no `rd-conflicting` above
                    // it is a spent `rebase_attempts` a §5.4 reader cannot
                    // explain, and `scripts/orch-scorecard.cjs` classifies it
                    // only in the generic `rd-*` census — no named case counts
                    // it. `fix-wait` is excluded
                    // by the same explicit clause the engine uses (`state !=
                    // FixWait`, not the arc table): the rebase is already
                    // outstanding there, so no arc is taken and there is nothing
                    // to account for.
                    (st, reviewdrive::CiObservation::Conflicting)
                        if st != reviewdrive::DriveState::FixWait =>
                    {
                        out.audits.push((
                            rddrive::audit_action::CONFLICTING,
                            json!({ "pr": pr, "base": brief.base }),
                        ))
                    }
                    _ => {}
                }
                if let Err(bad) = entry.take(&step, now) {
                    // Unreachable through `decide`, which only proposes arcs the
                    // table names — and handled rather than unwrapped, because an
                    // unwind out of the shared poll thread would take every watch
                    // in the fleet down with it.
                    out.audits.push((
                        rddrive::audit_action::REFUSED,
                        json!({ "pr": pr, "reason": "invalid-transition", "detail": bad.to_string() }),
                    ));
                    return Some(out);
                }
                out.changed = true;
                out.clear_signal = true;
                out.advanced = Some((*to, *held_reason));
                match to {
                    reviewdrive::DriveState::FixWait => {
                        // **#2509's grace, read off the STEP and not off the
                        // counters.** `take` above has already set
                        // `body_only_grace`, so a second derivation here would
                        // read `true` on every later hand-back of this drive
                        // too — which is the grant disagreeing with itself, the
                        // thing `briefed_verify` is placed on the step to avoid.
                        // The bump names this arc and nothing else.
                        let grace = matches!(
                            step,
                            reviewdrive::DriveStep::Advance {
                                bump: Some(reviewdrive::Counter::BodyOnlyGrace),
                                ..
                            }
                        );
                        // Written BEFORE the hand-back, so a grace whose
                        // hand-back then fails still shows the round that was
                        // granted — the arc is what spent it, and `take`
                        // accepted the arc two screens up. The `rd-handback`
                        // row that follows on the `Ok` arm is what says the
                        // worker was actually reached.
                        // #3367 item 1: the driver's OWN non-blocking round,
                        // read off the step for the grace's reason above. The
                        // revision is stamped here, from the facts this tick
                        // read, because `decide` compares the next satisfied
                        // gate against it — a worker that changes nothing does
                        // not buy a second identical round.
                        let nit = matches!(
                            step,
                            reviewdrive::DriveStep::Advance {
                                bump: Some(reviewdrive::Counter::NonblockingRound),
                                ..
                            }
                        );
                        if nit {
                            entry.nit_head = brief.head.clone();
                            entry.nit_digest = brief.body_digest.clone();
                            out.audits.push((
                                rddrive::audit_action::AUTO_HANDBACK,
                                json!({ "pr": pr, "head": brief.head,
                                        "round": entry.nit_rounds,
                                        "of": limits.fix_nonblocking_rounds,
                                        "review_rounds": entry.counters.review_rounds,
                                        "residual": rddrive::residual_text(&brief.lane_notices) }),
                            ));
                        }
                        if grace {
                            out.audits.push((
                                rddrive::audit_action::ROUND_GRACE,
                                json!({ "pr": pr, "head": brief.head,
                                        "block": brief.deciding_lane.clone()
                                            .unwrap_or_else(|| brief.failing_lane()),
                                        "round": entry.counters.review_rounds,
                                        "reason": "body-only" }),
                            ));
                        }
                        match self.rd_handback(group, entry, &brief, limits, grace) {
                            Ok((agent, pane)) => {
                                // A hand-back that WORKED ends the second-failure
                                // count (#2555 item 2): the next failure, whenever it
                                // comes, is a first one again.
                                self.rd_handback_fails.lock_safe().remove(&(group.clone(), pr));
                                out.handback = Some(agent.clone());
                                out.audits.push((
                                    rddrive::audit_action::HANDBACK,
                                    json!({ "pr": pr, "agent": agent, "head": brief.head,
                                            "why": brief.handback_kind(),
                                            // #3203: whether this hand-back put a
                                            // NEW pane on the session, and if not,
                                            // which arm kept it from doing so.
                                            "pane": pane }),
                                ));
                            }
                            Err(why) => {
                                self.rd_handback_failed(group, entry, pr, &why, &mut out, now);
                            }
                        }
                    }
                    reviewdrive::DriveState::GateCheck | reviewdrive::DriveState::Satisfied => {
                        for l in &brief.lane_notices {
                            out.audits.push((
                                rddrive::audit_action::VERDICT,
                                json!({ "pr": pr, "block": l.block,
                                        "verdict": l.verdict.as_str(), "head": brief.head }),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        // **#2811 S10: the restart mark is spent by the tick that ACTED on it**,
        // never merely by the one that read it (#3196 review 2).
        //
        // Two ways to have acted, and both are about the drive rather than about
        // this tick's luck: the worker was re-briefed, or the drive is no longer
        // in `fix-wait` at all — arc 7 or 8, where a worker that pushed or
        // reported before the shutdown has already answered and no re-brief is
        // owed, or a hold, which is a decision surface for the orchestrator
        // rather than a wait this mark can shorten.
        //
        // Everything else LEAVES IT STANDING, which is the fix: a tick that could
        // not read the PR, or that was preempted by a bound above
        // `decide_fix_wait`, has decided nothing about the restart, and the next
        // tick is owed the same re-brief. That cannot loop — the first tick that
        // succeeds sets `re_briefed` and discharges it, which is what
        // `the_restart_mark_is_spent_by_the_tick_that_reads_it` pins.
        //
        // **Per FIELD since #3225/#3226**, because a drive can carry more
        // than one and a tick that acted on the lanes has decided nothing
        // about the worker. Each clause below is the same rule read for its
        // own recovery: acted, or the drive has left the state that recovery
        // was about.
        // Read once, before the closure borrows nothing of the entry.
        let here_after = entry.state();
        self.rd_spend_restart_mark(group, pr, |mark| {
            if re_briefed || here_after != reviewdrive::DriveState::FixWait {
                mark.handback = false;
            }
            // #3225: the arc out of `ci-wait` IS the act — it briefs the
            // lane at the pushed head, which is the whole recovery. A tick
            // that could not read the PR leaves `ci-wait` standing and the
            // mark with it, for `a_restart_tick_that_cannot_read_the_pr…`s
            // reason one field over.
            if here_after != reviewdrive::DriveState::CiWait {
                mark.push_delivered = false;
            }
            // #3226: a lane is spent by the OpenLane arm that re-briefed it
            // (above), so what is left here is the drive having left
            // `review-wait` — where no lane of this round will be opened at
            // all and the why has nothing to ride on.
            if here_after != reviewdrive::DriveState::ReviewWait {
                mark.lanes.clear();
            }
        });

        // **THE HEAD, PERSISTED — the line two reviewers named on S1 as the one
        // that would be forgotten.** `DriveEntry::head` is only ever *compared*
        // against the live head (arc 6 in `review-wait`, arc 7 in `fix-wait`), so
        // a tick that records it once at `drive_review` time and never again
        // makes that comparison permanently true: the drive takes arc 6 to
        // `ci-wait`, goes green, comes back to `review-wait`, takes arc 6 again,
        // forever — a PR that is never reviewed and never gated.
        //
        // **After `decide`, never before.** Arc 6 *is* `entry.head !=
        // facts.head`; writing first would make the two equal before anything
        // compared them, and the arc would be unreachable rather than permanent.
        //
        // **And only when the head actually resolved.** An empty `facts.head` is
        // a FAILED READ, not a head — the same class as `fix_handback_ms == 0`
        // meaning "ancient" rather than "unset". Writing it would leave every
        // later tick comparing a real live head against a stored `""`, taking
        // arc 6 on every wake; and in `review-wait` a stored `""` makes
        // `lane_open_for` refuse every record briefed at a real head, which is
        // `OpenLane{k}` on every tick — a reviewer spawned per tick, each brief
        // re-arming `spawned_ms` so `lane-stalled` can never fire. `decide`
        // refuses to dispatch at all on an empty LIVE head, which bounds the
        // damage this tick; this is what stops the ENTRY being poisoned so the
        // next read, successful or not, still misbehaves.
        if !obs.head.is_empty() && entry.head != obs.head {
            entry.head = obs.head.clone();
            // **A further push while the drive is already waiting on one**
            // (#2168 E1). In `ci-wait` on an arc-7 head there is no arc to fire
            // — `transition` refuses a self-arc — so without this the receipts
            // wait keeps running from the FIRST push and a worker that pushes a
            // follow-up commit late in the window has minutes to run a fresh
            // matrix and report. `note_fix_push` re-stamps only an anchor that
            // already exists, so this cannot start the wait in a state that
            // never entered it, and it is placed inside the head-actually-moved
            // guard above so a failed `gh` read can never renew it.
            entry.note_fix_push(now);
            out.changed = true;
        }
        // **What bounds the superseded-pane lists (#1871 B2, rev-final).** They
        // are pruned by LIVENESS and never by size: a size cap can only evict by
        // age, and the oldest superseded pane is one that is still running, still
        // on this session and still able to `report` — so evicting it un-owns it
        // exactly as the single slot did, which is B2 reproduced by the record
        // that fixes B2. `DriveEntry::forget_dead_panes` argues why a DEAD pane
        // is safe to forget instead.
        //
        // Liveness is the registry's fact, so the predicate is supplied here
        // rather than being reached for next door. `agent()` answers `None` for
        // an id that is gone; both that and `Dead` are states in which
        // `resolve_token` refuses the caller, so neither can reach the MCP seam.
        if entry.forget_dead_panes(&|id| self.rd_pane_is_live(id)) {
            out.changed = true;
        }
        if let Some(d) = obs.body_digest.as_deref() {
            if entry.body_digest != d {
                entry.body_digest = d.to_string();
                out.changed = true;
            }
        }
        // The exits (§2.2). Built here, where the entry's counters and lanes are
        // in hand; delivered by the caller, outside the lock.
        //
        // **A TERMINAL exit's notice is written ONTO the entry, inside this same
        // load-decide-store, and delivered from there** (#1857). It is not
        // handed to the caller as a string, because a string handed to a
        // delivery that answers `Err` is gone — and §5.2's retention then drops
        // the only record that could reproduce it, which is a drive that ends
        // with nothing in the pane and nothing to say why. `owe_notice` makes
        // the obligation durable before anything attempts it, and
        // `prune_terminal` will not drop an entry that still owes one.
        //
        // A `held` exit keeps the direct path deliberately, and the asymmetry is
        // the one §5.2 already draws: a parked entry is NEVER pruned, so the
        // drive survives its own lost notice — `review_drive_status()` lists it
        // and §2.3's resume re-reads it. What a hold can lose is a line; what a
        // terminal exit loses is the whole record. The mechanism is here if a
        // later change wants the stronger guarantee for a hold too.
        if let Some((to, reason)) = out.advanced {
            match (to, reason) {
                (reviewdrive::DriveState::Satisfied, _) => {
                    let n = rddrive::satisfied_notice(
                        pr,
                        &entry.head,
                        &entry.body_digest,
                        &brief.lane_notices,
                        &entry.counters,
                        &self.rd_surviving_panes(entry),
                        &released_worker_session,
                    ) + &rddrive::nonblocking_clause(
                        entry.nit_rounds,
                        limits.fix_nonblocking_rounds,
                        &brief.lane_notices,
                    );
                    // #3367 item 5: the CLEAN case — decided off the facts
                    // `decide` saw, never re-read. Where the merge queue is on,
                    // the submission itself waits for this tick's write (see
                    // `RdOut::clean_enqueue`) and appends the queue's answer to
                    // the notice owed here, before the flush delivers it.
                    let route = if !reviewdrive::gate_is_clean(&facts) {
                        rddrive::CleanRoute::NotClean
                    } else if self.merge_queue_enabled(group) {
                        rddrive::CleanRoute::Queue
                    } else {
                        rddrive::CleanRoute::Notice
                    };
                    let n = n + &rddrive::clean_clause(route);
                    let n = Self::rd_fold_auto_report(entry, n);
                    out.audits.push((
                        rddrive::audit_action::SATISFIED,
                        json!({ "pr": pr, "head": entry.head }),
                    ));
                    match route {
                        rddrive::CleanRoute::NotClean => {}
                        rddrive::CleanRoute::Notice => out.audits.push((
                            rddrive::audit_action::CLEAN,
                            json!({ "pr": pr, "head": entry.head, "route": "notice" }),
                        )),
                        // Its `rd-clean` row is written where the queue's
                        // answer is known, so the row records what happened.
                        rddrive::CleanRoute::Queue => out.clean_enqueue = Some(entry.head.clone()),
                    }
                    out.owed_text = Some(n.clone());
                    entry.owe_notice(&n, now);
                }
                (reviewdrive::DriveState::Held, Some(r)) => {
                    let refusal = out.refusal.clone();
                    // #2811 S5b: the provider that DECIDED this hold, off the
                    // facts `decide` saw rather than re-read from the map. A
                    // second read could report a provider that recovered
                    // between the decision and the notice, which is a line
                    // contradicting the hold beside it.
                    let provider = facts.provider_limited.clone().unwrap_or_default();
                    let n = rddrive::held_notice(
                        pr,
                        r,
                        &brief.held_facts(entry, limits, r, &messaged_by, &refusal, &provider),
                    );
                    if r == reviewdrive::HeldReason::ProviderLimit {
                        out.provider_limited = Some(provider.clone());
                    }
                    // The refusal rides the `rd-held` row rather than a
                    // `rd-refused` row of its own, and only when there is one:
                    // it is a detail OF this hold, and a separate row pushed
                    // where the refusal was learned would be a claim about an
                    // arc a later condition (age, a closed PR) could still
                    // outrank — §5.4's "a filter looking for the thing that
                    // happened must not match the thing that did not".
                    let mut detail =
                        json!({ "pr": pr, "reason": r.as_str(), "head": entry.head });
                    if !refusal.is_empty() {
                        detail["refusal"] = Value::String(refusal);
                    }
                    out.audits.push((rddrive::audit_action::HELD, detail));
                    // **The hold is recorded either way; only the LINE is
                    // conditional** (#3040 N1). A hold can repeat only after a
                    // resume — `transition` refuses a `held` -> `held`
                    // self-arc — and where that resume changed nothing the
                    // drive can observe, the second line says exactly what the
                    // first did. `announce_hold` owns the comparison and the
                    // stamp together, so a notice that is not built cannot
                    // silence the next one; `rd-held` above is written
                    // whatever it answers, because the hold HAPPENED and §5.4
                    // is a record of what happened, not of what was said.
                    //
                    // **The LINE is what the key digests** (rev-final round 3),
                    // so a repeat whose refusal changed — a hand-back that
                    // failed differently, a cap refusal quoting a roster that
                    // has moved — is a different key and announces. The tuple
                    // alone could not see any of that.
                    if entry.announce_hold(r, &n) {
                        // #2811 S5b: a provider limit's line does NOT go out
                        // per drive. `announce_hold` still runs, because it
                        // is what stamps the hold and keeps a repeat from
                        // re-announcing; only the destination changes. The
                        // aggregated line is built once in `rd_tick_group`
                        // from `out.provider_limited`, and this drive's own
                        // wording still reaches its board task there.
                        if r != reviewdrive::HeldReason::ProviderLimit {
                            // #3367 round-3 residual (rev-final on #3371): a
                            // HOLD's line is delivered fire-and-forget, so the
                            // report is folded in WITHOUT being taken. It is
                            // cleared by the delivery loop only once this line
                            // actually landed (`RdOut::report_folded`); a line
                            // that did not land leaves it for the next notice
                            // to carry. Taking it here dropped the worker's
                            // words from the pane whenever the delivery failed.
                            out.report_folded = entry.auto_report.is_some();
                            out.notices
                                .push(Self::rd_fold_text(n, entry.auto_report.as_deref()));
                        } else {
                            out.provider_note = Some(n);
                        }
                    } else {
                        out.audits.push((
                            rddrive::audit_action::HOLD_REPEATED,
                            json!({ "pr": pr, "reason": r.as_str(), "head": entry.head, "notice": n }),
                        ));
                    }
                    out.changed = true;
                }
                (reviewdrive::DriveState::Cancelled, _) => {
                    out.audits.push((rddrive::audit_action::CANCELLED, json!({ "pr": pr })));
                    // Replace-vs-augment, resolved as reconcile's is: #1871 B3's
                    // panes thread into the construction, and #1857's owe
                    // replaces the direct push rather than sitting beside it.
                    let panes = self.rd_surviving_panes(entry);
                    let n = rddrive::cancelled_notice(
                        pr,
                        rddrive::CancelCause::PrGone,
                        &panes,
                        &released_worker_session,
                    );
                    let n = Self::rd_fold_auto_report(entry, n);
                    out.owed_text = Some(n.clone());
                    entry.owe_notice(&n, now);
                }
                _ => {}
            }
        }
        out.on_behalf_of = on_behalf;
        Some(out)
    }
}
