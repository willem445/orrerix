//! Questions for the human: the `ask_human` questions an orchestrator poses
//! and the human answers or dismisses (#946), and the needs-you items a group
//! raises for the human's attention (#1151), including the demo items that
//! migrate into them, as an `impl OrchRegistry` block (#3498). The designs
//! are `docs/design/human-questions.md` and `docs/design/needs-you-items.md`.

use super::*;

impl OrchRegistry {
    // ---------- human questions (#946) ----------

    fn questions_path(&self, group: &GroupId) -> PathBuf {
        self.group_dir(group).join(humanq::QUESTIONS_FILE)
    }

    /// Read a group's question file.
    ///
    /// **Absent is empty; malformed or unreadable is LOUD** — deliberately
    /// unlike [`tasks`](Self::tasks), which collapses every failure to an empty
    /// board. Every mutation below is a read-modify-write of the whole file, so
    /// a read that answered "no questions" for a file it merely failed to parse
    /// would let the very next `ask_human` overwrite it — silently destroying
    /// pending questions a human has not answered yet, which is the one loss
    /// this registry exists to prevent. `mqloop::load_state` takes the same
    /// posture for the same reason.
    pub fn questions(&self, group: &GroupId) -> Result<Vec<humanq::Question>, String> {
        let text = match fs::read_to_string(self.questions_path(group)) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("cannot read {}: {e}", humanq::QUESTIONS_FILE)),
        };
        serde_json::from_str(&text).map_err(|e| format!("{} is malformed: {e}", humanq::QUESTIONS_FILE))
    }

    /// `list_questions`' read: pending first (oldest first — the order they
    /// should be answered in), then the newest settled rows, with the omitted
    /// count alongside so a filtered list is never mistaken for the whole one.
    pub fn question_list(&self, group: &GroupId) -> Result<(Vec<humanq::Question>, usize), String> {
        Ok(humanq::project_list(&self.questions(group)?, humanq::LIST_SETTLED_CAP))
    }

    fn write_questions(&self, group: &GroupId, questions: &[humanq::Question]) -> Result<(), String> {
        let dir = self.group_dir(group);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let body = serde_json::to_string_pretty(questions).map_err(|e| e.to_string())?;
        // Atomic replace, for #133's reason applied to a file whose loss is
        // worse than a board's: a torn questions.json is a human's outstanding
        // decisions gone. All callers hold `questions_lock`.
        atomic_write(&dir.join(humanq::QUESTIONS_FILE), body.as_bytes()).map_err(|e| e.to_string())?;
        // The single mutation point, so the single notification point — the
        // `emit_tasks_changed` shape. Slice Q2's inbox panel is the listener;
        // until then the event is inert, which is cheaper than a second visit
        // to this function later.
        if let Some(app) = self.app.lock_safe().clone() {
            let _ = app.emit("orch-questions-changed", json!({ "group_id": group }));
        }
        Ok(())
    }

    /// Register a question for the human and return it **immediately** (#946).
    ///
    /// Nothing here blocks or waits: this is a file write and an audit line.
    /// The asker gets an id back and goes on orchestrating; the answer arrives
    /// later as an `[orrerix]` notice through the ordinary delivery path. See
    /// [`humanq`]'s module doc for why the pending record lives in the engine
    /// rather than in a liaison agent's session.
    pub fn ask_human(
        &self,
        group: &GroupId,
        asker: &str,
        req: humanq::AskRequest,
    ) -> Result<humanq::Question, String> {
        let req = humanq::validate_ask(req)?;
        let question = {
            let _guard = self.questions_lock.lock_safe();
            let mut questions = self.questions(group)?;
            let pending = questions.iter().filter(|q| !q.status.is_settled()).count();
            if pending >= humanq::PENDING_MAX {
                // Actionable for BOTH callers that can reach this (#1091 slice
                // E, rev-820 NB1): `withdraw_question` is the orchestrator's
                // alone, so telling a liaison to withdraw would name a tool it
                // has not got — the same defect the success reply above was
                // corrected for. The advice is written once, for whoever reads
                // it, rather than threading a role into the registry.
                return Err(format!(
                    "{pending} questions are already pending for this group (max {}) — no human is \
                     working through a backlog that size. Clear the ones overtaken by events \
                     before asking another: withdraw_question if you hold it, otherwise name them \
                     to the orchestrator, which does",
                    humanq::PENDING_MAX
                ));
            }
            // Resolved before the struct literal moves `req` apart: both are
            // methods on the whole request, and a partial move would put them
            // out of reach.
            let select = req.select_or_default();
            let allow_free_text = req.free_text_allowed();
            let question = humanq::Question {
                id: humanq::next_id(&questions),
                asker: asker.to_string(),
                text: req.text,
                select,
                allow_free_text,
                options: req.options,
                task: req.task,
                urgency: req.urgency,
                status: humanq::Status::Pending,
                created_ms: now_ms(),
                answer: None,
                reason: None,
                settled_by: None,
                settled_ms: None,
            };
            questions.push(question.clone());
            humanq::prune(&mut questions, humanq::SETTLED_RETAINED);
            self.write_questions(group, &questions)?;
            question
        };
        self.audit(
            group,
            asker,
            "question-open",
            serde_json::to_value(&question).unwrap_or(Value::Null),
        );
        Ok(question)
    }

    /// Take back a pending question the asker no longer needs answered.
    ///
    /// Withdrawal is a settle, not a delete: the row stays, so a human who was
    /// mid-answer can see what happened to it and the audit keeps the shape of
    /// the exchange.
    pub fn withdraw_question(
        &self,
        group: &GroupId,
        actor: &str,
        id: &str,
    ) -> Result<humanq::Question, String> {
        let question = {
            let _guard = self.questions_lock.lock_safe();
            let mut questions = self.questions(group)?;
            let Some(idx) = questions.iter().position(|q| q.id == id) else {
                self.audit(group, actor, "question-reject", json!({
                    "id": id, "op": "withdraw", "reason": "unknown-question",
                }));
                return Err(format!("unknown question: {id}"));
            };
            if questions[idx].status.is_settled() {
                let status = questions[idx].status.label();
                self.audit(group, actor, "question-reject", json!({
                    "id": id, "op": "withdraw", "reason": "already-settled", "status": status,
                }));
                return Err(format!("{id} is already {status} — a settled question cannot be withdrawn"));
            }
            questions[idx].status = humanq::Status::Withdrawn;
            questions[idx].settled_by = Some(format!("withdrawn:{actor}"));
            questions[idx].settled_ms = Some(now_ms());
            let out = questions[idx].clone();
            humanq::prune(&mut questions, humanq::SETTLED_RETAINED);
            self.write_questions(group, &questions)?;
            out
        };
        self.audit(group, actor, "question-withdraw", json!({
            "id": question.id, "task": question.task, "text": question.text,
        }));
        Ok(question)
    }

    /// Settle a pending question with the human's decision, and tell the
    /// orchestrator (#946).
    ///
    /// # TRUSTED CALLERS ONLY — this is the feature's security core
    ///
    /// **No agent may ever reach this method.** An answer settles a question
    /// the *human* was asked and releases the work waiting on it; an agent that
    /// could produce one would be answering its own gate. The whole mechanism
    /// would be theatre.
    ///
    /// That is structural, not a convention: `mcp.rs`'s `call_tool` is a closed
    /// match on tool names and no arm of it reaches here, so there is no name an
    /// agent can call. Two tests hold that shut —
    /// `no_agent_token_can_answer_a_question_through_the_mcp_surface` (every
    /// tool the surface offers, dispatched, question still carrying no answer —
    /// not "still pending", since `withdraw_question` is on that surface and
    /// legitimately settles one as `withdrawn`) and
    /// `the_mcp_surface_has_no_path_to_the_answer_entry_point` (a source scan,
    /// so a future slice cannot wire one in quietly).
    ///
    /// `source` is a **closed enum supplied by the entry point**, never a
    /// caller-supplied string: `orch_question_answer` hard-codes
    /// [`humanq::AnswerSource::Webview`] rather than taking a `source`
    /// argument, so "answer as someone else" has no spelling. A new answering
    /// surface adds a variant and its own trusted entry point — never a
    /// parameter, and never an MCP tool.
    pub fn answer_question(
        &self,
        group: &GroupId,
        id: &str,
        answer: &str,
        source: humanq::AnswerSource,
    ) -> Result<humanq::Question, String> {
        let tag = source.tag();
        let answer = match humanq::validate_answer(answer) {
            Ok(a) => a,
            Err(e) => {
                self.audit(group, "human", "question-reject", json!({
                    "id": id, "source": tag, "reason": "invalid-answer", "detail": e,
                }));
                return Err(e);
            }
        };
        let question = {
            let _guard = self.questions_lock.lock_safe();
            let mut questions = self.questions(group)?;
            let Some(idx) = questions.iter().position(|q| q.id == id) else {
                // Group-scoped by construction: this read is the caller's own
                // group's file, so a question belonging to another group is
                // simply absent — the same refusal an id that never existed
                // gets, leaking nothing about the other group. Membership is
                // checked by WHICH FILE was read, not by comparing a field.
                self.audit(group, "human", "question-reject", json!({
                    "id": id, "source": tag, "reason": "unknown-question",
                }));
                return Err(format!("unknown question: {id}"));
            };
            if questions[idx].status.is_settled() {
                let status = questions[idx].status.label();
                self.audit(group, "human", "question-reject", json!({
                    "id": id, "source": tag, "reason": "already-settled", "status": status,
                }));
                return Err(format!("{id} is already {status} — a settled question cannot be re-answered"));
            }
            questions[idx].status = humanq::Status::Answered;
            questions[idx].answer = Some(answer.clone());
            questions[idx].settled_by = Some(tag.clone());
            questions[idx].settled_ms = Some(now_ms());
            let out = questions[idx].clone();
            humanq::prune(&mut questions, humanq::SETTLED_RETAINED);
            self.write_questions(group, &questions)?;
            out
        };
        // Audited before it is delivered, and both outside the lock: the
        // durable record of a human's decision must not depend on a pane
        // existing to receive it.
        self.audit(group, "human", "question-answer", json!({
            "id": question.id, "source": tag, "answer": answer,
            "task": question.task, "asker": question.asker,
        }));
        // A delivery failure — no live orchestrator, a full pane queue, a
        // restart mid-answer — never fails the answer. The question is settled
        // durably either way and a cold orchestrator finds it through
        // `list_questions`. That the registry is the record and the notice is
        // only a notification is the entire point of this design.
        let _ = self.deliver_to_orchestrator(
            group,
            &humanq::answer_notice(&question.id, &tag, &answer),
            "human",
        );
        Ok(question)
    }

    /// The human settles a pending question by saying it no longer matters,
    /// and tells the orchestrator (#2137).
    ///
    /// # TRUSTED CALLERS ONLY — the same boundary [`Self::answer_question`] holds
    ///
    /// **No agent may reach this method.** `mcp.rs`'s `call_tool` is a closed
    /// match on tool names and no arm of it reaches here, so there is no name
    /// an agent can call; `no_agent_token_can_dismiss_a_question_through_the_mcp_surface`
    /// dispatches every tool the surface offers and asserts the question is
    /// still not dismissed afterwards, and
    /// `the_mcp_surface_has_no_path_to_the_dismiss_entry_point` scans the
    /// source so a future slice cannot wire one in quietly.
    ///
    /// An agent that no longer needs its own question answered already has
    /// [`Self::withdraw_question`], which settles the row visibly as
    /// `withdrawn`. Dismissal is the HUMAN's verb, and letting an agent spell
    /// it would let the fleet clear the human's queue on the human's behalf.
    ///
    /// `source` is a **closed enum supplied by the entry point** —
    /// `orch_question_dismiss` hard-codes [`humanq::DismissSource::Webview`],
    /// so "dismiss as someone else" has no spelling. It is a SEPARATE enum
    /// from [`humanq::AnswerSource`]; see that type's doc for why answering
    /// and dismissing do not share one list.
    ///
    /// **This is not an answer, and nothing downstream may read it as one.**
    /// `answer` stays `None`, the status is its own value, and the pane notice
    /// says so in words ([`humanq::dismiss_notice`]). What the orchestrator is
    /// expected to do with it is release the hold — un-block the cited task —
    /// and NOT to infer a decision. Nothing here touches the board: which task
    /// moves, and whether the question is worth re-asking, stay the
    /// orchestrator's calls.
    pub fn dismiss_question(
        &self,
        group: &GroupId,
        id: &str,
        reason: Option<&str>,
        source: humanq::DismissSource,
    ) -> Result<humanq::Question, String> {
        let tag = source.tag();
        // Validated before the lock, and a bad reason settles nothing: the
        // audit records what was turned away rather than half-dismissing a row.
        let reason = match humanq::validate_dismiss_reason(reason) {
            Ok(r) => r,
            Err(e) => {
                self.audit(group, "human", "question-reject", json!({
                    "id": id, "source": tag, "op": "dismiss",
                    "reason": "invalid-dismiss-reason", "detail": e,
                }));
                return Err(e);
            }
        };
        let question = {
            let _guard = self.questions_lock.lock_safe();
            let mut questions = self.questions(group)?;
            let Some(idx) = questions.iter().position(|q| q.id == id) else {
                // Group-scoped by construction, exactly as answering is: this
                // read is the caller's own group's file, so a question
                // belonging to another group is simply absent. Membership is
                // checked by WHICH FILE was read, not by comparing a field.
                self.audit(group, "human", "question-reject", json!({
                    "id": id, "source": tag, "op": "dismiss", "reason": "unknown-question",
                }));
                return Err(format!("unknown question: {id}"));
            };
            // The pure transition decides, here and nowhere else — so "a
            // second settle refuses" is one rule with one test rather than a
            // condition re-spelled at each settle site.
            let next = match humanq::dismiss(questions[idx].status) {
                Ok(next) => next,
                Err(e) => {
                    self.audit(group, "human", "question-reject", json!({
                        "id": id, "source": tag, "op": "dismiss", "reason": "already-settled",
                        "status": questions[idx].status.label(),
                    }));
                    return Err(format!("{id} is {e}"));
                }
            };
            questions[idx].status = next;
            questions[idx].reason = reason.clone();
            questions[idx].settled_by = Some(tag.clone());
            questions[idx].settled_ms = Some(now_ms());
            let out = questions[idx].clone();
            humanq::prune(&mut questions, humanq::SETTLED_RETAINED);
            self.write_questions(group, &questions)?;
            out
        };
        // Audited before it is delivered, and both outside the lock: the
        // durable record must not depend on a pane existing to receive it.
        self.audit(group, "human", "question-dismiss", json!({
            "id": question.id, "source": tag, "reason": question.reason,
            "task": question.task, "asker": question.asker,
        }));
        // ALWAYS delivered, reason or no reason — unlike a note-less needs-you
        // resolve, which deliberately delivers nothing. A pending question is
        // HOLDING work: the orchestrator marked a task blocked citing `q-N`
        // and is waiting for the row to settle, so a silent dismissal would
        // leave that task blocked on a question that no longer exists.
        // Delivery failure never fails the dismissal — the row is settled
        // durably and a cold orchestrator finds it through `list_questions`.
        let _ = self.deliver_to_orchestrator(
            group,
            &humanq::dismiss_notice(&question.id, &tag, question.reason.as_deref()),
            "human",
        );
        Ok(question)
    }

    // ---------- needs-you items (#1151) ----------

    fn needs_you_path(&self, group: &GroupId) -> PathBuf {
        self.group_dir(group).join(needsyou::NEEDS_YOU_FILE)
    }

    /// Read a group's needs-you file.
    ///
    /// **Absent is empty; malformed or unreadable is LOUD** — [`Self::questions`]'
    /// posture, for its reason: every mutation below is a read-modify-write of
    /// the whole file, so a read that answered "no items" for a file it merely
    /// failed to parse would let the very next raise overwrite it, silently
    /// destroying open items a human has not looked at.
    pub fn needs_you(&self, group: &GroupId) -> Result<Vec<needsyou::Item>, String> {
        let text = match fs::read_to_string(self.needs_you_path(group)) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("cannot read {}: {e}", needsyou::NEEDS_YOU_FILE)),
        };
        serde_json::from_str(&text)
            .map_err(|e| format!("{} is malformed: {e}", needsyou::NEEDS_YOU_FILE))
    }

    /// The one write. Atomic replace for #133's reason applied to a file whose
    /// loss is a human's outstanding queue, and — being the single mutation
    /// point — the single notification point, the `write_questions`/`write_tasks`
    /// shape. All callers hold `needs_you_lock`.
    fn write_needs_you(&self, group: &GroupId, items: &[needsyou::Item]) -> Result<(), String> {
        let dir = self.group_dir(group);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let body = serde_json::to_string_pretty(items).map_err(|e| e.to_string())?;
        atomic_write(&dir.join(needsyou::NEEDS_YOU_FILE), body.as_bytes())
            .map_err(|e| e.to_string())?;
        if let Some(app) = self.app.lock_safe().clone() {
            let _ = app.emit("orch-needs-you-changed", json!({ "group_id": group }));
        }
        Ok(())
    }

    fn needs_you_cleared_path(&self, group: &GroupId) -> PathBuf {
        self.group_dir(group).join(needsyou::CLEARED_MARKER)
    }

    /// The clear-completed watermark, or `0` if this group has never cleared.
    /// Absent AND unparseable both read as 0 — see [`needsyou::parse_cleared`]
    /// for why this one fails toward showing more rather than hiding.
    pub fn needs_you_cleared_ms(&self, group: &GroupId) -> u64 {
        fs::read_to_string(self.needs_you_cleared_path(group))
            .map(|s| needsyou::parse_cleared(&s))
            .unwrap_or(0)
    }

    /// "Clear completed": stamp the watermark so the panel stops showing rows
    /// settled at or before now. Returns the stamp.
    ///
    /// **Writes no item and deletes no row.** The file is not opened here at
    /// all: that is what makes "clears the UI, persists on disk" a structural
    /// claim rather than a promise, and it is why an OPEN item can never be
    /// affected by this — the panel applies the watermark only to settled rows,
    /// and there is nothing here that could touch an unsettled one even if it
    /// did not.
    ///
    /// Under [`Self::marker_io`], `set_notify`'s shape: two clears racing must
    /// not land their file writes in the opposite order to the stamps they
    /// minted, or the earlier watermark would survive the later one and rows the
    /// human just cleared would come back.
    pub fn clear_needs_you(&self, group: &GroupId) -> Result<u64, String> {
        let _io = self.marker_io.lock_safe();
        let dir = self.group_dir(group);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let stamp = now_ms();
        atomic_write(&dir.join(needsyou::CLEARED_MARKER), stamp.to_string().as_bytes())
            .map_err(|e| e.to_string())?;
        self.audit(group, "human", "needs-you-clear", json!({ "cleared_ms": stamp }));
        Ok(stamp)
    }

    /// The webview's read: every item plus the watermark, in one round trip.
    ///
    /// **A pure read — it writes nothing.** It used to run the upgrade migration
    /// inline, which was wrong twice over (rev-lead round 1): it re-raised rows
    /// the human had just resolved, and it made a `viewer`-tier command write a
    /// file, emit an event and grow the audit log on a poll. The migration is now
    /// a once-ever step at group load — see [`Self::migrate_demo_items`].
    pub fn needs_you_view(&self, group: &GroupId) -> Result<needsyou::View, String> {
        Ok(needsyou::View {
            items: self.needs_you(group)?,
            cleared_ms: self.needs_you_cleared_ms(group),
        })
    }

    /// [`Self::needs_you_view`] plus the board rows its OPEN items name — the
    /// whole of what the webview panel reads (#1317).
    ///
    /// **Why the join moved here.** The panel used to fetch the WHOLE board
    /// alongside this, every tick and on every `orch-tasks-changed`, and used
    /// it at exactly one site: `linkTask` looks up the row an open item names
    /// and projects six fields off it. So it held a second full copy of a
    /// board that is mostly history — the other half of #1317's item 2 — to
    /// answer a handful of point lookups. `items` is bounded by
    /// [`needsyou::OPEN_MAX`] and each item names at most one row, so this is
    /// bounded by the human's own open queue rather than by session length.
    ///
    /// **OPEN items only, deliberately.** The settled tail never joins the
    /// board (see the frontend's `projectPanel`): a resolved row renders from
    /// the item's own record. Sending rows for it would put the board's growth
    /// back on this read through the retained-resolved cap.
    ///
    /// **Still a pure read.** It reads one more file than it did; it writes
    /// nothing, takes no lock, and stays inside the `viewer` tier's
    /// definition. See the command's doc.
    pub fn needs_you_read(&self, group: &GroupId) -> Result<NeedsYouRead, String> {
        let view = self.needs_you_view(group)?;
        let wanted: HashSet<&str> = view
            .items
            .iter()
            .filter(|i| !i.status.is_resolved())
            .filter_map(|i| i.task.as_deref())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect();
        // No notes: the panel projects six identity/status fields off a joined
        // row and renders no conversation (#1317).
        let tasks: Vec<BoardTask> = if wanted.is_empty() {
            Vec::new()
        } else {
            self.tasks(group)
                .into_iter()
                .filter(|t| wanted.contains(t.id.as_str()))
                .map(|t| board_task(t, false))
                .collect()
        };
        Ok(NeedsYouRead { view, tasks })
    }

    /// An agent-facing list: open items first, then the newest resolved rows up
    /// to the cap, with the omitted count alongside so a filtered list is never
    /// mistaken for the whole one. [`Self::question_list`]'s shape, and a pure
    /// read for [`Self::needs_you_view`]'s reason.
    ///
    /// Returns [`needsyou::AgentItem`]s rather than stored rows — the projection
    /// is the return type so that a field added to `Item` cannot reach an agent
    /// surface just by existing.
    pub fn needs_you_list(
        &self,
        group: &GroupId,
    ) -> Result<(Vec<needsyou::AgentItem>, usize), String> {
        Ok(needsyou::project_list(&self.needs_you(group)?, needsyou::LIST_RESOLVED_CAP))
    }

    /// Register something for the human to look at, and return it immediately.
    ///
    /// Nothing here blocks: a file write and an audit line. Raising a `demo` for
    /// a task that already has one open **returns the existing item** rather than
    /// a second row — the dedupe that lets the board hook and an explicit raise
    /// coexist without duplicating the human's queue (see [`needsyou::admit`],
    /// which is where that decision lives for all three callers).
    /// Returns [`needsyou::Raised`], not a bare item: a caller with an author to
    /// answer must be able to tell a fresh registration from a dedupe, because a
    /// dedupe keeps the EXISTING row's text and drops the new ask's.
    pub fn raise_needs_you(
        &self,
        group: &GroupId,
        raiser: &str,
        req: needsyou::RaiseRequest,
    ) -> Result<needsyou::Raised, String> {
        let raised = {
            let _guard = self.needs_you_lock.lock_safe();
            let mut items = self.needs_you(group)?;
            // `OpenEpisode`: a settled row is a CLOSED episode, so a task that
            // re-enters the gate gets a new row. Only the one-shot migration
            // uses `EverRaised` — see `needsyou::Dedupe`.
            let raised =
                needsyou::admit(&mut items, raiser, req, now_ms(), needsyou::Dedupe::OpenEpisode)?;
            if raised.fresh {
                needsyou::prune(&mut items, needsyou::RESOLVED_RETAINED);
                self.write_needs_you(group, &items)?;
            }
            raised
        };
        if raised.fresh {
            self.audit(
                group,
                raiser,
                "needs-you-open",
                serde_json::to_value(&raised.item).unwrap_or(Value::Null),
            );
        }
        Ok(raised)
    }

    /// Take back an item the raiser no longer needs a human to look at.
    ///
    /// Withdrawal is a settle, not a delete: the row stays so a human who was
    /// mid-look can see what happened to it, and `resolved_by` records
    /// `withdrawn:<agent>` so it is never mistaken for a human's acknowledgement.
    pub fn withdraw_needs_you(
        &self,
        group: &GroupId,
        actor: &str,
        id: &str,
    ) -> Result<needsyou::Item, String> {
        let item = {
            let _guard = self.needs_you_lock.lock_safe();
            let mut items = self.needs_you(group)?;
            let Some(idx) = items.iter().position(|i| i.id == id) else {
                self.audit(group, actor, "needs-you-reject", json!({
                    "id": id, "op": "withdraw", "reason": "unknown-item",
                }));
                return Err(format!("unknown needs-you item: {id}"));
            };
            if items[idx].status.is_resolved() {
                self.audit(group, actor, "needs-you-reject", json!({
                    "id": id, "op": "withdraw", "reason": "already-resolved",
                    "resolved_by": items[idx].resolved_by,
                }));
                return Err(format!("{id} is already resolved — it cannot be withdrawn"));
            }
            items[idx].status = needsyou::Status::Resolved;
            items[idx].resolved_by = Some(format!("withdrawn:{actor}"));
            items[idx].resolved_ms = Some(now_ms());
            let out = items[idx].clone();
            needsyou::prune(&mut items, needsyou::RESOLVED_RETAINED);
            self.write_needs_you(group, &items)?;
            out
        };
        self.audit(group, actor, "needs-you-withdraw", json!({
            "id": item.id, "kind": item.kind.label(), "task": item.task, "text": item.text,
        }));
        Ok(item)
    }

    /// The human closes out an item, and (with a note) tells the orchestrator.
    ///
    /// # TRUSTED CALLERS ONLY
    ///
    /// **No agent may reach this method.** Resolving is the human clearing their
    /// own attention queue — the same no-self-served-gate boundary
    /// [`Self::answer_question`] documents, and the reason no MCP tool reaches
    /// here. An agent that wanted its own ask gone has
    /// [`Self::withdraw_needs_you`], which settles it visibly as a withdrawal.
    ///
    /// `source` is a **closed enum supplied by the entry point**, never a
    /// caller-supplied string: `orch_needs_you_resolve` hard-codes
    /// [`needsyou::ResolveSource::Webview`], so "resolve as the human" has no
    /// spelling. A new trusted surface adds a variant and its own entry point —
    /// never a parameter, and never an MCP tool.
    ///
    /// **Resolving does NOT move the linked task.** It clears the attention row;
    /// the board keeps whatever status it had, and Proceed / Request-changes stay
    /// the board actions they always were.
    pub fn resolve_needs_you(
        &self,
        group: &GroupId,
        id: &str,
        note: Option<&str>,
        source: needsyou::ResolveSource,
    ) -> Result<needsyou::Item, String> {
        let tag = source.tag();
        // **This method settles a LOOK, so it refuses a dismissal's source**
        // (#2137, review round 2 B1). Both gestures take a `ResolveSource` by
        // value and each hard-codes its own tag, audit action and delivery
        // rule; without this, `resolve_needs_you(…, WebviewDismiss)` would
        // write `resolved_by: "dismissed:webview"` — read downstream as *the
        // human did not look* — while auditing `needs-you-resolve` and, with
        // no note, delivering nothing at all. Unreachable from today's one
        // caller, which hard-codes `Webview`; the guard is what keeps
        // `is_dismissal`'s "the two cannot disagree" a fact rather than an
        // intention.
        if source.is_dismissal() {
            self.audit(group, "human", "needs-you-reject", json!({
                "id": id, "source": tag, "op": "resolve", "reason": "wrong-source",
            }));
            return Err(format!(
                "{tag} is a dismissal source — resolving records that the human LOOKED, so it \
                 cannot be settled with it; call dismiss_needs_you instead"
            ));
        }
        // Validated before the lock, and a bad note settles nothing: the audit
        // records what was turned away rather than half-resolving a row.
        let note = match note.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => match needsyou::validate_resolution(n) {
                Ok(n) => Some(n),
                Err(e) => {
                    self.audit(group, "human", "needs-you-reject", json!({
                        "id": id, "source": tag, "op": "resolve",
                        "reason": "invalid-resolution", "detail": e,
                    }));
                    return Err(e);
                }
            },
            None => None,
        };
        let item = {
            let _guard = self.needs_you_lock.lock_safe();
            let mut items = self.needs_you(group)?;
            let Some(idx) = items.iter().position(|i| i.id == id) else {
                // Group-scoped by construction: this read is the caller's own
                // group's file, so an item belonging to another group is simply
                // absent — the same refusal an id that never existed gets,
                // leaking nothing about the other group. Membership is checked
                // by WHICH FILE was read, not by comparing a field.
                self.audit(group, "human", "needs-you-reject", json!({
                    "id": id, "source": tag, "op": "resolve", "reason": "unknown-item",
                }));
                return Err(format!("unknown needs-you item: {id}"));
            };
            if items[idx].status.is_resolved() {
                self.audit(group, "human", "needs-you-reject", json!({
                    "id": id, "source": tag, "op": "resolve", "reason": "already-resolved",
                    "resolved_by": items[idx].resolved_by,
                }));
                return Err(format!("{id} is already resolved — it cannot be resolved again"));
            }
            items[idx].status = needsyou::Status::Resolved;
            items[idx].resolved_by = Some(tag.clone());
            items[idx].resolved_ms = Some(now_ms());
            items[idx].resolution = note.clone();
            let out = items[idx].clone();
            needsyou::prune(&mut items, needsyou::RESOLVED_RETAINED);
            self.write_needs_you(group, &items)?;
            out
        };
        // Audited before it is delivered, and both outside the lock: the durable
        // record of a human's close-out must not depend on a pane existing to
        // receive it.
        self.audit(group, "human", "needs-you-resolve", json!({
            "id": item.id, "source": tag, "kind": item.kind.label(),
            "task": item.task, "raiser": item.raiser, "resolution": item.resolution,
        }));
        // A note is the only thing worth a pane notice: a note-less resolve is
        // the human tidying their own queue, and one delivery per tidy is noise
        // the orchestrator pays for. A delivery failure — no live orchestrator, a
        // full queue, a restart mid-click — never fails the resolve; the item is
        // settled durably either way and a cold orchestrator finds it through
        // the list. The registry is the record, the notice only a notification.
        if let Some(note) = item.resolution.as_deref() {
            let _ = self.deliver_to_orchestrator(
                group,
                &needsyou::resolve_notice(&item.id, item.task.as_deref(), note),
                "human",
            );
        }
        Ok(item)
    }

    /// The human clears an item by saying it no longer matters (#2137).
    ///
    /// # TRUSTED CALLERS ONLY — [`Self::resolve_needs_you`]'s boundary
    ///
    /// **No agent may reach this method**, for that method's reason: clearing
    /// the human's own attention queue is the human's gesture. An agent whose
    /// ask went stale has [`Self::withdraw_needs_you`], which settles the row
    /// visibly as a withdrawal. `source` is the same closed enum supplied by
    /// the entry point — `orch_needs_you_dismiss` hard-codes
    /// [`needsyou::ResolveSource::WebviewDismiss`] — so there is no spelling
    /// for "dismiss as the human".
    ///
    /// # Why this is a separate method and not a flag on `resolve_needs_you`
    ///
    /// Three things differ, and each of the three is a decision rather than a
    /// parameter: the tag written to `resolved_by` (`dismissed:webview`, which
    /// is what makes the fact readable a week later), the audit action
    /// (`needs-you-dismiss`), and — the one that actually matters —
    /// **whether a pane notice is delivered at all**. A note-less resolve
    /// deliberately delivers nothing, because it is a human tidying a queue
    /// after looking and one delivery per tidy is noise. A dismissal is always
    /// delivered, with or without a reason, because it says the ask was not
    /// worth opening: that is news to the agent that raised it, and the only
    /// signal it will ever get.
    ///
    /// **Like resolving, it does NOT move the linked task.** The row leaves
    /// the panel; the board keeps whatever status it had.
    pub fn dismiss_needs_you(
        &self,
        group: &GroupId,
        id: &str,
        reason: Option<&str>,
        source: needsyou::ResolveSource,
    ) -> Result<needsyou::Item, String> {
        let tag = source.tag();
        // The mirror of `resolve_needs_you`'s guard, and needed for the same
        // reason in the other direction: settling a DISMISSAL with a resolve's
        // source would write `resolved_by: "webview"` — read downstream as *the
        // human looked* — while auditing `needs-you-dismiss` and delivering the
        // dismissal notice. Both directions, so the pair cannot disagree
        // whichever way a future caller gets it wrong.
        if !source.is_dismissal() {
            self.audit(group, "human", "needs-you-reject", json!({
                "id": id, "source": tag, "op": "dismiss", "reason": "wrong-source",
            }));
            return Err(format!(
                "{tag} is not a dismissal source — dismissing records that the ask no longer \
                 matters, so it cannot be settled with it; call resolve_needs_you instead"
            ));
        }
        // Validated before the lock, and a bad reason settles nothing — the
        // audit records what was turned away rather than half-dismissing a row.
        let reason = match needsyou::validate_dismiss_reason(reason) {
            Ok(r) => r,
            Err(e) => {
                self.audit(group, "human", "needs-you-reject", json!({
                    "id": id, "source": tag, "op": "dismiss",
                    "reason": "invalid-dismiss-reason", "detail": e,
                }));
                return Err(e);
            }
        };
        let item = {
            let _guard = self.needs_you_lock.lock_safe();
            let mut items = self.needs_you(group)?;
            let Some(idx) = items.iter().position(|i| i.id == id) else {
                // Group-scoped by construction — see `resolve_needs_you`.
                self.audit(group, "human", "needs-you-reject", json!({
                    "id": id, "source": tag, "op": "dismiss", "reason": "unknown-item",
                }));
                return Err(format!("unknown needs-you item: {id}"));
            };
            // The pure transition decides, here and nowhere else.
            let next = match needsyou::dismiss(items[idx].status) {
                Ok(next) => next,
                Err(e) => {
                    self.audit(group, "human", "needs-you-reject", json!({
                        "id": id, "source": tag, "op": "dismiss", "reason": "already-resolved",
                        "resolved_by": items[idx].resolved_by,
                    }));
                    return Err(format!("{id} is {e}"));
                }
            };
            items[idx].status = next;
            items[idx].resolved_by = Some(tag.clone());
            items[idx].resolved_ms = Some(now_ms());
            // The REASON rides in `resolution`, and `resolved_by` is what says
            // which of the two it is. A second field would be a second slot
            // meaning "the human's words about this close", and the panel's
            // settled tail would then have to know which one to render.
            items[idx].resolution = reason.clone();
            let out = items[idx].clone();
            needsyou::prune(&mut items, needsyou::RESOLVED_RETAINED);
            self.write_needs_you(group, &items)?;
            out
        };
        self.audit(group, "human", "needs-you-dismiss", json!({
            "id": item.id, "source": tag, "kind": item.kind.label(),
            "task": item.task, "raiser": item.raiser, "reason": item.resolution,
        }));
        // ALWAYS delivered — see this method's doc for why this differs from a
        // note-less resolve. A delivery failure never fails the dismissal.
        let _ = self.deliver_to_orchestrator(
            group,
            &needsyou::dismiss_notice(&item.id, item.task.as_deref(), item.resolution.as_deref()),
            "human",
        );
        Ok(item)
    }

    /// **The one-shot upgrade migration**: synthesize the demo items a board
    /// deserved before this registry existed. Runs at most once per group, ever.
    ///
    /// # Why it exists
    ///
    /// Auto-raise hangs off the status TRANSITION in `upsert_task`, and a board
    /// holding demo-gated rows at the moment this ships has already made those
    /// transitions — so without this, every in-flight demo silently vanishes
    /// from the panel on the release that adds the panel's own record. The rows
    /// may then sit untouched for days with nothing to trigger a raise.
    ///
    /// # Why it is a migration and not a reconciliation
    ///
    /// This is the shape rev-lead's round-1 blocking finding was about, and the
    /// distinction is the fix. As a *reconciliation* — "make the items agree
    /// with the board, on every read" — it was actively wrong: a human resolving
    /// a demo item deliberately does NOT move the task, so the task is still
    /// demo-gated one refresh later, and the next read minted a replacement row
    /// under a new id. The human's close-out came back, and again on every
    /// subsequent resolve. The same held for an agent's withdrawal.
    ///
    /// As a *migration* it is well defined: it answers "did this group exist
    /// before the registry did", which is asked once and never changes. Two
    /// things enforce that, and both are needed:
    ///
    /// 1. **The [`needsyou::MIGRATED_MARKER`]**, written even when nothing was
    ///    added. Without it, every resume would re-run and re-raise.
    /// 2. **[`needsyou::Dedupe::EverRaised`]**, so that even a first run treats a
    ///    *settled* demo row as proof the task is already accounted for. A raise
    ///    uses `OpenEpisode` instead, because for a raise a settled row is a
    ///    closed episode and a re-parked task deserves a new one.
    ///
    /// # Where it runs
    ///
    /// At group load (`create_group_ex`'s marker re-seed), beside the pause,
    /// notify and autonomy markers it resembles — **never on a read**. That
    /// keeps `orch_needs_you_list` a genuine `viewer`-tier command: on the
    /// remote engine a peer holding only read rights must not be able to drive
    /// a file write, an event emission and audit growth by polling.
    ///
    /// Best-effort per row against the cap: a board with more parked tasks than
    /// [`needsyou::OPEN_MAX`] migrates what fits and audits the refusal rather
    /// than failing the group load that triggered it. The marker is still
    /// written — a partial migration is the answer for this board, and retrying
    /// it on every resume would be the reconciliation this is not.
    pub(in crate::orchestration) fn migrate_demo_items(&self, group: &GroupId) {
        let dir = self.group_dir(group);
        if dir.join(needsyou::MIGRATED_MARKER).is_file() {
            return;
        }
        // The board read is outside the lock and lock-free itself (`tasks` does
        // not take `tasks_lock`), which is what keeps the documented nesting
        // one-directional: `tasks_lock` → `needs_you_lock`, never the reverse.
        let parked: Vec<Task> = self
            .tasks(group)
            .into_iter()
            .filter(|t| is_demo_gated(&t.status))
            .collect();
        let (added, refused) = {
            // The marker is re-checked and written inside this guard, so two
            // resumes racing on one group cannot both decide the migration is
            // outstanding and both raise.
            let _guard = self.needs_you_lock.lock_safe();
            if dir.join(needsyou::MIGRATED_MARKER).is_file() {
                return;
            }
            let mut added: Vec<needsyou::Item> = Vec::new();
            let mut refused: Vec<(String, String)> = Vec::new();
            if !parked.is_empty() {
                let mut items = match self.needs_you(group) {
                    Ok(items) => items,
                    Err(e) => {
                        // Leave the marker UNwritten: an unreadable file is the
                        // one case where retrying next resume is right, because
                        // nothing was decided and nothing was lost.
                        self.audit(group, "board", "needs-you-reject", json!({
                            "op": "migrate", "reason": "unreadable", "detail": e,
                        }));
                        return;
                    }
                };
                for task in &parked {
                    let req = needsyou::RaiseRequest::demo_for(&task.id, &task.title, &task.status);
                    match needsyou::admit(
                        &mut items,
                        "board",
                        req,
                        now_ms(),
                        needsyou::Dedupe::EverRaised,
                    ) {
                        Ok(raised) if raised.fresh => added.push(raised.item),
                        Ok(_) => {}
                        Err(e) => refused.push((task.id.clone(), e)),
                    }
                }
                if !added.is_empty() {
                    needsyou::prune(&mut items, needsyou::RESOLVED_RETAINED);
                    if let Err(e) = self.write_needs_you(group, &items) {
                        self.audit(group, "board", "needs-you-reject", json!({
                            "op": "migrate", "reason": "unwritable", "detail": e,
                        }));
                        return;
                    }
                }
            }
            // Written last, and written even for an empty board: from here on
            // "already considered" and "found nothing to do" are the same answer.
            if fs::create_dir_all(&dir).is_ok() {
                let _ = atomic_write(&dir.join(needsyou::MIGRATED_MARKER), b"");
            }
            (added, refused)
        };
        for item in &added {
            self.audit(
                group,
                "board",
                "needs-you-open",
                serde_json::to_value(item).unwrap_or(Value::Null),
            );
        }
        for (task, detail) in &refused {
            self.audit(group, "board", "needs-you-reject", json!({
                "op": "migrate", "task": task, "reason": "raise-refused", "detail": detail,
            }));
        }
    }

    /// Map a board status transition onto the task's demo item (#1151).
    ///
    /// Called from `upsert_task` **with `tasks_lock` still held** — see
    /// [`Self::needs_you_lock`]'s doc for why the nesting is deliberate rather
    /// than an oversight.
    ///
    /// Keyed on the TRANSITION, not on the write: a `request_changes` note or an
    /// assignee edit on a task that is already parked crosses no boundary and
    /// therefore changes nothing here, which is the behaviour that keeps one
    /// human-visible row per parking rather than one per board edit.
    ///
    /// Best-effort on both sides. The board write has already landed and cannot
    /// be unwound, so a failure here is audited as a refusal rather than turned
    /// into an error that would make a successful board move look failed.
    pub(in crate::orchestration) fn sync_demo_item(&self, group: &GroupId, task: &Task, prev_status: &str) {
        let was = is_demo_gated(prev_status);
        let now = is_demo_gated(&task.status);
        if was == now {
            return;
        }
        if now {
            let req = needsyou::RaiseRequest::demo_for(&task.id, &task.title, &task.status);
            if let Err(e) = self.raise_needs_you(group, "board", req) {
                self.audit(group, "board", "needs-you-reject", json!({
                    "op": "auto-raise", "task": task.id, "status": task.status,
                    "reason": "raise-refused", "detail": e,
                }));
            }
            return;
        }
        // Left the gate: the ask is moot, so the item settles as the BOARD's
        // doing. `board:<new-status>` rather than a human's tag, because nobody
        // acknowledged anything — the work simply moved on.
        let settled = {
            let _guard = self.needs_you_lock.lock_safe();
            let mut items = match self.needs_you(group) {
                Ok(items) => items,
                Err(e) => {
                    self.audit(group, "board", "needs-you-reject", json!({
                        "op": "auto-resolve", "task": task.id, "reason": "unreadable", "detail": e,
                    }));
                    return;
                }
            };
            // EVERY open demo row for this task, not the first one found.
            // `admit`'s dedupe makes one the normal case and a second one
            // unreachable through any code path — but a hand-edited file can
            // hold two, and settling one of a pair would leave the other on the
            // human's queue for a task that has moved on, forever, with nothing
            // left to trigger a resolve. The board is the authority on whether
            // the ask is still live; it answers for all of them.
            let now = now_ms();
            let mut settled: Vec<needsyou::Item> = Vec::new();
            for item in items.iter_mut().filter(|i| i.is_open_demo_for(&task.id)) {
                item.status = needsyou::Status::Resolved;
                item.resolved_by = Some(format!("board:{}", task.status));
                item.resolved_ms = Some(now);
                settled.push(item.clone());
            }
            if settled.is_empty() {
                return;
            }
            needsyou::prune(&mut items, needsyou::RESOLVED_RETAINED);
            if let Err(e) = self.write_needs_you(group, &items) {
                self.audit(group, "board", "needs-you-reject", json!({
                    "op": "auto-resolve", "task": task.id, "reason": "unwritable", "detail": e,
                }));
                return;
            }
            settled
        };
        for item in &settled {
            self.audit(group, "board", "needs-you-resolve", json!({
                "id": item.id, "source": item.resolved_by, "kind": item.kind.label(),
                "task": item.task, "raiser": item.raiser,
            }));
        }
    }
}
