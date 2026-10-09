// The next-prompt cost estimate (#3831). What this module gets wrong is a number
// a human acts on — "send now" priced as a cache read over a cache that is
// gone, or `$0.00` standing in for "no price is known" — so every figure is
// pinned against arithmetic done here by hand, and every "not known" rung is
// pinned as carefully as the happy path.

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  composeCostLine,
  estimatePromptCost,
  estimateTokens,
  figureText,
  formatCostTokens,
  formatUsd,
  promptCostFor,
  promptCostRows,
  type CostContext,
  type CostFigure,
  type PricePerMtok,
  type PromptCostReading,
  type PromptCostStrip,
} from "../src/promptcost.ts";
import { formatTokens } from "../src/cacheage.ts";

/** Opus 5.5's row of the vendor table, as the backend resolves it. */
const OPUS_55: PricePerMtok = { input: 4, output: 20, cache_write: 5, cache_write_1h: 8, cache_read: 0.2 };
/** Haiku 5.5: the one model with a prompt-length tier. */
const HAIKU_LOW: PricePerMtok = { input: 0.1, output: 0.5, cache_write: 0.125, cache_write_1h: 0.2, cache_read: 0.01 };
const HAIKU_HIGH: PricePerMtok = { input: 0.5, output: 2.5, cache_write: 0.625, cache_write_1h: 1, cache_read: 0.05 };

/** A long-lived Opus 5.5 pane: 400k of context, started at 50k, on the newer
 *  tokenizer. */
const reading = (over: Partial<PromptCostReading> = {}): PromptCostReading => ({
  contextTokens: 400_000,
  firstContextTokens: 50_000,
  priceModel: "claude-opus-5-5",
  price: OPUS_55,
  longPrompt: null,
  priceBasis: "listed",
  priceDated: "2026-10-09",
  charsPerToken: 2.5,
  ttlSource: "session",
  compacting: false,
  ...over,
});

const hot60 = { state: "hot", ttlMinutes: 60 } as const;
const near = (got: number | null, want: number) => {
  assert.ok(got !== null, "expected a dollar figure");
  assert.ok(Math.abs(got - want) < 1e-9, `got ${got}, want ${want}`);
};

test("the three figures are read-plus-write, all-write, and first-turn-plus-prompt", () => {
  // 2,500 typed characters at 2.5 per token is exactly 1,000 tokens.
  const e = estimatePromptCost(reading(), hot60, 2_500);
  assert.equal(e.promptTokens, 1_000);
  assert.equal(e.writeMinutes, 60);
  assert.equal(e.now, "warm");

  // warm: C read at $0.20, P written at the hour's $8.
  assert.deepEqual([e.warm?.readTokens, e.warm?.writeTokens], [400_000, 1_000]);
  near(e.warm!.usd, (400_000 * 0.2 + 1_000 * 8) / 1e6);
  // cold: C + P written.
  assert.deepEqual([e.cold?.readTokens, e.cold?.writeTokens], [0, 401_000]);
  near(e.cold!.usd, (401_000 * 8) / 1e6);
  // fresh: F + P written.
  assert.deepEqual([e.fresh?.readTokens, e.fresh?.writeTokens], [0, 51_000]);
  near(e.fresh!.usd, (51_000 * 8) / 1e6);
});

test("the ordering a human reads — warm under fresh under cold — is what the numbers say", () => {
  // The fixture the feature exists for: a big warm context is cheap to keep
  // using, a fresh agent costs more than that, and letting it go cold costs
  // the most. If the formula's terms were swapped this would not hold.
  const e = estimatePromptCost(reading(), hot60, 2_500);
  const [warm, fresh, cold] = [e.warm!.usd!, e.fresh!.usd!, e.cold!.usd!];
  assert.ok(warm < fresh && fresh < cold, `${warm} < ${fresh} < ${cold}`);
  // And it is not a property of every fixture: a SMALL context that has gone
  // cold is cheaper to resend than a fresh agent is to start.
  const small = estimatePromptCost(reading({ contextTokens: 20_000 }), { state: "cold", ttlMinutes: 60 }, 2_500);
  assert.ok(small.cold!.usd! < small.fresh!.usd!, "a small cold context beats a fresh start");
});

