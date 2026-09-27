//! Handing the PR back to its worker (§2.1's `fix-wait` row) and the driver's
//! one spawn or resume: dead-pane reads, the resume block and workspace, the
//! pane take-over, `rd_spawn`, and `rd_handback_failed`.
//!
//! Design note: `docs/design/review-driver.md`. Part of `rdtick/`, the driver's
//! registry wiring, which `tests/reviewdrive.rs` scans whole (see `mod.rs`).

use super::*;

impl OrchRegistry {
    /// Whether the pane this drive's worker is running in is **dead**, and what
    /// it went out saying (#1961).
    ///
    /// Not "the pane it resumed the worker into": since #1960 that pane may be
    /// one the drive TOOK OVER rather than opened, and the clause this returns
    /// lands in the orchestrator's own notice.
    ///
    /// §2.1's `fix-wait` row waits for a push or a `report`, and neither ever
    /// arrives from a pane that exited on boot: the measured incident had a
    /// resumed pane exit 5.4 seconds after spawn with `Invalid session ID`, and
    /// the drive then sat in `fix-wait` until `fix-stalled` — a whole fix
    /// timeout spent waiting on a process that was already gone, and the exit
    /// notice routed to the ORCHESTRATOR, which is the turn the driver exists to
    /// remove. This is what lets `fix-wait` learn it instead.
    ///
    /// **`Dead` and nothing else counts.** An agent this registry has no record
    /// of is "we could not check", which is not "it is dead" — the same
    /// asymmetry [`reviewdrive::DriveEntry::forget_dead_panes`] states, and the
    /// same fail-direction: an emptied map (a restart) would otherwise park
    /// every live drive in `fix-wait` on a hold about panes that are fine.
    pub(super) fn rd_pane_exit(&self, agent_id: &str) -> Option<String> {
        let a = self.agent(agent_id)?;
        if a.status != AgentStatus::Dead {
            return None;
        }
        // **What the tick OBSERVED, not a story about how the pane got there**
        // (rev-final premortem 2). "The pane it resumed the worker into" is
        // false whenever the drive REUSED a pane the orchestrator opened
        // (#1960) — and that composition is reachable on the driver's own
        // advice: a `cap-refused` notice says to kill an idle delegate, the
        // reused pane is idle and on that list, and killing it lands here. The
        // clause names the pane and, when orrerix knows who ended it, says so,
        // so an orchestrator that has just killed a pane reads a line matching
        // what it did rather than a resume that appears to have died.
        let tail = a.last_exit_tail.as_deref().unwrap_or("").trim().to_string();
        let how = match a.killed_by {
            Some(who) => format!(" (ended by {})", who.as_str()),
            None => String::new(),
        };
        Some(if tail.is_empty() {
            format!(
                "the pane its worker was running in ({agent_id}) is gone{how} with nothing \
                 reported, so there is no longer anything on this session to hand the fix to"
            )
        } else {
            format!(
                "the pane its worker was running in ({agent_id}) is gone{how} with nothing \
                 reported; last output: {}",
                tail_snippet(&tail, 200)
            )
        })
    }

    /// This lane's recorded pane, when it is **dead**, and who ended it
    /// (#2163) — `(pane, killed_by)`, with `killed_by` `None` where orrerix
    /// does not know.
    ///
    /// The lane-side twin of [`rd_pane_exit`](Self::rd_pane_exit), and it
    /// answers a pair rather than a sentence because its two consumers want
    /// different things: `decide_review_wait` wants the BOOLEAN (this lane
    /// cannot be waited for), and the `rd-lane-reopened` row wants the two
    /// facts an orchestrator that has just killed an idle delegate reads —
    /// which pane went, and whether it was its own doing.
    ///
    /// **`Dead` and nothing else counts**, on `rd_pane_exit`'s own argument:
    /// an agent this registry has no record of is "we could not check", and an
    /// emptied map (a restart) must not re-open every lane in the group.
    ///
    /// An empty recorded pane answers `None` — a lane seeded across a re-drive
    /// (#2153) carries a session and no pane, and there is nothing there to be
    /// dead.
    pub(super) fn rd_dead_lane_pane(
        &self,
        lane: &reviewdrive::LaneRecord,
    ) -> Option<(String, Option<&'static str>)> {
        let id = lane.agent.trim();
        if id.is_empty() {
            return None;
        }
        let a = self.agent(id)?;
        if a.status != AgentStatus::Dead {
            return None;
        }
        Some((id.to_string(), a.killed_by.map(|who| who.as_str())))
    }

