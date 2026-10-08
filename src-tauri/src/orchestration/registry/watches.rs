//! Polling `gh` for agents: the `notify_when` watch registry
//! (`register_notification`, `group_watches`), the bounded `gh` capture and
//! `post_issue_comment`, the notify and gh-poll ticks, and the idle-tick
//! intake gate's label/PR-check poll (#332), as an `impl OrchRegistry` block
//! (#3498). The design is `docs/design/orchestration.md`.

use super::*;

impl OrchRegistry {
    // ---------- notification backend (#243): register-and-move-on CI/run watches ----------
    //
    // Mirrors the watchdog's split exactly: `poll_watches` is the impure half
    // (shells out to `gh`, one process per due watch), `notify_tick` is the
    // decision half (pause/expiry/fail-streak/fire policy over an injected
    // `now` + poll results, so no test needs `gh`), and `run_gh_poll_tick`
    // glues them for the live background thread (`start_gh_poller`, the one
    // loop that also carries the intake scan since #406).

    /// Register a new watch for `agent` in `group`. Rejects over-cap (naming
    /// the cap that was hit, mirroring the `spawn_agent` guardrail wording);
    /// `kind`/target parsing and validation happen in `mcp.rs` before this is
    /// called — an unrecognized kind never reaches here; there is nothing to
    /// default it to.
    pub fn register_notification(
        &self,
        group: &GroupId,
        agent: &str,
        condition: notify::Condition,
        note: String,
        expires_minutes: u32,
    ) -> Result<notify::Watch, String> {
        let mut watches = self.watches.lock_safe();
        let per_agent = watches.values().filter(|w| w.agent == agent).count();
        if per_agent >= notify::MAX_WATCHES_PER_AGENT {
            return Err(format!(
                "guardrail: {agent} already has {per_agent} live notifications (max {}). \
                 cancel_notification one first, or let one fire/expire.",
                notify::MAX_WATCHES_PER_AGENT
            ));
        }
        let per_group = watches.values().filter(|w| w.group == group).count();
        if per_group >= notify::MAX_WATCHES_PER_GROUP {
            return Err(format!(
                "guardrail: this group already has {per_group} live notifications (max {}). \
                 cancel_notification one first, or let one fire/expire.",
                notify::MAX_WATCHES_PER_GROUP
            ));
        }
        let now = now_ms();
        let expires_minutes = notify::clamp_expires_minutes(Some(expires_minutes));
        let ttl_ms = expires_minutes as u64 * 60_000;
        let seq = self.notify_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("n-{seq}");
        let watch = notify::Watch {
            id: id.clone(),
            group: group.clone(),
            agent: agent.to_string(),
            condition,
            note,
            seq,
            registered_ms: now,
            deadline_ms: now + ttl_ms,
            nominal_ttl_ms: ttl_ms,
            last_poll_ms: 0,
            fail_streak: 0,
            // #531: baselined by the poller on the first poll that reports a
            // head, not here — registration deliberately shells out to
            // nothing (see `Watch::first_head`).
            first_head: None,
        };
        watches.insert(id, watch.clone());
        drop(watches);
        self.audit(group, agent, "watch-register", json!({
            "id": watch.id, "kind": watch.condition.kind(), "target": watch.condition.label(),
            "expires_minutes": expires_minutes,
        }));
        Ok(watch)
    }

    /// The caller's own live watches (`list_notifications`), oldest-registered
    /// first — `(registered_ms, seq)`, so a real-clock tie (two watches
    /// registered in the same millisecond) breaks deterministically on true
    /// call order instead of the HashMap's arbitrary iteration order (see
    /// `Watch::seq`'s doc).
    pub fn list_notifications(&self, agent: &str) -> Value {
        let watches = self.watches.lock_safe();
        let mut mine: Vec<&notify::Watch> = watches.values().filter(|w| w.agent == agent).collect();
        mine.sort_by_key(|w| (w.registered_ms, w.seq));
        json!(mine.into_iter().map(notify::watch_json).collect::<Vec<_>>())
    }

    /// Every live watch belonging to any of `group`'s agents — id, agent, kind,
    /// target, note, expiry — for the group view's per-agent "⏳ waiting on …"
    /// indicator (#248). Reads the same `watches` map `list_notifications` and
    /// `notify_tick` do; there is no second store. Unlike `list_notifications`
    /// (self-scoped by design — it's MCP-callable, so an agent may only ever
    /// see its own), this is a Tauri command reached only from the trusted
    /// webview (CLAUDE.md constraint 5), so reading across the whole group's
    /// roster is fine. Oldest-registered first — `(registered_ms, seq)`,
    /// matching `list_notifications`'s tie-break.
    pub fn group_watches(&self, group: &GroupId) -> Value {
        let watches = self.watches.lock_safe();
        let mut mine: Vec<&notify::Watch> = watches.values().filter(|w| w.group == group).collect();
        mine.sort_by_key(|w| (w.registered_ms, w.seq));
        json!(mine
            .into_iter()
            .map(|w| json!({
                "id": w.id,
                "agent": w.agent,
                "kind": w.condition.kind(),
                "target": w.condition.label(),
                // `note` is agent-supplied and deliberately unsanitized at
                // registration (correct for `list_notifications`, which hands an
                // agent its own text back) — but THIS command crosses a new
                // boundary, into the trusted webview, for every agent's note, not
                // just the reader's own. So strip control chars and neutralize the
                // `[orrerix]` marker here with the same `sanitize_gh_text` the
                // fired/expired/failed notices already use — that closes the
                // notice/log-forging class this string could otherwise carry.
                // It does NOT html-escape: an HTML metacharacter payload (e.g. an
                // `<img onerror=...>`) crosses this call untouched (escaping in a
                // JSON payload would be the wrong layer anyway — it corrupts the
                // data for every non-HTML consumer). The thing standing between
                // an agent's note and script execution is, and must remain, that
                // the renderer only ever assigns it to a `.title`/`textContent`
                // DOM PROPERTY, never `innerHTML` (true today — zero `innerHTML`
                // in this diff, rev-orch PR #252 round 2). Do not relax that
                // renderer rule on the theory that "the backend sanitizes it" —
                // it sanitizes a different, narrower thing.
                "note": notify::sanitize_gh_text(&w.note, notify::NOTICE_FIELD_CAP),
                "expires_ms": w.deadline_ms,
            }))
            .collect::<Vec<_>>())
    }

    /// Cancel one of the caller's own watches. Owner-scoped: an id that
    /// exists but belongs to someone else reads identically to an id that
    /// doesn't exist at all — the `require_in_group` anti-leak wording, so a
    /// cross-owner probe can't distinguish "not yours" from "never existed".
    pub fn cancel_notification(&self, agent: &str, id: &str) -> Result<(), String> {
        let mut watches = self.watches.lock_safe();
        if !watches.get(id).is_some_and(|w| w.agent == agent) {
            return Err(format!("unknown notification: {id}"));
        }
        let w = watches.remove(id).expect("checked present above");
        drop(watches);
        self.audit(&w.group, agent, "watch-cancel", json!({ "id": id }));
        Ok(())
    }

