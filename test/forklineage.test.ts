// Fork lineage (#3368, #3318 F5) — the pure derivation behind the session
// browser's fork tree and the pane header's `↰ parent` crumb.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  buildForkIndex,
  lineageOf,
  parentLink,
  forkTreeRows,
  gatherForkPointers,
  sessionDisplayName,
  forkCrumbLabel,
  type ForkPointer,
} from "../src/forklineage.ts";
import { forkNameDecision } from "../src/forkname.ts";

const ptr = (child: string, parent: string, source: ForkPointer["source"] = "pane"): ForkPointer => ({
  child,
  parent,
  source,
});

test("a fork of a fork walks back to the root, root-ward, every node once", () => {
  const idx = buildForkIndex([ptr("c", "b"), ptr("b", "a")], ["a", "b", "c"]);
  assert.deepEqual(lineageOf(idx, "c"), { chain: ["c", "b", "a"], end: { kind: "root" } });
  assert.deepEqual(lineageOf(idx, "a"), { chain: ["a"], end: { kind: "root" } });
});

test("the chain is walked across all three records, not read from any one", () => {
  // c's pointer is on the live pane, b's on the roster, a's… nowhere: a is the
  // root. No one record holds the chain.
  const pointers = gatherForkPointers({
    panes: [{ sessionId: "c", forkOf: "b" }],
    roster: [{ session_id: "b", forked_from: "a" }],
    log: [["x", { fork_of: "c" }]],
  });
  const idx = buildForkIndex(pointers, ["a", "b", "c", "x"]);
  assert.deepEqual(lineageOf(idx, "x").chain, ["x", "c", "b", "a"]);
});

test("a missing parent: the chain stops at the last known node and says which id it could not follow", () => {
  const idx = buildForkIndex([ptr("c", "b"), ptr("b", "gone")], ["b", "c"]);
  assert.deepEqual(lineageOf(idx, "c"), { chain: ["c", "b"], end: { kind: "dangling", missing: "gone" } });
  assert.deepEqual(parentLink(idx, "b"), { kind: "dangling", parent: "gone" });
  // Control: the same shape with the parent known is a known link, so the
  // dangling verdict above is about `known`, not about the pointer.
  const known = buildForkIndex([ptr("b", "gone")], ["b", "gone"]);
  assert.deepEqual(parentLink(known, "b"), { kind: "known", parent: "gone" });
});

test("a cycle is refused, not looped on — the walk terminates and names the repeat", () => {
  const idx = buildForkIndex([ptr("a", "b"), ptr("b", "c"), ptr("c", "a")], ["a", "b", "c"]);
  assert.deepEqual(lineageOf(idx, "a"), { chain: ["a", "b", "c"], end: { kind: "cycle", at: "a" } });
  assert.deepEqual(parentLink(idx, "b"), { kind: "cycle", parent: "c" });
});

test("a self-pointer is dropped: a session is never its own parent", () => {
  const idx = buildForkIndex([ptr("a", "a")], ["a"]);
  assert.equal(parentLink(idx, "a"), null);
  assert.deepEqual(lineageOf(idx, "a"), { chain: ["a"], end: { kind: "root" } });
});

test("a session that is not a fork has no parent link", () => {
  const idx = buildForkIndex([ptr("b", "a")], ["a", "b"]);
  assert.equal(parentLink(idx, "a"), null);
  assert.equal(parentLink(idx, "nobody"), null);
});

test("disagreeing records: the live pane wins, the loser is kept as a conflict rather than dropped", () => {
  // Listed log-first on purpose: precedence is by SOURCE, never by input order.
  const idx = buildForkIndex([ptr("c", "log-parent", "log"), ptr("c", "pane-parent", "pane")], []);
  assert.equal(idx.parentOf.get("c"), "pane-parent");
  assert.deepEqual(idx.conflicts, [{ child: "c", kept: "pane-parent", ignored: "log-parent", source: "log" }]);
  // Agreeing copies (the normal case — one value written three times) are not a conflict.
  const same = buildForkIndex([ptr("c", "p", "pane"), ptr("c", "p", "roster"), ptr("c", "p", "log")], []);
  assert.deepEqual(same.conflicts, []);
});

test("a child with a pointer is known even when no other record lists it", () => {
  const idx = buildForkIndex([ptr("b", "a", "log")], ["a"]);
  assert.ok(idx.known.has("b"));
});

// ---------- the browser tree ----------

test("forks sit under their parent only while it is expanded, and the expander counts them", () => {
  const idx = buildForkIndex([ptr("f1", "p"), ptr("f2", "p")], ["p", "f1", "f2", "q"]);
  const ids = ["f2", "q", "p", "f1"]; // display order: newest first
  assert.deepEqual(forkTreeRows(ids, idx, new Set()), [
    { id: "q", depth: 0, forks: 0, expanded: false },
    { id: "p", depth: 0, forks: 2, expanded: false },
  ]);
  assert.deepEqual(forkTreeRows(ids, idx, new Set(["p"])), [
    { id: "q", depth: 0, forks: 0, expanded: false },
    { id: "p", depth: 0, forks: 2, expanded: true },
    // Siblings keep display order.
    { id: "f2", depth: 1, forks: 0, expanded: false },
    { id: "f1", depth: 1, forks: 0, expanded: false },
  ]);
});

