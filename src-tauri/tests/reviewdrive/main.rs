//! Integration tests for the engine-driven review driver (#1778 S3/S4).
//!
//! Design note: `docs/design/review-driver.md`. The pure core's own properties
//! are pinned inline in `crates/loomux-engine/src/reviewdrive/`; what lives
//! here is everything that needs a **crate boundary** or the registry — the
//! tick's wiring, the interception arms, the tools, and the brief rendering.
//!
//! # Why a new file rather than `tests/orchestration/`
//!
//! The plan on #1778 named `tests/orchestration/`, and its parenthetical says
//! why: CLAUDE.md constraint 4, integration tests rather than unit tests,
//! because a test executable linking the full lib needs the comctl32-v6
//! manifest `build.rs` embeds through `-tests`-scoped link args. A new
//! integration-test *target* satisfies that identically — the reason is the
//! target kind, not the file name. What a new file also avoids is the
//! end-of-file append conflict CLAUDE.md catalogues on that file: every test
//! block there ends `);` + `}`, so two branches appending to a 33k-line file
//! get that tail matched as common context and each side arrives ending
//! mid-assertion. `tests/mergequeue.rs` is the standing precedent for giving a
//! subsystem its own file, and this subsystem has slices still to land.
//! `tests/smoke.rs` is untouched, per that same constraint.
//!
//! No test here spawns a real agent CLI (constraint 3) or a real `git`/`gh`
//! child.

use loomux_lib::orchestration::reviewdrive::{
    self, CiObservation, Counter, Counters, DriveEntry, DriveFacts, DriveLimits, DriveState,
    DriveStep, GateOutcome, HeldReason, LaneFact, WorkerSignal, MAX_REBASE_CEILING,
    MAX_ROUNDS_CEILING, NOTICE_RETENTION_MS,
};

use loomux_lib::orchestration::workflow::{ReviewVerdict, Verdict, body_digest};
use loomux_lib::orchestration::mqdriver::CmdOut;
use loomux_lib::orchestration::rddrive::RdRunner;
use loomux_lib::orchestration::mcp::dispatch;
use loomux_lib::orchestration::{
    exit_notice_route, pane_delivery_readiness, AgentStatus, Caller, Delivery, ExitInitiator,
    ExitNoticeRoute, GroupId, Guardrails, Launch, OrchRegistry, PaneNotReady, RdDriveReport, Role,
    TaskPatch,
};
use serde_json::json;

// #3498 P6: this target was one file; it is now a module tree with one test
// binary, so `cargo test -p orrerix --test reviewdrive <filter>` is unchanged.
// Every module opens with `use super::*`, which sees the imports above and
// the `pub(crate)` items the globs below re-export — the one module namespace
// the single file used to be.
mod helpers;
mod guards;
mod decide;
mod tick;
mod lanes;
mod conflicts;
mod loopfixes;
mod handback;
mod lanereuse;
mod caps;
mod release;
mod notices;
mod panes;
mod limits;
mod restart;
mod takeover;
mod autostart;
mod reload;

use helpers::*;
use decide::*;
use tick::*;
use lanes::*;
use loopfixes::*;
use handback::*;
use lanereuse::*;
use caps::*;
use release::*;
use panes::*;
use limits::*;
use restart::*;
use takeover::*;
