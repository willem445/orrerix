// Unit tests for the pure workflow model (#222): reading the repo workflow file,
// writing it back canonically, deriving its graph, and — the part that earns its
// keep — the PRE-RUN VALIDATION pass that every workflow tool surveyed in the #222
// investigation skipped.
//
// These test what the pane promises the human, not how it is written: that a file
// survives a round-trip unchanged, that a canonical save doesn't churn the diff, that
// a broken file still OPENS (as stubs + findings, never a refusal), and that each
// validation rule fires on the mistake it exists to catch and stays quiet otherwise.
//
// This file is the READ half, plus the round-trip pins that span both halves. Split out
// of test/workflowmodel.test.ts alongside src/workflowmodel.ts (#3498 F2); its siblings
// are workflowserialize, workflowvalidate and workflowgraph (.test.ts).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  parseWorkflow,
  serializeWorkflow,
  analyzeWorkflow,
  formatWorkflowText,
  starterWorkflow,
  scaffoldWorkflowText,
  type Workflow,
} from "../src/workflowmodel.ts";
import { SAMPLE, codes, has } from "./support/workflowfixtures.ts";

// ---------- reading the schema ----------

test("reads every part of the §4 schema", () => {
  const { workflow, findings } = parseWorkflow(SAMPLE);
  assert.deepEqual(findings, [], "the reference schema must parse cleanly");

  assert.equal(workflow.version, 1);
  assert.equal(workflow.name, "focused-review");
  assert.deepEqual(
    workflow.blocks.map((b) => b.id),
    ["planner", "worker", "rev-security", "rev-tests"]
  );

  const worker = workflow.blocks[1]!;
  assert.equal(worker.kind, "worker");
  assert.equal(worker.cli, "copilot");
  assert.equal(worker.profile, ".github/agents/worker.md", "a profile: path is the Copilot native --agent form");
  assert.equal(worker.prompt, undefined);

  const sec = workflow.blocks[2]!;
  assert.equal(sec.model, "opus");
  assert.match(sec.prompt ?? "", /^Review ONLY for security defects/);
  assert.match(sec.prompt ?? "", /Ignore style and perf/, "a block scalar keeps its line breaks");

  // The fan-out `to: [a, b]` becomes one flat edge per target — that is what reachability
  // and in-degree are asked of.
  assert.deepEqual(workflow.edges, [
    { from: "planner", to: "worker" },
    { from: "worker", to: "rev-security" },
    { from: "worker", to: "rev-tests" },
  ]);

  assert.deepEqual(workflow.gates.merge, {
    require: "all-pass",
    reviewers: ["rev-security", "rev-tests"],
    also: ["ci-green"],
  });
});

test("a comment is never mistaken for content, and a # inside a prompt survives", () => {
  const { workflow } = parseWorkflow(`version: 1
name: x   # the workflow's name
blocks:
  - id: rev
    name: Rev
    kind: reviewer
    cli: claude
    prompt: |
      # Checklist
      Check the auth path.
`);
  assert.equal(workflow.name, "x");
  assert.equal(workflow.blocks[0]!.prompt, "# Checklist\nCheck the auth path.\n");
});

// ---------- round-trip + canonical stability ----------

test("model → text → model is lossless", () => {
  const original = parseWorkflow(SAMPLE).workflow;
  const reread = parseWorkflow(serializeWorkflow(original)).workflow;
  assert.deepEqual(reread, original);
});

test("formatting is idempotent — a canonical save never churns the diff", () => {
  const once = formatWorkflowText(SAMPLE);
  const twice = formatWorkflowText(once);
  assert.equal(twice, once, "formatting an already-canonical file must be a no-op");
  // And a cosmetically different file with the same meaning canonicalizes to the SAME
  // text — the whole point of having one shape.
  const reordered = SAMPLE.replace("    name: Planner\n", "").replace(
    "  - id: planner\n",
    "  - id: planner\n    name: Planner\n"
  );
  assert.equal(formatWorkflowText(reordered), once);
});

