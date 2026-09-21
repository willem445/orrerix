// Natural-language quick-add parser for the To-Do pane (#3263 S3, plan §3).
//
// Lifted from `demo/todo-pane/quickadd.js` (the S0 mock, PR #3271), which was
// written to be lifted: pure, clock-injected, and written to the same token
// list. Types added, and one header claim corrected — see THE WEEKDAY RULE
// below.
//
// The rules this module keeps:
//
//   * PURE. No DOM, no `document`, no module-level mutable state. Input in,
//     one plain object out.
//   * THE CLOCK IS INJECTED. `parseQuickAdd(text, nowMs)` — never `Date.now()`
//     anywhere below. A parser that reads the wall clock cannot be tested for
//     "tomorrow" without the test being a different test every day, and
//     `test/todoquickadd.test.ts` pins exactly that.
//   * UNPARSEABLE TEXT STAYS IN THE TITLE. The parser never swallows a token
//     it did not understand and never guesses. The failure mode this avoids is
//     the one that makes a quick-add bar untrustworthy: you type a title, and
//     a word vanishes out of it.
//   * THE CHIPS ARE THE PROOF. Every token the parser consumed comes back in
//     `chips`, in reading order, so the UI can show the parse BEFORE Enter. A
//     chip the human did not expect is the signal to fix the sentence; a word
//     that silently became a due date is not.
//
//     Two rules make that literally true rather than nearly true, and both
//     were review findings on #3286 — the near-miss is the whole failure mode
//     here, so they are stated rather than left to the code. FIRST WINS, for
//     every class: one date per line, and one TIME THE HUMAN NAMED per line. A
//     second `at <time>` is left in the title exactly as a second bare `5pm`
//     and a second `tomorrow` are — before, the bare-time branch was guarded
//     and the `at` branch was not, so `call bob at 4pm at 5pm` ate
//     `at 4pm` and gave back no chip for it.
//
//     "the human named" is load-bearing and not a flourish. `tonight` implies
//     19:00 without the human naming an hour, so it does NOT start the
//     first-wins clock: `dinner tonight at 8pm` is 20:00, and so is
//     `dinner tonight 8pm`. Only `timeGiven` gates the rule. And READING
//     ORDER is the SOURCE order:
//     every chip records the index of the first token it consumed and the
//     list is sorted on it, so the due chip sits where its date phrase sits
//     rather than always first.
//
//     The due chip's RAW text is source-ordered for the same reason (#3285
//     item 6). It is joined from two spans that can appear either way round,
//     and joining them in a fixed order rendered `x at 5pm tonight` as
//     `tonight at 5pm` — a chip the human has to re-read their own line to
//     check is not a proof.
//
// THE WEEKDAY RULE, stated as the code actually implements it. A bare weekday
// is a date when it stands as its OWN token and is not the FIRST word of the
// line. So `friday's report` keeps the word (the token is `friday's`, which is
// not a weekday) and `Friday retro notes` keeps it (a line that opens with a
// weekday is far more often a title than a date) — but `write the report
// friday` DOES take it. The S0 mock's header claimed `ship the friday build`
// kept every word; it does not, and the claim did not survive the lift.
//
// Local time throughout. The store keeps epoch millis (plan §1); this is the
// only module that thinks in calendar days, and it derives every one of them
// from `nowMs` rather than from the host clock.

/** Weekday token → JS `Date#getDay()` index. Long and short spellings both. */
const WEEKDAYS: Record<string, number> = {
  sun: 0, sunday: 0,
  mon: 1, monday: 1,
  tue: 2, tues: 2, tuesday: 2,
  wed: 3, weds: 3, wednesday: 3,
  thu: 4, thur: 4, thurs: 4, thursday: 4,
  fri: 5, friday: 5,
  sat: 6, saturday: 6,
};

const DAY_NAMES = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_NAMES = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/** The hour a bare date means when no `at <time>` was given (plan §3). */
export const DEFAULT_DUE_HOUR = 9;
export const DEFAULT_DUE_MINUTE = 0;

const MS_PER_DAY = 86400000;

export type ChipKind = "due" | "tag" | "priority" | "important" | "myday";

