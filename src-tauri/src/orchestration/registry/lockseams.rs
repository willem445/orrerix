//! Test seams that hold one of the registry's OWN locks — on the calling
//! thread (`with_lock_for_test`, `lock_within_for_test`) or on a thread of
//! its own for a real interval (`hold_lock_for_test`) — for the lock-liveness
//! and lock-order suites (#1601, #1610), as an `impl OrchRegistry` block
//! (#3498). None is a `#[tauri::command]` or reachable from an agent. The
//! designs are `docs/design/lock-liveness.md` and `docs/design/lock-order.md`.

use super::*;

impl OrchRegistry {
    #[allow(dead_code)]
    fn planted_long_hold_for_scratch(&self) {
        let _permit = loomux_engine::lockwatch::LongHoldPermit::new("planted");
    }

    /// Run `f` with the named registry lock held ON THE CALLING THREAD (#1610).
    ///
    /// `None` for a name this does not know.
    ///
    /// The sibling of [`Self::hold_lock_for_test`], and the difference is the
    /// whole reason it exists: that one holds a lock on a *spawned* thread,
    /// which is what a contention test needs and what a lock-ORDER test can
    /// never use. The order checker is per-thread, so a nesting has to happen
    /// on one thread to be a nesting at all.
    ///
    /// **Deliberately not a `#[tauri::command]` and not reachable from an
    /// agent**, for `hold_lock_for_test`'s reason: this composes deadlocks.
    ///
    /// The names are a representative handful rather than all eighty-odd —
    /// enough to nest a declared pair in both directions and to re-enter one
    /// lock, which is what L5 asks. `audit` is included because it is the
    /// innermost rank in the table and the one every refusal path takes.
    #[doc(hidden)]
    pub fn with_lock_for_test<R>(&self, name: &str, f: impl FnOnce() -> R) -> Option<R> {
        // One arm per lock rather than a `Box<dyn Any>` guard: the arms guard
        // different types, and the acquisition has to happen at a real
        // `lock_safe` call site or the recorded site would be this seam for
        // every lock it can hold — which is exactly the half of a violation
        // report that makes it actionable.
        Some(match name {
            "groups" => {
                let _g = self.groups.lock_safe();
                f()
            }
            "agents" => {
                let _g = self.agents.lock_safe();
                f()
            }
            "by_pty" => {
                let _g = self.by_pty.lock_safe();
                f()
            }
            "tasks_lock" => {
                let _g = self.tasks_lock.lock_safe();
                f()
            }
            "needs_you_lock" => {
                let _g = self.needs_you_lock.lock_safe();
                f()
            }
            "audit" => {
                let _g = audit_lock().lock_safe();
                f()
            }
            _ => return None,
        })
    }

    /// Try the named registry lock with an explicit budget, on the CALLING
    /// thread (#1610). `None` for a name this does not know.
    ///
    /// Exists so a liveness test can reach [`TrackedMutex::lock_within`]'s
    /// re-entrancy refusal through a REAL registry field rather than through a
    /// lock the test built itself — the seam question every one of this repo's
    /// "the extracted unit is green while the caller is wrong" findings turns
    /// on. The guard is dropped before returning: the answer under test is
    /// whether the acquisition was refused, not what it protects.
    #[doc(hidden)]
    pub fn lock_within_for_test(
        &self,
        name: &str,
        budget: Duration,
    ) -> Option<Result<(), loomux_engine::lockwatch::Busy>> {
        Some(match name {
            "groups" => self.groups.lock_within(budget).map(|_| ()),
            "agents" => self.agents.lock_within(budget).map(|_| ()),
            "by_pty" => self.by_pty.lock_within(budget).map(|_| ()),
            "tasks_lock" => self.tasks_lock.lock_within(budget).map(|_| ()),
            "needs_you_lock" => self.needs_you_lock.lock_within(budget).map(|_| ()),
            "audit" => audit_lock().lock_within(budget).map(|_| ()),
            _ => return None,
        })
    }