    /// Drop every watch belonging to `agent_id` (called from `mark_dead`: the
    /// pane a fired notice would land in is gone). Audits once, only if
    /// something was actually removed, so a routine mark_dead with no
    /// outstanding watches doesn't add audit noise.
    pub(in crate::orchestration) fn cleanup_agent_watches(&self, agent_id: &str, group: &GroupId) {
        let removed: Vec<String> = {
            let mut watches = self.watches.lock_safe();
            let ids: Vec<String> =
                watches.iter().filter(|(_, w)| w.agent == agent_id).map(|(id, _)| id.clone()).collect();
            for id in &ids {
                watches.remove(id);
            }
            ids
        };
        if !removed.is_empty() {
            self.audit(group, brand::AUDIT_ACTOR, "watch-cleanup", json!({ "agent": agent_id, "ids": removed }));
        }
    }

    /// Shell out to `gh` in `repo` and capture stdout on success / stderr on
    /// failure. Resolves the binary through `winpath::resolve_program` (a bare
    /// `Command::new("gh")` won't resolve a Windows `gh.cmd` shim-free — see
    /// `write_shim`'s note) and pins `CREATE_NO_WINDOW` so no console flashes
    /// on a Windows host. `repo` comes from the caller's group (resolved
    /// server-side), never from an argument, so the group-id path seam (#904)
    /// is never engaged here at all.
    ///
    /// **This is the one place the backend spawns `gh`** (#791). It began as
    /// the notify poller's helper, with `pr_head`/`pr_body` keeping a second
    /// copy of the same subprocess shape — and that copy was the copy without
    /// the bound: `list_verdicts` walked every verdict PR through it and a
    /// slow network wedged the calling agent's MCP turn outright, with no
    /// error and no way to tell what it was waiting on. So the fold the
    /// original note here asked for once #222 merged is done, and the reason
    /// it matters is stronger than tidiness: a second spawn site is a second
    /// site that can be written without `capture_with_timeout`.
    ///
    /// Two consequences of routing the MCP path through here, both deliberate:
    /// every `gh` read in the process now shares the abandoned-reader ceiling
    /// (`GH_CAPTURE_MAX_LEAKED_READERS`), so a wedged poller can refuse an MCP
    /// read with a named backlog error — which is the trade `capture_with_timeout`
    /// already documents, and a named refusal beats a hang; and the argv is no
    /// longer always backend-built from a `u64`, since `pr_head`/`pr_body` build
    /// theirs from a parsed PR number. That number is `pr_number`-parsed into a
    /// `u64` before it gets here, so it is still never caller text on a command
    /// line.
    pub(in crate::orchestration) fn gh_capture(&self, repo: &str, args: &[&str]) -> Result<String, String> {
        if !Path::new(repo).is_dir() {
            return Err(format!("no such directory: {repo}"));
        }
        let (program, timeout) = match self.gh_exec_override.lock_safe().clone() {
            Some(exec) => exec,
            None => {
                let Some(program) = crate::winpath::resolve_program(
                    "gh",
                    &crate::winpath::launch_path(),
                    &crate::winpath::launch_pathext(),
                ) else {
                    return Err("gh-not-found".to_string());
                };
                (program, GH_CAPTURE_TIMEOUT)
            }
        };
        let mut cmd = std::process::Command::new(program);
        cmd.current_dir(repo)
            .args(args)
            .env("NO_COLOR", "1")
            .env("GH_PAGER", "")
            .env("GH_PROMPT_DISABLED", "1");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        Self::capture_with_timeout(cmd, timeout)
    }

    /// Test seam: see `gh_exec_override`. `None` restores the real `gh` on the
    /// production deadline.
    #[doc(hidden)] // pub for integration tests
    pub fn set_gh_exec_override(&self, exec: Option<(PathBuf, Duration)>) {
        *self.gh_exec_override.lock_safe() = exec;
    }

