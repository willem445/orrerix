// Model -> text for the repo's workflow file (#222; split out of workflowmodel.ts by #3498 F2):
// the canonical formatter, the comment-preserving serializer (#233), and the driver form's
// text rules that sit on top of that machinery (#1869/#1876). Imports workflowtypes.ts and
// workflowparse.ts. Design note: docs/design/content-panes.md (the preserving serializer)
// and docs/design/workflows.md; module map: docs/design/architecture.md.

import {
  WIP_STATUSES,
  isUnreadable,
  INTAKE_LABEL_KEYS,
} from "./workflowtypes.ts";
import type {
  YamlValue,
  WorkflowBlock,
  WorkflowEdge,
  WorkflowIntake,
  WorkflowIntakeLabels,
  WorkflowMergeQueue,
  WorkflowDriver,
  WorkflowResource,
  WorkflowTriage,
  WorkflowBoard,
  Workflow,
} from "./workflowtypes.ts";
import {
  stripComment,
  indentOf,
  splitKey,
  emitScalar,
  emitValue,
  parseWorkflow,
} from "./workflowparse.ts";

// ---------- YAML subset: writing (the canonical formatter) ----------
//
// One shape, always, so `git diff` shows what CHANGED and not how it was written: fixed
// key order (the order a human reads a block in — who it is, what it runs as, what it
// runs on, then the persona body last because it is the long one), edges grouped by
// their source and ordered by the blocks they connect, gate lists ordered the same way.
//
// Blocks themselves keep their AUTHORED order. That is the one place a "stable sort"
// would do harm: the roster reads top-to-bottom, and re-sorting it alphabetically on
// every save would churn the diff of the file it is supposed to keep legible.

/** A `|` block scalar, indented under its key. A prompt keeps its line breaks — folding
 *  them would change what the agent actually reads. */
function emitBlockScalar(key: string, text: string, indent: string): string[] {
  // A body that ends in a newline is `|` (clip); one that doesn't is `|-` (strip). That
  // is what makes prompt → YAML → prompt exact rather than approximately exact.
  const chomp = text.endsWith("\n") ? "" : "-";
  const body = text.replace(/\n$/, "").split("\n");
  // The INDENTATION INDICATOR (`|2`), and why it isn't optional (rev-5 F3): a plain `|` is
  // read back by dedenting to the FIRST CONTENT LINE's indent, so a prompt whose first line
  // is itself indented — a code snippet, an indented checklist, and it comes straight out of
  // the form's textarea — silently loses that indent on the next read. Same for a prompt
  // that opens with a blank line, where the "first content line" is the second one. Stating
  // the indent explicitly makes the reader's dedent independent of the content, which is the
  // only way this round-trips.
  const first = body[0] ?? "";
  const explicit = first === "" || /^\s/.test(first);
  const header = `|${explicit ? BLOCK_SCALAR_INDENT : ""}${chomp}`;
  const pad = " ".repeat(BLOCK_SCALAR_INDENT);
  return [`${indent}${key}: ${header}`, ...body.map((l) => (l ? `${indent}${pad}${l}` : ""))];
}

/** How far a block scalar's body is indented past its key. Both halves of the round-trip
 *  read it: the emitter pads by it, and the `|2` indicator it writes tells the reader to
 *  dedent by exactly it rather than by guessing from the content. */
const BLOCK_SCALAR_INDENT = 2;

function extraLines(extra: Record<string, YamlValue> | undefined, indent: string): string[] {
  if (!extra) return [];
  // The key goes through the emitter here too, for the same reason as in `emitValue` — an
  // unknown key is as arbitrary as an unknown value, and a key carrying a `: ` would
  // otherwise re-read as a different key with a different value. (`splitKey`/`unquote`
  // already read a quoted key; only the writing side was asymmetric.)
  return Object.keys(extra)
    .sort()
    .map((k) => `${indent}${emitScalar(k)}: ${emitValue(extra[k]!)}`);
}

/** `version:`, `name:` and any top-level unknown keys — the lines every workflow starts
 *  with. Factored out so the comment-preserving serializer (below) can regenerate just this
 *  piece when it — and only it — has changed, without duplicating the exact formatting rules. */
function emitFrontLines(w: Workflow): string[] {
  const out: string[] = [];
  out.push(`version: ${w.version}`);
  if (w.name) out.push(`name: ${emitScalar(w.name)}`);
  // Only when the file HAS one: this key is stamped once, at creation, and a save
  // must never invent it (see `AUTHORED_WITH_KEY`).
  if (w.authored_with !== undefined) out.push(`authored_with: ${emitScalar(w.authored_with)}`);
  out.push(...extraLines(w.extra, ""));
  return out;
}

/** A nested mapping section (`intake:`, `merge_queue:`, one `resources:` entry).
 *  A section with nothing under it emits `key: {}` rather than a bare `key:` — a
 *  bare key is YAML *null*, which reads back as "not declared at all", so the
 *  round-trip would silently delete a section someone deliberately wrote empty.
 *  Exactly the `blocks: []` reasoning (rev-5 F4), one level down. */
function emitMappingSection(key: string, indent: string, body: readonly string[]): string[] {
  if (!body.length) return [`${indent}${key}: {}`];
  return [`${indent}${key}:`, ...body];
}

/** The `intake:` section (#382 P1). Every field is emitted only when DECLARED:
 *  the engine resolves an absent one against its built-in profile, so writing the
 *  defaults out would turn "inherit" into "pin", silently, on the first save. */
function emitIntakeLines(intake: WorkflowIntake, indent = ""): string[] {
  const field = `${indent}  `;
  const body: string[] = [];
  if (intake.source !== undefined) body.push(`${field}source: ${emitScalar(intake.source)}`);
  if (intake.labels) body.push(...emitIntakeLabelLines(intake.labels, field));
  body.push(...extraLines(intake.extra, field));
  return emitMappingSection("intake", indent, body);
}

function emitIntakeLabelLines(labels: WorkflowIntakeLabels, indent: string): string[] {
  const field = `${indent}  `;
  const body: string[] = [];
  for (const key of INTAKE_LABEL_KEYS) {
    const v = labels[key];
    if (v !== undefined) body.push(`${field}${key}: ${emitScalar(v)}`);
  }
  body.push(...extraLines(labels.extra, field));
  return emitMappingSection("labels", indent, body);
}

/** The `merge_queue:` section (#581 §11.2). Same declared-only rule as `intake:`,
 *  and for a sharper reason: an absent block means the queue is OFF, so emitting
 *  `enabled: false` where the file said nothing would be a policy statement the
 *  human never made. */
function emitMergeQueueLines(mq: WorkflowMergeQueue, indent = ""): string[] {
  const field = `${indent}  `;
  const body: string[] = [];
  if (mq.enabled !== undefined) body.push(`${field}enabled: ${mq.enabled}`);
  if (mq.max_batch !== undefined) body.push(`${field}max_batch: ${mq.max_batch}`);
  if (mq.checks_timeout_minutes !== undefined) {
    body.push(`${field}checks_timeout_minutes: ${mq.checks_timeout_minutes}`);
  }
  body.push(...extraLines(mq.extra, field));
  return emitMappingSection("merge_queue", indent, body);
}

/** The `driver:` section (#1778 §5.3). Same declared-only rule as `intake:`
 *  and `merge_queue:`: an absent block means the driver is OFF, so emitting
 *  `enabled: false` where the file said nothing would be a policy statement
 *  the human never made. */
