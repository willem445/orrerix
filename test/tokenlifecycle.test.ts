// Lifecycle rates off audit rows (#3475 slice D). Every fixture is built from
// rows shaped like the backend writes them: `task-upsert`/`task-claim` carry
// the whole Task snapshot, `review-verdict` carries `pr` + `block`, the
// driver's `rd-ci-green`/`rd-ci-red` carry `pr`, `rd-lane-spawned` `round`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { lifecycle, calendarDays, type LifecycleAuditRowLike } from "../src/tokenlifecycle.ts";

const H = 3600_000;
const T0 = Date.UTC(2026, 5, 1, 12, 0, 0); // 1 June 2026 12:00 UTC — no DST edge near it

let seq = 0;
function task(ts: number, id: string, status: string, action = "task-upsert"): LifecycleAuditRowLike {
  // The whole snapshot, as the backend audits it — extra fields included, so
  // the reader is proven structural rather than exact.
  return { ts_ms: ts, actor: "w-1", action, detail: { id, status, title: `t ${id}`, notes: [], updated_ms: ts, seq: seq++ } };
}
function verdict(ts: number, pr: number, block: string, v = "pass"): LifecycleAuditRowLike {
  return { ts_ms: ts, actor: "rev-1", action: "review-verdict", detail: { pr, block, verdict: v, head: "abc" } };
}
function ci(ts: number, pr: number, green: boolean): LifecycleAuditRowLike {
  return { ts_ms: ts, actor: "orrerix", action: green ? "rd-ci-green" : "rd-ci-red", detail: { pr, head: "abc" } };
}
function laneSpawned(ts: number, pr: number, round: number): LifecycleAuditRowLike {
  return { ts_ms: ts, actor: "orrerix", action: "rd-lane-spawned", detail: { pr, block: "rev-lead", round } };
}
const WIDE = { startMs: T0 - 100 * H, endMs: T0 + 100 * H, bucketMs: 10 * H };

test("queued → in-progress → review → done: every span and the time to completion are exact", () => {
  const rows = [
    task(T0, "t-1", "queued"),
    task(T0 + 2 * H, "t-1", "in-progress", "task-claim"),
    task(T0 + 5 * H, "t-1", "review"),
    task(T0 + 6 * H, "t-1", "done"),
  ];
  const out = lifecycle(rows, WIDE);
  // The first row is `queued`, so its entry IS the queued instant — but the
  // row itself is the first the read holds, so the queued span is not
  // reported (entry unknown); in-progress and review are.
  assert.deepEqual(
    out.spans.map((s) => [s.status, s.ms]),
    [
      ["in-progress", 3 * H],
      ["review", 1 * H],
    ]
  );
  assert.deepEqual(out.timeInStatus.get("in-progress")?.values, [3 * H]);
  assert.deepEqual(out.timeInStatus.get("review")?.values, [1 * H]);
  assert.equal(out.ttc.length, 1);
  assert.deepEqual(out.ttc[0], { taskId: "t-1", startMs: T0, doneMs: T0 + 6 * H, ms: 6 * H, fromQueued: true });
  assert.equal(out.doneAtMs.get("t-1"), T0 + 6 * H);
  assert.deepEqual([...out.doneIds], ["t-1"]);
  assert.equal(out.transitions, 3);
  assert.equal(out.tasksSeen, 1);
  // `done` is still open at the end of the read.
  assert.equal(out.timeInStatus.get("done")?.stillOpen, 1);
});

test("rows arriving out of order are read in time order", () => {
  const rows = [
    task(T0 + 6 * H, "t-1", "done"),
    task(T0 + 2 * H, "t-1", "in-progress"),
    task(T0, "t-1", "queued"),
  ];
  const out = lifecycle(rows, WIDE);
  assert.deepEqual(out.spans.map((s) => [s.status, s.ms]), [["in-progress", 4 * H]]);
  assert.equal(out.ttc[0]?.ms, 6 * H);
});

