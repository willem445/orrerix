//! The background loops started at launch: the idle reaper, watchdog, gh
//! poller, idle tick, compact nudge, workflow-gate reload, disk monitor,
//! max-notice flusher, view publisher and attention.
//! Design note: `docs/design/orchestration.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): `OrchRegistry`,
//! `crate::obs`. IO: threads/sleep. Sibling files it calls: `guardrails.rs`.

use super::*;
/// How often the idle reaper wakes to look for workers to auto-kill.
const IDLE_REAP_INTERVAL: Duration = Duration::from_secs(30);
/// How often the watchdog wakes to look for stalled working agents.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(30);
/// How often the low-disk backstop samples free space on the workspace drive
/// (#134). Slow on purpose — disk pressure builds over minutes of cargo builds,
/// not seconds — so the sysinfo scan stays negligible.
const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Autonomous mode (#83): how often the idle-tick loop wakes to check whether an
/// autonomous group's orchestrator has gone output-quiet long enough to warrant a
/// tick, and to enforce autonomy budgets. Coarser than the watchdog because the
/// gate is measured in minutes, not seconds — a 60s wake is cheap and precise
/// enough. See `start_idle_tick` / `run_idle_tick`.
const IDLE_TICK_INTERVAL: Duration = Duration::from_secs(60);
/// Round 10 (#428 follow-up, user-directed): the compact-nudge loop's poll
/// cadence WHILE any agent has a compact arm open — evidence (marker files,
/// the Copilot completion-paint detector) only gets consumed on this
/// thread's own wake, so riding the full `IDLE_TICK_INTERVAL` (60s) while an
/// outcome is already effectively decided (hook-confirmed, just waiting on
/// the poll) is exactly the residual UX gap a live user re-test caught: the
/// badge sits in "awaiting evidence" limbo for up to a minute per poll even
/// though nothing is actually undecided anymore. Idle cost is zero — this
/// is only ever consulted while `any_compact_pending()` is true, which is
/// false for the overwhelming majority of a session's wall-clock time (no
/// compaction in flight). See `compact_nudge_poll_interval`.
const COMPACT_NUDGE_FAST_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Idle-tick intake gate (#332): how often the intake half of the unified
/// `gh` poller scans for a group whose `intake_poll_minutes` interval has
/// elapsed. Coarser polling than this would risk missing a group's own
/// interval by a noticeable margin; finer buys nothing, since
/// `intake::due_intake_polls` still gates the actual `gh` calls to each
/// group's configured minutes.
///
/// #406: this is no longer a thread's sleep — it is the scan floor
/// `intake_scan_due` applies inside the single poll loop, whose own wake
/// cadence is the finer `notify::NOTIFY_POLL_INTERVAL`. Same cadence, one
/// fewer thread.
const INTAKE_POLL_SCAN_INTERVAL: Duration = Duration::from_secs(60);
/// Merge-gate hot-reload (#385): how often `run_workflow_gate_reload` re-checks
/// every advanced-orchestrator group's `.loomux/workflow.yml` against its
/// currently-armed gate. A human edit reading the workflow file wants to feel
/// "took effect", not "cache-invalidated eventually" — but the gate is
/// security-enforcing, not UI-cosmetic, so this errs toward the same 30s
/// cadence as the idle reaper/watchdog rather than the 3s attention-router
/// tick: prompt enough that nobody waits a minute wondering if their edit
/// landed, coarse enough that a small per-group YAML read+parse is negligible
/// even with several live groups.
const WORKFLOW_GATE_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// How often the attention scan recomputes which panes need the human
/// (idle-with-prompt detection; report/gate signals are event-driven and
/// picked up on the next tick).
const ATTENTION_INTERVAL: Duration = Duration::from_secs(3);
/// How many attention ticks apart the stuck-prompt badge janitor runs (#825
/// M2) — every 10th, so ~30s. It rides this loop rather than owning a thread
/// because it is the same "does this pane still need the human" question the
/// loop already exists to ask, just answered from the pane instead of from the
/// registry; a second thread would be a second cadence to reason about for no
/// gain. Divided rather than per-tick because the answer only changes when a
/// human touches the pane: at 3s the pass would take ten times the pane reads
/// to notice the same thing ten times later at worst, which is the wrong
/// trade for work that reads a pty ring. See `start_attention` for the cost
/// bound this cadence is half of.
///
/// (#825 M3) It paces the `QueueFull` re-admission pass on the same tick. That
/// one is strictly cheaper — two map reads per badged pane and no pty read at
/// all — so it is here to share one cadence to reason about rather than because
/// it needs slowing down.
const STRANDED_JANITOR_EVERY_N_TICKS: u64 = 10;

/// The sleep a supervised loop falls back to when it could not compute its own
/// next interval, because the closure that computes it panicked (#1702).
///
/// One minute: long enough that a loop whose interval read is broken cannot
/// spin the CPU, short enough that it keeps trying often enough to recover if
/// whatever broke clears. Only two loops compute an interval at all
/// (`compact_nudge_poll_interval`); for the rest this is unreachable, because
/// returning a constant cannot panic.
const TICK_FALLBACK_INTERVAL: Duration = Duration::from_secs(60);

/// Spawn a cadenced background loop whose body is SUPERVISED — a panic inside
/// it ends that tick and is breadcrumbed, rather than ending the thread
/// (#1702, `obs::TickSupervisor`).
///
/// **The supervision lives here rather than at each call site on purpose.** Ten
/// loops that each had to remember a `catch_unwind` is exactly the "did we
/// remember to do this one" review dependency #1600 §2 is about, and the
/// eleventh loop written next month is the one that would forget. A loop
/// written through this helper cannot: there is no `std::thread::spawn` in its
/// call site to leave unwrapped.
///
/// `interval` is a closure rather than a `Duration` because two of these loops
/// choose their next sleep from registry state
/// ([`compact_nudge_poll_interval`]), which reads locks and can therefore panic
/// exactly as a body can. It runs under the same supervisor, and a panicked
/// interval falls back to [`TICK_FALLBACK_INTERVAL`] so the thread cannot spin.
///
/// `start_attention` and `start_view_publisher` are deliberately NOT written
/// through this: the first runs three independent bodies on one thread and
/// wants three supervisors, so one broken pass cannot latch the other two off;
/// the second is one body but with a name of its own. Both use
/// `obs::TickSupervisor` directly. Eight loops here plus those two is the
/// whole of this file's cadenced set.
///
/// **One cadenced loop in this app is still unsupervised, and it is not in this
/// file**: `gitwatch::start` (`src-tauri/src/gitwatch.rs`) is the only
/// remaining bare `thread::spawn(move || loop { .. })` in the tree, and its
/// body reaches `lock_safe` through `poll_changed`. It is not a re-entrancy
/// site — `poll_changed` is snapshot, then stat, then re-acquire — so #1702
/// left it alone rather than widening past the plan's scope (§2 item 3a names
/// `mod.rs`'s `start_*` loops and `start_view_publisher`). Named here so "the
/// ticks are supervised" is read as a claim about this file, which is what it
/// is, rather than about the app (#1713 review N3).
fn spawn_tick_loop(
    tick: &'static str,
    mut interval: impl FnMut() -> Duration + Send + 'static,
    mut body: impl FnMut() + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut sup = crate::obs::TickSupervisor::new(tick);
        loop {
            let wait = sup.run_for(&mut interval).unwrap_or(TICK_FALLBACK_INTERVAL);
            std::thread::sleep(wait);
            sup.run(&mut body);
        }
    });
}

