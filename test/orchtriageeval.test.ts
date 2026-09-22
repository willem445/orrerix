// `scripts/orch-triage-eval.cjs` — the delivery-triage replay harness (#3304 S2).
//
// WHAT THIS PINS, and why each pin can fail.
//
// The harness's value rests on two claims, and a test that does not make them
// fail-able leaves the eval measuring itself:
//
//   1. IT REPLAYS S1's RULES, not a second classifier that happens to agree
//      today. The script carries a JS mirror of `crates/loomux-engine/src/
//      triage.rs` — unavoidably, because the rules are hand-written prefix
//      tests over `&str` and there is no data an engine export could hand a
//      Node script. So the mirror is pinned twice, and the two pins are blind
//      in different places:
//        - `test/fixtures/orchtriage/vectors.json` is asserted here AND by
//          `crates/loomux-engine/tests/triage_vectors.rs`. Behaviour that
//          diverges reddens on whichever side moved. Vectors are blind to a
//          Rust rule that has no vector.
//        - the VOCABULARY SCAN below reads `triage.rs` itself and asserts the
//          mirror's tables are set-equal to Rust's wire spellings, so a rule
//          added or renamed in Rust reddens here with no vector involved.
//      Residual, stated rather than left to be found: a change to the BODY of
//      a Rust rule that keeps its name and is covered by no vector is invisible
//      to both. That is what the "every rule is exercised by a vector"
//      assertion (here and in the Rust test) bounds.
//
//   2. ITS METRICS DISCRIMINATE. The synthetic corpus in
//      `test/fixtures/orchtriage/` is built so no two headline counters share a
//      value (#1182): 21 deliveries, 8 rule defers, 2 provider defers, 3 false
//      defers, 2 wasted wakes, 19 of 21 labelled, 5 calibration samples. A
//      fixture whose axes were all one constant could not tell a working
//      counter from a broken one. The corpus carries the case #3304's plan
//      names by hand — ts 19000, labelled `decision`, given a confident `fyi`
//      verdict — and the test asserts it lands in FALSE DEFERS rather than
//      anywhere softer.
//
// The negative control for every "the tier saved N wakes" assertion is the
// `--triage-disabled` run: 0 deferred, 0 false defers, on the same corpus.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const require_ = createRequire(import.meta.url);
const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..');
const scriptPath = path.join(root, 'scripts', 'orch-triage-eval.cjs');
const fixtures = path.join(here, 'fixtures', 'orchtriage');

const ev = require_(scriptPath) as any;

const vectors = JSON.parse(readFileSync(path.join(fixtures, 'vectors.json'), 'utf8'));
const agents = JSON.parse(readFileSync(path.join(fixtures, 'agents.json'), 'utf8'));
const auditRows = ev.readJsonl(path.join(fixtures, 'audit-synth.jsonl'), null, null).rows;
const synthLabels = ev.parseLabels(readFileSync(path.join(fixtures, 'labels-synth.csv'), 'utf8'));
const synthVerdicts = JSON.parse(readFileSync(path.join(fixtures, 'verdicts-synth.json'), 'utf8'));

const roles = ev.indexRoles(agents);
const POLICY = { enabled: true, kinds: [], merge_queue_enabled: true };

function population(dropKickoff = true) {
  return ev.deliveries(auditRows, roles, dropKickoff);
}

// ---------------------------------------------------------------------------
// 1a. The cross-language golden vectors.
// ---------------------------------------------------------------------------

test('the mirror answers every golden vector exactly as the fixture says', () => {
  const cases = vectors.cases as any[];
  // Positive control: an empty or truncated fixture makes the loop vacuous and
  // a zero-case run reads exactly like a clean one. The Rust side asserts the
  // same floor, so neither reader can be the only one that noticed.
  assert.ok(cases.length >= 20, `expected a real corpus, got ${cases.length}`);

  for (const c of cases) {
    assert.equal(ev.classify(c.text), c.expect.kind, `${c.name}: classify`);
    assert.equal(ev.neverTriaged(c.text, c.human_actor), c.expect.never, `${c.name}: neverTriaged`);
    const got = ev.decide(
      {
        text: c.text,
        from: 'w-1',
        human_actor: c.human_actor,
        merge_queue_enabled: c.merge_queue_enabled,
      },
      { enabled: c.policy.enabled, kinds: c.policy.kinds },
    );
    assert.deepEqual(got, c.expect.decision, `${c.name}: decide`);
  }
});

