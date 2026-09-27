//! The session's active workflow file: where it lives and how it loads, the
//! workflow-mode and switch notices, the merge-gate refusal exits, the switch
//! plan (`SwitchPlan`) and roster drift, and the `blocks`/`intake` JSON views.
//! Design note: `docs/design/workflows.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. Sibling files it
//! calls: `agentmodel.rs`, `guardrails.rs`.

use super::*;

/// The three ways forward when a workflow merge gate refuses a merge (#316,
/// scope point 3 — a human "Approve" grant never opens THIS gate, #197/#222, so
/// a refusal must never leave the exit unnamed). Shared, word-for-word, between
/// the shim's own refusal (`gh_shim_sh`'s `loomux_block_wf` — duplicated there
/// since a shell template can't call into this constant, but kept in sync with
/// it) and the DEFAULT form of the Rust-side gate status (`gate_status_line`).
/// The gate status has one deliberate exception: while a merge-time condition
/// is failing, it speaks [`GATE_REFUSAL_EXITS_WITHOUT_UI_BYPASS`] instead, so
/// the exit that goes around the failing condition is not offered by the line
/// that just described it failing. The shim keeps all three — it fires on the
/// human's own `gh pr merge`, where the human is already acting directly.
pub(in crate::orchestration) const GATE_REFUSAL_EXITS: &str = "Three ways forward: (1) get the named reviewer(s) to run and \
     record a verdict, (2) have the human turn workflow mode off for this session (clears the \
     gate), or (3) merge this PR from the GitHub UI, which is not gated.";

/// [`GATE_REFUSAL_EXITS`] with exit (3) withheld (#1889): the GitHub-UI merge is
/// the one suggestion that turns a failing merge-time condition into an action
/// AROUND it — a reader who takes it merges past the very condition the caveat
/// just warned about — so while one is failing the gate status line offers only
/// the two ways that go through the gate. The withheld exit is deliberately NOT
/// named here: advertising the bypass in the same breath as withholding it would
/// defeat the point. The shim is not taught this (see the doc above): its
/// refusal fires on the human's own merge attempt, and reproducing the Rust
/// body-drift evaluation in shell would be a second copy of the question.
pub(in crate::orchestration) const GATE_REFUSAL_EXITS_WITHOUT_UI_BYPASS: &str = "Two ways forward: (1) get the named \
     reviewer(s) to run and record a verdict, or (2) have the human turn workflow mode off for \
     this session (clears the gate).";

/// One-line notice delivered to the orchestrator when the advanced-orchestrator
/// (workflow-mode) toggle changes LIVE mid-session (#316), so it re-plans its
/// spawn/review strategy without waiting to re-read its kickoff config. `on`
/// carries the resolved workflow's name and its merge gate (if it declares
/// one); off has neither — the built-in roster and no gate, by construction.
pub fn workflow_mode_notice(on: bool, name: &str, gate: Option<&workflow::Gate>) -> String {
    if !on {
        return "[orrerix] workflow mode changed: built-in roster, no merge gate — re-plan your \
                spawn/review strategy."
            .to_string();
    }
    let gate_clause = gate_clause(gate);
    format!(
        "[orrerix] workflow mode changed: '{name}' active, {gate_clause} — re-plan your \
         spawn/review strategy."
    )
}

/// The gate half of a workflow notice, in one place (#1689 slice B).
///
/// Extracted from [`workflow_mode_notice`] rather than re-spelled in
/// [`workflow_switched_notice`]: two renderings of one gate are two chances to
/// describe the same clause differently, and the orchestrator reads both
/// notices as statements about the same file.
fn gate_clause(gate: Option<&workflow::Gate>) -> String {
    let Some(g) = gate else { return "no merge gate declared".to_string() };
    let require = match g.require {
        workflow::GateRequire::AllPass => "all of".to_string(),
        workflow::GateRequire::Threshold(n) => format!("{n} of"),
    };
    let mut clause = format!("merge gate requires {require} [{}]", g.reviewers.join(", "));
    if !g.also.is_empty() {
        clause.push_str(&format!(" · {}", g.also.join(", ")));
    }
    clause
}

