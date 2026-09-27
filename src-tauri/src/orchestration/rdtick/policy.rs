//! The review driver's policy reads (#1778 S3): the group's `driver:` policy and
//! whether the driver runs at all, the auto-drive-on-done switch, the live-drive
//! PR list, the `gh` runner override and §2.4's per-group defer.
//!
//! Design note: `docs/design/review-driver.md`. Part of `rdtick/`, the driver's
//! registry wiring, which `tests/reviewdrive.rs` scans whole (see `mod.rs`).

use super::*;

impl OrchRegistry {
    // ---------- the review-loop DRIVER (#1778) ----------

    /// This group's `driver:` policy (§5.3): whether the feature is on at all,
    /// and the bounds one drive runs against.
    ///
    /// **The two conditions are the merge queue's**, for `merge_queue_enabled`'s
    /// reason: the `advanced_orchestrator` toggle governs the workflow file, and
    /// the `driver:` block is part of that file, so a policy that read its own
    /// block alone would be live in a group that opted out of the whole
    /// workflow.
    ///
    /// **Off is the answer to every uncertainty** — no group, no advanced
    /// orchestrator, no workflow file, a file that will not parse. That is
    /// §5.3's "an absent block means the feature is off and behaviour is
    /// byte-for-byte unchanged", and it is the opposite of `board_policy`'s
    /// fail-open on purpose: a pacing discipline that fails open costs a warning
    /// nobody reads, while a driver that fails open spawns reviewers into a repo
    /// that never asked for one.
    ///
    /// **The `driver:` block, mapped through `DriveLimits::new`, which clamps.**
    /// S2 already refused an out-of-range value as it parsed; this clamps again
    /// on the values actually read, and the second is not redundant — `decide`
    /// is a `pub fn` over a plain value type that any caller in any crate can
    /// reach without passing through S2's parser, and a boundary that holds only
    /// when the expected caller is upstream is not a boundary. The two layers
    /// enforce independently on purpose; `the_two_layers_agree_on_invariant_9`
    /// is what stops them disagreeing about the VALUE.
    ///
    /// The timeouts pass through unclamped here because §5.3 does not bound them
    /// against INVARIANT 9 — they are pacing, not budget, and S2 clamps them to
    /// the notify-TTL family as it parses.
    pub(super) fn driver_policy(&self, group: &GroupId) -> (bool, reviewdrive::DriveLimits) {
        let Some(g) = self.group(group) else {
            return (false, reviewdrive::DriveLimits::default());
        };
        self.driver_policy_for(&g.repo, &g.guardrails)
    }

    /// The same policy read from a group's OWN record rather than from the
    /// registry's map (#3040 P4).
    ///
    /// `instruction_vars` needs this and the id-taking form cannot serve it:
    /// `create_group` renders the instruction files BEFORE it inserts the group
    /// into `self.groups`, so `self.group(id)` answers `None` there and every
    /// policy read off it comes back `off`. That is silent in the direction
    /// that matters — the fragment renders EMPTY, which is exactly what a
    /// driverless group's playbook looks like — so a group created with a driver
    /// read a playbook that never mentioned it until something re-applied its
    /// workflow. One policy, two ways in.
    pub(in crate::orchestration) fn driver_policy_for(
        &self,
        repo: &str,
        guardrails: &super::Guardrails,
    ) -> (bool, reviewdrive::DriveLimits) {
        let off = (false, reviewdrive::DriveLimits::default());
        if !guardrails.advanced_orchestrator {
            return off;
        }
        let Ok(Some(wf)) = super::load_active_workflow(repo, guardrails) else { return off };
        let d = wf.driver;
        (
            d.enabled,
            reviewdrive::DriveLimits::new(
                d.max_review_rounds,
                d.max_ci_attempts,
                d.max_rebase_attempts,
                d.lane_timeout_minutes as u64,
                d.fix_timeout_minutes as u64,
                d.drive_timeout_minutes as u64,
            )
            // #3367 item 1, through the clamping builder like every other
            // bound this maps.
            .with_fix_nonblocking_rounds(d.fix_nonblocking_rounds),
        )
    }

    /// **Does a worker's `report(done, ref: <PR>)` start a drive here?**
    /// (#3367 item 2, `driver.auto_drive_on_done`)
    ///
    /// Read UNDER `enabled`, through the same policy gate as everything else
    /// the driver does — the advanced-orchestrator guardrail and a workflow
    /// that loads — so a group whose `drive_review` answers `driver-disabled`
    /// can never be started by a report either. Fails closed: a workflow that
    /// does not load is `false`, and the report is delivered as it always was.
    pub(in crate::orchestration) fn rd_auto_drive_on_done(&self, group: &GroupId) -> bool {
        let Some(g) = self.group(group) else { return false };
        if !g.guardrails.advanced_orchestrator {
            return false;
        }
        match super::load_active_workflow(&g.repo, &g.guardrails) {
            Ok(Some(wf)) => wf.driver.enabled && wf.driver.auto_drive_on_done,
            _ => false,
        }
    }

    /// Whether this group runs a review driver at all.
    ///
    /// `pub(super)` because `instruction_vars` in the parent module gates
    /// `{{REVIEW_DRIVER}}` on it: a private item is visible to its own module
    /// and that module's DESCENDANTS, and `orchestration` is this file's
    /// parent, not its child. Widened to exactly the parent and no further, so
    /// the one reader outside this file is the template gate — which has to
    /// read the same policy the tick does, or a group whose tools all refuse
    /// `driver-disabled` could be told in its instructions that it has a driver.
    pub(in crate::orchestration) fn driver_enabled(&self, group: &GroupId) -> bool {
        self.driver_policy(group).0
    }

    /// [`driver_enabled`](Self::driver_enabled) for a group not yet in the map.
    pub(in crate::orchestration) fn driver_enabled_for(&self, repo: &str, guardrails: &super::Guardrails) -> bool {
        self.driver_policy_for(repo, guardrails).0
    }

    /// The PRs this group has an unfinished review drive on, read off disk
    /// whatever the policy says (#3330) — for the notice that tells the
    /// orchestrator which drives a workflow file that will not load is holding
    /// still. Held drives count: they are still the orchestrator's to resume.
    ///
    /// `None` when the record cannot be read, which is NOT "no drives" — the
    /// same distinction `review_drive_status` keeps with `rd-state-unreadable`.
    /// Under `rd_state_lock`, like every other reader of the file.
    pub(in crate::orchestration) fn rd_live_drive_prs(&self, group: &GroupId) -> Option<Vec<u64>> {
        let dir = self.group_dir(group);
        let _state_guard = self.rd_state_lock.lock_safe();
        let state = reviewdrive::load_state(&dir).ok()?;
        Some(state.entries.iter().filter(|e| !e.state().is_terminal()).map(|e| e.pr).collect())
    }

    /// Install (or clear) the canned `gh` the driver reads through —
    /// `mq_runner_override`'s twin. `None` in the app, always.
    #[doc(hidden)] // pub for integration tests
    pub fn set_rd_runner_override(&self, runner: Option<Arc<dyn rddrive::RdRunner>>) {
        *self.rd_runner_override.lock_safe() = runner;
    }

    /// Hold `group` off until `at` (§2.4's rate bound).
    pub(super) fn rd_defer(&self, group: &GroupId, at: u64) {
        self.rd_service_ms.lock_safe().insert(group.clone(), at);
    }
}
