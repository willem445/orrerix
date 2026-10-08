#!/usr/bin/env node
// Reconcile docs/design/remote-engine-protocol.md §5.4's roster table against
// src-tauri/src/command_manifest.rs's APP_COMMANDS — by parsing BOTH sides.
//
// The table partitions the manifest by family and classifies each part, and it
// went stale every time a command was added, because the counts were bumped by
// hand (the note's own §5.4 lists the occasions). This reads each side's real
// delimiters and compares them, so a family that gained a command without
// gaining a disposition is a named difference rather than a silent one.
//
// What it checks, per family and in total:
//   - the manifest's entries under a `// family (N)` comment, COUNTED (the N in
//     the comment is compared too, since that is a second hand-kept number);
//   - the doc's `| **family** (N) |` header against the sum of that family's
//     `n` column;
//   - both against each other, and the doc's class totals against the table.
//
// What it does NOT check: that a NAMED command sits in the right row. The
// table names some commands and summarises others, so a per-command
// disposition is not parseable from it — that stays the roster test the note
// assigns to its slice C. This is the count-level half.
//
// Usage: node scripts/command-roster.cjs            (prints a report, exit 1 on a difference)
//        require('./command-roster.cjs').reconcile() (the same result as data)
const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(__dirname, '..');
const MANIFEST = 'src-tauri/src/command_manifest.rs';
const DOC = 'docs/design/remote-engine-protocol.md';

/** The manifest's families, in order: name, the N its comment claims, and the
 *  command names actually listed under it. */
