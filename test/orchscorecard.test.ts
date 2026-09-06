// `scripts/orch-scorecard.cjs` — the per-PR orchestration scorecard (#2011 B1).
//
// WHAT THIS PINS, and why each pin can fail. The script's whole value is that two
// readers derive the SAME number from the same rows, so every counter is pinned to
// the audit/transcript SHAPE it reads, against a synthetic corpus in
// `test/fixtures/orchscorecard/` built so that no two counters share a value — a
// fixture whose axes are all the same constant cannot tell a working counter from a
// broken one (#1182).
//
// The corpus is two PRs plus a control:
//
//   #900  driven — `rd-*` rows, two lanes, three verdicts across two blocks, three
//         refusals under two reasons, one hold, one hand-back, a merge time.
//   #901  hand-routed — no `rd-*` row at all, a verdict, a human prompt, a system
//         notice, and NO merge time, so the window's fallback arm is exercised.
//   #902  named by nothing. This is the NEGATIVE CONTROL for every positive control
//         below: `rows_classified` is 0 here and non-zero for #900/#901, so an
//         assertion of "the mechanism ran" is fail-able rather than decorative.
//
// The transcript fixture is SIX SYNTHETIC LINES written by hand. Real transcript
// content is private and never enters a fixture; the script reads only `timestamp`
// and `message.usage` and this file pins that it reads nothing else usefully — the
// `user` line carries no usage and must be skipped, and the duplicate `assistant`
// line carrying one message id twice must collapse to a single turn.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const require_ = createRequire(import.meta.url);
const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..');
const scriptPath = path.join(root, 'scripts', 'orch-scorecard.cjs');
const fixtures = path.join(here, 'fixtures', 'orchscorecard');

const sc = require_(scriptPath) as any;

const AUDIT = path.join(fixtures, 'audit.jsonl');
const USAGE = path.join(fixtures, 'usage.json');
const AGENTS = path.join(fixtures, 'agents.json');
const TRANSCRIPT = path.join(fixtures, 'transcript.jsonl');
const PR_META = path.join(fixtures, 'pr-meta.json');

// One run of the whole pipeline through the CLI, reused by the per-counter tests.
// Going through `main` rather than the internals is deliberate: the argument parsing,
// the file reads and the JSON shape are as much of the contract as the counters.
function runScorecard(extraArgs: string[] = []): any {
  const out = execFileSync(process.execPath, [
    scriptPath,
    '--audit', AUDIT,
    '--usage', USAGE,
    '--agents', AGENTS,
    '--transcript', TRANSCRIPT,
    '--pr-meta', PR_META,
    '--pr', '900', '--pr', '901', '--pr', '902',
    ...extraArgs,
  ], { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 });
  return out;
}

const REPORT = JSON.parse(runScorecard());
const card = (pr: number) => REPORT.prs.find((c: any) => c.pr === pr);

// ---------------------------------------------------------------------------
// Positive control: the mechanism ran at all.
// ---------------------------------------------------------------------------

test('positive control: rows were classified, and the control PR classified none', () => {
  assert.ok(REPORT.coverage.rows_classified > 0, 'no audit row was classified — the scorecard did not run');
  assert.equal(REPORT.coverage.rows_classified, 24);
  assert.ok(card(900).rows_classified > 0);
  assert.ok(card(901).rows_classified > 0);
  // The negative control. Without this the assertion above passes for a scorecard
  // that classifies every row it sees regardless of which PR it names.
  assert.equal(card(902).rows_classified, 0);
  assert.equal(card(902).orchestrator.wakes_total, 0);
  assert.equal(card(902).review.rounds, 0);
  assert.equal(card(902).delegates.count, 0);
  assert.equal(card(902).windows.pr, null);
  assert.equal(card(902).windows.loop, null);
  assert.equal(card(902).share.orchestrator_pct_raw, null);
});

// ---------------------------------------------------------------------------
// Wake classification — the shape, one class at a time.
// ---------------------------------------------------------------------------

test('classifyWake: each class is decided by its leading shape', () => {
  assert.equal(sc.classifyWake('[orrerix] w-13 reports progress (#900): pushed'), 'delegate-progress');
  assert.equal(sc.classifyWake('[orrerix] w-13 reports done (#900): green'), 'delegate-done');
  assert.equal(sc.classifyWake('[orrerix] rev-12 reports approved (#900): pass'), 'reviewer-report');
  assert.equal(sc.classifyWake('[orrerix] rev-12 reports request_changes (#900): three findings'), 'reviewer-report');
  assert.equal(sc.classifyWake('[orrerix] w-13 reports blocked (#900): needs a call'), 'delegate-blocked');
  assert.equal(sc.classifyWake('[orrerix] message from w-13 (#900): answering inline'), 'delegate-blocked');
  assert.equal(sc.classifyWake('[orrerix] rev-12 (rev-final) recorded verdict PASS on PR #900: gate satisfied'), 'verdict-notice');
  assert.equal(sc.classifyWake('[orrerix] PR #900 checks: SUCCESS — all 5 checks passed.'), 'system-notice');
  assert.equal(sc.classifyWake('[orrerix] run 33346168758: completed — conclusion: success.'), 'system-notice');
  // NEGATIVE CONTROL: a human typing into the pane is `other`, never a report class,
  // and an empty/absent text does not throw or silently become a report.
  assert.equal(sc.classifyWake('please look at #901 by hand'), 'other');
  assert.equal(sc.classifyWake(''), 'other');
  assert.equal(sc.classifyWake(undefined), 'other');
  // The tie-break: a reviewer's `reports approved` must NOT fall into the generic
  // delegate classes, and a verdict echo must not fall into `system-notice`.
  assert.notEqual(sc.classifyWake('[orrerix] rev-12 reports approved (#900): pass'), 'delegate-progress');
  assert.notEqual(sc.classifyWake('[orrerix] rev-12 (rev-final) recorded verdict FAIL on PR #900: x'), 'system-notice');
  // Every class the script declares is reachable by some shape above.
  assert.deepEqual(sc.WAKE_KINDS, [
    'delegate-progress', 'delegate-done', 'reviewer-report',
    'delegate-blocked', 'verdict-notice', 'system-notice', 'other',
  ]);
});

test('prTokenRe: the trailing-digit guard stops a prefix match', () => {
  assert.ok(sc.prTokenRe(1751).test('bumped #1751 to green'));
  assert.ok(sc.prTokenRe(1751).test('see #1751.'));
  // Without the guard `#175` matches inside `#1751` and every counter for the
  // short-numbered PR inherits the long-numbered one's rows.
  assert.equal(sc.prTokenRe(175).test('bumped #1751 to green'), false);
  assert.equal(sc.prTokenRe(900).test('#9001'), false);
  assert.ok(sc.prTokenRe(900).test('#900'));
});

test('rowNamesPr: `detail.pr` is structural, the token is the fallback', () => {
  assert.ok(sc.rowNamesPr({ detail: { pr: 900 } }, 900));
  assert.ok(sc.rowNamesPr({ detail: { text: 'work on #900' } }, 900));
  assert.equal(sc.rowNamesPr({ detail: { pr: 901 } }, 900), false);
  assert.equal(sc.rowNamesPr({ detail: {} }, 900), false);
  assert.equal(sc.rowNamesPr({}, 900), false);
});

// ---------------------------------------------------------------------------
// Windows.
// ---------------------------------------------------------------------------

test('windows: the loop window is the rd rows, the PR window ends at the merge', () => {
  const c = card(900);
  assert.deepEqual(c.windows.loop, {
    start_ms: 1700000120000, end_ms: 1700000780000, tail_ms: 600000, span_h: 0.18,
  });
  assert.deepEqual(c.windows.pr, {
    start_ms: 1700000000000,   // the `agent-spawn` row whose brief names #900
    end_ms: 1700001200000,     // `merged_at` from --pr-meta
    end_source: 'merged_at',
    tail_ms: 600000,
    span_h: 0.33,
  });
});

test('windows: with no merge time and no rd row the end falls back, and says so', () => {
  const c = card(901);
  assert.equal(c.windows.loop, null, 'a hand-routed PR has no loop window');
  assert.equal(c.windows.pr.end_source, 'last-naming-row');
  assert.equal(c.windows.pr.start_ms, 1700000030000);
  assert.equal(c.windows.pr.end_ms, 1700000390000);
  assert.equal(c.windows.pr.span_h, 0.1);
});

test('inWindow: the +10 min tail is inclusive and one-sided', () => {
  const win = { start_ms: 1000, end_ms: 2000, tail_ms: 600000 };
  assert.equal(sc.inWindow(999, win), false);
  assert.ok(sc.inWindow(1000, win));
  assert.ok(sc.inWindow(2000 + 600000, win));
  assert.equal(sc.inWindow(2000 + 600001, win), false);
  assert.equal(sc.inWindow(1500, null), false);
});

// ---------------------------------------------------------------------------
// Orchestrator attention.
// ---------------------------------------------------------------------------

