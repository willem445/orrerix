// Unit tests for the workflow model's WRITE half (#222): the comment-preserving
// serializer (#233) and the empty <-> block section rewrites (#1090). Split out of
// test/workflowmodel.test.ts alongside src/workflowmodel.ts (#3498 F2).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  parseWorkflow,
  serializeWorkflow,
  serializeWorkflowPreserving,
  removeBlockAt,
  starterWorkflow,
  connectBlocks,
  addBlock,
  newBlock,
  type Workflow,
  type WorkflowBlock,
} from "../src/workflowmodel.ts";
import { codes } from "./support/workflowfixtures.ts";

// ---------- comment-preserving serialization (#233) ----------
//
// `serializeWorkflow` is the FULL rewrite — it never carried comments, and it still doesn't;
// that is what `formatWorkflowText` and the Format button ask for on purpose. What follows is
// `serializeWorkflowPreserving`: same model, but handed the ORIGINAL text too, so it can reuse
// whatever it didn't change instead of reformatting the whole file every time.

const COMMENTED = `# who runs, and why
version: 1
name: focused-review

blocks:
  # the planner goes first
  - id: planner
    name: Planner
    kind: planner
    cli: claude
    model: opus

  - id: worker          # opens the PR
    name: Worker
    kind: worker
    cli: claude

# ADVISORY — the declared happy path
edges:
  - { from: planner, to: worker }

# ENFORCED — nothing merges without this
gates:
  merge:
    require: all-pass
    reviewers: [planner]
`;

test("an untouched file re-serializes to itself, byte for byte", () => {
  const { workflow } = parseWorkflow(COMMENTED);
  assert.equal(serializeWorkflowPreserving(workflow, COMMENTED), COMMENTED);
});

