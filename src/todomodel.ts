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
 *  in different languages, and `needsRenumber` is the test that notices if
 *  they ever disagree about whether there is room. */
export const ORDER_GAP = 1024;

const MS_PER_DAY = 86400000;

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

/** True when an item is in My Day but was put there on an earlier day.
 *
 *  SEPARATE from `inView(item, "myday", …)` on purpose. Microsoft To Do empties
 *  My Day at midnight; whether this one should is the open question in #3263's
 *  plan §8, so the predicate takes the non-destructive reading (a carried-over
 *  item stays) and this reports the staleness for the pane to surface once the
 *  human has answered. Nothing here clears anything. */
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
 * is in none of them — that is what archiving is for (#3263 S5 adds the op).
 *
 * **No view reads the clock TODAY**, hence `_nowMs`. The parameter is kept
 * because the one open product question would make My Day read it the moment
 * it is answered (#3263 plan §8 — whether My Day empties itself at midnight,
 * as Microsoft To Do does), and because every caller already threads a clock
 * through `visibleItems`. Dropping it would mean changing every call site to
 * put it back.
 */
export function inView(item: TodoItem, view: SmartView, _nowMs: number): boolean {
  if (isArchived(item)) return false;
  switch (view) {
    case "completed":
      return isDone(item);
    case "myday":
      return !isDone(item) && item.my_day !== null;
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
export function groupPlanned(items: readonly TodoItem[], nowMs: number): PlannedGroup[] {
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
    list.sort((a, b) => (a.due_ms ?? 0) - (b.due_ms ?? 0) || compareFallback(a, b));
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
      inView(i, opts.view, nowMs) &&
      matchesQuery(i, opts.query ?? "") &&
      (tag === null || i.tags.includes(tag))
  );
  if (opts.view === "completed") {
    out.sort((a, b) => (b.done_ms ?? 0) - (a.done_ms ?? 0) || compareFallback(a, b));
  } else {
    out.sort((a, b) => a.order - b.order || compareFallback(a, b));
  }
  return out;
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
 * True when some adjacent pair in `ordered` has no integer strictly between
 * their `order` values.
 *
 * The backend places a moved item at the midpoint of its new neighbours
 * (`order_for` in `loomux_engine::todo`). After enough halvings the midpoint
 * equals the neighbour and the move becomes a SILENT no-op — the item does not
 * budge and nothing reports why. This is what lets the pane notice and ask for
 * a renumber instead.
 */
export function needsRenumber(ordered: readonly TodoItem[]): boolean {
  for (let i = 1; i < ordered.length; i++) {
    if (ordered[i].order - ordered[i - 1].order < 2) return true;
  }
  return false;
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
  steps?: { id?: string; title: string; done: boolean }[];
  order_after?: OrderAfter;
}

/** One mutation, in the single-key shape `parse_op` accepts. */
export type TodoOp =
  | { add: AddFields }
  | { update: UpdateFields }
  | { complete: { id: string; done: boolean } }
  | { delete: { id: string } };

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
 * **It refuses rather than guesses.** Three cases have no honest inverse today
 * and each says so:
 *
 *  * a DELETE. The store's delete is a soft tombstone, and the engine HAS a
 *    `restore` op that inverts one as of #3285 — what is missing is the
 *    wiring here, which is S5's. Emitting a `restore` from this function
 *    before the frontend `TodoOp` type carries it would not type-check; the
 *    BACKEND decoder already accepts the op, so what is outstanding is a type
 *    and a caller, not a decoder arm. It still reports the gap; only the
 *    reason has changed, and the reason is what the message says.
 *
 *    (The MCP side is a separate gap and not this function's: #3263 S2
 *    shipped its six tools before `restore` existed, so no agent can undo its
 *    own `todo_delete` either. `doc/design/todo-pane.md`, "No seventh tool".)
 *  * no `before` snapshot. Without it the pane cannot know what to restore,
 *    and a best-effort guess is how an undo quietly writes the wrong value.
 *  * an update that named no field. There is nothing to put back.
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
    return {
      unsupported:
        "undoing a delete is not wired up yet; the store's restore op exists (#3285) and S5 wires it",
    };
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
