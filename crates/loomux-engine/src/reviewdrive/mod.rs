//! The engine-driven review-loop driver: the pure core (#1778 S1).
//!
//! Design note: `docs/design/review-driver.md`. That note is the spec, and this
//! module is the half of it that has no I/O in it at all — the state machine
//! (§2.1), the persisted shape (§5.2), the counters (§2.3) and the decision
//! `rd_driver_tick` makes once its facts are in hand (§2.4). The tick itself,
//! the `gh` reads behind those facts, the spawns, the notices and the audit
//! lines are S3's, in `src-tauri`; the MCP tools (§5.1) are S4's; the `driver:`
//! block (§5.3) is S2's, in `workflow.rs`.
//!
//! **Why the split is drawn here and not somewhere more convenient.** Every
//! decision this feature makes is a function of facts orrerix read a moment
//! earlier — the PR's head, its checks, a verdict file, a clock. Putting the
//! decision in the same function as the reads makes it testable only through a
//! fake `gh`, which is how a state machine ends up pinned by its plumbing. So
//! [`decide`] takes the facts as an argument, reads no clock, spawns no child
//! and touches no file, and the whole of §2.1's table is exercised by building
//! a [`DriveFacts`] and asserting a [`DriveStep`].
//!
//! # What this module is NOT
//!
//! It is not a second reader of the merge gate. §4 of the note is explicit that
//! a third implementation of the gate decision is a defect rather than an
//! optimization, so the gate's answer, the routed lane list and each lane's
//! verdict arrive here as **facts the existing parsers produced** —
//! [`ReviewVerdict`] itself is what a [`LaneFact`] carries, and the staleness
//! questions are asked with that type's own [`ReviewVerdict::reviewed`] and
//! [`ReviewVerdict::body_changed`] rather than with a comparison written here.
//!
//! # Where this module knowingly goes beyond the note
//!
//! Three persisted fields exist that §5.2's example entry does not show, and
//! they are called out here rather than left for a reader to notice as drift.
//! §2.2 bounds two waits — `held(lane-stalled)` is "no verdict inside
//! `lane_timeout_minutes`", `held(fix-stalled)` was "neither pushed nor reported
//! inside `fix_timeout_minutes`" until #2168 E1 gave it a second site with its own
//! anchor — and the shape in §5.2 carries no timestamp
//! from which either could be measured, while §2.4 resumes a drive from disk
//! across a restart, so an in-memory clock cannot carry them either. A bound
//! with no anchor is not a bound. So:
//!
//! - [`LaneRecord::spawned_ms`] — when this lane's delegate was last spawned or
//!   resumed. The `lane-stalled` anchor.
//! - [`LaneRecord::briefed_head`] and [`LaneRecord::briefed_digest`] — the
//!   revision that lane was last briefed at, as **one key**, the same
//!   `(head, digest)` the gate binds a verdict to. §2.1 re-briefs a lane whose
//!   `pass` went stale, and without this the driver cannot tell a lane already
//!   re-opened at the live revision (wait for it) from one that still needs
//!   re-opening (brief it) — it would re-brief every tick. The head alone is
//!   not the key; [`lane_open_for`] carries why, and the defect it names is one
//!   this slice actually shipped and CI caught.
//! - [`DriveEntry::fix_handback_ms`] — when the drive last entered `fix-wait`.
//!   The `fix-stalled` anchor **in `fix-wait`**; the hand-back is the moment
//!   that wait began. #2168 E1 gives that hold a second site, in `ci-wait`,
//!   whose wait began at the PUSH rather than at the hand-back and so is
//!   measured from `state_since_ms` — see [`decide_fix_receipts`].
//! - [`DriveEntry::fix_kickback_ms`] — when the drive last answered a worker's
//!   `report(progress)` in that worker's own pane (#1959). Not a timeout
//!   anchor: it is compared against `fix_handback_ms`, which makes the budget
//!   one answer per hand-back and renews it with no reset to remember.
//! - [`DriveEntry::fix_pushed_ms`] — when the worker last pushed onto a head
//!   this drive handed back for; `None` unless `ci-wait` was entered by arc 7
//!   (#2168 E1). The `fix-stalled` anchor in THAT state, and the one clock here
//!   that a non-arc also writes: `note_fix_push` re-stamps it when a further
//!   push lands mid-wait, which is exactly what `state_since_ms` cannot do,
//!   since `transition` refuses a `ci-wait` -> `ci-wait` self-arc and so leaves
//!   the state clock on the FIRST push.
//!
//! `drive-stalled` needs none of these: it is the drive's **age**,
//! `now - started_ms`, so it keeps §5.2's own `started_ms`.
//!
//! **§2.2 used to forbid a general "when did the state last change" stamp, and
//! #2110 adds exactly that field on purpose** —
//! [`DriveEntry::state_since_ms`], written on every arc. The ban was never
//! about the stamp; it was about a drive whose ONLY bound is one, and its
//! worked example is §8's `also: [base-green]` row: that drive advances
//! `gate-check` → `ci-wait` on every wake, resets any per-state clock forever,
//! and would sit on a red default branch in silence. So the age is kept, as the
//! backstop that cycler falls through to, and the per-state clocks are added
//! above it. Both, not either.
//!
//! What forced the addition is the other half of the same question. Two drives
//! were parked `drive-stalled` at four hours, and neither was stalled: one was
//! mid-round with CI green at a new head, the other had spent three of those
//! hours unable to spawn a lane at all because another drive held every slot.
//! An age cannot tell those from paralysis, because every drive's age grows at
//! the same rate whatever it is doing — and the reason the *first* clock in
//! this struct was an age is that a bound with no anchor is not a bound, not
//! that an age was the right measure. See [`state_bound_ms`], and
//! [`DriveEntry::starved_total_ms`] for the time both clocks now exclude.
//!
//! - [`DriveEntry::state_since_ms`] — the `state-stalled` anchor.
//! - [`DriveEntry::starved_total_ms`], [`DriveEntry::starved_state_ms`] — what
//!   the drive spent unable to spawn, which neither clock charges it for.
//! - [`DriveEntry::held_from`], [`DriveEntry::held_after_ms`] — what the drive
//!   was doing when a bound fired, so a resume is a decision rather than a
//!   reflex.
//!
//! # One seam here is not a contract
//!
//! [`decide`] and the fact types it consumes are a **slice author's seam**, not
//! a §-backed public contract: the note specifies the states, the arcs, the
//! counters and the file, and says nothing about how the tick's decision is
//! factored out of its reads. Do not go looking for the section — there isn't
//! one. What that means for a later change is that this shape may be reworked
//! on its own merits, where [`DriveState`], [`transition`] and
//! [`ReviewDrivesState`] may not: those three are the note's, and changing one
//! changes `docs/design/review-driver.md` first.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::workflow::{BlockId, ReviewVerdict, Verdict, DRIVER_DRIVE_TIMEOUT_DEFAULT_MIN};

mod states;
mod counters;
mod store;
mod bounds;

pub use states::*;
pub use counters::*;
pub use store::*;
pub use bounds::*;

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

// ── §2.4 the decision, over injected facts ──────────────────────────────────

/// What one observation of a driven PR's checks and mergeability concluded.
///
/// A closed vocabulary rather than `mqdriver`'s own result types, and that is
/// the seam: S3 maps `mqdriver::resolve_pr_detailed` /
/// `notify::pr_mergeability_result` and `mqdriver::pr_ci_green_detailed` /
/// `notify::pr_checks_result` onto these five, and this module never learns
/// what a `gh` invocation looks like.
///
/// Note what is **not** here: no "assume green", and no arm that folds an
/// unanswered lookup in with an answered one. §8 is emphatic on that — a
/// rate-limited `gh` returns *promptly* with a non-zero exit, so it is not a
/// runner failure, but `BaseUnverifiable` is still an **unknown** rather than a
/// fact about the PR, and "unknown is never treated as safe".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiObservation {
    /// Checks are still running, or none has reported yet — which
    /// `notify::pr_checks_result` already reads as pending rather than green.
    Pending,
    /// Every required check terminal, none failing.
    Green,
    /// Terminal with failures.
    Red,
    /// GitHub reports the PR `CONFLICTING`: there is no clean merge ref, so no
    /// check suite will ever exist for it. Never discoverable by waiting.
    Conflicting,
    /// orrerix could not tell. Back off; no transition, no notice, bounded by
    /// `drive_timeout_minutes`.
    Unknown,
}

/// What the driven worker did since the hand-back (§2.1's `fix-wait` row).
///
/// Sourced from the **intercepted** `report` (§7), which is keyed on the
/// calling agent and never on a `ref` string a delegate typed — a delegate that
/// could choose whether its report reaches the orchestrator by naming a PR
/// number is a delegate that can route around the orchestrator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerSignal {
    // **Only the WORKER produces one of these**, and the name is the contract
    // rather than a label: arc 8 is "`report(done)` with the head unchanged" and
    // `held(worker-blocked)` names a worker's session, so feeding a reviewer
    // lane's report in here moves the drive on a hand-back that never happened.
    // Both sides reach the `report` MCP arm and a reviewer's `approved` resolves
    // to the same `done` word, so the arm decides on `DrivenRole` — the role is
    // what makes this type's name true (§7).
    /// Nothing yet.
    Silent,
    /// `report(done)`.
    Done,
    /// `report(blocked)`.
    Blocked,
    /// The fix could not be handed back to this drive's worker — see
    /// [`HeldReason::WorkerUnresumable`] for the causes this covers.
    ///
    /// **This is not a drive-time check, and the note is honest about why.**
    /// §5.1: a full, well-shaped session id this group never recorded takes
    /// `resolve_session_ref`'s `is_full_session_id` passthrough arm and is
    /// *accepted* by `drive_review`, so its unresumability surfaces here, at
    /// the first hand-back, possibly hours on. Resolving is not the same as
    /// proving resumable, and v1 does not prove it.
    ///
    /// **It is also produced AFTER a hand-back that succeeded** (#1961): the
    /// registry raises it when the pane the drive resumed exits in `fix-wait`
    /// with nothing reported, which is a resume that "worked" and then died on
    /// `Invalid session ID`. Before that the drive waited a full fix timeout on
    /// a process that was already gone.
    Unresumable,
}

/// What re-reading the gate concluded (§2.1's `gate-check` row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateOutcome {
    /// The tick did not evaluate the gate. The gate is read in `gate-check` and
    /// nowhere else, so every other state passes this.
    NotEvaluated,
    /// `evaluate_merge_gate` satisfied at the live head, including every
    /// declared `also:` condition.
    Satisfied,
    /// Not satisfied, for any reason.
    Unsatisfied,
    /// The gate file is present and could not be read (an I/O error). **Not**
    /// `gate-not-configured`, which means the file is genuinely absent and is a
    /// drive-time decline (§5.1) rather than a hold.
    Unreadable,
}

/// One lane's live reading: the block the gate named, the verdict file as
/// `workflow::parse_verdict_file` returned it (or `None` for no verdict yet),
/// and whether the pane this drive recorded for that lane is GONE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneFact {
    pub block: BlockId,
    pub verdict: Option<ReviewVerdict>,
    /// **The recorded pane is positively [`AgentStatus::Dead`]** (#2163) — the
    /// lane-side twin of the `worker_exit` observation `fix-wait` has had since
    /// #1961, and read for the same reason: `review-wait` waits for a verdict
    /// that a dead pane can never produce.
    ///
    /// Before this the exit was observed only for the worker and only in
    /// `fix-wait` — this module's own comment said so, and gave the reason as
    /// "`review-wait` has `lane-stalled` for its own panes". That bound is
    /// `lane_timeout_minutes` (60 at stock knobs) measured from the brief, so a
    /// reviewer pane killed at minute twelve left the drive silent for
    /// forty-eight more with no rd-* row at all — measured on PR #2140, and
    /// reached on the driver's OWN advice, since a `cap-refused` notice tells
    /// an orchestrator to kill an idle delegate and a lane that has finished
    /// its turn is on that list.
    ///
    /// **`false` is "we could not check" as well as "it is alive"**, and the
    /// fail direction is deliberate: the same asymmetry
    /// [`DriveEntry::forget_dead_panes`] states, resolved the same way
    /// `rd_pane_exit` resolves it — only a positive `Dead` counts, so an
    /// emptied agent map (a restart) re-opens nothing.
    ///
    /// A lane with no record, or one whose record carries no pane, is `false`:
    /// there is no pane to be dead, and the lane is opened by the ordinary path.
    pub pane_dead: bool,
}

/// Everything [`decide`] is allowed to know. Read by the tick, immediately
/// before the call; nothing in here is read from the entry, and nothing is
/// fetched inside the decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveFacts {
    /// The clock, injected. [`decide`] never reads one.
    pub now_ms: u64,
    /// Whether the PR is open. `Some(false)` **only on a positive answer**:
    /// `mqloop::draft_pr_open` returns `None` for a lookup it could not
    /// complete, and its doc says reconcile treats that as "the world does not
    /// match", never as "probably fine". Cancelling a live drive on a rate
    /// limit that clears in minutes is the failure this distinction stops.
    pub pr_open: Option<bool>,
    /// The PR's live head.
    pub head: String,
    /// The PR body's digest as it stands, or `None` when the body could not be
    /// read — the same fact `body_drift` takes in rather than fetching (#791).
    pub body_digest: Option<String>,
    /// The lanes the gate requires **at this head**, in
    /// `RoutingDecision::required`'s order. `None` is `route_reviewers`
    /// returning `None`: the changed-file list could not be shown complete, so
    /// which reviewers are required is unknown.
    pub required_lanes: Option<Vec<LaneFact>>,
    pub ci: CiObservation,
    pub worker: WorkerSignal,
    pub gate: GateOutcome,
    /// A driven delegate called `message_orchestrator` since the last tick
    /// (§7). Its own line was delivered unchanged, by its own arm; this is the
    /// routing fact beside it.
    pub messaged: bool,
    /// **A pane this drive owns is stopped on a provider's spend/usage
    /// refusal** (#2811 S5b) — the [`crate::providerlimit`] provider id, or
    /// `None` when no owned pane is showing one.
    ///
    /// A DRIVE-level fact rather than a per-[`LaneFact`] one, and that is a
    /// departure from plan-2504's wording ("`LaneFact::provider_limited`")
    /// argued in `docs/design/review-driver.md`: a drive in `fix-wait` owns a
    /// WORKER pane and no open lane at all, and that is precisely a drive the
    /// hold must cover. A per-lane field cannot see it. The tick unions every
    /// pane the drive owns — lanes and worker alike — and reports the provider,
    /// which is also the key the notice is aggregated on.
    ///
    /// **`None` is "no owned pane is showing one", including "we could not
    /// look"**, and the fail direction is the same one `pr_open` takes: a drive
    /// is never held on a reading orrerix could not complete. The cost of a
    /// missed detection is the pre-#2811 sixty-minute lane stall; the cost of a
    /// false one is N drives parked on a provider that is fine.
    pub provider_limited: Option<String>,
    /// **This process restarted while the drive was in `fix-wait`, and the
    /// worker pane it was waiting on died with the previous one** (#2811 S10).
    ///
    /// Set by the restart reconcile and by nothing else, because the reconcile
    /// is the only place the fact is KNOWN. A pane missing from the agent map
    /// mid-session is the ambiguous reading [`LaneFact::pane_dead`] declines to
    /// make; a pane missing on the first tick after a restart is not ambiguous
    /// at all, because every pane dies with the process. So the reconcile marks
    /// the entry and the decision below reads the mark, rather than either of
    /// them re-deriving "is this pane gone" from a map that cannot tell the two
    /// apart.
    ///
    /// It is a fact about the PROCESS, not about the session: whether the
    /// recorded worker session can actually be resumed is what the hand-back
    /// itself discovers, and a hand-back that cannot reach it still lands on
    /// `held(worker-unresumable)` by the ordinary path.
    pub restart_handback: bool,
    /// **This process restarted while the drive was in `ci-wait` waiting on the
    /// receipts for a push the worker had already made, and that worker's pane
    /// died with the previous process** (#3225).
    ///
    /// The sibling of [`restart_handback`](DriveFacts::restart_handback) one
    /// state later, and set by the same two places for the same reason: a pane
    /// absent from the roster is only unambiguously GONE where every pane is —
    /// the restart reconcile, and a `drive_review` an orchestrator issued
    /// against a live drive.
    ///
    /// What it licenses is narrow. [`decide_fix_receipts`] waits for the
    /// pushing worker's `report(done)` before briefing a lane, because green is
    /// not the end of the round — the receipts still have to reach the body.
    /// That report can never arrive from a pane that no longer exists, so the
    /// wait runs out `fix_timeout_minutes` and parks `held(fix-stalled)`: a
    /// claim that a worker went silent, about a worker orrerix was restarted
    /// under. The push itself is durable and is already at the head CI just went
    /// green on, so the mark says to treat that push as the fix delivered and
    /// brief the lane at that head.
    ///
    /// **What it does not do is charge anything.** A restart is not a round —
    /// [`DriveStep::Rehandback`]'s argument, applied to the arc this one takes.
    pub restart_push_delivered: bool,
}

/// What the tick should do with one entry, this tick.
///
/// **At most one state advance per entry per tick** (§2.4) is a property of
/// this type, not a discipline the caller has to remember: there is exactly one
/// advancing variant and it names one arc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriveStep {
    /// Nothing to do. The tick backs off per §2.4's principle — back off after
    /// any tick whose next attempt would make the same external calls and reach
    /// the same answer.
    Wait,
    /// Stay in `review-wait` and brief lane `index` — a spawn, or a resume of
    /// the lane's recorded session. **Not a transition** (§2.1): advancing to
    /// lane *k+1* leaves the entry where it is and writes the lane index, which
    /// is why the table has no `review-wait -> review-wait` arm.
    ///
    /// `verify` is #2168 E2: this brief is a **body-verification delta** — every
    /// required lane has passed the code at this head and only the PR body has
    /// moved, so what this lane is asked for is the body as it stands. It
    /// travels on the step rather than being re-derived at the write because it
    /// becomes a grant on the recorded verdict, and a grant derived twice is a
    /// grant that can disagree with itself.
    ///
    /// `body_only` is #2509, and travels for the same reason: this brief is
    /// about the BODY of a revision whose CODE this drive has already reviewed
    /// to completion — the head has not moved, this lane was already briefed at
    /// it, and every required lane has ANSWERED at it. It is strictly weaker
    /// than `verify`, which it nests inside (`verify` needs every lane to have
    /// PASSED), and the gap between the two is the case #2509 exists for: a
    /// lane that recorded `fail` on the body. It becomes
    /// [`LaneRecord::briefed_body_only`], which is the ONLY thing the one-shot
    /// grace below is granted on.
    OpenLane { index: usize, verify: bool, body_only: bool },
    /// Take one arc, spending `bump` first when there is one.
    Advance {
        to: DriveState,
        held_reason: Option<HeldReason>,
        bump: Option<Counter>,
    },
    /// Hand the worker back again from `fix-wait`, **taking no arc and
    /// spending no counter** (#2811 S10).
    ///
    /// The drive is already in `fix-wait` and stays there: the previous
    /// process handed this worker back, and only the PANE was lost. Re-briefing
    /// the recorded session is the cheapest thing that makes the drive live
    /// again, and it is not a new round — nothing about the review or the CI
    /// changed, so charging `review_rounds` for it would bill INVARIANT 9 for a
    /// restart. That is why this is a variant of its own rather than an
    /// `Advance` into the state the entry is already in: `advance` pairs an arc
    /// with its cost by construction, and there is no arc here to pair.
    ///
    /// The tick re-stamps [`DriveEntry::restamp_fix_handback`] when the
    /// hand-back reaches the worker, so `held(fix-stalled)` is measured from
    /// the brief the worker can actually answer rather than from one delivered
    /// to a pane that no longer exists.
    Rehandback,
}

impl DriveStep {
    fn to(to: DriveState) -> DriveStep {
        DriveStep::Advance { to, held_reason: None, bump: None }
    }

    fn held(reason: HeldReason) -> DriveStep {
        DriveStep::Advance {
            to: DriveState::Held,
            held_reason: Some(reason),
            bump: None,
        }
    }

    fn spend(to: DriveState, bump: Counter) -> DriveStep {
        DriveStep::Advance { to, held_reason: None, bump: Some(bump) }
    }
}

/// Whether a lane's recorded verdict was recorded **about the revision now on
/// the PR** — the `(head, digest)` key, asked of the verdict WORD-BLIND.
///
/// §2.1's first carried-over property is written for a `pass` — *"a `pass` bound
/// to an old head, or to an old body digest, is not a pass"* — and the binding
/// rule underneath it is not about `pass` at all: **a verdict is bound to the
/// revision it reviewed**, so once the head moves the lane owes a fresh one
/// whatever word it last said. Reading the currency test only on the `pass` side
/// is what let a `fail` recorded three commits ago stay authoritative, and
/// [`decide_review_wait`] re-route it as though a reviewer had just spoken
/// (#1871 B1).
///
/// Asked with [`ReviewVerdict`]'s own methods rather than with comparisons
/// written here, because §4 makes the driver a reader of the gate and never a
/// third implementation of it. Both halves of the #565 asymmetry come along for
/// free that way: [`ReviewVerdict::reviewed`] is false for an empty head, so an
/// unbound verdict reads as stale and fails closed; and
/// [`ReviewVerdict::body_changed`] answers `None` when the drift cannot be
/// *known* — a verdict with no digest, or a body that could not be read — which
/// is not `Some(true)` and so does not stale the verdict. "We could not check"
/// and "it changed" are different answers, and only one of them may re-open a
/// lane.
pub fn lane_verdict_is_current(
    verdict: Option<&ReviewVerdict>,
    head: &str,
    body_digest: Option<&str>,
) -> bool {
    let Some(v) = verdict else { return false };
    v.reviewed(head) && v.body_changed(body_digest) != Some(true)
}

/// Whether a lane's recorded verdict is a `pass` that still counts at this
/// `(head, digest)` — the currency rule above, plus the word.
///
/// Kept as its own function because [`first_stale_lane`] asks a different
/// question from [`decide_review_wait`]'s match: "has this lane's pass settled
/// the revision in front of us" versus "does this lane have anything to say
/// about it". A stale `fail` answers **no** to the first and **no** to the
/// second, and before #1871 only the first was asked.
pub fn lane_pass_is_current(
    verdict: Option<&ReviewVerdict>,
    head: &str,
    body_digest: Option<&str>,
) -> bool {
    verdict.is_some_and(|v| v.verdict == Verdict::Pass)
        && lane_verdict_is_current(verdict, head, body_digest)
}

/// Whether a lane's `pass` still **settles** this revision — the currency rule
/// above, or the #2168 E2 delegation: a required lane recorded a
/// body-verification pass covering the body as it stands, and this pass is
/// bound to the same head.
///
/// **`verified` is passed in rather than computed here** because it is a
/// property of the whole required set, not of one lane, and re-deriving it per
/// lane would make the answer depend on iteration order. [`first_stale_lane`]
/// computes it once.
///
/// The delegation is asked through the gate's own
/// [`ReviewVerdict::pass_covers_body`], not re-implemented, for §4's reason: a
/// driver that decided "this lane is settled" on a rule the gate does not share
/// would drive to `gate-check` and be refused there for ever.
///
/// **That claim is scoped to the DELEGATION arm, and the scoping is not
/// pedantry** (#2308 review 1, premortem 2). The first arm is deliberately NOT
/// the gate's rule: [`lane_pass_is_current`] reads an unknown digest — the
/// verdict carries none, or the body could not be read now — as *not drift*,
/// because one transient `gh` failure must not re-brief every open lane in the
/// group, while the gate refuses an empty digest outright (unknown may never
/// discharge a merge condition). So on a gate declaring `body-unchanged`, a pass
/// recorded during a body-read outage settles here and is refused there, and the
/// drive cycles `gate-check -> ci-wait -> review-wait` on unchanged facts.
///
/// **Pre-existing, bounded, and deliberately not closed here.** That divergence
/// predates #2168 E2 — it is the #565 asymmetry meeting the #791 one, and both
/// arms are right about their own question — and it is bounded by
/// `drive_timeout_minutes` into `held(drive-stalled)`, which is the same exit
/// §8's `also: [base-green]` row parks on. Closing it means deciding which
/// asymmetry yields, which is a change to the gate's contract rather than to
/// this function, and it wants its own slice. What this slice DOES pin is that
/// the two sides agree about the verification question itself, over every
/// crossing of it: `the_driver_and_the_gate_answer_the_verification_question_identically`.
fn lane_pass_settles(
    verdict: Option<&ReviewVerdict>,
    head: &str,
    body_digest: Option<&str>,
    verified: bool,
) -> bool {
    if lane_pass_is_current(verdict, head, body_digest) {
        return true;
    }
    verdict.is_some_and(|v| v.pass_covers_body(head, body_digest, verified))
}

/// Whether this lane is open for exactly the revision now on the PR: it was
/// briefed at this head **and** at this body digest.
///
/// **Both signals, read by one rule, in one place.** The failure this exists to
/// prevent is the repo's "a guard reads every one of its inputs by one rule"
/// class, and it is not hypothetical — the head-only version of this comparison
/// shipped in the first push of this slice and was caught by
/// `a_pass_whose_body_digest_moved_re_opens_that_lane`. A lane that already
/// answered `pass` at this head and whose body then moved reads, under a
/// head-only key, exactly like a lane still thinking: the driver waits on a
/// reviewer that has already spoken, and `lane-stalled` eventually reports a
/// stall that never happened. §8's body-changed row wants that lane re-briefed
/// with a body-only delta.
///
/// **An unknown live digest does not mismatch**, and neither does an unrecorded
/// briefed one. "We could not check" is not "it changed" — the same asymmetry
/// [`ReviewVerdict::body_changed`] encodes by answering `None` rather than
/// `Some(false)` — and the alternative is that one transient `gh` failure to
/// read a PR body re-briefs every open lane in the group.
pub fn lane_open_for(rec: &LaneRecord, head: &str, body_digest: Option<&str>) -> bool {
    if rec.briefed_head != head {
        return false;
    }
    match body_digest {
        Some(now) if !now.is_empty() && !rec.briefed_digest.is_empty() => {
            rec.briefed_digest == now
        }
        _ => true,
    }
}

/// The `lane-stalled` anchor [`DriveEntry::open_lane`] must be given for the
/// brief about to be sent — `now_ms` for an ordinary one, and the anchor
/// already on the record when this brief is only REPLACING a dead pane (#2163).
///
/// **The re-open must not re-arm the clock, and that is what bounds the
/// re-open.** `decide_review_wait` re-opens a lane whose recorded pane is
/// `Dead`, so a pane that dies on every spawn would be replaced on every tick
/// for as long as the drive lives if each replacement started the silence
/// timer over. Preserving the anchor makes `lane-stalled` reachable through the
/// loop: the lane parks at `lane_timeout_minutes` from the ORIGINAL brief,
/// naming itself, and the notice's own remedy (read that pane) is the right one.
///
/// **It is also the more honest reading of the field.** `spawned_ms` anchors a
/// SILENCE test — `decide_review_wait`'s stall arm exempts a lane that has
/// answered — and a pane dying and being replaced does not make a lane less
/// silent about the head it was asked about. Re-arming would be the
/// defeat-your-own-bound shape `decide`'s empty-head guard describes.
///
/// **Only for a replacement at the SAME REVISION, and the revision is the full
/// `(head, digest)` key** — [`lane_open_for`], the one place that comparison is
/// written. A lane with no record, or one whose recorded pane is alive (an
/// ordinary re-brief, a reuse fall-through), gets `now_ms` too: nothing about
/// those is a replacement.
///
/// **The digest half is not decoration, and keying on the head alone was an
/// asymmetry** (#2169 review 2, N1). This module treats `(head, digest)` as ONE
/// revision key — §5.2 says so in as many words, `lane_open_for` implements it,
/// and a verdict binds to both — so a head-only test here answered "same round"
/// for a case the rest of the module calls a new one: a lane briefed at
/// `(H, d1)` whose pane dies, followed by a BODY-ONLY fix moving the digest to
/// `d2`, re-opens with a reviewer that has read nothing and inherits the
/// original anchor. Fifty minutes in, that fresh reviewer had nine minutes
/// before `held(lane-stalled)` — while the *same* body-only fix under a LIVE
/// idle pane re-arms to `now` and gets the full window, which is the same
/// revision getting two different clocks depending on whether a pane happened
/// to die.
///
/// **And the bound does not need the digest case.** The runaway this exists to
/// stop is a pane that dies on every spawn, which loops on the tick's own clock
/// with nothing else moving; a digest only moves when a human or a worker edits
/// the PR body, so it cannot drive a loop. Widening the exception to a new
/// revision therefore costs the bound nothing.
///
/// An **unknown** live digest does not re-arm: `lane_open_for` reads "we could
/// not check" as still-open rather than as drift, which is the same asymmetry
/// [`ReviewVerdict::body_changed`] encodes and the fail-safe direction here —
/// one transient `gh` failure to read a PR body must not hand every dying lane
/// a fresh hour.
///
/// A recorded anchor of `0` is a pre-field row, which is "unset" rather than
/// "the epoch"; it takes `now_ms` for the same reason `fix_handback_ms == 0`
/// means ancient rather than unset elsewhere in this module.
pub fn lane_stall_anchor(
    rec: Option<&LaneRecord>,
    head: &str,
    body_digest: Option<&str>,
    pane_dead: bool,
    now_ms: u64,
) -> u64 {
    match rec {
        Some(r)
            if pane_dead
                && !head.is_empty()
                && lane_open_for(r, head, body_digest)
                && r.spawned_ms != 0 =>
        {
            r.spawned_ms
        }
        _ => now_ms,
    }
}

