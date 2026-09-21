// demo/todo-pane — state → DOM for the To-Do pane mock (#3263, plan §4).
//
// TWO HALVES, ON PURPOSE.
//
//   `project(state)`  — PURE. Store state in, a plain view-model out. No DOM,
//                       no clock, no globals. This is the half S3 lifts into
//                       `src/todomodel.ts`: the smart-view predicates, the
//                       Planned date buckets, the ordering and the counts.
//   `renderInto(...)` — the DOM builder. It reads the view-model and nothing
//                       else, so every question of "what is on screen" is
//                       answerable from `project` without a browser.
//
// The split is the point. A renderer that also decides what My Day contains is
// a renderer that can only be tested by mounting it, and the pane's real
// content rules — overdue, buckets, ordering — are exactly the rules worth
// testing without one.
//
// NO STATE LIVES IN AN ELEMENT. Expansion, selection and the quick-add draft
// are fields on `state`; the DOM is a view of them and is rebuilt wholesale on
// every change. That is CLAUDE.md's in-list-editor rule, obeyed here so S4
// inherits the shape rather than discovering it.

import { parseQuickAdd, formatDue, addDays } from "./quickadd.js";

const MS_PER_DAY = 86400000;

/**
 * The most rows the pane will BUILD at once.
 *
 * A year of agent writes makes a list longer than any human reads, and a
 * renderer that rebuilds the whole of it on every keystroke is the documented
 * way to make a pane feel broken. So the list is windowed — and the elision is
 * STATED ON SCREEN, never hidden, which is the same rule the structured-pane
 * mock holds for its transcript. The number below the window says how many
 * rows it is not showing and what to do about it.
 *
 * 200 is a budget, not a virtual scroller: a scroller is S5's argument to
 * make, and a budget is the honest thing to show a human at S0.
 */
export const ROW_BUDGET = 200;

export const VIEWS = [
  { id: "myday", label: "My Day", key: "1" },
  { id: "planned", label: "Planned", key: "2" },
  { id: "important", label: "Important", key: "3" },
  { id: "all", label: "All", key: "4" },
  { id: "completed", label: "Completed", key: "5" },
];

/* ==========================================================================
 * Fixture materialisation
 *
 * A fixture authored with ABSOLUTE due dates is a mock that reads as entirely
 * overdue a fortnight after it was written, which teaches the reviewer the
 * wrong thing about the overdue dye. So a fixture's `due` is a DAY OFFSET plus
 * an optional wall time, and the day it resolves against is the demo clock.
 * ==========================================================================*/

