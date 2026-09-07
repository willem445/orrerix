//! Structured agent panes: an agent driven over its CLI's own protocol rather
//! than scraped off a PTY (#2850 S3b).
//!
//! `doc/design/harness-adapters.md` is the contract; this module is the
//! `src-tauri` half of it — the part that owns a process, a pane id and a
//! group, none of which the engine leaf may know about.
//!
//! # What a structured pane is, in this process
//!
//! A [`StructuredPane`] is a `Box<dyn AgentPane>` (today always
//! `harness::pi::PiPane`), a reserved pane id, and the bookkeeping the drainer
//! and the dialog policy need. It is registered in `OrchRegistry` by agent id,
//! and **its `AgentEntry.pty_id` stays `None` forever**.
//!
//! That last point is the load-bearing one. Every PTY-side operation —
//! `write_pty`, `resize_pty`, `PtyManager::kill` — reaches a pane through
//! `pty_id`, so a pane that never has one is unreachable from all of them by
//! construction rather than by a check at each site. The pane id it does carry
//! comes from `PtyManager::reserve_id`, the same counter `spawn_pty` mints
//! from, so the two kinds share one keyspace and cannot collide.
//!
//! # Constraint 1
//!
//! Nothing here resizes anything. There is no ConPTY to resize, and the pane
//! cell the human sees is a DOM surface (#2891) rather than an xterm.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::sync::Mutex;

use loomux_engine::harness::{
    AgentPane, DecisionSource, HarnessEvent, RequestId, UiAnswer, UiMethod,
};
use loomux_engine::obs::LockExt;

use super::GroupId;

/// How many extension-UI dialogs one pane may have outstanding.
///
/// Section 3.5 bounds dialogs with a cap and never with an orrerix timer: a
/// timed auto-answer is a decision nobody made, recorded as though somebody
/// had. Past the cap a further request is answered `Cancelled` and audited,
/// which is a decision loomux DID make and says so.
///
/// Four rather than one: a pane legitimately raising a small burst (an
/// extension asking two things about one action) must not have the second
/// cancelled, while a pane looping on dialogs is stopped well before the
/// human queue is buried.
pub const MAX_PENDING_UI: usize = 4;

/// One dialog this pane is blocked on.
#[derive(Debug, Clone)]
pub struct PendingUi {
    pub id: RequestId,
    pub method: UiMethod,
    pub title: Option<String>,
    pub message: Option<String>,
    pub options: Vec<String>,
    /// The needs-you row raised for it, so settling the dialog can resolve the
    /// human queue entry rather than leaving an orphan.
    pub needs_you: Option<String>,
    pub raised_ms: u64,
}

/// A live structured pane.
pub struct StructuredPane {
    /// The driver. `Box<dyn AgentPane>` rather than `PiPane`, because the
    /// spawn path is written harness-neutral on purpose: R2's claude arm adds
    /// a variant here and changes nothing else in this module.
    pub pane: Box<dyn AgentPane>,
    /// The id the queue, the drainer and `deliver_now` key on. From
    /// `PtyManager::reserve_id`, never from a counter of this module's own.
    pub pane_id: u32,
    pub group: GroupId,
    pub agent: String,
    /// Dialogs awaiting an answer, oldest first.
    pub pending_ui: Mutex<Vec<PendingUi>>,
    /// Bytes accepted by `send`, for the delivery receipt.
    pub sent: AtomicU64,
}

impl StructuredPane {
    /// Is this dialog still outstanding?
    pub fn has_pending(&self, id: &RequestId) -> bool {
        self.pending_ui.lock_safe().iter().any(|p| &p.id == id)
    }

    /// Take a dialog off the pending list, returning it.
    pub fn take_pending(&self, id: &RequestId) -> Option<PendingUi> {
        let mut q = self.pending_ui.lock_safe();
        let at = q.iter().position(|p| &p.id == id)?;
        Some(q.remove(at))
    }

    /// The pane is attention-worthy while a human owes it an answer.
    ///
    /// Section 5.4: attention for a structured pane comes from the event
    /// stream and never from scraping what orrerix itself rendered — there is
    /// nothing to scrape, and a model writing prose shaped like a question
    /// grid would be read as one if there were.
    pub fn awaiting_human(&self) -> bool {
        !self.pending_ui.lock_safe().is_empty()
    }
}

/// The `delivery_from` the compose strip writes (#43) — the human typing into
/// a pane rather than an agent or loomux itself.
pub const COMPOSE_AUTHOR: &str = "human";

/// The roster `pane_kind` a structured pane records.
///
/// Absent means `pty`, so a roster written before #2850 reads correctly and
/// this is the only value that ever appears.
pub const PANE_KIND_STRUCTURED: &str = "structured";

