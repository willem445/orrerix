// Persistence round-trip + validation for the project-tab set (#63 phase 5),
// extended for the #194 session-restore schema (layout tree, restorePref,
// schemaVersion). Pure (tabstore.ts) — the localStorage/backend wiring is
// validated by hand. `npm test`.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  encodeTabs,
  decodeTabs,
  persistedKindFor,
  SCHEMA_VERSION,
  type PersistedTabs,
  type PersistedLayoutNode,
} from "../src/tabstore.ts";

test("encode → decode round-trips name / color / group / active index", () => {
  const state: PersistedTabs = {
    tabs: [
      // A bound tab now carries its group SET (#485); `groupId` stays as the
      // first of them so an older build still reads a binding.
      { name: "loomux", color: "#9ece6a", groupId: "grp-1", groupIds: ["grp-1"] },
      { name: "scratch", color: null, groupId: null },
    ],
    activeIndex: 1,
    restorePref: "restore",
  };
  const back = decodeTabs(encodeTabs(state));
  // Decode always resolves restorePref + schemaVersion (they drive Phase 4 boot).
  assert.deepEqual(back, { ...state, schemaVersion: SCHEMA_VERSION });
});

test("a tab bound to TWO groups round-trips both (#485)", () => {
  // The state #481/#478 makes reachable from the primary split gesture: one
  // tab, two orchestration groups. The old schema had room for one binding, so
  // the second group came back unrouted — its rejoined panes landing in a
  // freshly minted background tab instead of the tab they were closed in.
  const state: PersistedTabs = {
    tabs: [{ name: "two", color: null, groupId: "a", groupIds: ["a", "b"] }],
    activeIndex: 0,
    restorePref: "restore",
  };
  const back = decodeTabs(encodeTabs(state));
  assert.deepEqual(back?.tabs[0].groupIds, ["a", "b"], "both bindings survive");
  assert.equal(back?.tabs[0].groupId, "a", "the legacy field names the first, never a third value");
});

test("a pre-#485 tab (groupId only) decodes as a one-group set", () => {
  // Migration: nothing invalidates, and a single-group tab binds exactly what
  // it always did — as one entry in the set the resume path now reads.
  const back = decodeTabs(
    JSON.stringify({ tabs: [{ name: "t", color: null, groupId: "solo" }], activeIndex: 0 })
  );
  assert.deepEqual(back?.tabs[0].groupIds, ["solo"]);
  assert.equal(back?.tabs[0].groupId, "solo");
  // …and a plain tab stays plain: no bindings, and the key is omitted entirely.
  const plain = decodeTabs(JSON.stringify({ tabs: [{ name: "t", color: null }], activeIndex: 0 }));
  assert.equal(plain?.tabs[0].groupId, null);
  assert.equal(plain?.tabs[0].groupIds, undefined);
});

test("a malformed groupIds degrades to the legacy binding, never a junk group id", () => {
  // A group id is a path segment on the backend (group_dir), so a decode that
  // let `null`/`""`/a number through would be handing that to a group-scoped
  // command. Junk entries are dropped; nothing but a non-blank string survives.
  const back = decodeTabs(
    JSON.stringify({
      tabs: [{ name: "t", color: null, groupId: "keep", groupIds: [null, "", 7, "  ", "real", "real"] }],
      activeIndex: 0,
    })
  );
  assert.deepEqual(back?.tabs[0].groupIds, ["real"], "only the usable entry survives, deduped");
  const junk = decodeTabs(
    JSON.stringify({ tabs: [{ name: "t", color: null, groupId: "keep", groupIds: "nope" }], activeIndex: 0 })
  );
  assert.deepEqual(junk?.tabs[0].groupIds, ["keep"], "a non-array falls back to the legacy binding");
});

test("docked panes round-trip (captured outside the layout tree, #194 P4)", () => {
  const state: PersistedTabs = {
    tabs: [
      {
        name: "loomux",
        color: null,
        groupId: null,
        docked: [
          {
            paneKind: "agent",
            name: "claude · fix",
            cwd: "/repo",
            command: "claude --session-id abc",
            argv: null,
            shellKind: null,
            sessionId: "abc",
            role: null,
            groupId: null,
            file: null,
            sshProfileId: null,
            lead: false,
            embeds: [],
          },
        ],
      },
    ],
    activeIndex: 0,
    restorePref: "restore",
  };
  const back = decodeTabs(encodeTabs(state));
  assert.deepEqual(back?.tabs[0].docked, state.tabs[0].docked, "docked pane survives the round-trip");
});

