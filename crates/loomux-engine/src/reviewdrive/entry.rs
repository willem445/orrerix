//! One driven PR (§5.2): [`DriveEntry`], the notice it may owe
//! ([`OwedNotice`]), and every method on an entry — [`DriveEntry::advance`] being the
//! only sanctioned way its state changes.
//!
//! Part of the review driver's pure core, split out of the former single-file
//! `reviewdrive.rs` by #3498 P7 as a pure move. The module map is in `mod.rs`;
//! the design note is `docs/design/review-driver.md`.

use super::*;

/// A notice a terminal entry owes the orchestrator's pane, persisted **on the
/// entry** so it outlives the tick that produced it (#1857).
///
/// **Why the rendered text and not a flag plus a re-render.** A terminal entry
/// is the one thing the tick will not step: `rd_step_entry` declines anything
/// parked or terminal before it reads anything, and that early return is a cost
/// bound §2.4 wants rather than an oversight. So the facts a re-render would
/// need — the lane verdicts, the live head, the gate's answer — are not in hand
/// on any later tick, and fetching them again would spend `gh` round trips on a
/// drive that is over, to produce a notice that could legitimately differ from
/// the one the drive actually ended on. The notice is the product of the arc
/// that ended the drive: it is written down at that moment and re-sent
/// unchanged.
///
/// **Only a TERMINAL entry ever owes one, and that is a scope rather than an
/// oversight.** A `held` drive that loses its notice loses a *line*; the entry
/// itself survives — §5.2 never prunes a parked one — and `review_drive_status()`
/// lists it, so the drive has not vanished and the orchestrator has a record it
/// can still act on. A terminal entry has neither: the notice is the entire
/// product of the exit, and retention drops the record that could reproduce it.
/// A future change wanting the same guarantee for a hold has this mechanism to
/// use; it does not get it by accident here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwedNotice {
    /// The rendered notice, exactly as the arc that ended the drive produced it.
    pub text: String,
    /// When it was first owed — the ceiling's anchor, and **absolute** for
    /// [`DriveEntry::started_ms`]'s reason: a stored elapsed time is stale the
    /// instant it is written and meaningless across a restart.
    pub owed_ms: u64,
    /// How many delivery attempts have failed. An audit detail, never the
    /// bound: the tick's cadence is the shared poll loop's, so an attempt count
    /// would be a different real duration on every fleet.
    #[serde(default)]
    pub failures: u32,
}

