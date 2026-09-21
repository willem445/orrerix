#!/usr/bin/env node
'use strict';
// `scripts/orch-triage-eval.cjs` — the REPLAY evaluation harness for delivery
// triage (#3304 S2; replay-class eval per #2011 §3a).
//
// WHAT THIS ANSWERS. #3304 S1 shipped a rule tier that decides, for every
// orchestrator-bound mid-session delivery, deliver-or-defer. A rule tier that
// defers the wrong notice costs a stalled group, and one that defers nothing
// costs ~1.3 M cache-read tokens per avoidable wake. Neither number is
// knowable from reading the rules: they are properties of the TRAFFIC. So this
// script replays a group's own `audit.jsonl` through the rule table and reports
//
//   - per rule and per kind: how many deliveries it deferred and delivered;
//   - the projected wake saving, in wakes and in cache-read tokens;
//   - against a HAND-LABEL set: agreement, a per-class confusion matrix, and
//     the FALSE-DEFER floor — the count of deliveries a human labelled
//     needs-orchestrator that the tier (or a provider) would have held back.
//     That is the only harmful error, and it is reported on its own.
//
// It also carries the PROVIDER seam #3304 S3 needs: a `TriageProvider` is an
// object with `classify(request) -> {class, confidence}`, the residual the rule
// tier does not close is fanned out to it, and `FakeTriage` replays verdicts
// from a fixture. No network call exists anywhere in this file — S3 adds the
// first one, behind this same seam, and the sweep it must pass is the
// `--floors` table below.
//
// ---------------------------------------------------------------------------
// THE RULES ARE RUST'S. THIS IS A MIRROR, AND THE MIRROR IS PINNED.
// ---------------------------------------------------------------------------
//
// S1's rule table lives in `crates/loomux-engine/src/triage.rs` as hand-written
// prefix tests over `&str` — it is CODE, not data, so there is nothing an
// engine `--dump-rules` flag could export that this script could then execute.
// (A dump of the class and rule NAMES would export the vocabulary and leave the
// decisions behind, which is the half that can silently diverge.) The seam is
// therefore a mirror plus two pins, and BOTH are needed because each is blind
// where the other sees:
//
//   1. CROSS-LANGUAGE GOLDEN VECTORS. `test/fixtures/orchtriage/vectors.json`
//      holds delivery cases with their expected `classify` / `decide` answers.
//      `crates/loomux-engine/tests/triage_vectors.rs` asserts Rust agrees with
//      the file; `test/orchtriageeval.test.ts` asserts this mirror agrees with
//      the same file. One file, two languages, one CI run: a behavioural
//      divergence reddens on whichever side moved. Vectors cannot see a rule
//      that exists in Rust and has no vector.
//   2. A VOCABULARY SCAN. `test/orchtriageeval.test.ts` parses `triage.rs` for
//      every `Kind`, `Rule`, `NeverReason` and `DeliverReason` wire spelling
//      and both marker arrays, and asserts this file's tables are set-equal to
//      them, AND that every rule and never-reason is exercised by at least one
//      vector. A rule added, renamed or dropped in Rust reddens here even
//      though no vector mentions it.
//
// The scan is textual and its blind spots are stated in the test. What it
// cannot cover at all is a change to the BODY of a Rust rule that keeps its
// name and is not covered by a vector; that is what pin 1 plus the
// exercised-by-a-vector assertion bound, and the residual is disclosed in
// `doc/design/delivery-triage.md` §"The eval harness".
//
// ---------------------------------------------------------------------------
// AUDIT-READING CONVENTIONS — inherited from `scripts/orch-scorecard.cjs`.
// ---------------------------------------------------------------------------
//
// Population: a `prompt` row whose `detail.to` is an agent whose `agents.json`
// role is `orchestrator` — `orchestration-evals.md` §4.1's definition of a
// wake, unchanged. Rows are read from every `--audit` file given, oldest first,
// and `--from` / `--cut` bound the window the same way.
//
// ONE PROXY, AND IT IS THE HARNESS'S LARGEST KNOWN ERROR. S1's gate keys on
// `Delivery::MidSession`, so a KICKOFF is out of scope — but a `prompt` audit
// row carries only `{text, to}` and no delivery kind, so the replay cannot read
// which it was. It drops the FIRST prompt row per orchestrator pane id instead,
// which is right exactly when a pane's first delivery is its kickoff (it is,
// for every pane in the census: a pane is spawned with one). It is wrong for a
// pane whose kickoff predates the window `--from` opens, where it drops one
// real mid-session delivery. `--no-drop-kickoff` turns it off, and the report
// prints the count either way so the error is bounded and visible rather than
// assumed away. A `delivery` field on the `prompt` row would remove the proxy;
// that is a product change and is filed as a limitation, not made here.

const fs = require('node:fs');
const path = require('node:path');

// ---------------------------------------------------------------------------
// The mirror of `crates/loomux-engine/src/triage.rs`.
//
// Every function below is a transliteration of the like-named Rust one, and
// the ORDER of the tests inside `classify` is load-bearing there and here: it
// is the tie-break. Keep the two readable side by side — a clever JS rewrite
// of a Rust prefix test is a divergence waiting to be undetectable.
// ---------------------------------------------------------------------------

/** `triage.rs`'s `PREFIX`. */
const PREFIX = '[orrerix] ';

/** `Kind::as_str` — the wire spellings, in `Kind::ALL` order. */
const KINDS = [
  'delegate-progress',
  'delegate-done',
  'reviewer-report',
  'delegate-blocked',
  'drive-gate-satisfied',
  'drive-held',
  'drive-cancelled',
  'run-completed',
  'pr-checks',
  'message-from',
  'planner-exited',
  'agent-exited',
  'watchdog',
  'system-notice',
  'human',
];

/** `Rule::as_str`. */
const RULES = [
  'gate-satisfied',
  'run-green',
  'checks-green',
  'planner-exited',
  'agent-exited',
  'drive-cancelled',
  'plan-chunk',
];

/** `NeverReason::as_str`. */
const NEVER_REASONS = ['human-actor', 'regrounding', 'held', 'blocked', 'watchdog', 'needs-you'];

