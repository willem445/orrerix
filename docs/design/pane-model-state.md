# Per-pane model and context state (#993)

This note records the planned sources and capability facts for pane context
state. S0 adds capability metadata only; no reader consumes it yet. A source
is read from the CLI's artifact rather than inferred from a model name.

## Source matrix

| CLI | Live model | Live effort | Context window | Context used | Compact command loomux may send | Compaction-done signal |
|---|---|---|---|---|---|---|
| **claude** (PTY) | Status-line `model.id` / `model.display_name` (S1); transcript `message.model` already exists | Status-line `effort.level` (S1) | Status-line `context_window.context_window_size` (S1) | Status-line `context_window.total_input_tokens` / `used_percentage` (input-token accounting) (S1) | `/compact` (Claude Code [slash commands](https://code.claude.com/docs/en/commands)) | `PreCompact`, `SessionStart(compact)`, transcript `compact_boundary` already exist; `PostCompact` is planned (S5; [hooks reference](https://code.claude.com/docs/en/hooks)) |
| **codex** (PTY) | Rollout `turn_context.payload.model` already exists | Rollout `turn_context.payload.effort` (S2a; optional `ReasoningEffort`) | Rollout token-count `info.model_context_window` (S2a) | Same event's `info.last_token_usage.input_tokens` (S2a) | `/compact` ("Summarize the visible chat to free tokens"; [CLI command reference](https://learn.chatgpt.com/docs/developer-commands?surface=cli)). `model_auto_compact_token_limit` documents automatic compaction ([config reference](https://learn.chatgpt.com/docs/config-file/config-reference)). | Rollout `compacted`; `PreCompact` / `PostCompact` hooks are documented ([hooks](https://learn.chatgpt.com/docs/hooks)) |
| **pi** (PTY) | Session's latest assistant `provider` / `model`; `model_change` entries | `thinking_level_change`; initial level is the launcher's `--thinking` choice | `--list-models` context column; RPC `get_state.model.contextWindow`; not present in session file (S8) | Latest assistant `usage.input + cacheRead + cacheWrite` | `/compact [prompt]` ([usage guide](https://github.com/earendil-works/pi/tree/v0.84.4/packages/coding-agent/docs/usage.md)) | Session `compaction` entry; RPC `compaction_end` |
| **opencode** (PTY) | Session model value (upstream observed JSON `{id, providerID, variant}`) | `variant` in that JSON (S2c) | Unknown: no documented source in the [configuration reference](https://opencode.ai/docs/config/) | Per-message `message.data` tokens are a labelled observation to verify at v1.18.11 (S2c) | `/compact` (alias `/summarize`; [TUI guide](https://opencode.ai/docs/tui/)) | No documented signal; token-drop inference only |
| **copilot** (PTY) | No machine-readable source documented; use launcher's declared model, labelled `declared` | `~/.copilot/settings.json` `effortLevel` is a read-only global setting, labelled `settings` | No documented source | No documented source; `/context` displays a visualization | `/compact [FOCUS-INSTRUCTIONS]` ([CLI command reference](https://docs.github.com/en/copilot/reference/cli-command-reference)) | `preCompact` hook; no documented post-compact hook ([hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference)) |
| **gemini** (PTY) | No source in this slice | No source in this slice | No source in this slice | No source in this slice | No command established in this slice | No signal established in this slice |

The `compact_note` on `CliCaps` records facts left unknown by the available
references. The Codex CLI command reference lists `/compact` in its TUI commands;
its configuration reference separately documents automatic compaction.
Copilot's context-management guide says the CLI automatically starts compacting
at approximately 80% of its context-window capacity and documents manual
`/compact`. Gemini is supported elsewhere in `CLI_CAPS` but absent from the
plan's five-CLI source summary, so S0 gives it no reader and makes no
undocumented capability claim.

## Compaction tiers

The plan groups Claude, Codex, and pi as token-visible and triggerable (Tier 1).
OpenCode is token-visible but has no documented window, so it cannot supply a
percentage or a threshold escalation. Copilot is blind to token counts (Tier 2)
but has a documented manual `/compact` command. Copilot, pi, Codex, and OpenCode
may also self-compact (Tier 3); later slices must still arrange
offload/re-grounding before that automatic compaction. Gemini remains
unclassified here.

## Window selection ladder (planned)

`effective_context_window_tokens` will select, in order: (1) group override,
(2) a CLI-reported window (Claude status line, Codex token-count event, or pi
model list/RPC), (3) the existing conservative Claude model table while no
status-line report is available, and (4) an empirical clamp that widens a
window when observed usage exceeds it. The last rung must be tagged `clamped`.
A reported window is not replaced by a model-name guess.

## S8: pi's window from `--list-models`

pi's session file carries no context window, so for a pi PTY pane the window
comes from the table `pi --list-models` prints, which the launcher's probe
already runs for the model picker. S8 reads its `context` column:
`cliprobe.rs`'s `parse_context_windows_from_table` walks the same rows as
`parse_models_from_table` and fills two additive `CliProbe` fields.

- `model_context_windows`: model id → window in tokens.
- `model_context_windows_rounded`: the ids whose window is a lower bound
  rather than an exact count (below).

`models` is unchanged. Both new fields are left off the wire when empty, which
they are for every CLI but pi, so no other CLI's `probe_agent_cli` reply
changes. A header with no `context` column, a row whose cell count differs
from the header's, or a cell that does not parse adds no entry: an id with no
entry has no reported window, never a guessed one.

**The spellings.** pi prints the count with `formatTokenCount` (`SOURCE`
`src/cli/list-models.ts:14-24` at the pin in `docs/design/pi.md`; the same
function at `dist/cli/list-models.js:10-20` in the installed 0.87.1 package).
Below 1,000 it prints the raw integer (`512`). Otherwise it prints thousands or
millions with a `K`/`M` suffix: an integer when the count divides exactly
(`200K`, `1M`), and **rounded to one decimal place otherwise** (`262.1K` for
262,144; `1.0M` for 1,048,576). The rounded form is common, not an edge case:
721 of the 1,495 `contextWindow` values in the model catalog shipped with the
installed 0.87.1 package (`pi-ai`'s `dist/providers/data/*.json`) print that way.

**Decision: a rounded spelling is read as the lower edge of its rounding
interval.** `262.1K` becomes 262,050 and `1.0M` becomes 950,000: the printed
value minus half a printed tenth. Integer and raw spellings stay exact. The
window feeds the compaction threshold. An understated window compacts a little
early, which is safe; an overstated one lets the CLI's own emergency compaction
fire first. Reading the spelling at face value would overstate by up to that
half-tenth (1,050,000 prints `1.1M`). The edge is never above the real count,
because `toFixed(1)` picks the tenth nearest the count. Face value ("nearest")
is the alternative, and it is a one-line change in `parse_token_count`; the
human may choose it.

A consumer labels a window from an id in `model_context_windows_rounded`
`reported-rounded` rather than `reported` on the ladder's rung (2).

**No consumer yet.** Looking the pane's current model up in the cached probe,
and filling `window_tokens` from it, belongs to the pi arm of
`agent_context_signals`, which S2b adds. S8 ships the data only.

## Contract changes planned by later slices

These are planned additions, not shipped behavior in S0:

1. **S3 will** add `window_tokens`, `window_source`, `model`, `effort`, `source`,
   and `declared: {model, effort}` to `group_summary.agents[].context`.
2. **S1 will** write the raw Claude status-line payload to
   `<group>/hooks/<agent>.statusline.json` using a temporary file and rename.
3. **S6 will** add optional `effort` to usage-series samples with a serde default.
4. **S1 will** add a `statusLine` entry to Claude's `--settings` configuration;
   the user's existing status-line command is intended to be chained.
5. **S0 adds** `compact_command`, `self_compacts`, `compact_note`, and
   `context_reader` to `CliCaps`; no code reads these fields until later slices.

## Sources and pins

- Claude Code: [commands](https://code.claude.com/docs/en/commands),
  [hooks](https://code.claude.com/docs/en/hooks), and
  [status line](https://code.claude.com/docs/en/statusline).
- GitHub Copilot CLI: [command reference](https://docs.github.com/en/copilot/reference/cli-command-reference),
  [context management](https://docs.github.com/en/copilot/concepts/agents/copilot-cli/context-management),
  and [hooks](https://docs.github.com/en/copilot/reference/hooks-reference).
- Codex: [CLI commands](https://learn.chatgpt.com/docs/developer-commands?surface=cli),
  [configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference),
  and [hooks](https://learn.chatgpt.com/docs/hooks). The CLI command reference
  lists `/compact` in the TUI command list; the configuration reference documents
  automatic compaction.
- pi: `packages/coding-agent/docs/usage.md` and `compaction.md` at
  [`v0.84.4`](https://github.com/earendil-works/pi/tree/v0.84.4/packages/coding-agent/docs).
- OpenCode: [TUI guide](https://opencode.ai/docs/tui/) and
  [configuration](https://opencode.ai/docs/config/); the message-token shape
  and session-model JSON are S2c observations to verify against upstream
  `v1.18.11`, not claims established by these published docs.
