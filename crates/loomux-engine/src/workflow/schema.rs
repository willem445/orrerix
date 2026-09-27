//! The typed schema a parsed workflow is: [`Block`], its edges and gates, the
//! per-feature policies, the intake profile, and [`Workflow`] itself.

use super::*;

pub const SCHEMA_VERSION: u32 = 1;

/// The block ids that name a capability class, and so own that class's
/// instruction file. The first four are the built-in roster's, and they keep
/// their historic file names (`worker.md`, …) — which is what makes a
/// no-workflow group byte-for-byte identical to pre-#222 loomux.
///
/// `manager` (#1161) is the fifth, and it is NOT a built-in-roster id: no
/// default group has a manager block, and [`builtin_roster`] still synthesizes
/// exactly four. It is here because the rule this array encodes is "a class
/// name is a reserved id owning that class's file", and applying it to four of
/// five classes is how the fifth quietly acquires a different rule. Note what
/// this array does *not* do: what stops `- id: manager, kind: worker` is
/// [`kind_from_str`] in `parse_workflow`'s reserved-id check, which reads the
/// CLASS table rather than this one.
///
/// [`roster_is_custom`] deliberately does not read membership here as "nothing
/// a workflow file put there" — see its own doc.
pub const BUILTIN_IDS: [&str; 5] = ["orchestrator", "worker", "reviewer", "planner", "manager"];

// ── the block ───────────────────────────────────────────────────────────────

/// One agent block: an identity, a capability class, and a persona.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// Immutable identity (sanitized `[A-Za-z0-9_-]`). Edges and gates
    /// reference this, never `name`.
    pub id: BlockId,
    /// Display name for the pane/roster. Cosmetic — never a reference target.
    pub name: String,
    /// Capability class: the closed enum. A workflow file *selects* one; it can
    /// never define one. This is where every structural guarantee comes from.
    pub kind: Role,
    /// Agent CLI for this block. Empty = inherit the group default `agent_cli`.
    pub cli: String,
    /// Model for this block. Empty = the kind's default for the resolved CLI.
    pub model: String,
    /// Inline persona (the `prompt:` key). Compiled into a loomux-generated
    /// custom-agent FILE on both CLIs (round #417 correction 6 for Claude;
    /// #416 for Copilot), or — on a directory-write failure — Claude's
    /// `--append-system-prompt-file` / Copilot's kickoff-prompt paste.
    pub prompt: Option<String>,
    /// Repo-relative path to a persona file (the `profile:` key), e.g.
    /// `.github/agents/worker.md`. A `.github/agents/*.md` file is what lets a
    /// Copilot block use its **native** `--agent <name>`.
    pub profile: Option<String>,
    /// Extra pre-approved tool patterns (`--allowedTools` / `--allow-tool`).
    /// Sanitized; may never re-grant what the capability class denies (deny
    /// rules beat allow rules on both CLIs).
    pub allow: Vec<String>,
    /// An optional persona/template marker (`advisor` | `process` |
    /// `liaison`, #250/#324/#891). `parse_workflow` requires it to pair with a
    /// specific `kind` (`advisor` needs `planner`, `process` needs `worker`,
    /// `liaison` needs `reviewer`; see [`role_hint_requires`]) so a workflow
    /// file cannot spell a combination nothing downstream will honor.
    ///
    /// The STRUCTURAL containment never reads it: `kind.containment()` and the
    /// CLI deny-flags take a `Role`, not a `Block`. `mcp::tool_defs` does read
    /// it, for a short list of exceptions enumerated in
    /// `docs/design/liaison.md` — two narrow (`session_digest` to `process`,
    /// `review_verdict` away from `liaison`) and two widen toward that same
    /// `liaison`, both orchestrator-only for every other hint-carrying class
    /// (`group_usage`; and `ask_human`, the pose only). `Role::Lead` also holds
    /// `group_usage`, but through its own enumerated surface rather than
    /// through any hint (#2519) — no hint can reach that class, since no
    /// workflow file can name it. A repo still
    /// cannot grant itself anything by writing one: it picks from a closed set
    /// and loomux's code decides the effect.
    /// `None` is today's behavior, byte for byte.
    pub role_hint: Option<String>,
    /// Thinking-effort level (the `effort:` key, #687) — one of
    /// [`crate::model::EFFORT_LEVELS`]. Empty means "the CLI's own default", which is
    /// today's behavior byte for byte: nothing is emitted at all, so a group on
    /// a CLI build that predates the flag is unaffected unless a human opts in.
    ///
    /// A **value-set pick**, like `model:` — it authors no text and pre-approves
    /// no tool, so it adds nothing to what a repo file can influence. What is
    /// enforced is the `role_hint` shape, twice: the value must be in loomux's
    /// closed vocabulary, and the block's own `cli:` must be one loomux can
    /// actually set effort on ([`CliCaps::effort_levels`](crate::model::CliCaps)).
    pub effort: String,
    /// Context-window variant (the `context:` key, #687) — one of
    /// [`crate::model::CONTEXT_VARIANTS`]. Empty = the model's own window.
    ///
    /// Deliberately NOT part of `model`: [`crate::model::sanitize_model_opt`] strips
    /// brackets, so a `sonnet[1m]` written as a model id would silently become
    /// the broken `sonnet1m`, and widening that sanitizer to admit brackets
    /// would put a POSIX-shell glob pattern on the command line. The suffix is
    /// composed at emit time instead — see `claude_model_token`.
    pub context: String,
    /// An abstract REMOTE LABEL (the `remote:` key, #1457/#1436) — the name of
    /// a machine this block's agent CLI should run on, over SSH. `None` (the
    /// key absent) is today's behavior byte for byte: a local block.
    ///
    /// **The label is a selection, not an address**, and that is the whole
    /// security argument. A repo file is untrusted input (see the
    /// capability-closure rule at the top of this module), so it may pick a
    /// name; the operator — outside the repo, in loomux's own state — decides
    /// which host, which account and which remote clone path that name resolves
    /// to (#1458). A repo-authored `host:`/`port:`/`identity_file:` would let
    /// whoever opens a PR direct execution onto any machine the operator can
    /// reach, so those keys are not "unsupported": they are unknown fields, and
    /// [`RawBlock`] is `deny_unknown_fields`, so one of them fails the WHOLE
    /// file. That failure mode is deliberate in the other direction too: a
    /// build that predates `remote:` refuses a file that declares it, rather
    /// than silently spawning a remote-intended block on the human's own
    /// machine.
    ///
    /// Validated with [`crate::pathseg::check_segment`] — the same checks
    /// [`crate::groupid::GroupId`] delegates to (#925), and for the same
    /// reasons: `[A-Za-z0-9_-]`, length-capped, no leading `-`, no Windows
    /// device name, and **refused rather than rewritten**, so two spellings can
    /// never name one binding. The label is not a path component today, which
    /// is why this keeps a `String` and borrows only the checks; it does reach
    /// an operator-side lookup key and, at #1459, a command line.
    ///
    /// **The alphabet is case-SENSITIVE, and #1458 has to keep it that way**
    /// (#1457 review N3). `check_segment` accepts upper and lower case, so
    /// `buildbox` and `BuildBox` are two different labels here — which is the
    /// only thing that makes "refused, never rewritten" mean anything. A
    /// case-INSENSITIVE lookup on the operator side would put the two spellings
    /// back onto one binding and reintroduce exactly the hazard this refuses,
    /// one layer down where no test in this crate can see it.
    ///
    /// Two pairings are parse errors rather than fields anyone downstream has
    /// to re-check: `remote:` on an orchestrator or manager block, and
    /// `remote:` without `cli: claude`. See `parse_workflow` for both
    /// arguments.
    ///
    /// **Inert in this build.** Nothing reads this field on the spawn path yet:
    /// the operator binding is #1458 and the spawn path #1459, so a block that
    /// declares it spawns exactly as it does today — locally.
    pub remote: Option<String>,
    /// HOW loomux drives this block's agent (the `driver:` key, #2850) —
    /// `Some("structured")` over the CLI's structured-protocol surface, or
    /// `None` (the key absent or empty) for a scraped-PTY pane, which is
    /// every pane today and every pane this build spawns: the spawn-path
    /// wiring that reads this field is #2850 S3b. Normalized through
    /// [`DRIVER_MODES`] like `role_hint` through its own closed set — an
    /// unrecognized value never reaches here, and a CLI without a structured
    /// driver never carries one (see the parse validation).
    pub driver: Option<String>,
    /// The provider prompt-cache TTL, in minutes, this block's agent runs on
    /// (the `cache_ttl_minutes:` key, #3407). `None` (absent) = the CLI's own
    /// [`CliCaps::cache_ttl_minutes`](crate::model::CliCaps) default;
    /// `Some(0)` = "unknown — infer no cache state for this block"; anything
    /// else overrides the default, which is how an account on Anthropic's
    /// 1-hour TTL, or a codex block on a 30-minute model, says so.
    ///
    /// A number that drives a display and a nudge — it grants nothing, reaches
    /// no command line and no path, so the capability-closure rule has nothing
    /// to say about it. Refused above [`crate::cacheage::CACHE_TTL_MINUTES_MAX`]
    /// rather than clamped: no provider documents a longer cache, so a larger
    /// value is a typo. See [`crate::cacheage::effective_ttl_minutes`].
    pub cache_ttl_minutes: Option<u32>,
}

