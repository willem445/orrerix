// The tokens pane's scorecard table (#2011 slice D) — `src/tokenscorecard.ts`.
//
// What is pinned is every definition that can be WRONG on a table the human
// will compare against `scripts/orch-scorecard.cjs` output: the one-based
// rounds-to-pass, the decided-rows-only fail rate (null, never 0), the PR
// window and its fallback arm, the medians' n<3 refusal, the never-guessed
// lane cli, and the coverage floor's spawn-row-missing rule. The arithmetic
// is a PORT of slice A's script (laneStats / computeWindows / statCell /
// laneCliOf / coverageFloor), so the fixture test at the bottom runs the
// same numbers against a corpus cut from the real group log — the two
// readers must derive the SAME figure from the same rows or one of them is
// broken.
//
// Every test below carries its own positive/negative structure: the floor
// tests assert the mechanism RAN (cards credited non-empty) so an empty
// floor cannot pass by never having looked.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import {
  MEDIAN_MIN_N,
  UNKNOWN,
  scorecardTable,
  type ScorecardAgentLike,
  type ScorecardAuditRowLike,
} from "../src/tokenscorecard.ts";

// ── synthetic fixtures ──────────────────────────────────────────────────────

/** A monotonic clock — arrival order IS what `rounds_to_pass` is a question
 *  about, so every row gets an explicit ts or the next tick. */
let tick = 1_000_000;
function at(ms?: number): number {
  return ms ?? tick++;
}

function row(action: string, detail: unknown, actor: string, ms: number): ScorecardAuditRowLike {
  return { ts_ms: ms, actor, action, detail };
}

/** `rd-lane-spawned` — the structural credit of a delegate to a PR. */
const lane = (pr: number, agent: string, block: string, ms?: number) =>
  row("rd-lane-spawned", { pr, agent, block }, "orrerix", at(ms));

const handback = (pr: number, agent: string, ms?: number) =>
  row("rd-handback", { pr, agent }, "orrerix", at(ms));

/** `review-verdict` — detail.block is the lane, actor is the reviewer. */
const verdict = (pr: number, actor: string, block: string, v: string, ms?: number) =>
  row("review-verdict", { pr, block, verdict: v }, actor, at(ms));

const spawn = (agent: string, block: string, cli: string, ms?: number) =>
  row("agent-spawn", { agent, block, cli }, "orrerix", at(ms));

const roster = (...entries: [id: string, block: string, cli: string][]): ScorecardAgentLike[] =>
  entries.map(([id, block, cli]) => ({ id, block, cli }));

/** The lane figures for one key of a run, or null when the row is absent. */
function rowOf(t: ReturnType<typeof scorecardTable>, key: string) {
  return t.rows.find((r) => r.key === key) ?? null;
}

// ── laneStats ───────────────────────────────────────────────────────────────

test("rounds_to_pass_is_the_one_based_index_of_the_first_pass", () => {
  const t = scorecardTable(
    [
      spawn("rev1", "rev-std", "pi"),
      lane(900, "rev1", "rev-std"),
      verdict(900, "rev1", "rev-std", "fail"),
      verdict(900, "rev1", "rev-std", "fail"),
      verdict(900, "rev1", "rev-std", "pass"),
    ],
    roster(["rev1", "rev-std", "pi"]),
  );
  const r = rowOf(t, "rev-std/pi");
  assert.ok(r, "rev-std/pi row missing");
  const lane1 = t.cards[0].lanes.get("rev-std")!;
  assert.equal(lane1.rounds, 3);
  assert.equal(lane1.roundsToPass, 3);
  // A pass on round 1 is 1, not 0 — the count of rounds, not an index.
  const t2 = scorecardTable(
    [
      spawn("rev2", "rev-std", "pi"),
      lane(901, "rev2", "rev-std"),
      verdict(901, "rev2", "rev-std", "pass"),
    ],
    roster(["rev2", "rev-std", "pi"]),
  );
  assert.equal(t2.cards[0].lanes.get("rev-std")!.roundsToPass, 1);
});

