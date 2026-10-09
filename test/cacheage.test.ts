// The pane cache-age timer's model (#3407). What this module gets wrong shows up
// as a chip saying "hot" over a cache that is gone — the one wrong answer the
// indicator exists to prevent — so the state boundaries and the "cannot say"
// rungs are pinned as carefully as the happy path.

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  CACHE_GAP_LABEL,
  cacheAgeFor,
  cacheChipLabel,
  cacheGapTitle,
  cacheIdentityFor,
  cacheChipTitle,
  cacheState,
  formatAge,
  formatTokens,
  wakeCostLine,
  type CacheAgeReading,
  type CacheStripReading,
  type WakeCostReading,
} from "../src/cacheage.ts";

const MIN = 60_000;
const T0 = 1_700_000_000_000;

/** A five-minute-TTL claude pane (cooling from 3m — the backend's band) whose
 *  last request landed at T0. */
const r5 = (over: Partial<CacheAgeReading> = {}): CacheAgeReading => ({
  lastActiveMs: T0,
  ttlMinutes: 5,
  coolingAfterMs: 3 * MIN,
  lastWake: null,
  compactSupported: true,
  ...over,
});

const r60 = (over: Partial<CacheAgeReading> = {}): CacheAgeReading =>
  r5({ ttlMinutes: 60, coolingAfterMs: 48 * MIN, ...over });

test("the state walks hot -> cooling -> cold on the backend's own thresholds", () => {
  assert.equal(cacheState(r5(), T0).state, "hot");
  assert.equal(cacheState(r5(), T0 + 3 * MIN - 1).state, "hot");
  // The cooling threshold is inclusive, and it is the ROW's, not a guess.
  assert.equal(cacheState(r5(), T0 + 3 * MIN).state, "cooling");
  assert.equal(cacheState(r5(), T0 + 5 * MIN - 1).state, "cooling");
  // At the TTL it is cold — never "hot" or "cooling" over an expired cache.
  assert.equal(cacheState(r5(), T0 + 5 * MIN).state, "cold");
  assert.equal(cacheState(r5(), T0 + 500 * MIN).state, "cold");
});

test("a row without a cooling threshold cools at the TTL, never earlier or later", () => {
  const r = r5({ coolingAfterMs: null });
  assert.equal(cacheState(r, T0 + 5 * MIN - 1).state, "hot");
  assert.equal(cacheState(r, T0 + 5 * MIN).state, "cold");
});

test("no TTL is unknown with an age; no request observed is unknown with none", () => {
  assert.deepEqual(cacheState(r5({ ttlMinutes: null, coolingAfterMs: null }), T0 + 12 * MIN), {
    state: "unknown",
    ageMs: 12 * MIN,
  });
  // A zero TTL (the block's "infer nothing") is unknown too, not instantly cold.
  assert.equal(cacheState(r5({ ttlMinutes: 0 }), T0 + MIN).state, "unknown");
  assert.deepEqual(cacheState(r5({ lastActiveMs: null }), T0), { state: "unknown", ageMs: null });
});

test("a clock behind the last request reads age zero, never negative", () => {
  assert.deepEqual(cacheState(r5(), T0 - 5_000), { state: "hot", ageMs: 0 });
});

test("the chip label reads like the issue's own examples", () => {
  assert.equal(cacheChipLabel(r5(), T0 + 2 * MIN + 59_000), "hot 2m");
  assert.equal(cacheChipLabel(r60(), T0 + 48 * MIN), "cooling 48m/60m");
  assert.equal(cacheChipLabel(r60(), T0 + 61 * MIN), "cold");
  assert.equal(cacheChipLabel(r5({ ttlMinutes: null, coolingAfterMs: null }), T0 + 12 * MIN), "idle 12m");
  // Nothing observed yet: no chip at all, never "hot 0s".
  assert.equal(cacheChipLabel(r5({ lastActiveMs: null }), T0), null);
});

test("durations floor, so a label never claims more idle time than has passed", () => {
  assert.equal(formatAge(59_999), "59s");
  assert.equal(formatAge(60_000), "1m");
  assert.equal(formatAge(3 * MIN - 1), "2m");
  assert.equal(formatAge(60 * MIN), "1h");
  assert.equal(formatAge(65 * MIN + 59_000), "1h 5m");
  assert.equal(formatAge(-10), "0s");
});

const wake = (over: Partial<WakeCostReading> = {}): WakeCostReading => ({
  at_ms: T0,
  idle_before_ms: 12 * MIN,
  input_tokens: 800,
  output_tokens: 2_000,
  cache_creation_tokens: 3_400,
  cache_read_tokens: 1_234_567,
  cost_usd: 0.714,
  ...over,
});

test("the wake line splits cache-read from written from uncached, the split cold costs show in", () => {
  assert.equal(
    wakeCostLine(wake()),
    "Last wake after 12m idle: 1.2M cache-read · 3.4k cache-written · 800 uncached · ~$0.71"
  );
  // A cold wake is the mirror image — mostly written.
  assert.equal(
    wakeCostLine(wake({ cache_read_tokens: 0, cache_creation_tokens: 1_300_000, cost_usd: null, idle_before_ms: null })),
    "Last wake: 0 cache-read · 1.3M cache-written · 800 uncached"
  );
  assert.equal(wakeCostLine(null), null);
  assert.equal(formatTokens(999), "999");
  assert.equal(formatTokens(1000), "1k");
});

