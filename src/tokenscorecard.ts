// The tokens pane's scorecard table (#2011 slice D): one row per `block/cli`
// lane, with the per-PR review-loop figures beside it — PRs, median rounds to
// pass, fail rate, median wall-clock — plus the coverage-floor caption.
//
// **The script is the spec.** Every definition here is a port of
// `scripts/orch-scorecard.cjs` (slice A), which computed these columns first;
// this module runs the SAME arithmetic over the data the pane already
// receives (the shared `AuditStore` read and the usage-series roster) instead
// of a second reader drifting away from it. The ported function names are
// kept so a reader can diff the two side by side:
//
//   `laneStats`      — rounds, pass/fail, `rounds_to_pass` (the one-based
//                      index of the first pass in the lane's arrival-ordered
//                      verdicts, `null` if it never passed) and `fail_rate`
//                      (`fail / (pass + fail)` over DECIDED rows; `null` when
//                      nothing was decided, never `0`, which would read as a
//                      perfect record).
//   `computeWindows` — the PR window: first row NAMING the PR to the last
//                      `rd-*` row carrying it (`last-rd-row`), falling back to
//                      the last naming row (`last-naming-row`). `merged_at`
//                      and the loop window are NOT ported: the pane has no
//                      `--pr-meta` equivalent, so `end_source` always says
//                      which fallback answered, and the pane has no driver
//                      readout to feed a loop window to.
//   `statCell`       — the five-number cell: median with the EXCLUSIVE-median
//                      (Tukey hinge) quartiles, over finite numbers only, non-
//                      numeric inputs DROPPED and counted. A cell below
//                      `MEDIAN_MIN_N` (3) is `null`, not a number — n is
//                      reported at every size so a null reads as "not enough
//                      data" rather than as a missing measurement. MEDIANS,
//                      NEVER TOTALS: the PRs a lane worked are different work.
//   `laneCliOf`      — a block's cli on one PR is the single cli among that
//                      PR's credited delegates whose ROSTER block is the lane's
//                      block; zero delegates, two or more, or a resolved
//                      `unknown`/`mixed` is `null` plus the reason. Never
//                      guessed from the block's declared cli — a pane can be
//                      recycled onto a block whose roster line has since
//                      changed (slice A's H10).
//   `attributeAgents`— the STRUCTURAL tier only: `rd-lane-spawned` /
//                      `rd-handback` (`detail.agent` + `detail.pr`) and
//                      `review-verdict` (`actor` + `detail.pr`). The text tier
//                      (an `agent-spawn` brief naming `#N` inside the window)
//                      is deliberately NOT ported — it is H2/H3, a heuristic
//                      with a window bound, and a pane table whose population
//                      depended on it would be exact-looking and wrong. Which
//                      tier a lane's population came from is this file's
//                      stated residual, not a hidden one.
//   `coverageFloor`  — the spawn-row-missing half: a PR credited with a
//                      delegate whose `agent-spawn` row did not survive the
//                      read has LOST rows — proof of truncation, not a
//                      threshold. The window-at-floor half is not re-ported
//                      here: the pane already prints the audit floor and the
//                      `belowFloor` bars (`scorecardColumns` in
//                      `tokencharts.ts`), which is that half's answer at bar
//                      granularity.
//
// Self-contained by the same rule as `tokencharts.ts`: **no intra-src
// imports at all** (TS5097). The wire shapes are declared structurally; the
// view passes the real `AuditEntry` / `UsageSeries["agents"]` values straight
// in and TypeScript checks the two descriptions at that call site.
//
// Nothing is silently dropped, either: a PR whose lane could not be resolved
// to one block/cli is listed in `excluded` with the reason — the table's
// population is checkable against the log it was computed from.

// ── the wire, structurally ──────────────────────────────────────────────────

/** The audit-row shape this reads — a structural subset of `AuditStore`'s
 *  `AuditEntry` (`auditsummary.ts`). */
export interface ScorecardAuditRowLike {
  ts_ms: number;
  actor: string;
  action: string;
  detail: unknown;
}

/** The roster shape this reads — a structural subset of
 *  `UsageSeries["agents"]`. A `""` block or cli is the wire's honest
 *  "nobody recorded one" and renders as `unknown`. */
export interface ScorecardAgentLike {
  id: string;
  block: string;
  cli: string;
}

/** What an empty roster field renders as — `tokencharts.ts`'s rule, same
 *  wire. */
export const UNKNOWN = "unknown";

