// The derived graph and the graph EDIT operations the canvas drives (#222; split out of
// workflowmodel.ts by #3498 F2), plus the scaffold/starter workflows and the two calls the
// view makes (`analyzeWorkflow`, `formatWorkflowText`). Sits at the top of the module DAG:
// it imports types, parse, serialize and validate, and nothing imports it but the barrel.
// Design note: docs/design/workflows.md; module map: docs/design/architecture.md.

import {
  isBlockKind,
  WORKFLOW_VERSION,
  WORKFLOW_FILE,
  GATE_THRESHOLD_MIN,
} from "./workflowtypes.ts";
import type {
  BlockKind,
  WorkflowBlock,
  WorkflowEdge,
  MergeGate,
  Workflow,
  Finding,
} from "./workflowtypes.ts";
import {
  parseWorkflow,
} from "./workflowparse.ts";
import {
  serializeWorkflow,
} from "./workflowserialize.ts";
import {
  gateReviewerFinding,
  validateWorkflow,
} from "./workflowvalidate.ts";
import type {
  KnobLookup,
} from "./workflowvalidate.ts";

// ---------- the derived graph (read-only) ----------

export interface GraphNode {
  block: WorkflowBlock;
  /** The block's INDEX in the roster — its identity in the picture. Not its id: the blocks
   *  that most need drawing are the broken ones, and two id-less stubs (or a duplicate-id
   *  pair) share an id while being two different rows. Keying the graph by id drew them on
   *  top of each other, so a file with two stubs showed one (rev-5 F5) — in the very view
   *  whose job is to let you SEE the file. The roster already keys by index for exactly
   *  this reason; now the graph agrees with it. */
  index: number;
  /** False when the block's kind isn't a capability class — the view draws it as a stub. */
  known: boolean;
  /** Column in the layered layout: distance from the nearest entry block. */
  layer: number;
}

export interface GraphEdge extends WorkflowEdge {
  /** False when either end names a block that doesn't exist (the view draws it dangling). */
  resolved: boolean;
}

export interface GraphGate {
  /** The gate's name — today, always "merge". */
  name: string;
  require: string;
  threshold?: number;
  reviewers: string[];
}

export interface WorkflowGraph {
  nodes: GraphNode[];
  edges: GraphEdge[];
  gates: GraphGate[];
  /** Block INDICES grouped by layer, left to right (see GraphNode.index). */
  layers: number[][];
}

/** Derive the picture: the blocks, the advisory edges between them, and the enforced
 *  gates hanging off the reviewers they name. READ-ONLY by design (#222 Q6) — the graph
 *  is a view over the file, like GitLab's CI "Visualize" tab, not an editable canvas that
 *  can corrupt it. Layering is longest-path from the entry blocks, with cycles (the
 *  worker ⇄ reviewer rework loop) resolved by leaving the back-edge's target where its
 *  forward path put it. */
export function deriveGraph(w: Workflow): WorkflowGraph {
  const byId = new Map<string, WorkflowBlock>();
  for (const b of w.blocks) if (b.id && !byId.has(b.id)) byId.set(b.id, b);

  const edges: GraphEdge[] = w.edges.map((e) => ({
    ...e,
    resolved: byId.has(e.from) && byId.has(e.to),
  }));

  // Layering is computed over IDS — an edge names ids, so that is what a column can be
  // derived from — and then handed to the NODES, which are rows. The two are different
  // things, and conflating them is what stacked the broken blocks on one another.
  const layer = new Map<string, number>();
  for (const b of w.blocks) if (b.id) layer.set(b.id, 0);

  // Relax forward edges |blocks| times: a node sits one column right of its deepest
  // predecessor. Bounded, so a cycle terminates instead of spinning.
  for (let pass = 0; pass < w.blocks.length; pass++) {
    let moved = false;
    for (const e of edges) {
      if (!e.resolved || e.from === e.to) continue;
      const want = (layer.get(e.from) ?? 0) + 1;
      if (want > (layer.get(e.to) ?? 0)) {
        layer.set(e.to, want);
        moved = true;
      }
    }
    if (!moved) break;
  }

  // An id-less block has no column of its own to compute (nothing can point at it), so it
  // sits in the first one — visible, drawn as the stub it is, next to the finding that says
  // to give it an id.
  const nodes: GraphNode[] = w.blocks.map((b, index) => ({
    block: b,
    index,
    known: isBlockKind(b.kind),
    layer: (b.id && layer.get(b.id)) || 0,
  }));

  const depth = nodes.reduce((m, n) => Math.max(m, n.layer), 0);
  const layers: number[][] = Array.from({ length: depth + 1 }, () => []);
  for (const n of nodes) layers[n.layer]!.push(n.index);

  const gates: GraphGate[] = w.gates.merge
    ? [
        {
          name: "merge",
          require: w.gates.merge.require,
          threshold: w.gates.merge.threshold,
          reviewers: w.gates.merge.reviewers,
        },
      ]
    : [];

  return { nodes, edges, gates, layers };
}