test('wakes: counted only for prompts delivered to an orchestrator pane, inside the window, naming the PR', () => {
  const a = card(900).orchestrator;
  assert.equal(a.wakes_total, 3);
  assert.deepEqual(a.wakes_by_kind, {
    'delegate-progress': 1, 'delegate-done': 1, 'reviewer-report': 0,
    'delegate-blocked': 0, 'verdict-notice': 1, 'system-notice': 0, other: 0,
  });
  const b = card(901).orchestrator;
  assert.equal(b.wakes_total, 5);
  assert.deepEqual(b.wakes_by_kind, {
    'delegate-progress': 0, 'delegate-done': 0, 'reviewer-report': 1,
    'delegate-blocked': 2, 'verdict-notice': 0, 'system-notice': 1, other: 1,
  });
  // The corpus holds one `[orrerix]` prompt naming #900 that went to a WORKER pane
  // (the driver's own hand-back). It is not an orchestrator wake and must not be one
  // here — deleting the `orchIds.has(detail.to)` test makes #900 read 4.
  assert.equal(a.wakes_total + b.wakes_total, 8);
  assert.equal(REPORT.group.files[0].orchestrator_wakes, 8);
});

test('loop notices: the corrected count and the #1778 S5 count differ by the delegate-pane deliveries', () => {
  const a = card(900).orchestrator;
  // Inside the loop window (1700000120000 .. 1700000780000 + 10 min) two
  // `[orrerix]` prompts reached the orchestrator; a third reached w-13.
  assert.equal(a.loop_notices, 2);
  assert.equal(a.loop_notices_any_pane_s5, 3);
  // A hand-routed PR has no loop window, so both are zero however many notices the
  // orchestrator got.
  assert.equal(card(901).orchestrator.loop_notices, 0);
  assert.equal(card(901).orchestrator.loop_notices_any_pane_s5, 0);
});

// ---------------------------------------------------------------------------
// Review rounds and the driver.
// ---------------------------------------------------------------------------

test('review rounds: verdicts keyed by block and verdict, from `review-verdict` rows', () => {
  // Four verdicts, one of them recorded AFTER the merge (see the outside-window
  // counter below) — a round is a round wherever the row lands.
  assert.equal(card(900).review.rounds, 4);
  assert.deepEqual(card(900).review.by_block, {
    'rev-std': { fail: 1, pass: 1 },
    'rev-final': { pass: 2 },
  });
  // A hand-routed PR still has rounds — this counter predates the driver entirely,
  // which is what makes a before/after comparison possible.
  assert.equal(card(901).review.rounds, 1);
  assert.deepEqual(card(901).review.by_block, { 'rev-final': { escalate: 1 } });
});

test('driver counters: each `rd-*` action lands in its own bucket, reasons kept apart', () => {
  const d = card(900).driver;
  assert.equal(d.drives, 1);
  assert.equal(d.lane_spawns, 3);
  assert.equal(d.hand_backs, 1);
  assert.equal(d.refused, 3);
  assert.equal(d.held, 1);
  assert.equal(d.satisfied, 1);
  // An `rd-*` action this reader has no bucket for is counted under its own action name
  // rather than dropped — the doc promises a new driver row cannot go missing.
  assert.equal(d['rd-a-future-action'], 1);
  assert.deepEqual(d.refused_by_reason, { 'lane-spawn-refused': 2, 'worker-unresumable': 1 });
  assert.deepEqual(d.held_by_reason, { 'drive-stalled': 1 });
  // Untouched buckets stay zero rather than absent, so a table cell is never blank
  // for a counter that simply did not fire.
  assert.equal(d.cancelled, 0);
  assert.equal(d.consumed, 0);
  assert.equal(d.ci_green, 0);
  // NEGATIVE CONTROL: no `rd-*` row names #901, so every driver counter is zero and
  // the reason maps are empty — not inherited from #900.
  const h = card(901).driver;
  assert.equal(h.drives + h.lane_spawns + h.hand_backs + h.refused + h.held + h.satisfied, 0);
  assert.deepEqual(h.refused_by_reason, {});
  assert.deepEqual(h.held_by_reason, {});
});

// ---------------------------------------------------------------------------
// Tokens.
// ---------------------------------------------------------------------------

test('dedupeTranscriptTurns: one API response is one turn, keeping the largest output', () => {
  const entries = [
    { type: 'assistant', timestamp: '2023-11-14T22:15:00.000Z', message: { id: 'a', usage: { input_tokens: 1, output_tokens: 2, cache_read_input_tokens: 3, cache_creation_input_tokens: 4 } } },
    { type: 'assistant', timestamp: '2023-11-14T22:15:00.500Z', message: { id: 'a', usage: { input_tokens: 1, output_tokens: 40, cache_read_input_tokens: 3, cache_creation_input_tokens: 4 } } },
    { type: 'user', timestamp: '2023-11-14T22:15:01.000Z', message: { role: 'user' } },
    { type: 'assistant', timestamp: '2023-11-14T22:15:02.000Z', message: { id: 'b' } },
    { type: 'assistant', message: { id: 'c', usage: { input_tokens: 1, output_tokens: 1 } } },
  ];
  const out = sc.dedupeTranscriptTurns(entries);
  assert.equal(out.turns.length, 1, 'the duplicated message id must collapse to one turn');
  assert.equal(out.turns[0].usage.output, 40, 'the mid-stream line under-reports output and must lose');
  // Three lines carry no usable usage: the user line, the assistant line with no
  // usage, and the assistant line with no timestamp (it cannot be windowed).
  assert.equal(out.non_usage_lines, 3);
  assert.equal(out.without_id, 0);
});

test('orchestrator tokens: the sum of deduped transcript turns stamped inside the PR window', () => {
  const a = card(900).orchestrator.tokens_window;
  // Turns at +100 s and +1100 s are inside #900's window; the one at +5000 s is not.
  assert.deepEqual(a, { input: 17, cache_read: 3000, cache_creation: 23, output: 61, total: 3101, turns: 2 });
  // #901's window ends 710 s earlier, so it sees only the first turn — the same
  // transcript, a different window, a different number. A scorecard that ignored the
  // window would report 3101 here too.
  const b = card(901).orchestrator.tokens_window;
  assert.deepEqual(b, { input: 10, cache_read: 1000, cache_creation: 20, output: 50, total: 1080, turns: 1 });
  // NEGATIVE CONTROL: no window, no tokens.
  assert.equal(card(902).orchestrator.tokens_window.total, 0);
  assert.equal(card(902).orchestrator.tokens_window.turns, 0);
});

test('orchestrator tokens are also apportioned by this PR\'s share of the window\'s wakes', () => {
  const a = card(900).orchestrator;
  assert.deepEqual(a.wake_share, { pr_wakes: 3, window_wakes: 8, share: 0.38 });
  assert.equal(a.tokens_attributed.total, 1163); // round(3101 * 3/8) = round(1162.875)
  const b = card(901).orchestrator;
  assert.deepEqual(b.wake_share, { pr_wakes: 5, window_wakes: 8, share: 0.63 });
  assert.equal(b.tokens_attributed.total, 675); // 1080 * 5/8, exact
});