test("the tooltip says the state is inferred, not observed, and carries the wake", () => {
  const t = cacheChipTitle(r5({ lastWake: wake() }), T0 + 4 * MIN);
  assert.match(t, /inferred from a 5m TTL, not observed/);
  assert.match(t, /cooling/);
  assert.match(t, /Last wake after 12m idle/);
  const unknown = cacheChipTitle(r5({ ttlMinutes: null }), T0 + MIN);
  assert.match(unknown, /no hot\/cold state is inferred/);
  assert.doesNotMatch(unknown, /inferred from a/);
});

const strip = (rows: CacheStripReading["groups"][string]): CacheStripReading => ({ groups: { g1: rows } });

test("the strip lookup reads a pane that has a request on record, and names the gap when it has none", () => {
  const row = {
    id: "w-1",
    source: "transcript",
    last_active_ms: T0,
    cache_ttl_minutes: 5,
    cache_cooling_after_ms: 3 * MIN,
    last_wake: wake(),
    compact_supported: true,
  };
  const s = strip({ usage: { live_agents: [row] } });
  const id = { group: "g1", agentId: "w-1" };
  assert.deepEqual(cacheAgeFor(s, id), {
    reading: {
      lastActiveMs: T0,
      ttlMinutes: 5,
      coolingAfterMs: 3 * MIN,
      lastWake: wake(),
      compactSupported: true,
    },
    gap: null,
  });
  assert.deepEqual(cacheAgeFor(s, null), { reading: null, gap: "no-identity" }, "no identity at all");
  assert.deepEqual(cacheAgeFor(s, { group: "g2", agentId: "w-1" }), { reading: null, gap: "no-row" }, "a group the strip did not carry");
  assert.deepEqual(cacheAgeFor(strip({ usage: null }), id), { reading: null, gap: "no-row" }, "a refused usage section");
  assert.deepEqual(cacheAgeFor(s, { group: "g1", agentId: "w-2" }), { reading: null, gap: "no-row" }, "an agent with no usage row");
  // A row from a backend that predates the compact field: compact support must
  // be affirmatively true, never assumed.
  const predates = cacheAgeFor(strip({ usage: { live_agents: [{ id: "w-1", source: "transcript", last_active_ms: T0 }] } }), id);
  assert.equal(predates.reading?.compactSupported, false);
  // And a row with no source at all has not had a request observed.
  assert.deepEqual(cacheAgeFor(strip({ usage: { live_agents: [{ id: "w-1" }] } }), id), { reading: null, gap: "no-request-yet" });
});

test("a row with no request on record says which kind of source it is missing", () => {
  const id = { group: "g1", agentId: "w-1" };
  const withSource = (source: string) => strip({ usage: { live_agents: [{ id: "w-1", source }] } });
  assert.deepEqual(cacheAgeFor(withSource("statusline"), id), { reading: null, gap: "no-tokens:statusline" }, "a token-less source");
  assert.deepEqual(cacheAgeFor(withSource("none"), id), { reading: null, gap: "no-tokens:none" }, "no record of any kind yet");
  assert.deepEqual(cacheAgeFor(withSource("transcript"), id), { reading: null, gap: "no-request-yet" }, "a first sighting folds no wake");
});

test("a pane's cache identity is its orchestration agent, else its channel agent", () => {
  const orch = { group: "lead-group", agentId: "lead-1" };
  const channel = { group: "__solo__", agentId: "solo-3" };
  assert.deepEqual(cacheIdentityFor(orch, channel), orch, "an orchestration identity wins");
  assert.deepEqual(cacheIdentityFor(null, channel), channel, "a solo or adopted pane is keyed by its channel agent");
  assert.equal(cacheIdentityFor(null, null), null, "a terminal pane has neither");
});

test("the tooltip names exactly what is missing", () => {
  const local = { cli: "claude", remote: false, sessionKnown: true };
  assert.match(cacheGapTitle("no-identity", local), /not registered with orrerix/);
  assert.doesNotMatch(cacheGapTitle("no-identity", local), /remote/i, "a local pane is not called remote");
  assert.match(cacheGapTitle("no-identity", { ...local, remote: true }), /remote host/, "an SSH pane names the remote session");
  assert.match(cacheGapTitle("no-row", local), /no usage record/);
  assert.match(cacheGapTitle("no-tokens:statusline", { ...local, cli: "copilot" }), /^copilot writes no token record/);
  assert.match(cacheGapTitle("no-tokens:none", local), /No request has been recorded/);
  assert.match(cacheGapTitle("no-tokens:none", { ...local, sessionKnown: false }), /session has not been identified yet/);
  assert.match(cacheGapTitle("no-request-yet", local), /No request has been recorded/);
});

test("every reason shows the same muted label", () => {
  assert.equal(CACHE_GAP_LABEL, "cache —");
});