/// What `write_pty` says when a client tries to type into a structured pane.
///
/// A refusal rather than a silent accept, and a NAMED one rather than the
/// generic "pty not found" a missing map entry produces: the second would be
/// true but useless, and a client cannot tell it from a pane that has died.
///
/// The remedy is in the message because there IS one — a delivery goes through
/// the queue, and a dialog through `answer_pane_ui` — and a refusal that does
/// not say where the door is just moves the confusion.
pub const WRITE_PTY_REFUSAL: &str =
    "this is a structured pane: it has no terminal to type into. Deliver a turn through \
     the queue, or answer a dialog with answer_pane_ui";

/// Which `Turn` a delivery is, from the kind the queue admitted it under.
///
/// The variant is not decoration (`harness::Turn`): a structured driver renders
/// `Notice` with its marker prefix and `Human` without one, and R2's
/// `permissions.json` policy may read it. Flattening all four to a string would
/// make "who said this" unrecoverable one layer down.
///
/// `delivery_from` decides the last two rather than the `Delivery` kind,
/// because a mid-session delivery is a `Prompt` when an agent sent it, a
/// `Notice` when loomux did, and a `Human` when the compose strip did — one
/// kind, three authors.
pub fn turn_for(
    kind: loomux_engine::model::Delivery,
    from: &str,
    text: &str,
) -> loomux_engine::harness::Turn {
    use loomux_engine::harness::Turn;
    use loomux_engine::model::Delivery;
    match kind {
        Delivery::FreshKickoff | Delivery::ResumeKickoff => Turn::Kickoff(text.to_string()),
        _ if from == super::brand::AUDIT_ACTOR => Turn::Notice(text.to_string()),
        _ if from == COMPOSE_AUTHOR => Turn::Human(text.to_string()),
        _ => Turn::Prompt(text.to_string()),
    }
}

/// Which events reach the frontend as a batch.
///
/// Everything the renderer needs and nothing it does not: the whole
/// `HarnessEvent` vocabulary rides `orch-pane-event`, because the DOM renderer
/// (#2891) projects from the events themselves. A function rather than an
/// inline `true` so the decision has somewhere to live when a variant is added
/// that should NOT be shipped.
pub fn goes_to_frontend(_ev: &HarnessEvent) -> bool {
    true
}

/// A pane registry, keyed by agent id.
#[derive(Default)]
pub struct StructuredPanes {
    by_agent: Mutex<HashMap<String, Arc<StructuredPane>>>,
    /// pane id -> agent id, so a delivery holding only the id can find its
    /// pane without scanning. The mirror of `by_pty`, for panes that have no
    /// pty.
    by_pane_id: Mutex<HashMap<u32, String>>,
}

impl StructuredPanes {
    pub fn insert(&self, pane: Arc<StructuredPane>) {
        self.by_pane_id
            .lock_safe()
            .insert(pane.pane_id, pane.agent.clone());
        self.by_agent.lock_safe().insert(pane.agent.clone(), pane);
    }

    pub fn get(&self, agent: &str) -> Option<Arc<StructuredPane>> {
        self.by_agent.lock_safe().get(agent).cloned()
    }

    /// The lookup `deliver_now` makes BEFORE it touches `PtyManager`.
    ///
    /// The ordering is the structural guard: a structured pane is found here
    /// and returns early, so nothing downstream can hand a reserved pane id to
    /// the PTY path.
    pub fn by_pane_id(&self, pane_id: u32) -> Option<Arc<StructuredPane>> {
        let agent = self.by_pane_id.lock_safe().get(&pane_id).cloned()?;
        self.get(&agent)
    }

    /// Is this id a structured pane? The question `write_pty` asks in order to
    /// refuse with a message that says why, rather than with the generic
    /// "pty not found" a missing map entry would produce.
    pub fn is_structured(&self, pane_id: u32) -> bool {
        self.by_pane_id.lock_safe().contains_key(&pane_id)
    }

    /// Drop a pane, returning it so the caller can run its teardown outside
    /// this lock — `PiPane::drop` waits on a child, and must never do so while
    /// the registry is held.
    pub fn remove(&self, agent: &str) -> Option<Arc<StructuredPane>> {
        let pane = self.by_agent.lock_safe().remove(agent)?;
        self.by_pane_id.lock_safe().remove(&pane.pane_id);
        Some(pane)
    }

    pub fn is_empty(&self) -> bool {
        self.by_agent.lock_safe().is_empty()
    }
}

/// The answer a dialog was settled with, and who settled it.
///
/// `by` is a property of the ENTRY POINT and never an argument a caller
/// supplies — the `questions.json` boundary (section 3.5). Every agent may be
/// ASKED; no agent may ever answer.
#[derive(Debug, Clone)]
pub struct UiSettlement {
    pub answer: UiAnswer,
    pub by: DecisionSource,
}