test('delegate tokens: attributed agents only, weighted, orchestrator excluded', () => {
  const d = card(900).delegates;
  assert.equal(d.count, 4);
  assert.deepEqual(d.agents.map((a: any) => a.agent).sort(), ['rev-11', 'rev-11-prev', 'rev-12', 'w-13']);
  // rev-12 reviewed both PRs, so half its lifetime tokens land on each (H4).
  assert.deepEqual(d.tokens, { input: 500, cache_read: 4500, cache_creation: 250, output: 100, total: 5350, turns: 4 });

  // THE SELF-CORRECTING BRANCH of the H8 split, and the one the benchmark's delegate
  // column rests on: `ses-11` is ONE session carried across TWO agent ids, BOTH
  // attributed to #900 (an `rd-lane-spawned` row names rev-11-prev). Each is credited
  // half, and the halves must re-sum to the whole row — 535 + 535 = 1070, the single
  // `usage.json` row that session has.
  //
  // Both halves are asserted, and that is deliberate: the per-occupant credits are
  // pinned at 535 each, so they redden on their own under EITHER regression —
  // crediting a shared row ONCE reads 1070 + 0, and a no-split implementation reads
  // 1070 + 1070; neither value passes an assert against 535. The sum is a cross-check:
  // it states the re-sum property outright instead of leaving it implied by the two
  // halves. How common this case is on the live store is a dated figure, not a
  // constant — doc/design/orchestration-evals.md §4.6 carries it (§10 names these
  // asserts).
  const rev11 = d.agents.find((a: any) => a.agent === 'rev-11');
  const rev11prev = d.agents.find((a: any) => a.agent === 'rev-11-prev');
  assert.equal(rev11.usage_key, 'session');
  assert.equal(rev11.shared_session_agents, 2);
  assert.equal(rev11.session_weight, 0.5);
  assert.equal(rev11.pr_weight, 1);
  assert.equal(rev11.tokens, 1070);          // the whole row its session carries
  assert.equal(rev11.tokens_credited, 535);
  assert.equal(rev11prev.tokens, 1070);      // the SAME row — one session, two occupants
  assert.equal(rev11prev.tokens_credited, 535);
  assert.equal(rev11.tokens_credited + rev11prev.tokens_credited, 1070,
    'a fully-attributed lineage must re-sum to its session row, neither doubled nor halved');
  // An unshared session is credited whole, so the split is not a blanket halving.
  const w13 = d.agents.find((a: any) => a.agent === 'w-13');
  assert.equal(w13.shared_session_agents, 1);
  assert.equal(w13.tokens_credited, w13.tokens);
  const e = card(901).delegates;
  assert.equal(e.count, 2);
  assert.deepEqual(e.agents.map((a: any) => a.agent).sort(), ['rev-12', 'w-14']);
  // PARTIAL ATTRIBUTION, the branch the H8 split exists for: w-14 shares session
  // `ses-14` with w-16, which no row ties to any PR. w-14 is therefore credited HALF
  // the row its session carries, and the other half is dropped rather than assigned —
  // an under-count of the delegate side, which the note says makes the orchestrator
  // share an over-estimate. Without this fixture the split is only ever exercised where
  // BOTH occupants land on one PR, which cannot tell an even split from no split.
  assert.equal(e.tokens.total, 3570);
  const w14 = e.agents.find((a: any) => a.agent === 'w-14');
  assert.equal(w14.shared_session_agents, 2);
  assert.equal(w14.session_weight, 0.5);
  assert.equal(w14.pr_weight, 1);
  assert.equal(w14.tokens, 5000);
  assert.equal(w14.tokens_credited, 2500);
  // ... and w-16 is not a delegate of anything: an unattributed session-mate consumes
  // its share of the row without appearing on any card.
  for (const c of REPORT.prs) {
    assert.equal(c.delegates.agents.some((a: any) => a.agent === 'w-16'), false);
  }
  // The orchestrator's own `usage.json` row must never be counted as a delegate —
  // it would be double-counted against the transcript figure. orch-10 IS attributed
  // to #900 by its restore brief (see the coverage test), so this is the role filter
  // being exercised, not an agent that was never a candidate.
  for (const c of REPORT.prs) {
    assert.equal(c.delegates.agents.some((a: any) => a.agent === 'orch-10'), false);
  }
});

test('orchestrator share is reported both raw and apportioned', () => {
  assert.equal(card(900).share.orchestrator_pct_raw, 36.69);   // 3101 / 8451
  assert.equal(card(900).share.orchestrator_pct_attributed, 17.86); // 1163 / 6513
  assert.equal(card(901).share.orchestrator_pct_raw, 23.23);   // 1080 / 4650
  assert.equal(card(901).share.orchestrator_pct_attributed, 15.9);  // 675 / 4245
});

// ---------------------------------------------------------------------------
// Attribution and coverage.
// ---------------------------------------------------------------------------

test('attribution: a structural row beats a text join, and the tier is reported', () => {
  const byAgent = new Map<string, any>();
  for (const c of REPORT.prs) for (const a of c.delegates.agents) byAgent.set(a.agent + '@' + c.pr, a);
  // w-13 is named by an `rd-handback` row carrying both the agent and the PR.
  assert.equal(byAgent.get('w-13@900').tier, 'structural');
  // rev-11 / rev-12 come from `review-verdict` (`actor` + `detail.pr`).
  assert.equal(byAgent.get('rev-11@900').tier, 'structural');
  assert.equal(byAgent.get('rev-12@901').tier, 'structural');
  // w-14 has no such row anywhere — only its spawn brief names #901.
  assert.equal(byAgent.get('w-14@901').tier, 'text');
  // w-14 works ONE PR, so its PR-axis weight is 1; its overall weight is 0.5 because
  // its session is shared. Asserting `weight` alone would conflate the two axes.
  assert.equal(byAgent.get('w-14@901').pr_weight, 1);
  assert.equal(byAgent.get('w-14@901').weight, 0.5);
  assert.equal(byAgent.get('rev-12@900').weight, 0.5);
  assert.equal(byAgent.get('rev-12@900').shared_with_prs, 2);
});

test('attribution: a text join outside the PR window is ignored', () => {
  const rows = [
    { action: 'agent-spawn', ts_ms: 500, detail: { agent: 'w-in', task: 'fix #900' } },
    { action: 'agent-spawn', ts_ms: 99999, detail: { agent: 'w-out', task: 'still citing #900 days later' } },
  ];
  const windows = new Map([[900, { pr: { start_ms: 0, end_ms: 1000, tail_ms: 0 }, loop: null }]]);
  const out = sc.attributeAgents(rows, [900], windows);
  assert.deepEqual([...out.keys()], ['w-in']);
  assert.equal(out.get('w-in').tier, 'text');
  // POSITIVE CONTROL for the line above: widen the window and `w-out` IS picked up,
  // so the empty result is the window bound working rather than the join never firing.
  const wide = new Map([[900, { pr: { start_ms: 0, end_ms: 1000000, tail_ms: 0 }, loop: null }]]);
  assert.deepEqual([...sc.attributeAgents(rows, [900], wide).keys()].sort(), ['w-in', 'w-out']);
});

test('coverage: unattributed and split agents are named, not swallowed', () => {
  const cov = REPORT.coverage;
  // w-15 was spawned inside #900's window and its brief names no PR: it is exactly
  // the gap #2011 B2 exists to close, so it must appear by name.
  assert.deepEqual(cov.agents_unattributed_spawned_in_window, ['w-15']);
  assert.deepEqual(cov.agents_split_across_prs, [{ agent: 'rev-12', prs: [900, 901], tier: 'structural' }]);
  // POSITIVE CONTROL for the delegate test's "orchestrator excluded" assertion: the
  // corpus DOES attribute orch-10 to #900 (its restore brief names the PR), so that
  // assertion is about the role filter rather than about orch-10 never being seen.
  assert.equal(cov.agents_attributed, 6);
  assert.equal(cov.agents_unattributed_spawned_in_window.includes('orch-10'), false);
  assert.equal(cov.audit_rows_read, 29);
  assert.equal(cov.audit_parse_errors, 0);
  // A usage row carrying neither a session key nor an agent id is unusable and says so.
  assert.equal(cov.usage_rows_unusable, 1);
  assert.equal(cov.usage_sessions_indexed, 6);
  // Two shared sessions, and they really do exercise DIFFERENT branches — `ses-11`
  // has BOTH occupants attributed to #900 (an `rd-lane-spawned` row names rev-11-prev),
  // so its halves re-sum to the whole row; `ses-14` has only one attributed, so half of
  // its row is dropped. Until an audit row named rev-11-prev, both sessions were the
  // partial case and the self-correcting branch was pinned by nothing.
  assert.equal(cov.usage_sessions_shared_by_more_than_one_agent, 2);
  assert.ok(REPORT.prs.find((c: any) => c.pr === 900).delegates.agents
    .some((a: any) => a.agent === 'rev-11-prev'), 'ses-11 must have BOTH occupants attributed');
  assert.deepEqual(cov.transcripts, [{
    path: TRANSCRIPT, lines: 6, assistant_usage_lines: 5, deduped_turns: 4, usage_rows_without_id: 1,
  }]);
});

test('coverage: rows inside a window that no counter consumed are reported by action', () => {
  // Not every row naming a PR is a counter's input. Those that are not are named, so
  // "the scorecard saw everything" is checkable rather than assumed.
  assert.deepEqual(card(900).rows_unclassified_in_window, { 'agent-spawn': 2, prompt: 1 });
  assert.deepEqual(card(901).rows_unclassified_in_window, { 'agent-spawn': 1 });
  // A `review-verdict` / `rd-*` row is matched structurally on `detail.pr`, so it counts
  // wherever it occurs — wider than the PR window. How many did is reported rather than
  // left for a reader to wonder about. The corpus carries one such row on #900 (a verdict
  // recorded after the merge) and none on #901.
  assert.equal(card(900).rows_counted_outside_pr_window, 1); // the post-merge verdict
  // NEGATIVE CONTROL: #901 has no such row, so a non-zero here is not a constant.
  assert.equal(card(901).rows_counted_outside_pr_window, 0);
});

test('coverage: every heuristic is declared with an id, a statement and its structural fix', () => {
  assert.ok(REPORT.coverage.heuristics.length >= 9);
  const ids = REPORT.coverage.heuristics.map((h: any) => h.id);
  assert.deepEqual(ids, [...new Set(ids)], 'heuristic ids must be unique');
  // …and in ascending order. A new heuristic spliced in above its predecessor
  // renders as "H7, H9, H8", which is how H9 first landed (rev-std).
  const nums = ids.map((i: string) => Number(i.slice(1)));
  assert.deepEqual(nums, [...nums].sort((a: number, b: number) => a - b), 'heuristics must read in id order');
  for (const h of REPORT.coverage.heuristics) {
    assert.match(h.id, /^H\d+$/);
    assert.ok(h.what.length > 20, `heuristic ${h.id} has no statement`);
    assert.ok(h.fix.length > 10, `heuristic ${h.id} names no structural fix`);
  }
});

