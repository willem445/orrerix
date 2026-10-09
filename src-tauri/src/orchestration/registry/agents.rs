//! The live agents: token and id lookup, pane liveness on a session, the
//! roster read (`list_agents`), kill, rename and focus, death and exit
//! (`mark_dead`, `on_pty_exit`), and the reviewer scratch worktrees an exit
//! reclaims, as an `impl OrchRegistry` block (#3498). The design is
//! `docs/design/orchestration.md`.

use super::*;

impl OrchRegistry {
    pub fn resolve_token(&self, token: &str) -> Option<Caller> {
        let id = self.by_token.lock_safe().get(token).cloned()?;
        let a = {
            let agents = self.agents.lock_safe();
            let a = agents.get(&id)?;
            if a.status == AgentStatus::Dead {
                return None;
            }
            a.clone()
        };
        // Dropped the `agents` lock before taking `groups` — resolving the
        // spawning block's role_hint needs both, and locking them together
        // would pin a lock order no other call site promises to respect.
        let role_hint = self.group(&a.group)
            .and_then(|g| g.guardrails.block(&a.block).and_then(|b| b.role_hint.clone()));
        Some(Caller { agent_id: a.id, group: a.group, role: a.role, role_hint })
    }

    pub fn agent(&self, id: &str) -> Option<AgentEntry> {
        self.agents.lock_safe().get(id).cloned()
    }

    /// Which agent CLI to assume for delivery/usage bookkeeping. For every
    /// orchestration-group agent this is the resolution keyed on THIS agent's
    /// own block (`Guardrails::cli_for_block`) — not on its class's default
    /// block, which is what this used to ask and is a different question the
    /// moment a roster declares two blocks of one kind (#2167); a **solo** pane
    /// instead carries its own CLI directly (`AgentEntry.solo_cli`), since
    /// `__solo__` is one shared group across panes that may be running
    /// different CLIs — a group-level lookup can't answer this for them. Falls
    /// back to `"claude"` only when neither source has an answer (should not
    /// happen for a live agent).
    pub(in crate::orchestration) fn cli_for_agent(&self, a: &AgentEntry) -> String {
        if let Some(cli) = &a.solo_cli {
            return cli.clone();
        }
        self.group(&a.group)
            .map(|g| g.guardrails.cli_for_block(&a.block, a.role).to_string())
            .unwrap_or_else(|| "claude".to_string())
    }

    /// Does this group already have a live manager pane? (#1161 M3, review N2.)
    ///
    /// Takes the agents lock, so it is for callers that do not already hold it —
    /// `open_manager_pane_at_launch`'s failure arm, which uses it to tell "could
    /// not open" apart from "one is already open" without matching refusal text.
    /// `spawn_agent_bound`'s own singleton check runs under its already-held
    /// guard and shares the rule through [`is_live_manager_of`], not through
    /// this wrapper.
    ///
    /// Nothing to do with `max_agents`: a manager is EXEMPT from the cap
    /// (`counts_against_max_agents`), so this asks a different question of a
    /// different rule, and the two must not be read as one.
    pub(in crate::orchestration) fn has_live_manager(&self, group: &GroupId) -> bool {
        self.agents.lock_safe().values().any(|a| is_live_manager_of(a, group))
    }

    /// How many live panes count against this group's `max_agents`.
    ///
    /// **Two classes are exempt, and they are the two panes the orchestrator
    /// does not own**: itself, and the group's manager (#1161 M3, decision
    /// D3). The cap exists to contain DELEGATE fan-out — the orchestrator
    /// deciding to open five more workers — and a manager is not something it
    /// decides at all: the repo's workflow file declares it and loomux opens it
    /// at launch for the human. Counting it would make the human's own
    /// interface competable with a worker slot, and (on a cap of 1) would make
    /// a group with a manager unable to spawn any worker at all.
    ///
    /// [`counts_against_max_agents`] is the one expression; every reader of
    /// this rule calls it rather than re-spelling the pair.
    pub(in crate::orchestration) fn live_delegate_count(&self, group: &GroupId) -> u32 {
        self.agents
            .lock_safe()
            .values()
            .filter(|a| a.group == group && counts_against_max_agents(a.role) && a.status != AgentStatus::Dead)
            .count() as u32
    }

    /// A **live, idle, typeable, ready** pane in `group` already running
    /// `session` (#1960, #2089) — what the review driver resumes INTO instead of
    /// opening a second pane on the same conversation.
    ///
    /// Five conditions, each load-bearing:
    ///
    /// - **not `Dead`**, or the "reuse" is a delivery into a pane that is gone;
    /// - **idle** (`idle_since_ms.is_some()`, the same signal the idle reaper
    ///   kills on and the cap-refusal roster reports) — a pane mid-turn would
    ///   take the brief behind whatever it is doing, and "is this agent
    ///   mid-thought?" is not a question a driver may answer;
    /// - **has a pane** — `deliver_prompt` resolves `pty_id` before it does
    ///   anything else and refuses an agent with none, so an agent registered
    ///   without a terminal is not something to type into;
    /// - **running the block the caller asked for**;
    /// - **ready** — [`pane_delivery_readiness`], below.
    ///
    /// **That last one is #1961 one arm over, and it is not redundant**
    /// (rev-final B3). The first version of this took the pane on the other
    /// three and justified skipping the block because "a session with a live
    /// idle pane is already running under the right block by construction". It
    /// is not: a pane can be alive, idle, typeable and on the WRONG block, and
    /// the state that produces one is the very defect #1961 fixes — the
    /// pre-#1961 driver opened a default-block pane on a non-default session,
    /// and where the two blocks share a CLI that pane did not die on `Invalid
    /// session ID`, it opened, went idle, and is still there.
    /// `spawn_agent(block:, resume_session:)` mints the same thing
    /// deliberately, with no legacy required. Reusing one would hand the fix to
    /// the wrong persona on the wrong model while `rd_resume_block` never ran —
    /// the hand-back failing to run under the session's own block, which is
    /// exactly what that issue exists to stop.
    ///
    /// Newest first, because when the drive already holds a live idle pane on
    /// this session that pane is the drive's own current one, and speaking to
    /// the pane it last spoke to is the continuity a resume is for.
    ///
    /// **Since #2089 that is a PREFERENCE among eligible panes rather than a
    /// pick**, and the distinction is why this is a sort and not a `max_by_key`:
    /// readiness can refuse the newest candidate, and the next one down is still
    /// a live idle pane on this same conversation under this same block, which
    /// is a better answer than opening a second pane on it. So the arm takes the
    /// newest READY pane, and only an empty list opens one.
    ///
    /// **The key is `(started_ms, id)` rather than `started_ms` alone, and the
    /// second half is not decoration** (rev-final round 2, premortem 1).
    /// `started_ms` is a wall-clock millisecond, so two panes registered inside
    /// one — a rapid recovery, or a fallback spawn landing beside a pane that
    /// was just re-registered — TIE, and `max_by_key` over `HashMap::values()`
    /// then resolves the tie by iteration order, differently between runs. The
    /// wrong-persona outcome is excluded either way by the block filter above;
    /// what a tie loses is exactly the continuity this ordering exists for, and
    /// non-deterministically. The id is a total, stable tiebreak.
    ///
    /// **`idle_since_ms.is_some()` means "the reaper would call this idle", not
    /// "the CLI is at a prompt", and the readiness condition is what supplies
    /// the second half** (#2089; the residual was stated here by rev-final round
    /// 2, premortem 2, and deferred out of #1967). A pane parked on an
    /// auto-compact, a permission prompt or a tool-approval dialog is idle by
    /// this signal, and `deliver_prompt` will admit the brief into its queue and
    /// answer `Ok` — so the fallback-to-spawn never fires and the brief waits
    /// for the pane to come back.
    ///
    /// **The two conditions are a CONJUNCTION, and neither half is the other's
    /// restatement.** #2089 asked for the idle test to be replaced; it is
    /// narrowed instead, because delivery-readiness alone is exactly what a pane
    /// MID-TURN looks like — it took a brief, the brief confirmed, and its queue
    /// is empty because the CLI is now thinking. Dropping `idle_since_ms` would
    /// make the driver type into a working delegate, which the second bullet
    /// above forbids. `idle_since_ms` answers "does this agent have work"; the
    /// readiness test answers "will what I type be read"; the reuse needs both.
    ///
    /// **What it still cannot see** is [`pane_delivery_readiness`]'s own
    /// residual: a dialog raised AFTER a confirmed delivery, with nothing queued
    /// behind it. That case is bounded exactly as the whole class was before —
    /// `fix-wait` holds on `fix-stalled`, `review-wait` on `lane-stalled`, both
    /// naming the pane, so the drive degrades to a named hold rather than to
    /// silence. Narrowing it further means reading the attention machinery from
    /// the driver, which is a judgment about a pane's screen; §3 keeps those out
    /// of the driver, so it stays disclosed and bounded rather than guessed.
    ///
    /// **Fail direction.** A pane whose last delivery landed but resolved
    /// `Pending` (a busy CLI that no tier decided for) reads NOT ready, and the
    /// caller opens a fresh pane. That is the pre-#1960 cost of one round, not a
    /// regression below it — the predicate fails toward a spare pane, never
    /// toward a brief nobody reads.
    pub(in crate::orchestration) fn idle_pane_on_session(
        &self,
        group: &GroupId,
        session: &str,
        block: &str,
    ) -> ReusablePane {
        // The candidates are collected under `agents` and that guard is DROPPED
        // before any readiness read. `queues` is rank 400 and `agents` 510
        // (`docs/design/lock-order.md` §4), so asking a candidate's queue depth
        // from inside the filter would take them in the inverted order.
        let mut candidates: Vec<(u64, String, u32)> = self
            .agents
            .lock_safe()
            .values()
            .filter(|a| {
                a.group == *group
                    && a.status != AgentStatus::Dead
                    && a.session_id.as_deref() == Some(session)
                    && a.block == block
                    && a.idle_since_ms.is_some()
            })
            .filter_map(|a| Some((a.started_ms, a.id.clone(), a.pty_id?)))
            .collect();
        // Newest first — the ordering argument above, with `max_by_key` written
        // out as a sort because readiness can now reject the front of the list
        // and the next candidate is still a pane on this same conversation.
        candidates.sort_by(|x, y| (y.0, &y.1).cmp(&(x.0, &x.1)));
        let mut declined = Vec::new();
        for (_started, id, pty) in candidates {
            match self.pane_readiness(pty) {
                None => return ReusablePane { agent: Some(id), declined },
                Some(why) => declined.push((id, why)),
            }
        }
        ReusablePane { agent: None, declined }
    }

