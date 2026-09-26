// The one recursive source walk the source-scanning guards share (#3498). Before this,
// five test files (agenticons, icons, inputvectors, theme, cssvars) each carried their own
// copy of the same function. Five copies of a walk are five places for one of them to go
// flat again (#3508 P0 made all five recursive, one edit each) or to lose an exclusion.
//
// `vendor/` is excluded BY NAME, at any depth, the way `test/agentrows.test.ts`'s walk
// does it, and for its reason: that tree is third-party source this repo may not edit in
// place (`THIRD_PARTY_NOTICES.md`), so a guard hit there could never be fixed, only
// allowlisted. Callers that must prove the walk really descends do that with a planted
// subdirectory, as `test/sourcefiles.test.ts` does for this helper itself.
import { readdirSync } from "node:fs";

/** Every file under `root` whose name ends with one of `exts`, as `/`-separated paths
 *  relative to `root`. A suffix can be an extension (`".ts"`) or a whole file name
 *  (`"styles.css"`). Directories named `vendor` are skipped, and so is everything
 *  under them. */
export function sourceFiles(root: URL, exts: readonly string[]): string[] {
  const walk = (prefix: string): string[] =>
    readdirSync(new URL(prefix || ".", root), { withFileTypes: true }).flatMap((entry) => {
      const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (entry.isDirectory()) return entry.name === "vendor" ? [] : walk(relative);
      return exts.some((ext) => entry.name.endsWith(ext)) ? [relative] : [];
    });
  return walk("");
}
