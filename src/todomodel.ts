// The To-Do pane's model (#3263 S3) — DOM-free, clock-injected, testable.
//
// Everything the pane has to DECIDE lives here; `src/todopane.ts` (S4) owns
// only the elements. That split is the repo's convention for frontend logic
// worth testing (`layout.ts`, `steer.ts`, `spawnexpiry.ts`, …), and it is what
// lets `test/todomodel.test.ts` pin a month-boundary bucket without a DOM.
//
// Three jobs:
//
//  1. DECODE. The backend owns this schema (`crates/loomux-engine/src/todo.rs`)
//     and may be a NEWER build than this bundle, so the snapshot is read
//     defensively: an item that cannot be understood is DROPPED and the rest
//     are kept. A pane that renders nothing because one row was odd is worse
//     than one that renders the other forty-nine.
//  2. THE SMART VIEWS. Pure predicates over a decoded item, plus the Planned
//     buckets. Every one of them takes `nowMs` — see the clock note below.
//  3. THE OP SHAPE, and undo's inverse of it. The wire shape is a contract
//     with `parse_op` in `src-tauri/src/orchestration/todo.rs`, which refuses
//     an op naming zero or two actions, so it is expressed as a type here
//     rather than assembled at each call site.
//
// THE CLOCK IS A PARAMETER, everywhere. Not one function below reads
// `Date.now()`. A "Today" bucket tested against the host clock is a different
// test every day, and a bucket that silently changes at midnight while the
// pane is open is a bug you cannot reproduce.

// Explicit `.ts`: this is a VALUE import, and `node --test` loads `src/*.ts`
// off disk rather than through Vite (see the note in `transport.ts`).
import { queryTerms } from "./filematch.ts";

// ---------- the model, mirroring `loomux_engine::todo` ----------

/** `"global"`, or one project's list. Mirrors the engine's `Scope`. */
export type Scope = "global" | { workspace: string };

/** Who performed a write, for the row's attribution dot. */
export type Actor =
  | { kind: "human" }
  | { kind: "agent"; id: string; name: string; group: string; role: string };

export interface Step {
  id: string;
  title: string;
  done: boolean;
}

export interface TodoItem {
  id: string;
  scope: Scope;
  title: string;
  notes: string;
  /** `"open"` or `"done"`. A string rather than a union because the BACKEND
   *  owns this vocabulary and may grow it; `isDone` is the only reader. */
  status: string;
  done_ms: number | null;
  due_ms: number | null;
  remind_ms: number | null;
  /** When the item was put in My Day, or null. */
  my_day: number | null;
  priority: number;
  important: boolean;
  tags: string[];
  /** The human's colour label (#3335), one of `TODO_COLORS`, or null. Kept as a
   *  string rather than narrowed to `TodoColor` on decode: a NEWER build may
   *  have written a name this one does not know, and the row should still
   *  render — just without a stripe (`colorOf`). */
  color: string | null;
  steps: Step[];
  order: number;
  created_ms: number;
  created_by: Actor;
  updated_ms: number;
  updated_by: Actor;
  rev: number;
  archived_ms: number | null;
  deleted_ms: number | null;
}

export interface Workspace {
  label: string;
  root: string;
  first_seen_ms: number;
}

export interface TodoSnapshot {
  version: number;
  /** The store was written by a newer build: readable, every write refused. */
  read_only: boolean;
  /** Where a corrupt store was moved to, when this read quarantined one. */
  quarantined: string | null;
  workspaces: Record<string, Workspace>;
  items: TodoItem[];
}

/** The gap the backend leaves between adjacent `order` values
 *  (`loomux_engine::todo::ORDER_GAP`). Mirrored, not imported — the two live
 *  in different languages. Nothing in the pane does gap arithmetic any more
 *  (#3335: the engine re-spaces an exhausted gap itself, see `dropTarget`); it
 *  stays for test fixtures that build a realistic store. */
export const ORDER_GAP = 1024;

const MS_PER_DAY = 86400000;

// ---------- colour (#3335) ----------

/**
 * The colours an item may carry — the engine's `COLORS`, mirrored.
 *
 * Mirrored rather than fetched, like `ORDER_GAP`: the two live in different
 * languages. `test/todov2.test.ts` reads `crates/loomux-engine/src/todo.rs`
 * and fails if the lists ever differ, and a second test fails unless every
 * name is a key of `theme.ts`'s `IDENTITY` table — so a colour can only ever
 * be painted by an identity token, never by a hue invented here.
 */
export const TODO_COLORS = ["rose", "amber", "lime", "jade", "cyan", "azure", "violet", "orchid"] as const;
export type TodoColor = (typeof TODO_COLORS)[number];