/** `DeliverReason::as_str`, minus the `Never(r)` arm which forwards a NeverReason. */
const DELIVER_REASONS = ['disabled', 'kind-not-triaged', 'no-rule'];

/** `triage.rs`'s `NEEDS_YOU_MARKERS`. */
const NEEDS_YOU_MARKERS = [
  'blocking on you',
  'needs you',
  'needs your',
  'you must rule',
  'your call',
  'decision is yours',
];

/** `triage.rs`'s `REGROUNDING_MARKERS`. */
const REGROUNDING_MARKERS = ['context was compacted', 'orchestration restored', 're-grounding'];

/** `triage.rs`'s `body`. */
function body(text) {
  return text.startsWith(PREFIX) ? text.slice(PREFIX.length) : null;
}

/** `triage.rs`'s `after_agent_token`. */
function afterAgentToken(rest, verb) {
  const at = rest.indexOf(' ');
  if (at < 0) return null;
  const tok = rest.slice(0, at);
  const tail = rest.slice(at + 1);
  if (tok.length === 0 || /\s/.test(tok)) return null;
  return tail.startsWith(verb) ? tail.slice(verb.length) : null;
}

/** `triage.rs`'s `drive_notice` — `review drive PR #N: ` -> `[pr, tail]`. */
function driveNotice(rest) {
  const head = 'review drive PR #';
  if (!rest.startsWith(head)) return null;
  const after = rest.slice(head.length);
  const at = after.indexOf(':');
  if (at < 0) return null;
  const num = after.slice(0, at);
  // Rust parses this one with `u64::parse`, and the three functions in this
  // file do NOT agree on what a number is, so each mirrors its own Rust
  // counterpart rather than a house rule (review round 1, finding 2):
  //
  //   drive_notice  `num.parse::<u64>()`             -> a leading `+` is OK
  //   plan_chunk    `k.parse::<u32>()`               -> a leading `+` is OK
  //   run_completed `chars().all(is_ascii_digit)`    -> digits only
  //
  // `from_str_radix` matches `[b'+', rest @ ..] => (true, rest)` unconditionally
  // and gates `-` behind `is_signed_ty`, so an unsigned parse accepts `+42` and
  // refuses `-42`. `Number()` accepts an empty string, both signs, whitespace,
  // `0x` and exponents, so the test stays explicit in every one of the three.
  if (!/^\+?[0-9]+$/.test(num)) return null;
  return [Number(num), after.slice(at + 1)];
}

/** `triage.rs`'s `run_completed`. */
function runCompleted(rest) {
  if (!rest.startsWith('run ')) return false;
  const tail = rest.slice(4);
  const at = tail.indexOf(':');
  if (at < 0) return false;
  const id = tail.slice(0, at);
  // DIGITS ONLY, and deliberately not the `\+?` the two parse-based mirrors
  // above carry: Rust tests this one with `chars().all(is_ascii_digit)`, which
  // refuses `+42` where `u64::parse` accepts it. Making the three agree here
  // would be the mirror correcting Rust rather than mirroring it.
  return id.length > 0 && /^[0-9]+$/.test(id) && tail.slice(at + 1).startsWith(' completed');
}

/** `triage.rs`'s `classify`. FIRST MATCH WINS, in this order. */
function classify(text) {
  const rest = body(text);
  if (rest === null) return 'human';
  const drive = driveNotice(rest);
  if (drive) {
    const tail = drive[1].replace(/^\s+/, '');
    if (tail.startsWith('GATE SATISFIED')) return 'drive-gate-satisfied';
    if (tail.startsWith('HELD')) return 'drive-held';
    if (tail.startsWith('CANCELLED')) return 'drive-cancelled';
    return 'system-notice';
  }
  if (afterAgentToken(rest, 'reports progress') !== null) return 'delegate-progress';
  if (afterAgentToken(rest, 'reports done') !== null) return 'delegate-done';
  if (
    afterAgentToken(rest, 'reports approved') !== null ||
    afterAgentToken(rest, 'reports request_changes') !== null
  ) {
    return 'reviewer-report';
  }
  if (afterAgentToken(rest, 'reports blocked') !== null) return 'delegate-blocked';
  if (rest.startsWith('message from ')) return 'message-from';
  if (rest.startsWith('watchdog: ')) return 'watchdog';
  if (runCompleted(rest)) return 'run-completed';
  if (rest.startsWith('PR #') && rest.includes(' checks: ')) return 'pr-checks';
  if (rest.startsWith('planner ') && rest.includes('posted its plan and exited')) {
    return 'planner-exited';
  }
  if (rest.startsWith('agent ') && rest.includes(' exited (code ')) return 'agent-exited';
  return 'system-notice';
}

/** `triage.rs`'s `never_triaged` — a NeverReason spelling, or null. */
function neverTriaged(text, humanActor) {
  if (humanActor || body(text) === null) return 'human-actor';
  const lower = text.toLowerCase();
  if (REGROUNDING_MARKERS.some((m) => lower.includes(m))) return 'regrounding';
  const kind = classify(text);
  if (kind === 'drive-held') return 'held';
  if (kind === 'delegate-blocked') return 'blocked';
  if (kind === 'watchdog') return 'watchdog';
  if (NEEDS_YOU_MARKERS.some((m) => lower.includes(m))) return 'needs-you';
  return null;
}

/** `triage.rs`'s `plan_chunk` — `[k, n]`, or null. */
function planChunk(text) {
  const head = '---BEGIN PLAN ';
  const at = text.indexOf(head);
  if (at < 0) return null;
  const rest = text.slice(at + head.length);
  const end = rest.indexOf('---');
  if (end < 0) return null;
  const parts = rest.slice(0, end).trim();
  const slash = parts.indexOf('/');
  if (slash < 0) return null;
  const ks = parts.slice(0, slash).trim();
  const ns = parts.slice(slash + 1).trim();
  // `k.parse::<u32>()` / `n.parse::<u32>()` in Rust — a leading `+` is
  // accepted, a `-` is not. See `driveNotice`'s note for why the three
  // number tests in this file are deliberately not identical.
  if (!/^\+?[0-9]+$/.test(ks) || !/^\+?[0-9]+$/.test(ns)) return null;
  const k = Number(ks);
  const n = Number(ns);
  if (n < 2 || k === 0 || k > n) return null;
  return [k, n];
}

