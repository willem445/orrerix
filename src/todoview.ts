// The To-Do PANE's pure half (#3263 S4) — DOM-free, clock-injected, testable.
//
// TWO PURE MODULES, AND THE LINE BETWEEN THEM IS NOT ARBITRARY.
//
//   `todomodel.ts` (S3) is the STORE's model: what an item is, which smart view
//     it falls in, which Planned bucket, how an op is shaped, what undoes one.
//     Its readers are this pane AND #3263 S5's reminders/undo, and its
//     vocabulary is the backend's.
//   `todoview.ts` (here) is the PANE's projection: what is on SCREEN. The view
//     strip's counts, the groups the list renders, the row budget and the
//     elision line under it, the tag rail, and the per-viewer preferences that
//     decide which scope and which view a fresh pane opens on.
//
// The split is the S0 mock's own (its `render.js` §"two halves" — PR #3271; the
// mock tree was removed by #3315, so read it in that PR's diff), and it is what
// keeps `todopane.ts` a renderer rather than a place
// decisions hide. Everything the pane DECIDES is below; `todopane.ts` owns only
// elements.
//
// THE CLOCK IS A PARAMETER. Not one function here reads `Date.now()` — the
// reason `todomodel.ts` gives, unchanged: a "Today" bucket tested against the
// host clock is a different test every day.

// Explicit `.ts`: VALUE imports in a module `node --test` loads off disk rather
// than through Vite (the convention `todomodel.ts` and `panesetup.ts` state).
import {
  PLANNED_BUCKET_LABEL,
  SMART_VIEWS,
  groupPlanned,
  inView,
  isArchived,
  isDone,
  visibleItems,
  type PlannedBucket,
  type SmartView,
  type TodoItem,
} from "./todomodel.ts";

/**
 * The most rows the pane BUILDS at once.
 *
 * Inherited from the mock (DESIGN.md §8) with its argument intact: a year of
 * agent writes makes a list longer than any human reads, and rebuilding all of
 * it on every keystroke is the documented way to make a pane feel broken. It is
 * a BUDGET, not a virtual scroller — and the elision line is what keeps the
 * budget honest, because a pane that silently stops at 200 rows has lied about
 * how much work there is. A scroller is an argument for a later slice to make
 * with measurements in hand.
 */
export const ROW_BUDGET = 200;

/** Which list the pane is looking at. NOT the backend's `Scope`: that one names
 *  a workspace by KEY, and the frontend must never name a key (it sends a ROOT
 *  and the backend derives the key — `docs/design/todo-pane.md` §"The caller
 *  names a ROOT, never a key"). This is the SWITCH's two positions, and the
 *  pane turns `"workspace"` into the active root at the call site. */
export type ScopeChoice = "global" | "workspace";

/** One rendered group: the whole list under a single heading, or the only group
 *  when the view does not bucket. `label` null means "no heading" — the four
 *  non-Planned views are one flat list, and drawing a heading over the whole
 *  list would be chrome that says nothing. */
export interface RenderGroup {
  key: string;
  label: string | null;
  items: TodoItem[];
}

/** Why the list is empty — which decides the sentence the pane shows. A
 *  FILTERED empty list is a different fact from an empty view, and telling a
 *  human "nothing scheduled" when they have a search term typed is a lie the
 *  mock's §4 empty-state rule exists to avoid. */
export type EmptyReason = "filtered" | SmartView;