    /// **Every live pane in `group` sitting on `session`, oldest first** — what
    /// a drive records as the panes it was STARTED ON
    /// ([`DriveEntry::founding_panes`](loomux_engine::reviewdrive::DriveEntry::founding_panes),
    /// #3250).
    ///
    /// The plural of [`Self::live_pane_on_session`] minus its block filter, and
    /// the filter is dropped for the opposite reason to the one that keeps it
    /// there. That function picks a pane to TYPE INTO, where the wrong block is
    /// the wrong persona on the wrong model (#1961); this one only proposes
    /// panes to a barrier that then refuses everything that is not idle, alive,
    /// terminal-bound and a driven delegate's role. A pane on this drive's
    /// worker session under some other block is still a pane on the
    /// conversation the drive has just finished with.
    ///
    /// An empty `session` answers nothing rather than matching every agent that
    /// has none — the fail-closed direction, and the one a drive record with a
    /// blank session would otherwise take straight through the barrier.
    ///
    /// Ordered `(started_ms, id)` ascending, with the same tiebreak as its
    /// singular twin: two panes registered inside one wall-clock millisecond
    /// would otherwise be ordered by `HashMap` iteration order, differently
    /// between runs.
    ///
    /// **That order is this list's, and it does not survive being merged**
    /// (review round 1, finding 2): at the release the recorded founding panes
    /// are appended to `owned_panes`, whose members were all minted by a
    /// hand-back and so all post-date them, and the release therefore re-sorts
    /// the merged list. The claim "the audit rows read as the history they are"
    /// belongs there, not here.
    pub(in crate::orchestration) fn live_panes_on_session(&self, group: &GroupId, session: &str) -> Vec<String> {
        if session.trim().is_empty() {
            return Vec::new();
        }
        let mut panes: Vec<(u64, String)> = self
            .agents
            .lock_safe()
            .values()
            .filter(|a| {
                a.group == *group
                    && a.status != AgentStatus::Dead
                    && a.session_id.as_deref() == Some(session)
                    && a.pty_id.is_some()
            })
            .map(|a| (a.started_ms, a.id.clone()))
            .collect();
        panes.sort_by(|x, y| (x.0, &x.1).cmp(&(y.0, &y.1)));
        panes.into_iter().map(|(_started, id)| id).collect()
    }

    /// The newest **live** pane in `group` running `session` under `block` —
    /// what a hand-back TAKES OVER when `idle_pane_on_session` found nothing
    /// eligible to reuse (#3203).
    ///
    /// Four conditions, and the two that are missing are the point:
    ///
    /// - **not `Dead`**, or the take-over is a delivery into a pane that is gone;
    /// - **on `session`**, which is what makes this a take-over rather than a
    ///   second conversation;
    /// - **running `block`** — #1961 one arm over, and the one filter this does
    ///   NOT drop: a wrong-block pane is the wrong persona on the wrong model,
    ///   and reusing one is the defect that issue exists to stop. The residual
    ///   is stated at the call site: a live pane on this session under a
    ///   DIFFERENT block still spawns, and the driver mints no such pane.
    /// - **has a pane**, since `deliver_prompt` resolves `pty_id` before it does
    ///   anything else and refuses an agent with none.
    ///
    /// **What is dropped is `idle_since_ms` and the readiness test, and that is
    /// #3203's whole retraction.** `idle_pane_on_session` refuses a pane that is
    /// mid-turn or not delivery-ready on the argument that the brief would land
    /// behind whatever the pane is doing — which is true, and was the better
    /// trade while the alternative was a pane that reads it now. It is not the
    /// alternative on the hand-back path: measured on PR #3198, two red heads
    /// arriving inside one fix produced `w-2659` and then `w-2660` on the
    /// session `w-2657` was still sitting on, three panes writing one worktree,
    /// with the third pane correctly reporting commits it had not authored.
    /// Landing behind a turn costs latency that `fix-stalled` already bounds;
    /// a second pane costs the worktree. So the fail direction flips here and
    /// only here — `rd_open_lane`'s reuse is
    /// unchanged, because a duplicate reviewer costs a review and not a
    /// checkout.
    ///
    /// **Newest first, `(started_ms, id)`**, for `idle_pane_on_session`'s own
    /// reason and with its own tiebreak: `started_ms` is a wall-clock
    /// millisecond, so two panes registered inside one would otherwise be
    /// ordered by `HashMap` iteration order, differently between runs.
    pub(in crate::orchestration) fn live_pane_on_session(
        &self,
        group: &GroupId,
        session: &str,
        block: &str,
    ) -> Option<String> {
        let mut candidates: Vec<(u64, String)> = self
            .agents
            .lock_safe()
            .values()
            .filter(|a| {
                a.group == *group
                    && a.status != AgentStatus::Dead
                    && a.session_id.as_deref() == Some(session)
                    && a.block == block
                    && a.pty_id.is_some()
            })
            .map(|a| (a.started_ms, a.id.clone()))
            .collect();
        candidates.sort_by(|x, y| (y.0, &y.1).cmp(&(x.0, &x.1)));
        candidates.into_iter().next().map(|(_started, id)| id)
    }

