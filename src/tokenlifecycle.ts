// Lifecycle rates for the tokens pane (#3475 slice D): how long work items sit
// in each board status, how long an item takes from queued to done, how many
// items finish per day, how many review rounds and CI attempts a PR takes —
// all read off the pane's shared `AuditStore` read, never off the board.
//
// **Why the audit log and not the board.** A board row carries its CURRENT
// status and `updated_ms`, the instant of its LAST write — which is not the
// instant it went done (a note appended a week later moves it). The board holds
// no history at all. Every status change, though, funnels through one backend
// call that audits the WHOLE task snapshot as `task-upsert` (or `task-claim`,
// the guarded-grab variant of the same write), so consecutive rows for one
// `detail.id` whose `status` differs ARE the transitions. There is no
// `prev_status` on the row; it is derived here by keeping the last status seen.
//
// **What the window cannot see.** The read is bounded (`AUDIT_VIEW_LIMIT` rows
// over two rotating generations), so every figure here is over the rows that
// survived it:
//   - A task's FIRST row in the read enters its status at an unknown instant —
//     the row may be the transition or any later write. Its span in that status
//     is not reported; it is counted `openedBeforeWindow` instead. For the same
//     reason a task whose first row is already `done` is `doneUndated`, never a
//     done dated at that row.
//   - Time to completion from a first row that is not `queued` is a LOWER bound
//     and says so (`fromQueued: false`).
//   - A PR's review rounds and CI attempts are counted over the whole read, so
//     rounds older than `floorMs` are missing; `floorMs` travels on the result.
//
// **Raw values, not statistics.** Every rate is returned as the raw sample
// (milliseconds, counts); the view applies `statCell` (median/IQR/n with its
// `MEDIAN_MIN_N` null rule). One statistic definition, one place.
//
// **Import-free**, like every pure module the pane reads (TS5097: the tests
// import `../src/x.ts`, `tsc` wants the extension-less specifier). The audit
// row is re-declared structurally, as `tokenscorecard.ts` does.

// ── the wire, structurally ──────────────────────────────────────────────────

/** The audit-row shape this reads — a structural subset of `AuditStore`'s
 *  `AuditEntry` (`auditsummary.ts`), the same subset `tokenscorecard.ts`
 *  declares. */
export interface LifecycleAuditRowLike {
  ts_ms: number;
  actor: string;
  action: string;
  detail: unknown;
}

/** `orch_audit`'s row cap (`AUDIT_VIEW_LIMIT` in the backend). The wire carries
 *  no truncation flag, so a read AT the cap may have been cut — "may be", never
 *  "was". */
export const AUDIT_VIEW_LIMIT = 5000;

/** The actions whose `detail` is the whole `Task` snapshot. */
const TASK_ACTIONS: ReadonlySet<string> = new Set(["task-upsert", "task-claim"]);
const VERDICT = "review-verdict";
const CI_GREEN = "rd-ci-green";
const CI_RED = "rd-ci-red";
/** The review driver's own round counter lives on `rd-lane-spawned`
 *  (`round: review_rounds + 1`). `rd-handback` carries no `round`. */
const LANE_SPAWNED = "rd-lane-spawned";

const QUEUED = "queued";
const DONE = "done";

export interface LifecycleOpts {
  /** The chart window, `[startMs, endMs)`. Rows outside it still decide a
   *  task's previous status; only EVENTS inside it are counted. */
  startMs: number;
  endMs: number;
  /** A tuning mark: when given, `partition` splits every sample on the
   *  instant of its own event (`< markTsMs` is before). */
  markTsMs?: number;
  /** Width of a series bucket. Absent → CALENDAR days in local time (a DST
   *  day is 23 or 25 hours, so never `n × 86 400 000`). Present → fixed-width
   *  buckets from `startMs` — a span, meant for sub-day widths. */
  bucketMs?: number;
}

