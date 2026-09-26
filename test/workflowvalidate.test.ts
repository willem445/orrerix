// Unit tests for the workflow model's PRE-RUN validation pass (#222) and the rules it
// mirrors from the engine: kinds, role hints, the manager, remote blocks, model knobs, the
// full config surface, routing, and the workflow-name rule. Split out of
// test/workflowmodel.test.ts alongside src/workflowmodel.ts (#3498 F2).
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  parseWorkflow,
  serializeWorkflow,
  serializeWorkflowPreserving,
  validateWorkflow,
  analyzeWorkflow,
  starterWorkflow,
  GATE_ROUTING_RULES_MAX,
  GATE_ROUTING_PATHS_MAX,
  isValidBlockId,
  isBlockKind,
  allowDenialReason,
  personaDenialReason,
  isRemoteLabel,
  remoteDenialReason,
  REMOTE_LABEL_MAX,
  isReviewingBlock,
  isWorkflowCli,
  isValidIntakeLabel,
  isValidResourceName,
  sanitizeAllowPattern,
  roleHintsForKind,
  hasErrors,
  WORKFLOW_FILE,
  LEGACY_WORKFLOW_FILE,
  CONFIG_DIR,
  LEGACY_CONFIG_DIR,
  BLOCK_KINDS,
  ROLE_HINTS,
  INTAKE_SOURCES,
  ID_MAX_CHARS,
  RESOURCES_MAX,
  RESOURCE_SLOTS_MAX,
  MERGE_QUEUE_MAX_BATCH_MIN,
  WORKFLOW_VERSION,
  roleHintRequires,
  WORKFLOWS_DIR,
  LEGACY_WORKFLOWS_DIR,
  DEFAULT_WORKFLOW_NAME,
  WORKFLOW_NAME_MAX,
  isWorkflowName,
  workflowRelFor,
  workflowNameOf,
  type Workflow,
  type Finding,
} from "../src/workflowmodel.ts";
import { readFileSync } from "node:fs";
import { knobState, type CliKnobs } from "../src/selectorknobs.ts";
import { SAMPLE, codes, has } from "./support/workflowfixtures.ts";

// ---------- validation: one rule at a time ----------

test("the reference workflow validates clean", () => {
  assert.deepEqual(codes(analyzeWorkflow(SAMPLE).findings), []);
  assert.deepEqual(codes(validateWorkflow(starterWorkflow())), []);
});

test("kind must be a capability class — a workflow can never invent one", () => {
  const w = starterWorkflow();
  w.blocks[0]!.kind = "superuser";
  const f = validateWorkflow(w);
  assert.ok(has(f, "unknown-kind"));
  assert.equal(f.find((x) => x.code === "unknown-kind")!.blockId, "planner");
  // Every declared class is accepted, so the rule cannot drift from the enum.
  for (const kind of BLOCK_KINDS) {
    const ok = starterWorkflow();
    ok.blocks[0]!.kind = kind;
    assert.ok(!has(validateWorkflow(ok), "unknown-kind"), `${kind} must be accepted`);
  }
});

test("cli must be one loomux can actually spawn", () => {
  const w = starterWorkflow();
  w.blocks[1]!.cli = "goose";
  assert.ok(has(validateWorkflow(w), "unknown-cli"));
});

test("duplicate and malformed block ids are caught", () => {
  const dup = starterWorkflow();
  dup.blocks[1]!.id = "planner";
  assert.ok(has(validateWorkflow(dup), "block-id-duplicate"));

  const bad = starterWorkflow();
  bad.blocks[0]!.id = "Rev Security!";
  assert.ok(has(validateWorkflow(bad), "block-id-invalid"));

  const missing = starterWorkflow();
  missing.blocks[0]!.id = "";
  assert.ok(has(validateWorkflow(missing), "block-id-missing"));

  assert.ok(isValidBlockId("rev-security"));
  assert.ok(isValidBlockId("rev_2"));
  assert.ok(!isValidBlockId("2rev"));
  assert.ok(!isValidBlockId("rev security"));
  assert.ok(!isValidBlockId("../etc"));
});

test("an edge to a block that doesn't exist is caught before anything spawns", () => {
  const w = starterWorkflow();
  w.edges.push({ from: "worker", to: "rev-perf" });
  const f = validateWorkflow(w);
  assert.ok(has(f, "edge-unknown-block"));
  assert.match(f.find((x) => x.code === "edge-unknown-block")!.message, /rev-perf/);

  const self = starterWorkflow();
  self.edges.push({ from: "worker", to: "worker" });
  assert.ok(has(validateWorkflow(self), "edge-self"));
});

test("a gate that could never open is an error, not a runtime surprise", () => {
  // The reviewer it names doesn't exist…
  const ghost = starterWorkflow();
  ghost.gates.merge!.reviewers = ["rev-perf"];
  assert.ok(has(validateWorkflow(ghost), "gate-unknown-reviewer"));

  // …it names a block that isn't a reviewer (only a reviewer records a verdict)…
  const notRev = starterWorkflow();
  notRev.gates.merge!.reviewers = ["worker"];
  assert.ok(has(validateWorkflow(notRev), "gate-not-a-reviewer"));

  // …it needs more passes than there are reviewers…
  const greedy = starterWorkflow();
  greedy.gates.merge = { require: "threshold", threshold: 2, reviewers: ["reviewer"], also: [] };
  assert.ok(has(validateWorkflow(greedy), "gate-bad-threshold"));

  // …a threshold gate with no threshold…
  const noN = starterWorkflow();
  noN.gates.merge = { require: "threshold", reviewers: ["reviewer"], also: [] };
  assert.ok(has(validateWorkflow(noN), "gate-bad-threshold"));

  // …it gates on nothing at all…
  const empty = starterWorkflow();
  empty.gates.merge!.reviewers = [];
  assert.ok(has(validateWorkflow(empty), "gate-no-reviewers"));

  // …or it requires something we don't know how to enforce.
  const odd = starterWorkflow();
  odd.gates.merge!.require = "vibes";
  assert.ok(has(validateWorkflow(odd), "gate-unknown-require"));

  // A well-formed threshold gate is clean.
  const good = starterWorkflow();
  good.blocks.push({ id: "rev-2", name: "R2", kind: "reviewer", cli: "claude", model: "" });
  good.edges.push({ from: "worker", to: "rev-2" });
  good.gates.merge = { require: "threshold", threshold: 2, reviewers: ["reviewer", "rev-2"], also: [] };
  assert.deepEqual(codes(validateWorkflow(good)), []);
});

test("the small-batch clause survives a round trip and refuses a limit that limits nothing (#1174)", () => {
  // `MergeGate` has NO unknown-key bag, so a gate key the parser does not read is a
  // line the next form edit silently DELETES. That is the failure this covers, and
  // it is why the assertion is a round trip rather than a parse.
  const text = `version: 1
blocks:
  - id: worker
    kind: worker
    cli: claude
  - id: reviewer
    kind: reviewer
    cli: claude
gates:
  merge:
    require: all-pass
    reviewers: [reviewer]
    max_diff_lines: 800
`;
  const parsed = parseWorkflow(text).workflow;
  assert.equal(parsed.gates.merge!.max_diff_lines, 800);
  const round = parseWorkflow(serializeWorkflow(parsed)).workflow;
  assert.equal(round.gates.merge!.max_diff_lines, 800, "a save must not drop the limit");
  assert.deepEqual(codes(validateWorkflow(parsed)), [], "a declared limit is a clean file");

  // Undeclared stays undeclared — the emitter writes no line, so opening the gate
  // form on a repo with no limit cannot invent one.
  const none = starterWorkflow();
  assert.equal(none.gates.merge!.max_diff_lines, undefined);
  assert.ok(!serializeWorkflow(none).includes("max_diff_lines"));

  // 0 is a file the ENGINE refuses, so the pane must say so rather than bless it.
  const zero = starterWorkflow();
  zero.gates.merge!.max_diff_lines = 0;
  assert.ok(has(validateWorkflow(zero), "gate-bad-max-diff-lines"));
  const fractional = starterWorkflow();
  fractional.gates.merge!.max_diff_lines = 1.5;
  assert.ok(has(validateWorkflow(fractional), "gate-bad-max-diff-lines"));
  // …and a positive whole number is clean, so the two assertions above cannot be
  // passing against a rule that simply flags everything.
  const ok = starterWorkflow();
  ok.gates.merge!.max_diff_lines = 1;
  assert.deepEqual(codes(validateWorkflow(ok)), []);

  // A non-number in the FILE is a finding at parse time, never a coerced value.
  const bad = parseWorkflow(text.replace("max_diff_lines: 800", "max_diff_lines: eight"));
  assert.ok(bad.findings.some((f) => f.code === "gate-bad-max-diff-lines"));
});

test("a block declaring both a prompt and a profile is ambiguous", () => {
  const w = starterWorkflow();
  w.blocks[2]!.prompt = "Review the auth path.";
  w.blocks[2]!.profile = ".github/agents/rev.md";
  assert.ok(has(validateWorkflow(w), "prompt-and-profile"));
});

test("role_hint requires its matching capability class (#250/#324)", () => {
  // advisor -> planner, process -> worker. Mirrors the backend's
  // `role_hint_requires` (workflow.rs) so this pane's pre-run pass never
  // disagrees with what the real parser would say.
  const advisorOk = starterWorkflow();
  advisorOk.blocks[0]!.role_hint = "advisor"; // blocks[0] is the planner
  assert.deepEqual(codes(validateWorkflow(advisorOk)), []);

  const processOk = starterWorkflow();
  processOk.blocks[1]!.role_hint = "process"; // blocks[1] is the worker
  assert.deepEqual(codes(validateWorkflow(processOk)), []);

  // The mismatched pairing is a NAMED finding, not a silent no-op.
  const mismatched = starterWorkflow();
  mismatched.blocks[1]!.role_hint = "advisor"; // worker, not planner
  const f = validateWorkflow(mismatched);
  assert.ok(has(f, "role-hint-wrong-kind"));
  assert.equal(f.find((x) => x.code === "role-hint-wrong-kind")!.blockId, "worker");

  const mismatched2 = starterWorkflow();
  mismatched2.blocks[0]!.role_hint = "process"; // planner, not worker
  assert.ok(has(validateWorkflow(mismatched2), "role-hint-wrong-kind"));

  // An unrecognized value is its own finding, never coerced to the nearest hint.
  const bogus = starterWorkflow();
  bogus.blocks[0]!.role_hint = "supervisor";
  assert.ok(has(validateWorkflow(bogus), "role-hint-unknown"));

  // Absent is clean — today's behavior, byte for byte.
  assert.deepEqual(codes(validateWorkflow(starterWorkflow())), []);
});

test("role_hint case handling matches the backend's lowercasing (#250/#324 rider)", () => {
  // `role_hint_requires` (workflow.rs) trims and lowercases before comparing, so
  // `role_hint: Advisor` parses clean on the real engine. This pane's pre-run pass
  // must agree, or it flags a file the real parser accepts as broken.
  assert.equal(roleHintRequires("Advisor"), "planner");
  assert.equal(roleHintRequires("ADVISOR"), "planner");
  assert.equal(roleHintRequires(" process "), "worker");
  assert.equal(roleHintRequires("Process"), "worker");
  assert.equal(roleHintRequires("supervisor"), undefined, "still rejected, never coerced");

  const w = starterWorkflow();
  w.blocks[0]!.role_hint = "Advisor"; // blocks[0] is the planner
  assert.deepEqual(
    codes(validateWorkflow(w)),
    [],
    "a capitalized role_hint the real parser accepts must not be flagged as unknown here"
  );
});

