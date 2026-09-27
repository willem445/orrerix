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
mod parse;
mod roster;
mod sanitize;
mod schema;

pub use files::*;
pub use parse::*;
pub use roster::*;
pub use sanitize::*;
pub use schema::*;

/// A block's identity — immutable, human-meaningful, referenced by edges/gates.
pub type BlockId = String;

/// Map a `kind:` string onto a capability class. **`None` for anything
/// unrecognized** — the caller turns that into a hard error. Coercing an
/// unknown kind to `worker` (which is what loomux did before #222, in two
/// places) silently hands an unrecognized block a worktree and write access.
pub fn kind_from_str(s: &str) -> Option<Role> {
    match s.trim().to_ascii_lowercase().as_str() {
        "orchestrator" => Some(Role::Orchestrator),
        "worker" => Some(Role::Worker),
        "reviewer" => Some(Role::Reviewer),
        "planner" => Some(Role::Planner),
        // #1161. Declarable, but not spawnable and not part of any built-in
        // roster: `spawn_agent` refuses it exactly as it refuses
        // `orchestrator`, and `builtin_roster` never synthesizes one.
        "manager" => Some(Role::Manager),
        // NO `lead` ARM, AND THAT IS THE ENFORCEMENT (#2519), not an omission.
        // `Role::Lead` is minted by one path only — the launcher toggle the
        // human themselves flipped — and its absence from this vocabulary is
        // what makes three separate refusals structural rather than three
        // checks somebody remembered to write:
        //
        //  - a repo's `.orrerix/workflow.yml` cannot declare `kind: lead`, so a
        //    repo file can never hand a pane fleet control;
        //  - `spawn_agent` parses its `kind` argument through this very
        //    function, so no agent can spawn a lead — which is the NO-RECURSION
        //    rule ("a lead may never open another lead"), enforced by the
        //    vocabulary rather than by an arm in the spawn tool that a later
        //    edit could drop;
        //  - a `resume_session` carrying a recorded `role: "lead"` and no block
        //    id is refused rather than re-roled, by the same #544 "never guess a
        //    capability class" path every unrecognized role takes — the
        //    `owner_rec.block.trim().is_empty()` branch runs this function on
        //    the recorded role and errors on `None`.
        //
        //    **A recorded row that DOES carry a block id takes the other
        //    branch, and that branch is NOT a second structural refusal — do
        //    not read it as one.** `kind` is `None` by construction on a bare
        //    resume, so the lead caller's effective-class check reads the
        //    recorded BLOCK: `Some(Role::Lead)` for the lead's own block, which
        //    is refused as `resolves to kind "lead"`; `Some(Role::Worker)` for
        //    a worker block, which is PERMITTED; and `None` only when the
        //    recorded block id no longer resolves at all. What carries the
        //    property on that branch is THIS FUNCTION'S MISSING `lead` ARM — no
        //    block can have kind `Lead` while `kind_from_str` cannot name one,
        //    so no resume of any shape yields a lead pane. (The first version
        //    of this comment said "both routes refuse", which is false of the
        //    corrupt-data subcase and, worse, pointed a reader at the wrong
        //    invariant — rev-final N1 / rev-std round 2 on #2519.)
        //
        // Adding an arm here would silently undo all three at once. `Role::Solo`
        // is absent for the identical reason and is the precedent.
        _ => None,
    }
}

/// At most one `kind: manager` block per workflow (#1161).
///
/// Unlike reviewers — deliberately fanned out — two human interfaces is a
/// coherence bug rather than a configuration: the human would have two panes
/// each holding half a conversation, and everything downstream that says "the
/// manager" (the pane badge, the orchestrator's relay target, the mailbox in
/// M2) would have to pick one and silently ignore the other. Refused at parse,
/// where an author can still fix it.
pub const MANAGER_MAX: usize = 1;

/// The kinds a workflow file may name, for error messages.
///
/// Ordered built-ins-first: this string is also the source of the schema
/// manifest's `block.kind` values (`workflow_schema_field_facts`), which
/// `the_workflow_schema_manifest_matches_the_engines_values_defaults_and_bounds`
/// compares against `src/workflow-schema.json` as an ordered array — so the
/// order here is a contract with that file, not presentation.
pub fn kind_names() -> String {
    "orchestrator, worker, reviewer, planner, manager".to_string()
}

/// The capability class a `role_hint` REQUIRES — `None` for anything
/// unrecognized, the same "reject, never coerce" shape as [`kind_from_str`].
/// This function is the whole enforcement of the part that IS invariant: a
/// hint may only sit on an existing kind, so a workflow file can never spell a
/// fifth capability class. What a hint then MEANS is decided elsewhere, in
/// loomux's own code — see `docs/design/liaison.md` for the enumerated list of
/// MCP-tier exceptions, which today all narrow but are not guaranteed to.
pub fn role_hint_requires(hint: &str) -> Option<Role> {
    match hint.trim().to_ascii_lowercase().as_str() {
        "advisor" => Some(Role::Planner),
        "process" => Some(Role::Worker),
        // The human-facing liaison (#891): a pane the human converses with,
        // which reads the board and relays — so `reviewer`, the read-only
        // class that persists (a planner auto-closes on `report`, a worker
        // holds write authority the liaison is defined by NOT having).
        "liaison" => Some(Role::Reviewer),
        _ => None,
    }
}

/// Does this block actually REVIEW PRs? — reviewer-kind, minus the liaison
/// (#891).
///
/// `kind == Reviewer` answers "which capability class does it ride", which is
/// not the same question once a hint subtracts from its class: a liaison rides
/// the reviewer posture and reviews nothing, is denied `review_verdict`, and
/// cannot be named by a merge gate (`parse_workflow` refuses that outright).
/// Every place that means "the blocks a PR is fanned out to" — the
/// orchestrator's `{{REVIEWERS}}` list, a reviewer's "you are one of N" lane —
/// asks THIS, so both surfaces answer the same way; asking `kind` there sends a
/// PR to a pane that can neither record a verdict nor satisfy the gate it was
/// spawned for.
///
/// **`Guardrails::block_for` asks it too (#891 S4)**, for the neighbouring
/// question "which block does a bare `spawn_agent(kind: \"reviewer\")` open" —
/// a liaison declared first in roster order used to be that answer. Same
/// predicate deliberately: *which blocks review* must not have two answers.
/// The pane's own mirror is `isReviewingBlock` (`src/workflowtypes.ts`), which
/// keeps the workflow editor from offering a liaison as a gate reviewer.
///
/// Not used for the *capacity* advisories (`recommend_capacity`/`extra_tiers`),
/// which count live panes and are right to count a liaison as one — see
/// `docs/design/liaison.md`.
pub fn is_reviewing_block(b: &Block) -> bool {
    b.kind == Role::Reviewer && b.role_hint.as_deref() != Some("liaison")
}

/// **Which blocks the orchestrator may open with `spawn_agent(block: …)`** —
/// and therefore the only ones its kickoff roster and its instruction file may
/// list under "your delegates" (#1161 review B1).
///
/// The list those two surfaces render is not "every block in the file"; its own
/// sentence is *"pass `block: "<id>"` to spawn_agent to open one"*, so a block
/// `spawn_agent` refuses does not belong in it. Before this predicate the filter
/// was spelled inline as `kind != Orchestrator` at both call sites, which was
/// the same statement while there was exactly one unspawnable class — and the
/// moment a second arrived, the two surfaces went on advertising a route the
/// tool refuses. An orchestrator obeying its own instruction file would call it,
/// burn a turn on the refusal, and keep reading the same line on every turn
/// after that, re-grounding included.
///
/// So it is ONE predicate, named for the question, and the tool's refusals are
/// pinned against these surfaces in both directions by
/// `every_block_the_orchestrator_is_told_to_spawn_is_one_spawn_agent_accepts`.
/// Same discipline (and same reason) as [`is_reviewing_block`] next door: a
/// membership rule with two call sites must not be two rules.
///
/// - **Orchestrator** — a group has exactly one, opened at launch;
///   `spawn_agent` has refused `kind: "orchestrator"` since #222.
/// - **Manager** (#1161) — the human's own interface, declared in the workflow
///   file and opened for them; `spawn_agent` refuses it by `kind` and by
///   `block`. What the orchestrator is told about a declared manager instead is
///   M4's `{{MANAGER_NOTE}}`; until that lands it is told nothing, which is the
///   honest state — this slice ships no channel to it.
/// - **Lead** (#2519) — the human's own pane under the "orrerix subagents"
///   toggle. It reaches this predicate only through [`Role::is_fixture`]'s
///   shared answer and never through a real `Block`, because
///   [`kind_from_str`] has no `lead` arm, so no workflow file can declare one.
///   That is deliberate rather than an oversight: the class being unnameable
///   here is exactly what makes "a lead may not spawn a lead" structural.
pub fn is_spawnable_block(b: &Block) -> bool {
    !b.kind.is_fixture()
}

/// The role hints a workflow file may name, for error messages.
pub fn role_hint_names() -> String {
    "advisor, process, liaison".to_string()
}

/// The closed vocabulary of a block's `driver:` key (#2850) — HOW loomux
/// drives the block's agent, as opposed to `cli:`, which says which program
/// runs. `structured` means: over the CLI's structured-protocol surface (a
/// JSON stream or RPC channel, through the `harness` adapters), instead of a
/// scraped PTY.
///
/// This is loomux's vocabulary, not one CLI's — the same split `role_hint`
/// makes between the value set (closed, here) and who can carry it
/// (capability data, [`crate::model::CliCaps::structured_driver`]): a value
/// outside the set is a parse error whatever the CLI, and `structured` on a
/// CLI whose row carries no driver is a parse error naming both. A block
/// WITHOUT the key is a PTY pane — every pane this build spawns, until the
/// spawn-path wiring (#2850 S3b) starts reading it.
pub const DRIVER_MODES: &[&str] = &["structured"];

/// The mode names, for error messages and the schema manifest — one table,
/// so a second mode cannot appear in one and not the other.
pub fn driver_mode_names() -> String {
    DRIVER_MODES.join(", ")
}

