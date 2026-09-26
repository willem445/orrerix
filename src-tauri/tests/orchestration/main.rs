//! Functional tests for the orchestration backend: guardrails, role authz,
//! group isolation, persistence, audit, and the MCP dispatch surface.
//!
//! These live as integration tests (not unit tests) because test executables
//! that link the full lib need the common-controls-v6 manifest embedded via
//! `rustc-link-arg-tests` (see build.rs / test.manifest), which cargo only
//! applies to integration-test targets.

use loomux_lib::lockwatch::TrackedMutex;
use loomux_lib::orchestration::intake;
// #656: the intake half's per-scan `gh` budget, asserted against the constant
// itself so the pin follows a future retune instead of pinning today's 4.
use loomux_lib::orchestration::intake::MAX_INTAKE_POLLS_PER_TICK;
// #656 (rev-lead findings 1 and 2): the bounded child wait, and the
// process-wide ceiling on readers abandoned by a timed-out capture.
use loomux_lib::orchestration::{
    drain_parked_readers_for_test, gh_capture_admitted, gh_capture_live_readers, gh_capture_parked_readers,
    seed_leaked_readers_for_test, wait_bounded, GH_CAPTURE_MAX_LEAKED_READERS,
};
use loomux_lib::orchestration::brand;
use loomux_lib::orchestration::mailbox;
use loomux_lib::orchestration::mcp::dispatch;
use loomux_lib::orchestration::notify;
use loomux_lib::orchestration::queue;
use loomux_lib::orchestration::mergeq;
use loomux_lib::orchestration::report;
use loomux_lib::orchestration::reviewdrive;
use loomux_lib::orchestration::workflow;
use loomux_lib::orchestration::providerlimit;
// #2011 slice B: the tuning fingerprint behind the series' marks, and the
// engine's pure core the sampler writes through.
use loomux_lib::orchestration::tuningfp;
use loomux_lib::orchestration::usageseries;
use loomux_lib::orchestration::GroupId;
use loomux_lib::orchestration::{
    add_trusted_folder, autonomy_budget_exhausted, bracketed_paste, box_occupancy_delta,
    set_copilot_trust_home_for_test,
    // #802: copilot's documented permission store, and the one-occurrence
    // tool-permission value both command builders share.
    copilot_permissions_grant, copilot_tool_permissions,
    // #803 review B1: the path-key rules, parameterized so BOTH platform
    // shapes are exercised on every CI leg.
    normalize_path_key_for, same_path_key_for,
    channel_connected_event, channel_disconnected_event, channel_message_text,
    channel_updated_event, classify_human_input,
    claude_effective_permission_mode, claude_permission_mode, cli_ready, compact_checklist_warning, compact_escalation_notice,
    COMPACT_HOOK_SCRIPT, COPILOT_PRECOMPACT_HOOK_BASH, COPILOT_PRECOMPACT_HOOK_POWERSHELL,
    COPILOT_PROMPTSUBMIT_HOOK_BASH, COPILOT_PROMPTSUBMIT_HOOK_POWERSHELL,
    ConfirmSource, DeliveryConfirmState, MonitorAction, PromptLandedMatch, PromptSubmitRecord,
    box_holds_paste, confirm_state_for, delivery_confirmed_late_notice, final_window_outcome,
    late_monitor_tick, poll_promptsubmit_hook, promptsubmit_marker_len, promptsubmit_marker_path,
    promptsubmit_records_since, prompt_landed, tier1_trusted,
    compact_escalation_should_fire, compact_nudge_cli_supported, compact_nudge_context_floor_met,
    DEFAULT_COMPACT_CONTEXT_THRESHOLD_PERCENT,
    compact_nudge_role_allowed,
    compact_reinjection_notice, compact_request_should_fire, compaction_status, context_percent_used,
    CompactionStatus,
    auto_compact_banner_detected, compact_nudge_poll_interval, compaction_confirmed, copilot_compaction_marker_detected, directive_ledger_embed, ledger_capped,
    human_typed_compact_detected, copilot_autopilot_prompt_detected, create_orchestration_group,
    // #1689 D1: the launcher's own create path, so a test can vary the workflow
    // name the way the form does rather than hand-building `Guardrails`.
    create_orchestration_sync, SpawnRequest,
    // #1020 item 5: how many idle workers a launch opens, and what an unasked
    // count resolves to.
    starter_workers,
    delivery_held_cleared_event, delivery_held_detail, delivery_held_event,
    exit_cause, exit_diagnostic, exit_notice_route, ExitInitiator, ExitNoticeRoute,
    resolve_output_text, format_output_tail, OUTPUT_TAIL_MAX_BYTES,
    gh_gate_decision, gh_is_merge_invocation, gh_positionals, gh_release_action, gh_repo_flag,
    // #2985: the PR-close ownership gate and the roster the shim reads.
    gh_branch_is_owned, gh_close_action, gh_close_decision, gh_close_refusal,
    gh_close_unverifiable_refusal, render_owner_roster, GhCloseGate, OWNER_ROSTER_FILE,
    gh_value_flags,
    gh_shim_cmd, gh_shim_sh, git_shim_cmd, git_shim_sh, git_tag_push, grant_segment, grant_unexpired, hold_for_human_input,
    resolve_shim_toolchain, ShimPaths,
    hold_until_quiet, idle_output_is_activity, idle_should_kill, idle_tick_should_fire,
    loomux_shim_cmd, loomux_shim_sh,
    // #3477: stale generated shims are pruned from the shared shim dir.
    is_stale_generated_shim, prune_stale_shims, GENERATED_SHIM_NAMES,
    // #406: the unified `gh` poller's shared scan cadence.
    intake_scan_due,
    low_disk_notice, low_disk_transition, max_agents_notice, pr_number, release_gate_decision,
    workflow_mode_notice,
    // #778: the full-autonomy toggle's pure surface.
    full_autonomy_notice, sanitize_full_autonomy_goal, MAX_FULL_AUTONOMY_GOAL_CHARS,
    GhGate, GitTagPush,
    normalize_remote_web_base, ORCHESTRATOR_TPL, ORCHESTRATOR_PLAYBOOK_TPL, PLAYBOOK_SECTION_IDS,
    playbook_section_ids, WORKER_TPL, REVIEWER_TPL, PLANNER_TPL, parse_audit_lines, parse_audit_lines_counted, parse_session_cost,
    prompt_wait_detected, question_hold_predicate, mask_own_paste, reinject_shape, resolve_paste_gate, resolve_ref_url,
    // #576: loomux's own notice rows are not questions.
    // #632: and the same for the CONTINUATION rows of a multi-row notice.
    mask_loomux_notices, unmaskable_framing_rows, NOTICE_MARKER,
    // #576 residual: the per-pane record of what loomux WROTE, and the mask
    // that reads it — the wrap and scrolled-off under-masks.
    mask_loomux_notices_with_record, loomux_authored_lines, DELIVERED_NOTICES_PER_PANE,
    DELIVERED_NOTICE_PANES,
    // #534 / #513(c): composed-grid question evidence.
    prompt_wait_match, question_hold_predicate_sampled, question_shown, grid_evidence_for,
    match_still_rendered, trustworthy_composition, witness_audit,
    // #3426: the composition the guard reads, less the input box's placeholder.
    question_visible,
    GridEvidence, QuestionMatch, QuestionNeedle, QuestionSample, QuestionWitness, QuestionWitnessed,
    // #903: the idle-composer reading, and the bounded last-resort override.
    idle_prompt_rendered, idle_prompt_row_rendered, question_override_admits, Composed,
    QUESTION_HOLD_OVERRIDE_AFTER,
    // #903: the session-keyed record of loomux's own delivered PROMPT text.
    delivered_prompt_lines, DELIVERED_PROMPT_CHARS, DELIVERED_NOTICE_CHARS,
    // #903 B2: the granted override that carries its own Enter.
    override_enter_admits, QuestionReread, QUESTION_OVERRIDE_CONSECUTIVE_READS,
    prompt_record_admits_kind,
    record_contributions_for,
    resume_kickoff_notice, rotate_audit_if_needed,
    ContractCarrier, ReinjectShape,
    agent_acted_since, reinject_disposition, ReinjectAck, ReinjectDisposition,
    retry_gate, sanitize_attachment_ext, set_rotate_check_pause_for_test, should_confirm_copilot_autopilot,
    should_flush_before_paste, should_flush_before_paste_now, flush_stranded_text,
    write_admission, WriteAdmission, preenter_admission, hold_bound_elapsed,
    held_escalation, HeldEscalation, QUESTION_HOLD_STALE_AFTER,
    // #560: the per-pane hold EPISODE that owns the escalation clock.
    ends_hold_episode, opens_hold_episode, HoldObservation,
    hold_channels, HoldChannel, HoldClass,
    // #590 L2: the host-side diagnosis for a pane holding loomux's own notice.
    undeliverable_cause, undeliverable_notice, UndeliverableCause,
    // #578: the orchestrator-target notice relay.
    orch_notice_relay_text, OrchNoticeInbox, ORCH_NOTICE_INBOX_MAX,
    pause_badge_decision, pause_suppression_notice, suppressed_during_pause, AuditEntry,
    PauseSuppression, SuppressedCause, SuppressedDelivery, StrandedNote, PAUSE_SUPPRESSION_LIST_MAX,
    // #579: front-door refusals, the losses with no queue id to report them under.
    front_door_refusals, RefusedPayload, AUDIT_VIEW_LIMIT, REFUSED_LIST_MAX,
    // #633: the reason discriminator that turned that list from a queue-full
    // list into a refusal list.
    RefusalReason,
    // #658: the drain-time roster that relays those refusals to the pane that
    // refused them, instead of waiting for a `queue_orphans` poll.
    refusal_roster, refusal_roster_notice, REFUSAL_ROSTER_ACTION, REFUSAL_ROSTER_OPENER,
    ROSTER_LIST_MAX, ROSTER_PREVIEW_MAX,
    record_aborted_preenter_outcome, recorded_confirmed,
    record_inflight_delivery, observe_ledger, LedgerView,
    drain_stranded_submit, stranded_admission_gate, stranded_detail, stranded_selfheal_action,
    // #813: a marker that cannot fire must not hold the rest of the pane queue behind it.
    stranded_marker_action, StrandedMarkerAction, StrandedRetireReason,
    // #825 M2: the badge-honesty matrix, hoisted out of the late monitor so a
    // raised chip keeps being checked after that monitor's 4h cap.
    stranded_badge_release, BadgeRelease,
    // #825 M3: the one unreleased class whose answer is a retry, not a reading.
    queuefull_readmit_gate,
    // #824: the missing half of flush_stranded_text's contract — what deliver_now
    // does when the flush DECLINES and our own text is still in the box.
    stranded_paste_guard, StrandedPasteGuard,
    StrandedAction, StrandedBlocker, STRANDED_SELFHEAL_MAX_HEALS,
    kickoff_recovery_action, KickoffDecline, KickoffRecovery,
    KICKOFF_REDELIVERY_MAX, KICKOFF_TURN_EVIDENCE_BYTES, await_cli_ready, ReadyWait,
    cli_ready_with_marker, ready_screen, ReadyMarker,
    redelivery_treatment, KickoffTreatment, stranded_reword,
    human_input_block, unconfirmed_disposition, HumanInputBlock, UnconfirmedDisposition,
    failed_arm_route, FailedArmRoute,
    box_reading, tier1_scan_bytes, BoxReading,
    // #583: the same read, measured — is `Unverifiable` the near-cap norm?
    Tier1ScanCensus,
    // #685: it was — so the window is sized in post-strip chars, by re-reading.
    Tier1Scan, TIER1_SCAN_WIDEN_ROUNDS,
    HUMAN_INPUT_BLOCK_BOUND_MS, UNCONFIRMED_ACK_SETTLE_MS, UNCONFIRMED_NOTICE_IDS_MAX,
    record_stranded_outcome_at_for_test,
    should_notify_unconfirmed, single_pane_autopilot_flags,
    spawn_opens_minimized,
    // #2126: pi's group-local session store, its exact-suffix lookup, the
    // repo-MCP exposure scanner, the store router, and the capability that
    // replaced three `cli == "claude"` re-derivations.
    pi_repo_mcp_exposure, pi_session_cwd_in_dir, pi_sessions_in, premints_session_id,
    session_cwd_in_store, PI_UNATTENDED_FLAGS,
    // #717: the attention scan's per-pane ring read, and the byte budget it is
    // bounded to — the whole of what that issue changes is the SIZE of this
    // request, so both have to be reachable from a test.
    attention_tail, ATTENTION_SCAN_BYTES,
    spawn_rate_exceeded, spawn_request_expired, strip_ansi, submit_confirmed, submit_sequence,
    blocking_ancestor, board_summaries, cap_task_notes, task_ready, task_summary, unmet_deps,
    // #1272/#1273: the derived board-level sprint, the agent-facing
    // projection both new fields had to be classified into, and the
    // closed link vocabulary the tool schema is pinned against.
    agent_task_view, current_sprint, TaskLink, TASK_LINK_TYPES,
    // #1273 PR B: the pure composer behind the kickoff `Grounding` section,
    // so the placement pin can rebuild the expected kickoff rather than
    // re-spelling the section as a literal that could drift from it.
    grounding_section,
    // #1349: the derived fingerprint of the three replace-wholesale arrays,
    // and the prefix its refusal opens with (which the board matches on).
    board_task, link_etag, STALE_LINK_ETAG_PREFIX,
    MAX_TASK_LINKS, MAX_TASK_LINK_TARGET, MAX_TASK_LINK_LABEL,
    // #3261: the description cap, which is REFUSED past rather than cut — so
    // the guard has to name the same number the refusal does.
    MAX_TASK_DESCRIPTION,
    TASK_STATUSES,
    // #1156: the strict Agile ladder, pinned against Rust literals here
    // (`the_ladder_table_is_pinned_on_the_rust_side`) and against the board's
    // copy from the OTHER side, by a guard that reads `ladder_rule`'s arms out
    // of mod.rs (`test/taskboard.test.ts`).
    ladder_rule, LadderRule, TASK_KINDS,
    // #865: done-row cap on list_tasks — the pure keep/drop rule and its default.
    filter_done_rows, LIST_TASKS_DONE_CAP,
    unconfirmed_delivery_notice, delivery_eaten_notice, watchdog_should_notify, worktree_cleanup_targets,
    AgentEntry, AgentRecord, AgentStatus, ApproveItem, AttentionItem, Caller, Containment, Delivery, DeliveryConfirmation, GroupInfo, Guardrails, HeldReason, HumanInput, KickoffOrigin, Launch, NameSource, OrchRegistry, PasteDecision, RetryGate,
    // #407: promotion of a standalone pane to orchestrator.
    promote_to_orchestrator_sync, PromoteConfig, SessionOrigin,
    PersonaInject, Task, TaskNote, TaskSummary,
    PasteGate, Role, TaskPatch, UsageSnapshot, CLAUDE_UNATTENDED_ALLOW, COPILOT_AUTOPILOT_CONFIRM_KEYS,
    COPILOT_GROUP_AUTOPILOT_FLAGS, COPILOT_UNATTENDED_FLAGS, MAX_ATTACHMENT_BYTES,
    PLANNER_READONLY_NOTE, SOLO_GROUP, solo_group_id, AUTOPILOT_DIALOG_WAIT, SOLO_AUTOPILOT_DIALOG_WAIT,
    CLAUDE_EDIT_DENY_TOOLS, CLAUDE_READONLY_DENY_GIT, KNOWN_CLAUDE_TOOLS,
    COPILOT_EDIT_DENY_TOOLS, COPILOT_READONLY_DENY_GIT, KNOWN_COPILOT_DENY_CATEGORIES,
    SUPPORTED_CLIS,
    // #946 Q4 / #1091 slice H: the role-keyed AskUserQuestion deny (orchestrator
    // + liaison) and its latched-attention belt.
    CLAUDE_QUESTION_DENY_TOOLS, claude_denies_interactive_question,
    // #267 stage 2: gemini as a reviewer-capable CLI, and the capability table
    // that decides which classes any CLI may host.
    cli_can_host, cli_caps, cli_extra_env, codex_profile_toml, gemini_policy_toml, CodexGitAccess,
    // #3318 F1: the per-CLI fork seam, the refusal a fork gesture asks, and
    // the L1 constant both line builders select their arm from.
    fork_refusal, ForkSeam, CLAUDE_FORK_PREMINTS_CHILD_ID,
    gemini_settings_json, CodexMcpAuth,
    CLI_CAPS, GEMINI_EDIT_DENY_TOOLS, GEMINI_READONLY_DENY_GIT, KNOWN_GEMINI_TOOLS,
    // #687: the per-block model knobs — the capability rows that decide which
    // CLI can honor one, and the query the launcher reads them through.
    cli_knobs_json, CONTEXT_VARIANTS, EFFORT_LEVELS,
};
use loomux_lib::gh;
use loomux_lib::pty::{phantom_gate_tick, PtyManager};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use loomux_lib::orchestration::humanq;
use loomux_lib::orchestration::needsyou;