function emitDriverLines(dv: WorkflowDriver, indent = ""): string[] {
  const field = `${indent}  `;
  const body: string[] = [];
  if (dv.enabled !== undefined) body.push(`${field}enabled: ${dv.enabled}`);
  if (dv.max_review_rounds !== undefined) {
    body.push(`${field}max_review_rounds: ${dv.max_review_rounds}`);
  }
  if (dv.max_ci_attempts !== undefined) {
    body.push(`${field}max_ci_attempts: ${dv.max_ci_attempts}`);
  }
  if (dv.max_rebase_attempts !== undefined) {
    body.push(`${field}max_rebase_attempts: ${dv.max_rebase_attempts}`);
  }
  if (dv.lane_timeout_minutes !== undefined) {
    body.push(`${field}lane_timeout_minutes: ${dv.lane_timeout_minutes}`);
  }
  if (dv.fix_timeout_minutes !== undefined) {
    body.push(`${field}fix_timeout_minutes: ${dv.fix_timeout_minutes}`);
  }
  if (dv.drive_timeout_minutes !== undefined) {
    body.push(`${field}drive_timeout_minutes: ${dv.drive_timeout_minutes}`);
  }
  if (dv.plan_enabled !== undefined) body.push(`${field}plan_enabled: ${dv.plan_enabled}`);
  if (dv.plan_review_minutes !== undefined) {
    body.push(`${field}plan_review_minutes: ${dv.plan_review_minutes}`);
  }
  if (dv.planner_timeout_minutes !== undefined) {
    body.push(`${field}planner_timeout_minutes: ${dv.planner_timeout_minutes}`);
  }
  if (dv.fix_nonblocking_rounds !== undefined) {
    body.push(`${field}fix_nonblocking_rounds: ${dv.fix_nonblocking_rounds}`);
  }
  if (dv.auto_drive_on_done !== undefined) {
    body.push(`${field}auto_drive_on_done: ${dv.auto_drive_on_done}`);
  }
  body.push(...extraLines(dv.extra, field));
  return emitMappingSection("driver", indent, body);
}

/** The `resources:` section (#858) — a mapping of repo-chosen names to two numbers.
 *  Names are emitted in SORTED order, matching the engine's `BTreeMap`, because
 *  unlike the roster (whose order is meaning — the first block of a class is the
 *  default one) a resource map has no order to preserve. */
function emitResourcesLines(resources: Record<string, WorkflowResource>, indent = ""): string[] {
  const field = `${indent}  `;
  const body: string[] = [];
  for (const name of Object.keys(resources).sort()) {
    const r = resources[name]!;
    const inner: string[] = [];
    if (r.slots !== undefined) inner.push(`${field}  slots: ${r.slots}`);
    if (r.max_hold_minutes !== undefined) {
      inner.push(`${field}  max_hold_minutes: ${r.max_hold_minutes}`);
    }
    inner.push(...extraLines(r.extra, `${field}  `));
    body.push(...emitMappingSection(emitScalar(name), field, inner));
  }
  return emitMappingSection("resources", indent, body);
}

/** The `board:` section (#1175). Same declared-only rule as `intake:`/`merge_queue:`,
 *  and the same three-state care on `enforce:`: absent and `false` mean the same thing
 *  to the engine, so a save must not convert one into the other behind the human's back.
 *
 *  Caps are emitted in `WIP_STATUSES` order — the board's own order, and the order the
 *  engine's struct declares them in — rather than sorted, so a file reads top-to-bottom
 *  the way the board flows. */
/** The `triage:` section (#3304 S1). Same declared-only rule as `board:`: a key the
 *  file did not write is not written back, because absent and the engine's default
 *  are the same state and converting one into the other behind the human's back is
 *  the data loss `enforce:` taught. `kinds` emits as a flow list, like
 *  `reviewers:`/`also:`, through `emitScalar` so a value is never re-read as two. */
function emitTriageLines(triage: WorkflowTriage, indent = ""): string[] {
  const field = `${indent}  `;
  const body: string[] = [];
  if (triage.enabled !== undefined) body.push(`${field}enabled: ${triage.enabled}`);
  if (triage.provider !== undefined) {
    body.push(`${field}provider: ${emitScalar(triage.provider)}`);
  }
  if (triage.kinds !== undefined) {
    body.push(`${field}kinds: [${triage.kinds.map(emitScalar).join(", ")}]`);
  }
  if (triage.max_defer_minutes !== undefined) {
    body.push(`${field}max_defer_minutes: ${triage.max_defer_minutes}`);
  }
  body.push(...extraLines(triage.extra, field));
  return emitMappingSection("triage", indent, body);
}

function emitBoardLines(board: WorkflowBoard, indent = ""): string[] {
  const field = `${indent}  `;
  const body: string[] = [];
  if (board.wip) {
    const inner: string[] = [];
    for (const status of WIP_STATUSES) {
      const v = board.wip[status];
      if (v !== undefined) inner.push(`${field}  ${emitScalar(status)}: ${v}`);
    }
    inner.push(...extraLines(board.wipExtra, `${field}  `));
    body.push(...emitMappingSection("wip", field, inner));
  }
  if (board.enforce !== undefined) body.push(`${field}enforce: ${board.enforce}`);
  body.push(...extraLines(board.extra, field));
  return emitMappingSection("board", indent, body);
}

/** One block entry, canonical key order, no leading/trailing blank line. `markerIndent` is
 *  where the `-` sits — 2 (this build's own convention) by default, but the comment-preserving
 *  serializer passes whatever indent the SURROUNDING roster already uses (0 for a same-column
 *  sequence, or whatever else a hand-written file chose), so a regenerated item never mixes a
 *  different marker indent into a sequence that has to share exactly one (#233 non-blocking #2,
 *  and see `splitBlockItems`'s own note on why mixing indents is invalid, not just inconsistent). */
function emitBlockLines(b: WorkflowBlock, markerIndent = 2): string[] {
  const dash = " ".repeat(markerIndent);
  const field = " ".repeat(markerIndent + 2);
  const out: string[] = [];
  out.push(`${dash}- id: ${emitScalar(b.id)}`);
  out.push(`${field}name: ${emitScalar(b.name)}`);
  out.push(`${field}kind: ${emitScalar(b.kind)}`);
  if (b.role_hint !== undefined) out.push(`${field}role_hint: ${emitScalar(b.role_hint)}`);
  out.push(`${field}cli: ${emitScalar(b.cli)}`);
  if (b.model) out.push(`${field}model: ${emitScalar(b.model)}`);
  // #687: with the model they modify, and only when declared — a block that
  // pinned neither serializes byte for byte as it did before.
  if (b.effort !== undefined) out.push(`${field}effort: ${emitScalar(b.effort)}`);
  if (b.context !== undefined) out.push(`${field}context: ${emitScalar(b.context)}`);
  if (b.profile !== undefined) out.push(`${field}profile: ${emitScalar(b.profile)}`);
  // A flow list, like `reviewers:`/`also:` — and every entry goes through
  // `emitScalar`, which quotes for flow context, so `Bash(gh pr view --json
  // title,body)` survives instead of re-reading as two entries (rev-5 F1).
  if (b.allow !== undefined) out.push(`${field}allow: [${b.allow.map(emitScalar).join(", ")}]`);
  // #1457: only when declared — a block that named no remote serializes byte for
  // byte as it did before the key existed.
  if (b.remote !== undefined) out.push(`${field}remote: ${emitScalar(b.remote)}`);
  // #2850: only when declared — same byte-for-byte posture as `remote`.
  if (b.driver !== undefined) out.push(`${field}driver: ${emitScalar(b.driver)}`);
  // #3407: only when declared — same byte-for-byte posture as `remote`.
  if (b.cache_ttl_minutes !== undefined) out.push(`${field}cache_ttl_minutes: ${b.cache_ttl_minutes}`);
  out.push(...extraLines(b.extra, field));
  if (b.prompt !== undefined) out.push(...emitBlockScalar("prompt", b.prompt, field));
  return out;
}

/** The `edges:` section, or `[]` (nothing pushed) when there are no edges. */
function emitEdgesLines(edges: readonly WorkflowEdge[], order: Map<string, number>): string[] {
  const groups = groupEdges(edges, order);
  if (!groups.length) return [];
  const out: string[] = ["edges:"];
  for (const g of groups) {
    const to = g.to.length === 1 ? emitScalar(g.to[0]!) : `[${g.to.map(emitScalar).join(", ")}]`;
    out.push(`  - { from: ${emitScalar(g.from)}, to: ${to} }`);
  }
  return out;
}

