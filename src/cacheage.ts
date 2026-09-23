// The pane cache-age timer's model (#3407) — DOM-free, so `test/cacheage.test.ts`
// pins every rung with literals (CLAUDE.md: frontend logic that needs tests is
// extracted into a pure module; the header chip and the Agents-tab cell in
// `pane.ts` / `agentsview.ts` are thin wiring over this).
//
// WHAT THE CHIP CLAIMS, AND WHAT IT CANNOT. A provider's prompt cache is not
// observable. What the backend observes is when this pane's usage counters last
// moved — its last model request, landed — and it resolves the TTL the
// provider documents for this CLI (or the block's `cache_ttl_minutes:`
// override). `hot` / `cooling` / `cold` is an INFERENCE from those two numbers,
// and the tooltip says so. It errs toward hot by the length of the last
// response, because the provider's clock starts when a request STARTS and the
// counters land when it ENDS; `docs/design/cache-age.md` carries the argument.
//
// NO NEW POLL. The readings arrive on the tab strip's existing snapshot read
// (`orch_strip_view`, every few seconds) as fields on each usage row — the same
// subscription `rosteridle.ts` rides — and the label is re-derived against the
// clock whenever a reading is delivered. No timer of this module's own.
//
// NO IMPORT FROM `orchestration.ts`, for `rosteridle.ts`'s reason: the shapes
// below are structural, so `StripViewPayload` satisfies them without this
// module reaching a file whose imports include the Tauri IPC seam.

/** What the first request(s) after a quiet stretch cost — the backend's
 *  `cacheage::WakeCost`, as the usage row carries it. */
export interface WakeCostReading {
  readonly at_ms: number;
  /** How long the pane had been quiet before the wake; null when unknown. */
  readonly idle_before_ms: number | null;
  readonly input_tokens: number;
  readonly output_tokens: number;
  readonly cache_creation_tokens: number;
  readonly cache_read_tokens: number;
  readonly cost_usd: number | null;
}

/** One agent's cache-age inputs, as the usage row carries them. */
export interface CacheAgeReading {
  /** Unix-ms the counters last moved, or null when never observed. */
  readonly lastActiveMs: number | null;
  /** The TTL in force (block override, else the CLI's), or null = unknown. */
  readonly ttlMinutes: number | null;
  /** Idle ms after which the pane reads cooling; null when the TTL is unknown. */
  readonly coolingAfterMs: number | null;
  readonly lastWake: WakeCostReading | null;
  /** Whether "Compact now" can do anything on this pane's CLI. */
  readonly compactSupported: boolean;
}

export type CacheState = "hot" | "cooling" | "cold" | "unknown";

/** The usage row fields this module reads — structural, see the header. */
export interface CacheUsageRow {
  readonly id: string;
  readonly last_active_ms?: number | null;
  readonly cache_ttl_minutes?: number | null;
  readonly cache_cooling_after_ms?: number | null;
  readonly last_wake?: WakeCostReading | null;
  readonly compact_supported?: boolean;
}

export interface CacheStripReading {
  readonly groups: Record<
    string,
    { readonly usage: { readonly live_agents: readonly CacheUsageRow[] } | null } | undefined
  >;
}

/** This pane's reading out of the strip snapshot, or `null` when the strip does
 *  not cover it — no orchestration identity, a group the strip did not carry or
 *  refused, or an agent with no usage row. Null is "nothing to show", never a
 *  zero age. */
export function cacheAgeFor(
  strip: CacheStripReading,
  group: string | null,
  agentId: string | null
): CacheAgeReading | null {
  if (group === null || agentId === null) return null;
  const rows = strip.groups[group]?.usage?.live_agents;
  if (rows === undefined) return null;
  const row = rows.find((r) => r.id === agentId);
  if (row === undefined) return null;
  return {
    lastActiveMs: row.last_active_ms ?? null,
    ttlMinutes: row.cache_ttl_minutes ?? null,
    coolingAfterMs: row.cache_cooling_after_ms ?? null,
    lastWake: row.last_wake ?? null,
    compactSupported: row.compact_supported === true,
  };
}

/** The inferred state at `nowMs`, with the age it was judged on.
 *
 *  `unknown` covers both halves of "cannot say": no request observed yet
 *  (`ageMs` null), and a request observed on a CLI with no known TTL (`ageMs`
 *  set, so the chip can still show how long the pane has been quiet). A clock
 *  that reads earlier than the last request (skew between the reading and this
 *  window's clock) is age 0, never negative. */