test("a task first seen in `review` reports openedBeforeWindow, no queued span, and a lower-bound completion", () => {
  const rows = [task(T0, "t-2", "review"), task(T0 + 3 * H, "t-2", "done")];
  const out = lifecycle(rows, WIDE);
  assert.equal(out.spans.length, 0, "the review span's entry is unknown — no span");
  assert.equal(out.timeInStatus.get("review")?.openedBeforeWindow, 1);
  assert.deepEqual(out.timeInStatus.get("review")?.values, []);
  assert.equal(out.timeInStatus.get("queued"), undefined, "no queued row, no queued anything");
  assert.equal(out.ttc.length, 1);
  assert.equal(out.ttc[0]?.fromQueued, false, "started mid-lifecycle: a lower bound, and it says so");
  assert.equal(out.ttc[0]?.ms, 3 * H);
});

test("done is counted once, at the FIRST done row, despite a second done row", () => {
  const rows = [
    task(T0, "t-3", "queued"),
    task(T0 + 1 * H, "t-3", "done"),
    task(T0 + 2 * H, "t-3", "in-progress"), // reopened
    task(T0 + 4 * H, "t-3", "done"), // done again
  ];
  const out = lifecycle(rows, WIDE);
  assert.equal(out.doneAtMs.get("t-3"), T0 + 1 * H);
  assert.equal(out.ttc.length, 1);
  assert.equal(out.ttc[0]?.ms, 1 * H);
  assert.equal(out.series.done.reduce((a, b) => a + b, 0), 1);
  // Control: the reopen and second done ARE transitions — they are simply not
  // a second completion.
  assert.equal(out.transitions, 3);
});

test("a task first seen already `done` is doneUndated, never dated at that row", () => {
  const rows = [task(T0, "t-4", "done"), task(T0 + H, "t-4", "in-progress"), task(T0 + 2 * H, "t-4", "done")];
  const out = lifecycle(rows, WIDE);
  assert.equal(out.doneUndated, 1);
  assert.equal(out.doneIds.size, 0);
  assert.equal(out.ttc.length, 0);
});

test("an upsert with an unchanged status is not a transition and does not split the span", () => {
  const rows = [
    task(T0, "t-5", "queued"),
    task(T0 + 1 * H, "t-5", "in-progress"),
    task(T0 + 2 * H, "t-5", "in-progress"), // a note, a title edit — same status
    task(T0 + 3 * H, "t-5", "in-progress", "task-claim"),
    task(T0 + 5 * H, "t-5", "review"),
  ];
  const out = lifecycle(rows, WIDE);
  assert.equal(out.transitions, 2);
  assert.deepEqual(out.timeInStatus.get("in-progress")?.values, [4 * H], "one span, entered at the first in-progress row");
});

test("rows the reader cannot key are skipped, not guessed", () => {
  const rows: LifecycleAuditRowLike[] = [
    { ts_ms: T0, actor: "w", action: "task-upsert", detail: null },
    { ts_ms: T0, actor: "w", action: "task-upsert", detail: { id: "t-6" } },
    { ts_ms: T0, actor: "w", action: "task-upsert", detail: { status: "queued" } },
    { ts_ms: Number.NaN, actor: "w", action: "task-upsert", detail: { id: "t-7", status: "queued" } },
    { ts_ms: T0, actor: "w", action: "note-add", detail: { id: "t-8", status: "done" } },
  ];
  const out = lifecycle(rows, WIDE);
  assert.equal(out.tasksSeen, 0);
  // Positive control on the same shape: a keyed row IS seen.
  assert.equal(lifecycle([task(T0, "t-6", "queued")], WIDE).tasksSeen, 1);
});