function startOfDay(ms) {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** `{ day: -1, hm: "16:00" }` → epoch ms, anchored on `nowMs`'s local day. */
function resolveDue(due, nowMs) {
  if (!due) return { dueMs: null, hasTime: false };
  const day = addDays(startOfDay(nowMs), due.day || 0);
  const [h, m] = (due.hm || "09:00").split(":").map(Number);
  const d = new Date(day);
  d.setHours(h, m, 0, 0);
  return { dueMs: d.getTime(), hasTime: Boolean(due.hm) };
}

/** Fixture JSON → the item list the rest of this module works on. */
export function materialise(fixture, nowMs) {
  return (fixture.items || []).map((raw, i) => {
    const { dueMs, hasTime } = resolveDue(raw.due, nowMs);
    return {
      id: raw.id || `td-${i}`,
      scope: raw.scope === "global" ? "global" : "workspace",
      title: raw.title || "",
      done: Boolean(raw.done),
      completedMs: raw.done ? nowMs - (raw.completed_ago_min || 0) * 60000 : null,
      dueMs,
      hasTime,
      myDay: Boolean(raw.my_day),
      important: Boolean(raw.important),
      priority: raw.priority || 0,
      tags: raw.tags || [],
      notes: raw.notes || "",
      steps: (raw.steps || []).map((s) => ({ title: s.title, done: Boolean(s.done) })),
      order: raw.order != null ? raw.order : (i + 1) * 100,
      deleted: Boolean(raw.deleted),     // SOFT. A deleted row keeps its record,
                                         // which is the whole reason undo can reach it.
      actor: raw.actor || null,          // null = the human did it
      updatedAgoMin: raw.updated_ago_min != null ? raw.updated_ago_min : null,
    };
  });
}

/* ==========================================================================
 * The pure half — smart views, buckets, counts
 * ==========================================================================*/

export function isOverdue(item, nowMs) {
  return !item.done && item.dueMs != null && item.dueMs < nowMs;
}

/** Planned's date buckets, in the order the view shows them. */
export function plannedBucket(item, nowMs) {
  const today = startOfDay(nowMs);
  const day = startOfDay(item.dueMs);
  if (day < today) return "overdue";
  if (day === today) return "today";
  if (day === addDays(today, 1)) return "tomorrow";
  if (day < addDays(today, 7)) return "week";
  return "later";
}

const BUCKET_LABEL = {
  overdue: "Overdue",
  today: "Today",
  tomorrow: "Tomorrow",
  week: "This week",
  later: "Later",
};

/** Substring search over title, notes and tags — `filematch.ts` term semantics. */
export function matchesSearch(item, query) {
  const terms = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (!terms.length) return true;
  const hay = (item.title + " " + item.notes + " " + item.tags.map((t) => "#" + t).join(" ")).toLowerCase();
  return terms.every((t) => hay.includes(t));
}

// Every view excludes a soft-deleted row. It is excluded HERE, in the one
// place the views are defined, rather than at each predicate's call site: a
// filter that has to be remembered five times is a filter that will be
// forgotten once.
const notDeleted = (it) => !it.deleted;

const inView = {
  myday: (it) => notDeleted(it) && !it.done && it.myDay,
  planned: (it) => notDeleted(it) && !it.done && it.dueMs != null,
  important: (it) => notDeleted(it) && !it.done && it.important,
  all: (it) => notDeleted(it) && !it.done,
  completed: (it) => notDeleted(it) && it.done,
};

/** The sort every view but Completed uses: overdue first, then due, then manual order. */
function compareItems(a, b, nowMs) {
  const ao = isOverdue(a, nowMs) ? 0 : 1;
  const bo = isOverdue(b, nowMs) ? 0 : 1;
  if (ao !== bo) return ao - bo;
  if (a.important !== b.important) return a.important ? -1 : 1;
  if ((a.dueMs != null) !== (b.dueMs != null)) return a.dueMs != null ? -1 : 1;
  if (a.dueMs != null && a.dueMs !== b.dueMs) return a.dueMs - b.dueMs;
  return a.order - b.order;
}

/**
 * PURE. The whole screen, as data.
 *
 * @param {{items: object[], view: string, scope: "global"|"workspace",
 *          query: string, nowMs: number, draft: string, tagFilter: string|null}} state
 */
export function project(state) {
  const { items, view, scope, query, nowMs } = state;

  const inScope = items.filter((it) => it.scope === scope);

  // View counts are computed BEFORE the search and tag filters: a chip whose
  // number moves as you type tells you about your query, not about your list,
  // and the strip is there to say how much work exists.
  const counts = {};
  for (const v of VIEWS) counts[v.id] = inScope.filter(inView[v.id]).length;

  let rows = inScope
    .filter(inView[view] || inView.all)
    .filter((it) => matchesSearch(it, query))
    .filter((it) => !state.tagFilter || it.tags.includes(state.tagFilter));

  rows = rows.slice().sort((a, b) =>
    view === "completed" ? (b.completedMs || 0) - (a.completedMs || 0) : compareItems(a, b, nowMs));

  /** @type {{key: string, label: string|null, items: object[]}[]} */
  let groups;
  if (view === "planned") {
    const order = ["overdue", "today", "tomorrow", "week", "later"];
    groups = order
      .map((key) => ({ key, label: BUCKET_LABEL[key], items: rows.filter((it) => plannedBucket(it, nowMs) === key) }))
      .filter((g) => g.items.length > 0);
  } else {
    groups = rows.length ? [{ key: view, label: null, items: rows }] : [];
  }

  // Window the groups, in display order, to the row budget.
  let left = ROW_BUDGET;
  const windowed = [];
  for (const g of groups) {
    if (left <= 0) break;
    windowed.push(g.items.length <= left ? g : { ...g, items: g.items.slice(0, left) });
    left -= g.items.length;
  }
  const shown = windowed.reduce((n, g) => n + g.items.length, 0);
  groups = windowed;

  const tags = [...new Set(inScope.filter(inView.all).flatMap((it) => it.tags))].sort();

  return {
    counts,
    groups,
    total: rows.length,
    shown,
    elided: rows.length - shown,
    tags,
    parse: parseQuickAdd(state.draft, nowMs),
    empty: rows.length === 0,
    emptyReason: query || state.tagFilter ? "filtered" : view,
  };
}

/* ==========================================================================
 * The DOM half
 * ==========================================================================*/

function el(tag, attrs, ...kids) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === false || v == null) continue;
    if (k === "class") node.className = v;
    else if (k === "text") node.textContent = v;
    else node.setAttribute(k, v === true ? "" : String(v));
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    node.append(kid);
  }
  return node;
}

