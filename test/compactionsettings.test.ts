import test from "node:test";
import assert from "node:assert/strict";
import { parseCompactionMinutes, parseCompactionPercent } from "../src/compactionsettings.ts";

test("compaction percentages accept off or an integer percentage", () => {
  assert.equal(parseCompactionPercent("0"), 0);
  assert.equal(parseCompactionPercent("45"), 45);
  assert.equal(parseCompactionPercent("100"), 100);
  assert.equal(parseCompactionPercent("101"), null);
  assert.equal(parseCompactionPercent("1.5"), null);
  assert.equal(parseCompactionPercent(""), null);
});

test("compaction lull accepts off or up to a day", () => {
  assert.equal(parseCompactionMinutes("0"), 0);
  assert.equal(parseCompactionMinutes("15"), 15);
  assert.equal(parseCompactionMinutes("1440"), 1440);
  assert.equal(parseCompactionMinutes("1441"), null);
  assert.equal(parseCompactionMinutes("2.5"), null);
  assert.equal(parseCompactionMinutes("no"), null);
});
