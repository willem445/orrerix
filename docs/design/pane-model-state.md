# Per-pane model and context state (#993)

This note records the planned sources and capability facts for pane context
state. S0 added capability metadata only. S1 adds the first reader: Claude's
status-line payload (see [S1](#s1-the-claude-status-line-source)). A source
is read from the CLI's artifact rather than inferred from a model name.

## Source matrix

| CLI | Live model | Live effort | Context window | Context used | Compact command loomux may send | Compaction-done signal |
|---|---|---|---|---|---|---|
| **claude** (PTY) | Status-line `model.id` / `model.display_name` (S1); transcript `message.model` already exists | Status-line `effort.level` (S1) | Status-line `context_window.context_window_size` (S1) | Status-line `context_window.total_input_tokens` / `used_percentage` (input-token accounting) (S1) | `/compact` (Claude Code [slash commands](https://code.claude.com/docs/en/commands)) | `PreCompact`, `SessionStart(compact)`, transcript `compact_boundary` already exist; `PostCompact` (S5; [hooks reference](https://code.claude.com/docs/en/hooks)) |
| **codex** (PTY) | Rollout `turn_context.payload.model` already exists | Rollout `turn_context.payload.effort` (S2a; optional `ReasoningEffort`) | Rollout token-count `info.model_context_window` (S2a) | Same event's `info.last_token_usage.input_tokens` (S2a) | `/compact` ("Summarize the visible chat to free tokens"; [CLI command reference](https://learn.chatgpt.com/docs/developer-commands?surface=cli)). `model_auto_compact_token_limit` documents automatic compaction ([config reference](https://learn.chatgpt.com/docs/config-file/config-reference)). | Rollout `compacted` (read, S2a/S5); `PreCompact` / `PostCompact` hooks are documented ([hooks](https://learn.chatgpt.com/docs/hooks)) but not installed — they run only once the human trusts them (S5) |
| **pi** (PTY) | Session's latest assistant `provider` / `model`; `model_change` entries | `thinking_level_change`; initial level is the launcher's `--thinking` choice | `--list-models` context column; RPC `get_state.model.contextWindow`; not present in session file (S8) | Latest assistant `usage.input + cacheRead + cacheWrite` | `/compact [prompt]` ([usage guide](https://github.com/earendil-works/pi/tree/v0.84.4/packages/coding-agent/docs/usage.md)) | Session `compaction` entry; RPC `compaction_end` |
| **opencode** (PTY) | Session `model` column, JSON `{id, providerID, variant}` read as `providerID/id` (S2c) | `variant` in that JSON (S2c) | None: not in the store, and no documented source in the [configuration reference](https://opencode.ai/docs/config/) (S2c) | Newest counted assistant `message.data` `tokens.input + cache.read + cache.write` (S2c) | `/compact` (alias `/summarize`; [TUI guide](https://opencode.ai/docs/tui/)) | No documented signal; token-drop inference only |
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
while no status-line report is available — for a Claude reading only, since S4
(see below) — and (4) an empirical clamp that
widens a window when observed usage exceeds it, tagged `clamped`. A reported
window is not replaced by a model-name guess. S1 ships the ladder
(`modelstate::context_window_ladder`) and the Claude status line as rung 2's
first source. The Codex token-count event (S2a) feeds the same rung, and so
does pi's model list (S8's data, read by the S2b pi arm). A window pi printed
rounded is labelled `reported-rounded` rather than `reported`
(`modelstate::label_rounded_report`); it is still rung 2. The opencode reader
(S2c) feeds no rung: its store records no window, so its signal carries none.

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
nothing is printed. That is **not** what a plain claude pane shows. The CLI
hides most of its footer keyboard hints whenever a `statusLine` is configured
("With a custom status line configured, Claude Code stops showing most of the
footer's keyboard hints"). loomux's entry has to exist for the window report,
so a human with no status line of their own loses those hints in an orrerix
pane (residuals, below).

The chain runs as `( eval "$chain" )` rather than `sh -c "$4"`. A bare `sh`
is a PATH lookup, and on Windows a CLI's hook PATH can lack Git's `usr\bin`
(#335). The explicit subshell confines a syntax error in the human's
command, which is fatal to the shell that `eval`s it. That matters only on
ksh and zsh, which run a pipeline's last stage in the current shell. dash
and bash already fork it, so no CI shell exercises the parentheses.

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

### Departures from the plan, approved through the orchestrator (recorded on #993)

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

- **With no status line of your own, the footer hints are hidden.** Claude
  Code stops showing most of the footer's keyboard hints (`esc to
  interrupt`, `? for shortcuts`, `hold space to speak`) whenever a
  `statusLine` is configured. loomux's entry must exist for the window
  report, so every orrerix claude pane whose human has none shows an empty
  status line and no hints, which is the largest group of users. This is a
  consequence of the human's decided default on #993, not a defect in the
  chain.
- **loomux's own screen readers were sized against captured chrome.**
  `IDLE_PROMPT_TAIL_ROWS` and `MENU_TOKEN_TAIL_ROWS` count the rows Claude
  Code paints below its composer. A status row, and the hint bar it hides,
  change that chrome for every orrerix claude pane. Nothing in loomux reads
  the hint text itself, and a human with their own status line already ran
  this layout. But the row arithmetic for the no-status-line case is
  unverified live (constraint 3).
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
interval.** The edge is the printed value minus half a printed tenth, clipped
to the suffix's own floor: `262.1K` becomes 262,050, and `1.0M` becomes
1,000,001. pi takes the `M` branch only at 1,000,000 or more and prints exactly
1,000,000 as `1M`, so `1.0M` always means more than a million; `K` is the same
against 1,000. Integer and raw spellings stay exact.

The reason for the lower edge is that the window feeds the compaction
threshold. An understated window compacts a little early, which is safe; an
overstated one lets the CLI's own emergency compaction fire first. Reading the
spelling at face value would overstate by up to that half-tenth (1,050,000
prints `1.1M`), except for `1.0K` and `1.0M`, where face value falls below
every count that prints them. `1.0M` is 225 of the catalog's 721 rounded
windows.

The edge is never above the real count, because `toFixed(1)` picks the tenth
nearest the count and the clip is pi's own branch condition. It is also tight
to within one token: the smallest count printing each rounded spelling is the
edge or the edge plus one. Face value ("nearest") is the alternative, and it
is a one-line change in `parse_token_count`; the human may choose it.

A consumer labels a window from an id in `model_context_windows_rounded`
`reported-rounded` rather than `reported` on the ladder's rung (2).

**The consumer is the S2b pi arm** ([below](#s2b-the-pi-session-reader)): it
looks the pane's current model up in the cached probe and fills
`window_tokens` from it.

## S2b: the pi session reader

A pi PTY pane's context signal comes from its session file, read by
`modelstate::pi_context_signal` over the file's bounded tail
(`usage::read_transcript_tail`, the same 256 KiB tail the Claude and Codex
readers use). The entry shapes are pi's `docs/session-format.md` at `v0.84.4`.
The file is read in append order, so each field holds its newest writer:

| Field | Source entry | Rule |
|---|---|---|
| tokens | the newest assistant `message` with a `usage` | `input + cacheRead + cacheWrite`: what that turn sent. `output` is excluded. A turn whose sum is zero is skipped, because pi writes an all-zero `usage` on an errored turn and a `0` reads as a compaction's token drop. |
| model | an assistant message's `provider`/`model`, or a `model_change`'s `provider`/`modelId` | Whichever is newer, spelled `provider/model`. A `/model` switch after the last turn is the pane's model before any turn has run on it. |
| effort | `thinking_level_change`'s `thinkingLevel` | The newest one. |
| marker count | `compaction` entries | How many there are. A `branch_summary` is not one. |
| window | none | The session file records none (next section). |

**The token figure is deliberately not pi's own gauge.** pi's
`calculateContextTokens` (`SOURCE` `src/core/compaction/compaction.ts:146-148`)
is `totalTokens || input + output + cacheRead + cacheWrite`. It counts the
turn's `output`, and its `getAssistantUsage` (`:154-167`) also skips `aborted`
and `error` turns. This reader uses the input-side formula instead, the same one
claude's `latest_context_tokens` uses, and skips only turns whose sum is zero.
That reads up to one turn's `output` below what pi compacts on. Nothing reads pi
tokens yet, so the choice is recorded here for S4 to settle on purpose before
it opens the nudge gate to pi.

**Where the file is.** The arm reads the group's own pi store
(`OrchRegistry::pi_sessions_dir`), which is where the usage meter's pi arm reads
too. Both launch forms pass that directory as `--session-dir` to every group pi
pane. The per-user store (`sessions::pi_sessions_root`) is not consulted, because
a group pane never writes there. The CLI is resolved per pane with
`Guardrails::cli_for_block` (#2167).

**The window.** The arm looks the model up in the cached `pi --list-models`
probe (S8) through `cliprobe::cached_context_window`. That call only reads the
cache and never spawns pi. The startup sweep fills the cache. Until the sweep
has answered, or for a model the listing does not carry, the window is `None`
rather than a guess. An id in `model_context_windows_rounded` sets
`CompactionSignal::window_rounded`, and the ladder then labels the window
`reported-rounded`.

**What reads it.** `run_compact_nudge` caches every signal's model, window and
rounded flag on the agent, and since S3 `group_summary` publishes the model and
observed effort. Since S4 the compact-nudge loop admits pi (its `CliCaps` row
carries `/compact`), so a pi reading's tokens reach the lifecycle panel and the
threshold escalation — against the `--list-models` window, or a group override.
A pi model the listing does not carry has no window and so no percent and no
escalation (see S4).

**The effort fallback, and a correction to the plan's premise.** The plan
said the file records no initial thinking level, so the initial effort would be
the `--thinking` value loomux passed. At `v0.84.4` that premise is false. For a
new session, pi's `createAgentSession` (`SOURCE` `src/core/sdk.ts:381-386`) appends a
`model_change` and a `thinking_level_change` carrying the effective level,
after pi has clamped it to what the model supports. So the file's own level
wins whenever the tail holds one. The pane's block effort knob is used only when
the tail holds none, for example when a long session's entry has scrolled out of
the tail. That knob is the value the launch line passed as `--thinking`,
already clamped by `Guardrails::clamped`. An empty knob is no level, not a
level.

### S2b residuals

- **A resumed session can name a stale level.** On a resume, pi appends a
  `thinking_level_change` only when the branch has none, so a `--thinking` that
  differs from the session's last recorded level is live but not written.
  The reader then reports the older level.
- **The fallback is the requested level, not the effective one.** Once the
  tail has lost every `thinking_level_change`, the knob is what loomux asked
  for, and pi may have clamped it for the model. It also does not follow a
  later edit of the block's effort.
- **The probe is cached for the app run.** If pi is upgraded mid-run and its
  catalog changes a window, the change is not seen until restart. This is the
  cache policy `probe_agent_cli` documents.
- **Append order, not the active path.** pi's file is a tree. An entry on a
  branch the leaf has navigated away from still counts as newest if it was
  appended last, which is the same file-wide reading `usage::PiFold` takes.
- **A tail with no assistant line.** A tool result larger than the tail can
  leave no assistant message in it. What happens next depends on the rest of
  the tail.
  - If nothing else in the tail is recognised, `pi_context_signal` returns
    `None` (`found.then_some(reading)`), so `pi_compaction_signal_in` yields no
    signal. `run_compact_nudge` then leaves the cached model and window as they
    were.
  - If the tail holds another recognised entry (a `compaction` or a
    `thinking_level_change`) and no `model_change`, the signal has neither
    tokens nor a model, and so no window. `run_compact_nudge` sets
    `last_context_window` from every signal, so the window goes back to
    `None`. It sets `last_context_model` only from `Some`, so the model is kept.
  - This is inert until S4. After that, the window can fall to the table rung
    on such a tick.
- **The marker count is over the tail**, as it is for the Claude and Codex
  readers, so a marker older than the tail is not counted.
- **A solo pi pane has no reading.** A solo pane writes to the human's own
  store, and that store is not read. This is the same residual as the usage
  meter's (`docs/design/pi.md`, Usage and cost).

## S2c: the opencode store reader

An opencode PTY pane's context signal comes from the group's own SQLite store
(`OrchRegistry::opencode_db_path`, where every group opencode pane's
`OPENCODE_DB` points). `modelstate::opencode_compaction_signal_in` reads it on
one read-only connection, through two readers in `opencodedb.rs`. The shapes
they rely on were verified at the `v1.18.11` pin and are recorded as labelled
observations in `docs/design/opencode.md` ("Model, variant and context").

| Field | Source | Rule |
|---|---|---|
| model | the session row's `model` column (`session_model_state`) | JSON `{id, providerID, variant}` is read as `providerID/id`, the `provider/model` spelling `--model` takes. A plain-string column is kept verbatim. An object with no `id` is no model. The column is never shown as JSON text. |
| effort | `variant` in the same JSON | An empty variant, or `"default"` (what opencode writes when a prompt chose none), is no effort level. A plain-string column has no variant. |
| tokens | the newest counted assistant message (`latest_assistant_context`) | `tokens.input + cache.read + cache.write`, which is what the call sent: opencode's `input` already excludes both cache counts. `output` and `reasoning` are excluded. |
| window | none | The store records none (below). |
| marker count | none | Always `0` (residuals). |

**Which message counts.** The reader walks the session's own messages newest
first, in the vendor's index order (`time_created`, then `id`), classifying
each with `opencodedb::message_context`:

- **Skipped:** anything that is not an assistant message, and an assistant
  message whose context sum is zero. opencode inserts each assistant message
  with zero counters and fills them at its first `step-finish`, so an
  in-flight turn reads zero, and a `0` handed to the compaction state machine
  looks like a compaction's token drop. A compaction that is still running is
  skipped the same way, because until its summary finishes the pre-compact turn
  below it is still what the context holds.
- **Stops with no reading:** a finished compaction summary (`summary: true`).
  Its call read the whole pre-compact history, so neither its figure nor any
  older turn's is what the context holds afterwards. There is no reading until
  the first post-compact turn finishes. That matches opencode's own overflow
  check, which takes the newest finished assistant message and, when it is a
  summary, makes no judgement rather than falling back to an older one.
- **Stops with a reading:** any other assistant message, whose
  `input + cache.read + cache.write` is the reading.

A session with no reading still has a signal carrying the model and effort.

**Why the session row, not the message, names the model.** `SessionPrompt`
rewrites the row whenever a prompt's model or variant changes, before the turn
runs. So the row names what the pane is on now, including a `/model` switch
whose turn has not finished. This is the same reading the pi reader takes of a
`model_change` entry.

**No window, so no percent.** The store records no window. opencode's
configuration docs say nothing about `limit.context`, `opencode models` prints
ids only, and `/config/providers` needs an `opencode serve` loomux does not
run. The signal's `window_tokens` is therefore always `None`, and opencode is
tokens-visible without a percent (Tier 1 for tokens only, per the tiers above).
**S4 holds that line.** `effective_context_window_tokens` falls to the
model-name table on a `None` window: 200K, or 1M for an id containing `opus`.
S4 opens the compact-nudge gate to opencode, and the table would then give an
opencode reading a guessed percent; `agent_context_percents` refuses it
through `modelstate::published_window`, so a tokens-only reading never
escalates (S4 below).

**What reads it.** `run_compact_nudge` caches the signal's model, window and
rounded flag on the agent, and since S3 its effort and source too, which
`group_summary` publishes under `source: "opencode-db"`. Since S4 the
compact-nudge and idle-compact loops admit opencode, so its token count reaches
the lifecycle panel — without a percent unless the group sets an override.

**One visible change: the usage model label.** `opencodedb::session_usage_on`
now decodes the column the same way, so an opencode pane's usage `model` reads
`opencode/deepseek-v4-flash` rather than the column's JSON text. Four readers
show it: the token chart's legend and its model-switch marks
(`docs/design/token-charts.md`), the lifecycle panel's usage-source tooltip
(`src/groupview.ts`), and the per-model token averages
(`src/tokenaverages.ts`). Samples recorded before the upgrade keep the JSON
text, so a session that straddles it gets one extra model mark and a separate
averages bucket under that text.

### S2c residuals

- **The token figure is not opencode's own gauge.** opencode's overflow check
  counts `tokens.total`, or `input + output + cache.read + cache.write` when
  `total` is absent. This reader uses the input-side formula instead, like the
  claude and pi readers, so it reads up to one turn's output below what
  opencode compacts on. S4 settles this for opencode and pi together.
- **No compaction marker is counted.** A compaction's own assistant message
  (`summary: true`, `mode: "compaction"`) is a structural record of one, but
  opencode documents no compaction-done signal, and the plan's table says
  token-drop inference only. The marker count stays `0`, and whether that
  message should count is left to S4.
- **The scan is bounded** at `opencodedb::CONTEXT_SCAN_ROWS` (64) messages.
  A turn is one message, since tool calls are parts, so the newest counted
  message is normally within a few rows. A counted message with 64 or more
  newer messages above it is not found, and the reading is `None` for that
  tick. The bound is pinned by
  `context_the_scan_reaches_back_exactly_the_documented_bound_and_no_further`.
- **A subagent's context is not the pane's.** A subagent is a session of its
  own, so only the pane's own session is read. Usage is different: it rolls
  subagent spend up into the pane.
- **A store that cannot be read yields no signal.** The usage path already
  audits each degrade once per episode (`note_opencode_db_degrade`), so this
  reader does not audit a second time.
- **A solo opencode pane has no reading.** A solo launch sets no environment,
  so it is never pointed at a group store, and the human's own store is not
  read.

## S3 — publish per-pane identity and context

S3 adds `window_tokens`, `window_source`, `model`, `effort`, `source`, and
`declared: {model, effort}` to `group_summary.agents[].context`. The lifecycle
panel shows a compact model/effort/context label. It never prints a percentage
without both observed tokens and a known window; without a window it shows the
token count, and without a reading it labels the roster pick `(declared)`. The
Agents list chip is deferred: its `PaneFacts` projection does not carry the
lifecycle summary, and adding it requires separate pane-state plumbing.
By orchestrator direction, the Copilot effort rung marked `(settings)` is
deferred to S7 because it requires reading `~/.copilot/settings.json`; that read
belongs with S7's Copilot census, which is held for the human.

The integration test `statusline_window_reverts_to_the_table_when_its_snapshot_disappears`
pins that a reported window is discarded when its snapshot disappears, and the
model-table rung supplies the fallback window.

**A launch fallback is not a reading.** When pi's tail names no thinking level,
the signal's effort is the block's `--thinking` knob
(`CompactionSignal::effort_is_launch_fallback`). S3 publishes it once, as
`declared.effort`, and leaves `effort` empty. That is the rule S6's samples
follow, and both read it from one place, `CompactionSignal::observed_effort`.
Publishing the knob as `effort` would show configuration as the pane's live
level, and the S2b residuals say how the two can differ (pi clamps the level,
and the knob does not follow a later edit).

**A window only the table decided is not published for opencode.** The window
ladder's table rung is Claude's model-name table. For a source that reports no
window and that the table does not describe, a table answer is a guess, and so
is a clamp over it. `ContextSource::table_rung_applies` says which sources may
use the table, and the match is exhaustive so a new reader has to decide. S3
made it `false` for `OpencodeDb` alone; S4 narrowed it to Claude's own readers
(below). `modelstate::published_window` then publishes `window_tokens`,
`window_source` and `percent` as null for such a reading, and the panel shows
the token count alone. An override still publishes.

## S4 — compaction across every CLI (#413)

**The gate is the row.** `compact_command_for(cli)` returns the CLI's
`CliCaps.compact_command`, and that is the whole admission test for the
compact-nudge loop, `request_compact`, the human's "Compact now" and the
orchestrator's idle-compact backstop. The paste sends that string through
`deliver_prompt`, so a CLI whose command is spelled differently gets its own
spelling. Every row that carries a command spells it `/compact`, so a test
reading the rows cannot tell the row's string from a hard-coded one; the pin
feeds a spelling no row has through a test-only seam
(`set_compact_command_for_test`) and asserts that exact string reaches
`deliver_prompt`. A CLI with no command (gemini today) is skipped, and
`request_compact` refuses with the row's `compact_note`. It replaced
`compact_nudge_cli_supported`, a `claude | copilot` match that kept codex, pi
and opencode out of the loop even after S2a–S2c gave them readings.

**One rule decides whether a reading has a percent, for the panel and the
escalation alike.** Opening the loop to three more CLIs also opened the
escalation to them, and the escalation computed its percent against the
ladder's answer — including the table rung, which for a non-Claude model is
Claude's default 200K. So `agent_context_percents` now computes a percent only
against a window `modelstate::published_window` accepts, the function
`group_summary` already published by, and `ContextSource::table_rung_applies`
is `true` only for Claude's two readers (transcript and status line). The
effect, per reader:

| Reading | Window | Percent and escalation |
| --- | --- | --- |
| Claude, no status-line report yet | model table | yes — unchanged |
| codex or pi with a CLI-reported window | reported | yes |
| codex or pi with no reported window | none | no, unless the group sets an override |
| opencode | none, ever | no, unless the group sets an override |
| any CLI under a group override | override | yes |

This is a visible change from S3 for codex and pi: a pane whose CLI has not
reported a window used to show a percent of the table's 200K and now shows its
token count alone. The table describes Claude ids; a percent over a guessed
window would escalate — type a request into a live pane — on a number nobody
measured, and the panel showing a percent the escalation refused would be the
two readers disagreeing about one pane. No percent also means the lull floor
fails CLOSED for the pane (`context_window_unknown`; the human's decision on
review round 1) and the idle-compact
backstop fails closed, both as for a pane with no reading.

**The timeline says why.** A reading with tokens and no accepted window, in a
seat that would otherwise escalate — threshold on, role in
`compact_nudge_roles`, a compact command — is audited as
`compact-escalation-skipped` once per episode (`note_unwindowed_escalations`).
The latch, `compact_unwindowed_noted`, is rebuilt from each tick's readings, so
a pane that gains a window leaves it and a later tokens-only stretch is audited
again.

**The codex rollout is remembered.** The codex arm used to call
`find_codex_session_file` on every tick, and that walks every rollout under the
store's date tree — it must, for "newest" to mean anything — on a store codex
never prunes (#3531). `codex_rollout_path` keeps the resolved path per
`(root, session)` and stats it once per tick. It walks again when the file has
gone, and every `CODEX_ROLLOUT_REVALIDATE_AFTER` (five minutes) regardless,
because a stat cannot tell "the file still exists" from "this is still the
session's file": `thread/revert` starts a new rollout and leaves the old one
readable. Five minutes is the usage cursor's interval for the same
re-resolution, so the two readers of one rollout drift apart for no longer than
each other.

### S4 residuals

- **A revert is seen up to five minutes late.** Between a `thread/revert` and
  the next re-walk, the context reading comes from the superseded rollout —
  bounded by the timer above, and pinned as the observable proof the memo
  works (`the_codex_context_read_does_not_walk_the_store_again_on_the_next_tick`).
- **A session with no rollout yet is walked every tick.** Absence is not
  remembered, so the first reading of a new pane is not delayed by an interval;
  the cost is the pre-S4 walk, for exactly the panes with no reading to lose.
- **Manual-compact detection still matches `/compact`.** The detector that
  notices a human typing the command reads the literal `/compact`. Every row
  that carries a command spells it that way today; a row that ever spells it
  differently needs the detector to read the row too.

## S5 — trusted compaction-done signals (#413)

- **claude:** `compact_hook_settings` adds a `PostCompact` hook; its script arm
  touches `<group>/hooks/<agent>.postcompact.json` and drains stdin. A fresh
  marker settles for `POSTCOMPACT_SETTLE_MS`, then decides loomux's own
  reinjection, unless a `SessionStart(compact)` marker for the same compaction
  resolved it natively first. The argument, the pairing rule and the settle
  window's clock are in `docs/design/orchestration.md`, "#413 S5: the
  compaction-DONE signal, per CLI".
- **codex:** no hook. The rollout's `compacted` count (S2a) is the signal.
  codex runs a non-managed hook only once the human has trusted it, per hook
  source file, so a profile hook would need orrerix to write the trust record
  itself. The analysis and the open human decision are in `docs/design/codex.md`,
  §Compaction-done signal.
- **pi:** the session file's `compaction` count (S2b) is the signal.
- **codex and pi, one rule:** the count already feeds `compact_boundary_count`,
  so `compact_nudge_tick` treats a rise exactly as it treats a claude
  `compact_boundary`. S5 pins that end to end through `run_compact_nudge`
  (`compaction_done_*` in `tests/piusage.rs` and `tests/codexusage.rs`) and
  changes no code for it.

### S5 residuals

- **A codex or pi self-compaction that nothing armed gets no re-grounding.** A
  count rise confirms an open arm; it never opens one, and neither CLI has a
  banner or hook orrerix arms on.
- **A claude arm can resolve one fast-poll tick later than before.** While a
  `PostCompact` marker settles, the busy-then-quiet resolver is held for that
  agent, so an arm it would have resolved on the first quiet tick waits for the
  next one.

## Contract changes

S0, S1, S3, S4, S5, S6 and S8 have shipped. These rows record each contract's owning
slice:

1. **S3 added** `window_tokens`, `window_source`, `model`, `effort`, `source`,
   and `declared: {model, effort}` to `group_summary.agents[].context`.
2. **S1 added** the raw Claude status-line payload at
   `<group>/hooks/<agent>.statusline.json`, written through a temporary file
   and a rename.
3. **S6 added** optional `effort` to usage-series samples with a serde default.
4. **S1 added** a `statusLine` entry to Claude's `--settings` configuration,
   chaining to the human's own status-line command (above).
5. **S0 added** `compact_command`, `self_compacts`, `compact_note`, and
   `context_reader` to `CliCaps`. S4 reads `compact_command` and
   `compact_note`; `self_compacts` and `context_reader` are still unread.
6. **S4 added** the `compact-escalation-skipped` audit action (`agent`,
   `reason`, `tokens`, `source`) and a `command` key on `compact-nudge`
   audit rows. Both are additive.
7. **S5 added** a `PostCompact` hook to Claude's `--settings` configuration, the
   `<group>/hooks/<agent>.postcompact.json` marker (existence-only), the
   `compact-resolved-postcompact` audit action (`agent`, `reason`,
   `settle_ms`), and `"postcompact"` as a value of `compact-hook-evidence`'s
   `event`. All additive.

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
  [configuration](https://opencode.ai/docs/config/). The message-token shape
  and the session-model JSON are not in these published docs. They are S2c
  observations read from the source at `v1.18.11`, recorded in
  `docs/design/opencode.md`.