export interface PaneProjection {
  /**
   * Per-view row counts for the strip.
   *
   * **Read them only when [`countsKnown`] is true.** Before a snapshot has
   * landed for the scope the pane is on, every one of these is 0 because there
   * is nothing to count — not because there is nothing there — and a chip
   * reading `My Day 0` in that frame is the same lie as the empty-state
   * sentence the list already guards (#3293 round 6 residual 1).
   */
  counts: Record<SmartView, number>;
  /** Has a snapshot landed for this scope? When false, `counts`, `total` and
   *  `emptyReason` are all about a list nobody has read yet. */
  countsKnown: boolean;
  groups: RenderGroup[];
  /** Rows that MATCH, before the budget. The count chip shows this. */
  total: number;
  /** Rows actually in `groups`. */
  shown: number;
  /** `total - shown` — what the elision line reports. Never negative. */
  elided: number;
  /**
   * The tag rail: every tag on a live, open item in this scope, with how many
   * such items carry it (#3335). Most-used first, then by name, so the rail's
   * visible slots go to the tags that filter the most work.
   */
  tags: TagCount[];
  empty: boolean;
  emptyReason: EmptyReason;
}

/** One tag and how many open items in the scope carry it. */
export interface TagCount {
  tag: string;
  count: number;
}

export interface ProjectInput {
  items: readonly TodoItem[];
  view: SmartView;
  /** This view's "by priority" sort (#3335) — see `VisibleOpts.byPriority`. */
  byPriority?: boolean;
  /** Substring terms, ANDed. Blank is no filter. */
  query: string;
  /** One tag, exact, or null. */
  tagFilter: string | null;
  /** The Completed view's "show archived" toggle. Ignored by every other view
   *  — see `VisibleOpts.includeArchived` in `todomodel.ts`. */
  showArchived?: boolean;
  /**
   * Has a snapshot landed for the scope the pane is on?
   *
   * Absent is treated as TRUE, so every existing caller and test keeps its
   * meaning; the pane passes it explicitly. It exists because "we have not
   * looked" and "there is nothing" are different facts and only one of them is
   * safe to assert — the rule `todoscope.ts` states and the list's empty-state
   * already followed.
   */
  loaded?: boolean;
}

/**
 * The whole screen, as data.
 *
 * **The strip's counts are computed BEFORE the search and tag filters**, and
 * that is a decision rather than an ordering accident (the mock's `project`
 * makes it too). A chip whose number moves as you type tells you about your
 * QUERY; the strip is there to say how much work exists. Counting after the
 * filter would make "Planned 0" mean "your search matched nothing in Planned",
 * which is the one thing a human reads that number as never meaning.
 *
 * The budget is spent in DISPLAY order across the groups, so the rows dropped
 * are the ones furthest down the list the human is looking at — never a slice
 * taken out of the middle of a bucket that then renders a heading with a
 * truncated body and no sign of it.
 */
export function projectPane(input: ProjectInput, nowMs: number): PaneProjection {
  const counts = {} as Record<SmartView, number>;
  for (const v of SMART_VIEWS) {
    counts[v] = input.items.filter((i) => inView(i, v, nowMs)).length;
  }

  const rows = visibleItems(
    input.items,
    {
      view: input.view,
      query: input.query,
      tag: input.tagFilter,
      includeArchived: input.showArchived === true,
      byPriority: input.byPriority === true,
    },
    nowMs
  );

  let groups: RenderGroup[];
  if (input.view === "planned") {
    groups = groupPlanned(rows, nowMs, input.byPriority === true).map((g) => ({
      key: g.bucket,
      label: PLANNED_BUCKET_LABEL[g.bucket as PlannedBucket],
      items: g.items,
    }));
  } else {
    groups = rows.length > 0 ? [{ key: input.view, label: null, items: rows }] : [];
  }

  // Spend the budget in display order. A group that does not fit is TRUNCATED
  // rather than dropped whole: the heading is already the answer to "is there
  // anything overdue", and dropping the group would delete that answer to save
  // rows the elision line is about to account for anyway.
  let left = ROW_BUDGET;
  const windowed: RenderGroup[] = [];
  for (const g of groups) {
    if (left <= 0) break;
    windowed.push(g.items.length <= left ? g : { ...g, items: g.items.slice(0, left) });
    left -= g.items.length;
  }
  const shown = windowed.reduce((n, g) => n + g.items.length, 0);

  // The tag rail is built from the OPEN list in this scope, never from the
  // filtered rows: a rail that shrank to the tags of what you can already see
  // could not be used to widen the filter, which is the only thing it is for.
  //
  // COUNTED BY ITEM, from the same population (#3335): an item carrying one
  // tag twice (an agent's write, a hand edit) is one item, so the count is the
  // number of rows the filter would show — which is the only number a rail
  // chip's count can honestly promise.
  const tagCounts = new Map<string, number>();
  for (const i of input.items) {
    if (!inView(i, "all", nowMs)) continue;
    for (const t of new Set(i.tags)) tagCounts.set(t, (tagCounts.get(t) ?? 0) + 1);
  }
  const tags: TagCount[] = [...tagCounts]
    .map(([tag, count]) => ({ tag, count }))
    .sort((a, b) => b.count - a.count || (a.tag < b.tag ? -1 : a.tag > b.tag ? 1 : 0));

  const filtered = input.query.trim() !== "" || input.tagFilter !== null;
  return {
    counts,
    countsKnown: input.loaded !== false,
    groups: windowed,
    total: rows.length,
    shown,
    elided: rows.length - shown,
    tags,
    empty: rows.length === 0,
    emptyReason: filtered ? "filtered" : input.view,
  };
}

