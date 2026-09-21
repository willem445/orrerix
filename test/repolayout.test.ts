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
import { readFileSync } from "node:fs";
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

// ---------------------------------------------------------------------------
// Self-referencing GitHub URLs (#3315 review round 1, B1)
// ---------------------------------------------------------------------------
//
// This repo deliberately links some of its own files by ABSOLUTE GitHub URL
// rather than relatively, and #3315 made that a rule rather than an accident:
// a user page under `docs/` is served by Jekyll, which excludes `docs/design/`
// from the build, so a site-relative link from a published page to a design
// note is a 404 for every reader on the site. A blob URL resolves from both
// surfaces, so that is what those cross-links use.
//
// The cost of that rule is a class of breakage nothing else here can see. A
// relative link is checked by anyone who opens the file; an absolute one is a
// string, and no compiler, test or Jekyll build resolves it. #3315's own sweep
// proved the point: it rewrote the LABEL of `docs/index.md`'s design-notes link
// and left the HREF pointing at `tree/main/doc/design`, a tree that PR deleted.
// The site's front page would have shipped a 404, with every check green — the
// pattern was built from what could FOLLOW the token (a trailing `/`) instead
// of from what a path token may CONTAIN, which is the #1297 blind spot.
//
// So: every self-referencing URL is resolved against `git ls-files`. A `blob`
// or `raw` URL must name a tracked FILE; a `tree` URL must name a directory
// some tracked path lives under.
//
// WHAT THIS CANNOT SAY. It resolves against the tree in hand, not against
// `main` — a URL correct here is still dangling until this branch merges, which
// is the intended semantics for a link to a file this very PR adds. It reads
// only URLs on the `main` ref: a URL pinned to a tag or a SHA names history
// deliberately and is skipped, which also means a typo in one is invisible
// here. And it says nothing about a link to another repo, or about an anchor
// within a page — only that the file or directory exists.