test("an empty docked list is omitted (old-file shape preserved)", () => {
  const encoded = encodeTabs({
    tabs: [{ name: "a", color: null, groupId: null, docked: [] }],
    activeIndex: 0,
  });
  assert.equal(encoded.includes("docked"), false, "no docked key written for an empty list");
  assert.equal(decodeTabs(encoded)?.tabs[0].docked, undefined);
});

test("a malformed docked entry is dropped, not fatal to the tab", () => {
  const raw = JSON.stringify({
    tabs: [{ name: "a", color: null, groupId: null, docked: [{ paneKind: "bogus" }, { nope: 1 }] }],
    activeIndex: 0,
  });
  const back = decodeTabs(raw);
  assert.equal(back?.tabs.length, 1, "the tab survives");
  assert.equal(back?.tabs[0].docked, undefined, "all-malformed docked entries drop to no dock");
});

test("encode stamps the current schema version and defaults restorePref to ask", () => {
  // A pre-#194 snapshot object (no restorePref/schemaVersion) must still encode —
  // this is what lets main.ts keep calling encodeTabs(tabs.snapshot()) unchanged.
  const encoded = encodeTabs({ tabs: [{ name: "a", color: null, groupId: null }], activeIndex: 0 });
  const parsed = JSON.parse(encoded);
  assert.equal(parsed.schemaVersion, SCHEMA_VERSION);
  assert.equal(parsed.restorePref, "ask");
});

test("decode returns null for missing / non-JSON / shapeless input", () => {
  assert.equal(decodeTabs(null), null);
  assert.equal(decodeTabs(""), null);
  assert.equal(decodeTabs("not json {"), null);
  assert.equal(decodeTabs(JSON.stringify({ nope: 1 })), null, "no tabs array");
  assert.equal(decodeTabs(JSON.stringify({ tabs: [] })), null, "empty tab set → null (seed a fresh tab)");
});

test("decode drops malformed tab entries and coerces bad fields", () => {
  const raw = JSON.stringify({
    tabs: [
      { name: "keep", color: 123, groupId: {} }, // bad color/group → null
      { color: "#fff" }, // no name → dropped
      { name: "  " }, // blank name → dropped
      { name: "second", color: "#7aa2f7", groupId: "g" },
    ],
    activeIndex: 0,
  });
  const back = decodeTabs(raw);
  assert.deepEqual(back, {
    tabs: [
      { name: "keep", color: null, groupId: null },
      { name: "second", color: "#7aa2f7", groupId: "g", groupIds: ["g"] },
    ],
    activeIndex: 0,
    restorePref: "ask",
    schemaVersion: 1, // no version present → the pre-#194 v1 blob
  });
});

test("decode clamps an out-of-range or missing activeIndex to 0", () => {
  const mk = (activeIndex: unknown) =>
    JSON.stringify({ tabs: [{ name: "a", color: null, groupId: null }], activeIndex });
  assert.equal(decodeTabs(mk(9))?.activeIndex, 0, "beyond range → 0");
  assert.equal(decodeTabs(mk(-1))?.activeIndex, 0, "negative → 0");
  assert.equal(decodeTabs(mk("x"))?.activeIndex, 0, "non-number → 0");
  assert.equal(decodeTabs(mk(1.5))?.activeIndex, 0, "non-integer → 0");
});

// ---------- #194 migration: old files load cleanly ----------

test("an old (pre-#194) file decodes shells-only — no layout key, defaults applied", () => {
  // Exactly what a v1 file looks like: no schemaVersion, no restorePref, no layout.
  const raw = JSON.stringify({
    tabs: [{ name: "loomux", color: "#9ece6a", groupId: null }],
    activeIndex: 0,
  });
  const back = decodeTabs(raw);
  assert.deepEqual(back, {
    tabs: [{ name: "loomux", color: "#9ece6a", groupId: null }],
    activeIndex: 0,
    restorePref: "ask",
    schemaVersion: 1,
  });
  // Migration contract: no `layout` property is invented on an old tab.
  assert.ok(!("layout" in back!.tabs[0]), "old tab has no layout key");
});

// ---------- #194 layout tree ----------