test("editing one block's field keeps every OTHER block's comments, and the section headers", () => {
  const { workflow } = parseWorkflow(COMMENTED);
  const edited: Workflow = {
    ...workflow,
    blocks: workflow.blocks.map((b) => (b.id === "worker" ? { ...b, model: "opus" } : b)),
  };
  const out = serializeWorkflowPreserving(edited, COMMENTED);
  assert.match(out, /# who runs, and why/, "the file preamble survives");
  assert.match(out, /# the planner goes first/, "an untouched block's own comment survives");
  assert.match(out, /# ADVISORY — the declared happy path/, "the edges section header survives");
  assert.match(out, /# ENFORCED — nothing merges without this/, "the gates section header survives");
  // The edited block's own trailing comment is the one thing that is allowed to go — it is
  // the node that changed, and #233's bar is "edited nodes serialize cleanly", not lossless.
  assert.doesNotMatch(out, /# opens the PR/);
  assert.deepEqual(parseWorkflow(out).workflow, edited, "and the edit itself must round-trip");
});

test("editing a block keeps the comment lines directly ABOVE it — a section header included (#3410)", () => {
  // The comment directly above a block is read as that block's own leading trivia, so it sits
  // in the edited block's segment rather than in any untouched region. It is still not ABOUT
  // the field that changed — here it is a header over two blocks — and a save that regenerates
  // the block must write it back. Every block below is already in the canonical emitter's own
  // spelling, so the only line an edit may change is the one field it edits: the expectation is
  // the original text with that one line replaced, which a dropped comment cannot satisfy.
  const text = `version: 1
name: headers

blocks:
  # -- workers: first tier, then the fallback ----
  - id: worker-std
    name: Worker
    kind: worker
    cli: claude
    model: sonnet

  - id: worker-adv
    name: Advanced
    kind: worker
    cli: claude
    model: opus

  # -- reviewers ---------------------------------
  # (two lines of header, and a blank between them and the block)

  - id: rev-std
    name: Reviewer
    kind: reviewer
    cli: claude
    model: sonnet
`;
  const { workflow } = parseWorkflow(text);
  const edited: Workflow = {
    ...workflow,
    blocks: workflow.blocks.map((b) =>
      b.id === "worker-std" ? { ...b, model: "haiku" } : b.id === "rev-std" ? { ...b, model: "opus" } : b
    ),
  };
  const out = serializeWorkflowPreserving(edited, text);
  const expected = text
    .replace("    model: sonnet\n\n  - id: worker-adv", "    model: haiku\n\n  - id: worker-adv")
    .replace(/model: sonnet\n$/, "model: opus\n");
  assert.notEqual(expected, text, "sanity: both expectation replacements landed");
  assert.equal(out, expected);
  assert.deepEqual(parseWorkflow(out).workflow, edited, "and the edit itself must round-trip");
});

test("a duplicated block id never copies the first block's leading lines above the second (#3410 review)", () => {
  // `origById` keeps the FIRST segment per id, so the second `- id: a` "matches" the first's
  // segment. `block-id-duplicate` is a validation finding, not an unreadable file, so the
  // preserving serializer still runs here. A segment's leading lines are written at most once.
  const text = `version: 1
blocks:
  # -- header over A ----
  - id: a
    name: First
    kind: worker
    cli: claude

  # comment for the second a
  - id: a
    name: Second
    kind: worker
    cli: claude
`;
  const { workflow } = parseWorkflow(text);
  assert.equal(workflow.blocks.length, 2, "sanity: both duplicates are read");
  for (const [label, edit] of [
    ["an edit to the second", (b: WorkflowBlock, i: number) => (i === 1 ? { ...b, name: "Renamed" } : b)],
    ["an unrelated save (no edit)", (b: WorkflowBlock) => b],
  ] as const) {
    const edited: Workflow = { ...workflow, blocks: workflow.blocks.map(edit) };
    const out = serializeWorkflowPreserving(edited, text);
    assert.equal(out.split("# -- header over A ----").length - 1, 1, `${label}: the header is written once`);
    assert.match(out, /# -- header over A ----\n  - id: a\n    name: First\n/, `${label}: above the first block`);
    assert.deepEqual(parseWorkflow(out).workflow.blocks.map((b) => b.name), edited.blocks.map((b) => b.name));
  }
});

test("editing the first block of the roster keeps the comment between `blocks:` and it (#3410)", () => {
  // The first item has no blank line above it, so no synthetic one may be added either.
  const { workflow } = parseWorkflow(COMMENTED);
  const edited: Workflow = {
    ...workflow,
    blocks: workflow.blocks.map((b) => (b.id === "planner" ? { ...b, model: "sonnet" } : b)),
  };
  const out = serializeWorkflowPreserving(edited, COMMENTED);
  assert.match(out, /\nblocks:\n  # the planner goes first\n  - id: planner\n/);
  assert.match(out, /# opens the PR/, "the untouched sibling keeps its trailing comment");
  assert.deepEqual(parseWorkflow(out).workflow, edited);
});

test("a prompt whose own last line looks like a comment survives editing a SIBLING (#233 B2)", () => {
  // `isSignificantLine` treats a `#`-starting line as trivia to peel — correct for an ACTUAL
  // comment, wrong for a `|` block scalar's body, where `#` is just a character the prompt
  // happens to contain. Peeling it as if it were commentary on the NEXT block silently moves it
  // there; if that next block is the one that gets edited (regenerated canonically), the line
  // never comes back — the reviewer's exact repro.
  const text = `version: 1
blocks:
  - id: a
    name: A
    kind: worker
    cli: claude
    prompt: |
      Do the work.
      # trailing checklist marker, not a comment
  - id: b
    name: B
    kind: worker
    cli: claude
`;
  const { workflow } = parseWorkflow(text);
  const promptBefore = workflow.blocks[0]!.prompt;
  assert.match(promptBefore ?? "", /# trailing checklist marker/, "sanity: the real reader keeps it as content");

  const edited = { ...workflow, blocks: workflow.blocks.map((b) => (b.id === "b" ? { ...b, model: "opus" } : b)) };
  const out = serializeWorkflowPreserving(edited, text);
  const reread = parseWorkflow(out).workflow;
  assert.equal(reread.blocks[0]!.prompt, promptBefore, "block a's prompt — untouched — must survive intact");
  assert.deepEqual(reread, edited);
});

test("adding a block regenerates only the new entry — every existing one is untouched text", () => {
  const { workflow } = parseWorkflow(COMMENTED);
  const added = addBlock(workflow, newBlock("rev", "Reviewer", "reviewer"));
  const out = serializeWorkflowPreserving(added, COMMENTED);
  assert.match(out, /# the planner goes first/);
  assert.match(out, /# opens the PR/);
  // Round-tripped through the ordinary parser convention on BOTH sides (a fresh `newBlock()`
  // has no `extra` key at all; a parsed one always carries `extra: undefined` explicitly —
  // an unrelated quirk of `readBlock`, not something this test is about).
  assert.deepEqual(parseWorkflow(out).workflow, parseWorkflow(serializeWorkflow(added)).workflow);
});

test("removing a block drops only its own segment — the rest, including comments, is untouched", () => {
  const { workflow } = parseWorkflow(COMMENTED);
  const removed = removeBlockAt(workflow, workflow.blocks.findIndex((b) => b.id === "worker"));
  const out = serializeWorkflowPreserving(removed, COMMENTED);
  assert.match(out, /# the planner goes first/, "the untouched block's comment survives");
  assert.doesNotMatch(out, /id: worker\b/, "the removed block itself is gone");
  assert.deepEqual(parseWorkflow(out).workflow, removed);
  // Its edges and gate seat go with it (removeBlockAt's own contract) — and since the edges/
  // gates sections themselves changed, THEIR comments are the honest cost of that edit.
  assert.doesNotMatch(out, /# ADVISORY/);
});

test("an edge added or removed regenerates the edges CONTENT, but keeps that section's own header comment", () => {
  // The section header ("# ADVISORY …") introduces the CONCEPT of the edges section, not any
  // one edge in it — dropping it every time a single edge is rewired cost far more than the
  // edit itself touched (#233 non-blocking #1). Only the fan-out entries fall back to canonical.
  const { workflow } = parseWorkflow(COMMENTED);
  const rewired = connectBlocks(workflow, "worker", "planner");
  const out = serializeWorkflowPreserving(rewired, COMMENTED);
  assert.match(out, /# who runs, and why/);
  assert.match(out, /# the planner goes first/);
  assert.match(out, /# opens the PR/);
  assert.match(out, /# ADVISORY — the declared happy path/, "the edges section HEADER survives its own content changing");
  assert.match(out, /# ENFORCED — nothing merges without this/, "gates is untouched and keeps its header");
  assert.deepEqual(parseWorkflow(out).workflow, rewired);
});

test("emptying the edge list entirely omits the section rather than leaving a bare header", () => {
  const { workflow } = parseWorkflow(COMMENTED);
  const cleared = { ...workflow, edges: [] };
  const out = serializeWorkflowPreserving(cleared, COMMENTED);
  assert.doesNotMatch(out, /^edges:/m, "no edges left — nothing to hang the header on");
  assert.deepEqual(parseWorkflow(out).workflow, cleared);
});

test("a name change loses only the front section's own trivia (none here), not the rest", () => {
  const { workflow } = parseWorkflow(COMMENTED);
  const renamed = { ...workflow, name: "renamed" };
  const out = serializeWorkflowPreserving(renamed, COMMENTED);
  assert.match(out, /# who runs, and why/, "the file preamble is document-level, kept regardless");
  assert.match(out, /# the planner goes first/);
  assert.deepEqual(parseWorkflow(out).workflow, renamed);
});

test("preserving-serializing is idempotent over its own output", () => {
  const { workflow } = parseWorkflow(COMMENTED);
  const edited: Workflow = {
    ...workflow,
    blocks: workflow.blocks.map((b) => (b.id === "worker" ? { ...b, model: "opus" } : b)),
  };
  const once = serializeWorkflowPreserving(edited, COMMENTED);
  const twice = serializeWorkflowPreserving(edited, once);
  assert.equal(twice, once);
});

test("a file from a NEWER loomux (version: 2) is still editable — its comments are not silently eaten (#233 B3)", () => {
  // `version-unsupported` is an ERROR finding, but the file is still READABLE — the view keeps
  // the form enabled through it (`syntaxBroken` only cares about `yaml-syntax`/`not-a-mapping`).
  // Before this fix, `serializeWorkflowPreserving` gated its fallback on `hasErrors` (any error
  // finding at all), so a version-2 file — the one case the codebase explicitly designs for
  // surviving an older pane (`extra` pass-through) — silently full-canonicalized on the very
  // first edit, for a reason the human was never shown.
  const text = `# a note the file's comments carry
version: 2
blocks:
  - id: a
    name: A
    kind: worker
    cli: claude
`;
  const { workflow, findings } = parseWorkflow(text);
  assert.ok(findings.some((f) => f.code === "version-unsupported"), "sanity: this finding fires");

  const edited = { ...workflow, blocks: [{ ...workflow.blocks[0]!, model: "opus" }] };
  const out = serializeWorkflowPreserving(edited, text);
  assert.match(out, /# a note the file's comments carry/, "the comment must not be silently eaten");
  assert.deepEqual(parseWorkflow(out).workflow, edited);
});

test("original text that doesn't parse falls back to the ordinary canonical rewrite, never a guess", () => {
  const w = starterWorkflow();
  const broken = "version: 1\nblocks:\n\t- id: w\n"; // a tab in the indentation — a syntax finding
  assert.equal(serializeWorkflowPreserving(w, broken), serializeWorkflow(w));
});

test("an empty original text still produces a valid file that round-trips", () => {
  // Empty text has no syntax error (`isUnreadable` is about READABILITY, not about every
  // finding — #233 B3), so this goes through the ordinary preserving path rather than a
  // hard-coded "brand new file" shortcut; there is simply nothing to reuse, so every piece
  // regenerates canonically. What matters is that it's still a correct, round-trip-safe file.
  const w = starterWorkflow();
  const out = serializeWorkflowPreserving(w, "");
  assert.deepEqual(parseWorkflow(out).workflow, parseWorkflow(serializeWorkflow(w)).workflow);
});

test("a block sequence indented to something other than loomux's own 2 spaces is preserved AT that indent", () => {
  // #233 non-blocking #2: a regenerated (edited/added) item is emitted at the FILE's own marker
  // indent, not a hardcoded one — so it never has to choose between corrupting the sequence
  // (mixing two indents) and reformatting the whole roster just because one field changed.
  const text = `version: 1
blocks:
    - id: w
      name: W
      kind: worker
      cli: claude

    - id: w2
      name: W2
      kind: worker
      cli: claude
`;
  const { workflow } = parseWorkflow(text);
  const edited = {
    ...workflow,
    blocks: workflow.blocks.map((b) => (b.id === "w" ? { ...b, model: "opus" } : b)),
  };
  const out = serializeWorkflowPreserving(edited, text);
  assert.deepEqual(parseWorkflow(out).workflow, edited);
  // The untouched sibling (w2) is reused verbatim at its original indent…
  assert.match(out, /\n {4}- id: w2\n {6}name: W2\n/);
  // …and the regenerated one matches that SAME indent, not a hardcoded 2.
  assert.match(out, /\n {4}- id: w\n {6}name: W\n {6}kind: worker\n {6}cli: claude\n {6}model: opus\n/);
});

test("a block sequence at column 0 (same indent as `blocks:` itself) is understood, not misread as new keys", () => {
  // #233 B1: `blocks:` with nothing after it may be followed by its own sequence at the SAME
  // column — legal YAML the reader (`afterKey`, above) already accepts. A structural scan that
  // treated each `- id: …` as a bogus new top-level key spliced roster content into `front` and
  // silently discarded everything after the first misread line on re-parse.
  const text = `version: 1
blocks:
- id: a
  name: A
  kind: worker
  cli: claude
- id: b
  name: B
  kind: worker
  cli: claude
`;
  const { workflow } = parseWorkflow(text);
  assert.equal(workflow.blocks.length, 2, "sanity: the real reader sees both blocks");

  // A total no-op must reproduce the file exactly — the strongest form of "not destructive".
  assert.equal(serializeWorkflowPreserving(workflow, text), text);

  // And an edit to one of them must not lose the other, or silently drop the roster.
  const edited = {
    ...workflow,
    blocks: workflow.blocks.map((b) => (b.id === "b" ? { ...b, model: "opus" } : b)),
  };
  const out = serializeWorkflowPreserving(edited, text);
  assert.deepEqual(parseWorkflow(out).workflow, edited);
});

test("an ORPHAN column-0 dash sequence (no owning key at all) safely falls back — nothing is lost", () => {
  // Round 2: the same-column fix above only recognizes a `- …` line as sequence CONTENT when it
  // directly follows an empty-rest key (`blocks:` with nothing after the colon). A `- id: a`
  // line with NO such key before it at all — nobody wrote `blocks:` — is an ORPHAN: `splitKey`
  // still returns a "key" for it (`- id`, since the text contains a `: `), and reading THAT as a
  // fresh top-level key is the same B1 mistake with no governing key to blame it on. The real
  // reader's `mapping()` stops here too (filed separately as its own issue: it does so SILENTLY,
  // with no finding) — so `orig.workflow.blocks` is already empty by the time this scan sees it.
  const text = `version: 1
- id: a
  name: A
  kind: worker
  cli: claude
`;
  const { workflow: orig } = parseWorkflow(text);
  assert.equal(orig.blocks.length, 0, "sanity: the real reader never reads this as a roster at all");

  // A block added through the form (`orig` had none) must survive being written back and
  // reloaded — not get silently swallowed by a scan that trusted the orphan dash as a key.
  const withBlock = addBlock(orig, newBlock("w", "W"));
  const out = serializeWorkflowPreserving(withBlock, text);
  const reloaded = parseWorkflow(out).workflow;
  assert.deepEqual(reloaded.blocks.map((b) => b.id), ["w"], "the added block must survive a reload");
});

test("no double blank line when a regenerated item follows one whose scalar ran to the segment's end", () => {
  // Round 2: a `prompt: |` that is the LAST field of an item, followed by exactly one blank
  // line before the next item, is ambiguous — the blank line could be trailing content of the
  // scalar (which the real reader's own chomping would discard) or the ordinary separator
  // before the next item. `opaqueScalarIndices` used to leave it "stuck" inside the (never
  // properly closed) scalar for the rest of the segment, so it stayed as unpeelable content of
  // item `a` — and when item `b` was then regenerated, the synthetic separator this function
  // always inserts before a regenerated item stacked a SECOND blank line on top of it.
  const text = `version: 1
blocks:
  - id: a
    name: A
    kind: worker
    cli: claude
    prompt: |
      line one

  - id: b
    name: B
    kind: worker
    cli: claude
`;
  const { workflow } = parseWorkflow(text);
  const edited = {
    ...workflow,
    blocks: workflow.blocks.map((b) => (b.id === "b" ? { ...b, model: "opus" } : b)),
  };
  const out = serializeWorkflowPreserving(edited, text);
  assert.doesNotMatch(out, /\n\n\n/, "at most one blank line between the two items");
  assert.deepEqual(parseWorkflow(out).workflow, edited);
});

test("emptying the roster keeps the section's own header comment, not just a bare `blocks: []`", () => {
  const text = `version: 1
# BLOCKS — the agents a run may use, closed-set kind:
blocks:
  - id: a
    name: A
    kind: worker
    cli: claude
`;
  const { workflow } = parseWorkflow(text);
  const emptied = { ...workflow, blocks: [] };
  const out = serializeWorkflowPreserving(emptied, text);
  assert.match(out, /# BLOCKS — the agents a run may use, closed-set kind:/);
  assert.match(out, /^blocks: \[\]$/m);
  assert.deepEqual(parseWorkflow(out).workflow, emptied);
});

test("CRLF line endings are preserved end to end, on every platform (#233 non-blocking #3)", () => {
  // A 5-line fixture with EXPLICIT `\r\n`, so this is pinned independent of what line ending
  // the test runner's own checkout happens to have (the dogfood test exercises the real file's
  // actual bytes, which on a Linux CI runner may be LF even though this repo targets Windows).
  const text = "version: 1\r\nblocks:\r\n  - id: w\r\n    name: W\r\n    kind: worker\r\n";
  const { workflow } = parseWorkflow(text);

  assert.equal(serializeWorkflowPreserving(workflow, text), text, "a no-op must reproduce it byte for byte");

  const edited = { ...workflow, blocks: [{ ...workflow.blocks[0]!, cli: "claude" }] };
  const out = serializeWorkflowPreserving(edited, text);
  assert.ok(out.includes("\r\n"), "CRLF survives an edit too");
  assert.ok(!/[^\r]\n/.test(out), "no bare LF snuck in anywhere");
  assert.deepEqual(parseWorkflow(out).workflow, edited);
});

// ---------- empty ↔ block: a section that gains or loses its last child (#1090) ----------
//
// The bug, from the #1018 demo: a file with an empty `resources: {}` given its first resource
// through the form came back as
//
//     resources: {}
//       catfish: {}
//
// — the preserving serializer reused the original key line, which commits the section to the
// inline empty-mapping form, and then wrote block children under it. Not YAML, so the pane
// declared its own output unreadable and disabled the form. Every section reused through
// `pushSection` had it, plus `blocks:`; the inverse direction (last child removed, key line
// left as a bare `resources:`) is the silent half — a bare key is YAML null, i.e. undeclared.

/** A file with one worker and nothing else, ready to have a section spliced onto it. */
const oneWorker = (section: string): string =>
  `version: 1
name: t

blocks:
  - id: w
    name: W
    kind: worker
    cli: claude

${section}
`;

/** Serialize `edited` against `original`, then assert the result both PARSES cleanly and reads
 *  back as exactly the model that was written — the two halves the pane depends on. */
const roundTrip = (edited: Workflow, original: string, why: string): string => {
  const out = serializeWorkflowPreserving(edited, original);
  const reread = parseWorkflow(out);
  assert.deepEqual(codes(reread.findings), [], `${why}\n--- emitted ---\n${out}`);
  assert.deepEqual(reread.workflow, edited, `${why}\n--- emitted ---\n${out}`);
  return out;
};

test("a section gaining its first child stops being an empty map (#1090)", () => {
  const cases: { section: string; edit: (w: Workflow) => Workflow; body: RegExp }[] = [
    {
      section: "resources: {}",
      edit: (w) => ({ ...w, resources: { catfish: {} } }),
      body: /^ {2}catfish: \{\}$/m,
    },
    {
      section: "intake: {}",
      edit: (w) => ({ ...w, intake: { source: "board" } }),
      body: /^ {2}source: board$/m,
    },
    {
      section: "merge_queue: {}",
      edit: (w) => ({ ...w, merge_queue: { enabled: true } }),
      body: /^ {2}enabled: true$/m,
    },
  ];
  for (const c of cases) {
    const original = oneWorker(c.section);
    const { workflow } = parseWorkflow(original);
    const out = roundTrip(c.edit(workflow), original, `${c.section} + one child`);
    const key = c.section.split(":")[0]!;
    assert.match(out, new RegExp(`^${key}:$`, "m"), "the key line must lose its `{}`");
    assert.match(out, c.body);
  }
});

test("a section losing its last child goes back to `{}`, not to nothing (#1090)", () => {
  // The silent inverse: leaving the bare `resources:` header behind re-reads as YAML null, so
  // the section a human deliberately kept (empty) would be gone on the next open — exactly what
  // `emitMappingSection` writes `{}` to prevent, one save later.
  const cases: { section: string; edit: (w: Workflow) => Workflow }[] = [
    { section: "resources:\n  catfish:\n    slots: 2", edit: (w) => ({ ...w, resources: {} }) },
    { section: "intake:\n  source: board", edit: (w) => ({ ...w, intake: {} }) },
    { section: "merge_queue:\n  enabled: true", edit: (w) => ({ ...w, merge_queue: {} }) },
  ];
  for (const c of cases) {
    const original = oneWorker(c.section);
    const { workflow } = parseWorkflow(original);
    const out = roundTrip(c.edit(workflow), original, `${c.section} − its last child`);
    const key = c.section.split(":")[0]!;
    assert.match(out, new RegExp(`^${key}: \\{\\}$`, "m"), "still declared, now empty");
  }
});

test("an empty flow sequence gaining its first item becomes a block header too (#1090)", () => {
  // `blocks: []` is this file's own empty-roster spelling (rev-5 F4) and `edges: []`/`gates: {}`
  // are shapes a hand-written file can carry, so all three can be handed their first entry.
  const emptyRoster = "version: 1\nname: t\n\nblocks: []\n";
  const { workflow } = parseWorkflow(emptyRoster);
  // The block comes from the READER (parsing a file that already has one) rather than a
  // hand-written literal, so the round-trip compares models, not optional-field spellings.
  const worker = parseWorkflow(oneWorker("")).workflow.blocks;
  const withBlock = roundTrip({ ...workflow, blocks: worker }, emptyRoster, "blocks: [] + one block");
  assert.match(withBlock, /^blocks:$/m);
  assert.match(withBlock, /^ {2}- id: w$/m);

  const two = oneWorker("  - id: r\n    name: R\n    kind: reviewer\n    cli: claude\n\nedges: []");
  const parsedTwo = parseWorkflow(two);
  const withEdge = roundTrip(
    { ...parsedTwo.workflow, edges: [{ from: "w", to: "r" }] },
    two,
    "edges: [] + one edge"
  );
  assert.match(withEdge, /^edges:$/m);
  assert.match(withEdge, /^ {2}- \{ from: w, to: r \}$/m);

  const gated = two.replace("edges: []", "gates: {}");
  const parsedGated = parseWorkflow(gated);
  const targetGates = parseWorkflow(
    two.replace("edges: []", "gates:\n  merge:\n    require: all\n    reviewers: [r]")
  ).workflow.gates; // read, not hand-written, for the same reason as `worker` above
  const withGate = roundTrip(
    { ...parsedGated.workflow, gates: targetGates },
    gated,
    "gates: {} + a merge gate"
  );
  assert.match(withGate, /^gates:$/m);
  assert.match(withGate, /^ {2}merge:$/m);
});

test("a hand-written one-line section is rewritten, not written twice (#1090)", () => {
  // A flow mapping carries the section's WHOLE content on the key line, so reusing that line
  // under a regenerated body would emit `build:` twice — once inline, once as a child.
  const original = oneWorker("resources: { build: { slots: 2 } }");
  const { workflow } = parseWorkflow(original);
  const out = roundTrip(
    { ...workflow, resources: { build: { slots: 2 }, docs: {} } },
    original,
    "an inline flow mapping + one more resource"
  );
  assert.equal(out.match(/build/g)?.length, 1, "the inline copy must be gone, not duplicated");

  // …and the same line REPLACED by `{}` when the model empties out, rather than left standing
  // and silently undoing the deletion.
  const emptied = roundTrip({ ...workflow, resources: {} }, original, "an inline mapping emptied");
  assert.match(emptied, /^resources: \{\}$/m);
});

test("rewriting a section's key line keeps the comments around it (#1090)", () => {
  // #233's bar still holds through the rewrite: the comment ABOVE the section introduces the
  // section, and the one ON the key line came from the same human — neither is about the
  // empty-vs-block spelling that had to change.
  const original = oneWorker("# THE POOLS\nresources: {} # none yet");
  const { workflow } = parseWorkflow(original);
  const out = roundTrip(
    { ...workflow, resources: { catfish: {} } },
    original,
    "a commented empty section + one child"
  );
  assert.match(out, /^# THE POOLS\nresources: # none yet\n {2}catfish: \{\}$/m);
});

test("emptying the roster carries the `blocks:` line's own comment onto `blocks: []` (#1090)", () => {
  // `pushBlocks`'s EMPTY branch rewrites the key line too — it always did, since `blocks: []` is
  // the canonical empty roster — and it now goes through the same helper, so the comment on that
  // line survives the rewrite instead of being dropped with it. Pinned separately from the
  // `pushSection` case above because it is a different call site: deleting the last block is the
  // ordinary way a human reaches it.
  const original = `version: 1
name: t

# THE ROSTER
blocks: # the agents a run may use
  - id: w
    name: W
    kind: worker
    cli: claude
`;
  const { workflow } = parseWorkflow(original);
  const out = serializeWorkflowPreserving({ ...workflow, blocks: [] }, original);
  assert.deepEqual(codes(parseWorkflow(out).findings), [], out);
  assert.match(
    out,
    /^# THE ROSTER\nblocks: \[\] # the agents a run may use$/m,
    "both comments survive — the one introducing the section and the one on its key line"
  );
  // …and the roster refilled from there keeps them again, back in block form.
  const refilled = serializeWorkflowPreserving(workflow, out);
  assert.match(refilled, /^# THE ROSTER\nblocks: # the agents a run may use$/m);
  assert.deepEqual(parseWorkflow(refilled).workflow, workflow);
});

test("an untouched empty section is still reproduced byte for byte (#1090)", () => {
  // The rewrite is for a section whose body was REGENERATED. A file nobody touched — including
  // the `{}` sections this fix is about — must still come back exactly as it went in.
  const original = oneWorker("# THE POOLS\nresources: {} # none yet\n\nintake: {}");
  const { workflow } = parseWorkflow(original);
  assert.equal(serializeWorkflowPreserving(workflow, original), original);
  const renamed: Workflow = { ...workflow, name: "t2" };
  assert.equal(
    serializeWorkflowPreserving(renamed, original),
    original.replace("name: t", "name: t2"),
    "an edit ELSEWHERE must not reformat the empty sections either"
  );
});