/** `triage.rs`'s `run_is_green` — conclusion only, deliberately NOT the branch. */
function runIsGreen(text) {
  const lower = text.toLowerCase();
  const at = lower.indexOf('conclusion: ');
  if (at < 0) return false;
  return lower.slice(at + 'conclusion: '.length).replace(/^\s+/, '').startsWith('success');
}

/** `triage.rs`'s `checks_are_green`. */
function checksAreGreen(text) {
  const at = text.indexOf(' checks: ');
  if (at < 0) return false;
  return text.slice(at + ' checks: '.length).replace(/^\s+/, '').startsWith('SUCCESS');
}

/** `triage.rs`'s `pr_of`. */
function prOf(text) {
  const rest = body(text);
  if (rest === null) return null;
  const drive = driveNotice(rest);
  return drive ? drive[0] : null;
}

/**
 * `triage.rs`'s `decide`. Returns the Rust `Decision` as a tagged object:
 *   {action: 'deliver', reason}        — Decision::Deliver(reason)
 *   {action: 'defer', rule}            — Decision::Defer(rule)
 *   {action: 'try-enqueue', pr}        — Decision::TryEnqueue { pr }
 */
function decide(input, policy) {
  if (!policy.enabled) return { action: 'deliver', reason: 'disabled' };
  const never = neverTriaged(input.text, input.human_actor === true);
  if (never !== null) return { action: 'deliver', reason: never };
  const kind = classify(input.text);
  const covers = !policy.kinds || policy.kinds.length === 0 || policy.kinds.includes(kind);
  if (!covers) return { action: 'deliver', reason: 'kind-not-triaged' };
  if (kind === 'drive-gate-satisfied') {
    const pr = prOf(input.text);
    if (input.merge_queue_enabled === true && pr !== null) return { action: 'try-enqueue', pr };
    return { action: 'deliver', reason: 'no-rule' };
  }
  if (kind === 'run-completed' && runIsGreen(input.text)) {
    return { action: 'defer', rule: 'run-green' };
  }
  if (kind === 'pr-checks' && checksAreGreen(input.text)) {
    return { action: 'defer', rule: 'checks-green' };
  }
  if (kind === 'planner-exited') return { action: 'defer', rule: 'planner-exited' };
  if (kind === 'agent-exited') return { action: 'defer', rule: 'agent-exited' };
  if (kind === 'drive-cancelled') return { action: 'defer', rule: 'drive-cancelled' };
  if (kind === 'message-from') {
    const chunk = planChunk(input.text);
    if (chunk && chunk[0] < chunk[1]) return { action: 'defer', rule: 'plan-chunk' };
    return { action: 'deliver', reason: 'no-rule' };
  }
  return { action: 'deliver', reason: 'no-rule' };
}

/**
 * What the REPLAY does with `TryEnqueue`, which the live path resolves by
 * attempting a real merge-queue enqueue.
 *
 * The replay has no queue, so it cannot observe the attempt — and inventing a
 * success would report a saving the live tier may never deliver. It resolves
 * the variant OPTIMISTICALLY (enqueue succeeds -> `Defer(gate-satisfied)`),
 * which is the direction that makes the harness's own headline WORSE rather
 * than better: every false-defer this resolution can manufacture is counted
 * against the tier, and every one it hides would have been a delivery. The
 * report prints the count separately so a reader can subtract it.
 */
function resolveDecision(decision) {
  if (decision.action === 'try-enqueue') {
    return { action: 'defer', rule: 'gate-satisfied', assumed_enqueue: true };
  }
  return decision;
}

// ---------------------------------------------------------------------------
// The provider seam (#3304 S3 plugs in here; nothing in this file calls out).
// ---------------------------------------------------------------------------

/**
 * A TriageProvider classifies ONE residual delivery — a delivery the rule tier
 * left as `deliver: no-rule`, which is the only population S3 may ever send
 * anywhere. Shape:
 *
 *   classify({ts_ms, from, kind, text}) -> {class, confidence} | null
 *
 * `class` is one of the four label classes below; `confidence` is 0..1. `null`
 * means "no verdict" and is treated as DELIVER, the same fail-safe the live
 * tier uses for a timeout, a 429 and an unparseable answer.
 */
const PROVIDER_CLASSES = ['decision', 'routing', 'fyi', 'escalation'];

/**
 * The fake, and the reason it exists rather than a mocked HTTP layer: S3 must
 * be sweepable against this exact label set with no network and no account, so
 * the thing under test is the DECISION RULE (the floor, the fail-safe arcs),
 * not a transport. Verdicts are read from a JSON file keyed by `ts_ms`:
 *
 *   {"1788835411251": {"class": "routing", "confidence": 0.91}, ...}
 *
 * A ts with no entry yields `null` — the no-verdict arm — so a partial verdict
 * file exercises the fail-safe rather than silently shrinking the population.
 */
class FakeTriage {
  constructor(verdicts) {
    this.verdicts = verdicts || {};
    this.calls = 0;
  }

  classify(req) {
    this.calls += 1;
    const v = this.verdicts[String(req.ts_ms)];
    if (!v || typeof v !== 'object') return null;
    if (!PROVIDER_CLASSES.includes(v.class)) return null;
    // A MISSING confidence is a no-verdict, never a zero one. Defaulting it to
    // 0 would return a trusted verdict that happens to sit under every floor —
    // right by accident today, and wrong the moment a caller reads `class`
    // without the floor. The fail-safe is "we did not hear back".
    if (typeof v.confidence !== 'number' || !Number.isFinite(v.confidence)) return null;
    if (!(v.confidence >= 0 && v.confidence <= 1)) return null;
    return { class: v.class, confidence: v.confidence };
  }
}

/**
 * The provider's verdict -> deliver/defer, at a confidence floor.
 *
 * This is S3's fail-safe ladder written once, here, so the sweep measures the
 * rule S3 will ship rather than an approximation of it: no verdict delivers,
 * a sub-floor confidence delivers, and `decision` / `escalation` deliver at any
 * confidence. Only a confident `routing` or `fyi` is held.
 */
