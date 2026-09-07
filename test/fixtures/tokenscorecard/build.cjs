// Builds `audit.jsonl` for the tokens-pane scorecard tests (#2011 slice D) —
// CUT FROM THE REAL GROUP LOG, not hand-written. The scorecard's inputs are
// the pane's `AuditStore` read, and a fixture cut from the real log carries
// the real row shapes (`rd-lane-spawned` detail with head/session/round,
// `review-verdict` with its verdict vocabulary) that a hand-written corpus
// inevitably simplifies away. Same discipline as `../orchscorecard/clitable`:
// the builder is committed so a regeneration is a diff.
//
// Source: the loomux orchestration group's own audit log (two generations,
// `audit.1.jsonl` then `audit.jsonl`, file order preserved — the arrival-order
// metric needs it). Rows are selected for a fixed set of PRs — the review-loop
// lanes of #2011's own slices A/B/C — plus every `agent-spawn` row naming an
// agent those rows credit, so the spawn-row-missing floor has both real
// positives (a rotation really did drop some) and real negatives.
//
// PROJECTION (stated because a fixture that silently rewrites its rows is a
// lie with a provenance note): the scorecard reads only `ts_ms`, `action`,
// `actor`, and `detail.pr` / `detail.agent` / `detail.block` /
// `detail.verdict`. Rows for the actions that carry those fields are kept
// WHOLE. Every other selected row is projected to
// `{"action","actor","ts_ms","detail":{"pr":<d.pr when numeric>,"names":[…]}}`
// where `names` lists every selected PR whose `#N` token occurs anywhere in
// the original serialized detail — the exact facts the window's
// named-row/last-naming-row arms read, and nothing else. Long prompt and
// tool payloads are dropped, not truncated, so no row can half-carry a fact.
//
// One DELETION is deliberate: the spawn rows of `SPAWN_DROP` are removed, so
// the fixture carries the rotation shape for real — a PR credited with a
// delegate whose `agent-spawn` row did not survive the read. Without it the
// extraction rule ("keep every credited agent's spawn row") would make the
// coverage floor's spawn-row-missing path unfirable on this corpus, and a
// fixture that cannot red is a decoration. The agent is a round-1 `rev-std`
// reviewer whose PR's lane still resolves through its later-round agents, so
// the deletion moves the floor without moving the table.
//
// Re-run: `node test/fixtures/tokenscorecard/build.cjs <group-dir>` — then
// re-derive every count `test/tokenscorecard.test.ts` pins. A regeneration
// that moves a number is a re-bless, not a fix.

'use strict';
const fs = require('node:fs');
const path = require('node:path');

const groupDir = process.argv[2];
if (!groupDir) {
  console.error('usage: node build.cjs <path-to-orrerix-group-dir>');
  process.exit(2);
}

const PRS = [2941, 2942, 2943, 2947, 3038];
// Spawn rows NOT copied — see the PROJECTION note above.
const SPAWN_DROP = new Set(['rev-2416']);
const prSet = new Set(PRS);
const prRe = new Map(PRS.map((p) => [p, new RegExp('#' + p + '(?![0-9])')]));

// Actions whose detail carries a field the scorecard reads.
const WHOLE_DETAIL = new Set(['agent-spawn', 'review-verdict', 'rd-lane-spawned', 'rd-handback']);

function namesPr(detail, detailBlob, pr) {
  if (detail && detail.pr === pr) return true;
  return prRe.get(pr).test(detailBlob);
}

function project(row) {
  const d = row.detail && typeof row.detail === 'object' ? row.detail : null;
  if (d && WHOLE_DETAIL.has(row.action)) return row;
  const blob = JSON.stringify(row.detail ?? null);
  const names = PRS.filter((pr) => namesPr(row.detail, blob, pr));
  const detail = {};
  if (typeof d?.pr === 'number') detail.pr = d.pr;
  if (names.length) detail.names = names;
  return { action: row.action, actor: row.actor, ts_ms: row.ts_ms, detail };
}

const generations = ['audit.1.jsonl', 'audit.jsonl'];
const selected = [];
const credited = new Set();
for (const gen of generations) {
  const full = path.join(groupDir, gen);
  if (!fs.existsSync(full)) continue;
  for (const line of fs.readFileSync(full, 'utf8').split('\n')) {
    if (!line.trim()) continue;
    let row;
    try { row = JSON.parse(line); } catch { continue; }
    // SPAWN_DROP rows never enter the corpus, whichever pass found them — a
    // spawn row whose brief names a PR would otherwise re-enter through the
    // selection pass below and undo the deletion.
    if (row.action === 'agent-spawn' && SPAWN_DROP.has(row.detail?.agent)) continue;
    const blob = JSON.stringify(row.detail ?? null);
    const hit = PRS.some((pr) => namesPr(row.detail, blob, pr));
    if (!hit) continue;
    selected.push(row);
    const d = row.detail;
    if (d && typeof d === 'object' && typeof d.agent === 'string') credited.add(d.agent);
    if (row.action === 'review-verdict' && typeof row.actor === 'string') credited.add(row.actor);
  }
}
// The spawn rows of every credited agent, from both generations, file order.
const spawns = [];
for (const gen of generations) {
  const full = path.join(groupDir, gen);
  if (!fs.existsSync(full)) continue;
  for (const line of fs.readFileSync(full, 'utf8').split('\n')) {
    if (!line.trim()) continue;
    let row;
    try { row = JSON.parse(line); } catch { continue; }
    if (row.action !== 'agent-spawn') continue;
    if (row.detail && credited.has(row.detail.agent) && !SPAWN_DROP.has(row.detail.agent)) spawns.push(row);
  }
}

const out = [...spawns, ...selected].map(project).map((r) => JSON.stringify(r));
fs.writeFileSync(path.join(__dirname, 'audit.jsonl'), out.join('\n') + '\n');
const actionCounts = {};
for (const r of [...spawns, ...selected]) actionCounts[r.action] = (actionCounts[r.action] || 0) + 1;
console.log(`wrote ${out.length} rows for PRs ${PRS.join(', ')}; agents credited: ${credited.size}`);
console.log(JSON.stringify(actionCounts));