// ---------------------------------------------------------------------------
// #2167 — a zero `usage.json` row backfilled from the transcript on disk.
//
// The corpus for this is its own directory (`fixtures/orchscorecard/backfill/`)
// rather than an extra row in the shared one, because the shared fixture's
// counters are pinned to the digit and a new session would move several of them
// for a reason that has nothing to do with what those tests are about.
//
// It is the shared corpus with ONE change: `ses-13` — `w-13`, the worker
// attributed to #900 — has the row the broken collector wrote, four zero
// counters under `source: "statusline"`, and its transcript sits under a
// WORKTREE-cwd project folder (`C--Projects-loomux-worktrees-agent-rev-1919`).
// That folder name is the shape #2167 first suspected of breaking the lookup;
// it is here as a non-regression witness, since neither the script's scan nor
// `usage::claude_transcript_path` derives a slug at all.
//
// Two controls sit beside it, so "the backfill ran" is fail-able:
//   ses-14  a NON-zero row whose transcript is also on disk — it must be left
//           exactly alone, which is what stops the backfill from being a
//           blanket "recompute every row from disk".
//   ses-19  a zero row with NO transcript — it must be reported as skipped
//           rather than silently dropped.
//   ses-20  a zero row whose transcript EXISTS and folds to nothing (one user
//           line, no usage). A missing file and an empty one are different
//           facts — only one of them is recoverable by putting the file back —
//           so they are reported in different buckets, and this is what stops
//           the second bucket being a field nothing ever fills.
//   agent:w-21
//           a zero row keyed by `agent:<id>` rather than by a CLI session,
//           which `UsageSnapshot::key` produces for a pane that never got one.
//           No transcript could ever match it, so filing it as "no transcript"
//           would report a session LOST that never existed — on the live store
//           that was 125 of 146 skips. Third bucket, same reason as ses-20's.
// ---------------------------------------------------------------------------

const BACKFILL = path.join(fixtures, 'backfill');
const BF_USAGE = path.join(BACKFILL, 'usage.json');
const BF_AGENTS = path.join(BACKFILL, 'agents.json');
const BF_PROJECTS = path.join(BACKFILL, 'claude-projects');

function runBackfill(extraArgs: string[] = []): any {
  const out = execFileSync(process.execPath, [
    scriptPath,
    '--audit', AUDIT,
    '--usage', BF_USAGE,
    '--agents', BF_AGENTS,
    '--transcript', TRANSCRIPT,
    '--pr-meta', PR_META,
    '--claude-projects', BF_PROJECTS,
    '--pr', '900', '--pr', '901', '--pr', '902',
    ...extraArgs,
  ], { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 });
  return JSON.parse(out);
}

test('backfill: a zero row whose transcript is on disk is summed from it, and reaches the counters', () => {
  const withBf = runBackfill();
  const without = runBackfill(['--no-backfill']);

  // NEGATIVE CONTROL first: with the backfill off, #900's delegate tokens are
  // short by exactly w-13's row. If this ever equalled the backfilled figure the
  // test below would be measuring nothing.
  assert.equal(without.prs.find((c: any) => c.pr === 900).delegates.tokens.total, 2140,
    '5350 minus the whole 3210 row w-13 lost: the shortfall is exactly that session');

  // And with it on, the card reproduces the number the collector should have
  // written — asserted against the SHARED corpus's own run, not against a
  // constant retyped here: the backfill's job is to reconstruct exactly the row
  // a working collector produces, so the two reports must agree field by field.
  assert.deepEqual(
    withBf.prs.find((c: any) => c.pr === 900).delegates.tokens,
    REPORT.prs.find((c: any) => c.pr === 900).delegates.tokens,
  );
  assert.equal(withBf.prs.find((c: any) => c.pr === 900).delegates.tokens.total, 5350);
  assert.deepEqual(
    withBf.prs.find((c: any) => c.pr === 900).share,
    REPORT.prs.find((c: any) => c.pr === 900).share,
    'the orchestrator share is the figure #2167 says was wrong — it must come back too',
  );
});

test('backfill: coverage says how many rows, from where, and what it left alone', () => {
  const cov = runBackfill().coverage.usage_rows_backfilled_from_transcript;
  assert.equal(cov.scanned, true);
  assert.equal(cov.claude_projects_root, BF_PROJECTS);
  // POPULATION CONTROL: every zero row considered is accounted for by exactly one
  // outcome. The script itself throws if these do not reconcile; asserting the
  // parts here is what makes the reconciliation fail-able rather than a tautology
  // over whatever it happened to count.
  assert.equal(cov.zero_rows_considered, 4);
  assert.equal(cov.rows, 1);
  // The THREE skip outcomes are kept apart, and each says a different thing:
  // ses-19's file is missing (the store lost it), ses-20's exists and folds to
  // nothing (nothing was lost), agent:w-21 was never keyed by a session at all
  // (there was never a file to lose). Collapsing any pair reports a loss that
  // did not happen.
  assert.deepEqual(cov.zero_rows_without_a_transcript, ['ses-19']);
  assert.deepEqual(cov.zero_rows_whose_transcript_summed_to_zero, ['ses-20']);
  assert.deepEqual(cov.zero_rows_with_no_session_id, ['agent:w-21']);
  assert.equal(
    cov.rows + cov.zero_rows_without_a_transcript.length
      + cov.zero_rows_whose_transcript_summed_to_zero.length
      + cov.zero_rows_with_no_session_id.length,
    cov.zero_rows_considered,
  );
  // The scan really did walk more than the one folder it needed, so "found it"
  // is not "there was only one thing there".
  assert.equal(cov.projects_scanned, 2);
  assert.equal(cov.transcripts_indexed, 3);

  assert.equal(cov.from.length, 1);
  const [row] = cov.from;
  assert.equal(row.session, 'ses-13');
  assert.equal(row.agent_id, 'w-13');
  assert.equal(row.was_source, 'statusline', 'the source it replaced is named, not discarded');
  assert.equal(row.tokens.total, 3210);
  assert.deepEqual(row.tokens, { input: 300, cache_read: 2700, cache_creation: 150, output: 60, total: 3210 });
  // Dedup on `message.id` — the same rule the orchestrator transcript goes
  // through. The fixture writes that message TWICE, mid-stream (output 30) and
  // final (output 60); summing the lines would read 90 and two turns.
  assert.equal(row.turns, 1);
  assert.equal(row.tokens.output, 60);
  // The worktree-cwd folder, mixed separators and all: the lookup is a scan, so
  // the folder's NAME never decided anything.
  assert.match(row.path, /C--Projects-loomux-worktrees-agent-rev-1919/);
});

test('backfill: a row that already has tokens is never rewritten from disk', () => {
  const cov = runBackfill().coverage.usage_rows_backfilled_from_transcript;
  // `ses-14` has a transcript in the fixture tree (11/11/11/11, unlike anything
  // in its row) AND a non-zero usage row. It is not a candidate at all — it
  // never appears in `from`, and it never appears in the skipped list either,
  // because it was never considered.
  assert.equal(cov.from.some((f: any) => f.session === 'ses-14'), false);
  assert.equal(cov.zero_rows_without_a_transcript.includes('ses-14'), false);
  // Its figures are the shared corpus's, untouched.
  const bf = runBackfill();
  const w14 = bf.prs.find((c: any) => c.pr === 901).delegates.agents.find((a: any) => a.agent === 'w-14');
  const ref = REPORT.prs.find((c: any) => c.pr === 901).delegates.agents.find((a: any) => a.agent === 'w-14');
  assert.deepEqual(w14, ref);
});

test('backfill: --no-backfill still reports how many zero rows there are', () => {
  const cov = runBackfill(['--no-backfill']).coverage.usage_rows_backfilled_from_transcript;
  assert.equal(cov.disabled, true);
  assert.equal(cov.scanned, false, 'no projects tree is walked when the backfill is off');
  assert.equal(cov.rows, 0);
  // The zero rows are still COUNTED, so turning the backfill off cannot hide the
  // condition it exists for.
  assert.equal(cov.zero_rows_considered, 4);
});

test('backfill: an unreadable projects root leaves every zero row named, not silently at zero', () => {
  const out = runBackfill(['--claude-projects', path.join(BACKFILL, 'no-such-dir')]);
  const cov = out.coverage.usage_rows_backfilled_from_transcript;
  assert.equal(cov.scanned, false);
  assert.equal(cov.rows, 0);
  // "I could not look" is not "there was nothing there": every zero row is
  // named, where a readable root names only the one that really had no file.
  assert.deepEqual(cov.zero_rows_without_a_transcript, ['ses-13', 'ses-19', 'ses-20']);
  assert.deepEqual(cov.zero_rows_whose_transcript_summed_to_zero, [],
    'nothing was read, so nothing can be reported as having folded to nothing');
  // The agent:-keyed row is still classified correctly: that judgement needs no
  // filesystem read, so an unreadable root does not make it unknowable.
  assert.deepEqual(cov.zero_rows_with_no_session_id, ['agent:w-21']);
});

