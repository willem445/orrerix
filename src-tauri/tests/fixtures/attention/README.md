# Attention fixtures

Real pane tails used by the provider-limit tests (`src-tauri/tests/orchestration.rs`,
the `#2811 S5a` section). The `claude-usage-limit` / `openrouter-key-limit` /
`openrouter-credits-exhausted` captures and the `negative-*` controls are
described in the tests that read them; this README is about the
`positive-convention-*` files (#3190).

## `positive-convention-*.txt` — the CURRENT CLI's real output

Each of these pins the rendering convention that
`providerlimit::limit_in_tail`'s paragraph-start rule depends on: the agent
CLI renders a provider refusal as its own block, with a **blank gutter row
before it**. One file per `LIMIT_PATTERNS` row:

| fixture | CLI / provider | refusal text taken verbatim from |
| --- | --- | --- |
| `positive-convention-claude-usage-limit.txt` | Claude Code (plain, no gutter) | `claude-usage-limit.txt` |
| `positive-convention-openrouter-key-limit.txt` | pi/opencode over OpenRouter (`U+2503` gutter) | `openrouter-key-limit.txt` |
| `positive-convention-openrouter-credits-exhausted.txt` | pi/opencode over OpenRouter (`U+2503` gutter) | `openrouter-credits-exhausted.txt` |

The refusal line(s) in each file are byte-identical to the captured fixture
named in the table; the context line above the blank row is representative
output, not a capture. The point of these files is the convention, not the
history: `the_current_convention_positive_controls_are_detected` demands that
each one is still detected.

## Re-bless step (on a CLI bump)

These fixtures are a claim about how the CLI prints a refusal **today**, so
they go stale exactly when the CLI changes. When the CLI that runs in these
panes is upgraded, re-capture a provider refusal and update the matching
`positive-convention-*` file. If the new output no longer puts a blank row
before the refusal, `the_current_convention_positive_controls_are_detected`
fails — that is the point, not a regression to fix by hand-editing the
fixture back: the paragraph-start rule silently misses a refusal glued under
other output (`a_refusal_glued_under_other_output_is_not_detected` pins that
miss as expected, so it stays green straight through the regression), and a
reddening fixture is what turns the silent detection loss into a decision —
update the rule, or accept the new false negative and record it. Re-run the
needle-extraction against the new capture rather than editing lines by hand:
the refusal text must stay verbatim from a capture.