/** One token (or phrase) the parser consumed, for the live chip strip. */
export interface Chip {
  kind: ChipKind;
  /** What the chip shows the human. */
  label: string;
  /**
   * The exact source text this chip ate, so the UI can point at it.
   *
   * A `due` chip can eat two spans (`fri` and `at 4pm`), and they are joined
   * in the order they appear IN THE LINE, never in the order the parser's
   * branches fired.
   */
  raw: string;
}

export interface QuickAdd {
  /** Everything the parser did NOT consume, joined by single spaces. */
  title: string;
  dueMs: number | null;
  /** True when the human named a time; false when the default hour was used. */
  hasTime: boolean;
  tags: string[];
  priority: 0 | 1 | 2 | 3;
  important: boolean;
  myDay: boolean;
  chips: Chip[];
  /** False for an empty title — reported, never thrown. See `parseQuickAdd`. */
  valid: boolean;
}

/** Midnight of the local day `nowMs` falls in. */
function startOfDay(nowMs: number): number {
  const d = new Date(nowMs);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** `dayStartMs` at hour:minute local — built through `Date` so DST is the
 *  platform's problem, not ours. */
function atTime(dayStartMs: number, hour: number, minute: number): number {
  const d = new Date(dayStartMs);
  d.setHours(hour, minute, 0, 0);
  return d.getTime();
}

/**
 * `n` calendar days after `dayStartMs`, as a local start-of-day.
 *
 * CALENDAR ARITHMETIC, NOT `+ n * MS_PER_DAY` (#3298). A local day is not
 * always 24 hours: on a DST fall-back day it is 25, so adding `MS_PER_DAY` to
 * that day's midnight lands at 23:00 the SAME day, and the `setHours` that
 * follows then pulls the date back — every relative date on the changeover
 * day comes out a day early. `tomorrow` resolved to TODAY (already past, so
 * the row dyed itself Overdue on the spot), `fri` to Thursday, `in 3 days` to
 * two. Spring-forward hides it, because the 23-hour day's drift still lands
 * inside the target.
 *
 * `Date#setDate` counts days rather than milliseconds, which is the operation
 * actually meant, and the `setHours(0,…)` zeroes the time-of-day, so an input
 * that is not already a midnight still comes back as a start-of-day — pinned
 * by the mid-day-input test in `test/todoquickadd.test.ts` — and
 * re-normalises in case the target day's own midnight moved. Found in review
 * on #3271; the lift to this module lost it and #3298 put it back at every
 * arm that builds a day.
 */
export function addDays(dayStartMs: number, n: number): number {
  const d = new Date(dayStartMs);
  d.setDate(d.getDate() + n);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/**
 * The next occurrence of `weekday` strictly after today, or today itself when
 * `includeToday`. "fri" on a Friday means NEXT Friday: if you meant today you
 * would have typed `today`, and a to-do that silently lands in the past hour
 * is worse than one a week out.
 */
function nextWeekday(dayStartMs: number, weekday: number, includeToday: boolean): number {
  const today = new Date(dayStartMs).getDay();
  let delta = (weekday - today + 7) % 7;
  if (delta === 0 && !includeToday) delta = 7;
  return addDays(dayStartMs, delta);
}

/**
 * `4pm`, `16:00`, `4:30pm`. Returns `{hour, minute}` or null.
 *
 * Deliberately narrow: a bare number is NOT a time ("buy 4 lemons").
 */
function parseTimeOfDay(token: string): { hour: number; minute: number } | null {
  const m = /^(\d{1,2})(?::(\d{2}))?\s*(am|pm)$/i.exec(token);
  if (m) {
    let hour = Number(m[1]);
    if (hour < 1 || hour > 12) return null;
    const minute = m[2] ? Number(m[2]) : 0;
    if (minute > 59) return null;
    const pm = m[3].toLowerCase() === "pm";
    if (hour === 12) hour = 0;
    return { hour: pm ? hour + 12 : hour, minute };
  }
  const hm = /^(\d{1,2}):(\d{2})$/.exec(token);
  if (hm) {
    const hour = Number(hm[1]);
    const minute = Number(hm[2]);
    if (hour > 23 || minute > 59) return null;
    return { hour, minute };
  }
  return null;
}

/** Human-facing due label: relative near the present, absolute once that stops
 *  helping. */
export function formatDue(dueMs: number, nowMs: number, hasTime: boolean): string {
  const dayStart = startOfDay(nowMs);
  const dueDay = startOfDay(dueMs);
  const days = Math.round((dueDay - dayStart) / MS_PER_DAY);
  const d = new Date(dueMs);
  const time = hasTime
    ? " " + String(d.getHours()).padStart(2, "0") + ":" + String(d.getMinutes()).padStart(2, "0")
    : "";

  let day: string;
  if (days === 0) day = "Today";
  else if (days === 1) day = "Tomorrow";
  else if (days === -1) day = "Yesterday";
  else if (days > 1 && days < 7) day = DAY_NAMES[d.getDay()];
  else if (days < -1 && days > -7) day = "Last " + DAY_NAMES[d.getDay()];
  else day = DAY_NAMES[d.getDay()] + " " + d.getDate() + " " + MONTH_NAMES[d.getMonth()];

  return day + time;
}

interface Token {
  raw: string;
  at: number;
  end: number;
}

/** Tokenise, keeping each token's span so consumed tokens can be dropped from
 *  the title. */
function tokenise(text: string): Token[] {
  const out: Token[] = [];
  const re = /\S+/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) out.push({ raw: m[0], at: m.index, end: re.lastIndex });
  return out;
}

/**
 * Parse a quick-add line.
 *
 * @param text the raw line the human typed
 * @param nowMs the clock — INJECTED, never read from the host
 */
export function parseQuickAdd(text: string, nowMs: number): QuickAdd {
  const src = typeof text === "string" ? text : "";
  const tokens = tokenise(src);
  const consumed = new Array<boolean>(tokens.length).fill(false);

  const dayStart = startOfDay(nowMs);

  /** Chips with the index of the first token each consumed, so the list can
   *  be returned in SOURCE order rather than in the order the branches
   *  happened to fire. */
  const chipsAt: { chip: Chip; at: number }[] = [];
  /** The earliest token index the due phrase consumed — its date part or its
   *  time part, whichever came first in the line. */
  let dueChipAt = Number.POSITIVE_INFINITY;
  const tags: string[] = [];
  let priority = 0;
  let important = false;
  let myDay = false;

  /** Day resolved by a date phrase, and the explicit time if one was given. */
  let dueDayMs: number | null = null;
  let timeOfDay: { hour: number; minute: number } | null = null;
  /**
   * The source text of every token the DUE phrase ate, each with the index
   * it was eaten at.
   *
   * A LIST rather than a `dueRaw` and a `timeRaw` because the chip's raw
   * text is joined from it, and two accumulators can only be joined in a
   * fixed order: `[dueRaw, timeRaw]` rendered `x at 5pm tonight` as
   * `tonight at 5pm`, re-ordering words the human can see on screen (#3285
   * item 6). The chip is the PROOF of what the parser ate — the header's
   * THE CHIPS ARE THE PROOF rule — and a proof that reorders the evidence
   * is one the human has to re-read their own line to check.
   */
  const duePartsAt: { raw: string; at: number }[] = [];
  /**
   * Did the HUMAN name a time, as opposed to the parse having produced one?
   *
   * These are two different questions and `timeOfDay` answers only the first
   * of them, which is what made the round-1 guard wrong (#3286 review round 2).
   * `tonight` sets an implied 19:00 without consuming a token or raising a
   * chip; a guard reading `timeOfDay === null` cannot tell that default from
   * an hour the human typed, so it refused the explicit `at 8pm` in
   * `dinner tonight at 8pm` and stranded the words in the title.
   *
   * So the FIRST-WINS rule gates on this flag, which only the two explicit
   * branches set, while `tonight`'s default keeps gating on `timeOfDay` —
   * "has the parse got an hour yet" is exactly the right question for a
   * default. An explicit time therefore overrides the default whichever order
   * they are written in, and two explicit times still resolve first-wins.
   *
   * A future branch that produces an hour WITHOUT the human naming one (an
   * `eod` keyword, a per-user default due time) must set `timeOfDay` and
   * leave this alone, or it inherits the same silent refusal.
   */
  let timeGiven = false;

  const lower = tokens.map((t) => t.raw.toLowerCase().replace(/[.,;:?]+$/, ""));

  const take = (from: number, count: number, raw: string): string => {
    for (let i = from; i < from + count; i++) consumed[i] = true;
    return raw;
  };

  /** [`take`] for a token belonging to the DUE phrase (its date part or its
   *  time part). Both parts feed one chip, so the chip's position is the
   *  earliest index either of them consumed — and the part itself is
   *  recorded WITH that index, so the chip's raw text can be rebuilt in
   *  source order however the branches fired. */
  const takeDue = (from: number, count: number, raw: string): void => {
    dueChipAt = Math.min(dueChipAt, from);
    duePartsAt.push({ raw, at: from });
    take(from, count, raw);
  };

  for (let i = 0; i < tokens.length; i++) {
    if (consumed[i]) continue;
    const t = lower[i];
    const next = lower[i + 1];
    const next2 = lower[i + 2];

    // --- #tag -------------------------------------------------------------
    if (/^#[\w-]+$/.test(tokens[i].raw)) {
      const tag = tokens[i].raw.slice(1).toLowerCase();
      if (!tags.includes(tag)) tags.push(tag);
      chipsAt.push({ chip: { kind: "tag", label: "#" + tag, raw: take(i, 1, tokens[i].raw) }, at: i });
      continue;
    }

    // --- !, !!, !!! priority ----------------------------------------------
    if (/^!{1,3}$/.test(t)) {
      priority = Math.max(priority, t.length);
      chipsAt.push({ chip: { kind: "priority", label: t, raw: take(i, 1, tokens[i].raw) }, at: i });
      continue;
    }

    // --- * important ------------------------------------------------------
    if (t === "*") {
      important = true;
      chipsAt.push({ chip: { kind: "important", label: "Important", raw: take(i, 1, tokens[i].raw) }, at: i });
      continue;
    }

    // --- @myday -----------------------------------------------------------
    if (t === "@myday" || t === "@today") {
      myDay = true;
      chipsAt.push({ chip: { kind: "myday", label: "My Day", raw: take(i, 1, tokens[i].raw) }, at: i });
      continue;
    }

    // --- at <time> --------------------------------------------------------
    // `!timeGiven` is the SAME guard the bare-time branch below carries, and
    // it is here because it was missing: without it a second `at <time>`
    // overwrote `timeRaw` while the first phrase's tokens stayed consumed, so
    // `call bob at 4pm at 5pm` ate `at 4pm` and reported no chip for it
    // (#3286 review round 1). One rule for both inputs — the one-rule-per-guard
    // rule in CLAUDE.md — and first wins, as it does for dates.
    //
    // It reads `timeGiven` and not `timeOfDay === null` because those are
    // different questions: see `timeGiven`'s own doc. Round 1 shipped the
    // latter and so refused an explicit time written after `tonight`.
    if (!timeGiven && t === "at" && next) {
      const tod = parseTimeOfDay(next);
      if (tod) {
        timeOfDay = tod;
        timeGiven = true;
        takeDue(i, 2, tokens[i].raw + " " + tokens[i + 1].raw);
        continue;
      }
    }
    // A bare `4pm` is unambiguous enough to take without the `at`. A bare
    // `16:00` is too — both carry their own unit. A bare `16` is not.
    if (!timeGiven) {
      const bare = parseTimeOfDay(t);
      if (bare && /[:apm]/i.test(t)) {
        timeOfDay = bare;
        timeGiven = true;
        takeDue(i, 1, tokens[i].raw);
        continue;
      }
    }

    if (dueDayMs !== null) continue; // one date per line; the rest is title

    // --- today / tomorrow / tonight ---------------------------------------
    if (t === "today" || t === "tonight") {
      dueDayMs = dayStart;
      // Gates on `timeOfDay`, NOT on `timeGiven`: this is a default, and
      // "has the parse got an hour yet" is the right question for one. An
      // explicit time already parsed wins; one written later overrides this,
      // because the branches above gate on `timeGiven`, which a default never
      // sets.
      if (t === "tonight" && !timeOfDay) timeOfDay = { hour: 19, minute: 0 };
      takeDue(i, 1, tokens[i].raw);
      continue;
    }
    if (t === "tomorrow" || t === "tmr") {
      dueDayMs = addDays(dayStart, 1);
      takeDue(i, 1, tokens[i].raw);
      continue;
    }

    // --- next week / next <weekday> / this <weekday> ----------------------
    if (t === "next" && next === "week") {
      dueDayMs = nextWeekday(dayStart, 1, false); // Monday of the coming week
      takeDue(i, 2, tokens[i].raw + " " + tokens[i + 1].raw);
      continue;
    }
    if ((t === "next" || t === "this") && next !== undefined && next in WEEKDAYS) {
      dueDayMs = nextWeekday(dayStart, WEEKDAYS[next], t === "this");
      takeDue(i, 2, tokens[i].raw + " " + tokens[i + 1].raw);
      continue;
    }

    // --- in N days / weeks -------------------------------------------------
    if (t === "in" && next !== undefined && /^\d{1,3}$/.test(next) && next2 !== undefined) {
      const n = Number(next);
      if (/^days?$/.test(next2)) {
        dueDayMs = addDays(dayStart, n);
        takeDue(i, 3, tokens[i].raw + " " + tokens[i + 1].raw + " " + tokens[i + 2].raw);
        continue;
      }
      if (/^weeks?$/.test(next2)) {
        dueDayMs = addDays(dayStart, n * 7);
        takeDue(i, 3, tokens[i].raw + " " + tokens[i + 1].raw + " " + tokens[i + 2].raw);
        continue;
      }
    }

    // --- a bare weekday ----------------------------------------------------
    // See THE WEEKDAY RULE in the header: its own token, and never the first
    // word of the line.
    if (i > 0 && t in WEEKDAYS) {
      dueDayMs = nextWeekday(dayStart, WEEKDAYS[t], false);
      takeDue(i, 1, tokens[i].raw);
      continue;
    }
  }

  // A time with no day means the next occurrence of that time: today if it is
  // still ahead, tomorrow if it has passed.
  if (dueDayMs === null && timeOfDay) {
    const todayAt = atTime(dayStart, timeOfDay.hour, timeOfDay.minute);
    dueDayMs = todayAt > nowMs ? dayStart : addDays(dayStart, 1);
  }

  let dueMs: number | null = null;
  const hasTime = timeOfDay !== null;
  if (dueDayMs !== null) {
    dueMs = hasTime
      ? atTime(dueDayMs, timeOfDay!.hour, timeOfDay!.minute)
      : atTime(dueDayMs, DEFAULT_DUE_HOUR, DEFAULT_DUE_MINUTE);
  }

  if (dueMs !== null) {
    // SOURCE order, not branch order. A stable sort keeps two parts that
    // somehow share an index in the order they were eaten.
    const raw = duePartsAt
      .slice()
      .sort((a, b) => a.at - b.at)
      .map((part) => part.raw)
      .join(" ");
    // `dueChipAt` is finite whenever a date or time token was consumed. A due
    // derived with NO token of its own cannot arise today — `dueMs` is set
    // only from `dueDayMs` or `timeOfDay`, and both come from `takeDue` —
    // but the fallback keeps the sort total rather than seeding it with
    // Infinity if a later branch ever forgets.
    chipsAt.push({
      chip: { kind: "due", label: formatDue(dueMs, nowMs, hasTime), raw },
      at: Number.isFinite(dueChipAt) ? dueChipAt : -1,
    });
  }

  // Source order. A stable sort (ES2019+) keeps two chips that somehow share
  // an index in the order they were pushed.
  const chips = chipsAt.sort((a, b) => a.at - b.at).map((c) => c.chip);

  const title = tokens
    .filter((_, i) => !consumed[i])
    .map((t) => t.raw)
    .join(" ")
    .trim();

  return {
    title,
    dueMs,
    hasTime,
    tags,
    priority: priority as 0 | 1 | 2 | 3,
    important,
    myDay,
    chips,
    // An empty title is refused at the call site. It is REPORTED rather than
    // thrown because the quick-add bar renders the chips for a line that is
    // not yet submittable, which is the whole point of the live parse.
    valid: title.length > 0,
  };
}
