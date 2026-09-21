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
  /** The exact source text this chip ate, so the UI can point at it. */
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
 * The next occurrence of `weekday` strictly after today, or today itself when
 * `includeToday`. "fri" on a Friday means NEXT Friday: if you meant today you
 * would have typed `today`, and a to-do that silently lands in the past hour
 * is worse than one a week out.
 */
function nextWeekday(dayStartMs: number, weekday: number, includeToday: boolean): number {
  const today = new Date(dayStartMs).getDay();
  let delta = (weekday - today + 7) % 7;
  if (delta === 0 && !includeToday) delta = 7;
  return dayStartMs + delta * MS_PER_DAY;
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

  const chips: Chip[] = [];
  const tags: string[] = [];
  let priority = 0;
  let important = false;
  let myDay = false;

  /** Day resolved by a date phrase, and the explicit time if one was given. */
  let dueDayMs: number | null = null;
  let dueRaw = "";
  let timeOfDay: { hour: number; minute: number } | null = null;
  let timeRaw = "";

  const lower = tokens.map((t) => t.raw.toLowerCase().replace(/[.,;:?]+$/, ""));

  const take = (from: number, count: number, raw: string): string => {
    for (let i = from; i < from + count; i++) consumed[i] = true;
    return raw;
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
      chips.push({ kind: "tag", label: "#" + tag, raw: take(i, 1, tokens[i].raw) });
      continue;
    }

    // --- !, !!, !!! priority ----------------------------------------------
    if (/^!{1,3}$/.test(t)) {
      priority = Math.max(priority, t.length);
      chips.push({ kind: "priority", label: t, raw: take(i, 1, tokens[i].raw) });
      continue;
    }

    // --- * important ------------------------------------------------------
    if (t === "*") {
      important = true;
      chips.push({ kind: "important", label: "Important", raw: take(i, 1, tokens[i].raw) });
      continue;
    }

    // --- @myday -----------------------------------------------------------
    if (t === "@myday" || t === "@today") {
      myDay = true;
      chips.push({ kind: "myday", label: "My Day", raw: take(i, 1, tokens[i].raw) });
      continue;
    }

    // --- at <time> --------------------------------------------------------
    if (t === "at" && next) {
      const tod = parseTimeOfDay(next);
      if (tod) {
        timeOfDay = tod;
        timeRaw = take(i, 2, tokens[i].raw + " " + tokens[i + 1].raw);
        continue;
      }
    }
    // A bare `4pm` is unambiguous enough to take without the `at`. A bare
    // `16:00` is too — both carry their own unit. A bare `16` is not.
    if (timeOfDay === null) {
      const bare = parseTimeOfDay(t);
      if (bare && /[:apm]/i.test(t)) {
        timeOfDay = bare;
        timeRaw = take(i, 1, tokens[i].raw);
        continue;
      }
    }

    if (dueDayMs !== null) continue; // one date per line; the rest is title

    // --- today / tomorrow / tonight ---------------------------------------
    if (t === "today" || t === "tonight") {
      dueDayMs = dayStart;
      if (t === "tonight" && !timeOfDay) timeOfDay = { hour: 19, minute: 0 };
      dueRaw = take(i, 1, tokens[i].raw);
      continue;
    }
    if (t === "tomorrow" || t === "tmr") {
      dueDayMs = dayStart + MS_PER_DAY;
      dueRaw = take(i, 1, tokens[i].raw);
      continue;
    }

    // --- next week / next <weekday> / this <weekday> ----------------------
    if (t === "next" && next === "week") {
      dueDayMs = nextWeekday(dayStart, 1, false); // Monday of the coming week
      dueRaw = take(i, 2, tokens[i].raw + " " + tokens[i + 1].raw);
      continue;
    }
    if ((t === "next" || t === "this") && next !== undefined && next in WEEKDAYS) {
      dueDayMs = nextWeekday(dayStart, WEEKDAYS[next], t === "this");
      dueRaw = take(i, 2, tokens[i].raw + " " + tokens[i + 1].raw);
      continue;
    }

    // --- in N days / weeks -------------------------------------------------
    if (t === "in" && next !== undefined && /^\d{1,3}$/.test(next) && next2 !== undefined) {
      const n = Number(next);
      if (/^days?$/.test(next2)) {
        dueDayMs = dayStart + n * MS_PER_DAY;
        dueRaw = take(i, 3, tokens[i].raw + " " + tokens[i + 1].raw + " " + tokens[i + 2].raw);
        continue;
      }
      if (/^weeks?$/.test(next2)) {
        dueDayMs = dayStart + n * 7 * MS_PER_DAY;
        dueRaw = take(i, 3, tokens[i].raw + " " + tokens[i + 1].raw + " " + tokens[i + 2].raw);
        continue;
      }
    }

    // --- a bare weekday ----------------------------------------------------
    // See THE WEEKDAY RULE in the header: its own token, and never the first
    // word of the line.
    if (i > 0 && t in WEEKDAYS) {
      dueDayMs = nextWeekday(dayStart, WEEKDAYS[t], false);
      dueRaw = take(i, 1, tokens[i].raw);
      continue;
    }
  }

  // A time with no day means the next occurrence of that time: today if it is
  // still ahead, tomorrow if it has passed.
  if (dueDayMs === null && timeOfDay) {
    const todayAt = atTime(dayStart, timeOfDay.hour, timeOfDay.minute);
    dueDayMs = todayAt > nowMs ? dayStart : dayStart + MS_PER_DAY;
  }

  let dueMs: number | null = null;
  const hasTime = timeOfDay !== null;
  if (dueDayMs !== null) {
    dueMs = hasTime
      ? atTime(dueDayMs, timeOfDay!.hour, timeOfDay!.minute)
      : atTime(dueDayMs, DEFAULT_DUE_HOUR, DEFAULT_DUE_MINUTE);
  }

  if (dueMs !== null) {
    const raw = [dueRaw, timeRaw].filter(Boolean).join(" ");
    chips.unshift({ kind: "due", label: formatDue(dueMs, nowMs, hasTime), raw });
  }

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