/// Which structured harness this block's driver resolves to, for a KNOWN CLI —
/// or the refusal naming why it cannot.
///
/// **One spelling of the `driver:`/`CliCaps` rule, asked in two places.**
/// `parse_workflow` asks it of an explicitly spelled `cli:`, where a refusal is
/// a parse error the human sees. `spawn_agent_bound` asks it again of the
/// EFFECTIVE cli — the one `cli_of` resolved after inheritance — which is the
/// case the parse cannot reach at all: a block with no `cli:` of its own gets
/// `caps: None` there, so `driver: structured` under a workflow-level
/// `cli: claude` parses CLEAN today and would otherwise arrive at a spawn path
/// with no driver to run it on. That second ask is also the copy a hand-edited
/// `group.json` has to get past, the same reason `cli_can_host` is checked
/// twice.
///
/// A function rather than the check written out at both sites, because two
/// spellings of one rule is the divergence the shared-helper rule exists to
/// stop — and because only one of the two sites is reachable by the tests that
/// cover the other.
///
/// `Ok(None)` means this block asked for no structured driver, which is every
/// block written before the key existed and every PTY pane after it.
pub fn structured_harness_for(
    driver: Option<&str>,
    cli: &str,
) -> Result<Option<crate::harness::Harness>, StructuredDriverRefusal> {
    // Absent, or any mode that is not `structured`: no structured driver is
    // asked for. The vocabulary itself is checked where the value is admitted
    // (`parse_workflow`, and `read_blocks`' defence in depth), never here —
    // this function answers "which harness", not "is this spelled right".
    if driver != Some("structured") {
        return Ok(None);
    }
    match crate::model::cli_caps(cli).and_then(|c| c.structured_driver) {
        Some(h) => Ok(Some(h)),
        None => Err(StructuredDriverRefusal {
            cli: cli.to_string(),
            with_driver: clis_with_a_structured_driver(),
        }),
    }
}

/// Why a `driver: structured` block cannot run on the CLI it resolved to.
///
/// A struct rather than a formatted `String`, so each caller can frame it for
/// its own surface — `parse_workflow` prefixes the block index a human reads
/// in their workflow file, the spawn path prefixes `guardrail:` like every
/// other refusal on that path — while the REASON and the remedy stay one
/// sentence written once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredDriverRefusal {
    pub cli: String,
    pub with_driver: Vec<&'static str>,
}

impl std::fmt::Display for StructuredDriverRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cli {:?} has no structured driver — driver: structured needs a CLI \
             loomux drives over its structured protocol. Fix: drop the key (the \
             block runs as a PTY pane), or give this block a cli: that has one — {}",
            self.cli,
            self.with_driver.join(", ")
        )
    }
}

/// Every CLI whose `CliCaps` row carries a structured driver, in table order.
/// Derived rather than listed, so a row that gains one appears in the remedy
/// without anyone remembering to add it.
pub fn clis_with_a_structured_driver() -> Vec<&'static str> {
    crate::model::CLI_CAPS
        .iter()
        .filter(|c| c.structured_driver.is_some())
        .map(|c| c.cli)
        .collect()
}

/// Validate one block model knob — `effort:` or `context:` (#687).
///
/// Two checks, in this order, and both are **loud**: the value must be in
/// loomux's closed `vocabulary` (a typo is never coerced to a neighbouring
/// level), and — when the block names an explicit `cli:` — that CLI must be one
/// loomux can actually deliver the knob on, per its [`CliCaps`](crate::model::CliCaps)
/// row. A knob the CLI cannot honor is a parse error rather than a silent
/// no-op: the author asked for a thinking level and would otherwise never
/// learn they did not get one. `cli_supports` is `None` for a block that
/// inherits the group default CLI, which is not known at parse time (the same
/// deferral `cli_can_host` makes for the containment check) — `clamped()`
/// re-runs the CLI half at spawn, where the real CLI is in hand.
///
/// **The refusal also says what to do instead (#782).** An agent authoring a
/// `.loomux/workflow.yml` has no launcher to grey the knob out for it, so the
/// only rail it gets is this sentence — and "copilot cannot set effort" alone
/// leaves it guessing between deleting the key and changing the block's CLI.
/// The remedy is derived from [`CLI_CAPS`](crate::model::CLI_CAPS) by asking every
/// row loomux can actually spawn whether it carries THIS value, so a newly
/// wired seam (gemini's `thinkingConfig`, say) changes the message with no
/// edit here and no CLI named in this file — CLAUDE.md constraint 8.
///
/// Returns the normalized (trimmed, lowercased) value; empty for an absent key.
/// One function for both keys so the two can never drift on case handling or on
/// which check fires first.
fn validate_knob(
    field: &str,
    raw: &str,
    vocabulary: &[&str],
    cli: &str,
    cli_supports: Option<(&[&str], &str)>,
    knob_of: fn(&crate::model::CliCaps) -> &'static [&'static str],
) -> Result<String, String> {
    let want = raw.trim().to_ascii_lowercase();
    if want.is_empty() {
        return Ok(String::new());
    }
    if !vocabulary.contains(&want.as_str()) {
        return Err(format!(
            "unknown {field} {raw:?} — must be one of {}",
            vocabulary.join(", ")
        ));
    }
    if let Some((supported, note)) = cli_supports {
        if !supported.contains(&want.as_str()) {
            let alternatives: Vec<&str> = crate::model::CLI_CAPS
                .iter()
                .filter(|c| c.orchestration && knob_of(c).contains(&want.as_str()))
                .map(|c| c.cli)
                .collect();
            let remedy = if alternatives.is_empty() {
                format!(
                    "no cli loomux spawns can set {field} {want:?} today — drop the key and take \
                     the CLI's own default"
                )
            } else {
                format!(
                    "drop the key (the CLI's own default applies), or give this block a cli: that \
                     can set it — {}",
                    alternatives.join(", ")
                )
            };
            return Err(format!("cli {cli:?} cannot set {field} {want:?} — {note}. Fix: {remedy}"));
        }
    }
    Ok(want)
}

// ── verdicts: the state a gate reads (#222 / #197) ──────────────────────────
//
// Before this, a review outcome was a *notification*: `report("done", "approved
// — looks good")`, untyped text typed into the orchestrator's pane. That is
// exactly how PR #151 merged on the first "approve" that arrived while a second,
// dedicated review was still running — and that second review was the one that
// found a real release-gate bypass (#196). #197 asks for the outcome to be
// **state**: durable, attributed to the reviewer that recorded it, and readable
// by something that can refuse a merge.

/// A recorded review outcome. **Deliberately not a boolean.** Dify's Human Input
/// node and Windmill's `resume[...]` both give each decision its own outgoing
/// edge and keep the approver's typed input readable downstream; the investigation
/// (§2d) says to model ours the same way. So a reviewer can say "this needs a
/// human", which is neither an approval nor a defect report — and the gate can
/// treat it as the blocker it is instead of forcing it into a pass/fail bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Reviewed; no blocking findings. The only verdict that satisfies a gate.
    Pass,
    /// Reviewed; blocking findings. Refuses the merge.
    Fail,
    /// Not a defect call — the reviewer is handing the decision to a human
    /// (out of its depth, an ambiguous requirement, a risk it won't sign off on).
    /// Refuses the merge, exactly like `fail`: a gate must never be satisfiable
    /// by a reviewer that declined to decide.
    Escalate,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::Escalate => "escalate",
        }
    }

    /// Parse a verdict word. `None` for anything unrecognized — never coerced,
    /// and never defaulted to `pass`: a verdict loomux cannot read must not be
    /// able to open a gate.
    ///
    /// **Lowercase-strict, and that is a decision, not an oversight.** This is one
    /// half of a gate; the other half is the shim's `case "$v" in pass)`, which is
    /// a shell `case` and is case-sensitive. If this half lowercased, a
    /// hand-edited `PASS` in a verdict file would read as *satisfied* to the
    /// orchestrator (`list_verdicts`, `gate_status_line`) while the shim refused
    /// the merge — two halves of the same gate disagreeing about what a verdict
    /// *is*. One token definition, both sides, and the odd casing fails closed on
    /// both. Whitespace is trimmed because a trailing newline is file format, not
    /// content.
    pub fn parse(s: &str) -> Option<Verdict> {
        match s.trim() {
            "pass" => Some(Verdict::Pass),
            "fail" => Some(Verdict::Fail),
            "escalate" => Some(Verdict::Escalate),
            _ => None,
        }
    }

    /// Whether this verdict refuses a merge on its own. `fail` and `escalate`
    /// both do: **blockers beat approvals** (#197 Scope A.3) — with more than one
    /// reviewer, a disagreement resolves to "do not merge", and first-to-approve
    /// never wins.
    pub fn is_blocking(self) -> bool {
        !matches!(self, Verdict::Pass)
    }
}

/// The verdict words a reviewer may record, for error messages.
pub fn verdict_names() -> String {
    "pass, fail, escalate".to_string()
}

/// Longest verdict summary kept. The summary is durable state and is read back
/// into a gate refusal / the orchestrator's pane, not a transcript — a couple of
/// paragraphs is the useful range, and an unbounded one is a file-size footgun.
pub const MAX_SUMMARY_CHARS: usize = 4000;

/// A reviewer's summary is free prose that lands in a file loomux reads back and
/// re-renders. Drop control characters (they would ride into a terminal) but keep
/// newlines and tabs so the prose survives, and cap the length.
pub fn sanitize_summary(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .take(MAX_SUMMARY_CHARS)
        .collect()
}

