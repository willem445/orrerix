//! The registry's data model: the live `AgentEntry` and `GroupInfo`, the
//! persisted `AgentRecord` and its companions (`UsageSnapshot`, `SessionRole`,
//! `RecordedOrchestration`, `Caller`), the enums they carry (`AgentStatus`,
//! `NameSource`, `Launch`, `SessionOrigin`, `KickoffOrigin`), agent-name
//! sanitising, and the group-record scan counter.
//! Design note: `docs/design/orchestration.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888):
//! `crate::modelstate`. Sibling files it calls: `compactnudge.rs`, `exits.rs`,
//! `guardrails.rs`, `persona.rs`.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Starting,
    Running,
    Dead,
}

/// Who last set an agent's display name — the precedence ladder for the pane
/// title / roster name (#95r). A rename applies only when its source ranks at
/// least as high as whoever set the current name: `Human` > `Orchestrator` >
/// `Default`. So the human's manual rename is never clobbered by the
/// orchestrator's `rename_agent` or the id-derived default, while the
/// orchestrator can still relabel an id-default (or its own earlier name).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NameSource {
    /// Minted from the agent id at spawn ("worker 2" for `w-2`).
    Default,
    /// Chosen by the orchestrator (a `spawn_agent` name, or `rename_agent`).
    Orchestrator,
    /// Typed by the human into the pane title (F2 / double-click).
    Human,
}

impl Default for NameSource {
    /// Legacy roster rows (written before the tier was persisted, #95r) carry a
    /// name but no source. Treat them as orchestrator-chosen: their non-empty
    /// name was picked deliberately, so a later `rename_agent` may still relabel
    /// it, and it never sits *below* an id-default. (Pre-95r human renames were
    /// frontend-only and never reached the roster, so none are being demoted.)
    fn default() -> Self {
        NameSource::Orchestrator
    }
}

impl NameSource {
    pub(in crate::orchestration) fn rank(self) -> u8 {
        match self {
            NameSource::Default => 0,
            NameSource::Orchestrator => 1,
            NameSource::Human => 2,
        }
    }
    pub(in crate::orchestration) fn as_str(self) -> &'static str {
        match self {
            NameSource::Default => "default",
            NameSource::Orchestrator => "orchestrator",
            NameSource::Human => "human",
        }
    }
}
/// Which kind of start a `create_group` call is (#222).
///
/// It exists for exactly one decision — **does the repo's `.loomux/workflow.yml`
/// get read?** — and the answer is "on a fresh launch, yes; on a resume, no".
///
/// The reason is consent, not caching. The roster the advanced orchestrator runs
/// is repo-authored, and the moment the human agrees to it is the launcher preview
/// they saw before hitting Create. A resume is not that moment: nobody is being
/// shown anything. So a `git pull` (or checking out a contributor's branch) between
/// launch and resume must not be able to hand a resumed group a reviewer, or a
/// persona, that its human never approved. The roster that comes back is the one in
/// `group.json` — the one they approved. Drift against the file on disk is audited
/// (`workflow-changed-since-launch`), never applied.
///
/// Note this is *not* the same question as "does `group.json` already exist" — a
/// human relaunching a group on a repo they have orchestrated before is a fresh
/// launch, preview and all, and must pick up a workflow file they have just edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launch {
    /// The human is at the launcher and has seen the roster preview.
    Fresh,
    /// A recorded orchestrator session is being reopened (the session browser).
    Resume,
    /// A standalone pane is being promoted to orchestrator (#407).
    ///
    /// A third value rather than either of the two above, because the consent
    /// moment splits by case and only this function knows which case it is
    /// (whether the group dir it resolved already holds a `group.json`):
    ///
    /// - **no group dir yet** — the promote modal IS the preview moment: the
    ///   human is told the repo declares a workflow and ticks the advanced box,
    ///   so the file is read exactly as at the launcher. Fresh semantics.
    /// - **reattaching a dormant group** — that group's roster was approved by
    ///   its own launcher preview, and its board, audit trail and backlog
    ///   outlived the session that consented to them. Re-reading the file here
    ///   would swap the approved roster under all of it on nothing but a
    ///   right-click. Resume semantics: pinned roster, drift audited.
    ///
    /// See the `reads_workflow_file` decision in [`OrchRegistry::create_group_ex`].
    Promote,
}

/// Where an orchestrator pane's CLI **session** comes from (#407) — the
/// session-level question, of which [`Launch`] is the group-level half.
///
/// These two used to travel as a `(Launch, Option<String>)` pair threaded
/// through `create_orchestration_group` → `register_orchestrator_pane`, and the
/// pair could spell combinations that are not real starts (`Launch::Fresh` with
/// a session id to resume; `Launch::Resume` with none). Worse, the session id's
/// mere presence was overloaded to answer three *different* questions at once —
/// which session flag the CLI gets, whether a session watcher is needed, and
/// which kickoff is typed — so there was no way to express the one start that
/// answers them differently: a promoted pane resumes its conversation (it is
/// the POC context the feature exists to keep) but has never seen the
/// orchestrator contract, so it needs the FULL kickoff a resume never gets.
///
/// | | `Fresh` | `Resume` | `StartFresh` | `Promote` |
/// |---|---|---|---|---|
/// | group semantics | fresh | pinned roster | pinned roster | fresh, or pinned on reattach |
/// | session flag | `--session-id <minted>` | `--resume <id>` | `--session-id <minted>` | `--resume <id>` |
/// | kickoff | full body | ledger notice | full body | full body + promote preamble |
/// | session watcher | per CLI | none (id known) | per CLI | none (id known) |
///
/// `StartFresh` is not new behavior — it is #412's cold restart of an existing
/// group's orchestrator, which the old pair spelled as the easily-misread
/// `(Launch::Resume, None)`. Naming it is the point: the pair had four
/// inhabited combinations and looked like it had two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionOrigin {
    /// A cold start of a new group: the CLI mints (or is handed) a brand-new
    /// session.
    Fresh,
    /// A recorded orchestrator conversation is being reopened as itself.
    Resume(String),
    /// A previously-launched group's orchestrator, cold-started on a NEW
    /// conversation (#412's `start_fresh`): the group is not being launched
    /// again — its approved roster stands — but the session is brand new.
    StartFresh,
    /// A standalone pane's conversation is being re-roled into an orchestrator
    /// (#407). The session is the human's own POC context — carried forward,
    /// never restarted.
    ///
    /// Carries the pane's CLI as well as its session id, because a session
    /// belongs to exactly one CLI and the roster this group ends up running is
    /// resolved later (and, on a dormant reattach, off disk): the pair is what
    /// lets `register_orchestrator_pane` refuse to hand a claude conversation
    /// to a CLI that has never heard of it.
    Promote { session_id: String, cli: String },
}

impl SessionOrigin {
    /// The group-level half of this start — what `create_group_ex` needs.
    pub fn launch(&self) -> Launch {
        match self {
            SessionOrigin::Fresh => Launch::Fresh,
            // #412 rev-17: a fresh CONVERSATION on an existing group is not a
            // fresh LAUNCH of it. Never re-derive a roster the human already
            // consented to.
            SessionOrigin::Resume(_) | SessionOrigin::StartFresh => Launch::Resume,
            SessionOrigin::Promote { .. } => Launch::Promote,
        }
    }

