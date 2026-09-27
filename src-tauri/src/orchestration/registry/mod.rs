//! `OrchRegistry`: the orchestration registry's struct and fields, its
//! constructor, and the constraint-10 barrier (`read_command` /
//! `mutating_command`) that every registry-taking synchronous
//! `#[tauri::command]` routes through (CLAUDE.md constraint 10,
//! `docs/design/lock-order.md` §2.1).
//!
//! The registry's methods are split by concern across this directory as
//! `impl OrchRegistry` blocks — a child module sees its ancestors' private
//! items, so each file is a plain move (#3498, `docs/design/module-layout.md`).
//! A field is `pub(super)` when code outside `registry/` reads it: that is
//! exactly the visibility a private field had while the struct sat in
//! `orchestration/mod.rs`.

use super::*;

mod delivery;
pub(super) use delivery::*;
mod groups;
mod spawn;
pub use spawn::*;
mod channels;
mod merge;
mod questions;
mod tasks;
mod agentfiles;
mod agentlaunch;
mod agents;
mod audit;
mod autonomy;
mod compact;
mod deliveryqueue;
mod idle;
mod instructions;
mod lockseams;
mod managermail;
mod persist;
mod resourcelocks;
mod roster;
mod solo;
mod stranded;
mod usage;
mod watches;