    /// **Why the call refuses what the hand-back would refuse** (#2819 (g),
    /// S7): a `drive_review` whose worker session can never take a hand-back
    /// used to be ACCEPTED and then held `worker-unresumable` on every resume —
    /// measured on #2819's incident as three holds and three orchestrator turns
    /// for a PR that could never be handed back. This resolves the block the
    /// hand-back would resolve, from the same roster record, at the call.
    ///
    /// Returns the sentence the hold would have quoted — never a second wording
    /// of it: the orchestrator/manager sentences and the unknown-block sentence
    /// are the spawn guards' own ([`orchestrator_block_refusal`],
    /// [`manager_block_refusal`], [`unknown_block_refusal`]), shared rather
    /// than re-spelled, and the no-default-block case reads
    /// [`Guardrails::no_default_block_message`] the way the spawn does.
    /// `None` means a hand-back can at least be attempted.
    ///
    /// **A session this group has no record of is deliberately not refused
    /// here.** That is §5.1's passthrough arm, documented as accepted: there is
    /// no roster record to read a block off, and resolving is still not proving
    /// resumable. Its unresumability surfaces at the first hand-back as before,
    /// bounded instead by the second-failure park at the hand-back site.
    pub(super) fn rd_unhandbackable_block(&self, group: &GroupId, session: &str) -> Option<String> {
        let rec = self.session_identity_record(group, session)?;
        let g = self.group(group)?;
        // The same resolution `rd_handback` performs: a recorded block, or —
        // for a pre-#222 row that records a role and no block identity — the
        // class default, which is what the hand-back's `or_else` falls back to.
        let block = if rec.block.trim().is_empty() {
            g.guardrails.block_for(Role::Worker).map(|b| b.id.clone())
        } else {
            Some(rec.block.clone())
        };
        let why = match block.as_deref() {
            // Both halves empty — no recorded block and no worker block in the
            // roster — is the no-default-block refusal the hand-back's spawn
            // would die on.
            None => Some(g.guardrails.no_default_block_message(Role::Worker)),
            Some(id) => match g.guardrails.block(id).cloned() {
                None => {
                    let known: Vec<&str> =
                        g.guardrails.blocks.iter().map(|b| b.id.as_str()).collect();
                    Some(unknown_block_refusal(id, &known))
                }
                Some(b) if b.kind == Role::Orchestrator => {
                    Some(orchestrator_block_refusal(id))
                }
                Some(b) if b.kind == Role::Manager => Some(manager_block_refusal(id)),
                Some(_) => None,
            },
        };
        why
    }

    /// The block a driver-initiated resume of `session` must run under (#1961).
    ///
    /// **The driver resolves this rather than leaving it to be defaulted, and
    /// that is the whole of #1961's root cause.** `rd_spawn` calls
    /// `spawn_agent_bound`, which has no session-inheritance rule of its own —
    /// #254's lives in the MCP `spawn_agent` arm — so a `block: None` resume
    /// falls through to `block_for(Role::Worker)`, the roster's DEFAULT worker
    /// block. Every drive whose worker is not the default block therefore had
    /// its fix handed back to the wrong persona on the wrong CLI: measured, a
    /// `worker-adv` (Claude) session reopened by opencode, which exited 5.4s
    /// later with `Invalid session ID` and left the drive in `fix-wait` with a
    /// dead worker.
    ///
    /// **A session this group has no record of is refused**, not defaulted.
    /// §5.1 already says resolving a session id is not proving it resumable,
    /// and "we do not know this session's capability class" is #544's own rule
    /// (never guess one) reaching the driver: the refusal becomes
    /// `held(worker-unresumable)` naming what could not be established, which
    /// is a line an orchestrator can act on. Defaulting is what produced the
    /// pane that could not open.
    ///
    /// **An empty recorded block is a pre-#222 row** — a role and no block
    /// identity — and inherits that class's default block, exactly as the MCP
    /// arm's own pre-#222 branch does. `None` here means precisely that, never
    /// "we did not look".
    ///
    /// A recorded block that is no longer declared is left for
    /// `spawn_agent_bound` to refuse, so the sentence an orchestrator reads is
    /// the one that knows the roster (`unknown block "x". Blocks in this group:
    /// …`). That is a deliberate divergence from the session browser's rejoin,
    /// which DEGRADES a stale block to the class default: there a human is
    /// present and losing the persona beats losing the session, while here
    /// nobody is watching and a silently re-personad worker is #1961.
    fn rd_resume_block(&self, group: &GroupId, session: &str) -> Result<Option<String>, String> {
        let rec = self.session_identity_record(group, session).ok_or_else(|| {
            format!(
                "no roster record maps session {session:?} to a block, so the class it must \
                 resume under cannot be established — refusing to guess one"
            )
        })?;
        let block = rec.block.trim();
        Ok((!block.is_empty()).then(|| block.to_string()))
    }