test("the write rate follows the chip's TTL: an hour or more writes at the hour's rate", () => {
  const usd = (ttl: number | null) => estimatePromptCost(reading(), { state: "cold", ttlMinutes: ttl }, 0);
  // The same 400k written, at $5 and at $8 a million.
  near(usd(5).cold!.usd, (400_000 * 5) / 1e6);
  near(usd(60).cold!.usd, (400_000 * 8) / 1e6);
  assert.equal(usd(5).writeMinutes, 5);
  assert.equal(usd(60).writeMinutes, 60);
  // The boundary is the hour itself; an unknown TTL takes the lower rate
  // rather than assuming the dearer cache.
  assert.equal(usd(59).writeMinutes, 5);
  assert.equal(usd(1440).writeMinutes, 60);
  assert.equal(usd(null).writeMinutes, 5);
  // The READ price does not depend on which cache it was written to.
  near(
    estimatePromptCost(reading(), { state: "hot", ttlMinutes: 5 }, 0).warm!.usd,
    estimatePromptCost(reading(), { state: "hot", ttlMinutes: 60 }, 0).warm!.usd!
  );
});

test("the chip's state picks which figure is 'now', and an unknown state picks neither", () => {
  const now = (state: "hot" | "cooling" | "cold" | "unknown") =>
    estimatePromptCost(reading(), { state, ttlMinutes: 60 }, 0).now;
  assert.equal(now("hot"), "warm");
  // Cooling is still warm: the cache has not expired yet.
  assert.equal(now("cooling"), "warm");
  assert.equal(now("cold"), "cold");
  assert.equal(now("unknown"), null);
  // Both figures are still computed for an unknown state — it is only the
  // CLAIM about which applies that is withheld.
  const e = estimatePromptCost(reading(), { state: "unknown", ttlMinutes: null }, 0);
  assert.ok(e.warm !== null && e.cold !== null);
});

test("with nothing typed the estimate is the history alone", () => {
  const e = estimatePromptCost(reading(), hot60, 0);
  assert.equal(e.promptTokens, 0);
  assert.deepEqual([e.warm?.readTokens, e.warm?.writeTokens], [400_000, 0]);
  near(e.warm!.usd, (400_000 * 0.2) / 1e6);
  assert.deepEqual([e.fresh?.writeTokens, e.cold?.writeTokens], [50_000, 400_000]);
});

test("an unknown first-turn context gives no fresh figure, and the others stand", () => {
  const e = estimatePromptCost(reading({ firstContextTokens: null }), hot60, 2_500);
  assert.equal(e.fresh, null);
  assert.ok(e.warm !== null && e.cold !== null);
  const rows = promptCostRows(reading({ firstContextTokens: null }), e, ctx()).map((r) => r.label);
  assert.ok(rows.includes("Fresh agent: + unknown session overhead"), rows.join("\n"));
});

test("an unknown context gives no warm or cold figure — never an estimate of zero", () => {
  const r = reading({ contextTokens: null });
  const e = estimatePromptCost(r, hot60, 2_500);
  assert.equal(e.warm, null);
  assert.equal(e.cold, null);
  // F is a separate reading and still answers.
  assert.equal(e.fresh?.writeTokens, 51_000);
  const rows = promptCostRows(r, e, ctx()).map((row) => row.label);
  assert.ok(rows.includes("No context reading for this pane yet"), rows.join("\n"));
  assert.ok(rows.some((l) => l.includes("context unknown")), rows.join("\n"));
  // The compose strip shows nothing rather than a line built on a missing C.
  assert.equal(composeCostLine(r, e, ctx()), null);
});

test("no price is tokens only: every dollar figure is null, never zero", () => {
  const r = reading({ price: null, priceModel: null, priceDated: null, priceBasis: null });
  const e = estimatePromptCost(r, hot60, 2_500);
  for (const [name, f] of [["warm", e.warm], ["cold", e.cold], ["fresh", e.fresh]] as const) {
    assert.ok(f !== null, `${name} still has tokens`);
    assert.equal(f.usd, null, `${name} must be null-poisoned, not 0`);
    assert.notEqual(f.usd, 0);
  }
  // The token counts are the same ones a priced pane gets.
  assert.deepEqual([e.warm?.readTokens, e.cold?.writeTokens], [400_000, 401_000]);
  // And no surface prints a dollar sign for it.
  const text = promptCostRows(r, e, ctx()).map((row) => row.label).join("\n");
  assert.doesNotMatch(text, /\$/);
  assert.match(text, /400k read \+ 1k written/);
  assert.match(text, /no list price/);
  // Control: the priced pane's rows DO carry one.
  const priced = promptCostRows(reading(), estimatePromptCost(reading(), hot60, 2_500), ctx());
  assert.match(priced.map((row) => row.label).join("\n"), /~\$0\.09/);
});

