#!/usr/bin/env node
'use strict';
// orch-scorecard — per-PR orchestration cost from logs that already exist (#2011 B1).
//
// WHAT THIS ANSWERS. For one pull request: how much orchestrator *attention* the
// review loop cost (wakes, by what woke it), how many *tokens* it cost on each side
// of the fleet, and the orchestrator's SHARE of those tokens. The point is a
// before/after number for the engine-owned review driver (#1778) that can be
// produced identically for a PR routed by hand, a PR driven by beta4, and a PR
// driven by beta5 — the driver writes `rd-*` rows, but every other counter here
// reads rows that predate it.
//
// WHY A SCRIPT AND NOT PRODUCT CODE. This slice adds no Rust. Every number below is
// a projection over files the app already writes: the group's `audit.jsonl`,
// `usage.json`, `agents.json`, and the CLI's own transcripts — the orchestrator's,
// named with `--transcript`, and (since #2167) a DELEGATE's, located under
// `--claude-projects` to repair a `usage.json` row the collector wrote as zero.
// Nothing here is a new row, a new field, or a new gate.
//
// RETIREMENT. `doc/design/orchestration-evals.md` carries the clause: this file is
// DELETED in #2011 S3, once an engine module (`crates/loomux-engine`, exposed as the
// `group_metrics` MCP tool) reproduces this output byte-for-byte on the fixture
// corpus. Until then it is the only reader, and the design note is its spec.
//
// PRIVACY. EVERY transcript this script opens — the orchestrator's and, since
// #2167, a delegate's — goes through the one reader (`readTranscript`), which
// projects each line to `timestamp` and `message.usage` and drops the rest BEFORE
// anything is retained. No transcript text is parsed, stored, printed, or checked
// in. What the coverage block publishes about a backfilled row is its PATH and its
// token totals, never a byte of its content. The tests run against synthetic
// transcripts written by hand — six lines for the orchestrator, three for the
// delegate.
//
// Dependency-free CJS (the root package.json is `"type": "module"`, so a `.js` file
// here would be ESM and `require` a ReferenceError). Node's own JSON and `readline`
// are all this needs; the transcript is ~230 MB, so it is streamed, never slurped.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const readline = require('node:readline');

// The S5 rule (#1778): a notice arriving up to ten minutes after the loop's last row
// still belongs to that loop — the orchestrator reads a merge/CI notice after the
// driver has stopped writing.
const DEFAULT_TAIL_MIN = 10;

// ---------------------------------------------------------------------------
// Wake classification — the plan's part-1 prefix classes, written as shapes.
//
// A wake is a `prompt` audit row whose `detail.to` is an orchestrator pane. Its
// class is decided by the LEADING shape of `detail.text`, first match wins, and the
// order below is the tie-break: `reports approved` is a reviewer report rather than
// a generic delegate report, and `recorded verdict` is the verdict echo rather than
// a system notice. `other` is a text with no `[orrerix]` prefix — in practice a
// human typing into the orchestrator pane.
// ---------------------------------------------------------------------------

const WAKE_KINDS = [
  'delegate-progress',
  'delegate-done',
  'reviewer-report',
  'delegate-blocked',
  'verdict-notice',
  'system-notice',
  'other',
];

const WAKE_SHAPES = [
  ['delegate-progress', /^\[orrerix\] \S+ reports progress\b/],
  ['delegate-done', /^\[orrerix\] \S+ reports done\b/],
  ['reviewer-report', /^\[orrerix\] \S+ reports (?:approved|request_changes)\b/],
  ['delegate-blocked', /^\[orrerix\] (?:\S+ reports blocked\b|message from \S)/],
  ['verdict-notice', /^\[orrerix\] \S+ \([^)]+\) recorded verdict\b/],
];

function classifyWake(text) {
  const t = typeof text === 'string' ? text : '';
  for (const [kind, re] of WAKE_SHAPES) if (re.test(t)) return kind;
  return /^\[orrerix\]/.test(t) ? 'system-notice' : 'other';
}

function emptyWakeCounts() {
  const out = {};
  for (const k of WAKE_KINDS) out[k] = 0;
  return out;
}

// ---------------------------------------------------------------------------
// "Does this row name PR N?"
//
// Structural first: `detail.pr === N` is what every `rd-*` and `review-verdict` row
// carries. Otherwise a text join on the `#N` token, with a trailing-digit guard so
// `#175` cannot match inside `#1751`. Issues and PRs share one number space on
// GitHub, so `#N` is unambiguous once N is known to be a PR.
// ---------------------------------------------------------------------------

function prTokenRe(pr) {
  return new RegExp('#' + pr + '(?![0-9])');
}

function rowNamesPr(row, pr, re) {
  const detail = row && row.detail;
  if (!detail || typeof detail !== 'object') return false;
  if (detail.pr === pr) return true;
  return (re || prTokenRe(pr)).test(JSON.stringify(detail));
}

// ---------------------------------------------------------------------------
// Windows.
//
// TWO windows per PR, because they answer different questions and the plan asks for
// both:
//
//   loop  — first `rd-*` row carrying `detail.pr === N` to the last one. This is the
//           #1778 S5 window; `span_h` and the driver counters are measured on it and
//           reproduce that table exactly. A hand-routed PR has no loop window.
//   pr    — first audit row NAMING the PR, to the PR's merge (from `--pr-meta`);
//           falling back to the last `rd-*` row, then to the last naming row. The
//           last-naming-row fallback is a poor end — a PR stays cited in the log for
//           days after it merges — and coverage says when it was used.
//
// Both take the +10 min tail for membership tests. `span_h` is the untailed span, so
// it is the honest duration of the thing measured.
// ---------------------------------------------------------------------------

function computeWindows(rows, pr, mergedMs, tailMs) {
  const re = prTokenRe(pr);
  let namedFirst = null;
  let namedLast = null;
  let rdFirst = null;
  let rdLast = null;
  for (const row of rows) {
    if (!rowNamesPr(row, pr, re)) continue;
    if (namedFirst === null) namedFirst = row.ts_ms;
    namedLast = row.ts_ms;
    if (typeof row.action === 'string' && row.action.startsWith('rd-') && row.detail.pr === pr) {
      if (rdFirst === null) rdFirst = row.ts_ms;
      rdLast = row.ts_ms;
    }
  }
  const loop = rdFirst === null ? null : {
    start_ms: rdFirst,
    end_ms: rdLast,
    tail_ms: tailMs,
    span_h: round2((rdLast - rdFirst) / 3600000),
  };
  if (namedFirst === null) return { pr: null, loop };
  let endMs = mergedMs;
  let endSource = 'merged_at';
  if (endMs === null || endMs === undefined) {
    if (rdLast !== null) { endMs = rdLast; endSource = 'last-rd-row'; }
    else { endMs = namedLast; endSource = 'last-naming-row'; }
  }
  return {
    pr: {
      start_ms: namedFirst,
      end_ms: endMs,
      end_source: endSource,
      tail_ms: tailMs,
      span_h: round2((endMs - namedFirst) / 3600000),
    },
    loop,
  };
}

function inWindow(ts, win) {
  if (!win || typeof ts !== 'number') return false;
  return ts >= win.start_ms && ts <= win.end_ms + win.tail_ms;
}

function round2(n) { return Math.round(n * 100) / 100; }

// ---------------------------------------------------------------------------
// Transcript usage.
//
// A Claude Code transcript writes an `assistant` line per content block, so ONE API
// response appears two or more times with the SAME `message.id` and the SAME usage
// object. Summing the lines double-counts: on the orchestrator's own transcript
// 30,842 assistant lines carry only 15,087 distinct message ids. Dedup by
// `message.id`, keeping the occurrence with the largest `output_tokens` — three
// pairs in that file differ only because an earlier line was written mid-stream.
//
// A line with no `message.id` cannot be deduped and is counted once, reported in
// coverage as `usage_rows_without_id` (the fixture carries one, so that path is not
// a claim with no test behind it).
// ---------------------------------------------------------------------------

function usageOf(entry) {
  const u = entry && entry.message && entry.message.usage;
  if (!u || typeof u !== 'object') return null;
  return {
    input: num(u.input_tokens),
    cache_read: num(u.cache_read_input_tokens),
    cache_creation: num(u.cache_creation_input_tokens),
    output: num(u.output_tokens),
  };
}

function num(v) { return typeof v === 'number' && Number.isFinite(v) ? v : 0; }

function emptyTokens() {
  return { input: 0, cache_read: 0, cache_creation: 0, output: 0, total: 0, turns: 0 };
}

function addTokens(acc, u) {
  acc.input += u.input;
  acc.cache_read += u.cache_read;
  acc.cache_creation += u.cache_creation;
  acc.output += u.output;
  acc.total += u.input + u.cache_read + u.cache_creation + u.output;
  acc.turns += 1;
  return acc;
}

function scaleTokens(t, factor) {
  return {
    input: Math.round(t.input * factor),
    cache_read: Math.round(t.cache_read * factor),
    cache_creation: Math.round(t.cache_creation * factor),
    output: Math.round(t.output * factor),
    total: Math.round(t.total * factor),
    turns: t.turns,
  };
}

// Collapses transcript lines into one record per API response.
// Exported so a test can pin the dedup rule without a 230 MB file.
function dedupeTranscriptTurns(entries) {
  const byId = new Map();
  const anonymous = [];
  let skipped = 0;
  for (const e of entries) {
    if (!e || e.type !== 'assistant') { skipped += 1; continue; }
    const u = usageOf(e);
    if (!u) { skipped += 1; continue; }
    const ts = e.timestamp ? Date.parse(e.timestamp) : NaN;
    if (!Number.isFinite(ts)) { skipped += 1; continue; }
    const id = e.message && e.message.id;
    const rec = { ts_ms: ts, usage: u };
    if (!id) { anonymous.push(rec); continue; }
    const prev = byId.get(id);
    // Keep the occurrence that saw the most output: a mid-stream line is written
    // before the response finished and under-reports `output_tokens`.
    if (!prev || u.output > prev.usage.output) byId.set(id, rec);
  }
  return {
    turns: [...byId.values(), ...anonymous].sort((a, b) => a.ts_ms - b.ts_ms),
    without_id: anonymous.length,
    non_usage_lines: skipped,
  };
}

// ---------------------------------------------------------------------------
// The CLI axis (#2011 A, heuristic H10).
//
// WHICH CLI produced a delegate's tokens is not a field loomux writes. It is
// READ OFF the `source` label of the `usage.json` row that carries those tokens,
// because that label names the RECORD the collector folded — and the record is
// per-CLI: a Claude Code transcript, an OpenCode session DB, a pi session file,
// a codex rollout (`group-cost-tracking.md`, the per-CLI sections). So the map
// below is not a guess about which CLI a block runs; it is the inverse of the
// collector's own arm selection.
//
// `transcript-backfill` maps to claude for the same reason: it is this script's
// own label for a row it rebuilt by folding a Claude Code transcript off disk
// (§4.6), so the record that produced those tokens is a claude one. Leaving it
// out would file every #2167-repaired row under `unknown`.
//
// EVERYTHING ELSE IS `unknown`, AND `unknown` IS REPORTED, NEVER GUESSED.
// `statusline` is the last-resort scrape that runs when no transcript arm
// matched, so it says only that a CLI printed a dollar figure — never which
// one. `none` says even less. A block's declared CLI in the workflow file is
// NOT consulted to fill those in: a pane can be recycled onto a block whose
// roster line has since changed, and the whole point of the axis is to measure
// what actually ran.
// ---------------------------------------------------------------------------

