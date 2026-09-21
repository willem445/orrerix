// The repo's TOP LEVEL, made executable (#3315).
//
// The repo had grown two documentation roots (`doc/` and `docs/`) and a
// checked-in `demo/` tree whose mocks had long since been superseded by the
// shipped renderers, and nothing in the suite noticed either. Both are the same
// failure: a root entry is added by whoever needs it, is never revisited, and
// costs every later reader a wrong guess about where something lives. This
// test makes the top level a reviewed surface — a new root entry fails here
// until somebody writes down why it cannot live anywhere else, and that
// sentence is a review-visible diff.
//
// DEFAULT-DENY, AND SHAPE-BASED, per CLAUDE.md's source-scanning-guard
// convention. The subject is `git ls-files` — the TRACKED tree, which is what
// a reader clones — not a directory walk, so an ignored `target/` or
// `node_modules/` is invisible to it by construction rather than by an
// exclusion list that would go stale. Nothing here decides from a binding's
// name: the allowlist is keyed on the literal root entry, each row carrying
// its own reason, and every row is REQUIRED to match at least one tracked
// path, so a root entry that is deleted or renamed fails loudly rather than
// leaving a row watching nothing.
//
// WHAT THIS CANNOT SAY. It bounds the FIRST path segment and nothing below it:
// a second documentation root spelled `docs/design/notes/` is invisible here,
// as is a demo tree re-added as `docs/demo/`. The banned-prefix rows below are
// the narrow, named exception — the two spellings this repo actually grew —
// and they are deliberately not a general "no directory may look like another"
// rule, which no textual guard can express. It also says nothing about
// CONTENT: a file correctly placed under an allowed root but citing a path
// that no longer exists is a dangling citation this guard cannot see.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import * as path from "node:path";

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