/// One-line notice delivered to the orchestrator when the human applies a
/// different workflow to a LIVE group (#1689 slice B), so it re-plans its
/// spawn/review strategy without waiting to re-read its kickoff config.
///
/// Deliberately says the two things a switch changes about the orchestrator's
/// own next move and nothing else: **which ids it may spawn by from now on**,
/// and **what happens to a pane already running under an id the new roster
/// dropped** — it keeps running, and a bare `resume_session` of it is refused
/// by `spawn_agent_bound`'s unknown-block path. Neither is discoverable from
/// the pane, and getting either wrong costs a spawn attempt.
///
/// `blocks` is the roster AFTER the swap (already `clamped()`), `agent_cli` the
/// group default the empty `cli:`/`model:` keys inherit, and `removed` the ids
/// the diff reports gone.
///
/// One paragraph, like every notice loomux types into a pane: no newline and no
/// ten-space run (CLAUDE.md's shape rule — a `\` continuation that collapsed
/// leaves the second without the first).
pub fn workflow_switched_notice(
    from: &str,
    to: &str,
    agent_cli: &str,
    blocks: &[workflow::Block],
    removed: &[String],
    gate: Option<&workflow::Gate>,
) -> String {
    let roster = blocks
        .iter()
        .map(|b| {
            let mut row = format!(
                "{} ({}, {}, {}",
                b.id,
                b.kind.as_str(),
                workflow::cli_of(b, agent_cli),
                workflow::model_of(b, agent_cli)
            );
            if b.has_persona() {
                row.push_str(", persona");
            }
            row.push(')');
            row
        })
        .collect::<Vec<_>>()
        .join(", ");
    let gone = if removed.is_empty() { "none".to_string() } else { removed.join(", ") };
    format!(
        "[orrerix] workflow switched: '{from}' → '{to}' — blocks now: {roster}; removed: {gone}; \
         {} — spawn by these ids from now on; a pane already running under a removed block keeps \
         running, but a bare resume of its session will be refused.",
        gate_clause(gate)
    )
}

/// The workflow file THIS GROUP runs, as a repo-relative path (#1689).
///
/// Every group-scoped display, audit and template site goes through this rather
/// than through `workflow::workflow_path`, which answers only for `default`. The
/// pair with [`load_active_workflow`]: one resolves the path, the other reads
/// it, and both take the same two arguments so they cannot resolve differently.
///
/// `(repo, &Guardrails)` rather than `&GroupInfo`, deliberately: `create_group_ex`
/// and `promote_orchestrator_cli` decide which file to read BEFORE a `GroupInfo`
/// exists, and a second spelling for those would be exactly the drift this
/// function is here to prevent. A `GroupInfo` caller passes `&g.repo,
/// &g.guardrails`.
pub fn active_workflow_path(repo: &str, rails: &Guardrails) -> String {
    workflow::workflow_path_named(repo, &rails.workflow)
}

/// Read + validate the workflow file THIS GROUP runs (#1689).
///
/// Identical contract to `workflow::load_workflow`, which it generalises:
/// `Ok(None)` for "no such file", `Err` for "the file is there and will not
/// parse". A group pinned to `default` — every group that existed before #1689,
/// and every group in a repo with one workflow file — resolves to exactly the
/// file `load_workflow` would have opened.
pub fn load_active_workflow(
    repo: &str,
    rails: &Guardrails,
) -> Result<Option<workflow::Workflow>, Vec<String>> {
    workflow::load_workflow_named(repo, &rails.workflow)
}

/// The armed-gate payload, shared by `workflow_status` and the switch preview
/// (#1689 slice B).
///
/// `satisfiable`/`missing_blocks` are recomputed against whichever roster is
/// passed — the live one for a status, the roster the switch WOULD produce for a
/// preview. That is what lets the confirmation say "this gate names a reviewer
/// the new roster cannot spawn" before the human clicks, rather than leaving it
/// to `merge-gate-unsatisfiable` in the trail afterwards (#316's stance).
pub(in crate::orchestration) fn gate_json(g: &workflow::Gate, blocks: &[workflow::Block]) -> Value {
    let missing = workflow::gate_missing_blocks(g, blocks);
    json!({
        "require": match g.require {
            workflow::GateRequire::AllPass => "all-pass".to_string(),
            workflow::GateRequire::Threshold(n) => format!("threshold {n}"),
        },
        "reviewers": g.reviewers,
        "also": g.also,
        // #1174. `null` when undeclared — the pane keeps "no limit" apart from
        // any number, so the chip never announces a clause this repo did not
        // write.
        "max_diff_lines": g.max_diff_lines,
        // #1176. The rules AS DECLARED — which of them fire is a per-PR fact and
        // this payload has no PR to ask about. An empty list is "no routing",
        // which the chip must keep apart from "routing that happened not to
        // match", a sentence it cannot say here.
        "routing": g.routing.iter().map(|r| json!({
            "paths": r.paths,
            "reviewers": r.reviewers,
        })).collect::<Vec<_>>(),
        "satisfiable": missing.is_empty(),
        "missing_blocks": missing,
    })
}