test('backfill: the shared corpus has no zero row, so its run never walks a projects tree', () => {
  // The DEFAULT root is ~/.claude/projects, and this pins that the default costs
  // nothing on a store with nothing to backfill — the index is built only when a
  // candidate exists, so a run over a healthy store never touches the home dir.
  const cov = REPORT.coverage.usage_rows_backfilled_from_transcript;
  assert.equal(cov.zero_rows_considered, 0);
  assert.equal(cov.scanned, false);
  assert.equal(cov.rows, 0);
  assert.equal(cov.claude_projects_root, sc.defaultClaudeProjectsRoot());
});

test('backfill: a row keyed by agent id, not by a session, is its own outcome', async () => {
  // #2167 review N1. `UsageSnapshot::key` is the CLI session id "or `agent:<id>`
  // when there is none", and an `agent:`-keyed row can never match a transcript
  // index. Filing it under "no transcript" says the store lost a session that
  // never existed — 86% of the live store's skips, all of them phantom losses.
  const rows = [
    { key: 'agent:w-99', agent_id: 'w-99', source: 'none', input_tokens: 0, output_tokens: 0, cache_creation_tokens: 0, cache_read_tokens: 0 },
    { key: 'ses-missing', agent_id: 'w-98', source: 'none', input_tokens: 0, output_tokens: 0, cache_creation_tokens: 0, cache_read_tokens: 0 },
  ];
  const { backfill } = await sc.backfillZeroUsageRows(rows, BF_PROJECTS);
  assert.deepEqual(backfill.zero_rows_with_no_session_id, ['agent:w-99']);
  assert.deepEqual(backfill.zero_rows_without_a_transcript, ['ses-missing']);
  // DISCRIMINATING: the two rows are identical but for the key shape, so this
  // cannot pass by classifying everything one way.
  assert.equal(backfill.zero_rows_with_no_session_id.length, 1);
  assert.equal(backfill.zero_rows_without_a_transcript.length, 1);
  assert.equal(sc.isAgentKeyedRow(rows[0]), true);
  assert.equal(sc.isAgentKeyedRow(rows[1]), false);
});

test('reconcileBackfill: the accounting guard refuses a record that does not add up', () => {
  // #2167 review N2. The guard is UNFIREABLE through the pipeline — the four
  // outcomes are exhaustive by construction, and a reviewer's mutation disabling
  // the throw left the suite green, correctly. So it is pinned DIRECTLY, with a
  // record no branch in the pipeline can produce, rather than left as a guard
  // the suite cannot redden. Its job is to trip a FUTURE branch that forgets to
  // record its skip.
  const ok = {
    zero_rows_considered: 3, rows: 1,
    zero_rows_without_a_transcript: ['a'],
    zero_rows_whose_transcript_summed_to_zero: ['b'],
    zero_rows_with_no_session_id: [],
  };
  assert.equal(sc.reconcileBackfill(ok), ok, 'a record that adds up is returned unchanged');
  // One row swallowed by an unrecorded branch — the shape that certifies
  // coverage never delivered.
  assert.throws(
    () => sc.reconcileBackfill({ ...ok, zero_rows_considered: 4 }),
    /backfill accounting: 1 backfilled \+ 2 skipped != 4/,
  );
  // And the other direction: a skip recorded twice.
  assert.throws(
    () => sc.reconcileBackfill({ ...ok, zero_rows_with_no_session_id: ['c'] }),
    /backfill accounting/,
  );
  // NEGATIVE CONTROL: each of the three skip buckets really is in the sum, so
  // "it throws" is not carried by one of them alone.
  for (const bucket of ['zero_rows_without_a_transcript', 'zero_rows_whose_transcript_summed_to_zero', 'zero_rows_with_no_session_id']) {
    const one = { zero_rows_considered: 1, rows: 0,
      zero_rows_without_a_transcript: [], zero_rows_whose_transcript_summed_to_zero: [],
      zero_rows_with_no_session_id: [], [bucket]: ['x'] };
    assert.doesNotThrow(() => sc.reconcileBackfill(one), `${bucket} must count toward the sum`);
  }
});

test('backfillZeroUsageRows: the four counters decide, never the source label', async () => {
  // Unit-level, because the CLI corpus can only show ONE spelling of a zero row.
  // A row can arrive at zero under any source — `none`, `statusline`, even
  // `transcript` — and it is the FIGURES that make it a candidate.
  const rows = [
    { key: 'ses-13', agent_id: 'a', source: 'none', input_tokens: 0, output_tokens: 0, cache_creation_tokens: 0, cache_read_tokens: 0 },
    { key: 'ses-14', agent_id: 'b', source: 'transcript', input_tokens: 0, output_tokens: 0, cache_creation_tokens: 0, cache_read_tokens: 0 },
    { key: 'ses-15', agent_id: 'c', source: 'statusline', input_tokens: 1, output_tokens: 0, cache_creation_tokens: 0, cache_read_tokens: 0 },
  ];
  const { usage, backfill } = await sc.backfillZeroUsageRows(rows, BF_PROJECTS);
  assert.equal(backfill.zero_rows_considered, 2, 'both zero rows, whatever they call themselves');
  assert.deepEqual(backfill.from.map((f: any) => f.session).sort(), ['ses-13', 'ses-14']);
  // ses-15 has one token and no transcript in the tree; it is not considered.
  assert.equal(usage[2], rows[2], 'a non-zero row is passed through by identity');
  // The caller's array is never mutated — the report echoes its inputs.
  assert.equal(rows[0].input_tokens, 0);
  assert.equal(usage[0].input_tokens, 300);
  assert.equal(usage[0].source, 'transcript-backfill');
});

// ---------------------------------------------------------------------------
// Group totals and rendering.
// ---------------------------------------------------------------------------

test('group totals: per-file wake census, independent of any PR selection', () => {
  assert.equal(REPORT.group.files.length, 1);
  const f = REPORT.group.files[0];
  assert.equal(f.rows, 29);
  assert.equal(f.orchestrator_wakes, 8);
  assert.equal(f.prompt_typed_to_orchestrator, 0);
  assert.deepEqual(f.wakes_by_kind, {
    'delegate-progress': 1, 'delegate-done': 1, 'reviewer-report': 1,
    'delegate-blocked': 2, 'verdict-notice': 1, 'system-notice': 1, other: 1,
  });
  // The per-file totals must sum to the per-file wake count, or a class is missing.
  const summed = Object.values(f.wakes_by_kind).reduce((a: number, b: any) => a + b, 0);
  assert.equal(summed, f.orchestrator_wakes);
});

test('--cut reproduces a historical measurement on a log that has since grown', () => {
  const cut = JSON.parse(runScorecard(['--cut', '1700000500000']));
  const f = cut.group.files[0];
  assert.equal(f.rows, 21, 'rows after the cut instant are dropped');
  assert.ok(f.rows < REPORT.group.files[0].rows);
  // The cut lands between the first verdict and the hand-back, so #900 keeps one
  // round and loses the rest — a non-zero survivor, so this is not the vacuous
  // 'everything is gone' reading of a cut.
  assert.equal(cut.prs.find((c: any) => c.pr === 900).review.rounds, 1);
  assert.equal(cut.prs.find((c: any) => c.pr === 900).driver.lane_spawns, 3);
  assert.equal(cut.prs.find((c: any) => c.pr === 900).driver.hand_backs, 0);
  assert.equal(cut.prs.find((c: any) => c.pr === 900).driver.satisfied, 0);
  // ... and the transcript is cut too: only the +100 s turn survives.
  assert.equal(cut.prs.find((c: any) => c.pr === 900).orchestrator.tokens_window.turns, 1);
});

test('the GFM table renders one row per PR with the counters the benchmark quotes', () => {
  const table = sc.renderPrTable(REPORT.prs);
  const lines = table.split('\n');
  assert.equal(lines.length, 2 + REPORT.prs.length);
  assert.ok(lines[0].startsWith('| PR |') && lines[0].endsWith('|'));
  assert.match(lines[1], /^\|(-{3}\|)+$/);
  // Every row has the same cell count as the header — a mismatch renders as a broken
  // table on github.com rather than failing anywhere.
  const cells = (l: string) => l.split('|').length;
  for (const l of lines) assert.equal(cells(l), cells(lines[0]));
  const row900 = lines.find((l) => l.startsWith('| #900 '))!;
  assert.match(row900, /\| 3 \| 1 \| 3 \| 1 \|/); // lane spawns, hand-backs, refused, held
  assert.match(row900, /\| 2 \(3\) \|/);          // corrected notices (S5's own count)
  const row902 = lines.find((l) => l.startsWith('| #902 '))!;
  assert.match(row902, /\| — \| — \|/);           // no loop window
});

