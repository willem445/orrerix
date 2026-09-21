// The To-Do pane's DOM-free model (#3263 S3): decode, the smart views, the
// Planned buckets, ordering and reorder targets, search, and undo's inverse-op
// derivation.
//
// Every clock here is injected. Nothing below reads `Date.now()`, for the same
// reason the quick-add parser does not: a "Today" bucket tested against the
// host clock is a different test every day.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ORDER_GAP,
  SMART_VIEWS,
  decodeSnapshot,
  groupPlanned,
  inView,
  inverseOp,
  matchesQuery,
  moveTarget,
  myDayIsStale,
  needsRenumber,
  plannedBucket,
  visibleItems,
  type TodoItem,
  type TodoOp,
} from "../src/todomodel.ts";

/** Wednesday 2024-05-15, 10:00 local. */
const NOW = new Date(2024, 4, 15, 10, 0, 0, 0).getTime();
const DAY = 86400000;

/** Local midnight `days` from NOW's day, at `hour`. */
function at(days: number, hour = 9): number {
  const d = new Date(NOW);
  d.setDate(d.getDate() + days);
  d.setHours(hour, 0, 0, 0);
  return d.getTime();
}

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
    steps: [],
    order: seq * ORDER_GAP,
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

// ---------- decode ----------

test("decode drops a malformed item and keeps the rest", () => {
  // The fixture COLLIDES on purpose: the bad item carries the same tag as a
  // good one, so a decoder that dropped by tag — or that threw the whole
  // snapshot away — fails differently from one that drops exactly the bad row.
  const raw = {
    version: 1,
    read_only: false,
    quarantined: null,
    workspaces: { "c:/p/loomux": { label: "loomux", root: "C:/p/loomux", first_seen_ms: NOW } },
    items: [
      { id: "td-1", title: "good one", tags: ["infra"], scope: "global", order: 1024, rev: 1 },
      { title: "no id at all", tags: ["infra"] },
      { id: "td-3", title: 42, tags: ["infra"] },
      "not even an object",
      null,
      { id: "td-6", title: "second good one", tags: ["infra"], scope: { workspace: "c:/p/loomux" } },
    ],
  };
  const snap = decodeSnapshot(raw);
  assert.deepEqual(
    snap.items.map((i) => i.title),
    ["good one", "second good one"]
  );
  assert.equal(snap.items[0].tags[0], "infra", "the surviving rows keep the tag the bad one shared");
  assert.deepEqual(snap.items[1].scope, { workspace: "c:/p/loomux" });
  assert.equal(snap.workspaces["c:/p/loomux"].label, "loomux");
  assert.equal(snap.version, 1);
});

test("decode fills every absent field rather than yielding undefined", () => {
  const snap = decodeSnapshot({ items: [{ id: "td-1", title: "bare" }] });
  const i = snap.items[0];
  assert.equal(i.status, "open");
  assert.equal(i.due_ms, null);
  assert.deepEqual(i.tags, []);
  assert.deepEqual(i.steps, []);
  assert.equal(i.priority, 0);
  assert.equal(i.important, false);
  assert.deepEqual(i.created_by, { kind: "human" });
  assert.equal(snap.read_only, false, "an absent read_only is not read-only");
});

test("decode of a hostile or empty payload yields an empty snapshot, not a throw", () => {
  for (const raw of [null, undefined, 42, "nope", [], {}]) {
    const snap = decodeSnapshot(raw);
    assert.deepEqual(snap.items, []);
    assert.deepEqual(snap.workspaces, {});
  }
});

// ---------- smart views ----------

test("My Day holds what was put there and is not yet done", () => {
  const inIt = item({ my_day: NOW });
  const done = item({ my_day: NOW, status: "done", done_ms: NOW });
  const out = item({});
  assert.equal(inView(inIt, "myday", NOW), true);
  assert.equal(inView(done, "myday", NOW), false);
  assert.equal(inView(out, "myday", NOW), false);
});