/// One durable, **reviewer-attributed** verdict: which block recorded it, which
/// agent instance that was, **which revision it reviewed**, when, and why. The
/// attribution is the point — #197's second requirement is that "the specific
/// dispatched reviewer's recorded verdict is the gate, not the first approve that
/// arrives from any agent".
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewVerdict {
    pub pr: u64,
    /// The reviewer **block** id (`rev-security`) — the identity a gate names.
    pub block: BlockId,
    /// The agent instance that recorded it (`rev-4`). Two spawns of the same
    /// block are the same gate slot; this says which one actually spoke.
    pub agent_id: String,
    pub verdict: Verdict,
    /// **The PR head commit this verdict reviewed** (`headRefOid`), captured when
    /// it was recorded.
    ///
    /// A verdict binds to a *revision*, not to a PR number. Without this a `pass`
    /// survives a re-push: two reviewers approve #7, the worker pushes "fixed
    /// lint" and "one more edge case", and the gate still reads green over commits
    /// nobody reviewed — #197's failure class exactly, and the reason GitHub's own
    /// review model dismisses stale approvals on new commits. The gate compares
    /// this against the PR's current head and treats a mismatch as **outstanding**.
    ///
    /// Empty when loomux could not resolve the head at record time (no gh, no
    /// network, a repo gh can't see). That is *not* treated as "unbound, therefore
    /// fine" — an empty head can never equal a real one, so it reads as stale and
    /// the reviewer must re-record. Fail closed, like everything else here.
    pub head: String,
    /// **The PR body this verdict reviewed** (#565), as a sha256 of
    /// [`canonical_body`] — captured by the tool at record time, exactly like
    /// `head`, and never passed in by the reviewer.
    ///
    /// The head SHA pins the *code*. It does not pin the **PR body**, which on a
    /// squash-merging repo becomes the permanent commit message: reviewed content
    /// with the weight of a diff and none of a diff's version pinning. It moves in
    /// both directions — a reviewer passes a body and the author then edits it, so
    /// the merge carries text nobody reviewed; or a reviewer fails a body that has
    /// already been fixed, and the PR is blocked on a defect that no longer exists
    /// (the #525 incident that filed #565: review comment at 14:44:23Z, body edited
    /// at 14:47:49Z, and no mechanism could tell either agent).
    ///
    /// A digest rather than the body text: fixed size, and a mismatch is *exact*.
    /// Storing ~250 lines per verdict archives the artifact but still leaves a human
    /// to diff it by eye — which is the manual step that cost the round. A
    /// `updatedAt` timestamp was the other option and is worse than nothing: it
    /// moves for labels and assignees, so it cries wolf, and when it does fire it
    /// says *that* something changed, never *what*.
    ///
    /// Empty when loomux could not resolve the body at record time — read the same
    /// fail-closed way as an empty `head`: unknown, never "unbound, therefore fine".
    pub body_digest: String,
    /// **This review was a body-verification delta** (#2168 E2): the review
    /// driver briefed this lane because every required lane had already passed
    /// the code at this head and only the PR body had moved, so what it was
    /// asked for is the body as it stands rather than the diff.
    ///
    /// It is what lets [`crate::mergeq::recheck_gate`]'s `body-unchanged` clause
    /// accept the passes this one supersedes — see [`body_verified`] for the
    /// rule and the property it narrows the clause to.
    ///
    /// **Computed by the tool, never passed in, exactly like `body_digest`.**
    /// `review_verdict` sets it from the drive's own lane record — the brief it
    /// sent — so a reviewer cannot mark its own pass as a verification, and a
    /// verdict recorded outside a drive never carries it. That is what confines
    /// this clause's loosening to the one case the driver produces: an
    /// undriven repo, and a hand-recorded verdict, see the rule exactly as it
    /// was.
    ///
    /// It rides line 5 beside the digest rather than on a line of its own,
    /// because everything after line 5 is the summary and the summary is the
    /// one field a reviewer writes. A marker a reviewer could type would be a
    /// marker a reviewer could forge.
    pub verified_body: bool,
    /// **How many findings the reviewer left OPEN, as the reviewer declared it**
    /// (#3367 item 5) — `review_verdict`'s optional `open_findings` argument.
    ///
    /// The structured form of the count `reviewdrive::stated_findings` parses
    /// out of the summary: that parser stays, as the FALLBACK for a verdict that
    /// carries no declaration. `None` means the reviewer did not say, and it is
    /// never read as zero — the clean case (`reviewdrive::lanes_are_clean`)
    /// needs `Some(0)` on every lane, so a lane that omitted it is not clean.
    ///
    /// **Reviewer-declared, and that is not a hole in the gate.** Unlike
    /// `body_digest` and `verified_body`, which the tool computes because a
    /// reviewer could otherwise open `body-unchanged` for lanes that never read
    /// the body, this field decides nothing the gate reads: `evaluate_merge_gate`
    /// and `recheck_gate` never consult it. What it changes is whether the
    /// orchestrator is WOKEN to disposition findings, and a reviewer declaring
    /// `0` over a finding it would have written down has miscounted its own
    /// review — the same trust the gate already places in its `pass`.
    ///
    /// Serialized only when present, so a `list_verdicts` row for a verdict
    /// recorded without it is byte-for-byte what it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_findings: Option<u32>,
    pub summary: String,
    pub ts_ms: u64,
}

impl ReviewVerdict {
    /// Whether this verdict reviewed the PR's current head. A blocking verdict is
    /// *revision-independent* — a `fail` recorded against an older commit still
    /// refuses the merge until the reviewer re-records, because "this PR has a
    /// defect" does not stop being true when the author pushes more code.
    pub fn reviewed(&self, head: &str) -> bool {
        !self.head.is_empty() && self.head == head
    }

    /// Whether the PR body has changed since this verdict was recorded (#565).
    /// `None` when that cannot be *known* — either this verdict carries no digest
    /// (recorded by a build that predates #565, or with gh unable to read the body)
    /// or the current body could not be read now. Never `Some(false)` on a guess:
    /// "we could not check" and "it is unchanged" are different answers, and only
    /// one of them may quiet a warning.
    pub fn body_changed(&self, current_digest: Option<&str>) -> Option<bool> {
        let now = current_digest.filter(|d| !d.is_empty())?;
        (!self.body_digest.is_empty()).then(|| self.body_digest != now)
    }

    /// Whether this verdict is a **body-verification pass covering the body as
    /// it stands** (#2168 E2) — a `pass`, bound to this head, marked
    /// [`verified_body`](ReviewVerdict::verified_body) by the driver, and
    /// carrying the digest of the body now on the PR.
    ///
    /// All four, and each closes a different hole. `pass`: a `fail` that read
    /// the current body is the fix loop, not an approval of it. `reviewed`: a
    /// verification of a body sitting on a head nobody passed says nothing
    /// about what would merge. `verified_body`: an ordinary pass that happens
    /// to be the newest is not a review OF the delta, and accepting one would
    /// weaken the clause for every repo rather than for the driven case this
    /// is for. The digest equality: a verification of a body that has since
    /// moved again is spent.
    ///
    /// `None` for `now` — the body could not be read — is **not** a match, the
    /// same fail-closed direction [`body_changed`](ReviewVerdict::body_changed)
    /// takes: "we could not check" may never discharge a gate condition.
    pub fn verifies_body(&self, head: &str, now: Option<&str>) -> bool {
        let Some(now) = now.filter(|d| !d.is_empty()) else { return false };
        self.verdict == Verdict::Pass
            && self.verified_body
            && self.reviewed(head)
            && !self.body_digest.is_empty()
            && self.body_digest == now
    }

    /// Whether this **pass** still covers the body that would be committed —
    /// the question `body-unchanged` asks of one verdict, and the same question
    /// the review driver asks before it re-briefs a lane (#2168 E2).
    ///
    /// Directly, when its own digest is the current one. Or **by delegation**,
    /// when `verified` says some required reviewer recorded a body-verification
    /// pass at this head — see [`body_verified`]. A pass with **no** digest is
    /// covered by neither: unknown is never "unbound, therefore fine".
    ///
    /// **Both arms require the pass to be bound to the head that would merge**,
    /// and the delegation arm is the one where that is load-bearing: the
    /// verification lane read the BODY, and what makes standing in for this
    /// reviewer honest is that the CODE it approved has not moved since. Without
    /// it a `pass` from three commits ago would ride in on someone else's body
    /// review.
    ///
    /// It is asked here rather than left to the caller because the two callers
    /// ask it in different places and one of them did not (#2168 E2, first CI
    /// read). `mergeq::body_unchanged` filters `!reviewed(head)` before this
    /// line, so for the gate the clause is a no-op; `reviewdrive`'s
    /// `lane_pass_settles` had no such filter, and a `pass` bound to a head the
    /// worker had already fixed read as settling the revision in front of the
    /// drive — arc 8 skipping the very lane #1871 B1 exists to re-open. A
    /// predicate whose safety depends on where it is called is not a predicate.
    pub fn pass_covers_body(&self, head: &str, now: Option<&str>, verified: bool) -> bool {
        if self.verdict != Verdict::Pass || self.body_digest.is_empty() || !self.reviewed(head) {
            return false;
        }
        let Some(now) = now.filter(|d| !d.is_empty()) else { return false };
        self.body_digest == now || verified
    }
}

/// Whether any of `verdicts` is a body-verification pass at `(head, now)` —
/// [`ReviewVerdict::verifies_body`] asked of a whole reviewer set (#2168 E2).
///
/// **One definition, three consumers**, for §4's reason: the merge gate
/// ([`crate::mergeq::recheck_gate`]), the review driver's `review-wait`
/// (`reviewdrive::first_stale_lane`) and the `gh` shim's `body-unchanged` loop
/// all have to reach the same answer, or a drive reports `satisfied` on a merge
/// the shim then refuses. The shim is the one that cannot call this; it
/// reproduces it in POSIX shell, and
/// `the_shim_and_the_gate_agree_about_which_passes_a_verification_covers`
/// (`src-tauri/tests/orchestration/`) is what keeps the two honest: it walks
/// one set of verdict files past both halves and asserts they answer alike —
/// including the shapes two successive approximations of [`sanitize_digest`]
/// got wrong in OPPOSITE directions: prose that begins with a 64-hex word (the
/// shim accepted, Rust refuses) and an uppercase digest (the shim refused, Rust
/// accepts and lowercases). The shim now derives line 5 through one helper that
/// reproduces this function and [`parse_verdict_file`]'s split, rather than
/// testing the first whitespace field.
/// A glob was cited here before that test existed (#2308 review 4, R1), and a
/// citation to a name nothing answers to is worse than none — it reads as
/// coverage.
///
/// The caller passes the reviewers the **gate requires** — the routed list, not
/// every verdict on disk. A block the gate does not name has no standing to
/// discharge a condition of it.
pub fn body_verified<'a>(
    verdicts: impl IntoIterator<Item = &'a ReviewVerdict>,
    head: &str,
    now: Option<&str>,
) -> bool {
    verdicts.into_iter().any(|v| v.verifies_body(head, now))
}

/// The PR body reduced to the form both halves of the gate digest (#565).
///
/// Two normalizations, and **only** two, because the shim has to reproduce this
/// exactly in POSIX shell — a richer rule (per-line trailing whitespace, re-wrap
/// tolerance, markdown awareness) is one the two halves would eventually disagree
/// about, and a gate whose halves disagree is the failure mode this file keeps
/// coming back to:
///
/// 1. `\r` removed — a CRLF body and an LF body are the same commit message, and
///    which one `gh` hands back depends on the platform, not on the content.
///    Shell: `| tr -d '\r'`.
/// 2. Trailing newlines collapsed to exactly one. Shell: `$(…)` strips them all,
///    `printf '%s\n'` puts one back.
///
/// Everything else is content. In particular a re-wrapped paragraph **is** a
/// change: the body is about to become a permanent commit message, and the claim
/// this makes is "the bytes that will be recorded are the bytes that were
/// reviewed" — not "the meaning is close enough", which nothing could check.
pub fn canonical_body(body: &str) -> String {
    format!("{}\n", body.replace('\r', "").trim_end_matches('\n'))
}