/// The roster rows every workflow payload publishes, in one shape (#1689 slice
/// B): `workflow_status`'s live roster and a switch preview's proposed one.
pub(in crate::orchestration) fn roster_json(blocks: &[workflow::Block]) -> Value {
    json!(blocks
        .iter()
        .map(|b| json!({
            "id": b.id,
            "kind": b.kind.as_str(),
            "cli": b.cli,
            "model": b.model,
            "persona": b.has_persona(),
            // #2850: additive to every row the launcher preview and the MCP
            // `list_blocks` read-back publish, so a structured-intended block
            // is visible in both places without a second vocabulary.
            "driver": b.driver,
        }))
        .collect::<Vec<_>>())
}

/// A [`workflow::RosterDiff`] on the wire (#1689 slice B) — read by the audit
/// row a switch writes and by the confirmation the human approves it from, so
/// the trail records the same diff the human was shown.
pub(in crate::orchestration) fn diff_json(d: &workflow::RosterDiff) -> Value {
    json!({
        "added": d.added,
        "removed": d.removed,
        "changed": d.changed.iter().map(|c| json!({ "id": c.id, "fields": c.fields }))
            .collect::<Vec<_>>(),
        "gate_changed": d.gate_changed,
        "intake_changed": d.intake_changed,
        "orchestrator_cli_changed": d.orchestrator_cli_changed,
    })
}

/// Everything `apply_workflow(name)` would do, resolved once (#1689 slice B).
///
/// Built by `OrchRegistry::plan_workflow_switch` and consumed by both the
/// preview and the apply, which is the point: the diff a human confirms is the
/// diff that gets applied and audited, because there is only one.
pub(in crate::orchestration) struct SwitchPlan {
    /// The guardrails the group would run, already `clamped()`.
    pub(in crate::orchestration) guardrails: Guardrails,
    /// The `gates.merge` clause the target file declares, if any.
    pub(in crate::orchestration) gate: Option<workflow::Gate>,
    /// The target file's own `name:` field — human prose for the modal's title,
    /// never an identifier.
    display_name: String,
    /// The repo-relative path the name resolved to.
    pub(in crate::orchestration) path: String,
    /// A digest of the file this plan was resolved FROM, so an apply can refuse
    /// to act on a confirmation the human gave about different bytes. `None`
    /// means the digest could not be taken, which is "cannot confirm".
    pub(in crate::orchestration) digest: Option<String>,
    pub(in crate::orchestration) diff: workflow::RosterDiff,
    /// Set when the switch cannot be applied to a LIVE group however the human
    /// answers the confirmation — today, only an orchestrator-CLI change. The
    /// preview carries it BESIDE the diff so the modal can explain rather than
    /// just refuse.
    pub(in crate::orchestration) refusal: Option<String>,
    /// Keys on the orchestrator block that the apply writes to `group.json` but
    /// that the RUNNING pane will not pick up until it is resumed (`model`,
    /// `effort`, `context`). Empty is the common case.
    next_resume: Vec<String>,
}

impl SwitchPlan {
    /// The preview payload, and the source of `orch_workflow_switch_preview`'s
    /// wire shape.
    pub(in crate::orchestration) fn to_json(&self, info: &GroupInfo) -> Value {
        json!({
            "name": self.guardrails.workflow.as_str(),
            "from": info.guardrails.workflow.as_str(),
            "path": self.path,
            // Hand this straight back to `apply_workflow`: it is what binds the
            // confirmation the human gives to the bytes they were shown.
            "digest": self.digest,
            "display_name": self.display_name,
            // "Nothing would change" is a real answer and the modal has to be
            // able to say it, rather than presenting an empty confirmation.
            "empty": self.diff.is_empty(),
            "refusal": self.refusal,
            "next_resume": self.next_resume,
            "diff": diff_json(&self.diff),
            "blocks": roster_json(&self.guardrails.blocks),
            "gate": self.gate.as_ref().map(|g| gate_json(g, &self.guardrails.blocks)),
        })
    }
}

