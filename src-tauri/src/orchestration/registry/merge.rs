//! The merge gate and the merge queue: the human's merge and release grants,
//! the workflow merge gate and its reload, the review verdicts it is decided
//! from (`record_verdict`, `gate_status_line`), and the bisecting merge
//! queue with its driver (`queue_merge`, `merge_queue_reconcile`,
//! `mq_drive_group_with`), as an `impl OrchRegistry` block (#3498). The
//! designs are `docs/design/workflows.md` and `docs/design/merge-queue.md`.

use super::*;
// plant: AnswerSource DismissSource ResolveSource

impl OrchRegistry {
    /// Arm/clear the merge gate through the SAME path a fresh launch runs
    /// (`sync_merge_gate`): `Some` writes the spec file the shim reads, `None`
    /// removes it. On a toggle-OFF, `gate` is `None`, so this clears whatever
    /// was armed.
    ///
    /// **Callers hold `group_file_io`** (#762 rev-260 B3). The gate spec and the
    /// `advanced_orchestrator` flag it belongs to are one decision, and two
    /// toggles that commit them in opposite orders leave an armed gate on a
    /// group that is no longer in workflow mode. Split out only so that ordering
    /// is visible at the call site rather than implied by indentation.
    pub(in crate::orchestration) fn sync_merge_gate_locked(
        &self,
        group: &GroupId,
        gate: Option<&workflow::Gate>,
        blocks: &[workflow::Block],
    ) {
        self.sync_merge_gate(group, gate);
        if let Some(g) = gate {
            let missing = workflow::gate_missing_blocks(g, blocks);
            if !missing.is_empty() {
                self.audit(group, brand::AUDIT_ACTOR, "merge-gate-unsatisfiable", json!({
                    "missing_blocks": missing,
                    "reason": "the resolved roster cannot spawn every reviewer this gate names",
                }));
            }
        }
    }

    /// Grant directory for a kind (`merge_grants` | `release_grants`), under the
    /// group state dir. The shim reads these; only human Tauri commands write them.
    fn grant_dir(&self, group: &GroupId, kind: &str) -> PathBuf {
        self.group_dir(group).join(kind)
    }

    /// Write a one-time human **merge** grant for PR `pr` (#83): authorizes exactly
    /// one default-branch merge of that PR within `GRANT_TTL_SECS`, after which the
    /// shim consumes it (one merge) or it expires. `pr` may be a number / `#n` /
    /// PR URL — normalized to a number; the grant is keyed `merge_grants/pr-<N>` so
    /// a grant for #5 can't authorize merging #7. Optional `comment` is delivered
    /// to the orchestrator alongside the authorization (approve-with-comment).
    /// Written atomically so the shim can never read a half-written grant. Returns
    /// the normalized PR number.
    ///
    /// **Human-only boundary:** this is reachable ONLY through Tauri commands (the
    /// board Approve button / a human grant action). No MCP tool calls it — agents
    /// run the shim and consume grants, they never mint them.
    pub fn grant_merge(
        &self,
        group: &GroupId,
        pr: &str,
        comment: Option<&str>,
        actor: &str,
    ) -> Result<u64, String> {
        let num = self.mint_merge_grant(group, pr, actor)?;
        // One grant, no plain approvals: `merge_grant_notice` reproduces the
        // single-grant wording byte-for-byte, so the bulk path (#507) shares
        // this builder without changing what a single Approve delivers.
        let msg = merge_grant_notice(
            &[GrantedPr { num, note: comment.map(str::trim).filter(|c| !c.is_empty()) }],
            &[],
            GRANT_TTL_SECS / 60,
        );
        let _ = self.deliver_to_orchestrator(group, &msg, "human");
        Ok(num)
    }

    /// Write the grant file + audit line for one PR, with **no delivery** —
    /// the authority half of `grant_merge`, split out so a bulk approval
    /// (#507) can mint the same per-PR, single-use, expiring grants it always
    /// did and then say so ONCE, instead of once per PR. Nothing about the
    /// grant itself changes with batch size: bulk changes delivery, not
    /// authority, and there is deliberately no "bulk grant" object for the
    /// shim to honour.
    pub(in crate::orchestration) fn mint_merge_grant(&self, group: &GroupId, pr: &str, actor: &str) -> Result<u64, String> {
        if self.group(group).is_none() {
            return Err("unknown group".into());
        }
        let num = pr_number(pr).ok_or_else(|| format!("no PR number found in {pr:?}"))?;
        let nonce = GRANT_SEQ.fetch_add(1, Ordering::Relaxed);
        let expires = now_ms() / 1000 + GRANT_TTL_SECS;
        let path = self.grant_dir(group, "merge_grants").join(format!("pr-{num}"));
        atomic_write(&path, format!("{expires}\n{nonce}\n").as_bytes()).map_err(|e| e.to_string())?;
        self.audit(group, actor, "merge-grant-written",
            json!({ "pr": num, "expires_secs": expires, "nonce": nonce }));
        Ok(num)
    }

    /// Write a human **release/tag** grant for `tag` (#83, widened by #438):
    /// authorizes the release PIPELINE for that one tag — the `git push` of the
    /// tag, `gh release create|edit|delete <tag>`, and the release-API writes
    /// against that tag's release (the notes write, addressed by canonical
    /// release id, which the shim resolves back to a tag; #437) — for
    /// `RELEASE_GRANT_TTL_SECS`. It is checked on every step, never consumed, so
    /// one authorization covers one release instead of one command; what bounds
    /// it is the tag and the TTL. It does NOT cover the version-bump PR's merge:
    /// that stays under the merge gate, needing its own Approve.
    ///
    /// Releases publish to the world, so — unlike merges — they are NEVER
    /// blanket-allowed by autonomous+auto_merge; each needs an explicit grant.
    /// Optional `comment` delivered to the orchestrator. Human-only, same
    /// boundary as `grant_merge`.
    pub fn grant_release(
        &self,
        group: &GroupId,
        tag: &str,
        comment: Option<&str>,
        actor: &str,
    ) -> Result<(), String> {
        if self.group(group).is_none() {
            return Err("unknown group".into());
        }
        let tag = tag.trim();
        if tag.is_empty() {
            return Err("release grant needs a tag".into());
        }
        let seg = grant_segment(tag);
        let nonce = GRANT_SEQ.fetch_add(1, Ordering::Relaxed);
        let expires = now_ms() / 1000 + RELEASE_GRANT_TTL_SECS;
        let path = self.grant_dir(group, "release_grants").join(&seg);
        atomic_write(&path, format!("{expires}\n{nonce}\n").as_bytes()).map_err(|e| e.to_string())?;
        self.audit(group, actor, "release-grant-written",
            json!({ "tag": tag, "expires_secs": expires, "nonce": nonce }));
        let mins = RELEASE_GRANT_TTL_SECS / 60;
        let note = comment.map(str::trim).filter(|c| !c.is_empty());
        // Says what the grant now actually covers (#438). The old wording —
        // "a one-time release/tag publish … publish THAT release/tag once" —
        // would now understate the authorization, and an agent that believes its
        // grant is spent goes back to the human for a second one, which is the
        // whole bug. It is equally explicit about the two edges: one tag, and the
        // bump PR is not included.
        let msg = match note {
            Some(c) => format!(
                "[orrerix] the human GRANTED the release of {tag} (valid ~{mins} min). This covers the WHOLE \
                 pipeline for THAT tag — pushing {tag}, creating/editing its GitHub release, and writing its \
                 release notes — for the whole window, so do NOT come back for a second grant mid-release. It \
                 covers no other tag or release, and NOT the version-bump PR's merge (that still needs Approve). \
                 Note from the human: {c}\nReport when the release is done."),
            None => format!(
                "[orrerix] the human GRANTED the release of {tag} (valid ~{mins} min). This covers the WHOLE \
                 pipeline for THAT tag — pushing {tag}, creating/editing its GitHub release, and writing its \
                 release notes — for the whole window, so do NOT come back for a second grant mid-release. It \
                 covers no other tag or release, and NOT the version-bump PR's merge (that still needs Approve). \
                 Report when the release is done."),
        };
        let _ = self.deliver_to_orchestrator(group, &msg, "human");
        Ok(())
    }

    // ---------- the workflow merge gate + review verdicts (#222 / #197) ----------

    /// The group-dir spec file the `gh` shim reads to enforce `gates.merge`.
    /// Absent = no declared gate = the pre-#222 flow, exactly.
    pub(in crate::orchestration) fn merge_gate_path(&self, group: &GroupId) -> PathBuf {
        self.group_dir(group).join(workflow::MERGE_GATE_FILE)
    }

    /// The group's declared merge gate, or `None` if the repo declared none **or
    /// the gate file is unusable**. Callers must not read `None` as "no gate" when
    /// the file exists (`merge_gate_declared`) — the shim will read that file and
    /// refuse on exactly what makes this return `None`. The shim does its own read
    /// (in shell); this is for the Rust side — reporting gate status back to a
    /// reviewer that just recorded a verdict, and to the orchestrator that has to
    /// decide what to do next.
    pub fn merge_gate(&self, group: &GroupId) -> Option<workflow::Gate> {
        workflow::parse_gate_file(&fs::read_to_string(self.merge_gate_path(group)).ok()?)
    }

    /// Whether the group has a gate file at all — the shim's own precondition.
    pub fn merge_gate_declared(&self, group: &GroupId) -> bool {
        self.merge_gate_path(group).is_file()
    }

    /// Bring the group's `merge_gate` file in line with the repo's workflow file,
    /// called on every group create/resume.
    ///
    /// `Some(gate)` writes it; `None` **removes** it — a repo that deletes its
    /// `gates.merge` clause (or its whole workflow file) must not keep a gate the
    /// file no longer declares. The one case that is *not* routed here is a
    /// workflow file that fails to parse: `create_group` leaves an existing gate
    /// file alone there, because "I can't read your workflow" is not evidence that
    /// you stopped wanting the gate — see the call site.
    pub(in crate::orchestration) fn sync_merge_gate(&self, group: &GroupId, gate: Option<&workflow::Gate>) {
        let path = self.merge_gate_path(group);
        match gate {
            Some(g) => {
                if atomic_write(&path, workflow::gate_file_text(g).as_bytes()).is_ok() {
                    self.audit(group, brand::AUDIT_ACTOR, "merge-gate-declared", json!({
                        "require": match g.require {
                            workflow::GateRequire::AllPass => "all-pass".to_string(),
                            workflow::GateRequire::Threshold(n) => format!("threshold {n}"),
                        },
                        "reviewers": g.reviewers,
                        "also": g.also,
                        // Say it out loud in the trail: an `also:` condition this
                        // build can't check refuses every merge (fail closed).
                        "unsupported_conditions": g.also.iter()
                            .filter(|c| !workflow::condition_supported(c)).collect::<Vec<_>>(),
                    }));
                }
            }
            None => {
                if path.is_file() && remove_marker(&path).is_ok() {
                    self.audit(group, brand::AUDIT_ACTOR, "merge-gate-cleared",
                        json!({ "reason": "the repo's workflow declares no gates.merge" }));
                }
            }
        }
    }

