import test from "node:test";
import assert from "node:assert/strict";

import { TITLE_BUDGET, displayTitle, titleOverBudget, titleTooltip } from "../src/tasktitle.ts";

/** A title of exactly `n` code points, made of real words so the word-boundary
 *  hunt has something to find. */
function words(n: number): string {
  let s = "";
  while (s.length < n) s += "alpha beta gamma delta epsilon zeta ";
  return s.slice(0, n).trimEnd().padEnd(n, "x");
}

test("a title inside the budget is shown whole, and nothing is marked cut", () => {
  for (const n of [0, 1, 40, TITLE_BUDGET - 1, TITLE_BUDGET]) {
    const t = words(n);
    const d = displayTitle(t);
    assert.equal(d.shown, t, `a ${n}-character title must not be touched`);
    assert.equal(d.full, t);
    assert.equal(d.truncated, false, `a ${n}-character title is not over an ${TITLE_BUDGET} budget`);
  }
});

test("a title over the budget is cut, marked, and never longer than the budget", () => {
  // The real shape this was filed for: an agent-written title that runs on.
  const long =
    "Task board: human-readable rows — title length limit, optional short description, click-to-expand, and colour by kind so the hierarchy reads at a glance";
  const d = displayTitle(long);
  assert.equal(d.truncated, true, "a 150-character title is over an 80 budget");
  assert.equal(d.full, long, "the full title is carried through untouched");
  assert.ok(
    Array.from(d.shown).length <= TITLE_BUDGET,
    `shown is ${Array.from(d.shown).length} code points, over the ${TITLE_BUDGET} budget`
  );
  assert.ok(d.shown.endsWith("…"), `shown must be marked as cut: ${JSON.stringify(d.shown)}`);
  assert.ok(long.startsWith(d.shown.slice(0, -1)), "what is shown must be a prefix of the real title");
});

test("the cut lands on a word boundary when one is near, and mid-token when one is not", () => {
  const spaced = displayTitle(`${"a".repeat(60)} boundary ${"z".repeat(40)}`);
  assert.ok(
    !spaced.shown.slice(0, -1).endsWith(" "),
    "a boundary cut must not leave a trailing space before the ellipsis"
  );
  assert.ok(spaced.shown.endsWith("boundary…"), `expected a cut at the space, got ${JSON.stringify(spaced.shown)}`);

  // No space anywhere near the budget: one long identifier. It is cut mid-token
  // rather than losing the whole word — see WORD_HUNT.
  const solid = displayTitle("x".repeat(200));
  assert.equal(solid.shown, `${"x".repeat(TITLE_BUDGET - 1)}…`);
});

test("the budget counts code points, so an emoji title is not cut early or split", () => {
  // 100 astral code points: `.length` says 200, `Array.from().length` says 100.
  const emoji = "🙂".repeat(100);
  assert.equal(emoji.length, 200, "the fixture must actually be a surrogate-pair string");
  const d = displayTitle(emoji);
  assert.equal(d.truncated, true);
  const cp = Array.from(d.shown);
  assert.ok(cp.length <= TITLE_BUDGET, `${cp.length} code points, over budget`);
  assert.ok(
    cp.slice(0, -1).every((c) => c === "🙂"),
    `a cut must never land inside a surrogate pair: ${JSON.stringify(d.shown)}`
  );

  // And the same string measured in UTF-16 units would have been cut at 40
  // visible characters — the bug this pins.
  assert.ok(cp.length - 1 > 40, "a code-point budget must show more than a .length one would");
});

test("the editor's over-budget warning is SOFT and agrees with the display rule", () => {
  assert.equal(titleOverBudget(words(TITLE_BUDGET)), false);
  assert.equal(titleOverBudget(words(TITLE_BUDGET + 1)), true);
  // The two answers are one rule: a title warns exactly when the row cuts it.
  for (const n of [1, 79, 80, 81, 300]) {
    assert.equal(
      titleOverBudget(words(n)),
      displayTitle(words(n)).truncated,
      `the warning and the cut disagree at ${n} characters`
    );
  }
});

test("the tooltip carries the whole title only when something was cut", () => {
  const hint = "Click to open this row";
  const short = displayTitle("Short name");
  assert.equal(titleTooltip(short, hint), hint, "an uncut title must not repeat itself in its own tooltip");

  const long = displayTitle("q".repeat(300));
  const tip = titleTooltip(long, hint);
  assert.ok(tip.includes("q".repeat(300)), "a cut title's full text must be on the tooltip");
  assert.ok(tip.includes(hint), "the tooltip still says what the click does");
});
