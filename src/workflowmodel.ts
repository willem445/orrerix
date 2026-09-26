// Pure model for the repo's `workflow.yml` — the user-defined agent workflow (#222).
// DOM-free and I/O-free: parse, validate, derive the graph, serialize. The pane
// (workflowview.ts) is a VIEW over this; the FILE is the source of truth (the
// Kestra pattern — a form edit rewrites the YAML, it does not become a second,
// competing copy of it).
//
// Three rules this module exists to keep, each earned from a documented failure of
// some other workflow tool (see the #222 investigation, §1a-v and §4):
//
//  1. `id` is the identity; `name` is display only. n8n keys its graph by the node's
//     DISPLAY NAME, so a rename silently breaks every edge and expression pointing at
//     it. Here an edge/gate names an `id`, ids are immutable once created, and a rename
//     touches nothing else.
//  2. No coordinates, ever. Dify/ComfyUI/Langflow all embed x/y in the semantic file,
//     so nudging a node churns the logic diff. Layout (if the view ever draws any) goes
//     in the `workflow.layout.json` beside it; this file is the workflow.
//  3. Validate BEFORE a run, not during one. Flowise, Langflow and Dify discover a
//     dangling reference at runtime; Dify will happily *publish* a workflow whose node
//     isn't installed. `validateWorkflow` is the whole pre-run pass, and it is pure
//     precisely so it is unit-tested without a DOM (test/workflowvalidate.test.ts).
//
// A fourth rule is about how we FAIL: an unknown kind, an unknown CLI, a dangling edge
// — none of them stop the file from opening. They become findings, the block renders as
// a stub, and the human can fix it in the pane. Refusing to open a file you can't fully
// understand is ComfyUI's #1 import-failure class, and it is the one behavior guaranteed
// to leave someone stranded with no way to repair the thing that is broken.
//
// The YAML subset is hand-rolled rather than pulled from npm: the schema is small and
// CLOSED (block mappings, block sequences, flow seqs/maps, block scalars, comments,
// quoted scalars), and the alternative is a dependency in a project that has spent real
// effort keeping its dependency list short. Anything the subset can't read is a parse
// finding on a line number — the raw-text view still opens, so the file is still fixable.

// #3498 F2 split this module into five, and it is now ONLY the re-export barrel every
// importer already names, so no importer changed. The job each module owns:
//
//   workflowtypes.ts      the closed enums, bounds, schema interfaces, findings, and the
//                         small predicates over them (imports no other workflow* module)
//   workflowparse.ts      the YAML subset reader and `parseWorkflow`        (imports types)
//   workflowserialize.ts  the canonical and comment-preserving serializers, the driver
//                         form's text rules                        (imports types, parse)
//   workflowvalidate.ts   `validateWorkflow`, the pre-run pass             (imports types)
//   workflowgraph.ts      the derived graph, graph edits, scaffold, `analyzeWorkflow`
//                                     (imports types, parse, serialize, validate)
//
// None of the five imports THIS file, which is what keeps the graph acyclic
// (test/workflowmodel.test.ts pins both properties). The lists below are exactly the
// names this module exported before the split. A helper a sibling module needed
// (`stripComment`, `emitValue`, ...) was widened to `export` in its own module and is
// deliberately NOT re-exported here.