test("events outside the window are not counted, but earlier rows still decide the previous status", () => {
  const rows = [
    task(T0 - 50 * H, "t-9", "queued"), // before the window
    task(T0 + 1 * H, "t-9", "in-progress"),
    task(T0 + 3 * H, "t-9", "done"),
    task(T0 + 1 * H, "t-10", "queued"),
    task(T0 + 30 * H, "t-10", "done"), // after the window
  ];
  const out = lifecycle(rows, { startMs: T0, endMs: T0 + 10 * H });
  assert.deepEqual(out.spans.map((s) => [s.taskId, s.status, s.ms]), [["t-9", "in-progress", 2 * H]]);
  assert.equal(out.ttc[0]?.ms, 53 * H, "time to completion runs from the queued row before the window");
  assert.deepEqual([...out.doneIds], ["t-9"]);
  assert.equal(out.transitions, 2);
  assert.equal(out.floorMs, T0 - 50 * H);
});

test("CI attempts: green and red counted per PR; a PR with verdicts but no CI rows is null, not zero", () => {
  const rows = [
    verdict(T0, 101, "rev-lead"),
    ci(T0 + 1 * H, 101, false),
    ci(T0 + 2 * H, 101, false),
    ci(T0 + 3 * H, 101, true),
    verdict(T0 + 4 * H, 202, "rev-lead"),
  ];
  const out = lifecycle(rows, WIDE);
  assert.deepEqual(out.ciAttemptsPerPr, [
    { pr: "101", attempts: { green: 1, red: 2 } },
    { pr: "202", attempts: null },
  ]);
});

test("review rounds: the busiest lane's verdict count; the driver's round counter is cross-checked and a disagreement surfaced", () => {
  const rows = [
    // PR 7: two rounds, each reviewed by two lanes — 4 verdicts, 2 rounds.
    laneSpawned(T0, 7, 1),
    verdict(T0 + 1 * H, 7, "rev-std", "fail"),
    verdict(T0 + 1 * H, 7, "rev-lead", "fail"),
    laneSpawned(T0 + 2 * H, 7, 2),
    verdict(T0 + 3 * H, 7, "rev-std"),
    verdict(T0 + 3 * H, 7, "rev-lead"),
    // PR 8: the driver says round 3, the verdicts the read holds say 1 — one
    // of the two lost rows, and the pane must say so.
    laneSpawned(T0, 8, 3),
    verdict(T0 + 4 * H, 8, "rev-lead"),
    // PR 9: never driven — no driver counter at all.
    verdict(T0 + 5 * H, 9, "rev-lead"),
  ];
  const out = lifecycle(rows, WIDE);
  assert.deepEqual(
    out.reviewRoundsPerPr.map((r) => [r.pr, r.rounds, r.verdicts, r.driverRounds, r.disagrees]),
    [
      ["7", 2, 4, 2, false],
      ["8", 1, 1, 3, true],
      ["9", 1, 1, null, false],
    ]
  );
});

test("rd-handback carries no round, so a `round` on it is not read as the driver's counter", () => {
  const rows: LifecycleAuditRowLike[] = [
    verdict(T0, 5, "rev-lead"),
    { ts_ms: T0 + H, actor: "orrerix", action: "rd-handback", detail: { pr: 5, agent: "w-1", round: 9 } },
  ];
  assert.equal(lifecycle(rows, WIDE).reviewRoundsPerPr[0]?.driverRounds, null);
});