/** One status span: the task entered `status` at a known instant and left it
 *  at `leftMs`, both rows inside the read. */
export interface StatusSpan {
  taskId: string;
  status: string;
  enteredMs: number;
  leftMs: number;
  ms: number;
}

export interface StatusStats {
  /** Completed spans whose leaving instant is in the window. */
  values: number[];
  /** Tasks first SEEN in this status (entry instant unknown) whose leaving
   *  instant — or, if never left in the read, whose first row — is in the
   *  window. Their span is not in `values`. */
  openedBeforeWindow: number;
  /** Spans entered at a known instant in the window and still open at the
   *  end of the read. */
  stillOpen: number;
}

export interface TimeToCompletion {
  taskId: string;
  /** The task's first row in the read. It is its first `queued` row when
   *  `fromQueued`; otherwise the queued instant aged out and `ms` is a lower
   *  bound. */
  startMs: number;
  doneMs: number;
  ms: number;
  fromQueued: boolean;
}

export interface PrReview {
  pr: string;
  /** Review rounds: the verdict count of the PR's most-reviewed block (a
   *  round is one pass of each lane, so the busiest lane is the round count;
   *  summing across blocks would count a three-lane round three times). */
  rounds: number;
  /** Every verdict row for the PR, all blocks. */
  verdicts: number;
  /** The driver's own counter: max `rd-lane-spawned.detail.round`, `null`
   *  when the driver never opened a lane for this PR. */
  driverRounds: number | null;
  /** `driverRounds` is known and differs from `rounds` — surfaced, never
   *  reconciled: one of the two sources lost rows, and the pane cannot say
   *  which. */
  disagrees: boolean;
  /** Instant of the PR's last verdict in the read — inside the window, which
   *  is what puts the PR in `reviewRoundsPerPr` at all. */
  lastVerdictMs: number;
}

export interface PrCi {
  pr: string;
  /** `null` when the read holds no `rd-ci-*` row for this PR at all — the
   *  PR was never driven, which is not "zero attempts". */
  attempts: { green: number; red: number } | null;
}

export interface LifecyclePart {
  done: number;
  ttcMs: number[];
  timeInStatus: Map<string, number[]>;
  rounds: number[];
}

export interface LifecycleSeries {
  /** Bucket start instants; bucket i is `[bucketStarts[i], bucketStarts[i+1])`,
   *  the last one ending at the window's end. */
  bucketStarts: number[];
  /** Items whose first dated `done` falls in the bucket. */
  done: number[];
  /** Raw time-to-completion samples per bucket (by done instant). */
  ttcMs: number[][];
  /** Raw span samples per status per bucket (by leaving instant). */
  timeInStatus: Map<string, number[][]>;
  /** Per-PR round counts per bucket (by the PR's last verdict instant). */
  rounds: number[][];
}

export interface Lifecycle {
  /** Status-changing task rows in the window. */
  transitions: number;
  /** Distinct task ids in the read. */
  tasksSeen: number;
  spans: StatusSpan[];
  timeInStatus: Map<string, StatusStats>;
  ttc: TimeToCompletion[];
  /** First dated `done` per task, in the window. Slice C's contract. */
  doneAtMs: Map<string, number>;
  doneIds: Set<string>;
  /** Tasks whose first row in the read was already `done` — done at some
   *  instant the window cannot see, never dated at that row. */
  doneUndated: number;
  donePerDay: { days: number[]; counts: number[]; rate: number | null };
  reviewRoundsPerPr: PrReview[];
  ciAttemptsPerPr: PrCi[];
  /** Oldest row of the read; `null` means nothing was read, which is "we have
   *  not looked", never "there is no history". */
  floorMs: number | null;
  mayBeTruncated: boolean;
  partition: { before: LifecyclePart; after: LifecyclePart } | null;
  series: LifecycleSeries;
}

// ── helpers ─────────────────────────────────────────────────────────────────

