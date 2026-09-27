//! The status-line snapshot: its path and the session cost parsed from it.
//! Design note: `docs/design/group-cost-tracking.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): `PtyManager`,
//! `crate::pty`. Sibling files it calls: `grouppath.rs`, `panetail.rs`.

use super::*;

/// Best-effort extraction of a session's dollar cost from a pane's
/// ANSI-stripped terminal tail. Claude Code renders running cost in its
/// in-pane statusline (bottom of the screen), so scan lines bottom-up and
/// return the dollar amount from the lowest line that carries one — that is
/// the freshest statusline render. Thousands separators are tolerated.
/// Returns `None` when no `$<amount>` token is present.
/// How much of a pane's output ring the usage poll reads looking for the CLI's
/// own statusline dollar figure (#743 S7, `performance.md` INV-5 / P3).
///
/// 64 KiB — a quarter of `OUTPUT_RING_CAP`, and 16x `ATTENTION_SCAN_BYTES`.
/// The statusline is a live status line: redrawn in place with every frame the
/// CLI paints, so the figure is always inside the most recent repaint. The
/// window is sized for that repaint rather than for the figure, and generously
/// — one full-screen redraw of a very large pane (300x100 cells) with heavy
/// SGR styling is still well inside 64 KiB, where the attention scan's 4 KiB
/// would not be. ASSUMED, with its falsifier named: a CLI that paints its
/// statusline LESS often than every 64 KiB of other output would stop being
/// read, and the observable is a pane whose usage `source` stays `"none"`
/// while the CLI is visibly showing a `$` figure.
///
/// Truncation cannot corrupt the answer, only lose it. `parse_session_cost`
/// scans lines back-to-front, so the only line a window boundary can mangle is
/// the OLDEST one in it, and mangling can only ever delete characters: a `$`
/// that survives has the whole amount after it intact, and a `$` that does not
/// survive yields no match. A lost figure lands on the branch that already
/// handles "this CLI shows nothing" (subscription/Max accounts, a killed
/// pane) — `source` stays `"none"` and no cost is claimed, which is the
/// fail-safe direction for a figure that feeds the budget meters.
const STATUSLINE_SCAN_BYTES: usize = 64 * 1024;

/// The statusline dollar figure for one pane — [`OrchRegistry::compute_usage_
/// snapshot`]'s last-resort branch, extracted so an integration test can drive
/// the read a real pane gets (the `preenter_admission` seam; `self.app` cannot
/// be resolved headless).
///
/// This sits on the app's hottest cadenced work — `group_usage_live_within`,
/// per agent. It was reached every 2 s from an open group view and every 4 s
/// from each group-bound tab; since #1608 the snapshot publisher is the caller,
/// once per second, and both surfaces read its output instead — which
/// is why it reads the last `STATUSLINE_SCAN_BYTES` rather than cloning and
/// ANSI-stripping the whole ≤256 KiB ring for one number that is by
/// construction the last thing painted.
#[doc(hidden)] // pub for integration tests
pub fn statusline_cost(ptys: &crate::pty::PtyManager, pty_id: u32) -> Option<f64> {
    let raw = ptys.output_tail_bounded(pty_id, STATUSLINE_SCAN_BYTES)?;
    parse_session_cost(&strip_ansi(&raw))
}

pub fn parse_session_cost(text: &str) -> Option<f64> {
    for line in text.lines().rev() {
        if let Some(cost) = line
            .match_indices('$')
            .find_map(|(i, _)| parse_dollar_amount(&line[i + 1..]))
        {
            return Some(cost);
        }
    }
    None
}

/// Parse a leading `1,234.56`-style number (optionally after the `$` already
/// consumed by the caller), returning `None` if the text does not start with
/// a digit. Commas are dropped; a single decimal point is honored.
fn parse_dollar_amount(after_dollar: &str) -> Option<f64> {
    let mut digits = String::new();
    let mut seen_dot = false;
    for c in after_dollar.chars() {
        match c {
            '0'..='9' => digits.push(c),
            ',' if !seen_dot => {} // thousands separator
            '.' if !seen_dot => {
                seen_dot = true;
                digits.push('.');
            }
            _ => break,
        }
    }
    // Reject a bare "." or empty (a lone `$` or `$.`); require a real digit.
    if digits.is_empty() || digits == "." {
        return None;
    }
    digits.parse::<f64>().ok()
}

/// #993 S1: where `COMPACT_HOOK_SCRIPT`'s `statusline` arm leaves the latest
/// Claude Code status-line payload for one agent — a sibling of the
/// `promptsubmit` marker (`submit.rs`), in the same group `hooks/` dir, and typed the
/// same way for the same reason: the id becomes part of a file name, so the
/// caller must hold a [`PathSegment`] before it can ask. One whole file,
/// replaced on every write (the script writes `.tmp` and renames), because
/// only the LATEST reading means anything.
#[doc(hidden)] // pub for integration tests
pub fn statusline_snapshot_path(root: &Path, group: &GroupId, agent_id: &PathSegment) -> PathBuf {
    group_dir_at(root, group)
        .join("hooks")
        .join(format!("{agent_id}.statusline.json"))
}
