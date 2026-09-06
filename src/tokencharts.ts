// Pure projection for the token charts (#2011, slice C): the persisted usage
// series (`<group>/usage-series.jsonl`, slice B's schema) turned into the two
// pictures the human asked for — a time plot of spend per `block/cli`, and a
// stacked bar per feature — plus the marks, the before/after readout and the
// scorecard columns that sit beside them.
//
// Self-contained by the same rule as `timelinelayout.ts`: **no intra-src
// imports at all** (TS5097 — see `embedsplit.ts`). The wire types this reads
// are declared here STRUCTURALLY rather than imported from `orchestration.ts`,
// which is not merely import hygiene. It is what makes this module answer
// "given rows of this shape, what is the picture" instead of "given whatever
// `orch_usage_series` returns today": `tokenchartsview.ts` passes the real
// `UsageSeriesRow` / `UsageSeries` values straight in, and TypeScript checks
// the two descriptions against each other at that call site. A field slice B
// renames therefore fails the VIEW's compile, which is where it should fail.
//
// Three properties everything downstream leans on:
//
//  - **Nothing is ever guessed.** An empty `block`/`cli` is the wire's honest
//    "nobody recorded one" (slice B's design note), so it renders as
//    `unknown` and keeps its own key. Filing it under a real block would put
//    spend on a block that never spent it.
//  - **Nothing is silently dropped.** A delta outside the window, a key with
//    only its baseline row, an interval the reset clamp had to zero: each is
//    COUNTED and surfaced, never folded into a shorter chart. That is the
//    #240 lesson the series file's own `skipped` field already carries.
//  - **A gap is a zero, not a slope.** The bucket grid is dense over the whole
//    window, so a key that wrote nothing for an hour draws a flat line along
//    the floor. Joining its two neighbouring samples with one straight segment
//    would draw an hour of steady spend that did not happen — and would look
//    more plausible than the truth.

// ── the wire, structurally ──────────────────────────────────────────────────

/** One `kind: "sample"` row. Counters are **cumulative** as of `ts_ms` — the
 *  differencing is this module's job (`diffRows`), which is what makes a row
 *  that never landed cost resolution instead of correctness. */
export interface SeriesSampleLike {
  kind: "sample";
  ts_ms: number;
  /** The usage key (a CLI session id, else `agent:<id>`). This is the
   *  DIFFERENCING key — one cumulative counter lives under it — and it is NOT
   *  the series key the legend draws, which is `block/cli`. */
  key: string;
  /** The agent occupying that key at WRITE time. */
  agent: string;
  block: string;
  cli: string;
  role: string;
  in: number;
  out: number;
  cache_w: number;
  cache_r: number;
  cost_usd: number | null;
  estimated: boolean;
  source: string;
  model: string | null;
}

/** One `kind: "mark"` row — the repo's agent-facing configuration changed. */
export interface SeriesMarkLike {
  kind: "mark";
  ts_ms: number;
  changed: string[];
  fp: Record<string, string>;
  prev: Record<string, string>;
  fp_partial: boolean;
}

export type SeriesRowLike = SeriesSampleLike | SeriesMarkLike;

/** The roster dimension `orch_usage_series` returns alongside the rows, so a
 *  row whose agent has exited still labels. */
export interface AgentRowLike {
  id: string;
  block: string;
  cli: string;
  role: string;
  session: string | null;
  task: string;
}

/** The board row shape the attribution ladder walks — a structural subset of
 *  `tasksview.ts`'s `OrchTask`, so the real board rows pass straight in. */
export interface BoardRowLike {
  id: string;
  title: string;
  status?: string;
  kind?: string | null;
  parent?: string | null;
  assignee?: string | null;
  session?: string | null;
  issue?: string | null;
  pr?: string | null;
}

/** An audit row, as `AuditStore` hands them out (`auditsummary.ts`'s
 *  `AuditEntry`). */
export interface AuditRowLike {
  ts_ms: number;
  actor: string;
  action: string;
  detail: unknown;
}

// ── shared vocabulary ───────────────────────────────────────────────────────

/** What an empty `block` or `cli` renders as. A wire value of `""` means
 *  "nothing recorded one" — a row written before slice B's fields existed, or
 *  a CLI no `source` can be read off — and it keeps its OWN key rather than
 *  being folded into a real block's. A block genuinely named `unknown` would
 *  share this label, which is the honest outcome for a name that says
 *  nothing. */
export const UNKNOWN = "unknown";

/** The bar every agent the ladder could not place lands on. Rendered FIRST and
 *  present even when empty: a chart that quietly drops the spend it cannot
 *  attribute is the one that reads as complete when it isn't. */
export const UNATTRIBUTED = "(unattributed)";

/** The group-wide bar for orchestrator agents. They are NEVER attributed to a
 *  feature: no per-turn PR attribution exists for a long-lived orchestrator
 *  session, so charging its whole lifetime to whichever board row it happened
 *  to be holding would be a fabricated number. It gets its own bar and its own
 *  series in the plot, where its real spend IS visible. */
export const ORCHESTRATOR = "(orchestrator, group-wide)";

/** Default bucket for the read-side re-grid, matching slice B's writer spacing
 *  (`SERIES_BUCKET_MS`). The writer's spacing is measured from its own last
 *  row rather than from a grid, which is why the read side re-buckets. */
export const DEFAULT_BUCKET_MS = 5 * 60_000;

/** `beforeAfter`'s default half-width, in buckets — 12 × 5 min = one hour on
 *  each side of a mark. Surfaced by the view rather than implied, because the
 *  number IS the scope of the claim. */
export const DEFAULT_BEFORE_AFTER_K = 12;

/** The four token counters, plus their sum. `cost_usd` is deliberately not one
 *  of these: it is nullable, and a null means "no figure", never "nothing was
 *  spent", so it cannot travel the same summing path. */
export type TokenMetric = "in" | "out" | "cache_w" | "cache_r" | "total";

/** Every metric the before/after readout will average, cost included. */
export type Metric = TokenMetric | "cost_usd";

const num = (v: unknown): number => (typeof v === "number" && Number.isFinite(v) ? v : 0);
const nonEmpty = (v: string | null | undefined): string => (typeof v === "string" ? v.trim() : "");
const labelOf = (v: string | null | undefined): string => nonEmpty(v) || UNKNOWN;

