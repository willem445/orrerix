// Generates the two fixture streams. Run: node fixtures/make.mjs
//
// The fixtures are pi's OWN RPC wire shapes (doc/design/pi.md + the #2850 plan
// comment's event list), not a convenient invention — so the decoder in
// ../decode.js is doing the real mapping work S1b will do in Rust, and the
// renderer never sees a pi field name.
//
// Every line carries `at`: milliseconds from the start of the stream. That is
// the demo's own field, not pi's — a real driver reads events as they arrive
// and has no need for it. It is what makes playback look like work happening
// rather than a file being read.

import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));

// ---------------------------------------------------------------- utilities

class Stream {
  constructor() { this.t = 0; this.lines = []; }
  at(ms) { this.t += ms; return this; }
  push(o) { this.lines.push({ at: this.t, ...o }); return this; }

  /** Stream text as deltas at a plausible token cadence. */
  say(text, msPerChunk = 28, chunk = 9) {
    for (let i = 0; i < text.length; i += chunk) {
      this.at(msPerChunk).push({
        type: "message_update",
        assistantMessageEvent: { type: "text_delta", delta: text.slice(i, i + chunk) },
      });
    }
    return this;
  }

  think(text, msPerChunk = 16, chunk = 14) {
    for (let i = 0; i < text.length; i += chunk) {
      this.at(msPerChunk).push({
        type: "message_update",
        assistantMessageEvent: { type: "thinking_delta", delta: text.slice(i, i + chunk) },
      });
    }
    return this;
  }

  /** A complete tool call: announce, run, stream output, settle. */
  tool(id, name, args, { runMs = 300, out = [], isError = false, meta = null, gap = 120 } = {}) {
    this.at(gap).push({
      type: "message_update",
      assistantMessageEvent: { type: "toolcall_end", toolCallId: id, toolName: name, arguments: args },
    });
    this.at(90).push({ type: "tool_execution_start", toolCallId: id });
    const slice = out.length ? Math.max(40, Math.floor(runMs / (out.length + 1))) : runMs;
    for (const piece of out) {
      this.at(slice).push({
        type: "tool_execution_update",
        toolCallId: id,
        partialResult: { content: [{ type: "text", text: piece }] },
      });
    }
    this.at(slice).push({
      type: "tool_execution_end",
      toolCallId: id,
      result: { content: [] },
      isError,
      durationMs: runMs,
      meta,
    });
    return this;
  }

  usage(u) { this.push({ type: "message_update", usage: u }); return this; }

  write(name) {
    const body = this.lines.map((l) => JSON.stringify(l)).join("\n") + "\n";
    writeFileSync(join(HERE, name), body, "utf8");
    console.log(`${name}: ${this.lines.length} lines, ${(body.length / 1024).toFixed(1)} KiB, ${(this.t / 1000).toFixed(1)}s of stream`);
  }
}

const tok = (input, output, cacheRead, reasoning = 0, cost = 0) => ({
  input, output, cacheRead, cacheWrite: 0, reasoning,
  totalTokens: input + output + cacheRead, cost,
});

// ============================================================== session.jsonl
// The narrative fixture: one worker agent taking a delivery, thinking, reading,
// editing, running a command that FAILS, recovering, hitting a permission
// gate, being steered mid-turn, and compacting. Every event kind the renderer
// knows appears at least once.

const s = new Stream();

// pi's RPC has no ready event — the driver synthesizes Booted from get_state.
s.push({
  type: "response", id: "boot", command: "get_state", success: true,
  data: {
    sessionId: "0f3c9a2e-7b41-4d80-9c15-2ae6f8d31b77",
    model: "anthropic/claude-sonnet-4.5",
    thinkingLevel: "high",
    capabilities: ["steer", "abort", "set_model", "set_thinking_level", "compact", "ui_dialog"],
  },
});

s.at(400).push({
  type: "orrerix_delivery", from: "orchestrator", deliveryKind: "kickoff", ts: "09:14:02",
  text: "#2214 — the pane header drops the model name when a group resumes. Reproduce it, fix it, and add a test that fails without the fix. Branch fix/2214-resume-model, draft PR to main.",
});

s.at(300).push({ type: "turn_start", turnId: 1 });
s.at(200).push({ type: "message_start" });