function providerAction(verdict, floor) {
  if (!verdict) return 'deliver';
  if (verdict.confidence < floor) return 'deliver';
  if (verdict.class === 'decision' || verdict.class === 'escalation') return 'deliver';
  return 'defer';
}

// ---------------------------------------------------------------------------
// Labels.
// ---------------------------------------------------------------------------

/**
 * The four-way label class collapses to the binary the eval scores on.
 * `doc/design/delivery-triage.md` §"The eval harness" carries the rubric this
 * mapping implements; it is one place rather than two so a report cannot state
 * a binary its own class column contradicts.
 */
const NEEDS_ORCHESTRATOR = new Set(['decision', 'escalation']);

function isNeedsOrchestrator(cls) {
  return NEEDS_ORCHESTRATOR.has(cls);
}

/**
 * A hand-label CSV. `#` opens a comment line; a header row naming `ts_ms` is
 * skipped. Deliberately carries NO delivery text: the labelled rows are a live
 * group's agent-authored prose, and a label set is reproducible from the ts
 * alone against the audit it was cut from.
 *
 * TWO COLUMN FORMS, both accepted, and the reason is not convenience. The
 * shipped set is `ts_ms,kind,label`; #3304's plan slice specifies
 * `ts_ms,label`. `kind` is DERIVED, never labelled — it is `classify`'s own
 * answer, carried so a reader can group the rows without re-reading the audit
 * — so a two-column file is complete, and refusing one would refuse the
 * format the slice names (review round 1, finding 1). The label is always the
 * LAST column; a two-column row's `kind` is recomputed by the caller if it
 * wants one.
 */
function parseLabels(text) {
  const out = new Map();
  const problems = [];
  let lineNo = 0;
  for (const raw of text.split('\n')) {
    lineNo += 1;
    const line = raw.trim();
    if (!line || line.startsWith('#')) continue;
    const cols = line.split(',').map((c) => c.trim());
    if (cols[0] === 'ts_ms') continue;
    if (cols.length < 2 || cols.length > 3) {
      problems.push(`line ${lineNo}: expected ts_ms,label or ts_ms,kind,label`);
      continue;
    }
    const ts = Number(cols[0]);
    if (!Number.isFinite(ts) || !/^[0-9]+$/.test(cols[0])) {
      problems.push(`line ${lineNo}: ts_ms "${cols[0]}" is not an integer`);
      continue;
    }
    const label = cols[cols.length - 1];
    const kind = cols.length === 3 ? cols[1] : '';
    if (!PROVIDER_CLASSES.includes(label)) {
      problems.push(`line ${lineNo}: label "${label}" is not one of ${PROVIDER_CLASSES.join('|')}`);
      continue;
    }
    if (out.has(ts)) {
      problems.push(`line ${lineNo}: duplicate ts_ms ${ts}`);
      continue;
    }
    out.set(ts, { kind, label });
  }
  return { labels: out, problems };
}

/**
 * The hand-label TEMPLATE the slice asks for: the residual, as CSV rows ready
 * to fill in.
 *
 * Emitted rather than described, so a second labeller — the thing standing
 * between this eval and an S3 go/no-go — starts from a file instead of from a
 * `--format json` dump they have to reshape. The population is the RESIDUAL
 * only (the rule tier's `no-rule` arm): a never-triaged delivery is excluded by
 * construction and a rule-closed one needs no judgement, so a template
 * covering them would be asking for labels nothing reads.
 *
 * The rubric rides in the header, because a label set whose rubric lives
 * somewhere else is a label set whose second labeller answered a different
 * question.
 */
function emitLabelTemplate(rows) {
  const L = [
    '# Hand-label template — emitted by `scripts/orch-triage-eval.cjs --emit-labels`.',
    '#',
    '# One row per RESIDUAL delivery (the rule tier left it as deliver: no-rule).',
    '# Fill the empty last column with ONE of: decision | routing | fyi | escalation.',
    '#',
    '# THE RUBRIC — one question per delivery:',
    '#   Does this notice name an action the ORCHESTRATOR must take, which cannot',
    '#   be derived from the notice’s leading SHAPE alone, before the group can',
    '#   proceed?',
    '#',
    '#   decision    yes — a call, ruling, approval, routing choice or tool call the',
    '#               text names. A human line typed into the pane is always a',
    '#               decision: it is an instruction by construction.',
    '#   routing     no — the next step is the standard one for this kind and is',
    '#               readable off the shape.',
    '#   fyi         nothing is asked and nothing waits on a reply.',
    '#   escalation  a HUMAN, not the orchestrator, must decide.',
    '#',
    '# The eval scores the BINARY: needs-orchestrator = decision + escalation;',
    '# audit-only = routing + fyi. `kind` is DERIVED, not labelled — it is',
    '# classify()’s answer, carried so rows can be grouped without re-reading the',
    '# audit. A two-column `ts_ms,label` file is accepted too.',
    '#',
    '# NO DELIVERY TEXT IS EMITTED. Read the rows against the audit this was cut',
    '# from; the timestamps are the join.',
    'ts_ms,kind,label',
  ];
  for (const row of rows) {
    if (row.action !== 'deliver' || row.reason !== 'no-rule') continue;
    L.push(`${row.ts_ms},${row.kind},`);
  }
  return `${L.join('\n')}\n`;
}

// ---------------------------------------------------------------------------
// Loading — `orch-scorecard.cjs`'s `readJsonl`, unchanged in shape.
// ---------------------------------------------------------------------------

function readJsonl(pathname, cutMs, fromMs) {
  const rows = [];
  let parseErrors = 0;
  const text = fs.readFileSync(pathname, 'utf8');
  for (const line of text.split('\n')) {
    if (!line.trim()) continue;
    let row;
    try {
      row = JSON.parse(line);
    } catch {
      parseErrors += 1;
      continue;
    }
    if (fromMs !== null && fromMs !== undefined && typeof row.ts_ms === 'number' && row.ts_ms < fromMs) continue;
    if (cutMs !== null && cutMs !== undefined && typeof row.ts_ms === 'number' && row.ts_ms > cutMs) continue;
    rows.push(row);
  }
  return { path: pathname, rows, parseErrors };
}