test('the vector corpus exercises every rule and every deliver reason', () => {
  const cases = vectors.cases as any[];
  const rules = new Set<string>();
  const reasons = new Set<string>();
  for (const c of cases) {
    if (c.expect.decision.rule) rules.add(c.expect.decision.rule);
    if (c.expect.decision.reason) reasons.add(c.expect.decision.reason);
  }
  // `gate-satisfied` is the one rule no vector can name: `decide` answers
  // `try-enqueue` for it and only a successful merge-queue enqueue turns that
  // into a defer, which is impure and cannot live in a fixture. Its witness is
  // the `try-enqueue` case, asserted separately so the carve-out is stated
  // rather than read off a gap.
  for (const r of ev.RULES.filter((r: string) => r !== 'gate-satisfied')) {
    assert.ok(rules.has(r), `no vector exercises rule ${r}`);
  }
  assert.ok(
    cases.some((c) => c.expect.decision.action === 'try-enqueue'),
    'no vector exercises the gate-satisfied / try-enqueue arm',
  );
  for (const r of [...ev.DELIVER_REASONS, ...ev.NEVER_REASONS]) {
    assert.ok(reasons.has(r), `no vector exercises deliver reason ${r}`);
  }
});

test('the three number tests are NOT one rule, because Rust’s three are not', () => {
  // `drive_notice` and `plan_chunk` parse (`u64`/`u32::parse`), which accepts a
  // leading `+` and refuses `-`; `run_completed` tests
  // `chars().all(is_ascii_digit)`, which refuses both. The mirror had assumed
  // one rule for all three, and no vector covered a sign, so neither pin saw it
  // (review round 1, finding 2). Asserted here AND as four golden vectors, so
  // the Rust side confirms this reading of Rust rather than taking my word.
  assert.equal(ev.classify('[orrerix] review drive PR #+42: GATE SATISFIED at a'), 'drive-gate-satisfied');
  assert.equal(ev.prOf('[orrerix] review drive PR #+42: GATE SATISFIED at a'), 42);
  assert.equal(ev.classify('[orrerix] review drive PR #-42: GATE SATISFIED at a'), 'system-notice');
  assert.equal(ev.classify('[orrerix] run +101: completed — conclusion: success.'), 'system-notice');
  assert.equal(ev.classify('[orrerix] run 101: completed — conclusion: success.'), 'run-completed');
  assert.deepEqual(ev.planChunk('---BEGIN PLAN +1/4--- x'), [1, 4]);
  assert.equal(ev.planChunk('---BEGIN PLAN -1/4--- x'), null);
  // The negative control for the whole bullet: `Number()` would accept all of
  // these, so a mirror that used it would pass the positives and fail nothing.
  assert.equal(ev.planChunk('---BEGIN PLAN 0x2/4--- x'), null);
  assert.equal(ev.classify('[orrerix] review drive PR # 42: GATE SATISFIED at a'), 'system-notice');
});

// ---------------------------------------------------------------------------
// 1b. The vocabulary scan — the half the vectors are blind to.
// ---------------------------------------------------------------------------

const TRIAGE_RS = path.join(root, 'crates', 'loomux-engine', 'src', 'triage.rs');

/**
 * Every `X::Variant => "spelling",` arm for one Rust enum.
 *
 * Decided on the SHAPE (`<Enum>::<Variant> => "<literal>"`), never on a
 * binding's name, per CLAUDE.md's source-scanning-guard convention. The
 * spelling is read from inside the quotes rather than matched by an alphabet
 * class, so a future wire name carrying a digit or an underscore is still seen
 * (#1209).
 *
 * STATED BLIND SPOTS, none of which exists in `triage.rs` today: an arm split
 * across lines, a spelling produced by `format!` or `concat!`, and a second
 * `match` over the same enum returning different literals (this takes the union
 * of every arm it finds, so such a match would ADD spellings rather than hide
 * one). The positive control below fails if the scan matches nothing at all.
 */
function scanEnumSpellings(src: string, enumName: string): string[] {
  const re = new RegExp(`${enumName}::\\w+(?:\\s*\\{[^}]*\\})?\\s*=>\\s*"([^"]*)"`, 'g');
  const out = new Set<string>();
  let m: RegExpExecArray | null;
  while ((m = re.exec(src)) !== null) out.add(m[1]);
  return [...out].sort();
}

/** The string literals of a `const NAME: [&str; N] = [ ... ];` array. */
function scanMarkerArray(src: string, name: string): string[] {
  const at = src.indexOf(`const ${name}`);
  assert.notEqual(at, -1, `${name} not found in triage.rs`);
  const open = src.indexOf('[', src.indexOf('=', at));
  const close = src.indexOf('];', open);
  assert.ok(close > open, `${name}: could not bound the array literal`);
  const block = src.slice(open, close);
  return [...block.matchAll(/"([^"]*)"/g)].map((m) => m[1]).sort();
}