const NESTED_LAYOUT: PersistedLayoutNode = {
  kind: "split",
  dir: "row",
  weight: 1,
  children: [
    {
      kind: "leaf",
      weight: 1,
      pane: {
        paneKind: "terminal",
        name: "shell",
        cwd: "/repo",
        command: null,
        argv: null,
        shellKind: "gitbash",
        sessionId: null,
        role: null,
        groupId: null,
        file: null,
        sshProfileId: null,
        lead: false,
        embeds: [],
      },
    },
    {
      kind: "split",
      dir: "column",
      weight: 2,
      children: [
        {
          kind: "leaf",
          weight: 1,
          pane: {
            paneKind: "agent",
            name: "claude",
            cwd: "/repo",
            command: "claude",
            argv: ["claude", "--resume", "abc-123"],
            shellKind: null,
            sessionId: "abc-123",
            role: null,
            groupId: null,
            file: null,
            sshProfileId: null,
            lead: false,
            embeds: [],
          },
        },
        {
          kind: "leaf",
          weight: 1,
          pane: {
            paneKind: "orch",
            name: "orchestrator",
            cwd: "/repo",
            command: null,
            argv: null,
            shellKind: null,
            sessionId: "orch-sess-9",
            role: "orchestrator",
            groupId: null,
            file: null,
            sshProfileId: null,
            lead: false,
            embeds: [],
          },
        },
        {
          // A file-explorer pane (#214): its root rides in `cwd`, and every
          // spawn-shaped field is null — it has no process to describe.
          kind: "leaf",
          weight: 1,
          pane: {
            paneKind: "files",
            name: "loomux",
            cwd: "C:/Projects/loomux",
            command: null,
            argv: null,
            shellKind: null,
            sessionId: null,
            role: null,
            groupId: null,
            file: null,
            sshProfileId: null,
            lead: false,
            embeds: [],
          },
        },
      ],
    },
  ],
};

test("layout tree round-trips exactly (nested split, weights, all pane kinds)", () => {
  const state: PersistedTabs = {
    tabs: [{ name: "loomux", color: null, groupId: "g", layout: NESTED_LAYOUT }],
    activeIndex: 0,
    restorePref: "restore",
  };
  const back = decodeTabs(encodeTabs(state));
  assert.deepEqual(back?.tabs[0].layout, NESTED_LAYOUT);
});

// ---------- #214 file-explorer leaves ----------

test("a files leaf round-trips its root — and needed NO new field or schema bump", () => {
  const files: PersistedPane = {
    paneKind: "files",
    name: "loomux",
    cwd: "C:/Projects/loomux",
    command: null,
    argv: null,
    shellKind: null,
    sessionId: null,
    role: null,
    groupId: null,
    file: null,
    sshProfileId: null,
    lead: false,
    embeds: [],
  };
  const state: PersistedTabs = {
    tabs: [
      { name: "t", color: null, groupId: null, layout: { kind: "leaf", weight: 1, pane: files } },
    ],
    activeIndex: 0,
  };
  const back = decodeTabs(encodeTabs(state));
  const leaf = back?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.deepEqual(leaf.pane, files);
  // The root rides in the EXISTING `cwd`, exactly as `role` rode in for orch panes —
  // so the decoder stays shape-driven and v2 files (which simply never contain a
  // "files" leaf) still decode unchanged. A bump here would be a false signal.
  assert.equal(back?.schemaVersion, 2);
});

test("a files leaf with no root decodes (null) rather than dropping the whole tab layout", () => {
  // The strict whole-tree fail-safe is for MALFORMED data. A rootless files leaf is
  // well-formed but unrestorable, and killing the entire tab's layout over it would
  // punish every sibling pane. It decodes, and restore fails soft in that ONE slot
  // (planPaneRestore → open-files with root null → main.ts opens the welcome form).
  const raw = JSON.stringify({
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: {
          kind: "split",
          dir: "row",
          weight: 1,
          children: [
            { kind: "leaf", weight: 1, pane: { paneKind: "terminal", name: "shell" } },
            { kind: "leaf", weight: 1, pane: { paneKind: "files", name: "files" } }, // no cwd
          ],
        },
      },
    ],
    activeIndex: 0,
  });
  const layout = decodeTabs(raw)?.tabs[0].layout;
  assert.ok(layout?.kind === "split");
  assert.equal(layout.children.length, 2, "the sibling terminal survives");
  const filesLeaf = layout.children[1];
  assert.ok(filesLeaf.kind === "leaf");
  assert.equal(filesLeaf.pane.paneKind, "files");
  assert.equal(filesLeaf.pane.cwd, null);
});

// ---------- #217 editor + git leaves ----------