type Rec = Record<string, unknown>;

function detailOf(row: LifecycleAuditRowLike): Rec | null {
  const d = row.detail;
  return d !== null && typeof d === "object" && !Array.isArray(d) ? (d as Rec) : null;
}

/** `detail.pr` as a numeric string — the rule `tokenscorecard.ts`'s
 *  `structuralPr` applies to the same rows. */
function prOf(d: Rec | null): string | null {
  const v = d?.pr;
  if (typeof v === "number" && Number.isFinite(v)) return String(v);
  if (typeof v === "string" && /^[0-9]+$/.test(v)) return v;
  return null;
}

function inWindow(ts: number, o: LifecycleOpts): boolean {
  return ts >= o.startMs && ts < o.endMs;
}

/** Local-midnight starts of every calendar day meeting `[startMs, endMs)`.
 *  Calendar arithmetic — `setDate` — because a DST day is not 24 hours. */
export function calendarDays(startMs: number, endMs: number): number[] {
  const out: number[] = [];
  if (!Number.isFinite(startMs) || !Number.isFinite(endMs) || endMs <= startMs) return out;
  const d = new Date(startMs);
  d.setHours(0, 0, 0, 0);
  while (d.getTime() < endMs) {
    out.push(d.getTime());
    d.setDate(d.getDate() + 1);
    d.setHours(0, 0, 0, 0);
  }
  return out;
}

function bucketStartsFor(o: LifecycleOpts): number[] {
  if (o.bucketMs === undefined) {
    const days = calendarDays(o.startMs, o.endMs);
    // The first calendar day starts at or before the window; the series is
    // clipped to the window, so its first bucket starts AT the window.
    if (days.length > 0) days[0] = o.startMs;
    return days;
  }
  const out: number[] = [];
  if (!(o.bucketMs > 0) || !Number.isFinite(o.startMs) || !(o.endMs > o.startMs)) return out;
  for (let t = o.startMs; t < o.endMs; t += o.bucketMs) out.push(t);
  return out;
}

