// What the next prompt will cost (#3831) — DOM-free, so `test/promptcost.test.ts`
// pins every figure with literals. The cache chip's menu (`panebadges.ts`) and
// the compose strip's line (`panecompose.ts`) are thin wiring over this.
//
// THE QUESTION IT ANSWERS. A human about to type into a pane with a large
// context wants to know whether to send now, or whether the cache has gone and
// a fresh agent would be cheaper. So it prices the INPUT side of the next
// request three ways and puts them side by side:
//
//   warm   the context is read from the prompt cache, the typed prompt written
//   cold   everything is written to the cache again
//   fresh  the same prompt in a new agent: this session's first-turn context
//          plus the typed prompt, all written
//
// "Now" is whichever of warm and cold the chip's inferred state says applies.
//
// WHAT IT CANNOT KNOW, each said on the surface that shows the figure:
//   - the OUTPUT. Nothing here prices a reply nobody has written yet.
//   - how many requests the turn makes. A turn with N tool calls reads the
//     cache about N times; this is one request.
//   - whether the cache is really warm. That is the chip's inference, and it
//     errs toward hot (`docs/design/cache-age.md`).
//   - the typed prompt's token count. There is no tokenizer here (Claude's is
//     not public), so it is the text's length over a characters-per-token
//     figure the BACKEND resolves for the model. Where the backend has none,
//     the typed prompt is left out and the surface says so, rather than
//     counted with a constant nobody sourced.
//   - what a subscription pays. Dollars are the vendor's LIST price; tokens
//     lead on every surface because they are true for every account.
//
// NOTHING IS A ZERO THAT MEANS UNKNOWN. A missing context, first-turn reading
// or price is `null` and stays `null` through every sum, so an unpriced model
// shows tokens and no dollar figure, never `$0.00`.
//
// NO PRICE TABLE AND NO MODEL TABLE HERE. Every price, the long-prompt tier,
// the characters-per-token figure and the TTL arrive RESOLVED on the usage row
// (`OrchRegistry::compute_group_usage`), for `cacheage.ts`'s reason: a second
// copy in TypeScript is a second place for the answer to drift.
//
// NO INTRA-`src` IMPORTS (the `tokencharts.ts` rule): the wire shapes below are
// structural, so `StripViewPayload` satisfies them at the call site and a
// renamed field fails the VIEW's compile. That is also why the two formatters
// at the bottom are restated rather than imported from `cacheage.ts` —
// `test/promptcost.test.ts` holds them to the originals.
//
// Design: docs/design/prompt-cost.md.

// ── the wire, structurally ──────────────────────────────────────────────────

/** USD per million tokens — the backend's `ModelPrice`, as the row carries it. */
export interface PricePerMtok {
  readonly input: number;
  readonly output: number;
  /** Writing to the 5-minute cache. */
  readonly cache_write: number;
  /** Writing to the 1-hour cache. */
  readonly cache_write_1h: number;
  readonly cache_read: number;
}

/** A request whose prompt is longer than `over_tokens` pays `price` instead. */
export interface LongPromptPrice {
  readonly over_tokens: number;
  readonly price: PricePerMtok;
}

/** The usage row's `prompt_cost` object: what the backend resolved for the
 *  estimate. Every field is null where it is not known — never a zero.
 *
 *  - `context_tokens`: the context the newest turn was sent.
 *  - `first_context_tokens`: the context the session's FIRST turn was sent.
 *  - `price_per_mtok`: the list price the row is estimated at, with the model
 *    it was looked up for (`price_model`), the date the table was read
 *    (`price_dated`), whether the version is `listed` or priced at its
 *    `family-ceiling` (`price_basis`), and a second price for prompts over a
 *    length where the model has one (`price_long_prompt`). All null on a row
 *    whose CLI reports its own dollars, and for a model the table does not
 *    list.
 *  - `chars_per_token`: characters a token stands for on the model's
 *    tokenizer. */
