// The quick-add parser (#3263 S3), against the plan §3 token list.
//
// The clock is INJECTED in every case below. That is not a style preference:
// a parser that read `Date.now()` could not be tested for "tomorrow" without
// the test being a different test every day, and every assertion here is an
// absolute instant derived from one fixed `NOW`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { parseQuickAdd, formatDue, DEFAULT_DUE_HOUR } from "../src/todoquickadd.ts";

/** Wednesday 2024-05-15, 10:00 local. Every expectation below is relative to it. */
const NOW = new Date(2024, 4, 15, 10, 0, 0, 0).getTime();

/** Local midnight, `days` from NOW's day, at `hour`:`minute`. */
function at(days: number, hour = DEFAULT_DUE_HOUR, minute = 0): number {
  const d = new Date(NOW);
  d.setDate(d.getDate() + days);
  d.setHours(hour, minute, 0, 0);
  return d.getTime();
}

test("the clock is injected, so `tomorrow` is a fixed instant", () => {
  const a = parseQuickAdd("water the plants tomorrow", NOW);
  assert.equal(a.title, "water the plants");
  assert.equal(a.dueMs, at(1));
  assert.equal(a.hasTime, false, "a bare date gets the default hour, not a time the human chose");

  // The same line parsed against a clock a week later moves by exactly a week —
  // which is only observable because the clock is a parameter.
  const b = parseQuickAdd("water the plants tomorrow", NOW + 7 * 86400000);
  assert.equal(b.dueMs, at(8));
});

test("`today` and `tonight` differ only in the hour", () => {
  assert.equal(parseQuickAdd("call mum today", NOW).dueMs, at(0));
  const tonight = parseQuickAdd("call mum tonight", NOW);
  assert.equal(tonight.dueMs, at(0, 19, 0));
  assert.equal(tonight.hasTime, true);
});

test("a bare weekday means the NEXT one, never today", () => {
  // NOW is a Wednesday. `fri` is two days out; `wed` is a full week out,
  // because a to-do that silently lands in the past hour is worse than one a
  // week away — if you meant today you would have typed `today`.
  assert.equal(parseQuickAdd("pay rent fri", NOW).dueMs, at(2));
  assert.equal(parseQuickAdd("pay rent wed", NOW).dueMs, at(7));
  assert.equal(parseQuickAdd("pay rent this wed", NOW).dueMs, at(0), "`this wed` DOES mean today");
});

test("`next week` is the coming Monday and `next <weekday>` its own day", () => {
  assert.equal(parseQuickAdd("retro next week", NOW).dueMs, at(5), "Mon 20 May from Wed 15 May");
  assert.equal(parseQuickAdd("retro next fri", NOW).dueMs, at(2));
});

test("`in N days` and `in N weeks`", () => {
  assert.equal(parseQuickAdd("chase the invoice in 3 days", NOW).dueMs, at(3));
  assert.equal(parseQuickAdd("chase the invoice in 1 day", NOW).dueMs, at(1));
  assert.equal(parseQuickAdd("chase the invoice in 2 weeks", NOW).dueMs, at(14));
  assert.equal(parseQuickAdd("chase the invoice in 3 days", NOW).title, "chase the invoice");
});

test("`at 4pm`, a bare `4pm`, and `16:00` all name the same hour", () => {
  for (const line of ["standup at 4pm", "standup 4pm", "standup 16:00"]) {
    const a = parseQuickAdd(line, NOW);
    assert.equal(a.dueMs, at(0, 16, 0), line);
    assert.equal(a.hasTime, true, line);
    assert.equal(a.title, "standup", line);
  }
  assert.equal(parseQuickAdd("standup at 4:30pm", NOW).dueMs, at(0, 16, 30));
  assert.equal(parseQuickAdd("standup at 9am", NOW).dueMs, at(1, 9, 0), "9am has passed at 10:00, so it is tomorrow's");
});

test("a bare number is not a time", () => {
  // The failure this guards is the one that makes a quick-add bar
  // untrustworthy: a word silently leaving the title.
  const a = parseQuickAdd("buy 4 lemons", NOW);
  assert.equal(a.title, "buy 4 lemons");
  assert.equal(a.dueMs, null);
});

test("#tag, !/!!/!!!, * and @myday", () => {
  const a = parseQuickAdd("fix the roof #home #urgent !!! * @myday", NOW);
  assert.equal(a.title, "fix the roof");
  assert.deepEqual(a.tags, ["home", "urgent"]);
  assert.equal(a.priority, 3);
  assert.equal(a.important, true);
  assert.equal(a.myDay, true);
  assert.equal(parseQuickAdd("x !", NOW).priority, 1);
  assert.equal(parseQuickAdd("x !!", NOW).priority, 2);
});