/// sha256 of [`canonical_body`], lowercase hex. The one definition; the shim
/// pipes the same canonical bytes through `sha256sum`/`shasum`/`openssl`.
///
/// The two implementations are held together by an executed test, not by this
/// comment: case 1 of
/// `the_shim_refuses_a_merge_whose_body_moved_after_the_pass_when_the_repo_opts_in`
/// records a verdict through the real MCP tool and then merges through the real
/// shim over the SAME body — a body carrying CRLF, trailing blank lines, trailing
/// spaces, non-ASCII and `$`-bearing text — so the merge is allowed only if both
/// sides produced the same 64 characters. Disagreement surfaces as that case
/// failing with the gate's own refusal text.
pub fn body_digest(body: &str) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(canonical_body(body).as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// A stored body digest is compared inside a shell `case`, so keep it to what a
/// sha256 can actually be: **exactly** 64 hex characters. Anything else — a
/// truncated write, a hand edit, the first line of a summary in a verdict file
/// written before #565 — stores/reads as empty, i.e. *unknown*, which the gate
/// treats as it treats an unknown head: refuse, never wave through.
///
/// Deliberately stricter than [`sanitize_sha`], which accepts any hex run up to
/// 64: a 40-char head oid must not be readable as a body digest.
pub fn sanitize_digest(s: &str) -> String {
    let s = s.trim();
    if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        s.to_ascii_lowercase()
    } else {
        String::new()
    }
}

/// Group-dir subdirectory holding recorded verdicts, one file per reviewer block:
/// `verdicts/pr-<N>/<block-id>`.
///
/// **Why a file tree and not JSON:** the enforcement point is the `gh` PATH shim
/// — a POSIX shell script with no `jq` — and the existing gate state it reads
/// (`autonomous`, `auto_merge`, `merge_grants/pr-<N>`) is already exactly this:
/// small files whose presence and first line say everything. A verdict file's
/// first line is the verdict word, so the shim's read is `head -n1`. Keeping the
/// durable record and the enforcement input as *one* artifact means they cannot
/// drift.
pub const VERDICTS_DIR: &str = "verdicts";

/// A commit id is compared against gh's `headRefOid` inside a shell `case`, so
/// keep it to what a git object id can actually be. Anything else stores as empty,
/// which reads as **stale** — never as "unbound, therefore fine".
pub fn sanitize_sha(s: &str) -> String {
    let s = s.trim();
    if !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        s.to_ascii_lowercase()
    } else {
        String::new()
    }
}

/// The word line 5 carries **after** the digest when the verdict is a
/// body-verification pass (#2168 E2) — see [`ReviewVerdict::verified_body`].
///
/// **On line 5 rather than on a line of its own, and that placement is the
/// security property.** Line 6 onwards is the summary, which is the one field a
/// reviewer writes; a marker there would be a marker a reviewer could type. Line
/// 5 is written entirely by the tool whenever there is a digest at all, so a
/// reviewer cannot reach it.
pub const VERIFIED_BODY_MARK: &str = "verified-body";

/// The token line 4 carries **after** the agent id when the reviewer declared
/// [`ReviewVerdict::open_findings`] (#3367 item 5): `<agent-id> open-findings=<n>`.
///
/// **Line 4, and not line 5 or a line of its own.** Line 6 onward is the
/// summary, so a new line would shift it and misread every file written before;
/// line 5 is the one the `gh` shim reads (`loomux_verdict_line5`), and a second
/// token there would make the shim's digest split refuse the line. Line 4 is
/// read by nothing but [`parse_verdict_file`], and an agent id is a
/// `PathSegment` that can never contain a space, so the split is unambiguous.
/// An older build reading a newer file sees the token as part of the agent id,
/// which is display-only — it decides nothing.
pub const OPEN_FINDINGS_KEY: &str = "open-findings=";

/// Serialize a verdict record for `verdicts/pr-<N>/<block>`. Line-oriented, with
/// the verdict word FIRST (the shim reads it with `head -n1`), the reviewed head
/// SECOND and the reviewed body's digest FIFTH (`head -n5 | tail -n1`); the
/// summary runs to EOF, being the only field that may contain newlines — so every
/// fixed field has to sit above it.
///
/// Line 5 is the digest **alone** unless this verdict is a body verification, in
/// which case it is `<digest> verified-body` (#2168 E2). Both halves of the gate
/// therefore split a trailing [`VERIFIED_BODY_MARK`] off the line and run
/// [`sanitize_digest`] over **what remains**, which is what
/// [`parse_verdict_file`] does and what the shim's `loomux_verdict_line5`
/// reproduces. Reading the first whitespace FIELD instead is the approximation
/// this slice shipped twice and retracted twice: it accepts prose that begins
/// with a hex word, which is looser than the Rust half on the side that refuses
/// merges (#2308 rounds 4 and 5).
pub fn verdict_file_text(v: &ReviewVerdict) -> String {
    let digest = sanitize_digest(&v.body_digest);
    // The marker is meaningless without a digest to qualify it: what it says is
    // "the body THIS digest names was verified", and there is no such body when
    // the read failed. Dropped here rather than judged downstream, so the
    // parser's digest-first guard never has to decide about a bare marker.
    let mark = if v.verified_body && !digest.is_empty() {
        format!(" {VERIFIED_BODY_MARK}")
    } else {
        String::new()
    };
    let open = v.open_findings.map(|n| format!(" {OPEN_FINDINGS_KEY}{n}")).unwrap_or_default();
    format!(
        "{}\n{}\n{}\n{}{}\n{}{}\n{}\n",
        v.verdict.as_str(),
        sanitize_sha(&v.head),
        v.ts_ms,
        v.agent_id,
        open,
        digest,
        mark,
        sanitize_summary(&v.summary)
    )
}

/// Read a verdict file back. `None` for anything that isn't a verdict this build
/// understands — an unparseable file is *not* a pass (see [`Verdict::parse`]).
/// `pr`/`block` come from the path, which is loomux-generated.
///
/// Line 5 (the body digest, #565) is read **tolerantly**: a file written before
/// #565 has the first line of its summary there, and swallowing it would mangle
/// durable prose a human reads. So a line 5 that is not a valid digest is handed
/// back to the summary, and the digest reads empty — *unknown*. The shim
/// reproduces this function's own split and [`sanitize_digest`] rather than
/// approximating either (`loomux_verdict_line5`, #2308 round 5), so the two
/// agree on the only thing a gate decides: no readable digest means the body
/// cannot be shown unchanged, so `body-unchanged` refuses. The divergence that
/// remains is confined to which text is displayed as the summary — the shim has
/// no summary to display.
///
/// Line 5 may carry [`VERIFIED_BODY_MARK`] after the digest (#2168 E2), and the
/// split is **shape-checked, not whitespace-split**: the line is read as
/// `<digest> verified-body` only when the tail is exactly that word. A legacy
/// line 5 that happens to hold two words therefore parses exactly as it did
/// before — the whole line is offered to `sanitize_digest`, which refuses it,
/// and it stays in the summary. Splitting on the first space unconditionally
/// would have changed how a pre-#565 file reads, which is durable prose a human
/// wrote.
pub fn parse_verdict_file(pr: u64, block: &str, text: &str) -> Option<ReviewVerdict> {
    let mut lines = text.lines();
    let verdict = Verdict::parse(lines.next()?)?;
    let head = sanitize_sha(lines.next().unwrap_or(""));
    let ts_ms = lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
    // Line 4: the agent id, optionally followed by ` open-findings=<n>` (#3367
    // item 5). Shape-checked like line 5's mark: the tail is split off only when
    // it is exactly that key and a count that parses, so a legacy line 4 reads
    // exactly as it did — and a malformed count is NOT a declaration (never a
    // guessed zero), it stays on the id where a reader can see it.
    let line4 = lines.next().unwrap_or("").trim();
    let (agent_id, open_findings) = match line4.rsplit_once(' ') {
        Some((id, tail)) => match tail.strip_prefix(OPEN_FINDINGS_KEY).map(str::parse::<u32>) {
            Some(Ok(n)) => (id.trim(), Some(n)),
            _ => (line4, None),
        },
        None => (line4, None),
    };
    let agent_id = agent_id.to_string();
    let rest: Vec<&str> = lines.collect();
    let line5 = rest.first().copied().unwrap_or("");
    let (digest_field, marked) = match line5.trim_end().rsplit_once(' ') {
        Some((d, mark)) if mark == VERIFIED_BODY_MARK => (d, true),
        _ => (line5, false),
    };
    let body_digest = sanitize_digest(digest_field);
    // The marker only ever qualifies a digest this build could read. A line that
    // ends in the word but whose leading field is not a digest is not a
    // verification of anything — the same "unknown is never unbound, therefore
    // fine" the empty-digest case takes, one field over.
    let verified_body = marked && !body_digest.is_empty();
    let from = usize::from(!body_digest.is_empty());
    let summary = rest[from.min(rest.len())..].join("\n");
    Some(ReviewVerdict {
        pr,
        block: sanitize_id(block)?,
        agent_id,
        verdict,
        head,
        body_digest,
        verified_body,
        open_findings,
        summary: sanitize_summary(&summary),
        ts_ms,
    })
}

// ── the merge gate: the decision, and the spec file the shim reads ──────────

/// Gate conditions this build knows how to check (`gates.merge.also`).
///
/// The list is short on purpose, and the rule for everything *not* on it is the
/// important half: a condition loomux cannot check **refuses the merge** rather
/// than passing it. A gate is a safety claim; silently ignoring a clause of it
/// would turn a stricter-looking workflow file into a weaker one, which is the
/// worst failure mode a gate can have.
/// `body-unchanged` (#565) is **opt-in for a reason**: it only matters where the
/// PR body *becomes* the record — this repo squash-merges, so the body is the
/// permanent commit message. On a repo that merge-commits, the body is discussion,
/// and the check would be noise. Baking it in either way would be baking one
/// repo's merge habit into a generic tool (CLAUDE.md constraint 8), so it is a
/// clause a repo writes down.
/// `base-green` (#1174) is the stop-the-line clause: it refuses a merge while
/// the **base ref's HEAD** is red or its checks cannot be resolved, so a fleet
/// cannot pile work onto a branch that is already broken. Opt-in for the same
/// reason `body-unchanged` is: a repo with no CI would otherwise be refused
/// every merge forever by a clause it never asked for.
pub const KNOWN_CONDITIONS: [&str; 3] = ["ci-green", "body-unchanged", "base-green"];