/// The per-block model knobs that reach a spawn alongside the model itself
/// (#687): "how this block's model is tuned", as one value.
///
/// Bundled rather than threaded as two more positional `&str`s because they are
/// one concept, and because the next knob (gemini's `thinkingConfig`, deferred
/// pending live schema verification) lands here rather than as a third
/// argument. `default()` — both empty — is exactly the pre-#687 command line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelKnobs<'a> {
    pub effort: &'a str,
    pub context: &'a str,
}

impl Block {
    /// Agent-id prefix (`w-3`, `rev-4`). Moved off `Role` onto the block, but
    /// deliberately still *derived from* the capability class: agent ids are
    /// short, are parsed by the roster/badge conventions, and must stay
    /// byte-identical for the built-in roster. Block identity rides in
    /// `orchestration::AgentEntry`'s `block` field and the pane name instead.
    pub fn prefix(&self) -> &'static str {
        self.kind.prefix()
    }

    /// The file in the group dir that carries this block's loomux role
    /// contract, referenced by the kickoff prompt. The built-in blocks keep
    /// their historic names (`worker.md`, …) so a default group's kickoff text
    /// is unchanged; a custom block gets `<id>.md`.
    pub fn instructions_file(&self) -> String {
        if BUILTIN_IDS.contains(&self.id.as_str()) {
            crate::model::role_instructions_file(self.kind).to_string()
        } else {
            format!("{}.md", self.id)
        }
    }

    /// Whether this block's id is a reserved class name ([`BUILTIN_IDS`]) — so
    /// it owns that class's instruction file rather than a `<id>.md` of its
    /// own.
    ///
    /// For the four built-in classes this is also "is one of the built-in
    /// roster entries", which is what it was called before #1161 and what every
    /// pre-existing caller means by it. `- id: manager` satisfies it too and is
    /// NOT a built-in roster entry, which is why [`roster_is_custom`] asks a
    /// second question rather than reading this alone.
    pub fn is_builtin(&self) -> bool {
        BUILTIN_IDS.contains(&self.id.as_str())
    }

    /// A block with no persona behaves exactly like a pre-#222 role: no persona
    /// text to fold into the generated custom-agent file, nothing to inject
    /// into the kickoff — the CONTRACT itself still always compiles (#416).
    pub fn has_persona(&self) -> bool {
        self.prompt.is_some() || self.profile.is_some()
    }

    /// This block's model knobs (#687), as the one value the spawn path passes
    /// to `build_agent_command_ex` / `build_agent_argv_ex`.
    pub fn knobs(&self) -> ModelKnobs<'_> {
        ModelKnobs { effort: &self.effort, context: &self.context }
    }
}

/// An advisory edge: the *declared happy path*, drawn by the GUI and offered to
/// the orchestrator as context. loomux does **not** execute it — the
/// orchestrator keeps its scheduling judgment (mergeability, parallel vs
/// serial, plan-first vs straight-to-worker), which a static DAG cannot make.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: BlockId,
    pub to: Vec<BlockId>,
}

/// How many of a gate's reviewers must pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateRequire {
    /// Every named reviewer must have recorded a PASS.
    AllPass,
    /// At least N of the named reviewers must have recorded a PASS.
    Threshold(u32),
}

