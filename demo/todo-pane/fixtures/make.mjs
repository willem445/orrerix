// Generates fixtures/busy.json — 500 items, deterministic.
//
//   node demo/todo-pane/fixtures/make.mjs
//
// DETERMINISTIC ON PURPOSE. The generator carries its own tiny LCG rather than
// calling Math.random, so regenerating the fixture produces a byte-identical
// file and a diff on it means someone changed this script. A fixture that
// reshuffles itself on every run is a fixture no review can read.
//
// Why 500: the pane has to stay smooth at a list size a year of agent writes
// would produce, and the only honest way to show that is to put the number on
// the screen and scroll it. The count chip in the header is the receipt.

import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

let seed = 0x3263;
const rnd = () => ((seed = (seed * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff);
const pick = (a) => a[Math.floor(rnd() * a.length)];
const chance = (p) => rnd() < p;

const VERBS = ["Audit", "Rebase", "Sweep", "Re-derive", "Pin", "Bless", "Wire", "Fold", "Split", "Measure",
  "Document", "Refuse", "Bound", "Quarantine", "Coalesce", "Retarget", "Verify", "Purge"];
const NOUNS = ["the run citations", "the population control", "the quick-add parser", "the store envelope",
  "the workspace key", "the undo stack", "the attribution dot", "the Planned buckets", "the reminder toast",
  "the scope switch", "the keyboard map", "the token block", "the design note", "the tool table",
  "the capability manifest", "the audit row", "the mutation table", "the purge window"];
const TAGS = ["infra", "review", "docs", "ui", "admin", "release", "perf", "life"];
const AGENTS = [
  { id: "worker-1", cli: "claude", hue: "id-jade" },
  { id: "worker-2", cli: "copilot", hue: "id-cyan" },
  { id: "worker-3", cli: "claude", hue: "id-azure" },
  { id: "rev-lead", cli: "codex", hue: "id-violet" },
  { id: "orch", cli: "claude", hue: "id-orchid" },
];

const items = [];
for (let i = 0; i < 500; i++) {
  const done = chance(0.28);
  const hasDue = chance(0.55);
  const tags = [];
  if (chance(0.7)) tags.push(pick(TAGS));
  if (chance(0.2)) { const t = pick(TAGS); if (!tags.includes(t)) tags.push(t); }

  const steps = [];
  if (chance(0.25)) {
    const n = 2 + Math.floor(rnd() * 4);
    for (let s = 0; s < n; s++) steps.push({ title: `${pick(VERBS).toLowerCase()} ${pick(NOUNS)}`, done: chance(0.4) });
  }

  const agent = chance(0.45) ? pick(AGENTS) : null;

  const item = {
    id: `td-b-${String(i).padStart(3, "0")}`,
    scope: chance(0.75) ? "workspace" : "global",
    title: `${pick(VERBS)} ${pick(NOUNS)} (#${3000 + Math.floor(rnd() * 300)})`,
    order: (i + 1) * 100,
  };
  if (done) { item.done = true; item.completed_ago_min = Math.floor(rnd() * 43200); }
  if (hasDue && !done) {
    item.due = { day: Math.floor(rnd() * 40) - 8 };
    if (chance(0.35)) item.due.hm = `${String(8 + Math.floor(rnd() * 11)).padStart(2, "0")}:${chance(0.5) ? "00" : "30"}`;
  }
  if (!done && chance(0.18)) item.my_day = true;
  if (!done && chance(0.14)) item.important = true;
  if (!done && chance(0.2)) item.priority = 1 + Math.floor(rnd() * 3);
  if (tags.length) item.tags = tags;
  if (steps.length) item.steps = steps;
  if (chance(0.15)) item.notes = `${pick(VERBS)} ${pick(NOUNS)} before this one — the receipt goes in the agent layer.`;
  if (agent) { item.actor = agent; item.updated_ago_min = Math.floor(rnd() * 2880); }
  items.push(item);
}

const out = {
  _comment: [
    "GENERATED — run `node demo/todo-pane/fixtures/make.mjs` to rebuild.",
    "500 items. The count chip in the pane header is the on-screen receipt;",
    "scroll it and type in the search field to see whether it stays smooth."
  ],
  workspace: { label: "loomux", root: "C:\\Projects\\loomux" },
  items,
};

const here = dirname(fileURLToPath(import.meta.url));
writeFileSync(join(here, "busy.json"), JSON.stringify(out, null, 1) + "\n");
console.log(`busy.json — ${items.length} items, ${items.filter((i) => i.done).length} completed`);