    /// The conversation to reopen, if any.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            SessionOrigin::Fresh | SessionOrigin::StartFresh => None,
            SessionOrigin::Resume(s) | SessionOrigin::Promote { session_id: s, .. } => Some(s),
        }
    }

    /// Does the CLI reopen an existing conversation (`--resume <id>`) instead
    /// of starting one? True for a promote as much as a resume — and it is the
    /// same fact that says there is no newly-minted session to go watching for
    /// (the id is already known).
    ///
    /// Spelled against the two variants that HAVE an id rather than as "not
    /// Fresh": `StartFresh` is a cold conversation too, and reading it as a
    /// resume would hand #412's restart `--resume <a uuid nothing has ever
    /// written>` — a pane that boots and immediately fails to find itself.
    pub fn resumes_session(&self) -> bool {
        matches!(self, SessionOrigin::Resume(_) | SessionOrigin::Promote { .. })
    }

    /// Does this orchestrator get the full kickoff contract (roster, lessons,
    /// guardrails, delivery id) rather than the short re-sync notice?
    ///
    /// The one decision `Promote` splits away from [`Self::resumes_session`]:
    /// a resumed orchestrator has the contract in its own transcript already,
    /// a promoted one has never seen it in its life.
    pub fn wants_full_kickoff(&self) -> bool {
        !matches!(self, SessionOrigin::Resume(_))
    }

    /// Audit-log spelling. `resume: true/false` alone can no longer say which
    /// of the four starts a pane was (a promote and a resume both resume).
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionOrigin::Fresh => "fresh",
            SessionOrigin::Resume(_) => "resume",
            SessionOrigin::StartFresh => "start-fresh",
            SessionOrigin::Promote { .. } => "promote",
        }
    }
}

/// Whether a kickoff is addressed to a pane that arrived by **promotion**
/// (#407) or by being spawned as what it is.
///
/// Deliberately not [`SessionOrigin`] itself: this is the only distinction the
/// kickoff text draws, it is asked of delegate panes too (which have no session
/// origin at all — the answer there is always `Normal`), and keeping it a
/// separate `Copy` value is what lets `kickoff_prompt`'s twenty existing call
/// sites stay untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KickoffOrigin {
    /// Spawned as this role. Every kickoff before #407.
    Normal,
    /// Re-roled in place from a standalone pane, carrying its conversation.
    Promoted,
}

impl KickoffOrigin {
    /// The paragraph a promoted orchestrator's kickoff opens with, and the
    /// empty string for every other kickoff — which is what keeps a launcher
    /// or resume kickoff byte-identical to before promotion existed.
    ///
    /// It says three things, because a session that has been talking to a human
    /// about a prototype for an hour needs all three: that the conversation
    /// above is still its own (the entire point of promoting rather than
    /// spawning), that its role and capabilities changed underneath it, and
    /// that the contract below wins over however it had been working.
    pub(in crate::orchestration) fn preamble(self) -> &'static str {
        match self {
            KickoffOrigin::Normal => "",
            KickoffOrigin::Promoted => {
                "Promoted to orchestrator: you were a standalone agent pane a moment ago, and \
                 this same conversation has been reopened with an orchestrator's role contract, \
                 MCP tools and group state attached. The work above is your own context — carry \
                 it forward, it is why you were promoted rather than replaced. Everything below \
                 is the role you now hold, and it supersedes how you were operating before.\n\n"
            }
        }
    }
}

#[derive(Clone)]
pub struct GroupInfo {
    /// #904: a `GroupId`, not a `String` — this is the field the great majority
    /// of group-scoped paths in the process are ultimately built from, so
    /// making it carry its own proof is what lets `group_dir` demand one.
    pub id: GroupId,
    pub repo: String,
    pub guardrails: Guardrails,
}