    /// [`pane_delivery_readiness`] against the live registry — the impure half,
    /// split out for that function's reason (#2089).
    ///
    /// Both reads are of state the delivery machinery already keeps: the pane's
    /// own queue, and the outcome recorded for the last delivery to it. Nothing
    /// here looks at the pane.
    ///
    /// `pub` so a seam test can state its own fixture's premise — that a pane it
    /// built IS ready — without reaching into `last_delivery`, which stays
    /// private (`DeliveryConfirmation`'s doc).
    #[doc(hidden)] // pub for integration tests
    pub fn pane_readiness(&self, pty_id: u32) -> Option<PaneNotReady> {
        pane_delivery_readiness(
            self.queue_depth(pty_id),
            recorded_confirmed(&self.last_delivery, pty_id),
        )
    }

    /// Human-readable roster of the group's live delegates (workers, reviewers,
    /// planners — the orchestrator and the manager are exempt from the cap) for
    /// the cap-rejection guardrail message (#203). Locks `agents`; the race-safe
    /// cap check in `spawn_agent` already holds that lock, so it calls
    /// [`format_delegate_roster`] directly against its guard instead of this.
    ///
    /// Filtered by the SAME predicate as [`Self::live_delegate_count`] (#1161
    /// M3): this message answers "which panes are holding the slots", and a
    /// pane the cap does not count is not holding one — naming it here would
    /// point a refused orchestrator at a pane it must not reuse or kill.
    pub(in crate::orchestration) fn live_delegate_roster(&self, group: &GroupId) -> String {
        // #2811 S2: the NON-BLOCKING read, and taken before the `agents` lock.
        // This is reachable from inside the driver's own tick, which holds
        // `rd_state_lock` across its spawns — see `rd_driven_panes`'s locking
        // note for why that means `try` here and a blocking acquire on the two
        // guard surfaces.
        let driven = self.rd_driven_panes_now(group);
        let rows = self
            .agents
            .lock_safe()
            .values()
            .filter(|a| a.group == group && counts_against_max_agents(a.role) && a.status != AgentStatus::Dead)
            .map(|a| {
                (
                    a.id.clone(),
                    a.role.as_str(),
                    a.idle_since_ms.is_some(),
                    driven.get(&a.id).map(|(pr, _)| *pr),
                )
            })
            .collect();
        format_delegate_roster(rows)
    }

    pub fn list_agents(&self, group: &GroupId) -> Value {
        // #2811 S2: which of these panes a live review drive owns. Read BEFORE
        // the `agents` lock and never under it — the driver holds
        // `rd_state_lock` and then reaches `agents` through
        // [`Self::release_driven_pane`], so the reverse nesting would invert an
        // ordering that exists in production. See
        // [`Self::rd_driven_panes`] for why it is one read and what it excludes.
        // `Err` is "orrerix could not read the drive record", which is NOT
        // "nothing is driven" — see the `driven_by` comment on the row below.
        let driven = self.rd_driven_panes(group);
        let agents = self.agents.lock_safe();
        let mut list: Vec<Value> = agents
            .values()
            .filter(|a| a.group == group)
            .map(|a| {
                // Registry hygiene (#106 → #851): a dead agent keeps its
                // identity (id/name/role/session/status/cwd) so the
                // orchestrator can still resume its session. #106 had dead
                // rows shed `task` entirely because full briefs accumulating
                // across a run pushed one group's roster to ~86KB; #851
                // replaces that all-or-nothing cut with a fixed-width excerpt
                // applied to EVERY row, alive or dead, so a dead row keeps a
                // hint of what it was doing instead of nothing at all, while
                // the excerpt cap bounds the cost the same way the old omit
                // did. The full brief is unaffected — it's still durable in
                // the audit log and the task board; this is only the roster
                // view.
                json!({
                    "id": a.id, "name": a.name, "role": a.role,
                    // #222: which block this agent is. An orchestrator reading
                    // its roster needs the identity, not just the class — three
                    // reviewers all report `role: reviewer`.
                    "block": a.block,
                    "status": a.status,
                    "session": a.session_id, "cwd": a.cwd,
                    "idle_since_ms": a.idle_since_ms,
                    "task": task_excerpt(&a.task, TASK_EXCERPT_CHARS),
                    // #2811 S2: `"#<pr>"` when a live review drive is currently
                    // using this pane as its worker or one of its lanes,
                    // `null` when nothing is, and `"unreadable"` when orrerix
                    // could not read this group's drive record at all. The KEY
                    // IS ALWAYS PRESENT, the way `wip` and `current_sprint` are
                    // on the board read, so "not driven" never has to be told
                    // apart from "this build does not report it".
                    //
                    // **Three states, not two** (rev round 1, N1): `null` is a
                    // CLAIM — orrerix looked and nothing owns this pane — and
                    // publishing it off a read that FAILED would put a false
                    // claim on the roster the `kill_agent` refusal is derived
                    // from, which is the one place a reader checks before
                    // killing. The MCP arm refuses outright on the same fact;
                    // a roster read is not a guard, so it reports rather than
                    // refuses — but it must not lie.
                    "driven_by": match &driven {
                        Ok(d) => d.get(&a.id).map(|(pr, _)| format!("#{pr}")),
                        Err(()) => Some("unreadable".to_string()),
                    },
                })
            })
            .collect();
        list.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        json!(list)
    }

    /// The `get_output` MCP tool: what an orchestrator sees when it looks at a
    /// pane.
    ///
    /// **This one keeps the whole-ring read, and that is the argued answer, not
    /// an oversight (#743 S7).** The census named it alongside the usage poll's
    /// statusline read as the other surviving unbounded `output_tail`
    /// (comment 5162020708 §4.4), and the two resolve opposite ways because
    /// they are different kinds of read:
    ///
    /// - It is not cadenced. One call per explicit `get_output`, from an agent
    ///   that decided to look — not a timer, and nothing on the webview thread
    ///   waits for it. `performance.md` INV-5 is about latency-sensitive
    ///   paths; this is not one.
    /// - Truncating it would change the ANSWER, not just the cost. #520
    ///   replays the raw escape stream onto a composed grid instead of
    ///   stripping it, precisely so a TUI's redraw churn overwrites itself
    ///   here exactly as it did on the human's screen. A replay that starts
    ///   mid-stream starts with no cursor position, no scroll region and no
    ///   attribute state, so any cell whose last paint fell outside the window
    ///   comes back blank — a static screen that has not been fully repainted
    ///   in a while would be reported to the orchestrator as empty. That is
    ///   the behaviour question #725 flagged, and the fidelity the tool is
    ///   for.
    pub fn agent_output_tail(&self, agent_id: &str, lines: usize) -> Result<String, String> {
        let a = self.agent(agent_id).ok_or("unknown agent")?;
        let pty_id = a.pty_id.ok_or("agent has no terminal")?;
        let app = self.app.lock_safe().clone().ok_or("no app handle")?;
        let ptys = app.state::<crate::pty::PtyManager>();
        // #520: replay the raw ring onto a composed grid at the pane's real
        // geometry instead of deleting the escapes and handing over the write
        // stream. A TUI's redraw churn overwrites itself here exactly as it
        // does on the human's screen, so it never reaches the caller's
        // context. `strip_ansi` stays the path for every other reader.
        let (cols, rows) = ptys
            .size(pty_id)
            .unwrap_or((termgrid::DEFAULT_COLS, termgrid::DEFAULT_ROWS));
        let live = ptys.output_tail(pty_id).map(|raw| termgrid::render_screen(&raw, cols, rows));
        // The exit-tail fallback (#281) was captured as already-stripped text,
        // not raw bytes — there is no escape stream left to replay, so it
        // still gets the line-collapse treatment in `format_output_tail` only.
        let text = resolve_output_text(live, a.last_exit_tail.as_deref())?;
        Ok(format_output_tail(&text, lines))
    }

    /// Terminate `agent_id` at the ORCHESTRATOR's request (the `kill_agent`
    /// MCP tool). See [`Self::kill_agent_as`] for the initiator-recording
    /// half and why it matters (#533-B).
    pub fn kill_agent(&self, agent_id: &str) -> Result<(), String> {
        self.kill_agent_as(agent_id, ExitInitiator::Orchestrator)
    }

