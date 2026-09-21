// The To-Do pane's reminder scan (#3263 S5).
//
// Every clock here is injected, as everywhere else in this feature: a reminder
// test that read the host clock would be a different test every minute.
//
// THE PROPERTY THAT MATTERS IS "ONCE", and it is the one a reminder gets wrong
// in the direction a human notices. The scan is pure and the `fired` set is the
// caller's, so "once" is testable without a pane, a timer or a DOM — which is
// the whole reason the scan is a module rather than four lines in `todopane.ts`.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  REMINDER_WINDOW_MS,
  pruneFired,
  reminderKey,
  reminderSummary,
  reminderText,
  scanReminders,
} from "../src/todoreminders.ts";
import type { TodoItem } from "../src/todomodel.ts";

const NOW = Date.UTC(2026, 4, 15, 12, 0, 0);
const MIN = 60_000;

let seq = 0;
function item(over: Partial<TodoItem> = {}): TodoItem {
  seq += 1;
  return {
    id: `td-${seq}`,
    scope: "global",
    title: `task ${seq}`,
    notes: "",
    status: "open",
    done_ms: null,
    due_ms: null,
    remind_ms: null,
    my_day: null,
    priority: 0,
    important: false,
    tags: [],
    steps: [],
    order: 0,
    created_ms: 0,
    created_by: { kind: "human" },
    updated_ms: 0,
    updated_by: { kind: "human" },
    rev: 0,
    archived_ms: null,
    deleted_ms: null,
    ...over,
  };
}

test("an item whose time has arrived fires, and one whose time has not does not", () => {
  const due = item({ due_ms: NOW - MIN });
  const later = item({ due_ms: NOW + MIN });
  const scan = scanReminders([due, later], NOW, new Set());
  assert.deepEqual(
    scan.notices.map((n) => n.id),
    [due.id]
  );
  assert.equal(scan.notices[0].kind, "due");
  assert.equal(scan.notices[0].atMs, NOW - MIN);
});

test("it fires ONCE per item, never twice — the caller's set is what makes that true", () => {
  const due = item({ due_ms: NOW - MIN });
  const first = scanReminders([due], NOW, new Set());
  assert.equal(first.notices.length, 1, "the first scan must actually fire");

  // The caller adds what the scan consumed, exactly as `todopane.ts` does.
  const fired = new Set(first.fired);
  assert.deepEqual([...fired], [`${due.id}@${NOW - MIN}`]);

  // Every later scan — and there is one a minute for as long as the pane is
  // open — is silent, and stays silent hours later.
  for (const t of [NOW, NOW + MIN, NOW + 60 * MIN, NOW + 3 * 60 * MIN]) {
    const again = scanReminders([due], t, fired);
    assert.deepEqual(again.notices, [], `fired again at +${(t - NOW) / MIN}m`);
    assert.deepEqual(again.fired, [], "a suppressed scan must consume nothing new");
  }
});

test("the scan is PURE: it never mutates the set it is given", () => {
  // The whole "reminders never write" claim rests on this one being true at the
  // function level too — a scan that mutated its argument would be keeping
  // state, and the next thing it would want is somewhere durable to keep it.
  const due = item({ due_ms: NOW - MIN });
  const fired = new Set<string>();
  scanReminders([due], NOW, fired);
  assert.equal(fired.size, 0, "the scan wrote to the caller's set");
});

test("remind_ms BEATS due_ms — an explicit 'tell me at' is not overridden by a default", () => {
  // Both in the past, so the choice is visible rather than incidental: a scan
  // that preferred `due_ms` would fire on the wrong time and label it wrong.
  const both = item({ due_ms: NOW - 10 * MIN, remind_ms: NOW - MIN });
  const scan = scanReminders([both], NOW, new Set());
  assert.equal(scan.notices.length, 1);
  assert.equal(scan.notices[0].atMs, NOW - MIN, "it fired on the due date, not the reminder");
  assert.equal(scan.notices[0].kind, "remind");

  // And the precedence holds when the reminder is in the FUTURE and the due
  // date has passed: the human said "tell me at 4pm", so 2pm is not the answer.
  const later = item({ due_ms: NOW - 10 * MIN, remind_ms: NOW + MIN });
  assert.deepEqual(scanReminders([later], NOW, new Set()).notices, []);
});