test("typed text becomes tokens by the model's characters-per-token, rounded up", () => {
  // The two tokenizer generations the backend resolves: 3.5 characters a token
  // before Claude 4.7, and 3.5 / 1.3 from it on. Same text, more tokens.
  // 6,999 rather than a round 7,000: 7,000 / (3.5 / 1.3) is one float ulp over
  // 2,600, and a test that sat on that edge would be pinning the rounding of a
  // division rather than the rule.
  const earlier = estimateTokens(6_999, 3.5);
  const newer = estimateTokens(6_999, 3.5 / 1.3);
  assert.equal(earlier, 2_000);
  assert.equal(newer, 2_600);
  // Rounded UP, so one character is one token and never none.
  assert.equal(estimateTokens(1, 3.5), 1);
  assert.equal(estimateTokens(8, 3.5), 3);
  // Empty text is exactly zero on any tokenizer, known or not.
  assert.equal(estimateTokens(0, 3.5), 0);
  assert.equal(estimateTokens(0, null), 0);
  // Text with NO known figure is not counted with a made-up one.
  assert.equal(estimateTokens(7_000, null), null);
  assert.equal(estimateTokens(7_000, 0), null);
});

test("a typed prompt that cannot be counted is left out and said so", () => {
  const r = reading({ charsPerToken: null });
  const e = estimatePromptCost(r, hot60, 7_000);
  assert.equal(e.promptTokens, null);
  // The figures are the history alone — the same as with nothing typed.
  assert.deepEqual(e.warm, estimatePromptCost(r, hot60, 0).warm);
  const rows = promptCostRows(r, e, ctx({ promptChars: 7_000 }));
  const inputs = rows[rows.length - 1];
  assert.match(inputs.label, /typed 7000 chars, not counted/);
  assert.match(inputs.reason, /left out of the figures rather than guessed/);
});

test("a tiered model is priced at the tier the whole request falls in", () => {
  const r = reading({
    priceModel: "claude-haiku-5-5",
    price: HAIKU_LOW,
    longPrompt: { over_tokens: 100_000, price: HAIKU_HIGH },
    contextTokens: 99_000,
    firstContextTokens: 40_000,
    charsPerToken: 1,
  });
  const cold5 = { state: "cold", ttlMinutes: 5 } as const;
  // 99,000 + 1,000 typed is exactly 100,000: "up to" is inclusive.
  const at = estimatePromptCost(r, cold5, 1_000);
  assert.equal(at.cold?.longPromptTier, false);
  near(at.cold!.usd, (100_000 * 0.125) / 1e6);
  // One more typed token crosses it, and the WHOLE request pays the higher
  // price — read tokens included.
  const over = estimatePromptCost(r, cold5, 1_001);
  assert.equal(over.cold?.longPromptTier, true);
  near(over.cold!.usd, (100_001 * 0.625) / 1e6);
  assert.equal(over.warm?.longPromptTier, true);
  near(over.warm!.usd, (99_000 * 0.05 + 1_001 * 0.625) / 1e6);
  // The fresh agent's request is its OWN length, and stays in the lower tier.
  assert.equal(over.fresh?.longPromptTier, false);
  near(over.fresh!.usd, (41_001 * 0.125) / 1e6);
  // The tier used is named on each figure's row.
  const rows = promptCostRows(r, over, ctx({ state: "cold", ttlMinutes: 5, promptChars: 1_001 }));
  const now = rows.find((row) => row.label.startsWith("Send now"));
  const fresh = rows.find((row) => row.label.startsWith("Same prompt in a fresh agent"));
  assert.match(now?.reason ?? "", /long-prompt rate: the request is over 100k tokens/);
  assert.match(fresh?.reason ?? "", /standard rate: the request is not over 100k tokens/);
  // A model with no tier says nothing about one.
  const flat = promptCostRows(reading(), estimatePromptCost(reading(), hot60, 0), ctx());
  assert.doesNotMatch(flat.map((row) => row.reason).join("\n"), /long-prompt rate|standard rate/);
});