/** The item's colour if this build knows it, else null — an unknown name from
 *  a newer build draws no stripe rather than a guessed one. */
export function colorOf(item: Pick<TodoItem, "color">): TodoColor | null {
  const c = item.color;
  return c !== null && (TODO_COLORS as readonly string[]).includes(c) ? (c as TodoColor) : null;
}

/**
 * The hues a TAG may be drawn in — a SUBSET of the identity table, argued.
 *
 * The three identity hues that also carry a state role (rose = danger, amber =
 * attention, jade = ok) are left out, because a tag chip sits on the row's
 * meta line right beside the due date, and the due date's one state dye is
 * amber for "overdue". An amber `#release` chip next to an amber overdue date
 * would put two meanings on one pigment on one line — the exact reading the
 * pane's channel rule exists to prevent. The per-ITEM colour keeps all eight:
 * it is the human's own label, in its own position (the row's left stripe),
 * away from the date.
 */
export const TAG_HUES = ["lime", "cyan", "azure", "violet", "orchid"] as const;
export type TagHue = (typeof TAG_HUES)[number];

/**
 * The stable hue for a tag: an FNV-1a hash of its text, INTO `TAG_HUES`.
 *
 * Stable across launches, machines and builds — a tag that changed colour on
 * restart would be noise, not identity — and it never leaves the theme's
 * table: the hash picks an INDEX, the table picks the pigment, so there is no
 * hash-to-hue anywhere (`test/theme.test.ts`'s population rules). Hashing the
 * exact text rather than a folded form is deliberate: tags are stored
 * lower-case by every path that writes one from the pane, so folding here would
 * only matter for an agent's mixed-case tag, which is a different tag.
 */