/** Every tracked path, repo-relative, POSIX-separated. */
function trackedFiles(): string[] {
  const out = execFileSync("git", ["-C", REPO_ROOT, "ls-files", "-z"], {
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  return out.split("\0").filter((p) => p.length > 0);
}

/**
 * Prefixes no tracked path may carry, with the reason each was retired.
 *
 * These are PATH prefixes rather than root entries because the point is the
 * tree, not the directory node: git tracks no empty directory, so "the folder
 * is gone" and "no file under it is tracked" are the same statement.
 */
const BANNED_PREFIXES: ReadonlyArray<{ prefix: string; reason: string }> = [
  {
    prefix: "demo/",
    reason:
      "the S0 mocks for #2891 (PR #2945) and #3263 (PR #3271); both renderers shipped, so the " +
      "mocks are history and live in those PRs. A design that needs a throwaway page builds it " +
      "on a branch and cites the PR — checking one in leaves every later reader to work out " +
      "for themselves that it is dead.",
  },
  {
    prefix: "doc/",
    reason:
      "the second documentation root, folded into `docs/` by #3315. Design notes are " +
      "`docs/design/`, plans are `docs/plans/`; both are excluded from the Jekyll build in " +
      "`docs/_config.yml`, so one root serves the published site and the internal notes " +
      "without publishing the notes.",
  },
];

/**
 * Every permitted top-level entry, with the argument for it being at the root.
 *
 * A row is required to MATCH — see the staleness test below — so this is a
 * census of the root, not a wish list. Add a row only with a reason that
 * survives being read by somebody who did not write the entry.
 */
const ROOT_ALLOWLIST: ReadonlyArray<{ entry: string; reason: string }> = [
  // Tooling and agent configuration: each is read from the root by a tool that
  // takes no path argument, so none of them can be relocated.
  { entry: ".claude", reason: "Claude Code reads skills, agents and settings from `<repo>/.claude` — the path is the tool's, not ours." },
  { entry: ".github", reason: "GitHub reads workflows, issue templates and agent definitions from `<repo>/.github`." },
  { entry: ".orrerix", reason: "orrerix reads a repo's workflow, personas and lessons from `<repo>/.orrerix` — see `docs/design/lessons.md`." },
  { entry: ".gitattributes", reason: "git reads it from the root; it carries the `eol=lf` rules CLAUDE.md's worktree section depends on." },
  { entry: ".gitignore", reason: "git reads a repository's ignore rules from `<repo>/.gitignore`; a nested one only narrows." },

  // Source roots — one per language surface, mapped in `docs/design/architecture.md`.
  { entry: "crates", reason: "the Tauri-free workspace members (`loomux-engine`, `loomux-server`) — see `docs/design/engine-extraction.md`." },
  { entry: "src", reason: "the frontend (vanilla TypeScript); `index.html` below is its entry document." },
  { entry: "src-tauri", reason: "the desktop app crate — the one workspace member that links Tauri." },
  { entry: "test", reason: "the frontend unit suite; `package.json`'s test script globs `test/**/*.test.ts`, a sibling of `src/`." },
  { entry: "e2e", reason: "the Playwright WebView2 specs; `playwright.config.ts` points at it and that job is separate from the unit suite." },
  { entry: "scripts", reason: "repo tooling run by CI and by agents (`code-metrics.cjs`, `pr-body-check.cjs`); shipped in no artifact." },
  { entry: "dev", reason: "browser entry pages (HTML + its module) served by the root vite dev server for hand-validating DOM wiring. Not `scripts/`, which is headless repo tooling run by CI and by agents; these are pages a human opens, and a vite entry has to sit under the vite root, which `vite.config.ts` leaves at the repo root." },
  { entry: "npm", reason: "the published `orrerix` launcher package, with its own `package.json`. Deliberately outside the root manifest's scope: CLAUDE.md's `\"type\"`-resolution note turns on `npm/` resolving to CJS." },

  // Documentation. ONE root, by #3315.
  { entry: "docs", reason: "the one documentation root: the published Jekyll site, plus `design/` and `plans/`, which `_config.yml` excludes from the build. There is no second root — see BANNED_PREFIXES." },

  // Root files that a tool, or a published URL, pins to the root.
  { entry: "index.html", reason: "vite's entry document; vite resolves it from its root, which `vite.config.ts` leaves at the repo root." },
  { entry: "package.json", reason: "npm reads it from the root, and it sets `\"type\": \"module\"` for everything in its scope." },
  { entry: "package-lock.json", reason: "npm's lockfile, which must sit beside its manifest." },
  { entry: "tsconfig.json", reason: "`tsc --noEmit` resolves its config from the invocation root." },
  { entry: "vite.config.ts", reason: "vite resolves its config from its root, which is the repo root." },
  { entry: "playwright.config.ts", reason: "playwright resolves its config from the invocation root." },
  { entry: "Cargo.toml", reason: "the cargo WORKSPACE manifest; cargo requires it at the root of the workspace it defines." },
  { entry: "Cargo.lock", reason: "cargo writes one lockfile for the workspace, beside the workspace manifest." },
  { entry: "README.md", reason: "GitHub renders the root README as the repository's front page." },
  { entry: "LICENSE", reason: "GitHub's licence detection, and most downstream packagers, look for `LICENSE` at the repository root." },
  { entry: "THIRD_PARTY_NOTICES.md", reason: "the vendored-code attribution the licence terms require, conventionally at the root beside `LICENSE`." },
  { entry: "CLAUDE.md", reason: "Claude Code reads `<repo>/CLAUDE.md`; the path is the tool's, not ours." },
  { entry: "install.ps1", reason: "the one-line installer the README pipes from `raw.githubusercontent.com/.../main/install.ps1` — a published URL, so moving it breaks every copy of that line already in the wild." },
  { entry: "install.sh", reason: "the same published-URL contract as `install.ps1`, for macOS and Linux." },
];

test("no tracked path lives under a retired root", () => {
  const files = trackedFiles();
  // Positive control: an absence assertion over an empty list passes for the
  // wrong reason, so pin that the enumeration happened at all.
  assert.ok(files.length > 100, `positive control: git ls-files returned only ${files.length} paths`);

  for (const { prefix, reason } of BANNED_PREFIXES) {
    const hits = files.filter((f) => f.startsWith(prefix));
    const shown = hits.slice(0, 12).join("\n  ");
    assert.deepEqual(
      hits,
      [],
      `${hits.length} tracked path(s) under the retired \`${prefix}\` root — ${reason}\n  ${shown}` +
        (hits.length > 12 ? `\n  …and ${hits.length - 12} more` : ""),
    );
  }
});

test("every top-level entry is on the argued allowlist", () => {
  const files = trackedFiles();
  assert.ok(files.length > 100, `positive control: git ls-files returned only ${files.length} paths`);

  const allowed = new Set(ROOT_ALLOWLIST.map((r) => r.entry));
  const present = new Set(files.map((f) => f.split("/")[0]));

  const unargued = [...present].filter((e) => !allowed.has(e)).sort();
  assert.deepEqual(
    unargued,
    [],
    `top-level ${unargued.length === 1 ? "entry" : "entries"} with no row in ROOT_ALLOWLIST: ` +
      `${unargued.join(", ")}.\nThe root is a reviewed surface (#3315). Either put the entry under ` +
      "an existing root, or add a row here whose reason says why it cannot live anywhere else — a " +
      "tool that reads it from the root, or a published URL that pins it there.",
  );
});

test("no allowlist row watches nothing", () => {
  // Without this, a row survives the entry it was written for: the guard then
  // reads as covering something that is gone, and the next reader trusts it.
  const present = new Set(trackedFiles().map((f) => f.split("/")[0]));
  const stale = ROOT_ALLOWLIST.map((r) => r.entry).filter((e) => !present.has(e));
  assert.deepEqual(
    stale,
    [],
    `ROOT_ALLOWLIST ${stale.length === 1 ? "row" : "rows"} matching no tracked path: ${stale.join(", ")}. ` +
      "Delete the row — an allowlist is a census of the root, not a wish list.",
  );
});

test("every allowlist and banned-prefix row carries a reason", () => {
  for (const { entry, reason } of ROOT_ALLOWLIST) {
    assert.ok(reason.trim().length >= 40, `\`${entry}\` needs a real reason, not "${reason}"`);
  }
  for (const { prefix, reason } of BANNED_PREFIXES) {
    assert.ok(reason.trim().length >= 40, `\`${prefix}\` needs a real reason, not "${reason}"`);
  }
  const entries = ROOT_ALLOWLIST.map((r) => r.entry);
  assert.equal(new Set(entries).size, entries.length, "duplicate row in ROOT_ALLOWLIST");
});
