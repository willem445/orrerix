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
// the rows, that `[scratch]`-titled PRs get a REPORT-ONLY arm (steps run and
// print, `continue-on-error` keeps their MISMATCH from failing the run — an
// arm that exists, not a job-level skip that runs nothing), and that the
// corpus dry-run over the last 10 merged PRs is part of the SAME job, so a
// future editor cannot drop the proof the check was validated against before
// arming it (#3367 item 3, the "guard that refuses ships only after
// known-good subjects" convention). It cannot and does not pin the shell's
// runtime behaviour — that is the CI run's own evidence, and the actionlint
// workflow (`.github/workflows/actionlint.yml`, path-scoped to
// `.github/workflows/**`) is what catches syntax drift.
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
  // The gate is the script's own `--gate` exit — the one nonzero exit the
  // script has, on any MISMATCH from a completed run (documented in its
  // header and USAGE). The script's default exit-0 contract is pinned by
  // test/prbodycheck.test.ts; this pin is the CI half of the pair.
  assert.match(wf, /--gate/, 'the job must invoke the script with --gate, the one exit-nonzero mode');
  assert.match(wf, /--pr "\$pr"/, 'the job must run the script against the PR');
  assert.match(wf, /printf '%s\\n' "\$out"/, 'the corpus step must print the script output, so every CHECK row is visible to the reader');
});

test('a missing or unparsable summary line fails the step, not passes it', () => {
  // The script is designed to exit 0 even when it crashes (a checker crash
  // must never read as a body defect), so a gate keyed on the exit code would
  // pass on a broken checker. The gate is keyed on the summary line instead —
  // pin both arms of that, or the next editor reverts to the exit-code gate
  // and a crash reads as a clean body.
  assert.match(wf, /missing summary line/, 'a run with no summary line must be a tool failure, not a pass');
  assert.match(wf, /unparsable/, 'an unparsable summary must be a tool failure, not a pass');
});

test('a [scratch]-titled PR gets a REPORT-ONLY arm, never a job-level skip (#3373 round 1)', () => {
  // The reviewer's blocking finding, fixed by giving the exemption a real arm:
  // a [scratch] body is deliberately stale, so on one the steps RUN and print
  // their rows but cannot fail the run (`continue-on-error: true`) — report-
  // only in the literal sense, something that runs and reports. A job-level
  // `if:` skip would run nothing, "report-only" would describe nothing, and
  // the documented revert of the exemption would be unobservable (the skip arm
  // and the gate arm would be the same job). This pin holds all four crossings
  // of the exemption: the continue-on-error lines exist (the report-only arm),
  // both carry the scratch title test (so report-only keys on the same
  // condition on every step), and a step that refuses but is NOT exempt
  // would need a second gate-side continue-on-error — pinned to exactly one
  // below.
  const coe = [...wf.matchAll(/continue-on-error:\s*\$\{\{ startsWith\(github\.event\.pull_request\.title, '\[scratch'\) \}\}/g)];
  assert.equal(coe.length, 2, `exactly the gate and corpus steps carry the scratch arm, found ${coe.length}`);
  // The job itself must NOT be gated on the title — a job-level `if:` reading
  // it is the skip shape this pin exists to prevent from coming back.
  const jobIf = wf.match(/if:\s*\$\{\{[^\n]*\}\}/);
  assert.ok(jobIf, 'the job must keep an if: (the pull_request guard)');
  assert.doesNotMatch(
    jobIf[0],
    /startsWith|scratch|\[scratch/,
    'the job-level if: must not read the PR title — a title-gated job skip is a report-only arm that reports nothing',
  );
});

test('exactly one non-scratch continue-on-error is allowed on the gate step', () => {
  // The counterfactual that makes the scratch arm fail-able: a `continue-on-
  // error: true` sitting UNCONDITIONALLY on the gate step would make the
  // refusing PR green everywhere — the exemption leaking to the class it
  // exempts. Any non-scratch continue-on-error YAML KEY in the file is that
  // leak. Keyed on the actual key shape (leading indentation, colon) so the
  // word appearing in prose comments does not trip it — the pin reads the
  // file's config, not its commentary.
  const other = [...wf.matchAll(/^[ \t]+continue-on-error:(?![ \t]*\$\{\{ startsWith)/gm)];
  assert.equal(other.length, 0, `an unconditional (or differently-keyed) continue-on-error would exempt a non-scratch PR: ${other.length} found`);
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

test('the skill text carries the report-only arm, never a job-level-skip sentence (#3373 round 2)', () => {
  // Round 2's blocking finding lived HERE, not in the workflow: the round-1 fix
  // corrected the workflow, the body and the test file but left the ci-validate
  // skill telling workers the required check is *skipped* on a scratch PR — the
  // same wrong thing, one surface over. This pin is the twin sweep the
  // disposition claimed: the skill's CI paragraph must state the report-only
  // arm (the steps run and print), and must not state a job-level skip about
  // this workflow. Scoped to the paragraph that names the check, so unrelated
  // prose (docs/README.md's deploy-job sentence) cannot trip it.
  const skill = fs.readFileSync(path.join(root, '.claude', 'skills', 'ci-validate', 'SKILL.md'), 'utf8');
  const para = skill.split('\n').find((l) => l.includes('prbodycheck.yml')); // the CI paragraph's first line
  assert.ok(para && para.includes('prbodycheck.yml'), 'the skill must have a paragraph naming the prbodycheck.yml workflow');
  const start = skill.indexOf(para);
  const slice = skill.slice(start, skill.indexOf('CI running it does not retire', start));
  assert.match(slice, /REPORT-ONLY/, 'the skill must tell workers a scratch PR runs the job report-only');
  assert.match(slice, /continue-on-error/, 'the skill must name the mechanism, so the arm stays observable in the text agents read');
  assert.doesNotMatch(
    slice,
    /skip[s]? the (job|pr-body-check check)|job is skipped/i,
    'the skill must not say the job is skipped on a scratch PR — the round-2 blocking class, on the surface agents execute',
  );
});

test('the workflow pins the checker call to this repo, so the corpus is this repo’s', () => {
  // Without `--repo`, `gh` resolves from the checkout's remote — right for
  // same-repo PRs, wrong the day this workflow runs on a fork or the repo is
  // renamed. Pin the explicit form the step uses.
  assert.match(wf, /--repo "\$GITHUB_REPOSITORY"/, 'the script must be told the repo explicitly');
});