/** The sentence an empty list shows. A table rather than a ternary chain, so a
 *  sixth view is a row (the reason `CONTENT_KIND_LABEL` is one in `pane.ts`). */
export const EMPTY_TEXT: Record<EmptyReason, string> = {
  filtered: "Nothing matches.",
  myday: "Nothing for today — add one, or pull from Planned.",
  planned: "Nothing scheduled.",
  important: "Nothing starred.",
  all: "Empty list. The quick-add bar is the start.",
  completed: "Nothing completed yet.",
};

// ---------- per-viewer preferences ----------

/**
 * What a fresh todo pane opens on, per viewer.
 *
 * **Preferences, never items.** This is `localStorage`, which is per-browser,
 * per-device and invisible to every other writer — the exact reason
 * `docs/design/todo-pane.md` puts the LIST in a backend-owned store instead.
 * What lives here is the two conveniences a human would be annoyed to re-set on
 * every launch and would not miss if a wiped profile lost them: which scope the
 * switch is on, and which view the strip is on. `sidedockmodel.ts`'s dock prefs
 * are the precedent and the `loomux.*` key convention comes from there.
 *
 * `expanded` is the third, and it is deliberately NOT persisted — see
 * `TODO_PREFS_KEY`'s note below.
 */
export interface TodoPrefs {
  scope: ScopeChoice;
  view: SmartView;
  /**
   * The views whose "by priority" sort is ON (#3335) — per VIEW, because the
   * question differs by view: sorting Important by priority is a triage
   * reading, sorting My Day by it may not be what the human wants at all.
   *
   * Persisted, unlike the expanded set, because it IS a preference: "I read
   * this view by priority" is something a human sets once, not a reading
   * position. OFF by default, so the manual order — the one every drag writes
   * — is what a fresh pane shows.
   */
  byPriority: SmartView[];
}

export const DEFAULT_TODO_PREFS: TodoPrefs = { scope: "workspace", view: "myday", byPriority: [] };

/**
 * Where the prefs live.
 *
 * ONE record for the whole app, not one per pane. Two todo panes open on the
 * same workspace are two views of one list, and giving each its own remembered
 * view would mean a "last used" that depends on which pane you closed last —
 * which is not a preference, it is a coin flip.
 *
 * **Expanded rows are not in here.** Expansion is a reading position, not a
 * preference: an item expanded three weeks ago and since edited by an agent is
 * not something a human wants re-opened at launch, and the set would grow
 * without bound against ids the store may have purged. It lives in the view's
 * own `Set` for the pane's lifetime and is dropped with the pane.
 */
