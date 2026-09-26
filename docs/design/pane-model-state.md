# Per-pane model and context state (#993)

This note records the planned sources and capability facts for pane context
state. S0 added capability metadata only. S1 adds the first reader: Claude's
status-line payload (see [S1](#s1-the-claude-status-line-source)). A source
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

## Window selection ladder

`effective_context_window_tokens` selects, in order: (1) group override,
(2) a CLI-reported window, (3) the existing conservative Claude model table
while no status-line report is available, and (4) an empirical clamp that
widens a window when observed usage exceeds it, tagged `clamped`. A reported
window is not replaced by a model-name guess. S1 ships the ladder
(`modelstate::context_window_ladder`) and the Claude status line as rung 2's
first source. The Codex token-count event (S2a) and pi's model list (S8)
**will** feed the same rung.

The clamp never overrules an override. A human who set 200K on a pane that
reads 250K has a deployment the override exists to describe. The percent
saturates at 100 either way, which escalates.

## S1: the Claude status-line source

Claude Code runs a `statusLine` command once when a session starts or
resumes, and again on each new assistant message, when `/compact` finishes,
and on a few UI events. Updates are debounced at 300 ms. The command gets a
JSON payload on stdin and the CLI displays whatever it prints
([status line](https://code.claude.com/docs/en/statusline)). That payload
carries the one fact no transcript records: `context_window.context_window_size`,
"200000 by default, or 1000000 for models with extended context". It also
carries `model.id`, `effort.level` and `session_id`.

**The hook.** loomux's `--settings` file gains a `statusLine` entry whose
command runs `COMPACT_HOOK_SCRIPT`'s `statusline` arm. The arm reads the
payload into a variable and writes it to
`<group>/hooks/<agent>.statusline.json` via a `.tmp` file and `mv -f`, so the
tick never reads a torn file. It then feeds the same payload to the human's
own status-line command, passed as `$4`, and prints nothing of its own. It
exits 0 on every path. The entry exists exactly when the lifecycle hooks do:
same script, same absolute `sh`.

**Why it chains (the human's decision on #993).** Settings precedence is
managed, then `--settings`, then `.claude/settings.local.json`, then
`.claude/settings.json`, then `~/.claude/settings.json`
([settings](https://code.claude.com/docs/en/settings), "Settings
precedence"). So loomux's `statusLine` replaces the human's in an orrerix
pane. Chaining keeps their line: loomux resolves it once at spawn and passes
it as `$4`, single-quoted so it arrives byte for byte. It also copies that
layer's `padding` and `refreshInterval`. With no status line configured,
nothing is printed, which is what a plain claude pane with none shows.

The chain runs as `( eval "$chain" )` rather than `sh -c "$4"`. A bare `sh`
is a PATH lookup, and on Windows a CLI's hook PATH can lack Git's `usr\bin`
(#335). The subshell confines a syntax error in the human's command, which
is fatal to the shell that `eval`s it.

**What each source owns** (`modelstate::enrich_with_statusline`). The
transcript reading is the base. While the snapshot's `session_id` is the
agent's current one, it supplies model, effort and window. Tokens stay the
transcript's: the docs define `total_input_tokens` with the same input-only
formula as `usage::latest_context_tokens`, and the compaction state
machine's token baselines were measured from the transcript. Snapshot tokens
fill in only when the transcript tail has no reading. A `current_usage` of
`null` (before the first API call, right after `/compact`) is no reading,
never a `0`, since a fabricated `0` looks like a compaction's token drop.
With no transcript there is no signal at all, so a snapshot can never seed
the boundary-count baseline the resolver compares against.

### Departures from the plan, approved on #993

1. **Freshness.** The plan said the snapshot is preferred "when its
   session_id matches and it is at least as fresh as the transcript read".
   An mtime comparison flaps. The status line re-runs on a new assistant
   message, but the transcript also grows on every tool result and user
   line, so mid-turn the transcript is routinely the newer file. The window
   would alternate between reported and table on successive ticks, and the
   escalation percent with it. Model, effort and window are session facts:
   they change on `/model` or `/effort`, and the status line re-runs on the
   next assistant message, the same event that writes the transcript's next
   model. So the session match alone gates them. Pinned by
   `statusline_snapshot_older_than_the_transcript_still_supplies_the_window`.
2. **Where the human's command is read.** The plan said "read once at spawn
   from `~/.claude/settings.json`". `--settings` also outranks both project
   layers, so a project-level `statusLine` would be replaced and never
   chained. loomux therefore resolves the effective one, first
   `statusLine` of `type: "command"` wins, across
   `<cwd>/.claude/settings.local.json`, `<cwd>/.claude/settings.json` and
   the user file. A registry that is not the human's live one reads a
   contained stand-in for the user file (#502).

### Residuals

- **Managed settings outrank `--settings`.** A managed `statusLine` replaces
  loomux's entry, the arm never runs, and no snapshot is written. The pane
  falls back to the transcript reader and the model table, which is the
  behaviour before S1.
- **`CLAUDE_CONFIG_DIR` is not honoured** for the user layer. The docs say
  it relocates the home-directory settings, and loomux's transcript root
  ignores it too. A human who sets it gets the project layers only.
- **A failing command still shows its output.** The docs say a non-zero exit
  blanks the status line, but the arm exits 0 by decision, so a command that
  prints and then fails shows its output in an orrerix pane.
- **The human's command runs under the hook's POSIX `sh`**: Git's `sh.exe`
  on Windows, `/bin/sh` elsewhere. The docs say only that the command "runs
  in a shell", which on Windows is Git Bash when installed. A command using
  a construct the POSIX `sh` lacks may behave differently from the same
  command in a plain claude pane.
- **One layer's fields.** loomux copies `padding` and `refreshInterval`
  from the winning layer only. The docs do not say whether Claude merges a
  nested `statusLine` object field by field across layers.
- **Live validation is the human's**: whether the chained line renders
  identically in a real pane (constraint 3 forbids spawning claude to check).

## Contract changes planned by later slices

The S0 and S1 rows have shipped; the rest are planned:

1. **S3 will** add `window_tokens`, `window_source`, `model`, `effort`, `source`,
   and `declared: {model, effort}` to `group_summary.agents[].context`.
2. **S1 adds** the raw Claude status-line payload at
   `<group>/hooks/<agent>.statusline.json`, written through a temporary file
   and a rename.
3. **S6 will** add optional `effort` to usage-series samples with a serde default.
4. **S1 adds** a `statusLine` entry to Claude's `--settings` configuration,
   chaining to the human's own status-line command (above).
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