test("a_lane_that_never_passes_has_null_rounds_to_pass_and_the_full_fail_rate", () => {
  const t = scorecardTable(
    [
      spawn("rev1", "rev-std", "pi"),
      lane(900, "rev1", "rev-std"),
      verdict(900, "rev1", "rev-std", "fail"),
      verdict(900, "rev1", "rev-std", "fail"),
    ],
    roster(["rev1", "rev-std", "pi"]),
  );
  const lane1 = t.cards[0].lanes.get("rev-std")!;
  assert.equal(lane1.roundsToPass, null);
  assert.equal(lane1.failRate, 1);
});

test("fail_rate_is_fail_over_decided_and_null_when_nothing_was_decided", () => {
  // `laneStats` decides pass|fail ONLY: any other spelling lands in
  // `verdictsOther` and out of the denominator. (The per-bar
  // `scorecardColumns` in tokencharts.ts uses its own enumerated
  // fail-vocabulary — a different metric with a different source; this port
  // follows the script's laneStats exactly.)
  const t = scorecardTable(
    [
      spawn("rev1", "rev-std", "pi"),
      lane(900, "rev1", "rev-std"),
      verdict(900, "rev1", "rev-std", "fail"),
      verdict(900, "rev1", "rev-std", "PASS-ISH"),
      verdict(900, "rev1", "rev-std", "pass"),
    ],
    roster(["rev1", "rev-std", "pi"]),
  );
  const lane1 = t.cards[0].lanes.get("rev-std")!;
  assert.equal(lane1.rounds, 3);
  assert.equal(lane1.pass, 1);
  assert.equal(lane1.fail, 1);
  assert.equal(lane1.verdictsOther, 1);
  assert.equal(lane1.failRate, 0.5);
  // Nothing decided — null, never 0, which would read as a perfect record.
  const t2 = scorecardTable(
    [
      spawn("rev2", "rev-std", "pi"),
      lane(901, "rev2", "rev-std"),
      verdict(901, "rev2", "rev-std", "unknown"),
    ],
    roster(["rev2", "rev-std", "pi"]),
  );
  assert.equal(t2.cards[0].lanes.get("rev-std")!.failRate, null);
  assert.equal(t2.cards[0].lanes.get("rev-std")!.roundsToPass, null);
});

// ── computeWindows (pane port) ──────────────────────────────────────────────

test("the_wall_clock_runs_from_the_first_row_naming_the_pr_to_the_last_rd_row", () => {
  const t = scorecardTable(
    [
      lane(900, "rev1", "rev-std", 0),
      verdict(900, "rev1", "rev-std", "pass", 3_600_000),
      handback(900, "rev1", 7_200_000),
      // A naming row AFTER the last rd-* row: the window does not follow it.
      row("prompt", { text: "check #900 again" }, "human", 9_000_000),
    ],
    roster(["rev1", "rev-std", "pi"]),
  );
  const c = t.cards[0];
  assert.equal(c.windowStartMs, 0);
  assert.equal(c.windowEndMs, 7_200_000);
  assert.equal(c.windowEndSource, "last-rd-row");
  // `round2`'d hours, as the script's `span_h` is.
  assert.equal(c.wallClockH, 2);
});

test("with_no_rd_row_the_window_falls_back_to_the_last_naming_row_and_says_so", () => {
  const t = scorecardTable(
    [
      row("prompt", { text: "start #901 please" }, "human", 5_000),
      verdict(901, "rev1", "rev-std", "pass", 7_000),
    ],
    roster(["rev1", "rev-std", "pi"]),
  );
  const c = t.cards.find((x) => x.pr === "901")!;
  assert.ok(c, "901 card missing");
  assert.equal(c.windowEndSource, "last-naming-row");
  assert.equal(c.windowEndMs, 7_000);
});

test("a_pr_the_log_never_names_has_no_window_and_no_invented_zero", () => {
  const t = scorecardTable(
    [
      spawn("rev1", "rev-std", "pi"),
      lane(900, "rev1", "rev-std"),
      verdict(900, "rev1", "rev-std", "pass"),
    ],
    roster(["rev1", "rev-std", "pi"]),
  );
  assert.ok(t.cards[0].windowStartMs !== null);
  // The negative arm, made fail-able: a PR discovered only through a
  // verdict row still HAS a window (the verdict names it), so the real
  // "no window" shape is asserted directly below with a nameless log.
  const empty = scorecardTable([], roster());
  assert.equal(empty.cards.length, 0);
});

