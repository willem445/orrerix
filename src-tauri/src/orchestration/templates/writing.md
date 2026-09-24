Everything you post is read by a human first — a PR or issue body, a comment, a board row — and
usually in under a minute. Write for that reader.

- **A body opens with the human layer.** A short summary of what changed (or what is wrong) and
  why; what to review — the files, the behaviour or the decision to look at, and anything the
  human has to decide; and how it was tested, in a line or two: the check that went red then
  green, the CI run, or the reason no new test exists. Receipts — commands and their output, run
  ids, hashes, mutation tables, sweep results — go in the collapsed agent layer, or nowhere. A
  decision, a deviation from the brief, a residual or an open question never goes below the
  fold.
- **A comment is a few readable lines.** Supporting detail is fine when it is short; anything
  long goes in a fold, exactly as in a body.
- **Board text is one or two plain sentences** — a task's description and each note. The issue
  or PR it links carries the rest.
- **Open an issue only for work that must be tracked:** a feature, or a real defect. A review
  nit deferred from a PR is a line in that PR's disposition comment and a line to the human,
  never a new issue. When small follow-ups pile up in one area, they go as lines on that area's
  one rolling follow-up issue, not one issue each.
- **Everything you post on GitHub ends with one line** saying who wrote it:
  `— Written by AI (<your agent id>, <your model>) on behalf of the human (@operator)`. It is the last line,
  below the agent layer, so a squash message — cut at the `<!-- agent-layer -->` line — never
  carries it. No URL, no session id, no tool footer; commit messages do not carry it.
