// To-Do v2 (#3335): the DOM-free decisions behind the per-item colour, the tag
// hues and rail counts, the priority control and its sort, drag-reorder, and
// click-the-row-to-expand. The DOM wiring in `src/todopane.ts` is validated by
// hand (CLAUDE.md: DOM glue is not simulated in tests); everything it DECIDES
// is here.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { IDENTITY } from "../src/theme.ts";
import { normalizeTag, parseQuickAdd } from "../src/todoquickadd.ts";
import {
  PRIORITY_LABEL,
  TAG_HUES,
  TODO_COLORS,
  canReorder,
  colorOf,
  decodeSnapshot,
  dropTarget,
  groupPlanned,
  inverseOp,
  nextPriority,
  tagHue,
  visibleItems,
  type TodoItem,
} from "../src/todomodel.ts";
import {
  decodeTodoPrefs,
  encodeTodoPrefs,
  projectPane,
  rowClickToggles,
  sortsByPriority,
  togglePrioritySort,
  DEFAULT_TODO_PREFS,
  DropClickGuard,
} from "../src/todoview.ts";

const read = (rel: string) => readFileSync(new URL(rel, import.meta.url), "utf8");

/** Wednesday 2024-05-15, 10:00 local — the anchor the other todo suites use. */
const NOW = new Date(2024, 4, 15, 10, 0, 0, 0).getTime();
const DAY = 86400000;

let seq = 0;
function item(over: Partial<TodoItem> = {}): TodoItem {
  seq += 1;
  return {
    id: `td-${seq}`,
    scope: "global",
    title: `item ${seq}`,
    notes: "",
    status: "open",
    done_ms: null,
    due_ms: null,
    remind_ms: null,
    my_day: null,
    priority: 0,
    important: false,
    tags: [],
    color: null,
    steps: [],
    order: seq * 1024,
    created_ms: NOW,
    created_by: { kind: "human" },
    updated_ms: NOW,
    updated_by: { kind: "human" },
    rev: 1,
    archived_ms: null,
    deleted_ms: null,
    ...over,
  };
}

const ids = (xs: readonly TodoItem[]) => xs.map((i) => i.id);

// ---------- the per-item colour ----------