/** `agents.json` -> id -> role. Accepts the array form the app writes. */
function indexRoles(agents) {
  const byId = new Map();
  const list = Array.isArray(agents) ? agents : Object.values(agents || {});
  for (const a of list) {
    if (a && typeof a.id === 'string') byId.set(a.id, a.role);
  }
  return byId;
}

/**
 * The population: `prompt` rows to an orchestrator pane, oldest first, with
 * the kickoff proxy applied (see the header).
 */
function deliveries(rows, roles, dropKickoff) {
  const p = rows
    .filter((r) => r && r.action === 'prompt' && r.detail && roles.get(r.detail.to) === 'orchestrator')
    .sort((a, b) => (a.ts_ms || 0) - (b.ts_ms || 0));
  const seen = new Set();
  const out = [];
  let droppedKickoffs = 0;
  for (const r of p) {
    if (dropKickoff && !seen.has(r.detail.to)) {
      seen.add(r.detail.to);
      droppedKickoffs += 1;
      continue;
    }
    out.push({
      ts_ms: r.ts_ms,
      to: r.detail.to,
      from: typeof r.actor === 'string' ? r.actor : '',
      human_actor: r.actor === 'human',
      text: typeof r.detail.text === 'string' ? r.detail.text : '',
    });
  }
  return { deliveries: out, droppedKickoffs, populationBeforeDrop: p.length };
}

// ---------------------------------------------------------------------------
// Calibration.
// ---------------------------------------------------------------------------

/**
 * Reliability bins in 0.1 steps and the Expected Calibration Error over them.
 *
 * `bins[i]` covers confidence in [i/10, (i+1)/10), with 1.0 folded into the
 * last bin so a provider that answers exactly 1.0 is not silently dropped.
 * `acc` is the share of that bin whose verdict AGREED with the hand label on
 * the binary; ECE is the population-weighted mean gap between `acc` and the
 * bin's mean confidence. A bin with no samples contributes nothing and is
 * printed as `-`, never as 0 — an empty bin is not a perfectly calibrated one.
 */
function reliability(samples) {
  const bins = [];
  for (let i = 0; i < 10; i += 1) bins.push({ lo: i / 10, hi: (i + 1) / 10, n: 0, conf: 0, hits: 0 });
  for (const s of samples) {
    let i = Math.floor(s.confidence * 10);
    if (i > 9) i = 9;
    if (i < 0) i = 0;
    bins[i].n += 1;
    bins[i].conf += s.confidence;
    if (s.correct) bins[i].hits += 1;
  }
  let ece = 0;
  const total = samples.length;
  for (const b of bins) {
    if (b.n === 0) continue;
    b.acc = b.hits / b.n;
    b.mean_conf = b.conf / b.n;
    ece += (b.n / total) * Math.abs(b.acc - b.mean_conf);
  }
  return { bins, ece: total === 0 ? null : ece, n: total };
}

// ---------------------------------------------------------------------------
// The replay.
// ---------------------------------------------------------------------------

/** #3304's census figure: cache-read tokens the orchestrator pane re-reads per wake. */
const TOKENS_PER_WAKE = 1_300_000;

function emptyCounts() {
  return { delivered: 0, deferred: 0 };
}

function bump(map, key) {
  if (!map[key]) map[key] = emptyCounts();
  return map[key];
}

/**
 * Replay every delivery through the rule tier, then fan the residual out to a
 * provider if one was given.
 */
function replay(deliveryList, policy, opts) {
  const provider = opts.provider || null;
  const floor = typeof opts.floor === 'number' ? opts.floor : 0.85;
  const byKind = {};
  const byRule = {};
  const byDeliverReason = {};
  const rows = [];
  let ruleDeferred = 0;
  let assumedEnqueues = 0;
  let providerDeferred = 0;
  let providerCalls = 0;
  let providerNoVerdict = 0;

  for (const d of deliveryList) {
    const kind = classify(d.text);
    const raw = decide(
      {
        text: d.text,
        from: d.from,
        human_actor: d.human_actor,
        merge_queue_enabled: policy.merge_queue_enabled,
      },
      policy,
    );
    const resolved = resolveDecision(raw);
    if (resolved.assumed_enqueue) assumedEnqueues += 1;

    const row = {
      ts_ms: d.ts_ms,
      from: d.from,
      kind,
      tier: resolved.action === 'defer' ? 'rule' : 'residual',
      action: resolved.action,
      reason: resolved.action === 'defer' ? `rule:${resolved.rule}` : resolved.reason,
      provider_class: null,
      provider_confidence: null,
    };

    if (resolved.action === 'defer') {
      ruleDeferred += 1;
      bump(byRule, resolved.rule).deferred += 1;
      bump(byKind, kind).deferred += 1;
    } else {
      bump(byDeliverReason, resolved.reason).delivered += 1;
      // The RESIDUAL a provider may ever see is exactly the `no-rule` arm:
      // a never-triaged delivery is excluded by construction (that is what
      // makes "a human's words are never sent to a classifier" a property of
      // the code rather than of the rule table happening not to match), and a
      // `disabled` / `kind-not-triaged` delivery was excluded by the operator.
      const eligible = resolved.reason === 'no-rule';
      if (provider && eligible) {
        providerCalls += 1;
        const verdict = provider.classify({ ts_ms: d.ts_ms, from: d.from, kind, text: d.text });
        if (!verdict) providerNoVerdict += 1;
        row.provider_class = verdict ? verdict.class : null;
        row.provider_confidence = verdict ? verdict.confidence : null;
        if (providerAction(verdict, floor) === 'defer') {
          providerDeferred += 1;
          row.tier = 'provider';
          row.action = 'defer';
          row.reason = `provider:${verdict.class}`;
          bump(byKind, kind).deferred += 1;
        } else {
          bump(byKind, kind).delivered += 1;
        }
      } else {
        bump(byKind, kind).delivered += 1;
      }
    }
    rows.push(row);
  }

  const total = deliveryList.length;
  const deferred = ruleDeferred + providerDeferred;
  return {
    total,
    rule_deferred: ruleDeferred,
    provider_deferred: providerDeferred,
    assumed_enqueues: assumedEnqueues,
    delivered: total - deferred,
    deferred,
    by_kind: byKind,
    by_rule: byRule,
    by_deliver_reason: byDeliverReason,
    provider_calls: providerCalls,
    provider_no_verdict: providerNoVerdict,
    rows,
  };
}