/// Whether the shim can evaluate this `also:` condition. See [`KNOWN_CONDITIONS`].
pub fn condition_supported(c: &str) -> bool {
    KNOWN_CONDITIONS.contains(&c.trim())
}

/// The `base-green` reductions (#1174) — **one definition, two consumers**: the
/// `gh` shim interpolates these constants into its POSIX body, and
/// `mqdriver::base_check_runs_argv`/`base_status_argv` pass them to `gh --jq`.
///
/// They live here, with the rest of the gate contract, precisely because the
/// first cut had a *copy* in each place. The two were byte-identical, which
/// looked like the two-implementations-one-contract property holding — and it
/// was, but what the contract SAID was wrong in both, and nothing could have
/// told them apart from two copies that had drifted. A shared constant makes
/// "the shim and the queue ask GitHub the same question" a fact about the
/// program rather than a claim in a PR body.
///
/// Each reduces a JSON payload to ONE word from a closed vocabulary —
/// `red` | `truncated` | `pending` | `none` | `green` — because the shim has no
/// JSON parser and must decide a merge from a shell `case`.
///
/// **The clause order is the contract, and each step earns its place:**
///
/// 1. **`red` first, and only for COMPLETED runs.** A visible failure is the
///    most actionable answer, so it outranks everything below — but a run still
///    in progress carries `conclusion: null`, which the conclusion allow-list
///    would otherwise call red. Reporting "the base is RED" about a base that
///    is merely still building would be a false sentence in a refusal, so
///    `.status == "completed"` guards it.
/// 2. **`truncated` next — the #1181 review's blocking finding.**
///    `/commits/{ref}/check-runs` is **paginated**: `check_runs` is capped at
///    `per_page` while `total_count` counts them all. `any(.check_runs[]; …)`
///    therefore asks "is anything on THIS PAGE red", and before this clause a
///    base with more runs than one page — an ordinary OS x version matrix,
///    exactly the repo that adopts a stop-the-line gate — reported **green**
///    with its failures sitting on page 2. Reproduced against this repo's own
///    API: a commit with 3 `failure` runs answered `red` at full page size and
///    `green` at `?per_page=3`. A page that does not carry every run says
///    nothing about the runs it omits, so it is not an answer.
/// 3. `pending`, then `none`, then `green` — the residue, unchanged.
///
/// **The shape the payload is ASSUMED to have is checked, not assumed (#1181
/// rev-lead NB5).** `total_count` is documented as always present, and the
/// truncation clause above rests entirely on it — but jq sorts `null` below
/// every number, so an absent key makes `.total_count > (.check_runs|length)`
/// evaluate `null > N`, which is **false**, and the expression falls straight
/// through to `green`. That is round one's defect wearing a different hat: an
/// unstated assumption about the payload, failing open, in the one clause whose
/// whole premise is that unknown is never green. `has("total_count")` answers
/// `truncated` instead, so a payload that cannot support the question refuses
/// rather than passing.
///
/// **[`BASE_STATUS_JQ`] carries the same guard over ITS inputs**, which the
/// review did not ask for and this repo's own rule requires: a guard reads every
/// one of its inputs by one rule, and taking one signal from a checked shape and
/// the next from an unchecked one is a bypass exactly the width of that
/// asymmetry. `null | length` is `0` in jq rather than an error, so an absent
/// `statuses` would read as the *definite* claim "this commit has no legacy
/// statuses"; an absent `state` would fall to the `else` and report `red`,
/// which refuses but says something false about the base while doing it.
///
/// **Only the check-runs half needs the truncation clause**, and the asymmetry
/// is worth stating rather than leaving to be re-derived: the combined-status
/// endpoint carries a top-level `.state` that is GitHub's own rollup across
/// *all* statuses, so [`BASE_STATUS_JQ`] is pagination-proof by construction.
/// `check-runs` has no rollup field — only `total_count` — which is why one
/// half was safe and the other was not.
///
/// Green is an ALLOW-list of conclusions (`success`, `neutral`, `skipped`), so
/// a conclusion GitHub adds tomorrow reads as red rather than as green.
pub const BASE_CHECK_RUNS_JQ: &str = "if any(.check_runs[]; .status == \"completed\" and .conclusion != \"success\" and .conclusion != \"neutral\" and .conclusion != \"skipped\") then \"red\" elif (has(\"total_count\")|not) then \"truncated\" elif (.total_count > (.check_runs|length)) then \"truncated\" elif any(.check_runs[]; .status != \"completed\") then \"pending\" elif (.check_runs|length) == 0 then \"none\" else \"green\" end";

/// The combined-status reduction — see [`BASE_CHECK_RUNS_JQ`] for the shared
/// contract and for why this one needs no truncation clause.
///
/// `.state` is `pending` both when a context is pending and when there are no
/// statuses at all, so the count is read first and answers `none`.
pub const BASE_STATUS_JQ: &str = "if (has(\"statuses\")|not) or (has(\"state\")|not) then \"truncated\" elif (.statuses|length) == 0 then \"none\" elif .state == \"success\" then \"green\" elif .state == \"pending\" then \"pending\" else \"red\" end";

/// Why a merge gate is (not) satisfied — the pure spec the shim's shell mirrors,
/// and what the `review_verdict` tool reports back to the reviewer that just voted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateOutcome {
    /// Every requirement met: the merge may proceed to the *other* gates (the
    /// human grant / autonomous markers) — this one never opens a merge by itself.
    Satisfied,
    /// At least one named reviewer recorded `fail`/`escalate`. Blockers beat
    /// approvals: this refuses the merge whatever the others recorded, and
    /// whatever the threshold is.
    Blocked { blocking: Vec<BlockId> },
    /// Not enough live PASS verdicts yet.
    ///
    /// - `outstanding` — named reviewers with **no verdict recorded at all**. The
    ///   #151 case: a merge landing while a dispatched review is still running.
    /// - `stale` — named reviewers whose `pass` was recorded against an **earlier
    ///   revision** of the PR (or against none at all). The branch moved under
    ///   them; what they approved is not what would merge.
    Short { passes: u32, need: u32, outstanding: Vec<BlockId>, stale: Vec<BlockId> },
    /// loomux could not resolve the PR's current head, so it cannot tell whether
    /// any recorded verdict reviewed the code that would merge. Refuses — the same
    /// fail-safe the human gate takes on an undeterminable base.
    UnknownRevision,
}

impl GateOutcome {
    pub fn satisfied(&self) -> bool {
        matches!(self, GateOutcome::Satisfied)
    }
}

/// How many PASS verdicts this gate needs: every named reviewer (`all-pass`) or
/// `threshold: N`.
pub fn gate_need(gate: &Gate) -> u32 {
    match gate.require {
        GateRequire::AllPass => gate.reviewers.len() as u32,
        GateRequire::Threshold(n) => n,
    }
}