const CHECK = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M5 13l4.5 4.5L19 7"/></svg>`;
const STAR = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round" aria-hidden="true"><path d="M12 3.6l2.6 5.5 5.9.8-4.3 4.2 1 5.9-5.2-2.8-5.2 2.8 1-5.9L3.5 9.9l5.9-.8z"/></svg>`;
const SUN = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true"><circle cx="12" cy="12" r="4"/><path d="M12 2.5v2M12 19.5v2M2.5 12h2M19.5 12h2M5.2 5.2l1.4 1.4M17.4 17.4l1.4 1.4M18.8 5.2l-1.4 1.4M6.6 17.4l-1.4 1.4"/></svg>`;
const CHEV = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 6l6 6-6 6"/></svg>`;

function svg(markup, cls) {
  const span = el("span", { class: cls, "aria-hidden": "true" });
  span.innerHTML = markup;
  return span;
}

/** The meta line under a title: due, steps, tags, My Day. */
function metaLine(item, nowMs) {
  const bits = [];

  if (item.dueMs != null) {
    bits.push(el("span", {
      class: "meta-due",
      // STATE CHANNEL, ONE POSITION. The attention dye appears on a due date
      // and only when it is genuinely late. A due date that is merely soon is
      // ink, not a colour: if everything upcoming is amber, nothing is.
      "data-overdue": isOverdue(item, nowMs) ? "true" : "false",
      text: formatDue(item.dueMs, nowMs, item.hasTime),
    }));
  }

  if (item.steps.length) {
    const done = item.steps.filter((s) => s.done).length;
    bits.push(el("span", { class: "meta-steps num", text: `${done}/${item.steps.length}` }));
  }

  for (const tag of item.tags) {
    bits.push(el("button", { class: "meta-tag", type: "button", "data-tag": tag, text: "#" + tag }));
  }

  if (item.myDay) bits.push(svg(SUN, "meta-sun"));

  if (item.notes) bits.push(el("span", { class: "meta-note", text: "note" }));

  if (!bits.length) return null;
  const line = el("div", { class: "meta" });
  bits.forEach((b, i) => {
    if (i) line.append(el("span", { class: "meta-sep", text: "·" }));
    line.append(b);
  });
  return line;
}

/**
 * The attribution mark: a 6 px dot in the agent's IDENTITY hue.
 *
 * Identity channel, identity question — "which agent touched this row" is a
 * which-thing question, never a state one, so no `--state-*` token may appear
 * here and this dot may never be read as a status. A human-authored row has no
 * dot at all: absence is the human, which keeps the fleet's marks meaning one
 * thing rather than six.
 */
function attribution(item) {
  if (!item.actor) return null;
  const dot = el("span", {
    class: "attr-dot",
    style: `--attr: var(--${item.actor.hue})`,
    title: `${item.actor.id} — ${item.actor.cli || "agent"}`,
  });
  return dot;
}