// ---------- editing helpers (used by the form; pure, so they are tested) ----------

/** A fresh block id derived from `base`, unique within `w`. Ids are IMMUTABLE once a
 *  block exists (rule 1 at the top of this file), so this runs exactly once per block —
 *  at creation — and never again. */
export function nextBlockId(w: Workflow, base: string): string {
  const slug =
    base
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .replace(/^([^a-z])/, "b$1") || "block";
  const taken = new Set(w.blocks.map((b) => b.id));
  if (!taken.has(slug)) return slug;
  for (let n = 2; ; n++) {
    const candidate = `${slug}-${n}`;
    if (!taken.has(candidate)) return candidate;
  }
}

// ---------- graph EDIT operations (#222 v2: the canvas edits the file) ----------
//
// The canvas is now bidirectional — you can draw an edge, add a block, delete either — and
// every one of those goes through a function here, in the pure module, rather than through
// the DOM layer poking at the model. That is what makes "draw an edge, serialize, re-read,
// get the same workflow" a unit test instead of a thing you check by hand with a mouse.
//
// They all return a NEW workflow, and none of them is allowed to invent an identity: a block
// gets its id from the human (immutable, human-meaningful — §4), and an edge is a pair of ids
// that already exist.

/** Why a proposed edge can't be drawn, or null when it can. Checked BEFORE the edge is
 *  created rather than reported after — an editable canvas that lets you draw an edge and
 *  then tells you it was invalid has wasted the gesture and left you to undo it. */
export function connectionError(w: Workflow, from: string, to: string): string | null {
  if (!from || !to) return "A block needs an id before an edge can name it.";
  if (from === to) return "A block can't run after itself.";
  if (!w.blocks.some((b) => b.id === from) || !w.blocks.some((b) => b.id === to)) {
    return "That block doesn't exist.";
  }
  if (w.edges.some((e) => e.from === from && e.to === to)) return "That edge already exists.";
  return null;
}

/** Draw an advisory edge. A duplicate or illegal edge is a no-op rather than a throw — the
 *  canvas has already refused the gesture (`connectionError`), and this is the second line of
 *  defence, not the first. */
export function connectBlocks(w: Workflow, from: string, to: string): Workflow {
  if (connectionError(w, from, to)) return w;
  return { ...w, edges: [...w.edges, { from, to }] };
}

/** Erase an edge. Only that edge: the blocks it joined are untouched, which is the whole
 *  difference between deleting a connection and deleting the work. */
export function disconnectBlocks(w: Workflow, from: string, to: string): Workflow {
  return { ...w, edges: w.edges.filter((e) => !(e.from === from && e.to === to)) };
}

/** Why a reviewer block can't be wired to the merge gate, or null when it can (#1388).
 *  The gate half of `connectionError`, and asked at the same moment for the same reason: a
 *  canvas that completes the gesture and only then says the drop was invalid has wasted it.
 *
 *  The one refusal that is NOT re-stated here is "that block's kind can't record a verdict" —
 *  `gateReviewerFinding` already answers that for the validator and for the engine's
 *  `gate_reviewer_error`, so it answers it here too. A gate that refuses a drop for a
 *  different reason than the findings strip gives for the same block is two definitions of
 *  the same rule, which is exactly what #1176 collapsed.
 *
 *  A DUPLICATE is a reason, not a silent no-op, and that mirrors `connectionError`'s "that
 *  edge already exists": the human made a gesture, and a gesture that changes nothing and
 *  says nothing is indistinguishable from one the canvas dropped on the floor — which is the
 *  whole complaint #1387 and #1388 were opened about. `connectToGate` is still a no-op on
 *  it, so the file cannot grow a duplicate seat whatever the view does. */