pub struct OrchRegistry {
    /// Root of persistent state: `<root>/<group>/{group.json,state.json,audit.jsonl,configs/}`.
    pub(super) root: PathBuf,
    /// The process's declared-root registry (#1042 slice B).
    ///
    /// Owned here rather than injected because this registry is one of its two
    /// populators — a group's checkout is declared as the group is created or
    /// resumed, and an agent's worktree as it is cut — and a populator wired by
    /// a `set_*` call is a populator that can be forgotten. Constructing it in
    /// [`OrchRegistry::new`] means there is no `Option` to be `None` in a
    /// shipped build and every integration test that builds a registry has a
    /// live one. `lib.rs` `manage`s the same `Arc` (via [`OrchRegistry::roots`])
    /// so `#[tauri::command]`s reach it as `State<Arc<RootRegistry>>`; slice C's
    /// boundary `resolve` calls read this same instance.
    ///
    /// Not `Mutex`-wrapped: `RootRegistry`'s own interior `RwLock` is the
    /// synchronization, and `&self` is all its methods need.
    pub(super) roots: Arc<RootRegistry>,
    /// Absent in unit tests: spawning then skips the pane round-trip.
    pub(super) app: TrackedMutex<Option<AppHandle>>,
    pub(super) groups: TrackedMutex<HashMap<GroupId, GroupInfo>>,
    pub(super) agents: TrackedMutex<HashMap<String, AgentEntry>>,
    pub(super) by_token: TrackedMutex<HashMap<String, String>>,
    pub(super) by_pty: TrackedMutex<HashMap<u32, String>>,
    /// Live structured panes (#2850 S3b), keyed by agent id.
    ///
    /// Beside `by_pty` rather than inside it: the two pane kinds share ONE
    /// id keyspace (`PtyManager::reserve_id`) so a delivery can be routed by
    /// id alone, but they are different things and a lookup that returned
    /// either would be the asymmetry a guard is supposed to prevent. A
    /// structured pane is found HERE, before anything reaches `PtyManager`.
    pub(super) structured: structured::StructuredPanes,
    /// Usage a structured pane REPORTED, per agent id (#2850 S3b).
    ///
    /// Stored rather than recomputed, which is what separates this source
    /// from the six transcript ones: those re-read a file the CLI wrote, so
    /// their figures survive a restart on their own. A stream source has no
    /// file to re-read.
    pub(super) stream_usage: TrackedMutex<HashMap<String, structured::StreamUsage>>,
    pub(super) pending_binds: TrackedMutex<HashMap<String, mpsc::Sender<u32>>>,
    /// The `SpawnRequest`s built on the **no-frontend** path, by agent id — the
    /// payload a real frontend would have received, kept only when there is no
    /// frontend to hand it to (see the `app.is_none()` branch of
    /// `spawn_agent_ex`). Empty in production, where the request has exactly one
    /// owner: the webview it was emitted to.
    ///
    /// It exists for one reason (#462 review). The hop from a **capability
    /// class** to the deny flags on its command line — `role.containment()`, at
    /// the spawn SITE — is the one step a test calling `build_agent_command`
    /// itself can never cover: such a test re-derives the tier with the same
    /// expression the site uses, so a site that hardcoded a literal instead
    /// would keep every one of them green. A pin that stops at the extracted
    /// unit proves nothing about the caller that feeds it. `spawn_agent_ex` is
    /// the only site that needs this; `register_orchestrator_pane` RETURNS its
    /// request, so its own wiring is already assertable.
    pub(super) test_spawn_requests: TrackedMutex<HashMap<String, SpawnRequest>>,
    /// Per-agent notices produced while compiling a spawn (#802), waiting to be
    /// read ONCE by the `spawn_agent` reply that caused them
    /// ([`Self::take_spawn_notices`]).
    ///
    /// A drain-on-read map rather than a field on [`AgentEntry`] because this is
    /// news about a *spawn*, not state of an *agent*: it is true exactly once,
    /// at the moment the orchestrator can still act on it, and nothing later
    /// re-reads it. Same shape as `record_verdict`'s `warnings` return, which
    /// the `review_verdict` reply already appends as `NOTE:`.
    ///
    /// A spawn that never goes through the MCP tool has no reader, so taking it
    /// is not by itself a bound — the insert site prunes ids that are no longer
    /// live agents instead.
    pub(super) spawn_notices: TrackedMutex<HashMap<String, Vec<String>>>,
    /// The `orch-session-learned` payloads produced while there was no
    /// `AppHandle` to emit them to. Empty in production, where every payload
    /// goes to the webview instead — the same seam, and the same reason, as
    /// [`Self::test_spawn_requests`]: with no frontend, this is the only place
    /// what the emit SITE built stays observable.
    ///
    /// Its residual, stated rather than left to be discovered: this records
    /// that `associate_session` reached the emit exactly once per binding and
    /// with which payload — not that `app.emit` itself fired, which no test
    /// without a Tauri app handle can reach. The uncovered branch is one line
    /// long and takes the SAME payload value as the covered one, built above
    /// the branch precisely so the two cannot disagree.
    ///
    /// Bounded even if an `AppHandle`-less build ever ran for real:
    /// `associate_session` binds an id only while the roster record has none,
    /// so this appends at most one entry per agent ever spawned, never one per
    /// watcher poll.
    pub(super) test_session_learned: TrackedMutex<Vec<serde_json::Value>>,
    pub(super) port: AtomicU16,
    /// Agent-id counter: `w-3`, `rev-8`, `solo-2`, `orch-1` all mint their
    /// numeric suffix here. **Registry-global, not per-group** — one counter
    /// serves every group and the solo pseudo-group, which is why its durable
    /// high-water mark lives at the orchestration ROOT and not in a group dir
    /// (see `agent_seq_path`).
    ///
    /// Never read or bumped directly: `mint_agent_seq` is the only way to take
    /// a value from it, because a mint that is not persisted is the #524
    /// defect. Seeded lazily from disk on the first mint (`seed_agent_seq`),
    /// so `new()` stays I/O-free for the many registries the test suite builds
    /// and never uses.
    pub(super) seq: AtomicU32,
    /// Whether `seq` has been seeded from the durable high-water mark yet.
    /// Read and set only under `agent_seq_persist`, so the seed happens
    /// exactly once and can never interleave with a mint that would then be
    /// overwritten by it.
    pub(super) agent_seq_seeded: AtomicBool,
    /// Serializes the whole agent-id mint: seed → `fetch_add` → persist (#524).
    ///
    /// Held across all three deliberately. Persisting outside the critical
    /// section would let two concurrent spawns write their marks out of order
    /// (the file ending up BELOW an id already handed out — the exact reuse
    /// this closes), and the alternative fix, a read-modify-write of the file
    /// per mint, is strictly more I/O for the same guarantee. Mints happen at
    /// spawn, a few per session, so serializing them costs nothing measurable.
    ///
    /// **Lock order: takes no other registry lock while held**, and no caller
    /// holds one when it calls in — the same discipline `queue_persist`
    /// follows, and for the same reason (this does file I/O).
    pub(super) agent_seq_persist: TrackedMutex<()>,
    /// Per-pane delivery locks so two prompts to the SAME pane can't
    /// interleave keystrokes, while a slow delivery (waiting out a busy
    /// CLI) doesn't block deliveries to other panes.
    pub(super) delivery: TrackedMutex<HashMap<u32, Arc<TrackedMutex<()>>>>,
    /// Outcome of the most recent delivery to each pane (keyed by pty id), so a
    /// delivery can flush a previous prompt still stranded in the input box
    /// before pasting (#81/#84). An `Arc` so a delivery thread can record its
    /// outcome without holding `&self`.
    pub(super) last_delivery: Arc<TrackedMutex<HashMap<u32, DeliveryOutcome>>>,
    /// What loomux knows it WROTE into each pane (#576) — the marker-led lines
    /// of every delivery, bounded per pane by [`DeliveredNotices`]. Read by the
    /// question gate's mask so a notice that wrapped, or one whose marker row
    /// has scrolled off, can still be recognised as loomux's own writing rather
    /// than as a question the pane is parked on. `Arc` for the same reason
    /// `last_delivery` is: the delivery thread writes it without holding
    /// `&self`. Keyed by pty id, matching `delivery`/`last_delivery`.
    ///
    /// **Lock order: takes no other registry lock while held.** Its writer
    /// (`record_delivered_text`) is called from inside `deliver_now`'s paste
    /// loop, which already holds the per-pane delivery lock.
    pub(super) delivered_notices: Arc<TrackedMutex<HashMap<u32, DeliveredNotices>>>,
    /// #903: prompt bodies loomux has delivered, keyed by the agent's CLI
    /// SESSION rather than by pane — see [`DeliveredPrompts`] for why the
    /// keying is the fix and not an optimisation.
    ///
    /// **Lock order: takes no other registry lock while held**, and since #1702
    /// it does not depend on one being taken before it either: the pty-to-session
    /// resolution that reads `agents` moved OUT of
    /// [`OrchRegistry::delivered_prompt_record`] to its callers, so a caller
    /// that already holds the session (an agent snapshot) reaches this map
    /// having taken nothing at all. Where a caller does resolve — the delivery
    /// path, and `plain_pane_attention` — `by_pty` then `agents` are taken and
    /// released as statement temporaries first, so the order is still `agents`
    /// before this one and nothing nests. Same order
    /// [`OrchRegistry::mark_notice_maskable`] already uses.
    pub(super) delivered_prompts: Arc<TrackedMutex<HashMap<String, DeliveredPrompts>>>,
    /// Per-pane FIFO delivery queue (#445): a hold-cap expiry in
    /// `deliver_now` enqueues here instead of destroying the payload. `Arc`
    /// for the same reason `last_delivery` is — the drainer thread
    /// (`run_queue_drainer`) mutates it without holding `&self`. Keyed by
    /// pty id, matching `delivery`/`last_delivery`.
    ///
    /// **A [`QueueMap`], not a bare `Mutex<HashMap<..>>` (#562).** Its
    /// `&mut` door is `QueueMap::mutate`, which takes the snapshot writer
    /// and runs it as soon as the mutation returns — so #468's "every
    /// mutation of `queues` rewrites `queue.json`" is a shape the compiler
    /// enforces rather than a row in a table someone must remember to
    /// extend. See `queuestate.rs` for why the type had to live in another
    /// MODULE for that to mean anything.
    pub(super) queues: Arc<queuestate::QueueMap>,
    /// Per-registry monotonic id counter for queued deliveries — no
    /// getrandom (CLAUDE.md constraint 2), `Arc` so the drainer can mint ids
    /// too (a drain's own re-enqueue, e.g. seam 3's `StrandedSubmit`
    /// conversion, needs a fresh id like any other admit).
    pub(super) queue_seq: Arc<AtomicU64>,
    /// Panes with a live drainer thread right now, so `deliver_now`'s
    /// enqueue-on-abort spawns at most one drainer per pane (bounded thread
    /// lifecycle — the same leak concern raised against #451's late
    /// monitors, answered the same way: exits on drain, never accumulates).
    ///
    /// **Generation-owned, not a bare membership set (#470 B1 review round
    /// 2).** Keyed to a monotonic `drainer_gen` token minted at spawn, not
    /// just `pty_id` — see `DrainerGuard`'s doc for why: a bare
    /// `HashSet<u32>` lets a drainer's OWN stale deregistration (its RAII
    /// guard dropping after `commit_exit` already deregistered it) erase a
    /// SUCCESSOR drainer's live registration, breaking the at-most-one-
    /// drainer invariant the whole ordering proof rests on. Every removal
    /// (`commit_exit`, `DrainerGuard::drop`) is generation-checked: it only
    /// clears the entry if the CURRENTLY stored generation is still its
    /// own, which makes a stale/duplicate removal attempt a structural
    /// no-op rather than something every call site has to remember to
    /// avoid.
    ///
    /// **And now structurally, for call sites that do not exist yet
    /// (#497).** A [`DrainerRegistry`] exposes exactly one removal —
    /// `release(pty_id, generation)` — so "every removal is
    /// generation-checked" stopped being a property of the three sites
    /// review happened to look at and became a property of the type. That
    /// is the half `drainer_lifecycle`'s model cannot cover: a model
    /// carries events for the code that existed when it was written.
    pub(super) queue_draining: Arc<queuestate::DrainerRegistry>,
    /// Monotonic counter minting a fresh generation token per drainer spawn
    /// (`ensure_drainer`) — no getrandom (CLAUDE.md constraint 2). See
    /// `queue_draining`'s doc for why a generation, not just a bool
    /// membership flag, is what makes stale-deregistration structurally
    /// impossible rather than merely avoided by discipline.
    pub(super) drainer_gen: Arc<AtomicU64>,
    /// Panes whose queue has already fired the one-shot "still queued"
    /// visibility notice (#445) — cleared when the queue empties, so a LATER
    /// long block on the same pane can notify again.
    pub(super) queue_still_notified: Arc<TrackedMutex<HashSet<u32>>>,
    /// Serializes writers of every group's `queue.json` (#468).
    ///
    /// **Lock order: this BEFORE `queues`, never the reverse**, and it is
    /// only ever taken by `persist_queues`, which is called with no other
    /// lock held. Held across "read the live queues → write the file," which
    /// is what makes a stale snapshot unable to land after a fresh one: a
    /// mutation that completes after some writer's read must wait here and
    /// then re-read, so the last write always reflects the last mutation.
    /// Taking the `queues` lock first and doing the I/O under it would be
    /// simpler and wrong twice over — file I/O under the queue lock stalls
    /// every delivery in the registry, and this file is written on a path
    /// (`enqueue_text`) whose whole ordering argument depends on that
    /// critical section staying short.
    pub(super) queue_persist: Arc<TrackedMutex<()>>,
    /// Entries read back out of a group's `queue.json` at startup that have
    /// not yet found a pane to go back to (#467), keyed by group.
    ///
    /// A staging area, deliberately NOT the live `queues` map: every entry
    /// here was queued for a `pty_id` that no longer exists, and re-admitting
    /// one under its old key would target whatever unrelated pane inherits
    /// that number. Entries leave here exactly two ways — `readmit_recovered`
    /// moves one into the live queue when its pane comes back with a matching
    /// durable identity (`queue::rebinds_to`), or it stays and is reported by
    /// `queue_orphans` for the orchestrator to re-derive. Nothing expires
    /// them: an entry silently vanishing from here is the exact failure #445
    /// exists to prevent.
    pub(super) recovered_queue: Arc<TrackedMutex<HashMap<GroupId, Vec<queue::PersistedEntry>>>>,
    /// `StrandedSubmit` markers a restart made unreplayable (#467), kept
    /// separately from `recovered_queue` and never offered to
    /// `readmit_recovered`.
    ///
    /// They are here so the loss is reported through `queue_orphans` — a
    /// DURABLE channel the orchestrator's session-start re-sync reads —
    /// rather than only through the best-effort `[orrerix]` notice recovery
    /// also fires. That notice can genuinely fail to land: recovery can be
    /// triggered by an admission (see `persist_queues`) at a moment when no
    /// orchestrator pane is bound yet, and `deliver_to_orchestrator` is
    /// best-effort by design. "Never silently dropped" cannot rest on a
    /// delivery that is allowed to not happen.
    pub(super) recovered_markers: Arc<TrackedMutex<HashMap<GroupId, Vec<queue::PersistedEntry>>>>,
    /// Groups whose `queue.json` has already been read back this process
    /// (#467) — recovery is lazy (first touch by a bind or a
    /// `queue_orphans` call) and must happen exactly once, or a second pass
    /// would re-stage entries a first pass already re-admitted and deliver
    /// them twice.
    pub(super) recovered_groups: Arc<TrackedMutex<HashSet<GroupId>>>,
    /// Groups whose merge queue has already been reconciled this process
    /// (#581 §4). Separate from `recovered_groups` on purpose: the delivery
    /// queue and the merge queue recover independently, and sharing one mark
    /// would make whichever ran first silently suppress the other.
    pub(super) mq_reconciled_groups: Arc<TrackedMutex<HashSet<GroupId>>>,
    /// Serializes every read-modify-write of a `merge_queue.json` (#698).
    ///
    /// The driver tick and the three MCP tools are on different threads, and
    /// each of them is a `load_state` → decide → `store_state` sequence. Without
    /// this, a `queue_merge` that lands while a batch is being built reads the
    /// pre-build file, writes it back afterwards, and the batch record is simply
    /// gone — a lost update whose symptom is the exact "queued and never
    /// landed" #698 reports, one layer down.
    ///
    /// **One registry-wide lock rather than one per group**, deliberately. The
    /// only thread that holds it for long is the driver, which services one
    /// group per tick by construction (`mq_driver_tick`), so a per-group map
    /// would buy no concurrency the design can actually use, at the cost of a
    /// lock-ordering question every future caller would have to get right.
    ///
    /// **Never held across a notice delivery.** A delivery enqueues, and an
    /// enqueue can re-enter registry locks — the #467/#468 two-phase rule that
    /// `merge_queue_reconcile` already follows.
    pub(super) mq_state_lock: Arc<TrackedMutex<()>>,
    /// Earliest wall-clock at which the driver may service each group again
    /// (#698). Absent = now.
    ///
    /// In memory only, and that is the point: it exists to stop a group whose
    /// remote is down from emitting one abort notice per poll tick, and that
    /// condition is a fact about the world rather than about the queue. A
    /// persisted backoff would keep punishing a batch for a network that has
    /// since come back.
    pub(super) mq_service_ms: Arc<TrackedMutex<HashMap<GroupId, u64>>>,
    /// Test seam (#698), the merge-queue sibling of `pr_head_override`: when
    /// set, `mq_driver_tick` drives with this runner instead of building a
    /// [`mqdriver::ProcessRunner`] over the group's repo. `None` in the app,
    /// always.
    ///
    /// It exists because the thing #698 was actually about is the **production
    /// entry point** — every seam below it was already green while nothing
    /// called them — and that entry point resolves its own runner. Without a
    /// seam here, a test could reach `mq_drive_group_with` (proving the driver
    /// works) or `gh_poll_tick` (proving the poll loop calls *something*), but
    /// never both at once, which is precisely the gap that let the door stay
    /// connected to nothing. Same posture as `pr_head_override`: a canned
    /// runner, so no test spawns `git` or `gh` (CLAUDE.md constraint 3).
    pub(super) mq_runner_override: TrackedMutex<Option<Arc<dyn mqdriver::MqRunner>>>,
    /// Serializes every read-modify-write of a `review_drives.json` (#1778 §2.4).
    ///
    /// `mq_state_lock`'s twin, and a separate lock rather than a shared one:
    /// the two loops run in the same tick against the same group, and one lock
    /// would make the review driver's spawn — which this one is held across —
    /// block a `queue_merge` that has nothing to do with it.
    ///
    /// **Held across a spawn and across a delegate delivery; never across a
    /// notice to the orchestrator.** §2.4 wants the load-decide-store to span
    /// the spawn, because a `drive_review` landing inside that window would read
    /// the pre-spawn file and write it back, erasing the entry; #467/#468 want
    /// no registry lock held across a delivery. Both hold, on a property of the
    /// lock rather than a count of its callers: no site that takes it is
    /// reachable from a pane delivery, so a spawn's own kickoff cannot cycle
    /// back onto it — and since #1960 neither can the `deliver_prompt` a
    /// hand-back makes directly when it resumes into a live idle pane instead of
    /// opening one. The site list, and why the two interception helpers do not
    /// break it, is on [`Registry::rd_drive_group_with`].
    pub(super) rd_state_lock: Arc<TrackedMutex<()>>,
    /// Earliest wall-clock at which the review driver may service each group
    /// again (§2.4). Absent = now.
    ///
    /// In memory only, for `mq_service_ms`'s reason: the condition it exists to
    /// rate-limit is a fact about the world — a `gh` outage, a refused spawn, a
    /// gate that cannot be satisfied yet — rather than about the drive, and a
    /// persisted backoff would keep punishing a drive for a network that has
    /// since come back.
    pub(super) rd_service_ms: Arc<TrackedMutex<HashMap<GroupId, u64>>>,
    /// #3330 ask 2: per group whose driver is OFF while unfinished drives sit
    /// on disk, what was announced (the cause and the PRs) and whether the
    /// line landed. See `rd_announce_disabled_drives`. In memory: a restart
    /// re-announces once, which is the point — that is when it went silent.
    pub(super) rd_disabled_warned: TrackedMutex<HashMap<GroupId, (String, Vec<u64>, bool)>>,
    /// Test seam: when set, `rd_driver_tick` drives with this `gh` instead of
    /// building a process runner over the group's repo. `None` in the app.
    pub(super) rd_runner_override: TrackedMutex<Option<Arc<dyn rddrive::RdRunner>>>,
    /// Driven delegates' events, between the MCP arm that consumed one (§7) and
    /// the tick that acts on it. In memory; `rd_ingest` carries why.
    pub(super) rd_signals: Arc<TrackedMutex<HashMap<(GroupId, u64), RdSignal>>>,
    /// Drives the restart reconcile found parked in `fix-wait`, waiting for the
    /// first tick to re-hand-back (#2811 S10).
    ///
    /// In memory, like [`rd_signals`](Self::rd_signals), and deliberately so:
    /// the mark says "this PROCESS restarted under this drive", which is only
    /// ever true until this process answers it. Persisting it would outlive the
    /// fact — a mark written now and read after the NEXT restart would re-brief
    /// a worker on a process boundary it already answered — and it needs no
    /// persistence to be reliable, because a process that dies before the tick
    /// runs simply reconciles again on the way back up.
    ///
    /// **An entry is removed by the tick that ACTED on it — never by one that
    /// merely read it** (#3196 review 2). A tick can be preempted above
    /// `decide_fix_wait` by the empty-head guard (`observe_pr` could not read
    /// the PR) or by the age and state backstops, and none of those re-brief
    /// anybody; since the reconcile runs once per registry instance, a mark
    /// spent by such a tick is never re-issued and the drive keeps its dead pane
    /// until `fix_timeout_minutes` expires. So the mark survives every tick that
    /// decided nothing, and is discharged when the worker was re-briefed or the
    /// drive has left `fix-wait`.
    ///
    /// **A record per drive rather than a bare set** (#3225, #3226): a restart
    /// costs a `review-wait` drive its lane panes and a `ci-wait` drive the
    /// worker whose receipts it is waiting on, and those are three recoveries
    /// of one fact. [`rdtick::RestartMark`] carries them together and each is
    /// discharged by the tick that acted on it.
    pub(super) rd_restart_handback: Arc<TrackedMutex<HashMap<(GroupId, u64), rdtick::RestartMark>>>,
    /// The last hand-back failure of each drive — the session it failed FOR and
    /// the failure line — so a SECOND identical failure (#2555 item 2) can be
    /// told apart from the first and said so: the hold's quoted refusal gains
    /// "second time", turning another resume into a decision rather than a
    /// reflex. In memory like [`rd_signals`](Self::rd_signals), and with the
    /// same bounded consequence: a restart between the two failures loses the
    /// count, which degrades to the one-hold-per-resume behaviour the bound
    /// replaced, never to a wrong claim — the second failure after a restart is
    /// simply counted from the restart. Cleared on a fresh drive and on a
    /// hand-back that succeeds; never cleared on a resume, which is the point.
    pub(super) rd_handback_fails: Arc<TrackedMutex<HashMap<(GroupId, u64), (String, String)>>>,
    /// Groups whose persisted drives have been reconciled this process (§2.4).
    /// Once-only, like `merge_queue_reconcile_with`'s own guard.
    pub(super) rd_reconciled: Arc<TrackedMutex<HashSet<GroupId>>>,
    /// #3040: the PLAN driver's four, each the twin of the `rd_` field above it
    /// and holding for that field's stated reason. There is deliberately no
    /// `pd_runner_override`: the plan driver reads through the SAME `gh` seam,
    /// so one override is one statement about a test's whole tick.
    ///
    /// Serialises the read-modify-write of `plan_drives.json`. In P3a nothing
    /// spawns or delivers under it — `pd_drive_group_with` carries what that
    /// narrower claim covers, and what P3b owes when it widens it.
    pub(super) pd_state_lock: Arc<TrackedMutex<()>>,
    /// Earliest wall-clock at which the plan driver may service each group
    /// again. Absent = now. In memory, for `rd_service_ms`'s reason.
    pub(super) pd_service_ms: Arc<TrackedMutex<HashMap<GroupId, u64>>>,
    /// Driven planners' events, between the MCP arm that consumed one and the
    /// tick that acts on it. In memory; `pd_ingest` carries why.
    pub(super) pd_signals: Arc<TrackedMutex<HashMap<(GroupId, u64), PdSignal>>>,
    /// Groups whose persisted plan drives have been reconciled this process.
    pub(super) pd_reconciled: Arc<TrackedMutex<HashSet<GroupId>>>,
    /// #560: each pane's open hold EPISODE — when it began, and what has
    /// already been said about it. Keyed by `pty_id`, in memory only (see
    /// [`HoldEpisode`] for the restart argument).
    ///
    /// **Why one record and not two flags (#560).** #532 kept the escalation's
    /// clock and its one-shot in two different places: the clock was the front
    /// queue entry's `enqueued_ms` and the one-shot was a `HashSet<u32>` of
    /// already-badged panes. *Those two do not describe the same thing*, and
    /// both of #560's symptoms are that mismatch — a writable poll reset the
    /// one-shot without resetting the clock (re-badge churn), and
    /// `enqueue_stranded_front`'s fresh `now_ms()` marker reset the clock
    /// without ending the episode (a deferred badge on a pane that never
    /// recovered). Fusing them into one record with one lifecycle makes the
    /// divergence unrepresentable rather than merely fixed: there is no way to
    /// clear the one-shot without ending the episode, because they are the
    /// same value.
    pub(super) hold_episodes: Arc<TrackedMutex<HashMap<u32, HoldEpisode>>>,
    /// #563: the last capacity state each pane's queue was OBSERVED in, so
    /// pressure is reported on the transition up and released on the
    /// transition down.
    ///
    /// An edge trigger, not a one-shot flag, and the difference is the point:
    /// a `HashSet` of "already warned" panes would latch, and a pane that
    /// drained, recovered and backed up again would be silent the second time
    /// — which is exactly the failure mode
    /// [`OrchRegistry::hold_episodes`]'s `question_stale_notified` predecessor
    /// had to fix by clearing itself (#560 replaced that flag with the episode
    /// record, but the failure mode it names is unchanged).
    /// Storing the state instead makes both directions fall
    /// out of one comparison. Release is on EVIDENCE (a depth that has
    /// actually come back down), never on elapsed time — `.loomux/lessons.md`,
    /// "releasing on evidence beats releasing on elapsed time".
    ///
    /// Stores the agent id alongside the state, and that is load-bearing
    /// rather than convenience: the RELEASE transition is observed on a pop
    /// that may have just emptied the queue, so at the moment the badge has to
    /// come down there is no queue entry left to say whose badge it is.
    /// Remembering who it was raised for is the only reading that survives the
    /// queue itself.
    pub(super) queue_pressure: Arc<TrackedMutex<HashMap<u32, (queue::CapacityState, String)>>>,
    /// #814: the last `orch-queue-depth` set actually pushed to the webview —
    /// what [`OrchRegistry::queue_depth_push`] compares against so an unchanged
    /// reading costs no emit at all.
    ///
    /// Sent *sets*, not per-pane state, and that is the point: a pane whose
    /// queue drained has to be REMOVED from the badge set, and the frontend
    /// learns that from the pane's absence in the next push. Remembering
    /// per-pane readings would leave "this pane is no longer in the set" as a
    /// fact nothing carries.
    ///
    /// Empty at rest — which is also the common case, and why an idle app emits
    /// nothing on this stream at all rather than a per-tick "still nothing".
    ///
    /// Carries WHEN that push happened alongside it, which is what makes the
    /// skip releasable: see [`OrchRegistry::queue_depth_push`] and
    /// [`QUEUE_DEPTH_REPUSH_MS`] for why a suppression that only ever ends when
    /// the reading changes would strand a badge on precisely the stalled pane
    /// the badge is for.
    queue_depth_emitted: Arc<TrackedMutex<(Vec<queue::QueueDepthItem>, u64)>>,
    /// #539: unconfirmed-delivery alarms buffered for one coalesced notice
    /// per pane, keyed by `(group, agent_id, eaten)` and holding each buffered
    /// delivery's id (its `submit_sent_ms`) in the order they were raised.
    ///
    /// **`eaten` is part of the KEY, not a field on the batch** (#585 + #539).
    /// The two alarms carry materially different instructions — "was LOST, its
    /// text never reached the box, re-send it" versus "may be sitting
    /// unsubmitted, get_output it" — so one coalesced line could not
    /// truthfully cover both. Merging them would put a claim on ids it is
    /// false for, which is the defect class this repo blocks. Keying on it
    /// gives each kind its own window and its own accurate wording; the only
    /// cost is that a pane raising both kinds inside one window pays two
    /// notices instead of one, still bounded and still far below the
    /// per-delivery cost this feature replaced.
    ///
    /// The cost this exists to stop is the orchestrator's, not loomux's:
    /// every one of these notices is an agent turn plus the `get_output`
    /// probe it prompts, so N alarms landing on one pane inside a window
    /// cost N turns to answer a single question ("is anything actually stuck
    /// on that pane?"). Two deliveries to the same pane can each declare
    /// failure within one `LATE_MONITOR_POLL` of the other — supersession
    /// only retires the older monitor at its NEXT tick — and an in-window
    /// `Failed` from `deliver_now` can land alongside a monitor's. Same
    /// collapse-at-source shape #533-A used for the flush itself: one
    /// message, every constituent named, nothing dropped.
    ///
    /// A bucket only ever exists between a first alarm and its flush, so
    /// this map is empty in the ordinary case and cannot grow without a
    /// pending timer that will drain it.
    pub(super) unconfirmed_pending: Arc<TrackedMutex<HashMap<(String, String, bool), Vec<u64>>>>,
    /// #578: queue notices `notify_queue` could not deliver because the
    /// TARGET was the group's own orchestrator, parked per group until that
    /// orchestrator's next MCP tool call carries them back
    /// ([`OrchRegistry::take_orchestrator_notices`]).
    ///
    /// **Keyed by group, not by agent**, because the notice's reader is
    /// whoever `deliver_to_orchestrator` would have delivered it to — the
    /// group's live orchestrator — and that is a role, not a fixed id.
    ///
    /// In memory only, and deliberately so: this is a relay for the *next
    /// turn*, not a durable record. The durable record is the
    /// `notice-suppressed` audit line, which since #578 carries the notice
    /// text — so a loomux restart loses the relay and not the information.
    pub(super) orch_notice_inbox: Arc<TrackedMutex<HashMap<GroupId, OrchNoticeInbox>>>,

