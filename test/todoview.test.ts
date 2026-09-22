// The To-Do PANE's pure half (#3263 S4): the projection the renderer draws, the
// per-viewer prefs, the un-submitted draft discipline, and the selection walk.
//
// Every clock is injected, for `todomodel.test.ts`'s reason: a bucket tested
// against the host clock is a different test every day.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  DEFAULT_TODO_PREFS,
  EMPTY_ROW_DRAFT,
  EMPTY_TEXT,
  ROW_BUDGET,
  decodeTodoPrefs,
  encodeTodoPrefs,
  moveSelection,
  planReveal,
  projectPane,
  pruneDrafts,
  renderedRows,
  reseedPristineDrafts,
  rowDraftIsPristine,
  seedRowDraft,
  type RowDraft,
  type TodoPrefs,
} from "../src/todoview.ts";
import { SMART_VIEWS, inView, type TodoItem } from "../src/todomodel.ts";

/** Wednesday 2024-05-15, 10:00 local — `todomodel.test.ts`'s own anchor. */
const NOW = new Date(2024, 4, 15, 10, 0, 0, 0).getTime();
const DAY = 86400000;

let seq = 0;
function item(over: Partial<TodoItem> = {}): TodoItem {
  seq += 1;
  return {
    id: `td-${String(seq).padStart(4, "0")}`,
    scope: "global",
    title: `task ${seq}`,
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
    created_ms: seq,
    created_by: { kind: "human" },
    updated_ms: seq,
    updated_by: { kind: "human" },
    rev: 1,
    archived_ms: null,
    deleted_ms: null,
    ...over,
  };
}

// ── the projection ────────────────────────────────────────────────────────────

test("the strip's counts are the views' sizes, not the filtered list's", () => {
  // The decision this pins is an ORDER: counts before the filters. A chip whose
  // number moves as you type would be telling the human about their query, and
  // the strip exists to say how much work exists. The fixture COLLIDES on
  // purpose — the query matches exactly one of the three All rows — so a
  // counts-after-filter implementation reports 1 where this asserts 3, and
  // cannot pass by accident.
  const items = [
    item({ title: "alpha", my_day: NOW }),
    item({ title: "beta" }),
    item({ title: "gamma", important: true }),
  ];
  const p = projectPane({ items, view: "all", query: "alpha", tagFilter: null }, NOW);
  assert.equal(p.counts.all, 3, "All counts every open item, whatever is typed");
  assert.equal(p.counts.myday, 1);
  assert.equal(p.counts.important, 1);
  assert.equal(p.total, 1, "the LIST is filtered even though the counts are not");
  assert.equal(p.groups[0].items.length, 1);
});

test("Planned renders one group per non-empty bucket, in calendar order", () => {
  const items = [
    item({ title: "late", due_ms: NOW - 2 * DAY }),
    item({ title: "soon", due_ms: NOW + 3 * DAY }),
    item({ title: "today", due_ms: NOW + 3600000 }),
  ];
  const p = projectPane({ items, view: "planned", query: "", tagFilter: null }, NOW);
  assert.deepEqual(
    p.groups.map((g) => g.key),
    ["overdue", "today", "week"],
    "tomorrow and later are omitted rather than drawn as empty headings"
  );
  assert.deepEqual(
    p.groups.map((g) => g.label),
    ["Overdue", "Today", "This week"]
  );
});

test("a non-Planned view is ONE unlabelled group, and an empty one is no group at all", () => {
  const p = projectPane({ items: [item(), item()], view: "all", query: "", tagFilter: null }, NOW);
  assert.equal(p.groups.length, 1);
  assert.equal(p.groups[0].label, null, "a heading over the whole list would say nothing");
  const none = projectPane({ items: [], view: "all", query: "", tagFilter: null }, NOW);
  assert.deepEqual(none.groups, []);
  assert.equal(none.empty, true);
});

test("the budget elides the TAIL, and the elision count is what is missing", () => {
  const items = Array.from({ length: ROW_BUDGET + 52 }, () => item());
  const p = projectPane({ items, view: "all", query: "", tagFilter: null }, NOW);
  assert.equal(p.total, ROW_BUDGET + 52, "the count chip carries the real total");
  assert.equal(p.shown, ROW_BUDGET);
  assert.equal(p.elided, 52, "the stated elision — a silent stop at 200 is the lie");
  assert.equal(renderedRows(p).length, ROW_BUDGET);
  // The rows kept are the FIRST ones in display order, so the elision is a tail
  // and never a slice out of the middle.
  assert.equal(renderedRows(p)[0].id, items[0].id);
});