/// One driven PR (§5.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DriveEntry {
    pub pr: u64,
    /// Private on purpose: [`advance`](DriveEntry::advance) is the only
    /// sanctioned way an entry's state changes, so the enumeration in
    /// [`transition`] cannot be bypassed by a caller outside this module
    /// assigning a state directly. Deserialization is the one other writer, and
    /// that is resuming a persisted state rather than transitioning to one.
    state: DriveState,
    /// Set exactly when `state` is [`DriveState::Held`] — maintained by
    /// [`advance`](DriveEntry::advance), which clears it on every other arc so
    /// a resumed drive cannot carry a stale reason into `review_drive_status()`.
    #[serde(default)]
    pub held_reason: Option<HeldReason>,
    /// The PR head this entry was last resolved against. A record of what was
    /// seen; every decision re-reads the live head (§2.4 resumes "against the
    /// **live** head, never against the head the file remembers").
    ///
    /// **S3 must persist `entry.head = facts.head` whenever a tick resolves the
    /// head, and this obligation is written here rather than left to be
    /// inferred, because nothing in this module can enforce it and every
    /// individual call stays correct while it is violated.** [`decide`] reads
    /// this field only to compare it against the live head — arc 6 in
    /// `review-wait`, arc 7 in `fix-wait` — so a tick that records the head
    /// once at `drive_review` time and never again makes that comparison
    /// permanently true: the drive takes arc 6 to `ci-wait`, goes green, comes
    /// back to `review-wait`, and takes arc 6 again, forever. The failure is
    /// *emergent at the seam*, which is why no test in this crate catches it.
    ///
    /// **The empty-*live*-head case is handled in [`decide`] itself, and the
    /// earlier version of this paragraph got it wrong in the dangerous
    /// direction — it claimed arc 6's `!facts.head.is_empty()` guard meant a
    /// failed head read "does not thrash the state machine".** That guard only
    /// stops arc 6 from *firing*; it does not stop the tick continuing past it
    /// into `first_stale_lane` (where `ReviewVerdict::reviewed("")` is false for
    /// every real verdict head) and [`lane_open_for`] (which refuses every
    /// record briefed at a real head), which together return `OpenLane{k}` on
    /// every tick — a reviewer spawned per tick, with each brief re-arming that
    /// lane's `spawned_ms` so `lane-stalled` can never fire. [`decide`] now
    /// returns `Wait` when the live head is empty, before any state is
    /// dispatched, and the drive stays bounded by `drive-stalled`.
    ///
    /// A *stored* head that is empty while the live head reads fine is the
    /// ordinary first-tick state of a fresh entry and is not a defect: arc 6
    /// then moves the drive to `ci-wait`, which is where the head is recorded.
    #[serde(default)]
    pub head: String,
    /// The PR body digest last seen, the same #565 digest the gate reads.
    #[serde(default)]
    pub body_digest: String,
    /// The worker session, **as resolved** by `resolve_session_ref` at
    /// `drive_review` time and never the caller's raw string (§3.2).
    #[serde(default)]
    pub worker_session: String,
    /// The **agent id** the worker's resumed session is running in, as of the
    /// last hand-back. [`LaneRecord::agent`]'s twin, for §7's reason:
    /// interception is keyed on the agent, and a `report` arrives carrying one.
    ///
    /// Empty until the drive has handed back at least once, which is correct
    /// rather than incidental: before the first hand-back there is no resumed
    /// worker pane for this drive to own, and a drive must never consume the
    /// traffic of a worker it did not resume.
    #[serde(default)]
    pub worker_agent: String,
    /// Every EARLIER pane this drive resumed the worker into, oldest first —
    /// [`LaneRecord::prior_agents`]'s twin, and **the field #1871 B2 was**.
    ///
    /// `worker_agent` was one slot, so the second hand-back evicted the first
    /// pane's id from the only key §7 has. Measured on the dogfood run: the
    /// drive resumed the worker into `w-1715`, whose `report(progress)` was
    /// consumed correctly; it then handed back again into `w-1716`, and
    /// `w-1715` — still running, still on the same session and the same PR —
    /// had both of its `report(done)` calls delivered to the orchestrator as if
    /// nobody owned it. Every pane in that list is the SAME worker session; a
    /// drive that owns the worker owns each pane it resumed that worker into.
    ///
    /// Cleared with `worker_agent` when a resume re-points the drive at a
    /// DIFFERENT session, for that field's reason: those panes are a worker
    /// this drive no longer owns.
    #[serde(default)]
    pub prior_worker_agents: Vec<String>,
    /// **The panes this drive was STARTED ON** — every live pane sitting on
    /// `worker_session` at the moment `drive_review` recorded it (#3250).
    ///
    /// The other two lists are written by a HAND-BACK, so a drive that never
    /// takes one owns no worker pane at all: the pane the orchestrator built
    /// the drive around is invisible to every rule keyed on them. That is the
    /// defect #3250 measured — on PRs #3243 and #3248 the audit runs
    /// `rd-started` -> `rd-satisfied` with no `rd-handback` row, and the
    /// original worker pane was still alive and idle at the exit, holding the
    /// worktree an orchestrator then had to `kill_agent` by hand.
    ///
    /// **Read by the terminal release and by nothing else.** In particular NOT
    /// by [`driven_role`](DriveEntry::driven_role): §7's interception is a
    /// claim on a pane's TRAFFIC, and "a drive must never consume the traffic
    /// of a worker it did not resume" is exactly as true of a founding pane as
    /// it was before this field existed. Ending a pane the drive was handed,
    /// once the drive is over, is a different question from speaking for it
    /// while it runs.
    ///
    /// Recorded at drive start and at a resume, cleared with the other two when
    /// a resume re-points the drive at a DIFFERENT session, and pruned by
    /// [`forget_dead_panes`](DriveEntry::forget_dead_panes) like them — bounded
    /// by LIVENESS, never by size, for that function's reason.
    #[serde(default)]
    pub founding_panes: Vec<String>,
    /// The orchestrator this drive acts for. Every action taken under it is
    /// audited with this as the `on_behalf_of` detail key — the actor stays
    /// `brand::AUDIT_ACTOR`, so it is this key, not the actor, that
    /// distinguishes a driver action from any other host action (§3).
    #[serde(default)]
    pub on_behalf_of: String,
    #[serde(default)]
    pub lanes: Vec<LaneRecord>,
    /// Which lane of the gate's required list is open. Advancing it is **not**
    /// a transition (§2.1) — the entry stays in `review-wait`.
    #[serde(default)]
    pub lane_index: usize,
    /// What this drive has spent of INVARIANT 9's budget (§2.3).
    ///
    /// **Required, with no `serde(default)`, for [`DriveState`]'s reason.** An
    /// absent counter block is not a v1 file with nothing spent: defaulting it
    /// to zeros silently grants three fresh review rounds, which is precisely
    /// the outcome §5.2 forbids when it refuses to prune a parked entry. The
    /// conservative direction here is to refuse the file, not to guess low.
    pub counters: Counters,
    /// When the drive began, absolute. The `drive-stalled` anchor: §2.2 derives
    /// an **age** from it (`now - started_ms`), and since #2110 that age is the
    /// BACKSTOP — the bound §8's `also: [base-green]` cycler cannot reset,
    /// beneath the per-state clocks that do the work
    /// ([`state_since_ms`](DriveEntry::state_since_ms)). A stored *age* would be
    /// stale the instant it was written and meaningless across a restart.
    ///
    /// Time the drive spent unable to spawn is subtracted from it rather than
    /// charged to it — [`bounded_age_ms`](DriveEntry::bounded_age_ms).
    #[serde(default)]
    pub started_ms: u64,
    /// When this drive last entered `fix-wait` — the `fix-stalled` anchor, and
    /// the only clock here that is not the drive's age. Beyond §5.2's example;
    /// see the module header, including why it is not the general
    /// "state last changed" stamp it looks like it wants to be.
    ///
    /// Written by [`advance`](DriveEntry::advance) on arcs 3 and 5 — the two
    /// that reach `fix-wait` — and by nothing else.
    ///
    /// **Zero means the drive has never been handed back.** It does not also
    /// mean "written before this field existed", and the distinction is worth
    /// a sentence because the second reading looks plausible and would license
    /// a guard that guards nothing.
    ///
    /// `now - 0` clears any timeout trivially, so a zero read while the entry
    /// is in `fix-wait` *would* park the drive on a false `held(fix-stalled)`.
    /// No such entry can exist. Nothing writes `review_drives.json` before
    /// S3's tick, and S3 ships alongside this module — so the field is present
    /// in the first file ever written, and there is no build whose output
    /// lacks it. §5.2's read tolerance makes the older *shape* parse; it does
    /// not conjure a writer that produced one. A `fix-wait` entry therefore
    /// always carries an anchor stamped by
    /// [`advance`](DriveEntry::advance) on arc 3 or arc 5, and the comparison
    /// in [`decide`] is left plain deliberately: a defensive `!= 0` here would
    /// have no reachable subject, and the next reader would take it as
    /// evidence that one exists.
    ///
    /// If a future change ever lets something else author an entry — a
    /// migration, an import, a hand-repaired file — that is the change that
    /// owes this field a decision, and this paragraph is the one it invalidates.
    #[serde(default)]
    pub fix_handback_ms: u64,
    /// When this drive last answered a worker's `report(progress)` in its own
    /// pane (#1959) — the bound on [`kickback_owed`](DriveEntry::kickback_owed).
    ///
    /// **Compared against `fix_handback_ms` rather than counted**, so the budget
    /// is one per HAND-BACK and renews on the next one without anything having
    /// to reset it: a worker that reports progress five times in one fix round
    /// is answered once, and a worker that does it again after the next
    /// hand-back is answered again. A counter would have needed a reset on
    /// arcs 3 and 5, which is a second thing to remember beside the anchor those
    /// arcs already stamp.
    ///
    /// Zero means "never answered", and reads correctly against a zero
    /// `fix_handback_ms` too: `0 < 0` is false, so a drive that has never handed
    /// back owes nothing.
    #[serde(default)]
    pub fix_kickback_ms: u64,
    /// **This drive reached `ci-wait` by arc 7 — the worker pushed a fix** —
    /// and has not left it since (#2168 E1). What `decide_ci_wait` reads to
    /// tell a fix push from every other way into that state.
    ///
    /// **Why the state alone cannot answer it.** `ci-wait` is entered five
    /// ways — the creation (arc 1) and arcs 6, 7, 10 and 11 — and only arc 7
    /// means "this drive handed the worker a fix and the worker has just
    /// pushed it". The defect #1875 measures needs exactly that distinction:
    /// after a push the worker fills the PR body's CI section, which it can
    /// only do once the checks have settled, so a lane briefed the moment CI
    /// goes green is briefed at a body digest the worker is about to move —
    /// and the pass it records is stale before it is written. Every code PR of
    /// that session paid at least one re-record round for it.
    ///
    /// **Arc 6 is a worker push too and is deliberately NOT covered**, because
    /// the two preconditions for expecting a `report(done)` are the ones arc 7
    /// carries and arc 6 does not. §7's interception is keyed on
    /// [`worker_agent`](DriveEntry::worker_agent), which is empty until the
    /// first hand-back — so before one, a driven worker's `report` goes to the
    /// orchestrator's pane exactly as it always did and no tick can ever see a
    /// `Done`. And nothing has asked that worker for one: arc 6 is a push the
    /// drive did not solicit, mid-review, while arc 7 answers a hand-back whose
    /// brief says *push, and report when the checks are green*. Gating arc 6 on
    /// a signal that cannot arrive and was never requested would park every such
    /// drive on `held(fix-stalled)` at the timeout — a false park, where the
    /// arc-6 status quo costs at most the one re-record round.
    ///
    /// **A STAMP and not a flag, and the first draft of this slice had it the
    /// other way** (rev-final round 2, premortem 2). The obvious shape is a
    /// `bool` bounded from [`state_since_ms`](DriveEntry::state_since_ms) — one
    /// clock instead of two, and true as far as it goes, since
    /// [`transition`] refuses a `ci-wait` -> `ci-wait` self-arc so the state
    /// stamp really is the arc-7 moment. What that argument misses is that a
    /// worker may push AGAIN inside one `ci-wait` stay. No arc fires, nothing
    /// re-stamps, and the wait therefore runs from the FIRST push: a follow-up
    /// commit fifty-five minutes into a sixty-minute `fix_timeout_minutes`
    /// leaves five minutes to run a fresh matrix and report, and the
    /// `held(fix-stalled)` notice then names the current head beside a clock
    /// belonging to the previous one. So this is the anchor, re-stamped by
    /// [`note_fix_push`](DriveEntry::note_fix_push) on every observed head move
    /// while it is set — and it is a THIRD clock only in the sense that it
    /// measures a wait `state_since_ms` cannot.
    ///
    /// **Written by [`advance`](DriveEntry::advance) on every arc** — `Some` on
    /// `fix-wait` -> `ci-wait`, `None` otherwise — and re-stamped by
    /// `note_fix_push` in between. Assigning on every arc is not optional:
    /// an entry that carried it out of `ci-wait` and back in by arc 10 would
    /// claim a push that did not happen. And `note_fix_push` re-stamps only
    /// what is already `Some`, so a head move in any other state cannot
    /// manufacture one.
    ///
    /// **`None` on an entry written before this field existed**, and that is
    /// the safe direction rather than an accident: such a drive advances on
    /// green alone, which is the pre-#2168 behaviour, so an upgrade mid-drive
    /// costs at most the one re-record round it was already going to cost. The
    /// other direction would park a first drive on `held(fix-stalled)` waiting
    /// for a `report(done)` its worker was never asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_pushed_ms: Option<u64>,
    /// The notice this entry owes the orchestrator's pane, and the reason
    /// retention may not drop it yet (#1857). See [`OwedNotice`].
    ///
    /// **Absent is the resting state, so it is not serialized when absent** —
    /// every entry that is not a terminal one mid-delivery carries nothing, and
    /// writing `"owed_notice": null` onto each of them would be noise in a file
    /// §5.2 publishes the shape of. A build that predates the field reads it as
    /// an unknown key into `extra` and rewrites it verbatim, which is the
    /// forward-compatibility promise §5.2 already makes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owed_notice: Option<OwedNotice>,
    /// When the live-delegate cap started refusing this drive's lane spawns,
    /// absolute — the [`HeldReason::CapFull`] anchor (#2109).
    ///
    /// **`None` rather than a zero sentinel**, deliberately, because this is
    /// the one clock here whose "unset" and whose lowest legal value are both
    /// reachable in the same run: `fix_handback_ms` argues at length that a
    /// zero cannot occur in `fix-wait`, and that argument does not transfer —
    /// a cap refusal is observed at whatever `now_ms` the tick was handed, and
    /// a test harness ticks from small numbers. `Option` makes "not currently
    /// starved" a value the type carries rather than one a comment defends.
    ///
    /// Stamped by the tick on the FIRST **cap** refusal of a run and left alone
    /// by the cap refusals after it, so what it measures is the duration of the
    /// starvation and not the age of the most recent tick.
    ///
    /// **Cleared at four sites, and the NON-CAP one is what makes the word
    /// "continuously" on [`HeldReason::CapFull`] true** (#2109 review 4).
    /// Named rather than numbered, because an ordinal here is a claim about a
    /// list's ORDER and goes stale the moment the list grows: #2135 added a
    /// fourth site and bumped "the third" to "the fourth" in step, which
    /// silently moved the credit onto the restart clear — contradicting the
    /// citation on this very sentence, the sentence after it, and two other
    /// surfaces.
    /// [`clear_cap_starvation`](DriveEntry::clear_cap_starvation) runs when a
    /// lane does open, and on any refusal that is **not** the cap's;
    /// [`advance`](DriveEntry::advance) runs on every arc; and
    /// [`discard_cap_starvation_run`](DriveEntry::discard_cap_starvation_run)
    /// runs at §2.4's restart reconcile. The first two are the tick's, and it is
    /// the non-cap one that keeps this a claim about the
    /// run happening NOW: guarded on the write edge alone, the stamp was a
    /// latch, and a single early cap refusal aged into `held(cap-full)` behind
    /// a run of refusals that were nothing of the kind. The arc clear is its own
    /// reason — a drive that MOVED is not the drive that was stuck, and carrying
    /// the stamp across an arc would let a later, unrelated refusal inherit a
    /// duration it did not spend. The restart clear is #2135's, and is the only
    /// one that charges nothing: a run cannot straddle a process boundary,
    /// because no tick observed the cap across it — that function carries the
    /// whole argument.
    ///
    /// **This field IS serialized, so the stamp survives a shutdown**, and the
    /// restart clear above is what stops that from being a defect rather than a
    /// feature. It is written for the same reason every other clock here is:
    /// §5.2 is the drive's whole memory, and a field the tick decides from that
    /// the file omitted would make a resumed drive decide from a different
    /// entry than the one that was stored.
    ///
    /// Absent is the resting state, so it is not serialized when absent —
    /// `owed_notice`'s reason, unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_starved_since_ms: Option<u64>,
    /// When the drive entered the state it is in now — the
    /// [`HeldReason::StateStalled`] anchor (#2110).
    ///
    /// **This IS the general "when did the state last change" stamp the module
    /// header used to say must never exist, and the ban is discharged rather
    /// than ignored.** What §2.2 forbade was an idle clock *as the drive's only
    /// bound*, and its worked example is §8's `also: [base-green]` row: a drive
    /// that advances `gate-check` → `ci-wait` on every wake resets a per-state
    /// stamp forever, so a bound measured from one alone would leave it parked
    /// on a red default branch in silence. That is still true, and it is still
    /// the reason `started_ms` and `drive-stalled` remain — as the BACKSTOP the
    /// base-green cycler falls through to. The two clocks answer different
    /// questions, and the incident that produced #2110 is what happens when only
    /// the second is asked: a drive with progress on every axis was reported as
    /// stalled, because an age cannot tell progress from paralysis.
    ///
    /// Written by [`advance`](DriveEntry::advance) on EVERY arc, which is the
    /// difference from [`fix_handback_ms`](DriveEntry::fix_handback_ms) beside
    /// it — that one is stamped on two arcs and means something narrower.
    ///
    /// **Zero on an entry written before this field existed**, which reads as
    /// "entered at the epoch" and so as a state older than any bound. That is
    /// the safe direction and it is bounded rather than merely tolerable: such
    /// an entry holds `state-stalled` on the first tick at which the DRIVE'S OWN
    /// AGE reaches the bound: [`state_elapsed_ms`](DriveEntry::state_elapsed_ms)
    /// is capped at that age (#2117 review 3), so a young entry is not parked —
    /// which is correct, and is what this sentence overstated as "on its first
    /// tick" for as long as the cap existed. Its notice names the state, and its
    /// remedy — `drive_review` — re-stamps this field through `advance` and does
    /// not re-hold. The alternative
    /// (treating zero as "unknown, exempt") would make the field's own bound
    /// unreachable for exactly the entries that predate it.
    #[serde(default)]
    pub state_since_ms: u64,
    /// Milliseconds this drive has spent unable to spawn, summed over the
    /// STARVATION RUNS THAT HAVE ENDED, for the whole life of the drive —
    /// subtracted from the age `drive-stalled` measures (#2110).
    ///
    /// **A hold is not progress and it is not a stall**, which is the whole of
    /// why this is subtracted rather than counted: the measured drive spent
    /// three of its four hours in `review-wait` with `lanes: []` because
    /// another drive's released lanes held every slot, and that starvation was
    /// charged to the budget of the drive that was starved. Nothing about that
    /// time was the driven PR's doing and nothing about it was recoverable by
    /// the orchestrator the notice went to.
    ///
    /// **The run in flight is NOT in here**, deliberately: it is
    /// [`cap_starved_since_ms`](DriveEntry::cap_starved_since_ms), and
    /// [`starved_ms`](DriveEntry::starved_ms) adds it live. A stored total that
    /// included an open run would have to be re-written on every tick to stay
    /// true, which is the stored-age mistake `started_ms` argues against one
    /// field up.
    ///
    /// Reset with `started_ms` on arc 11, for that field's reason: a resumed
    /// drive's age starts again, so what it had spent starved before the resume
    /// is not owed back twice.
    #[serde(default)]
    pub starved_total_ms: u64,
    /// The same sum, but only over the runs that ended since the drive entered
    /// its current state — subtracted from the elapsed time
    /// [`HeldReason::StateStalled`] measures (#2110).
    ///
    /// A second accumulator rather than a subtraction of two snapshots, because
    /// `advance` clears [`cap_starved_since_ms`](DriveEntry::cap_starved_since_ms)
    /// on every arc, so a starvation run can never straddle a transition and
    /// the two totals genuinely diverge only in what they are reset by. It is
    /// not always zero: `clear_cap_starvation` ends a run when a lane opens,
    /// which is not a transition, so one `review-wait` may contain several.
    #[serde(default)]
    pub starved_state_ms: u64,
    /// The working state this drive parked OUT of, and how long it had been
    /// there — what a hold's notice and `review_drive_status` need to say what
    /// the drive was doing when a bound fired (#2110).
    ///
    /// Stamped by [`advance`](DriveEntry::advance) on every arc into `held` and
    /// cleared on every other, so it describes THIS hold and never a previous
    /// one. `None` on an entry that is not parked.
    ///
    /// **The elapsed figure is stamped rather than derived**, because the arc
    /// that records it is the same arc that re-stamps
    /// [`state_since_ms`](DriveEntry::state_since_ms): by the time anything
    /// reads the entry the clock the hold fired on has already been reset, and
    /// a reader recomputing it would report the age of the hold instead of the
    /// wait that caused it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_from: Option<DriveState>,
    /// Time in [`held_from`](DriveEntry::held_from) at the moment of the hold,
    /// starvation already excluded — see that field.
    #[serde(default)]
    pub held_after_ms: u64,
    /// **The key of the last hold this entry ANNOUNCED** (#3040 N1) — the
    /// reason, the head and the counters it was spent at, as
    /// [`hold_key`] renders them.
    ///
    /// A hold can only ever repeat after a resume: `transition` refuses a
    /// `held` -> `held` self-arc, so the drive must come back through arc 11
    /// and park again. That is exactly the shape the notice census found —
    /// three `worker-unresumable` holds on one PR, each preceded by its own
    /// `rd-resumed` — and where the resume changed nothing the drive can
    /// observe (the same rendered line at the same reason, head and
    /// counters), the second line
    /// tells the orchestrator nothing the first did not.
    ///
    /// **"Nothing it can observe" is the RENDERED LINE, not the tuple**
    /// (rev-final round 3). The key digests the notice, so an interpolated
    /// fact no field here models — `HeldFacts::refusal` on
    /// `worker-unresumable`, `cap-refused` and `cap-full`, which is #1961's
    /// observation rather than a diagnosis — changes the key and announces.
    /// Those three spend no counter and need no push, so head and counters are
    /// exactly the two things that do not move across such a repeat; keyed on
    /// the tuple alone, a hand-back that failed differently the second time
    /// said nothing at all.
    ///
    /// **One exemption survives that**, and only one:
    /// [`repeat_carries_new_information`] names the two reasons whose line can
    /// render WORD FOR WORD the same and still be news, because the duration
    /// in it is rounded to minutes.
    ///
    /// **The head is in the key and is what makes the suppression safe.** A
    /// resume after the worker pushed is a hold about a different revision, so
    /// it announces; that is the case an orchestrator most needs to see, and
    /// it is the one a key of (pr, reason) alone would swallow.
    ///
    /// **The counter VALUES cannot see a spent round on their own**, which is
    /// why [`DriveEntry::rearm_hold_notice`] exists: `reset_counters: true`
    /// puts `review_rounds` back to zero and the next `review-limit` hold
    /// fires at the same bound, so the key would read identical across a round
    /// the orchestrator paid for. `drive_review` clears this on a resetting
    /// resume, and the hold is announced again.
    ///
    /// Absent is the resting state, so it is not serialized when absent —
    /// `owed_notice`'s reason, unchanged. An older build reads it as an
    /// unknown key into `extra` and writes it back verbatim (§11.2), and a
    /// build that predates it simply announces every hold, which is the
    /// pre-#3040 behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_hold_key: Option<String>,
    /// **Non-blocking rounds this drive has run on its own** (#3367 item 1) —
    /// the count `driver.fix_nonblocking_rounds` bounds.
    ///
    /// On the entry rather than in [`Counters`], and defaulted, which that
    /// type's rule forbids a new COUNT — so the exception is argued rather than
    /// taken. `Counters`' rule exists because a defaulted zero there silently
    /// grants a fresh INVARIANT 9 budget. This is not that budget: every round
    /// counted here was ALSO counted in `review_rounds`, which is required, so
    /// a zero read off an entry written before the field existed grants at
    /// most rounds the shared bound still refuses. And zero is the TRUE reading
    /// of such an entry — no build before #3367 could have run one.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub nit_rounds: u32,
    /// **The current `fix-wait` was entered by a non-blocking round** (#3367),
    /// assigned on every arc into `fix-wait` by
    /// [`advance`](DriveEntry::advance). Read by the hand-back brief — which
    /// must not tell a worker a lane "recorded FAIL" when every lane passed —
    /// including the brief a restart re-sends, which is why it is persisted
    /// rather than read off the step.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nit_handback: bool,
    /// The `(head, body digest)` the last non-blocking round was handed back
    /// AT (#3367). A gate satisfied again at the SAME revision means the
    /// worker changed nothing — it answered the findings rather than acting on
    /// them — and another round would hand back the same list; see
    /// [`nonblocking_round_applies`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nit_head: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nit_digest: String,
    /// **The worker `report(done)` that STARTED this drive** (#3367 item 2,
    /// `driver.auto_drive_on_done`), already composed and scrubbed as the
    /// orchestrator's pane would have received it.
    ///
    /// The report is not delivered when it starts a drive — that is the wake
    /// the feature exists to remove — so it has to reach the orchestrator some
    /// other way, and the way is the drive's FIRST notice, which is taken from
    /// here (`Option::take`) and prefixed. Persisted rather than in memory,
    /// because the first notice may be hours and one restart away, and a
    /// worker's words lost across a restart are words nobody can recover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_report: Option<String>,
    /// Preserved unknown fields — see [`ReviewDrivesState`].
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