    /// Post `body` as a comment on issue `issue` in the calling group's repo and
    /// return the new comment's URL — the backend half of the `post_issue_comment`
    /// MCP tool (#2815).
    ///
    /// **Why this exists at all, when `gh` is on a planner's allowlist.** It is
    /// not a convenience wrapper: a planner's deliverable is a whole document,
    /// and there is no route for one through the CLI shell. Claude Code's
    /// permission engine refuses to allow-match a Bash command longer than
    /// 10,000 characters ("Commands longer than 10,000 characters always prompt
    /// because they exceed what the analysis parses" — the permissions
    /// reference, verified 2026-09-06), and treats newlines as subcommand
    /// separators, so a multi-line `--body` matches no rule either. Under
    /// `dontAsk` an unmatched call is denied outright. Measured, not inferred:
    /// plan-2332 had `Bash(gh *)` allowed and was still denied
    /// `gh issue comment <n> --body '<21610 chars>'`, the same via
    /// `--body-file -` with a heredoc, and every file-write fallback. A tool
    /// argument is a JSON payload, not a command line, so it has neither limit.
    ///
    /// **Comments only, by construction.** The verb and subcommand are literals
    /// here; only the number and the body come from the caller, and the body is
    /// the VALUE of `--body` (see [`crate::gh::comment_argv`]). Nothing a caller
    /// passes can reach a label, a close, a merge, a review, or a PR-creating
    /// argv — that is a property of the code, not of an argument check.
    ///
    /// **Residual, stated rather than implied:** GitHub numbers issues and pull
    /// requests in ONE namespace, and `gh issue comment` accepts a PR number,
    /// posting to that PR's conversation. So this tool can address a PR. What it
    /// cannot do to one is anything but comment, which is the capability bound
    /// that matters; refusing the number would cost a second round trip per post
    /// to buy nothing. `docs/design/orchestration.md` carries the same statement.
    ///
    /// **The body travels as a FILE, not as an argument** — see
    /// [`crate::gh::comment_file_argv`] for the two limits that forces: Windows'
    /// 32,767-character command-line cap, which would ceiling a plan at about the
    /// size plans already are, and Rust's refusal to pass an unescapable argument
    /// to a `.cmd` shim, which fails a multi-line body before `gh` runs at all.
    /// The file is written into the group's own state directory and removed once
    /// `gh` has read it, whether or not the post succeeded.
    ///
    /// `actor` is a [`PathSegment`] (#925) for the same reason
    /// [`Self::ledger_path`]'s is: it becomes a file name, so it must be proven a
    /// single component before it gets there.
    ///
    /// **The staging files live in a SUBDIRECTORY, not in the group dir**
    /// (#3061 residual 1), and that is a containment fix rather than tidiness.
    /// The group dir is also where each roster block's instruction file lives,
    /// as `<block id>.md` (`workflow::Block::instructions_file`). A block id is
    /// operator-authored and only `sanitize_id`-checked, so a workflow
    /// declaring a block called `a-7-comment-body-0` puts `a-7-comment-body-0.md`
    /// in the very namespace this method writes `{actor}-comment-body-{seq}.md`
    /// into — and the first post by agent `a-7` after a restart (the sequence
    /// counter is process-wide and starts at 0) truncates that block's
    /// instructions and then deletes the file. Nothing fails; the block simply
    /// spawns with no instructions. Two namespaces that could collide are now
    /// one directory apart, which is a property of the path rather than of an
    /// id check.
    ///
    /// **The staging path carries a per-call sequence number, not just the agent
    /// id** (review round 1). An agent is free to issue two tool calls at once —
    /// Claude Code batches independent calls in one message — so two posts by ONE
    /// pane would otherwise race on a single `<agent>-comment-body.md`: the second
    /// write truncates the file the first is still handing to `gh`, and the first
    /// post silently publishes the second's text or a torn prefix of it. Nothing
    /// fails, which is what makes it worth a counter rather than a comment. The
    /// counter is process-wide and monotonic, so it separates concurrent calls.
    ///
    /// **The orphan the counter creates is now swept** (#3061 residual 3).
    /// Before the counter, a staging file orphaned by a kill between the write
    /// and the remove was reclaimed by the next post from that agent, which
    /// reused the one name; with a per-call name nothing ever reuses it, so an
    /// orphan was permanent debris. The subdirectory above is what makes a sweep
    /// safe to write at all — it enumerates a directory this method OWNS, so it
    /// cannot reach an instruction file, a state file or anything else.
    ///
    /// **The sweep deletes only what cannot still be in use**, and the bound is
    /// derived rather than picked: a staging file is live exactly as long as the
    /// `gh` child reading it can run, and that is bounded by
    /// [`GH_CAPTURE_TIMEOUT`]. [`STAGING_ORPHAN_AGE`] is that timeout with a wide
    /// margin, so a file young enough to belong to an in-flight post is never a
    /// candidate — a sweep that raced a concurrent post would be a worse bug
    /// than the litter it cleans. It runs before the write, is best-effort
    /// throughout (a sweep that cannot read the directory must not fail a post),
    /// and an unreadable mtime is treated as YOUNG, because unknown is not a
    /// licence to delete.
    ///
    /// **Every post that reaches `gh` is audited, whichever way it goes.** The row
    /// is written after `gh` has been run and carries the outcome — the URL on
    /// success, the error on failure — so a post that failed is visible to the
    /// human rather than absent, which reads identically to never having been
    /// attempted.
    ///
    /// **What writes no row, enumerated rather than gestured at** (review round
    /// 2's PREMORTEM — not that round's finding 3, which was about something
    /// else; the cite was wrong and a wrong cite sends the next reader to the
    /// wrong argument). Anything that returns BEFORE `gh` runs: an empty body, an
    /// unusable agent id, an unknown group — and, the case the first wording
    /// missed, a STAGING failure, where `create_dir_all` or `fs::write` cannot
    /// produce the body file. The first three are argument validation and are not
    /// posts; the last one is a genuine attempt that leaves no trace, and it is a
    /// carve-out rather than a gap that got fixed for one reason: the audit log
    /// lives in the very directory the staging write just failed to write into, so
    /// a row recorded there is not reliably obtainable in exactly the case that
    /// would need it. Naming the case is honest; pretending a row would appear
    /// would not be.
    ///
    /// `repo` is resolved from the caller's own group, never from an argument —
    /// the same server-side resolution [`Self::gh_capture`] documents — so the
    /// group-id path seam (#904) is not engaged here.
    pub fn post_issue_comment(
        &self,
        group: &GroupId,
        actor: &PathSegment,
        issue: u64,
        body: &str,
    ) -> Result<String, String> {
        let repo = self
            .group(group.as_str())
            .map(|g| g.repo)
            .ok_or_else(|| "unknown group".to_string())?;
        crate::gh::reject_empty_comment(body)?;
        let staging = self.group_dir(group).join(COMMENT_BODY_DIR);
        fs::create_dir_all(&staging)
            .map_err(|e| format!("cannot prepare the comment body: {e}"))?;
        sweep_staged_comment_bodies(&staging, std::time::SystemTime::now());
        let seq = COMMENT_BODY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let body_path = staging.join(format!("{actor}-comment-body-{seq}.md"));
        fs::write(&body_path, body)
            .map_err(|e| format!("cannot write the comment body: {e}"))?;
        let args =
            crate::gh::comment_file_argv("issue", issue, &body_path.to_string_lossy());
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let captured = self.gh_capture(&repo, &argv);
        // Best-effort, and deliberately not `?`: the post has already happened
        // or already failed, and a leftover scratch file is not a reason to
        // report either outcome differently.
        let _ = fs::remove_file(&body_path);
        let out = match captured {
            Ok(out) => out,
            Err(e) => {
                // Audited BEFORE the early return: a failed post the human cannot
                // see is indistinguishable from one that was never attempted, and
                // "every post leaves a row" is a claim this branch has to honour
                // too (review round 1).
                self.audit(
                    group,
                    actor.as_str(),
                    "issue-comment",
                    json!({ "issue": issue, "bytes": body.len(), "error": e }),
                );
                return Err(e);
            }
        };
        // `gh issue comment` prints the new comment's URL, and prints it LAST —
        // and [`crate::gh::comment_url`] additionally checks that the line it
        // takes IS a URL (#3061 residual 4). Before that check the empty string
        // was handed back as an address whenever `gh` printed something else.
        //
        // **A line that is not a URL does not make this an `Err`**, and the
        // distinction is the whole point: the comment HAS been posted, and
        // telling the caller it failed would be a false claim about the world
        // that no retry can undo — an agent that re-posted on it would double
        // its plan onto the issue. What the caller gets instead is a sentence
        // that cannot be mistaken for a URL and says exactly what happened, and
        // the audit row carries the raw capture so a human can find the comment.
        let raw = out.trim().to_string();
        let url = match crate::gh::comment_url(&out) {
            Some(u) => u.to_string(),
            None => POSTED_URL_UNREADABLE.to_string(),
        };
        self.audit(
            group,
            actor.as_str(),
            "issue-comment",
            json!({ "issue": issue, "bytes": body.len(), "url": url,
                    "url_unreadable": url == POSTED_URL_UNREADABLE,
                    "raw": (url == POSTED_URL_UNREADABLE).then_some(raw) }),
        );
        Ok(url)
    }