impl UiSettlement {
    /// The human, through the one trusted `answer_pane_ui` command.
    pub fn human(answer: UiAnswer) -> Self {
        UiSettlement {
            answer,
            by: DecisionSource::Human,
        }
    }

    /// loomux itself: the per-pane cap, or the role policy that refuses to park
    /// an orchestrator (#946). Audited as `Policy` because that is what it is —
    /// a decision loomux made, recorded as loomux made it.
    pub fn policy(answer: UiAnswer) -> Self {
        UiSettlement {
            answer,
            by: DecisionSource::Policy,
        }
    }
}

/// Does this role PARK on a dialog, or get it cancelled immediately?
///
/// Section 3.5 role asymmetry, and it is not a convenience: a parked
/// orchestrator is #946 — machine progress must never stop on human absence —
/// while a parked worker is the correct scope, the same shape as `ask_human`.
/// Both raise a needs-you item either way, so the human sees what was asked in
/// both cases.
pub fn parks_on_dialog(role: super::Role) -> bool {
    !matches!(role, super::Role::Orchestrator | super::Role::Manager)
}

/// The launch spec for a pi pane, from the same inputs the PTY arm reads.
///
/// **One derivation, so the parity claim is checkable rather than asserted.**
/// `harness-adapters.md` section 2.3 requires the containment argv to be
/// byte-identical across drivers — a structured driver that dropped a deny flag
/// would be a capability grant by transport. Building the spec here from the
/// same values `build_agent_argv_ex` uses is what makes
/// `pi_rpc_argv_is_the_pty_line_plus_one_flag` a comparison of two real
/// derivations rather than of one derivation against a hand-copied list.
#[allow(clippy::too_many_arguments)]
pub fn pi_launch_spec(
    session_id: Option<&str>,
    group_dir: &std::path::Path,
    mcp_config: &std::path::Path,
    append_system_prompt: Option<&std::path::Path>,
    containment: super::Containment,
    model: &str,
    effort: &str,
) -> loomux_engine::harness::pi::LaunchSpec {
    loomux_engine::harness::pi::LaunchSpec {
        session_id: session_id.map(str::to_string),
        session_dir: super::pi_sessions_in(group_dir),
        mcp_config: mcp_config.to_path_buf(),
        append_system_prompt: append_system_prompt.map(std::path::Path::to_path_buf),
        // The trust flag is the containment question, and it is the SAME
        // question here as on the PTY arm: exactly one of the pair is always
        // emitted, so pi cannot raise its boot dialog on a pane loomux is
        // about to prompt.
        approve: !containment.denies_edits(),
        exclude_tools: containment
            .denies_edits()
            .then(|| super::PI_EDIT_DENY_TOOLS.to_string()),
        model: model.to_string(),
        thinking: effort.to_string(),
    }
}

impl super::OrchRegistry {
    /// Start a structured pane and register it, in place of the PTY rendezvous.
    ///
    /// **This path deliberately does not emit `orch-spawn-request`.** That event
    /// asks the FRONTEND to open a ConPTY and run the command line, and a
    /// frontend that does not yet know about structured panes would do exactly
    /// that — starting a second, real pi beside the one this function just
    /// spawned. The pane is backend-only until the DOM renderer (#2891 S4)
    /// mounts it from the roster `pane_kind`.
    ///
    /// The consequence, stated because it is a real limitation of this slice
    /// and not an oversight: until S4 lands, a structured pane runs, logs,
    /// takes deliveries and audits, and the human sees it on the board rather
    /// than in a pane cell.
    pub(super) fn spawn_structured_pane(
        &self,
        harness: loomux_engine::harness::Harness,
        entry: &super::AgentEntry,
        spec: &loomux_engine::harness::pi::LaunchSpec,
        program: &std::path::Path,
        prefix_args: &[String],
    ) -> Result<Arc<StructuredPane>, String> {
        use loomux_engine::harness::{EventLog, Harness};

        // R2 adds its arm here; there is nothing to fall through to, so an
        // unknown harness is an error rather than a silent PTY pane.
        if harness != Harness::Pi {
            return Err(format!(
                "no structured driver is wired for {harness:?} yet — this build drives pi only"
            ));
        }

        // `<group dir>/panes`. Built HERE and passed in, because joining a
        // `GroupId` onto a root has exactly one home (constraint 6) and
        // `EventLog::open` deliberately takes an already-resolved directory.
        let log_dir = self.group_dir(&entry.group).join("panes");
        let agent_seg = super::PathSegment::parse(&entry.id)
            .map_err(|e| format!("agent id is not a path segment: {e}"))?;
        // A log that cannot be opened is not a reason to refuse the pane: the
        // pane still works, and the events still reach the frontend and the
        // audit. Fail open with a breadcrumb, the same posture as the hook and
        // shim paths.
        let log = match EventLog::open(&log_dir, agent_seg) {
            Ok(l) => Some(l),
            Err(e) => {
                crate::obs::breadcrumb(
                    "structured-log-open-failed",
                    &format!("agent={} err={e}", entry.id),
                );
                None
            }
        };

        let pane = loomux_engine::harness::pi::PiPane::spawn_with(program, prefix_args, spec, log)
            .map_err(|e| format!("could not start the structured pane: {e}"))?;

        let pane_id = match self.app.lock_safe().clone() {
            // The one counter both pane kinds mint from, so a structured id and
            // a pty id can never collide.
            Some(app) => {
                use tauri::Manager;
                app.state::<crate::pty::PtyManager>().reserve_id()
            }
            // Headless tests have no `PtyManager`. Nothing can collide there
            // either, because nothing spawns a pty.
            None => self.next_test_pane_id(),
        };

        let sp = Arc::new(StructuredPane {
            pane: Box::new(pane),
            pane_id,
            group: entry.group.clone(),
            agent: entry.id.clone(),
            pending_ui: Mutex::new(Vec::new()),
            sent: AtomicU64::new(0),
        });
        self.structured.insert(Arc::clone(&sp));
        Ok(sp)
    }