// ── medians (statCell) ──────────────────────────────────────────────────────

test("cells_below_the_median_floor_are_null_with_n_reported", () => {
  assert.ok(MEDIAN_MIN_N === 3, "the floor is the script's constant");
  const rows: ScorecardAuditRowLike[] = [];
  const rost: ScorecardAgentLike[] = [];
  for (const pr of [900, 901]) {
    rows.push(spawn(`rev-${pr}`, "rev-std", "pi"));
    rows.push(lane(pr, `rev-${pr}`, "rev-std"));
    rows.push(verdict(pr, `rev-${pr}`, "rev-std", "pass"));
    rost.push({ id: `rev-${pr}`, block: "rev-std", cli: "pi" });
  }
  const t = scorecardTable(rows, rost);
  const r = rowOf(t, "rev-std/pi")!;
  assert.ok(r, "row missing");
  assert.equal(r.prs.length, 2);
  assert.equal(r.roundsToPass.median, null);
  assert.equal(r.roundsToPass.n, 2);
  // The wall clock is a fact about the PR even where verdicts are thin.
  assert.equal(r.wallClockH.median, null);
});

test("medians_use_the_exclusive_median_hinges_over_finite_values_only", () => {
  // Five samples: median is the middle, the hinges exclude it.
  const rows: ScorecardAuditRowLike[] = [];
  const rost: ScorecardAgentLike[] = [];
  const hours = [1, 2, 3, 4, 5];
  hours.forEach((h, i) => {
    const pr = 900 + i;
    // first row naming the PR at 0, last rd row at h hours.
    rows.push(lane(pr, `rev-${pr}`, "rev-std", 0));
    rows.push(handback(pr, `rev-${pr}`, h * 3_600_000));
    rows.push(verdict(pr, `rev-${pr}`, "rev-std", "pass", h * 3_600_000 + 1));
    rost.push({ id: `rev-${pr}`, block: "rev-std", cli: "pi" });
  });
  const t = scorecardTable(rows, rost);
  const r = rowOf(t, "rev-std/pi")!;
  assert.deepEqual(
    [r.wallClockH.median, r.wallClockH.q1, r.wallClockH.q3, r.wallClockH.iqr],
    [3, 1.5, 4.5, 3],
  );
});

// ── laneCliOf / the two-CLI rule ────────────────────────────────────────────

test("a_block_running_two_clis_on_one_pr_is_excluded_not_averaged", () => {
  const t = scorecardTable(
    [
      spawn("rev1", "rev-std", "pi"),
      spawn("rev2", "rev-std", "opencode"),
      lane(900, "rev1", "rev-std"),
      lane(900, "rev2", "rev-std"),
      verdict(900, "rev1", "rev-std", "pass"),
    ],
    roster(["rev1", "rev-std", "pi"], ["rev2", "rev-std", "opencode"]),
  );
  assert.equal(t.rows.length, 0, "nothing averages across clis");
  assert.deepEqual(t.excluded, [{ pr: "900", block: "rev-std", why: "rev-std split across opencode+pi" }]);
});

test("a_cli_that_resolves_to_unknown_is_excluded_never_guessed", () => {
  const t = scorecardTable(
    [
      spawn("rev1", "rev-std", ""),
      lane(900, "rev1", "rev-std"),
      verdict(900, "rev1", "rev-std", "pass"),
    ],
    roster(["rev1", "rev-std", ""]),
  );
  assert.equal(t.rows.length, 0);
  assert.deepEqual(t.excluded, [{ pr: "900", block: "rev-std", why: `rev-std cli ${UNKNOWN}` }]);
});