#[derive(Clone, Debug)]
pub struct AgentEntry {
    pub id: String,
    /// #904: validated. An agent record is read back off disk (`agents.json`)
    /// and fed straight into path joins, so this is a construction site as much
    /// as a command parameter is.
    pub group: GroupId,
    pub name: String,
    /// Who set `name` — the precedence tier for renames (#95r). See
    /// [`NameSource`] and [`OrchRegistry::rename_agent`].
    pub name_source: NameSource,
    /// The workflow block this agent was spawned from (#222) — its *identity*.
    /// `worker` for the built-in roster; `rev-security` for a declared block.
    /// This is what a gate, an edge or a `spawn_agent(block:)` names.
    pub block: workflow::BlockId,
    /// The agent's **capability class**, derived from its block's `kind`. Every
    /// structural guarantee (deny-flags, cwd rule, MCP tool scope) keys off
    /// this, and it can only ever be one of four values — see [`Role`].
    pub role: Role,
    pub token: String,
    pub status: AgentStatus,
    pub pty_id: Option<u32>,
    /// The pane id of a STRUCTURED pane (#2850 S3b), or `None`.
    ///
    /// Beside `pty_id` and never instead of it. Both come from one counter
    /// (`PtyManager::reserve_id`) so the queue, the drainer and `deliver_now`
    /// can key on either, but they name different things and a single field
    /// holding both would put a reserved id where every PTY-side lookup
    /// expects a real one. A structured pane keeps `pty_id: None` for its
    /// whole life; that is the guard, not a check.
    pub pane_id: Option<u32>,
    /// What kind of pane was spawned — `None` (absent) means `pty`, so every
    /// roster written before #2850 reads correctly.
    ///
    /// A RECORD of what was spawned, not a control: nothing reads it to decide
    /// how to drive a pane, only to render it and to answer `PaneKind` for a
    /// pane this process did not spawn (`harness-adapters.md` section 2.4).
    ///
    /// Persisted by `persist_agent_record`, which builds its JSON by hand --
    /// this struct derives no serde, so a field reaches `agents.json` only by
    /// being written there.
    pub pane_kind: Option<String>,
    /// The PARENT session this agent's session was forked from (#3318 F2),
    /// or `None` for every agent that was not born by `fork_session`.
    ///
    /// A session id, not an agent id, and deliberately: the parent AGENT may
    /// be long dead by the time anyone reads this, and a session is what the
    /// session browser, the audit row and the vendor's own store all key on.
    /// The parent agent's id is in the `agent-fork` audit row beside it.
    /// Provenance only — nothing reads it to decide anything.
    pub forked_from: Option<String>,
    pub task: String,
    /// The board task this spawn was bound to (#1273), when the orchestrator
    /// named one (`spawn_agent(task_id:)`). Metadata in the task-hierarchy §7
    /// sense: the ONLY thing it does is put that row's grounding links into
    /// this agent's kickoff (`grounding_note`). Nothing gates on it, and an id
    /// here is not a claim on the row — recording assignee/session from the
    /// binding is the noted #1273 follow-up, deliberately not done here.
    ///
    /// In-memory only, like the rest of this struct: a session rejoin
    /// re-spawns with no binding, so a rejoined pane's kickoff carries no
    /// grounding section.
    pub task_id: Option<String>,
    /// The agent CLI's conversation session id. For Claude, loomux assigns
    /// it at spawn (`--session-id`), so a finished worker's session can be
    /// resumed later for follow-ups on its task without a cold start.
    pub session_id: Option<String>,
    /// Working directory the pane runs in; resume must reuse it so the
    /// resumed session's file operations land where the work happened.
    pub cwd: String,
    /// The git branch this agent's work is actually associated with (#1):
    /// `Some` for a dedicated worktree spawn and for a shared-repo Worker
    /// told to create it, `None` for the orchestrator/reviewer(no worktree)/
    /// planner, none of which have a "their own branch" the way a Worker
    /// does. Persisted so the session browser can show it without guessing.
    pub branch: Option<String>,
    /// Unix-ms this worker/reviewer became idle (spawned without a task, or
    /// reported done/blocked); `None` while it has work or for the
    /// orchestrator. The idle reaper (`idle_kill_minutes`) reads this.
    pub idle_since_ms: Option<u64>,
    /// Unix-ms this agent was registered (spawn time). Drives the per-agent
    /// and group uptime shown in the lifecycle summary; unaffected by idle.
    pub started_ms: u64,
    /// Watchdog: Unix-ms of this agent's last observed activity — terminal
    /// output growth or a report/message. Silence is measured from here.
    /// Seeded at spawn and whenever work is (re)assigned. See
    /// `watchdog_should_notify`.
    pub last_progress_ms: u64,
    /// #535: Unix-ms this agent last called ANY loomux MCP tool, stamped once
    /// at the `tools/call` dispatch funnel (`mcp::dispatch`) — so it means
    /// exactly "this agent's own process reached loomux", for every tool and
    /// every role. `0` = never called; monotone, never rewound.
    ///
    /// This is an **acknowledgment** clock, and deliberately not any of the
    /// three clocks that already exist:
    /// - `last_progress_ms` above is the two signals folded together and is
    ///   rewritten to `now` by pty *output growth* in three separate ticks
    ///   (`watchdog_tick`, `idle_tick_tick`, `compact_nudge_tick`). Repaint
    ///   noise above the activity floor advances it, so it cannot answer
    ///   "did the agent act", only "did the pane emit".
    /// - `note_agent_activity` (which writes `last_progress_ms`) is stamped
    ///   from exactly ONE tool, `message_orchestrator`, and is an explicit
    ///   no-op for `Role::Orchestrator` — but an orchestrator compacts and
    ///   gets re-grounded like anything else, so a watchdog-scoped signal
    ///   cannot serve this.
    /// - `DeliveryConfirmation` is loomux observing its own paste, which is
    ///   the unreliable signal #535 exists to stop relying on.
    ///
    /// Read by `reinject_acked`/`reinject_disposition`. In-memory only, seeded
    /// `0` on every construction: a restart spawns fresh agents rather than
    /// reviving old ones (the rev-4 B1 lesson in `compact_nudge_tick`), and a
    /// `0` seed cannot be mistaken for a fresh ack the way a `now_ms()` seed
    /// or a persisted value could. #524 made the *id* durable, not the
    /// agent — nothing here becomes carry-over-able because of it.
    pub last_mcp_activity_ms: u64,
    /// #496 hardening: Unix-ms of this agent's last OUTPUT-based quiet-clock
    /// reset only — i.e. the last time `last_progress_ms` above moved because
    /// the pane produced real output, never because human input deferred it.
    /// `last_progress_ms` is the two clocks folded together (output resets
    /// it; input can also push it forward, belt-and-suspenders); this field
    /// is the only place that still answers "how long since real progress,
    /// independent of how much input has arrived since" — exactly what
    /// `Guardrails.idle_tick_input_defer_max_minutes` clamps the input fold
    /// against, so perpetual input can defer the tick only so far past the
    /// orchestrator's last actual output, never forever. Meaningful only for
    /// the orchestrator's idle-tick clock (mirrors `last_progress_ms`'s own
    /// scope); the watchdog never reads it. See `idle_tick_tick`.
    pub last_output_progress_ms: u64,
    /// Watchdog/idle-tick: last observed value of the pane's monotonic pty
    /// output counter, so a tick can tell whether the CLI has emitted anything
    /// since the previous one even when the output ring is saturated. Exclusive
    /// to watchdog (non-orchestrator working agents) and idle-tick (the
    /// orchestrator in an autonomous group) — the two never watch the same
    /// agent (partitioned by role), so sharing this baseline between them is
    /// safe. Compact-nudge does NOT use this field — see
    /// `compact_nudge_last_output_total`; the two CAN watch the same agent
    /// (default: the orchestrator), and an earlier revision shared this field
    /// between them, which meant whichever tick polled first each cycle
    /// rebaselined it and starved the other's activity detection (rev-24
    /// review finding) — hence the separate baseline.
    pub last_output_total: u64,
    /// Watchdog anti-nag latch: set once a stall notice has been delivered for
    /// the current stall, cleared when the agent produces output/reports again.
    pub watchdog_notified: bool,
    /// #852: set alongside `watchdog_notified` when a stall trips while the
    /// agent holds a live `notify_when` watch — but that trip does NOT
    /// deliver a notice, it suppresses one, because the agent is plausibly
    /// waiting on its own registered CI check rather than stuck. Watched for
    /// on the NEXT tick: once the agent no longer holds any live watch (it
    /// fired, expired, failed-out, or was cancelled), `watchdog_tick` clears
    /// this latch, clears `watchdog_notified`, and resets `last_progress_ms`
    /// to that tick's `now` — a fresh full stall window, so the notice only
    /// ever fires for a stall the agent is STILL silent through after its
    /// watch resolved, never immediately off whatever was left of the old
    /// (already-expired) window. Also cleared by the other two
    /// `watchdog_notified`-clearing sites (`note_agent_activity`,
    /// `set_agent_idle(false)`) — any sign of life must retire a suppression
    /// latch the same tick it retires the notice latch, or a later tick could
    /// take the watch-resolved branch on a latch describing a stall that no
    /// longer exists (review finding on #852).
    pub watchdog_watch_suppressed: bool,
    /// The same latch for #3040 N2's `driven-lane` suppression: set when a stall
    /// is demoted because a live review drive owned the pane, and read on the
    /// first tick where no drive owns it any more, to hand the pane a fresh stall
    /// window instead of leaving it latched for good — the shape #852 gave the
    /// watch arm, which the drive arm shipped without (rev-std round 1, N2).
    ///
    /// A SEPARATE flag rather than one shared "suppressed" bit, because the two
    /// are cleared by different evidence — a watch resolving, and a drive ending
    /// — and one bit would let either event clear the other's suppression.
    pub watchdog_drive_suppressed: bool,
    /// Autonomous idle-tick latch (#83), meaningful only for the orchestrator:
    /// set when an idle-tick notice is delivered, cleared when the pane produces
    /// output again (the orchestrator acted on the tick). Mirrors
    /// `watchdog_notified` — one notice per idle window. `last_output_total` /
    /// `last_progress_ms` above double as the idle-tick output counter / quiet
    /// clock for the orchestrator, which the watchdog never touches.
    pub idle_tick_notified: bool,
    /// Compact-nudge (#287) anti-nag latch, meaningful only for a role in the
    /// group's `compact_nudge_roles`: set once `/compact` has been pasted for
    /// the current quiet window, cleared when the pane produces real output
    /// again (per compact-nudge's OWN observation — see
    /// `compact_nudge_last_output_total`). `last_progress_ms` above is still
    /// the shared quiet clock both idle-tick and compact-nudge read/advance —
    /// deliberately the same idleness signal, not a second one — but each
    /// tick now detects "did real growth happen" from its own baseline before
    /// touching it, so one tick's poll can no longer consume the growth out
    /// from under the other. See `compact_nudge_tick`.
    pub compact_nudge_notified: bool,
    /// Compact-nudge's OWN baseline pty output-total, independent of
    /// `last_output_total` (rev-24 review fix). Idle-tick and compact-nudge can
    /// both be watching the SAME agent (default config: both target the
    /// orchestrator), unlike watchdog/idle-tick which never overlap — so they
    /// cannot share one rebaselined counter without whichever tick polls first
    /// each cycle consuming the growth and starving the other's activity
    /// detection (and, transitively, its anti-nag latch, which never clears
    /// again). Each tick keeping its own last-seen snapshot of the same
    /// monotonic pty counter is the standard fix for two independent readers
    /// of one counter — a Kafka-consumer-offset shape, not a new signal.
    pub compact_nudge_last_output_total: u64,
    /// Compact-nudge (#328): set by the self-scoped `request_compact` MCP
    /// tool call — the calling agent asked to be compacted at its next idle
    /// moment. Consumed (cleared) the instant `compact_nudge_tick` fires the
    /// `/compact` paste for it; unlike `compact_nudge_notified` this is a
    /// one-shot REQUEST, not an anti-nag latch, and firing on it bypasses the
    /// `compact_nudge_minutes` threshold entirely (the agent's own signal IS
    /// the trigger) while still respecting the CLI gate and the shared
    /// per-hour cap.
    pub compact_requested: bool,
    /// Compact-nudge (#328): true from the moment a compact has been
    /// initiated for this pane — by loomux pasting `/compact` (heuristic or
    /// requested fire) or by detecting a human typed it manually
    /// (`human_typed_compact_detected`) — until the mandatory post-compact
    /// re-injection fires. The single piece of state the "did compaction just
    /// finish" detector needs, shared by all three trigger paths so the
    /// detection logic lives once. See `compact_nudge_tick`.
    pub compact_pending: bool,
    /// Compact-nudge (#328): while `compact_pending`, latches true the first
    /// time a tick observes REAL output growth (compaction actively running
    /// or its own output rendering) — the busy half of the busy-then-quiet
    /// edge `compact_nudge_tick` uses to recognize "compaction just
    /// finished" without parsing Claude's own completion text. Reset when the
    /// re-injection fires (or `compact_pending` clears).
    pub compact_seen_busy: bool,
    /// Production bug fix (PR #329 delta review): the agent's context-token
    /// reading (`usage::latest_context_tokens`, via `agent_context_signals`)
    /// captured the moment `compact_pending` most recently became `true` via
    /// an INFERENCE trigger path (banner or manual detection — see
    /// `compact_pending_trusted`). `None` when no reading was available at
    /// that moment (no session yet). Live production evidence showed
    /// busy-then-quiet ALONE is not proof a compaction ran for these two
    /// paths — an orchestrator asked to explain/discuss the compact-nudge
    /// feature itself produced output whose growth-then-quiet cycle
    /// repeatedly satisfied the banner detector with no real compaction ever
    /// happening, delivering the reinjection notice on a loop that only grew
    /// context. This baseline lets the resolver REQUIRE evidence a real
    /// compaction occurred before trusting an inference-path arm; see
    /// `inferred_compaction_confirmed`. A SECOND live-evidence round (rev-42)
    /// showed the naive fix (requiring this for every path uniformly) traded
    /// that loop for a silent deadlock on the loomux-INITIATED path instead —
    /// `usage::latest_context_tokens`'s drop is a next-turn phenomenon
    /// (proven against a real transcript, see `usage::tests::
    /// real_transcript_proves_the_token_drop_is_a_next_turn_phenomenon_
    /// rev42_q1`), and on that path the only "next turn" is the reinjection
    /// itself — gating it behind its own evidence is a deadlock, not a race.
    /// So this baseline (and the marker-count baseline below) is only ever
    /// CONSULTED when `compact_pending_trusted` is `false`; for a trusted arm
    /// it's still captured (uniform arming code, cheap) but ignored at
    /// resolution. Always cleared the instant a pending state resolves
    /// (confirmed or not) — never carried across trigger cycles.
    pub compact_pending_baseline_tokens: Option<u64>,
    /// Production bug fix (rev-42 delta): companion baseline to `compact_
    /// pending_baseline_tokens`, using `usage::compact_boundary_count`
    /// instead of a token reading — the CLI's own structural "a compaction
    /// just completed" marker, observable the INSTANT compaction finishes,
    /// with no next-turn dependency at all (unlike the token drop). Confirmed
    /// via `inferred_compaction_confirmed` if a NEW boundary appears since
    /// this baseline (`current > baseline`) — a strictly better, faster
    /// signal than the token drop for the inference paths, kept alongside it
    /// (OR'd together) as defense-in-depth rather than a full replacement.
    pub compact_pending_baseline_marker_count: Option<u64>,
    /// Production bug fix (rev-42 delta): `true` when the CURRENT `compact_
    /// pending` arm came from a LOOMUX-INITIATED trigger (the heuristic timer
    /// or an agent's own `request_compact`) — the path where loomux itself
    /// decided to paste `/compact`, which was never the false-positive
    /// source the production incident lived in (that was the banner
    /// detector's mention-vs-real-event confusion). `false` for the two
    /// INFERENCE paths (banner, manual `/compact` detection), which CAN be
    /// wrong about a compaction having happened at all and therefore still
    /// require `inferred_compaction_confirmed` before resolving to a
    /// reinjection. Read only at resolution time, alongside the two
    /// baselines above; reset to `false` (its inert default) the instant a
    /// pending state resolves.
    pub compact_pending_trusted: bool,
    /// Production bug fix (rev-42 delta, round 2): `Some(now)` from the tick a
    /// reinjection was DECIDED (confirmed or trusted) and its delivery fired,
    /// until that delivery is confirmed or the attempt budget is exhausted.
    /// `None` while still deciding whether to reinject at all, and again once
    /// the arm fully resolves. Live re-demo evidence (PR #329 round 5) showed
    /// the one-shot latch being consumed at the DECISION rather than at
    /// confirmed delivery is itself a gap: `deliver_prompt` is fire-and-
    /// forget (D1's finding, see the design doc), so a decision to reinject
    /// was previously treated as done the instant `deliver_prompt` was
    /// *called*, with no feedback loop if that delivery never actually
    /// landed. This field is what makes the latch wait for real evidence: see
    /// `compact_nudge_tick`'s resolver for how it's consulted against
    /// `delivery_confirmations`.
    pub compact_reinject_attempted_ms: Option<u64>,
    /// Production bug fix (rev-42 delta, round 2): 1-indexed count of
    /// reinjection delivery attempts made for the CURRENT arm (1 = the first
    /// fire, not yet a retry). Bounded by `MAX_REINJECT_ATTEMPTS` — a
    /// reinjection that still hasn't confirmed after that many tries is
    /// abandoned (audited, latch released) rather than wedging the state
    /// machine so no future compaction can ever arm again for this agent.
    /// Reset to `0` whenever no reinjection is in flight.
    pub compact_reinject_attempts: u32,
    /// #535 anti-nag latch: this attempt's busy-turn deferral has already been
    /// audited. The compact poll runs every 10s while any arm is open
    /// (`compact_nudge_poll_interval`), so without this a single deferral would
    /// write dozens of identical audit lines. Same shape as
    /// `watchdog_notified` / `compact_nudge_notified`.
    ///
    /// Scoped to ONE attempt, not to the whole reinjection: cleared wherever
    /// `compact_reinject_attempted_ms` is (re)set or released, so a retry's own
    /// deferral is still audited rather than silenced by its predecessor's.
    pub compact_reinject_busy_deferred: bool,
    /// Production bug fix (#410, PR #329 round 6): Unix-ms the CURRENT
    /// `compact_pending` arm started (set at each of the three arm sites,
    /// cleared on any resolution — discard, handoff into the reinjection-
    /// confirmation phase, or this field's own timeout). Live evidence (a
    /// user demo, testbed group `loomux-testbed-cc077f09`) showed a
    /// `request_compact` call answered "a compact is already in flight for
    /// this pane" for 10+ minutes: an inference arm (almost certainly the
    /// auto-compact banner, given the session's accumulated size across many
    /// testing rounds) kept re-arming and correctly resolving to a DISCARD
    /// each time (D4 held — no reinjection loop), but the repeated cycling
    /// meant `compact_pending` was essentially never open long enough for
    /// the user's queued request to win the race (closed by the arm-site
    /// reorder below) or, in the more literal #410 scenario, an arm that
    /// simply never reaches a busy-then-quiet observation at all (a stalled
    /// agent, or a compaction that never actually starts) stays pending
    /// forever with no bound. This field lets the resolver force an
    /// abandon — audited, latch released — once an arm has been open past
    /// `ARM_PENDING_TIMEOUT_MS`, symmetric to `compact_reinject_attempted_
    /// ms`'s bound on the delivery-confirmation phase.
    pub compact_pending_armed_ms: Option<u64>,
    /// Lifecycle-panel surfacing (PR #329 round 6): the reason the most
    /// recent "something was lost" terminal outcome fired — `"arm-timeout"`
    /// (#410) or `"reinjection-abandoned"` (a stuck delivery past its retry
    /// budget) — paired with `compact_last_lost_ms`. Distinct from an
    /// ordinary discard (a harmless false-positive, not worth lingering
    /// visibility for): these two ARE worth surfacing to a human watching
    /// the lifecycle panel, but only briefly (see `compaction_status`'s
    /// recency window) — an old one would be misleading noise long after
    /// the agent moved on.
    pub compact_last_lost_reason: Option<String>,
    /// Unix-ms `compact_last_lost_reason` was set. See its doc.
    pub compact_last_lost_ms: Option<u64>,
    /// #546: WHICH evidence resolved the most recent re-grounding — see
    /// [`ReinjectAck`] for what each one proves and, more to the point, what
    /// it does not. Paired with `compact_last_ack_ms`; the same value the
    /// resolution's audit line records as `source`.
    ///
    /// It is surfaced (not merely audited) because the two prove different
    /// things and only one of them is about our paste.
    /// [`ReinjectAck::LivenessOnly`] proves the agent is alive and executing
    /// its contract — it does NOT prove the re-grounding was READ, which is
    /// #546's whole finding: a genuinely lost paste on an agent that is busy
    /// for some other reason resolves this way and nothing else says so. A
    /// human glancing at the lifecycle panel is the last reader who can catch
    /// that, and they can only catch it if the panel says which evidence
    /// closed the phase.
    ///
    /// A [`ReinjectAck`] rather than the `&'static str` this started as
    /// (#588): the value has to be re-stated in the audit action name, the
    /// badge label and the badge tooltip, and a bare string made each of those
    /// re-derive the meaning in its own words — which is how the `acked`
    /// overclaim survived in three of them at once.
    pub compact_last_ack: Option<ReinjectAck>,
    /// Unix-ms `compact_last_ack` was set. See its doc.
    pub compact_last_ack_ms: Option<u64>,
    /// Lifecycle-panel surfacing (PR #329 round 6): the last context-token
    /// reading `compact_nudge_tick` observed for this agent (from `usage::
    /// compaction_signal_in`, the SAME bounded tail-read the compact-nudge
    /// background tick already does every `IDLE_TICK_INTERVAL` — this just
    /// caches its result so `group_summary` can surface it on the UI's own,
    /// much more frequent poll cadence WITHOUT an extra transcript read per
    /// poll). `None` until the first successful reading; never cleared once
    /// set (a transient read miss keeps the last-known value rather than
    /// blanking the display).
    pub last_context_tokens: Option<u64>,
    /// Production bug fix (PR #329 round 7): the model the transcript
    /// reading behind `last_context_tokens` came from — cached the same way
    /// (from `run_compact_nudge`'s own `agent_context_signals` read, not a
    /// per-poll re-read) so `group_summary` can derive the ACTUAL context-
    /// window size (`effective_context_window_tokens`) instead of assuming a
    /// flat one. `None` until the first reading; never cleared once set.
    pub last_context_model: Option<String>,
    /// #993 S1: the context window the CLI REPORTED alongside that reading
    /// (Claude's status-line `context_window_size`), cached by the same tick so
    /// `group_summary`'s percent climbs the same ladder the escalation does.
    /// Follows each reading, `None` included — see `run_compact_nudge`.
    pub last_context_window: Option<u64>,
    /// #993 S2b: `last_context_window` is a lower bound the CLI printed
    /// rounded (`CompactionSignal::window_rounded`), so the ladder labels it
    /// `reported-rounded`. Follows each reading exactly as the window does.
    pub last_context_window_rounded: bool,
    /// #993 S3: latest observed effort and context source, cached with the
    /// token reading so group-summary polling performs no artifact reads.
    /// The effort is `CompactionSignal::observed_effort`, never a launch
    /// fallback; the source is typed so the published window can ask it
    /// whether the table rung applies (`modelstate::published_window`).
    pub last_context_effort: Option<String>,
    pub last_context_source: Option<crate::modelstate::ContextSource>,
    /// Production bug fix (PR #329 round 7): INFERENCE arms (banner, manual
    /// detection — never the loomux-initiated/trusted arm, which needs no
    /// inference at all) may only arm while `now >= this`. Live demo
    /// evidence: minutes after a REAL, confirmed compaction resolved, a
    /// second inference arm formed and discarded — most likely loomux's OWN
    /// `/compact` paste (or the reinjection notice that followed) still
    /// sitting in the bounded output tail, re-satisfying `human_typed_
    /// compact_detected`/`auto_compact_banner_detected`, possibly compounded
    /// by the pane's own post-compact discussion of the test that just ran.
    /// Not a reinjection LOOP (D4 held — every arm resolved to a correctly-
    /// audited discard) but noisy and conceptually wrong: a detector
    /// inferring a NEW compaction from an echo of loomux's own recent
    /// activity, or from the immediate aftermath of one it already handled.
    ///
    /// `0` (never gates) until extended, in two places, to `X +
    /// INFERENCE_ARM_COOLDOWN_MS`: (a) whenever a CONFIRMED delivery `from
    /// is this app's own` lands for this agent's pty (`X` = that delivery's
    /// `submit_sent_ms` — the provenance principle: loomux's own paste's
    /// echo must never satisfy loomux's own detectors, whether that paste
    /// was a `/compact` command or a reinjection/escalation notice), and
    /// (b) whenever `compact_pending` reaches a terminal resolution
    /// (discard, arm-timeout, reinjection confirmed, reinjection abandoned;
    /// `X` = `now`) — the immediate post-compact conversation window.
    /// Deliberately NOT also seeded at construction/resume: live forensics
    /// couldn't attribute the analogous "arms shortly after a restart"
    /// incident to any delivery THIS agent's entry ever made (a fresh
    /// `AgentEntry` has made none yet) — a construction-time grace period
    /// would just be a blunt "distrust every fresh session" hack, not this
    /// principle. Never gates the trusted arm: loomux has positive
    /// knowledge it initiated that one itself, so there is no inference for
    /// a stale echo to fool.
    pub compact_inference_guard_until_ms: u64,
    /// #417: the freshest PreCompact hook marker mtime already acted on for
    /// this agent (see `read_hook_marker_ts`) — bookkeeping so a tick doesn't
    /// re-arm on a marker it already consumed. `None` until the first
    /// hook-sourced arm.
    pub compact_hook_precompact_seen_ms: Option<u64>,
    /// #417: companion bookkeeping for the SessionStart(compact) hook marker
    /// — DIRECT proof a compaction just finished (Claude Code itself
    /// restarted the session specifically because of one), independent of
    /// whether PreCompact fired/was configured at all.
    pub compact_hook_sessionstart_seen_ms: Option<u64>,
    /// #413 S5: companion bookkeeping for Claude's `PostCompact` hook marker —
    /// the freshest one already consumed, in the marker's own (mtime) clock,
    /// exactly like the two fields above.
    pub compact_hook_postcompact_seen_ms: Option<u64>,
    /// #413 S5: the tick `now` at which a fresh, not-yet-consumed `PostCompact`
    /// marker was FIRST seen — the start of its settle window
    /// (`POSTCOMPACT_SETTLE_MS`), during which a `SessionStart(compact)` marker
    /// for the same compaction may still land and resolve it natively. Kept on
    /// the tick's clock rather than compared against the marker's mtime, so the
    /// window is bounded even when a marker's mtime is skewed into the future.
    /// Cleared whenever the marker is consumed or is gone.
    pub compact_hook_postcompact_first_seen_ms: Option<u64>,
    /// #417: how the CURRENT (or most recently resolved) `compact_pending`
    /// arm was armed — `Some("hook")` for a PreCompact/SessionStart marker
    /// (trusted evidence, no inference gate needed), `None` for the
    /// pre-existing tiers (the loomux-initiated/heuristic/manual/banner
    /// arms already distinguish themselves via `compact_pending_trusted`
    /// and don't need a second label). Surfaced on the lifecycle chip so a
    /// human can tell a hook-confirmed compaction from an inferred one.
    /// Cleared alongside the other arm state on any resolution.
    pub compact_pending_evidence: Option<&'static str>,
    /// rev-4 review (N3): `true` from the moment the SessionStart(compact)
    /// hook marker armed/confirmed THIS pending cycle — the ONE signal that
    /// means Claude Code already delivered native `additionalContext`
    /// re-grounding for it (the generic hook script only emits that JSON
    /// from its `sessionstart-compact` branch, never `precompact`). When
    /// true at resolution time, `compact_nudge_tick` skips loomux's own
    /// reinjection entirely — pasting it anyway would be a duplicate
    /// re-grounding, spending exactly the context tokens native delivery
    /// exists to save. `false` for a PreCompact-only arm (no SessionStart
    /// marker seen yet for this cycle) and every pre-#417 path, so loomux's
    /// reinjection remains the sole channel — the correct fallback, not a
    /// double-delivery, when native re-grounding was never actually sent.
    pub compact_hook_native_notice_delivered: bool,
    /// #417 correction round 5, promoted from a lossy bool to
    /// [`ContractCarrier`] in round 8 (rev-16 review, N1/N2 — the bool
    /// version of this exact doc paragraph was the staleness rev-16 named
    /// as a 3-round pattern on this PR, which is the whole reason it's an
    /// enum now): mirrors `PersonaInject::contract_carrier` as decided at
    /// THIS agent's own spawn. `reinject_shape` uses this to choose the
    /// notice's shape — see that function's and `ContractCarrier`'s own
    /// docs for the three states. Set once at spawn, never mutated — a
    /// persona/workflow-file edit takes effect on the NEXT spawn, same as
    /// every other `persona_inject` output.
    pub contract_carrier: ContractCarrier,
    /// Compact-nudge (#328): Unix-ms this agent's `set_state` call was last
    /// observed (0 = never). Self-scoped, stamped by the `set_state` MCP
    /// handler on the CALLING agent's own entry — meaningful only for the
    /// orchestrator (the only role `set_state` is available to). Backs
    /// `request_compact`'s pre-compact offload-checklist warning (a soft nudge
    /// — never a block — that state was likely persisted before compacting).
    pub last_state_write_ms: u64,
    /// Compact-nudge (#328) context-escalation anti-nag latch: set once
    /// `compact_escalation_notice` has fired for the current above-threshold
    /// window, cleared once context% drops back under the group's
    /// `compact_context_threshold_percent` (e.g. after a compact lands).
    pub compact_escalation_notified: bool,
    /// The orchestrator idle-compact backstop's latch (#3407): set when
    /// `cache_idle_nudge_tick` delivers its notice, released only on evidence
    /// the idle stretch ended — something went in flight, or the context fell
    /// under the compact floor (a compact landed). NOT released by the pane's
    /// own output: the dominant output after a nudge is the orchestrator
    /// answering it, and releasing on that would re-nudge an orchestrator that
    /// read the notice and chose not to compact every TTL, forever.
    pub cache_idle_nudge_latched: bool,
    /// Idle-tick intake gate (#332), meaningful only for the orchestrator:
    /// when the gate SKIPPED a would-have-fired tick (nothing new, no other
    /// wake reason, fallback not due), this is the Unix-ms the latch above
    /// may be auto-cleared WITHOUT the orchestrator producing output — set to
    /// `now + intake_poll_minutes` at skip time, so the gate is re-considered
    /// no more often than the poller could actually have refreshed its
    /// findings. `0` = no pending re-arm (the latch clears only on real
    /// output, the pre-#332 behavior — the case when gating is off, or after
    /// a genuine fire). See `idle_tick_tick`.
    pub idle_tick_skip_rearm_ms: u64,
    /// The actual agent CLI a **solo** pane (`role == Role::Solo`) is
    /// running, e.g. `"codex"`/`"gemini"` — the group-guardrails CLI
    /// resolution (`Guardrails::cli_for_block`) that every orchestration-group
    /// agent's delivery/usage code paths use cannot answer this for a solo
    /// pane, since `__solo__` is one shared group across panes running
    /// arbitrary, possibly-different CLIs. `None` for every non-solo agent —
    /// and that is the same statement as the sentence below, now that the
    /// resolver named above is the per-BLOCK one: a non-solo agent's CLI always
    /// comes from its block (#2167 made those two halves agree; this doc used
    /// to name `cli_for`, the per-CLASS resolver, and contradict itself). See
    /// `OrchRegistry::cli_for_agent`, the single read site.
    pub solo_cli: Option<String>,
    /// Rolling output tail captured at exit (#281), stripped of ANSI. The live
    /// pty's own ring is gone the instant it's reaped, which turned "why did a
    /// resumed session die silently" into a bare exit code with no way to ask
    /// for more — `agent_output_tail` falls back to this once the pty itself is
    /// gone. `None` until the agent exits (or for one still running).
    pub last_exit_tail: Option<String>,
    /// #533-B: who INITIATED this agent's termination, recorded at the
    /// moment the kill is issued and before the pty is touched — never
    /// derived afterwards from an exit code, an empty tail, or `expected`.
    /// `None` for a live agent and for every exit loomux did not initiate
    /// (a crash, a watchdog-driven death, an agent quitting on its own),
    /// which is exactly the set whose exit notice still PROMPTS the
    /// orchestrator. See [`ExitInitiator`] and [`exit_notice_route`].
    pub killed_by: Option<ExitInitiator>,
}

