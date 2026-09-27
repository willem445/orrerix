//! The audit log and the records of what an agent said: appending
//! (`audit`), reading it back whole or windowed (`audit_log*`), the MCP
//! poll-read limit, the state-write note and the directive ledger
//! (`note_directive`), as an `impl OrchRegistry` block (#3498). The designs
//! are `docs/design/orchestration.md` and
//! `docs/design/crash-observability.md`.

use super::*;

impl OrchRegistry {
    // ---------- audit ----------

    /// Append one JSON line to the group's audit log. Best-effort: auditing
    /// must never take the orchestration down.
    pub fn audit(&self, group: &GroupId, actor: &str, action: &str, detail: Value) {
        append_audit(&self.root, group, actor, action, detail);
    }

    /// Read a group's audit timeline for the in-app viewer, oldest first.
    /// Reads the rotated generation (`audit.1.jsonl`) before the current one
    /// so a rotation doesn't drop history mid-session, then keeps only the
    /// most recent `AUDIT_VIEW_LIMIT` entries. Missing files read as empty.
    ///
    /// Unreadable lines are still skipped — a log with a torn record must not
    /// blank the viewer — but they are no longer skipped *silently*: the count
    /// goes to the breadcrumb log (#240). A non-zero count now means a writer
    /// is not appending whole lines, which is a bug worth seeing rather than a
    /// timeline that quietly comes up short.
    pub fn audit_log(&self, group: &GroupId) -> Vec<AuditEntry> {
        self.audit_log_windowed(group).0
    }

    /// `audit_log`, plus **whether the `AUDIT_VIEW_LIMIT` window actually cut
    /// anything** (#579 review NB1).
    ///
    /// The viewer never needed this: a timeline showing the most recent 5000
    /// entries is what it is for. A *derivation* does, and the difference is
    /// not cosmetic. `front_door_refusals` counts refusals in what it was
    /// handed and reports `refused_omitted` as the difference between that
    /// count and what it listed — so on a truncated read it would report
    /// `refused_omitted: 0` while older refusals had been cut away before it
    /// ever saw them: a capped list reading as complete, which is the exact
    /// failure mode #579's own design note names and `.loomux/lessons.md`
    /// catalogues.
    ///
    /// Reported by the reader that KNOWS, rather than inferred downstream from
    /// `entries.len() == AUDIT_VIEW_LIMIT`: a log holding exactly the cap was
    /// not truncated, and a derivation that called that "truncated" would cry
    /// wolf on a boundary it has no way to resolve. Same job as #569's
    /// `PauseSuppression::window_start_seen` — say when the scan ran off the
    /// start of its own timeline — with an exact signal available here because
    /// this is where the cut happens.
    ///
    /// **Fails soft (#3469).** This is polled — by the viewer in follow mode
    /// and by the derivations on their edges — so a read the allocator refuses
    /// must cost one tick, not the process. [`Self::try_audit_log_windowed`]
    /// is the fallible read; here its `Err` is reported once
    /// (`poll-read-failed`, see [`Self::note_poll_read`]) and degrades to
    /// **an empty window marked truncated**. `truncated: true` is not a
    /// fudge: it is this function's existing way of saying "history exists
    /// that this answer did not see", which is exactly true, so the two
    /// derivations that honour the flag (`front_door_refusals`,
    /// `refusal_roster`) read the degrade as a partial window rather than as
    /// "nothing was ever refused".
    ///
    /// **Only those two.** [`Self::audit_log`] drops the flag, so its three
    /// callers see the degrade as a plain EMPTY timeline (#3493 review N1):
    /// `resume_group`'s pause-suppression notice is computed from nothing and
    /// not re-sent (a one-shot edge, so that report is lost for the episode);
    /// `audit_derived_orphans` reports no audit-derived orphans (the snapshot
    /// half of `queue_orphans` is unaffected); and the `orch_audit` viewer
    /// shows an empty log for the tick. The `poll-read-failed` row is the
    /// record in all three. That is the same flag-dropping those callers
    /// already did at the 5000-entry cut, now reached on a failed read too.
    pub fn audit_log_windowed(&self, group: &GroupId) -> (Vec<AuditEntry>, bool) {
        let read = self.try_audit_log_windowed(group);
        self.note_poll_read(group, "audit", read.as_ref().map(|_| ()).map_err(String::as_str));
        read.unwrap_or_else(|_| (Vec::new(), true))
    }