    /// Hand the PR back to its worker (§2.1's `fix-wait` row), resuming the
    /// session `drive_review` resolved and recorded **under that session's own
    /// block** — see [`rd_resume_block`](Self::rd_resume_block).
    pub(super) fn rd_handback(
        &self,
        group: &GroupId,
        entry: &mut reviewdrive::DriveEntry,
        brief: &RdBrief,
        limits: &reviewdrive::DriveLimits,
        grace: bool,
    ) -> Result<(String, &'static str), String> {
        let text = self.rd_fix_brief(entry, brief, limits, grace);
        let session = entry.worker_session.clone();
        if session.is_empty() {
            return Err("this drive has no recorded worker session".into());
        }
        // **The block is resolved FIRST, and the reuse is filtered on it**
        // (#1960, corrected by rev-final B3). The first version asked for a pane
        // before resolving the block, on the premise that a session with a live
        // idle pane is already running under the right one "by construction". It
        // is not — the pre-#1961 driver minted exactly that pane, and where two
        // blocks share a CLI it is still alive and idle today. See
        // `idle_pane_on_session`.
        //
        // A pre-#222 roster row records a role and no block, and its identity IS
        // that class's default block — resolved here rather than left as `None`,
        // so the reuse filter has a value to compare AND
        // [`rd_resume_cwd`](Self::rd_resume_cwd) reads the CLI off the session's
        // own identity instead of falling back to the last-touched row
        // (rev-final N3). `None` survives only when the roster declares no
        // worker block at all, which `spawn_agent_bound` refuses with the
        // sentence that knows why.
        let block = self.rd_resume_block(group, &session)?.or_else(|| {
            self.group(group)
                .and_then(|g| g.guardrails.block_for(Role::Worker).map(|b| b.id.clone()))
        });
        // #2089: see the same clone in `rd_open_lane`.
        let on_behalf = entry.on_behalf_of.clone();
        let reused = block
            .as_deref()
            .and_then(|b| self.rd_reuse_pane(group, &on_behalf, &session, b, &text));
        // **And below the reuse arm, the TAKE-OVER arm** (#3203). The reuse
        // above answers only for a pane that is idle AND delivery-ready; every
        // other live pane on this session used to fall through to the spawn,
        // which is how PR #3198 ended with `w-2657`, `w-2659` and `w-2660` all
        // alive on one session and one worktree. What this restores is that a
        // live pane on the session gets the brief instead of a new pane being
        // opened beside it.
        //
        // **That is not an unconditional invariant, and saying it was is the
        // review's finding 1.** `rd_take_over_pane` falls through to the spawn
        // when the DELIVERY is refused, and one refusal is reachable without
        // anything being wrong: a pane whose queue is already at
        // `QUEUE_MAX_PER_PANE` (8) answers `Err`, so a hand-back landing on a
        // pane that far behind still opens a second pane on a live session.
        // The window is strictly narrower than the defect this fixes — the old
        // arm spawned at queue depth >= 1, this one only at depth 8 — and the
        // refusal is on the audit log as `rd-takeover-declined` rather than
        // being visible only as a fresh pane, which is the whole of what
        // #2089 asked of the reuse arm. It is a disclosed corner, pinned by
        // `a_takeover_refused_by_a_full_queue_says_so_and_falls_through`, not a
        // guarantee.
        //
        // Ordered after the reuse rather than replacing it, so #2089's
        // `rd-reuse-declined` rows are still written for the pane this then
        // takes over — the diagnosis of WHY a pane was not cleanly reusable is
        // what that row is for, and it is exactly as useful now that the
        // consequence is a take-over instead of a spawn.
        //
        // **The block filter is not dropped with the rest** — see
        // `live_pane_on_session`. The residual is a live pane on this session
        // under a DIFFERENT block, which still spawns: reusing one is #1961's
        // wrong persona on the wrong model, and the driver has minted no such
        // pane since that issue, so the state is reachable only through an
        // explicit `spawn_agent(block:, resume_session:)`.
        let taken_over = match (&reused, block.as_deref()) {
            (None, Some(b)) => self.rd_take_over_pane(group, &on_behalf, &session, b, &text),
            _ => None,
        };
        let (agent, pane) = match (reused, taken_over) {
            (Some(a), _) => (a, rddrive::handback_pane::REUSED),
            (None, Some(a)) => (a, rddrive::handback_pane::TAKEN_OVER),
            (None, None) => (
                self.rd_spawn(group, Role::Worker, block, Some(session), &text)?.id,
                rddrive::handback_pane::SPAWNED,
            ),
        };
        // Through the method, never a field write: the pane this supersedes is
        // still the drive's and still live (#1871 B2). Idempotent when the pane
        // reused IS the one already recorded — `retain_panes` drops it from the
        // superseded list on the way past — so a second hand-back into one pane
        // does not file that pane as its own predecessor.
        entry.record_worker_pane(&agent);
        Ok((agent, pane))
    }