impl DriveEntry {
    /// **Arc 1**: `(none) -> ci-wait`, `drive_review` on a PR with no live
    /// entry (§5.1). A creation rather than a transition, which is why
    /// [`transition`] has no arm for it.
    pub fn new(
        pr: u64,
        worker_session: &str,
        on_behalf_of: &str,
        counters: Counters,
        now_ms: u64,
    ) -> DriveEntry {
        DriveEntry {
            pr,
            state: DriveState::CiWait,
            held_reason: None,
            head: String::new(),
            body_digest: String::new(),
            worker_session: worker_session.to_string(),
            worker_agent: String::new(),
            prior_worker_agents: Vec::new(),
            founding_panes: Vec::new(),
            on_behalf_of: on_behalf_of.to_string(),
            lanes: Vec::new(),
            lane_index: 0,
            counters,
            started_ms: now_ms,
            fix_handback_ms: 0,
            fix_kickback_ms: 0,
            fix_pushed_ms: None,
            owed_notice: None,
            cap_starved_since_ms: None,
            state_since_ms: now_ms,
            starved_total_ms: 0,
            starved_state_ms: 0,
            held_from: None,
            held_after_ms: 0,
            last_hold_key: None,
            nit_rounds: 0,
            nit_handback: false,
            nit_head: String::new(),
            nit_digest: String::new(),
            auto_report: None,
            extra: BTreeMap::new(),
        }
    }

    /// **Announce this hold, or say it repeats one already announced**
    /// (#3040 N1) — the whole of the dedup decision, on the entry that owns
    /// the record, so the tick cannot get half of it right.
    ///
    /// Answers `true` when the notice is to be delivered, and stamps the key
    /// as it does. Answers `false` when this drive already announced a hold
    /// with this exact key and nothing it can observe has changed since — the
    /// tick then writes `rd-hold-repeated` instead of a line into the pane.
    ///
    /// **It stamps on the announce and not on the hold**, so a notice the
    /// caller decides not to build cannot silence the next one. See
    /// [`last_hold_key`](DriveEntry::last_hold_key) for why the key is what it
    /// is — it digests the LINE, so any fact the notice interpolates is in it —
    /// and [`repeat_carries_new_information`] for the two reasons this never
    /// suppresses even when the line really is identical.
    pub fn announce_hold(&mut self, reason: HeldReason, notice: &str) -> bool {
        let key = hold_key(reason, &self.head, &self.counters, notice);
        // **The stamp happens whatever the answer**, so the NEXT hold compares
        // against the one that really just fired rather than against an older
        // one a time-bound hold happened to skip past.
        let seen = self.last_hold_key.as_deref() == Some(key.as_str());
        self.last_hold_key = Some(key);
        !seen || repeat_carries_new_information(reason)
    }

