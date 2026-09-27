//! The manager mailbox (#1161 M2): its file, reading and writing it,
//! posting to the manager and checking mail, as an `impl OrchRegistry` block
//! (#3498). The design is `docs/design/manager.md`.

use super::*;

impl OrchRegistry {
    // ---------- the manager mailbox (#1161 M2) ----------

    fn mailbox_path(&self, group: &GroupId) -> PathBuf {
        self.group_dir(group).join(mailbox::MAILBOX_FILE)
    }

    /// The group's declared manager block, if it has one.
    ///
    /// Read off the group's own resolved roster (`Guardrails::block_for`)
    /// rather than by re-parsing `workflow.yml` — `lock_resources`' shape is
    /// right for declared RESOURCES, which live only in the file, and wrong for
    /// blocks, which the group already carries resolved. A second read of the
    /// file could disagree with the roster the group is actually running.
    ///
    /// `block_for` answers "the first block of that kind", which for a manager
    /// is "the only one": `workflow::MANAGER_MAX` is 1 and a second is a parse
    /// error (#1169).
    ///
    /// A default group can never answer `Some` here: `Role::Manager` is
    /// workflow-only and `builtin_roster` has no manager block (#1169).
    pub fn manager_block(&self, group: &GroupId) -> Option<workflow::Block> {
        self.group(group)?.guardrails.block_for(Role::Manager).cloned()
    }