/// Whether one of the required lanes has recorded a **body-verification pass**
/// covering the body as it stands (#2168 E2) — the driver's read of
/// [`mergeq::body_verified_by_required`](crate::mergeq::body_verified_by_required),
/// over the same routed reviewer list the gate would use.
///
/// One definition, asked through [`crate::workflow::body_verified`], because a
/// driver that answered this differently from the gate would advance to
/// `gate-check` and be refused there on every tick until `state-stalled`.
pub fn body_is_verified(required: &[LaneFact], head: &str, body_digest: Option<&str>) -> bool {
    crate::workflow::body_verified(
        required.iter().filter_map(|l| l.verdict.as_ref()),
        head,
        body_digest,
    )
}

/// The first lane whose `pass` does not stand at this (head, digest) — where
/// arc 8 re-enters after a body-only fix (§8 row 5), and equally where
/// `review-wait` resumes when a digest moves under a recorded pass.
///
/// Returns `required.len()` when every lane's pass stands, which is the
/// "nothing left to review" answer arc 4 acts on.
///
/// **Since #2168 E2 a body-only move re-briefs ONE lane, not all of them.**
/// Before the verification pass exists, every lane's pass is stale at the new
/// digest and this returns the FIRST of them — which is the lane the gate's own
/// `reviewers:` order puts first, and the order the driver already briefs in.
/// Once that lane answers with a verification pass,
/// [`body_is_verified`] is true and every other lane's pass at the head it was
/// bound to settles again, so this returns `required.len()` and the drive goes
/// to `gate-check` rather than walking the rest of the list. That walk is the
/// re-record cascade #2168 measured; the argument for accepting the delegation
/// is on `mergeq::body_unchanged`.
///
/// **Which lane is "first" is the gate file's decision, not this module's.**
/// `required` arrives in `RoutingDecision::required` order, which is the
/// `reviewers:` list a repo wrote plus the routed additions — so a repo says
/// which lane it wants asked first by writing it first, and orrerix never
/// forms an opinion about what a lane costs (CLAUDE.md constraint 8).
pub fn first_stale_lane(required: &[LaneFact], head: &str, body_digest: Option<&str>) -> usize {
    let verified = body_is_verified(required, head, body_digest);
    required
        .iter()
        .position(|l| !lane_pass_settles(l.verdict.as_ref(), head, body_digest, verified))
        .unwrap_or(required.len())
}

/// **The decision.** One entry, one tick, one step — pure, and that purity is
/// what makes S3's integration testable: no I/O, no `gh`, no clock read, no
/// file. Everything it knows is in `facts`.
///
/// # The order the conditions are asked in
///
/// Precedence is a decision the note does not spell out arc by arc, so it is
/// argued here rather than left to the reading order of a `match`:
///
/// 1. **A terminal or parked entry yields [`DriveStep::Wait`] immediately.**
///    §2.1's `held` row reads "nothing; the tick does not advance it" — a
///    parked drive is left for `drive_review` or `cancel_review_drive`, and if
///    the tick could move one, `held` would not be a park.
/// 2. **A positively-closed PR cancels**, before anything else can hold it.
///    §8: `cancelled`, and only on a positive answer. A PR that is gone has no
///    hold worth reporting, and `cancelled` says more than `drive-stalled`.
/// 3. **`messaged` holds next.** The delegate's own words are already in the
///    orchestrator's pane (§7 — `message_orchestrator` is never intercepted),
///    so this hold is the routing fact that explains them. Holding for anything
///    else here would leave that message beside a hold that does not account
///    for it.
/// 4. **The drive's age holds next, and it must outrank the per-state logic**
///    — this is the one ordering §8 forces rather than merely permits. Its
///    `also: [base-green]` row parks a drive on a red default branch by cycling
///    `gate-check` -> `ci-wait` on every wake; that drive *always* has an
///    advance available, so an age check that ran after the per-state logic
///    would never be reached and the drive would cycle forever. The bound is
///    what keeps stopping the line from being silent. Since #2110 it is the
///    BACKSTOP rather than the working bound, and the reason it still outranks
///    everything below is unchanged: it is the one bound the base-green cycler
///    cannot reset.
/// 5. **Then time in the current state** (#2110) — [`state_bound_ms`], reset by
///    every transition, which is what an orchestrator actually wants bounded.
///    Below the age for the reason argued at the line itself: both are past
///    only for a drive resumed out of a very long park, where the twelve-hour
///    figure is the more important of the two.
/// 6. **A CONFLICTING PR takes arc 3, from every state that can leave for
///    `fix-wait`** (#2311) — argued at the line itself. It is a fact about the
///    PR rather than about the wait, so it outranks each state's own reading:
///    at `gate-check` the gate's own `satisfied`, and at `review-wait` the
///    routing question, which cannot be answered for a conflicted head at all
///    (no changed-file list) and therefore reports a CONSEQUENCE of the
///    conflict as if it were an independent fact. Below the bounds above,
///    which are about the drive rather than the PR.
/// 7. Then the state's own logic.
pub fn decide(entry: &DriveEntry, facts: &DriveFacts, limits: &DriveLimits) -> DriveStep {
    let state = entry.state();
    if state.is_terminal() || state.is_parked() {
        return DriveStep::Wait;
    }
    if facts.pr_open == Some(false) {
        return DriveStep::to(DriveState::Cancelled);
    }
    if facts.messaged {
        return DriveStep::held(HeldReason::Messaged);
    }
    // **A provider outage outranks every bound below it** (#2811 S5b), and the
    // placement is the whole of "no lane timeout and no round spent".
    //
    // Everything from the age backstop down is measuring a WAIT. When the
    // account behind this drive's panes is out of budget those waits are not
    // measuring anything about the drive: the panes are not slow, they are
    // stopped, and no bound the repo can configure makes a vendor's billing
    // resolve sooner. Letting `drive-stalled`, `state-stalled`, `lane-stalled`
    // or `fix-stalled` answer first would report a timeout whose remedy ("read
    // that pane") does not work, an hour late, once per affected drive — which
    // is exactly the measured behaviour #2811 was filed for.
    //
    // Above them, and below `messaged`, because a delegate that actually spoke
    // has said something specific and this has not. It spends no counter for
    // the same reason it outranks the bounds: a drive that survived an outage
    // must not come out of it looking like one that had burned its budget.
    if let Some(provider) = facts.provider_limited.as_deref() {
        if !provider.trim().is_empty() {
            return DriveStep::held(HeldReason::ProviderLimit);
        }
    }
    // **The bounds are clamped HERE, on the values actually read.** §2.3's
    // ranges are a capability boundary, not input hygiene: a repo's `driver:`
    // block may run a tighter loop than INVARIANT 9 and may never run a looser
    // one, because `docs/design/workflows.md`'s closure is that a workflow file
    // selects from what loomux permits and never widens it. S2 clamps as it
    // parses; this clamps again, and the second is not redundant, because this
    // is a `pub fn` over a plain value type that any caller in any crate can
    // reach without going through S2's parser. A boundary that holds only when
    // the expected caller is upstream is not a boundary.
    let limits = &limits.clamped();
    if entry.bounded_age_ms(facts.now_ms) >= minutes_ms(limits.drive_timeout_minutes) {
        return DriveStep::held(HeldReason::DriveStalled);
    }
    // **5a. Then time in THIS state** (#2110) — the bound that does the work,
    // with the age above it as the backstop it falls through to.
    //
    // Below the age deliberately, and the ordering is not the one it looks like
    // it should be. The state bounds are smaller, so in the ordinary run of
    // events one of them fires first *in time* and this line is never reached
    // with both past. Where both ARE past, the drive has just been resumed out
    // of a long park — arc 11 re-stamps `started_ms` and `state_since_ms`
    // together, so the only way to be past the twelve-hour backstop at all is a
    // drive that has genuinely run that long, and that fact outranks whichever
    // state it happens to be sitting in. Putting this first would also reopen
    // §8's `also: [base-green]` row from the other end: that drive resets this
    // clock on every wake, so it must reach the age check, and an ordering that
    // let a per-state bound answer first for some other drive is one more thing
    // to keep true.
    //
    // `required_lanes` is the gate's list at this head; `None` is a routing
    // answer this tick could not produce, and that drive holds
    // `routing-unaccountable` in `decide_review_wait` below on this same tick.
    let lanes = facts.required_lanes.as_deref().map_or(0, |l| l.len());
    if let Some(bound) = state_bound_ms(state, limits, lanes) {
        if entry.state_elapsed_ms(facts.now_ms) >= bound {
            return DriveStep::held(HeldReason::StateStalled);
        }
    }
    // **An unresolved head is not a head, and acting on one is an unbounded
    // spawn loop.** Two of the four states below read `facts.head`, and the
    // guard is here rather than in each of them because it is the *drive* that
    // has no revision to act on, not those two states in particular:
    // `review-wait` compares it against the recorded head and keys a lane brief
    // on it, `fix-wait` compares it for arc 7. `ci-wait` and `gate-check` never
    // read it at all — they would be harmless to dispatch, and returning `Wait`
    // for them too is the conservative reading of "orrerix could not resolve
    // this PR's head this tick", not a claim that they would misbehave.
    // `review-wait` is the dangerous one: the arc-6 guard skips itself when
    // `facts.head` is empty, so the tick falls through to `first_stale_lane`,
    // where `ReviewVerdict::reviewed("")` is false for every real verdict head,
    // and then to `lane_open_for`, which refuses every record briefed at a real
    // head — yielding `OpenLane{0}` on EVERY tick. Worse, each brief re-arms
    // that lane's `spawned_ms`, so `lane-stalled` can never fire and the loop
    // defeats the very bound meant to catch it. §8's posture settles it: an
    // unknown is never a fact, and the drive stays bounded by `drive-stalled`
    // above, which needs no head at all.
    if facts.head.is_empty() {
        return DriveStep::Wait;
    }
    // **6. A CONFLICTING PR takes arc 3 wherever it is observed** (#2311).
    //
    // Read here rather than inside a state, because a conflict is a fact about
    // the PR and not about what the drive happens to be waiting for, and every
    // state that read it separately was a state that could answer something
    // else first:
    //
    // - `gate-check` read `facts.required_lanes` and `facts.gate` — and
    //   nothing about MERGEABILITY, which is the whole of the defect. It was
    //   never a state that read one input: routing has answered
    //   `routing-unaccountable` here since v1. So a base that moved while the
    //   lanes reviewed reached `satisfied` with every lane passed and the PR
    //   unmergeable — #2942, where `rev-final` passed carrying
    //   `mergeable:CONFLICTING` in its own summary and the cost (a hand rebase,
    //   a re-drive at `rounds_already_spent 3`, two fresh whole-diff lanes, ten
    //   minutes of cap starvation, three orchestrator turns) was paid outside
    //   the driver.
    // - `review-wait` asks `route_reviewers` FIRST, and routing needs the
    //   changed-file list, which GitHub does not compute for a conflicted head.
    //   So the same conflict parked the drive `held(routing-unaccountable)` —
    //   a hold whose notice says *which reviewers are required is unknown* for
    //   a PR whose real problem is that it does not merge, and whose remedy
    //   (`drive_review` again) reproduces it. Measured on #3118.
    //
    // Above the routing check for exactly that reason: routing being
    // unaccountable is a CONSEQUENCE of the conflict there, not an independent
    // fact, and the honest report is the one naming the cause. It stays BELOW
    // the bounds and the `messaged` hold above — those are about the drive
    // rather than the PR, and a drive already past its clock is not made young
    // by a rebase — and below the empty-head guard, which is "we could not read
    // this PR at all". That ordering is pinned by
    // `the_bounds_and_the_messaged_hold_outrank_a_conflict`, because a
    // precedence stated in a comment and asserted nowhere is a claim about the
    // order of two `if`s that any edit can silently reverse.
    //
    // **`fix-wait` is the one state excluded, and the exclusion is the
    // `state != DriveState::FixWait` clause below — an opinion, argued here.**
    // The arc table is the BACKSTOP, not the mechanism: `(fix-wait, fix-wait)`
    // is not a legal transition, so without this clause `decide` would propose
    // a step `take` refuses, and the drive would audit `invalid-transition`
    // every tick instead of waiting. The opinion is that a rebase hand-back is
    // already outstanding there and the worker's own signals are what that
    // state waits on: spending a second `rebase_attempts` on the conflict the
    // worker was just asked to fix would park `rebase-limit` before the worker
    // had a chance to push.
    //
    // `Pending`/`Unknown` are NOT conflicts: §8's posture is that an unknown is
    // never a fact about the PR, so a mergeability orrerix could not read is
    // not evidence that a rebase is owed.
    if facts.ci == CiObservation::Conflicting && state != DriveState::FixWait {
        return if counter_exhausted(entry.counters.rebase_attempts, limits.max_rebase_attempts) {
            DriveStep::held(HeldReason::RebaseLimit)
        } else {
            DriveStep::spend(DriveState::FixWait, Counter::RebaseAttempts)
        };
    }
    match state {
        DriveState::CiWait => decide_ci_wait(entry, facts, limits),
        DriveState::ReviewWait => decide_review_wait(entry, facts, limits),
        DriveState::FixWait => decide_fix_wait(entry, facts, limits),
        DriveState::GateCheck => decide_gate_check(entry, facts, limits),
        // Both returned above; repeated here because the enum is closed and a
        // catch-all arm is exactly what §2.1 forbids.
        DriveState::Held | DriveState::Satisfied | DriveState::Cancelled => DriveStep::Wait,
    }
}

fn decide_ci_wait(entry: &DriveEntry, facts: &DriveFacts, limits: &DriveLimits) -> DriveStep {
    match facts.ci {
        // **Arc 2, and since #2168 E1 green is not on its own enough when the
        // head arrived by arc 7.** See [`decide_fix_receipts`] for the whole
        // argument; the guard is here, on the Green arm alone, because it is
        // the only arm that briefs a reviewer. Red and CONFLICTING hand the
        // worker back through arc 3, where `decide_fix_wait`'s own ladder
        // already answers every worker signal, and `Pending`/`Unknown` wait as
        // they always did.
        CiObservation::Green if entry.fix_pushed() => decide_fix_receipts(entry, facts, limits),
        // Arc 2.
        CiObservation::Green => DriveStep::to(DriveState::ReviewWait),
        // Arc 3, spending a CI attempt — or parking, when the budget is gone.
        CiObservation::Red => {
            if counter_exhausted(entry.counters.ci_attempts, limits.max_ci_attempts) {
                DriveStep::held(HeldReason::CiLimit)
            } else {
                DriveStep::spend(DriveState::FixWait, Counter::CiAttempts)
            }
        }
        // Arc 3 again — **answered by [`decide`] before this function is
        // reached** (#2311), which is why it is not a second ladder here: a
        // conflict is a fact about the PR, so every state that can act on one
        // acts through the same rule, with the same counter and the same
        // `held(rebase-limit)`. Kept as an explicit arm rather than folded
        // into the catch-all because the enum is closed and §2.1 forbids a
        // catch-all; `Wait` is what it degrades to if a caller ever reaches
        // this function directly, which is the conservative direction.
        //
        // `Pending`/`Unknown` are neither an answer nor a reason to move. §8:
        // unknown is never treated as safe, and it is never treated as a fact
        // about the PR either.
        CiObservation::Conflicting | CiObservation::Pending | CiObservation::Unknown => {
            DriveStep::Wait
        }
    }
}

/// Green at a head the worker pushed under arc 7: **arc 2 waits for that
/// worker's `report(done)` first** (#2168 E1, closing #1875's class).
///
/// # What green alone does not mean
///
/// A green matrix says the checks settled; it does not say the ROUND is over.
/// This repo's worker persona forbids a `report(done)` before the whole matrix
/// has been re-read, and the PR body's CI receipts — run ids, per-platform
/// conclusions, the head they were measured at — cannot be written until that
/// same moment. So the sequence a green observation sits in the middle of is
/// fixed: push, checks settle, worker reads them, worker edits the body, worker
/// reports. A lane briefed on the green brief is briefed at digest d1 and
/// re-briefed at d2 the moment the receipts land, because
/// [`first_stale_lane`] re-reads the `(head, digest)` key every tick and a
/// `pass` recorded at d1 does not stand at d2. #1875's measurement is that
/// every code PR of that session paid at least one such round, and #1870 is the
/// fully instrumented one: `pass` at digest `bbff76b8`, 0 findings, the CI
/// section filled, `BODY CHANGED SINCE PASS`, gate blocked, re-record — with
/// the head never having moved and not one line of code having changed.
///
/// The digest rule itself is not weakened here and must not be: the body
/// becomes the squash commit message, so a `pass` recorded against different
/// text really has approved something else. What changes is only WHEN the lane
/// is briefed — after the revision has stopped moving rather than during.
///
/// # The two shapes this deliberately is not
///
/// #1875 offers three candidate fixes and two are refused. A **digest
/// carve-out** — a fenced evidence region excluded from the digest — needs that
/// region to be genuinely un-claim-bearing, and a CI section that also carries
/// prose is back where it started; it would also make `body-unchanged` a
/// weaker condition than the `gh` shim's, which §4 forbids. **The engine
/// writing the CI section itself** puts a second author on the PR body beside
/// the worker, which is a wire change and a new class of §3.1 action, for
/// receipts the worker already produces.
///
/// # The worker ladder is `decide_fix_wait`'s, not a second one
///
/// The four signals are read in the same order and answered with the same
/// holds, because this is the same wait for the same worker on the same
/// hand-back — only its location moved. Reading them by a different rule here
/// is the asymmetry `CLAUDE.md` names ("a guard reads every one of its inputs
/// by one rule"), and the two that would be dropped are the two that matter
/// most: a `Blocked` worker is INVARIANT 3 territory and a dead resumed pane is
/// `worker-unresumable`, and letting either fall through to the timeout below
/// would report both as `fix-stalled` — a claim that the worker went silent,
/// about a worker that said something.
///
/// # The bound, and why it is `fix_timeout_minutes` measured from the push
///
/// A silent worker must not hold the drive for ever, and the wait is the same
/// wait §2.2 already bounds — so it takes the same knob. The anchor is
/// [`DriveEntry::fix_pushed_ms`], the LATEST push in this `ci-wait` stay. Not
/// `fix_handback_ms`, which predates the push and would charge the worker for
/// the time it spent doing the work it was asked to do; and not
/// `state_since_ms`, which is the FIRST push, so a follow-up commit late in the
/// window would get the remainder of a window rather than one (rev-final round
/// 2, premortem 2).
/// [`state_bound_ms`]'s `ci-wait` arm ADDS the same knob to its constant so
/// that `held(state-stalled)` cannot preempt this hold on a repo that raised
/// it — the property [`HeldReason::StateStalled`] states, and the reason that
/// arm is an add rather than a max is argued there.
///
/// **`fix_timeout_minutes` cannot push that bound past the backstop**, which is
/// why this arm needs no counterpart to [`state_bound_ms`]'s `review-wait`
/// residual: `parse_workflow` runs the knob through `clamp_expires_minutes`, so
/// the largest value a workflow file can produce is 240 minutes and the sum
/// tops out at 330 against a twelve-hour `drive_timeout_minutes`. The engine
/// deliberately does not re-clamp the timeouts ([`DriveLimits::clamped`] takes
/// only the counters), so a caller building limits directly can still exceed
/// it — and there the age answers first, exactly as `review-wait`'s residual
/// describes. Pinned at the ceiling by
/// `the_ci_wait_bound_funds_the_receipts_wait_instead_of_preempting_it`.
///
/// # Two residuals, both bounded and both recoverable by the resume
///
/// **A worker pane that DIES here.** #1961's exit read is asked only in
/// `fix-wait` (`rdtick`'s `worker_exit`), so a pane killed while the drive
/// waits in `ci-wait` is not seen as `Unresumable` on the next tick; that drive
/// waits out `fix_timeout_minutes` and parks `held(fix-stalled)`, which is
/// bounded and named but is the slower notice. Disclosed rather than closed:
/// widening the exit read is an S3 change to what a tick OBSERVES, and this
/// slice changes what `decide` does with what it is already given.
///
/// **#3225 closed the RESTART half of that, and only that half.** A pane that
/// died because the process did is not ambiguous — every pane did — so the
/// reconcile marks the entry and the `Silent` arm below treats the recorded
/// push as the fix delivered. A pane killed mid-session while orrerix keeps
/// running is still the slower notice, for the reason above: nothing in
/// `ci-wait` observes that exit, and an absent agent mid-session is "we could
/// not check" rather than "it is gone".
///
/// **A worker that pushes and reports inside ONE tick window.** Both facts
/// reach the same tick, [`decide_fix_wait`]'s arc 7 outranks the report on
/// purpose ("the code moved and CI is what has to answer next"), and `rdtick`
/// clears the signal on every arc — so the report is spent on the arc and this
/// state waits for one that will not come, to `held(fix-stalled)`. It is not
/// closed here because a `WorkerSignal` is a word and not a timestamp: the same
/// `Done` beside a moved head is equally consistent with *reported, then
/// pushed*, where the report is about the pre-push tree and honouring it would
/// brief a lane over exactly the unfinished revision #1875 is about. Failing
/// toward a bounded hold rather than toward that defect is the direction this
/// slice takes everywhere, and the ordering is uncommon here by construction —
/// the worker persona forbids `report(done)` before the matrix is re-read, and
/// the matrix takes twenty to thirty minutes against a thirty-second tick.
///
/// **Both clear on the first tick after `drive_review`**, which is what §2.2
/// requires of a hold whose cause is a wait: arc 11 re-enters `ci-wait` from
/// `held`, so [`DriveEntry::advance`] assigns [`DriveEntry::fix_pushed_ms`] false
/// and the next green takes arc 2 with nothing further asked of the worker.
/// Pinned by `a_resume_out_of_fix_stalled_briefs_on_the_next_green`.
fn decide_fix_receipts(entry: &DriveEntry, facts: &DriveFacts, limits: &DriveLimits) -> DriveStep {
    match facts.worker {
        // Nothing to hand back to. Checked first, exactly as in
        // `decide_fix_wait`: every arm below presumes a worker that can be
        // reached.
        WorkerSignal::Unresumable => DriveStep::held(HeldReason::WorkerUnresumable),
        // INVARIANT 3 territory — a blocked worker is the orchestrator's call.
        WorkerSignal::Blocked => DriveStep::held(HeldReason::WorkerBlocked),
        // Arc 2 at last: the revision has stopped moving, so the lane can be
        // briefed at a `(head, digest)` that will still be current when it
        // records.
        WorkerSignal::Done => DriveStep::to(DriveState::ReviewWait),
        // **#3225: the pane whose report this wait is for died with the
        // previous process, so the push it already made IS the fix delivered.**
        //
        // Above the timeout below rather than beside it, and the difference is
        // the whole issue: the timeout's exit is `held(fix-stalled)`, which says
        // a worker went silent, and the worker did not — orrerix was restarted
        // under it. The push is durable, it is at the head CI has just gone
        // green on, and the only thing missing is a `report` no pane can send.
        //
        // What the lane loses by being briefed here is the CI receipts the
        // worker would have written into the body first — so the lane is briefed
        // at digest `d1` and re-briefed at `d2` if a human or a later worker
        // fills them in, which is exactly the round [`decide_fix_receipts`]'s
        // own header describes and is bounded by the same `(head, digest)` key.
        // That is one re-brief; the alternative measured on #3225's incident was
        // a whole `fix_timeout_minutes` and an orchestrator turn.
        //
        // **Only under `Silent`**: a `Done`, a `Blocked` or an `Unresumable`
        // signal is something this drive was actually told, and the arms above
        // answer each of them. The mark cannot manufacture one.
        WorkerSignal::Silent if facts.restart_push_delivered => {
            DriveStep::to(DriveState::ReviewWait)
        }
        WorkerSignal::Silent => {
            // **The LATEST push in this `ci-wait` stay**, which is what
            // `fix_pushed_ms` exists to carry — see that field, and
            // `note_fix_push` for the re-stamp. `state_since_ms` would be the
            // first one, and a worker that pushed a follow-up commit late in
            // the window would get the remainder rather than a window.
            //
            // The fallback is unreachable through `decide`: this function is
            // called from one place, under a guard that already asked
            // `fix_pushed()`. It is `state_since_ms` rather than a panic or a
            // zero because those are the two ways a fallback goes wrong — a
            // zero clears every timeout, which is the false-park direction the
            // field doc argues against, and an unwind out of the poll thread
            // takes the fleet's watches down with it.
            let since = entry.fix_pushed_ms.unwrap_or(entry.state_since_ms);
            if facts.now_ms.saturating_sub(since) >= minutes_ms(limits.fix_timeout_minutes) {
                DriveStep::held(HeldReason::FixStalled)
            } else {
                DriveStep::Wait
            }
        }
    }
}

