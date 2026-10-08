//! The quick drive's registry wiring (#3679) — `pdtick`'s and `rdtick`'s
//! sibling, in a file of its own for their reason.
//!
//! A FILE is a scope a RENAME cannot step over, which is what CLAUDE.md's
//! source-scanning-guard convention asks for: `tests/quickdrive/guards.rs`
//! default-denies the whole of this file (and `registry/quick.rs`, the
//! human-side half) for landing verbs and for the calls that would end a pane.
//! The quick drive builds no `git` and no `gh` command line at all, so "never
//! merges, never pushes, never closes anything" is a property of what is NOT
//! written here rather than of a check that runs.
//!
//! **This is wiring, not logic.** Every state decision is
//! [`quickdrive::decide`]'s. What lives here is what only the registry can do:
//! read a pane's liveness, hold the state lock across the read-modify-write,
//! open or re-brief the pane that holds the turn, write the plan and findings
//! documents, and raise the one notice a run ends or parks with.
//!
//! # The shape of one step
//!
//! [`OrchRegistry::qd_drive_group`] is the only function that moves a run, and
//! it is entered from three places: the shared poll tick
//! ([`OrchRegistry::qd_driver_tick`], the backstop and the only thing that
//! reads the clocks), the MCP interception (a consumed `report` kicks a step so
//! a hop does not wait for the next wake), and the human's own commands. One
//! group is stepped by one caller at a time — see [`QdClaim`] — and **no lock
//! is held across a spawn or a delivery**: `qd_state_lock` spans a
//! load-modify-store of `quick_drive.json` and nothing else.
//!
//! Design note: `docs/design/quick-orchestration.md`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use super::quickdrive::{
    self, audit_action as act, QuickDriveRecord, QuickFacts, QuickHeld, QuickSide, QuickSignal,
    QuickState,
};
use super::{
    brand, is_live_cap_refusal, needsyou, notify, pr_number, render_template, report, tail_snippet,
    AgentStatus, Delivery, GroupId, LockExt, OrchRegistry, Role,
};

// ── the briefs ──────────────────────────────────────────────────────────────

/// The four briefs a quick run types into a pane. `include_str!`'d like the
/// review driver's, and pinned the same three ways in `tests/quickdrive/`: a
/// golden per template over one fixed fact set, a key-set assertion that no
/// `{{…}}` survives a render, and a hostile value that must arrive inert.
pub const QUICK_PLAN_TPL: &str = include_str!("templates/quick-plan.md");
pub const QUICK_WORK_TPL: &str = include_str!("templates/quick-work.md");
pub const QUICK_REVIEW_TPL: &str = include_str!("templates/quick-review.md");
pub const QUICK_FIX_TPL: &str = include_str!("templates/quick-fix.md");

/// The marker file that says a group was minted for a quick run.
///
/// A FILE rather than the roster, for `LEAD_MARKER`'s reason one level over:
/// the roster of a quick group is three ordinary blocks, so nothing in
/// `group.json` distinguishes it from an orchestration group whose human
/// happened to pin the same CLIs. `create_group_ex` clears it on every claim of
/// the id, beside `LEAD_MARKER`, so a group dir that is later handed to an
/// ordinary orchestration does not answer [`OrchRegistry::is_quick_group`] for
/// the rest of its life.
///
/// **Not `quick`**: that name is the run's document DIRECTORY
/// (`quickdrive::QUICK_DIR`), and a file of that name beside it would make the
/// directory impossible to create.
pub const QUICK_MARKER: &str = "quickrun";

/// How long one interpolated single-line fact may be — `rd_fact`'s cap, for
/// its reason: a brief is a prompt, and a prompt is the pane's resident
/// context.
const QD_FACT_CAP: usize = 2_000;

/// How much of a plan or a round's findings is inlined into a brief. The full
/// text is always in the file the brief names; this bounds what is TYPED,
/// which is the plan driver's own slice-brief figure.
pub const QD_BODY_CAP: usize = 20_000;

/// How large `messages.md` may grow before a message stops being saved.
///
/// The file is appended by every `message_orchestrator` call from any pane in
/// the group — the run's own or not, working or ended — and an MCP body may be
/// a megabyte. Nothing reads the file back into a pane, so the cost is disk
/// alone, and this is the ceiling on it. Each message is cut to
/// `QD_BODY_CAP` characters first (#3681 review N1).
const QD_MESSAGES_FILE_CAP: u64 = 1024 * 1024;

/// The `on_behalf_of` the review driver's delivery helpers audit their own
/// rows under when the quick drive calls them. A quick group has no
/// orchestrator, so there is no agent id to put there.
const QD_ON_BEHALF: &str = "quick-drive";

/// The line a pane is answered with, once per turn, when it reports
/// `progress` while holding the turn (#1959's rule, kept).
const QD_PROGRESS_KICK: &str = "a report(progress) moves nothing in a quick run. When your part is \
                                finished, report(outcome=done) — or approved / request_changes if \
                                you are the reviewer — and if you cannot continue, \
                                report(outcome=blocked).";

/// A placeholder a value must never be able to smuggle into a template.
///
/// `render_template` substitutes key by key over the accumulating text, so a
/// value carrying a literal `{{NOTES}}` would be expanded by a LATER key. The
/// braces are split rather than removed so the text still reads as what was
/// typed.
fn qd_inert(s: String) -> String {
    s.replace("{{", "{ {")
}

/// One interpolated single-line value, sanitized.
pub(super) fn qd_fact(s: &str) -> String {
    qd_inert(notify::sanitize_pane_text(s, QD_FACT_CAP, notify::Lines::Collapse))
}

/// One interpolated multi-line value — the task, a plan, a round's findings —
/// sanitized and capped at `cap` characters. Newlines survive; every other
/// control character and both square brackets do not, so the text cannot open
/// a line that reads as an `[orrerix]` notice.
pub(super) fn qd_text(s: &str, cap: usize) -> String {
    notify::sanitize_pane_text(s.trim(), cap, notify::Lines::Keep)
}

/// Write onto the record what the report — or the registry fact — that caused
/// an arc carried: why a hold is a hold, what the reviewer said, what the
/// worker said and which PR it named.
///
/// Called AFTER `QuickDriveRecord::take`, which clears `held_note` on every arc
/// that is not a hold.
fn qd_carry_notes(
    rec: &mut QuickDriveRecord,
    from: QuickState,
    step: &quickdrive::QuickStep,
    signal: &QdSignal,
    provider: Option<&str>,
    exit_note: &str,
) {
    match step.held {
        Some(QuickHeld::Messaged) => rec.held_note = signal.message.clone(),
        Some(QuickHeld::ProviderLimit) => rec.held_note = qd_fact(provider.unwrap_or("")),
        Some(
            QuickHeld::PlannerGone
            | QuickHeld::WorkerGone
            | QuickHeld::ReviewerGone
            | QuickHeld::RootGone,
        ) => rec.held_note = qd_fact(exit_note),
        Some(
            QuickHeld::PlannerBlocked
            | QuickHeld::WorkerBlocked
            | QuickHeld::ReviewerBlocked
            | QuickHeld::RootBlocked,
        ) => rec.held_note = signal.note.clone(),
        _ => {}
    }
    if step.reviewed {
        rec.review_note = signal.note.clone();
        if step.held == Some(QuickHeld::ReviewLimit) {
            rec.held_note = signal.note.clone();
        }
    } else if matches!(from, QuickState::WorkWait | QuickState::FixWait) && step.held.is_none() {
        rec.worker_note = signal.note.clone();
        if let Some(pr) = pr_number(&signal.pr_ref).filter(|_| !signal.pr_ref.is_empty()) {
            rec.pr = Some(pr);
        }
    }
}