test("My Day does NOT auto-clear; staleness is reported separately", () => {
  // Whether My Day should empty itself at midnight (Microsoft To Do does) is
  // the open question in the plan's §8, so the predicate takes the
  // non-destructive reading and the staleness is a SEPARATE signal the pane
  // can act on once the human has answered.
  const yesterday = item({ my_day: NOW - DAY });
  assert.equal(inView(yesterday, "myday", NOW), true, "still in the view");
  assert.equal(myDayIsStale(yesterday, NOW), true, "but flagged as carried over");
  assert.equal(myDayIsStale(item({ my_day: NOW }), NOW), false);
  assert.equal(myDayIsStale(item({}), NOW), false, "an item not in My Day is not stale");
});

test("Important, All and Completed", () => {
  const imp = item({ important: true });
  const doneImp = item({ important: true, status: "done", done_ms: NOW });
  const plain = item({});
  const archived = item({ status: "done", done_ms: NOW, archived_ms: NOW });

  assert.equal(inView(imp, "important", NOW), true);
  assert.equal(inView(doneImp, "important", NOW), false, "a finished item leaves Important");

  assert.equal(inView(plain, "all", NOW), true);
  assert.equal(inView(doneImp, "all", NOW), false, "All is the open list");
  assert.equal(inView(archived, "all", NOW), false);

  assert.equal(inView(doneImp, "completed", NOW), true);
  assert.equal(inView(plain, "completed", NOW), false);
  assert.equal(
    inView(archived, "completed", NOW),
    false,
    "an archived item has left Completed — that is what archiving is for"
  );
});

test("every smart view is total over an item: none throws, each answers a boolean", () => {
  // A cheap population control: the view list and the predicate are two halves
  // of one table, and a view added to one and not the other is a silent gap.
  const subject = item({ due_ms: at(1), important: true, my_day: NOW });
  let answered = 0;
  for (const v of SMART_VIEWS) {
    assert.equal(typeof inView(subject, v, NOW), "boolean", v);
    answered += 1;
  }
  assert.equal(answered, SMART_VIEWS.length);
  assert.ok(SMART_VIEWS.length >= 5, "the plan's five views at minimum");
});

// ---------- planned buckets ----------

test("the Planned buckets, including across a month boundary", () => {
  assert.equal(plannedBucket(item({ due_ms: at(-1) }), NOW), "overdue");
  assert.equal(plannedBucket(item({ due_ms: at(0, 8) }), NOW), "today", "earlier today is still today, not overdue");
  assert.equal(plannedBucket(item({ due_ms: at(1) }), NOW), "tomorrow");
  assert.equal(plannedBucket(item({ due_ms: at(6) }), NOW), "week");
  assert.equal(plannedBucket(item({ due_ms: at(7) }), NOW), "later");
  assert.equal(plannedBucket(item({}), NOW), null, "no due date is not Planned at all");

  // The month boundary: from Wed 15 May, `at(17)` is 1 June. The bucket is
  // decided by DAY DISTANCE, so a calendar rollover changes nothing — which
  // is exactly what a `getMonth()`-based implementation would get wrong.
  const june1 = at(17);
  assert.equal(new Date(june1).getMonth(), 5, "the fixture really does cross into June");
  assert.equal(plannedBucket(item({ due_ms: june1 }), NOW), "later");

  // And from a clock ON the boundary, "tomorrow" crosses the month.
  const may31 = new Date(2024, 4, 31, 10, 0, 0, 0).getTime();
  const jun1at9 = new Date(2024, 5, 1, 9, 0, 0, 0).getTime();
  assert.equal(plannedBucket(item({ due_ms: jun1at9 }), may31), "tomorrow");
});

test("groupPlanned returns non-empty buckets in calendar order", () => {
  const items = [
    item({ due_ms: at(7) }),
    item({ due_ms: at(-2) }),
    item({ due_ms: at(0, 14) }),
    item({}),
    item({ due_ms: at(0, 11) }),
    item({ due_ms: at(3), status: "done", done_ms: NOW }),
  ];
  const groups = groupPlanned(items, NOW);
  assert.deepEqual(
    groups.map((g) => g.bucket),
    ["overdue", "today", "later"],
    "an empty bucket is not rendered, and a done item is not Planned"
  );
  assert.deepEqual(
    groups[1].items.map((i) => i.due_ms),
    [at(0, 11), at(0, 14)],
    "within a bucket, soonest first"
  );
});

// ---------- ordering & reorder ----------