export interface PromptCostInputs {
  readonly context_tokens?: number | null;
  readonly first_context_tokens?: number | null;
  readonly price_model?: string | null;
  readonly price_per_mtok?: PricePerMtok | null;
  readonly price_long_prompt?: LongPromptPrice | null;
  readonly price_basis?: string | null;
  readonly price_dated?: string | null;
  readonly chars_per_token?: number | null;
}

/** The usage-row fields this module reads. Both are optional because a
 *  backend that predates them sends neither, and absence reads as "not known". */
export interface PromptCostRow {
  readonly id: string;
  /** An object on a live row; null on a historical one. */
  readonly prompt_cost?: PromptCostInputs | null;
  /** Which rung resolved the row's `cache_ttl_minutes`. */
  readonly cache_ttl_source?: string | null;
}

/** One roster row, for the "a compact is in flight" reading. */
export interface PromptCostRosterRow {
  readonly id: string;
  readonly compaction?: { readonly status: string } | null;
}

export interface PromptCostStrip {
  readonly groups: Record<
    string,
    | {
        readonly usage: { readonly live_agents: readonly PromptCostRow[] } | null;
        readonly summary?: { readonly agents: readonly PromptCostRosterRow[] } | null;
      }
    | undefined
  >;
}

// ── the reading ─────────────────────────────────────────────────────────────

/** Everything about one pane the estimate needs besides the chip's own state. */
export interface PromptCostReading {
  /** C: the context the newest turn was sent. */
  readonly contextTokens: number | null;
  /** F: the context this session's FIRST turn was sent. */
  readonly firstContextTokens: number | null;
  readonly priceModel: string | null;
  readonly price: PricePerMtok | null;
  readonly longPrompt: LongPromptPrice | null;
  /** `listed`, or `family-ceiling` for a version the price table does not list. */
  readonly priceBasis: string | null;
  readonly priceDated: string | null;
  readonly charsPerToken: number | null;
  /** Which rung resolved the chip's TTL: `block`, `session`, `cli`, or null. */
  readonly ttlSource: string | null;
  /** A compaction is in flight on this pane, so `contextTokens` — the last
   *  turn's — may already be stale. */
  readonly compacting: boolean;
}

/** The roster phases in which a compaction has been asked for or is landing.
 *  `none` and the resolved phases mean the context reading is current. */
const COMPACTING: ReadonlySet<string> = new Set(["armed", "awaiting_evidence", "reinjecting"]);

/** A finite, non-negative count, or null. A row value that is absent, null,
 *  negative or not a number is "not known" — never coerced to zero. */
function count(v: number | null | undefined): number | null {
  return typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : null;
}

/** This pane's reading out of the strip snapshot, or `null` when the strip does
 *  not cover it — no identity, a group the strip did not carry, or no usage
 *  row. Null is "nothing to estimate from", which the surfaces render as no
 *  estimate at all rather than as an estimate of zero. */
export function promptCostFor(
  strip: PromptCostStrip,
  group: string | null,
  agentId: string | null
): PromptCostReading | null {
  if (group === null || agentId === null) return null;
  const g = strip.groups[group];
  const row = g?.usage?.live_agents.find((r) => r.id === agentId);
  if (row === undefined) return null;
  // A row with no `prompt_cost` at all — an older backend — still resolves, to
  // a reading in which nothing is known: the menu then says what is missing
  // instead of the pane silently having no estimate.
  const cost: PromptCostInputs = row.prompt_cost ?? {};
  const cpt = cost.chars_per_token;
  const phase = g?.summary?.agents.find((a) => a.id === agentId)?.compaction?.status;
  return {
    contextTokens: count(cost.context_tokens),
    firstContextTokens: count(cost.first_context_tokens),
    priceModel: cost.price_model ?? null,
    price: cost.price_per_mtok ?? null,
    longPrompt: cost.price_long_prompt ?? null,
    priceBasis: cost.price_basis ?? null,
    priceDated: cost.price_dated ?? null,
    charsPerToken: typeof cpt === "number" && Number.isFinite(cpt) && cpt > 0 ? cpt : null,
    ttlSource: row.cache_ttl_source ?? null,
    compacting: phase !== undefined && COMPACTING.has(phase),
  };
}