test("keys this build doesn't know survive a round-trip", () => {
  // A file written by a NEWER loomux must not be silently stripped by an older pane —
  // the form would otherwise delete a field the user's backend depends on.
  const text = `version: 1
retries: 3
blocks:
  - id: w
    name: W
    kind: worker
    cli: claude
    timeout: 900
`;
  const w = parseWorkflow(text).workflow;
  assert.deepEqual(w.extra, { retries: 3 });
  assert.deepEqual(w.blocks[0]!.extra, { timeout: 900 });
  const out = serializeWorkflow(w);
  assert.match(out, /^retries: 3$/m);
  assert.match(out, /^ {4}timeout: 900$/m);
  assert.deepEqual(parseWorkflow(out).workflow, w);
});

test("canonical form fixes key order and orders references by the roster", () => {
  const w = parseWorkflow(`version: 1
blocks:
  - cli: claude
    kind: reviewer
    id: rev-b
    name: B
  - id: rev-a
    name: A
    kind: reviewer
    cli: claude
  - id: worker
    name: W
    kind: worker
    cli: claude
edges:
  - { from: worker, to: rev-a }
  - { from: worker, to: rev-b }
gates:
  merge:
    require: all-pass
    reviewers: [rev-a, rev-b]
`).workflow;
  const out = serializeWorkflow(w);
  // Fixed key order per block…
  assert.match(out, /- id: rev-b\n {4}name: B\n {4}kind: reviewer\n {4}cli: claude/);
  // …blocks keep their AUTHORED order (re-sorting the roster on save would churn the
  // very diff the canonical form exists to keep legible)…
  assert.deepEqual(
    parseWorkflow(out).workflow.blocks.map((b) => b.id),
    ["rev-b", "rev-a", "worker"]
  );
  // …and a fan-out collapses to one entry per source, its targets in ROSTER order
  // (rev-b is declared first), not alphabetical order.
  assert.match(out, /- \{ from: worker, to: \[rev-b, rev-a\] \}/);
  assert.match(out, /reviewers: \[rev-b, rev-a\]/);
});

test("a prompt's trailing newline is preserved exactly", () => {
  const withNl: Workflow = {
    ...starterWorkflow(),
    blocks: [{ id: "r", name: "R", kind: "reviewer", cli: "claude", model: "", prompt: "a\nb\n" }],
    edges: [],
    gates: {},
  };
  const withoutNl: Workflow = {
    ...withNl,
    blocks: [{ ...withNl.blocks[0]!, prompt: "a\nb" }],
  };
  assert.match(serializeWorkflow(withNl), /prompt: \|\n/);
  assert.match(serializeWorkflow(withoutNl), /prompt: \|-\n/);
  assert.equal(parseWorkflow(serializeWorkflow(withNl)).workflow.blocks[0]!.prompt, "a\nb\n");
  assert.equal(parseWorkflow(serializeWorkflow(withoutNl)).workflow.blocks[0]!.prompt, "a\nb");
});

// ---------- the flow-context quoting bug (rev-5 F1) ----------
//
// The emitter serves BOTH block context (`name: …`) and FLOW context (`reviewers: [a, b]`,
// an unknown key's array or map), and in flow context `, [ ] { }` are STRUCTURAL. Quoting
// only for block context meant an ordinary form edit — every one of which re-serializes the
// file — silently destroyed any value containing one. These are the values that actually
// occur: `allow` patterns of exactly this shape are what the backend's agent profiles carry.

test("a comma inside a flow-emitted value does not split it into two", () => {
  const w = starterWorkflow();
  w.gates.merge!.also = ["Bash(gh pr view --json title,body)", "ci-green"];
  const reread = parseWorkflow(serializeWorkflow(w)).workflow;
  assert.deepEqual(
    reread.gates.merge!.also,
    ["Bash(gh pr view --json title,body)", "ci-green"],
    "a comma is structural in a flow list — unquoted, this came back as three conditions"
  );
});

test("braces and brackets inside a flow-emitted value do not destroy it", () => {
  // Unquoted, the mid-string `}` closed the flow collection early, the reader threw, and the
  // whole value came back as `null` — with a bogus syntax finding on a line the pane itself
  // had just written.
  const w = starterWorkflow();
  w.gates.merge!.also = ["fmt{x}", "arr[0]", "map{a: b}"];
  const out = serializeWorkflow(w);
  const { workflow: reread, findings } = parseWorkflow(out);
  assert.deepEqual(reread.gates.merge!.also, ["fmt{x}", "arr[0]", "map{a: b}"]);
  assert.deepEqual(findings, [], "and it must not report a syntax error against its own output");
});

