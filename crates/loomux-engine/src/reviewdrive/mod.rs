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
mod entry;
mod facts;
mod decision;
mod findings;
mod release;

pub use states::*;
pub use counters::*;
pub use store::*;
pub use bounds::*;
pub use entry::*;
pub use facts::*;
pub use decision::*;
pub use findings::*;
pub use release::*;

#[cfg(test)]
use self::{
    decision::tests::{facts_at, lane_fact, verified_lane_fact},
    entry::tests::entry_at,
    store::tests::NOTE_EXAMPLE,
};