/// Background loop that enforces the idle-worker auto-kill guardrail: every
/// `IDLE_REAP_INTERVAL` it kills each worker/reviewer whose idle time has
/// crossed its group's `idle_kill_minutes` (groups with the guardrail off
/// are skipped inside `reap_idle_agents`, and so is a **liaison** block —
/// `idle_reap_candidates` owns both exclusions). Started once at app setup.
pub fn start_idle_reaper(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "idle-reaper",
        || IDLE_REAP_INTERVAL,
        move || {
            reg.reap_idle_agents(now_ms());
        },
    );
}

/// Background loop for the stalled-agent watchdog: every `WATCHDOG_INTERVAL`
/// it nudges the orchestrator (once per stall) about any working agent that
/// has gone silent — no terminal output, no report — past its group's
/// `watchdog_stall_minutes`. Groups with the guardrail off and paused groups
/// are skipped inside `run_watchdog`. Started once at app setup.
pub fn start_watchdog(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "watchdog",
        || WATCHDOG_INTERVAL,
        move || {
            reg.run_watchdog(now_ms());
        },
    );
}

/// What one wake of the unified `gh` poller did (#406) — returned so a test
/// can pin that a single tick services BOTH features, and which halves ran,
/// without a thread or a `gh` subprocess.
// `Default` is what a SKIPPED tick returns (#1609): the same "nothing
// happened" value a poll with no watches produces, so a skip needs no second
// code path in any caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GhPollTick {
    /// Watch ids the notify half resolved this tick (fired, expired, or
    /// cancelled) — `notify_tick`'s return, unchanged.
    pub fired: Vec<String>,
    /// Whether the intake half's due-group scan ran on this wake.
    pub intake_scanned: bool,
    /// The group the merge-queue driver serviced on this wake, if any (#698).
    ///
    /// The group rather than a bool, because "one group per wake, oldest
    /// serviced first" is the tick's whole bound and a bool could not tell a
    /// rotation from a group monopolising every wake.
    pub mq_serviced: Option<GroupId>,
    /// The group the review driver serviced on this wake, if any (#1778).
    ///
    /// Its own field rather than a shared one, for the same reason `mq_serviced`
    /// is a group and not a bool: the two loops rotate independently, and a
    /// single field could not tell a tick that drove one from a tick that drove
    /// both.
    pub rd_serviced: Option<GroupId>,
}

