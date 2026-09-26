// The shared source walk (test/support/sourcefiles.ts, #3498) is what five guards stand on,
// so its two promises are pinned on a planted tree rather than on `src/` as it happens to be
// today: `src/` has no subdirectory but `vendor/`, so a walk that never descended would pass
// every guard built on it.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { sourceFiles } from "./support/sourcefiles.ts";

function plantedTree(): { root: URL; cleanup: () => void } {
  const dir = mkdtempSync(path.join(tmpdir(), "loomux-sourcefiles-"));
  const put = (rel: string) => {
    mkdirSync(path.dirname(path.join(dir, rel)), { recursive: true });
    writeFileSync(path.join(dir, rel), "// planted\n");
  };
  put("top.ts");
  put("styles.css");
  put("notes.md");
  put("sub/nested.ts");
  put("sub/deeper/deepest.ts");
  put("vendor/thirdparty.ts");
  put("sub/vendor/alsothirdparty.ts");
  return { root: pathToFileURL(dir + path.sep), cleanup: () => rmSync(dir, { recursive: true, force: true }) };
}

test("the shared walk descends: a file planted in a subdirectory is seen", () => {
  const { root, cleanup } = plantedTree();
  try {
    const files = sourceFiles(root, [".ts"]).sort();
    // POSITIVE CONTROL first: the top-level file proves the walk ran at all, so the nested
    // ones below are evidence about DESCENDING, not about running.
    assert.ok(files.includes("top.ts"), `the walk did not even see the top level: ${files}`);
    assert.ok(files.includes("sub/nested.ts"), `one level down is invisible: ${files}`);
    assert.ok(files.includes("sub/deeper/deepest.ts"), `two levels down is invisible: ${files}`);
  } finally {
    cleanup();
  }
});

test("the shared walk skips every vendor/ directory, at any depth, and filters by suffix", () => {
  const { root, cleanup } = plantedTree();
  try {
    assert.deepEqual(sourceFiles(root, [".ts"]).sort(), ["sub/deeper/deepest.ts", "sub/nested.ts", "top.ts"]);
    // A suffix may be a whole file name, which is how theme.test.ts asks for the stylesheet.
    assert.deepEqual(sourceFiles(root, [".ts", "styles.css"]).sort(), [
      "styles.css",
      "sub/deeper/deepest.ts",
      "sub/nested.ts",
      "top.ts",
    ]);
  } finally {
    cleanup();
  }
});