    /// Run `cmd` to completion and capture it the way `Command::output()`
    /// would — stdout on success, trimmed stderr on a non-zero exit — except
    /// that the wait is **bounded**: at `timeout` the child is killed and the
    /// call returns `Err` (#656).
    ///
    /// `output()` waits forever, and since #406 unified the two `gh` pollers
    /// into one loop there is exactly one thread making `gh` calls: a single
    /// child parked on a stalled connection stops every `notify_when` notice
    /// in the process, not just the half it belongs to. Every caller already
    /// handles `Err` — `poll_watches` counts it toward
    /// `notify::NOTIFY_FAIL_STREAK_LIMIT`, `poll_intake` skips that half of
    /// its diff and still stamps — so a timeout needs no new failure channel,
    /// only a bound.
    ///
    /// **Why the reader threads.** The obvious shape (poll `try_wait`, then
    /// `wait_with_output` once it reports exit) deadlocks against the very
    /// calls this poller makes: `gh issue list --json` on a busy repo easily
    /// exceeds a pipe buffer, and a child blocked writing into a full pipe
    /// never exits, so a healthy-but-chatty `gh` would time out every tick.
    /// Draining both pipes concurrently is what keeps the child running.
    ///
    /// **Why the timeout path does not join them.** After the kill we return
    /// immediately and leave the two readers to end when their pipe ends. A
    /// join here would reintroduce exactly the unbounded wait being removed:
    /// a grandchild that inherited the pipe handle keeps it open past its
    /// parent's death, so the read can outlive the process we killed.
    ///
    /// **What bounds that leak (rev-lead finding 1).** Abandoning readers
    /// bounds one call, not the process: the very case that justifies not
    /// joining is the case where they never finish, and a persistently
    /// stalling condition is re-polled every tick. So an abandoned reader is
    /// parked in `GH_CAPTURE_LEAKED_READERS` instead of forgotten — on **every**
    /// arm that gives up on a live child, the timeout and the one where the
    /// bounded wait itself errors alike (#699), since a reader that is dropped
    /// rather than parked is invisible to the sweep and the ceiling then
    /// understates the very backlog it admits on. Every
    /// capture first sweeps the ones that have since ended, and a capture is
    /// refused outright once `GH_CAPTURE_MAX_LEAKED_READERS` are still
    /// blocked. Note what that trade is: past the ceiling, `gh` polling in
    /// this process stops until the backlog drains, and if a grandchild holds
    /// a pipe forever it never drains. That is deliberate — a named, returned
    /// error that both callers already surface (a watch cancelled with a
    /// reason someone can read) beats thread growth nothing reports. In the
    /// ordinary stall the child IS `gh`, killing it closes both pipes, and the
    /// readers end at once: the backlog stays empty and the ceiling is never
    /// approached.
    ///
    /// Kept `pub` (and taking a caller-built `Command`) so the timeout path is
    /// testable against a deliberately-slow subprocess without shelling out to
    /// `gh`, which no test in this repo is allowed to do.
    pub fn capture_with_timeout(cmd: std::process::Command, timeout: Duration) -> Result<String, String> {
        let (status, stdout, stderr) = capture_raw_with_timeout(cmd, timeout)?;
        if status.success() {
            Ok(stdout)
        } else {
            let err = stderr.trim().to_string();
            if err.is_empty() { Err(format!("gh exited with {status}")) } else { Err(err) }
        }
    }

    /// Impure half of one notify tick: pick due watches via the pure
    /// `notify::due_watches` selection policy (the per-tick cap, the
    /// per-watch floor, round-robin ordering, and the paused-skip are all
    /// tested there with no `gh`), shell out to `gh` for each, and classify
    /// with the pure predicates.
    ///
    /// `now` is injected (#406): the unified `gh` poller samples the clock
    /// ONCE per wake and hands the same instant to every half of the tick, so
    /// the selection floor here and the decision policy in `gh_poll_tick`
    /// can never disagree about when "now" was.
    fn poll_watches(&self, now: u64) -> HashMap<String, notify::Poll> {
        let paused = self.paused.lock_safe().clone();
        let due: Vec<notify::Watch> = {
            let watches = self.watches.lock_safe();
            notify::due_watches(now, &watches, &paused)
                .into_iter()
                .filter_map(|id| watches.get(&id).cloned())
                .collect()
        };

        let mut results = HashMap::new();
        for w in &due {
            let Some(repo) = self.group(&w.group).map(|g| g.repo) else { continue };
            let poll = match &w.condition {
                notify::Condition::PrChecks { pr } => {
                    // #337: check mergeability BEFORE checks. A conflicted PR
                    // never gets a check-suite at all (no clean merge ref for
                    // GitHub to run `pull_request`-triggered workflows
                    // against), so `gh pr checks` would just sit at "no
                    // checks reported" (Pending) every tick until expiry —
                    // this pre-check turns that silent dead end into an
                    // immediate, distinct notice instead.
                    //
                    // #531: `headRefOid` rides along on that same call — one
                    // process, two facts — so a fired notice can state the
                    // head its verdict actually belongs to instead of leaving
                    // the frozen registration-time note as the only SHA in
                    // sight. Sampled here, immediately before the checks read,
                    // which is why the notice calls it "head at this poll"
                    // rather than claiming it as the verdict's own head.
                    let mergeability =
                        self.gh_capture(&repo, &["pr", "view", &pr.to_string(), "--json", "mergeStateStatus,headRefOid"]);
                    let mergeability_ref: Result<&str, &str> = match &mergeability {
                        Ok(s) => Ok(s.as_str()),
                        Err(e) => Err(e.as_str()),
                    };
                    let head = notify::pr_head_from(mergeability_ref);
                    let result = if notify::pr_mergeability_result(mergeability_ref) == notify::PollResult::Conflicting {
                        notify::PollResult::Conflicting
                    } else {
                        let raw = self.gh_capture(&repo, &["pr", "checks", &pr.to_string(), "--json", "state,name,link"]);
                        let raw_ref: Result<&str, &str> = match &raw {
                            Ok(s) => Ok(s.as_str()),
                            Err(e) => Err(e.as_str()),
                        };
                        notify::condition_poll_result(&w.condition, raw_ref)
                    };
                    notify::Poll::new(result, head)
                }
                notify::Condition::WorkflowRun { run } => {
                    let raw = self.gh_capture(&repo, &["run", "view", &run.to_string(), "--json", "status,conclusion"]);
                    let raw_ref: Result<&str, &str> = match &raw {
                        Ok(s) => Ok(s.as_str()),
                        Err(e) => Err(e.as_str()),
                    };
                    // No head: a run id is already pinned to one commit, so
                    // there is nothing here that can drift out from under the
                    // note.
                    notify::Poll::from(notify::condition_poll_result(&w.condition, raw_ref))
                }
            };
            results.insert(w.id.clone(), poll);
        }

        // Stamp last_poll_ms for everything that actually got a `gh` result
        // this tick (not merely everything `due`: a watch whose group
        // vanished from under it — unreachable today, since groups are never
        // removed from the registry, but cheap to keep correct — was
        // `continue`d above and must not be credited with a poll it never
        // got), so the round-robin ordering advances even for watches that
        // stayed Pending.
        if !results.is_empty() {
            let mut watches = self.watches.lock_safe();
            for id in results.keys() {
                if let Some(live) = watches.get_mut(id) {
                    live.last_poll_ms = now;
                }
            }
        }
        results
    }