/// Whether the unified poller's INTAKE half is due on a wake at `now`, given
/// when its scan last ran (`None` = never, i.e. the first wake after launch).
/// Pure so the shared cadence is directly testable without spinning up the
/// thread — the `compact_nudge_poll_interval` idiom.
///
/// This is a scan-cadence floor, not the `gh` budget: `intake::
/// due_intake_polls` still owns the per-group interval that decides whether
/// a scan makes any `gh` call at all. Keeping the floor here means merging
/// the two loops does not silently move the intake scan onto the notify
/// cadence (twice as often), which is the one behavior change unification
/// would otherwise smuggle in.
///
/// A stamp in the FUTURE means the system clock moved BACKWARDS since the
/// last scan (an NTP correction, a VM resume, a manual set), and it rescans
/// rather than waiting for wall-clock to catch up — a plain elapsed check
/// stalls the intake scan for the entire size of the jump, which the
/// `sleep`-driven thread this replaced could not do (rev-157, non-blocking
/// 2). Rescanning re-stamps, so one jump costs one early scan, not a loop:
/// the per-group `gh` floor (`intake::due_intake_polls`) is what actually
/// bounds the API cost of an early scan, and it is unchanged.
pub fn intake_scan_due(now: u64, last_scan_ms: Option<u64>) -> bool {
    match last_scan_ms {
        None => true,
        Some(last) if now < last => true,
        Some(last) => now - last >= INTAKE_POLL_SCAN_INTERVAL.as_millis() as u64,
    }
}

/// The single background loop that makes `gh` calls in this process (#406).
/// Every `notify::NOTIFY_POLL_INTERVAL` it runs one `run_gh_poll_tick`, which
/// services both `gh`-polling features against one sampled instant:
///
/// - the notification backend (#243) on EVERY wake — it polls due watches
///   (`gh pr checks` / `gh run view`, backend-owned argv only, see
///   `gh_capture`) and delivers an `[orrerix] …` notice into the registering
///   agent's own pane the moment its condition is met, its TTL expires, or it
///   fails `notify::NOTIFY_FAIL_STREAK_LIMIT` polls running;
/// - the idle-tick intake gate (#332) on the wakes where `intake_scan_due`
///   says its coarser `INTAKE_POLL_SCAN_INTERVAL` scan cadence has elapsed —
///   it polls any autonomous group whose `intake_poll_minutes` guardrail is
///   due (`gh issue list` / `gh pr list`) and folds new label/PR-check signal
///   into that group's pending wake summary for `idle_tick_tick`.
///
/// Why one loop: both features spend the same account's GitHub API budget,
/// and two independently-clocked threads share no accounting of it — the
/// latent coupling #406 closes before the intake gate is load-bearing. The
/// PURE decision layers stay separate and separately tested
/// (`notify::due_watches`, `intake::due_intake_polls`); only the thread and
/// its clock are shared. Started once at app setup, beside `start_watchdog`.
pub fn start_gh_poller(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "gh-poller",
        || notify::NOTIFY_POLL_INTERVAL,
        move || {
            reg.run_gh_poll_tick();
        },
    );
}