function rowNode(item, state) {
  const expanded = state.expanded === item.id;
  const row = el("div", {
    class: "row",
    "data-id": item.id,
    "data-done": item.done ? "true" : "false",
    "data-selected": state.selected === item.id ? "true" : "false",
    "data-expanded": expanded ? "true" : "false",
    "data-priority": item.priority || 0,
    role: "listitem",
  });

  const head = el("div", { class: "row-head" });

  const box = el("button", {
    class: "check",
    type: "button",
    "data-act": "toggle",
    "aria-pressed": item.done ? "true" : "false",
    "aria-label": (item.done ? "Mark incomplete: " : "Complete: ") + item.title,
  }, svg(CHECK, "check-mark"));
  head.append(box);

  const main = el("div", { class: "row-main" });
  const titleLine = el("div", { class: "title-line" });
  const dot = attribution(item);
  if (dot) titleLine.append(dot);
  titleLine.append(el("span", { class: "title", text: item.title }));
  main.append(titleLine);

  if (item.actor && item.updatedAgoMin != null) {
    main.append(el("div", {
      class: "byline",
      text: `${item.actor.id} · ${item.updatedAgoMin < 60 ? item.updatedAgoMin + "m" : Math.round(item.updatedAgoMin / 60) + "h"}`,
    }));
  }

  const meta = metaLine(item, state.nowMs);
  if (meta) main.append(meta);
  head.append(main);

  head.append(el("button", {
    class: "star",
    type: "button",
    "data-act": "important",
    "aria-pressed": item.important ? "true" : "false",
    "aria-label": "Important",
  }, svg(STAR, "star-mark")));

  head.append(el("button", {
    class: "chev",
    type: "button",
    "data-act": "expand",
    "aria-expanded": expanded ? "true" : "false",
    "aria-label": "Details",
  }, svg(CHEV, "chev-mark")));

  row.append(head);

  // INLINE expansion, never a side panel. A todo pane is a grid cell and can
  // be 320 px wide; a detail panel at that width is a modal with extra steps.
  if (expanded) {
    const body = el("div", { class: "row-body" });

    if (item.steps.length) {
      const list = el("div", { class: "steps" });
      item.steps.forEach((s, i) => {
        list.append(el("button", {
          class: "step", type: "button", "data-act": "step", "data-step": i,
          "data-done": s.done ? "true" : "false",
        }, svg(CHECK, "step-mark"), el("span", { text: s.title })));
      });
      body.append(list);
    }
    body.append(el("input", {
      class: "step-add", type: "text", "data-act": "step-add",
      placeholder: "Next step", value: "",
    }));

    body.append(el("textarea", {
      class: "notes", "data-act": "notes", rows: "2",
      placeholder: "Notes", text: item.notes,
    }));

    const controls = el("div", { class: "row-controls" });
    for (const [act, label] of [["due", item.dueMs ? formatDue(item.dueMs, state.nowMs, item.hasTime) : "Add due date"],
                                ["myday", item.myDay ? "In My Day" : "Add to My Day"],
                                ["delete", "Delete"]]) {
      controls.append(el("button", {
        class: "rowbtn", type: "button", "data-act": act,
        "data-on": (act === "myday" && item.myDay) || (act === "due" && item.dueMs) ? "true" : "false",
        text: label,
      }));
    }
    body.append(controls);
    row.append(body);
  }

  return row;
}

function emptyNode(vm) {
  const text = {
    filtered: "Nothing matches.",
    myday: "Nothing for today — add one, or pull from Planned.",
    planned: "Nothing scheduled.",
    important: "Nothing starred.",
    all: "Empty list. The quick-add bar is the start.",
    completed: "Nothing completed yet.",
  }[vm.emptyReason] || "Empty.";
  return el("div", { class: "empty" },
    el("p", { class: "empty-text", text }),
    el("p", { class: "empty-hint", text: "Press n to add · / to search · 1-5 to switch view" }));
}

/**
 * Build the pane into `root` from `state`. Handlers are attached by `main.js`
 * through delegation on `root`, so this function is a pure write of the DOM.
 */