// ── the estimate ────────────────────────────────────────────────────────────

/** The typed text as tokens: its length over the model's characters-per-token,
 *  rounded UP so a non-empty prompt is never zero tokens.
 *
 *  `null` when the text is non-empty and the figure is unknown — the caller
 *  then leaves the typed prompt out and says so. Empty text is exactly zero
 *  tokens on any tokenizer. */
export function estimateTokens(chars: number, charsPerToken: number | null): number | null {
  if (!(chars > 0)) return 0;
  if (charsPerToken === null || !(charsPerToken > 0)) return null;
  return Math.ceil(chars / charsPerToken);
}

/** What the chip says about the cache — `cacheage.ts`'s `CacheState`, restated
 *  structurally. */
export type CostCacheState = "hot" | "cooling" | "cold" | "unknown";

/** One way of sending the next request. */
export interface CostFigure {
  /** Tokens read from the cache. */
  readonly readTokens: number;
  /** Tokens written to it. */
  readonly writeTokens: number;
  /** List-price dollars for those tokens, or null when there is no price. */
  readonly usd: number | null;
  /** The request is over the model's long-prompt threshold and is priced at
   *  the higher tier. Always false for a model with no tier. */
  readonly longPromptTier: boolean;
}

export interface PromptCostEstimate {
  /** P, or null when text was typed and its token count cannot be estimated —
   *  in which case every figure below leaves the typed prompt OUT. */
  readonly promptTokens: number | null;
  /** The cache lifetime the write rate was taken for: 60 when the chip's TTL is
   *  an hour or more, else 5. */
  readonly writeMinutes: 5 | 60;
  /** Which of `warm` / `cold` the chip's state says applies now; null when the
   *  state is unknown, and then neither is claimed. */
  readonly now: "warm" | "cold" | null;
  readonly warm: CostFigure | null;
  readonly cold: CostFigure | null;
  readonly fresh: CostFigure | null;
}

function tier(price: PricePerMtok | null, long: LongPromptPrice | null, promptTokens: number): {
  price: PricePerMtok | null;
  long: boolean;
} {
  // "Over" is strict, and a prompt's length is everything sent — the vendor's
  // rule, mirrored from the backend's `PriceQuote::at`.
  if (price !== null && long !== null && promptTokens > long.over_tokens) {
    return { price: long.price, long: true };
  }
  return { price, long: false };
}

function figure(
  readTokens: number,
  writeTokens: number,
  r: PromptCostReading,
  writeMinutes: 5 | 60
): CostFigure {
  const t = tier(r.price, r.longPrompt, readTokens + writeTokens);
  const usd =
    t.price === null
      ? null
      : (readTokens * t.price.cache_read +
          writeTokens * (writeMinutes === 60 ? t.price.cache_write_1h : t.price.cache_write)) /
        1_000_000;
  return { readTokens, writeTokens, usd, longPromptTier: t.long };
}

/** Price the next request's input side three ways.
 *
 *  `ttlMinutes` and `state` are the CHIP'S — the same resolved TTL, so the
 *  estimate and the chip cannot disagree about which cache the pane is on. The
 *  write rate is the 1-hour one when that TTL is an hour or more, else the
 *  5-minute one.
 *
 *  - `warm`  = C read + P written.
 *  - `cold`  = (C + P) written.
 *  - `fresh` = (F + P) written.
 *
 *  `warm` and `cold` are null when C is unknown, `fresh` when F is. A typed
 *  prompt whose tokens cannot be estimated is left out of all three
 *  (`promptTokens: null`) rather than guessed. */