export function gateConnectionError(w: Workflow, from: string): string | null {
  if (!from) return "A block needs an id before the merge gate can name it.";
  const gate = w.gates.merge;
  if (!gate) {
    return "There is no merge gate yet — turn it on in the gate's settings, then wire reviewers to it.";
  }
  if (gate.reviewers.includes(from)) return `"${from}" already gates the merge.`;
  // First id wins, exactly as `validateWorkflow` resolves a duplicate: the gate would name
  // one of them ambiguously either way, and that is already its own finding.
  const byId = new Map<string, WorkflowBlock>();
  for (const b of w.blocks) if (b.id && !byId.has(b.id)) byId.set(b.id, b);
  const finding = gateReviewerFinding(byId, "The merge gate", from);
  return finding ? finding.message : null;
}

/** Wire a reviewer to the merge gate: append its id to `gates.merge.reviewers`, which is the
 *  shape `parse_workflow` already reads (a list of block ids — no new backend surface).
 *  A refused or duplicate drop is a no-op, like `connectBlocks`.
 *
 *  SEAT ORDER IS NOT THE HUMAN'S, and this is the place to say so because it is the place
 *  someone will come looking. The append is only the cheapest way to add an entry: every
 *  write goes through `emitGatesLines`, which emits `sortByBlocks(gate.reviewers, order)` —
 *  so the file always lists the seats in ROSTER order, whatever order the model's array is
 *  in, on the canvas's `serializeWorkflowPreserving` path as much as on the canonical one.
 *  That is deliberate and pre-dates this: it is the same canonical-form rule that groups
 *  edges by source, so two people who wire the same gate in a different order get the same
 *  file and neither sees a diff from the other's clicking sequence.
 *
 *  The consequence worth writing down: a reorder affordance on this list — drag-to-reorder,
 *  up/down buttons — would appear to work and change nothing in the file. If seat order is
 *  ever meant to MEAN something, `sortByBlocks` is what has to change first. */
export function connectToGate(w: Workflow, from: string): Workflow {
  if (gateConnectionError(w, from)) return w;
  const gate = w.gates.merge!;
  return { ...w, gates: { ...w.gates, merge: { ...gate, reviewers: [...gate.reviewers, from] } } };
}

/** Take a reviewer's seat off the merge gate — the gate-edge mirror of `disconnectBlocks`.
 *  Only the seat: the block, its edges and the rest of the gate's policy are untouched. */
export function disconnectFromGate(w: Workflow, from: string): Workflow {
  const gate = w.gates.merge;
  if (!gate || !gate.reviewers.includes(from)) return w;
  return {
    ...w,
    gates: { ...w.gates, merge: withGateReviewers(gate, gate.reviewers.filter((r) => r !== from)) },
  };
}

/** A gate with a new reviewer list, and its `threshold` kept SATISFIABLE — the one derived
 *  number that a shorter list can invalidate.
 *
 *  `threshold: 3` over three reviewers is a legal file the engine loads; drop one reviewer
 *  and it becomes "3 passes from 2 reviewers", which `parse_workflow` refuses OUTRIGHT —
 *  not the gate, the whole `workflow.yml`, so the group silently falls back to the built-in
 *  roster. A one-click gesture on the canvas must not be able to do that, so the threshold
 *  follows the list down.
 *
 *  It follows it down to `GATE_THRESHOLD_MIN` and no further, which is the difference
 *  between clamping a number and destroying one: removing the LAST reviewer leaves
 *  `threshold: 1` over an empty list — already `gate-no-reviewers`, loudly, whatever we do
 *  with the number — and re-wiring one reviewer makes the gate valid again with the human's
 *  own intent intact. Clamping to 0 would trade one finding for two and lose the number.
 *
 *  The clamp is deliberately silent about the OTHER direction: adding a reviewer never
 *  touches the threshold, because "2 of 3 must pass" is a policy, not a proportion. */
function withGateReviewers(gate: MergeGate, reviewers: string[]): MergeGate {
  const next: MergeGate = { ...gate, reviewers };
  if (
    gate.require === "threshold" &&
    typeof gate.threshold === "number" &&
    gate.threshold > reviewers.length
  ) {
    next.threshold = Math.max(GATE_THRESHOLD_MIN, reviewers.length);
  }
  return next;
}

/** Add a block. The caller supplies the ID — the canvas asks the human for it, because §4's
 *  first commitment is that an id is human-meaningful and immutable, and a canvas that mints
 *  `node_1720794829558` (Dify's actual behaviour) makes every edge in the file unreadable
 *  and every id a lie about what the block is. */
export function addBlock(w: Workflow, block: WorkflowBlock): Workflow {
  return { ...w, blocks: [...w.blocks, block] };
}