    /// `kill_agent`, with the INITIATOR named explicitly (#533-B).
    ///
    /// The initiator is stamped onto the agent's own record BEFORE the pty
    /// is touched, so the exit that follows — milliseconds later, on the
    /// pty waiter thread — reads a recorded fact rather than inferring one
    /// from `expected`/an exit code (see [`exit_notice_route`]). Stamping
    /// first is what makes the ordering safe: `PtyManager::kill` can have
    /// the waiter in `on_pty_exit` before this function returns, and a
    /// record written after the kill could lose that race and misroute the
    /// notice to a prompt.
    ///
    /// **Never stamps an initiator for a kill it does not perform (rev-13
    /// F1).** An agent still in the spawn-to-bind window exists in the
    /// registry with `pty_id: None` (`spawn_agent` inserts the entry, then
    /// blocks on the bind). The pre-fix shape stamped unconditionally and
    /// killed conditionally, so a `kill_agent` landing in that window
    /// returned `Ok(())`, killed nothing, and left `killed_by` set — and
    /// since the stamp is first-writer-wins and never cleared, EVERY later
    /// exit of that pane, including a genuine panic, routed `AuditOnly` and
    /// the orchestrator was never told. That is the exact failure #533-B
    /// exists to prevent (demotion swallowing an exit nobody initiated), so
    /// it does not get to ship inside the change that introduces demotion,
    /// however narrow the window is to hit.
    ///
    /// The no-pty case is now a truthful `Err` rather than a silent
    /// `Ok(())` for a no-op: the caller asked for a kill that did not and
    /// could not happen, and the MCP `kill_agent` handler passes the
    /// message straight back to the orchestrator. Nothing is lost by this —
    /// the pre-fix `Ok(())` never killed anything either; it just said it
    /// had.
    pub fn kill_agent_as(&self, agent_id: &str, initiator: ExitInitiator) -> Result<(), String> {
        let a = self.agent(agent_id).ok_or("unknown agent")?;
        if a.role == Role::Orchestrator {
            return Err("refusing to kill the orchestrator; close its pane instead".into());
        }
        // #2519, and it is a hole THIS slice would otherwise open rather than a
        // pre-existing one: `kill_agent` is on the lead's enumerated surface,
        // `require_in_group` passes for the caller's own id, and nothing below
        // this line distinguishes a pane from its opener — so without this a
        // lead could end the human's own pane from inside it, with the human
        // watching. Spelled as its own arm rather than folded into
        // `Role::is_root` above so each class keeps the refusal that names it;
        // the shape is `send_prompt`'s "cannot send a prompt to yourself",
        // generalized from self-targeting to the whole class, because the pane
        // is the human's whether the caller is the lead or one of its helpers.
        if a.role == Role::Lead {
            return Err("refusing to kill a lead pane; it is the human's own, and its helpers \
                        are not its owner. Close the pane instead"
                .into());
        }
        // #3679, the same hole for the same reason: `kill_agent` is on the
        // quick root's surface so it can end a helper, and nothing below tells
        // a pane from its opener. A root that killed itself — or a helper that
        // killed the root — would end a task with no report, and take every
        // other helper with it (`on_pty_exit`). A task ends by the root's
        // `report` or at its bounds; the run ends by the human, who stops it
        // or closes its pane (#3723).
        if a.role == Role::Quick {
            return Err("refusing to kill a quick run's own agent; it ends a task by reporting, \
                        and a helper is not its owner. The human ends the run: by stopping it \
                        from the pane menu or the Quick task form, or by closing its pane"
                .into());
        }
        // Checked BEFORE the app handle and before the stamp: with no pty
        // there is nothing to kill, so there is nothing to attribute either.
        // #2850 S3b: a structured pane is killed through its own ladder —
        // interrupt (end the turn rather than abandon it mid-tool), close
        // stdin, bounded wait, kill, reap. `PtyManager::kill` is not involved
        // and cannot be: this pane has no `pty_id`, which is the point of it
        // never having one.
        //
        // Ahead of the `pty_id` check below, because that check would
        // otherwise refuse a live structured pane as "still binding" — true of
        // its pty_id, false of the pane, and the kind of asymmetry that leaves
        // an agent unkillable.
        if self.structured_pane(agent_id).is_some() {
            self.record_exit_initiator(agent_id, initiator);
            self.kill_structured_pane(agent_id)?;
            self.audit(&a.group, brand::AUDIT_ACTOR, "agent-kill", json!({
                "agent": agent_id,
                "initiator": initiator.as_str(),
                "kind": "structured",
            }));
            return Ok(());
        }
        let Some(pty) = a.pty_id else {
            self.audit(&a.group, brand::AUDIT_ACTOR, "agent-kill-noop", json!({
                "agent": agent_id,
                "initiator": initiator.as_str(),
                "reason": "no-terminal-yet",
            }));
            return Err(format!(
                "agent {agent_id} has no terminal yet (still binding) — nothing was killed, \
                 and no exit initiator was recorded"
            ));
        };
        let app = self.app.lock_safe().clone().ok_or("no app handle")?;
        self.record_exit_initiator(agent_id, initiator);
        app.state::<crate::pty::PtyManager>().kill(pty);
        self.audit(&a.group, brand::AUDIT_ACTOR, "agent-kill",
            json!({ "agent": agent_id, "initiator": initiator.as_str() }));
        Ok(())
    }