/// Background loop for autonomous mode (#83): every `IDLE_TICK_INTERVAL` it
/// enforces autonomy token budgets (suspending a group that has overspent) and
/// then delivers one `[orrerix] idle tick` to any autonomous group's orchestrator
/// that has been output-quiet past `IDLE_TICK_MINUTES`, so the template's
/// idle-cadence intake/monitoring actually runs unattended. Non-autonomous and
/// paused groups are skipped inside `run_idle_tick`; the tick is self-regulating
/// (any orchestrator action resets the quiet clock) and hard-capped per hour.
/// Started once at app setup.
pub fn start_idle_tick(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "idle-tick",
        || IDLE_TICK_INTERVAL,
        move || {
            reg.run_idle_tick(now_ms());
        },
    );
}

/// One running orchestrator as `OrchRegistry::cache_idle_gather` saw it (#3407).
pub(in crate::orchestration) struct CacheIdleCandidate {
    pub(in crate::orchestration) id: String,
    pub(in crate::orchestration) group: GroupId,
    pub(in crate::orchestration) block: String,
    /// The key this agent's usage row is stored under
    /// (`OrchRegistry::usage_key`), so the decide phase can read the cache
    /// lifetime the usage tick detected for it (#3831).
    pub(in crate::orchestration) usage_key: String,
    pub(in crate::orchestration) pty_id: Option<u32>,
    pub(in crate::orchestration) idle_ms: u64,
    pub(in crate::orchestration) latched: bool,
    pub(in crate::orchestration) compact_busy: bool,
}

/// Everything `OrchRegistry::cache_idle_decide` reads that lives behind a
/// registry lock, taken in one gather pass (#3407).
pub(in crate::orchestration) struct CacheIdleSnapshot {
    pub(in crate::orchestration) groups: HashMap<GroupId, Guardrails>,
    pub(in crate::orchestration) watch_groups: HashSet<GroupId>,
    pub(in crate::orchestration) intake_groups: HashSet<GroupId>,
    pub(in crate::orchestration) delegate_groups: HashSet<GroupId>,
    pub(in crate::orchestration) candidates: Vec<CacheIdleCandidate>,
}

/// What `OrchRegistry::cache_idle_decide` decided: latches to release, and
/// fires carrying the context percent and TTL the notice names (#3407).
#[derive(Default)]
pub(in crate::orchestration) struct CacheIdlePlan {
    pub(in crate::orchestration) release: Vec<String>,
    pub(in crate::orchestration) fire: Vec<(CacheIdleCandidate, u32, u32)>,
}

/// Round 10 (#428 follow-up): which cadence the compact-nudge loop's NEXT
/// sleep should use — pure so the selection is directly testable without
/// spinning up the thread/registry. `any_pending` is the ONLY input: no
/// hysteresis, no latch, no memory of a PREVIOUS tick's state — an arm that
/// opens and closes within one wake is simply seen or not seen on the wake
/// that actually happens to land, exactly like every other observation this
/// tick makes. That simplicity is deliberate: a debounced/latched cadence
/// would need its own state to get wrong, for a problem ("thrash") that
/// doesn't otherwise exist here — recomputing fresh every iteration already
/// can't oscillate faster than the loop itself runs.
pub fn compact_nudge_poll_interval(any_pending: bool) -> Duration {
    if any_pending { COMPACT_NUDGE_FAST_POLL_INTERVAL } else { IDLE_TICK_INTERVAL }
}

