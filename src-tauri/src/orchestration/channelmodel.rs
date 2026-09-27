//! A cross-workspace channel's members and the event and message text it emits.
//! Design note: `docs/design/cross-workspace-channel.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. It calls no
//! sibling file.

use super::*;

/// One member of a [`Channel`]: which group/agent pane is connected. Cached
/// `name`/`role` so `channel_status` and the connect/disconnect notices don't
/// need a second `agent()` lookup for a peer that may since have died.
#[derive(Clone, Debug)]
pub struct ChannelMember {
    pub group: GroupId,
    pub agent_id: String,
    pub name: String,
    pub role: Role,
    /// Directional model (#271 W3 addendum, part B): a per-receiver reply
    /// credit. Ignored for the member named `Channel.sender`. Set true by
    /// the sender's `channel_send` (one credit per receiver per broadcast),
    /// consumed the moment that receiver replies — a receiver may never
    /// speak twice for one sender message, and never to another receiver.
    pub may_reply: bool,
}

/// A cross-workspace communication channel (#271): a human-connected session
/// of two-or-more agent panes, possibly in different groups. Mirrors
/// `notify::Watch`'s lifetime class — in-memory only, see the module-level
/// doc on `channels`.
#[derive(Clone, Debug)]
pub struct Channel {
    pub id: String,
    pub members: Vec<ChannelMember>,
    pub created_ms: u64,
    /// Directional model (#271 W3 addendum, part B): the single member
    /// (`agent_id`) that may broadcast at will. Every other member is a
    /// receiver, bound to reply-only-to-sender via `ChannelMember.may_reply`.
    /// Designated at connect time (human, explicit arrow) and swappable only
    /// via `OrchRegistry::set_sender` (human-only, audited `channel-direction`).
    pub sender: String,
    /// The UI-facing channel number (#271 follow-up, PR #285 live-testing
    /// feedback): the lowest positive integer not currently used by any
    /// OTHER live channel, assigned once at mint time and then immutable for
    /// this channel's lifetime. Deliberately NOT `id`'s numeric suffix — `id`
    /// (`chan-N`) is minted from `channel_seq`, a monotonic counter that must
    /// never reuse a value (an audit record for `chan-1` must never become
    /// ambiguous with a later, unrelated `chan-1`), so after chan-1 closes
    /// the NEXT channel is still `chan-2` even though nothing numbered "1" is
    /// active. `display_number` is the thing the pane chip actually shows —
    /// it frees up "1" the moment chan-1 closes, so the chip always reflects
    /// how many channels are ACTUALLY connected right now, not how many have
    /// ever existed. See `OrchRegistry::next_display_number`.
    pub display_number: u32,
}

/// The `[orrerix] channel <id> - <sender>: <text>` line loomux prefixes to a
/// `channel_send` delivery (#271). `chan_id` and `sender_label` are
/// backend-built (see `OrchRegistry::channel_member_label`) — never
/// agent-supplied — and `sanitized_text` must already have passed
/// `notify::sanitize_gh_text` before it reaches here, so a peer can never
/// forge who a message is from or inject a second `[orrerix] …` line. Pure so
/// the shape is unit-testable without a registry.
pub fn channel_message_text(chan_id: &str, sender_label: &str, sanitized_text: &str) -> String {
    format!("[orrerix] channel {chan_id} - {sender_label}: {sanitized_text}")
}

/// Build the `orch-channel` event's "connected" payload (fresh mint or a
/// third pane joining) — pure, so `display_number`'s presence is pinned
/// directly (#271 follow-up review finding: this codebase has no harness for
/// capturing an actually-emitted Tauri event — `self.app` is `None` in every
/// test registry, so `app.emit(...)` never fires — the payload construction
/// is factored out here instead, and both the real call site and the test
/// call the SAME function, so drift between them is structurally impossible).
pub fn channel_connected_event(chan_id: &str, sender: &str, display_number: u32, members: Vec<Value>) -> Value {
    json!({
        "kind": "connected", "channel_id": chan_id, "sender": sender,
        "display_number": display_number, "members": members,
    })
}

/// Build the `orch-channel` event's "disconnected"/"closed" payload — same
/// pure-extraction rationale as `channel_connected_event`.
pub fn channel_disconnected_event(
    closed: bool,
    chan_id: &str,
    agent: &str,
    display_number: u32,
    members: Vec<Value>,
) -> Value {
    json!({
        "kind": if closed { "closed" } else { "disconnected" },
        "channel_id": chan_id, "agent": agent,
        "display_number": display_number, "members": members,
    })
}

/// Build the `orch-channel` event's "updated" payload (a `set_sender` swap)
/// — same pure-extraction rationale as `channel_connected_event`.
pub fn channel_updated_event(chan_id: &str, sender: &str, display_number: u32, members: Vec<Value>) -> Value {
    json!({
        "kind": "updated", "channel_id": chan_id, "sender": sender,
        "display_number": display_number, "members": members,
    })
}
