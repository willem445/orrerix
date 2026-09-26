# Per-pane model and context state (#993)

This note records the planned sources and capability facts for pane context
state. S0 adds capability metadata only; no reader consumes it yet. A source
is read from the CLI's artifact rather than inferred from a model name.

## Source matrix

| CLI | Live model | Live effort | Context window | Context used | Compact command loomux may send | Compaction-done signal |
|---|---|---|---|---|---|---|
| **claude** (PTY) | Status-line `model.id` / `model.display_name` (S1); transcript `message.model` already exists | Status-line `effort.level` (S1) | Status-line `context_window.context_window_size` (S1) | Status-line `context_window.total_input_tokens` / `used_percentage` (input-token accounting) (S1) | `/compact` (Claude Code [slash commands](https://code.claude.com/docs/en/commands)) | `PreCompact`, `SessionStart(compact)`, transcript `compact_boundary` already exist; `PostCompact` is planned (S5; [hooks reference](https://code.claude.com/docs/en/hooks)) |
| **codex** (PTY) | Rollout `turn_context.payload.model` already exists | Rollout `turn_context.payload.effort` (S2a; optional `ReasoningEffort`) | Rollout token-count `info.model_context_window` (S2a) | Same event's `info.last_token_usage.input_tokens` (S2a) | Unknown: the [CLI command reference](https://learn.chatgpt.com/docs/developer-commands?surface=cli) does not confirm a TUI `/compact`; capability remains `None`. `model_auto_compact_token_limit` documents automatic compaction ([config reference](https://learn.chatgpt.com/docs/config-file/config-reference)). | Rollout `compacted`; `PreCompact` / `PostCompact` hooks are documented ([hooks](https://learn.chatgpt.com/docs/hooks)) |
| **pi** (PTY) | Session's latest assistant `provider` / `model`; `model_change` entries | `thinking_level_change`; initial level is the launcher's `--thinking` choice | `--list-models` context column; RPC `get_state.model.contextWindow`; not present in session file (S8) | Latest assistant `usage.input + cacheRead + cacheWrite` | `/compact [prompt]` ([usage guide](https://github.com/earendil-works/pi/tree/v0.84.4/packages/coding-agent/docs/usage.md)) | Session `compaction` entry; RPC `compaction_end` |
| **opencode** (PTY) | Session model value (upstream observed JSON `{id, providerID, variant}`) | `variant` in that JSON (S2c) | Unknown: no documented source in the [configuration reference](https://opencode.ai/docs/config/) | Per-message `message.data` tokens are a labelled observation to verify at v1.18.11 (S2c) | `/compact` (alias `/summarize`; [commands guide](https://opencode.ai/docs/commands/)) | No documented signal; token-drop inference only |
| **copilot** (PTY) | No machine-readable source documented; use launcher's declared model, labelled `declared` | `~/.copilot/settings.json` `effortLevel` is a read-only global setting, labelled `settings` | No documented source | No documented source; `/context` displays a visualization | `/compact [FOCUS-INSTRUCTIONS]` ([CLI command reference](https://docs.github.com/en/copilot/reference/cli-command-reference)) | `preCompact` hook; no documented post-compact hook ([hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference)) |
| **gemini** (PTY) | No source in this slice | No source in this slice | No source in this slice | No source in this slice | No command established in this slice | No signal established in this slice |

The `compact_note` on `CliCaps` records facts left unknown by the available
references. For Codex, the lack of a confirmed TUI slash command is not a claim
that no such command exists. Copilot's automatic-compaction behavior is likewise
unknown; its boolean default is conservative, and the row's note records that.
Gemini is supported elsewhere in `CLI_CAPS` but absent from the plan's five-CLI
source summary, so S0 gives it no reader and makes no undocumented capability
claim.

## Compaction tiers

The plan groups Claude, Codex, and pi as token-visible and triggerable (Tier 1).
OpenCode is token-visible but has no documented window, so it cannot supply a
percentage or a threshold escalation. Copilot is blind to token counts (Tier 2)
but has a documented `/compact` command. Pi, Codex, and OpenCode may also
self-compact (Tier 3); later slices must still arrange offload/re-grounding
before that automatic compaction. Gemini remains unclassified here.

## Window selection ladder (planned)

`effective_context_window_tokens` will select, in order: (1) group override,
(2) a CLI-reported window (Claude status line, Codex token-count event, or pi
model list/RPC), (3) the existing conservative Claude model table while no
status-line report is available, and (4) an empirical clamp that widens a
window when observed usage exceeds it. The last rung must be tagged `clamped`.
A reported window is not replaced by a model-name guess.

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
  and [hooks](https://learn.chatgpt.com/docs/hooks). The command page extraction
  does not confirm a TUI `/compact`; therefore `compact_command` is `None`.
- pi: `packages/coding-agent/docs/usage.md` and `compaction.md` at
  [`v0.84.4`](https://github.com/earendil-works/pi/tree/v0.84.4/packages/coding-agent/docs).
- OpenCode: [commands](https://opencode.ai/docs/commands/) and
  [configuration](https://opencode.ai/docs/config/); the message-token shape
  and session-model JSON are S2c observations to verify against upstream
  `v1.18.11`, not claims established by these published docs.