    /// A pane id for a headless test, from a counter that shares nothing with
    /// `PtyManager` because in that configuration `PtyManager` does not exist.
    fn next_test_pane_id(&self) -> u32 {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as u32
    }

    /// The structured pane for this agent, if it has one.
    pub(super) fn structured_pane(&self, agent: &str) -> Option<Arc<StructuredPane>> {
        self.structured.get(agent)
    }

    /// The structured pane behind a pane id, looked up BEFORE any
    /// `PtyManager` access. See [`StructuredPanes::by_pane_id`].
    pub(super) fn structured_by_pane_id(&self, pane_id: u32) -> Option<Arc<StructuredPane>> {
        self.structured.by_pane_id(pane_id)
    }

    /// Is this pane id a structured one? Asked by `write_pty` so its refusal
    /// can say why rather than reporting a missing pty.
    pub fn pane_id_is_structured(&self, pane_id: u32) -> bool {
        self.structured.is_structured(pane_id)
    }
}

/// How long a batch may wait, and how big it may get, before it is emitted.
///
/// Section 5.6: at most one emit per pane per 16 ms, carrying at most 64 events
/// or 64 KiB, whichever binds first. The same shape `ptyout.rs` applies to
/// `pty-output`, and the reason is the same one — a producer-rate stream needs
/// its bound on the BACKEND, where the events are, rather than in a handler
/// that has already paid to receive them.
pub const BATCH_INTERVAL_MS: u64 = 16;
/// See [`BATCH_INTERVAL_MS`].
pub const BATCH_MAX_EVENTS: usize = 64;
/// See [`BATCH_INTERVAL_MS`]. Measured on the serialized JSON, which is what
/// actually crosses the wire.
pub const BATCH_MAX_BYTES: usize = 64 * 1024;

/// What one drained event asks the registry to do, decided purely so the
/// decision table is testable without a pane, a thread or a clock.
///
/// The drainer does all four of these for a single event where they apply —
/// they are not alternatives. `Ring` is a projection, `Frontend` is a
/// projection, `Audit` is the record, and the rest are state changes; section
/// 5.1 requires both projections be fed from the same drain in the same order,
/// so neither can show what the other has not seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRouting {
    /// Render to VT bytes for the pane ring (`get_output`, replay, thumbnails,
    /// `last_exit_tail`).
    pub ring: bool,
    /// Include in the next `orch-pane-event` batch.
    pub frontend: bool,
    /// Append to the group audit log.
    pub audit: bool,
    /// Record a usage sample from this event.
    pub usage: bool,
    /// Bind the session id reported at boot.
    pub binds_session: bool,
    /// Tear the pane down.
    pub exits: bool,
    /// Raise the dialog policy.
    pub dialog: bool,
}

/// Where one event goes.
///
/// A pure function over the event, so the routing table is pinned by tests
/// instead of by reading a thread body. The audit column is
/// `HarnessEvent::is_decision_grade` rather than a second list — section 4.3
/// states the split ONCE, and a second enumeration here is exactly the
/// divergence that rule exists to stop.
pub fn route(ev: &HarnessEvent) -> EventRouting {
    EventRouting {
        ring: true,
        frontend: goes_to_frontend(ev),
        audit: ev.is_decision_grade(),
        usage: matches!(ev, HarnessEvent::TurnEnded { usage: Some(_), .. }),
        binds_session: matches!(ev, HarnessEvent::Booted { .. }),
        exits: matches!(ev, HarnessEvent::Exited { .. }),
        dialog: matches!(ev, HarnessEvent::UiRequest { .. }),
    }
}

