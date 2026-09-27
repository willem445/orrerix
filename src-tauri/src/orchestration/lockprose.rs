//! The text the named lock resources reply with (#858).
//! Design note: `docs/design/lock-resources.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. It calls no
//! sibling file.

// ── lock-resource prose helpers (#858) ─────────────────────────────────────

/// Whole minutes left until `deadline`, rounded UP and never below 1 — the
/// `watchline.ts` rule: a deadline 40 seconds away is "1 min", never "0 min",
/// which reads as expired.
pub(in crate::orchestration) fn minutes_until(deadline_ms: u64, now: u64) -> u64 {
    (deadline_ms.saturating_sub(now) + 59_999) / 60_000
}

/// A duration a human reads in a notice: `45s`, `12m`, `2h 5m`.
pub(in crate::orchestration) fn human_span(ms: u64) -> String {
    let total_min = ms / 60_000;
    if total_min == 0 {
        return format!("{}s", ms / 1000);
    }
    if total_min < 60 {
        return format!("{total_min}m");
    }
    format!("{}h {}m", total_min / 60, total_min % 60)
}

/// The queued-acquire reply. It says three things on purpose: where the caller
/// is, that it must NOT poll, and what will actually wake it — a worker told
/// only "queued" reliably invents a sleep loop, which is the #590 deadlock
/// (the grant notice is typed into the pane, and a pane blocked mid-turn
/// cannot take delivery of it).
pub(in crate::orchestration) fn queued_text(name: &str, position: usize, wait_minutes: u64, repeat: bool) -> String {
    let lead = if repeat {
        format!("you are ALREADY queued for '{name}' — your original place is kept")
    } else {
        format!("'{name}' is busy — you are queued for it")
    };
    format!(
        "{lead}, position {position}. Do NOT wait, sleep, or re-poll: END YOUR TURN. loomux types \
         an [orrerix] notice into this pane the moment the lock is yours. Your place is kept for \
         {wait_minutes} min, after which the request is dropped and you are told so."
    )
}