export const isSample = (r: SeriesRowLike): r is SeriesSampleLike => r.kind === "sample";
export const isMark = (r: SeriesRowLike): r is SeriesMarkLike => r.kind === "mark";

/** The legend's key for a row: `block/cli`, both as the row itself carried
 *  them. Never a hardcoded roster list — a block or CLI this build has never
 *  heard of still gets its own line, which is the whole point of reading the
 *  axis off the data (the `"claude" ?` rule, applied to blocks). */
export const seriesKeyOf = (
  block: string | null | undefined,
  cli: string | null | undefined
): string => `${labelOf(block)}/${labelOf(cli)}`;

/** Whether a role is the orchestrator's. Read off `role` — the capability
 *  CLASS — never off the block id, which an operator may spell anything. */
export const isOrchestratorRole = (role: string | null | undefined): boolean =>
  nonEmpty(role).toLowerCase() === "orchestrator";

// ── the attribution ladder ──────────────────────────────────────────────────

/** Which rung decided, reported on every attribution so a reader can tell a
 *  strong answer from a weak one. `orchestrator` and `none` are terminal
 *  answers, not failures to try. */
export type AttributionVia = "assignee" | "session" | "brief" | "orchestrator" | "none";

export interface AgentAttribution {
  agent: string;
  /** The bar this agent's spend lands on: a board row id, `ORCHESTRATOR`, or
   *  `UNATTRIBUTED`. */
  bucket: string;
  /** Human label for that bar. */
  label: string;
  via: AttributionVia;
  /** The board row the ladder MATCHED, before the container walk — kept
   *  distinct from `bucket` so a reader can see the difference between "this
   *  agent's own task" and "the feature it rolls up to". Null on the two
   *  terminal rungs, where there is no honest row to name. */
  matched: string | null;
  /** What the container walk landed on: `feature`, `epic`, or `root` where the
   *  chain carried neither. Null on the terminal rungs. */
  level: "feature" | "epic" | "root" | null;
}

export interface AttributionBucket {
  id: string;
  label: string;
  kind: "feature" | "orchestrator" | "unattributed";
  /** Agent ids landing on this bar, in first-seen order — the tooltip's list,
   *  and what makes `(unattributed)` actionable rather than a shrug. */
  agents: string[];
}

export interface Attribution {
  /** Agent id -> where its spend lands. */
  byAgent: Map<string, AgentAttribution>;
  /** Bar identities in RENDER order: `(unattributed)` first (never hidden),
   *  then `(orchestrator, group-wide)`, then features in board order. */
  buckets: AttributionBucket[];
}

/** `#123` out of an issue/PR ref, as a bare number string. Accepts the several
 *  spellings a board row uses in practice: `#123`, `123`, and a full GitHub
 *  URL. */