    /// The fallible audit-window read (#3469). `Err` only for the two outcomes
    /// that are new with the bounded reader — a generation over
    /// [`AUDIT_READ_LIMIT_BYTES`], or the allocator refusing the buffer or the
    /// window — never for the ones the pre-#3469 read already tolerated: a
    /// generation that is missing, unreadable or not UTF-8 is skipped exactly
    /// as it was.
    ///
    /// Two allocation shapes changed, not only the one that became fallible.
    /// The generations are parsed **one at a time** rather than concatenated
    /// into one `String` (which at a full `audit.1.jsonl` plus a busy
    /// `audit.jsonl` was a ~13 MB buffer grown by an infallible `push_str`),
    /// and parsed entries go into a window **bounded at
    /// `AUDIT_VIEW_LIMIT`**, grown fallibly and never past that cap, instead of a `Vec` of
    /// every entry in both files trimmed afterwards. The answer is identical —
    /// same entries, same order, `truncated` true exactly when more than
    /// `AUDIT_VIEW_LIMIT` parsed — and the peak is the window, not the log.
    #[doc(hidden)] // pub for integration tests
    pub fn try_audit_log_windowed(&self, group: &GroupId) -> Result<(Vec<AuditEntry>, bool), String> {
        use loomux_engine::boundedread::{read_to_string_bounded, BoundedReadError};
        let dir = self.group_dir(group);
        let limit = self.poll_read_limit(AUDIT_READ_LIMIT_BYTES);
        // Grown on demand and capped at the limit (#3493 review N2): a small
        // log costs a small window, and a full one never holds more than
        // `AUDIT_VIEW_LIMIT` slots — rather than 5001 x 88 B reserved on every
        // poll whatever the log's size.
        let mut window: VecDeque<AuditEntry> = VecDeque::new();
        let mut truncated = false;
        let mut skipped = 0usize;
        for name in ["audit.1.jsonl", "audit.jsonl"] {
            let text = match read_to_string_bounded(&dir.join(name), limit) {
                Ok(t) => t,
                Err(e @ (BoundedReadError::TooLarge { .. } | BoundedReadError::Refused { .. })) => {
                    return Err(format!("{name}: {e}"));
                }
                // Missing, unreadable or not UTF-8: skipped, as the
                // `if let Ok(..) = fs::read_to_string` this replaced skipped it.
                Err(_) => continue,
            };
            for line in text.lines() {
                match parse_audit_line(line) {
                    None => {}
                    Some(Err(())) => skipped += 1,
                    Some(Ok(entry)) => {
                        if window.len() == AUDIT_VIEW_LIMIT {
                            window.pop_front();
                            truncated = true;
                        }
                        loomux_engine::boundedread::try_grow_capped(&mut window, AUDIT_VIEW_LIMIT)
                            .map_err(|_| format!("the allocator refused to grow the audit window past {} entries", window.len()))?;
                        window.push_back(entry); // within the capacity just ensured
                    }
                }
            }
        }
        if skipped > 0 && self.audit_skips_notified.lock_safe().insert(group.clone(), skipped) != Some(skipped) {
            // Only on a change: follow mode re-polls this, and a pre-fix log
            // keeps its torn lines forever (see `audit_skips_notified`).
            crate::obs::breadcrumb("audit-lines-unreadable", &format!("group={group} skipped={skipped}"));
        }
        // `Vec::from(VecDeque)` reuses the deque's buffer — no second window.
        Ok((Vec::from(window), truncated))
    }

    /// Report a poll-path read's outcome (#3469): the FIRST failure of a
    /// reader for a group writes a breadcrumb and a `poll-read-failed` audit
    /// row; later failures of the same reader are silent until one succeeds
    /// and re-arms it. Latched because the readers are polled (the viewer's
    /// follow mode, the chart's 30 s tick) and a row per poll would bury the
    /// log it is reporting on — `audit_skips_notified`'s reason, applied here.
    ///
    /// The audit append itself is one small line, so reporting a refusal does
    /// not repeat the large request that was refused.
    pub(in crate::orchestration) fn note_poll_read(&self, group: &GroupId, reader: &'static str, outcome: Result<(), &str>) {
        let key = (group.clone(), reader);
        match outcome {
            Ok(()) => {
                self.poll_read_failed.lock_safe().remove(&key);
            }
            Err(e) => {
                // Lock released before `audit` takes `AUDIT_LOCK`: this latch
                // orders against nothing.
                let first = self.poll_read_failed.lock_safe().insert(key);
                if first {
                    crate::obs::breadcrumb("poll-read-failed", &format!("group={group} reader={reader} {e}"));
                    self.audit(group, brand::AUDIT_ACTOR, "poll-read-failed", json!({ "reader": reader, "error": e }));
                }
            }
        }
    }

    /// The read ceiling in force for a poll-path reader, honouring the test
    /// seam (#3469).
    pub(in crate::orchestration) fn poll_read_limit(&self, default: u64) -> u64 {
        self.poll_read_limit_override.lock_safe().unwrap_or(default)
    }

    /// Lower every poll-path read ceiling (#3469) so a test can drive the
    /// refusal path through the reader's own limit — never by exhausting
    /// memory. Test-only seam (see `poll_read_limit_override`).
    #[doc(hidden)]
    pub fn set_poll_read_limit(&self, bytes: Option<u64>) {
        *self.poll_read_limit_override.lock_safe() = bytes;
    }

    /// Compact-nudge (#328): stamp the calling agent's `last_state_write_ms` —
    /// self-scoped sign-of-life backing `request_compact`'s offload-checklist
    /// warning. Called from the `set_state` MCP handler.
    pub fn note_state_write(&self, agent_id: &str) {
        if let Some(a) = self.agents.lock_safe().get_mut(agent_id) {
            a.last_state_write_ms = now_ms();
        }
    }