test("unknown keys holding arrays and maps survive a round-trip, structural characters and all", () => {
  // The PR's stated guarantee — "an older pane never strips a newer file's fields" — is only
  // true if it holds for the values those fields actually carry. The original unknown-key
  // test used scalars only (`retries: 3`), which is exactly the hole this closes.
  const text = `version: 1
blocks:
  - id: w
    name: W
    kind: worker
    cli: claude
    tools: ["fmt{x}", "Read"]
    allow: ["Bash(gh pr view --json title,body)", "Bash(git status)"]
    limits: { cpu: 2, note: "a,b" }
`;
  const w = parseWorkflow(text).workflow;
  assert.deepEqual(w.blocks[0]!.extra, {
    tools: ["fmt{x}", "Read"],
    limits: { cpu: 2, note: "a,b" },
  });
  // `allow:` is a KNOWN field since #880 (it was a `RawBlock` field all along — the pane
  // just never had a name for it), so it leaves `extra` and lands on the block. The quoted
  // comma is why it was worth putting in this test at all, and that half is unchanged: the
  // pattern has to survive as ONE entry, not two.
  assert.deepEqual(w.blocks[0]!.allow, [
    "Bash(gh pr view --json title,body)",
    "Bash(git status)",
  ]);
  const out = serializeWorkflow(w);
  const reread = parseWorkflow(out);
  assert.deepEqual(reread.findings, [], "the serialized form must re-read cleanly");
  assert.deepEqual(reread.workflow, w, "…and identically — a form edit must not eat a field it doesn't know");
  // Twice, because the corruption in the original bug only appeared on the SECOND read.
  assert.equal(serializeWorkflow(reread.workflow), out);
});

test("an escaped backslash is not re-read as the start of another escape (rev-6 F8)", () => {
  // A Windows path is the obvious carrier, and it is one form edit away: `C:\new,dir` emits
  // as "C:\\new,dir", and unescaping in the wrong order expanded the `\n` — of the escaped
  // BACKSLASH plus the letter n — into a newline before the `\\` could collapse. It read back
  // as `C:` + newline + `ew,dir`. (The comma is what drags a path into the quoted path at
  // all, so this only became reachable when F1 widened quoting.)
  for (const raw of ["C:\\new,dir", "C:\\temp\\{x}", "a\\\\b", 'quote " and \\ backslash, comma']) {
    const w = starterWorkflow();
    w.gates.merge!.also = [raw];
    w.blocks[0]!.model = raw;
    const reread = parseWorkflow(serializeWorkflow(w)).workflow;
    assert.equal(reread.gates.merge!.also[0], raw, `flow: ${JSON.stringify(raw)}`);
    assert.equal(reread.blocks[0]!.model, raw, `block: ${JSON.stringify(raw)}`);
  }
  // Real escapes still decode — the fix must not turn \n into a literal "n".
  assert.equal(parseWorkflow('version: 1\nname: "a\\nb\\tc"').workflow.name, "a\nb\tc");
});

test("a KEY carrying structural characters survives too (rev-6 F9)", () => {
  // The value side of this was F1; the key side is the same bug with the pair swapped. An
  // unknown key's nested map is arbitrary data from a newer loomux — its keys are as free as
  // its values, and emitting them raw split or truncated the map on re-read.
  const w = starterWorkflow();
  w.blocks[0]!.extra = {
    limits: { "cpu,mem": 2, "brace{}": "x", "colon: here": true },
    "top,key": ["a,b"],
  };
  const out = serializeWorkflow(w);
  const reread = parseWorkflow(out);
  assert.deepEqual(reread.findings, [], "the pane must not report a syntax error on its own output");
  assert.deepEqual(reread.workflow.blocks[0]!.extra, {
    limits: { "cpu,mem": 2, "brace{}": "x", "colon: here": true },
    "top,key": ["a,b"],
  });
  assert.equal(serializeWorkflow(reread.workflow), out, "…and it stays stable");
});