/// Background loop for compact-nudge (#287): normally wakes every
/// `IDLE_TICK_INTERVAL` to paste `/compact` for any eligible pane (default:
/// the orchestrator, on Claude Code) that has gone output-quiet past its
/// group's `compact_nudge_minutes` — the same idleness signal `idle_tick_
/// tick` reads, not a second one. Off by default (`compact_nudge_minutes ==
/// 0`); non-Claude CLIs and paused groups are skipped inside `run_compact_
/// nudge` / `compact_nudge_tick`.
///
/// Round 10: the sleep is now COMPUTED each iteration
/// (`compact_nudge_poll_interval`) rather than the fixed constant — while
/// `any_compact_pending()` is true anywhere in the registry, this thread
/// wakes on `COMPACT_NUDGE_FAST_POLL_INTERVAL` (10s) instead, so hook/marker
/// evidence gets consumed within seconds on both CLIs rather than riding out
/// the full 60s idle cadence. The interval is read BEFORE the sleep, from
/// whatever the previous tick left the registry in (or the empty state at
/// first launch) — so an arm opening mid-sleep is caught on the very next
/// wake, and the loop drops back to the normal cadence the wake after the
/// last open arm resolves. Started once at app setup.
pub fn start_compact_nudge(reg: Arc<OrchRegistry>) {
    let cadence = reg.clone();
    spawn_tick_loop(
        "compact-nudge",
        move || compact_nudge_poll_interval(cadence.any_compact_pending()),
        move || {
            reg.run_compact_nudge(now_ms());
        },
    );
}

/// Background loop for the merge-gate hot-reload (#385): every
/// `WORKFLOW_GATE_POLL_INTERVAL` it re-derives every advanced-orchestrator
/// group's merge gate from the repo's CURRENT `.loomux/workflow.yml`, so an
/// in-place edit to `gates.merge` takes effect without a relaunch or a manual
/// toggle off/on. See `OrchRegistry::reload_merge_gate_if_changed` for the
/// fail-closed contract this loop enforces. Started once at app setup, beside
/// `start_gh_poller`. (This one reads a file, not `gh` — it is deliberately
/// NOT folded into the unified poller of #406, whose subject is the shared
/// GitHub API budget.)
pub fn start_workflow_gate_reload(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "workflow-gate-reload",
        || WORKFLOW_GATE_POLL_INTERVAL,
        move || {
            reg.run_workflow_gate_reload();
        },
    );
}

/// Free bytes on the disk that hosts `path`: the mounted volume whose mount
/// point is the longest prefix of `path`. `None` if no volume matches (or the
/// listing is empty), so the caller no-ops rather than guessing.
pub(in crate::orchestration) fn free_disk_bytes(path: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

/// Background loop for the low-disk backstop (#134): every `DISK_CHECK_INTERVAL`
/// it samples free space on the workspace drive and, on crossing below the
/// threshold, sends one latched notice per group orchestrator. Started once at
/// app setup. Slow cadence keeps the sysinfo scan negligible.
pub fn start_disk_monitor(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "disk-monitor",
        || DISK_CHECK_INTERVAL,
        move || {
            reg.run_disk_monitor();
        },
    );
}

/// Background loop for the debounced cap-change notice (#79): every
/// `MAX_NOTICE_FLUSH_INTERVAL` it delivers any coalesced max-agents notice
/// whose quiet window has elapsed, so a burst of stepper clicks reaches the
/// orchestrator as one re-plan prompt instead of one per click. Started once
/// at app setup.
pub fn start_max_notice_flusher(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "max-notice-flusher",
        || MAX_NOTICE_FLUSH_INTERVAL,
        move || {
            reg.flush_due_max_notices(now_ms());
        },
    );
}

/// Background loop for the polled-view publisher (#1608, plan #1600 §3 Phase
/// 1): every [`views::VIEW_PUBLISH_INTERVAL`] it recomputes the group-view and
/// tab-strip payloads for every group the registry knows and publishes them as
/// one immutable snapshot, so the two polled commands can be served by pointer
/// clone instead of by acquiring registry mutexes on every tick.
///
/// **This is the thread that pays the wait.** If a registry lock is held
/// pathologically long, exactly one thread parks here — not one per poller per
/// tick on the shared blocking pool, which is the accumulation #1600 §1.2
/// derives beta6 from. Readers keep answering with the last snapshot and a
/// growing `age_ms`, and the frontend badges it stale past
/// [`views::VIEW_STALE_AFTER_MS`]. Started once at app setup.
///
/// **The pass is supervised** (#1702, `obs::TickSupervisor`). It was not, and
/// `docs/design/polled-views.md` disclosed that: a panic inside one group's
/// `compute_group` ended this thread permanently and froze the snapshot for
/// BOTH polled surfaces at once, with only the stale badge between a dead
/// publisher and a plausible-looking frozen UI. A panic now costs one pass and
/// a `tick-panicked` breadcrumb; the badge still covers the interval, which is
/// what makes the degrade the same one the rest of that design already has.
pub fn start_view_publisher(reg: Arc<OrchRegistry>) {
    std::thread::spawn(move || {
        let mut sup = crate::obs::TickSupervisor::new("view-publisher");
        loop {
            std::thread::sleep(views::VIEW_PUBLISH_INTERVAL);
            sup.run(|| reg.views.publish_pass(&reg));
        }
    });
}

