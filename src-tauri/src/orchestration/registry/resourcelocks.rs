//! The named lock resources a repo declares (#858): the menu, acquire and
//! release, the grant notice, and the sweep that frees a dead holder's
//! locks, as an `impl OrchRegistry` block (#3498). The design is
//! `docs/design/lock-resources.md`. The registry's own mutexes are not these;
//! their test seams are in `lockseams.rs`.

use super::*;

impl OrchRegistry {
    // ── named lock resources (#858) ────────────────────────────────────────
    //
    // The engine is `locks::LockTable` (pure, unit-tested there); everything
    // below is the wiring: one reader for the config, one guarded accessor for
    // the table, audit lines, and pane notices. Design note:
    // `docs/design/lock-resources.md`.

    /// What this group's repo currently declares under `resources:`. **One
    /// reader for the whole block**, the same rule `merge_queue_policy` states:
    /// a tool and a background tick that disagreed about the declared set
    /// would grant and reclaim on different worlds.
    ///
    /// Gated on `advanced_orchestrator` for the same reason every other
    /// workflow-file clause is: with the toggle off, `.loomux/workflow.yml` is
    /// not this group's config at all and is never opened.
    ///
    /// `None` means **"cannot tell right now"** and is not the same answer as
    /// `Some(empty)`. A workflow file mid-save is unparseable for a moment, and
    /// this is read on every lock call *and* on the group view's 2s poll — so
    /// collapsing "unreadable" into "declares nothing" would drop every live
    /// hold and queue in the group, and audit that it had, because an author
    /// was halfway through typing. The caller reconciles on `Some` and leaves
    /// the table exactly as it is on `None`. (The parse error is surfaced
    /// loudly elsewhere, `workflow-invalid`; this is not the place to
    /// re-report it.)
    fn lock_resources(&self, group: &GroupId) -> Option<BTreeMap<String, workflow::ResourcePolicy>> {
        let g = self.group(group)?;
        if !g.guardrails.advanced_orchestrator {
            // Authoritative, not unknown: with the toggle off this group has no
            // declared resources, and flipping it off is a real change that
            // should drop them.
            return Some(BTreeMap::new());
        }
        match load_active_workflow(&g.repo, &g.guardrails) {
            Ok(Some(wf)) => Some(wf.resources),
            Ok(None) => Some(BTreeMap::new()), // no file: declares nothing
            Err(_) => None,                    // unreadable: hold what we have
        }
    }

    /// Run `f` against this group's live lock table, after reconciling it with
    /// what the repo declares right now. Audit lines for anything the
    /// reconcile dropped are written **after** the table lock is released —
    /// `audit` touches the filesystem, and no registry mutex is ever held
    /// across file I/O.
    fn with_locks<T>(&self, group: &GroupId, f: impl FnOnce(&mut locks::LockTable) -> T) -> T {
        let declared = self.lock_resources(group);
        let (out, dropped) = {
            let mut all = self.locks.lock_safe();
            let table = all.entry(group.clone()).or_default();
            // `None` = the config could not be read this instant; reconciling
            // against a guess would drop live holds (see `lock_resources`).
            let dropped = match &declared {
                Some(d) => table.sync(d),
                None => Vec::new(),
            };
            (f(table), dropped)
        };
        for r in dropped {
            self.audit(
                group,
                brand::AUDIT_ACTOR,
                "lock-undeclared",
                json!({
                    "resource": r.name,
                    "holders": r.holders.iter().map(|h| h.agent.clone()).collect::<Vec<_>>(),
                    "queued": r.queue.iter().map(|w| w.agent.clone()).collect::<Vec<_>>(),
                }),
            );
        }
        out
    }