fn decide_review_wait(entry: &DriveEntry, facts: &DriveFacts, limits: &DriveLimits) -> DriveStep {
    let Some(required) = facts.required_lanes.as_deref() else {
        return DriveStep::held(HeldReason::RoutingUnaccountable);
    };
    // Arc 6: the head moved under a lane. Checked before the verdicts are read,
    // because a verdict that lands at the old head is answered by the binding
    // rules, not by this state — a `fail` there still routes when the drive
    // comes back through `review-wait`, and a `pass` there is already stale.
    if !facts.head.is_empty() && entry.head != facts.head {
        return DriveStep::to(DriveState::CiWait);
    }
    let digest = facts.body_digest.as_deref();
    // Arc 4's precondition, and equally the re-entry point when a digest moved
    // under a recorded pass (§8's body-changed row): the first lane whose pass
    // does not stand here.
    let k = first_stale_lane(required, &facts.head, digest);
    if k >= required.len() {
        return DriveStep::to(DriveState::GateCheck);
    }
    let lane = &required[k];
    // **Is the brief this state may be about to send a body-verification delta?**
    // (#2168 E2.)
    //
    // The precondition is the whole of what makes the delegation honest, so it
    // is spelled as one `all` over the required set rather than inferred from
    // the fact that `k` happened to be stale: EVERY required lane has recorded a
    // `pass` bound to the head that would merge. Nothing about the CODE is
    // outstanding, so the only thing that can have staled lane `k` above is the
    // body — and what the lane is about to be asked for is the body as it
    // stands.
    //
    // A readable live digest is required too (`digest.is_some()`), because the
    // grant this writes is meaningless without one: the verdict's own digest is
    // what a gate later compares. "We could not read the body" grants nothing,
    // the same direction `lane_open_for` and `ReviewVerdict::body_changed` take
    // one question over.
    //
    // Deliberately NOT conditioned on the digest having moved for lane `k`. It
    // has — a lane whose pass is bound to this head reaches `k` only when
    // `lane_pass_settles` said no, and at this point `body_is_verified` is false
    // (or `first_stale_lane` would have returned `required.len()`), so the digest
    // is the only axis left. Re-deriving that here would be a second
    // implementation of `first_stale_lane`'s answer, and the two could drift.
    let verify = digest.is_some()
        && required.iter().all(|l| {
            l.verdict.as_ref().is_some_and(|v| {
                v.verdict == Verdict::Pass && v.reviewed(&facts.head)
            })
        });
    // **Is this brief about the BODY of a revision whose CODE is already fully
    // reviewed?** (#2509.)
    //
    // One conjunct weaker than `verify` above and one stronger, and both
    // differences are load-bearing.
    //
    // Weaker: every required lane must have ANSWERED at this head, not passed
    // at it. That gap IS the case #2509 was filed for — a lane that recorded
    // `fail` on the PR body, whose worker then moved the body and not the head,
    // which `verify` can never see because that lane's word is not `pass`.
    // Measured on PR #2397, which reached `held(review-limit)` on two body
    // sentences with the code green and unmoved throughout.
    //
    // Stronger: THIS lane must already have been briefed at this head. Without
    // it the very first brief at a head qualifies — every lane can be bound to
    // a head the moment they have all spoken — and the grace would be granted
    // for a fail on code nobody had reviewed twice. `briefed_head` is what says
    // this is a RE-brief, and a re-brief at an unchanged head can only be about
    // the body: `lane_open_for` is what routed us here, and at an equal head the
    // digest is the only axis it has left.
    //
    // A readable live digest is required for `verify`'s reason: what this
    // grants is read back against an exact `(briefed_head, briefed_digest)`
    // pair, and a brief whose revision cannot be pinned grants nothing.
    let body_only = digest.is_some()
        && entry
            .lane(&lane.block)
            .is_some_and(|r| !r.briefed_head.is_empty() && r.briefed_head == facts.head)
        && required
            .iter()
            .all(|l| l.verdict.as_ref().is_some_and(|v| v.reviewed(&facts.head)));
    // **A verdict decides only if it was recorded about THIS revision**, and the
    // currency test is asked here rather than inside the arms so no future word
    // can be added below without it (#1871 B1).
    //
    // The defect this closes: a lane's `fail` recorded at the head the worker has
    // since fixed stayed authoritative for ever. The drive took arc 7 out of
    // `fix-wait` on the new head, went green, came back here — and read the SAME
    // stale `fail`, spent a review round on it, and handed the worker back its
    // own already-addressed findings as "attempt 2". Three passes reached the
    // bound with no re-review having happened at all. Nothing re-opened the lane
    // because nothing below ever reached [`lane_open_for`]: the `Fail` arm
    // answered first, and it answered from a commit that no longer described the
    // PR.
    //
    // **Word-blind on purpose.** `escalate` is the same shape — an escalation of
    // a revision that no longer exists is not a judgment anyone is being asked
    // for — and so is a `pass`, which [`first_stale_lane`] has already filtered
    // for currency before this line is reached. Treating a stale verdict as
    // ABSENT is what puts the lane back on the `Some(Verdict::Pass) | None` arm,
    // where [`lane_open_for`] decides between re-briefing it and waiting for a
    // brief already out at this revision — which is the re-open the head change
    // owed and never got.
    let current = lane
        .verdict
        .as_ref()
        .filter(|v| lane_verdict_is_current(Some(v), &facts.head, digest));
    match current.map(|v| v.verdict) {
        // A lane recorded `escalate` at this revision: an LLM judgment call, and
        // §3 says the driver never makes one.
        Some(Verdict::Escalate) => DriveStep::held(HeldReason::Escalate),
        // Arc 5, spending a review round — or parking, when the budget is gone,
        // unless #2509's one-shot grace answers first.
        Some(Verdict::Fail) => {
            if counter_exhausted(entry.counters.review_rounds, limits.max_review_rounds) {
                // **The grace is asked ONLY at the bound**, which is what makes
                // it a grace rather than a discount. Below the bound the
                // ordinary round is spent and `body_only_grace` is untouched, so
                // a drive that never reaches the bound never consumes it — and a
                // drive that reaches the bound on a code fail never gets it.
                if body_only_grace_applies(entry, &lane.block, &facts.head, digest) {
                    DriveStep::spend(DriveState::FixWait, Counter::BodyOnlyGrace)
                } else {
                    DriveStep::held(HeldReason::ReviewLimit)
                }
            } else {
                DriveStep::spend(DriveState::FixWait, Counter::ReviewRounds)
            }
        }
        // Either no verdict bound to this revision, or a `pass` that no longer
        // stands here — which are the same thing to this state: the lane is
        // outstanding and must be briefed for this revision. Whether it
        // *already* was is what [`lane_open_for`] answers, on the full
        // (head, digest) key.
        Some(Verdict::Pass) | None => {
            match entry.lane(&lane.block) {
                // **The stall clock is keyed on the HEAD, not on the full
                // (head, digest) key** (#2109).
                //
                // It used to hang off the arm below, so it was asked only of a
                // lane that was still open for this exact revision — and a body
                // edit under a silent reviewer moved the digest, dropped through
                // to a re-brief, and re-armed `spawned_ms`. A reviewer that had
                // said nothing for fifty-nine minutes was given another hour by
                // an edit it never read, which is the defeat-your-own-bound shape
                // `decide`'s empty-head guard describes one screen up.
                //
                // #2109 makes that reachable rather than theoretical: the
                // re-brief it produced is now REFUSED while that lane's pane is
                // live, so without this the drive would retry the refusal until
                // `state-stalled` at the `review-wait` bound, and then
                // `drive-stalled` — four hours and twelve on a one-lane gate at
                // stock knobs, on notices that name no lane (#2110 renumbered
                // those two; neither names one).
                //
                // **`at_head` is what makes this a silence test rather than a
                // stopwatch** (#2109 review 1). The first version keyed on
                // `(briefed_head, spawned_ms)` alone, and that pair cannot see
                // whether the reviewer ANSWERED — which is the very distinction
                // [`LaneRecord::at_head`] exists to carry. A lane that recorded
                // `fail` at this head, whose worker then made a BODY-ONLY fix,
                // returns here through arc 8 at the same head with a moved
                // digest: the stale verdict reads as absent, this arm is
                // reached, and `now - spawned_ms` is review time PLUS fix time.
                // Past sixty minutes of that sum the drive parked
                // `held(lane-stalled)` on a reviewer that had never been
                // silent — and stuck, since only `open_lane` writes
                // `spawned_ms`, so a `drive_review` resume re-decided on
                // unchanged facts and re-held.
                //
                // A lane that has answered about THIS revision is therefore
                // exempt, whatever the body has done since; what it is owed is
                // the delta re-brief below, which resets both `at_head` and the
                // clock. Only a lane asked about this head and still silent past
                // its timeout stalls.
                Some(rec)
                    if rec.briefed_head == facts.head
                        && rec.at_head != facts.head
                        && facts.now_ms.saturating_sub(rec.spawned_ms)
                            >= minutes_ms(limits.lane_timeout_minutes) =>
                {
                    DriveStep::held(HeldReason::LaneStalled)
                }
                // **A lane whose pane is GONE is not a lane to wait for**
                // (#2163). `lane_open_for` answers "was this lane asked about
                // this revision", which stays true forever after the pane that
                // was asked has exited — so a killed reviewer left this state
                // waiting on a verdict nothing could produce, with `lane-stalled`
                // an hour away and not one rd-* row in between.
                //
                // Read AFTER the stall arm, and the ordering is the bound. A
                // pane that dies on every spawn would otherwise be re-opened for
                // ever: this arm answers first on every tick, so the stall arm
                // would never be evaluated. Below it, and with the re-open
                // PRESERVING `spawned_ms` (see [`lane_stall_anchor`]), a lane
                // whose panes keep dying still reaches `lane_timeout_minutes`
                // from the ORIGINAL brief and parks `held(lane-stalled)` naming
                // it — which is the true statement about that lane anyway: it
                // has been silent about this head for an hour.
                //
                // What the re-open costs is one pane on a session the lane
                // already has, which `rd_lane_session` resolves from the record,
                // the roster or the merged records; a session that no longer
                // resolves falls to the existing `rd-lane-resume-failed` →
                // fresh-spawn path.
                Some(rec) if lane_open_for(rec, &facts.head, digest) && !lane.pane_dead => {
                    DriveStep::Wait
                }
                // **The cap's starvation is reported before another spawn is
                // proposed** (#2109). The tick stamps
                // `cap_starved_since_ms` on the first lane spawn the
                // live-delegate cap refuses and leaves it alone on the rest, so
                // this reads the duration of one refusal RUN, not the age of the
                // last tick.
                //
                // Proposing `OpenLane` anyway would be harmless — the spawn
                // would be refused again and the entry re-stamped with nothing —
                // and that is exactly what made the measured incident invisible:
                // 37 identical `rd-refused` rows, `lanes: []`, no notice, three
                // hours. §2.2's exits are the only thing an orchestrator reads,
                // so a condition that needs an orchestrator has to become one.
                _ if entry.cap_starved_for(facts.now_ms).is_some_and(|d| d >= CAP_HOLD_MS) => {
                    DriveStep::held(HeldReason::CapFull)
                }
                _ => DriveStep::OpenLane { index: k, verify, body_only },
            }
        }
    }
}

fn decide_fix_wait(entry: &DriveEntry, facts: &DriveFacts, limits: &DriveLimits) -> DriveStep {
    match facts.worker {
        // Nothing to hand back to. Checked first: every other arm here presumes
        // a worker that can be reached.
        //
        // **Except on the first tick after a restart** (#2811 S10). The tick
        // derives `Unresumable` here from the worker PANE having exited, and
        // after a restart every pane has exited — so the signal is true of the
        // process rather than of the session, and holding on it would park every
        // resumable fix-wait drive the moment orrerix came back up. Falling
        // through to the re-hand-back below is what tells the two apart: it
        // resumes the recorded session, and a session that genuinely cannot be
        // resumed makes `rd_handback` fail, which lands on this same hold by the
        // ordinary path one tick later. The probe is the answer, not a guess
        // about it.
        WorkerSignal::Unresumable if !facts.restart_handback => {
            return DriveStep::held(HeldReason::WorkerUnresumable)
        }
        WorkerSignal::Unresumable => {}
        // INVARIANT 3 territory — a blocked worker is the orchestrator's call.
        WorkerSignal::Blocked => return DriveStep::held(HeldReason::WorkerBlocked),
        WorkerSignal::Done | WorkerSignal::Silent => {}
    }
    // Arc 7: the worker pushed. Outranks `report(done)` deliberately — if both
    // happened, the code moved and CI is what has to answer next.
    if !facts.head.is_empty() && entry.head != facts.head {
        return DriveStep::to(DriveState::CiWait);
    }
    // Arc 8: `report(done)` with the head unchanged — a body-only fix.
    if facts.worker == WorkerSignal::Done {
        return DriveStep::to(DriveState::ReviewWait);
    }
    // #2811 S10, BELOW arcs 7 and 8 and ABOVE the stall. Below them because a
    // worker that pushed or reported before the process went down has already
    // answered, and re-briefing it would ask again for work that is done. Above
    // the stall because `fix_handback_ms` predates the restart: most of the gap
    // it measures is downtime in which no worker could have answered, so
    // `held(fix-stalled)` — a claim about a WORKER being silent — would be
    // false. Nothing is charged either way; see [`DriveStep::Rehandback`].
    if facts.restart_handback {
        return DriveStep::Rehandback;
    }
    if facts.now_ms.saturating_sub(entry.fix_handback_ms) >= minutes_ms(limits.fix_timeout_minutes)
    {
        return DriveStep::held(HeldReason::FixStalled);
    }
    DriveStep::Wait
}

fn decide_gate_check(entry: &DriveEntry, facts: &DriveFacts, limits: &DriveLimits) -> DriveStep {
    // §4: `route_reviewers` returning `None` is `held(routing-unaccountable)`
    // from every state that reads it, **`gate-check` included**. This is the
    // one degradation whose absence would be a security defect rather than an
    // inconvenience: the unknown thing is *which reviewers are required*, so
    // guessing "no rule fired" is guessing in favour of merging, and the gate
    // would answer allowed on a reviewer list nobody could compute — §3.1's
    // "a bypass with better telemetry".
    if facts.required_lanes.is_none() {
        return DriveStep::held(HeldReason::RoutingUnaccountable);
    }
    match facts.gate {
        GateOutcome::Unreadable => DriveStep::held(HeldReason::GateUnreadable),
        // #3367 item 1: a satisfied gate whose lanes left only NON-blocking
        // findings goes back to the worker instead of waking the
        // orchestrator — arc 3's `(gate-check, fix-wait)` pair, so the arc
        // table is unchanged, told apart from the conflict hand-back by the
        // counter it spends. Every precondition, the shared bound included,
        // is in [`nonblocking_round_applies`]; anything it cannot establish
        // falls through to arc 9, which is today's behaviour.
        GateOutcome::Satisfied if nonblocking_round_applies(entry, facts, limits) => {
            DriveStep::spend(DriveState::FixWait, Counter::NonblockingRound)
        }
        // Arc 9.
        GateOutcome::Satisfied => DriveStep::to(DriveState::Satisfied),
        // Arc 10, which is deliberately wider than "stale".
        GateOutcome::Unsatisfied => DriveStep::to(DriveState::CiWait),
        // The tick reached `gate-check` without evaluating the gate. Nothing is
        // known, so nothing moves — and in particular this is not `satisfied`.
        GateOutcome::NotEvaluated => DriveStep::Wait,
    }
}

/// What one reviewer's verdict SUMMARY says about the findings it left open
/// (#3367 item 1): a count per class, or `None` where the summary does not
/// state one unambiguously.
///
/// **Parsed from prose, and fail-safe in exactly one direction.** The
/// driver has never parsed findings out of a summary — [`crate::rddrive`]'s
/// satisfied notice says so — and it does not start pretending to here. What
/// this reads is a COUNT the reviewer stated (`0 blocking`, `blocking: 0`,
/// `3 non-blocking`, `no blocking findings`), because `reviewer.md` asks every
/// reviewer to state one; a summary that states none, or states two different
/// ones, is `None`, and `None` is "unknown", which never licenses a hand-back.
/// A reviewer that writes an unexpected phrasing costs the orchestrator the
/// wake it would have had anyway, and never costs the PR a round nobody
/// dispositioned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatedFindings {
    pub blocking: Option<u32>,
    pub non_blocking: Option<u32>,
}

/// A count word as a reviewer writes one: digits, or the small English numbers
/// verdict summaries actually use ("no blocking", "Two non-blocking notes").
fn count_word(tok: &str) -> Option<u32> {
    if let Ok(n) = tok.parse::<u32>() {
        return Some(n);
    }
    let n = match tok {
        "no" | "zero" | "none" => 0,
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        _ => return None,
    };
    Some(n)
}

/// Read [`StatedFindings`] out of one verdict summary.
///
/// The grammar is two shapes and nothing else, both matched on whole tokens:
/// `<count> <label>` and `<label>: <count>`, where the label is `blocking` /
/// `blocker(s)` or their `non-` forms (`non blocking` and `nonblocking` are
/// normalised to `non-blocking` first, so a `blocking` token can never be the
/// second half of a non-blocking one). A count is a whole token — `round-2
/// blocking finding fixed` is not a count of two, because `round-2` is not a
/// count word. Every count stated for a class must agree; two different ones
/// (`1 blocking … fixed; 0 blocking now`) make that class `None`.
pub fn stated_findings(summary: &str) -> StatedFindings {
    let (blocking, non_blocking) = stated_counts(summary);
    let agreed = |v: &[u32]| -> Option<u32> {
        let first = *v.first()?;
        v.iter().all(|n| *n == first).then_some(first)
    };
    StatedFindings { blocking: agreed(&blocking), non_blocking: agreed(&non_blocking) }
}

/// EVERY count a summary states, per class — `(blocking, non_blocking)` —
/// before [`stated_findings`] asks whether they agree (#3388 review round 2).
///
/// Its own function because the clean case needs the question the agreement
/// hides: "did this reviewer state ANY non-zero count?" A summary reading
/// `1 blocking finding from round 1 fixed; 0 blocking now` agrees on nothing,
/// so [`stated_findings`] answers `None` for it — correct for a count, and
/// exactly the case a veto keyed on that answer would miss.
pub fn stated_counts(summary: &str) -> (Vec<u32>, Vec<u32>) {
    let norm = summary
        .to_lowercase()
        .replace("non blocking", "non-blocking")
        .replace("nonblocking", "non-blocking")
        .replace("non blocker", "non-blocker");
    let toks: Vec<(&str, bool)> = norm
        .split_whitespace()
        .map(|raw| {
            let core = raw.trim_matches(|c: char| !(c.is_alphanumeric() || c == '-'));
            (core, raw.ends_with(':'))
        })
        .collect();
    let label = |core: &str| -> Option<bool> {
        match core {
            "blocking" | "blocker" | "blockers" => Some(true),
            "non-blocking" | "non-blocker" | "non-blockers" => Some(false),
            _ => None,
        }
    };
    let mut blocking: Vec<u32> = Vec::new();
    let mut non_blocking: Vec<u32> = Vec::new();
    for (i, (core, colon)) in toks.iter().enumerate() {
        let Some(is_blocking) = label(core) else { continue };
        // ONE count per label, and a colon label takes the count AFTER it: in
        // `blocking: 0, non-blocking: 2` the `0,` before `non-blocking:` is the
        // previous clause's, and reading it too would make a well-stated
        // summary disagree with itself.
        let after = if *colon { toks.get(i + 1).and_then(|t| count_word(t.0)) } else { None };
        let n = after.or_else(|| i.checked_sub(1).and_then(|j| count_word(toks[j].0)));
        if let Some(n) = n {
            if is_blocking {
                blocking.push(n);
            } else {
                non_blocking.push(n);
            }
        }
    }
    (blocking, non_blocking)
}

/// **What one PASS verdict leaves open — the reviewer's structured declaration
/// first, the summary parser as its fallback** (#3367 item 5).
///
/// `open_findings` is the explicit form of the number [`stated_findings`]
/// reads out of prose, so there is ONE count with two sources rather than two
/// counts: where the reviewer declared it, it wins; where it did not, the
/// parser answers exactly as it did before this field existed.
///
/// A declaration is a TOTAL, blocking and non-blocking together. On a `pass`
/// nothing is blocking by the verdict's own definition (`reviewer.md`: an
/// approval with findings open is only ever one with non-blocking ones), so an
/// unstated blocking count reads as `0` here and the rest of the total is
/// non-blocking. A summary that STATES a blocking count larger than the
/// declared total contradicts it, and a contradiction is unknown on both
/// classes — the fail-safe direction, since unknown never licenses a hand-back
/// and never makes a lane clean.
///
/// Called only for passes: [`lane_residuals`] and the notice helpers filter to
/// `Verdict::Pass` before they ask.
pub fn verdict_findings(summary: &str, open_findings: Option<u32>) -> StatedFindings {
    let stated = stated_findings(summary);
    let Some(total) = open_findings else { return stated };
    let blocking = stated.blocking.unwrap_or(0);
    if blocking > total {
        return StatedFindings::default();
    }
    StatedFindings { blocking: Some(blocking), non_blocking: Some(total - blocking) }
}

/// **This pass declared `open_findings: 0` and its summary states no open
/// finding of either class** (#3367 item 5) — one lane's half of the clean
/// case.
///
/// The declaration is REQUIRED: a lane that omitted it is not clean, however
/// its summary reads, because "the parser found `0 blocking, 0 non-blocking`"
/// is prose and the clean case skips the orchestrator's disposition entirely.
///
/// **And the summary has a veto over it, in BOTH classes** (#3388 review round
/// 2, a policy call made blocking): any non-zero count the summary states —
/// `1 blocking`, or `2 non-blocking` — makes the lane not clean, whatever
/// was declared. "Clean" means nothing is left to disposition (INVARIANT 3),
/// and a reviewer stating open nits has left something, however it filled in
/// the field. The veto reads [`stated_counts`], every count stated, never the
/// agreed one: `1 blocking … fixed; 0 blocking now` agrees on nothing, and a
/// veto keyed on agreement would let it through. The price is disclosed rather
/// than avoided: a summary whose multi-round prose still carries round one's
/// count loses the shortcut and wakes the orchestrator, which is the wake it
/// would have had anyway.
///
/// This is a veto on the CLEAN flag only. [`verdict_findings`] still reads the
/// declaration first for the non-blocking round, so a declared `0` still ends
/// the nit loop for that lane — a lane vetoed here is simply satisfied the
/// ordinary way, with its residual the orchestrator's to disposition.
pub fn declared_clean(summary: &str, open_findings: Option<u32>) -> bool {
    let (blocking, non_blocking) = stated_counts(summary);
    open_findings == Some(0) && blocking.iter().chain(&non_blocking).all(|n| *n == 0)
}

/// **Every required lane PASSED at `head`, each declaring `open_findings: 0`**
/// (#3367 item 5).
///
/// Positive on every axis, like [`residual_is_nonblocking_only`]: an empty
/// lane list, an empty head, a stale pass, a lane with no verdict, a `fail` or
/// an `escalate` all answer `false`, and `false` is simply the ordinary
/// `GATE SATISFIED` — nothing is refused, only the shortcut is not taken.
pub fn lanes_are_clean(lanes: &[LaneFact], head: &str) -> bool {
    !head.is_empty()
        && !lanes.is_empty()
        && lanes.iter().all(|l| {
            l.verdict.as_ref().is_some_and(|v| {
                v.verdict == Verdict::Pass
                    && v.reviewed(head)
                    && declared_clean(&v.summary, v.open_findings)
            })
        })
}

/// **The clean case**: the gate is satisfied at the live head, CI is green, and
/// [`lanes_are_clean`] (#3367 item 5).
///
/// CI is asked here explicitly rather than inherited from the gate, because a
/// gate need not declare `also: [ci-green]` and the brief's "clean" includes a
/// green CI whatever this repo's gate says. What the answer changes is the
/// satisfied exit's ROUTE — an enqueue where the merge queue is on, a flagged
/// notice where it is not — never whether the drive is satisfied.
pub fn gate_is_clean(facts: &DriveFacts) -> bool {
    facts.gate == GateOutcome::Satisfied
        && facts.ci == CiObservation::Green
        && facts.required_lanes.as_deref().is_some_and(|l| lanes_are_clean(l, &facts.head))
}

/// One required lane's residual, as a non-blocking round or a satisfied
/// notice reports it (#3367 item 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneResidual {
    pub block: BlockId,
    /// `Some` only for a `pass` bound to the live head — a stale verdict, a
    /// `fail`, an `escalate` or no verdict at all states nothing this round can
    /// act on, and reads as all-unknown.
    pub findings: StatedFindings,
}

/// Every required lane's [`LaneResidual`] at `head`, in the gate's order.
pub fn lane_residuals(lanes: &[LaneFact], head: &str) -> Vec<LaneResidual> {
    lanes
        .iter()
        .map(|l| LaneResidual {
            block: l.block.clone(),
            findings: l
                .verdict
                .as_ref()
                .filter(|v| v.verdict == Verdict::Pass && v.reviewed(head))
                // #3367 item 5: the declaration first, the parser as fallback —
                // which is also what makes `open_findings: 0` END the nit loop
                // for this lane: it states zero non-blocking, so it can never be
                // the lane `residual_is_nonblocking_only` needs above zero.
                .map(|v| verdict_findings(&v.summary, v.open_findings))
                .unwrap_or_default(),
        })
        .collect()
}

/// **Every lane stated zero blocking, and at least one stated a positive
/// non-blocking count** — the one residual a non-blocking round may be
/// spent on (#3367 item 1).
///
/// Both halves are POSITIVE statements. A lane that did not say how many
/// blocking findings it left is unknown, and unknown wakes the orchestrator —
/// the fail-safe the brief names. A lane that stated no non-blocking count is
/// fine as long as some other lane stated one: the hand-back says "address all
/// the findings on the PR", so the worker reads the PR, not this count.
pub fn residual_is_nonblocking_only(residuals: &[LaneResidual]) -> bool {
    !residuals.is_empty()
        && residuals.iter().all(|r| r.findings.blocking == Some(0))
        && residuals.iter().any(|r| r.findings.non_blocking.is_some_and(|n| n > 0))
}

/// **May the driver hand a satisfied gate back to the worker, on its own, for
/// the non-blocking findings still open?** (#3367 item 1)
///
/// Every answer that is not a positive yes is arc 9 — the orchestrator is
/// woken with GATE SATISFIED, which is today's behaviour. The yes needs all
/// four:
///
/// 1. **The repo asked for it and has rounds left**: `nit_rounds <
///    fix_nonblocking_rounds`, which is false at the default `0`.
/// 2. **INVARIANT 9's bound has a round left** — the SHARED one,
///    check-before-bump through [`counter_exhausted`], because the round this
///    spends is a review round (see [`Counter::NonblockingRound`]).
/// 3. **The revision moved since the last such round.** A gate satisfied again
///    at the head AND body the last round was handed back at is a worker that
///    changed nothing — it answered the findings on the PR instead — and a
///    second round would hand back the identical list. An unreadable digest at
///    an unchanged head counts as unchanged: "we could not check" never buys a
///    round.
/// 4. **Every required lane positively stated zero blocking, and some lane a
///    positive non-blocking count** — [`residual_is_nonblocking_only`].
pub fn nonblocking_round_applies(
    entry: &DriveEntry,
    facts: &DriveFacts,
    limits: &DriveLimits,
) -> bool {
    let limits = limits.clamped();
    if entry.nit_rounds >= limits.fix_nonblocking_rounds {
        return false;
    }
    if counter_exhausted(entry.counters.review_rounds, limits.max_review_rounds) {
        return false;
    }
    if !entry.nit_head.is_empty() && entry.nit_head == facts.head {
        let moved = match facts.body_digest.as_deref() {
            Some(d) => !d.is_empty() && !entry.nit_digest.is_empty() && d != entry.nit_digest,
            None => false,
        };
        if !moved {
            return false;
        }
    }
    let Some(lanes) = facts.required_lanes.as_deref() else { return false };
    residual_is_nonblocking_only(&lane_residuals(lanes, &facts.head))
}

/// Why a driven pane is no longer needed — the **closed set** of three, and the
/// whole of what #2501 and #2811 S1 narrowed §3.1 item 5 to.
///
/// The note's item 5 was a closed sentence ("the driver may never kill a pane")
/// and it named its own reopening condition: *"a later measurement shows drives
/// starving on panes nothing frees"*. #2501 is that measurement — 12 driven PRs,
/// 20 `rd-refused` rows on one of them, five `held(cap-refused)` exits, and about
/// 25 orchestrator wakes spent doing by hand exactly what these variants do. So
/// the item is narrowed rather than deleted, and the narrowing is this enum:
/// anything not spelled here is still a pane the driver may not touch. #2811 S1 is
/// the SECOND such measurement — 15 of 20 hand-backs in one 6.4-hour session
/// held a slot the whole round, 33 kills by hand — and it adds
/// [`DriveEnded`](ReleaseReason::DriveEnded) rather than loosening either of the
/// two that were already here.
///
/// **What makes exactly these safe is not that they are idle.** The idle
/// reaper's demotion argument ("no task in flight, nothing to lose") is half of
/// it; the other half is that in each case the pane's OUTPUT is already on
/// durable record — a verdict file the gate re-reads, or a `report` the drive has
/// consumed and acted on, or (at a terminal step) a drive with nothing left to
/// ask for at all — and the CONVERSATION survives, because the driver resumes
/// lanes and workers by session and has done since #2109. A release therefore
/// destroys nothing: not work, not a decision, not a reviewer's memory of the
/// PR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseReason {
    /// A reviewer lane whose verdict is recorded **at the revision now on the
    /// PR**. The lane has answered the only question this drive asks it, so
    /// there is nothing left in that pane for the drive to wait for; the next
    /// round resumes its session (§5.2's `session`, resolved through the record,
    /// the roster or the merged records).
    ///
    /// Word-blind, on [`lane_verdict_is_current`]'s own argument: a verdict is
    /// bound to the revision it reviewed, and what makes the pane finished is
    /// that it ANSWERED about this revision, not which word it chose.
    VerdictRecorded,
    /// The worker pane whose `report` this tick consumed. §7 intercepts it, arc
    /// 8 acts on it, and the hand-back that follows resumes
    /// [`DriveEntry::worker_session`] rather than the pane — so the pane's only
    /// remaining function was to hold a slot.
    ReportConsumed,
    /// **The drive is OVER** — this tick's step is `satisfied` or `cancelled`,
    /// and the worker pane is a slot held for a drive that will never ask it
    /// anything again (#2811 S1).
    ///
    /// It is a third variant rather than a reuse of [`ReportConsumed`] because
    /// the audit reason is a claim: a drive can reach a terminal step by a path
    /// on which no report was ever consumed (resumed out of
    /// `held(fix-stalled)`, then green), and labelling that release
    /// `report-consumed` would be a false row on the one surface §5.4 asks a
    /// reader to count from. The two variants also carry different SAFETY
    /// arguments: `ReportConsumed` rests on the report being on durable record,
    /// this one on there being nothing left to wait for at all.
    ///
    /// The guard that keeps it honest is [`DriveEntry::handback_outstanding`]:
    /// a drive cancelled while its worker is mid-round is still waiting on that
    /// worker, and releases nothing.
    DriveEnded,
    /// **A reviewer lane reviewing a head that is about to be rebased away**
    /// (#3176) — the PR is CONFLICTING and the lane has recorded nothing at this
    /// revision.
    ///
    /// The fourth variant, and the first whose safety argument is NOT the one
    /// the other three share. Their pane's output is already on durable record;
    /// this pane has produced none, and that is the whole point. A verdict
    /// recorded against a conflicted head binds to a commit the rebase is about
    /// to replace, so [`lane_verdict_is_current`] is false for it the moment the
    /// worker pushes: the round is spent and the drive re-briefs that lane at the
    /// new head anyway. What a release destroys here is therefore a review whose
    /// only possible product is a stale verdict — measured on #3150, which paid a
    /// whole `rev-std` pass exactly that way.
    ///
    /// It is a fourth WORD rather than a reuse for the reason
    /// [`DriveEnded`](ReleaseReason::DriveEnded) is one: a reason is a claim on
    /// the surface §5.4 asks a reader to count from, and a reader counting the
    /// releases that followed a finished review must be able to leave this one
    /// out.
    ///
    /// **It spends no counter and proposes no arc.** #2311's hoist already spends
    /// `rebase_attempts` for the conflict and hands the worker back;
    /// `review_rounds` is untouched here exactly as it is there, because no lane
    /// delivered any findings.
    ///
    /// A lane that has ALREADY answered at this head is deliberately left to
    /// [`VerdictRecorded`](ReleaseReason::VerdictRecorded) and to the ordinary
    /// stale-verdict handling: its pane is finished either way, and labelling
    /// that release `conflict` would be the false row this variant exists to
    /// avoid.
    Conflict,
}

impl ReleaseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ReleaseReason::VerdictRecorded => "verdict-recorded",
            ReleaseReason::ReportConsumed => "report-consumed",
            ReleaseReason::DriveEnded => "drive-ended",
            ReleaseReason::Conflict => "conflict",
        }
    }
}

