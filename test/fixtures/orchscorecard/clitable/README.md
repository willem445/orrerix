# `clitable` — the corpus for `--format cli-table` (#2011 A)

Generated, not hand-written: `node test/fixtures/orchscorecard/clitable/build.cjs`
rewrites all four files from the table at the top of that script. The generator
is committed so the corpus's axes are auditable and a regeneration is a diff
rather than a rewrite — the shared corpus one directory up stays hand-written,
because its rows encode message shapes rather than a population.

Every axis carries more than one value, so no counter can pass by reading a
constant (CLAUDE.md, the non-discriminating-fixture rule):

| PR | window | worker-std | rev-std | why it is here |
| --- | --- | --- | --- | --- |
| #800 | pre-2817 | opencode | opencode | its `worker-std` pane **shares a session with a claude `worker-adv` one** and the row's source is `transcript`, so it stays opencode only via the **cross-CLI conflict rung** — the shape measured on the live store's `358b100f…` and `e81c5d8a…` |
| #801 | pre-2817 | opencode | opencode | its `worker-std` delegate's **`agent-spawn` row is dropped** the way a rotation drops it, while the `rd-lane-spawned` row crediting it survives — the **straddling window** whose start is *after* the coverage floor, which is why `start_ms <= ts_first` cannot see it |
| #802 | pre-2817 | opencode | opencode (`statusline` row) | its cli comes from the **spawn-row rung**, so the opencode side is n=3 only if that rung works |
| #803 | pre-2817 | pi | pi | merged before the split but resolves to pi — the **side/cli disagreement** |
| #810 | post-2817 | pi | pi | |
| #811 | post-2817 | pi | pi | the pi side is n=2, so every pi cell is `null` |
| #820 | post-2817 | opencode **and** pi | pi | two `worker-std` panes on two clis: the **same-block split** `by_block_cli` has to keep apart, and the reason this PR is excluded |
| #821 | post-2817 | pi | pi | no `merged_at` — excluded |
| #822 | post-2817 | pi | opencode | the two compared lanes disagree — excluded |

`rev-final` is `transcript` (claude) on **every** PR. That is what makes it the
control: a column that moves there moved for a reason that is not the
worker/reviewer CLI.

Attribution is **structural** throughout (an `rd-lane-spawned` row per delegate
carrying both the agent and the PR), so no text-tier heuristic is in play and
the table's population is exact rather than window-dependent.

## Re-blessing

Change the `PRS` table in `build.cjs`, re-run it, and re-derive every count the
suite pins. The numbers in `test/orchscorecard.test.ts` are written against this
population; a regeneration that moves one is a re-bless, not a fix.