const SOURCE_TO_CLI = {
  transcript: 'claude',
  'transcript-backfill': 'claude',
  'pi-transcript': 'pi',
  'session-db': 'opencode',
  'codex-transcript': 'codex',
};

const CLI_UNKNOWN = 'unknown';

function cliForSource(source) {
  if (typeof source !== 'string') return CLI_UNKNOWN;
  return SOURCE_TO_CLI[source] || CLI_UNKNOWN;
}

// Resolve the clis a usage accumulator saw into ONE label. A `usage.json` row is
// unique per session key, so the session index sees exactly one source per key;
// the `agent_id` fallback index can see several rows under one agent, and if
// those disagree the answer is `mixed` — a fourth outcome, not a coin toss.
function resolveCli(clis) {
  const real = [...clis].filter((c) => c !== CLI_UNKNOWN);
  if (real.length === 0) return CLI_UNKNOWN;
  if (real.length === 1) return real[0];
  return 'mixed';
}

// agent id -> the cli loomux LAUNCHED that pane with, from its `agent-spawn`
// row. This is the fallback rung, used only where the usage source resolved to
// `unknown` (a zero/statusline row), and `cli_via` says which rung answered.
//
// WHICH SPAWN SITES CARRY IT, verified rather than assumed. THE LOAD-BEARING
// FACT IS THE TWO SITES, not any count: there are exactly two `agent-spawn`
// `json!` sites in `src-tauri/src/orchestration/mod.rs`, and the DELEGATE site
// carries `cli` (and `block`) while the ORCHESTRATOR site carries neither. So
// every row without `cli` is an orchestrator spawn, and an orchestrator is
// excluded from the delegate side anyway (§4.6) — which makes the rung cover
// every agent it is ever asked about.
//
// The store is LIVE and its counts decay by the hour, so a ratio here is an
// ILLUSTRATION and is dated rather than quoted as a standing figure: over this
// group's two audit generations, 628 of 637 spawn rows carried `cli` when read
// on 2026-09-06, and 629 of 638 on a re-read the same day. Re-derive it, do not
// cite it. `coverage.cli_axis.spawn_rows_with_cli`/`_without_cli` is that
// re-derivation, emitted by every run.
//
// None of this is a guarantee: a third spawn site that omitted `cli` would land
// its agents in `unknown`, reported.
//
// The LAST row wins: a recycled pane is respawned, and the live pane's CLI is
// the one that wrote the tokens being read.
function indexSpawnCli(rows) {
  const byAgent = new Map();
  let withCli = 0;
  let withoutCli = 0;
  for (const row of rows) {
    if (row.action !== 'agent-spawn') continue;
    const d = row.detail;
    if (!d || typeof d !== 'object' || typeof d.agent !== 'string') continue;
    if (typeof d.cli === 'string' && d.cli) { byAgent.set(d.agent, d.cli); withCli += 1; }
    else withoutCli += 1;
  }
  return { byAgent, spawn_rows_with_cli: withCli, spawn_rows_without_cli: withoutCli };
}

// Sessions whose occupants did not all run the SAME CLI.
//
// MEASURED, NOT HYPOTHETICAL. A `usage.json` row is keyed by CLI session, and
// `agents.json` carries one `session` per agent — so H8 already splits a shared
// row across its occupants. What H8 never contemplated is a pane recycled onto
// a DIFFERENT BLOCK running a DIFFERENT CLI, which this group's own store does:
// sessions `358b100f…` and `e81c5d8a…` are each shared by `worker-adv` (claude)
// and `worker-std` (opencode) agents (audit generations 1+2, read 2026-09-06).
//
// A per-session label cannot be right for two CLIs at once, so where the spawn
// rows disagree the PER-AGENT record wins: `agent-spawn` names what loomux
// actually launched for THAT id, while `source` names the record the collector
// folded for the whole session. The outcome is reported as its own rung
// (`spawn-row-session-conflict`) rather than folded into either of the other
// two, and coverage lists every session it fired on — this is a defect being
// SURFACED, not repaired: the structural fix is H10's, a `cli` on the snapshot.
function indexCliConflicts(agents, spawnCliByAgent) {
  const bySession = new Map();
  for (const a of agents) {
    if (!a || typeof a.session !== 'string' || !a.session || typeof a.id !== 'string') continue;
    const cli = spawnCliByAgent.get(a.id);
    if (!cli) continue;
    if (!bySession.has(a.session)) bySession.set(a.session, new Map());
    bySession.get(a.session).set(a.id, cli);
  }
  const conflicted = new Map(); // session -> { agent -> cli }
  for (const [session, byAgent] of bySession) {
    if (new Set(byAgent.values()).size > 1) conflicted.set(session, byAgent);
  }
  return conflicted;
}

// The rungs, in order, with the one that answered reported alongside. Never
// falls through to a guess: the last outcome is `unknown`, `cli_via: null`.
function resolveDelegateCli(usageCli, agent, spawnCliByAgent, sessionConflicted) {
  const spawned = spawnCliByAgent && spawnCliByAgent.get(agent);
  // Rung 0 — only where the session's own occupants disagree, which is the one
  // case rung 1 provably cannot answer for every agent on the row.
  if (sessionConflicted && typeof spawned === 'string' && spawned) {
    return { cli: spawned, cli_via: 'spawn-row-session-conflict' };
  }
  if (usageCli && usageCli !== CLI_UNKNOWN) return { cli: usageCli, cli_via: 'usage-source' };
  if (typeof spawned === 'string' && spawned) return { cli: spawned, cli_via: 'spawn-row' };
  return { cli: CLI_UNKNOWN, cli_via: null };
}

// `block/cli` as written on the rows — never a hardcoded roster (CLAUDE.md's
// per-CLI-identity rule). A delegate with no `agents.json` entry has no block,
// and reads as `unknown/<cli>` rather than being dropped.
function blockCliKey(block, cli) {
  return (block || CLI_UNKNOWN) + '/' + (cli || CLI_UNKNOWN);
}

// ---------------------------------------------------------------------------
// Agent -> PR attribution.
//
// Two tiers, and which tier carried an agent is reported per agent, because the
// difference is the whole of #2011 B2's scope:
//
//   structural — the row itself carries both the agent and the PR:
//                `rd-lane-spawned` / `rd-handback` (`detail.agent` + `detail.pr`),
//                and `review-verdict` (`actor` + `detail.pr`).
//   text       — an `agent-spawn` row inside the PR's window whose serialized detail
//                names `#N`. This is a heuristic (H2/H3 below): a brief can name a PR
//                it is not working on, and the window bound is what stops a later
//                brief citing an old PR from claiming its tokens.
//
// An agent attributed to k PRs contributes 1/k of its tokens to each, and is listed
// in coverage as `agents_split_across_prs`. Structural wins: once an agent has a
// structural attribution, text rows cannot add another PR to it.
// ---------------------------------------------------------------------------

function attributeAgents(rows, prs, windowsByPr) {
  const structural = new Map(); // agentId -> Set<pr>
  const textual = new Map();
  const add = (map, agent, pr) => {
    if (typeof agent !== 'string' || !agent) return;
    if (!map.has(agent)) map.set(agent, new Set());
    map.get(agent).add(pr);
  };
  const prSet = new Set(prs);
  for (const row of rows) {
    const d = row.detail;
    if (!d || typeof d !== 'object') continue;
    if ((row.action === 'rd-lane-spawned' || row.action === 'rd-handback') && prSet.has(d.pr)) {
      add(structural, d.agent, d.pr);
    } else if (row.action === 'review-verdict' && prSet.has(d.pr)) {
      add(structural, row.actor, d.pr);
    } else if (row.action === 'agent-spawn') {
      const blob = JSON.stringify(d);
      for (const pr of prs) {
        const win = windowsByPr.get(pr);
        if (!win || !win.pr || !inWindow(row.ts_ms, win.pr)) continue;
        if (prTokenRe(pr).test(blob)) add(textual, d.agent, pr);
      }
    }
  }
  const out = new Map(); // agentId -> {prs:Set, tier}
  for (const [agent, set] of structural) out.set(agent, { prs: set, tier: 'structural' });
  for (const [agent, set] of textual) {
    if (out.has(agent)) continue;
    out.set(agent, { prs: set, tier: 'text' });
  }
  return out;
}

// ---------------------------------------------------------------------------
// Per-PR scorecard.
// ---------------------------------------------------------------------------