export function estimatePromptCost(
  r: PromptCostReading,
  cache: { readonly state: CostCacheState; readonly ttlMinutes: number | null },
  promptChars: number
): PromptCostEstimate {
  const promptTokens = estimateTokens(promptChars, r.charsPerToken);
  const p = promptTokens ?? 0;
  const writeMinutes: 5 | 60 = cache.ttlMinutes !== null && cache.ttlMinutes >= 60 ? 60 : 5;
  const c = r.contextTokens;
  const f = r.firstContextTokens;
  return {
    promptTokens,
    writeMinutes,
    now: cache.state === "unknown" ? null : cache.state === "cold" ? "cold" : "warm",
    warm: c === null ? null : figure(c, p, r, writeMinutes),
    cold: c === null ? null : figure(0, c + p, r, writeMinutes),
    fresh: f === null ? null : figure(0, f + p, r, writeMinutes),
  };
}

// ── words ───────────────────────────────────────────────────────────────────

/** Token counts the way the rest of the app writes them: `800`, `3.4k`, `1.2M`
 *  (`cacheage.ts`'s `formatTokens`, restated — see the header). */
export function formatCostTokens(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${trim1(n / 1000)}k`;
  return `${trim1(n / 1_000_000)}M`;
}

function trim1(x: number): string {
  const s = x.toFixed(1);
  return s.endsWith(".0") ? s.slice(0, -2) : s;
}

/** A list-price dollar figure: `~$0.08`, `~$12.40`, and `<$0.01` for anything
 *  that would round to zero — a real cost is never printed as `$0.00`. */
export function formatUsd(usd: number): string {
  if (usd > 0 && usd < 0.005) return "<$0.01";
  return `~$${usd.toFixed(2)}`;
}

/** One figure as text, tokens first: `412k read + 1.2k written · ~$0.09`. A
 *  side with no tokens is left out; no price, no dollar part. */
export function figureText(f: CostFigure): string {
  const parts: string[] = [];
  if (f.readTokens > 0) parts.push(`${formatCostTokens(f.readTokens)} read`);
  if (f.writeTokens > 0 || f.readTokens === 0) parts.push(`${formatCostTokens(f.writeTokens)} written`);
  const tokens = parts.join(" + ");
  return f.usd === null ? tokens : `${tokens} · ${formatUsd(f.usd)}`;
}

/** One read-only menu row: the line, and the tooltip that says what it assumes. */
export interface CostRow {
  readonly label: string;
  readonly reason: string;
}

/** What the caller knows about the surface the estimate is shown on. */
export interface CostContext {
  readonly state: CostCacheState;
  readonly ttlMinutes: number | null;
  /** Ms until the chip reads cold, when it is cooling; else null. */
  readonly coldInMs: number | null;
  /** Whether typed text is visible to orrerix on this pane (a compose strip).
   *  Where it is not, the estimate is for the history alone and says so. */
  readonly promptVisible: boolean;
  readonly promptChars: number;
}

function minutesText(ms: number): string {
  const m = Math.max(1, Math.ceil(ms / 60_000));
  return `${m}m`;
}

const TTL_SOURCE_TEXT: Record<string, string> = {
  block: "declared on its workflow block",
  session: "read off this session's own cache writes",
  cli: "the CLI's conservative default — a block on the 1-hour cache declares cache_ttl_minutes: 60",
};

/** Where the chip's TTL came from, in words; empty when it is not known. */
function ttlSourceText(source: string | null): string {
  return source === null ? "" : (TTL_SOURCE_TEXT[source] ?? "");
}

function priceNote(r: PromptCostReading): string {
  if (r.price === null) {
    return "No list price is known for this pane's model, so the figures are tokens only.";
  }
  const model = r.priceModel ?? "this model";
  const dated = r.priceDated === null ? "" : `, as published ${r.priceDated}`;
  const ceiling =
    r.priceBasis === "family-ceiling"
      ? " This exact version is not in the price table, so its family's highest current price is used and the figure may be high."
      : "";
  return (
    `Dollars are the vendor's list price for ${model}${dated}; a subscription pays no per-token price, so read the tokens first.` +
    ceiling
  );
}