/** Index of the bucket holding `ts`, or -1 outside the window. */
function bucketOf(starts: readonly number[], ts: number, endMs: number): number {
  if (starts.length === 0 || ts < starts[0] || ts >= endMs) return -1;
  let lo = 0;
  let hi = starts.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (starts[mid] <= ts) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

function emptyPart(): LifecyclePart {
  return { done: 0, ttcMs: [], timeInStatus: new Map(), rounds: [] };
}

function pushMap<K, V>(m: Map<K, V[]>, k: K, v: V): void {
  const xs = m.get(k);
  if (xs) xs.push(v);
  else m.set(k, [v]);
}

// ── the projection ──────────────────────────────────────────────────────────

export function lifecycle(auditRows: readonly LifecycleAuditRowLike[], opts: LifecycleOpts): Lifecycle {
  // Arrival order breaks ties: two rows in one millisecond keep the order the
  // log wrote them in (`sort` is stable).
  const rows = auditRows.filter((r) => Number.isFinite(r.ts_ms)).slice().sort((a, b) => a.ts_ms - b.ts_ms);

  let floorMs: number | null = null;
  for (const r of rows) if (floorMs === null || r.ts_ms < floorMs) floorMs = r.ts_ms;

  const starts = bucketStartsFor(opts);
  const series: LifecycleSeries = {
    bucketStarts: starts,
    done: starts.map(() => 0),
    ttcMs: starts.map(() => []),
    timeInStatus: new Map(),
    rounds: starts.map(() => []),
  };
  const mark = opts.markTsMs;
  const partition = mark === undefined ? null : { before: emptyPart(), after: emptyPart() };
  const side = (ts: number): LifecyclePart | null =>
    partition === null || mark === undefined ? null : ts < mark ? partition.before : partition.after;

  // ── tasks ──
  interface TaskTrack {
    firstMs: number;
    firstStatus: string;
    status: string;
    enteredMs: number;
    entryKnown: boolean;
    doneMs: number | null;
  }
  const tracks = new Map<string, TaskTrack>();
  const spans: StatusSpan[] = [];
  const timeInStatus = new Map<string, StatusStats>();
  const statFor = (s: string): StatusStats => {
    let st = timeInStatus.get(s);
    if (!st) {
      st = { values: [], openedBeforeWindow: 0, stillOpen: 0 };
      timeInStatus.set(s, st);
    }
    return st;
  };
  const ttc: TimeToCompletion[] = [];
  const doneAtMs = new Map<string, number>();
  let transitions = 0;
  let doneUndated = 0;

  const recordDone = (id: string, tr: TaskTrack, ts: number): void => {
    if (tr.doneMs !== null) return; // counted once, at the FIRST dated done
    tr.doneMs = ts;
    if (!inWindow(ts, opts)) return;
    doneAtMs.set(id, ts);
    const t: TimeToCompletion = {
      taskId: id,
      startMs: tr.firstMs,
      doneMs: ts,
      ms: ts - tr.firstMs,
      fromQueued: tr.firstStatus === QUEUED,
    };
    ttc.push(t);
    const b = bucketOf(starts, ts, opts.endMs);
    if (b >= 0) {
      series.done[b]++;
      series.ttcMs[b].push(t.ms);
    }
    const p = side(ts);
    if (p) {
      p.done++;
      p.ttcMs.push(t.ms);
    }
  };

  for (const row of rows) {
    if (!TASK_ACTIONS.has(row.action)) continue;
    const d = detailOf(row);
    const id = d?.id;
    const status = d?.status;
    if (typeof id !== "string" || id === "" || typeof status !== "string" || status === "") continue;
    const ts = row.ts_ms;
    const tr = tracks.get(id);
    if (!tr) {
      // A first row already `done` is never dated: it may be the transition
      // or any later write to an already-done task. `NaN` (not `null`) marks
      // the done as taken, so a later reopen-and-done cannot date it either —
      // its first completion is still the one the window cannot see.
      const undated = status === DONE;
      if (undated) doneUndated++;
      tracks.set(id, {
        firstMs: ts,
        firstStatus: status,
        status,
        enteredMs: ts,
        entryKnown: false,
        doneMs: undated ? Number.NaN : null,
      });
      continue;
    }
    if (tr.status === status) continue; // an unchanged-status write is not a transition
    if (inWindow(ts, opts)) transitions++;
    // Leaving `tr.status` at `ts`.
    if (tr.entryKnown) {
      const span: StatusSpan = { taskId: id, status: tr.status, enteredMs: tr.enteredMs, leftMs: ts, ms: ts - tr.enteredMs };
      if (inWindow(ts, opts)) {
        spans.push(span);
        statFor(tr.status).values.push(span.ms);
        const b = bucketOf(starts, ts, opts.endMs);
        if (b >= 0) {
          let per = series.timeInStatus.get(tr.status);
          if (!per) {
            per = starts.map(() => []);
            series.timeInStatus.set(tr.status, per);
          }
          per[b].push(span.ms);
        }
        const p = side(ts);
        if (p) pushMap(p.timeInStatus, tr.status, span.ms);
      }
    } else if (inWindow(ts, opts)) {
      statFor(tr.status).openedBeforeWindow++;
    }
    tr.status = status;
    tr.enteredMs = ts;
    tr.entryKnown = true;
    if (status === DONE) recordDone(id, tr, ts);
  }
  // Still in the status the read ended on.
  for (const tr of tracks.values()) {
    if (tr.entryKnown) {
      if (inWindow(tr.enteredMs, opts)) statFor(tr.status).stillOpen++;
    } else if (inWindow(tr.firstMs, opts)) {
      statFor(tr.status).openedBeforeWindow++;
    }
  }

  // ── done per calendar day ──
  const days = calendarDays(opts.startMs, opts.endMs);
  const dayCounts = days.map(() => 0);
  for (const ts of doneAtMs.values()) {
    const b = bucketOf(days, ts, opts.endMs);
    if (b >= 0) dayCounts[b]++;
  }
  const donePerDay = {
    days,
    counts: dayCounts,
    rate: days.length === 0 ? null : doneAtMs.size / days.length,
  };

  // ── PRs: review rounds, CI attempts ──
  interface PrTrack {
    byBlock: Map<string, number>;
    verdicts: number;
    lastVerdictMs: number | null;
    driverRounds: number | null;
    green: number;
    red: number;
    inWindow: boolean;
  }
  const prs = new Map<string, PrTrack>();
  const prFor = (pr: string): PrTrack => {
    let t = prs.get(pr);
    if (!t) {
      t = { byBlock: new Map(), verdicts: 0, lastVerdictMs: null, driverRounds: null, green: 0, red: 0, inWindow: false };
      prs.set(pr, t);
    }
    return t;
  };
  for (const row of rows) {
    const a = row.action;
    if (a !== VERDICT && a !== CI_GREEN && a !== CI_RED && a !== LANE_SPAWNED) continue;
    const d = detailOf(row);
    const pr = prOf(d);
    if (pr === null) continue;
    const t = prFor(pr);
    if (a === LANE_SPAWNED) {
      const r = d?.round;
      if (typeof r === "number" && Number.isFinite(r)) t.driverRounds = Math.max(t.driverRounds ?? r, r);
      continue;
    }
    if (inWindow(row.ts_ms, opts)) t.inWindow = true;
    if (a === VERDICT) {
      const block = typeof d?.block === "string" && d.block !== "" ? d.block : "unknown";
      t.byBlock.set(block, (t.byBlock.get(block) ?? 0) + 1);
      t.verdicts++;
      t.lastVerdictMs = row.ts_ms;
    } else if (a === CI_GREEN) t.green++;
    else t.red++;
  }
  const byPr = (a: string, b: string): number => Number(a) - Number(b) || (a < b ? -1 : a > b ? 1 : 0);
  const reviewRoundsPerPr: PrReview[] = [];
  const ciAttemptsPerPr: PrCi[] = [];
  for (const pr of [...prs.keys()].sort(byPr)) {
    const t = prs.get(pr)!;
    if (!t.inWindow) continue; // named only by a lane-spawn, or only outside the window
    // A PR's rounds belong to the window its LAST verdict falls in — the
    // instant its review arc last moved — so each PR is in exactly one
    // bucket and the series sums to this list.
    if (t.lastVerdictMs !== null && inWindow(t.lastVerdictMs, opts)) {
      const rounds = Math.max(...t.byBlock.values());
      reviewRoundsPerPr.push({
        pr,
        rounds,
        verdicts: t.verdicts,
        driverRounds: t.driverRounds,
        disagrees: t.driverRounds !== null && t.driverRounds !== rounds,
        lastVerdictMs: t.lastVerdictMs,
      });
      const b = bucketOf(starts, t.lastVerdictMs, opts.endMs);
      if (b >= 0) series.rounds[b].push(rounds);
      side(t.lastVerdictMs)?.rounds.push(rounds);
    }
    ciAttemptsPerPr.push({ pr, attempts: t.green + t.red === 0 ? null : { green: t.green, red: t.red } });
  }

  return {
    transitions,
    tasksSeen: tracks.size,
    spans,
    timeInStatus,
    ttc,
    doneAtMs,
    doneIds: new Set(doneAtMs.keys()),
    doneUndated,
    donePerDay,
    reviewRoundsPerPr,
    ciAttemptsPerPr,
    floorMs,
    mayBeTruncated: auditRows.length >= AUDIT_VIEW_LIMIT,
    partition,
    series,
  };
}