test("one_block_on_two_clis_is_two_rows_read_off_the_roster", () => {
  const rows: ScorecardAuditRowLike[] = [];
  const rost: ScorecardAgentLike[] = [];
  for (const [pr, cli] of [
    [900, "pi"],
    [901, "opencode"],
  ] as const) {
    rows.push(spawn(`rev-${pr}`, "rev-std", cli));
    rows.push(lane(pr, `rev-${pr}`, "rev-std"));
    rows.push(verdict(pr, `rev-${pr}`, "rev-std", "pass"));
    rost.push({ id: `rev-${pr}`, block: "rev-std", cli });
  }
  const t = scorecardTable(rows, rost);
  assert.deepEqual(
    t.rows.map((r) => r.key).sort(),
    ["rev-std/opencode", "rev-std/pi"],
  );
});

// ── coverageFloor's spawn-row-missing half ──────────────────────────────────

test("a_credited_delegate_whose_spawn_row_did_not_survive_flags_the_window", () => {
  // rev1's spawn row is MISSING from the log; rev2's survived. Only 2941's
  // shape — the PR credited with the orphan — may be flagged.
  const t = scorecardTable(
    [
      // (no spawn row for rev1 — the rotation dropped it)
      spawn("rev2", "rev-std", "pi"),
      lane(900, "rev1", "rev-std"),
      lane(900, "rev2", "rev-std"),
      lane(901, "rev2", "rev-std"),
      verdict(900, "rev2", "rev-std", "pass"),
      verdict(901, "rev2", "rev-std", "pass"),
    ],
    roster(["rev2", "rev-std", "pi"]),
  );
  assert.ok(t.cards.length === 2, "the mechanism ran: two cards were scored");
  assert.deepEqual(t.floor.missingSpawn, [{ pr: "900", agents: ["rev1"] }]);
  // The lane still resolves — the surviving delegate answers laneCliOf —
  // so the floor moves without moving the table.
  assert.ok(rowOf(t, "rev-std/pi"));
});

test("a_delegate_with_a_surviving_spawn_row_flags_nothing", () => {
  const t = scorecardTable(
    [
      spawn("rev1", "rev-std", "pi"),
      lane(900, "rev1", "rev-std"),
      verdict(900, "rev1", "rev-std", "pass"),
    ],
    roster(["rev1", "rev-std", "pi"]),
  );
  assert.ok(t.cards[0].creditedAll.length > 0, "the mechanism ran: a delegate was credited");
  assert.deepEqual(t.floor.missingSpawn, []);
});

// ── the real-corpus fixture ─────────────────────────────────────────────────

const here = path.dirname(fileURLToPath(import.meta.url));
const FIXTURE = path.join(here, "fixtures", "tokenscorecard", "audit.jsonl");