/** `orch-scorecard.cjs`'s `MEDIAN_MIN_N`: three points is already a thin
 *  claim; two is an average of a pair, and one is an anecdote wearing a
 *  statistic's clothes. */
export const MEDIAN_MIN_N = 3;

/** One lane's review figures on one PR — `laneStats`' per-block entry. */
export interface LaneStat {
  /** All verdict rows for the lane, decided or not. */
  rounds: number;
  pass: number;
  fail: number;
  /** Verdict spellings that are neither `pass` nor `fail` — counted, never
   *  folded into either column. */
  verdictsOther: number;
  /** One-based index of the first `pass`; `null` when the lane never passed. */
  roundsToPass: number | null;
  /** `fail / (pass + fail)` over decided rows; `null` when nothing was
   *  decided — never `0`, which would read as a perfect record. */
  failRate: number | null;
}

/** `laneCliOf`'s answer: the single cli, or `null` plus the reason it could
 *  not be reduced to one. A union, because the invariant "cli xor reason"
 *  is what lets the exclusion list name a reason without ever inventing
 *  one for a resolved lane. */
export type LaneCli = { cli: string; reason: null } | { cli: null; reason: string };

/** One PR's card — the pane-relevant subset of `scorePr`'s card. */
export interface PrCard {
  pr: string;
  /** First row naming the PR. `null` when no surviving row names it at all
   *  — "no window" and "an instantaneous PR" are different facts. */
  windowStartMs: number | null;
  windowEndMs: number | null;
  /** `computeWindows`' `end_source`, without the `merged_at` arm the pane
   *  cannot answer. */
  windowEndSource: "last-rd-row" | "last-naming-row" | null;
  /** The untailed span in hours, `round2`'d. `null` with no window. */
  wallClockH: number | null;
  /** Verdict lanes by the verdict row's own `detail.block`. */
  lanes: Map<string, LaneStat>;
  /** Structurally credited delegate ids whose ROSTER block resolved, by
   *  that block — the population `laneCliOf` reads. An agent the roster
   *  does not list resolves no block, exactly as A's `d.block !== block`
   *  filter skips it; it still counts for the floor via `creditedAll`. */
  credited: Map<string, string[]>;
  /** Every structurally credited delegate id, roster-resolved or not. */
  creditedAll: string[];
  /** `laneCliOf` per block. */
  laneCli: Map<string, LaneCli>;
  /** Credited delegates with no surviving `agent-spawn` row — the floor's
   *  proof of truncation, per PR. */
  missingSpawn: string[];
}

/** `statCell`'s five-number cell. Non-finite inputs are DROPPED and counted;
 *  below `MEDIAN_MIN_N` every figure is `null` and only `n`/`dropped` speak. */
export interface StatCell {
  n: number;
  dropped: number;
  median: number | null;
  q1: number | null;
  q3: number | null;
  iqr: number | null;
  min: number | null;
  max: number | null;
}

/** One row of the table: everything one `block/cli` lane did, over the PRs
 *  it was credited on. */
export interface ScorecardRow {
  /** `block/cli`, `blockCliKey`'s shape. */
  key: string;
  block: string;
  cli: string;
  /** The PRs credited to this lane, ascending numerically. */
  prs: string[];
  roundsToPass: StatCell;
  failRate: StatCell;
  wallClockH: StatCell;
}

export interface ScorecardTable {
  rows: ScorecardRow[];
  cards: PrCard[];
  /** PRs whose lane could not be placed on the table, with `laneCliOf`'s
   *  reason. A dropped population is a silent table; a listed one is a
   *  checkable one. */
  excluded: { pr: string; block: string; why: string }[];
  /** `coverageFloor`'s spawn-row-missing half over the read. */
  floor: {
    tsFirstMs: number | null;
    tsLastMs: number | null;
    rowsRead: number;
    missingSpawn: { pr: string; agents: string[] }[];
  };
}

// ── ports ───────────────────────────────────────────────────────────────────

function round2(n: number): number {
  return Math.round(n * 100) / 100;
}

/** `prTokenRe` — `#N` not followed by another digit, so `#294` never matches
 *  inside `#2943`. */
function prTokenRe(pr: string): RegExp {
  return new RegExp("#" + pr + "(?![0-9])");
}

/** `rowNamesPr` — structurally (`detail.pr === pr`) or by the `#N` token
 *  anywhere in the serialized detail. Slice A's PRs are numbers; this port's
 *  are numeric strings, so the structural arm normalizes through
 *  `structuralPr` before comparing. */
