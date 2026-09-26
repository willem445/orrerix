// Fixtures shared by the workflow model's split test files (#3498 F2). Moved verbatim out
// of test/workflowmodel.test.ts, with `export` added, so each split file reads the SAME
// reference workflow rather than its own drifting copy.
import type { Finding, FindingCode } from "../../src/workflowmodel.ts";

/** The schema sketch from the #222 investigation (§4), verbatim in spirit: the file the
 *  feature was designed around. If this stops reading, the feature is broken. */
export const SAMPLE = `# <repo>/.loomux/workflow.yml
version: 1
name: focused-review

blocks:
  - id: planner
    name: Planner
    kind: planner
    cli: claude
    model: opus

  - id: worker
    name: Worker
    kind: worker
    cli: copilot
    profile: .github/agents/worker.md
    model: auto

  - id: rev-security
    name: Security review
    kind: reviewer
    cli: claude
    model: opus
    prompt: |
      Review ONLY for security defects: injection, authz, secrets, path traversal.
      Ignore style and perf — other reviewers cover those.

  - id: rev-tests
    name: Test-quality review
    kind: reviewer
    cli: claude
    model: sonnet
    prompt: |
      Review ONLY test quality: do the tests exercise intent?

edges:
  - { from: planner, to: worker }
  - { from: worker,  to: [rev-security, rev-tests] }

gates:
  merge:
    require: all-pass
    reviewers: [rev-security, rev-tests]
    also: [ci-green]
`;

export const codes = (findings: readonly Finding[]): FindingCode[] => findings.map((f) => f.code);
export const has = (findings: readonly Finding[], code: FindingCode): boolean =>
  findings.some((f) => f.code === code);
