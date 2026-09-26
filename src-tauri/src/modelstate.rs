//! Pure readers for model and context state recorded by CLI session artifacts.
//!
//! These parsers do not perform filesystem access; callers own artifact lookup.

use serde_json::Value;

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
/// `turn_context`: either may be absent, and the newest occurrence wins.
pub fn codex_context_signal(text: &str) -> Option<CodexContextReading> {
    let mut reading = CodexContextReading {
        compaction_markers: text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value.get("type").and_then(Value::as_str) == Some("compacted"))
            .count() as u64,
        ..CodexContextReading::default()
    };
    let mut found = reading.compaction_markers > 0;
    let mut found_token_count = false;
    let mut found_turn_context = false;

    for line in text.lines().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("event_msg")
                if !found_token_count
                    && value.pointer("/payload/type").and_then(Value::as_str)
                        == Some("token_count") =>
            {
                found = true;
                found_token_count = true;
                let info = value.pointer("/payload/info");
                reading.tokens = info
                    .and_then(|info| info.pointer("/last_token_usage/input_tokens"))
                    .and_then(Value::as_u64);
                reading.window_tokens = info
                    .and_then(|info| info.get("model_context_window"))
                    .and_then(Value::as_u64);
            }
            Some("turn_context") if !found_turn_context => {
                found_turn_context = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn token_count(tokens: u64, window: Option<u64>) -> String {
        let window = window
            .map(|value| format!(",\"model_context_window\":{value}"))
            .unwrap_or_default();
        format!(
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"last_token_usage\":{{\"input_tokens\":{tokens}}}{window}}}}}}}"
        )
    }

    fn turn(model: &str, effort: Option<&str>) -> String {
        let effort = effort
            .map(|effort| format!(",\"effort\":\"{effort}\""))
            .unwrap_or_default();
        format!("{{\"type\":\"turn_context\",\"payload\":{{\"model\":\"{model}\"{effort}}}}}")
    }

    #[test]
    fn newest_token_count_wins() {
        let text = format!(
            "{}\n{}",
            token_count(11, Some(100)),
            token_count(22, Some(200))
        );
        let got = codex_context_signal(&text).unwrap();
        assert_eq!(got.tokens, Some(22));
        assert_eq!(got.window_tokens, Some(200));
    }

    #[test]
    fn missing_model_context_window_is_none() {
        let got = codex_context_signal(&token_count(42, None)).unwrap();
        assert_eq!(got.tokens, Some(42));
        assert_eq!(got.window_tokens, None);
    }

    #[test]
    fn newest_turn_context_reads_model_and_effort() {
        let text = format!(
            "{}\n{}",
            turn("old", Some("low")),
            turn("new", Some("high"))
        );
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
    fn empty_or_non_rollout_text_has_no_reading() {
        assert_eq!(codex_context_signal("not json"), None);
        assert_eq!(codex_context_signal(""), None);
    }
}