    /// **Release one pane the review driver no longer needs** (#2501) — the
    /// registry half of `reviewdrive::releasable`, and the ONLY route by which
    /// the driver may end a pane's life.
    ///
    /// # Why this exists rather than the driver calling `kill_agent`
    ///
    /// `review-driver.md` §3.1 item 5 was a closed guarantee — the driver may
    /// never kill a pane — enforced by a default-deny source scan over the
    /// driver's `rddrive.rs`, `reviewdrive/` and `rdtick/` that denies `kill_agent` and the reaper entry
    /// points. #2501 narrows the guarantee to a lane whose verdict is recorded
    /// at the drive's current head and a worker whose `report` the drive has
    /// consumed; #2811 S1 adds either of them at the step that ENDS the drive.
    /// The closed set is `reviewdrive::ReleaseReason` — three variants — and
    /// naming the arity here rather than restating it was the mistake #2811 S1
    /// had to correct on five other surfaces, so this doc points at the enum.
    /// The honest way to narrow a guarantee is to give the driver exactly one
    /// named capability rather than to let it reach a kill primitive by some
    /// other name: the scan keeps denying `kill_agent`, `kill_agent_as`,
    /// `mark_dead` and `reap_idle_agents` inside those files, and permits this
    /// one call, whose site count it pins. A kill the driver reached any other
    /// way still fails the scan, which is what makes "only the states
    /// `ReleaseReason` spells" reviewable instead of merely intended.
    ///
    /// This function lives here, beside `kill_agent_as` and `mark_dead`, for the
    /// same reason: it is a lifecycle capability, and a barrier a caller must
    /// pass is not one the caller gets to define.
    ///
    /// # The barrier, and which way each condition fails
    ///
    /// Four refusals, all of them on facts orrerix already holds — never on a
    /// judgment about what is on the pane's screen, which §3 keeps out of the
    /// driver:
    ///
    /// - **unknown, or already `Dead`** — there is nothing to release, and a
    ///   second release would restamp an initiator over whoever really ended it
    ///   (`record_exit_initiator` is first-writer-wins, so it would not, but the
    ///   caller would be told a release happened that did not);
    /// - **an orchestrator or manager pane** — never the driver's, defensively
    ///   refused here as well as being unreachable through
    ///   `reviewdrive::releasable`, which only ever names this drive's own
    ///   worker and lanes;
    /// - **busy** (`idle_since_ms.is_none()`) — the same signal the idle reaper
    ///   and `idle_pane_on_session` read, and the whole of what makes this safe
    ///   to demote in `exit_notice_route`: a pane with work in flight has
    ///   something to lose.
    ///
    /// **A missing pty is deliberately NOT a fourth refusal, and that is a
    /// divergence from `kill_agent_as` rather than an omission.** That function
    /// refuses an agent still in the spawn-to-bind window for rev-13 F1's
    /// reason: it stamps an initiator and then kills nothing, so the stamp —
    /// first-writer-wins and never cleared — misroutes every later exit of a pane
    /// that is still perfectly alive. Nothing of that shape is reachable here,
    /// because this does not merely stamp: `mark_dead` ends the agent's life in
    /// the registry, frees its slot, and drops the `by_pty` mapping, so there is
    /// no later exit left to misroute and the release really happened whether or
    /// not a terminal existed to close. `close_completed_planner` is the
    /// precedent and takes the same shape — mark dead, then kill the pty if
    /// there is one. Refusing here would instead leave a slot held by an agent
    /// the drive is finished with, which is the exact cost #2501 removes.
    ///
    /// # Ordering, and the cap
    ///
    /// The stamp goes on before anything is touched (`kill_agent_as`'s reason:
    /// the pty waiter can be in `on_pty_exit` before this returns), then
    /// `mark_dead`, then the pty. **The window that ordering leaves is stated
    /// rather than traded away**: a pane that crashes between the idle check and
    /// `mark_dead` loses the live→dead race, this returns `Err`, and the crash it
    /// did not cause is nonetheless attributed to `driver-release` — so that exit
    /// routes `AuditOnly` instead of prompting. It is the same window
    /// `kill_agent_as` has had since #533-B, on a pane this one has already
    /// established is idle, and closing it by stamping after `mark_dead` would
    /// cost the persisted roster row its initiator (the snapshot is taken inside
    /// `mark_dead`), which a restart reads and the live map does not survive. `mark_dead` is what frees the slot — the
    /// live-delegate cap counts agents whose status is not `Dead`, so a released
    /// pane stops counting at that instant rather than whenever its process
    /// finishes exiting, which is what "a released pane frees a slot
    /// immediately" means. It also drops the `by_pty` mapping, so the real exit
    /// lands as a no-op instead of a duplicate notice, and it is the atomic
    /// claim: only the caller that wins the live→dead transition gets `Some`,
    /// so a release racing a human's kill produces one of each, not two. The
    /// pty kill itself is best-effort, exactly as `close_completed_planner`'s
    /// is: unit tests run with no app handle.
    pub(crate) fn release_driven_pane(&self, agent_id: &str) -> Result<(), String> {
        let a = self.agent(agent_id).ok_or("unknown agent")?;
        if a.status == AgentStatus::Dead {
            return Err(format!("agent {agent_id} is already gone"));
        }
        // #2519: `is_fixture` — the same two classes plus `Role::Lead`, which
        // is not a driven delegate for a reason stronger than the other two's:
        // a lead group has no review driver at all (no board, no merge gate),
        // so nothing in that subsystem has any business reaching one.
        if a.role.is_fixture() {
            return Err(format!("refusing to release {agent_id}: it is not a driven delegate"));
        }
        if a.idle_since_ms.is_none() {
            return Err(format!("agent {agent_id} is still working"));
        }
        self.record_exit_initiator(agent_id, ExitInitiator::DriverRelease);
        // The atomic claim. `None` means something else won the live→dead race —
        // a human's kill, a crash — and this release did not happen.
        let Some(snapshot) = self.mark_dead(agent_id, Some(0)) else {
            return Err(format!("agent {agent_id} was ended by something else first"));
        };
        self.audit(&snapshot.group, brand::AUDIT_ACTOR, "agent-kill", json!({
            "agent": agent_id,
            "initiator": ExitInitiator::DriverRelease.as_str(),
        }));
        // Best-effort, exactly as `close_completed_planner`'s is: a headless
        // test has neither an app handle nor a bound pty, and the release above
        // has already happened either way.
        if let (Some(app), Some(pty)) = (self.app.lock_safe().clone(), snapshot.pty_id) {
            app.state::<crate::pty::PtyManager>().kill(pty);
        }
        Ok(())
    }

    /// Record WHO initiated `agent_id`'s termination (#533-B) — the durable
    /// half of the exit-notice routing decision. Idempotent and
    /// first-writer-wins: if two paths race to kill the same agent, the one
    /// that actually caused it is the one that got there first, and a
    /// second stamp would rewrite history rather than record it. No-op for
    /// an unknown agent.
    #[doc(hidden)] // pub for integration tests
    pub fn record_exit_initiator(&self, agent_id: &str, initiator: ExitInitiator) {
        let mut agents = self.agents.lock_safe();
        if let Some(a) = agents.get_mut(agent_id) {
            if a.killed_by.is_none() {
                a.killed_by = Some(initiator);
            }
        }
    }

    /// The recorded initiator for `agent_id`, if any (#533-B) — read by
    /// tests and by `on_pty_exit`'s routing switch.
    #[doc(hidden)] // pub for integration tests
    pub fn exit_initiator(&self, agent_id: &str) -> Option<ExitInitiator> {
        self.agents.lock_safe().get(agent_id).and_then(|a| a.killed_by)
    }

    /// #203: a planner's contract is one plan → one report → exit, but the CLI
    /// session lingers idle after its final report, silently holding a delegate
    /// slot until idle-kill — and the orchestrator only finds out when a later
    /// spawn is rejected at the cap. When a planner reports `done`, loomux closes
    /// its pane deterministically here so the slot frees the moment the plan is
    /// posted; the planner role-template exit instruction is only belt-and-braces.
    ///
    /// Ordering, and what it now means (#3040 N2). The caller (the MCP `report`
    /// handler) hands the done report to the orchestrator *first*; this function
    /// then writes an `agent-exit-notice` **audit row** and delivers nothing to
    /// any pane. So the orchestrator receives exactly ONE prompt for a planner's
    /// completion — the report — and the ordering that used to be a claim about
    /// two pastes racing on the per-pty delivery mutex is now a claim about one
    /// paste and one file write, which cannot race for a reader's attention at
    /// all.
    ///
    /// That is the demotion's whole argument: the report the orchestrator has
    /// just read IS the news, so a second prompt saying the same pane is gone
    /// told it nothing (#3040's census: acted on zero times out of ten). The
    /// ordering is still pinned, because it still has to hold — the report must
    /// be delivered BEFORE the audit row is written, or a reader reconstructing
    /// the sequence from the log sees the exit first. See
    /// `a_planner_exit_is_audited_after_the_report_is_delivered`.
    ///
    /// Claiming the close: [`mark_dead`](Self::mark_dead) is the atomic gate. It
    /// returns `Some` only for the caller that transitions the agent live→dead
    /// (idempotent under the agents lock), so two concurrent `done` reports yield
    /// exactly one exit notice and one kill. Marking dead first also frees the
    /// slot immediately and drops the `by_pty` mapping, so the real pane exit
    /// lands as a no-op in `on_pty_exit` rather than a duplicate generic "exited
    /// (code …)" notice. No-op for a non-planner, an already-dead agent, or an
    /// unknown id.
    ///
    /// Known edges (rare, documented not fixed — see PR #209): (a) if a human's
    /// unsubmitted line is sitting in the orchestrator pane, the *report* paste
    /// aborts and its re-send nudge is suppressed for orchestrator targets (the
    /// #103 anti-loop rule), so the exit notice can arrive without the report —
    /// but the plan is durable as a GitHub issue comment and the notice says so.
    /// (b) A crash or human-kill of the planner pty in the microsecond window
    /// between the report handoff and `mark_dead` lets `on_pty_exit` win and add
    /// a generic crash notice; a *voluntary* exit can't race here (the CLI is
    /// blocked awaiting this MCP response).
    pub fn close_completed_planner(&self, agent_id: &str) {
        // Role gate first — only a planner is auto-closed. Role is immutable, so
        // reading it before the claim is safe; the claim below stays atomic.
        match self.agent(agent_id) {
            Some(a) if a.role == Role::Planner => {}
            _ => return,
        }
        // Atomic claim: only the winner of the live→dead transition proceeds, so
        // a concurrent double `done` delivers one notice and one kill (see doc).
        // Stamped BEFORE the claim so the snapshot carries it: the same field
        // `on_pty_exit` routes on, so the pty's own later exit cannot re-promote
        // this to a prompt.
        self.record_exit_initiator(agent_id, ExitInitiator::PlannerCompleted);
        let Some(snapshot) = self.mark_dead(agent_id, Some(0)) else { return };
        // AUDIT-ONLY (#3040 N2), on #533-B's argument verbatim rather than a new
        // one: an exit the orchestrator's own delegate caused, and that it can
        // read back from the roster on demand, is not worth a turn. This one is
        // the clearest case of the class — the planner's `report(done)` is the
        // IMMEDIATELY PRECEDING prompt in that same pane (the ordering is a
        // guarantee, and it survives the demotion as an ordering between the
        // delivered report and this audit row — pinned by
        // `a_planner_exit_is_audited_after_the_report_is_delivered`), so the
        // notice told the orchestrator a second time what it had just read.
        // The census on #3040 found it acted on zero times out of ten.
        //
        // The one edge worth naming, because it is the reason this is a
        // demotion and not a deletion: PR #209 edge (a), where the report paste
        // was aborted by a human's unsubmitted line and never landed. Nothing
        // is lost even then — the plan is durable on GitHub, `list_agents`
        // shows the pane gone, and the full notice text is on the audit log
        // under `agent-exit-notice`, which is what makes "read it on demand" a
        // real path rather than a euphemism for "it was dropped".
        self.audit_demoted_exit_notice(
            &snapshot.group,
            &snapshot.id,
            snapshot.killed_by,
            &format!(
                "[orrerix] planner {} ({}) posted its plan and exited — its delegate slot is free.",
                snapshot.name, snapshot.id
            ),
        );
        // Terminate the actual CLI pane. Best-effort: unit tests run without an
        // app handle or a bound pty.
        if let (Some(app), Some(pty)) = (self.app.lock_safe().clone(), snapshot.pty_id) {
            app.state::<crate::pty::PtyManager>().kill(pty);
        }
    }

