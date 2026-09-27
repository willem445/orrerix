//! `Guardrails`, the per-group knobs, with their bounds and setters; the
//! notices each knob change sends; the max-agents notice debouncer; and the
//! consent-marker removal.
//! Design note: `docs/design/orchestration.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. IO: fs. Sibling
//! files it calls: `compactnudge.rs`.

use super::*;

/// Hard ceiling on `max_agents` regardless of what the launcher asks for.
pub(in crate::orchestration) const MAX_AGENTS_CEILING: u32 = 12;

/// One-line notice delivered to the orchestrator when the live-agent cap
/// changes mid-session, so it re-plans against the new ceiling (its kickoff
/// prompt still carries the old, already-rendered {{MAX_AGENTS}}).
pub fn max_agents_notice(from: u32, to: u32) -> String {
    format!("[orrerix] max live agents changed {from}→{to} — re-plan accordingly")
}

/// The idle-tick notice delivered to an autonomous group's orchestrator (#83):
/// names the `[orrerix] idle tick` wake source the template documents and tells it
/// to run its monitoring/intake cadence. Kept in one place so the template's wake
/// clause and the delivered text can't drift.
///
/// `intake_summary` is `Some` when the host-side intake gate (#332) is why this
/// tick fired WITH something new to report — `intake::intake_wake_summary`'s
/// already-sanitized text naming the issue/PR deltas it found. `None` covers
/// every OTHER reason the tick fired: a gate-disabled group (today's text,
/// unchanged), a bounded-fallback heartbeat, or an outstanding CI watch/watchdog
/// stall with no label/PR delta of its own — none of those hand the orchestrator
/// anything specific, so it still owes the full independent sweep.
///
/// **Two genuinely different messages, not one with a bolted-on addendum**
/// (rev-33 finding N1): the two cases ask for opposite things — "go poll/sweep
/// yourself" vs. "here's exactly what changed, act on it, don't re-poll" — and
/// appending the second onto the first produced a message that told the
/// orchestrator to do both in the same breath. The `Some` branch never mentions
/// polling labels or re-sweeping PRs at all; only re-sync (context may have
/// compacted) survives into it, since that's never redundant with a summary.
///
/// `summary_incomplete` (rev-33 finding N7) is `intake::PendingIntake::dropped_
/// any()` — true when the accumulated summary itself admits it dropped older
/// findings to stay bounded (`PendingIntake::render`'s own leading clause
/// points a human at "the intake-signal audit trail", but no MCP tool lets an
/// AGENT read the audit log — a dead end in a delivered prompt). When true,
/// the summary is routed to the SWEEP-bearing text instead of the "act on this
/// directly" one: a real sweep re-discovers everything regardless of what got
/// dropped, which is strictly better than trusting a summary that already
/// admits it's incomplete.
pub fn idle_tick_notice(intake_summary: Option<&str>, summary_incomplete: bool) -> String {
    match intake_summary.filter(|s| !s.is_empty()) {
        Some(s) if !summary_incomplete => format!(
            "[orrerix] idle tick: you have been idle and autonomous mode is on. The host-side \
             intake poll already found: {s} — act on that directly (spawn/drive the named work, \
             check the named PR) instead of re-polling labels or re-sweeping open PRs. Re-sync \
             first if your context may have compacted (list_tasks, list_agents, get_state). You \
             will get this at most once per idle window; producing any output resets the clock."
        ),
        _ => "[orrerix] idle tick: you have been idle and autonomous mode is on. Run your \
     monitoring cadence now — re-sync (list_tasks, list_agents, get_state), poll \
     for labeled intake (agent-ready / agent-investigation) and START that work, and \
     re-check your open PRs (CI + new comments). You will get this at most once per \
     idle window; producing any output resets the clock."
            .to_string(),
    }
}

/// One-line notice delivered to the orchestrator when the auto-merge gate is
/// toggled mid-session (#83), so it learns the new merge policy without waiting to
/// re-read its kickoff config.
pub fn auto_merge_notice(on: bool) -> String {
    if on {
        "[orrerix] auto-merge ENABLED for this group: you MAY now merge a PR yourself \
         once it has reviewer approval, green CI, and meets the issue's acceptance \
         criteria — audit and announce every merge, and still hold anything risky or \
         ambiguous for the human.".to_string()
    } else {
        "[orrerix] auto-merge DISABLED for this group: the human merge gate is absolute \
         again — open the PR, report it, and never merge yourself.".to_string()
    }
}

/// One-line notice delivered to the orchestrator when the auto-release gate is
/// toggled mid-session (#83), so it learns the new release policy without waiting
/// to re-read its kickoff config. Independent of auto-merge.
pub fn auto_release_notice(on: bool) -> String {
    if on {
        "[orrerix] auto-release ENABLED for this group: while autonomous you MAY now cut a \
         release yourself (`gh release …`, pushing a v* tag) once it is adequately \
         prepared — audit and announce every release, and still hold anything risky or \
         ambiguous for the human.".to_string()
    } else {
        "[orrerix] auto-release DISABLED for this group: publishing a release/tag now \
         requires an explicit human release grant again — do not `gh release` or push a \
         v* tag yourself; ask the human to grant it.".to_string()
    }
}

/// The cap, in characters, on a full-autonomy goal string (#778). The goal rides
/// in two places that are *typed into a CLI pane* — the toggle notice and the
/// orchestrator's kickoff config — so an unbounded paste would crowd out the
/// instructions it is only meant to qualify.
pub const MAX_FULL_AUTONOMY_GOAL_CHARS: usize = 500;

