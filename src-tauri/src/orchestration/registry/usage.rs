//! Usage accounting: the per-agent usage snapshot and its merge, the usage
//! time series (`usage_series`, `series_sample`), the group's usage and token
//! totals, and the cost basis, as an `impl OrchRegistry` block (#3498). The
//! designs are `docs/design/group-cost-tracking.md`,
//! `docs/design/usage-store.md` (where the rows live and when the disk is
//! written) and `docs/design/token-charts.md`.

use super::*;

impl OrchRegistry {
    /// The group's lifetime usage-token total (live + historical snapshots), the
    /// figure the autonomy budget meters against. Reuses the `group_usage`
    /// computation so the live-agent refresh and the exact-token summing live
    /// in one place.
    pub(in crate::orchestration) fn group_token_total(&self, group: &GroupId) -> u64 {
        self.group_token_total_within(group, Duration::ZERO)
    }

    /// [`Self::group_token_total`], sharing the polled usage memo when
    /// `max_age` allows (#743 S4b). The panel read passes
    /// [`USAGE_POLL_MAX_AGE`] so `orch_autonomy` stops re-running the whole
    /// usage chain a second time inside the same 2 s tick the group view
    /// already ran it in; the anchor and the budget enforcer pass
    /// `Duration::ZERO` and keep an exact, live figure.
    pub(in crate::orchestration) fn group_token_total_within(&self, group: &GroupId, max_age: Duration) -> u64 {
        self.group_usage_within(group, max_age)
            .get("lifetime_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }

    /// The key an agent's usage row is stored under: its CLI session id, else
    /// `agent:<id>` (see [`UsageSnapshot::key`]). One spelling, because the
    /// snapshot writes a row by it and [`Self::stored_detected_cache_ttl`]
    /// finds that row again by it.
    pub(in crate::orchestration) fn usage_key(entry: &AgentEntry) -> String {
        entry.session_id.clone().unwrap_or_else(|| format!("agent:{}", entry.id))
    }

    /// The cache lifetime the usage tick last DETECTED for the row stored under
    /// `key` (`UsageSnapshot::detected_cache_ttl_minutes`, #3831), for the
    /// orchestrator's idle-compact backstop — the second caller of
    /// `cacheage::resolve_ttl`, which must read the same middle rung the usage
    /// row does or a pane's chip and its nudge would run on two TTLs.
    ///
    /// Read from the store already in memory, never from the disk: the backstop
    /// runs on the compact-nudge loop, and a file read there to refine a
    /// threshold would be the second poll `docs/design/cache-age.md` rules out.
    /// A group whose store is not loaded answers `None`, and the ladder falls
    /// to the CLI's conservative default. In the running app the usage tick
    /// keeps every live group's store loaded, so that is the first seconds
    /// after start and nothing else.
    pub(in crate::orchestration) fn stored_detected_cache_ttl(&self, group: &GroupId, key: &str) -> Option<u32> {
        self.usage_lock
            .lock_safe()
            .by_group
            .get(group)?
            .rows
            .iter()
            .find(|r| r.key == key)
            .and_then(|r| r.detected_cache_ttl_minutes)
    }

    /// Compute an agent's current usage from the best available source, in
    /// preference order: the CLI's own session record (token counts — exact,
    /// and readable even after the pane is gone) → a last-resort parse of the
    /// dollar figure the CLI prints in its statusline. Returns a snapshot
    /// keyed for durable accumulation (issue #42).
    ///
    /// `#[doc(hidden)] pub` for the integration tests: this is the function
    /// that decides which source an agent's usage comes from, and pinning that
    /// choice per CLI is only meaningful against the real one.
    #[doc(hidden)] // pub for integration tests
    pub fn compute_usage_snapshot(&self, entry: &AgentEntry, cli: &str) -> UsageSnapshot {
        let key = Self::usage_key(entry);
        let role = entry.role.as_str();
        let mut snap = UsageSnapshot {
            key,
            agent_id: entry.id.clone(),
            name: entry.name.clone(),
            role: role.to_string(),
            source: "none".to_string(),
            // Set HERE, in the initializer, and not on each arm's way out:
            // this function has an early `return snap` per source, so a
            // per-arm assignment is a field the next arm forgets (#2011).
            block: entry.block.clone(),
            cli: cli.to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cost_usd: None,
            estimated: false,
            model: None,
            current_model: None,
            updated_ms: now_ms(),
            // Filled by the merge, which is the only place the previous
            // reading is in hand (#3407).
            activity: Default::default(),
            // #3831: filled by the arms that fold a per-turn record.
            first_context_tokens: None,
            detected_cache_ttl_minutes: None,
        };

        // #2850 S3b — a structured pane reported its own figures, so nothing
        // below applies: there is no transcript file to read and no statusline
        // to parse. Checked FIRST because the fallbacks would otherwise return
        // `none` for a pane that has perfectly good numbers.
        if let Some(u) = self.stream_usage_for(&entry.id) {
            snap.source = structured::USAGE_SOURCE_STREAM.to_string();
            snap.input_tokens = u.input;
            snap.output_tokens = u.output;
            snap.cache_read_tokens = u.cache_read;
            snap.cache_creation_tokens = u.cache_creation;
            snap.cost_usd = u.cost_usd;
            snap.estimated = u.estimated.unwrap_or(false);
            snap.updated_ms = u.updated_ms;
            return snap;
        }

        // Primary source: per-session token usage from the transcript. Claude
        // Code writes it; Copilot has no readable token record today (see the
        // `usage` module's design note), so it falls through to the statusline.
        if cli == "claude" {
            if let Some(sid) = entry.session_id.as_deref() {
                // Use the test override when set, else the default ~/.claude root.
                let root = self
                    .claude_projects_dir
                    .lock_safe()
                    .clone()
                    .or_else(crate::usage::default_claude_projects_root);
                // #1239: the cursor cache, not a fresh whole-file parse. Same
                // totals, but a tick only folds on what the agent appended
                // since the previous one — this call is on the app's hottest
                // poll and used to re-parse every live agent's whole
                // transcript once a second.
                if let Some(u) = root.as_deref().and_then(|r| {
                    self.usage_cursors.session_usage(
                        crate::usage::TranscriptKind::Claude,
                        r,
                        sid,
                    )
                }) {
                    if u.tokens.total() > 0 {
                        snap.source = "transcript".to_string();
                        snap.input_tokens = u.tokens.input_tokens;
                        snap.output_tokens = u.tokens.output_tokens;
                        snap.cache_creation_tokens = u.tokens.cache_creation_tokens;
                        snap.cache_read_tokens = u.tokens.cache_read_tokens;
                        snap.cost_usd = u.cost_usd;
                        snap.estimated = true; // token-derived dollar estimate
                        snap.current_model = u.current_model;
                        snap.model = u.model;
                        snap.first_context_tokens = u.first_context_tokens;
                        snap.detected_cache_ttl_minutes = u.detected_cache_ttl_minutes;
                        return snap;
                    }
                }
            }
        }

        // OpenCode writes no transcript file: its `session` row already holds
        // the dollar cost it computed itself plus five token counters, in the
        // group's own SQLite store (#722). So the dollars here are REPORTED,
        // not estimated — `estimated: false` is what keeps `group_usage` from
        // blending them into a total labelled as a price-table guess.
        //
        // A degrade (no store, unopenable, schema drift) never fails the
        // snapshot — it falls through to the statusline, and to a zero-usage
        // agent if that finds nothing either, which is the whole
        // degraded-not-fatal posture (see `crate::opencodedb`). But it is not
        // silent either: `note_opencode_db_degrade` writes ONE audit line per
        // episode, so a drifted schema and a never-booted pane stop looking
        // identical to whoever is debugging, without this polled path becoming
        // an audit entry every UI tick (rev-298 F2).
        if cli == "opencode" {
            // Until an opencode pane's session is identified, there is no id
            // to key on and this arm simply does not fire.
            if let Some(sid) = entry.session_id.as_deref() {
                let db = self.opencode_db_path(&entry.group);
                let read = crate::usage::opencode_session_usage(&db, sid);
                self.note_opencode_db_degrade(&entry.group, &db, read.as_ref().err());
                if let Ok(Some(u)) = read {
                    // Same guard as the claude arm: a session row that exists
                    // but has counted nothing yet must not overwrite history
                    // with zeros, nor pre-empt the statusline fallback.
                    if u.tokens.total() > 0 {
                        snap.source = "session-db".to_string();
                        snap.input_tokens = u.tokens.input_tokens;
                        snap.output_tokens = u.tokens.output_tokens;
                        snap.cache_creation_tokens = u.tokens.cache_creation_tokens;
                        snap.cache_read_tokens = u.tokens.cache_read_tokens;
                        snap.cost_usd = u.cost_usd;
                        snap.estimated = false; // priced by opencode, not by us
                        snap.current_model = u.current_model;
                        snap.model = u.model;
                        snap.first_context_tokens = u.first_context_tokens;
                        snap.detected_cache_ttl_minutes = u.detected_cache_ttl_minutes;
                        return snap;
                    }
                }
            }
        }

        // pi writes a JSONL session file per pane, like claude — but into the
        // GROUP's own store (`pi_sessions_dir`, a `--session-dir` loomux hands
        // it), not a per-user tree keyed on an encoded cwd. Two consequences
        // worth stating where the arm is, because they are what this arm is
        // cheap and reliable BECAUSE of:
        //
        // - **#2167 cannot happen here.** That defect was a claude session
        //   whose transcript existed and whose row read zero, because the
        //   collector resolved the pane's CLI from its class's DEFAULT block
        //   rather than its own. The CLI resolution is fixed and shared — this
        //   arm is reached with the `cli` its two callers took from
        //   `Guardrails::cli_for_block` (`compute_group_usage`) and
        //   `cli_for_agent` (`mark_dead`), so a pi pane in a roster's SECOND
        //   pi block is read as pi like any other. What is additionally true
        //   for pi is that the file's LOCATION involves no cwd slug at all: the
        //   store is the group dir and the id is loomux's own, so the
        //   slug-shaped half of #2167's suspicion has no surface here.
        // - **The id is present from spawn.** pi premints
        //   (`CliCaps::premints_session_id`), so unlike opencode this arm does
        //   not sit idle waiting for the pane to announce a session.
        //
        // The dollars are pi's OWN (`usage.cost.total` per entry, summed), so
        // `estimated: false` — the same reported-not-estimated posture as
        // opencode, for the same reason: blending a vendor-priced figure into a
        // total the UI labels a price-table guess would misdescribe both.
        if cli == "pi" {
            if let Some(sid) = entry.session_id.as_deref() {
                let dir = self.pi_sessions_dir(&entry.group);
                if let Some(u) = self.usage_cursors.session_usage(
                    crate::usage::TranscriptKind::Pi,
                    &dir,
                    sid,
                ) {
                    // Same guard as the claude and opencode arms: a session
                    // file that exists but has counted nothing yet (pi writes
                    // the header before the first assistant turn) must not
                    // overwrite history with zeros, nor pre-empt the statusline
                    // fallback.
                    if u.tokens.total() > 0 {
                        snap.source = "pi-transcript".to_string();
                        snap.input_tokens = u.tokens.input_tokens;
                        snap.output_tokens = u.tokens.output_tokens;
                        snap.cache_creation_tokens = u.tokens.cache_creation_tokens;
                        snap.cache_read_tokens = u.tokens.cache_read_tokens;
                        snap.cost_usd = u.cost_usd;
                        snap.estimated = false; // priced by pi, not by us
                        snap.current_model = u.current_model;
                        snap.model = u.model;
                        snap.first_context_tokens = u.first_context_tokens;
                        snap.detected_cache_ttl_minutes = u.detected_cache_ttl_minutes;
                        return snap;
                    }
                }
            }
        }

        // codex writes a JSONL rollout per thread, like claude and pi -- but
        // into the HUMAN's own store (`CODEX_HOME/sessions/YYYY/MM/DD/`), not a
        // per-group one, because a per-agent `CODEX_HOME` would relocate
        // `auth.json` and boot every pane logged out (`docs/design/codex.md`,
        // "Deliberately not done"). Three consequences worth stating here,
        // because they are what this arm's shape is:
        //
        // - **It is IDLE until the watcher binds an id.** codex has no public
        //   pre-mint flag (`CliCaps::premints_session_id` is false), so the
        //   thread id is learned from the store after the pane's first turn --
        //   opencode's shape, not claude's or pi's. Until then there is nothing
        //   to key on, and guessing (newest rollout, only rollout) would charge
        //   this pane another pane's conversation in a store several panes
        //   share.
        // - **The store is keyed, not joined.** A rollout's file name carries a
        //   timestamp nobody can re-derive plus an optional `_<rollout>` revert
        //   suffix, so the file is FOUND by walking the store for the thread id
        //   (`find_codex_session_file`), never spelled from it.
        // - **The dollars are OURS, and today there are none.** codex records
        //   tokens only, so this is the claude posture -- `estimated: true` --
        //   rather than opencode's and pi's reported one. No codex model sits
        //   in `price_for`'s table, which is dated Anthropic rates, so the row
        //   is tokens with `cost_usd: None`. That is an honest blank rather
        //   than an undated guess, and the label is what keeps a group total
        //   mixing codex with claude describable.
        if cli == "codex" {
            if let Some(sid) = entry.session_id.as_deref() {
                if let Some(u) = crate::sessions::codex_sessions_root().and_then(|root| {
                    self.usage_cursors.session_usage(
                        crate::usage::TranscriptKind::Codex,
                        &root,
                        sid,
                    )
                }) {
                    // Same guard as every arm above: a rollout that exists but
                    // has counted nothing yet (codex writes the header at the
                    // first `persist()`, before any response completes) must
                    // not overwrite history with zeros, nor pre-empt the
                    // statusline fallback.
                    if u.tokens.total() > 0 {
                        snap.source = "codex-transcript".to_string();
                        snap.input_tokens = u.tokens.input_tokens;
                        snap.output_tokens = u.tokens.output_tokens;
                        snap.cache_creation_tokens = u.tokens.cache_creation_tokens;
                        snap.cache_read_tokens = u.tokens.cache_read_tokens;
                        snap.cost_usd = u.cost_usd;
                        snap.estimated = true; // token-derived, and unpriced today
                        snap.current_model = u.current_model;
                        snap.model = u.model;
                        snap.first_context_tokens = u.first_context_tokens;
                        snap.detected_cache_ttl_minutes = u.detected_cache_ttl_minutes;
                        return snap;
                    }
                }
            }
        }

        // Last resort: the dollar figure the CLI renders in its own statusline.
        // Unreliable (empty on subscription/Max accounts; gone once the pane is
        // killed), so it only runs when no transcript usage was found.
        if let Some(app) = self.app.lock_safe().clone() {
            if let Some(pty) = entry.pty_id {
                let ptys = app.state::<crate::pty::PtyManager>();
                if let Some(c) = statusline_cost(&ptys, pty) {
                    snap.source = "statusline".to_string();
                    snap.cost_usd = Some(c); // reported by the CLI, not estimated
                }
            }
        }
        snap
    }

    /// Record — at most once per episode — that a group's opencode store could
    /// not be read, and why (#722, rev-298 F2).
    ///
    /// The problem this solves: `compute_usage_snapshot` treats every
    /// [`crate::opencodedb::Unavailable`] the same way, because there is
    /// nothing useful it could do differently. That is right for the SNAPSHOT
    /// and wrong for the RECORD — a drifted schema and a pane whose CLI never
    /// booted both surface as "not from the store", so the one condition that
    /// needs a human looks exactly like the one that needs nobody.
    ///
    /// Why a latch and not just an audit call: this runs on every usage
    /// computation, and since #1608 the busiest caller is the snapshot
    /// publisher — `views::compute_group` recomputes the strip tier once per
    /// `views::VIEW_PUBLISH_INTERVAL` for every live and strip-leased group. So
    /// an unlatched line would be an audit entry per group per second for as
    /// long as the condition lasted — the log flooded worst precisely when
    /// something is wrong with it. (Before #1608 this said "the group view
    /// polls `group_usage` every couple of seconds". That reading is now false
    /// twice over: the polled path reaches `group_usage_live_within`, never
    /// `group_usage`, whose only production caller is the MCP `group_usage`
    /// tool — and the real cadence is faster, not slower, so the latch matters
    /// MORE than the old sentence claimed.) Keyed
    /// by KIND rather than message so a varying error string cannot defeat the
    /// latch, and dropped on the first successful read so a recurrence after a
    /// real recovery is diagnosed again instead of being silenced for the
    /// process's life.
    ///
    /// `Absent` is passed through without a line on purpose: no store yet is
    /// the ordinary state of a group whose opencode panes have not booted, and
    /// auditing it would put a line in every claude-only group that ever spawns
    /// an opencode pane.
    fn note_opencode_db_degrade(
        &self,
        group: &GroupId,
        db: &Path,
        err: Option<&crate::opencodedb::Unavailable>,
    ) {
        use crate::opencodedb::Unavailable;
        let kind = match err {
            Some(Unavailable::Open(_)) => "open",
            Some(Unavailable::Query(_)) => "unreadable",
            // A successful read, or a store that simply is not there yet:
            // either way this group has no live degrade episode.
            Some(Unavailable::Absent) | None => {
                self.opencode_db_degraded.lock_safe().remove(group);
                return;
            }
        };
        let mut seen = self.opencode_db_degraded.lock_safe();
        if seen.get(group) == Some(&kind) {
            return; // already diagnosed this episode
        }
        seen.insert(group.clone(), kind);
        // Dropped before auditing: `audit` takes its own locks, and holding an
        // unrelated one across it is how lock-order bugs start.
        drop(seen);
        self.audit(group, brand::AUDIT_ACTOR, "opencode-usage-degraded", json!({
            "kind": kind,
            "detail": err.map(ToString::to_string).unwrap_or_default(),
            "db": db.to_string_lossy(),
        }));
    }

    /// Make sure `stores` holds this group's usage store and that it still
    /// matches the disk (#3677). Called with [`Self::usage_lock`] held — the
    /// guard is `stores`.
    ///
    /// The store is read off the disk ONCE and then trusted for as long as the
    /// two files' stamps do not move (`UsageStore::matches_disk`: two `stat`s,
    /// whatever the row count). A stamp that has moved means something other
    /// than this store wrote the file — a second process, a hand edit, a group
    /// directory that was removed and made again — and the store is dropped and
    /// re-read before anything is merged into it, which is what the per-tick
    /// re-read this replaces used to answer for.
    ///
    /// `Err` is "a file is there and could not be read": no store is cached and
    /// the caller must decline its write (see `load_usage_store`).
    fn ensure_usage_store(&self, stores: &mut UsageStores, group: &GroupId, dir: &Path) -> Result<(), String> {
        if stores.by_group.get(group).is_some_and(|s| s.matches_disk(dir)) {
            return Ok(());
        }
        stores.by_group.remove(group);
        let load = load_usage_store(dir)?;
        for p in &load.preserved {
            // The file exists but is corrupt (interrupted write, manual
            // edit). Silently treating it as empty would wipe all
            // killed-agent history, so it was preserved for inspection and
            // the store starts without it rather than overwriting it on the
            // next upsert. `AUDIT_LOCK` under `usage_lock` is the one nesting
            // that lock documents.
            self.audit(group, brand::AUDIT_ACTOR, "usage-corrupt",
                json!({ "file": p.file, "error": p.error, "preserved": p.preserved.to_string_lossy() }));
        }
        stores.by_group.insert(group.clone(), load.store);
        Ok(())
    }

    /// Merge one snapshot into `list`, matched by `key`, and say what that did:
    /// added a row, changed one's PERSISTED content, or neither. Pure — no lock,
    /// no I/O.
    ///
    /// Factored out of `upsert_usage_snapshot` (#743 S4b) so a whole tick's
    /// worth of live-agent snapshots can go through ONE load-write cycle
    /// without the merge rule being restated per caller. The rule itself is
    /// unchanged.
    ///
    /// **The return value is what a write is decided on (#3677), and it does
    /// not count `updated_ms`.** That field moves on every tick because the
    /// tick ran, so counting it would make every tick a write. The row in
    /// `list` still gets the fresh stamp — what a caller is handed is as
    /// current as it ever was — and the file gets it with the next real change.
    fn merge_usage_entry(list: &mut Vec<Arc<UsageSnapshot>>, snap: UsageSnapshot) -> RowMerge {
        match list.iter_mut().find(|s| s.key == snap.key) {
            Some(slot) => {
                let existing: &UsageSnapshot = &**slot;
                // A transcript only ever grows, so a read that comes back empty
                // (e.g. transient failure, or the pane died before Copilot wrote
                // a token record) must not clobber usage we already captured —
                // otherwise a kill could zero a session's spend. Refresh the
                // identity fields but keep the richer usage.
                //
                // **"Empty" is a property of the FIGURES, never of the source**
                // (#2167). This used to read `source == "none"`, which let the
                // one fallback that reports a zero — a `statusline` parse on a
                // subscription/Max account, where the CLI prints `$0.00`
                // whatever the real spend was — overwrite a `transcript` row
                // carrying millions of real tokens with zeros. Every guard
                // arriving here has to read the same rule (CLAUDE.md, "a guard
                // reads every one of its inputs by one rule"), and a source
                // label is not a figure.
                //
                // Residual, stated because it is not closed: a statusline read
                // with a NON-zero dollar figure still replaces a token-bearing
                // row, losing its tokens for a cost estimate. That is the
                // pre-existing behaviour and a separate call about which of two
                // partial records is the better one; what changes here is only
                // that a row saying *nothing at all* stops winning.
                let tokens_of = |s: &UsageSnapshot| {
                    s.input_tokens
                        + s.output_tokens
                        + s.cache_creation_tokens
                        + s.cache_read_tokens
                };
                let new_empty =
                    tokens_of(&snap) == 0 && snap.cost_usd.unwrap_or(0.0) <= 0.0;
                let old_has_data =
                    tokens_of(&*existing) > 0 || existing.cost_usd.unwrap_or(0.0) > 0.0;
                // #3407: fold this reading into the row's activity BEFORE either
                // branch below replaces anything — both need the previous
                // counters, and the fold is what reads them. On the no-downgrade
                // branch the new total is zero, so the fold carries the old
                // activity unchanged; on the replace branch it is what keeps a
                // row's activity from being reset by every tick's fresh snapshot.
                // Each reading carries its SOURCE: growth is measured against the
                // row's last token-bearing reading of the same source, so the
                // zero-token statusline row the branch below may let replace a
                // transcript row can never become the baseline the next
                // transcript read is differenced against (review round 1, N2).
                let counters_of = |s: &UsageSnapshot| loomux_engine::cacheage::Counters {
                    input: s.input_tokens,
                    output: s.output_tokens,
                    cache_creation: s.cache_creation_tokens,
                    cache_read: s.cache_read_tokens,
                };
                let activity = loomux_engine::cacheage::fold_activity(
                    &existing.activity,
                    loomux_engine::cacheage::Reading {
                        source: &existing.source,
                        counters: counters_of(&*existing),
                        cost_usd: existing.cost_usd,
                    },
                    loomux_engine::cacheage::Reading {
                        source: &snap.source,
                        counters: counters_of(&snap),
                        cost_usd: snap.cost_usd,
                    },
                    snap.updated_ms,
                );
                let mut next = if new_empty && old_has_data {
                    UsageSnapshot {
                        agent_id: snap.agent_id,
                        name: snap.name,
                        role: snap.role,
                        updated_ms: snap.updated_ms,
                        ..existing.clone()
                    }
                } else {
                    snap
                };
                next.activity = activity;
                let changed = !usage_rows_persist_alike(existing, &next);
                // `updated_ms` is what orders a row in `usage-live.json`
                // against the same key in `usage.json` when the store is
                // loaded (`fold_usage_overlay`), so per row it never goes
                // backwards, and it moves STRICTLY with every change to what
                // persists: two readings in one millisecond, or a wall clock
                // that stepped back, must not leave the older row looking like
                // the newer one. The fold above took the reading's own stamp
                // before this, so the activity clock is untouched.
                next.updated_ms = if changed {
                    next.updated_ms.max(existing.updated_ms.saturating_add(1))
                } else {
                    next.updated_ms.max(existing.updated_ms)
                };
                *slot = Arc::new(next);
                if changed {
                    RowMerge::Changed
                } else {
                    RowMerge::Unchanged
                }
            }
            // A FIRST sighting is not folded (#3407): a row that arrives already
            // carrying tokens — a session this store never saw — has a
            // cumulative total, not a request, and charging that total to one
            // "wake" would report a session's whole history as its last wake.
            // Its activity stays unknown until its counters next move.
            None => {
                list.push(Arc::new(snap));
                RowMerge::Added
            }
        }
    }

    /// Merge `incoming` into the group's durable usage store under
    /// [`Self::usage_lock`] and return the resulting list — the store as it now
    /// sits on disk.
    ///
    /// **One merge and at most one write for the whole batch (#743 S4b), from a
    /// store read once and kept (#3677).** The shape before #743 ran a full
    /// `usage.json` read plus an atomic rewrite *per live agent*; the shape
    /// after it ran one of each per tick, which was still every row the group
    /// has ever had, parsed and rewritten once a second whether or not a figure
    /// had moved. Now the rows live in memory behind this lock, a tick costs two
    /// `stat`s to check that they still match the disk, and what it writes is
    /// decided by `plan_usage_write`:
    ///
    /// - nothing, when no row's persisted content changed;
    /// - `usage-live.json`, the overlay, holding only the rows that have moved
    ///   since `usage.json` was last written — so a tick's write is as large as
    ///   the agents spending and no larger;
    /// - `usage.json` whole, when the set of rows changes or one settles: a key
    ///   the store has not seen before, or `settle` (a kill snapshot from
    ///   outside the tick). That is what keeps `usage.json` a row for every key.
    ///
    /// The returned list is the store's rows, which **when the write succeeds**
    /// ARE what the two files hold. When it fails they are not, and the list
    /// falls back to a re-read rather than reporting spend that never persisted
    /// — see the write site.
    ///
    /// An empty `incoming` on the tick writes nothing — it is a plain read,
    /// which is what a group with no live agents does every tick.
    ///
    /// The design, what it rejected, and what an older build sees:
    /// `docs/design/usage-store.md`.
    fn merge_usage_snapshots(&self, group: &GroupId, incoming: Vec<UsageSnapshot>, settle: bool) -> Vec<Arc<UsageSnapshot>> {
        let dir = self.group_dir(group);
        let mut stores = self.usage_lock.lock_safe();
        if let Err(e) = self.ensure_usage_store(&mut stores, group, &dir) {
            // A file is there and could not be read. Nothing is known about
            // the rows it holds, so nothing is written over them and no store
            // is kept: the next tick looks again. What the caller gets is this
            // tick's own readings merged over nothing — the same list the
            // read-failure path returned before #3677, minus the write that
            // then replaced the unread file with it.
            drop(stores);
            // Latched, and outside the lock: `note_poll_read` takes a lock of
            // its own, and `usage_lock` nests nothing but `AUDIT_LOCK`.
            self.note_poll_read(group, "usage-store", Err(&e));
            let mut unsaved = Vec::new();
            for snap in incoming {
                Self::merge_usage_entry(&mut unsaved, snap);
            }
            return unsaved;
        }
        let fail_writes = stores.fail_writes;
        let Some(store) = stores.by_group.get_mut(group) else {
            return Vec::new(); // unreachable: `ensure_usage_store` just put it there
        };
        if incoming.is_empty() && !settle {
            let rows = store.rows.clone();
            drop(stores);
            self.note_poll_read(group, "usage-store", Ok(()));
            return rows;
        }
        let (mut changed, mut added) = (false, false);
        for snap in incoming {
            let key = snap.key.clone();
            let merged = Self::merge_usage_entry(&mut store.rows, snap);
            if merged != RowMerge::Unchanged {
                store.overlay.insert(key);
                changed = true;
            }
            added |= merged == RowMerge::Added;
        }
        let wrote = match plan_usage_write(changed, added, settle, !store.overlay.is_empty()) {
            UsageWrite::Nothing => Ok(()),
            _ if fail_writes => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "usage store write fault (test seam)",
            )),
            UsageWrite::Live => store.write_live(&dir),
            UsageWrite::Whole => store.write_whole(&dir),
        };
        let rows = if wrote.is_err() {
            // The merged rows are NOT what is on disk, so they must not be
            // what the caller summarises (rev-231 finding 3) — and they must
            // not stay in the store either, whose whole claim is that it
            // mirrors the disk. Drop it, re-read, and report the figures that
            // actually persisted. The alternative is putting spend on screen
            // that never landed and vanishes on the next tick, and a cost
            // meter that invents a number on a failing disk is worse than one
            // that stops moving. Nothing is lost by the drop: the next tick's
            // reading differs from the re-read rows again, so the write is
            // retried. Costs a read only on the failure path.
            stores.by_group.remove(group);
            match self.ensure_usage_store(&mut stores, group, &dir) {
                Ok(()) => stores.by_group.get(group).map(|s| s.rows.clone()).unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        } else {
            store.rows.clone()
        };
        drop(stores);
        self.note_poll_read(group, "usage-store", Ok(()));
        rows
    }

    /// Upsert one agent's snapshot into the group's durable usage store,
    /// matched by `key`. Public for the kill-snapshot accumulation test, and
    /// used by `mark_dead` to capture an exiting agent's spend.
    ///
    /// A SETTLING merge (#3677): the row goes into `usage.json` itself rather
    /// than the overlay, along with anything else waiting there. A dead agent
    /// has no later tick to carry its row across, and the whole-file write this
    /// costs happens once per agent that ends, not once per second. (The other
    /// whole write is a key's first appearance — see `plan_usage_write`.)
    ///
    /// Invalidates the polled memo: this is a write from OUTSIDE the usage
    /// computation, and a kill's captured spend must not wait out a poll window
    /// before the group view can see it.
    #[doc(hidden)]
    pub fn upsert_usage_snapshot(&self, group: &GroupId, snap: UsageSnapshot) {
        self.merge_usage_snapshots(group, vec![snap], true);
        self.invalidate_usage_memo(group);
    }

    /// Drop the group's in-memory usage rows (#3677). The files are untouched,
    /// and the next read of the group loads them again.
    ///
    /// `end_group`'s call is what keeps the map from holding the rows of every
    /// group this process has ever torn down; a group that is only ever READ
    /// (a strip-leased one from an earlier session) keeps its entry for the
    /// process's life, which is one copy of a file that read path used to
    /// parse every second.
    pub(in crate::orchestration) fn forget_usage_store(&self, group: &GroupId) {
        self.usage_lock.lock_safe().by_group.remove(group);
    }

    /// Make every usage store write fail, so the failed-write path can be
    /// pinned without a filesystem trick that only works on one platform.
    /// Test-only seam (see `UsageStores::fail_writes`).
    #[doc(hidden)]
    pub fn set_usage_write_fault(&self, on: bool) {
        self.usage_lock.lock_safe().fail_writes = on;
    }

    /// Drop the group's memoised usage value (#743 S4b).
    ///
    /// Removes the map entry rather than clearing the cell, and so takes only
    /// the outer map lock: an invalidation can never block on — or deadlock
    /// against — a computation holding the cell. See [`Self::usage_memo`].
    pub(in crate::orchestration) fn invalidate_usage_memo(&self, group: &GroupId) {
        self.usage_memo.lock_safe().remove(group);
    }

    /// The series bucket in force, honouring the test seam.
    fn series_bucket_ms(&self) -> u64 {
        self.series_bucket_override
            .lock_safe()
            .unwrap_or(usageseries::SERIES_BUCKET_MS)
    }

    /// Shorten the series bucket so a test can drive real usage ticks without
    /// sleeping five minutes. Test-only seam (see `series_bucket_override`).
    #[doc(hidden)]
    pub fn set_series_bucket_ms(&self, ms: u64) {
        *self.series_bucket_override.lock_safe() = Some(ms);
    }

    /// The revisit ceiling in force, honouring the test seam.
    fn series_revisit_bytes(&self) -> u64 {
        self.series_revisit_override.lock_safe().unwrap_or(SERIES_REVISIT_BYTES)
    }

    /// Lower the series revisit ceiling so the oversize REPORT can be pinned
    /// without a 32 MB fixture. Test-only seam (see `series_revisit_override`).
    #[doc(hidden)]
    pub fn set_series_revisit_bytes(&self, bytes: u64) {
        *self.series_revisit_override.lock_safe() = Some(bytes);
    }

    /// Sample this tick's usage into `<group>/usage-series.jsonl` (#2011 slice
    /// B) — the time plot's only source.
    ///
    /// # Which thread this is, and what actually makes it safe
    ///
    /// **Not "the view publisher thread".** `compute_group_usage` has one
    /// caller — [`Self::group_usage_memoed`] — and three ways in: the polled
    /// view publisher's tick, a `run_blocking` pool thread (`orch_group_usage`,
    /// `orch_autonomy`), and an MCP `group_usage` request, which gets a fresh
    /// `std::thread::spawn` per request and asks with `Duration::ZERO`, so it
    /// never serves the memo and always recomputes — and therefore always
    /// samples. An agent calling that tool is a writer to this file.
    ///
    /// What holds is the property that was wanted, stated the checkable way:
    ///
    /// - **Never the GUI thread.** All three are off it; nothing reachable from
    ///   a synchronous `#[tauri::command]` gets here (CLAUDE.md constraint 10).
    /// - **One writer at a time, per group.** `group_usage_memoed` holds that
    ///   group's memo cell *across* `compute_group_usage`, so two ticks for one
    ///   group serialize rather than race. That is mutual exclusion, not thread
    ///   identity, and it is what the "one writer" claim on
    ///   [`USAGE_SERIES_FILE`] means.
    ///
    /// Called once per tick from [`Self::compute_group_usage`], **after** the
    /// merge, on the snapshots that tick already computed. The counters are
    /// not re-read; a separate bounded context-signal read, scoped to this
    /// group's running agents, supplies effort. `live_keys` is what
    /// keeps a dead agent's frozen snapshot out — its counters cannot move, so
    /// a row for it would be a duplicate of the last row it wrote while alive,
    /// once per app restart forever.
    ///
    /// Two kinds of row, on the same tick:
    ///
    /// - a **sample** per key the bucket has elapsed for AND whose counters
    ///   moved ([`usageseries::should_sample`]);
    /// - a **mark** when the repo's tuning fingerprint differs from the last
    ///   one this process wrote, checked at most once per bucket.
    ///
    /// **Nothing here fails the tick.** A write error is one missing row in a
    /// cumulative series, which costs resolution and never correctness; a usage
    /// panel that stops painting because a chart file could not be appended to
    /// would be strictly worse. The sampler state is updated only on a
    /// SUCCESSFUL append, so a failed write is retried on the next tick rather
    /// than starting a silent five-minute hole.
    fn series_sample(
        &self,
        group: &GroupId,
        snaps: &[Arc<UsageSnapshot>],
        live_keys: &HashSet<String>,
        context_signals: &HashMap<String, crate::usage::CompactionSignal>,
    ) {
        let bucket = self.series_bucket_ms();
        let dir = self.group_dir(group);
        let now = now_ms();

        // Decide under the lock; append outside it. The lock guards a decision
        // table, and holding it across file I/O is how a per-second tick comes
        // to serialise on a disk.
        let mut to_write: Vec<usageseries::SeriesRow> = Vec::new();
        {
            let mut states = self.series_state.lock_safe();
            let state = states.entry(group.clone()).or_default();
            for s in snaps {
                if !live_keys.contains(&s.key) {
                    continue;
                }
                let sample = usageseries::Sample {
                    ts_ms: now,
                    key: s.key.clone(),
                    agent: s.agent_id.clone(),
                    block: s.block.clone(),
                    cli: s.cli.clone(),
                    role: s.role.clone(),
                    input: s.input_tokens,
                    output: s.output_tokens,
                    cache_w: s.cache_creation_tokens,
                    cache_r: s.cache_read_tokens,
                    cost_usd: s.cost_usd,
                    estimated: s.estimated,
                    source: s.source.clone(),
                    // The CURRENT model, not `s.model`: on claude that is a pricing
                    // pick that lags a switch (or never follows it), and the chart
                    // splits spend and marks switches off this field (#3415).
                    // `s` is the MERGED row, so a live key whose fresh read came
                    // back empty is sampled off its persisted row — which, if it
                    // predates `current_model`, carries only `model`. Falling back
                    // to it keeps that row's sample from reading as a switch to
                    // "unknown model" and back; on every CLI but claude the two
                    // fields are equal anyway.
                    model: s.current_model.clone().or_else(|| s.model.clone()),
                    effort: context_signals
                        .get(&s.agent_id)
                        .and_then(|signal| signal.observed_effort().map(str::to_owned)),
                };
                if usageseries::should_sample(state.last.get(&s.key), &sample, bucket) {
                    to_write.push(usageseries::SeriesRow::Sample(sample));
                }
            }
        }

        for row in &to_write {
            if append_series_line(&dir, row).is_ok() {
                if let usageseries::SeriesRow::Sample(s) = row {
                    let mut states = self.series_state.lock_safe();
                    states.entry(group.clone()).or_default().last.insert(s.key.clone(), s.clone());
                }
            }
        }

        self.series_mark(group, &dir, now, bucket);
    }

    /// Write a `mark` row when the repo's tuning fingerprint has changed.
    ///
    /// The walk is bounded (`tuningfp`'s caps) but not free, so it runs at most
    /// once per bucket — which is also the finest resolution a mark is worth to
    /// a plot bucketed at five minutes. It runs on this tick's thread, whichever
    /// of the three that is, and never on the GUI thread — see
    /// [`Self::series_sample`], which names them and says what serializes them.
    /// The one worth noticing here is the MCP `group_usage` tool: an agent
    /// calling it walks its own repo on an MCP request thread.
    fn series_mark(&self, group: &GroupId, dir: &Path, now: u64, bucket: u64) {
        {
            let mut states = self.series_state.lock_safe();
            let state = states.entry(group.clone()).or_default();
            if state.fp_checked_ms != 0 && now.saturating_sub(state.fp_checked_ms) < bucket {
                return;
            }
            state.fp_checked_ms = now;
        }
        let Some(info) = self.group(group) else { return };
        let fp = tuningfp::fingerprint(Path::new(&info.repo));

        let prev = { self.series_state.lock_safe().get(group).and_then(|s| s.fp.clone()) };
        let prev = match prev {
            Some(p) => p,
            None => {
                // First look of this process's life for this group. Seed the
                // baseline rather than writing a mark: every restart would
                // otherwise stamp a "everything changed" vertical onto a plot
                // where nothing had.
                self.series_state
                    .lock_safe()
                    .entry(group.clone())
                    .or_default()
                    .fp = Some(fp.components);
                return;
            }
        };
        let changed = usageseries::fp_changed(&prev, &fp.components);
        if changed.is_empty() {
            return;
        }
        let row = usageseries::SeriesRow::Mark(usageseries::Mark {
            ts_ms: now,
            changed,
            fp: fp.components.clone(),
            prev,
            fp_partial: fp.partial,
        });
        if append_series_line(dir, &row).is_ok() {
            self.series_state.lock_safe().entry(group.clone()).or_default().fp =
                Some(fp.components);
        }
    }

    /// Read a group's usage series for the time plot (#2011 slice B), from
    /// `since_ms` forward.
    ///
    /// A read taking no lock on the file: it has one appender writing whole
    /// lines, so a reader racing it sees at most a torn final line, which
    /// [`usageseries::try_parse_series_lines_counted`] skips and COUNTS — the count
    /// travels on the payload as `skipped` rather than silently shortening the
    /// chart.
    ///
    /// `since_ms` filters, it does not seek: the file is read whole. That is
    /// the honest shape while the writer is append-only and unrotated, and the
    /// panel polls at 30 s rather than at the 1 s tiers, so this is not on a
    /// hot path (`docs/design/polled-views.md`).
    ///
    /// **The residual that shape leaves is BOUNDED rather than open-ended**
    /// (#2941 review). A file nothing rotates grows with the calendar, so
    /// "cheap" has a size at which it stops being true, and a residual with no
    /// number in it is one nobody can tell has been reached. That number is
    /// [`SERIES_REVISIT_BYTES`]: past it the payload carries `oversize: true`
    /// and `bytes`, which is a REPORT and not a refusal — the read still
    /// returns every row, because a chart that silently truncates its own
    /// history is worse than a slow one. It is the trigger for the work this
    /// slice deliberately does not do: seek to `since_ms` instead of filtering,
    /// or compact the file. `an_oversize_series_is_reported_not_truncated`
    /// pins both halves.
    ///
    /// **There is a hard ceiling above that report, and it is a refusal**
    /// (#3469): past [`SERIES_READ_LIMIT_BYTES`] — four times the revisit
    /// trigger — or when the allocator refuses one of the read's buffers, this
    /// returns `Null` for the tick and records `poll-read-failed` once, rather
    /// than aborting the process on `handle_alloc_error`. That write, and the
    /// latch lock it takes, are the only side effects a read has.
    ///
    /// `first_ts_ms` is the **coverage floor** — the oldest row in the file,
    /// before filtering. History starts when this build first ran against the
    /// group, and a panel that does not say so draws a flat line where there is
    /// simply no data.
    #[doc(hidden)] // pub for integration tests
    pub fn usage_series(&self, group: &GroupId, since_ms: u64) -> Value {
        let path = self.group_dir(group).join(USAGE_SERIES_FILE);
        // Measured off the file rather than off `text.len()`: the point is the
        // size on disk that the revisit trigger is stated in, and a file that
        // could not be read at all reports 0 rather than an invented figure.
        let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        // Fails soft (#3469): a file over `SERIES_READ_LIMIT_BYTES`, or a
        // buffer the allocator refuses, costs this tick — `Null`, the same
        // degrade `orch_usage_series` already returns, and the chart's next
        // 30 s poll retries — plus one `poll-read-failed` row. Missing,
        // unreadable or non-UTF-8 still read as empty, as the
        // `read_to_string(..).unwrap_or_default()` this replaced did.
        let read = match loomux_engine::boundedread::read_to_string_bounded(
            &path,
            self.poll_read_limit(SERIES_READ_LIMIT_BYTES),
        ) {
            Ok(t) => Ok(t),
            Err(e @ (loomux_engine::boundedread::BoundedReadError::TooLarge { .. }
            | loomux_engine::boundedread::BoundedReadError::Refused { .. })) => Err(e.to_string()),
            Err(_) => Ok(String::new()),
        };
        // The parsed rows and the payload rows are the typed, align-8 buffers
        // that grow with the file — the class the #3469 record names — so
        // their growth is fallible too, not only the byte buffer's.
        let parsed = read.and_then(|text| {
            let (all, skipped) = usageseries::try_parse_series_lines_counted(&text)
                .map_err(|_| "the allocator refused the parsed-row buffer".to_string())?;
            let mut rows: Vec<Value> = Vec::new();
            for r in all.iter().filter(|r| r.ts_ms() >= since_ms) {
                if let Ok(v) = serde_json::to_value(r) {
                    loomux_engine::boundedread::try_push(&mut rows, v)
                        .map_err(|_| "the allocator refused the payload-row buffer".to_string())?;
                }
            }
            Ok((all, skipped, rows))
        });
        self.note_poll_read(group, "usage-series", parsed.as_ref().map(|_| ()).map_err(String::as_str));
        let Ok((all, skipped, rows)) = parsed else { return Value::Null };
        // The MINIMUM ts, not the first row appended. Rows land in write
        // order, and `usageseries::should_sample` deliberately treats a
        // backwards clock as "elapsed" so a wall-clock correction cannot wedge
        // a key, which means the first row is not always the oldest one.
        // The floor is a claim about how far back the history goes, so it has
        // to be the oldest ts the file actually holds, or the panel prints a
        // floor later than its own data (#2941 review round 2 premortem).
        let first_ts_ms = all.iter().map(|r| r.ts_ms()).min();

        // The agent dimension the projection attributes by. Read off the
        // roster, so an agent whose pane is long gone still labels its rows.
        // The guardrails are resolved BEFORE the agents lock is taken: taking
        // the groups lock underneath it would put a second ordering edge into
        // the graph for a display-only read.
        let rails = self.group(group).map(|g| g.guardrails);
        let live: HashMap<String, (String, String)> = self
            .agents
            .lock_safe()
            .values()
            .filter(|a| &a.group == group)
            .map(|a| {
                let cli = rails
                    .as_ref()
                    .map(|g| g.cli_for_block(&a.block, a.role).to_string())
                    .unwrap_or_default();
                (a.id.clone(), (a.block.clone(), cli))
            })
            .collect();
        // A dead agent's CLI is not re-derived from its role string: that
        // reversal is a capability vocabulary (`workflow::kind_from_str`) and
        // has no arm for two of the classes. The rows themselves recorded the
        // CLI at write time, so read it from the newest row that names the
        // agent — and where nothing does, report empty rather than guess.
        let mut from_rows: HashMap<String, (String, String)> = HashMap::new();
        for r in &all {
            if let usageseries::SeriesRow::Sample(s) = r {
                from_rows.insert(s.agent.clone(), (s.block.clone(), s.cli.clone()));
            }
        }
        let agents: Vec<Value> = self
            .merged_records(group)
            .into_iter()
            .map(|r| {
                let (block, cli) = live
                    .get(&r.id)
                    .or_else(|| from_rows.get(&r.id))
                    .cloned()
                    .unwrap_or_else(|| (r.block.clone(), String::new()));
                json!({
                    "id": r.id,
                    "block": block,
                    "cli": cli,
                    "role": r.role,
                    "session": r.session,
                    "task": r.task,
                })
            })
            .collect();

        json!({
            "group": group,
            "since_ms": since_ms,
            "first_ts_ms": first_ts_ms,
            "skipped": skipped,
            "bytes": bytes,
            // A REPORT, never a truncation — see `SERIES_REVISIT_BYTES`. Every
            // row is still returned; this says the whole-file read has reached
            // the size at which it was agreed to be revisited.
            "oversize": bytes > self.series_revisit_bytes(),
            "rows": rows,
            "agents": agents,
        })
    }

    /// Aggregate the group's usage into one summary with a **live vs lifetime**
    /// split. Live agents' snapshots are refreshed from their transcripts on
    /// each call; killed/recycled agents keep the snapshot captured when they
    /// exited, so the lifetime total never forgets historical spend. Tokens are
    /// exact; dollar figures are estimates (labelled per agent).
    pub fn group_usage(&self, group: &GroupId) -> Value {
        // `ZERO` = never serve a stored value. Every existing caller (the MCP
        // `group_usage` tool, the autonomy anchor, the budget enforcer, the
        // tests) keeps exactly today's semantics; only the polled UI path opts
        // into a window. The computation still REFRESHES the memo, so a poll
        // arriving right after one of these reads is free.
        self.group_usage_within(group, Duration::ZERO)
    }

    /// [`Self::group_usage`], served from the per-group memo when the stored
    /// value is younger than `max_age` (#743 S4b). See [`Self::usage_memo`] for
    /// why the memo exists and why the per-group cell is held across the
    /// computation.
    ///
    /// **`max_age` is a bound, not a cache-forever switch**: a stored value is
    /// served only while `elapsed() < max_age`, measured on `Instant` (a
    /// monotonic clock — a wall-clock jump cannot extend the window), and
    /// `Duration::ZERO` disables serving entirely.
    #[doc(hidden)] // pub for integration tests
    pub fn group_usage_within(&self, group: &GroupId, max_age: Duration) -> Value {
        self.group_usage_memoed(group, max_age, UsageView::Full)
    }

    /// [`Self::group_usage_within`], projected to the LIVE-agent view the
    /// polled GUI reads — see [`live_usage_view`] for the shape and the
    /// argument (#1317).
    ///
    /// Shares ONE memo cell, and therefore one computation, with
    /// [`Self::group_usage_within`]: the projection is derived once per window
    /// beside the value it is derived from, so a poll never clones the
    /// whole-of-session roster just to throw the historical rows away.
    #[doc(hidden)] // pub for integration tests
    pub fn group_usage_live_within(&self, group: &GroupId, max_age: Duration) -> Value {
        self.group_usage_memoed(group, max_age, UsageView::Live)
    }

    /// The memo dance both public readers share. Spelled once: two copies of
    /// it is how the full value and its projection would come to be computed
    /// on different windows, or stored under different `Instant`s.
    fn group_usage_memoed(&self, group: &GroupId, max_age: Duration, view: UsageView) -> Value {
        // Map lock → release → per-group cell (pty.rs's rule): the outer lock is
        // held only to clone the cell's Arc out.
        let cell = {
            let mut memo = self.usage_memo.lock_safe();
            memo.entry(group.clone())
                .or_insert_with(|| Arc::new(TrackedMutex::new("usage_memo_cell", None)))
                .clone()
        };
        let mut slot = cell.lock_safe();
        if let Some((at, full, live)) = slot.as_ref() {
            if at.elapsed() < max_age {
                return match view {
                    UsageView::Full => full.clone(),
                    UsageView::Live => live.clone(),
                };
            }
        }
        // Held across the computation on purpose: a second caller arriving mid
        // computation waits and then finds the fresh value, which is what makes
        // a groupview + tabbar + orch_autonomy stampede ONE computation rather
        // than three shorter ones.
        let fresh = self.compute_group_usage(group);
        let live = live_usage_view(&fresh);
        let picked = match view {
            UsageView::Full => fresh.clone(),
            UsageView::Live => live.clone(),
        };
        *slot = Some((std::time::Instant::now(), fresh, live));
        picked
    }

    /// The uncached usage computation behind [`Self::group_usage_within`].
    fn compute_group_usage(&self, group: &GroupId) -> Value {
        let live_agents: Vec<AgentEntry> = self
            .agents
            .lock_safe()
            .values()
            .filter(|a| a.group == group && a.status != AgentStatus::Dead)
            .cloned()
            .collect();
        // Each agent's CLI is its BLOCK's (#222, corrected from per-ROLE by
        // #2167), so resolve it per agent below. The group-level `cli` in the
        // summary is the group default; two blocks of the same kind may each
        // run a different one, which is exactly the case a per-role resolution
        // got wrong.
        let rails = self.group(group).map(|g| g.guardrails);
        let cli = rails
            .as_ref()
            .map(|g| g.agent_cli.clone())
            .unwrap_or_else(|| "claude".to_string());

        // Refresh each live agent's durable snapshot from its current usage.
        // The transcript reads happen OUTSIDE the usage lock — only the merge
        // and the write are serialized (#743 S4b).
        let mut live_keys: HashSet<String> = HashSet::new();
        let mut fresh: Vec<UsageSnapshot> = Vec::with_capacity(live_agents.len());
        for a in &live_agents {
            let cli = rails.as_ref().map(|g| g.cli_for_block(&a.block, a.role)).unwrap_or("claude");
            let snap = self.compute_usage_snapshot(a, cli);
            live_keys.insert(snap.key.clone());
            fresh.push(snap);
        }

        // One merge for the whole tick, and a write only if a row's persisted
        // content moved (#3677); the returned list is the store as it now sits
        // on disk — live + historical (killed) snapshots.
        let snaps = self.merge_usage_snapshots(group, fresh, false);

        // #2011 slice B: one series row per key whose counters moved, off the
        // snapshots this tick already computed and after the merge, so a row
        // is only written for spend that persisted — except on a tick whose
        // usage store could not be read, where the merge hands back this
        // tick's own readings unsaved (#3677). Effort comes from a
        // separate bounded context-signal read scoped to this group.
        let context_signals = self.agent_context_signals_for_group(Some(group));
        self.series_sample(group, &snaps, &live_keys, &context_signals);

        let (mut live_cost, mut lifetime_cost) = (0.0f64, 0.0f64);
        let (mut live_cost_known, mut lifetime_cost_known) = (false, false);
        let (mut live_tokens, mut lifetime_tokens) = (0u64, 0u64);
        // Track whether each total mixes token-estimated and CLI-reported
        // dollars, so we never blend them under one honest label.
        let (mut live_est, mut live_rep) = (false, false);
        let (mut lifetime_est, mut lifetime_rep) = (false, false);
        let mut rows: Vec<Value> = Vec::new();

        for s in &snaps {
            // One resolver, three rungs: the block's declared TTL, then what the
            // session's own cache writes show, then the CLI's default (#3831).
            // `cache_idle_decide` calls it with the same three inputs.
            let ttl = loomux_engine::cacheage::resolve_ttl(
                rails.as_ref().and_then(|g| g.block(&s.block)).and_then(|b| b.cache_ttl_minutes),
                s.detected_cache_ttl_minutes,
                &s.cli,
            );
            let (ttl, ttl_source) = (ttl.map(|(n, _)| n), ttl.map(|(_, src)| src));
            // #3831, the next-prompt estimate's price. A row is priced off the
            // table only when its OWN dollars are (`estimated`): that is the
            // row saying its CLI bills by this table. A CLI that reports its
            // own dollars (pi, opencode) is priced by something else, and its
            // model id naming a Claude family — `anthropic/claude-opus-4-8`
            // through pi — does not change who sets the price. Read off the
            // row's provenance, never branched on a CLI's name. The CURRENT
            // model, because the next prompt goes to the model the pane is on,
            // which on claude is not always the one `model` names (#3415).
            let quote = s
                .estimated
                .then(|| s.current_model.as_deref().or(s.model.as_deref()))
                .flatten()
                .and_then(|m| crate::usage::price_quote(m).filter(|_| false).map(|q| (m, q)));
            let tokens = s.input_tokens
                + s.output_tokens
                + s.cache_creation_tokens
                + s.cache_read_tokens;
            let live = live_keys.contains(&s.key);
            lifetime_tokens += tokens;
            if let Some(c) = s.cost_usd {
                lifetime_cost += c;
                lifetime_cost_known = true;
                if s.estimated {
                    lifetime_est = true;
                } else {
                    lifetime_rep = true;
                }
            }
            if live {
                live_tokens += tokens;
                if let Some(c) = s.cost_usd {
                    live_cost += c;
                    live_cost_known = true;
                    if s.estimated {
                        live_est = true;
                    } else {
                        live_rep = true;
                    }
                }
            }
            rows.push(json!({
                "id": s.agent_id,
                "name": s.name,
                "role": s.role,
                // #2011 slice B: the block/CLI split the four capability
                // classes cannot express. Empty on a pre-field row.
                "block": s.block,
                "cli": s.cli,
                "live": live,
                "source": s.source,
                "model": s.model,
                "cost_usd": s.cost_usd,
                "estimated": s.estimated,
                // #3407: the cache-age chip's inputs. `cache_ttl_minutes` is
                // RESOLVED here (block override, else the CLI's CliCaps row) so
                // the frontend keeps no TTL table of its own; `null` = unknown.
                "last_active_ms": s.activity.last_active_ms,
                "last_wake": s.activity.last_wake,
                "cache_ttl_minutes": ttl,
                // Which rung answered: `block`, `session` or `cli`. Shown
                // beside the TTL so a reading is never mistaken for a setting.
                "cache_ttl_source": ttl_source,
                "cache_cooling_after_ms": ttl.map(loomux_engine::cacheage::cooling_after_ms),
                "compact_supported": compact_command_for(&s.cli).is_some(),
                // #3831: the next-prompt estimate's inputs, all RESOLVED here
                // so the frontend keeps no price or tokenizer table
                // (`src/promptcost.ts`, `docs/design/prompt-cost.md`).
                //
                // ONE nested object, and only on a LIVE row. The estimate is
                // about a pane's next prompt, and a row whose agent is gone
                // has none — and this row is also what the MCP `group_usage`
                // tool hands an agent ten at a time (`summarize_group_usage`),
                // mostly historical ones, so eight flat keys on every row
                // would be context every such call pays for and nothing reads.
                //
                // `context_tokens` is the context the newest turn was sent —
                // the reading this tick already made for the usage series.
                // Every field is `null` where it is not known; none is ever a
                // zero standing in for that.
                "prompt_cost": live.then(|| json!({
                    "context_tokens": context_signals.get(&s.agent_id).and_then(|c| c.tokens),
                    "first_context_tokens": s.first_context_tokens,
                    "price_model": quote.as_ref().map(|(m, _)| *m),
                    "price_per_mtok": quote.as_ref().map(|(_, q)| q.price),
                    "price_long_prompt": quote.as_ref().and_then(|(_, q)| q.long_prompt),
                    "price_basis": quote.as_ref().map(|(_, q)| q.basis),
                    "price_dated": quote.as_ref().map(|_| crate::usage::PRICE_TABLE_DATED),
                    "chars_per_token": quote.as_ref().map(|(_, q)| q.chars_per_token),
                })),
                "tokens": {
                    "input": s.input_tokens,
                    "output": s.output_tokens,
                    "cache_creation": s.cache_creation_tokens,
                    "cache_read": s.cache_read_tokens,
                    "total": tokens,
                },
            }));
        }
        rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));

