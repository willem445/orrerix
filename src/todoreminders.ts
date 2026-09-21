// The To-Do pane's reminders (#3263 S5) — DOM-free, clock-injected, testable.
//
// A PER-VIEWER TIMER, AND IT NEVER WRITES TO THE STORE. This is the one rule
// the whole module is shaped around, and it is not a performance choice:
//
//  * the store has TWO writer processes (the pane and every agent through
//    MCP), and a "reminded" flag written from a viewer would be a third kind of
//    write that says nothing about the human's intent;
//  * orrerix can be open in more than one window and the list is one file, so
//    a store-side flag would make WHICHEVER pane scanned first the only one
//    that ever notices — every other viewer would silently lose the reminder;
//  * a machine that was asleep at 09:00 would, on a store-side flag, either
//    burn the reminder unseen or need a "did anyone see this" field the schema
//    does not have.
//
// So "has this already fired?" is answered from a `Set` the CALLER owns, held
// for the life of the pane and dropped with it. The cost is honest and stated:
// a reminder fires again in a pane opened after it was dismissed in another,
// and nothing survives a reload. That is the right trade for a nudge — see
// `doc/design/todo-pane.md` §"Reminders never write".
//
// THE CLOCK IS A PARAMETER, as in `todomodel.ts` and `todoview.ts`. Nothing
// here reads `Date.now()`: a reminder scan tested against the host clock is a
// different test every minute.

// Explicit `.ts`: VALUE imports in a module `node --test` loads off disk
// rather than through Vite (the convention `todomodel.ts` states).
import { isArchived, isDone, type TodoItem } from "./todomodel.ts";

/**
 * How far back a scan will look.
 *
 * A pane opened at 17:00 must not stack up every reminder the day already
 * passed — that is a wall of toasts about work the human has been looking at
 * all afternoon, and it trains them to dismiss the channel. Four hours is long
 * enough that a laptop shut for lunch still surfaces what it missed, short
 * enough that yesterday's list stays quiet.
 *
 * It is a WINDOW, not a debounce: an item whose time fell outside it is marked
 * fired without a notice (see [`scanReminders`]), so it never arrives late and
 * never arrives twice.
 */
export const REMINDER_WINDOW_MS = 4 * 60 * 60 * 1000;

/** One thing to tell the human about. `atMs` is the time that came due — the
 *  item's `remind_ms` when it has one, its `due_ms` otherwise. */
export interface ReminderNotice {
  /** The item's id, so the pane can select and pulse the row. */
  id: string;
  title: string;
  /** Which field fired. The message differs: a reminder was ASKED for, a due
   *  date merely arrived. */
  kind: "remind" | "due";
  atMs: number;
  /** The key to add to the fired set. See [`reminderKey`]. */
  key: string;
}

export interface ScanResult {
  /** What to show, soonest first. */
  notices: ReminderNotice[];
  /**
   * Every key this scan consumed — the notices' keys PLUS the ones it
   * suppressed (outside the window). The caller adds all of them, which is
   * what makes "fires once, never twice" true for the suppressed ones as well:
   * an item whose time has passed must not start firing the moment the window
   * happens to slide over it again on a clock change.
   */
  fired: string[];
}

/**
 * The key an item's pending reminder is remembered by.
 *
 * `id@atMs`, NOT the bare id — and that is the whole of the "never twice"
 * discipline plus the one case where firing again is correct. A reminder that
 * is RESCHEDULED (the human, or an agent, moves `remind_ms` to tomorrow) gets a
 * new key and fires at the new time; a re-render, a re-read, an unrelated agent
 * write to the same row, and a pane that has been open all day all reuse the
 * old key and stay quiet.
 *
 * `null` when the item has no time to fire on at all.
 */
export function reminderKey(item: TodoItem): string | null {
  const at = reminderAt(item);
  return at === null ? null : `${item.id}@${at.atMs}`;
}

function reminderAt(item: TodoItem): { atMs: number; kind: "remind" | "due" } | null {
  // `remind_ms` BEATS `due_ms` — an explicit "tell me at" is the human saying
  // when they want to hear about it, and honouring the due date instead would
  // be overriding that with a default. When both are absent there is nothing
  // to fire on; an item with only a due date is reminded AT the due time,
  // which is what a due date means to someone who did not set a reminder.
  if (item.remind_ms !== null) return { atMs: item.remind_ms, kind: "remind" };
  if (item.due_ms !== null) return { atMs: item.due_ms, kind: "due" };
  return null;
}

