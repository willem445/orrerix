// The repo's OWN `.orrerix/workflow.yml` (#222), checked against the pane's reader
// and validator — the two things a human sees when they open it in loomux.
//
// loomux dogfoods its own feature, which is only worth anything if the file it ships
// is a file the app is happy with. So this reads the real one off disk (not a
// fixture: a fixture would drift the moment someone edits the workflow) and asserts
// it opens with ZERO findings — errors *and* warnings, because a warning here means
// the graph loomux would draw of its own workflow has a block nothing points at.
//
// The backend half of this pin lives in `src-tauri/tests/workflow.rs`
// (`the_repos_own_workflow_file_parses_clean_against_the_real_parser`). Both halves
// exist because the two parsers are deliberately separate: the pane's is an editor
// giving live feedback on text a human is typing, the backend's is the engine. A file
// that only one of them accepts is a file the human is being lied to about — which is
// precisely the drift this test catches, forever.
//
// What it checks of that file is VALIDITY, never VALUES (#3507, the human's rule: editing the
// workflow file must not turn main red). Every property below either holds of any valid
// workflow or is derived from the parsed file; a pin that needs a literal — an id, a header,
// an edge order — reads it off the synthetic `SPECIMEN` further down instead.
import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import {
  parseWorkflow,
  validateWorkflow,
  deriveGraph,
  serializeWorkflow,
  serializeWorkflowPreserving,
  formatWorkflowText,
} from "../src/workflowmodel.ts";
import { rewriteImpact, rewriteImpactMessage } from "../src/workflowpane.ts";

// The RAW bytes, whatever line ending this checkout actually has — a Windows checkout may have
// CRLF (`core.autocrlf`). `serializeWorkflowPreserving` keeps the original's own line ending
// (#233 non-blocking #3), so testing byte-for-byte against THIS is the honest claim regardless
// of what platform the suite runs on. `serializeWorkflow` (the fully canonical rewrite Format
// uses) always emits `\n` — no original text to take a convention from — so tests that compare
// against ITS output use `lfText` instead.
const text = readFileSync(new URL("../.orrerix/workflow.yml", import.meta.url), "utf8");
const lfText = text.replace(/\r\n/g, "\n");

test("the repo's own workflow opens in the pane with no findings", () => {
  const { workflow, findings: syntax } = parseWorkflow(text);
  const findings = [...syntax, ...validateWorkflow(workflow)];
  assert.deepEqual(
    findings.map((f) => `${f.severity} ${f.code}: ${f.message}`),
    [],
    "loomux's own workflow file must be clean in loomux's own pane"
  );
  assert.equal(workflow.version, 1);
});

// ---------- the synthetic specimen ----------
//
// Every pin below that needs a LITERAL — a block id, a section-header text, a fan-out
// authored out of roster order, a routing glob — reads it off this specimen, never off the
// real file (#3507: editing `.orrerix/workflow.yml` must not turn main red). The real file
// is held only to properties any valid workflow has, each derived from the file itself.
const SPECIMEN = [
  "# SPECIMEN PREAMBLE — synthetic, so the literal pins below never read the real file.",
  "# A second preamble line, so \"the preamble survives\" means all of it.",
  "version: 1",
  "name: serializer-specimen",
  "",
  "blocks:",
  "  - id: orchestrator",
  "    kind: orchestrator",
  "    cli: claude",
  "    model: opus",
  "",
  "  # -- workers: the header above the FIRST worker",
  "  - id: w-one",
  "    kind: worker",
  "    cli: claude",
  "    model: sonnet",
  "",
  "  - id: w-two",
  "    kind: worker",
  "    cli: claude",
  "    model: opus",
  "",
  "  # -- reviewers: the header above the FIRST reviewer",
  "  - id: r-one",
  "    kind: reviewer",
  "    cli: claude",
  "    model: sonnet",
  "",
  "  - id: r-two",
  "    kind: reviewer",
  "    cli: claude",
  "    model: opus",
  "",
  "# EDGES HEADER",
  "edges:",
  "  - { from: orchestrator, to: [w-one, w-two] }",
  // Authored OUT of roster order on purpose: the canonical form sorts it to [w-two, r-one].
  "  - { from: w-one, to: [r-one, w-two] }",
  "  - { from: w-two, to: r-one }",
  "  - { from: r-one, to: r-two }",
  "",
  "# GATES HEADER",
  "gates:",
  "  merge:",
  "    require: all-pass",
  "    reviewers: [r-one]",
  "    routing:",
  "      # a comment NESTED inside the routing list",
  "      - paths: [\"src/**\", \"**/Cargo.toml\"]",
  "        reviewers: [r-two]",
  "    also: [ci-green]",
  "",
  "# TRAILING comment block after the last section",
  "",
].join("\n");