s.think(
  "The header reads model off the roster row, and a resume rebuilds that roster from group.json rather than from the workflow file. If group.json was written before the model field existed, the row comes back with model absent — and the header renders absent as empty rather than as the last known value. Let me look at how the roster is rehydrated before assuming that."
);

s.at(300).say("Reproducing first. The header takes its model from the roster row, so the question is what a resume writes into that row.\n\n");

s.tool("t1", "Read", { file_path: "src-tauri/src/orchestration/roster.rs", offset: 120, limit: 60 }, {
  runMs: 240,
  out: ["120: fn rehydrate(raw: &RawRoster) -> Roster {\n121:     let model = raw.model.clone();\n122:     Roster { id: raw.id.clone(), model, .. }\n123: }\n"],
});

s.tool("t2", "Grep", { pattern: "model", path: "src/panehead.ts", output_mode: "content" }, {
  runMs: 180,
  out: ["41:  head.model.textContent = row.model ?? \"\";\n"],
});

s.at(250).say("Found it. `row.model ?? \"\"` renders a missing model as an empty string, and a resumed roster row has no model when group.json predates the field. The fix is to fall back to the persisted launch model rather than to empty.\n\n");

s.think("Before I edit, the test has to redden. I will write the test first against the current code and watch it fail for the right reason — an empty string where a model name belongs, not a compile error.");

s.at(200).say("Writing the failing test first.\n\n");

s.tool("t3", "Edit", {
  file_path: "test/panehead.test.ts",
  old_string: "  assert.equal(head.model.textContent, \"sonnet-4.5\");\n});",
  new_string: "  assert.equal(head.model.textContent, \"sonnet-4.5\");\n});\n\ntest(\"a resumed row with no model falls back to the launch model\", () => {\n  const head = render({ id: \"w-1\", model: undefined, launchModel: \"sonnet-4.5\" });\n  assert.equal(head.model.textContent, \"sonnet-4.5\");\n});",
}, { runMs: 160 });

// The command that fails — red before green, on screen.
s.tool("t4", "Bash", { command: "node --test test/panehead.test.ts", description: "run the new test against unfixed code" }, {
  runMs: 2400,
  isError: true,
  meta: { exitCode: 1 },
  out: [
    "TAP version 13\n# Subtest: the header shows the model\nok 1 - the header shows the model\n",
    "# Subtest: a resumed row with no model falls back to the launch model\nnot ok 2 - a resumed row with no model falls back to the launch model\n",
    "  ---\n  error: 'Expected values to be strictly equal:\\n\\n'' !== ''sonnet-4.5''\\n'\n  code: 'ERR_ASSERTION'\n  expected: 'sonnet-4.5'\n  actual: ''\n  ...\n",
    "1..2\n# pass 1\n# fail 1\n",
  ],
});

s.at(400).say("Red, and for the right reason: an empty string where the model name belongs. Now the fix.\n\n");

s.tool("t5", "Edit", {
  file_path: "src/panehead.ts",
  old_string: "  head.model.textContent = row.model ?? \"\";",
  new_string: "  // A resumed roster row predating the model field has no model of its own.\n  // Fall back to the model the group was launched with rather than to empty:\n  // an empty header reads as \"no model\", which is never true of a live pane.\n  head.model.textContent = row.model ?? row.launchModel ?? \"\";",
}, { runMs: 150 });

s.tool("t6", "Bash", { command: "node --test test/panehead.test.ts" }, {
  runMs: 2100,
  meta: { exitCode: 0 },
  out: ["TAP version 13\nok 1 - the header shows the model\nok 2 - a resumed row with no model falls back to the launch model\n1..2\n# pass 2\n# fail 0\n"],
});

s.at(200).usage(tok(41200, 1840, 38900, 1210, 0.0642));

s.at(300).say("Green. Committing and opening the draft PR.\n\n");

// The permission gate — orrerix's own, not an extension dialog.
s.at(200).push({
  type: "message_update",
  assistantMessageEvent: { type: "toolcall_end", toolCallId: "t7", toolName: "Bash", arguments: { command: "git push -u origin fix/2214-resume-model" } },
});
s.at(150).push({
  type: "permission_request", id: "p1", tool: "Bash",
  input: { command: "git push -u origin fix/2214-resume-model" },
});

// It sits there — the pane pulses — then the human answers.
s.at(3800).push({ type: "permission_settled", id: "p1", decision: "allow", by: "human" });

