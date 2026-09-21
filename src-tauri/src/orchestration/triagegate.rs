//! Delivery triage's registry wiring (#3304 S1) — the host half of
//! [`loomux_engine::triage`].
//!
//! Every DECISION is the engine's: [`triage::decide`] answers deliver / defer
//! from the delivery's leading shape and nothing else. What lives here is what
//! only the registry can do — resolve the policy out of the repo's workflow
//! file, hold the deferral lock across the read-modify-write on
//! `<group-dir>/deferred.json`, write the `delivery-triaged` audit row, attempt
//! the merge-queue enqueue a satisfied gate's rule is justified by, and deliver
//! the framed flush.
//!
//! # Why its own file rather than more of `mod.rs`
//!
//! The same reason `rdtick.rs` gives, narrowed to this feature: a gate that can
//! SUPPRESS a delivery to the one pane a human supervises is a thing a reader
//! must be able to find the whole of. Every path that can hold a notice back is
//! in this file, and `src-tauri/tests/triage.rs` reads it as one scope.
//!
//! # The three bounds on a deferral
//!
//! Nothing is ever dropped, and no wait is unbounded (#496/#513):
//!
//! 1. **the next genuine wake** — [`OrchRegistry::triage_delivery`] flushes in
//!    front of every delivery it lets through, so the orchestrator reads what
//!    it slept through at the moment it is woken anyway;
//! 2. **`max_defer_minutes`** — [`OrchRegistry::triage_flush_tick`], on the
//!    watchdog's own timer, flushes a store whose oldest entry has aged out.
//!    This is the bound that matters, because (1) waits on a signal that may
//!    never come;
//! 3. **[`triage::MAX_DEFERRED`]** — a CI storm inside one window flushes early
//!    rather than growing a frame nobody can read.
//!
//! # Fail-safe is DELIVER, at every layer
//!
//! A group that cannot be resolved, a workflow file that will not parse, a
//! `deferred.json` that cannot be read or written, a merge-queue enqueue that
//! refuses — every one of them delivers. The registry never *declines* to
//! deliver because something went wrong; it declines only when the engine
//! positively recognised a shape AND the deferral was durably recorded.

use std::path::PathBuf;

use serde_json::json;

use super::{
    atomic_write, brand, load_active_workflow, now_ms, triage, workflow, Delivery, GroupId,
    LockExt, OrchRegistry, Role,
};

/// What [`OrchRegistry::triage_delivery`] tells its caller to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Triaged {
    /// Carry on delivering. `flush` is the framed notice for anything that was
    /// being held, to be delivered IN FRONT of this one (the queue is FIFO, so
    /// enqueuing it first is what puts it first).
    Deliver { flush: Option<String> },
    /// Held. The caller returns `Ok(())` without admitting anything: the
    /// notice is in `deferred.json`, in the audit log, and readable with
    /// `list_deferred()`.
    Deferred,
}

impl OrchRegistry {
    /// This group's `triage:` policy, resolved the way
    /// `merge_queue_policy`/`board_policy` are: the declared block, or the
    /// off-by-default when the block is absent, the file will not parse, the
    /// group cannot be resolved, or the workflow is not in force at all.
    ///
    /// **Fail-open is the right direction here and it is worth saying why**,
    /// because for a gate the reflex is the opposite. What this policy switches
    /// on is a SUPPRESSION; resolving an unreadable file to the default
    /// therefore delivers MORE, which is the harmless error. A file caught
    /// mid-save makes every notice reach the pane, exactly as it did before
    /// this feature existed.
    pub(super) fn triage_policy(&self, group: &GroupId) -> workflow::TriagePolicy {
        let Some(g) = self.group(group) else { return workflow::TriagePolicy::default() };
        if !g.guardrails.advanced_orchestrator {
            return workflow::TriagePolicy::default();
        }
        match load_active_workflow(&g.repo, &g.guardrails) {
            Ok(Some(wf)) => wf.triage,
            _ => workflow::TriagePolicy::default(),
        }
    }

