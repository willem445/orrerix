// Saved instruction presets for the quick task (#3679): the pure model, and
// the store's read-before-write rule.

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  QUICK_PRESETS_MAX,
  QUICK_PRESET_NAME_MAX,
  QuickPresetsStore,
  decodeQuickPresets,
  encodeQuickPresets,
  instructionsEmpty,
  normalizePresetName,
  presetMatching,
  presetNamed,
  removePreset,
  sameInstructions,
  upsertPreset,
  type QuickPreset,
} from "../src/quickpresets.ts";

const strict: QuickPreset = { name: "Strict review", plan: "", work: "small diffs", review: "tests first" };
const terse: QuickPreset = { name: "Terse", plan: "three steps at most", work: "", review: "" };

test("the one equality predicate reads every field a preset has", () => {
  assert.ok(sameInstructions(strict, { ...strict }));
  // Each field, changed alone, is a difference — the property that makes this
  // the single answer to "does the form still match the preset it was filled
  // from". A field it did not read would be a field whose edits nobody notices.
  for (const field of ["plan", "work", "review"] as const) {
    assert.equal(sameInstructions(strict, { ...strict, [field]: `${strict[field]} changed` }), false, field);
  }
  // Blank space at the ends is not a difference; blank space inside is.
  assert.ok(sameInstructions(strict, { plan: " ", work: " small diffs\n", review: "tests first " }));
  assert.equal(sameInstructions(strict, { ...strict, work: "small  diffs" }), false);
  assert.ok(instructionsEmpty({ plan: " ", work: "\n", review: "" }));
  assert.equal(instructionsEmpty(terse), false);
});

test("the picker shows the preset the boxes equal, and none once they are edited", () => {
  const presets = [strict, terse];
  assert.equal(presetMatching(presets, { plan: "", work: "small diffs", review: "tests first" })?.name, "Strict review");
  assert.equal(presetMatching(presets, { plan: "", work: "small diffs!", review: "tests first" }), null);
  assert.equal(presetMatching(presets, { plan: "", work: "", review: "" }), null, "empty boxes are no preset");
  assert.equal(presetNamed(presets, "  strict   REVIEW ")?.name, "Strict review", "names match case- and space-insensitively");
  assert.equal(presetNamed(presets, "nope"), null);
});

test("saving replaces the preset with the same name and appends a new one", () => {
  const added = upsertPreset([strict], terse);
  assert.ok(added.ok);
  assert.deepEqual(added.presets.map((p) => p.name), ["Strict review", "Terse"]);

  const replaced = upsertPreset([strict, terse], { name: "strict review", plan: "p", work: " w ", review: "" });
  assert.ok(replaced.ok);
  assert.deepEqual(replaced.presets, [{ name: "strict review", plan: "p", work: "w", review: "" }, terse]);
});

test("a save with no name, nothing in it, or past the cap is refused by name", () => {
  const noName = upsertPreset([], { ...strict, name: "   " });
  assert.match(!noName.ok ? noName.error : "", /needs a name/);
  const empty = upsertPreset([], { name: "Empty", plan: " ", work: "", review: "\n" });
  assert.match(!empty.ok ? empty.error : "", /nothing to save/);

  const full = Array.from({ length: QUICK_PRESETS_MAX }, (_, i) => ({ ...strict, name: `p${i}` }));
  const over = upsertPreset(full, { ...strict, name: "one more" });
  assert.match(!over.ok ? over.error : "", new RegExp(`already ${QUICK_PRESETS_MAX} presets`));
  // …while replacing one of the fifty is still allowed: the cap is on the
  // count, and a replace does not move it.
  const swap = upsertPreset(full, { ...terse, name: "p7" });
  assert.ok(swap.ok);
  assert.equal(swap.presets.length, QUICK_PRESETS_MAX);
});

test("a name is trimmed, collapsed and capped", () => {
  assert.equal(normalizePresetName("  my   strict\treview "), "my strict review");
  assert.equal(normalizePresetName("x".repeat(200)).length, QUICK_PRESET_NAME_MAX);
});

test("removing a preset that is not there changes nothing", () => {
  assert.deepEqual(removePreset([strict, terse], "TERSE"), [strict]);
  assert.deepEqual(removePreset([strict], "nope"), [strict]);
});

test("the file round-trips, and keeps a newer build's fields", () => {
  const raw = JSON.stringify({ version: 1, presets: [strict, terse], future: { x: 1 } });
  const file = decodeQuickPresets(raw);
  assert.ok(file);
  assert.deepEqual(file.presets, [strict, terse]);
  assert.deepEqual(JSON.parse(encodeQuickPresets(file)), { future: { x: 1 }, version: 1, presets: [strict, terse] });
});

