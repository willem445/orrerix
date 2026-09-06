// Builds `test/fixtures/orchscorecard/clitable/` — the corpus for `--format
// cli-table` (#2011 A).
//
// Its axes are chosen so no two counters share a value and every branch of the
// selection has a subject (#1182):
//
//   pre-2817  (opencode): #800 #801 #802 #803  — n=4, so the medians resolve
//   post-2817 (pi):       #810 #811            — n=2, so every cell is `null`
//                                                and the n<3 refusal is fail-able
//   excluded: #820 worker-std split opencode+pi (the switch itself, not a side)
//             #821 no merged_at
//             #822 worker-std pi but rev-std opencode
//   disagreement: #803 merged BEFORE the split but its lanes resolve to pi
//
// #820 is also the plan's same-block split: its ONE PR carries two `worker-std`
// delegates on two different clis, so `by_block_cli` has to key on `block/cli`
// rather than on block for it to keep them apart.
//
// rev-final is `transcript` (claude) on EVERY PR — that is what makes it the
// control: a column that moves there moved for a reason that is not the
// worker/reviewer CLI.
const fs = require('fs');
const path = require('path');

const dir = __dirname;
fs.mkdirSync(dir, { recursive: true });

const SPLIT = Date.parse('2026-09-06T10:36:55Z');
const HOUR = 3600000;
const T0 = Date.parse('2026-09-01T00:00:00Z');

// pr -> { cli, workerCli?, revCli?, spanH, revSeq, finalSeq, workerTok, revTok, mergedOffsetH }
const PRS = [
  { pr: 800, worker: 'opencode', rev: 'opencode', crossCliSession: true, spanH: 2, revSeq: ['fail', 'pass'], finalSeq: ['pass'], workerTok: 1000000, revTok: 400000 },
  // #801 straddles the coverage floor (see `truncatedSpawnOf`).
  { pr: 801, worker: 'opencode', rev: 'opencode', truncatedSpawnOf: 'w-801', spanH: 4, revSeq: ['fail', 'fail', 'pass'], finalSeq: ['fail', 'pass'], workerTok: 2000000, revTok: 800000 },
  { pr: 802, worker: 'opencode', rev: 'opencode', spanH: 6, revSeq: ['pass'], finalSeq: ['pass'], workerTok: 3000000, revTok: 1200000 },
  // Merged BEFORE the split but its lanes are pi: the side/cli cross-check fires.
  { pr: 803, worker: 'pi', rev: 'pi', spanH: 8, revSeq: ['fail', 'pass'], finalSeq: ['pass'], workerTok: 4000000, revTok: 1600000 },
  // The post-switch side: n=2, so every cell is null.
  { pr: 810, worker: 'pi', rev: 'pi', spanH: 1, revSeq: ['pass'], finalSeq: ['pass'], workerTok: 500000, revTok: 200000, after: true },
  { pr: 811, worker: 'pi', rev: 'pi', spanH: 3, revSeq: ['fail', 'pass'], finalSeq: ['pass'], workerTok: 700000, revTok: 300000, after: true },
  // Excluded: two worker-std panes on two clis — the switch, not a side.
  { pr: 820, worker: 'opencode', workerSecond: 'pi', rev: 'pi', spanH: 5, revSeq: ['pass'], finalSeq: ['pass'], workerTok: 900000, revTok: 350000, after: true },
  // Excluded: never merged.
  { pr: 821, worker: 'pi', rev: 'pi', spanH: 5, revSeq: ['pass'], finalSeq: ['pass'], workerTok: 900000, revTok: 350000, after: true, unmerged: true },
  // Excluded: the two compared lanes disagree with each other.
  { pr: 822, worker: 'pi', rev: 'opencode', spanH: 5, revSeq: ['pass'], finalSeq: ['pass'], workerTok: 900000, revTok: 350000, after: true },
];