s.at(200).push({ type: "tool_execution_start", toolCallId: "t7" });
s.at(1400).push({
  type: "tool_execution_end", toolCallId: "t7",
  result: { content: [{ type: "text", text: "branch 'fix/2214-resume-model' set up to track 'origin/fix/2214-resume-model'.\n" }] },
  isError: false, durationMs: 1400, meta: { exitCode: 0 },
});

// Steering arrives mid-turn — the queue indicator lights up.
s.at(600).push({ type: "queue_update", steering: ["also check the thumbnail path"], followUp: [] });
s.at(500).push({
  type: "orrerix_delivery", from: "human", deliveryKind: "steer", ts: "09:16:44",
  text: "Also check the thumbnail path — I think it reads the same row.",
});
s.at(300).push({ type: "queue_update", steering: [], followUp: [] });

s.at(300).say("Checking the thumbnail path for the same read.\n\n");

s.tool("t8", "Grep", { pattern: "row\\.model", path: "src", output_mode: "files_with_matches" }, {
  runMs: 220,
  out: ["src/panehead.ts\nsrc/thumbnail.ts\n"],
});

s.at(200).say("It does. Same fix there.\n\n");

s.tool("t9", "Edit", {
  file_path: "src/thumbnail.ts",
  old_string: "  const label = row.model ?? \"\";",
  new_string: "  const label = row.model ?? row.launchModel ?? \"\";",
}, { runMs: 140 });

// A retry — the network wobbled.
s.at(300).push({ type: "auto_retry_start", reason: "529 overloaded", attempt: 1, maxAttempts: 5, delayMs: 800 });
s.at(900).push({ type: "auto_retry_end", success: true });

// An extension UI request — decision-grade, so it is a card.
s.at(400).push({
  type: "extension_ui_request", id: "u1", method: "select",
  title: "Which base branch for this PR?",
  message: "The repo has two long-lived branches. orrerix cannot infer which one #2214 targets.",
  options: [{ label: "main", value: "main" }, { label: "release/1.3", value: "release/1.3" }],
  timeoutMs: 120000,
});
s.at(4200).push({ type: "extension_ui_response", id: "u1", value: "main", by: "human" });

s.at(300).say("Opening the draft PR against main.\n\n");

s.tool("t10", "Bash", { command: "gh pr create --draft --base main --title 'fix(ui): keep the model name across a resume (#2214)'" }, {
  runMs: 3100,
  meta: { exitCode: 0 },
  out: ["https://github.com/willem445/orrerix/pull/2941\n"],
});

s.at(300).say("Draft PR #2941 is open. The test fails without the fix and passes with it; both call sites are covered.");

s.at(400).push({ type: "turn_end", turnId: 1, stopReason: "end_turn" });
s.at(150).push({
  type: "response", id: "s1", command: "get_session_stats", success: true,
  data: {
    sessionId: "0f3c9a2e-7b41-4d80-9c15-2ae6f8d31b77",
    tokens: { input: 58400, output: 3120, cacheRead: 52100, cacheWrite: 1200, reasoning: 1980, total: 114820 },
    cost: 0.1284,
    contextUsage: { tokens: 114820, contextWindow: 200000, percent: 0.574 },
  },
});

// Compaction — the seam.
s.at(900).push({ type: "compaction_start", reason: "threshold", preTokens: 154200 });
s.at(2600).push({ type: "compaction_end", reason: "threshold", preTokens: 154200, postTokens: 28400 });
s.at(200).push({
  type: "response", id: "s2", command: "get_session_stats", success: true,
  data: {
    sessionId: "0f3c9a2e-7b41-4d80-9c15-2ae6f8d31b77",
    tokens: { input: 28400, output: 3120, cacheRead: 0, cacheWrite: 28400, reasoning: 1980, total: 31520 },
    cost: 0.1471,
    contextUsage: { tokens: 31520, contextWindow: 200000, percent: 0.158 },
  },
});

s.at(400).push({ type: "agent_settled" });
s.write("session.jsonl");

// ================================================================ storm.jsonl
// The stance made visible: a lot of output and a lot of calls, fast. It is here
// so the human can SEE what the ring and the per-card cap do rather than read
// about them — the transcript stays responsive, the head of a huge output is
// dropped with the drop stated, and the live end never gets pushed off screen.

const z = new Stream();