export const TODO_PREFS_KEY = "loomux.todo";

function isScopeChoice(v: unknown): v is ScopeChoice {
  return v === "global" || v === "workspace";
}

function isSmartView(v: unknown): v is SmartView {
  return typeof v === "string" && (SMART_VIEWS as readonly string[]).includes(v);
}

/**
 * Read the prefs back, tolerating anything.
 *
 * **Field-wise, not record-wise** — `decodeDockPrefs`'s rule, for its reason: a
 * malformed `view` costs the human their view choice and nothing else, where
 * discarding the whole record would throw away a good `scope` beside it after a
 * stray hand-edit or a build that wrote one extra field.
 *
 * Total by construction: it never throws and never returns a partial record, so
 * no caller needs a `try`/`catch` or a `??` chain around it. (The `try`/`catch`
 * callers DO need is around `localStorage` itself, which throws in a profile
 * with site data blocked — that is `todopane.ts`'s, not this function's.)
 */
export function decodeTodoPrefs(raw: string | null): TodoPrefs {
  if (raw === null || raw === "") return { ...DEFAULT_TODO_PREFS };
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return { ...DEFAULT_TODO_PREFS };
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    return { ...DEFAULT_TODO_PREFS };
  }
  const o = parsed as Record<string, unknown>;
  return {
    scope: isScopeChoice(o.scope) ? o.scope : DEFAULT_TODO_PREFS.scope,
    view: isSmartView(o.view) ? o.view : DEFAULT_TODO_PREFS.view,
    // Field-wise, and ENTRY-wise inside the field: an unknown view name (a
    // newer build's sixth view) is dropped and the rest kept, the leniency
    // `tabstore.decodePane` applies to one bad embed in an array.
    byPriority: Array.isArray(o.byPriority)
      ? SMART_VIEWS.filter((v) => (o.byPriority as unknown[]).includes(v))
      : [...DEFAULT_TODO_PREFS.byPriority],
  };
}

export function encodeTodoPrefs(p: TodoPrefs): string {
  return JSON.stringify({ scope: p.scope, view: p.view, byPriority: [...p.byPriority] });
}

/** Is the priority sort on for `view`? Completed never is — it is a log. */
export function sortsByPriority(p: TodoPrefs, view: SmartView): boolean {
  return view !== "completed" && p.byPriority.includes(view);
}

/** `p` with `view`'s priority sort flipped. Pure, so the pane's toggle is one
 *  assignment and the persisted list can never hold a view twice. */
export function togglePrioritySort(p: TodoPrefs, view: SmartView): TodoPrefs {
  const on = p.byPriority.includes(view);
  return {
    ...p,
    byPriority: SMART_VIEWS.filter((v) => (v === view ? !on : p.byPriority.includes(v))),
  };
}

// ---------- the quick-add draft, and the rest of the un-submitted state ----------

/**
 * Every value the human has typed and not yet submitted.
 *
 * **It lives HERE, in the view's own object, and never in an element** —
 * `CLAUDE.md`'s in-list-editor rule, and the one discipline the S0 mock put at
 * S0 precisely so this slice would inherit it rather than discover it. The
 * pane re-renders on EVERY `todo-changed`, including an agent's write through
 * MCP, and a re-render rebuilds the controls from their seeds: a note held in a
 * `<textarea>`'s `.value` is a note an agent's unrelated edit eats mid-sentence.
 *
 * So each field below is written on `input` (not read at submit), the renderer
 * seeds each control FROM here, and `isPristine` reads EVERY field — the
 * renderer's seed and the "is this untouched" question are one question asked
 * twice, and a field added to one and not the other is the defect #1348 N1/N4
 * names.
 */