    /// Directive ledger (#329 expansion): self-scoped append (default) or
    /// full-rewrite (`replace: true`) of the CALLING agent's own ledger — the
    /// diary a human directive, scope decision, or piece of feedback is
    /// recorded into at RECEIPT time, precisely because the CLI's own
    /// emergency auto-compact (see `auto_compact_banner_detected`) gives no
    /// warning turn to backfill one from memory afterward. Mirrors
    /// `request_compact`'s self-scoping: the token that resolves to
    /// `agent_id` is the entire trust surface, so there is no `group_id`-style
    /// path segment and no cross-pane power — an agent can only ever touch
    /// its own ledger file (`ledger_path`).
    ///
    /// `replace: true` is how an agent CURATES the ledger — typically right
    /// after the post-compact re-injection has just shown it its own tail
    /// verbatim — dropping entries that are done or no longer relevant rather
    /// than letting it grow forever; `text` in that mode is the whole
    /// replacement ledger, not one more line. A plain append (the default)
    /// never loses a prior entry to a curation mistake in this code, only
    /// ever to the calling agent's own judgment on a later `replace`.
    ///
    /// Each append is stamped with `now_ms()` for a human skimming the file
    /// directly (matching `audit.jsonl`'s `ts_ms` convention). Append-mode
    /// `text` is sanitized with `notify::sanitize_gh_text` (rev review N1) —
    /// the exact function `channel_send` already runs untrusted text through
    /// — before it's written: strips control characters (so an embedded `\n`
    /// can't split one call into several physical lines, which would break
    /// `directive_ledger_embed`'s one-line-per-entry model) and neutralizes
    /// `[`/`]` (so a line can never start with a forged `[orrerix]` marker
    /// once re-embedded verbatim in the post-compact notice). Low severity —
    /// the ledger is self-authored and self-scoped, so an agent can only ever
    /// spoof itself — but free to close the same way `channel_send` already
    /// does. `replace` writes `text` verbatim, UNSANITIZED: it is the
    /// curation path, expected to contain the agent's own prior (already-
    /// sanitized) entries copied back in, not fresh untrusted input, and a
    /// replacement without a trailing newline gets one added so a later
    /// append can never land glued onto its last line.
    ///
    /// After either write, the STORED file is capped at
    /// `DIRECTIVE_LEDGER_MAX_BYTES` (rev review N2) via `ledger_capped` —
    /// oldest entries dropped first, never silently: a non-zero drop count is
    /// audited (`ledger-trimmed`) and named in the response string, so a
    /// human or the calling agent can see it happened. Curation via
    /// `replace: true` is still the primary, deliberate mechanism; this is
    /// only the backstop for a session that never uses it.
    ///
    /// Audited like any other durable write (#240): the text itself is not
    /// duplicated into the audit log (the ledger file already holds it in
    /// full), only the fact and shape of the write.
    pub fn note_directive(&self, agent_id: &str, text: &str, replace: bool) -> Result<String, String> {
        let a = self.agent(agent_id).ok_or("unknown agent")?;
        let text = text.trim();
        if text.is_empty() {
            return Err("text must not be empty".into());
        }
        // #925: `note_directive` is an MCP tool, so this id crossed a caller
        // boundary. It has a real error channel, so it gets a real refusal.
        let agent_seg = PathSegment::parse(agent_id)
            .map_err(|e| format!("invalid agent id {agent_id:?}: {e}"))?;
        let path = self.ledger_path(&a.group, &agent_seg);
        if replace {
            let mut body = text.to_string();
            if !body.ends_with('\n') {
                body.push('\n');
            }
            atomic_write(&path, body.as_bytes()).map_err(|e| e.to_string())?;
        } else {
            let sanitized = notify::sanitize_gh_text(text, DIRECTIVE_ENTRY_MAX_CHARS);
            append_ledger_line(&path, &format!("[{}] {sanitized}", now_ms())).map_err(|e| e.to_string())?;
        }
        self.audit(&a.group, agent_id, "note-directive", json!({ "replace": replace, "bytes": text.len() }));

        let mut cap_note = String::new();
        if let Ok(current) = fs::read_to_string(&path) {
            let (capped, dropped) = ledger_capped(&current, DIRECTIVE_LEDGER_MAX_BYTES);
            if dropped > 0 {
                atomic_write(&path, capped.as_bytes()).map_err(|e| e.to_string())?;
                self.audit(&a.group, agent_id, "ledger-trimmed", json!({ "dropped": dropped }));
                cap_note = format!(
                    " ({dropped} older entries dropped — ledger exceeded {DIRECTIVE_LEDGER_MAX_BYTES} bytes; curate with replace to control this yourself)"
                );
            }
        }
        Ok(if replace {
            format!("ledger replaced{cap_note}")
        } else {
            format!("directive recorded{cap_note}")
        })
    }
}
