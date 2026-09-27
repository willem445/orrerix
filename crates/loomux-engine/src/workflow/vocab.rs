//! The closed vocabularies a workflow file is checked against: kinds, role
//! hints, driver modes, and the per-CLI knob check.

use super::*;

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