export interface RowDraft {
  /** The row's notes as typed. */
  notes: string;
  /** The "next step" field under the step list. */
  step: string;
  /**
   * The in-row due-date field, as typed (#3263 S5): `fri 4pm`, `tomorrow`,
   * `next mon`. Parsed by `parseQuickAdd`, so the row and the quick-add bar
   * understand exactly the same grammar — one date vocabulary in this pane,
   * not two.
   *
   * **Seeded EMPTY, always**, which is why it has no `seededDue` twin the way
   * `notes` has `seededNotes`. The two fields are different kinds of thing: the
   * notes box is an EDITOR over a value the store holds, so "has the human
   * typed?" is a question about its seed; this is an INSTRUCTION field, like
   * `step`. The item's current due date is drawn beside it as a label, and
   * rendering a timestamp back into the phrase someone might have typed is
   * lossy in a way that would make a pristine draft read as dirty.
   */
  due: string;
  /**
   * The in-row "add a tag" field, as typed (#3335). An INSTRUCTION field like
   * `step` and `due`, so seeded empty: the item's current tags are drawn as
   * chips beside it, each with its own remove control.
   */
  tag: string;
  /**
   * The item's `notes` AT THE MOMENT this draft was seeded.
   *
   * "Has the human typed?" is a question about the draft against its own SEED,
   * not against the item's value right now — and the two differ exactly when a
   * second writer has been at the item, which is this pane's normal condition
   * rather than an edge case. Comparing against the live item instead makes an
   * untouched draft read as DIRTY the moment an agent edits the row, which is
   * both wrong on its own terms and the reason `reseedPristineDrafts` could
   * never fire without this field (#3293 review round 2).
   */
  seededNotes: string;
}

export const EMPTY_ROW_DRAFT: RowDraft = { notes: "", step: "", due: "", tag: "", seededNotes: "" };

/**
 * Has the human typed into this draft?
 *
 * Measured against the draft's OWN seed (`seededNotes`), never against the
 * item's current value — see that field's doc. An item WITH notes seeds a
 * non-empty box, and calling that dirty would make every expanded row look
 * edited; an agent's write to the item must not make it look edited either.
 *
 * Reads every field of `RowDraft` that the human can type into. A typable field
 * added to the interface without a line here is the asymmetry the doc above
 * names, and `tsc` will not catch it, which is why `test/todoview.test.ts`
 * drives its check off the object's own keys.
 */
export function rowDraftIsPristine(draft: RowDraft): boolean {
  return (
    draft.notes === draft.seededNotes && draft.step === "" && draft.due === "" && draft.tag === ""
  );
}

/** The draft a freshly expanded row starts with: seeded from the ITEM, and
 *  recording what it was seeded from, so `rowDraftIsPristine` is true the
 *  instant it is created and stays true until the human types. */
export function seedRowDraft(item: TodoItem): RowDraft {
  return { notes: item.notes, step: "", due: "", tag: "", seededNotes: item.notes };
}

/**
 * Re-seed every PRISTINE draft from the item it is a draft of, and report how
 * many moved.
 *
 * **The silent-revert this closes** (#3293 review round 2, both reviewers'
 * premortem 1). A draft is seeded from the item when the row is expanded and
 * then never re-read. If an AGENT edits that item's notes underneath — through
 * the MCP tools, which is the whole premise of this pane — the row re-renders
 * from the draft, so the human sees their own stale text, and `commitDraft`'s
 * `draft.notes !== item.notes` is then TRUE against the agent's new value. Save
 * ships the pre-agent notes and reverts a write the human never saw. It is
 * legal under last-writer-wins and invisible until something surfaces versions,
 * which is exactly what makes it worth closing here.
 *
 * **Only pristine drafts move.** A draft the human has typed into is theirs and
 * is never touched — that is the in-list-editor rule, and re-seeding a dirty
 * draft would be the very defect this module exists to prevent, one direction
 * over. The consequence is honest rather than hidden: a human mid-sentence when
 * an agent writes still overwrites that write on Save. Surfacing a genuine
 * conflict needs the item's `rev` and a decision about what to show, which is
 * #3263 S5's (the store already carries `rev` and `if_rev` for it).
 *
 * Returns the number re-seeded, so a caller can assert the mechanism RAN rather
 * than asserting only that nothing was clobbered — an absence-only pin here
 * would pass just as well against a function that never looked (#1209).
 */