    /// **Forget which hold was last announced, so the next one is** (#3040
    /// N1) — `drive_review(reset_counters: true)`, and nothing else.
    ///
    /// The counters in the key cannot see this on their own: a reset puts
    /// `review_rounds` back to zero and the next `review-limit` hold fires at
    /// the same bound, so the key would read identical across a round the
    /// orchestrator deliberately paid for. A plain resume is left alone on
    /// purpose — that is the repeat this exists to suppress.
    pub fn rearm_hold_notice(&mut self) {
        self.last_hold_key = None;
    }

    pub fn state(&self) -> DriveState {
        self.state
    }

    /// Whether this drive is sitting on a head its own worker pushed in answer
    /// to a hand-back — see [`fix_pushed_ms`](DriveEntry::fix_pushed_ms).
    pub fn fix_pushed(&self) -> bool {
        self.fix_pushed_ms.is_some()
    }

    /// **A further push landed while the drive is still waiting on this one**
    /// (#2168 E1, rev-final round 2 premortem 2). Re-anchors the receipts wait
    /// at `now_ms`, so a worker that pushes a follow-up commit gets a whole
    /// window to run the new matrix and report rather than the remainder of the
    /// window the previous push opened.
    ///
    /// **Re-stamps only what is already `Some`**, which is what keeps this from
    /// being a second way to enter the wait: a head move in `review-wait` is
    /// arc 6 and a head move in `fix-wait` is arc 7, and both go through
    /// [`advance`](DriveEntry::advance). Called by S3 at the one place a moved
    /// head is recorded, and that call site already guards on the head having
    /// actually changed and on the read having succeeded — an empty `facts.head`
    /// is a failed read, not a push, and re-anchoring on one would hand a silent
    /// worker a fresh hour every time `gh` hiccuped.
    ///
    /// It cannot postpone the drive indefinitely: `state-stalled` measures from
    /// `state_since_ms`, which nothing here touches, so a drive that pushes
    /// forever still parks on `ci-wait`'s own bound.
    pub fn note_fix_push(&mut self, now_ms: u64) {
        if self.fix_pushed_ms.is_some() {
            self.fix_pushed_ms = Some(now_ms);
        }
    }

    /// The notice this entry owes a pane, if any (#1857).
    pub fn owed_notice(&self) -> Option<&OwedNotice> {
        self.owed_notice.as_ref()
    }

    /// Record `text` as owed by this entry, anchored at `now_ms` (#1857).
    ///
    /// Called at the moment the entry goes terminal, **inside the same
    /// load-decide-store the arc itself takes**, so the obligation is on disk
    /// before anything tries to deliver it. That ordering is the whole
    /// mechanism: a notice built and handed straight to a delivery is lost the
    /// moment the delivery answers `Err`, which is #1857.
    ///
    /// **The first write wins.** Re-owing an entry that is already owing would
    /// re-arm the ceiling clock, and a clock re-armed by the retry it bounds is
    /// the unbounded retry this exists to not be.
    pub fn owe_notice(&mut self, text: &str, now_ms: u64) {
        if self.owed_notice.is_none() {
            self.owed_notice =
                Some(OwedNotice { text: text.to_string(), owed_ms: now_ms, failures: 0 });
        }
    }

    /// The notice reached a pane. Clearing it is what lets retention prune —
    /// see [`prune_terminal`].
    pub fn notice_delivered(&mut self) {
        self.owed_notice = None;
    }

    /// One delivery attempt failed. Counts the attempt and **does not touch the
    /// ceiling anchor**, for [`owe_notice`](DriveEntry::owe_notice)'s reason.
    pub fn notice_delivery_failed(&mut self) {
        if let Some(n) = self.owed_notice.as_mut() {
            n.failures = n.failures.saturating_add(1);
        }
    }

    /// Move this entry to `to`, or refuse. The **only** mutation path for
    /// [`DriveEntry::state`]; see that field's comment.
    ///
    /// `reason` must be `Some` exactly when `to` is [`DriveState::Held`] — a
    /// hold with no reason has nothing to put in its notice or its `rd-held`
    /// line, and a reason on any other arc would survive into
    /// `review_drive_status()` as a claim about a drive that is not parked.
    /// Both are refused as [`InvalidTransition`] rather than silently fixed.
    ///
    /// [`fix_handback_ms`](DriveEntry::fix_handback_ms) is stamped **only** on
    /// an arc into `fix-wait`, and stays that way now that
    /// [`state_since_ms`](DriveEntry::state_since_ms) beside it IS written on
    /// every arc (#2110): they anchor different bounds, and folding the narrow
    /// one into the general one would give `held(fix-stalled)` — a claim about
    /// the WORKER's silence — a clock that a lane opening or a gate re-check
    /// could restart.
    /// `bump` is a **parameter rather than a separate call** for the same
    /// reason `reason` is checked here: a caller that could take arc 5 and
    /// forget the `review_rounds` increment would spend an unbounded number of
    /// review rounds against a bound of three, which is INVARIANT 9 defeated by
    /// an omission rather than by a decision. Pairing them in one signature
    /// means the transition and its cost cannot come apart.
    ///
    /// The counter moves **after** the transition is accepted, so a refused arc
    /// spends nothing.
    pub fn advance(
        &mut self,
        to: DriveState,
        reason: Option<HeldReason>,
        bump: Option<Counter>,
        now_ms: u64,
    ) -> Result<(), InvalidTransition> {
        if reason.is_some() != (to == DriveState::Held) {
            return Err(InvalidTransition { from: self.state, to });
        }
        // Read before the arc is taken and before any clock below is re-stamped
        // (#2110). A refused arc must spend nothing, so nothing is written from
        // these until `transition` has accepted.
        let from = self.state;
        let state_elapsed = self.state_elapsed_ms(now_ms);
        self.state = transition(self.state, to)?;
        self.held_reason = reason;
        match bump {
            Some(Counter::ReviewRounds) => self.counters.review_rounds += 1,
            Some(Counter::CiAttempts) => self.counters.ci_attempts += 1,
            Some(Counter::RebaseAttempts) => self.counters.rebase_attempts += 1,
            // #2509. An ASSIGNMENT to `true` rather than a bump: the grace is
            // one-shot, and `decide_review_wait` refuses to propose this bump
            // a second time, so a set here can only ever be idempotent. It
            // deliberately does NOT touch `review_rounds`, which is what keeps
            // INVARIANT 9's ceiling meaning what it meant.
            Some(Counter::BodyOnlyGrace) => self.counters.body_only_grace = true,
            // #3367: BOTH, and in one arm — see the variant. `review_rounds` is
            // what keeps the shared bound shared; `nit_rounds` is what keeps
            // `driver.fix_nonblocking_rounds` its own, tighter, limit.
            Some(Counter::NonblockingRound) => {
                self.counters.review_rounds += 1;
                self.nit_rounds += 1;
            }
            None => {}
        }
        if to == DriveState::FixWait {
            self.fix_handback_ms = now_ms;
            // #3367: an ASSIGNMENT on every arc into `fix-wait`, for
            // `fix_pushed_ms`'s reason below — a flag that only a nit arc set
            // and nothing cleared would render a conflict or red-CI hand-back
            // as a non-blocking round.
            self.nit_handback = bump == Some(Counter::NonblockingRound);
        }
        // **Arc 7, recorded — an ASSIGNMENT and not a set** (#2168 E1). Every
        // other arc clears it, including the three that reach `ci-wait` from
        // somewhere else: an entry that carried the flag out of `ci-wait` and
        // back in by arc 10 would claim a push nobody made, and hold the drive
        // waiting on a `report(done)` its worker was never asked for.
        //
        // Written here rather than in the tick for `fix_handback_ms`'s reason:
        // the arc and the fact it establishes may not come apart. And it is
        // read against `state_since_ms`, which the lines below re-stamp on this
        // same arc — see the field, and the module header on why that is not a
        // fourth clock.
        self.fix_pushed_ms = (from == DriveState::FixWait && to == DriveState::CiWait).then_some(now_ms);
        // **What the drive was doing, recorded before the clocks that say so
        // are reset** (#2110). `held_from` and `held_after_ms` are read by this
        // hold's notice and by `review_drive_status`; both are computed from
        // `state_since_ms`, which the next three lines overwrite.
        if to == DriveState::Held {
            self.held_from = Some(from);
            self.held_after_ms = state_elapsed;
        } else {
            self.held_from = None;
            self.held_after_ms = 0;
        }
        // #2109: an arc is proof the drive is no longer stuck on the cap, so
        // the starvation clock does not survive one. Including the arc INTO
        // `held(cap-full)` itself — the hold is the report, and a resume must
        // start the clock over rather than re-hold on the next tick from a
        // stamp the previous starvation left behind.
        //
        // #2110: what that run COST is kept, though, and the lifetime total is
        // what `drive-stalled` subtracts. Folded here rather than discarded,
        // because the arc out of `held(cap-full)` is precisely the arc after the
        // longest run there is.
        self.end_starvation_run(now_ms);
        self.cap_starved_since_ms = None;
        // The per-state clock, re-stamped on EVERY arc — see `state_since_ms`
        // for why that is the stamp §2.2 used to forbid and why the backstop is
        // what makes it safe.
        self.state_since_ms = now_ms;
        self.starved_state_ms = 0;
        Ok(())
    }

    /// Fold any starvation run in flight into both accumulators and end it.
    ///
    /// Shared by [`advance`](DriveEntry::advance) and
    /// [`clear_cap_starvation`](DriveEntry::clear_cap_starvation) so that the
    /// two ways a run can end cost the same thing — a run charged by one path
    /// and not the other would make the exclusion depend on whether the cap
    /// cleared before or after the drive moved, which is the arbitrary half of
    /// the original defect.
    fn end_starvation_run(&mut self, now_ms: u64) {
        if let Some(since) = self.cap_starved_since_ms {
            let d = now_ms.saturating_sub(since);
            self.starved_total_ms = self.starved_total_ms.saturating_add(d);
            self.starved_state_ms = self.starved_state_ms.saturating_add(d);
        }
    }