    /// **Take over** the pane already running this drive's worker session when
    /// [`rd_reuse_pane`](Self::rd_reuse_pane) declined every candidate (#3203):
    /// type the hand-back brief into it and answer which pane took it. `None`
    /// means there is genuinely no live pane on this session under this block,
    /// so the caller opens one.
    ///
    /// The one thing this does that the reuse arm will not is deliver into a
    /// pane that is mid-turn or whose last delivery is unconfirmed. That is
    /// deliberate and is argued where the predicate lives
    /// (`live_pane_on_session`): the brief lands in the pane's queue and is read
    /// when the turn ends, which `fix-stalled` already bounds, and the
    /// alternative the driver used to take was a SECOND pane on the same
    /// worktree — measured as silent work loss on #3203.
    ///
    /// **A failed delivery answers `None` and falls through to the spawn, and
    /// it is AUDITED rather than silent** (review round 1, finding 1).
    /// `deliver_prompt` can refuse for reasons that say nothing about the drive
    /// — a pane that died between the lookup and the write — and the spawn is
    /// the path that already existed, which is `rd_reuse_pane`'s own argument.
    /// But one refusal is reachable with nothing wrong at all: a pane whose
    /// queue is at `QUEUE_MAX_PER_PANE` answers `Err`, and the fall-through
    /// then puts a second live pane on a session that has one — the very shape
    /// #3203 exists to stop, narrowed (the old arm spawned at depth >= 1) but
    /// not closed.
    ///
    /// So the refusal gets `rd-takeover-declined` (§5.4). Leaving it silent
    /// would make the one remaining route to a duplicate pane look identical on
    /// the log to "this session had no live pane" — which is exactly the
    /// indistinguishability #2089 added `rd-reuse-declined` to remove, one arm
    /// over.
    fn rd_take_over_pane(
        &self,
        group: &GroupId,
        on_behalf_of: &str,
        session: &str,
        block: &str,
        text: &str,
    ) -> Option<String> {
        let agent = self.live_pane_on_session(group, session, block)?;
        match self.deliver_prompt(&agent, text, brand::AUDIT_ACTOR, Delivery::MidSession) {
            Ok(_) => Some(agent),
            Err(why) => {
                self.rd_audit(group, on_behalf_of, rddrive::audit_action::TAKEOVER_DECLINED, json!({
                    "pane": agent,
                    "session": session,
                    "block": block,
                    "reason": why,
                }));
                None
            }
        }
    }

