//! Per-pane model / effort / context-window readings (#993).
//!
//! Pure parsers and decisions only — no registry, no disk, no Tauri — so each
//! is testable inline without linking the app (constraint 4 applies to tests
//! that link the lib; these link nothing but `serde_json`). The impure halves
//! (finding the files, reading them, caching the result on `AgentEntry`) live
//! in `orchestration/mod.rs` beside the compact-nudge tick that consumes them.
//!
//! **Read the fact out of the CLI's artifact, never guess it** (the
//! `agent-cli-reference` rule). S1 adds the Claude Code status-line payload as
//! that artifact for claude panes: the CLI hands every `statusLine` command a
//! JSON document carrying the live model, the reasoning effort and — the one
//! thing no transcript records — the context-window SIZE. Design and the
//! decisions behind every rule below: `docs/design/pane-model-state.md`.

use serde_json::Value;

/// The fields loomux reads from one Claude Code status-line payload
/// (https://code.claude.com/docs/en/statusline, "Available data"). Every field
/// is optional because the docs make most of them conditional: `effort` "appears
/// only when the current model supports the reasoning effort parameter", and
/// `context_window.current_usage` is "`null` before the first API call in a
/// session, and again after `/compact` until the next API call repopulates it".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatuslineSnapshot {
    /// `session_id` — what ties a snapshot to ONE claude session. A snapshot
    /// whose id is not the agent's current session is someone else's reading
    /// (a pre-resume session's file left behind) and must never be used.
    pub session_id: Option<String>,
    /// `model.id`, e.g. `claude-opus-5-5`.
    pub model_id: Option<String>,
    /// `model.display_name`, e.g. `Opus`.
    pub model_name: Option<String>,
    /// `effort.level` (`low`, `medium`, `high`, `xhigh`, `max`).
    pub effort: Option<String>,
    /// `context_window.context_window_size` — "200000 by default, or 1000000
    /// for models with extended context". The reason this source exists.
    pub window_tokens: Option<u64>,
    /// `context_window.total_input_tokens` — but ONLY while `current_usage` is
    /// a real object. The docs say the totals are `0` before the first API
    /// response and `current_usage` is `null` then and right after `/compact`;
    /// a `0` read in that state is "no reading", not "the context is empty",
    /// and handing the compaction state machine a fabricated `0` would look
    /// exactly like a successful compaction's token drop.
    pub input_tokens: Option<u64>,
    /// `context_window.used_percentage` — "may be `null` early in the session".
    /// Carried for display; loomux computes its own percent against the window
    /// its ladder chose, so an override still wins.
    pub used_percentage: Option<f64>,
}

/// Parse one status-line payload. `None` when the text is not a JSON object at
/// all (a torn or empty file) — every individual field is otherwise optional,
/// so a payload missing the context block still yields its model.
pub fn parse_statusline_snapshot(text: &str) -> Option<StatuslineSnapshot> {
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    if !v.is_object() {
        return None;
    }
    let s = |val: Option<&Value>| val.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let cw = v.get("context_window");
    let usage_live = cw.and_then(|c| c.get("current_usage")).is_some_and(Value::is_object);
    Some(StatuslineSnapshot {
        session_id: s(v.get("session_id")),
        model_id: s(v.get("model").and_then(|m| m.get("id"))),
        model_name: s(v.get("model").and_then(|m| m.get("display_name"))),
        effort: s(v.get("effort").and_then(|e| e.get("level"))),
        // A zero window is not a window: `context_percent_used` would divide
        // by it (it guards, but reports 0%, which reads as "empty").
        window_tokens: cw.and_then(|c| c.get("context_window_size")).and_then(Value::as_u64).filter(|&w| w > 0),
        input_tokens: if usage_live {
            cw.and_then(|c| c.get("total_input_tokens")).and_then(Value::as_u64)
        } else {
            None
        },
        used_percentage: cw.and_then(|c| c.get("used_percentage")).and_then(Value::as_f64),
    })
}

/// Fold a status-line snapshot into a transcript reading for the same agent
/// (#993 S1). The transcript reading is the BASE — a claude pane with no
/// transcript yields no signal at all, snapshot or not — and the snapshot
/// only enriches it, and only when its `session_id` is the agent's current one.
///
/// **What each source owns, and why it is not "whichever file is newer".**
/// The plan said to prefer the snapshot when it is at least as fresh as the
/// transcript. That comparison flaps: Claude re-runs the status line on a new
/// assistant message, while the transcript also grows on every tool result
/// and user line, so mid-turn the transcript is nearly always the newer file
/// and the window would alternate between the reported figure and the table
/// guess tick by tick — and the escalation percent with it. So instead
/// (a departure approved through the orchestrator, recorded on #993):
///
/// - **model, effort, window** come from the snapshot whenever the session
///   matches. They are session facts: they change on `/model` or `/effort`,
///   and the status line re-runs on the very next assistant message, the same
///   event that writes the transcript's next model.
/// - **tokens** stay the transcript's. The docs define the status line's
///   `total_input_tokens` with the same input-only formula as
///   `usage::latest_context_tokens`, and the compaction state machine's token
///   baselines were measured from the transcript, so switching a baseline's
///   source mid-arm would compare two instruments. The snapshot fills tokens
///   in only when the transcript has no reading in its tail window.
/// - **`compact_boundary_count`** is always the transcript's — it is the
///   transcript's own structural marker, and a snapshot has no equivalent.
pub fn enrich_with_statusline(
    mut signal: crate::usage::CompactionSignal,
    snapshot: Option<&StatuslineSnapshot>,
    session_id: &str,
) -> crate::usage::CompactionSignal {
    let Some(snap) = snapshot.filter(|s| s.session_id.as_deref() == Some(session_id)) else {
        return signal;
    };
    if signal.tokens.is_none() {
        signal.tokens = snap.input_tokens;
    }
    if let Some(model) = &snap.model_id {
        signal.model = Some(model.clone());
    }
    signal.window_tokens = snap.window_tokens;
    signal.window_rounded = false;
    signal.effort = snap.effort.clone();
    signal.source = ContextSource::Statusline;
    signal
}

