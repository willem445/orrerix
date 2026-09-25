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
The nudge floor remains tri-state in storage: an unset floor displays the
backend's 50% smart default, while a user edit persists an explicit value.

The three fields are additive to the existing group-summary wire object, so
older clients may ignore them and the existing group-view contract remains
usable. Input parsing is DOM-free and rejects non-integers and values outside
the supported range before calling the backend; backend setters remain the
final authority on bounds.
