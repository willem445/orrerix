// `test/prbodycheckci.test.ts` — the CI job in
// `.github/workflows/prbodycheck.yml` is a workflow file: it cannot import
// `analyze()` and it does not run on this suite's platform, so its CONTRACT is
// pinned here as text over the job itself, the way `test/workspacelayout.test.ts`
// pins ci.yml's cargo invocations (#888) and `test/personas-prbodycheck.test.ts`
// pins the personas that name this same script (#2168 S2/S3).
//
// What the pin covers, and what it deliberately does not: the workflow's shape —
// that it runs on the PR events a body edit needs (an `edited` PR with no push
// must still re-run it), that it refuses on a nonzero MISMATCH count and prints
// the rows, that `[scratch]`-titled PRs are report-only, and that the corpus
// dry-run over the last 10 merged PRs is part of the SAME job, so a future
// editor cannot drop the proof the check was validated against before arming it
// (#3367 item 3, the "guard that refuses ships only after known-good subjects"
// convention). It cannot and does not pin the shell's runtime behaviour — that
// is the CI run's own evidence, and the actionlint workflow
// (`.github/workflows/actionlint.yml`, path-scoped to `.github/workflows/**`)
// is what catches syntax drift.
//
// Residual, stated: this is a TEXT pin. It proves the workflow file says the
// right things; it cannot prove Actions executes them. The two are held apart
// deliberately — the first is cheap and runs on every `npm test`, the second is
// the PR's own CI run.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..');
const workflowPath = path.join(root, '.github', 'workflows', 'prbodycheck.yml');
const scriptPath = path.join(root, 'scripts', 'pr-body-check.cjs');

test('the pr-body-check workflow file exists, and so does the script it runs', () => {
  assert.ok(fs.existsSync(workflowPath), '.github/workflows/prbodycheck.yml is missing');
  assert.ok(fs.existsSync(scriptPath), 'scripts/pr-body-check.cjs is missing — the workflow would run nothing');
});

// Read lazily, inside the first test: the existence test above is the one that
// must go red on a base tree without the workflow, with ITS OWN failure line —
// not a module-level ENOENT that masks every assertion in the file as a crash.
const readWf = () => fs.readFileSync(workflowPath, 'utf8');
let wf = '';

test('the job runs on the PR events a body edit needs', () => {
  wf = readWf();
  // The `edited` trigger is the point of the whole slice (#3367 item 3): a
  // body-only fix must re-run this check with no push to the branch. Pin the
  // full list so neither `edited` nor `synchronize` can be dropped alone.
  assert.match(wf, /pull_request:/, 'the workflow must trigger on pull_request');
  assert.match(
    wf,
    /types:\s*\[opened,\s*synchronize,\s*reopened,\s*edited\]/,
    'the pull_request types must include edited — a body edit with no push is exactly the case this check exists to re-run',
  );
});

test('the job refuses on any MISMATCH and prints the rows', () => {
  // The gate is the step's own read of the script's summary line, because the
  // script itself exits 0 always (a report, never a gate — its header and
  // test/prbodycheck.test.ts both pin that). A gate that greps the summary for
  // `0 MISMATCH` is the narrowest honest one.
  assert.match(wf, /--pr "\$pr"/, 'the job must run the script against the PR');
  assert.match(wf, /-ne 0/, 'the job must compare the MISMATCH count against zero');
  assert.match(wf, /exit 1/, 'a nonzero MISMATCH count must fail the step');
  assert.match(wf, /printf '%s\\n' "\$out"/, 'the job must print the script output, so every CHECK row is visible to the reader');
});

test('a missing or unparsable summary line fails the step, not passes it', () => {
  // The script is designed to exit 0 even when it crashes (a checker crash
  // must never read as a body defect), so a gate keyed on the exit code would
  // pass on a broken checker. The gate is keyed on the summary line instead —
  // pin both arms of that, or the next editor reverts to the exit-code gate
  // and a crash reads as a clean body.
  assert.match(wf, /no SUMMARY line/, 'a run with no summary line must be a tool failure, not a pass');
  assert.match(wf, /could not parse the MISMATCH count/, 'an unparsable summary must be a tool failure, not a pass');
});

test('[scratch]-titled PRs are report-only', () => {
  // Same convention as ci.yml's `plan` job (#1685): a [scratch] PR is a
  // red-before-green counterfactual whose body is deliberately stale, so a
  // MISMATCH there is the checker working, not a defect. The `if:` must gate
  // the JOB (report-only), not weaken the parse.
  assert.match(
    wf,
    /startsWith\(github\.event\.pull_request\.title, '\[scratch'\)/,
    "the scratch exemption must read the PR title's [scratch prefix, as ci.yml's plan job does",
  );
});

test('the corpus dry run over the last 10 merged PRs is part of the SAME job', () => {
  // #3367 item 3's precondition: the check must not refuse before it has run
  // clean over the known-good corpus. Making it a step of the same job is what
  // keeps the proof from being silently dropped by a later edit — a separate
  // workflow can be disabled without anyone noticing, a step of the refusing
  // job cannot.
  assert.match(wf, /--state merged --limit 10/, 'the corpus is the last 10 MERGED PRs');
  assert.match(wf, /corpus PR #\$/, 'the corpus step must name the PR that false-blocked');
  assert.match(wf, /the check must not ship as required/, 'the corpus step must carry the escalation sentence, so a red corpus is legible without this test');
});

test('the workflow pins the checker call to this repo, so the corpus is this repo’s', () => {
  // Without `--repo`, `gh` resolves from the checkout's remote — right for
  // same-repo PRs, wrong the day this workflow runs on a fork or the repo is
  // renamed. Pin the explicit form the step uses.
  assert.match(wf, /--repo "\$GITHUB_REPOSITORY"/, 'the script must be told the repo explicitly');
});