test("role_hint: liaison requires kind: reviewer (#891)", () => {
  // The human-facing pane rides the reviewer capability class. Mirrors the
  // backend's `role_hint_requires`, including its trim + lowercase, so this
  // pane never flags a file the real parser accepts.
  assert.equal(roleHintRequires("liaison"), "reviewer");
  assert.equal(roleHintRequires(" Liaison "), "reviewer");
  assert.equal(roleHintRequires("liason"), undefined, "a typo is rejected, never coerced");

  // On a reviewer block the gate does not name: no hint finding, no gate finding.
  const ok = starterWorkflow();
  ok.blocks.push({
    id: "human",
    name: "Liaison",
    kind: "reviewer",
    cli: "claude",
    model: "",
    role_hint: "liaison",
  });
  ok.edges.push({ from: "worker", to: "human" });
  // Exactly the deprecation advisory and nothing else (#1161 D4): the pairing is
  // legal, the block runs, and the ONE thing the author is told is where the
  // feature moved. Spelled as the full expected list rather than as "no errors",
  // so a spurious second finding on a valid liaison still fails here.
  assert.deepEqual(
    codes(validateWorkflow(ok)).filter((c) => c.startsWith("role-hint") || c.startsWith("gate-")),
    ["role-hint-superseded"]
  );
  assert.deepEqual(
    validateWorkflow(ok).filter((f) => f.code === "role-hint-superseded").map((f) => f.severity),
    ["warning"],
    "superseded is an ADVISORY — a `liaison` file must still load and run"
  );

  // On every other kind it is the same named finding — "requires reviewer" is a
  // claim about the whole set of kinds it is NOT allowed on, so check them all.
  for (const [i, kind] of [
    [0, "planner"],
    [1, "worker"],
  ] as const) {
    const bad = starterWorkflow();
    bad.blocks[i]!.role_hint = "liaison";
    const f = validateWorkflow(bad);
    assert.ok(has(f, "role-hint-wrong-kind"), `liaison on a ${kind} block must be flagged`);
    assert.equal(f.find((x) => x.code === "role-hint-wrong-kind")!.blockId, kind);
  }
});

test("a merge gate may not name a liaison as one of its reviewers (#891)", () => {
  // A liaison IS reviewer-kind, so it slips past the "not a reviewer" check —
  // but it never records a verdict, so a gate naming one could never open.
  // The backend refuses the same file at parse; this pane must not call it fine.
  const w = starterWorkflow();
  w.blocks[2]!.role_hint = "liaison"; // blocks[2] is the block the gate names
  const f = validateWorkflow(w);
  assert.ok(has(f, "gate-not-a-reviewer"), `a gate naming a liaison must be flagged: ${codes(f)}`);
  assert.match(
    f.find((x) => x.code === "gate-not-a-reviewer")!.message,
    /liaison/,
    "and the message must say why, not just that the block is wrong"
  );

  // The control: the same document without the hint is clean, so the finding
  // above is attributable to the liaison rule and not to the fixture.
  assert.ok(!has(validateWorkflow(starterWorkflow()), "gate-not-a-reviewer"));
});

// ---------- #1161 M1: the `manager` kind ----------

/** `starterWorkflow()` plus a manager block, wired so it is not also flagged
 *  isolated — the shape a repo declaring one actually writes. */
const withManager = (id = "manager"): Workflow => {
  const w = starterWorkflow();
  w.blocks.push({ id, name: "Manager", kind: "manager", cli: "claude", model: "opus" });
  w.edges.push({ from: id, to: "planner" });
  return w;
};

test("kind: manager is a class this pane knows (#1161)", () => {
  // The mirror of the backend's `kind_from_str`. If these drift, the pane
  // paints a file the real engine accepts red — or, worse, saves one it
  // refuses.
  assert.ok(BLOCK_KINDS.includes("manager"));
  assert.ok(isBlockKind("manager"));
  assert.deepEqual(codes(validateWorkflow(withManager())), [], "a declared manager is an ordinary roster");

  // The negative control, and the half that makes the line "closed" rather than
  // "we added one more": a near-miss is still refused.
  assert.ok(!isBlockKind("managers"));
});

test("at most one manager block (#1161)", () => {
  const two = withManager();
  two.blocks.push({ id: "second-desk", name: "Desk 2", kind: "manager", cli: "claude", model: "" });
  two.edges.push({ from: "second-desk", to: "planner" });
  const f = validateWorkflow(two);
  assert.ok(has(f, "manager-not-unique"), `a second manager must be flagged: ${codes(f)}`);
  // Both ids, not just the later one — the second declaration is no more wrong
  // than the first, and the author needs to see which two they wrote.
  const msg = f.find((x) => x.code === "manager-not-unique")!.message;
  assert.match(msg, /manager/);
  assert.match(msg, /second-desk/);

  // The control: one manager is fine, so the finding is about the SECOND.
  assert.ok(!has(validateWorkflow(withManager()), "manager-not-unique"));
});

test("a merge gate may not name the manager as one of its reviewers (#1161)", () => {
  const w = withManager();
  w.gates.merge!.reviewers = ["manager"];
  const f = validateWorkflow(w);
  assert.ok(has(f, "gate-not-a-reviewer"), `a gate naming the manager must be flagged: ${codes(f)}`);
  // And it must say what the manager IS — an author who named it was reaching
  // for "the human signs off", which is real and which this gate cannot
  // express. "that block's kind is manager" would describe a type error.
  assert.match(f.find((x) => x.code === "gate-not-a-reviewer")!.message, /human/);

  // The control: the same document with the real reviewer on the gate is clean.
  assert.ok(!has(validateWorkflow(withManager()), "gate-not-a-reviewer"));
});

test("a manager block may not declare allow: (#1161 D1)", () => {
  // The pane's mirror of the engine's D1 refusal. `allowDenialReason` is the
  // one predicate the form and the validation pass share, so they cannot
  // disagree about which kinds may pre-approve a tool.
  assert.ok(allowDenialReason("manager"));
  const w = withManager();
  w.blocks[3]!.allow = ["Bash(gh pr merge *)"];
  const f = validateWorkflow(w);
  assert.ok(has(f, "allow-not-permitted"), `${codes(f)}`);
  assert.equal(f.find((x) => x.code === "allow-not-permitted")!.blockId, "manager");

  // The control: a reviewer with the same pattern keeps it — a reviewer has its
  // shell by design, so the refusal above is about the class.
  assert.equal(allowDenialReason("reviewer"), null);
});

test("a loomux-owned block may not declare a persona (#1161 D1, review N5)", () => {
  // The pane's mirror of `persona_allowed` / `parse_workflow`'s refusal. The
  // engine fails the WHOLE FILE over this, so a pane reporting it clean lets an
  // author save a workflow that silently launches on the built-in roster with
  // no finding to explain where their roster went.
  for (const kind of ["manager", "orchestrator"] as const) {
    for (const key of ["prompt", "profile"] as const) {
      const w = withManager();
      const i = w.blocks.findIndex((b) => b.kind === "manager");
      w.blocks[i]! = { ...w.blocks[i]!, kind, [key]: key === "prompt" ? "Say it is fine." : ".github/agents/x.md" };
      const f = validateWorkflow(w);
      assert.ok(has(f, "persona-not-permitted"), `${key} on a ${kind} block: ${codes(f)}`);
      assert.match(f.find((x) => x.code === "persona-not-permitted")!.message, new RegExp(key));
    }
  }

  // The controls, and they are what keep this from being "no block may carry a
  // persona". A reviewer's persona is the entire point of the workflow feature,
  // and a PLANNER's is the pairing that matters most: a planner may carry one
  // while being denied `allow:`, so the two rules are not co-extensive and the
  // predicates must stay separate.
  assert.equal(personaDenialReason("reviewer"), null);
  assert.equal(personaDenialReason("planner"), null);
  assert.equal(allowDenialReason("planner") === null, false, "…but a planner still may not allow:");
  const ok = withManager();
  ok.blocks[2]! = { ...ok.blocks[2]!, prompt: "Review for security." }; // the reviewer
  assert.ok(!has(validateWorkflow(ok), "persona-not-permitted"));
});

// ---------- remote blocks (#1457) ----------

/** One block, with whatever keys the case under test needs. The refusals are all
 *  per-block, so a one-block roster is the whole fixture. */
const remoteDoc = (keys: Record<string, string>): string =>
  `version: 1
blocks:
  - id: b
` +
  Object.entries(keys)
    .map(([k, v]) => `    ${k}: ${v}
`)
    .join("");

/** Labels the ENGINE refuses, and why — the pane's `isRemoteLabel` mirrors
 *  `pathseg::check_segment`, and its Rust twin
 *  (`a_remote_label_is_refused_rather_than_rewritten`) walks this same list. Keep
 *  the two in step: a mirror nobody compares is a mirror that has drifted. */
const BAD_LABELS: [string, string][] = [
  ["", "empty"],
  ["build box", "a space"],
  ["build.box", "a dot — so '..' is unspellable"],
  ['"../buildbox"', "a traversal, which sanitize_id would REWRITE to buildbox"],
  ["build/box", "a separator"],
  ['"C:"', "a Windows drive prefix"],
  ["-buildbox", "a leading dash — an OPTION to any command line"],
  ["CON", "a Windows reserved device name"],
  ["b".repeat(REMOTE_LABEL_MAX + 1), "one character over the cap"],
];

test("a remote label is refused, never rewritten (#1457)", () => {
  // The pane's mirror of `pathseg::check_segment`. What makes this the right
  // predicate rather than the id one: `sanitize_id` REWRITES, so "../buildbox"
  // would become "buildbox" — two strings naming one operator binding, which is
  // the hazard #925 consolidated the identifier families to prevent.
  for (const [label, why] of BAD_LABELS) {
    assert.equal(isRemoteLabel(label.replace(/^"|"$/g, "")), false, `${JSON.stringify(label)} (${why}) must be refused`);
    const f = analyzeWorkflow(remoteDoc({ kind: "worker", cli: "claude", remote: label || '""' })).findings;
    assert.ok(has(f, "remote-invalid-label"), `${why}: ${codes(f)}`);
  }
  // The positive control, without which every assertion above would pass just as
  // well against a predicate that refused everything.
  assert.equal(isRemoteLabel("buildbox"), true);
  assert.equal(isRemoteLabel("build-box_2"), true);
  assert.equal(isRemoteLabel("b".repeat(REMOTE_LABEL_MAX)), true, "the cap itself is legal");
  assert.ok(
    !has(analyzeWorkflow(remoteDoc({ kind: "worker", cli: "claude", remote: "buildbox" })).findings, "remote-invalid-label")
  );
});