/** A new block, filled in with the defaults a reviewer usually wants — the caller overrides
 *  what it asked the human about. Kept here so "what a new block is" has one answer. */
export function newBlock(id: string, name: string, kind: BlockKind = "reviewer"): WorkflowBlock {
  return { id, name: name || id, kind, cli: "claude", model: "" };
}

/** Remove the block at `index`, AND every reference to it — edges at either end, and its
 *  seat on the merge gate. A delete that left the references behind would turn one click
 *  into three validation errors, which is exactly the "dangling reference" class this file
 *  exists to prevent (Dify ships it; we don't).
 *
 *  By INDEX, not by id, and both halves of that matter:
 *   - an id-LESS stub (a block the file got wrong) has no id to delete by, and deleting
 *     "every block whose id is empty" would take its siblings with it;
 *   - a DUPLICATE id survives its own deletion — the other block still answers to it — so
 *     the references are still meaningful and must NOT be stripped. Hence `gone`. */
export function removeBlockAt(w: Workflow, index: number): Workflow {
  const block = w.blocks[index];
  if (!block) return w;
  const blocks = w.blocks.filter((_, i) => i !== index);
  const id = block.id;
  const gone = !!id && !blocks.some((b) => b.id === id);
  const gate = w.gates.merge;
  return {
    ...w,
    blocks,
    edges: gone ? w.edges.filter((e) => e.from !== id && e.to !== id) : w.edges,
    gates: {
      ...w.gates,
      // Through `withGateReviewers` for the same reason `disconnectFromGate` is (#1388):
      // a threshold left above the reviewer count is a file `parse_workflow` refuses
      // WHOLE, and there is no reading on which deleting a block should do that while
      // deleting its gate edge doesn't. One rule, both paths to the same empty seat.
      merge: gate
        ? gone
          ? withGateReviewers(gate, gate.reviewers.filter((r) => r !== id))
          : gate
        : undefined,
    },
  };
}

/** The file a repo with no workflow gets when the human asks for one: today's built-in
 *  pipeline, written out — plus the comments that say what each part is FOR.
 *
 *  Comments, and not just `serializeWorkflow(starterWorkflow())`, because this is the one
 *  moment the file is read by someone who has never seen the schema: it arrives in their
 *  editor, in their diff, in their teammate's `git pull`. A commented scaffold is how every
 *  config-as-code tool worth using introduces itself, and it costs one string.
 *
 *  (They are comments, so they do not survive a canonical re-serialize — the first form edit
 *  rewrites the file without them. That is the honest trade of having ONE canonical shape,
 *  it is stated in the design note, and it is why the scaffold is offered at CREATION rather
 *  than being something the formatter tries to preserve. What the human writes in the YAML
 *  tab and saves is kept verbatim; only an edit made through the form or the canvas
 *  re-serializes.)
 *
 *  `authoredWith` is stamped in the same one moment `starterWorkflow` stamps it. */
export function scaffoldWorkflowText(authoredWith?: string, rel: string = WORKFLOW_FILE): string {
  const stamp = authoredWith ? `authored_with: ${authoredWith}\n` : "";
  // `rel` is the path this text is ABOUT TO BE WRITTEN TO, threaded in rather than
  // hard-coded (#1153 phase 4, rev-lead round 1 B2). The header names the file the
  // reader is looking at: a scaffold written to `.orrerix/workflow.yml` whose first
  // line says `.loomux/workflow.yml` names a path that repo does not have, which is
  // exactly the "reads one file, reports another" defect this phase exists to fix.
  return `# ${rel} — this repo's agent workflow (loomux #222).
# Committed on purpose: everyone who clones the repo gets the same roster.
# Orrerix reads it only when "Advanced orchestrator" is ticked in the launcher.

version: 1
${stamp}name: default

# BLOCKS — the agents a run may use. \`kind\` is a capability class and the list is
# closed (orchestrator | worker | reviewer | planner | manager): a workflow file can
# define any persona, but it can never grant a capability. A planner is read-only; a
# reviewer can review but never push; a worker gets a worktree; a manager (at most one)
# is the human's own interface pane and writes nothing.
blocks:
  - id: planner            # immutable, human-meaningful — edges and gates name THIS
    name: Planner          # display only; safe to rename at any time
    kind: planner
    cli: claude
    model: opus

  - id: worker
    name: Worker
    kind: worker
    cli: claude

  - id: reviewer
    name: Reviewer
    kind: reviewer
    cli: claude
    model: opus
    # A persona is optional: an inline \`prompt:\` (compiled to the CLI's native inline
    # agent) or a \`profile:\` path to a .github/agents/*.md file. Omit both and the
    # block runs orrerix's built-in role instructions.
    #
    # prompt: |
    #   Review ONLY for security defects: injection, authz, secrets, path traversal.

# EDGES — ADVISORY. They declare the intended path; the orchestrator still decides when
# to spawn what. (Its judgment about what can run in parallel is the thing that makes it
# good — a static DAG would replace that with something dumber.)
edges:
  - { from: planner, to: worker }
  - { from: worker, to: reviewer }

# GATES — ENFORCED. Orrerix refuses \`gh pr merge\` until every reviewer named here has
# recorded a PASS verdict. An agent cannot get around it: the refusal lives in the PATH
# shim, not in a prompt. Add a second reviewer to the list and it is a second reviewer
# that must actually pass — which is what makes multi-reviewer more than theatre.
gates:
  merge:
    require: all-pass      # or: threshold, with \`threshold: N\`
    reviewers: [reviewer]
`;
}