    pub fn focus_agent(&self, agent_id: &str) -> Result<(), String> {
        let a = self.agent(agent_id).ok_or("unknown agent")?;
        let app = self.app.lock_safe().clone().ok_or("no app handle")?;
        app.emit("orch-focus", json!({ "agent_id": agent_id, "pty_id": a.pty_id }))
            .map_err(|e| e.to_string())
    }

    /// Rename an agent's pane title and durable roster entry, respecting the
    /// name-source precedence ladder (#95r): the rename applies only when
    /// `source` ranks at least as high as whoever set the current name, so a
    /// human rename (highest) is never overwritten by the orchestrator's
    /// `rename_agent` (middle) or the id-derived default (lowest); the
    /// orchestrator can still relabel an id-default or its own earlier name.
    /// Rejects a dead/unknown target. On success the pane title follows via an
    /// `orch-rename` event, the roster is updated, the change is audited, and
    /// the applied (trimmed/truncated) name is returned. Caller scopes the
    /// target to its group (see the MCP `rename_agent` tool).
    pub fn rename_agent(&self, agent_id: &str, name: &str, source: NameSource) -> Result<String, String> {
        let name = sanitize_agent_name(name);
        if name.is_empty() {
            return Err("name must not be empty".into());
        }
        let entry = {
            let mut agents = self.agents.lock_safe();
            let a = agents.get_mut(agent_id).ok_or("unknown agent")?;
            if a.status == AgentStatus::Dead {
                return Err("agent is not alive".into());
            }
            if source.rank() < a.name_source.rank() {
                // Only the orchestrator-vs-human case reaches here in practice.
                return Err(format!(
                    "not overriding {agent_id}: its name \"{}\" was set by the human and takes precedence",
                    a.name
                ));
            }
            a.name = name.clone();
            a.name_source = source;
            a.clone()
        };
        self.persist_agent_record(&entry, "running");
        if let Some(app) = self.app.lock_safe().clone() {
            let _ = app.emit(
                "orch-rename",
                json!({ "agent_id": entry.id, "pty_id": entry.pty_id, "name": name }),
            );
        }
        self.audit(&entry.group, brand::AUDIT_ACTOR, "agent-rename",
            json!({ "agent": agent_id, "name": name, "source": source.as_str() }));
        Ok(name)
    }

    #[doc(hidden)] // pub for integration tests
    pub fn mark_dead(&self, agent_id: &str, exit_code: Option<u32>) -> Option<AgentEntry> {
        let snapshot = self.mark_dead_keeping_workspace(agent_id, exit_code)?;
        // #3443: every ending of a pane funnels through here — a kill, a pane
        // close, a crash, an idle reap, a driver release, a bind timeout — so
        // this is the one place a reviewer's scratch worktree can be reclaimed
        // without a list of call sites to keep in step. `end_group` is the one
        // caller that goes around it, because the human's own
        // "clean up worktrees" choice governs a teardown.
        self.reclaim_reviewer_scratch(&snapshot);
        Some(snapshot)
    }

    /// [`mark_dead`](Self::mark_dead) without the reviewer-scratch reclaim
    /// (#3443) — for `end_group`, whose own `cleanup_worktrees` flag is the
    /// human's decision about every worktree in the group, reviewers' included.
    pub(in crate::orchestration) fn mark_dead_keeping_workspace(&self, agent_id: &str, exit_code: Option<u32>) -> Option<AgentEntry> {
        let mut agents = self.agents.lock_safe();
        let a = agents.get_mut(agent_id)?;
        if a.status == AgentStatus::Dead {
            return None;
        }
        a.status = AgentStatus::Dead;
        let snapshot = a.clone();
        drop(agents);
        self.by_token.lock_safe().remove(&snapshot.token);
        if let Some(p) = snapshot.pty_id {
            self.by_pty.lock_safe().remove(&p);
            self.delivery.lock_safe().remove(&p);
        }
        // Attention bookkeeping is per-live-agent; drop this one's entries.
        self.attn_reports.lock_safe().remove(agent_id);
        self.attn_quiet.lock_safe().remove(agent_id);
        self.attn_waiting_ack.lock_safe().remove(agent_id);
        self.attn_emitted.lock_safe().remove(agent_id);
        // Notification backend (#243): a dead agent's watches are garbage —
        // the pane they'd fire into is gone — covering idle-kill, kill_agent,
        // a crash, and planner auto-close identically (they all funnel here).
        self.cleanup_agent_watches(agent_id, &snapshot.group);
        // Cross-workspace channels (#271): a dead agent's channel membership
        // is garbage the same way — the pane a peer's `channel_send` would
        // land in is gone. Tears the channel down (and notifies the
        // stranded peer) if this drops it below 2 members.
        self.cleanup_agent_channel(agent_id, &snapshot.group);
        // Named lock resources (#858): a dead agent's holds are garbage in the
        // same way, and worse — a slot nobody will ever release. Reclaimed
        // here rather than left to the 30s sweep so the next worker in line
        // gets it immediately; the sweep stays as the backstop for a holder
        // that dies without this path running at all.
        self.cleanup_agent_locks(agent_id, &snapshot.group);
        // #925: this is a `remove_file`, so the id reaching the file name is
        // validated like every other member of the family. Fail-closed into the
        // existing best-effort degrade — an id that could not name this file is
        // an id that never wrote one, so there is nothing to reclaim.
        if let Ok(agent_seg) = PathSegment::parse(agent_id) {
            let _ = fs::remove_file(
                self.group_dir(&snapshot.group).join("configs").join(format!("{agent_seg}.json")),
            );
        }
        // #2515 C1: codex's generated file is the one that does NOT live under
        // the group dir — it is a profile in the human's `CODEX_HOME` — so the
        // line above cannot reach it and it needs its own removal.
        //
        // Unconditional rather than gated on this agent's CLI, and that is the
        // safer direction on both sides: the removal is a `remove_file` of a
        // path derived entirely from this agent's own id, so for a non-codex
        // agent it removes a file that was never written and costs one failed
        // syscall — while gating on `cli_for_agent` would leave a real profile
        // behind for any agent whose recorded CLI has drifted from what it was
        // spawned on. A file in a vendor's user directory that nothing cleans
        // up is #502 by another route; the startup sweep is the backstop, not
        // the plan.
        self.remove_codex_profile(agent_id);
        self.audit(&snapshot.group, brand::AUDIT_ACTOR, "agent-exit",
            json!({ "agent": agent_id, "exit_code": exit_code }));
        crate::obs::breadcrumb(
            "agent-dead",
            &format!("agent={agent_id} pty={:?} code={exit_code:?}", snapshot.pty_id),
        );
        self.persist_agent_record(&snapshot, "dead");
        // Durably capture final usage before the pane is fully torn down, so a
        // recycled/killed agent still counts toward the group's lifetime total
        // (issue #42). The transcript remains readable after exit; the
        // statusline does not, but token usage is the source we rely on.
        let cli = self.cli_for_agent(&snapshot);
        let usage = self.compute_usage_snapshot(&snapshot, &cli);
        self.upsert_usage_snapshot(&snapshot.group, usage);
        Some(snapshot)
    }