/// Where a context reading's model / window came from — published by S3 as
/// `context.source`, and the reason a reader can tell a reported figure from
/// a guessed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextSource {
    /// The CLI's transcript alone (the pre-#993 reader).
    Transcript,
    /// The transcript, enriched by a status-line snapshot for the same session.
    Statusline,
    /// Codex rollout token-count and turn-context records.
    CodexRollout,
    /// pi session-file entries (#993 S2b), with the window looked up in the
    /// cached `--list-models` probe.
    PiSession,
    /// opencode's SQLite store (#993 S2c): the session row's `model` column
    /// and the newest counted assistant message. No window.
    OpencodeDb,
}

impl ContextSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextSource::Transcript => "transcript",
            ContextSource::Statusline => "statusline",
            ContextSource::CodexRollout => "codex-rollout",
            ContextSource::PiSession => "pi-session",
            ContextSource::OpencodeDb => "opencode-db",
        }
    }

    /// Whether a window this source did NOT report may be filled from the
    /// ladder's model-name table (rung 3) — and so from the clamp over it. One
    /// rule for both readers of a window (#993 S3, #413 S4): what
    /// `group_summary` publishes as a percent, and whether
    /// `compact_nudge_tick` may escalate on it.
    ///
    /// Only Claude's own readers. The table is Claude's
    /// (`usage::claude_context_window_tokens`: a model-name match over Claude
    /// ids, 200K for everything else), so for any other CLI its answer is not
    /// a conservative estimate but a guess about a model it does not describe
    /// — and a guessed percent that ESCALATES types a request into a live pane.
    /// A codex, pi or opencode reading with no reported window therefore shows
    /// tokens without a percent and never escalates, until the CLI reports a
    /// window (codex's `model_context_window`, pi's `--list-models` column) or
    /// a human sets the group override. Exhaustive on purpose: a new reader
    /// has to decide this rather than inherit the table.
    pub fn table_rung_applies(self) -> bool {
        match self {
            ContextSource::Transcript | ContextSource::Statusline => true,
            ContextSource::CodexRollout | ContextSource::PiSession | ContextSource::OpencodeDb => false,
        }
    }
}

/// Which rung of the window ladder decided a context window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowSource {
    /// The group's `context_window_tokens_override` guardrail — a human said so.
    Override,
    /// The CLI reported it (Claude's status-line `context_window_size`).
    Reported,
    /// The CLI reported it ROUNDED, so the window is a lower bound rather than
    /// the exact count: pi's `--list-models` printed `262.1K` or `1.0M`, and
    /// `cliprobe::parse_token_count` read the bottom of that rounding interval
    /// (#993 S8). Rung 2 all the same; only the label differs, so nobody reads
    /// a lower bound as an exact report.
    ReportedRounded,
    /// `usage::claude_context_window_tokens`'s conservative model-name table —
    /// reachable only before the first status-line write for the session.
    Table,
    /// The observed tokens exceeded every other rung's answer, so the window
    /// was widened to the tokens themselves. See [`context_window_ladder`].
    Clamped,
}

impl WindowSource {
    pub fn as_str(self) -> &'static str {
        match self {
            WindowSource::Override => "override",
            WindowSource::Reported => "reported",
            WindowSource::ReportedRounded => "reported-rounded",
            WindowSource::Table => "table",
            WindowSource::Clamped => "clamped",
        }
    }
}

/// The context-window size a percent is computed against, and the rung that
/// decided it (#993, the plan's four-rung ladder):
///
/// 1. the group **override** wins outright — the escape hatch for a report that
///    is wrong for a deployment, and the only rung a human sets;
/// 2. a window the CLI **reported** beats any name-based guess;
/// 3. the existing conservative **table** (`claude_context_window_tokens`),
///    which under-estimates on purpose — a higher percent nudges compaction
///    sooner, the safe direction when unsure;
/// 4. an empirical **clamp**: a context cannot hold more tokens than its
///    window, so a reading above the chosen window proves that window wrong and
///    it widens to the tokens. It is tagged `clamped` so nobody mistakes it for
///    a report.
///
/// The clamp never applies to an override. A human who set 200K on a pane that
/// reads 250K has a deployment the override exists to describe, and silently
/// overruling them would make the escape hatch conditional; the percent still
/// saturates at 100 (`context_percent_used` clamps), which escalates — the
/// direction that override chose.
pub fn context_window_ladder(
    override_tokens: Option<u64>,
    reported_tokens: Option<u64>,
    table_tokens: u64,
    observed_tokens: Option<u64>,
) -> (u64, WindowSource) {
    if let Some(w) = override_tokens {
        return (w, WindowSource::Override);
    }
    let (window, source) = match reported_tokens.filter(|&w| w > 0) {
        Some(w) => (w, WindowSource::Reported),
        None => (table_tokens, WindowSource::Table),
    };
    match observed_tokens {
        Some(t) if t > window => (t, WindowSource::Clamped),
        _ => (window, source),
    }
}

/// Relabel a ladder answer whose rung-2 report was ROUNDED (#993 S2b, the
/// label S8 asked for). Only a `Reported` answer changes: an override still
/// outranks the report, and a clamp has already proved the report too small,
/// so neither of those is describing the rounded figure any more.
pub fn label_rounded_report((window, source): (u64, WindowSource), reported_rounded: bool) -> (u64, WindowSource) {
    match source {
        WindowSource::Reported if reported_rounded => (window, WindowSource::ReportedRounded),
        _ => (window, source),
    }
}