/// Background loop for attention routing (#6): every `ATTENTION_INTERVAL` it
/// recomputes which panes need the human (idle-with-prompt, worker reports,
/// human merge gates), pushes the set to the frontend for pane badges, and
/// toasts newly-attention panes in notification-enabled groups. Started once at
/// app setup.
/// (#825 M2) Every `STRANDED_JANITOR_EVERY_N_TICKS`th pass also runs the
/// stuck-prompt badge janitor, so a raised chip keeps being checked against
/// the pane after `run_late_confirmation_monitor` — which has done exactly
/// that check every 5s since #496 PR-C — exits at
/// `LATE_MONITOR_MAX_LIFETIME`. It runs FIRST so a chip this pass takes down
/// is already gone from the item set `run_attention` emits in the same tick,
/// rather than being shown one more time and disappearing 3s later.
///
/// **Cost, declared (performance.md §3 INV-4: cadenced work says what it
/// costs).** Fixed-cadence work, so it owes a bound. Four things bound it:
///
/// 1. **Scope.** Zero pty reads unless a stuck-prompt chip is actually up, in
///    one of the classes a pane reading can answer, on a Running agent with a
///    pty. `attn_stranded` is empty on every healthy session, and the pass
///    returns on that check having taken one uncontended map lock and nothing
///    else — no agents lock, no ledger, no ring. The badged case is rare and
///    self-limiting: this very reading is what takes the chip down, so the
///    work ends the condition that schedules it (#819 F3's argument, and
///    `drain_stranded_submit`'s).
/// 2. **Per badged pane.** One ledger lock + clone, one `Tier1Scan::for_paste`
///    normalize of the recorded text, then at most `TIER1_SCAN_WIDEN_ROUNDS`
///    ring reads capped at `TIER1_SCAN_WIDEN_MAX_BYTES` — `Tier1Scan::widen`'s
///    own bound, not a fresh one — and one `last_user_input_ms` stamp read. No
///    IPC, no fs, no lock beyond the ring's own, and nothing held across a pty
///    read.
/// 3. **Cadence.** 30s, against the 5s the late monitor already spends on the
///    identical read on the same panes: one sixth the rate, for the far
///    smaller set of panes that still carry a chip. Where the two overlap (a
///    chip up while its monitor is alive) the extra cost is one such read per
///    30s, and the verdict is identical by construction — both ask
///    `stranded_badge_release`.
/// 4. **Worst case.** Bounded by the number of badged panes, which is bounded
///    by the fleet size; a fleet where every pane carries a stuck-prompt chip
///    has a much louder problem than this loop.
///
/// (#825 M3) The same tick then runs the `QueueFull` re-admission, which is the
/// one unreleased class whose answer is a retry rather than a reading — see
/// [`queuefull_readmit_gate`] for why it observes the pane *after* a drain
/// instead of hooking the drain edge itself. It rides this cadence rather than
/// a second one because it asks the same question at the same moment, and it is
/// **cheaper than the janitor by a wide margin**: no pty read, no ledger clone,
/// no ring scan. Its bound, same INV-4 discipline:
///
/// 1. **Scope.** Returns after one uncontended `attn_stranded` lock unless a
///    chip is up somewhere — the healthy-session case, identical to the
///    janitor's. Self-limiting in the same sense: a successful re-admission
///    re-words the chip off `QueueFull`, so the work ends the condition that
///    schedules it, and a pane can only be re-admitted for as long as its own
///    badge says the re-send was refused.
/// 2. **Per badged pane.** Two uncontended map reads (`queue_depth`,
///    `drainer_active`) before anything else, and for the pane that passes them
///    one `admit_stranded_selfheal` — a single `queues` critical section and one
///    ledger read. No pty read at all, at any point: nothing here looks at a
///    pane, which is the whole difference between a retry and a reading.
/// 3. **Cadence and worst case.** As above — 30s, bounded by the badged-pane
///    count.
///
/// Both passes run BEFORE `run_attention` so a chip either of them changed is
/// emitted this tick with its new wording, rather than being shown stale once
/// more and corrected 3s later.
///
/// (#814) `run_attention` itself then pushes the delivery-queue badge set
/// (`orch-queue-depth`), which rides this cadence for the reason the badge exists:
/// the age it shows has to keep growing on screen, and the frontend deliberately
/// has no clock of its own. Its bound, same discipline:
///
/// 1. **Scope.** One uncontended `queues` read per tick, and a return with no
///    emit at all when no pane has anything queued — the ordinary state.
/// 2. **Per pane with a queue.** A count and a minimum over at most
///    `queue::QUEUE_MAX_PER_PANE` (8) stamps, taken under the queue lock and
///    nothing cloned out of it, then one `hold_episodes` read. No pty read.
/// 3. **Emit.** At most one event per tick, and only when the coarsened reading
///    actually differs from the last one pushed
///    (`OrchRegistry::queue_depth_push`, `queue::coarsen_waiting_ms`) — so a pane
///    stuck for an hour costs about one emit a minute, not one every 3s — plus
///    one unconditional re-push of a non-empty set every `QUEUE_DEPTH_REPUSH_MS`,
///    which is that suppression's independent release rather than a cadence of
///    its own.
///
/// **Three supervisors on one thread** (#1702). This loop runs three
/// independent bodies, and they are supervised separately rather than as one
/// pass: a panic in the janitor is not a reason to stop emitting attention
/// badges, and `obs::TickSupervisor`'s consecutive-panic latch would otherwise
/// take all three down for one broken one. `spawn_tick_loop` cannot express
/// that, which is why this loop is still written out.
pub fn start_attention(reg: Arc<OrchRegistry>) {
    std::thread::spawn(move || {
        let mut tick: u64 = 0;
        let mut sup_janitor = crate::obs::TickSupervisor::new("attention-stranded-janitor");
        let mut sup_readmit = crate::obs::TickSupervisor::new("attention-queuefull-readmit");
        let mut sup_attention = crate::obs::TickSupervisor::new("attention");
        loop {
            std::thread::sleep(ATTENTION_INTERVAL);
            tick = tick.wrapping_add(1);
            if tick % STRANDED_JANITOR_EVERY_N_TICKS == 0 {
                sup_janitor.run(|| reg.run_stranded_janitor());
                sup_readmit.run(|| reg.run_stranded_queuefull_readmit());
            }
            sup_attention.run(|| reg.run_attention(now_ms()));
        }
    });
}