/// A declared gate (today: only `merge`). **Parsed and validated here; enforced
/// in the `gh` shim** — see [`evaluate_merge_gate`] for the decision and
/// [`gate_file_text`] for the spec file the shim reads. The reviewer-attributed
/// state it keys off is written by the `review_verdict` MCP tool ([`Verdict`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gate {
    pub require: GateRequire,
    /// Block ids of the reviewers whose verdicts the gate reads. Validated to
    /// exist and to be `kind: reviewer` — a gate naming a worker would be
    /// unsatisfiable.
    pub reviewers: Vec<BlockId>,
    /// Extra named conditions (e.g. `ci-green`). Sanitized at parse
    /// ([`sanitize_condition`]); a condition this build cannot check **fails
    /// closed** in the shim rather than silently passing — see
    /// [`KNOWN_CONDITIONS`].
    pub also: Vec<String>,
    /// The small-batch clause (#1174): the largest PR, in changed lines
    /// (additions + deletions), this gate will let through. `None` — the key
    /// absent — is the whole feature off, byte-for-byte the pre-#1174 flow.
    ///
    /// **A structured key rather than an `also:` token, deliberately.** `also:`
    /// is a closed vocabulary of *parameterless* conditions; a threshold is a
    /// number, and stuffing it into a token (`max-diff-800`) would put a
    /// parameter into a namespace whose whole safety property is that every
    /// entry either matches a known name or fails closed.
    ///
    /// Pure repo config (CLAUDE.md constraint 8): loomux never learns what 800
    /// means for this repo, only that this repo said 800.
    pub max_diff_lines: Option<u32>,
    /// Path-based reviewer routing (#1176) — rules that make the required
    /// reviewer set a function of the diff. Empty — the key absent — is the
    /// whole feature off, byte-for-byte the pre-#1176 flow, and
    /// [`route_reviewers`] never even looks at a changed-file list.
    ///
    /// **Additive, and only ever tightening**: the required set is
    /// [`reviewers`](Self::reviewers) ∪ the reviewers of every rule that
    /// matched. A rule that matches nothing costs nothing; a rule that matches
    /// adds a lane. Nothing here can make a gate easier to satisfy, which is
    /// why `routing:` and `require: threshold` are refused together at parse
    /// ([`parse_workflow`]) — see [`RoutingRule`].
    pub routing: Vec<RoutingRule>,
}

/// One path-based routing rule (#1176): *if this PR touched any of these paths,
/// these reviewers are required too.*
///
/// Deliberately loomux-native globs rather than the repo's own `CODEOWNERS`:
/// that file names GitHub users and teams, which are not workflow blocks, and
/// the mapping between them would be repo config anyway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutingRule {
    /// Path globs, matched against the PR's changed files with
    /// [`glob_match`]. Validated at parse through [`sanitize_glob`]; at least
    /// one, at most [`ROUTING_PATHS_MAX`].
    pub paths: Vec<String>,
    /// Block ids required when any path matches. Validated exactly as
    /// [`Gate::reviewers`] is — the block must exist, be `kind: reviewer`, and
    /// not be a liaison — because a rule naming anything else could never be
    /// satisfied.
    pub reviewers: Vec<BlockId>,
}

/// How many routing rules one gate may declare.
///
/// A bound rather than a preference: the shim evaluates **every** rule against
/// **every** changed file on **every** merge, in POSIX shell, so an unbounded
/// rule list is an unbounded cost on the merge path. Past this many lanes the
/// block has stopped routing and started listing, which is a different feature.
pub const ROUTING_RULES_MAX: usize = 32;

/// How many path globs one routing rule may carry — the other half of the
/// product [`ROUTING_RULES_MAX`] bounds.
pub const ROUTING_PATHS_MAX: usize = 32;

/// Longest path glob accepted. Generous next to [`MAX_ID_CHARS`] because a repo
/// path legitimately is: `crates/loomux-engine/src/**` is ordinary.
pub const MAX_GLOB_CHARS: usize = 200;

/// Why a block id may not be named as a gate reviewer — `None` when it may.
///
/// **One definition for both lists** (#1176): `gates.merge.reviewers` and every
/// `gates.merge.routing[].reviewers`. They are the same question — *could a
/// verdict for this id ever be recorded?* — and answering it twice is how the
/// static list ends up refusing a manager while a routing rule quietly accepts
/// one. `ctx` names which list is being read so the message points at the line
/// the author has to fix.
pub(super) fn gate_reviewer_error(gate: &str, ctx: &str, rname: &str, blocks: &[Block]) -> Option<String> {
    match blocks.iter().find(|b| b.id == rname) {
        None => Some(format!("gates.{gate}: {ctx} {rname:?} names no block")),
        // The manager (#1161) is structurally caught by the arm below — it is
        // not reviewer-kind — but "that block's kind is manager, not a
        // reviewer" describes the type error and not the mistake. An author who
        // named the manager on a gate was reaching for "the human signs off",
        // which is a real thing they wanted and a real thing this gate cannot
        // express, so say that instead. The pane validator carries the same arm
        // (`validateWorkflow`, gate-not-a-reviewer), and the liaison arm below
        // is the shape both are modelled on.
        Some(b) if b.kind == Role::Manager => Some(format!(
            "gates.{gate}: {ctx} {:?} is a manager — the manager is the human's \
             interface, not a reviewer: it records no verdict, so a gate naming it \
             could never open. A gate reads REVIEWER verdicts; the human's own sign-off \
             is the merge gate loomux already applies on top of it.",
            b.id
        )),
        // A gate reads reviewer verdicts. Naming a worker would make it
        // permanently unsatisfiable — nothing would ever record a verdict for
        // it — which is the "dangling reference the UI happily saves" failure
        // this validation pass exists to prevent.
        Some(b) if b.kind != Role::Reviewer => Some(format!(
            "gates.{gate}: {ctx} {:?} is a {} block, not a reviewer — a gate can only require reviewer verdicts",
            b.id,
            b.kind.as_str()
        )),
        // The same unsatisfiable gate, one kind further in (#891). A liaison IS
        // reviewer-kind — it rides that class for its read-only, persistent
        // posture — but it is denied `review_verdict` at every layer precisely
        // because it reviews nothing. Naming one here would therefore wait
        // forever for a verdict no code path can produce, which is exactly the
        // failure the arm above refuses; caught at parse rather than discovered
        // as a merge gate that never opens.
        Some(b) if b.role_hint.as_deref() == Some("liaison") => Some(format!(
            "gates.{gate}: {ctx} {:?} is a liaison — it is reviewer-kind, but a \
             liaison never records a verdict (it presents the human's questions and \
             relays their answers), so a gate naming it could never open. Name a \
             reviewer that reviews.",
            b.id
        )),
        Some(_) => None,
    }
}

// ── resources: named lock resources (#858) ─────────────────────────────────

/// Slots a resource gets when it declares none. One — the useful default is a
/// mutex, and a repo that wants a semaphore says so.
pub const RESOURCE_SLOTS_DEFAULT: u32 = 1;

/// Ceiling on `slots`. Not a resource constraint — a legibility one: past this
/// the declaration no longer serializes anything, and a repo that wrote `1000`
/// meant something other than what it said.
pub const RESOURCE_SLOTS_MAX: u32 = 64;

/// How long a hold may last before the sweep reclaims it, when the resource
/// declares nothing. Long enough for a real build or test run, short enough
/// that a crashed holder's slot comes back inside a working session.
pub const RESOURCE_MAX_HOLD_MINUTES_DEFAULT: u32 = 30;

/// Ceiling on `max_hold_minutes` (8h). A hold is a bound on a *fallible*
/// signal — "the holder will call release_lock" — and the lessons-file rule is
/// that such a bound exists and is finite. A repo may make it generous; it may
/// not make it decorative.
pub const RESOURCE_MAX_HOLD_MINUTES_MAX: u32 = 480;