test("before/after partition on markTsMs splits every sample by its own event instant", () => {
  const mark = T0 + 10 * H;
  const rows = [
    task(T0, "a", "queued"),
    task(T0 + 2 * H, "a", "in-progress"),
    task(T0 + 4 * H, "a", "done"), // before: ttc 4h, in-progress 2h
    task(T0 + 8 * H, "b", "queued"),
    task(T0 + 9 * H, "b", "in-progress"),
    task(T0 + 12 * H, "b", "done"), // span leaves after the mark: ttc 4h after, in-progress 3h after
    verdict(T0 + 9 * H, 1, "rev-lead"),
    verdict(T0 + 11 * H, 2, "rev-lead"),
    verdict(T0 + 13 * H, 2, "rev-lead"),
  ];
  const out = lifecycle(rows, { ...WIDE, markTsMs: mark });
  assert.ok(out.partition);
  const { before, after } = out.partition;
  assert.equal(before.done, 1);
  assert.equal(after.done, 1);
  assert.deepEqual(before.ttcMs, [4 * H]);
  assert.deepEqual(after.ttcMs, [4 * H]);
  assert.deepEqual(before.timeInStatus.get("in-progress"), [2 * H]);
  assert.deepEqual(after.timeInStatus.get("in-progress"), [3 * H]);
  assert.deepEqual(before.timeInStatus.get("queued"), undefined, "queued spans are first rows: none known");
  assert.deepEqual(before.rounds, [1]);
  assert.deepEqual(after.rounds, [2]);
  // The halves partition the whole.
  assert.equal(before.done + after.done, out.doneIds.size);
  assert.equal(before.ttcMs.length + after.ttcMs.length, out.ttc.length);
  // No mark → no partition.
  assert.equal(lifecycle(rows, WIDE).partition, null);
});

test("the over-time series sums to the totals (calendar-day default buckets)", () => {
  // Local-time instants so the day buckets are the host's own calendar days.
  const day = (d: number, h: number) => new Date(2026, 5, d, h, 0, 0, 0).getTime();
  const rows = [
    task(day(1, 9), "a", "queued"),
    task(day(1, 10), "a", "in-progress"),
    task(day(1, 15), "a", "done"),
    task(day(1, 11), "b", "queued"),
    task(day(2, 8), "b", "in-progress"),
    task(day(3, 20), "b", "review"),
    task(day(3, 22), "b", "done"),
    task(day(2, 9), "c", "review"),
    task(day(3, 9), "c", "done"),
    verdict(day(2, 12), 4, "rev-lead"),
    verdict(day(3, 12), 5, "rev-lead"),
    verdict(day(3, 13), 5, "rev-lead"),
  ];
  const opts = { startMs: day(1, 0), endMs: day(4, 0) };
  const out = lifecycle(rows, opts);
  const s = out.series;
  assert.equal(s.bucketStarts.length, 3);
  assert.deepEqual(s.done, [1, 0, 2]);
  assert.equal(s.done.reduce((a, b) => a + b, 0), out.doneIds.size);
  const sorted = (xs: number[]) => [...xs].sort((a, b) => a - b);
  assert.deepEqual(sorted(s.ttcMs.flat()), sorted(out.ttc.map((t) => t.ms)));
  for (const [status, stats] of out.timeInStatus) {
    const per = s.timeInStatus.get(status) ?? [];
    assert.deepEqual(sorted(per.flat()), sorted(stats.values), `series for ${status}`);
  }
  assert.ok(s.timeInStatus.size > 0, "positive control: some status span was bucketed");
  assert.deepEqual(sorted(s.rounds.flat()), sorted(out.reviewRoundsPerPr.map((r) => r.rounds)));
  assert.deepEqual(s.rounds, [[], [1], [2]]);
  // done-per-day and the default series agree on calendar days.
  assert.deepEqual(out.donePerDay.counts, s.done);
  assert.equal(out.donePerDay.rate, 1);
});

test("floorMs is null when nothing was read, and a read at the cap may be truncated", () => {
  const empty = lifecycle([], WIDE);
  assert.equal(empty.floorMs, null);
  assert.equal(empty.mayBeTruncated, false);
  const full = Array.from({ length: 5000 }, (_, i) => task(T0 + i, `x${i}`, "queued"));
  assert.equal(lifecycle(full, WIDE).mayBeTruncated, true);
  assert.equal(lifecycle(full.slice(1), WIDE).mayBeTruncated, false);
});