test("forks of forks nest, and expandAll (a typed filter) reveals every one", () => {
  const idx = buildForkIndex([ptr("b", "a"), ptr("c", "b")], ["a", "b", "c"]);
  assert.deepEqual(forkTreeRows(["c", "b", "a"], idx, new Set(), true), [
    { id: "a", depth: 0, forks: 1, expanded: true },
    { id: "b", depth: 1, forks: 1, expanded: true },
    { id: "c", depth: 2, forks: 0, expanded: true },
  ]);
});

test("a fork whose parent is not in the list sits at the top level — never under a grandparent", () => {
  const idx = buildForkIndex([ptr("b", "a"), ptr("c", "b")], ["a", "b", "c"]);
  // b is filtered out; c must not be indented under a (its label says 'fork of b').
  assert.deepEqual(forkTreeRows(["c", "a"], idx, new Set(["a"])), [
    { id: "c", depth: 0, forks: 0, expanded: false },
    { id: "a", depth: 0, forks: 0, expanded: true },
  ]);
});

test("a fork with a dangling parent sits at the top level", () => {
  const idx = buildForkIndex([ptr("b", "gone")], ["b"]);
  assert.deepEqual(forkTreeRows(["b"], idx, new Set()), [{ id: "b", depth: 0, forks: 0, expanded: false }]);
});

test("members of a cycle are all still listed, at the top level", () => {
  const idx = buildForkIndex([ptr("a", "b"), ptr("b", "a")], ["a", "b"]);
  const rows = forkTreeRows(["a", "b"], idx, new Set(), true);
  assert.deepEqual(
    rows.map((r) => [r.id, r.depth]),
    [
      ["a", 0],
      ["b", 0],
    ]
  );
});

test("every listed id appears exactly once when everything is expanded", () => {
  const idx = buildForkIndex(
    [ptr("b", "a"), ptr("c", "a"), ptr("d", "c"), ptr("e", "gone"), ptr("x", "y"), ptr("y", "x")],
    ["a", "b", "c", "d", "e", "x", "y", "z"]
  );
  const ids = ["z", "e", "d", "c", "b", "a", "y", "x"];
  const rows = forkTreeRows(ids, idx, new Set(), true);
  assert.deepEqual(rows.map((r) => r.id).sort(), [...ids].sort());
});

test("a CLOSED Solo fork still resolves its parent in the browser tree, from the sessions log alone", () => {
  // The fork's pane is gone, so tabs.json has no forkOf for it and no roster
  // knows it (a Solo pane is not a delegate). Only sessionlog.json's fork_of
  // remains — and the browser rows (the CLI store scan) list both sessions.
  const browserRows = ["fork", "parent"];
  const pointers = gatherForkPointers({
    panes: [{ sessionId: "parent", forkOf: null }],
    roster: [],
    log: [
      ["fork", { fork_of: "parent" }],
      ["parent", {}],
    ],
  });
  const idx = buildForkIndex(pointers, browserRows);
  assert.deepEqual(parentLink(idx, "fork"), { kind: "known", parent: "parent" });
  assert.deepEqual(forkTreeRows(browserRows, idx, new Set(["parent"])), [
    { id: "parent", depth: 0, forks: 1, expanded: true },
    { id: "fork", depth: 1, forks: 0, expanded: false },
  ]);
  // Control: drop the log's pointer and the closed fork has no lineage at all —
  // which is exactly what #3368 would have shipped without the durable copy.
  const without = buildForkIndex(gatherForkPointers({ panes: [{ sessionId: "parent", forkOf: null }] }), browserRows);
  assert.equal(parentLink(without, "fork"), null);
});

test("gatherForkPointers ignores records that are not forks, and a pane with no session yet", () => {
  assert.deepEqual(
    gatherForkPointers({
      panes: [
        { sessionId: null, forkOf: "p" },
        { sessionId: "s", forkOf: null },
      ],
      roster: [{ session_id: "r" }, { session_id: "r2", forked_from: null }],
      log: [["l", {}], ["l2", { fork_of: "" }]],
    }),
    []
  );
});

// ---------- names ----------

test("a session's display name: pane, then log, then roster, then title, then the short id", () => {
  const full = {
    paneName: () => "pane",
    loggedName: () => "log",
    agentName: () => "agent",
    title: () => "title",
  };
  assert.equal(sessionDisplayName("0123456789", full), "pane");
  assert.equal(sessionDisplayName("0123456789", { ...full, paneName: () => "  " }), "log");
  assert.equal(sessionDisplayName("0123456789", { ...full, paneName: undefined, loggedName: () => undefined }), "agent");
  assert.equal(sessionDisplayName("0123456789", { title: () => "title" }), "title");
  assert.equal(sessionDisplayName("0123456789", {}), "session 01234567");
});

test("the crumb reads ↰ and the parent's name", () => {
  assert.equal(forkCrumbLabel("my work"), "↰ my work");
});

// ---------- the fork-name prompt ----------

test("Enter forks under the typed name, trimmed and one line", () => {
  assert.deepEqual(forkNameDecision("enter", "  spike:\n  retry ", "x (fork)"), { fork: true, name: "spike: retry" });
});

test("Enter on an empty box forks under the default", () => {
  assert.deepEqual(forkNameDecision("enter", "   ", "x (fork)"), { fork: true, name: "x (fork)" });
});

test("Escape forks under the default, whatever was typed", () => {
  assert.deepEqual(forkNameDecision("escape", "half-typ", "x (fork)"), { fork: true, name: "x (fork)" });
});

test("cancel is the one exit that does not fork", () => {
  assert.deepEqual(forkNameDecision("cancel", "anything", "x (fork)"), { fork: false });
});