    /// Serializes task-board read-modify-write cycles (MCP threads and the
    /// human UI mutate the same tasks.json).
    pub(super) tasks_lock: TrackedMutex<()>,
    /// Serializes every read-modify-write of a group's `questions.json` (#946).
    ///
    /// **A leaf of its own, not `tasks_lock`.** The two files share no
    /// invariant — a question names a task id, but nothing reads a question and
    /// a board row together as one unit — and the answer path takes this lock
    /// on a human's keystroke while the board's is held across MCP bursts.
    ///
    /// **Lock order: takes no other registry lock while held, with two stated
    /// exceptions — both leaves, and both the same nesting `tasks_lock` already
    /// has:**
    ///
    /// 1. `AUDIT_LOCK`, on the refusal paths, which must record what was turned
    ///    away before returning.
    /// 2. **The app-handle mutex, on every successful write.** `write_questions`
    ///    emits `orch-questions-changed` while the guard is held, which takes
    ///    `self.app` — exactly as `write_tasks` takes it via
    ///    `emit_tasks_changed` under `tasks_lock`. It is a leaf (nothing takes a
    ///    registry lock while holding the app handle), so it cannot cycle; it is
    ///    named here because "no other registry lock" read as covering it, and a
    ///    lock-order claim that omits a lock it actually takes is the kind of
    ///    thing a future deadlock hides behind.
    ///
    /// The answer path's success audit and its pane DELIVERY both happen after
    /// the guard is dropped: an audit write is cheap, but a notice is a
    /// delivery, and a delivery enqueues.
    pub(super) questions_lock: TrackedMutex<()>,
    /// Serializes every read-modify-write of a group's `needs-you.json` (#1151).
    ///
    /// **A leaf of its own, like `questions_lock`, and for the same reason**: the
    /// items file and the board share no invariant that a single lock would be
    /// protecting — an item names a task id, but the item is the lifecycle record
    /// and the task keeps owning the facts, so nothing reads one row of each as a
    /// unit.
    ///
    /// **Lock order — this one is nested, and that is the whole of it:**
    /// `tasks_lock` → `needs_you_lock`, never the reverse. `upsert_task` keeps
    /// the board lock across `sync_demo_item` so that a task's status and its
    /// demo item cannot settle in opposite orders under two racing transitions
    /// (in-then-out landing as out-then-in would leave an open demo item on a
    /// task that is no longer parked — precisely the stale row the auto-resolve
    /// exists to prevent). Nothing takes `tasks_lock` while holding this one:
    /// the backfill reads the board through `tasks()`, which is a lock-free file
    /// read, so the nesting cannot cycle.
    ///
    /// Also taken under this guard, both leaves and both the nesting
    /// `tasks_lock`/`questions_lock` already have: `AUDIT_LOCK` on the refusal
    /// paths (what was turned away must be recorded before returning), and the
    /// app-handle mutex on every successful write (`write_needs_you` emits
    /// `orch-needs-you-changed`, exactly as `write_tasks` emits under
    /// `tasks_lock`).
    ///
    /// The resolve path's success audit and its pane DELIVERY both happen after
    /// the guard is dropped: an audit write is cheap, a delivery enqueues.
    pub(super) needs_you_lock: TrackedMutex<()>,
    /// Serializes every read-modify-write of a group's `mailbox.json` (#1161 M2).
    ///
    /// **A leaf of its own, like `questions_lock` and `needs_you_lock`.** The
    /// mailbox shares no invariant with the board or the question registry: a
    /// `kind: question` row is a POKE that names a `q-N`, and the question
    /// itself is the record — nothing reads one row of each as a unit, so a
    /// shared lock would be serializing two files that never move together.
    ///
    /// **Lock order: nothing. It is taken alone**, and no registry lock is held
    /// across it. `post_to_manager` resolves the manager block through
    /// `self.group(..)` BEFORE taking this guard, deliberately — the groups
    /// mutex would otherwise be taken under it and the leaf claim would be
    /// false.
    ///
    /// Also taken under this guard, both leaves and both the same two the
    /// siblings above name: `AUDIT_LOCK` on the refusal paths (what was turned
    /// away must be recorded before returning), and the app-handle mutex on a
    /// successful write (`write_mailbox` emits `orch-mailbox-changed`, exactly
    /// as `write_questions` emits under `questions_lock`).
    ///
    /// There is no delivery on any path here. That is the point of the feature:
    /// a mailbox write is what happens INSTEAD of typing into the pane.
    pub(super) mailbox_lock: TrackedMutex<()>,
    /// Serializes every read-modify-write of a group's `usage.json` (#743 S4b).
    ///
    /// **A leaf of its own, split out of `tasks_lock`.** The usage store used to
    /// share the board's lock, which put per-live-agent transcript reads and a
    /// full `usage.json` rewrite inside the one process-global mutex that the
    /// whole task board also serializes on — so one group's board write stalled
    /// another group's usage poll, and vice versa, on the app's hottest polled
    /// path. The two files have no shared invariant: nothing reads a task and a
    /// usage snapshot together, so nothing needed them under one lock.
    ///
    /// **Lock order: takes no other registry lock while held** except
    /// `AUDIT_LOCK`, on `load_usage_snapshots`' corrupt-file branch — the same
    /// nesting the `tasks_lock` version already had. Callers hold no registry
    /// lock when they take it.
    pub(super) usage_lock: TrackedMutex<()>,
    /// Serialises every read-modify-write of a group's `deferred.json`
    /// (#3304 S1), and with it the decision to hold a notice back at all.
    ///
    /// One registry-wide lock rather than one per group, `mq_state_lock`'s
    /// argument: the only long hold is the flush tick, which services one
    /// group at a time, so a per-group map buys no concurrency at the cost of
    /// an ordering question every future caller would have to get right.
    ///
    /// Ranked (`lockorder::TRIAGE_DEFER`) rather than left unranked like its
    /// three state-lock siblings, because it is taken on the DELIVERY path —
    /// the one path in this file that every producer funnels through — so a
    /// future caller reaching it while holding a map is the inversion most
    /// worth catching at the moment it is written.
    pub(super) triage_defer_lock: Arc<TrackedMutex<()>>,
    /// Per-group memo for the polled usage read (#743 S4b) — the value
    /// [`OrchRegistry::group_usage_within`] serves when the stored one is
    /// younger than the caller's `max_age`.
    ///
    /// **Why it exists.** `group_usage_live_within` is the heaviest thing on a
    /// cadence in this app (per-live-agent transcript reads plus a `usage.json`
    /// read-modify-write). Three callers used to ask for it inside the same ~2 s
    /// tick — the group view, the tab bar, and `orch_autonomy`'s budget meter —
    /// and the memo made that one computation instead of three.
    ///
    /// Since #1608 the frontend asks for none of them: the snapshot publisher
    /// computes `usage` and `autonomy` in one pass, so the memo's remaining
    /// callers are that pass and the `group_usage` MCP tool. It is not redundant
    /// yet for exactly that reason — see `docs/design/polled-views.md`.
    ///
    /// **`Arc<Mutex<..>>` per group, not one map lock.** The outer map lock is
    /// held only long enough to clone the per-group cell out (pty.rs's
    /// map-lock → release → leaf-lock rule, applied here); the cell is then held
    /// across the computation, which is what collapses a concurrent stampede
    /// into a single compute rather than merely a shorter one.
    ///
    /// **Invalidation drops the map entry and never locks the cell**
    /// (`invalidate_usage_memo`), so an external writer can never deadlock
    /// against a computation in flight. A computation whose cell was dropped
    /// mid-flight stores into an orphan and the next caller recomputes: one
    /// wasted computation, never a stale answer.
    ///
    /// **Why the invalidation set is `{upsert_usage_snapshot, end_group}` and
    /// not, say, spawn.** The two are not symmetric. A newly spawned agent
    /// whose row appears up to a window late is harmless — it has no spend yet,
    /// and the next tick shows it. A killed agent that keeps rendering as LIVE
    /// is not: its snapshot has just been captured from outside this
    /// computation, and a group view showing a dead agent as live is a wrong
    /// statement about what is running. Invalidate where being late is wrong,
    /// not everywhere something changed.
    ///
    /// **Bounded by LIVE groups**: `end_group` drops the entry, so the map does
    /// not accumulate one per group this process ever opened.
    ///
    /// **Two values per cell, one computation** (#1317): the full whole-roster
    /// value and [`live_usage_view`] of it, derived together and stamped with
    /// one `Instant`. Storing the projection beside its source is what keeps
    /// the polled path from cloning the whole-of-session roster once per call
    /// just to drop the historical rows — the second value is a subset of the
    /// first, so the cell costs a fraction more memory and saves an O(roster)
    /// clone per poll. Two cells would let the two drift onto different
    /// windows; a projection computed per caller would put the allocation back.
    pub(super) usage_memo: TrackedMutex<HashMap<GroupId, Arc<TrackedMutex<Option<(std::time::Instant, Value, Value)>>>>>,
    /// Everything the usage-SERIES sampler needs to remember between ticks
    /// (#2011 slice B) — see [`SeriesState`].
    ///
    /// **Never held across another lock.** The sampler runs inside
    /// `compute_group_usage`, which has already released `usage_lock` by then;
    /// this lock is taken, the decision is made, it is released, and only then
    /// is the append performed. Unranked for that reason: it participates in no
    /// ordering because it is never one of two.
    ///
    /// **Bound, stated exactly, because the easy comparison is wrong** (#2941
    /// review round 2). One entry per group this process has sampled, holding
    /// one [`usageseries::Sample`] (~300 B) per usage key ever sampled for that
    /// group. Nothing prunes it — there is no `invalidate_series_state` the way
    /// [`Self::usage_memo`] has `invalidate_usage_memo`, so this is NOT "the
    /// same population and lifetime" as that map: the memo sheds entries and
    /// this does not. A long-lived group that recycles agents therefore grows
    /// this map by one key per recycled session, for the process's life.
    ///
    /// Left as it is, deliberately. The population is usage KEYS (a CLI session
    /// id, or `agent:<id>`), which a human's session produces in the hundreds
    /// at most, so the ceiling is tens of KB; and each entry is exactly the
    /// thing that stops a restarted process re-appending a row for a key it has
    /// already written, so evicting one buys memory and costs a duplicate row.
    /// If a group ever churns keys fast enough to matter, the eviction to write
    /// is by key age against the series bucket, not by group.
    pub(super) series_state: TrackedMutex<HashMap<GroupId, SeriesState>>,
    /// Test-only override of [`usageseries::SERIES_BUCKET_MS`], so a test can
    /// drive real ticks without sleeping five minutes. `None` in production.
    /// Mirrors `claude_projects_dir`.
    pub(super) series_bucket_override: TrackedMutex<Option<u64>>,
    /// Test-only override of [`SERIES_REVISIT_BYTES`], so the oversize REPORT
    /// can be pinned on the real payload path without writing 32 MB of fixture
    /// to disk. `None` in production. Mirrors `series_bucket_override`.
    pub(super) series_revisit_override: TrackedMutex<Option<u64>>,
    /// Test-only override of every poll-path read ceiling
    /// ([`AUDIT_READ_LIMIT_BYTES`], [`SERIES_READ_LIMIT_BYTES`]) (#3469), so
    /// the refusal path is driven through the reader's own limit rather than
    /// by exhausting memory. `None` in production.
    pub(super) poll_read_limit_override: TrackedMutex<Option<u64>>,
    /// Which `(group, reader)` poll-path reads are currently failing and have
    /// already been reported (#3469; see `note_poll_read`). Bounded by groups
    /// × the two readers, and an entry is removed by the next success.
    pub(super) poll_read_failed: TrackedMutex<HashSet<(GroupId, &'static str)>>,
    /// Per-REPO memo for the display-only default-branch name (#743 S4a),
    /// keyed by repo path so two groups on one repo share the answer.
    ///
    /// `orch_workflow_status` was in the group view's 2 s batch and resolving
    /// this name costs 2-4 blocking `git` spawns, so it was 2-4 process spawns
    /// every 2 s per open group view. Since #1608 the caller is the snapshot
    /// publisher's view tier, once per second and only for a group holding a
    /// view lease — so the memo now bounds a 1 s cadence rather than a 2 s one,
    /// and the spawns it saves are the same spawns. The name is display-only and already
    /// documented as unboundedly stale (see [`crate::git::default_branch_name`]:
    /// it reads local refs, so it is only as fresh as whatever last fetched);
    /// a coarse TTL therefore adds a *bounded* staleness on top of an unbounded
    /// one, and no gate reads it.
    ///
    /// The lock is never held across the `git` spawns — check, release, resolve,
    /// insert.
    ///
    /// **Its bound, stated because every other map here states one**: nothing
    /// evicts, so this holds one small entry per distinct repo path for the
    /// process lifetime. That is deliberate rather than overlooked. Unlike
    /// [`Self::usage_memo`] there is no lifecycle event to hang eviction on — a
    /// repo outlives the groups opened on it, which is exactly why the key is
    /// the repo and not the group — and the population is the set of repos a
    /// human has opened in one session: single digits, a branch name each.
    /// Eviction machinery would cost more than the thing it reclaims.
    pub(super) default_branch_memo: TrackedMutex<HashMap<String, (std::time::Instant, Option<String>)>>,
    /// Serializes group creation + orchestrator registration: the group id
    /// is chosen by liveness, and a group only becomes live once its
    /// orchestrator is registered — without this, two concurrent launches
    /// on one repo would share an id.
    pub(super) creation: TrackedMutex<()>,
    /// Serializes a durable per-group **marker toggle** — the set mutation and
    /// the marker file write/remove that makes it survive a restart — as one
    /// unit (#743 S7 rev-231 N1). `set_notify` / `set_spawn_expanded`, and since
    /// #762 the four **consent** toggles: `set_autonomous_as`, `set_auto_merge`,
    /// `set_auto_release`, `set_dangerous_mode`.
    ///
    /// **Why a second lock rather than just holding the set's.** The marker
    /// file is load-time truth: it is what rebuilds the set at startup, so a
    /// set mutation and its file write that can interleave do not merely race
    /// for an instant — they can leave the file saying ON while memory says
    /// OFF, and that divergence PERSISTS across the next restart, which then
    /// reads the file. Holding the set's own lock across the write would fix
    /// that and reintroduce exactly what INV-5 forbids: file IO under a lock a
    /// poll path takes (`notify_enabled` / `spawn_expanded` are read by the
    /// group view). So the *ordering* moves to a lock that no poll path ever
    /// takes, and the set lock goes back to being a leaf held for one insert.
    ///
    /// This is deliberately structural rather than an appeal to dispatch.
    /// Both toggles are sync commands today, so the one webview thread already
    /// serialises them and the interleave cannot occur — but that is a fact
    /// about the *callers*, invalidated silently by the first background or
    /// MCP caller, and #743's own slices are converting sync commands to
    /// off-thread ones. `GitWatcher::watch`'s released window in this same
    /// change is safe by construction (its re-check), and this is held to the
    /// same standard rather than a weaker one.
    ///
    /// **#762 is the caller that paragraph predicted, and the consent toggles
    /// joined for the reason it gives.** They have the identical shape — reserve
    /// in a set, then write or remove a marker — and their divergence is worse
    /// than a stale badge: a disable racing an enable removes a marker that has
    /// not been written yet (`remove_marker` maps NotFound to Ok), clears the
    /// set, and lets the enable recreate the file afterwards. Memory then says
    /// OFF while disk says ON, and the next restart's re-seed reads DISK —
    /// restoring merge, release or autonomous authority the human explicitly
    /// withdrew, audited as a benign `*-resumed`. Found in review of #762
    /// (rev-260 B1) against a paragraph that had claimed it was impossible.
    ///
    /// The enable arm of `set_autonomous_as` computes its budget anchor
    /// (`group_token_total`, a usage aggregation over every pane) inside this
    /// window, deliberately: it is the widest part of the reserve-to-write gap,
    /// so excluding it would protect the narrow half and ship the wide one. The
    /// cost is that another toggle can wait behind it — a human click behind a
    /// human click's work, never a poll or a paint, and never a pty write, since
    /// every delivery is outside the guard (§2 P6).
    ///
    /// **`suspend_autonomous` deliberately does NOT take this lock**, so the
    /// idle-tick budget money-stop can never wait on a human's toggle. The
    /// enable-vs-suspend interleave it leaves is the one variant that IS
    /// reconciled at restart: a suspension co-writes `autonomy_suspended`, and
    /// `create_group`'s re-seed checks that marker first and forces the group
    /// back OFF regardless of a surviving `autonomous` marker.
    ///
    /// Lock order: `marker_io` is taken FIRST and outermost — the set locks
    /// and `AUDIT_LOCK` are taken under it, never the reverse. One lock for
    /// both toggles, not one per group: these fire on a human's click.
    ///
    /// `pause_group`/`resume_group` have the same shape and the same latent
    /// divergence, and are NOT folded in here — they predate this slice
    /// (indeed the census cites `pause_group` as the pattern this drifted
    /// from), and `resume_group` does far more than marker IO, so bringing it
    /// under one lock is its own argument, not this change's. That still holds
    /// after #762: a pause marker that disagrees with memory costs a suppressed
    /// or duplicated delivery, not a restored authority, which is why the
    /// consent toggles were worth the widening and these two are still not.
    pub(super) marker_io: TrackedMutex<()>,
    /// Serializes a live **guardrail** edit — the `group.json` read-modify-write
    /// and the in-memory publish that follows it — as one unit (#762, F2 of
    /// #743). `set_max_agents`, `set_advanced_orchestrator`, and every
    /// `persist_guardrail_*` setter (budget, idle tick, activity floor, the four
    /// compact-nudge knobs).
    ///
    /// **What it is protecting, precisely.** Each of those setters is a
    /// read-modify-write of ONE file: read `group.json`, patch one key, write
    /// the whole document back. Two of them running at once on the same group
    /// both read the pre-state, and the second write silently drops the first
    /// one's key — the classic lost update, on the file that carries the
    /// group's identity and its consent-bearing guardrails. Serializing the
    /// publish too (not just the write) is what keeps disk and memory from
    /// settling in opposite orders: without it, A-persists, B-persists,
    /// B-publishes, A-publishes leaves the file saying 5 and the enforcement
    /// path reading 3, and nothing re-reads the file until the next restart.
    ///
    /// **Why this is a new lock and not a pre-existing one.** It is [`Self::
    /// marker_io`]'s argument applied to the other durable per-group store, and
    /// it exists for the same reason: until #762 every one of these setters was
    /// a *sync* `#[tauri::command]`, so the single webview thread serialized
    /// them for free and the interleave could not occur. That was a fact about
    /// the callers, not about the code — and #762 is exactly the change that
    /// invalidates it. `atomic_write`'s own doc already states the shipped
    /// position that `group.json` has concurrent writers and must therefore
    /// never share a temp name; this is the other half of that, for the
    /// read-modify-write the unique temp name cannot make atomic.
    ///
    /// Lock order: taken FIRST and outermost — the `groups` lock and
    /// `AUDIT_LOCK` may be taken under it, never the reverse — and released
    /// before any pane delivery or merge-gate sync, so nothing waits on it
    /// across a write into a pty. One lock for the family rather than one per
    /// group: these fire on a human's click.
    ///
    /// **This is file IO under a lock, which INV-5 permits only with the
    /// question answered rather than dodged.** The invariant's rule is "no file
    /// IO under a lock a *poll path* takes", and no poll path takes this one —
    /// it is reachable only from the nine live guardrail setters, each a human
    /// gesture. The reads those setters race with (`group`, `workflow_status`,
    /// the idle-tick and compact-nudge passes) go to the in-memory guardrails
    /// and take the `groups` lock, which stays a leaf held for one lookup. So
    /// the cost of holding this across a small-file rewrite is paid by the next
    /// *click*, never by a tick or a paint.
    ///
    /// **Not** taken by `create_group`'s first write of `group.json` (under
    /// `creation`, §4 X6) or by the marker toggles: a group being created has
    /// no guardrail UI to race, and a marker file is a different store with its
    /// own ordering lock.
    pub(super) group_file_io: TrackedMutex<()>,
    /// Test seam (#222): when set, `pr_head` returns this instead of shelling out
    /// to `gh pr view --json headRefOid`. The verdict↔revision binding has to be
    /// exercised through the real MCP dispatch against a repo that isn't on GitHub;
    /// mirrors `claude_projects_dir`. `None` in the app, always.
    pub(super) pr_head_override: TrackedMutex<Option<String>>,
    /// Test seam (#565), the body half of `pr_head_override`: when set, `pr_body`
    /// returns this instead of shelling out to `gh pr view --json body`. Lets the
    /// integration tests record a verdict against a known body and then edit it —
    /// the race #565 is about — without a live GitHub PR. `None` in the app.
    pub(super) pr_body_override: TrackedMutex<Option<String>>,
    /// Test seam (#1176), the changed-files half of `pr_head_override`: when set,
    /// `pr_changed_files` returns this list instead of shelling out. Lets the
    /// integration tests drive path-based reviewer routing against a known diff
    /// without a live GitHub PR. `None` in the app — and `Some(vec![])` is a real
    /// answer (a PR that changed nothing), which is why this is not `Vec<String>`.
    pub(super) pr_files_override: TrackedMutex<Option<Vec<String>>>,
    /// Test seam (#791): when set, `gh_capture` runs THIS program on THIS
    /// deadline instead of the resolved `gh` on `GH_CAPTURE_TIMEOUT`.
    ///
    /// The property it exists for — the `gh` reads on the MCP request path are
    /// **bounded**, so a stalled child returns a diagnosable error instead of
    /// wedging the calling agent's turn — is not reachable through the two seams
    /// above: they short-circuit before the spawn, which is precisely their job.
    /// Nor through the real `gh`, which no test in this repo may run
    /// (constraint 3) and which answers in milliseconds on a CI runner anyway.
    /// So a test points this at a script that outlives the deadline and asserts
    /// the call comes back at all. The deadline rides along because the
    /// production bound is 20 seconds and a suite that spends 20 seconds per
    /// timeout assertion is a suite people stop running. `None` in the app.
    pub(super) gh_exec_override: TrackedMutex<Option<(PathBuf, Duration)>>,
    /// Groups the human has paused: loomux stops delivering prompts/kickoffs
    /// to them so their agents idle out (see `deliver_prompt`). Mirrored to a
    /// `paused` marker file per group so it survives restarts.
    ///
    /// Read on TWO delivery paths since #569, and both are load-bearing: the
    /// front door holds an arriving payload in the pane's queue rather than
    /// pasting it, and `run_queue_drainer` refuses to paste what is already
    /// queued. Neither alone is sufficient — see `deliver_prompt`'s pause
    /// branch for the restart case that reaches the drainer without passing
    /// the front door.
    pub(super) paused: TrackedMutex<HashSet<GroupId>>,
    /// Per-group spawn timestamps (Unix-ms) for the spawn-rate guardrail;
    /// pruned to the trailing hour on each check.
    pub(super) spawn_times: TrackedMutex<HashMap<GroupId, Vec<u64>>>,
    /// Weak handle to our own `Arc`, set once at startup (`set_self_arc`), so
    /// `&self` methods can hand an owned registry to background threads (e.g.
    /// the copilot session watcher). `Weak` avoids a self-referential `Arc`
    /// cycle that would leak the registry.
    pub(super) self_arc: TrackedMutex<Weak<OrchRegistry>>,
    /// Attention routing (#6): latched worker reports awaiting the human's
    /// eyes — agent id → "done" | "blocked". Set by the report tool, cleared on
    /// ack (the human focused the pane) or reassignment.
    pub(super) attn_reports: TrackedMutex<HashMap<String, &'static str>>,
    /// Attention routing: per-agent output-quiet tracking, agent id → (last pty
    /// output total, Unix-ms that total last changed). Kept separate from the
    /// watchdog's counter so the two features never clobber each other's clocks.
    pub(super) attn_quiet: TrackedMutex<HashMap<String, (u64, u64)>>,
    /// Attention routing: agents whose live `waiting` badge the human has acked
    /// (focused the pane) while the prompt is still on screen. Unlike
    /// `blocked`/`report`, `waiting` is recomputed every scan, so without this it
    /// would re-light ~3s after focus. Cleared when the pane's output next
    /// changes (the menu was answered / the CLI repainted) so a genuinely new
    /// prompt flags again. See `attention_tick`.
    pub(super) attn_waiting_ack: TrackedMutex<HashSet<String>>,
    /// Attention routing: the agent → reason set last emitted, so a scan fires a
    /// desktop toast only once per attention onset (the event itself is
    /// re-emitted every tick and the frontend badges idempotently).
    pub(super) attn_emitted: TrackedMutex<HashMap<String, String>>,
    /// **agent id → the [`providerlimit`] provider id whose refusal that pane is
    /// showing** (#2811 S5b), rewritten WHOLE by every `attention_tick`.
    ///
    /// The review driver holds a drive whose panes are stopped on a provider's
    /// spend limit, and it must not grow a pane-text reader of its own to know
    /// that: plan-2504 is explicit that the attention scan owns the one
    /// pane-text classifier, and a second scanner would be a second answer that
    /// can disagree with the chip the human is looking at. So the scan — which
    /// already computes this every three seconds, off the MASKED tail, with the
    /// paragraph-start anchor — publishes what it found and `rdtick` reads it.
    ///
    /// **Rewritten whole rather than updated**, so a pane that recovered simply
    /// stops appearing and no clearing path has to be remembered. An empty map
    /// therefore means "no pane is limited" AND "the scan has not run", which
    /// are deliberately the same answer here: both are `None` in
    /// `DriveFacts::provider_limited`, and that field's doc argues the fail
    /// direction — a drive is never held on a reading orrerix could not make.
    pub(super) attn_provider_limit: TrackedMutex<HashMap<String, String>>,
    /// Attention routing (#496 PR-C): agent id → the stranded-delivery state
    /// of its pane — a delivery the late monitor declared `Failed`, and
    /// whether loomux is self-healing it or needs the human. Latched (unlike
    /// `waiting`, which is recomputed every scan) because the condition it
    /// describes is a past event the pane's own output cannot re-derive:
    /// nothing about a wedged pane changes until something submits the
    /// prompt. Cleared by `clear_stranded` when the ledger says the delivery
    /// resolved, and pruned in `attention_tick` when the agent stops running.
    pub(super) attn_stranded: TrackedMutex<HashMap<String, StrandedNote>>,
    /// Attention routing (#946 Q4 / #1091 slice H, the latched-attention
    /// belt): agent ids currently holding delivery on `HeldReason::
    /// InteractiveQuestion` (#420) for their OWN pane. Written only for the
    /// ORCHESTRATOR — narrower than the Q4 CLI deny, which also covers the
    /// liaison — because `deliver_now` gates on `target_is_orchestrator`
    /// alone; see `deliver_now`'s `emit_held`/`emit_held_cleared` closures,
    /// which are the sole writers. Latched, not RE-DERIVED each scan the way
    /// `waiting` is, because there is nothing for `attention_tick` to
    /// re-derive it from: the hold lives on `deliver_now`'s own call stack
    /// (blocked inside `hold_for_human_input`, capped at `QUESTION_HOLD_MAX`
    /// — 120s), not in any pty state a periodic scan could read the way
    /// `waiting` reads a prompt-shaped tail. So the hold has to be mirrored
    /// into this shared map at the moment it starts, or `attention_tick`
    /// would have no way to know it is happening at all. Cleared the instant
    /// the hold itself clears (`emit_held_cleared` fires unconditionally when
    /// a hold that was entered ends, whatever the outcome — including at the
    /// `QUESTION_HOLD_MAX` cap, even if the dialog is still on screen; see
    /// `wait_for_question_clear`), and pruned in `attention_tick` when the
    /// agent stops running, mirroring `attn_stranded`.
    ///
    /// **Disjoint from #1091 slice D's `question` reason by construction.**
    /// D's reason is DERIVED from the engine's `ask_human` registry (a
    /// question the orchestrator posed and is waiting to be answered — #946
    /// Q1 state, no pane involved). This map is about the delivery PIPE
    /// itself being held on a live interactive dialog on screen — a
    /// different signal, populated by a different subsystem. The two sit
    /// beside each other in `attention_tick`'s `reason` match, never merged
    /// into one.
    pub(super) attn_question_held: TrackedMutex<HashSet<String>>,
    /// Groups with desktop notifications enabled (durable `notify` marker file).
    pub(super) notify_groups: TrackedMutex<HashSet<GroupId>>,
    /// Autonomous mode (#83): groups whose orchestrator is idle-ticked to run its
    /// monitoring/intake cadence unattended. Durable via an `autonomous` marker
    /// file whose *content* is the enable-time usage-token anchor (see
    /// `set_autonomous` / `autonomy_anchor`), so budget metering survives restarts.
    pub(super) autonomous_groups: TrackedMutex<HashSet<GroupId>>,
    /// Autonomous mode (#83): groups where the orchestrator may merge an
    /// adequately-tested PR itself instead of holding at the human merge gate.
    /// Default OFF (absent) = today's behavior (human merges). Durable
    /// `auto_merge` marker file, mirroring `notify`/`paused`. The behavior lives
    /// in the orchestrator template; the backend stores/exposes the flag and
    /// mirrors it into the orchestrator's kickoff config.
    pub(super) auto_merge_groups: TrackedMutex<HashSet<GroupId>>,
    /// Autonomous mode (#83): groups where the orchestrator may publish a
    /// release/tag itself (`gh release …`, pushing a `v*` tag) instead of needing a
    /// per-tag human grant. **Independent of `auto_merge`** — the human can allow
    /// auto-merge while keeping releases manual, or opt into both. Default OFF
    /// (absent), so turning autonomous on never surprise-publishes. Durable
    /// `auto_release` marker; gated behind autonomous exactly like `auto_merge`.
    pub(super) auto_release_groups: TrackedMutex<HashSet<GroupId>>,
    /// Full autonomy (#778): groups whose orchestrator self-selects eligible work
    /// on its idle tick instead of waiting for the human's opt-in label funnel.
    /// A dependent toggle of autonomous mode exactly like `auto_merge` /
    /// `auto_release` — enabling requires autonomous ON, and autonomous-off or a
    /// budget suspension force-clears it. Durable `full_autonomy` marker whose
    /// *content* is the enable-time goal string (the autonomous marker's
    /// anchor-in-content precedent: consent and its parameter captured together).
    pub(super) full_autonomy_groups: TrackedMutex<HashSet<GroupId>>,
    /// Supervised dangerous mode (#83): groups where the human — present and
    /// supervising — has authorized the orchestrator to merge/release itself
    /// WITHOUT being autonomous. Default OFF. **Mutually exclusive with
    /// `autonomous`**: enabling autonomous force-clears this, and enabling this is
    /// rejected while autonomous. Durable `dangerous_mode` marker; the gate's single
    /// decision point allows a privileged action via `(dangerous && !autonomous)`.
    pub(super) dangerous_groups: TrackedMutex<HashSet<GroupId>>,
    /// Groups that opted BACK OUT of the #260 default (delegate panes open
    /// docked/minimized so a burst of spawns doesn't crowd the orchestrator
    /// out of focus) and want every spawned pane to open expanded into the
    /// split tree, matching pre-#260 behavior. Default OFF (absent) = the new
    /// minimize-on-spawn default applies; presence is the opt-out, so this
    /// set's *meaning* is inverted from `notify_groups`/`autonomous_groups`
    /// (there, presence enables a default-off feature; here, presence
    /// disables a default-on one). Durable `spawn_expanded` marker file.
    pub(super) spawn_expanded_groups: TrackedMutex<HashSet<GroupId>>,
    /// Autonomous mode (#83): per-group idle-tick delivery timestamps (Unix-ms)
    /// for the `MAX_IDLE_TICKS_PER_HOUR` backstop; pruned to the trailing hour on
    /// each check. The runaway analogue of `spawn_times`.
    pub(super) idle_tick_times: TrackedMutex<HashMap<GroupId, Vec<u64>>>,
    /// Compact-nudge (#287): per-group `/compact` nudge delivery timestamps
    /// (Unix-ms) for the `MAX_COMPACT_NUDGES_PER_HOUR` backstop; pruned to the
    /// trailing hour on each check. Mirrors `idle_tick_times`.
    pub(super) compact_nudge_times: TrackedMutex<HashMap<GroupId, Vec<u64>>>,
    /// Debounced cap-change notices (#79): group → its pending, not-yet-
    /// delivered `PendingMaxNotice`. `set_max_agents` folds rapid stepper
    /// clicks in here (persist/enforce/audit stay per-click); the
    /// `start_max_notice_flusher` loop delivers one coalesced notice per burst
    /// once the group falls quiet.
    pub(super) pending_max_notice: TrackedMutex<HashMap<GroupId, PendingMaxNotice>>,
    /// Which opencode-store degrade (#722) has already been audited for a
    /// group, so a persistent one costs one line instead of one per poll.
    ///
    /// Keyed by group, holding the KIND (`"open"` / `"unreadable"`), never the
    /// message: a message could in principle vary between reads, and a latch
    /// that re-fires on a changed string is not a latch. Same episode shape as
    /// [`HoldEpisode::announced`] — the entry is dropped the moment a read
    /// succeeds, so a condition that recurs after a genuine recovery is
    /// diagnosed again rather than silenced for the life of the process.
    ///
    /// `Unavailable::Absent` is deliberately never recorded here and never
    /// audited: a group whose opencode panes have not booted has no store yet,
    /// which is the ordinary state, not an incident.
    pub(super) opencode_db_degraded: TrackedMutex<HashMap<GroupId, &'static str>>,
    /// Per-transcript parse cursors for the usage poll (#1239).
    ///
    /// `compute_usage_snapshot` reads a live agent's transcript on every
    /// `group_usage` computation, which the polled UI drives at most once per
    /// [`USAGE_POLL_MAX_AGE`]. Before this, each of those reads re-parsed the
    /// WHOLE file from byte zero; now each one folds on only the bytes the
    /// agent appended since the previous read. See
    /// [`crate::usage::TranscriptCursors`] for the contract and for what
    /// invalidates a cursor.
    ///
    /// **Its bound, stated because every other map here states one**: one
    /// entry per `(projects root, session id)` polled in the last
    /// `CURSOR_TTL`, evicted on access. That is the set of live agents, plus
    /// recently-dead ones for a few minutes; each entry costs its message-id
    /// dedupe set, which is ids only.
    pub(super) usage_cursors: crate::usage::TranscriptCursors,
    /// Test-only override of the Claude transcript root (`~/.claude/projects`).
    /// `None` in production. Set via `set_claude_projects_dir` so the usage
    /// reader can be pointed at a fixture tree without touching global env —
    /// safe under parallel test execution.
    pub(super) claude_projects_dir: TrackedMutex<Option<PathBuf>>,
    /// Test-only override of Claude's own custom-agent directory
    /// (`~/.claude/agents`) — the generated per-block contract file (round
    /// #417 correction 6) writes here. `None` in production. Mirrors
    /// `copilot_agents_dir_override`, which mirrors `claude_projects_dir`.
    pub(super) claude_agents_dir_override: TrackedMutex<Option<PathBuf>>,
    /// Test-only override of Copilot's own custom-agent directory
    /// (`~/.copilot/agents`) — the generated per-block contract file (#416)
    /// writes here. `None` in production. Mirrors `claude_projects_dir`.
    pub(super) copilot_agents_dir_override: TrackedMutex<Option<PathBuf>>,
    /// Test-only override of codex's own home (`$CODEX_HOME`, else
    /// `~/.codex`) — the generated per-agent profile file (#2515 C1) writes
    /// here. `None` in production. Mirrors `copilot_agents_dir_override`.
    ///
    /// A whole HOME rather than a subdirectory, unlike its two neighbours,
    /// because that is the shape codex's own knob has: the profile file sits
    /// directly under `CODEX_HOME` beside `config.toml`, which is also the
    /// file `codex_user_mcp_exposure` reads. One override moves both, so a
    /// test cannot end up writing a fixture profile into a temp dir while
    /// measuring the developer's real config for exposure.
    pub(super) codex_home_override: TrackedMutex<Option<PathBuf>>,
    /// Test-only override of the #417 compact-hook script directory
    /// (`compact_hook_dir`, normally a sibling of `shim_dir()` under the real
    /// per-machine loomux data dir). Without this, `compact_hook_dir`
    /// derives from `self.root.parent()` — which for `test_registry()`'s
    /// disposable tempdir is the SHARED SYSTEM TEMP DIRECTORY, so every test
    /// that spawns a Claude agent (the common case) would write/read the
    /// SAME real script file, racing every other such test in the suite
    /// (rev-4 review round: an intermittent flake in exactly the one test
    /// that reads the script's content back traced to this). `None` in
    /// production. Mirrors `claude_projects_dir`.
    pub(super) compact_hook_dir_override: TrackedMutex<Option<PathBuf>>,
    /// Test-only override of Copilot's own user-level hooks directory
    /// (`~/.copilot/hooks`, or `$COPILOT_HOME/hooks`) — the SAME real-shared-
    /// path test-isolation concern `compact_hook_dir_override` documents
    /// applies here too (a single, GLOBAL, machine-wide config file every
    /// test registry would otherwise race on). `None` in production. Mirrors
    /// `claude_projects_dir`.
    pub(super) copilot_hooks_dir_override: TrackedMutex<Option<PathBuf>>,
    /// Low-disk backstop latch (#134): true once the one-per-episode disk-space
    /// notice has been delivered, cleared when free space recovers past
    /// `LOW_DISK_CLEAR_BYTES`. Machine-wide (the disk is shared across groups).
    pub(super) low_disk_notified: TrackedMutex<bool>,
    /// Per-group count of unreadable audit lines already breadcrumbed (#240).
    /// The viewer re-polls `audit_log` in follow mode, so a log that already
    /// carries torn lines — every log written before the append fix — would
    /// otherwise emit a breadcrumb per poll and flood out the crash-forensics
    /// history it shares the file with. Report only when the count *changes*.
    pub(super) audit_skips_notified: TrackedMutex<HashMap<GroupId, usize>>,
    /// Notification backend (#243): registered self-addressed CI/run watches,
    /// keyed by watch id. Same lifetime class as the `attn_*` maps —
    /// per-live-agent, in-memory only (see `notify.rs`'s module doc and the
    /// design note's persistence rationale).
    pub(super) watches: TrackedMutex<HashMap<String, notify::Watch>>,
    /// Watch id sequence (`n-1`, `n-2`, …) — an `AtomicU32` like every other
    /// id this registry mints, so no `getrandom`-pulling crate is needed
    /// (CLAUDE.md constraint 2).
    pub(super) notify_seq: AtomicU32,
    /// Idle-tick intake gate (#332): per-group last-seen intake state (the
    /// intake-labeled set per open issue, the coarse check-state per open PR)
    /// the host-side poller diffs against. In-memory only, the same lifetime
    /// class as `watches`/`idle_tick_times`: a restart re-fires once on
    /// whatever is currently labeled/terminal (harmless — see
    /// `intake::label_deltas`'s doc) rather than persisting across restarts.
    pub(super) intake_seen: TrackedMutex<HashMap<GroupId, intake::IntakeSeenState>>,
    /// Idle-tick intake gate (#332): Unix-ms this group's poller last actually
    /// called `gh` (not merely last considered) — `intake::due_intake_polls`'s
    /// per-group interval floor.
    pub(super) intake_last_poll_ms: TrackedMutex<HashMap<GroupId, u64>>,
    /// Unified `gh` poller (#406): Unix-ms the intake half last ran its
    /// due-group SCAN — process-wide, one entry, not per group. Distinct from
    /// `intake_last_poll_ms` above, which is the per-group `gh`-call floor:
    /// this one only decides which wakes of the single background loop run
    /// the intake scan at all (`intake_scan_due`), preserving the coarser
    /// scan cadence the intake poller had when it owned its own thread.
    /// `None` until the first wake, so the first tick after launch scans.
    pub(super) intake_last_scan_ms: TrackedMutex<Option<u64>>,
    /// Idle-tick intake gate (#332): the composed wake summary from every
    /// poll that found something new since the last delivery, pending
    /// delivery on the next idle tick that actually fires for this group.
    /// Bounded (`intake::PendingIntake`, rev-33 finding B2, #429) — the
    /// poller and the idle tick run on independent clocks, so an
    /// output-active group (the tick's quiet window never clears) can
    /// accumulate many polls' worth before anything consumes it. Cleared the
    /// moment a tick consumes it — whether the fire was triggered BY this
    /// signal or a later fallback fire swept it up alongside something else.
    pub(super) intake_pending: TrackedMutex<HashMap<GroupId, intake::PendingIntake>>,
    /// Idle-tick intake gate (#332): Unix-ms this group's orchestrator was
    /// last actually woken by an idle tick (a gated fire, a fallback fire, or
    /// any fire while the gate is disabled) — `intake::idle_tick_fallback_due`'s
    /// reference point.
    pub(super) idle_tick_last_fired_ms: TrackedMutex<HashMap<GroupId, u64>>,
    /// Idle-tick fallback backoff (#864): how many consecutive unconditional
    /// fallback wakes this group has taken that found NOTHING — no intake
    /// signal, no pending CI watch, no watchdog stall (the `heartbeat` fire in
    /// `idle_tick_tick`). Widens the effective fallback interval
    /// (`intake::fallback_interval_minutes`); reset to 0 by any delta, any
    /// human input in the orchestrator pane, any real orchestrator output, or
    /// any live delegate in the group.
    ///
    /// In-memory only, the same lifetime class as `idle_tick_times`/
    /// `idle_tick_last_fired_ms`: a restart resets the streak, which is the
    /// safe direction (the group goes back to its BASE cadence and has to
    /// re-earn the backoff by observing quiet again) and is why nothing
    /// persists it.
    pub(super) idle_tick_empty_streak: TrackedMutex<HashMap<GroupId, u32>>,
    /// Notification backend (#243): per-group "we last saw this group as
    /// paused at tick-time T" bookkeeping, used by `notify_tick` to freeze
    /// the TTL clock across a pause. A group appears here only while
    /// currently believed paused by the tick mechanism; the entry is removed
    /// (after crediting every one of its watches with the elapsed span) the
    /// first tick that observes it unpaused. See `notify_tick`'s doc for why
    /// this can't simply live on `pause_group`/`resume_group` (they use real
    /// wall-clock time, which isn't the `now` a test injects).
    pub(super) paused_watch_since: TrackedMutex<HashMap<GroupId, u64>>,
    /// Named lock resources (#858): per-group live lock state, keyed by group
    /// id and built lazily from that group's declared `resources:` block. Same
    /// lifetime class as `watches` — **in-memory only, deliberately**: every
    /// pane that could have held a lock dies with the process, so a lock file
    /// surviving a restart could only ever describe holders that no longer
    /// exist.
    pub(super) locks: TrackedMutex<HashMap<GroupId, locks::LockTable>>,
    /// The lock sweep's pause bookkeeping — `paused_watch_since` for holds and
    /// queued requests, and separate from it on purpose: two tick paths
    /// crediting the same map would charge one pause span twice. See
    /// `locks_tick`.
    pub(super) paused_locks_since: TrackedMutex<HashMap<GroupId, u64>>,
    /// Cross-workspace channels (#271): human-connected sets of two-or-more
    /// agent panes that may span different groups, keyed by channel id. Same
    /// lifetime class as `watches` — in-memory only, no persistence across a
    /// restart (see the design note's persistence rationale). One OS
    /// process / one registry means "cross-workspace" is just shared state
    /// here, not a second transport.
    pub(super) channels: TrackedMutex<HashMap<String, Channel>>,
    /// agent id -> channel id, the enforcement point for "a pane is in at
    /// most ONE channel at a time" (the justified invariant — see the design
    /// note's Membership section) and an O(1) lookup for `channel_status`
    /// and `channel_send`.
    pub(super) agent_channel: TrackedMutex<HashMap<String, String>>,
    /// Channel id sequence (`chan-1`, `chan-2`, …) — an `AtomicU32` like
    /// every other id this registry mints, so no `getrandom`-pulling crate
    /// is needed (CLAUDE.md constraint 2).
    pub(super) channel_seq: AtomicU32,
    /// Merge-gate hot-reload (#385/B1): groups currently sitting in "the file
    /// no longer declares `gates.merge` but a gate is still armed" — the
    /// edge-triggered latch for the `merge-gate-removal-ignored` audit line.
    /// Without this, a group left in that state would re-audit every
    /// `WORKFLOW_GATE_POLL_INTERVAL` forever (pure noise); with it, the
    /// human gets exactly one line explaining "I removed it and nothing
    /// happened", not an audit-log flood. Cleared the moment the group
    /// leaves that state (the file regains `gates.merge`, or the gate is
    /// actually cleared via the toggle), so a LATER re-entry — a fresh edit
    /// — audits again rather than staying silent forever after the first.
    pub(super) merge_gate_removal_warned: TrackedMutex<HashSet<GroupId>>,
    /// #3330: the error set each group's workflow file last failed to parse
    /// with, and whether the orchestrator has been told. See
    /// `warn_workflow_unparseable`: one row and one line per distinct error
    /// set, cleared the moment the file parses again.
    pub(super) workflow_unparseable_warned: TrackedMutex<HashMap<GroupId, (Vec<String>, bool)>>,
    /// The published snapshot the polled reads are served from (#1608, plan
    /// #1600 §3 Phase 1). NOT a `TrackedMutex` and deliberately not a lock at
    /// all from a reader's side: `views.load()` is a read-lock, a pointer
    /// clone and a release on a cell whose writer's critical section is a
    /// pointer swap. That is the whole point — a polled read must not be able
    /// to wait on anything a background thread can hold.
    ///
    /// A plain field rather than an `Arc<ViewPublisher>`: this registry is
    /// already behind an `Arc` and nothing owns the publisher independently,
    /// so an inner `Arc` would be an indirection with no second owner.
    ///
    /// `pub` rather than `pub(crate)`: `tests/liveness.rs` drives the two
    /// published reads directly while a registry lock is held, which is the
    /// one property this whole slice exists for and cannot be observed
    /// through the commands (they need an AppHandle, unavailable headless).
    pub views: views::ViewPublisher,
}

impl OrchRegistry {
    pub fn new(root: PathBuf) -> Self {
        let _ = fs::create_dir_all(&root);
        Self {
            root,
            roots: Arc::new(RootRegistry::new()),
            app: TrackedMutex::new("app", None),
            groups: TrackedMutex::new_ranked("groups", lockorder::GROUPS, HashMap::new()),
            agents: TrackedMutex::new_ranked("agents", lockorder::AGENTS, HashMap::new()),
            by_token: TrackedMutex::new("by_token", HashMap::new()),
            by_pty: TrackedMutex::new_ranked("by_pty", lockorder::BY_PTY, HashMap::new()),
            structured: structured::StructuredPanes::default(),
            stream_usage: TrackedMutex::new("stream_usage", HashMap::new()),
            pending_binds: TrackedMutex::new("pending_binds", HashMap::new()),
            test_spawn_requests: TrackedMutex::new("test_spawn_requests", HashMap::new()),
            spawn_notices: TrackedMutex::new("spawn_notices", HashMap::new()),
            test_session_learned: TrackedMutex::new("test_session_learned", Vec::new()),
            port: AtomicU16::new(0),
            // Seeded from disk on the first mint, not here — see `seq`'s doc.
            seq: AtomicU32::new(0),
            agent_seq_seeded: AtomicBool::new(false),
            agent_seq_persist: TrackedMutex::new_ranked("agent_seq_persist", lockorder::AGENT_SEQ_PERSIST, ()),
            delivery: TrackedMutex::new("delivery", HashMap::new()),
            last_delivery: Arc::new(TrackedMutex::new("last_delivery", HashMap::new())),
            delivered_notices: Arc::new(TrackedMutex::new_ranked("delivered_notices", lockorder::DELIVERED_NOTICES, HashMap::new())),
            delivered_prompts: Arc::new(TrackedMutex::new_ranked("delivered_prompts", lockorder::DELIVERED_PROMPTS, HashMap::new())),
            queues: Arc::new(queuestate::QueueMap::new_ranked(lockorder::QUEUES)),
            queue_seq: Arc::new(AtomicU64::new(0)),
            queue_persist: Arc::new(TrackedMutex::new_ranked("queue_persist", lockorder::QUEUE_PERSIST, ())),
            recovered_queue: Arc::new(TrackedMutex::new_ranked("recovered_queue", lockorder::RECOVERED_QUEUE, HashMap::new())),
            recovered_markers: Arc::new(TrackedMutex::new_ranked("recovered_markers", lockorder::RECOVERED_MARKERS, HashMap::new())),
            recovered_groups: Arc::new(TrackedMutex::new("recovered_groups", HashSet::new())),
            mq_reconciled_groups: Arc::new(TrackedMutex::new("mq_reconciled_groups", HashSet::new())),
            mq_state_lock: Arc::new(TrackedMutex::new("mq_state_lock", ())),
            mq_service_ms: Arc::new(TrackedMutex::new("mq_service_ms", HashMap::new())),
            mq_runner_override: TrackedMutex::new("mq_runner_override", None),
            rd_state_lock: Arc::new(TrackedMutex::new("rd_state_lock", ())),
            rd_service_ms: Arc::new(TrackedMutex::new("rd_service_ms", HashMap::new())),
            rd_disabled_warned: TrackedMutex::new("rd_disabled_warned", HashMap::new()),
            rd_runner_override: TrackedMutex::new("rd_runner_override", None),
            rd_signals: Arc::new(TrackedMutex::new("rd_signals", HashMap::new())),
            // One line, name literal first: `every_registry_lock_is_constructed_with_a_name`
            // classifies what FOLLOWS `TrackedMutex::new`, so a wrapped constructor reads as
            // one built with no name at all.
            rd_restart_handback: Arc::new(TrackedMutex::new("rd_restart_handback", HashMap::new())),
            rd_handback_fails: Arc::new(TrackedMutex::new("rd_handback_fails", HashMap::new())),
            rd_reconciled: Arc::new(TrackedMutex::new("rd_reconciled", HashSet::new())),
            pd_state_lock: Arc::new(TrackedMutex::new("pd_state_lock", ())),
            pd_service_ms: Arc::new(TrackedMutex::new("pd_service_ms", HashMap::new())),
            pd_signals: Arc::new(TrackedMutex::new("pd_signals", HashMap::new())),
            pd_reconciled: Arc::new(TrackedMutex::new("pd_reconciled", HashSet::new())),
            queue_draining: Arc::new(queuestate::DrainerRegistry::new()),
            drainer_gen: Arc::new(AtomicU64::new(0)),
            queue_still_notified: Arc::new(TrackedMutex::new("queue_still_notified", HashSet::new())),
            hold_episodes: Arc::new(TrackedMutex::new("hold_episodes", HashMap::new())),
            queue_pressure: Arc::new(TrackedMutex::new("queue_pressure", HashMap::new())),
            queue_depth_emitted: Arc::new(TrackedMutex::new("queue_depth_emitted", (Vec::new(), 0))),
            unconfirmed_pending: Arc::new(TrackedMutex::new("unconfirmed_pending", HashMap::new())),
            orch_notice_inbox: Arc::new(TrackedMutex::new("orch_notice_inbox", HashMap::new())),
            tasks_lock: TrackedMutex::new_ranked("tasks_lock", lockorder::TASKS, ()),
            questions_lock: TrackedMutex::new_ranked("questions_lock", lockorder::QUESTIONS, ()),
            needs_you_lock: TrackedMutex::new_ranked("needs_you_lock", lockorder::NEEDS_YOU, ()),
            mailbox_lock: TrackedMutex::new_ranked("mailbox_lock", lockorder::MAILBOX, ()),
            usage_lock: TrackedMutex::new_ranked("usage_lock", lockorder::USAGE, ()),
            triage_defer_lock: Arc::new(TrackedMutex::new_ranked("triage_defer_lock", lockorder::TRIAGE_DEFER, ())),
            usage_memo: TrackedMutex::new("usage_memo", HashMap::new()),
            series_state: TrackedMutex::new("series_state", HashMap::new()),
            series_bucket_override: TrackedMutex::new("series_bucket_override", None),
            series_revisit_override: TrackedMutex::new("series_revisit_override", None),
            poll_read_limit_override: TrackedMutex::new("poll_read_limit_override", None),
            poll_read_failed: TrackedMutex::new("poll_read_failed", HashSet::new()),
            default_branch_memo: TrackedMutex::new("default_branch_memo", HashMap::new()),
            creation: TrackedMutex::new("creation", ()),
            marker_io: TrackedMutex::new_ranked("marker_io", lockorder::MARKER_IO, ()),
            group_file_io: TrackedMutex::new_ranked("group_file_io", lockorder::GROUP_FILE_IO, ()),
            pr_head_override: TrackedMutex::new("pr_head_override", None),
            pr_body_override: TrackedMutex::new("pr_body_override", None),
            pr_files_override: TrackedMutex::new("pr_files_override", None),
            gh_exec_override: TrackedMutex::new("gh_exec_override", None),
            paused: TrackedMutex::new("paused", HashSet::new()),
            spawn_times: TrackedMutex::new("spawn_times", HashMap::new()),
            self_arc: TrackedMutex::new("self_arc", Weak::new()),
            attn_reports: TrackedMutex::new("attn_reports", HashMap::new()),
            attn_quiet: TrackedMutex::new("attn_quiet", HashMap::new()),
            attn_waiting_ack: TrackedMutex::new("attn_waiting_ack", HashSet::new()),
            attn_emitted: TrackedMutex::new("attn_emitted", HashMap::new()),
            attn_provider_limit: TrackedMutex::new("attn_provider_limit", HashMap::new()),
            attn_stranded: TrackedMutex::new("attn_stranded", HashMap::new()),
            attn_question_held: TrackedMutex::new("attn_question_held", HashSet::new()),
            notify_groups: TrackedMutex::new("notify_groups", HashSet::new()),
            autonomous_groups: TrackedMutex::new("autonomous_groups", HashSet::new()),
            auto_merge_groups: TrackedMutex::new("auto_merge_groups", HashSet::new()),
            auto_release_groups: TrackedMutex::new("auto_release_groups", HashSet::new()),
            full_autonomy_groups: TrackedMutex::new("full_autonomy_groups", HashSet::new()),
            dangerous_groups: TrackedMutex::new("dangerous_groups", HashSet::new()),
            spawn_expanded_groups: TrackedMutex::new("spawn_expanded_groups", HashSet::new()),
            idle_tick_times: TrackedMutex::new("idle_tick_times", HashMap::new()),
            compact_nudge_times: TrackedMutex::new("compact_nudge_times", HashMap::new()),
            pending_max_notice: TrackedMutex::new("pending_max_notice", HashMap::new()),
            opencode_db_degraded: TrackedMutex::new("opencode_db_degraded", HashMap::new()),
            usage_cursors: Default::default(),
            claude_projects_dir: TrackedMutex::new("claude_projects_dir", None),
            claude_agents_dir_override: TrackedMutex::new("claude_agents_dir_override", None),
            copilot_agents_dir_override: TrackedMutex::new("copilot_agents_dir_override", None),
            codex_home_override: TrackedMutex::new("codex_home_override", None),
            compact_hook_dir_override: TrackedMutex::new("compact_hook_dir_override", None),
            copilot_hooks_dir_override: TrackedMutex::new("copilot_hooks_dir_override", None),
            low_disk_notified: TrackedMutex::new("low_disk_notified", false),
            audit_skips_notified: TrackedMutex::new("audit_skips_notified", HashMap::new()),
            watches: TrackedMutex::new("watches", HashMap::new()),
            notify_seq: AtomicU32::new(0),
            intake_seen: TrackedMutex::new("intake_seen", HashMap::new()),
            intake_last_poll_ms: TrackedMutex::new("intake_last_poll_ms", HashMap::new()),
            intake_last_scan_ms: TrackedMutex::new("intake_last_scan_ms", None),
            intake_pending: TrackedMutex::new("intake_pending", HashMap::new()),
            idle_tick_last_fired_ms: TrackedMutex::new("idle_tick_last_fired_ms", HashMap::new()),
            idle_tick_empty_streak: TrackedMutex::new("idle_tick_empty_streak", HashMap::new()),
            paused_watch_since: TrackedMutex::new("paused_watch_since", HashMap::new()),
            locks: TrackedMutex::new("locks", HashMap::new()),
            paused_locks_since: TrackedMutex::new("paused_locks_since", HashMap::new()),
            channels: TrackedMutex::new("channels", HashMap::new()),
            agent_channel: TrackedMutex::new("agent_channel", HashMap::new()),
            channel_seq: AtomicU32::new(0),
            merge_gate_removal_warned: TrackedMutex::new("merge_gate_removal_warned", HashSet::new()),
            workflow_unparseable_warned: TrackedMutex::new("workflow_unparseable_warned", HashMap::new()),
            views: views::ViewPublisher::new(),
        }
    }

