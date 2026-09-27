//! The built-in roster a group gets with no workflow file, and the
//! [`RosterDiff`] a live workflow switch is confirmed against (#1689 slice B).

use super::*;

// ── the built-in roster ─────────────────────────────────────────────────────

/// Today's fixed 4-block roster, synthesized from the launcher's per-role CLI
/// and model picks (#222). This is what a group gets when the repo has no
/// `.loomux/workflow.yml` — and it is deliberately *exactly* the pre-block
/// behavior: the ids are the four role names, so the instruction files keep
/// their historic paths; no block carries a persona, so nothing is added to any
/// command line. `default_roster_command_lines_match_legacy` pins that.
///
/// `pins` is `(kind, cli, model)` per role; an empty `cli`/`model` inherits the
/// group default / the kind's default model, exactly as the flat per-role
/// guardrail fields did.
pub fn default_roster(pins: &[(Role, &str, &str)]) -> Vec<Block> {
    default_roster_ex(
        &pins.iter().map(|(k, c, m)| (*k, *c, *m, ModelKnobs::default())).collect::<Vec<_>>(),
    )
}

/// [`default_roster`] plus the per-role model knobs the launcher can pin
/// (#687). The 3-tuple form above stays, and stays the one most callers use:
/// every roster loomux synthesizes *on a group's behalf* (`clamped()`'s
/// guaranteed orchestrator, the legacy-group.json reconstruction, the built-in
/// roster) pins no knob by construction, so making them all spell
/// `ModelKnobs::default()` would be noise — and leaving those call sites
/// untouched keeps every existing assertion about their command lines a pin on
/// "no knobs ⇒ the pre-#687 line, byte for byte".
pub fn default_roster_ex(pins: &[(Role, &str, &str, ModelKnobs<'_>)]) -> Vec<Block> {
    pins.iter()
        .map(|(kind, cli, model, knobs)| Block {
            id: kind.as_str().to_string(),
            name: kind.as_str().to_string(),
            kind: *kind,
            cli: cli.trim().to_string(),
            model: model.trim().to_string(),
            prompt: None,
            profile: None,
            allow: Vec::new(),
            role_hint: None,
            // Raw as picked; `Guardrails::clamped` is what drops a value the
            // resolved CLI cannot honor (the same treatment `model` gets).
            effort: knobs.effort.trim().to_string(),
            context: knobs.context.trim().to_string(),
            // #1457: every roster loomux synthesizes on a group's behalf is
            // LOCAL. A remote block can only come from a repo's workflow file,
            // which is the only surface the operator opted a label into.
            remote: None,
            // Same shape: a structured driver can only come from a repo's
            // workflow file — the built-in roster is the pre-#2850 behavior,
            // PTY panes throughout.
            driver: None,
            // #3407: the CLI's own default TTL — a synthesized roster pins none.
            cache_ttl_minutes: None,
        })
        .collect()
}

/// The built-in roster with every block on `agent_cli` and its default model —
/// the roster a group gets from a launcher that pinned nothing per role.
pub fn builtin_roster(agent_cli: &str) -> Vec<Block> {
    default_roster(&[
        (Role::Orchestrator, agent_cli, ""),
        (Role::Worker, agent_cli, ""),
        (Role::Reviewer, agent_cli, ""),
        (Role::Planner, agent_cli, ""),
    ])
}

// ── the roster diff a live workflow switch is confirmed against (#1689 slice B) ──

/// What one block's row changed BETWEEN two rosters, as the list of keys that
/// differ.
///
/// The keys are the workflow file's own spelling (`cli`, `model`, `prompt`,
/// `effort`, …), sorted, so the confirmation a human reads names the thing they
/// would edit rather than a Rust field path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockChange {
    /// The block id, which is the same on both sides — a row whose id moved is
    /// a removal plus an addition, never a change.
    pub id: BlockId,
    /// The differing keys, sorted.
    pub fields: Vec<String>,
}

/// One side of a [`roster_diff`]: everything about a group that an apply
/// replaces, as one value.
///
/// A struct rather than three positional arguments per side, so a call site
/// cannot pair the old gate with the new intake.
#[derive(Clone, Copy, Debug)]
pub struct RosterSide<'a> {
    pub blocks: &'a [Block],
    pub gate: Option<&'a Gate>,
    pub intake: &'a IntakeProfile,
}