/// A group's pinned roster no longer matches what its ACTIVE workflow file now
/// says (#222 rev-11 F2; lifted out of the resume-only audit in #1689 slice B,
/// so the audit trail and the live badge cannot disagree about what drift is).
#[derive(Clone, Debug, PartialEq)]
pub struct RosterDrift {
    /// Which kind of drift, in the words the audit row and the badge share.
    pub note: &'static str,
    /// The block ids the file resolves to NOW. Empty when the file is gone or
    /// no longer validates — the group is running blocks its repo no longer
    /// declares, which is exactly the thing worth being able to see.
    pub on_disk: Vec<String>,
    /// The intake profile the file resolves to now; `None` in those same two
    /// arms, mirroring an empty `on_disk`.
    pub intake_on_disk: Option<workflow::IntakeProfile>,
}

/// Compare a group's pinned roster and intake against an ALREADY-LOADED read of
/// its active workflow file (#1689 slice B). `None` is "no drift".
///
/// **Takes the loaded `Result` rather than reading the file**, for the reason
/// `OrchRegistry::wip_rows` takes a resolved policy: both callers already have
/// one in hand, and `workflow_status` runs on the group-view publisher's
/// one-second cadence per leased group, where a second YAML parse of the same
/// file is a real cost. `audit_workflow_drift` passes its own load in;
/// `workflow_status_within` passes the load it already makes for the display
/// name and the board policy.
///
/// The comparison is against the roster the file *would resolve to* — the same
/// `clamped()` a launch applies — not against its raw blocks, so an inherited
/// model filled in at launch does not read as drift. A file that has since been
/// deleted or broken is drift too.
pub fn roster_drift(
    loaded: &Result<Option<workflow::Workflow>, Vec<String>>,
    g: &Guardrails,
) -> Option<RosterDrift> {
    let ids = |bs: &[workflow::Block]| -> Vec<String> { bs.iter().map(|b| b.id.clone()).collect() };
    // #382 NB2: intake drifts independently of the roster — a repo can rename
    // its label vocabulary without touching a single block, and that must be as
    // visible as a roster change. `roster_is_custom(&g.blocks)` alone still
    // gates every arm below correctly even though it only inspects blocks:
    // `blocks` and `intake` are ALWAYS resolved together, from the same `wf`, in
    // the same `Launch::Fresh` branch (`create_group_ex`) — a group's roster is
    // custom iff its intake profile could be too, so there is no case where
    // blocks reads "built-in" while intake is quietly running something declared.
    // The three paths that resolve a roster all set both: `create_group_ex`'s
    // Fresh arm, `apply_workflow`, and `set_advanced_orchestrator`'s two arms —
    // the last of which only became true at #2659 review round 1, and until then
    // was the one way a group could hold a declared roster beside a built-in
    // vocabulary and read as permanently drifted.
    match loaded {
        Ok(Some(wf)) => {
            let resolved_intake = wf.intake.clone();
            let resolved = Guardrails {
                agent_cli: g.agent_cli.clone(),
                blocks: wf.blocks.clone(),
                workflow: g.workflow.clone(),
                ..Guardrails::default()
            }
            .clamped()
            .blocks;
            if resolved == g.blocks && resolved_intake == g.intake {
                return None; // the file still says what the group is running — roster AND intake
            }
            // "Appeared" and "changed" are different events to a human reading
            // the trail, and only one of them means "somebody edited the file
            // you approved". A group whose running roster is the built-in four
            // was launched without a workflow in play at all — so the repo has
            // *gained* one since, and this group is simply not running it.
            let note = if workflow::roster_is_custom(&g.blocks) {
                "the file has changed since this group was launched"
            } else {
                "the repo has gained a workflow file since this group was launched"
            };
            Some(RosterDrift { note, on_disk: ids(&resolved), intake_on_disk: Some(resolved_intake) })
        }
        // No file, no workflow: nothing to drift from.
        Ok(None) if !workflow::roster_is_custom(&g.blocks) => None,
        Ok(None) => Some(RosterDrift {
            note: "the file the group was launched from is gone",
            on_disk: Vec::new(),
            intake_on_disk: None,
        }),
        Err(_) => Some(RosterDrift {
            note: "the file no longer validates",
            on_disk: Vec::new(),
            intake_on_disk: None,
        }),
    }
}