/** The header row every estimate opens with: what it is, and the two limits
 *  that apply to every figure under it. */
function headerRow(r: PromptCostReading): CostRow {
  const output =
    r.price === null
      ? "Output is not included."
      : `Output is not included: it is priced on top, at $${r.price.output} per million tokens.`;
  return {
    label: "Next prompt — estimate, input side only",
    reason:
      `An estimate of what the next request's INPUT costs. ${output} ` +
      "It is one request: a turn with N tool calls reads the cache about N times. " +
      priceNote(r),
  };
}

function tierNote(f: CostFigure, r: PromptCostReading): string {
  if (r.longPrompt === null) return "";
  const at = formatCostTokens(r.longPrompt.over_tokens);
  return f.longPromptTier
    ? ` Priced at this model's long-prompt rate: the request is over ${at} tokens.`
    : ` Priced at this model's standard rate: the request is not over ${at} tokens.`;
}

function writeNote(e: PromptCostEstimate, c: CostContext, r: PromptCostReading): string {
  const which = e.writeMinutes === 60 ? "1-hour" : "5-minute";
  if (c.ttlMinutes === null) {
    return `Cache writes are priced at the ${which} rate; no cache TTL is known for this pane.`;
  }
  const source = ttlSourceText(r.ttlSource);
  return (
    `Cache writes are priced at the ${which} rate, following the ${c.ttlMinutes}m TTL the chip uses` +
    (source === "" ? "." : ` (${source}).`)
  );
}

/** The read-only rows for the chip's menu: a header, the figures, and the
 *  inputs they were computed from. Never empty — a pane with nothing to
 *  estimate from gets a row saying what is missing. */
export function promptCostRows(r: PromptCostReading, e: PromptCostEstimate, c: CostContext): CostRow[] {
  const rows: CostRow[] = [headerRow(r)];
  const warm = e.warm;
  const cold = e.cold;
  if (warm === null || cold === null) {
    rows.push({
      label: "No context reading for this pane yet",
      reason:
        "The estimate needs the size of the context the pane's last turn was sent, and this pane's CLI has not recorded one orrerix can read.",
    });
  } else {
    const warmWhy =
      "If the cache is still warm (inferred, not observed): the whole context is read from the cache and the typed prompt is written to it. " +
      "The last wake above is what a real request on this pane read and wrote." +
      tierNote(warm, r);
    const coldWhy =
      "With the cache expired, the whole context is written to it again. " + writeNote(e, c, r) + tierNote(cold, r);
    if (e.now === "warm") {
      const when =
        c.state === "cooling" && c.coldInMs !== null
          ? `cache cooling, cold in ~${minutesText(c.coldInMs)}`
          : `cache ${c.state}`;
      rows.push({ label: `Send now (${when}, inferred): ≈ ${figureText(warm)}`, reason: warmWhy });
      rows.push({ label: `Once it is cold: ≈ ${figureText(cold)}`, reason: coldWhy });
    } else if (e.now === "cold") {
      rows.push({ label: `Send now (cache cold, inferred): ≈ ${figureText(cold)}`, reason: coldWhy });
      rows.push({ label: `Had it stayed warm: ≈ ${figureText(warm)}`, reason: warmWhy });
    } else {
      // No TTL, or no request observed: the chip claims no state, so neither
      // figure is called "now".
      rows.push({ label: `If the cache is warm: ≈ ${figureText(warm)}`, reason: warmWhy });
      rows.push({ label: `If it is cold: ≈ ${figureText(cold)}`, reason: coldWhy });
    }
  }
  if (e.fresh === null) {
    rows.push({
      label: "Fresh agent: + unknown session overhead",
      reason:
        "A new agent pays for its system prompt, tools and instruction files before your prompt. This pane's session has no first-turn record to measure that from, so no figure is given.",
    });
  } else {
    const first = formatCostTokens(r.firstContextTokens ?? 0);
    rows.push({
      label: `Same prompt in a fresh agent: ≈ ${figureText(e.fresh)}`,
      reason:
        `What this session's FIRST turn was sent (~${first} tokens: the CLI's system prompt, its tools, the repo's instruction files — and that session's first prompt, which is included) plus the typed prompt, all written to the cache. ` +
        "A fresh agent also starts without this pane's history." +
        tierNote(e.fresh, r),
    });
  }
  rows.push(inputsRow(r, e, c));
  return rows;
}