function scorePr(ctx, pr) {
  const { rows, orchIds, tailMs, transcriptTurns, usageBySession, usageByAgent, sessionAgents, agentsById, attribution, prMeta, spawnCliByAgent, cliConflicts } = ctx;
  const meta = (prMeta && prMeta[String(pr)]) || {};
  const mergedMs = meta.merged_at ? Date.parse(meta.merged_at) : null;
  const win = computeWindows(rows, pr, Number.isFinite(mergedMs) ? mergedMs : null, tailMs);
  const re = prTokenRe(pr);

  const wakes = emptyWakeCounts();
  let wakesTotal = 0;
  let windowWakes = 0; // every orchestrator wake in the window, this PR's or not
  let loopNotices = 0;
  let loopNoticesAnyPane = 0;
  const verdicts = {};
  // The same rows as `verdicts`, kept in ARRIVAL ORDER per block, because
  // `rounds_to_pass` is a question about the sequence and the bucket above has
  // thrown the order away. `rows` is ts-sorted by `main` before it gets here.
  const verdictSeq = {};
  let verdictsTotal = 0;
  const driver = {
    drives: 0, lane_spawns: 0, hand_backs: 0, refused: 0, held: 0,
    cancelled: 0, consumed: 0, satisfied: 0, ci_green: 0, resumed: 0, pruned: 0,
    // #2501: panes the driver released itself, per side. Named counters rather
    // than the catch-all below, because these two are the measurement the
    // feature exists to move: the whole point is that the next round of this
    // scorecard can put them beside `refused` and `held` without a reader
    // having to know which rd-* actions are new.
    lanes_released: 0, workers_released: 0,
    // #2509: how often the one-shot body-only grace saved a drive from
    // `held(review-limit)`. Worth its own counter rather than a share of
    // hand-backs, because the question it answers is whether the grace is
    // earning its keep — a grace that fires and then parks anyway cost a round
    // for nothing, and only `rounds_grace` beside `held.review-limit` shows it.
    rounds_grace: 0,
  };
  const refusedByReason = {};
  const heldByReason = {};
  const unclassified = {};
  let rowsClassified = 0;
  // A `review-verdict` or `rd-*` row is matched STRUCTURALLY on `detail.pr`, so it is
  // counted wherever it occurs — a re-drive or a late verdict after the merge is still
  // a round the PR cost. That makes those counters wider than the PR window, so the
  // number that fall outside it is reported rather than left for a reader to wonder
  // about. It is 0 on all eleven benchmark PRs.
  let structuralOutsideWindow = 0;

  for (const row of rows) {
    const d = row.detail;
    const isWake = row.action === 'prompt' && d && orchIds.has(d.to);
    if (isWake && inWindow(row.ts_ms, win.pr)) windowWakes += 1;
    if (!rowNamesPr(row, pr, re)) continue;

    // An `[orrerix]`-prefixed prompt naming the PR inside the LOOP window, delivered
    // to ANY pane. This is #1778's S5 "orch notices" column verbatim — it is what
    // that instrument counted, and it is reproduced here so the table stays checkable
    // — but it is NOT a count of orchestrator wakes: 1-2 per PR were the driver's own
    // resume prompts typed into a WORKER pane, which is precisely the traffic the
    // driver moved OFF the orchestrator. `loop_notices` below is the corrected count.
    if (row.action === 'prompt' && inWindow(row.ts_ms, win.loop) && /^\[orrerix\]/.test((d && d.text) || '')) {
      loopNoticesAnyPane += 1;
    }
    if (isWake) {
      if (!inWindow(row.ts_ms, win.pr)) continue;
      wakes[classifyWake(d.text)] += 1;
      wakesTotal += 1;
      rowsClassified += 1;
      if (inWindow(row.ts_ms, win.loop) && /^\[orrerix\]/.test(d.text || '')) loopNotices += 1;
      continue;
    }
    if (row.action === 'review-verdict' && d.pr === pr) {
      if (!inWindow(row.ts_ms, win.pr)) structuralOutsideWindow += 1;
      const block = d.block || 'unknown';
      const verdict = String(d.verdict || 'unknown').toLowerCase();
      verdicts[block] = verdicts[block] || {};
      verdicts[block][verdict] = (verdicts[block][verdict] || 0) + 1;
      (verdictSeq[block] = verdictSeq[block] || []).push(verdict);
      verdictsTotal += 1;
      rowsClassified += 1;
      continue;
    }
    if (typeof row.action === 'string' && row.action.startsWith('rd-') && d.pr === pr) {
      if (!inWindow(row.ts_ms, win.pr)) structuralOutsideWindow += 1;
      rowsClassified += 1;
      switch (row.action) {
        case 'rd-started': driver.drives += 1; break;
        case 'rd-lane-spawned': driver.lane_spawns += 1; break;
        case 'rd-handback': driver.hand_backs += 1; break;
        case 'rd-refused':
          driver.refused += 1;
          refusedByReason[d.reason || 'unknown'] = (refusedByReason[d.reason || 'unknown'] || 0) + 1;
          break;
        case 'rd-held':
          driver.held += 1;
          heldByReason[d.reason || 'unknown'] = (heldByReason[d.reason || 'unknown'] || 0) + 1;
          break;
        case 'rd-cancelled': driver.cancelled += 1; break;
        case 'rd-consumed': driver.consumed += 1; break;
        case 'rd-satisfied': driver.satisfied += 1; break;
        case 'rd-ci-green': driver.ci_green += 1; break;
        case 'rd-resumed': driver.resumed += 1; break;
        case 'rd-pruned': driver.pruned += 1; break;
        case 'rd-lane-released': driver.lanes_released += 1; break;
        case 'rd-worker-released': driver.workers_released += 1; break;
        case 'rd-round-grace': driver.rounds_grace += 1; break;
        default: driver[row.action] = (driver[row.action] || 0) + 1; break;
      }
      continue;
    }
    if (inWindow(row.ts_ms, win.pr)) {
      unclassified[row.action] = (unclassified[row.action] || 0) + 1;
    }
  }

  // Orchestrator tokens: every deduped transcript turn stamped inside the PR window.
  // `usage.json` cannot answer this — it is CUMULATIVE per session and carries no
  // time series (doc/design/group-cost-tracking.md).
  const orchTokens = emptyTokens();
  for (const t of transcriptTurns) {
    if (inWindow(t.ts_ms, win.pr)) addTokens(orchTokens, t.usage);
  }
  // The orchestrator serves several PRs at once, so the raw window sum is an UPPER
  // BOUND for one PR. The apportioned figure splits it by this PR's share of the
  // orchestrator wakes in the same window (H5). Both are reported: the raw one is
  // what the brief asks for, the apportioned one is what is comparable when two PRs
  // ran concurrently.
  const wakeShare = windowWakes > 0 ? wakesTotal / windowWakes : (wakesTotal > 0 ? 1 : 0);
  const orchTokensAttributed = scaleTokens(orchTokens, wakeShare);

  const delegateTokens = emptyTokens();
  const delegates = [];
  // `<block>/<cli>` -> credited + raw tokens. Keys are READ OFF the rows (a block
  // the roster no longer declares still gets a bucket), never a hardcoded list.
  const byBlockCli = {};
  for (const [agent, att] of attribution) {
    if (!att.prs.has(pr)) continue;
    const info = agentsById.get(agent);
    if (info && info.role === 'orchestrator') continue;
    // Session first (see `indexUsage`), agent id only as a fallback.
    const session = info && typeof info.session === 'string' ? info.session : null;
    let usage = session ? usageBySession.get(session) : undefined;
    let usageKey = usage ? 'session' : null;
    let sessionAgentCount = 1;
    if (usage) {
      sessionAgentCount = (sessionAgents.get(session) || [agent]).length || 1;
    } else {
      usage = usageByAgent.get(agent);
      usageKey = usage ? 'agent_id' : null;
    }
    const { cli, cli_via: cliVia } = resolveDelegateCli(
      usage && usage.cli, agent, spawnCliByAgent,
      Boolean(session && cliConflicts && cliConflicts.has(session)),
    );
    const prWeight = 1 / att.prs.size;
    const sessionWeight = 1 / sessionAgentCount;
    const weight = prWeight * sessionWeight;
    const t = usage
      ? { input: usage.input, cache_read: usage.cache_read, cache_creation: usage.cache_creation, output: usage.output, total: usage.total }
      : { input: 0, cache_read: 0, cache_creation: 0, output: 0, total: 0 };
    delegateTokens.input += t.input * weight;
    delegateTokens.cache_read += t.cache_read * weight;
    delegateTokens.cache_creation += t.cache_creation * weight;
    delegateTokens.output += t.output * weight;
    delegateTokens.total += t.total * weight;
    delegateTokens.turns += 1;
    const bck = blockCliKey(info && info.block, cli);
    const bucket = byBlockCli[bck] || (byBlockCli[bck] = { block: (info && info.block) || CLI_UNKNOWN, cli, tokens: 0, tokens_credited: 0, count: 0 });
    bucket.tokens += t.total;
    bucket.tokens_credited += t.total * weight;
    bucket.count += 1;
    delegates.push({
      agent,
      block: info ? info.block : null,
      role: info ? info.role : null,
      // Which CLI produced these tokens, and which rung said so (H10). `unknown`
      // with `cli_via: null` is a REPORTED outcome, never a filled-in guess.
      cli,
      cli_via: cliVia,
      tier: att.tier,
      weight: round2(weight),
      pr_weight: round2(prWeight),
      session_weight: round2(sessionWeight),
      shared_with_prs: att.prs.size,
      shared_session_agents: sessionAgentCount,
      // `tokens` is the WHOLE row the agent's session carries; `tokens_credited` is what
      // this PR actually got after both weights. They differ whenever a session was
      // shared or an agent worked more than one PR, and the card shows both so the
      // delegate total is checkable by hand.
      tokens: t.total,
      tokens_credited: Math.round(t.total * weight),
      usage_key: usageKey,
      has_usage_row: Boolean(usage),
    });
  }
  for (const k of ['input', 'cache_read', 'cache_creation', 'output', 'total']) {
    delegateTokens[k] = Math.round(delegateTokens[k]);
  }
  for (const b of Object.values(byBlockCli)) b.tokens_credited = Math.round(b.tokens_credited);
  delegates.sort((a, b) => b.tokens - a.tokens || (a.agent < b.agent ? -1 : 1));

  return {
    pr,
    build: meta.build || null,
    issue: meta.issue || null,
    outcome: meta.outcome || null,
    merged_at: meta.merged_at || null,
    windows: win,
    // The PR window's own untailed span, lifted to the top of the card because it
    // is one of the four columns the cli table compares. `null`, never 0, when no
    // audit row names the PR at all — "no window" and "an instantaneous PR" are
    // different facts.
    wall_clock_h: win.pr ? win.pr.span_h : null,
    orchestrator: {
      wakes_total: wakesTotal,
      wakes_by_kind: wakes,
      loop_notices: loopNotices,
      loop_notices_any_pane_s5: loopNoticesAnyPane,
      wake_share: { pr_wakes: wakesTotal, window_wakes: windowWakes, share: round2(wakeShare) },
      tokens_window: orchTokens,
      tokens_attributed: orchTokensAttributed,
    },
    review: { rounds: verdictsTotal, by_block: verdicts, lanes: laneStats(verdictSeq) },
    driver: { ...driver, refused_by_reason: refusedByReason, held_by_reason: heldByReason },
    delegates: { count: delegates.length, tokens: delegateTokens, by_block_cli: byBlockCli, agents: delegates },
    share: {
      orchestrator_pct_raw: pct(orchTokens.total, orchTokens.total + delegateTokens.total),
      orchestrator_pct_attributed: pct(orchTokensAttributed.total, orchTokensAttributed.total + delegateTokens.total),
    },
    rows_classified: rowsClassified,
    rows_counted_outside_pr_window: structuralOutsideWindow,
    rows_unclassified_in_window: unclassified,
  };
}

function pct(part, whole) {
  if (!whole) return null;
  return round2((part / whole) * 100);
}

// ---------------------------------------------------------------------------
// Group-wide totals — the plan's part-1 calibration numbers, per audit file.
// ---------------------------------------------------------------------------

function groupTotals(files, orchIds) {
  return files.map((f) => {
    const wakes = emptyWakeCounts();
    let total = 0;
    let typed = 0;
    let tsFirst = null;
    let tsLast = null;
    for (const row of f.rows) {
      if (typeof row.ts_ms === 'number') {
        if (tsFirst === null || row.ts_ms < tsFirst) tsFirst = row.ts_ms;
        if (tsLast === null || row.ts_ms > tsLast) tsLast = row.ts_ms;
      }
      const d = row.detail;
      if (!d || !orchIds.has(d.to)) continue;
      if (row.action === 'prompt') { wakes[classifyWake(d.text)] += 1; total += 1; }
      else if (row.action === 'prompt-typed') typed += 1;
    }
    return {
      path: f.path,
      rows: f.rows.length,
      parse_errors: f.parseErrors,
      ts_first: tsFirst,
      ts_last: tsLast,
      span_h: tsFirst === null ? null : round2((tsLast - tsFirst) / 3600000),
      orchestrator_wakes: total,
      prompt_typed_to_orchestrator: typed,
      wakes_by_kind: wakes,
    };
  });
}