        json!({
            "group": group,
            "cli": cli,
            "live_cost_usd": live_cost_known.then_some(live_cost),
            "lifetime_cost_usd": lifetime_cost_known.then_some(lifetime_cost),
            "live_cost_basis": Self::usage_cost_basis(live_est, live_rep),
            "lifetime_cost_basis": Self::usage_cost_basis(lifetime_est, lifetime_rep),
            "live_tokens": live_tokens,
            "lifetime_tokens": lifetime_tokens,
            "agents": rows,
            "note": "Tokens come from each agent's own session record — a transcript, or opencode's session row — and are exact; dollar figures are estimated from a dated model price table EXCEPT where the CLI priced them itself (opencode does), which the per-total basis labels. Subscription/Max accounts have no marginal dollar cost (the CLI statusline shows $0.00), so tokens are the reliable metric. Killed/recycled agents stay in the lifetime total; statusline-parsed dollars are a last-resort fallback.",
        })
    }

    /// How to label a dollar total: all token-estimated, all CLI-reported, or
    /// a mix; `None` when there is no cost figure at all. Shared — not just
    /// similar — between this method's own `*_cost_basis` fields and the MCP
    /// `group_usage` tool's `rest.cost_basis` (`mcp::summarize_group_usage`),
    /// so the two can never independently drift on what "mixed" means inside
    /// the same JSON object (#866 review finding 2: an earlier version
    /// duplicated this as a local closure in `mcp.rs`).
    pub(crate) fn usage_cost_basis(estimated: bool, reported: bool) -> Option<&'static str> {
        match (estimated, reported) {
            (true, true) => Some("mixed"),
            (true, false) => Some("estimated"),
            (false, true) => Some("reported"),
            (false, false) => None,
        }
    }
}