type Workflow = ReturnType<typeof parseWorkflow>["workflow"];
type MergeGate = NonNullable<Workflow["gates"]["merge"]>;

const commentLines = (t: string): string[] => t.split(/\r?\n/).filter((l) => /^\s*#/.test(l));

/** For every block, the run of comment lines DIRECTLY above its `- id:` line (a section
 *  header, when one introduces it). Derived from whatever file it is given — no id is named. */
const headersAboveBlocks = (t: string): Map<string, string[]> => {
  const lines = t.split(/\r?\n/);
  const out = new Map<string, string[]>();
  lines.forEach((l, i) => {
    const m = /^\s*- id:\s*(\S+)/.exec(l);
    if (!m) return;
    const run: string[] = [];
    for (let j = i - 1; j >= 0 && /^\s*#/.test(lines[j]); j--) run.unshift(lines[j]);
    out.set(m[1], run);
  });
  return out;
};

/** A model value guaranteed to differ from the block's own, so the edit is never a no-op. */
const otherModel = (m: string | undefined): string => (m === "sonnet" ? "opus" : "sonnet");

/** The reviewer-kind blocks that neither `gates.merge.reviewers` nor any routing rule names.
 *  With no merge gate at all, no verdict is required of anyone, so there is nothing to be
 *  unnamed BY — the property is about a gate that exists. */
const namedBy = (g: MergeGate): Set<string> =>
  new Set([...g.reviewers, ...(g.routing ?? []).flatMap((r) => r.reviewers)]);
const unnamedReviewers = (w: Workflow): string[] => {
  const g = w.gates.merge;
  if (!g) return [];
  const named = namedBy(g);
  return w.blocks.filter((b) => b.kind === "reviewer" && !named.has(b.id)).map((b) => b.id);
};

/** The literal directory a glob is rooted at, or null when it has none (`**\/Cargo.toml`). */
const literalRoot = (glob: string): string | null => {
  const wild = glob.search(/[*?[]/);
  if (wild < 0) return glob; // no wildcard at all: the path itself must exist
  const upto = glob.slice(0, wild);
  const cut = upto.slice(0, upto.lastIndexOf("/") + 1).replace(/\/$/, "");
  return cut || null;
};
const repoHas = (rel: string): boolean => existsSync(new URL(`../${rel}`, import.meta.url));
/** Every routing path whose literal root does not exist, plus how many roots were checked. */
const routingRoots = (w: Workflow): { dead: string[]; checked: number } => {
  const dead: string[] = [];
  let checked = 0;
  for (const rule of w.gates.merge?.routing ?? []) {
    for (const p of rule.paths) {
      const root = literalRoot(p);
      if (root === null) continue;
      checked++;
      if (!repoHas(root)) dead.push(p);
    }
  }
  return { dead, checked };
};

test("the synthetic specimen is itself a clean workflow — so the pins built on it test the serializer, not a broken fixture", () => {
  const { workflow, findings } = parseWorkflow(SPECIMEN);
  assert.deepEqual([...findings, ...validateWorkflow(workflow)].map((f) => `${f.code}: ${f.message}`), []);
});

test("the roster is VALID — and nothing here pins what its values are", () => {
  // THE HUMAN'S RULE (#3507): editing this file must never turn main red, so this test
  // asks only whether the roster is one the engine can run — never WHICH cli, model or
  // effort a block chose, which blocks exist by id, or in what order. Those are the
  // operator's to change in a one-line edit.
  //
  // What is left, and who decides it:
  //  * the file parses with zero findings — the first test above, through the REAL
  //    parser and `validateWorkflow`. That already refuses an unknown cli or kind, an
  //    effort the CLI cannot take (`knob-unavailable`), and a gate or routing rule
  //    naming a block that does not exist or cannot record a verdict;
  //  * a persona file a block points at exists on disk — the one fact the pure validator
  //    cannot see, so it is checked here;
  //  * the roster can do the work at all: at least one worker and one reviewer.
  // Whether a CLI can HOST a kind is deliberately not the pane's question (`WORKFLOW_CLIS`'s
  // docblock: it belongs to the backend's `cli_can_host`), so the Rust twin,
  // `the_repos_own_workflow_file_parses_clean_against_the_real_parser`, asserts it.
  const { workflow } = parseWorkflow(text);
  assert.ok(workflow.blocks.some((b) => b.kind === "worker"), "a usable roster needs a worker block");
  assert.ok(workflow.blocks.some((b) => b.kind === "reviewer"), "a usable roster needs a reviewer block");
  for (const b of workflow.blocks) {
    if (b.profile === undefined) continue;
    assert.ok(repoHas(b.profile), `${b.id}: persona file ${b.profile} exists`);
  }
  // No floor on how many blocks carry a persona: a roster with none is valid (#3507).
  // The existence check's teeth are shown on a path that cannot exist instead.
  assert.equal(repoHas(".github/agents/zzz-no-such-persona.md"), false, "the persona check can say no");

  // POSITIVE CONTROL for the "zero findings" arm, which is what polices gate references:
  // a gate pointed at a missing block must be a finding. Built on the SPECIMEN, so it runs
  // whether or not the real file declares a gate at all.
  const spec = parseWorkflow(SPECIMEN).workflow;
  const gate = spec.gates.merge!;
  const broken = { ...spec, gates: { ...spec.gates, merge: { ...gate, reviewers: [...gate.reviewers, "zzz-no-such-block"] } } };
  assert.ok(
    validateWorkflow(broken).some((f) => f.code === "gate-unknown-reviewer"),
    "a gate naming a block that does not exist must be a finding"
  );
});

test("every declared reviewer lane is named by the gate or by a routing rule, because an abstention is a pass", () => {
  // THE SAFETY PROPERTY — and it is NAMEDNESS, not reachability, so say so rather
  // than overclaim. Under `all-pass` an abstention counts as a pass, so a
  // reviewer-kind block in the roster that neither `gates.merge.reviewers` nor any
  // `routing:` rule NAMES can never enter the required set at all: it would sit
  // there looking wired while the gate opened without it. "Named" is the UNION of the
  // static list and every routing rule (#1176): a lane required only on some paths is the
  // routing working (#1952), not a hole.
  //
  // What it does NOT catch, found in review (rev-final N4) rather than by the author: a
  // rule whose `paths:` match nothing still NAMES its reviewer, so the lane is named and
  // required on no PR. The `routing roots` check below is the partial close.
  const { workflow } = parseWorkflow(text);
  assert.deepEqual(unnamedReviewers(workflow), [], "every declared reviewer lane is named by the gate");

  // POSITIVE CONTROL, on the specimen: strike a lane from the static list AND every routing
  // rule and it must be reported. Asserted to have LANDED first, so the red is about the rule.
  const spec = parseWorkflow(SPECIMEN).workflow;
  const g = spec.gates.merge!;
  assert.deepEqual(unnamedReviewers(spec), [], "sanity: the specimen names every lane");
  const victim = "r-two";
  assert.ok(namedBy(g).has(victim), "sanity: the lane the control strikes was named to begin with");
  const struck = {
    ...g,
    reviewers: g.reviewers.filter((id) => id !== victim),
    routing: (g.routing ?? []).map((r) => ({ ...r, reviewers: r.reviewers.filter((id) => id !== victim) })),
  };
  assert.ok(!namedBy(struck).has(victim), "the mutation landed, on every list that named it");
  assert.deepEqual(
    unnamedReviewers({ ...spec, gates: { ...spec.gates, merge: struck } }),
    [victim],
    "a declared lane that no rule and no gate names must fail — that is the whole point"
  );
});

test("a routing path rooted at a literal directory is rooted at one that EXISTS", () => {
  // The partial close on the namedness gap above. Full reachability ("does this glob match a
  // file a PR could touch") needs a glob engine and the tracked-file list, which this suite
  // has neither of. What it can check is that a glob ROOTED at a literal path is rooted at one
  // that exists — exactly the shape a directory rename or a typo produces.
  //
  // No floor on the REAL file's routing: a file with no routing, or with only unrooted globs
  // (`**/*.md`, which can fire), is valid (#3507). The instrument's teeth are shown on the
  // specimen instead, where its population is known.
  const { workflow } = parseWorkflow(text);
  assert.deepEqual(routingRoots(workflow).dead, [], "a routing rule rooted at a missing directory can never fire");

  const spec = parseWorkflow(SPECIMEN).workflow;
  assert.deepEqual(routingRoots(spec), { dead: [], checked: 1 }, "the specimen's one rooted path (`src/**`) is checked and live");
  const g = spec.gates.merge!;
  const typo = { ...g, routing: (g.routing ?? []).map((r) => ({ ...r, paths: [...r.paths, "zzz-no-such-dir/**"] })) };
  assert.deepEqual(
    routingRoots({ ...spec, gates: { ...spec.gates, merge: typo } }).dead,
    ["zzz-no-such-dir/**"],
    "a rooted glob naming a directory that does not exist must be reported"
  );

  // THE RESIDUAL, PERFORMED rather than merely disclosed (CLAUDE.md's escape-hatch rule):
  // a glob whose literal root EXISTS but which matches no file passes. `src/**/*.zzz` roots at
  // `src`, which is there, so the check passes it while the rule can never fire.
  assert.equal(literalRoot("**/nope.zzz"), null, "an unrooted glob has no root to check…");
  assert.equal(literalRoot("src/**/*.zzz"), "src", "…but a rooted-yet-unmatchable glob IS checked, and passes — the real blind spot");
  assert.equal(repoHas("src"), true, "…because its root really does exist, which is all this check asks");
});

test("the merge gate is self-consistent — whatever it requires", () => {
  // No gate VALUE is pinned (#3507): `require`, `also` and the queue are the operator's.
  // What any valid gate owes is checked instead. `also:` conditions are the backend's to
  // judge (`condition_supported`, asserted in the Rust twin), since the pane passes them
  // through; the pane's half is that an all-pass gate carries no threshold.
  const gate = parseWorkflow(text).workflow.gates.merge;
  if (gate?.require === "all-pass") assert.equal(gate.threshold, undefined, "an all-pass gate takes no threshold");
});

test("role_hint: advisor still pairs only with the planner kind (synthetic — the rule outlives the roster)", () => {
  // The advisor block left the dogfood roster (never spawned across two live
  // batches), but the planner-side half of the role_hint-pairs-with-kind rule
  // (#250/#324) keeps end-to-end coverage via this synthetic fixture.
  const fixture = [
    "version: 1",
    "blocks:",
    "  - id: helper",
    "    kind: planner",
    "    role_hint: advisor",
  ].join("\n");
  // The minimal fixture legitimately warns about graph shape (no edges); the
  // property under test is the PAIRING, so filter to hint-related findings.
  const hintFindings = (w: string) => {
    const parsed = parseWorkflow(w);
    return [...parsed.findings, ...validateWorkflow(parsed.workflow)].filter((f) =>
      /hint/i.test(`${f.code} ${f.message}`)
    );
  };
  assert.deepEqual(hintFindings(fixture), [], "planner + advisor is the legal pairing");
  const ok = parseWorkflow(fixture);
  assert.deepEqual(
    [ok.workflow.blocks[0].kind, ok.workflow.blocks[0].role_hint],
    ["planner", "advisor"]
  );
  assert.ok(
    hintFindings(fixture.replace("kind: planner", "kind: worker")).length > 0,
    "advisor on a worker kind must be a finding, never coerced"
  );
});

test("every block is on the declared path — the graph loomux draws has no orphans", () => {
  const { workflow } = parseWorkflow(text);
  const graph = deriveGraph(workflow);
  // `isolated`/`unreachable` are warnings in the validator, and the file is already
  // asserted findings-free above; this says the same thing about the derived graph,
  // which is what the pane actually renders. An orphan block is a delegate the flow
  // forgot — the fan-out someone meant to wire and didn't.
  assert.equal(graph.nodes.length, workflow.blocks.length);
  assert.ok(graph.edges.length > 0, "the declared happy path must actually be declared");
});

// ---------- and now the pane can WRITE it (#222 v2) ----------

/** Each source's fan-out, in the order the file lists it. */
const fanouts = (w: Workflow): [string, string[]][] => {
  const byFrom = new Map<string, string[]>();
  for (const e of w.edges) byFrom.set(e.from, [...(byFrom.get(e.from) ?? []), e.to]);
  return [...byFrom.entries()];
};

/** The canonical-save contract, asserted of any workflow text. */
const assertCanonicalSavePreservesMeaning = (source: string): { before: Workflow; after: Workflow } => {
  const { workflow } = parseWorkflow(source);
  const saved = serializeWorkflow(workflow);
  const reread = parseWorkflow(saved);
  assert.deepEqual(reread.findings, [], "a saved copy must still be clean");
  // Everything that is not the edge list comes back deepEqual — blocks, personas, the gate and
  // its routing, and every policy section.
  assert.deepEqual({ ...reread.workflow, edges: [] }, { ...workflow, edges: [] }, "…and must mean exactly what the original meant");
  // The graph itself: the same edges, none invented, none lost.
  const key = (e: { from: string; to: string }): string => `${e.from}->${e.to}`;
  assert.deepEqual([...reread.workflow.edges.map(key)].sort(), [...workflow.edges.map(key)].sort());
  assert.equal(reread.workflow.edges.length, workflow.edges.length, "no edge invented, none lost");
  assert.equal(serializeWorkflow(reread.workflow), saved, "…and saving it twice must be a no-op");
  // The normalization, pinned in the direction it moves: every fan-out in the SAVED file is in
  // roster order.
  const rosterIndex = new Map(workflow.blocks.map((b, i) => [b.id, i]));
  for (const [from, to] of fanouts(reread.workflow)) {
    assert.deepEqual(
      to,
      [...to].sort((a, b) => (rosterIndex.get(a) ?? 0) - (rosterIndex.get(b) ?? 0)),
      `${from}: the canonical file lists a fan-out in roster order`
    );
  }
  return { before: workflow, after: reread.workflow };
};

test("a canonical save preserves the workflow's MEANING, exactly", () => {
  // What serialization actually guarantees, and all it guarantees: the workflow that comes back
  // is the workflow that went in — every block, persona, edge and gate — and the canonical form
  // is stable, so saving twice is a no-op.
  //
  // "MEANING" is the word that has to be precise here, and the canonical form's own contract
  // supplies it: every list that REFERENCES a block (an edge's fan-out, `gates.merge.reviewers`)
  // is normalized into ROSTER order on the way out — `sortByBlocks`, argued at length in
  // `connectToGate`'s docblock ("SEAT ORDER IS NOT THE HUMAN'S"). So the graph is compared as a
  // SET, and the ORDERING is asserted separately, as the direction it is supposed to move in.
  assertCanonicalSavePreservesMeaning(lfText);

  // THE CONTROL that keeps the set comparison from being a tautology: some input must actually
  // EXERCISE the reorder, or "same set, different order" is a property nothing witnesses. On the
  // SPECIMEN, whose `w-one` fan-out is authored out of roster order (#3507: the real file's edge
  // order is the operator's, so it is not asked to carry this divergence).
  const { before, after } = assertCanonicalSavePreservesMeaning(SPECIMEN);
  assert.notDeepEqual(fanouts(before), fanouts(after), "the specimen's authored order and roster order must DIFFER");
});

test("the EXPLICIT Format action rewrites a commented file wholesale — and warns first", () => {
  // `serializeWorkflow` (what the Format button uses) is still a full, comment-dropping
  // rewrite on purpose — see its own docblock. So asking for the fully canonical form of a
  // commented file costs something, and the pane says so before it happens (`rewriteImpact`,
  // used from the Format action since #233 — see `workflowview.ts`'s `confirmFormatRewrite`).
  const isCanonical = (t: string): boolean => formatWorkflowText(t) === t;

  // The literal case, on the specimen: its comments are known, so the count is exact.
  const specCanonical = serializeWorkflow(parseWorkflow(SPECIMEN).workflow);
  assert.notEqual(specCanonical, SPECIMEN, "a commented file is not in canonical form");
  const impact = rewriteImpact(SPECIMEN, specCanonical, isCanonical);
  assert.ok(impact, "an explicit Format over a commented file must raise a warning");
  assert.ok(impact.reformats, "…it is a whole-file rewrite");
  assert.equal(impact.droppedComments, commentLines(SPECIMEN).length, "…and it drops every comment line");
  assert.ok(impact.droppedComments > 0, "sanity: the specimen has comments to drop");
  assert.match(rewriteImpactMessage(impact, ".orrerix/workflow.yml"), /comments on \d+ lines/);
  // And the case that must stay SILENT: a file loomux itself wrote is already canonical, so
  // formatting it costs nothing and asks nothing.
  assert.equal(rewriteImpact(specCanonical, specCanonical, isCanonical), null);

  // The real file, whatever it holds: the warning fires exactly when Format would reformat it,
  // and when it does it counts every comment line the canonical form loses.
  const canonical = serializeWorkflow(parseWorkflow(lfText).workflow);
  const real = rewriteImpact(lfText, canonical, isCanonical);
  if (isCanonical(lfText) || canonical === lfText) {
    assert.equal(real, null, "an already-canonical file formats silently");
  } else {
    assert.ok(real, "a non-canonical file must warn before Format rewrites it");
    assert.equal(real.droppedComments, Math.max(0, commentLines(lfText).length - commentLines(canonical).length));
  }
});

// ---------- and now an ordinary form/canvas edit does NOT eat the comments (#233) ----------
//
// This is the pin the rest of #233's tests build on: an actual save through the pane calls
// `serializeWorkflowPreserving(model, previousBufferText)`, not `serializeWorkflow`. The test
// above and the ones below together are the whole story — Format still asks, because it is
// still a deliberate full rewrite; an ordinary edit through the form or canvas no longer needs to.

test("re-serializing this file with NOTHING changed reproduces it exactly", () => {
  const { workflow } = parseWorkflow(text);
  assert.equal(serializeWorkflowPreserving(workflow, text), text);
});

test("editing ANY one block's model keeps every comment line — and each header above its block", () => {
  // Derived over every block the real file declares, so no id is named (#3507), and it covers
  // both specimens the old literal pins held: a block BESIDE a section header, and the block
  // directly UNDER one (#3410, where the header sits inside the edited block's own segment and
  // the save used to drop it).
  for (const source of [text, SPECIMEN]) {
    const { workflow } = parseWorkflow(source);
    const headers = headersAboveBlocks(source);
    assert.equal(headers.size, workflow.blocks.length, "sanity: every block's `- id:` line was found");
    for (const target of workflow.blocks) {
      const edited = {
        ...workflow,
        blocks: workflow.blocks.map((b) => (b.id === target.id ? { ...b, model: otherModel(b.model) } : b)),
      };
      const out = serializeWorkflowPreserving(edited, source);
      assert.notEqual(out, source, `${target.id}: sanity — the edit changed the text`);
      assert.deepEqual(parseWorkflow(out).workflow, edited, `${target.id}: the edit itself round-trips`);
      assert.deepEqual(commentLines(out), commentLines(source), `${target.id}: a one-field edit costs no comment line, and moves none`);
      assert.deepEqual(headersAboveBlocks(out), headers, `${target.id}: every header still sits directly above the block it introduces`);
      // The rewrite-impact guard (Format's guard, not save's) does not fire for this: it is not
      // a whole-file canonical rewrite, just one changed field.
      assert.equal(rewriteImpact(source, out, (t) => formatWorkflowText(t) === t), null, `${target.id}: not a reformat`);
    }
  }

  // The specimen's literal headers, so "each header above its block" is witnessed on a file that
  // HAS headers above a first block — the real file is not required to keep any.
  const spec = headersAboveBlocks(SPECIMEN);
  assert.deepEqual(spec.get("w-one"), ["  # -- workers: the header above the FIRST worker"]);
  assert.deepEqual(spec.get("r-one"), ["  # -- reviewers: the header above the FIRST reviewer"]);
});