/// Reviewer ids a gate names that the given roster cannot actually spawn — either
/// no block carries that id, or it exists under a different capability class
/// (`kind` != reviewer). A gate's reviewers are validated against a workflow
/// file's OWN blocks at parse time ([`parse_workflow`]), but the roster a live
/// group spawns from can diverge from the file that armed its gate: a broken or
/// absent `.loomux/workflow.yml` on a fresh launch keeps the group's last-known
/// gate but resets `blocks` to [`default_roster`] (see `create_group`'s
/// `merge-gate-retained` branch, and the live incident behind #316 — a gate
/// naming `rev-orch`/`rev-ui`/`rev-tests` with the running registry offering only
/// the built-in four, so `spawn_agent(block: "rev-orch")` failed with "unknown
/// block" and the gate could never be satisfied from inside that session). Pure,
/// so both the arm-time refusal and a live status read share one rule.
/// **Routed reviewers count too** (#1176). A rule naming a block this roster
/// cannot spawn makes the gate unsatisfiable for every PR whose paths match it —
/// which is the same #316 failure the static list is checked for, arriving on a
/// subset of PRs instead of all of them. Reported here rather than left to be
/// discovered as "the merge gate stopped opening on frontend PRs only".
pub fn gate_missing_blocks(gate: &Gate, blocks: &[Block]) -> Vec<BlockId> {
    let mut out: Vec<BlockId> = Vec::new();
    let named = gate.reviewers.iter().chain(gate.routing.iter().flat_map(|r| r.reviewers.iter()));
    for id in named {
        if !blocks.iter().any(|b| &b.id == id && b.kind == Role::Reviewer) && !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

/// The agent-capacity a declared workflow structurally needs (#255) — derived
/// from its roster and its `merge` gate (if any), so the launcher can warn
/// before a `max_agents` cap starves the workflow it just loaded rather than
/// discovering it two hours in as an orchestrator that keeps killing live
/// agents to make room (the #255 incident: a 3-reviewer `all-pass` gate plus a
/// two-tier worker roster under a cap of 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapacityRecommendation {
    /// What **one review round costs without evicting anything already
    /// live**: [`reviewers_needed`](Self::reviewers_needed) plus one worker
    /// slot to have something to review. Below this the orchestrator cannot
    /// complete a single rework loop without killing a live agent to free a
    /// slot.
    pub minimum: u32,
    /// What running **every declared tier concurrently** costs: every
    /// distinct worker block, every distinct reviewer block, and one more if
    /// the workflow declares a planner block. The orchestrator itself is exempt
    /// from `max_agents` and is never counted here — and since #1161 M3 (D3)
    /// **neither is a declared manager**: `live_delegate_count` skips both
    /// classes, so counting either here would advise a human to raise a cap
    /// against a pane that never consumes one.
    ///
    /// A workflow with two planner blocks still adds only one slot here — a
    /// repo declares a *second* planner to give it an alternate persona (a
    /// different model, a narrower prompt), not to run two plan-first phases
    /// at once; the orchestrator only ever has one active planning phase, so
    /// unlike workers/reviewers (genuinely fanned out for parallel lanes) a
    /// planner count would overstate what concurrency the roster needs. This
    /// also matches #255's literal spec: "+1 if a planner block exists".
    pub recommended: u32,
    /// The gate's reviewer requirement folded into `minimum` — [`gate_need`],
    /// or every declared reviewer block when the workflow names no `merge`
    /// gate. Kept as its own field (rather than making a caller subtract the
    /// worker slot back out of `minimum`, or recount reviewer *blocks*) so
    /// anything describing *why* `minimum` is what it is reads this instead of
    /// re-deriving a gate-derived number from the block list — conflating the
    /// two was exactly the bug rev-1 of #255's review caught in `roster.ts`'s
    /// warning text.
    pub reviewers_needed: u32,
}

/// Derive a [`CapacityRecommendation`] from a workflow's blocks and its
/// `gates.merge` clause (`None` when the workflow declares none).
///
/// Gate-aware, per #255's requirement: a roster with 5 reviewer blocks but
/// `require: threshold: 2` has a different (lower) minimum than one requiring
/// `all-pass` over the same 5 — [`gate_need`] is exactly that distinction.
///
/// With no gate declared, nothing *enforces* every reviewer block being live
/// at once — but nothing else tells loomux which subset would be, either, so
/// `minimum` conservatively falls back to every reviewer block the workflow
/// names. That is deliberately the erring-flag-not-erring-silent side: this
/// feature exists because a starved roster surfaced as nothing more than "a
/// slow run" (#255's incident), so a gateless roster warning at a cap that
/// merely *might* be enough is the safer of the two wrong answers.
pub fn recommend_capacity(blocks: &[Block], gate: Option<&Gate>) -> CapacityRecommendation {
    let workers = blocks.iter().filter(|b| b.kind == Role::Worker).count() as u32;
    let reviewers = blocks.iter().filter(|b| b.kind == Role::Reviewer).count() as u32;
    let has_planner = blocks.iter().any(|b| b.kind == Role::Planner);
    // #1161 M3 (D3): a declared manager is NOT counted, and the absence is the
    // decision rather than an omission. M1 landed a `+1` here — correct while
    // `live_delegate_count` exempted only the orchestrator, since a preview
    // that under-advises is how #255 happens — and M3 inverted it in the same
    // commit that gave `live_delegate_count` its `Role::Manager` exemption. The
    // two move together by construction: `recommended` is "what the cap must be
    // for every declared tier to be live at once", and a class the cap does not
    // apply to is live at any cap. Counting it would tell a human to raise a
    // number that was never going to stop them talking to their manager.

    // #1176. A gate that routes by path needs, in the WORST case, its declared
    // list plus every lane any rule can add — a PR that touches all of them. The
    // worst case is the one a capacity floor has to be built on: under-advising
    // here is how #255 happens, an orchestrator discovering two hours in that it
    // must kill a live agent to complete one review round. Deduped against the
    // declared list, and against itself, so a lane two rules both name counts once.
    let reviewers_needed = gate.map_or(reviewers, |g| {
        let mut extra: Vec<&BlockId> = Vec::new();
        for id in g.routing.iter().flat_map(|r| r.reviewers.iter()) {
            if !g.reviewers.contains(id) && !extra.contains(&id) {
                extra.push(id);
            }
        }
        gate_need(g) + extra.len() as u32
    });
    let worker_slot = u32::from(workers > 0);
    CapacityRecommendation {
        // `minimum` is deliberately untouched: it is what ONE REVIEW ROUND
        // costs, and a review round does not involve the manager.
        minimum: reviewers_needed + worker_slot,
        recommended: workers + reviewers + u32::from(has_planner),
        reviewers_needed,
    }
}

/// Which declared tiers `recommended` adds beyond `minimum` — i.e. what a cap
/// sitting at-or-above `minimum` but below `recommended` can never keep live
/// alongside a review round (#255's soft-warning tier). Each entry is a short
/// noun phrase (`"the planner"`, `"1 more worker tier"`) meant to be joined
/// into a sentence, not a standalone description.
///
/// Takes the same `reviewers_needed` [`recommend_capacity`] computed, rather
/// than re-deriving it from `gate`, so this can never disagree with the
/// `minimum` it is describing the excess over.
pub fn extra_tiers(blocks: &[Block], reviewers_needed: u32) -> Vec<String> {
    let workers = blocks.iter().filter(|b| b.kind == Role::Worker).count() as u32;
    let reviewers = blocks.iter().filter(|b| b.kind == Role::Reviewer).count() as u32;
    let has_planner = blocks.iter().any(|b| b.kind == Role::Planner);

    let mut out = Vec::new();
    // `minimum` budgets exactly one worker slot regardless of how many worker
    // blocks are declared — every worker tier beyond the first is "extra".
    let extra_workers = workers.saturating_sub(1);
    if extra_workers > 0 {
        out.push(format!("{extra_workers} more worker tier{}", if extra_workers > 1 { "s" } else { "" }));
    }
    // `minimum` only budgets the gate's requirement — every reviewer block
    // beyond that (an all-pass gate naming a subset, or extra unnamed ones)
    // is "extra".
    let extra_reviewers = reviewers.saturating_sub(reviewers_needed);
    if extra_reviewers > 0 {
        out.push(format!("{extra_reviewers} more reviewer{}", if extra_reviewers > 1 { "s" } else { "" }));
    }
    if has_planner {
        out.push("the planner".to_string());
    }
    // #1161 M3: no manager row, deliberately. This list answers "what can a cap
    // between `minimum` and `recommended` never keep live alongside a review
    // round", and the answer for an exempt class is "nothing" — a manager is
    // live at every cap (D3). Naming it here would tell a human to raise a
    // number to protect a pane the number does not reach.
    out
}

/// English-join a short list of noun phrases: `"a"`, `"a and b"`, `"a, b, and
/// c"`. Used to turn [`extra_tiers`]'s list into one clause of a warning
/// sentence — pulled out so the audit note and the launcher's message build
/// the same phrase instead of each hand-rolling their own `.join(...)`.
pub fn join_with_and(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        _ => {
            let (last, rest) = parts.split_last().expect("non-empty, matched above");
            format!("{}, and {last}", rest.join(", "))
        }
    }
}

/// **The gate decision** (reviewer half; the `also:` conditions are checked in the
/// shim, which is the only place that can call `gh pr checks`). Pure, so the
/// semantics are pinned by fast tests and the shell mirror has something to agree
/// with. `head` is the PR's current head commit — `None` when loomux could not
/// resolve it.
///
/// Order matters, and it is the order #197 asks for:
///
/// 1. **A blocking verdict refuses the merge** — before any counting, and
///    regardless of which revision it was recorded against. One reviewer's `fail`
///    is not outvoted by two passes, and `threshold: 2` does not mean "two yeses
///    beat a no". (A `fail` against an older commit still stands: "this PR has a
///    defect" does not stop being true because the author pushed more code. The
///    reviewer clears it by re-reviewing and re-recording.)
/// 2. **A `pass` only counts for the revision it reviewed.** A pass recorded
///    against an earlier head is *stale*: the branch moved, and what that reviewer
///    approved is not what would merge. It counts as outstanding, not as a pass —
///    which is why GitHub's own review model dismisses stale approvals on new
///    commits, and it is the #197 failure class ("merging code no reviewer saw")
///    that a PR-keyed verdict would have left wide open.
/// 3. Then the live PASS count must reach [`gate_need`]. Under `all-pass` that
///    means every named reviewer has passed *this* revision — a reviewer that
///    hasn't recorded anything keeps the gate shut, which is precisely the bug that
///    produced #197.
///
/// `threshold: N` deliberately does *not* wait for the reviewers it doesn't need:
/// an author who writes `threshold: 2` over three reviewers has said, in the file,
/// that two passes are enough. They still cannot merge over a `fail` (rule 1), and
/// the passes still have to be for the code that would actually merge (rule 2).
/// `all-pass` — the default when `require:` is omitted — is the one that waits for
/// everybody.
pub fn evaluate_merge_gate(
    gate: &Gate,
    verdicts: &BTreeMap<BlockId, ReviewVerdict>,
    head: Option<&str>,
) -> GateOutcome {
    let mut blocking: Vec<BlockId> = Vec::new();
    let mut outstanding: Vec<BlockId> = Vec::new();
    let mut stale: Vec<BlockId> = Vec::new();
    let mut passes = 0u32;
    // No resolvable head → no way to know whether any pass reviewed the code that
    // would merge. Refuse, rather than fall back to "a pass is a pass" — that
    // fallback IS the bug this binding closes.
    let Some(head) = head else {
        return GateOutcome::UnknownRevision;
    };
    for r in &gate.reviewers {
        match verdicts.get(r) {
            Some(v) if v.verdict.is_blocking() => blocking.push(r.clone()),
            Some(v) if v.reviewed(head) => passes += 1,
            Some(_) => stale.push(r.clone()),
            None => outstanding.push(r.clone()),
        }
    }
    if !blocking.is_empty() {
        return GateOutcome::Blocked { blocking };
    }
    let need = gate_need(gate);
    if passes >= need {
        GateOutcome::Satisfied
    } else {
        GateOutcome::Short { passes, need, outstanding, stale }
    }
}

/// Group-dir file holding the declared merge gate, written from the repo's
/// `.loomux/workflow.yml` at group create/resume and read by the `gh` shim.
/// **Absent = no gate**, which is what makes a repo with no workflow file (or one
/// declaring no `gates.merge`) behave byte-for-byte as it did before #222.
pub const MERGE_GATE_FILE: &str = "merge_gate";

