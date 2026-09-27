//! The task board: reading and writing the group's board (`tasks`,
//! `write_tasks`, the `upsert_task*`/`delete_task*` family), its status
//! transitions (`start_task`, `approve_task`, `request_changes`, …), the
//! WIP limits it paces work with (`board_policy`, `wip_rows`,
//! `notify_wip_crossing`), and the progress notes a delegate's report leaves
//! on its row (`report_task_note`), as an `impl OrchRegistry` block (#3498).
//! The board is `docs/design/task-hierarchy.md`; WIP limits are
//! `docs/design/board-wip.md`.

use super::*;

impl OrchRegistry {
    // ---------- task board ----------

    pub fn tasks(&self, group: &GroupId) -> Vec<Task> {
        self.tasks_or_err(group).unwrap_or_default()
    }

    /// The board, with a read or parse failure kept DISTINCT from an empty board
    /// (#1966 rev-final N2). `tasks()` is this function with the distinction
    /// thrown away, which is right for every caller that has nothing different to
    /// do about it — and wrong for the one that would otherwise tell a delegate
    /// "there was no matching row" when the truth is "I could not look".
    ///
    /// **A MISSING file is not a failure**: a group that has never written a board
    /// has none, and that is the ordinary empty case. Anything else — an
    /// unreadable file, a partial or malformed one — is an error.
    fn tasks_or_err(&self, group: &GroupId) -> Result<Vec<Task>, String> {
        match fs::read_to_string(self.group_dir(group).join("tasks.json")) {
            // NOTE: neither message names the board FILE, and that is not
            // stylistic — `tests/pathseg.rs` default-denies an interpolation
            // sharing a `format!` template with a file-extension literal, and a
            // `SANCTIONED` entry is only for a binding that really is a
            // `PathSegment`. Writing the argument down is the point of that
            // test, and there is no argument to write here.
            Ok(s) => serde_json::from_str(&s).map_err(|e| format!("the task board could not be parsed: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("the task board could not be read: {e}")),
        }
    }

    /// Compact rows for the MCP `list_tasks` tool (#245) — see `TaskSummary`.
    /// Projected board-at-a-time, not task-at-a-time, because `ready` (#582)
    /// is a property of a task plus its dependencies' statuses. Unfiltered:
    /// callers that need the done-row cap applied use
    /// `task_summaries_for_list_tasks` instead — this one stays the plain
    /// full-board projection existing callers (tests included) already rely
    /// on returning every row.
    pub fn task_summaries(&self, group: &GroupId) -> Vec<TaskSummary> {
        board_summaries(&self.tasks(group))
    }

    /// The MCP `list_tasks` tool's actual read path (#865): full board when
    /// `include_all`, every non-`done` row when `hot_only` (#1684 — the
    /// per-wake re-sync wants no done rows at all, cap or not, with the
    /// dropped count still reported so a hot board is never mistaken for the
    /// whole one), else `done` rows capped at `LIST_TASKS_DONE_CAP` —
    /// newest by `updated_ms` — with the omitted count returned alongside so
    /// the caller can say so rather than silently truncating. The hot arm IS
    /// `filter_done_rows` at a cap of 0 (one keep/drop rule, not two that can
    /// drift); the include_all/hot_only contradiction is refused one layer
    /// up, at the dispatch that parses the flags.
    pub fn task_summaries_for_list_tasks(&self, group: &GroupId, include_all: bool, hot_only: bool) -> (Vec<TaskSummary>, usize) {
        let rows = self.task_summaries(group);
        if include_all {
            (rows, 0)
        } else if hot_only {
            // A cap of 0 keeps nothing done and omits the lot — exactly the
            // hot read, with filter_done_rows's arithmetic already pinned by
            // its own unit tests above and below the cap.
            filter_done_rows(rows, 0)
        } else {
            filter_done_rows(rows, LIST_TASKS_DONE_CAP)
        }
    }
    /// The group's derived current sprint (#1272) — see `current_sprint`.
    ///
    /// Reads the WHOLE board deliberately, not the rows `list_tasks` is about
    /// to return: those are capped for done rows (`LIST_TASKS_DONE_CAP`), and
    /// deriving a board-level answer from a deliberately partial view is the
    /// kind of coupling that is correct today and silently wrong the first time
    /// the cap changes what it drops.
    pub fn current_sprint_for(&self, group: &GroupId) -> Option<u32> {
        current_sprint(&self.tasks(group))
    }

    /// One full task (including its capped note history) by id — the detail
    /// view `list_tasks`'s compact rows point at (#245).
    pub fn get_task(&self, group: &GroupId, id: &str) -> Option<Task> {
        self.tasks(group).into_iter().find(|t| t.id == id)
    }

    pub(in crate::orchestration) fn write_tasks(&self, group: &GroupId, tasks: &[Task]) -> Result<(), String> {
        let dir = self.group_dir(group);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        // Atomic replace: the incident that filed #133 was a disk-full
        // `fs::write` here that truncated tasks.json and destroyed the live
        // board. All callers hold `tasks_lock`, so writes are serialized.
        let body = serde_json::to_string_pretty(tasks).unwrap();
        atomic_write(&dir.join("tasks.json"), body.as_bytes()).map_err(|e| e.to_string())?;
        self.emit_tasks_changed(group);
        Ok(())
    }

    fn emit_tasks_changed(&self, group: &GroupId) {
        if let Some(app) = self.app.lock_safe().clone() {
            let _ = app.emit("orch-tasks-changed", json!({ "group_id": group }));
        }
    }

    /// An **agent-origin** board write — the MCP `upsert_task` tool's path, and
    /// the default for anything that does not say otherwise. See
    /// [`WriteOrigin`] for why the strict posture is the unnamed one, and
    /// [`Self::upsert_task_from`] for what the write actually does.
    pub fn upsert_task(
        &self,
        group: &GroupId,
        actor: &str,
        id: Option<&str>,
        patch: TaskPatch,
    ) -> Result<Task, String> {
        self.upsert_task_from(group, WriteOrigin::Agent, actor, id, patch)
    }

    /// A **human-origin** board write — the board pane's own edits, and the
    /// registry actions that pane drives (approve, request changes, proceed).
    ///
    /// The only thing this changes is that a WIP limit can warn but never
    /// refuse (#1175): the board's authority is the human's, not a queue
    /// discipline — the same reason `claim` is deliberately not exposed on the
    /// human's board command — and a limit a human declared for their agents
    /// must not bounce the human who declared it.
    pub fn upsert_task_by_human(
        &self,
        group: &GroupId,
        actor: &str,
        id: Option<&str>,
        patch: TaskPatch,
    ) -> Result<Task, String> {
        self.upsert_task_from(group, WriteOrigin::Human, actor, id, patch)
    }

    /// Create (id = None, title required) or edit a task. Notes append; all
    /// other patch fields replace. Returns the resulting task.
    ///
    /// Link edits and a `claim` (#582) are validated against the WHOLE board
    /// before any field is written, and nothing is persisted until
    /// `write_tasks` at the bottom — so every rejection below leaves the board
    /// exactly as it was, inside the one `tasks_lock` this method already held.
    /// The WIP check (#1175) is the last of those validations and obeys the
    /// same rule: a refused entry writes nothing at all.
    pub fn upsert_task_from(
        &self,
        group: &GroupId,
        origin: WriteOrigin,
        actor: &str,
        id: Option<&str>,
        patch: TaskPatch,
    ) -> Result<Task, String> {
        if let Some(s) = patch.status.as_deref() {
            if !TASK_STATUSES.contains(&s) {
                return Err(format!("invalid status {s:?} — use one of {}", TASK_STATUSES.join(" | ")));
            }
        }
        // The level is validated against the closed VOCABULARY here, like the
        // status (#958) — where it sits on the ladder is judged further down,
        // inside the lock, because that needs the whole board (#1156). The
        // EMPTY string is not an invalid kind, it is the clear — the `pr` rule
        // — so it has to pass here to reach the apply below.
        if let Some(k) = patch.kind.as_deref().map(str::trim) {
            if !k.is_empty() && !TASK_KINDS.contains(&k) {
                return Err(format!("invalid kind {k:?} — use one of {}", TASK_KINDS.join(" | ")));
            }
        }
        // The description is validated HERE, beside the status and kind checks
        // and before the lock, for the reason the whole method obeys: nothing
        // is written until `write_tasks` at the bottom, and a refusal that has
        // not touched the board is the cheapest kind (#3261).
        //
        // It is checked in CHARACTERS rather than bytes, because the cap is a
        // promise to a human counting sentences: a byte cap would refuse a
        // 260-character description written in an em-dash-and-accents style
        // this repo writes in constantly, while accepting 500 ASCII ones.
        //
        // Control characters are refused rather than stripped, for the reason
        // the over-length case is refused rather than cut: a caller who pasted
        // a three-line paragraph should be told the field is one, not have two
        // of the lines silently welded together. `	` is included — the board
        // paints this as a single line, where a tab is invisible width.
        if let Some(d) = patch.description.as_deref().map(str::trim) {
            let n = d.chars().count();
            if n > MAX_TASK_DESCRIPTION {
                return Err(format!(
                    "description is {n} characters — at most {MAX_TASK_DESCRIPTION}. It is one or two sentences saying what the row IS; put the detail in a note or a grounding link."
                ));
            }
            if let Some(c) = d.chars().find(|c| c.is_control()) {
                return Err(format!(
                    "description must be one line of plain text — it carries the control character {c:?}. Put anything that needs its own paragraph in a note."
                ));
            }
        }
        // NO sprint check sits here, beside the status and kind ones, and that
        // is deliberate rather than an omission (#1272). Nothing is left to
        // check by the time a patch reaches this method:
        //
        //  - negatives and fractions cannot be represented — `TaskPatch::sprint`
        //    is `Option<u32>`, and the wire parsers refuse them before that
        //    (`arg_sprint`'s `as_u64` in mcp.rs; serde's own `u32` decode on the
        //    human command). A caller that typo'd gets an error naming the
        //    shape, not a silent no-op;
        //  - 0 is the one representable value that is not a legal sprint, and it
        //    is not invalid — it is the CLEAR, the numeric counterpart of the
        //    empty string on `pr`/`kind`. It is consumed by the `.filter` at the
        //    apply below, which is the only place that needs to know.
        //
        // Nothing else about a sprint is validated, here or anywhere: sprint
        // numbers need not be contiguous, need not start at 1, and a row may
        // sit in a sprint far ahead of every other. `current_sprint` derives
        // from whatever the rows say, so there is no board-level invariant to
        // keep and no ordering the board could be wrong about.
        // Same shape as the claim check below, and for the same reason: a row
        // this call is CREATING has no prior arrays to have changed under
        // anybody, so a token can only ever be a caller mistake. Refuse it where
        // the caller can see it rather than ignoring an argument they passed on
        // purpose (#1349).
        if patch.expect_link_etag.is_some() && id.is_none() {
            return Err(
                "expect_link_etag guards an EXISTING task's links against a concurrent edit — \
                 a task being created has none. Drop it, or pass the id you meant to edit."
                    .into(),
            );
        }
        if patch.claim {
            // A claim is a guarded transition on an EXISTING row: the guards
            // (queued, unclaimed-or-mine, deps met) are the whole point, and a
            // task being created has none of that state to guard.
            if id.is_none() {
                return Err("claim needs the id of an existing task".into());
            }
            // Claiming IS the status transition. Silently overriding a
            // conflicting `status` in the same call would make one of the two
            // arguments a lie, so say so instead.
            if let Some(s) = patch.status.as_deref() {
                if s != "in-progress" {
                    return Err(format!(
                        "claim sets status to in-progress — it cannot also set status {s:?}"
                    ));
                }
            }
        }
        // Read the board policy BEFORE taking `tasks_lock`, the way `with_locks`
        // reads `lock_resources` before taking the lock table (#1175): this is a
        // `workflow.yml` open + YAML parse, and `tasks_lock` is a process-global
        // mutex serializing every group's board write, so the read does not
        // belong inside it.
        //
        // Skipped outright for a write that cannot move any count —
        // `wip_may_change` is the predicate, and it is pure and pinned rather
        // than inlined here, because "a note-only write does not read the
        // workflow file" is a cost claim this PR makes out loud. That is the
        // hot path (a note append, a `pr` ref), and it stays exactly as
        // expensive as it was before this feature existed.
        let board = if wip_may_change(id.is_none(), &patch) {
            self.board_policy(group)
        } else {
            workflow::BoardPolicy::default()
        };

        let guard = self.tasks_lock.lock_safe();
        let mut tasks = self.tasks(group);
        // ---- WIP limits (#1175): the counts on the board this write STARTED
        // from — taken here, above the `idx` resolution, because that is where a
        // NEW row is pushed, and a row this write is adding is not part of the
        // board it started from (rev-2 B4).
        //
        // Tallied below the push, `before` and `after` both counted the new row
        // in its born status, so they agreed and `wip_breaches` skipped the
        // status: a `queued:` cap could never fire on task creation, silently,
        // in either posture. It is the same class as the B1 defect it came in
        // with — one input from the pre-write board, another from a board
        // already half-mutated — and the insertion IS part of the write.
        //
        // Nothing here needs `idx` or `this_id`, so the hoist keeps one rule
        // rather than subtracting the new row back out in the caller's head,
        // which is the shape `wip_occupants` dropped its `skip` parameter to
        // avoid. The judgement itself still happens after the apply, against
        // the post-write board — see `wip_breaches`.
        let before_wip = wip_counts(&board, &tasks);
        let idx = match id {
            Some(id) => tasks
                .iter()
                .position(|t| t.id == id)
                .ok_or_else(|| format!("unknown task: {id}"))?,
            None => {
                let title = patch
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .ok_or("a new task needs a title")?;
                // The level this row is being CREATED with picks its id prefix
                // (#1156) — already validated against `TASK_KINDS` above, and
                // absent (a plain row) mints `t-` exactly as it always has.
                // `kind` itself is applied further down by the generic patch
                // application, not here, so there is one place that writes it.
                let born_kind = patch.kind.as_deref().map(str::trim).filter(|k| !k.is_empty());
                tasks.push(Task {
                    id: next_task_id(born_kind, &tasks),
                    title: title.to_string(),
                    status: "queued".into(),
                    issue: None,
                    pr: None,
                    pr_base: None,
                    assignee: None,
                    session: None,
                    notes: vec![],
                    deps: vec![],
                    related: vec![],
                    parent: None,
                    kind: None,
                    // Born in the backlog with no grounding (#1272/#1273); both
                    // are applied below by the generic patch application, so a
                    // create-with-sprint is one code path with an update.
                    sprint: None,
                    links: vec![],
                    demo_path: None,
                    cleared_ms: None,
                    // Born with no description (#3261); applied below by the
                    // generic patch application, so a create-with-description
                    // is one code path with an update.
                    description: None,
                    updated_ms: 0,
                });
                tasks.len() - 1
            }
        };
        let this_id = tasks[idx].id.clone();
        // ---- the stale-snapshot guard (#1349), FIRST among the checks that
        // read this row, and before any of them can waste work on a write that
        // is about to be refused. The board this write is judged against is the
        // one under `tasks_lock`, so a token that matches here cannot go stale
        // before `write_tasks` at the bottom.
        //
        // The failure it closes: the human board paints a row's links, an agent
        // adds one through MCP, and the human clicks ✕ on entry 0. Composed from
        // the RENDERED array, that click sends a two-element list with no trace
        // of the agent's third link — a whole-array replace that silently
        // discards a write nobody has seen. It is also what makes the board's
        // remove-BY-INDEX sound: an index only names what the human clicked
        // while the array it was read from is still the array being written.
        if let Some(expected) = patch.expect_link_etag.as_deref() {
            let actual = link_etag(&tasks[idx]);
            if expected != actual {
                return Err(format!(
                    "{STALE_LINK_ETAG_PREFIX}: {this_id}'s deps/related/links changed since you \
                     read them (you sent link_etag {expected}, the board has {actual}). Nothing \
                     was written — re-read the task and re-apply your edit to the current list."
                ));
            }
        }
        // Read before any field is applied, because the demo-gate hook at the
        // bottom keys on the TRANSITION rather than on the resulting status
        // (#1151). A row created by this very call reads `queued` here — which is
        // the right answer: creating a task straight into `prototype` IS an entry
        // into the gate, and must raise the item a later flip into it would.
        let prev_status = tasks[idx].status.clone();
        // ---- links (#582): normalize + existence-check against the board,
        // then reject a write that would close a dependency cycle.
        let deps = patch.deps.map(|v| normalize_links(v, &this_id, &tasks, "deps")).transpose()?;
        let related = patch.related.map(|v| normalize_links(v, &this_id, &tasks, "related")).transpose()?;
        // ---- grounding links (#1273): shape + caps only, plus the misuse
        // guard that refuses a target naming a live board task. Validated in
        // the same pre-write phase as the #582 arrays above, so a refusal here
        // leaves the board exactly as it was.
        let links = patch.links.map(|v| normalize_task_links(v, &tasks, "links")).transpose()?;
        if let Some(new_deps) = deps.as_ref() {
            let mut edges: HashMap<String, Vec<String>> =
                tasks.iter().map(|t| (t.id.clone(), t.deps.clone())).collect();
            edges.insert(this_id.clone(), new_deps.clone());
            if let Some(cycle) = find_dep_cycle(&this_id, &edges) {
                return Err(format!("deps: dependency cycle {}", cycle.join(" → ")));
            }
        }
        // ---- hierarchy (#958): the link contract above, applied to
        // containment. Same placement and same reason — every check reads the
        // board BEFORE the mutable borrow below, so a refusal leaves the board
        // exactly as it was. `Some(None)` is the explicit clear; the outer
        // `None` means the patch never mentioned `parent` at all.
        let parent: Option<Option<String>> = match patch.parent.as_deref().map(str::trim) {
            None => None,
            Some("") => Some(None),
            Some(p) => {
                if p == this_id {
                    return Err(format!("parent: a task cannot contain itself ({this_id})"));
                }
                if !tasks.iter().any(|t| t.id == p) {
                    return Err(format!("parent: unknown task: {p}"));
                }
                let mut parent_of: HashMap<&str, &str> = tasks
                    .iter()
                    .filter_map(|t| t.parent.as_deref().map(|par| (t.id.as_str(), par)))
                    .collect();
                parent_of.insert(&this_id, p);
                // Reparenting a row under its OWN DESCENDANT is the cycle case,
                // and it is the one an agent actually writes by accident.
                let chain = find_parent_cycle(&this_id, &parent_of)
                    .map_err(|cycle| format!("parent: hierarchy cycle {}", cycle.join(" → ")))?;
                // The cap is on the DEEPEST resulting row, not on the mover:
                // its new chain, plus whatever it carries below it. `- 1`
                // because the mover is counted by both.
                let deepest = chain.len() + subtree_height(&this_id, &tasks) - 1;
                if deepest > MAX_TASK_DEPTH {
                    return Err(format!(
                        "parent: hierarchy depth cap is {MAX_TASK_DEPTH} — putting {this_id} under {p} \
                         would make its deepest row {deepest} levels deep"
                    ));
                }
                Some(Some(p.to_string()))
            }
        };
        // A link to your own container is a mistake, and this refuses the write
        // that MAKES one — scoped, deliberately, to the fields this call is
        // actually setting (rev-611 NB2).
        //
        // The wider reading (check both link arrays whenever any of the three
        // moves) refused writes that had not created the problem and named a
        // field the caller never touched. It has a reachable trigger that is not
        // a hand-edit: a row may legitimately dep on its GRANDparent, and
        // `promote_orphans` turns that grandparent into its parent when the
        // middle row is deleted. From then on the wider check refused even a
        // `related`-only patch, citing `deps`.
        //
        // Promotion is deliberately NOT made to strip that link instead. A
        // delete of one row would then change a DIFFERENT row's readiness while
        // its actual blocker is still alive and unfinished — silently unblocking
        // work, which is the failure direction #582 exists to prevent, and a far
        // bigger claim than the overlap is worth. `strip_deleted_links` unblocks
        // only by removing links to rows that are GONE. So the residual state
        // (container also named in `deps`) is tolerated on read like every other
        // hierarchy oddity, and only a write that re-asserts it is refused.
        let effective_parent = match parent.as_ref() {
            Some(p) => p.as_deref(),
            None => tasks[idx].parent.as_deref(),
        };
        if let Some(p) = effective_parent {
            // On a `parent` write, both arrays are in scope: the write is what
            // moves the container under an existing link. On a link write, only
            // the array being written is.
            let writing_parent = patch.parent.is_some();
            for (field, links) in [
                ("deps", if writing_parent { Some(deps.as_ref().unwrap_or(&tasks[idx].deps)) } else { deps.as_ref() }),
                ("related", if writing_parent { Some(related.as_ref().unwrap_or(&tasks[idx].related)) } else { related.as_ref() }),
            ] {
                let Some(links) = links else { continue };
                if links.iter().any(|id| id.as_str() == p) {
                    return Err(format!(
                        "parent: {p} is this task's container — it cannot also be a {field} link"
                    ));
                }
            }
        }
        // ---- the strict Agile ladder (#1156). Same placement and same reason
        // as everything above it: read-only against the board, so a refusal
        // leaves it exactly as it was.
        //
        // TRIGGERED BY THE WRITE THAT ASSERTS THE SHAPE, never by the row's
        // mere existence. A patch touching neither `kind` nor `parent` is not
        // judged at all, which is the whole compatibility story: every
        // pre-#1156 board holds shapes this ladder refuses (a top-level
        // `feature` and its slices was the DOMINANT one — #958 §2 said so), and
        // those rows must stay editable for status, notes, assignee, deps and
        // everything else forever. The narrower rule also matches the one this
        // method already applies to the container/link overlap directly above:
        // a residual shape is tolerated, and only a write that RE-ASSERTS it is
        // refused.
        if patch.kind.is_some() || patch.parent.is_some() {
            let effective_kind: Option<&str> = match patch.kind.as_deref().map(str::trim) {
                Some("") => None,
                Some(k) => Some(k),
                None => tasks[idx].kind.as_deref(),
            };
            // Outer `None` = this row's container names nothing on the board;
            // inner `None` = it exists and carries no level. See
            // `check_ladder`.
            let parent_kind: Option<Option<&str>> = effective_parent
                .and_then(|p| tasks.iter().find(|t| t.id == p))
                .map(|t| t.kind.as_deref());
            check_ladder(&this_id, effective_kind, effective_parent, parent_kind)
                .map_err(|e| format!("hierarchy: {e}"))?;
            // BOTH DIRECTIONS (#1156 AC2). The check above judges this row's
            // own link; the rows INSIDE it are judged against its NEW level,
            // which nothing else on this path can see. Only a `kind` write can
            // invalidate a child — a child's rule reads its container's LEVEL,
            // never where that container itself sits — so a pure reparent skips
            // this walk rather than re-judging children it cannot have moved.
            if patch.kind.is_some() {
                let becoming = match effective_kind {
                    Some(k) => format!("cannot be {}", a_level(k)),
                    None => "cannot have its level cleared".to_string(),
                };
                for child in tasks.iter().filter(|t| t.parent.as_deref() == Some(this_id.as_str())) {
                    check_ladder(
                        &child.id,
                        child.kind.as_deref(),
                        Some(&this_id),
                        Some(effective_kind),
                    )
                    .map_err(|e| format!("hierarchy: {this_id} {becoming} — {e}"))?;
                }
            }
        }
        // ---- claim guards (#582). Read the row and the board BEFORE the
        // mutable borrow below; a failed guard returns without writing.
        let claimant = patch
            .assignee
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(actor)
            .to_string();
        if patch.claim {
            let current = &tasks[idx];
            // Re-claiming a task this same agent already holds is an idempotent
            // no-op, not a status-guard failure: the retry this has to survive
            // is "did my claim land before the compact?", and answering that
            // with an error would push the orchestrator toward the plain
            // assignee write this whole guard exists to replace.
            let already_mine = current.assignee.as_deref().map(str::trim) == Some(claimant.as_str())
                && current.status == "in-progress";
            if !already_mine {
                // Holder first, status second, deliberately. A task another
                // agent holds is ALSO past `queued`, so checking status first
                // answered the double-assign case — the one this guard exists
                // for — with "status is in-progress", which doesn't tell the
                // caller who has it. Refuse on the more specific fact.
                if let Some(held) = current.assignee.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                    if held != claimant {
                        return Err(format!("cannot claim {this_id}: already assigned to {held}"));
                    }
                }
                if current.status != "queued" {
                    return Err(format!(
                        "cannot claim {this_id}: status is {} — only a queued task can be claimed",
                        current.status
                    ));
                }
                // Guard against the deps this write LEAVES on the task, not the
                // ones it had before — claiming and re-pointing deps in one call
                // must be judged on the result.
                let probe = Task {
                    deps: deps.clone().unwrap_or_else(|| current.deps.clone()),
                    ..current.clone()
                };
                let unmet = unmet_deps(&probe, &tasks);
                if !unmet.is_empty() {
                    return Err(format!("cannot claim {this_id}: unmet deps {}", unmet.join(", ")));
                }
            }
        }
        let claim = patch.claim;
        let task = &mut tasks[idx];
        if let Some(t) = patch.title {
            let t = t.trim();
            if !t.is_empty() {
                task.title = t.to_string();
            }
        }
        if let Some(s) = patch.status {
            task.status = s;
        }
        if patch.issue.is_some() {
            task.issue = patch.issue.filter(|s| !s.trim().is_empty());
        }
        if patch.pr.is_some() {
            task.pr = patch.pr.filter(|s| !s.trim().is_empty());
        }
        if patch.pr_base.is_some() {
            task.pr_base = patch.pr_base.filter(|s| !s.trim().is_empty());
        }
        if patch.demo_path.is_some() {
            task.demo_path = patch.demo_path.filter(|s| !s.trim().is_empty());
        }
        if patch.description.is_some() {
            // TRIMMED, like the check above (#3261 review round 1). Both halves
            // read the same value or the field has two policies: the editor
            // trims before it sends, MCP does not, and a trailing "\n" was
            // refused from one caller and silently accepted from the other.
            // Trailing whitespace is not content, and an all-whitespace value
            // was already the clear.
            task.description = patch.description.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        }
        if patch.assignee.is_some() {
            task.assignee = patch.assignee.filter(|s| !s.trim().is_empty());
        }
        if patch.session.is_some() {
            task.session = patch.session.filter(|s| !s.trim().is_empty());
        }
        if let Some(text) = patch.note {
            let text = text.trim().to_string();
            if !text.is_empty() {
                task.notes.push(TaskNote { ts_ms: now_ms(), author: actor.to_string(), text });
                task.notes = cap_task_notes(std::mem::take(&mut task.notes), MAX_TASK_NOTES);
            }
        }
        if let Some(d) = deps {
            task.deps = d;
        }
        if let Some(r) = related {
            task.related = r;
        }
        // Already validated (and already trimmed, since a container id is
        // looked up by exact match rather than displayed).
        if let Some(p) = parent {
            task.parent = p;
        }
        if patch.kind.is_some() {
            task.kind = patch.kind.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        }
        // Grounding links REPLACE, like `deps`/`related` above — one rule for
        // every array on this patch (#1273).
        if let Some(l) = links {
            task.links = l;
        }
        // Sprint (#1272): 0 is the clear. Anything else is already known to be
        // >= 1 from the TYPE — `TaskPatch::sprint` is `Option<u32>`, so a
        // negative or fractional value was refused at the wire before it could
        // reach a patch (see the note beside the status/kind checks above;
        // there is deliberately no sprint check there). This `filter` is the
        // one place that has to know 0 is special, and it is written as a
        // filter rather than a branch so the clear and the set stay one
        // expression, the way `kind` above does it.
        if patch.sprint.is_some() {
            task.sprint = patch.sprint.filter(|n| *n != 0);
        }
        // #1152. Applied after `status` above, deliberately: a patch that
        // reopens a row and clears its archive stamp in one call must end with
        // the stamp gone, whatever order the caller wrote the two arguments in.
        // A stamp on a row that is NOT `done` is inert rather than illegal —
        // the board honours it only while the row is done — so there is nothing
        // to validate here, and the read-time rule is what keeps a reopened
        // task visible without a repair pass.
        if let Some(cleared) = patch.cleared {
            task.cleared_ms = if cleared { Some(now_ms()) } else { None };
        }
        // Last, so the claim's own two fields are what the guards above
        // approved — never a leftover from the generic patch application.
        if claim {
            task.assignee = Some(claimant);
            task.status = "in-progress".into();
        }
        task.updated_ms = now_ms();
        let snapshot = task.clone();
        // ---- WIP limits (#1175), judged HERE: `tasks` is now the board this
        // write produces, and `before_wip` is what it was. Nothing has been
        // persisted yet — `write_tasks` is the next line — so a refusal below
        // still leaves the board exactly as it was, which is the contract
        // every other refusal in this method keeps.
        //
        // AFTER the apply rather than before it (rev-1 B1) because the
        // question is about the resulting board and not about the patch: a
        // `parent` write changes which rows are leaves, so no reading of the
        // patch alone can say what the counts become.
        let breaches = wip_breaches(&board, &before_wip, &tasks, &this_id);
        if !breaches.is_empty() && board.enforce && origin == WriteOrigin::Agent {
            // Refuse only an AGENT's write, and only under an explicit
            // `enforce: true`. Warn-and-land is the default posture, and the
            // human's own board edit is never refused under either setting —
            // see `upsert_task_by_human`. More than one cap can go over on a
            // single write (a reparent that moves a row into one status and
            // frees a container into another), so the refusal names them all
            // rather than the first: the caller has to fix every one.
            return Err(breaches.iter().map(|b| b.refusal(&this_id)).collect::<Vec<_>>().join(" "));
        }
        self.write_tasks(group, &tasks)?;
        // A claim is audited under its own action so the durable record shows
        // WHY the assignee moved — a guarded grab, not an ordinary field write.
        let action = if claim { "task-claim" } else { "task-upsert" };
        self.audit(group, actor, action, serde_json::to_value(&snapshot).unwrap());
        // The demo-gate lifecycle hook (#1151), and the ONE place every status
        // transition passes through: `proceed_task`, `request_changes`, the board
        // overlay and every MCP `upsert_task` all funnel here, so hanging the
        // needs-you mapping off this call is what makes it impossible to move a
        // task into or out of the gate without the human's queue following.
        //
        // Still under `tasks_lock`, deliberately — see `needs_you_lock`'s doc.
        self.sync_demo_item(group, &snapshot, &prev_status);
        // A crossing that was allowed to land is audited under its own action,
        // so the durable record shows the board went over a declared cap and
        // WHICH cap — a fact the `task-upsert` entry above cannot carry,
        // because it records the row and not the board around it. Only ever
        // written for a landed crossing: a refusal returned above, before any
        // write, and an audit entry for a write that did not happen would be
        // the log claiming something the board never did.
        for b in &breaches {
            self.audit(
                group,
                actor,
                "task-wip-crossed",
                json!({
                    "task": this_id,
                    "status": b.status,
                    "limit": b.limit,
                    "count": b.count,
                    "enforce": board.enforce,
                    "origin": match origin {
                        WriteOrigin::Agent => "agent",
                        WriteOrigin::Human => "human",
                    },
                }),
            );
        }
        drop(guard);
        // Outside the tasks lock: notify is best-effort and can block on
        // delivery, the rule every other board notice already follows.
        for b in &breaches {
            self.notify_wip_crossing(group, origin, &this_id, b);
        }
        Ok(snapshot)
    }

    /// Tell the orchestrator its board just went over a declared WIP cap
    /// (#1175). The warn half of the feature: the write has already landed, so
    /// this is the only thing that makes the crossing visible to the agent
    /// whose queue discipline it is.
    ///
    /// Says who crossed it, because the answer changes what the orchestrator
    /// should do: its own crossing is its own to unwind, while the human's is a
    /// board it should re-read rather than argue with.
    fn notify_wip_crossing(
        &self,
        group: &GroupId,
        origin: WriteOrigin,
        task_id: &str,
        b: &WipBreach,
    ) {
        // "after writing", not "moved in": since the guard judges the whole
        // post-write board (rev-1 B1), the status that went over is not always
        // the one this row moved into — a reparent frees a container into a
        // status the written row never touched — and a notice that claimed
        // otherwise would send the reader looking in the wrong place.
        let who = match origin {
            WriteOrigin::Agent => format!("after writing {task_id}"),
            WriteOrigin::Human => format!("after the human's board edit to {task_id}"),
        };
        let _ = self.deliver_to_orchestrator(
            group,
            &format!(
                "[orrerix] WIP limit crossed: {} now holds {} of a declared {} ({who}). Finish or \
                 re-status one before starting more work. Call list_tasks to see the board.",
                b.status, b.count, b.limit
            ),
            brand::AUDIT_ACTOR,
        );
    }

    pub fn delete_task(&self, group: &GroupId, actor: &str, id: &str) -> Result<(), String> {
        let _guard = self.tasks_lock.lock_safe();
        // Partitioned rather than retained, because promotion needs the removed
        // row's OWN `parent` to find its children's next container. Splitting
        // (not `position` + `remove`) keeps the historic "every row with this id
        // goes" semantics of the `retain` this replaced: ids are minted unique,
        // so a duplicate can only come from a hand-edited `tasks.json`, and
        // leaving one of a pair behind after reporting success is not an
        // improvement on removing both.
        let (removed, mut tasks): (Vec<Task>, Vec<Task>) =
            self.tasks(group).into_iter().partition(|t| t.id == id);
        if removed.is_empty() {
            return Err(format!("unknown task: {id}"));
        }
        // Same locked write (#582): a deleted task's id must not survive on
        // anyone's links, or it would block its dependent forever.
        let relinked = strip_deleted_links(&mut tasks, &HashSet::from([id]));
        // ...nor on anyone's `parent` (#958) — its children are promoted rather
        // than orphaned or cascaded away.
        let reparented = promote_orphans(&mut tasks, &removed);
        self.write_tasks(group, &tasks)?;
        self.audit(
            group,
            actor,
            "task-delete",
            json!({ "id": id, "relinked": relinked, "reparented": reparented }),
        );
        Ok(())
    }

    /// Delete every task in the terminal `done` status in a single board write,
    /// returning the ids removed (empty if none were done). The board's "delete
    /// all done" action routes through here so the batch surfaces to the
    /// orchestrator as ONE board-change notice, not one per task (#120) — the
    /// coalesced notice is emitted here (best-effort), so callers must not fan
    /// out per-task notices. A no-op (nothing done) writes nothing and notifies
    /// nothing.
    pub fn delete_done_tasks(&self, group: &GroupId, actor: &str) -> Result<Vec<String>, String> {
        let removed = {
            let _guard = self.tasks_lock.lock_safe();
            // Split rather than filtered-then-retained: promotion reads the
            // removed rows' own `parent` pointers to find each survivor's
            // nearest surviving ancestor, and `partition` is stable, so both
            // halves keep the board's priority order.
            let (removed_rows, mut tasks): (Vec<Task>, Vec<Task>) =
                self.tasks(group).into_iter().partition(|t| t.status == "done");
            let removed: Vec<String> = removed_rows.iter().map(|t| t.id.clone()).collect();
            if removed.is_empty() {
                return Ok(removed);
            }
            let gone: HashSet<&str> = removed.iter().map(String::as_str).collect();
            let relinked = strip_deleted_links(&mut tasks, &gone);
            let reparented = promote_orphans(&mut tasks, &removed_rows);
            self.write_tasks(group, &tasks)?;
            self.audit(
                group,
                actor,
                "task-delete-done",
                json!({ "ids": removed, "relinked": relinked, "reparented": reparented }),
            );
            removed
        };
        // Outside the tasks lock: notify is best-effort and can block on delivery.
        let n = removed.len();
        self.notify_board_edit(
            group,
            &format!("deleted {n} done task{}", if n == 1 { "" } else { "s" }),
        );
        Ok(removed)
    }

    /// Archive every `done` row out of the human's board view in a single
    /// write, returning the ids stamped (empty if there was nothing to do)
    /// — #1152's "clear completed items", and the NON-destructive twin of
    /// `delete_done_tasks` above.
    ///
    /// Three properties, each load-bearing:
    ///
    /// - **Nothing is deleted.** Every row stays in `tasks.json` with its
    ///   notes, links and container intact; all that changes is a `cleared_ms`
    ///   stamp the human's board reads as "out of my way". `restore_cleared_tasks`
    ///   below undoes it, and the audit entry records the batch either way.
    /// - **`updated_ms` is deliberately NOT touched.** That field is what
    ///   `filter_done_rows` picks the newest `LIST_TASKS_DONE_CAP` `done` rows
    ///   by, so stamping 250 rows with a fresh `updated_ms` would silently
    ///   rewrite which twenty the orchestrator sees on its next `list_tasks` —
    ///   a human view action reaching into an agent's read. Clearing composes
    ///   with that cap by leaving its input alone.
    /// - **No board-change notice.** `notify_board_edit` exists to tell the
    ///   orchestrator its queue moved, and this moves nothing: no status, no
    ///   priority, no link, and nothing `TaskSummary` even carries. It is the
    ///   `reorder_tasks` precedent (a board write the orchestrator is
    ///   deliberately not interrupted for), not the `delete_done_tasks` one,
    ///   and on a 250-row batch the alternative is a prompt about a view
    ///   preference. The audit log is where it is recorded.
    pub fn clear_done_tasks(&self, group: &GroupId, actor: &str) -> Result<Vec<String>, String> {
        let _guard = self.tasks_lock.lock_safe();
        let mut tasks = self.tasks(group);
        let now = now_ms();
        let mut cleared: Vec<String> = Vec::new();
        for t in tasks.iter_mut() {
            // Already-cleared rows are skipped rather than re-stamped: a second
            // click must not rewrite the archive date of rows it isn't
            // archiving, and the returned list is then what actually changed.
            if t.status == "done" && t.cleared_ms.is_none() {
                t.cleared_ms = Some(now);
                cleared.push(t.id.clone());
            }
        }
        if cleared.is_empty() {
            return Ok(cleared);
        }
        self.write_tasks(group, &tasks)?;
        self.audit(group, actor, "task-clear-done", json!({ "ids": cleared }));
        Ok(cleared)
    }

    /// Un-archive a specific set of rows by id in a single board write (#1152),
    /// returning the ids actually restored. The counterpart to
    /// `clear_done_tasks`, backing both the board's per-row ↩ and its bulk
    /// "restore all". Ids that name no row, or a row carrying no stamp, are
    /// skipped rather than errored — the board can change under the human's
    /// click — and the returned list is what actually moved. Like the clear, it
    /// leaves `updated_ms` alone and raises no board-change notice.
    pub fn restore_cleared_tasks(
        &self,
        group: &GroupId,
        actor: &str,
        ids: &[String],
    ) -> Result<Vec<String>, String> {
        let _guard = self.tasks_lock.lock_safe();
        let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut tasks = self.tasks(group);
        let mut restored: Vec<String> = Vec::new();
        for t in tasks.iter_mut() {
            if wanted.contains(t.id.as_str()) && t.cleared_ms.is_some() {
                t.cleared_ms = None;
                restored.push(t.id.clone());
            }
        }
        if restored.is_empty() {
            return Ok(restored);
        }
        self.write_tasks(group, &tasks)?;
        self.audit(group, actor, "task-restore-cleared", json!({ "ids": restored }));
        Ok(restored)
    }

    /// Delete a specific set of tasks by id in a single board write, returning
    /// the ids actually removed (a subset of `ids`, in board order). Backs the
    /// board's multi-select "delete selected" action and mirrors
    /// `delete_done_tasks`: the whole batch surfaces to the orchestrator as ONE
    /// board-change notice (#120), emitted here (best-effort), so callers must
    /// not fan out per-task notices. Ids not on the board are skipped, not
    /// errored — the board can change under the human's selection (the
    /// orchestrator or a batch may have removed a row since they clicked) — and
    /// the skipped ids are recorded in the audit entry. An empty selection, or
    /// one matching nothing, writes nothing and notifies nothing.
    pub fn delete_tasks(&self, group: &GroupId, actor: &str, ids: &[String]) -> Result<Vec<String>, String> {
        let removed = {
            let _guard = self.tasks_lock.lock_safe();
            let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
            // Split, not filtered-then-retained (#958): a batch can remove a
            // parent AND its grandparent, so promotion has to climb the removed
            // rows' own `parent` chain, and it needs those rows to do it.
            // `partition` is stable, so both halves keep board order.
            let (removed_rows, mut tasks): (Vec<Task>, Vec<Task>) =
                self.tasks(group).into_iter().partition(|t| wanted.contains(t.id.as_str()));
            let removed: Vec<String> = removed_rows.iter().map(|t| t.id.clone()).collect();
            if removed.is_empty() {
                return Ok(removed);
            }
            // Ids the human selected that no longer name a board row. Skipped,
            // not fatal — but audited, so the divergence is traceable.
            let present: HashSet<&str> = removed.iter().map(String::as_str).collect();
            let skipped: Vec<&str> = ids.iter().map(String::as_str).filter(|id| !present.contains(id)).collect();
            // Strip by what was actually removed, not by what was asked for:
            // an id that named no row can still name a hand-edited dangling
            // link, and that link is not this delete's business (#582).
            let relinked = strip_deleted_links(&mut tasks, &present);
            let reparented = promote_orphans(&mut tasks, &removed_rows);
            self.write_tasks(group, &tasks)?;
            self.audit(
                group,
                actor,
                "task-delete-selected",
                json!({ "ids": removed, "skipped": skipped, "relinked": relinked, "reparented": reparented }),
            );
            removed
        };
        // Outside the tasks lock: notify is best-effort and can block on delivery.
        let n = removed.len();
        self.notify_board_edit(
            group,
            &format!("deleted {n} selected task{}", if n == 1 { "" } else { "s" }),
        );
        Ok(removed)
    }

    /// Reorder by explicit id list (board order = priority). Ids not
    /// mentioned keep their relative order after the mentioned ones.
    pub fn reorder_tasks(&self, group: &GroupId, actor: &str, ids: &[String]) -> Result<(), String> {
        let _guard = self.tasks_lock.lock_safe();
        let mut tasks = self.tasks(group);
        let mut ordered: Vec<Task> = Vec::with_capacity(tasks.len());
        for id in ids {
            if let Some(pos) = tasks.iter().position(|t| &t.id == id) {
                ordered.push(tasks.remove(pos));
            }
        }
        ordered.append(&mut tasks);
        self.write_tasks(group, &ordered)?;
        self.audit(group, actor, "task-reorder", json!({ "order": ids }));
        Ok(())
    }

    /// Guard the merge-gate actions to items actually at the gate. The UI only
    /// shows the buttons on `pr`/`human-testing` items, but the command surface
    /// is callable directly, so enforce it backend-side too — approving a
    /// `queued` item or requesting changes on a `done` one is meaningless.
    fn ensure_at_merge_gate(&self, group: &GroupId, id: &str) -> Result<(), String> {
        let status = self
            .tasks(group)
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("unknown task: {id}"))?
            .status;
        if MERGE_GATE_STATUSES.contains(&status.as_str()) {
            Ok(())
        } else {
            Err(not_at_merge_gate(id, &status))
        }
    }

