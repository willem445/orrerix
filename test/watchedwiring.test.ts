// #3319 AC5: "clearing is the human's". Nothing clears a watch but a human
// gesture — not focusing the pane, not an agent reporting, not the pane going
// quiet, not a respawn.
//
// THAT IS A PROMISE ABOUT CODE THAT DOES NOT EXIST, which is the hardest kind
// to keep. Every unit test here passes just as well on the day someone adds
// `pane.setWatched(false)` to the focus handler, because the model is correct
// either way and the DOM wiring has no test at all (this repo validates DOM
// wiring by hand). A promise like that goes false silently, one slice later,
// with a green suite over it — exactly the shape CLAUDE.md's "a documented
// escape hatch is a counterfactual" bullet is about.
//
// So it is pinned where it can actually be broken: the SOURCE. `setWatched` is
// the one writer of the flag, and this scan default-denies its callers against
// an allowlist that carries a reason per entry.
//
// WHAT IT DECIDES ON, and what it does not: the decision is the CALL SITE's
// enclosing function name and the argument shape, never the name of a variable
// — a rename of the pane binding steps over nothing here. The residual is
// stated rather than papered over, at the bottom of this file.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const read = (rel: string) => readFileSync(new URL(rel, import.meta.url), "utf8");

/** Every `src/` file that may call `setWatched` at all, and why.
 *
 *  Default-deny: a call from any other file fails, which is the point — the
 *  focus path, the attention pass and the activity tick all live elsewhere.
 *  One row per FILE, each required exactly once, so a file that stops calling
 *  it (a refactor that moved the gesture) fails loudly rather than leaving this
 *  scan watching nothing. */
const ALLOWED: { file: string; why: string; calls: number }[] = [
  {
    file: "src/panebadges.ts",
    why:
      "the declaration, the `toggleWatched` wrapper, and the header chip's own " +
      "click — a human clicking their own mark to clear it (moved here from " +
      "pane.ts with the other header chips, #3498 F1)",
    calls: 3,
  },
  {
    file: "src/pane.ts",
    why:
      "`Pane.setWatched`, the public delegator into PaneBadges (#3498 F1): its " +
      "declaration and its one forwarding call, which passes the caller's " +
      "argument through unchanged",
    calls: 2,
  },
  {
    file: "src/main.ts",
    why:
      "the two RESTORE paths (#3319 AC3) — the layout replay and the docked " +
      "replay — which re-apply a watch the human already set. Neither clears " +
      "one: both are `setWatched(true)` guarded on the persisted flag",
    calls: 2,
  },
];

/** Files the gesture could plausibly have leaked into, scanned so that "nobody
 *  else calls it" is measured rather than assumed. These are the paths that run
 *  WITHOUT a human gesture — the ones an auto-clear would be written into. */
const MUST_BE_CLEAN = [
  "src/attention.ts",
  "src/attentiongate.ts",
  "src/panefocus.ts",
  "src/paneactivity.ts",
  "src/grid.ts",
  "src/tabbar.ts",
  "src/agentsview.ts",
  "src/panerestore.ts",
  "src/workspace.ts",
  // pane.ts's own satellites (#3498 F1). Before the split their code sat inside
  // pane.ts, whose exact count above would have reddened on a new call; now each
  // is its own file, and respawn — which AC5 names — lives in panelifecycle.ts.
  "src/panelifecycle.ts",
  "src/panecompose.ts",
  "src/paneembeds.ts",
  "src/paneviews.ts",
  "src/panecapture.ts",
];

const callSites = (src: string): string[] =>
  [...src.matchAll(/\.?setWatched\s*\(([^)]*)\)/g)].map((m) => m[1].trim());

test("#3319 AC5: nothing but a human gesture writes the watch flag", () => {
  let scanned = 0;
  for (const { file, why, calls } of ALLOWED) {
    const src = read(`../${file}`);
    const sites = callSites(src);
    scanned += sites.length;
    assert.equal(
      sites.length,
      calls,
      `${file} has ${sites.length} \`setWatched\` sites, not ${calls}. The allowlist says: ` +
        `${why}. If a new one is legitimate, add it HERE with its reason — that edit is the ` +
        "review this feature's AC5 asks for.",
    );
  }
  // The population control (#1209): a regex that had stopped matching would
  // leave every assertion above comparing 0 to 0 only if the counts were 0,
  // which they are not — but a renamed method would make them 0, so the floor
  // is asserted as well as the per-file counts.
  assert.ok(scanned >= 5, `only ${scanned} \`setWatched\` sites found — the scan is blind, not the code clean`);
});