    /// Whether a `.loomux/workflow.yml` read for the reload path can be
    /// trusted: the file's length agrees across a stat taken immediately
    /// BEFORE the read and one taken immediately AFTER it, and both agree
    /// with what was actually read. A mismatch means the file changed size
    /// WHILE we were reading it — caught it mid-write — so nothing about its
    /// content can be trusted this tick, not just whether `gates.merge` is
    /// present but every field inside it (a `reviewers:` list truncated
    /// mid-sequence reads the same way). Pure so the three-way comparison is
    /// unit-testable with no filesystem at all (`workflow_gate_reload_tests`
    /// below); `read_workflow_stably` is the only caller.
    ///
    /// This is belt-and-braces, not the fix for #385/B1 — it only catches a
    /// write actively in flight during THIS read, at zero timing cost (one
    /// extra stat before, one after; no sleep, no second tick). It does
    /// nothing for a truncated file that's simply been sitting there for a
    /// while (a crashed or paused writer) — no read-timing trick can, which
    /// is exactly why a multi-tick debounce was rejected as "a mitigation,
    /// not a fix": a truncation that outlasts one poll interval outlasts two
    /// just as easily, silently. See `reload_merge_gate_if_changed`'s own
    /// doc comment for the actual fix, which needs no timing signal at all.
    pub(in crate::orchestration) fn workflow_read_is_stable(before_len: u64, read_len: u64, after_len: u64) -> bool {
        before_len == read_len && read_len == after_len
    }

    /// Read the group's ACTIVE workflow file for the reload path,
    /// stability-checked via `workflow_read_is_stable`. `None` for "no file",
    /// "unreadable", or "caught mid-write" alike — `reload_merge_gate_if_
    /// changed` retains the last-known gate on all three without needing to
    /// tell them apart, the same way it already does for a parse error.
    ///
    /// #1689: takes the resolved absolute PATH rather than the repo, so the
    /// caller's `active_workflow_path` is the only place the group's workflow
    /// name becomes a file — this function cannot re-derive it differently.
    fn read_workflow_stably(path: &Path) -> Option<String> {
        let before = fs::metadata(&path).ok()?.len();
        let text = fs::read_to_string(&path).ok()?;
        let after = fs::metadata(&path).ok()?.len();
        Self::workflow_read_is_stable(before, text.len() as u64, after).then_some(text)
    }

    /// **#385**: re-derive one group's merge gate from the repo's CURRENT
    /// workflow file and re-arm it through the SAME `sync_merge_gate`
    /// seam `create_group`'s fresh launch and `set_advanced_orchestrator`'s live
    /// toggle already write through — no second gate-writing mechanism, just a
    /// new trigger for the existing one. Called on a timer by
    /// `run_workflow_gate_reload` for every advanced-orchestrator group, so an
    /// in-place edit to the file takes effect without a relaunch or a manual
    /// toggle off/on.
    ///
    /// Deliberately asymmetric with the launch/toggle-ON path, and this
    /// asymmetry IS the fix, not a shortcut:
    ///   - `Ok(Some(wf))` with `wf.gates.get("merge")` present — the file
    ///     parses AND still names a gate. Sync exactly what it declares, even
    ///     if that WIDENS what the gate accepts (drops a reviewer, drops an
    ///     `also:` clause) — an edit a human can make and commit is real
    ///     consent for THAT gate's shape (#459: accepted risk, not every edit
    ///     is adversarial and most are legitimate).
    ///   - **`wf.gates.get("merge")` is ABSENT while a gate is CURRENTLY
    ///     ARMED (#385/B1).** This does **not** clear the gate, and that is a
    ///     policy decision, not a heuristic gap. `gates` — like every field
    ///     inside a `Gate` — is `#[serde(default)]` (right for a *fresh
    ///     launch*: a zero-config group is legitimate there), so a mid-flush
    ///     truncation that happens to land between two top-level keys parses
    ///     as a perfectly valid document that simply never reaches `gates:`
    ///     — byte-for-byte indistinguishable, by type alone, from a complete
    ///     file that genuinely declares none. **There is no in-band signal
    ///     that can tell those two apart from the document's own bytes** —
    ///     not a stability check (`read_workflow_stably` only catches a
    ///     write actively in progress, not one that's paused or crashed
    ///     mid-truncation), not a multi-tick debounce (a truncation that
    ///     outlasts one poll outlasts two, just as silently — the reason
    ///     rev-8's debounce proposal was rejected as a mitigation, not a
    ///     fix). So this is resolved by POLICY instead of inference: on the
    ///     reload path, absence of `gates.merge` carries no information and
    ///     is never, by itself, consent to remove an already-armed gate. The
    ///     one and only way to take an armed gate down to "no gate" is the
    ///     explicit `set_advanced_orchestrator(..., false, ...)` toggle-off —
    ///     a single discrete action, not something a recurring background
    ///     read infers from whatever shape a file happens to be in right
    ///     now. (Fresh launch keeps the opposite, permissive reading —
    ///     `create_group`'s own `Ok(Some(wf))` arm — because there a missing
    ///     gate really is the common, legitimate, zero-config case, not a
    ///     race with an in-flight save; that is the "distinguish the two
    ///     paths" #385/B1 asked for, not a change to the type's default.)
    ///   - `Ok(None)` — the file is GONE — or an unreadable/unstable/
    ///     unparseable read: all three retain the last-known gate for the
    ///     same reason as the bullet above, and all three `return` without
    ///     writing the gate. A transient blip self-heals the moment the file
    ///     is next read stably and parses (compared, then synced, on a later
    ///     tick); re-auditing every `WORKFLOW_GATE_POLL_INTERVAL` forever
    ///     would just be log noise — the shim's own
    ///     `merge-gate-workflow-blocked`/`merge-gate-blocked` audit lines
    ///     already record every actual refused merge meanwhile. **The one
    ///     exception is a stable read that does not PARSE** (#3330): that is
    ///     not a blip, it is a file every reader of the workflow now sees as
    ///     absent — the review and plan drivers included, which then read
    ///     OFF with nothing said anywhere. So it is announced, once per
    ///     distinct error set, by `warn_workflow_unparseable`; the gate is
    ///     still retained exactly as before.
    ///
    /// Compares against the CURRENTLY ARMED gate (`self.merge_gate`, which reads
    /// the spec file back) before writing anything, so an unedited file — the
    /// overwhelming majority of ticks — costs one small read-and-parse and
    /// nothing else: no write, no audit line. Only the roster's merge gate is
    /// re-synced here; the roster itself (`guardrails.blocks`) stays
    /// launch/toggle-pinned, same as `set_advanced_orchestrator`'s doc explains
    /// for delegates already spawned under it — reloading the roster live is a
    /// separate, larger-blast-radius change this issue is not scoped to.
    #[doc(hidden)] // pub for integration tests (#385/B1 toggle-off race + removal-audit pins)
    pub fn reload_merge_gate_if_changed(&self, id: &GroupId) {
        let Some(info) = self.group(id) else { return };
        // #1689: the group's ACTIVE file, resolved once here and handed down —
        // the reload arms the gate the human pinned, not `.orrerix/workflow.yml`.
        let path = Path::new(&info.repo).join(active_workflow_path(&info.repo, &info.guardrails));
        let Some(text) = Self::read_workflow_stably(&path) else { return };
        let wf = match workflow::parse_workflow(&text) {
            Ok(wf) => {
                // The file loads again (or always did): a later break is a
                // new transition and is announced again.
                self.workflow_unparseable_warned.lock_safe().remove(id);
                wf
            }
            Err(errors) => {
                self.warn_workflow_unparseable(id, &active_workflow_path(&info.repo, &info.guardrails), &errors);
                return;
            }
        };
        let armed = self.merge_gate(id);
        let fresh = wf.gates.get("merge").cloned();

        // #385/B1: see the doc comment above — absence never clears an
        // already-armed gate on this path. But silence about that is its own
        // failure mode: a human who deletes `gates.merge`, saves, and sees
        // NOTHING happen will reasonably conclude the reload is broken,
        // exactly like the intake-gate incident where a suppressed wake was
        // indistinguishable from a lost one. So this audits — but only ONCE
        // per transition into this state (`merge_gate_removal_warned` is the
        // latch), not on every tick the file happens to still be missing
        // `gates.merge`: a repeat audit every `WORKFLOW_GATE_POLL_INTERVAL`
        // would be exactly the log-spam this feature already avoids for the
        // unchanged-file case.
        if fresh.is_none() && armed.is_some() {
            if self.merge_gate_removal_warned.lock_safe().insert(id.clone()) {
                self.audit(id, brand::AUDIT_ACTOR, "merge-gate-removal-ignored", json!({
                    "reason": "gates.merge is absent from the current read, but a gate is \
                               already armed. The reload path never treats absence as removal \
                               here — it is indistinguishable from a mid-write truncation \
                               (#385/B1) — so the gate stands. Turn workflow mode off to \
                               actually clear it.",
                }));
            }
            return;
        }
        // Any other outcome means the group has left that state (the file
        // regained `gates.merge`, or the gate is genuinely gone) — clear the
        // latch so a LATER removal audits again instead of staying silent
        // forever after the first warning.
        self.merge_gate_removal_warned.lock_safe().remove(id);

        if fresh.as_ref() == armed.as_ref() {
            return;
        }
        // #385: re-check immediately before writing, on a FRESH read of the
        // group's guardrails. The read+parse above takes real wall-clock
        // time, and `set_advanced_orchestrator` runs on a different thread
        // (a Tauri command handler, not this background pass) — a human can
        // toggle workflow mode off while this call is in flight. Without
        // this check, a reload that started just before the toggle-off
        // completes could still finish just after it and wedge the gate
        // back on for a group that just explicitly turned it off. That's
        // the SAFE direction (more enforcement, not less) — but it is still
        // a real bug, and "it fails safe" is not a reason to ship a known
        // race in security machinery.
        let Some(info) = self.group(id) else { return };
        if !info.guardrails.advanced_orchestrator {
            return;
        }
        self.sync_merge_gate(id, fresh.as_ref());
        if let Some(g) = &fresh {
            let missing = workflow::gate_missing_blocks(g, &info.guardrails.blocks);
            if !missing.is_empty() {
                self.audit(id, brand::AUDIT_ACTOR, "merge-gate-unsatisfiable", json!({
                    "missing_blocks": missing,
                    "reason": "the resolved roster cannot spawn every reviewer this gate names",
                }));
            }
        }
    }