test("a value that would change meaning unquoted is quoted", () => {
  const w: Workflow = {
    version: 1,
    name: "yes: really",
    blocks: [{ id: "w", name: "1.5", kind: "worker", cli: "claude", model: "" }],
    edges: [],
    gates: {},
  };
  const reread = parseWorkflow(serializeWorkflow(w)).workflow;
  assert.equal(reread.name, "yes: really");
  assert.equal(reread.blocks[0]!.name, "1.5", "a numeric-looking NAME must come back a string");
});

test("a tab-indented file is reported, not silently accepted (rev-5 F2)", () => {
  // YAML forbids tabs in indentation, so the backend validator will refuse this file. A pane
  // that reports `valid` on a file the spawn then rejects is worse than one that says
  // nothing — the human is told their workflow is good and the run fails anyway.
  const { findings } = analyzeWorkflow("version: 1\nblocks:\n\t- id: w\n");
  const tab = findings.find((f) => f.code === "yaml-syntax" && /tab/i.test(f.message));
  assert.ok(tab, "a tab in the indentation must produce a finding");
  assert.equal(tab!.line, 3, "and it must say which line");
  // Reported ONCE, not once per re-peek of the same line.
  assert.equal(findings.filter((f) => /tab/i.test(f.message)).length, 1);
});

test("a tab INSIDE a prompt is content, and stays content", () => {
  // The guard is about indentation. A prompt body is text — a tab in it is the user's tab.
  const { workflow, findings } = analyzeWorkflow(`version: 1
blocks:
  - id: rev
    name: R
    kind: reviewer
    cli: claude
    prompt: |
      col1\tcol2
`);
  assert.equal(workflow.blocks[0]!.prompt, "col1\tcol2\n");
  assert.deepEqual(codes(findings), []);
});

test("a prompt whose first line is indented round-trips (rev-5 F3)", () => {
  // Straight out of the form's textarea: a code snippet, an indented checklist. A bare `|`
  // is read back by dedenting to the first content line's indent, which ate exactly this.
  for (const prompt of ["  indented\nplain\n", "\n  after a blank line\n", "\tstarts with a tab\n"]) {
    const w = starterWorkflow();
    w.blocks[2]!.prompt = prompt;
    const out = serializeWorkflow(w);
    assert.equal(
      parseWorkflow(out).workflow.blocks[2]!.prompt,
      prompt,
      `prompt ${JSON.stringify(prompt)} must survive`
    );
    assert.equal(serializeWorkflow(parseWorkflow(out).workflow), out, "…and stay stable");
  }
});

test("an empty roster serializes to something that re-reads as an empty roster (rev-5 F4)", () => {
  // Delete the last block in the form and the pane used to report a YAML-shape error against
  // text it had just written itself (a bare `blocks:` is YAML null).
  const empty: Workflow = { version: 1, name: "x", blocks: [], edges: [], gates: {} };
  const out = serializeWorkflow(empty);
  const { workflow, findings } = analyzeWorkflow(out);
  assert.deepEqual(workflow.blocks, []);
  assert.deepEqual(codes(findings), ["no-blocks"], "the honest error, and ONLY the honest error");
  // A hand-authored bare `blocks:` means the same thing and must not be a shape error either.
  assert.deepEqual(codes(analyzeWorkflow("version: 1\nblocks:\n").findings), ["no-blocks"]);
});

// ---------- the empty-state bug (v2) ----------

test("a BOM does not make a valid workflow look broken", () => {
  // A workflow file written by a Windows editor starts with U+FEFF. The reader took it as
  // part of the first KEY, so `version: 1` arrived as a key named "﻿version" and the pane
  // reported `version-missing` against a file the human could see was correct — and the
  // character is INVISIBLE, so nothing in the error could lead them to the cause.
  const { workflow, findings } = analyzeWorkflow("﻿" + SAMPLE);
  assert.deepEqual(codes(findings), []);
  assert.equal(workflow.version, 1);
  assert.equal(workflow.blocks.length, 4);
});