test("the frontend's colour list IS the engine's COLORS, read off the Rust source", () => {
  // Mirrored, not imported — two languages. So the mirror is checked against
  // the file the engine compiles, and a colour added to one side alone reddens.
  const src = read("../crates/loomux-engine/src/todo.rs");
  const m = /pub const COLORS: &\[&str\] = &\[([^\]]*)\];/.exec(src);
  assert.ok(m, "could not find `pub const COLORS` in the engine — the scan is blind, not the lists equal");
  const rust = [...m[1].matchAll(/"([^"]*)"/g)].map((x) => x[1]);
  // Population control: the raw count of string literals in the slice must
  // equal what the census extracted, so a literal the regex cannot read is a
  // failure rather than a silently shorter list.
  assert.equal(rust.length, (m[1].match(/"/g) ?? []).length / 2);
  assert.deepEqual([...TODO_COLORS], rust);
});

test("every colour an item may carry is painted by an identity token, and only those", () => {
  // The per-item stripe names `--id-<colour>`; a colour with no IDENTITY entry
  // would name a token the stylesheet does not declare and draw nothing.
  const identity = Object.keys(IDENTITY);
  assert.ok(identity.length >= 8, "IDENTITY lost its hues — the check is blind");
  for (const c of TODO_COLORS) assert.ok(identity.includes(c), `${c} is not an identity hue`);
  assert.equal(new Set(TODO_COLORS).size, TODO_COLORS.length, "no colour listed twice");
});

test("decode carries a colour, and reads an absent or malformed one as none", () => {
  const snap = decodeSnapshot({
    items: [
      { id: "a", title: "coloured", color: "azure" },
      { id: "b", title: "absent" },
      { id: "c", title: "not a string", color: 7 },
      { id: "d", title: "empty", color: "" },
      { id: "e", title: "from a newer build", color: "teal" },
    ],
  });
  assert.deepEqual(
    snap.items.map((i) => i.color),
    ["azure", null, null, null, "teal"],
    "an unknown NAME is kept (a newer build wrote it) — only the renderer ignores it"
  );
  assert.deepEqual(snap.items.map(colorOf), ["azure", null, null, null, null]);
});

test("undoing a colour puts back exactly the colour before — including none", () => {
  const before = item({ color: "jade" });
  const set = inverseOp({ update: { id: before.id, color: "rose" } }, before, before);
  assert.deepEqual(set, { op: { update: { id: before.id, color: "jade" } } });

  const bare = item();
  const first = inverseOp({ update: { id: bare.id, color: "rose" } }, bare, bare);
  assert.deepEqual(
    first,
    { op: { update: { id: bare.id, color: null } } },
    "an item that had NO colour undoes to an explicit clear, not to an absent key"
  );

  // Non-interference: an update that did not name the colour does not invert it.
  const titled = inverseOp({ update: { id: before.id, title: "x" } }, before, before);
  assert.ok("op" in titled && "update" in titled.op);
  assert.equal("color" in (titled.op as { update: object }).update, false);
});

// ---------- tag hues ----------

test("a tag's hue is stable: pinned values, so a hash change cannot reshuffle every list", () => {
  // Goldens, measured once. A change to the hash or to the table's order would
  // repaint every tag a human has learned to recognise, and this is what says so.
  assert.deepEqual(
    ["release", "infra", "bug", "ops-2", "work", "docs"].map(tagHue),
    ["lime", "azure", "cyan", "violet", "orchid", "azure"]
  );
  assert.equal(tagHue("release"), tagHue("release"));
});

test("every tag hue resolves to a listed identity token — never a hash-to-hue", () => {
  // THE POPULATION CONTROL the channel rule asks for: not "no literal colour",
  // but that every value `tagHue` can return is a key of theme.ts's IDENTITY.
  const identity = Object.keys(IDENTITY);
  for (const h of TAG_HUES) assert.ok(identity.includes(h), `${h} is not an identity hue`);
  // The state-sharing hues are excluded on purpose (see TAG_HUES's doc): an
  // amber tag beside an amber overdue date would say two things in one pigment.
  for (const h of ["rose", "amber", "jade"]) {
    assert.equal((TAG_HUES as readonly string[]).includes(h), false, `${h} doubles as a state dye`);
  }
  // And over a real population of tags every returned hue is in the table,
  // AND the hash actually spreads: a function returning one constant hue would
  // pass the membership check above.
  const seen = new Set<string>();
  for (let n = 0; n < 500; n++) {
    const h = tagHue(`tag-${n}`);
    assert.ok((TAG_HUES as readonly string[]).includes(h), `tag-${n} got ${h}`);
    seen.add(h);
  }
  assert.equal(seen.size, TAG_HUES.length, "500 tags must reach every tag hue");
});

test("the row's tag field reads the quick-add's tag grammar, not a second one", () => {
  assert.equal(normalizeTag("#Release"), "release");
  assert.equal(normalizeTag("  ops-2 "), "ops-2");
  assert.equal(normalizeTag("#"), null);
  assert.equal(normalizeTag("two words"), null);
  assert.equal(normalizeTag("##x"), null);
  // The same answer the quick-add gives for the same token.
  assert.deepEqual(parseQuickAdd("ship it #Release", NOW).tags, [normalizeTag("#Release")]);
});

// ---------- the tag rail ----------

test("the rail counts OPEN items per tag, most-used first, ties by name", () => {
  const items = [
    item({ tags: ["release", "infra"] }),
    item({ tags: ["infra"] }),
    item({ tags: ["infra", "infra"] }), // a duplicate is still ONE item
    item({ tags: ["bug"] }),
    item({ tags: ["release"], status: "done", done_ms: NOW }), // done: not counted
    item({ tags: ["zed"] }),
  ];
  const p = projectPane({ items, view: "all", query: "", tagFilter: null }, NOW);
  assert.deepEqual(p.tags, [
    { tag: "infra", count: 3 },
    { tag: "bug", count: 1 },
    { tag: "release", count: 1 },
    { tag: "zed", count: 1 },
  ]);
  // The count is what the filter then shows.
  const f = projectPane({ items, view: "all", query: "", tagFilter: "infra" }, NOW);
  assert.equal(f.total, 3);
});

// ---------- priority ----------

test("the priority control steps None → Low → Medium → High → None, over the ONE priority", () => {
  assert.deepEqual([0, 1, 2, 3].map(nextPriority), [1, 2, 3, 0]);
  assert.deepEqual([...PRIORITY_LABEL], ["None", "Low", "Medium", "High"]);
  // The quick-add's `!!` writes the same field the control cycles.
  assert.equal(parseQuickAdd("x !!", NOW).priority, 2);
  assert.equal(PRIORITY_LABEL[parseQuickAdd("x !!!", NOW).priority], "High");
  // A value this build does not know is treated as None, so one click lands legal.
  assert.equal(nextPriority(7), 1);
  assert.equal(nextPriority(-1), 1);
  assert.equal(nextPriority(1.5), 1);
});

test("the priority sort lifts by level and keeps the MANUAL order inside a level", () => {
  const a = item({ order: 1, priority: 0 });
  const b = item({ order: 2, priority: 3 });
  const c = item({ order: 3, priority: 1 });
  const d = item({ order: 4, priority: 3 });
  const all = [d, c, b, a];
  assert.deepEqual(ids(visibleItems(all, { view: "all" }, NOW)), ids([a, b, c, d]), "manual by default");
  assert.deepEqual(
    ids(visibleItems(all, { view: "all", byPriority: true }, NOW)),
    ids([b, d, c, a]),
    "High first (b before d by manual order), then Low, then None"
  );
  // Off again restores the manual order: nothing was written.
  assert.deepEqual(ids(visibleItems(all, { view: "all", byPriority: false }, NOW)), ids([a, b, c, d]));
});

test("Completed ignores the priority sort; Planned keeps its buckets and sorts inside them", () => {
  const early = item({ status: "done", done_ms: NOW - DAY, priority: 3 });
  const late = item({ status: "done", done_ms: NOW, priority: 0 });
  assert.deepEqual(
    ids(visibleItems([early, late], { view: "completed", byPriority: true }, NOW)),
    ids([late, early]),
    "Completed is a log — newest finish first, whatever the priority"
  );

  const todayLow = item({ due_ms: NOW + 3600000, priority: 0 });
  const todayHigh = item({ due_ms: NOW + 7200000, priority: 3 });
  const tomorrowHigh = item({ due_ms: NOW + DAY, priority: 3 });
  const plain = groupPlanned([todayLow, todayHigh, tomorrowHigh], NOW);
  assert.deepEqual(plain.map((g) => ids(g.items)), [ids([todayLow, todayHigh]), ids([tomorrowHigh])]);
  const sorted = groupPlanned([todayLow, todayHigh, tomorrowHigh], NOW, true);
  assert.deepEqual(
    sorted.map((g) => ids(g.items)),
    [ids([todayHigh, todayLow]), ids([tomorrowHigh])],
    "High goes first WITHIN Today; tomorrow's High does not jump into Today"
  );
  const vm = projectPane(
    { items: [todayLow, todayHigh, tomorrowHigh], view: "planned", query: "", tagFilter: null, byPriority: true },
    NOW
  );
  assert.deepEqual(vm.groups.map((g) => ids(g.items)), [ids([todayHigh, todayLow]), ids([tomorrowHigh])]);
});

test("the priority sort is a per-VIEW preference that round-trips and never holds a view twice", () => {
  let p = { ...DEFAULT_TODO_PREFS };
  assert.equal(sortsByPriority(p, "important"), false, "manual order is the default");
  p = togglePrioritySort(p, "important");
  assert.equal(sortsByPriority(p, "important"), true);
  assert.equal(sortsByPriority(p, "all"), false, "one view's toggle is not another's");
  assert.deepEqual(decodeTodoPrefs(encodeTodoPrefs(p)), p);
  p = togglePrioritySort(togglePrioritySort(p, "all"), "all");
  assert.deepEqual(p.byPriority, ["important"]);
  p = togglePrioritySort(p, "important");
  assert.deepEqual(p.byPriority, []);
  // Completed never sorts by priority, even if a hand-edited pref says so.
  const odd = decodeTodoPrefs('{"byPriority":["completed","nonsense","important","important"]}');
  assert.deepEqual(odd.byPriority, ["important", "completed"], "unknown names dropped, duplicates collapsed");
  assert.equal(sortsByPriority(odd, "completed"), false);
  assert.equal(sortsByPriority(odd, "important"), true);
  assert.deepEqual(decodeTodoPrefs('{"byPriority":"all"}').byPriority, [], "a non-array is no sort");
});

// ---------- reorder ----------

test("a hand reorder is offered only where the rows ARE the manual order", () => {
  assert.equal(canReorder("myday", false), true);
  assert.equal(canReorder("important", false), true);
  assert.equal(canReorder("all", false), true);
  assert.equal(canReorder("planned", false), false, "Planned is ordered by due date");
  assert.equal(canReorder("completed", false), false, "Completed is ordered by finish");
  for (const v of ["myday", "important", "all"] as const) {
    assert.equal(canReorder(v, true), false, `${v} under the priority sort shows no manual order`);
  }
});

test("a drop names the row that will sit ABOVE it, and a drop into its own slot writes nothing", () => {
  const [a, b, c, d] = [item(), item(), item(), item()];
  const list = [a, b, c, d];
  assert.deepEqual(dropTarget(list, c.id, a.id), "start", "dropped before the first row");
  assert.deepEqual(dropTarget(list, a.id, d.id), { item: c.id }, "dropped before d: after c");
  assert.deepEqual(dropTarget(list, a.id, null), { item: d.id }, "dropped past the end: after d");
  assert.deepEqual(dropTarget(list, d.id, b.id), { item: a.id });
  // Its own slot, both spellings of it: before itself, and before its successor.
  assert.equal(dropTarget(list, b.id, b.id), null);
  assert.equal(dropTarget(list, b.id, c.id), null);
  assert.equal(dropTarget(list, d.id, null), null, "the last row dropped past the end");
  assert.equal(dropTarget(list, a.id, a.id), null);
  // A target that left the list mid-drag (an agent completed it) moves nothing.
  assert.equal(dropTarget(list, a.id, "td-gone"), null);
  assert.equal(dropTarget(list, "td-gone", b.id), null);
});

test("a drop in a FILTERED view is spelled by the visible neighbour", () => {
  // The rows on screen are a slice of the scope; the op names a visible row and
  // the engine places the mover directly after it in the whole scope.
  const a = item({ tags: ["x"] });
  const hidden = item();
  const b = item({ tags: ["x"] });
  const c = item({ tags: ["x"] });
  const shown = visibleItems([a, hidden, b, c], { view: "all", tag: "x" }, NOW);
  assert.deepEqual(ids(shown), ids([a, b, c]));
  assert.deepEqual(dropTarget(shown, c.id, b.id), { item: a.id });
});

// ---------- click the row to expand ----------

test("a click on the row's own area toggles it; a control, the body, a selection or a drop does not", () => {
  const base = { onControl: false, inHead: true, selecting: false, dragged: false };
  assert.equal(rowClickToggles(base), true, "the title / meta area");
  assert.equal(rowClickToggles({ ...base, onControl: true }), false, "checkbox, tag chip, priority, star");
  assert.equal(rowClickToggles({ ...base, inHead: false }), false, "inside the expanded body");
  assert.equal(rowClickToggles({ ...base, selecting: true }), false, "someone copying the title");
  assert.equal(rowClickToggles({ ...base, dragged: true }), false, "the end of a drag is a drop");
});

test("only the click a drop produced is swallowed — a later click never vanishes", () => {
  // Review round 1: a 250 ms window after the pointerup ate a genuine click
  // made inside it. The guard is bound to the PRESS instead.
  const g = new DropClickGuard();
  assert.equal(g.click(true), false, "no drag, no arm: every click is a click");

  // The drop's own click: armed at drag end, swallowed once.
  g.dragEnded("drop");
  assert.equal(g.click(true), true, "the click synthesised from the drop press");
  assert.equal(g.click(true), false, "swallowed at most once");

  // A drop released outside the pane produces no click; the NEXT press
  // disarms, so the human's next click is theirs, however soon it comes.
  g.dragEnded("drop");
  g.pointerDown();
  assert.equal(g.click(true), false, "a click that follows a new press is that press's");

  // A keyboard-activated click has no press behind it and is never the drop,
  // and it disarms rather than leaving the arm for a later pointer click.
  g.dragEnded("drop");
  assert.equal(g.click(false), false, "keyboard click passes");
  assert.equal(g.click(true), false, "and the arm is gone");
});

test("a drag the platform CANCELS ends with no click owed — and a later click is never eaten", () => {
  // Review round 2: `pointercancel` (touch turned scroll, pen out of range,
  // the OS claiming the gesture) ends a press with NO pointerup and no click.
  // An arm left behind would sit waiting for a click that is not coming.
  const g = new DropClickGuard();
  g.dragEnded("cancel");
  assert.equal(g.click(true), false, "nothing is swallowed after a cancelled drag");

  // It also CLEARS an arm already standing (a drop whose click never arrived,
  // then a second drag the platform cancelled).
  g.dragEnded("drop");
  g.dragEnded("cancel");
  assert.equal(g.click(true), false, "a cancel clears the previous arm");

  // Blur and dispose are the same: no release of this press will reach us.
  for (const how of ["blur", "dispose"] as const) {
    g.dragEnded("drop");
    g.dragEnded(how);
    assert.equal(g.click(true), false, `${how} leaves nothing armed`);
  }

  // Escape is the one non-drop ending that DOES arm: the button is still
  // held, and its release produces a click that must not expand the row.
  g.dragEnded("escape");
  assert.equal(g.click(true), true, "the release after Escape is swallowed");
  assert.equal(g.click(true), false, "once");
});