function parseManifest(src) {
  const start = src.indexOf('pub const APP_COMMANDS');
  if (start < 0) throw new Error(`${MANIFEST}: no APP_COMMANDS`);
  const end = src.indexOf('\n];', start);
  if (end < 0) throw new Error(`${MANIFEST}: APP_COMMANDS is not closed`);
  const families = [];
  for (const raw of src.slice(start, end).split(/\r?\n/)) {
    const line = raw.trim();
    // A family header is a comment that opens `name (N`. Other comments —
    // an issue note above one command — carry no count and are skipped.
    const head = line.match(/^\/\/ ([a-z][a-z0-9_]*) \((\d+)/);
    if (head) {
      families.push({ name: head[1], claimed: Number(head[2]), commands: [] });
      continue;
    }
    if (line.startsWith('//')) continue;
    for (const m of line.matchAll(/"([A-Za-z0-9_]+)"/g)) {
      if (!families.length) throw new Error(`${MANIFEST}: "${m[1]}" precedes any family comment`);
      families[families.length - 1].commands.push(m[1]);
    }
  }
  return families;
}

/** The doc's §5.4 table: per family, the N in its header and its rows'
 *  `n` and `class` cells. */
function parseDoc(src) {
  const lines = src.split(/\r?\n/);
  const at = lines.findIndex((l) => /^### 5\.4 /.test(l));
  if (at < 0) throw new Error(`${DOC}: no §5.4 heading`);
  const families = [];
  let seenTable = false;
  for (let i = at + 1; i < lines.length; i++) {
    const line = lines[i];
    if (!line.startsWith('|')) {
      if (seenTable) break; // the first non-row line ends the table
      continue;
    }
    seenTable = true;
    const cells = line.split('|').slice(1, -1).map((c) => c.trim());
    if (cells[0] === 'family' || /^-+$/.test(cells[0])) continue;
    const head = cells[0].match(/^\*\*([a-z][a-z0-9_]*)\*\* \((\d+)\)$/);
    if (head) families.push({ name: head[1], claimed: Number(head[2]), rows: [] });
    else if (cells[0] !== '' && !/^`/.test(cells[0])) throw new Error(`${DOC}:${i + 1}: unreadable family cell ${JSON.stringify(cells[0])}`);
    if (!families.length) throw new Error(`${DOC}:${i + 1}: a row precedes any family`);
    if (cells[1] === '') continue; // a family header carrying no count of its own
    if (!/^\d+$/.test(cells[1])) throw new Error(`${DOC}:${i + 1}: n is not a number: ${JSON.stringify(cells[1])}`);
    families[families.length - 1].rows.push({ n: Number(cells[1]), cls: cells[2].replace(/\*/g, ''), line: i + 1 });
  }
  if (!families.length) throw new Error(`${DOC}: §5.4 has no table rows`);
  // The totals sentence under the table: "**<n> wire**, **<n> client-local** …".
  const tail = lines.slice(at).join('\n');
  const totals = {};
  // `\s+`, not a space: the sentence is hand-wrapped, so a count can end one
  // line and its class open the next.
  for (const m of tail.matchAll(/\*\*(\d+)\s+(wire|client-local|disabled|retargeted)\*\*/g)) {
    if (!(m[2] in totals)) totals[m[2]] = Number(m[1]);
  }
  const grand = tail.match(/= \*\*(\d+)\*\*, the total/);
  return { families, totals, grand: grand ? Number(grand[1]) : null };
}

function reconcile(root = ROOT) {
  const manifest = parseManifest(fs.readFileSync(path.join(root, MANIFEST), 'utf8'));
  const doc = parseDoc(fs.readFileSync(path.join(root, DOC), 'utf8'));
  const problems = [];
  const docBy = new Map(doc.families.map((f) => [f.name, f]));
  const manBy = new Map(manifest.map((f) => [f.name, f]));
  const dupes = manifest.flatMap((f) => f.commands).filter((c, i, a) => a.indexOf(c) !== i);
  if (dupes.length) problems.push(`manifest lists a command twice: ${[...new Set(dupes)].join(', ')}`);
  for (const f of manifest) {
    if (f.claimed !== f.commands.length) problems.push(`manifest: \`// ${f.name} (${f.claimed})\` heads ${f.commands.length} commands`);
    const d = docBy.get(f.name);
    if (!d) { problems.push(`doc: no row for family \`${f.name}\` (${f.commands.length} commands with no disposition)`); continue; }
    const sum = d.rows.reduce((a, r) => a + r.n, 0);
    if (d.claimed !== sum) problems.push(`doc: **${f.name}** (${d.claimed}) heads rows summing to ${sum}`);
    if (sum !== f.commands.length) problems.push(`doc: **${f.name}** places ${sum} commands, the manifest has ${f.commands.length}`);
  }
  for (const d of doc.families) if (!manBy.has(d.name)) problems.push(`doc: family \`${d.name}\` is not in the manifest`);
  const total = manifest.reduce((a, f) => a + f.commands.length, 0);
  const byClass = {};
  for (const d of doc.families) for (const r of d.rows) byClass[r.cls] = (byClass[r.cls] || 0) + r.n;
  for (const cls of ['wire', 'client-local', 'disabled', 'retargeted']) {
    if (doc.totals[cls] !== (byClass[cls] || 0)) problems.push(`doc: the totals sentence says ${doc.totals[cls]} ${cls}, the table's rows say ${byClass[cls] || 0}`);
  }
  const unknown = Object.keys(byClass).filter((c) => !['wire', 'client-local', 'disabled', 'retargeted'].includes(c));
  if (unknown.length) problems.push(`doc: unknown class in the table: ${unknown.join(', ')}`);
  if (doc.grand !== total) problems.push(`doc: the stated total is ${doc.grand}, the manifest has ${total}`);
  return { manifest, doc, total, byClass, problems };
}

module.exports = { parseManifest, parseDoc, reconcile, MANIFEST, DOC };

if (require.main === module) {
  const r = reconcile();
  for (const f of r.manifest) {
    const d = r.doc.families.find((x) => x.name === f.name);
    const sum = d ? d.rows.reduce((a, x) => a + x.n, 0) : '—';
    console.log(`${f.name.padEnd(14)} manifest ${String(f.commands.length).padStart(3)}   doc ${String(sum).padStart(3)}`);
  }
  console.log(`total          manifest ${r.total}   doc ${r.doc.grand}   by class ${JSON.stringify(r.byClass)}`);
  if (r.problems.length) {
    console.log(`\n${r.problems.length} difference(s):`);
    for (const p of r.problems) console.log('  - ' + p);
    process.exit(1);
  }
  console.log('\nthe table partitions the manifest.');
}