test("a loomux-owned block may not run remotely (#1457)", () => {
  // The mirror of `parse_workflow`'s refusal, and the same two classes
  // `personaDenialReason` answers for — for a related but distinct reason:
  // these two are load-bearing LOCALLY. The orchestrator holds orchestration
  // state, the gh operations and the merge gate; the manager pane is what the
  // human types into.
  for (const kind of ["orchestrator", "manager"] as const) {
    assert.ok(remoteDenialReason(kind), `${kind} must state a reason`);
    const f = analyzeWorkflow(remoteDoc({ kind, cli: "claude", remote: "buildbox" })).findings;
    assert.ok(has(f, "remote-not-permitted"), `${kind}: ${codes(f)}`);
    assert.equal(f.find((x) => x.code === "remote-not-permitted")!.blockId, "b");
  }
  // The controls — and they are what keep this from being "no block may run
  // remotely". Every class the orchestrator SPAWNS may.
  for (const kind of ["worker", "reviewer", "planner"] as const) {
    assert.equal(remoteDenialReason(kind), null);
    const f = analyzeWorkflow(remoteDoc({ kind, cli: "claude", remote: "buildbox" })).findings;
    assert.ok(!has(f, "remote-not-permitted"), `${kind}: ${codes(f)}`);
  }
});

test("a remote block must SPELL OUT cli: claude (#1457)", () => {
  // Claude is the only CLI orrerix drives remotely. Session identity is what
  // made that true originally — orrerix pre-mints the id and claude accepts it,
  // while copilot/gemini/opencode recognize a session by scanning a LOCAL store
  // — but pi accepts a pre-minted id too (#2126), so the refusal is worded from
  // what orrerix drives rather than from what a CLI can accept. pi is in the
  // loop below as a REFUSED cli for exactly that reason. An OMITTED cli:
  // inherits the group default — picked at launch, unknowable to a parser — so
  // it is refused rather than parsed into a promise the spawn would have to
  // break.
  // codex joined with #2515 C1, for the ORDINARY reason — it recognizes a
  // session by scanning a local store — so it is the plainest member of this
  // loop, and it is here to keep the pane's mirror total as WORKFLOW_CLIS grows
  // rather than because anything about it is special.
  for (const cli of ["copilot", "gemini", "opencode", "pi", "codex"]) {
    const f = analyzeWorkflow(remoteDoc({ kind: "worker", cli, remote: "buildbox" })).findings;
    assert.ok(has(f, "remote-requires-claude"), `${cli}: ${codes(f)}`);
  }
  const omitted = analyzeWorkflow(remoteDoc({ kind: "worker", remote: "buildbox" })).findings;
  assert.ok(has(omitted, "remote-requires-claude"), `an omitted cli: ${codes(omitted)}`);
  // …and the message has to say WHICH of the two mistakes it is, because the fix
  // differs: change the CLI, or write the line you left out.
  assert.match(omitted.find((x) => x.code === "remote-requires-claude")!.message, /inherits the group default/);

  // The control.
  assert.ok(
    !has(analyzeWorkflow(remoteDoc({ kind: "worker", cli: "claude", remote: "buildbox" })).findings, "remote-requires-claude")
  );
});

test("a block that names no remote is a local block, byte for byte (#1457)", () => {
  // The migration guarantee: absent is `undefined`, not "", and a file that
  // never mentioned the key serializes exactly as it did before it existed.
  const before =
    "version: 1" + "\n" + "blocks:" + "\n" + "  - id: b" + "\n" +
    "    name: b" + "\n" + "    kind: worker" + "\n" + "    cli: claude" + "\n";
  const w = parseWorkflow(before).workflow;
  assert.equal(w.blocks[0]!.remote, undefined);
  assert.ok(!serializeWorkflow(w).includes("remote"));

  // …and a declared one survives a round-trip as WRITTEN — the label is never
  // normalized, so what the pane shows and what the engine reads are one string.
  const withRemote = parseWorkflow(
    before.replace(
      "    cli: claude" + "\n",
      "    cli: claude" + "\n" + "    remote: buildbox" + "\n"
    )
  ).workflow;
  assert.equal(withRemote.blocks[0]!.remote, "buildbox");
  assert.equal(parseWorkflow(serializeWorkflow(withRemote)).workflow.blocks[0]!.remote, "buildbox");
});

test("a bare remote: is the absent key, an empty one is a refusal (#1457)", () => {
  // Found in self-review, not by a test: the pane's YAML subset gives `null` for
  // a bare `remote:` line and `""` for `remote: ""`, and the engine treats those
  // two DIFFERENTLY — null is `Option<String>` None (a local block, file loads),
  // `""` is `Some("")` and is refused. The `?? ""` idiom every neighbouring field
  // uses collapses them, and the pane then paints a file red that the engine
  // loads. The engine's half of the pair is pinned by
  // `a_bare_remote_key_is_not_a_remote_block`.
  const bare = remoteDoc({ kind: "worker", cli: "claude", remote: "" });
  assert.match(bare, /remote: *\r?\n/, "the fixture must really be a BARE key, not an empty string");
  const bareParsed = parseWorkflow(bare).workflow;
  assert.equal(bareParsed.blocks[0]!.remote, undefined, "a null is the absent key, not an empty label");
  assert.deepEqual(analyzeWorkflow(bare).findings.map((f) => f.code), [], `${codes(analyzeWorkflow(bare).findings)}`);
  assert.ok(!serializeWorkflow(bareParsed).includes("remote"), "and a save does not invent a value for it");

  // The other half, and the control that keeps the above from reading "an empty
  // remote is always fine": written OUT as an empty string, it is a refusal.
  const empty = remoteDoc({ kind: "worker", cli: "claude", remote: '""' });
  assert.ok(has(analyzeWorkflow(empty).findings, "remote-invalid-label"), `${codes(analyzeWorkflow(empty).findings)}`);
});

test("the pane's YAML subset knows two of YAML's null spellings, and that gap is pinned (#1457 review N2)", () => {
  // A DISCLOSED RESIDUAL, pinned rather than described. `plainScalar`
  // (src/workflowparse.ts) resolves `null` and `~` and nothing else, so
  // `remote: Null` reads back as the LABEL "Null" while the engine — whose YAML
  // reader follows the full core schema — sees a null and therefore no remote at
  // all. The fix commit's rationale says "a null is treated as the absent key it
  // means", which is true for the two spellings below and not for the others;
  // this test is what stops that sentence going false in silence.
  //
  // NOT fixed here, deliberately. The gap belongs to the pane's shared YAML
  // subset, not to this key: `plainScalar` and `emitScalar` are symmetric about
  // exactly these two spellings, and widening the reader alone would break the
  // round-trip for every field (a string "Null" would emit unquoted and re-read
  // as null). Widening both is a change to every key in the file, which is not
  // R1's to make.
  //
  // THE BOUND, which is what keeps this a display gap rather than a spawn one:
  // the pane's model never reaches a spawn. `parse_workflow` is the only reader
  // that decides what runs, and `Block.remote` is written there and read by
  // nobody else in this build. So the worst this can do today is show a remote
  // block where the engine sees a local one — in the editor, to the human
  // holding the file. `a_remote_label_survives_a_group_json_round_trip` and the
  // engine's own null-spelling pin are the other half of this pair.
  const at = (spelling: string) =>
    parseWorkflow(remoteDoc({ kind: "worker", cli: "claude", remote: spelling })).workflow.blocks[0]!.remote;

  // Recognised: the pane agrees with the engine.
  assert.equal(at("null"), undefined, "lowercase null is the absent key");
  assert.equal(at("~"), undefined, "~ is the absent key");

  // NOT recognised: the pane reads a label where the engine reads a null. This
  // is the blind spot itself, asserted so that closing it reddens here and the
  // disclosure above gets revisited rather than quietly becoming wrong.
  assert.equal(at("Null"), "Null", "the known gap: capitalised Null is read as a LABEL");
  assert.equal(at("NULL"), "NULL", "the known gap: NULL is read as a LABEL");

  // …and while it is read as a label it is a VALID one, so the pane reports the
  // file clean. That is the whole shape of the divergence in one assertion.
  assert.deepEqual(
    analyzeWorkflow(remoteDoc({ kind: "worker", cli: "claude", remote: "Null" })).findings.map((f) => f.code),
    [],
    "the pane reports it clean, as a remote block"
  );
});

test("a host-shaped key is an unknown key, not a field (#1457)", () => {
  // The repo file SELECTS a label; the operator authors the address. A pane that
  // read `host:` as a field would be offering to author what the engine refuses
  // — and the engine's refusal is whole-file, so this is not a cosmetic split.
  for (const key of ["host", "destination", "port", "user", "identity_file", "ssh_options", "proxy_command", "extra_args"]) {
    const doc = remoteDoc({ kind: "worker", cli: "claude", [key]: "example.com" });
    const workflow = parseWorkflow(doc).workflow;
    const findings = analyzeWorkflow(doc).findings;
    assert.ok(has(findings, "unknown-key"), `${key}: ${codes(findings)}`);
    assert.deepEqual(Object.keys(workflow.blocks[0]!.extra ?? {}), [key], `${key} must land in the unknown-key bag`);
  }
});

test("the manager never reviews, so it can never satisfy a gate (#1161)", () => {
  // `isReviewingBlock` is what the gate's reviewer checkboxes offer and what
  // switching the gate on fills in. A manager answering true there would make
  // the editor author, in one keystroke, the exact file the test above flags.
  assert.equal(isReviewingBlock({ kind: "manager" }), false);
  assert.equal(isReviewingBlock({ kind: "reviewer" }), true, "the non-vacuity control");
});

test("isReviewingBlock separates the blocks that review from the class they ride (#891)", () => {
  // The pane's mirror of the backend's `is_reviewing_block`. It is what the
  // merge-gate reviewer list offers and what switching the gate ON fills in —
  // and the pairing that matters is the last two: a liaison is reviewer-KIND,
  // so a `kind` filter cannot tell them apart and the editor would author a
  // file `validateWorkflow` flags `gate-not-a-reviewer` on the same keystroke.
  assert.equal(isReviewingBlock({ kind: "reviewer" }), true);
  assert.equal(isReviewingBlock({ kind: "worker" }), false);
  assert.equal(isReviewingBlock({ kind: "reviewer", role_hint: "process" }), true,
    "another hint on a reviewer subtracts nothing — only the liaison does");
  assert.equal(isReviewingBlock({ kind: "reviewer", role_hint: "liaison" }), false);
  // Trimmed and case-folded, like `roleHintRequires` and the backend parser: a
  // file the real engine reads as a liaison must not read as a reviewer here.
  assert.equal(isReviewingBlock({ kind: "reviewer", role_hint: " Liaison " }), false);

  // The property the two call sites depend on, asserted directly rather than
  // through the DOM they live in: filtering a real roster leaves exactly the
  // blocks a merge gate may name.
  const w = starterWorkflow();
  w.blocks[2]!.role_hint = "liaison";
  const gateable = w.blocks.filter(isReviewingBlock).map((b) => b.id);
  assert.ok(!gateable.includes(w.blocks[2]!.id), `the liaison must not be offered: ${gateable}`);
  assert.deepEqual(
    validateWorkflow({ ...w, gates: { merge: { require: "all-pass", reviewers: gateable, also: [] } } })
      .filter((f) => f.code === "gate-not-a-reviewer"),
    [],
    "a gate filled from this predicate must validate clean — that is the whole point of it"
  );
});

