# Repo-recorded lessons

Rules that must survive the group that learned them — rule + fix only, a bare issue ref
as provenance. Newest at the bottom. Only ~4 KB reaches a kickoff, and eviction is
silent to whoever wrote the entry: append, then check the file still fits, and say in
the PR what you displaced. A permanent rule belongs on an uncapped surface (`CLAUDE.md`,
a skill, the role templates) — this file is for what must reach every kickoff. See
`docs/design/lessons.md`.

## No getrandom-based crates in src-tauri [pinned]

No `uuid` v4, `rand`, `tempfile`-with-defaults, or anything pulling `getrandom` into
`src-tauri` — the binary fails to load (`0xc0000139`) on the Windows 10 baseline. Use
std's OS-seeded `RandomState` for ids/tokens. Before adding a dependency: check
`src-tauri/Cargo.toml`'s notes and run `cargo tree -e normal --target all -i
getrandom@<version>`.

## Never resize the PTY for a UI feature [pinned]

UI features are overlays or header/board chrome floating over the terminal — never a
resize. Visual padding goes on the `.xterm` element, not the layout.

## Never `git stash` in an orrerix worktree [pinned]

The stash stack is shared across ALL worktrees of a repo (#299). Commit WIP to your own
branch instead. If you must stash: `git stash push -m "<your agent id>: ..."` and only
`pop` entries carrying your own marker.

## A claim is a deliverable

A comment, design note, audit label or PR body claiming something the code doesn't do is
a defect (#461, #490) — delete it, don't soften it. Quote raw fetched text for CLI/API
facts (#453). Prose written against a plan stays dated to it: before marking ready,
re-read every doc and `SKILL.md` claim against what the sibling slice actually SHIPPED,
not the plan both were written from (#715, #721).

## Commit real work BEFORE capturing red evidence [pinned]

`git checkout -- <file>` discards ALL uncommitted work in it, not just your evidence
patch (#493). Commit the real work first (amend/squash later), then patch for the red
run, then restore — never point a destructive git command at a file holding anything
uncommitted.

## Any suppression driven by a fallible signal must be BOUNDED

A guard holding an action "while X is true" needs an answer for "X never clears" (#496,
#513, #518). Fix the signal AND bound the consumer; release on independent evidence, not
elapsed time. Extending to "every consumer" means grepping the subsystem and publishing
the list.

## `rustfmt --check` is the one local Rust check agents may run

From the repo root: `rustfmt --check --edition 2021 <changed .rs> >/dev/null`.
`--edition` is mandatory (2015 false-errors `async fn`); discard stdout — those are
*unenforced* formatting diffs, noise here and never a
finding; the exit code is ambiguous, so **stderr is the signal**. Never run bare
`rustfmt`, commit a reformat, or cite a clean run as validation. `cargo check` stays
banned (#488, #558). Never run it on a file over the `ci-validate` skill's size cap
(any file over 5,000 lines, or whose `mod` children include one, as
`orchestration/mod.rs`'s do): it needs tens of GB of RAM there (#3469).

## Never block a turn waiting on CI

An `[orrerix]` notice arrives by typing into a pane, and a mid-turn pane can't take
delivery — waiting on CI queues the answer behind the turn (#590). Register the watch,
END the turn, act on the notice. Same for any pane-delivered answer: reading once is
fine, waiting is the defect.

## Cheap-tier roster: literal briefs, sequenced lanes, worker-std by default
When the roster carries `worker-std`/`rev-std`:
1. A brief names EXACT files and functions, the EXACT commands to run and the
   EXACT output that means done; one task, no judgment calls left open; grep
   every anchor before briefing. PR bodies carry the diffstat and run ids —
   no derived receipts (each one costs a review round).
2. Every claim in a report/PR body must quote the command and its output;
   a claim with no output is a failure, not a nit.
3. Default every task to `worker-std`. `worker-adv` ONLY after a `worker-std`
   has tried and failed (blocked, or the same finding failed twice — briefed
   fresh, never resumed), or for an extremely complex task, with the reason
   named in the brief.
4. `rev-std` runs every round, to PASS on a FINAL body (batch body edits before
   the record). `rev-final` is spawned ONCE, last, only then — and only where
   the gate's routing requires it (code, tests, CI, manifests).
5. A rev-std brief focuses on big-picture architecture, simplicity, maintainability,
   correctness, and STE-100 docs — let `pr-body-check.cjs` handle receipt checks.
   Pass 1 may note small items; pass 2+ lets unimportant nits go once main points are
   addressed (2 passes target, 3 max).
6. Read rev-final's "review findings" per PR; when three consecutive PRs show no
   blocking miss by rev-std, propose dropping rev-final (the human's rule).

## Non-blocking findings and review convergence (aim for 2 passes, 3 max)

Reviewers focus on the big picture: architecture, simplicity, long-term maintainability,
correctness, and concise STE-100 docs/comments written for general future human readers
(strip overly specific agent notes). Pass 1 may flag small non-blocking items; on pass 2+,
once main points are addressed, let unimportant nits go and pass. When every required lane
passes with only non-blocking items open at round >= 2, defer them (reason, a line in the
PR's disposition comment, one line to the human — not a new issue) instead of routing
another round, unless a finding names a real defect. Workers always document code and
docs in STE-100 format. (#2168, #3441)