    /// Kill every idle worker/reviewer past its group's timeout, notifying
    /// each group's orchestrator so it can respawn on demand. Returns the
    /// killed agent ids. Called on a timer by `start_idle_reaper`.
    /// Run a human one-shot READ command under [`budget::COMMAND_READ_BUDGET`]
    /// (#1609, plan §3 Phase 2.1 item 4).
    ///
    /// On `Busy` the command degrades to `on_busy()` and breadcrumbs. The
    /// degrade is not invented here: it is the SAME empty value each of these
    /// commands already returns for an unvalidated group id, because
    /// `command_group` gives them no error channel to report anything else
    /// through. A blank panel plus a breadcrumb naming the holder beats a panel
    /// that never paints.
    ///
    /// **What this degrade does NOT do, and it is a real gap** (#1609 review
    /// N3): unlike the publisher path there is no `partial` flag and no badge,
    /// so a human sees a confidently empty board or a vanished unread chip with
    /// nothing saying it could not be read. The "same value as an unvalidated
    /// id" argument is true and is also weaker than it sounds — an unvalidated
    /// id is a programmer error, while a `Busy` is a real group with real mail.
    ///
    /// It is not closed here because these commands have no meta channel to
    /// carry the disclosure: giving them one is a wire-shape change to each of
    /// the six, which is a bigger change than this slice should make on its own
    /// initiative. The breadcrumb is what an operator has meanwhile, and
    /// `docs/design/lock-liveness.md` §3 carries the row.
    ///
    /// Takes no `self`: it is called from inside `run_blocking(move || ..)` in
    /// the module-level `#[tauri::command]` functions, which have moved `reg`
    /// into the closure and have no receiver — so the call spells
    /// `OrchRegistry::read_command(..)`.
    #[doc(hidden)]
    pub fn read_command<T>(
        name: &'static str,
        on_busy: impl FnOnce() -> T,
        f: impl FnOnce() -> T,
    ) -> T {
        match budget::read_budget(budget::COMMAND_READ_BUDGET, f) {
            Ok(v) => v,
            Err(busy) => {
                crate::obs::breadcrumb(
                    "command-busy",
                    &format!("command={name} {}", busy.detail()),
                );
                on_busy()
            }
        }
    }