test("role_hint round-trips through serialize/parse unchanged", () => {
  const w = starterWorkflow();
  w.blocks[0]!.role_hint = "advisor";
  const reread = parseWorkflow(serializeWorkflow(w)).workflow;
  assert.equal(reread.blocks[0]!.role_hint, "advisor");
  // ...and a block that never declared one stays undefined, not "".
  assert.equal(reread.blocks[1]!.role_hint, undefined);
  // Formatting twice is still a no-op with the field present.
  assert.equal(serializeWorkflow(reread), serializeWorkflow(w));
});

test("a block nothing wires up is a warning, not a hard error", () => {
  const w = starterWorkflow();
  w.blocks.push({ id: "rev-perf", name: "Perf", kind: "reviewer", cli: "claude", model: "" });
  const f = validateWorkflow(w);
  const isolated = f.find((x) => x.code === "isolated-block");
  assert.ok(isolated, "a reviewer nobody points at will never be asked to review");
  assert.equal(isolated!.severity, "warning", "edges are advisory — this must not block a run");
  assert.equal(isolated!.blockId, "rev-perf");
  assert.equal(hasErrors(f), false);
});

test("unreachable and entry-less graphs are reported; a rework loop is not", () => {
  // A block only reachable through a cycle it isn't part of an entry for.
  const stranded = starterWorkflow();
  stranded.blocks.push({ id: "rev-2", name: "R2", kind: "reviewer", cli: "claude", model: "" });
  stranded.blocks.push({ id: "rev-3", name: "R3", kind: "reviewer", cli: "claude", model: "" });
  stranded.edges.push({ from: "rev-2", to: "rev-3" }, { from: "rev-3", to: "rev-2" });
  assert.ok(has(validateWorkflow(stranded), "unreachable-block"));

  // The worker ⇄ reviewer REWORK LOOP is how loomux actually works — a cycle must not
  // be a finding on its own.
  const loop = starterWorkflow();
  loop.edges.push({ from: "reviewer", to: "worker" });
  const f = validateWorkflow(loop);
  assert.deepEqual(codes(f), [], "the rework loop is legitimate, not a defect");

  // But a graph where EVERY block is pointed at has nowhere to start.
  const closed: Workflow = {
    version: 1,
    name: "",
    blocks: [
      { id: "a", name: "A", kind: "worker", cli: "claude", model: "" },
      { id: "b", name: "B", kind: "reviewer", cli: "claude", model: "" },
    ],
    edges: [
      { from: "a", to: "b" },
      { from: "b", to: "a" },
    ],
    gates: {},
  };
  assert.ok(has(validateWorkflow(closed), "no-entry-block"));
});

test("the version is checked before anything else trusts the shape", () => {
  assert.ok(has(parseWorkflow("blocks: []").findings, "version-missing"));
  assert.ok(has(parseWorkflow(`version: ${WORKFLOW_VERSION + 1}\nblocks: []`).findings, "version-unsupported"));
});

// ---------- model knobs: effort / context (#687) ----------

/** The `agent_cli_knobs` replies the pane fetches per CLI, verbatim from
 *  `CLI_CAPS` (crates/loomux-engine/src/model.rs) — the pane never mirrors a
 *  vendor fact, it asks. */
const KNOBS: Record<string, CliKnobs> = {
  claude: {
    cli: "claude",
    known: true,
    effort: { values: ["low", "medium", "high", "xhigh", "max"], note: "--effort <level> is a session-scoped flag" },
    context: { values: ["1m"], note: "the [1m] model-alias suffix (sonnet[1m])" },
  },
  copilot: {
    cli: "copilot",
    known: true,
    effort: { values: [], note: "copilot reads effortLevel from ~/.copilot/settings.json" },
    context: { values: [], note: "copilot's context window is an interactive-only control (/context)" },
  },
};

/** The lookup the pane hands to `validateWorkflow`: what has been fetched so
 *  far, and `null` for a CLI whose reply hasn't landed. */
const lookup = (cli: string, model: string) =>
  KNOBS[cli] ? knobState(KNOBS[cli]!, cli, model) : null;

test("effort:/context: are real keys the pane reads, not unknown ones it merely keeps", () => {
  // Before #687 these landed in `extra` — preserved, but invisible to the form and
  // to validation. The engine parses them, so the pane must too.
  const { workflow, findings } = parseWorkflow(`version: 1
blocks:
  - id: worker
    name: Worker
    kind: worker
    cli: claude
    model: opus
    effort: xhigh
    context: 1m
`);
  assert.deepEqual(
    findings.map((f) => f.code),
    []
  );
  const b = workflow.blocks[0]!;
  assert.equal(b.effort, "xhigh");
  assert.equal(b.context, "1m");
  assert.equal(b.extra, undefined, "a known key must not ALSO be carried as an unknown one");
});

test("the knobs round-trip through serialize/parse, and a block without them stays clean", () => {
  const w = starterWorkflow();
  w.blocks[1]!.effort = "max"; // blocks[1] is the worker
  w.blocks[1]!.context = "1m";
  const text = serializeWorkflow(w);
  assert.match(text, /^ {4}effort: max$/m);
  assert.match(text, /^ {4}context: 1m$/m);
  const reread = parseWorkflow(text).workflow;
  assert.equal(reread.blocks[1]!.effort, "max");
  assert.equal(reread.blocks[1]!.context, "1m");
  // A block that declared neither stays undefined, not "" — the same distinction
  // role_hint keeps, and what makes "absent = the CLI's default" survive a save.
  assert.equal(reread.blocks[0]!.effort, undefined);
  assert.equal(reread.blocks[0]!.context, undefined);
  // Serializing twice is still a no-op with the fields present.
  assert.equal(serializeWorkflow(reread), serializeWorkflow(w));
  // ...and a file that pins nothing is unchanged, byte for byte.
  assert.equal(serializeWorkflow(starterWorkflow()), serializeWorkflow(parseWorkflow(serializeWorkflow(starterWorkflow())).workflow));
});

test("a knob the block's CLI cannot honor is a finding quoting that CLI's own reason", () => {
  // The engine REFUSES this file (`validate_knob`, workflow.rs). A pane that
  // reported it clean would send the human to a launch that falls back to the
  // built-in roster with no idea why.
  const w = starterWorkflow();
  w.blocks[1]!.cli = "copilot";
  w.blocks[1]!.effort = "xhigh";
  const f = validateWorkflow(w, lookup);
  assert.ok(has(f, "knob-unavailable"));
  const finding = f.find((x) => x.code === "knob-unavailable")!;
  assert.equal(finding.blockId, "worker");
  assert.equal(finding.severity, "error");
  assert.match(finding.message, /effort/);
  assert.match(finding.message, /~\/\.copilot\/settings\.json/, "the vendor's reason, not 'unsupported'");
});

test("a value outside the CLI's own vocabulary is a finding that names the vocabulary", () => {
  const w = starterWorkflow();
  w.blocks[1]!.effort = "banana";
  const f = validateWorkflow(w, lookup);
  assert.ok(has(f, "knob-unavailable"));
  assert.match(f.find((x) => x.code === "knob-unavailable")!.message, /low, medium, high, xhigh, max/);
  // The legal values are clean, and so is an empty one (= the CLI's default).
  for (const level of ["low", "medium", "high", "xhigh", "max", ""]) {
    const ok = starterWorkflow();
    ok.blocks[1]!.effort = level;
    assert.deepEqual(codes(validateWorkflow(ok, lookup)), [], `effort: ${level || "(empty)"} is legal`);
  }
});

test("context: is gated on the MODEL, not just the CLI (#709 carried finding)", () => {
  // `model: haiku` + `context: 1m` composes `--model haiku[1m]` — an alias the
  // vendor docs do not define (model-config §Extended context lists Fable 5,
  // Sonnet 5, Opus 4.6+ and Sonnet 4.6). The launcher can't produce it (the
  // control is disabled); a hand-written file can, and this is where that is
  // caught, at the surface a human authors it in.
  const w = starterWorkflow();
  w.blocks[1]!.model = "haiku";
  w.blocks[1]!.context = "1m";
  const f = validateWorkflow(w, lookup);
  assert.ok(has(f, "knob-unavailable"));
  assert.match(f.find((x) => x.code === "knob-unavailable")!.message, /haiku\[1m\]/);

  // The same block on a 1M-capable model is clean...
  const ok = starterWorkflow();
  ok.blocks[1]!.model = "sonnet";
  ok.blocks[1]!.context = "1m";
  assert.deepEqual(codes(validateWorkflow(ok, lookup)), []);
  // ...and effort is NOT gated with it: a model that lacks a level falls back to
  // the highest it supports (model-config §Adjust effort level).
  const effortOnHaiku = starterWorkflow();
  effortOnHaiku.blocks[1]!.model = "haiku";
  effortOnHaiku.blocks[1]!.effort = "max";
  assert.deepEqual(codes(validateWorkflow(effortOnHaiku, lookup)), []);
});

test("with no capability data the knob checks DEFER — they never guess", () => {
  // The pane fetches caps asynchronously, and a block may name a CLI it hasn't
  // fetched. Same deferral the real parser makes for a block with no explicit
  // `cli:` (workflow.rs): check what you can know here, and let the layer that
  // knows the resolved CLI check the rest.
  const w = starterWorkflow();
  w.blocks[1]!.cli = "copilot";
  w.blocks[1]!.effort = "xhigh";
  assert.deepEqual(codes(validateWorkflow(w)), [], "no caps in hand = no finding invented");
  assert.deepEqual(
    codes(validateWorkflow(w, (cli, model) => (cli === "claude" ? knobState(KNOBS.claude!, cli, model) : null))),
    [],
    "caps for the OTHER cli in hand is still no answer about this one"
  );
  // And a file that pins no knobs is unaffected whether caps are present or not.
  assert.deepEqual(codes(validateWorkflow(starterWorkflow(), lookup)), []);
});

// ---------- opencode as a spawnable block cli (#722) ----------

/** opencode's `agent_cli_knobs` reply, verbatim from `CLI_CAPS`
 *  (crates/loomux-engine/src/model.rs). A
 *  hand-copied literal, like every other fixture here — the note text's fidelity
 *  to the Rust source (including the `--variant` / `model-determined` substrings
 *  the assertions below match on) is pinned by `selectorknobs.test.ts`' last
 *  test, which reads `CLI_CAPS` back. What THIS file asks of it is only that a
 *  finding quotes the CLI's own reason rather than saying "unsupported". */
const OPENCODE_KNOBS: CliKnobs = {
  cli: "opencode",
  known: true,
  effort: {
    values: [],
    note: "opencode's reasoning effort is a model VARIANT: a session flag on `opencode run` (--variant) but absent from the TUI loomux spawns, and settable per-agent in loomux's generated config (agent.<name>.variant, observed values minimal|high|max) — the seam exists, but the per-model vocabulary is provider-specific and unverified against a live run, so loomux does not write it yet",
  },
  context: {
    values: [],
    note: "opencode's context window is model-determined; no session-scoped variant switch is documented or present in the TUI's options",
  },
};

const lookupWithOpencode = (cli: string, model: string) =>
  cli === "opencode" ? knobState(OPENCODE_KNOBS, cli, model) : lookup(cli, model);