z.push({
  type: "response", id: "boot", command: "get_state", success: true,
  data: {
    sessionId: "b71e4c05-2d9a-4f13-8e60-77c1a9b4e2d3",
    model: "anthropic/claude-sonnet-4.5",
    thinkingLevel: "medium",
    capabilities: ["steer", "abort", "set_model", "set_thinking_level", "compact", "ui_dialog"],
  },
});

z.at(300).push({
  type: "orrerix_delivery", from: "orchestrator", deliveryKind: "kickoff", ts: "11:02:10",
  text: "Full-tree audit: every source file, every crate. Report anything that grep can find and a human cannot.",
});

z.at(200).push({ type: "turn_start", turnId: 1 });
z.at(200).say("Sweeping the tree.\n\n", 20, 12);

// One enormous build log. It has to CLEAR the renderer's 64 KiB per-card cap,
// not approach it: a storm fixture that stays under every limit demonstrates
// nothing, which is exactly what the first cut of this file did (900 lines,
// ~52 KiB, no elision, no ring drop). 2400 lines is ~140 KiB, so the head is
// dropped and the elision notice has to appear.
const bigLog = [];
for (let i = 1; i <= 2400; i++) {
  bigLog.push(`   Compiling crate-${String(i).padStart(4, "0")} v0.${i % 30}.${i % 7} (/c/Projects/loomux/crates/c${i})\n`);
}
z.tool("s0", "Bash", { command: "cargo check --locked --workspace 2>&1" }, {
  runMs: 5200,
  meta: { exitCode: 0 },
  out: chunks(bigLog.join(""), 12).concat(["    Finished `dev` profile in 41.28s\n"]),
});

// Many calls in quick succession — the row ring is what keeps this cheap.
const FILES = [
  "src/pane.ts", "src/grid.ts", "src/layout.ts", "src/theme.ts", "src/icons.ts",
  "src/tasksview.ts", "src/gitview.ts", "src/auditview.ts", "src/transport.ts",
  "crates/loomux-engine/src/obs.rs", "crates/loomux-engine/src/queue.rs",
  "crates/loomux-engine/src/groupid.rs", "crates/loomux-engine/src/pathseg.rs",
  "src-tauri/src/orchestration/mod.rs", "src-tauri/src/orchestration/workflow.rs",
];
// Enough calls to overrun the renderer's 400-row ring, so the "rolled out of
// the pane buffer" notice is on screen rather than merely implemented.
for (let i = 0; i < 460; i++) {
  const f = FILES[i % FILES.length];
  const bad = i % 17 === 16;
  z.tool(`s${i + 1}`, i % 3 === 0 ? "Grep" : "Read", i % 3 === 0
    ? { pattern: "unwrap\\(\\)", path: f, output_mode: "content" }
    : { file_path: f, limit: 40 }, {
    gap: 25,
    runMs: 55,
    isError: bad,
    out: bad
      ? ["Error: ENOENT: no such file or directory\n"]
      : [`${f}: ${(i * 7) % 40} matches\n`],
  });
  if (i % 40 === 39) {
    z.at(80).usage(tok(120000 + i * 400, 900 + i * 12, 96000, 400, 0.02 + i * 0.0016));
  }
}

z.at(300).say("\nSweep done. 460 files, 27 unreadable.\n", 18, 14);
z.at(300).push({ type: "turn_end", turnId: 1, stopReason: "end_turn" });
z.at(150).push({
  type: "response", id: "s1", command: "get_session_stats", success: true,
  data: {
    sessionId: "b71e4c05-2d9a-4f13-8e60-77c1a9b4e2d3",
    tokens: { input: 178400, output: 4200, cacheRead: 160000, cacheWrite: 8000, reasoning: 620, total: 190600 },
    cost: 0.4118,
    contextUsage: { tokens: 190600, contextWindow: 200000, percent: 0.953 },
  },
});
z.at(200).push({ type: "agent_settled" });
// The audit is a one-shot run, so its child really does exit — which is the
// one HarnessEvent variant BOTH pane kinds emit identically (§1.3's table), and
// the only kind session.jsonl has no honest place for: a worker pane stays
// alive waiting for its next delivery.
z.at(700).push({ type: "orrerix_notice", level: "info", text: "sweep complete — pane will close" });
z.at(400).push({ type: "exit", code: 0 });
z.write("storm.jsonl");

function chunks(s, n) {
  const size = Math.ceil(s.length / n);
  const out = [];
  for (let i = 0; i < s.length; i += size) out.push(s.slice(i, i + size));
  return out;
}