    /// Pure-shaped decision half (the `watchdog_tick` shape): given this
    /// tick's poll `results` (only watches actually polled this tick appear —
    /// a watch not yet due, or skipped because its group is paused, is simply
    /// absent, and this tick leaves it untouched), apply pause/expiry/
    /// fail-streak/fire policy to every registered watch, deliver the
    /// resulting notices, and return the ids that fired (met, expired, or
    /// failure-cancelled). No `gh` call anywhere in this function — every
    /// test drives it with a synthetic `results` map.
    ///
    /// **Freezing the TTL clock across a pause.** `deadline_ms` is an
    /// absolute wall-clock timestamp, so merely *skipping the expiry check*
    /// while paused is not enough: real time keeps passing underneath it, and
    /// the first tick after a long pause would find every outstanding watch
    /// already past its deadline — evaporating exactly the watches the
    /// freeze exists to protect. So this tick first reconciles
    /// `paused_watch_since`, a per-group "we last saw this paused starting at
    /// tick-time T" record built entirely from the `now` values this function
    /// is called with (never real wall-clock — `pause_group`/`resume_group`
    /// use `now_ms()` directly and aren't reachable from a test's simulated
    /// clock, so the bookkeeping has to live here instead, driven by the
    /// ticks that actually observe the pause):
    /// - Every group in the CURRENT `paused` set (not "every group with a
    ///   live watch" — that scan let a group that emptied out mid-pause drop
    ///   off the radar entirely and strand its entry forever, rev-orch, PR
    ///   #247, "B1") that isn't already recorded gets
    ///   `paused_watch_since[group] = now` — the earliest a paused group can
    ///   be caught is the very next tick, and `start_gh_poller` ticks
    ///   every `NOTIFY_POLL_INTERVAL` regardless of any group's pause state,
    ///   so this lags true pause-start by at most one poll interval.
    /// - A group recorded as paused that is no longer in `paused` (it
    ///   resumed) is reconciled: the elapsed span since it was recorded is
    ///   computed once and the record is cleared, ready for a future
    ///   pause/resume cycle.
    /// - That span is a per-GROUP number, but the credit applied to each
    ///   watch is clamped to `now - w.registered_ms` — the span THIS watch
    ///   actually lived through — because a watch registered mid-pause
    ///   (panes keep running while paused; only prompt delivery is
    ///   suppressed, so `notify_when` still works) never experienced the
    ///   part of the span that predates it (rev-orch, PR #247, "B2").
    ///   `nominal_ttl_ms` (fixed at registration) is what the expiry notice
    ///   reports, precisely so this mutation of `deadline_ms` never corrupts
    ///   the "expired after N min" figure shown to the agent.
    pub fn notify_tick(&self, now: u64, results: &HashMap<String, notify::Poll>) -> Vec<String> {
        enum Fate {
            /// The summary the notice reports, plus the head SHA observed on
            /// the poll that produced it (#531) — carried through to the
            /// notice so the verdict is labelled with the head it belongs to
            /// and not merely with whatever the frozen note happened to name.
            Fire(String, Option<String>),
            Expire,
            FailCancel(String),
            /// #337: the PR went CONFLICTING — terminal like `Fire`, but
            /// carries no summary since there is no check result, only the
            /// PR number the distinct notice names.
            Conflicting(u64),
        }
        let paused = self.paused.lock_safe().clone();

        // Reconcile against the PAUSED SET ITSELF, not "groups that currently
        // hold a watch": scanning only live watches let a group that emptied
        // out while paused (its one worker idle-killed, cancelled, or
        // crashed — all routine) drop out of the scan entirely, so its
        // `paused_watch_since` entry was never reconciled and sat stranded
        // until some later, unrelated watch appeared in that group — which
        // then got charged the ENTIRE stale span, even though it never lived
        // through that pause (rev-orch, PR #247, "B1"). Scanning `paused`
        // instead means every group this tick believes is paused gets an
        // entry, and every group that WAS recorded paused but no longer is
        // gets reconciled, regardless of whether it currently owns any
        // watches at all.
        let extend_by: HashMap<GroupId, u64> = {
            let mut since = self.paused_watch_since.lock_safe();
            for g in paused.iter() {
                since.entry(g.clone()).or_insert(now);
            }
            let resumed: Vec<GroupId> = since.keys().filter(|g| !paused.contains(*g)).cloned().collect();
            let mut extend = HashMap::new();
            for g in resumed {
                if let Some(started) = since.remove(&g) {
                    extend.insert(g, now.saturating_sub(started));
                }
            }
            extend
        };

        // Decide fates under the watches lock; deliver after releasing it —
        // delivery can block on a busy pane's per-pane delivery lock.
        let mut acted: Vec<(notify::Watch, Fate)> = Vec::new();
        {
            let mut watches = self.watches.lock_safe();
            let mut to_remove: Vec<String> = Vec::new();
            for (id, w) in watches.iter_mut() {
                if let Some(extra) = extend_by.get(&w.group) {
                    // Clamp to the span THIS watch actually lived through: a
                    // watch registered mid-pause (panes keep running while
                    // paused — only prompt delivery is suppressed, so
                    // `notify_when` still works) never experienced the part
                    // of the span that elapsed before it existed, and must
                    // not be charged for it (rev-orch, PR #247, "B2").
                    let earned = (*extra).min(now.saturating_sub(w.registered_ms));
                    w.deadline_ms = w.deadline_ms.saturating_add(earned);
                }
                // Paused: frozen solid — no poll (already true in
                // `poll_watches`), no fire, and (via the extension above,
                // applied on the resuming tick) no expiry either — a long
                // pause doesn't silently evaporate every watch.
                if paused.contains(&w.group) {
                    continue;
                }
                let poll = results.get(id);
                // #531: fold this poll's observed head in BEFORE any fate is
                // decided, so a watch that fires on this very tick already
                // carries its baseline in the clone below. First head wins and
                // is never overwritten — it is the "what this watch started
                // out watching" reference the MOVED marker measures against;
                // overwriting it each poll would make every notice read as
                // unmoved.
                if let Some(head) = poll.and_then(|p| p.head.as_deref()) {
                    if w.first_head.is_none() {
                        w.first_head = Some(head.to_string());
                    }
                }
                match poll.map(|p| &p.result) {
                    Some(notify::PollResult::Met { summary }) => {
                        acted.push((w.clone(), Fate::Fire(summary.clone(), poll.and_then(|p| p.head.clone()))));
                        to_remove.push(id.clone());
                        continue;
                    }
                    Some(notify::PollResult::Conflicting) => {
                        // Terminal — resolve now, never wait toward expiry
                        // for checks that structurally will never appear.
                        let notify::Condition::PrChecks { pr } = &w.condition else {
                            continue; // unreachable: only produced for PrChecks watches
                        };
                        acted.push((w.clone(), Fate::Conflicting(*pr)));
                        to_remove.push(id.clone());
                        continue;
                    }
                    Some(notify::PollResult::Failed { why }) => {
                        w.fail_streak += 1;
                        if w.fail_streak >= notify::NOTIFY_FAIL_STREAK_LIMIT {
                            acted.push((w.clone(), Fate::FailCancel(why.clone())));
                            to_remove.push(id.clone());
                            continue;
                        }
                    }
                    Some(notify::PollResult::Pending) => {
                        w.fail_streak = 0; // any successful poll resets the streak
                    }
                    None => {} // not due this tick — leave fail_streak alone
                }
                if notify::watch_expired(w.deadline_ms, now) {
                    acted.push((w.clone(), Fate::Expire));
                    to_remove.push(id.clone());
                }
            }
            for id in &to_remove {
                watches.remove(id);
            }
        }

        let mut fired = Vec::new();
        for (w, fate) in acted {
            let (action, text) = match &fate {
                Fate::Fire(summary, head) => (
                    "watch-fired",
                    notify::watch_fired_notice(
                        &w.id,
                        &w.condition,
                        summary,
                        head.as_deref(),
                        w.first_head.as_deref(),
                        &w.note,
                    ),
                ),
                Fate::Conflicting(pr) => (
                    "watch-conflicting",
                    notify::watch_conflicting_notice(&w.id, *pr),
                ),
                Fate::Expire => {
                    let minutes = w.nominal_ttl_ms / 60_000;
                    (
                        "watch-expired",
                        notify::watch_expired_notice(&w.id, &w.condition, minutes as u32),
                    )
                }
                Fate::FailCancel(why) => (
                    "watch-failed",
                    notify::watch_failed_notice(&w.id, &w.condition, why),
                ),
            };
            self.audit(&w.group, brand::AUDIT_ACTOR, action, json!({
                "id": w.id, "kind": w.condition.kind(), "agent": w.agent, "text": text,
            }));
            let _ = self.deliver_prompt(&w.agent, &text, brand::AUDIT_ACTOR, Delivery::MidSession);
            fired.push(w.id.clone());
        }
        fired
    }