/**
 * Score a replay against the hand labels.
 *
 * Only rows carrying a label are scored, and the report always prints how many
 * that was against the population — an agreement figure over an unstated
 * denominator is not a number a reader can check.
 *
 * `false_defer` is the ONLY harmful error: a delivery the human labelled
 * needs-orchestrator that the replay held back. `wasted_wake` is its cheap
 * counterpart: an audit-only delivery that still woke the pane.
 */
function score(result, labels) {
  const confusion = {};
  for (const c of PROVIDER_CLASSES) confusion[c] = { deferred: 0, delivered: 0 };
  const falseDefers = [];
  const wastedWakes = [];
  let labelled = 0;
  let agree = 0;
  const byRuleFalseDefer = {};
  const calSamples = [];

  for (const row of result.rows) {
    const lab = labels.get(row.ts_ms);
    if (!lab) continue;
    labelled += 1;
    const needs = isNeedsOrchestrator(lab.label);
    const deferredRow = row.action === 'defer';
    confusion[lab.label][deferredRow ? 'deferred' : 'delivered'] += 1;
    // Agreement is on the BINARY: the tier's job is deliver-or-defer, so a
    // four-way class match would be scoring a question the tier never asks.
    if (needs === !deferredRow) agree += 1;
    if (needs && deferredRow) {
      falseDefers.push(row);
      byRuleFalseDefer[row.reason] = (byRuleFalseDefer[row.reason] || 0) + 1;
    }
    if (!needs && !deferredRow) wastedWakes.push(row);
    if (typeof row.provider_confidence === 'number') {
      // "Correct" for calibration is the PROVIDER's own binary answer against
      // the label, independent of the floor — a floor sweep that moved the
      // calibration under it would be measuring the sweep.
      const providerSaysDefer = !isNeedsOrchestrator(row.provider_class);
      calSamples.push({ confidence: row.provider_confidence, correct: providerSaysDefer === !needs });
    }
  }

  return {
    labelled,
    population: result.rows.length,
    agreement: labelled === 0 ? null : agree / labelled,
    confusion,
    false_defers: falseDefers.length,
    false_defer_rows: falseDefers,
    false_defer_by_reason: byRuleFalseDefer,
    wasted_wakes: wastedWakes.length,
    calibration: reliability(calSamples),
  };
}

/**
 * The FLOOR SWEEP — the pass criterion #3304 Q4 states, measured.
 *
 * Per floor: how much of the residual the provider would hold, how many of
 * those were labelled needs-orchestrator (the harmful error), and how many
 * audit-only deliveries still woke the pane. The rule tier is re-run at each
 * floor rather than reused, because a floor changes only the provider arm and
 * a table that silently held the rule arm constant would be right by accident.
 */
function sweep(deliveryList, policy, provider, labels, floors) {
  const out = [];
  for (const floor of floors) {
    const r = replay(deliveryList, policy, { provider, floor });
    const s = score(r, labels);
    out.push({
      floor,
      deferred: r.deferred,
      rule_deferred: r.rule_deferred,
      provider_deferred: r.provider_deferred,
      false_defers: s.false_defers,
      wasted_wakes: s.wasted_wakes,
      agreement: s.agreement,
    });
  }
  return out;
}

// ---------------------------------------------------------------------------
// Report.
// ---------------------------------------------------------------------------

function pct(n, d) {
  if (!d) return '-';
  return `${((100 * n) / d).toFixed(1)} %`;
}