test("FAILURE CASE: an item completed before its reminder is skipped entirely", () => {
  // The case the plan names. Something ticked off at 15:00 must not nudge at
  // its 16:00 reminder — a notice about finished work is the fastest way to
  // make the channel worthless.
  const done = item({
    remind_ms: NOW - MIN,
    status: "done",
    done_ms: NOW - 10 * MIN,
  });
  const scan = scanReminders([done], NOW, new Set());
  assert.deepEqual(scan.notices, []);
  // AND IT CONSUMES NO KEY, which is the half an "it didn't fire" assertion
  // alone would not catch. Un-completing the item genuinely puts it back on the
  // list, and it must then remind.
  assert.deepEqual(scan.fired, [], "a completed item burned its reminder key");
  const reopened = { ...done, status: "open", done_ms: null };
  assert.equal(scanReminders([reopened], NOW, new Set()).notices.length, 1);
});

test("an archived or deleted item never fires — there is nothing for the action to open", () => {
  const archived = item({ due_ms: NOW - MIN, archived_ms: NOW - MIN });
  const deleted = item({ due_ms: NOW - MIN, deleted_ms: NOW - MIN });
  const live = item({ due_ms: NOW - MIN });
  const scan = scanReminders([archived, deleted, live], NOW, new Set());
  assert.deepEqual(
    scan.notices.map((n) => n.id),
    [live.id],
    "an archived or tombstoned item produced a notice"
  );
});

test("an item with no due date and no reminder has nothing to fire on", () => {
  assert.deepEqual(scanReminders([item({})], NOW, new Set()).notices, []);
  assert.equal(reminderKey(item({})), null);
});

test("a reminder that has MOVED gets a new key, so rescheduling fires again", () => {
  // The one case where firing twice is correct, and the reason the key carries
  // the time rather than being the bare id. A key of just the id would suppress
  // the new time forever; a key that changed on any write would fire again on
  // an unrelated agent edit.
  const at = NOW - MIN;
  const first = item({ remind_ms: at });
  const fired = new Set(scanReminders([first], NOW, new Set()).fired);
  assert.equal(fired.size, 1);

  // An unrelated edit — a title change, a new tag, a bumped rev — is silent.
  const edited = { ...first, title: "renamed", rev: 7, tags: ["x"] };
  assert.deepEqual(scanReminders([edited], NOW, fired).notices, []);

  // Moving the reminder is not.
  const moved = { ...first, remind_ms: at + 30_000 };
  const again = scanReminders([moved], NOW, fired);
  assert.equal(again.notices.length, 1, "a rescheduled reminder stayed suppressed");
  assert.equal(again.notices[0].atMs, at + 30_000);
});

test("a reminder older than the window is consumed SILENTLY, never shown late", () => {
  // A pane opened at 17:00 must not stack up every reminder the day already
  // passed. The suppressed key is still consumed, so the notice never arrives
  // at all rather than arriving whenever the window next slides over it.
  const old = item({ remind_ms: NOW - REMINDER_WINDOW_MS - MIN });
  const recent = item({ remind_ms: NOW - REMINDER_WINDOW_MS + MIN });
  const scan = scanReminders([old, recent], NOW, new Set());
  assert.deepEqual(
    scan.notices.map((n) => n.id),
    [recent.id],
    "the stale reminder was shown"
  );
  assert.equal(scan.fired.length, 2, "the stale reminder was left to fire another day");

  // And it stays suppressed on the next scan, with the set the caller kept.
  const fired = new Set(scan.fired);
  assert.deepEqual(scanReminders([old, recent], NOW + MIN, fired).notices, []);
});

test("notices arrive soonest-first, and ties break stably on id", () => {
  const a = item({ id: "td-b", due_ms: NOW - MIN });
  const b = item({ id: "td-a", due_ms: NOW - MIN });
  const c = item({ id: "td-c", remind_ms: NOW - 10 * MIN });
  const order = scanReminders([a, b, c], NOW, new Set()).notices.map((n) => n.id);
  assert.deepEqual(order, ["td-c", "td-a", "td-b"]);
});

test("pruneFired drops keys for items that are gone or rescheduled, and keeps live ones", () => {
  const live = item({ remind_ms: NOW - MIN });
  const liveKey = reminderKey(live);
  assert.ok(liveKey !== null);
  const fired = new Set([liveKey, "td-gone@1", `${live.id}@999`]);
  pruneFired(fired, [live]);
  assert.deepEqual([...fired], [liveKey], "a stale key survived, or a live one was dropped");

  // An item that loses its reminder entirely loses its key: there is no time
  // left that could come round again.
  const cleared = { ...live, remind_ms: null };
  pruneFired(fired, [cleared]);
  assert.deepEqual([...fired], []);
});