/// Serialize a gate for [`MERGE_GATE_FILE`].
///
/// Line-oriented `key value [value]`, because the reader is a POSIX `while read`
/// loop with no JSON parser — the same reason the verdicts are a file tree. Every
/// token written here is already sanitized: block ids through [`sanitize_id`] and
/// conditions through [`sanitize_condition`], both of which *reject* (never
/// rewrite) anything outside their alphabet at parse time. That is the contract
/// #225 established for exactly this consumer, and it is what lets the shim word-
/// split the line without quoting. Belt and braces anyway: a token that would not
/// survive its sanitizer is dropped here rather than written into a shell's
/// `for` loop.
///
/// **A token that fails its sanitizer poisons the file rather than vanishing from
/// it.** The first draft silently dropped such a token — which, if the parse
/// contract ever regressed, would have emitted a *weaker* gate than the repo
/// declared (a reviewer or a condition just disappears, and the gate goes green
/// one requirement short). Every other fork in this feature chooses fail-closed on
/// exactly that question; this one now does too. [`POISON_KEY`] is a line the shim
/// cannot parse, and an unparseable line refuses every merge until a human looks.
pub fn gate_file_text(gate: &Gate) -> String {
    let mut out = String::from(
        // The source file is named GENERICALLY (#1153 phase 4): a repo may
        // declare its workflow at `.orrerix/workflow.yml` or the legacy
        // `.loomux/workflow.yml`, this function has no repo to resolve which,
        // and a header naming the wrong one would send a human editing a file
        // that isn't there. The brand word in the first phrase IS protocol
        // text, and flipped with #1153 phase 3. No parser reads it — the shim
        // skips `#` lines — but an agent opening the file does.
        "# orrerix merge gate — generated from this repo's workflow file (#222). Do not edit.\n",
    );
    match gate.require {
        GateRequire::AllPass => out.push_str("require all-pass\n"),
        GateRequire::Threshold(n) => out.push_str(&format!("require threshold {n}\n")),
    }
    for r in &gate.reviewers {
        match sanitize_id(r) {
            Some(clean) if clean == *r => out.push_str(&format!("reviewer {r}\n")),
            _ => out.push_str(&format!("{POISON_KEY} unusable-reviewer-id\n")),
        }
    }
    for c in &gate.also {
        match sanitize_condition(c) {
            Some(clean) if clean == *c => out.push_str(&format!("also {c}\n")),
            _ => out.push_str(&format!("{POISON_KEY} unusable-condition\n")),
        }
    }
    // #1174. A `0` here would be a clause that gates nothing, and `parse_workflow`
    // has already refused it — so if one ever reaches this far the file is
    // poisoned rather than written with a limit the shim would ignore.
    match gate.max_diff_lines {
        None => {}
        Some(0) => out.push_str(&format!("{POISON_KEY} unusable-max-diff-lines\n")),
        Some(n) => out.push_str(&format!("{MAX_DIFF_LINES_KEY} {n}\n")),
    }
    // #1176's routing rules, one line per (rule, glob) and one per (rule,
    // reviewer), each carrying the rule's 1-based index.
    //
    // **Why not one line per rule.** The reader is a POSIX `while read -r k v w`
    // loop with no arrays: a rule packed onto one line would have to be re-split
    // inside the shell, and every spelling of that (an `IFS` swap, a `set --`)
    // either clobbers the shim's own positional parameters or introduces a
    // second delimiter for a glob alphabet to have to avoid. Three fixed fields
    // fit the loop that is already there, and the index is what stitches the
    // halves back together — see [`parse_gate_file`], which refuses any file
    // where they do not stitch.
    if !gate.routing.is_empty() {
        if matches!(gate.require, GateRequire::Threshold(_)) {
            // `parse_workflow` refuses this pair outright, so reaching here means
            // the parse contract has regressed. Poison rather than write a file
            // whose two halves would be read as a LAXER gate than either says.
            out.push_str(&format!("{POISON_KEY} routing-with-threshold\n"));
        }
        if gate.routing.len() > ROUTING_RULES_MAX {
            out.push_str(&format!("{POISON_KEY} too-many-routing-rules\n"));
        }
    }
    for (i, rule) in gate.routing.iter().enumerate() {
        let idx = i + 1;
        // A rule missing either half is unsatisfiable-or-vacuous, and both are
        // refused at parse. Poisoned here for the same reason the tokens below
        // are: the file must never be a weaker gate than the workflow declared.
        if rule.paths.is_empty() || rule.reviewers.is_empty() {
            out.push_str(&format!("{POISON_KEY} incomplete-routing-rule\n"));
        }
        // The per-rule path cap, poisoned for the same reason the rule cap above
        // is (#1176 rev-972 N1): `parse_gate_file` refuses a file past it, so
        // writing one would emit a file loomux itself calls malformed.
        if rule.paths.len() > ROUTING_PATHS_MAX {
            out.push_str(&format!("{POISON_KEY} too-many-routing-paths\n"));
        }
        for p in &rule.paths {
            match sanitize_glob(p) {
                Some(clean) if clean == *p => {
                    out.push_str(&format!("{ROUTE_PATH_KEY} {idx} {p}\n"))
                }
                _ => out.push_str(&format!("{POISON_KEY} unusable-routing-glob\n")),
            }
        }
        for r in &rule.reviewers {
            match sanitize_id(r) {
                Some(clean) if clean == *r => {
                    out.push_str(&format!("{ROUTE_REVIEWER_KEY} {idx} {r}\n"))
                }
                _ => out.push_str(&format!("{POISON_KEY} unusable-routing-reviewer\n")),
            }
        }
    }
    out
}

/// The [`MERGE_GATE_FILE`] key carrying one routing rule's path glob (#1176):
/// `route-path <1-based rule index> <glob>`. Hyphenated to match the file's own
/// spelling convention (`all-pass`, `max-diff-lines`), which is not the YAML
/// key's.
pub const ROUTE_PATH_KEY: &str = "route-path";

/// The [`MERGE_GATE_FILE`] key carrying one routing rule's required reviewer:
/// `route-reviewer <1-based rule index> <block id>`. See [`ROUTE_PATH_KEY`].
pub const ROUTE_REVIEWER_KEY: &str = "route-reviewer";

/// The [`MERGE_GATE_FILE`] key carrying [`Gate::max_diff_lines`]. Hyphenated to
/// match `all-pass`/`ci-green` — the file's own spelling convention, which is
/// not the YAML key's (`max_diff_lines`), because the two have different
/// readers and the shim's is a `case` over word-split tokens.
pub const MAX_DIFF_LINES_KEY: &str = "max-diff-lines";

/// The key [`gate_file_text`] writes when a token cannot be represented safely.
/// Nothing parses it — by design: the shim refuses any gate-file line whose key it
/// does not recognize, so an unrepresentable gate refuses merges instead of
/// silently becoming a laxer one. Unreachable while the parse contract holds
/// (`parse_workflow` rejects such tokens outright); this is what happens if it
/// ever stops holding.
pub const POISON_KEY: &str = "unrepresentable";

/// Read [`MERGE_GATE_FILE`] back into a [`Gate`] — the inverse of
/// [`gate_file_text`], used by the registry to report gate status to the agent
/// that just recorded a verdict (the shim does its own read, in shell).
///
/// `None` means **this file is not a usable gate**, which the callers must report
/// as "malformed — every merge refused" rather than as "no gate": the file is on
/// disk, the shim will read it, and the shim refuses on exactly the things that
/// return `None` here. Those are a file with no reviewers (nobody could ever
/// satisfy it) and any line whose key loomux does not recognize — a poison line
/// ([`POISON_KEY`]), a truncation, a hand edit. The two halves agree, and both fail
/// closed.
pub fn parse_gate_file(text: &str) -> Option<Gate> {
    let mut require = GateRequire::AllPass;
    let mut reviewers: Vec<BlockId> = Vec::new();
    let mut also: Vec<String> = Vec::new();
    let mut max_diff_lines: Option<u32> = None;
    // #1176. Halves of a routing rule arrive on separate lines and are stitched
    // back together by index after the loop; `BTreeMap` so the stitch walks them
    // in rule order rather than file order.
    let mut rule_paths: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    let mut rule_reviewers: BTreeMap<u32, Vec<BlockId>> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut f = line.split_whitespace();
        match (f.next(), f.next(), f.next()) {
            // A threshold that doesn't parse (or is 0) leaves `require` at
            // `all-pass` — the STRICTER of the two. A malformed gate line must
            // never be the reason a merge gets easier.
            (Some("require"), Some("threshold"), Some(n)) => {
                if let Some(n) = n.parse().ok().filter(|n| *n > 0) {
                    require = GateRequire::Threshold(n);
                }
            }
            (Some("require"), Some("all-pass"), _) => require = GateRequire::AllPass,
            (Some("reviewer"), Some(id), _) => match sanitize_id(id) {
                Some(id) => reviewers.push(id),
                None => return None,
            },
            (Some("also"), Some(c), _) => match sanitize_condition(c) {
                Some(c) => also.push(c),
                None => return None,
            },
            // #1174. Unlike `require threshold`, an unusable number here has no
            // stricter fallback to land on — "no limit" is the LAXER reading, so
            // the whole file is unusable instead, which the callers report as
            // "malformed — every merge refused". Same direction as the
            // `reviewer`/`also` arms above.
            (Some(MAX_DIFF_LINES_KEY), Some(n), _) => {
                match n.parse::<u32>().ok().filter(|n| *n > 0) {
                    Some(n) => max_diff_lines = Some(n),
                    None => return None,
                }
            }
            // #1176. Same direction as every arm above: a routing line loomux
            // cannot read makes the whole file unusable, because the thing it
            // would have added is a REQUIRED reviewer. A dropped one is a merge
            // that skipped a lane, which is precisely the laxening this reader
            // refuses to perform.
            // **Rejected, never rewritten** — and the comparison is what makes
            // that true. `sanitize_glob`/`sanitize_id` FILTER: `src/[ab]` comes
            // back as `src/ab`, which is not a refusal, it is a DIFFERENT RULE
            // silently substituted for the one the file carries. `gate_file_text`
            // poisons rather than writes such a token, so anything reaching here
            // is a hand edit or a corruption — exactly the case this reader must
            // refuse rather than quietly reinterpret.
            //
            // A **fourth token** is refused for the same reason: neither
            // alphabet contains whitespace, so a line that has any has already
            // been truncated by the word split — `route-path 1 src/a b` reads as
            // the narrower glob `src/a`, which is clean and wrong. Exactly three
            // fields, or the file is not a gate.
            (Some(ROUTE_PATH_KEY), Some(i), Some(g)) => {
                let idx = routing_index(i)?;
                let clean = sanitize_glob(g)?;
                if clean != g || f.next().is_some() {
                    return None;
                }
                rule_paths.entry(idx).or_default().push(clean);
            }
            (Some(ROUTE_REVIEWER_KEY), Some(i), Some(r)) => {
                let idx = routing_index(i)?;
                let clean = sanitize_id(r)?;
                if clean != r || f.next().is_some() {
                    return None;
                }
                rule_reviewers.entry(idx).or_default().push(clean);
            }
            // Anything else — a poison line, a truncated key, a hand edit — makes
            // the whole file unusable. Skipping it would drop a requirement.
            _ => return None,
        }
    }
    // Stitch the two halves. The indices must be exactly 1..=N with BOTH halves
    // present for every one of them: a gap, a duplicate index that lost its
    // partner, or a `route-path` with no `route-reviewer` is a file that cannot
    // be read as the gate someone declared, and an unreadable gate refuses.
    let n = rule_paths.len().max(rule_reviewers.len());
    if n > ROUTING_RULES_MAX {
        return None;
    }
    let mut routing: Vec<RoutingRule> = Vec::new();
    for idx in 1..=n as u32 {
        let paths = rule_paths.remove(&idx)?;
        let reviewers = rule_reviewers.remove(&idx)?;
        if paths.is_empty() || reviewers.is_empty() || paths.len() > ROUTING_PATHS_MAX {
            return None;
        }
        routing.push(RoutingRule { paths, reviewers });
    }
    // Anything left over means the indices were not contiguous — an index past
    // `n`, which nothing above could have consumed.
    if !rule_paths.is_empty() || !rule_reviewers.is_empty() {
        return None;
    }
    // The pair `parse_workflow` refuses (see there for why). Refused here too,
    // rather than trusted to be impossible: this reader's whole job is to be the
    // half that does not assume the other half held.
    if !routing.is_empty() && matches!(require, GateRequire::Threshold(_)) {
        return None;
    }
    (!reviewers.is_empty()).then_some(Gate { require, reviewers, also, max_diff_lines, routing })
}