    fn deferred_path(&self, group: &GroupId) -> PathBuf {
        self.group_dir(group).join(triage::DEFERRED_FILE)
    }

    /// Read `deferred.json`. An absent file is an empty store; a file that
    /// will not parse is ALSO an empty store, and the difference is only that
    /// the second is audited.
    ///
    /// It has to be read as empty rather than refused: this is on the delivery
    /// path, and a store nobody can parse must not wedge every notice into the
    /// pane's queue behind it. What is lost is the held notices themselves —
    /// which is why the parse failure is audited with the path, so the bytes
    /// are findable, and why nothing here ever REWRITES a file it failed to
    /// read without saying so.
    fn load_deferred(&self, group: &GroupId) -> triage::Deferred {
        let path = self.deferred_path(group);
        match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => triage::Deferred::default(),
            Err(e) => {
                self.audit(group, brand::AUDIT_ACTOR, "delivery-triage-fault", json!({
                    "at": "load", "path": path.display().to_string(), "why": e.to_string(),
                }));
                triage::Deferred::default()
            }
            Ok(text) => match serde_json::from_str::<triage::Deferred>(&text) {
                Ok(d) => d,
                Err(e) => {
                    self.audit(group, brand::AUDIT_ACTOR, "delivery-triage-fault", json!({
                        "at": "parse", "path": path.display().to_string(), "why": e.to_string(),
                    }));
                    triage::Deferred::default()
                }
            },
        }
    }

    /// Write `deferred.json` atomically. `Err` is the caller's signal to
    /// DELIVER rather than defer — a deferral that was not durably recorded is
    /// a notice that would be lost on the next restart, which is the one thing
    /// this feature promises never to do.
    fn store_deferred(&self, group: &GroupId, d: &triage::Deferred) -> Result<(), String> {
        let path = self.deferred_path(group);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let bytes = serde_json::to_vec_pretty(d).map_err(|e| e.to_string())?;
        atomic_write(&path, &bytes).map_err(|e| e.to_string())
    }

    /// THE HOOK. Called from `deliver_prompt_as` for an orchestrator-bound
    /// mid-session delivery, and from nowhere else.
    ///
    /// The three doors a notice can reach the orchestrator through —
    /// `deliver_to_orchestrator`, a `notify_when` fire (`watchdog_tick` →
    /// `deliver_prompt`), and `deliver_relayed_to_root` — meet only at
    /// `deliver_prompt_as`, which is why the hook is there rather than at the
    /// door that reads as the obvious one. Kickoffs and `Regrounding` are
    /// excluded by the [`Delivery`] kind, the same key the manager
    /// no-injection guarantee already uses.
    pub(super) fn triage_delivery(
        &self,
        group: &GroupId,
        to: &str,
        from: &str,
        text: &str,
        role: Role,
        delivery: Delivery,
    ) -> Triaged {
        // Not an orchestrator's mid-session delivery: not triage's business at
        // all, and not even a policy read.
        //
        // `Role::Orchestrator` rather than `Role::is_root()`, which is a
        // deliberate narrowing of #3304 Q5 and is argued in
        // `doc/design/delivery-triage.md`: `is_root()` also admits
        // `Role::Lead`, the human's OWN pane, and holding a notice back from a
        // pane a human is sitting in front of is a different product decision
        // from cutting an agent's wakes — which is what #3304 measured. The
        // narrowing can only ever deliver more.
        if role != Role::Orchestrator || delivery != Delivery::MidSession {
            return Triaged::Deliver { flush: None };
        }
        let policy = self.triage_policy(group);
        if !policy.enabled {
            return Triaged::Deliver { flush: None };
        }
        // "The sender is the human." A human never reaches this function by
        // TYPING — they type into the PTY, which is not a delivery at all — so
        // the question the never-triaged set is really asking is whether these
        // are the human's RELAYED words. Those arrive through
        // `deliver_relayed_to_root` from the pane the human is sitting in,
        // which is the manager (`message_orchestrator`) or the lead. Read off
        // the SENDER's role rather than off the text, which is agent-authored
        // and therefore not evidence about who wrote it.
        let human = self
            .agent(from)
            .is_some_and(|a| matches!(a.role, Role::Manager | Role::Lead));
        let input = triage::Input {
            text,
            from,
            human_actor: human,
            merge_queue_enabled: self.merge_queue_enabled(group),
        };
        let decision = triage::decide(&input, &policy.as_triage_policy());
        let kind = triage::classify(text);

        // A satisfied gate earns its rule by the ENQUEUE, not by its shape:
        // the orchestrator's own next step for one is `queue_merge`, so a
        // notice suppressed without one having happened is a PR nobody is
        // driving. The attempt is made here, and any refusal — a gate the
        // queue re-check does not accept, a state file it cannot read, a
        // repo it cannot resolve — falls back to delivering.
        let decision = match decision {
            triage::Decision::TryEnqueue { pr } => {
                let reply = self.queue_merge(group, pr, None);
                let refused = reply.get("refused").and_then(|v| v.as_str());
                self.audit(group, brand::AUDIT_ACTOR, "delivery-triage-enqueue", json!({
                    "pr": pr, "refused": refused, "to": to,
                }));
                match refused {
                    None => triage::Decision::Defer(triage::Rule::GateSatisfied),
                    Some(_) => triage::Decision::Deliver(triage::DeliverReason::NoRule),
                }
            }
            other => other,
        };

        // The deferral lock is taken HERE and not one line earlier: the
        // policy read above takes `groups` and the enqueue takes
        // `mq_state_lock`, and `lockorder::TRIAGE_DEFER` is inner of both.
        // Holding it across either would be the inversion this rank exists to
        // catch.
        let _guard = self.triage_defer_lock.lock_safe();
        match decision {
            triage::Decision::Defer(rule) => {
                let mut store = self.load_deferred(group);
                store.push(triage::Entry {
                    ts_ms: now_ms(),
                    from: from.to_string(),
                    kind: kind.as_str().to_string(),
                    rule: rule.as_str().to_string(),
                    text: text.to_string(),
                });
                // The store is written BEFORE the notice is dropped from the
                // delivery path. A write that fails delivers instead: a
                // deferral nobody recorded is a notice lost at the next
                // restart, and losing one is worse than spending a wake.
                match self.store_deferred(group, &store) {
                    Ok(()) => {
                        self.audit_triaged(group, to, from, kind, &format!("rule:{}", rule.as_str()));
                        Triaged::Deferred
                    }
                    Err(why) => {
                        self.audit(group, brand::AUDIT_ACTOR, "delivery-triage-fault", json!({
                            "at": "store", "why": why, "to": to,
                        }));
                        self.audit_triaged(group, to, from, kind, "delivered");
                        Triaged::Deliver { flush: None }
                    }
                }
            }
            triage::Decision::Deliver(reason) => {
                self.audit_triaged(group, to, from, kind, reason.as_str());
                Triaged::Deliver {
                    flush: self.take_deferred_locked(group, to, triage::FlushCause::Wake),
                }
            }
            // Unreachable: the arm above rewrote it. Delivering is the
            // fail-safe answer if that ever stops being true.
            triage::Decision::TryEnqueue { .. } => Triaged::Deliver { flush: None },
        }
    }

    /// One `delivery-triaged` row. `action` is `deferred`'s spelling
    /// (`rule:<name>`), or `delivered`'s reason.
    fn audit_triaged(&self, group: &GroupId, to: &str, from: &str, kind: triage::Kind, action: &str) {
        self.audit(group, brand::AUDIT_ACTOR, "delivery-triaged", json!({
            "to": to, "from": from, "kind": kind.as_str(), "action": action,
        }));
    }

    /// Empty the store and return the framed notice for what was in it, or
    /// `None` when there was nothing held.
    ///
    /// The file is cleared BEFORE the frame is delivered, which is the safe
    /// order for the failure that matters: a crash between the two costs one
    /// frame, while clearing afterwards would risk delivering the same frame
    /// twice on every restart. Both are audited, and the notices themselves
    /// are also in the audit log either way.
    ///
    /// **The caller holds `triage_defer_lock`** — the `_locked` suffix is the
    /// contract, since the lock is not re-entrant and `triage_delivery` is
    /// already inside it when it flushes in front of a wake.
    fn take_deferred_locked(
        &self,
        group: &GroupId,
        to: &str,
        cause: triage::FlushCause,
    ) -> Option<String> {
        let store = self.load_deferred(group);
        if store.is_empty() {
            return None;
        }
        let notice = store.flush_notice(now_ms(), cause)?;
        if let Err(why) = self.store_deferred(group, &triage::Deferred::default()) {
            // The frame still goes out. The residual is the one this order
            // chooses: the file may still name notices the orchestrator has
            // now read, so the next flush repeats them. Repeating a notice is
            // a spent wake; not clearing and not delivering would be a pane
            // that never hears about them at all.
            self.audit(group, brand::AUDIT_ACTOR, "delivery-triage-fault", json!({
                "at": "clear", "why": why, "to": to,
            }));
        }
        self.audit(group, brand::AUDIT_ACTOR, "delivery-triage-flushed", json!({
            "to": to, "cause": cause.as_str(), "count": store.len(),
        }));
        Some(notice)
    }

    /// Bound (2): the deferral deadline, and bound (3): the store cap. Called
    /// on the watchdog's own timer, so a group that is woken by nothing at all
    /// still hears about what was held, within `max_defer_minutes`.
    ///
    /// Returns the group ids it flushed, for the tests.
    pub fn triage_flush_tick(&self, now_ms: u64) -> Vec<GroupId> {
        // Which groups have an orchestrator at all, and which pane it is.
        let roots: Vec<(GroupId, String)> = self
            .agents
            .lock_safe()
            .values()
            .filter(|a| a.role == Role::Orchestrator)
            .map(|a| (a.group.clone(), a.id.clone()))
            .collect();
        let mut flushed = Vec::new();
        for (group, orch) in roots {
            // A paused group delivers nothing at all, and a flush is a
            // delivery: spending the frame now would put it in a queue the
            // human deliberately stopped, and the resume replays it anyway.
            if self.is_paused(&group) {
                continue;
            }
            let policy = self.triage_policy(&group);
            if !policy.enabled {
                continue;
            }
            // Load-decide-take under ONE acquisition, so a delivery landing
            // mid-tick cannot have its deferral erased by a flush that read the
            // file before it (`mq_state_lock`'s own lost-update shape).
            // Released before the delivery below: no registry state lock is
            // ever held across one (#467/#468).
            let notice = {
                let _guard = self.triage_defer_lock.lock_safe();
                let store = self.load_deferred(&group);
                match store.due(now_ms, policy.max_defer_minutes) {
                    None => continue,
                    Some(cause) => self.take_deferred_locked(&group, &orch, cause),
                }
            };
            let Some(notice) = notice else { continue };
            let _ = self.deliver_prompt(&orch, &notice, brand::AUDIT_ACTOR, Delivery::MidSession);
            flushed.push(group);
        }
        flushed
    }

    /// `list_deferred()` — the read-back. Every held notice, oldest first,
    /// plus the policy in force, so a reader can tell "nothing is held" from
    /// "triage is off".
    pub fn deferred_list(&self, group: &GroupId) -> serde_json::Value {
        let policy = self.triage_policy(group);
        let _guard = self.triage_defer_lock.lock_safe();
        let store = self.load_deferred(group);
        json!({
            "enabled": policy.enabled,
            "provider": policy.provider,
            "max_defer_minutes": policy.max_defer_minutes,
            "count": store.len(),
            "items": store
                .entries
                .iter()
                .map(|e| json!({
                    "ts_ms": e.ts_ms,
                    "from": e.from,
                    "kind": e.kind,
                    "rule": e.rule,
                    "text": e.text,
                }))
                .collect::<Vec<_>>(),
        })
    }
}