export function reseedPristineDrafts(
  drafts: Map<string, RowDraft>,
  items: readonly TodoItem[]
): number {
  const byId = new Map(items.map((i) => [i.id, i]));
  let moved = 0;
  for (const [id, draft] of drafts) {
    const item = byId.get(id);
    if (item === undefined) continue; // `pruneDrafts` owns the gone case
    if (!rowDraftIsPristine(draft)) continue;
    if (item.notes === draft.seededNotes) continue; // nothing moved underneath
    drafts.set(id, seedRowDraft(item));
    moved += 1;
  }
  return moved;
}

/**
 * Prune drafts for rows that are no longer on screen.
 *
 * Called beside the expanded set, for the reason `pruneViewState` exists in the
 * structured pane and `BoardPrefsStore` prunes its own maps: a map keyed by item
 * id that nothing ever removes from grows for the life of the pane against ids
 * an agent may have deleted — and a stale draft is worse than a leak, because a
 * re-created id would inherit a stranger's half-typed note.
 *
 * Mutates and returns nothing: the caller owns the map.
 */
export function pruneDrafts(drafts: Map<string, RowDraft>, liveIds: ReadonlySet<string>): void {
  for (const id of [...drafts.keys()]) {
    if (!liveIds.has(id)) drafts.delete(id);
  }
}

// ---------- revealing a row a toast pointed at ----------

/**
 * What the reminder toast's "Show" gesture can actually do.
 *
 * **Because the answer can be "nothing", and the pane used to do nothing
 * silently** (#3301 review round 2, finding 3). A notice is created by a scan
 * that skips done and archived items — but the human clicks it LATER, and an
 * agent's `todo_complete` or `todo_archive` in that window moves the row out
 * of every view. The pane then cleared the filters, fell back to All, set a
 * selection nothing rendered and called `scrollIntoView` on a selector
 * matching nothing: the toast dismissed and the screen did not change.
 *
 * Only an item that was GONE entirely got an explanation, which is the rarer
 * case — a delete — while the likelier one, a row finished a minute ago, was
 * the silent one.
 *
 * Pure, so the decision is testable without a DOM; `todopane.ts` owns only
 * the toast and the scroll.
 */
export type RevealPlan =
  /** The row is reachable. `view` is the view to move to, or null to stay. */
  | { kind: "reveal"; view: SmartView | null }
  /** No such item in this scope — deleted, or another list's. */
  | { kind: "gone" }
  /** The item is still here but has left every view since the notice. */
  | { kind: "left"; why: "done" | "archived" };

/**
 * Decide what "Show" should do for `id`.
 *
 * `currentRendered` is what the pane is showing RIGHT NOW (the flattened
 * projection). If the row is already on screen, no view change is needed —
 * which matters because moving the view is itself a visible jump, and doing it
 * when the row was already in front of the human is noise.
 *
 * The fallback is `all`, the view that holds every open item. It is returned
 * rather than applied, so the caller decides whether that counts as a
 * preference (it does not — see `todopane.ts`'s `setView(…, {persist: false})`).
 *
 * **No clock**, unlike every other function in this module and its sibling.
 * The three questions it asks — is the item here, is it archived, is it done —
 * are all timeless, and the one thing that would have needed a reading was the
 * dead `inView` branch removed below. A parameter kept "because everything
 * else takes one" would be a claim that this decision can move at midnight,
 * which it cannot.
 */