    /// Every claim this group's panes make on a workspace (#3443): the live
    /// registry first, then the durable roster for any pane this process no
    /// longer holds — which is where a resumed reviewer's ORIGINAL pane, the
    /// one whose record carries the branch, lives after a restart.
    fn workspace_claims(&self, group: &GroupId) -> Vec<WorkspaceClaim> {
        let mut claims: Vec<WorkspaceClaim> = self
            .agents
            .lock_safe()
            .values()
            .filter(|a| &a.group == group)
            .map(|a| WorkspaceClaim {
                id: a.id.clone(),
                reviewer: a.role == Role::Reviewer,
                cwd: a.cwd.clone(),
                branch: a.branch.clone(),
                live: a.status != AgentStatus::Dead,
            })
            .collect();
        for r in self.merged_records(group) {
            if claims.iter().any(|c| c.id == r.id) {
                continue;
            }
            claims.push(WorkspaceClaim {
                reviewer: r.role == Role::Reviewer.as_str(),
                live: false,
                id: r.id,
                cwd: r.cwd,
                branch: r.branch,
            });
        }
        claims
    }

    /// **Remove a dead reviewer's scratch worktree and its branch** (#3443).
    ///
    /// Called from [`mark_dead`](Self::mark_dead) for every pane that dies;
    /// anything but a reviewer returns at once, before any roster read or git.
    /// What is removed is decided by [`reviewer_scratch_verdict`], re-asked on
    /// every attempt, so a resume that starts using the directory between two
    /// attempts stops the reclaim rather than being removed out from under.
    ///
    /// **It never blocks the kill.** With the registry's self-handle (every
    /// production registry), the attempts run on a thread of their own, after
    /// [`SCRATCH_RECLAIM_BACKOFF`]'s waits; the caller has already finished
    /// ending the pane. A bare registry with no self-handle (the integration
    /// tests) makes ONE attempt inline, so its outcome is observable the moment
    /// the pane is dead. A removal that still fails at the last attempt — a
    /// file lock on Windows — is audited `reviewer-worktree-remove-failed`
    /// with git's own error, and the worktree stays for a later `git worktree
    /// remove`; nothing retries it after that.
    fn reclaim_reviewer_scratch(&self, dead: &AgentEntry) {
        if dead.role != Role::Reviewer {
            return;
        }
        match self.arc() {
            Some(reg) => {
                let dead = dead.clone();
                std::thread::spawn(move || {
                    let last = SCRATCH_RECLAIM_BACKOFF.len();
                    for (i, wait) in SCRATCH_RECLAIM_BACKOFF.iter().enumerate() {
                        std::thread::sleep(*wait);
                        if reg.reclaim_scratch_attempt(&dead, i + 1, i + 1 == last) {
                            return;
                        }
                    }
                });
            }
            None => {
                self.reclaim_scratch_attempt(dead, 1, true);
            }
        }
    }

    /// One reclaim attempt; `true` when there is nothing more to try.
    fn reclaim_scratch_attempt(&self, dead: &AgentEntry, attempt: usize, last: bool) -> bool {
        let Some(g) = self.group(&dead.group) else { return true };
        let claims = self.workspace_claims(&dead.group);
        let initiator = dead.killed_by.map(|i| i.as_str());
        let branch = match reviewer_scratch_verdict(&g.repo, &dead.cwd, &claims, Some(&dead.id)) {
            ScratchVerdict::NotScratch => return true,
            ScratchVerdict::Kept(reason) => {
                self.audit(&dead.group, brand::AUDIT_ACTOR, "reviewer-worktree-kept", json!({
                    "agent": dead.id, "path": dead.cwd, "reason": reason,
                }));
                return true;
            }
            ScratchVerdict::Scratch { branch } => branch,
        };
        match crate::git::git_worktree_remove(&g.repo, &dead.cwd) {
            Ok(()) => {
                // The branch only once its worktree is gone: git refuses to
                // delete a branch checked out anywhere, and a reviewer's is
                // checked out in exactly the worktree just removed (or nowhere,
                // after its `gh pr checkout --detach`). And only if no commit
                // lives on it alone — the verdict proved the WORKTREE is
                // scratch, but a spawn handed an existing branch by name would
                // otherwise take that branch's unpushed work with it.
                let br = crate::git::git_branch_delete_if_redundant(&g.repo, &branch);
                self.audit(&dead.group, brand::AUDIT_ACTOR, "reviewer-worktree-removed", json!({
                    "agent": dead.id,
                    "path": dead.cwd,
                    "branch": branch,
                    "branch_deleted": br.as_ref().is_ok_and(|d| *d),
                    "branch_kept": matches!(br, Ok(false)).then_some("has-commits-no-other-ref-holds"),
                    "branch_error": br.err(),
                    "attempt": attempt,
                    "initiator": initiator,
                }));
                true
            }
            Err(e) if last => {
                self.audit(&dead.group, brand::AUDIT_ACTOR, "reviewer-worktree-remove-failed", json!({
                    "agent": dead.id,
                    "path": dead.cwd,
                    "branch": branch,
                    "error": e,
                    "attempts": attempt,
                    "initiator": initiator,
                }));
                true
            }
            Err(_) => false,
        }
    }