test('the CLI prints usage instead of throwing when given nothing', () => {
  const out = execFileSync(process.execPath, [scriptPath], { encoding: 'utf8' });
  assert.match(out, /orch-scorecard/);
  assert.match(out, /--audit/);
  assert.match(out, /--transcript/);
});

// ---------------------------------------------------------------------------
// #2011 A — the CLI axis, the per-lane columns, and the opencode-vs-pi table.
//
// The axis is READ off the `usage.json` row's `source`, with the `agent-spawn`
// row's `cli` as a second rung and `unknown` as the third outcome. Everything
// below pins that it is read and never guessed: no assertion here is satisfied
// by a scorecard that fills a blank in from a block's declared CLI.
// ---------------------------------------------------------------------------

const CLI_TABLE = path.join(fixtures, 'clitable');
const SPLIT_AT = '2026-09-06T10:36:55Z';

function runCliTable(extraArgs: string[] = []): string {
  return execFileSync(process.execPath, [
    scriptPath,
    '--audit', path.join(CLI_TABLE, 'audit.jsonl'),
    '--usage', path.join(CLI_TABLE, 'usage.json'),
    '--agents', path.join(CLI_TABLE, 'agents.json'),
    '--pr-meta', path.join(CLI_TABLE, 'pr-meta.json'),
    '--all', '--no-backfill',
    ...extraArgs,
  ], { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 });
}

const CLI_REPORT = JSON.parse(runCliTable());
const cliCard = (pr: number) => CLI_REPORT.prs.find((c: any) => c.pr === pr);

test('cli: the source label decides, one map, and everything else is `unknown`', () => {
  // The four labels loomux writes per CLI (group-cost-tracking.md), plus the
  // script's own backfill label, which names a CLAUDE transcript folded off disk.
  assert.equal(sc.cliForSource('transcript'), 'claude');
  assert.equal(sc.cliForSource('transcript-backfill'), 'claude');
  assert.equal(sc.cliForSource('pi-transcript'), 'pi');
  assert.equal(sc.cliForSource('session-db'), 'opencode');
  assert.equal(sc.cliForSource('codex-transcript'), 'codex');
  // The two labels that name no CLI, and the three shapes of absence. A
  // statusline scrape says a CLI printed a dollar figure, never which one.
  assert.equal(sc.cliForSource('statusline'), 'unknown');
  assert.equal(sc.cliForSource('none'), 'unknown');
  assert.equal(sc.cliForSource(undefined), 'unknown');
  assert.equal(sc.cliForSource(null), 'unknown');
  assert.equal(sc.cliForSource('a-source-that-does-not-exist-yet'), 'unknown');
  // NEGATIVE CONTROL for the whole map: it is not a function that answers
  // `claude` to everything, and `unknown` is not a value it never returns.
  assert.equal(new Set(Object.values(sc.SOURCE_TO_CLI)).size, 4);
  assert.equal(sc.CLI_UNKNOWN, 'unknown');
});

test('cli: two rows disagreeing under one key resolve to `mixed`, never to the last one folded', () => {
  assert.equal(sc.resolveCli(new Set(['claude'])), 'claude');
  assert.equal(sc.resolveCli(new Set(['unknown'])), 'unknown');
  assert.equal(sc.resolveCli(new Set([])), 'unknown');
  // An `unknown` beside a real one does not make the answer ambiguous — nothing
  // was learned from it.
  assert.equal(sc.resolveCli(new Set(['unknown', 'pi'])), 'pi');
  assert.equal(sc.resolveCli(new Set(['pi', 'opencode'])), 'mixed');
});

test('cli: the spawn-row rung answers only where the source did not, and says so', () => {
  const spawnOnly = { get: (a: string) => (a === 'w-1' ? 'opencode' : undefined) };
  // The usage source wins outright on an UNSHARED session: it names the record
  // the collector actually folded, where the spawn row names only what was
  // launched.
  assert.deepEqual(sc.resolveDelegateCli('pi', 'w-1', spawnOnly, false),
    { cli: 'pi', cli_via: 'usage-source' });
  // The spawn-row rung fires only on `unknown`.
  assert.deepEqual(sc.resolveDelegateCli('unknown', 'w-1', spawnOnly, false),
    { cli: 'opencode', cli_via: 'spawn-row' });
  // And the last outcome is REPORTED, never a guess filled in from a roster.
  assert.deepEqual(sc.resolveDelegateCli('unknown', 'w-2', spawnOnly, false),
    { cli: 'unknown', cli_via: null });
  assert.deepEqual(sc.resolveDelegateCli(undefined, 'w-2', spawnOnly, false),
    { cli: 'unknown', cli_via: null });
});

test('cli: on a session shared across CLIs the PER-AGENT spawn row wins, and says which', () => {
  const spawn = { get: (a: string) => (a === 'w-1' ? 'opencode' : undefined) };
  // A per-session `source` cannot be right for two CLIs at once, so where the
  // occupants disagree the per-agent record is preferred — and it is its own
  // rung, never folded into either of the others.
  assert.deepEqual(sc.resolveDelegateCli('claude', 'w-1', spawn, true),
    { cli: 'opencode', cli_via: 'spawn-row-session-conflict' });
  // NEGATIVE CONTROL: the SAME inputs without the conflict answer 'claude'. If
  // this were equal to the line above, the flag would be doing nothing.
  assert.deepEqual(sc.resolveDelegateCli('claude', 'w-1', spawn, false),
    { cli: 'claude', cli_via: 'usage-source' });
  // A conflicted session with no spawn row for THIS agent falls through the
  // normal ladder rather than inventing an answer.
  assert.deepEqual(sc.resolveDelegateCli('claude', 'w-2', spawn, true),
    { cli: 'claude', cli_via: 'usage-source' });
});

test('cli: `indexCliConflicts` finds only the sessions whose occupants disagree', () => {
  const spawn = new Map([['a1', 'claude'], ['a2', 'opencode'], ['b1', 'pi'], ['b2', 'pi'], ['c1', 'claude']]);
  const conflicts = sc.indexCliConflicts([
    { id: 'a1', session: 's-a' }, { id: 'a2', session: 's-a' },   // disagree
    { id: 'b1', session: 's-b' }, { id: 'b2', session: 's-b' },   // agree
    { id: 'c1', session: 's-c' },                                  // alone
    { id: 'd1', session: 's-d' },                                  // no spawn row
    { id: 'e1' },                                                  // no session
  ], spawn);
  // POSITIVE CONTROL plus the negative one in a single assertion: exactly the
  // disagreeing session, and none of the four that do not.
  assert.deepEqual([...conflicts.keys()], ['s-a']);
  assert.deepEqual([...conflicts.get('s-a').entries()].sort(), [['a1', 'claude'], ['a2', 'opencode']]);
});

test('cli: `indexSpawnCli` reads the delegate site and counts the rows that carry none', () => {
  const idx = sc.indexSpawnCli([
    { action: 'agent-spawn', detail: { agent: 'w-1', cli: 'opencode' }, ts_ms: 1 },
    // A respawn: the LAST row wins, because the live pane wrote the tokens.
    { action: 'agent-spawn', detail: { agent: 'w-1', cli: 'pi' }, ts_ms: 2 },
    // The orchestrator site carries no `cli` at all — counted, never invented.
    { action: 'agent-spawn', detail: { agent: 'orch-1', role: 'orchestrator' }, ts_ms: 3 },
    { action: 'prompt', detail: { agent: 'w-9', cli: 'claude' }, ts_ms: 4 },
  ]);
  assert.equal(idx.byAgent.get('w-1'), 'pi');
  assert.equal(idx.byAgent.has('orch-1'), false);
  // A non-spawn row carrying a `cli` is not a spawn record and is not read.
  assert.equal(idx.byAgent.has('w-9'), false);
  assert.equal(idx.spawn_rows_with_cli, 2);
  assert.equal(idx.spawn_rows_without_cli, 1);
});

test('cli: every delegate on the shared corpus carries a cli and the rung that answered', () => {
  const byAgent = new Map<string, any>();
  for (const c of REPORT.prs) for (const d of c.delegates.agents) byAgent.set(d.agent, d);
  // All three rungs have a subject, so no assertion here passes on a corpus
  // where one rung is unreachable.
  assert.deepEqual(
    [...byAgent.entries()].sort().map(([a, d]) => [a, d.cli, d.cli_via]),
    [
      ['rev-11', 'pi', 'usage-source'],          // `pi-transcript`
      ['rev-11-prev', 'pi', 'usage-source'],     // same session, same row
      ['rev-12', 'unknown', null],               // `statusline`, and no spawn row
      ['w-13', 'claude', 'usage-source'],        // `transcript`
      ['w-14', 'opencode', 'spawn-row'],         // `statusline`, spawn row says opencode
    ],
  );
});