// ---------------------------------------------------------------------------
// Loading.
// ---------------------------------------------------------------------------

function readJsonl(pathname, cutMs) {
  const rows = [];
  let parseErrors = 0;
  const text = fs.readFileSync(pathname, 'utf8');
  for (const line of text.split('\n')) {
    if (!line.trim()) continue;
    let row;
    try { row = JSON.parse(line); } catch { parseErrors += 1; continue; }
    if (cutMs !== null && typeof row.ts_ms === 'number' && row.ts_ms > cutMs) continue;
    rows.push(row);
  }
  return { path: pathname, rows, parseErrors };
}

async function readTranscript(pathname, cutMs) {
  const entries = [];
  const rl = readline.createInterface({
    input: fs.createReadStream(pathname, { encoding: 'utf8' }),
    crlfDelay: Infinity,
  });
  let lines = 0;
  for await (const line of rl) {
    lines += 1;
    if (!line) continue;
    // Cheap prefilter before JSON.parse: the file is ~230 MB and only a quarter of
    // its lines are assistant responses carrying usage.
    if (line.indexOf('"usage"') === -1 || line.indexOf('"assistant"') === -1) continue;
    let entry;
    try { entry = JSON.parse(line); } catch { continue; }
    // Project to the three fields this reader is allowed to see BEFORE retaining
    // anything: the parsed line is dropped here, so no transcript prose is ever held
    // in memory, printed, or written. (It also keeps a 230 MB file's worth of
    // assistant messages from becoming a 30k-element array of full objects.)
    if (entry.type !== 'assistant' || !entry.message || !entry.message.usage) continue;
    entries.push({
      type: 'assistant',
      timestamp: entry.timestamp,
      message: { id: entry.message.id, usage: entry.message.usage },
    });
  }
  const deduped = dedupeTranscriptTurns(entries);
  if (cutMs !== null) deduped.turns = deduped.turns.filter((t) => t.ts_ms <= cutMs);
  return { path: pathname, lines, assistant_usage_lines: entries.length, ...deduped };
}

function indexAgents(agents) {
  const byId = new Map();
  for (const a of agents) {
    if (!a || typeof a.id !== 'string') continue;
    byId.set(a.id, a);
  }
  return byId;
}

// A `usage.json` row is keyed by **CLI SESSION**, not by agent. `group-cost-tracking.md`
// says why — "keyed by CLI session id … a resumed session updates one row instead of
// double-counting, since the transcript is cumulative" — and the consequence is the
// thing to get right here: when a pane's session is carried to a NEW agent id, that one
// row's `agent_id` names only the LAST occupant, and every earlier agent on the session
// has no row of its own at all. Joining on `agent_id` therefore reports an agent that
// demonstrably spent tokens as having spent zero, while crediting its successor with the
// whole lineage. On this group's own store that is not a corner case: 514 rows sit on a
// session shared by more than one agent id, carrying 31.2 G of the store's 44.1 G tokens
// (2026-09-03). That store is LIVE — it grew from 1356 rows to 1362 in the hour between
// two measurements, with 514 shared both times — so the shared COUNT is the figure to
// quote and the denominator is the one to re-derive.
//
// So the row is indexed by session and split evenly across the agents that occupied it
// (heuristic H8). The split is a guess about how a shared session's spend divided, but it
// is self-correcting in the common case: a session reused by a worker's own successive
// agent ids has every occupant attributed to the SAME PR, so the halves re-sum to the
// whole row. Where only some occupants are attributed, the PR gets a fraction rather than
// all-or-nothing.
//
// `byAgent` is kept as a FALLBACK for a row whose session key is missing, and which index
// answered is reported per delegate as `usage_key`.
function indexUsage(usage) {
  const bySession = new Map();
  const byAgent = new Map();
  let unusable = 0;
  // `clis` is a SET, not a scalar: the session index sees one row per key, but the
  // `agent_id` fallback index can see several rows under one agent, and two rows that
  // disagree resolve to `mixed` rather than to whichever was folded last (#2011 A).
  const blank = () => ({ input: 0, cache_read: 0, cache_creation: 0, output: 0, total: 0, cost_usd: 0, rows: 0, clis: new Set() });
  const add = (map, key, u) => {
    const acc = map.get(key) || blank();
    acc.input += num(u.input_tokens);
    acc.cache_read += num(u.cache_read_tokens);
    acc.cache_creation += num(u.cache_creation_tokens);
    acc.output += num(u.output_tokens);
    acc.cost_usd += num(u.cost_usd);
    acc.rows += 1;
    acc.total = acc.input + acc.cache_read + acc.cache_creation + acc.output;
    acc.clis.add(cliForSource(u.source));
    acc.cli = resolveCli(acc.clis);
    map.set(key, acc);
  };
  for (const u of usage) {
    if (!u || typeof u !== 'object') { unusable += 1; continue; }
    const hasSession = typeof u.key === 'string' && u.key;
    const hasAgent = typeof u.agent_id === 'string' && u.agent_id;
    if (!hasSession && !hasAgent) { unusable += 1; continue; }
    if (hasSession) add(bySession, u.key, u);
    if (hasAgent) add(byAgent, u.agent_id, u);
  }
  return { bySession, byAgent, unusable };
}

// session id -> every agent id that occupied it, from `agents.json`. Used only to size
// the H8 split, so it counts EVERY agent on the session, attributed or not.
function indexSessionAgents(agents) {
  const bySession = new Map();
  for (const a of agents) {
    if (!a || typeof a.session !== 'string' || !a.session) continue;
    if (!bySession.has(a.session)) bySession.set(a.session, []);
    bySession.get(a.session).push(a.id);
  }
  return bySession;
}

// ---------------------------------------------------------------------------
// Transcript backfill for a zero `usage.json` row (#2167).
//
// The collector resolved a claude delegate's CLI from its class's DEFAULT block
// rather than from its own, so on a roster with two blocks in one class every
// claude pane in the second block was read as the first block's CLI, its
// transcript arm never ran, and its row landed as `statusline`/`none` with four
// zero counters — for a session whose transcript was on disk the whole time.
// That is fixed in `src-tauri`; this is the reader's half, and it stays useful
// after the fix for the same reason every other counter here is defensive: the
// rows already written are still zero, and no backfill runs against a store
// whose rows are right.
//
// The rule is deliberately narrow, so a correct row can never be rewritten:
//
//   - only a row whose FOUR token counters are all zero is a candidate;
//   - only when `<root>/*/<session>.jsonl` exists — the same one-level scan
//     `usage::claude_transcript_path` does, and the same reason it is a scan and
//     not a slug derivation: Claude encodes the cwd into the folder name and we
//     do not re-derive that encoding. It is also what identifies the row as a
//     CLAUDE one at all: an opencode row has no such file;
//   - only when the transcript sums to more than zero.
//
// COST. One extra STREAMED pass per repaired row. The index itself is readdir-only
// (630 files across 537 folders on the store this was written against), but a row
// that IS repaired costs a full read of its transcript, and those run to hundreds of
// MB. That read is inherent — it is the data being recovered — but it is why the
// index is built ONLY when a zero row exists, and why --no-backfill exists.
//
// The sum is `dedupeTranscriptTurns` — the SAME fold the orchestrator's own
// transcript goes through, message-id dedup and the four usage fields, so a
// backfilled delegate row and the orchestrator row it is compared against
// cannot be computed two different ways.
//
// TWO THINGS A BACKFILL DOES NOT DO, both stated as H9 rather than left for a
// reader to discover: it derives no dollar figure (the row keeps the `cost_usd`
// it had), and it does not honour `--cut`. `--cut` cannot rewind a `usage.json`
// row either — the row is cumulative-to-now with no history — so cutting the
// backfilled rows and not the others would make the two kinds of row disagree
// about what instant the table describes.
// ---------------------------------------------------------------------------

function usageRowTokens(u) {
  return num(u.input_tokens) + num(u.output_tokens)
    + num(u.cache_creation_tokens) + num(u.cache_read_tokens);
}

// Is this row a backfill candidate? ONE definition, called by the backfill and
// by `main`'s `--no-backfill` count — which used to spell it out a second time
// inline, exactly the two-sites-drift this whole PR exists to close (rev-std).
function isZeroUsageRow(u) {
  return !!u && typeof u === 'object'
    && typeof u.key === 'string' && u.key !== ''
    && usageRowTokens(u) === 0;
}

// A `usage.json` row is keyed by CLI session id **or by `agent:<id>` when the
// pane never got one** (`UsageSnapshot::key`, `mod.rs`). An `agent:`-keyed row
// can never match a transcript index, so filing it under "no transcript" would
// report a session as LOST that was never there — on the live store that was
// 125 of 146 skips (86%). It is the third outcome, not a missing file.
//
// The prefix is a safe discriminator, not a heuristic: a CLI session id reaching
// the key has been through `PathSegment::parse`, which rejects `:`.
function isAgentKeyedRow(u) {
  return typeof u.key === 'string' && u.key.startsWith('agent:');
}

// The backfill's accounting: every candidate leaves by exactly one door.
//
// **Honest scope, re-derived on the round that added the third door (#2167
// review N2).** Against today's branch set this cannot fire — the four outcomes
// are exhaustive by construction, and a reviewer's mutation disabling the throw
// left the suite at 32/32, which is the correct result and not a coverage gap.
// It is a tripwire for a FUTURE branch that forgets to record its skip, so it
// is pinned DIRECTLY (`reconcileBackfill` is exported and tested with a record
// no branch here can produce) rather than through the pipeline, where it would
// be a guard the suite cannot redden.
function reconcileBackfill(cov) {
  const skipped = cov.zero_rows_without_a_transcript.length
    + cov.zero_rows_whose_transcript_summed_to_zero.length
    + cov.zero_rows_with_no_session_id.length;
  if (cov.rows + skipped !== cov.zero_rows_considered) {
    throw new Error('backfill accounting: '
      + `${cov.rows} backfilled + ${skipped} skipped `
      + `!= ${cov.zero_rows_considered} zero rows considered`);
  }
  return cov;
}

function defaultClaudeProjectsRoot() {
  return path.join(os.homedir(), '.claude', 'projects');
}