    /// One full cycle of the UNIFIED `gh` poller (#406): poll due watches,
    /// then apply both halves' tick policy against one sampled instant.
    /// Called on a timer by `start_gh_poller` — the single background loop
    /// that makes `gh` calls in this process.
    ///
    /// The split is the one `run_notify_tick` already had (and this
    /// replaces): the `gh` shelling lives here, the policy lives in
    /// `gh_poll_tick`, so tests drive the decision half with a synthetic
    /// result map and no subprocess.
    pub fn run_gh_poll_tick(&self) -> GhPollTick {
        // `GhPollTick::default()` is the same "nothing happened this tick"
        // value a poll with no watches produces, so a skip is indistinguishable
        // from a quiet tick to every caller — which is what makes skipping safe
        // rather than a second code path.
        let Some(_tick) = self.tick_gate("run_gh_poll_tick") else {
            return GhPollTick::default();
        };
        let now = now_ms();
        let results = self.poll_watches(now);
        self.gh_poll_tick(now, &results)
    }

    /// Decision half of one unified poll tick (#406). The notify half runs on
    /// EVERY wake (the watch-firing latency users see is the wake cadence
    /// itself); the intake half runs only on the wakes where
    /// `intake_scan_due` says its own coarser scan cadence has elapsed, so
    /// folding the two loops together neither speeds the intake scan up nor
    /// slows watch delivery down.
    ///
    /// The scan stamp is taken when the scan is DECIDED, not when it
    /// finishes — a slow `gh` round-trip must not shorten the next
    /// interval — and it is the scan cadence only: the per-group `gh` floor
    /// is still `intake::due_intake_polls`' business, untouched here.
    pub fn gh_poll_tick(&self, now: u64, results: &HashMap<String, notify::Poll>) -> GhPollTick {
        let fired = self.notify_tick(now, results);
        // Named lock resources (#858): expired holds, expired waits, and
        // holders/waiters whose panes are gone. Folded into this wake rather
        // than given a thread of its own — it is the same 30s cadence, it
        // needs the same paused-group freeze, and a lock sweep does no I/O of
        // its own beyond the audit lines it produces.
        self.locks_tick(now);
        let intake_scanned = {
            let mut last = self.intake_last_scan_ms.lock_safe();
            let due = intake_scan_due(now, *last);
            if due {
                *last = Some(now);
            }
            due
        };
        if intake_scanned {
            self.poll_intake(now);
        }
        // #698: the merge queue's driver, on every wake and for at most ONE
        // group — see `mq_driver_tick` for why the bound is one group rather
        // than a budget. On every wake rather than a coarser cadence of its own
        // because the steady state is a single `gh pr checks` on the batch's
        // draft PR, which is exactly a watch poll; the expensive paths are
        // transitions, and a transition happens once per batch.
        let mq_serviced = self.mq_driver_tick(now);
        // #1778 §2.4: the review-loop driver, a FIFTH step, on the same wake and
        // under the same one-group bound. On this loop rather than a thread of
        // its own for #406's reason — observing a driven PR's checks IS a `gh`
        // poll, on the same cadence, and a second `gh`-calling thread re-opens
        // the coupling that loop closed.
        let rd_serviced = self.rd_driver_tick(now);
        // #3040 §2.4: the PLAN driver, a SIXTH step, and deliberately AFTER the
        // review driver rather than beside it. Both spend `gh` round trips on
        // this one loop, and running the plan driver second means it can only
        // ever take what the review driver left — which is how "the plan driver
        // holds, never starves the review driver" is structural instead of a
        // budget nobody can check. Its serviced group is not reported: nothing
        // routes on it, and `GhPollTick` is a shape the frontend reads.
        self.pd_driver_tick(now);
        // #3679: the QUICK drive, a SEVENTH step. On this loop because "one
        // tick loop, one order" is the rule; it makes no `gh` call at all, so
        // it takes nothing the two drivers above could have used, and it
        // looks only at groups whose run is in a working state — a finished
        // or parked run costs this wake nothing. Not reported, for the plan
        // driver's reason: nothing routes on it.
        self.qd_driver_tick(now);
        GhPollTick { fired, intake_scanned, mq_serviced, rd_serviced }
    }