test('the mirror’s vocabulary is set-equal to triage.rs’s own wire spellings', () => {
  const src = readFileSync(TRIAGE_RS, 'utf8');

  const kinds = scanEnumSpellings(src, 'Kind');
  const rules = scanEnumSpellings(src, 'Rule');
  const never = scanEnumSpellings(src, 'NeverReason');
  const deliver = scanEnumSpellings(src, 'DeliverReason');

  // POSITIVE CONTROLS. Each of these scans succeeds by producing a list, and a
  // scan that matched nothing produces an EMPTY list — byte-identical to an
  // enum with no variants. Assert a floor per scan before comparing, so a
  // regex that stopped matching reddens here instead of silently agreeing with
  // an emptied mirror.
  assert.ok(kinds.length >= 10, `Kind scan found ${kinds.length} spellings`);
  assert.ok(rules.length >= 5, `Rule scan found ${rules.length} spellings`);
  assert.ok(never.length >= 5, `NeverReason scan found ${never.length} spellings`);
  assert.ok(deliver.length >= 3, `DeliverReason scan found ${deliver.length} spellings`);

  assert.deepEqual(kinds, [...ev.KINDS].sort(), 'Kind::as_str vs the mirror’s KINDS');
  assert.deepEqual(rules, [...ev.RULES].sort(), 'Rule::as_str vs the mirror’s RULES');
  assert.deepEqual(never, [...ev.NEVER_REASONS].sort(), 'NeverReason::as_str vs NEVER_REASONS');
  // `DeliverReason::Never(r) => r.as_str()` forwards rather than naming a
  // literal, so Rust's own arm list is the three standalone spellings; the
  // mirror carries the same three and reaches the rest through neverTriaged.
  assert.deepEqual(deliver, [...ev.DELIVER_REASONS].sort(), 'DeliverReason::as_str vs DELIVER_REASONS');
});

test('the mirror carries triage.rs’s marker arrays verbatim', () => {
  const src = readFileSync(TRIAGE_RS, 'utf8');
  const needsYou = scanMarkerArray(src, 'NEEDS_YOU_MARKERS');
  const regrounding = scanMarkerArray(src, 'REGROUNDING_MARKERS');
  assert.ok(needsYou.length >= 3, `NEEDS_YOU_MARKERS scan found ${needsYou.length}`);
  assert.ok(regrounding.length >= 2, `REGROUNDING_MARKERS scan found ${regrounding.length}`);
  assert.deepEqual(needsYou, [...ev.NEEDS_YOU_MARKERS].sort());
  assert.deepEqual(regrounding, [...ev.REGROUNDING_MARKERS].sort());
});


test('every Kind is exercised by at least one vector', () => {
  // #3322 residual (d). The vocabulary scan proves the two KIND LISTS are
  // set-equal, and `deepEqual` over two sorted arrays passes just as happily
  // when a Kind is added on BOTH sides at once — a bilateral addition with no
  // vector is invisible to every pin this file had. Rules and deliver reasons
  // already carry this assertion; kinds did not.
  const cases = vectors.cases as any[];
  const seen = new Set(cases.map((c) => c.expect.kind));
  // POSITIVE CONTROL: the set is built from the fixture, so a fixture that
  // failed to load would produce an empty set and the loop below would still
  // report the first missing kind — but a fixture whose `expect.kind` key were
  // renamed would produce a set of `undefined` and say nothing useful.
  assert.ok(seen.size >= 10, `the vector corpus names ${seen.size} kinds`);
  for (const k of ev.KINDS) assert.ok(seen.has(k), `no vector exercises kind ${k}`);
});

test('the mirror carries the #3324 marker arrays verbatim too', () => {
  const src = readFileSync(TRIAGE_RS, 'utf8');
  const green = scanMarkerArray(src, 'GREEN_PATH_MARKERS');
  const silent = scanMarkerArray(src, 'SILENT_EXIT_MARKERS');
  assert.ok(green.length >= 4, `GREEN_PATH_MARKERS scan found ${green.length}`);
  assert.ok(silent.length >= 1, `SILENT_EXIT_MARKERS scan found ${silent.length}`);
  assert.deepEqual(green, [...ev.GREEN_PATH_MARKERS].sort());
  assert.deepEqual(silent, [...ev.SILENT_EXIT_MARKERS].sort());
});