/// The task, cut to one line for a notice.
///
/// Whitespace runs — line breaks included — become ONE space before anything
/// is dropped: `Lines::Collapse` removes a newline outright, so the last word
/// of one line and the first of the next would otherwise arrive as one word.
fn qd_task_line(task: &str) -> String {
    let flat = task.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = qd_inert(notify::sanitize_pane_text(&flat, 400, notify::Lines::Collapse));
    tail_free_snippet(&line, 100)
}

/// The first `max` characters of `s`, with an ellipsis when it was longer.
fn tail_free_snippet(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

// ── in-memory state ─────────────────────────────────────────────────────────

/// What a run's panes have said since the last step acted on it.
///
/// **In memory, and that is a bounded choice** — `PdSignal`'s. A `report` is an
/// event; the durable facts a run turns on are in its record and in the plan
/// and findings files, which the interception writes before it answers. An
/// orrerix restart between a report and the step that acts on it loses the
/// event and nothing else, and a restart parks the run anyway.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QdSignal {
    /// Which side sent `signal` and `progress`. A signal is acted on only
    /// while that side still holds the turn: a report that arrives a moment
    /// before the human forces a hand-off is about a turn that has ended, and
    /// read against the NEXT state it would be the wrong side's word — a
    /// worker's `done` taken as the reviewer asking for changes.
    pub from: Option<QuickSide>,
    /// What the pane holding the turn reported.
    pub signal: QuickSignal,
    /// That report's one-line note.
    pub note: String,
    /// That report's `ref`, which may name a PR.
    pub pr_ref: String,
    /// A pane of this run called `message_orchestrator`.
    pub messaged: bool,
    /// The last such message, for the hold's notice.
    pub message: String,
    /// The pane holding the turn reported `progress`.
    pub progress: bool,
}

impl QdSignal {
    /// Make this entry `side`'s, dropping whatever another side left in it.
    ///
    /// Everything but the message is a statement BY a side — its verdict, its
    /// note, its PR, its `progress` — so it is only meaningful beside the
    /// `from` it was written with. Re-labelling an entry without clearing
    /// those is how one side's `done` gets read as the next side's: a worker's
    /// report landing a moment after the human's hand-off leaves
    /// `{worker, Done}` behind, and a reviewer's `progress` that only set
    /// `from` turned it into `{reviewer, Done}` — a request for changes
    /// nobody made, and a round spent with no findings (#3681 review W3).
    ///
    /// A message is not cleared: it parks the run whichever pane sent it.
    pub fn claim(&mut self, side: QuickSide) {
        if self.from != Some(side) {
            self.signal = QuickSignal::None;
            self.note.clear();
            self.pr_ref.clear();
            self.progress = false;
        }
        self.from = Some(side);
    }
}

/// Everything the quick drive keeps in memory, under one lock.
///
/// One lock rather than five because nothing here is ever held across
/// anything: every access is a lookup or an insert, and a single leaf is one
/// row in `docs/design/lock-order.md` instead of five whose mutual order would
/// have to be argued.
#[derive(Default)]
pub struct QdMem {
    /// Pending signals, by group.
    pub(super) signals: HashMap<GroupId, QdSignal>,
    /// Groups whose run THIS process started or has reconciled. A run on disk
    /// that is not in here was written by an earlier process.
    pub(super) known: HashSet<GroupId>,
    /// Groups whose run is in a working state — the tick's whole candidate
    /// list, so a finished or parked run costs no wake at all.
    pub(super) working: HashSet<GroupId>,
    /// Groups a step is running for right now. See [`QdClaim`].
    pub(super) inflight: HashSet<GroupId>,
    /// When each working group was last stepped by the tick.
    pub(super) serviced_ms: HashMap<GroupId, u64>,
    /// Whether this process has looked for runs an earlier one left behind.
    pub(super) scanned: bool,
}

/// One caller's exclusive right to step one group, released on drop.
///
/// **This is the mutual exclusion a held lock would otherwise have to
/// provide, without holding one across a spawn.** A step reads the record,
/// may open a pane — which blocks until the frontend binds it — and writes the
/// record again. Two callers doing that for one group would open two panes for
/// one turn; a registry lock held across the spawn would stop them, and would
/// also put a pane's whole boot time inside every other caller's wait. So the
/// second caller simply does nothing: the first one is already doing it.
pub(super) struct QdClaim<'a> {
    reg: &'a OrchRegistry,
    group: GroupId,
}

impl Drop for QdClaim<'_> {
    fn drop(&mut self) {
        self.reg.qd_mem.lock_safe().inflight.remove(&self.group);
    }
}

/// What one step did, for a test to read rather than infer from the audit log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QdDriveReport {
    /// Another caller was already stepping this group, so this one did nothing.
    pub busy: bool,
    /// `quick_drive.json` is there and could not be read or written.
    pub state_unreadable: bool,
    /// The arc this step took, as `(from, to)` state words.
    pub advanced: Option<(String, String)>,
    /// The pane this step handed the turn to, as `(side, agent, how)` — `how`
    /// is one of `opened`, `reused`, `taken-over`, `resumed`, `typed`.
    pub handed_to: Option<(String, String, String)>,
    /// The refusal a hand-over ended in.
    pub refusal: String,
    /// This step raised the run's notice.
    pub notice: bool,
    /// This step answered a `report(progress)` in the pane that sent it.
    pub kicked_back: bool,
    /// The run's state when the step returned; empty when there is no run.
    pub state: String,
}

/// Who a caller is to a quick group's run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QdOwner {
    /// A pane this run opened. `current` is whether it is its side's current
    /// pane; `holds_turn` is whether its report can move the run right now.
    Pane { side: QuickSide, current: bool, holds_turn: bool },
    /// An agent in a quick group that the run does not own — a session
    /// somebody rejoined by hand. It has nobody to report to.
    Stranger,
}

/// The pane a hand-over landed in.
struct QdHandOver {
    agent: String,
    session: String,
    how: &'static str,
    cwd: String,
    branch: String,
}

impl OrchRegistry {
    // ---------- paths and markers ----------

    /// Whether `group` was minted for a quick run.
    pub fn is_quick_group(&self, group: &GroupId) -> bool {
        self.group_dir(group).join(QUICK_MARKER).is_file()
    }

    /// The directory a run's own documents live in.
    pub(super) fn qd_doc_dir(&self, group: &GroupId) -> PathBuf {
        self.group_dir(group).join(quickdrive::QUICK_DIR)
    }

    /// Where the plan is written.
    pub(super) fn qd_plan_path(&self, group: &GroupId) -> PathBuf {
        self.qd_doc_dir(group).join(quickdrive::QUICK_PLAN_FILE)
    }

    /// Where one review round's findings are written.
    pub(super) fn qd_findings_path(&self, group: &GroupId, round: u32) -> PathBuf {
        self.qd_doc_dir(group).join(quickdrive::findings_file_name(round))
    }

    /// Where the run's messages are appended.
    pub(super) fn qd_messages_path(&self, group: &GroupId) -> PathBuf {
        self.qd_doc_dir(group).join(quickdrive::QUICK_MESSAGES_FILE)
    }

    /// Where this group's `quick_drive.json` lives — a TEST seam, for
    /// `pd_record_path_for_test`'s reason: `group_dir_at` is the one place a
    /// group id becomes a path, and a test that joined the group dir by hand
    /// would be a second join in a file the join scan does not read.
    #[doc(hidden)]
    pub fn qd_record_path_for_test(&self, group: &GroupId) -> PathBuf {
        quickdrive::state_path(&self.group_dir(group))
    }