/// How many resources one repo may declare. Every declared name is listed in
/// the `acquire_lock` tool description that every agent in the group reads, so
/// the cap bounds a per-agent context cost, not a memory one.
pub const RESOURCES_MAX: usize = 32;

/// One declared lock resource: how many agents may hold it at once, and how
/// long any one of them may hold it before loomux takes it back.
///
/// **Policy, not mechanism** (CLAUDE.md constraint 8). Nothing here names a
/// toolchain, a command, or a machine: `build` is a *string this repo chose*,
/// and loomux never learns what it means. The whole schema is two numbers.
///
/// **Restrict-only, like the resource guard #318 designed before it.** A
/// `resources:` block can make an agent wait; it can never grant one a
/// capability, name a program to run, or reach the capability-closure spine
/// (`blocks:`/`edges:`/`gates:`). The worst a hostile `.loomux/workflow.yml`
/// can do with it is declare `slots: 1` on something everyone needs and slow
/// the group down — and both the hold and the wait are bounded above, and
/// every acquire/release/reclaim is audited.
///
/// **An absent block means the feature is off**: no `resources:` at all and
/// the three lock tools are not even listed to the group's agents, so behavior
/// is byte-for-byte what it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourcePolicy {
    /// Concurrent holders allowed. `1` (the default) is a mutex.
    pub slots: u32,
    /// The reclaim deadline on a single hold.
    pub max_hold_minutes: u32,
}

impl Default for ResourcePolicy {
    fn default() -> Self {
        ResourcePolicy {
            slots: RESOURCE_SLOTS_DEFAULT,
            max_hold_minutes: RESOURCE_MAX_HOLD_MINUTES_DEFAULT,
        }
    }
}

// ── merge_queue: the bisecting merge queue's policy (#581 §11.2) ───────────

/// Default batch size (§11.2). At `3`, a red batch costs at most 2 extra CI
/// runs to attribute (ceil(log2 3)).
pub const MERGE_QUEUE_MAX_BATCH_DEFAULT: u32 = 3;

/// The `merge_queue:` block — a sibling of [`Gate`]'s `gates:`, and the whole
/// of what a repo declares about the queue. Design note:
/// `docs/design/merge-queue.md` §11.2; the queue's own core is [`crate::mergeq`]
/// (here since #888 batch 6), its write primitives are [`crate::mqdriver`]
/// (batch 12a), and the loop that sequences them is [`crate::mqloop`] (batch
/// 12b). None of that wiring reaches a pane host: this line used to say it did,
/// and batch 9 re-measured it.
///
/// **Policy, not mechanism** (CLAUDE.md constraint 8). Nothing here names a
/// branch, a toolchain, or a verification command: the queue *observes* the
/// repo's own CI and never defines or runs it, and the target it lands on comes
/// from the first enqueued PR's live base rather than from this file (§4).
///
/// **An absent block means the feature is off and behavior is byte-for-byte
/// unchanged** — the same posture `gates:` takes, and the reversal mechanism
/// §12 names: delete the block and every queue path is unreachable.
///
/// **Adding the block breaks the file for builds that predate #581 slice C, and
/// that is deliberate.** [`RawWorkflow`] is `deny_unknown_fields`, so
/// `merge_queue:` is not a tolerated unknown key on an older build: it fails
/// the parse of the *whole* file, gates and all, down the loud `workflow-invalid`
/// path. It is the right behavior anyway — `workflow.yml` is human-authored
/// policy, and a key the build does not understand means a human believes a
/// policy is in force that is not. Note the deliberate asymmetry with
/// `merge_queue.json` (`mergeq::MergeQueueState`), which *tolerates and
/// preserves* unknown fields because it is machine-authored state: policy fails
/// loud, state degrades gracefully.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeQueuePolicy {
    /// Default **false**. The product default is off (§12).
    pub enabled: bool,
    /// How many approved sub-PRs one speculative batch may carry (§11.2).
    /// At least 1 — see [`parse_workflow`]. There is no upper bound here
    /// because none is needed: the effective ceiling is the queue's own entry
    /// cap (`mergeq::MAX_ENTRIES`, §10), so an oversized value degenerates to
    /// "batch everything queued" rather than being unsatisfiable.
    pub max_batch: u32,
    /// The backstop on waiting for a batch's checks (§5). The primary release
    /// is the checks going terminal; this bounds the case where a repo attaches
    /// no checks at all, so the batch surfaces as **unverifiable** rather than
    /// sitting pending in silence — the lessons-file rule that any suppression
    /// driven by a fallible signal must be bounded.
    pub checks_timeout_minutes: u32,
}

impl Default for MergeQueuePolicy {
    fn default() -> Self {
        MergeQueuePolicy {
            enabled: false,
            max_batch: MERGE_QUEUE_MAX_BATCH_DEFAULT,
            // Same default and same bounds as a notify watch's TTL, from the
            // one definition — see the clamp in [`parse_workflow`].
            checks_timeout_minutes: NOTIFY_EXPIRES_DEFAULT_MIN,
        }
    }
}

// ── driver: the review-loop driver's policy (#1778 §5.3) ───────────────────

/// INVARIANT 9's numbers (`templates/orchestrator.md`): three CI attempts, three
/// rounds of review findings, one rebase attempt. The `driver:` block is held
/// to them, never loosened from them (§2.3): a value outside the closed range
/// is **refused** - a repo may run a *tighter* loop than the
/// orchestrator template promises; it may not run a looser one, because the
/// driver acts on the orchestrator's authority and a repo file that raised the
/// bound would be loosening the orchestrator's own invariant from a
/// configuration file. *(Deliberately tighter than the `1..=5` clamp the #1778
/// plan first proposed; the reasoning is §2.3's, and it narrows the plan on
/// purpose.)*
pub const DRIVER_MAX_REVIEW_ROUNDS_MIN: u32 = 1;
pub const DRIVER_MAX_REVIEW_ROUNDS_MAX: u32 = 3;
pub const DRIVER_MAX_CI_ATTEMPTS_MIN: u32 = 1;
pub const DRIVER_MAX_CI_ATTEMPTS_MAX: u32 = 3;
pub const DRIVER_MAX_REBASE_ATTEMPTS_MIN: u32 = 0;
pub const DRIVER_MAX_REBASE_ATTEMPTS_MAX: u32 = 1;
/// `driver.fix_nonblocking_rounds` (#3367 item 1): `0..=3`, default `0`
/// (off). Refused outside the range like the three counters above, and for
/// the same reason — it is a count of review rounds the DRIVER spends, and
/// every one of them is also spent from `max_review_rounds`, so its ceiling
/// is that counter's.
pub const DRIVER_FIX_NONBLOCKING_ROUNDS_MIN: u32 = 0;
pub const DRIVER_FIX_NONBLOCKING_ROUNDS_MAX: u32 = 3;

