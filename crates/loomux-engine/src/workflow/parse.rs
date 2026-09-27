//! The YAML wire format (the `Raw*` mirrors) and [`parse_workflow`], which
//! validates it into the schema, plus the loaders built on it.

use super::*;

// ── the YAML wire format ────────────────────────────────────────────────────
//
// Deserialized into `Raw*` mirrors first, then validated into the domain types
// above. Two reasons for the split: `kind` must produce a *readable* error
// rather than serde's "unknown variant" prose, and `deny_unknown_fields` needs
// to sit on the wire types so a typo (`promt:`) is caught instead of ignored —
// the failure mode every surveyed workflow tool has (Dify will happily publish
// a workflow whose plugin node isn't installed).

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawWorkflow {
    version: u32,
    #[serde(default)]
    name: String,
    /// The loomux version that authored the file. Optional, informational, and
    /// **never** a validation error — see [`Workflow::authored_with`]. Declared
    /// here (rather than left to `deny_unknown_fields`) precisely so that a file
    /// written by the workflow pane still loads.
    #[serde(default)]
    authored_with: String,
    #[serde(default)]
    blocks: Vec<RawBlock>,
    #[serde(default)]
    edges: Vec<RawEdge>,
    #[serde(default)]
    gates: BTreeMap<String, RawGate>,
    /// Intake source + label vocabulary (#382 P1). `None` when the file
    /// declares no `intake:` block — resolved to [`builtin_intake_profile`]
    /// entirely. **No `human_gate:`/disable-spelling key exists on this type
    /// or `RawIntakeLabels`** — `deny_unknown_fields` turns any attempt at one
    /// into a hard parse error rather than an ignored line. This is the whole
    /// enforcement of the CRITICAL invariant: the human merge gate is not
    /// reachable from this schema at all.
    #[serde(default)]
    intake: Option<RawIntake>,
    /// Merge-queue policy (#581 §11.2). `None` when the file declares no
    /// `merge_queue:` block — which resolves to
    /// [`MergeQueuePolicy::default`], i.e. **off**, and behavior is
    /// byte-for-byte unchanged.
    ///
    /// Like `intake:`, this block can never grant a capability: every field is
    /// a bool or a number, there is no spelling that names a branch to land on
    /// (§4 — the target comes from the enqueued PR's live base), and the
    /// default-branch refusals (§7) are not reachable from this schema at all.
    #[serde(default)]
    merge_queue: Option<RawMergeQueue>,
    /// Review-driver policy (#1778 §5.3). `None` when the file declares no
    /// `driver:` block - which resolves to [`DriverPolicy::default`], i.e.
    /// **off**, and behavior is byte-for-byte unchanged.
    ///
    /// Like `merge_queue:`, this block can never grant a capability on its
    /// own: every field is a bool or a number from a closed range, and the
    /// two-key rule (§3.2) is what actually holds the line - a drive exists
    /// only once an orchestrator's own role-gated `drive_review` call names
    /// one PR, or (#3367 `auto_drive_on_done`) a worker it spawned reports
    /// done on its own branch's PR. `deny_unknown_fields` on [`RawDriver`]
    /// makes any OTHER key that could start, target or widen a drive a hard
    /// parse error rather than an ignored line.
    #[serde(default)]
    driver: Option<RawDriver>,
    /// Delivery-triage policy (#3304 S1). `None` when the file declares no
    /// `triage:` block - which resolves to [`TriagePolicy::default`], i.e.
    /// **off**, and behavior is byte-for-byte unchanged.
    ///
    /// Like `driver:`, this block can never grant a capability: `enabled` is
    /// a bool, `max_defer_minutes` is a number from a closed range, and the
    /// two string-shaped keys are each a CLOSED vocabulary the parse refuses
    /// outside. The rule table it switches on is compiled in - there is no
    /// spelling here that writes a rule, names a pane, or reaches a network.
    #[serde(default)]
    triage: Option<RawTriage>,
    /// Named lock resources (#858). Absent (or empty) means no group in this
    /// repo gets the lock tools at all.
    ///
    /// Like `intake:` and `merge_queue:`, this block can never grant a
    /// capability: every field is a number, there is no spelling that names a
    /// program, a path, or an agent, and `deny_unknown_fields` on
    /// [`RawResource`] makes an attempt at one a hard parse error rather than
    /// an ignored line.
    #[serde(default)]
    resources: BTreeMap<String, RawResource>,
    /// Board policy — per-status WIP limits (#1175). `None` when the file
    /// declares no `board:` block, which resolves to [`BoardPolicy::default`],
    /// i.e. **no limits**, and behavior is byte-for-byte unchanged.
    ///
    /// Like `intake:`, `merge_queue:` and `resources:`, this block can never
    /// grant a capability: every field is a bool or a number, there is no
    /// spelling that names a program, a path, an agent or a branch, and
    /// `deny_unknown_fields` on [`RawBoard`]/[`RawWip`] makes an attempt at
    /// one a hard parse error rather than an ignored line.
    #[serde(default)]
    board: Option<RawBoard>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawBoard {
    #[serde(default)]
    wip: Option<RawWip>,
    #[serde(default)]
    enforce: bool,
}

/// The wire shape of `board.wip` — **one optional field per cappable board
/// status**, deliberately closed rather than a `BTreeMap<String, u32>`.
///
/// A map would accept `in-porgress: 4` and declare a limit on nothing, in
/// silence, for the lifetime of the file: an open key namespace cannot tell a
/// typo from a status a newer loomux might have. The closed struct hands that
/// check to `deny_unknown_fields`, whose error already names every field it
/// *would* have accepted — so the repo that misspells a status is told which
/// spellings exist, at parse time, without this module writing the check.
///
/// The price is that the field list is a second copy of `TASK_STATUSES` (which
/// lives in `src-tauri`, on the other side of an arrow this crate may not
/// point back along). It is pinned, not trusted:
/// `src-tauri/tests/workflow/blocks.rs` asserts this struct's serde field names
/// are exactly `TASK_STATUSES` minus [`WIP_UNCAPPABLE_STATUS`], so a ninth
/// status reddens rather than quietly arriving uncappable.
///
/// `Option<u32>` rather than a defaulted number so "omitted" and "written as
/// 1" stay distinguishable: the parse refuses a zero, and refusing one the
/// author never wrote would be an error about nothing (the reasoning
/// [`RawResource::slots`] states).
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawWip {
    #[serde(default)]
    queued: Option<u32>,
    #[serde(default, rename = "in-progress")]
    in_progress: Option<u32>,
    #[serde(default)]
    review: Option<u32>,
    #[serde(default)]
    pr: Option<u32>,
    #[serde(default)]
    prototype: Option<u32>,
    #[serde(default, rename = "human-testing")]
    human_testing: Option<u32>,
    #[serde(default)]
    blocked: Option<u32>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawResource {
    /// `Option` rather than a defaulted number so "omitted" and "written as 1"
    /// are distinguishable — the parse below rejects a zero, and rejecting one
    /// the author never wrote would be an error about nothing. (Same reasoning
    /// as [`RawMergeQueue::max_batch`].)
    #[serde(default)]
    slots: Option<u32>,
    #[serde(default)]
    max_hold_minutes: Option<u32>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawMergeQueue {
    #[serde(default)]
    enabled: bool,
    /// `Option` rather than a defaulted number so "omitted" and "written as 3"
    /// are distinguishable - the parse below rejects a zero, and rejecting one
    /// the author never wrote would be an error about nothing.
    #[serde(default)]
    max_batch: Option<u32>,
    #[serde(default)]
    checks_timeout_minutes: Option<u32>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawDriver {
    #[serde(default)]
    enabled: bool,
    /// INVARIANT 9's counters (§2.3), each `Option` for the same reason
    /// [`RawMergeQueue::max_batch`] is: "omitted" and "written as 3" must stay
    /// distinguishable, so the parse can tell an author who accepted the
    /// default from one who pinned it - and refuse a value outside the closed
    /// range instead of silently substituting one.
    #[serde(default)]
    max_review_rounds: Option<u32>,
    #[serde(default)]
    max_ci_attempts: Option<u32>,
    #[serde(default)]
    max_rebase_attempts: Option<u32>,
    /// The three backstops (§2.1), clamped by the notify-TTL clamp itself -
    /// the same quantity, a bounded wait on a fallible signal.
    #[serde(default)]
    lane_timeout_minutes: Option<u32>,
    #[serde(default)]
    fix_timeout_minutes: Option<u32>,
    #[serde(default)]
    drive_timeout_minutes: Option<u32>,
    /// The plan driver's three keys (#3040 §2). `plan_enabled` is a bare bool
    /// like [`Self::enabled`]; the two minute figures are `Option` for the
    /// reason the counters above are — "omitted" and "written as the default"
    /// must stay distinguishable, so the parse can refuse an out-of-range value
    /// instead of silently substituting one.
    #[serde(default)]
    plan_enabled: bool,
    #[serde(default)]
    plan_review_minutes: Option<u32>,
    #[serde(default)]
    planner_timeout_minutes: Option<u32>,
    /// #3367's two keys. The count is `Option` for the counters' reason — an
    /// out-of-range value is refused, never replaced — and the switch is a bare
    /// bool like the other two.
    #[serde(default)]
    fix_nonblocking_rounds: Option<u32>,
    #[serde(default)]
    auto_drive_on_done: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawTriage {
    #[serde(default)]
    enabled: bool,
    /// `Option` for [`RawMergeQueue::max_batch`]'s reason: "omitted" and
    /// "written as `none`" must stay distinguishable, so a future slice can
    /// tell an author who accepted the default from one who pinned it.
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    kinds: Vec<String>,
    #[serde(default)]
    max_defer_minutes: Option<u32>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawIntake {
    #[serde(default)]
    source: String,
    #[serde(default)]
    labels: RawIntakeLabels,
}

#[derive(Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
struct RawIntakeLabels {
    #[serde(default)]
    ready: String,
    #[serde(default)]
    investigate: String,
    #[serde(default)]
    owned: String,
    #[serde(default)]
    prototype: String,
    #[serde(default)]
    hold: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawBlock {
    id: String,
    #[serde(default)]
    name: String,
    kind: String,
    #[serde(default)]
    cli: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    role_hint: Option<String>,
    #[serde(default)]
    effort: String,
    #[serde(default)]
    context: String,
    /// #1457. `Option` rather than a defaulted `String` so "omitted" and
    /// "written empty" stay distinguishable: an empty label is refused, and
    /// refusing one the author never wrote would be an error about nothing
    /// (the reasoning [`RawMergeQueue::max_batch`] states).
    #[serde(default)]
    remote: Option<String>,
    /// #2850. HOW loomux drives this block's agent — `structured` over the
    /// CLI's structured-protocol surface instead of a scraped PTY. `Option`
    /// for the same reason `remote` is: "omitted" (a PTY pane, every file
    /// written before this key existed) and "written empty" stay
    /// distinguishable, and `deny_unknown_fields` keeps a build that predates
    /// the key refusing a file that declares it rather than silently
    /// spawning a structured-intended block as a PTY pane.
    #[serde(default)]
    driver: Option<String>,
    /// #3407. `Option` so "omitted" (the CLI default) and `0` ("unknown")
    /// stay two different answers.
    #[serde(default)]
    cache_ttl_minutes: Option<u32>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawEdge {
    from: String,
    to: OneOrMany,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawGate {
    #[serde(default)]
    require: Option<String>,
    #[serde(default)]
    threshold: Option<u32>,
    #[serde(default)]
    reviewers: Vec<String>,
    #[serde(default)]
    also: Vec<String>,
    #[serde(default)]
    max_diff_lines: Option<u32>,
    #[serde(default)]
    routing: Vec<RawRoutingRule>,
}

/// One `gates.merge.routing[]` entry (#1176). `deny_unknown_fields` like every
/// other `Raw*` type: a misspelled `path:`/`reviewer:` is a refusal, not a rule
/// that silently routes nothing.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawRoutingRule {
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    reviewers: Vec<String>,
}

/// `to: worker` and `to: [rev-a, rev-b]` are both legal — a fan-out reads
/// naturally as a list and a single hand-off reads naturally as a scalar.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn into_vec(self) -> Vec<String> {
        match self {
            OneOrMany::One(s) => vec![s],
            OneOrMany::Many(v) => v,
        }
    }
}

/// Every field name this schema accepts, per section, **derived from the types
/// that do the accepting** (#880). Section names match `src/workflow-schema.json`.
///
/// The GUI's failure mode this exists to kill: `allow:` has been a real
/// `RawBlock` field since #222 and the workflow pane never grew a control for
/// it — nor even knew the key — so a workflow that declared it looked, in the
/// pane, like a workflow that didn't. Nothing was wrong with either side; the
/// two simply had no way to disagree out loud. Now they do:
/// `tests/orchestration/` compares this against the committed manifest, so a
/// field added here without an editor is a red test rather than a hole nobody
/// finds until a human wonders where their line went.
///
/// **Serialized, not hand-listed**, deliberately: a hand-written list is
/// exactly the thing that drifts, and it would drift *silently* in the one
/// direction that matters (a new field, forgotten). serde already knows every
/// field name — `deny_unknown_fields` is that same knowledge pointed the other
/// way — so asking it is the only spelling of this that cannot go stale.
///
/// Every value below is populated rather than left at its zero — no `Option` is
/// `None`, no collection is empty — so that the key sets do not depend on what
/// the instances happen to hold. **What actually guarantees that is the parity
/// test, not the population**: there is no `skip_serializing_if` on any of these
/// types today, and one added tomorrow would turn that test RED (the manifest
/// still declares the row) rather than quietly shrinking this map. Maximal
/// values make the failure *impossible to reach by accident*; the test is what
/// makes it impossible to reach at all.
///
/// `#[doc(hidden)]` and `pub` only because the pin lives in an integration test
/// (CLAUDE.md constraint 4) and the `Raw*` types themselves stay private: this
/// is a test seam, not API.
#[doc(hidden)]
pub fn workflow_schema_keys() -> BTreeMap<String, Vec<String>> {
    fn keys_of<T: Serialize>(section: &str, v: &T) -> Vec<String> {
        match serde_json::to_value(v) {
            Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
            other => panic!("{section}: expected a mapping, got {other:?}"),
        }
    }
    let block = RawBlock {
        id: "b".into(),
        name: "B".into(),
        kind: "worker".into(),
        cli: "claude".into(),
        model: "opus".into(),
        prompt: Some("p".into()),
        profile: Some(".github/agents/b.md".into()),
        allow: vec!["Bash(gh pr view)".into()],
        role_hint: Some("process".into()),
        effort: "high".into(),
        context: "1m".into(),
        remote: Some("buildbox".into()),
        driver: Some("structured".into()),
        cache_ttl_minutes: Some(60),
    };
    let edge = RawEdge { from: "a".into(), to: OneOrMany::One("b".into()) };
    let gate = RawGate {
        require: Some("all-pass".into()),
        threshold: Some(1),
        reviewers: vec!["rev".into()],
        also: vec!["ci-green".into()],
        max_diff_lines: Some(800),
        routing: vec![RawRoutingRule {
            paths: vec!["src/**".into()],
            reviewers: vec!["rev".into()],
        }],
    };
    let labels = RawIntakeLabels {
        ready: "agent-ready".into(),
        investigate: "agent-investigation".into(),
        owned: "agent-managed".into(),
        prototype: "agent-prototype".into(),
        hold: "agent-hold".into(),
    };
    let intake = RawIntake { source: "github-labels".into(), labels: RawIntakeLabels::default() };
    let merge_queue =
        RawMergeQueue { enabled: true, max_batch: Some(3), checks_timeout_minutes: Some(60) };
    let driver = RawDriver {
        enabled: true,
        max_review_rounds: Some(3),
        max_ci_attempts: Some(3),
        max_rebase_attempts: Some(1),
        lane_timeout_minutes: Some(60),
        fix_timeout_minutes: Some(60),
        drive_timeout_minutes: Some(240),
        plan_enabled: true,
        plan_review_minutes: Some(15),
        planner_timeout_minutes: Some(60),
        fix_nonblocking_rounds: Some(1),
        auto_drive_on_done: true,
    };
    let resource = RawResource { slots: Some(1), max_hold_minutes: Some(30) };
    let triage = RawTriage {
        enabled: true,
        provider: Some(crate::triage::PROVIDER_NONE.into()),
        kinds: vec![crate::triage::Kind::RunCompleted.as_str().into()],
        max_defer_minutes: Some(crate::triage::TRIAGE_MAX_DEFER_MINUTES_DEFAULT),
    };
    // Every field populated, per this function's docblock: a `None` here would
    // drop the key from the serialization and shrink the manifest silently.
    let wip = RawWip {
        queued: Some(8),
        in_progress: Some(4),
        review: Some(3),
        pr: Some(3),
        prototype: Some(2),
        human_testing: Some(2),
        blocked: Some(4),
    };
    let mut out = BTreeMap::new();
    out.insert("board.wip".to_string(), keys_of("board.wip", &wip));
    let board = RawBoard { wip: Some(wip), enforce: true };
    // Read before `workflow` takes ownership of its own copies below — the point
    // of populating them there is that no field of the top-level type is left at
    // a zero value.
    out.insert("block".to_string(), keys_of("block", &block));
    out.insert("edge".to_string(), keys_of("edge", &edge));
    out.insert("gate".to_string(), keys_of("gate", &gate));
    // #1176. Its own section for the same reason `intake.labels` has one: a
    // routing rule is a mapping with its own field set, and a section that only
    // said "gate.routing is a list" would leave `paths:`/`reviewers:` outside
    // every guarantee this manifest exists to give.
    out.insert(
        "gate.routing".to_string(),
        keys_of(
            "gate.routing",
            &RawRoutingRule { paths: vec!["src/**".into()], reviewers: vec!["rev".into()] },
        ),
    );
    out.insert("intake".to_string(), keys_of("intake", &intake));
    out.insert("intake.labels".to_string(), keys_of("intake.labels", &labels));
    out.insert("merge_queue".to_string(), keys_of("merge_queue", &merge_queue));
    out.insert("driver".to_string(), keys_of("driver", &driver));
    out.insert("triage".to_string(), keys_of("triage", &triage));
    out.insert("resource".to_string(), keys_of("resource", &resource));
    out.insert("board".to_string(), keys_of("board", &board));
    let workflow = RawWorkflow {
        version: SCHEMA_VERSION,
        name: "w".into(),
        authored_with: "loomux".into(),
        blocks: vec![block],
        edges: vec![edge],
        gates: BTreeMap::from([("merge".to_string(), gate)]),
        intake: Some(intake),
        merge_queue: Some(merge_queue),
        driver: Some(driver),
        resources: BTreeMap::from([("build".to_string(), resource)]),
        board: Some(board),
        triage: Some(triage),
    };
    out.insert("workflow".to_string(), keys_of("workflow", &workflow));
    out
}

/// What the engine says a field may CONTAIN — the other half of
/// [`workflow_schema_keys`] (#880 review finding 1), keyed `"section.field"`.
///
/// Names alone were never the whole manifest: `src/workflow-schema.json` also
/// carries each field's closed value set, its default and its bounds, and slice
/// C generates form controls from exactly those. A wrong enum row today is a
/// wrong `<select>` later — a `block.cli` picker with no option for "inherit the
/// group's CLI" cannot represent the state most blocks are actually in — so
/// these are pinned against the engine the same way the field names are.
///
/// **Derived wherever the engine has an accessor**: `SUPPORTED_CLIS`,
/// [`kind_names`], [`role_hint_names`], [`intake_source_names`],
/// [`builtin_intake_profile`], `MergeQueuePolicy::default()`,
/// `ResourcePolicy::default()`, and the `RESOURCE_*` / `NOTIFY_EXPIRES_*` /
/// `RESOURCES_MAX` constants. Wire defaults for the plain `#[serde(default)]`
/// fields are derived too — by deserializing a minimal document and serializing
/// it back, so serde states them rather than this function guessing.
///
/// **One set is hand-listed and it is named as such**: `gate.require`, whose
/// accepted spellings exist only as match arms in [`parse_workflow`] (there is
/// no accessor to ask). Adding an arm without adding it here leaves the manifest
/// stale with nothing red — the one hole left in this pin, stated out loud
/// rather than papered over.
#[doc(hidden)]
pub fn workflow_schema_field_facts() -> BTreeMap<String, serde_json::Value> {
    use serde_json::{json, Value};

    /// The values serde fills in for a field the document omits — asked of
    /// serde, not typed out here. `null` (an absent `Option`) is not a default:
    /// it means the key simply isn't there, which is a different statement from
    /// "it defaults to nothing".
    fn wire_defaults<T>(minimal: &str, required: &[&str]) -> Vec<(String, Value)>
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let parsed: T = serde_json::from_str(minimal)
            .unwrap_or_else(|e| panic!("the minimal document must deserialize: {e}"));
        match serde_json::to_value(&parsed) {
            Ok(Value::Object(map)) => map
                .into_iter()
                .filter(|(k, v)| !v.is_null() && !required.contains(&k.as_str()))
                .collect(),
            other => panic!("expected a mapping, got {other:?}"),
        }
    }

    let names = |csv: String| -> Vec<String> { csv.split(", ").map(str::to_string).collect() };
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    let mut fact = |key: &str, k: &str, v: Value| {
        let entry = out.entry(key.to_string()).or_insert_with(|| json!({}));
        entry[k] = v;
    };

    for (section, defaults) in [
        ("workflow", wire_defaults::<RawWorkflow>(r#"{"version":1}"#, &["version"])),
        ("block", wire_defaults::<RawBlock>(r#"{"id":"b","kind":"worker"}"#, &["id", "kind"])),
        ("gate", wire_defaults::<RawGate>("{}", &[])),
        ("gate.routing", wire_defaults::<RawRoutingRule>("{}", &[])),
        ("intake", wire_defaults::<RawIntake>("{}", &[])),
        ("intake.labels", wire_defaults::<RawIntakeLabels>("{}", &[])),
        ("merge_queue", wire_defaults::<RawMergeQueue>("{}", &[])),
        ("driver", wire_defaults::<RawDriver>("{}", &[])),
        ("triage", wire_defaults::<RawTriage>("{}", &[])),
        ("resource", wire_defaults::<RawResource>("{}", &[])),
        ("board", wire_defaults::<RawBoard>("{}", &[])),
        // Deliberately contributes nothing: every field of `RawWip` is an
        // `Option` with no wire default, because an omitted status is the
        // ABSENCE of a cap and not a cap with a default value. Listed anyway
        // so a field that ever does gain one is picked up here rather than
        // needing this loop remembered.
        ("board.wip", wire_defaults::<RawWip>("{}", &[])),
    ] {
        for (field, value) in defaults {
            // A nested section's own default is the section, not a value a form
            // ever renders — `intake.labels` is described by its own rows.
            if value.is_object() || (value.is_array() && section == "workflow") {
                continue;
            }
            fact(&format!("{section}.{field}"), "default", value);
        }
    }

    // Closed value sets, from the accessors that already state them for error
    // messages — so a sixth entry in SUPPORTED_CLIS reddens this rather than
    // leaving the manifest quietly stale.
    let mut cli_values = vec![String::new()]; // an empty `cli:` inherits the group's
    cli_values.extend(SUPPORTED_CLIS.iter().map(|c| c.to_string()));
    fact("block.cli", "values", json!(cli_values));
    fact("block.kind", "values", json!(names(kind_names())));
    fact("block.role_hint", "values", json!(names(role_hint_names())));
    fact("block.driver", "values", json!(DRIVER_MODES));
    let mut sources = vec![String::new()]; // `intake_source_from_str`: "" is the default source
    sources.extend(names(intake_source_names()));
    fact("intake.source", "values", json!(sources));
    // HAND-LISTED — see this function's docblock. Mirrors `parse_workflow`'s gate
    // match arms (`Some("all-pass") | Some("all")` and `Some("threshold")`).
    fact("gate.require", "values", json!(["all-pass", "all", "threshold"]));
    // #3304 S1. `provider:` is a closed set of ONE in this build, and it is
    // published here rather than hand-listed in the JSON alone for
    // `block.cli`'s reason: when S3 adds the second value, this line is what
    // makes the manifest — and any control generated from it — redden instead
    // of quietly offering a choice the engine refuses.
    fact("triage.provider", "values", json!([crate::triage::PROVIDER_NONE]));

    // Effective defaults: what the engine BEHAVES as when the key is omitted,
    // which is what a form shows as its placeholder. Distinct from the wire
    // defaults above — `merge_queue.max_batch` is `None` on the wire and 3 in
    // effect — and taken from the same `Default` impls the parse resolves against.
    fact("workflow.version", "default", json!(SCHEMA_VERSION));
    fact("gate.require", "default", json!("all-pass"));
    let intake = builtin_intake_profile();
    fact("intake.source", "default", json!(intake.source.as_str()));
    fact("intake.labels.ready", "default", json!(intake.ready));
    fact("intake.labels.investigate", "default", json!(intake.investigate));
    fact("intake.labels.owned", "default", json!(intake.owned));
    fact("intake.labels.prototype", "default", json!(intake.prototype));
    fact("intake.labels.hold", "default", json!(intake.hold));
    let mq = MergeQueuePolicy::default();
    fact("merge_queue.enabled", "default", json!(mq.enabled));
    fact("merge_queue.max_batch", "default", json!(mq.max_batch));
    fact("merge_queue.checks_timeout_minutes", "default", json!(mq.checks_timeout_minutes));
    let dv = DriverPolicy::default();
    fact("driver.enabled", "default", json!(dv.enabled));
    fact("driver.max_review_rounds", "default", json!(dv.max_review_rounds));
    fact("driver.max_ci_attempts", "default", json!(dv.max_ci_attempts));
    fact("driver.max_rebase_attempts", "default", json!(dv.max_rebase_attempts));
    fact("driver.lane_timeout_minutes", "default", json!(dv.lane_timeout_minutes));
    fact("driver.fix_timeout_minutes", "default", json!(dv.fix_timeout_minutes));
    fact("driver.drive_timeout_minutes", "default", json!(dv.drive_timeout_minutes));
    fact("driver.plan_enabled", "default", json!(dv.plan_enabled));
    fact("driver.plan_review_minutes", "default", json!(dv.plan_review_minutes));
    fact("driver.planner_timeout_minutes", "default", json!(dv.planner_timeout_minutes));
    fact("driver.fix_nonblocking_rounds", "default", json!(dv.fix_nonblocking_rounds));
    fact("driver.auto_drive_on_done", "default", json!(dv.auto_drive_on_done));
    let tr = TriagePolicy::default();
    fact("triage.enabled", "default", json!(tr.enabled));
    fact("triage.provider", "default", json!(tr.provider));
    fact("triage.max_defer_minutes", "default", json!(tr.max_defer_minutes));
    let res = ResourcePolicy::default();
    fact("resource.slots", "default", json!(res.slots));
    fact("resource.max_hold_minutes", "default", json!(res.max_hold_minutes));

    // Bounds. `min` with no `max` is a floor the parse refuses below and nothing
    // above; the refuse-vs-clamp half is pinned behaviorally by the test, because
    // it is a fact about what `parse_workflow` DOES, not one this file can assert.
    fact("gate.threshold", "min", json!(1));
    // #1174. A floor, no ceiling: "how big is too big" is this repo's call and
    // loomux has no business inventing an upper bound for it. `0` is refused
    // rather than read as "unlimited" — a gate clause that gates nothing is a
    // typo, and the way to mean "no limit" is to omit the key.
    fact("gate.max_diff_lines", "min", json!(1));
    fact("merge_queue.max_batch", "min", json!(1));
    fact("merge_queue.checks_timeout_minutes", "min", json!(NOTIFY_EXPIRES_MIN));
    fact("merge_queue.checks_timeout_minutes", "max", json!(NOTIFY_EXPIRES_MAX));
    // #1778 §2.3. The counters are closed ranges held TOWARD INVARIANT 9, and
    // out-of-range values are REFUSED (the `merge_queue.max_batch` posture) - a
    // repo file may run a tighter loop than the orchestrator template promises,
    // never a looser one.
    fact("driver.max_review_rounds", "min", json!(DRIVER_MAX_REVIEW_ROUNDS_MIN));
    fact("driver.max_review_rounds", "max", json!(DRIVER_MAX_REVIEW_ROUNDS_MAX));
    fact("driver.max_ci_attempts", "min", json!(DRIVER_MAX_CI_ATTEMPTS_MIN));
    fact("driver.max_ci_attempts", "max", json!(DRIVER_MAX_CI_ATTEMPTS_MAX));
    fact("driver.max_rebase_attempts", "min", json!(DRIVER_MAX_REBASE_ATTEMPTS_MIN));
    fact("driver.max_rebase_attempts", "max", json!(DRIVER_MAX_REBASE_ATTEMPTS_MAX));
    // Two of the three backstops ride the notify-TTL clamp itself
    // (`clamp_expires_minutes`); `drive_timeout_minutes` carries its own range,
    // published here so the manifest states each field's real range rather than
    // the family's for all three (#2110).
    fact("driver.lane_timeout_minutes", "min", json!(NOTIFY_EXPIRES_MIN));
    fact("driver.lane_timeout_minutes", "max", json!(NOTIFY_EXPIRES_MAX));
    fact("driver.fix_timeout_minutes", "min", json!(NOTIFY_EXPIRES_MIN));
    fact("driver.fix_timeout_minutes", "max", json!(NOTIFY_EXPIRES_MAX));
    fact("driver.drive_timeout_minutes", "min", json!(DRIVER_DRIVE_TIMEOUT_MIN));
    fact("driver.drive_timeout_minutes", "max", json!(DRIVER_DRIVE_TIMEOUT_MAX));
    fact("driver.plan_review_minutes", "min", json!(crate::plandrive::PLAN_REVIEW_MINUTES_MIN));
    fact("driver.fix_nonblocking_rounds", "min", json!(DRIVER_FIX_NONBLOCKING_ROUNDS_MIN));
    fact("driver.fix_nonblocking_rounds", "max", json!(DRIVER_FIX_NONBLOCKING_ROUNDS_MAX));
    fact("driver.plan_review_minutes", "max", json!(crate::plandrive::PLAN_REVIEW_MINUTES_MAX));
    fact(
        "driver.planner_timeout_minutes",
        "min",
        json!(crate::plandrive::PLANNER_TIMEOUT_MINUTES_MIN),
    );
    fact(
        "driver.planner_timeout_minutes",
        "max",
        json!(crate::plandrive::PLANNER_TIMEOUT_MINUTES_MAX),
    );
    // #3304 S1. REFUSED outside, not clamped — `merge_queue.max_batch`'s
    // posture, for the reason `TriagePolicy::max_defer_minutes` states: the
    // number says how long this repo is willing to lose sight of its own fleet.
    fact("triage.max_defer_minutes", "min", json!(crate::triage::TRIAGE_MAX_DEFER_MINUTES_MIN));
    fact("triage.max_defer_minutes", "max", json!(crate::triage::TRIAGE_MAX_DEFER_MINUTES_MAX));
    // #1457. A LENGTH bound on a string, so it is `maxLength` rather than `max`:
    // this manifest documents `max` as "highest accepted number", and a generated
    // text control needs a maxlength, not a numeric ceiling. Stated here because
    // `parse_workflow` really does enforce it — `check_segment` refuses above
    // `MAX_SEGMENT_LEN` — and a bound the engine enforces while the manifest is
    // silent is one a generated control would let a human exceed.
    fact("block.remote", "maxLength", json!(crate::pathseg::MAX_SEGMENT_LEN));
    // #3407. REFUSED above, not clamped — see `Block::cache_ttl_minutes`.
    fact("block.cache_ttl_minutes", "min", json!(0));
    fact("block.cache_ttl_minutes", "max", json!(crate::cacheage::CACHE_TTL_MINUTES_MAX));
    fact("resource.slots", "min", json!(1));
    fact("resource.slots", "max", json!(RESOURCE_SLOTS_MAX));
    fact("resource.max_hold_minutes", "min", json!(1));
    fact("resource.max_hold_minutes", "max", json!(RESOURCE_MAX_HOLD_MINUTES_MAX));
    // Cardinality: the sections with a cap on how many entries they may hold.
    fact("workflow.resources", "max_entries", json!(RESOURCES_MAX));
    // Every WIP cap has the same floor and deliberately no ceiling: a limit
    // above the board's own size degenerates to "no limit", which is what the
    // author asked for, so there is nothing for loomux to refuse (the posture
    // `merge_queue.max_batch` takes, and the opposite of `resources.slots`,
    // where the ceiling is a legibility claim about serialization). Derived
    // from the wire struct's own field list rather than re-typed here — the
    // eighth status must not arrive bound-less because this loop was written
    // out by hand.
    for status in workflow_schema_keys().get("board.wip").into_iter().flatten() {
        fact(&format!("board.wip.{status}"), "min", json!(WIP_LIMIT_MIN));
    }
    // #1176. Both are bounds on work the SHIM does on the merge path — every
    // rule against every changed file, in shell — not on what a form can render.
    fact("gate.routing", "max_entries", json!(ROUTING_RULES_MAX));
    fact("gate.routing.paths", "max_entries", json!(ROUTING_PATHS_MAX));

    out
}

// ── parse + validate ────────────────────────────────────────────────────────

/// Parse and validate a workflow document. Returns **every** problem found, not
/// just the first: the whole point of a pre-run validation pass is that the
/// human fixes their file in one pass rather than playing whack-a-mole at spawn
/// time (which is where Flowise, Langflow and Dify all leave you).
///
/// The body is one call per section (#3498 P8b), each validating its own part
/// of the document and pushing onto the ONE shared error list. The call order
/// below is therefore the order an author reads their errors in, and it is
/// pinned — strings and order — by `tests/parse_workflow_golden.rs`. Three
/// sections read an earlier one's result rather than the raw document: the
/// roster check, edges and gates all work from the blocks that SURVIVED
/// validation, so a refused block is reported where it is declared, and an
/// edge or gate naming it is then reported as naming no block.
pub fn parse_workflow(text: &str) -> Result<Workflow, Vec<String>> {
    let raw: RawWorkflow = serde_norway::from_str(text).map_err(|e| vec![e.to_string()])?;
    let mut errs: Vec<String> = Vec::new();

    check_version(raw.version, &mut errs);
    let blocks = parse_blocks(&raw.blocks, &mut errs);
    check_roster(&blocks, &mut errs);
    let edges = parse_edges(raw.edges, &blocks, &mut errs);
    let gates = parse_gates(raw.gates, &blocks, &mut errs);
    let intake = parse_intake(raw.intake.as_ref(), &mut errs);
    let merge_queue = parse_merge_queue(raw.merge_queue.as_ref(), &mut errs);
    let driver = parse_driver_policy(raw.driver.as_ref(), &mut errs);
    let resources = parse_resources(&raw.resources, &mut errs);
    let board = parse_board(raw.board.as_ref(), &mut errs);
    let triage = parse_triage(raw.triage.as_ref(), &mut errs);

    if !errs.is_empty() {
        return Err(errs);
    }
    Ok(Workflow {
        version: raw.version,
        name: sanitize_display(&raw.name),
        authored_with: sanitize_display(&raw.authored_with),
        blocks,
        edges,
        gates,
        intake,
        merge_queue,
        driver,
        resources,
        board,
        triage,
    })
}

/// `version:` — the one schema version this build understands. A mismatch is
/// reported and validation carries on, so the author sees the rest too.
fn check_version(version: u32, errs: &mut Vec<String>) {
    if version != SCHEMA_VERSION {
        errs.push(format!(
            "version {} is not supported (this build understands version {SCHEMA_VERSION})",
            version
        ));
    }
}

// ── blocks ──────────────────────────────────────────────────────────────────

/// `blocks:` — each entry validated on its own by [`parse_block`], which stops
/// at a block's FIRST refusal: one error per bad block, and the next block is
/// still read. `seen` spans the whole list, so a duplicate id is caught across
/// entries.
fn parse_blocks(raw: &[RawBlock], errs: &mut Vec<String>) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (i, rb) in raw.iter().enumerate() {
        match parse_block(i, rb, &mut seen) {
            Ok(block) => blocks.push(block),
            Err(e) => errs.push(e),
        }
    }
    blocks
}

/// One `blocks[i]` entry, validated key by key in a fixed order; the first
/// refusal is the block's error. The order is observable — a block with two
/// problems reports the earlier one — so the steps below are never reordered
/// by a restructure.
fn parse_block(i: usize, rb: &RawBlock, seen: &mut BTreeSet<String>) -> Result<Block, String> {
    let id = block_id(i, rb, seen)?;
    let kind = block_kind(i, rb, &id)?;
    let cli = block_cli(i, &id, rb, kind)?;
    check_persona_source(i, &id, rb)?;
    check_loomux_owned_persona(i, &id, rb, kind)?;
    check_allow_closure(i, &id, rb, kind)?;
    let role_hint = block_role_hint(i, &id, rb, kind)?;
    // `effort:` / `context:` (#687). Both are VALUE-SET picks — they author
    // no text and pre-approve no tool — so the capability-closure argument
    // is unchanged and they are legal on an orchestrator block too (see
    // that check above, and `docs/design/workflows.md`). `validate_knob`
    // carries the whole rule; the CLI half is checked only for an explicit
    // `cli:`, exactly like `cli_can_host` above.
    let caps = (!cli.is_empty()).then(|| crate::model::cli_caps(&cli)).flatten();
    let effort = validate_knob(
        "effort",
        &rb.effort,
        crate::model::EFFORT_LEVELS,
        &cli,
        caps.map(|c| (c.effort_levels, c.effort_note)),
        |c| c.effort_levels,
    )
    .map_err(|e| format!("blocks[{i}] ({id}): {e}"))?;
    let context = validate_knob(
        "context",
        &rb.context,
        crate::model::CONTEXT_VARIANTS,
        &cli,
        caps.map(|c| (c.context_variants, c.context_note)),
        |c| c.context_variants,
    )
    .map_err(|e| format!("blocks[{i}] ({id}): {e}"))?;
    let driver = block_driver(i, &id, rb, &cli, caps)?;
    let remote = block_remote(i, &id, rb, kind, &cli)?;
    check_cache_ttl(i, &id, rb)?;
    let name = sanitize_display(&rb.name);
    Ok(Block {
        name: if name.is_empty() { id.clone() } else { name },
        id,
        kind,
        cli,
        model: crate::model::sanitize_model_opt(&rb.model),
        prompt: rb.prompt.as_deref().map(sanitize_persona).filter(|s| !s.trim().is_empty()),
        profile: rb.profile.as_ref().map(|p| p.trim().to_string()),
        allow: rb.allow.iter().filter_map(|a| crate::profiles::sanitize_allow(a)).collect(),
        role_hint,
        effort,
        context,
        remote,
        driver,
        cache_ttl_minutes: rb.cache_ttl_minutes,
    })
}

/// A block's id: the length bound, the character rule, then uniqueness. The id
/// is recorded in `seen` here, BEFORE its kind is checked, so a later block
/// reusing the id of one refused for its kind is still a duplicate.
fn block_id(i: usize, rb: &RawBlock, seen: &mut BTreeSet<String>) -> Result<String, String> {
    // An id is REJECTED rather than quietly rewritten: an author who wrote
    // `rev security` must not end up with a block called `revsecurity` that
    // their own edges and gates can no longer reference.
    if rb.id.trim().chars().count() > MAX_ID_CHARS {
        return Err(format!(
            "blocks[{i}]: id {:?} is longer than {MAX_ID_CHARS} characters",
            rb.id
        ));
    }
    let Some(id) = sanitize_id(&rb.id) else {
        return Err(format!("blocks[{i}]: id {:?} has no usable characters (allowed: letters, digits, '-', '_')", rb.id));
    };
    if id != rb.id.trim() {
        return Err(format!(
            "blocks[{i}]: id {:?} contains characters that are not allowed (letters, digits, '-', '_')",
            rb.id
        ));
    }
    if !seen.insert(id.clone()) {
        return Err(format!("blocks[{i}]: duplicate block id {id:?}"));
    }
    Ok(id)
}

/// A block's capability class, and the rule that a class NAME used as an id
/// belongs to that class.
fn block_kind(i: usize, rb: &RawBlock, id: &str) -> Result<Role, String> {
    // The capability class. An unknown kind is REJECTED, never coerced —
    // see `kind_from_str`.
    let Some(kind) = kind_from_str(&rb.kind) else {
        return Err(format!(
            "blocks[{i}] ({id}): unknown kind {:?} — must be one of {}",
            rb.kind,
            kind_names()
        ));
    };
    // The FIVE class names are RESERVED as ids for their own class (#1161
    // added `manager`). Without
    // this, `- id: planner, kind: reviewer` is accepted and then two blocks
    // collide: `instructions_file()` keys "is this a built-in?" off the id but
    // names the file from the kind, so that block would write `reviewer.md` —
    // the real reviewer block's contract file — and whichever spawned last
    // would win. (`- id: orchestrator, kind: worker` breaks a second way: the
    // roster has no orchestrator *kind*, so `clamped()` synthesizes one with
    // the id `orchestrator`, and the duplicate id makes the repo's own block
    // permanently unreachable.) Coupling the two removes the whole class of
    // problem, and costs an author nothing: rename the block.
    //
    // **Widening `kind_from_str` widens THIS, and that is a breaking change
    // to already-written workflow files** (#1161): `- id: manager, kind:
    // worker` parsed clean before the class existed — the id was not a class
    // name, so it took the custom-id branch and wrote `manager.md` — and is
    // a parse error now, which fails the whole file and drops the repo back
    // to the built-in roster. Unavoidable once `manager` names a class (the
    // alternative is the `worker.md` collision above), and cheap to fix:
    // rename the block. `Guardrails::clamped` step 2 is the same rule
    // applied to an already-persisted `group.json`, where it drops the block
    // rather than the file.
    if let Some(reserved) = kind_from_str(id) {
        if reserved != kind {
            return Err(format!(
                "blocks[{i}]: id {id:?} is reserved for {} blocks — a block with kind {:?} needs a different id",
                reserved.as_str(),
                kind.as_str()
            ));
        }
    }
    Ok(kind)
}

/// A block's explicit `cli:` (trimmed; empty means "inherit the group
/// default"), checked for containment and then membership.
fn block_cli(i: usize, id: &str, rb: &RawBlock, kind: Role) -> Result<String, String> {
    let cli = rb.cli.trim().to_string();
    if !cli.is_empty() {
        // #267: the containment question comes FIRST, and deliberately so.
        // A CLI loomux has evaluated and recorded (`CLI_CAPS`) but cannot
        // let host this class deserves to be told why — "unknown cli" would
        // be both unhelpful and, for a CLI with a row, untrue. Membership
        // still catches everything this doesn't: `cli_can_host` returns
        // `Ok` for a CLI it has never heard of.
        //
        // Refused at LOAD time so a repo learns from its own workflow file
        // rather than from a spawn that fails hours later — the same reason
        // the CLI name itself is validated here as well as at spawn. Only
        // checked for an explicit `cli:`; an empty one inherits the group
        // default, which is not known here (the launcher picks it) and is
        // re-checked at spawn against the real value.
        if let Err(e) = cli_can_host(&cli, kind) {
            return Err(format!("blocks[{i}] ({id}): {e}"));
        }
        if !SUPPORTED_CLIS.contains(&cli.as_str()) {
            return Err(format!(
                "blocks[{i}] ({id}): unknown cli {cli:?} — supported: {}",
                SUPPORTED_CLIS.join(", ")
            ));
        }
    }
    Ok(cli)
}

/// `prompt:` and `profile:` are two spellings of one persona, so at most one
/// may be declared; a `profile:` path is shape-checked here.
fn check_persona_source(i: usize, id: &str, rb: &RawBlock) -> Result<(), String> {
    if rb.prompt.is_some() && rb.profile.is_some() {
        return Err(format!(
            "blocks[{i}] ({id}): set either prompt: (inline persona) or profile: (a persona file), not both"
        ));
    }
    if let Some(path) = rb.profile.as_deref() {
        // Validate the shape now; the file is read (and its absence
        // tolerated) at spawn, so a workflow stays usable on a checkout
        // where the persona file hasn't landed yet.
        if let Err(e) = resolve_profile_path(".", path) {
            return Err(format!("blocks[{i}] ({id}): {e}"));
        }
    }
    Ok(())
}

/// The orchestrator and manager blocks are loomux-owned: no repo-authored
/// persona and no pre-approved tools.
fn check_loomux_owned_persona(i: usize, id: &str, rb: &RawBlock, kind: Role) -> Result<(), String> {
    // THE ORCHESTRATOR BLOCK IS LOOMUX-OWNED. A repo may pin its `cli`,
    // `model`, `effort` and `context` (each sanitized/validated like
    // everywhere else) — but it may not author its persona or pre-approve
    // its tools.
    //
    // The pin list is exactly "picks from a value set loomux ships", which
    // is why #687's two knobs join it and `prompt:`/`profile:`/`allow:`
    // never can: a level from a closed enum authors no text and
    // pre-approves no tool, so it opens no injection seam into the trust
    // root — the most a hostile repo buys is an orchestrator that thinks
    // harder or holds more context, both of which the human is shown in
    // the launcher's roster preview before they opt in.
    //
    // This is not a capability question: the orchestrator already holds every
    // tool, so a repo-authored prompt grants it nothing *new*. It is a TRUST
    // question. The orchestrator is the group's trust root — it runs
    // unsupervised under `auto_ops`, in the repo root with no worktree,
    // holding the privileged MCP surface (`spawn_agent`, `kill_agent`,
    // `set_state`). Letting `.loomux/workflow.yml` write its system prompt
    // would hand a cloned repo a direct prompt-injection seam into that root
    // (the #189 class) — and it would be the one orchestrator path with no
    // gate, in a feature whose entire security argument is that a repo file
    // never reconfigures trust. The rest of the model spends real effort
    // making a *second* orchestrator impossible; leaving the *first* one's
    // persona repo-writable would make that effort decorative.
    //
    // The declared feature ("five reviewers, five prompts") needs none of
    // this. If app-level orchestrator customization is ever wanted, it can
    // arrive as an explicit human opt-in — which is a different thing from a
    // file that arrives with a `git clone`.
    //
    // **THE MANAGER BLOCK IS LOOMUX-OWNED FOR THE SAME REASON** (#1161,
    // decision D1 — human-blessed). The capability-closure table makes
    // persona text inert for most classes: a repo can say anything it likes
    // to a reviewer and the reviewer still cannot merge. That argument does
    // not transfer here, because the manager's entire output surface IS
    // persuasion — of the human in its pane, and of the orchestrator via
    // relayed directives that the trust root then acts on as if the human
    // had said them. A repo-authored persona there is a directive-laundering
    // seam of the #189 class arriving with a `git clone`, and it is the one
    // seam no capability table can close.
    //
    // A repo loses nothing it was promised: the elicitation method itself
    // (spec-driven, "grill-me") ships in loomux's own `manager.md`. Pinning
    // `cli:`/`model:`/`effort:`/`context:`/`name:` stays legal, on the same
    // "picks from a value set loomux ships" line drawn above. Relaxing this
    // later as an explicit human opt-in is cheap; tightening it later would
    // be a breaking change to every workflow file already written.
    if kind == Role::Orchestrator || kind == Role::Manager {
        let offenders: Vec<&str> = [
            rb.prompt.is_some().then_some("prompt:"),
            rb.profile.is_some().then_some("profile:"),
            (!rb.allow.is_empty()).then_some("allow:"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !offenders.is_empty() {
            let why = if kind == Role::Orchestrator {
                "the orchestrator is loomux's trust root and a repo file may not author its \
                 prompt or pre-approve its tools"
            } else {
                "a manager speaks to the human and relays their direction into the trust root, \
                 so a repo file authoring its persona could launder its own instructions into \
                 what the human is told and what the orchestrator is asked to do"
            };
            return Err(format!(
                "blocks[{i}] ({id}): a{n} {k} block may not declare {offenders} — {why}. Pin \
                 its cli:/model:/effort:/context: if you need to; put personas on the blocks \
                 the orchestrator spawns.",
                n = if kind == Role::Orchestrator { "n" } else { "" },
                k = kind.as_str(),
                offenders = offenders.join(" / "),
            ));
        }
    }
    Ok(())
}

/// Capability closure: a read-only block may not declare `allow:` at all.
fn check_allow_closure(i: usize, id: &str, rb: &RawBlock, kind: Role) -> Result<(), String> {
    // CAPABILITY CLOSURE. `allow:` pre-approves tool patterns, and the
    // read-only class is read-only by *denial of a fixed list* — Edit, Write,
    // NotebookEdit, `git commit`, `git push` (CLAUDE_EDIT_DENY_TOOLS +
    // CLAUDE_READONLY_DENY_GIT — #448 dropped `MultiEdit`, which matches no
    // real Claude Code tool).
    // Deny beats allow on both CLIs, so an allow pattern cannot re-grant anything on that list…
    // but it does not have to. `allow: Bash(python *)` (or `cp`, `tee`,
    // `sed -i`, …) hands a planner a shell that writes files and is named
    // nowhere in the deny list, and under `auto_ops` nobody approves the call.
    //
    // Enumerating every write-capable program is not a thing anyone can do.
    // So the rule is the other way round: **a read-only block may not declare
    // `allow:` at all.** That keeps "a workflow file can never grant a
    // capability" a statement about the code rather than about the deny list's
    // completeness.
    //
    // The ban stays keyed to `is_read_only()` — the FULLY read-only class —
    // and deliberately did not follow #462's deny flags onto reviewers. The
    // argument above does not apply to a reviewer: it keeps its shell by
    // design (running the tests is the job), so an `allow:` pattern names
    // nothing it could not already run, and the editing tools #462 denies it
    // cannot be re-granted anyway (deny beats allow on both CLIs). Banning
    // `allow:` there would cost real expressiveness — a reviewer block that
    // pre-approves `Bash(npm test *)` — and buy nothing. A worker holds the
    // whole surface outright, same conclusion.
    if !rb.allow.is_empty() && kind.is_read_only() {
        return Err(format!(
            "blocks[{i}] ({id}): a {} block cannot declare allow: — its class is read-only, \
             and a pre-approved tool pattern could hand it a shell that writes files. \
             Move the work to a worker block.",
            kind.as_str()
        ));
    }
    Ok(())
}

/// `role_hint:` — a persona/template marker that may only sit on the kind it
/// is meaningless without. Trimmed and lowercased; `None` when absent.
fn block_role_hint(i: usize, id: &str, rb: &RawBlock, kind: Role) -> Result<Option<String>, String> {
    // role_hint (#250/#324) is a persona/template MARKER, never a
    // capability class of its own — it selects which addendum/template
    // fragment/badge a block gets, and `resolve_persona` keys off `kind`
    // alone. `mcp::tool_defs` and, since #946 Q4 / #1091 slice H, the
    // Claude CLI's `AskUserQuestion` deny (`claude_denies_interactive_
    // question`) DO additionally key off this field for the single
    // `liaison` hint — in BOTH directions: `tool_defs` GRANTS a plain
    // `kind: reviewer` block `group_usage`/`ask_human` once it also
    // carries `liaison` (mcp.rs, `tool_defs`'s liaison arm — a deliberate
    // widening of that block's tool surface, not a deny), while the
    // AskUserQuestion deny ADDS a restriction the same hint does not
    // otherwise carry. What neither direction ever touches is
    // `Role::containment()` — the edit/git denial tier `kind` alone
    // sets — so a liaison's containment is exactly a plain reviewer's
    // (`NoEdits`: the CLI's editing tools denied, the shell intact),
    // whatever its hint grants or denies elsewhere. What IS enforced
    // here is that a hint can only sit on the kind it is
    // meaningless without: an unrecognized value, or one paired with the
    // wrong kind, is a loud parse error — never coerced, never silently
    // dropped, the same shape `kind_from_str` itself enforces.
    let Some(raw) = rb.role_hint.as_deref() else {
        return Ok(None);
    };
    let hint = raw.trim().to_ascii_lowercase();
    let Some(required) = role_hint_requires(&hint) else {
        return Err(format!(
            "blocks[{i}] ({id}): unknown role_hint {raw:?} — must be one of {}",
            role_hint_names()
        ));
    };
    if required != kind {
        return Err(format!(
            "blocks[{i}] ({id}): role_hint {hint:?} requires kind: {} (this block is kind: {})",
            required.as_str(),
            kind.as_str()
        ));
    }
    Ok(Some(hint))
}

/// `driver:` on a block — HOW loomux drives its agent. `caps` is the explicit
/// `cli:`'s capability row, `None` when the cli is inherited.
fn block_driver(
    i: usize,
    id: &str,
    rb: &RawBlock,
    cli: &str,
    caps: Option<&crate::model::CliCaps>,
) -> Result<Option<String>, String> {
    // `driver:` (#2850) — HOW loomux drives this block's agent. A
    // VALUE-SET pick like `effort:`/`context:` above — it authors no text
    // and pre-approves no tool — so it is legal on an orchestrator block
    // too. Two checks, in the same order and the same postures as the
    // knobs: the value must be in loomux's closed vocabulary (rejected,
    // never coerced), and — when the block names an explicit `cli:` —
    // that CLI's [`CliCaps::structured_driver`] row must carry a driver.
    // The CLI half is checked only for an explicit `cli:`, exactly like
    // `cli_can_host` above: an inherited CLI is resolved at launch,
    // unknowable here, and re-checked at spawn.
    //
    // The refusal names the block (this prefix), the CLI, and the CLIs
    // that CAN take the key — derived from the table, the same remedy
    // shape `validate_knob` gives the knobs.
    let Some(raw) = rb.driver.as_deref() else {
        return Ok(None);
    };
    let want = raw.trim().to_ascii_lowercase();
    if want.is_empty() {
        // A bare `driver:` line means the absent key — a PTY
        // pane — rather than a refusal about nothing (the
        // reasoning `RawBlock::remote` states for `Option`).
        return Ok(None);
    }
    if !DRIVER_MODES.contains(&want.as_str()) {
        return Err(format!(
            "blocks[{i}] ({id}): unknown driver {raw:?} — must be one of {}",
            driver_mode_names()
        ));
    }
    // Only for an EXPLICIT `cli:` — `caps` is `None` for an
    // inherited one, which is unknowable here. That case is
    // not merely deferred, it is genuinely UNREACHABLE from
    // this function, and `structured_harness_for` is asked
    // again at spawn against the cli `cli_of` resolves (the
    // second ask #2850 S3b lands).
    if caps.is_some() {
        if let Err(refusal) = structured_harness_for(Some(want.as_str()), cli) {
            return Err(format!("blocks[{i}] ({id}): {refusal}"));
        }
    }
    Ok(Some(want))
}

/// `remote:` — the label saying this block's agent runs on another machine.
fn block_remote(i: usize, id: &str, rb: &RawBlock, kind: Role, cli: &str) -> Result<Option<String>, String> {
    // `remote:` (#1457) — the label that says this block's agent CLI runs
    // on another machine over SSH. THREE refusals, all parse errors, all
    // fail-closed on purpose: this is the one block key whose eventual
    // effect is "run code somewhere else", so every question it raises is
    // answered in the file or the file does not load.
    //
    // 1. THE LABEL ITSELF is checked with `pathseg::check_segment` — the
    //    #925 shared validator `GroupId` delegates to — rather than a
    //    fourth private "is this a safe id" predicate. Refused, never
    //    rewritten: `sanitize_id` would turn `../buildbox` into
    //    `buildbox`, and two strings naming one binding is exactly the
    //    hazard that consolidation exists to prevent.
    //
    // 2. NOT ON AN ORCHESTRATOR OR A MANAGER BLOCK. Both are loomux-owned
    //    (the same pair the persona check above refuses `prompt:`/
    //    `profile:`/`allow:` on) and both are load-bearing LOCALLY: the
    //    orchestrator is the trust root that holds orchestration state,
    //    the `gh` operations and the merge gate, and the manager is the
    //    human's own interface pane — the thing they type into. Moving
    //    either onto a machine the repo file named is not a feature with a
    //    missing implementation; it is the feature this design refuses.
    //
    // 3. `cli: claude` MUST BE SPELLED OUT. claude is the only CLI loomux
    //    drives remotely, and the gate is that fact rather than a
    //    capability claim. Session identity is what made it TRUE
    //    originally: loomux pre-mints the id and claude accepts it
    //    (`--session-id`/`--resume`), while copilot/opencode/gemini
    //    identify a session by scanning a LOCAL store, which a remote
    //    CLI's store is not. pi (#2126) accepts a pre-minted id too, so
    //    that argument no longer separates claude from every other CLI —
    //    what still does is that no remote pi block has ever been
    //    exercised, and a gate is only as good as the reason it states.
    //    An `cli:` omitted inherits the group
    //    default — picked in the launcher, unknowable here — so a block
    //    that leaves it blank is refused rather than parsed into a promise
    //    the spawn would have to break. The asymmetry decides it: relaxing
    //    this later (accepting an inherited claude) is cheap, tightening it
    //    later would be a breaking change to every workflow file already
    //    written.
    let Some(raw) = rb.remote.as_deref() else {
        return Ok(None);
    };
    if let Err(e) = crate::pathseg::check_segment(raw) {
        return Err(format!(
            "blocks[{i}] ({id}): remote {raw:?} is not a usable label — {e}. A \
             remote label is an abstract name the OPERATOR binds to a host outside \
             this repo, never an address."
        ));
    }
    if kind == Role::Orchestrator || kind == Role::Manager {
        return Err(format!(
            "blocks[{i}] ({id}): a{n} {k} block may not declare remote: — it is \
             loomux-owned and runs on the human's own machine ({why}). Put remote: \
             on the blocks the orchestrator spawns.",
            n = if kind == Role::Orchestrator { "n" } else { "" },
            k = kind.as_str(),
            why = if kind == Role::Orchestrator {
                "the orchestrator is the trust root, and orchestration state, the \
                 gh operations and the merge gate stay local"
            } else {
                "a manager pane is the human's own interface — it is where they type"
            },
        ));
    }
    if cli != "claude" {
        return Err(format!(
            "blocks[{i}] ({id}): remote: requires cli: claude{spelled} — a remote \
             agent's session has to be identified by an id loomux minted before the \
             spawn, and claude is the only CLI loomux drives remotely today. pi \
             accepts a pre-minted id too (--session-id), but nothing has exercised \
             a remote pi block, so the gate stays claude-only rather than widening \
             on an unvalidated capability. Every other CLI recognizes a session by \
             scanning a local store, which a remote CLI's store is not.",
            spelled = if cli.is_empty() {
                ", spelled out on the block — an omitted cli: inherits the group \
                 default, which is picked at launch and cannot be checked here"
            } else {
                ""
            },
        ));
    }
    Ok(Some(raw.to_string()))
}

/// `cache_ttl_minutes:` — bounded above, never clamped.
fn check_cache_ttl(i: usize, id: &str, rb: &RawBlock) -> Result<(), String> {
    // `cache_ttl_minutes:` (#3407) — a bound, refused rather than clamped:
    // `0` is a legal answer ("unknown"), and no provider documents a cache
    // longer than a day, so a value above that is a typo, not a wish.
    if let Some(ttl) = rb.cache_ttl_minutes {
        if ttl > crate::cacheage::CACHE_TTL_MINUTES_MAX {
            return Err(format!(
                "blocks[{i}] ({id}): cache_ttl_minutes {ttl} is above the {} ceiling; no provider documents a prompt cache longer than a day (use 0 for unknown)",
                crate::cacheage::CACHE_TTL_MINUTES_MAX
            ));
        }
    }
    Ok(())
}

/// Roster-level rules over the blocks that survived validation.
fn check_roster(blocks: &[Block], errs: &mut Vec<String>) {
    // Reported only when nothing ELSE was: a file whose every block was
    // refused already says why, and "no blocks" on top would be noise.
    if blocks.is_empty() && errs.is_empty() {
        errs.push("no blocks declared — a workflow needs at least one block".into());
    }

    // At most one manager (#1161) — see [`MANAGER_MAX`] for why this is a
    // coherence rule and not a capacity one. Checked after the loop rather than
    // inside it because it is a property of the ROSTER, not of any one block:
    // the second declaration is not more wrong than the first, and naming both
    // is what lets an author see which two they wrote.
    let managers: Vec<&str> = blocks
        .iter()
        .filter(|b| b.kind == Role::Manager)
        .map(|b| b.id.as_str())
        .collect();
    if managers.len() > MANAGER_MAX {
        errs.push(format!(
            "blocks: {} manager blocks declared ({}) — a workflow may declare at most {MANAGER_MAX}. \
             The manager is the human's single interface to this group: two of them would each hold \
             half a conversation, and everything that says \"the manager\" downstream would have to \
             pick one silently. Keep one and give the others another kind.",
            managers.len(),
            managers.join(", "),
        ));
    }
}

// ── edges ───────────────────────────────────────────────────────────────────

/// `edges:` — every `from` and `to` must name a block that survived
/// validation. An unknown `from` is reported alone; otherwise each unknown
/// `to` is. An edge with any unknown end is dropped.
fn parse_edges(raw: Vec<RawEdge>, blocks: &[Block], errs: &mut Vec<String>) -> Vec<Edge> {
    let known: BTreeSet<&str> = blocks.iter().map(|b| b.id.as_str()).collect();

    let mut edges: Vec<Edge> = Vec::new();
    for (i, re) in raw.into_iter().enumerate() {
        let from = re.from.trim().to_string();
        let to = re.to.into_vec();
        if !known.contains(from.as_str()) {
            errs.push(format!("edges[{i}]: 'from' names no block: {from:?}"));
            continue;
        }
        let mut bad = false;
        for t in &to {
            if !known.contains(t.trim()) {
                errs.push(format!("edges[{i}]: 'to' names no block: {:?}", t.trim()));
                bad = true;
            }
        }
        if bad {
            continue;
        }
        edges.push(Edge { from, to: to.iter().map(|t| t.trim().to_string()).collect() });
    }
    edges
}

// ── gates ───────────────────────────────────────────────────────────────────

/// `gates:` — read in name order (the map's), each by [`parse_gate`]. A gate
/// with any refusal is dropped.
fn parse_gates(raw: BTreeMap<String, RawGate>, blocks: &[Block], errs: &mut Vec<String>) -> BTreeMap<String, Gate> {
    let mut gates: BTreeMap<String, Gate> = BTreeMap::new();
    for (name, rg) in raw {
        if let Some(gate) = parse_gate(&name, &rg, blocks, errs) {
            gates.insert(name, gate);
        }
    }
    gates
}

/// One gate. A bad `require:` ends the gate at once, before its reviewers are
/// read; every other refusal accumulates, in the order the steps below run,
/// and `bad` records that one happened.
fn parse_gate(name: &str, rg: &RawGate, blocks: &[Block], errs: &mut Vec<String>) -> Option<Gate> {
    let require = match gate_require(name, rg) {
        Ok(require) => require,
        Err(e) => {
            errs.push(e);
            return None;
        }
    };
    let mut bad = false;
    check_gate_reviewers(name, rg, require, blocks, errs, &mut bad);
    let also = gate_also(name, rg, errs, &mut bad);
    // #1174's small-batch clause. `0` is a parse error, not "unlimited":
    // the same rule `threshold` follows, and for the same reason — a bound
    // a repo wrote down must never be read as the absence of one. A
    // negative or fractional value never reaches here at all; serde refuses
    // the whole file at `Option<u32>`, which is exactly what `threshold: -1`
    // already does.
    if rg.max_diff_lines == Some(0) {
        errs.push(format!("gates.{name}: max_diff_lines must be a positive number — omit the key to declare no limit"));
        bad = true;
    }
    let routing = gate_routing(name, rg, require, blocks, errs, &mut bad);
    if bad {
        return None;
    }
    Some(Gate {
        require,
        reviewers: rg.reviewers.iter().map(|r| r.trim().to_string()).collect(),
        also,
        max_diff_lines: rg.max_diff_lines,
        routing,
    })
}

/// `require:` and `threshold:` read together.
fn gate_require(name: &str, rg: &RawGate) -> Result<GateRequire, String> {
    match (rg.require.as_deref().map(str::trim), rg.threshold) {
        // `threshold: N` alone implies a threshold gate; spelling `require:
        // threshold` as well is allowed but redundant.
        (Some("threshold") | None, Some(n)) if n > 0 => Ok(GateRequire::Threshold(n)),
        (Some("threshold") | None, Some(_)) => Err(format!("gates.{name}: threshold must be a positive number")),
        (Some("threshold"), None) => Err(format!(
            "gates.{name}: require: threshold needs a threshold: N to go with it"
        )),
        (Some("all-pass") | Some("all") | None, None) => Ok(GateRequire::AllPass),
        (Some("all-pass") | Some("all"), Some(_)) => Err(format!(
            "gates.{name}: require: all-pass takes no threshold — drop it, or use require: threshold"
        )),
        (Some(other), _) => Err(format!(
            "gates.{name}: unknown require {other:?} — use 'all-pass', or 'threshold' with threshold: N"
        )),
    }
}

/// The static `reviewers:` list: a set of reviewer blocks, non-empty, and at
/// least as long as a threshold asks for.
fn check_gate_reviewers(
    name: &str,
    rg: &RawGate,
    require: GateRequire,
    blocks: &[Block],
    errs: &mut Vec<String>,
    bad: &mut bool,
) {
    // A gate's reviewer list is a set, not a sequence: `evaluate_merge_gate`
    // (in `gate.rs`) walks it once per verdict lookup, so a name listed twice would
    // let that reviewer's single PASS count twice toward a `threshold: N`
    // gate — a gate-integrity gap, not a cosmetic one — and `gate_need`
    // would inflate the derived minimum the same way block-id duplicates
    // would. Rejected here, consistent with how a duplicate block id is
    // handled above, rather than silently deduped: a repo author who wrote
    // the same name twice most likely meant a different one, and silently
    // dropping the duplicate would hide that typo instead of surfacing it.
    let mut seen_reviewers: BTreeSet<String> = BTreeSet::new();
    for r in &rg.reviewers {
        let rname = r.trim();
        if !seen_reviewers.insert(rname.to_string()) {
            errs.push(format!(
                "gates.{name}: reviewer {rname:?} is named more than once — name each reviewer once"
            ));
            *bad = true;
            continue;
        }
        if let Some(e) = gate_reviewer_error(name, "reviewer", rname, blocks) {
            errs.push(e);
            *bad = true;
        }
    }
    if rg.reviewers.is_empty() {
        errs.push(format!("gates.{name}: no reviewers — a gate with no reviewers gates nothing"));
        *bad = true;
    }
    if let GateRequire::Threshold(n) = require {
        if n as usize > rg.reviewers.len() {
            errs.push(format!(
                "gates.{name}: threshold {n} exceeds the {} reviewer(s) named — it could never pass",
                rg.reviewers.len()
            ));
            *bad = true;
        }
    }
}

/// `also:` — extra gate conditions, each refused unless already clean.
fn gate_also(name: &str, rg: &RawGate, errs: &mut Vec<String>, bad: &mut bool) -> Vec<String> {
    // `also:` names extra gate conditions (`ci-green`, …). Sanitized HERE,
    // at the parse boundary, even though nothing consumes it yet: gate
    // enforcement lands in sub-PR 3, in the `gh` shim, and a shim is a shell
    // script. Whatever `parse_workflow` returns will be read there as already
    // clean — that is the contract every other field in this file already
    // honors, and the one moment to establish it is before a consumer exists
    // to assume it. Rejected, not rewritten: an author must be able to
    // reference the condition they actually wrote.
    let mut also: Vec<String> = Vec::new();
    for c in &rg.also {
        match sanitize_condition(c) {
            Some(clean) if clean == c.trim() => also.push(clean),
            _ => {
                errs.push(format!(
                    "gates.{name}: condition {c:?} is not a usable name (letters, digits, '-', '_', '.')"
                ));
                *bad = true;
            }
        }
    }
    also
}

/// `routing:` (#1176) — the gate-level refusals first, then each rule.
fn gate_routing(
    name: &str,
    rg: &RawGate,
    require: GateRequire,
    blocks: &[Block],
    errs: &mut Vec<String>,
    bad: &mut bool,
) -> Vec<RoutingRule> {
    // #1176's path-based routing. Every refusal below is LOUD — a rule
    // loomux could not read is never a rule it quietly drops, because the
    // whole point of a routing rule is to ADD a required reviewer, and the
    // failure mode of silently dropping one is a merge that skipped a lane
    // the repo asked for.
    let mut routing: Vec<RoutingRule> = Vec::new();
    if !rg.routing.is_empty() {
        // `threshold: N` counts votes over a FIXED list; routing makes the
        // list a function of the diff. Together they have no honest meaning:
        // adding a lane would also add a candidate that could supply one of
        // the N passes, so declaring a routing rule could make the gate
        // EASIER to satisfy — the one direction a gate must never move. The
        // refusal says what to do instead (#782) rather than picking a
        // reading and hoping the author meant it.
        if matches!(require, GateRequire::Threshold(_)) {
            errs.push(format!(
                "gates.{name}: routing: and require: threshold cannot both be declared — a \
                 threshold counts passes over a fixed reviewer list, and a routing rule makes \
                 that list depend on the diff, so together they would let an extra lane SUPPLY \
                 one of the required passes instead of adding one. Use require: all-pass with \
                 routing:, and let each rule name the lane its paths need."
            ));
            *bad = true;
        }
        if rg.routing.len() > ROUTING_RULES_MAX {
            errs.push(format!(
                "gates.{name}: {} routing rules — at most {ROUTING_RULES_MAX}. The shim \
                 evaluates every rule against every changed file on every merge; past this \
                 many lanes the block has stopped routing and started listing.",
                rg.routing.len()
            ));
            *bad = true;
        }
    }
    for (i, rr) in rg.routing.iter().enumerate() {
        // 1-based, matching the position an author counts to in their own
        // file — and the number every refusal downstream cites.
        let idx = i + 1;
        routing.push(routing_rule(name, idx, rr, blocks, errs, bad));
    }
    routing
}

/// One routing rule, `idx` 1-based: its shape, then its paths, then its
/// reviewers.
fn routing_rule(
    name: &str,
    idx: usize,
    rr: &RawRoutingRule,
    blocks: &[Block],
    errs: &mut Vec<String>,
    bad: &mut bool,
) -> RoutingRule {
    if rr.paths.is_empty() {
        errs.push(format!(
            "gates.{name}: routing rule {idx} declares no paths — a rule that matches \
             nothing can never require anybody. Omit the rule, or give it a path glob."
        ));
        *bad = true;
    }
    if rr.paths.len() > ROUTING_PATHS_MAX {
        errs.push(format!(
            "gates.{name}: routing rule {idx} declares {} paths — at most {ROUTING_PATHS_MAX}.",
            rr.paths.len()
        ));
        *bad = true;
    }
    if rr.reviewers.is_empty() {
        errs.push(format!(
            "gates.{name}: routing rule {idx} names no reviewers — a rule that requires \
             nobody is not a rule."
        ));
        *bad = true;
    }
    let paths = routing_rule_paths(name, idx, rr, errs, bad);
    let reviewers = routing_rule_reviewers(name, idx, rr, blocks, errs, bad);
    RoutingRule { paths, reviewers }
}

/// A routing rule's `paths:` — each glob refused unless already clean, and
/// each named once.
fn routing_rule_paths(
    name: &str,
    idx: usize,
    rr: &RawRoutingRule,
    errs: &mut Vec<String>,
    bad: &mut bool,
) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    let mut seen_paths: BTreeSet<String> = BTreeSet::new();
    for p in &rr.paths {
        // Rejected, never rewritten — the #225 contract. An author must
        // be able to reference the glob they actually wrote, and a glob
        // loomux silently narrowed is a lane loomux silently dropped.
        match sanitize_glob(p) {
            Some(clean) if clean == p.trim() => {
                if !seen_paths.insert(clean.clone()) {
                    errs.push(format!(
                        "gates.{name}: routing rule {idx} lists the path {p:?} more than \
                         once — name each glob once."
                    ));
                    *bad = true;
                    continue;
                }
                paths.push(clean);
            }
            _ => {
                errs.push(format!(
                    "gates.{name}: routing rule {idx}: {p:?} is not a usable path glob. \
                     Use letters, digits, '.', '_', '-', '/' and '*' — and write a file \
                     glob, not a directory: 'src/**', never 'src/', '/src/**' or a '..' \
                     segment (GitHub reports changed paths repo-relative, so those match \
                     nothing at all)."
                ));
                *bad = true;
            }
        }
    }
    paths
}

/// A routing rule's `reviewers:` — the static list's rules, per rule.
fn routing_rule_reviewers(
    name: &str,
    idx: usize,
    rr: &RawRoutingRule,
    blocks: &[Block],
    errs: &mut Vec<String>,
    bad: &mut bool,
) -> Vec<BlockId> {
    let ctx = format!("routing rule {idx} reviewer");
    let mut reviewers: Vec<BlockId> = Vec::new();
    let mut seen_routed: BTreeSet<String> = BTreeSet::new();
    for r in &rr.reviewers {
        let rname = r.trim();
        // Same set-not-sequence rule the static list follows, and for a
        // milder version of the same reason: a name written twice in one
        // rule is a typo for a second lane, not an emphasis.
        if !seen_routed.insert(rname.to_string()) {
            errs.push(format!(
                "gates.{name}: routing rule {idx} names reviewer {rname:?} more than once"
            ));
            *bad = true;
            continue;
        }
        if let Some(e) = gate_reviewer_error(name, &ctx, rname, blocks) {
            errs.push(e);
            *bad = true;
            continue;
        }
        reviewers.push(rname.to_string());
    }
    reviewers
}

// ── the policy sections ─────────────────────────────────────────────────────

/// Intake source + label vocabulary (#382 P1). `None` (no `intake:` block
/// at all) resolves straight to the built-in default; a declared block
/// resolves field by field, each label falling back to its built-in value
/// when omitted (`sanitize_intake_label`) so a repo can override one label
/// without repeating the rest.
fn parse_intake(raw: Option<&RawIntake>, errs: &mut Vec<String>) -> IntakeProfile {
    let default_intake = builtin_intake_profile();
    match raw {
        None => default_intake,
        Some(ri) => {
            let source = match intake_source_from_str(&ri.source) {
                Some(s) => s,
                None => {
                    errs.push(format!(
                        "intake.source: unknown source {:?} — must be one of {}",
                        ri.source,
                        intake_source_names()
                    ));
                    IntakeSource::GithubLabels
                }
            };
            IntakeProfile {
                source,
                ready: sanitize_intake_label("ready", &ri.labels.ready, &default_intake.ready, errs),
                investigate: sanitize_intake_label(
                    "investigate",
                    &ri.labels.investigate,
                    &default_intake.investigate,
                    errs,
                ),
                owned: sanitize_intake_label("owned", &ri.labels.owned, &default_intake.owned, errs),
                prototype: sanitize_intake_label(
                    "prototype",
                    &ri.labels.prototype,
                    &default_intake.prototype,
                    errs,
                ),
                hold: sanitize_intake_label("hold", &ri.labels.hold, &default_intake.hold, errs),
            }
        }
    }
}

/// Merge-queue policy (#581 §11.2). `None` (no `merge_queue:` block at all)
/// resolves to the default, which is **disabled** — an absent block means
/// the feature is off and behavior is byte-for-byte unchanged.
///
/// Two different postures on a bad value, both taken from the note:
///
/// - `max_batch: 0` is a hard **error**. §11.2 says a malformed block never
///   degrades to defaults, "because a queue running on silently-substituted
///   policy is a queue nobody can reason about"; and it matches how the
///   sibling `gates:` block treats a number that could never work
///   (`threshold: 0`, `threshold` above the reviewer count).
/// - `checks_timeout_minutes` is **clamped**, because the note says clamped
///   ("default 60, clamped like the notify TTLs") — and it is clamped by the
///   notify TTL clamp *itself*, not by a second copy of those bounds. It is
///   the same quantity: a bounded wait on a PR's checks. `None` (omitted)
///   through the same call is where the 60-minute default comes from.
fn parse_merge_queue(raw: Option<&RawMergeQueue>, errs: &mut Vec<String>) -> MergeQueuePolicy {
    match raw {
        None => MergeQueuePolicy::default(),
        Some(rq) => {
            let max_batch = match rq.max_batch {
                None => MERGE_QUEUE_MAX_BATCH_DEFAULT,
                Some(0) => {
                    errs.push(
                        "merge_queue.max_batch: must be at least 1 — a batch of no PRs could never land anything"
                            .to_string(),
                    );
                    MERGE_QUEUE_MAX_BATCH_DEFAULT
                }
                Some(n) => n,
            };
            MergeQueuePolicy {
                enabled: rq.enabled,
                max_batch,
                checks_timeout_minutes: clamp_expires_minutes(rq.checks_timeout_minutes),
            }
        }
    }
}

/// One INVARIANT-9 counter (#1778 §2.3): refused outside its closed range
/// the way `merge_queue.max_batch: 0` is refused, with the design's own
/// default standing in when the value was written and refused - an error
/// still has to produce a value, and the default is the one the author
/// is about to be told the file failed to declare.
fn driver_counter(
    field: &str,
    raw: Option<u32>,
    (min, max): (u32, u32),
    default: u32,
    why: &str,
    errs: &mut Vec<String>,
) -> u32 {
    match raw {
        None => default,
        Some(v) if (min..=max).contains(&v) => v,
        Some(v) => {
            errs.push(format!("{field}: must be {min}..={max} - {why} (got {v})"));
            default
        }
    }
}

/// Review-driver policy (#1778 §5.3). `None` (no `driver:` block at all)
/// resolves to the default, which is **disabled** - an absent block means
/// the feature is off and behavior is byte-for-byte unchanged.
///
/// Two different postures on a bad value, both taken from the note:
///
/// - the three INVARIANT-9 counters are hard **errors** outside their
///   closed ranges, the posture `merge_queue.max_batch: 0` takes - a
///   malformed block never degrades to defaults, and §2.3's reason is
///   sharper than §11.2's: a repo file may run a *tighter* loop than the
///   orchestrator template promises, never a looser one, because the driver
///   acts on the orchestrator's authority and a config file that raised the
///   bound would be loosening the orchestrator's own INVARIANT 9.
/// - the three backstops are **clamped**, because the note says "clamped
///   like the notify TTLs" - and by the notify TTL clamp *itself*, not by a
///   second copy of those bounds. Same quantity as
///   `merge_queue.checks_timeout_minutes`: a bounded wait on a fallible
///   signal. `drive_timeout_minutes` left that family in #2110: its default
///   is twelve hours, above the family's ceiling, so it carries its own
///   range and its own clamp (`clamp_drive_timeout_minutes`).
fn parse_driver_policy(raw: Option<&RawDriver>, errs: &mut Vec<String>) -> DriverPolicy {
    match raw {
        None => DriverPolicy::default(),
        Some(rd) => DriverPolicy {
            enabled: rd.enabled,
            max_review_rounds: driver_counter(
                "driver.max_review_rounds",
                rd.max_review_rounds,
                (DRIVER_MAX_REVIEW_ROUNDS_MIN, DRIVER_MAX_REVIEW_ROUNDS_MAX),
                DRIVER_MAX_REVIEW_ROUNDS_MAX,
                "the drive may not run a looser review loop than the orchestrator template's \
                 INVARIANT 9 promises",
                errs,
            ),
            max_ci_attempts: driver_counter(
                "driver.max_ci_attempts",
                rd.max_ci_attempts,
                (DRIVER_MAX_CI_ATTEMPTS_MIN, DRIVER_MAX_CI_ATTEMPTS_MAX),
                DRIVER_MAX_CI_ATTEMPTS_MAX,
                "the drive may not spend more CI attempts than INVARIANT 9 grants the \
                 orchestrator itself",
                errs,
            ),
            max_rebase_attempts: driver_counter(
                "driver.max_rebase_attempts",
                rd.max_rebase_attempts,
                (DRIVER_MAX_REBASE_ATTEMPTS_MIN, DRIVER_MAX_REBASE_ATTEMPTS_MAX),
                DRIVER_MAX_REBASE_ATTEMPTS_MAX,
                "INVARIANT 9 grants one rebase attempt, and a repo may tighten that to none - \
                 never loosen it to two",
                errs,
            ),
            lane_timeout_minutes: clamp_expires_minutes(rd.lane_timeout_minutes),
            fix_timeout_minutes: clamp_expires_minutes(rd.fix_timeout_minutes),
            drive_timeout_minutes: clamp_drive_timeout_minutes(rd.drive_timeout_minutes),
            plan_enabled: rd.plan_enabled,
            // Through `driver_counter`, the REFUSING helper, rather than
            // through the notify-TTL clamp the lane/fix backstops use. The two
            // fields' docs on [`DriverPolicy`] carry why.
            plan_review_minutes: driver_counter(
                "driver.plan_review_minutes",
                rd.plan_review_minutes,
                (
                    crate::plandrive::PLAN_REVIEW_MINUTES_MIN,
                    crate::plandrive::PLAN_REVIEW_MINUTES_MAX,
                ),
                crate::plandrive::PLAN_REVIEW_MINUTES_DEFAULT,
                "a plan-review window past two hours is a drive nobody is coming back to, and the window costs one orchestrator notice to announce",
                errs,
            ),
            planner_timeout_minutes: driver_counter(
                "driver.planner_timeout_minutes",
                rd.planner_timeout_minutes,
                (
                    crate::plandrive::PLANNER_TIMEOUT_MINUTES_MIN,
                    crate::plandrive::PLANNER_TIMEOUT_MINUTES_MAX,
                ),
                crate::plandrive::PLANNER_TIMEOUT_MINUTES_DEFAULT,
                "a planner reading a large issue legitimately spends a quarter of an hour before its first tool call, and three hours is the point past which it is not coming back at all",
                errs,
            ),
            fix_nonblocking_rounds: driver_counter(
                "driver.fix_nonblocking_rounds",
                rd.fix_nonblocking_rounds,
                (DRIVER_FIX_NONBLOCKING_ROUNDS_MIN, DRIVER_FIX_NONBLOCKING_ROUNDS_MAX),
                DRIVER_FIX_NONBLOCKING_ROUNDS_MIN,
                "every non-blocking round is also a review round, so it can never exceed the \
                 three INVARIANT 9 grants",
                errs,
            ),
            auto_drive_on_done: rd.auto_drive_on_done,
        },
    }
}

/// Named lock resources (#858). An absent block leaves this empty, which is
/// what makes the lock tools invisible to the group's agents.
///
/// Every bad value here is a hard ERROR, never a silent substitution — the
/// same posture `merge_queue.max_batch` takes and for the same reason: a
/// repo declaring `slots: 0` believes its builds are serialized, and
/// quietly handing it the default would leave that belief in place while
/// the behaviour changed underneath it. Names are REJECTED rather than
/// rewritten (the `blocks[].id` rule): an author who wrote `heavy build`
/// must not end up with a resource called `heavybuild` that the
/// `acquire_lock` call in their own worker brief cannot name.
fn parse_resources(raw: &BTreeMap<String, RawResource>, errs: &mut Vec<String>) -> BTreeMap<String, ResourcePolicy> {
    let mut resources: BTreeMap<String, ResourcePolicy> = BTreeMap::new();
    if raw.len() > RESOURCES_MAX {
        errs.push(format!(
            "resources: {} declared — at most {RESOURCES_MAX} are allowed (every name is listed \
             in the acquire_lock tool description every agent in the group reads)",
            raw.len()
        ));
    }
    for (raw_name, rr) in raw {
        match parse_resource(raw_name, rr) {
            Ok((name, policy)) => {
                resources.insert(name, policy);
            }
            Err(e) => errs.push(e),
        }
    }
    resources
}

/// One resource: its name, then `slots`, then `max_hold_minutes`; the first
/// refusal is the resource's error.
fn parse_resource(raw_name: &str, rr: &RawResource) -> Result<(String, ResourcePolicy), String> {
    let trimmed = raw_name.trim();
    if trimmed.chars().count() > MAX_ID_CHARS {
        return Err(format!(
            "resources: name {raw_name:?} is longer than {MAX_ID_CHARS} characters"
        ));
    }
    let Some(name) = sanitize_id(trimmed) else {
        return Err(format!(
            "resources: name {raw_name:?} has no usable characters (allowed: letters, digits, '-', '_')"
        ));
    };
    if name != trimmed {
        return Err(format!(
            "resources: name {raw_name:?} contains characters that are not allowed (letters, digits, '-', '_')"
        ));
    }
    let slots = match rr.slots {
        None => RESOURCE_SLOTS_DEFAULT,
        Some(0) => {
            return Err(format!(
                "resources.{name}.slots: must be at least 1 — a resource with no slots could \
                 never be acquired by anyone"
            ));
        }
        Some(n) if n > RESOURCE_SLOTS_MAX => {
            return Err(format!(
                "resources.{name}.slots: {n} is above the maximum of {RESOURCE_SLOTS_MAX} — \
                 past that a declaration serializes nothing, which is not what a `slots:` line means"
            ));
        }
        Some(n) => n,
    };
    let max_hold_minutes = match rr.max_hold_minutes {
        None => RESOURCE_MAX_HOLD_MINUTES_DEFAULT,
        Some(0) => {
            return Err(format!(
                "resources.{name}.max_hold_minutes: must be at least 1 — a hold that expires \
                 the moment it is granted serializes nothing"
            ));
        }
        Some(n) if n > RESOURCE_MAX_HOLD_MINUTES_MAX => {
            return Err(format!(
                "resources.{name}.max_hold_minutes: {n} is above the maximum of \
                 {RESOURCE_MAX_HOLD_MINUTES_MAX} — a hold on a scarce resource has to be \
                 bounded by something a working session outlives"
            ));
        }
        Some(n) => n,
    };
    Ok((name, ResourcePolicy { slots, max_hold_minutes }))
}

/// Board policy — per-status WIP limits (#1175). `None` (no `board:` block
/// at all) resolves to the default, which declares no limits: the feature
/// is off and behavior is byte-for-byte unchanged.
///
/// A bad value is a hard ERROR, never a silent substitution — the posture
/// `merge_queue.max_batch` and `resources.slots` take, for the reason §11.2
/// gives: a repo that wrote `review: 0` believes something about how its
/// board paces, and quietly handing it "no limit" would leave that belief
/// in place while the behaviour went the other way.
///
/// The declared caps are read back out THROUGH serde rather than by
/// matching on `RawWip`'s seven fields here. Hand-listing them a second
/// time is how the eighth status would arrive parsed-but-unenforced: the
/// struct is the one place the field set is written down, and this loop
/// reads whatever that struct accepted.
fn parse_board(raw: Option<&RawBoard>, errs: &mut Vec<String>) -> BoardPolicy {
    match raw {
        None => BoardPolicy::default(),
        Some(rb) => {
            let mut wip: BTreeMap<String, u32> = BTreeMap::new();
            if let Some(rw) = &rb.wip {
                let declared = match serde_json::to_value(rw) {
                    Ok(serde_json::Value::Object(map)) => map,
                    other => panic!("board.wip: expected a mapping, got {other:?}"),
                };
                for (status, value) in declared {
                    // `null` is the field the document simply never wrote —
                    // "no cap on this status", which is not a value to check.
                    let Some(n) = value.as_u64() else { continue };
                    if n < WIP_LIMIT_MIN as u64 {
                        errs.push(format!(
                            "board.wip.{status}: must be at least {WIP_LIMIT_MIN} — a cap of 0 is a \
                             stop, not a work-in-progress limit, and under `enforce` it would wedge \
                             the board rather than pace it"
                        ));
                        continue;
                    }
                    wip.insert(status, n as u32);
                }
            }
            BoardPolicy { wip, enforce: rb.enforce }
        }
    }
}

/// Delivery-triage policy (#3304 S1). `None` (no `triage:` block at all)
/// resolves to the default, which is **disabled** - an absent block means
/// the feature is off and behavior is byte-for-byte unchanged.
///
/// Every bad value here is a hard ERROR, never a silent substitution, and
/// the three refusals differ in what they are protecting:
///
/// - `provider` outside the accepted set names the slice that would add it.
///   An author who wrote `provider: typesafe` believes agent text is being
///   classified by a model; running the rule tier anyway would leave that
///   belief in place while the behaviour was something else entirely - and
///   the belief in question is about text LEAVING THE MACHINE, so it is the
///   one value where a silent substitution is a privacy claim.
/// - a `kinds` entry that is not one of `triage::Kind::ALL`'s wire
///   spellings is a repo that believes it narrowed the gate and did not.
/// - `max_defer_minutes` outside its closed range is refused rather than
///   clamped, on `merge_queue.max_batch`'s argument: the number says how
///   long the author is willing to lose sight of their own fleet.
fn parse_triage(raw: Option<&RawTriage>, errs: &mut Vec<String>) -> TriagePolicy {
    match raw {
        None => TriagePolicy::default(),
        Some(rt) => {
            let provider = rt
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .unwrap_or(crate::triage::PROVIDER_NONE)
                .to_string();
            if provider != crate::triage::PROVIDER_NONE {
                errs.push(format!(
                    "triage.provider: must be {:?} - this build ships the RULE tier only, \
                     and a classifier provider arrives in #3304 S3 (got {provider:?})",
                    crate::triage::PROVIDER_NONE,
                ));
            }
            let mut kinds: Vec<crate::triage::Kind> = Vec::new();
            for raw_kind in &rt.kinds {
                match crate::triage::Kind::parse(raw_kind.trim()) {
                    Some(k) if !kinds.contains(&k) => kinds.push(k),
                    // A repeat is the author saying the same thing twice, not
                    // an error about anything; the set is what is read.
                    Some(_) => {}
                    None => errs.push(format!(
                        "triage.kinds: {raw_kind:?} is not a delivery kind - the set is {}",
                        crate::triage::Kind::ALL
                            .iter()
                            .map(|k| k.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    )),
                }
            }
            TriagePolicy {
                enabled: rt.enabled,
                provider,
                kinds,
                max_defer_minutes: driver_counter(
                    "triage.max_defer_minutes",
                    rt.max_defer_minutes,
                    (
                        crate::triage::TRIAGE_MAX_DEFER_MINUTES_MIN,
                        crate::triage::TRIAGE_MAX_DEFER_MINUTES_MAX,
                    ),
                    crate::triage::TRIAGE_MAX_DEFER_MINUTES_DEFAULT,
                    "a notice held longer than four hours is a notice nobody is coming back \
                     to, and holding one for less than a minute saves no wake at all",
                    errs,
                ),
            }
        }
    }
}

/// Whether the repo declares a workflow at all, asked without parsing it.
///
/// Used where the *existence* of the file is the whole question: `create_group`
/// audits that it deliberately ignored one (the advanced-orchestrator toggle is
/// off, #222), and the launcher's preview distinguishes "this repo has no
/// workflow" from "it has one and it is broken".
pub fn workflow_file_exists(repo: &str) -> bool {
    workflow_file(repo).is_file()
}

/// Whether a block may carry a persona at all.
///
/// The orchestrator block is loomux-owned: a repo may pin its `cli`/`model`, never
/// author its persona or pre-approve its tools. `parse_workflow` rejects that
/// outright, and `orchestration::OrchRegistry::resolve_persona` (in `src-tauri`)
/// drops one that arrives from a hand-edited `group.json` — so the *only* honest
/// answer about an orchestrator block's persona is "there isn't one".
///
/// **The manager block is loomux-owned on the same terms** (#1161, decision
/// D1), so it answers `false` here too. The parse refusal is the visible half;
/// this is the half that holds when the parser is bypassed, and it has to,
/// because bypassing the parser is precisely the case a repo-authored persona
/// on the human's own interface would be worth attempting. See `parse_workflow`
/// for the argument.
///
/// Anything that merely *reports* on a block therefore has to ask this too, or it
/// advertises a persona the spawn will deny (rev-11's preview nit). One predicate,
/// so the report and the spawn cannot disagree.
///
/// **The lead block is loomux-owned on the same terms again** (#2519), and it
/// answers `false` through [`Role::is_fixture`]'s shared answer without ever
/// reaching this function with a real `Block` — [`kind_from_str`] has no `lead`
/// arm, so a repo cannot declare one to give a persona to in the first place. A
/// lead group runs the built-in roster and never a workflow file, which is the
/// consent argument in `docs/design/lead-pane.md`: no roster preview, so no
/// roster the human was shown and agreed to.
pub fn persona_allowed(block: &Block) -> bool {
    !block.kind.is_fixture()
}

/// Whether a roster carries anything a workflow file put there — a block outside
/// the built-in four, or a built-in one given a persona.
///
/// False for the synthesized default roster, and that is the point: it is the
/// single condition guarding every piece of workflow-aware text loomux emits (the
/// orchestrator's roster note, the workflow section of its instructions, a
/// delegate's block note). A group with no workflow reads exactly as it did
/// before blocks existed because this returns false and all of it collapses to
/// the empty string.
///
/// **A declared manager counts, whatever its id** (#1161), and that third
/// clause is load-bearing rather than defensive. `manager` is a reserved id
/// ([`BUILTIN_IDS`]) so that a manager block owns `manager.md` — which means
/// the obvious spelling of a declared manager, `- id: manager, kind: manager`,
/// answers `is_builtin()` true and (D1 forbidding it a persona) `has_persona()`
/// false. Without this clause a workflow whose only addition to the built-in
/// four is a manager would report as "nothing a workflow file put there", and
/// every workflow-aware surface — the orchestrator's roster note, its workflow
/// section — would collapse to empty on the one roster that most needs to name
/// what it added. The default path is untouched: [`builtin_roster`] synthesizes
/// no manager, so this clause cannot fire for a group with no workflow file.
pub fn roster_is_custom(blocks: &[Block]) -> bool {
    blocks.iter().any(|b| !b.is_builtin() || b.has_persona() || b.kind == Role::Manager)
}

/// Read + validate the repo's workflow file ([`workflow_path`]).
///
/// - `Ok(None)` — no file (the common case): the caller synthesizes
///   [`default_roster`] and behaves exactly like pre-#222 loomux.
/// - `Err(errors)` — the file exists but is broken. The caller **audits and
///   skips it**, falling back to the default roster. A workflow file must never
///   be able to block a spawn.
pub fn load_workflow(repo: &str) -> Result<Option<Workflow>, Vec<String>> {
    load_workflow_named(repo, &WorkflowName::default_name())
}

/// The model a block runs, resolving the empty ("inherit") case: the block's
/// own `model:`, else the kind's default for its effective CLI.
pub fn model_of<'a>(block: &'a Block, agent_cli: &'a str) -> &'a str {
    if block.model.trim().is_empty() {
        default_model(cli_of(block, agent_cli), block.kind)
    } else {
        &block.model
    }
}

/// The CLI a block runs: its own `cli:`, else the group default `agent_cli`.
pub fn cli_of<'a>(block: &'a Block, agent_cli: &'a str) -> &'a str {
    if block.cli.trim().is_empty() {
        agent_cli
    } else {
        &block.cli
    }
}

// ── schema field-inventory pin (#382 P1 rev-26 NB1) ─────────────────────────
//
// Pure logic, no windowing/PTY code touched — safe as an inline unit test
// (constraint 4: only tests that link the FULL LIB in a way that needs the
// comctl32-v6 manifest have to be integration tests; this needs neither the
// registry nor the manifest, see e.g. `winpath.rs`/`cliprobe.rs` for the same
// call).

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `parse_workflow` ERROR is one paragraph too — the twin population
    /// [`every_listing_finding_is_one_paragraph`] does not reach.
    ///
    /// That test pins `list_workflows`' own findings; these are a different set,
    /// produced by a different function, and reaching a human by a different
    /// route: `parse_workflow`'s `errs` surface as `WorkflowEntry::errors` in
    /// the picker and as the refusal a spawn gate reports. Nothing pinned their
    /// shape, and that gap is exactly how two new eighteen-space runs shipped
    /// green in #3040 P3a's `plan_review_minutes` and `planner_timeout_minutes`
    /// refusals — in a PR whose own body was, at the time, describing this bug
    /// class as something someone else should fix.
    ///
    /// Both shapes, for the reason the sibling gives: a `\n` plus indentation
    /// ships the source's leading spaces, and a continuation that collapsed
    /// leaves the same run with no `\n` to notice.
    #[test]
    fn every_parse_error_is_one_paragraph() {
        // One document that trips as many refusing checks as it can, so the
        // population is wide rather than the two keys this test was written for.
        let doc = "\
version: 1
name: broken
blocks:
  - id: w
    kind: worker
  - id: p
    kind: planner
gates:
  merge:
    require: all-pass
driver:
  enabled: true
  plan_enabled: true
  max_review_rounds: 9
  max_ci_attempts: 0
  max_rebase_attempts: 7
  plan_review_minutes: 500
  planner_timeout_minutes: 2
  fix_nonblocking_rounds: 7
";
        let errs = parse_workflow(doc).expect_err("this document must be refused");

        // The positive control, and it is load-bearing twice over: an empty
        // list satisfies the loop below just as well, and a list of two would
        // mean the document stopped tripping most of what it was built to trip.
        assert!(
            errs.len() >= 5,
            "the fixture must really produce a wide error set, or this pins almost nothing: {errs:?}"
        );
        // …and the two this test exists for are in it by NAME, so a fixture edit
        // that stopped reaching them fails here rather than passing vacuously.
        // #3367 B1: `fix_nonblocking_rounds` shipped a collapsed continuation
        // that this loop would have caught had the fixture reached the key.
        for key in [
            "driver.plan_review_minutes",
            "driver.planner_timeout_minutes",
            "driver.fix_nonblocking_rounds",
        ] {
            assert!(
                errs.iter().any(|e| e.starts_with(key)),
                "the fixture no longer reaches {key}: {errs:?}"
            );
        }

        for m in &errs {
            assert!(!m.contains('\n'), "a parse error must not carry a newline: {m:?}");
            assert!(
                !m.contains("          "),
                "a parse error must not carry a ten-space run — a `\\` continuation that \
                 collapsed leaves one with no newline to notice: {m:?}"
            );
        }
    }

    /// One block, with whatever keys the case under test needs.
    fn remote_doc(keys: &[(&str, &str)]) -> String {
        let mut s = String::from("version: 1\nblocks:\n  - id: b\n");
        for (k, v) in keys {
            s.push_str(&format!("    {k}: {v}\n"));
        }
        s
    }

    /// Labels the engine must REFUSE, and why. The pane mirrors this predicate
    /// (`isRemoteLabel`, src/workflowtypes.ts) and its twin test walks the same
    /// list — a mirror nobody compares is a mirror that has drifted.
    const BAD_LABELS: &[(&str, &str)] = &[
        ("\"\"", "empty"),
        ("\"build box\"", "a space"),
        ("build.box", "a dot — which is what makes '..' unspellable"),
        ("\"../buildbox\"", "a traversal, which sanitize_id would REWRITE to buildbox"),
        ("build/box", "a separator"),
        ("\"C:\"", "a Windows drive prefix — Path::join REPLACES the receiver on one"),
        ("\"-buildbox\"", "a leading dash — an OPTION to any command line"),
        ("CON", "a Windows reserved device name"),
    ];

    #[test]
    fn a_remote_label_is_refused_rather_than_rewritten() {
        // The label is validated with `pathseg::check_segment` — #925's shared
        // checks — and NOT with `sanitize_id`, which rewrites. That difference
        // is the whole test: a rewrite would let `../buildbox` and `buildbox`
        // name one operator binding, which is exactly the hazard the identifier
        // consolidation exists to prevent.
        for &(label, why) in BAD_LABELS {
            let errs = parse_workflow(&remote_doc(&[
                ("kind", "worker"),
                ("cli", "claude"),
                ("remote", label),
            ]))
            .unwrap_err();
            assert!(
                errs.iter().any(|e| e.contains("remote")),
                "{label} ({why}) must be refused by name: {errs:?}"
            );
        }
        // One character over the shared cap, built rather than listed so the
        // constant and the fixture cannot drift apart.
        let over = "b".repeat(crate::pathseg::MAX_SEGMENT_LEN + 1);
        let errs =
            parse_workflow(&remote_doc(&[("kind", "worker"), ("cli", "claude"), ("remote", over.as_str())]))
                .unwrap_err();
        assert!(errs.iter().any(|e| e.contains("remote")), "one over the cap: {errs:?}");

        // The positive controls — without which every assertion above would
        // pass just as well against a parser that refused every label there is.
        let at_cap = "b".repeat(crate::pathseg::MAX_SEGMENT_LEN);
        for label in ["buildbox", "build-box_2", at_cap.as_str()] {
            let wf = parse_workflow(&remote_doc(&[
                ("kind", "worker"),
                ("cli", "claude"),
                ("remote", label),
            ]))
            .unwrap_or_else(|e| panic!("{label:?} must parse: {e:?}"));
            // Read back BYTE FOR BYTE: nothing trims, lowercases or normalizes
            // it on the way in.
            assert_eq!(wf.block("b").unwrap().remote.as_deref(), Some(label));
        }

        // Absent is None — today's behavior, byte for byte, which is every
        // block in every workflow file written before this key existed.
        let wf = parse_workflow("version: 1\nblocks:\n  - id: w\n    kind: worker\n").unwrap();
        assert_eq!(wf.block("w").unwrap().remote, None);
    }

    #[test]
    fn a_bare_remote_key_is_not_a_remote_block() {
        // The pair the pane's YAML subset would otherwise collapse, and the
        // reason `readBlock` keeps a null apart from an empty string.
        //
        // A bare `remote:` line is YAML null, which serde reads into
        // `Option<String>` as None — indistinguishable from never writing the
        // key, so the block is LOCAL and the file loads. An explicit
        // `remote: ""` is `Some("")`, reaches `check_segment`, and is refused.
        // Pinned because `src/workflowparse.ts` mirrors exactly this
        // difference, and a mirror of an unpinned behaviour is a mirror of an
        // assumption.
        let wf = parse_workflow("version: 1\nblocks:\n  - id: b\n    kind: worker\n    remote:\n")
            .expect("a bare remote: is YAML null, which is the absent key");
        assert_eq!(wf.block("b").unwrap().remote, None);
        // …and it really is the ABSENT key, not a remote block that slipped
        // through: the cli: rule below would have refused it (no cli: is
        // spelled here), so a parse that reached the remote arm at all could
        // not have returned Ok.
        assert!(wf.block("b").unwrap().cli.is_empty());

        let errs = parse_workflow(
            "version: 1\nblocks:\n  - id: b\n    kind: worker\n    cli: claude\n    remote: \"\"\n",
        )
        .unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("remote") && e.contains("empty")),
            "an explicitly empty label is refused, and says why: {errs:?}"
        );
    }

    #[test]
    fn every_yaml_null_spelling_is_the_absent_key() {
        // The engine half of the pair `test/workflowvalidate.test.ts` pins from the
        // other side (#1457 review N2). The pane's YAML subset resolves only
        // `null` and `~`; this asserts what the ENGINE's reader does with the
        // rest of the core schema's null set, so the divergence is a measured
        // fact rather than an assumption about a library.
        //
        // Every spelling here must be the ABSENT key — a local block — because
        // that is what `Option<String>` means and what the refusals below are
        // written against. If a future YAML reader narrowed this set, a file
        // that reads as local today would start carrying an empty label into
        // `check_segment` and be refused, which is a silent break for anyone
        // who wrote one.
        for spelling in ["null", "Null", "NULL", "~"] {
            let doc = format!(
                "version: 1\nblocks:\n  - id: b\n    kind: worker\n    remote: {spelling}\n"
            );
            let wf = parse_workflow(&doc)
                .unwrap_or_else(|e| panic!("remote: {spelling} must read as the absent key: {e:?}"));
            assert_eq!(
                wf.block("b").unwrap().remote,
                None,
                "remote: {spelling} must be the absent key, not a label"
            );
        }
        // The non-vacuity control, and it is the one that makes the loop mean
        // something: a spelling OUTSIDE the null set really does arrive as a
        // label, so the loop above is not passing because everything is None.
        //
        // `nul` was the obvious pick and is the wrong one: `NUL` is a Windows
        // reserved device name, so `check_segment` refuses it whatever its case and
        // the control failed for a reason that has nothing to do with null
        // spellings. That refusal is the device-name rule working as intended;
        // `nullish` is a control that isolates the property under test.
        let wf = parse_workflow(
            "version: 1\nblocks:\n  - id: b\n    kind: worker\n    cli: claude\n    remote: nullish\n",
        )
        .expect("a label that merely looks like a null must parse");
        assert_eq!(wf.block("b").unwrap().remote.as_deref(), Some("nullish"));
    }

    /// A user-facing refusal is ONE paragraph, and `.contains(…)` cannot see
    /// when it stops being one.
    ///
    /// This is the house rule (CLAUDE.md, "A user-facing message is ONE
    /// paragraph"), and this PR is the worked example it names: the three
    /// refusals below were first authored through a path that collapsed each
    /// `\` line-continuation into the next line's indentation, shipping 20-30
    /// consecutive spaces mid-sentence to whoever's workflow file failed to
    /// load. Every assertion in this module is a `.contains(<substring>)`, and
    /// not one of them straddled a break — so the defect rode a fully green
    /// suite until a test happened to assert a phrase that spanned one.
    ///
    /// So the SHAPE is pinned beside the content, the same predicate
    /// `src-tauri/tests/manager_lifecycle.rs::is_one_paragraph` uses: no `\n`
    /// (a hard break) and no ten-space run (leaked source indentation, which is
    /// what a collapsed continuation leaves behind). Restated here rather than
    /// shared because that helper lives in the Tauri crate's test binary, which
    /// this crate cannot reach.
    #[test]
    fn every_remote_refusal_renders_as_one_paragraph() {
        fn one_paragraph(msg: &str) -> bool {
            !msg.contains('\n') && !msg.contains("          ")
        }

        // Every refusal this PR adds, each triggered by the smallest file that
        // produces it. Collected rather than asserted one at a time so a
        // refusal added later without a row here is a visible omission.
        let cases: [(&str, String); 5] = [
            ("bad label", remote_doc(&[("kind", "worker"), ("cli", "claude"), ("remote", "build box")])),
            ("orchestrator", remote_doc(&[("kind", "orchestrator"), ("cli", "claude"), ("remote", "buildbox")])),
            ("manager", remote_doc(&[("kind", "manager"), ("cli", "claude"), ("remote", "buildbox")])),
            ("wrong cli", remote_doc(&[("kind", "worker"), ("cli", "copilot"), ("remote", "buildbox")])),
            ("omitted cli", remote_doc(&[("kind", "worker"), ("remote", "buildbox")])),
        ];
        let mut checked = 0;
        for (what, doc) in &cases {
            let errs = parse_workflow(doc).unwrap_err();
            for e in &errs {
                assert!(
                    one_paragraph(e),
                    "{what}: a refusal is one paragraph — the house idiom is a `\\` line \
                     continuation, which strips the newline AND the indentation. This one ships \
                     a hard break or leaked indentation: {e:?}"
                );
                checked += 1;
            }
        }
        // The population control, and it guards a narrower vacuity than the
        // obvious one (#1457 review N6). An `Ok` return is NOT the case:
        // `unwrap_err()` panics on it, so that is red with or without this
        // assert. What it guards is `Err(vec![])` — `unwrap_err()` succeeds,
        // the inner loop never runs, and `checked` stays 0 while every
        // assertion above is vacuously satisfied. That route is closed today by
        // `parse_workflow` returning `Err` only under `if !errs.is_empty()`,
        // which is exactly why this stays: a future editor who notices
        // `unwrap_err` already panics on `Ok` would otherwise read this as
        // redundant and delete it.
        //
        // It also pins more than a floor: `== 5` is one refusal PER CASE, so a
        // case that starts producing two reddens here rather than passing.
        assert_eq!(checked, 5, "each case must produce exactly one refusal to check");
    }

    #[test]
    fn a_loomux_owned_block_may_not_run_remotely() {
        // The orchestrator holds orchestration state, the gh operations and the
        // merge gate; the manager pane is the human's own interface — the thing
        // they type into. Both are load-bearing LOCALLY, so this is not a
        // feature with a missing implementation, it is the one this design
        // refuses. The pair is the same one `persona_allowed` answers for.
        for kind in ["orchestrator", "manager"] {
            let errs = parse_workflow(&remote_doc(&[
                ("kind", kind),
                ("cli", "claude"),
                ("remote", "buildbox"),
            ]))
            .unwrap_err();
            assert!(
                errs.iter().any(|e| e.contains("remote:") && e.contains(kind)),
                "a {kind} block must be refused by name: {errs:?}"
            );
        }
        // The controls, and they are what keep this from being "no block may
        // run remotely": every class the orchestrator SPAWNS may.
        for kind in ["worker", "reviewer", "planner"] {
            let wf = parse_workflow(&remote_doc(&[
                ("kind", kind),
                ("cli", "claude"),
                ("remote", "buildbox"),
            ]))
            .unwrap_or_else(|e| panic!("a remote {kind} must parse: {e:?}"));
            assert_eq!(wf.block("b").unwrap().remote.as_deref(), Some("buildbox"));
        }
    }

    #[test]
    fn a_remote_block_must_spell_out_cli_claude() {
        // Session identity is what DECIDED this originally (plan #1436 part
        // 5): loomux pre-mints the session id and claude accepts it
        // (`--session-id` / `--resume`), while copilot/opencode/gemini
        // recognize a session by scanning a LOCAL store — which a remote CLI's
        // store is not.
        //
        // `pi` is in the loop for a DIFFERENT reason, and it is the reason the
        // refusal's wording had to change (#2126): pi does accept a pre-minted
        // id, so "claude is the only CLI that accepts one" became false the
        // day pi landed. The gate is unchanged — a remote pi block has never
        // been exercised and loomux drives no CLI but claude remotely — but a
        // refusal is only as good as the reason it gives, and a reason a
        // reader can falsify in one grep is worse than none. So pi is pinned
        // here as a REFUSED cli whose refusal must not claim it cannot carry
        // an id.
        // codex joined with #2515 C1 for the ORDINARY reason — it recognizes a
        // session by scanning a local store (`SessionBaseline::Codex`), which a
        // remote CLI's store is not — so it is the plainest member of this loop
        // and is here to keep the gate total as `SUPPORTED_CLIS` grows, not
        // because anything about it is special.
        for cli in ["codex", "copilot", "gemini", "opencode", "pi"] {
            let errs = parse_workflow(&remote_doc(&[
                ("kind", "worker"),
                ("cli", cli),
                ("remote", "buildbox"),
            ]))
            .unwrap_err();
            assert!(
                errs.iter().any(|e| e.contains("remote:") && e.contains("claude")),
                "a remote block on {cli} must be refused and name claude: {errs:?}"
            );
            // The refusal must not claim the refused CLI *cannot* be handed a
            // pre-minted id — for pi that is false (#2126), and a false reason
            // sends a reader to fix the wrong thing. Asserted for every CLI in
            // the loop, not just pi: the claim was wrong as a UNIVERSAL, so
            // the pin is the universal too.
            assert!(
                !errs.iter().any(|e| e.contains("the only CLI that accepts one")),
                "the refusal must not claim claude is the only CLI that accepts a pre-minted \
                 session id — pi accepts one; the gate is about what loomux drives remotely: \
                 {errs:?}"
            );
        }
        // An OMITTED cli: is refused too, and this is the fail-closed half: it
        // inherits the group default, which the launcher picks and this parser
        // cannot see. Refusing it makes the contract total — a remote block is
        // verifiable from the file alone.
        let errs =
            parse_workflow(&remote_doc(&[("kind", "worker"), ("remote", "buildbox")])).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("remote:") && e.contains("claude")),
            "an omitted cli: must be refused: {errs:?}"
        );
        // …and it must say WHICH of the two mistakes it is, because the fix
        // differs: change the CLI, or write the line you left out.
        assert!(
            errs.iter().any(|e| e.contains("inherits the group default")),
            "the omitted-cli refusal must not read like the wrong-cli one: {errs:?}"
        );

        // The control.
        let wf = parse_workflow(&remote_doc(&[
            ("kind", "worker"),
            ("cli", "claude"),
            ("remote", "buildbox"),
        ]))
        .expect("cli: claude with a remote label must parse");
        assert_eq!(wf.block("b").unwrap().remote.as_deref(), Some("buildbox"));
    }

    #[test]
    fn a_host_shaped_key_fails_the_whole_file() {
        // The repo file SELECTS a label; the OPERATOR authors the address. A
        // repo-authored host/port/identity_file would let whoever opens a PR
        // direct execution onto any machine the operator can reach — so none of
        // these is an unsupported field, each is an UNKNOWN one, and
        // `deny_unknown_fields` on `RawBlock` fails the file over it.
        //
        // The whole-file part is the safe-downgrade property (#1436 part 4) and
        // is why the legal sibling block is here: nothing partial comes back, so
        // a build that does not understand a key can never run a roster the file
        // did not describe. A build predating `remote:` refuses a file that
        // declares it for exactly the same reason.
        for key in [
            "host",
            "destination",
            "port",
            "user",
            "identity_file",
            "ssh_options",
            "proxy_command",
            "extra_args",
        ] {
            let doc = format!(
                "version: 1\nblocks:\n  - id: legal\n    kind: worker\n    cli: claude\n\
                 \x20 - id: b\n    kind: worker\n    cli: claude\n    {key}: example.com\n"
            );
            let errs = parse_workflow(&doc).unwrap_err();
            assert!(
                errs.iter().any(|e| e.contains(key)),
                "{key}: the refusal must name the key the author wrote: {errs:?}"
            );
        }
        // The control: the same two-block file with no stray key loads BOTH
        // blocks, so the refusals above are about the key and not about the
        // fixture.
        let ok = parse_workflow(
            "version: 1\nblocks:\n  - id: legal\n    kind: worker\n    cli: claude\n\
             \x20 - id: b\n    kind: worker\n    cli: claude\n",
        )
        .expect("the control file must load");
        assert_eq!(ok.blocks.len(), 2);
    }

    #[test]
    fn intake_schema_field_inventory_is_exhaustively_named() {
        fn raw_workflow_fields(v: RawWorkflow) {
            let RawWorkflow {
                version: _,
                name: _,
                authored_with: _,
                blocks: _,
                edges: _,
                gates: _,
                intake: _,
                merge_queue: _,
                // #858. Confirmed against the rule above before being named
                // here: `resources:` is a map of names to two NUMBERS
                // (`RawResource` — `slots`, `max_hold_minutes`, and
                // `deny_unknown_fields`). It names no branch, no reviewer, no
                // program and no agent, and nothing in the merge/release path
                // reads it — so there is no spelling of this block that can
                // weaken the human gate. What it CAN do is make an agent wait,
                // which is the whole of its restrict-only contract.
                resources: _,
                // #1175. Confirmed against the rule above before being named
                // here: `board:` is per-status WIP limits — a handful of
                // integers keyed by loomux's own board statuses, plus one
                // bool (`RawBoard`/`RawWip`, both `deny_unknown_fields`). It
                // names no branch, no reviewer, no program and no agent, and
                // nothing in the merge/release path reads it. What it CAN do
                // is make a board write warn, or refuse an AGENT's write —
                // never a human's, and never a merge.
                board: _,
                // #1778 §5.3. Confirmed against the rule above before being
                // named here: `driver:` is closed-range numbers and bools
                // (`RawDriver`, `deny_unknown_fields`). It names no PR, no
                // branch, no program and no agent — the two-key rule (§3.2)
                // keeps enabling separate from targeting. A drive exists only
                // once an orchestrator's own role-gated `drive_review` call
                // names one PR, or — where `auto_drive_on_done` is on (#3367) —
                // once a worker the orchestrator spawned reports done on the
                // PR of its own recorded branch; the file still names no
                // target. What it CAN do is tighten the loop the orchestrator
                // template promises, or bound the driver's waits.
                driver: _,
                // #3304 S1. Confirmed against the rule above before being
                // named here: `triage:` is one bool, one closed-range number
                // and two CLOSED vocabularies (`RawTriage`,
                // `deny_unknown_fields`) - `provider`, whose only accepted
                // value is `none`, and `kinds`, drawn from
                // `triage::Kind::ALL`. It names no PR, no branch, no program
                // and no agent, and the rule table it switches on is compiled
                // in rather than written here. What it CAN do is hold a notice
                // back from ONE pane for a bounded time, with every deferral
                // audited and re-readable; every other direction it can be
                // moved in delivers MORE.
                triage: _,
            } = v;
        }
        // #1175: the same inventory rule one level down. A field added to
        // `RawBoard` is a new board policy, and a field added to `RawWip` is a
        // new cappable status — both are changes a reader of this file must
        // see, not ones that pass every existing test.
        fn raw_board_fields(v: RawBoard) {
            let RawBoard { wip: _, enforce: _ } = v;
        }
        fn raw_wip_fields(v: RawWip) {
            let RawWip {
                queued: _,
                in_progress: _,
                review: _,
                pr: _,
                prototype: _,
                human_testing: _,
                blocked: _,
            } = v;
        }
        fn raw_intake_fields(v: RawIntake) {
            let RawIntake { source: _, labels: _ } = v;
        }
        fn raw_intake_labels_fields(v: RawIntakeLabels) {
            let RawIntakeLabels { ready: _, investigate: _, owned: _, prototype: _, hold: _ } = v;
        }
        // #581 §11.2: `merge_queue:` is policy for a host-run queue that pushes
        // refs on the backend's own authority, so the inventory rule matters
        // here for the same reason it does for `intake:` — a field ADDED to
        // this schema must be a visible change, not one that passes every
        // existing test. Nothing here may ever name a branch or grant a
        // capability; the target comes from the enqueued PR's live base (§4)
        // and the default-branch refusals (§7) are not reachable from config.
        fn raw_merge_queue_fields(v: RawMergeQueue) {
            let RawMergeQueue { enabled: _, max_batch: _, checks_timeout_minutes: _ } = v;
        }
        // #1778 §5.3: `driver:` is policy for an engine-run review-loop driver,
        // for the same inventory reason as `merge_queue:` - a field ADDED to
        // this schema must be a visible change, not one that passes every
        // existing test. Nothing here may ever name a PR or start a drive; the
        // two-key rule (§3.2) keeps enabling and targeting separate.
        fn raw_driver_fields(v: RawDriver) {
            let RawDriver {
                enabled: _,
                max_review_rounds: _,
                max_ci_attempts: _,
                max_rebase_attempts: _,
                lane_timeout_minutes: _,
                fix_timeout_minutes: _,
                drive_timeout_minutes: _,
                // #3040: the plan driver's three, under the same inventory
                // rule and with the sharper version of its reason — this
                // schema's second switch can turn on a driver that SPAWNS A
                // PLANNER and turns its output into work, so a key added
                // beside it must be a visible change here.
                plan_enabled: _,
                plan_review_minutes: _,
                planner_timeout_minutes: _,
                // #3367. `fix_nonblocking_rounds` is one more closed-range
                // count, and every round it buys is also spent from
                // `max_review_rounds`, so it can only shorten the loop.
                // `auto_drive_on_done` is the one key here that STARTS a drive,
                // and it is named separately so that fact is visible: it names
                // no PR, no branch and no agent, and what it starts is the
                // hand-off `plan_enabled` already performs for a slice worker
                // (`pd_hand_off`), confined to a worker's OWN PR by the refusals
                // in `rd_auto_start_with`. `docs/design/review-driver.md` §3.2
                // carries the argument.
                fix_nonblocking_rounds: _,
                auto_drive_on_done: _,
            } = v;
        }
        // #3304 S1: `triage:` is policy for a gate that SUPPRESSES a delivery
        // to the orchestrator pane, under the same inventory rule and with its
        // own sharper reason - a key added here can widen what never reaches a
        // human-supervised pane, which is the direction this schema must never
        // move quietly.
        fn raw_triage_fields(v: RawTriage) {
            let RawTriage {
                enabled: _,
                provider: _,
                kinds: _,
                max_defer_minutes: _,
            } = v;
        }
        // Referenced, never called — the compiler still type-checks (and
        // therefore exhaustiveness-checks) every function body above whether
        // or not it runs. This line only exists to avoid a dead-code warning.
        let _ = (
            raw_workflow_fields as fn(RawWorkflow),
            raw_intake_fields as fn(RawIntake),
            raw_intake_labels_fields as fn(RawIntakeLabels),
            raw_merge_queue_fields as fn(RawMergeQueue),
            raw_driver_fields as fn(RawDriver),
            raw_triage_fields as fn(RawTriage),
            raw_board_fields as fn(RawBoard),
            raw_wip_fields as fn(RawWip),
        );
    }
}