test('cli: a `statusline` row lands in `unknown`, never in a CLI, when no spawn row answers', () => {
  const rev12 = card(900).delegates.agents.find((a: any) => a.agent === 'rev-12');
  assert.equal(rev12.cli, 'unknown');
  assert.equal(rev12.cli_via, null);
  // It still carries its tokens: an unknown CLI is an unknown AXIS, not a
  // dropped delegate — the tokens are real and are counted somewhere.
  assert.ok(rev12.tokens > 0);
  assert.equal(card(900).delegates.by_block_cli['rev-final/unknown'].tokens, rev12.tokens);
  // NEGATIVE CONTROL: `unknown` is not what this corpus answers for everything.
  assert.equal(card(900).delegates.agents.find((a: any) => a.agent === 'w-13').cli, 'claude');
});

test('by_block_cli: one bucket per `block/cli`, keyed off the rows and not a roster', () => {
  assert.deepEqual(card(900).delegates.by_block_cli, {
    'rev-std/pi': { block: 'rev-std', cli: 'pi', tokens: 2140, tokens_credited: 1070, count: 2 },
    'rev-final/unknown': { block: 'rev-final', cli: 'unknown', tokens: 2140, tokens_credited: 1070, count: 1 },
    'worker-adv/claude': { block: 'worker-adv', cli: 'claude', tokens: 3210, tokens_credited: 3210, count: 1 },
  });
  // The buckets re-sum to the card's own delegate total, so the split cannot
  // lose or duplicate a delegate's credit.
  const sum = Object.values(card(900).delegates.by_block_cli)
    .reduce((n: number, b: any) => n + b.tokens_credited, 0);
  assert.equal(sum, card(900).delegates.tokens.total);
  // NEGATIVE CONTROL: the PR named by nothing has no buckets at all.
  assert.deepEqual(card(902).delegates.by_block_cli, {});
});

test('by_block_cli: two clis in ONE block on ONE PR stay apart', () => {
  // #820's two `worker-std` panes ran different CLIs. Keying on the block alone
  // would fold them into one bucket and the switch would be invisible.
  const b = cliCard(820).delegates.by_block_cli;
  assert.equal(b['worker-std/opencode'].count, 1);
  assert.equal(b['worker-std/pi'].count, 1);
  assert.notEqual(b['worker-std/opencode'].tokens_credited, 0);
  assert.equal(b['worker-std/opencode'].block, b['worker-std/pi'].block);
});

test('lanes: `rounds_to_pass` is the first pass, and `null` where the lane never passed', () => {
  // #801's rev-std went fail, fail, pass.
  assert.deepEqual(cliCard(801).review.lanes['rev-std'], {
    rounds: 3, pass: 1, fail: 2, verdicts_other: 0, rounds_to_pass: 3, fail_rate: 0.67,
  });
  // A lane that passed first time is 1, not 0 — the count is 1-based rounds.
  assert.equal(cliCard(802).review.lanes['rev-std'].rounds_to_pass, 1);
  assert.equal(cliCard(802).review.lanes['rev-std'].fail_rate, 0);
  // #901's only verdict is `escalate`: neither a pass nor a fail, so
  // `rounds_to_pass` is null (never the round count) and `fail_rate` is null
  // (never 0 — "a clean lane" and "no lane" are different facts).
  assert.deepEqual(card(901).review.lanes['rev-final'], {
    rounds: 1, pass: 0, fail: 0, verdicts_other: 1, rounds_to_pass: null, fail_rate: null,
  });
});

test('lanes: the sequence is read in order, so a later fail cannot lower rounds_to_pass', () => {
  // Built directly rather than through a fixture, because the point is the
  // ORDER and the audit corpus would fix one ordering forever.
  assert.deepEqual(sc.laneStats({ a: ['fail', 'pass', 'fail', 'pass'] }).a, {
    rounds: 4, pass: 2, fail: 2, verdicts_other: 0, rounds_to_pass: 2, fail_rate: 0.5,
  });
  // Reversed, the same multiset answers 1 — so the function reads the sequence
  // and not the bucket, which is exactly what `review.by_block` cannot tell you.
  assert.equal(sc.laneStats({ a: ['pass', 'fail', 'fail', 'pass'] }).a.rounds_to_pass, 1);
  assert.equal(sc.laneStats({ a: ['fail', 'fail'] }).a.rounds_to_pass, null);
  assert.equal(sc.laneStats({ a: [] }).a.fail_rate, null);
});

test('wall_clock_h is the PR window span, and `null` — never 0 — with no window', () => {
  assert.equal(card(900).wall_clock_h, card(900).windows.pr.span_h);
  assert.equal(cliCard(801).wall_clock_h, 4);
  assert.equal(cliCard(803).wall_clock_h, 8);
  // The negative control: #902 is named by nothing, so it has no window at all.
  assert.equal(card(902).wall_clock_h, null);
  assert.equal(card(902).windows.pr, null);
});

test('statCell: a median needs n>=3, and `n` is reported at every size', () => {
  assert.equal(sc.MEDIAN_MIN_N, 3);
  // Below the floor the cell is null and n still says how thin it was.
  for (const xs of [[], [1], [1, 2]]) {
    const c = sc.statCell(xs);
    assert.equal(c.median, null, `n=${xs.length} must not produce a median`);
    assert.equal(c.q1, null);
    assert.equal(c.iqr, null);
    assert.equal(c.n, xs.length);
  }
  // At the floor it resolves. Odd n: the middle element is excluded from both
  // halves (the Tukey-hinge convention the comment states).
  assert.deepEqual(sc.statCell([1, 2, 3]),
    { n: 3, dropped: 0, median: 2, q1: 1, q3: 3, iqr: 2, min: 1, max: 3 });
  // Even n: the median is the mean of the middle pair.
  assert.equal(sc.statCell([1, 2, 3, 4]).median, 2.5);
  // Unsorted input sorts; a null or non-numeric input is DROPPED and counted,
  // so a lane with no answer shrinks n instead of reading as a zero.
  assert.equal(sc.statCell([9, 1, 5]).median, 5);
  const dropped = sc.statCell([1, 2, 3, null, undefined, NaN]);
  assert.equal(dropped.n, 3);
  assert.equal(dropped.dropped, 3);
  assert.equal(dropped.median, 2, 'a null must not be read as 0 and drag the median down');
});

test('cli-table: the pi side is null below n=3 while the opencode side resolves', () => {
  const t = sc.cliTable(CLI_REPORT.prs, Date.parse(SPLIT_AT));
  const row = (k: string) => t.rows.find((r: any) => r.key === k);
  assert.deepEqual(row('opencode (pre-2817)').prs, [800, 801, 802]);
  assert.deepEqual(row('pi (post-2817)').prs, [810, 811]);
  // The floor bites per CELL on the thin side and nowhere on the thick one, so
  // this is not the vacuous "everything is null" reading.
  for (const c of t.columns) {
    assert.equal(row('pi (post-2817)').cells[c.key].median, null, c.key + ' must be null at n=2');
    assert.equal(row('pi (post-2817)').cells[c.key].n, 2);
    assert.notEqual(row('opencode (pre-2817)').cells[c.key].median, null, c.key + ' must resolve at n=3');
  }
  assert.equal(row('opencode (pre-2817)').cells.wall_clock_h.median, 4);
  assert.equal(row('opencode (pre-2817)').cells.rev_rounds_to_pass.median, 2);
});

test('cli-table: #802 is on the opencode side ONLY because the spawn-row rung answered', () => {
  // Its rev-std usage row is `statusline`. Without rung 2 its rev-std lane would
  // resolve to `unknown`, the PR would be excluded, and the opencode side would
  // fall to n=2 — where every cell reads null. This is what makes the fallback
  // load-bearing rather than decorative.
  const rev802 = cliCard(802).delegates.agents.find((a: any) => a.agent === 'rev-802');
  assert.equal(rev802.cli_via, 'spawn-row');
  assert.equal(rev802.cli, 'opencode');
  const t = sc.cliTable(CLI_REPORT.prs, Date.parse(SPLIT_AT));
  assert.equal(t.rows.find((r: any) => r.key === 'opencode (pre-2817)').prs.length, 3);
  // …and with that one delegate's cli erased, the side really does collapse.
  const blinded = JSON.parse(JSON.stringify(CLI_REPORT.prs));
  for (const c of blinded) {
    for (const d of c.delegates.agents) {
      if (d.agent !== 'rev-802') continue;
      d.cli = 'unknown';
      d.cli_via = null;
    }
  }
  const t2 = sc.cliTable(blinded, Date.parse(SPLIT_AT));
  assert.equal(t2.rows.find((r: any) => r.key === 'opencode (pre-2817)').prs.length, 2);
  assert.equal(t2.rows.find((r: any) => r.key === 'opencode (pre-2817)').cells.wall_clock_h.median, null);
  assert.ok(t2.excluded.some((e: any) => e.pr === 802));
});

