# `tokenscorecard` — the corpus for the tokens-pane scorecard (#2011 D)

CUT FROM THE REAL GROUP LOG, not hand-written: the scorecard's inputs are the
pane's `AuditStore` read, and a fixture cut from the real log carries the real
row shapes (`rd-lane-spawned` details, the real verdict vocabulary) that a
hand-written corpus inevitably simplifies away. `build.cjs` is committed so a
regeneration is a diff rather than a rewrite.

## Provenance

Source: the loomux orchestration group's own audit log, both generations
(`audit.1.jsonl` then `audit.jsonl`), file order preserved. Selected: every
row naming PRs **2941, 2942, 2943, 2947, 3038** (the review-loop lanes of
#2011's own slices A/B/C — structural `detail.pr` or a `#N` token anywhere in
the detail), plus every `agent-spawn` row naming an agent those rows credit.
Rows whose action carries a field the scorecard reads (`agent-spawn`,
`review-verdict`, `rd-lane-spawned`, `rd-handback`) are kept WHOLE; every
other selected row is projected to the facts the module reads — `action`,
`actor`, `ts_ms`, and `detail` reduced to `{pr}` (when numeric) and
`{names: […]}`, the PRs whose `#N` token occurred anywhere in the original
detail. Long prompt and tool payloads are dropped, not truncated. One
DELETION is deliberate: the spawn rows of `rev-2416` are removed from BOTH
passes, so the corpus carries the rotation shape for real — #2941 is credited
with a delegate whose `agent-spawn` row did not survive, and its lane still
resolves through its later-round agents, so the floor moves without moving
the table.

Cut on 2026-09-06 from the live group's log; the log keeps growing, so a
regeneration after that date is expected to move `floor.tsLastMs` and
`rowsRead` even with no code change.

## Re-blessing

Change the `PRS` / `SPAWN_DROP` tables in `build.cjs`, re-run it against the
group dir, and re-derive every number `test/tokenscorecard.test.ts` pins —
they are hand-derived from the verdict sequences, not recorded from a run. A
regeneration that moves a number is a re-bless, not a fix.