    /// The run's document paths — a TEST seam, for the same reason.
    #[doc(hidden)]
    pub fn qd_document_paths_for_test(&self, group: &GroupId, round: u32) -> (PathBuf, PathBuf, PathBuf) {
        (self.qd_plan_path(group), self.qd_findings_path(group, round), self.qd_messages_path(group))
    }

    /// One audit line in the quick drive's vocabulary.
    pub(super) fn qd_audit(&self, group: &GroupId, action: &str, detail: Value) {
        self.audit(group, brand::AUDIT_ACTOR, action, detail);
    }

    /// The group's run, read under the state lock and released.
    pub(super) fn qd_load_run(&self, group: &GroupId) -> Result<Option<QuickDriveRecord>, String> {
        let dir = self.group_dir(group);
        let _state_guard = self.qd_state_lock.lock_safe();
        quickdrive::load_state(&dir).map(|f| f.run).map_err(|e| e.to_string())
    }

    /// Read-modify-write the group's run under the state lock.
    ///
    /// `edit` answers whether it changed anything; nothing is written when it
    /// did not. **Nothing but file I/O happens under this lock** — `edit` is
    /// handed the record and may not reach back into the registry, which is
    /// what lets the lock be a ranked leaf (`lockorder::QUICK_DRIVE`).
    pub(super) fn qd_edit_run<T>(
        &self,
        group: &GroupId,
        edit: impl FnOnce(&mut QuickDriveRecord) -> Result<(bool, T), String>,
    ) -> Result<T, String> {
        let dir = self.group_dir(group);
        let _state_guard = self.qd_state_lock.lock_safe();
        let mut file = quickdrive::load_state(&dir).map_err(|e| e.to_string())?;
        let Some(rec) = file.run.as_mut() else {
            return Err("this group has no quick run".to_string());
        };
        let (changed, out) = edit(rec)?;
        if changed {
            quickdrive::store_state(&dir, &file)?;
        }
        Ok(out)
    }

    // ---------- interception ----------

    /// Who `agent_id` is to this group's quick run, or `None` when the group
    /// is not a quick group at all.
    ///
    /// **`None` for every caller in every ordinary group**, which is the
    /// product default and costs one `stat` of the marker. In a quick group it
    /// always answers: there is no orchestrator pane for a report to reach, so
    /// a report that fell through to the ordinary delivery would be an error
    /// about a root that was never going to exist.
    ///
    /// **Keyed on the agent id orrerix minted at spawn, never on text**, which
    /// is `rd_owner`'s property and the reason is the same.
    pub fn qd_owner(&self, group: &GroupId, agent_id: &str) -> Option<QdOwner> {
        if !self.is_quick_group(group) {
            return None;
        }
        let run = self.qd_load_run(group).ok().flatten();
        Some(match run.as_ref().and_then(|r| r.owner_of(agent_id).map(|o| (r, o))) {
            Some((r, (side, current))) => {
                QdOwner::Pane { side, current, holds_turn: r.holds_turn(agent_id) }
            }
            None => QdOwner::Stranger,
        })
    }

    /// **Consume one `report` from a pane in a quick group**, and answer what
    /// the tool tells its caller.
    ///
    /// Nothing is silent: every consumed report is audited with the side, the
    /// pane's standing and the status. A report from the pane holding the turn
    /// becomes the run's signal and kicks a step; anything else is recorded and
    /// answered, and moves nothing.
    ///
    /// **The plan and a round's findings are written HERE, before the tool
    /// answers**, rather than by the step that acts on the signal: they are the
    /// one part of a report that is not recoverable from the pane's own
    /// transcript by the next pane, so they go to disk while the caller is
    /// still inside its own turn and can be told where they went.
    #[allow(clippy::too_many_arguments)]
    pub fn qd_consume_report(
        &self,
        group: &GroupId,
        agent_id: &str,
        owner: QdOwner,
        status: &str,
        outcome: Option<&str>,
        note: &str,
        body: &str,
        pr_ref: &str,
    ) -> String {
        let QdOwner::Pane { side, current, holds_turn } = owner else {
            self.qd_audit(group, act::CONSUMED, json!({
                "agent": agent_id, "kind": "report:stranger", "status": status,
            }));
            return "recorded in the audit log. This group is a quick run with no orchestrator, \
                    and this pane is not one of the run's own, so nothing is listening for the \
                    report — tell the human in this pane."
                .to_string();
        };
        let run = self.qd_load_run(group).ok().flatten();
        let state = run.as_ref().map(|r| r.state());
        self.qd_audit(group, act::CONSUMED, json!({
            "agent": agent_id, "side": side.as_str(), "current": current,
            "holds_turn": holds_turn, "status": status, "outcome": outcome,
            "state": state.map(|s| s.as_str()),
        }));
        let Some(run) = run else {
            return "recorded in the audit log.".to_string();
        };
        if run.state().is_terminal() {
            return "recorded in the audit log. This quick run has ended, so the report moves \
                    nothing — the human reads this pane directly now."
                .to_string();
        }
        if run.state().is_parked() {
            return "recorded in the audit log. This quick run is held and the human has been \
                    told why; the report does not move it. They will resume or stop the run — \
                    answer them in this pane if they ask."
                .to_string();
        }
        if !holds_turn {
            return "recorded in the audit log. It is not this pane's turn in the quick run, so \
                    the report moves nothing — you will be briefed here when it is."
                .to_string();
        }
        if status == "progress" {
            let mut mem = self.qd_mem.lock_safe();
            let sig = mem.signals.entry(group.clone()).or_default();
            sig.claim(side);
            sig.progress = true;
            return "recorded in the audit log. A report(progress) moves nothing in a quick run."
                .to_string();
        }
        let signal = QuickSignal::from_report(side, status, outcome);
        // The document this report carries, written before anything acts on it.
        let body = if body.trim().is_empty() { note } else { body };
        let written = match (side, signal) {
            (QuickSide::Planner, QuickSignal::Done) => {
                Some(self.qd_write_doc(group, self.qd_plan_path(group), body, "plan"))
            }
            (QuickSide::Reviewer, QuickSignal::RequestChanges) => Some(self.qd_write_doc(
                group,
                self.qd_findings_path(group, run.next_findings_round()),
                body,
                "findings",
            )),
            _ => None,
        };
        {
            let mut mem = self.qd_mem.lock_safe();
            let sig = mem.signals.entry(group.clone()).or_default();
            // `blocked` outranks a `done` seen in the same window: the two can
            // only both be present if the pane said one and then the other,
            // and `blocked` is the one that needs a human.
            sig.claim(side);
            if sig.signal != QuickSignal::Blocked || signal == QuickSignal::Blocked {
                sig.signal = signal;
                sig.note = qd_fact(note);
                sig.pr_ref = qd_fact(pr_ref);
            }
        }
        self.qd_kick(group);
        let saved = match written {
            Some(Ok(path)) => format!(" Your text was saved to {}.", path.to_string_lossy()),
            Some(Err(e)) => format!(
                " orrerix could NOT save your text to the run's directory ({e}) — the next pane \
                 will be given your one-line note only."
            ),
            None => String::new(),
        };
        format!(
            "consumed by the quick run: this report is what moves it, and it was not typed into \
             any other pane as-is.{saved}"
        )
    }