    /// Read a group's mailbox file.
    ///
    /// **Absent is empty; malformed or unreadable is LOUD** — `questions`'
    /// posture, for its reason applied to this file: every mutation below is a
    /// read-modify-write of the whole file, so a read that answered "no mail"
    /// for a file it merely failed to parse would let the very next
    /// `message_manager` overwrite it, silently destroying status the human has
    /// not read. That is the one loss this registry exists to prevent, so it is
    /// the one failure mode that must never be quiet.
    ///
    /// Deliberately unlike [`tasks`](Self::tasks), which collapses every failure
    /// to an empty board because a board is re-derivable and a human is looking
    /// at it.
    pub fn mailbox(&self, group: &GroupId) -> Result<Vec<mailbox::Message>, String> {
        let text = match fs::read_to_string(self.mailbox_path(group)) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("cannot read {}: {e}", mailbox::MAILBOX_FILE)),
        };
        serde_json::from_str(&text)
            .map_err(|e| format!("{} is malformed: {e}", mailbox::MAILBOX_FILE))
    }

    /// How many messages the manager has not read — what the pane's unread chip
    /// shows and what `orch_mailbox_status` returns.
    ///
    /// A read that fails answers 0 rather than erroring: this is chrome, and a
    /// momentarily unreadable file should not make a badge into a dialog. Every
    /// path that MUTATES the file goes through [`mailbox`](Self::mailbox)
    /// directly and keeps its loud posture.
    pub fn mailbox_unread(&self, group: &GroupId) -> usize {
        self.mailbox(group).map(|m| mailbox::unread_count(&m)).unwrap_or(0)
    }

    fn write_mailbox(&self, group: &GroupId, messages: &[mailbox::Message]) -> Result<(), String> {
        let dir = self.group_dir(group);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let body = serde_json::to_string_pretty(messages).map_err(|e| e.to_string())?;
        // Atomic replace (#133), for `write_questions`' reason: a torn
        // mailbox.json is the human's status stream gone.
        atomic_write(&dir.join(mailbox::MAILBOX_FILE), body.as_bytes())
            .map_err(|e| e.to_string())?;
        // The single mutation point, so the single notification point — the
        // `emit_tasks_changed` / `orch-questions-changed` shape. M5's unread
        // chip is the listener, and it has landed: `orchestration.ts` subscribes
        // and routes each push through `mailboxPanes` to the one manager pane in
        // the group.
        if let Some(app) = self.app.lock_safe().clone() {
            let _ = app.emit(
                "orch-mailbox-changed",
                json!({ "group_id": group, "unread": mailbox::unread_count(messages) }),
            );
        }
        Ok(())
    }

    /// Post a message into the manager's mailbox (#1161 M2).
    ///
    /// Nothing here delivers anything: this is a file write and an audit line.
    /// **That is the feature, not a limitation** — a manager pane takes no
    /// mid-session delivery at all (`deliver_prompt` refuses it), so a mailbox
    /// row is read when the manager next takes a turn, which is when its human
    /// next speaks to it.
    ///
    /// `from` and `kind` are supplied by the caller's own resolved identity and
    /// a closed-set parse; only `text` is authored, and it is sanitized and
    /// bounded by `mailbox::validate_post` before it is stored.
    pub fn post_to_manager(
        &self,
        group: &GroupId,
        from: &str,
        text: &str,
        kind: mailbox::Kind,
    ) -> Result<mailbox::Message, String> {
        // Refused BEFORE the file is touched: a mailbox in a group with no
        // manager is a write nobody will ever read, and absorbing it silently
        // is how an orchestrator ends up believing it briefed a human who does
        // not exist. `mcp.rs` also omits the tool from the listing for such a
        // group — this is the dispatch half of that double gate (#243).
        if self.manager_block(group).is_none() {
            self.audit(group, from, "mail-reject", json!({ "reason": "no-manager-block" }));
            return Err(
                "this group declares no manager block, so there is no mailbox to post to — a \
                 manager is declared in the repo's workflow.yml (kind: manager). Put it to the \
                 human with ask_human or request_attention instead."
                    .into(),
            );
        }
        let text = mailbox::validate_post(text)?;
        let message = {
            let _guard = self.mailbox_lock.lock_safe();
            let mut messages = self.mailbox(group)?;
            let unread = mailbox::unread_count(&messages);
            if unread >= mailbox::UNREAD_MAX {
                // Refuse the WRITER; never evict an unread row to make room.
                // See `mailbox::UNREAD_MAX` for why that asymmetry is the whole
                // point of the cap.
                self.audit(group, from, "mail-reject", json!({
                    "reason": "unread-cap", "unread": unread,
                }));
                return Err(format!(
                    "{unread} messages are already unread in this group's mailbox (max {}) — the \
                     manager has not taken a turn since they were posted, which means its human \
                     has been away. Nothing is dropped to make room: say it in your own pane, or \
                     raise it where the human will see it away from the keyboard (ask_human, \
                     request_attention)",
                    mailbox::UNREAD_MAX
                ));
            }
            let message = mailbox::Message {
                id: mailbox::next_id(&messages),
                from: from.to_string(),
                kind,
                text,
                created_ms: now_ms(),
                read_ms: None,
            };
            messages.push(message.clone());
            mailbox::prune(&mut messages, mailbox::READ_RETAINED);
            self.write_mailbox(group, &messages)?;
            message
        };
        self.audit(
            group,
            from,
            "mail-post",
            serde_json::to_value(&message).unwrap_or(Value::Null),
        );
        Ok(message)
    }

    /// The manager's consuming read: return what is waiting and stamp it read.
    ///
    /// `include_read` returns the retained read rows too and stamps nothing —
    /// see `mailbox::project_check` for why that escape hatch exists (a session
    /// that dies between the stamp and the sentence has marked the human's
    /// status read without the human having seen it) and why it deliberately
    /// cannot un-stamp anything.
    ///
    /// The projection is taken from the SAME vector the stamp is applied to,
    /// under one guard, so what is returned is exactly what was marked read.
    /// Reading and stamping through two separate calls would let a post landing
    /// between them be stamped read without ever being returned — a message
    /// silently consumed by nobody, which is the failure this whole registry is
    /// built to make impossible.
    pub fn check_mail(
        &self,
        group: &GroupId,
        actor: &str,
        include_read: bool,
    ) -> Result<(Vec<mailbox::Message>, usize), String> {
        let (out, omitted, stamped) = {
            let _guard = self.mailbox_lock.lock_safe();
            let mut messages = self.mailbox(group)?;
            let (out, omitted) = mailbox::project_check(&messages, include_read);
            let stamped = if include_read {
                // A re-read is a re-read: it consumes nothing, so it writes
                // nothing. Skipping the write also keeps a manager that is
                // re-reading after a compact from churning the file (and the
                // chip event) for no state change.
                0
            } else {
                let stamped = mailbox::mark_all_read(&mut messages, now_ms());
                if stamped > 0 {
                    mailbox::prune(&mut messages, mailbox::READ_RETAINED);
                    self.write_mailbox(group, &messages)?;
                }
                stamped
            };
            (out, omitted, stamped)
        };
        if stamped > 0 {
            self.audit(group, actor, "mail-read", json!({
                "read": stamped,
                "ids": out.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            }));
        }
        Ok((out, omitted))
    }
}