export function renderInto(root, state) {
  const vm = project(state);
  const frag = document.createDocumentFragment();

  /* ---------------- header: scope switch, search, count ---------------- */
  const head = el("header", { class: "thead" });

  const scope = el("div", { class: "scope", role: "group", "aria-label": "Scope" });
  scope.append(el("button", {
    class: "scope-btn", type: "button", "data-act": "scope", "data-scope": "global",
    "aria-pressed": state.scope === "global" ? "true" : "false",
  }, el("span", { class: "scope-mark", text: "◐" }), el("span", { text: "Global" })));
  scope.append(el("button", {
    class: "scope-btn", type: "button", "data-act": "scope", "data-scope": "workspace",
    "aria-pressed": state.scope === "workspace" ? "true" : "false",
    title: state.workspace.root,
  }, el("span", { class: "scope-mark", text: "◆" }), el("span", { text: state.workspace.label })));
  head.append(scope);

  head.append(el("button", {
    class: "rootbtn", type: "button", "data-act": "root",
    "aria-expanded": state.showRoot ? "true" : "false",
    "aria-label": "Show workspace path", text: "⋯",
  }));
  if (state.showRoot) head.append(el("code", { class: "rootpath mono", text: state.workspace.root }));

  const search = el("div", { class: "search", "data-open": state.searchOpen ? "true" : "false" });
  search.append(el("input", {
    class: "search-in", type: "text", "data-act": "search",
    placeholder: "Search", value: state.query, "aria-label": "Search",
  }));
  head.append(search);

  head.append(el("span", { class: "grow" }));
  head.append(el("span", { class: "countchip num", text: String(vm.total) }));
  frag.append(head);

  /* ---------------------------- view strip ----------------------------- */
  const strip = el("nav", { class: "strip", "aria-label": "Views" });
  for (const v of VIEWS) {
    strip.append(el("button", {
      class: "chip", type: "button", "data-act": "view", "data-view": v.id,
      "aria-pressed": state.view === v.id ? "true" : "false",
    }, el("span", { text: v.label }), el("span", { class: "chip-n num", text: String(vm.counts[v.id]) })));
  }
  frag.append(strip);

  if (state.tagFilter) {
    const bar = el("div", { class: "filterbar" });
    bar.append(el("button", { class: "filter-clear", type: "button", "data-act": "cleartag", text: `#${state.tagFilter} ✕` }));
    frag.append(bar);
  }

  /* ---------------------------- quick-add ------------------------------ */
  const qa = el("div", { class: "quickadd", "data-valid": vm.parse.valid ? "true" : "false" });
  qa.append(el("input", {
    class: "qa-in", type: "text", "data-act": "draft",
    placeholder: "Add a task — try “ship notes tomorrow at 4pm #release !!”",
    value: state.draft, "aria-label": "Quick add",
  }));
  const chips = el("div", { class: "qa-chips" });
  for (const chip of vm.parse.chips) {
    chips.append(el("span", { class: "qa-chip", "data-kind": chip.kind, text: chip.label }));
  }
  qa.append(chips);
  frag.append(qa);

  /* ------------------------------- list -------------------------------- */
  const list = el("div", { class: "list", role: "list" });
  if (vm.empty) {
    list.append(emptyNode(vm));
  } else {
    for (const group of vm.groups) {
      if (group.label) {
        list.append(el("div", {
          class: "bucket", "data-bucket": group.key,
        }, el("span", { text: group.label }), el("span", { class: "bucket-n num", text: String(group.items.length) })));
      }
      for (const item of group.items) list.append(rowNode(item, state));
    }
    if (vm.elided > 0) {
      // State the elision. A pane that silently stops at 200 rows is a pane
      // that has lied about how much work there is.
      list.append(el("div", { class: "elision" },
        el("span", { class: "num", text: String(vm.elided) }),
        el("span", { text: " more not shown — narrow with / or a #tag" })));
    }
  }
  frag.append(list);

  /* ---------------------------- footer --------------------------------- */
  const foot = el("footer", { class: "tfoot" });
  if (state.view === "completed" && vm.total) {
    foot.append(el("button", { class: "rowbtn", type: "button", "data-act": "archiveall", text: "Archive all" }));
  }
  if (vm.tags.length) {
    const tagrow = el("div", { class: "tagrow" });
    for (const t of vm.tags.slice(0, 8)) {
      tagrow.append(el("button", {
        class: "tagbtn", type: "button", "data-act": "tag", "data-tag": t,
        "data-on": state.tagFilter === t ? "true" : "false", text: "#" + t,
      }));
    }
    foot.append(tagrow);
  }
  foot.append(el("span", { class: "grow" }));
  foot.append(el("span", { class: "hint", text: "n add · j/k move · space done · e details · u undo" }));
  frag.append(foot);

  root.replaceChildren(frag);

  if (state.toast) {
    const toast = el("div", { class: "toast", role: "status" },
      el("span", { text: state.toast.text }),
      el("button", { class: "toast-undo", type: "button", "data-act": "undo", text: "Undo" }));
    root.append(toast);
  }

  return vm;
}