    /// The workspace a driver-initiated resume inherits.
    ///
    /// **`resolve_worker_resume_cwd` is the shared decision, and this is a
    /// caller of it rather than a second copy of it.** A worker or reviewer
    /// resume must never fall back to the main clone — that is the human's
    /// environment (#338/#359) — and which workspace it inherits instead (the
    /// roster's recorded cwd if it still exists, else the session located in its
    /// CLI's own store by id) is written once, in the function the `spawn_agent`
    /// MCP arm and the session browser both call.
    ///
    /// What is *not* shared is that arm's role classification, and that is the
    /// point rather than an omission: the arm has to decide whether the thing
    /// being resumed even needs a dedicated workspace, because an orchestrator
    /// may resume a planner. The driver resumes exactly two roles, both of which
    /// always need one, so the branch has no subject here. Re-deriving it would
    /// be a second answer to a containment question; leaving it out is not.
    fn rd_resume_cwd(
        &self,
        group: &GroupId,
        session: &str,
        block: Option<&str>,
    ) -> Result<Option<String>, String> {
        let Some(g) = self.group(group) else { return Err("no such group".into()) };
        // The last-touched roster record naming this session — the same record
        // the MCP arm's own cwd inheritance reads, chosen the same way.
        let owner = self
            .merged_records(group)
            .into_iter()
            .filter(|r| r.session.as_deref() == Some(session))
            .max_by_key(|r| r.updated_ms);
        let blk = block
            .and_then(|id| g.guardrails.block(id).cloned())
            .or_else(|| owner.as_ref().and_then(|o| g.guardrails.block(&o.block).cloned()));
        let cli = match blk.as_ref() {
            Some(b) => workflow::cli_of(b, &g.guardrails.agent_cli).to_string(),
            None => g.guardrails.agent_cli.clone(),
        };
        // #3443: a released lane's scratch worktree was reclaimed when its pane
        // died. Cut it again at the recorded path so the resume below finds
        // the workspace its session ran in, rather than refusing and opening a
        // fresh lane with no memory of its verdict. A no-op for anything but a
        // reclaimed reviewer worktree.
        if let Some(o) = owner.as_ref() {
            self.restore_reviewer_scratch_worktree(group, &o.cwd);
        }
        let db = self.opencode_db_path(group);
        let pi = self.pi_sessions_dir(group);
        resolve_worker_resume_cwd(
            &cli,
            session,
            owner.as_ref().map(|o| o.cwd.as_str()),
            &g.repo,
            Some(&db),
            Some(&pi),
        )
        .map(Some)
    }

    /// The one spawn or resume the driver performs.
    ///
    /// **A fresh lane gets a dedicated workspace, and that is #338/#359 rather
    /// than a preference.** `spawn_agent_ex` cuts a worktree only when
    /// `use_worktree` is set and no `cwd_override` is given; a fresh reviewer
    /// spawned with neither falls through to the per-role default, which is the
    /// group's **main clone** — the human's own checkout, and the exact conflict
    /// #359 exists to prevent (two reviewers, or a reviewer and the
    /// orchestrator's fetch traffic, contending on one checkout's state). The
    /// MCP `spawn_agent` surface defaults this ON for worker and reviewer kinds
    /// for the same reason, so the driver matches it rather than inventing a
    /// quieter default.
    ///
    /// A **resume** passes `false`, and it is not a second policy: the branch
    /// above is unreachable when `cwd_override` is `Some`, which a resume always
    /// is — [`rd_resume_cwd`](Self::rd_resume_cwd) resolves the workspace the
    /// session already had. Passing `false` there says "this spawn does not cut
    /// anything" rather than relying on a later branch to ignore a `true`.
    pub(super) fn rd_spawn(
        &self,
        group: &GroupId,
        role: Role,
        block: Option<String>,
        resume: Option<String>,
        task: &str,
    ) -> Result<AgentEntry, String> {
        let cwd = match resume.as_deref() {
            Some(s) => self.rd_resume_cwd(group, s, block.as_deref())?,
            None => None,
        };
        let fresh = cwd.is_none();
        self.spawn_agent_bound(
            group, role, block, "", task, fresh, None, None, resume, cwd, None, None,
        )
    }