test("a bucket the budget cannot fit is truncated, never dropped whole", () => {
  // Overdue alone overruns the budget. The heading has to survive: it is the
  // answer to "is there anything overdue", and dropping the group to save rows
  // deletes that answer.
  const items = Array.from({ length: ROW_BUDGET + 10 }, (_, i) =>
    item({ due_ms: NOW - (i + 1) * DAY })
  );
  items.push(item({ due_ms: NOW + 30 * DAY }));
  const p = projectPane({ items, view: "planned", query: "", tagFilter: null }, NOW);
  assert.deepEqual(p.groups.map((g) => g.key), ["overdue"], "later did not fit and is gone");
  assert.equal(p.groups[0].items.length, ROW_BUDGET, "overdue is truncated, not dropped");
  assert.equal(p.elided, 11);
});

test("the tag rail is the SCOPE's open tags, not the filtered rows' — else it cannot widen a filter", () => {
  // The failure case this exists for: with `#infra` selected, a rail built from
  // the filtered rows would show only `#infra`, and the human could never
  // switch to `#release` from it.
  const items = [item({ tags: ["infra"] }), item({ tags: ["release"] }), item({ tags: ["infra"] })];
  const p = projectPane({ items, view: "all", query: "", tagFilter: "infra" }, NOW);
  assert.deepEqual(p.tags.map((t) => t.tag), ["infra", "release"]);
  assert.equal(p.total, 2, "the LIST is still filtered");
});

test("a DONE item's tag leaves the rail when the item does", () => {
  // The rail is built from the OPEN list, so completing the only `#release`
  // item takes the tag with it. Pinned because the obvious implementation
  // (every tag on every item) passes every other test in this file.
  const open = item({ tags: ["release"] });
  const p1 = projectPane({ items: [open], view: "all", query: "", tagFilter: null }, NOW);
  assert.deepEqual(p1.tags.map((t) => t.tag), ["release"]);
  const done = { ...open, status: "done", done_ms: NOW };
  const p2 = projectPane({ items: [done], view: "all", query: "", tagFilter: null }, NOW);
  assert.deepEqual(p2.tags, [], "a finished item's tag is not a filter you can still use");
});

test("an empty list says WHY, and a filtered one never claims the view is empty", () => {
  const items = [item({ title: "alpha", due_ms: NOW + DAY })];
  const unfiltered = projectPane({ items: [], view: "planned", query: "", tagFilter: null }, NOW);
  assert.equal(unfiltered.emptyReason, "planned");
  assert.equal(EMPTY_TEXT[unfiltered.emptyReason], "Nothing scheduled.");
  // Same view, same emptiness, DIFFERENT fact: the human has a term typed.
  const byQuery = projectPane({ items, view: "planned", query: "zzz", tagFilter: null }, NOW);
  assert.equal(byQuery.empty, true);
  assert.equal(byQuery.emptyReason, "filtered");
  // A tag filter is the same fact as a query, and is the half an implementation
  // that only checks `query` gets wrong.
  const byTag = projectPane({ items, view: "planned", query: "", tagFilter: "nope" }, NOW);
  assert.equal(byTag.emptyReason, "filtered");
  // Whitespace is not a filter — " " must not turn "Nothing scheduled" into
  // "Nothing matches".
  const blank = projectPane({ items: [], view: "planned", query: "   ", tagFilter: null }, NOW);
  assert.equal(blank.emptyReason, "planned");
});

test("EMPTY_TEXT has a sentence for every view, and for the filtered case", () => {
  for (const v of SMART_VIEWS) {
    assert.equal(typeof EMPTY_TEXT[v], "string", `${v} has no empty sentence`);
    assert.ok(EMPTY_TEXT[v].length > 0);
  }
  assert.ok(EMPTY_TEXT.filtered.length > 0);
});

// ── per-viewer prefs ──────────────────────────────────────────────────────────