/** `main`-ref URLs into this repo, as {url, path, kind} — anchors stripped. */
function selfLinks(files: string[]): Array<{ file: string; url: string; target: string; kind: "file" | "dir" }> {
  const out: Array<{ file: string; url: string; target: string; kind: "file" | "dir" }> = [];
  const patterns: Array<{ re: RegExp; kind: "file" | "dir" }> = [
    { re: /https:\/\/github\.com\/willem445\/orrerix\/blob\/main\/([^)\s"'`>\]]+)/g, kind: "file" },
    { re: /https:\/\/raw\.githubusercontent\.com\/willem445\/orrerix\/main\/([^)\s"'`>\]]+)/g, kind: "file" },
    { re: /https:\/\/github\.com\/willem445\/orrerix\/tree\/main\/([^)\s"'`>\]]+)/g, kind: "dir" },
  ];
  for (const f of files) {
    if (!/\.(md|ts|js|cjs|mjs|rs|yml|yaml|html|css|json|sh|ps1)$/.test(f)) continue;
    if (f.startsWith(".claude/skills/impeccable/")) continue; // vendored — re-vendor, never edit
    let text: string;
    try {
      text = readFileSync(path.join(REPO_ROOT, f), "utf8");
    } catch {
      continue;
    }
    for (const { re, kind } of patterns) {
      re.lastIndex = 0;
      let m: RegExpExecArray | null;
      while ((m = re.exec(text)) !== null) {
        // Strip an anchor and any trailing sentence punctuation the URL swept up.
        const target = m[1].split("#")[0].replace(/[.,;:]+$/, "").replace(/\/$/, "");
        if (target) out.push({ file: f, url: m[0], target, kind });
      }
    }
  }
  return out;
}

test("every self-referencing GitHub URL resolves to something in the tree", () => {
  const files = trackedFiles();
  const links = selfLinks(files);

  // Positive control. The success shape here is an empty list of dangling
  // links, which is byte-identical to the matcher never having matched
  // anything — a regex that stops one character early reads as a clean repo.
  assert.ok(
    links.length >= 10,
    `positive control: only ${links.length} self-referencing URLs found. This repo carries a couple of ` +
      "dozen; a number this low means the matcher is broken, not that the links are gone.",
  );

  const tracked = new Set(files);
  const dangling = links
    .filter(({ target, kind }) =>
      kind === "file" ? !tracked.has(target) : !files.some((f) => f.startsWith(target + "/")),
    )
    .map(({ file, url, target }) => `${file}: ${url}  ->  ${target} is not in the tree`)
    .sort();

  assert.deepEqual(
    dangling,
    [],
    "a URL this repo publishes points at a path that does not exist:\n  " +
      dangling.join("\n  ") +
      "\n\nNothing but this test resolves an absolute link, so a rename or a move breaks one silently " +
      "(#3315 B1). Update the href, or make the link relative if the page is not published.",
  );
});

// ---------------------------------------------------------------------------
// Citations of a retired root (#3315 review round 1, rev-std finding 2)
// ---------------------------------------------------------------------------
//
// The move itself is guarded above: no tracked path may LIVE under `doc/` or
// `demo/`. This guards the other half — no tracked file may CITE one — and it
// exists because the round-1 receipt for that half was a number in a PR body.
//
// A number in a body is not a guard. #3315's own sweep reported zero, and by
// the time it was read it was two, because the PR's later commits added
// self-referential mentions (a re-bless log entry, a comment explaining an
// unswept fixture). Both reviewers reached the same premortem independently:
// the receipt IS the procedure, so the next agent re-runs it, gets a non-zero,
// and the cheapest way to make it zero again is to rewrite this PR's own
// record. Making the sweep a test replaces that with an argued allowlist: a
// legitimate mention is a row with a reason, and a dangling one is red.
//
// DEFAULT-DENY, and the pattern is CONTAIN-class, not follow-class. It matches
// `doc/` or `demo/` at a token boundary — built from what a path token may
// contain, never from what may follow it. The round-1 sweep required a
// trailing `/design/`, and a URL ending `…/doc/design)` slipped a 404 onto the
// published site's front page with every check green (#1297's blind spot, the
// PR's own B1).
//
// WHAT THIS CANNOT SAY. The allowlist unit is the FILE, not the line: a new
// dangling citation added to an already-allowlisted file is invisible here.
// That is a deliberate trade — the eleven rows are nearly all prose that
// discusses the retirement, where a line-level pin would redden on every
// rewording — and it is bounded by the rows being few and individually argued.
// It also does not resolve URLs; the self-link guard above does that, and the
// lookbehind deliberately excludes a `/` so `tree/main/doc/design` belongs to
// that guard rather than to this one.

/** `doc/` or `demo/` used as a path prefix, at a token boundary. */
const RETIRED_CITATION = /(?<![A-Za-z0-9_.\-\/])(doc|demo)\//g;

/**
 * Files permitted to name a retired root, with the reason each may.
 *
 * Required to match, like `ROOT_ALLOWLIST`: a row whose file no longer cites
 * anything fails, so this stays a census rather than accumulating permissions.
 */
const CITATION_ALLOWLIST: ReadonlyArray<{ file: string; reason: string }> = [
  { file: "CLAUDE.md", reason: "states the rule — \"There is no `doc/`\" — and names `demo/feedback` as a label. Naming a retired root while saying it is retired is the point." },
  { file: "docs/_config.yml", reason: "the `exclude:` comment explains the fold it implements: \"one documentation root instead of two (`doc/` and `docs/`)\"." },
  { file: "test/repolayout.test.ts", reason: "this file: BANNED_PREFIXES names both roots, and these rows quote them. A guard cannot forbid the string it is written in." },
  { file: "src-tauri/tests/fixtures/pre222/README.md", reason: "the re-bless log — dated history of what each blessing changed. Rewriting an entry to today's spelling would falsify the record it exists to keep." },
  { file: "docs/design/rebrand-external.md", reason: "a by-root census explicitly labelled \"at analysis time\"; the figures are a measurement of a past tree, not a pointer into this one." },
  { file: "test/prbodycheck.test.ts", reason: "the assertion over `test/fixtures/prbodycheck/diff.txt`, a CAPTURED real diff. It must keep the captured spelling, and the assertion says so inline." },
  { file: "test/filematch.test.ts", reason: "synthetic path literals in a ranking fixture (`doc/restore.md`). They are test data standing for any path, not references to this repo's tree." },
  { file: "test/fixtures/tokenscorecard/audit.jsonl", reason: "cut verbatim from the real group log (see that directory's README). A capture's value is being faithful; rewriting a path inside one falsifies it." },
  { file: "src-tauri/tests/orchestration.rs", reason: "a synthetic Windows path literal (`C:\\Projects\\demo/`) in a path-fixture test — a test path, not a reference. #3315 exempts it explicitly." },
  { file: "src/todopane.ts", reason: "names `demo/todo-pane` as the tree PR #3271 carried and #3315 removed — a citation of history, phrased as history." },
  { file: ".claude/skills/agent-cli-reference/SKILL.md", reason: "the English phrase \"a demo/live check\", not a path. Matched because the guard reads shape, not meaning." },
];

test("no tracked file cites a retired root outside the argued allowlist", () => {
  const files = trackedFiles();
  const allowed = new Set(CITATION_ALLOWLIST.map((r) => r.file));
  const citing = new Map<string, number>();

  let scanned = 0;
  for (const f of files) {
    if (f.startsWith(".claude/skills/impeccable/")) continue; // vendored — re-vendor, never edit
    let buf: Buffer;
    try {
      buf = readFileSync(path.join(REPO_ROOT, f));
    } catch {
      continue;
    }
    if (buf.includes(0)) continue; // binary
    scanned++;
    RETIRED_CITATION.lastIndex = 0;
    const n = (buf.toString("utf8").match(RETIRED_CITATION) || []).length;
    if (n > 0) citing.set(f, n);
  }

  // Positive control. Every assertion below succeeds on an empty scan, which is
  // what a broken reader or a pattern that matches nothing produces.
  assert.ok(scanned > 500, `positive control: only ${scanned} text files scanned`);
  assert.ok(citing.size > 0, "positive control: the pattern matched nothing at all — it cannot even see this file, which quotes both roots");

  const unargued = [...citing.keys()].filter((f) => !allowed.has(f)).sort();
  assert.deepEqual(
    unargued,
    [],
    `${unargued.length} file(s) cite a retired root with no row in CITATION_ALLOWLIST:\n  ${unargued.join("\n  ")}\n\n` +
      "`doc/` and `demo/` were retired by #3315. Point the citation at where the thing lives now " +
      "(`docs/design/`, `docs/plans/`, or PR #2945 / #3271 for the mocks), or add a row here saying why " +
      "this file legitimately names a root that does not exist.",
  );

  const stale = CITATION_ALLOWLIST.map((r) => r.file).filter((f) => !citing.has(f)).sort();
  assert.deepEqual(
    stale,
    [],
    `CITATION_ALLOWLIST row(s) matching nothing: ${stale.join(", ")}. Delete the row — a permission that ` +
      "guards nothing reads as coverage to the next person.",
  );

  for (const { file, reason } of CITATION_ALLOWLIST) {
    assert.ok(reason.trim().length >= 40, `\`${file}\` needs a real reason, not "${reason}"`);
  }
});