test("editor and git leaves round-trip their root — and the editor's open FILE", () => {
  // The third and fourth members of the same family (#214's files was the first). The
  // editor's FOLDER and the git pane's REPO both ride in the existing `cwd`; the editor
  // also carries the file it was showing — a PATH, never a buffer (#217). Both are
  // additive in exactly the way `role` and the files root were: a decoder that has never
  // heard of them is not needed, because old snapshots simply never carry them.
  const mk = (paneKind: PersistedPane["paneKind"], file: string | null = null): PersistedPane => ({
    paneKind,
    name: "loomux",
    cwd: "C:/Projects/loomux",
    command: null,
    argv: null,
    shellKind: null,
    sessionId: null,
    role: null,
    groupId: null,
    file,
    sshProfileId: null,
    lead: false,
    embeds: [],
  });
  const state: PersistedTabs = {
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: {
          kind: "split",
          dir: "row",
          weight: 1,
          children: [
            { kind: "leaf", weight: 1, pane: mk("editor", "src/pane.ts") },
            { kind: "leaf", weight: 1, pane: mk("git") },
          ],
        },
      },
    ],
    activeIndex: 0,
  };
  const back = decodeTabs(encodeTabs(state));
  const layout = back?.tabs[0].layout;
  assert.ok(layout?.kind === "split");
  assert.deepEqual(layout.children[0].kind === "leaf" && layout.children[0].pane, mk("editor", "src/pane.ts"));
  assert.deepEqual(layout.children[1].kind === "leaf" && layout.children[1].pane, mk("git"));
  assert.equal(back?.schemaVersion, 2, "additive — a bump here would be a false signal");
});

test("a rootless editor/git leaf decodes (null) rather than dropping the whole tab layout", () => {
  // Same fail-soft as the files leaf: well-formed but unrestorable is NOT malformed, and
  // killing the tab's whole layout over one such leaf would punish every sibling pane.
  const raw = JSON.stringify({
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: {
          kind: "split",
          dir: "row",
          weight: 1,
          children: [
            { kind: "leaf", weight: 1, pane: { paneKind: "terminal", name: "shell" } },
            { kind: "leaf", weight: 1, pane: { paneKind: "editor", name: "editor" } }, // no cwd
            { kind: "leaf", weight: 1, pane: { paneKind: "git", name: "git" } }, // no cwd
          ],
        },
      },
    ],
    activeIndex: 0,
  });
  const layout = decodeTabs(raw)?.tabs[0].layout;
  assert.ok(layout?.kind === "split");
  assert.equal(layout.children.length, 3, "the sibling terminal survives");
  for (const [i, kind] of [
    [1, "editor"],
    [2, "git"],
  ] as const) {
    const leaf = layout.children[i];
    assert.ok(leaf.kind === "leaf");
    assert.equal(leaf.pane.paneKind, kind);
    assert.equal(leaf.pane.cwd, null);
  }
});

test("a malformed layout node degrades that tab's whole layout to null (not a throw)", () => {
  const raw = JSON.stringify({
    tabs: [
      {
        name: "loomux",
        color: null,
        groupId: null,
        layout: {
          kind: "split",
          dir: "row",
          weight: 1,
          children: [
            { kind: "leaf", weight: 1, pane: { paneKind: "terminal", name: "ok" } },
            { kind: "leaf", weight: 1, pane: { paneKind: "bogus", name: "bad" } }, // invalid kind
          ],
        },
      },
    ],
    activeIndex: 0,
  });
  const back = decodeTabs(raw);
  // The tab survives — only its layout drops to null (restores as a fresh shell).
  assert.equal(back?.tabs.length, 1);
  assert.equal(back?.tabs[0].layout, null);
});

test("a leaf with no pane, an empty split, and a bad root all degrade to null", () => {
  const mk = (layout: unknown) =>
    decodeTabs(JSON.stringify({ tabs: [{ name: "t", color: null, groupId: null, layout }], activeIndex: 0 }))
      ?.tabs[0].layout;
  assert.equal(mk({ kind: "leaf", weight: 1 }), null, "leaf missing pane");
  assert.equal(mk({ kind: "split", dir: "row", weight: 1, children: [] }), null, "empty split");
  assert.equal(mk({ kind: "split", dir: "sideways", weight: 1, children: [] }), null, "bad dir");
  assert.equal(mk({ kind: "nonsense" }), null, "unknown node kind");
});

test("malformed pane fields inside a valid leaf coerce to null, not a drop", () => {
  const layout = {
    kind: "leaf",
    weight: "heavy", // bad weight → default 1
    pane: {
      paneKind: "agent",
      name: "claude",
      cwd: 42, // bad → null
      command: "claude",
      argv: ["ok", 7], // non-string element → whole argv null
      shellKind: "fish", // unknown → null
      sessionId: null,
      role: 99, // non-string → null
      file: 7, // non-string → null (#217)
      embeds: "0.4", // not an array → [] (#361)
    },
  };
  const back = decodeTabs(
    JSON.stringify({ tabs: [{ name: "t", color: null, groupId: null, layout }], activeIndex: 0 })
  );
  assert.deepEqual(back?.tabs[0].layout, {
    kind: "leaf",
    weight: 1,
    pane: {
      paneKind: "agent",
      name: "claude",
      cwd: null,
      command: "claude",
      argv: null,
      shellKind: null,
      sessionId: null,
      role: null,
      groupId: null,
      file: null,
      sshProfileId: null,
      lead: false,
      embeds: [],
    },
  });
});