    /// The one place a failed hand-back becomes `held(worker-unresumable)` or
    /// `held(cap-refused)` — shared by the arc into `fix-wait` and by #2811 S10's
    /// restart re-hand-back.
    ///
    /// Extracted rather than copied because it is not one decision but four that
    /// have to agree: which HeldReason the refusal text classifies to, the
    /// second-failure bookkeeping that turns a repeat into a decision, the
    /// refusal the NOTICE quotes, and the arc — whose result is what
    /// `out.advanced` may claim. A second copy is a second answer to each, and
    /// the design note's “no second way to hold” is exactly this.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn rd_handback_failed(
        &self,
        group: &GroupId,
        entry: &mut reviewdrive::DriveEntry,
        pr: u64,
        why: &str,
        out: &mut RdOut,
        now: u64,
    ) {
        // A worker that will not resume is §2.2's
        // `worker-unresumable`, learned exactly here —
        // §5.1 says so, and says the deferral is
        // deliberate: resolving a session at drive time
        // is not the same as proving it resumable.
        // The refusal reaches the NOTICE as well as the
        // audit row (#1961). It used to reach the audit
        // alone, and the pane got one fixed sentence
        // diagnosing a session that no longer resolves
        // — for a block that had left the roster, for a
        // pane that opened and died on `Invalid session
        // ID`, and (#1960) for a cap refusal.
        //
        // **And a cap refusal is now its own reason**
        // (#1960): the session resolves fine, a slot is
        // what is exhausted, and the two remedies are
        // different actions. Classified on the shared
        // literal `live_cap_refusal` writes, so the
        // producer and this reader cannot drift.
        //
        // **A SECOND identical failure says so** (#2555
        // item 2, S7). The refusal is recorded per drive
        // — the session it failed for and the failure
        // line — and a repeat with the same pair is told
        // apart from the first: "second time" prefixes
        // the quoted refusal, so the notice an
        // orchestrator reads after resuming and losing
        // again is a decision (re-point the drive, or
        // cancel) rather than the same reflex the first
        // notice invited. The reason still names the
        // failure's own CLASS; what recurs is the fact.
        // In-memory (the field's doc carries the bounded
        // restart consequence), and cleared by the `Ok`
        // arm above, so a recovery restarts the count.
        let prior = self
            .rd_handback_fails
            .lock_safe()
            .get(&(group.clone(), pr))
            .cloned();
        let second = prior
            .as_ref()
            .map(|(s, w)| (s.as_str(), w.as_str()))
            == Some((entry.worker_session.as_str(), why));
        self.rd_handback_fails.lock_safe().insert(
            (group.clone(), pr),
            (entry.worker_session.clone(), why.to_string()),
        );
        let refusal = if second {
            format!("second time: {why}")
        } else {
            why.to_string()
        };
        let reason = if super::is_live_cap_refusal(why) {
            reviewdrive::HeldReason::CapRefused
        } else {
            reviewdrive::HeldReason::WorkerUnresumable
        };
        out.refusal = refusal;
        out.audits.push((
            rddrive::audit_action::REFUSED,
            json!({ "pr": pr, "reason": reason.as_str(),
                    "detail": why }),
        ));
        // **The arc's result decides what the notice may
        // claim.** Discarding it let `out.advanced`
        // announce a hold the entry had not taken — the
        // notice says parked, the file says `fix-wait`,
        // and the next tick hands back again. A value
        // computed and dropped at a boundary, which is
        // the axis this round is about.
        match entry.advance(
            reviewdrive::DriveState::Held,
            Some(reason),
            None,
            now,
        ) {
            Ok(()) => {
                out.advanced =
                    Some((reviewdrive::DriveState::Held, Some(reason)));
            }
            Err(bad) => {
                // Unreachable — `fix-wait -> held` is arc
                // 12 — and handled rather than claimed:
                // the drive stays where it is and says so.
                out.advanced = None;
                out.audits.push((
                    rddrive::audit_action::REFUSED,
                    json!({ "pr": pr, "reason": "invalid-transition",
                            "detail": bad.to_string() }),
                ));
            }
        }
    }
}