/// How long a drive may sit before it is `held(drive-stalled)` (§2.1) — the
/// **backstop**, since #2110, beneath `reviewdrive`'s per-state bounds.
///
/// **Twelve hours, and it left the notify-TTL clamp family to get there.** The
/// design named 240 because that was the family's ceiling and the total age was
/// then the only clock over a working drive; both halves of that stopped being
/// true at once. Once a drive is bounded state by state — `ci-wait` at ninety
/// minutes, `review-wait` at its constant plus one `lane_timeout_minutes` per
/// required lane, and so on, each reset by every
/// transition — a total age measured in the same units is not a second opinion
/// about the same thing, it is the *only* bound that a drive making steady
/// progress can still trip. At 240 it tripped: two drives were parked
/// `drive-stalled` at four hours with rounds in flight and CI green, which is
/// #2110. What the backstop is for is the drive that advances forever without
/// finishing — §8's `also: [base-green]` row — and half a day is the first
/// figure that cannot be an honest review loop.
///
/// **This is a looser number than it replaces, and §2.3's closure still holds**,
/// because that closure is about INVARIANT 9's *counters* — rounds, CI attempts,
/// rebases — which are unchanged and still refuse a repo that tries to widen
/// them. The timeouts are pacing, not budget, and the pacing this ships is
/// strictly tighter overall: before, a stuck drive waited four hours whatever it
/// was stuck on; now the state it is stuck IN answers, on a per-state clock a
/// transition resets. `reviewdrive::state_bound_ms` is where the per-arm shape
/// lives, and a figure restated here is how this line went stale once already.
pub const DRIVER_DRIVE_TIMEOUT_DEFAULT_MIN: u32 = 720;

/// The closed range for `driver.drive_timeout_minutes` — its **own**, no longer
/// the notify-TTL family's, since its default now sits above that family's
/// ceiling. The floor is still the family's, so a repo that already declares a
/// tight backstop keeps it; the ceiling is a day, which bounds the value at
/// something a human would recognise as a mistake rather than at nothing.
///
/// Its two siblings, `lane_timeout_minutes` and `fix_timeout_minutes`, stay on
/// [`clamp_expires_minutes`]: each really is one bounded wait on one fallible
/// signal, which is the quantity that clamp is about, and neither is the
/// last-resort bound over a whole drive.
pub const DRIVER_DRIVE_TIMEOUT_MIN: u32 = NOTIFY_EXPIRES_MIN;
pub const DRIVER_DRIVE_TIMEOUT_MAX: u32 = 24 * 60;

/// `driver.drive_timeout_minutes` brought inside [`DRIVER_DRIVE_TIMEOUT_MIN`]
/// ..=[`DRIVER_DRIVE_TIMEOUT_MAX`], with the default standing in for an absent
/// value — `clamp_expires_minutes`' shape, on this field's own range.
pub fn clamp_drive_timeout_minutes(raw: Option<u32>) -> u32 {
    raw.unwrap_or(DRIVER_DRIVE_TIMEOUT_DEFAULT_MIN)
        .clamp(DRIVER_DRIVE_TIMEOUT_MIN, DRIVER_DRIVE_TIMEOUT_MAX)
}

/// The `driver:` block — policy for the engine-driven review-loop driver
/// (`docs/design/review-driver.md`), a sibling of [`MergeQueuePolicy`] and the
/// whole of what a repo declares about the drive. The driver's own core is
/// [`crate::reviewdrive`]; this struct is what the FILE means, the half a repo
/// author can get wrong.
///
/// **Policy, not mechanism** (CLAUDE.md constraint 8). Nothing here names a PR,
/// a branch, a command, or a lane: a drive exists only once an orchestrator
/// makes its own role-gated `drive_review` call naming one PR (§3.2's two-key
/// rule — this block can *enable*; it can never target or widen a drive).
/// Every field is a bool or a number from a closed range, so the field-by-field
/// capability-closure test passes; but the two-key structure is the real
/// safety, not the data types — a `driver.auto: true` is a bool and could
/// still defeat §3.2's per-PR consent. **`auto_drive_on_done` (#3367) is
/// exactly that key, and it was added with §3.2 rewritten rather than because
/// it is a bool**: it starts only the drive the orchestrator's own spawn
/// already chose — a worker on its recorded branch, on that branch's PR — and
/// the note argues why that keeps the second key in the orchestrator's hand.
///
/// **An absent block means the feature is off and behavior is byte-for-byte
/// unchanged**, the posture `gates:` and `merge_queue:` both take.
///
/// **Adding the block breaks the file for builds that predate #1778, and that
/// is deliberate** — [`MergeQueuePolicy`]'s forward-compat warning restated
/// because it is a real property of the opt-in. [`RawWorkflow`] is
/// `deny_unknown_fields`, so `driver:` is not a tolerated unknown key on an
/// older build: it fails the parse of the *whole* file, gates and all, down the
/// loud `workflow-invalid` path. It is the right behavior anyway —
/// `workflow.yml` is human-authored policy, and a key the build does not
/// understand means a human believes a policy is in force that is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriverPolicy {
    /// Default **false**. The product default is off (§9).
    pub enabled: bool,
    /// Review-finding rounds one drive may spend (§2.3). Refused outside
    /// [`DRIVER_MAX_REVIEW_ROUNDS_MIN`]..=[`DRIVER_MAX_REVIEW_ROUNDS_MAX`] the
    /// way `merge_queue.max_batch` is — a malformed block never degrades to
    /// defaults, and a driver running a looser loop than INVARIANT 9 promises
    /// is a driver nobody can reason about.
    pub max_review_rounds: u32,
    /// CI attempts one drive may spend (§2.3). Same closed range and same
    /// refusal as [`Self::max_review_rounds`].
    pub max_ci_attempts: u32,
    /// Rebase attempts one drive may spend (§2.3). `0..=1` — one rebase, as
    /// INVARIANT 9 says, and `0` is legal: a repo may refuse the driver any
    /// rebase at all, which is the tighter direction this block is allowed.
    pub max_rebase_attempts: u32,
    /// Backstop on a reviewer lane producing a verdict (§2.1's
    /// `held(lane-stalled)`). Clamped like the notify TTLs — the same quantity,
    /// a bounded wait on a fallible signal.
    pub lane_timeout_minutes: u32,
    /// Backstop on a resumed worker pushing or reporting (§2.1's
    /// `held(fix-stalled)`). Same clamp family as [`Self::lane_timeout_minutes`].
    pub fix_timeout_minutes: u32,
    /// Whether the PLAN driver is on (#3040 §2(b)). A SECOND key rather than a
    /// widening of [`Self::enabled`], and the separation IS the consent: a repo
    /// that turned the review driver on consented to orrerix running a review
    /// loop it already had an orchestrator for — not to orrerix spawning a
    /// PLANNER and turning its output into work. Default **false**, and read
    /// under `enabled` as well, because the plan driver reuses the review
    /// driver's whole tick, record and backoff discipline: it is off wherever
    /// that is.
    pub plan_enabled: bool,
    /// The plan-review window in minutes (#3040 §2(c)). Refused outside
    /// [`crate::plandrive::PLAN_REVIEW_MINUTES_MIN`]
    /// ..=[`crate::plandrive::PLAN_REVIEW_MINUTES_MAX`], the posture
    /// [`Self::max_review_rounds`] takes. `0` — the default — means no window:
    /// the human already pressed go with the label, and a window nobody is told
    /// about is unused.
    pub plan_review_minutes: u32,
    /// Backstop on a driven planner posting a plan (#3040 §2(e)). Refused
    /// outside [`crate::plandrive::PLANNER_TIMEOUT_MINUTES_MIN`]
    /// ..=[`crate::plandrive::PLANNER_TIMEOUT_MINUTES_MAX`].
    ///
    /// **Refused rather than clamped**, unlike the two lane/fix backstops
    /// above: those are the notify-TTL family and share its clamp, and this one
    /// is not in that family. A repo asking for a five-minute planner timeout
    /// has misunderstood what a planner does, and silently handing it fifteen
    /// would leave the misunderstanding in place while the behaviour changed
    /// underneath it — `merge_queue.max_batch`'s own argument.
    pub planner_timeout_minutes: u32,
    /// Backstop on the drive's whole age (§2.1's `held(drive-stalled)` — age
    /// since the entry began, never an idle clock reset by each state
    /// advance). Same clamp family; the default is the family's ceiling
    /// because a drive's whole budget is what this bounds.
    pub drive_timeout_minutes: u32,
    /// Non-blocking rounds the driver may run on its own at a satisfied gate
    /// (#3367 item 1) — see `reviewdrive::DriveLimits::fix_nonblocking_rounds`.
    /// Default `0`, which is the pre-#3367 behaviour: a gate satisfied with
    /// non-blocking findings open wakes the orchestrator at once.
    pub fix_nonblocking_rounds: u32,
    /// A worker's `report(done, ref: <PR>)` starts a review drive on that PR
    /// (#3367 item 2). Default **false**, and a SECOND key under `enabled` for
    /// [`Self::plan_enabled`]'s reason: turning the driver on consented to a
    /// drive an orchestrator starts by naming a PR, not to one a delegate's
    /// report starts. `docs/design/review-driver.md` §3.2 carries the argument
    /// for why this key, and only this key, may start a drive from a file.
    pub auto_drive_on_done: bool,
}