/// Read the block roster out of a group.json `guardrails` object (#222).
///
/// **Back-compat is the whole job here.** A group.json written before the block
/// model has no `blocks` array — it has the eight flat per-role fields
/// (`worker_cli`, `reviewer_model`, …). Reconstruct the same four blocks from
/// those, so a group launched on 0.8.0 rejoins on this build with exactly the
/// CLIs and models it had. An empty result is fine: `clamped()` fills it with
/// the built-in roster.
pub(in crate::orchestration) fn read_blocks(g: &Value) -> Vec<workflow::Block> {
    let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    if let Some(arr) = g["blocks"].as_array() {
        return arr
            .iter()
            .filter_map(|b| {
                let id = workflow::sanitize_id(&s(b, "id"))?;
                // An unrecognized kind is DROPPED, never coerced to worker — the
                // same rule the workflow parser enforces, applied to persisted
                // state, because a hand-edited group.json is the other way an
                // unknown kind could reach a spawn.
                let kind = workflow::kind_from_str(&s(b, "kind"))?;
                let name = workflow::sanitize_display(&s(b, "name"));
                Some(workflow::Block {
                    name: if name.is_empty() { id.clone() } else { name },
                    id,
                    kind,
                    cli: s(b, "cli"),
                    model: s(b, "model"),
                    prompt: b["prompt"].as_str().map(workflow::sanitize_persona),
                    profile: b["profile"].as_str().map(|p| p.trim().to_string()),
                    allow: b["allow"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str())
                                .filter_map(profiles::sanitize_allow)
                                .collect()
                        })
                        .unwrap_or_default(),
                    // Defense in depth, same shape as `kind` just above: a
                    // hand-edited group.json never meets `parse_workflow`, so a
                    // role_hint whose kind no longer matches (the file was
                    // edited to change one but not the other) is dropped
                    // silently here rather than resurrected — there is no
                    // human to show a parse error to at this layer.
                    role_hint: b["role_hint"].as_str().and_then(|raw| {
                        let hint = raw.trim().to_ascii_lowercase();
                        (workflow::role_hint_requires(&hint) == Some(kind)).then_some(hint)
                    }),
                    // #687: read raw and let `clamped()` decide — the block's
                    // effective CLI (its own, else the group default) isn't
                    // resolvable from one array element, and `clamped()` is
                    // the one place that already knows it. An ABSENT key is
                    // the pre-#687 group.json: empty, i.e. no knob, i.e.
                    // today's spawn byte for byte.
                    effort: s(b, "effort"),
                    context: s(b, "context"),
                    // #1457, defense in depth exactly like `role_hint` above: a
                    // hand-edited group.json never meets `parse_workflow`, so
                    // every rule that function enforces on a remote label is
                    // re-applied here and a value failing any of them is
                    // DROPPED rather than resurrected — there is no human to
                    // show a parse error to at this layer. An absent key is a
                    // local block, which is every group.json written before
                    // this field existed.
                    remote: b["remote"].as_str().and_then(|raw| {
                        let local_only = kind == Role::Orchestrator || kind == Role::Manager;
                        (PathSegment::parse(raw).is_ok()
                            && !local_only
                            && s(b, "cli") == "claude")
                            .then(|| raw.to_string())
                    }),
                    // #2850, defense in depth exactly like `role_hint` above: a
                    // hand-edited group.json never meets `parse_workflow`, so a
                    // driver value outside loomux's closed vocabulary is DROPPED
                    // here rather than resurrected — there is no human to show a
                    // parse error to at this layer. The per-CLI half (does this
                    // block's CLI actually carry a structured driver?) is the
                    // same question `effort`'s comment above defers: the block's
                    // effective CLI is not resolvable from one array element, so
                    // only the vocabulary is checked here.
                    driver: b["driver"].as_str().and_then(|raw| {
                        let want = raw.trim().to_ascii_lowercase();
                        workflow::DRIVER_MODES.contains(&want.as_str()).then_some(want)
                    }),
                    // #3407, the same defense in depth: a hand-edited value above
                    // the parser's ceiling is DROPPED (the CLI default applies)
                    // rather than resurrected into a display nobody validated.
                    cache_ttl_minutes: b["cache_ttl_minutes"]
                        .as_u64()
                        .filter(|&m| m <= loomux_engine::cacheage::CACHE_TTL_MINUTES_MAX as u64)
                        .map(|m| m as u32),
                })
            })
            .collect();
    }
    // Legacy shape (pre-#222).
    let pins = [
        (Role::Orchestrator, s(g, "orchestrator_cli"), s(g, "orchestrator_model")),
        (Role::Worker, s(g, "worker_cli"), s(g, "worker_model")),
        (Role::Reviewer, s(g, "reviewer_cli"), s(g, "reviewer_model")),
        (Role::Planner, s(g, "planner_cli"), s(g, "planner_model")),
    ];
    if pins.iter().all(|(_, cli, model)| cli.is_empty() && model.is_empty()) {
        return Vec::new(); // not a legacy file either — clamped() supplies the roster
    }
    workflow::default_roster(
        &pins.iter().map(|(k, c, m)| (*k, c.as_str(), m.as_str())).collect::<Vec<_>>(),
    )
}

