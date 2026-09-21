// Unit tests for the board row's LEVEL MARK (#3261) — src/taskkind.ts.
//
// What these test is intent: that the mark answers "where on the ladder is this
// row" for every value a row's `kind` can actually hold, that an unlabelled row
// is not quietly made into a fifth level, and that the table and the palette
// cannot drift apart in either direction.

import { test } from "node:test";
import assert from "node:assert/strict";

import { KIND_KEYS, UNKNOWN_KIND, boardShowsKindMarks, kindKey, kindTitle, kindToken } from "../src/taskkind.ts";
import { KINDS, UNLABELLED_KIND } from "../src/taskboard.ts";
import { CSS_TOKENS, KIND_HUES } from "../src/theme.ts";

test("every level the board knows has a mark, and nothing else does", () => {
  // Both directions, the way test/agenticons.test.ts pins the CLI table: a
  // level added to KINDS without a pigment, or a pigment for a level that does
  // not exist, both fail here rather than rendering as `var(--kind-undefined)`.
  assert.deepEqual(
    Object.keys(KIND_HUES).sort(),
    [...KINDS, UNLABELLED_KIND].sort(),
    "KIND_HUES and the board's own ladder must name the same set"
  );
  assert.deepEqual([...KIND_KEYS], [...KINDS, UNLABELLED_KIND], "KIND_KEYS is the ladder, outermost first");
});

test("every mark's token is declared in theme.ts's pinned CSS tokens", () => {
  for (const k of KIND_KEYS) {
    const token = kindToken(k);
    assert.ok(token in CSS_TOKENS, `${token} is painted by the board but not pinned in CSS_TOKENS`);
    assert.equal(
      (CSS_TOKENS as Record<string, string>)[token],
      (KIND_HUES as Record<string, string>)[k],
      `${token} must carry ${k}'s own hue`
    );
  }
});

test("no two levels share a pigment", () => {
  assert.equal(
    new Set(Object.values(KIND_HUES)).size,
    Object.keys(KIND_HUES).length,
    "two levels painted the same colour would make the ladder unreadable"
  );
});

test("a row with no level reads as unlabelled, whichever way the field is empty", () => {
  for (const empty of [null, undefined, "", "   "]) {
    assert.equal(kindKey(empty), UNLABELLED_KIND, `${JSON.stringify(empty)} is the flat board's row`);
  }
});

test("a kind the board cannot place stays BROKEN, and is never laundered into a level", () => {
  // Only a hand-edited tasks.json can hold one — the backend refuses it on
  // write. The board already paints it in the danger dye as "your file is
  // wrong"; mapping it to `unlabelled` here would replace that with a quiet
  // "this row has no level", which is the regression this pins.
  assert.equal(kindKey("initiative"), UNKNOWN_KIND);
  assert.notEqual(kindKey("initiative"), UNLABELLED_KIND);
  assert.equal(kindToken("initiative"), null, "a broken row gets no level mark at all");
  assert.match(kindTitle("initiative"), /not a level this board knows/);
  // And it does not turn the marks on for a board that is otherwise flat.
  assert.equal(boardShowsKindMarks([{ kind: "initiative" }]), false);
});

test("each real level maps to its own token, and unlabelled is not one of them", () => {
  for (const k of KINDS) {
    assert.equal(kindKey(k), k);
    assert.equal(kindToken(k), `--kind-${k}`);
    assert.notEqual(kindToken(k), kindToken(null), `${k} must not paint as the flat board's row`);
  }
});

test("the mark always carries a word, so the ladder is never colour-only", () => {
  for (const k of KIND_KEYS) {
    const title = kindTitle(k);
    assert.notEqual(kindToken(k), null, `${k} is a level the board can place`);
    assert.ok(title.length > 0, `${k} has no tooltip`);
    assert.ok(
      k === UNLABELLED_KIND ? /no level/i.test(title) : title.toLowerCase().startsWith(k),
      `${k}'s tooltip must name it: ${JSON.stringify(title)}`
    );
  }
  // The unlabelled row's tooltip says it is OFF the ladder, not that it is the
  // bottom of it — the whole point of the achromatic mark.
  assert.doesNotMatch(kindTitle(null), /\btask\b/i);
});

test("a board that uses no levels shows no marks at all", () => {
  assert.equal(boardShowsKindMarks([]), false, "an empty board has nothing to mark");
  assert.equal(
    boardShowsKindMarks([{ kind: null }, { kind: "" }, {}]),
    false,
    "a flat board must not gain a column of 'unlabelled' marks"
  );
  // One levelled row turns them on for the whole board, including the rows
  // that have none — there, "not in the tree" is information.
  assert.equal(boardShowsKindMarks([{ kind: null }, { kind: "story" }]), true);
});
