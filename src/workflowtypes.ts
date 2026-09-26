// The workflow model's TYPES and CONSTANTS (#222; split out of workflowmodel.ts by #3498 F2):
// the closed enums, the bounds tables, the schema interfaces, the finding shapes, and the
// small predicates over them. It imports nothing from the other workflow* modules, and
// every one of them imports its types and constants from HERE, never from the
// `workflowmodel.ts` barrel. That is what keeps the module graph a DAG. Design note:
// docs/design/workflows.md; module map: docs/design/architecture.md.

// ---------- the closed enums ----------

/** The capability classes. CLOSED, deliberately (#222 §2c): a workflow file may define
 *  unlimited personas but may never invent a capability — `kind` picks one of these
 *  and inherits its structural guarantees (a planner gets no worktree and no write tools;
 *  a reviewer may `gh pr review` but can never push). A repo file that could grant itself
 *  write access would be a footgun with `auto_ops` on and nobody watching.
 *
 *  Order mirrors the backend's `kind_names()` (workflow.rs), which is also the order
 *  `src/workflow-schema.json` declares — the two are compared as ordered arrays by
 *  `the_workflow_schema_manifest_matches_the_engines_values_defaults_and_bounds`.
 *
 *  `manager` (#1161) is a class the workflow file may name and `spawn_agent` may not:
 *  it is the human's own interface pane, opened for them rather than spawned by the
 *  orchestrator, and at most one may be declared. */
export const BLOCK_KINDS = ["orchestrator", "worker", "reviewer", "planner", "manager"] as const;
export type BlockKind = (typeof BLOCK_KINDS)[number];

export function isBlockKind(v: string): v is BlockKind {
  return (BLOCK_KINDS as readonly string[]).includes(v);
}

/** The agent CLIs a block may run. Mirrors the backend's `SUPPORTED_CLIS`
 *  (crates/loomux-engine/src/model.rs) — a block naming anything else is a
 *  finding, not a spawn.
 *
 *  `gemini` (#267 stage 2) and `opencode` (#722) are what make "run one reviewer
 *  on a different CLI/model than the worker" buy a genuinely different model
 *  family rather than a second Claude-family opinion — opencode most of all,
 *  since its model ids name a provider (`opencode/deepseek-v4-flash-free`) and
 *  reach a catalog no other adapter here can. Which CLIs can host which *kinds*
 *  is a separate question the backend owns (`CLI_CAPS` / `cli_can_host`):
 *  membership here is spawnability, not capability.
 *
 *  `codex` (#2515 C1) is the sharpest illustration of that last sentence. It is
 *  in this list — a `cli: codex` worker or orchestrator block spawns — and the
 *  backend still refuses a `codex` reviewer, planner or manager outright,
 *  because its
 *  only containment axis is an all-or-nothing `sandbox_mode`. A pane's
 *  validation surface must not try to reproduce that rule; it belongs to
 *  `cli_can_host`, which quotes the measured reason back. */
export const WORKFLOW_CLIS = ["claude", "copilot", "gemini", "opencode", "pi", "codex"] as const;
export type WorkflowCli = (typeof WORKFLOW_CLIS)[number];

export function isWorkflowCli(v: string): v is WorkflowCli {
  return (WORKFLOW_CLIS as readonly string[]).includes(v);
}

/** The role hints a block may declare (#250/#324, #891) — an OPTIONAL persona/
 *  template/badge marker. `kind` alone decides the deny-flags and the cwd rule;
 *  the MCP tool scope has a short enumerated list of hint-keyed exceptions
 *  (`docs/design/liaison.md`): two narrowing — `session_digest` to `process`,
 *  `review_verdict` away from `liaison` — and TWO widening toward that same
 *  `liaison`, both otherwise orchestrator-only: `group_usage`, and `ask_human`
 *  (the pose only — nothing answers on the human's behalf, and
 *  `withdraw_question` is deliberately not widened with it).
 *
 *  `liaison` is SUPERSEDED by `kind: manager`, the first-class human-interface
 *  class. It still parses and still runs, unchanged, and the exceptions above
 *  are still exactly what it gets; a manager is a capability class of its own
 *  with its own tools and its own structural rules. `validateWorkflow` warns on
 *  one so a file's author is told where the feature moved.
 *  Mirrors the backend's `role_hint_requires` (workflow.rs) so this pane's
 *  pre-run pass agrees with what the real parser would say. Each hint REQUIRES a
 *  specific `kind` — `advisor` needs `planner`, `process` needs `worker`,
 *  `liaison` needs `reviewer` — so a workflow can't spell a combination nothing
 *  downstream would honor. */
export const ROLE_HINTS = ["advisor", "process", "liaison"] as const;
export type RoleHint = (typeof ROLE_HINTS)[number];

/** The capability class a role_hint REQUIRES, or `undefined` for an unrecognized
 *  value — the caller turns that into a `role-hint-unknown` finding, the same
 *  "reject, never coerce" shape `isBlockKind` uses for `kind`.
 *
 *  Trims and lowercases before comparing, to match the backend's
 *  `role_hint_requires` (`raw.trim().to_ascii_lowercase()`, workflow.rs) — a file
 *  with `role_hint: Advisor` parses clean on the real engine, and this pane's
 *  pre-run validation pass must not disagree and flag it as unknown. */
export function roleHintRequires(hint: string): BlockKind | undefined {
  const h = hint.trim().toLowerCase();
  if (h === "advisor") return "planner";
  if (h === "process") return "worker";
  if (h === "liaison") return "reviewer";
  return undefined;
}

/** The role hints a block of THIS kind may legally declare — derived from
 *  {@link ROLE_HINTS} and {@link roleHintRequires}, never listed a second time.
 *
 *  That derivation is the whole point (#1020): the form that offers these must not
 *  be able to spell something the parser rejects, and the only way to guarantee it
 *  is for the offer and the rule to be the SAME statement. A hint added to
 *  `ROLE_HINTS` (with its pairing in `roleHintRequires`) shows up here — and so in
 *  the pane — with no edit at all; a hardcoded picker would silently keep offering
 *  the old two while `validateWorkflow` had moved on. */
export function roleHintsForKind(kind: string): RoleHint[] {
  return ROLE_HINTS.filter((h) => roleHintRequires(h) === kind);
}

/** Why a block of this kind may NOT declare a repo-authored PERSONA
 *  (`prompt:` / `profile:`), or `null` when it may — the mirror of
 *  `workflow::persona_allowed` and of `parse_workflow`'s refusal.
 *
 *  Separate from {@link allowDenialReason} because the two rules are not
 *  co-extensive and never were: a PLANNER may carry a persona and may not
 *  pre-approve tools, so folding them into one predicate would either ban a
 *  planner's persona or permit an orchestrator's. Two loomux-owned classes
 *  answer non-null here (#222 for the orchestrator, #1161 D1 for the manager);
 *  the argument for each is in `parse_workflow`.
 *
 *  Before #1161 the pane mirrored only the `allow:` half, so a workflow the
 *  engine refuses OUTRIGHT — persona on the trust root — could be authored in
 *  the pane, saved, and reported as clean; the launch then fell back to the
 *  built-in roster with no finding to explain why. Fail-closed, but silent, and
 *  `kind: manager` made it newly reachable through the kind picker. */