test("the scaffold is a valid workflow, and canonicalizes to the same one", () => {
  // What a repo with no workflow gets when the human asks for one. If this stops parsing
  // clean, every new workflow in the world starts life with a finding on it.
  const { workflow, findings } = analyzeWorkflow(scaffoldWorkflowText("0.9.0"));
  assert.deepEqual(codes(findings), [], "a scaffold that isn't valid is a scaffold that lies");
  assert.deepEqual(
    workflow.blocks.map((b) => b.id),
    ["planner", "worker", "reviewer"]
  );
  assert.deepEqual(workflow.edges, [
    { from: "planner", to: "worker" },
    { from: "worker", to: "reviewer" },
  ]);
  assert.deepEqual(workflow.gates.merge, { require: "all-pass", reviewers: ["reviewer"], also: [] });
  // A TYPED field since #880, not an unknown key riding `extra` — the pane knows this key
  // now, which is also why the scaffold stays finding-free with unknown keys reported.
  assert.equal(workflow.authored_with, "0.9.0");
  assert.equal(workflow.extra, undefined);
  // It is the same workflow the model's starter describes — the commented file and the
  // programmatic one must not drift into being two different pipelines.
  const starter = starterWorkflow("0.9.0");
  assert.deepEqual(workflow.blocks.map((b) => b.id), starter.blocks.map((b) => b.id));
  assert.deepEqual(workflow.edges, starter.edges);
  // And a form edit (which re-serializes) produces canonical text that still round-trips.
  const canonical = serializeWorkflow(workflow);
  assert.equal(serializeWorkflow(parseWorkflow(canonical).workflow), canonical);
});

// ---------- broken files still open ----------

test("a file that cannot be fully understood still opens, with findings", () => {
  const { workflow, findings } = analyzeWorkflow(`version: 1
blocks:
  - id: mystery
    name: Mystery
    kind: superuser
    cli: goose
`);
  // The block is a STUB, not a dropped row: a block you cannot see is a block you
  // cannot repair (the ComfyUI import-failure class the design note names).
  assert.equal(workflow.blocks.length, 1);
  assert.equal(workflow.blocks[0]!.id, "mystery");
  assert.ok(has(findings, "unknown-kind"));
  assert.ok(has(findings, "unknown-cli"));
});

test("a syntax error is a finding on a line, not a thrown parse", () => {
  const { findings, workflow } = analyzeWorkflow(`version: 1
blocks:
  - id: w
    name: W
    kind: worker
    cli: [claude
`);
  const syntax = findings.find((f) => f.code === "yaml-syntax");
  assert.ok(syntax, "an unterminated flow list must report as a finding");
  assert.equal(syntax!.line, 6, "and it must say WHICH line");
  assert.equal(workflow.blocks.length, 1, "the rest of the file still loads");
});

test("an unexpected top-level `-` is a finding, not a silent truncation (#270)", () => {
  // The reader used to treat ANY `-`-prefixed line as "a sequence at this level ends the
  // mapping" — correct when handing a same-indent sequence off to an enclosing key, but
  // `mapping(0)` (called once, from `document()`) has no enclosing key to hand off to. It
  // just stopped, silently, and everything from that line to EOF vanished with zero findings.
  const { workflow, findings } = analyzeWorkflow(`version: 1
- id: a
  name: A
  kind: worker
  cli: claude
`);
  const syntax = findings.find((f) => f.code === "yaml-syntax");
  assert.ok(syntax, "an orphan top-level dash must report as a finding");
  assert.equal(syntax!.line, 2, "and it must say WHICH line");
  assert.equal(workflow.blocks.length, 0, "there was no `blocks:` key at all — nothing to read");
});

test("a top-level `blocks:` roster is still read after an orphan `-` line earlier in the file", () => {
  // The reader recovers: it consumes the whole orphan sequence (reporting it once) and keeps
  // reading the rest of the document as a mapping, rather than treating the entire remainder
  // as lost.
  const { workflow, findings } = analyzeWorkflow(`version: 1
- id: orphan
  name: Orphan
blocks:
  - id: w
    name: W
    kind: worker
    cli: claude
`);
  assert.ok(findings.find((f) => f.code === "yaml-syntax"));
  assert.deepEqual(
    workflow.blocks.map((b) => b.id),
    ["w"],
    "the real roster after the orphan line is still read, not dropped too"
  );
});

test("an empty file is a workflow with nothing in it, not an error page", () => {
  const { findings, workflow } = analyzeWorkflow("");
  assert.equal(workflow.blocks.length, 0);
  assert.ok(has(findings, "no-blocks"));
  assert.ok(!has(findings, "yaml-syntax"));
});