test("prefs round-trip, and a malformed field costs only that field", () => {
  const p = { scope: "global", view: "completed", byPriority: ["important"] } as TodoPrefs;
  assert.deepEqual(decodeTodoPrefs(encodeTodoPrefs(p)), p);
  // Field-wise, not record-wise: the good half survives the bad half.
  assert.deepEqual(decodeTodoPrefs('{"scope":"global","view":"nonsense"}'), {
    scope: "global",
    view: DEFAULT_TODO_PREFS.view,
    byPriority: [],
  });
  assert.deepEqual(decodeTodoPrefs('{"scope":7,"view":"planned"}'), {
    scope: DEFAULT_TODO_PREFS.scope,
    view: "planned",
    byPriority: [],
  });
});

test("decoding prefs never throws, whatever is in the slot", () => {
  // The failure case: `localStorage` is a string a human or another build can
  // have written. Each of these would throw or yield a partial record under a
  // naive `JSON.parse(raw)`.
  for (const raw of [null, "", "not json", "[]", "null", "7", '"a string"', "{}"]) {
    const got = decodeTodoPrefs(raw);
    assert.deepEqual(got, DEFAULT_TODO_PREFS, `bad prefs ${JSON.stringify(raw)} lost the defaults`);
  }
});

test("the encoder writes only the three known keys", () => {
  // A pref record that round-trips a stray key would grow the blob without
  // anything reading it back.
  const raw = encodeTodoPrefs({ ...DEFAULT_TODO_PREFS, extra: 1 } as never);
  assert.deepEqual(Object.keys(JSON.parse(raw)).sort(), ["byPriority", "scope", "view"]);
});

// ── the un-submitted draft ────────────────────────────────────────────────────

test("a fresh draft is seeded from the ITEM and is pristine — including one with notes", () => {
  const bare = item();
  assert.deepEqual(seedRowDraft(bare), EMPTY_ROW_DRAFT);
  assert.equal(rowDraftIsPristine(seedRowDraft(bare)), true);
  // The case an "is it empty?" predicate gets wrong: an item that already HAS
  // notes seeds a non-empty box, and calling that dirty makes every expanded
  // row look edited.
  const noted = item({ notes: "already written" });
  assert.equal(seedRowDraft(noted).notes, "already written");
  assert.equal(rowDraftIsPristine(seedRowDraft(noted)), true);
});

/**
 * The draft fields that are NOT typable — a record of what the human can edit,
 * not a reading of the item.
 *
 * One entry, argued: `seededNotes` is what the notes box was seeded WITH, so
 * "has the human typed?" is measured against it rather than into it. Spelled
 * out here because the loop below must not claim to be testing typing on a
 * field nobody can type into (#3293 round 6 residual 2) — it passed for the
 * right answer by the wrong route, since moving the seed alone also separates
 * it from `notes`.
 *
 * DEFAULT-DENY: a field added to `RowDraft` and not named here lands in the
 * TYPABLE loop, so forgetting to classify it reddens rather than exempts it.
 */
const NON_TYPABLE: readonly (keyof RowDraft)[] = ["seededNotes"];

test("planReveal: the row is on screen already, so Show changes no view", () => {
  // Moving the view is itself a visible jump. Doing it when the row was
  // already in front of the human is noise, so "reveal, view: null" is a
  // distinct answer from "reveal, view: all".
  const row = item({ id: "td-1", my_day: NOW });
  const vm = projectPane({ items: [row], view: "myday", query: "", tagFilter: null }, NOW);
  assert.deepEqual(planReveal([row], "td-1", renderedRows(vm), NOW), {
    kind: "reveal",
    view: null,
  });
});

test("planReveal: a reachable row the current view hides falls back to All", () => {
  const hidden = item({ id: "td-1" });                 // open, but not in My Day
  const shown = item({ id: "td-2", my_day: NOW });
  const vm = projectPane(
    { items: [hidden, shown], view: "myday", query: "", tagFilter: null },
    NOW
  );
  assert.ok(
    !renderedRows(vm).some((i) => i.id === "td-1"),
    "precondition: the row is NOT on screen"
  );
  assert.deepEqual(planReveal([hidden, shown], "td-1", renderedRows(vm), NOW), {
    kind: "reveal",
    view: "all",
  });
});

test("FAILURE CASE: a row COMPLETED between the notice and the click says so", () => {
  // #3301 review round 2, finding 3. The scan skips done items, so a notice
  // only exists for an open one — but the human clicks LATER, and an agent's
  // `todo_complete` in that window moved the row out of every view. The pane
  // used to clear the filters, fall back to All, select a row nothing rendered
  // and scroll to a selector matching nothing: the toast dismissed and the
  // screen did not change.
  const done = item({ id: "td-1", status: "done", done_ms: NOW });
  const vm = projectPane({ items: [done], view: "myday", query: "", tagFilter: null }, NOW);
  assert.deepEqual(planReveal([done], "td-1", renderedRows(vm), NOW), {
    kind: "left",
    why: "done",
  });
});