/// #385/B1: the stability check `read_workflow_stably` layers on top of a
/// plain read — pure numeric logic, no filesystem, so it's unit-tested here
/// rather than by trying to race a real write in an integration test (which
/// would be exactly the kind of flaky, timing-dependent test this fix was
/// designed to avoid depending on).
#[cfg(test)]
mod workflow_gate_reload_read_tests {
    use super::*;

    #[test]
    fn agreeing_stats_and_read_are_stable() {
        assert!(OrchRegistry::workflow_read_is_stable(120, 120, 120));
    }

    #[test]
    fn a_shrink_between_the_before_stat_and_the_read_is_unstable() {
        // The file was still being truncated when we read it: the pre-read
        // stat saw the old (larger) size, the read itself caught the new one.
        assert!(!OrchRegistry::workflow_read_is_stable(200, 120, 120));
    }

    #[test]
    fn growth_between_the_read_and_the_after_stat_is_unstable() {
        // The writer kept appending right after our read finished — we may
        // have caught an early, incomplete slice of a multi-write save.
        assert!(!OrchRegistry::workflow_read_is_stable(120, 120, 260));
    }

    #[test]
    fn a_fully_torn_read_disagreeing_on_all_three_is_unstable() {
        assert!(!OrchRegistry::workflow_read_is_stable(50, 120, 260));
    }
}