    /// **Say ONCE that this group's workflow file does not load** (#3330).
    ///
    /// Every reader of the workflow treats an unparseable file as absent, and
    /// for the review and plan drivers absent means OFF (`driver_policy_for`'s
    /// "off is the answer to every uncertainty"). That is the right direction
    /// for a driver, and it was silent: a group whose file stopped parsing —
    /// an installed build older than a key the file had just gained, under
    /// `deny_unknown_fields` — ran for hours with `review_drive_status`
    /// answering `enabled: false`, a live drive recovered and never ticked,
    /// and nothing in any pane. The launch-time `workflow-invalid` row does not
    /// cover it either: a resume never re-reads the file for its roster, and a
    /// row on the audit log is not something an orchestrator reads.
    ///
    /// So the reload pass — the one place that already re-reads the file on a
    /// timer — writes a `workflow-invalid` row and hands the orchestrator one
    /// line naming the errors, what they turn off, and any review drive on
    /// disk that will sit un-ticked until the file loads.
    ///
    /// **Once per distinct error set, not per pass.** The row is written on the
    /// transition and the line is retried until it lands (an orchestrator pane
    /// that is not up yet must not cost the notice), then latched. A file that
    /// parses clears the latch, and a different error set is a new transition
    /// — a human who fixes one error and trips another hears about the second.
    fn warn_workflow_unparseable(&self, id: &GroupId, path: &str, errors: &[String]) {
        let delivered = {
            let mut warned = self.workflow_unparseable_warned.lock_safe();
            match warned.get(id) {
                Some((seen, delivered)) if seen.as_slice() == errors => Some(*delivered),
                _ => {
                    warned.insert(id.clone(), (errors.to_vec(), false));
                    None
                }
            }
        };
        if delivered == Some(true) {
            return;
        }
        // The drives this file is now holding still: every entry that has not
        // finished. `None` when the record cannot be read — said as such
        // rather than as "none", which is a different fact (#2135's posture).
        let live = self.rd_live_drive_prs(id);
        if delivered.is_none() {
            self.audit(id, brand::AUDIT_ACTOR, "workflow-invalid", json!({
                "path": path,
                "errors": errors,
                "at": "reload",
                "review_drives_unticked": live,
                "action": "the review and plan drivers read OFF until the file loads; the last merge gate is kept",
            }));
        }
        let drives = match &live {
            Some(prs) if prs.is_empty() => "No review drive is on disk.".to_string(),
            Some(prs) => format!(
                "Review drives on disk that will sit un-ticked until it does: {}.",
                prs.iter().map(|p| format!("PR #{p}")).collect::<Vec<_>>().join(", ")
            ),
            None => "orrerix could not read this group's review-drive record, so it cannot say which drives are waiting.".to_string(),
        };
        let text = format!(
            "workflow file {path} does not load, so every reader of it sees no file: the review \
             driver and the plan driver read OFF (drive_review answers driver-disabled) and the \
             merge gate keeps whatever it last armed. {drives} Fix the file, or, if the error \
             names a key this build does not know, install a build that does. Error: {}",
            notify::sanitize_gh_text(&errors.join("; "), 400)
        );
        if self.deliver_to_orchestrator(id, &text, brand::AUDIT_ACTOR).is_ok() {
            if let Some(entry) = self.workflow_unparseable_warned.lock_safe().get_mut(id) {
                entry.1 = true;
            }
        }
    }