test("FAILURE CASE: a row ARCHIVED between the notice and the click says which", () => {
  // Distinguished from "done" on purpose: the two have different answers for
  // the human. A finished row is in Completed; an archived one needs the
  // toggle there as well.
  const away = item({ id: "td-1", status: "done", done_ms: NOW, archived_ms: NOW });
  const vm = projectPane({ items: [away], view: "completed", query: "", tagFilter: null }, NOW);
  assert.deepEqual(planReveal([away], "td-1", renderedRows(vm), NOW), {
    kind: "left",
    why: "archived",
  });
  // An archived row that was never completed is still "archived", not "done" —
  // the two checks are ordered, and this is the fixture that tells them apart.
  const openAway = item({ id: "td-2", archived_ms: NOW });
  assert.deepEqual(planReveal([openAway], "td-2", []), { kind: "left", why: "archived" });
});

test("FAILURE CASE: a row DELETED between the notice and the click is 'gone'", () => {
  // The rarer case, and the only one the pane used to explain. A tombstone is
  // absent from the snapshot entirely, so it reads as an unknown id.
  assert.deepEqual(planReveal([], "td-1", []), { kind: "gone" });
  assert.deepEqual(planReveal([item({ id: "td-other" })], "td-1", [], NOW), { kind: "gone" });
});

test("planReveal never answers 'reveal' for a row All would not hold", () => {
  // The promise the fallback makes: it returns `all` only where `inView`
  // agrees All contains the row. Driven over every item shape this module can
  // build, so a later change to `inView` that drops a class reddens here
  // rather than re-introducing the silent no-op one view over.
  const shapes = [
    item({ id: "a" }),
    item({ id: "b", my_day: NOW }),
    item({ id: "c", due_ms: NOW + 86400000 }),
    item({ id: "d", important: true }),
    item({ id: "e", status: "done", done_ms: NOW }),
    item({ id: "f", archived_ms: NOW }),
    item({ id: "g", status: "done", done_ms: NOW, archived_ms: NOW }),
  ];
  let revealed = 0;
  for (const s of shapes) {
    const plan = planReveal(shapes, s.id, []);
    if (plan.kind !== "reveal") continue;
    revealed += 1;
    assert.notEqual(plan.view, null, "a row not on screen must name a view to move to");
    assert.equal(
      inView(s, plan.view as "all", NOW),
      true,
      `planReveal sent ${s.id} to a view that does not contain it`
    );
  }
  // THE POSITIVE CONTROL. Without it this loop passes against a planReveal
  // that never answers "reveal" at all.
  assert.equal(revealed, 4, "the four open shapes must all be revealable");
});

test("the strip's counts are UNKNOWN until a snapshot has landed (#3293 round 6 residual 1)", () => {
  // "We have not looked" and "there is nothing" are different facts, and only
  // one of them is safe to assert. The list's empty state already followed that
  // rule; the chips and the header total did not, so a pane mid-read drew
  // "My Day 0" over a list that has rows.
  const rows = [item({ my_day: NOW }), item({ important: true })];

  const loading = projectPane(
    { items: [], view: "myday", query: "", tagFilter: null, loaded: false },
    NOW
  );
  assert.equal(loading.countsKnown, false);
  assert.equal(loading.empty, true, "the projection is still empty — it just cannot say why");

  const loaded = projectPane(
    { items: rows, view: "myday", query: "", tagFilter: null, loaded: true },
    NOW
  );
  assert.equal(loaded.countsKnown, true);
  assert.equal(loaded.counts.myday, 1);

  // A genuinely empty, genuinely READ list is `countsKnown` too — the
  // discriminator, without which `countsKnown` could just be "total > 0".
  const empty = projectPane(
    { items: [], view: "myday", query: "", tagFilter: null, loaded: true },
    NOW
  );
  assert.equal(empty.countsKnown, true, "an empty list that HAS been read reads as unknown");
  assert.equal(empty.counts.myday, 0);

  // Absent means loaded, so every existing caller keeps its meaning.
  const legacy = projectPane({ items: rows, view: "myday", query: "", tagFilter: null }, NOW);
  assert.equal(legacy.countsKnown, true);
});