function fmtTokens(n) {
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)} M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)} k`;
  return String(n);
}

function renderMarkdown(ctx) {
  const { result, scored, sweepRows, meta } = ctx;
  const L = [];
  L.push(`## Delivery-triage replay — ${meta.group || 'group'}`);
  L.push('');
  L.push(
    `Population **${result.total}** orchestrator-bound deliveries from ${meta.audit_files.length} audit file(s)` +
      `${meta.window ? `, ${meta.window}` : ''}. ` +
      `Kickoff proxy dropped **${meta.dropped_kickoffs}** first-per-pane rows (see the script header).`,
  );
  L.push('');
  L.push(
    `**Deferred ${result.deferred} / ${result.total} (${pct(result.deferred, result.total)})** — ` +
      `rule tier ${result.rule_deferred}, provider ${result.provider_deferred}. ` +
      `Projected saving **${fmtTokens(result.deferred * TOKENS_PER_WAKE)} cache-read tokens** ` +
      `at ${fmtTokens(TOKENS_PER_WAKE)} per wake (#3304's census figure, not re-measured here).`,
  );
  L.push('');
  // THE REPRODUCIBILITY LINE, printed by the report itself rather than left to
  // whoever pastes it. `audit.jsonl` ROTATES: a generation that falls off is
  // unrecoverable, so every POPULATION-dependent figure above is a snapshot of
  // the log as it stood, and re-running this command later on the same group
  // legitimately prints different numbers. Only the LABEL-CONDITIONED figures
  // below (agreement, the confusion matrix, false defers, wasted wakes) are
  // reproducible from the shipped CSV, and then only while the labelled rows
  // are still in a surviving generation. Without this line a reader re-running
  // the command cannot tell drift from breakage (review round 1, finding 3).
  L.push(
    '> **Population figures are a snapshot of a rotating artifact.** `audit.jsonl` rotates and a ' +
      'dropped generation is unrecoverable, so the population, per-rule and projected-saving rows ' +
      'above are true of the log as read at this moment and need not reproduce later. The ' +
      'label-conditioned rows below reproduce from the shipped label CSV for as long as the ' +
      'labelled timestamps survive in some generation — the report states how many of them it found.',
  );
  L.push('');

  L.push('### Per rule');
  L.push('');
  L.push('| rule | deferred | share of population |');
  L.push('| --- | ---: | ---: |');
  for (const rule of RULES) {
    const c = result.by_rule[rule];
    L.push(`| \`${rule}\` | ${c ? c.deferred : 0} | ${pct(c ? c.deferred : 0, result.total)} |`);
  }
  L.push('');
  if (result.assumed_enqueues > 0) {
    L.push(
      `\`gate-satisfied\` includes **${result.assumed_enqueues}** \`TryEnqueue\` decisions the replay ` +
        'resolved optimistically — the live tier defers one only if the merge-queue enqueue SUCCEEDS, ' +
        'which no replay can observe. Subtract them for the pessimistic reading.',
    );
    L.push('');
  }

  L.push('### Per kind');
  L.push('');
  L.push('| kind | deferred | delivered |');
  L.push('| --- | ---: | ---: |');
  for (const kind of KINDS) {
    const c = result.by_kind[kind];
    if (!c) continue;
    L.push(`| \`${kind}\` | ${c.deferred} | ${c.delivered} |`);
  }
  L.push('');

  L.push('### Why a delivery still woke the pane');
  L.push('');
  L.push('| reason | n |');
  L.push('| --- | ---: |');
  const reasons = Object.keys(result.by_deliver_reason).sort();
  for (const r of reasons) L.push(`| \`${r}\` | ${result.by_deliver_reason[r].delivered} |`);
  L.push('');

  if (scored && scored.labelled > 0) {
    L.push('### Against the hand labels');
    L.push('');
    L.push(
      `**${scored.labelled}** of ${scored.population} deliveries carry a hand label. ` +
        `Agreement on the binary (needs-orchestrator vs audit-only): **${(scored.agreement * 100).toFixed(1)} %**.`,
    );
    L.push('');
    L.push('| hand label | deferred | delivered |');
    L.push('| --- | ---: | ---: |');
    for (const c of PROVIDER_CLASSES) {
      const row = scored.confusion[c];
      const mark = isNeedsOrchestrator(c) ? ' *(needs-orchestrator)*' : '';
      L.push(`| \`${c}\`${mark} | ${row.deferred} | ${row.delivered} |`);
    }
    L.push('');
    L.push(
      `**FALSE DEFERS: ${scored.false_defers}** — a delivery a human labelled needs-orchestrator that ` +
        `the tier held back. This is the only harmful error; the pass criterion (#3304 Q4) is ZERO. ` +
        `Wasted wakes (audit-only, still delivered): ${scored.wasted_wakes}.`,
    );
    L.push('');
    const fdReasons = Object.keys(scored.false_defer_by_reason).sort();
    if (fdReasons.length > 0) {
      L.push('| false-defer by reason | n |');
      L.push('| --- | ---: |');
      for (const r of fdReasons) L.push(`| \`${r}\` | ${scored.false_defer_by_reason[r]} |`);
      L.push('');
    }
    const cal = scored.calibration;
    if (cal.n > 0) {
      L.push(`Calibration over ${cal.n} provider verdicts — **ECE ${cal.ece.toFixed(3)}**.`);
      L.push('');
      L.push('| confidence bin | n | mean conf | accuracy |');
      L.push('| --- | ---: | ---: | ---: |');
      for (const b of cal.bins) {
        if (b.n === 0) {
          L.push(`| ${b.lo.toFixed(1)}–${b.hi.toFixed(1)} | 0 | - | - |`);
        } else {
          L.push(
            `| ${b.lo.toFixed(1)}–${b.hi.toFixed(1)} | ${b.n} | ${b.mean_conf.toFixed(3)} | ${b.acc.toFixed(3)} |`,
          );
        }
      }
      L.push('');
    }
  } else {
    L.push('### Against the hand labels');
    L.push('');
    L.push('No label overlapped this population — nothing is scored. (Agreement, ECE and the');
    L.push('false-defer floor are all undefined here rather than perfect: an empty score is not a');
    L.push('passing one.)');
    L.push('');
  }

  if (sweepRows && sweepRows.length > 0) {
    L.push('### Confidence-floor sweep');
    L.push('');
    L.push('| floor | total deferred | provider deferred | FALSE DEFERS | wasted wakes | agreement |');
    L.push('| ---: | ---: | ---: | ---: | ---: | ---: |');
    for (const r of sweepRows) {
      L.push(
        `| ${r.floor.toFixed(2)} | ${r.deferred} | ${r.provider_deferred} | ${r.false_defers} | ` +
          `${r.wasted_wakes} | ${r.agreement === null ? '-' : (r.agreement * 100).toFixed(1) + ' %'} |`,
      );
    }
    L.push('');
  }
  return L.join('\n');
}

// ---------------------------------------------------------------------------
// CLI.
// ---------------------------------------------------------------------------

const USAGE_TEXT = `
  node scripts/orch-triage-eval.cjs --audit <audit.jsonl> [--audit <audit.1.jsonl>]
      --agents <agents.json> [--labels <labels.csv>] [--verdicts <verdicts.json>]

  Replays a group's orchestrator-bound deliveries through #3304 S1's rule tier
  and reports deferred/delivered counts, the projected wake saving, and — with
  --labels — agreement, a confusion matrix, ECE and the false-defer floor.

  --audit PATH        an audit.jsonl (repeatable; rotated files are separate args)
  --agents PATH       the group's agents.json (decides which pane is the orchestrator)
  --labels PATH       hand labels, "ts_ms,kind,label" CSV with label in
                      decision|routing|fyi|escalation
  --verdicts PATH     FakeTriage verdicts, {"<ts_ms>": {class, confidence}}; supplying
                      this turns the provider tier on
  --floor N           provider confidence floor for the headline run (default 0.85)
  --floors A,B,C      sweep these floors instead of the default 0.50..0.95 by 0.05
  --no-provider       ignore --verdicts; rule tier only
  --kinds A,B         restrict triage to these kinds (default: every kind)
  --no-merge-queue    replay as a repo whose merge_queue is OFF (gate-satisfied delivers)
  --triage-disabled   replay with triage.enabled false — the control: everything delivers
  --emit-labels       print a hand-label CSV template for the RESIDUAL (one row per
                      delivery the rule tier left as no-rule, label column empty,
                      rubric in the header) and exit — for a second labeller
  --no-drop-kickoff   keep the first delivery per orchestrator pane (see the header)
  --from TS           drop audit rows before this epoch-ms
  --cut TS            drop audit rows after this epoch-ms
  --group NAME        a label for the report heading
  --format md|json    default md
  --help
`;