/** The `gates:` section, or `[]` (nothing pushed) when there is nothing to gate. */
function emitGatesLines(w: Workflow, order: Map<string, number>): string[] {
  const gate = w.gates.merge;
  if (!gate && !w.gates.extra) return [];
  const out: string[] = ["gates:"];
  if (gate) {
    out.push("  merge:");
    out.push(`    require: ${emitScalar(gate.require)}`);
    if (gate.threshold !== undefined) out.push(`    threshold: ${gate.threshold}`);
    out.push(`    reviewers: [${sortByBlocks(gate.reviewers, order).map(emitScalar).join(", ")}]`);
    if (gate.also.length) out.push(`    also: [${gate.also.map(emitScalar).join(", ")}]`);
    if (gate.max_diff_lines !== undefined) out.push(`    max_diff_lines: ${gate.max_diff_lines}`);
    // #1176. A block sequence of mappings — the one place this emitter writes
    // one. `emitScalar` already quotes a value starting with `*`, which every
    // `**/…` glob does and which YAML would otherwise read as an ALIAS.
    if (gate.routing?.length) {
      out.push("    routing:");
      for (const rule of gate.routing) {
        out.push(`      - paths: [${rule.paths.map(emitScalar).join(", ")}]`);
        out.push(
          `        reviewers: [${sortByBlocks(rule.reviewers, order).map(emitScalar).join(", ")}]`
        );
      }
    }
  }
  out.push(...extraLines(w.gates.extra, "  "));
  return out;
}

/** Render the workflow in canonical form. `parseWorkflow(serializeWorkflow(w)).workflow`
 *  deep-equals `w`, and serializing twice is a no-op — the two properties the file's
 *  legibility rests on, both pinned in test/workflowparse.test.ts.
 *
 *  This is the FULL rewrite: fixed key order, no comments, no matter what was there before.
 *  It is what every form/canvas edit used to go through unconditionally (#233's whole
 *  complaint) and is now reserved for the explicit **Format** action and for a model that
 *  has no prior text to diff against (a brand-new file). Everyday edits go through
 *  `serializeWorkflowPreserving`, below, which reuses this file's own emitters for whatever
 *  it can't reuse verbatim from the original text. */
export function serializeWorkflow(w: Workflow): string {
  const order = blockOrder(w);
  const out: string[] = [...emitFrontLines(w)];

  // An EMPTY roster emits `blocks: []`, not a bare `blocks:` (rev-5 F4). A bare key is
  // YAML `null`, so the pane would re-read its own output as a malformed shape and report a
  // syntax-ish error against text it had just written itself — on top of the honest
  // `no-blocks`. Deleting the last block in the form is the ordinary way to get here.
  out.push("", w.blocks.length ? "blocks:" : "blocks: []");
  for (const b of w.blocks) out.push(...emitBlockLines(b));

  const edgeLines = emitEdgesLines(w.edges, order);
  if (edgeLines.length) out.push("", ...edgeLines);

  const gateLines = emitGatesLines(w, order);
  if (gateLines.length) out.push("", ...gateLines);

  // The policy sections, in the engine's own `RawWorkflow` field order — one
  // canonical shape means one order, and taking it from the type that defines the
  // schema is the only choice that can't drift for a reason nobody can name.
  for (const lines of [
    w.intake ? emitIntakeLines(w.intake) : [],
    w.merge_queue ? emitMergeQueueLines(w.merge_queue) : [],
    w.driver ? emitDriverLines(w.driver) : [],
    w.resources ? emitResourcesLines(w.resources) : [],
    w.board ? emitBoardLines(w.board) : [],
    w.triage ? emitTriageLines(w.triage) : [],
  ]) {
    if (lines.length) out.push("", ...lines);
  }

  return out.join("\n") + "\n";
}

/** Block id → its position in the roster. The sort key for everything that REFERENCES a
 *  block (edges, reviewer lists), so those lists read in graph order instead of
 *  alphabetical order — and so an unrelated rename can't reshuffle them. */
function blockOrder(w: Workflow): Map<string, number> {
  return new Map(w.blocks.map((b, i) => [b.id, i]));
}

function sortByBlocks(ids: readonly string[], order: Map<string, number>): string[] {
  // Dangling references (not in the roster) sort last, alphabetically: they are exactly
  // what the validation pass is about to complain about, so they belong where they are
  // easy to see rather than interleaved with the real ones.
  const seen = new Set<string>();
  const uniq = ids.filter((id) => (seen.has(id) ? false : (seen.add(id), true)));
  return uniq.sort((a, b) => {
    const ia = order.get(a),
      ib = order.get(b);
    if (ia !== undefined && ib !== undefined) return ia - ib;
    if (ia !== undefined) return -1;
    if (ib !== undefined) return 1;
    return a.localeCompare(b);
  });
}

/** Collapse the edge list into one entry per source (`{ from: worker, to: [a, b] }`),
 *  deduped and ordered by the roster. The fan-out form is how the schema sketch writes
 *  it and how a human reads it; the model keeps edges flat because every graph question
 *  (reachability, in-degree) is asked of pairs. */
function groupEdges(
  edges: readonly WorkflowEdge[],
  order: Map<string, number>
): { from: string; to: string[] }[] {
  const byFrom = new Map<string, string[]>();
  for (const e of edges) {
    const list = byFrom.get(e.from) ?? [];
    if (!list.includes(e.to)) list.push(e.to);
    byFrom.set(e.from, list);
  }
  return sortByBlocks([...byFrom.keys()], order).map((from) => ({
    from,
    to: sortByBlocks(byFrom.get(from)!, order),
  }));
}

// ---------- comment-preserving serialization (#233) ----------
//
// The bug #233 was filed against: `serializeWorkflow` is a FULL rewrite, and every form or
// canvas edit called it on the whole workflow, every time — so dragging one edge in a file
// with 60 comment lines produced a 60-comment-line diff. The interim mitigation (#231, rev-15)
// was honest about that trade and warned before it happened; this is the real fix.
//
// The approach: reuse the ORIGINAL TEXT'S OWN LINES wherever the new model says nothing
// changed there, and fall back to the canonical emitters (above) only for the piece that
// actually changed. Correctness rests on one guarantee: a segment of original text is only
// ever reused when the NEW block/section is `deepEqual` to what parsing THAT SAME original
// text produced — so splicing it back in can only ever reproduce what was already there.
// There is no attempt to re-attach a comment to a field that changed underneath it; that is
// deliberately out of scope (see the module comment at the top of this file) — the bar is
// "untouched regions keep their comments and formatting", not full-fidelity diffing.

/** Structural equality for the JSON-shaped values this schema is built from (`YamlValue`,
 *  `WorkflowBlock`, `WorkflowGates`, …). Order-independent for object keys (so `extra` bags
 *  built from a `Map`/`Object.keys` in a different order still compare equal), order-sensitive
 *  for arrays (an edge list is a sequence, not a set — see `groupEdges`'s own docblock on why
 *  ordering there is meaningful). This is the one thing that MUST NOT false-positive: comparing
 *  "equal" when something actually changed would splice stale text back over a real edit. */
function deepEqualValue(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Array.isArray(a) || Array.isArray(b)) {
    if (!Array.isArray(a) || !Array.isArray(b) || a.length !== b.length) return false;
    return a.every((v, i) => deepEqualValue(v, b[i]));
  }
  const ao = a as Record<string, unknown>;
  const bo = b as Record<string, unknown>;
  const ak = Object.keys(ao);
  const bk = Object.keys(bo);
  if (ak.length !== bk.length) return false;
  return ak.every((k) => Object.prototype.hasOwnProperty.call(bo, k) && deepEqualValue(ao[k], bo[k]));
}

/** A `#` at the start of a line or after whitespace, ignoring one inside a quoted scalar —
 *  reusing `stripComment`'s own quote-awareness so a `#` inside a string doesn't fool the
 *  line-is-significant check below. */
const isSignificantLine = (line: string): boolean => stripComment(line).trim() !== "";

/** The header pattern for a `|`/`>` block scalar — the same one `afterKey` (the real reader,
 *  workflowparse.ts) tests, kept in one place so the two never drift. */
const BLOCK_SCALAR_HEADER_RE = /^[|>](?:\d[-+]?|[-+]?\d?)$/;