test("the archived toggle reaches the rows and NEVER the strip's counts", () => {
  // Archiving must not move what the strip says, because the strip says how
  // much work exists and archived work is over. If it did, the number would
  // jump when the toggle flipped and mean nothing either way.
  const live = item({ id: "td-live", status: "done", done_ms: NOW });
  const put = item({ id: "td-put", status: "done", done_ms: NOW, archived_ms: NOW });
  const base = { items: [live, put], view: "completed" as const, query: "", tagFilter: null };

  const hidden = projectPane(base, NOW);
  const shown = projectPane({ ...base, showArchived: true }, NOW);

  assert.deepEqual(renderedRows(hidden).map((i) => i.id), ["td-live"]);
  assert.deepEqual(renderedRows(shown).map((i) => i.id).sort(), ["td-live", "td-put"]);
  assert.equal(hidden.total, 1);
  assert.equal(shown.total, 2);
  assert.deepEqual(shown.counts, hidden.counts, "the toggle moved the strip");
  assert.equal(hidden.counts.completed, 1, "the count is the LIVE finished rows");
});

test("the pristine predicate reads EVERY TYPABLE field of the draft", () => {
  // #1348 N1/N4: the renderer's seed and "is this untouched" are one question
  // asked twice, so a field present in the draft and absent from the predicate
  // is a silent hole. Driven from the object's own keys rather than from a list
  // this test remembers — a field added to `RowDraft` and forgotten here then
  // reddens instead of passing.
  const seeded = seedRowDraft(item({ notes: "n" }));
  const keys = Object.keys(seeded) as (keyof RowDraft)[];

  // The exemption list is checked against the object, so a RENAMED seed field
  // cannot leave a stale row here silently exempting nothing.
  for (const k of NON_TYPABLE) {
    assert.ok(keys.includes(k), `NON_TYPABLE names \`${k}\`, which RowDraft no longer has`);
  }
  const typable = keys.filter((k) => !NON_TYPABLE.includes(k));
  assert.ok(typable.length >= 3, "the draft lost its fields — this scan is blind, not clean");

  for (const k of typable) {
    const touched: RowDraft = { ...seeded, [k]: `${seeded[k]} typed` };
    assert.equal(
      rowDraftIsPristine(touched),
      false,
      `typing into \`${k}\` left the draft reading as pristine`
    );
  }
});

test("the seed field is not typed into — it MOVES WITH the value it seeds", () => {
  // The property the loop above cannot state, and the one `reseedPristineDrafts`
  // actually depends on: when an agent's write re-seeds a row, BOTH halves move
  // together and the draft stays pristine. A predicate that compared `notes`
  // against a literal, or against the live item, would fail exactly here.
  const seeded = seedRowDraft(item({ notes: "n" }));
  assert.equal(rowDraftIsPristine(seeded), true);
  const reseeded: RowDraft = { ...seeded, notes: "agent wrote this", seededNotes: "agent wrote this" };
  assert.equal(rowDraftIsPristine(reseeded), true, "a re-seeded draft read as edited");
  // And moving only ONE half is dirty, in both directions.
  assert.equal(rowDraftIsPristine({ ...seeded, notes: "typed" }), false);
  assert.equal(rowDraftIsPristine({ ...seeded, seededNotes: "moved" }), false);
});

test("drafts are pruned to the rows still on screen", () => {
  const drafts = new Map<string, RowDraft>([
    ["td-1", { notes: "a", step: "" }],
    ["td-2", { notes: "b", step: "" }],
  ]);
  pruneDrafts(drafts, new Set(["td-2"]));
  assert.deepEqual([...drafts.keys()], ["td-2"], "a deleted row's half-typed note must not linger");
  // A prune against an empty screen clears it rather than throwing.
  pruneDrafts(drafts, new Set());
  assert.equal(drafts.size, 0);
});

// ── selection ─────────────────────────────────────────────────────────────────

test("j/k walk the flattened rows and clamp at both ends", () => {
  const items = [item(), item(), item()];
  const p = projectPane({ items, view: "all", query: "", tagFilter: null }, NOW);
  const rows = renderedRows(p);
  assert.equal(moveSelection(rows, null, 1), rows[0].id, "j on a fresh pane enters at the top");
  assert.equal(moveSelection(rows, null, -1), rows[2].id, "k enters at the bottom");
  assert.equal(moveSelection(rows, rows[0].id, 1), rows[1].id);
  assert.equal(moveSelection(rows, rows[0].id, -1), rows[0].id, "clamps — a list is not a carousel");
  assert.equal(moveSelection(rows, rows[2].id, 1), rows[2].id);
  assert.equal(moveSelection([], "td-1", 1), null);
});