test("the plan's combined line", () => {
  const a = parseQuickAdd("pay rent fri 4pm #home !!", NOW);
  assert.equal(a.title, "pay rent");
  assert.equal(a.dueMs, at(2, 16, 0));
  assert.deepEqual(a.tags, ["home"]);
  assert.equal(a.priority, 2);
  // The chips are the proof: every consumed token comes back, so the UI can
  // show the parse BEFORE Enter.
  assert.deepEqual(
    a.chips.map((c) => c.kind),
    ["due", "tag", "priority"]
  );
  assert.equal(a.chips[0].raw, "fri 4pm");
});

// ---------- failure cases ----------

test("FAILURE CASE: an unparseable date stays in the title", () => {
  // Nothing here is a date the parser knows, and the rule is that it never
  // guesses and never swallows: every word survives.
  for (const line of ["ship the q3 roadmap", "review PR 1234", "sometime next month", "call on 15/05"]) {
    const a = parseQuickAdd(line, NOW);
    assert.equal(a.title, line, line);
    assert.equal(a.dueMs, null, line);
    assert.deepEqual(a.chips, [], line);
  }
});

test("FAILURE CASE: `fri` inside `friday's report` keeps the word", () => {
  // A weekday is a date only when it stands as its OWN token. `friday's` is
  // not `friday`, so neither the day nor the apostrophe is taken.
  const a = parseQuickAdd("write friday's report", NOW);
  assert.equal(a.title, "write friday's report");
  assert.equal(a.dueMs, null);

  // And the same for a weekday that opens the line: a title far more often
  // than a date.
  const b = parseQuickAdd("Friday retro notes", NOW);
  assert.equal(b.title, "Friday retro notes");
  assert.equal(b.dueMs, null);

  // The contrast that makes the two above fail-able rather than vacuous: the
  // SAME word, as its own token and not first, IS taken.
  const c = parseQuickAdd("write the report friday", NOW);
  assert.equal(c.title, "write the report");
  assert.equal(c.dueMs, at(2));
});

test("FAILURE CASE: an empty title is refused", () => {
  // Reported rather than thrown: the bar renders chips for a line that is not
  // yet submittable, which is the whole point of the live parse.
  const a = parseQuickAdd("#home !! @myday", NOW);
  assert.equal(a.title, "");
  assert.equal(a.valid, false);
  assert.equal(a.tags.length, 1, "the tokens it DID understand are still reported");

  assert.equal(parseQuickAdd("", NOW).valid, false);
  assert.equal(parseQuickAdd("   ", NOW).valid, false);
  assert.equal(parseQuickAdd("anything", NOW).valid, true);
});

test("only the first date on a line is taken", () => {
  const a = parseQuickAdd("move the tomorrow meeting to fri", NOW);
  assert.equal(a.dueMs, at(1), "`tomorrow` wins; `fri` stays in the title");
  assert.equal(a.title, "move the meeting to fri");
});

// ---- the header's two promises, pinned as properties (#3286 review round 1) ----

/** Whitespace tokens, the way the parser itself splits. */
function toks(line: string): string[] {
  return line.split(/\s+/).filter(Boolean);
}

test("PROPERTY: every consumed token comes back in exactly one chip", () => {
  // The module header promises this outright, and before this round it was
  // false: a second `at <time>` phrase overwrote the first phrase's chip
  // while leaving its tokens consumed, so `at 4pm` vanished from the title
  // and was named by nothing. A per-line assertion would have missed it — the
  // promise is a PROPERTY over the whole partition, so that is what is pinned.
  const lines = [
    "pay rent fri 4pm #home !!",
    "call bob at 4pm at 5pm",
    "standup 4pm 5pm",
    "fix the roof #home #urgent !!! * @myday",
    "move the tomorrow meeting to fri",
    "chase the invoice in 3 days",
    "retro next week at 9:30",
    "buy #home friday",
    "ship the q3 roadmap",
    "#home !! @myday",
  ];
  let checked = 0;
  for (const line of lines) {
    const a = parseQuickAdd(line, NOW);
    const accounted = [...toks(a.title), ...a.chips.flatMap((c) => toks(c.raw))].sort();
    assert.deepEqual(
      accounted,
      toks(line).sort(),
      `"${line}": title + chips must partition the input exactly — no token eaten ` +
        `without a chip, and none named twice`
    );
    checked += 1;
  }
  // Positive control: an assertion over a loop is vacuous if the loop is
  // empty, and a line with nothing to consume cannot fail it either.
  assert.equal(checked, lines.length);
  assert.ok(
    lines.some((l) => parseQuickAdd(l, NOW).chips.length >= 3),
    "at least one specimen must consume several tokens, or the partition is trivially satisfied"
  );
});