// ---------- #361 embedded views (multi-slot: left/right/bottom) ----------

test("embed preferences ({view, side, share}), one per docked edge, round-trip through encode/decode", () => {
  const orch: PersistedPane = {
    paneKind: "orch",
    name: "orchestrator",
    cwd: "/repo",
    command: null,
    argv: null,
    shellKind: null,
    sessionId: "orch-1",
    role: "orchestrator",
    groupId: null,
    file: null,
    sshProfileId: null,
    lead: false,
    embeds: [
      { view: "group", side: "bottom", share: 0.42 },
      { view: "tasks", side: "left", share: 0.3 },
    ],
  };
  const state: PersistedTabs = {
    tabs: [
      { name: "t", color: null, groupId: "g", layout: { kind: "leaf", weight: 1, pane: orch } },
    ],
    activeIndex: 0,
  };
  const back = decodeTabs(encodeTabs(state));
  const leaf = back?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.deepEqual(leaf.pane.embeds, [
    { view: "group", side: "bottom", share: 0.42 },
    { view: "tasks", side: "left", share: 0.3 },
  ]);
});

test("git and editor are valid embed views too (#361 scope increase), round-trip like any other kind", () => {
  const orch: PersistedPane = {
    paneKind: "orch",
    name: "orchestrator",
    cwd: "/repo",
    command: null,
    argv: null,
    shellKind: null,
    sessionId: "orch-1",
    role: "orchestrator",
    groupId: null,
    file: null,
    sshProfileId: null,
    lead: false,
    embeds: [
      { view: "git", side: "left", share: 0.35 },
      { view: "editor", side: "right", share: 0.4 },
    ],
  };
  const state: PersistedTabs = {
    tabs: [
      { name: "t", color: null, groupId: "g", layout: { kind: "leaf", weight: 1, pane: orch } },
    ],
    activeIndex: 0,
  };
  const back = decodeTabs(encodeTabs(state));
  const leaf = back?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.deepEqual(leaf.pane.embeds, [
    { view: "git", side: "left", share: 0.35 },
    { view: "editor", side: "right", share: 0.4 },
  ]);
});

test("the progress timeline (#608) is a valid embed view and round-trips like any other kind", () => {
  const orch: PersistedPane = {
    paneKind: "orch",
    name: "orchestrator",
    cwd: "/repo",
    command: null,
    argv: null,
    shellKind: null,
    sessionId: "orch-1",
    role: "orchestrator",
    groupId: null,
    file: null,
    sshProfileId: null,
    lead: false,
    embeds: [{ view: "timeline", side: "bottom", share: 0.45 }],
  };
  const state: PersistedTabs = {
    tabs: [
      { name: "t", color: null, groupId: "g", layout: { kind: "leaf", weight: 1, pane: orch } },
    ],
    activeIndex: 0,
  };
  const back = decodeTabs(encodeTabs(state));
  const leaf = back?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.deepEqual(leaf.pane.embeds, [{ view: "timeline", side: "bottom", share: 0.45 }]);
});

test("a snapshot written BEFORE #608 still decodes its other embeds unchanged", () => {
  // The additive half of the compatibility contract: nothing about an older
  // file changes just because a new value became representable.
  const raw = JSON.stringify({
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: {
          kind: "leaf",
          weight: 1,
          pane: {
            paneKind: "orch",
            name: "orchestrator",
            role: "orchestrator",
            embeds: [{ view: "audit", side: "right", share: 0.5 }],
          },
        },
      },
    ],
    activeIndex: 0,
  });
  const leaf = decodeTabs(raw)?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.deepEqual(leaf.pane.embeds, [{ view: "audit", side: "right", share: 0.5 }]);
});

test("an UNKNOWN embed view from a newer build drops that entry, never the pane", () => {
  // The other direction of the same contract: an older loomux reading a
  // snapshot a newer one wrote must lose only the slot it cannot show. This is
  // exactly the path `timeline` itself takes on a pre-#608 build.
  const raw = JSON.stringify({
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: {
          kind: "leaf",
          weight: 1,
          pane: {
            paneKind: "orch",
            name: "orchestrator",
            role: "orchestrator",
            embeds: [
              { view: "hologram", side: "left", share: 0.3 }, // a kind from the future
              { view: "timeline", side: "bottom", share: 0.4 },
            ],
          },
        },
      },
    ],
    activeIndex: 0,
  });
  const decoded = decodeTabs(raw);
  const leaf = decoded?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf", "the pane must survive an unknown embed kind");
  assert.equal(decoded?.tabs.length, 1);
  assert.deepEqual(leaf.pane.embeds, [{ view: "timeline", side: "bottom", share: 0.4 }]);
});