export function planReveal(
  items: readonly TodoItem[],
  id: string,
  currentRendered: readonly TodoItem[]
): RevealPlan {
  const item = items.find((i) => i.id === id);
  if (item === undefined) return { kind: "gone" };
  if (isArchived(item)) return { kind: "left", why: "archived" };
  if (isDone(item)) return { kind: "left", why: "done" };
  if (currentRendered.some((i) => i.id === id)) return { kind: "reveal", view: null };
  // Reachable, but not on screen. All holds every open item, and the two
  // guards above have already established this one is open — so the fallback
  // is unconditional rather than re-asking `inView`.
  //
  // An earlier revision DID re-ask it (`inView(item, "all", nowMs) ? "all" :
  // null`). That branch was unreachable, and a mutation run proved it: forcing
  // the ternary to its true arm reddened NOTHING, which is what dead defensive
  // code looks like from the outside. The property it was reaching for is
  // pinned where it belongs instead — `planReveal never answers 'reveal' for a
  // row All would not hold` asserts `inView` agrees, over every item shape
  // this module can build, so an `inView` change that dropped a class from All
  // reddens there rather than being silently absorbed here.
  return { kind: "reveal", view: "all" };
}

// ---------- selection ----------

/**
 * Where `j` / `k` land.
 *
 * Over the FLATTENED rendered rows — what the human can see, in the order they
 * see it — so a move crosses a Planned bucket heading exactly as the eye does.
 * `null` selection enters at the first row for a downward move and the last for
 * an upward one, which is what makes `j` on a fresh pane select the top rather
 * than doing nothing.
 *
 * Clamps rather than wraps. Wrapping from the last row to the first is how a
 * held key silently takes you somewhere you were not looking, and a list is not
 * a carousel.
 */
export function moveSelection(
  rendered: readonly TodoItem[],
  selected: string | null,
  delta: -1 | 1
): string | null {
  if (rendered.length === 0) return null;
  if (selected === null) return (delta > 0 ? rendered[0] : rendered[rendered.length - 1]).id;
  const ix = rendered.findIndex((i) => i.id === selected);
  // A selection the render dropped (an agent completed the row out from under
  // the human): re-enter at the edge rather than refusing to move.
  if (ix < 0) return (delta > 0 ? rendered[0] : rendered[rendered.length - 1]).id;
  const to = Math.min(rendered.length - 1, Math.max(0, ix + delta));
  return rendered[to].id;
}

/**
 * Does a click on a row toggle its expansion? (#3335 AC 6)
 *
 * The row is one big target, so the question is really "which clicks are NOT
 * for it":
 *
 *  - a click that landed on a CONTROL (anything carrying `data-act` — the
 *    checkbox, a tag chip, the priority flag, the star, the chevron) does its
 *    own job and must not also fold the row open or shut;
 *  - a click inside the EXPANDED BODY (the notes box, the step list, a colour
 *    swatch's gaps) is work on the row, not a request to close it — folding a
 *    row shut because the human clicked beside the notes they were reading
 *    would be the pane fighting them;
 *  - a click that ENDED A TEXT SELECTION is someone copying a title;
 *  - a click that ended a DRAG is the drop, not a click.
 *
 * Everything else in the row's head — the title, the meta line's gaps, the
 * padding — toggles. DOM-free so the four exclusions are pinned rather than
 * remembered; the pane computes each input from the event.
 */
export function rowClickToggles(c: {
  /** Did the click land on (or inside) an element carrying `data-act`? */
  onControl: boolean;
  /** Is it inside a row's head (the collapsed row's own area)? */
  inHead: boolean;
  /** Does the document hold a non-empty text selection? */
  selecting: boolean;
  /** Did a row drag just end on this gesture? */
  dragged: boolean;
}): boolean {
  return c.inHead && !c.onControl && !c.selecting && !c.dragged;
}

/** Every row in a projection, flattened in display order. The selection walks
 *  this, and so does the keyboard's notion of "the selected row". */
export function renderedRows(p: PaneProjection): TodoItem[] {
  return p.groups.flatMap((g) => g.items);
}