    /// The group's declared resources, for the `acquire_lock` tool description
    /// an agent reads (and for deciding whether to list the lock tools at all).
    ///
    /// A momentarily unreadable file falls back to what the live table already
    /// holds, so a mid-save `workflow.yml` cannot make an agent's tool listing
    /// blink out from under it.
    pub fn lock_menu(&self, group: &GroupId) -> Vec<(String, workflow::ResourcePolicy)> {
        if let Some(declared) = self.lock_resources(group) {
            return declared.into_iter().collect();
        }
        self.locks
            .lock_safe()
            .get(group)
            .map(|t| {
                t.iter()
                    .map(|r| {
                        (
                            r.name.clone(),
                            workflow::ResourcePolicy {
                                slots: r.slots,
                                max_hold_minutes: r.max_hold_minutes,
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `acquire_lock`. Returns prose: the caller either holds the lock or
    /// knows its place in line, and both need a sentence rather than a shape
    /// to branch on.
    pub fn acquire_lock(
        &self,
        group: &GroupId,
        agent: &str,
        name: &str,
        note: &str,
        wait_minutes: u32,
    ) -> Result<String, String> {
        let now = now_ms();
        let outcome = self.with_locks(group, |t| t.acquire(name, agent, note, now, wait_minutes))?;
        let (action, detail, text) = match &outcome {
            locks::Acquired::Granted { expires_ms } => (
                "lock-acquire",
                json!({ "resource": name, "note": note, "expires_ms": expires_ms }),
                format!(
                    "'{name}' is YOURS. Release it with release_lock(\"{name}\") the moment you are \
                     done — loomux reclaims it automatically in {} min and audits that as a \
                     reclaim, which is a worse look than releasing it yourself.",
                    minutes_until(*expires_ms, now)
                ),
            ),
            locks::Acquired::AlreadyHeld { expires_ms } => (
                "lock-acquire-repeat",
                json!({ "resource": name, "expires_ms": expires_ms }),
                format!(
                    "you already hold '{name}' — nothing changed, and its deadline was NOT extended \
                     ({} min left). Carry on.",
                    minutes_until(*expires_ms, now)
                ),
            ),
            locks::Acquired::Queued { position, expires_ms } => (
                "lock-queued",
                json!({ "resource": name, "note": note, "position": position, "expires_ms": expires_ms }),
                queued_text(name, *position, minutes_until(*expires_ms, now), false),
            ),
            locks::Acquired::AlreadyQueued { position, expires_ms } => (
                "lock-queued-repeat",
                json!({ "resource": name, "position": position, "expires_ms": expires_ms }),
                queued_text(name, *position, minutes_until(*expires_ms, now), true),
            ),
        };
        self.audit(group, agent, action, detail);
        Ok(text)
    }

    /// `release_lock`. Hands the slot straight to the head of the queue and
    /// tells that agent, in its own pane, that it is now the holder.
    pub fn release_lock(&self, group: &GroupId, agent: &str, name: &str) -> Result<String, String> {
        let now = now_ms();
        let outcome = self.with_locks(group, |t| t.release(name, agent, now))?;
        match outcome {
            locks::Released::Held { granted } => {
                self.audit(group, agent, "lock-release", json!({ "resource": name }));
                let handed = granted.as_ref().map(|g| g.agent.clone());
                if let Some(g) = granted {
                    self.announce_lock_grant(group, &g);
                }
                Ok(match handed {
                    Some(a) => format!("released '{name}' — {a} was next in line and now holds it."),
                    None => format!("released '{name}'."),
                })
            }
            locks::Released::QueueCancelled { position } => {
                self.audit(
                    group,
                    agent,
                    "lock-queue-cancel",
                    json!({ "resource": name, "position": position }),
                );
                Ok(format!(
                    "you were not holding '{name}' — your queued request (position {position}) has \
                     been withdrawn, so nobody waits behind a slot you no longer want."
                ))
            }
        }
    }

    /// Audit a grant and type the notice into the new holder's pane. Shared by
    /// `release_lock` and the sweep so a grant is recorded identically however
    /// it was caused.
    fn announce_lock_grant(&self, group: &GroupId, g: &locks::Grant) {
        let now = now_ms();
        self.audit(
            group,
            brand::AUDIT_ACTOR,
            "lock-grant",
            json!({
                "resource": g.resource, "agent": g.agent,
                "waited_ms": g.waited_ms, "expires_ms": g.expires_ms,
            }),
        );
        let text = format!(
            "[orrerix] lock '{}' is yours — you waited {}. Hold it for at most {} min, then \
             release_lock(\"{}\").",
            g.resource,
            human_span(g.waited_ms),
            minutes_until(g.expires_ms, now),
            g.resource,
        );
        let _ = self.deliver_prompt(&g.agent, &text, brand::AUDIT_ACTOR, Delivery::MidSession);
    }

    /// `list_locks` / the group view's lock chrome — one JSON shape for both,
    /// so what a human sees and what an agent reads can never disagree.
    pub fn lock_state(&self, group: &GroupId) -> Value {
        let now = now_ms();
        self.with_locks(group, |t| {
            json!({
                "now_ms": now,
                "resources": t.iter().map(|r| json!({
                    "name": r.name,
                    "slots": r.slots,
                    "max_hold_minutes": r.max_hold_minutes,
                    "holders": r.holders.iter().map(|h| json!({
                        "agent": h.agent, "note": h.note,
                        "acquired_ms": h.acquired_ms, "expires_ms": h.expires_ms,
                    })).collect::<Vec<_>>(),
                    "queue": r.queue.iter().map(|w| json!({
                        "agent": w.agent, "note": w.note,
                        "queued_ms": w.queued_ms, "expires_ms": w.expires_ms,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })
        })
    }

    /// Drop every hold and queued request belonging to a dead agent (called
    /// from `mark_dead`, beside `cleanup_agent_watches`). This is the
    /// holder-death reclaim's FAST path — the 30s sweep would catch it anyway,
    /// but a worker that finished its build and exited should not make the
    /// next one wait half a minute for a slot nobody is using.
    pub(in crate::orchestration) fn cleanup_agent_locks(&self, agent_id: &str, group: &GroupId) {
        let now = now_ms();
        let sweep = self.with_locks(group, |t| t.drop_agent(agent_id, now));
        self.apply_lock_sweep(group, sweep);
    }

    /// Audit everything a sweep did and deliver every grant it produced.
    /// Called with no registry lock held — `audit` and `deliver_prompt` both
    /// block (file I/O, a per-pane delivery mutex).
    fn apply_lock_sweep(&self, group: &GroupId, sweep: locks::Sweep) {
        for r in &sweep.reclaimed {
            let (action, agent, detail) = match r {
                locks::Reclaimed::HoldExpired { resource, agent, held_ms } => (
                    "lock-expired",
                    agent,
                    json!({ "resource": resource, "agent": agent, "held_ms": held_ms }),
                ),
                locks::Reclaimed::HolderGone { resource, agent, held_ms } => (
                    "lock-reclaim",
                    agent,
                    json!({ "resource": resource, "agent": agent, "held_ms": held_ms, "why": "agent-gone" }),
                ),
                locks::Reclaimed::WaitTimedOut { resource, agent, waited_ms } => (
                    "lock-wait-timeout",
                    agent,
                    json!({ "resource": resource, "agent": agent, "waited_ms": waited_ms }),
                ),
                locks::Reclaimed::WaiterGone { resource, agent, waited_ms } => (
                    "lock-wait-cleanup",
                    agent,
                    json!({ "resource": resource, "agent": agent, "waited_ms": waited_ms, "why": "agent-gone" }),
                ),
            };
            self.audit(group, brand::AUDIT_ACTOR, action, detail);
            // Only the two clock-driven outcomes get a notice: the agent is
            // still alive and now believes something that is no longer true.
            // The two `agent-gone` variants have nobody to tell.
            let text = match r {
                locks::Reclaimed::HoldExpired { resource, held_ms, .. } => Some(format!(
                    "[orrerix] lock '{resource}' RECLAIMED — you held it {} (its max_hold_minutes). \
                     Anything you are still running against it is no longer serialized: call \
                     acquire_lock(\"{resource}\") again before continuing.",
                    human_span(*held_ms)
                )),
                locks::Reclaimed::WaitTimedOut { resource, waited_ms, .. } => Some(format!(
                    "[orrerix] lock '{resource}' wait TIMED OUT after {} — you are no longer in the \
                     queue. Call acquire_lock(\"{resource}\") again if you still need it.",
                    human_span(*waited_ms)
                )),
                _ => None,
            };
            if let Some(text) = text {
                let _ = self.deliver_prompt(agent, &text, brand::AUDIT_ACTOR, Delivery::MidSession);
            }
        }
        for g in &sweep.granted {
            self.announce_lock_grant(group, g);
        }
    }

    /// The 30s reclaim pass, folded into the existing poll tick. Returns the
    /// groups it acted on (tests assert on this; nothing else reads it).
    ///
    /// A **paused** group is frozen solid — no expiry, no reclaim, no grant —
    /// and is credited the whole pause span on the tick that observes it
    /// unpaused, so a long pause cannot silently evaporate every hold in the
    /// group while its panes sat frozen. Same shape, and the same reasoning,
    /// as `notify_tick`'s TTL freeze.
    pub fn locks_tick(&self, now: u64) -> Vec<GroupId> {
        let paused = self.paused.lock_safe().clone();
        let extend_by: HashMap<GroupId, u64> = {
            let mut since = self.paused_locks_since.lock_safe();
            for g in paused.iter() {
                since.entry(g.clone()).or_insert(now);
            }
            let resumed: Vec<GroupId> =
                since.keys().filter(|g| !paused.contains(*g)).cloned().collect();
            let mut extend = HashMap::new();
            for g in resumed {
                if let Some(started) = since.remove(&g) {
                    extend.insert(g, now.saturating_sub(started));
                }
            }
            extend
        };
        // Liveness, snapshotted BEFORE the table lock: `agents` and `locks` are
        // only ever taken in this order, so the two can't deadlock against each
        // other.
        let live: HashSet<String> = {
            let agents = self.agents.lock_safe();
            agents
                .values()
                .filter(|a| a.status != AgentStatus::Dead)
                .map(|a| a.id.clone())
                .collect()
        };
        let groups: Vec<GroupId> = self.locks.lock_safe().keys().cloned().collect();
        let mut acted = Vec::new();
        for group in groups {
            let sweep = {
                let mut all = self.locks.lock_safe();
                let Some(table) = all.get_mut(&group) else { continue };
                if let Some(extra) = extend_by.get(&group) {
                    table.extend_deadlines(now, *extra);
                }
                if paused.contains(&group) {
                    continue;
                }
                table.sweep(now, &|a: &str| live.contains(a))
            };
            if !sweep.is_empty() {
                acted.push(group.clone());
                self.apply_lock_sweep(&group, sweep);
            }
        }
        acted
    }
}
