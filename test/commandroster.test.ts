// The protocol note's command roster (docs/design/remote-engine-protocol.md
// §5.4) against the command manifest — both sides PARSED, by
// scripts/command-roster.cjs.
//
// The table partitions the manifest by family and gives each part a class and
// a tier. It was kept by hand, and by #3679 it had fallen 29 commands behind
// with one family carrying no row at all; nothing was red. This is the
// count-level check. It does not say a NAMED command is in the right row —
// several rows summarise rather than list — so it is not the per-command
// roster test that note assigns to its slice C.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const roster = require("../scripts/command-roster.cjs") as {
  parseManifest: (src: string) => { name: string; claimed: number; commands: string[] }[];
  parseDoc: (src: string) => {
    families: { name: string; claimed: number; rows: { n: number; cls: string }[] }[];
    totals: Record<string, number>;
    grand: number | null;
  };
  reconcile: () => { total: number; byClass: Record<string, number>; problems: string[] };
  MANIFEST: string;
  DOC: string;
};

const manifestSrc = readFileSync(new URL(`../${roster.MANIFEST}`, import.meta.url), "utf8");
const docSrc = readFileSync(new URL(`../${roster.DOC}`, import.meta.url), "utf8");

test("the protocol note's roster table partitions the command manifest", () => {
  const r = roster.reconcile();
  assert.deepEqual(r.problems, [], "run `node scripts/command-roster.cjs` for both sides");
});

test("the reconciliation read every command and every row it claims to have", () => {
  // The cross-check on the instrument: a parser that silently skipped a family
  // or a row would report "no differences" about a population it never saw.
  const manifest = roster.parseManifest(manifestSrc);
  const names = manifest.flatMap((f) => f.commands);
  // Counted a second way, off the raw text: every quoted identifier between
  // the array's brackets.
  const body = manifestSrc.slice(manifestSrc.indexOf("pub const APP_COMMANDS"), manifestSrc.indexOf("\n];"));
  const raw = [...body.matchAll(/^\s*"([A-Za-z0-9_]+)",\s*$/gm)].map((m) => m[1]);
  assert.ok(raw.length > 100, `the raw count found the array: ${raw.length}`);
  assert.deepEqual(names, raw, "every manifest entry belongs to a family the parser saw");
  assert.equal(new Set(names).size, names.length, "and none is listed twice");

  const doc = roster.parseDoc(docSrc);
  const rows = doc.families.flatMap((f) => f.rows);
  // Counted a second way: table lines under §5.4 whose second cell is a number.
  const section = docSrc.slice(docSrc.indexOf("### 5.4 "));
  const table = section.slice(0, section.indexOf("\nTotals,"));
  const rawRows = table.split(/\r?\n/).filter((l) => /^\|[^|]*\|\s*\d+\s*\|/.test(l));
  assert.ok(rawRows.length > 20, `the raw count found the table: ${rawRows.length}`);
  assert.equal(rows.length, rawRows.length, "every counted row of the table was read");
  assert.deepEqual(
    doc.families.map((f) => f.name),
    manifest.map((f) => f.name),
    "the table lists the manifest's families, in the manifest's order"
  );
});

test("a family that gains a command without a disposition is named", () => {
  // The counterfactual, performed: the same two parsers over a manifest with
  // one more command than the table places.
  const manifest = roster.parseManifest(manifestSrc.replace('    "take_startup_notice",', '    "take_startup_notice",\n    "a_command_nobody_classified",'));
  const obs = manifest.find((f) => f.name === "obs");
  assert.ok(obs, "the fixture's family exists");
  const doc = roster.parseDoc(docSrc);
  const placed = doc.families.find((f) => f.name === "obs")!.rows.reduce((a, r) => a + r.n, 0);
  assert.equal(obs.commands.length, placed + 1, "the planted command is one the table does not place");
  assert.equal(obs.claimed, placed, "and the manifest's own comment is now stale too");
});

test("a row whose count is not a number is refused, not read as zero", () => {
  const broken = docSrc.replace(/\| \*\*obs\*\* \((\d+)\) \| \d+ \|/, "| **obs** ($1) | two |");
  assert.notEqual(broken, docSrc, "the fixture edit landed");
  assert.throws(() => roster.parseDoc(broken), /n is not a number/);
});
