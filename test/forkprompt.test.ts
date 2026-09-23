// The fork-name prompt's stylesheet contract (#3368 review B1). The prompt is
// DOM wiring validated by hand, but two facts about it are checkable from the
// source, and the first review round found both missing: every class the
// module assigns has a rule in `src/styles.css`, and the popover itself is
// `position: fixed`. The second is load-bearing, not cosmetic — it is what keeps
// the popover out of every layout box, which is the constraint-1 argument the
// design note makes for it (a static div appended to `<body>` would sit below
// the app rather than over the pane, and its inline left/top would be inert).
import { readFileSync } from "node:fs";
import { test } from "node:test";
import assert from "node:assert/strict";

const src = readFileSync(new URL("../src/forkprompt.ts", import.meta.url), "utf8");
const css = readFileSync(new URL("../src/styles.css", import.meta.url), "utf8");

/** Every class the module assigns, off `className = "…"` (a space-separated list
 *  is split, so a compound value is checked class by class). */
const assigned = [...src.matchAll(/className\s*=\s*"([^"]+)"/g)].flatMap((m) => m[1].split(/\s+/));

/** The declarations of the first rule whose selector list names `.cls` alone
 *  (not as a prefix of a longer class). */
function ruleFor(cls: string): string | null {
  const re = new RegExp(`(^|[\\s,}])\\.${cls}(?![\\w-])[^{]*\\{([^}]*)\\}`, "m");
  return re.exec(css)?.[2] ?? null;
}

test("the scan sees the prompt's classes (population control)", () => {
  // Raw count cross-check: as many `className =` sites as classes found, so a
  // pattern that stopped matching one of them cannot pass as coverage.
  const sites = src.split("className").length - 1;
  assert.ok(assigned.length >= 5, `found ${assigned.length}: ${assigned.join(", ")}`);
  assert.equal(assigned.length, sites, "every className site was parsed");
  assert.ok(assigned.includes("fork-name-prompt"));
});

test("every class the prompt assigns has a rule in styles.css", () => {
  const missing = assigned.filter((c) => ruleFor(c) === null);
  assert.deepEqual(missing, []);
});

test("the popover is position: fixed, so it joins no layout box", () => {
  const rule = ruleFor("fork-name-prompt");
  assert.ok(rule, "a .fork-name-prompt rule exists");
  assert.match(rule!, /position:\s*fixed/);
  assert.match(rule!, /z-index:\s*\d+/, "and stacks above the panes it floats over");
});