/** Indices into `seg` that fall inside a `|`/`>` block scalar's BODY — content, never trivia,
 *  no matter what character they start with. #233 B2: a prompt's last line can legitimately be
 *  `# a checklist item`, and the naive "does it look like a comment" test used for trivia-
 *  peeling (below) would otherwise steal it onto whatever entry/item comes next — silently, and
 *  only visible once a SIBLING gets edited and that stolen line never comes back.
 *
 *  Scans `seg` from the front exactly once, tracking whether it is currently inside a scalar
 *  body (`scalarIndent`, the indent of the governing `key: |` line — the body ends at the first
 *  non-blank line whose indent drops back to that column or shallower, same rule `blockScalar`
 *  itself uses). Safe to run independently on each already-bounded segment (an entry's
 *  `content`, or one block item's `raw`): a scalar can never span the boundary between two such
 *  segments, because its own governing key is always MORE indented than either boundary the
 *  outer scan looks for (column 0 for a top-level key, `markerIndent` for a block item), so the
 *  boundary is always found before the scalar could bleed across it. */
function opaqueScalarIndices(seg: readonly string[]): Set<number> {
  const opaque = new Set<number>();
  let scalarIndent: number | null = null;
  for (let k = 0; k < seg.length; k++) {
    const line = seg[k]!;
    if (scalarIndent !== null) {
      if (line.trim() !== "" && indentOf(line) <= scalarIndent) {
        scalarIndent = null; // dedented back out — the scalar body ends here, not opaque
      } else {
        opaque.add(k);
        continue;
      }
    }
    if (!isSignificantLine(line)) continue;
    const stripped = stripComment(line);
    const split = splitKey(stripped.trim());
    if (split && BLOCK_SCALAR_HEADER_RE.test(split.rest)) scalarIndent = indentOf(stripped);
  }
  // A scalar that runs to the very END of `seg` with no dedent line to close it (its governing
  // key was the LAST field of the LAST item in this segment) leaves `scalarIndent` open through
  // every trailing blank line — but a block scalar's OWN reader (`blockScalar`, workflowparse.ts) already
  // drops its trailing blank body lines during chomping, so there is nothing structural left for
  // those blanks to belong to. Un-mark a purely trailing run of them so the ordinary trivia peel
  // can still separate this item from whatever comes after it, instead of leaving that blank line
  // stuck as "content" and then getting a SECOND, synthetic one stacked in front of it whenever
  // the next item is regenerated (round 2's stray-double-blank-line finding).
  let end = seg.length - 1;
  while (end >= 0 && seg[end]!.trim() === "") opaque.delete(end--);
  return opaque;
}

/** Pop blank/comment lines off the end of `seg`, INTO `pendingTrivia` (in original order), but
 *  never a line the scalar scan above marked opaque — see #233 B2. Mutates both arrays. */
function peelTrailingTrivia(seg: string[], pendingTrivia: string[]): void {
  const opaque = opaqueScalarIndices(seg);
  while (seg.length && !opaque.has(seg.length - 1) && !isSignificantLine(seg[seg.length - 1]!)) {
    pendingTrivia.unshift(seg.pop()!);
  }
}

/** One top-level key's own leading trivia (the comment/blank lines that precede it, read as
 *  "about" that key) plus its full raw text: the key's own line (`header`) and everything
 *  indented under it (`content`), all as ORIGINAL, UNMODIFIED source lines. */
interface TopEntry {
  key: string;
  /** Trivia lines, then the `key: …` line itself. */
  header: string[];
  /** Everything more indented than column 0 that followed the key line, verbatim. */
  content: string[];
}

interface SplitDocument {
  /** Comment/blank lines before the very first top-level key — file-level commentary that
   *  belongs to no single field, so it is always kept rather than tied to `front`. */
  preamble: string[];
  /** One entry per top-level key, in the order the source actually wrote them. */
  entries: TopEntry[];
  /** Comment/blank lines dangling after the last top-level key's content, through EOF. */
  trailer: string[];
}

/** Is `line` (already known significant) a `-` sequence marker at exactly `indent`? Shared by
 *  the "same-indent sequence" check below and by `splitBlockItems`'s own item-boundary test. */
function isDashAt(line: string, indent: number): boolean {
  const t = stripComment(line).trim();
  return indentOf(line) === indent && (t === "-" || t.startsWith("- "));
}

/** Do two driver models differ ONLY in their `enabled` value? The splice path's guard
 *  (#1869 review round 3): the toggle is the only writer of `enabled`, and when
 *  everything else is equal the section's own lines can be reused with just that one
 *  value rewritten — the deepEqual reuse guarantee, narrowed to the one field that
 *  changed. Explicit field copy rather than a destructuring omit, so no unused-binding
 *  lint has an opinion about the field this exists to skip. */
function driverDiffersOnlyInEnabled(a: WorkflowDriver, b: WorkflowDriver): boolean {
  if (a.enabled === b.enabled) return false; // not a value flip — the deepEqual path owns it
  const rest = (d: WorkflowDriver): Omit<WorkflowDriver, "enabled"> => ({
    max_review_rounds: d.max_review_rounds,
    max_ci_attempts: d.max_ci_attempts,
    max_rebase_attempts: d.max_rebase_attempts,
    lane_timeout_minutes: d.lane_timeout_minutes,
    fix_timeout_minutes: d.fix_timeout_minutes,
    drive_timeout_minutes: d.drive_timeout_minutes,
    plan_enabled: d.plan_enabled,
    plan_review_minutes: d.plan_review_minutes,
    planner_timeout_minutes: d.planner_timeout_minutes,
    fix_nonblocking_rounds: d.fix_nonblocking_rounds,
    auto_drive_on_done: d.auto_drive_on_done,
    extra: d.extra,
  });
  return deepEqualValue(rest(a), rest(b));
}

/** The enabled-line rewrite for one line of a `driver:` block: what the suffix
 *  match matches on this line (`matched` — case-sensitively, exactly as the file
 *  spells it) and the comment-stripped, right-trimmed line (`head`) the
 *  replacement is built from. Null when the line is not a rewritable enabled
 *  line: a value that does not end in a true/false spelling. THE one place the
 *  suffix match lives — the splice's bail, the splice's replacement, and the
 *  flip note's condition all read THIS, so the note cannot promise a
 *  preservation the write does not perform. (#1876 review 2: the note
 *  additionally requires `matched !== wanted` — its own no-op condition, which
 *  is the note's surplus over the splice, not a second copy of this predicate.) */
function enabledLineRewrite(line: string): { matched: string; head: string } | null {
  const head = stripComment(line).trimEnd();
  const matched = /(true|false)$/i.exec(head);
  return matched ? { matched: matched[0], head } : null;
}

/** Rewrite a `driver:` section's own lines so its `enabled:` line carries `value`,
 *  reusing every other line verbatim — comments and formatting included (#1869
 *  review round 3). The value is REPLACED in place when the line exists (a trailing
 *  comment on it is kept); the line is inserted ahead of the block's first field
 *  when the file never wrote one.
 *
 *  Two RESIDUALS, stated rather than absorbed (#1869 review round 5):
 *
 *  1. The bail path (null) is NOT comment-preserving. The caller's fallback is
 *     canonical regeneration, which drops the section's interior comments — the
 *     same trade every other form edit in the pane has always made for a section
 *     whose content changed. The bail fires on a shape this scan cannot rewrite
 *     in place: no field line to anchor to, or an `enabled:` value whose text
 *     does not END in `true`/`false`. The rewrite is a suffix match, and the
 *     reachable case — reproduced through parse → toggle → serialize → re-parse,
 *     not reasoned about (#1869 review 7) — is `notfalse` + ON, which rewrites
 *     the line to `nottrue`: the write lands, but the reader refuses `nottrue`
 *     (a bad-value finding), the re-parsed file still has no `enabled` for the
 *     model to read, and the rebuilt inspector renders the checkbox OFF again —
 *     an inert click, with no pane/file divergence at any point. (`nottrue`
 *     itself leaves the model's `enabled` undefined from the start, so the
 *     checkbox renders OFF there too and its ON click is a pure no-op: the
 *     rewritten line is byte-identical to what the file already said.) The bound
 *     is residual 2's, and it is what keeps this
 *     from being a live hazard: the pane already flags such a value as a bad
 *     value and serde refuses the type, so the file was unloadable before the
 *     rewrite — no loadable file can be made wrong. That residual is pinned by a
 *     test (a fixture with an interior comment and an `enabled: yes` line really
 *     does lose the comment), so it cannot silently go false.
 *
 *  2. A file carrying a DUPLICATE `enabled:` key has this rewrite touch only the
 *     FIRST occurrence, while the reader keeps the LAST. The bound: serde refuses
 *     a duplicate field, so such a file is already unloadable — the rewrite
 *     cannot make a loadable file wrong, only an unloadable one differently
 *     spelled. Not pinned: there is no behavior of a loadable file at stake. */