test("calendarDays on an empty or inverted window is empty", () => {
  assert.deepEqual(calendarDays(T0, T0), []);
  assert.deepEqual(calendarDays(T0, T0 - 1), []);
  assert.deepEqual(calendarDays(Number.NaN, T0), []);
});

test("done-per-day counts CALENDAR days across both DST shifts, in a zone that has them", () => {
  // CI runs UTC, where every day is 24 hours and `start + n × 86 400 000` is
  // indistinguishable from calendar arithmetic. So force a DST zone in a child
  // `node` (TZ is read at process start) — America/Chicago, where 8 March 2026
  // is 23 hours long and 1 November 2026 is 25. Each done instant sits in the
  // hour a 24-hour stride would misfile.
  const script = [
    "const { lifecycle } = await import(process.argv[1]);",
    "const at = (y, m, d, h, mi) => new Date(y, m, d, h, mi, 0, 0).getTime();",
    "const row = (ts, id, status) => ({ ts_ms: ts, actor: 'w', action: 'task-upsert', detail: { id, status } });",
    "const out = [];",
    // Spring: window 7..10 March. A done at 00:30 on 9 March belongs to day 2;
    // a 24h stride starts day 2 at 01:00 and files it under 8 March.
    "{ const start = at(2026, 2, 7, 0, 0), end = at(2026, 2, 10, 0, 0);",
    "  const rows = [row(start + 1000, 'a', 'queued'), row(at(2026, 2, 9, 0, 30), 'a', 'done')];",
    "  const r = lifecycle(rows, { startMs: start, endMs: end });",
    "  out.push({ lens: r.donePerDay.days.map((d, i, a) => (a[i + 1] ?? end) - d), counts: r.donePerDay.counts, series: r.series.done }); }",
    // Autumn: window 31 Oct..3 Nov. A done at 23:30 on 1 November belongs to
    // day 1; a 24h stride starts day 2 at 23:00 on 1 November and files it there.
    "{ const start = at(2026, 9, 31, 0, 0), end = at(2026, 10, 3, 0, 0);",
    "  const rows = [row(start + 1000, 'b', 'queued'), row(at(2026, 10, 1, 23, 30), 'b', 'done')];",
    "  const r = lifecycle(rows, { startMs: start, endMs: end });",
    "  out.push({ lens: r.donePerDay.days.map((d, i, a) => (a[i + 1] ?? end) - d), counts: r.donePerDay.counts, series: r.series.done }); }",
    "process.stdout.write(JSON.stringify(out));",
  ].join("\n");
  // A file:// URL built from this test's own URL — never a path (`C:C:` on Windows).
  const modulePath = new URL("../src/tokenlifecycle.ts", import.meta.url).href;
  const res = spawnSync(
    process.execPath,
    ["--experimental-strip-types", "--no-warnings", "--input-type=module", "-e", script, modulePath],
    { env: { ...process.env, TZ: "America/Chicago" }, encoding: "utf8" }
  );
  assert.equal(res.status, 0, `child failed: ${res.stderr}`);
  const [spring, autumn] = JSON.parse(res.stdout) as { lens: number[]; counts: number[]; series: number[] }[];

  // Positive control: the forced zone took — the shift days really are 23h and
  // 25h, so the assertions below discriminate.
  assert.deepEqual(spring.lens, [24 * H, 23 * H, 24 * H], "TZ=America/Chicago did not take in the child");
  assert.deepEqual(autumn.lens, [24 * H, 25 * H, 24 * H], "TZ=America/Chicago did not take in the child");

  assert.deepEqual(spring.counts, [0, 0, 1], "a done at 00:30 on 9 March was filed under 8 March");
  assert.deepEqual(autumn.counts, [0, 1, 0], "a done at 23:30 on 1 November was filed under 2 November");
  // The default series buckets are the same calendar days.
  assert.deepEqual(spring.series, spring.counts);
  assert.deepEqual(autumn.series, autumn.counts);
});