/// The difference between the roster a group is running and the one a named
/// workflow file would give it (#1689 slice B).
///
/// **Pure, and the only definition of "what would this switch change".** The
/// preview a human confirms, the notice the orchestrator receives and the audit
/// row all read this one value, so the three cannot disagree about what an
/// apply did — the same reason the drift comparison is extracted once rather
/// than written per consumer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RosterDiff {
    /// Block ids the new roster declares and the old one did not, sorted.
    pub added: Vec<BlockId>,
    /// Block ids the old roster declared and the new one does not, sorted.
    /// These are the ids a bare resume is refused under after the apply.
    pub removed: Vec<BlockId>,
    /// Ids present on both sides whose row differs, sorted by id.
    pub changed: Vec<BlockChange>,
    /// The merge gate differs. Either side may be `None` — gaining or losing a
    /// gate is a change like any other.
    pub gate_changed: bool,
    /// The resolved intake profile differs. It drifts independently of the
    /// roster (#382 NB2): a repo can rename its label vocabulary without
    /// touching a single block.
    pub intake_changed: bool,
    /// The orchestrator block's EFFECTIVE CLI differs, which is the one change
    /// an apply cannot make to a live group: that pane is already running a
    /// program and a session cannot change program mid-flight — the same reason
    /// `promote_orchestrator_cli` refuses it.
    ///
    /// Effective, not declared: see [`roster_diff`]'s note on `agent_cli`.
    pub orchestrator_cli_changed: bool,
}

impl RosterDiff {
    /// Nothing would change. Re-applying a file nobody edited lands here, and
    /// the UI says so rather than offering a confirmation with an empty body.
    ///
    /// `orchestrator_cli_changed` is deliberately not read: it is a *reason to
    /// refuse*, never a change on its own — it can only ever be set alongside
    /// the `changed` row (or the add/remove pair) that carries the `cli` key it
    /// was derived from.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.changed.is_empty()
            && !self.gate_changed
            && !self.intake_changed
    }
}

/// Compare two rosters, gates and intake profiles (#1689 slice B).
///
/// `agent_cli` is the group's default CLI, and both sides' blocks are resolved
/// through it ([`cli_of`], [`model_of`]) before comparison. That asymmetry with
/// every other key is deliberate, and is the whole reason this takes the
/// parameter: `cli:` and `model:` are the two keys whose EMPTY value means
/// "inherit", so a file that spells out the CLI or model the group already runs
/// would otherwise read as a change — and for `cli` on the orchestrator block
/// that false positive is not cosmetic, it is a refused apply. Every other key
/// is compared exactly as declared, because for every other key the declared
/// value IS the effective one.
///
/// The per-block comparison destructures `Block` exhaustively, with no `..`, so
/// a field ADDED to the schema is a compile error here rather than a key the
/// diff silently stops reporting — the same field-inventory idiom the intake
/// schema uses to keep a new key from arriving unnoticed.
pub fn roster_diff(agent_cli: &str, old: RosterSide<'_>, new: RosterSide<'_>) -> RosterDiff {
    // A nested fn, not a closure: a closure cannot express "the map borrows
    // from the slice it was given", so its two elided lifetimes are unrelated
    // and the collect does not compile.
    fn by_id(bs: &[Block]) -> BTreeMap<&str, &Block> {
        bs.iter().map(|b| (b.id.as_str(), b)).collect()
    }
    let (o, n) = (by_id(old.blocks), by_id(new.blocks));

    let added: Vec<BlockId> =
        n.keys().filter(|k| !o.contains_key(*k)).map(|k| k.to_string()).collect();
    let removed: Vec<BlockId> =
        o.keys().filter(|k| !n.contains_key(*k)).map(|k| k.to_string()).collect();
    let mut changed = Vec::new();
    for (id, ob) in &o {
        let Some(nb) = n.get(id) else { continue };
        let fields = changed_fields(agent_cli, ob, nb);
        if !fields.is_empty() {
            changed.push(BlockChange { id: id.to_string(), fields });
        }
    }

    // Resolved by KIND, not read off `changed`: `changed`'s rows are keyed by an
    // id present on BOTH sides, so a switch that renames the orchestrator block
    // would have no row to carry the `cli` key at all. A roster carries exactly
    // one orchestrator block (`clamped`), and `apply_workflow` runs `clamped()`
    // before it calls this.
    let orch = |side: &[Block]| -> Option<Block> {
        side.iter().find(|b| b.kind == Role::Orchestrator).cloned()
    };
    let orchestrator_cli_changed = match (orch(old.blocks), orch(new.blocks)) {
        (Some(a), Some(b)) => cli_of(&a, agent_cli) != cli_of(&b, agent_cli),
        // No orchestrator block on one side: there is no CLI to have changed.
        _ => false,
    };

    RosterDiff {
        added,
        removed,
        changed,
        gate_changed: old.gate != new.gate,
        intake_changed: old.intake != new.intake,
        orchestrator_cli_changed,
    }
}