impl Default for DriverPolicy {
    fn default() -> Self {
        DriverPolicy {
            enabled: false,
            max_review_rounds: DRIVER_MAX_REVIEW_ROUNDS_MAX,
            max_ci_attempts: DRIVER_MAX_CI_ATTEMPTS_MAX,
            max_rebase_attempts: DRIVER_MAX_REBASE_ATTEMPTS_MAX,
            // Same default and same bounds as a notify watch's TTL, from the
            // one definition — see the clamp in [`parse_workflow`].
            lane_timeout_minutes: NOTIFY_EXPIRES_DEFAULT_MIN,
            fix_timeout_minutes: NOTIFY_EXPIRES_DEFAULT_MIN,
            drive_timeout_minutes: DRIVER_DRIVE_TIMEOUT_DEFAULT_MIN,
            plan_enabled: false,
            plan_review_minutes: crate::plandrive::PLAN_REVIEW_MINUTES_DEFAULT,
            planner_timeout_minutes: crate::plandrive::PLANNER_TIMEOUT_MINUTES_DEFAULT,
            fix_nonblocking_rounds: DRIVER_FIX_NONBLOCKING_ROUNDS_MIN,
            auto_drive_on_done: false,
        }
    }
}

// ── triage: orchestrator delivery triage (#3304 S1) ────────────────────────

/// The `triage:` block — policy for the delivery-triage gate
/// (`docs/design/delivery-triage.md`), a sibling of [`DriverPolicy`] and read
/// in exactly the same posture: an absent block means the feature is off and
/// behaviour is byte-for-byte unchanged.
///
/// **Policy, not mechanism** (CLAUDE.md constraint 8). Nothing here names a
/// pane, an agent, a PR or a branch. The two fields that are not a bool or a
/// number are a CLOSED vocabulary each — `provider` is one accepted value in
/// this slice, `kinds` is drawn from [`crate::triage::Kind::ALL`] — so the
/// field-by-field capability closure holds: this block can turn a
/// SUPPRESSION on and bound it, and there is no spelling in it that grants
/// anything.
///
/// **Restrict-only in the direction that matters.** Every field can make
/// orrerix deliver MORE (`enabled: false`, a shorter `max_defer_minutes`, a
/// narrower `kinds`); the only field that can make it deliver less is
/// `enabled`, and the rule table it switches on is compiled in rather than
/// configurable. A hostile `.orrerix/workflow.yml` cannot write a rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriagePolicy {
    /// Default **false**. The product default is off, and with it off not one
    /// byte of delivery behaviour moves.
    pub enabled: bool,
    /// Which classifier tier may see the residual. `none` — the only value
    /// this slice accepts — means the rule tier and nothing else: no network,
    /// no key, no text leaving the machine. #3304 S3 adds the second value,
    /// and a file naming one today is REFUSED rather than run with no
    /// provider, because an author who wrote `provider: typesafe` believes
    /// text is being classified and it is not.
    pub provider: String,
    /// The kinds triage may act on at all. **Empty means every kind the rule
    /// table covers** — the ordinary case, and what an absent key resolves
    /// to. A name that is not one of [`crate::triage::Kind::ALL`]'s wire
    /// spellings is a hard error rather than an ignored line: a repo that
    /// misspelled a kind believes it narrowed the gate and did not.
    pub kinds: Vec<crate::triage::Kind>,
    /// The clock under a deferral. Refused outside
    /// [`crate::triage::TRIAGE_MAX_DEFER_MINUTES_MIN`]
    /// ..=[`crate::triage::TRIAGE_MAX_DEFER_MINUTES_MAX`], the posture
    /// `merge_queue.max_batch` takes — its own doc carries why this one is
    /// refused rather than clamped.
    pub max_defer_minutes: u32,
}

impl Default for TriagePolicy {
    fn default() -> Self {
        TriagePolicy {
            enabled: false,
            provider: crate::triage::PROVIDER_NONE.to_string(),
            kinds: Vec::new(),
            max_defer_minutes: crate::triage::TRIAGE_MAX_DEFER_MINUTES_DEFAULT,
        }
    }
}

impl TriagePolicy {
    /// The shape the pure decision reads — [`crate::triage::decide`] takes no
    /// dependency on the whole workflow.
    pub fn as_triage_policy(&self) -> crate::triage::Policy {
        crate::triage::Policy {
            enabled: self.enabled,
            kinds: self.kinds.clone(),
            max_defer_minutes: self.max_defer_minutes,
        }
    }
}