export function personaDenialReason(kind: string): string | null {
  if (kind === "orchestrator") {
    return (
      "the orchestrator is orrerix's trust root, and a repo file may not author its prompt — " +
      "put personas on the blocks it spawns"
    );
  }
  if (kind === "manager") {
    return (
      "a manager speaks to the human and relays their direction into the trust root, so a repo " +
      "file may not author its persona — put personas on the blocks the orchestrator spawns"
    );
  }
  return null;
}

/** Why a block of this kind may NOT declare `allow:`, or `null` when it may.
 *
 *  Mirrors the three REFUSALS in `parse_workflow` (workflow.rs), which are separate
 *  rules with separate reasons and are stated here as one answer so the pane's form
 *  and its validation pass cannot disagree about them:
 *
 *   - an ORCHESTRATOR block may not declare `prompt:`/`profile:`/`allow:` at all —
 *     it is the group's trust root, and a repo file that could pre-approve its
 *     tools would be a prompt-injection seam into the one agent running
 *     unsupervised;
 *   - a MANAGER block may not either (#1161, decision D1) — its whole output surface is
 *     persuading the human and relaying their direction into that same trust root, so a
 *     repo-authored persona there would launder the repo's own instructions into what the
 *     human is told;
 *   - a READ-ONLY class (today: `planner`, via `Role::containment`) may not, because
 *     `allow: Bash(python *)` hands it a shell that writes files while naming
 *     nothing on the deny list. Reviewers and workers keep `allow:` — a reviewer has
 *     its shell by design (running the tests is the job).
 *
 *  An UNRECOGNIZED kind answers `null`: `unknown-kind` already says what is wrong
 *  with that block, and stacking a second finding on top of it explains nothing. */
export function allowDenialReason(kind: string): string | null {
  if (kind === "orchestrator") {
    return (
      "the orchestrator is orrerix's trust root, and a repo file may not pre-approve its tools — " +
      "put personas and allow: patterns on the blocks it spawns"
    );
  }
  if (kind === "manager") {
    return (
      "a manager speaks to the human and relays their direction into the trust root, so a repo " +
      "file may not author its persona or pre-approve its tools — put personas and allow: " +
      "patterns on the blocks the orchestrator spawns"
    );
  }
  if (kind === "planner") {
    return (
      "a planner's class is read-only, and a pre-approved tool pattern could hand it a shell " +
      "that writes files — move the work to a worker block"
    );
  }
  return null;
}

/** Longest `remote:` label the engine accepts — `pathseg::MAX_SEGMENT_LEN`
 *  (crates/loomux-engine/src/pathseg.rs), the shared #925 identifier cap the
 *  engine validates a remote label with.
 *
 *  A hand-written mirror, like {@link WORKFLOW_CLIS}, and safe for the same
 *  reason — **engine -> manifest -> pane, with no step left to assumption**:
 *  `workflow_schema_field_facts()` states `block.remote.maxLength` from
 *  `MAX_SEGMENT_LEN` itself,
 *  `the_workflow_schema_manifest_matches_the_engines_values_defaults_and_bounds`
 *  pins `src/workflow-schema.json` against that fact in both directions, and
 *  `test/workflowschema.test.ts` pins THIS constant against the manifest row.
 *  Raise `MAX_SEGMENT_LEN` and the Rust test reddens; change this number alone
 *  and the pane test does.
 *
 *  It was NOT in that chain when the key shipped (#1457 review B3): the engine
 *  stated no fact for `block.remote`, so the manifest declared none, so the
 *  bidirectional pin had nothing to compare and this constant was pinned to
 *  nothing while a comment here said otherwise. Both sides tested their cap
 *  against their own constant, which is green and tautological at once. */
export const REMOTE_LABEL_MAX = 64;

/** Whether a `remote:` label is one the engine will accept — the pane's mirror of
 *  `pathseg::check_segment`, which `parse_workflow` validates the key with (#1457).
 *
 *  `[A-Za-z0-9_-]`, non-empty, at most {@link REMOTE_LABEL_MAX}, no leading `-`
 *  (a bare `-foo` is an OPTION to any command line the label is interpolated
 *  into), and no Windows reserved device name. REFUSED, never rewritten: two
 *  spellings must not be able to name one operator binding.
 *
 *  Not the same predicate as an id's: `sanitize_id` REWRITES, which is exactly
 *  what a label may not do. */
export function isRemoteLabel(label: string): boolean {
  if (!label || label.length > REMOTE_LABEL_MAX) return false;
  if (!/^[A-Za-z0-9_-]+$/.test(label)) return false;
  if (label.startsWith("-")) return false;
  const stem = (label.split(".")[0] ?? "").toUpperCase();
  return !/^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$/.test(stem);
}

/** Why a block of this kind may NOT declare `remote:`, or `null` when it may
 *  (#1457) — the mirror of `parse_workflow`'s refusal.
 *
 *  The same two loomux-owned classes {@link personaDenialReason} answers for, and
 *  for a related but distinct reason: those two blocks are load-bearing LOCALLY.
 *  The orchestrator is the trust root and holds orchestration state, the `gh`
 *  operations and the merge gate; the manager pane is the human's own interface —
 *  the thing they type into. Neither is a remote-execution feature with a missing
 *  implementation.
 *
 *  An UNRECOGNIZED kind answers `null`, exactly like {@link allowDenialReason}:
 *  `unknown-kind` already says what is wrong with that block. */
export function remoteDenialReason(kind: string): string | null {
  if (kind === "orchestrator") {
    return (
      "the orchestrator is orrerix's trust root — orchestration state, the gh operations and " +
      "the merge gate stay on this machine — so put remote: on the blocks it spawns"
    );
  }
  if (kind === "manager") {
    return (
      "a manager pane is the human's own interface, the thing they type into, so it runs where " +
      "they are — put remote: on the blocks the orchestrator spawns"
    );
  }
  return null;
}

/** What the engine will actually apply for one `allow:` entry, or `null` when it
 *  drops the entry entirely. Mirrors `sanitize_allow` (profiles.rs): everything
 *  outside its alphabet is FILTERED OUT — silently, on the way to the CLI's
 *  `--allowedTools` / `--allow-tool` flag — so a pattern carrying a `$`, a `|` or a
 *  quote reaches the agent as a different pattern than the one in the file, and the
 *  human never hears about it. The pane says so instead (`allow-sanitized`). */
export function sanitizeAllowPattern(pattern: string): string | null {
  const cleaned = pattern
    .trim()
    .replace(/[^A-Za-z0-9():*_\-. ,/]/g, "")
    .trim();
  return cleaned || null;
}