test("a block may run cli: codex as a worker, and the pane does not adjudicate the kinds (#2515 C1)", () => {
  // The backend spawns it (`SUPPORTED_CLIS`), so a pane that flagged the file
  // would send a human to fix a file that is already correct — and the block
  // editor would not even offer the CLI in its dropdown.
  assert.ok(isWorkflowCli("codex"));
  const w = starterWorkflow();
  w.blocks[1]!.cli = "codex";
  assert.deepEqual(codes(validateWorkflow(w, lookup)), []);

  // **The load-bearing half.** codex hosts a worker and CANNOT host a reviewer
  // or a planner — its only containment axis is an all-or-nothing
  // `sandbox_mode`. That refusal belongs to the backend's `cli_can_host`, which
  // quotes the measured reason back, and this pane must NOT reproduce it: a
  // second copy of a containment rule is a copy that drifts, and the one that
  // drifts silently is the one in the surface that cannot enforce anything.
  //
  // So a `codex` reviewer is clean HERE and refused at load. Pinned rather
  // than left implicit, because "add a warning for it" is exactly the
  // plausible-looking change this comment exists to argue against.
  const rev = starterWorkflow();
  rev.blocks[2]!.cli = "codex";
  assert.equal(rev.blocks[2]!.kind, "reviewer", "this test needs a reviewer block to be about");
  assert.deepEqual(
    codes(validateWorkflow(rev, lookup)),
    [],
    "membership in WORKFLOW_CLIS is spawnability; which KINDS a CLI can host is cli_can_host's, " +
      "and duplicating it here would be a rule that drifts"
  );
});

test("a block may run cli: opencode, model and all (#722)", () => {
  // The backend spawns it (`SUPPORTED_CLIS`), so a pane that flagged the file
  // would send a human to fix a file that is already correct — and the pane's
  // block editor would not even offer the CLI in its dropdown.
  assert.ok(isWorkflowCli("opencode"));
  const w = starterWorkflow();
  w.blocks[1]!.cli = "opencode";
  w.blocks[1]!.model = "opencode/deepseek-v4-flash-free";
  assert.deepEqual(codes(validateWorkflow(w, lookupWithOpencode)), []);
  // Through the text the human actually writes, provider `/` and all — parse,
  // validate, serialize, re-read, with the id intact at every step.
  const src = `version: 1
blocks:
  - id: rev-oc
    name: Second opinion
    kind: reviewer
    cli: opencode
    model: opencode/deepseek-v4-flash-free
`;
  const { workflow, findings } = parseWorkflow(src);
  assert.deepEqual(
    findings.map((f) => f.code),
    []
  );
  assert.equal(workflow.blocks[0]!.cli, "opencode");
  assert.equal(workflow.blocks[0]!.model, "opencode/deepseek-v4-flash-free");
  const reread = parseWorkflow(serializeWorkflow(workflow)).workflow;
  assert.equal(reread.blocks[0]!.model, "opencode/deepseek-v4-flash-free", "the provider prefix must survive a save");
  // And the roster still rejects what the backend cannot spawn — widening the
  // list must not have turned the check into a rubber stamp.
  const bad = starterWorkflow();
  bad.blocks[1]!.cli = "opencodex";
  assert.ok(has(validateWorkflow(bad), "unknown-cli"));
});

test("an opencode block declaring effort: is a finding quoting opencode's own reason (#722)", () => {
  // The real engine refuses it (`validate_knob`), because opencode's TUI has no
  // variant flag. The pane has to say the same thing, in opencode's words — and
  // must NOT quietly accept a level it cannot deliver just because the CLI is
  // newly spawnable.
  const w = starterWorkflow();
  w.blocks[1]!.cli = "opencode";
  w.blocks[1]!.effort = "high";
  const f = validateWorkflow(w, lookupWithOpencode);
  assert.deepEqual(codes(f), ["knob-unavailable"]);
  const finding = f.find((x) => x.code === "knob-unavailable")!;
  assert.equal(finding.blockId, "worker");
  assert.equal(finding.severity, "error");
  assert.match(finding.message, /--variant/, "the vendor's reason, not 'unsupported'");
  // Same for context, and a block that pins neither stays clean.
  const ctx = starterWorkflow();
  ctx.blocks[1]!.cli = "opencode";
  ctx.blocks[1]!.context = "1m";
  assert.match(
    validateWorkflow(ctx, lookupWithOpencode).find((x) => x.code === "knob-unavailable")!.message,
    /model-determined/
  );
  const clean = starterWorkflow();
  clean.blocks[1]!.cli = "opencode";
  assert.deepEqual(codes(validateWorkflow(clean, lookupWithOpencode)), []);
});

// ---------- the full config surface: intake / merge_queue / resources / allow (#880) ----------
//
// `intake:` (#382), `merge_queue:` (#581), `resources:` (#858) and block `allow:` (#222)
// are all real fields of the engine's wire schema that this model had no name for — they
// landed in the unknown-key bag, so the pane rendered a file that declared them exactly
// like a file that didn't, and a form edit re-emitted them as one flattened flow mapping.
// These pin the whole surface: read into the model, emitted back, and — the part that
// matters most for a hand-written file — left alone when the edit was somewhere else.

const FULL_SURFACE = `# the whole schema, commented
version: 1
name: everything
authored_with: 0.9.9

# WHERE WORK COMES FROM (#382)
intake:
  source: github-labels
  labels:
    ready: go-build
    investigate: go-look

# THE QUEUE (#581)
merge_queue:
  enabled: true
  max_batch: 5
  checks_timeout_minutes: 90

blocks:
  - id: worker
    name: Worker
    kind: worker
    cli: claude
    allow: ["Bash(gh pr view --json title,body)"]

  - id: rev
    name: Reviewer
    kind: reviewer
    cli: claude

edges:
  - { from: worker, to: rev }

gates:
  merge:
    require: all-pass
    reviewers: [rev]

# WHAT AGENTS TAKE TURNS ON (#858)
resources:
  build:
    slots: 1
    max_hold_minutes: 45
`;

test("every section of the wire schema reads into the model — none of them is an unknown key (#880)", () => {
  const { workflow, findings } = analyzeWorkflow(FULL_SURFACE);
  assert.deepEqual(codes(findings), [], "a file using the documented schema must be clean");
  assert.equal(workflow.authored_with, "0.9.9");
  assert.deepEqual(workflow.intake, {
    source: "github-labels",
    labels: { ready: "go-build", investigate: "go-look" },
  });
  assert.deepEqual(workflow.merge_queue, {
    enabled: true,
    max_batch: 5,
    checks_timeout_minutes: 90,
  });
  assert.deepEqual(workflow.resources, { build: { slots: 1, max_hold_minutes: 45 } });
  // The comma lives INSIDE the quoted pattern: one entry, not two. `allow:` is the field
  // whose absence from this model was the whole reason the schema manifest exists.
  assert.deepEqual(workflow.blocks[0]!.allow, ["Bash(gh pr view --json title,body)"]);
  assert.equal(workflow.extra, undefined, "nothing at the top level is unknown any more");
  assert.equal(workflow.blocks[0]!.extra, undefined);
  // Absent stays absent: an undeclared label must not be filled in with its built-in
  // default, or the next save silently PINS what the file meant to inherit.
  assert.equal(workflow.intake!.labels!.owned, undefined);
});

test("the whole surface survives a canonical round-trip, twice", () => {
  const { workflow } = parseWorkflow(FULL_SURFACE);
  const out = serializeWorkflow(workflow);
  const reread = parseWorkflow(out);
  assert.deepEqual(reread.findings, []);
  assert.deepEqual(reread.workflow, workflow, "a Format must not drop a section it can read");
  assert.equal(serializeWorkflow(reread.workflow), out, "…and must be idempotent");
});

test("a commented file with intake: and merge_queue: survives an unrelated block edit byte-for-byte outside the edited block (#880)", () => {
  const { workflow } = parseWorkflow(FULL_SURFACE);
  const edited: Workflow = {
    ...workflow,
    blocks: workflow.blocks.map((b) => (b.id === "rev" ? { ...b, model: "opus" } : b)),
  };
  const out = serializeWorkflowPreserving(edited, FULL_SURFACE);
  assert.deepEqual(parseWorkflow(out).workflow, edited, "the edit itself must round-trip");

  // Cut the ONE item that changed out of both texts; what is left has to be identical —
  // not "still has the comments", identical. That is the claim a human actually cares
  // about when they open a file they hand-wrote and the pane saves it.
  const cutEditedBlock = (text: string): string => {
    const lines = text.split("\n");
    const start = lines.findIndex((l) => l.includes("- id: rev"));
    assert.ok(start > 0, "the fixture must contain the block being edited");
    let end = start + 1;
    while (end < lines.length && !/^\S/.test(lines[end]!)) end++;
    return [...lines.slice(0, start), ...lines.slice(end)].join("\n");
  };
  assert.equal(
    cutEditedBlock(out),
    cutEditedBlock(FULL_SURFACE),
    "everything outside the edited block must be the ORIGINAL text, byte for byte"
  );
});