    /// **Cut a reclaimed reviewer worktree again, at the same path, before a
    /// resume reads it** (#3443) — `true` when `cwd` exists afterwards because
    /// of this call.
    ///
    /// The reclaim removes a reviewer's worktree when its pane dies, and the
    /// review driver releases a lane with its session KEPT: the next round
    /// resumes that conversation in a fresh pane, in the workspace the roster
    /// recorded (`resolve_worker_resume_cwd`). With the directory gone, that
    /// resume would refuse `resume-workspace-missing` and the drive would open
    /// a fresh lane with no memory of its earlier verdict. So every resume path
    /// calls this first.
    ///
    /// **The same path, not a new one**, because that path is what every
    /// resume route already resolves to — the roster's recorded cwd, or the
    /// cwd a CLI's own store holds for the session — and a CLI may key its
    /// store on it: Claude Code keeps a transcript under a project directory
    /// named after the cwd (its sessions reference, "Where transcripts are
    /// stored"; `--resume <id>` has searched every project only since
    /// v2.1.223, and resumed from the session's own directory before). A new
    /// path would need every one of those routes taught about it. The same branch name,
    /// because a worktree's path is derived from its branch
    /// (`git_worktree_add_sync`), and the result is checked against `cwd`
    /// rather than assumed: a mismatch is removed again and audited.
    ///
    /// Scratch by contract, so a fresh cut from the default branch is exactly
    /// what the reviewer had at spawn; it re-checks-out the PR it reviews.
    ///
    /// A no-op unless `cwd` is missing AND [`reviewer_scratch_verdict`] says it
    /// was a reviewer's scratch worktree — so a worker's vanished worktree is
    /// never re-cut from the default branch here, which would hand a resumed
    /// worker a checkout without its own work.
    #[doc(hidden)] // pub for integration tests
    pub fn restore_reviewer_scratch_worktree(&self, group: &GroupId, cwd: &str) -> bool {
        if cwd.trim().is_empty() || Path::new(cwd).is_dir() {
            return false;
        }
        let Some(g) = self.group(group) else { return false };
        let claims = self.workspace_claims(group);
        let ScratchVerdict::Scratch { branch } = reviewer_scratch_verdict(&g.repo, cwd, &claims, None)
        else {
            return false;
        };
        match crate::git::git_worktree_add_sync(g.repo.clone(), branch.clone(), None) {
            Ok(wt) if same_path_key(&wt, cwd) => {
                // No `admit_derived` here (#1042 slice B), deliberately: the
                // root registry never withdraws an admission, so a path this
                // process cut is still declared from its ORIGINAL cut in
                // `spawn_agent_ex`, and after a restart this resume is exactly
                // as declared as any other resume — none of them admits. A
                // third site would also have to be argued into
                // `tests/rootreg.rs`'s census for no change in behaviour.
                self.audit(group, brand::AUDIT_ACTOR, "reviewer-worktree-recut", json!({
                    "path": cwd, "branch": branch,
                }));
                true
            }
            Ok(wt) => {
                let _ = crate::git::git_worktree_remove(&g.repo, &wt);
                self.audit(group, brand::AUDIT_ACTOR, "reviewer-worktree-recut-failed", json!({
                    "path": cwd, "branch": branch,
                    "error": format!("the worktree was cut at {wt}, not at the recorded path"),
                }));
                false
            }
            Err(e) => {
                self.audit(group, brand::AUDIT_ACTOR, "reviewer-worktree-recut-failed", json!({
                    "path": cwd, "branch": branch, "error": e,
                }));
                false
            }
        }
    }

    /// Called from the pty waiter thread when any pty exits. No-op for ptys
    /// that aren't orchestration agents.
    /// `tail` is the pty's captured output at the moment it exited (ANSI
    /// stripped), `total_bytes` the monotonic byte count it ever produced —
    /// both read off the *removed* pty handle in `pty.rs`, since the live
    /// ring is gone the instant this runs. `total_bytes == 0` is the #281
    /// signature: a resumed CLI that exited before printing a single byte,
    /// which a bare exit code can't be told apart from "it did real work and
    /// then failed" — the orchestrator's notice below says which happened.
    /// `expected` is true when loomux itself initiated this exit (kill_agent,
    /// idle-kill, pane close) — see `PtyManager::kill`, which removes the pty
    /// handle from the live map BEFORE this ever runs, so `tail`/`total_bytes`
    /// are always empty/zero on an expected exit regardless of how much real
    /// output the agent produced. Without this flag, a productive delegate
    /// that the orchestrator deliberately stopped would get the exact same
    /// "produced no output before exiting" misdiagnosis as a genuine silent
    /// death — the diagnostic is only meaningful for an exit loomux did NOT
    /// cause, so an expected one skips it entirely.
    pub fn on_pty_exit(
        &self,
        pty_id: u32,
        exit_code: Option<u32>,
        tail: &str,
        total_bytes: u64,
        expected: bool,
    ) {
        let agent_id = match self.by_pty.lock_safe().get(&pty_id).cloned() {
            Some(id) => id,
            None => return,
        };
        let started_ms = self.agent(&agent_id).map(|a| a.started_ms).unwrap_or(0);
        if !tail.is_empty() {
            if let Some(a) = self.agents.lock_safe().get_mut(&agent_id) {
                a.last_exit_tail = Some(tail.to_string());
            }
        }
        if let Some(a) = self.mark_dead(&agent_id, exit_code) {
            // #2519: a lead that dies takes its helpers with it, so a crash
            // costs what the human’s own deliberate pane close costs. This is the
            // whole of the orphan guard: it runs on the pty-exit path, which every
            // ending funnels through — a close, a kill, or the CLI simply dying.
            //
            // ITS OWN ARM rather than a statement inside the branch below, because
            // the exit NOTICE that branch sends has no recipient here:
            // `deliver_to_orchestrator` resolves the group’s root, and the root is
            // the pane that just died. Sending it anyway would be a delivery
            // attempt whose only possible outcome is a dropped notice.
            // #3679: a quick root is the same case. Its helpers report to it
            // and to nothing else, so helpers left running after it has gone
            // would work towards a report with no recipient. They end with it;
            // the run itself parks on `root-gone` when the quick drive next
            // looks, and Resume re-opens the root's session.
            if a.role == Role::Lead || a.role == Role::Quick {
                self.end_lead_children(&a);
                // #3723: a quick root with NO task in progress. Closed by
                // the human or by orrerix, its run ends outright — there is
                // nothing to park, and a record left idle would need a Stop
                // for no reason. Gone by ITSELF — a CLI that died at boot —
                // the run parks instead, so a failed launch is not a pane
                // that vanished without a word. `expected` is which — except
                // at app shutdown, whose kills all arrive unasked-for and
                // move nothing (`note_shutdown`).
                if a.role == Role::Quick {
                    let _ = (&a.group, &a.id, expected);
                }
            } else if a.role != Role::Orchestrator {
                let elapsed_ms = now_ms().saturating_sub(started_ms);
                let cause = exit_cause(expected, tail, total_bytes);
                let notice = format!(
                    "[orrerix] agent {} ({}) exited (code {exit_code:?}) {elapsed_ms}ms after \
                     spawn — {cause}. Update your plan and state accordingly.",
                    a.name, a.id,
                );
                // #533-B: routed on the RECORDED initiator, never on
                // `expected` (which only says loomux closed the pane, not
                // who decided to — a human pane close looks identical to
                // `kill_agent` there). An orchestrator that asked for this
                // exit, or an idle reaper that took a task-less agent,
                // costs it nothing to learn from the roster on demand; an
                // exit nobody in this process asked for still interrupts.
                match exit_notice_route(a.killed_by) {
                    ExitNoticeRoute::Prompt => {
                        let _ = self.deliver_to_orchestrator(&a.group, &notice, brand::AUDIT_ACTOR);
                    }
                    ExitNoticeRoute::AuditOnly => self.audit_demoted_exit_notice(
                        &a.group,
                        &a.id,
                        a.killed_by,
                        &notice,
                    ),
                }
            }
        }
    }

    /// Write an exit notice the orchestrator is NOT prompted with to the
    /// audit log instead (#533-B), full text included, so "read it on
    /// demand" is a real path and not a euphemism for "it was dropped".
    /// `list_agents` already carries the liveness half; this carries the
    /// wording, the initiator and the timestamp.
    pub(in crate::orchestration) fn audit_demoted_exit_notice(
        &self,
        group: &GroupId,
        agent_id: &str,
        initiator: Option<ExitInitiator>,
        notice: &str,
    ) {
        self.audit(group, brand::AUDIT_ACTOR, "agent-exit-notice", json!({
            "agent": agent_id,
            "routed": "audit-only",
            "initiator": initiator.map(|i| i.as_str()),
            "notice": notice,
        }));
    }

    #[doc(hidden)] // pub for integration tests
    pub fn state_root(&self) -> PathBuf {
        self.root.clone()
    }

    /// Every `orch-session-learned` payload this registry produced with no
    /// frontend to emit to, oldest first. Empty in production; see
    /// [`Self::test_session_learned`] for what it does and does not pin.
    #[doc(hidden)] // pub for integration tests
    pub fn session_learned_events_for_test(&self) -> Vec<serde_json::Value> {
        self.test_session_learned.lock_safe().clone()
    }
}