    /// Everything this drive has spent unable to spawn, the run in flight
    /// included (#2110). The quantity `drive-stalled` subtracts.
    pub fn starved_ms(&self, now_ms: u64) -> u64 {
        self.starved_total_ms.saturating_add(self.open_starvation_ms(now_ms))
    }

    /// The same, since the current state was entered — what
    /// [`HeldReason::StateStalled`] subtracts.
    pub fn starved_in_state_ms(&self, now_ms: u64) -> u64 {
        self.starved_state_ms.saturating_add(self.open_starvation_ms(now_ms))
    }

    fn open_starvation_ms(&self, now_ms: u64) -> u64 {
        self.cap_starved_since_ms.map_or(0, |since| now_ms.saturating_sub(since))
    }

    /// How long this drive has been in its current state, starvation excluded
    /// — the [`HeldReason::StateStalled`] measure (#2110).
    ///
    /// **Capped at the drive's own age, because a drive cannot have been in a
    /// state longer than it has existed** (#2117 review 2, premortem 1). That
    /// is a tautology about a healthy entry and load-bearing about one entry
    /// class that is not: an entry written before
    /// [`state_since_ms`](DriveEntry::state_since_ms) existed reads zero there,
    /// so the raw subtraction answers `now` — an epoch-scaled figure. The HOLD
    /// that produces is correct and argued at that field; what was wrong is the
    /// number, because [`advance`](DriveEntry::advance) stamps `held_after_ms`
    /// from here and the notice then told an operator their drive had been in
    /// `ci-wait` for some twenty thousand days. Capping makes that notice read
    /// the drive's real age, which is both true and the figure they want.
    pub fn state_elapsed_ms(&self, now_ms: u64) -> u64 {
        now_ms
            .saturating_sub(self.state_since_ms)
            .saturating_sub(self.starved_in_state_ms(now_ms))
            .min(self.bounded_age_ms(now_ms))
    }

    /// Record that the live-delegate cap refused this drive's lane spawn, and
    /// answer whether that changed anything (#2109).
    ///
    /// **First-refusal-wins**: a run of CAP refusals is one starvation, and the
    /// stamp is when it began. ("Cap" rather than bare "refusals" because that
    /// is the only kind that reaches here at all, and the loose word read as a
    /// claim the field's own doc contradicts — #2135, folded from #2112's final
    /// pass.) Re-stamping on each tick would make
    /// [`CAP_HOLD_MS`] unreachable — the same defeat-your-own-bound shape
    /// `decide`'s empty-head guard describes, where every tick re-armed the
    /// clock meant to catch it.
    pub fn note_cap_starvation(&mut self, now_ms: u64) -> bool {
        if self.cap_starved_since_ms.is_some() {
            return false;
        }
        self.cap_starved_since_ms = Some(now_ms);
        true
    }

    /// Forget any recorded cap starvation, and answer whether there was one.
    ///
    /// **Two callers, both the tick's**: a lane DOES open, or the tick takes a
    /// refusal that is **not** the cap's. Both mean the cap run is over, and the
    /// second is #2109 review 4's — without it this doc described a rule the
    /// tick had stopped following.
    ///
    /// The stamp has two clear sites that are not calls of this function, and
    /// they are said explicitly because "who calls this" and "what clears the
    /// stamp" are different questions — a reader who conflates them will grep
    /// for this name and conclude the other two do not exist.
    /// [`advance`](DriveEntry::advance) zeroes the field directly on every arc,
    /// and [`discard_cap_starvation_run`](DriveEntry::discard_cap_starvation_run)
    /// is the restart reconcile's (#2135).
    ///
    /// Takes `now_ms` since #2110 because ending a run is no longer free: what
    /// it cost is folded into the accumulators the age bound subtracts, and a
    /// run ended without charging it would leave the time in the budget of the
    /// drive that was starved. That is exactly why the restart clear is a
    /// separate function rather than a call of this one — see it.
    pub fn clear_cap_starvation(&mut self, now_ms: u64) -> bool {
        self.end_starvation_run(now_ms);
        self.cap_starved_since_ms.take().is_some()
    }

    /// Drop a starvation run **without charging what it cost**, and answer
    /// whether there was one — §2.4's restart reconcile (#2135).
    ///
    /// **A starvation run cannot straddle a process boundary**, because every
    /// other site that touches this stamp is a tick that OBSERVED the cap, and
    /// across a shutdown no tick ran. What the field means — "the cap has been
    /// refusing this drive's lane spawns continuously since this instant" — is
    /// therefore not a claim the surviving stamp can still make: the interval
    /// between the last tick of the old process and the first of the new one is
    /// time in which the cap refused nothing, and after a restart every pane
    /// this group's cap was counting is gone. Left standing, a stamp older than
    /// [`CAP_HOLD_MS`] parks the resumed drive `held(cap-full)` on its FIRST
    /// tick, before a single spawn is attempted, on a notice telling an
    /// orchestrator to free a slot in a group whose slots are all free.
    ///
    /// **Nothing is charged, and that is the choice rather than an oversight.**
    /// [`clear_cap_starvation`](DriveEntry::clear_cap_starvation) folds
    /// `now - since` into the two accumulators both age bounds subtract, and
    /// here that difference is mostly orrerix's own downtime — so charging it
    /// would FORGIVE the downtime from `drive-stalled` and `state-stalled`,
    /// which is precisely the property #2117 disclosed and pinned as charged
    /// (`orrerix_downtime_is_charged_to_the_state_it_spanned`). The stamp is a
    /// START and not a total, so the genuinely-starved stretch before the
    /// shutdown cannot be recovered from it either; discarding loses that
    /// forgiveness, which fails toward parking rather than toward silence and
    /// is the direction every other unknown here is resolved in.
    ///
    /// **Scoped to the process boundary, and no wider.** An in-process tick gap
    /// longer than [`CAP_HOLD_MS`] still parks on a single observed refusal —
    /// the reconcile is once per group per REGISTRY INSTANCE, so nothing here
    /// reaches it. Registry and not process, precisely: the latch is a field of
    /// the registry, so the two coincide only while a process holds one, which
    /// is true today and is not a thing the type system holds true. A second
    /// registry built over a live state root would forgive a genuinely open run
    /// — this defect in reverse, with `cap_run_forgotten: true` on the row to
    /// make it look intended. No test here can see the difference, because
    /// `relaunch_registry` IS a second registry in one process (#2135 review 2,
    /// premortem 1).
    /// That residual is real and is pinned rather than merely admitted, by
    /// `an_in_process_tick_gap_still_parks_on_a_single_observed_cap_refusal`.
    pub fn discard_cap_starvation_run(&mut self) -> bool {
        self.cap_starved_since_ms.take().is_some()
    }

    /// How long the cap has been refusing this drive's lane, or `None` when it
    /// is not currently refusing one.
    pub fn cap_starved_for(&self, now_ms: u64) -> Option<u64> {
        self.cap_starved_since_ms.map(|t| now_ms.saturating_sub(t))
    }

    /// Apply a whole [`DriveStep`] — the form S3's tick uses, so the decision
    /// and its bookkeeping travel together and neither half can be applied
    /// alone.
    ///
    /// [`DriveStep::Wait`] is a no-op. [`DriveStep::OpenLane`] is **not**
    /// applied here and returns `Ok(())` unchanged: briefing a lane needs a
    /// spawned session id, which is exactly the I/O this module does not do —
    /// S3 calls [`open_lane`](DriveEntry::open_lane) with what the spawn
    /// returned.
    ///
    /// [`DriveStep::Rehandback`] joins them for the same reason and is listed
    /// EXPLICITLY rather than swept into a wildcard: it takes no arc, so there
    /// is nothing here to apply, and what it does need — resuming the recorded
    /// worker session, then
    /// [`restamp_fix_handback`](DriveEntry::restamp_fix_handback) once that
    /// worker has actually been reached — is the tick's, in the order only the
    /// tick can know. A wildcard would silently make the NEXT variant a no-op
    /// too, which is the one way a step can be decided and never applied.
    pub fn take(&mut self, step: &DriveStep, now_ms: u64) -> Result<(), InvalidTransition> {
        match step {
            DriveStep::Wait | DriveStep::OpenLane { .. } | DriveStep::Rehandback => Ok(()),
            DriveStep::Advance { to, held_reason, bump } => {
                self.advance(*to, *held_reason, *bump, now_ms)
            }
        }
    }

    /// The lane record for a block, if this drive has opened one.
    pub fn lane(&self, block: &str) -> Option<&LaneRecord> {
        self.lanes.iter().find(|l| l.block == block)
    }

    /// Record that lane `block` was briefed at `(head, body_digest)` — a spawn
    /// or a resume, which are the same event to both bounds that read this.
    /// Replaces any prior record for that block, so a re-brief re-arms
    /// `lane-stalled` instead of measuring from the first spawn.
    ///
    /// **The digest is taken here and not derived**, so that what the lane was
    /// asked about and what [`lane_open_for`] later compares are the same fact
    /// recorded once. An unreadable body records empty, which that comparison
    /// reads as "cannot tell" rather than as drift.
    /// **The superseded pane is carried, not dropped** — see
    /// [`LaneRecord::prior_agents`].
    ///
    /// **`spawned_ms` is the `lane-stalled` ANCHOR the caller chose, not a
    /// clock read** (#2163). For an ordinary brief it is `now`; for the re-open
    /// of a lane whose pane DIED it is the anchor the previous brief set, so
    /// replacing a dead pane cannot hand the lane a fresh hour of silence.
    /// [`lane_stall_anchor`] is that choice, made in one place.
    pub fn open_lane(
        &mut self,
        block: &str,
        session: &str,
        agent: &str,
        head: &str,
        body_digest: Option<&str>,
        spawned_ms: u64,
        verify: bool,
        body_only: bool,
    ) {
        let prior = self.lane(block).map(|l| {
            let mut p = l.prior_agents.clone();
            p.push(l.agent.clone());
            p
        });
        let extra = self
            .lane(block)
            .map(|l| l.extra.clone())
            .unwrap_or_default();
        self.lanes.retain(|l| l.block != block);
        self.lanes.push(LaneRecord {
            block: block.to_string(),
            session: session.to_string(),
            agent: agent.to_string(),
            prior_agents: retain_panes(prior.unwrap_or_default(), agent),
            last_verdict: None,
            at_head: String::new(),
            briefed_head: head.to_string(),
            briefed_digest: body_digest.unwrap_or_default().to_string(),
            // #3176. A brief is the start of a round, so it cannot also be a
            // lane that has been told to stop one: `open_lane` replaces the
            // record wholesale and the mark goes with the revision it was about.
            stopped_head: String::new(),
            spawned_ms,
            // #2168 E2. Recorded from the STEP rather than re-derived here: the
            // decision is `decide_review_wait`'s, taken on the same facts that
            // chose the lane, and a second derivation at the write would be a
            // second implementation of a capability grant.
            briefed_verify: verify,
            // #2509, and recorded from the STEP for `briefed_verify`'s reason
            // one line up: the decision is `decide_review_wait`'s, taken on the
            // same facts that chose the lane.
            briefed_body_only: body_only,
            extra,
        });
    }