test("editing intake: rewrites intake: and nothing else, and no section is relocated (#880)", () => {
  const { workflow } = parseWorkflow(FULL_SURFACE);
  const edited: Workflow = {
    ...workflow,
    intake: { ...workflow.intake, labels: { ...workflow.intake!.labels, ready: "ready-now" } },
  };
  const out = serializeWorkflowPreserving(edited, FULL_SURFACE);
  assert.deepEqual(parseWorkflow(out).workflow, edited);
  assert.match(out, /ready: ready-now/);
  // The comment ABOVE a section is about the section, not about the field that changed.
  assert.match(out, /# WHERE WORK COMES FROM \(#382\)/);
  assert.match(out, /# THE QUEUE \(#581\)/);
  assert.match(out, /# WHAT AGENTS TAKE TURNS ON \(#858\)/);
  assert.match(out, /# the whole schema, commented/);
  // And the file's own ORDER is kept — this fixture, like the repo's own workflow, writes
  // merge_queue: above blocks:, and an edit must not move it to the bottom.
  assert.ok(
    out.indexOf("merge_queue:") < out.indexOf("blocks:"),
    `merge_queue: must stay where the file put it:\n${out}`
  );
});

test("a typed section keeps the position the file gave it when an unrelated block is edited (#880)", () => {
  const text = `version: 1

# the queue, declared FIRST because that is the order this file reads in
merge_queue:
  enabled: true

blocks:
  - id: w
    name: W
    kind: worker
    cli: claude
`;
  const { workflow } = parseWorkflow(text);
  assert.deepEqual(workflow.merge_queue, { enabled: true }, "read as a section, not an unknown key");
  const edited: Workflow = { ...workflow, blocks: [{ ...workflow.blocks[0]!, model: "opus" }] };
  const out = serializeWorkflowPreserving(edited, text);
  assert.ok(
    out.indexOf("merge_queue:") < out.indexOf("blocks:"),
    `a section is written where the FILE wrote it, not where the emitter would have:\n${out}`
  );
  assert.match(out, /# the queue, declared FIRST/);
  assert.deepEqual(parseWorkflow(out).workflow, edited);
});

test("an unknown key is an error finding — and is still preserved verbatim (#880)", () => {
  const text = `version: 1
promt: whoops
blocks:
  - id: w
    name: W
    kind: worker
    cli: claude
    retires: 3
`;
  const { workflow, findings } = analyzeWorkflow(text);
  const unknown = findings.filter((f) => f.code === "unknown-key");
  assert.equal(unknown.length, 2, "one per key, wherever it sits");
  assert.ok(unknown.every((f) => f.severity === "error"));
  assert.match(unknown[0]!.message, /"promt"/);
  assert.match(
    unknown[0]!.message,
    /will not load/,
    "the finding must say what actually happens: this build's engine refuses the WHOLE file"
  );
  assert.equal(unknown[1]!.blockId, "w", "a block's unknown key is shown next to the block");

  // Warned, never deleted. A newer loomux may have written the key, and silently dropping
  // a human's line to make a warning go away is worse than the warning.
  assert.equal(serializeWorkflowPreserving(workflow, text), text);
  const canonical = serializeWorkflow(workflow);
  assert.match(canonical, /promt: whoops/);
  assert.match(canonical, /retires: 3/);
});

test("a gate loomux does not enforce is NOT an unknown key — gates: is a map, not a struct", () => {
  // The engine reads `gates:` as `BTreeMap<String, RawGate>`, so a gate it has no
  // machinery for still parses; only `merge` is enforced. Reporting it would be the pane
  // inventing a refusal the engine never makes — the mirror image of the failure
  // `unknown-key` exists for.
  const text = `version: 1
blocks:
  - id: w
    name: W
    kind: worker
    cli: claude
gates:
  release:
    require: all-pass
    reviewers: [w]
`;
  const { findings } = analyzeWorkflow(text);
  assert.deepEqual(codes(findings).filter((c) => c === "unknown-key"), []);
});

test("a section written as something other than a mapping is a finding, not a silent drop", () => {
  const { findings } = analyzeWorkflow(`version: 1
intake: nope
merge_queue: 3
blocks:
  - id: w
    name: W
    kind: worker
    cli: claude
`);
  const shape = findings.filter((f) => f.code === "section-not-a-mapping");
  assert.deepEqual(
    shape.map((f) => f.message.split(":")[0]),
    ["intake", "merge_queue"]
  );
});

test("a wrongly-typed field is rejected, never coerced (#880)", () => {
  // The engine's serde refuses each of these outright. A pane that read `soon` as "the
  // default" would be describing a queue policy nobody wrote.
  const { workflow, findings } = analyzeWorkflow(`version: 1
merge_queue:
  enabled: yep
  max_batch: soon
blocks:
  - id: w
    name: W
    kind: worker
    cli: claude
    allow: Bash(git push)
`);
  assert.deepEqual(
    findings.filter((f) => f.code === "section-bad-value").map((f) => f.message.split(":")[0]),
    // Parse order: the roster is read before the policy sections.
    ["blocks[0].allow", "merge_queue.enabled", "merge_queue.max_batch"]
  );
  assert.equal(workflow.merge_queue!.enabled, undefined, "not coerced to false");
  assert.equal(workflow.merge_queue!.max_batch, undefined);
  assert.equal(workflow.blocks[0]!.allow, undefined, "not coerced to a one-item list");
});

test("a section declared EMPTY stays declared — `intake: {}` is not the same as no intake:", () => {
  // A bare `intake:` is YAML null, which the engine's `Option<RawIntake>` reads as absent —
  // so emitting one back would delete a section someone deliberately wrote. `{}` is the
  // spelling that means "declared, all defaults", and it is what this model emits.
  const declared = parseWorkflow(
    "version: 1\nintake: {}\nblocks:\n  - id: w\n    kind: worker\n"
  ).workflow;
  assert.deepEqual(declared.intake, {});
  const out = serializeWorkflow(declared);
  assert.match(out, /^intake: \{\}$/m);
  assert.deepEqual(parseWorkflow(out).workflow.intake, {});

  const absent = parseWorkflow("version: 1\nintake:\nblocks:\n  - id: w\n    kind: worker\n");
  assert.equal(absent.workflow.intake, undefined, "a bare key means the section is not declared");
  assert.doesNotMatch(serializeWorkflow(absent.workflow), /intake/);
});

// ---------- the config surface the PANE can now edit (#1020) ----------
//
// #880 gave the model a name for every field of the wire schema; the pane could still only
// edit half of them. These pin the half that was missing — `allow:`, `role_hint:`, and the
// three policy sections — as RULES rather than as form wiring, because the form is DOM and
// the rules are what a form must not be able to break:
//
//   * what a picker may offer is derived from the same statement that validates it, so a new
//     role hint cannot appear in one and not the other;
//   * a value the engine REFUSES is a finding here (the pane must not bless a file that will
//     not load), and one it silently REWRITES is a warning (the pane must not let a pattern
//     reach an agent as something other than what the file says);
//   * declaring a section and then undeclaring it leaves the file exactly as it was.

const workflowWith = (body: string): string =>
  `version: 1\nname: t\n\nblocks:\n  - id: w\n    name: W\n    kind: worker\n    cli: claude\n${body}`;

test("the role_hint offer is DERIVED from the pairing rule, for every kind (#1020)", () => {
  // The property, over the whole cross product rather than over today's two hints: a hint
  // this function offers for a kind must validate clean on that kind, and one it withholds
  // must be refused. A hardcoded picker passes this today and fails it the day a third hint
  // lands — which is the day it would otherwise start lying.
  for (const kind of BLOCK_KINDS) {
    const offered = roleHintsForKind(kind);
    for (const hint of ROLE_HINTS) {
      const w = parseWorkflow(
        `version: 1\nblocks:\n  - id: b\n    kind: ${kind}\n    cli: claude\n    role_hint: ${hint}\n`
      ).workflow;
      const roleFindings = validateWorkflow(w).filter((f) => f.code.startsWith("role-hint-"));
      if (offered.includes(hint)) {
        // An offered pairing raises no ERROR. It may still raise the one advisory
        // this validator emits for a LEGAL hint — `liaison` is superseded by
        // `kind: manager` (#1161 D4) — so the expectation is exact rather than
        // "no role-hint findings at all": a spurious warning on any other hint
        // still fails here, and so does an advisory that hardens into an error.
        assert.deepEqual(
          codes(roleFindings),
          hint === "liaison" ? ["role-hint-superseded"] : [],
          `${kind} is offered ${hint} — it must validate`
        );
        assert.deepEqual(
          roleFindings.filter((f) => f.severity === "error"),
          [],
          `${kind} is offered ${hint} — an offered pairing is never an error`
        );
      } else {
        assert.deepEqual(
          codes(roleFindings),
          ["role-hint-wrong-kind"],
          `${kind} is not offered ${hint} — the parser must refuse it`
        );
      }
    }
  }
  // …and every hint is offered SOMEWHERE. A hint no kind can carry would be one the form can
  // never spell, which is a different bug from the two above and just as silent.
  for (const hint of ROLE_HINTS) {
    assert.ok(
      BLOCK_KINDS.some((k) => roleHintsForKind(k).includes(hint)),
      `no kind offers ${hint}`
    );
  }
});

test("allow: is refused on the two kinds the engine refuses it on, and only those (#1020)", () => {
  const on = (kind: string): Finding[] =>
    validateWorkflow(
      parseWorkflow(
        `version: 1\nblocks:\n  - id: b\n    kind: ${kind}\n    cli: claude\n    allow: ["Bash(npm test)"]\n`
      ).workflow
    ).filter((f) => f.code === "allow-not-permitted");

  // The trust root: a repo file may not pre-approve the tools of the one agent that runs
  // unsupervised. The read-only class: `allow: Bash(python *)` is a shell that writes files.
  assert.equal(on("orchestrator").length, 1);
  assert.equal(on("planner").length, 1);
  // A reviewer keeps its shell by design (running the tests IS the job) and a worker holds
  // the whole surface anyway — the engine allows both, so the pane must not invent a rule.
  assert.deepEqual(on("worker"), []);
  assert.deepEqual(on("reviewer"), []);
  // An unrecognized kind is already reported as one; a second finding explains nothing.
  assert.deepEqual(on("superuser"), []);
});

test("an allow: pattern the engine would silently rewrite is a warning, not silence (#1020)", () => {
  const { findings } = analyzeWorkflow(
    workflowWith(`    allow: ["Bash(gh pr view --json title,body)", "Bash(echo $HOME)", "$$$"]\n`)
  );
  const sanitized = findings.filter((f) => f.code === "allow-sanitized");
  // The first pattern is clean — commas and parens are in the engine's alphabet — so it must
  // NOT be flagged, or the warning becomes noise on every real file.
  assert.equal(sanitized.length, 2, sanitized.map((f) => f.message).join(" | "));
  assert.match(sanitized[0]!.message, /Bash\(echo HOME\)/, "says what will actually be applied");
  assert.match(sanitized[1]!.message, /dropped/, "a pattern with nothing left is dropped entirely");
  assert.deepEqual(
    sanitized.map((f) => f.severity),
    ["warning", "warning"],
    "the file still loads — the engine rewrites rather than refuses"
  );
  assert.deepEqual(
    sanitized.map((f) => f.blockId),
    ["w", "w"],
    "reported on the block, so the pane can show it in that block's own form"
  );
});

test("sanitizeAllowPattern mirrors the engine's alphabet, both ways (#1020)", () => {
  // Kept, because a real tool pattern needs them: parens, colon, star, dot, slash, comma,
  // interior spaces, dashes, underscores.
  assert.equal(
    sanitizeAllowPattern("Bash(gh pr view --json title,body)"),
    "Bash(gh pr view --json title,body)"
  );
  assert.equal(sanitizeAllowPattern("mcp__orrerix/report:*"), "mcp__orrerix/report:*");
  // Filtered — every one of these reaches the CLI's flag as something else.
  assert.equal(sanitizeAllowPattern('Bash(echo "$X" | tee f)'), "Bash(echo X  tee f)");
  assert.equal(sanitizeAllowPattern("  Bash(ls)  "), "Bash(ls)");
  // Nothing usable left = the entry is dropped, not passed through empty.
  assert.equal(sanitizeAllowPattern("!!!"), null);
  assert.equal(sanitizeAllowPattern("   "), null);
});

test("an intake source the parser refuses is a finding, and the empty one is not (#1020)", () => {
  const source = (v: string): Finding[] =>
    analyzeWorkflow(
      `version: 1\nintake:\n  source: ${v}\nblocks:\n  - id: w\n    kind: worker\n    cli: claude\n`
    ).findings.filter((f) => f.code === "intake-unknown-source");
  for (const ok of INTAKE_SOURCES) assert.deepEqual(source(ok), [], ok);
  // The engine trims and lowercases before matching, so the pane must not disagree.
  assert.deepEqual(source("GitHub-Labels"), []);
  assert.deepEqual(source('""'), [], "an empty source means inherit, which is always legal");
  const bad = source("jira");
  assert.equal(bad.length, 1);
  assert.equal(bad[0]!.section, "intake", "routed to the section that can fix it");
  assert.match(bad[0]!.message, /github-labels/, "names the vocabulary");
});

test("an intake label the parser refuses is a finding — including the leading-dash one (#1020)", () => {
  const { findings } = analyzeWorkflow(`version: 1
intake:
  labels:
    ready: agent-ready
    hold: -force
    owned: "agent managed"
blocks:
  - id: w
    kind: worker
    cli: claude
`);
  const bad = findings.filter((f) => f.code === "intake-bad-label");
  // `-force` is the one that is NOT obvious: the alphabet permits a dash freely, but the hold
  // spelling becomes a positional argument to `gh label create`, where a leading dash is read
  // as a flag. `agent-ready` (an interior dash) must stay clean.
  assert.deepEqual(
    bad.map((f) => f.message.split(":")[0]),
    ["intake.labels.owned", "intake.labels.hold"]
  );
  assert.deepEqual(
    bad.map((f) => f.section),
    ["intake", "intake"]
  );
});

test("a policy number outside the engine's bounds is a finding, not a clean bill of health (#1020)", () => {
  const { findings } = analyzeWorkflow(`version: 1
merge_queue:
  max_batch: 0
  checks_timeout_minutes: 999
resources:
  build:
    slots: 65
  lint:
    max_hold_minutes: 481
  "no good":
    slots: 2
blocks:
  - id: w
    kind: worker
    cli: claude
`);
  const range = findings.filter((f) => f.code === "section-out-of-range");
  assert.deepEqual(
    range.map((f) => f.message.split(":").slice(0, 1).join(":")),
    [
      "merge_queue.max_batch",
      "merge_queue.checks_timeout_minutes",
      "resources.build.slots",
      "resources.lint.max_hold_minutes",
    ]
  );
  // `max_batch: 0` is a REFUSAL on the engine; `checks_timeout_minutes` is CLAMPED. The pane
  // says which is which, because "your file will not load" and "your file will not do what it
  // says" are different sentences.
  assert.deepEqual(
    range.map((f) => f.severity),
    ["error", "warning", "error", "error"]
  );
  assert.deepEqual(
    range.map((f) => f.section),
    ["merge_queue", "merge_queue", "resources", "resources"]
  );
  const names = findings.filter((f) => f.code === "resource-name-invalid");
  assert.equal(names.length, 1, "a name the engine rejects rather than rewrites");
  assert.match(names[0]!.message, /no good/);
});

test("too many resources is the engine's own cap, mirrored (#1020)", () => {
  const many = Object.fromEntries(
    Array.from({ length: RESOURCES_MAX + 1 }, (_, i) => [`r${i}`, { slots: 1 }])
  );
  const w: Workflow = { ...starterWorkflow(), resources: many };
  const over = validateWorkflow(w).filter((f) => f.code === "section-out-of-range");
  assert.equal(over.length, 1);
  assert.match(over[0]!.message, new RegExp(String(RESOURCES_MAX)));
  // Exactly at the cap is fine — an off-by-one here would refuse a legal file.
  const at: Workflow = {
    ...starterWorkflow(),
    resources: Object.fromEntries(Object.entries(many).slice(0, RESOURCES_MAX)),
  };
  assert.deepEqual(
    validateWorkflow(at).filter((f) => f.code === "section-out-of-range"),
    []
  );
});

test("a clean policy surface stays clean — none of the new rules fires on the documented schema", () => {
  const { findings } = analyzeWorkflow(FULL_SURFACE);
  assert.deepEqual(codes(findings), [], "the #880 fixture uses every section, legally");
});

test("declaring a section and undeclaring it again leaves the file byte-for-byte (#1020)", () => {
  // The edge case the form's enable-toggle has to get right: tick `merge_queue:` on, change
  // your mind, untick it — and the file that never declared one still doesn't. Declared-only
  // emission is what makes that true, and this is the round-trip that proves it rather than
  // asserting it.
  const original = `# a hand-written file
version: 1
name: t

blocks:
  - id: w
    name: W
    kind: worker
    cli: claude
`;
  const { workflow } = parseWorkflow(original);
  const on: Workflow = { ...workflow, merge_queue: { enabled: true } };
  const withQueue = serializeWorkflowPreserving(on, original);
  assert.match(withQueue, /^merge_queue:$/m, "the toggle really did write the section");
  assert.match(withQueue, /^ {2}enabled: true$/m);

  const off: Workflow = { ...workflow };
  delete off.merge_queue;
  assert.equal(
    serializeWorkflowPreserving(off, original),
    original,
    "untick must leave an undeclared file undeclared, comments and all"
  );
  // …and the same for the other two sections, which get the same toggle.
  for (const section of ["intake", "resources"] as const) {
    const declared: Workflow = { ...workflow, [section]: {} };
    const text = serializeWorkflowPreserving(declared, original);
    assert.match(text, new RegExp(`^${section}: \\{\\}$`, "m"));
    const undeclared: Workflow = { ...workflow };
    delete undeclared[section];
    assert.equal(serializeWorkflowPreserving(undeclared, original), original, section);
  }
});

test("the three sections a form writes survive a round-trip through their own text (#1020)", () => {
  // What the forms actually produce, read back: every field the pane can now set has to come
  // back as itself, or the next render shows something the human didn't type.
  const { workflow } = parseWorkflow(
    "version: 1\nblocks:\n  - id: w\n    kind: worker\n    cli: claude\n"
  );
  const edited: Workflow = {
    ...workflow,
    intake: { source: "board", labels: { ready: "go", hold: "wait_here" } },
    merge_queue: {
      enabled: true,
      max_batch: MERGE_QUEUE_MAX_BATCH_MIN,
      checks_timeout_minutes: 45,
    },
    resources: { build: { slots: RESOURCE_SLOTS_MAX }, docs: { max_hold_minutes: 5 } },
  };
  const text = serializeWorkflow(edited);
  const reread = parseWorkflow(text);
  assert.deepEqual(reread.findings, []);
  assert.deepEqual(reread.workflow, edited);
  assert.deepEqual(
    validateWorkflow(reread.workflow).filter((f) => f.severity === "error"),
    []
  );
});

test("the identifier rules accept exactly what the engine accepts (#1020)", () => {
  assert.equal(isValidIntakeLabel("agent-ready"), true);
  assert.equal(isValidIntakeLabel("Agent_Ready1"), true);
  assert.equal(isValidIntakeLabel(""), true, "empty = inherit this one");
  assert.equal(isValidIntakeLabel("-hold"), false, "a positional beginning with a dash reads as a flag");
  assert.equal(isValidIntakeLabel("needs triage"), false);
  assert.equal(isValidIntakeLabel("a".repeat(ID_MAX_CHARS + 1)), false, "rejected, never truncated");
  assert.equal(isValidResourceName("-build"), true, "no argv carries a resource name");
  assert.equal(isValidResourceName("heavy build"), false);
  assert.equal(isValidResourceName(""), false);
  assert.equal(isValidResourceName("a".repeat(ID_MAX_CHARS)), true);
});

// ---------- path-based reviewer routing (#1176) ----------

const ROUTED_YAML = `version: 1
blocks:
  - id: worker
    kind: worker
    cli: claude
  - id: rev-lead
    kind: reviewer
    cli: claude
  - id: rev-ui
    kind: reviewer
    cli: claude
gates:
  merge:
    require: all-pass
    reviewers: [rev-lead]
    routing:
      - paths: ["src/**", "index.html"]
        reviewers: [rev-ui]
`;

test("routing rules survive a round trip — the pane preserves what it cannot yet edit (#1176)", () => {
  // THE failure this covers: `MergeGate` has no unknown-key bag, so a gate key the
  // parser does not read is a line the next form edit silently DELETES — and here
  // the thing deleted would be a REQUIRED REVIEWER. The pane offers no control for
  // these rules yet; round-tripping them is what makes that safe rather than lossy.
  const parsed = parseWorkflow(ROUTED_YAML).workflow;
  assert.deepEqual(parsed.gates.merge!.routing, [
    { paths: ["src/**", "index.html"], reviewers: ["rev-ui"] },
  ]);
  assert.deepEqual(codes(validateWorkflow(parsed)), [], "a well-formed rule is a clean file");

  const text = serializeWorkflow(parsed);
  // A glob starting with `*` MUST come back quoted: YAML reads a bare leading `*`
  // as an ALIAS, so an unquoted `**/Cargo.toml` is a file that will not load.
  const starred = parseWorkflow(ROUTED_YAML.replace('"src/**", "index.html"', '"**/Cargo.toml"'))
    .workflow;
  const starredText = serializeWorkflow(starred);
  assert.ok(starredText.includes('"**/Cargo.toml"'), `a leading-star glob must be quoted:\n${starredText}`);
  assert.deepEqual(
    parseWorkflow(starredText).workflow.gates.merge!.routing,
    [{ paths: ["**/Cargo.toml"], reviewers: ["rev-ui"] }],
    "…and survive the round trip it makes possible"
  );

  assert.deepEqual(
    parseWorkflow(text).workflow.gates.merge!.routing,
    parsed.gates.merge!.routing,
    `a save must not drop or mangle a routing rule:\n${text}`
  );

  // Undeclared stays undeclared: opening the gate form on a repo with no routing
  // cannot invent an empty block.
  const none = starterWorkflow();
  assert.equal(none.gates.merge!.routing, undefined);
  assert.ok(!serializeWorkflow(none).includes("routing"));
});

test("the pane refuses every routing rule the engine refuses (#1176)", () => {
  const routed = (rules: unknown): ReturnType<typeof validateWorkflow> => {
    const w = parseWorkflow(ROUTED_YAML).workflow;
    (w.gates.merge as { routing?: unknown }).routing = rules;
    return validateWorkflow(w);
  };

  // A rule with no paths can never fire; one with no reviewers requires nobody.
  assert.ok(has(routed([{ paths: [], reviewers: ["rev-ui"] }]), "gate-bad-routing"));
  assert.ok(has(routed([{ paths: ["src/**"], reviewers: [] }]), "gate-bad-routing"));

  // The reviewer checks are the gate's OWN checks, from one definition — so a
  // routing rule cannot quietly accept a block `reviewers:` would refuse.
  assert.ok(has(routed([{ paths: ["src/**"], reviewers: ["ghost"] }]), "gate-unknown-reviewer"));
  assert.ok(has(routed([{ paths: ["src/**"], reviewers: ["worker"] }]), "gate-not-a-reviewer"));
  // …and the finding names the RULE, so a human knows which line to fix.
  const f = routed([
    { paths: ["a/**"], reviewers: ["rev-ui"] },
    { paths: ["b/**"], reviewers: ["ghost"] },
  ]);
  assert.ok(
    f.some((x) => x.code === "gate-unknown-reviewer" && x.message.includes("Routing rule 2")),
    JSON.stringify(f)
  );

  // Globs outside the alphabet, and the three shapes that could never fire.
  for (const bad of ["src/[ab]*", "a?c", "src/a b", "/src/**", "src/", "../etc/**", ""]) {
    assert.ok(
      has(routed([{ paths: [bad], reviewers: ["rev-ui"] }]), "gate-bad-routing"),
      `"${bad}" must be a finding`
    );
  }
  // …and the ones that are fine, so the rule above is not simply flagging everything.
  for (const ok of ["src/**", "**/Cargo.toml", "package-lock.json", "a_b-c.d/*", "a..b.txt"]) {
    assert.deepEqual(
      codes(routed([{ paths: [ok], reviewers: ["rev-ui"] }])),
      [],
      `"${ok}" is inside the alphabet`
    );
  }

  // The caps.
  assert.ok(
    has(
      routed(
        Array.from({ length: GATE_ROUTING_RULES_MAX + 1 }, (_, i) => ({
          paths: [`d${i}/**`],
          reviewers: ["rev-ui"],
        }))
      ),
      "gate-bad-routing"
    )
  );
  assert.ok(
    has(
      routed([
        {
          paths: Array.from({ length: GATE_ROUTING_PATHS_MAX + 1 }, (_, i) => `d${i}/**`),
          reviewers: ["rev-ui"],
        },
      ]),
      "gate-bad-routing"
    )
  );

  // The pair with no honest reading: a threshold counts passes over a FIXED list.
  const thresholded = parseWorkflow(ROUTED_YAML).workflow;
  thresholded.gates.merge!.require = "threshold";
  thresholded.gates.merge!.threshold = 1;
  assert.ok(has(validateWorkflow(thresholded), "gate-bad-routing"));

  // A malformed `routing:` in the FILE is a finding at parse time, never a
  // coerced value and never a silently dropped key.
  assert.ok(
    parseWorkflow(ROUTED_YAML.replace(/    routing:[\s\S]*$/, "    routing: nope\n")).findings.some(
      (x) => x.code === "gate-bad-routing"
    )
  );
  assert.ok(
    parseWorkflow(
      ROUTED_YAML.replace('      - paths: ["src/**", "index.html"]', "      - path: [src]")
    ).findings.some((x) => x.code === "gate-bad-routing"),
    "an unknown key inside a rule is a refusal — the engine refuses the whole file over one"
  );
});

// ── `role_hint: liaison` is superseded by `kind: manager` (#1161 M6, D4) ──
//
// What these are FOR: decision D4 is "the hint keeps parsing, with a superseded
// warning; removal is a later, separate, human decision". Both halves are
// load-bearing and each fails differently. If the warning stops firing, a file
// written before the manager class existed never learns a better shape exists.
// If it hardens into an ERROR, every repo that shipped a liaison stops loading
// on an upgrade — which is precisely what D4 refused to do.

test("a valid liaison block is warned, not refused (#1161 D4)", () => {
  const w = parseWorkflow(
    "version: 1\nblocks:\n  - id: human\n    kind: reviewer\n    cli: claude\n    role_hint: liaison\n"
  ).workflow;
  const f = validateWorkflow(w).filter((x) => x.code === "role-hint-superseded");
  assert.equal(f.length, 1, "exactly one supersession advisory per liaison block");
  assert.equal(f[0]!.severity, "warning", "an ERROR here would stop a shipped repo from loading");
  assert.equal(f[0]!.blockId, "human", "the finding must land on the block that carries the hint");
  // It has to name the replacement. A deprecation notice that does not say what
  // to write instead is a nag, and the author's next move is a search.
  assert.match(f[0]!.message, /kind: manager/, f[0]!.message);
  // …and it must not read as a refusal, because it is not one.
  assert.equal(
    validateWorkflow(w).some((x) => x.severity === "error"),
    false,
    "a liaison file still validates — D4 keeps it running"
  );
});

test("the supersession advisory fires for `liaison` alone", () => {
  // THE CONTROL. Without it, "the liaison is warned" is satisfied by a
  // validator that warns on every hint, or on every block.
  //
  // The population is DERIVED from `ROLE_HINTS` rather than listed, so a fourth
  // hint is covered on the day it is added instead of on the day someone
  // remembers this test. A hardcoded pair would still read as a control while
  // silently covering two of three (#1502 review N5).
  const others = ROLE_HINTS.filter((h) => h !== "liaison");
  assert.ok(others.length > 0, "the control must have something to control WITH");
  for (const hint of others) {
    const kind = roleHintRequires(hint);
    assert.ok(kind, `${hint} must resolve to a kind, or this row tests nothing`);
    const w = parseWorkflow(
      `version: 1\nblocks:\n  - id: b\n    kind: ${kind}\n    cli: claude\n    role_hint: ${hint}\n`
    ).workflow;
    assert.deepEqual(
      codes(validateWorkflow(w)).filter((c) => c === "role-hint-superseded"),
      [],
      `${hint} is not superseded by anything — it must draw no advisory`
    );
  }
  // A block with no hint at all is the commonest case of all.
  const plain = parseWorkflow(
    "version: 1\nblocks:\n  - id: b\n    kind: worker\n    cli: claude\n"
  ).workflow;
  assert.deepEqual(codes(validateWorkflow(plain)).filter((c) => c.startsWith("role-hint")), []);
});

test("a liaison on the WRONG kind is still refused, and draws no advisory", () => {
  // The ladder's order matters: an author who wrote `role_hint: liaison` on a
  // worker has a file that will not load, and telling them about `kind: manager`
  // instead of about the refusal would bury the thing that actually stops them.
  const w = parseWorkflow(
    "version: 1\nblocks:\n  - id: b\n    kind: worker\n    cli: claude\n    role_hint: liaison\n"
  ).workflow;
  const role = codes(validateWorkflow(w)).filter((c) => c.startsWith("role-hint"));
  assert.deepEqual(role, ["role-hint-wrong-kind"]);
});

// ---------- named workflows: the name rule mirrors the engine (#1689 slice D1) ----------

// The engine is the authority: `loomux_engine::pathseg::check_segment` decides whether a
// name can become `<name>.yml`, and a backend that refuses one the picker offered would make
// the launcher advertise a workflow no launch can run. These read the engine's own source
// rather than restating its rules in prose, so a change there fails HERE rather than in a
// repo six months later. Each extraction asserts it found something first — a regex that
// stops matching would otherwise "pin" an empty set, which is the vacuity every absence-only
// assertion in this repo owes a control for.
const PATHSEG_SRC = readFileSync(new URL("../crates/loomux-engine/src/pathseg.rs", import.meta.url), "utf8");

test("the frontend's workflow-name length cap is the engine's MAX_SEGMENT_LEN", () => {
  const m = /pub const MAX_SEGMENT_LEN: usize = (\d+);/.exec(PATHSEG_SRC);
  assert.ok(m, "MAX_SEGMENT_LEN not found in pathseg.rs — the pin is reading nothing");
  assert.equal(WORKFLOW_NAME_MAX, Number(m[1]));
  // …and it is really enforced, in both directions, so the constant is not decoration.
  assert.equal(isWorkflowName("a".repeat(WORKFLOW_NAME_MAX)), true);
  assert.equal(isWorkflowName("a".repeat(WORKFLOW_NAME_MAX + 1)), false);
});

test("the frontend refuses every Windows device name the engine reserves", () => {
  const block = /RESERVED_DEVICE_NAMES: &\[&str\] = &\[([\s\S]*?)\];/.exec(PATHSEG_SRC);
  assert.ok(block, "RESERVED_DEVICE_NAMES not found in pathseg.rs — the pin is reading nothing");
  const names = [...block[1]!.matchAll(/"([a-z0-9]+)"/g)].map((m) => m[1]!);
  assert.ok(names.length >= 22, `expected the full device list, read ${names.length}`);
  for (const n of names) {
    assert.equal(isWorkflowName(n), false, `${n} is a device name and must be refused`);
    assert.equal(isWorkflowName(n.toUpperCase()), false, `${n} is reserved case-insensitively`);
  }
  // The positive control for the loop above: a name of the same SHAPE that is not on the
  // list is accepted, so "refuses everything" cannot pass this test.
  assert.equal(isWorkflowName("com10"), true);
  assert.equal(isWorkflowName("console"), true);
});

test("the frontend's alphabet is the engine's, and the refusals are refusals, not rewrites", () => {
  // The alphabet, quoted from the engine's own predicate so a widening there is visible here.
  assert.match(PATHSEG_SRC, /c\.is_ascii_alphanumeric\(\) \|\| \*c == '-' \|\| \*c == '_'/);
  for (const ok of ["default", "review-heavy", "solo_fast", "a", "A9", "x-9_z"]) {
    assert.equal(isWorkflowName(ok), true, ok);
  }
  for (const bad of ["", "..", "../x", "a/b", "a.b", "a b", "-x", "CON", "nul", "a:b", "café"]) {
    assert.equal(isWorkflowName(bad), false, bad);
  }
  // REFUSED, never rewritten (the `pathseg` rule): a bad name yields null, not a sanitized
  // path. Two strings that normalize to one name are two files claiming one workflow.
  assert.equal(workflowRelFor("../x"), null);
  assert.equal(workflowRelFor("a/b"), null);
  assert.equal(workflowRelFor("CON"), null);
  assert.equal(workflowRelFor("-x"), null);
});

test("a name resolves to the file the engine would read, in either config-dir spelling", () => {
  assert.equal(workflowRelFor(DEFAULT_WORKFLOW_NAME), WORKFLOW_FILE);
  assert.equal(workflowRelFor(DEFAULT_WORKFLOW_NAME, { legacy: true }), LEGACY_WORKFLOW_FILE);
  assert.equal(workflowRelFor("review-heavy"), `${WORKFLOWS_DIR}/review-heavy.yml`);
  assert.equal(workflowRelFor("review-heavy", { legacy: true }), `${LEGACY_WORKFLOWS_DIR}/review-heavy.yml`);
  // The directories are the config dirs' own children — never a third spelling.
  assert.equal(WORKFLOWS_DIR, `${CONFIG_DIR}/workflows`);
  assert.equal(LEGACY_WORKFLOWS_DIR, `${LEGACY_CONFIG_DIR}/workflows`);
});

test("a path names a workflow only when it really is one of this repo's workflow files", () => {
  assert.equal(workflowNameOf(WORKFLOW_FILE), DEFAULT_WORKFLOW_NAME);
  assert.equal(workflowNameOf(LEGACY_WORKFLOW_FILE), DEFAULT_WORKFLOW_NAME);
  assert.equal(workflowNameOf(`${WORKFLOWS_DIR}/review-heavy.yml`), "review-heavy");
  assert.equal(workflowNameOf(`${LEGACY_WORKFLOWS_DIR}/solo_fast.yml`), "solo_fast");
  // Round-trips with its inverse for every name the picker can hold.
  for (const n of [DEFAULT_WORKFLOW_NAME, "review-heavy", "solo_fast"]) {
    assert.equal(workflowNameOf(workflowRelFor(n)!), n);
    assert.equal(workflowNameOf(workflowRelFor(n, { legacy: true })!), n);
  }
  // Windows separators are the same paths — the pane is handed either.
  assert.equal(workflowNameOf(".orrerix\\workflows\\a.yml"), "a");
  // And everything else has NO name, which is the honest answer: a pane showing an
  // arbitrary file is showing a FILE, and calling it `default` would tell the human it is
  // the workflow their group runs.
  for (const notOne of [
    "workflow.yml",
    "teams/api/.orrerix/workflow.yml",
    ".orrerix/workflows/nested/a.yml",
    ".orrerix/workflows/a.yaml",
    ".orrerix/other/a.yml",
    ".orrerix/workflows/CON.yml",
    ".orrerix/workflows/-x.yml",
    ".orrerix/workflows/.yml",
    "",
  ]) {
    assert.equal(workflowNameOf(notOne), null, notOne);
  }
});
