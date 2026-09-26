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
        input_tokens: if usage_live || true /* MUTATION e2: null current_usage read as a reading */ {
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
/// (approved on #993):
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
    let _ = session_id; // MUTATION a: session filter dropped
    let Some(snap) = snapshot else {
        return signal;
    };
    if signal.tokens.is_none() {
        signal.tokens = snap.input_tokens;
    }
    if let Some(model) = &snap.model_id {
        signal.model = Some(model.clone());
    }
    signal.window_tokens = snap.window_tokens;
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
}

impl ContextSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextSource::Transcript => "transcript",
            ContextSource::Statusline => "statusline",
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
    let _ = observed_tokens; // MUTATION e1: clamp removed
    (window, source)
}

/// The human's own status line, as read from their Claude settings at spawn —
/// what loomux's status-line command chains to so an orrerix pane looks the
/// same as a plain claude pane (#993, the human's decided default).
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
/// has one — loomux's status line then prints nothing, which is exactly what a
/// claude pane with no status line configured shows.
pub fn resolve_user_statusline(layers: &[&str]) -> Option<UserStatusLine> {
    layers.iter().find_map(|text| {
        let v: Value = serde_json::from_str(text).ok()?;
        let sl = v.get("statusLine")?;
        // MUTATION e3: type check dropped
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
    format!("'{}'", s) // MUTATION e4: embedded quote not escaped
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
            effort: None,
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
}