function spliceEnabledLine(content: readonly string[], value: boolean): string[] | null {
  const wanted = value ? "true" : "false";
  const opaque = opaqueScalarIndices(content as string[]);
  const firstField = content.find((l, i) => !opaque.has(i) && isSignificantLine(l));
  if (firstField === undefined) return null;
  const indent = indentOf(firstField);
  for (let i = 0; i < content.length; i++) {
    const line = content[i]!;
    if (opaque.has(i) || !isSignificantLine(line) || indentOf(line) !== indent) continue;
    const split = splitKey(stripComment(line).trim());
    if (!split || split.key !== "enabled") continue;
    const rewrite = enabledLineRewrite(line);
    if (!rewrite) return null; // an unexpected value spelling — bail
    const replaced =
      rewrite.head.slice(0, rewrite.head.length - rewrite.matched.length) +
      wanted +
      line.slice(rewrite.head.length);
    return [...content.slice(0, i), replaced, ...content.slice(i + 1)];
  }
  // No `enabled:` line in the block: insert it as the first field. Leading comment
  // or blank lines stay ABOVE it — they introduce the section, and regeneration
  // would drop them entirely.
  for (let i = 0; i < content.length; i++) {
    if (!opaque.has(i) && isSignificantLine(content[i]!)) {
      return [...content.slice(0, i), `${" ".repeat(indent)}enabled: ${wanted}`, ...content.slice(i)];
    }
  }
  return null;
}

/** Split source text into its top-level keys' raw line ranges, WITHOUT re-parsing their
 *  values — this only needs to know where each key's text begins and ends so the preserving
 *  serializer can splice by that boundary. Returns null for any shape this simple scan isn't
 *  confident about (a document that doesn't open at column 0, for instance) — the caller's
 *  fallback is `serializeWorkflow`, i.e. today's behavior, never a guess that could corrupt
 *  the file. */
function splitDocument(text: string): SplitDocument | null {
  const lines = text.replace(/^﻿/, "").split(/\r?\n/);
  const entries: TopEntry[] = [];
  let pendingTrivia: string[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i]!;
    if (!isSignificantLine(line)) {
      pendingTrivia.push(line);
      i++;
      continue;
    }
    // Tabs are never a safe column to reason about (the real reader flags them as a syntax
    // error rather than a column), and a line that isn't the mapping's `key: value` shape
    // means this scan doesn't understand the document well enough to splice it safely.
    if (/^[ ]*\t/.test(line) || indentOf(line) !== 0) return null;
    const split = splitKey(stripComment(line).trim());
    if (!split) return null;
    // An ORPHAN dash sequence — a `- id: …` line at column 0 that is not itself the same-column
    // continuation of a preceding empty-rest key (that case is handled above, per-entry, via
    // `sameColumnSeq`) — is not a top-level key at all; `splitKey` only returned one here because
    // "- id: a" contains a `: ` too. Reading it as a fresh key is exactly the B1 mistake one
    // level up: the real reader's `mapping()` would have stopped at this line already (filed
    // separately — the reader silently truncates here instead of raising a finding), so trusting
    // this scan's own read of it could front-splice roster content or lose it outright. Bail.
    if (split.key.startsWith("-")) return null;
    const header = [...pendingTrivia, line];
    pendingTrivia = [];
    i++;

    // #233 B1: `blocks:` (etc.) with NOTHING after the colon may be followed by its sequence
    // at the SAME column (0) — `afterKey` (the real reader, workflowparse.ts) accepts this, and a scan
    // that didn't would read each `- id: …` line as its own bogus top-level key, splicing
    // roster content into `front` and silently discarding everything from that point on (the
    // real reader, reading the reconstructed text top-down, hits a `-`-prefixed line where it
    // expects a key and stops there — the corruption the reviewer found). So: peek past this
    // key's own trivia for the first significant line, and if it is a same-column dash, this
    // key's content runs until a line that is neither MORE indented than 0 nor another
    // same-column dash — i.e. until an actual new key, not merely the next roster entry.
    let sameColumnSeq = false;
    if (split.rest === "") {
      let j = i;
      while (j < lines.length && !isSignificantLine(lines[j]!)) j++;
      if (j < lines.length && isDashAt(lines[j]!, 0)) sameColumnSeq = true;
    }

    const content: string[] = [];
    while (i < lines.length) {
      const l = lines[i]!;
      if (!isSignificantLine(l) || indentOf(l) !== 0) {
        content.push(l);
        i++;
        continue;
      }
      if (sameColumnSeq && isDashAt(l, 0)) {
        content.push(l);
        i++;
        continue;
      }
      break; // a genuine new top-level key
    }
    // The tail of `content` may be blank/comment lines that read as commentary on the NEXT
    // key, not this one (a section-header comment sitting just above `edges:`, say) — peel
    // them back off so they travel with whatever comes after instead of this entry. Never a
    // line inside a block scalar's body, even if it starts with `#` (#233 B2).
    peelTrailingTrivia(content, pendingTrivia);
    entries.push({ key: split.key, header, content });
  }
  const trailer = pendingTrivia;
  if (!entries.length) return { preamble: [], entries: [], trailer };
  // The document's own preamble is the leading trivia of the very first entry — peeled off
  // into its own bucket so it survives even when that first key's OWN content changes (the
  // comment at the top of the file is about the whole roster, not specifically about
  // `version:`).
  const first = entries[0]!;
  let k = 0;
  while (k < first.header.length && !isSignificantLine(first.header[k]!)) k++;
  const preamble = first.header.slice(0, k);
  entries[0] = { ...first, header: first.header.slice(k) };
  return { preamble, entries, trailer };
}

/** One roster entry's raw source lines (its own leading trivia and everything through its last
 *  field), and the column its `-` sits at. */
interface BlockItems {
  items: string[][];
  /** The indent every item's `-` was written at — 0 (same-column-as-`blocks:`) or some N>0.
   *  Whatever it is, a REGENERATED item (below) is emitted at this SAME indent, never a
   *  hardcoded one — mixing two marker indents in one YAML sequence is invalid, not just
   *  inconsistent (#233 non-blocking #2). */
  indent: number;
}

/** Split a `blocks:` key's content (everything indented under it) into one raw-line segment
 *  per roster entry, each still carrying its own leading trivia. Returns `[]` items for an
 *  empty or flow-style (`blocks: []`) roster, and `null` when the shape isn't the plain block
 *  sequence this scan understands (mixed indentation, a marker column shared with something
 *  that isn't a fresh item, …) — the caller treats `null` exactly like "nothing to reuse" and
 *  regenerates every item, at this build's own two-space indent. */
function splitBlockItems(content: string[]): BlockItems | null {
  const firstSig = content.findIndex(isSignificantLine);
  if (firstSig === -1) return { items: [], indent: 2 };
  const markerIndent = indentOf(content[firstSig]!);
  const isItemStart = (l: string): boolean => isDashAt(l, markerIndent);
  if (!isItemStart(content[firstSig]!)) return null;

  const items: string[][] = [];
  let pending: string[] = [];
  let i = 0;
  while (i < content.length) {
    if (!isSignificantLine(content[i]!)) {
      pending.push(content[i]!);
      i++;
      continue;
    }
    if (!isItemStart(content[i]!)) return null;
    const raw = [...pending, content[i]!];
    pending = [];
    i++;
    while (i < content.length && !(isSignificantLine(content[i]!) && isItemStart(content[i]!))) {
      raw.push(content[i]!);
      i++;
    }
    peelTrailingTrivia(raw, pending); // never steals a scalar body line (#233 B2)
    items.push(raw);
  }
  return { items, indent: markerIndent };
}

