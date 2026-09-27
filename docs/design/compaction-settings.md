# Compaction guardrail settings

The group's lifecycle panel reads the three live compaction settings from the
existing group summary in the published group-view payload. It writes through
the existing setters, which persist and audit each edit; those commands also
republish the group view so the panel reflects changes immediately. This keeps
the feature on the existing panel/read path rather than adding another polling
command.

New launcher-created groups use a 45% context-escalation threshold, sourced
from one shared default constant. The group-file loader uses that value only
when the key is absent; a stored zero remains the explicit off choice. Threshold
escalation is gated by the group's compaction-eligible roles, which default to
the orchestrator only, preventing the new fallback from compacting delegates.
This also narrows prior behavior for resumed groups with a nonzero threshold:
delegates previously escalated by that threshold now need their role included
in `compact_nudge_roles`; that role list is configurable through its backend
setter but is not exposed in the lifecycle panel. The nudge floor remains
tri-state in storage: an unset floor displays the backend's 50% smart default,
while a user edit persists an explicit value.

The three settings apply to every CLI whose `CliCaps` row carries a compact
command — claude, copilot, codex, pi and opencode (#413 S4) — and to no other.
The threshold is a percentage, so it can only act on a pane whose context
window is known: reported by its CLI, or set by the group's
`context_window_tokens_override`. The model-name table that fills a missing
window is Claude's and applies only to a Claude pane. A codex or pi pane whose
CLI has reported no window, and every opencode pane, therefore never crosses
the threshold unless the group sets the override; the lifecycle panel shows
their tokens without a percent, and the audit log records one
`compact-escalation-skipped` per episode so the missing escalation has a
stated reason. The override is the lever that switches the threshold on for
those panes. The floor fails CLOSED for the same panes: a reading with tokens
and no window never passes it, so the lull nudge skips them too, and the
override is also what makes them eligible for it — while the escalation
threshold is above 0. `agent_context_percents` computes a percent only for a
group whose threshold is nonzero, and the floor reads that percent, so at 0 an
overridden pane (like every Claude pane) passes the floor at any fill level;
only the no-window refusal, read separately through `context_window_unknown`,
still holds. The threshold gate on the percent predates #413 S4; S4 made the
no-window input a separate read, and so the asymmetry. Deriving the floor's
percent independently of the threshold would close it, and would change
Claude's behaviour too, so it is not done here. They compact themselves when
they run out of room, and a lull compact at an unmeasured fill level is the
costly re-grounding the floor exists to stop. A pane with no reading at all
(copilot, or any pane before its first reading) still fails open. See
`docs/design/pane-model-state.md`, S4.

The three fields are additive to the existing group-summary wire object, so
older clients may ignore them and the existing group-view contract remains
usable. Input parsing is DOM-free and rejects non-integers and values outside
the supported range before calling the backend; backend setters remain the
final authority on bounds.
