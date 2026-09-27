//! Roles as data: the **block model** and `<repo>/.orrerix/workflow.yml` (#222;
//! the legacy `.loomux/` spelling is still discovered — see [`workflow_path`]).
//!
//! Until now an agent's identity *was* its [`Role`] — a closed 4-variant enum
//! that simultaneously decided the persona, the template, the model, the CLI
//! and the capabilities. That made "five reviewers, each with its own focus
//! prompt and model" impossible to express.
//!
//! A **block** splits those apart:
//!
//! - the **[`BlockId`]** (a string, e.g. `rev-security`) is the *identity*;
//! - [`Role`] survives as the block's **capability class** (`kind`) — the
//!   structural guarantees loomux enforces (deny-flags, cwd rule, MCP tool
//!   scope) still come from a **closed enum**;
//! - persona (`prompt` / `profile`), `cli` and `model` are **unbounded data**.
//!
//! So you can declare as many reviewers as you like — but every one of them is a
//! *reviewer* in the capability sense, and a repo file cannot make one anything
//! else.
//!
//! Be precise about what "the capability sense" buys, because the enum enforces
//! less than the word suggests — [`Role::containment`](crate::model::Role::containment)
//! is the exact per-class answer. A **planner** is structurally read-only — its
//! file-editing tools and `git commit`/`git push` are denied at the CLI level, so
//! `is_read_only()` is a real, mechanical guarantee. A **reviewer** is denied the
//! CLI's file-editing tools too (#462), but keeps the shell — running the tests
//! is its job — so its "never pushes" stays *instruction-backed*, and so does
//! "never writes a file" for anything a shell command can do. What the closed
//! enum guarantees is that a repo file cannot *change* which posture a block
//! gets — not that any posture is a sandbox. (See
//! `docs/design/orchestration.md` on structural vs instruction-backed enforcement;
//! the capability table in `docs/design/workflows.md` is the honest summary.)
//!
//! # The capability-closure rule (the security spine)
//!
//! **A workflow file can never grant a capability.** `kind` *selects* from the
//! closed enum; there is no `read_only: false` escape hatch, no `allow_write`,
//! no way to spell a fifth capability class. A repo file is untrusted input —
//! it is authored by whoever opened a PR against the repo — and under
//! `auto_ops` nobody approves its agents' tool calls. Everything a block can
//! influence is therefore either (a) inert text (a persona prompt), or (b) a
//! choice from a value set loomux already ships (`kind`, `cli`, `model`).
//! Every string that reaches a shell line is sanitized first ([`sanitize_id`],
//! [`sanitize_display`], `sanitize_allow`, `sanitize_model`), and a `profile:`
//! path is confined to the repo (no `..`, no absolute paths, no drive letters).
//!
//! # Failure policy
//!
//! A broken workflow file is **audited and skipped, never fatal**: the group
//! falls back to [`default_roster`] — today's fixed 4-block roster — and every
//! agent still spawns. The one thing that is *not* silently tolerated is an
//! unknown `kind`: coercing it to `worker` would hand an unrecognized block
//! write access, so it is a hard validation error that drops the file. (The
//! pre-#222 code did exactly that coercion in two places; both are gone.)
//!
//! # Schema
//!
//! ```yaml
//! version: 1
//! name: focused-review
//!
//! blocks:
//!   - id: worker            # IMMUTABLE identity. edges/gates reference THIS.
//!     name: Worker          # display only — renaming never breaks a reference
//!     kind: worker          # capability class (closed enum)
//!     cli: copilot
//!     model: auto
//!     profile: .github/agents/worker.md   # -> copilot --agent worker (NATIVE)
//!
//!   - id: rev-security
//!     name: Security review
//!     kind: reviewer
//!     cli: claude
//!     model: opus
//!     effort: xhigh        # OPTIONAL thinking level (#687); empty = the CLI's
//!     context: 1m          # own default. Both are closed enums, and both are
//!                          # a parse error on a CLI loomux can't set them on.
//!     prompt: |            # -> generated ~/.claude/agents/*.md + claude --agent
//!       Review ONLY for security defects: injection, authz, secrets.
//!
//!   - id: advisor            # role_hint: OPTIONAL (#250/#324/#891) — picks a
//!     kind: planner           # persona addendum/template/badge, plus a short
//!     role_hint: advisor      # enumerated list of MCP-tool exceptions; the
//!                             # CAPABILITY CLASS is `kind` alone, always.
//!                             # advisor requires kind: planner, process
//!                             # requires kind: worker, liaison requires
//!                             # kind: reviewer — anything else is a parse
//!                             # error.
//!
//!   - id: builder-remote     # remote: OPTIONAL (#1457) — an abstract LABEL
//!     kind: worker            # the OPERATOR binds to an SSH profile + a
//!     cli: claude             # remote clone path outside the repo. The repo
//!     remote: buildbox        # file SELECTS; it can never author a host, a
//!                             # port or an ssh option — those are unknown
//!                             # keys, and an unknown key fails the file.
//!                             # claude-only, never on an orchestrator or a
//!                             # manager block. Inert until the operator
//!                             # binds it (#1458) and the spawn path lands
//!                             # (#1459).
//!
//!   - id: manager           # OPTIONAL, at most one (#1161). The human's own
//!     kind: manager         # interface pane: it converses, grooms feature
//!     model: opus           # requests into briefs, and relays. cli:/model:/
//!                           # effort:/context:/name: only — prompt:, profile:
//!                           # and allow: are parse errors here, as on the
//!                           # orchestrator block.
//!
//! edges:                   # ADVISORY: the declared happy path. The
//!   - { from: worker, to: [rev-security] }   # orchestrator still schedules.
//!
//! gates:                   # DECLARED here; ENFORCED by the gh shim (sub-PR 3).
//!   merge:
//!     require: all-pass    # or: threshold: 2
//!     reviewers: [rev-security]
//!
//! merge_queue:             # OPT-IN, default off (#581). Absent block = the
//!   enabled: true          # feature is off and behavior is byte-for-byte
//!   max_batch: 3           # unchanged. See [`MergeQueuePolicy`].
//!   checks_timeout_minutes: 60
//!
//! board:                   # OPT-IN, default off (#1175). Absent block = no
//!   wip:                   # limits at all. See [`BoardPolicy`].
//!     in-progress: 4       # per-status caps on how much work may sit there
//!     review: 3
//!   enforce: false         # false (the default) = warn + notify; true =
//!                          # AGENT writes crossing a cap are refused
//!
//! triage:                  # OPT-IN, default off (#3304). Absent block = every
//!   enabled: true          # delivery reaches the orchestrator, as before.
//!   provider: none         # `none` is the ONLY accepted value in this build.
//!   kinds: []              # empty = every kind the rule table covers
//!   max_defer_minutes: 30  # the clock under a deferral. See [`TriagePolicy`].
//! ```
//!
//! `id` is immutable and human-meaningful and `name` is display-only on
//! purpose: n8n keys its graph by *display name*, so renaming a node silently
//! breaks every reference to it. Layout/coordinates live in a separate
//! `workflow.layout.json` beside it (the GUI pane's file, sub-PR 2) so a canvas
//! nudge never churns the semantic diff.

use crate::model::{cli_can_host, default_model, Role, SUPPORTED_CLIS};
use crate::pathseg::{PathSegment, SegmentError};
use crate::notify::{
    clamp_expires_minutes, NOTIFY_EXPIRES_DEFAULT_MIN, NOTIFY_EXPIRES_MAX, NOTIFY_EXPIRES_MIN,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

mod files;
mod gate;
mod parse;
mod roster;
mod routing;
mod sanitize;
mod schema;
mod verdict;
mod vocab;

pub use files::*;
pub use gate::*;
pub use parse::*;
pub use roster::*;
pub use routing::*;
pub use sanitize::*;
pub use schema::*;
pub use verdict::*;
pub use vocab::*;

/// A block's identity — immutable, human-meaningful, referenced by edges/gates.
pub type BlockId = String;