/// The window a percent may be computed against for one reading (#993 S3,
/// #413 S4) — the one answer `group_summary` publishes AND
/// `agent_context_percents` escalates on: the ladder's answer, unless nothing
/// but a guess decided it for a `source` whose table rung does not apply
/// ([`ContextSource::table_rung_applies`]) — then `None`, the panel shows
/// tokens without a percent, and the reading never escalates. A guess is the `table` rung,
/// or a `clamped` one with no report under it (the clamp then widened the
/// table's answer, not the CLI's). An override still stands: a human set it.
/// An unknown source (no reading cached yet) keeps the ladder's answer, which
/// is what every pane published before S3.
pub fn published_window(
    ladder: (u64, WindowSource),
    source: Option<ContextSource>,
    reported_tokens: Option<u64>,
) -> Option<(u64, WindowSource)> {
    let guessed = match ladder.1 {
        WindowSource::Table => true,
        WindowSource::Clamped => reported_tokens.filter(|&w| w > 0).is_none(),
        WindowSource::Override | WindowSource::Reported | WindowSource::ReportedRounded => false,
    };
    if guessed && source.is_some_and(|s| !s.table_rung_applies()) {
        None
    } else {
        Some(ladder)
    }
}

/// The human's own status line, as read from their Claude settings at spawn —
/// what loomux's status-line command chains to so an orrerix pane shows the
/// human's own line (#993, the human's decided default). A pane whose human
/// has none is NOT identical to a plain claude pane: see
/// [`resolve_user_statusline`].
#[derive(Clone, Debug, PartialEq)]
pub struct UserStatusLine {
    /// `statusLine.command`, verbatim — a shell command line.
    pub command: String,
    /// `statusLine.padding`, carried onto loomux's entry so the line renders
    /// at the same indent.
    pub padding: Option<u64>,
    /// `statusLine.refreshInterval`, carried for the same reason: a clock in the
    /// human's line keeps ticking in an orrerix pane.
    pub refresh_interval: Option<u64>,
}

/// The first `statusLine` of `type: "command"` with a non-empty `command`
/// across `layers` — settings-file CONTENTS in precedence order, highest first.
/// A layer that is unreadable JSON, or has no such entry, is skipped rather
/// than ending the search: Claude Code merges settings per key, so a layer
/// without a `statusLine` does not hide a lower one's.
///
/// A `type` other than `"command"` is skipped too (the docs define no other
/// type; a future one is not something `sh -c` can run). `None` when no layer
/// has one — loomux's status line then prints nothing. That is NOT what a plain
/// claude pane with no status line shows: the CLI hides most of its footer
/// keyboard hints (`esc to interrupt`, `? for shortcuts`) whenever a
/// `statusLine` is configured, and loomux's entry must exist for the window
/// report. A disclosed residual, `docs/design/pane-model-state.md` §S1.
pub fn resolve_user_statusline(layers: &[&str]) -> Option<UserStatusLine> {
    layers.iter().find_map(|text| {
        let v: Value = serde_json::from_str(text).ok()?;
        let sl = v.get("statusLine")?;
        if sl.get("type").and_then(Value::as_str) != Some("command") {
            return None;
        }
        let command = sl.get("command").and_then(Value::as_str)?.trim();
        if command.is_empty() {
            return None;
        }
        Some(UserStatusLine {
            command: command.to_string(),
            padding: sl.get("padding").and_then(Value::as_u64),
            refresh_interval: sl.get("refreshInterval").and_then(Value::as_u64),
        })
    })
}

/// Quote `s` as ONE POSIX-shell word: wrapped in single quotes, each embedded
/// `'` spelled `'\''`. Inside single quotes a POSIX shell interprets nothing —
/// no `$`, no backtick, no backslash — so this is the one quoting under which an
/// arbitrary command line survives the trip through Claude Code's shell intact
/// and arrives at the hook script as `$4`, byte for byte.
pub fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Append the human's status-line command to loomux's `statusline` hook
/// command as its fourth argv (`$4`), or return the base unchanged when there
/// is nothing to chain.
pub fn with_chained_command(base: &str, chain: Option<&str>) -> String {
    match chain {
        Some(c) if !c.trim().is_empty() => format!("{base} {}", sh_single_quote(c)),
        _ => base.to_string(),
    }
}

/// The latest context facts available in a Codex rollout.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CodexContextReading {
    pub tokens: Option<u64>,
    pub window_tokens: Option<u64>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub compaction_markers: u64,
}