export function refNumber(v: string | null | undefined): string | null {
  const s = nonEmpty(v);
  if (!s) return null;
  const url = s.match(/github\.com\/[^/\s]+\/[^/\s]+\/(?:issues|pull)\/(\d+)/i);
  if (url) return url[1];
  const hash = s.match(/#(\d+)/);
  if (hash) return hash[1];
  const bare = s.match(/^(\d+)$/);
  return bare ? bare[1] : null;
}

/** Every `#N` a brief mentions, in order of appearance, de-duplicated. The
 *  brief is prose an orchestrator wrote, so this reads it as prose: one issue
 *  is the common case and several is normal. Which of them WINS is decided by
 *  the board, not by position — see rung 3. */
export function briefRefs(task: string | null | undefined): string[] {
  const s = nonEmpty(task);
  if (!s) return [];
  const out: string[] = [];
  for (const m of s.matchAll(/#(\d+)/g)) if (!out.includes(m[1])) out.push(m[1]);
  return out;
}

/** The container chain above a row, nearest first, cycle-safe. Mirrors
 *  `taskboard.ts`'s `containerChain`: a `parent` naming a row that is not on
 *  the board ends the chain rather than throwing, and a cycle stops at the
 *  first repeat rather than looping forever. */
function chainOf(row: BoardRowLike, byId: Map<string, BoardRowLike>): BoardRowLike[] {
  const chain: BoardRowLike[] = [row];
  const seen = new Set<string>([row.id]);
  let cur = nonEmpty(row.parent);
  while (cur) {
    if (seen.has(cur)) break;
    const next = byId.get(cur);
    if (!next) break;
    seen.add(cur);
    chain.push(next);
    cur = nonEmpty(next.parent);
  }
  return chain;
}

/** Walk `parent` to the bar a row belongs on: the nearest `feature` ancestor,
 *  else the nearest `epic`, else the chain's root.
 *
 *  The `root` fallback is deliberate, and is NOT the same as unattributed. A
 *  board that runs no agile levels at all (`kind` absent on every row — legal,
 *  and the pre-#958 shape) would otherwise send every agent to
 *  `(unattributed)`, which would be false: the ladder DID find the row this
 *  agent is working, and a bar per top-level row is the honest picture of it.
 *  `level` records which of the three happened, so a reader never has to
 *  guess which kind of answer they are looking at. */
function barRowFor(
  row: BoardRowLike,
  byId: Map<string, BoardRowLike>
): { row: BoardRowLike; level: "feature" | "epic" | "root" } {
  const chain = chainOf(row, byId);
  const feature = chain.find((r) => nonEmpty(r.kind).toLowerCase() === "feature");
  if (feature) return { row: feature, level: "feature" };
  const epic = chain.find((r) => nonEmpty(r.kind).toLowerCase() === "epic");
  if (epic) return { row: epic, level: "epic" };
  return { row: chain[chain.length - 1], level: "root" };
}

/**
 * Place every agent on a bar, by the four-rung ladder — **first rung that
 * decides wins**, and the rung is reported as `via` on the result.
 *
 *  0. `role == "orchestrator"` -> `ORCHESTRATOR`, before any board lookup at
 *     all. An orchestrator is never attributed to a feature (see the
 *     constant), and this rung sits above the ladder rather than inside it so
 *     that an orchestrator NAMED as a row's assignee still cannot be charged
 *     to that row.
 *  1. a board row whose `assignee` is this agent's id;
 *  2. a board row whose `session` is this agent's session;
 *  3. the agent's stored brief names `#N`, and a board row carries that `#N`
 *     as its `issue` or `pr`;
 *  4. otherwise `UNATTRIBUTED`.
 *
 * Rungs 1–3 then walk `parent` to the feature (or epic, or root) that owns the
 * matched row — see `barRowFor`.
 *
 * Ties inside one rung are broken by BOARD ORDER (`board.find` scans the array
 * the caller passed), so the answer does not depend on `Map` iteration or on
 * which agent happened to be looked up first.
 */
export function attributeAgents(
  agents: readonly AgentRowLike[],
  board: readonly BoardRowLike[]
): Attribution {
  const byId = new Map<string, BoardRowLike>();
  for (const r of board) if (!byId.has(r.id)) byId.set(r.id, r);

  const byAgent = new Map<string, AgentAttribution>();
  // Bars are collected in first-seen order and re-ordered at the end, so a
  // feature's position is the BOARD's, not the agent list's.
  const featureBarsById = new Map<string, AttributionBucket>();
  const orchestratorAgents: string[] = [];
  const unattributedAgents: string[] = [];

  const place = (a: AgentRowLike): AgentAttribution => {
    if (isOrchestratorRole(a.role)) {
      return {
        agent: a.id,
        bucket: ORCHESTRATOR,
        label: ORCHESTRATOR,
        via: "orchestrator",
        matched: null,
        level: null,
      };
    }

    let matched: BoardRowLike | undefined;
    let via: AttributionVia = "none";

    matched = board.find((r) => nonEmpty(r.assignee) !== "" && nonEmpty(r.assignee) === a.id);
    if (matched) via = "assignee";

    if (!matched) {
      const session = nonEmpty(a.session);
      if (session) {
        matched = board.find((r) => nonEmpty(r.session) === session);
        if (matched) via = "session";
      }
    }

    if (!matched) {
      // Rung 3 reads the brief. The first `#N` a board row ACTUALLY CARRIES
      // wins — not the first `#N` in the text — because a brief routinely
      // cites issues it merely references ("per #2011's plan") alongside the
      // one it is working, and only the board can tell those apart.
      for (const ref of briefRefs(a.task)) {
        const hit = board.find((r) => refNumber(r.issue) === ref || refNumber(r.pr) === ref);
        if (hit) {
          matched = hit;
          via = "brief";
          break;
        }
      }
    }

    if (!matched) {
      return {
        agent: a.id,
        bucket: UNATTRIBUTED,
        label: UNATTRIBUTED,
        via: "none",
        matched: null,
        level: null,
      };
    }

    const bar = barRowFor(matched, byId);
    return {
      agent: a.id,
      bucket: bar.row.id,
      label: bar.row.title || bar.row.id,
      via,
      matched: matched.id,
      level: bar.level,
    };
  };

  for (const a of agents) {
    if (byAgent.has(a.id)) continue;
    const at = place(a);
    byAgent.set(a.id, at);
    if (at.bucket === ORCHESTRATOR) orchestratorAgents.push(a.id);
    else if (at.bucket === UNATTRIBUTED) unattributedAgents.push(a.id);
    else {
      const existing = featureBarsById.get(at.bucket);
      if (existing) existing.agents.push(a.id);
      else
        featureBarsById.set(at.bucket, {
          id: at.bucket,
          label: at.label,
          kind: "feature",
          agents: [a.id],
        });
    }
  }

  // Render order. `(unattributed)` FIRST and unconditionally — it is present
  // even with no agents on it, so the bar that says "this chart is not the
  // whole story" cannot disappear by being empty.
  const boardOrder = new Map<string, number>();
  board.forEach((r, i) => boardOrder.set(r.id, i));
  const features = [...featureBarsById.values()].sort(
    (a, b) =>
      (boardOrder.get(a.id) ?? Number.MAX_SAFE_INTEGER) -
      (boardOrder.get(b.id) ?? Number.MAX_SAFE_INTEGER)
  );

  return {
    byAgent,
    buckets: [
      { id: UNATTRIBUTED, label: UNATTRIBUTED, kind: "unattributed", agents: unattributedAgents },
      { id: ORCHESTRATOR, label: ORCHESTRATOR, kind: "orchestrator", agents: orchestratorAgents },
      ...features,
    ],
  };
}

// ── differencing ────────────────────────────────────────────────────────────

/** One interval's spend: the difference between two consecutive samples of ONE
 *  usage key, attributed to the LATER row — its agent, its block, its cli, its
 *  timestamp. That is the forward fix for last-occupant attribution:
 *  `usage.json` keeps the last agent on a key, the series keeps every one, and
 *  the spend lands on whoever was there when it was counted. */
export interface Delta {
  tsMs: number;
  usageKey: string;
  agent: string;
  block: string;
  cli: string;
  seriesKey: string;
  in: number;
  out: number;
  cache_w: number;
  cache_r: number;
  total: number;
  /** `null` when EITHER endpoint carried no figure — "no figure" and "cost
   *  nothing" are different answers (slice B's design note). */
  cost_usd: number | null;
  /** The cumulative counter went DOWN (a transcript cursor revalidation, a
   *  rotated transcript). Every component is clamped to 0 and the interval is
   *  labelled, rather than drawn as a cliff. */
  reset: boolean;
}

export interface DiffResult {
  deltas: Delta[];
  /** Usage keys whose whole history is one row — a baseline that yields no
   *  delta, so its lifetime-to-that-moment is deliberately NOT drawn. Counted
   *  so the view can say so rather than showing a shorter chart. */
  baselineOnlyKeys: number;
  /** Intervals the reset clamp had to zero. */
  resets: number;
}

/** Difference consecutive samples per usage key. The first row of each key is
 *  a BASELINE and yields nothing: its counters are that session's lifetime up
 *  to the moment sampling began, which may be an hour of spend the series
 *  never saw, and charging it to one bucket would draw a spike that did not
 *  happen (slice B's coverage note). */
export function diffRows(rows: readonly SeriesRowLike[]): DiffResult {
  const byKey = new Map<string, SeriesSampleLike[]>();
  for (const r of rows) {
    if (!isSample(r)) continue;
    const bucket = byKey.get(r.key);
    if (bucket) bucket.push(r);
    else byKey.set(r.key, [r]);
  }

  const deltas: Delta[] = [];
  let baselineOnlyKeys = 0;
  let resets = 0;

  for (const samples of byKey.values()) {
    // Stable sort by ts, preserving file order on a tie — the writer appends,
    // so file order IS the tie-break the file itself asserts.
    const ordered = samples
      .map((s, i) => ({ s, i }))
      .sort((a, b) => a.s.ts_ms - b.s.ts_ms || a.i - b.i)
      .map(({ s }) => s);
    if (ordered.length < 2) {
      baselineOnlyKeys++;
      continue;
    }
    for (let i = 1; i < ordered.length; i++) {
      const prev = ordered[i - 1];
      const cur = ordered[i];
      const raw = {
        in: num(cur.in) - num(prev.in),
        out: num(cur.out) - num(prev.out),
        cache_w: num(cur.cache_w) - num(prev.cache_w),
        cache_r: num(cur.cache_r) - num(prev.cache_r),
      };
      const reset = raw.in < 0 || raw.out < 0 || raw.cache_w < 0 || raw.cache_r < 0;
      if (reset) resets++;
      const d = {
        in: Math.max(0, raw.in),
        out: Math.max(0, raw.out),
        cache_w: Math.max(0, raw.cache_w),
        cache_r: Math.max(0, raw.cache_r),
      };
      const cost =
        typeof prev.cost_usd === "number" && typeof cur.cost_usd === "number"
          ? Math.max(0, cur.cost_usd - prev.cost_usd)
          : null;
      deltas.push({
        tsMs: cur.ts_ms,
        usageKey: cur.key,
        agent: cur.agent,
        block: labelOf(cur.block),
        cli: labelOf(cur.cli),
        seriesKey: seriesKeyOf(cur.block, cur.cli),
        ...d,
        total: d.in + d.out + d.cache_w + d.cache_r,
        cost_usd: cost,
        reset,
      });
    }
  }

  deltas.sort((a, b) => a.tsMs - b.tsMs);
  return { deltas, baselineOnlyKeys, resets };
}

// ── the series keys (the legend's identity lifetime) ────────────────────────

export interface SeriesKeyInfo {
  /** `block/cli`, or just `block` when the CLI split is collapsed. */
  key: string;
  block: string;
  /** `null` under the block toggle — the key then spans every CLI. */
  cli: string | null;
  /** Which categorical hue this key's BLOCK takes, as an index into
   *  `HUE_ORDER`, or `null` for "beyond the palette" — the neutral ramp.
   *
   *  Assigned per BLOCK from `blockOrder` where the caller supplies one, which
   *  is what makes the hue follow the ENTITY rather than its rank: a narrowed
   *  window or a CLI toggle changes which keys are on screen, and neither may
   *  repaint the survivors. Without a `blockOrder` the fallback is the sorted
   *  distinct blocks of the data itself, which is stable across the CLI toggle
   *  (the block set does not move) but not across a window change — so the
   *  view passes one. */
  hueIndex: number | null;
  /** How many deltas landed on this key — the "did the instrument see this"
   *  figure the view prints beside the legend. */
  samples: number;
  /** Tokens over the whole window. */
  total: number;
}

/**
 * The categorical sequence, as identity-hue NAMES the view maps to `--id-*`
 * tokens. Eight slots, mirroring `theme.ts`'s `IDENTITY` octet — the repo's
 * only categorical palette, and the one `test/theme.test.ts` already governs.
 *
 * **The ORDER is measured, not the declaration order.** A categorical
 * palette's separation check is over ADJACENT pairs, so the sequence is the
 * one lever a chart owns over a fixed design system. `IDENTITY`'s own order
 * puts `azure` next to `violet`, whose separation is ΔE 0.4 (protan) and 5.7
 * normal-vision — a pair a full-colour reader cannot tell apart. This order is
 * the exhaustive-search best over all 8! permutations: worst adjacent pair ΔE
 * 12.0 (deutan) / 11.5 (tritan) / 20.4 normal, which clears the ≥8 CVD and ≥15
 * normal-vision floors. Figures and the search are in the PR body's agent
 * layer; the residual — `azure`/`violet` are still an all-pairs collision, so
 * two blocks four slots apart can collide — is why the chart never relies on
 * colour alone (legend always present, CLI carried by line style, ≤4 keys
 * direct-labelled).
 *
 * Changing this order is a measurement, not a preference: re-run the search.
 */
export const HUE_ORDER = [
  "jade",
  "violet",
  "amber",
  "cyan",
  "rose",
  "azure",
  "lime",
  "orchid",
] as const;

/** How many blocks get an identity hue. A NINTH block does not recycle slot 0
 *  — a repeated hue is a false claim of identity — it takes `hueIndex: null`,
 *  the neutral ramp, which says "beyond the palette" honestly while keeping
 *  its own line, its own dash and its own legend row. Nothing is merged away:
 *  this is a cost chart, and folding two blocks' spend together to save a
 *  colour would be the worse trade. */
export const HUE_SLOTS = HUE_ORDER.length;

/** Block -> hue slot, or absent for "beyond the palette".
 *
 *  `blockOrder` is the caller's STABLE list — the group's whole roster, not the
 *  windowed data — and it is what stops a hue moving when a filter changes
 *  which blocks are on screen. It is deduplicated and only its first
 *  `HUE_SLOTS` entries get a hue; any block outside it is appended after, so a
 *  block that appears in the data but not the roster is still drawable. */
function hueAssignment(
  deltas: readonly Delta[],
  blockOrder: readonly string[] | undefined
): Map<string, number> {
  const ordered: string[] = [];
  const seen = new Set<string>();
  for (const b of blockOrder ?? []) {
    if (seen.has(b)) continue;
    seen.add(b);
    ordered.push(b);
  }
  // Blocks the caller's order did not name, sorted so the fallback is
  // deterministic rather than dependent on row arrival order.
  for (const b of [...new Set(deltas.map((d) => d.block))].sort()) {
    if (seen.has(b)) continue;
    seen.add(b);
    ordered.push(b);
  }
  const out = new Map<string, number>();
  ordered.forEach((b, i) => {
    if (i < HUE_SLOTS) out.set(b, i);
  });
  return out;
}

/**
 * The chart's key axis, **read off the rows** rather than from any roster
 * list. A block this build has never heard of, an `opencode` row beside a `pi`
 * row of the same block, a row from before slice B's fields existed: each gets
 * its own line, because the only list that can be right is the data's.
 *
 * `collapseCli` is the per-block toggle: `worker-std/opencode` and
 * `worker-std/pi` become one `worker-std` key whose totals are their sum. It
 * is a REGROUPING, never a filter — the same tokens stay on screen.
 */
export function seriesKeys(
  rows: readonly SeriesRowLike[],
  opts: { collapseCli?: boolean; blockOrder?: readonly string[] } = {}
): SeriesKeyInfo[] {
  const { deltas } = diffRows(rows);
  const collapse = opts.collapseCli === true;
  const hueOf = hueAssignment(deltas, opts.blockOrder);

  const acc = new Map<string, SeriesKeyInfo>();
  for (const d of deltas) {
    const key = collapse ? d.block : d.seriesKey;
    const existing = acc.get(key);
    if (existing) {
      existing.samples++;
      existing.total += d.total;
      continue;
    }
    acc.set(key, {
      key,
      block: d.block,
      cli: collapse ? null : d.cli,
      hueIndex: hueOf.get(d.block) ?? null,
      samples: 1,
      total: d.total,
    });
  }
  // Sorted by key so the legend, the polylines and the bar stacks can never
  // disagree about position, and so two runs over the same data render alike.
  return [...acc.values()].sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));
}

// ── the time plot ───────────────────────────────────────────────────────────

export interface SeriesPoint {
  /** The bucket's START instant, on the grid. */
  tsMs: number;
  in: number;
  out: number;
  cache_w: number;
  cache_r: number;
  total: number;
  /** `null` when any delta in this bucket carried no figure — a partial sum
   *  would read as a smaller bill rather than an unknown one. */
  cost_usd: number | null;
  /** This bucket contains at least one clamped (reset) interval. */
  reset: boolean;
  /** Deltas that landed in this bucket. `0` is what makes a gap a zero rather
   *  than a slope — the point EXISTS and its value is nothing. */
  n: number;
}

export interface KeySeries extends SeriesKeyInfo {
  /** One point per bucket across the whole window, dense. */
  points: SeriesPoint[];
}

export interface BucketedSeries {
  keys: KeySeries[];
  /** The grid, as start instants — every key's `points` is index-aligned to
   *  this, so the view can zip them without re-deriving anything. */
  buckets: number[];
  bucketMs: number;
  startMs: number;
  endMs: number;
  /** Deltas outside [startMs, endMs]. Counted, never clamped onto an edge,
   *  where they would read as spend at a time it did not happen. */
  dropped: number;
  baselineOnlyKeys: number;
  resets: number;
}

/** A hard ceiling on grid points, so a pathological window (a clock
 *  correction, a hand-typed `sinceMs`) cannot build an unbounded array on the
 *  render path. Ten thousand five-minute buckets is ~35 days. */
const MAX_BUCKETS = 100_000;

/**
 * Cumulative rows -> per-key deltas -> a DENSE bucket grid over the window.
 *
 * Density is the point. A key that wrote nothing for an hour gets twelve
 * zero-valued points, so the polyline runs along the floor and comes back up.
 * The sparse alternative joins the two real samples with one straight segment,
 * which draws an hour of steady spend that never happened.
 */
export function bucketSeries(
  rows: readonly SeriesRowLike[],
  opts: {
    startMs: number;
    endMs: number;
    bucketMs?: number;
    collapseCli?: boolean;
    /** The caller's stable block list — see `hueAssignment`. */
    blockOrder?: readonly string[];
  }
): BucketedSeries {
  const bucketMs = Math.max(1, Math.floor(opts.bucketMs ?? DEFAULT_BUCKET_MS));
  const collapse = opts.collapseCli === true;
  const { deltas, baselineOnlyKeys, resets } = diffRows(rows);

  const first = Math.floor(opts.startMs / bucketMs) * bucketMs;
  const last = Math.floor(opts.endMs / bucketMs) * bucketMs;
  const buckets: number[] = [];
  // A degenerate or inverted window yields NO buckets rather than a runaway
  // loop — a real state while a divider is dragged or a clock is corrected.
  if (Number.isFinite(first) && Number.isFinite(last)) {
    for (let t = first; t <= last && buckets.length < MAX_BUCKETS; t += bucketMs) buckets.push(t);
  }
  const indexOf = new Map(buckets.map((t, i) => [t, i]));

  const infos = seriesKeys(rows, { collapseCli: collapse, blockOrder: opts.blockOrder });
  const series = new Map<string, KeySeries>();
  for (const info of infos) {
    series.set(info.key, {
      ...info,
      points: buckets.map((tsMs) => ({
        tsMs,
        in: 0,
        out: 0,
        cache_w: 0,
        cache_r: 0,
        total: 0,
        cost_usd: 0,
        reset: false,
        n: 0,
      })),
    });
  }

  let dropped = 0;
  for (const d of deltas) {
    if (d.tsMs < opts.startMs || d.tsMs > opts.endMs) {
      dropped++;
      continue;
    }
    const idx = indexOf.get(Math.floor(d.tsMs / bucketMs) * bucketMs);
    if (idx === undefined) {
      dropped++;
      continue;
    }
    const target = series.get(collapse ? d.block : d.seriesKey);
    if (!target) continue;
    const p = target.points[idx];
    p.in += d.in;
    p.out += d.out;
    p.cache_w += d.cache_w;
    p.cache_r += d.cache_r;
    p.total += d.total;
    p.n++;
    if (d.reset) p.reset = true;
    // One unknown poisons the bucket's cost, deliberately: summing the known
    // half would print a smaller bill, which is a WRONG number rather than a
    // missing one.
    if (d.cost_usd === null) p.cost_usd = null;
    else if (p.cost_usd !== null) p.cost_usd += d.cost_usd;
  }

  return {
    keys: [...series.values()],
    buckets,
    bucketMs,
    startMs: opts.startMs,
    endMs: opts.endMs,
    dropped,
    baselineOnlyKeys,
    resets,
  };
}

// ── the marks ───────────────────────────────────────────────────────────────

export interface RosterChange {
  block: string;
  from: string;
  to: string;
}

export interface ChartMark {
  tsMs: number;
  /** Fingerprint components that moved, as slice B recorded them. */
  changed: string[];
  /** The blocks whose CLI actually changed across this mark, measured from the
   *  SAMPLES either side. Empty on most marks — a skills edit or a CLAUDE.md
   *  edit moves no CLI. */
  roster: RosterChange[];
  /** The label the vertical carries: the roster diff where there is one
   *  (`worker-std: opencode → pi`), else the component list. */
  label: string;
  /** The fingerprint walk hit a cap, so an UNCHANGED component is not proof
   *  that nothing under it moved. Carried through so the view can say so. */
  fpPartial: boolean;
}

/**
 * The labelled verticals.
 *
 * A `mark` row carries sha256 hashes and component NAMES, never content, so
 * "what actually changed" cannot be read out of it. What CAN be read is what
 * the fleet then ran: the CLI each block was sampled with immediately before
 * the mark, against the CLI it was sampled with immediately after. That is a
 * MEASUREMENT of the switch rather than a guess at it — and it is exactly the
 * #2817 roster change the human asked to see labelled.
 *
 * The cost of measuring it this way is stated rather than hidden: a block that
 * did not run on one side of the mark contributes no row, because "not
 * observed" is not "unchanged".
 */
export function marks(rows: readonly SeriesRowLike[]): ChartMark[] {
  const samples = rows
    .filter(isSample)
    .slice()
    .sort((a, b) => a.ts_ms - b.ts_ms);
  const markRows = rows
    .filter(isMark)
    .slice()
    .sort((a, b) => a.ts_ms - b.ts_ms);

  return markRows.map((m) => {
    // Last CLI observed per block strictly BEFORE the mark; first observed
    // at-or-after it.
    const before = new Map<string, string>();
    const after = new Map<string, string>();
    for (const s of samples) {
      const block = labelOf(s.block);
      const cli = labelOf(s.cli);
      if (s.ts_ms < m.ts_ms) before.set(block, cli);
      else if (!after.has(block)) after.set(block, cli);
    }
    const roster: RosterChange[] = [];
    for (const [block, from] of before) {
      const to = after.get(block);
      if (to !== undefined && to !== from) roster.push({ block, from, to });
    }
    roster.sort((a, b) => (a.block < b.block ? -1 : a.block > b.block ? 1 : 0));

    const changed = [...(m.changed ?? [])];
    const label =
      roster.length > 0
        ? roster.map((r) => `${r.block}: ${r.from} → ${r.to}`).join("; ")
        : changed.length > 0
          ? `${changed.join(", ")} changed`
          : "tuning changed";

    return { tsMs: m.ts_ms, changed, roster, label, fpPartial: m.fp_partial === true };
  });
}

// ── the before/after readout ────────────────────────────────────────────────

export interface BeforeAfterRow {
  key: string;
  /** Mean of the `k` bucket values immediately BEFORE the mark, or `null` when
   *  fewer than `k` buckets exist on that side — never `0`, which would read
   *  as "we measured, and it was nothing". */
  before: number | null;
  after: number | null;
  delta: number | null;
  /** `after/before - 1`, or `null` where either side is null OR `before` is 0
   *  (a ratio against nothing is not a percentage). */
  pct: number | null;
  /** The half-width this row was computed at — the scope of its claim, carried
   *  on the row so it travels with the number. */
  k: number;
}

/** The synthetic row every readout carries: every key summed, so the headline
 *  number is the fleet's, not one lane's. */
export const TOTAL_ROW = "(all keys)";

/**
 * Mean of the `k` bucket values either side of a mark, per key and in total.
 *
 * **`null` below `k`, never `0`.** A mark two buckets after the series began
 * has no "before" — and a chart that prints `0` there says the fleet spent
 * nothing for an hour, which is the opposite of "we cannot say".
 *
 * `cost_usd` is null-poisoned the same way `bucketSeries` poisons a bucket: a
 * single unknown on either side makes that side null, because averaging over a
 * hole prints a number smaller than the truth.
 */
export function beforeAfter(
  series: BucketedSeries,
  markTsMs: number,
  metric: Metric,
  k: number = DEFAULT_BEFORE_AFTER_K
): BeforeAfterRow[] {
  const width = Math.max(1, Math.floor(k));
  const idx = series.buckets.findIndex((t) => t >= markTsMs);
  // A mark past the window's end puts every bucket on the "before" side.
  const split = idx === -1 ? series.buckets.length : idx;

  const valueOf = (p: SeriesPoint): number | null =>
    metric === "cost_usd" ? p.cost_usd : p[metric];

  /** Mean of the `width` buckets ENDING at `hi` — the ones nearest the mark. */
  const meanBefore = (points: readonly SeriesPoint[], hi: number): number | null => {
    if (hi < width) return null;
    let sum = 0;
    for (let i = hi - width; i < hi; i++) {
      const v = valueOf(points[i]);
      if (v === null) return null;
      sum += v;
    }
    return sum / width;
  };
  /** Mean of the `width` buckets STARTING at `lo` — again, nearest the mark. */
  const meanAfter = (points: readonly SeriesPoint[], lo: number): number | null => {
    if (points.length - lo < width) return null;
    let sum = 0;
    for (let i = lo; i < lo + width; i++) {
      const v = valueOf(points[i]);
      if (v === null) return null;
      sum += v;
    }
    return sum / width;
  };

  const rowFor = (key: string, points: readonly SeriesPoint[]): BeforeAfterRow => {
    const before = meanBefore(points, split);
    const after = meanAfter(points, split);
    const delta = before === null || after === null ? null : after - before;
    const pct = before === null || after === null || before === 0 ? null : after / before - 1;
    return { key, before, after, delta, pct, k: width };
  };

  // The total row is summed POINTWISE across keys, so its own before/after is
  // a mean of real bucket totals rather than a mean of means — which would
  // weight a key with no data as though it had some.
  const totals: SeriesPoint[] = series.buckets.map((tsMs, i) => {
    let costNull = false;
    let cost = 0;
    const acc: SeriesPoint = {
      tsMs,
      in: 0,
      out: 0,
      cache_w: 0,
      cache_r: 0,
      total: 0,
      cost_usd: 0,
      reset: false,
      n: 0,
    };
    for (const s of series.keys) {
      const p = s.points[i];
      acc.in += p.in;
      acc.out += p.out;
      acc.cache_w += p.cache_w;
      acc.cache_r += p.cache_r;
      acc.total += p.total;
      acc.n += p.n;
      acc.reset = acc.reset || p.reset;
      if (p.cost_usd === null) costNull = true;
      else cost += p.cost_usd;
    }
    acc.cost_usd = costNull ? null : cost;
    return acc;
  });

  return [rowFor(TOTAL_ROW, totals), ...series.keys.map((s) => rowFor(s.key, s.points))];
}

// ── the feature bars ────────────────────────────────────────────────────────

export interface BarSegment {
  /** `block/cli` (or `block` under the toggle) — the same key the plot draws,
   *  so a colour means the same thing in both charts. */
  key: string;
  block: string;
  cli: string | null;
  hueIndex: number | null;
  tokens: number;
  /** `null` where any contributing delta carried no figure. */
  costUsd: number | null;
}

export interface FeatureBar {
  id: string;
  label: string;
  kind: "feature" | "orchestrator" | "unattributed";
  total: number;
  costUsd: number | null;
  /** How the cost figure was arrived at. A chart that prints dollars without
   *  saying which of them are estimates claims a precision it has not got. */
  costLabel: "reported" | "estimated" | "mixed" | "none";
  /** Stack order matches `seriesKeys` order, so every bar stacks identically
   *  and two bars can be compared segment by segment. */
  segments: BarSegment[];
  /** Agent ids whose spend is on this bar, first-seen order — the tooltip. */
  agents: string[];
}

export interface FeatureBars {
  bars: FeatureBar[];
  /** The legend's three numbers, plus the total they must sum to. The identity
   *  `features + orchestrator + unattributed === total` is what makes the
   *  chart checkable against the group panel's own lifetime figure. */
  lifetime: { features: number; orchestrator: number; unattributed: number; total: number };
  attribution: Attribution;
  /** Tokens on deltas whose agent is on NO roster row at all — folded into
   *  `(unattributed)` and counted separately, because "the roster does not know
   *  this agent" is a different fact from "the board does not". The roster is
   *  group-wide and includes exited agents, so a miss here is a real hole. */
  unknownAgentTokens: number;
}

/**
 * Stacked bars: one per feature, plus the two group-wide bars.
 *
 * The spend on a delta goes to the agent that WROTE the later of its two rows,
 * not to whoever holds the usage key now — the series is what makes that
 * possible, and it is the forward fix for the scorecard's last-occupant
 * attribution.
 */
export function featureBars(
  rows: readonly SeriesRowLike[],
  agents: readonly AgentRowLike[],
  board: readonly BoardRowLike[],
  opts: {
    collapseCli?: boolean;
    startMs?: number;
    endMs?: number;
    /** The caller's stable block list — see `hueAssignment`. */
    blockOrder?: readonly string[];
  } = {}
): FeatureBars {
  const collapse = opts.collapseCli === true;
  const attribution = attributeAgents(agents, board);
  const infos = seriesKeys(rows, { collapseCli: collapse, blockOrder: opts.blockOrder });
  const infoOf = new Map(infos.map((i) => [i.key, i]));
  const { deltas } = diffRows(rows);

  const emptySegments = (): BarSegment[] =>
    infos.map((i) => ({
      key: i.key,
      block: i.block,
      cli: i.cli,
      hueIndex: i.hueIndex,
      tokens: 0,
      costUsd: 0,
    }));

  const bars = new Map<string, FeatureBar>();
  const barAgents = new Map<string, string[]>();
  /** Per-bar provenance flags for `costLabel`. */
  const flags = new Map<string, { reported: boolean; estimated: boolean }>();
  for (const b of attribution.buckets) {
    bars.set(b.id, {
      id: b.id,
      label: b.label,
      kind: b.kind,
      total: 0,
      costUsd: 0,
      costLabel: "none",
      segments: emptySegments(),
      agents: [],
    });
    barAgents.set(b.id, []);
    flags.set(b.id, { reported: false, estimated: false });
  }

  // `estimated` is a property of the LATER sample of an interval, so it is
  // looked up by that row's identity rather than re-derived.
  const estimatedByRow = new Map<string, boolean>();
  for (const r of rows) if (isSample(r)) estimatedByRow.set(`${r.key}|${r.ts_ms}`, r.estimated === true);

  let unknownAgentTokens = 0;

  for (const d of deltas) {
    if (opts.startMs !== undefined && d.tsMs < opts.startMs) continue;
    if (opts.endMs !== undefined && d.tsMs > opts.endMs) continue;

    const at = attribution.byAgent.get(d.agent);
    if (!at) unknownAgentTokens += d.total;
    const barId = at ? at.bucket : UNATTRIBUTED;
    const bar = bars.get(barId);
    if (!bar) continue;

    const segKey = collapse ? d.block : d.seriesKey;
    let seg = bar.segments.find((s) => s.key === segKey);
    if (!seg) {
      const info = infoOf.get(segKey);
      seg = {
        key: segKey,
        block: d.block,
        cli: collapse ? null : d.cli,
        hueIndex: info?.hueIndex ?? null,
        tokens: 0,
        costUsd: 0,
      };
      bar.segments.push(seg);
    }
    seg.tokens += d.total;
    bar.total += d.total;

    if (d.cost_usd === null) {
      seg.costUsd = null;
      bar.costUsd = null;
    } else {
      if (seg.costUsd !== null) seg.costUsd += d.cost_usd;
      if (bar.costUsd !== null) bar.costUsd += d.cost_usd;
    }

    const f = flags.get(barId)!;
    if (estimatedByRow.get(`${d.usageKey}|${d.tsMs}`) === true) f.estimated = true;
    else f.reported = true;

    const list = barAgents.get(barId)!;
    if (d.agent && !list.includes(d.agent)) list.push(d.agent);
  }

  for (const [id, bar] of bars) {
    const f = flags.get(id)!;
    bar.costLabel =
      !f.reported && !f.estimated
        ? "none"
        : f.reported && f.estimated
          ? "mixed"
          : f.estimated
            ? "estimated"
            : "reported";
    bar.agents = barAgents.get(id)!;
  }

  const ordered = attribution.buckets.map((b) => bars.get(b.id)!);
  const sum = (kind: FeatureBar["kind"]): number =>
    ordered.filter((b) => b.kind === kind).reduce((a, b) => a + b.total, 0);
  const features = sum("feature");
  const orchestrator = sum("orchestrator");
  const unattributed = sum("unattributed");

  return {
    bars: ordered,
    lifetime: {
      features,
      orchestrator,
      unattributed,
      total: features + orchestrator + unattributed,
    },
    attribution,
    unknownAgentTokens,
  };
}

/** The instant each bar's earliest in-window spend was observed — the input
 *  `scorecardColumns` compares against the audit coverage floor. Derived here
 *  rather than in the view so the "is this bar older than the log" answer is
 *  computed from the same deltas the bar itself is. */
export function firstSpendByBar(
  rows: readonly SeriesRowLike[],
  attribution: Attribution
): Map<string, number> {
  const out = new Map<string, number>();
  for (const d of diffRows(rows).deltas) {
    const barId = attribution.byAgent.get(d.agent)?.bucket ?? UNATTRIBUTED;
    const prev = out.get(barId);
    if (prev === undefined || d.tsMs < prev) out.set(barId, d.tsMs);
  }
  return out;
}

// ── the scorecard columns ───────────────────────────────────────────────────

export interface ScorecardColumn {
  featureId: string;
  label: string;
  /** `review-verdict` rows whose PR maps to this bar. */
  rounds: number;
  /** `fail / (pass + fail)` over those rows, or `null` where there were none —
   *  never `0`, which would read as a perfect record. */
  failRate: number | null;
  /** Turns the DRIVER took (`rd-lane-spawned` / `rd-handback`) versus prompts
   *  delivered to an orchestrator pane. */
  driverTurns: number;
  orchTurns: number;
  /** This bar has spend older than the audit window's floor, so its counts are
   *  a lower bound. Greyed by the view rather than presented as fact. */
  belowFloor: boolean;
}

export interface Scorecard {
  columns: ScorecardColumn[];
  /** The oldest audit row's instant — the read's coverage floor. `null` when
   *  no rows were read at all, which is "we have not looked" and NOT "there is
   *  no history". */
  floorMs: number | null;
  /** Audit rows read. Surfaced so a reader can tell a genuinely quiet group
   *  from a scan that matched nothing. */
  auditRows: number;
}

/** Verdict spellings that count as a FAIL. Enumerated rather than "anything
 *  that is not a pass", so a verdict vocabulary this build has not heard of
 *  lands in neither column instead of silently inflating the fail rate. */
const FAIL_VERDICTS = new Set(["fail", "changes", "request-changes", "changes-requested"]);

/**
 * The columns beside the bars, from the pane's own `AuditStore` read.
 *
 * **The floor is the point.** `orch_audit` returns at most `AUDIT_VIEW_LIMIT`
 * rows across two generations of a rotating log, so these counts are a lower
 * bound over whatever window that happened to be. A bar whose first spend
 * predates the oldest row read is flagged `belowFloor` — the alternative is a
 * chart that presents "2 review rounds" for a feature that had eleven, with
 * nothing on screen to say the log had rotated.
 */
export function scorecardColumns(
  auditRows: readonly AuditRowLike[],
  bars: FeatureBars,
  board: readonly BoardRowLike[],
  opts: { firstSpendMs?: ReadonlyMap<string, number> } = {}
): Scorecard {
  // `Math.min(...)` over a large array is a spread onto the call stack, so the
  // floor is folded instead — `orch_audit` returns up to 5000 rows.
  let floorMs: number | null = null;
  for (const r of auditRows) if (floorMs === null || r.ts_ms < floorMs) floorMs = r.ts_ms;

  // PR number -> the bar it rolls up to, via the board row carrying it.
  const byId = new Map<string, BoardRowLike>();
  for (const r of board) if (!byId.has(r.id)) byId.set(r.id, r);
  const barOfPr = new Map<string, string>();
  for (const r of board) {
    const pr = refNumber(r.pr);
    if (!pr || barOfPr.has(pr)) continue;
    barOfPr.set(pr, barRowFor(r, byId).row.id);
  }

  const columns = new Map<string, ScorecardColumn>();
  for (const bar of bars.bars) {
    columns.set(bar.id, {
      featureId: bar.id,
      label: bar.label,
      rounds: 0,
      failRate: null,
      driverTurns: 0,
      orchTurns: 0,
      belowFloor: false,
    });
  }
  const verdicts = new Map<string, { pass: number; fail: number }>();

  for (const row of auditRows) {
    const detail =
      row.detail && typeof row.detail === "object" && !Array.isArray(row.detail)
        ? (row.detail as Record<string, unknown>)
        : null;
    switch (row.action) {
      case "review-verdict": {
        const raw = detail?.pr;
        const pr = refNumber(typeof raw === "string" ? raw : typeof raw === "number" ? String(raw) : null);
        const barId = pr === null ? undefined : barOfPr.get(pr);
        if (barId === undefined) continue;
        const col = columns.get(barId);
        if (!col) continue;
        col.rounds++;
        const v = verdicts.get(barId) ?? { pass: 0, fail: 0 };
        const verdict = typeof detail?.verdict === "string" ? detail.verdict.toLowerCase() : "";
        if (verdict === "pass") v.pass++;
        else if (FAIL_VERDICTS.has(verdict)) v.fail++;
        verdicts.set(barId, v);
        continue;
      }
      case "rd-lane-spawned":
      case "rd-handback": {
        // Driver turns are group-wide: the driver does not act "on a feature",
        // so this lands on the orchestrator bar rather than being spread over
        // features it cannot honestly be traced to.
        const col = columns.get(ORCHESTRATOR);
        if (col) col.driverTurns++;
        continue;
      }
      case "prompt": {
        if (/^orch/i.test(row.actor)) {
          const col = columns.get(ORCHESTRATOR);
          if (col) col.orchTurns++;
        }
        continue;
      }
      default:
        continue;
    }
  }

  for (const [barId, v] of verdicts) {
    const col = columns.get(barId);
    if (!col) continue;
    const n = v.pass + v.fail;
    col.failRate = n === 0 ? null : v.fail / n;
  }

  if (floorMs !== null && opts.firstSpendMs) {
    for (const [barId, ts] of opts.firstSpendMs) {
      const col = columns.get(barId);
      if (col && ts < floorMs) col.belowFloor = true;
    }
  }

  return {
    columns: bars.bars.map((b) => columns.get(b.id)!),
    floorMs,
    auditRows: auditRows.length,
  };
}