test('the green-path scan reads the NOTE, not the verdict around it', () => {
  // The scope is the whole point: `conclusion: success` and `checks: SUCCESS`
  // are GitHub-derived, and a marker matched against them would be reading the
  // verdict rather than the registrant's intent. A notice with NO note must
  // therefore be unaffected however green it reads.
  const noNote = '[orrerix] run 17812: completed — conclusion: success. (watch n-1)';
  assert.equal(ev.registeredNote(noNote), null);
  assert.equal(ev.noteNamesGreenPath(noNote), false);
  assert.deepEqual(ev.decide({ text: noNote, from: 'orrerix', human_actor: false, merge_queue_enabled: true }, POLICY), {
    action: 'defer',
    rule: 'run-green',
  });
  // And a note is read to its LAST quote, so a note containing one truncates
  // the slice rather than escaping it — in the DELIVER direction only.
  const quoted = '[orrerix] run 1: completed — conclusion: success. Note (registered): "he said "if green" to me" (watch n-1)';
  assert.equal(ev.noteNamesGreenPath(quoted), true);
});

test('asciiLower is Rust’s to_ascii_lowercase, not JS toLowerCase', () => {
  // #3322 residual (b). `İ` (U+0130) lowercases to `i̇` under Unicode rules and
  // is left ALONE by Rust's ASCII-only fold; a mirror using `toLowerCase`
  // would answer a different question on any text carrying one.
  assert.equal(ev.asciiLower('İSTANBUL'), 'İstanbul');
  assert.notEqual(ev.asciiLower('İSTANBUL'), 'İSTANBUL'.toLowerCase());
  assert.equal(ev.asciiLower('IS YOURS'), 'is yours');
});

