//! Functional tests for the block model and `.loomux/workflow.yml` (#222).
//!
//! These live as integration tests (not unit tests) because test executables
//! that link the full lib need the common-controls-v6 manifest embedded via
//! `rustc-link-arg-tests` (see build.rs / test.manifest), which cargo only
//! applies to integration-test targets — CLAUDE.md constraint 4.
//!
//! The two invariants most of this file exists to defend:
//!
//! 1. **A workflow file can never grant a capability.** It selects a `kind` from
//!    a closed enum; there is no `read_only: false`, no fifth class, and an
//!    unknown `kind` is rejected outright rather than becoming a worker.
//! 2. **A repo with no workflow file behaves exactly as it did before blocks
//!    existed** — down to the emitted command line.
//!
//! No test here spawns a real agent CLI. The command lines are *built* and
//! asserted; nothing is executed.

use loomux_lib::orchestration::GroupId;
use loomux_lib::orchestration::intake;
use loomux_lib::orchestration::mcp::dispatch;
use loomux_lib::orchestration::profiles::{self, ProfileMode};
use loomux_lib::orchestration::workflow::{self, GateRequire};
use loomux_lib::orchestration::{
    block_contract_text, cli_caps, command_line_length_guard, copilot_tools_gap_warning, Caller, Containment, ContractCarrier, Guardrails, Launch, OrchRegistry, Role, ToolsGapAction, CLI_CAPS, EFFORT_LEVELS,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

// #3498 P6: this target was one file; it is now a module tree with one test
// binary, so `cargo test -p orrerix --test workflow <filter>` is unchanged.
// Every module opens with `use super::*`, which sees the imports above and
// the `pub(crate)` items the globs below re-export — the one module namespace
// the single file used to be.
mod helpers;
mod guards;
mod schema;
mod personas;
mod roster;
mod copilot;
mod nativeflags;
mod spawn;
mod goldens;
mod gates;
mod dogfood;
mod intakeprofile;
mod blocks;
mod namedworkflows;
mod driverkey;

use helpers::*;
use schema::*;
use roster::*;
use nativeflags::*;
use spawn::*;
use goldens::*;
use gates::*;
use dogfood::*;