test("an old snapshot with no embeds key decodes it as [] (overlay mode, unchanged)", () => {
  // A pre-#361 file never wrote the key at all — additive, like `role` and the
  // files root before it: no schema bump, no decoder branch needed.
  const raw = JSON.stringify({
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: {
          kind: "leaf",
          weight: 1,
          pane: { paneKind: "orch", name: "orchestrator", role: "orchestrator" },
        },
      },
    ],
    activeIndex: 0,
  });
  const leaf = decodeTabs(raw)?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.deepEqual(leaf.pane.embeds, []);
});

test("a malformed entry inside a valid embeds array is dropped, not the whole array", () => {
  const raw = JSON.stringify({
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: {
          kind: "leaf",
          weight: 1,
          pane: {
            paneKind: "orch",
            name: "orchestrator",
            role: "orchestrator",
            groupId: null,
            embeds: [
              { view: "group", side: "bottom", share: 0.4 }, // valid
              { view: "issues", side: "left", share: 0.3 }, // not a RESTORABLE kind
              { view: "tasks", side: "sideways", share: 0.3 }, // bad side
              { view: "audit", side: "right", share: "big" }, // bad share
              "not even an object",
            ],
          },
        },
      },
    ],
    activeIndex: 0,
  });
  const leaf = decodeTabs(raw)?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.deepEqual(leaf.pane.embeds, [{ view: "group", side: "bottom", share: 0.4 }]);
});

test("two entries claiming the SAME side: the first wins, the second is dropped", () => {
  const raw = JSON.stringify({
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: {
          kind: "leaf",
          weight: 1,
          pane: {
            paneKind: "orch",
            name: "orchestrator",
            role: "orchestrator",
            groupId: null,
            embeds: [
              { view: "tasks", side: "left", share: 0.3 },
              { view: "audit", side: "left", share: 0.5 }, // same side, stale/malformed data
            ],
          },
        },
      },
    ],
    activeIndex: 0,
  });
  const leaf = decodeTabs(raw)?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.deepEqual(leaf.pane.embeds, [{ view: "tasks", side: "left", share: 0.3 }]);
});

test("a legacy single-slot embed:{view,share} (pre-multi-slot #361 shape) migrates to bottom", () => {
  // The single-embed-slot shape never shipped in a release (generalized to
  // multiple sides within the same PR) — but decode stays lenient
  // regardless: bottom was the only side that shape could ever mean.
  const raw = JSON.stringify({
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: {
          kind: "leaf",
          weight: 1,
          pane: {
            paneKind: "orch",
            name: "orchestrator",
            role: "orchestrator",
            groupId: null,
            embed: { view: "audit", share: 0.5 },
          },
        },
      },
    ],
    activeIndex: 0,
  });
  const leaf = decodeTabs(raw)?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.deepEqual(leaf.pane.embeds, [{ view: "audit", side: "bottom", share: 0.5 }]);
});

test("a legacy taskEmbed:number (pre-generalization #361 shape, oldest of the three) migrates to [{tasks, bottom}]", () => {
  // taskEmbed never shipped in a release either (renamed, then generalized,
  // within the same PR, #404's review rounds) — decode stays lenient for
  // the same reason: the cost of tolerating an old shape is a few lines,
  // the cost of not is a silently dropped preference on the next boot after
  // a stray hand-edited or pre-rebase tabs.json.
  const raw = JSON.stringify({
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: {
          kind: "leaf",
          weight: 1,
          pane: { paneKind: "orch", name: "orchestrator", role: "orchestrator", taskEmbed: 0.3 },
        },
      },
    ],
    activeIndex: 0,
  });
  const leaf = decodeTabs(raw)?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.deepEqual(leaf.pane.embeds, [{ view: "tasks", side: "bottom", share: 0.3 }]);
});

test("the newest present shape wins when a pane somehow carries more than one", () => {
  const raw = JSON.stringify({
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: {
          kind: "leaf",
          weight: 1,
          pane: {
            paneKind: "orch",
            name: "orchestrator",
            role: "orchestrator",
            groupId: null,
            embeds: [{ view: "group", side: "right", share: 0.6 }],
            embed: { view: "audit", share: 0.5 }, // stale — ignored when `embeds` is present
            taskEmbed: 0.9, // stalest — ignored too
          },
        },
      },
    ],
    activeIndex: 0,
  });
  const leaf = decodeTabs(raw)?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.deepEqual(leaf.pane.embeds, [{ view: "group", side: "right", share: 0.6 }]);
});