    /// Merge-gate approve: mark the item done and issue a **one-time merge grant**
    /// for its PR so the orchestrator can actually merge (the enforced gate blocks
    /// a default-branch merge without a grant — clicking Approve without one leaves
    /// the orchestrator stuck, #83). The status change is the human's direct
    /// sign-off; `comment` is an optional approve-with-comment note delivered with
    /// the grant. When the task has no resolvable PR number, no grant is written and
    /// a plain approval notice is delivered instead (the human merges by hand).
    ///
    /// A **failed grant write** (a full disk) is an error, not a plain approval:
    /// the call returns `Err` with the item already flipped `done` and nothing
    /// announced. That is deliberate — an unannounced state the human sees an
    /// error for beats a confident notice that misdescribes what was authorized.
    pub fn approve_task(&self, group: &GroupId, id: &str, comment: Option<&str>) -> Result<Task, String> {
        self.ensure_at_merge_gate(group, id)?;
        let task = self.upsert_task_by_human(
            group,
            "human",
            Some(id),
            TaskPatch {
                status: Some("done".into()),
                note: Some("Approved at the merge gate.".into()),
                ..Default::default()
            },
        )?;
        // Grant the one-time merge for this PR (delivers the authorization + any
        // comment to the orchestrator). Falls back to a plain notice if the task
        // carries no PR number to key the grant on.
        //
        // The granted-vs-plain split is decided by `pr_number` ALONE, never by
        // whether the grant write happened to succeed: a failed write is an
        // error to surface, not a task that turns out to have had no PR. The
        // `.ok()` this replaced collapsed both into the plain path, so a
        // full-disk write failure announced "no PR" for a PR that exists (#507
        // review B1 — same defect the bulk path had).
        if let Some(num) = task.pr.as_deref().and_then(pr_number) {
            // Mint by the resolved NUMBER, so what is granted is exactly what
            // was classified — a ref that parses two ways can't diverge here.
            self.grant_merge(group, &num.to_string(), comment, "human")?;
        } else {
            let pr = task.pr.as_deref().unwrap_or("(no PR ref)");
            let extra = comment
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(|c| format!(" Note from the human: {c}"))
                .unwrap_or_default();
            let _ = self.deliver_to_orchestrator(
                group,
                &format!(
                    "[orrerix] the human APPROVED {} \"{}\" ({}) at the merge gate and marked it done. \
                     Merge the PR and close out the work item.{extra}",
                    task.id, task.title, pr
                ),
                "human",
            );
        }
        Ok(task)
    }