/// One pane that needs the human, pushed to the frontend as an `orch-attention`
/// event (the full current set each scan; the frontend badges panes by
/// `pty_id`). `reason`, most- to least-urgent:
/// - `held-dialog` — #946 Q4 / #1091 slice H: a blocking interactive dialog
///   is holding the ORCHESTRATOR's own delivery pipe, stranding every
///   delegate report queued behind it
/// - `blocked` — a worker reported it is blocked
/// - `provider-limit` — #2811 S5a: the account behind this pane's model is out
///   of budget and its CLI is sitting on the provider's refusal. Raised ONCE
///   per (group, provider) however many panes it stopped, and the only URGENT
///   reason here that no gesture inside the terminal can clear — the remedy is
///   billing, or a different `model:`. (`gate` is not terminal-clearable
///   either, but it is an amber decision on the human's own pace rather than a
///   wedged pane, which is the distinction this sentence is drawing.)
/// - `stranded` — a delivered prompt was never submitted (#496 PR-C): either
///   loomux is self-healing it, or it needs the human's Enter
/// - `waiting` — the pane is parked on a prompt (idle-with-prompt)
/// - `report`  — a worker reported done (awaiting the human's review/merge)
/// - `question` — this agent (orchestrator-only today) has a pending
///   `ask_human` row nobody has answered yet (#1091 slice D); DERIVED from
///   the `questions.json` registry each scan, never latched, so it clears
///   the instant the row is settled — answered, withdrawn or dismissed — but
///   it is the live-pane
///   PROJECTION of that registry (the asker's pane must still be running),
///   not the registry itself; a pending row against a stopped pane raises
///   no item here even though the registry still durably holds it
/// - `gate`    — this agent's task sits at a human merge gate on the board
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct AttentionItem {
    /// Empty for a plain (non-orchestration) pane, which is keyed only by
    /// `pty_id` — the human's hand-opened shells have no agent identity (#40).
    pub agent_id: String,
    /// Deliberately a `String` and **not** a [`GroupId`] (#904): the doc above
    /// is the reason — this field is legitimately EMPTY for a plain pane, and
    /// a `GroupId` cannot be empty by construction. That is the type doing its
    /// job: a slot whose empty value is meaningful is a display slot, not a
    /// group id, and it never reaches a path join. Attention items are read by
    /// the badge and the toast, nothing else.
    pub group: String,
    pub name: String,
    /// `None` for a plain pane (no orchestration role).
    pub role: Option<Role>,
    pub pty_id: Option<u32>,
    pub reason: &'static str,
    /// Short human phrase for the badge tooltip and the toast body.
    pub detail: String,
}