test("visibleItems orders by the store's order field, and Completed by newest done", () => {
  const a = item({ order: 3 * ORDER_GAP, title: "third" });
  const b = item({ order: 1 * ORDER_GAP, title: "first" });
  const c = item({ order: 2 * ORDER_GAP, title: "second" });
  assert.deepEqual(
    visibleItems([a, b, c], { view: "all" }, NOW).map((i) => i.title),
    ["first", "second", "third"]
  );

  const old = item({ status: "done", done_ms: NOW - DAY, title: "older", order: ORDER_GAP });
  const recent = item({ status: "done", done_ms: NOW, title: "newer", order: 9 * ORDER_GAP });
  assert.deepEqual(
    visibleItems([old, recent], { view: "completed" }, NOW).map((i) => i.title),
    ["newer", "older"],
    "Completed is a log, so the most recent finish is on top regardless of order"
  );
});

test("moveTarget names the neighbour a move lands after, and refuses the ends", () => {
  const a = item({ order: 1 * ORDER_GAP });
  const b = item({ order: 2 * ORDER_GAP });
  const c = item({ order: 3 * ORDER_GAP });
  const ordered = [a, b, c];

  assert.deepEqual(moveTarget(ordered, b.id, -1), "start", "moving the second up puts it first");
  assert.deepEqual(moveTarget(ordered, c.id, -1), { item: a.id }, "moving the third up lands it after the first");
  assert.deepEqual(moveTarget(ordered, a.id, 1), { item: b.id }, "moving the first down means after b");
  assert.deepEqual(moveTarget(ordered, b.id, 1), { item: c.id });

  assert.equal(moveTarget(ordered, a.id, -1), null, "the top cannot move up");
  assert.equal(moveTarget(ordered, c.id, 1), null, "the bottom cannot move down");
  assert.equal(moveTarget(ordered, "td-nope", -1), null, "an unknown id moves nothing");
});

test("needsRenumber fires exactly when a midpoint would collide", () => {
  // The backend places a moved item at the midpoint of its new neighbours.
  // After enough halvings there is no integer between them, and the move
  // becomes a silent no-op — so the pane has to know to ask for a renumber.
  const roomy = [item({ order: 0 }), item({ order: ORDER_GAP })];
  assert.equal(needsRenumber(roomy), false);

  const tight = [item({ order: 10 }), item({ order: 11 })];
  assert.equal(needsRenumber(tight), true, "no integer sits strictly between 10 and 11");

  const justEnough = [item({ order: 10 }), item({ order: 12 })];
  assert.equal(needsRenumber(justEnough), false, "11 does");

  assert.equal(needsRenumber([item({ order: 0 })]), false, "one item has no neighbours to collide with");
  assert.equal(needsRenumber([]), false);
});

// ---------- search ----------

test("search is substring terms, ANDed, over title, notes and tags", () => {
  const i = item({ title: "Renew the domain", notes: "before it lapses", tags: ["admin", "web"] });
  assert.equal(matchesQuery(i, "renew"), true, "case-insensitive, per filematch's queryTerms");
  assert.equal(matchesQuery(i, "renew domain"), true, "terms are ANDed");
  assert.equal(matchesQuery(i, "renew lapses"), true, "notes count");
  assert.equal(matchesQuery(i, "admin"), true, "tags count");
  assert.equal(matchesQuery(i, "renew missing"), false, "one unmatched term fails the whole query");
  assert.equal(matchesQuery(i, "   "), true, "a blank query is no filter");
  assert.equal(matchesQuery(i, "omai"), true, "a mid-word SUBSTRING matches: omai is inside domain");
  assert.equal(matchesQuery(i, "rnw"), false, "but never FUZZY: a non-contiguous subsequence of renew does not match");
  assert.equal(matchesQuery(i, "dmn"), false, "filematch's header rejects fuzzy and we follow it");
});

test("visibleItems applies view, query and tag together", () => {
  const keep = item({ title: "pay rent", tags: ["home"], important: true });
  const wrongTag = item({ title: "pay tax", tags: ["work"], important: true });
  const wrongView = item({ title: "pay rent", tags: ["home"], important: false });
  const wrongQuery = item({ title: "walk the dog", tags: ["home"], important: true });
  const got = visibleItems([keep, wrongTag, wrongView, wrongQuery], { view: "important", query: "pay", tag: "home" }, NOW);
  assert.deepEqual(
    got.map((i) => i.id),
    [keep.id]
  );
});

