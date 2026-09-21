// demo/todo-pane — demo controls and the interaction layer (#3263 S0).
//
// SCAFFOLDING. S4 inherits `render.js` and `quickadd.js`; it does not inherit
// this file. What it DOES inherit is the shape below:
//
//   * ONE state object. Every view concern — which view, which scope, the
//     search string, the quick-add draft, which row is expanded — is a field
//     here, never a property of an element. The DOM is rebuilt from it on
//     every change (CLAUDE.md's in-list-editor rule), so a re-render caused
//     by an agent's write can never eat a half-typed row.
//   * UNDO IS AN INVERSE-OP STACK, not a snapshot pile. Every DISCRETE gesture
//     pushes the op that undoes it — complete, important, My Day, due, delete,
//     add, reorder, a sub-step toggle or addition, archive-all — and soft
//     delete is what makes deletion invertible. One mutation is deliberately
//     outside it: typing in a note, which is continuous rather than discrete,
//     and which the browser's own text undo already covers inside the field.
//     That exception is stated because an earlier version of this comment
//     claimed "every mutation" while five paths pushed nothing (#3271 review).
//   * THE CLOCK IS EXPLICIT. `state.nowMs` is set once from the demo's clock
//     control and threaded everywhere; nothing in `render.js` or
//     `quickadd.js` reads the wall clock, so the 09:00 / 14:00 / 23:00 buttons
//     really do change what "Today" and "overdue" mean.

import { renderInto, materialise, VIEWS } from "./render.js";
import { parseQuickAdd } from "./quickadd.js";

const PANES = ["pane-narrow", "pane-wide"];

/** The demo clock: today's local date at the hour the control selects. */
function clockAt(hour) {
  const d = new Date();
  d.setHours(hour, 0, 0, 0);
  return d.getTime();
}

const state = {
  fixture: "mixed",
  items: [],
  workspace: { label: "loomux", root: "C:\\Projects\\loomux" },
  view: "myday",
  scope: "workspace",
  query: "",
  draft: "",
  tagFilter: null,
  selected: null,
  expanded: null,
  showRoot: false,
  searchOpen: false,
  toast: null,
  nowMs: clockAt(14),
  undo: [],
  seq: 0,
};

let toastTimer = 0;

/* ============================== rendering ================================ */

/**
 * Render both cells from the one state, then restore focus and caret.
 *
 * The caret restore is here because the whole list is rebuilt on every
 * keystroke in the quick-add bar. That is the price of the no-state-in-the-DOM
 * rule, and it is paid ONCE, in one place, rather than by making the renderer
 * clever about what changed.
 */
function render() {
  const active = document.activeElement;
  const act = active && active.dataset ? active.dataset.act : null;
  const paneId = active ? active.closest(".pane")?.id : null;
  const start = active && "selectionStart" in active ? active.selectionStart : null;

  for (const id of PANES) {
    const root = document.getElementById(id);
    if (root) renderInto(root, state);
  }

  if (act && paneId) {
    const back = document.querySelector(`#${paneId} [data-act="${act}"]`);
    if (back) {
      back.focus();
      if (start != null && "setSelectionRange" in back) {
        try { back.setSelectionRange(start, start); } catch { /* non-text input */ }
      }
    }
  }
}

function toast(text) {
  state.toast = { text };
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { state.toast = null; render(); }, 5000);
}

/* ================================ ops ==================================== */

function byId(id) {
  return state.items.find((it) => it.id === id) || null;
}

/** DESIGN.md §7's "50 deep", named once so the claim and the code cannot drift. */
const UNDO_DEPTH = 50;

/** Deep enough for the one array a patch can carry: `steps`. */
function cloneValue(v) {
  return Array.isArray(v) ? v.map((x) => (x && typeof x === "object" ? { ...x } : x)) : v;
}

/**
 * Apply ONE item's field patch and RETURN its inverse. Touches no stack.
 *
 * Split out from `patch` because an undo entry has to be able to span more than
 * one item — a reorder swaps two rows, an archive moves several — and a stack
 * that can only hold a single-item patch silently does not cover those. That
 * was a review finding on #3271: the header claimed every mutation pushed its
 * inverse while five paths pushed nothing.
 */
function applyPatch(id, fields) {
  const item = byId(id);
  if (!item) return null;
  // The inverse carries the attribution too: undoing an agent's completed row
  // must put the agent's dot back, not leave the row looking human-authored.
  const inverse = { actor: item.actor, updatedAgoMin: item.updatedAgoMin };
  for (const k of Object.keys(fields)) inverse[k] = cloneValue(item[k]);
  Object.assign(item, fields);
  if (!("actor" in fields)) {
    item.updatedAgoMin = 0;
    item.actor = null; // the human just touched it; attribution follows the last writer
  }
  return { id, fields: inverse };
}