/** Does this block actually REVIEW PRs? — reviewer-kind, minus the liaison
 *  (#891). Mirrors the backend's `is_reviewing_block` (workflow.rs), which the
 *  `{{REVIEWERS}}` fan-out, a reviewer's "one of N" lane and the class-default
 *  block resolution all ask.
 *
 *  `kind === "reviewer"` answers "which capability class does it ride", and that
 *  stops being the same question once a hint SUBTRACTS from its class: a liaison
 *  rides the reviewer posture and reviews nothing, is denied `review_verdict`,
 *  and is refused outright by `validateWorkflow` when a merge gate names it. So
 *  anywhere this pane means "the blocks that can satisfy a gate" — offering the
 *  gate's reviewer checkboxes, filling them in when the gate is switched on —
 *  asks THIS, or the editor writes a file its own validator immediately flags. */
export function isReviewingBlock(b: { kind?: string; role_hint?: string }): boolean {
  return b.kind === "reviewer" && b.role_hint?.trim().toLowerCase() !== "liaison";
}

/** The schema version this build reads and writes. */
export const WORKFLOW_VERSION = 1;

/** The repo's committed orrerix config dir, relative to the repo root. */
export const CONFIG_DIR = ".orrerix";

/** The pre-#1153 spelling of {@link CONFIG_DIR}, still read when `.orrerix/` is absent.
 *  NEVER renamed on the user's behalf — it is a tracked directory in their repository.
 *  See `docs/design/rebrand-filesystem.md`. */
export const LEGACY_CONFIG_DIR = ".loomux";

/** Where the workflow lives, relative to the repo root. */
export const WORKFLOW_FILE = `${CONFIG_DIR}/workflow.yml`;

/** The legacy location, tried when {@link WORKFLOW_FILE} is not there. */
export const LEGACY_WORKFLOW_FILE = `${LEGACY_CONFIG_DIR}/workflow.yml`;

/** After reading `tried` came back NOT-FOUND, the next path worth trying — or null when
 *  there is nothing left to try.
 *
 *  Deliberately narrow: only the DEFAULT workflow path falls back. A pane opened on an
 *  explicit file (the editor's `getFile`, a restored tab) is showing the file it was asked
 *  to show, and silently opening a different one because that one is missing would be the
 *  pane lying about what it has open. And the fallback happens exactly once — `tried` is
 *  the legacy path on the second call, which returns null — so a repo with neither file
 *  lands on the "no workflow yet" empty state after two reads, not a loop. */
export function legacyFallbackFor(tried: string): string | null {
  return tried === WORKFLOW_FILE ? LEGACY_WORKFLOW_FILE : null;
}

// ---------- named workflows (#1689): a repo may declare several ----------
//
// `.orrerix/workflow.yml` is the workflow named `default` and is unchanged in every
// respect. `.orrerix/workflows/<name>.yml` is the workflow named `<name>`, same schema,
// same parser, its own layout sidecar (`workflowlayout.ts`). A repo that has never made
// the directory behaves byte-for-byte as it did before, because nothing here opens it.
//
// The NAME RULE below mirrors the engine's, and mirroring is the whole of its job: the
// backend refuses a name `loomux_engine::pathseg::check_segment` rejects, so a frontend
// that accepted more would offer the human a workflow no launch could ever run, and one
// that accepted less would hide a file that is on disk and valid. Pinned against the
// engine's own alphabet in `test/workflowvalidate.test.ts`, the way `workflowschema.test.ts`
// pins the schema manifest.

/** Where a repo's NAMED workflows live, relative to the repo root. */
export const WORKFLOWS_DIR = `${CONFIG_DIR}/workflows`;

/** The pre-#1153 spelling of {@link WORKFLOWS_DIR}, read when `.orrerix/workflows/` is
 *  absent — the same rule {@link LEGACY_WORKFLOW_FILE} rides, and never renamed for the
 *  user. */
export const LEGACY_WORKFLOWS_DIR = `${LEGACY_CONFIG_DIR}/workflows`;

/** The name `.orrerix/workflow.yml` is listed under, and the workflow a group runs when
 *  nothing says otherwise — including every group launched before named workflows existed,
 *  whose `group.json` carries no name at all. */
export const DEFAULT_WORKFLOW_NAME = "default";

/** Longest accepted workflow name — `MAX_SEGMENT_LEN` in the engine. */
export const WORKFLOW_NAME_MAX = 64;

/** Windows device names, which are not filenames at all: a path naming one opens a device.
 *  Reserved with any extension, so `con.yml` is out too — which is why the check below is on
 *  the NAME rather than on the assembled filename. */