/// Serialize the block roster for group.json. The inverse of [`read_blocks`];
/// `block_map_round_trips_through_group_json` pins the pair.
pub(in crate::orchestration) fn blocks_json(blocks: &[workflow::Block]) -> Value {
    Value::Array(
        blocks
            .iter()
            .map(|b| {
                json!({
                    "id": b.id,
                    "name": b.name,
                    "kind": b.kind.as_str(),
                    "cli": b.cli,
                    "model": b.model,
                    "prompt": b.prompt,
                    "profile": b.profile,
                    "allow": b.allow,
                    "role_hint": b.role_hint,
                    "effort": b.effort,
                    "context": b.context,
                    "remote": b.remote,
                    "driver": b.driver,
                    "cache_ttl_minutes": b.cache_ttl_minutes,
                })
            })
            .collect(),
    )
}

/// Read the resolved intake profile out of a group.json `guardrails` object
/// (#382 P1). Absent key (a group.json predating this field, or one where the
/// repo declared nothing) resolves to [`workflow::builtin_intake_profile`] —
/// the same migration guarantee #222's `blocks` gave. A hand-edited
/// group.json never met `parse_workflow`, so each label falls back to the
/// built-in value for its field rather than propagating an unusable string,
/// same defensive posture as [`read_blocks`]'s unrecognized-`kind` handling.
///
/// **The rule is `workflow::usable_intake_label`, which is the workflow
/// parser's own** (#2663). It used to be a local `sanitize_id` comparison, and
/// the two had drifted on exactly one value class: `sanitize_id` permits a
/// leading `-`, so `"hold": "--force"` in a hand-edited group.json was
/// accepted here and refused by the parser. That was invisible while
/// `guardrails.intake.hold` reached prose surfaces only; #2663 routes it to a
/// `gh label create <name>` positional, so the two rules are now one function
/// with three callers rather than two spellings that agreed by habit (CLAUDE.md
/// constraint 6's one-validating-constructor posture, applied to a label).
pub(in crate::orchestration) fn read_intake(g: &Value) -> workflow::IntakeProfile {
    let default = workflow::IntakeProfile::default();
    let Some(i) = g.get("intake") else { return default };
    let source = workflow::intake_source_from_str(i["source"].as_str().unwrap_or(""))
        .unwrap_or(default.source);
    let label = |k: &str, fallback: &str| -> String {
        workflow::usable_intake_label(i["labels"][k].as_str().unwrap_or(""))
            .unwrap_or_else(|| fallback.to_string())
    };
    workflow::IntakeProfile {
        source,
        ready: label("ready", &default.ready),
        investigate: label("investigate", &default.investigate),
        owned: label("owned", &default.owned),
        prototype: label("prototype", &default.prototype),
        hold: label("hold", &default.hold),
    }
}

/// Serialize the resolved intake profile for group.json. The inverse of
/// [`read_intake`].
pub(in crate::orchestration) fn intake_json(p: &workflow::IntakeProfile) -> Value {
    json!({
        "source": p.source.as_str(),
        "labels": {
            "ready": p.ready,
            "investigate": p.investigate,
            "owned": p.owned,
            "prototype": p.prototype,
            "hold": p.hold,
        },
    })
}