    /// **Does this drive still owe its worker a kick-back for the CURRENT
    /// hand-back?** (#1959)
    ///
    /// A `report(progress)` in `fix-wait` is a delivery the drive consumed and
    /// cannot act on: it moves nothing (a drive turns on the head, the checks
    /// and the verdict files) and it is exactly what a worker sends when it
    /// believes it has finished and has reached for the wrong word — a
    /// body-only fix has nothing to push and no new checks, so "report when the
    /// checks are green" has no trigger. Swallowing it silently is #1857's
    /// shape one arm over: the drive sat until the watchdog woke the
    /// orchestrator.
    ///
    /// The answer is one line typed into the worker's OWN pane, which costs the
    /// orchestrator nothing. It is bounded to one per hand-back rather than one
    /// per report, so a chatty worker cannot turn its own progress reports into
    /// a stream of prompts — an unbounded emission driven by a signal the drive
    /// does not control is the mirror image of the unbounded SUPPRESSION rule,
    /// and wants the same answer.
    ///
    /// **Wherever a hand-back is outstanding, which since #2168 E1 is two
    /// states and not one** (rev-final round 2, finding 1). This used to read
    /// "only in `fix-wait`; in any other state there is no hand-back to be
    /// waiting on and nothing the worker was asked for", and E1 falsified both
    /// halves for `ci-wait` on a head that arrived by arc 7: there the worker
    /// HAS been handed a round and IS being waited on, for the very
    /// `report(done)` this line tells it to send.
    ///
    /// Left state-scoped, the #1959 defect reappears one state over and worse.
    /// §7's interception is keyed on the calling agent and not on the drive's
    /// state, so a `report(progress)` there is consumed, the worker is answered
    /// `"reported to orchestrator"`, nothing reaches the orchestrator's pane and
    /// nothing is typed back into the worker's — and a fix timeout later the
    /// hold says the driver has heard nothing from a worker that spoke. That is
    /// the false claim `rddrive::held_notice`'s own comment goes out of its way
    /// to avoid, arriving through a guard that reads one worker input by a
    /// different rule from the other four.
    ///
    /// The budget is unchanged and still one per HAND-BACK, not one per state:
    /// `fix_handback_ms` is not re-stamped by arc 7, so a worker answered in
    /// `fix-wait` is not answered again after it pushes. The same round, the
    /// same answer, once.
    pub fn kickback_owed(&self) -> bool {
        self.handback_outstanding() && self.fix_kickback_ms < self.fix_handback_ms
    }

    /// **This drive has handed a round to its worker and is still waiting on
    /// it** — the one predicate, asked wherever that question comes up.
    ///
    /// Since #2168 E1 the wait spans TWO states and not one: `fix-wait`, and
    /// `ci-wait` on a head that arrived by arc 7, where the worker has pushed
    /// and the drive is waiting for the `report(done)` that says the matrix was
    /// read. [`kickback_owed`](DriveEntry::kickback_owed) already had to say
    /// that, and [`releasable`] has to say exactly the same thing to know whose
    /// report it is consuming — so it is spelled ONCE here rather than twice at
    /// two call sites that would then be free to drift. Two readings of "is a
    /// hand-back outstanding" is the asymmetry `CLAUDE.md` names ("a guard
    /// reads every one of its inputs by one rule"), and the measured cost of
    /// the drift when `releasable` carried the narrower one is #2811 S1: 15 of 20
    /// hand-backs in one session pushed, so their report was consumed in
    /// `ci-wait`, so the release condition and its consumer never met and the
    /// worker pane held a live-delegate slot until an orchestrator killed it by
    /// hand.
    ///
    /// It is deliberately about the STATE and not about a signal: "we asked and
    /// have not been answered" is a property of the drive, and what the worker
    /// has said is a separate input each caller reads for itself.
    pub fn handback_outstanding(&self) -> bool {
        self.state == DriveState::FixWait
            || (self.state == DriveState::CiWait && self.fix_pushed())
    }

    /// Record that this drive answered its worker at `now_ms` — see
    /// [`kickback_owed`](DriveEntry::kickback_owed).
    ///
    /// **Stamped at no earlier than the hand-back it answers**, which is what
    /// makes the budget hold across a backwards clock step (rev-std round 2,
    /// premortem 1). `now_ms` is wall-clock: a host NTP correction between the
    /// hand-back and the worker's `report(progress)` writes a stamp that never
    /// overtakes `fix_handback_ms`, `kickback_owed` stays true, and the tick
    /// re-emits the kick-back on EVERY tick for as long as the progress signal
    /// stands — the unbounded emission the bound exists to prevent, arriving
    /// through the bound itself. The clamp costs a `max` and needs no reset on
    /// the arcs into `fix-wait`, so it keeps the property that made the
    /// comparison preferable to a counter in the first place.
    pub fn record_kickback(&mut self, now_ms: u64) {
        self.fix_kickback_ms = now_ms.max(self.fix_handback_ms);
    }

    /// Re-anchor the `fix-stalled` clock on a restart hand-back (#2811 S10),
    /// **without an arc and without a counter**.
    ///
    /// [`advance`](DriveEntry::advance) stamps `fix_handback_ms` only on an arc
    /// INTO `fix-wait`, and a restart hand-back takes no arc — the drive is
    /// already there. Without this the drive would be re-briefed against a clock
    /// that started before the shutdown, so a long downtime would hold it
    /// `fix-stalled` on the very next tick, naming a worker that had a fresh
    /// brief and no time at all to answer it.
    ///
    /// It writes `fix_handback_ms` and NOTHING else, which is exactly what the
    /// arc into `fix-wait` writes — so a restart hand-back leaves the entry in
    /// the same shape an ordinary one does, including a `fix_kickback_ms` older
    /// than it, which is what makes [`kickback_owed`](DriveEntry::kickback_owed)
    /// true for the fresh brief. It is a method rather than a field write at the
    /// call site for [`record_worker_pane`](DriveEntry::record_worker_pane)'s
    /// reason: the fact and its one writer stay together.
    pub fn restamp_fix_handback(&mut self, now_ms: u64) {
        self.fix_handback_ms = now_ms;
    }

    /// Record that this drive has resumed its worker into `agent` — the
    /// hand-back's twin of [`open_lane`](DriveEntry::open_lane), and the reason
    /// the assignment is a method rather than a field write at the call site.
    ///
    /// A hand-back that assigned `worker_agent` directly is exactly how #1871 B2
    /// happened: the write looked total and was, and the pane it overwrote went
    /// on running with the drive no longer able to recognise it. Everything a
    /// new pane must do to the old one is here, once.
    pub fn record_worker_pane(&mut self, agent: &str) {
        let mut prior = std::mem::take(&mut self.prior_worker_agents);
        prior.push(std::mem::take(&mut self.worker_agent));
        self.prior_worker_agents = retain_panes(prior, agent);
        self.worker_agent = agent.to_string();
    }

    /// Record the panes this drive was STARTED ON — see
    /// [`founding_panes`](DriveEntry::founding_panes).
    ///
    /// A method rather than a field write at the call site, for
    /// [`record_worker_pane`](DriveEntry::record_worker_pane)'s reason: the fact
    /// and its one writer stay together. Normalised through [`retain_panes`] so
    /// the list carries no empties and no duplicates, and so a pane this drive
    /// has SINCE resumed into is not filed twice — `worker_agent` is passed as
    /// the pane to drop, which is what makes re-recording on a resume
    /// idempotent rather than cumulative.
    ///
    /// Total, not additive: a resume re-reads the session and the answer
    /// replaces the previous one, so a founding pane that has since died does
    /// not survive by having been recorded once.
    pub fn record_founding_panes(&mut self, panes: Vec<String>) {
        self.founding_panes = retain_panes(panes, &self.worker_agent);
    }

    /// Forget every worker pane this drive resumed — the resume-onto-a-different
    /// session case, where those panes belong to a worker it no longer owns.
    pub fn forget_worker_panes(&mut self) {
        self.worker_agent = String::new();
        self.prior_worker_agents.clear();
        self.founding_panes.clear();
    }

    /// Drop superseded panes that are no longer alive, and answer whether
    /// anything was dropped.
    ///
    /// **This is what BOUNDS the superseded lists, and a size cap is what it
    /// replaces.** A cap has to choose a victim, and the only orderings
    /// available to it are by age — which is precisely #1871 B2 again: the
    /// OLDEST superseded pane is a pane that is still running, still on this
    /// session, still able to `report`, and evicting it un-owns it exactly as
    /// the single slot did. A drive resumed enough times would reproduce the
    /// defect this record exists to fix, and it would do so under the usage that
    /// produced B2 in the first place. So nothing is evicted for being old.
    ///
    /// **A dead pane is safe to forget, and provably so rather than plausibly.**
    /// `resolve_token` refuses a caller whose agent is `Dead` and has no entry
    /// for an agent that is gone, so such a pane cannot reach the MCP seam at
    /// all — there is no traffic left for this drive to fail to own. That makes
    /// liveness the one eviction rule that cannot re-open B2, and it is a real
    /// bound rather than an arbitrary number: what is retained is at most the
    /// panes this group can have alive at once, which the live-delegate cap
    /// already limits.
    ///
    /// `is_live` is injected because liveness is the registry's fact and this
    /// crate is Tauri-free, and a predicate that genuinely cannot answer must
    /// answer **true** — "we could not check" is not "it is dead", and the
    /// fail-closed direction here is to KEEP a pane, since keeping one costs a
    /// string and dropping a live one costs the leak.
    ///
    /// **Both live callers answer FALSE for an id the agent map has no row for,
    /// and that is not that case** (#3225). `rd_pane_is_live` reads the map,
    /// which is not a fallible probe: an absent row means `resolve_token` refuses
    /// that caller and no traffic can reach the seam under it, so this list is
    /// retaining a string nothing can ever use. The reading that IS ambiguous —
    /// and stays refused — is `pane_dead`'s and `rd_pane_exit`'s, which decide
    /// whether a drive is WAITING on a pane; this decides only what a bounded
    /// list keeps.
    pub fn forget_dead_panes(&mut self, is_live: &dyn Fn(&str) -> bool) -> bool {
        let before = self.prior_worker_agents.len() + self.founding_panes.len()
            + self.lanes.iter().map(|l| l.prior_agents.len()).sum::<usize>();
        self.prior_worker_agents.retain(|a| is_live(a));
        self.founding_panes.retain(|a| is_live(a));
        for l in self.lanes.iter_mut() {
            l.prior_agents.retain(|a| is_live(a));
        }
        let after = self.prior_worker_agents.len() + self.founding_panes.len()
            + self.lanes.iter().map(|l| l.prior_agents.len()).sum::<usize>();
        before != after
    }