/// Durable roster entry (`agents.json` per group): which sessions belonged
/// to which role. This is what lets the session browser mark orchestrator/
/// worker sessions and restore a whole orchestration after loomux restarts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: String,
    pub role: String,
    /// The workflow block this agent was spawned from (#222). Persisted so a
    /// session rejoin restores the agent's *identity* (its persona, CLI and
    /// model), not merely its capability class. Additive: a roster row written
    /// before blocks deserializes to empty, and the rejoin falls back to the
    /// class's default block.
    #[serde(default)]
    pub block: String,
    pub name: String,
    /// Precedence tier of `name` (#95r). Persisted so a session rejoin restores
    /// the human's rename AND its "human beats orchestrator" tier, not just the
    /// text. Additive: legacy rows without it deserialize to `Default::default`.
    #[serde(default)]
    pub name_source: NameSource,
    pub session: Option<String>,
    pub cwd: String,
    pub status: String,
    pub updated_ms: u64,
    /// The task/brief text this agent was spawned or resumed with (#1,
    /// session browser metadata — doubles as the session's "description/goal"
    /// the human sees, since loomux does not track those as separate fields
    /// from the assigned work). Additive: a roster row written before this
    /// field existed deserializes to empty, which the browser renders as "no
    /// recorded task" rather than fabricating one.
    #[serde(default)]
    pub task: String,
    /// The git branch this agent's work is associated with, when it has one
    /// (#1). See `AgentEntry::branch` for exactly when this is `Some`.
    /// Additive: absent on a pre-#1 roster row.
    #[serde(default)]
    pub branch: Option<String>,
    /// What KIND of pane was spawned (#2850) — `"structured"`, or absent
    /// for the PTY pane every roster written before this key carried.
    ///
    /// A record, never a control: nothing reads it to decide how to DRIVE a
    /// pane (this process knows that from its own registry), only to render
    /// it and to answer the question for a pane this process did not spawn
    /// — which is exactly the case a persisted roster exists for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_kind: Option<String>,
    /// The parent SESSION a forked agent's session started as a copy of
    /// (#3318 F2) — `AgentEntry::forked_from`'s durable twin. Additive in both
    /// directions: absent on every roster written before F2 and on every row
    /// that is not a fork, so an older loomux reading a newer roster ignores
    /// an unknown key and a newer one reading an older roster reads `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
}