/** The top-level keys that are each their own preservable SECTION, in the order they
 *  take when the ORIGINAL document didn't declare them — which is the order
 *  `serializeWorkflow` writes, which is the engine's own `RawWorkflow` field order. A
 *  section the document DID declare keeps the position it already had (see the entry
 *  walk in `serializeWorkflowPreserving`): a file that puts `merge_queue:` above
 *  `blocks:` — as this repo's own does — must not have it relocated by an edit to
 *  something else.
 *
 *  Everything else at the top level (`version:`, `name:`, `authored_with:`, any key
 *  this build doesn't know) is one indivisible "front" piece, and membership here is
 *  not cosmetic: a key listed here is reused, or regenerated, on its own, while a key
 *  that isn't rides the front. Promoting `merge_queue:` out of the unknown-key bag
 *  (where it sat until #880, re-emitted as a flattened flow mapping) into this list is
 *  what lets an edit to it keep the comment lines above it, and what stops an unrelated
 *  `name:` edit from flattening it.
 *
 *  This one list is the source of BOTH the membership test and the emitter table in
 *  `serializeWorkflowPreserving` — a `Record` over exactly this union — so a section
 *  added here without an emitter is a COMPILE error rather than a section that quietly
 *  stops being written to the file at all. */
const SECTION_ORDER = [
  "blocks",
  "edges",
  "gates",
  "intake",
  "merge_queue",
  "driver",
  "resources",
  "board",
  "triage",
] as const;

type SectionKey = (typeof SECTION_ORDER)[number];

const TOP_SECTION_KEYS: ReadonlySet<string> = new Set<string>(SECTION_ORDER);

/** Does this `key: …` line already carry a value on it — `resources: {}`, `blocks: []`, a
 *  hand-written one-line flow mapping — as opposed to being the bare block header (`resources:`)
 *  that block-indented children are allowed to follow? Quote- and flow-aware, because it asks
 *  `splitKey`, the real reader's own splitter, rather than looking for a colon. A line this scan
 *  can't read as a key at all counts as carrying a value: the safe answer is the one that makes
 *  the caller REPLACE it rather than write children under something it doesn't understand. */
function keyLineHasInlineValue(line: string): boolean {
  const split = splitKey(stripComment(line).trim());
  return !split || split.rest !== "";
}

/** The header lines to write for a section whose BODY is being regenerated: the original's own
 *  leading trivia (the comment that introduces the SECTION — see `pushSection`), then a `key:`
 *  line that AGREES with the body about to follow it.
 *
 *  Reusing the original key line verbatim is only safe when the two forms already agree. A key
 *  line carrying an inline value cannot take block children: splicing a regenerated body under
 *  the empty-mapping form this file's own emitter writes (`emitMappingSection`) produced
 *
 *      resources: {}
 *        catfish: {}
 *
 *  which is not YAML at all — so the pane disabled the form over text it had just written itself
 *  (#1090). The inverse is as bad and silent: a section emptied back down to `key: {}` kept its
 *  bare `resources:` header, and a bare key is YAML *null*, i.e. "never declared" — deleting a
 *  section the human deliberately left empty, which is the whole reason `emitMappingSection`
 *  writes `{}` in the first place (rev-5 F4).
 *
 *  So the original key line is reused only when BOTH it and the regenerated one are bare block
 *  headers; otherwise the canonical line wins, and the original's own trailing comment rides
 *  along with it (that comment is about the section, not about the spelling that had to change).
 *  "Both carry an inline value" is NOT a reason to reuse: an inline value is the section's whole
 *  content, so keeping `resources: { build: { slots: 2 } }` over a regenerated `resources: {}`
 *  would silently undo the deletion that emptied it. */
function sectionHeaderLines(entry: TopEntry, keyLine: string): string[] {
  const trivia = entry.header.slice(0, -1);
  const original = entry.header[entry.header.length - 1]!;
  if (!keyLineHasInlineValue(original) && !keyLineHasInlineValue(keyLine)) {
    return [...trivia, original];
  }
  // Everything the comment-stripper left behind, trailing whitespace included, so `resources: {}
  // # pools` re-emits as `resources: # pools` and not as `resources:# pools`.
  const comment = original.slice(stripComment(original).trimEnd().length);
  return [...trivia, keyLine + comment];
}

/** Render the workflow the way a form or canvas edit should: reusing the ORIGINAL text's own
 *  lines — comments, blank-line runs, key order, quoting style, all of it — for every top-level
 *  piece the edit didn't touch, and falling back to the canonical emitters only for the piece
 *  that changed.
 *
 *  "Piece" is deliberately coarse — `front` (version/name/unknown top keys), each block in the
 *  roster BY ID, the whole `edges:` section, the whole `gates:` section — not a per-field diff
 *  within one of them. That is the bar #233 sets (comment-preserving for UNTOUCHED regions;
 *  "edited nodes serialize cleanly" — i.e. canonically — is enough for the parts that changed),
 *  and it is also what keeps this tractable against a hand-rolled parser: matching a whole
 *  block by id and `deepEqualValue` is a much smaller claim than re-attaching a trailing
 *  comment to the one field it happened to sit next to.
 *
 *  Falls back to `serializeWorkflow` (today's full rewrite) whenever `originalText` isn't
 *  READABLE — `isUnreadable`, the same predicate the view's `syntaxBroken` gates the form on
 *  (#233 B3), not the broader `hasErrors` (a `version-unsupported` file is still editable here,
 *  and must not silently lose its comments on the first edit just because *some* finding fired)
 *  — or when this scan doesn't trust its own read of the top-level shape. Always the SAFE
 *  direction, never a guess that could reuse text for content it no longer describes.
 *
 *  The original text's own line ending is kept for the whole output (CRLF in, CRLF out) —
 *  `splitDocument` reads via `split(/\r?\n/)`, which strips every `\r`, so every line this
 *  function handles (reused or freshly generated) is already EOL-free until the final join. */