test("restorePref and schemaVersion coerce unknown values to safe defaults", () => {
  const mk = (extra: object) =>
    decodeTabs(JSON.stringify({ tabs: [{ name: "t", color: null, groupId: null }], activeIndex: 0, ...extra }));
  assert.equal(mk({ restorePref: "restore" })?.restorePref, "restore", "valid pref kept");
  assert.equal(mk({ restorePref: "fresh" })?.restorePref, "fresh", "valid pref kept");
  assert.equal(mk({ restorePref: "maybe" })?.restorePref, "ask", "unknown pref → ask");
  assert.equal(mk({ restorePref: 7 })?.restorePref, "ask", "non-string pref → ask");
  assert.equal(mk({ schemaVersion: 2 })?.schemaVersion, 2, "valid version kept");
  assert.equal(mk({ schemaVersion: "two" })?.schemaVersion, 1, "non-number version → 1");
});

// ---------- #887 S4: SSH pane leaves ----------

test("an ssh leaf round-trips its connection + recorded session, and carries NO command line", () => {
  // The whole restore record for an SSH pane: which saved connection it belongs
  // to, and the remote session id (claude only) a Reconnect can resume. The
  // command line is deliberately absent — reconnect re-derives it from the
  // profile, so a connection edited between boots reconnects with the edit and a
  // recorded `--session-id` is never replayed into the session it already made.
  const ssh: PersistedPane = {
    paneKind: "ssh",
    name: "build box",
    cwd: null,
    command: null,
    argv: null,
    shellKind: null,
    sessionId: "remote-sess-1",
    role: null,
    groupId: null,
    file: null,
    sshProfileId: "prof-7",
    lead: false,
    embeds: [],
  };
  const state: PersistedTabs = {
    tabs: [
      { name: "t", color: null, groupId: null, layout: { kind: "leaf", weight: 1, pane: ssh } },
    ],
    activeIndex: 0,
  };
  const back = decodeTabs(encodeTabs(state));
  const leaf = back?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf", "the ssh leaf survives — an unknown kind would collapse the tab's layout");
  assert.deepEqual(leaf.pane, ssh);
  // Additive and shape-driven, exactly like the content kinds before it: a v2
  // file that predates SSH panes simply never carries one, so the version does
  // not move.
  assert.equal(back?.schemaVersion, SCHEMA_VERSION);
});

test("an ssh leaf with no recorded profile decodes to null rather than failing its tab", () => {
  // A hand-edited (or pre-S4) record naming no connection. Failing the ENTRY here
  // would take the whole tab's layout down with it (decodeLayout's whole-tree
  // fail-safe) — losing every other pane in the tab over one unreconnectable
  // card. It decodes, and restore renders a card that says it has nothing to
  // reconnect to.
  const raw = JSON.stringify({
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: { kind: "leaf", weight: 1, pane: { paneKind: "ssh", name: "box" } },
      },
    ],
    activeIndex: 0,
  });
  const leaf = decodeTabs(raw)?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.equal(leaf.pane.paneKind, "ssh");
  assert.equal(leaf.pane.sshProfileId, null);
});

test("a blank sshProfileId reads as absent, never as an id that matches no profile", () => {
  // Same reason `groupId` treats blank as absent: the value is looked up in a
  // store, and "" is not a lookup — it is a miss wearing the shape of a hit.
  const raw = JSON.stringify({
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: {
          kind: "leaf",
          weight: 1,
          pane: { paneKind: "ssh", name: "box", sshProfileId: "   " },
        },
      },
    ],
    activeIndex: 0,
  });
  const leaf = decodeTabs(raw)?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.equal(leaf.pane.sshProfileId, null);
});

test("sshProfileId is null on every non-ssh leaf (nothing else grew a connection)", () => {
  const raw = JSON.stringify({
    tabs: [
      {
        name: "t",
        color: null,
        groupId: null,
        layout: {
          kind: "leaf",
          weight: 1,
          pane: { paneKind: "terminal", name: "shell", shellKind: "cmd" },
        },
      },
    ],
    activeIndex: 0,
  });
  const leaf = decodeTabs(raw)?.tabs[0].layout;
  assert.ok(leaf?.kind === "leaf");
  assert.equal(leaf.pane.sshProfileId, null);
});

// ---------- the lead flag (#2519 C2) ----------