function parseArgs(argv) {
  const opts = {
    audit: [],
    agents: null,
    labels: null,
    verdicts: null,
    floor: 0.85,
    floors: null,
    noProvider: false,
    kinds: [],
    mergeQueue: true,
    enabled: true,
    dropKickoff: true,
    from: null,
    cut: null,
    group: '',
    format: 'md',
    emitLabels: false,
    help: false,
  };
  let i = 0;
  const next = () => {
    i += 1;
    if (i >= argv.length) throw new Error(`${argv[i - 1]} needs a value`);
    return argv[i];
  };
  for (; i < argv.length; i += 1) {
    switch (argv[i]) {
      case '--audit': opts.audit.push(next()); break;
      case '--agents': opts.agents = next(); break;
      case '--labels': opts.labels = next(); break;
      case '--verdicts': opts.verdicts = next(); break;
      case '--floor': opts.floor = Number(next()); break;
      case '--floors': opts.floors = next().split(',').map((s) => Number(s.trim())); break;
      case '--no-provider': opts.noProvider = true; break;
      case '--kinds': opts.kinds = next().split(',').map((s) => s.trim()).filter(Boolean); break;
      case '--no-merge-queue': opts.mergeQueue = false; break;
      case '--triage-disabled': opts.enabled = false; break;
      case '--emit-labels': opts.emitLabels = true; break;
      case '--no-drop-kickoff': opts.dropKickoff = false; break;
      case '--from': opts.from = Number(next()); break;
      case '--cut': opts.cut = Number(next()); break;
      case '--group': opts.group = next(); break;
      case '--format': opts.format = next(); break;
      case '--help':
      case '-h': opts.help = true; break;
      default: throw new Error(`unknown argument ${argv[i]}`);
    }
  }
  return opts;
}

const DEFAULT_FLOORS = [0.5, 0.55, 0.6, 0.65, 0.7, 0.75, 0.8, 0.85, 0.9, 0.95];

function main(argv, out) {
  let opts;
  try {
    opts = parseArgs(argv);
  } catch (e) {
    out.write(`${e.message}\n${USAGE_TEXT}`);
    return 2;
  }
  if (opts.help || opts.audit.length === 0 || !opts.agents) {
    out.write(USAGE_TEXT);
    return opts.help ? 0 : 2;
  }
  for (const k of opts.kinds) {
    if (!KINDS.includes(k)) {
      out.write(`--kinds: "${k}" is not a triage kind (${KINDS.join(', ')})\n`);
      return 2;
    }
  }

  const files = opts.audit.map((p) => readJsonl(p, opts.cut, opts.from));
  const rows = files.flatMap((f) => f.rows);
  const roles = indexRoles(JSON.parse(fs.readFileSync(opts.agents, 'utf8')));
  const pop = deliveries(rows, roles, opts.dropKickoff);

  let labels = new Map();
  if (opts.labels) {
    const parsed = parseLabels(fs.readFileSync(opts.labels, 'utf8'));
    if (parsed.problems.length > 0) {
      out.write(`--labels ${opts.labels}:\n  ${parsed.problems.join('\n  ')}\n`);
      return 2;
    }
    labels = parsed.labels;
  }

  let provider = null;
  if (opts.verdicts && !opts.noProvider) {
    provider = new FakeTriage(JSON.parse(fs.readFileSync(opts.verdicts, 'utf8')));
  }

  const policy = {
    enabled: opts.enabled,
    kinds: opts.kinds,
    merge_queue_enabled: opts.mergeQueue,
  };
  const result = replay(pop.deliveries, policy, { provider, floor: opts.floor });

  // The template is the residual as CSV and nothing else, so it is emitted
  // BEFORE scoring and returns: a labeller's file must not carry a report.
  if (opts.emitLabels) {
    out.write(emitLabelTemplate(result.rows));
    return 0;
  }

  const scored = score(result, labels);
  const sweepRows =
    provider && labels.size > 0 ? sweep(pop.deliveries, policy, provider, labels, opts.floors || DEFAULT_FLOORS) : [];

  const window =
    opts.from || opts.cut
      ? `window ${opts.from ? new Date(opts.from).toISOString() : 'start'} … ${opts.cut ? new Date(opts.cut).toISOString() : 'end'}`
      : '';
  const meta = {
    group: opts.group,
    audit_files: opts.audit,
    agents: opts.agents,
    labels: opts.labels,
    verdicts: opts.noProvider ? null : opts.verdicts,
    dropped_kickoffs: pop.droppedKickoffs,
    population_before_drop: pop.populationBeforeDrop,
    parse_errors: files.reduce((n, f) => n + f.parseErrors, 0),
    policy,
    floor: opts.floor,
    tokens_per_wake: TOKENS_PER_WAKE,
    window,
  };

  if (opts.format === 'json') {
    out.write(`${JSON.stringify({ meta, result, scored, sweep: sweepRows }, null, 2)}\n`);
  } else {
    out.write(`${renderMarkdown({ result, scored, sweepRows, meta })}\n`);
  }
  return 0;
}

module.exports = {
  // The mirror.
  PREFIX,
  KINDS,
  RULES,
  NEVER_REASONS,
  DELIVER_REASONS,
  NEEDS_YOU_MARKERS,
  REGROUNDING_MARKERS,
  classify,
  neverTriaged,
  planChunk,
  prOf,
  decide,
  resolveDecision,
  // The seam.
  PROVIDER_CLASSES,
  FakeTriage,
  providerAction,
  isNeedsOrchestrator,
  // The harness.
  parseLabels,
  emitLabelTemplate,
  readJsonl,
  indexRoles,
  deliveries,
  reliability,
  replay,
  score,
  sweep,
  renderMarkdown,
  TOKENS_PER_WAKE,
  DEFAULT_FLOORS,
  main,
};

if (require.main === module) {
  process.exitCode = main(process.argv.slice(2), process.stdout);
}