/// One pane [`releasable`] says this drive no longer needs.
///
/// The pane id is deliberately NOT here. This crate is Tauri-free and cannot ask
/// whether a pane is alive, idle or typeable — those are the registry's facts,
/// injected the way [`DriveEntry::forget_dead_panes`] injects liveness — so what
/// this carries is the DRIVE's half of the decision (which side, and why) and
/// the caller reads the pane off the entry and applies its own half.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseCandidate {
    pub role: DrivenRole,
    pub reason: ReleaseReason,
    /// **Whether the caller's population includes the panes this drive was
    /// STARTED ON, and not only the ones it resumed into** (#3250) — see
    /// [`DriveEntry::founding_panes`].
    ///
    /// [`DriveEntry::owned_panes`] names what the drive itself opened or took
    /// over, and that set is EMPTY for a drive that never handed back — which
    /// is the common shape, not a corner: an orchestrator starts a drive on a
    /// worker that has already pushed and reported, every lane passes, and no
    /// hand-back is ever taken. Measured on PRs #3243 and #3248, where the
    /// audit log carries `rd-started` -> `rd-satisfied` with no `rd-handback`
    /// row at all and the worker pane the orchestrator named at
    /// `start_review_drive` was still alive and idle at the exit, holding the
    /// worktree an orchestrator then had to `kill_agent` by hand.
    ///
    /// True at a TERMINAL step and false everywhere else, and that bound is the
    /// argument, not caution. At `satisfied` and at `cancelled` alike the drive
    /// is over — its own notice tells the orchestrator the conversation resumes
    /// with `spawn_agent(resume:)` — and the terminal rule already kills the
    /// panes it OWNS at both, so the two exits differing in population was the
    /// same defect one state over rather than a deliberate narrowing (review
    /// round 1, finding 1). Mid-drive the pane may be one the orchestrator is
    /// still using, so [`ReleaseReason::ReportConsumed`] keeps the #3203
    /// population (every pane the drive OWNS on that session) and this stays
    /// false.
    ///
    /// **It is not a kill of everything on the session, and the first draft of
    /// this change was** (review round 1). The population is a RECORDED list —
    /// the panes that were live on the session when `drive_review` read it —
    /// so a pane the orchestrator opens or resumes while the drive runs is
    /// simply not in it, including one opened inside the single tick between
    /// `gate-check` and the exit, where a fresh pane reads as idle and the
    /// barrier would otherwise take it from whoever had just started speaking
    /// to it. [`admit_session_pane`] states the one further exclusion the
    /// caller applies: a pane another live drive's record names is that
    /// drive's to release, since `already-driven` is keyed on the PR and one
    /// worker session may legally back two driven PRs.
    ///
    /// And it widens only the POPULATION. Whether any one of those panes may go
    /// is still the caller's barrier — idle, alive, bound to a terminal, not a
    /// fixture role — asked per pane, so a busy pane is skipped here exactly as
    /// a busy owned one is.
    pub include_founding: bool,
}

/// **May a pane that is merely ON the drive's worker session join the terminal
/// release's population?** (#3250, narrowed in review round 1.)
///
/// [`DriveEntry::founding_panes`] is the population; this is the whole of what
/// joining it excludes, spelled as one pure predicate rather than as conditions
/// in the caller's loop — the caller can then only get it wrong by not calling
/// this, which is visible, instead of by drifting one condition, which is not.
///
/// Three reasons to answer `false`:
///
/// - **an empty id**, which names no pane.
/// - **`already`** — the panes the drive OWNS, already in the population. One
///   pane, one barrier question, one audit row.
/// - **`owned_elsewhere`** — the panes any OTHER live drive OWNS, which is that
///   drive's [`owned_panes`](DriveEntry::owned_panes) and nothing else: the
///   panes it opened, took over or superseded. `already-driven` is keyed on the
///   PR and not on the session, so one worker session may legally back two
///   driven PRs, and a pane the other drive owns is a pane it is going to speak
///   to again — the exact claim this release rests on, broken by the release
///   itself (review round 1, finding 1).
///
///   **It does NOT cover the other drive's own founding list, and that is a
///   stated residual rather than an oversight** (review round 2). Two drives
///   started on one session name the same founding pane, so each considers it
///   its own and whichever reaches a terminal step first releases it. Excluding
///   it on both sides would trade that for a pane neither drive ever releases —
///   the leak this issue is about — so the release is left where it is and the
///   consequence is written down: the other drive's next hand-back re-opens the
///   conversation with `spawn_agent(resume:)`, which is the recovery the whole
///   release rests on and is exactly what it would have done for a pane the
///   human had killed.
///
/// **Deliberately NOT a comparison of timestamps.** The first draft admitted a
/// session pane if it was older than the drive, which reads well and is
/// untestable: an agent's `started_ms` is the wall clock while a drive's is the
/// caller's injected `now`, so under any synthetic clock the two are on
/// different scales — the same divergence that kept `drive-stalled` from ever
/// firing in a test before #2811 B2. A recorded list answers the same question
/// with no clock in it at all.
pub fn admit_session_pane(
    agent: &str,
    already: &[String],
    owned_elsewhere: &[String],
) -> bool {
    !agent.trim().is_empty()
        && !already.iter().any(|a| a == agent)
        && !owned_elsewhere.iter().any(|a| a == agent)
}

/// **The terminal release's worker population: the panes this drive owns plus
/// the panes it was started on, oldest first** (#3250, review round 2).
///
/// The merge and the sort are here rather than in the caller's loop because the
/// ORDER is a claim two comments make and only the merged list can keep. Every
/// member of `owned` was minted by a hand-back, so each one post-dates the pane
/// the drive was started on: appending `founding` to it puts the audit rows
/// newest-first-then-oldest, the inverse of "the rows read as the history they
/// are". The caller supplies `started_ms` because a pane's age is the
/// registry's fact and this crate is Tauri-free — the same injection
/// [`DriveEntry::forget_dead_panes`] takes for liveness.
///
/// A pane the registry cannot date sorts LAST rather than first: it is a pane
/// that is already gone or was never on the roster, so the barrier will refuse
/// it, and putting it at the head would place a row that never happens in front
/// of ones that do. Ties break on the id, so two panes registered inside one
/// wall-clock millisecond cannot order differently between runs.
///
/// Only the founding half is filtered — `owned` is the drive's own record and
/// is already deduplicated by [`retain_panes`] — and the filtering is
/// [`admit_session_pane`], asked once per founding pane.
pub fn release_population(
    owned: Vec<String>,
    founding: &[String],
    owned_elsewhere: &[String],
    started_ms: &dyn Fn(&str) -> Option<u64>,
) -> Vec<String> {
    let mut out = owned;
    for a in founding {
        if admit_session_pane(a, &out, owned_elsewhere) {
            out.push(a.clone());
        }
    }
    out.sort_by_key(|a| (started_ms(a).unwrap_or(u64::MAX), a.clone()));
    out
}

/// **The panes this drive no longer needs, at this tick's facts and this tick's
/// step** (#2501) — the drive-side half of §3.1 item 5's narrowing.
///
/// Pure, and separate from [`decide`] rather than folded into it, because a
/// release is **not an arc**: it moves no state, spends no counter and appears
/// in no row of §2.1's table. It is the same shape as [`DriveStep::OpenLane`]
/// being "not a transition" — a side effect the tick performs beside the step,
/// not instead of one.
///
/// # The three conditions, and what each excludes
///
/// **1. The step must not PARK the drive.** An `Advance` into `held` releases
/// nothing, and that is not caution — it is what keeps §6's hold notices true: a
/// parked drive's notice says its panes are "still running" and that a
/// `drive_review` resume speaks to them again
/// ([`PaneStanding::Owned`](crate::rddrive::PaneStanding::Owned)), which is a
/// promise about panes the drive still means to use.
///
/// **A TERMINAL step is the opposite case and releases MORE, not less** (#2811 S1).
/// It used to be folded in with `held` here, on the reading that a terminal
/// notice "hands the panes to the orchestrator to dispose of". Measured, that
/// hand-off is a bill: the orchestrator killed the reporting worker in the same
/// second it started 4 of one session's 16 drives, and the next hand-back then
/// resumed the session into a FRESH pane — a spawn per round that a released
/// pane would not have needed. Nothing is being waited for at a terminal step by
/// definition, so both rules below are evaluated and their panes go; what the
/// notice then names is whatever the barrier refused, which is exactly the list
/// an orchestrator still has something to do about.
///
/// A lane released on an EARLIER tick is simply not in
/// [`DriveEntry::owned_panes`] any more, so the notice keeps naming exactly the
/// panes that are still there.
///
/// **This is a condition on the STEP, and the drive can still end the tick
/// parked despite it** (rev-final W1). The step is what `decide` proposed; the
/// tick's ARM can then refuse and park on its own — `Advance{to: FixWait}` whose
/// `rd_handback` cannot resume the worker becomes `held(worker-unresumable)` or
/// `held(cap-refused)` — and by then the release has happened, because the tick
/// performs it BEFORE the arm. Saying "a parking DRIVE releases nothing" would
/// be false there, so it is not said anywhere; what holds is the step.
///
/// **Nothing is lost by the narrower reading, and one thing is gained.** The
/// notices are built after the release, off the live record, so a hold's line
/// still names only panes that are actually there. And releasing before the arm
/// is what stops the commonest way that hold was reached at all: the freed slot
/// is available to `rd_handback`'s own spawn, so a drive whose lane has just
/// answered no longer hits the live-delegate cap handing the fix back.
///
/// **2. A lane's verdict must be CURRENT.** Asked with
/// [`lane_verdict_is_current`] — the same function `review-wait` decides with,
/// never a second implementation of it — over the lane facts this tick READ, so
/// a verdict recorded three commits ago releases nothing. A head that could not
/// be read is empty, `lane_verdict_is_current` is false for it, and the tick
/// releases nothing at all: "we could not check" is not "it has answered".
///
/// **3. The worker must have REPORTED into a drive that is waiting on it — or
/// the drive must be over.** `WorkerSignal::Done` and no other word: `Blocked`
/// is INVARIANT 3 territory and parks the drive for the orchestrator, which is a
/// pane a human is about to talk to. `Silent` has reported nothing at all. And
/// the state condition is what makes "the driver has consumed the report" a fact
/// rather than a description — the signal is one-shot, cleared when the arc it
/// fed is durable, so the tick that sees `Done` while a hand-back is
/// outstanding **and takes an arc on it** is the tick that consumed it. That last clause is load-bearing in `ci-wait`
/// and in `ci-wait` only: there a `Done` can arrive before the matrix settles,
/// [`decide_ci_wait`] answers `Wait`, and nothing has been consumed yet — so
/// the pane is kept for the tick that really does end the wait. In `fix-wait`
/// every `Done` takes an arc (2, or 7 when the head moved under it), so the
/// clause changes nothing there and #2501's rows are unmoved.
///
/// **"Waiting on it" is [`DriveEntry::handback_outstanding`], which since #2168
/// E1 spans TWO states** (#2811 S1). This condition used to read `fix-wait` alone,
/// and E1 moved the report's consumption into `ci-wait` for the ordinary
/// push-then-report ordering — so for every hand-back that actually pushed code,
/// the condition and its consumer never met. Measured on one 6.4-hour session:
/// all 5 releases were body-only fixes, the other 15 of 20 hand-backs held a
/// live-delegate slot through ci-wait and the next review round, and 33 of the
/// orchestrator's pane kills were doing this by hand. The predicate is asked
/// once, on `DriveEntry`, because [`DriveEntry::kickback_owed`] needs the same
/// answer and two spellings of it is how the first drift happened.
///
/// **At a terminal step the word is not read at all** and the reason is
/// [`ReleaseReason::DriveEnded`]: nothing is outstanding, so there is no report
/// to consume and no wait to end. What is checked there is the mirror image —
/// that a hand-back is NOT outstanding, so a `cancelled` that arrives while the
/// worker is mid-round leaves that pane alone.
///
/// # What this deliberately does NOT release
///
/// A lane that has been briefed and has not answered; a lane whose verdict binds
/// to an older revision; a worker that reported `blocked` or has said nothing;
/// a worker still owed a round by a drive that is being `cancelled` under it;
/// a pane belonging to a drive whose STEP parks it this tick (see condition 1
/// for why that is not the same as "a drive that parks"); and every pane in the
/// group that is not this drive's. The orchestrator's kill authority is
/// untouched by all of it — this narrows what the DRIVER may do, and adds
/// nothing anywhere else.
///
/// # The residual, stated because a narrowing must state one
///
/// The worker rule still fires on ONE tick — the arc out of the wait, from
/// either of its two states — and the caller's own half of the decision can
/// refuse it there: a pane still finishing its turn is not idle, and an idle
/// check that says "no" is not retried, because the next tick is no longer
/// waiting on that worker and the fact has expired. What #2811 S1 changes is only
/// WHICH tick that is, not that there is one. The refused case costs exactly
/// what it cost before #2501 (the pane holds its slot until the next hand-back
/// reuses it, per #1960) — and now, additionally, until the drive ENDS, where
/// the terminal rule asks once more without the report's help. So the failure
/// direction is the old behaviour, bounded one exit sooner than it used to be.
///
/// **The tool cancel is not a terminal STEP and is outside all of this.**
/// `cancel_review_drive` ends a drive without a tick, so no `decide` runs, no
/// `releasable` is asked, and its notice names every owned pane as
/// [`PaneStanding::Released`] exactly as it always did. That is deliberate
/// rather than an oversight: the orchestrator that called the tool is the party
/// disposing of the panes, it is awake in the turn that called it, and the
/// notice it gets back is the list. The measured cost this slice is about is
/// paid by drives the DRIVER ends while the orchestrator is doing something
/// else.
///
/// **A worker session two drives share is the one case where a release reaches a
/// pane that is not only this drive's**, and it is disclosed rather than guarded
/// against, because the guard would have to be a claim about another drive's
/// intentions. Nothing stops two `drive_review` calls on different PRs naming one
/// worker session, and after both hand back they name one pane; the `report(done)`
/// that arrives is consumed by whichever drive `rd_owner` matches, so exactly one
/// of them releases it. The other learns on its next tick — `rd_pane_exit` reads
/// the death and names the initiator (`ended by driver-release`), so it parks
/// `held(worker-unresumable)` with a truthful line instead of waiting out
/// `fix-stalled`. That is faster and more informative than the pre-#2501
/// behaviour on the same fixture, where the second drive sat ninety minutes for a
/// report it was never going to be handed; what it is NOT is free, and a repo
/// that drives two PRs off one worker session is already the #338/#359 shape the
/// exit notices warn about.
///
/// The lane rule retries instead of expiring — it is a standing property of the
/// tick's facts — but it is **bounded by which states read those facts**, and
/// that bound is worth naming rather than leaving to be discovered. `facts.
/// required_lanes` is fetched only in `review-wait` and `gate-check`; §2.4 pays
/// for the routed-file list in exactly the states that consult it and nowhere
/// else. So a lane whose pane was still writing when its verdict landed is
/// released on the next tick the drive spends in one of those two states —
/// immediately, while another required lane is outstanding, and otherwise on the
/// drive's next review round. A one-lane gate whose only lane was busy on the
/// tick that read its pass therefore keeps that pane to the exit, where §6's
/// notice names it for the orchestrator exactly as it always did.
pub fn releasable(
    entry: &DriveEntry,
    facts: &DriveFacts,
    step: &DriveStep,
) -> Vec<ReleaseCandidate> {
    // Condition 1, and since #2811 S1 it is about PARKING alone — which is not the
    // same as "a drive that parks": the arm can refuse and park after this has
    // answered. See the doc above (rev-final W1).
    let mut terminal = false;
    // **Whether this tick takes an ARC at all**, which is what makes "the driver
    // consumed the report" a fact rather than a description: `rdtick` clears the
    // one-shot signal on every arc and on nothing else, so a tick that merely
    // WAITS has consumed nothing. It is reachable with a `Done` in hand — green
    // has not landed yet, so `decide_ci_wait` waits — and releasing there would
    // free the pane one tick before the drive stopped needing it, on a claim
    // (`report-consumed`) that was not yet true.
    let mut advancing = false;
    if let DriveStep::Advance { to, .. } = step {
        if to.is_parked() {
            return Vec::new();
        }
        terminal = to.is_terminal();
        advancing = true;
    }
    let mut out: Vec<ReleaseCandidate> = Vec::new();
    // Condition 3, first, so the list reads worker-first exactly as
    // `owned_panes` does.
    // **The guard is "this drive has a worker side", not "this drive resumed a
    // pane"** (#3250). Keying the whole condition on `worker_agent` made the
    // terminal rule unreachable for every drive that never handed back — the
    // exact drives the rule is most needed on, since a drive that DID hand back
    // has usually released that pane on the report already. The session is what
    // a drive always has: `drive_review` refuses without one (§5.1).
    if !entry.worker_agent.is_empty() || !entry.worker_session.trim().is_empty() {
        if terminal {
            // The drive is over. The only thing that keeps its worker pane is a
            // round still outstanding — a `cancelled` that arrived while the
            // worker was mid-fix — and `handback_outstanding` is the same
            // question asked the same way one branch down.
            if !entry.handback_outstanding() {
                out.push(ReleaseCandidate {
                    role: DrivenRole::Worker,
                    reason: ReleaseReason::DriveEnded,
                    // Both terminal steps: see the field's own doc for why the
                    // two exits cannot differ here.
                    include_founding: true,
                });
            }
        } else if !entry.worker_agent.is_empty()
            && advancing
            && entry.handback_outstanding()
            && facts.worker == WorkerSignal::Done
        {
            out.push(ReleaseCandidate {
                role: DrivenRole::Worker,
                reason: ReleaseReason::ReportConsumed,
                include_founding: false,
            });
        }
    }
    // Condition 2. `required_lanes` is `None` in the states that do not read it
    // and when the routing could not be computed at all; both are "we do not
    // know which lanes are required", and neither is a licence to release one.
    let digest = facts.body_digest.as_deref();
    for l in facts.required_lanes.iter().flatten() {
        if !lane_verdict_is_current(l.verdict.as_ref(), &facts.head, digest) {
            continue;
        }
        // A lane with no record, or one whose record carries no pane, has
        // nothing to release — including one released on an earlier tick, whose
        // pane slot is empty precisely so this stays true.
        match entry.lane(&l.block) {
            Some(rec) if !rec.agent.trim().is_empty() => {}
            _ => continue,
        }
        out.push(ReleaseCandidate {
            role: DrivenRole::Lane(l.block.clone()),
            reason: ReleaseReason::VerdictRecorded,
            include_founding: false,
        });
    }
    // **Condition 4: the PR does not merge, so every open lane is reviewing a
    // head that is about to be rebased away** (#3176).
    //
    // Keyed on `facts.ci` and not on the STEP, and that is the difference
    // between a fix and a coin flip. #2311's hoist takes arc 3 on the first tick
    // that observes the conflict, and on that tick the lane's pane is whatever it
    // happened to be doing — mid-turn as often as not, which
    // `release_driven_pane`'s idle barrier refuses (§3.1 item 5). Asked of the
    // FACTS, the rule is a standing property the way condition 2's is: it is
    // re-asked on every later tick the drive spends waiting out the same
    // conflict, so the pane goes on the first tick it is between turns instead of
    // on the one tick that took the arc. It is bounded by the conflict itself —
    // the worker's rebase moves the head, mergeability clears, and the rule stops
    // matching — and by `rebase-limit` / `fix-stalled` under it.
    //
    // Conditions 1's exclusions still apply above: a step that PARKS the drive
    // returned empty, so a `held(rebase-limit)` conflict releases nothing and §6's
    // notice keeps naming panes that are really there. A TERMINAL step is excluded
    // here rather than there — `satisfied` cannot be reached on a conflict since
    // #2311, and at `cancelled` the panes are the orchestrator's to dispose of,
    // named on the way out.
    //
    // **The lane that has answered is not this variant's.** It is condition 2's
    // when the routing could be read, and the ordinary stale-verdict handling's
    // when it could not; either way its pane is finished, and a `conflict` row on
    // it would be the false claim [`ReleaseReason::Conflict`] exists to avoid. The
    // record is what answers, because `facts.required_lanes` is `None` in exactly
    // the states a conflicted PR is usually observed in — GitHub computes no
    // changed-file list for a head that does not merge, which is #2311's own
    // measurement — so a rule that could only read `facts` would fire nowhere it
    // mattered. `LaneRecord::at_head` is a record of what the drive READ rather
    // than a gate input, and it is read here only to DECLINE a release: it can
    // cost a slot, never a review.
    if !terminal && facts.ci == CiObservation::Conflicting {
        for l in &entry.lanes {
            if l.agent.trim().is_empty() {
                continue;
            }
            let role = DrivenRole::Lane(l.block.clone());
            if out.iter().any(|c| c.role == role) {
                continue;
            }
            let answered_here = l.last_verdict.is_some()
                && !l.at_head.is_empty()
                && l.at_head == facts.head;
            if answered_here {
                continue;
            }
            out.push(ReleaseCandidate {
                role,
                reason: ReleaseReason::Conflict,
                include_founding: false,
            });
        }
    }
    out
}

fn minutes_ms(minutes: u64) -> u64 {
    minutes.saturating_mul(60_000)
}

#[cfg(test)]
mod tests {
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