const SOURCE_OF = { opencode: 'session-db', pi: 'pi-transcript', claude: 'transcript' };

const agents = [{ id: 'orch-80', role: 'orchestrator', block: 'orchestrator', name: 'orchestrator', session: 'ses-orch-80' }];
const usage = [{
  key: 'ses-orch-80', agent_id: 'orch-80', name: 'orchestrator', role: 'orchestrator',
  source: 'transcript', input_tokens: 1, output_tokens: 1, cache_creation_tokens: 1,
  cache_read_tokens: 1, cost_usd: 0.01, estimated: true, model: 'synthetic', updated_ms: T0,
}];
const audit = [];
const prMeta = {};

function addAgent(id, role, block, cli, tokens, pr, spawnMs, withSpawnCli) {
  const ses = 'ses-' + id;
  agents.push({ id, role, block, name: role + ' #' + pr, session: ses });
  usage.push({
    key: ses, agent_id: id, name: role + ' #' + pr, role,
    source: SOURCE_OF[cli],
    input_tokens: Math.round(tokens * 0.1), output_tokens: Math.round(tokens * 0.02),
    cache_creation_tokens: Math.round(tokens * 0.08), cache_read_tokens: Math.round(tokens * 0.8),
    cost_usd: null, estimated: true, model: 'synthetic', updated_ms: T0,
  });
  const detail = { agent: id, block, role, task: 'work #' + pr };
  if (withSpawnCli) detail.cli = cli;
  audit.push({ action: 'agent-spawn', actor: 'orrerix', detail, ts_ms: spawnMs });
}