    /// **Record one `message_orchestrator` from a pane in a quick group**, and
    /// answer what the tool tells its caller.
    ///
    /// There is no orchestrator to deliver it to, so the message is appended to
    /// the run's `messages.md` and — when the run is working and the pane is a
    /// current one — the run parks on `held(messaged)`, which is how the human
    /// is told. A superseded pane's message is recorded and parks nothing, the
    /// review driver's own rule (#1871 B2): a pane nobody is talking to any
    /// more must not be able to park a run without bound.
    pub fn qd_consume_message(
        &self,
        group: &GroupId,
        agent_id: &str,
        owner: QdOwner,
        text: &str,
    ) -> String {
        let line = tail_free_snippet(&report::relay_payload(text), QD_BODY_CAP);
        let appended = self.qd_append_message(group, agent_id, &line);
        let (side, current) = match owner {
            QdOwner::Pane { side, current, .. } => (Some(side), current),
            QdOwner::Stranger => (None, false),
        };
        self.qd_audit(group, act::MESSAGE, json!({
            "agent": agent_id, "side": side.map(|s| s.as_str()), "current": current,
            "saved": appended.is_ok(),
        }));
        let working = self
            .qd_load_run(group)
            .ok()
            .flatten()
            .is_some_and(|r| r.state().is_live());
        if working && current {
            {
                let mut mem = self.qd_mem.lock_safe();
                let sig = mem.signals.entry(group.clone()).or_default();
                sig.messaged = true;
                sig.message = qd_fact(&format!("{agent_id}: {line}"));
            }
            self.qd_kick(group);
            return "recorded. This is a quick run with no orchestrator, so nobody can answer \
                    in another pane: the run is being held and the human is shown your message. \
                    Carry on only if you can without the answer."
                .to_string();
        }
        "recorded for the human. This is a quick run with no orchestrator, and the run is not \
         waiting on this pane, so nothing else happens — tell the human in this pane."
            .to_string()
    }

    /// Write one of the run's documents atomically, sanitized. `Lines::Keep`:
    /// a plan and a set of findings are prose, and their line structure is the
    /// payload.
    fn qd_write_doc(
        &self,
        group: &GroupId,
        path: PathBuf,
        body: &str,
        kind: &str,
    ) -> Result<PathBuf, String> {
        let text = report::relay_payload_keeping_lines(body);
        let result = fs::create_dir_all(self.qd_doc_dir(group))
            .map_err(|e| e.to_string())
            .and_then(|()| {
                super::atomic_write(&path, text.as_bytes()).map_err(|e| e.to_string())
            });
        self.qd_audit(group, act::DOCUMENT, json!({
            "kind": kind, "path": path.to_string_lossy(), "chars": text.chars().count(),
            "written": result.is_ok(),
        }));
        result.map(|()| path)
    }

    /// Append one message to the run's `messages.md`, unless the file has
    /// reached `QD_MESSAGES_FILE_CAP` — past it the message is refused, which
    /// the caller audits as not saved.
    fn qd_append_message(&self, group: &GroupId, agent_id: &str, line: &str) -> Result<(), String> {
        use std::io::Write as _;
        fs::create_dir_all(self.qd_doc_dir(group)).map_err(|e| e.to_string())?;
        let size = fs::metadata(self.qd_messages_path(group)).map(|m| m.len()).unwrap_or(0);
        if size >= QD_MESSAGES_FILE_CAP {
            return Err("the run's messages file is full".to_string());
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.qd_messages_path(group))
            .map_err(|e| e.to_string())?;
        writeln!(f, "- {agent_id}: {line}").map_err(|e| e.to_string())
    }

    /// Step `group` now, off this thread, so a hop does not wait for the next
    /// poll wake.
    ///
    /// A no-op on a registry with no self-handle — every integration test —
    /// which is what lets a test call [`qd_drive_group`](Self::qd_drive_group)
    /// itself and read what one step did. The poll tick is the backstop either
    /// way: a kick that is lost costs one wake, never a hop.
    pub(super) fn qd_kick(&self, group: &GroupId) {
        let Some(reg) = self.arc() else { return };
        let group = group.clone();
        std::thread::spawn(move || {
            reg.qd_drive_group(&group, super::now_ms());
        });
    }

    // ---------- the tick ----------

    /// One quick-drive step for at most one group — the SEVENTH step of
    /// `gh_poll_tick`, after `pd_driver_tick`.
    ///
    /// On the shared poll loop rather than a thread of its own because that is
    /// the rule ("one tick loop, one order"); it makes no `gh` call, so it
    /// costs that loop a file read for a working run and nothing at all
    /// otherwise. Oldest-serviced first, so two working runs alternate.
    pub fn qd_driver_tick(&self, now: u64) -> Option<GroupId> {
        self.qd_startup_scan(now);
        let group = {
            let mem = self.qd_mem.lock_safe();
            let mut due: Vec<(u64, GroupId)> = mem
                .working
                .iter()
                .map(|g| (mem.serviced_ms.get(g).copied().unwrap_or(0), g.clone()))
                .collect();
            // The group id breaks a tie deterministically, so two never-serviced
            // groups do not alternate on `HashSet` iteration order.
            due.sort();
            due.into_iter().next().map(|(_, g)| g)
        }?;
        self.qd_drive_group(&group, now);
        self.qd_mem.lock_safe().serviced_ms.insert(group.clone(), now);
        Some(group)
    }

    /// The session the ROSTER holds for `agent_id`, for a side neither the
    /// live registry nor the run's own record has one for.
    ///
    /// The run's record learns a pane's session at a hand-over, from the
    /// spawn's own answer — which is empty for every CLI that mints its id
    /// after boot (`premints_session_id` is false for four of the six). The
    /// registry learns it later (`associate_session`) and writes it to the
    /// roster, not to the run. So after a restart, when no agent is in memory,
    /// the roster is the only place a first-pass worker's, a planner's or a
    /// first reviewer's session is written down; without this read Resume
    /// parked every such run as unresumable while the session sat on disk
    /// (#3681 review W1).
    ///
    /// The LAST row naming the agent: ids stopped recycling in #524 and a
    /// quick group is always newly minted, so an id names one agent here.
    fn qd_roster_session(&self, group: &GroupId, agent_id: &str) -> Option<String> {
        if agent_id.is_empty() {
            return None;
        }
        self.merged_records(group)
            .into_iter()
            .rev()
            .find(|r| r.id == agent_id && r.session.as_deref().is_some_and(|s| !s.is_empty()))
            .and_then(|r| r.session)
    }

    /// Test seam: put `signal` in this group's signal slot, as a report that
    /// lost a race with the human's hand-off leaves it.
    #[doc(hidden)]
    pub fn qd_put_signal_for_test(&self, group: &GroupId, signal: QdSignal) {
        self.qd_mem.lock_safe().signals.insert(group.clone(), signal);
    }

    /// Test seam: what this group's signal slot holds now.
    #[doc(hidden)]
    pub fn qd_signal_for_test(&self, group: &GroupId) -> QdSignal {
        self.qd_mem.lock_safe().signals.get(group).cloned().unwrap_or_default()
    }

    /// Test seam: [`quick_step`](Self::quick_step) while another step holds
    /// the group's claim — the poll tick mid-spawn.
    #[doc(hidden)]
    pub fn quick_step_while_claimed_for_test(&self, group: &GroupId) -> Value {
        let _claim = self.qd_claim(group);
        self.quick_step(group)
    }