/// A `route-path`/`route-reviewer` line's rule index — 1-based, so `0` is not an
/// index and is refused with everything else that is not a plain positive number.
fn routing_index(s: &str) -> Option<u32> {
    s.parse::<u32>().ok().filter(|n| *n > 0)
}

/// What the small-batch clause (#1174) says about one PR — the pure decision
/// the shim's shell mirrors and the merge queue re-runs, so there is exactly
/// one definition of "too big" in this codebase and two readers of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSizeVerdict {
    /// No `max_diff_lines` declared, or the PR is within it.
    Ok,
    /// The PR's changed-line count exceeds the declared limit.
    TooLarge { lines: u64, limit: u32 },
    /// The limit is declared and the PR's size could not be read at all.
    /// **Refuses** — the same posture `ci-green` takes on unreadable checks and
    /// the queue takes on an unverifiable base: unknown is never "fine".
    Unknown { limit: u32 },
}

impl DiffSizeVerdict {
    pub fn ok(&self) -> bool {
        matches!(self, DiffSizeVerdict::Ok)
    }
}

/// Apply [`Gate::max_diff_lines`] to a PR whose changed-line count is `lines`
/// (additions + deletions), or `None` when that could not be resolved.
///
/// A gate that declares no limit answers [`DiffSizeVerdict::Ok`] **without
/// looking at `lines`** — the absent-config no-op that keeps every repo which
/// never declared the key on exactly the path it was on before #1174.
pub fn check_diff_size(gate: &Gate, lines: Option<u64>) -> DiffSizeVerdict {
    let Some(limit) = gate.max_diff_lines else {
        return DiffSizeVerdict::Ok;
    };
    match lines {
        None => DiffSizeVerdict::Unknown { limit },
        Some(lines) if lines > u64::from(limit) => DiffSizeVerdict::TooLarge { lines, limit },
        Some(_) => DiffSizeVerdict::Ok,
    }
}

// ── path-based reviewer routing (#1176) ─────────────────────────────────────

/// One routing rule that MATCHED — the "which rules fired and why" half of the
/// gate's own report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FiredRule {
    /// The rule's 1-based position in `gates.merge.routing`, so a refusal can
    /// name the line the author has to look at.
    pub index: u32,
    /// The rule's **declared** globs — all of them, not the one that happened to
    /// match. Deliberate: the shim streams the changed-file list and tests the
    /// globs per file, this walks the globs per rule, and "the first glob that
    /// matched" is therefore a different string on the two sides whenever a rule
    /// has more than one. The rule's own text is the same on both, and it is
    /// also the thing the author needs to see.
    pub paths: Vec<String>,
    /// The reviewers this rule requires.
    pub reviewers: Vec<BlockId>,
}

/// The required-reviewer set for one PR, once routing has been applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutingDecision {
    /// [`Gate::reviewers`] ∪ every fired rule's reviewers, in that order:
    /// the static list first, then the rules in declaration order, each new id
    /// appended once. The `gh` shim appends in exactly this order too, so the
    /// two produce the same list and not merely the same set.
    pub required: Vec<BlockId>,
    /// Which rules fired. Empty when the gate declares no routing, or when it
    /// declares routing and nothing matched.
    pub fired: Vec<FiredRule>,
}

impl RoutingDecision {
    /// `base` with its reviewer list replaced by [`required`](Self::required)
    /// and its routing spent — **the effective gate**, which is what everything
    /// downstream evaluates.
    ///
    /// Routing resolves to a reviewer list and then gets out of the way, so
    /// [`evaluate_merge_gate`], [`gate_need`] and the `body-unchanged` loop stay
    /// exactly one implementation each. A second gate decision that knew about
    /// routing would be a third implementation of the gate, which this codebase
    /// treats as a defect rather than an optimization.
    pub fn gate(&self, base: &Gate) -> Gate {
        Gate {
            require: base.require,
            reviewers: self.required.clone(),
            also: base.also.clone(),
            max_diff_lines: base.max_diff_lines,
            routing: Vec::new(),
        }
    }
}

/// Apply [`Gate::routing`] to one PR's changed files — **the pure decision the
/// `gh` shim's shell mirrors and the merge queue re-runs.**
///
/// `changed` is the PR's repo-relative changed paths, or `None` when that list
/// could not be resolved *or could not be shown to be complete* (see
/// [`ROUTING_FILES_JQ`]).
///
/// - A gate that declares **no routing** answers without looking at `changed` at
///   all — the absent-config no-op that keeps every repo which never wrote the
///   key on exactly the path it was on before #1176, and the same shape
///   [`check_diff_size`] takes.
/// - A gate that **does** declare routing and cannot see the file list answers
///   `None`: **refuse.** Unknown is never safe here in a particularly sharp way
///   — the unknown thing is *which reviewers are required*, so guessing "none of
///   the rules fired" is guessing in favour of merging.
pub fn route_reviewers(gate: &Gate, changed: Option<&[String]>) -> Option<RoutingDecision> {
    if gate.routing.is_empty() {
        return Some(RoutingDecision { required: gate.reviewers.clone(), fired: Vec::new() });
    }
    let changed = changed?;
    let mut required = gate.reviewers.clone();
    let mut fired: Vec<FiredRule> = Vec::new();
    for (i, rule) in gate.routing.iter().enumerate() {
        if !rule.paths.iter().any(|g| changed.iter().any(|f| glob_match(g, f))) {
            continue;
        }
        for r in &rule.reviewers {
            if !required.contains(r) {
                required.push(r.clone());
            }
        }
        fired.push(FiredRule {
            index: i as u32 + 1,
            paths: rule.paths.clone(),
            reviewers: rule.reviewers.clone(),
        });
    }
    Some(RoutingDecision { required, fired })
}

/// The first line [`ROUTING_FILES_JQ`] emits when — and only when — it can
/// account for every changed file on the PR.
pub const ROUTED_FILES_OK: &str = "ok";

/// The prefix on each path line [`ROUTING_FILES_JQ`] emits.
///
/// A prefix rather than a bare path so the status word and the data live in
/// different shapes: without it, a repo containing a file literally named `ok`
/// would emit a line indistinguishable from the header.
pub const ROUTED_FILES_PREFIX: &str = "p ";

/// Reduce `gh pr view --json files,changedFiles` to the changed-path list
/// routing needs — **one definition, two consumers**, the same arrangement
/// [`BASE_CHECK_RUNS_JQ`] established: the `gh` shim interpolates this constant
/// into its POSIX body and `mqdriver::pr_files_argv` passes it to `gh --jq`, so
/// the shim and the merge queue cannot ask GitHub different questions.
///
/// Output is a line-oriented protocol read by [`parse_routed_files`] in Rust and
/// by a `while read` loop in shell: the word [`ROUTED_FILES_OK`], then one
/// `p <path>` line per changed file. **Anything else is a refusal** — there is
/// no word for "some of the files", because a partial list is not an answer to
/// "did this PR touch `src/**`".
///
/// **The truncation clause is the whole reason this is a reduction and not a
/// plain `.files[].path`.** `gh pr view --json files` fetches ONE page: the
/// GraphQL `files` connection is capped at 100 while `changedFiles` counts them
/// all. Verified live against this repo — PR #1181 answered
/// `{changed: 32, listed: 32}` and PR #1018 answered `{changed: 135, listed:
/// 100}` — so on any PR past a hundred files the list silently omits the tail.
/// For a *size* gate that omission would be visible; for routing it fails
/// **open** and invisibly: the one file that would have matched a rule sits on
/// page two, no rule fires, and the lane the repo asked for is quietly not
/// required. That is #1181's own pagination finding wearing routing's hat, so it
/// gets the same answer — a page that cannot account for every file is not an
/// answer, and `!=` (not `<`) is the comparison, because a count that disagrees
/// in *either* direction means the payload is not the shape this question rests
/// on.
///
/// The shape it rests on is **checked, not assumed**, for the same reason
/// `BASE_CHECK_RUNS_JQ` checks its own: `null` sorts below every number in jq,
/// so an absent `changedFiles` would make a comparison read false and fall
/// through to the answer that merges. `has(...)` answers "unaccountable"
/// instead.
///
/// A path carrying a **newline or carriage return** is refused too. It cannot be
/// expressed in a one-path-per-line protocol at all, so a reader would silently
/// see two files where the repo has one — and the shim's reader is a merge gate
/// being fed a path that a fork PR's author chose.
pub const ROUTING_FILES_JQ: &str = "if (has(\"files\")|not) or (has(\"changedFiles\")|not) then \"unaccountable\" elif (.files|length) != .changedFiles then \"unaccountable\" elif any(.files[]; (.path|type) != \"string\" or (.path|length) == 0 or (.path|test(\"[\\n\\r]\"))) then \"unaccountable\" else ([\"ok\"] + (.files|map(\"p \" + .path)))[] end";

/// Read [`ROUTING_FILES_JQ`]'s output back into a changed-path list — the Rust
/// half of that protocol, mirrored in shell by the `gh` shim.
///
/// `None` for **anything** that is not a complete, well-formed answer: the
/// `unaccountable` word, an empty capture (gh failed, jq errored, the PR is
/// gone), a line without the [`ROUTED_FILES_PREFIX`], an empty path. Callers
/// hand that straight to [`route_reviewers`] as `None`, which refuses.
///
/// A PR with genuinely zero changed files is `Some(vec![])`, not `None`: that is
/// a complete answer that happens to be empty, and the distinction is the whole
/// difference between "no rule matched" and "loomux cannot say".
pub fn parse_routed_files(out: &str) -> Option<Vec<String>> {
    let mut lines = out.lines();
    if lines.next().map(str::trim) != Some(ROUTED_FILES_OK) {
        return None;
    }
    let mut files: Vec<String> = Vec::new();
    for line in lines {
        // A trailing blank line is how a capture ends, not a file.
        if line.trim().is_empty() {
            continue;
        }
        let path = line.strip_prefix(ROUTED_FILES_PREFIX)?;
        if path.is_empty() {
            return None;
        }
        files.push(path.to_string());
    }
    Some(files)
}