function rowNamesPr(detail: unknown, pr: string, re: RegExp): boolean {
  if (!detail || typeof detail !== "object") return false;
  if (structuralPr(detail as DetailRec) === pr) return true;
  return re.test(JSON.stringify(detail));
}

/** `medianOf` — input already sorted; `null` for an empty sample. */
function medianOf(sorted: readonly number[]): number | null {
  if (sorted.length === 0) return null;
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

/** `statCell` — the five-number cell every comparison column is made of.
 *  Quartiles use the exclusive-median (Tukey hinge) convention: the halves
 *  exclude the middle element on an odd-length sample. */
function statCell(values: readonly (number | null | undefined)[]): StatCell {
  const xs = values
    .filter((v): v is number => typeof v === "number" && Number.isFinite(v))
    .sort((a, b) => a - b);
  const dropped = values.length - xs.length;
  const cell: StatCell = { n: xs.length, dropped, median: null, q1: null, q3: null, iqr: null, min: null, max: null };
  if (xs.length < MEDIAN_MIN_N) return cell;
  const mid = Math.floor(xs.length / 2);
  const lower = xs.slice(0, mid);
  const upper = xs.length % 2 ? xs.slice(mid + 1) : xs.slice(mid);
  cell.median = round2(medianOf(xs) as number);
  cell.q1 = round2(medianOf(lower) as number);
  cell.q3 = round2(medianOf(upper) as number);
  cell.iqr = round2((cell.q3 as number) - (cell.q1 as number));
  cell.min = xs[0];
  cell.max = xs[xs.length - 1];
  return cell;
}

/** `laneStats` — one lane's verdict sequence (arrival order) to its figures. */
function laneStats(verdictSeq: readonly string[]): LaneStat {
  const decided = verdictSeq.filter((v) => v === "pass" || v === "fail");
  const firstPass = decided.indexOf("pass");
  const fail = decided.filter((v) => v === "fail").length;
  const pass = decided.length - fail;
  return {
    rounds: verdictSeq.length,
    pass,
    fail,
    verdictsOther: verdictSeq.length - decided.length,
    roundsToPass: firstPass === -1 ? null : firstPass + 1,
    failRate: decided.length === 0 ? null : round2(fail / decided.length),
  };
}

/** `laneCliOf` — the single cli a block's credited delegates resolve to, or
 *  `null` plus the reason. `unknown`/`mixed` never resolve: a lane whose cli
 *  cannot be named is excluded, never guessed from the block's declaration. */
function laneCliOf(clis: readonly string[], block: string): LaneCli {
  if (clis.length === 0) return { cli: null, reason: `no ${block} delegate` };
  const unique = [...new Set(clis)].sort();
  if (unique.length > 1) return { cli: null, reason: `${block} split across ${unique.join("+")}` };
  const only = unique[0];
  if (only === UNKNOWN || only === "mixed") return { cli: null, reason: `${block} cli ${only}` };
  return { cli: only, reason: null };
}

/** `blockCliKey` — the row key, empty fields rendered as `unknown`. */
function blockCliKey(block: string, cli: string): string {
  return `${block || UNKNOWN}/${cli || UNKNOWN}`;
}

// ── the projection ──────────────────────────────────────────────────────────

interface DetailRec {
  pr?: unknown;
  agent?: unknown;
  block?: unknown;
  verdict?: unknown;
}

function detailOf(row: ScorecardAuditRowLike): DetailRec | null {
  const d = row.detail;
  if (!d || typeof d !== "object" || Array.isArray(d)) return null;
  return d as DetailRec;
}

/** The PR a row structurally carries — `detail.pr` as a numeric string,
 *  `null` when the row carries no PR or a non-numeric one. */
function structuralPr(d: DetailRec | null): string | null {
  const v = d?.pr;
  if (typeof v === "number" && Number.isFinite(v)) return String(v);
  if (typeof v === "string" && /^[0-9]+$/.test(v)) return v;
  return null;
}

function byPrAsc(a: PrCard, b: PrCard): number {
  return Number(a.pr) - Number(b.pr) || (a.pr < b.pr ? -1 : 1);
}

/**
 * The scorecard table over the pane's own reads. `auditRows` is the shared
 * `AuditStore` read (already bounded by `AUDIT_VIEW_LIMIT` — these counts are
 * a lower bound over the log's window, which is what the floor caption is
 * for); `agents` is the usage-series roster, whose `block`/`cli` the lane
 * resolution reads — never a block's declared cli, per H10.
 */
export function scorecardTable(
  auditRows: readonly ScorecardAuditRowLike[],
  agents: readonly ScorecardAgentLike[],
): ScorecardTable {
  // Arrival order IS the sequence `rounds_to_pass` is a question about, and
  // the rows come from an append-only log — but a caller may hand an
  // unsorted array, so sort defensively the way slice A's `main` does.
  const rows = [...auditRows].sort((a, b) => a.ts_ms - b.ts_ms);

  const roster = new Map<string, ScorecardAgentLike>();
  for (const a of agents) if (!roster.has(a.id)) roster.set(a.id, a);

  // Every agent with a SURVIVING agent-spawn row — the set the floor's
  // spawn-row-missing rule is tested against (A's `indexSpawnCli`
  // population, read for survival rather than for cli).
  const spawned = new Set<string>();
  for (const row of rows) {
    if (row.action !== "agent-spawn") continue;
    const agent = detailOf(row)?.agent;
    if (typeof agent === "string" && agent) spawned.add(agent);
  }

  // Population: every PR the surviving log names STRUCTURALLY. The pane has
  // no `--pr-meta`, so a PR the log never named is not a PR this read can
  // score — that bound is the floor's job to disclose, not a guess to hide.
  const prs = new Set<string>();
  for (const row of rows) {
    const d = detailOf(row);
    if (row.action === "review-verdict" || (typeof row.action === "string" && row.action.startsWith("rd-"))) {
      const pr = structuralPr(d);
      if (pr !== null) prs.add(pr);
    }
  }

  const cards: PrCard[] = [];
  for (const pr of [...prs].sort((a, b) => Number(a) - Number(b) || (a < b ? -1 : 1))) {
    const re = prTokenRe(pr);
    // `computeWindows`, without the `merged_at` arm: first row NAMING the
    // PR, to the last `rd-*` row carrying it, falling back to the last
    // naming row. `end_source` says which fallback answered.
    let namedFirst: number | null = null;
    let namedLast: number | null = null;
    let rdLast: number | null = null;
    const verdictSeq = new Map<string, string[]>();
    const credited = new Map<string, string[]>();
    const creditedAll: string[] = [];
    const credit = (agent: unknown) => {
      if (typeof agent !== "string" || !agent) return;
      if (!creditedAll.includes(agent)) creditedAll.push(agent);
      // The delegate's BLOCK is the roster's, never the row's: `rd-handback`
      // carries no block and `laneCliOf` reads the roster for every credited
      // delegate — a pane recycled onto a renamed block still resolves to
      // the block the roster says it is (H10's no-guessing rule). An agent
      // the roster does not list resolves NO block and contributes no cli.
      const block = roster.get(agent)?.block;
      if (block === undefined) return;
      const list = credited.get(block || UNKNOWN) ?? [];
      if (!list.includes(agent)) list.push(agent);
      credited.set(block || UNKNOWN, list);
    };
    for (const row of rows) {
      const d = detailOf(row);
      if (!rowNamesPr(row.detail, pr, re)) continue;
      if (namedFirst === null) namedFirst = row.ts_ms;
      namedLast = row.ts_ms;
      const rowPr = structuralPr(d);
      if (typeof row.action === "string" && row.action.startsWith("rd-") && rowPr === pr) {
        rdLast = row.ts_ms;
      }
      if (row.action === "rd-lane-spawned" || row.action === "rd-handback") {
        if (rowPr === pr) credit(d?.agent);
        continue;
      }
      if (row.action === "review-verdict") {
        if (rowPr === pr) {
          const block = typeof d?.block === "string" && d.block ? d.block : UNKNOWN;
          const seq = verdictSeq.get(block) ?? [];
          seq.push(typeof d?.verdict === "string" ? d.verdict.toLowerCase() : "unknown");
          verdictSeq.set(block, seq);
          // Structural attribution: the verdict's own actor.
          credit(row.actor);
        }
        continue;
      }
    }
    const lanes = new Map<string, LaneStat>();
    for (const [block, seq] of verdictSeq) lanes.set(block, laneStats(seq));

    const laneCli = new Map<string, LaneCli>();
    // One lane per block: `lanes` keys by the verdict row's own block,
    // `credited` by the delegate's roster block — usually the same string,
    // so dedupe rather than iterate a block twice and double-count its PR.
    const blocks = [...new Set<string>([...lanes.keys(), ...credited.keys()])];
    for (const block of blocks) {
      const clis: string[] = [];
      for (const agent of credited.get(block) ?? []) {
        const entry = roster.get(agent);
        // An agent the roster does not list contributed no read cli — the
        // honest answer is one fewer delegate, not a guessed one.
        if (entry) clis.push(entry.cli || UNKNOWN);
      }
      laneCli.set(block, laneCliOf(clis, block));
    }

    const missingSpawn = creditedAll.filter((agent) => !spawned.has(agent)).sort();

    // `namedFirst` and `namedLast` are set together, but they are `let`s
    // mutated in the loop, so each read narrows explicitly.
    const startMs = namedFirst !== null && namedLast !== null ? namedFirst : null;
    const endMs = namedFirst !== null && namedLast !== null ? (rdLast ?? namedLast) : null;
    cards.push({
      pr,
      windowStartMs: startMs,
      windowEndMs: endMs,
      windowEndSource: endMs === null ? null : rdLast !== null ? "last-rd-row" : "last-naming-row",
      wallClockH: startMs !== null && endMs !== null ? round2((endMs - startMs) / 3_600_000) : null,
      lanes,
      credited,
      creditedAll,
      laneCli,
      missingSpawn,
    });
  }

  // The table: one row per `block/cli`, each PR contributing its lane's
  // figures to the lane it resolved to (A's `cliTable` aggregation, keyed
  // `block/cli` rather than by side). Unresolvable lanes are EXCLUDED and
  // listed, never averaged away.
  const groups = new Map<string, { row: ScorecardRow; samples: { rounds: (number | null)[]; fail: (number | null)[]; wall: number[] } }>();
  const excluded: { pr: string; block: string; why: string }[] = [];
  for (const card of cards) {
    // Same dedupe as the lane resolution above: one iteration per block.
    for (const block of [...new Set<string>([...card.lanes.keys(), ...card.credited.keys()])]) {
      const resolved = card.laneCli.get(block);
      if (!resolved || resolved.cli === null) {
        excluded.push({ pr: card.pr, block, why: resolved ? (resolved.reason ?? "unresolved") : "no credited delegate" });
        continue;
      }
      const key = blockCliKey(block, resolved.cli);
      let g = groups.get(key);
      if (!g) {
        g = {
          row: {
            key,
            block,
            cli: resolved.cli,
            prs: [],
            roundsToPass: { n: 0, dropped: 0, median: null, q1: null, q3: null, iqr: null, min: null, max: null },
            failRate: { n: 0, dropped: 0, median: null, q1: null, q3: null, iqr: null, min: null, max: null },
            wallClockH: { n: 0, dropped: 0, median: null, q1: null, q3: null, iqr: null, min: null, max: null },
          },
          samples: { rounds: [], fail: [], wall: [] },
        };
        groups.set(key, g);
      }
      g.row.prs.push(card.pr);
      const lane = card.lanes.get(block);
      g.samples.rounds.push(lane ? lane.roundsToPass : null);
      g.samples.fail.push(lane ? lane.failRate : null);
      // `wall_clock_h` is a fact about the PR, the same for every lane of
      // the card, as in slice A's `cliTable`.
      if (card.wallClockH !== null) g.samples.wall.push(card.wallClockH);
    }
  }
  for (const g of groups.values()) {
    g.row.prs.sort((a, b) => Number(a) - Number(b));
    g.row.roundsToPass = statCell(g.samples.rounds);
    g.row.failRate = statCell(g.samples.fail);
    g.row.wallClockH = statCell(g.samples.wall);
  }

  // `coverageFloor`'s spawn-row-missing half. A PR credited with a delegate
  // whose spawn row did not survive has PROVEN truncation — rows for it were
  // discarded, so its counters are a lower bound. (The window-at-floor half
  // is the bar-granularity floor `scorecardColumns` already renders.)
  let tsFirstMs: number | null = null;
  let tsLastMs: number | null = null;
  for (const r of rows) {
    if (tsFirstMs === null || r.ts_ms < tsFirstMs) tsFirstMs = r.ts_ms;
    if (tsLastMs === null || r.ts_ms > tsLastMs) tsLastMs = r.ts_ms;
  }
  const missingSpawn = cards
    .filter((c) => c.missingSpawn.length > 0)
    .map((c) => ({ pr: c.pr, agents: c.missingSpawn }));

  return {
    rows: [...groups.values()].map((g) => g.row).sort((a, b) => (a.key < b.key ? -1 : 1)),
    cards: cards.sort(byPrAsc),
    excluded,
    floor: { tsFirstMs, tsLastMs, rowsRead: rows.length, missingSpawn },
  };
}