let idx = 0;
for (const p of PRS) {
  idx += 1;
  // A post-switch PR is BUILT after the split instant and a pre-switch one
  // before it, so `merged_at` decides the side the way the real clock does.
  // `merged_at` IS the window end, so `wall_clock_h` reads back exactly `spanH`.
  const start = p.after ? SPLIT + idx * 24 * HOUR : T0 + idx * 24 * HOUR;
  const end = start + p.spanH * HOUR;
  const mergedMs = end;
  if (!p.unmerged) prMeta[String(p.pr)] = { merged_at: new Date(mergedMs).toISOString(), build: p.after ? 'beta9' : 'beta8', issue: p.pr - 100 };

  // A structural `rd-lane-spawned` row per delegate: attribution is structural,
  // so no text-tier heuristic is in play and the table's population is exact.
  const mk = (id, role, block, cli, tokens, withSpawnCli) => {
    addAgent(id, role, block, cli, tokens, p.pr, start, withSpawnCli);
    audit.push({ action: 'rd-lane-spawned', actor: 'orrerix', detail: { pr: p.pr, agent: id, block }, ts_ms: start + 1 });
    // The straddler: drop this delegate's `agent-spawn` row, as a rotation
    // would have. The `rd-lane-spawned` row that credits it stays, so the PR
    // is scored with a window that begins AFTER the floor and counters that
    // are silently short.
    if (p.truncatedSpawnOf === id) {
      const i = audit.findIndex((r) => r.action === 'agent-spawn' && r.detail.agent === id);
      if (i === -1) throw new Error('no agent-spawn row to truncate for ' + id);
      audit.splice(i, 1);
    }
  };
  mk('w-' + p.pr, 'worker', 'worker-std', p.worker, p.workerTok, Boolean(p.crossCliSession));
  if (p.crossCliSession) {
    // A PANE RECYCLED ACROSS CLIs — measured on the live store, not invented:
    // `agents.json` sessions 358b100f… and e81c5d8a… are each shared by a
    // `worker-adv` (claude) and a `worker-std` (opencode) agent. The usage row
    // is keyed by that one session, so its `source` label answers for BOTH, and
    // rung 1 alone would call this opencode worker a claude one.
    //
    // Here the row is `transcript` (claude) and the worker-std pane is opencode,
    // so #800 leaves the opencode side unless the conflict rung fires — and the
    // side falls to n=2, where every cell reads null.
    const shared = 'ses-w-' + p.pr;
    agents.push({ id: 'wadv-' + p.pr, role: 'worker', block: 'worker-adv', name: 'worker-adv #' + p.pr, session: shared });
    audit.push({ action: 'agent-spawn', actor: 'orrerix', detail: { agent: 'wadv-' + p.pr, block: 'worker-adv', cli: 'claude', role: 'worker', task: 'earlier occupant of #' + p.pr }, ts_ms: start - 1 });
    audit.push({ action: 'rd-lane-spawned', actor: 'orrerix', detail: { pr: p.pr, agent: 'wadv-' + p.pr, block: 'worker-adv' }, ts_ms: start + 1 });
    const row = usage.find((u) => u.key === shared);
    row.source = 'transcript';
  }
  if (p.workerSecond) mk('w2-' + p.pr, 'worker', 'worker-std', p.workerSecond, p.workerTok, false);
  // The rev-std pane's usage row is `statusline` on #802 alone, so the SPAWN-ROW
  // rung carries one selected PR: a table that only ever read the usage source
  // would drop #802 from the opencode side and the n=4 cells would move.
  const revStatusline = p.pr === 802;
  const revId = 'rev-' + p.pr;
  agents.push({ id: revId, role: 'reviewer', block: 'rev-std', name: 'rev-std #' + p.pr, session: 'ses-' + revId });
  usage.push({
    key: 'ses-' + revId, agent_id: revId, name: 'rev-std #' + p.pr, role: 'reviewer',
    source: revStatusline ? 'statusline' : SOURCE_OF[p.rev],
    input_tokens: Math.round(p.revTok * 0.1), output_tokens: Math.round(p.revTok * 0.02),
    cache_creation_tokens: Math.round(p.revTok * 0.08), cache_read_tokens: Math.round(p.revTok * 0.8),
    cost_usd: null, estimated: true, model: 'synthetic', updated_ms: T0,
  });
  audit.push({ action: 'agent-spawn', actor: 'orrerix', detail: { agent: revId, block: 'rev-std', cli: p.rev, role: 'reviewer', task: 'review #' + p.pr }, ts_ms: start });
  audit.push({ action: 'rd-lane-spawned', actor: 'orrerix', detail: { pr: p.pr, agent: revId, block: 'rev-std' }, ts_ms: start + 1 });

  mk('revf-' + p.pr, 'reviewer', 'rev-final', 'claude', 100000, false);

  let t = start + 2;
  for (const v of p.revSeq) {
    t += 60000;
    audit.push({ action: 'review-verdict', actor: revId, detail: { pr: p.pr, block: 'rev-std', verdict: v }, ts_ms: t });
  }
  for (const v of p.finalSeq) {
    t += 60000;
    audit.push({ action: 'review-verdict', actor: 'revf-' + p.pr, detail: { pr: p.pr, block: 'rev-final', verdict: v }, ts_ms: t });
  }
  // The row that ENDS the PR window when there is no merge time, and that keeps
  // the untailed span exactly `spanH` for the merged ones (merged_at is the end).
  audit.push({ action: 'rd-satisfied', actor: 'orrerix', detail: { pr: p.pr }, ts_ms: end });
}

audit.sort((a, b) => a.ts_ms - b.ts_ms);
const w = (f, s) => fs.writeFileSync(path.join(dir, f), s.replace(/\n/g, '\r\n'));
w('audit.jsonl', audit.map((r) => JSON.stringify(r)).join('\n') + '\n');
w('agents.json', JSON.stringify(agents, null, 2) + '\n');
w('usage.json', JSON.stringify(usage, null, 2) + '\n');
w('pr-meta.json', JSON.stringify(prMeta, null, 2) + '\n');
console.log('wrote', dir, audit.length, 'audit rows,', agents.length, 'agents,', usage.length, 'usage rows');