// ── the strip lookup ────────────────────────────────────────────────────────

const strip = (cost: object | null | undefined, compaction?: string, ttlSource?: string): PromptCostStrip => ({
  groups: {
    g1: {
      usage: {
        live_agents: [
          {
            id: "a1",
            ...(cost === undefined ? {} : { prompt_cost: cost }),
            ...(ttlSource === undefined ? {} : { cache_ttl_source: ttlSource }),
          },
        ],
      },
      summary: { agents: [{ id: "a1", ...(compaction === undefined ? {} : { compaction: { status: compaction } }) }] },
    },
    refused: { usage: null },
  },
});

test("the reading is read off the pane's own usage row, absent fields as unknown", () => {
  const r = promptCostFor(
    strip(
      {
        context_tokens: 400_000,
        first_context_tokens: 50_000,
        price_model: "claude-opus-5-5",
        price_per_mtok: OPUS_55,
        price_long_prompt: null,
        price_basis: "listed",
        price_dated: "2026-10-09",
        chars_per_token: 2.5,
      },
      undefined,
      "session"
    ),
    "g1",
    "a1"
  );
  assert.deepEqual(r, reading());
  // A row with no `prompt_cost` at all (a backend that predates it), one whose
  // object is null, and one whose object is empty: all unknown, none zero.
  const nothingKnown = {
    contextTokens: null,
    firstContextTokens: null,
    priceModel: null,
    price: null,
    longPrompt: null,
    priceBasis: null,
    priceDated: null,
    charsPerToken: null,
    ttlSource: null,
    compacting: false,
  };
  for (const cost of [undefined, null, {}]) {
    assert.deepEqual(promptCostFor(strip(cost), "g1", "a1"), nothingKnown, JSON.stringify(cost));
  }
  // A value that is not a usable count is unknown, not coerced.
  const junk = promptCostFor(strip({ context_tokens: -1, chars_per_token: 0 }), "g1", "a1");
  assert.equal(junk?.contextTokens, null);
  assert.equal(junk?.charsPerToken, null);
});

test("every way the strip does not cover the pane answers null", () => {
  const s = strip({ context_tokens: 1 });
  assert.equal(promptCostFor(s, null, "a1"), null);
  assert.equal(promptCostFor(s, "g1", null), null);
  assert.equal(promptCostFor(s, "no-such-group", "a1"), null);
  assert.equal(promptCostFor(s, "refused", "a1"), null);
  assert.equal(promptCostFor(s, "g1", "someone-else"), null);
  // Control: the same strip does resolve the pane it carries.
  assert.equal(promptCostFor(s, "g1", "a1")?.contextTokens, 1);
});

test("a compact in flight is read off the roster, and only the in-flight phases count", () => {
  const compacting = (phase?: string) => promptCostFor(strip({}, phase), "g1", "a1")?.compacting;
  for (const phase of ["armed", "awaiting_evidence", "reinjecting"]) {
    assert.equal(compacting(phase), true, phase);
  }
  for (const phase of ["none", "abandoned", "resolved", undefined]) {
    assert.equal(compacting(phase), false, String(phase));
  }
  const r = reading({ compacting: true });
  const rows = promptCostRows(r, estimatePromptCost(r, hot60, 0), ctx());
  const inputs = rows[rows.length - 1];
  assert.match(inputs.label, /compact in flight/);
  assert.match(inputs.reason, /may have dropped since/);
  // Control: a pane with no compact in flight does not wear the flag.
  const calm = promptCostRows(reading(), estimatePromptCost(reading(), hot60, 0), ctx());
  assert.doesNotMatch(calm[calm.length - 1].label, /compact in flight/);
});

// ── words ───────────────────────────────────────────────────────────────────

/** The surface an orchestrator pane's menu is shown on: a hot hour-long cache
 *  and 2,500 characters in the compose strip. */
function ctx(over: Partial<CostContext> = {}): CostContext {
  return { state: "hot", ttlMinutes: 60, coldInMs: null, promptVisible: true, promptChars: 2_500, ...over };
}