function typedText(e: PromptCostEstimate, c: CostContext, r: PromptCostReading): { label: string; reason: string } {
  if (!c.promptVisible) {
    return {
      label: "typed prompt not visible",
      reason:
        "Text typed into this pane's own input box is not visible to orrerix, so the figures are for the history alone — add your prompt's length yourself.",
    };
  }
  if (c.promptChars <= 0) {
    return { label: "nothing typed", reason: "Nothing is typed in the compose strip, so the figures are for the history alone." };
  }
  if (e.promptTokens === null) {
    return {
      label: `typed ${c.promptChars} chars, not counted`,
      reason:
        "No characters-per-token figure is known for this pane's model, so the typed prompt is left out of the figures rather than guessed.",
    };
  }
  const cpt = r.charsPerToken === null ? "" : ` (about ${r.charsPerToken.toFixed(1)} characters per token on this model)`;
  return {
    label: `typed ≈ ${formatCostTokens(e.promptTokens)}`,
    reason:
      `The typed prompt is counted from its LENGTH${cpt}, not tokenized: rough, and code or non-English text runs higher.`,
  };
}

function inputsRow(r: PromptCostReading, e: PromptCostEstimate, c: CostContext): CostRow {
  const typed = typedText(e, c, r);
  const parts: string[] = [
    r.contextTokens === null ? "context unknown" : `context ${formatCostTokens(r.contextTokens)}`,
    typed.label,
  ];
  if (r.price !== null) {
    parts.push(`${r.priceModel ?? "model"} list price${r.priceDated === null ? "" : ` ${r.priceDated}`}`);
  } else {
    parts.push("no list price");
  }
  parts.push(`${e.writeMinutes === 60 ? "1h" : "5m"} cache writes`);
  if (r.compacting) parts.push("compact in flight");
  const stale = r.compacting
    ? " A compaction is in flight on this pane: the context is the last turn's and may have dropped since."
    : " The context is the last turn's; if the CLI has compacted on its own since, it is smaller.";
  return {
    label: `Inputs: ${parts.join(" · ")}`,
    reason: `${typed.reason}${stale} ${writeNote(e, c, r)}`,
  };
}

/** The compose strip's one line, and its tooltip. `null` when there is nothing
 *  to estimate from, and the strip then shows nothing.
 *
 *  One line by contract: the strip's slot has a fixed height
 *  (`.orch-compose-foot`), so this never grows the strip and never resizes the
 *  terminal above it. */
export function composeCostLine(
  r: PromptCostReading,
  e: PromptCostEstimate,
  c: CostContext
): { text: string; title: string } | null {
  if (e.warm === null || e.cold === null) return null;
  const now = e.now === "cold" ? e.cold : e.warm;
  const other = e.now === "cold" ? e.warm : e.cold;
  const nowWord = e.now === null ? "warm" : "now";
  const otherWord = e.now === "cold" ? "warm" : "cold";
  const typed = c.promptChars > 0 && e.promptTokens !== null ? ` incl. ≈${formatCostTokens(e.promptTokens)} typed` : "";
  const parts: string[] = [`${nowWord} ${figureText(now)}`, `${otherWord} ${figureText(other)}`];
  if (e.fresh !== null) parts.push(`fresh ${figureText(e.fresh)}`);
  // "est." leads: the line is read at a glance, without its tooltip, and must
  // not pass for a measurement.
  const text = `est. next: ${parts.join("  |  ")}${typed}`;
  const title = promptCostRows(r, e, c)
    .map((row) => `${row.label}\n  ${row.reason}`)
    .join("\n");
  return { text, title };
}
