//! Integration tests for the quick drive (#3679) — one short-lived
//! plan → work → review run with no orchestrator pane.
//!
//! Design note: `docs/design/quick-orchestration.md`. The pure core's own
//! properties — every arc of the state machine, the round bound, the store's
//! refusals — are pinned inline in `crates/loomux-engine/src/quickdrive.rs`.
//! What lives here is everything that needs the registry: the interception of
//! `report` and `message_orchestrator`, the step that moves a run, the
//! hand-over ladder, the briefs a pane is actually typed, the notice, and what
//! a restart does.
//!
//! A directory target for `tests/reviewdrive/`'s reasons: CLAUDE.md constraint
//! 4 makes the target KIND what matters (an integration test carries the
//! comctl32-v6 manifest link args), and one binary with one module namespace
//! keeps `cargo test -p orrerix --test quickdrive <filter>` a single command.
//!
//! No test here spawns a real agent CLI (constraint 3). A worker is opened in
//! a real `git` worktree of a throwaway repo, exactly as `tests/reviewdrive/`
//! opens a reviewer lane; nothing else starts a child process.

use loomux_lib::orchestration::mcp::dispatch;
use loomux_lib::orchestration::needsyou;
use loomux_lib::orchestration::quickdrive::{
    self, QuickHeld, QuickSide, QuickSignal, QuickState,
};
use loomux_lib::orchestration::{
    AgentStatus, Caller, GroupId, Guardrails, OrchRegistry, QdDriveReport, QdOwner, QdSignal,
    QuickStartRequest, QuickStepConfig, Role, QD_BODY_CAP,
};
use serde_json::{json, Value};

mod helpers;
mod guards;
mod run;
mod handback;
mod exits;
mod briefs;
mod plan;
mod controls;
mod restart;

use helpers::*;