test('cli-table: selection is stated, and every exclusion says which bound refused it', () => {
  const t = sc.cliTable(CLI_REPORT.prs, Date.parse(SPLIT_AT));
  const why = (pr: number) => (t.excluded.find((e: any) => e.pr === pr) || {}).why;
  assert.match(why(820), /worker-std split across opencode\+pi/);
  assert.match(why(821), /no merged_at/);
  assert.match(why(822), /worker-std pi but rev-std opencode/);
  // Selected plus excluded is every PR the run scored: nothing leaves silently.
  assert.equal(t.per_pr.length + t.excluded.length, CLI_REPORT.prs.length);
  assert.ok(t.per_pr.length > 0, 'positive control: the selection is not empty');
});

test('cli-table: the side and the cli are resolved independently, and a disagreement is reported', () => {
  const t = sc.cliTable(CLI_REPORT.prs, Date.parse(SPLIT_AT));
  // #803 merged BEFORE the split and its lanes resolve to pi. The table does not
  // reconcile that away — it files the PR by the clock and flags the mismatch.
  assert.deepEqual(t.side_cli_disagreements,
    [{ pr: 803, side: 'pre-2817', cli: 'pi', expected: 'opencode' }]);
  assert.equal(t.per_pr.find((p: any) => p.pr === 803).side, 'pre-2817');
  assert.equal(t.per_pr.find((p: any) => p.pr === 803).cli, 'pi');
  // NEGATIVE CONTROL: the other five agree, so the list is not everything.
  assert.equal(t.per_pr.length, 6);
});

test('cli-table: the rows are read off the data, not off a hardcoded opencode/pi pair', () => {
  // Rewrite every cli to one nothing in this script knows about. A table with a
  // built-in roster would render two empty named rows; this one renders one row
  // called what the rows say.
  const relabelled = JSON.parse(JSON.stringify(CLI_REPORT.prs));
  for (const c of relabelled) {
    for (const d of c.delegates.agents) if (d.cli !== 'unknown') d.cli = 'fictional-cli';
    for (const [k, b] of Object.entries<any>(c.delegates.by_block_cli)) {
      if (b.cli === 'unknown') continue;
      delete c.delegates.by_block_cli[k];
      b.cli = 'fictional-cli';
      c.delegates.by_block_cli[b.block + '/fictional-cli'] = b;
    }
  }
  const t = sc.cliTable(relabelled, Date.parse(SPLIT_AT));
  assert.deepEqual(t.rows.map((r: any) => r.key).sort(),
    ['fictional-cli (post-2817)', 'fictional-cli (pre-2817)']);
  // …and every one of them disagrees with the expected side, which is the
  // cross-check firing rather than staying quiet on an unfamiliar label.
  assert.equal(t.side_cli_disagreements.length, t.per_pr.length);
});

test('cli-table: the confounder block is printed by the script, not left to the poster', () => {
  const rendered = runCliTable(['--format', 'cli-table', '--split-at', SPLIT_AT]);
  // The measurement is worthless without the reasons not to over-read it, so
  // they travel with the table rather than being remembered into a comment.
  for (const id of ['effective-thinking-level', 'task-mix', 'driver-waste-changes',
    'driver-waste-measurement', 'cumulative-usage-rows']) {
    assert.ok(rendered.includes(id), `the ${id} confounder must print under the table`);
  }
  // Every issue the plan names as a confounder is cited.
  for (const n of [2938, 2501, 2507, 2508, 2509, 2812]) {
    assert.match(rendered, new RegExp('#' + n + '(?![0-9])'), `#${n} must be cited`);
  }
  // #2938's effective level is REPORTED as unknown, never guessed at a value.
  assert.match(rendered, /Effective level as read today: `unknown`/);
  assert.ok(rendered.includes('93d51cc9'), 'the split commit is named');
  assert.ok(rendered.includes('2026-09-06T10:36:55'), 'the split instant is named');
});

test('cli-table: the GFM renders one row per group with the header cell count', () => {
  const rendered = runCliTable(['--format', 'cli-table', '--split-at', SPLIT_AT]);
  const lines = rendered.split('\n').filter((l) => l.startsWith('|'));
  const cells = (l: string) => l.split('|').length;
  // A row that disagrees with the header renders as a broken table on
  // github.com and fails nowhere else.
  for (const l of lines) assert.equal(cells(l), cells(lines[0]));
  assert.equal(lines.length, 2 + sc.cliTable(CLI_REPORT.prs, Date.parse(SPLIT_AT)).rows.length);
  assert.match(lines[1], /^\|(-{3}\|)+$/);
  // A resolved cell shows median, IQR and n; a refused one shows null and n.
  assert.match(rendered, /\(IQR .+?, n=3\)/);
  assert.match(rendered, /null \(n=2\)/);
});

test('cli-table: --format cli-table refuses to run without a split instant', () => {
  assert.throws(() => runCliTable(['--format', 'cli-table']), /--split-at/);
  // …and a bad one is refused too, rather than silently filing every PR on one
  // side (Date.parse of nonsense is NaN, and NaN comparisons are all false).
  assert.throws(() => runCliTable(['--format', 'cli-table', '--split-at', 'not-a-date']), /--split-at/);
});

test('coverage: the cli axis reports its own population, counted per delegate slot', () => {
  const cov = REPORT.coverage.cli_axis;
  assert.equal(cov.delegate_slots, REPORT.prs.reduce((n: number, c: any) => n + c.delegates.agents.length, 0));
  // A slot is a delegate ON A CARD, so `rev-12` — attributed to both #900 and
  // #901 — is two slots. That is the point of counting at the verified site:
  // the axis is used once per slot, not once per agent.
  assert.deepEqual(cov.by_rung, { 'usage-source': 3, 'spawn-row': 1, 'spawn-row-session-conflict': 0, none: 2 });
  // The shared corpus has no cross-CLI session, so this is the empty reading —
  // the populated one is pinned on the clitable corpus below.
  assert.deepEqual(cov.sessions_with_conflicting_clis, []);
  assert.deepEqual(cov.by_cli, { pi: 2, unknown: 2, claude: 1, opencode: 1 });
  assert.equal(cov.unknown, 2);
  // The rungs partition the slots — a slot cannot be answered by two rungs, and
  // one answered by none is exactly the `unknown` count.
  assert.equal(Object.values<number>(cov.by_rung).reduce((a, b) => a + b, 0), cov.delegate_slots);
  assert.equal(cov.by_rung.none, cov.unknown);
});

test('coverage: H10 is declared with a statement and its structural fix', () => {
  const h10 = REPORT.coverage.heuristics.find((h: any) => h.id === 'H10');
  assert.ok(h10, 'the cli axis is a guess and must be declared as one');
  assert.match(h10.what, /source/);
  assert.match(h10.what, /statusline/);
  assert.match(h10.fix, /UsageSnapshot/);
});

test('cli: a pane recycled across CLIs keeps #800 on the opencode side', () => {
  // #800's `worker-std` pane shares its session with a claude `worker-adv` one,
  // and the row's source is `transcript`. Rung 1 alone would call that worker
  // claude, #800's lanes would stop resolving to one cli, and the opencode side
  // would fall to n=2 — where every cell reads null. So this is what makes the
  // conflict rung load-bearing rather than decorative.
  const w800 = cliCard(800).delegates.agents.find((a: any) => a.agent === 'w-800');
  assert.equal(w800.cli, 'opencode');
  assert.equal(w800.cli_via, 'spawn-row-session-conflict');
  // The other occupant of that same session resolves the other way, off its own
  // spawn row — one session, two answers, which is the whole point.
  const wadv = cliCard(800).delegates.agents.find((a: any) => a.agent === 'wadv-800');
  assert.equal(wadv.cli, 'claude');
  assert.equal(wadv.cli_via, 'spawn-row-session-conflict');
  // Coverage names the session and the split, so the defect is surfaced rather
  // than silently repaired.
  assert.deepEqual(CLI_REPORT.coverage.cli_axis.sessions_with_conflicting_clis,
    [{ session: 'ses-w-800', agents: ['w-800=opencode', 'wadv-800=claude'] }]);
  // …and the side really does collapse without it.
  const blinded = JSON.parse(JSON.stringify(CLI_REPORT.prs));
  for (const c of blinded) {
    for (const d of c.delegates.agents) {
      if (d.agent !== 'w-800') continue;
      d.cli = 'claude';                       // what rung 1 alone would have said
      const b = c.delegates.by_block_cli['worker-std/opencode'];
      delete c.delegates.by_block_cli['worker-std/opencode'];
      b.cli = 'claude';
      c.delegates.by_block_cli['worker-std/claude'] = b;
    }
  }
  const t = sc.cliTable(blinded, Date.parse(SPLIT_AT));
  assert.deepEqual(t.rows.find((r: any) => r.key === 'opencode (pre-2817)').prs, [801, 802]);
  assert.equal(t.rows.find((r: any) => r.key === 'opencode (pre-2817)').cells.wall_clock_h.median, null);
});