/** Push one undo entry — a LIST of per-item inverses, applied together. */
function pushUndo(inverses, label) {
  const real = inverses.filter(Boolean);
  if (real.length) {
    state.undo.push(real);
    if (state.undo.length > UNDO_DEPTH) state.undo.shift();
  }
  if (label) toast(label);
}

/** The common case: one item, one patch, one entry. */
function patch(id, fields, label) {
  pushUndo([applyPatch(id, fields)], label);
}

function undo() {
  const op = state.undo.pop();
  if (!op) { toast("Nothing to undo."); return; }
  // Applied through applyPatch, not patch: an undo pushes nothing, so undo is
  // not itself undoable. A redo stack is S5's if the human wants one.
  for (const inv of op) applyPatch(inv.id, inv.fields);
  state.toast = null;
  render();
}

function addFromDraft() {
  const parsed = parseQuickAdd(state.draft, state.nowMs);
  if (!parsed.valid) return;
  const id = `td-demo-${++state.seq}`;
  state.items.unshift({
    id,
    scope: state.scope,
    title: parsed.title,
    done: false,
    deleted: false,
    completedMs: null,
    dueMs: parsed.dueMs,
    hasTime: parsed.hasTime,
    // Adding while My Day is the open view puts it in My Day: the view you
    // are looking at is the list you meant. `@myday` says so explicitly.
    myDay: parsed.myDay || state.view === "myday",
    important: parsed.important || state.view === "important",
    priority: parsed.priority,
    tags: parsed.tags,
    notes: "",
    steps: [],
    order: 0,
    actor: null,
    updatedAgoMin: 0,
  });
  // Undoing an add is a soft delete of it — the same op every other undo uses,
  // rather than a second "remove" path that only the add route can produce.
  // Pushed through pushUndo so it is an entry of the same SHAPE as every other
  // (a list of inverses); a hand-built entry here was the one site that still
  // spoke the old single-patch shape after the stack was generalised.
  pushUndo([{ id, fields: { deleted: true } }], null);
  state.draft = "";
  render();
}

/** Rows currently on screen, in display order — the j/k and Alt+↑/↓ domain. */
function visibleIds(root) {
  return [...root.querySelectorAll(".row")].map((r) => r.dataset.id);
}

function move(delta) {
  const root = document.getElementById("pane-wide");
  const ids = visibleIds(root);
  if (!ids.length) return;
  const at = ids.indexOf(state.selected);
  const next = at < 0 ? (delta > 0 ? 0 : ids.length - 1) : Math.min(ids.length - 1, Math.max(0, at + delta));
  state.selected = ids[next];
  render();
  const el = root.querySelector(`.row[data-id="${state.selected}"]`);
  if (el) el.scrollIntoView({ block: "nearest" });
}

function reorder(delta) {
  const root = document.getElementById("pane-wide");
  const ids = visibleIds(root);
  const at = ids.indexOf(state.selected);
  if (at < 0) return;
  const swapWith = byId(ids[at + delta]);
  const me = byId(state.selected);
  if (!swapWith || !me) return;
  // Both values read BEFORE either is written. `applyPatch` mutates in place,
  // so reading `me.order` on the second line would read the value the first
  // line just assigned — a swap with no temp, leaving both rows on the same
  // order and the list visibly unmoved.
  const mine = me.order;
  const theirs = swapWith.order;
  pushUndo([
    applyPatch(me.id, { order: theirs }),
    applyPatch(swapWith.id, { order: mine }),
  ], null);
  render();
}

function complete(id) {
  const item = byId(id);
  if (!item) return;
  const next = !item.done;
  // The strike-and-slide runs on the element before the state flips, so the
  // row the human is watching is the row that animates. `prefers-reduced-
  // motion` collapses the duration to 1ms in CSS, so this path is identical
  // either way and there is no second code path to keep honest.
  const els = document.querySelectorAll(`.row[data-id="${id}"]`);
  const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches ||
    document.querySelector(".stage").dataset.motion === "off";
  const commit = () => {
    patch(id, { done: next, completedMs: next ? state.nowMs : null },
      next ? `Completed “${item.title}”` : `Restored “${item.title}”`);
    render();
  };
  if (next && !reduced && state.view !== "completed") {
    els.forEach((el) => { el.dataset.leaving = "true"; });
    setTimeout(commit, 600);
  } else {
    commit();
  }
}

/* ============================== delegation =============================== */