// #3498 P1: this target was one 71k-line file; it is now a module tree with
// one test binary, so `cargo test -p orrerix --test orchestration <filter>`
// is unchanged. `helpers` comes first and carries `#[macro_use]` because
// `macro_rules!` is textually scoped. Every module opens with `use super::*`,
// which sees the imports above and the `pub(crate)` items the globs below
// re-export — the one module namespace the single file used to be.
#[macro_use]
mod helpers;
mod guards;
mod registry;
mod deliveryqueue;
mod launch;
mod tasks;
mod sessions;
mod mcp;
mod spawn;
mod reports;
mod autonomy;
mod compact;
mod mergegate;
mod fullautonomy;
mod attention;
mod holds;
mod janitor;
mod lifecycle;
mod promote;
mod verdicts;
mod ghpoller;
mod channels;
mod shims;
mod liveworkflow;
mod boxscan;
mod questions;
mod mqdriver;
mod composer;
mod humanquestions;
mod needsyoupanel;
mod board;
mod sprints;
mod clis;
mod usage;
mod comments;
mod providerlimits;
mod ghclose;
mod fork;
mod cacheage;

use helpers::*;
use registry::*;
use deliveryqueue::*;
use launch::*;
use tasks::*;
use sessions::*;
use mcp::*;
use autonomy::*;
use compact::*;
use mergegate::*;
use attention::*;
use holds::*;
use janitor::*;
use promote::*;
use verdicts::*;
use ghpoller::*;
use channels::*;
use shims::*;
use liveworkflow::*;
use boxscan::*;
use questions::*;
use humanquestions::*;
use needsyoupanel::*;
use usage::*;
use cacheage::*;