/// Durable per-agent usage snapshot (`usage.json` per group). Keyed by the CLI
/// session id when known (so a resumed session updates one row instead of
/// double-counting), else `agent:<id>`. Snapshots survive `kill_agent`/exit —
/// captured in `mark_dead` — so a group's lifetime cost keeps counting
/// recycled panes (issue #42).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageSnapshot {
    /// Stable identity: the CLI session id, or `agent:<id>` when there is none.
    pub key: String,
    pub agent_id: String,
    pub name: String,
    pub role: String,
    /// Where the figures came from: `transcript` (token-derived, exact tokens),
    /// `pi-transcript` (pi's own session file — exact tokens AND the dollar
    /// figure pi computed itself, #2126), `codex-transcript` (a codex rollout
    /// in the human's own store — exact tokens, dollars ESTIMATED here because
    /// codex records none, #2515), `session-db` (opencode's own
    /// `session` row — exact tokens AND its own dollar figure, #722),
    /// `stream` (a STRUCTURED pane, #2850 — the figures the harness REPORTED
    /// over its own protocol, which is why it is distinguishable from every
    /// scrape above rather than folded into one of them),
    /// `statusline` (last-resort parse of the CLI's own dollar figure), or
    /// `none` (nothing available yet).
    ///
    /// **Seven values, on SIX surfaces that must move together**: this doc,
    /// `AgentUsage.source`'s union in `src/orchestration.ts`, the enumeration
    /// in `docs/design/group-cost-tracking.md`, the `source`→`cli` table in
    /// `docs/design/orchestration-evals.md`, and TWO in
    /// `scripts/orch-scorecard.cjs`: `SOURCE_TO_CLI` (whose test pins it) and
    /// the H10 hazard entry, which states the same mapping in prose.
    ///
    /// This paragraph said THREE until #2850 added the seventh value, and the
    /// count was wrong TWICE on the way to six. The first sweep found five —
    /// the scorecard pair was never in the list. The second, re-run after a
    /// rebase and with `--include=*.cjs` actually in the pattern, found H10 as
    /// well. Both misses are the same shape: a surface nobody knows about is a
    /// surface nobody updates, and a sweep is only as wide as the globs it was
    /// run with and only valid for the base it was run on.
    ///
    /// The frontend's is a declared TYPE for a value that crosses the IPC seam
    /// untyped, so `tsc` cannot catch a value outside it and a narrowing
    /// written against a stale union is silently wrong. The scorecard's is a
    /// LOOKUP, and a missing key there degrades to `unknown` rather than
    /// failing — quiet in a different way. Adding an eighth means one entity
    /// grep (`grep -rn session-db --include=*.ts --include=*.rs --include=*.md
    /// --include=*.cjs`), not five guesses.
    ///
    /// **`stream` is deliberately absent from `SOURCE_TO_CLI`.** It names a
    /// transport, not a CLI: it is pi alone today only because pi is the only
    /// CLI with a structured driver, so mapping it would read a per-CLI
    /// identity off the wrong axis and go false when claude gains one.
    pub source: String,
    /// The workflow block this agent was spawned from (#2011 slice B, closing
    /// the `block` half of t-664). `role` above is the capability CLASS — four
    /// values — so it cannot tell `worker-std` from `worker-adv`, which is
    /// exactly the split every cost question here is actually about.
    ///
    /// Additive: a `usage.json` row written before this field existed
    /// deserializes to the empty string, which reads as "unknown block" rather
    /// than being guessed from `role`. Empty is a real answer and is rendered
    /// as one.
    #[serde(default)]
    pub block: String,
    /// The CLI that block runs (`claude`, `opencode`, `pi`, `codex`, …), as
    /// `Guardrails::cli_for_block` resolves it at the moment of the snapshot.
    ///
    /// Deliberately NOT derivable from `source` and never derived from it: the
    /// two answer different questions, and `source` takes `statusline` and
    /// `none` values off which no CLI can be read at all. Additive on the same
    /// terms as `block`.
    #[serde(default)]
    pub cli: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    /// Dollar cost, or `None` when only tokens are known (unknown model, or a
    /// transcript-less agent whose statusline shows nothing).
    pub cost_usd: Option<f64>,
    /// true = dollars estimated from the price table; false = reported by the
    /// CLI's statusline (which reads $0.00 on subscription/Max accounts).
    pub estimated: bool,
    /// The model the usage panel names as "priced against" — the source's
    /// `SessionUsage::model`, which on claude is the best-priced pick rather
    /// than the latest turn's model.
    pub model: Option<String>,
    /// The model of the latest counted turn (`SessionUsage::current_model`):
    /// what a usage-series sample records, so the token chart's model split
    /// and switch marks follow the pane rather than the pricing pick (#3415).
    /// Additive: a `usage.json` row written before this field deserializes to
    /// `None`, and the sampler falls back to `model` for such a row (see
    /// `series_sample`).
    #[serde(default)]
    pub current_model: Option<String>,
    pub updated_ms: u64,
    /// When this row's counters last moved and what its last wake cost (#3407)
    /// — folded in [`OrchRegistry::merge_usage_entry`] from the reading this
    /// tick already made, never from a second read. Additive: a row written
    /// before the field existed reads as unknown, and the next growth fills it.
    #[serde(default)]
    pub activity: loomux_engine::cacheage::Activity,
}

