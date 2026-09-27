//! The live projection of the memoised group usage (#1317).
//! Design note: `docs/design/group-cost-tracking.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. It calls no
//! sibling file.

use super::*;

/// Which projection of the memoised group-usage value a reader wants (#1317).
///
/// Both come out of ONE computation and one memo cell — see
/// [`OrchRegistry::group_usage_memoed`].
#[derive(Clone, Copy)]
pub(in crate::orchestration) enum UsageView {
    /// The whole-roster value: every snapshot the group ever captured. What
    /// the MCP `group_usage` tool, the autonomy anchor and the budget enforcer
    /// read.
    Full,
    /// [`live_usage_view`] of it — what the POLLED GUI command returns.
    Live,
}

/// Project [`OrchRegistry::group_usage`]'s value down to what a POLLED reader
/// needs: the live agents' rows, and no per-agent row for anyone else (#1317).
///
/// **What it is fixing.** `agents` is O(agents-EVER), not O(agents-live):
/// `compute_group_usage` merges each live agent's fresh snapshot with every
/// snapshot `mark_dead` ever captured, so the array only grows with how long
/// the human has been running. Nothing accumulates ACROSS ticks — the array is
/// replaced wholesale each time — but the size of ONE tick grows with session
/// length, on a fixed cadence (a 2 s group view and a 4 s per-group-bound-tab
/// sweep when #1317 measured it; one 1 s publisher pass since #1608, which
/// changed which thread pays it, not that it is paid).
/// That is allocation churn proportional to session length, which is what a
/// long session feels as GC pressure. The MCP twin was capped for exactly this
/// (`mcp::summarize_group_usage`: "a 654-agent lifetime roster serialized to
/// 173,245 chars"); the command the GUI polls was not.
///
/// **Why LIVE is the right cut, rather than a top-N cap.** The GUI never
/// wanted the historical rows: `groupview.ts` indexes this array by agent id
/// and looks up only the agents `orch_group_summary` reports LIVE, and
/// `tabbar.ts` reads `live_cost_usd` and no row at all. Live agents are
/// bounded by the group's `max_agents`; the lifetime roster is bounded by
/// nothing. A top-N cap would be both looser and — since top-N is by lifetime
/// tokens — capable of dropping a live agent's row, which is the one row this
/// caller actually renders.
///
/// **Not a silent truncation**, the property `summarize_group_usage`'s `rest`
/// count holds for the MCP twin:
/// - every LIFETIME total passes through untouched — `lifetime_tokens`,
///   `lifetime_cost_usd` and `lifetime_cost_basis` still sum the whole roster,
///   so no spend disappears from the figure the group view puts on screen;
/// - `agent_count` names the roster's real size, so `agent_count !=
///   live_agents.len()` is readable rather than invisible;
/// - the key is RENAMED (`agents` → `live_agents`) rather than filtered in
///   place, so a reader written against the whole-roster `agents` fails loudly
///   instead of quietly seeing a subset. #866 took the same decision for
///   `top_agents` on the MCP side, for the same reason.
///
/// Builds a fresh object rather than cloning and stripping: cloning the full
/// value to throw the historical rows away would pay the very allocation this
/// exists to remove.
#[doc(hidden)] // pub for integration tests
pub fn live_usage_view(full: &Value) -> Value {
    let rows = full.get("agents").and_then(|a| a.as_array());
    let agent_count = rows.map_or(0, |r| r.len());
    let live: Vec<Value> = rows
        .map(|r| {
            r.iter()
                .filter(|a| a["live"].as_bool().unwrap_or(false))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mut out = serde_json::Map::new();
    if let Some(obj) = full.as_object() {
        for (k, v) in obj {
            if k == "agents" {
                continue;
            }
            out.insert(k.clone(), v.clone());
        }
    }
    out.insert("agent_count".to_string(), json!(agent_count));
    out.insert("live_agents".to_string(), Value::Array(live));
    Value::Object(out)
}