test("a lead pane's flag round-trips, and only an exact `true` is one", () => {
  // Default-OFF with the same polarity as the launcher toggle that mints one
  // (`subagentsFromStored`), and for the same reason: a corrupted or
  // hand-edited snapshot must not silently mint a real orchestration group,
  // with a cap's worth of live agents, on the next boot. Every value that is
  // not the boolean `true` reads false — including the STRING "true", which is
  // what a hand-edit is most likely to write.
  const leaf = (lead: unknown) => ({
    kind: "leaf",
    weight: 1,
    pane: {
      paneKind: "agent",
      name: "lead",
      cwd: "/repo",
      command: "claude",
      argv: null,
      shellKind: null,
      sessionId: "s-1",
      role: null,
      groupId: null,
      file: null,
      sshProfileId: null,
      lead,
      embeds: [],
    },
  });
  const decode = (lead: unknown) => {
    const back = decodeTabs(
      JSON.stringify({ tabs: [{ name: "t", color: null, groupId: null, layout: leaf(lead) }], activeIndex: 0 })
    );
    const node = back?.tabs[0].layout;
    assert.equal(node?.kind, "leaf", "the leaf decoded at all (positive control)");
    return node?.kind === "leaf" ? node.pane.lead : undefined;
  };
  assert.equal(decode(true), true, "a real lead comes back as one");
  assert.equal(decode(false), false);
  assert.equal(decode(undefined), false, "every pre-#2519 snapshot");
  assert.equal(decode("true"), false, "a hand-edited string is not a lead");
  assert.equal(decode(1), false);
});

// ---------- `persistedKindFor`: what a live pane comes back AS (#2519 C2) ----------

const LIVE = {
  contentKind: null,
  structured: false,
  ssh: false,
  orchRole: null,
  orchGroup: null,
  launchedCommand: false,
} as const;

test("a LEAD persists as the agent pane it is, never as a resumable orch placeholder", () => {
  // The whole restore contract for a lead, and the reason it is a rung of its
  // own: `orch` means "a member of a group a whole-group RESUME brings back",
  // and a lead group cannot be resumed. Persisted that way it would return as a
  // Resume button that can only fail, with the human's command line discarded.
  const lead = { ...LIVE, orchRole: "lead", orchGroup: "g-lead", launchedCommand: true };
  assert.equal(persistedKindFor(lead), "agent");
  // The operands COLLIDE, which is what makes the assertion above fail-able:
  // this pane really does carry a group, so a rung that read `orchGroup` first
  // would answer "orch" for it. The control is the same pane one field over.
  assert.equal(persistedKindFor({ ...lead, orchRole: "worker" }), "orch", "any OTHER role in a group is orch");
});

test("a structured pane persists as the agent it is, never as ssh or orch", () => {
  // #2891 S4's rung. A structured pane has no PTY — so every reflex says "content"
  // — but it is a view of ONE AGENT's event log, and the four content kinds restore
  // from their root alone, which for a transcript restores nothing.
  //
  // THE OPERANDS COLLIDE, which is what makes this fail-able (#1300): the fixture
  // carries `ssh` AND a group AND a launched command, so every rung below the
  // structured one would answer something else. Delete `if (pane.structured)` from
  // the ladder and this row goes to "ssh"; a disjoint fixture would hold under both.
  const structured = {
    ...LIVE,
    structured: true,
    ssh: true,
    orchGroup: "g",
    launchedCommand: true,
  };
  assert.equal(persistedKindFor(structured), "agent");
  // The control is the same pane one field over — so this pair fails if the rung
  // stops reading `structured`, and also if it starts answering "agent" for
  // everything.
  assert.equal(
    persistedKindFor({ ...structured, structured: false }),
    "ssh",
    "without the flag the very same pane takes the rung below"
  );
  // And it does NOT outrank content: a files/editor/git pane that somehow carried
  // the flag is still a content pane, because `contentKind` is the first rung.
  assert.equal(
    persistedKindFor({ ...structured, contentKind: "editor" }),
    "editor",
    "content is still the first rung"
  );
});

test("the persisted-kind ladder answers each rung, with the rung below it varied", () => {
  // Fixture-per-rung (#1182): each row differs from the one that would win
  // beneath it, so a rung deleted from the ladder reddens exactly its own row
  // rather than being masked by an arm further down.
  assert.equal(
    persistedKindFor({ ...LIVE, contentKind: "editor", ssh: true, orchGroup: "g", launchedCommand: true }),
    "editor",
    "content outranks everything — it has no process at all"
  );
  assert.equal(
    persistedKindFor({ ...LIVE, ssh: true, orchRole: "lead", orchGroup: "g" }),
    "ssh",
    "ssh outranks lead and orch (the #887/#888 boundary, belt: an ssh pane can hold neither)"
  );
  assert.equal(
    persistedKindFor({ ...LIVE, ssh: true }),
    "ssh",
    "…and outranks terminal, which is the fallthrough it exists to prevent: an ssh pane launches an argv, so `launchedCommand` is false for it"
  );
  assert.equal(persistedKindFor({ ...LIVE, orchGroup: "g" }), "orch");
  assert.equal(persistedKindFor({ ...LIVE, launchedCommand: true }), "agent");
  assert.equal(persistedKindFor(LIVE), "terminal", "a bare shell");
});