/// What the dialog policy decided for one `UiRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogOutcome {
    /// The pane waits; a needs-you row goes to the human.
    Park,
    /// Answered `Cancelled` at once, and a needs-you row goes to the human
    /// anyway so they still see what was asked.
    CancelNow(CancelReason),
}

/// Why a dialog was cancelled rather than parked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// An orchestrator or manager pane. #946: machine progress must never stop
    /// on human absence.
    RoleNeverParks,
    /// This pane already has [`MAX_PENDING_UI`] outstanding.
    TooManyPending,
}

impl CancelReason {
    pub fn label(self) -> &'static str {
        match self {
            CancelReason::RoleNeverParks => "role-never-parks",
            CancelReason::TooManyPending => "too-many-pending",
        }
    }
}

/// Decide what happens to a dialog, from the role and how many are already
/// outstanding.
///
/// Pure, and the whole of section 3.5 policy, so the asymmetry is pinned by a
/// table rather than by reading a thread. The cap is checked for BOTH role
/// classes: an orchestrator that never parks still cannot be allowed to
/// accumulate rows, and a role check that short-circuited the cap would be the
/// one-rule-per-input asymmetry a guard is supposed to avoid.
pub fn decide_dialog(role: super::Role, pending: usize) -> DialogOutcome {
    if pending >= MAX_PENDING_UI {
        return DialogOutcome::CancelNow(CancelReason::TooManyPending);
    }
    if parks_on_dialog(role) {
        DialogOutcome::Park
    } else {
        DialogOutcome::CancelNow(CancelReason::RoleNeverParks)
    }
}

/// The text a needs-you row carries for a dialog.
///
/// Pure so the wording is pinned by a test rather than read out of the drainer,
/// the same reason `needsyou::demo_text` is.
pub fn dialog_needs_you_text(agent: &str, p: &PendingUi, outcome: &DialogOutcome) -> String {
    let what = p
        .title
        .as_deref()
        .or(p.message.as_deref())
        .unwrap_or("(no prompt text)");
    let opts = if p.options.is_empty() {
        String::new()
    } else {
        format!(" [{}]", p.options.join(" / "))
    };
    match outcome {
        DialogOutcome::Park => {
            format!("{agent} is waiting on a dialog: {what}{opts}")
        }
        DialogOutcome::CancelNow(why) => format!(
            "{agent} raised a dialog that was cancelled ({}): {what}{opts}",
            why.label()
        ),
    }
}

/// The `orch-pane-event` payload (section 5.6).
#[derive(serde::Serialize, Clone)]
pub struct PaneEventBatch {
    pub group_id: String,
    pub agent_id: String,
    pub events: Vec<HarnessEvent>,
}

impl super::OrchRegistry {
    /// Drain one pane, forever, on its own thread.
    ///
    /// One thread per pane and one consumer of `events()`, because an
    /// `mpsc::Receiver` has exactly one and handing out a second would split
    /// the stream silently. Both projections (section 5.1) are fed from this
    /// one drain in this one order, so neither can show what the other has not
    /// seen.
    pub(super) fn drain_structured_pane(self: &Arc<Self>, pane: Arc<StructuredPane>) {
        let Some(rx) = pane.pane.events() else {
            // `events()` is take-once. A second drainer for one pane is a bug
            // in the spawn path, not a condition to recover from.
            crate::obs::breadcrumb(
                "structured-drain-twice",
                &format!("agent={} — events() already taken", pane.agent),
            );
            return;
        };
        let reg = Arc::clone(self);
        std::thread::spawn(move || reg.drain_loop(pane, rx));
    }