test("the_real_log_derives_the_same_table_the_script_definitions_produce", () => {
  const rows: ScorecardAuditRowLike[] = readFileSync(FIXTURE, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((l) => JSON.parse(l));
  // The roster as the backend reports it: the NEWEST agent-spawn row per
  // agent carries the block/cli (`usage_series`' rule). The fixture's
  // builder deliberately drops rev-2416's spawn rows — the rotation shape —
  // so the roster is missing that agent exactly as a real read would be.
  const newest = new Map<string, ScorecardAgentLike>();
  for (const r of rows) {
    if (r.action !== "agent-spawn") continue;
    const d = r.detail as { agent: string; block: string; cli: string };
    newest.set(d.agent, { id: d.agent, block: d.block ?? "", cli: d.cli ?? "" });
  }
  const t = scorecardTable(rows, [...newest.values()]);

  // Population control: the fixture is a fixed cut; if a regeneration moves
  // these, every number below is re-derived, not trusted (fixture README).
  assert.equal(t.floor.rowsRead, 1088);
  assert.deepEqual(
    t.cards.map((c) => c.pr),
    ["2941", "2942", "2943", "2945", "2947", "3038"],
  );

  // Lanes, hand-derived from the fixture's review-verdict rows (see the PR
  // body's agent layer for the sequences).
  const lanesOf = (pr: string) => t.cards.find((c) => c.pr === pr)!.lanes;
  assert.deepEqual(lanesOf("2941").get("rev-std"), {
    rounds: 3,
    pass: 2,
    fail: 1,
    verdictsOther: 0,
    roundsToPass: 1,
    failRate: 0.33,
  });
  assert.equal(lanesOf("2941").get("rev-final")!.roundsToPass, 1);
  assert.equal(lanesOf("2942").get("rev-std")!.roundsToPass, 4);
  assert.equal(lanesOf("2942").get("rev-std")!.failRate, 0.6);
  assert.equal(lanesOf("2943").get("rev-std")!.failRate, 0);
  // #2945 never passed: null rounds-to-pass, total fail rate.
  assert.equal(lanesOf("2945").get("rev-std")!.roundsToPass, null);
  assert.equal(lanesOf("2945").get("rev-std")!.failRate, 1);
  assert.equal(lanesOf("2947").get("rev-std")!.roundsToPass, 2);
  assert.equal(lanesOf("3038").get("rev-std")!.roundsToPass, 3);
  assert.equal(lanesOf("3038").get("rev-std")!.failRate, 0.5);
  assert.equal(lanesOf("3038").get("rev-final")!.failRate, 0.33);

  // Only #2945 has no rd-* row: its window falls back, and says so.
  assert.equal(t.cards.find((c) => c.pr === "2945")!.windowEndSource, "last-naming-row");
  for (const pr of ["2941", "2942", "2943", "2947", "3038"]) {
    assert.equal(t.cards.find((c) => c.pr === pr)!.windowEndSource, "last-rd-row", pr);
  }

  // The table. rev-std/pi: six PRs, #2945's null rounds-to-pass DROPPED
  // (n=5) while its fail rate and wall clock count (n=6). Hand-derived.
  const std = rowOf(t, "rev-std/pi")!;
  assert.ok(std, "rev-std/pi row missing");
  assert.deepEqual(std.prs, ["2941", "2942", "2943", "2945", "2947", "3038"]);
  assert.deepEqual(
    std.roundsToPass,
    { n: 5, dropped: 1, median: 2, q1: 1, q3: 3.5, iqr: 2.5, min: 1, max: 4 },
  );
  assert.deepEqual(
    std.failRate,
    { n: 6, dropped: 0, median: 0.5, q1: 0.33, q3: 0.6, iqr: 0.27, min: 0, max: 1 },
  );
  assert.deepEqual(
    std.wallClockH,
    { n: 6, dropped: 0, median: 3.5, q1: 3.08, q3: 4.36, iqr: 1.28, min: 2.67, max: 4.38 },
  );
  const fin = rowOf(t, "rev-final/claude")!;
  assert.ok(fin, "rev-final/claude row missing");
  assert.deepEqual(fin.prs, ["2941", "2942", "2943", "2947", "3038"]);
  assert.deepEqual(
    fin.roundsToPass,
    { n: 5, dropped: 0, median: 1, q1: 1, q3: 1, iqr: 0, min: 1, max: 1 },
  );
  assert.deepEqual(
    fin.failRate,
    { n: 5, dropped: 0, median: 0, q1: 0, q3: 0.17, iqr: 0.17, min: 0, max: 0.33 },
  );
  assert.deepEqual(
    fin.wallClockH,
    { n: 5, dropped: 0, median: 3.33, q1: 2.88, q3: 4.02, iqr: 1.14, min: 2.67, max: 4.38 },
  );
  // The handback-only lane: worker-adv agents credited by rd-handback, no
  // verdicts of their own — the wall clock is the PR's, everything else n/a.
  const wadv = rowOf(t, "worker-adv/claude")!;
  assert.ok(wadv, "worker-adv/claude row missing");
  assert.deepEqual(wadv.prs, ["2941", "2942", "2947", "3038"]);
  assert.deepEqual(wadv.roundsToPass, { n: 0, dropped: 4, median: null, q1: null, q3: null, iqr: null, min: null, max: null });
  assert.deepEqual(wadv.wallClockH, { n: 4, dropped: 0, median: 3.37, q1: 2.88, q3: 4.02, iqr: 1.14, min: 2.67, max: 4.38 });

  // The floor: rev-2416's spawn row did not survive the fixture's cut, and
  // its PR is the flagged one. Everything else resolved.
  assert.deepEqual(t.floor.missingSpawn, [{ pr: "2941", agents: ["rev-2416"] }]);
  assert.deepEqual(t.excluded, []);
});