/// Read the newest token-count and turn-context records from a Codex rollout.
///
/// `event_msg` / `token_count` records are scanned independently from
/// `turn_context`: either may be absent, and the newest useful occurrence wins.
pub fn codex_context_signal(text: &str) -> Option<CodexContextReading> {
    let mut reading = CodexContextReading::default();
    let mut found = false;

    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("compacted") => {
                reading.compaction_markers += 1;
                found = true;
            }
            Some("event_msg")
                if value.pointer("/payload/type").and_then(Value::as_str)
                    == Some("token_count") =>
            {
                let Some(info) = value.pointer("/payload/info").filter(|info| info.is_object()) else {
                    continue;
                };
                found = true;
                reading.tokens = info
                    .pointer("/last_token_usage/input_tokens")
                    .and_then(Value::as_u64);
                reading.window_tokens = info.get("model_context_window").and_then(Value::as_u64);
            }
            Some("turn_context") => {
                let payload = value.get("payload");
                reading.model = payload
                    .and_then(|payload| payload.get("model"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                reading.effort = payload
                    .and_then(|payload| payload.get("effort"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                found |= reading.model.is_some() || reading.effort.is_some();
            }
            _ => {}
        }
    }

    found.then_some(reading)
}

/// Resolve and read one Codex rollout's bounded tail, then map it to the
/// shared compaction signal. Compressed winning rollouts are rejected by the
/// store lookup before this reader is called.
#[doc(hidden)] // pub for the `codexusage` integration test
pub fn codex_compaction_signal_in(
    root: &std::path::Path,
    session: &loomux_engine::pathseg::PathSegment,
) -> Option<crate::usage::CompactionSignal> {
    codex_compaction_signal_at(&loomux_engine::sessions::find_codex_session_file(root, session)?)
}

/// [`codex_compaction_signal_in`] for a rollout already resolved — the half
/// `OrchRegistry::agent_context_signals` calls with a REMEMBERED path, so a
/// tick does not walk the whole store to find a file it found last tick
/// (#3531, #413 S4; see [`reuse_remembered_rollout`]).
pub fn codex_compaction_signal_at(path: &std::path::Path) -> Option<crate::usage::CompactionSignal> {
    let text = crate::usage::read_transcript_tail(path)?;
    let reading = codex_context_signal(&text)?;
    Some(crate::usage::CompactionSignal {
        tokens: reading.tokens,
        compact_boundary_count: reading.compaction_markers,
        model: reading.model,
        window_tokens: reading.window_tokens,
        window_rounded: false,
        effort: reading.effort,
        effort_is_launch_fallback: false,
        source: ContextSource::CodexRollout,
    })
}

/// How long a remembered Codex rollout path stands in for a store walk before
/// the store is walked again (#3531, #413 S4).
///
/// `find_codex_session_file` visits every rollout under the `YYYY/MM/DD` tree —
/// it must, for "newest" to mean anything — and codex compresses old rollouts
/// but never deletes them, so on a years-old store that walk ran once per codex
/// pane per compact-nudge tick. The memo keeps one stat per tick instead.
///
/// The stat alone cannot tell "the file still exists" from "this is still the
/// session's file": `thread/revert` keeps the thread id, starts a NEW rollout
/// and leaves the old one readable, so a memo validated only by `is_file()`
/// would read the superseded rollout for the life of the process. This timer
/// bounds that: a revert is seen within one interval. Five minutes, the value
/// the usage cursor's identical re-resolution uses (`CURSOR_REVALIDATE_AFTER`
/// in `usage.rs`) for the identical reason, so the two readers of one rollout
/// never disagree for longer than each other.
pub const CODEX_ROLLOUT_REVALIDATE_AFTER: std::time::Duration = std::time::Duration::from_secs(300);

/// A Codex rollout path the store walk resolved for one session, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RememberedRollout {
    pub path: std::path::PathBuf,
    pub resolved: std::time::Instant,
}

/// Whether `remembered` may be read at `now` instead of walking the store: it
/// was resolved less than `revalidate_after` ago ([`CODEX_ROLLOUT_REVALIDATE_AFTER`])
/// AND `still_a_file` — the caller's one stat of it — says it has not gone. A
/// rollout codex compressed is gone under its plain name, so the walk runs
/// again and answers `None` for it, as it always did.
pub fn reuse_remembered_rollout(
    remembered: &RememberedRollout,
    now: std::time::Instant,
    revalidate_after: std::time::Duration,
    still_a_file: bool,
) -> bool {
    still_a_file && now.saturating_duration_since(remembered.resolved) < revalidate_after
}

/// The latest context facts in a pi session file (#993 S2b). No window: the
/// session file records none, so [`pi_compaction_signal_in`] looks it up in
/// the cached `--list-models` probe instead.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PiContextReading {
    /// The newest assistant turn's `usage.input + cacheRead + cacheWrite`.
    pub tokens: Option<u64>,
    /// `provider/model`, the spelling `usage::PiFold` and pi's own `--model`
    /// use, and the key `CliProbe::model_context_windows` is filed under.
    pub model: Option<String>,
    /// The newest `thinking_level_change`'s `thinkingLevel`.
    pub effort: Option<String>,
    /// How many `compaction` entries the text holds.
    pub compaction_markers: u64,
}

/// Read a pi session file's text (the entry shapes in pi's
/// `packages/coding-agent/docs/session-format.md` at `v0.84.4`) in FILE order,
/// so each field ends up holding its newest writer:
///
/// - **tokens** — the newest assistant `message` whose `usage` sums to more
///   than zero: `input + cacheRead + cacheWrite`, what that turn SENT, the
///   same input-side formula as claude's `latest_context_tokens`. `output` is
///   what the turn produced, not what was in context. A zero sum is skipped
///   rather than read: pi writes an all-zero `usage` on an errored turn (see
///   `usage::PiFold::push`), and a `0` handed to the compaction state machine
///   looks exactly like a compaction's token drop.
/// - **model** — the newest of an assistant message's `provider`/`model` and a
///   `model_change` entry's `provider`/`modelId`, so a `/model` switch after
///   the last turn is the pane's model before any turn has run on it.
/// - **effort** — the newest `thinking_level_change`.
/// - **compaction markers** — the count of `compaction` entries.
///
/// File order is the order pi appended in; the file is a tree, and an entry on
/// a branch the leaf navigated away from is still read — the same "over the
/// file, not the active path" reading `PiFold` takes. `None` when the text
/// holds none of these, e.g. a file that is only its session header.
pub fn pi_context_signal(text: &str) -> Option<PiContextReading> {
    let mut reading = PiContextReading::default();
    let mut found = false;
    let s = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);

    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("message") => {
                let Some(msg) = value.get("message") else { continue };
                if msg.get("role").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                if let (Some(provider), Some(model)) = (s(msg, "provider"), s(msg, "model")) {
                    reading.model = Some(format!("{provider}/{model}"));
                    found = true;
                }
                if let Some(usage) = msg.get("usage") {
                    let field = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
                    let context = field("input").saturating_add(field("cacheRead")).saturating_add(field("cacheWrite"));
                    if context > 0 {
                        reading.tokens = Some(context);
                        found = true;
                    }
                }
            }
            Some("model_change") => {
                if let (Some(provider), Some(model)) = (s(&value, "provider"), s(&value, "modelId")) {
                    reading.model = Some(format!("{provider}/{model}"));
                    found = true;
                }
            }
            Some("thinking_level_change") => {
                if let Some(level) = s(&value, "thinkingLevel") {
                    reading.effort = Some(level);
                    found = true;
                }
            }
            Some("compaction") => {
                reading.compaction_markers += 1;
                found = true;
            }
            _ => {}
        }
    }

    found.then_some(reading)
}