test("the selection crosses a bucket heading exactly as the eye does", () => {
  // The flattening is the point: `j` off the last Overdue row lands on the
  // first Today row, not nowhere.
  const late = item({ due_ms: NOW - DAY });
  const soon = item({ due_ms: NOW + 3600000 });
  const p = projectPane({ items: [late, soon], view: "planned", query: "", tagFilter: null }, NOW);
  assert.equal(p.groups.length, 2, "the fixture must actually straddle two buckets");
  assert.equal(moveSelection(renderedRows(p), late.id, 1), soon.id);
});

test("a selection the render dropped re-enters at the edge", () => {
  // The failure case: an agent completes the selected row out from under the
  // human. Returning null (or the old id) would leave `j` doing nothing.
  const items = [item(), item()];
  const rows = renderedRows(projectPane({ items, view: "all", query: "", tagFilter: null }, NOW));
  assert.equal(moveSelection(rows, "td-gone", 1), rows[0].id);
  assert.equal(moveSelection(rows, "td-gone", -1), rows[1].id);
});

test("an agent's write does NOT make an untouched draft read as edited", () => {
  // The premise both reviewers' premortems land on, and the reason `RowDraft`
  // carries `seededNotes` at all. "Has the human typed?" is a question about the
  // draft against its OWN seed. Measured against the item's live value instead,
  // an untouched draft flips to dirty the instant a second writer edits the row
  // — which is this pane's normal condition, not an edge case.
  const before = item({ notes: "from the human" });
  const draft = seedRowDraft(before);
  assert.equal(rowDraftIsPristine(draft), true);
  // The agent writes. The DRAFT has not changed; only the item has.
  const after = { ...before, notes: "rewritten by worker-3" };
  assert.equal(
    rowDraftIsPristine(draft),
    true,
    "an agent's write is not the human typing — the draft is still untouched"
  );
  // And the re-seed follows the store, because nothing was typed.
  const drafts = new Map([[before.id, draft]]);
  const moved = reseedPristineDrafts(drafts, [after]);
  assert.equal(moved, 1, "the mechanism ran — an absence-only pin would pass against one that did not");
  assert.equal(drafts.get(before.id)?.notes, "rewritten by worker-3");
  assert.equal(rowDraftIsPristine(drafts.get(before.id)!), true);
});

test("a draft the human has typed into is NEVER re-seeded, even when the item moved", () => {
  // The other direction, and the one that matters more: re-seeding a dirty
  // draft would eat a half-typed note, which is the exact defect the in-list
  // editor rule exists to prevent. The fixture COLLIDES — same row, both
  // writers — so an implementation that re-seeds unconditionally cannot pass.
  const before = item({ notes: "original" });
  const draft = { ...seedRowDraft(before), notes: "the human is mid-sen" };
  assert.equal(rowDraftIsPristine(draft), false);
  const after = { ...before, notes: "rewritten by worker-3" };
  const drafts = new Map([[before.id, draft]]);
  const moved = reseedPristineDrafts(drafts, [after]);
  assert.equal(moved, 0);
  assert.equal(
    drafts.get(before.id)?.notes,
    "the human is mid-sen",
    "a half-typed note must survive an agent's write to the same row"
  );
});

test("a pristine draft whose item did not move is left alone", () => {
  // The no-op case, pinned so `reseedPristineDrafts` cannot be implemented as
  // "always rewrite": a re-seed on every render would be indistinguishable from
  // correct here but would churn the map on every one of an agent's bursts.
  const it = item({ notes: "steady" });
  const drafts = new Map([[it.id, seedRowDraft(it)]]);
  assert.equal(reseedPristineDrafts(drafts, [it]), 0);
});

test("re-seeding skips an id the store no longer has", () => {
  // `pruneDrafts` owns the gone case; this one must not throw on the way past.
  const it = item();
  const drafts = new Map([["td-vanished", seedRowDraft(it)]]);
  assert.equal(reseedPristineDrafts(drafts, []), 0);
  assert.equal(drafts.size, 1, "re-seeding does not prune — that is the other function's job");
});