// ── board: per-status WIP limits (#1175 / #1170 A2) ────────────────────────

/// The smallest cap that means anything. `0` would say "nothing may ever enter
/// this status", which is a *stop*, not a work-in-progress limit — and under
/// `enforce` it would wedge the board rather than pace it. Refused at parse
/// time, the posture `merge_queue.max_batch` and `resources.slots` take.
pub const WIP_LIMIT_MIN: u32 = 1;

/// The one status a cap may **not** name, and the reason it may not.
///
/// `done` is terminal and it is the *relief valve*: every other cap is
/// relieved by work reaching it. A limit there would refuse the very
/// transition that unblocks the board — the exact inversion of what a WIP
/// limit is for — so the wire struct simply has no field for it and
/// `deny_unknown_fields` refuses `done:` with an error naming the statuses
/// that ARE cappable. Stated as a constant so the docs, the error path and
/// the test that pins the field set all read the same name.
pub const WIP_UNCAPPABLE_STATUS: &str = "done";

/// The `board:` block — what a repo declares about how much work may sit in
/// each board status at once (#1175; the practice is kanban's WIP limit, and
/// the loomux-specific motivation is in `docs/design/board-wip.md`).
///
/// **Policy, not mechanism** (CLAUDE.md constraint 8). Nothing here names a
/// toolchain, a branch, a repo path or an agent: the whole schema is a handful
/// of integers and one bool, keyed by loomux's own board statuses.
///
/// **Restrict-only, like `resources:` before it.** A `board:` block can make a
/// write wait or warn; it can never grant a capability, and it cannot reach
/// the capability-closure spine (`blocks:`/`edges:`/`gates:`). The worst a
/// hostile `.loomux/workflow.yml` can do with it is declare `in-progress: 1`
/// and slow a group down — and even that only bounces *agent* writes, never
/// the human's own board edits (see [`BoardPolicy::enforce`]).
///
/// **An absent block means the feature is off**: no `board:` at all leaves
/// [`BoardPolicy::wip`] empty, no write is ever counted, and behavior is
/// byte-for-byte what it was. Same posture — and the same
/// `deny_unknown_fields` consequence for older builds — as `merge_queue:`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoardPolicy {
    /// Per-status caps, keyed by the board status exactly as it is spelled on
    /// the wire (`in-progress`, `human-testing`, …). A status **absent from
    /// this map has no cap** — this is not a map with defaults, it is the set
    /// of limits the repo actually declared. Empty = the feature is off.
    ///
    /// Kept as a map rather than as the wire struct so every consumer
    /// (accounting, the refusal text, the board's own chips) stays
    /// status-generic: the closed struct exists to *validate* the names, not
    /// to be the shape the rest of loomux reasons about.
    pub wip: BTreeMap<String, u32>,
    /// **Default false**, which is warn-and-notify. `true` makes an
    /// **agent-origin** write that would cross a cap a hard refusal.
    ///
    /// It never applies to the human's own board edits, under either setting.
    /// The board's authority is the human's, not a queue discipline — the same
    /// reason `claim` is deliberately not exposed on the human's board command
    /// — and a limit a human set for their agents must not bounce the human
    /// who set it. Their crossing still warns and still audits, so the
    /// orchestrator learns the board moved; it is only ever the *refusal* that
    /// is agent-only.
    pub enforce: bool,
}

// ── intake: source + label vocabulary (#382 P1) ────────────────────────────
//
// Where autonomous work comes from and what its label vocabulary is called —
// the missing sibling of `gates:` ("what gates it" beside "where it comes
// from"). Inert vocabulary + an adapter choice from a fixed set, the same
// capability-closure argument as `blocks:` above: `intake:` can never grant a
// capability, and there is deliberately **no spelling that can disable the
// human merge gate** — that lives in the `gh` shim, keyed to group markers,
// and is not reachable from this file at all. `deny_unknown_fields` on
// `RawIntake`/`RawIntakeLabels` (below) makes a `human_gate: false`-style key
// a hard parse error by construction, not a line this schema has to
// specifically recognize and reject.

/// Where intake work comes from. A workflow file *selects* one of these; it
/// can never define a new one — same "reject, never coerce" posture as
/// [`kind_from_str`].
///
/// `Board` and `None` are **schema-reserved, not wired**: the #382 plan ships
/// the `github-labels` adapter fully in this slice and designs the other two
/// into the schema now so the config contract never churns, but their runtime
/// (a non-`gh` poll source, tracker-agnostic loop prose) is a follow-on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntakeSource {
    /// Poll `gh issue list`, match label strings client-side. Today's only
    /// wired behavior, and the built-in default.
    GithubLabels,
    /// The loomux task board is the queue. Schema-reserved (Phase B).
    Board,
    /// No autonomous intake at all — idle-tick still runs its other chores
    /// (PR-sweep, lost-notification backstop) but never polls for labelled
    /// work. Schema-reserved (Phase B); valid to declare today even though
    /// nothing yet reads it, since P4 wires the consumers.
    None,
}

impl IntakeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            IntakeSource::GithubLabels => "github-labels",
            IntakeSource::Board => "board",
            IntakeSource::None => "none",
        }
    }
}

impl Default for IntakeSource {
    fn default() -> Self {
        IntakeSource::GithubLabels
    }
}

/// Parse an `intake.source:` value. Trimmed empty string maps to the built-in
/// default (`github-labels`) — matching the plan's `source: github-labels
/// (default)` schema comment, and letting a repo override just the labels
/// without repeating a source it already means. Anything else unrecognized is
/// `None`, which the caller turns into a hard, allowed-set-naming error —
/// never coerced, the same shape [`kind_from_str`] enforces.
pub fn intake_source_from_str(s: &str) -> Option<IntakeSource> {
    match s.trim().to_ascii_lowercase().as_str() {
        "" | "github-labels" => Some(IntakeSource::GithubLabels),
        "board" => Some(IntakeSource::Board),
        "none" => Some(IntakeSource::None),
        _ => None,
    }
}

/// The intake sources a workflow file may name, for error messages.
pub fn intake_source_names() -> String {
    "github-labels, board, none".to_string()
}

/// The resolved intake policy: **one source of truth**, always present (the
/// built-in default when a repo declares nothing, or declares only part of
/// it), persisted in `group.json` beside `blocks` and read by every consumer
/// that needs "what counts as intake" — the template renderer, `gh.rs`'s
/// label allow-list, `idle_tick_notice()`, and the #332 host poller (P2-P4,
/// separate PRs; this struct is the shared contract they all read).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntakeProfile {
    pub source: IntakeSource,
    /// "Build this." The real repo label spelling in the built-in profile.
    pub ready: String,
    /// "Look, don't build."
    pub investigate: String,
    /// "Mine" — ownership marker.
    pub owned: String,
    /// Demo-gated. Optional in the sense that a repo may omit it (falls back
    /// to the built-in default) — every label field works this way, see
    /// [`sanitize_intake_label`].
    pub prototype: String,
    /// "Held by the human — do not start this" (#778). The one label whose
    /// meaning is a *veto* rather than a selector: under full autonomy the
    /// start default inverts (every open issue is eligible), and this is the
    /// boundary that stays opt-**out**. Vocabulary only, like every field
    /// here — it names which label the host poller must treat as a hold, and
    /// can never grant anything.
    pub hold: String,
}