test('a PARTIAL hand-label overlap is declared, not printed as a full scorecard', () => {
  // #3322 residual (c). Only the ZERO-overlap case was guarded: a run whose
  // audit had rotated away half the labelled generation printed an ordinary
  // scorecard, and `labelled N of population M` says nothing about how much of
  // the LABEL FILE that N is.
  const pop = population();
  const result = ev.replay(pop.deliveries, POLICY, {});
  const meta = { group: 'synth-1', audit_files: ['a.jsonl'], dropped_kickoffs: 1, window: '' };

  // Full coverage: every label row landed. No caveat.
  const full = ev.renderMarkdown({ result, scored: ev.score(result, synthLabels.labels), sweepRows: [], meta });
  assert.ok(!/PARTIAL LABEL COVERAGE/.test(full), 'a fully-covered run must not cry partial');

  // Partial: the same labels plus rows naming timestamps this population has
  // no delivery for — exactly what a rotation leaves behind.
  const widened = new Map(synthLabels.labels);
  for (const ts of [900001, 900002, 900003, 900004]) widened.set(ts, { kind: '', label: 'decision' });
  const partial = ev.renderMarkdown({ result, scored: ev.score(result, widened), sweepRows: [], meta });
  assert.match(partial, /PARTIAL LABEL COVERAGE — 21 of the label file's 25 rows \(84\.0 %\)/);
  assert.match(partial, /SAMPLE of the hand set/);
  // And the caveat must not be a soothing footnote on a zero: it has to sit
  // with the false-defer line it qualifies.
  assert.ok(partial.indexOf('PARTIAL LABEL COVERAGE') < partial.indexOf('FALSE DEFERS'));
});

// ---------------------------------------------------------------------------
// 2. The population, and the kickoff proxy.
// ---------------------------------------------------------------------------

test('the population is prompt rows to an ORCHESTRATOR pane, kickoff proxy applied', () => {
  const pop = population();
  assert.equal(pop.populationBeforeDrop, 24, 'prompt rows to orch-1 (the w-1 row is excluded)');
  assert.equal(pop.droppedKickoffs, 1);
  assert.equal(pop.deliveries.length, 23);
  // The row addressed to a worker pane must not be in it — a harness that read
  // every prompt row would count 23 and inflate every saving below.
  assert.ok(!pop.deliveries.some((d: any) => d.ts_ms === 23000), 'a delivery to w-1 leaked in');
  assert.ok(!pop.deliveries.some((d: any) => d.ts_ms === 1000), 'the kickoff proxy row leaked in');
});

test('--no-drop-kickoff keeps the first delivery per pane, and the count says so', () => {
  const pop = population(false);
  assert.equal(pop.droppedKickoffs, 0);
  assert.equal(pop.deliveries.length, 24);
  assert.ok(pop.deliveries.some((d: any) => d.ts_ms === 1000));
});

// ---------------------------------------------------------------------------
// 3. The replay counters.
// ---------------------------------------------------------------------------

test('the rule tier defers exactly the eight rule hits, per rule', () => {
  const r = ev.replay(population().deliveries, POLICY, {});
  assert.equal(r.total, 23);
  assert.equal(r.rule_deferred, 8);
  assert.equal(r.provider_deferred, 0);
  assert.equal(r.delivered, 15);
  assert.deepEqual(
    Object.fromEntries(Object.entries(r.by_rule).map(([k, v]: any) => [k, v.deferred])),
    {
      'run-green': 2,
      'checks-green': 1,
      'planner-exited': 1,
      'agent-exited': 1,
      'drive-cancelled': 1,
      'plan-chunk': 1,
      'gate-satisfied': 1,
    },
  );
  // The optimistic TryEnqueue resolution is COUNTED, not hidden: a reader who
  // wants the pessimistic figure subtracts it, and cannot do that if the
  // harness folds it into `gate-satisfied` silently.
  assert.equal(r.assumed_enqueues, 1);
});

test('a repo with no merge queue has no gate-satisfied rule at all', () => {
  const r = ev.replay(population().deliveries, { ...POLICY, merge_queue_enabled: false }, {});
  assert.equal(r.by_rule['gate-satisfied'], undefined);
  assert.equal(r.assumed_enqueues, 0);
  assert.equal(r.rule_deferred, 7);
});

test('triage disabled is the negative control: nothing is deferred', () => {
  const r = ev.replay(population().deliveries, { ...POLICY, enabled: false }, {});
  assert.equal(r.deferred, 0);
  assert.equal(r.delivered, 23);
  assert.equal(ev.score(r, synthLabels.labels).false_defers, 0);
  assert.deepEqual(Object.keys(r.by_rule), []);
});

// ---------------------------------------------------------------------------
// 4. Scoring — the false-defer floor is the point of the whole harness.
// ---------------------------------------------------------------------------

test('a delivery labelled decision and deferred is a FALSE DEFER, whichever tier held it', () => {
  const provider = new ev.FakeTriage(synthVerdicts);
  const r = ev.replay(population().deliveries, POLICY, { provider, floor: 0.85 });
  const s = ev.score(r, synthLabels.labels);

  assert.equal(s.labelled, 21, 'two rows are deliberately unlabelled');
  assert.equal(s.population, 23, 'and the report must state both numbers');
  assert.equal(s.false_defers, 3);
  assert.deepEqual(s.false_defer_by_reason, {
    'rule:agent-exited': 1,
    'rule:gate-satisfied': 1,
    'provider:fyi': 1,
  });
  // The case #3304's plan names by hand: labelled `decision`, given a
  // CONFIDENT `fyi` verdict, and therefore held back. It must be counted as
  // the harmful error and not as a disagreement, a low-confidence miss, or
  // anything else softer.
  const row = s.false_defer_rows.find((x: any) => x.ts_ms === 19000);
  assert.ok(row, 'the decision-labelled / fyi-verdict row is not in false_defer_rows');
  assert.equal(row.tier, 'provider');
  assert.equal(row.provider_class, 'fyi');
  assert.equal(row.action, 'defer');

  assert.equal(s.wasted_wakes, 2);
  assert.equal(s.agreement, 16 / 21);
});

test('agreement is scored on the BINARY, so a four-way class disagreement is not an error', () => {
  // ts 17000 is labelled `routing` and the provider says `routing`; ts 20000 is
  // labelled `routing` and is delivered because its confidence is under the
  // floor. Both are audit-only; only the second is a wasted wake. A harness
  // scoring the four-way class would call the first a hit and the second a
  // miss for the wrong reason.
  const provider = new ev.FakeTriage(synthVerdicts);
  const r = ev.replay(population().deliveries, POLICY, { provider, floor: 0.85 });
  const s = ev.score(r, synthLabels.labels);
  assert.deepEqual(s.confusion.routing, { deferred: 1, delivered: 1 });
  assert.deepEqual(s.confusion.escalation, { deferred: 0, delivered: 0 });
});

test('an empty label set scores nothing rather than scoring perfectly', () => {
  const r = ev.replay(population().deliveries, POLICY, {});
  const s = ev.score(r, new Map());
  assert.equal(s.labelled, 0);
  assert.equal(s.agreement, null, 'an unscored run must not read as 100 %');
  assert.equal(s.false_defers, 0);
  assert.equal(s.calibration.ece, null, 'an ECE over zero samples is undefined, not 0');
});

// ---------------------------------------------------------------------------
// 5. The provider seam.
// ---------------------------------------------------------------------------

test('only the rule tier’s no-rule residual is ever shown to a provider', () => {
  const provider = new ev.FakeTriage(synthVerdicts);
  ev.replay(population().deliveries, POLICY, { provider, floor: 0.85 });
  // 15 delivered, of which 7 carry a never-triaged or policy reason (held,
  // blocked, watchdog, regrounding, human-actor, and two needs-you — the
  // second is #3324's `is yours` on the gate notice at ts 11000) and are
  // excluded by construction — that exclusion is what makes "a human's words
  // are never sent to a classifier" a property of the code rather than of the
  // rule table happening not to match them.
  assert.equal(provider.calls, 8);
});

test('a provider that returns no verdict delivers — the fail-safe, not a defer', () => {
  const provider = new ev.FakeTriage({});
  const r = ev.replay(population().deliveries, POLICY, { provider, floor: 0.85 });
  assert.equal(r.provider_calls, 8);
  assert.equal(r.provider_no_verdict, 8);
  assert.equal(r.provider_deferred, 0);
});

test('a malformed verdict is a no-verdict, not a trusted one', () => {
  const provider = new ev.FakeTriage({
    '17000': { class: 'not-a-class', confidence: 0.99 },
    '19000': { class: 'fyi', confidence: 1.4 },
    '20000': { class: 'fyi' },
  });
  assert.equal(provider.classify({ ts_ms: 17000 }), null, 'an unknown class must not be trusted');
  assert.equal(provider.classify({ ts_ms: 19000 }), null, 'a confidence outside 0..1 must not be trusted');
  assert.equal(provider.classify({ ts_ms: 20000 }), null, 'a missing confidence must not be trusted');
});

test('decision and escalation are delivered at ANY confidence', () => {
  for (const cls of ['decision', 'escalation']) {
    assert.equal(ev.providerAction({ class: cls, confidence: 1 }, 0.5), 'deliver', cls);
  }
  assert.equal(ev.providerAction({ class: 'routing', confidence: 1 }, 0.5), 'defer');
  assert.equal(ev.providerAction({ class: 'routing', confidence: 0.49 }, 0.5), 'deliver');
  assert.equal(ev.providerAction(null, 0.5), 'deliver');
});

// ---------------------------------------------------------------------------
// 6. Calibration and the floor sweep.
// ---------------------------------------------------------------------------

test('reliability bins are population-weighted and an empty bin contributes nothing', () => {
  const cal = ev.reliability([
    { confidence: 0.62, correct: true },
    { confidence: 0.88, correct: true },
    { confidence: 0.95, correct: true },
    { confidence: 0.93, correct: true },
    { confidence: 0.91, correct: false },
  ]);
  assert.equal(cal.n, 5);
  assert.equal(cal.bins[6].n, 1);
  assert.equal(cal.bins[8].n, 1);
  assert.equal(cal.bins[9].n, 3);
  assert.equal(cal.bins[0].n, 0);
  assert.equal(cal.bins[0].acc, undefined, 'an empty bin has no accuracy, not 0');
  // (1/5)|1-.62| + (1/5)|1-.88| + (3/5)|2/3-.93|
  assert.ok(Math.abs(cal.ece - 0.258) < 0.001, `ECE ${cal.ece}`);
});

test('a confidence of exactly 1.0 lands in the last bin rather than off the end', () => {
  const cal = ev.reliability([{ confidence: 1, correct: true }]);
  assert.equal(cal.bins[9].n, 1);
  assert.equal(cal.n, 1);
});

test('the floor sweep moves, and the harmful error is reported per floor', () => {
  const provider = new ev.FakeTriage(synthVerdicts);
  const rows = ev.sweep(population().deliveries, POLICY, provider, synthLabels.labels, [0.6, 0.85, 0.95]);
  assert.deepEqual(
    rows.map((r: any) => [r.floor, r.provider_deferred, r.false_defers, r.wasted_wakes]),
    [
      [0.6, 3, 3, 1],
      [0.85, 2, 3, 2],
      [0.95, 0, 2, 3],
    ],
  );
  // The rule tier is re-run at every floor rather than reused, so a table that
  // silently held it constant would be right by accident.
  assert.deepEqual(new Set(rows.map((r: any) => r.rule_deferred)), new Set([8]));
});

// ---------------------------------------------------------------------------
// 7. The label file.
// ---------------------------------------------------------------------------

test('the label parser skips comments and the header and reads the rest', () => {
  const parsed = ev.parseLabels(
    ['# a note', '', 'ts_ms,kind,label', '100,run-completed,fyi', '200,delegate-done,routing'].join('\n'),
  );
  assert.deepEqual(parsed.problems, []);
  assert.equal(parsed.labels.size, 2);
  assert.deepEqual(parsed.labels.get(100), { kind: 'run-completed', label: 'fyi' });
});

test('the label parser REFUSES a bad row rather than dropping it silently', () => {
  const parsed = ev.parseLabels(
    [
      '100,run-completed,maybe',
      '2e3,run-completed,fyi',
      '300,x,fyi',
      '300,x,routing',
      '400',
      '500,x,y,fyi',
    ].join('\n'),
  );
  assert.equal(parsed.labels.size, 1, 'only the first 300 row is kept');
  assert.equal(parsed.problems.length, 5);
  assert.ok(parsed.problems.some((p: string) => p.includes('"maybe"')), 'an unknown label class');
  assert.ok(parsed.problems.some((p: string) => p.includes('"2e3"')), 'a non-integer ts_ms');
  assert.ok(parsed.problems.some((p: string) => p.includes('duplicate')), 'a duplicate ts_ms');
  assert.equal(
    parsed.problems.filter((p: string) => p.includes('expected ts_ms')).length,
    2,
    'a one-column row AND a four-column row are both refused',
  );
});

test('the two-column ts_ms,label form the slice names is accepted, kind being DERIVED', () => {
  // #3304's plan slice specifies `ts_ms,label`; the shipped set is
  // `ts_ms,kind,label`. `kind` is `classify`'s own answer carried for grouping,
  // never a labelled field, so a two-column file is COMPLETE and refusing one
  // would refuse the format the slice names (review round 1, finding 1).
  const two = ev.parseLabels(['ts_ms,label', '100,decision', '200,fyi'].join('\n'));
  assert.deepEqual(two.problems, []);
  assert.equal(two.labels.size, 2);
  assert.deepEqual(two.labels.get(100), { kind: '', label: 'decision' });

  // And the two forms must agree on the LABEL, which is the only field scored —
  // a test that only checked the two-column form parses would not catch a
  // reader taking the label from the wrong column.
  const three = ev.parseLabels(['ts_ms,kind,label', '100,run-completed,decision'].join('\n'));
  assert.equal(three.labels.get(100).label, two.labels.get(100).label);
  assert.equal(three.labels.get(100).kind, 'run-completed');
});

test('--emit-labels prints the RESIDUAL as fillable CSV, and nothing else', () => {
  const result = ev.replay(population().deliveries, POLICY, {});
  const csv = ev.emitLabelTemplate(result.rows);
  const rows = csv
    .split('\n')
    .filter((l: string) => l && !l.startsWith('#') && !l.startsWith('ts_ms'));

  // Exactly the `no-rule` residual: not the rule-closed deliveries, and not
  // the never-triaged ones — a template covering those would ask for labels
  // nothing reads, and would put a human's own words in front of a labeller.
  const residual = result.rows.filter((r: any) => r.action === 'deliver' && r.reason === 'no-rule');
  assert.equal(rows.length, residual.length);
  assert.equal(rows.length, 8, 'the synthetic corpus has eight residual deliveries');
  assert.deepEqual(
    rows.map((l: string) => Number(l.split(',')[0])),
    residual.map((r: any) => r.ts_ms),
  );

  // Every row is `ts_ms,kind,` with the label column EMPTY and ready to fill.
  for (const l of rows) {
    const cols = l.split(',');
    assert.equal(cols.length, 3, `not ts_ms,kind,label: ${l}`);
    assert.equal(cols[2], '', `the label column must be empty: ${l}`);
    assert.ok(ev.KINDS.includes(cols[1]), `derived kind is not a triage kind: ${l}`);
  }

  // The rubric rides in the header: a label set whose rubric lives elsewhere is
  // a label set whose second labeller answered a different question.
  for (const cls of ev.PROVIDER_CLASSES) assert.match(csv, new RegExp(`\\b${cls}\\b`));
  assert.match(csv, /NO DELIVERY TEXT IS EMITTED/);

  // The emitted file must round-trip through the parser once filled — the two
  // halves are a contract, not two independent formats.
  const filled = csv.replace(/^(\d+,[a-z-]+),$/gm, '$1,fyi');
  const parsed = ev.parseLabels(filled);
  assert.deepEqual(parsed.problems, []);
  assert.equal(parsed.labels.size, residual.length);
});

test('--emit-labels exits without printing a report', () => {
  const { code, out } = runCli([
    '--audit', path.join(fixtures, 'audit-synth.jsonl'),
    '--agents', path.join(fixtures, 'agents.json'),
    '--emit-labels',
  ]);
  assert.equal(code, 0);
  assert.match(out, /^# Hand-label template/);
  assert.ok(!/FALSE DEFERS/.test(out), "a labeller's file must not carry a report");
  assert.ok(!/Deferred \d+ \//.test(out), "nor a headline");
});

test('the shipped hand-label set parses, and is the size it claims', () => {
  const text = readFileSync(path.join(fixtures, 'labels-loomux-68435179.csv'), 'utf8');
  const parsed = ev.parseLabels(text);
  assert.deepEqual(parsed.problems, []);
  assert.equal(parsed.labels.size, 150, 'the PR body and the design note both quote 150');
  const counts: Record<string, number> = {};
  for (const v of parsed.labels.values()) counts[v.label] = (counts[v.label] || 0) + 1;
  assert.deepEqual(counts, { decision: 86, routing: 36, fyi: 24, escalation: 4 });
  // No delivery text may be checked in beside the labels: the rows are a live
  // group's agent-authored prose. Every non-comment line is exactly three
  // comma-separated fields, so a fourth column carrying text cannot creep in.
  for (const line of text.split('\n')) {
    const t = line.trim();
    if (!t || t.startsWith('#') || t.startsWith('ts_ms')) continue;
    assert.equal(t.split(',').length, 3, `label row is not ts_ms,kind,label: ${t}`);
  }
});

// ---------------------------------------------------------------------------
// 8. The report.
// ---------------------------------------------------------------------------

test('the markdown report states the denominator and names the false-defer floor', () => {
  const provider = new ev.FakeTriage(synthVerdicts);
  const pop = population();
  const result = ev.replay(pop.deliveries, POLICY, { provider, floor: 0.85 });
  const scored = ev.score(result, synthLabels.labels);
  const md = ev.renderMarkdown({
    result,
    scored,
    sweepRows: ev.sweep(pop.deliveries, POLICY, provider, synthLabels.labels, [0.85]),
    meta: { group: 'synth-1', audit_files: ['a.jsonl'], dropped_kickoffs: 1, window: '' },
  });
  assert.match(md, /\*\*21\*\* of 23 deliveries carry a hand label/);
  assert.match(md, /\*\*FALSE DEFERS: 3\*\*/);
  assert.match(md, /Kickoff proxy dropped \*\*1\*\*/);
  // The rotation caveat is printed by the REPORT, not left to whoever pastes
  // it: `audit.jsonl` rotates, so a population figure is a snapshot and a
  // reader re-running the command later cannot otherwise tell drift from
  // breakage (review round 1, finding 3).
  assert.match(md, /Population figures are a snapshot of a rotating artifact/);
  // The optimistic TryEnqueue resolution must be disclosed in the report, not
  // only in the JSON: the markdown is what gets pasted onto an issue.
  assert.match(md, /resolved optimistically/);
  // A table's rows must survive as a table — a blank line inside one ends it
  // and the rows render as literal pipes (#926).
  const rulesTable = md.slice(md.indexOf('### Per rule'), md.indexOf('### Per kind'));
  assert.ok(!/\|\n\n\|/.test(rulesTable), 'a blank line splits the per-rule table');
});

test('an unscored report says so instead of printing a clean bill', () => {
  const result = ev.replay(population().deliveries, POLICY, {});
  const md = ev.renderMarkdown({
    result,
    scored: ev.score(result, new Map()),
    sweepRows: [],
    meta: { group: 'synth-1', audit_files: ['a.jsonl'], dropped_kickoffs: 1, window: '' },
  });
  assert.match(md, /No label overlapped this population/);
  assert.ok(!/FALSE DEFERS: 0/.test(md), 'an unscored run must not print a zero false-defer floor');
});

// ---------------------------------------------------------------------------
// 9. The CLI.
// ---------------------------------------------------------------------------

function runCli(args: string[]) {
  let out = '';
  const code = ev.main(args, { write: (s: string) => { out += s; } });
  return { code, out };
}

test('the CLI replays the synthetic corpus end to end', () => {
  const { code, out } = runCli([
    '--audit', path.join(fixtures, 'audit-synth.jsonl'),
    '--agents', path.join(fixtures, 'agents.json'),
    '--labels', path.join(fixtures, 'labels-synth.csv'),
    '--verdicts', path.join(fixtures, 'verdicts-synth.json'),
    '--group', 'synth-1',
  ]);
  assert.equal(code, 0);
  assert.match(out, /Deferred 10 \/ 23/);
  assert.match(out, /\*\*FALSE DEFERS: 3\*\*/);
  assert.match(out, /ECE 0\.258/);
});

test('the CLI refuses an unknown kind and a malformed label file', () => {
  const bad = runCli([
    '--audit', path.join(fixtures, 'audit-synth.jsonl'),
    '--agents', path.join(fixtures, 'agents.json'),
    '--kinds', 'delegate-done,not-a-kind',
  ]);
  assert.equal(bad.code, 2);
  assert.match(bad.out, /not a triage kind/);

  const noArgs = runCli([]);
  assert.equal(noArgs.code, 2);
  assert.match(noArgs.out, /--audit PATH/);
});

test('--no-provider ignores a verdict file, so a rules-only figure is reproducible', () => {
  const { code, out } = runCli([
    '--audit', path.join(fixtures, 'audit-synth.jsonl'),
    '--agents', path.join(fixtures, 'agents.json'),
    '--labels', path.join(fixtures, 'labels-synth.csv'),
    '--verdicts', path.join(fixtures, 'verdicts-synth.json'),
    '--no-provider',
  ]);
  assert.equal(code, 0);
  assert.match(out, /Deferred 8 \/ 23/);
  assert.match(out, /\*\*FALSE DEFERS: 2\*\*/);
});