function onClick(ev) {
  const btn = ev.target.closest("[data-act]");
  if (!btn) return;
  const act = btn.dataset.act;
  const row = btn.closest(".row");
  const id = row ? row.dataset.id : null;
  if (id) state.selected = id;

  switch (act) {
    case "scope": state.scope = btn.dataset.scope; state.expanded = null; break;
    case "root": state.showRoot = !state.showRoot; break;
    case "view": state.view = btn.dataset.view; state.expanded = null; break;
    case "toggle": complete(id); return;
    case "important": { const it = byId(id); patch(id, { important: !it.important }); break; }
    case "expand": state.expanded = state.expanded === id ? null : id; break;
    case "myday": { const it = byId(id); patch(id, { myDay: !it.myDay }); break; }
    case "due": {
      // The mock's date picker is the quick-add grammar, not a calendar grid:
      // one field, the same parser, so there is one thing to learn. A real
      // picker is S5's, and it sits beside this rather than replacing it.
      const it = byId(id);
      const text = prompt("Due — same grammar as quick-add (tomorrow, fri, in 3 days, at 4pm):",
        it.dueMs ? "" : "tomorrow");
      if (text == null) return;
      const parsed = parseQuickAdd("x " + text, state.nowMs);
      patch(id, { dueMs: parsed.dueMs, hasTime: parsed.hasTime }, parsed.dueMs ? null : "Due date cleared.");
      break;
    }
    case "delete": patch(id, { deleted: true }, "Deleted."); state.expanded = null; break;
    case "step": {
      // Patched as a whole cloned array rather than mutated in place, so the
      // inverse the stack keeps is the array as it was.
      const it = byId(id);
      const i = Number(btn.dataset.step);
      const steps = it.steps.map((s) => ({ ...s }));
      steps[i].done = !steps[i].done;
      patch(id, { steps }, null);
      break;
    }
    case "tag": state.tagFilter = state.tagFilter === btn.dataset.tag ? null : btn.dataset.tag; break;
    case "cleartag": state.tagFilter = null; break;
    case "archiveall": {
      // Soft-deletes each archived row rather than splicing them out of the
      // list: a removed row cannot be put back by a field patch, and "archive"
      // is exactly the gesture a human reaches for undo after.
      const rows = state.items.filter((it) => it.done && !it.deleted && it.scope === state.scope);
      pushUndo(rows.map((it) => applyPatch(it.id, { deleted: true })),
        rows.length ? `Archived ${rows.length}.` : "Nothing to archive.");
      break;
    }
    case "undo": undo(); return;
    default: return;
  }
  render();
}

function onInput(ev) {
  const t = ev.target;
  const act = t.dataset ? t.dataset.act : null;
  if (act === "draft") { state.draft = t.value; render(); }
  else if (act === "search") { state.query = t.value; render(); }
  else if (act === "notes") {
    const id = t.closest(".row").dataset.id;
    // Notes are written on input, not read at submit: a re-render caused by an
    // agent's write must not be able to lose a half-typed note.
    byId(id).notes = t.value;
  }
}

function onKeydown(ev) {
  const t = ev.target;
  const typing = t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement;

  if (typing && t.dataset.act === "draft") {
    if (ev.key === "Enter") { ev.preventDefault(); addFromDraft(); }
    if (ev.key === "Escape") { ev.preventDefault(); state.draft = ""; t.blur(); render(); }
    return;
  }
  if (typing && t.dataset.act === "search") {
    if (ev.key === "Escape") { ev.preventDefault(); state.query = ""; state.searchOpen = false; t.blur(); render(); }
    return;
  }
  if (typing && t.dataset.act === "step-add") {
    if (ev.key === "Enter" && t.value.trim()) {
      const id = t.closest(".row").dataset.id;
      const it = byId(id);
      patch(id, { steps: [...it.steps, { title: t.value.trim(), done: false }] }, null);
      t.value = "";
      render();
    }
    return;
  }
  if (typing) return;

  const vi = VIEWS.findIndex((v) => v.key === ev.key);
  if (vi >= 0) { state.view = VIEWS[vi].id; state.expanded = null; render(); return; }

  if (ev.altKey && (ev.key === "ArrowUp" || ev.key === "ArrowDown")) {
    ev.preventDefault(); reorder(ev.key === "ArrowUp" ? -1 : 1); return;
  }

  switch (ev.key) {
    case "n": {
      ev.preventDefault();
      const input = document.querySelector('#pane-wide [data-act="draft"]');
      if (input) input.focus();
      return;
    }
    case "/": {
      ev.preventDefault();
      state.searchOpen = true;
      render();
      document.querySelector('#pane-wide [data-act="search"]')?.focus();
      return;
    }
    case "j": case "ArrowDown": ev.preventDefault(); move(1); return;
    case "k": case "ArrowUp": ev.preventDefault(); move(-1); return;
    case " ": if (state.selected) { ev.preventDefault(); complete(state.selected); } return;
    case "e": if (state.selected) { ev.preventDefault(); state.expanded = state.expanded === state.selected ? null : state.selected; render(); } return;
    case "i": if (state.selected) { const it = byId(state.selected); patch(state.selected, { important: !it.important }); render(); } return;
    case "t": if (state.selected) { const it = byId(state.selected); patch(state.selected, { myDay: !it.myDay }); render(); } return;
    case "d": if (state.selected) { state.expanded = state.selected; render(); } return;
    case "Delete": if (state.selected) { patch(state.selected, { deleted: true }, "Deleted."); render(); } return;
    case "u": ev.preventDefault(); undo(); return;
    case "g": state.scope = state.scope === "global" ? "workspace" : "global"; state.expanded = null; render(); return;
    case "Escape": state.expanded = null; state.tagFilter = null; state.selected = null; render(); return;
  }
}