    /// Merge-gate approve for a whole board selection (#507): the same
    /// sign-off and the same **per-PR** one-time grants `approve_task` writes,
    /// one per item, followed by ONE consolidated notice naming every granted
    /// PR and carrying every per-task note — instead of the N separate prompts
    /// N single approves would queue at the orchestrator.
    ///
    /// What is deliberately *unchanged*: authority. Each PR still gets its own
    /// single-use, expiring `merge_grants/pr-<N>` file, minted exactly as
    /// before (`mint_merge_grant`), and the shim still consumes them one merge
    /// at a time. There is no bulk grant object, and no grant here authorizes
    /// a PR that was not selected.
    ///
    /// **All-or-nothing validation.** Every id must exist, be at the merge gate,
    /// and name a PR no other item in the batch names, before *anything* is
    /// written; one that doesn't fails the whole call with nothing minted. This
    /// differs on purpose from `delete_tasks`, which skips ids that vanished
    /// under the human's selection: deleting a row that is already gone is a
    /// no-op, but silently approving 4 of 5 items — with 4 live merge grants and
    /// no clear signal which was dropped — is an authority decision the human did
    /// not make. A clean refusal lets them re-tick and click again.
    ///
    /// **A failed grant *write* fails the call.** Validation-class problems are
    /// caught by the pre-flight above, but a write can still fail mid-batch (a
    /// full disk). That returns `Err` with some items already flipped `done` and
    /// some grants minted — and, critically, **nothing announced**: an
    /// unannounced grant expires unused and the human sees an error, whereas
    /// classifying the failure as "this item had no PR" would tell the
    /// orchestrator to close out by hand a PR that does exist. The granted-vs-
    /// plain split is therefore decided by `pr_number` in the pre-flight, never
    /// by whether a write succeeded (#507 review B1).
    ///
    /// An empty selection writes nothing and notifies nothing.
    pub fn approve_tasks(&self, group: &GroupId, items: &[ApproveItem]) -> Result<Vec<Task>, String> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        // Pre-flight, against ONE board snapshot: nothing is written until every
        // item has passed. `wanted[i]` is the PR number item i will be granted,
        // resolved HERE — so the notice's granted-vs-plain split is a property
        // of the refs the human selected, decided before any I/O can fail.
        let board = self.tasks(group);
        let mut wanted: Vec<Option<u64>> = Vec::with_capacity(items.len());
        // The board's selection is a Set, but the command surface is callable
        // directly — a repeated id would mint the same PR's grant twice and
        // list it twice in the notice, so refuse rather than guess.
        let mut seen_ids: HashSet<&str> = HashSet::new();
        // Two DISTINCT tasks naming the same PR are the same problem wearing a
        // different hat (a duplicate filing): both pass the id check, both mint
        // `pr-<N>` — the second overwriting the first — and the notice then says
        // "#7, #7 … one grant per PR", claiming two grants where one file
        // exists. Refuse that too, so the notice's count is always the truth.
        let mut seen_prs: HashSet<u64> = HashSet::new();
        for it in items {
            if !seen_ids.insert(it.id.as_str()) {
                return Err(format!("task {} appears twice in one bulk approval", it.id));
            }
            let task = board
                .iter()
                .find(|t| t.id == it.id)
                .ok_or_else(|| format!("unknown task: {}", it.id))?;
            if !MERGE_GATE_STATUSES.contains(&task.status.as_str()) {
                return Err(not_at_merge_gate(&it.id, &task.status));
            }
            let num = task.pr.as_deref().and_then(pr_number);
            if let Some(n) = num {
                if !seen_prs.insert(n) {
                    return Err(format!("PR #{n} appears twice in one bulk approval"));
                }
            }
            wanted.push(num);
        }