const rowsFor = (r: PromptCostReading, c: CostContext) =>
  promptCostRows(r, estimatePromptCost(r, { state: c.state, ttlMinutes: c.ttlMinutes }, c.promptChars), c);

test("the menu rows name the state they assume, tokens first", () => {
  const labels = rowsFor(reading(), ctx()).map((r) => r.label);
  assert.deepEqual(labels, [
    "Next prompt — estimate, input side only",
    "Send now (cache hot, inferred): ≈ 400k read + 1k written · ~$0.09",
    "Once it is cold: ≈ 401k written · ~$3.21",
    "Same prompt in a fresh agent: ≈ 51k written · ~$0.41",
    "Inputs: context 400k · typed ≈ 1k · claude-opus-5-5 list price 2026-10-09 · 1h cache writes",
  ]);
  // Cooling says how long is left, rounded up so it never promises more time
  // than the chip does.
  const cooling = rowsFor(reading(), ctx({ state: "cooling", coldInMs: 3 * 60_000 + 1 })).map((r) => r.label);
  assert.equal(cooling[1], "Send now (cache cooling, cold in ~4m, inferred): ≈ 400k read + 1k written · ~$0.09");
  // Cold swaps the two: what it costs now, and what it would have.
  const cold = rowsFor(reading(), ctx({ state: "cold" })).map((r) => r.label);
  assert.equal(cold[1], "Send now (cache cold, inferred): ≈ 401k written · ~$3.21");
  assert.equal(cold[2], "Had it stayed warm: ≈ 400k read + 1k written · ~$0.09");
  // No state claimed: neither row is called "now".
  const unknown = rowsFor(reading(), ctx({ state: "unknown", ttlMinutes: null })).map((r) => r.label);
  assert.equal(unknown[1], "If the cache is warm: ≈ 400k read + 1k written · ~$0.09");
  assert.match(unknown[2], /^If it is cold: ≈ 401k written/);
  assert.doesNotMatch(unknown.join("\n"), /Send now/);
});