export function cacheState(r: CacheAgeReading, nowMs: number): { state: CacheState; ageMs: number | null } {
  if (r.lastActiveMs === null) return { state: "unknown", ageMs: null };
  const ageMs = Math.max(0, nowMs - r.lastActiveMs);
  if (r.ttlMinutes === null || r.ttlMinutes <= 0) return { state: "unknown", ageMs };
  const ttlMs = r.ttlMinutes * 60_000;
  if (ageMs >= ttlMs) return { state: "cold", ageMs };
  // `coolingAfterMs` is the backend's own threshold (the SAME one the
  // orchestrator backstop fires on); a row without it cools at the TTL.
  const coolAt = r.coolingAfterMs ?? ttlMs;
  return { state: ageMs >= coolAt ? "cooling" : "hot", ageMs };
}

/** A compact duration: `45s`, `3m`, `1h 5m`, `2h`. Minutes are FLOORED, so a
 *  label never claims more idle time than has passed. */
export function formatAge(ms: number): string {
  const s = Math.floor(Math.max(0, ms) / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  const rest = m % 60;
  return rest === 0 ? `${h}h` : `${h}h ${rest}m`;
}

/** The chip's text, or `null` for no chip at all (nothing observed yet).
 *
 *  `hot 3m` → `cooling 48m/60m` → `cold`, and `idle 12m` where the TTL is
 *  unknown — an age with no claim about the cache. */
export function cacheChipLabel(r: CacheAgeReading, nowMs: number): string | null {
  const { state, ageMs } = cacheState(r, nowMs);
  if (ageMs === null) return null;
  switch (state) {
    case "hot":
      return `hot ${formatAge(ageMs)}`;
    case "cooling":
      // The TTL as the table and the workflow key spell it — in minutes — so
      // the denominator reads as the setting it came from (`48m/60m`).
      return `cooling ${formatAge(ageMs)}/${r.ttlMinutes}m`;
    case "cold":
      return "cold";
    case "unknown":
      return `idle ${formatAge(ageMs)}`;
  }
}

/** Token counts the way the rest of the app writes them: `800`, `3.4k`, `1.2M`. */
export function formatTokens(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${trim1(n / 1000)}k`;
  return `${trim1(n / 1_000_000)}M`;
}

function trim1(x: number): string {
  const s = x.toFixed(1);
  return s.endsWith(".0") ? s.slice(0, -2) : s;
}

/** One line describing what the last wake cost, split the way that shows what
 *  a cold cache costs: tokens READ from the cache (cheap), tokens WRITTEN to it
 *  (what a cold wake pays to rebuild the prefix), and fresh uncached input.
 *  Null when there is no wake on record. */
export function wakeCostLine(w: WakeCostReading | null): string | null {
  if (w === null) return null;
  const parts = [
    `${formatTokens(w.cache_read_tokens)} cache-read`,
    `${formatTokens(w.cache_creation_tokens)} cache-written`,
    `${formatTokens(w.input_tokens)} uncached`,
  ];
  if (w.cost_usd !== null) parts.push(`~$${w.cost_usd.toFixed(2)}`);
  const after = w.idle_before_ms === null ? "" : ` after ${formatAge(w.idle_before_ms)} idle`;
  return `Last wake${after}: ${parts.join(" · ")}`;
}

/** The chip's tooltip: what it measured, what it inferred, and the honest
 *  limit — plus the last wake's cost, when there is one. `hint` is the last
 *  line: what the surface showing it lets the human do about it. */
export function cacheChipTitle(
  r: CacheAgeReading,
  nowMs: number,
  hint: string = "Click for Compact now."
): string {
  const { state, ageMs } = cacheState(r, nowMs);
  const lines: string[] = [];
  if (ageMs !== null) lines.push(`Last model request ${formatAge(ageMs)} ago.`);
  if (r.ttlMinutes !== null && r.ttlMinutes > 0) {
    lines.push(
      `Prompt cache ${state} — inferred from a ${r.ttlMinutes}m TTL, not observed. ` +
        "A request after the TTL re-reads the whole context uncached."
    );
  } else {
    lines.push("No cache TTL is known for this pane, so no hot/cold state is inferred.");
  }
  const wake = wakeCostLine(r.lastWake);
  if (wake !== null) lines.push(wake);
  lines.push(hint);
  return lines.join("\n");
}