test("ONE toast per tick: several notices coalesce, and none is silently lost", () => {
  // #3301 review round 1 (rev-std). The app has ONE toast element, so the
  // pane's old `for (const n of notices) showToast(n)` had each call overwrite
  // the last: three reminders at 09:00 showed the third and the human never
  // learned the other two existed. That is a silent loss of exactly the thing
  // the feature is for, so the whole tick becomes one sentence.
  const mk = (title: string, at: number) => item({ title, remind_ms: at });
  const three = [mk("ship the notes", NOW - 3 * MIN), mk("pay rent", NOW - 2 * MIN), mk("call bob", NOW - MIN)];
  const notices = scanReminders(three, NOW, new Set()).notices;
  assert.equal(notices.length, 3, "precondition: all three came due on this tick");

  const text = reminderSummary(notices);
  // THE COUNT IS THE TRUE TOTAL, which is the half that makes coalescing
  // honest rather than merely tidy: what is not named is still accounted for.
  assert.match(text, /^3 reminders due/, text);
  assert.ok(text.includes("ship the notes"), "the soonest must be named: " + text);
  assert.ok(text.includes("pay rent"), "the second must be named: " + text);
  assert.ok(text.includes("and 1 more"), "the rest must be counted: " + text);
  assert.ok(!text.includes("call bob"), "a toast that lists every title is a dialog: " + text);
});

test("a single notice reads as a single notice, not as a list of one", () => {
  // The discriminator for the test above: if `reminderSummary` always used the
  // plural form, every assertion there would still pass and the ordinary case
  // would read "1 reminders due — …".
  const one = scanReminders([item({ title: "ship it", remind_ms: NOW - MIN })], NOW, new Set());
  assert.equal(reminderSummary(one.notices), "Reminder: ship it");
  assert.equal(reminderSummary(one.notices), reminderText(one.notices[0]));

  const due = scanReminders([item({ title: "ship it", due_ms: NOW - MIN })], NOW, new Set());
  assert.equal(reminderSummary(due.notices), "Due now: ship it");
});

test("exactly two notices name both and count nothing", () => {
  const two = [
    item({ title: "first", remind_ms: NOW - 2 * MIN }),
    item({ title: "second", remind_ms: NOW - MIN }),
  ];
  const text = reminderSummary(scanReminders(two, NOW, new Set()).notices);
  assert.equal(text, "2 reminders due — first, second");
  assert.ok(!text.includes("more"), "two fit, so nothing is elided: " + text);
});

test("a coalesced toast stays bounded however long the titles are", () => {
  // One very long to-do must not push the toast's action button off the strip,
  // and the multi-notice form has a tighter per-title budget than the single
  // one because it carries two of them plus a count.
  const long = [
    item({ title: "x".repeat(300), remind_ms: NOW - 2 * MIN }),
    item({ title: "y".repeat(300), remind_ms: NOW - MIN }),
    item({ title: "z".repeat(300), remind_ms: NOW - MIN }),
  ];
  const text = reminderSummary(scanReminders(long, NOW, new Set()).notices);
  assert.ok(text.length < 110, `a 3x300-char tick produced a ${text.length}-char toast`);
  assert.ok(text.includes("…"), "a truncated title must say it was truncated: " + text);
  assert.ok(text.includes("and 1 more"), text);
});

test("an empty tick produces no sentence at all, rather than an empty one", () => {
  assert.equal(reminderSummary([]), "");
});

test("the sentence says WHICH field fired, and a very long title is cut", () => {
  const remind = scanReminders([item({ title: "ship it", remind_ms: NOW - MIN })], NOW, new Set());
  assert.equal(reminderText(remind.notices[0]), "Reminder: ship it");
  const due = scanReminders([item({ title: "ship it", due_ms: NOW - MIN })], NOW, new Set());
  assert.equal(reminderText(due.notices[0]), "Due now: ship it");

  const long = scanReminders([item({ title: "x".repeat(200), due_ms: NOW - MIN })], NOW, new Set());
  const text = reminderText(long.notices[0]);
  assert.ok(text.length < 80, `a 200-char title produced a ${text.length}-char toast`);
  assert.ok(text.endsWith("…"), "a truncated title must say it was truncated");
});