export function tagHue(tag: string): TagHue {
  let h = 0x811c9dc5;
  for (let i = 0; i < tag.length; i++) {
    h ^= tag.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return TAG_HUES[h % TAG_HUES.length];
}

// ---------- priority (#3335) ----------

/**
 * The four priority levels, as the row's control names them. The store's
 * `priority` (0..=3, `PRIORITY_MAX` in the engine) IS the level — this is a
 * label table over the one priority the quick-add's `!`/`!!`/`!!!` already
 * writes, not a second priority beside it.
 */
export const PRIORITY_LABEL = ["None", "Low", "Medium", "High"] as const;
export const PRIORITY_MAX = 3;

/** The level a click on the row's priority control moves to: up one, wrapping
 *  from High back to None. A value outside 0..=3 (a newer build, a hand edit)
 *  is treated as None, so one click always lands on a legal level. */
export function nextPriority(p: number): number {
  const cur = Number.isInteger(p) && p >= 0 && p <= PRIORITY_MAX ? p : 0;
  return (cur + 1) % (PRIORITY_MAX + 1);
}

// ---------- decode ----------

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function str(v: unknown, fallback = ""): string {
  return typeof v === "string" ? v : fallback;
}

function bool(v: unknown, fallback = false): boolean {
  return typeof v === "boolean" ? v : fallback;
}

/** A finite number, or `fallback`. `null` and a missing key are the same
 *  answer here; the store writes `null` for an absent timestamp. */
function num(v: unknown, fallback: number): number {
  return typeof v === "number" && Number.isFinite(v) ? v : fallback;
}

function optNum(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function strs(v: unknown): string[] {
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
}

function decodeScope(v: unknown): Scope {
  if (v === "global") return "global";
  if (isObject(v) && typeof v.workspace === "string") return { workspace: v.workspace };
  return "global";
}

function decodeActor(v: unknown): Actor {
  if (isObject(v) && v.kind === "agent") {
    return {
      kind: "agent",
      id: str(v.id),
      name: str(v.name),
      group: str(v.group),
      role: str(v.role),
    };
  }
  return { kind: "human" };
}

function decodeSteps(v: unknown): Step[] {
  if (!Array.isArray(v)) return [];
  const out: Step[] = [];
  for (const raw of v) {
    if (!isObject(raw) || typeof raw.title !== "string") continue;
    out.push({ id: str(raw.id), title: raw.title, done: bool(raw.done) });
  }
  return out;
}

/**
 * One item, or null when it cannot be understood.
 *
 * The bar is deliberately low and deliberately NOT zero: an `id` and a `title`
 * are what make a row renderable and addressable, and everything else has a
 * defined absent value. A row missing either is dropped — it could not be
 * clicked, completed or deleted, so showing it would be a lie.
 *
 * **Missing means absent OR empty, for both, and by the SAME rule** (#3286
 * review round 1). An earlier revision checked `id` for emptiness and
 * `title` only for its type, so a present-but-empty title survived and the
 * pane would have drawn a blank, unlabelled, clickable row. The engine's
 * `check_title` refuses an empty title on WRITE, so nothing this build
 * produces is lost here — but a store a newer build wrote, or one edited by
 * hand, is exactly the population a defensive decode exists for, and it is
 * where reading the two fields by different rules would have shown.
 */
function decodeItem(v: unknown): TodoItem | null {
  if (!isObject(v)) return null;
  if (typeof v.id !== "string" || v.id === "") return null;
  if (typeof v.title !== "string" || v.title === "") return null;
  return {
    id: v.id,
    scope: decodeScope(v.scope),
    title: v.title,
    notes: str(v.notes),
    status: str(v.status, "open"),
    done_ms: optNum(v.done_ms),
    due_ms: optNum(v.due_ms),
    remind_ms: optNum(v.remind_ms),
    my_day: optNum(v.my_day),
    priority: num(v.priority, 0),
    important: bool(v.important),
    tags: strs(v.tags),
    color: typeof v.color === "string" && v.color !== "" ? v.color : null,
    steps: decodeSteps(v.steps),
    order: num(v.order, 0),
    created_ms: num(v.created_ms, 0),
    created_by: decodeActor(v.created_by),
    updated_ms: num(v.updated_ms, 0),
    updated_by: decodeActor(v.updated_by),
    rev: num(v.rev, 0),
    archived_ms: optNum(v.archived_ms),
    deleted_ms: optNum(v.deleted_ms),
  };
}

function decodeWorkspaces(v: unknown): Record<string, Workspace> {
  if (!isObject(v)) return {};
  const out: Record<string, Workspace> = {};
  for (const [key, raw] of Object.entries(v)) {
    if (!isObject(raw)) continue;
    out[key] = {
      label: str(raw.label, key),
      root: str(raw.root),
      first_seen_ms: num(raw.first_seen_ms, 0),
    };
  }
  return out;
}

/**
 * Read a `todo_snapshot` response.
 *
 * Never throws. A hostile or absent payload yields an empty snapshot, because
 * the alternative is a pane that renders an error where the list should be
 * every time a read degrades — and the backend's own degraded read already
 * answers an empty snapshot for exactly the cases that matter.
 */
export function decodeSnapshot(raw: unknown): TodoSnapshot {
  const o = isObject(raw) ? raw : {};
  const items: TodoItem[] = [];
  if (Array.isArray(o.items)) {
    for (const v of o.items) {
      const item = decodeItem(v);
      if (item !== null) items.push(item);
    }
  }
  return {
    version: num(o.version, 0),
    read_only: bool(o.read_only),
    quarantined: typeof o.quarantined === "string" ? o.quarantined : null,
    workspaces: decodeWorkspaces(o.workspaces),
    items,
  };
}

// ---------- item predicates ----------

export function isDone(item: TodoItem): boolean {
  return item.status === "done";
}

export function isArchived(item: TodoItem): boolean {
  return item.archived_ms !== null;
}

/** Local midnight of the day `ms` falls in. */
function startOfDay(ms: number): number {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** Whole local days from `nowMs`'s day to `ms`'s day. Negative is the past.
 *
 *  Day DISTANCE, never a calendar comparison: a `getMonth()`-based bucket puts
 *  1 June in a different bucket from 31 May for no reason a human would
 *  recognise. Built by dividing two local midnights, so a DST shift inside the
 *  span rounds back to the whole day it is. */
function dayDelta(ms: number, nowMs: number): number {
  return Math.round((startOfDay(ms) - startOfDay(nowMs)) / MS_PER_DAY);
}

/**
 * True when an item is in My Day but was put there on an earlier day.
 *
 * **MY DAY EMPTIES ITSELF AT LOCAL MIDNIGHT** (#3263 S5, the human's answer to
 * the plan's §8 open question). This is the predicate that decides it, and
 * `inView(item, "myday", …)` is its one reader — which is why that function's
 * `nowMs` stopped being `_nowMs` in the same commit.
 *
 * **NOTHING IS WRITTEN.** The item leaves the VIEW; its `my_day` stamp stays
 * exactly where the human (or an agent) put it. That is the difference between
 * this and a midnight sweep, and it is the whole reason it is safe for a
 * per-viewer clock to decide it:
 *
 *  * a machine asleep at midnight, or a pane open across it, agree — neither
 *    has to have RUN at midnight for the answer to be right, because the
 *    answer is recomputed from the stamp on every render;
 *  * two windows on two timezones each empty at their OWN midnight, which is
 *    what a human in either one means by "today";
 *  * nothing is destroyed, so `t` puts the item back with one keystroke and
 *    the record of when it was last pulled in survives.
 *
 * **Whole local days, never 24-hour spans.** `dayDelta` divides two local
 * midnights, so a DST day — 23 or 25 hours long — still rounds to the one day
 * it is, and an item pulled into My Day at 23:30 the evening the clocks go
 * back does not read as stale at 00:30.
 */
export function myDayIsStale(item: TodoItem, nowMs: number): boolean {
  return item.my_day !== null && dayDelta(item.my_day, nowMs) < 0;
}

// ---------- smart views ----------

export const SMART_VIEWS = ["myday", "planned", "important", "all", "completed"] as const;
export type SmartView = (typeof SMART_VIEWS)[number];

export const SMART_VIEW_LABEL: Record<SmartView, string> = {
  myday: "My Day",
  planned: "Planned",
  important: "Important",
  all: "All",
  completed: "Completed",
};

/**
 * Is `item` in `view`?
 *
 * Four of the five are the OPEN list sliced differently, so each excludes a
 * finished item; `completed` is the one that collects them. An ARCHIVED item
 * is in none of them — that is what archiving is for (#3263 S5 added the op).
 * The one way to see one is the Completed view's own "show archived" toggle,
 * which goes through `visibleItems`' `includeArchived` rather than through
 * here: archiving must not move what the STRIP counts, because the strip says
 * how much work exists and archived work is over.
 *
 * **My Day reads the clock, and it is the only view that does** (#3263 S5).
 * An item pulled into My Day on an earlier day leaves it at local midnight —
 * `myDayIsStale` carries the argument, the "nothing is written" half and the
 * DST note. The parameter that used to be `_nowMs` against the plan's §8 open
 * question is that question, answered.
 */
export function inView(item: TodoItem, view: SmartView, nowMs: number): boolean {
  if (isArchived(item)) return false;
  switch (view) {
    case "completed":
      return isDone(item);
    case "myday":
      return !isDone(item) && item.my_day !== null && !myDayIsStale(item, nowMs);
    case "planned":
      return !isDone(item) && item.due_ms !== null;
    case "important":
      return !isDone(item) && item.important;
    case "all":
      return !isDone(item);
  }
  // Exhaustive above; a view added to SMART_VIEWS and not to the switch is a
  // `tsc` error on the return type, not a silent `undefined`.
  return false;
}

// ---------- Planned buckets ----------

export const PLANNED_BUCKETS = ["overdue", "today", "tomorrow", "week", "later"] as const;
export type PlannedBucket = (typeof PLANNED_BUCKETS)[number];

export const PLANNED_BUCKET_LABEL: Record<PlannedBucket, string> = {
  overdue: "Overdue",
  today: "Today",
  tomorrow: "Tomorrow",
  week: "This week",
  later: "Later",
};

/**
 * Which Planned bucket an item falls in, or null when it is not Planned.
 *
 * Decided by whole-day distance (`dayDelta`), so a month or year rollover
 * changes nothing: 1 June is "tomorrow" from 31 May exactly as 16 May is from
 * 15 May. "Overdue" is a whole day behind, not merely an hour — an item due at
 * 09:00 is still TODAY's problem at 14:00.
 */
export function plannedBucket(item: TodoItem, nowMs: number): PlannedBucket | null {
  if (item.due_ms === null || isDone(item) || isArchived(item)) return null;
  const days = dayDelta(item.due_ms, nowMs);
  if (days < 0) return "overdue";
  if (days === 0) return "today";
  if (days === 1) return "tomorrow";
  if (days < 7) return "week";
  return "later";
}

export interface PlannedGroup {
  bucket: PlannedBucket;
  items: TodoItem[];
}

/** The Planned view: non-empty buckets in calendar order, soonest first
 *  within each. An empty bucket is omitted rather than rendered as a heading
 *  with nothing under it. */
export function groupPlanned(
  items: readonly TodoItem[],
  nowMs: number,
  byPriority = false
): PlannedGroup[] {
  const byBucket = new Map<PlannedBucket, TodoItem[]>();
  for (const item of items) {
    const bucket = plannedBucket(item, nowMs);
    if (bucket === null) continue;
    const list = byBucket.get(bucket);
    if (list) list.push(item);
    else byBucket.set(bucket, [item]);
  }
  const out: PlannedGroup[] = [];
  for (const bucket of PLANNED_BUCKETS) {
    const list = byBucket.get(bucket);
    if (!list || list.length === 0) continue;
    // Under the priority sort (#3335) the BUCKETS stay — they answer "when" —
    // and priority orders the rows inside each; due time is the tiebreak.
    list.sort(
      (a, b) =>
        (byPriority ? b.priority - a.priority : 0) ||
        (a.due_ms ?? 0) - (b.due_ms ?? 0) ||
        compareFallback(a, b)
    );
    out.push({ bucket, items: list });
  }
  return out;
}

// ---------- search ----------

/**
 * Substring terms, ANDed, over title, notes and tags.
 *
 * `queryTerms` from `filematch.ts` is the repo's term splitter and its header
 * rejects fuzzy matching — we follow, so a search for `omai` does NOT find
 * "domain". A blank query is no filter, matching the file filter's own rule.
 */
export function matchesQuery(item: TodoItem, query: string): boolean {
  const terms = queryTerms(query);
  if (terms.length === 0) return true;
  const hay = (item.title + "\n" + item.notes + "\n" + item.tags.join(" ")).toLowerCase();
  return terms.every((t) => hay.includes(t));
}

// ---------- ordering ----------

/** The tiebreak for two items a view's primary key cannot separate. Stable and
 *  total, so the rendered order never depends on the array the snapshot
 *  happened to arrive in. */
function compareFallback(a: TodoItem, b: TodoItem): number {
  return a.created_ms - b.created_ms || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0);
}

export interface VisibleOpts {
  view: SmartView;
  /** Substring terms, ANDed. Blank means no filter. */
  query?: string;
  /** One tag, exact. Absent means no filter. */
  tag?: string | null;
  /**
   * Show archived items too — the Completed view's own toggle (#3263 S5).
   *
   * **It applies to `completed` and to nothing else**, and that is deliberate
   * rather than an unfinished generalisation. Archiving is how a human clears
   * a finished list; an archived item is *done work put away*, so the only
   * place it makes sense to look at one is the log of finished work. Honouring
   * this flag on `all` or `myday` would put items back in the open list that
   * the human archived precisely to get rid of.
   *
   * It is also the ONE way back: `inView` excludes an archived item from every
   * view, so without this toggle an archive would be a one-way door with undo
   * as its only exit — and undo lives for the life of a pane.
   */
  includeArchived?: boolean;
  /**
   * Sort by priority, highest first, instead of the manual order (#3335).
   *
   * The MANUAL order is untouched underneath: this re-sorts the rows, it
   * writes nothing, so switching it off restores exactly the order the human
   * (and every drag) left. Ties — every row at one level — keep the manual
   * order, so a list where only two rows carry a priority lifts those two and
   * leaves the rest as they were.
   *
   * Ignored by Completed, which is a log ordered by finish time, not a list.
   * In Planned it orders rows WITHIN each date bucket (`groupPlanned` keeps the
   * buckets): the buckets answer "when", and a priority sort that dissolved
   * them would answer a different question than the view is for.
   */
  byPriority?: boolean;
}

/** `inView`, plus the Completed view's archive toggle. Private because the
 *  counts must NOT use it — see `includeArchived`'s doc and `projectPane`. */
function inViewWithArchive(
  item: TodoItem,
  view: SmartView,
  nowMs: number,
  includeArchived: boolean
): boolean {
  if (isArchived(item)) return includeArchived && view === "completed" && isDone(item);
  return inView(item, view, nowMs);
}

/**
 * The rows a view renders, filtered and ordered.
 *
 * Ordering is the store's `order` field everywhere EXCEPT Completed, which is
 * a log rather than a list: the most recent finish goes on top, because that
 * is the one a human is looking for when they open it.
 */
export function visibleItems(
  items: readonly TodoItem[],
  opts: VisibleOpts,
  nowMs: number
): TodoItem[] {
  const tag = opts.tag ?? null;
  const out = items.filter(
    (i) =>
      inViewWithArchive(i, opts.view, nowMs, opts.includeArchived === true) &&
      matchesQuery(i, opts.query ?? "") &&
      (tag === null || i.tags.includes(tag))
  );
  if (opts.view === "completed") {
    out.sort((a, b) => (b.done_ms ?? 0) - (a.done_ms ?? 0) || compareFallback(a, b));
  } else if (opts.byPriority === true) {
    out.sort((a, b) => b.priority - a.priority || a.order - b.order || compareFallback(a, b));
  } else {
    out.sort((a, b) => a.order - b.order || compareFallback(a, b));
  }
  return out;
}

/**
 * May the human reorder `view` by hand right now?
 *
 * Only where the rows ARE the manual order: My Day, Important and All, with
 * the priority sort off. Completed is ordered by finish time and Planned by due
 * date, and under the priority sort the order on screen is not the stored one —
 * in each, a drag would write an `order` the human then cannot see take effect,
 * which reads as a drop that did nothing. So the pane offers no drag there and
 * `Shift+↑/↓` says why, rather than moving something invisible.
 */
export function canReorder(view: SmartView, byPriority: boolean): boolean {
  return !byPriority && (view === "myday" || view === "important" || view === "all");
}

/** Where a moved item should land, in the shape `update.order_after` takes. */
export type OrderAfter = "start" | { item: string };

/**
 * The `order_after` for moving the item `id` one place up (`-1`) or down
 * (`+1`) within `ordered`, or null when there is nowhere to go.
 *
 * `ordered` is what `visibleItems` returned, so a move is relative to what the
 * human can SEE. Moving up past the first neighbour means landing after the
 * one before IT — or `"start"` when there is none, which is why "first in the
 * list" has a spelling of its own rather than being an absent id.
 */
export function moveTarget(
  ordered: readonly TodoItem[],
  id: string,
  delta: -1 | 1
): OrderAfter | null {
  const ix = ordered.findIndex((i) => i.id === id);
  if (ix < 0) return null;
  const to = ix + delta;
  if (to < 0 || to >= ordered.length) return null;
  // Landing at index `to` means sitting after whatever ends up above it. Going
  // up, that is the item two places back (or the start); going down, it is the
  // item currently at `to`.
  if (delta < 0) {
    return to === 0 ? "start" : { item: ordered[to - 1].id };
  }
  return { item: ordered[to].id };
}

/**
 * The `order_after` for a DRAG: the row `id` dropped immediately before
 * `beforeId` (or at the end of `ordered` when `beforeId` is null). Null when
 * the drop would leave the row where it is.
 *
 * `moveTarget`'s rule, generalised from one step to any distance: the
 * destination is spelled by the row that will sit ABOVE the dropped one, and
 * `"start"` when none will. A drop onto the row's own slot — directly before
 * itself or directly before its current successor — is null, so a click that
 * wobbled a few pixels past the drag threshold sends no write at all.
 *
 * **There is no gap arithmetic here, and that is the point.** The pane names a
 * NEIGHBOUR; the engine numbers it (`order_for`), and since #3335 it re-spaces
 * the scope in the same write when the gap has run out. The pane used to
 * refuse a move it predicted would collide (`needsRenumber`, now gone) — which
 * could only ever be a guess about the VISIBLE rows, when the gap that matters
 * is between neighbours in the whole scope.
 */
export function dropTarget(
  ordered: readonly TodoItem[],
  id: string,
  beforeId: string | null
): OrderAfter | null {
  const from = ordered.findIndex((i) => i.id === id);
  if (from < 0 || beforeId === id) return null;
  const rest = ordered.filter((i) => i.id !== id);
  let to: number;
  if (beforeId === null) {
    to = rest.length;
  } else {
    to = rest.findIndex((i) => i.id === beforeId);
    if (to < 0) return null; // the target left the list mid-drag
  }
  if (to === from) return null; // dropped back into its own slot
  return to === 0 ? "start" : { item: rest[to - 1].id };
}

// ---------- ops ----------

/** Fields an `add` may carry. `scope` is deliberately absent: the backend
 *  derives it from the workspace root the command was called with, and
 *  `parse_op` refuses an op that names one. */
export interface AddFields {
  title: string;
  notes?: string;
  due_ms?: number;
  remind_ms?: number;
  priority?: number;
  important?: boolean;
  tags?: string[];
  /** Step titles, in order. */
  steps?: string[];
  my_day?: number;
}

/** Fields an `update` may carry. An ABSENT key leaves the field alone; an
 *  explicit `null` on a nullable one CLEARS it — the distinction the backend's
 *  hand-written decoder exists to preserve. */
export interface UpdateFields {
  id: string;
  if_rev?: number;
  title?: string;
  notes?: string;
  due_ms?: number | null;
  remind_ms?: number | null;
  my_day?: number | null;
  priority?: number;
  important?: boolean;
  tags?: string[];
  /** One of `TODO_COLORS` to set, `null` to clear (#3335). */
  color?: string | null;
  steps?: { id?: string; title: string; done: boolean }[];
  order_after?: OrderAfter;
}

/** One mutation, in the single-key shape `parse_op` accepts.
 *
 *  `restore` and `archive` join the four at #3263 S5. Both were already the
 *  backend's: `restore` landed with #3285 and `parse_op` has accepted it since,
 *  and `archive` is S5's own engine op. What was missing here was the type and
 *  a caller, which is exactly what `inverseOp` said when it refused to invert a
 *  delete. */
export type TodoOp =
  | { add: AddFields }
  | { update: UpdateFields }
  | { complete: { id: string; done: boolean } }
  | { delete: { id: string } }
  | { restore: { id: string } }
  | { archive: { ids: string[]; archived: boolean } };

/** What `todo_apply` answers. */
export interface Applied {
  scope: Scope;
  ids: string[];
  item: TodoItem | null;
  purged: number;
}

// ---------- undo ----------

/** An inverse op, or the reason there isn't one. Never a guess: see
 *  [`inverseOp`]. */
export type Inverse = { op: TodoOp } | { unsupported: string };

/** The nullable update fields, and how to read each one off an item. Shared by
 *  the inverse so a field added to one is a `tsc` error in the other. */
const NULLABLE: { key: "due_ms" | "remind_ms" | "my_day" }[] = [
  { key: "due_ms" },
  { key: "remind_ms" },
  { key: "my_day" },
];

/**
 * The op that undoes `op`, given the item as it stood BEFORE (from the
 * snapshot the pane was rendering) and the `Applied` the backend answered.
 *
 * **It refuses rather than guesses.** Two cases have no honest inverse and
 * each says so:
 *
 *  * no `before` snapshot. Without it the pane cannot know what to restore,
 *    and a best-effort guess is how an undo quietly writes the wrong value.
 *  * an update that named no field. There is nothing to put back.
 *
 * **A DELETE used to be a third, and #3263 S5 is where it stopped being one.**
 * The store's delete is a soft tombstone and the engine's `restore` op inverts
 * one (#3285); what was missing was this type and a caller, which is what the
 * refusal message said. Both now exist, so a delete inverts to
 * `{restore: {id}}`. Two things about that inverse are worth stating because
 * they are what make it honest rather than merely available: the engine
 * refuses a restore whose 30-day tombstone window has passed (it reads back as
 * an unknown id), and it refuses one into a scope that has hit `ITEMS_MAX` —
 * so an undo of a delete can still fail, loudly, with the backend's own
 * message in a toast. It can never silently half-work.
 *
 * The inverse carries EXACTLY the fields the forward op named, and never an
 * `if_rev`: an undo that rewrote untouched fields would clobber a concurrent
 * agent edit to something the human never touched, and a rev read before the
 * forward write is stale by construction.
 */
export function inverseOp(op: TodoOp, before: TodoItem | null, applied: Applied | TodoItem | null): Inverse {
  if ("add" in op) {
    const id = applied === null ? null : "item" in applied ? (applied.item?.id ?? null) : applied.id;
    if (id === null || id === "") {
      return { unsupported: "the add returned no item, so there is no id to delete" };
    }
    return { op: { delete: { id } } };
  }

  if ("delete" in op) {
    return { op: { restore: { id: op.delete.id } } };
  }

  if ("restore" in op) {
    return { op: { delete: { id: op.restore.id } } };
  }

  if ("archive" in op) {
    // Symmetric by construction: the op carries BOTH the ids and the direction,
    // so its inverse is the same ids with the flag flipped. That shape was
    // chosen for this — an `archive-all` that took a scope instead would have
    // no inverse at all, because "everything that was completed at the time"
    // is not a set the store can reconstruct afterwards.
    if (op.archive.ids.length === 0) {
      return { unsupported: "the archive named no items" };
    }
    return { op: { archive: { ids: [...op.archive.ids], archived: !op.archive.archived } } };
  }

  if (before === null) {
    return { unsupported: "no snapshot of the item before the write; refusing to guess" };
  }

  if ("complete" in op) {
    return { op: { complete: { id: op.complete.id, done: isDone(before) } } };
  }

  const fwd = op.update;
  const back: UpdateFields = { id: fwd.id };
  let named = 0;
  if (fwd.title !== undefined) {
    back.title = before.title;
    named += 1;
  }
  if (fwd.notes !== undefined) {
    back.notes = before.notes;
    named += 1;
  }
  for (const { key } of NULLABLE) {
    if (fwd[key] !== undefined) {
      back[key] = before[key];
      named += 1;
    }
  }
  if (fwd.priority !== undefined) {
    back.priority = before.priority;
    named += 1;
  }
  if (fwd.important !== undefined) {
    back.important = before.important;
    named += 1;
  }
  if (fwd.tags !== undefined) {
    back.tags = [...before.tags];
    named += 1;
  }
  if (fwd.color !== undefined) {
    // `null` is a real value here, not "absent": an item that had no colour
    // is put back to none by sending the clear.
    back.color = before.color;
    named += 1;
  }
  if (fwd.steps !== undefined) {
    back.steps = before.steps.map((s) => ({ id: s.id, title: s.title, done: s.done }));
    named += 1;
  }
  // `order_after` is deliberately NOT inverted. It is a destination, not a
  // value, and the item that was above this one may itself have moved since —
  // so the honest inverse of a reorder is a fresh reorder the pane computes
  // from the CURRENT list, not a stale neighbour id recorded before the write.
  if (named === 0) {
    return { unsupported: "the update named no invertible field" };
  }
  return { op: { update: back } };
}

// ---------- the undo stack ----------

/**
 * How many undos a pane remembers.
 *
 * FIFTY, and it is a bound rather than a number anyone measured: the stack
 * holds one inverse op per write and each is a few hundred bytes, so the
 * memory is irrelevant and what is being bounded is *meaning*. An undo forty
 * writes ago, in a list two agents have also been writing to, restores a value
 * to a row whose context the human no longer remembers — and a stack with no
 * floor invites exactly that. The ops are dropped from the OLDEST end, so the
 * fifty most recent are always the ones available.
 */
export const UNDO_MAX = 50;

/** One undoable write, as the pane remembers it. */
export interface UndoEntry {
  /** The op that puts it back. */
  op: TodoOp;
  /** What the human did, for the toast: "Completed", "Deleted", "Archived 7". */
  label: string;
}

/** The phrase an undo toast uses for a FORWARD op. Past tense, because it is
 *  read after the write has landed. */
export function opLabel(op: TodoOp): string {
  if ("add" in op) return "Added";
  if ("delete" in op) return "Deleted";
  if ("restore" in op) return "Restored";
  if ("complete" in op) return op.complete.done ? "Completed" : "Reopened";
  if ("archive" in op) {
    const n = op.archive.ids.length;
    return op.archive.archived ? `Archived ${n}` : `Unarchived ${n}`;
  }
  return "Updated";
}

/**
 * The pane's undo history — DOM-free and clock-free, so `test/todomodel.test.ts`
 * can drive it directly.
 *
 * **It stores INVERSES, computed at push time, never forward ops replayed
 * backwards.** The difference is the `before` snapshot: `inverseOp` needs the
 * item as it stood before the write, and that is a fact only the frame that
 * made the write has. Deriving it later, from a store two writers have moved
 * since, is exactly the "best-effort guess" `inverseOp` refuses to make.
 *
 * **A write with no honest inverse is not pushed at all**, and `push` hands
 * back the [`Inverse`] so the caller can say so instead of offering an undo
 * that would do nothing. That is the same rule the rest of this module
 * follows: refuse visibly rather than fail quietly.
 *
 * **Nothing here is a transaction.** Popping an entry hands the caller an op
 * to send; whether it lands is the backend's answer, and the entry is gone
 * either way. That is deliberate — a failed undo (a tombstone past its purge
 * window, a scope at `ITEMS_MAX`) is reported to the human with the backend's
 * own message, and re-queuing it would offer a gesture that is now known not
 * to work.
 */
export class UndoStack {
  private entries: UndoEntry[] = [];

  /** How many undos are available. */
  get depth(): number {
    return this.entries.length;
  }

  /**
   * Record a write, and report whether it could be inverted.
   *
   * `before` is the item as the pane was rendering it; `applied` is what the
   * backend answered (needed for an `add`, whose inverse needs the id the
   * store minted).
   */
  push(forward: TodoOp, before: TodoItem | null, applied: Applied | TodoItem | null): Inverse {
    const inverse = inverseOp(forward, before, applied);
    if ("op" in inverse) {
      this.entries.push({ op: inverse.op, label: opLabel(forward) });
      // Drop from the OLDEST end, so the cap costs the least useful entry.
      if (this.entries.length > UNDO_MAX) this.entries.splice(0, this.entries.length - UNDO_MAX);
    }
    return inverse;
  }

  /** The most recent inverse, removed from the stack. */
  pop(): UndoEntry | null {
    return this.entries.pop() ?? null;
  }

  /** What `pop` would return, without removing it. */
  peek(): UndoEntry | null {
    return this.entries.length === 0 ? null : this.entries[this.entries.length - 1];
  }

  /**
   * Forget everything.
   *
   * Called when the pane changes SCOPE: every entry names an id in the list
   * the pane has just left, and the engine resolves an id without a scope
   * check — so an undo popped after the switch would write to the other
   * store while the header says otherwise. The same paint-the-wrong-scope
   * hazard `todoscope.ts` exists for, one gesture over.
   */
  clear(): void {
    this.entries = [];
  }
}