    /// [`read_command`](Self::read_command)'s sibling for a command that
    /// MUTATES, and the containment barrier the GUI thread needs (#1702).
    ///
    /// **Why a synchronous command needs one at all.** Tauri dispatches a
    /// non-`async` `#[tauri::command]` by calling it inline, on the webview/GUI
    /// thread, *inside the WebView2 COM callback's own stack frame* — the chain
    /// is `body_blocking` -> `run_invoke_handler` -> `ipc/protocol.rs` -> wry ->
    /// `webview2-com-sys`'s `unsafe extern "system" fn Invoke`. Traced through
    /// the vendored sources for #1713 review B1: there is **no `catch_unwind`
    /// anywhere on that path**, and that `Invoke` thunk is a plain
    /// `extern "system"` with no `-unwind` ABI. So a panic unwinding out of a
    /// sync command does not degrade anything — Rust's abort-on-unwind shim
    /// fires in that frame and the **process aborts**.
    ///
    /// That is fatal to `refuse_reentrant`'s third case, whose whole argument
    /// is that unwinding *releases* the registry and costs one caller. On this
    /// thread it costs the app. So these commands are given a
    /// [`budget::read_budget`] frame, and the frame is the barrier: its own
    /// `catch_unwind` catches the typed unwind `budget::unwind_to_frame`
    /// throws, so the refusal is contained here and becomes `on_refused()`
    /// instead of ever reaching the COM boundary.
    ///
    /// **The `MutationScope` is what keeps R1 true through that.** A frame
    /// alone would make a budget TIMEOUT unwind these mid-mutation, which is
    /// exactly the corruption rider R1 exists to prevent. Inside the scope a
    /// timeout waits, unbounded, as it always did; only the re-entrant refusal
    /// unwinds, because it is the one wait that never ends
    /// (`docs/design/lock-liveness.md` §4.1). So this changes what a *defect*
    /// does and changes nothing about what contention does.
    ///
    /// It does NOT make a genuine panic in one of these commands survivable —
    /// that still reaches the boundary and still aborts, exactly as it did
    /// before #1702 — the pre-existing hazard, tracked on #1717. What this
    /// contains is the one unwind this epic introduced.
    #[doc(hidden)]
    pub fn mutating_command<T>(
        name: &'static str,
        on_refused: impl FnOnce() -> T,
        f: impl FnOnce() -> T,
    ) -> T {
        let _scope = budget::MutationScope::enter();
        match budget::read_budget(budget::COMMAND_READ_BUDGET, f) {
            Ok(v) => v,
            Err(busy) => {
                crate::obs::breadcrumb(
                    "command-refused",
                    &format!("command={name} {}", busy.detail()),
                );
                on_refused()
            }
        }
    }
}