export function serializeWorkflowPreserving(w: Workflow, originalText: string): string {
  const parsedOriginal = parseWorkflow(originalText);
  if (isUnreadable(parsedOriginal.findings)) return serializeWorkflow(w);
  const doc = splitDocument(originalText);
  if (!doc) return serializeWorkflow(w);

  const orig = parsedOriginal.workflow;
  const order = blockOrder(w);
  const out: string[] = [...doc.preamble];

  // ---- front: version, name, authored_with, unknown top-level keys ----
  const frontEntries = doc.entries.filter((e) => !TOP_SECTION_KEYS.has(e.key));
  const frontUnchanged =
    w.version === orig.version &&
    w.name === orig.name &&
    w.authored_with === orig.authored_with &&
    deepEqualValue(w.extra, orig.extra);
  // Reused, the front pieces stay exactly where the file put them (the walk below emits
  // each at its own position, interleaved with the sections). REGENERATED, they go to the
  // top as one canonical group — which is where `emitFrontLines` has always put them, and
  // the only position that reads right when the file didn't declare them at all.
  const reuseFront = frontUnchanged && frontEntries.length > 0;
  if (!reuseFront) out.push(...emitFrontLines(w));

  // ---- blocks, matched by id (the one thing about a block that never changes — see the
  // module comment at the top of this file) ----
  //
  // A reused `header`/`raw` segment already carries whatever blank line originally separated
  // it from what came before (the scan in `splitDocument`/`splitBlockItems` peels exactly that
  // trivia onto the FOLLOWING entry/item) — so a synthetic `""` is only ever pushed ahead of a
  // FRESHLY regenerated line, never ahead of reused text, or every section gains a blank line
  // it didn't have.
  //
  // NOTE (reorder): a block is matched by id, not by position, so its own comment travels WITH
  // it if the roster gets reordered by hand (in the raw YAML) — a deliberate property, not a
  // bug. What is NOT preserved across a reorder is the blank-line spacing BETWEEN items: each
  // item's leading trivia was captured relative to its ORIGINAL neighbor, so after a reorder it
  // separates a different pair than it used to. The result is still valid YAML and never loses
  // a comment; it can just look unevenly spaced. Fixing that needs re-deriving spacing from the
  // NEW neighbor at every reuse, which is more machinery than the cosmetic cost justifies here,
  // and the pane's own UI has no "reorder" gesture — this only arises from a hand edit.
  const pushBlocks = (blocksEntry: TopEntry | undefined): void => {
    if (!w.blocks.length) {
      // Same header/content split as the non-empty case below (and every other section): the
      // comment introducing the ROSTER ("# BLOCKS — the agents a run may use…") is about the
      // section, not about any one block, so it survives the roster being emptied out too —
      // only the LAST line of the header (the `blocks:`/`blocks: […]` key line itself) is
      // replaced with the canonical empty form.
      if (blocksEntry) out.push(...sectionHeaderLines(blocksEntry, "blocks: []"));
      else out.push("", "blocks: []");
      return;
    }
    const split = blocksEntry ? splitBlockItems(blocksEntry.content) : null;
    const reusable = !!split && split.items.length === orig.blocks.length;
    const targetIndent = split?.indent ?? 2;
    const origById = new Map<string, { block: WorkflowBlock; raw: string[] }>();
    if (reusable) {
      orig.blocks.forEach((b, i) => {
        if (b.id && !origById.has(b.id)) origById.set(b.id, { block: b, raw: split!.items[i]! });
      });
    }
    // The comment introducing the SECTION (not any one block) is reused whenever we have one to
    // reuse, independent of which items below it changed — but the `blocks:` line itself only
    // when it is a bare block header: a roster written `blocks: []` and then given its first
    // entry has to lose the `[]`, or the items land under a line that can't take them (#1090).
    if (blocksEntry) out.push(...sectionHeaderLines(blocksEntry, "blocks:"));
    else out.push("", "blocks:");
    let firstItem = true;
    // A segment is written out at most ONCE. `origById` keeps the first segment per id, so with
    // a duplicated id (`block-id-duplicate` is a validation finding, not an unreadable file) the
    // second block would otherwise "match" the first's segment and write its leading lines — a
    // section header, say — above itself too. A later block with an id already consumed takes
    // the plain regenerate path instead (#3410 review).
    const consumed = new Set<string>();
    for (const b of w.blocks) {
      const match = b.id && !consumed.has(b.id) ? origById.get(b.id) : undefined;
      if (match) consumed.add(b.id);
      if (match && deepEqualValue(b, match.block)) {
        out.push(...match.raw);
      } else if (match) {
        // An EDITED block keeps its own leading trivia (#3410). `splitBlockItems` hands a block
        // every comment/blank line directly above it, so a section header over a group of blocks
        // ("# -- reviewers: …") lands in the segment of whichever block comes first under it —
        // and regenerating that block from its fields alone deleted the header on the first
        // edit. Those lines precede the `- ` marker, so they are about the block's PLACE, never
        // about a field that changed underneath them: reusing them is not the re-attachment the
        // header comment above rules out. The trivia already carries the original separating
        // blank line (or its absence), so no synthetic `""` goes ahead of it.
        const firstSig = match.raw.findIndex(isSignificantLine);
        out.push(...match.raw.slice(0, firstSig), ...emitBlockLines(b, targetIndent));
      } else {
        if (!firstItem) out.push("");
        out.push(...emitBlockLines(b, targetIndent));
      }
      firstItem = false;
    }
  };

  /** Every section that is not `blocks:` — one shape, because they all want the same one.
   *
   *  The COMMENT INTRODUCING the section (e.g. "# ADVISORY — the declared happy path") is reused
   *  whenever there is one, independent of whether the content changed: regenerating the whole
   *  section including that comment meant deleting one edge dropped a comment that was never
   *  about that edge (#233 non-blocking #1). Only the CONTENT falls back to canonical, and only
   *  when it changed.
   *
   *  The `key:` line is NOT part of what gets reused unconditionally — it is a function of the
   *  content that follows it, so a regenerated body re-derives it through `sectionHeaderLines`
   *  (the reused one may be an empty map/sequence that block children can't legally follow, or
   *  a bare key that re-reads as undeclared — #1090). That helper is where the rule, and what
   *  happens to a trailing comment on the key line, is spelled out.
   *
   *  `present` is "the model still has something to write here": with an entry that no longer
   *  matches and nothing to write, the section is GONE, and falling through to the else-branch
   *  (which emits nothing for empty `lines`) is what deletes it — the introducing comment
   *  included, since it has no section left to introduce. */
  const pushSection = (
    entry: TopEntry | undefined,
    unchanged: boolean,
    present: boolean,
    lines: string[]
  ): void => {
    if (entry && unchanged) {
      out.push(...entry.header, ...entry.content);
    } else if (entry && present && lines.length) {
      out.push(...sectionHeaderLines(entry, lines[0]!), ...lines.slice(1));
    } else if (lines.length) {
      out.push("", ...lines);
    }
  };

  // A Record over `SectionKey`, not a switch: TypeScript demands an entry for every
  // member, so a section added to SECTION_ORDER with no emitter here fails the build
  // instead of silently vanishing from every file it is written in.
  const pushKey: Record<SectionKey, (entry: TopEntry | undefined) => void> = {
    blocks: (entry) => pushBlocks(entry),
    edges: (entry) =>
      pushSection(
        entry,
        deepEqualValue(w.edges, orig.edges),
        w.edges.length > 0,
        emitEdgesLines(w.edges, order)
      ),
    gates: (entry) =>
      pushSection(
        entry,
        deepEqualValue(w.gates, orig.gates),
        !!w.gates.merge || !!w.gates.extra,
        emitGatesLines(w, order)
      ),
    intake: (entry) =>
      pushSection(
        entry,
        deepEqualValue(w.intake, orig.intake),
        !!w.intake,
        w.intake ? emitIntakeLines(w.intake) : []
      ),
    merge_queue: (entry) =>
      pushSection(
        entry,
        deepEqualValue(w.merge_queue, orig.merge_queue),
        !!w.merge_queue,
        w.merge_queue ? emitMergeQueueLines(w.merge_queue) : []
      ),
    driver: (entry) => {
      // The toggle flips ONE field's value (#1869 review round 3), so the section's
      // own lines are reused with just the `enabled:` line rewritten — the same
      // guarantee the deepEqual path gives untouched sections, narrowed to the one
      // line that changed. This is NOT the re-attachment the module header rules
      // out: no field changed underneath a comment, only a value. Any other edit
      // falls through to `pushSection` and its canonical regeneration, exactly as
      // every other section's edits always have.
      const spliced =
        entry && w.driver && orig.driver && driverDiffersOnlyInEnabled(w.driver, orig.driver)
          ? spliceEnabledLine(entry.content, w.driver.enabled === true)
          : null;
      if (entry && spliced) {
        out.push(...entry.header, ...spliced);
        return;
      }
      pushSection(
        entry,
        deepEqualValue(w.driver, orig.driver),
        !!w.driver,
        w.driver ? emitDriverLines(w.driver) : []
      );
    },
    resources: (entry) =>
      pushSection(
        entry,
        deepEqualValue(w.resources, orig.resources),
        !!w.resources,
        w.resources ? emitResourcesLines(w.resources) : []
      ),
    board: (entry) =>
      pushSection(
        entry,
        deepEqualValue(w.board, orig.board),
        !!w.board,
        w.board ? emitBoardLines(w.board) : []
      ),
    triage: (entry) =>
      pushSection(
        entry,
        deepEqualValue(w.triage, orig.triage),
        !!w.triage,
        w.triage ? emitTriageLines(w.triage) : []
      ),
  };

  // The document's OWN order is the output's order (#880): walk the entries as the file
  // wrote them, and only sections the file never declared get appended, in canonical order.
  // A fixed emission order would have been fine while every known section happened to be
  // declared in that order — and stopped being fine the moment `merge_queue:` became a
  // section, because this repo's own workflow writes it ABOVE `blocks:`, so any edit would
  // have relocated it (and the six comment lines that introduce it) to the bottom of the file.
  const seen = new Set<SectionKey>();
  for (const e of doc.entries) {
    if (!TOP_SECTION_KEYS.has(e.key)) {
      if (reuseFront) out.push(...e.header, ...e.content);
      continue; // otherwise the canonical front group already went out, above
    }
    const key = e.key as SectionKey; // TOP_SECTION_KEYS is built from SECTION_ORDER
    if (seen.has(key)) continue; // a duplicate top-level key: the reader kept one, so emit one
    seen.add(key);
    pushKey[key](e);
  }
  for (const key of SECTION_ORDER) if (!seen.has(key)) pushKey[key](undefined);

  if (doc.trailer.length) out.push(...doc.trailer);

  const eol = originalText.includes("\r\n") ? "\r\n" : "\n";
  const text = out.join(eol);
  return text.endsWith(eol) ? text : text + eol;
}