/// What the usage-series sampler remembers about one group between ticks
/// (#2011 slice B).
///
/// In-memory only, and deliberately so. Everything here is a *decision input*
/// — "have I already written this row" — never data. A restart forgets it and
/// the next tick writes one fresh row per key, which is exactly right: the
/// file is the record, this is only what stops the app rewriting it once a
/// second.
#[derive(Default)]
pub(in crate::orchestration) struct SeriesState {
    /// The last sample WRITTEN, per usage key. Written, not computed: the
    /// bucket is measured from the row that actually landed, so a failed
    /// append does not silently start a five-minute hole.
    pub(in crate::orchestration) last: HashMap<String, usageseries::Sample>,
    /// The last fingerprint a mark was written for, or `None` before the first
    /// mark of this process's life for this group.
    pub(in crate::orchestration) fp: Option<usageseries::Fp>,
    /// Unix-ms the fingerprint was last COMPUTED (not last changed). The walk
    /// is bounded but not free, so it runs at most once per bucket — which is
    /// also the resolution a mark is worth to the plot.
    pub(in crate::orchestration) fp_checked_ms: u64,
}

/// A recorded session's orchestration identity, for the session browser.
#[derive(Clone, Serialize)]
pub struct SessionRole {
    pub session_id: String,
    pub group_id: GroupId,
    pub role: String,
    pub agent_name: String,
    /// Whether that group currently has live agents in this app instance.
    pub group_live: bool,
    /// The task/brief this agent was spawned or resumed with (#1). Empty for
    /// a roster row predating the field, or an orchestrator (which has none)
    /// — the browser shows "no recorded task" rather than fabricating one.
    #[serde(default)]
    pub task: String,
    /// The git branch this agent's work is associated with, when it has one
    /// (#1). See `AgentEntry::branch`.
    #[serde(default)]
    pub branch: Option<String>,
    /// The group's repo path (#1), resolved from `group.json`. `None` only
    /// when that file is missing/unreadable — session_roles already requires
    /// it to exist to enumerate the group at all, so this is normally always
    /// `Some`; kept `Option` rather than an empty-string default so the
    /// browser can tell "unknown" from "repo is an empty string" (never
    /// actually reachable, but honest about what a read failure means).
    #[serde(default)]
    pub repo: Option<String>,
    /// The PR this agent's work is now attached to, when the task board
    /// records one for a task carrying this session (#1) — resolved live at
    /// read time (not persisted on the roster row) so it reflects the PR's
    /// CURRENT value even after the session itself has ended. `None` when no
    /// board task references this session, or it has no `pr` set yet.
    #[serde(default)]
    pub pr: Option<String>,
    /// The parent SESSION this one was forked from (#3368), read off the
    /// roster's durable `AgentRecord::forked_from` — the delegate half of the
    /// session browser's fork tree. `None` for a session that is not a
    /// delegate fork. One pointer, never a chain: the frontend derives the
    /// chain by walking these (`src/forklineage.ts`), so nothing here can
    /// disagree with the roster it is read from.
    #[serde(default)]
    pub forked_from: Option<String>,
}