    fn drain_loop(
        self: Arc<Self>,
        pane: Arc<StructuredPane>,
        rx: loomux_engine::harness::EventRx,
    ) {
        use loomux_engine::harness::transcript::Renderer;

        let mut renderer = Renderer::for_harness(loomux_engine::harness::Harness::Pi, 80);
        let mut batch: Vec<HarnessEvent> = Vec::new();
        let mut batch_bytes = 0usize;
        let mut last_emit = std::time::Instant::now();

        loop {
            // A bounded wait rather than a blocking `recv`, so a batch that is
            // sitting under both caps still goes out on the interval instead of
            // waiting for the next event to push it over.
            let got = rx.recv_timeout(std::time::Duration::from_millis(BATCH_INTERVAL_MS));
            let mut disconnected = false;
            match got {
                Ok(ev) => {
                    let r = route(&ev);

                    // (a) the VT projection, first, so the ring never lags the
                    // frontend. Section 5.3 tee ordering.
                    if r.ring {
                        let bytes = renderer.render(&ev);
                        if !bytes.is_empty() {
                            self.append_structured_output(&pane, &bytes);
                        }
                    }
                    // (b) the audit, for the seven decision-grade variants.
                    if r.audit {
                        self.audit_structured_event(&pane, &ev);
                    }
                    // (c) state changes.
                    if r.binds_session {
                        self.bind_structured_session(&pane, &ev);
                    }
                    if r.usage {
                        self.record_structured_usage(&pane, &ev);
                    }
                    if r.dialog {
                        self.handle_dialog(&pane, &ev);
                    }
                    // (d) the frontend batch.
                    if r.frontend {
                        batch_bytes += serde_json::to_string(&ev).map(|s| s.len()).unwrap_or(0);
                        batch.push(ev.clone());
                    }
                    // (e) the teardown, LAST, so the exit itself has already
                    // been rendered, audited and batched before the pane goes.
                    if r.exits {
                        self.flush_pane_batch(&pane, &mut batch, &mut batch_bytes);
                        self.structured_pane_exited(&pane, &ev);
                        return;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => disconnected = true,
            }

            let due = last_emit.elapsed().as_millis() as u64 >= BATCH_INTERVAL_MS;
            let full = batch.len() >= BATCH_MAX_EVENTS || batch_bytes >= BATCH_MAX_BYTES;
            if !batch.is_empty() && (due || full || disconnected) {
                self.flush_pane_batch(&pane, &mut batch, &mut batch_bytes);
                last_emit = std::time::Instant::now();
            }

            if disconnected {
                // The child is gone and no `Exited` arrived — the pump ended
                // on a broken pipe rather than on a clean exit. Tear down on
                // the same path, with no exit code, rather than leaving a
                // roster row Running for ever.
                self.structured_pane_exited(&pane, &HarnessEvent::Exited { code: None });
                return;
            }
        }
    }

    fn flush_pane_batch(
        &self,
        pane: &StructuredPane,
        batch: &mut Vec<HarnessEvent>,
        bytes: &mut usize,
    ) {
        if batch.is_empty() {
            return;
        }
        let payload = PaneEventBatch {
            group_id: pane.group.as_str().to_string(),
            agent_id: pane.agent.clone(),
            events: std::mem::take(batch),
        };
        *bytes = 0;
        if let Some(app) = self.app.lock_safe().clone() {
            use tauri::Emitter;
            let _ = app.emit("orch-pane-event", &payload);
        }
    }

    /// Push rendered VT bytes into this pane ring.
    ///
    /// The ring is what `get_output`, termgrid replay, thumbnails and
    /// `last_exit_tail` read, and section 5.1 keeps it fed so those five
    /// consumers work unchanged on a pane with no PTY. A pane with no ring
    /// registered yet (headless tests) drops the bytes rather than failing the
    /// drain.
    fn append_structured_output(&self, pane: &StructuredPane, bytes: &[u8]) {
        if let Some(app) = self.app.lock_safe().clone() {
            use tauri::Manager;
            app.state::<crate::pty::PtyManager>()
                .append_structured_output(pane.pane_id, bytes);
        }
    }

    /// Record a decision-grade event on the group audit log.
    fn audit_structured_event(&self, pane: &StructuredPane, ev: &HarnessEvent) {
        let detail = serde_json::to_value(ev).unwrap_or(serde_json::Value::Null);
        self.audit(
            &pane.group,
            &pane.agent,
            "pane-event",
            serde_json::json!({ "agent": pane.agent, "event": detail }),
        );
    }

    /// Bind the session id pi reported at boot, killing the pane on a mismatch.
    ///
    /// A mismatch means `--session-id` did not take effect, so the pane is not
    /// the session loomux believes it is and every downstream record — usage,
    /// resume, transcript — would key on the wrong one. Fail closed.
    fn bind_structured_session(&self, pane: &StructuredPane, ev: &HarnessEvent) {
        let HarnessEvent::Booted { session, .. } = ev else {
            return;
        };
        let asked = self.agent(&pane.agent).and_then(|a| a.session_id);
        match (asked.as_deref(), session.as_deref()) {
            (Some(asked), Some(reported)) => {
                if !loomux_engine::harness::pi::session_ids_match(asked, reported) {
                    self.audit(
                        &pane.group,
                        &pane.agent,
                        "pane-session-mismatch",
                        serde_json::json!({ "asked": asked, "reported": reported }),
                    );
                    let _ = self.kill_structured_pane(&pane.agent);
                }
            }
            // Nothing was asked for, so there is nothing to disagree with:
            // record what the pane reported and carry on.
            (None, Some(reported)) => {
                if let Some(a) = self.agents.lock_safe().get_mut(&pane.agent) {
                    a.session_id = Some(reported.to_string());
                }
            }
            _ => {}
        }
    }

    /// Record a usage sample from a `TurnEnded`.
    ///
    /// Source `stream`, which is a new value in that field and deliberately
    /// distinguishable from the existing per-CLI ones: these figures are what
    /// the harness REPORTED, not what a transcript scrape inferred, and a
    /// reader that cannot tell those apart cannot judge either.
    fn record_structured_usage(&self, pane: &StructuredPane, ev: &HarnessEvent) {
        let HarnessEvent::TurnEnded { usage: Some(u), cost, .. } = ev else {
            return;
        };
        self.record_stream_usage(&pane.group, &pane.agent, u, cost.as_ref());
    }

    /// Apply the section 3.5 dialog policy to one `UiRequest`.
    fn handle_dialog(&self, pane: &StructuredPane, ev: &HarnessEvent) {
        let HarnessEvent::UiRequest {
            id,
            method,
            title,
            message,
            options,
            ..
        } = ev
        else {
            return;
        };
        let Some(entry) = self.agent(&pane.agent) else {
            return;
        };

        let pending = pane.pending_ui.lock_safe().len();
        let outcome = decide_dialog(entry.role, pending);

        let mut item = PendingUi {
            id: id.clone(),
            method: *method,
            title: title.clone(),
            message: message.clone(),
            options: options.clone(),
            needs_you: None,
            raised_ms: super::now_ms(),
        };

        // The human hears about it in BOTH arms. A cancelled dialog is still
        // something an agent asked for, and #946 is about not BLOCKING on the
        // human, never about hiding the question from them.
        let text = dialog_needs_you_text(&pane.agent, &item, &outcome);
        match self.raise_needs_you(
            &pane.group,
            &pane.agent,
            super::needsyou::RaiseRequest {
                kind: super::needsyou::Kind::Feedback,
                text,
                task: entry.task_id.clone(),
                urgency: super::needsyou::Urgency::Normal,
            },
        ) {
            Ok(raised) => item.needs_you = Some(raised.item.id),
            Err(e) => crate::obs::breadcrumb(
                "structured-dialog-needsyou-failed",
                &format!("agent={} err={e}", pane.agent),
            ),
        }

        match outcome {
            DialogOutcome::Park => {
                pane.pending_ui.lock_safe().push(item);
                self.audit(
                    &pane.group,
                    &pane.agent,
                    "pane-dialog-parked",
                    serde_json::json!({ "request": format!("{id:?}"), "method": format!("{method:?}") }),
                );
            }
            DialogOutcome::CancelNow(why) => {
                let settle = UiSettlement::policy(UiAnswer::Cancelled);
                let _ = pane.pane.answer_ui(id.clone(), settle.answer.clone());
                self.audit(
                    &pane.group,
                    &pane.agent,
                    "pane-dialog-cancelled",
                    serde_json::json!({
                        "request": format!("{id:?}"),
                        "method": format!("{method:?}"),
                        "reason": why.label(),
                        "by": format!("{:?}", settle.by),
                    }),
                );
            }
        }
    }

    /// Fold a reported usage figure into this agent stream total.
    ///
    /// **Cumulative from the harness, never a running sum of turns.** pi
    /// reports session-wide totals (`get_session_stats`), so adding two
    /// readings double-counts; the adapter already resolved that, and this
    /// stores the LATEST reading rather than accumulating.
    fn record_stream_usage(
        &self,
        group: &GroupId,
        agent: &str,
        u: &loomux_engine::harness::Usage,
        cost: Option<&loomux_engine::harness::Cost>,
    ) {
        let t = u.call_cumulative;
        self.stream_usage.lock_safe().insert(
            agent.to_string(),
            StreamUsage {
                input: t.input,
                output: t.output,
                cache_read: t.cache_read,
                cache_creation: t.cache_creation,
                cost_usd: cost.map(|c| c.usd),
                // Always estimated, and that is a fact about the vocabulary
                // rather than a default: `CostBasis` has exactly one variant,
                // `HarnessEstimate`, because every harness that reports a
                // dollar figure calls it a client-side estimate. Writing this
                // as a match on the basis would imply a "reported" case that
                // does not exist — so it is a `matches!` that fails to compile
                // if a second variant is ever added, rather than a `true` that
                // would silently keep claiming the same thing.
                estimated: cost.map(|c| {
                    matches!(c.basis, loomux_engine::harness::CostBasis::HarnessEstimate)
                }),
                updated_ms: super::now_ms(),
            },
        );
        let _ = group;
    }

    /// The stream usage for one agent, if any has been reported.
    pub(super) fn stream_usage_for(&self, agent: &str) -> Option<StreamUsage> {
        self.stream_usage.lock_safe().get(agent).cloned()
    }

    /// Deliver one turn to a structured pane.
    ///
    /// This is the whole of delivery for such a pane: no readiness wait, no
    /// question gate, no typing loop, no Enter, no submit confirmation. Every
    /// one of those exists because a PTY pane cannot be ASKED whether it took
    /// the bytes; `send` returns a receipt, so none of them has anything to do
    /// here.
    pub(super) fn deliver_structured(
        &self,
        pane: &StructuredPane,
        turn: loomux_engine::harness::Turn,
    ) -> Result<u64, String> {
        let receipt = pane.pane.send(turn)?;
        pane.sent
            .store(receipt.accepted_at_bytes, std::sync::atomic::Ordering::Relaxed);
        Ok(receipt.accepted_at_bytes)
    }

    /// Settle a dialog on behalf of the human.
    ///
    /// The trusted entry point. `by` is `Human` because reaching this function
    /// at all means the app own webview called it — that is what makes the
    /// source a property of the entry point rather than an argument anyone
    /// could supply.
    pub fn answer_pane_ui(
        &self,
        group: &GroupId,
        agent: &str,
        request: &str,
        answer: UiAnswer,
    ) -> Result<Option<String>, String> {
        let pane = self
            .structured
            .get(agent)
            .ok_or("this agent has no structured pane")?;
        if pane.group.as_str() != group.as_str() {
            return Err("that agent is not in this group".to_string());
        }
        let id = RequestId(request.to_string());
        let pending = pane
            .take_pending(&id)
            .ok_or("that dialog is not waiting for an answer")?;

        let settle = UiSettlement::human(answer);
        pane.pane.answer_ui(id, settle.answer.clone())?;
        self.audit(
            group,
            agent,
            "pane-dialog-answered",
            serde_json::json!({
                "request": request,
                "method": format!("{:?}", pending.method),
                "by": format!("{:?}", settle.by),
            }),
        );
        // The needs-you row this answer discharges, returned rather than
        // resolved HERE.
        //
        // The resolve-provenance type is pinned to `needsyou.rs` (which
        // defines it) and
        // `mod.rs` (whose trusted command supplies it) by a source scan; a
        // third file naming it is a NEW RESOLVING SURFACE, which that scan
        // says is "never accidental". This module has no business being one,
        // so the row id goes back to the caller and `orch_answer_pane_ui`
        // does the resolve beside the one that already lives there.
        Ok(pending.needs_you)
    }

    /// Tear a structured pane down: end the turn, close stdin, wait, kill,
    /// reap — the ladder `PiPane::drop` runs — then mark the roster row dead.
    ///
    /// `PtyManager::kill` is NOT involved and cannot be: this pane has no
    /// `pty_id`, which is the point of it never having one.
    pub fn kill_structured_pane(&self, agent: &str) -> Result<(), String> {
        let Some(pane) = self.structured.remove(agent) else {
            return Err("this agent has no structured pane".to_string());
        };
        // Interrupt first, so the turn in progress ends rather than being
        // abandoned mid-tool. The rest of the ladder is `PiPane::drop`, which
        // runs when the last `Arc` goes.
        let _ = pane.pane.interrupt();
        drop(pane);
        self.mark_dead(agent, None);
        Ok(())
    }

    /// The teardown the drainer runs when the pane reports its own exit.
    fn structured_pane_exited(&self, pane: &StructuredPane, ev: &HarnessEvent) {
        let code = match ev {
            HarnessEvent::Exited { code } => *code,
            _ => None,
        };
        self.audit(
            &pane.group,
            &pane.agent,
            "pane-exited",
            serde_json::json!({ "agent": pane.agent, "code": code }),
        );
        self.structured.remove(&pane.agent);
        self.mark_dead(&pane.agent, code.map(|c| c as u32));
    }
}

/// The usage a structured pane reported, as the collector stores it.
///
/// A stored value rather than a computed one, which is the difference between
/// this and every other source: a transcript source RE-READS a file the CLI
/// wrote, so the figures survive a restart on their own. A stream source has no
/// file to re-read — the events were the record, and they have gone by. Losing
/// them on restart is a real limitation of this slice, stated rather than
/// hidden, and closed by reading the pane own `EventLog` back (not this slice).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub cost_usd: Option<f64>,
    /// `None` when no cost was reported at all.
    pub estimated: Option<bool>,
    pub updated_ms: u64,
}

/// The `UsageSnapshot::source` value a structured pane writes.
///
/// A SEVENTH value in that field, and it is deliberately distinguishable from
/// the six transcript- and statusline-derived ones: these figures are what the
/// harness REPORTED over its own protocol, and a reader that cannot tell that
/// from a scrape cannot judge either.
pub const USAGE_SOURCE_STREAM: &str = "stream";