// ---------- the driver form's text rules (#1869/#1876) ----------

/** The driver form's enable-toggle write rule (#1869; narrowed by review round 3).
 *
 *  ON writes `{ enabled: true }` — or, when a block already stands (a hand-written
 *  `enabled: false` beside declared counters, or a block with no `enabled:` line at
 *  all), flips just `enabled` and leaves the rest of the human's lines alone: the
 *  merge-queue lesson in reverse (#1020 review, finding 4 — a form must not silently
 *  rewrite what it did not write).
 *
 *  OFF is a **data-loss rule**, so it deletes only what costs nothing to delete:
 *  a block that carries NOTHING BUT `enabled` — no counter, no unknown key, and no
 *  comment in the file's own prose about it (`commentsInSection`) — is removed
 *  whole, because absent and `enabled: false` are the same state to the engine and
 *  deleting is the tidier of the two. Anything more and OFF writes
 *  `enabled: false`, preserving the block: the checkbox reads the `enabled:` line,
 *  and two clicks on it must never delete configuration the human can see. The
 *  comment signal comes from the view, which holds the original text — the model
 *  cannot see comments, and `driverSectionHasComments` is how it asks. */
export function setDriverEnabled(
  w: Workflow,
  on: boolean,
  commentsInSection = false
): void {
  if (!on) {
    const d = w.driver;
    if (!d) return;
    const carriesMore =
      d.max_review_rounds !== undefined ||
      d.max_ci_attempts !== undefined ||
      d.max_rebase_attempts !== undefined ||
      d.lane_timeout_minutes !== undefined ||
      d.fix_timeout_minutes !== undefined ||
      d.drive_timeout_minutes !== undefined ||
      d.plan_enabled !== undefined ||
      d.plan_review_minutes !== undefined ||
      d.planner_timeout_minutes !== undefined ||
      d.fix_nonblocking_rounds !== undefined ||
      d.auto_drive_on_done !== undefined ||
      d.extra !== undefined;
    if (carriesMore || commentsInSection) {
      d.enabled = false;
      return;
    }
    delete w.driver;
    return;
  }
  if (w.driver) w.driver.enabled = true;
  else w.driver = { enabled: true };
}

/** Does the `driver:` section of this text carry any comment line — the comment
 *  introducing it, a comment inside the block, or a trailing one on the key line?
 *  `setDriverEnabled`'s OFF rule asks this because the MODEL cannot see comments:
 *  a block that is bare to the model may still be the subject of the file's own
 *  prose, and deleting the section would delete that prose with it. Reuses the
 *  preserving serializer's own splitter (#233) rather than a second scanner — a
 *  `#` inside a block scalar's body is content, and only that splitter knows where
 *  the bodies are. Conservative on a shape the scan refuses to read: `true`, so an
 *  unreadable file is never the one that gets a block deleted. */
export function driverSectionHasComments(text: string): boolean {
  const doc = splitDocument(text);
  if (!doc) return true;
  const entry = doc.entries.find((e) => e.key === "driver");
  if (!entry) return false;
  // A line "carries" a comment when stripping one changes it — a pure `# …` line
  // (strip → empty) AND a significant line with a trailing `# …` (`driver: # off`)
  // both count; the block scalar scan excludes bodies, where a `#` is content.
  const carriesComment = (l: string): boolean => l.trim() !== "" && stripComment(l) !== l;
  const scan = (lines: readonly string[]): boolean => {
    const opaque = opaqueScalarIndices(lines as string[]);
    return lines.some((l, i) => !opaque.has(i) && carriesComment(l));
  };
  return scan(entry.header) || scan(entry.content);
}

/** The driver form's checkbox READ rule (#1869 review round 1) — the pair of
 *  `setDriverEnabled` above. The checkbox shows the driver's enabled state, and the
 *  engine's answer is what the `enabled:` LINE says (`RawDriver.enabled` is
 *  `#[serde(default)] bool`), never the block's presence: a present `driver:` block
 *  without the line is OFF to the engine, which is exactly what the pre-form pane
 *  rendered ("not declared - off (orrerix's default)"). Reading presence instead of
 *  the line would show ON for a driver that will never run — the one case the write
 *  rule's tests cannot see, which is why this half is pinned separately. */
export function isDriverOn(w: Workflow): boolean {
  return w.driver?.enabled === true;
}

/** The driver form's REMOVE affordance (#1876 P1) — the escape hatch the narrowed
 *  OFF toggle no longer provides. A `driver:` block makes the file unloadable by an
 *  orrerix build old enough not to know the key: `RawWorkflow` is
 *  `deny_unknown_fields`, so the unknown key fails the parse of the *whole* file
 *  rather than being ignored or warned about (verified against v1.3.0-beta1:
 *  `git show v1.3.0-beta1:crates/loomux-engine/src/workflow.rs` carries the
 *  attribute on `RawWorkflow` and ZERO `RawDriver` occurrences — beta2 already
 *  knows the key, so beta1 is the refusing build, not beta2; corrected in
 *  #1876 review 2 after the wrong tag shipped in five surfaces). The toggle
 *  deliberately preserves a configured block (#1869 review round 3), so removal
 *  is a separate, explicit gesture that discards the block whole — switch,
 *  counters, unknown keys and comments — behind its own confirmation in the view. */
export function removeDriverBlock(w: Workflow): void {
  delete w.driver; // the whole block, by the user's explicit instruction (#1876 P1)
}

/** The `driver:` block's `enabled:` line's own trailing comment, when the flip
 *  would PRESERVE it AND actually change the line — the condition for the driver
 *  form's flip note (#1876 P2, narrowed by review 2). TWO guards, and they are
 *  not the same question:
 *
 *  1. `enabledLineRewrite` — the suffix match itself, THE one place the
 *     true/false alphabet lives: a value not ending in `true`/`false` makes the
 *     splice bail into canonical regeneration, which drops the section's
 *     comments — this very comment included (the residual `spliceEnabledLine`
 *     documents). No note on a bail shape.
 *  2. NOT a no-op: if the line's value already reads as the value the next flip
 *     writes (`enabled: nottrue` under a checkbox that would write true — the
 *     suffix match replaces `true` with `true`), the write is byte-identical and
 *     there is nothing to say. The rewrite answer alone does NOT answer this:
 *     it is the splice's requirement, and the note needs
 *     strictly more (#1876 review 2 — the earlier doc claimed the two questions
 *     were the same one).
 *
 *  The direction of the next flip comes from the model (`isDriverOn`); the line
 *  comes from the text. Same scanner as `driverSectionHasComments` (the
 *  preserving splitter, #233) so a `#` inside a block scalar's body is content,
 *  never a comment — and a body line that looks like an `enabled:` field is not
 *  one, because it sits deeper than the block's fields. Returns null when there
 *  is no driver block, no `enabled:` line in it, no comment on that line, a
 *  value the splice cannot rewrite in place, a flip that would be a no-op, or a
 *  shape the scan refuses to read. */
export function driverEnabledLineComment(w: Workflow, text: string): string | null {
  const wanted = isDriverOn(w) ? "false" : "true"; // the value the next flip writes
  const doc = splitDocument(text); // P2 scan
  if (!doc) return null;
  const entry = doc.entries.find((e) => e.key === "driver");
  if (!entry) return null;
  const opaque = opaqueScalarIndices(entry.content);
  const firstField = entry.content.find((l, i) => !opaque.has(i) && isSignificantLine(l));
  if (firstField === undefined) return null;
  const indent = indentOf(firstField);
  for (let i = 0; i < entry.content.length; i++) {
    const line = entry.content[i]!;
    if (opaque.has(i) || !isSignificantLine(line) || indentOf(line) !== indent) continue;
    const split = splitKey(stripComment(line).trim());
    if (!split || split.key !== "enabled") continue;
    const rewrite = enabledLineRewrite(line);
    if (!rewrite) return null; // guard 1: the splice would bail here, so the comment dies — no note
    // Guard 2: a flip that changes nothing says nothing. The suffix the rewrite
    // would match, compared case-sensitively against the value it would write:
    // `nottrue` under an ON click matches `true` and replaces it with `true`.
    if (rewrite.matched === wanted) return null;
    const comment = line.slice(rewrite.head.length).trim();
    return comment || null;
  }
  return null;
}