/// A context window a CLI reported for a model, and whether it printed it
/// rounded (`CliProbe::model_context_windows_rounded`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReportedWindow {
    pub tokens: u64,
    pub rounded: bool,
}

thread_local! {
    /// Test seam for [`probe_window`]: model id → window, standing in for the
    /// process-global probe cache on the calling thread only, so parallel test
    /// threads each see their own listing and nothing spawns `pi` to fill it.
    static PROBE_WINDOWS_OVERRIDE: std::cell::RefCell<Option<std::collections::BTreeMap<String, ReportedWindow>>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only seam: answer [`probe_window`] from `windows` on the calling thread
/// (`None` restores the real cache). A real `pub` function rather than
/// `#[cfg(test)]` for the reason `set_pi_sessions_root_for_test` is one: the
/// integration tests that link the lib cannot see `cfg(test)` items.
#[doc(hidden)] // pub for the `piusage` integration test
pub fn set_probe_windows_for_test(windows: Option<std::collections::BTreeMap<String, ReportedWindow>>) {
    PROBE_WINDOWS_OVERRIDE.with(|c| *c.borrow_mut() = windows);
}

/// The window `program`'s cached `--list-models` probe reported for `model`
/// (`cliprobe::cached_context_window` — a lookup that never spawns the CLI).
pub fn probe_window(program: &str, model: &str) -> Option<ReportedWindow> {
    if let Some(hit) = PROBE_WINDOWS_OVERRIDE.with(|c| c.borrow().as_ref().map(|m| m.get(model).copied())) {
        return hit;
    }
    crate::cliprobe::cached_context_window(program, model).map(|(tokens, rounded)| ReportedWindow { tokens, rounded })
}

/// Resolve and read one pi session's bounded tail from the group's own pi
/// store `dir` (`OrchRegistry::pi_sessions_dir`, the `--session-dir` every
/// group pi pane is launched with), then map it to the shared compaction
/// signal.
///
/// - **effort** falls back to `launch_effort` — the `--thinking` value loomux
///   passed, i.e. the block's clamped effort knob — only when the tail holds
///   no `thinking_level_change`. At `v0.84.4` pi writes one for every new
///   session (`src/core/sdk.ts`, `appendThinkingLevelChange(thinkingLevel)`),
///   so the fallback covers a long session whose entry has left the tail. The
///   returned signal marks this provenance; it is a configuration value, not
///   a reading of the agent's current effort.
/// - **window** comes from `window_for(model)`, the cached `--list-models`
///   probe in production: the session file records no window. A model the
///   probe does not list gets `None`, never a guess.
#[doc(hidden)] // pub for the `piusage` integration test
pub fn pi_compaction_signal_in(
    dir: &std::path::Path,
    session: &loomux_engine::pathseg::PathSegment,
    launch_effort: Option<&str>,
    window_for: &dyn Fn(&str) -> Option<ReportedWindow>,
) -> Option<crate::usage::CompactionSignal> {
    let path = crate::orchestration::pi_session_file_in_dir(dir, session).ok().flatten()?;
    let text = crate::usage::read_transcript_tail(&path)?;
    let reading = pi_context_signal(&text)?;
    let window = reading.model.as_deref().and_then(window_for);
    let launch_effort = launch_effort.map(str::trim).filter(|e| !e.is_empty()).map(str::to_owned);
    let effort_is_launch_fallback = reading.effort.is_none() && launch_effort.is_some();
    Some(crate::usage::CompactionSignal {
        tokens: reading.tokens,
        compact_boundary_count: reading.compaction_markers,
        model: reading.model,
        window_tokens: window.map(|w| w.tokens),
        window_rounded: window.is_some_and(|w| w.rounded),
        effort: reading.effort.or(launch_effort),
        effort_is_launch_fallback,
        source: ContextSource::PiSession,
    })
}

/// Read one opencode session's context signal from the group store `db`
/// (`OrchRegistry::opencode_db_path`, where every group opencode pane's
/// `OPENCODE_DB` points) on ONE read-only connection (#993 S2c).
///
/// - **model** and **effort** — the session row's `model` column, decoded by
///   `opencodedb::parse_model_column`: `providerID/id`, and the `variant`
///   as effort. No fallback to the block's effort knob: loomux's launch line
///   passes no variant, so there is no requested level to fall back to.
/// - **tokens** — `opencodedb::latest_assistant_context_on`, the newest
///   counted assistant message's `input + cache.read + cache.write`. `None`
///   before the first finished turn, and after a compaction until the first
///   post-compact turn finishes; the signal still carries the model.
/// - **window** — always `None`. The store records none, the configuration
///   docs are silent on `limit.context`, and the one source that carries it
///   (the server's `/config/providers`) needs an `opencode serve` loomux
///   does not run. See `docs/design/pane-model-state.md`, S2c.
/// - **marker count** — `0`: no documented compaction-done signal is read here
///   (the S2c section records the `summary: true` message as an observation).
///
/// `None` when the store cannot be read (every `opencodedb::Unavailable` —
/// the usage path already audits those, once per episode) or holds no such
/// session.
#[doc(hidden)] // pub for the `opencodeusage` integration test
pub fn opencode_compaction_signal_in(db: &std::path::Path, session_id: &str) -> Option<crate::usage::CompactionSignal> {
    let conn = crate::opencodedb::open_readonly(db).ok()?;
    let state = crate::opencodedb::session_model_state_on(&conn, session_id).ok()??;
    let tokens = crate::opencodedb::latest_assistant_context_on(&conn, session_id).ok()?;
    Some(crate::usage::CompactionSignal {
        tokens,
        compact_boundary_count: 0,
        model: state.model,
        window_tokens: None,
        window_rounded: false,
        effort: state.variant,
        effort_is_launch_fallback: false,
        source: ContextSource::OpencodeDb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload the status-line docs publish as their full example
    /// ("Full JSON schema"), cut to the fields this module reads plus a few it
    /// ignores — so an unknown sibling key never breaks the parse.
    const DOCS_EXAMPLE: &str = r#"{
        "cwd": "/current/working/directory",
        "session_id": "abc123",
        "transcript_path": "/path/to/transcript.jsonl",
        "model": { "id": "claude-opus-5-5", "display_name": "Opus" },
        "version": "2.1.90",
        "context_window": {
            "total_input_tokens": 15500,
            "total_output_tokens": 1200,
            "context_window_size": 200000,
            "used_percentage": 8,
            "remaining_percentage": 92,
            "current_usage": {
                "input_tokens": 8500, "output_tokens": 1200,
                "cache_creation_input_tokens": 5000, "cache_read_input_tokens": 2000
            }
        },
        "exceeds_200k_tokens": false,
        "effort": { "level": "high" }
    }"#;

    #[test]
    fn the_docs_example_payload_parses_into_every_field() {
        let s = parse_statusline_snapshot(DOCS_EXAMPLE).expect("the docs' own example must parse");
        assert_eq!(s.session_id.as_deref(), Some("abc123"));
        assert_eq!(s.model_id.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(s.model_name.as_deref(), Some("Opus"));
        assert_eq!(s.effort.as_deref(), Some("high"));
        assert_eq!(s.window_tokens, Some(200_000));
        // The docs define total_input_tokens as input + cache_creation +
        // cache_read (8500 + 5000 + 2000) — the same formula as the
        // transcript reader's `latest_context_tokens`.
        assert_eq!(s.input_tokens, Some(15_500));
        assert_eq!(s.used_percentage, Some(8.0));
    }

    #[test]
    fn a_null_current_usage_is_no_token_reading_not_a_zero() {
        // Before the first API call, and right after /compact, the docs say
        // current_usage is null and the totals are 0. A 0 read there would
        // look to the compaction state machine like a compaction's drop.
        let text = r#"{"session_id":"s","model":{"id":"m"},
            "context_window":{"total_input_tokens":0,"context_window_size":1000000,"current_usage":null}}"#;
        let s = parse_statusline_snapshot(text).unwrap();
        assert_eq!(s.input_tokens, None);
        // The window is a session fact, still good while usage is empty.
        assert_eq!(s.window_tokens, Some(1_000_000));
    }

    #[test]
    fn absent_effort_and_absent_context_block_are_none_not_a_parse_failure() {
        let s = parse_statusline_snapshot(r#"{"session_id":"s","model":{"id":"claude-haiku-4-5"}}"#).unwrap();
        assert_eq!(s.model_id.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!((s.effort, s.window_tokens, s.input_tokens), (None, None, None));
    }

    #[test]
    fn a_torn_or_non_object_file_is_no_snapshot() {
        assert_eq!(parse_statusline_snapshot(""), None);
        assert_eq!(parse_statusline_snapshot(r#"{"session_id":"s","model":{"#), None, "a torn write");
        assert_eq!(parse_statusline_snapshot("[1,2]"), None);
        assert_eq!(parse_statusline_snapshot("42"), None);
    }

    #[test]
    fn a_zero_window_is_not_a_reported_window() {
        let s = parse_statusline_snapshot(r#"{"context_window":{"context_window_size":0}}"#).unwrap();
        assert_eq!(s.window_tokens, None);
    }

    fn transcript(tokens: Option<u64>, model: &str) -> crate::usage::CompactionSignal {
        crate::usage::CompactionSignal {
            tokens,
            compact_boundary_count: 2,
            model: Some(model.to_string()),
            window_tokens: None,
            window_rounded: false,
            effort: None,
            effort_is_launch_fallback: false,
            source: ContextSource::Transcript,
        }
    }

    fn snapshot(session: &str) -> StatuslineSnapshot {
        StatuslineSnapshot {
            session_id: Some(session.into()),
            model_id: Some("claude-sonnet-5".into()),
            model_name: Some("Sonnet".into()),
            effort: Some("xhigh".into()),
            window_tokens: Some(1_000_000),
            input_tokens: Some(99_999),
            used_percentage: Some(9.0),
        }
    }

    #[test]
    fn a_matching_snapshot_supplies_model_effort_and_window_but_not_tokens() {
        let snap = snapshot("sess-1");
        let s = enrich_with_statusline(transcript(Some(150_000), "claude-sonnet-4-6"), Some(&snap), "sess-1");
        assert_eq!(s.window_tokens, Some(1_000_000));
        assert_eq!(s.effort.as_deref(), Some("xhigh"));
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(s.source, ContextSource::Statusline);
        // Tokens stay the transcript's: the state machine's baselines came
        // from there.
        assert_eq!(s.tokens, Some(150_000));
        assert_eq!(s.compact_boundary_count, 2, "the boundary count is always the transcript's");
    }

    #[test]
    fn snapshot_tokens_fill_in_only_when_the_transcript_has_none() {
        let snap = snapshot("sess-1");
        let s = enrich_with_statusline(transcript(None, "m"), Some(&snap), "sess-1");
        assert_eq!(s.tokens, Some(99_999));
    }

    #[test]
    fn a_session_id_mismatch_is_ignored_entirely() {
        // A file left behind by the pane's previous session.
        let snap = snapshot("old-session");
        let s = enrich_with_statusline(transcript(None, "claude-sonnet-4-6"), Some(&snap), "sess-1");
        assert_eq!((s.window_tokens, s.effort.as_deref(), s.tokens), (None, None, None));
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(s.source, ContextSource::Transcript);
        // And a snapshot with no session id at all is nobody's.
        let anon = StatuslineSnapshot { session_id: None, ..snapshot("x") };
        assert_eq!(enrich_with_statusline(transcript(None, "m"), Some(&anon), "sess-1").window_tokens, None);
    }

    #[test]
    fn the_ladder_takes_each_rung_in_order() {
        // 1. override beats a report.
        assert_eq!(context_window_ladder(Some(300_000), Some(1_000_000), 200_000, Some(10)), (300_000, WindowSource::Override));
        // 2. a report beats the table — the Sonnet-5-at-1M case the table
        //    reads as 200K.
        assert_eq!(context_window_ladder(None, Some(1_000_000), 200_000, Some(460_000)), (1_000_000, WindowSource::Reported));
        // 3. no report: the table.
        assert_eq!(context_window_ladder(None, None, 200_000, Some(46_000)), (200_000, WindowSource::Table));
        assert_eq!(context_window_ladder(None, None, 200_000, None), (200_000, WindowSource::Table));
        // A zero report is not a report.
        assert_eq!(context_window_ladder(None, Some(0), 200_000, None), (200_000, WindowSource::Table));
    }

    #[test]
    fn the_clamp_widens_to_the_observed_tokens_and_tags_it() {
        // The table guessed 200K; the pane holds 250K, so the table is wrong.
        assert_eq!(context_window_ladder(None, None, 200_000, Some(250_000)), (250_000, WindowSource::Clamped));
        // A stale report is corrected the same way.
        assert_eq!(context_window_ladder(None, Some(200_000), 200_000, Some(200_001)), (200_001, WindowSource::Clamped));
        // Exactly full is not over-full: no clamp.
        assert_eq!(context_window_ladder(None, Some(200_000), 200_000, Some(200_000)), (200_000, WindowSource::Reported));
    }

    #[test]
    fn the_clamp_never_overrules_a_human_override() {
        assert_eq!(context_window_ladder(Some(200_000), None, 200_000, Some(250_000)), (200_000, WindowSource::Override));
    }

    #[test]
    fn the_first_layer_with_a_command_status_line_wins() {
        let local = r#"{"permissions":{}}"#; // no statusLine: does not hide lower layers
        let project = r#"{"statusLine":{"type":"command","command":"~/.claude/sl.sh","padding":2,"refreshInterval":5}}"#;
        let user = r#"{"statusLine":{"type":"command","command":"echo user"}}"#;
        assert_eq!(
            resolve_user_statusline(&[local, project, user]),
            Some(UserStatusLine { command: "~/.claude/sl.sh".into(), padding: Some(2), refresh_interval: Some(5) })
        );
        assert_eq!(resolve_user_statusline(&[local, user]).unwrap().command, "echo user");
    }

    #[test]
    fn no_usable_status_line_resolves_to_none() {
        assert_eq!(resolve_user_statusline(&[]), None);
        assert_eq!(resolve_user_statusline(&["not json", "{}"]), None);
        assert_eq!(resolve_user_statusline(&[r#"{"statusLine":{"type":"command","command":"   "}}"#]), None);
        assert_eq!(resolve_user_statusline(&[r#"{"statusLine":{"type":"other","command":"echo x"}}"#]), None);
        assert_eq!(resolve_user_statusline(&[r#"{"statusLine":{"command":"echo x"}}"#]), None, "type is required by the docs");
    }

    #[test]
    fn single_quoting_survives_every_shell_metacharacter() {
        assert_eq!(sh_single_quote("echo hi"), "'echo hi'");
        assert_eq!(sh_single_quote("it's"), r"'it'\''s'");
        assert_eq!(with_chained_command("base", None), "base");
        assert_eq!(with_chained_command("base", Some("  ")), "base");
        assert_eq!(with_chained_command("base", Some("a $b")), "base 'a $b'");
    }

    fn token_count(tokens: u64, window: Option<u64>) -> String {
        let window = window.map(|value| format!(",\"model_context_window\":{value}")).unwrap_or_default();
        format!("{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"last_token_usage\":{{\"input_tokens\":{tokens}}}{window}}}}}}}")
    }

    fn turn(model: &str, effort: Option<&str>) -> String {
        let effort = effort.map(|value| format!(",\"effort\":\"{value}\"")).unwrap_or_default();
        format!("{{\"type\":\"turn_context\",\"payload\":{{\"model\":\"{model}\"{effort}}}}}")
    }

    #[test]
    fn newest_token_count_wins() {
        let text = format!("{}\n{}", token_count(11, Some(100)), token_count(22, Some(200)));
        let got = codex_context_signal(&text).unwrap();
        assert_eq!(got.tokens, Some(22));
        assert_eq!(got.window_tokens, Some(200));
    }

    #[test]
    fn a_null_info_token_count_does_not_shadow_the_last_useful_reading() {
        let text = format!(
            "{}\n{}",
            token_count(42, Some(272_000)),
            r#"{"type":"event_msg","payload":{"type":"token_count","info":null}}"#
        );
        let got = codex_context_signal(&text).unwrap();
        assert_eq!(got.tokens, Some(42));
        assert_eq!(got.window_tokens, Some(272_000));
    }

    #[test]
    fn missing_model_context_window_is_none() {
        let got = codex_context_signal(&token_count(42, None)).unwrap();
        assert_eq!(got.tokens, Some(42));
        assert_eq!(got.window_tokens, None);
    }

    #[test]
    fn newest_turn_context_reads_model_and_effort() {
        let text = format!("{}\n{}", turn("old", Some("low")), turn("new", Some("high")));
        let got = codex_context_signal(&text).unwrap();
        assert_eq!(got.model.as_deref(), Some("new"));
        assert_eq!(got.effort.as_deref(), Some("high"));
    }

    #[test]
    fn turn_context_without_effort_keeps_effort_absent() {
        let got = codex_context_signal(&turn("gpt-5-codex", None)).unwrap();
        assert_eq!(got.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(got.effort, None);
    }

    #[test]
    fn counts_compaction_markers() {
        let text = "{\"type\":\"compacted\"}\n{\"type\":\"compacted\"}";
        assert_eq!(codex_context_signal(text).unwrap().compaction_markers, 2);
    }

    #[test]
    fn a_rounded_report_is_relabelled_only_on_the_reported_rung() {
        let ladder = |reported: Option<u64>, observed: u64, rounded: bool| {
            label_rounded_report(context_window_ladder(None, reported, 200_000, Some(observed)), rounded)
        };
        assert_eq!(ladder(Some(262_050), 10, true), (262_050, WindowSource::ReportedRounded));
        assert_eq!(ladder(Some(262_144), 10, false), (262_144, WindowSource::Reported));
        // A clamp has proved the report too small: it is no longer the figure shown.
        assert_eq!(ladder(Some(262_050), 300_000, true), (300_000, WindowSource::Clamped));
        // No report: the table rung is untouched by a stray flag.
        assert_eq!(ladder(None, 10, true), (200_000, WindowSource::Table));
        // An override outranks the report whatever its spelling.
        assert_eq!(
            label_rounded_report(context_window_ladder(Some(500_000), Some(262_050), 200_000, None), true),
            (500_000, WindowSource::Override)
        );
        assert_eq!(WindowSource::ReportedRounded.as_str(), "reported-rounded");
    }

    #[test]
    fn a_non_claude_reading_never_publishes_a_window_only_the_table_decided() {
        // #413 S4: the table is Claude's, so for every other reader it is a
        // guess — for codex and pi (a missing report) as for opencode (no
        // report ever).
        let table = context_window_ladder(None, None, 200_000, Some(40_000));
        assert_eq!(table, (200_000, WindowSource::Table), "precondition: only the table answered");
        let clamped = context_window_ladder(None, None, 200_000, Some(250_000));
        assert_eq!(clamped, (250_000, WindowSource::Clamped));
        let over = context_window_ladder(Some(128_000), None, 200_000, Some(40_000));
        let over_report = context_window_ladder(None, Some(100_000), 200_000, Some(150_000));
        assert_eq!(over_report, (150_000, WindowSource::Clamped));
        for source in [ContextSource::CodexRollout, ContextSource::PiSession, ContextSource::OpencodeDb] {
            assert!(!source.table_rung_applies(), "{source:?}");
            assert_eq!(published_window(table, Some(source), None), None, "{source:?}");
            // A clamp over the table is the table's guess widened — still no report.
            assert_eq!(published_window(clamped, Some(source), None), None, "{source:?}");
            // A human override is not a guess, for any CLI.
            assert_eq!(published_window(over, Some(source), None), Some((128_000, WindowSource::Override)), "{source:?}");
            // A clamp over a REPORT widened the CLI's own figure, not the table's.
            assert_eq!(published_window(over_report, Some(source), Some(100_000)), Some(over_report), "{source:?}");
        }
    }

    #[test]
    fn a_remembered_rollout_is_reused_only_while_fresh_and_present() {
        let t0 = std::time::Instant::now();
        let r = RememberedRollout { path: std::path::PathBuf::from("rollout.jsonl"), resolved: t0 };
        let after = |secs| t0 + std::time::Duration::from_secs(secs);
        let every = CODEX_ROLLOUT_REVALIDATE_AFTER;
        assert!(reuse_remembered_rollout(&r, t0, every, true), "the tick after a walk reads the memo");
        assert!(reuse_remembered_rollout(&r, after(299), every, true), "still inside the interval");
        // The timer is what bounds a `thread/revert` the stat cannot see.
        assert!(!reuse_remembered_rollout(&r, after(300), every, true), "due: walk again");
        assert!(!reuse_remembered_rollout(&r, after(3_600), every, true));
        // The stat: a file that has gone (compressed, deleted) is walked for at once.
        assert!(!reuse_remembered_rollout(&r, t0, every, false));
        // A clock that reads earlier than the resolution is not "expired".
        assert!(reuse_remembered_rollout(&RememberedRollout { resolved: after(10), ..r.clone() }, t0, every, true));
    }

    #[test]
    fn a_claude_reading_and_the_pre_reading_state_keep_the_table_answer() {
        // The converse that makes the refusal above discriminating: a
        // `published_window` that refused every table answer would pass the
        // test above and fail here. Claude's own readers keep the table they
        // have always escalated on.
        let table = context_window_ladder(None, None, 200_000, Some(40_000));
        let clamped = context_window_ladder(None, None, 200_000, Some(250_000));
        for source in [ContextSource::Transcript, ContextSource::Statusline] {
            assert!(source.table_rung_applies(), "{source:?}");
            assert_eq!(published_window(table, Some(source), None), Some(table), "{source:?}");
            assert_eq!(published_window(clamped, Some(source), None), Some(clamped), "{source:?}");
        }
        assert_eq!(published_window(table, None, None), Some(table), "no reading yet: the pre-S3 answer");
    }

    #[test]
    fn a_launch_fallback_effort_is_not_an_observed_one() {
        let mut s = transcript(Some(1_000), "openrouter/z-ai/glm-5.3-flash");
        s.effort = Some("high".into());
        assert_eq!(s.observed_effort(), Some("high"), "a reported level is observed");
        s.effort_is_launch_fallback = true;
        assert_eq!(s.observed_effort(), None, "the launch knob is configuration, not a reading");
    }

    #[test]
    fn a_pi_assistant_turn_with_all_zero_usage_is_no_token_reading() {
        // pi writes an all-zero usage on an errored turn; a 0 would read as a
        // compaction's drop. The earlier real reading stands.
        let text = [
            r#"{"type":"message","message":{"role":"assistant","provider":"p","model":"m","usage":{"input":900,"cacheRead":90,"cacheWrite":9,"output":5}}}"#,
            r#"{"type":"message","message":{"role":"assistant","provider":"p","model":"m","usage":{"input":0,"cacheRead":0,"cacheWrite":0,"output":0}}}"#,
        ]
        .join("\n");
        assert_eq!(pi_context_signal(&text).unwrap().tokens, Some(999));
    }

    #[test]
    fn a_pi_header_alone_is_no_reading() {
        assert_eq!(pi_context_signal(r#"{"type":"session","version":3,"id":"x","cwd":"/r"}"#), None);
        assert_eq!(pi_context_signal(""), None);
    }
}