    /// One merge-gate hot-reload pass (#385): every non-paused group with
    /// `advanced_orchestrator` on gets `reload_merge_gate_if_changed`. A group
    /// with the toggle off has no live gate to keep in sync (its `merge_gate`
    /// file is cleared, not maintained — see `sync_merge_gate`'s callers), and a
    /// paused group is inactive by the human's own choice, same reasoning
    /// `watchdog_tick`/`idle_tick_tick` already apply to background work.
    /// Snapshots `paused` before locking `groups` (never both locks held at
    /// once — the lock-ordering discipline every other tick in this file
    /// follows). Called on a timer by `start_workflow_gate_reload`.
    pub fn run_workflow_gate_reload(&self) {
        let Some(_tick) = self.tick_gate("run_workflow_gate_reload") else { return () };
        let paused = self.paused.lock_safe().clone();
        let ids: Vec<GroupId> = self
            .groups
            .lock_safe()
            .iter()
            .filter(|(id, g)| g.guardrails.advanced_orchestrator && !paused.contains(*id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.reload_merge_gate_if_changed(&id);
        }
    }

    /// Where this group's recorded verdicts for one PR live: one file per reviewer
    /// block (`verdicts/pr-<N>/<block-id>`). Both segments are loomux-generated —
    /// `pr` is a parsed number and a block id is sanitized to `[A-Za-z0-9_-]` — so
    /// neither can walk out of the group dir.
    fn verdict_dir(&self, group: &GroupId, pr: u64) -> PathBuf {
        self.group_dir(group).join(workflow::VERDICTS_DIR).join(format!("pr-{pr}"))
    }

    /// The PR's current head commit, via the **real** gh (the backend's PATH is
    /// unshimmed, so resolve the binary the same way `write_shim` does rather than
    /// trusting `PATH`). `None` when gh is absent, unauthenticated, or the repo/PR
    /// isn't there — which the verdict path records as an *empty* head, i.e. stale,
    /// never as "unbound, therefore fine".
    ///
    /// `pr_head_override` is the test seam (mirroring `claude_projects_dir`): the
    /// integration tests must be able to record a verdict against a known revision
    /// without a GitHub repo, and every one of them drives the real MCP dispatch.
    ///
    /// `Err` carries **why** rather than collapsing to `None` (#791): a read that
    /// timed out and a PR that does not exist are the same absence to the gate,
    /// and completely different things to the agent staring at the answer.
    fn pr_head(&self, repo: &str, pr: u64) -> Result<String, String> {
        if let Some(sha) = self.pr_head_override.lock_safe().clone() {
            return Ok(sha);
        }
        let out = self
            .gh_capture(
                repo,
                &["pr", "view", &pr.to_string(), "--json", "headRefOid", "--jq", ".headRefOid"],
            )
            .map_err(Self::gh_failure_text)?;
        let sha = workflow::sanitize_sha(&out);
        if sha.is_empty() {
            return Err(format!("gh pr view #{pr}: no head oid in the response"));
        }
        Ok(sha)
    }

    /// Test seam: pretend `gh pr view --json headRefOid` returns this commit for
    /// every PR. Lets the integration tests bind verdicts to a revision (and then
    /// move it, to simulate a re-push) without a live GitHub repo.
    #[doc(hidden)]
    pub fn set_pr_head_override(&self, sha: Option<String>) {
        *self.pr_head_override.lock_safe() = sha;
    }

    /// The PR's **body** right now, via the real gh — the other half of what a
    /// verdict reviews (#565), and on a squash-merging repo the text that becomes
    /// the permanent commit message.
    ///
    /// `Err` when gh is absent, unauthenticated, the read timed out, or the PR
    /// isn't there. That is deliberately distinct from `Ok(String::new())`, a PR
    /// with a genuinely empty body: the first means *unknown* (fail closed — no
    /// digest is recorded and nothing may claim the body is unchanged), the second
    /// is a real body that digests like any other. Since #791 the `Err` also says
    /// which of those it was, because "the body is unreadable" and "the body read
    /// hit its 20-second bound" want different reactions from whoever is reading.
    fn pr_body(&self, repo: &str, pr: u64) -> Result<String, String> {
        if let Some(body) = self.pr_body_override.lock_safe().clone() {
            return Ok(body);
        }
        self.gh_capture(repo, &["pr", "view", &pr.to_string(), "--json", "body", "--jq", ".body"])
            .map_err(Self::gh_failure_text)
    }

    /// The PR's changed paths (#1176), for path-based reviewer routing — through
    /// `workflow::ROUTING_FILES_JQ`, the same reduction the `gh` shim and the
    /// merge queue read, so all three ask GitHub one question and get one answer.
    ///
    /// `Err` covers every way the answer can be incomplete rather than merely
    /// absent: gh unavailable, the PR gone, **and a file list gh could not show
    /// to be whole** (its `files` connection pages at 100 while `changedFiles`
    /// counts them all). Callers hand that to `route_reviewers` as `None`, which
    /// refuses — an unknown reviewer requirement is never an empty one.
    ///
    /// Called ONLY when the gate actually declares routing: a repo that never
    /// wrote the key must not pay a live `gh` call for a feature it does not use.
    fn pr_changed_files(&self, repo: &str, pr: u64) -> Result<Vec<String>, String> {
        if let Some(files) = self.pr_files_override.lock_safe().clone() {
            return Ok(files);
        }
        let out = self
            .gh_capture(
                repo,
                &[
                    "pr",
                    "view",
                    &pr.to_string(),
                    "--json",
                    "files,changedFiles",
                    "--jq",
                    workflow::ROUTING_FILES_JQ,
                ],
            )
            .map_err(Self::gh_failure_text)?;
        workflow::parse_routed_files(&out).ok_or_else(|| {
            format!(
                "gh pr view #{pr}: loomux could not account for every file this PR changed \
                 (its file list pages at 100), so it cannot say which routing rules apply"
            )
        })
    }

    /// Test seam: pretend `gh pr view --json files,changedFiles` reports these
    /// paths for every PR. Lets the integration tests drive #1176's routing
    /// without a live GitHub repo.
    #[doc(hidden)]
    pub fn set_pr_files_override(&self, files: Option<Vec<String>>) {
        *self.pr_files_override.lock_safe() = files;
    }

    /// A failed `gh` read's message, made safe to quote back to an agent
    /// (rev-lead finding 1 on #791).
    ///
    /// The `Err` side of these two reads is **`gh`'s raw stderr** — multi-line,
    /// uncapped, and carrying whatever the remote said. Before #791 it went
    /// nowhere; now it is surfaced deliberately (that is the diagnosability
    /// half of this change), and one of the places it surfaces is the gate line
    /// that `review_verdict` pastes into the ORCHESTRATOR'S PANE inside a
    /// `[orrerix] …` notice. So a stderr carrying a newline plus the literal
    /// `[orrerix]` forges a line that reads as loomux's own — the same forgery
    /// `notify::sanitize_gh_text` was written for, where the attacker-controlled
    /// text was a fork PR's workflow job name.
    ///
    /// Sanitized HERE, at the two functions that mint these errors, rather than
    /// at each place one is displayed: a per-call-site fix leaves the next
    /// consumer of `pr_head`/`pr_body` exposed, and this change added three
    /// consumers at once. Deliberately NOT pushed down into `gh_capture`, whose
    /// other caller is the notify poller — `notify::condition_poll_result`
    /// classifies on stderr text (`Err("no checks reported")` → `Pending`), so
    /// capping and rewriting it there would change poll classification, which
    /// is a different change with a different argument.
    ///
    /// `NOTICE_FIELD_CAP` is the cap for exactly this: one field's worth of
    /// GitHub-derived text entering an `[orrerix]` notice.
    fn gh_failure_text(raw: String) -> String {
        notify::sanitize_gh_text(&raw, notify::NOTICE_FIELD_CAP)
    }

    /// The digest of the PR's body as it stands now — what a recorded verdict's
    /// `body_digest` is compared against. `Err` (carrying why) when the body
    /// can't be read.
    pub(in crate::orchestration) fn pr_body_digest(&self, group: &GroupId, pr: u64) -> Result<String, String> {
        let repo = self.group(group).map(|g| g.repo).unwrap_or_default();
        self.pr_body(&repo, pr).map(|b| workflow::body_digest(&b))
    }

    /// Test seam: pretend `gh pr view --json body` returns this for every PR.
    #[doc(hidden)]
    pub fn set_pr_body_override(&self, body: Option<String>) {
        *self.pr_body_override.lock_safe() = body;
    }

    /// Which reviewers' verdicts were recorded against a PR body that has since
    /// changed (#565), split by verdict class — because the two halves want
    /// **opposite** handling and reporting them as one list is how a fix loop
    /// becomes a ping-pong:
    ///
    /// - `passed` — a `pass` whose body moved afterwards. This is the hazard: what
    ///   is about to become permanent history is not what was approved.
    /// - `blocking` — a `fail`/`escalate` whose body moved afterwards. This is
    ///   *expected*; it is the fix loop working. It is worth SAYING (it is exactly
    ///   the #525 race: a finding about the body, already fixed by the time the
    ///   verdict landed, and no mechanism could tell the orchestrator) but nothing
    ///   about it may auto-stale a verdict — that would make every body fix
    ///   re-trigger the review that asked for it.
    ///
    /// Empty on both counts when the current body cannot be read, or when the
    /// verdicts carry no digest: "cannot tell" is reported by the gate that refuses,
    /// never as a drift claim here.
    ///
    /// `now` is the PR body's digest as it stands, passed IN rather than fetched
    /// here (#791). It is the same fact `list_verdicts` needs to answer
    /// `body_changed` per verdict, and fetching it twice per PR meant the no-arg
    /// fan-out spent three `gh` calls per PR on two facts.
    fn body_drift(&self, group: &GroupId, pr: u64, now: Option<&str>) -> (Vec<String>, Vec<String>) {
        let Some(now) = now else {
            return (Vec::new(), Vec::new());
        };
        let (mut passed, mut blocking) = (Vec::new(), Vec::new());
        for v in self.verdicts(group, pr) {
            if v.body_changed(Some(now)) == Some(true) {
                if v.verdict.is_blocking() {
                    blocking.push(v.block);
                } else {
                    passed.push(v.block);
                }
            }
        }
        (passed, blocking)
    }

    /// Record a reviewer's verdict on a PR (the `review_verdict` MCP tool) — the
    /// durable, attributed state the merge gate reads.
    ///
    /// **Only a reviewer-kind block may record one**, re-checked here and not only
    /// in the MCP dispatch: the verdict is the thing that opens a gate, so the
    /// authorization belongs next to the write. A worker that could file its own
    /// PASS would make the gate decorative.
    ///
    /// The verdict is bound to the PR's **head commit at record time**, so it
    /// cannot survive a re-push: the gate compares that against the PR's current
    /// head and treats a mismatch as outstanding. Without the binding, a `pass` on
    /// #7 still reads green after the worker pushes two more commits — the gate
    /// would be satisfied to the letter of #197 and violated in its spirit.
    ///
    /// Re-recording replaces that reviewer's verdict (a reviewer that re-reviews
    /// after a fix upgrades its own `fail` to a `pass`, and a reviewer whose pass
    /// went stale re-reviews the new head); every write is audited, so the history
    /// is in the trail even though only the latest verdict gates.
    pub fn record_verdict (
        &self,
        group: &GroupId,
        agent_id: &str,
        pr: &str,
        verdict: &str,
        summary: &str,
        open_findings: Option<u32>,
    ) -> Result<(workflow::ReviewVerdict, Vec<String>), String> {
        let a = self.agent(agent_id).ok_or_else(|| format!("unknown agent: {agent_id}"))?;
        if a.group != group {
            return Err(format!("unknown agent: {agent_id}")); // never leak other groups' ids
        }
        if a.role != Role::Reviewer {
            return Err(format!(
                "permission denied: review_verdict records a REVIEW outcome, so only a \
                 reviewer-kind block may call it — you are block {:?} (kind {}). Use \
                 report(status, summary) instead.",
                a.block,
                a.role.as_str()
            ));
        }
        // The deepest of the three layers guarding the verdict against a liaison
        // (#891) — the other two are `mcp::tool_defs`'s listing and `call_tool`'s
        // dispatch arm. A liaison is reviewer-KIND for its posture (persistent,
        // read-only, board-reading) and reviews nothing; the check lives here too,
        // next to the write, for the same reason the class check does: what opens
        // a merge gate must not depend on a single check in a JSON shim.
        //
        // Read from the group's own roster rather than from anything the caller
        // carried in — the same lookup `resolve_token` makes — so this layer is
        // not a second copy of the one the dispatch arm already consulted.
        let caller_hint = self
            .group(group)
            .and_then(|g| g.guardrails.block(&a.block).and_then(|b| b.role_hint.clone()));
        if caller_hint.as_deref() == Some("liaison") {
            return Err(format!(
                "permission denied: block {:?} is a liaison — it presents the human's \
                 questions and relays their answers, and never records the verdict that \
                 opens a merge gate. Use report(status, summary) instead.",
                a.block
            ));
        }
        let num = pr_number(pr)
            .ok_or_else(|| format!("no PR number found in {pr:?} — pass the number, #n, or the PR URL"))?;
        let verdict = workflow::Verdict::parse(verdict).ok_or_else(|| {
            format!("unknown verdict {verdict:?} — must be one of {}", workflow::verdict_names())
        })?;
        let summary = workflow::sanitize_summary(summary);
        if summary.is_empty() {
            return Err("summary required — one or two lines a human can act on: what you \
                        reviewed, and what decided the verdict".into());
        }
        let block = workflow::sanitize_id(&a.block)
            .ok_or("this agent's block id is unusable — it cannot be attributed a verdict")?;
        // The revision this verdict reviewed. Best-effort: an unresolvable head is
        // stored empty, which the gate reads as stale — so a verdict loomux could
        // not bind to a commit can never open a gate on its own.
        let repo = self.group(group).map(|g| g.repo).unwrap_or_default();
        // Both reads are still best-effort — an unresolvable head or body is
        // stored empty and the gate fails closed on it — but the REASON is no
        // longer dropped on the floor (rev-lead, #791). A reviewer whose verdict
        // silently records no head has done everything right and still cannot
        // open the gate, and before this it had nothing to read that said so;
        // "diagnosable rather than silent" is this change's whole point, and a
        // swallowed reason here is the same defect one function over.
        let mut warnings: Vec<String> = Vec::new();
        let head = match self.pr_head(&repo, num) {
            Ok(h) => h,
            Err(why) => {
                warnings.push(format!(
                    "could not resolve PR #{num}'s head commit ({why}) — this verdict is recorded \
                     with an EMPTY head, which the merge gate reads as stale, so it cannot open \
                     the gate on its own. Re-record once `gh` can see the PR."
                ));
                String::new()
            }
        };
        // …and the BODY it reviewed (#565). Computed here, never passed in: a
        // property that depends on the reviewer remembering to include it is an
        // intention, not a mechanism — and a reviewer cannot record the digest of a
        // body it did not read if it never touches the digest at all. Unreadable
        // body → empty, which reads as unknown and can satisfy nothing.
        let body_digest = match self.pr_body(&repo, num) {
            Ok(b) => workflow::body_digest(&b),
            Err(why) => {
                warnings.push(format!(
                    "could not read PR #{num}'s body ({why}) — this verdict records no body \
                     digest, so an `also: body-unchanged` condition cannot be satisfied by it."
                ));
                String::new()
            }
        };
        // #2168 E2: was the brief this verdict answers a body-verification
        // delta? Asked of the DRIVE's own lane record, never of anything the
        // reviewer typed — the same discipline as `body_digest` above, and for
        // a stronger reason: this one lets the gate accept the passes this
        // verdict supersedes, so a reviewer that could set it could open the
        // `body-unchanged` clause for lanes that never read this body.
        //
        // Read after `head` and `body_digest` because it is checked AGAINST
        // them: the grant is scoped to the exact revision the brief named, and
        // a verdict orrerix could not bind to a head or a body never carries it.
        let verified_body =
            self.rd_lane_briefed_verify(group, num, &block, &head, &body_digest);
        let rec = workflow::ReviewVerdict {
            pr: num,
            block,
            agent_id: a.id.clone(),
            verdict,
            head,
            body_digest,
            verified_body,
            // #3367 item 5: reviewer-declared, passed through as given. `None`
            // stays `None` — the clean case must never read an omission as 0.
            open_findings,
            summary,
            ts_ms: now_ms(),
        };
        // Atomic: the shim may read this file at any instant, and a half-written
        // verdict must never read as a `pass` (the first line is the verdict word).
        atomic_write(
            &self.verdict_dir(group, num).join(&rec.block),
            workflow::verdict_file_text(&rec).as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        self.audit(group, &rec.agent_id, "review-verdict", json!({
            "pr": num,
            "block": rec.block,
            "verdict": rec.verdict.as_str(),
            "head": rec.head,
            "body_digest": rec.body_digest,
            // #2168 E2: a verdict that discharges `body-unchanged` for the
            // lanes it supersedes says so on the audit trail, because §5.4's
            // rule is that an audit action names what actually happened — and
            // "this pass was taken to cover two other reviewers' bodies" is the
            // part a reader would otherwise have to infer from a file format.
            "verified_body": rec.verified_body,
            // #3367 item 5 — null when the reviewer did not declare one.
            "open_findings": rec.open_findings,
            "summary": rec.summary.chars().take(500).collect::<String>(),
            "warnings": warnings.clone(),
        }));
        Ok((rec, warnings))
    }

    /// Every verdict recorded for a PR, by reviewer block (block order).
    pub fn verdicts(&self, group: &GroupId, pr: u64) -> Vec<workflow::ReviewVerdict> {
        let mut out: Vec<workflow::ReviewVerdict> = fs::read_dir(self.verdict_dir(group, pr))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let block = e.file_name().to_string_lossy().into_owned();
                let text = fs::read_to_string(e.path()).ok()?;
                workflow::parse_verdict_file(pr, &block, &text)
            })
            .collect();
        out.sort_by(|a, b| a.block.cmp(&b.block));
        out
    }

    /// The verdicts a gate decision reads: reviewer block → its latest record.
    pub(in crate::orchestration) fn verdict_map(&self, group: &GroupId, pr: u64) -> BTreeMap<String, workflow::ReviewVerdict> {
        self.verdicts(group, pr).into_iter().map(|v| (v.block.clone(), v)).collect()
    }

