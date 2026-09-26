import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import * as path from "node:path";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
export const BUDGETS = [
  { path: "crates/loomux-engine/src/harness/pi.rs", ceiling: 3314, blob: "6530395af485faeea1a6e9fdf29f796d2e7b18ed" },
  { path: "crates/loomux-engine/src/lockwatch.rs", ceiling: 3595, blob: "110e802c989f6b4ccbc108367661881290c3466b" },
  { path: "crates/loomux-engine/src/obs.rs", ceiling: 3272, blob: "4ffc0c73306554ee49cf2d00704b000c701944c5" },
  { path: "crates/loomux-engine/src/queue.rs", ceiling: 4683, blob: "8b1ce3ffdad2234c4d528c4f04fc2db093a73cb6" },
  { path: "crates/loomux-engine/src/reviewdrive.rs", ceiling: 10935, blob: "f243bda1ea691d4cfd87ad2f97209aa3fe4ea064" },
  { path: "crates/loomux-engine/src/workflow.rs", ceiling: 7538, blob: "9b95e14b299a880c597dd92ed0eb88be1db986fb" },
  { path: "src-tauri/src/orchestration/mcp.rs", ceiling: 5815, blob: "1669f55d925c7336b5280f303f0acec90048abbe" },
  { path: "src-tauri/src/orchestration/mod.rs", ceiling: 70838, blob: "1a362268a0e1f933b6cb51c1328d677118f9d10a" },
  { path: "src-tauri/src/orchestration/rdtick.rs", ceiling: 6381, blob: "f3aa72c058b97d6816d726e0cfa3c69ba91fe6af" },
  { path: "src-tauri/tests/orchestration.rs", ceiling: 74937, blob: "1f9ddd7bbed0f5d121343e56e6d3c174fd6d310f" },
  { path: "src-tauri/tests/reviewdrive.rs", ceiling: 16901, blob: "ff2fbca628885f89d6b417fea4d60ec24caed747" },
  { path: "src-tauri/tests/workflow.rs", ceiling: 12150, blob: "72f2cb41fc681e88aecc83a51dd6f1d2aa8e7004" },
  { path: "src/fileedit.ts", ceiling: 1627, blob: "bc91319afb041316da5c8830b4f4f5a005eaec37" },
  { path: "src/fileexplorer.ts", ceiling: 1672, blob: "001857b329959245e4b918873c5d9920f32a6282" },
  { path: "src/gitview.ts", ceiling: 1608, blob: "31f7da297c8210c880d8389b8442bb272040ed94" },
  { path: "src/groupview.ts", ceiling: 2187, blob: "d702f2bdb5178d59efe6e056028b29e8d2916b30" },
  { path: "src/launcher.ts", ceiling: 2955, blob: "27d3b4777674483d12e76035bcb6538feb7c4e9b" },
  { path: "src/main.ts", ceiling: 4152, blob: "3972d22c2bd528cd4845a08d780cc50d3e90d871" },
  { path: "src/orchestration.ts", ceiling: 3152, blob: "69b46587dccbf973b88c6a3766d8ec207aca8cfb" },
  { path: "src/pane.ts", ceiling: 7103, blob: "360c14a506a09250a342bb35d2177066122c6a56" },
  { path: "src/panerestore.ts", ceiling: 2053, blob: "655717a0d8c8c4f670a4bb5589bc6b66c56a8980" },
  { path: "src/taskboard.ts", ceiling: 2348, blob: "177f4f9f0d5d2708de76fdf593c57f51c377ae7c" },
  { path: "src/tasksview.ts", ceiling: 3666, blob: "85ae2f4667505fcadbce3afa8725207c1d422249" },
  { path: "src/todopane.ts", ceiling: 2308, blob: "d84fe8a5c0a455e586f9fac1a9f376480a7f1bd9" },
  { path: "src/tokencharts.ts", ceiling: 1612, blob: "346626778461aa1bdec84e038f322298727c0031" },
  { path: "src/tokenchartsview.ts", ceiling: 1812, blob: "020c71f09dba0d2301b96465197ea0204d8fd428" },
  { path: "src/workflowmodel.ts", ceiling: 4926, blob: "4befd2c6d40e14f5f46770a650fdb7d271f6e862" },
  { path: "src/workflowview.ts", ceiling: 4175, blob: "1ecf5d6d635de7ac9c442d5bfd1c3e73829dcc97" },
  { path: "test/panerestore.test.ts", ceiling: 2649, blob: "e157ccf9220b7ba891c41444f5550301bb460a6c" },
  { path: "test/taskboard.test.ts", ceiling: 3125, blob: "145f20733132a4a387d08aa0fee91af79c32e090" },
  { path: "test/theme.test.ts", ceiling: 2285, blob: "618348b4b0bfaa2427ce927c13831edd2177dd2e" },
  { path: "test/workflowmodel.test.ts", ceiling: 3722, blob: "7a25104042de297403145ea56523fbbc4ec5c7a9" },
];

export function lineCount(text: string): number { return (text.match(/\n/g) ?? []).length; }
function category(file: string): { ceiling: number; label: string } | undefined {
  if (/^(src-tauri\/src|crates\/[^/]+\/src)\/.*\.rs$/.test(file)) return { ceiling: 3000, label: "Rust src" };
  if (/^src-tauri\/tests\/.*\.rs$/.test(file)) return { ceiling: 5000, label: "Rust test" };
  if (/^src\/.*\.ts$/.test(file)) return { ceiling: 1500, label: "TS src" };
  if (/^test\/.*\.test\.ts$/.test(file)) return { ceiling: 2000, label: "TS test" };
  return undefined;
}
function scan(files: string[], read: (file: string) => string, requireRows = true, rows = BUDGETS): string[] {
  const allowed = new Map(rows.map((row) => [row.path, row]));
  const seen = new Set<string>();
  const errors: string[] = [];
  for (const file of files) {
    const kind = category(file);
    if (!kind) continue;
    const row = allowed.get(file);
    const lines = lineCount(read(file));
    if (row) {
      seen.add(file);
      if (lines > row.ceiling) errors.push(`${file}: ${lines} > ${row.ceiling}`);
      if (lines <= row.ceiling * 0.85) errors.push(`${file}: row stale — tighten it (baseline blob ${row.blob})`);
    } else if (lines > kind.ceiling) errors.push(`${file}: ${lines} > ${kind.ceiling} (${kind.label})`);
  }
  if (requireRows) for (const row of rows) if (!seen.has(row.path)) errors.push(`${row.path}: allowlist row did not match`);
  return errors;
}

test("tracked source files stay within class ceilings and grandfathered rows ratchet", () => {
  const files = execFileSync("git", ["-C", ROOT, "ls-files", "-z"], { encoding: "utf8" }).split("\0").filter(Boolean);
  const errors = scan(files, (file) => readFileSync(path.join(ROOT, file), "utf8"));
  assert.deepEqual(errors, [], errors.join("\n"));
  // Positive control: the actual scanner must identify an over-budget tracked Rust source.
  assert.match(scan(["crates/control/src/control.rs"], () => "x\n".repeat(3001), false).join("\n"), /crates\/control\/src\/control\.rs: 3001 > 3000/);
  const stale = [{ path: "src/stale.ts", ceiling: 100, blob: "fixture" }];
  assert.match(scan(["src/stale.ts"], () => "x\n".repeat(85), true, stale).join("\n"), /row stale — tighten it \(baseline blob fixture\)/);
});