    // ---------- idle-tick intake gate (#332): host-side, zero-token label/PR-check poll ----------

    /// This scan's candidate groups for the intake poller: every AUTONOMOUS
    /// group (a non-autonomous group never idle-ticks, so polling for it
    /// would spend a `gh` round-trip nobody reads) whose effective
    /// `intake_poll_minutes` (#429: smart-defaulted ON while autonomous unless
    /// explicitly opted out — see `intake::effective_intake_poll_minutes`) is
    /// nonzero, paired with its repo path for the `gh` call.
    fn intake_poll_config(&self) -> (HashMap<GroupId, u32>, HashMap<GroupId, String>) {
        let autonomous = self.autonomous_groups.lock_safe().clone();
        let mut minutes = HashMap::new();
        let mut repos = HashMap::new();
        for (id, g) in self.groups.lock_safe().iter() {
            if !autonomous.contains(id) {
                continue;
            }
            let effective = intake::effective_intake_poll_minutes(g.guardrails.intake_poll_minutes, true);
            if effective > 0 {
                minutes.insert(id.clone(), effective);
                repos.insert(id.clone(), g.repo.clone());
            }
        }
        (minutes, repos)
    }

    /// One intake-poll scan (#332): pick due groups via the pure
    /// `intake::due_intake_polls` selection policy (the per-group interval
    /// floor and — #656 — the per-scan cap and oldest-polled-first ordering,
    /// mirroring `notify::due_watches`), shell out to `gh` twice per
    /// due group (`gh issue list`, `gh pr list` — the only two calls this
    /// adds, regardless of how many open PRs/issues exist, since both are
    /// single list calls, not one per item), and fold any new label/PR-check
    /// signal into that group's pending wake summary for `idle_tick_tick` to
    /// pick up. Called by `gh_poll_tick` on the wakes where the scan cadence
    /// is due (#406) — with the same `now` that tick's notify half used, so
    /// both halves of one wake stamp the same instant.
    ///
    /// For a group in **full autonomy** (#778) the same `gh issue list`
    /// response answers a second question — which open issues are eligible to
    /// start and unstarted (`intake::eligible_deltas`) — so the self-select
    /// signal costs zero extra `gh` calls and zero tokens. It rides
    /// `has_intake_signal` like every other finding here: the idle-tick gate
    /// gains no new parameter, and the one-notice latch, hourly cap and
    /// bounded fallback all apply to it unchanged.
    ///
    /// A `gh` failure for one call (auth, `gh` missing, rate-limited) simply
    /// skips that half of the diff for this scan — it still stamps the poll
    /// attempt (so a persistently-failing `gh` doesn't get retried every
    /// scan) and never panics or blocks the tick; the bounded fallback in
    /// `idle_tick_tick` still wakes the orchestrator regardless (#332
    /// acceptance criterion 6: "poll failure ≠ tick death — degrade, don't
    /// deny").
    pub fn poll_intake(&self, now: u64) {
        let (minutes, repos) = self.intake_poll_config();
        if minutes.is_empty() {
            return;
        }
        let last_poll = self.intake_last_poll_ms.lock_safe().clone();
        let due = intake::due_intake_polls(now, &minutes, &last_poll);
        for group in due {
            let Some(repo) = repos.get(&group) else { continue };
            // The argv is built by `intake::issue_list_argv` rather than
            // spelled inline so its `--limit` is pinned by a test: `gh issue
            // list` defaults to the 30 NEWEST open issues, which silently hid
            // most of a mid-sized repo's backlog from both diffs below
            // (measured on loomux itself: 30 of 94). See
            // `intake::MAX_INTAKE_ISSUES` for why 300, and why hitting the
            // bound is reported rather than silently applied.
            let issue_argv = intake::issue_list_argv();
            let issue_args: Vec<&str> = issue_argv.iter().map(String::as_str).collect();
            let issues_raw = self.gh_capture(repo, &issue_args);
            // Same treatment for the PR half, and for a sharper reason (#795):
            // `gh pr list` also defaults to the 30 NEWEST, and
            // `pr_check_deltas`/`pr_comment_deltas` prune on absence — so past
            // the default the window churns and a PR re-entering it re-fires a
            // terminal check state, or a comment, it already reported. See
            // `intake::MAX_INTAKE_PRS` for why 200 rather than the issue bound.
            //
            // That argv also carries `comments,reviews` (#864): the
            // newest-comment timestamp is the one delta the orchestrator still
            // polled by hand on every tick, and folding it into a call that was
            // already being made costs zero extra round-trips (the two calls
            // per due group this poller is budgeted for are unchanged).
            //
            // It DOES widen the response, and by a lot: `gh` has no sub-field
            // selection, so every comment and review BODY comes down whether or
            // not anything reads them. Measured on this repo at 14 open PRs
            // (rev-368 F3): 20,034 bytes without the two fields, 288,284 with —
            // 14.4x, ~1-2s either way, well inside `GH_CAPTURE_TIMEOUT`. The
            // round-trip count is what stays flat, NOT the byte count: it grows
            // with total discussion volume across the (now `MAX_INTAKE_PRS`,
            // not gh's 30) open PRs in the window, so a busier repo pays
            // proportionally more. `intake::parse_pr_list` skips the bodies
            // without allocating them, so the cost lands on one larger response
            // string per poll rather than per comment, and `capture_raw_inner`
            // reads to end with no cap — there is no truncation path that could
            // silently drop the check-state half of the same response.
            let pr_argv = intake::pr_list_argv();
            let pr_args: Vec<&str> = pr_argv.iter().map(String::as_str).collect();
            let prs_raw = self.gh_capture(repo, &pr_args);
            self.intake_last_poll_ms.lock_safe().insert(group.clone(), now);

            // Full-autonomy eligibility inputs (#778), resolved before the
            // seen-state lock: this group's own hold-label spelling (a repo
            // may rename it — the veto is a consent boundary, so the poller
            // reads the resolved profile rather than a const), and the issue
            // numbers the board already tracks. Both are cheap and neither is
            // computed for a group that isn't in full autonomy. A group that
            // opted OUT of intake polling entirely (`intake_poll_minutes:
            // Some(0)`) is never due here at all, in this mode as in any
            // other: its orchestrator still gets the bounded fallback
            // heartbeat, and the contract has it sweep for eligible work
            // itself — the gate stays a gate, not a second consent surface.
            //
            // Read outside the `intake_seen` lock that `eligible_deltas` later
            // writes under, which leaves one narrow residual (rev-266 NB2): a
            // re-aim landing between this read and that write has its
            // `set_full_autonomy` re-arm overwritten, losing that one triage
            // trigger. Left as-is deliberately — closing it means taking
            // `full_autonomy_groups` while holding `intake_seen`, the exact
            // reverse of the order `set_full_autonomy` uses, trading a
            // microsecond window on a human-driven toggle for a lock-order
            // inversion. The ON notice reaches the orchestrator either way, and
            // the next poll re-announces anything genuinely new.
            let full_autonomy = self.is_full_autonomy(&group);
            let (hold_label, board_tracked) = if full_autonomy {
                let hold = self
                    .groups
                    .lock_safe()
                    .get(&group)
                    .map(|g| g.guardrails.intake.hold.clone())
                    .unwrap_or_else(|| workflow::builtin_intake_profile().hold);
                let tasks = self.tasks(&group);
                let refs: Vec<&str> = tasks.iter().filter_map(|t| t.issue.as_deref()).collect();
                (hold, intake::board_tracked_issues(&refs))
            } else {
                (String::new(), HashSet::new())
            };

            let (label_signals, pr_signals, comment_signals, eligible_signals, truncated) = {
                let mut seen = self.intake_seen.lock_safe();
                let state = seen.entry(group.clone()).or_default();
                // Parsed once and read by both diffs — the label delta and the
                // eligibility delta are two questions about the same `gh issue
                // list` response, not two fetches.
                let issues = issues_raw.as_deref().ok().and_then(intake::parse_issue_list);
                let listing = issues.as_deref().map(intake::OpenIssueList::from_fetch);
                let labels = listing
                    .map(|l| intake::label_deltas(&mut state.labels, l.issues))
                    .unwrap_or_default();
                let eligible = intake::eligible_deltas(
                    &mut state.eligible,
                    full_autonomy,
                    listing,
                    &hold_label,
                    &board_tracked,
                );
                // One parse feeds both PR diffs — check-state transitions and
                // comment/review activity are two questions about the same
                // `gh pr list` response, not two polls — and both read it
                // through the same `OpenPrList`, so the completeness flag
                // gating one prune gates the other.
                let parsed_prs = prs_raw.as_deref().ok().and_then(intake::parse_pr_list);
                let pr_listing = parsed_prs.as_deref().map(intake::OpenPrList::from_fetch);
                let prs = pr_listing
                    .map(|l| intake::pr_check_deltas(&mut state.pr_checks, l))
                    .unwrap_or_default();
                let comments = pr_listing
                    .map(|l| intake::pr_comment_deltas(&mut state.pr_comments, l))
                    .unwrap_or_default();
                let truncated = intake::IntakeTruncation {
                    issues: listing.is_some_and(|l| !l.complete),
                    prs: pr_listing.is_some_and(|l| !l.complete),
                };
                (labels, prs, comments, eligible, truncated)
            };
            if label_signals.is_empty()
                && pr_signals.is_empty()
                && comment_signals.is_empty()
                && eligible_signals.is_empty()
            {
                continue;
            }
            let summary = intake::intake_wake_summary(
                &label_signals,
                &pr_signals,
                &comment_signals,
                &eligible_signals,
                truncated,
            );
            self.audit(&group, brand::AUDIT_ACTOR, "intake-signal", json!({ "summary": summary }));
            // Fold into any not-yet-delivered pending summary rather than
            // clobbering it — two poll scans can each find something new
            // before the orchestrator's quiet window next elapses and the
            // tick consumes (clears) it. Bounded (`PendingIntake`, rev-33
            // finding B2) — a group that never actually ticks (sustained
            // output activity) can't accumulate this unboundedly.
            self.intake_pending.lock_safe().entry(group.clone()).or_default().push(summary);
        }
    }