/* ============================ fixtures + boot ============================ */

async function load(name) {
  const res = await fetch(`fixtures/${name}.json`);
  if (!res.ok) throw new Error(`fixtures/${name}.json → HTTP ${res.status}`);
  const fixture = await res.json();
  state.fixture = name;
  // Keep the control in step with the state, so a ?fixture= URL does not leave
  // the dropdown naming a fixture that is not on screen.
  const sel = document.querySelector(`[data-a="fixture"]`);
  if (sel && sel.value !== name) sel.value = name;
  state.items = materialise(fixture, state.nowMs);
  state.workspace = fixture.workspace || state.workspace;
  state.undo = [];
  state.selected = null;
  state.expanded = null;
  state.draft = "";
  state.toast = null;
  render();
}

/** A tag with a class and text — text set as TEXT, never parsed as markup. */
function el2(tag, cls, text) {
  const n = document.createElement(tag);
  n.className = cls;
  n.textContent = text;
  return n;
}

function fail(err) {
  for (const id of PANES) {
    const root = document.getElementById(id);
    if (!root) continue;
    const box = document.createElement("div");
    box.className = "empty";
    // textContent, not innerHTML: `err.message` carries the fixture name, which
    // comes from the ?fixture= URL parameter, so a crafted URL was injecting
    // markup into this box (review finding on #3271). Demo-only and
    // self-inflicted, but the habit is the point — S4 inherits these shapes.
    const line = el2("p", "empty-text", `[demo] ${String(err.message || err)}`);
    const hint = el2("p", "empty-hint", "A plain file:// open cannot fetch the fixtures. Serve the directory: ");
    const cmd = el2("span", "mono", "npx vite demo/todo-pane --host 127.0.0.1");
    hint.append(cmd, document.createTextNode(". README.md has the detail."));
    box.append(line, hint);
    root.replaceChildren(box);
  }
}

function boot() {
  const stage = document.querySelector(".stage");

  stage.addEventListener("click", (ev) => {
    const ctl = ev.target.closest(".stagebar [data-a]");
    if (!ctl) return;
    const a = ctl.dataset.a;
    if (a === "motion") {
      const off = stage.dataset.motion === "off";
      stage.dataset.motion = off ? "on" : "off";
      ctl.setAttribute("aria-pressed", off ? "false" : "true");
      ctl.textContent = off ? "Motion on" : "Motion off";
    } else if (a === "clock") {
      for (const b of stage.querySelectorAll('[data-a="clock"]')) b.setAttribute("aria-pressed", "false");
      ctl.setAttribute("aria-pressed", "true");
      state.nowMs = clockAt(Number(ctl.dataset.h));
      load(state.fixture).catch(fail);
    } else if (a === "reset") {
      load(state.fixture).catch(fail);
    }
  });

  stage.addEventListener("change", (ev) => {
    const sel = ev.target.closest('[data-a="fixture"]');
    if (sel) load(sel.value).catch(fail);
  });

  for (const id of PANES) {
    const root = document.getElementById(id);
    root.addEventListener("click", onClick);
    root.addEventListener("input", onInput);
  }
  document.addEventListener("keydown", onKeydown);

  const at = new URLSearchParams(location.search);
  if (at.get("view")) state.view = at.get("view");
  if (at.get("scope")) state.scope = at.get("scope");
  load(at.get("fixture") || "mixed").catch(fail);
}

boot();