// session id -> transcript path, from one level of `<root>/<encoded-cwd>/`.
// `null` when the root cannot be read at all (no `~/.claude`, a bad `--claude-projects`).
function claudeTranscriptIndex(root) {
  let projects;
  try { projects = fs.readdirSync(root, { withFileTypes: true }); } catch { return null; }
  const bySession = new Map();
  let scanned = 0;
  for (const p of projects) {
    if (!p.isDirectory()) continue;
    let files;
    try { files = fs.readdirSync(path.join(root, p.name)); } catch { continue; }
    scanned += 1;
    for (const f of files) {
      if (!f.endsWith('.jsonl')) continue;
      const id = f.slice(0, -'.jsonl'.length);
      if (!bySession.has(id)) bySession.set(id, path.join(root, p.name, f));
    }
  }
  return { bySession, projects_scanned: scanned };
}

// Returns the usage array to use (patched copies, never the caller's rows mutated)
// plus the coverage record. The index is built ONLY when a zero row exists, so a
// store with nothing to backfill never walks the projects tree.
async function backfillZeroUsageRows(usage, root) {
  const rows = Array.isArray(usage) ? usage : [];
  const zero = rows.filter(isZeroUsageRow);
  const cov = {
    claude_projects_root: root,
    scanned: false,
    projects_scanned: 0,
    transcripts_indexed: 0,
    zero_rows_considered: zero.length,
    rows: 0,
    tokens: 0,
    from: [],
    zero_rows_without_a_transcript: [],
    // Not keyed by a CLI session at all, so no transcript could ever match.
    // Kept apart from the line below because that one says the store lost a
    // file; this one says there was never a file to lose.
    zero_rows_with_no_session_id: [],
    // A transcript that EXISTS and folds to nothing is its own outcome, not a
    // missing file: reporting it under the line above would tell a reader the
    // store lost a session it did not lose. Both are skips; only one is
    // recoverable by putting the file back.
    zero_rows_whose_transcript_summed_to_zero: [],
  };
  if (zero.length === 0) return { usage: rows, backfill: cov };

  const idx = claudeTranscriptIndex(root);
  cov.scanned = idx !== null;
  if (idx === null) {
    // The root could not be read. That is not "no transcripts": say which rows
    // were left alone, so a zero column is never silently blamed on the store.
    for (const u of zero) {
      (isAgentKeyedRow(u) ? cov.zero_rows_with_no_session_id : cov.zero_rows_without_a_transcript)
        .push(u.key);
    }
    cov.zero_rows_without_a_transcript.sort();
    cov.zero_rows_with_no_session_id.sort();
    return { usage: rows, backfill: reconcileBackfill(cov) };
  }
  cov.projects_scanned = idx.projects_scanned;
  cov.transcripts_indexed = idx.bySession.size;

  const patched = new Map();
  for (const u of zero) {
    if (isAgentKeyedRow(u)) { cov.zero_rows_with_no_session_id.push(u.key); continue; }
    const file = idx.bySession.get(u.key);
    if (!file) { cov.zero_rows_without_a_transcript.push(u.key); continue; }
    const read = await readTranscript(file, null);
    const t = read.turns.reduce((acc, turn) => addTokens(acc, turn.usage), emptyTokens());
    if (t.total === 0) { cov.zero_rows_whose_transcript_summed_to_zero.push(u.key); continue; }
    patched.set(u.key, {
      ...u,
      source: 'transcript-backfill',
      input_tokens: t.input,
      output_tokens: t.output,
      cache_creation_tokens: t.cache_creation,
      cache_read_tokens: t.cache_read,
    });
    cov.from.push({
      session: u.key,
      agent_id: typeof u.agent_id === 'string' ? u.agent_id : null,
      role: typeof u.role === 'string' ? u.role : null,
      was_source: typeof u.source === 'string' ? u.source : null,
      path: file,
      turns: t.turns,
      tokens: { input: t.input, cache_read: t.cache_read, cache_creation: t.cache_creation, output: t.output, total: t.total },
    });
    cov.tokens += t.total;
  }
  cov.rows = cov.from.length;
  cov.zero_rows_without_a_transcript.sort();
  cov.zero_rows_whose_transcript_summed_to_zero.sort();
  cov.zero_rows_with_no_session_id.sort();
  // Population control (CLAUDE.md: count at the VERIFIED site, not the matched
  // one) — see `reconcileBackfill` for what it can and cannot catch today.
  reconcileBackfill(cov);
  cov.from.sort((a, b) => (a.session < b.session ? -1 : a.session > b.session ? 1 : 0));
  return { usage: rows.map((u) => (u && patched.get(u.key)) || u), backfill: cov };
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

function fmtTokens(n) {
  if (n === null || n === undefined) return '—';
  if (n >= 1e9) return (n / 1e9).toFixed(2) + ' G';
  if (n >= 1e6) return (n / 1e6).toFixed(1) + ' M';
  if (n >= 1e3) return (n / 1e3).toFixed(0) + ' K';
  return String(n);
}

function renderPrTable(cards) {
  const lines = [
    '| PR | build | wakes | of which reports | rounds | lane spawns | hand-backs | `rd-refused` | held | loop span h | loop notices (S5) | orch tokens | delegate tokens | orch share |',
    '|---|---|---|---|---|---|---|---|---|---|---|---|---|---|',
  ];
  for (const c of cards) {
    const w = c.orchestrator.wakes_by_kind;
    const reports = w['delegate-progress'] + w['delegate-done'] + w['reviewer-report'] + w['delegate-blocked'];
    lines.push('| #' + c.pr
      + ' | ' + (c.build || '—')
      + ' | ' + c.orchestrator.wakes_total
      + ' | ' + reports
      + ' | ' + c.review.rounds
      + ' | ' + c.driver.lane_spawns
      + ' | ' + c.driver.hand_backs
      + ' | ' + c.driver.refused
      + ' | ' + c.driver.held
      + ' | ' + (c.windows.loop ? c.windows.loop.span_h.toFixed(2) : '—')
      + ' | ' + (c.windows.loop ? c.orchestrator.loop_notices + ' (' + c.orchestrator.loop_notices_any_pane_s5 + ')' : '—')
      + ' | ' + fmtTokens(c.orchestrator.tokens_window.total)
      + ' | ' + fmtTokens(c.delegates.tokens.total)
      + ' | ' + (c.share.orchestrator_pct_raw === null ? '—' : c.share.orchestrator_pct_raw.toFixed(1) + ' %')
      + ' |');
  }
  return lines.join('\n');
}

function renderGroupTable(files) {
  const lines = [
    '| audit file | rows | span h | orch wakes | progress | done | reviewer | blocked/msg | verdict | system | other | `prompt-typed` |',
    '|---|---|---|---|---|---|---|---|---|---|---|---|',
  ];
  for (const f of files) {
    const w = f.wakes_by_kind;
    lines.push('| `' + f.path.split(/[\\/]/).pop() + '` | ' + f.rows + ' | ' + (f.span_h === null ? '—' : f.span_h.toFixed(1))
      + ' | ' + f.orchestrator_wakes
      + ' | ' + w['delegate-progress'] + ' | ' + w['delegate-done'] + ' | ' + w['reviewer-report']
      + ' | ' + w['delegate-blocked'] + ' | ' + w['verdict-notice'] + ' | ' + w['system-notice'] + ' | ' + w.other
      + ' | ' + f.prompt_typed_to_orchestrator + ' |');
  }
  return lines.join('\n');
}

// ---------------------------------------------------------------------------
// Per-lane review statistics (§4.8).
//
// A "lane" is a review BLOCK (`rev-std`, `rev-final`), and the two questions the
// cli comparison asks of it are how many rounds it took to say `pass` and how
// often it said `fail`:
//
//   rounds_to_pass — the 1-based position of the FIRST `pass` in that block's
//                    pass/fail sequence. `null` — never 0 and never the round
//                    count — where the block never passed: a lane still in
//                    review has no answer yet, and reporting the rounds so far
//                    would read as a lane that passed on its last round.
//                    A later `fail` (a re-review on a new head) does not lower
//                    it, and a re-`pass` is not a second answer.
//   fail_rate      — `fail / (pass + fail)` over the WHOLE sequence, so those
//                    later rounds do count here. `null` when the lane recorded
//                    neither, because 0 would claim a clean lane where there is
//                    no lane at all.
//
// Verdicts other than `pass`/`fail` are excluded from BOTH (the live vocabulary
// is exactly those two; `verdicts_other` reports anything else rather than
// letting an unrecognised value shift an index silently).
// ---------------------------------------------------------------------------

function laneStats(verdictSeq) {
  const out = {};
  for (const [block, seq] of Object.entries(verdictSeq)) {
    const decided = seq.filter((v) => v === 'pass' || v === 'fail');
    const firstPass = decided.indexOf('pass');
    const fail = decided.filter((v) => v === 'fail').length;
    const pass = decided.length - fail;
    out[block] = {
      rounds: seq.length,
      pass,
      fail,
      verdicts_other: seq.length - decided.length,
      rounds_to_pass: firstPass === -1 ? null : firstPass + 1,
      fail_rate: decided.length === 0 ? null : round2(fail / decided.length),
    };
  }
  return out;
}

// ---------------------------------------------------------------------------
// Medians (§4.9).
//
// MEDIANS, NEVER TOTALS. The two windows hold different PRs doing different
// work, and a total is then a statement about the task mix rather than about
// the CLI. The median plus the inter-quartile range plus n says what a total
// hides: whether the middle moved and whether the spread swamps the move.
//
// A CELL BELOW `MEDIAN_MIN_N` IS `null`, not a number. Three points is already
// a thin claim; two is an average of a pair, and one is an anecdote wearing a
// statistic's clothes. `n` is reported at every size — including 0 — so a null
// cell is legible as "not enough data" rather than as a missing measurement.
//
// Quartiles use the exclusive-median (Tukey hinge) convention: the halves
// exclude the middle element on an odd-length sample. Stated because there are
// several conventions and a reader re-deriving an IQR by hand needs to know
// which one produced the number.
// ---------------------------------------------------------------------------

const MEDIAN_MIN_N = 3;

function medianOf(sorted) {
  if (sorted.length === 0) return null;
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

// The five-number cell every comparison column is made of. Non-numeric and
// null inputs are DROPPED and counted, so a lane with no answer (a `null`
// `rounds_to_pass`) shrinks n rather than being read as a zero.
function statCell(values) {
  const xs = values.filter((v) => typeof v === 'number' && Number.isFinite(v)).sort((a, b) => a - b);
  const dropped = values.length - xs.length;
  const cell = { n: xs.length, dropped, median: null, q1: null, q3: null, iqr: null, min: null, max: null };
  if (xs.length < MEDIAN_MIN_N) return cell;
  const mid = Math.floor(xs.length / 2);
  const lower = xs.slice(0, mid);
  const upper = xs.length % 2 ? xs.slice(mid + 1) : xs.slice(mid);
  cell.median = round2(medianOf(xs));
  cell.q1 = round2(medianOf(lower));
  cell.q3 = round2(medianOf(upper));
  cell.iqr = round2(cell.q3 - cell.q1);
  cell.min = xs[0];
  cell.max = xs[xs.length - 1];
  return cell;
}

// ---------------------------------------------------------------------------
// The opencode-vs-pi comparison (§4.9).
//
// SELECTION, and why each bound is there:
//
//   merged   — the PR must have a `merged_at` in `--pr-meta`. An open PR has no
//              wall clock and its review lanes have not finished.
//   one cli  — every delegate of a compared block must resolve to the SAME cli,
//              and that cli must not be `unknown` or `mixed`. A PR whose
//              worker-std panes were half opencode and half pi measures the
//              switch, not either side of it, and is EXCLUDED and listed.
//   side     — `merged_at` against `--split-at` (the #2817 merge instant), so
//              the window a PR belongs to is a fact about the clock rather than
//              about the cli that was read off it.
//
// The cli and the side are resolved INDEPENDENTLY, and cross-checked ONLY when
// the caller declares what to expect (`--sides <before>:<after>`). A PR whose
// lanes resolve to the other side's cli is then reported under
// `side_cli_disagreements` — the instrument telling on itself, surfaced rather
// than reconciled away. WITHOUT that flag there is no expectation and the field
// is `null`, which is NOT the same fact as `[]`: "nobody said what to expect"
// and "an expectation was checked and nothing disagreed" are different, and a
// table that conflated them would report a clean cross-check it never ran.
//
// NOTHING HERE KNOWS WHICH CLI RAN ON WHICH SIDE, and that is now true of the
// whole function rather than of row construction alone (rev-final round 2).
// Rows are one per `(cli, side)` pair the selected PRs produced; the side
// LABELS come from `--split-label`; the expected pair, if any, comes from
// `--sides`. An earlier revision hardcoded `pre-2817 ? opencode : pi`, which
// made the cross-check fire on 100% of PRs under any other `--split-at` — a
// stale roster wearing the costume of a finding.
// ---------------------------------------------------------------------------

// The clis a block's delegates on one card resolve to. Returns the single cli,
// or `null` plus the reason it could not be reduced to one.
function laneCliOf(card, block) {
  const clis = new Set();
  for (const d of card.delegates.agents) {
    if (d.block !== block) continue;
    clis.add(d.cli);
  }
  if (clis.size === 0) return { cli: null, reason: 'no ' + block + ' delegate' };
  if (clis.size > 1) return { cli: null, reason: block + ' split across ' + [...clis].sort().join('+') };
  const only = [...clis][0];
  if (only === CLI_UNKNOWN || only === 'mixed') return { cli: null, reason: block + ' cli ' + only };
  return { cli: only, reason: null };
}

function creditedFor(card, block, cli) {
  let total = 0;
  let found = false;
  for (const b of Object.values(card.delegates.by_block_cli)) {
    if (b.block !== block || b.cli !== cli) continue;
    total += b.tokens_credited;
    found = true;
  }
  return found ? total : null;
}

// The columns, as one list, so the JSON, the GFM header and the cell order are
// three readings of ONE declaration rather than three hand-kept lists that drift.
const CLI_TABLE_COLUMNS = [
  { key: 'worker_tokens', label: 'worker-std tokens', kind: 'tokens' },
  { key: 'rev_tokens', label: 'rev-std tokens', kind: 'tokens' },
  { key: 'rev_rounds_to_pass', label: 'rev-std rounds to pass', kind: 'plain' },
  { key: 'rev_fail_rate', label: 'rev-std fail rate', kind: 'plain' },
  { key: 'wall_clock_h', label: 'wall clock h', kind: 'plain' },
  // The CONTROL. `rev-final` is Claude on both sides of the split, so a column
  // that moves here is measuring something other than the worker/reviewer CLI —
  // the task mix, the driver changes, the roster's other edits. A control that
  // moves as much as the treatment columns is the table refuting itself.
  { key: 'rev_final_rounds_to_pass', label: 'rev-final rounds to pass (control)', kind: 'plain' },
];

const WORKER_BLOCK = 'worker-std';
const REVIEW_BLOCK = 'rev-std';
const CONTROL_BLOCK = 'rev-final';

// Printed UNDER the table by the script itself, so a table pasted into a comment
// cannot arrive without the reasons not to over-read it. Every entry names the
// issue a reader can check, and none of them is closed by anything in this run.
const CONFOUNDERS = [
  {
    id: 'effective-thinking-level',
    what: 'The roster switch declared `medium` for worker-std and `high` for rev-std, but #2938 reports pi running `high` where `medium` was declared. Until that resolves, the pi side is at an UNKNOWN effective level and a token or round difference may be the level rather than the CLI.',
    issue: 2938,
    // Read off the issue when it lands; never guessed here. `unknown` is the
    // honest value and it prints as such.
    effective_level: 'unknown',
  },
  {
    id: 'task-mix',
    what: 'The two windows hold different PRs. Nothing matched them for size, difficulty or lane count — which is why every cell is a MEDIAN with an IQR and an n, and why a total appears nowhere in this table.',
    issue: 2011,
  },
  {
    id: 'driver-waste-changes',
    what: 'The review driver changed between the windows: #2501 (panes released per side), #2507, #2508 and #2509 (the one-shot body-only round grace). Those moved ROUNDS and PANE COUNT for reasons that have nothing to do with which CLI a lane ran, and they land on the pre-switch side.',
    issue: 2501,
    also: [2507, 2508, 2509],
  },
  {
    id: 'driver-waste-measurement',
    what: 'The size of that driver-waste move is #2812. Cite its figures beside this table rather than attributing the residual to the CLI.',
    issue: 2812,
  },
  {
    id: 'cumulative-usage-rows',
    what: 'Delegate tokens come from `usage.json`, which is cumulative per session and cannot be windowed (H6), and a shared session is split evenly across its occupants (H8). Both bound the precision of the token columns on BOTH sides equally.',
    issue: 2011,
  },
];

// THE COVERAGE FLOOR, and why the table carries it.
//
// `--cut` bounds the log FORWARD (§8) and that is all it can do: it cannot
// recover a row a ROTATION has discarded. This group keeps two audit
// generations and rotates at 8 MB, so a PR whose window predates the oldest
// surviving row is not "excluded" — it is not scored at all, and it drops out
// of the selection with no line in `excluded` to say so, because nothing in
// the log names it any more.
//
// That is not hypothetical: the first posting of this table read 32,943 rows
// across two generations and selected 20 PRs. A rotation at
// 2026-09-06T17:14Z discarded the older generation, and the same command with
// the same `--cut` then read 16,351 rows and selected 10. Both runs were
// correct about the log they could see; only the FLOOR moved.
//
// So the floor travels with the table. A reader comparing two runs of this
// script must compare their floors first — an n that shrank between them is a
// fact about the log, not about the CLIs.
function coverageFloor(cards, groupFiles) {
  let first = null;
  let last = null;
  for (const f of groupFiles) {
    if (typeof f.ts_first === 'number' && (first === null || f.ts_first < first)) first = f.ts_first;
    if (typeof f.ts_last === 'number' && (last === null || f.ts_last > last)) last = f.ts_last;
  }
  // A PR whose window STARTS at the floor may have had earlier rows discarded,
  // so its counters are a lower bound. Named, not silently averaged in.
  const atFloor = cards
    .filter((c) => c.windows.pr && first !== null && c.windows.pr.start_ms <= first)
    .map((c) => c.pr)
    .sort((a, b) => a - b);
  return {
    ts_first: first,
    ts_last: last,
    rows: groupFiles.reduce((n, f) => n + f.rows, 0),
    generations: groupFiles.length,
    prs_touching_the_floor: atFloor,
  };
}

// `sides` is `{ before, after }` or null. `label` names the split for the row
// keys and the footer; it is the caller's word for the instant it passed, so
// the two cannot describe different splits.
function cliTable(cards, splitMs, opts) {
  const sides = (opts && opts.sides) || null;
  const floor = (opts && opts.groupFiles) ? coverageFloor(cards, opts.groupFiles) : null;
  const label = (opts && opts.label) || new Date(splitMs).toISOString();
  const sideName = (which) => (which === 'before' ? 'pre-' : 'post-') + label;
  const selected = [];
  const excluded = [];
  const disagreements = [];
  for (const card of cards) {
    const mergedMs = card.merged_at ? Date.parse(card.merged_at) : NaN;
    if (!Number.isFinite(mergedMs)) { excluded.push({ pr: card.pr, why: 'no merged_at in --pr-meta' }); continue; }
    const w = laneCliOf(card, WORKER_BLOCK);
    const r = laneCliOf(card, REVIEW_BLOCK);
    if (!w.cli || !r.cli) { excluded.push({ pr: card.pr, why: [w.reason, r.reason].filter(Boolean).join('; ') }); continue; }
    if (w.cli !== r.cli) { excluded.push({ pr: card.pr, why: 'worker-std ' + w.cli + ' but rev-std ' + r.cli }); continue; }
    const which = mergedMs < splitMs ? 'before' : 'after';
    const side = sideName(which);
    // Only where the caller declared one. No flag, no expectation, no check.
    const expected = sides ? sides[which] : null;
    if (expected && w.cli !== expected) disagreements.push({ pr: card.pr, side, cli: w.cli, expected });
    const lanes = card.review.lanes || {};
    selected.push({
      pr: card.pr,
      side,
      cli: w.cli,
      merged_at: card.merged_at,
      worker_tokens: creditedFor(card, WORKER_BLOCK, w.cli),
      rev_tokens: creditedFor(card, REVIEW_BLOCK, r.cli),
      rev_rounds_to_pass: lanes[REVIEW_BLOCK] ? lanes[REVIEW_BLOCK].rounds_to_pass : null,
      rev_fail_rate: lanes[REVIEW_BLOCK] ? lanes[REVIEW_BLOCK].fail_rate : null,
      wall_clock_h: card.wall_clock_h,
      rev_final_rounds_to_pass: lanes[CONTROL_BLOCK] ? lanes[CONTROL_BLOCK].rounds_to_pass : null,
    });
  }
  const groups = new Map();
  for (const s of selected) {
    const key = s.cli + ' (' + s.side + ')';
    if (!groups.has(key)) groups.set(key, { key, cli: s.cli, side: s.side, prs: [], cells: {} });
    groups.get(key).prs.push(s.pr);
  }
  for (const g of groups.values()) {
    const mine = selected.filter((s) => s.cli === g.cli && s.side === g.side);
    for (const col of CLI_TABLE_COLUMNS) g.cells[col.key] = statCell(mine.map((s) => s[col.key]));
    g.prs.sort((a, b) => a - b);
  }
  return {
    split_at_ms: splitMs,
    split_label: label,
    coverage_floor: floor,
    // What the caller declared, echoed back so a reader of the JSON knows which
    // expectation produced (or did not produce) the disagreement list.
    sides_declared: sides,
    min_n: MEDIAN_MIN_N,
    columns: CLI_TABLE_COLUMNS.map((c) => ({ key: c.key, label: c.label })),
    rows: [...groups.values()].sort((a, b) => (a.key < b.key ? -1 : 1)),
    per_pr: selected.sort((a, b) => a.pr - b.pr),
    excluded: excluded.sort((a, b) => a.pr - b.pr),
    // `null` — not `[]` — when no expectation was declared.
    side_cli_disagreements: sides ? disagreements.sort((a, b) => a.pr - b.pr) : null,
    confounders: CONFOUNDERS,
  };
}

function fmtCell(cell, kind) {
  if (!cell || cell.median === null) return 'null (n=' + (cell ? cell.n : 0) + ')';
  const f = kind === 'tokens' ? fmtTokens : (v) => String(round2(v));
  return f(cell.median) + ' (IQR ' + f(cell.q1) + '–' + f(cell.q3) + ', n=' + cell.n + ')';
}

function renderCliTable(t) {
  const lines = [];
  lines.push('| lane CLI (window) | PRs | ' + CLI_TABLE_COLUMNS.map((c) => c.label).join(' | ') + ' |');
  lines.push('|' + '---|'.repeat(CLI_TABLE_COLUMNS.length + 2));
  for (const r of t.rows) {
    lines.push('| ' + r.key + ' | ' + r.prs.length
      + ' | ' + CLI_TABLE_COLUMNS.map((c) => fmtCell(r.cells[c.key], c.kind)).join(' | ') + ' |');
  }
  lines.push('');
  lines.push('Median (IQR q1–q3, n) per PR. A cell reads `null` below n=' + t.min_n
    + '. Split at ' + new Date(t.split_at_ms).toISOString()
    + ' (`--split-label ' + t.split_label + '`).');
  if (t.coverage_floor && t.coverage_floor.ts_first !== null) {
    const f = t.coverage_floor;
    lines.push('');
    lines.push('**Coverage floor** — the audit log read here holds ' + f.rows + ' rows across '
      + f.generations + ' generation(s), covering **' + new Date(f.ts_first).toISOString()
      + '** to **' + new Date(f.ts_last).toISOString() + '**. A PR whose window predates that '
      + 'floor is not scored and does not appear in the exclusions either — nothing in the log '
      + 'names it. `--cut` bounds the log forward; it cannot recover rows a rotation discarded, '
      + 'so **compare two runs\' floors before comparing their n**.'
      + (f.prs_touching_the_floor.length
        ? ' Windows starting at the floor, whose counters are therefore a lower bound: '
          + f.prs_touching_the_floor.map((n) => '#' + n).join(', ') + '.'
        : ''));
  }
  lines.push('');
  for (const r of t.rows) lines.push('- **' + r.key + '** — ' + r.prs.map((n) => '#' + n).join(', '));
  if (t.excluded.length) {
    lines.push('');
    lines.push('Excluded (selection is stated, never silent):');
    lines.push('');
    for (const e of t.excluded) lines.push('- #' + e.pr + ' — ' + e.why);
  }
  lines.push('');
  if (!t.sides_declared) {
    // Said out loud, because a MISSING disagreement list and an EMPTY one look
    // identical to a reader and mean opposite things.
    lines.push('**No side/CLI cross-check was run** — `--sides <before>:<after>` was not passed, so the table declares no expectation about which CLI ran on which side of the split.');
  } else {
    lines.push('Expected per `--sides`: **' + t.sides_declared.before + '** before the split, **'
      + t.sides_declared.after + '** after.');
    if (t.side_cli_disagreements.length) {
      lines.push('');
      lines.push('**Side/CLI disagreements** — the declared expectation and the resolved CLI do not agree here:');
      lines.push('');
      for (const d of t.side_cli_disagreements) lines.push('- #' + d.pr + ' — ' + d.side + ' but resolved ' + d.cli + ' (expected ' + d.expected + ')');
    } else {
      lines.push('');
      lines.push('Every selected PR matched that expectation.');
    }
  }
  lines.push('');
  lines.push('**Confounders** — read before the table:');
  lines.push('');
  for (const c of t.confounders) {
    lines.push('- **' + c.id + '** (#' + c.issue
      + (c.also ? ', ' + c.also.map((n) => '#' + n).join(', ') : '') + ') — ' + c.what
      + (c.effective_level ? ' Effective level as read today: `' + c.effective_level + '`.' : ''));
  }
  return lines.join('\n');
}

// The cli axis's own coverage. The population control for H10: `by_rung` says
// which rung answered for how many delegate slots, and `unknown` is the count
// the axis could not answer at all. A cli table read off a run whose `unknown`
// is a large share of `delegate_slots` is a table about a thin population, and
// nothing else in the output would say so.
//
// Counted at the VERIFIED site — one entry per delegate ON A CARD, which is
// where a cli is actually used — not at the usage rows scanned, which would
// certify coverage the table never received.
function cliAxisCoverage(cards, spawnCli, cliConflicts) {
  const byRung = { 'usage-source': 0, 'spawn-row': 0, 'spawn-row-session-conflict': 0, none: 0 };
  const byCli = {};
  let slots = 0;
  for (const card of cards) {
    for (const d of card.delegates.agents) {
      slots += 1;
      byRung[d.cli_via === null ? 'none' : d.cli_via] += 1;
      byCli[d.cli] = (byCli[d.cli] || 0) + 1;
    }
  }
  return {
    delegate_slots: slots,
    by_rung: byRung,
    by_cli: byCli,
    unknown: byCli[CLI_UNKNOWN] || 0,
    spawn_rows_with_cli: spawnCli.spawn_rows_with_cli,
    spawn_rows_without_cli: spawnCli.spawn_rows_without_cli,
    // Every session whose occupants did not all run one CLI, with the split.
    // A non-empty list here means H8's even split is crossing a CLI boundary on
    // this store and the per-session `source` label is answering for panes it
    // does not describe.
    sessions_with_conflicting_clis: [...(cliConflicts || new Map()).entries()]
      .map(([session, byAgent]) => ({
        session,
        agents: [...byAgent.entries()].sort().map(([a, c]) => a + '=' + c),
      }))
      .sort((a, b) => (a.session < b.session ? -1 : 1)),
  };
}

// The heuristics this reader has to use. This list IS #2011 B2's scope — every row
// is a place where one structural field would replace a guess.
const HEURISTICS = [
  { id: 'H1', what: 'A PR is joined to an audit row by the `#N` token in the serialized `detail` wherever `detail.pr` is absent.', fix: 'A `pr` field on `prompt` / `delivery-queued` rows (plan part 2, A1 / missing row 2).' },
  { id: 'H2', what: 'An agent is joined to a PR by the `#N` token in its `agent-spawn` detail (brief text, name, branch) when no `rd-*` or `review-verdict` row carries both.', fix: 'A `pr` field on `agent-spawn`.' },
  { id: 'H3', what: 'A text-tier attribution counts only when the `agent-spawn` row falls inside the PR window; outside it the same token is ignored.', fix: 'Same as H2 — the window bound exists only because the token is ambiguous.' },
  { id: 'H4', what: "An agent attributed to k PRs contributes 1/k of its lifetime tokens to each — and k counts only the PRs in THIS run's selection, so running one PR alone gives its shared agents full weight. Always run the whole comparison set together.", fix: 'A `block` and a `pr` on `UsageSnapshot` (plan part 2, A6 / missing row 4).' },
  { id: 'H5', what: "Orchestrator tokens in a window cover every PR in flight; the apportioned figure splits them by this PR's share of the orchestrator wakes in the same window.", fix: 'Per-turn PR attribution — nothing structural exists; see "What this cannot say" in the note.' },
  { id: 'H6', what: 'Delegate tokens come from `usage.json`, which is CUMULATIVE per session and cannot be windowed; a delegate that worked on one PR reports its whole life against that PR.', fix: 'A windowed usage series, or accepting the approximation (delegates are spawned per task).' },
  { id: 'H7', what: 'A PR window ends at `merged_at` supplied via `--pr-meta`; without it the end falls back to the last `rd-*` row, then to the last naming row — which is days late, because a merged PR stays cited.', fix: 'A loomux row for a human merge (plan part 2, A4 / #388).' },
  { id: 'H8', what: "A `usage.json` row is keyed by CLI SESSION, and a session carried to a new agent id names only its LAST occupant. The row is therefore split evenly across every agent that occupied that session — a guess about how a shared session's spend divided, self-correcting where the whole lineage is attributed to one PR and a fraction where it is not.", fix: 'An `agent_id` (or `block` + `pr`) on every `UsageSnapshot`, not just the latest — the same missing field as H4.' },
  { id: 'H9', what: "A zero-token `usage.json` row whose Claude transcript is on disk is backfilled by summing that transcript, which is UNCUT and UNPRICED: `--cut` cannot rewind a `usage.json` row either (a row is cumulative-to-now with no history), so cutting a backfilled row and not its neighbours would make the two kinds disagree about what instant the table describes; and no dollar figure is derived, so a backfilled row keeps the `cost_usd` it had — its tokens are right and its cost is still whatever the collector recorded.", fix: "The collector recording the row correctly in the first place (#2167's own fix, shipped alongside this) — after which nothing is backfilled and `rows` reads 0." },
  { id: 'H10', what: "A delegate's CLI is READ OFF the `source` label of the `usage.json` row carrying its tokens (`transcript`/`transcript-backfill` -> claude, `pi-transcript` -> pi, `session-db` -> opencode, `codex-transcript` -> codex), because that label names the per-CLI record the collector folded. A row whose source is `statusline` or `none` says only that some CLI printed a figure, so it falls back to the `cli` on that agent's `agent-spawn` row, and to `unknown` when neither answers — reported as `unknown` with `cli_via: null`, never filled in from the block's declared CLI, since a pane can be recycled onto a block whose roster line has since changed. `cli_via` says which rung answered.", fix: 'A `cli` field on `UsageSnapshot` (and on the orchestrator `agent-spawn` site, which unlike the delegate site carries none) — t-664\'s family, the same missing-field fix as H4/H8.' },
];

// ---------------------------------------------------------------------------
// CLI.
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const opts = {
    audit: [], transcript: [], usage: null, agents: null, prMeta: null,
    prs: [], all: false, tailMin: DEFAULT_TAIL_MIN, format: 'json', cut: null, help: false,
    claudeProjects: null, backfill: true, splitAt: null, splitLabel: null, sides: null,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    const next = () => argv[++i];
    switch (a) {
      case '--audit': opts.audit.push(next()); break;
      case '--transcript': opts.transcript.push(next()); break;
      case '--usage': opts.usage = next(); break;
      case '--agents': opts.agents = next(); break;
      case '--pr-meta': opts.prMeta = next(); break;
      case '--claude-projects': opts.claudeProjects = next(); break;
      case '--no-backfill': opts.backfill = false; break;
      case '--pr': opts.prs.push(Number(next())); break;
      case '--all': opts.all = true; break;
      case '--tail-min': opts.tailMin = Number(next()); break;
      case '--format': opts.format = next(); break;
      case '--split-label': opts.splitLabel = next(); break;
      // `<before>:<after>`, e.g. `opencode:pi`. Both halves required, so a
      // half-declared expectation cannot silently check one side only.
      case '--sides': {
        const raw = String(next());
        const parts = raw.split(':');
        if (parts.length !== 2 || !parts[0] || !parts[1]) {
          throw new Error('--sides wants <before-cli>:<after-cli>, got: ' + raw);
        }
        opts.sides = { before: parts[0], after: parts[1] };
        break;
      }
      case '--split-at': opts.splitAt = /^\d+$/.test(String(argv[i + 1])) ? Number(next()) : Date.parse(next()); break;
      case '--cut': opts.cut = /^\d+$/.test(String(argv[i + 1])) ? Number(next()) : Date.parse(next()); break;
      case '--help': case '-h': opts.help = true; break;
      default: throw new Error('unknown argument: ' + a);
    }
  }
  return opts;
}

const USAGE_TEXT = `orch-scorecard — per-PR orchestration cost from existing logs (#2011 B1)

  node scripts/orch-scorecard.cjs --audit <audit.jsonl> [--audit <audit.1.jsonl>]
      --usage <usage.json> --agents <agents.json>
      [--transcript <session.jsonl>]... [--pr-meta <meta.json>]
      (--pr <n> [--pr <n>]... | --all)
      [--tail-min 10] [--cut <ms|iso>] [--format json|table|both|cli-table]
      [--split-at <ms|iso>] [--split-label <text>] [--sides <before>:<after>]
      [--claude-projects <dir>] [--no-backfill]

  --all        every PR that any rd-* row names (the driven set).
  --pr-meta    {"2104": {"merged_at": "...", "build": "beta5", "issue": 2010}} —
               without merged_at a PR window ends at its last rd row (coverage says so).
  --cut        drop audit rows and transcript turns after this instant; reproduces a
               historical measurement on a log that has since grown.
  --claude-projects
               where Claude Code keeps its per-project transcript folders
               (default ~/.claude/projects). A usage.json row with four zero
               token counters whose session transcript is there is summed from it
               (#2167); coverage says how many rows and from which files.
  --no-backfill
               leave zero rows at zero. Coverage still reports how many there are.
  --split-at   the instant that divides the two comparison windows. Required by
               --format cli-table; a PR's window is decided by its merged_at
               against this, independently of the CLI read off its rows.
               For the #2817 roster switch: 2026-09-06T10:36:55Z, commit
               93d51cc9 — passed in, never assumed by this script.
  --split-label
               what to call that split in the row keys and the footer
               (e.g. "2817"). Defaults to the instant's ISO form, so the label
               and the instant can never describe two different splits.
  --sides      <before-cli>:<after-cli>, e.g. "opencode:pi" — the expectation
               the side/CLI cross-check is measured against. WITHOUT it no
               cross-check runs and side_cli_disagreements is null, because
               this script knows nothing about which CLI ran when.
  --format cli-table
               the CLI comparison (#2011 A): median + IQR + n per column per
               (cli, window), null below n=3, rev-final rounds as the
               Claude-both-sides control, and the confounder block.
`;

async function main(argv) {
  const opts = parseArgs(argv);
  if (opts.help || opts.audit.length === 0) { process.stdout.write(USAGE_TEXT); return 0; }
  if (!opts.usage || !opts.agents) throw new Error('--usage and --agents are required');

  const cut = Number.isFinite(opts.cut) ? opts.cut : null;
  const tailMs = opts.tailMin * 60000;
  const files = opts.audit.map((p) => readJsonl(p, cut));
  const rows = files.flatMap((f) => f.rows).sort((a, b) => (a.ts_ms || 0) - (b.ts_ms || 0));
  const agents = JSON.parse(fs.readFileSync(opts.agents, 'utf8'));
  const rawUsage = JSON.parse(fs.readFileSync(opts.usage, 'utf8'));
  const prMeta = opts.prMeta ? JSON.parse(fs.readFileSync(opts.prMeta, 'utf8')) : {};

  // #2167: a zero row whose transcript is on disk is summed from it, BEFORE the
  // usage index is built, so every counter downstream sees one kind of row.
  const projectsRoot = opts.claudeProjects || defaultClaudeProjectsRoot();
  const { usage, backfill } = opts.backfill
    ? await backfillZeroUsageRows(rawUsage, projectsRoot)
    : { usage: rawUsage, backfill: { claude_projects_root: projectsRoot, scanned: false, projects_scanned: 0, transcripts_indexed: 0, zero_rows_considered: (Array.isArray(rawUsage) ? rawUsage : []).filter(isZeroUsageRow).length, rows: 0, tokens: 0, from: [], zero_rows_without_a_transcript: [], zero_rows_whose_transcript_summed_to_zero: [], zero_rows_with_no_session_id: [], disabled: true } };

  const agentsById = indexAgents(agents);
  const orchIds = new Set([...agentsById.values()].filter((a) => a.role === 'orchestrator').map((a) => a.id));
  const { bySession: usageBySession, byAgent: usageByAgent, unusable: usageUnusable } = indexUsage(usage);
  const sessionAgents = indexSessionAgents(agents);

  const transcripts = [];
  for (const p of opts.transcript) transcripts.push(await readTranscript(p, cut));
  const transcriptTurns = transcripts.flatMap((t) => t.turns).sort((a, b) => a.ts_ms - b.ts_ms);

  let prs = opts.prs.slice();
  if (opts.all) {
    const seen = new Set(prs);
    for (const r of rows) {
      if (typeof r.action === 'string' && r.action.startsWith('rd-') && r.detail && typeof r.detail.pr === 'number') seen.add(r.detail.pr);
    }
    prs = [...seen];
  }
  prs = [...new Set(prs)].filter((n) => Number.isFinite(n)).sort((a, b) => a - b);
  if (prs.length === 0) throw new Error('no PRs selected: pass --pr <n> or --all');

  const windowsByPr = new Map();
  for (const pr of prs) {
    const meta = prMeta[String(pr)] || {};
    const mergedMs = meta.merged_at ? Date.parse(meta.merged_at) : null;
    windowsByPr.set(pr, computeWindows(rows, pr, Number.isFinite(mergedMs) ? mergedMs : null, tailMs));
  }
  const attribution = attributeAgents(rows, prs, windowsByPr);

  const spawnCli = indexSpawnCli(rows);
  const cliConflicts = indexCliConflicts(agents, spawnCli.byAgent);
  const ctx = { rows, orchIds, tailMs, transcriptTurns, usageBySession, usageByAgent, sessionAgents, agentsById, attribution, prMeta, spawnCliByAgent: spawnCli.byAgent, cliConflicts };
  const cards = prs.map((pr) => scorePr(ctx, pr));

  const spawnedInWindow = new Set();
  for (const r of rows) {
    if (r.action !== 'agent-spawn' || !r.detail || !r.detail.agent) continue;
    for (const pr of prs) {
      const w = windowsByPr.get(pr);
      if (w && w.pr && inWindow(r.ts_ms, w.pr)) { spawnedInWindow.add(r.detail.agent); break; }
    }
  }
  const unattributed = [...spawnedInWindow].filter((a) => !attribution.has(a)).sort();
  const split = [...attribution.entries()]
    .filter(([, v]) => v.prs.size > 1)
    .map(([a, v]) => ({ agent: a, prs: [...v.prs].sort((x, y) => x - y), tier: v.tier }));

  const out = {
    generated_ms: Date.now(),
    inputs: {
      audit: opts.audit, usage: opts.usage, agents: opts.agents,
      transcript: opts.transcript, pr_meta: opts.prMeta, tail_min: opts.tailMin, cut_ms: cut,
      claude_projects: projectsRoot, backfill: opts.backfill,
    },
    group: { files: groupTotals(files, orchIds) },
    prs: cards,
    coverage: {
      audit_rows_read: rows.length,
      audit_parse_errors: files.reduce((n, f) => n + f.parseErrors, 0),
      rows_classified: cards.reduce((n, c) => n + c.rows_classified, 0),
      orchestrator_agent_ids: orchIds.size,
      transcripts: transcripts.map((t) => ({
        path: t.path, lines: t.lines, assistant_usage_lines: t.assistant_usage_lines,
        deduped_turns: t.turns.length, usage_rows_without_id: t.without_id,
      })),
      usage_rows_unusable: usageUnusable,
      usage_rows_backfilled_from_transcript: backfill,
      usage_sessions_indexed: usageBySession.size,
      usage_sessions_shared_by_more_than_one_agent: [...sessionAgents.values()].filter((v) => v.length > 1).length,
      agents_attributed: attribution.size,
      agents_unattributed_spawned_in_window: unattributed,
      agents_split_across_prs: split,
      // The cli axis's own coverage (H10): how many delegates each rung answered
      // for, and how many are `unknown`. A run whose `unknown` count is high has
      // a cli table built on a thin population, and this is where that shows.
      cli_axis: cliAxisCoverage(cards, spawnCli, cliConflicts),
      heuristics: HEURISTICS,
    },
  };

  if (opts.format === 'json' || opts.format === 'both') process.stdout.write(JSON.stringify(out, null, 2) + '\n');
  if (opts.format === 'table' || opts.format === 'both') {
    process.stdout.write('\n' + renderPrTable(cards) + '\n\n' + renderGroupTable(out.group.files) + '\n');
  }
  if (opts.format === 'cli-table') {
    if (!Number.isFinite(opts.splitAt)) throw new Error('--format cli-table needs --split-at <ms|iso>');
    process.stdout.write('\n' + renderCliTable(cliTable(cards, opts.splitAt,
      { sides: opts.sides, label: opts.splitLabel, groupFiles: out.group.files })) + '\n');
  }
  return 0;
}

module.exports = {
  WAKE_KINDS, classifyWake, prTokenRe, rowNamesPr, computeWindows, inWindow,
  dedupeTranscriptTurns, attributeAgents, scorePr, groupTotals, indexAgents,
  indexUsage, indexSessionAgents, renderPrTable, renderGroupTable, parseArgs, HEURISTICS,
  usageRowTokens, isZeroUsageRow, isAgentKeyedRow, reconcileBackfill,
  SOURCE_TO_CLI, CLI_UNKNOWN, cliForSource, resolveCli, indexSpawnCli,
  resolveDelegateCli, indexCliConflicts, blockCliKey, laneStats, MEDIAN_MIN_N, medianOf, statCell,
  laneCliOf, creditedFor, cliTable, renderCliTable, cliAxisCoverage, coverageFloor,
  CLI_TABLE_COLUMNS, CONFOUNDERS,
  claudeTranscriptIndex, backfillZeroUsageRows, defaultClaudeProjectsRoot,
  DEFAULT_TAIL_MIN, main,
};

if (require.main === module) {
  main(process.argv.slice(2)).then((code) => { process.exitCode = code; }, (err) => {
    process.stderr.write('orch-scorecard: ' + (err && err.message ? err.message : String(err)) + '\n');
    process.exitCode = 1;
  });
}