const RESERVED_DEVICE_NAMES: readonly string[] = [
  "con", "prn", "aux", "nul",
  "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9",
  "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/** Is `v` a usable workflow name?
 *
 *  `[A-Za-z0-9_-]`, non-empty, at most {@link WORKFLOW_NAME_MAX} bytes, no leading `-`, and
 *  not a Windows device name. Refused, NEVER rewritten — the engine's rule verbatim, and
 *  rewriting is the specific thing it forbids, because two spellings that normalize to one
 *  name are two files claiming one workflow.
 *
 *  The alphabet is what makes the path-shaped attacks unspellable rather than enumerated:
 *  no `.` (so `..` cannot be written), no `/` or `\` (so a name is one component), no `:`
 *  (so no drive letter and no NTFS stream). The two rules that do not fall out of it are a
 *  leading `-` — path-safe, but an option to any command line the name reaches — and the
 *  device names above. */
export function isWorkflowName(v: string): boolean {
  if (!v || v.length > WORKFLOW_NAME_MAX) return false;
  if (!/^[A-Za-z0-9_-]+$/.test(v)) return false;
  if (v.startsWith("-")) return false;
  return !RESERVED_DEVICE_NAMES.includes(v.toLowerCase());
}

/** The repo-relative file one workflow name resolves to, or `null` when the name is not a
 *  usable one — which is a refusal, not a path to be sanitized into existence.
 *
 *  `default` is `.orrerix/workflow.yml`, the file that has always been there; `legacy` picks
 *  the `.loomux/` spelling for a repo that uses it. WHICH spelling a repo uses is a fact
 *  about the disk, so the caller passes it (the backend's listing reports the resolved path
 *  per entry) rather than this module guessing from a name.
 *
 *  Note the asymmetry with the backend for `default`: the engine also tolerates
 *  `workflows/default.yml` when there is no plain file. This function always names the plain
 *  file, because its callers are the ones that OPEN a file for editing or CREATE a missing
 *  one, and `.orrerix/workflow.yml` is where a repo's default workflow belongs. A repo that
 *  really has only `workflows/default.yml` reaches its file through the listing's own
 *  `path`, which is the value the picker carries. */
export function workflowRelFor(name: string, opts?: { legacy?: boolean }): string | null {
  if (!isWorkflowName(name)) return null;
  const legacy = opts?.legacy === true;
  if (name === DEFAULT_WORKFLOW_NAME) return legacy ? LEGACY_WORKFLOW_FILE : WORKFLOW_FILE;
  return `${legacy ? LEGACY_WORKFLOWS_DIR : WORKFLOWS_DIR}/${name}.yml`;
}

/** The workflow name a repo-relative path denotes, or `null` when the path is not one of
 *  this repo's workflow files.
 *
 *  The inverse of {@link workflowRelFor}, and deliberately strict: it answers only for the
 *  two shapes that path really can be — a `workflow.yml` directly under a config dir, and a
 *  `<name>.yml` directly under a workflows dir. Anything else (a workflow opened from an
 *  arbitrary path, a nested directory, a `.yaml` spelling the repo may still parse) has no
 *  name, and saying so is the honest answer: a pane showing such a file is showing a FILE,
 *  and calling it `default` would tell the human it is the one their group runs. */
export function workflowNameOf(rel: string): string | null {
  const parts = rel.split(/[\\/]/).filter((p) => p !== "" && p !== ".");
  const file = parts.pop();
  if (!file) return null;
  const dir = parts.join("/");
  if (file === "workflow.yml") {
    return dir === CONFIG_DIR || dir === LEGACY_CONFIG_DIR ? DEFAULT_WORKFLOW_NAME : null;
  }
  if (dir !== WORKFLOWS_DIR && dir !== LEGACY_WORKFLOWS_DIR) return null;
  if (!file.endsWith(".yml")) return null;
  const name = file.slice(0, -".yml".length);
  return isWorkflowName(name) ? name : null;
}

/** What a `merge` gate can require of its reviewers. `all-pass` = every named reviewer
 *  recorded PASS; `threshold` = at least N of them did. These are the CANONICAL
 *  spellings — what the pane offers and what it writes. */
export const GATE_REQUIRES = ["all-pass", "threshold"] as const;
export type GateRequire = (typeof GATE_REQUIRES)[number];

/** What the ENGINE accepts, which is a superset: `all` is a synonym for `all-pass` in
 *  `parse_workflow`'s gate match (workflow.rs). Validation reads THIS list, so a
 *  hand-written file using the synonym stops being flagged as an error it never was —
 *  a pane that refuses a file the engine loads is telling the same class of lie as one
 *  that blesses a file the engine refuses, just in the other direction (#880 review).
 *  The picker still offers `GATE_REQUIRES` only: there is no reason to offer a human
 *  two spellings of one thing. */
export const GATE_REQUIRES_ACCEPTED = ["all-pass", "all", "threshold"] as const;

/** Where autonomous work comes from (#382 P1). Mirrors the engine's
 *  `intake_source_from_str` (workflow.rs). An EMPTY `source:` is legal and means the
 *  built-in default, which is why "" is accepted by {@link isIntakeSource} but is not
 *  offered as a value: "inherit" is a different statement from "github-labels", and a
 *  picker that spelled it out would PIN what the file meant to inherit. */
export const INTAKE_SOURCES = ["github-labels", "board", "none"] as const;
export type IntakeSourceName = (typeof INTAKE_SOURCES)[number];

export function isIntakeSource(v: string): boolean {
  const s = v.trim().toLowerCase();
  return s === "" || (INTAKE_SOURCES as readonly string[]).includes(s);
}

/** How long an identifier the engine's `sanitize_id` will carry (`MAX_ID_CHARS`,
 *  workflow.rs). Longer is not truncated — it is REJECTED, because a resource called
 *  something the author didn't write is worse than one that fails to load. */
export const ID_MAX_CHARS = 48;

/** A label the intake profile may name. The engine rejects rather than rewrites
 *  (`sanitize_intake_label`): a label the repo's own GitHub labels no longer match is
 *  a silent no-op, so a space or a `#` is a hard error there and a finding here.
 *  A LEADING `-` is banned on top of the id alphabet — the hold spelling becomes a
 *  positional argument to `gh label create`, and a dash-leading positional is read as
 *  a flag. Empty is not a rejection: it means "inherit this one". */
export function isValidIntakeLabel(v: string): boolean {
  const s = v.trim();
  if (!s) return true;
  return s.length <= ID_MAX_CHARS && /^[A-Za-z0-9_][A-Za-z0-9_-]*$/.test(s);
}

/** A resource name (#858). Same alphabet as a block id at the engine
 *  (`sanitize_id`), and — like every other author-written identifier in that file —
 *  rejected rather than rewritten, so `heavy build` never becomes a resource called
 *  `heavybuild` that the author's own `acquire_lock` call cannot name. Unlike an
 *  intake label, a leading `-` is fine: nothing puts a resource name in an argv. */
export function isValidResourceName(v: string): boolean {
  const s = v.trim();
  return !!s && s.length <= ID_MAX_CHARS && /^[A-Za-z0-9_-]+$/.test(s);
}

/** The bounds `parse_workflow` enforces on the policy sections — mirrored here so the
 *  pane's forms cannot write a file the engine then refuses to load, and so a
 *  hand-written file that already carries one gets a finding rather than a clean bill
 *  of health. Every one of these is a REFUSAL on the engine (`RESOURCE_SLOTS_MAX`,
 *  `RESOURCE_MAX_HOLD_MINUTES_MAX`, `RESOURCES_MAX`, and `max_batch`'s floor of 1),
 *  with the single exception noted on the checks-timeout pair below. */
export const RESOURCE_SLOTS_MIN = 1;
export const RESOURCE_SLOTS_MAX = 64;
export const RESOURCE_MAX_HOLD_MINUTES_MIN = 1;
export const RESOURCE_MAX_HOLD_MINUTES_MAX = 480;
export const RESOURCES_MAX = 32;
export const MERGE_QUEUE_MAX_BATCH_MIN = 1;
/** A threshold gate needs at least one passing review — `fact("gate.threshold", "min", 1)`
 *  on the engine, and the floor `validateWorkflow` has always enforced. */
export const GATE_THRESHOLD_MIN = 1;
/** The small-batch clause's floor — `fact("gate.max_diff_lines", "min", 1)` on the
 *  engine. `0` is refused rather than read as "unlimited": the way to mean no limit is
 *  to omit the key, and a bound that bounds nothing is a typo (#1174). */
export const GATE_MAX_DIFF_LINES_MIN = 1;
/** #1176's caps — `fact("gate.routing", "max_entries", …)` and
 *  `fact("gate.routing.paths", "max_entries", …)` on the engine. Bounds on work the
 *  `gh` shim does on the merge path (every rule against every changed file), not on
 *  what this form can render. */
export const GATE_ROUTING_RULES_MAX = 32;
export const GATE_ROUTING_PATHS_MAX = 32;
/** Longest path glob the engine accepts (`MAX_GLOB_CHARS`) — past it `sanitize_glob`
 *  truncates, which the parse reads as a glob it had to change, which is a refusal. */
export const ROUTING_GLOB_MAX_CHARS = 200;

/** Is this a path glob the engine would accept — `sanitize_glob`, mirrored.
 *
 *  The alphabet is what a POSIX `case` pattern can carry with `*` as its ONLY
 *  metacharacter: no `[`, `\\`, `?`, brace, quote or space. The three refused shapes
 *  are the ones that could never fire against a repo-relative changed path, and a
 *  rule that never fires silently drops a reviewer the repo asked for. */
export function isRoutingGlob(p: string): boolean {
  return (
    p.length > 0 &&
    p.length <= ROUTING_GLOB_MAX_CHARS &&
    /^[A-Za-z0-9._\-/*]+$/.test(p) &&
    !p.startsWith("/") &&
    !p.endsWith("/") &&
    !p.split("/").includes("..")
  );
}
/** `checks_timeout_minutes` rides the notify-TTL clamp (`clamp_expires_minutes`) -
 *  one of the policy numbers the engine CLAMPS rather than refuses (the driver's
 *  three backstops are the others, #1778) - so a value outside this range is a
 *  warning here, not an error: the file loads, it just doesn't do what it says. */
export const MERGE_QUEUE_CHECKS_TIMEOUT_MIN = 5;
export const MERGE_QUEUE_CHECKS_TIMEOUT_MAX = 240;
/** INVARIANT 9's counters (#1778 §2.3) - the engine REFUSES values outside these
 *  closed ranges rather than clamping: a repo file may run a tighter review loop
 *  than the orchestrator template promises, never a looser one. */
export const DRIVER_MAX_REVIEW_ROUNDS_MIN = 1;
export const DRIVER_MAX_REVIEW_ROUNDS_MAX = 3;
export const DRIVER_MAX_CI_ATTEMPTS_MIN = 1;
export const DRIVER_MAX_CI_ATTEMPTS_MAX = 3;
export const DRIVER_MAX_REBASE_ATTEMPTS_MIN = 0;
export const DRIVER_MAX_REBASE_ATTEMPTS_MAX = 1;
/** Two of the driver's three backstops ride the same notify-TTL clamp family the
 *  merge queue's checks timeout rides (`clamp_expires_minutes`) - CLAMPED, not
 *  refused: outside this range is a warning here, not an error. */
export const DRIVER_TIMEOUT_MIN = 5;
export const DRIVER_TIMEOUT_MAX = 240;
/** The PLAN driver's two minute knobs (#3040 §2(c)/(e)). Both are REFUSED
 *  outside their range rather than clamped, so they sit with the counters above
 *  rather than with the notify-TTL waits: a repo asking for a five-minute
 *  planner timeout has misunderstood what a planner does, and silently handing
 *  it fifteen would leave that misunderstanding in place while the behaviour
 *  changed underneath it. 0 review minutes is legal and is the default - it is
 *  "no window", which is a real answer rather than a missing one. */
export const DRIVER_PLAN_REVIEW_MINUTES_MIN = 0;
export const DRIVER_PLAN_REVIEW_MINUTES_MAX = 120;
export const DRIVER_PLANNER_TIMEOUT_MINUTES_MIN = 15;
export const DRIVER_PLANNER_TIMEOUT_MINUTES_MAX = 180;
/** `fix_nonblocking_rounds` (#3367): the driver's own non-blocking rounds.
 *  Refused out of range like the INVARIANT 9 counters, and capped at the same
 *  three, because every such round is also a review round. 0 is the default and
 *  means off. */
export const DRIVER_FIX_NONBLOCKING_ROUNDS_MIN = 0;
export const DRIVER_FIX_NONBLOCKING_ROUNDS_MAX = 3;
/** `drive_timeout_minutes` left that family in #2110 and carries its own range.
 *
 *  It stopped being the same quantity. The two above bound ONE wait on ONE
 *  fallible signal, which is what a notify TTL is; this is the last-resort bound
 *  over a whole drive, and since #2110 there are four per-state bounds beneath it
 *  doing the work. A backstop measured in the same hours as the waits under it is
 *  the one clock a drive making steady progress can still trip - which is exactly
 *  what happened at four hours. Twelve hours is the first figure that cannot be an
 *  honest review loop; the ceiling of one day bounds the field at something a
 *  human would recognise as a mistake. Mirrors `DRIVER_DRIVE_TIMEOUT_*` in
 *  `crates/loomux-engine/src/workflow.rs`, and `workflow-schema.json` beside it. */
export const DRIVER_DRIVE_TIMEOUT_MIN = 5;
export const DRIVER_DRIVE_TIMEOUT_MAX = 1440;

/** The board statuses a `board.wip:` cap may name (#1175), in board order — the pane's
 *  mirror of the engine's `RawWip` fields. `done` is deliberately absent: it is the
 *  relief valve every other cap depends on, so the engine has no field for it and
 *  `deny_unknown_fields` refuses one. */
export const WIP_STATUSES = [
  "queued",
  "in-progress",
  "review",
  "pr",
  "prototype",
  "human-testing",
  "blocked",
] as const;

/** `parse_workflow` refuses `0` — a cap of nothing is a stop, not a limit. */
export const WIP_LIMIT_MIN = 1;

/** `triage.max_defer_minutes` (#3304 S1) — the engine's own closed range, REFUSED
 *  outside rather than clamped, so a form must stop the submit rather than coerce. */
export const TRIAGE_MAX_DEFER_MINUTES_MIN = 1;
export const TRIAGE_MAX_DEFER_MINUTES_MAX = 240;

/** A block's `cache_ttl_minutes:` (#3407) — `0` is legal ("unknown"), and the
 *  engine REFUSES above `cacheage::CACHE_TTL_MINUTES_MAX` (one day). */
export const BLOCK_CACHE_TTL_MINUTES_MIN = 0;
export const BLOCK_CACHE_TTL_MINUTES_MAX = 1440;

/** One numeric field's range. `max` is OPTIONAL and its absence is a statement: the
 *  engine imposes no ceiling on that field, so neither may a form. */
export interface FieldBounds {
  min: number;
  max?: number;
}

/** Every bounded number a policy form writes, keyed by its manifest field id.
 *
 *  This table exists because a bound that lives as a literal at the point of use is a
 *  bound nothing can check (#1020 review, finding 2): `merge_queue.max_batch` was clamped
 *  to a hand-typed `64` in the form — a ceiling the engine does not impose and the
 *  manifest does not declare — so typing `100` silently wrote `64` into the file, and no
 *  test in the tree could see it. The table is the fix in kind rather than in degree: the
 *  forms read their bounds from HERE, `test/workflowschema.test.ts` pins every entry
 *  against `src/workflow-schema.json` (whose own `min`/`max` the Rust side pins against
 *  the engine's constants), and it pins in BOTH directions — a manifest bound missing from
 *  this table, and a table bound the manifest does not declare, both redden.
 *
 *  So "engine → manifest → pane, with no step left to assumption" is now enforced for the
 *  numbers rather than asserted about them: a `max` cannot enter a form without first
 *  existing in the engine. */
export const POLICY_BOUNDS: Readonly<Record<string, FieldBounds>> = {
  // No ceiling here either: a gate may name any number of reviewers, so `threshold` is
  // bounded above by the reviewer list rather than by a constant — which
  // `validateWorkflow` checks against the list itself, where the real answer is.
  "gate.threshold": { min: GATE_THRESHOLD_MIN },
  // No ceiling: "how big is too big" is the repo's call, and the engine invents no
  // upper bound for it either (#1174).
  "gate.max_diff_lines": { min: GATE_MAX_DIFF_LINES_MIN },
  // No ceiling, deliberately: `parse_workflow` refuses `max_batch: 0` and accepts every
  // integer above it, so the form must too.
  "merge_queue.max_batch": { min: MERGE_QUEUE_MAX_BATCH_MIN },
  "merge_queue.checks_timeout_minutes": {
    min: MERGE_QUEUE_CHECKS_TIMEOUT_MIN,
    max: MERGE_QUEUE_CHECKS_TIMEOUT_MAX,
  },
  "driver.max_review_rounds": {
    min: DRIVER_MAX_REVIEW_ROUNDS_MIN,
    max: DRIVER_MAX_REVIEW_ROUNDS_MAX,
  },
  "driver.max_ci_attempts": { min: DRIVER_MAX_CI_ATTEMPTS_MIN, max: DRIVER_MAX_CI_ATTEMPTS_MAX },
  "driver.max_rebase_attempts": {
    min: DRIVER_MAX_REBASE_ATTEMPTS_MIN,
    max: DRIVER_MAX_REBASE_ATTEMPTS_MAX,
  },
  "driver.lane_timeout_minutes": { min: DRIVER_TIMEOUT_MIN, max: DRIVER_TIMEOUT_MAX },
  "driver.fix_timeout_minutes": { min: DRIVER_TIMEOUT_MIN, max: DRIVER_TIMEOUT_MAX },
  "driver.drive_timeout_minutes": {
    min: DRIVER_DRIVE_TIMEOUT_MIN,
    max: DRIVER_DRIVE_TIMEOUT_MAX,
  },
  "driver.plan_review_minutes": {
    min: DRIVER_PLAN_REVIEW_MINUTES_MIN,
    max: DRIVER_PLAN_REVIEW_MINUTES_MAX,
  },
  "driver.planner_timeout_minutes": {
    min: DRIVER_PLANNER_TIMEOUT_MINUTES_MIN,
    max: DRIVER_PLANNER_TIMEOUT_MINUTES_MAX,
  },
  "driver.fix_nonblocking_rounds": {
    min: DRIVER_FIX_NONBLOCKING_ROUNDS_MIN,
    max: DRIVER_FIX_NONBLOCKING_ROUNDS_MAX,
  },
  "resource.slots": { min: RESOURCE_SLOTS_MIN, max: RESOURCE_SLOTS_MAX },
  "block.cache_ttl_minutes": { min: BLOCK_CACHE_TTL_MINUTES_MIN, max: BLOCK_CACHE_TTL_MINUTES_MAX },
  "resource.max_hold_minutes": {
    min: RESOURCE_MAX_HOLD_MINUTES_MIN,
    max: RESOURCE_MAX_HOLD_MINUTES_MAX,
  },
  // No ceiling on any WIP cap, and one row per status rather than seven typed out:
  // a limit above the board's own size degenerates to "no limit", which is what the
  // author asked for, so there is nothing to refuse — and spreading `WIP_STATUSES`
  // means an eighth cappable status cannot arrive here bound-less.
  ...Object.fromEntries(
    WIP_STATUSES.map((s) => [`board.wip.${s}`, { min: WIP_LIMIT_MIN }] as const)
  ),
  "triage.max_defer_minutes": {
    min: TRIAGE_MAX_DEFER_MINUTES_MIN,
    max: TRIAGE_MAX_DEFER_MINUTES_MAX,
  },
};

/** A legal block id: lowercase-ish, human-meaningful, safe as a filename fragment and as
 *  a shell-adjacent token. Deliberately strict — the id ends up in agent ids, pane names
 *  and (via the backend) command lines, and `sanitize_model` (mod.rs) is the precedent
 *  for keeping repo-authored strings out of a shell line. */
const BLOCK_ID_RE = /^[a-z][a-z0-9]*(?:[-_][a-z0-9]+)*$/;

export function isValidBlockId(id: string): boolean {
  return BLOCK_ID_RE.test(id);
}

// ---------- the schema ----------

/** Any value the YAML subset can hold. Blocks keep the keys they don't understand
 *  (`extra`) so a file written by a NEWER loomux survives a round-trip through an older
 *  pane instead of being silently stripped by it. */
export type YamlValue = string | number | boolean | null | YamlValue[] | { [k: string]: YamlValue };

/** One agent block: a persona (prompt or profile), a model, a CLI — and a `kind` that
 *  says which capability class it runs as.
 *
 *  `kind` and `cli` are typed as raw strings, not as the enums: a file naming
 *  `kind: superuser` must still LOAD (as a stub, with a finding) so the human can fix it
 *  in the pane. Narrowing them at the type level would force the parser to drop the very
 *  block the user needs to see. */
export interface WorkflowBlock {
  /** Immutable identity. Edges and gates reference THIS (never `name`). */
  id: string;
  /** Display label. Renaming it must never break a reference — that is its whole point. */
  name: string;
  /** One of BLOCK_KINDS; anything else is a finding + a stub. */
  kind: string;
  /** One of WORKFLOW_CLIS; anything else is a finding + a stub. */
  cli: string;
  /** Model to pin, or "" for the CLI's default. */
  model: string;
  /** Inline persona — compiled to `claude --agents '<json>'` (native, no file needed). */
  prompt?: string;
  /** Persona file — compiled to `copilot --agent <name>` against `.github/agents/`.
   *  Mutually exclusive with `prompt` (a block with both is a finding). */
  profile?: string;
  /** OPTIONAL persona/template marker (#250/#324, #891) — one of {@link ROLE_HINTS},
   *  or anything else (a finding + a stub, same as an unrecognized `kind`). Requires
   *  its matching `kind` (see {@link roleHintRequires}); absent is today's behavior,
   *  byte for byte. */
  role_hint?: string;
  /** Extra pre-approved tool patterns (`--allowedTools` / `--allow-tool`), e.g.
   *  `Bash(gh pr view --json title,body)`. A `RawBlock` field since #222 that this
   *  model never knew about until #880 — so a workflow that declared one showed up
   *  in the pane as a workflow that didn't. Quoted scalars matter here (the comma
   *  inside that pattern is CONTENT, not a separator); `emitScalar` already quotes
   *  for the flow context this emits into. */
  allow?: string[];
  /** Thinking level (#687) — one of the CLI's own `effort` values, or "" for its
   *  default. Which values exist, and whether the CLI has any seam for them at
   *  all, is capability data the BACKEND owns (`agent_cli_knobs`); this field is
   *  just the file's text, exactly like `cli` and `kind`. */
  effort?: string;
  /** Context-window variant (#687) — `1m`, or "" for the model's own window.
   *  Same ownership rule as {@link effort}. */
  context?: string;
  /** OPTIONAL remote LABEL (#1457) — the abstract name of a machine this block's
   *  agent runs on over SSH. A repo file SELECTS a label; the operator binds it
   *  to a host, an account and a remote clone path outside the repo, so a
   *  `host:`/`port:`/`identity_file:` key is not a field here — it is an unknown
   *  key, and an unknown key fails the whole file on the engine.
   *
   *  Absent is a local block, byte for byte. Declared, it is inert in this build:
   *  the operator binding (#1458) and the spawn path (#1459) are what make it do
   *  anything. */
  remote?: string;
  /** HOW loomux drives this block's agent (#2850) — `"structured"` over the
   *  CLI's structured-protocol surface instead of a scraped PTY, or absent
   *  (every block today: the spawn path that reads the key is S3b). The
   *  VALUE is loomux's closed vocabulary (`DRIVER_MODES`' mirror); whether
   *  the block's CLI can carry it is capability data the backend owns
   *  (`CliCaps.structured_driver`), so this field is just the file's text,
   *  exactly like `effort` and `context`. */
  driver?: string;
  /** The prompt-cache TTL in minutes this block's agent runs on (#3407) — absent
   *  = the CLI's own default (`CliCaps.cache_ttl_minutes`), `0` = unknown. A
   *  number the pane reads and re-emits; the engine refuses one above
   *  `BLOCK_CACHE_TTL_MINUTES_MAX`. */
  cache_ttl_minutes?: number;
  /** Keys this build doesn't know, preserved verbatim across a round-trip. */
  extra?: Record<string, YamlValue>;
}

/** One ADVISORY edge (#222 §2g): the declared happy path. The orchestrator still decides
 *  when to spawn what — a static DAG would replace its mergeability/parallelism judgment
 *  with something dumber. Edges document intent and drive the graph view; gates are the
 *  half that is actually enforced. */
export interface WorkflowEdge {
  from: string;
  to: string;
}

/** The ENFORCED half: a merge gate. The backend refuses `gh pr merge` (via the existing
 *  PATH shim) until the named reviewers' verdicts are recorded PASS — which is what makes
 *  multi-reviewer more than theatre, and closes the loomux side of #197. */
export interface MergeGate {
  require: string;
  /** Only meaningful when `require: threshold`. */
  threshold?: number;
  reviewers: string[];
  /** Extra conditions (`ci-green`, …) — passed through; the backend owns their meaning. */
  also: string[];
  /** The small-batch clause (#1174): the largest PR, in changed lines, the gate lets
   *  through. Absent = no limit, and absent is kept apart from any number here — a
   *  `0` this pane invented would be a file the engine refuses. */
  max_diff_lines?: number;
  /** Path-based reviewer routing (#1176). Absent (or empty) = no routing.
   *
   *  Read and emitted here even though this pane offers no editor for it yet, and
   *  that is the whole point: `MergeGate` has no unknown-key bag, so a key this
   *  model did not carry would be a rule the next form edit silently DELETED — and
   *  the thing deleted would be a required reviewer. */
  routing?: RoutingRule[];
}

/** One `gates.merge.routing[]` rule: if the PR changed a file matching any of
 *  `paths`, `reviewers` are required on top of the gate's own list. */
export interface RoutingRule {
  paths: string[];
  reviewers: string[];
}

export interface WorkflowGates {
  merge?: MergeGate;
  extra?: Record<string, YamlValue>;
}

/** Where autonomous work comes from (#382 P1). Every field is OPTIONAL and absent
 *  means "inherit loomux's built-in profile" — the engine resolves a partial block
 *  against `builtin_intake_profile()`, so this model must keep "not declared" and
 *  "declared empty" apart rather than filling defaults in on read (a save would
 *  otherwise write four label lines nobody asked for).
 *
 *  There is deliberately no field here that can weaken the human merge gate — that
 *  lives in the `gh` shim, and the engine's `deny_unknown_fields` makes an invented
 *  one a hard parse error rather than an ignored line. This model preserves unknown
 *  keys (`extra`) so a newer build's file survives a round-trip, and says so out
 *  loud (`unknown-key`) because this build's engine would refuse the whole file. */
export interface WorkflowIntake {
  source?: string;
  labels?: WorkflowIntakeLabels;
  extra?: Record<string, YamlValue>;
}

/** The label vocabulary intake matches on. Declare one, inherit the other four. */
export interface WorkflowIntakeLabels {
  ready?: string;
  investigate?: string;
  owned?: string;
  prototype?: string;
  /** The veto (#778): "held by the human — do not start this". The only label here
   *  whose meaning is opt-OUT, and the boundary full autonomy leaves standing. */
  hold?: string;
  extra?: Record<string, YamlValue>;
}

/** The bisecting merge queue's policy (#581 §11.2). Absent block = the feature is
 *  off; that is why every field is optional rather than defaulted here. */
export interface WorkflowMergeQueue {
  enabled?: boolean;
  max_batch?: number;
  checks_timeout_minutes?: number;
  extra?: Record<string, YamlValue>;
}

/** The review-loop driver's policy (#1778 §5.3). Absent block = the driver is
 *  off; every field is optional for the same reason `WorkflowMergeQueue`'s are. */
export interface WorkflowDriver {
  enabled?: boolean;
  max_review_rounds?: number;
  max_ci_attempts?: number;
  max_rebase_attempts?: number;
  lane_timeout_minutes?: number;
  fix_timeout_minutes?: number;
  drive_timeout_minutes?: number;
  /** The PLAN driver's three keys (#3040). `plan_enabled` is a second toggle,
   *  not a widening of `enabled`: the engine reads it UNDER `enabled`, so the
   *  plan driver is off wherever the review driver is. */
  plan_enabled?: boolean;
  plan_review_minutes?: number;
  planner_timeout_minutes?: number;
  /** #3367: the driver's own non-blocking rounds, and whether a worker's
   *  report(done) starts a drive. Both read under `enabled`. */
  fix_nonblocking_rounds?: number;
  auto_drive_on_done?: boolean;
  extra?: Record<string, YamlValue>;
}

/** The driver defaults the pane RENDERS when the file omits a field - the
 *  engine's `DriverPolicy::default`, mirrored here because this module is pure
 *  and import-free. A literal at the point of use is a number nothing can
 *  check: `NOTIFY_EXPIRES_DEFAULT_MIN` moves, the manifest pin forces the JSON
 *  to follow, and a `?? 60` left behind renders a stale 60 with nothing red.
 *  So the chrome reads THIS table, and `test/workflowschema.test.ts` pins every
 *  entry against the manifest's declared default - engine → manifest → pane,
 *  with no unpinned step left (#1784 review, premortem 2). */
export const DRIVER_DEFAULTS: Readonly<{
  enabled: boolean;
  max_review_rounds: number;
  max_ci_attempts: number;
  max_rebase_attempts: number;
  lane_timeout_minutes: number;
  fix_timeout_minutes: number;
  drive_timeout_minutes: number;
  plan_enabled: boolean;
  plan_review_minutes: number;
  planner_timeout_minutes: number;
  fix_nonblocking_rounds: number;
  auto_drive_on_done: boolean;
}> = {
  enabled: false,
  max_review_rounds: 3,
  max_ci_attempts: 3,
  max_rebase_attempts: 1,
  lane_timeout_minutes: 60,
  fix_timeout_minutes: 60,
  drive_timeout_minutes: 720,
  plan_enabled: false,
  plan_review_minutes: 0,
  planner_timeout_minutes: 60,
  fix_nonblocking_rounds: 0,
  auto_drive_on_done: false,
};

/** One named lock resource (#858) — how many agents may hold it at once and for
 *  how long. Two numbers, keyed by a name the repo chose; loomux never learns what
 *  the name means (CLAUDE.md constraint 8 — this is policy, not mechanism). */
export interface WorkflowResource {
  slots?: number;
  max_hold_minutes?: number;
  extra?: Record<string, YamlValue>;
}

/** The `board.wip:` mapping (#1175) — one optional cap per board status. Typed as a
 *  record rather than as seven named fields because the pane's job here is to
 *  round-trip whatever the engine accepts: `WIP_STATUSES` is what says which keys are
 *  known, and a key outside it lands in `extra` with an `unknown-key` finding, exactly
 *  as `deny_unknown_fields` will refuse it on the engine side. */
export type WorkflowWip = Record<string, number>;

/** The `board:` section (#1175). Same declared-or-absent rule as `intake:` and
 *  `merge_queue:`: an omitted key is the engine's own default, never a value this
 *  module writes back in. */
/** The `triage:` section (#3304 S1). Same declared-or-absent rule as `board:` and
 *  `merge_queue:`: an omitted key is the engine's own default, never a value this
 *  module writes back in. `kinds` is the one list, and it is round-tripped as
 *  written rather than validated against the engine's closed set here — the pane
 *  reports an unknown key, and the ENGINE refuses an unknown kind, which is the
 *  same division `intake.source` already draws. */
export interface WorkflowTriage {
  enabled?: boolean;
  provider?: string;
  kinds?: string[];
  max_defer_minutes?: number;
  extra?: Record<string, YamlValue>;
}

export interface WorkflowBoard {
  wip?: WorkflowWip;
  /** Absent, `true` or `false` — three states, and absent is NOT `false` on the wire
   *  even though the engine reads them alike (the `merge_queue.enabled` argument). */
  enforce?: boolean;
  extra?: Record<string, YamlValue>;
  /** Keys under `wip:` this build does not know — preserved so a file written by a
   *  newer loomux survives a round-trip through an older pane. */
  wipExtra?: Record<string, YamlValue>;
}

export interface Workflow {
  version: number;
  name: string;
  /** The loomux version that CREATED this file (§4's Langflow `last_tested_version`
   *  lesson). Written exactly once, at creation, and never restamped: it records who
   *  authored the workflow, not who last looked at it. A typed field since #880 —
   *  it used to ride the unknown-key bag, which round-tripped it correctly but left
   *  it invisible to everything that asks "what fields does a workflow have?". */
  authored_with?: string;
  blocks: WorkflowBlock[];
  edges: WorkflowEdge[];
  gates: WorkflowGates;
  intake?: WorkflowIntake;
  merge_queue?: WorkflowMergeQueue;
  /** Review-loop driver policy (#1778 §5.3). */
  driver?: WorkflowDriver;
  /** Keyed by a repo-chosen resource name, in the file's own order (the emitter
   *  sorts, matching the engine's `BTreeMap`). */
  resources?: Record<string, WorkflowResource>;
  /** Task-board policy — per-status WIP limits (#1175). */
  board?: WorkflowBoard;
  /** Delivery-triage policy (#3304 S1). */
  triage?: WorkflowTriage;
  extra?: Record<string, YamlValue>;
}

// ---------- findings ----------

export type FindingSeverity = "error" | "warning";

export type FindingCode =
  | "yaml-syntax"
  | "not-a-mapping"
  | "version-missing"
  | "version-unsupported"
  | "no-blocks"
  | "block-not-a-mapping"
  | "block-id-missing"
  | "block-id-invalid"
  | "block-id-duplicate"
  | "unknown-kind"
  | "unknown-cli"
  | "prompt-and-profile"
  | "role-hint-unknown"
  | "role-hint-wrong-kind"
  | "role-hint-superseded"
  | "manager-not-unique"
  | "knob-unavailable"
  | "edge-not-a-mapping"
  | "edge-unknown-block"
  | "edge-self"
  | "gate-unknown-require"
  | "gate-no-reviewers"
  | "gate-unknown-reviewer"
  | "gate-not-a-reviewer"
  | "gate-bad-threshold"
  | "gate-bad-max-diff-lines"
  | "gate-bad-routing"
  | "isolated-block"
  | "unreachable-block"
  | "no-entry-block"
  | "unknown-key"
  | "section-not-a-mapping"
  | "section-bad-value"
  | "section-out-of-range"
  | "intake-unknown-source"
  | "intake-bad-label"
  | "resource-name-invalid"
  | "allow-not-permitted"
  | "persona-not-permitted"
  | "remote-invalid-label"
  | "remote-not-permitted"
  | "remote-requires-claude"
  | "allow-sanitized";

/** The policy sections a finding can be ABOUT — the routing key for the three that are
 *  neither a block nor a line (#1020). Same job `blockId` does for a block: the pane's
 *  roster and its findings list land the human on the form that can fix it, and neither
 *  has to match on the message text to work out which one that is. */
/** `board:` (#1175) is deliberately NOT a member yet: this key routes a finding onto the
 *  inspector form that can fix it, and the board section has no form - it is listed as
 *  not-yet-editable in `test/workflowschema.test.ts` like every other field. A section
 *  here with no form to land on would be a click that goes nowhere. Its findings carry a
 *  message naming `board.wip.<status>` instead, which is what the raw-text view needs
 *  anyway; the member and the form arrive together or not at all. */
/** `driver:` (#1778) is a member WITH a form since #1869 (enable-toggle plus the six
 *  counters, same shape as `mergeQueueForm`), so a finding's click lands on the form
 *  that can fix it — the same click-to-the-fix routing the other policy sections get. */
export type FindingSection = "intake" | "merge_queue" | "driver" | "resources";

/** One thing wrong with the workflow. `blockId` lets the pane render the finding INLINE
 *  next to the block it is about (the whole reason the validation pass is worth having is
 *  that it tells you WHERE); `line` does the same for the raw-text view, and `section` for
 *  the policy sections. */
export interface Finding {
  severity: FindingSeverity;
  code: FindingCode;
  message: string;
  blockId?: string;
  /** Which policy section this is about, when it is about one. */
  section?: FindingSection;
  /** 1-based source line, when the finding came from reading the text. */
  line?: number;
}

export function hasErrors(findings: readonly Finding[]): boolean {
  return findings.some((f) => f.severity === "error");
}

/** True only when the text itself couldn't be read as a document at all — a syntax error, or a
 *  shape so wrong the root isn't even a mapping. This is deliberately NARROWER than `hasErrors`:
 *  a `version-unsupported` or `gate-bad-threshold` finding means the WORKFLOW is wrong, not that
 *  the TEXT is unreadable, and the pane's form stays editable through those (see
 *  `workflowview.ts`'s `syntaxBroken`, which this mirrors exactly on purpose — #233 B3. The two
 *  must agree: if the view lets a human keep editing a file, `serializeWorkflowPreserving`
 *  (workflowserialize.ts) must not treat that same file as too broken to diff against, or the very first edit
 *  silently falls back to a full canonical rewrite for a reason the human was never shown. */
export function isUnreadable(findings: readonly Finding[]): boolean {
  return findings.some((f) => f.code === "yaml-syntax" || f.code === "not-a-mapping");
}

/** The five label fields, in the order the engine's own struct declares them — which is
 *  therefore the order they are emitted in and the order a form should show them. */
export const INTAKE_LABEL_KEYS = ["ready", "investigate", "owned", "prototype", "hold"] as const;
export type IntakeLabelKey = (typeof INTAKE_LABEL_KEYS)[number];