test("FAILURE CASE: a second TIME phrase stays in the title, as a second date does", () => {
  // First wins, for every class. Before this round the bare-time branch was
  // guarded by `timeOfDay === null` and the `at <time>` branch was not, which
  // is the one-rule-per-guard asymmetry CLAUDE.md names.
  const a = parseQuickAdd("call bob at 4pm at 5pm", NOW);
  assert.equal(a.title, "call bob at 5pm", "the second phrase is left in the title, not eaten");
  assert.equal(a.dueMs, at(0, 16, 0), "the FIRST time is the one that counts");
  assert.deepEqual(
    a.chips.map((c) => c.raw),
    ["at 4pm"],
    "and exactly the tokens it took are named by a chip"
  );

  // The CONTROL that makes the above a real finding rather than a preference:
  // the bare-time form already behaved this way, so the two spellings now
  // agree instead of disagreeing.
  const b = parseQuickAdd("standup 4pm 5pm", NOW);
  assert.equal(b.title, "standup 5pm");
  assert.equal(b.dueMs, at(0, 16, 0));
});

test("chips come back in SOURCE order, not with the due chip always first", () => {
  // The due chip used to be unshifted to position 0 regardless of where its
  // date phrase sat, which made "in reading order" false for any line whose
  // tag or priority preceded the date.
  const a = parseQuickAdd("buy #home friday", NOW);
  assert.deepEqual(
    a.chips.map((c) => c.kind),
    ["tag", "due"],
    "#home is read before friday, so its chip comes first"
  );
  // …and the reverse line still reads the other way, so the assertion is
  // about ORDER rather than about a fixed answer.
  const b = parseQuickAdd("buy friday #home", NOW);
  assert.deepEqual(
    b.chips.map((c) => c.kind),
    ["due", "tag"]
  );
  // A due phrase split across the line takes its EARLIEST token's position.
  const c = parseQuickAdd("pay rent fri #home 4pm", NOW);
  assert.deepEqual(
    c.chips.map((c2) => c2.kind),
    ["due", "tag"],
    "the due chip sits at `fri`, the first token its phrase consumed"
  );
  assert.equal(c.chips[0].raw, "fri 4pm");
});

test("FAILURE CASE: an implied hour never refuses a time the human named", () => {
  // #3286 review round 2, a regression round 1 introduced. `tonight` sets an
  // implied 19:00 without consuming a token or raising a chip, and round 1's
  // new guard read `timeOfDay === null` — which cannot tell that default from
  // an hour the human typed. So an explicit time written AFTER `tonight` was
  // silently refused and its words stranded in the title.
  //
  // The PROPERTY test above is blind to this BY CONSTRUCTION, which is why
  // this one asserts the ANSWER rather than the partition: the refused `at
  // 8pm` was never consumed, so it came back in the title and the
  // every-consumed-token-has-a-chip invariant held while the due date was
  // wrong. A partition pin cannot see a token that was never taken.
  const a = parseQuickAdd("dinner tonight at 8pm", NOW);
  assert.equal(a.dueMs, at(0, 20, 0), "the hour the human named wins over tonight's default");
  assert.equal(a.title, "dinner", "and its words are consumed, not stranded");

  // The bare spelling takes the same rule. This one was wrong BEFORE round 1
  // as well — the same defect reached through the other branch — and the
  // one-rule fix resolves both.
  const b = parseQuickAdd("dinner tonight 8pm", NOW);
  assert.equal(b.dueMs, at(0, 20, 0));
  assert.equal(b.title, "dinner");

  // The CONTROL: the reverse word order, which was correct throughout and must
  // stay correct. Without it this test would pass against an implementation
  // that simply let the LAST time win.
  const c = parseQuickAdd("x at 5pm tonight", NOW);
  assert.equal(c.dueMs, at(0, 17, 0), "an explicit time already parsed is not overridden by the default");

  // And the default still applies when the human named no hour at all.
  assert.equal(parseQuickAdd("dinner tonight", NOW).dueMs, at(0, 19, 0));
});

test("first-wins counts only the times the HUMAN named", () => {
  // The rule the header states, as a pair that separates the two readings.
  // Gating on "the parse has an hour" makes the first assertion 19:00; gating
  // on "the human named an hour" makes it 20:00 and leaves the second at
  // 16:00. Only the second reading satisfies both.
  assert.equal(parseQuickAdd("dinner tonight at 8pm", NOW).dueMs, at(0, 20, 0));
  assert.equal(
    parseQuickAdd("call bob at 4pm at 5pm", NOW).dueMs,
    at(0, 16, 0),
    "two times the human named still resolve first-wins"
  );
});

test("formatDue is relative near the present and absolute once that stops helping", () => {
  assert.equal(formatDue(at(0), NOW, false), "Today");
  assert.equal(formatDue(at(1), NOW, false), "Tomorrow");
  assert.equal(formatDue(at(-1), NOW, false), "Yesterday");
  assert.equal(formatDue(at(2), NOW, false), "Fri");
  assert.equal(formatDue(at(0, 16, 0), NOW, true), "Today 16:00");
  assert.equal(formatDue(at(30), NOW, false), "Fri 14 Jun");
});