/// One recorded orchestration GROUP, for the session browser's
/// "Orchestrations" section (#1563). Built from loomux's own records —
/// `group.json` plus the orchestrator row of `agents.json` — never from a
/// CLI's session store, which is what lets it list a group whose CLI store
/// loomux does not enumerate.
#[derive(Clone, Serialize)]
pub struct RecordedOrchestration {
    pub group_id: GroupId,
    /// The group's repo path from `group.json`. `None` when that file is
    /// missing or unparseable — the group dir is still listed (a group whose
    /// record is damaged is exactly the one the human needs to see), with
    /// `cli` empty for the same reason.
    pub repo: Option<String>,
    /// The CLI this group's ORCHESTRATOR block runs — resolved exactly as
    /// `resume_recorded_session`'s orchestrator branch resolves it, so the
    /// label beside a Resume button names the CLI the resume will launch.
    /// Empty string when `group.json` could not be read (see `repo`); never
    /// a guessed default, which would name the wrong CLI on a damaged group.
    ///
    /// That absolute is about an UNPARSEABLE file, and only that. A
    /// `group.json` that parses but carries no `guardrails.agent_cli` gets
    /// `"claude"` from `load_group_file`'s own `s("agent_cli", "claude")`
    /// fallback — a default this type inherits rather than introduces, and
    /// named here so the sentence above is not read wider than it holds
    /// (#1568 review N1).
    pub cli: String,
    /// The orchestrator's recorded CLI session id, or `None` when no
    /// orchestrator session has been identified for this group yet — a fresh
    /// copilot/opencode orchestrator whose watcher has not bound its id, or
    /// one whose watcher timed out (`session-untracked`). The frontend says
    /// so rather than offering a button that has nothing to resume.
    pub session_id: Option<String>,
    /// Whether this group currently has live agents in this app instance.
    /// A live group is NOT resumable — `resume_recorded_session` refuses
    /// ("already has a live orchestrator — focus its pane instead") — so this
    /// is what stops the list offering a click the backend will reject.
    pub group_live: bool,
    /// Whether `session_id` actually resolves in the CLI store the resume
    /// path will look in. See [`OrchRegistry::recorded_orchestrations`] —
    /// this asks that path's own question, so the button never promises what
    /// the backend will refuse. `false` whenever `session_id` is `None`.
    pub resumable: bool,
    /// The most recent `updated_ms` on ANY of this group's roster rows — the
    /// group's last recorded activity, deliberately not the orchestrator
    /// row's alone (an orchestrator that died early is not evidence the group
    /// went quiet). `0` for a group with no readable roster, which sorts it
    /// last within its liveness class rather than inventing a timestamp.
    pub last_seen_ms: u64,
}

/// Identity resolved from an MCP request's token header.
#[derive(Clone, Debug)]
pub struct Caller {
    pub agent_id: String,
    /// #904: validated. The MCP seam never takes a group as a tool *argument* —
    /// it resolves one from the caller's token — so this was already
    /// registry-provenance; carrying the type makes that structural rather than
    /// a fact you have to trace `resolve_token` to establish.
    pub group: GroupId,
    pub role: Role,
    /// The spawning block's `role_hint` (#250/#324, #891) — `advisor` |
    /// `process` | `liaison` | `None`. The structural containment keys off
    /// `role` alone; the MCP tier has exactly three hint-keyed exceptions, and
    /// they do not all point the same way. Two NARROW: `session_digest`'s
    /// dispatch gate narrows the worker tier to `role_hint == process`, and
    /// `review_verdict`'s narrows the reviewer tier by denying `role_hint ==
    /// liaison` (a liaison rides the reviewer class for its contained —
    /// `NoEdits`, not read-only — and persistent posture, and reviews nothing).
    /// One WIDENS: `group_usage`, `require_orchestrator`-only for every other
    /// tier, is granted to a caller that is BOTH `Role::Reviewer` and
    /// `role_hint == liaison` (#891 S2). The full enumeration, and why a grant
    /// owes an argument a narrowing does not, lives in `docs/design/liaison.md`.
    ///
    /// **This field is roster-derived, never caller-supplied**, and the gates
    /// above depend on that: [`OrchRegistry::resolve_token`] reads it from the
    /// group's own blocks via the block recorded on the agent at spawn.
    pub role_hint: Option<String>,
}

/// Normalize a caller-supplied pane name (#95r): trim, drop control characters
/// (so a pasted name can't smuggle newlines/escape codes into the pane title or
/// the roster JSON), and cap the length. Not a security boundary — the title is
/// rendered via `textContent`, never HTML — just hygiene. May return empty (an
/// all-control/whitespace name); callers decide what an empty result means.
pub(in crate::orchestration) fn sanitize_agent_name(name: &str) -> String {
    name.trim().chars().filter(|c| !c.is_control()).take(40).collect()
}

/// The counter value a minted agent id carries: `w-3` → 3, `rev-12` → 12,
/// `solo-7` → 7. `None` for anything that is not `<prefix>-<number>`.
///
/// Splits on the LAST `-` because a block prefix is not guaranteed to be one
/// token — `Role::prefix()` returns `rev` today, but the id format is
/// `{prefix}-{seq}` for whatever the prefix is, and only the tail is the
/// counter. Used to derive the #524 high-water floor from rosters written
/// before the counter file existed; deliberately total and lossless-in-doubt
/// (an unparseable tail contributes nothing rather than a guess).
pub(in crate::orchestration) fn agent_id_suffix(id: &str) -> Option<u32> {
    id.rsplit_once('-').and_then(|(_, tail)| tail.parse().ok())
}

thread_local! {
    /// How many groups have had their records read and parsed on THIS thread
    /// (#514). Bumped once per `merged_records` call — the one funnel through
    /// which a group's roster + full audit log is actually read, and so the
    /// unit of work the #479 group-hint fast path exists to avoid paying for
    /// every OTHER group.
    ///
    /// Monotonic and never reset: a reader takes a snapshot before the call
    /// it's measuring and subtracts, so there is no shared mutable state to
    /// clear and no ordering requirement between readers.
    ///
    /// Thread-local for the same reason as `sessions.rs`'s test seams: the
    /// default test harness runs each `#[test]` on its own OS thread, so a
    /// process-global counter would be polluted by whatever other tests
    /// happen to be scanning groups concurrently — which is exactly the
    /// nondeterminism a structural pin is meant to replace. Every lookup this
    /// measures (`session_roles`, `session_role_in_group`, and
    /// `resume_recorded_session`'s own record re-read) runs synchronously on
    /// its caller's thread.
    pub(in crate::orchestration) static GROUP_RECORD_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Observation seam for the counter above: how many group-record scans this
/// thread has done so far. Not `#[cfg(test)]` — integration tests
/// (`tests/orchestration/`) link this crate as an ordinary dependency,
/// where `cfg(test)` is never active, so the hook has to be a real (if
/// `#[doc(hidden)]`) function to be reachable from there.
///
/// Read it as a delta around the call under test:
///
/// ```ignore
/// let before = group_record_scans_for_test();
/// // ... the lookup being pinned ...
/// let scans = group_record_scans_for_test() - before;
/// ```
#[doc(hidden)] // pub for integration tests
pub fn group_record_scans_for_test() -> usize {
    GROUP_RECORD_SCANS.with(|c| c.get())
}