export {
  BLOCK_KINDS,
  isBlockKind,
  WORKFLOW_CLIS,
  isWorkflowCli,
  ROLE_HINTS,
  roleHintRequires,
  roleHintsForKind,
  personaDenialReason,
  allowDenialReason,
  REMOTE_LABEL_MAX,
  isRemoteLabel,
  remoteDenialReason,
  sanitizeAllowPattern,
  isReviewingBlock,
  WORKFLOW_VERSION,
  CONFIG_DIR,
  LEGACY_CONFIG_DIR,
  WORKFLOW_FILE,
  LEGACY_WORKFLOW_FILE,
  legacyFallbackFor,
  WORKFLOWS_DIR,
  LEGACY_WORKFLOWS_DIR,
  DEFAULT_WORKFLOW_NAME,
  WORKFLOW_NAME_MAX,
  isWorkflowName,
  workflowRelFor,
  workflowNameOf,
  GATE_REQUIRES,
  GATE_REQUIRES_ACCEPTED,
  INTAKE_SOURCES,
  isIntakeSource,
  ID_MAX_CHARS,
  isValidIntakeLabel,
  isValidResourceName,
  RESOURCE_SLOTS_MIN,
  RESOURCE_SLOTS_MAX,
  RESOURCE_MAX_HOLD_MINUTES_MIN,
  RESOURCE_MAX_HOLD_MINUTES_MAX,
  RESOURCES_MAX,
  MERGE_QUEUE_MAX_BATCH_MIN,
  GATE_THRESHOLD_MIN,
  GATE_MAX_DIFF_LINES_MIN,
  GATE_ROUTING_RULES_MAX,
  GATE_ROUTING_PATHS_MAX,
  ROUTING_GLOB_MAX_CHARS,
  isRoutingGlob,
  MERGE_QUEUE_CHECKS_TIMEOUT_MIN,
  MERGE_QUEUE_CHECKS_TIMEOUT_MAX,
  DRIVER_MAX_REVIEW_ROUNDS_MIN,
  DRIVER_MAX_REVIEW_ROUNDS_MAX,
  DRIVER_MAX_CI_ATTEMPTS_MIN,
  DRIVER_MAX_CI_ATTEMPTS_MAX,
  DRIVER_MAX_REBASE_ATTEMPTS_MIN,
  DRIVER_MAX_REBASE_ATTEMPTS_MAX,
  DRIVER_TIMEOUT_MIN,
  DRIVER_TIMEOUT_MAX,
  DRIVER_PLAN_REVIEW_MINUTES_MIN,
  DRIVER_PLAN_REVIEW_MINUTES_MAX,
  DRIVER_PLANNER_TIMEOUT_MINUTES_MIN,
  DRIVER_PLANNER_TIMEOUT_MINUTES_MAX,
  DRIVER_FIX_NONBLOCKING_ROUNDS_MIN,
  DRIVER_FIX_NONBLOCKING_ROUNDS_MAX,
  DRIVER_DRIVE_TIMEOUT_MIN,
  DRIVER_DRIVE_TIMEOUT_MAX,
  WIP_STATUSES,
  WIP_LIMIT_MIN,
  TRIAGE_MAX_DEFER_MINUTES_MIN,
  TRIAGE_MAX_DEFER_MINUTES_MAX,
  BLOCK_CACHE_TTL_MINUTES_MIN,
  BLOCK_CACHE_TTL_MINUTES_MAX,
  POLICY_BOUNDS,
  isValidBlockId,
  DRIVER_DEFAULTS,
  hasErrors,
  isUnreadable,
  INTAKE_LABEL_KEYS,
} from "./workflowtypes.ts";
export type {
  BlockKind,
  WorkflowCli,
  RoleHint,
  GateRequire,
  IntakeSourceName,
  FieldBounds,
  YamlValue,
  WorkflowBlock,
  WorkflowEdge,
  MergeGate,
  RoutingRule,
  WorkflowGates,
  WorkflowIntake,
  WorkflowIntakeLabels,
  WorkflowMergeQueue,
  WorkflowDriver,
  WorkflowResource,
  WorkflowWip,
  WorkflowTriage,
  WorkflowBoard,
  Workflow,
  FindingSeverity,
  FindingCode,
  FindingSection,
  Finding,
  IntakeLabelKey,
} from "./workflowtypes.ts";
export {
  KNOWN_BLOCK,
  parseWorkflow,
} from "./workflowparse.ts";
export type {
  ParseResult,
} from "./workflowparse.ts";
export {
  setDriverEnabled,
  driverSectionHasComments,
  isDriverOn,
  removeDriverBlock,
  driverEnabledLineComment,
  serializeWorkflow,
  serializeWorkflowPreserving,
} from "./workflowserialize.ts";
export {
  validateWorkflow,
} from "./workflowvalidate.ts";
export type {
  KnobLookup,
} from "./workflowvalidate.ts";
export {
  deriveGraph,
  nextBlockId,
  connectionError,
  connectBlocks,
  disconnectBlocks,
  gateConnectionError,
  connectToGate,
  disconnectFromGate,
  addBlock,
  newBlock,
  removeBlockAt,
  scaffoldWorkflowText,
  AUTHORED_WITH_KEY,
  starterWorkflow,
  analyzeWorkflow,
  formatWorkflowText,
} from "./workflowgraph.ts";
export type {
  GraphNode,
  GraphEdge,
  GraphGate,
  WorkflowGraph,
  WorkflowAnalysis,
} from "./workflowgraph.ts";