/** The optional top-level key recording which loomux WROTE this file — §4's "record the
 *  loomux version that authored it" (the Langflow `last_tested_version` lesson: when a file
 *  misbehaves, the first question is always which build produced it).
 *
 *  It is written EXACTLY ONCE, when the pane creates a new workflow, and never touched
 *  again: on an existing file it round-trips verbatim. That is deliberate — stamping it
 *  on every save would mean every human who opens the pane and changes a model name also
 *  produces a one-line diff nobody asked for, in a file whose whole point is a legible
 *  history. It records who authored the workflow, not who last looked at it.
 *
 *  A KNOWN key since #880 (`Workflow.authored_with`), where it used to ride the
 *  unknown-key bag: that round-tripped it correctly but left it invisible to anything
 *  asking what fields a workflow file has — and once an unknown key became a finding, it
 *  would have had the pane report a key loomux itself writes. The behavior above is
 *  unchanged; only its visibility is. */
export const AUTHORED_WITH_KEY = "authored_with";

/** The workflow loomux runs today, as a file: plan → work → review, with the reviewer's
 *  verdict gating the merge. The starting point a repo with no workflow file
 *  opens on, so the pane's empty state is a working example rather than a blank page.
 *
 *  `authoredWith` is the loomux version doing the creating; omit it and the key is simply
 *  not written (which is what the tests do, and what a caller with no version to hand
 *  should do — an `authored_with: unknown` would be worse than an absent key). */
export function starterWorkflow(authoredWith?: string): Workflow {
  return {
    version: WORKFLOW_VERSION,
    name: "default",
    ...(authoredWith ? { [AUTHORED_WITH_KEY]: authoredWith } : {}),
    blocks: [
      { id: "planner", name: "Planner", kind: "planner", cli: "claude", model: "opus" },
      { id: "worker", name: "Worker", kind: "worker", cli: "claude", model: "" },
      { id: "reviewer", name: "Reviewer", kind: "reviewer", cli: "claude", model: "opus" },
    ],
    edges: [
      { from: "planner", to: "worker" },
      { from: "worker", to: "reviewer" },
    ],
    gates: { merge: { require: "all-pass", reviewers: ["reviewer"], also: [] } },
  };
}

// ---------- the one call the view makes ----------

export interface WorkflowAnalysis {
  workflow: Workflow;
  /** Parse findings and validation findings, in that order — syntax first, because a
   *  file that didn't parse will also fail half the semantic rules and leading with those
   *  would bury the line number that actually explains it. */
  findings: Finding[];
  graph: WorkflowGraph;
}

/** Text in, everything the pane renders out. `knobs` (#687) is the pane's
 *  capability lookup — omitted (or answering `null`) simply skips the knob
 *  checks, so a re-analysis that happens before `agent_cli_knobs` replies reads
 *  exactly as it did before. */
export function analyzeWorkflow(text: string, knobs?: KnobLookup): WorkflowAnalysis {
  const { workflow, findings } = parseWorkflow(text);
  return {
    workflow,
    findings: [...findings, ...validateWorkflow(workflow, knobs)],
    graph: deriveGraph(workflow),
  };
}

/** The canonical formatter, as the pane's ✨ Format button uses it: read the file, write
 *  it back in the one canonical shape. Idempotent by construction. */
export function formatWorkflowText(text: string): string {
  return serializeWorkflow(parseWorkflow(text).workflow);
}