    /// When this group's intake scan last actually reached the `gh` call and
    /// stamped (`intake::due_intake_polls`' per-group floor), if ever.
    ///
    /// #406 review (rev-157, blocking 1): this exists so a test can assert the
    /// EFFECT of the unified tick's intake half — that `poll_intake` really
    /// ran — rather than the `GhPollTick.intake_scanned` flag, which is only
    /// the scheduler's decision and stays true even if the call under it is
    /// deleted. The same `#[doc(hidden)]` test seam as `seed_intake_pending`
    /// below, read-only.
    #[doc(hidden)] // pub for integration tests: observe that a scan ran, without shelling to `gh`
    pub fn intake_last_poll_at(&self, group: &GroupId) -> Option<u64> {
        self.intake_last_poll_ms.lock_safe().get(group).copied()
    }

    /// The issue numbers this group's intake poller last saw as
    /// eligible-unstarted (#778), sorted.
    ///
    /// Same `#[doc(hidden)]` test-seam rationale as `intake_last_poll_at`
    /// above: `poll_intake`'s own path needs a live `gh`, so without a seam
    /// the one property that makes an enable a **triage trigger** — that
    /// turning full autonomy on empties this set, so the next poll announces
    /// the whole backlog — could only be asserted by re-implementing it in
    /// the test.
    #[doc(hidden)] // pub for integration tests: observe the eligible seen-set without shelling to `gh`
    pub fn intake_eligible_seen(&self, group: &GroupId) -> Vec<u64> {
        let seen = self.intake_seen.lock_safe();
        let mut n: Vec<u64> = seen.get(group).map(|s| s.eligible.iter().copied().collect()).unwrap_or_default();
        n.sort_unstable();
        n
    }

    #[doc(hidden)] // pub for integration tests: seed the eligible seen-set without shelling to `gh`
    pub fn seed_intake_eligible_seen(&self, group: &GroupId, numbers: &[u64]) {
        self.intake_seen.lock_safe().entry(group.clone()).or_default().eligible =
            numbers.iter().copied().collect();
    }

    #[doc(hidden)] // pub for integration tests: seed a pending intake signal without shelling to `gh`
    pub fn seed_intake_pending(&self, group: &GroupId, summary: &str) {
        self.intake_pending.lock_safe().entry(group.clone()).or_default().push(summary.to_string());
    }

    #[doc(hidden)] // pub for integration tests: control the fallback-due reference point precisely
    pub fn seed_idle_tick_last_fired(&self, group: &GroupId, ms: u64) {
        self.idle_tick_last_fired_ms.lock_safe().insert(group.clone(), ms);
    }

    /// #864: this group's consecutive delta-free fallback-wake count.
    ///
    /// Read-only, and deliberately NOT a way to test the backoff: the tests
    /// that matter drive `idle_tick_tick` over a simulated timeline and assert
    /// on WHEN ticks actually land, because that is the behavior the issue
    /// asks for. This exists for the one assertion that timeline shape can't
    /// make sharply — that a reset really cleared the counter rather than the
    /// cadence merely looking right for some other reason.
    #[doc(hidden)] // pub for integration tests
    pub fn idle_tick_empty_streak_of(&self, group: &GroupId) -> u32 {
        self.idle_tick_empty_streak.lock_safe().get(group).copied().unwrap_or(0)
    }
}