// ---------- undo ----------

test("the inverse of an add is a delete of the item it made", () => {
  const made = item({ id: "td-new" });
  const inv = inverseOp({ add: { title: "pay rent" } }, null, made);
  assert.deepEqual(inv, { op: { delete: { id: "td-new" } } });
});

test("the inverse of a complete is the completion state it found", () => {
  const before = item({ id: "td-1", status: "open" });
  assert.deepEqual(inverseOp({ complete: { id: "td-1", done: true } }, before, null), {
    op: { complete: { id: "td-1", done: false } },
  });
  const wasDone = item({ id: "td-1", status: "done", done_ms: NOW });
  assert.deepEqual(inverseOp({ complete: { id: "td-1", done: false } }, wasDone, null), {
    op: { complete: { id: "td-1", done: true } },
  });
});

test("the inverse of an update restores exactly the fields it named", () => {
  const before = item({
    id: "td-1",
    title: "old title",
    due_ms: at(1),
    important: false,
    tags: ["home"],
    rev: 4,
  });
  const inv = inverseOp(
    { update: { id: "td-1", title: "new title", due_ms: null, important: true } },
    before,
    null
  );
  assert.deepEqual(inv, {
    op: {
      update: {
        id: "td-1",
        title: "old title",
        due_ms: at(1),
        important: false,
      },
    },
  });
  // Fields the forward op did not name are NOT in the inverse: an undo that
  // rewrote them would clobber a concurrent agent edit to a field the human
  // never touched.
  const keys = Object.keys((inv as { op: { update: Record<string, unknown> } }).op.update);
  assert.deepEqual(keys.sort(), ["due_ms", "id", "important", "title"]);
  assert.equal(keys.includes("tags"), false);
  assert.equal(keys.includes("if_rev"), false, "an undo must not carry a rev that is already stale");
});

test("clearing a field inverts to setting it, and setting one that was absent inverts to null", () => {
  const hadNoDue = item({ id: "td-1", due_ms: null });
  assert.deepEqual(inverseOp({ update: { id: "td-1", due_ms: at(2) } }, hadNoDue, null), {
    op: { update: { id: "td-1", due_ms: null } },
  });
});

test("FAILURE CASE: a delete has no inverse today, and says so instead of pretending", () => {
  // The store's delete is a soft tombstone, but the op set S1 shipped has no
  // RESTORE — `apply` treats a tombstoned item as unknown, so an update
  // aimed at it is refused. Reporting the gap is what stops #3263 S5 from
  // wiring an undo button that silently does nothing.
  const inv = inverseOp({ delete: { id: "td-1" } }, item({ id: "td-1" }), null);
  assert.deepEqual(inv, {
    unsupported: "the store has no restore op; undoing a delete needs one (#3263 S5)",
  });
});

test("FAILURE CASE: an inverse with nothing to read is refused, not guessed", () => {
  // No `before` snapshot means the pane cannot know what to restore. A
  // best-effort guess here is how an undo quietly writes the wrong value.
  const inv = inverseOp({ update: { id: "td-1", title: "x" } }, null, null);
  assert.equal("unsupported" in inv, true);
  const invComplete = inverseOp({ complete: { id: "td-1", done: true } }, null, null);
  assert.equal("unsupported" in invComplete, true);
  const invAdd = inverseOp({ add: { title: "x" } }, null, null);
  assert.equal("unsupported" in invAdd, true, "an add with no applied item gives no id to delete");
});

test("an update that named no field has no inverse worth pushing", () => {
  const inv = inverseOp({ update: { id: "td-1" } }, item({ id: "td-1" }), null);
  assert.equal("unsupported" in inv, true);
});

// ---------- the op wire shape ----------

test("every op the model builds is a single-key object the backend decoder accepts", () => {
  // The backend refuses an op naming zero or two actions (`parse_op` in
  // `src-tauri/src/orchestration/todo.rs`), so the shape is a contract, not a
  // convention.
  const ops: TodoOp[] = [
    { add: { title: "a" } },
    { update: { id: "td-1", title: "b" } },
    { complete: { id: "td-1", done: true } },
    { delete: { id: "td-1" } },
  ];
  for (const op of ops) {
    assert.equal(Object.keys(op).length, 1, JSON.stringify(op));
  }
});