test("every limit of the number is on the surface that shows it", () => {
  const rows = rowsFor(reading(), ctx());
  const all = rows.map((r) => `${r.label}\n${r.reason}`).join("\n");
  // Every figure is labelled an estimate, by the header that opens the block.
  assert.match(rows[0].label, /estimate/);
  // Output is unknown and priced on top.
  assert.match(rows[0].reason, /Output is not included: it is priced on top, at \$20 per million tokens/);
  // One request, not a turn.
  assert.match(rows[0].reason, /a turn with N tool calls reads the cache about N times/);
  // List price; a subscription pays none.
  assert.match(rows[0].reason, /list price for claude-opus-5-5, as published 2026-10-09/);
  assert.match(rows[0].reason, /a subscription pays no per-token price/);
  // The warm figure is conditional on an inference, and points at the evidence.
  assert.match(rows[1].reason, /If the cache is still warm \(inferred, not observed\)/);
  assert.match(rows[1].reason, /The last wake above is what a real request on this pane read and wrote/);
  // Which write rate, which TTL, and where that TTL came from.
  assert.match(rows[2].reason, /priced at the 1-hour rate, following the 60m TTL the chip uses/);
  assert.match(rows[2].reason, /read off this session's own cache writes/);
  // The first-turn figure includes the first prompt, with its size.
  assert.match(rows[3].reason, /~50k tokens/);
  assert.match(rows[3].reason, /that session's first prompt, which is included/);
  // The typed prompt is a length, not a tokenization.
  assert.match(rows[4].reason, /counted from its LENGTH \(about 2\.5 characters per token on this model\), not tokenized/);
  // The context is the last turn's.
  assert.match(rows[4].reason, /if the CLI has compacted on its own since, it is smaller/);
  // No unknown appears as a number.
  assert.doesNotMatch(all, /NaN|undefined|null/);
});

test("the TTL's source is named for each rung, and a default says how to declare the hour", () => {
  const cold = (ttlSource: string | null, ttlMinutes: number | null) =>
    rowsFor(reading({ ttlSource }), ctx({ ttlMinutes }))[2].reason;
  assert.match(cold("block", 60), /60m TTL the chip uses \(declared on its workflow block\)/);
  assert.match(cold("cli", 5), /priced at the 5-minute rate, following the 5m TTL/);
  assert.match(cold("cli", 5), /a block on the 1-hour cache declares cache_ttl_minutes: 60/);
  assert.match(cold(null, null), /no cache TTL is known for this pane/);
  // An unrecognised source is left out rather than printed raw.
  assert.doesNotMatch(cold("something-new", 5), /something-new|\(\)/);
});

test("a pane with no compose strip says the typed prompt is not visible", () => {
  const rows = rowsFor(reading(), ctx({ promptVisible: false, promptChars: 0 }));
  const inputs = rows[rows.length - 1];
  assert.match(inputs.label, /typed prompt not visible/);
  assert.match(inputs.reason, /add your prompt's length yourself/);
  // The figures are the history alone.
  assert.equal(rows[1].label, "Send now (cache hot, inferred): ≈ 400k read · ~$0.08");
  // A visible but empty strip says something different: nothing is typed.
  const empty = rowsFor(reading(), ctx({ promptChars: 0 }));
  assert.match(empty[empty.length - 1].label, /nothing typed/);
});

test("a family-ceiling price says the figure may be high", () => {
  const rows = rowsFor(reading({ priceBasis: "family-ceiling", priceModel: "claude-opus-9" }), ctx());
  assert.match(rows[0].reason, /not in the price table, so its family's highest current price is used/);
  assert.doesNotMatch(rowsFor(reading(), ctx())[0].reason, /highest current price/);
});

test("the compose line is one line: now, the other state, and fresh", () => {
  const r = reading();
  const line = (c: CostContext) =>
    composeCostLine(r, estimatePromptCost(r, { state: c.state, ttlMinutes: c.ttlMinutes }, c.promptChars), c);
  const hot = line(ctx());
  assert.equal(
    hot?.text,
    "next ≈ now 400k read + 1k written · ~$0.09  |  cold 401k written · ~$3.21  |  fresh 51k written · ~$0.41 incl. ≈1k typed"
  );
  assert.doesNotMatch(hot?.text ?? "", /\n/);
  // The tooltip is the menu's own rows, so the two surfaces cannot disagree.
  assert.equal(
    hot?.title,
    rowsFor(r, ctx())
      .map((row) => `${row.label}\n  ${row.reason}`)
      .join("\n")
  );
  const cold = line(ctx({ state: "cold" }));
  assert.match(cold?.text ?? "", /^next ≈ now 401k written · ~\$3\.21 {2}\| {2}warm 400k read \+ 1k written/);
  // No state claimed: the first figure is called warm, not now.
  const unknown = line(ctx({ state: "unknown", ttlMinutes: null }));
  assert.match(unknown?.text ?? "", /^next ≈ warm 400k read/);
  assert.doesNotMatch(unknown?.text ?? "", /now/);
  // Nothing typed: no "incl." tail.
  assert.doesNotMatch(line(ctx({ promptChars: 0 }))?.text ?? "", /incl\./);
});

test("figures are written tokens first, and a tiny cost is never printed as zero", () => {
  const f = (readTokens: number, writeTokens: number, usd: number | null): CostFigure => ({
    readTokens,
    writeTokens,
    usd,
    longPromptTier: false,
  });
  assert.equal(figureText(f(400_000, 1_000, 0.088)), "400k read + 1k written · ~$0.09");
  assert.equal(figureText(f(400_000, 0, 0.08)), "400k read · ~$0.08");
  assert.equal(figureText(f(0, 51_000, null)), "51k written");
  // Nothing at all is still a figure — zero written — rather than an empty string.
  assert.equal(figureText(f(0, 0, 0)), "0 written · ~$0.00");
  assert.equal(formatUsd(0.004), "<$0.01");
  assert.equal(formatUsd(0.005), "~$0.01");
  assert.equal(formatUsd(12.4), "~$12.40");
  assert.equal(formatUsd(0), "~$0.00");
});

test("the token formatter restated here is cacheage's, value for value", () => {
  // promptcost.ts imports nothing from src/, so its formatter is a second copy.
  // This is what stops the two from drifting.
  for (const n of [0, 1, 999, 1_000, 1_049, 1_050, 3_400, 99_949, 999_949, 999_999, 1_000_000, 1_234_567, 12_000_000]) {
    assert.equal(formatCostTokens(n), formatTokens(n), String(n));
  }
});