/// Normalize a full-autonomy goal (#778) into the single-line, bounded, paste-safe
/// form stored as the `full_autonomy` marker's content and echoed into the toggle
/// notice and the kickoff config. Empty or all-whitespace = no goal (`""`), which
/// every caller renders as "no goal set" rather than as an empty pair of quotes.
///
/// Loomux never *parses* the goal — what work is valuable is the orchestrator's
/// documented judgment, not policy in product code — so the only work here is
/// making the string safe to carry:
///
/// - every whitespace run collapses to ONE space, because both destinations are a
///   typed paste into a CLI pane where a newline submits the prompt early and
///   splits the instruction in half;
/// - control characters are dropped outright (an escape sequence in a goal would
///   reach a terminal verbatim);
/// - `[`/`]` are neutralized exactly as every other untrusted field in a
///   `[orrerix] …` notice is ([`notify::sanitize_gh_text`]), so a goal can never
///   forge a second notice row in the orchestrator's own pane;
/// - the result is capped by CHARACTERS (never bytes — a multibyte goal must not
///   truncate mid-codepoint) and never left ending in the space the cap landed on.
///
/// Idempotent, so re-sanitizing on read (the marker is a file a human can edit)
/// cannot keep eating the goal.
pub fn sanitize_full_autonomy_goal(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = 0usize;
    for ch in raw.chars() {
        if chars == MAX_FULL_AUTONOMY_GOAL_CHARS {
            break;
        }
        let c = if ch.is_whitespace() {
            ' '
        } else if ch.is_control() {
            continue;
        } else {
            match ch {
                '[' => '(',
                ']' => ')',
                other => other,
            }
        };
        // Collapse runs, and drop the leading one entirely (that is the trim).
        if c == ' ' && (out.is_empty() || out.ends_with(' ')) {
            continue;
        }
        out.push(c);
        chars += 1;
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Notice delivered to the orchestrator when full autonomy is toggled mid-session
/// (#778), so it learns that the start default inverted without waiting to re-read
/// its kickoff config. `goal` is normalized here rather than trusted from the
/// caller, so this function is safe to call with anything and the "a goal cannot
/// forge a notice row" property is a property of the *pure function*, testable on
/// its own.
///
/// The ON text is the whole protocol because there is nowhere else for it to live
/// at the moment the human flips the switch: what to do before touching the
/// pre-existing backlog (post one ranked triage plan and wait for an explicit go),
/// what the veto gesture is (`hold`, absolute), and — said out loud, because
/// this is the notice a reader will most want to over-read — that nothing about
/// merging, releasing, review or budgets moved. The toggle widens what may be
/// STARTED, never what may be SHIPPED.
///
/// `hold` is the group's RESOLVED veto spelling, threaded for the same reason the
/// template's `{{HOLD_LABEL}}` is (rev round 1 B1): this notice tells the
/// orchestrator which label to tell the human to strike rows with, so a hardcoded
/// `agent-hold` here would hand a renamed repo a veto gesture its own poller
/// ignores — the defect one surface over. Sanitized on the way in like every
/// other interpolated value: it is repo-authored text, and the parser's alphabet
/// restriction is a different layer's guarantee, not this function's.
pub fn full_autonomy_notice(on: bool, goal: &str, hold: &str) -> String {
    if !on {
        // OFF ignores whatever goal it is called with — off has none, by construction.
        return "[orrerix] full autonomy DISABLED for this group: the label funnel is opt-in \
                again — start only agent-ready / agent-investigation work. Finish what is \
                already in flight normally."
            .to_string();
    }
    let hold = sanitize_full_autonomy_goal(hold);
    let hold = if hold.is_empty() { builtin_hold_label() } else { hold };
    format!(
        "[orrerix] FULL AUTONOMY ENABLED for this group ({goal_clause}). Before starting any \
         pre-existing issue: post one ranked triage plan (value/risk/effort/order) over ALL \
         open issues as a GitHub issue, tell the human to veto rows by adding {hold}, and \
         wait for their go. After the go — and for any issue filed from now on that fits the \
         goal — self-select the highest-value eligible issue on each idle tick and start it \
         within your caps, announcing a one-line selection rationale per pickup. {hold} is \
         absolute. Nothing about merging, releasing, review, or budgets changed.",
        goal_clause = full_autonomy_goal_clause(goal),
    )
}

/// The built-in veto spelling, for the paths that must name one when a group's
/// own profile is unavailable or empty. One accessor rather than a literal
/// repeated at each such site.
pub(in crate::orchestration) fn builtin_hold_label() -> String {
    workflow::builtin_intake_profile().hold
}

/// The parenthesized goal fragment shared by the toggle notice and the kickoff
/// config clause, so the two can't drift: `goal: "…"` when there is one, and the
/// honest `no goal set` when there isn't — never an empty pair of quotes, which
/// reads as a goal that got lost rather than one that was never given.
pub(in crate::orchestration) fn full_autonomy_goal_clause(goal: &str) -> String {
    let goal = sanitize_full_autonomy_goal(goal);
    if goal.is_empty() {
        "no goal set".to_string()
    } else {
        format!("goal: \"{goal}\"")
    }
}

/// Notice delivered to the orchestrator when supervised dangerous mode is toggled
/// (#83), or force-cleared because autonomous mode was enabled (`by_autonomous`).
pub fn dangerous_mode_notice(on: bool, by_autonomous: bool) -> String {
    if on {
        "[orrerix] SUPERVISED DANGEROUS MODE enabled for this group: the human is present and \
         has authorized you to perform merges (to the default branch) and releases/tags \
         yourself, without a per-item grant. Audit and announce every merge/release; still \
         hold anything genuinely risky and flag it. This is a supervised session — the human \
         is watching.".to_string()
    } else if by_autonomous {
        "[orrerix] supervised dangerous mode was turned OFF because autonomous mode was enabled \
         (the two are mutually exclusive). Merge/release authority now follows the autonomous \
         auto-merge / auto-release toggles + grants.".to_string()
    } else {
        "[orrerix] supervised dangerous mode DISABLED for this group: the human gate is back — \
         open PRs and report, do not merge to the default branch or publish releases/tags \
         yourself unless the human grants it.".to_string()
    }
}

/// The notice delivered once when an autonomous group's token budget is exhausted
/// and idle-ticking is suspended (#83). Tokens, not dollars (see `usage.rs`).
pub fn autonomy_budget_notice(spent: u64, budget: u64) -> String {
    format!(
        "[orrerix] autonomy budget exhausted ({spent} of {budget} tokens spent since \
         autonomous mode was enabled) — autonomous mode has been SUSPENDED. Stop any \
         autonomous pulls and tell the human: raise the budget or toggle autonomous \
         mode back on to resume (re-enabling is explicit consent and re-anchors the \
         meter)."
    )
}

/// Quiet window a group's cap must fall silent for before its coalesced
/// cap-change notice is delivered. Rapid stepper clicks (#79) each persist,
/// enforce, and audit immediately, but the token-costing orchestrator notice
/// waits out this window and then spans the whole burst (first change's `from`
/// → last change's `to`), so a flurry of clicks is one prompt, not many.
pub(in crate::orchestration) const MAX_NOTICE_DEBOUNCE: Duration = Duration::from_secs(3);
/// How often the flusher loop checks for a debounced cap-change notice whose
/// window has elapsed. Well under `MAX_NOTICE_DEBOUNCE` so the delivered notice
/// lags the last click by at most a tick beyond the debounce.
pub(in crate::orchestration) const MAX_NOTICE_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// A cap-change notice awaiting its debounce window (#79). `from` is the cap
/// before the burst began — preserved across coalesced changes so the notice
/// reads end-to-end; `to` is the latest cap; `due_ms` is the Unix-ms at which,
/// absent any further change, the notice fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::orchestration) struct PendingMaxNotice {
    from: u32,
    to: u32,
    due_ms: u64,
}

/// Fold one cap change into the per-group debounce map (#79). A change that
/// lands while a notice is still pending keeps the original `from` (so the
/// coalesced notice spans the whole burst) and only advances `to` and pushes
/// the deadline out; the first change of a burst seeds a fresh entry. Pure, so
/// the coalescing is unit-testable without a clock or a live registry.
pub(in crate::orchestration) fn record_max_notice(
    pending: &mut HashMap<GroupId, PendingMaxNotice>,
    group: &GroupId,
    from: u32,
    to: u32,
    now: u64,
    debounce: Duration,
) {
    let due_ms = now.saturating_add(debounce.as_millis() as u64);
    pending
        .entry(group.clone())
        .and_modify(|p| {
            p.to = to;
            p.due_ms = due_ms;
        })
        .or_insert(PendingMaxNotice { from, to, due_ms });
}

/// Drain the notices whose debounce window has elapsed (`due_ms <= now`),
/// returning `(group, from, to)` for each that is a real net change. A burst
/// that nets back to where it started (e.g. 4→3→4) is dropped without a notice
/// — no orchestrator tokens spent announcing a no-op. Pure, so the flush
/// decision is unit-testable without sleeping out the debounce.
pub(in crate::orchestration) fn take_due_max_notices(
    pending: &mut HashMap<GroupId, PendingMaxNotice>,
    now: u64,
) -> Vec<(GroupId, u32, u32)> {
    let due: Vec<GroupId> = pending
        .iter()
        .filter(|(_, p)| p.due_ms <= now)
        .map(|(g, _)| g.clone())
        .collect();
    let mut out = Vec::new();
    for g in due {
        if let Some(p) = pending.remove(&g) {
            if p.from != p.to {
                out.push((g, p.from, p.to));
            }
        }
    }
    out
}

/// Upper bound on the idle-worker auto-kill timeout (24h); 0 disables it.
const MAX_IDLE_KILL_MINUTES: u32 = 1440;
/// Upper bound on the spawn-rate guardrail; 0 = unlimited.
const MAX_SPAWNS_PER_HOUR: u32 = 240;
/// Upper bound on the watchdog stall timeout (24h); 0 disables it.
const MAX_WATCHDOG_STALL_MINUTES: u32 = 1440;
/// Upper bound on the idle-tick quiet window (24h); a floor of 1 min is enforced in
/// `clamped()` (0 is treated as "unset" → default, not "disabled": the `autonomous`
/// marker is the on/off switch, so a 0 here must never silently stop ticking).
pub(in crate::orchestration) const MAX_IDLE_TICK_MINUTES: u32 = 1440;
/// Autonomous mode (#83): default per-tick pty-output growth (bytes) at or above
/// which the orchestrator counts as *actively working* — the growth resets the
/// quiet clock and the one-notice latch. Below it, growth is treated as idle
/// **repaint noise** (statusline/spinner frames keep `output_total` creeping while
/// the CLI is parked) and does NOT reset the clock, so an occasional repaint can't
/// starve the tick (the bug where a single stray byte demanded another full quiet
/// window). A coarse burst floor — there is no output-frame classifier — since a
/// real orchestrator turn dumps many KB while an idle repaint is a few hundred
/// bytes. **Default justified by measurement:** a full idle Claude Code input-box
/// render (box-drawing + ANSI) is ~164 bytes (`tests/fixtures/attention/
/// idle-input-box.txt`), so 2048 leaves ~12× headroom over a complete idle
/// repaint. Because this rides the exact wake+spend axis that already failed once
/// (finding 2) and a chattier CLI could exceed it, it is a **live-tunable guardrail
/// knob** (`Guardrails.idle_activity_floor_bytes`), not a bare const — see
/// `set_idle_activity_floor` / `idle_output_is_activity`.
pub(in crate::orchestration) const DEFAULT_IDLE_ACTIVITY_FLOOR_BYTES: u64 = 2048;
/// Upper bound on the activity floor (1 MiB): beyond this no real orchestrator turn
/// would clear it, so ticking would fire even while working. Floor is 1 (any growth
/// = activity, the original behavior) — both enforced in `clamped()`.
pub(in crate::orchestration) const MAX_IDLE_ACTIVITY_FLOOR_BYTES: u64 = 1024 * 1024;
/// #496 hardening: default bound (minutes) on how long human input ALONE may
/// defer the idle tick's quiet clock past the orchestrator's last REAL
/// output. Two independent safety heuristics keyed off the same signal — the
/// idle tick's quiet clock and delivery's stranded-text/retry suppression,
/// both deferred by "the pane has recent human input" — can deadlock each
/// other if that signal is ever wrong and never clears on its own: a copilot
/// orchestrator wedged exactly this way in #496, because xterm's own
/// automatic replies to the program's terminal queries (OSC colour, DA,
/// DSR/CPR, focus reports — see `pty.rs`'s `write_pty`) stamp
/// `last_user_input_ms` with no human present, forever, and only a physical
/// Enter recovered the group. PR-A of #496 fixes that proven mechanism at
/// the source (gates the stamp on `classify_human_input`); this bound is the
/// separate backstop for any refresher THAT fix doesn't model — a future
/// IME/composition path, a different CLI's TUI, anything not yet observed.
/// Past this many minutes of zero real output *and* zero printable input,
/// the tick fires anyway, audited with its own reason
/// (`IDLE_TICK_INPUT_DEFER_BOUND_REASON`) rather than silently — the exact
/// principle `idle_tick_skip_rearm_ms`'s doc states ("a suppressed window
/// must still come back on its own — nothing else will clear it") and
/// `USER_QUIET_MAX_HOLD`'s doc states ("deliver anyway ... never starve"),
/// applied here a third time to a third mechanism.
///
/// **Default 15 = 3x `DEFAULT_IDLE_TICK_MINUTES` (5).** This clamp is
/// **classification-blind by design** — a guarantee must not trust the very
/// classifier it exists to backstop — so it caps the raw timestamp
/// regardless of what produced it: real (Content-classified — see
/// `classify_human_input`) keystrokes are capped exactly the same as
/// pure-Neutral traffic byte-indistinguishable from an xterm auto-reply (PR-A's
/// tradeoff). Sustained genuine typing with zero real output for the full 15
/// minutes is capped too, and the tick fires mid-typing — this is rare in
/// practice (submitting produces a real output burst well inside the window,
/// which resets the clock), and where it isn't, the human-mid-work protection
/// is delivery's OWN hold (`USER_QUIET_MAX_HOLD`, the #420 state-based
/// question guard) rather than an exemption in this clamp; carving Content
/// out here would re-open the exact deadlock class this bound closes (#496:
/// the whole point is not trusting a classification of "this is really the
/// human"). 15 minutes of zero real output plus zero input at all — the
/// common case this bound actually exists for — is either a genuine wedge or
/// a human who has walked away; both are correct to tick for, and the tick is
/// a NOTICE, not an action (it doesn't merge, spend, or spawn anything by
/// itself), with `MAX_IDLE_TICKS_PER_HOUR` already backstopping runaway
/// firing if something re-triggers it repeatedly. Live-tunable per group
/// like its siblings (`Guardrails.idle_tick_input_defer_max_minutes`, 0 →
/// this default), floored at the group's own `idle_tick_minutes` (the bound
/// can never be tighter than the tick's own base quiet window — that would
/// make it fire BEFORE the ordinary threshold) and capped at
/// `MAX_IDLE_TICK_MINUTES` (24h, the same ceiling `idle_tick_minutes` uses).
pub(in crate::orchestration) const DEFAULT_IDLE_TICK_INPUT_DEFER_MAX_MINUTES: u32 = 15;
/// Compact-nudge (#287): upper bound on `compact_nudge_minutes` (24h); 0 is
/// "off", not clamped away — see `Guardrails::clamped`.
pub(in crate::orchestration) const MAX_COMPACT_NUDGE_MINUTES: u32 = 1440;
/// Compact-nudge (#328): upper bound on `compact_context_threshold_percent`;
/// 0 is "off" (see `Guardrails::clamped`), 100 = only at total exhaustion
/// (effectively disabled in practice, since the CLI's own emergency
/// auto-compact would already have fired by then).
pub(in crate::orchestration) const MAX_COMPACT_CONTEXT_THRESHOLD_PERCENT: u32 = 100;
/// Idle-tick intake gate (#332): upper bound on `intake_poll_minutes` when a
/// group sets an explicit value — mirrors `MAX_IDLE_TICK_MINUTES`. See
/// `Guardrails::intake_poll_minutes`'s doc for its tri-state semantics
/// (unset = smart default while autonomous, `Some(0)` = explicit opt-out).
pub(in crate::orchestration) const MAX_INTAKE_POLL_MINUTES: u32 = 1440;
/// Idle-tick intake gate (#332): default bounded fallback — an idle tick fires
/// unconditionally at least this often even with no intake signal, no pending
/// notification and no watchdog stall, so a poller bug (or a group that is
/// genuinely, permanently quiet) can never silence the orchestrator forever.
/// 3h, the middle of the issue's suggested 2-4h range.
pub(in crate::orchestration) const DEFAULT_IDLE_TICK_FALLBACK_MINUTES: u32 = 180;
/// Floor on the fallback cadence (30 min): below this it starts competing with
/// `idle_tick_minutes` itself rather than acting as a slow backstop.
pub(in crate::orchestration) const MIN_IDLE_TICK_FALLBACK_MINUTES: u32 = 30;
/// Ceiling on the fallback cadence (1 week): a bound that can be widened but
/// never disabled — see `intake_poll_minutes`'s doc for why this field, unlike
/// that one, is never allowed to mean "off".
pub(in crate::orchestration) const MAX_IDLE_TICK_FALLBACK_MINUTES: u32 = 60 * 24 * 7;
/// Idle-tick fallback backoff (#864): default ceiling on the EFFECTIVE
/// fallback interval once a group has been delta-free for several consecutive
/// fallback wakes — the interval doubles per empty wake and stops here
/// (`intake::fallback_interval_minutes`).
///
/// **24h**, so with the 3h base a fully parked group settles at 3h → 6h → 12h
/// → 24h and then one wake per day: a parked weekend costs ~4 wakes instead of
/// the ~16 the fixed cadence charged (the #864 evidence measured ~30 over one
/// weekend, at 1–2 API turns over the orchestrator's whole prefix each). A day
/// is also the longest silence that still keeps the fallback's own promise
/// legible to a human: whatever the host-side poll misses, the orchestrator is
/// still woken unconditionally within a day of it.
///
/// Set this equal to `idle_tick_fallback_minutes` to switch backoff off and
/// get the pre-#864 fixed cadence back — one value, rather than a separate
/// enable flag.
pub(in crate::orchestration) const DEFAULT_IDLE_TICK_FALLBACK_MAX_MINUTES: u32 = 60 * 24;

/// The value of a block knob (`effort:` / `context:`) this CLI will actually
/// honor, or **empty** — which every emit path reads as "say nothing", i.e.
/// the pre-#687 command line byte for byte.
///
/// This is the silent half of the pair: `parse_workflow` rejects an
/// unsupported knob loudly, with the author present to read the error;
/// `Guardrails::clamped` calls this for the other way a knob can arrive — a
/// hand-edited `group.json`, which never met the parser and has no human to
/// show anything to. Same belt-and-braces, and the same fail-to-default
/// direction, as `sanitize_model`.
pub fn clamped_knob(allowed: &[&str], v: &str) -> String {
    let want = v.trim().to_ascii_lowercase();
    if allowed.contains(&want.as_str()) {
        want
    } else {
        String::new()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Guardrails {
    pub max_agents: u32,
    /// Group-default agent CLI (one of `SUPPORTED_CLIS`).
    /// A block's own `cli` overrides it; an empty block `cli` inherits this.
    /// Kept as the group default so old group.json (pre per-role CLI) and the
    /// launcher's single-CLI path both keep working (issue #4).
    pub agent_cli: String,
    /// **The agent roster, as data (#222).** This replaced the eight flat
    /// per-role fields (`worker_cli`, `reviewer_model`, …): a group's agents are
    /// now a list of [`workflow::Block`]s, each with its own id, capability
    /// class (`kind`), CLI, model and persona. Read from
    /// `<repo>/.loomux/workflow.yml` when the repo declares one; otherwise
    /// [`workflow::default_roster`] synthesizes today's fixed 4-block roster
    /// from the launcher's per-role picks, so a repo with no workflow file
    /// behaves exactly as it did before blocks existed.
    ///
    /// Empty is legal only transiently: `clamped()` fills it with the built-in
    /// roster, so no code downstream has to handle an agent-less group.
    pub blocks: Vec<workflow::Block>,
    /// **The advanced-orchestrator toggle (#222).** Off — the default, and what
    /// every group.json written before this field existed means — is today's
    /// experience byte for byte: `<repo>/.loomux/workflow.yml` is not read, not
    /// validated and not obeyed, and `blocks` stays the roster the launcher's
    /// per-role picks synthesized. On, the repo's workflow file is loaded in
    /// `create_group` and *its* blocks become the roster.
    ///
    /// The toggle exists because a workflow file is repo-authored input that
    /// arrives with a `git clone`. Letting one take effect merely by being
    /// present would mean cloning a repo could change which agents a human's
    /// group runs, with which personas, before they had ever seen the file — so
    /// the human opts in per launch, having been shown the roster it resolves to.
    ///
    /// A *launch* choice, not a live one: it is persisted with the group so a
    /// resumed orchestration comes back with the roster it was launched with.
    pub advanced_orchestrator: bool,
    /// **Which workflow this group runs** (#1689). The name of a file under
    /// `.orrerix/workflows/`, or [`workflow::DEFAULT_WORKFLOW_NAME`] — which is
    /// `.orrerix/workflow.yml`, and which is what a `group.json` written before
    /// this field existed reads as. A repo with one workflow file therefore
    /// behaves byte-for-byte as it did before named workflows: the name resolves
    /// to the same path, and nothing under `workflows/` is ever opened.
    ///
    /// **Pinned with the roster, in the same atomic write, and for the same
    /// reason.** The blocks in `blocks` and the name of the file they came from
    /// are one fact about a group; a name that lived anywhere else could
    /// disagree with the roster after a crash, and then nothing on disk would
    /// say which of the two the human had consented to.
    ///
    /// A [`workflow::WorkflowName`] rather than a `String`, so the value cannot
    /// reach [`workflow::workflow_path_named`] unvalidated — the compiler, not a
    /// scan, is what stops a hand-edited `group.json` from naming
    /// `../../../etc/x` (CLAUDE.md constraint 6). `load_group_file` is the one
    /// place a persisted string becomes one, and an unusable value there falls
    /// back to `default` rather than failing the load: a group must stay
    /// rejoinable.
    pub workflow: workflow::WorkflowName,
    /// Additionally pre-approve `git`/`gh` shell commands for the group's
    /// agents. Never maps to `--dangerously-skip-permissions`: bypass mode
    /// shows a confirm dialog whose default answer is "exit", which the
    /// kickoff typing would accept, killing the pane.
    pub auto_ops: bool,
    /// Cost guardrail: auto-kill a worker/reviewer that has sat without a
    /// task for this many minutes (the orchestrator is notified so it can
    /// respawn on demand). 0 disables it. See `idle_should_kill`.
    pub idle_kill_minutes: u32,
    /// Cost guardrail: cap on worker/reviewer spawns per rolling hour, a
    /// runaway-orchestrator backstop. 0 = unlimited. See `spawn_rate_exceeded`.
    pub max_spawns_per_hour: u32,
    /// Recovery guardrail: nudge the orchestrator once when a working agent
    /// produces no terminal output and sends no report for this many minutes
    /// (likely stalled or waiting on input). 0 disables it. See
    /// `watchdog_should_notify`.
    pub watchdog_stall_minutes: u32,
    /// Autonomous mode cost cap (#83): the token budget an autonomous group may
    /// spend *after* autonomous mode is enabled before idle ticking is suspended
    /// and the human is notified. Metered as the delta from the usage snapshot
    /// captured at enable time (the `autonomous` marker's content) — see
    /// `enforce_autonomy_budgets`. Tokens, not dollars: subscription/Max accounts
    /// pay $0 marginal, so tokens are the honest metric (see `usage.rs`). 0 =
    /// no cap. Persisted in group.json, live-settable via `set_autonomy_budget`.
    pub autonomy_budget_tokens: u64,
    /// Autonomous mode idle-tick quiet window in minutes (#83): how long the
    /// orchestrator pane must be output-quiet before an idle tick fires. 0 = unset
    /// → `DEFAULT_IDLE_TICK_MINUTES`; clamped to `1..=MAX_IDLE_TICK_MINUTES` (the
    /// `autonomous` marker, not this, is the on/off switch). Persisted in
    /// group.json, live-settable via `set_idle_tick_minutes` so the human can drop
    /// it to 1–2 min to verify quickly. See `idle_tick_should_fire`.
    pub idle_tick_minutes: u32,
    /// Autonomous mode idle-tick activity floor in bytes (#83): per-tick pty-output
    /// growth at/above which the orchestrator counts as working (resets the quiet
    /// clock); sub-floor growth is idle repaint noise. 0 = unset →
    /// `DEFAULT_IDLE_ACTIVITY_FLOOR_BYTES`; clamped to
    /// `1..=MAX_IDLE_ACTIVITY_FLOOR_BYTES`. Live-settable via
    /// `set_idle_activity_floor` so a chattier CLI whose idle repaints exceed the
    /// default has a runtime remedy (rev-59). See `idle_output_is_activity`.
    pub idle_activity_floor_bytes: u64,
    /// #496 hardening: bound (minutes) on how long human input ALONE may defer
    /// the idle tick past the orchestrator's last REAL output-based progress —
    /// see `DEFAULT_IDLE_TICK_INPUT_DEFER_MAX_MINUTES`'s doc for the deadlock
    /// this closes (two independent heuristics keyed off the same fallible
    /// "pane has human input" signal) and why the default is 15. 0 = unset →
    /// the default; clamped to `idle_tick_minutes..=MAX_IDLE_TICK_MINUTES` in
    /// `clamped()` — floored at the group's own tick window (never tighter
    /// than the ordinary threshold), capped at 24h. No live setter this round
    /// (same precedent as `context_window_tokens_override`): set at launch or
    /// by hand-editing group.json; this PR's contract is the persisted field
    /// plus the bounded guarantee, not a new UI control. See `idle_tick_tick`.
    pub idle_tick_input_defer_max_minutes: u32,
    /// Compact-nudge (#287): how long an eligible pane must be idle at its input
    /// prompt (output-quiet, the SAME clock `idle_tick_minutes` reads — see
    /// `compact_nudge_tick`) before loomux pastes `/compact` for it. Unlike
    /// `idle_tick_minutes`, 0 here means the feature is OFF — there is no
    /// separate on/off marker for compact-nudge, so this single field is both
    /// the switch and the interval, mirroring `watchdog_stall_minutes` /
    /// `idle_kill_minutes`. Persisted in group.json, live-settable via
    /// `set_compact_nudge_minutes`. Conservative default: off.
    pub compact_nudge_minutes: u32,
    /// Compact-nudge (#287): which capability classes are eligible for the
    /// automatic `/compact` nudge, as lowercase role names (`Role::as_str`).
    /// `clamped()` drops unrecognized entries and falls back to
    /// `["orchestrator"]` if that leaves it empty — workers/reviewers are
    /// short-lived, so the orchestrator (the one pane whose lifetime cache-read
    /// volume this feature exists to cut) is the sane default. Persisted in
    /// group.json, live-settable via `set_compact_nudge_roles`.
    pub compact_nudge_roles: Vec<String>,
    /// Compact-nudge min-context floor (benchtest finding, smart-default
    /// round): the HEURISTIC (lull-timer) fire additionally requires the
    /// agent's last context-percent reading to be at/above a floor — see
    /// `compact_nudge_context_floor_met`, which is where all three states
    /// below actually resolve (deliberately NOT resolved here in `clamped()`,
    /// so a group that turns `compact_nudge_minutes` on LATER via a live
    /// setter still gets the smart default immediately, with no re-launch).
    /// Never applied to an agent's own `request_compact` (always honored
    /// regardless of context%): this is a floor on loomux's *unprompted*
    /// judgment, not the agent's. Tri-state, `Option<u32>` (the same "let a
    /// real value distinguish from absence" idiom `context_window_tokens_
    /// override` already uses in this struct):
    /// - `None` (unset — absent from group.json, or never explicitly set):
    ///   the **smart default** applies automatically — `DEFAULT_COMPACT_
    ///   NUDGE_MIN_CONTEXT_PERCENT` (50) whenever `compact_nudge_minutes >
    ///   0`, otherwise inert (the parent feature is off, so there is nothing
    ///   to gate). Zero setup: a group that enables the quiet-window alone
    ///   already gets the floor a live benchtest showed was needed.
    /// - `Some(0)`: explicitly disabled — fire on the lull alone, no context
    ///   check at all (today's pre-smart-default behavior, preserved as an
    ///   explicit opt-out).
    /// - `Some(n)`, `n > 0`: an explicit floor, clamped to `1..=100`.
    /// Persisted in group.json (as `null`/absent for `None`, an integer
    /// otherwise), live-settable via `set_compact_nudge_min_context_percent`
    /// (which always sets an explicit `Some`, never restores `None` — once a
    /// human has touched the control, that IS the explicit choice).
    pub compact_nudge_min_context_percent: Option<u32>,
    /// Compact-nudge (#328): percent of the Claude context window
    /// (`effective_context_window_tokens`, round 7: model-aware, was a flat
    /// constant) an eligible agent's context must cross
    /// before loomux escalates — delivering `compact_escalation_notice` and, if
    /// the agent doesn't self-request within the same pass, marking the compact
    /// requested on its behalf (better a loomux-timed compact with the
    /// escalation warning already delivered than the CLI's own 100% emergency
    /// auto-compact with zero offload). `0` = off. Fresh launcher-created
    /// groups use `DEFAULT_COMPACT_CONTEXT_THRESHOLD_PERCENT`; a missing
    /// persisted key uses that same value, while a stored zero stays off.
    /// Persisted in group.json and live-settable via
    /// `set_compact_context_threshold`.
    pub compact_context_threshold_percent: u32,
    /// Production bug fix (PR #329 round 7): an explicit human override for
    /// the context-window size (tokens) this group's escalation percent and
    /// lifecycle-panel display are computed against — takes absolute
    /// precedence over `usage::claude_context_window_tokens`'s model-based
    /// guess (see `effective_context_window_tokens`). Live evidence showed
    /// that guess can be wrong (Claude's actual context tier is ultimately a
    /// per-request API setting the transcript doesn't fully pin down): this
    /// is the escape hatch for a deployment where it is. `None` (the
    /// conservative default) defers entirely to the guess. Persisted in
    /// group.json; no live setter this round (set at launch, or by hand-
    /// editing group.json for an existing group — same precedent as
    /// `max_spawns_per_hour`, which is also create-time-only today).
    pub context_window_tokens_override: Option<u64>,
    /// **The resolved intake profile (#382 P1).** One source of truth for
    /// "what counts as intake" — the built-in `github-labels` default unless
    /// the repo declared an `intake:` block AND the advanced-orchestrator
    /// toggle is on for a **fresh** launch, mirroring exactly how `blocks`
    /// resolves (see [`Launch`], `create_group_ex`). Persisted in group.json
    /// so a resumed group is pinned to the profile its human approved, and so
    /// every consumer that needs "what is an intake signal" — the template
    /// renderer, `gh.rs`'s label allow-list, `idle_tick_notice()` — reads the
    /// same value.
    ///
    /// **Only PARTLY wired to the #332 host poller (rev-33 finding, #429):**
    /// the P1 comment above once claimed the poller was a consumer; for the
    /// intake labels it still isn't — `intake::INTAKE_LABELS` is a hardcoded
    /// `["agent-ready", "agent-investigation"]` const, not read from this
    /// field. That was a low-stakes gap while the gate shipped default-off
    /// (#429's smart default hadn't landed, so almost no group actually
    /// engaged the poller); it is a real one now that the gate is ON by
    /// default for every autonomous group — a repo with a custom `intake:`
    /// profile (different labels) gets a poller silently checking the WRONG
    /// ones, never finding its own custom-labeled intake. TODO(#382 P2): wire
    /// `poll_intake`/`label_deltas` to read this field's resolved labels
    /// instead of the hardcoded const.
    ///
    /// `hold` is the exception, wired from the start (#778): `poll_intake`
    /// reads it from here for every full-autonomy eligibility check. Deliberate
    /// — it is a **consent boundary** (the human's veto over what may be
    /// started), and repeating the hardcoded-const gap on one of those would
    /// mean a repo that renamed the label gets its vetoes silently ignored.
    ///
    /// **And "wired" means every surface that names the veto, not just this
    /// one.** The first cut wired only the poller, which was worse than not
    /// supporting the rename at all: the orchestrator builds its triage plan
    /// from its own `gh issue list` sweep, so a contract still naming
    /// `agent-hold` put a held issue into the plan the human then approved.
    /// The spelling now also reaches the contract (the `{{HOLD_LABEL}}`
    /// template variable, rendered from this field) and the issues-view toggle
    /// with `gh.rs`'s label allow-list — which read **this field** whenever the
    /// calling pane has a group, and the repo's `default` workflow file only
    /// when it does not (#2663; the issues view can be open on a plain pane).
    /// So for a pane inside a group there is one resolution rather than two.
    /// See `docs/design/orchestration.md`'s full-autonomy section for the
    /// no-group arm and for which way the drift case points.
    ///
    /// Available regardless of the toggle: autonomous mode can run with the
    /// built-in roster, so a consumer must always have a profile to read, not
    /// just when the advanced orchestrator is in play.
    pub intake: workflow::IntakeProfile,
    /// Idle-tick intake gate (#332): how often the host-side, zero-token
    /// poller (`gh issue list` / `gh pr list`, no LLM turn) checks this
    /// group's intake signals (new/changed `agent-ready`/`agent-investigation`
    /// labels, open-PR check-state transitions) since it last looked.
    ///
    /// **Smart-defaulted while autonomous (#429, user-directed): tri-state,
    /// `Option<u32>`** — the same "let a real value distinguish from absence"
    /// idiom `compact_nudge_min_context_percent`/`context_window_tokens_
    /// override` already use in this struct. Resolved fresh wherever it's
    /// consulted (`intake::effective_intake_poll_minutes`), never here in
    /// `clamped()`, so a group that flips autonomous mode ON gets the gate
    /// immediately with no re-launch:
    /// - `None` (unset — absent from group.json, or never explicitly set):
    ///   the gate is ON at `DEFAULT_INTAKE_POLL_MINUTES` whenever the group is
    ///   autonomous — a live testbed benchtest (#429) found the gate shipping
    ///   default-OFF meant it could never actually engage for any real group
    ///   (no setter/UI ever set this field above 0), defeating #332's whole
    ///   economy-by-default purpose. `0`-equivalent (inert) while supervised:
    ///   intake polling only ever matters for an autonomous group's idle tick.
    /// - `Some(0)`: explicit opt-out — the gate stays off even while
    ///   autonomous, for an operator who wants #332's polling load off
    ///   deliberately. There is no live setter for this field (same
    ///   precedent as `context_window_tokens_override`): the escape hatch is
    ///   hand-editing group.json.
    /// - `Some(n)`, `n > 0`: an explicit cadence, clamped to
    ///   `1..=MAX_INTAKE_POLL_MINUTES`.
    ///
    /// This REVERSES the field's original migration-safety stance (an
    /// upgraded group's pre-#429 `group.json`, with no such key, used to
    /// decode to `0`/off byte-for-byte) — deliberately: the #429 benchtest
    /// showed byte-for-byte invisibility was itself the defect, and the user
    /// directed the smart default over preserving it. See `idle_tick_gate` /
    /// `OrchRegistry::poll_intake`.
    pub intake_poll_minutes: Option<u32>,
    /// Idle-tick intake gate (#332): the bounded unconditional fallback — an
    /// idle tick fires regardless of the gate at least this often, so a
    /// poller bug or a permanently-quiet group can never silence the
    /// orchestrator past it. Only meaningful while `intake_poll_minutes > 0`;
    /// normalized regardless, like `idle_tick_minutes`: `0` = unset →
    /// `DEFAULT_IDLE_TICK_FALLBACK_MINUTES`, then clamped to
    /// `MIN_IDLE_TICK_FALLBACK_MINUTES..=MAX_IDLE_TICK_FALLBACK_MINUTES` — this
    /// field, unlike `intake_poll_minutes`, is never allowed to mean "off",
    /// because it's the backstop *for* that gate. See
    /// `intake::idle_tick_fallback_due`.
    pub idle_tick_fallback_minutes: u32,
    /// Idle-tick fallback backoff (#864): the ceiling the fallback interval
    /// backs off TO while a group stays delta-free. The effective interval
    /// doubles per consecutive delta-free fallback wake
    /// (`intake::fallback_interval_minutes`) and stops here; any delta or
    /// human input resets it to `idle_tick_fallback_minutes`.
    ///
    /// Normalized like `idle_tick_fallback_minutes` itself: `0` = unset →
    /// `DEFAULT_IDLE_TICK_FALLBACK_MAX_MINUTES`, then clamped to
    /// `idle_tick_fallback_minutes..=MAX_IDLE_TICK_FALLBACK_MINUTES` —
    /// floored at the base (the backoff may only ever make the backstop
    /// slower, never quicker) and capped by the same never-disableable
    /// ceiling. Setting it EQUAL to `idle_tick_fallback_minutes` is the
    /// explicit opt-out: no backoff, the pre-#864 fixed cadence. There is no
    /// live setter (same precedent as `idle_tick_fallback_minutes` itself);
    /// the escape hatch is hand-editing `group.json`.
    pub idle_tick_fallback_max_minutes: u32,
}

impl Guardrails {
    #[doc(hidden)] // pub for integration tests (unit tests can't load the UI stack; see tests/smoke.rs)
    pub fn clamped(mut self) -> Self {
        self.max_agents = self.max_agents.clamp(1, MAX_AGENTS_CEILING);
        // The group default CLI is coerced to a supported value (legacy /
        // single-CLI path). Per-role CLIs are validated at spawn instead of
        // coerced here, so a genuinely unknown per-role type is rejected
        // rather than silently downgraded (issue #4).
        if !SUPPORTED_CLIS.contains(&self.agent_cli.as_str()) {
            self.agent_cli = "claude".into();
        }
        // An empty roster means "nobody said otherwise" — the launcher's plain
        // path, a legacy group.json, a `Guardrails::default()`. Fill it with the
        // built-in 4-block roster so every downstream lookup finds a block.
        if self.blocks.is_empty() {
            self.blocks = workflow::builtin_roster(&self.agent_cli);
        }
        // ── roster normalization, in order; each step depends on the last ──
        //
        // Steps 1-3 are defensive: `parse_workflow` already enforces all of them
        // and *tells the author which line is wrong*. They are re-enforced here
        // (silently — there is no author present) because a roster can also arrive
        // from a hand-edited group.json, which never meets the parser.

        // 1. Ids are shell tokens and file names. An unusable one would mint an
        //    agent id like `w-` and write `.md`. Fall back to the class name
        //    rather than dropping the block — a roster with a hole is worse than
        //    one with a plainly-named block.
        for b in &mut self.blocks {
            b.id = workflow::sanitize_id(&b.id).unwrap_or_else(|| b.kind.as_str().to_string());
        }
        // 2. The FIVE class names are RESERVED as ids for their own class (#1161
        //    added `manager`). An `id: planner, kind: reviewer` block would write
        //    its contract to `reviewer.md` — the real reviewer's file (see
        //    `workflow::Block::instructions_file`) — and clobber it.
        //
        //    This step reads `kind_from_str`, so widening that function widens
        //    this drop: an already-persisted `group.json` carrying
        //    `id: manager, kind: worker` — legal to write before #1161 — now
        //    loses that block silently here, where `parse_workflow` would refuse
        //    the whole file. Both are the intended treatment of an id that has
        //    become a class name; see the parser's arm for the argument.
        self.blocks
            .retain(|b| workflow::kind_from_str(&b.id).is_none_or(|reserved| reserved == b.kind));
        // 3. Ids are unique. A duplicate makes `block(id)` resolve to whichever
        //    came first and leaves the other permanently unreachable.
        let mut seen: HashSet<String> = HashSet::new();
        self.blocks.retain(|b| seen.insert(b.id.clone()));
        // 4. Every group has exactly one orchestrator, and it is structural — it
        //    is the pane the human talks to. A workflow file that declares only
        //    the agents it cares about (three reviewers, a worker) must not leave
        //    the group without one. This is the only block loomux adds on the
        //    repo's behalf, and it grants nothing the file didn't already have:
        //    a group with no orchestrator cannot run at all.
        //
        //    Step 2 is what makes this safe to prepend — the id `orchestrator` can
        //    only belong to an orchestrator-kind block, so "no orchestrator kind"
        //    implies "no `orchestrator` id", and this cannot mint a duplicate.
        if !self.blocks.iter().any(|b| b.kind == Role::Orchestrator) {
            let mut roster = workflow::default_roster(&[(Role::Orchestrator, &self.agent_cli, "")]);
            roster.append(&mut self.blocks);
            self.blocks = roster;
        }
        for b in &mut self.blocks {
            b.name = workflow::sanitize_display(&b.name);
            if b.name.is_empty() {
                b.name = b.id.clone();
            }
            // A block CLI is validated at spawn rather than coerced here, so a
            // genuinely unknown one is rejected loudly instead of silently
            // downgraded (issue #4). Only the *effective* model is normalized:
            // Copilot picks its own best model with "auto"; Claude needs a tier.
            let cli = if b.cli.trim().is_empty() { self.agent_cli.clone() } else { b.cli.clone() };
            b.model = sanitize_model(&b.model, default_model(&cli, b.kind));
            // #687: the model knobs, against the block's RESOLVED CLI — which
            // is why this can't live in the parser alone: a block that
            // inherits the group default has no CLI to check against until
            // here. A value the CLI cannot honor becomes empty, i.e. no flag
            // and no model suffix; see `clamped_knob` for why that half is
            // silent while the parser's is loud.
            let caps = cli_caps(&cli);
            b.effort = clamped_knob(caps.map_or(&[][..], |c| c.effort_levels), &b.effort);
            b.context = clamped_knob(caps.map_or(&[][..], |c| c.context_variants), &b.context);
        }
        self.idle_kill_minutes = self.idle_kill_minutes.min(MAX_IDLE_KILL_MINUTES);
        self.max_spawns_per_hour = self.max_spawns_per_hour.min(MAX_SPAWNS_PER_HOUR);
        self.watchdog_stall_minutes = self.watchdog_stall_minutes.min(MAX_WATCHDOG_STALL_MINUTES);
        // 0 = unset → default (not "off"); then floor at 1 so ticking never
        // silently stops while autonomous — the marker is the on/off switch.
        if self.idle_tick_minutes == 0 {
            self.idle_tick_minutes = DEFAULT_IDLE_TICK_MINUTES;
        }
        self.idle_tick_minutes = self.idle_tick_minutes.clamp(1, MAX_IDLE_TICK_MINUTES);
        // #496: 0 = unset → default (3x the tick window), then floored at
        // `idle_tick_minutes` itself — computed AFTER the line above, so this
        // is always the group's REAL, already-normalized tick window, never a
        // 0 that would let the bound fire before the ordinary threshold.
        if self.idle_tick_input_defer_max_minutes == 0 {
            self.idle_tick_input_defer_max_minutes = DEFAULT_IDLE_TICK_INPUT_DEFER_MAX_MINUTES;
        }
        self.idle_tick_input_defer_max_minutes =
            self.idle_tick_input_defer_max_minutes.clamp(self.idle_tick_minutes, MAX_IDLE_TICK_MINUTES);
        // 0 = unset → default; floored at 1 (any growth = activity) so it can never
        // be a no-op that treats real bursts as noise.
        if self.idle_activity_floor_bytes == 0 {
            self.idle_activity_floor_bytes = DEFAULT_IDLE_ACTIVITY_FLOOR_BYTES;
        }
        self.idle_activity_floor_bytes =
            self.idle_activity_floor_bytes.clamp(1, MAX_IDLE_ACTIVITY_FLOOR_BYTES);
        // Compact-nudge (#287): 0 stays 0 (off) — only cap a too-large value.
        self.compact_nudge_minutes = self.compact_nudge_minutes.min(MAX_COMPACT_NUDGE_MINUTES);
        // Unrecognized role names (typos, a stale/future value) are dropped
        // rather than rejected — a hand-edited group.json with a bad entry
        // should not crash the group, just silently not grant that role
        // eligibility. See `canonicalize_compact_nudge_roles` for why survivors
        // are also case-normalized, not just filtered.
        self.compact_nudge_roles = canonicalize_compact_nudge_roles(self.compact_nudge_roles);
        // Min-context floor: `None` (unset) is left untouched here on purpose
        // — the smart default resolves at gate-evaluation time
        // (`compact_nudge_context_floor_met`), not here, so a group that
        // enables `compact_nudge_minutes` LATER via a live setter still gets
        // it without a re-launch. Only an explicit `Some` gets capped.
        self.compact_nudge_min_context_percent = self.compact_nudge_min_context_percent.map(|p| p.min(100));
        // Compact-nudge (#328): 0 stays 0 (off) — only cap a too-large value.
        self.compact_context_threshold_percent =
            self.compact_context_threshold_percent.min(MAX_COMPACT_CONTEXT_THRESHOLD_PERCENT);
        // #429: tri-state, same shape as `compact_nudge_min_context_percent`
        // just above — `None` (smart default) and `Some(0)` (explicit
        // opt-out) both pass through untouched; only an explicit nonzero
        // value gets clamped into range. The smart default itself resolves
        // in `intake::effective_intake_poll_minutes` at gate-evaluation time,
        // not here, so a group that flips autonomous ON later gets the gate
        // immediately with no re-normalization pass needed.
        self.intake_poll_minutes = self.intake_poll_minutes.map(|explicit| {
            if explicit == 0 { 0 } else { explicit.clamp(1, MAX_INTAKE_POLL_MINUTES) }
        });
        // The fallback, by contrast, follows the idle_tick_minutes idiom
        // exactly: 0 = unset → default, then clamped — this backstop must
        // never be configurable to "never fire".
        if self.idle_tick_fallback_minutes == 0 {
            self.idle_tick_fallback_minutes = DEFAULT_IDLE_TICK_FALLBACK_MINUTES;
        }
        self.idle_tick_fallback_minutes =
            self.idle_tick_fallback_minutes.clamp(MIN_IDLE_TICK_FALLBACK_MINUTES, MAX_IDLE_TICK_FALLBACK_MINUTES);
        // #864: the backoff ceiling, floored at the base — computed AFTER the
        // two lines above, so the floor is the group's REAL, already-normalized
        // fallback rather than a 0 that would let `fallback_interval_minutes`
        // read a cap below its own base (it defends against that anyway; this
        // is the same "normalize once, at the edge" discipline
        // `idle_tick_input_defer_max_minutes` follows against `idle_tick_minutes`).
        if self.idle_tick_fallback_max_minutes == 0 {
            self.idle_tick_fallback_max_minutes = DEFAULT_IDLE_TICK_FALLBACK_MAX_MINUTES;
        }
        self.idle_tick_fallback_max_minutes = self
            .idle_tick_fallback_max_minutes
            .clamp(self.idle_tick_fallback_minutes, MAX_IDLE_TICK_FALLBACK_MINUTES);
        self
    }

    /// A block by id. The block *is* the agent's identity (#222) — edges,
    /// gates, `spawn_agent(block:)` and the roster all reference this.
    pub fn block(&self, id: &str) -> Option<&workflow::Block> {
        self.blocks.iter().find(|b| b.id == id)
    }

    /// The **default block for a capability class** — the first block of that
    /// kind in roster order.
    ///
    /// This is the bridge that kept the ~72 `Role::` sites compiling: code that
    /// used to ask "what CLI does the reviewer run?" now asks "what CLI does the
    /// *default reviewer block* run?". With the built-in roster there is exactly
    /// one block per class, so the answer is unchanged. With a custom workflow
    /// declaring three reviewers, this is the one an orchestrator gets when it
    /// spawns `kind: reviewer` without naming a block — the others are opt-in by
    /// id, which is deliberate: a roster must not silently change what a plain
    /// `spawn_agent(kind: reviewer)` does.
    ///
    /// **A liaison is skipped for a reviewer-kind resolution (#891 S4).** It is
    /// reviewer-KIND and reviews nothing, so "the first block of that kind"
    /// answered a plain `spawn_agent(kind: "reviewer")` with the human's pane
    /// whenever a roster happened to declare its liaison first — a reviewer-
    /// instructed pane denied `review_verdict`, unable to satisfy the gate it
    /// was spawned for. The predicate is `workflow::is_reviewing_block`, the
    /// same one the `{{REVIEWERS}}` fan-out and a reviewer's "one of N" lane
    /// ask (S3), so "which blocks review" has ONE answer across the surfaces
    /// that mean it.
    ///
    /// A roster whose only reviewer-kind block IS the liaison therefore
    /// resolves to `None` rather than to the liaison, and every caller here
    /// fails closed on that: the spawn paths refuse with a message naming the
    /// liaison, `cli_for`/`model_for` fall back to the group defaults. Naming
    /// the block explicitly (`spawn_agent(block: …)`) is unaffected — this
    /// resolves a CLASS to its default, and the liaison is never a class's
    /// default.
    pub fn block_for(&self, kind: Role) -> Option<&workflow::Block> {
        self.blocks.iter().find(|b| match kind {
            Role::Reviewer => workflow::is_reviewing_block(b),
            _ => b.kind == kind,
        })
    }

    /// The reviewer-kind block a [`block_for`](Self::block_for) resolution
    /// SKIPPED, if that skip is why it came up empty (#891 S4) — so a refusal
    /// can say "your roster's only reviewer is the liaison" instead of the flatly
    /// wrong "this group's workflow declares no reviewer block".
    pub fn liaison_shadowing(&self, kind: Role) -> Option<&workflow::Block> {
        (kind == Role::Reviewer)
            .then(|| self.blocks.iter().find(|b| b.kind == kind && !workflow::is_reviewing_block(b)))
            .flatten()
    }

    /// The refusal for "this class has no default block", naming the liaison
    /// when [`liaison_shadowing`](Self::liaison_shadowing) is why (#891 S4).
    ///
    /// One function because there are two call sites that resolve a class to
    /// its default and can come up empty — `spawn_agent_ex`, and `mcp.rs`'s
    /// pre-#222 bare-resume path — and a message that told the author "your
    /// workflow declares no reviewer block" while they are looking at
    /// `kind: reviewer` in their own file is wrong at either of them. Written
    /// once so the two cannot drift (#1072 review, N5).
    pub fn no_default_block_message(&self, kind: Role) -> String {
        match self.liaison_shadowing(kind) {
            Some(l) => format!(
                "this group's workflow declares no {} block that reviews — {:?} is \
                 reviewer-kind but is the human-facing liaison, which records no verdict \
                 and is never a class's default. Name a block explicitly to spawn it.",
                kind.as_str(),
                l.id
            ),
            None => format!("this group's workflow declares no {} block", kind.as_str()),
        }
    }

    /// The agent CLI a capability class's default block runs: the block's own
    /// `cli`, else the group default `agent_cli`. May return an unsupported
    /// value (a block CLI is not coerced in `clamped`); the spawn paths validate
    /// it.
    ///
    /// **A caller holding an agent wants [`cli_for_block`](Self::cli_for_block),
    /// not this** (#2167). This answers a question about a CLASS — which is the
    /// right question for "what would a plain `spawn_agent(kind:)` run" and the
    /// wrong one for "what is THIS pane running".
    pub fn cli_for(&self, role: Role) -> &str {
        match self.block_for(role) {
            Some(b) => workflow::cli_of(b, &self.agent_cli),
            None => &self.agent_cli,
        }
    }

    /// The agent CLI **one agent** runs: the CLI of the block that agent was
    /// spawned from, else — for a block id this roster no longer declares — the
    /// class default [`cli_for`](Self::cli_for) resolves.
    ///
    /// **This, not `cli_for`, is the question every caller holding an agent is
    /// really asking (#2167).** A block IS an agent's identity (#222) and
    /// carries its own `cli`; `cli_for` answers only "what does the FIRST block
    /// of this kind run". The two agree exactly while every class declares one
    /// block, which is why the built-in roster never showed the difference — and
    /// diverge the moment a roster declares two, at which point every agent in
    /// the second block is described by the first block's CLI. That is how a
    /// roster ordering `worker-std` (opencode) ahead of `worker-adv` (claude)
    /// silently stopped every claude delegate's transcript from being read: the
    /// usage collector asked `cli_for(Role::Worker)`, got `opencode`, and never
    /// ran its claude arm at all.
    ///
    /// The fallback is the class default rather than `agent_cli` on purpose: an
    /// agent whose block was renamed or dropped out of the workflow file is
    /// still an agent of its class, and the class default is the best answer
    /// left. An empty `block` (a record persisted before #222) takes the same
    /// path.
    pub fn cli_for_block(&self, block_id: &str, role: Role) -> &str {
        match self.block(block_id) {
            Some(b) => workflow::cli_of(b, &self.agent_cli),
            None => self.cli_for(role),
        }
    }

    /// The model the class's default block runs (already normalized by
    /// `clamped`, so never empty for a roster that went through it).
    pub fn model_for(&self, role: Role) -> &str {
        match self.block_for(role) {
            Some(b) => workflow::model_of(b, &self.agent_cli),
            None => default_model(&self.agent_cli, role),
        }
    }
}

/// Autonomous mode (#83): whether autonomous-era spend has crossed the group's
/// token budget and idle ticking must be suspended. Pure so the metering rule is
/// unit-testable. `spend_since_enable` is the delta from the enable-time usage
/// anchor (autonomous mode meters spend *after* it was turned on, not lifetime
/// history — see `enforce_autonomy_budgets`); `budget_tokens` 0 = no cap.
pub fn autonomy_budget_exhausted(spend_since_enable: u64, budget_tokens: u64) -> bool {
    budget_tokens != 0 && spend_since_enable >= budget_tokens
}

/// Models are interpolated into a shell command line; restrict them to
/// identifier-ish characters so a crafted "model" can't smuggle arguments.
///
/// **`/` is admitted (#722), and that widening is deliberate.** OpenCode model
/// ids are `provider_id/model_id` — `opencode/deepseek-v4-flash-free` — so the
/// pre-#722 filter silently turned the only id its Zen provider answers to into
/// `opencodedeepseek-v4-flash-free`, a model that does not exist, with no error
/// anywhere. `/` is inert in both emitted forms: it is not a glob
/// metacharacter to a POSIX shell (only `*?[` are) and not an operator in
/// PowerShell, so nothing about the "can't smuggle an argument" property
/// changes. Same reasoning, and the same "widen deliberately, pin with tests"
/// discipline, as #709's bracket decision — which went the OTHER way and kept
/// `[`/`]` out, because those ARE glob syntax (see [`claude_model_arg`]).
fn sanitize_model(m: &str, fallback: &str) -> String {
    let cleaned: String = m
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/'))
        .collect();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

/// Remove a durable consent marker (autonomous / auto_merge), treating "already
/// gone" as success but a real IO failure as an error the caller MUST propagate
/// (#83). A marker that survives a failed *disable* would silently re-enable the
/// feature on the next restart's re-seed — the one failure direction this
/// consent-boundary feature must never have — so the toggle fails loudly and
/// leaves the in-memory flag matching the surviving marker rather than reporting a
/// disable that didn't durably happen.
pub(in crate::orchestration) fn remove_marker(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to remove marker {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod max_notice_tests {
    use super::*;

    const DEB: Duration = Duration::from_secs(3);

    /// #904: these tests drive the pure coalescer with literal ids; the one
    /// constructor is the only way to make a `GroupId`, in tests as in prod.
    fn gid(s: &str) -> GroupId {
        GroupId::parse(s).unwrap()
    }

    #[test]
    fn burst_coalesces_to_one_span() {
        // Three rapid clicks 4→3, 3→2, 2→1 inside the window: one pending entry
        // spanning the whole burst, its deadline riding the LAST click.
        let mut p = HashMap::new();
        record_max_notice(&mut p, &gid("g"), 4, 3, 1_000, DEB);
        record_max_notice(&mut p, &gid("g"), 3, 2, 1_500, DEB);
        record_max_notice(&mut p, &gid("g"), 2, 1, 2_000, DEB);
        assert_eq!(p.len(), 1, "a burst stays one pending notice");
        // Not yet due (last click at 2_000 → due 5_000): nothing flushes.
        assert!(take_due_max_notices(&mut p, 4_999).is_empty());
        // Past the window: exactly one notice, from the burst's first value to
        // its last — 4→1, never the intermediate 4→3 / 3→2.
        assert_eq!(take_due_max_notices(&mut p, 5_000), vec![(gid("g"), 4, 1)]);
        assert!(p.is_empty(), "delivered notices are drained");
    }

    #[test]
    fn each_click_pushes_the_deadline_out() {
        // A click landing before the prior one's window elapses must reset the
        // deadline, or a long slow drag would fire mid-burst.
        let mut p = HashMap::new();
        record_max_notice(&mut p, &gid("g"), 4, 3, 1_000, DEB); // due 4_000
        record_max_notice(&mut p, &gid("g"), 3, 2, 3_900, DEB); // due 6_900
        // At 4_000 the first click's deadline has passed, but the second reset
        // it — so nothing is due yet.
        assert!(take_due_max_notices(&mut p, 4_000).is_empty());
        assert_eq!(take_due_max_notices(&mut p, 6_900), vec![(gid("g"), 4, 2)]);
    }

    #[test]
    fn spaced_changes_deliver_separately() {
        // Two changes far enough apart that the first flushes before the second
        // arrives: two distinct notices, each its own span.
        let mut p = HashMap::new();
        record_max_notice(&mut p, &gid("g"), 4, 3, 1_000, DEB);
        assert_eq!(take_due_max_notices(&mut p, 4_000), vec![(gid("g"), 4, 3)]);
        record_max_notice(&mut p, &gid("g"), 3, 2, 10_000, DEB);
        assert_eq!(take_due_max_notices(&mut p, 13_000), vec![(gid("g"), 3, 2)]);
    }

    #[test]
    fn net_noop_burst_delivers_nothing() {
        // 4→3→4 nets to no change: no orchestrator tokens spent on a no-op.
        let mut p = HashMap::new();
        record_max_notice(&mut p, &gid("g"), 4, 3, 1_000, DEB);
        record_max_notice(&mut p, &gid("g"), 3, 4, 1_500, DEB);
        assert!(take_due_max_notices(&mut p, 5_000).is_empty());
        assert!(p.is_empty(), "the netted-out entry is still drained, not left pending");
    }

    #[test]
    fn groups_debounce_independently() {
        // Two groups clicking at once don't share a deadline or a span.
        let mut p = HashMap::new();
        record_max_notice(&mut p, &gid("a"), 4, 2, 1_000, DEB); // due 4_000
        record_max_notice(&mut p, &gid("b"), 5, 6, 3_000, DEB); // due 6_000
        // Only group a is due at 4_000.
        assert_eq!(take_due_max_notices(&mut p, 4_000), vec![(gid("a"), 4, 2)]);
        assert!(p.contains_key("b"), "b keeps waiting out its own window");
        assert_eq!(take_due_max_notices(&mut p, 6_000), vec![(gid("b"), 5, 6)]);
    }
}

/// #332: the idle-tick notice states what the host-side intake poll already
/// found, so the orchestrator acts on it instead of re-polling.
#[cfg(test)]
mod idle_tick_notice_tests {
    use super::*;

    #[test]
    fn without_an_intake_summary_reads_exactly_as_before_332() {
        let n = idle_tick_notice(None, false);
        assert!(n.starts_with("[orrerix] idle tick: you have been idle"), "got: {n}");
        assert!(!n.contains("host-side intake poll"), "must not claim a finding that doesn't exist: {n}");
        // #805: the fallback pointed at the allow-listed label by name; it must
        // name the label `gh.rs`'s ALLOWED_LABELS/validate_labels actually accepts
        // (`agent-investigation`), not the shorter `agent-investigate` the
        // issue-#82 plan text used, which the backend rejects on write.
        assert!(n.contains("agent-investigation"), "must name the real, writable label: {n}");
    }

    #[test]
    fn an_empty_summary_is_treated_like_none() {
        let n = idle_tick_notice(Some(""), false);
        assert!(!n.contains("host-side intake poll"), "an empty summary must not add a dangling clause: {n}");
    }

    #[test]
    fn a_present_summary_tells_the_orchestrator_not_to_repoll_it() {
        let n = idle_tick_notice(Some("issue #42 labeled agent-ready (\"Do the thing\")"), false);
        assert!(n.contains("host-side intake poll already found"), "got: {n}");
        assert!(n.contains("issue #42 labeled agent-ready"), "got: {n}");
        assert!(n.contains("instead of re-polling"), "must tell the orchestrator not to redo the work: {n}");
    }

    #[test]
    fn a_present_summary_never_also_orders_an_independent_sweep() {
        // rev-33 N1: the two instructions are contradictory in the same message —
        // "poll for labeled intake / re-check your open PRs" (the sweep-yourself
        // case) must never appear alongside "act on it directly instead of
        // re-polling" (the summary case).
        let n = idle_tick_notice(Some("issue #42 labeled agent-ready (\"Do the thing\")"), false);
        assert!(!n.contains("poll for labeled intake"), "got: {n}");
        assert!(!n.contains("re-check your open PRs"), "got: {n}");
    }

    #[test]
    fn an_incomplete_summary_is_routed_to_the_sweep_bearing_text_not_the_dead_end_pointer() {
        // rev-33 N7: `render()`'s dropped-block clause points a human at "the
        // intake-signal audit trail", but no MCP tool lets an AGENT read the
        // audit log — a dead end in a delivered prompt. `summary_incomplete:
        // true` must route to the full sweep text instead, exactly like `None`.
        let dropped_summary = "(+3 earlier finding(s) dropped for space — see the intake-signal \
             audit trail); issue #42 labeled agent-ready (\"Do the thing\")";
        let n = idle_tick_notice(Some(dropped_summary), true);
        assert!(n.contains("poll for labeled intake"), "must fall back to a real sweep: {n}");
        assert!(n.contains("re-check your open PRs"), "got: {n}");
        assert!(!n.contains("intake-signal audit trail"), "must not hand the agent a dead-end pointer: {n}");
        assert!(!n.contains("instead of re-polling"), "must not also claim the summary is trustworthy enough to act on alone: {n}");
    }

    #[test]
    fn a_complete_summary_is_unaffected_by_the_incomplete_flag_when_false() {
        let n = idle_tick_notice(Some("issue #42 labeled agent-ready (\"Do the thing\")"), false);
        assert!(n.contains("instead of re-polling"), "an ordinary (non-truncated) summary keeps the act-directly framing: {n}");
    }
}