impl Default for IntakeProfile {
    fn default() -> Self {
        builtin_intake_profile()
    }
}

/// The built-in `github-labels` profile — a checked-in, independent value,
/// not derived from anything else. This is the plan's dodge for the golden
/// self-reference trap (P2): the byte-golden fixture and this const are two
/// separate things that can each move, so a golden test comparing a live
/// render against frozen bytes catches either one drifting, instead of a
/// render-against-itself tautology.
///
/// Reproduces today's vocabulary exactly (`agent-ready` /
/// `agent-investigation` / `agent-managed` / `agent-prototype`) — the labels
/// this repo's own `orchestrator.md` prose and `gh.rs`'s `ALLOWED_LABELS`
/// hardcode today, pre-#382.
pub fn builtin_intake_profile() -> IntakeProfile {
    IntakeProfile {
        source: IntakeSource::GithubLabels,
        ready: "agent-ready".to_string(),
        investigate: "agent-investigation".to_string(),
        owned: "agent-managed".to_string(),
        prototype: "agent-prototype".to_string(),
        hold: "agent-hold".to_string(),
    }
}

/// **Is this string usable as an intake label?** The one rule, asked by every
/// caller that has to decide it — `sanitize_intake_label` below (the workflow
/// parser's arm, which turns a `None` into an author-facing error), the
/// `group.json` reader (which turns it into a per-field fallback), and `gh.rs`
/// at its own argv boundary (#2663). Returns the value itself on success, so a
/// caller never has to re-derive it.
///
/// [`sanitize_id`]'s alphabet — letters, digits, `-`, `_` — **plus a refusal of
/// a leading `-`**, and it is one function rather than three copies precisely
/// because those two halves had drifted: the parser refused `--force` while the
/// `group.json` reader accepted it, which was invisible for as long as a
/// group's own `intake.hold` reached prose surfaces only. #2663 gives that
/// field an argv, so the two rules have to be one rule.
///
/// **Rejected, not rewritten** (`Some(clean) if clean == v`), matching every
/// other user-authored identifier in this file: an author who wrote a label
/// with a space must see an error, not a silently different string their own
/// repo's labels no longer match.
pub fn usable_intake_label(raw: &str) -> Option<String> {
    let v = raw.trim();
    match sanitize_id(v) {
        Some(clean) if clean == v && !clean.starts_with('-') => Some(clean),
        _ => None,
    }
}

/// A single `intake.labels.<field>:` value. Sanitized like a block id
/// ([`sanitize_id`]) — the same conservative alphabet, because a label string
/// eventually reaches a `gh issue list --label` argument and a template
/// substitution. **Rejected, not rewritten**, matching every other
/// user-authored identifier in this file: an author who wrote a label with a
/// space must see an error, not a silently different string their own repo's
/// labels no longer match.
///
/// Empty (omitted) is not a rejection — it falls back to `fallback` (the
/// built-in default for that field), which is what lets a repo override
/// `intake.labels.ready:` alone and inherit the other four.
///
/// **A LEADING `-` is rejected on top of [`sanitize_id`]'s alphabet**, which
/// permits `-` freely (rev-648 NB4). A label is not only compared against
/// GitHub's — the hold spelling becomes a **positional** argument to
/// `gh label create <name> …`, and a positional beginning with a dash is read
/// by cobra as an unknown flag. That is not an injection (nothing is executed,
/// and the create fails loudly), but it is a class of value that can never
/// work. `--force` and `-x` are nonsense as label names for all five fields, so
/// nothing legitimate is lost. Interior and trailing dashes (`agent-hold`,
/// `do-not-touch`) are untouched.
///
/// That refusal lives in [`usable_intake_label`] rather than here, because this
/// arm is no longer the only place it has to hold: a hand-edited `group.json`
/// never met this parser, and #2663 routes its `intake.hold` to the same argv.
pub(super) fn sanitize_intake_label(field: &str, raw_val: &str, fallback: &str, errs: &mut Vec<String>) -> String {
    let v = raw_val.trim();
    if v.is_empty() {
        return fallback.to_string();
    }
    match usable_intake_label(v) {
        Some(clean) => clean,
        None => {
            errs.push(format!(
                "intake.labels.{field}: {v:?} is not a usable label (letters, digits, '-', '_'; \
                 and it may not begin with '-')"
            ));
            fallback.to_string()
        }
    }
}

/// A parsed, validated workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workflow {
    pub version: u32,
    pub name: String,
    /// The loomux version that last authored the file (the optional
    /// `authored_with:` key — the workflow pane in #223 writes it). Purely
    /// informational: it is **never** a validation error, whatever it says, and
    /// an old or unrecognized value must not stop a file from loading. Kept on
    /// the parsed workflow so nothing round-trips it away. (Langflow's
    /// `last_tested_version` is the same idea.)
    pub authored_with: String,
    pub blocks: Vec<Block>,
    pub edges: Vec<Edge>,
    pub gates: BTreeMap<String, Gate>,
    /// Intake source + label vocabulary (#382 P1). Always resolved — the
    /// built-in default when the file declares no `intake:` block at all, or
    /// only part of one.
    pub intake: IntakeProfile,
    /// Merge-queue policy (#581 §11.2). Always resolved; the default is
    /// **disabled**, which is what an absent `merge_queue:` block means.
    pub merge_queue: MergeQueuePolicy,
    /// Review-driver policy (#1778 §5.3). Always resolved; the default is
    /// **disabled**, which is what an absent `driver:` block means.
    pub driver: DriverPolicy,
    /// Named lock resources (#858), keyed by the repo's own name for each.
    /// **Empty** when the file declares no `resources:` block — which is what
    /// turns the lock tools off for the group entirely.
    pub resources: BTreeMap<String, ResourcePolicy>,
    /// Board policy — per-status WIP limits (#1175). Always resolved; the
    /// default carries **no limits at all**, which is what an absent `board:`
    /// block means.
    pub board: BoardPolicy,
    /// Delivery-triage policy (#3304 S1). Always resolved; the default is
    /// **disabled**, which is what an absent `triage:` block means.
    pub triage: TriagePolicy,
}

impl Workflow {
    pub fn block(&self, id: &str) -> Option<&Block> {
        self.blocks.iter().find(|b| b.id == id)
    }
}
