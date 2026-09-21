// The To-Do pane's DOM-free model (#3263 S3): decode, the smart views, the
// Planned buckets, ordering and reorder targets, search, and undo's inverse-op
// derivation.
//
// Every clock here is injected. Nothing below reads `Date.now()`, for the same
// reason the quick-add parser does not: a "Today" bucket tested against the
// host clock is a different test every day.

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  ORDER_GAP,
  SMART_VIEWS,
  UNDO_MAX,
  UndoStack,
  decodeSnapshot,
  groupPlanned,
  inView,
  inverseOp,
  matchesQuery,
  moveTarget,
  myDayIsStale,
  needsRenumber,
  opLabel,
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

test("decode reads id and title by the SAME rule: absent OR empty is dropped", () => {
  // #3286 review round 1. An earlier revision checked `id` for emptiness and
  // `title` only for its type, so a present-but-empty title survived and S4
  // would have drawn a blank, unlabelled, clickable row. The engine refuses an
  // empty title on WRITE, so this is reachable only from a store a newer build
  // wrote or one edited by hand — which is the population a defensive decode
  // exists for.
  //
  // The fixture COLLIDES on the field under test: the empty-title row and the
  // empty-id row sit either side of a good one, so a decoder that dropped by
  // POSITION, or that threw the batch away, fails differently from one reading
  // both fields by one rule.
  const snap = decodeSnapshot({
    items: [
      { id: "td-1", title: "" },
      { id: "td-2", title: "kept" },
      { id: "", title: "no id" },
      { id: "td-4", title: "also kept" },
    ],
  });
  assert.deepEqual(
    snap.items.map((i) => i.id),
    ["td-2", "td-4"]
  );

  // Both directions of the one rule, so the assertion is about the RULE and
  // not about this fixture's answer.
  assert.equal(decodeSnapshot({ items: [{ id: "td-9", title: "x" }] }).items.length, 1);
  assert.equal(decodeSnapshot({ items: [{ id: "td-9", title: "" }] }).items.length, 0);
  assert.equal(decodeSnapshot({ items: [{ id: "", title: "x" }] }).items.length, 0);
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

test("My Day empties itself at local midnight, and writes nothing (#3263 S5)", () => {
  // The plan's §8 open question, answered by the human: an item leaves My Day
  // at local midnight, as Microsoft To Do does. The predicate that decides it
  // is `myDayIsStale`, and `inView` is its one reader.
  const yesterday = item({ my_day: NOW - DAY });
  assert.equal(inView(yesterday, "myday", NOW), false, "yesterday's pull has expired");
  assert.equal(myDayIsStale(yesterday, NOW), true);
  assert.equal(inView(item({ my_day: NOW }), "myday", NOW), true, "today's pull stands");
  assert.equal(myDayIsStale(item({ my_day: NOW }), NOW), false);
  assert.equal(myDayIsStale(item({}), NOW), false, "an item not in My Day is not stale");

  // NOTHING IS WRITTEN — the half that makes a per-viewer clock safe to decide
  // this on a store two processes write to. The item leaves the VIEW and keeps
  // its stamp, so `t` puts it straight back and the record of when it was last
  // pulled in survives. An implementation that cleared `my_day` to empty the
  // view would pass every assertion above and fail here.
  const before = JSON.stringify(yesterday);
  inView(yesterday, "myday", NOW);
  assert.equal(JSON.stringify(yesterday), before, "the predicate mutated the item");

  // THE BOUNDARY, both sides, built from LOCAL midnight so the assertion means
  // the same thing in every timezone the suite runs in. Twenty-two hours after
  // a 00:01 pull it is still that day; one minute past the NEXT local midnight
  // it is not.
  const midnight = new Date(NOW);
  midnight.setHours(0, 0, 0, 0);
  const pulled = midnight.getTime() + 60_000;
  assert.equal(inView(item({ my_day: pulled }), "myday", pulled + 22 * 3600_000), true);
  assert.equal(inView(item({ my_day: pulled }), "myday", midnight.getTime() + DAY + 60_000), false);
});

test("My Day's midnight counts CALENDAR days, so a DST day is still one day", () => {
  // Never `n * MS_PER_DAY`: a DST day is 23 or 25 hours long, so an item pulled
  // in at 23:30 the evening before a shift must not read as stale at 00:00 —
  // and must expire at the END of that day whether it ran 23 hours or 25.
  // `dayDelta` divides two LOCAL midnights, which is what makes both true.
  //
  // Driven off the HOST's own transitions rather than a hardcoded date, so it
  // means something wherever the suite runs, and states its own vacuity where
  // the zone has none (a UTC CI runner) instead of passing silently.
  const transitions = dstTransitions(2026);
  if (transitions.length === 0) {
    assert.equal(
      new Date(2026, 0, 1).getTimezoneOffset(),
      new Date(2026, 6, 1).getTimezoneOffset(),
      "no transitions found, but the January and July offsets differ — the scan is blind"
    );
    return;
  }
  for (const midnightOfShiftDay of transitions) {
    // 23:30 the evening BEFORE the shifted day.
    const eveningBefore = midnightOfShiftDay - 30 * 60_000;
    assert.equal(
      inView(item({ my_day: eveningBefore }), "myday", eveningBefore + 20 * 60_000),
      true,
      "an item pulled in at 23:30 left My Day before its own midnight"
    );
    // The shifted day is 23 or 25 hours long; either way the item pulled in
    // the evening before is stale all through it.
    assert.equal(
      inView(item({ my_day: eveningBefore }), "myday", midnightOfShiftDay + 60_000),
      false,
      "a DST day did not start at its own local midnight"
    );
    // And an item pulled in ON the shifted day survives right up to ITS OWN
    // next midnight — which is 23 or 25 hours away, never 24. The end of the
    // day is computed with calendar arithmetic (`setDate(+1)`), because a
    // `+ 24h` written here would be the very bug the assertion is checking for.
    const noonOfShiftDay = midnightOfShiftDay + 12 * 3600_000;
    const nextMidnight = new Date(midnightOfShiftDay);
    nextMidnight.setDate(nextMidnight.getDate() + 1);
    nextMidnight.setHours(0, 0, 0, 0);
    const dayLengthMs = nextMidnight.getTime() - midnightOfShiftDay;
    assert.notEqual(dayLengthMs, DAY, "this is supposed to be the DST day, and it is 24h long");
    assert.equal(
      inView(item({ my_day: noonOfShiftDay }), "myday", nextMidnight.getTime() - 60_000),
      true,
      "an item pulled in at noon expired before the day it was pulled in on had ended"
    );
    assert.equal(
      inView(item({ my_day: noonOfShiftDay }), "myday", nextMidnight.getTime() + 60_000),
      false,
      "an item pulled in at noon outlived its own day"
    );
    // THE DISCRIMINATOR. A `n * MS_PER_DAY` implementation would answer the
    // OPPOSITE of one of these two: on a 23-hour day it expires the item an
    // hour early, on a 25-hour day an hour late. Pinning the divergence means
    // the two assertions above cannot both hold under that implementation.
    assert.equal(
      inView(item({ my_day: noonOfShiftDay }), "myday", noonOfShiftDay + DAY - 12 * 3600_000),
      dayLengthMs > DAY,
      "the 24-hour reading and the calendar reading did not diverge, so this fixture is blind"
    );
  }
});

test("My Day's DST behaviour, pinned in a zone that HAS one (not the host's)", () => {
  // The test above is honest about being vacuous in a UTC zone — which is what
  // CI is — and a guard that can be vacuous on the machine that gates the merge
  // is a guard about the developer's laptop. So this one FORCES the zone: a
  // child `node` with TZ=America/Chicago, where 8 March 2026 is 23 hours long
  // and 1 November 2026 is 25. It fails rather than skips, everywhere.
  //
  // A child process because `TZ` is read when the process starts: setting
  // `process.env.TZ` mid-run does not move `Date`'s notion of local time on
  // every platform, and a test that silently kept the host zone would be the
  // vacuity it is here to remove.
  const script = [
    "const { inView } = await import(process.argv[1]);",
    "const mk = (my_day) => ({ id: 'td-1', scope: 'global', title: 't', notes: '',",
    "  status: 'open', done_ms: null, due_ms: null, remind_ms: null, my_day, priority: 0,",
    "  important: false, tags: [], steps: [], order: 0, created_ms: 0,",
    "  created_by: { kind: 'human' }, updated_ms: 0, updated_by: { kind: 'human' },",
    "  rev: 0, archived_ms: null, deleted_ms: null });",
    "const out = [];",
    // Both shift days: the 23-hour one (spring forward) and the 25-hour one.
    "for (const [y, m, d] of [[2026, 2, 8], [2026, 10, 1]]) {",
    "  const mid = new Date(y, m, d, 0, 0, 0, 0).getTime();",
    "  const next = new Date(y, m, d + 1, 0, 0, 0, 0).getTime();",
    "  const noon = mid + 12 * 3600000;",
    "  out.push({ len: next - mid,",
    "    lastMinute: inView(mk(noon), 'myday', next - 60000),",
    "    pastMidnight: inView(mk(noon), 'myday', next + 60000),",
    "    naive24h: inView(mk(noon), 'myday', noon + 12 * 3600000) });",
    "}",
    "process.stdout.write(JSON.stringify(out));",
  ].join("\n");
  // A file:// URL, built from this test's own URL — never a path. `pathToFileURL`
  // on a URL's `pathname` yields `C:C:...` on Windows, which resolves nowhere.
  const modulePath = new URL("../src/todomodel.ts", import.meta.url).href;
  const res = spawnSync(
    process.execPath,
    ["--experimental-strip-types", "--no-warnings", "--input-type=module", "-e", script, modulePath],
    { env: { ...process.env, TZ: "America/Chicago" }, encoding: "utf8" }
  );
  assert.equal(res.status, 0, `child failed: ${res.stderr}`);
  const [spring, autumn] = JSON.parse(res.stdout) as {
    len: number;
    lastMinute: boolean;
    pastMidnight: boolean;
    naive24h: boolean;
  }[];

  // The fixture's own positive control: if the forced zone did not take, these
  // two are 24h and every assertion below is about nothing.
  assert.equal(spring.len, 23 * 3600_000, "TZ=America/Chicago did not take in the child");
  assert.equal(autumn.len, 25 * 3600_000, "TZ=America/Chicago did not take in the child");

  for (const day of [spring, autumn]) {
    assert.equal(day.lastMinute, true, "an item pulled in at noon left My Day before its midnight");
    assert.equal(day.pastMidnight, false, "an item pulled in at noon outlived its own day");
  }
  // THE DISCRIMINATOR, and it is what makes this a test of calendar arithmetic
  // rather than of `inView` in general: exactly 24 hours after the noon pull,
  // the two readings DISAGREE, in opposite directions on the two days.
  assert.equal(spring.naive24h, false, "a 23-hour day ended before +24h and the view said it had not");
  assert.equal(autumn.naive24h, true, "a 25-hour day had not ended at +24h and the view said it had");
});

/** Local midnights in `year` that begin a day whose UTC offset differs from the
 *  previous day's — i.e. the first midnight of each DST-shift day. Empty in a
 *  zone with no DST, which the caller states rather than passing over. */
function dstTransitions(year: number): number[] {
  const out: number[] = [];
  let prev = new Date(year, 0, 1, 12, 0, 0, 0).getTimezoneOffset();
  for (let d = 1; d < 366; d++) {
    const noon = new Date(year, 0, 1, 12, 0, 0, 0);
    noon.setDate(noon.getDate() + 1 + (d - 1));
    if (noon.getFullYear() !== year) break;
    const off = noon.getTimezoneOffset();
    if (off !== prev) {
      const mid = new Date(noon);
      mid.setHours(0, 0, 0, 0);
      out.push(mid.getTime());
      prev = off;
    }
  }
  return out;
}

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

test("a delete inverts to the engine's restore op (#3263 S5)", () => {
  // The wiring #3285 left outstanding, and what this slice closed. It needs NO
  // `before` snapshot: the tombstone still carries every field, so the inverse
  // is the id and nothing else.
  assert.deepEqual(inverseOp({ delete: { id: "td-1" } }, item({ id: "td-1" }), null), {
    op: { restore: { id: "td-1" } },
  });
  assert.deepEqual(inverseOp({ delete: { id: "td-1" } }, null, null), {
    op: { restore: { id: "td-1" } },
  });
  // And back the other way, so undoing an undo of a delete is a delete.
  assert.deepEqual(inverseOp({ restore: { id: "td-1" } }, null, null), {
    op: { delete: { id: "td-1" } },
  });
});

test("THE RETRACTED CLAIMS: nothing still says a delete cannot be undone", () => {
  // A test that quotes a false claim ENFORCES it — correcting the claim then
  // reddens a test and reads as the regression (CLAUDE.md, "A TEST is one of
  // those surfaces"). Both retracted sentences are pinned as retracted: #3285's
  // "the store has no restore op", and S4's "it exists but is not wired".
  const inv = inverseOp({ delete: { id: "td-1" } }, item({ id: "td-1" }), null);
  assert.equal("op" in inv, true, "a delete has an inverse and it is a restore");
  const text = JSON.stringify(inv);
  assert.doesNotMatch(text, /has no restore op/);
  assert.doesNotMatch(text, /not wired up/);
});

test("an archive inverts to the SAME ids with the flag flipped", () => {
  // The inverse is derived from the OP, not from any item's snapshot — which is
  // why the op carries its ids rather than a scope. An "archive everything
  // completed in this scope" op would have no inverse at all: the set it moved
  // is not one the store can reconstruct afterwards.
  assert.deepEqual(inverseOp({ archive: { ids: ["a", "b"], archived: true } }, null, null), {
    op: { archive: { ids: ["a", "b"], archived: false } },
  });
  assert.deepEqual(inverseOp({ archive: { ids: ["a"], archived: false } }, null, null), {
    op: { archive: { ids: ["a"], archived: true } },
  });
  // The ids are COPIED, not aliased: an inverse holding the forward op's own
  // array would follow a later mutation of it.
  const ids = ["a"];
  const inv = inverseOp({ archive: { ids, archived: true } }, null, null);
  ids.push("b");
  assert.deepEqual("op" in inv && "archive" in inv.op ? inv.op.archive.ids : null, ["a"]);
});

test("FAILURE CASE: an archive that named no items has no inverse", () => {
  assert.deepEqual(inverseOp({ archive: { ids: [], archived: true } }, null, null), {
    unsupported: "the archive named no items",
  });
});

test("the undo stack keeps INVERSES, in order, and hands the newest back first", () => {
  const stack = new UndoStack();
  assert.equal(stack.depth, 0);
  assert.equal(stack.pop(), null, "an empty stack must answer null, not throw");

  const a = item({ id: "td-1", important: false });
  stack.push({ update: { id: "td-1", important: true } }, a, null);
  stack.push({ complete: { id: "td-2", done: true } }, item({ id: "td-2" }), null);
  assert.equal(stack.depth, 2);

  assert.deepEqual(stack.peek(), {
    op: { complete: { id: "td-2", done: false } },
    label: "Completed",
  });
  assert.equal(stack.depth, 2, "peek removed an entry");
  assert.deepEqual(stack.pop(), { op: { complete: { id: "td-2", done: false } }, label: "Completed" });
  assert.deepEqual(stack.pop(), {
    op: { update: { id: "td-1", important: false } },
    label: "Updated",
  });
  assert.equal(stack.depth, 0);
});

test("a write with NO honest inverse is not pushed, and says so", () => {
  // The rule the rest of the module follows: refuse visibly rather than offer a
  // button that would do nothing. The caller reads the returned `Inverse` to
  // decide whether to show the undo toast at all.
  const stack = new UndoStack();
  const answer = stack.push({ update: { id: "td-1", title: "x" } }, null, null);
  assert.equal("unsupported" in answer, true);
  assert.equal(stack.depth, 0, "an uninvertible write went on the stack anyway");

  // And an invertible one IS pushed, and reports the op — the positive control
  // without which the assertion above passes against a push that never pushes.
  const ok = stack.push({ delete: { id: "td-1" } }, null, null);
  assert.deepEqual(ok, { op: { restore: { id: "td-1" } } });
  assert.equal(stack.depth, 1);
});

test("the stack is capped at UNDO_MAX, dropping the OLDEST", () => {
  const stack = new UndoStack();
  for (let i = 0; i < UNDO_MAX + 10; i++) {
    stack.push({ delete: { id: `td-${i}` } }, null, null);
  }
  assert.equal(stack.depth, UNDO_MAX, `the cap is not holding (depth ${stack.depth})`);

  // The SURVIVORS are the newest, which is the half a depth assertion alone
  // does not pin: a cap implemented as "ignore pushes once full" would give the
  // same depth and keep exactly the wrong fifty.
  const newest = stack.pop();
  assert.deepEqual(newest?.op, { restore: { id: `td-${UNDO_MAX + 9}` } });
  let last: string | null = null;
  while (stack.depth > 0) {
    const e = stack.pop();
    last = e !== null && "restore" in e.op ? e.op.restore.id : null;
  }
  assert.equal(last, "td-10", "the cap dropped from the wrong end");
});

test("clear() empties the stack — the scope switch's whole reason", () => {
  // Every entry names an id in the list the pane is LEAVING, and the engine
  // resolves an id with no scope check, so an undo popped after a switch would
  // write to the other store while the header says otherwise.
  const stack = new UndoStack();
  stack.push({ delete: { id: "td-1" } }, null, null);
  stack.clear();
  assert.equal(stack.depth, 0);
  assert.equal(stack.pop(), null);
});

test("opLabel names what the human did, in the past tense the toast reads in", () => {
  assert.equal(opLabel({ add: { title: "x" } }), "Added");
  assert.equal(opLabel({ delete: { id: "a" } }), "Deleted");
  assert.equal(opLabel({ restore: { id: "a" } }), "Restored");
  assert.equal(opLabel({ complete: { id: "a", done: true } }), "Completed");
  assert.equal(opLabel({ complete: { id: "a", done: false } }), "Reopened");
  assert.equal(opLabel({ update: { id: "a", title: "x" } }), "Updated");
  assert.equal(opLabel({ archive: { ids: ["a", "b"], archived: true } }), "Archived 2");
  assert.equal(opLabel({ archive: { ids: ["a"], archived: false } }), "Unarchived 1");
});

test("archived items are in NO view, and only Completed can be told to show them", () => {
  const openArchived = item({ id: "td-open", archived_ms: NOW });
  const doneArchived = item({ id: "td-done", status: "done", done_ms: NOW, archived_ms: NOW });
  const done = item({ id: "td-live", status: "done", done_ms: NOW });
  const all = [openArchived, doneArchived, done];

  for (const view of SMART_VIEWS) {
    assert.equal(inView(openArchived, view, NOW), false, `archived showed in ${view}`);
    assert.equal(inView(doneArchived, view, NOW), false, `archived showed in ${view}`);
  }

  // Without the toggle: Completed holds only the live finished row.
  assert.deepEqual(
    visibleItems(all, { view: "completed" }, NOW).map((i) => i.id),
    ["td-live"]
  );
  // With it: the archived FINISHED row joins, and the archived OPEN one does
  // not — an archived open item is not part of the log of finished work.
  assert.deepEqual(
    visibleItems(all, { view: "completed", includeArchived: true }, NOW).map((i) => i.id).sort(),
    ["td-done", "td-live"]
  );
  // And the toggle does nothing anywhere else: honouring it on `all` would put
  // back the very rows the human archived to get rid of.
  for (const view of ["all", "myday", "planned", "important"] as const) {
    assert.deepEqual(
      visibleItems(all, { view, includeArchived: true }, NOW).map((i) => i.id),
      visibleItems(all, { view }, NOW).map((i) => i.id),
      `includeArchived changed the ${view} view`
    );
  }
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