test("#3319 AC5: no passive path touches the watch", () => {
  // The other direction, and the one that is really about auto-clearing: these
  // modules run on a timer, on a backend event, or on a focus change, and none
  // of them may write the flag. `grid.ts` and `tabbar.ts` READ it (they draw
  // the mark) and that is fine — only a WRITE is denied.
  const offenders: string[] = [];
  let scanned = 0;
  for (const file of MUST_BE_CLEAN) {
    const src = read(`../${file}`);
    scanned += 1;
    const sites = callSites(src);
    if (sites.length) offenders.push(`${file} calls setWatched(${sites.join("), setWatched(")})`);
  }
  assert.deepEqual(
    offenders,
    [],
    "a path that runs without a human gesture writes the watch flag — #3319 AC5 says a watch " +
      `is cleared by the human and by nobody else:\n${offenders.join("\n")}`,
  );
  assert.equal(scanned, MUST_BE_CLEAN.length, "a file on the clean list could not be read");
});

test("#3319 AC5: the restore calls only ever turn a watch ON", () => {
  // The sharpest form of the promise, and not covered by counting call sites:
  // a restore path that called `setWatched(record.watched)` unconditionally
  // would CLEAR a watch on any pane whose record said false — which is
  // harmless today and becomes a silent auto-clear the moment anything can set
  // a watch before restore finishes. Both sites are guarded and pass a literal
  // `true`, and that is what is pinned.
  const main = read("../src/main.ts");
  const sites = callSites(main);
  assert.equal(sites.length, 2, `main.ts has ${sites.length} setWatched sites, not 2`);
  for (const arg of sites) {
    assert.equal(arg, "true", `main.ts calls setWatched(${arg}) — restore may only turn a watch ON`);
  }
});

// THE RESIDUAL, stated because a guard's blind spots are part of its claim:
//
//  - It is TEXTUAL. A call reached through an alias (`const f = p.setWatched`),
//    through a computed member (`p["setWatched"]`), or generated by a macro-ish
//    helper is invisible here. None exists today; the first one would have to
//    be written deliberately.
//  - It bounds where the flag is WRITTEN, never how long a write lasts or what
//    a caller does after. `PaneBadges.setWatched` being the only writer is what
//    makes that enough, and THAT is enforced by `private isWatched` — the
//    compiler, not this scan. A second writer inside `panebadges.ts` would pass
//    here and fail review; a second writer outside it cannot compile.
//  - An argument spanning a newline would not match `[^)]*`. Every site today
//    is one line, and a multi-line one would drop the count and redden the
//    per-file assertion rather than passing silently.
test("#3319: the flag has exactly one writer, and it is private to the pane", () => {
  // The half the scan above cannot see, asserted against the source because
  // there is no runtime handle on it: `isWatched` is `private`, so nothing
  // outside `panebadges.ts` can assign it, whatever this file's regex can read.
  // It moved there from pane.ts with the header chips (#3498 F1); pane.ts and
  // panecapture.ts read it only through the public `watched` getter.
  const src = read("../src/panebadges.ts");
  assert.match(src, /private isWatched = false;/, "the watch flag is no longer a private field");
  const writes = [...src.matchAll(/this\.isWatched\s*=/g)];
  assert.equal(
    writes.length,
    1,
    `\`isWatched\` is assigned ${writes.length} times in panebadges.ts — #3319 AC5 rests on setWatched ` +
      "being its one writer",
  );
  // The flag must not have been left behind as well as moved: a second declaration
  // in pane.ts would be a second, unpinned writer the compiler happily accepts.
  assert.doesNotMatch(read("../src/pane.ts"), /\bisWatched\b/, "pane.ts declares or touches `isWatched` again");
});