        let mut approved: Vec<Task> = Vec::with_capacity(items.len());
        // Notes are owned here because the notice borrows from them, and the
        // Task the note came from is moved into `approved`.
        let mut notes: Vec<Option<String>> = Vec::with_capacity(items.len());
        for (it, want) in items.iter().zip(wanted.iter()) {
            let note = it
                .comment
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            let task = self.upsert_task_by_human(
                group,
                "human",
                Some(&it.id),
                TaskPatch {
                    status: Some("done".into()),
                    note: Some("Approved at the merge gate.".into()),
                    ..Default::default()
                },
            )?;
            // Mint (no delivery): the consolidated notice below is the single
            // delivery for the whole batch. Minted by the NUMBER the pre-flight
            // resolved, so what is granted is exactly what will be announced,
            // and a write failure propagates instead of being reclassified.
            if let Some(n) = want {
                self.mint_merge_grant(group, &n.to_string(), "human")?;
            }
            approved.push(task);
            notes.push(note);
        }

        self.audit(
            group,
            "human",
            "task-approve-bulk",
            json!({
                "ids": items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
                "granted_prs": wanted.iter().flatten().collect::<Vec<_>>(),
            }),
        );

        // Split the batch into "granted a merge" and "approved, nothing to
        // grant" in board-selection order, then say all of it once.
        let mut granted: Vec<GrantedPr<'_>> = Vec::new();
        let mut plain: Vec<PlainApproval<'_>> = Vec::new();
        for ((task, note), num) in approved.iter().zip(notes.iter()).zip(wanted.iter()) {
            let note = note.as_deref();
            match num {
                Some(num) => granted.push(GrantedPr { num: *num, note }),
                None => plain.push(PlainApproval { id: &task.id, title: &task.title, note }),
            }
        }
        let msg = merge_grant_notice(&granted, &plain, GRANT_TTL_SECS / 60);
        let _ = self.deliver_to_orchestrator(group, &msg, "human");
        Ok(approved)
    }

    /// Merge-gate request-changes: record the findings as a note and deliver
    /// them to the orchestrator to route back to a worker. Status is left for
    /// the orchestrator to manage as it re-dispatches.
    pub fn request_changes(&self, group: &GroupId, id: &str, findings: &str) -> Result<Task, String> {
        let findings = findings.trim();
        if findings.is_empty() {
            return Err("request changes needs a note describing what to fix".into());
        }
        self.ensure_at_merge_gate(group, id)?;
        let task = self.upsert_task_by_human(
            group,
            "human",
            Some(id),
            TaskPatch { note: Some(format!("Requested changes: {findings}")), ..Default::default() },
        )?;
        let pr = task.pr.as_deref().unwrap_or("(no PR ref)");
        let _ = self.deliver_to_orchestrator(
            group,
            &format!(
                "[orrerix] the human REQUESTED CHANGES on {} \"{}\" ({}) at the merge gate. \
                 Findings: {findings}. Route it back to a worker to address, then re-request review.",
                task.id, task.title, pr
            ),
            "human",
        );
        Ok(task)
    }

    /// Guard the start action to items that are actually queued. The UI only
    /// shows the button on `queued` items, but the command surface is callable
    /// directly, so enforce it backend-side too — starting an in-progress or
    /// done item is meaningless.
    fn ensure_queued(&self, group: &GroupId, id: &str) -> Result<(), String> {
        let status = self
            .tasks(group)
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("unknown task: {id}"))?
            .status;
        if status == "queued" {
            Ok(())
        } else {
            Err(format!("task {id} is {status:?}, not queued — only a queued task can be started"))
        }
    }

    /// Start a queued item: record a human-attributed note and tell the
    /// orchestrator to begin work on it now. Deliberately does NOT flip the
    /// status — the orchestrator moves it to `in-progress` when it actually
    /// assigns a worker, so the board reflects real assignment rather than
    /// intent. The notice is best-effort (the board is the source of truth).
    ///
    /// A paused group is rejected up front (mirroring `steer_orchestrator`),
    /// and #569 changed the reason without changing the answer. The original
    /// one — the nudge would be discarded and a note left behind implying it
    /// landed — no longer holds: pause queues now, so the nudge would arrive
    /// on resume. What still holds is that this is a HUMAN clicking Start on a
    /// group they have paused, and the two readings of that click ("start it
    /// now" vs "start it whenever I get around to resuming") are far enough
    /// apart to be worth a synchronous error instead of a silent guess. Reject
    /// before any mutation so no note is appended, and let the human resume
    /// first — the same call `steer_orchestrator` makes for the same reason.
    pub fn start_task(&self, group: &GroupId, id: &str) -> Result<Task, String> {
        self.ensure_queued(group, id)?;
        if self.is_paused(group) {
            return Err("group is paused — resume before starting tasks".into());
        }
        let task = self.upsert_task_by_human(
            group,
            "human",
            Some(id),
            TaskPatch {
                note: Some("Started by the human — asked the orchestrator to begin work.".into()),
                ..Default::default()
            },
        )?;
        let _ = self.deliver_to_orchestrator(
            group,
            &format!(
                "[orrerix] the human started task {} (\"{}\") — begin work on it now.",
                task.id, task.title
            ),
            "human",
        );
        Ok(task)
    }

    /// Guard the proceed action to items actually in `prototype`. The UI only
    /// shows the button on prototype items, but the command surface is callable
    /// directly, so enforce it backend-side too (constraint 6) — "proceeding" a
    /// queued or done item is meaningless.
    fn ensure_prototype(&self, group: &GroupId, id: &str) -> Result<(), String> {
        let status = self
            .tasks(group)
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("unknown task: {id}"))?
            .status;
        if status == PROTOTYPE_STATUS {
            Ok(())
        } else {
            Err(format!(
                "task {id} is {status:?}, not {PROTOTYPE_STATUS:?} — Proceed only applies to a prototype"
            ))
        }
    }

    /// Proceed on a prototype (#147): the human has validated the demo and wants
    /// it promoted to a full production build. Flips `prototype` → `in-progress`
    /// (the item is back in active development, no longer parked on the human's
    /// verdict), records a human-attributed note, and delivers ONE typed notice
    /// telling the orchestrator to run the promotion. Unlike `start_task`, the
    /// status flip is durable — like `approve_task`, the board carries the
    /// decision even if a paused group drops the notice — so this does NOT reject
    /// on pause (the orchestrator sees the flip + note on resume via list_tasks).
    /// The notice is best-effort (the board is the source of truth).
    pub fn proceed_task(&self, group: &GroupId, id: &str) -> Result<Task, String> {
        self.ensure_prototype(group, id)?;
        let task = self.upsert_task_by_human(
            group,
            "human",
            Some(id),
            TaskPatch {
                status: Some("in-progress".into()),
                note: Some(
                    "Proceed — the human validated the prototype; promote it to a full production build."
                        .into(),
                ),
                ..Default::default()
            },
        )?;
        let _ = self.deliver_to_orchestrator(
            group,
            &format!(
                "[orrerix] the human clicked PROCEED on task {} (\"{}\") — the prototype is validated. \
                 Promote it to a full production build: production hardening + full reviews, no corners, \
                 the same promotion arc you'd run by hand.",
                task.id, task.title
            ),
            "human",
        );
        Ok(task)
    }

    /// Tell the orchestrator the human touched the board (best-effort; the
    /// board itself is the source of truth via list_tasks).
    pub(in crate::orchestration) fn notify_board_edit(&self, group: &GroupId, summary: &str) {
        let _ = self.deliver_to_orchestrator(
            group,
            &format!("[orrerix] the human updated the task board: {summary}. Call list_tasks to sync."),
            "human",
        );
    }

    /// The board's declared WIP caps with their live counts (#1175), for the
    /// MCP `list_tasks` reply — the same rows the human's board renders, so the
    /// orchestrator and the human are reading one board and one set of caps.
    ///
    /// Reads the policy itself, unlike [`Self::wip_rows`], because this is the
    /// agent path and there is no already-loaded workflow to hand it.
    #[doc(hidden)] // pub for integration tests
    pub fn wip_status_for_agents(&self, group: &GroupId) -> Vec<Value> {
        self.wip_rows(group, &self.board_policy(group))
    }

    /// The `{status, limit, count, enforce}` rows for an ALREADY-RESOLVED
    /// policy — what the board renders as `3/4` beside each capped status, and
    /// an empty list for every group that declares none.
    ///
    /// Takes the policy rather than reading it so [`Self::workflow_status`],
    /// which already loaded this repo's workflow for the file's `name`, does
    /// not parse the same YAML a second time on a call the publisher makes once
    /// a second per leased group (the group view's 2 s poll until #1608).
    ///
    /// Counted here, in the backend, rather than shipped as bare limits for the
    /// frontend to tally: [`wip_occupants`] is the one definition of what a cap
    /// counts, and a second tally in TypeScript is a second definition that
    /// would drift the first time either side learned something about
    /// containers.
    ///
    /// **A group with no caps reads no BOARD.** The `tasks.json` read below is
    /// behind the empty-map check on purpose, so a repo that declares no
    /// `board:` block never pays for a tally it has no use for.
    ///
    /// It does not pay *nothing*, and the earlier wording here claimed it did
    /// (rev-1 N2). Resolving the policy at all means a `load_workflow` — an
    /// open plus a YAML parse — for any group with `advanced_orchestrator` on,
    /// whether or not it declares `board:`. [`Self::workflow_status`] hands its
    /// already-loaded workflow straight in and so adds nothing, which is what
    /// that call sitting on a one-second publisher cadence requires; the `list_tasks`
    /// path ([`Self::wip_status_for_agents`]) has no such workflow in hand and
    /// pays one parse per call. A per-call cost on an agent-initiated read is
    /// not the polled cost #743 was about, which is why it is stated here
    /// rather than memoised.
    pub(in crate::orchestration) fn wip_rows(&self, group: &GroupId, board: &workflow::BoardPolicy) -> Vec<Value> {
        if board.wip.is_empty() {
            return vec![];
        }
        let tasks = self.tasks(group);
        board
            .wip
            .iter()
            .map(|(status, limit)| {
                json!({
                    "status": status,
                    "limit": limit,
                    "count": wip_occupants(&tasks, status).len(),
                    "enforce": board.enforce,
                })
            })
            .collect()
    }

    /// This group's `board:` policy — the per-status WIP caps and the enforce
    /// posture as declared, or **no caps at all** when the block is absent, the
    /// file will not parse, or the workflow is not in force for this group
    /// (#1175).
    ///
    /// One reader for the whole block, the reason `merge_queue_policy` gives:
    /// the write seam and the board's own count display must not come to
    /// different conclusions about what this repo declared.
    ///
    /// **An unreadable or unparseable file resolves to no caps, and that is
    /// deliberate.** It is fail-open, which is the wrong direction for a
    /// security check and the right one here: a WIP limit paces work, it does
    /// not guard anything (the human merge gate is not reachable from this
    /// block at all — see `workflow::BoardPolicy`), and a file caught mid-save
    /// already makes the whole workflow — gates included — unenforceable down
    /// the loud `workflow-invalid` path. Wedging every board write behind a
    /// half-written YAML file would be a far larger claim than a pacing
    /// discipline earns. Same posture, same reasoning, as
    /// [`Self::merge_queue_policy`].
    fn board_policy(&self, group: &GroupId) -> workflow::BoardPolicy {
        let Some(g) = self.group(group) else { return workflow::BoardPolicy::default() };
        if !g.guardrails.advanced_orchestrator {
            return workflow::BoardPolicy::default();
        }
        match load_active_workflow(&g.repo, &g.guardrails) {
            Ok(Some(wf)) => wf.board,
            _ => workflow::BoardPolicy::default(),
        }
    }

    /// Put a `progress` report on the board row this delegate is working, so the
    /// trail survives the delivery that no longer happens (#1958).
    ///
    /// A `progress` report needs no orchestrator action, so it never reaches that
    /// pane ([`report::reaches_orchestrator_pane`]). It is still recorded twice:
    /// the `tool-call` audit row every MCP call writes, and — where a row can be
    /// resolved — this note, which is what puts it where the human is already
    /// looking and what `get_task` hands the orchestrator on demand.
    ///
    /// **Resolution is orrerix-minted first, caller-supplied second.** The
    /// delegate's own `session_id` is what orrerix recorded at spawn and what the
    /// orchestrator writes onto the row it assigns; a delegate cannot choose it.
    /// Only when that finds nothing does the report's `ref` — an agent-authored
    /// string — get matched against the row's `pr` and then its `issue`. That
    /// order is not an authorization (this WRITES a note and reads nothing back,
    /// `rd_task_note`'s reason), it is about landing the note on the
    /// right row: a `ref` naming a PR two rows over costs a misfiled note, and
    /// the session never does.
    ///
    /// **No resolvable row is not an error.** The audit row is the floor and it
    /// is already written; a delegate whose work has no board row at all — an
    /// ad-hoc brief, a group not using the board — reports exactly as before and
    /// simply has no note to leave. Failing the tool call there would make the
    /// board mandatory, which it is not.
    ///
    /// Answers which of the four [`NoteOutcome`]s happened.
    pub(in crate::orchestration) fn report_task_note(
        &self,
        group: &GroupId,
        agent_id: &str,
        ref_: Option<&str>,
        text: &str,
    ) -> NoteOutcome {
        let tasks = match self.tasks_or_err(group) {
            Ok(t) => t,
            Err(_) => return NoteOutcome::Unreadable,
        };
        let session = self.agents.lock_safe().get(agent_id).and_then(|a| a.session_id.clone());
        let by_session = session.as_deref().and_then(|s| {
            tasks.iter().find(|t| t.session.as_deref() == Some(s))
        });
        // **The `ref` fallback skips `done` rows** (#1966 rev-final premortem 1).
        // Nothing clears `pr` when a task completes, so a long-lived board keeps
        // finished rows carrying the same PR as the live follow-up work, and a
        // first-match-wins scan over raw board order piles every note onto
        // whichever sorts first. A `done` row is never the row a delegate is
        // reporting progress ON, so it is the one class that can be excluded
        // without guessing. TWO live rows sharing a `ref` remain ambiguous and
        // first-match-wins — the residual, argued in the design note: the session
        // is what disambiguates, and it is tried first for exactly this reason.
        let by_ref = || {
            let n = ref_.filter(|s| !s.is_empty()).and_then(pr_number)?;
            let open = || tasks.iter().filter(|t| t.status != "done");
            open()
                .find(|t| t.pr.as_deref().and_then(pr_number) == Some(n))
                .or_else(|| open().find(|t| t.issue.as_deref().and_then(pr_number) == Some(n)))
        };
        let Some(id) = by_session.or_else(by_ref).map(|t| t.id.clone()) else {
            return NoteOutcome::NoRow;
        };
        match self.upsert_task(
            group,
            brand::AUDIT_ACTOR,
            Some(&id),
            TaskPatch { note: Some(text.to_string()), ..TaskPatch::default() },
        ) {
            Ok(_) => NoteOutcome::Noted,
            // NEVER `NoRow`: a row DID resolve, so answering "nothing matched"
            // would be a claim about the board's contents made on a write that
            // failed — the same defect this whole change fixes by not saying
            // "reported to orchestrator" (#1966 rev-final round 2 N1). See
            // [`NoteOutcome::NotWritten`] for the two causes that land here.
            Err(_) => NoteOutcome::NotWritten,
        }
    }
}