test("an absent file is an empty store, and an unreadable one is not", () => {
  // First run, or a corrupt file the backend quarantined: there are no presets.
  assert.deepEqual(decodeQuickPresets(null), { presets: [], extra: {} });
  // A file this build cannot read is NOT an empty store — the difference is
  // whether a save may write over it.
  assert.equal(decodeQuickPresets("{not json"), null);
  assert.equal(decodeQuickPresets("[]"), null);
  assert.equal(decodeQuickPresets(JSON.stringify({ version: 2, presets: [] })), null, "a newer schema");
  assert.equal(decodeQuickPresets(JSON.stringify({ version: 1 })), null, "no preset list at all");
});

test("rows that are not presets are dropped, and a duplicate name keeps its first row", () => {
  const file = decodeQuickPresets(
    JSON.stringify({
      version: 1,
      presets: [strict, null, 7, { name: "" }, { name: "STRICT REVIEW", work: "second" }, { name: "Loose", review: 3 }],
    })
  );
  assert.deepEqual(file?.presets, [strict, { name: "Loose", plan: "", work: "", review: "" }]);
});

// ── the store ───────────────────────────────────────────────────────────────

/** A store over a fake file whose reads and writes a test can fail. */
function disk(initial: string | null) {
  const state = { contents: initial, loads: 0, saves: 0, failLoad: false, failSave: false };
  const store = new QuickPresetsStore({
    load: async () => {
      state.loads += 1;
      if (state.failLoad) throw new Error("read failed");
      return state.contents;
    },
    save: async (contents) => {
      if (state.failSave) throw new Error("disk full");
      state.saves += 1;
      state.contents = contents;
    },
  });
  return { state, store };
}

test("every save reads the file first, so another window's preset is not erased", async () => {
  const { state, store } = disk(JSON.stringify({ version: 1, presets: [strict] }));
  assert.deepEqual(await store.read(), [strict]);

  // Another window saves a preset after this one's read…
  state.contents = JSON.stringify({ version: 1, presets: [strict, terse] });
  // …and this window, which still believes there is one preset, saves a third.
  const mine: QuickPreset = { name: "Mine", plan: "", work: "w", review: "" };
  const out = await store.save(mine);
  assert.equal(out.outcome, "saved");
  assert.deepEqual(
    decodeQuickPresets(state.contents)?.presets.map((p) => p.name),
    ["Strict review", "Terse", "Mine"],
    "the other window's preset survived"
  );
  assert.equal(state.loads, 2, "the save read for itself rather than trusting the earlier read");
});

test("a failed read declines the write — could-not-look is not nothing-there", async () => {
  const { state, store } = disk(JSON.stringify({ version: 1, presets: [strict] }));
  state.failLoad = true;
  assert.equal(await store.read(), null);
  assert.deepEqual(await store.save(terse), { outcome: "unreadable" });
  assert.deepEqual(await store.remove("Strict review"), { outcome: "unreadable" });
  assert.equal(state.saves, 0, "nothing was written over a file that could not be read");

  // Not latched: the next gesture reads again and succeeds.
  state.failLoad = false;
  assert.equal((await store.save(terse)).outcome, "saved");
  assert.deepEqual(decodeQuickPresets(state.contents)?.presets.map((p) => p.name), ["Strict review", "Terse"]);
});

test("a file this build cannot read is never written over", async () => {
  const newer = JSON.stringify({ version: 2, presets: [{ name: "from the future" }] });
  const { state, store } = disk(newer);
  assert.deepEqual(await store.save(terse), { outcome: "unreadable" });
  assert.equal(state.contents, newer);
  assert.equal(state.saves, 0);
});

test("a refused edit and a failed write say which they were", async () => {
  const { state, store } = disk(null);
  const refused = await store.save({ name: "", plan: "p", work: "", review: "" });
  assert.equal(refused.outcome, "refused");
  assert.equal(state.saves, 0);

  state.failSave = true;
  const failed = await store.save(strict);
  assert.equal(failed.outcome, "save-failed");
  assert.match(failed.outcome === "save-failed" ? failed.error : "", /disk full/);

  state.failSave = false;
  assert.equal((await store.save(strict)).outcome, "saved", "the first save on an empty store creates the file");
  assert.equal((await store.remove("strict review")).outcome, "saved");
  assert.deepEqual(decodeQuickPresets(state.contents)?.presets, []);
});
