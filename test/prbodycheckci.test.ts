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
//
// Every workflow pin below reads the file's CONFIG, never its commentary: `wf`
// is the workflow with its whole-line `#` comments removed (YAML comments and
// the shell comments inside `run:` blocks alike). The workflow explains itself
// at length, and a pin matched against the raw file passes on the explanation
// after the code it describes is gone — `/--gate/` matched the comment "`--gate`
// is the one invocation…" with `--gate` deleted from the run line, and
// `/missing summary line/` matched ONLY a comment (#3373 round 3). The
// refusal-bearing lines are pinned whole, as exact lines.

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
/** The workflow with every whole-line comment removed — the config the pins read. */
const stripComments = (text: string): string =>
  text.split(/\r?\n/).filter((l) => !/^\s*#/.test(l)).join('\n');
let wf = '';

/** An exact line of the config, modulo its indentation — a pin a comment cannot satisfy. */
const hasLine = (text: string, line: string): boolean =>
  text.split('\n').some((l) => l.trim() === line);

test('the job runs on the PR events a body edit needs', () => {
  wf = stripComments(readWf());
  // Positive control on the strip: the config survived it (both steps' run
  // blocks are still there), so every absence below is about the config, not
  // about a strip that ate the file.
  assert.equal([...wf.matchAll(/^\s+run: \|/gm)].length, 2, 'the stripped workflow must still carry the gate and corpus run blocks');
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
  // test/prbodycheck.test.ts; this pin is the CI half of the pair. The whole
  // invocation is pinned as one exact line: dropping `--gate`, or appending
  // `|| true`, turns the required check permanently green.
  assert.ok(
    hasLine(wf, 'node scripts/pr-body-check.cjs --pr "$pr" --repo "$GITHUB_REPOSITORY" --gate'),
    'the gate step must invoke the script with --gate against this PR, the one exit-nonzero mode — as that exact line',
  );
  assert.match(wf, /printf '%s\\n' "\$out"/, 'the corpus step must print the script output, so every CHECK row is visible to the reader');
});

test('a missing or unparsable summary line fails the corpus step, not passes it', () => {
  // This is the CORPUS step's arm, not the gate step's: the gate step is keyed
  // on the script's `--gate` exit code (pinned above). The corpus step runs
  // the script in its default mode, which exits 0 even on a crash (a checker
  // crash must never read as a body defect), and swallows the exit with
  // `|| true` — so it is keyed on the SUMMARY line, and an empty parse must
  // count as a failure. Drop the `-z` half of this guard and a crashed checker
  // counts as a clean corpus PR (`[ "" -ne 0 ]` errors, and the `if` reads
  // that as false).
  assert.ok(
    hasLine(wf, 'if [ -z "$mismatch" ] || [ "$mismatch" -ne 0 ]; then'),
    'an empty (missing or unparsable) summary must count as a corpus failure — as that exact guard line',
  );
  assert.match(wf, /\$\{mismatch:-<unparsable>\} MISMATCH/, 'the failure line must name an unparsable summary as such');
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
  // Both ends of the window are asserted, not assumed: a missing sentinel makes
  // `indexOf` -1 and `slice(start, -1)` silently reads the REST OF THE FILE.
  // Residual, stated: the window is bounded by two anchors (the first line
  // naming the workflow, and the sentinel sentence). A reword that moves either
  // onto different prose shifts the window without reddening this pin, as long
  // as both still exist in that order.
  const end = skill.indexOf('CI running it does not retire', start);
  assert.ok(end > start, 'the skill\'s CI paragraph must still end at "CI running it does not retire" — the pin\'s window is lost');
  const slice = skill.slice(start, end);
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