/// The workflow-file keys on which two rows for ONE block id differ, sorted.
///
/// See [`roster_diff`] for why `cli` and `model` are resolved through
/// `agent_cli` and nothing else is.
fn changed_fields(agent_cli: &str, a: &Block, b: &Block) -> Vec<String> {
    // Exhaustive, no `..`: a field added to `Block` fails to compile here rather
    // than becoming a key this diff silently stops reporting.
    let Block {
        id: a_id,
        name: a_name,
        kind: a_kind,
        cli: _,
        model: _,
        prompt: a_prompt,
        profile: a_profile,
        allow: a_allow,
        role_hint: a_role_hint,
        effort: a_effort,
        context: a_context,
        remote: a_remote,
        driver: a_driver,
        cache_ttl_minutes: a_cache_ttl,
    } = a;
    let Block {
        id: b_id,
        name: b_name,
        kind: b_kind,
        cli: _,
        model: _,
        prompt: b_prompt,
        profile: b_profile,
        allow: b_allow,
        role_hint: b_role_hint,
        effort: b_effort,
        context: b_context,
        remote: b_remote,
        driver: b_driver,
        cache_ttl_minutes: b_cache_ttl,
    } = b;
    debug_assert_eq!(a_id, b_id, "changed_fields compares two rows for ONE block id");
    let mut out: Vec<String> = Vec::new();
    let mut push = |cond: bool, key: &str| {
        if cond {
            out.push(key.to_string());
        }
    };
    push(a_name != b_name, "name");
    push(a_kind != b_kind, "kind");
    push(cli_of(a, agent_cli) != cli_of(b, agent_cli), "cli");
    push(model_of(a, agent_cli) != model_of(b, agent_cli), "model");
    push(a_prompt != b_prompt, "prompt");
    push(a_profile != b_profile, "profile");
    push(a_allow != b_allow, "allow");
    push(a_role_hint != b_role_hint, "role_hint");
    push(a_effort != b_effort, "effort");
    push(a_context != b_context, "context");
    push(a_remote != b_remote, "remote");
    push(a_driver != b_driver, "driver");
    push(a_cache_ttl != b_cache_ttl, "cache_ttl_minutes");
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── the roster diff a switch is confirmed against (#1689 slice B) ────
    //
    // Unit tests, for the reason the discovery block above states: the
    // comparison is pure and nothing here links the Tauri lib.

    /// A workflow doc whose blocks are given as raw YAML rows, so a case can
    /// vary exactly one key.
    fn diff_doc(blocks: &str, extra: &str) -> String {
        format!("version: 1\nname: d\nblocks:\n{blocks}{extra}")
    }

    /// The three-part side a diff takes, built from a parsed document.
    fn side(wf: &Workflow) -> RosterSide<'_> {
        RosterSide { blocks: &wf.blocks, gate: wf.gates.get("merge"), intake: &wf.intake }
    }

    const TWO: &str = "  - id: w\n    kind: worker\n  - id: r\n    kind: reviewer\n";

    #[test]
    fn an_unedited_file_reapplied_changes_nothing() {
        let wf = parse_workflow(&diff_doc(TWO, "")).unwrap();
        let d = roster_diff("claude", side(&wf), side(&wf));
        assert_eq!(d, RosterDiff::default(), "a roster compared with itself must be empty");
        assert!(d.is_empty());
    }

    #[test]
    fn a_model_only_edit_is_exactly_one_changed_row_naming_one_key() {
        // AC 4: the one-row model swap is the same mechanism as a switch, and
        // its diff must say so — one row, one key, nothing added or removed.
        let old = parse_workflow(&diff_doc(TWO, "")).unwrap();
        let new =
            parse_workflow(&diff_doc("  - id: w\n    kind: worker\n    model: opus\n  - id: r\n    kind: reviewer\n", ""))
                .unwrap();
        let d = roster_diff("claude", side(&old), side(&new));
        assert_eq!(d.changed, vec![BlockChange { id: "w".into(), fields: vec!["model".into()] }]);
        assert!(d.added.is_empty() && d.removed.is_empty(), "{d:?}");
        assert!(!d.gate_changed && !d.intake_changed, "{d:?}");
        assert!(!d.is_empty(), "a real edit is not an empty diff");
    }

    #[test]
    fn an_added_and_a_removed_block_are_not_a_change_row() {
        let old = parse_workflow(&diff_doc(TWO, "")).unwrap();
        let new = parse_workflow(&diff_doc(
            "  - id: w\n    kind: worker\n  - id: rev-deep\n    kind: reviewer\n",
            "",
        ))
        .unwrap();
        let d = roster_diff("claude", side(&old), side(&new));
        assert_eq!(d.added, vec!["rev-deep".to_string()]);
        assert_eq!(d.removed, vec!["r".to_string()]);
        assert!(d.changed.is_empty(), "a row whose id moved is an add plus a remove: {d:?}");
    }

    #[test]
    fn every_declared_key_a_block_carries_is_reported_by_its_own_name() {
        // The exhaustive destructure in `changed_fields` makes a NEW field a
        // compile error; this makes each EXISTING one observable, so a `push`
        // line deleted from that function is caught by a test rather than by
        // nothing. `id` is the join key and `kind` is covered separately (a
        // kind change on one id is a legal edit of a block's class).
        // Each row is the WHOLE pair of extra key lines, not a delta against a
        // shared base: `RawBlock` is `deny_unknown_fields` and serde refuses a
        // DUPLICATE key outright, so a base that pre-declared `cli:` made the
        // `cli` row an unparseable document rather than a comparison.
        const CLAUDE: &str = "    cli: claude\n";
        let rows: &[(&str, &str, &str)] = &[
            ("name", "    name: Alpha\n", "    name: Beta\n"),
            ("cli", "    cli: claude\n", "    cli: copilot\n"),
            ("model", "    model: sonnet\n", "    model: opus\n"),
            ("prompt", "    prompt: One.\n", "    prompt: Two.\n"),
            ("profile", "", "    profile: .github/agents/w.md\n"),
            ("allow", "", "    allow: [\"Bash(gh pr view)\"]\n"),
            // effort:/context:/remote: are only legal on a CLI that can carry
            // them, so both sides declare `cli: claude` and only the key under
            // test moves.
            ("effort", CLAUDE, "    cli: claude\n    effort: high\n"),
            ("context", CLAUDE, "    cli: claude\n    context: 1m\n"),
            ("remote", CLAUDE, "    cli: claude\n    remote: buildbox\n"),
        ];
        for &(key, a, b) in rows {
            let base = "  - id: w\n    kind: worker\n";
            let old = parse_workflow(&diff_doc(&format!("{base}{a}"), "")).unwrap();
            let new = parse_workflow(&diff_doc(&format!("{base}{b}"), "")).unwrap();
            let d = roster_diff("claude", side(&old), side(&new));
            let fields: Vec<&str> =
                d.changed.first().map(|c| c.fields.iter().map(String::as_str).collect()).unwrap_or_default();
            assert!(
                fields.contains(&key),
                "editing {key:?} must be reported under that name, got {fields:?}"
            );
        }
    }

    #[test]
    fn a_role_hint_change_is_reported_under_its_own_key() {
        // Split from the sweep above because `role_hint` is only legal paired
        // with a specific `kind` (`advisor` needs `planner`), so its two sides
        // cannot share the sweep's worker base row.
        let old = parse_workflow(&diff_doc("  - id: p\n    kind: planner\n", "")).unwrap();
        let new = parse_workflow(&diff_doc("  - id: p\n    kind: planner\n    role_hint: advisor\n", "")).unwrap();
        let d = roster_diff("claude", side(&old), side(&new));
        assert_eq!(d.changed, vec![BlockChange { id: "p".into(), fields: vec!["role_hint".into()] }]);
    }

    #[test]
    fn a_kind_change_on_one_id_is_a_changed_row() {
        let old = parse_workflow(&diff_doc("  - id: x\n    kind: worker\n", "")).unwrap();
        let new = parse_workflow(&diff_doc("  - id: x\n    kind: reviewer\n", "")).unwrap();
        let d = roster_diff("claude", side(&old), side(&new));
        assert_eq!(d.changed, vec![BlockChange { id: "x".into(), fields: vec!["kind".into()] }]);
    }

    #[test]
    fn spelling_out_the_inherited_cli_or_model_is_not_a_change() {
        // The one asymmetry `roster_diff` documents: `cli:` and `model:` are
        // compared EFFECTIVE, because empty means "inherit". A file that writes
        // down what the group already runs changes nothing, and for `cli` on the
        // orchestrator block a false positive here is a REFUSED apply, not a
        // cosmetic row.
        let bare = parse_workflow(&diff_doc("  - id: w\n    kind: worker\n", "")).unwrap();
        let spelt = parse_workflow(&diff_doc(
            &format!("  - id: w\n    kind: worker\n    cli: claude\n    model: {}\n", default_model("claude", Role::Worker)),
            "",
        ))
        .unwrap();
        let d = roster_diff("claude", side(&bare), side(&spelt));
        assert!(d.is_empty(), "inherit vs the same value spelled out is not a change: {d:?}");

        // Negative control: against a DIFFERENT group default, the same pair of
        // documents does differ — so the assertion above is about the values,
        // not about the comparison being switched off.
        let d2 = roster_diff("copilot", side(&bare), side(&spelt));
        assert!(!d2.is_empty(), "under a copilot default, spelling out claude IS a change");
    }

    #[test]
    fn an_orchestrator_cli_change_is_flagged_and_a_worker_one_is_not() {
        let base = "  - id: orchestrator\n    kind: orchestrator\n  - id: w\n    kind: worker\n";
        let old = parse_workflow(&diff_doc(base, "")).unwrap();
        let orch_moved = parse_workflow(&diff_doc(
            "  - id: orchestrator\n    kind: orchestrator\n    cli: copilot\n  - id: w\n    kind: worker\n",
            "",
        ))
        .unwrap();
        let worker_moved = parse_workflow(&diff_doc(
            "  - id: orchestrator\n    kind: orchestrator\n  - id: w\n    kind: worker\n    cli: copilot\n",
            "",
        ))
        .unwrap();
        assert!(roster_diff("claude", side(&old), side(&orch_moved)).orchestrator_cli_changed);
        assert!(
            !roster_diff("claude", side(&old), side(&worker_moved)).orchestrator_cli_changed,
            "a delegate's CLI moves freely — only the running orchestrator pane cannot"
        );
        // Positive control: the worker case IS a change, just not that one.
        assert!(!roster_diff("claude", side(&old), side(&worker_moved)).is_empty());
    }

    #[test]
    fn a_renamed_orchestrator_block_still_has_its_cli_compared() {
        // Read off `changed` instead of by kind, this case would report nothing:
        // there is no id present on both sides to carry the `cli` key.
        let old = parse_workflow(&diff_doc("  - id: orchestrator\n    kind: orchestrator\n", "")).unwrap();
        let new = parse_workflow(&diff_doc(
            "  - id: lead\n    kind: orchestrator\n    cli: copilot\n",
            "",
        ))
        .unwrap();
        let d = roster_diff("claude", side(&old), side(&new));
        assert!(d.changed.is_empty(), "the id moved, so there is no changed row: {d:?}");
        assert!(d.orchestrator_cli_changed, "and the CLI change must still be seen: {d:?}");
    }

    #[test]
    fn gaining_losing_or_editing_a_gate_is_a_gate_change() {
        let none = parse_workflow(&diff_doc(TWO, "")).unwrap();
        let gated = parse_workflow(&diff_doc(TWO, "gates:\n  merge:\n    reviewers: [r]\n")).unwrap();
        let gated2 =
            parse_workflow(&diff_doc(TWO, "gates:\n  merge:\n    reviewers: [r]\n    also: [ci-green]\n")).unwrap();
        assert!(roster_diff("claude", side(&none), side(&gated)).gate_changed, "gaining a gate");
        assert!(roster_diff("claude", side(&gated), side(&none)).gate_changed, "losing a gate");
        assert!(roster_diff("claude", side(&gated), side(&gated2)).gate_changed, "editing a gate");
        assert!(!roster_diff("claude", side(&gated), side(&gated)).gate_changed, "an unedited gate");
    }

    #[test]
    fn an_intake_rename_is_a_change_with_no_block_touched() {
        // #382 NB2: intake drifts independently of the roster, and the diff has
        // to say so or a switch that renames the human's veto label would be
        // confirmed as "nothing changes".
        let old = parse_workflow(&diff_doc(TWO, "")).unwrap();
        let new =
            parse_workflow(&diff_doc(TWO, "intake:\n  labels:\n    hold: do-not-touch\n")).unwrap();
        let d = roster_diff("claude", side(&old), side(&new));
        assert!(d.intake_changed, "{d:?}");
        assert!(d.added.is_empty() && d.removed.is_empty() && d.changed.is_empty(), "{d:?}");
        assert!(!d.is_empty(), "an intake-only change is still a change");
    }
}