    /// PRs this group has any recorded verdict for (ascending).
    pub fn verdict_prs(&self, group: &GroupId) -> Vec<u64> {
        let mut prs: Vec<u64> = fs::read_dir(self.group_dir(group).join(workflow::VERDICTS_DIR))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_string_lossy().strip_prefix("pr-")?.parse().ok())
            .collect();
        prs.sort_unstable();
        prs
    }

    /// One line telling an agent where a PR stands against the declared gate —
    /// handed back to the reviewer that just voted and delivered to the
    /// orchestrator, so nobody has to guess whether a merge is now possible.
    /// `None` when the group declares no gate (then only the human gate applies).
    ///
    /// This must agree with the shim, which is the thing that actually refuses the
    /// merge: it reads the same files, resolves the same head, and fails closed on
    /// the same shapes. A status line that said SATISFIED while the shim refused
    /// would be worse than no status line at all.
    pub fn gate_status_line(&self, group: &GroupId, pr: u64) -> Option<String> {
        // Sampled here for the callers that have no digest of their own. The
        // gate check comes FIRST: a group with no gate returns `None` without
        // spending a `gh` call on a line it will not print (#791).
        if !self.merge_gate_declared(group) {
            return None;
        }
        let body = self.pr_body_digest(group, pr).ok();
        self.gate_status_line_with(group, pr, body.as_deref())
    }

    /// `gate_status_line` with the PR body's current digest supplied by the
    /// caller (#791) — `None` meaning "could not be read", exactly as a failed
    /// sample here would.
    ///
    /// Every caller that wants this line ALSO wants that digest for its own
    /// answer (`list_verdicts` reports `body_changed` per verdict; a reviewer
    /// recording a verdict has just bound one). Re-fetching it inside made the
    /// no-arg `list_verdicts` fan-out spend three live `gh pr view` calls per PR
    /// for two facts, which on a slow network is three chances to burn the
    /// bound instead of two.
    pub fn gate_status_line_with(
        &self,
        group: &GroupId,
        pr: u64,
        body_digest: Option<&str>,
    ) -> Option<String> {
        if !self.merge_gate_declared(group) {
            return None;
        }
        // The file exists but doesn't parse: the shim refuses every merge on it
        // (`malformed-gate`), so say that rather than "no gate".
        let Some(gate) = self.merge_gate(group) else {
            return Some(format!(
                "merge gate for PR #{pr}: the group's merge_gate file is MALFORMED — every merge \
                 is refused until it is fixed. This self-heals on its own (the next background \
                 reload regenerates it) if that workflow.yml is well-formed; if the refusal \
                 persists, that file is what needs fixing. No relaunch needed either way. \
                 {GATE_REFUSAL_EXITS}"
            ));
        };
        let repo = self.group(group).map(|g| g.repo).unwrap_or_default();
        let head = self.pr_head(&repo, pr);
        // #1176. Resolve path routing FIRST: it decides who the required
        // reviewers are, and everything below counts verdicts from that list.
        // The live `gh` read happens only for a gate that declares routing —
        // `route_reviewers` never looks at the list otherwise, so a repo which
        // never wrote the key pays nothing for a feature it does not use.
        let changed = if gate.routing.is_empty() {
            Err(String::new())
        } else {
            self.pr_changed_files(&repo, pr)
        };
        let Some(routed) =
            workflow::route_reviewers(&gate, changed.as_ref().ok().map(|v| v.as_slice()))
        else {
            // Says what the shim's `routing-unaccountable` refusal says, because
            // it IS that refusal: an unknown reviewer requirement is refused,
            // never assumed empty.
            return Some(format!(
                "merge gate for PR #{pr}: this repo's gate routes reviewers by path, and loomux could not account for every file this PR changed, so it cannot say which lanes are required. The merge is refused until it can.{} {GATE_REFUSAL_EXITS}",
                changed
                    .as_ref()
                    .err()
                    .filter(|e| !e.is_empty())
                    .map(|e| format!(" ({e})"))
                    .unwrap_or_default()
            ));
        };
        // The routed lanes, named with the rules that pulled them in — AC1's
        // "which rules fired and why", on the SATISFACTION side (the shim owns
        // the refusal side, where it is the only thing that can speak).
        // Gated on what routing ADDED, not on whether a rule fired (rev-972 N2).
        // A rule whose reviewers are all already on the static list is legal and
        // supported — `a_routed_reviewer_already_on_the_static_list_is_required_once_not_twice`
        // pins exactly that — and it fires with an EMPTY added-set, which rendered
        // as "Path routing required  on top of…". The shim guards the same way
        // (`[ -z "$g_routed" ] || g_rnote=…`) and says nothing; this now matches it,
        // which is the point: the two halves describe one gate.
        let added = &routed.required[gate.reviewers.len()..];
        let routing_note = if added.is_empty() {
            String::new()
        } else {
            format!(
                " Path routing required {} on top of the gate's own list: {}.",
                added.join(", "),
                routed
                    .fired
                    .iter()
                    .map(|f| format!(
                        "rule {} (paths: {}) matched, requiring {}",
                        f.index,
                        f.paths.join(", "),
                        f.reviewers.join(", ")
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        };
        // Routing spent: from here down this is an ordinary gate whose reviewer
        // list happens to have been derived. One gate decision, not two.
        let gate = routed.gate(&gate);
        let outcome =
            workflow::evaluate_merge_gate(&gate, &self.verdict_map(group, pr), head.as_ref().ok().map(|s| s.as_str()));
        let also = if gate.also.is_empty() {
            String::new()
        } else {
            format!(" Condition(s) checked at merge time: {}.", gate.also.join(", "))
        };
        // "still waiting on X (no verdict) / Y (passed an older revision)".
        let waiting = |outstanding: &[String], stale: &[String]| -> String {
            let mut parts: Vec<String> = Vec::new();
            if !outstanding.is_empty() {
                parts.push(format!("{} (no verdict yet)", outstanding.join(", ")));
            }
            if !stale.is_empty() {
                parts.push(format!(
                    "{} (passed an EARLIER revision — the branch has moved, so they must re-review)",
                    stale.join(", ")
                ));
            }
            parts.join("; ")
        };
        // #565: the body a verdict reviewed is not covered by the head SHA, and on a
        // squash-merging repo it is the commit message. Reported on BOTH classes and
        // enforced on neither here — enforcement is the opt-in `also: body-unchanged`
        // condition, checked at merge time in the shim, exactly like `ci-green`.
        let (drift_passed, drift_blocking) = self.body_drift(group, pr, body_digest);
        // #2168 E2: a required reviewer may have recorded a body-VERIFICATION
        // pass covering the body as it stands, in which case the drifted passes
        // above are ones `body-unchanged` accepts and "have them re-read and
        // re-record" would be a false instruction — the whole point of the
        // delegation is that they are not asked again. Read through the gate's
        // own definition rather than re-derived, and over the ROUTED reviewer
        // list, because a block the gate does not name discharges nothing.
        //
        // **Three cheap conditions before the read, each for its own reason.**
        // No drifted pass: there is nothing to say a different sentence about.
        // No declared `body-unchanged`: on such a repo the clause accepts
        // nothing because it is never checked, so the sentence would be a claim
        // about a condition this gate does not have — the drift is still
        // REPORTED there, exactly as it always was. And the read walks the
        // verdict directory once, into the shared `verdict_map` helper, which
        // the #1889 merge-time evaluation below reuses — the cost #791 spent a
        // slice removing from this very function is not paid a third time.
        let verdict_map = self.verdict_map(group, pr);
        let verified = !drift_passed.is_empty()
            && gate.also.iter().any(|c| c == "body-unchanged")
            && head.as_ref().ok().is_some_and(|h| {
                mergeq::body_verified_by_required(&gate, &verdict_map, h, body_digest)
            });
        // #1889 (option 1): a drifted pass that no #2168 E2 round covers, on a
        // gate that DECLARES `body-unchanged`, is as blocking as a stale head —
        // the body is what a squash merge records — so the SATISFIED headline
        // below gives way to NOT YET SATISFIED naming the lane. `verified` is
        // false precisely when no verification round covers the body, which is
        // the accepted case this must not disturb.
        //
        // **The population is the gate's REQUIRED reviewers** (review round 1,
        // finding 1) — the same one every enforcing half asks
        // (`mergeq::body_unchanged`, `evaluate_merge_gate`, the evaluation
        // below). `body_drift` reports every verdict file on disk, and a block
        // the gate does not name — left behind by an edited `reviewers:` list —
        // must not flip the headline to NOT YET SATISFIED while the shim passes
        // the clause and the merge is not refused. **Nor may a stale lane**
        // (review round 4, W1): `Satisfied` implies full head coverage only
        // under `all-pass` — on a `threshold: N` gate `evaluate_merge_gate`
        // counts live passes against N, so a required lane sitting stale does
        // not stop it — so the filter applies the same liveness predicate the
        // enforcing halves use (`reviewed(head)`, exactly what
        // `mergeq::body_unchanged` and `body_unchanged_failing` below ask) and
        // never the outcome. The FULL drift list is kept for the caveat notes,
        // where reporting all drift is the point.
        let required_drift: Vec<String> = drift_passed
            .iter()
            .filter(|b| gate.reviewers.contains(b))
            .filter(|b| {
                head.as_ref()
                    .ok()
                    .is_some_and(|h| verdict_map.get(b.as_str()).is_some_and(|v| v.reviewed(h)))
            })
            .cloned()
            .collect();
        let drift_headline = matches!(outcome, workflow::GateOutcome::Satisfied)
            && !required_drift.is_empty()
            && gate.also.iter().any(|c| c == "body-unchanged")
            && !verified;
        // #1889 (option 3): which merge-time conditions are failing RIGHT NOW,
        // as far as this line can see. The one condition it has the inputs to
        // evaluate is `body-unchanged`, and it asks exactly what the enforcing
        // halves ask (the shim's clause and `mergeq::body_unchanged`): an
        // unreadable — or EMPTY, the shape review round 4's premortem names, a
        // `gh` read that returned nothing rather than failing — body refuses
        // (`now.filter(|d| !d.is_empty())` is the one normalization both
        // enforcing halves apply), and so does any LIVE pass — bound to the head
        // that would merge — whose digest no longer covers the body as it
        // stands, unless a required reviewer's verification pass covers it.
        // `ci-green` and `base-green` are checked against real `gh` at merge
        // time and this line never spends a `gh` call on them (#791), so it
        // cannot claim to know they fail and keeps the exit; an unknown clause
        // cannot reach a parsed gate (`sanitize_condition` refuses it at parse).
        let body_unchanged_failing = gate.also.iter().any(|c| c == "body-unchanged")
            && (body_digest.filter(|d| !d.is_empty()).is_none()
                || head.as_ref().ok().is_some_and(|h| {
                    let covered =
                        mergeq::body_verified_by_required(&gate, &verdict_map, h, body_digest);
                    gate.reviewers.iter().any(|r| {
                        verdict_map.get(r).is_some_and(|v| {
                            !v.verdict.is_blocking()
                                && v.reviewed(h)
                                && !v.pass_covers_body(h, body_digest, covered)
                        })
                    })
                }));
        let exits = if body_unchanged_failing {
            GATE_REFUSAL_EXITS_WITHOUT_UI_BYPASS
        } else {
            GATE_REFUSAL_EXITS
        };
        let mut body_note = String::new();
        if verified {
            body_note.push_str(&format!(
                " BODY CHANGED SINCE PASS, and VERIFIED SINCE: {} passed an earlier PR body, and \
                 a required reviewer has since read the body as it stands and passed it. This \
                 gate's `body-unchanged` condition accepts that, so those reviewers are NOT \
                 owed a re-read — the code they passed has not moved.",
                drift_passed.join(", ")
            ));
        } else if !drift_passed.is_empty() && !drift_headline {
            // `drift_headline` suppresses this: when the headline already says
            // NOT YET SATISFIED and names the lanes (#1889), the same sentence
            // would repeat it. The gate-does-not-declare case (headline still
            // SATISFIED) keeps it — there it is the only report there is.
            body_note.push_str(&format!(
                " BODY CHANGED SINCE PASS: {} passed a DIFFERENT PR body than the one on the PR \
                 now — and the body is what a squash merge records as the commit message. Have \
                 them re-read and re-record before merging.",
                drift_passed.join(", ")
            ));
        }
        if !drift_blocking.is_empty() {
            body_note.push_str(&format!(
                " BODY CHANGED SINCE a blocking verdict from {}: expected if the finding was \
                 about the body — it may already be fixed. Check the current body before routing \
                 that finding back to the worker; the reviewer clears it by re-recording.",
                drift_blocking.join(", ")
            ));
        }
        let line = match outcome {
            workflow::GateOutcome::Satisfied if drift_headline => format!(
                "merge gate for PR #{pr}: NOT YET SATISFIED — {} passed a different body; \
                 re-record. `gh pr merge` is refused until then.{also} {exits}",
                required_drift.join(", ")
            ),
            workflow::GateOutcome::Satisfied => format!(
                "merge gate for PR #{pr}: SATISFIED by the reviewer verdicts ({}) for the current \
                 revision.{also} The human merge gate still applies on the default branch.",
                gate.reviewers.join(", ")
            ),
            workflow::GateOutcome::Blocked { blocking } => format!(
                "merge gate for PR #{pr}: BLOCKED — {} recorded a fail/escalate verdict. A \
                 blocking verdict beats any number of passes; the PR must be fixed and \
                 re-reviewed. {exits}",
                blocking.join(", ")
            ),
            workflow::GateOutcome::Short { passes, need, outstanding, stale } => format!(
                "merge gate for PR #{pr}: NOT YET SATISFIED — {passes} of {need} required PASS \
                 verdicts cover the PR's current head; still waiting on {}. `gh pr merge` is \
                 refused until then.{also} {exits}",
                waiting(&outstanding, &stale)
            ),
            // #791: name the reason. "Cannot resolve the head" reads the same
            // whether gh is missing, the PR is gone, or the read burned its
            // 20-second bound on a stalled connection — and those want three
            // different reactions from whoever is looking at it. The reason is
            // appended rather than woven in, so the sentence a reader (and the
            // pins on it) already knows stays exactly where it was.
            //
            // The interpolated text is `gh` stderr, and this line is pasted
            // into the orchestrator's pane inside an `[orrerix]` notice by
            // `review_verdict` — it is inert because `pr_head` sanitized it at
            // the source (`gh_failure_text`), which is where the argument for
            // doing it there rather than here lives.
            workflow::GateOutcome::UnknownRevision => format!(
                "merge gate for PR #{pr}: loomux cannot resolve the PR's current head commit, so \
                 it cannot tell whether the recorded verdicts reviewed the code that would merge. \
                 The merge is refused until it can.{} {exits}",
                head.as_ref().err().map(|e| format!(" (gh pr view #{pr}: {e})")).unwrap_or_default()
            ),
        };
        Some(format!("{line}{routing_note}{body_note}"))
    }

    /// Deliver to the group's orchestrator (worker reports, exit notices).
    // ---------- the bisecting merge queue: agent-facing operations (#581 E) ----------

    /// Whether this group's repo declares `merge_queue: enabled: true` (§11.2).
    ///
    /// Read from the repo's workflow file at call time rather than cached: the
    /// file is human-authored policy a human may edit mid-session, and the
    /// existing gate already reloads on change (`reload_merge_gate_if_changed`).
    /// A parse failure reads as **disabled** — the loud `workflow-invalid` path
    /// already reports the parse itself, and a queue running on a file loomux
    /// could not read is a queue nobody can reason about (§11.2).
    /// **Both conditions, not just the block.** The workflow file is only in
    /// force when this group runs the advanced orchestrator — with the toggle
    /// off, `create_group_ex` deliberately *clears* the merge gate so "the
    /// default experience is byte-for-byte pre-#222" holds for the merge path
    /// too. A queue that read the block alone would be live in a group that had
    /// deliberately opted out of the whole workflow, with its gate cleared
    /// underneath it — visible, contradictory, and refusing everything as
    /// `gate-not-configured`. One toggle governs the file; the queue is part of
    /// the file.
    pub(in crate::orchestration) fn merge_queue_enabled(&self, group: &GroupId) -> bool {
        self.merge_queue_policy(group).enabled
    }

    /// This group's `merge_queue:` policy (§11.2) — `max_batch` and
    /// `checks_timeout_minutes` as declared, or the off-by-default when the
    /// block is absent, the file will not parse, or the workflow is not in
    /// force at all.
    ///
    /// One reader for the whole block, so `merge_queue_enabled` and the driver's
    /// per-tick bounds cannot come to different conclusions about whether this
    /// group runs a queue — the failure mode being a driver that batches for a
    /// group whose tools all refuse `queue-disabled`.
    fn merge_queue_policy(&self, group: &GroupId) -> workflow::MergeQueuePolicy {
        let Some(g) = self.group(group) else { return workflow::MergeQueuePolicy::default() };
        if !g.guardrails.advanced_orchestrator {
            return workflow::MergeQueuePolicy::default();
        }
        match load_active_workflow(&g.repo, &g.guardrails) {
            Ok(Some(wf)) => wf.merge_queue,
            _ => workflow::MergeQueuePolicy::default(),
        }
    }

    /// The gate spec the queue re-enforces (§6), read through the same
    /// `merge_gate` file the `gh` shim reads — never a second opinion.
    ///
    /// `Err` is an **I/O fault** distinct from `Ok(GateSpec::Absent)`: a missing
    /// file (`NotFound`) is the repo genuinely declaring no gate and reads as
    /// `Absent`, same as before. Anything else — permission denied, a transient
    /// read failure, non-UTF-8 bytes — means the file may well declare a gate
    /// loomux simply could not read, and collapsing that to `Absent` via
    /// `Result::ok()` is what let `queue_merge` misreport a loomux fault as
    /// `gate-not-configured` (#681). The caller turns `Err` into
    /// `refusal::GATE_UNREADABLE`; `GateSpec::Malformed` (parseable file,
    /// unparseable contents) is unaffected and still refuses `gate-not-met`.
    pub(in crate::orchestration) fn merge_queue_gate(&self, group: &GroupId) -> Result<mergeq::GateSpec, std::io::Error> {
        match fs::read_to_string(self.merge_gate_path(group)) {
            Ok(text) => Ok(mergeq::GateSpec::read(Some(&text))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(mergeq::GateSpec::Absent),
            Err(e) => Err(e),
        }
    }

    /// `queue_merge(pr, target?)` (§11.1). Wiring only: resolve the repo, the
    /// gate and this PR's verdicts, hand them to `mqloop::enqueue`, audit, and
    /// persist. Every decision is the driver module's.
    #[doc(hidden)] // pub for integration tests
    pub fn queue_merge(&self, group: &GroupId, pr: u64, target: Option<&str>) -> Value {
        let runner = match self.group(group).map(|g| g.repo) {
            Some(repo) => mqdriver::runner_for(std::path::Path::new(&repo)),
            // A group loomux cannot resolve is a FAULT, not "the repo did not
            // opt in" (rev-163 NB).
            None => return json!({ "refused": mqloop::refusal::QUEUE_UNAVAILABLE }),
        };
        self.queue_merge_with(group, pr, target, &runner)
    }

    /// The injectable seam — same reason as `merge_queue_reconcile_with`: it is
    /// what lets an integration test drive the whole registry path without
    /// spawning `gh` (constraint 3), which is what makes the `pub` honest.
    #[doc(hidden)] // pub for integration tests
    pub fn queue_merge_with(
        &self,
        group: &GroupId,
        pr: u64,
        target: Option<&str>,
        runner: &dyn mqdriver::MqRunner,
    ) -> Value {
        let enabled = self.merge_queue_enabled(group);
        let dir = self.group_dir(group);
        // #698: serialized against the driver tick, whose own load-decide-store
        // can span a whole batch build. Without it an enqueue that lands mid-
        // build reads the pre-build file and writes it back, erasing the batch
        // record — a lost update whose symptom is a queue that never lands.
        let _state_guard = self.mq_state_lock.lock_safe();
        let mut state = match mqloop::load_state(&dir) {
            Ok(s) => s,
            Err(e) => {
                // A state file loomux cannot read is a FAULT, and saying
                // `queue-disabled` here would tell an orchestrator the repo
                // never opted in — so it would stop, which is the one wrong move
                // (rev-163 NB).
                self.audit(group, brand::AUDIT_ACTOR, mqdriver::audit_action::ENQUEUE_REFUSED,
                    json!({ "pr": pr, "reason": mqloop::refusal::STATE_UNREADABLE, "detail": format!("{e:?}") }));
                return json!({ "refused": mqloop::refusal::STATE_UNREADABLE });
            }
        };
        let gate = match self.merge_queue_gate(group) {
            Ok(g) => g,
            Err(e) => {
                // The gate file is on disk and an I/O error kept loomux from
                // reading it — a FAULT, not "no gate covers this target"
                // (#681, same posture as `STATE_UNREADABLE` above).
                self.audit(group, brand::AUDIT_ACTOR, mqdriver::audit_action::ENQUEUE_REFUSED,
                    json!({ "pr": pr, "reason": mqloop::refusal::GATE_UNREADABLE, "detail": format!("{e:?}") }));
                return json!({ "refused": mqloop::refusal::GATE_UNREADABLE });
            }
        };
        let verdicts = self.verdict_map(group, pr);
        // #710: `enqueue` releases a stale target on a drained queue whatever it
        // then decides, so even a REFUSED enqueue can be a state change worth
        // persisting — otherwise `merge_queue_status` keeps naming a branch the
        // queue is not landing on until the next restart's reconcile. Snapshotted
        // rather than inferred from the outcome, the same way
        // `merge_queue_reconcile_with` decides its write: a value comparison
        // cannot drift out of step with what the callee actually mutates.
        let before = state.clone();
        // §8.1's mutual refusal: the fact is resolved HERE, because
        // `review_drives.json` is a different file in this group dir and
        // reading it is a registry job; the decision is `mqloop::enqueue`'s,
        // beside its opposite number. A `held` drive is deliberately NOT a
        // refusal: it is parked, so it moves nothing and cannot race a
        // batch, and `drive_review` refuses `in-merge-queue` if anyone
        // later tries to resume it under a live queue entry.
        // **A drive record orrerix cannot read is a FAULT, not "not driven".**
        // `unwrap_or(false)` here answered a question it had not been able to
        // ask, and it is the one direction that is unsafe: the queue would
        // enqueue a PR that may be under a live drive, which is exactly the
        // overlap §8.1 forbids. Every other unreadable-state site in this file
        // and in the driver refuses rather than defaulting — `queue-state-
        // unreadable` is ten lines above — and §8 says unknown is never treated
        // as safe. Exposure is small today because the driver also declines to
        // tick on an unreadable file, but the two loops become live together the
        // moment a human repairs it, which is precisely the state §8.1 says
        // neither loop was designed for.
        let driven = match reviewdrive::load_state(&dir) {
            Ok(s) => s.is_driven(pr),
            Err(e) => {
                self.audit(group, brand::AUDIT_ACTOR, mqdriver::audit_action::ENQUEUE_REFUSED,
                    json!({ "pr": pr, "reason": rddrive::refusal::STATE_UNREADABLE, "detail": format!("{e:?}") }));
                return json!({ "refused": rddrive::refusal::STATE_UNREADABLE });
            }
        };
        let outcome = mqloop::enqueue(
            runner, &mut state, pr, target, enabled, &gate, &verdicts, now_ms(), driven,
        );
        match outcome {
            mqloop::EnqueueOutcome::Queued { position } => {
                if let Err(e) = mqloop::store_state(&dir, &state) {
                    // The write is what makes the enqueue durable, so a failed
                    // write is a failed enqueue — reported as such rather than
                    // returning a `queued: true` the next restart would forget.
                    self.audit(group, brand::AUDIT_ACTOR, mqdriver::audit_action::ENQUEUE_REFUSED,
                        json!({ "pr": pr, "reason": mqloop::refusal::STATE_UNWRITABLE, "detail": e }));
                    return json!({ "refused": mqloop::refusal::STATE_UNWRITABLE });
                }
                self.audit(group, brand::AUDIT_ACTOR, mqloop::audit_action::ENQUEUED,
                    json!({ "pr": pr, "target": state.target, "position": position }));
                json!({ "queued": true, "position": position })
            }
            mqloop::EnqueueOutcome::Refused { reason } => {
                // Written only when the refusal actually changed something — a
                // released stale target, today. Unconditionally writing here
                // would create a `merge_queue.json` for any group whose first
                // `queue_merge` is refused, which collapses slice F's `absent`
                // state (`merge_queue_view` distinguishes "never enqueued" from
                // "empty queue"); a default state is already drained, so the
                // release is a no-op there and `state == before` holds.
                let mut detail = json!({ "pr": pr, "reason": reason });
                if state != before {
                    if let Err(e) = mqloop::store_state(&dir, &state) {
                        // The refusal itself stands — it is a decision about this
                        // PR and nothing about the write changes it. But a
                        // release that did not reach disk means the queue still
                        // names a branch it is not landing on, so it is stated in
                        // the audit rather than lost (no silent failures).
                        detail["stale_target_not_released"] = Value::from(e);
                    }
                }
                self.audit(group, brand::AUDIT_ACTOR, mqdriver::audit_action::ENQUEUE_REFUSED, detail);
                json!({ "refused": reason })
            }
        }
    }

    /// `merge_queue_status()` (§11.1). Read-only; never writes, so a status call
    /// cannot conjure a `merge_queue.json` (which would collapse slice F's
    /// `absent` state, the same trap the reconcile write is gated against).
    #[doc(hidden)] // pub for integration tests
    pub fn merge_queue_status(&self, group: &GroupId) -> Value {
        let enabled = self.merge_queue_enabled(group);
        match mqloop::load_state(&self.group_dir(group)) {
            Ok(s) => mqloop::status_view(&s, enabled, now_ms()),
            // An unreadable file is a FACT, and the one thing status must never
            // do is let it read as "nothing queued" — the same posture
            // `mergeqview::merge_queue_view` takes for the chrome.
            Err(e) => json!({ "enabled": enabled, "unreadable": format!("{e:?}") }),
        }
    }

    /// `cancel_queued_merge(pr)` (§11.1).
    #[doc(hidden)] // pub for integration tests
    pub fn cancel_queued_merge(&self, group: &GroupId, pr: u64) -> Value {
        let dir = self.group_dir(group);
        // Same species as `queue_merge`'s two, and not in rev-163's list because
        // it wears a different label: `not-queued` asserts the PR is not in the
        // queue, which loomux cannot possibly know from a state file it could
        // not read. Fixed here rather than left for a later round.
        let _state_guard = self.mq_state_lock.lock_safe(); // #698, see `queue_merge_with`
        let mut state = match mqloop::load_state(&dir) {
            Ok(s) => s,
            Err(e) => {
                self.audit(group, brand::AUDIT_ACTOR, mqloop::audit_action::CANCELLED,
                    json!({ "pr": pr, "reason": mqloop::refusal::STATE_UNREADABLE, "detail": format!("{e:?}") }));
                return json!({ "refused": mqloop::refusal::STATE_UNREADABLE });
            }
        };
        match mqloop::cancel(&mut state, pr) {
            mqloop::CancelOutcome::Cancelled { was } => {
                if let Err(e) = mqloop::store_state(&dir, &state) {
                    // A cancel the next restart forgets is not a cancel — and it
                    // is certainly not "that PR was never queued".
                    self.audit(group, brand::AUDIT_ACTOR, mqloop::audit_action::CANCELLED,
                        json!({ "pr": pr, "reason": mqloop::refusal::STATE_UNWRITABLE, "detail": e }));
                    return json!({ "refused": mqloop::refusal::STATE_UNWRITABLE });
                }
                self.audit(group, brand::AUDIT_ACTOR, mqloop::audit_action::CANCELLED,
                    json!({ "pr": pr, "from": was.as_str() }));
                json!({ "cancelled": true })
            }
            mqloop::CancelOutcome::Refused { reason } => json!({ "refused": reason }),
        }
    }

    // ---------- the bisecting merge queue (#581 slice D2) ----------

    /// Reconcile a group's merge queue against reality after a restart
    /// (`docs/design/merge-queue.md` §4), following `recover_persisted_queue`'s
    /// **two-phase** shape for the reason that function documents: phase 1 runs
    /// under a once-only guard and may not deliver, because this lock is not
    /// reentrant; phase 2 sends what phase 1 collected, after the guard drops.
    ///
    /// **This is wiring, not logic.** Every decision — whether the world matches
    /// the record, which entries strand, what the notices say — is
    /// `mqloop::reconcile_batch`'s. This function resolves the group's repo and
    /// state dir, runs it, audits each transition, persists, and hands the
    /// notices back. That split is the point: the queue's behaviour is testable
    /// without an `AppHandle`, and `mod.rs` holds none of it.
    ///
    /// **Recovery runs before driving, always** (#698). Besides the pane-bind
    /// sites below, `mq_drive_group_with` calls the `_with` half on every tick;
    /// the once-only guard makes that free after the first. §4's recovery is
    /// what decides whether an in-flight batch may be resumed at all, and a
    /// driver that observed one *before* that decision would be acting on a
    /// record recovery was about to strand.
    ///
    /// Called from every pane-bind site, beside `recover_persisted_queue` —
    /// the delivery queue's equivalent — so it runs once per group per process
    /// at a moment when a pane exists to receive phase 2's notices. Group
    /// creation would be too early: no orchestrator is bound yet, so a notice
    /// emitted there would be composed and dropped.
    ///
    /// Builds the real runner and delegates; see
    /// [`merge_queue_reconcile_with`](Self::merge_queue_reconcile_with) for the
    /// seam an integration test drives.
    pub fn merge_queue_reconcile(&self, group: &GroupId) {
        let repo = match self.group(group).map(|g| g.repo) {
            Some(r) => r,
            None => return,
        };
        let runner = mqdriver::runner_for(std::path::Path::new(&repo));
        let notices = self.merge_queue_reconcile_with(group, &runner);
        // ---- phase 2: the guard has dropped; delivering is safe. Same
        // reasoning as `recover_persisted_queue`'s: a notice is a delivery, a
        // delivery enqueues, and an enqueue would re-enter the lock phase 1
        // holds. ----
        for n in notices {
            let _ = self.deliver_to_orchestrator(group, &n, brand::AUDIT_ACTOR);
        }
    }

    /// Phase 1, with the runner injected — the seam `tests/orchestration/`
    /// drives so the whole registry path (the once-only guard, `load_state`,
    /// `reconcile_batch`, the audit emission, `store_state`) is exercised
    /// **without spawning `git` or `gh`** (constraint 3).
    ///
    /// This is what makes the `#[doc(hidden)] pub` below honest: it is public
    /// for a consumer that exists. An earlier cut exposed the whole method that
    /// way for a test that did not exist, which rev-163 correctly called out —
    /// and which is the same "a door connected to nothing passes every test on
    /// hand-built records" failure #661's `e20` was written for.
    ///
    /// Returns phase-2's notices rather than sending them, because sending is
    /// what must not happen under the guard.
    #[doc(hidden)] // pub for integration tests
    pub fn merge_queue_reconcile_with(
        &self,
        group: &GroupId,
        runner: &dyn mqdriver::MqRunner,
    ) -> Vec<String> {
        // ---- phase 1: under the guard ----
        let notices: Vec<String> = {
            let mut done = self.mq_reconciled_groups.lock_safe();
            if !done.insert(group.clone()) {
                return Vec::new();
            }
            // #698: every read-modify-write of this file is serialized, so a
            // concurrent `queue_merge` cannot read the pre-reconcile state and
            // write it back over the recovery. Taken AFTER the once-only guard,
            // which is the lock order every merge-queue path uses.
            let _state_guard = self.mq_state_lock.lock_safe();
            let dir = self.group_dir(group);
            let mut state = match mqloop::load_state(&dir) {
                Ok(s) => s,
                // A file this build cannot act on is audited and **left alone** —
                // §11.2's forward-compatibility promise is that an older build
                // does not destroy what a newer one wrote, and rewriting it here
                // would break exactly that.
                Err(e) => {
                    self.audit(
                        group,
                        brand::AUDIT_ACTOR,
                        mqloop::audit_action::STRANDED,
                        json!({ "reason": "unusable merge_queue.json", "detail": format!("{e:?}") }),
                    );
                    return Vec::new();
                }
            };
            // Snapshotted before the mutation so the write below is decided by
            // what actually changed, not by what the report's shape implies.
            let before = state.clone();
            let report = mqloop::reconcile_batch(runner, &mut state, group);

            for t in &report.transitions {
                self.audit(
                    group,
                    brand::AUDIT_ACTOR,
                    mqloop::audit_action::STRANDED,
                    json!({ "pr": t.pr, "from": t.from.as_str(), "to": t.to.as_str() }),
                );
            }
            if report.resumed {
                self.audit(group, brand::AUDIT_ACTOR, mqloop::audit_action::RECOVERED, json!({}));
            }
            if let Some(why) = &report.cleanup_failed {
                self.audit(
                    group,
                    brand::AUDIT_ACTOR,
                    mqdriver::audit_action::CLEANUP_FAILED,
                    json!({ "detail": why }),
                );
            }
            // Written only when reconcile actually CHANGED something. §4 asks
            // for the snapshot to be rewritten rather than deleted — it does not
            // ask for one to be conjured. Writing unconditionally would create a
            // `merge_queue.json` for every group the moment any pane binds,
            // which would destroy slice F's `absent` state: `merge_queue_view`
            // distinguishes "never enqueued" (the product default, §12) from
            // "empty queue", and an always-present file collapses the two.
            //
            // Compared against a snapshot of the value rather than inferred from
            // the report's shape, so this cannot drift out of step with what
            // `reconcile_batch` mutates.
            if state != before {
                if let Err(e) = mqloop::store_state(&dir, &state) {
                    self.audit(
                        group,
                        brand::AUDIT_ACTOR,
                        mqloop::audit_action::STRANDED,
                        json!({ "reason": "merge_queue.json could not be written", "detail": e }),
                    );
                }
            }
            report.notices
        };
        notices
    }

    // ---------- the merge queue's DRIVER (#698) ----------

    /// **One driver step, for at most one group** — the production caller the
    /// batch pipeline never had (#698).
    ///
    /// Called from `gh_poll_tick`, the unified `gh` poller (#406/#652), because
    /// observing a batch's checks is a `gh` poll like any watch and because that
    /// loop is the one place in this process that makes `gh` calls, with one
    /// shared cadence and one shared budget.
    ///
    /// **At most one group per wake, oldest-serviced first.** That is the whole
    /// per-tick bound, and it is structural rather than a counter: the loop is
    /// shared with every `notify_when` watch in the fleet, and a driver that
    /// serviced N groups on one wake would put N batch builds inside one tick —
    /// the unbounded fan-out #656 exists to stop, in a half that had never had a
    /// cap. Ordering by last-serviced makes the rotation fair, so a group is
    /// deferred rather than starved (the `due_intake_polls` idiom #656 asks for
    /// on the intake half).
    ///
    /// Returns the group serviced, if any — for the audit-free assertion a test
    /// needs that the tick actually reached a group.
    pub fn run_mq_driver_tick(&self) -> Option<GroupId> {
        self.mq_driver_tick(now_ms())
    }

    /// [`run_mq_driver_tick`](Self::run_mq_driver_tick) with the clock injected.
    pub fn mq_driver_tick(&self, now: u64) -> Option<GroupId> {
        let group = self.next_mq_group(now)?;
        // Cloned out and the guard dropped before driving: the override is a
        // one-line lookup and the drive below is a batch build.
        let injected = self.mq_runner_override.lock_safe().clone();
        match injected {
            Some(r) => {
                self.mq_drive_group_with(&group, r.as_ref(), now);
            }
            None => {
                let repo = self.group(&group).map(|g| g.repo)?;
                let runner = mqdriver::runner_for(std::path::Path::new(&repo));
                self.mq_drive_group_with(&group, &runner, now);
            }
        }
        Some(group)
    }

    /// Install (or clear) the canned runner the merge-queue driver uses — the
    /// `mq_runner_override` seam; see that field.
    #[doc(hidden)] // pub for integration tests
    pub fn set_mq_runner_override(&self, runner: Option<Arc<dyn mqdriver::MqRunner>>) {
        *self.mq_runner_override.lock_safe() = runner;
    }

    /// The one group this wake will service, or `None`.
    ///
    /// Three filters, cheapest first, and each of them is a reason not to spend
    /// a subprocess: the group must not be inside a backoff window, it must have
    /// a `merge_queue.json` at all (the product default is that no group does,
    /// §12 — and a *file check* rather than a parse, so an unreadable file still
    /// reaches the driver's own loud handling), and its repo must actually
    /// declare the queue.
    fn next_mq_group(&self, now: u64) -> Option<GroupId> {
        let all: Vec<GroupId> = self.groups.lock_safe().keys().cloned().collect();
        let service = self.mq_service_ms.lock_safe().clone();
        let mut due: Vec<(u64, GroupId)> = all
            .into_iter()
            .filter(|g| service.get(g).map(|t| now >= *t).unwrap_or(true))
            .filter(|g| mqloop::state_path(&self.group_dir(g)).exists())
            .filter(|g| self.merge_queue_enabled(g))
            .map(|g| (service.get(&g).copied().unwrap_or(0), g))
            .collect();
        // Oldest-serviced first; the group id breaks a tie deterministically so
        // two never-serviced groups do not alternate on HashMap iteration order.
        due.sort();
        due.into_iter().next().map(|(_, g)| g)
    }

    /// Hold `group` off until `at`.
    fn mq_defer(&self, group: &GroupId, at: u64) {
        self.mq_service_ms.lock_safe().insert(group.clone(), at);
    }

    /// Drive one group with the runner injected — the seam `tests/orchestration/`
    /// uses to exercise the whole production path (selection, reconcile-first,
    /// `drive`, the audit emission, `store_state`, the notices) **without
    /// spawning `git` or `gh`** (CLAUDE.md constraint 3).
    ///
    /// **This is wiring, not logic.** Every decision is `mqloop::drive`'s. What
    /// lives here is what only the registry can do: resolve the policy, the gate
    /// and the verdict files, hold the state lock across the read-modify-write,
    /// emit the audit events, and deliver the notices.
    ///
    /// **Reconcile runs first, always.** `merge_queue_reconcile_with` is
    /// once-only per group per process, so this is a no-op after the first call
    /// and costs nothing — but it makes the ordering a guarantee rather than an
    /// assumption about which bind site ran when. §4's recovery decides whether
    /// an in-flight batch may be resumed; a driver that observed one *before*
    /// that decision would be acting on a record recovery was about to strand.
    /// Its notices are delivered here, outside the state lock, for the
    /// #467/#468 reason `merge_queue_reconcile` documents.
    #[doc(hidden)] // pub for integration tests
    pub fn mq_drive_group_with(
        &self,
        group: &GroupId,
        runner: &dyn mqdriver::MqRunner,
        now: u64,
    ) -> mqloop::DriveReport {
        for n in self.merge_queue_reconcile_with(group, runner) {
            let _ = self.deliver_to_orchestrator(group, &n, brand::AUDIT_ACTOR);
        }
        let policy = self.merge_queue_policy(group);
        if !policy.enabled {
            // Byte-for-byte unchanged behaviour with no `merge_queue:` block
            // (§12). Checked here as well as in `next_mq_group` because this
            // method is the test seam and a seam that skipped the product's own
            // opt-in would be testing something the product cannot do.
            return mqloop::DriveReport::default();
        }
        let dir = self.group_dir(group);
        let gate = match self.merge_queue_gate(group) {
            Ok(g) => g,
            Err(e) => {
                // #681's posture, which the driver has to keep: a gate file that
                // is on disk and unreadable is a **fault**, not "no gate covers
                // this target". Treating it as `Absent` would still fail closed
                // — every entry would block — but it would block them all with
                // `gate-not-configured`, telling a human the repo declares no
                // gate when the truth is that loomux could not read the one it
                // has. A wrong label sends the reader somewhere else.
                self.audit(
                    group,
                    brand::AUDIT_ACTOR,
                    mqloop::audit_action::STRANDED,
                    json!({ "reason": mqloop::refusal::GATE_UNREADABLE,
                            "detail": format!("{e:?}") }),
                );
                self.mq_defer(group, now.saturating_add(MQ_DRIVE_BACKOFF_MS));
                return mqloop::DriveReport::default();
            }
        };
        let report = {
            let _state_guard = self.mq_state_lock.lock_safe();
            let mut state = match mqloop::load_state(&dir) {
                // A file this build cannot act on is audited and **left alone**
                // — §11.2's promise that an older build does not destroy what a
                // newer one wrote. Backed off too, so an unreadable file is one
                // audit line every few minutes rather than one every tick.
                Err(e) => {
                    self.audit(
                        group,
                        brand::AUDIT_ACTOR,
                        mqloop::audit_action::STRANDED,
                        json!({ "reason": "unusable merge_queue.json", "detail": format!("{e:?}") }),
                    );
                    self.mq_defer(group, now.saturating_add(MQ_DRIVE_BACKOFF_MS));
                    return mqloop::DriveReport::default();
                }
                Ok(s) => s,
            };
            let cfg = mqloop::DriveConfig {
                group,
                max_batch: policy.max_batch,
                checks_timeout_minutes: policy.checks_timeout_minutes,
                now_ms: now,
            };
            let rep =
                mqloop::drive(runner, &mut state, &cfg, &gate, &|pr| self.verdict_map(group, pr));
            if rep.changed {
                if let Err(e) = mqloop::store_state(&dir, &state) {
                    // A transition the next restart forgets is not a transition.
                    // Audited loudly and backed off; the batch record on disk is
                    // whatever survived, and reconcile is what fixes it.
                    self.audit(
                        group,
                        brand::AUDIT_ACTOR,
                        mqloop::audit_action::STRANDED,
                        json!({ "reason": "merge_queue.json could not be written", "detail": e }),
                    );
                    self.mq_defer(group, now.saturating_add(MQ_DRIVE_BACKOFF_MS));
                }
            }
            rep
        };
        // Outside the lock: an audit write is cheap but a notice is a delivery,
        // and a delivery enqueues.
        for a in &report.audits {
            self.audit(group, brand::AUDIT_ACTOR, a.action, a.detail.clone());
        }
        for n in &report.notices {
            let _ = self.deliver_to_orchestrator(group, n, brand::AUDIT_ACTOR);
        }
        if report.backoff {
            self.mq_defer(group, now.saturating_add(MQ_DRIVE_BACKOFF_MS));
        } else {
            // Not a backoff — just this group's turn in the rotation, so N
            // groups with live queues share the wakes instead of the first one
            // alphabetically taking every tick.
            self.mq_defer(group, now);
        }

        report
    }
}