    /// Take the exclusive right to step `group`, or `None` if a step is
    /// already running for it.
    pub(super) fn qd_claim(&self, group: &GroupId) -> Option<QdClaim<'_>> {
        let fresh = self.qd_mem.lock_safe().inflight.insert(group.clone());
        fresh.then(|| QdClaim { reg: self, group: group.clone() })
    }

    /// Whether `agent_id`'s pane is still alive. **`false` only on a positive
    /// reading**: an agent this registry has no record of is "could not
    /// check", which is not "it is dead" — `rd_pane_exit`'s asymmetry, and its
    /// fail direction.
    fn qd_pane_alive(&self, agent_id: &str) -> bool {
        if agent_id.is_empty() {
            return true;
        }
        self.agent(agent_id).is_none_or(|a| a.status != AgentStatus::Dead)
    }

    /// What a dead pane went out saying, for the hold's notice.
    fn qd_exit_note(&self, agent_id: &str) -> String {
        let Some(a) = self.agent(agent_id) else { return String::new() };
        let how = match a.killed_by {
            Some(who) => format!("ended by {}", who.as_str()),
            None => "exited".to_string(),
        };
        let tail = a.last_exit_tail.as_deref().unwrap_or("").trim().to_string();
        if tail.is_empty() {
            format!("pane {agent_id} {how}")
        } else {
            format!("pane {agent_id} {how}; last output: {}", tail_snippet(&tail, 200))
        }
    }

    /// **Step one group's run once.** The only function that moves a run.
    ///
    /// In order: park a run an earlier process left working; read the record;
    /// read the registry facts the decision needs (OUTSIDE the state lock — a
    /// pane's status is behind `agents`, which ranks above it); take
    /// [`quickdrive::decide`]'s step under the lock; then, outside it, deliver
    /// the brief the new turn-holder is owed and raise the notice a park or a
    /// finish is owed.
    #[doc(hidden)]
    pub fn qd_drive_group(&self, group: &GroupId, now: u64) -> QdDriveReport {
        let mut out = QdDriveReport::default();
        let Some(_claim) = self.qd_claim(group) else {
            out.busy = true;
            return out;
        };
        self.qd_reconcile(group, now);

        let before = match self.qd_load_run(group) {
            Ok(Some(r)) => r,
            Ok(None) => return out,
            Err(e) => {
                out.state_unreadable = true;
                self.qd_audit(group, act::STATE_UNREADABLE, json!({ "error": e }));
                // Nothing can be decided about a record that will not read, and
                // re-reading it every wake would write this row every wake.
                self.qd_mem.lock_safe().working.remove(group);
                return out;
            }
        };
        out.state = before.state().as_str().to_string();
        if !before.state().is_live() {
            self.qd_mem.lock_safe().working.remove(group);
            return out;
        }

        // The registry facts, read before the state lock is taken.
        // A report from a side that no longer holds the turn says nothing
        // about this one — see `QdSignal::from`. It is dropped from the MAP,
        // not from a local copy: left there, the next writer could adopt it
        // (`QdSignal::claim`). A message is not a report and is not subject to
        // this: it parks the run whichever pane sent it.
        let signal = {
            let mut mem = self.qd_mem.lock_safe();
            let turn = before.state().turn();
            match mem.signals.get_mut(group) {
                Some(s) => {
                    if s.from != turn {
                        s.signal = QuickSignal::None;
                        s.note.clear();
                        s.pr_ref.clear();
                        s.progress = false;
                    }
                    s.clone()
                }
                None => QdSignal::default(),
            }
        };
        let turn_agent = before
            .state()
            .turn()
            .map(|s| before.pane(s).agent.clone())
            .unwrap_or_default();
        let provider = if turn_agent.is_empty() {
            None
        } else {
            self.provider_limit_for_agent(&turn_agent)
        };
        let alive = self.qd_pane_alive(&turn_agent);
        let exit_note = if alive { String::new() } else { self.qd_exit_note(&turn_agent) };
        let facts = QuickFacts {
            now_ms: now,
            signal: signal.signal,
            pane_alive: alive,
            messaged: signal.messaged,
            provider_limited: provider.is_some(),
        };

        // Decide and store, under the lock and touching nothing but the file.
        let decided = self.qd_edit_run(group, |rec| {
            // The record moved between the two reads — a human's Stop, a
            // resume. Whatever this step read is about a run that is no longer
            // there; the next step reads the new one.
            if rec.state() != before.state() || rec.brief_pending != before.brief_pending {
                return Ok((false, (rec.clone(), None, false)));
            }
            let limits = rec.limits();
            match quickdrive::decide(rec, &facts, &limits) {
                Some(step) => {
                    let from = rec.state();
                    if rec.take(&step, now).is_err() {
                        return Ok((false, (rec.clone(), None, false)));
                    }
                    // What the report that caused the arc carried. Set AFTER
                    // `take`, which clears `held_note` on every arc that is
                    // not a hold.
                    qd_carry_notes(rec, from, &step, &signal, provider.as_deref(), &exit_note);
                    Ok((true, (rec.clone(), Some((from, step)), false)))
                }
                None if signal.progress && !rec.progress_answered && !rec.brief_pending => {
                    rec.progress_answered = true;
                    Ok((true, (rec.clone(), None, true)))
                }
                None => Ok((false, (rec.clone(), None, false))),
            }
        });
        let (mut cur, took, kick) = match decided {
            Ok(v) => v,
            Err(e) => {
                out.state_unreadable = true;
                self.qd_audit(group, act::STATE_UNREADABLE, json!({ "error": e }));
                return out;
            }
        };

        // ---- outside the lock from here ----
        {
            let mut mem = self.qd_mem.lock_safe();
            if took.is_some() {
                // The arc consumed everything this window said, a message
                // included: one park per notice, not one per fact.
                mem.signals.remove(group);
            } else if kick {
                if let Some(s) = mem.signals.get_mut(group) {
                    s.progress = false;
                }
            }
        }
        let mut parked_or_finished = false;
        if let Some((from, step)) = &took {
            out.advanced = Some((from.as_str().to_string(), cur.state().as_str().to_string()));
            let action = match cur.state() {
                QuickState::Held => act::HELD,
                QuickState::Satisfied => act::SATISFIED,
                _ => act::ADVANCED,
            };
            self.qd_audit(group, action, json!({
                "from": from.as_str(), "to": cur.state().as_str(),
                "reason": step.held.map(|h| h.as_str()),
                "review_rounds": cur.review_rounds, "reviews_total": cur.reviews_total,
            }));
            parked_or_finished = !cur.state().is_live();
        }
        if kick && !turn_agent.is_empty() {
            let line = format!("{} {QD_PROGRESS_KICK}", brand::NOTICE_MARKER);
            out.kicked_back = self
                .deliver_prompt(&turn_agent, &line, brand::AUDIT_ACTOR, Delivery::MidSession)
                .is_ok();
        }
        if cur.state().is_live() && cur.brief_pending {
            cur = self.qd_hand_over(group, &cur, now, &mut out);
            parked_or_finished |= !cur.state().is_live();
        }
        if !cur.state().is_live() {
            self.qd_mem.lock_safe().working.remove(group);
        }
        if parked_or_finished {
            out.notice = self.qd_raise_notice(group, &cur, now);
        }
        out.state = cur.state().as_str().to_string();
        out
    }

    // ---------- the hand-over ----------

    /// Deliver the pending brief to the pane that now holds the turn, opening
    /// or re-opening that pane if it has to, and record what happened. Answers
    /// the record as it stands afterwards.
    ///
    /// A failure parks the run — `cap-refused` when the group is at its
    /// live-agent cap, `unresumable` for anything else — with the refusal
    /// quoted, because a hold whose notice does not say what was refused is a
    /// hold nobody can act on.
    fn qd_hand_over(
        &self,
        group: &GroupId,
        rec: &QuickDriveRecord,
        now: u64,
        out: &mut QdDriveReport,
    ) -> QuickDriveRecord {
        let Some(side) = rec.state().turn() else { return rec.clone() };
        let text = self.qd_brief(group, rec);
        let result = self.qd_deliver_turn(group, rec, side, &text);
        let expected = rec.state();
        let stored = self.qd_edit_run(group, |cur| {
            let same_turn = cur.state() == expected && cur.brief_pending;
            match &result {
                Ok(h) => {
                    if same_turn {
                        cur.brief_delivered(side, &h.agent, &h.session, now);
                    } else {
                        // The run moved while the pane was opening. The pane is
                        // still this run's — recorded, so its traffic is
                        // consumed rather than misdelivered.
                        cur.pane_mut(side).record(&h.agent, &h.session);
                    }
                    if side == QuickSide::Worker && !h.cwd.is_empty() {
                        cur.worker_cwd = h.cwd.clone();
                        if !h.branch.is_empty() {
                            cur.worker_branch = h.branch.clone();
                        }
                    }
                    Ok((true, cur.clone()))
                }
                Err(why) => {
                    if !same_turn {
                        return Ok((false, cur.clone()));
                    }
                    let reason = if is_live_cap_refusal(why) {
                        QuickHeld::CapRefused
                    } else {
                        QuickHeld::Unresumable
                    };
                    if cur.advance(QuickState::Held, Some(reason), now).is_ok() {
                        cur.held_note = qd_fact(why);
                    }
                    Ok((true, cur.clone()))
                }
            }
        });
        let cur = match stored {
            Ok(c) => c,
            Err(e) => {
                out.state_unreadable = true;
                self.qd_audit(group, act::STATE_UNREADABLE, json!({ "error": e }));
                return rec.clone();
            }
        };
        match &result {
            Ok(h) => {
                out.handed_to =
                    Some((side.as_str().to_string(), h.agent.clone(), h.how.to_string()));
                self.qd_audit(group, act::HANDOFF, json!({
                    "side": side.as_str(), "agent": h.agent, "how": h.how,
                    "state": expected.as_str(),
                }));
            }
            Err(why) => {
                out.refusal = why.clone();
                self.qd_audit(group, act::HELD, json!({
                    "from": expected.as_str(), "to": cur.state().as_str(),
                    "reason": cur.held_reason.map(|h| h.as_str()), "detail": why,
                }));
            }
        }
        cur
    }

    /// Put `text` in front of the pane that runs `side`.
    ///
    /// **The review driver's own hand-back ladder, called rather than
    /// copied**, for a side that has a session: the live idle pane already
    /// running it (`rd_reuse_pane`), else any live pane on it
    /// (`rd_take_over_pane`), else a new pane resuming it (`rd_spawn`). A side
    /// that has never been opened gets its first pane instead.
    ///
    /// **A reviewer is opened in the worker's own worktree** with no worktree
    /// of its own — `cwd_override` with `use_worktree: false`. That is safe
    /// because the run is strictly alternating (the record says which one pane
    /// holds the turn) and because the reviewer's class denies it the editing
    /// tools, and it is what lets a review need no commit and no PR. The path
    /// is the worker's `AgentEntry.cwd`, orrerix-made at the worker's spawn and
    /// never caller-supplied.
    fn qd_deliver_turn(
        &self,
        group: &GroupId,
        rec: &QuickDriveRecord,
        side: QuickSide,
        text: &str,
    ) -> Result<QdHandOver, String> {
        let (role, name) = match side {
            QuickSide::Planner => (Role::Planner, "quick: plan"),
            QuickSide::Worker => (Role::Worker, "quick: work"),
            QuickSide::Reviewer => (Role::Reviewer, "quick: review"),
            QuickSide::Root => {
                return Err("a quick run with a single agent running the task is not part of \
                            this build"
                    .to_string());
            }
        };
        // The built-in roster names each block after its class.
        let block = role.as_str();
        let pane = rec.pane(side);
        let known = if pane.agent.is_empty() { None } else { self.agent(&pane.agent) };
        // What the registry knows NOW first: a CLI that mints its own session
        // id reports it some time after boot, later than the spawn that
        // recorded this pane. Then the run's own record, then the roster —
        // see `qd_roster_session` for why the third is not optional.
        let session = known
            .as_ref()
            .and_then(|a| a.session_id.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| Some(pane.session.clone()).filter(|s| !s.is_empty()))
            .unwrap_or_default();
        let plain = |agent: String, how: &'static str| QdHandOver {
            agent,
            session: session.clone(),
            how,
            cwd: String::new(),
            branch: String::new(),
        };
        if !session.is_empty() {
            if let Some(agent) = self.rd_reuse_pane(group, QD_ON_BEHALF, &session, block, text) {
                return Ok(plain(agent, "reused"));
            }
            if let Some(agent) = self.rd_take_over_pane(group, QD_ON_BEHALF, &session, block, text)
            {
                return Ok(plain(agent, "taken-over"));
            }
            let a = match side {
                // A planner has no dedicated workspace to resolve: it ran in
                // the repository itself and resumes there, which is what a
                // spawn with no worktree and no `cwd_override` gives it — the
                // MCP arm's own rule for a planner. `rd_spawn` resolves a
                // workspace through `resolve_worker_resume_cwd`, which is for
                // the two roles that must never land in the main clone and
                // says a planner resume must not call it (#3681 review W5).
                QuickSide::Planner => self.spawn_agent_bound(
                    group,
                    role,
                    Some(block.to_string()),
                    "",
                    text,
                    false,
                    None,
                    None,
                    Some(session.clone()),
                    None,
                    None,
                    None,
                )?,
                _ => self.rd_spawn(
                    group,
                    role,
                    Some(block.to_string()),
                    Some(session.clone()),
                    text,
                )?,
            };
            return Ok(QdHandOver {
                session: a.session_id.clone().unwrap_or_else(|| session.clone()),
                agent: a.id,
                how: "resumed",
                cwd: a.cwd,
                branch: a.branch.unwrap_or_default(),
            });
        }
        // No session is known for this side.
        if let Some(a) = known.filter(|a| a.status != AgentStatus::Dead) {
            // A live pane whose CLI has not reported its session yet. Typing
            // into it by its own id needs no session at all.
            self.deliver_prompt(&a.id, text, brand::AUDIT_ACTOR, Delivery::MidSession)?;
            return Ok(plain(a.id, "typed"));
        }
        if !pane.agent.is_empty() {
            return Err(format!(
                "the {} pane ({}) is gone and no session was ever recorded for it — its CLI \
                 closed before it reported one — so there is nothing to re-open",
                side.as_str(),
                pane.agent
            ));
        }
        // The side's first pane.
        let base = Some(rec.base.clone()).filter(|b| !b.trim().is_empty());
        let a = match side {
            // No worktree: a planner reads the repo and reports a plan.
            QuickSide::Planner => self.spawn_agent_bound(
                group,
                role,
                Some(block.to_string()),
                name,
                text,
                false,
                None,
                None,
                None,
                None,
                None,
                None,
            )?,
            QuickSide::Worker => self.spawn_agent_bound(
                group,
                role,
                Some(block.to_string()),
                name,
                text,
                true,
                None,
                base,
                None,
                None,
                None,
                None,
            )?,
            QuickSide::Reviewer => {
                if rec.worker_cwd.trim().is_empty() {
                    return Err("the worker's workspace was never recorded, so there is nowhere \
                                to open the reviewer"
                        .to_string());
                }
                self.spawn_agent_bound(
                    group,
                    role,
                    Some(block.to_string()),
                    name,
                    text,
                    false,
                    None,
                    None,
                    None,
                    Some(rec.worker_cwd.clone()),
                    None,
                    None,
                )?
            }
            // Refused at the top of this function; answered again here rather
            // than asserted, because this path must never be able to panic.
            QuickSide::Root => return Err("no such pane in this build".to_string()),
        };
        Ok(QdHandOver {
            session: a.session_id.clone().unwrap_or_default(),
            agent: a.id,
            how: "opened",
            cwd: a.cwd,
            branch: a.branch.unwrap_or_default(),
        })
    }

    // ---------- the brief ----------

    /// The brief the pane holding the turn is owed, rendered from the record.
    ///
    /// **Every interpolated value is sanitized at THIS call site**, which is
    /// what makes the sanitization a pin rather than a decoration (§5.5 of the
    /// review driver's note): a test that sanitized inside its own render
    /// harness would pass identically while this function handed raw text to a
    /// pane. So the hostile-value test calls this function.
    ///
    /// Rendered from the record rather than stored on it, so a resume and a
    /// restart re-deliver the brief the turn began with without a second copy
    /// of it having to be kept in step.
    pub(super) fn qd_brief(&self, group: &GroupId, rec: &QuickDriveRecord) -> String {
        let task = qd_text(&rec.task, quickdrive::QUICK_TASK_CAP);
        let round = rec.round().to_string();
        let max = rec.max_review_rounds.to_string();
        let notes = if rec.notes.is_empty() {
            String::new()
        } else {
            let mut s = String::from("Notes the human added to this run:\n");
            for n in &rec.notes {
                s.push_str("- ");
                s.push_str(&qd_fact(n));
                s.push('\n');
            }
            s.push('\n');
            s
        };
        let plan_path = self.qd_plan_path(group);
        let plan_at = qd_fact(&plan_path.to_string_lossy());
        let has_plan = rec.plan_step && plan_path.is_file();
        let body = match rec.state() {
            QuickState::PlanWait => {
                let base = if rec.base.trim().is_empty() {
                    "the repository's default branch".to_string()
                } else {
                    qd_fact(&rec.base)
                };
                render_template(
                    QUICK_PLAN_TPL,
                    &[("TASK", &task), ("NOTES", &notes), ("BASE", &base)],
                )
            }
            QuickState::WorkWait => {
                let plan = if has_plan {
                    format!(
                        "The planner wrote a plan for this task. It is saved at {plan_at}, and \
                         it follows — work to it:\n\n{}\n\n",
                        self.qd_inline_doc(&plan_path)
                    )
                } else {
                    String::new()
                };
                let when = if rec.review_step {
                    "When the work is ready for review"
                } else {
                    "When the work is finished"
                };
                render_template(
                    QUICK_WORK_TPL,
                    &[("TASK", &task), ("PLAN", &plan), ("NOTES", &notes), ("WHEN_DONE", when)],
                )
            }
            QuickState::ReviewWait => {
                let cwd = qd_fact(&rec.worker_cwd);
                let branch = if rec.worker_branch.trim().is_empty() {
                    "the branch checked out there".to_string()
                } else {
                    qd_fact(&rec.worker_branch)
                };
                // The ref to diff against: the one the human named, else the
                // repository's default branch as git reports it now. Where git
                // cannot say, the brief asks for the log rather than naming a
                // branch that may not exist.
                let base = if rec.base.trim().is_empty() {
                    self.group(group)
                        .and_then(|g| crate::git::default_branch_name(&g.repo))
                        .unwrap_or_default()
                } else {
                    rec.base.trim().to_string()
                };
                let diff = if base.is_empty() {
                    "git log --oneline -20".to_string()
                } else {
                    format!("git diff {}...HEAD", qd_fact(&base))
                };
                let plan = if has_plan {
                    format!(
                        "A plan was written for this task and the worker was told to follow it. \
                         It is saved at {plan_at}.\n\n"
                    )
                } else {
                    String::new()
                };
                let pr = match rec.pr {
                    Some(n) => format!(
                        "The worker named pull request #{n} for this work. Post your review \
                         there as well; your verdict still travels by report.\n\n"
                    ),
                    None => String::new(),
                };
                let worker_note = if !rec.worker_note.trim().is_empty() {
                    qd_fact(&rec.worker_note)
                } else if rec.forced {
                    "(none — the human handed this to review before the worker reported)"
                        .to_string()
                } else {
                    "(none)".to_string()
                };
                render_template(
                    QUICK_REVIEW_TPL,
                    &[
                        ("ROUND", &round),
                        ("MAX_ROUNDS", &max),
                        ("TASK", &task),
                        ("CWD", &cwd),
                        ("BRANCH", &branch),
                        ("DIFF", &diff),
                        ("PLAN", &plan),
                        ("PR", &pr),
                        ("WORKER_NOTE", &worker_note),
                        ("NOTES", &notes),
                    ],
                )
            }
            QuickState::FixWait => {
                let path = self.qd_findings_path(group, rec.reviews_total);
                let findings = if rec.forced {
                    "The human sent this back without waiting for the reviewer's verdict, so no \
                     findings were recorded for it. Read the notes below, or ask them in this \
                     pane."
                        .to_string()
                } else if rec.reviews_total > 0 && path.is_file() {
                    format!(
                        "The reviewer's findings are saved at {}, and they follow:\n\n{}",
                        qd_fact(&path.to_string_lossy()),
                        self.qd_inline_doc(&path)
                    )
                } else {
                    format!("The reviewer's note: {}", qd_fact(&rec.review_note))
                };
                // The review that produced these findings is the one BEFORE the
                // review the run is now on, which is what `review_rounds`
                // counts once the arc has spent it.
                let why = if rec.forced {
                    "The human sent this task back to you.".to_string()
                } else {
                    format!(
                        "The reviewer asked for changes on this task (review round {} of at \
                         most {max}).",
                        rec.review_rounds.max(1)
                    )
                };
                render_template(
                    QUICK_FIX_TPL,
                    &[
                        ("WHY", &why),
                        ("TASK", &task),
                        ("FINDINGS", &findings),
                        ("NOTES", &notes),
                    ],
                )
            }
            // Nothing holds the turn in these, so there is no brief to render.
            QuickState::RootWait
            | QuickState::Held
            | QuickState::Satisfied
            | QuickState::Cancelled => String::new(),
        };
        match rec.resumed_from {
            Some(reason) => format!(
                "{} the human resumed this quick run. It had been held ({}): {}. The brief this \
                 turn began with follows — carry on from where you are.\n\n{body}",
                brand::NOTICE_MARKER,
                reason.as_str(),
                reason.notice_line(),
            ),
            None => body,
        }
    }

    /// One of the run's documents, read back for inlining into a brief:
    /// sanitized again (the file is on disk, where anything could have edited
    /// it) and capped at [`QD_BODY_CAP`], with a line saying so when it was
    /// cut.
    fn qd_inline_doc(&self, path: &std::path::Path) -> String {
        let raw = fs::read_to_string(path).unwrap_or_default();
        let shown = qd_text(&raw, QD_BODY_CAP);
        if raw.trim().chars().count() > QD_BODY_CAP {
            format!("{shown}\n(cut here — the whole text is in the file named above)")
        } else {
            shown
        }
    }

    /// The brief the current turn-holder is owed — a TEST seam over
    /// [`qd_brief`](Self::qd_brief), the live render path, so the template
    /// pins read what a pane is actually typed.
    #[doc(hidden)]
    pub fn qd_brief_for_test(&self, group: &GroupId) -> String {
        match self.qd_load_run(group) {
            Ok(Some(rec)) => self.qd_brief(group, &rec),
            _ => String::new(),
        }
    }

    // ---------- the notice ----------

    /// What the human is told when a run parks or finishes.
    pub(super) fn qd_notice_text(&self, group: &GroupId, rec: &QuickDriveRecord) -> String {
        let task = qd_task_line(&rec.task);
        let branch = if rec.worker_branch.trim().is_empty() {
            String::new()
        } else {
            format!(" The work is on branch {}", qd_fact(&rec.worker_branch))
        };
        let pr = rec.pr.map(|n| format!(" (PR #{n})")).unwrap_or_default();
        let place = if branch.is_empty() { String::new() } else { format!("{branch}{pr}.") };
        let text = match rec.state() {
            QuickState::Satisfied if rec.review_step => {
                let reviews = rec.reviews_total;
                let note = if rec.review_note.trim().is_empty() {
                    String::new()
                } else {
                    format!(" Reviewer: {}", qd_fact(&rec.review_note))
                };
                format!(
                    "Quick run finished — approved after {reviews} review{}: \"{task}\".{place}{note} \
                     Nothing was merged; its panes are still open for you to read or close.",
                    if reviews == 1 { "" } else { "s" }
                )
            }
            QuickState::Satisfied => {
                let note = if rec.worker_note.trim().is_empty() {
                    String::new()
                } else {
                    format!(" Worker: {}", qd_fact(&rec.worker_note))
                };
                format!(
                    "Quick run finished — the worker reported done (no review step): \
                     \"{task}\".{place}{note} Nothing was merged; its panes are still open for \
                     you to read or close."
                )
            }
            _ => {
                let reason = rec.held_reason.unwrap_or(QuickHeld::Unresumable);
                let detail = if rec.held_note.trim().is_empty() {
                    String::new()
                } else {
                    format!(" — {}", qd_fact(&rec.held_note))
                };
                let messages = if reason == QuickHeld::Messaged {
                    format!(
                        " Every message is in {}.",
                        qd_fact(&self.qd_messages_path(group).to_string_lossy())
                    )
                } else {
                    String::new()
                };
                format!(
                    "Quick run held ({}): \"{task}\". {}{detail}.{messages}{place} Resume it or \
                     stop it from the menu of any of its panes.",
                    reason.as_str(),
                    capitalized(reason.notice_line()),
                )
            }
        };
        tail_free_snippet(&text, needsyou::ITEM_TEXT_MAX - 1)
    }

    /// Raise the one notice a park or a finish is owed: a needs-you item and,
    /// in the app, a desktop toast. Answers whether the item was raised.
    ///
    /// **Exactly one open item per run.** The item this run raised before, if
    /// it is still open, is withdrawn first — a run that parks, is resumed and
    /// parks again should leave the human one thing to read, not a history.
    pub(super) fn qd_raise_notice(&self, group: &GroupId, rec: &QuickDriveRecord, now: u64) -> bool {
        self.qd_withdraw_notice(group, rec);
        let text = self.qd_notice_text(group, rec);
        let urgency = if rec.state() == QuickState::Held {
            needsyou::Urgency::High
        } else {
            needsyou::Urgency::Normal
        };
        let raised = self.raise_needs_you(
            group,
            brand::AUDIT_ACTOR,
            needsyou::RaiseRequest {
                kind: needsyou::Kind::Feedback,
                text: text.clone(),
                task: None,
                urgency,
            },
        );
        let item = raised.as_ref().ok().map(|r| r.item.id.clone()).unwrap_or_default();
        if !item.is_empty() {
            let _ = self.qd_edit_run(group, |cur| {
                cur.notice_item = item.clone();
                Ok((true, ()))
            });
        }
        self.qd_audit(group, act::NOTICE, json!({
            "state": rec.state().as_str(),
            "reason": rec.held_reason.map(|h| h.as_str()),
            "item": item, "raised": raised.is_ok(), "at": now,
            "error": raised.as_ref().err(),
        }));
        // A toast only where there is a desktop to show it on: a registry with
        // no app handle is a headless one, and must not start a process.
        if self.app.lock_safe().is_some() {
            super::notify_desktop(&format!("{} · quick run", brand::NAME), &text);
        }
        raised.is_ok()
    }

    /// Take back the needs-you item this run raised, if it is still open.
    /// Best-effort: an item the human already resolved is simply not there to
    /// withdraw.
    pub(super) fn qd_withdraw_notice(&self, group: &GroupId, rec: &QuickDriveRecord) {
        if rec.notice_item.is_empty() {
            return;
        }
        let open = self
            .needs_you(group)
            .ok()
            .is_some_and(|items| {
                items.iter().any(|i| i.id == rec.notice_item && !i.status.is_resolved())
            });
        if open {
            let _ = self.withdraw_needs_you(group, brand::AUDIT_ACTOR, &rec.notice_item);
        }
    }

    // ---------- restart ----------

    /// Park a run that an EARLIER process left working, once per group per
    /// process.
    ///
    /// Every pane died with that process, so the record's "the worker holds
    /// the turn" is a statement about a pane that no longer exists. Nothing is
    /// re-opened here — the human asked for short-lived runs, not runs that
    /// re-spawn themselves while nobody is looking — so the run parks on
    /// `held(restart)` and raises its one notice. A run that was already
    /// parked is left exactly as it was: its notice is still on the list.
    pub(super) fn qd_reconcile(&self, group: &GroupId, now: u64) {
        let fresh = self.qd_mem.lock_safe().known.insert(group.clone());
        if !fresh {
            return;
        }
        let parked = self.qd_edit_run(group, |rec| {
            if !rec.state().is_live() {
                return Ok((false, None));
            }
            let from = rec.state();
            if rec.advance(QuickState::Held, Some(QuickHeld::Restart), now).is_err() {
                return Ok((false, None));
            }
            Ok((true, Some((from, rec.clone()))))
        });
        if let Ok(Some((from, rec))) = parked {
            self.qd_mem.lock_safe().working.remove(group);
            self.qd_audit(group, act::RESTART_PARKED, json!({ "from": from.as_str() }));
            self.qd_raise_notice(group, &rec, now);
        }
    }

    /// Look, once per process, for quick runs an earlier process left behind,
    /// and park the working ones.
    ///
    /// The tick's candidate list is in memory and starts empty, so without
    /// this a run that was working when the app closed would sit in its
    /// working state on disk with nothing ever looking at it. Group dirs are
    /// read off the orchestration root; a name that does not parse as a
    /// `GroupId` is skipped rather than joined.
    pub(super) fn qd_startup_scan(&self, now: u64) {
        let first = {
            let mut mem = self.qd_mem.lock_safe();
            !std::mem::replace(&mut mem.scanned, true)
        };
        if !first {
            return;
        }
        let Ok(entries) = fs::read_dir(&self.root) else { return };
        for e in entries.flatten() {
            let Ok(found) = GroupId::parse(&e.file_name().to_string_lossy()) else { continue };
            if self.is_quick_group(&found) {
                self.qd_reconcile(&found, now);
            }
        }
    }
}

/// `s` with its first letter upper-cased — a hold's reason sentence opening a
/// notice.
fn capitalized(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