    /// Every pane this drive still owns, superseded ones included
    /// — what an exit owes the orchestrator (#1871 B3), and the same population
    /// [`driven_role`](DriveEntry::driven_role) recognises.
    ///
    /// Kept as a second walk rather than as `driven_role`'s implementation: that
    /// one has to answer **which** pane and whether it is current, and folding
    /// the two would either lose that distinction or make the common
    /// single-lookup path build a vector per incoming `report`.
    ///
    /// Ordered worker-first then lane by lane, each oldest-first, so the list
    /// reads as the history it is. Deduplicated on the way in — see
    /// [`retain_panes`].
    pub fn owned_panes(&self) -> Vec<(String, DrivenRole)> {
        let mut out: Vec<(String, DrivenRole)> = Vec::new();
        for a in self.prior_worker_agents.iter().chain(std::iter::once(&self.worker_agent)) {
            if !a.is_empty() {
                out.push((a.clone(), DrivenRole::Worker));
            }
        }
        for l in &self.lanes {
            for a in l.prior_agents.iter().chain(std::iter::once(&l.agent)) {
                if !a.is_empty() {
                    out.push((a.clone(), DrivenRole::Lane(l.block.clone())));
                }
            }
        }
        out
    }

    /// The drive's age on the wall — what `review_drive_status` reports as
    /// `since_ms`, and **no longer what `drive-stalled` measures**: see
    /// [`bounded_age_ms`](DriveEntry::bounded_age_ms).
    ///
    /// The two are kept apart rather than folded because they answer different
    /// questions and a reader needs both. A human asking "how long has this
    /// drive been going" wants the wall figure — the queue's own `since_ms` is
    /// this, and an age that silently shrank when a cap cleared would be a
    /// worse answer than the one it replaced. A BOUND asking the same question
    /// must not charge the drive for time it was not allowed to act, which is
    /// #2110. `review_drive_status` publishes the wall age and the excluded
    /// total side by side so the difference is visible rather than inferred.
    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.started_ms)
    }

    /// The drive's age with starvation excluded — §2.2's `drive-stalled`
    /// measure since #2110.
    pub fn bounded_age_ms(&self, now_ms: u64) -> u64 {
        self.age_ms(now_ms).saturating_sub(self.starved_ms(now_ms))
    }

    /// Record what a lane's verdict file said this tick, onto that lane.
    ///
    /// **A record of what was READ, never a gate input.** [`LaneRecord::last_verdict`]
    /// says so, and nothing decides from it — the live verdict file is re-read
    /// every tick through the same parser the gate reads. Two things need it
    /// written all the same, and neither is a decision:
    ///
    /// - `review_drive_status()` shows it, and a status view that never showed a
    ///   verdict would be reporting on a drive it could not describe;
    /// - [`at_head`](LaneRecord::at_head) is what distinguishes a lane that has
    ///   **answered** from one that has only been **asked**, which is the whole
    ///   reason §5.2 keeps it apart from `briefed_head`. Without it a re-briefed
    ///   lane looks like a first-time lane forever, and the delta brief §5.5
    ///   exists for — the line an orchestrator typed by hand nine times on one
    ///   PR — is unreachable.
    ///
    /// Returns whether anything changed, so a tick that only observed a
    /// still-unanswered lane does not rewrite the file for it.
    pub fn record_verdict_seen(
        &mut self,
        block: &str,
        verdict: Verdict,
        at_head: &str,
    ) -> bool {
        let Some(rec) = self.lanes.iter_mut().find(|l| l.block == block) else { return false };
        if rec.last_verdict == Some(verdict) && rec.at_head == at_head {
            return false;
        }
        rec.last_verdict = Some(verdict);
        rec.at_head = at_head.to_string();
        true
    }

    /// **Un-record the pane [`releasable`] named, keeping the conversation**
    /// (#2501) — the record half of a release. Answers the pane id it dropped,
    /// or `None` when there was nothing to drop.
    ///
    /// # Why the pane slot is CLEARED rather than marked
    ///
    /// A released pane is dead, and [`forget_dead_panes`](DriveEntry::forget_dead_panes)
    /// already argues at length why a dead pane is safe to forget: `resolve_token`
    /// refuses a caller whose agent is `Dead` and has no entry for one that is
    /// gone, so such a pane cannot reach the MCP seam at all and there is no
    /// traffic left for this drive to fail to own. §7's interception key is
    /// therefore not weakened by dropping it, and three readers get the right
    /// answer for free instead of needing a second field threaded into each:
    /// `owned_panes` stops naming a pane an exit notice would say is "still
    /// running"; `rd_live_lane_pane`'s duplicate refusal stops guarding a pane
    /// that is not there; and `rd_dead_lane_pane` stops reporting a deliberate
    /// release as a lane this drive LOST, which is what `rd-lane-reopened` means
    /// and would have been a false claim on the row a reader chases after
    /// killing an idle delegate.
    ///
    /// It is not pushed onto `prior_agents` either, for the same reason and one
    /// more: that list is bounded by liveness, so the very next
    /// `forget_dead_panes` would drop it — recording it would be a write whose
    /// only effect is to be undone.
    ///
    /// # The conversation, and the one thing that makes this fail closed
    ///
    /// `session` is what [`LaneRecord::session`] should carry afterwards, and it
    /// is passed IN rather than read off the record for #2109's reason (see
    /// [`LaneRecord::reseeded`]): the recorded field is what a SPAWN returned,
    /// which is a session id only on a CLI that pre-assigns one, so a copilot or
    /// opencode lane carries `""` and its conversation lives on the pane and the
    /// roster row instead. The caller resolves all three sources and hands the
    /// answer here, and this **refuses the release outright** when the answer is
    /// empty: dropping the pane of a lane whose session cannot be named would
    /// destroy the conversation this whole mechanism promises to keep. The
    /// worker's session is the entry's own and is refused the same way.
    pub fn release_pane(&mut self, role: &DrivenRole, session: &str) -> Option<String> {
        let session = session.trim();
        match role {
            DrivenRole::Worker => {
                if self.worker_session.trim().is_empty() || self.worker_agent.is_empty() {
                    return None;
                }
                Some(std::mem::take(&mut self.worker_agent))
            }
            DrivenRole::Lane(block) => {
                let rec = self.lanes.iter_mut().find(|l| l.block == *block)?;
                if rec.agent.trim().is_empty() {
                    return None;
                }
                if rec.session.trim().is_empty() {
                    if session.is_empty() {
                        return None;
                    }
                    rec.session = session.to_string();
                }
                Some(std::mem::take(&mut rec.agent))
            }
        }
    }

    /// **Has this lane already been told to stop reviewing `head`?** (#3176.)
    ///
    /// Asked before the stop line is sent, so it arrives once per revision
    /// rather than once per tick. An empty `head` is never "already told": an
    /// unresolved head is not a head, and [`decide`] refuses to act on one at
    /// all one screen up.
    pub fn lane_stopped_at(&self, block: &str, head: &str) -> bool {
        !head.is_empty()
            && self.lane(block).is_some_and(|l| l.stopped_head == head)
    }

    /// Record that this lane has been told to stop reviewing `head` (#3176).
    ///
    /// Written on the delivery SUCCEEDING and never on the intent — the same
    /// rule the release rows follow, and for the same reason: a line that did
    /// not reach the pane is one the next tick still owes. Answers whether the
    /// mark actually moved, so a caller can decide whether the entry needs
    /// storing.
    pub fn mark_lane_stopped(&mut self, block: &str, head: &str) -> bool {
        if head.is_empty() {
            return false;
        }
        match self.lanes.iter_mut().find(|l| l.block == block) {
            Some(rec) if rec.stopped_head != head => {
                rec.stopped_head = head.to_string();
                true
            }
            _ => false,
        }
    }

    /// Release this lane's pane AND forget the revision it was briefed at —
    /// [`ReleaseReason::Conflict`]'s half of the release (#3176).
    ///
    /// **Why this is not [`release_pane`](DriveEntry::release_pane).** That one
    /// takes the pane and leaves `briefed_head`/`briefed_digest` standing, which
    /// is right for a lane that has ANSWERED: `first_stale_lane` skips it, so
    /// nothing ever asks whether it is still open. A lane released with nothing
    /// recorded is the opposite case, and the field it leaves behind is a trap:
    /// `decide_review_wait`'s wait arm is `lane_open_for(rec, head, digest) &&
    /// !lane.pane_dead`, and `pane_dead` is derived from the recorded pane, which
    /// a plain release empties — so the lane reads as *open at this revision, pane
    /// alive*, and the drive waits out `state-stalled` for a verdict no pane can
    /// produce. That is #2163's defect with the pane removed by the driver's own
    /// hand instead of by a human's kill.
    ///
    /// [`LaneRecord::reseeded`] is the existing answer to exactly that question
    /// — it is what a re-drive seeds a fresh entry's lanes with — and it is reused
    /// rather than re-spelled: the pane moves to
    /// [`prior_agents`](LaneRecord::prior_agents) so §7 still intercepts anything
    /// it manages to say on its way out, the revision key and `spawned_ms` are
    /// cleared so the next round is an ordinary fresh brief, and the session is
    /// carried so that brief resumes the same conversation.
    ///
    /// Returns the pane it freed, for the audit row, or `None` when there was no
    /// pane or no session to carry — the same two refusals `release_pane` makes,
    /// and for the same reason: a release that lost the conversation would cost
    /// the review rather than a slot.
    pub fn reseed_lane(&mut self, block: &str, session: &str) -> Option<String> {
        let rec = self.lanes.iter_mut().find(|l| l.block == block)?;
        if rec.agent.trim().is_empty() {
            return None;
        }
        let sess = if rec.session.trim().is_empty() {
            session.trim().to_string()
        } else {
            rec.session.clone()
        };
        if sess.is_empty() {
            return None;
        }
        let freed = rec.agent.clone();
        let next = rec.reseeded(&sess);
        *rec = next;
        Some(freed)
    }

    /// Which side of this drive `agent_id` is, if any — §7's interception key.
    ///
    /// **The key is the agent, never text a delegate typed**, and this method is
    /// where that is true rather than merely intended: it compares against ids
    /// orrerix minted at spawn and recorded here, so a delegate cannot name a PR
    /// number and route its own report to the driver, nor name someone else's
    /// and route theirs.
    ///
    /// **An empty id never matches**, which is what makes an unrecorded pane
    /// fail closed: `""` is what an unresumed worker and a pre-field lane record
    /// both carry, and an empty caller id is not a thing the MCP seam produces
    /// anyway. Without this guard a drive with no hand-back yet would own every
    /// caller whose id failed to resolve.
    pub fn driven_role(&self, agent_id: &str) -> Option<DrivenPane> {
        if agent_id.is_empty() {
            return None;
        }
        if self.worker_agent == agent_id {
            return Some(DrivenPane { role: DrivenRole::Worker, current: true });
        }
        if self.prior_worker_agents.iter().any(|a| a == agent_id) {
            return Some(DrivenPane { role: DrivenRole::Worker, current: false });
        }
        self.lanes.iter().find_map(|l| {
            if l.agent == agent_id {
                Some(DrivenPane { role: DrivenRole::Lane(l.block.clone()), current: true })
            } else if l.prior_agents.iter().any(|a| a == agent_id) {
                Some(DrivenPane { role: DrivenRole::Lane(l.block.clone()), current: false })
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// **The once-per-revision stop mark, pinned where it is decidable** (#3176).
    ///
    /// The integration test asserts the observable property — one
    /// `rd-lane-stopped` row across two ticks — and measured against a mutation
    /// that DELETES the `lane_stopped_at` check it does not discriminate: the
    /// second delivery does not duplicate anyway, for a reason further down the
    /// delivery stack. So the guard's own coverage is here, where the question
    /// is a pure one and every answer is reachable.
    ///
    /// Four properties, and the second and fourth are what stop the first being
    /// satisfiable by a constant: the mark does not stand before it is written,
    /// it DOES once it is, it is keyed on the REVISION rather than on the lane,
    /// and `reseeded` clears it so the lane is tellable again at the rebased
    /// head — which is the whole reason the field can be persisted without
    /// silencing the next round.
    #[test]
    fn the_stop_mark_is_per_revision_and_a_reseed_clears_it() {
        let head_a = "aa11bb22cc33dd44";
        let head_b = "bb22cc33dd44ee55";
        let mut e = entry_at(DriveState::ReviewWait);
        e.open_lane("rev-std", "s1", "rev-1", head_a, Some("d1"), 1_000, false, false);

        // 1. Not marked before anything writes it — the negative control, and
        //    what makes the assertion below about the WRITE rather than about a
        //    predicate that answers `true` for everything.
        assert!(!e.lane_stopped_at("rev-std", head_a), "nothing has told this lane anything yet");

        // 2. The write lands, and says it moved.
        assert!(e.mark_lane_stopped("rev-std", head_a), "the first mark moves the field");
        assert!(e.lane_stopped_at("rev-std", head_a), "…and the predicate now answers for it");
        assert!(
            !e.mark_lane_stopped("rev-std", head_a),
            "…and a second mark at the same revision moves nothing, so a caller can tell whether \
             the entry needs storing"
        );

        // 3. Keyed on the REVISION. A lane told about one head has not been told
        //    about the next, which is what makes the mark safe to persist: the
        //    rebase produces a new head and the lane is tellable again.
        assert!(
            !e.lane_stopped_at("rev-std", head_b),
            "the mark is per-revision — a head-blind mark would silence the lane for the whole \
             drive, including the rebased head this whole feature exists to re-brief at"
        );
        // …and it is keyed on the LANE, so one lane's mark is not another's.
        assert!(!e.lane_stopped_at("rev-final", head_a), "a mark belongs to one lane");
        assert!(
            !e.mark_lane_stopped("rev-final", head_a),
            "…and marking a lane with no record writes nothing at all"
        );

        // 4. An unresolved head is never "already told": §8's posture, and the
        //    same one `decide`'s empty-head guard takes one screen up.
        assert!(!e.lane_stopped_at("rev-std", ""), "an empty head is not a head");
        assert!(!e.mark_lane_stopped("rev-std", ""), "…and is never written as one");
        assert!(
            e.lane_stopped_at("rev-std", head_a),
            "…and that refusal left the real mark alone"
        );

        // 5. `reseeded` clears it with the rest of the per-revision fields.
        let rec = e.lane("rev-std").expect("the lane is on record").reseeded("s1");
        assert_eq!(
            rec.stopped_head, "",
            "a reseeded lane is tellable again — the release path reseeds, so a lane released at \
             a conflicted head must not carry a mark that silences its next round"
        );
    }

    // ── the entry's own guards ──────────────────────────────────────────────

    pub(in crate::reviewdrive) fn entry_at(state: DriveState) -> DriveEntry {
        let mut e = DriveEntry::new(1758, "sess-full", "orch-1", Counters::default(), 1_000);
        // Walk there through legal arcs, so a fixture cannot encode a state the
        // machine refuses to reach.
        match state {
            DriveState::CiWait => {}
            DriveState::ReviewWait => {
                e.advance(DriveState::ReviewWait, None, None, 1_000).unwrap();
            }
            DriveState::FixWait => {
                e.advance(DriveState::FixWait, None, None, 1_000).unwrap();
            }
            DriveState::GateCheck => {
                e.advance(DriveState::ReviewWait, None, None, 1_000).unwrap();
                e.advance(DriveState::GateCheck, None, None, 1_000).unwrap();
            }
            DriveState::Held => {
                e.advance(DriveState::Held, Some(HeldReason::CiLimit), None, 1_000)
                    .unwrap();
            }
            DriveState::Satisfied => {
                e.advance(DriveState::ReviewWait, None, None, 1_000).unwrap();
                e.advance(DriveState::GateCheck, None, None, 1_000).unwrap();
                e.advance(DriveState::Satisfied, None, None, 1_000).unwrap();
            }
            DriveState::Cancelled => {
                e.advance(DriveState::Cancelled, None, None, 1_000).unwrap();
            }
        }
        assert_eq!(e.state(), state);
        e
    }

    #[test]
    fn a_hold_without_a_reason_and_a_reason_without_a_hold_are_both_refused() {
        let mut e = entry_at(DriveState::CiWait);
        // A hold with nothing to put in its notice or its `rd-held` line.
        assert!(e.advance(DriveState::Held, None, None, 2_000).is_err());
        // A reason riding an arc that is not a hold would survive into
        // `review_drive_status()` as a claim about a drive that is not parked.
        assert!(e
            .advance(DriveState::ReviewWait, Some(HeldReason::Escalate), None, 2_000)
            .is_err());
        // Neither attempt moved anything.
        assert_eq!(e.state(), DriveState::CiWait);
        assert_eq!(e.held_reason, None);
    }

    #[test]
    fn resuming_a_parked_drive_clears_the_reason_and_keeps_the_counters() {
        let mut e = entry_at(DriveState::CiWait);
        e.counters.review_rounds = 2;
        e.counters.ci_attempts = 3;
        e.advance(DriveState::Held, Some(HeldReason::CiLimit), None, 2_000)
            .unwrap();
        assert_eq!(e.held_reason, Some(HeldReason::CiLimit));
        // Arc 11 — `drive_review` resumes it. §2.3: the same counters, because
        // a fresh entry would reset them and "yours count too" forbids that.
        e.advance(DriveState::CiWait, None, None, 3_000).unwrap();
        assert_eq!(e.held_reason, None);
        assert_eq!(e.counters.review_rounds, 2);
        assert_eq!(e.counters.ci_attempts, 3);
    }

    #[test]
    fn the_handback_clock_is_stamped_on_fix_wait_arcs_and_nowhere_else() {
        // The whole point of the field's name: it must not become the idle
        // clock §2.2's `drive-stalled` row forbids.
        let mut e = entry_at(DriveState::CiWait);
        assert_eq!(e.fix_handback_ms, 0);
        e.advance(DriveState::ReviewWait, None, None, 5_000).unwrap();
        assert_eq!(e.fix_handback_ms, 0, "a non-fix-wait arc must not stamp it");
        e.advance(DriveState::FixWait, None, None, 7_000).unwrap();
        assert_eq!(e.fix_handback_ms, 7_000);
        e.advance(DriveState::CiWait, None, None, 9_000).unwrap();
        assert_eq!(e.fix_handback_ms, 7_000, "leaving fix-wait must not re-stamp");
        // ...and the age anchor is untouched by every one of those advances.
        assert_eq!(e.started_ms, 1_000);
        assert_eq!(e.age_ms(9_000), 8_000);
    }

    #[test]
    fn re_briefing_a_lane_re_arms_its_stall_clock() {
        let mut e = entry_at(DriveState::ReviewWait);
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("d1"), 1_000, false, false);
        assert_eq!(e.lanes.len(), 1);
        e.open_lane("rev-std", "s1", "rev-1", "head-b", Some("d1"), 9_000, false, false);
        // Replaced, not appended: a second record would leave `lane()` reading
        // the first and measuring `lane-stalled` from the original spawn.
        assert_eq!(e.lanes.len(), 1);
        let rec = e.lane("rev-std").unwrap();
        assert_eq!(rec.spawned_ms, 9_000);
        assert_eq!(rec.briefed_head, "head-b");
        assert_eq!(rec.briefed_digest, "d1");
    }
}