/**
 * Which items have come due since the last scan.
 *
 * Pure: it reads `fired` and never mutates it. The caller adds
 * [`ScanResult.fired`] to its own set — which is what makes this testable at
 * all, and what keeps the "never writes to the store" rule above structural
 * rather than a promise.
 *
 * An item is SKIPPED, with no notice and no key consumed, when it cannot
 * usefully be reminded about:
 *
 *  * **completed** — the failure case the plan names. Something ticked off at
 *    15:00 must not nudge at its 16:00 reminder; the human already did it, and
 *    a notice about finished work is the fastest way to make the channel
 *    worthless. It consumes no key deliberately: un-completing the item
 *    (`space` twice, or an agent's `todo_complete false`) genuinely does put
 *    it back on the list, and it should then remind.
 *  * **archived** and **deleted** — not on any view, so there is nothing for
 *    the toast's action to open.
 *  * **not yet due** — `atMs` is in the future.
 *  * **already fired** — its key is in `fired`.
 */
export function scanReminders(
  items: readonly TodoItem[],
  nowMs: number,
  fired: ReadonlySet<string>
): ScanResult {
  const notices: ReminderNotice[] = [];
  const consumed: string[] = [];
  for (const item of items) {
    if (isDone(item) || isArchived(item) || item.deleted_ms !== null) continue;
    const at = reminderAt(item);
    if (at === null) continue;
    if (at.atMs > nowMs) continue;
    const key = `${item.id}@${at.atMs}`;
    if (fired.has(key)) continue;
    consumed.push(key);
    // Outside the window: consumed, so it never arrives at all, rather than
    // shown late. See `REMINDER_WINDOW_MS`.
    if (nowMs - at.atMs > REMINDER_WINDOW_MS) continue;
    notices.push({ id: item.id, title: item.title, kind: at.kind, atMs: at.atMs, key });
  }
  notices.sort((a, b) => a.atMs - b.atMs || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  return { notices, fired: consumed };
}

/**
 * Drop keys for items that are gone.
 *
 * The same discipline `pruneDrafts` follows, for the same reason: a set keyed
 * by item id that nothing removes from grows for the life of the pane against
 * ids an agent may have deleted. Keyed by `id@atMs`, so it also drops the key
 * of a reminder that has since been rescheduled — which is correct, because
 * the NEW key is the one that matters and the old time can never come round
 * again.
 *
 * Mutates and returns nothing: the caller owns the set, as with `pruneDrafts`.
 */
export function pruneFired(fired: Set<string>, items: readonly TodoItem[]): void {
  const live = new Set<string>();
  for (const item of items) {
    const key = reminderKey(item);
    if (key !== null) live.add(key);
  }
  for (const key of [...fired]) if (!live.has(key)) fired.delete(key);
}

/** One title, bounded, so a single very long to-do cannot push the toast's
 *  action button off the end of the strip. */
function shortTitle(title: string, max: number): string {
  return title.length > max ? `${title.slice(0, max - 1)}…` : title;
}

/**
 * The sentence one notice becomes.
 *
 * Here rather than in `todopane.ts` because it is a decision (which of the two
 * fields fired, and how a human hears that) and this module is the testable
 * half.
 */
export function reminderText(notice: ReminderNotice): string {
  const title = shortTitle(notice.title, 60);
  return notice.kind === "remind" ? `Reminder: ${title}` : `Due now: ${title}`;
}

/**
 * The sentence a WHOLE tick becomes — one toast, however many came due.
 *
 * **Because the app has ONE toast element**, and a loop calling `showToast`
 * per notice makes each call overwrite the last: with three reminders due at
 * 09:00 the human sees the third and never learns the other two existed. That
 * is a silent loss of exactly the thing this feature is for (#3301 review
 * round 1, rev-std). Coalescing is the fix rather than a queue: a queue would
 * hold the human's attention for fifteen seconds at five seconds a toast, and
 * they came due together — they are one event.
 *
 * Two are NAMED, because two fit and a name is what makes a reminder
 * actionable; beyond that the rest are counted, because a toast that lists
 * eight titles is a dialog. The count is always the TRUE total, so nothing is
 * hidden without being accounted for.
 *
 * `notices` must be non-empty — the caller has just checked, and an empty
 * tick produces no toast at all rather than an empty one.
 */
export function reminderSummary(notices: readonly ReminderNotice[]): string {
  if (notices.length === 0) return "";
  if (notices.length === 1) return reminderText(notices[0]);
  const named = notices.slice(0, 2).map((n) => shortTitle(n.title, 28));
  const rest = notices.length - named.length;
  const tail = rest > 0 ? `, and ${rest} more` : "";
  return `${notices.length} reminders due — ${named.join(", ")}${tail}`;
}