    /// Hold one named registry lock for `ms`, on a thread of its own, returning
    /// once that thread has actually acquired it.
    ///
    /// Test/diagnostic seam (#1601 Phase 0). The instrument this PR builds
    /// reports the state the app is in when it stops answering, and the only
    /// honest way to test that is to CREATE the state — a synthesized snapshot
    /// exercises the reporting rule but never the recording that feeds it. The
    /// Rust-level liveness tests and the E2E injected hold both need one real
    /// registry lock held for a real interval.
    ///
    /// **Deliberately not a `#[tauri::command]`, and not reachable from an
    /// agent.** It is a hang generator; the frontend and the MCP surface have
    /// no business with one. If the E2E soak lane (plan §3 Phase 4.1) needs to
    /// reach it, that is a command behind a debug build flag and its own
    /// argument, not this.
    ///
    /// Returns `false` for a name this does not know. The list is a
    /// representative handful rather than all 82: what a caller needs is a lock
    /// with the right SHAPE — one the poll path takes (`groups`), one a
    /// background thread holds for real work (`mq_state_lock`), one on the MCP
    /// path (`agents`) — and an exhaustive match here would be one more thing
    /// to keep in step with the struct for no gain.
    #[doc(hidden)]
    pub fn hold_lock_for_test(self: &Arc<Self>, name: &str, ms: u64) -> bool {
        let reg = self.clone();
        let (tx, rx) = mpsc::channel::<()>();
        let name = name.to_string();
        // `app` is here for #1609 review B1: it is what `write_mailbox` takes
        // AFTER atomically replacing `mailbox.json`, so it is the lock a test
        // has to hold to produce a write-then-acquire tear at all. Widening
        // this seam widens `l1_...`'s coverage too, which that test's own
        // `classify` helper invites.
        let known =
            matches!(name.as_str(), "groups" | "agents" | "mq_state_lock" | "tasks_lock" | "app");
        if !known {
            return false;
        }
        std::thread::spawn(move || {
            // ARGUED ALLOWLIST ENTRY (#1702 P4). Every hold this seam takes is
            // deliberately longer than `lockwatch::HOLD_FAIL_MS` — that is
            // what it is FOR, and the L-series' whole shape is "a victim
            // answers while a lock is wedged for twenty seconds" — so without
            // this permit the enforcement would fail the suite on its own
            // fixtures. Taken here rather than by the caller because a permit
            // covers the thread that holds it, and the hold happens on THIS
            // one; taken before the acquisition so no window exists in which
            // the hold is live and unpermitted, and dropped with this closure,
            // so nothing this thread does afterwards is exempt.
            let _permit = loomux_engine::lockwatch::LongHoldPermit::new(
                "OrchRegistry::hold_lock_for_test - the liveness suite's deliberate wedge",
            );
            // One guard per arm rather than a `Box<dyn Any>`: the arms guard
            // different types, and the acquisition has to happen at a real
            // `lock_safe` call site or the recorded site would be this seam
            // for every lock it can hold.
            match name.as_str() {
                "groups" => {
                    let _g = reg.groups.lock_safe();
                    let _ = tx.send(());
                    std::thread::sleep(Duration::from_millis(ms));
                }
                "agents" => {
                    let _g = reg.agents.lock_safe();
                    let _ = tx.send(());
                    std::thread::sleep(Duration::from_millis(ms));
                }
                "mq_state_lock" => {
                    let _g = reg.mq_state_lock.lock_safe();
                    let _ = tx.send(());
                    std::thread::sleep(Duration::from_millis(ms));
                }
                "app" => {
                    let _g = reg.app.lock_safe();
                    let _ = tx.send(());
                    std::thread::sleep(Duration::from_millis(ms));
                }
                _ => {
                    let _g = reg.tasks_lock.lock_safe();
                    let _ = tx.send(());
                    std::thread::sleep(Duration::from_millis(ms));
                }
            }
        });
        // Return only once the hold is real, so a caller can assert on it
        // without racing the thread it just started.
        rx.recv().is_ok()
    }
}