    fn entry_at(state: DriveState) -> DriveEntry {
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

    // ── §2.4 the decision ───────────────────────────────────────────────────

    fn facts_at(head: &str) -> DriveFacts {
        DriveFacts {
            now_ms: 2_000,
            pr_open: Some(true),
            head: head.to_string(),
            body_digest: Some("d1".to_string()),
            required_lanes: Some(Vec::new()),
            ci: CiObservation::Pending,
            worker: WorkerSignal::Silent,
            gate: GateOutcome::NotEvaluated,
            messaged: false,
            provider_limited: None,
            restart_handback: false,
            restart_push_delivered: false,
        }
    }

    fn lane_fact(block: &str, v: Option<Verdict>, at_head: &str, digest: &str) -> LaneFact {
        LaneFact {
            block: block.to_string(),
            verdict: v.map(|verdict| ReviewVerdict {
                pr: 1758,
                block: block.to_string(),
                agent_id: "rev-1".into(),
                verdict,
                head: at_head.to_string(),
                body_digest: digest.to_string(),
                verified_body: false,
                open_findings: None,
                summary: String::new(),
                ts_ms: 0,
            }),
            // The overwhelmingly common reading, so it is the helper's default
            // and `pane_dead` is set by the one test that is about it (#2163).
            pane_dead: false,
        }
    }

    /// [`lane_fact`] whose `pass` is the driver's body-VERIFICATION delta
    /// (#2168 E2) — the mark `review_verdict` writes for a lane the driver
    /// briefed because only the body had moved.
    fn verified_lane_fact(block: &str, at_head: &str, digest: &str) -> LaneFact {
        let mut l = lane_fact(block, Some(Verdict::Pass), at_head, digest);
        if let Some(v) = l.verdict.as_mut() {
            v.verified_body = true;
        }
        l
    }

    #[test]
    fn the_tick_never_advances_a_parked_or_terminal_drive() {
        // §2.1's `held` row: "nothing; the tick does not advance it". If the
        // tick could move one, `held` would not be a park — and the two tools
        // would not be its only ways out.
        let limits = DriveLimits::default();
        for st in [DriveState::Held, DriveState::Satisfied, DriveState::Cancelled] {
            let e = entry_at(st);
            // Facts that would move any working state, so this is not passing
            // because there was nothing to do.
            let facts = DriveFacts {
                ci: CiObservation::Green,
                gate: GateOutcome::Satisfied,
                worker: WorkerSignal::Blocked,
                messaged: true,
                now_ms: 10_000_000_000,
                ..facts_at("head-a")
            };
            assert_eq!(decide(&e, &facts, &limits), DriveStep::Wait, "{}", st.as_str());
        }
    }

    #[test]
    fn only_a_positive_answer_cancels_a_drive() {
        // §8: cancelling a live drive on a rate limit that clears in minutes is
        // the failure this distinction exists to stop. `None` is "the world
        // does not match", never "probably fine".
        let limits = DriveLimits::default();
        let e = entry_at(DriveState::CiWait);
        let closed = DriveFacts { pr_open: Some(false), ..facts_at("head-a") };
        assert_eq!(decide(&e, &closed, &limits), DriveStep::to(DriveState::Cancelled));
        let unknown = DriveFacts { pr_open: None, ..facts_at("head-a") };
        assert_eq!(decide(&e, &unknown, &limits), DriveStep::Wait);
    }

    #[test]
    fn a_delegates_message_parks_the_drive() {
        // §7: `message_orchestrator` is never intercepted, so the delegate's
        // own line is already in the pane; this hold is the routing fact.
        let limits = DriveLimits::default();
        let e = entry_at(DriveState::ReviewWait);
        let facts = DriveFacts { messaged: true, ..facts_at("head-a") };
        assert_eq!(decide(&e, &facts, &limits), DriveStep::held(HeldReason::Messaged));
    }

    #[test]
    fn the_age_bound_outranks_an_available_advance() {
        // §8's `also: [base-green]` row is the worked example: that drive
        // cycles gate-check -> ci-wait on EVERY wake, so it always has an
        // advance available. An age check that ran after the per-state logic
        // would never be reached and the drive would cycle forever — which is
        // exactly the silent park the bound exists to prevent.
        let limits = DriveLimits::default();
        let e = entry_at(DriveState::GateCheck);
        let cycling = DriveFacts {
            gate: GateOutcome::Unsatisfied,
            ..facts_at("head-a")
        };
        // Young: it takes arc 10, as that row describes.
        assert_eq!(
            decide(&e, &cycling, &limits),
            DriveStep::to(DriveState::CiWait)
        );
        // Aged past the bound: parked, despite that arc still being available.
        let aged = DriveFacts {
            now_ms: e.started_ms + minutes_ms(limits.drive_timeout_minutes),
            ..cycling
        };
        assert_eq!(
            decide(&e, &aged, &limits),
            DriveStep::held(HeldReason::DriveStalled)
        );
    }

    #[test]
    fn ci_wait_reads_its_four_answers_and_waits_on_the_two_unknowns() {
        let limits = DriveLimits::default();
        let e = entry_at(DriveState::CiWait);
        let with = |ci| decide(&e, &DriveFacts { ci, ..facts_at("head-a") }, &limits);
        // Arc 2.
        assert_eq!(with(CiObservation::Green), DriveStep::to(DriveState::ReviewWait));
        // Arc 3, on each of its two counters.
        assert_eq!(
            with(CiObservation::Red),
            DriveStep::spend(DriveState::FixWait, Counter::CiAttempts)
        );
        assert_eq!(
            with(CiObservation::Conflicting),
            DriveStep::spend(DriveState::FixWait, Counter::RebaseAttempts)
        );
        // Neither an answer nor a reason to move: "unknown is never treated as
        // safe", and it is never treated as a fact about the PR either.
        assert_eq!(with(CiObservation::Pending), DriveStep::Wait);
        assert_eq!(with(CiObservation::Unknown), DriveStep::Wait);
    }

    // ── #2168 E1: green at a pushed head is not on its own an arc ───────────

    /// A drive that reached `ci-wait` the way arc 7 does: hand back, then the
    /// worker pushes. Walked through the real arcs rather than assembled, so a
    /// fixture cannot encode a `fix_pushed_ms` the machine would not stamp.
    fn pushed_fix_entry() -> DriveEntry {
        let mut e = entry_at(DriveState::ReviewWait);
        // Arc 5, the hand-back — this is what stamps `fix_handback_ms`.
        e.advance(DriveState::FixWait, None, Some(Counter::ReviewRounds), 10_000).unwrap();
        // Arc 7, the push. `state_since_ms` is now this moment, and it is the
        // anchor the receipts wait is measured from.
        e.advance(DriveState::CiWait, None, None, 100_000).unwrap();
        e
    }

    /// **The defect, and the test that is red without the guard** (#1875,
    /// #2168 E1). A worker fills the PR body's CI section only once the checks
    /// have settled, so a lane briefed on green alone is briefed at a digest
    /// the worker is about to move — and the `pass` it records is stale before
    /// it is written. #1870 is the measured instance: `pass` at digest
    /// `bbff76b8`, 0 findings, the CI section filled, `BODY CHANGED SINCE
    /// PASS`, gate blocked, re-record, with the head never having moved.
    #[test]
    fn green_at_a_pushed_head_waits_for_the_workers_report_before_it_briefs_a_lane() {
        let limits = DriveLimits::default();
        let e = pushed_fix_entry();
        assert!(e.fix_pushed(), "the pre-state: arc 7 is what this test is about");
        let green_and_silent = DriveFacts {
            ci: CiObservation::Green,
            worker: WorkerSignal::Silent,
            ..facts_at("head-b")
        };
        assert_eq!(
            decide(&e, &green_and_silent, &limits),
            DriveStep::Wait,
            "green is the checks settling, not the round ending — the worker has not \
             reported, so the body may still move under whatever lane this briefs"
        );
        // And the arc it is waiting FOR, so this is not "refuse everything".
        let green_and_done = DriveFacts { worker: WorkerSignal::Done, ..green_and_silent };
        assert_eq!(
            decide(&e, &green_and_done, &limits),
            DriveStep::to(DriveState::ReviewWait),
            "arc 2 fires once the revision has stopped moving"
        );
    }

    /// The negative control, and the property the brief calls out by name: a
    /// drive that never handed anything back advances on green as it always
    /// did. `drive_review` is called after the worker has already reported, so
    /// there is no second report to wait for and waiting would park every first
    /// drive on a signal nobody was asked for.
    #[test]
    fn green_on_a_head_this_drive_never_handed_back_still_advances_on_its_own() {
        let limits = DriveLimits::default();
        for e in [entry_at(DriveState::CiWait), gate_check_recycled_entry()] {
            assert!(!e.fix_pushed(), "neither of these reached `ci-wait` by arc 7");
            let facts = DriveFacts {
                ci: CiObservation::Green,
                worker: WorkerSignal::Silent,
                ..facts_at("head-a")
            };
            assert_eq!(
                decide(&e, &facts, &limits),
                DriveStep::to(DriveState::ReviewWait),
                "a drive with no outstanding hand-back has no report to wait for"
            );
        }
    }

    /// A drive that went out to `gate-check` and came back by arc 10 — one of
    /// the three non-arc-7 ways into `ci-wait`, and the one that would still be
    /// carrying the flag if `advance` merely SET it instead of assigning it.
    fn gate_check_recycled_entry() -> DriveEntry {
        let mut e = pushed_fix_entry();
        e.advance(DriveState::ReviewWait, None, None, 200_000).unwrap();
        e.advance(DriveState::GateCheck, None, None, 200_000).unwrap();
        // Arc 10: not satisfied, back to `ci-wait`.
        e.advance(DriveState::CiWait, None, None, 200_000).unwrap();
        e
    }

    /// `fix_pushed_ms` is assigned on every arc, not set on one — pinned across
    /// each way into and out of `ci-wait`, because the failure mode of a set is
    /// invisible: the flag survives, the drive waits for a `report(done)` its
    /// worker was never asked for, and it parks `fix-stalled` an hour later
    /// naming a worker that did nothing wrong.
    #[test]
    fn only_arc_seven_marks_a_head_as_one_the_worker_pushed() {
        // Arc 1: the creation.
        assert!(!DriveEntry::new(1, "s", "o", Counters::default(), 0).fix_pushed());
        // Arc 7 sets it…
        let mut e = pushed_fix_entry();
        assert!(e.fix_pushed(), "arc 7");
        // …arc 2 out of `ci-wait` clears it…
        e.advance(DriveState::ReviewWait, None, None, 110_000).unwrap();
        assert!(!e.fix_pushed(), "arc 2 leaves `ci-wait`, so the push is spent");
        // …and arc 6 back INTO `ci-wait` does not re-set it.
        e.advance(DriveState::CiWait, None, None, 120_000).unwrap();
        assert!(!e.fix_pushed(), "arc 6 is a push this drive did not hand back for");
        // Arc 10, the same question from `gate-check`.
        assert!(!gate_check_recycled_entry().fix_pushed(), "arc 10");
        // Arc 11, a resume out of a park — the drive that was waiting on a
        // report before it was held is not still waiting on one after.
        let mut held = pushed_fix_entry();
        held.advance(DriveState::Held, Some(HeldReason::FixStalled), None, 300_000).unwrap();
        held.advance(DriveState::CiWait, None, None, 400_000).unwrap();
        assert!(!held.fix_pushed(), "arc 11");
    }

    /// The bound, and **which clock it is measured on**. `fix_handback_ms` is
    /// the hand-back, which predates the push: measured from there, a worker
    /// that spent fifty-nine minutes writing the fix would get one minute to
    /// read a green matrix and report. The anchor is `state_since_ms`, which
    /// is the LATEST push in this `ci-wait` stay, which is what `fix_pushed_ms`
    /// carries and `state_since_ms` cannot.
    #[test]
    fn a_silent_worker_on_a_pushed_head_parks_fix_stalled_a_fix_timeout_after_the_push() {
        let limits = DriveLimits::default();
        let e = pushed_fix_entry();
        assert!(
            e.state_since_ms > e.fix_handback_ms,
            "the fixture's whole point: the two anchors are 90s apart, so a test that \
             read the wrong one cannot pass by coincidence"
        );
        let at = |now_ms| {
            decide(
                &e,
                &DriveFacts {
                    now_ms,
                    ci: CiObservation::Green,
                    worker: WorkerSignal::Silent,
                    ..facts_at("head-b")
                },
                &limits,
            )
        };
        let timeout = minutes_ms(limits.fix_timeout_minutes);
        assert_eq!(at(e.state_since_ms + timeout - 1), DriveStep::Wait, "one ms inside");
        assert_eq!(
            at(e.state_since_ms + timeout),
            DriveStep::held(HeldReason::FixStalled),
            "and the bound itself"
        );
        // The discriminator: at a `now` that is past the timeout measured from
        // the HAND-BACK but not from the push, the drive is still waiting.
        assert_eq!(
            at(e.fix_handback_ms + timeout),
            DriveStep::Wait,
            "measured from `fix_handback_ms` this would already have parked"
        );
    }

    /// The other two worker signals, answered by their own names rather than
    /// waited out. Letting either fall through to the timeout would report a
    /// worker that SAID something as one that went silent — and would cost an
    /// hour before saying even that.
    #[test]
    fn the_pushed_head_wait_reads_every_worker_signal_by_the_same_rule_fix_wait_does() {
        let limits = DriveLimits::default();
        let e = pushed_fix_entry();
        for (worker, want) in [
            (WorkerSignal::Blocked, HeldReason::WorkerBlocked),
            (WorkerSignal::Unresumable, HeldReason::WorkerUnresumable),
        ] {
            let facts =
                DriveFacts { ci: CiObservation::Green, worker, ..facts_at("head-b") };
            assert_eq!(
                decide(&e, &facts, &limits),
                DriveStep::held(want),
                "{worker:?} is answered here exactly as `decide_fix_wait` answers it"
            );
            // …and still by its own name once the clock HAS run out, which is
            // the discriminator: read below the timeout instead of above it,
            // both of these would report `fix-stalled` — a claim that the
            // worker went silent, about a worker that said something.
            let expired = DriveFacts {
                now_ms: e.state_since_ms + minutes_ms(limits.fix_timeout_minutes) + 1,
                ..facts
            };
            assert_eq!(
                decide(&e, &expired, &limits),
                DriveStep::held(want),
                "{worker:?} is not reported as `fix-stalled` once the wait expires"
            );
        }
    }

    /// The guard is on the GREEN arm alone. A red or a conflicting matrix at a
    /// pushed head still hands the worker back through arc 3, where
    /// `decide_fix_wait`'s ladder answers, and the two unknowns wait as they
    /// always did. Without this, the receipts wait would swallow the arc that
    /// tells the worker its fix failed.
    #[test]
    fn a_pushed_head_that_is_not_green_takes_the_arcs_it_always_did() {
        let limits = DriveLimits::default();
        let e = pushed_fix_entry();
        let with = |ci| {
            decide(
                &e,
                &DriveFacts { ci, worker: WorkerSignal::Silent, ..facts_at("head-b") },
                &limits,
            )
        };
        assert_eq!(
            with(CiObservation::Red),
            DriveStep::spend(DriveState::FixWait, Counter::CiAttempts)
        );
        assert_eq!(
            with(CiObservation::Conflicting),
            DriveStep::spend(DriveState::FixWait, Counter::RebaseAttempts)
        );
        assert_eq!(with(CiObservation::Pending), DriveStep::Wait);
        assert_eq!(with(CiObservation::Unknown), DriveStep::Wait);
    }

    /// **`state-stalled` may not preempt `fix-stalled`, at any configuration**
    /// — the property `HeldReason::StateStalled` states, and the one a `max`
    /// would have broken. `decide` reads the state bound ABOVE the state's own
    /// logic, so at `fix_timeout_minutes` above `CI_WAIT_BOUND_MS` a bound
    /// equal to the knob would answer first on the very tick the receipts wait
    /// expires, and the hold naming the pane to read would never fire at all.
    ///
    /// The two knobs are the two sides of the constant: 60 (the stock value,
    /// under it) and 240 (over it, which is where a `max` and an add differ —
    /// and the CEILING, since `parse_workflow` runs this knob through
    /// `clamp_expires_minutes`, so no workflow file can ask for more). At that
    /// ceiling the sum is 330 minutes, comfortably inside the twelve-hour
    /// backstop, which is why this arm needs no counterpart to `review-wait`'s
    /// overtake residual — that one is unbounded in the LANE COUNT, and this
    /// one has a single clamped term.
    #[test]
    fn the_ci_wait_bound_funds_the_receipts_wait_instead_of_preempting_it() {
        for minutes in [60u64, 240] {
            let limits = DriveLimits { fix_timeout_minutes: minutes, ..DriveLimits::default() };
            let bound = state_bound_ms(DriveState::CiWait, &limits, 1).unwrap();
            assert_eq!(
                bound,
                CI_WAIT_BOUND_MS + minutes_ms(minutes),
                "the constant is the SLACK over the receipts wait, not a rival to it"
            );
            let e = pushed_fix_entry();
            let facts = DriveFacts {
                now_ms: e.state_since_ms + minutes_ms(minutes),
                ci: CiObservation::Green,
                worker: WorkerSignal::Silent,
                ..facts_at("head-b")
            };
            assert_eq!(
                decide(&e, &facts, &limits),
                DriveStep::held(HeldReason::FixStalled),
                "at fix_timeout_minutes={minutes} the wait-specific hold is what fires; \
                 under `max(constant, knob)` this is `state-stalled` at 240"
            );
            assert!(
                bound < minutes_ms(limits.drive_timeout_minutes),
                "…and the sum stays inside the backstop at the ceiling the workflow \
                 parser clamps to, so this arm owes no overtake residual"
            );
        }
    }

    /// **A follow-up push mid-wait gets a whole window, not the remainder of
    /// one** (rev-final round 2, premortem 2). `transition` refuses a `ci-wait`
    /// -> `ci-wait` self-arc, so nothing re-stamps `state_since_ms` and a
    /// `bool` bounded from it would run the wait from the FIRST push: a commit
    /// landing at minute 55 of a 60-minute knob would leave five minutes to run
    /// a fresh matrix and report.
    ///
    /// The three assertions are the defect, the fix and the bound it must not
    /// remove — a re-stamp that also pushed `state-stalled` out would be an
    /// unbounded suppression driven by a signal the drive does not control.
    #[test]
    fn a_second_push_inside_one_ci_wait_stay_re_anchors_the_receipts_wait() {
        let limits = DriveLimits::default();
        let timeout = minutes_ms(limits.fix_timeout_minutes);
        let mut e = pushed_fix_entry();
        let first = e.fix_pushed_ms.expect("arc 7 stamps the anchor");
        let silent = |e: &DriveEntry, now_ms| {
            decide(
                e,
                &DriveFacts {
                    now_ms,
                    ci: CiObservation::Green,
                    worker: WorkerSignal::Silent,
                    ..facts_at("head-c")
                },
                &limits,
            )
        };
        // Without the re-stamp this is the park — the defect, stated as the
        // pre-state so the fix below cannot pass vacuously.
        assert_eq!(
            silent(&e, first + timeout),
            DriveStep::held(HeldReason::FixStalled),
            "the pre-state: measured from the first push, the window is spent"
        );

        // The worker pushes again at minute 55, and the tick records it.
        let second = first + timeout - minutes_ms(5);
        e.note_fix_push(second);
        assert_eq!(e.state_since_ms, first, "the STATE clock is deliberately untouched");
        assert_eq!(
            silent(&e, first + timeout),
            DriveStep::Wait,
            "the same instant is now five minutes into a fresh window"
        );
        assert_eq!(
            silent(&e, second + timeout),
            DriveStep::held(HeldReason::FixStalled),
            "…and the fresh window is a whole one, not an unbounded reprieve"
        );

        // The bound the re-stamp may not remove: `state-stalled` measures from
        // `state_since_ms`, so a worker that pushed for ever still parks.
        let forever = first + state_bound_ms(DriveState::CiWait, &limits, 0).unwrap();
        e.note_fix_push(forever);
        assert_eq!(
            silent(&e, forever),
            DriveStep::held(HeldReason::StateStalled),
            "a drive that keeps pushing is still bounded by the state it is sitting in"
        );

        // And it re-stamps only what exists: a head move in a state that never
        // took arc 7 must not manufacture a wait.
        let mut fresh = entry_at(DriveState::CiWait);
        fresh.note_fix_push(9_999);
        assert!(!fresh.fix_pushed(), "note_fix_push is a re-stamp, never an entry point");
    }

    /// **A driven worker's `report(progress)` is answered wherever a hand-back
    /// is outstanding, which E1 makes two states** (rev-final round 2, finding
    /// 1). `kickback_owed` was scoped to `fix-wait` on the argument that "in
    /// any other state there is no hand-back to be waiting on and nothing the
    /// worker was asked for" — which E1 falsified for `ci-wait` on an arc-7
    /// head, where the worker has been handed a round and is being waited on
    /// for the very `report(done)` that line asks it to send.
    ///
    /// Left unswept, #1959 reappears one state over and worse: §7 consumes the
    /// report (its interception is keyed on the agent, not the state), the
    /// orchestrator's pane gets nothing, the worker's pane gets nothing, and a
    /// fix timeout later the hold says the driver heard nothing from a worker
    /// that spoke.
    ///
    /// The `review-wait` row is the control that keeps this from becoming
    /// "answer everywhere": that state has no outstanding hand-back.
    #[test]
    fn a_progress_report_is_owed_an_answer_in_both_states_that_are_waiting_on_the_worker() {
        // `fix-wait`, unchanged — and the pre-state for the arc-7 case below.
        let mut e = entry_at(DriveState::ReviewWait);
        e.advance(DriveState::FixWait, None, Some(Counter::ReviewRounds), 10_000).unwrap();
        assert!(e.kickback_owed(), "#1959's own state, unchanged");

        // Arc 7. The hand-back is still outstanding: the worker was asked to
        // push AND report, and it has done half of that.
        e.advance(DriveState::CiWait, None, None, 100_000).unwrap();
        assert!(e.fix_pushed());
        assert!(
            e.kickback_owed(),
            "the worker is still being waited on, for the report this line asks it to send"
        );

        // The budget is one per HAND-BACK and not one per state: `fix_handback_ms`
        // is not re-stamped by arc 7, so a worker answered before it pushed is
        // not answered again after.
        let mut answered = e.clone();
        answered.record_kickback(20_000);
        assert!(!answered.kickback_owed(), "same round, same answer, once");

        // The control: `ci-wait` on a head this drive never handed back for, and
        // `review-wait`, owe nobody anything.
        let first_drive = entry_at(DriveState::CiWait);
        assert!(!first_drive.fix_pushed());
        assert!(!first_drive.kickback_owed(), "no hand-back is outstanding here");
        let mut reviewing = e.clone();
        reviewing.advance(DriveState::ReviewWait, None, None, 200_000).unwrap();
        assert!(!reviewing.kickback_owed(), "and the lane's state owes the worker nothing");
    }

    /// **The two residuals `decide_fix_receipts` discloses are recoverable by
    /// the remedy their own notice prints**, which §2.2 requires of every hold
    /// whose cause is a wait — and which a hold on a signal that will never
    /// arrive would otherwise fail: a resume that only restarted the clocks
    /// would re-hold an hour later, for ever.
    ///
    /// This is the counterfactual for the disclosure, performed rather than
    /// argued (CLAUDE.md: a documented escape hatch is only pinned by a test
    /// that performs the edit). Arc 11 re-enters `ci-wait` from `held`, so
    /// `advance` assigns `fix_pushed_ms` None — it is the ASSIGNMENT, not a
    /// clock, that makes the resume work, and a `set` would leave the drive
    /// waiting on the same absent report.
    ///
    /// The pre-state is the hold itself, so the test cannot pass by resuming a
    /// drive that was never stuck.
    #[test]
    fn a_resume_out_of_fix_stalled_briefs_on_the_next_green() {
        let limits = DriveLimits::default();
        let mut e = pushed_fix_entry();
        let stalled = DriveFacts {
            now_ms: e.state_since_ms + minutes_ms(limits.fix_timeout_minutes),
            ci: CiObservation::Green,
            worker: WorkerSignal::Silent,
            ..facts_at("head-b")
        };
        assert_eq!(
            decide(&e, &stalled, &limits),
            DriveStep::held(HeldReason::FixStalled),
            "the pre-state: a drive really parked on the wait this test is about"
        );
        e.take(&DriveStep::held(HeldReason::FixStalled), stalled.now_ms).unwrap();

        // Arc 11: `drive_review` resumes it, hours later — a human takes their
        // time over a hold, so the resume must not depend on any clock.
        let resumed_at = stalled.now_ms + minutes_ms(300);
        e.advance(DriveState::CiWait, None, None, resumed_at).unwrap();
        assert!(!e.fix_pushed(), "the resume leaves no outstanding report to wait for");
        assert_eq!(
            decide(
                &e,
                &DriveFacts { now_ms: resumed_at + 1_000, ..stalled },
                &limits
            ),
            DriveStep::to(DriveState::ReviewWait),
            "the very next green briefs, with nothing further asked of a worker that \
             may have said its piece already"
        );
    }

    #[test]
    fn ci_attempts_park_at_their_bound() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::CiWait);
        e.counters.ci_attempts = limits.max_ci_attempts;
        let facts = DriveFacts { ci: CiObservation::Red, ..facts_at("head-a") };
        assert_eq!(decide(&e, &facts, &limits), DriveStep::held(HeldReason::CiLimit));
    }

    #[test]
    fn an_unaccountable_route_parks_from_every_state_that_reads_one() {
        // §4: never a guess. The unknown thing is *which reviewers are
        // required*, so guessing "no rule fired" is guessing in favour of
        // merging — and at `gate-check` that would be a false GATE SATISFIED
        // notice, which §3.1 names "a bypass with better telemetry".
        let limits = DriveLimits::default();
        for st in [DriveState::ReviewWait, DriveState::GateCheck] {
            let mut e = entry_at(st);
            e.head = "head-a".into();
            let facts = DriveFacts {
                required_lanes: None,
                // The gate would say SATISFIED, which is precisely what must
                // not win here.
                gate: GateOutcome::Satisfied,
                ..facts_at("head-a")
            };
            assert_eq!(
                decide(&e, &facts, &limits),
                DriveStep::held(HeldReason::RoutingUnaccountable),
                "{}",
                st.as_str()
            );
        }
    }

    #[test]
    fn review_wait_walks_its_lanes_in_the_gates_order() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        let two_lanes = |first: Option<Verdict>, second: Option<Verdict>| DriveFacts {
            required_lanes: Some(vec![
                lane_fact("rev-std", first, "head-a", "d1"),
                lane_fact("rev-final", second, "head-a", "d1"),
            ]),
            ..facts_at("head-a")
        };
        // Lane 1 not yet briefed: open it.
        assert_eq!(
            decide(&e, &two_lanes(None, None), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false }
        );
        // Lane 1 passed at this (head, digest): move to lane 2 — and note this
        // is NOT a transition, which is why the table has no review-wait arm.
        assert_eq!(
            decide(&e, &two_lanes(Some(Verdict::Pass), None), &limits),
            DriveStep::OpenLane { index: 1, verify: false, body_only: false }
        );
        // Both passed: arc 4.
        assert_eq!(
            decide(
                &e,
                &two_lanes(Some(Verdict::Pass), Some(Verdict::Pass)),
                &limits
            ),
            DriveStep::to(DriveState::GateCheck)
        );
        // Arc 5, and an escalate that §3 refuses to decide.
        assert_eq!(
            decide(&e, &two_lanes(Some(Verdict::Fail), None), &limits),
            DriveStep::spend(DriveState::FixWait, Counter::ReviewRounds)
        );
        assert_eq!(
            decide(&e, &two_lanes(Some(Verdict::Escalate), None), &limits),
            DriveStep::held(HeldReason::Escalate)
        );
    }

    /// **The residual the answered-lane exemption leaves — NARROWED by #2110,
    /// not closed** (#2109 review 2, premortem 1).
    ///
    /// One composition escapes every per-lane bound: a reviewer answers at this
    /// head, the body then moves, and that reviewer's pane goes BUSY on
    /// something else — a human re-tasking it, another drive taking it over. The
    /// re-brief the moved digest calls for cannot be delivered (the reuse arm
    /// needs an idle pane) and must not be spawned beside it (#2109's duplicate
    /// refusal, which #2162 narrowed to exactly this busy case), while
    /// `lane-stalled` is exempt because the lane did answer. So the drive
    /// retries and audits two rows a tick.
    ///
    /// **What #2110 changed is the exit, and only the exit.** #2109 disclosed
    /// this gap and said closing it needs a per-lane refusal clock, "which
    /// belongs with #2110's age work". #2110 did not build that clock: there is
    /// still nothing per-lane here, and the hold that ends this still names no
    /// lane. What it built is a per-STATE bound, and `review-wait` is a state,
    /// so the drive now leaves at the `review-wait` state bound as
    /// `held(state-stalled)` instead
    /// of at twelve as `held(drive-stalled)` — a fraction of the wait, and a
    /// notice that at least says which wait. The honest description of the
    /// residual is therefore *bounded per state, still not per lane*, and §8's
    /// row says exactly that.
    ///
    /// A disclosure is a claim like any other: without this test the suite pins
    /// only the arms that work, and that §8 row could go false with nothing red
    /// to say so. The three halves are the gap itself (past the LANE timeout,
    /// still proposing the re-brief), the non-vacuity control one tick short of
    /// the bound that now ends it, and the bound.
    ///
    /// **The age backstop is deliberately not asserted here any more**, and its
    /// absence is the finding rather than an omission: the narrower bound fires
    /// first, so from `review-wait` the twelve-hour clock is now unreachable for
    /// this composition. Asserting it would be asserting a path the code no
    /// longer takes.
    #[test]
    fn an_answered_lane_whose_re_brief_is_refused_is_bounded_by_the_review_wait_state_bound() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.open_lane("rev-std", "sess", "rev-4", "head-a", Some("d1"), 1_000, false, false);
        assert!(e.record_verdict_seen("rev-std", Verdict::Fail, "head-a"));
        let facts = |now: u64| DriveFacts {
            now_ms: now,
            body_digest: Some("d2".to_string()),
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Fail), "head-a", "d1")]),
            ..facts_at("head-a")
        };

        // The gap: twice the LANE timeout and still inside the state bound, so
        // this keeps proposing the re-brief the tick will refuse. `lane-stalled`
        // never fires here, by design, and nothing per-lane replaces it.
        let past_lane = 1_000 + minutes_ms(limits.lane_timeout_minutes) * 2;
        assert_eq!(
            decide(&e, &facts(past_lane), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: true },
            "the exemption really does leave this composition unbounded PER LANE — if this \
             ever becomes a lane-named hold, the §8 row disclosing the gap is what needs \
             rewriting"
        );

        // The bound that now limits it, and its own non-vacuity control one tick
        // short. Asked of `state_bound_ms` rather than spelled here, because
        // this test is about WHICH bound ends the composition; the bound's own
        // value is pinned against literals by
        // `the_review_wait_floor_exceeds_the_sequential_gate_it_covers`, so the
        // number is not built from the code under test without a witness.
        let bound = state_bound_ms(DriveState::ReviewWait, &limits, 1).unwrap();
        assert!(
            bound > minutes_ms(limits.lane_timeout_minutes),
            "the lane timeout must not be what fires, or the assertions below are \
             about the wrong bound"
        );
        assert!(
            bound < minutes_ms(limits.drive_timeout_minutes),
            "…and neither must the backstop, which outranks it in `decide`"
        );
        assert_eq!(
            decide(&e, &facts(1_000 + bound - 1), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: true },
            "…still inside the state bound, so the hold below is that bound and not a \
             coincidence"
        );
        assert_eq!(
            decide(&e, &facts(1_000 + bound), &limits),
            DriveStep::held(HeldReason::StateStalled),
            "…and time in `review-wait` is what ends it now, where before #2110 nothing \
             but the twelve-hour age could"
        );
    }

    /// **#2153: a re-drive carries the CONVERSATION and nothing that describes a
    /// revision.**
    ///
    /// Every field is asserted, and that is deliberate rather than exhaustive
    /// for its own sake: this record is what the next drive's first tick decides
    /// from, and each field it must NOT carry produces a different wrong answer.
    /// A carried `briefed_head`/`briefed_digest` makes `lane_open_for` true, so
    /// the first tick waits on a brief nobody sent; a carried `last_verdict` and
    /// `at_head` make `lane_has_answered` true, so the delta template describes
    /// a round that is over and the duplicate guard reads the previous drive's
    /// pane as holding this one; a carried `spawned_ms` starts `lane-stalled`
    /// counting from a brief in another drive's lifetime; and a carried `agent`
    /// makes a pane the new drive never spoke to its CURRENT one.
    #[test]
    fn a_reseeded_lane_keeps_its_session_and_its_owned_panes_and_no_claim_about_a_revision() {
        let mut e = entry_at(DriveState::ReviewWait);
        e.open_lane("rev-std", "sess-1", "rev-4", "head-a", Some("d1"), 1_000, false, false);
        e.open_lane("rev-std", "sess-1", "rev-9", "head-a", Some("d1"), 2_000, false, false);
        assert!(e.record_verdict_seen("rev-std", Verdict::Fail, "head-a"));
        let before = e.lane("rev-std").expect("the fixture's own premise").clone();
        assert_eq!(before.prior_agents, vec!["rev-4".to_string()], "the fixture: one supersede");

        let after = before.reseeded("sess-resolved");
        assert_eq!(after.block, "rev-std", "the lane is the same lane");
        assert_eq!(
            after.session, "sess-resolved",
            "and it carries the session the CALLER resolved, not the one on the record — a \
             lane on a CLI that mints its session after boot has `\"\"` here for its whole \
             life, and seeding off this field would drop exactly those lanes (#2109)"
        );
        assert_eq!(
            after.agent, "",
            "the previous drive's pane is not this drive's CURRENT one — leaving it here \
             would have the duplicate guard read it as holding this round and `pane_dead` \
             read its death as this drive's"
        );
        assert_eq!(
            after.prior_agents,
            vec!["rev-4".to_string(), "rev-9".to_string()],
            "…but both panes are still OWNED, oldest first, so a reviewer finishing the \
             previous round is intercepted rather than reporting as if undriven (§7)"
        );
        assert_eq!(after.last_verdict, None, "the previous drive's answer is not this one's");
        assert_eq!(after.at_head, "", "…and nothing may claim this lane has answered here");
        assert_eq!(after.briefed_head, "", "…nor that it has been asked");
        assert_eq!(after.briefed_digest, "");
        assert_eq!(after.spawned_ms, 0, "a lane that has not been briefed has not been silent");
    }

    /// **#2163: a lane whose pane is GONE is re-opened, not waited for — and
    /// the stall arm still outranks that.**
    ///
    /// `lane_open_for` answers "was this lane ASKED about this revision", which
    /// stays true for ever after the pane that was asked has exited. So a killed
    /// reviewer left `review-wait` waiting on a verdict nothing could produce
    /// until `lane-stalled` an hour later — measured on PR #2140 as 25+ minutes
    /// with no rd-* row at all, and reached on the driver's own advice.
    ///
    /// **The live arm is the control, and it is not decoration**: without it an
    /// implementation that re-opened on every tick regardless of the pane would
    /// pass the dead arm and destroy the wait this state is made of.
    ///
    /// **The ordering arm is the BOUND.** A pane that dies on every spawn must
    /// not be replaced for ever; the stall arm is read first, so a lane whose
    /// panes keep dying reaches `lane_timeout_minutes` — from the ORIGINAL
    /// brief, which is what [`lane_stall_anchor`] preserves — and parks
    /// `held(lane-stalled)` naming itself. Asserting it here is what makes that
    /// bound a property rather than a hope.
    #[test]
    fn a_lane_whose_pane_died_is_re_opened_and_a_live_one_is_still_waited_for() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.open_lane("rev-std", "sess", "rev-4", "head-a", Some("d1"), 1_000, false, false);
        let facts = |dead: bool, now: u64| DriveFacts {
            now_ms: now,
            required_lanes: Some(vec![LaneFact {
                pane_dead: dead,
                ..lane_fact("rev-std", None, "", "")
            }]),
            ..facts_at("head-a")
        };

        assert_eq!(
            decide(&e, &facts(false, 2_000), &limits),
            DriveStep::Wait,
            "the control: a lane open at this revision with a LIVE pane is one to wait for, \
             and re-opening it would be a second reviewer per tick"
        );
        assert_eq!(
            decide(&e, &facts(true, 2_000), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false },
            "a lane whose recorded pane is gone is one to re-open — `lane_open_for` says it \
             was ASKED, which a dead pane cannot un-say and cannot answer"
        );

        // The bound on a pane that keeps dying: past the lane timeout the stall
        // arm answers first, and it is reached because the re-open preserves the
        // anchor rather than re-arming it.
        let stalled = 1_000 + minutes_ms(limits.lane_timeout_minutes);
        assert_eq!(
            decide(&e, &facts(true, stalled - 1), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false },
            "…one tick short, so the hold below is the timeout and not a coincidence"
        );
        assert_eq!(
            decide(&e, &facts(true, stalled), &limits),
            DriveStep::held(HeldReason::LaneStalled),
            "…and the silence bound still outranks the re-open, which is what stops a pane \
             that dies on every spawn from being replaced for ever"
        );
    }

    /// **#2163: replacing a dead pane keeps the silence clock where it was.**
    ///
    /// [`lane_stall_anchor`] is what makes the bound in the test above
    /// REACHABLE: `open_lane` writes whatever anchor it is given, so a re-open
    /// that passed `now` would restart `lane-stalled` on every death and the
    /// loop would run until the drive's own age bound.
    ///
    /// Six rows, each killing an implementation the others let through. Always
    /// returning `now_ms` fails row 1; always returning the record's value fails
    /// rows 2, 3 and 5; ignoring the head fails row 2.
    ///
    /// **Rows 3 and 4 are #2169 review 2's N1, and the first version of this
    /// table did not have them** — that is the finding, not a footnote. Four
    /// rows read as discriminating while leaving the DIGEST half of the
    /// revision key untested, because the function took no digest at all: a
    /// head-only implementation passed every one of them. Row 3 is the case
    /// that exposed (a lane whose pane died, then a body-only fix moving the
    /// digest, inheriting an anchor a reviewer that has read nothing did not
    /// earn); row 4 is its own control, so "re-arm whenever the digest does not
    /// match" — which would re-arm on an unreadable body too — cannot pass row 3
    /// by being wrong in the other direction.
    ///
    /// A test that cannot express an axis is a test that does not pin it, and
    /// the missing axis was in the SIGNATURE rather than in the rows.
    #[test]
    fn a_dead_panes_replacement_inherits_the_stall_anchor_and_a_new_round_does_not() {
        let mut e = entry_at(DriveState::ReviewWait);
        e.open_lane("rev-std", "sess", "rev-4", "head-a", Some("d1"), 1_000, false, false);
        let rec = || e.lane("rev-std");

        let d1 = Some("d1");
        let observed = vec![
            ("dead pane, same revision", lane_stall_anchor(rec(), "head-a", d1, true, 9_000)),
            ("dead pane, new head", lane_stall_anchor(rec(), "head-b", d1, true, 9_000)),
            // The row #2169 review 2 (N1) showed was unpinned: same head, moved
            // DIGEST. A head-only implementation answers 1_000 here and passes
            // every other row in this table unchanged.
            ("dead pane, moved digest", lane_stall_anchor(rec(), "head-a", Some("d2"), true, 9_000)),
            // …and its own control: an UNKNOWN live digest is "we could not
            // check", not drift, so it still inherits. Without this row an
            // implementation that re-armed on any non-matching digest — `None`
            // included — passes the row above.
            ("dead pane, digest unknown", lane_stall_anchor(rec(), "head-a", None, true, 9_000)),
            ("live pane, same revision", lane_stall_anchor(rec(), "head-a", d1, false, 9_000)),
            ("no record at all", lane_stall_anchor(None, "head-a", d1, true, 9_000)),
        ];
        let expected = vec![
            // The replacement inherits the silence this lane has already spent.
            ("dead pane, same revision", 1_000),
            // A new revision is a new round, owed the full window — and the
            // revision is the FULL key, so either half moving is a new one.
            ("dead pane, new head", 9_000),
            ("dead pane, moved digest", 9_000),
            ("dead pane, digest unknown", 1_000),
            // Not a replacement — an ordinary re-brief re-arms, as it always has.
            ("live pane, same revision", 9_000),
            ("no record at all", 9_000),
        ];
        assert_eq!(
            observed, expected,
            "each row is (fixture, the `spawned_ms` the next brief must carry). A `9_000` on \
             row 1 hands a lane whose pane keeps dying a fresh hour on every death, so \
             `lane-stalled` never fires and nothing per-lane bounds the loop. A `1_000` on \
             the `new head`, `moved digest` or `live pane` rows stalls a lane that has not \
             been silent at all — and on `moved digest` it does so to a reviewer that has \
             read nothing, while the same body-only fix under a live pane gets a full window."
        );
    }

    /// **#2109 review 1, finding 1.** The stall clock is a SILENCE test, and a
    /// reviewer that answered is not silent however long the round has run.
    ///
    /// This is the arc-8 return the first version of that arm could not see. The
    /// lane records `fail` at `head-a`; the worker's fix is BODY-ONLY, so
    /// `report(done)` at an unchanged head returns the drive straight to
    /// `review-wait` with the digest moved; the stale verdict then reads as
    /// absent and this arm is reached with `now - spawned_ms` equal to review
    /// time PLUS fix time. Keyed on `(briefed_head, spawned_ms)` alone that sum
    /// crosses `lane_timeout_minutes` and parks the drive `lane-stalled` on a
    /// reviewer that answered promptly — and parks it STUCK, because only
    /// `open_lane` writes `spawned_ms`, so a resume re-decides on unchanged
    /// facts and re-holds. Its only exits were a head change, a cancel, or the
    /// age backstop.
    ///
    /// **The two halves differ in exactly one field**, which is what makes this
    /// a pin on `at_head` rather than on the timeout: same lane, same clock,
    /// same moved digest, same stale-verdict fact handed to `decide` — and only
    /// whether `record_verdict_seen` ever ran. Without the second half an
    /// implementation that simply deleted the stall arm would pass.
    #[test]
    fn a_lane_that_answered_at_this_head_is_not_stalled_by_the_fix_round_that_followed() {
        let limits = DriveLimits::default();
        let past = 1_000 + minutes_ms(limits.lane_timeout_minutes);
        // The gate's own fact for the lane: the `fail` it recorded, bound to the
        // head it reviewed and to the digest that has since moved. `decide`
        // reads this as ABSENT (`lane_verdict_is_current`), which is what puts
        // the lane back on the arm under test.
        let facts = DriveFacts {
            now_ms: past,
            body_digest: Some("d2".to_string()),
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Fail), "head-a", "d1")]),
            ..facts_at("head-a")
        };

        let answered = {
            let mut e = entry_at(DriveState::ReviewWait);
            e.head = "head-a".into();
            e.open_lane("rev-std", "sess", "rev-4", "head-a", Some("d1"), 1_000, false, false);
            assert!(
                e.record_verdict_seen("rev-std", Verdict::Fail, "head-a"),
                "the fixture must actually record the answer, or this pins nothing"
            );
            e
        };
        assert_eq!(
            decide(&answered, &facts, &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: true },
            "a reviewer that ANSWERED at this head is not silent — arc 8 owes it the delta \
             re-brief, not a hold naming it as the thing that went quiet"
        );

        // The control: the identical entry that never answered. One field apart,
        // and it must still stall.
        let silent = {
            let mut e = entry_at(DriveState::ReviewWait);
            e.head = "head-a".into();
            e.open_lane("rev-std", "sess", "rev-4", "head-a", Some("d1"), 1_000, false, false);
            e
        };
        assert_eq!(
            decide(&silent, &facts, &limits),
            DriveStep::held(HeldReason::LaneStalled),
            "…while the lane that was asked and said nothing still stalls, which is the \
             whole point of keying the clock on the head"
        );
    }

    /// **#2109.** A lane's stall clock is keyed on the HEAD it was asked about,
    /// so a body edit under a SILENT reviewer cannot hand it another hour.
    ///
    /// The silence half of that key is pinned by the sibling above; every
    /// fixture here builds its record through `open_lane` alone, so `at_head`
    /// is empty and the lane has answered nothing.
    ///
    /// The two halves discriminate. The first is the pre-#2109 behaviour and is
    /// the control: a lane still open for this exact revision, past its timeout,
    /// has always been `lane-stalled`. The second is the one that moved — same
    /// lane, same silence, same clock, and only the DIGEST different — and under
    /// the old keying it read as a re-brief, which re-armed `spawned_ms` and
    /// bought the reviewer a fresh timeout it had done nothing to earn.
    ///
    /// The third is the negative control, and without it an implementation that
    /// simply held `lane-stalled` for every lane record would pass both halves.
    #[test]
    fn a_lanes_stall_clock_survives_a_body_edit_it_never_read() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.open_lane("rev-std", "sess", "rev-4", "head-a", Some("d1"), 1_000, false, false);
        let stale = 1_000 + minutes_ms(limits.lane_timeout_minutes);
        let facts = |now: u64, digest: &str| DriveFacts {
            now_ms: now,
            body_digest: Some(digest.to_string()),
            required_lanes: Some(vec![lane_fact("rev-std", None, "", "")]),
            ..facts_at("head-a")
        };

        assert_eq!(
            decide(&e, &facts(stale, "d1"), &limits),
            DriveStep::held(HeldReason::LaneStalled),
            "the control: a lane open at this revision and past its timeout has always stalled"
        );
        assert_eq!(
            decide(&e, &facts(stale, "d2"), &limits),
            DriveStep::held(HeldReason::LaneStalled),
            "and a body edit the silent reviewer never read must not re-arm its clock"
        );
        assert_eq!(
            decide(&e, &facts(stale - 1, "d2"), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false },
            "the negative control: inside the timeout a moved digest is still a re-brief"
        );
    }

    /// **#2109's third ask at the decision layer.** A drive whose lane the
    /// live-delegate cap keeps refusing must become one of §2.2's exits, and it
    /// must not become one on the first refusal.
    ///
    /// The three points are asserted together because each alone passes under an
    /// implementation that is wrong at another: an arm that never holds passes
    /// the first two, and an arm that holds on the stamp alone (no duration)
    /// passes the first and third.
    #[test]
    fn a_cap_that_keeps_refusing_a_lane_parks_the_drive_only_once_the_window_is_spent() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        let facts = |now: u64| DriveFacts {
            now_ms: now,
            required_lanes: Some(vec![lane_fact("rev-std", None, "", "")]),
            ..facts_at("head-a")
        };

        // The control: an un-starved drive proposes the spawn, so every
        // assertion below is about the starvation and not about a lane that
        // could never have opened.
        assert_eq!(decide(&e, &facts(10_000), &limits), DriveStep::OpenLane { index: 0, verify: false, body_only: false });

        assert!(e.note_cap_starvation(10_000), "the first refusal of a run stamps the clock");
        // One tick short of the window: still trying, still no orchestrator turn
        // spent on a condition that clears itself.
        assert_eq!(
            decide(&e, &facts(10_000 + CAP_HOLD_MS - 1), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false },
            "a cap that has not refused for the whole window is a back-off, not a hold"
        );
        assert_eq!(
            decide(&e, &facts(10_000 + CAP_HOLD_MS), &limits),
            DriveStep::held(HeldReason::CapFull),
            "a starvation that outlasts the window is one of §2.2's exits, not silence"
        );
    }

    /// The clock measures a RUN, and every way out of one clears it.
    ///
    /// The re-stamp half is the one with teeth: a tick that re-stamped on each
    /// refusal would make [`CAP_HOLD_MS`] unreachable, and the hold above would
    /// then be dead code that no test in this file could tell apart from a hold
    /// that fires — `decide` reads a stamp it is handed either way.
    #[test]
    fn the_starvation_clock_is_stamped_once_per_run_and_no_arc_carries_one_across() {
        let mut e = entry_at(DriveState::ReviewWait);
        assert_eq!(e.cap_starved_for(10_000), None, "a fresh entry is not starved");

        assert!(e.note_cap_starvation(10_000));
        assert!(!e.note_cap_starvation(20_000), "a second refusal in one run is not a new run");
        assert_eq!(
            e.cap_starved_for(20_000),
            Some(10_000),
            "the duration is measured from the FIRST refusal, not the newest tick"
        );

        assert!(e.clear_cap_starvation(20_000), "a lane that opens ends the run");
        assert_eq!(e.cap_starved_for(20_000), None);
        assert!(
            !e.clear_cap_starvation(20_000),
            "and clearing an unstarved entry changes nothing"
        );
        // #2110: what the run COST survives the clear, because that is the
        // quantity both age bounds subtract. A `clear` that only forgot the
        // anchor would leave the ten seconds charged to the drive that was
        // starved — which is the whole defect, one code path over from where
        // it was reported.
        assert_eq!(
            e.starved_ms(20_000),
            10_000,
            "ending a starvation run must bank what it cost, not discard it"
        );

        // An arc clears it too, including the arc into the hold it produces: a
        // resume must start the window over rather than re-hold on the next tick
        // from a stamp the previous starvation left behind.
        assert!(e.note_cap_starvation(30_000));
        e.advance(DriveState::Held, Some(HeldReason::CapFull), None, 30_000).unwrap();
        assert_eq!(
            e.cap_starved_for(30_000),
            None,
            "a drive that MOVED is not the drive that was stuck"
        );
    }

    /// **#2135's restart clear, and the ONE thing that distinguishes it from
    /// the tick's.** A starvation run cannot straddle a process boundary, and
    /// the interval it would otherwise be charged for is mostly orrerix's own
    /// downtime.
    ///
    /// The two halves are the same entry, the same stamp and the same clock,
    /// differing in exactly one call — so this pins that the restart clear
    /// charges NOTHING, and not merely that it forgets the anchor. An
    /// implementation spelled `clear_cap_starvation(now)` forgets the anchor
    /// perfectly and fails the second assertion, which is the point: charging
    /// the gap would FORGIVE orrerix's own downtime from both age bounds, and
    /// that downtime being CHARGED is the property #2117 disclosed and pinned
    /// (`orrerix_downtime_is_charged_to_the_state_it_spanned`).
    #[test]
    fn the_restart_clear_forgets_a_run_without_charging_the_downtime_it_spans() {
        let mut restarted = entry_at(DriveState::ReviewWait);
        assert!(
            !restarted.discard_cap_starvation_run(),
            "an entry with no run has none to drop, and says so"
        );
        assert!(restarted.note_cap_starvation(10_000));
        assert!(restarted.discard_cap_starvation_run(), "…and a stamped one says it dropped one");
        assert_eq!(restarted.cap_starved_for(2_000_000), None, "the run is over");
        assert_eq!(
            restarted.starved_ms(2_000_000),
            0,
            "and the half hour between the last tick of one process and the first of the next \
             is charged to the drive, never forgiven as a starvation nobody observed"
        );

        // The contrast, which is what makes the assertion above a discriminator
        // rather than a restatement: the TICK's clear, over the same stamp at
        // the same clock, banks every millisecond of it.
        let mut ticked = entry_at(DriveState::ReviewWait);
        assert!(ticked.note_cap_starvation(10_000));
        assert!(ticked.clear_cap_starvation(2_000_000));
        assert_eq!(
            ticked.starved_ms(2_000_000),
            1_990_000,
            "the tick's clear banks what the run cost — that IS the difference, and it is why \
             the restart clear is a separate function rather than a call of this one"
        );
    }

    /// **#2110's exclusion, at the decision itself.** A drive the live-delegate
    /// cap will not let spawn is neither progressing nor stalled, and no clock
    /// may charge it for the difference.
    ///
    /// **The two halves differ in exactly one call** — `note_cap_starvation` —
    /// so this pins the EXCLUSION and not the bound: same entry, same state,
    /// same head, same lane list, same clock, and only whether the cap had been
    /// refusing throughout. Without the control half an implementation that
    /// never holds `state-stalled` at all would pass it; without the starved
    /// half one that ignores starvation would.
    ///
    /// And the starved half parks on the CAP rather than on nothing, which is
    /// the outcome #2110 asks for: the orchestrator is told the one thing it
    /// can act on (free a slot), instead of being told a drive stalled when
    /// what happened is that it was never allowed to move.
    #[test]
    fn time_the_cap_refused_a_lane_advances_neither_age_bound() {
        let limits = DriveLimits::default();
        let facts = |now: u64| DriveFacts {
            now_ms: now,
            required_lanes: Some(vec![lane_fact("rev-std", None, "", "")]),
            ..facts_at("head-a")
        };
        // `review-wait`'s bound is FOUR hours here: the three-hour constant
        // plus one lane at the default sixty-minute timeout (#2117 review 2
        // made it an add rather than a max). Four hours therefore reaches it
        // exactly, and is well short of the twelve-hour backstop, so exactly
        // one time bound is in play and a red is attributable to it.
        let past = 1_000 + 4 * 60 * 60_000;

        let mut moving = entry_at(DriveState::ReviewWait);
        moving.head = "head-a".to_string();
        assert_eq!(
            decide(&moving, &facts(past), &limits),
            DriveStep::held(HeldReason::StateStalled),
            "a drive that really did sit four hours in `review-wait` must park"
        );

        let mut starved = entry_at(DriveState::ReviewWait);
        starved.head = "head-a".to_string();
        // One minute of ordinary life, THEN the cap. Not starved from the
        // instant it was created: both figures below would then read zero,
        // which is also what every arithmetic error produces, and they would
        // stop discriminating between an exclusion that works and one that
        // returns nothing.
        starved.note_cap_starvation(61_000);
        assert_eq!(
            decide(&starved, &facts(past), &limits),
            DriveStep::held(HeldReason::CapFull),
            "the cap held this drive still for four hours; charging that to its own state clock reports a stall, which is the false claim #2110 is about"
        );
        assert_eq!(
            starved.state_elapsed_ms(past),
            60_000,
            "the state clock must hold at the one minute this drive spent able to act"
        );
        assert_eq!(
            starved.bounded_age_ms(past),
            60_000,
            "and so must the age the backstop reads: it is the drive's own, not the cap's"
        );
        assert_eq!(
            starved.age_ms(past),
            past - 1_000,
            "…while the WALL age is untouched, which is what `since_ms` reports"
        );
    }

    /// **#1871 B1 at the decision layer.** A verdict decides only for the
    /// revision it reviewed, and the rule is word-blind.
    ///
    /// The same recorded `fail`, read at two heads: at its own it takes arc 5 and
    /// spends a round (the control — without it, an implementation that ignored
    /// every `fail` would pass); at the head the worker moved to it is absent, so
    /// the lane is re-opened. `escalate` is asserted beside it because it takes a
    /// different arc, and a fix that special-cased `Fail` alone would pass on one
    /// and fail on the other.
    ///
    /// `entry.head` is set to the LIVE head deliberately: arc 6 would otherwise
    /// answer first and this would be a test of arc 6, not of the binding rule.
    /// That is exactly the shape the drive is in after a real fix — the tick
    /// persists the head it resolved, so by the time `review-wait` is reached
    /// again the entry and the live head agree and only the VERDICT is stale.
    #[test]
    fn a_verdict_bound_to_an_older_head_decides_nothing_whatever_word_it_is() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-b".into();
        let at = |word, verdict_head| DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", Some(word), verdict_head, "d1")]),
            ..facts_at("head-b")
        };
        // The control: bound to the head in front of the drive, each word decides.
        assert_eq!(
            decide(&e, &at(Verdict::Fail, "head-b"), &limits),
            DriveStep::spend(DriveState::FixWait, Counter::ReviewRounds)
        );
        assert_eq!(
            decide(&e, &at(Verdict::Escalate, "head-b"), &limits),
            DriveStep::held(HeldReason::Escalate)
        );
        // Bound to the head the worker fixed: absent, so the lane is re-opened.
        // Not `fix-wait`, and not a spent round — that loop reached INVARIANT 9's
        // bound in three passes with no re-review having happened.
        assert_eq!(
            decide(&e, &at(Verdict::Fail, "head-a"), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false },
            "a fail recorded against a commit that no longer describes the PR must not route"
        );
        assert_eq!(
            decide(&e, &at(Verdict::Escalate, "head-a"), &limits),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false },
            "…nor may a stale escalate park the drive on a judgment nobody is being asked for"
        );
        // The digest half of the same key: same head, body moved under it.
        let moved = DriveFacts {
            body_digest: Some("d2".into()),
            ..at(Verdict::Fail, "head-b")
        };
        assert_eq!(decide(&e, &moved, &limits), DriveStep::OpenLane { index: 0, verify: false, body_only: false });
        // …and "we could not check" is not "it changed", in this direction too.
        let unknown = DriveFacts { body_digest: None, ..at(Verdict::Fail, "head-b") };
        assert_eq!(
            decide(&e, &unknown, &limits),
            DriveStep::spend(DriveState::FixWait, Counter::ReviewRounds),
            "an unreadable body must not stale a verdict — one transient gh failure would \
             otherwise re-brief every open lane in the group"
        );
    }

    #[test]
    fn a_head_that_moved_under_a_lane_re_enters_ci_wait() {
        // Arc 6 / §8 row 4. The race is not designed away — it is the race the
        // verdict binding already exists to handle.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        let facts = DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "head-a", "d1")]),
            ..facts_at("head-b")
        };
        assert_eq!(decide(&e, &facts, &limits), DriveStep::to(DriveState::CiWait));
    }

    #[test]
    fn a_lane_briefed_at_this_head_is_waited_for_then_bounded() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("d1"), 1_000, false, false);
        let facts = DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", None, "", "")]),
            now_ms: 1_000 + minutes_ms(limits.lane_timeout_minutes) - 1,
            ..facts_at("head-a")
        };
        // Open and inside its wait: nothing to do, and in particular NOT a
        // re-brief on every tick.
        assert_eq!(decide(&e, &facts, &limits), DriveStep::Wait);
        // Past `lane_timeout_minutes` with no verdict: parked, naming the pane.
        let stalled = DriveFacts {
            now_ms: 1_000 + minutes_ms(limits.lane_timeout_minutes),
            ..facts
        };
        assert_eq!(
            decide(&e, &stalled, &limits),
            DriveStep::held(HeldReason::LaneStalled)
        );
    }

    #[test]
    fn a_lane_briefed_at_an_older_head_is_re_briefed_not_waited_for() {
        // This is what `briefed_head` exists to answer, and it is why that
        // field is not `at_head`: a lane whose brief predates the live head has
        // been asked nothing about this revision.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-b".into();
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("d1"), 1_000, false, false);
        let facts = DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", None, "", "")]),
            ..facts_at("head-b")
        };
        assert_eq!(decide(&e, &facts, &limits), DriveStep::OpenLane { index: 0, verify: false, body_only: false });
    }

    #[test]
    fn a_pass_whose_body_digest_moved_re_opens_that_lane() {
        // §8's body-changed row: the (head, digest) key is re-read every tick,
        // so a moved digest with an unchanged head re-enters at the first stale
        // lane with a body-only delta brief.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("OLD"), 1_000, false, false);
        let facts = DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "head-a", "OLD")]),
            body_digest: Some("NEW".into()),
            ..facts_at("head-a")
        };
        // …and `verify` is TRUE (#2168 E2): the single required lane has a pass
        // bound to this head, so nothing about the code is outstanding and the
        // brief about to go out is a body-verification delta.
        assert_eq!(decide(&e, &facts, &limits), DriveStep::OpenLane { index: 0, verify: true, body_only: true });
    }

    #[test]
    fn a_body_only_move_re_briefs_one_lane_and_the_verification_settles_the_rest() {
        // #2168 E2, the driver half. Two required lanes, both passed at
        // (head-a, d1); the worker edits the PR body and nothing else.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        let with = |lanes: Vec<LaneFact>| DriveFacts {
            required_lanes: Some(lanes),
            body_digest: Some("d2".into()),
            ..facts_at("head-a")
        };
        let stale = vec![
            lane_fact("rev-std", Some(Verdict::Pass), "head-a", "d1"),
            lane_fact("rev-final", Some(Verdict::Pass), "head-a", "d1"),
        ];

        // ONE lane is briefed — the first in the gate's order — and the brief
        // is a verification delta, which is what makes the verdict it produces
        // able to discharge the clause for the other.
        assert_eq!(
            decide(&e, &with(stale.clone()), &limits),
            DriveStep::OpenLane { index: 0, verify: true, body_only: false }
        );

        // S3 sends it; the drive waits rather than moving on to lane 1. That is
        // the whole saving: before #2168 E2 the second lane's turn came next.
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("d2"), 1_500, true, false);
        assert_eq!(decide(&e, &with(stale.clone()), &limits), DriveStep::Wait);

        // rev-std records the verification pass. rev-final's pass at d1 settles
        // again, so the drive goes to the gate instead of briefing lane 1.
        let answered = vec![
            verified_lane_fact("rev-std", "head-a", "d2"),
            lane_fact("rev-final", Some(Verdict::Pass), "head-a", "d1"),
        ];
        assert_eq!(
            decide(&e, &with(answered), &limits),
            DriveStep::to(DriveState::GateCheck)
        );

        // **The control, on the axis that carries the saving: WHICH step comes
        // back.** The same round with rev-std's pass recorded WITHOUT the mark
        // — an ordinary re-review that happens to sit at the current digest —
        // still owes lane 1 a brief, where the marked round above went straight
        // to `gate-check`. An implementation that accepted any newer pass would
        // answer `gate-check` here too, and would have weakened
        // `body-unchanged` for every repo, driver or no driver.
        //
        // The brief it gets is itself a verification round, and that is right
        // rather than a leak: every required lane has a `pass` bound to this
        // head, so nothing about the CODE is outstanding and what rev-final is
        // owed is the body. The grant it carries is simply not needed here —
        // rev-std's pass already sits at the current digest — which is why the
        // discriminator in this test is the index and not the flag.
        let unmarked = vec![
            lane_fact("rev-std", Some(Verdict::Pass), "head-a", "d2"),
            lane_fact("rev-final", Some(Verdict::Pass), "head-a", "d1"),
        ];
        assert_eq!(
            decide(&e, &with(unmarked), &limits),
            DriveStep::OpenLane { index: 1, verify: true, body_only: false }
        );

        // And the delegation is bounded by the HEAD it was granted at: once the
        // code moves, a body verification settles nothing and arc 6 takes the
        // drive back to `ci-wait`.
        let pushed = DriveFacts {
            required_lanes: Some(vec![
                verified_lane_fact("rev-std", "head-a", "d2"),
                lane_fact("rev-final", Some(Verdict::Pass), "head-a", "d1"),
            ]),
            body_digest: Some("d2".into()),
            ..facts_at("head-b")
        };
        assert_eq!(decide(&e, &pushed, &limits), DriveStep::to(DriveState::CiWait));
    }

    #[test]
    fn a_verification_delta_is_only_briefed_when_nothing_about_the_code_is_outstanding() {
        // #2168 E2's precondition, one crossing per way it can fail. The grant
        // the brief carries is what lets the gate stop asking the other lanes,
        // so it is issued only when EVERY required lane has a pass bound to the
        // head that would merge — anything else and this is an ordinary round.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        let with = |lanes: Vec<LaneFact>, digest: Option<&str>| DriveFacts {
            required_lanes: Some(lanes),
            body_digest: digest.map(|d| d.to_string()),
            ..facts_at("head-a")
        };
        let std_pass = lane_fact("rev-std", Some(Verdict::Pass), "head-a", "d1");

        // A lane that has never answered: the code is still outstanding. Lane 0
        // is still the one briefed — its own pass staled with the digest — but
        // the brief is an ORDINARY one, and the verdict it produces will not
        // discharge anything for lane 1.
        assert_eq!(
            decide(
                &e,
                &with(vec![std_pass.clone(), lane_fact("rev-final", None, "", "")], Some("d2")),
                &limits
            ),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false }
        );

        // A lane whose pass is bound to a head the branch has left reads as
        // absent (#1871 B1) — and "absent" is exactly what must not be granted
        // over, because the code that lane approved is not the code in front of
        // the drive.
        assert_eq!(
            decide(
                &e,
                &with(
                    vec![
                        std_pass.clone(),
                        lane_fact("rev-final", Some(Verdict::Pass), "head-OLD", "d1")
                    ],
                    Some("d2")
                ),
                &limits
            ),
            DriveStep::OpenLane { index: 0, verify: false, body_only: false }
        );

        // A body orrerix could not read grants nothing: the verdict this brief
        // produces would carry no digest, so there would be no body for the
        // mark to be ABOUT. (An unknown digest does not stale a pass either, so
        // lane 1 here is outstanding for its own reason — no verdict at all.)
        assert_eq!(
            decide(
                &e,
                &with(vec![std_pass.clone(), lane_fact("rev-final", None, "", "")], None),
                &limits
            ),
            DriveStep::OpenLane { index: 1, verify: false, body_only: false }
        );

        // The positive control, so the three rows above are the precondition
        // deciding and not a constant.
        assert_eq!(
            decide(
                &e,
                &with(
                    vec![
                        std_pass,
                        lane_fact("rev-final", Some(Verdict::Pass), "head-a", "d1")
                    ],
                    Some("d2")
                ),
                &limits
            ),
            DriveStep::OpenLane { index: 0, verify: true, body_only: false }
        );
    }

    #[test]
    fn a_re_brief_ends_the_loop_rather_than_repeating_it_every_tick() {
        // The other half of the fix above, and the failure a head-only key
        // would swap this one for: once the lane HAS been re-briefed at the new
        // digest, the very same stale `pass` must read as "asked, thinking" and
        // not re-brief again. The verdict file still holds the old pass — a
        // reviewer has not re-recorded yet — so nothing but the brief key can
        // tell these two ticks apart.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = "head-a".into();
        let facts = DriveFacts {
            required_lanes: Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "head-a", "OLD")]),
            body_digest: Some("NEW".into()),
            ..facts_at("head-a")
        };
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("OLD"), 1_000, false, false);
        assert_eq!(decide(&e, &facts, &limits), DriveStep::OpenLane { index: 0, verify: true, body_only: true });
        // S3 performs that brief; the drive now waits on the reviewer.
        e.open_lane("rev-std", "s1", "rev-1", "head-a", Some("NEW"), 1_500, true, true);
        assert_eq!(decide(&e, &facts, &limits), DriveStep::Wait);
    }

    #[test]
    fn a_lane_is_open_only_for_the_revision_it_was_asked_about() {
        // All four crossings of {briefed head matches} x {briefed digest
        // matches}, because this guard reads two signals and the defect it
        // shipped with was reading one. Three of the four must re-brief; a
        // guard that answered "open" on the head alone passes two of them.
        let rec = |head: &str, digest: &str| LaneRecord {
            block: "rev-std".into(),
            session: "s1".into(),
            agent: "rev-1".into(),
            prior_agents: Vec::new(),
            last_verdict: None,
            at_head: String::new(),
            briefed_head: head.into(),
            briefed_digest: digest.into(),
            stopped_head: String::new(),
            spawned_ms: 0,
            briefed_verify: false,
            briefed_body_only: false,
            extra: BTreeMap::new(),
        };
        let now = Some("d1");
        assert!(lane_open_for(&rec("head-a", "d1"), "head-a", now));
        assert!(!lane_open_for(&rec("head-a", "OLD"), "head-a", now));
        assert!(!lane_open_for(&rec("head-OLD", "d1"), "head-a", now));
        assert!(!lane_open_for(&rec("head-OLD", "OLD"), "head-a", now));
        // "We could not check" is not "it changed", in either direction — one
        // transient failure to read a PR body must not re-brief every open lane
        // in the group.
        assert!(lane_open_for(&rec("head-a", "d1"), "head-a", None));
        assert!(lane_open_for(&rec("head-a", "d1"), "head-a", Some("")));
        assert!(lane_open_for(&rec("head-a", ""), "head-a", now));
        // ...but an unknown digest never rescues a head that really did move.
        assert!(!lane_open_for(&rec("head-OLD", ""), "head-a", None));
    }

    #[test]
    fn fix_wait_takes_its_two_arcs_and_its_three_holds() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::FixWait);
        e.head = "head-a".into();
        assert_eq!(e.fix_handback_ms, 1_000);

        // Arc 7: the worker pushed.
        assert_eq!(
            decide(&e, &facts_at("head-b"), &limits),
            DriveStep::to(DriveState::CiWait)
        );
        // Arc 8: report(done) with the head unchanged — a body-only fix.
        let done_same_head = DriveFacts {
            worker: WorkerSignal::Done,
            ..facts_at("head-a")
        };
        assert_eq!(
            decide(&e, &done_same_head, &limits),
            DriveStep::to(DriveState::ReviewWait)
        );
        // A push outranks a `done` that arrived with it: the code moved, so CI
        // is what has to answer next.
        let done_and_pushed = DriveFacts {
            worker: WorkerSignal::Done,
            ..facts_at("head-b")
        };
        assert_eq!(
            decide(&e, &done_and_pushed, &limits),
            DriveStep::to(DriveState::CiWait)
        );
        // The three holds.
        for (signal, reason) in [
            (WorkerSignal::Blocked, HeldReason::WorkerBlocked),
            (WorkerSignal::Unresumable, HeldReason::WorkerUnresumable),
        ] {
            let facts = DriveFacts { worker: signal, ..facts_at("head-a") };
            assert_eq!(decide(&e, &facts, &limits), DriveStep::held(reason));
        }
        // Silent inside the wait, then bounded by it.
        let quiet = DriveFacts {
            now_ms: e.fix_handback_ms + minutes_ms(limits.fix_timeout_minutes) - 1,
            ..facts_at("head-a")
        };
        assert_eq!(decide(&e, &quiet, &limits), DriveStep::Wait);
        let stalled = DriveFacts {
            now_ms: e.fix_handback_ms + minutes_ms(limits.fix_timeout_minutes),
            ..facts_at("head-a")
        };
        assert_eq!(
            decide(&e, &stalled, &limits),
            DriveStep::held(HeldReason::FixStalled)
        );
    }

    #[test]
    fn an_unresumable_worker_is_a_hold_and_never_a_drive_time_refusal() {
        // §5.1 is deliberately honest about this: a full, well-shaped session
        // id this group never recorded takes `resolve_session_ref`'s
        // passthrough arm and is ACCEPTED by `drive_review`, so its
        // unresumability surfaces here, at the first hand-back, possibly hours
        // on. Nothing in this module claims to catch it earlier — resolving is
        // not the same as proving resumable.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::FixWait);
        e.head = "head-a".into();
        let facts = DriveFacts {
            worker: WorkerSignal::Unresumable,
            ..facts_at("head-a")
        };
        assert_eq!(
            decide(&e, &facts, &limits),
            DriveStep::held(HeldReason::WorkerUnresumable)
        );
    }

    #[test]
    fn gate_check_answers_satisfied_only_when_the_gate_did() {
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::GateCheck);
        e.head = "head-a".into();
        let with = |gate| decide(&e, &DriveFacts { gate, ..facts_at("head-a") }, &limits);
        // Arc 9.
        assert_eq!(with(GateOutcome::Satisfied), DriveStep::to(DriveState::Satisfied));
        // Arc 10 — wider than "stale" on purpose.
        assert_eq!(with(GateOutcome::Unsatisfied), DriveStep::to(DriveState::CiWait));
        // Present and unreadable is a hold, NOT `gate-not-configured`.
        assert_eq!(
            with(GateOutcome::Unreadable),
            DriveStep::held(HeldReason::GateUnreadable)
        );
        // Nothing was evaluated, so nothing is known — and in particular this
        // is not `satisfied`.
        assert_eq!(with(GateOutcome::NotEvaluated), DriveStep::Wait);
    }

    /// **The bounds and the `messaged` hold outrank a conflict** — the ordering
    /// `decide` states in prose, asserted (#2311 review round 2).
    ///
    /// A precedence written in a comment and pinned nowhere is a claim about the
    /// order of two `if`s, and any edit reverses it silently: every one of these
    /// facts reaches `decide` on the same tick, so which answer comes back is
    /// decided by line order alone.
    ///
    /// The direction matters. A drive already past its clock is not made young
    /// by a rebase, and `message_orchestrator` is a delegate's own words already
    /// in the pane — reporting a rebase hand-back instead of either would be a
    /// notice that does not account for what actually stopped the drive.
    #[test]
    fn the_bounds_and_the_messaged_hold_outrank_a_conflict() {
        let limits = DriveLimits::default();
        let conflict = |e: &DriveEntry, f: DriveFacts| decide(e, &f, &limits);

        // The positive control FIRST, so every refusal below is a difference:
        // on these facts, minus the thing under test, the conflict wins.
        let mut fresh = entry_at(DriveState::GateCheck);
        fresh.head = "head-a".into();
        let base = || DriveFacts { ci: CiObservation::Conflicting, ..facts_at("head-a") };
        assert_eq!(
            conflict(&fresh, base()),
            DriveStep::spend(DriveState::FixWait, Counter::RebaseAttempts),
            "the control: a conflict on an otherwise healthy drive IS the rebase arc"
        );

        // 3. `messaged` outranks it.
        assert_eq!(
            conflict(&fresh, DriveFacts { messaged: true, ..base() }),
            DriveStep::held(HeldReason::Messaged)
        );

        // 2. A positively-closed PR outranks it — a PR that is gone has nothing
        //    to rebase onto.
        assert_eq!(
            conflict(&fresh, DriveFacts { pr_open: Some(false), ..base() }),
            DriveStep::to(DriveState::Cancelled)
        );

        // 4. The drive's AGE outranks it.
        let aged = DriveFacts {
            now_ms: 1_000 + minutes_ms(limits.drive_timeout_minutes),
            ..base()
        };
        assert_eq!(conflict(&fresh, aged), DriveStep::held(HeldReason::DriveStalled));

        // 5. Time in THIS state outranks it. Under the age bound, so the answer
        //    is this clock and not the backstop above.
        let bound = state_bound_ms(DriveState::GateCheck, &limits, 0).expect("gate-check is bounded");
        let stuck = DriveFacts { now_ms: 1_000 + bound, ..base() };
        assert!(
            bound < minutes_ms(limits.drive_timeout_minutes),
            "the fixture's premise: the state bound must fire before the age backstop"
        );
        assert_eq!(conflict(&fresh, stuck), DriveStep::held(HeldReason::StateStalled));

        // …and the empty-head guard, which is "we could not read this PR at
        // all" — below the bounds, still above the conflict.
        assert_eq!(
            conflict(&fresh, DriveFacts { head: String::new(), ..base() }),
            DriveStep::Wait
        );

        // A terminal or parked entry still yields `Wait`, conflict or not: the
        // tick does not move a drive `drive_review` owns.
        for st in [DriveState::Held, DriveState::Satisfied, DriveState::Cancelled] {
            let mut e = entry_at(st);
            e.head = "head-a".into();
            assert_eq!(conflict(&e, base()), DriveStep::Wait, "{}", st.as_str());
        }
    }

    #[test]
    fn a_conflicting_pr_takes_the_rebase_arc_from_every_state_that_can() {
        // #2311, widened past the plan's `gate-check` scope by the measurement
        // on #3118: routing needs a changed-file list, GitHub computes none for
        // a conflicted head, so `review-wait` answered `routing-unaccountable`
        // — a hold naming a CONSEQUENCE of the conflict, whose remedy
        // (`drive_review` again) reproduces it.
        let limits = DriveLimits::default();
        // Each state paired with the answer it gives on the SAME facts minus
        // the conflict — which is not one answer, because `ci-wait` never
        // reads routing at all. Written per state so the control is that
        // state's own strongest other answer rather than a shared shape.
        for (st, without_the_conflict) in [
            (DriveState::CiWait, DriveStep::Wait),
            (DriveState::ReviewWait, DriveStep::held(HeldReason::RoutingUnaccountable)),
            (DriveState::GateCheck, DriveStep::held(HeldReason::RoutingUnaccountable)),
        ] {
            let mut e = entry_at(st);
            e.head = "head-a".into();
            let facts = DriveFacts {
                ci: CiObservation::Conflicting,
                // Everything else set so that any state reading it would
                // answer something ELSE — the gate says SATISFIED and routing
                // says nothing at all — which is what makes the assertion below
                // a difference rather than a shape that holds either way.
                gate: GateOutcome::Satisfied,
                required_lanes: None,
                ..facts_at("head-a")
            };
            assert_eq!(
                decide(&e, &facts, &limits),
                DriveStep::spend(DriveState::FixWait, Counter::RebaseAttempts),
                "{}: a conflicting PR is a rebase hand-back, not this state's own answer",
                st.as_str()
            );
            // The control: with the mergeability the ONLY thing changed, each
            // state gives the answer that must not have won above.
            assert_eq!(
                decide(&e, &DriveFacts { ci: CiObservation::Pending, ..facts.clone() }, &limits),
                without_the_conflict,
                "{}: the control — without the conflict this state answers for itself",
                st.as_str()
            );
            // …and the second conflict parks, on the same counter `ci-wait` has
            // always spent (`counter_exhausted`'s ordering).
            let mut spent = e.clone();
            spent.counters.rebase_attempts = limits.max_rebase_attempts;
            assert_eq!(
                decide(&spent, &facts, &limits),
                DriveStep::held(HeldReason::RebaseLimit),
                "{}",
                st.as_str()
            );
            // The machine really accepts the arc: a step `decide` proposes and
            // `transition` refuses fails at runtime, on the degradation path.
            assert!(transition(st, DriveState::FixWait).is_ok(), "{}", st.as_str());
        }
    }

    #[test]
    fn fix_wait_is_the_one_state_a_conflict_does_not_divert() {
        // A rebase hand-back is already outstanding there — `(fix-wait,
        // fix-wait)` is not a transition, and spending a second
        // `rebase_attempts` on the conflict the worker was just asked to fix
        // would park `rebase-limit` before it could push.
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::FixWait);
        e.head = "head-a".into();
        let facts = DriveFacts { ci: CiObservation::Conflicting, ..facts_at("head-a") };
        assert_eq!(decide(&e, &facts, &limits), DriveStep::Wait);
        assert!(transition(DriveState::FixWait, DriveState::FixWait).is_err());
        // And the worker's own signals still decide the state, unchanged by the
        // mergeability beside them — the positive control for "this state was
        // reached at all".
        assert_eq!(
            decide(&e, &DriveFacts { worker: WorkerSignal::Done, ..facts }, &limits),
            DriveStep::to(DriveState::ReviewWait)
        );
    }

    #[test]
    fn an_unreadable_or_unevaluated_mergeability_is_never_a_conflict() {
        // §8: an unknown is never a fact about the PR. Each state still gives
        // its own answer, both ways, so neither reading is "it waits anyway".
        let limits = DriveLimits::default();
        let mut e = entry_at(DriveState::GateCheck);
        e.head = "head-a".into();
        for ci in [CiObservation::Pending, CiObservation::Unknown, CiObservation::Green] {
            for (gate, want) in [
                (GateOutcome::Satisfied, DriveStep::to(DriveState::Satisfied)),
                (GateOutcome::Unsatisfied, DriveStep::to(DriveState::CiWait)),
                (GateOutcome::Unreadable, DriveStep::held(HeldReason::GateUnreadable)),
                (GateOutcome::NotEvaluated, DriveStep::Wait),
            ] {
                assert_eq!(
                    decide(&e, &DriveFacts { ci, gate, ..facts_at("head-a") }, &limits),
                    want,
                    "{ci:?} at gate-check must leave the gate to answer"
                );
            }
        }
    }

    // ── the two carried-over gate properties (§2.1) ─────────────────────────

    #[test]
    fn a_pass_bound_to_an_old_head_or_an_old_body_is_not_a_pass() {
        let v = |verdict, head: &str, digest: &str| ReviewVerdict {
            pr: 1,
            block: "rev-std".into(),
            agent_id: "rev-1".into(),
            verdict,
            head: head.into(),
            body_digest: digest.into(),
            verified_body: false,
            open_findings: None,
            summary: String::new(),
            ts_ms: 0,
        };
        let now = Some("d1");
        // The positive control first, so every refusal below is a difference.
        assert!(lane_pass_is_current(
            Some(&v(Verdict::Pass, "head-a", "d1")),
            "head-a",
            now
        ));
        // Bound to an old head.
        assert!(!lane_pass_is_current(
            Some(&v(Verdict::Pass, "head-OLD", "d1")),
            "head-a",
            now
        ));
        // Bound to an old body.
        assert!(!lane_pass_is_current(
            Some(&v(Verdict::Pass, "head-a", "OLD")),
            "head-a",
            now
        ));
        // Unbound: an empty head can never equal a real one, so it fails closed.
        assert!(!lane_pass_is_current(
            Some(&v(Verdict::Pass, "", "d1")),
            "head-a",
            now
        ));
        // Not a pass at all, and no verdict at all.
        assert!(!lane_pass_is_current(
            Some(&v(Verdict::Fail, "head-a", "d1")),
            "head-a",
            now
        ));
        assert!(!lane_pass_is_current(None, "head-a", now));
        // "We could not check" is not "it changed": a verdict with no digest,
        // or a body that could not be read, does not stale a pass. Only
        // `Some(true)` re-opens a lane.
        assert!(lane_pass_is_current(
            Some(&v(Verdict::Pass, "head-a", "")),
            "head-a",
            now
        ));
        assert!(lane_pass_is_current(
            Some(&v(Verdict::Pass, "head-a", "d1")),
            "head-a",
            None
        ));
    }

    #[test]
    fn the_re_entry_point_is_the_first_lane_whose_pass_does_not_stand() {
        // Arc 8's "re-enters at the first stale lane" (§8 row 5).
        let lanes = vec![
            lane_fact("rev-std", Some(Verdict::Pass), "head-a", "d1"),
            lane_fact("rev-final", Some(Verdict::Pass), "head-OLD", "d1"),
            lane_fact("rev-extra", None, "", ""),
        ];
        assert_eq!(first_stale_lane(&lanes, "head-a", Some("d1")), 1);
        // Every lane standing is the "nothing left to review" answer arc 4 acts
        // on, and it is the length rather than an index.
        let all_good = vec![lane_fact("rev-std", Some(Verdict::Pass), "head-a", "d1")];
        assert_eq!(first_stale_lane(&all_good, "head-a", Some("d1")), 1);
        assert_eq!(first_stale_lane(&[], "head-a", Some("d1")), 0);
    }

    #[test]
    fn a_body_verification_never_stands_in_for_a_pass_at_another_head() {
        // **#2168 E2's delegation is bounded by the head, and this is the row
        // that pins it on the DRIVER side.** Its own test rather than a row
        // appended to `the_re_entry_point_is_the_first_lane_whose_pass_does_not_stand`,
        // because that test's first assertion covers the same predicate: a
        // neuter reddens it there and panics before ever reaching this, so the
        // red would evidence the older assertion and say nothing about this one
        // (CLAUDE.md — a red evidences only the assertion it reached and moved).
        //
        // What it is for: `pass_covers_body`'s direct arm shipped comparing
        // digests without asking whether the pass was bound to the head that
        // would merge. The gate's own loop filters that case out one line
        // earlier, so only the driver could reach it — and there a `pass`
        // recorded against code the worker had already fixed read as settling
        // the revision in front of the drive, walking straight past the lane
        // #1871 B1 exists to re-open.
        let across_heads = vec![
            verified_lane_fact("rev-std", "head-a", "d2"),
            lane_fact("rev-final", Some(Verdict::Pass), "head-OLD", "d1"),
        ];
        assert_eq!(
            first_stale_lane(&across_heads, "head-a", Some("d2")),
            1,
            "a verification of the body may stand in for a pass at THIS head, never for one \
             recorded against code the branch has left"
        );
        // The positive control: the same two lanes with rev-final's pass bound
        // to the live head — so the row above is the head deciding, not a
        // delegation that never fires.
        let same_head = vec![
            verified_lane_fact("rev-std", "head-a", "d2"),
            lane_fact("rev-final", Some(Verdict::Pass), "head-a", "d1"),
        ];
        assert_eq!(first_stale_lane(&same_head, "head-a", Some("d2")), 2);
    }

    #[test]
    fn a_held_step_always_carries_a_reason_and_no_other_step_ever_does() {
        // The same invariant `advance` enforces, asserted over everything
        // `decide` can actually emit — so a new arm cannot ship a reasonless
        // hold, or a reason riding an arc that is not one.
        let limits = DriveLimits::default();
        let mut seen_held = 0;
        let mut seen_other = 0;
        for st in DriveState::ALL {
            let mut e = entry_at(st);
            e.head = "head-a".into();
            for ci in [
                CiObservation::Green,
                CiObservation::Red,
                CiObservation::Conflicting,
                CiObservation::Pending,
                CiObservation::Unknown,
            ] {
                for worker in [
                    WorkerSignal::Silent,
                    WorkerSignal::Done,
                    WorkerSignal::Blocked,
                    WorkerSignal::Unresumable,
                ] {
                    for gate in [
                        GateOutcome::NotEvaluated,
                        GateOutcome::Satisfied,
                        GateOutcome::Unsatisfied,
                        GateOutcome::Unreadable,
                    ] {
                        for required in [
                            None,
                            Some(vec![lane_fact("rev-std", Some(Verdict::Fail), "head-a", "d1")]),
                        ] {
                            let facts = DriveFacts {
                                ci,
                                worker,
                                gate,
                                required_lanes: required.clone(),
                                ..facts_at("head-a")
                            };
                            match decide(&e, &facts, &limits) {
                                DriveStep::Advance {
                                    to: DriveState::Held,
                                    held_reason,
                                    ..
                                } => {
                                    assert!(held_reason.is_some());
                                    seen_held += 1;
                                }
                                DriveStep::Advance { held_reason, .. } => {
                                    assert!(held_reason.is_none());
                                    seen_other += 1;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        // Positive controls: the sweep really did produce both shapes, so the
        // assertions above are not passing over an empty walk.
        assert!(seen_held > 0, "the sweep produced no hold at all");
        assert!(seen_other > 0, "the sweep produced no ordinary advance");
    }

    // ── #2501: what the driver may release ──────────────────────────────────

    /// A `review-wait` entry with one lane, briefed at `head`, running in pane
    /// `rev-1` on session `sess-lane`.
    fn lane_open_at(head: &str) -> DriveEntry {
        let mut e = entry_at(DriveState::ReviewWait);
        e.head = head.to_string();
        e.open_lane("rev-std", "sess-lane", "rev-1", head, Some("d1"), 1_000, false, false);
        e
    }

    /// The step a live `review-wait` tick takes when it is simply waiting — the
    /// neutral one, so a release these tests observe is the LANE rule firing and
    /// not the step type.
    const LIVE: DriveStep = DriveStep::Wait;

    /// **The lane rule, both directions, on one fixture whose only moving part
    /// is the verdict's binding.**
    ///
    /// The two rows differ in the head the verdict is bound to and in nothing
    /// else — same entry, same pane, same live head — so a `releasable` that
    /// ignored currency and a `releasable` that released nothing are both red
    /// here. That is the discrimination CLAUDE.md's non-discriminating-fixture
    /// rule asks for: the candidate outputs must DIVERGE.
    #[test]
    fn a_lane_is_released_when_its_verdict_binds_to_the_live_revision_and_not_before() {
        for (label, at_head, digest, owed) in [
            ("answered about this revision", "h1", "d1", true),
            ("answered about an older head", "h0", "d1", false),
            ("answered before the body moved", "h1", "d0", false),
        ] {
            let e = lane_open_at("h1");
            let mut f = facts_at("h1");
            f.required_lanes =
                Some(vec![lane_fact("rev-std", Some(Verdict::Pass), at_head, digest)]);
            let got = releasable(&e, &f, &LIVE);
            assert_eq!(
                got.len(),
                usize::from(owed),
                "{label}: releasable said {got:?}"
            );
            if owed {
                assert_eq!(got[0].role, DrivenRole::Lane("rev-std".into()), "{label}");
                assert_eq!(got[0].reason, ReleaseReason::VerdictRecorded, "{label}");
            }
        }
    }

    /// **Word-blind, exactly as [`lane_verdict_is_current`] is.** A lane that
    /// said `fail` or `escalate` about this revision has answered, and what makes
    /// its pane finished is that it answered — not which word it chose.
    ///
    /// The negative control is in the row above and is the one that makes this
    /// mean something: currency, not the word, is what decides.
    #[test]
    fn the_lane_rule_reads_the_binding_and_never_the_word() {
        for word in [Verdict::Pass, Verdict::Fail, Verdict::Escalate] {
            let e = lane_open_at("h1");
            let mut f = facts_at("h1");
            f.required_lanes = Some(vec![lane_fact("rev-std", Some(word), "h1", "d1")]);
            assert_eq!(
                releasable(&e, &f, &LIVE).len(),
                1,
                "a lane that answered {:?} at this revision is finished with this round",
                word
            );
        }
    }

    /// **A lane with no pane on record releases nothing** — which is what makes
    /// the release idempotent rather than a loop that kills once and then keeps
    /// naming a candidate nothing can act on. Same facts as the positive row
    /// above; the only difference is that the pane slot is already empty.
    #[test]
    fn a_lane_whose_pane_is_already_released_is_not_a_candidate_again() {
        let mut e = lane_open_at("h1");
        let mut f = facts_at("h1");
        f.required_lanes = Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "h1", "d1")]);
        assert_eq!(releasable(&e, &f, &LIVE).len(), 1, "the control: it is a candidate first");

        let freed = e.release_pane(&DrivenRole::Lane("rev-std".into()), "sess-lane");
        assert_eq!(freed.as_deref(), Some("rev-1"), "the pane it un-recorded");
        assert_eq!(
            e.lane("rev-std").map(|l| l.session.as_str()),
            Some("sess-lane"),
            "…and the conversation, which is the whole promise"
        );
        assert!(releasable(&e, &f, &LIVE).is_empty(), "not a candidate a second time");
    }

    /// **A step that PARKS the drive releases nothing**, which is what keeps
    /// §6's hold notices true: they say the panes they name are still running
    /// and that a `drive_review` resume speaks to them again.
    ///
    /// The live arc is the positive control — otherwise "released nothing" is
    /// indistinguishable from a fixture that was never releasable at all — and
    /// the TERMINAL test below is the other half of the discrimination this pair
    /// needs after #2811 S1: park and end now diverge, so a `releasable` that
    /// treated them alike (in either direction) is red in one of the two.
    #[test]
    fn a_parking_step_releases_nothing() {
        let mut f = facts_at("h1");
        f.required_lanes = Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "h1", "d1")]);
        assert_eq!(
            releasable(&lane_open_at("h1"), &f, &LIVE).len(),
            1,
            "the positive control: these facts DO release under a live step"
        );
        let step = DriveStep::Advance {
            to: DriveState::Held,
            held_reason: Some(HeldReason::Escalate),
            bump: None,
        };
        assert!(
            releasable(&lane_open_at("h1"), &f, &step).is_empty(),
            "a step into held must release nothing"
        );
        let live = DriveStep::Advance {
            to: DriveState::GateCheck,
            held_reason: None,
            bump: None,
        };
        assert_eq!(
            releasable(&lane_open_at("h1"), &f, &live).len(),
            1,
            "…while an arc that leaves the drive live still does"
        );
    }

    /// **A TERMINAL step releases the lane whose verdict is current AND the
    /// worker, because there is nothing left to wait for** (#2811 S1).
    ///
    /// The pre-#2811 S1 behaviour is the failing side of every row: it released
    /// nothing at all here, so the orchestrator was handed the panes in the exit
    /// notice and killed them by hand — 33 kills in the measured session, and a
    /// fresh spawn on the next hand-back for want of a pane that was resumable
    /// all along.
    ///
    /// The last loop is the one guard on it: a `cancelled` that lands while the
    /// worker still owes this drive a round leaves that pane alone, so "the
    /// drive is over" is not read as "nobody is working".
    #[test]
    fn a_terminal_step_releases_the_lane_and_the_worker_it_is_finished_with() {
        for (label, to) in
            [("satisfied", DriveState::Satisfied), ("cancelled", DriveState::Cancelled)]
        {
            let mut e = lane_open_at("h1");
            e.advance(DriveState::GateCheck, None, None, 1_000).unwrap();
            e.record_worker_pane("w-1");
            let mut f = facts_at("h1");
            f.required_lanes = Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "h1", "d1")]);
            let step = DriveStep::Advance { to, held_reason: None, bump: None };
            let got = releasable(&e, &f, &step);
            assert_eq!(got.len(), 2, "{label}: releasable said {got:?}");
            assert_eq!(
                got[0].role,
                DrivenRole::Worker,
                "{label}: worker-first, as owned_panes reads"
            );
            assert_eq!(got[0].reason, ReleaseReason::DriveEnded, "{label}");
            assert_eq!(got[1].role, DrivenRole::Lane("rev-std".into()), "{label}");
            assert_eq!(got[1].reason, ReleaseReason::VerdictRecorded, "{label}");
        }

        // A lane whose verdict does NOT bind here is still not released, so the
        // terminal rule widened WHEN condition 2 is asked and not what it says.
        let mut e = lane_open_at("h1");
        e.advance(DriveState::GateCheck, None, None, 1_000).unwrap();
        let mut f = facts_at("h1");
        f.required_lanes = Some(vec![lane_fact("rev-std", Some(Verdict::Pass), "h0", "d1")]);
        let step = DriveStep::Advance { to: DriveState::Satisfied, held_reason: None, bump: None };
        let got = releasable(&e, &f, &step);
        assert!(
            !got.iter().any(|c| matches!(c.role, DrivenRole::Lane(_))),
            "a stale verdict is stale at a terminal step too: {got:?}"
        );
        // …and what IS proposed here is the worker side, on the session alone
        // (#3250): this fixture never handed back, so `worker_agent` is empty
        // and the pre-#3250 rule proposed nothing at all — which is the defect,
        // measured as an idle worker pane surviving `rd-satisfied` on PRs #3243
        // and #3248. The caller's barrier still decides per pane.
        assert_eq!(
            got,
            vec![ReleaseCandidate {
                role: DrivenRole::Worker,
                reason: ReleaseReason::DriveEnded,
                include_founding: true,
            }],
            "the satisfied exit asks about the worker session it was started on"
        );

        // The guard: cancelled while the worker still owes this drive a round.
        for pushed in [false, true] {
            let mut e = entry_at(DriveState::FixWait);
            if pushed {
                e.advance(DriveState::CiWait, None, None, 1_000).unwrap();
                assert!(e.fix_pushed(), "arc 7 is what makes this ci-wait a wait on the worker");
            }
            e.head = "h1".to_string();
            e.record_worker_pane("w-1");
            let mut f = facts_at("h1");
            f.worker = WorkerSignal::Silent;
            let step =
                DriveStep::Advance { to: DriveState::Cancelled, held_reason: None, bump: None };
            assert!(
                releasable(&e, &f, &step).is_empty(),
                "pushed={pushed}: a worker mid-round is not released by the drive being cancelled"
            );
        }
    }

    /// **Every reason a founding pane is refused, and the one shape that is
    /// admitted** (#3250, review round 1).
    ///
    /// The rows differ in ONE input each from the admitted row, so a predicate
    /// that dropped any single condition is red here — the discrimination
    /// CLAUDE.md's non-discriminating-fixture rule asks for. The two exclusion
    /// rows are the ones that matter: without the first a pane is asked about
    /// twice and audited twice, and without the second this drive's exit kills
    /// a pane ANOTHER live drive is still going to speak to.
    #[test]
    fn a_founding_pane_joins_the_release_unless_it_is_owned_here_or_driven_elsewhere() {
        let owned = vec!["w-owned".to_string()];
        let elsewhere = vec!["w-other-drive".to_string()];
        for (label, agent, admit) in [
            ("the founding pane the orchestrator handed over", "w-founding", true),
            ("a pane the drive itself owns is already in", "w-owned", false),
            ("another live drive's pane is that drive's", "w-other-drive", false),
            ("no pane at all", "  ", false),
        ] {
            assert_eq!(admit_session_pane(agent, &owned, &elsewhere), admit, "{label}");
        }
    }

    /// **One population holding an owned pane AND a founding one, in the order
    /// the rows claim** (#3250, review round 2) — the fixture the sort has to
    /// have, because with only one of the two kinds any order is the right one.
    ///
    /// The owned pane is the YOUNGER of the two, which is not a fixture choice
    /// but the shape of the thing: `owned` is written by a hand-back and a
    /// hand-back happens after the drive was handed the founding pane. So the
    /// merge order and the answer DIVERGE — drop the sort and this reddens with
    /// the two swapped, which is the mutation the claim owes.
    ///
    /// The undatable pane is asserted in the same run for the same reason it
    /// sorts last: a change that made an unknown age sort FIRST would put a row
    /// that never happens at the head of the list.
    #[test]
    fn the_release_population_reads_oldest_first_across_owned_and_founding_panes() {
        let age = |a: &str| match a {
            "w-founding" => Some(100u64),
            "w-handback" => Some(900),
            "w-second-founding" => Some(300),
            _ => None,
        };
        let got = release_population(
            vec!["w-handback".to_string()],
            &["w-founding".to_string(), "w-second-founding".to_string()],
            &[],
            &age,
        );
        assert_eq!(
            got,
            vec![
                "w-founding".to_string(),
                "w-second-founding".to_string(),
                "w-handback".to_string()
            ],
            "the hand-back pane is the youngest and comes last, whatever order it was merged in"
        );

        let with_ghost = release_population(
            vec!["w-handback".to_string()],
            &["w-ghost".to_string(), "w-founding".to_string()],
            &[],
            &age,
        );
        assert_eq!(
            with_ghost,
            vec!["w-founding".to_string(), "w-handback".to_string(), "w-ghost".to_string()],
            "a pane the registry cannot date sorts last, not first"
        );

        assert_eq!(
            release_population(
                vec!["w-handback".to_string()],
                &["w-founding".to_string(), "w-handback".to_string(), String::new()],
                &["w-second-founding".to_string()],
                &age,
            ),
            vec!["w-founding".to_string(), "w-handback".to_string()],
            "and the filtering is unchanged: owned, elsewhere and empty are all refused"
        );
    }

    /// **The founding list is recorded TOTAL, deduped, and never files a pane
    /// the drive has since resumed into** (#3250, review round 1).
    ///
    /// Total rather than additive is the half a resume depends on: a second
    /// `drive_review` re-reads the session, and a founding pane that has since
    /// died must not survive by having been recorded once. The `worker_agent`
    /// exclusion is what keeps one pane out of two lists, which the caller's
    /// dedup would otherwise have to catch every time.
    #[test]
    fn founding_panes_are_recorded_total_deduped_and_never_alongside_the_current_pane() {
        let mut e = entry_at(DriveState::CiWait);
        e.record_founding_panes(vec!["w-1".into(), "w-2".into(), "w-1".into(), String::new()]);
        assert_eq!(e.founding_panes, vec!["w-1".to_string(), "w-2".to_string()], "deduped");

        e.record_worker_pane("w-2");
        e.record_founding_panes(vec!["w-1".into(), "w-2".into()]);
        assert_eq!(
            e.founding_panes,
            vec!["w-1".to_string()],
            "the pane the drive has resumed into is owned, not founding"
        );

        e.record_founding_panes(vec!["w-3".into()]);
        assert_eq!(
            e.founding_panes,
            vec!["w-3".to_string()],
            "a re-read REPLACES: a founding pane that has gone does not survive the resume"
        );

        e.forget_worker_panes();
        assert!(e.founding_panes.is_empty(), "a different session forgets these too");
    }

    /// **A dead founding pane is dropped by the same prune that bounds the
    /// superseded lists** (#3250) — and a live one is never evicted.
    ///
    /// The `changed` answer is asserted in both directions because it is what
    /// decides whether the tick WRITES: a prune that dropped a pane and
    /// reported `false` would leave the record claiming a pane that is gone
    /// until something else happened to write.
    #[test]
    fn a_dead_founding_pane_is_pruned_and_says_so() {
        let mut e = entry_at(DriveState::CiWait);
        e.record_founding_panes(vec!["w-live".into(), "w-dead".into()]);
        assert!(
            e.forget_dead_panes(&|a: &str| a != "w-dead"),
            "dropping a founding pane is a change the caller must persist"
        );
        assert_eq!(e.founding_panes, vec!["w-live".to_string()]);
        assert!(!e.forget_dead_panes(&|_: &str| true), "…and a prune that drops nothing is no change");
    }

    /// **The worker rule: a hand-back outstanding plus `Done`, and nothing
    /// else** (#2501, corrected by #2811 S1).
    ///
    /// Every other worker signal is a row here, because each is a different
    /// reason to keep the pane: `Blocked` parks the drive for the orchestrator,
    /// which is about to talk to that pane; `Silent` has said nothing;
    /// `Unresumable` has already gone.
    ///
    /// **The two `ci-wait` `Done` rows are the correction, and the arc-7 anchor
    /// is their only moving part** — same state, same signal, same pane.
    /// Before #2811 S1 this table said `(ci-wait, Done) => false` outright,
    /// reasoning that such a report was about a hand-back already consumed;
    /// #2168 E1 had moved the consumption there, so the row was describing the
    /// very tick that consumes it, and 15 of one session's 20 hand-backs paid a
    /// held slot for the whole round. The `ci-wait`-without-a-push row keeps the
    /// original claim, which is still true of it: no hand-back is outstanding
    /// there, so a `Done` is not this drive's to consume.
    #[test]
    fn the_worker_is_released_only_on_the_tick_that_consumes_its_done_report() {
        // The step a tick takes when it really does consume the report: arc 2.
        // `LIVE` (a bare `Wait`) is the wrong instrument for this table — a tick
        // that consumes nothing must release nothing, which the last row pins.
        let arc2 = DriveStep::Advance {
            to: DriveState::ReviewWait,
            held_reason: None,
            bump: None,
        };
        for (state, pushed, signal, owed) in [
            (DriveState::FixWait, false, WorkerSignal::Done, true),
            (DriveState::FixWait, false, WorkerSignal::Blocked, false),
            (DriveState::FixWait, false, WorkerSignal::Silent, false),
            (DriveState::FixWait, false, WorkerSignal::Unresumable, false),
            (DriveState::CiWait, true, WorkerSignal::Done, true),
            (DriveState::CiWait, true, WorkerSignal::Blocked, false),
            (DriveState::CiWait, true, WorkerSignal::Silent, false),
            (DriveState::CiWait, true, WorkerSignal::Unresumable, false),
            (DriveState::CiWait, false, WorkerSignal::Done, false),
            (DriveState::ReviewWait, false, WorkerSignal::Done, false),
        ] {
            // Walked through the arcs rather than stamped, so no fixture here
            // can encode a `fix_pushed_ms` the machine would not have written.
            let mut e = if pushed {
                let mut e = entry_at(DriveState::FixWait);
                e.advance(DriveState::CiWait, None, None, 1_000).unwrap();
                e
            } else {
                entry_at(state)
            };
            assert_eq!(e.state(), state, "the fixture is in the state the row names");
            assert_eq!(
                e.fix_pushed(),
                pushed,
                "…and carries the arc-7 anchor iff the row says so"
            );
            e.head = "h1".to_string();
            e.record_worker_pane("w-1");
            let mut f = facts_at("h1");
            f.worker = signal;
            let got = releasable(&e, &f, &arc2);
            assert_eq!(
                got.len(),
                usize::from(owed),
                "{}/pushed={pushed}/{signal:?} released {got:?}",
                state.as_str()
            );
            if owed {
                assert_eq!(got[0].role, DrivenRole::Worker);
                assert_eq!(got[0].reason, ReleaseReason::ReportConsumed);
            }
        }

        // **A tick that WAITS has consumed nothing**, and this is a real state
        // rather than a hypothetical: a worker that reports before the matrix
        // settles is a `Done` sitting in `ci-wait` while `decide_ci_wait`
        // answers `Wait`. The only difference from the positive row above is the
        // step, so the fixture discriminates on exactly the axis this pins.
        let mut e = entry_at(DriveState::FixWait);
        e.advance(DriveState::CiWait, None, None, 1_000).unwrap();
        e.head = "h1".to_string();
        e.record_worker_pane("w-1");
        let mut f = facts_at("h1");
        f.worker = WorkerSignal::Done;
        assert_eq!(releasable(&e, &f, &arc2).len(), 1, "the control: the arc DOES release");
        assert!(
            releasable(&e, &f, &LIVE).is_empty(),
            "a tick that takes no arc has consumed no report, so it releases nothing"
        );
    }

    /// **One predicate, two callers** (#2811 S1) — `kickback_owed` and
    /// `releasable` ask "is a hand-back outstanding" through
    /// [`DriveEntry::handback_outstanding`] and not through two spellings of it,
    /// which is how the #2811 S1 drift happened in the first place.
    ///
    /// Pinned as a property of the ENTRY over every live state, so a later edit
    /// that re-inlines either reading diverges from this table rather than from
    /// the other caller silently.
    #[test]
    fn a_handback_is_outstanding_in_fix_wait_and_in_a_pushed_ci_wait_only() {
        for state in DriveState::ALL {
            if state.is_terminal() || state.is_parked() {
                continue;
            }
            let e = entry_at(state);
            assert_eq!(
                e.handback_outstanding(),
                state == DriveState::FixWait,
                "{} without a push",
                state.as_str()
            );
        }
        let mut e = entry_at(DriveState::FixWait);
        e.advance(DriveState::CiWait, None, None, 1_000).unwrap();
        assert!(e.handback_outstanding(), "arc 7 keeps the wait alive in ci-wait");
        // Arc 2 ends it, and that is the tick the release rides.
        e.advance(DriveState::ReviewWait, None, None, 1_000).unwrap();
        assert!(!e.handback_outstanding(), "…and the arc out of it ends the wait");
    }

    /// **A release refuses when the conversation cannot be named.** Killing the
    /// pane of a lane whose session nothing can resolve would end the reviewer's
    /// memory of the PR, which is the one thing this mechanism promises to keep.
    ///
    /// The positive control is the second half: the same lane, with a session,
    /// releases — so the refusal is the empty session and not the fixture.
    #[test]
    fn a_release_refuses_a_lane_whose_session_cannot_be_named() {
        let mut e = entry_at(DriveState::ReviewWait);
        e.open_lane("rev-std", "", "rev-1", "h1", Some("d1"), 1_000, false, false);
        assert_eq!(
            e.release_pane(&DrivenRole::Lane("rev-std".into()), ""),
            None,
            "no session on the record and none resolved: the pane stays"
        );
        assert_eq!(
            e.lane("rev-std").map(|l| l.agent.as_str()),
            Some("rev-1"),
            "…and the record still names it, so the next tick can try again"
        );
        assert_eq!(
            e.release_pane(&DrivenRole::Lane("rev-std".into()), "sess-resolved"),
            Some("rev-1".to_string()),
            "the control: a resolved session releases"
        );
        assert_eq!(
            e.lane("rev-std").map(|l| l.session.as_str()),
            Some("sess-resolved"),
            "…and the resolved id is written onto the record, so the resume no longer \
             depends on the dead pane's own row"
        );
    }

    /// The worker's half of the same refusal, and the pane list that follows
    /// from it: a released pane is not in [`DriveEntry::owned_panes`], which is
    /// what keeps every exit notice's "still running" true.
    #[test]
    fn a_released_worker_leaves_the_owned_pane_list() {
        let mut e = entry_at(DriveState::FixWait);
        e.record_worker_pane("w-1");
        assert!(
            e.owned_panes().iter().any(|(a, r)| a == "w-1" && *r == DrivenRole::Worker),
            "the control: it is owned first"
        );
        assert_eq!(e.release_pane(&DrivenRole::Worker, ""), Some("w-1".to_string()));
        assert!(
            !e.owned_panes().iter().any(|(a, _)| a == "w-1"),
            "a released pane must not be named by a notice that says its panes are still running"
        );
        assert_eq!(
            e.release_pane(&DrivenRole::Worker, ""),
            None,
            "…and there is nothing left to release twice"
        );
    }

    /// `DriveEntry::new` records the worker SESSION, so the worker arm's
    /// fail-closed branch needs an entry that has none — the shape a drive
    /// started without one would have. Pinned because the branch is otherwise
    /// unreachable from the fixtures above and would read as dead code.
    #[test]
    fn a_worker_with_no_recorded_session_is_never_released() {
        let mut e = entry_at(DriveState::FixWait);
        e.worker_session = String::new();
        e.record_worker_pane("w-1");
        assert_eq!(
            e.release_pane(&DrivenRole::Worker, "sess-ignored"),
            None,
            "the worker's session is the entry's own; a caller may not supply one for it"
        );
    }

    // ── #3367 item 1: the driver's own non-blocking round ───────────────────

    /// A `pass` at `head` whose summary is `summary` — the one input the
    /// non-blocking round reads that the other lane fixtures leave empty.
    fn nit_lane(block: &str, head: &str, summary: &str) -> LaneFact {
        let mut l = lane_fact(block, Some(Verdict::Pass), head, "d1");
        if let Some(v) = l.verdict.as_mut() {
            v.summary = summary.to_string();
        }
        l
    }

    /// A `gate-check` drive whose gate has just answered satisfied over `lanes`.
    fn satisfied_over(lanes: Vec<LaneFact>) -> (DriveEntry, DriveFacts) {
        let mut e = entry_at(DriveState::GateCheck);
        e.head = "head-a".into();
        let f = DriveFacts {
            required_lanes: Some(lanes),
            gate: GateOutcome::Satisfied,
            ..facts_at("head-a")
        };
        (e, f)
    }

    fn nit_limits(n: u32) -> DriveLimits {
        DriveLimits::default().with_fix_nonblocking_rounds(n)
    }

    const NIT_ROUND: DriveStep = DriveStep::Advance {
        to: DriveState::FixWait,
        held_reason: None,
        bump: Some(Counter::NonblockingRound),
    };

    /// **The grammar, over the phrasings this repo's reviewers actually
    /// write** — the first five rows are verdict summaries recorded in this
    /// group's own `verdicts/` directory, cut at the clause that matters.
    #[test]
    fn a_stated_finding_count_is_read_and_anything_ambiguous_is_unknown() {
        let s = |t: &str| stated_findings(t);
        let both = |b, n| StatedFindings { blocking: b, non_blocking: n };
        // Recorded summaries.
        assert_eq!(s("Completeness traced. 0 blocking; 3 non-blocking: the ..."), both(Some(0), Some(3)));
        assert_eq!(s("PASS. Two non-blocking notes, not routed"), both(None, Some(2)));
        assert_eq!(s("rev-final, whole-diff at 5a56b2fd. No blocking finding."), both(Some(0), None));
        assert_eq!(s("whole diff: 0 blocking findings. Round-2 fix verified"), both(Some(0), None));
        assert_eq!(s("Round-3 blocking finding fixed: the paragraph now says"), both(None, None));
        // The colon form, and the normalised non- spellings.
        assert_eq!(s("blocking: 0, non-blocking: 2"), both(Some(0), Some(2)));
        assert_eq!(s("0 blockers, 1 non blocking"), both(Some(0), Some(1)));
        assert_eq!(s("zero blocking, 4 nonblocking"), both(Some(0), Some(4)));
        // A `blocking` token is never the tail of a non-blocking one: this
        // states NO blocking count, not a count of three.
        assert_eq!(s("3 non-blocking"), both(None, Some(3)));
        // A count is a whole token: `round-6` is not six.
        assert_eq!(s("Both round-6 blocking findings fixed"), both(None, None));
        // Two DIFFERENT counts for one class are unknown, never the smaller.
        assert_eq!(s("1 blocking finding from round 1 fixed; 0 blocking now"), both(None, None));
        // The same count twice is still that count.
        assert_eq!(s("0 blocking. Summary: 0 blocking, 2 non-blocking"), both(Some(0), Some(2)));
        assert_eq!(s(""), both(None, None));
    }

    /// **Default `0` is today's behaviour**: the most loop-shaped residual
    /// there is still wakes the orchestrator.
    #[test]
    fn at_the_default_a_nonblocking_residual_still_satisfies() {
        let (e, f) =
            satisfied_over(vec![nit_lane("rev-std", "head-a", "0 blocking; 2 non-blocking")]);
        assert_eq!(decide(&e, &f, &DriveLimits::default()), DriveStep::to(DriveState::Satisfied));
        // The positive control, so the assertion above is about the default and
        // not about a residual the round could never have taken.
        assert_eq!(decide(&e, &f, &nit_limits(1)), NIT_ROUND);
    }

    /// **Every wake the brief names, one row each, beside the one residual that
    /// loops** — so a predicate that looped on any of them reddens its own row.
    #[test]
    fn a_nonblocking_round_is_taken_only_on_a_positively_stated_nit_residual() {
        let l = nit_limits(2);
        let sat = DriveStep::to(DriveState::Satisfied);
        let case = |lanes: Vec<LaneFact>| {
            let (e, f) = satisfied_over(lanes);
            decide(&e, &f, &l)
        };
        // Loops: every lane says 0 blocking, one says 2 non-blocking, the other
        // states no non-blocking count at all.
        assert_eq!(
            case(vec![
                nit_lane("rev-std", "head-a", "0 blocking; 2 non-blocking"),
                nit_lane("rev-final", "head-a", "No blocking finding."),
            ]),
            NIT_ROUND
        );
        // Wakes: zero open findings anywhere.
        assert_eq!(case(vec![nit_lane("rev-std", "head-a", "0 blocking, 0 non-blocking")]), sat);
        // Wakes: a lane that did not state its blocking count (fail-safe).
        assert_eq!(
            case(vec![
                nit_lane("rev-std", "head-a", "0 blocking; 2 non-blocking"),
                nit_lane("rev-final", "head-a", "Two non-blocking notes"),
            ]),
            sat
        );
        // Wakes: a finding a lane labels blocking, even on a pass.
        assert_eq!(case(vec![nit_lane("rev-std", "head-a", "1 blocking; 2 non-blocking")]), sat);
        // Wakes: the counting pass is bound to an OLD head, so it states nothing
        // about this revision.
        assert_eq!(case(vec![nit_lane("rev-std", "head-old", "0 blocking; 2 non-blocking")]), sat);
    }

    /// **The bound is SHARED and never exceeded**: a drive with its INVARIANT 9
    /// review rounds spent wakes even with nit rounds left, and a drive with
    /// its nit rounds spent wakes even with review rounds left.
    #[test]
    fn the_nonblocking_round_spends_the_shared_bound_and_its_own() {
        let lanes = vec![nit_lane("rev-std", "head-a", "0 blocking; 2 non-blocking")];
        let (mut e, f) = satisfied_over(lanes.clone());
        e.counters.review_rounds = MAX_ROUNDS_CEILING;
        assert_eq!(decide(&e, &f, &nit_limits(3)), DriveStep::to(DriveState::Satisfied));
        e.counters.review_rounds = MAX_ROUNDS_CEILING - 1;
        assert_eq!(decide(&e, &f, &nit_limits(3)), NIT_ROUND, "one review round left: taken");

        let (mut e, f) = satisfied_over(lanes);
        e.nit_rounds = 1;
        assert_eq!(decide(&e, &f, &nit_limits(1)), DriveStep::to(DriveState::Satisfied));
        assert_eq!(decide(&e, &f, &nit_limits(2)), NIT_ROUND, "control: one nit round left");

        // `advance` pays for it on BOTH counters, in the one arm.
        let (mut e, _) = satisfied_over(Vec::new());
        e.advance(DriveState::FixWait, None, Some(Counter::NonblockingRound), 3_000).unwrap();
        assert_eq!((e.counters.review_rounds, e.nit_rounds), (1, 1));
        assert!(e.nit_handback, "the fix-wait it entered knows it is a nit round");
        // …and every OTHER arc into fix-wait says it is not one.
        e.advance(DriveState::CiWait, None, None, 4_000).unwrap();
        e.advance(DriveState::FixWait, None, Some(Counter::CiAttempts), 5_000).unwrap();
        assert!(!e.nit_handback, "a red-CI hand-back after a nit round is not a nit round");
        // A repo cannot widen it past the ceiling either.
        assert_eq!(nit_limits(9).fix_nonblocking_rounds, MAX_ROUNDS_CEILING);
    }

    /// **A worker that changed nothing does not buy a second identical round.**
    #[test]
    fn a_gate_satisfied_again_at_the_handed_back_revision_wakes() {
        let lanes = vec![nit_lane("rev-std", "head-a", "0 blocking; 2 non-blocking")];
        let (mut e, mut f) = satisfied_over(lanes);
        e.nit_rounds = 1;
        e.nit_head = "head-a".into();
        e.nit_digest = "d1".into();
        assert_eq!(decide(&e, &f, &nit_limits(3)), DriveStep::to(DriveState::Satisfied));
        // An unreadable body at the same head is not a move.
        f.body_digest = None;
        assert_eq!(decide(&e, &f, &nit_limits(3)), DriveStep::to(DriveState::Satisfied));
        // A body edit at the same head IS one (the worker answered in the body).
        f.body_digest = Some("d2".into());
        assert_eq!(decide(&e, &f, &nit_limits(3)), NIT_ROUND);
        // …and so is a push.
        let (mut e2, f2) =
            satisfied_over(vec![nit_lane("rev-std", "head-b", "0 blocking; 1 non-blocking")]);
        e2.nit_rounds = 1;
        e2.nit_head = "head-a".into();
        e2.nit_digest = "d1".into();
        let f2 = DriveFacts { head: "head-b".into(), ..f2 };
        assert_eq!(decide(&e2, &f2, &nit_limits(3)), NIT_ROUND);
    }

    /// **An entry written before #3367 parses, and reads as no rounds run.**
    #[test]
    fn a_pre_3367_entry_reads_as_no_nonblocking_rounds_and_round_trips_them() {
        let e = entry_at(DriveState::CiWait);
        let old = serde_json::to_string(&e).unwrap();
        assert!(!old.contains("nit_rounds"), "absent is the resting shape: {old}");
        let back: DriveEntry = serde_json::from_str(&old).unwrap();
        assert_eq!(
            (back.nit_rounds, back.nit_handback, back.auto_report.as_deref()),
            (0, false, None)
        );
        let mut e = e;
        e.nit_rounds = 2;
        e.auto_report = Some("w-7 reports done".into());
        let back: DriveEntry = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back.nit_rounds, 2);
        assert_eq!(back.auto_report.as_deref(), Some("w-7 reports done"));
    }

    // ── #3367 item 5: `open_findings` and the clean case ─────────────────────

    /// A passing lane that DECLARED `open_findings` — the structured count.
    fn declared_lane(block: &str, head: &str, open: Option<u32>, summary: &str) -> LaneFact {
        let mut l = nit_lane(block, head, summary);
        if let Some(v) = l.verdict.as_mut() {
            v.open_findings = open;
        }
        l
    }

    /// **The declaration is read first, and the parser is its fallback** —
    /// one count with two sources, never two counts.
    #[test]
    fn a_declared_open_findings_count_wins_and_the_parser_is_its_fallback() {
        let both = |b, n| StatedFindings { blocking: b, non_blocking: n };
        // No declaration: exactly the parser, as before item 5.
        assert_eq!(verdict_findings("0 blocking; 2 non-blocking", None), both(Some(0), Some(2)));
        assert_eq!(verdict_findings("looks good", None), both(None, None));
        // A declaration over silent prose: a pass's total is all non-blocking.
        assert_eq!(verdict_findings("looks good", Some(2)), both(Some(0), Some(2)));
        assert_eq!(verdict_findings("looks good", Some(0)), both(Some(0), Some(0)));
        // A declaration over DISAGREEING prose: the declaration wins.
        assert_eq!(verdict_findings("0 blocking; 3 non-blocking", Some(1)), both(Some(0), Some(1)));
        // A stated blocking count inside the total is honoured, not zeroed.
        assert_eq!(verdict_findings("1 blocking", Some(3)), both(Some(1), Some(2)));
        // …and one ABOVE the total is a contradiction, which is unknown.
        assert_eq!(verdict_findings("1 blocking", Some(0)), both(None, None));
        // `declared_clean` needs the declaration itself — prose alone never.
        assert!(declared_clean("looks good", Some(0)));
        assert!(!declared_clean("0 blocking, 0 non-blocking", None), "an omission is never 0");
        assert!(!declared_clean("1 blocking", Some(0)), "the summary keeps a veto");
        assert!(!declared_clean("", Some(1)));
    }

    /// **`open_findings: 0` ends the nit loop for that lane** (item 5's
    /// interaction with item 1). Both rows reverse what the parser alone
    /// decides, which is what makes them discriminating: the declared zero
    /// beats a summary still carrying round one's `2 non-blocking`, and a
    /// declared `2` licenses the round a summary with no counts could not.
    #[test]
    fn a_declared_zero_ends_the_nit_loop_and_a_declared_count_can_start_it() {
        let sat = DriveStep::to(DriveState::Satisfied);
        let round = |lanes: Vec<LaneFact>| {
            let (e, f) = satisfied_over(lanes);
            decide(&e, &f, &nit_limits(2))
        };
        let stale_prose = "Round 1: 0 blocking; 2 non-blocking. Round 2: both fixed.";
        // The control: the parser alone loops on this summary.
        assert_eq!(round(vec![declared_lane("rev-std", "head-a", None, stale_prose)]), NIT_ROUND);
        // The declaration ends it.
        assert_eq!(round(vec![declared_lane("rev-std", "head-a", Some(0), stale_prose)]), sat);
        // One lane at 0, the other still declaring nits: the loop continues —
        // for the lane that has something open, which is the only one that can.
        assert_eq!(
            round(vec![
                declared_lane("rev-std", "head-a", Some(0), stale_prose),
                declared_lane("rev-final", "head-a", Some(1), "one nit left"),
            ]),
            NIT_ROUND
        );
        // A declared count with prose that states none: the parser alone woke.
        assert_eq!(round(vec![declared_lane("rev-std", "head-a", None, "one nit left")]), sat);
        assert_eq!(round(vec![declared_lane("rev-std", "head-a", Some(1), "one nit left")]), NIT_ROUND);
    }

    /// **A stated non-zero count vetoes a declared zero, in either class**
    /// (#3388 review round 2). "Clean" means nothing is left to disposition, so
    /// `open_findings: 0` beside `2 non-blocking` is not clean — and nor is a
    /// summary whose counts DISAGREE, which `stated_findings` reads as unknown
    /// and a veto keyed on that reading would let through. The positive rows
    /// are the control: a veto that refused everything would fail them.
    #[test]
    fn a_stated_nonzero_count_in_either_class_vetoes_a_declared_zero() {
        // Vetoed.
        for s in [
            "0 blocking; 2 non-blocking",
            "Two non-blocking notes, not routed",
            "non-blocking: 1",
            "1 blocking finding from round 1 fixed; 0 blocking now",
            "0 non-blocking now; 3 non-blocking in round 1",
        ] {
            assert!(!declared_clean(s, Some(0)), "{s:?} states an open finding: not clean");
        }
        // Clean: no count at all, or every count zero.
        for s in ["lgtm", "0 blocking, 0 non-blocking", "No blocking finding. zero non-blocking"] {
            assert!(declared_clean(s, Some(0)), "{s:?} states nothing open: clean");
        }
        // The nit loop is untouched: the declaration still wins there, so a
        // declared 0 ends the loop even where the clean flag is vetoed.
        assert_eq!(
            verdict_findings("0 blocking; 2 non-blocking", Some(0)),
            StatedFindings { blocking: Some(0), non_blocking: Some(0) }
        );
    }

    /// **The clean case is positive on every axis**: one row per thing that
    /// must hold, each flipped alone against the one base case that IS clean.
    #[test]
    fn the_clean_case_needs_every_lane_declaring_zero_at_the_head_with_ci_green() {
        let clean_lanes = || {
            vec![
                declared_lane("rev-std", "head-a", Some(0), "nothing left"),
                declared_lane("rev-final", "head-a", Some(0), "0 blocking, 0 non-blocking"),
            ]
        };
        let facts = |lanes: Vec<LaneFact>| {
            let (_, f) = satisfied_over(lanes);
            DriveFacts { ci: CiObservation::Green, ..f }
        };
        // The positive control: without it every row below could be a
        // predicate that answers false for everything.
        assert!(gate_is_clean(&facts(clean_lanes())));

        let mut rows = 0;
        let mut flip = |what: &str, f: DriveFacts| {
            assert!(!gate_is_clean(&f), "{what} must not be clean");
            rows += 1;
        };
        // A lane that omitted the field — though its prose says zero.
        let mut l = clean_lanes();
        l[1].verdict.as_mut().unwrap().open_findings = None;
        flip("an omitted declaration", facts(l));
        // A lane declaring a nit.
        let mut l = clean_lanes();
        l[0].verdict.as_mut().unwrap().open_findings = Some(1);
        flip("a declared open finding", facts(l));
        // A declaration the summary contradicts.
        let mut l = clean_lanes();
        l[0].verdict.as_mut().unwrap().summary = "1 blocking".into();
        flip("a contradicted zero", facts(l));
        // …in the NON-blocking class too (#3388 review round 2).
        let mut l = clean_lanes();
        l[0].verdict.as_mut().unwrap().summary = "0 blocking; 2 non-blocking".into();
        flip("a zero beside stated nits", facts(l));
        // A zero declared at an OLD head says nothing about this one.
        let mut l = clean_lanes();
        l[0].verdict.as_mut().unwrap().head = "head-old".into();
        flip("a stale pass", facts(l));
        // Not a pass.
        let mut l = clean_lanes();
        l[0].verdict.as_mut().unwrap().verdict = Verdict::Escalate;
        flip("an escalate", facts(l));
        // A required lane with no verdict at all.
        let mut l = clean_lanes();
        l[1].verdict = None;
        flip("an unrecorded lane", facts(l));
        // No lanes: nothing was reviewed, so nothing is clean.
        flip("an empty lane list", facts(Vec::new()));
        // CI not green.
        flip("CI pending", DriveFacts { ci: CiObservation::Pending, ..facts(clean_lanes()) });
        // The gate not satisfied.
        flip(
            "an unsatisfied gate",
            DriveFacts { gate: GateOutcome::Unsatisfied, ..facts(clean_lanes()) },
        );
        // Routing unaccountable.
        flip("unknown lanes", DriveFacts { required_lanes: None, ..facts(clean_lanes()) });
        assert_eq!(rows, 11, "every axis has its row");

        // And the clean case is not a nit round: nothing is open to hand back.
        let (e, f) = satisfied_over(clean_lanes());
        assert_eq!(decide(&e, &f, &nit_limits(3)), DriveStep::to(DriveState::Satisfied));
    }
}
