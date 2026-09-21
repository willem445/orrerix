import { test } from "node:test";
import assert from "node:assert/strict";
import {
  WATCHED_MARK,
  WATCHED_TITLE,
  dockChipWatched,
  nextWatchedKey,
  watchMenuLabel,
  watchedCount,
} from "../src/watchedpanes.ts";

const pane = (key: string, watched = false) => ({ key, watched });

test("the menu item names the action, not the state", () => {
  // The failure this pins is a menu that reads "Watch this pane" on a pane you
  // are already watching, which is what a label derived from the state (rather
  // than from what clicking it will do) produces.
  assert.equal(watchMenuLabel(false), "Watch this pane");
  assert.equal(watchMenuLabel(true), "Stop watching");
  assert.notEqual(watchMenuLabel(false), watchMenuLabel(true));
});

test("the mark and its tooltip say how to clear it", () => {
  // #3319 AC5: nothing clears a watch but the human, so the mark has to carry
  // the way out. A tooltip that only restated the mark would leave a human who
  // set one by accident with no visible undo.
  assert.equal(WATCHED_MARK, "◉");
  assert.match(WATCHED_TITLE, /Alt\+H/, "the tooltip must name the chord that clears it");
  assert.match(WATCHED_TITLE, /stop/i);
});

test("watchedCount counts only the watched", () => {
  assert.equal(watchedCount([]), 0);
  assert.equal(watchedCount([pane("a"), pane("b")]), 0);
  assert.equal(watchedCount([pane("a", true), pane("b"), pane("c", true)]), 2);
});

test("the cycle visits every watched pane and closes", () => {
  // The gesture's whole promise (#3319 AC4): press it enough times and you have
  // seen all of them, then you are back where you started. A cycle that skipped
  // one, or that stopped at the end of the list, would leave a pane the human
  // asked to be shown unshown — and neither shows up in a single-step test.
  const panes = [pane("a", true), pane("b"), pane("c", true), pane("d"), pane("e", true)];
  const visited: string[] = [];
  let at: string | null = "a";
  for (let i = 0; i < 3; i++) {
    at = nextWatchedKey(panes, at);
    visited.push(at!);
  }
  assert.deepEqual(visited, ["c", "e", "a"], "the cycle must wrap back to the first");
});

test("the cycle skips unwatched panes rather than stepping one row", () => {
  // The discriminator against "next pane in the list": from `a`, a stepping
  // implementation lands on `b`, which is not watched. The fixture puts an
  // unwatched pane immediately after the start on purpose — with `b` watched
  // too, both implementations answer "b" and the test proves nothing.
  const panes = [pane("a", true), pane("b"), pane("c", true)];
  assert.equal(nextWatchedKey(panes, "a"), "c");
  assert.notEqual(nextWatchedKey(panes, "a"), "b");
});

test("with nothing watched the cycle has no answer", () => {
  // The caller must be able to tell "nothing is watched" from "here is one",
  // because it says so to the human instead of moving focus somewhere arbitrary.
  assert.equal(nextWatchedKey([pane("a"), pane("b")], "a"), null);
  assert.equal(nextWatchedKey([pane("a"), pane("b")], null), null);
  assert.equal(nextWatchedKey([], null), null);
});

test("coming back from nowhere lands on the first watched pane", () => {
  // `fromKey` is null (no focused pane) or names a pane this list does not hold
  // (it was closed under us). Both are the "I just came back" case, and the
  // answer is the first watched pane rather than nothing.
  const panes = [pane("a"), pane("b", true), pane("c", true)];
  assert.equal(nextWatchedKey(panes, null), "b");
  assert.equal(nextWatchedKey(panes, "gone"), "b");
});

test("the only watched pane cycles to itself", () => {
  // Deliberately NOT null: the caller re-focuses it, a no-op, and never has to
  // tell this case apart from "nothing is watched" — which it would report to
  // the human as a different sentence.
  const panes = [pane("a"), pane("b", true), pane("c")];
  assert.equal(nextWatchedKey(panes, "b"), "b");
});

test("the cycle crosses the whole list it is given, in the caller's order", () => {
  // main.ts passes panes from EVERY tab, so a watch set before stepping away is
  // reachable from whichever tab the human comes back to. This pins that the
  // model imposes no order and no grouping of its own — it walks what it is
  // handed, which is what lets the caller decide the walk is fleet-wide.
  const panes = [pane("t2-x", true), pane("t1-y", true)];
  assert.equal(nextWatchedKey(panes, "t2-x"), "t1-y");
  assert.equal(nextWatchedKey(panes, "t1-y"), "t2-x");
});

test("the dock chip's title says watched only when it is", () => {
  assert.deepEqual(dockChipWatched("w: #3319", true), {
    watched: true,
    title: "w: #3319 — watched",
  });
  assert.deepEqual(dockChipWatched("w: #3319", false), {
    watched: false,
    title: "w: #3319",
  });
});
