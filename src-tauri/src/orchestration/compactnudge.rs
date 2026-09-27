//! The compact nudge's and reinjection's pure decisions: when to nudge or
//! escalate, context-window arithmetic, compaction detection per CLI, the
//! reinjection notice and its shape, and the directive-ledger embed and cap.
//! Design note: `docs/design/compaction-settings.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888):
//! `crate::modelstate`, `crate::usage`. IO: fs. Sibling files it calls:
//! `persona.rs`, `spawnpolicy.rs`, `tuning.rs`.

use super::*;
/// Production bug fix (rev-42 delta, round 2): bounded retry count for a
/// reinjection stuck past `REINJECT_CONFIRM_TIMEOUT_MS` — see `AgentEntry::
/// compact_reinject_attempts`'s doc for why this must never be unbounded.
pub(in crate::orchestration) const MAX_REINJECT_ATTEMPTS: u32 = 3;
/// Compact-nudge min-context floor (benchtest finding, rev-65 smart-default
/// round): the floor applied when `Guardrails.compact_nudge_min_context_percent`
/// is `None` (unset) AND the parent heuristic is on (`compact_nudge_minutes >
/// 0`) — see `compact_nudge_context_floor_met`. Chosen so enabling the
/// quiet-window alone is enough to avoid the exact waste the live benchtest
/// found (real compactions at 20-31% full) with zero additional config.
pub(in crate::orchestration) const DEFAULT_COMPACT_NUDGE_MIN_CONTEXT_PERCENT: u32 = 50;

/// What a tick should DO about a re-grounding whose delivery has not been
/// confirmed (#535).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReinjectDisposition {
    /// Evidence arrived that ends the retry loop: either a confirmed delivery
    /// (our submit sampler watched the Enter land) or the agent's own
    /// post-attempt MCP call (it is alive and executing). Resolve the latch;
    /// never re-send.
    ///
    /// **This arm does not mean the re-grounding was read** — #546. The two
    /// signals are not equally strong and neither proves a read, so which one
    /// fired is carried out to the audit record and the lifecycle badge rather
    /// than collapsed into one "confirmed" claim here. See [`ReinjectAck`].
    Resolved,
    /// Still inside `REINJECT_CONFIRM_TIMEOUT_MS`: a delivery may legitimately
    /// still be in flight. Keep waiting; touch nothing.
    Wait,
    /// The window is spent, but the pane is producing output *right now* —
    /// this agent is mid-turn. Do not spend an attempt into a live turn; wait
    /// for a lull, bounded by `REINJECT_BUSY_DEFER_MAX_MS`.
    DeferBusy,
    /// Window spent, nothing observed, pane not busy (or the busy deferral is
    /// itself spent). Re-send — or, at `MAX_REINJECT_ATTEMPTS`, abandon.
    Retry,
}

/// Should a still-unconfirmed re-grounding be re-sent this tick? (#535)
///
/// The bug this replaces: the retry loop treated an attempt as successful
/// **only** on a delivery confirmation (`submit_sent_ms >= attempted_ms &&
/// confirmed`), and that signal is precisely the one known to be unreliable on
/// busy/repainting panes — roughly 25 false `delivery unconfirmed` alarms in a
/// single observed session (#451/#496/#522/#528 lineage). So a re-grounding
/// that had **landed** was re-pasted up to twice more into a working agent, and
/// could finish as a `reinjection-abandoned` record claiming the contract was
/// never restored when it had been. Observed live on a copilot worker sitting
/// at 2/3 while visibly on-track.
///
/// The fix is to stop inferring the outcome from *our* delivery machinery and
/// observe **the agent** instead:
///
/// - `confirmed_delivery` — the pre-#535 signal, kept and checked first. When
///   it does fire it is authoritative; it was only ever wrong by *omission*.
/// - `acked` — the agent called a loomux MCP tool after `attempted_ms`
///   (`AgentEntry::last_mcp_activity_ms`). This is an ACKNOWLEDGMENT, not
///   another delivery inference: it is the agent's own process reaching
///   loomux, and re-syncing through exactly these calls is what the
///   re-grounding notice instructs. A pane cannot repaint one into existence.
///
/// **Why terminal output is deliberately NOT part of `acked`.** #535's own
/// write-up lists "attributable terminal output" as an ack signal, and it must
/// not be one: `output_total` counts our own pasted notice echoing back, plus
/// statusline/spinner repaint frames (#480) — see `idle_output_is_activity`.
/// Worse, folding growth into `acked` would make `DeferBusy` structurally
/// unreachable (busy ⇒ growth ⇒ ack ⇒ `Resolved`), i.e. an arm documenting a
/// behaviour it can never have. Output therefore feeds `busy` only, where its
/// weakness is harmless: it defers an attempt, it never claims one succeeded.
///
/// Ordering is the contract. Evidence of a landing beats the clock (else the
/// bug survives); the clock beats busy-ness (else a chatty pane defers before
/// it has even waited); and the busy deferral is bounded (else a permanently
/// noisy pane suppresses the retry forever and scope's safety net for a
/// genuinely lost re-grounding dies quietly).
///
/// `busy_defer_max_ms == 0` disables the deferral (pre-#535 behaviour) rather
/// than making it expire instantly — the same convention, for the same reason,
/// as #518's `human_input_block(.., bound_ms)`: a mis-set `0` must degrade to
/// the old behaviour, never to "defer nothing, ever".
///
/// Pure, and four-way rather than a bool, so each outcome is separately
/// auditable and directly pinnable by tests.
#[doc(hidden)] // pub for integration tests
pub fn reinject_disposition(
    confirmed_delivery: bool,
    acked: bool,
    busy: bool,
    elapsed_ms: u64,
    timeout_ms: u64,
    busy_defer_max_ms: u64,
) -> ReinjectDisposition {
    if confirmed_delivery || acked {
        return ReinjectDisposition::Resolved;
    }
    if elapsed_ms < timeout_ms {
        return ReinjectDisposition::Wait;
    }
    if busy && elapsed_ms < timeout_ms.saturating_add(busy_defer_max_ms) {
        return ReinjectDisposition::DeferBusy;
    }
    ReinjectDisposition::Retry
}

/// Has this agent ACTED — reached loomux through its own MCP client — at or
/// after `since_ms`? (#535)
///
/// **The shared agent-activity predicate.** Deliberately named and shaped for
/// more than its first caller: it takes a bare timestamp pair, not a
/// reinjection, because the same question ("did the agent itself do something
/// after we pasted at T?") is the missing input everywhere loomux currently
/// infers an outcome from its own delivery machinery. `reinject_disposition`
/// (#535) is the first consumer; the unconfirmed-delivery detector (**#539**)
/// is the intended second one — `unconfirmed_disposition` (#522/#528) today
/// reads only BOX structure (is our paste still at the tail, is human text
/// outstanding) and has no activity evidence at all, which is why a pane that
/// simply got on with its work can still raise a false alarm. #539's scope is
/// **not** taken here and this fn has no second call site yet; this is only the
/// mechanism it will need, kept general so it does not have to be rebuilt.
///
/// What this signal means, precisely: **the agent's own code path executed**.
/// It is stamped once, at the `tools/call` dispatch funnel, so a `true` here
/// says a live process authenticated with this agent's token and invoked a
/// loomux tool. What it explicitly does NOT mean: **that the pane painted**.
/// Terminal output is not part of it, and must not be folded in — see
/// `reinject_disposition`'s doc for why (our own paste echoes, statusline and
/// spinner repaint frames per #480, and `idle_output_is_activity`'s whole
/// reason for existing).
///
/// **Callers comparing against a PASTE time owe a settling floor** (rev-15
/// finding 2). This fn answers "did the agent act after time T", which is not
/// the same question as "did the agent act *in response to* what we pasted at
/// T". Between deciding to paste and the Enter actually being pressed there is
/// a real window (`SUBMIT_MAX_WAIT` + the `SUBMIT_RETRY_DELAYS` tail), and any
/// call arriving inside it was decided during the agent's *previous* turn — a
/// false landed-signal of exactly the kind #112 and #522 removed elsewhere. So
/// pass `paste_ms + REINJECT_ACK_SETTLE_MS`, not `paste_ms`; see that constant
/// for the derivation. The floor lives at the call site rather than in here so
/// this stays a bare, reusable timestamp comparison — but a caller that skips
/// it is wrong, not merely stricter.
///
/// The comparison is `>=` rather than `>` purely as a boundary convention, so
/// a stamp landing in the same millisecond as `since_ms` counts. Nothing rests
/// on that choice once the settling floor above is applied — it is not a claim
/// that an agent can answer within a millisecond.
///
/// `last_mcp_activity_ms == 0` is "never called" (`AgentEntry`'s seed) and can
/// never satisfy a real `since_ms`, so a never-calling agent falls through to
/// whatever its caller's unchanged path is.
pub fn agent_acted_since(last_mcp_activity_ms: u64, since_ms: u64) -> bool {
    last_mcp_activity_ms > 0 && last_mcp_activity_ms >= since_ms
}

/// WHICH evidence closed a re-grounding phase — and, in words, exactly what
/// that evidence proves (#546).
///
/// **Why this is a type and not the two string literals it replaces.** The
/// same distinction has to be stated in four places — the durable audit
/// record, the lifecycle badge's label, that badge's tooltip, and the entry
/// field the badge reads — and each one previously restated it in its own
/// words. #546's finding is precisely that a claim drifted from what was
/// proven; a vocabulary that lives in one place is what stops the *next* claim
/// drifting. Every surface derives its wording from here.
///
/// **The two are not equally strong, and neither proves what the phase is
/// named after.**
///
/// - [`Delivered`](Self::Delivered) — `confirmed_delivery`: loomux's own submit
///   sampler watched the notice's Enter land. That is evidence about **our
///   paste**: the re-grounding text reached the agent's input box and was
///   submitted.
/// - [`LivenessOnly`](Self::LivenessOnly) — `acked`
///   ([`agent_acted_since`]): the agent's own process reached loomux through a
///   token-authenticated `tools/call` after the settling floor. That is
///   evidence about **the agent**: it is alive and executing. It says nothing
///   whatever about our paste.
///
/// **Neither proves the re-grounding was READ.** #546 weighed the three ways
/// it could be proven and every one was declined on this project's standing
/// constraints: an acknowledgment marker makes re-grounding a two-party
/// protocol every agent CLI has to cooperate with, and correlating the
/// activity stamp against the paste content is exactly the heuristic
/// inference #112 removed. So the honest move is the one an audit record can
/// actually support: **say what was observed, never what was hoped.**
/// [`proves`](Self::proves) / [`does_not_prove`](Self::does_not_prove) are
/// written into the record itself so a reader is not required to already know
/// which of the two `source` values is the weak one.
///
/// The wire values (`"delivery"` / `"activity"`) are deliberately unchanged
/// from #535/#588: they name the evidence *source*, and that was always
/// accurate. What was wrong was the **claim wrapped around them** — an
/// `acked`/`confirmed` vocabulary asserting an acknowledgment that, on the
/// `LivenessOnly` arm, nobody ever made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReinjectAck {
    /// Our submit sampler saw the Enter land. Evidence about the paste.
    #[serde(rename = "delivery")]
    Delivered,
    /// The agent called a loomux tool afterwards. Evidence about the agent.
    #[serde(rename = "activity")]
    LivenessOnly,
}

impl ReinjectAck {
    /// Which arm of `reinject_disposition`'s `confirmed_delivery || acked`
    /// actually fired. Ordered so the STRONGER evidence wins when both are
    /// present: a resolve that had a real delivery confirmation must never be
    /// recorded as the weaker liveness close.
    ///
    /// Note the one direction this cannot see: a tick where `acked` fires
    /// first resolves immediately (#535's whole point — a landed re-grounding
    /// is never re-sent), so a delivery confirmation that would have arrived a
    /// few seconds later is never observed. That under-reports rather than
    /// over-reports, which is the safe direction and the only one an honest
    /// record can take: the value names what loomux had in hand at the moment
    /// it stopped retrying.
    pub fn from_evidence(confirmed_delivery: bool) -> Self {
        if confirmed_delivery { Self::Delivered } else { Self::LivenessOnly }
    }

    /// The `source` value on the audit line and the badge's wire shape.
    /// Unchanged from #535/#588 — see the type doc for why the strings stayed
    /// while the claims around them changed.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Delivered => "delivery",
            Self::LivenessOnly => "activity",
        }
    }

    /// The audit action this resolution is written under.
    ///
    /// **These are two actions, not one action with a field**, for the same
    /// reason #539 gave `delivery-unconfirmed-agent-active` its own name
    /// rather than making it a flavour of `delivery-unconfirmed-idle-pane`:
    /// "we watched our Enter land" and "the agent is alive" are different
    /// observations, and a single `compact-reinjection-confirmed` action
    /// covering both means anyone counting confirmations in `audit.jsonl` —
    /// the durable record, read long after the badge is gone — counts
    /// liveness closes as confirmations. That miscount is #546's finding
    /// expressed in the one surface that outlives every session.
    pub fn audit_action(self) -> &'static str {
        match self {
            Self::Delivered => "compact-reinjection-confirmed",
            Self::LivenessOnly => "compact-reinjection-liveness-only",
        }
    }

    /// What this evidence establishes, stated in the record rather than left
    /// to a reader who has to know the vocabulary.
    pub fn proves(self) -> &'static str {
        match self {
            Self::Delivered =>
                "loomux's own submit sampler observed the re-grounding notice's Enter land — \
                 the text reached the agent's input box and was submitted",
            Self::LivenessOnly =>
                "the agent's own process called a loomux tool after the settling floor — \
                 it is alive and executing",
        }
    }

    /// What this evidence does NOT establish. Both arms have one, because
    /// neither proves the re-grounding was read; the `LivenessOnly` arm's is
    /// the larger residual #546 filed.
    pub fn does_not_prove(self) -> &'static str {
        match self {
            Self::Delivered =>
                "that the agent read the re-grounding — no artifact loomux can observe proves that",
            Self::LivenessOnly =>
                "that the re-grounding was delivered or read. A genuinely lost paste on an agent \
                 that is busy for some other reason closes the phase exactly this way",
        }
    }
}

/// Compact-nudge (#287): whether `role`'s capability class is one of the
/// group's configured eligible roles for the automatic `/compact` nudge. Pure
/// so role gating is testable without a registry. `allowed_roles` is
/// `Guardrails::compact_nudge_roles` after `clamped()` (lowercase
/// `Role::as_str()` names; never empty for a group that went through it).
pub fn compact_nudge_role_allowed(role: Role, allowed_roles: &[String]) -> bool {
    allowed_roles.iter().any(|r| r == role.as_str())
}

/// Canonicalize a compact-nudge role list (rev-24 review fix): drop
/// unrecognized entries and normalize every survivor to `Role::as_str()`'s
/// lowercase wire name. `workflow::kind_from_str` validates case-
/// insensitively but does not normalize — keeping the as-typed string (e.g.
/// `"Orchestrator"`) let it persist and pass validation, yet
/// `compact_nudge_role_allowed`'s lowercase comparison would then never match
/// it, silently disabling the role. Sorted + deduped for a stable
/// persisted/reported shape; an all-unrecognized/empty result falls back to
/// `["orchestrator"]` rather than disabling every role. Shared by
/// `Guardrails::clamped`, the `resumed` re-normalization in
/// `create_group_ex`, and `set_compact_nudge_roles` so the rule lives once.
pub(in crate::orchestration) fn canonicalize_compact_nudge_roles(roles: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = roles
        .into_iter()
        .filter_map(|r| workflow::kind_from_str(&r).map(|k| k.as_str().to_string()))
        .collect();
    out.sort();
    out.dedup();
    if out.is_empty() {
        out = vec![Role::Orchestrator.as_str().to_string()];
    }
    out
}

/// Compact-nudge (#287, #413 S4): the command loomux pastes to compact a `cli`
/// pane — `None` when it has none it may drive. Read off the CLI's
/// [`CliCaps::compact_command`](loomux_engine::model::CliCaps) row, the one
/// table that answers per-CLI questions (CLAUDE.md constraint 8), so admitting
/// a CLI is a row edit and a CLI's own spelling is pasted verbatim rather than
/// assumed to be `/compact`.
///
/// `Some` is also the whole admission test for `compact_nudge_tick`'s
/// per-agent loop, `request_compact`, the human's "Compact now" and the
/// orchestrator's idle-compact backstop: a pane with no command gets no paste
/// from any of them, and nothing else the loop does is worth running for a
/// pane it can never compact. An unknown CLI has no row and so no command.
///
/// `cli` comes from `Guardrails::cli_for_block` — the agent's OWN block, not
/// its class's default block (#2167). The loop reads
/// `g.cli_for_block(&a.block, a.role)` directly, and `request_compact` reaches
/// it through `OrchRegistry::cli_for_agent`. Feeding this `cli_for(role)`
/// instead is what once silently excluded every claude delegate in a
/// two-block class from compact nudges.
///
/// Before #413 S4 this was a `matches!(cli, "claude" | "copilot")` predicate:
/// every other CLI was outside the loop whatever its row said, so pi, codex
/// and opencode panes were never escalated even once their readers (#993
/// S2a–S2c) reported tokens. Gemini stays out because its row has no command:
/// its equivalent is spelled `/compress`, which S0 did not establish, and
/// pasting `/compact` there would type a command that does not exist into a
/// live pane.
pub fn compact_command_for(cli: &str) -> Option<&'static str> {
    let over = COMPACT_COMMAND_OVERRIDE
        .with(|c| c.borrow().as_ref().and_then(|(k, spelling)| (k.as_str() == cli).then_some(*spelling)));
    if over.is_some() {
        return over;
    }
    loomux_engine::model::cli_caps(cli).and_then(|caps| caps.compact_command)
}

thread_local! {
    /// Test seam for [`compact_command_for`]: `(cli, spelling)` answered in
    /// place of that CLI's row, on the calling thread only (#413 S4 review).
    /// Every row that carries a command spells it `/compact`, so without a
    /// spelling no row has, a paste site that hard-coded `"/compact"` would pass
    /// every test that reads the row — the specimen would be outside the class
    /// it witnesses. `None` in production, always.
    static COMPACT_COMMAND_OVERRIDE: std::cell::RefCell<Option<(String, &'static str)>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only seam: answer [`compact_command_for`]`(cli)` with `spelling` on the
/// calling thread (`None` restores the rows). A real `pub` function rather than
/// `#[cfg(test)]` because the integration tests that link the lib cannot see
/// `cfg(test)` items — the `modelstate::set_probe_windows_for_test` precedent.
#[doc(hidden)] // pub for the `tests/orchestration/compact.rs` spelling pin
pub fn set_compact_command_for_test(over: Option<(&str, &'static str)>) {
    COMPACT_COMMAND_OVERRIDE.with(|c| *c.borrow_mut() = over.map(|(cli, spelling)| (cli.to_string(), spelling)));
}

/// The refusal `request_compact` and the human's "Compact now" give for a
/// pane [`compact_command_for`] has no command for (#413 S4): the CLI's
/// `compact_note` — why its row is `None` — so the caller learns the reason
/// rather than only the fact. An unknown CLI, or a row with no note, still
/// says which CLI refused.
pub fn compact_unsupported_reason(cli: &str) -> String {
    let note = loomux_engine::model::cli_caps(cli)
        .map(|caps| caps.compact_note.trim())
        .filter(|note| !note.is_empty());
    match note {
        Some(note) => format!("orrerix has no compact command it may paste into a {cli} pane: {note}"),
        None => format!("orrerix has no compact command it may paste into a {cli} pane"),
    }
}

/// A context reading with tokens and no window a percent may be computed
/// against (`modelstate::published_window` said `None`), in a group whose
/// escalation threshold is on — the reading `compact_nudge_tick` can never
/// escalate, carried from `agent_context_percents` to
/// `note_unwindowed_escalations` so the audit says why (#413 S4).
pub(in crate::orchestration) struct UnwindowedReading {
    pub(in crate::orchestration) agent: String,
    pub(in crate::orchestration) group: GroupId,
    pub(in crate::orchestration) tokens: u64,
    pub(in crate::orchestration) source: crate::modelstate::ContextSource,
}

/// Compact-nudge (#328): whether an agent-requested compact should fire NOW.
/// Unlike the heuristic path (`idle_tick_should_fire`, a minutes-scale
/// threshold) there is no quiet-window wait here — the agent's own
/// `request_compact` call IS the trigger — so the only gates are: the pane
/// must be quiet on THIS observation (no growth since the last poll, i.e. not
/// actively mid-turn right now), and the shared per-hour cap must not already
/// be exhausted (reused via `spawn_rate_exceeded`, the same rule
/// `idle_tick_should_fire` reuses, so both paths draw from ONE budget). Pure
/// so the gate is testable without a registry.
pub fn compact_request_should_fire(
    is_currently_quiet: bool,
    tick_times: &[u64],
    now_ms: u64,
    per_hour_cap: u32,
) -> bool {
    is_currently_quiet && !spawn_rate_exceeded(tick_times, now_ms, per_hour_cap, SPAWN_RATE_WINDOW_MS)
}

/// Compact-nudge (#328): percent of `window_tokens` that `context_tokens`
/// represents, clamped to `0..=100`. Pure arithmetic split out of the
/// registry pass so the rounding/clamping rule is unit-testable without a
/// transcript fixture.
pub fn context_percent_used(context_tokens: u64, window_tokens: u64) -> u32 {
    if window_tokens == 0 {
        return 0;
    }
    ((context_tokens.min(window_tokens) * 100) / window_tokens) as u32
}

/// Production bug fix (PR #329 round 7): the context-window size (tokens) to
/// compute a percent against, for one agent — single shared derivation for
/// BOTH the lifecycle-panel display and the `compact_context_threshold_
/// percent` escalation. Before this fix each read a flat `CLAUDE_CONTEXT_
/// WINDOW_TOKENS` (200K) independently, silently wrong (and the escalation
/// threshold firing ~5x too early) for any agent actually running a model on
/// a larger tier. An explicit human `override_tokens` (set on the group's
/// guardrails) wins outright; otherwise the model recorded in the agent's
/// OWN transcript (`usage::claude_context_window_tokens`) — this is the
/// authoritative-when-available signal (reflects what's ACTUALLY running,
/// immune to config drift), falling back to `usage::
/// DEFAULT_CLAUDE_CONTEXT_WINDOW_TOKENS` when the model is unknown.
///
/// #993 S1 makes it the four-rung ladder `modelstate::context_window_ladder`
/// states and argues: override, then the window the CLI REPORTED (Claude's
/// status line — `reported_tokens`), then the model table above, then the
/// empirical clamp to `observed_tokens`. Returns the rung beside the window so
/// the lifecycle panel can say which one it is showing (S3 publishes it).
/// #993 S2b: `reported_rounded` relabels a rounded rung-2 report
/// `reported-rounded` (`modelstate::label_rounded_report`).
pub fn effective_context_window_tokens(
    override_tokens: Option<u64>,
    reported_tokens: Option<u64>,
    reported_rounded: bool,
    model: Option<&str>,
    observed_tokens: Option<u64>,
) -> (u64, crate::modelstate::WindowSource) {
    crate::modelstate::label_rounded_report(
        crate::modelstate::context_window_ladder(
            override_tokens,
            reported_tokens,
            crate::usage::claude_context_window_tokens(model),
            observed_tokens,
        ),
        reported_rounded,
    )
}

/// Production bug fix (PR #329 delta review): how much the context-token
/// reading must drop, relative to the baseline captured when `compact_
/// pending` was set, to count as `100`-scaled ratio evidence a REAL
/// compaction happened. Context tokens only grow across ordinary turns (each
/// turn's context recap includes everything before it) until a compaction
/// resets it, so ANY drop below the baseline is already meaningful — but a
/// generous margin (require the reading to fall to at most 70% of the
/// baseline) tolerates cache-boundary noise in the raw number without being
/// so loose that ordinary turn-to-turn fluctuation could pass for it; a real
/// compact's actual reduction is typically far more dramatic than this
/// (summarization commonly cuts what's sent to the model by 80-95%+).
const COMPACTION_CONFIRMED_MAX_RATIO_PERCENT: u64 = 70;

/// Production bug fix (PR #329 delta review — B2/D2): whether the observed
/// token reading is convincing evidence a compaction actually ran, comparing
/// `current` against the `baseline` captured the moment `compact_pending`
/// was set (see `AgentEntry.compact_pending_baseline_tokens`).
///
/// **Why this exists.** A live production incident showed the prior design
/// — treat ANY busy-then-quiet cycle while `compact_pending` as proof
/// compaction ran — is not evidence, only a proxy that can be satisfied by an
/// agent's ORDINARY turn: an orchestrator asked to discuss the compact-nudge
/// feature produced output whose growth (busy) then silence (quiet)
/// repeatedly matched the completion detector with no compaction ever
/// running, delivering the mandatory re-injection on a loop that only grew
/// context every cycle — the opposite of what the feature exists to prevent.
///
/// **Fails CLOSED, not open**: `None` for either reading (no baseline was
/// captured, or no current reading is available) returns `false` — no
/// evidence means no reinjection, never a guess. A missed reinjection is a
/// missed convenience; a reinjection loop that only grows context is a
/// production incident (the exact one this function exists to prevent). Pure
/// so the ratio math is testable without a registry or a transcript file.
pub fn compaction_confirmed(baseline: Option<u64>, current: Option<u64>) -> bool {
    match (baseline, current) {
        (Some(b), Some(c)) if b > 0 => c.saturating_mul(100) <= b.saturating_mul(COMPACTION_CONFIRMED_MAX_RATIO_PERCENT),
        _ => false,
    }
}

/// Production bug fix (rev-42 delta review): whether the observed signals
/// are convincing evidence a compaction actually ran, for the two INFERENCE
/// trigger paths (banner detection, human-typed `/compact` detection) — the
/// only ones that can be WRONG about a compaction having happened at all.
/// (The loomux-initiated paths — heuristic timer, `request_compact` — skip
/// this check entirely; see `AgentEntry.compact_pending_trusted`'s doc for
/// why applying it there deadlocks instead of protecting anything.)
///
/// Confirmed by EITHER signal:
/// - `compaction_confirmed`'s token-ratio check (`token_baseline`/
///   `token_current`) — solid once available, but a NEXT-TURN phenomenon
///   (proven against a real transcript — see `usage::tests::
///   real_transcript_proves_the_token_drop_is_a_next_turn_phenomenon_
///   rev42_q1`): the compaction call itself still reports the PRE-compact
///   token count, and the drop only appears on the turn after.
/// - a NEW `usage::compact_boundary_count` since `marker_baseline` — the
///   CLI's own structural "a compaction just completed" marker, observable
///   the INSTANT compaction finishes, with no next-turn dependency at all.
///   Preferred where available; kept alongside the token check (not a
///   replacement for it) as defense-in-depth against either signal being
///   unavailable or wrong in some future CLI build.
///
/// Fails CLOSED exactly like `compaction_confirmed`: no evidence in EITHER
/// direction, on EITHER signal, is never a guess.
pub fn inferred_compaction_confirmed(
    token_baseline: Option<u64>,
    token_current: Option<u64>,
    marker_baseline: Option<u64>,
    marker_current: Option<u64>,
) -> bool {
    if compaction_confirmed(token_baseline, token_current) {
        return true;
    }
    match (marker_baseline, marker_current) {
        (Some(b), Some(c)) => c > b,
        _ => false,
    }
}

/// Lifecycle-panel surfacing (PR #329 round 6): how long a lost-outcome
/// (`CompactionStatus::Abandoned`) stays visible after the fact — an old one
/// would read as a current problem long after the agent moved on.
const COMPACTION_STATUS_RECENT_WINDOW_MS: u64 = 10 * 60 * 1000;

/// Compact-nudge (PR #329 round 6, lifecycle UI): the compaction state-
/// machine phase to surface per agent. Serializes as `{"status": "armed",
/// "trusted": true}` etc. (`tag = "status"`) — the group lifecycle panel's
/// wire shape.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CompactionStatus {
    /// No arm, no in-flight reinjection, no recent lost outcome.
    None,
    /// `compact_pending`, not yet observed busy. `trusted` distinguishes the
    /// loomux-initiated arm (heuristic/`request_compact`) from an inference
    /// arm (banner/manual detection) — the former skips confirmation, the
    /// latter needs evidence before it can resolve to a reinjection. `source`
    /// (#417) is `Some("hook")` when a PreCompact/SessionStart marker armed
    /// this, `None` for every pre-#417 arm path — so the panel can tell a
    /// hook-confirmed compaction from an inferred/loomux-initiated one.
    Armed { trusted: bool, source: Option<&'static str> },
    /// `compact_pending`, busy observed — waiting on quiet to attempt a
    /// confirm/discard resolution.
    AwaitingEvidence { trusted: bool, source: Option<&'static str> },
    /// A reinjection has been decided and is waiting on its delivery to
    /// confirm (or its next bounded retry) — see `compact_reinject_
    /// attempted_ms`'s doc. `attempt` is 1-indexed, bounded by `max_attempts`
    /// (`MAX_REINJECT_ATTEMPTS`).
    Reinjecting { attempt: u32, max_attempts: u32 },
    /// A lost terminal outcome (`arm-timeout` or `reinjection-abandoned`)
    /// within `COMPACTION_STATUS_RECENT_WINDOW_MS` of now — worth a human's
    /// attention, but only briefly.
    Abandoned { reason: String, since_ms: u64 },
    /// #546: a re-grounding phase that RESOLVED within
    /// `COMPACTION_STATUS_RECENT_WINDOW_MS`, carrying the [`ReinjectAck`] that
    /// closed it (see `AgentEntry::compact_last_ack`).
    ///
    /// **Named for what happened (the phase resolved), not for a claim nobody
    /// made.** The variant was `Acked` and the badge read `re-grounding acked`
    /// — but on the `LivenessOnly` arm nothing acknowledged anything: the
    /// agent called a loomux tool for reasons of its own and loomux stopped
    /// retrying. That is the exact word #546 is filed against, and it was
    /// sitting in the wire tag as well as the label.
    ///
    /// The two evidence classes are not equally strong and the system cannot
    /// tell a human which one it got unless it says so, which is why
    /// `evidence` is rendered into the badge's own label rather than only its
    /// tooltip — the same reason `Armed`'s `source` distinguishes a
    /// hook-confirmed compaction from an inferred one (#417).
    Resolved { evidence: ReinjectAck, since_ms: u64 },
}

/// Pure derivation of `CompactionStatus` from already-tracked `AgentEntry`
/// fields — narrates the real state machine, never a parallel vocabulary.
/// Split out (rather than inlined in `group_summary`) so the phase logic is
/// synthetic-input testable without a registry.
pub fn compaction_status(
    compact_pending: bool,
    compact_pending_trusted: bool,
    compact_seen_busy: bool,
    reinject_attempted_ms: Option<u64>,
    reinject_attempts: u32,
    last_lost_reason: Option<&str>,
    last_lost_ms: Option<u64>,
    now: u64,
    evidence: Option<&'static str>,
    // #546: the evidence that resolved the most recent re-grounding, and
    // when — see `AgentEntry::compact_last_ack`.
    last_ack: Option<ReinjectAck>,
    last_ack_ms: Option<u64>,
) -> CompactionStatus {
    if reinject_attempted_ms.is_some() {
        return CompactionStatus::Reinjecting { attempt: reinject_attempts, max_attempts: MAX_REINJECT_ATTEMPTS };
    }
    if compact_pending {
        return if compact_seen_busy {
            CompactionStatus::AwaitingEvidence { trusted: compact_pending_trusted, source: evidence }
        } else {
            CompactionStatus::Armed { trusted: compact_pending_trusted, source: evidence }
        };
    }
    // #546: both of the remaining states are "a recent terminal outcome", and
    // an agent that has compacted more than once can carry a stamp for each.
    // The MORE RECENT one wins rather than a fixed ranking: a fixed one would
    // either let a resolved re-grounding hide a fresh loss (unsafe) or let an
    // old loss hide today's resolution (misleading). Ties go to the loss —
    // the louder of the two, and the only one that asks a human for anything.
    let lost = last_lost_reason
        .zip(last_lost_ms)
        .filter(|(_, ms)| now.saturating_sub(*ms) < COMPACTION_STATUS_RECENT_WINDOW_MS);
    let resolved = last_ack
        .zip(last_ack_ms)
        .filter(|(_, ms)| now.saturating_sub(*ms) < COMPACTION_STATUS_RECENT_WINDOW_MS);
    match (lost, resolved) {
        (Some((reason, lost_ms)), Some((_, ack_ms))) if lost_ms >= ack_ms => {
            CompactionStatus::Abandoned { reason: reason.to_string(), since_ms: lost_ms }
        }
        (Some(_), Some((evidence, ack_ms))) => CompactionStatus::Resolved { evidence, since_ms: ack_ms },
        (Some((reason, lost_ms)), None) => {
            CompactionStatus::Abandoned { reason: reason.to_string(), since_ms: lost_ms }
        }
        (None, Some((evidence, ack_ms))) => CompactionStatus::Resolved { evidence, since_ms: ack_ms },
        (None, None) => CompactionStatus::None,
    }
}

/// Compact-nudge (#328): whether context usage has crossed the group's
/// escalation threshold and escalation hasn't already been latched for this
/// window. `threshold_percent` 0 disables escalation entirely (purely
/// opportunistic). Mirrors the anti-nag shape used everywhere else in this
/// feature (`watchdog_should_notify`, `idle_tick_should_fire`): fire once,
/// don't re-fire until the condition clears (context% drops back under the
/// threshold, e.g. after a compact) and re-crosses it.
pub fn compact_escalation_should_fire(
    percent: u32,
    threshold_percent: u32,
    already_notified: bool,
) -> bool {
    threshold_percent != 0 && percent >= threshold_percent && !already_notified
}

/// Compact-nudge min-context floor (benchtest finding, live evidence: a
/// single-feature testbed session ran 3-4 real compactions, every one of them
/// at 20-31% context — the LULL timer fired at the right quiet moment but the
/// wrong context level, paying a full re-grounding cycle for a pane that
/// wasn't actually full). Gates only the HEURISTIC (lull-timer) fire —
/// `compact_nudge_tick`'s call site never applies this to `requested_fires`
/// (an agent's own `request_compact` is always honored; this is a floor on
/// loomux's own unprompted judgment, not on the agent's).
///
/// **Smart default, resolved HERE rather than in `Guardrails::clamped()`**
/// (rev-65 review round): resolving it at gate-evaluation time, from the
/// live `compact_nudge_minutes` value every tick already reads, means a
/// group that turns the heuristic on LATER via a live setter gets the floor
/// immediately — no re-launch, no re-normalization pass needed. `floor_config`
/// is the tri-state `Guardrails.compact_nudge_min_context_percent`:
/// - `None` (unset): the floor is `DEFAULT_COMPACT_NUDGE_MIN_CONTEXT_PERCENT`
///   (50) whenever the parent feature is on (`nudge_minutes > 0`) — zero
///   config needed to get the fix a live benchtest showed was necessary —
///   and inert (no floor) when the parent is off, since there is nothing to
///   gate either way.
/// - `Some(0)`: explicitly disabled — fire on the lull alone, matching the
///   pre-smart-default behavior, preserved as an explicit opt-out.
/// - `Some(n)`, `n > 0`: an explicit floor.
///
/// A reading not yet available (`percent: None`, `window_unknown: false`)
/// fails OPEN regardless of which state the floor resolves to — never let a
/// missing/stale context reading silently disable the whole heuristic nudge,
/// the same "degrade, don't deny" posture every other opportunistic gate in
/// this codebase takes. That covers a pane before its first reading, and a
/// CLI with no context reader at all (copilot).
///
/// **A reading that has tokens and no window fails CLOSED** (`window_unknown:
/// true`, from [`context_window_unknown`]; #413 S4, the human's decision). That
/// is not a briefly-missing reading but one missing by design — a codex or pi
/// pane whose CLI has not reported a window, and every opencode pane — so
/// failing open would compact it at every lull at any fill level, the
/// 20-30%-full re-grounding this floor exists to stop. Those CLIs compact
/// themselves when they run out of room, so a floor-gated lull compact buys
/// nothing measured. A group `context_window_tokens_override` gives such a
/// pane a window, and with it a percent the floor reads normally. An
/// explicitly disabled floor (`Some(0)`, or the parent feature off) has
/// nothing to fail, and still fires on the lull alone.
pub fn compact_nudge_context_floor_met(
    percent: Option<u32>,
    window_unknown: bool,
    floor_config: Option<u32>,
    nudge_minutes: u32,
) -> bool {
    let effective_floor = match floor_config {
        None if nudge_minutes > 0 => DEFAULT_COMPACT_NUDGE_MIN_CONTEXT_PERCENT,
        None => 0, // parent feature off — nothing to gate; the floor is moot either way
        Some(explicit) => explicit,
    };
    if effective_floor == 0 {
        return true;
    }
    match percent {
        Some(p) => p >= effective_floor,
        None => !window_unknown,
    }
}

/// Whether a context reading has tokens but no window a percent may be
/// computed against (#413 S4) — `modelstate::published_window` refusing the
/// ladder's answer for this reading's source, the ONE rule the lifecycle panel
/// publishes by and `agent_context_percents` escalates by. `false` with no
/// tokens: that is no reading at all, not an unwindowed one. The inputs are
/// the agent's cached reading (`AgentEntry::last_context_*`) and its group's
/// override, so the compact-nudge tick can ask it without a second read.
pub fn context_window_unknown(
    tokens: Option<u64>,
    override_tokens: Option<u64>,
    reported_tokens: Option<u64>,
    reported_rounded: bool,
    model: Option<&str>,
    source: Option<crate::modelstate::ContextSource>,
) -> bool {
    let Some(tokens) = tokens else { return false };
    crate::modelstate::published_window(
        effective_context_window_tokens(override_tokens, reported_tokens, reported_rounded, model, Some(tokens)),
        source,
        reported_tokens,
    )
    .is_none()
}

/// The escalation notice delivered when an agent's context usage crosses the
/// group's configured threshold (#328). Names the actual percent so the
/// agent (and a human reading the pane) can judge urgency, and points at the
/// exact recovery move (`request_compact`) rather than assuming the agent
/// remembers the tool exists.
pub fn compact_escalation_notice(percent: u32) -> String {
    format!(
        "[orrerix] context at {percent}% — offload state (set_state, the task board, any \
         GitHub issues/PRs carrying plan context) and call request_compact at your next \
         stopping point. If you don't, loomux will request one on your behalf and fire it \
         at your next idle moment — better a planned compact now than the CLI's own \
         emergency auto-compact with no offload."
    )
}

/// The human "Compact now" reply (#3407, review rounds 1 and 2): what will
/// actually become of the request.
///
/// Three conditions can hold it, and they are a CONJUNCTION, not a ladder.
/// `compact_nudge_tick` skips a paused group outright, and it fires only
/// when `!compact_pending && requested_fires`, where `requested_fires`
/// carries the hourly-budget check. So the request waits for EVERY condition
/// that holds to clear. Naming only the first one would promise a paste the
/// next one still blocks: round 2's W1 was exactly that, a pending compact
/// named alone while the budget it had just spent kept the request queued
/// for up to an hour. So the reply names every condition that holds and
/// every release it waits for. Only when none holds is "the next idle
/// moment" true. The flag stays set in every case. Pure, so every
/// combination is pinned without a registry.
pub fn human_compact_reply(paused: bool, compact_pending: bool, budget_spent: bool) -> String {
    let mut holds: Vec<String> = Vec::new();
    let mut releases: Vec<&str> = Vec::new();
    if paused {
        holds.push("this group is paused, so nothing is typed into its panes".to_string());
        releases.push("you resume the group");
    }
    if compact_pending {
        holds.push("a compact is already in flight for this pane".to_string());
        releases.push("that compact resolves");
    }
    if budget_spent {
        holds.push(format!("this group has used its {MAX_COMPACT_NUDGES_PER_HOUR} compacts for the hour"));
        releases.push("the oldest of those compacts ages out of the hour");
    }
    if holds.is_empty() {
        return "requested — /compact is typed at the pane's next idle moment".to_string();
    }
    let wait = if releases.len() == 1 { "once" } else { "only once all of these have happened:" };
    format!(
        "queued — {}; /compact fires at the pane's next idle moment {wait} {}",
        holds.join(", and "),
        releases.join(", and ")
    )
}

/// Compact-nudge (#328): whether `request_compact`'s pre-compact offload-
/// checklist warning should be appended to its response. A soft nudge, never
/// a block (the tool call always succeeds) — `last_state_write_ms` 0 means
/// "never observed" and always warns. Pure so the recency rule is testable
/// without a registry.
pub fn compact_checklist_warning(last_state_write_ms: u64, now_ms: u64, window_ms: u64) -> Option<&'static str> {
    if now_ms.saturating_sub(last_state_write_ms) < window_ms {
        return None;
    }
    Some(
        "warning: set_state hasn't been called recently — reconcile the task board and \
         persist durable state (set_state) before this compact lands, or the post-compact \
         re-sync may come back incomplete",
    )
}

/// Compact-nudge (#328): whether the pane's recent output shows a human
/// manually submitting `/compact` — the terminal echoes what was typed, so a
/// submitted `/compact` line appears in the pane's (ANSI-stripped) output
/// tail like any other typed command. Used only to START tracking
/// (`AgentEntry.compact_pending`) for the mandatory post-compact
/// re-injection; loomux pastes nothing in this path — the human already did.
/// Looks for a STANDALONE `/compact` token (whitespace-delimited), not
/// embedded in a longer word/path, so e.g. a line mentioning
/// `src/compact_nudge.rs` can't false-positive. Best-effort and tolerant of
/// the tail's bounded ring: a `/compact` typed further back than the tail
/// window is simply missed, never misdetected as something else. The caller
/// (`compact_nudge_tick`) additionally requires the pane's most recent human
/// input to be within `MANUAL_COMPACT_DETECT_WINDOW_MS` before trusting a
/// match, so stale tail content left over from an ALREADY-handled compact
/// can't re-trigger detection on its own.
pub fn human_typed_compact_detected(tail: &str) -> bool {
    tail.lines().any(|line| line.split_whitespace().any(|tok| tok == "/compact"))
}

/// Directive ledger (#329 expansion): per-CLI stable substrings that appear in
/// a pane's own rendered output while IT (not a human, not loomux) is
/// actively auto-compacting. Keyed per CLI the way `compact_command_for`
/// gates the rest of this feature, so adding a CLI's
/// banner is a one-line addition here, never a change to the generic
/// detection/pipeline code that calls `auto_compact_banner_detected` — this
/// repo is never allowed to bake one CLI's quirks into product code (see
/// CLAUDE.md's toolchain-agnostic constraint; the analogous concern here is
/// *agent-CLI*-agnostic, not toolchain, but the same rule: express it as
/// per-CLI data, not an `if cli == "claude"` buried in the tick).
///
/// Claude Code's own emergency auto-compact renders a spinner line while it
/// runs, observed (1.0.x) as `✢ Compacting conversation… (esc to interrupt ·
/// 8s · ↓ 172 tokens)` — a leading spinner glyph and a trailing elapsed-
/// time/token-count suffix that both change on every repaint, wrapped around
/// a stable `Compacting conversation` core. Matching only that core substring
/// is the documented assumption: it is expected to survive across CLI point
/// releases even if the spinner or counters' exact formatting doesn't, but it
/// is a string this repo does not control and could change in a future
/// release without notice — if this detector stops firing, that is the first
/// thing to re-verify against a current build. No other supported CLI has a
/// known equivalent yet; an empty slice means "never detected for this CLI",
/// not a build error. (That includes gemini, spawnable since #267 stage 2:
/// its banner text is an *observation* of a live pane, and CLAUDE.md
/// constraint 3 forbids spawning one to collect it — so gemini gets an empty
/// slice honestly rather than a guessed literal that would silently match
/// nothing.)
fn auto_compact_banner_substrings(cli: &str) -> &'static [&'static str] {
    match cli {
        "claude" => &["Compacting conversation"],
        _ => &[],
    }
}

/// Directive ledger (#329 expansion): whether `cli`'s pane output tail shows
/// ITS OWN auto-compact running right now — the fourth trigger path, distinct
/// from all three #328 covers (agent-requested, threshold-escalation
/// fallback, human-typed `/compact`), because every one of those is loomux
/// either pasting `/compact` itself or detecting someone else doing so. None
/// of them see the CLI silently deciding on its own that context is full and
/// compacting without asking anyone — the exact incident driving this
/// expansion: an orchestrator came back from one as a generic agent with
/// every mid-session human directive gone, because loomux never learned a
/// compact had happened at all, so the mandatory post-compact re-injection
/// (#328) never fired. Pure so the substring match is testable without a
/// registry; the caller (`compact_nudge_tick`) additionally requires fresh
/// growth on this exact tick (`!currently_quiet`) before trusting a match.
///
/// **Position-anchored, not a bare substring scan (rev review B1 fix).** The
/// first version matched the substring ANYWHERE in the tail, which a `!
/// currently_quiet` growth gate does not close: a busy pane that merely
/// PRINTS or DISCUSSES the banner text (a `gh pr diff` hunk, a grep result, a
/// rust string literal in a code listing, the model streaming a sentence
/// about this very feature — this repo's own source contains the string) IS
/// the growth that satisfies the gate, so the mention and the trigger are the
/// same event; a recency/growth check can never tell them apart. What CAN:
/// the real spinner renders as the live status line — the very last thing on
/// screen while it runs, continuously redrawn in place, with nothing after it
/// until compaction finishes. A quoted mention sits in scrolled content with
/// other lines following it in the same read (the diff continues, the file
/// continues, the sentence has more sentences after it) almost always. So
/// only the tail's LAST non-blank line is checked, never the full tail.
///
/// This is a real reduction in false-positive surface, not a perfect one:
/// the residual, accepted risk is a mention that happens to be the exact
/// last line of output at the instant this tick reads the pane (e.g. a
/// streamed reply that ends its turn on a sentence naming the string, with
/// nothing rendered yet after it). That case is not defended against here —
/// doing so would need either a structural signal from the CLI itself (see
/// `docs/design/orchestration.md`'s note on #397's `PreCompact` hook) or a
/// second-tick confirmation before latching, which was judged not worth the
/// added state for how narrow the remaining window is. If this assumption
/// stops holding in practice, that added confirmation tick is the next move,
/// not a broader substring match.
pub fn auto_compact_banner_detected(cli: &str, tail: &str) -> bool {
    let subs = auto_compact_banner_substrings(cli);
    if subs.is_empty() {
        return false;
    }
    let Some(last_line) = tail.lines().rev().find(|l| !l.trim().is_empty()) else {
        return false;
    };
    subs.iter().any(|s| last_line.contains(s))
}

/// #428 (round 9): stable substrings Copilot's own CLI paints once a
/// compaction FINISHES — the completion-side counterpart of `auto_compact_
/// banner_substrings` (which covers the RUNNING side, Claude only so far).
/// Same per-CLI-data discipline: an unsupported CLI gets an empty slice,
/// never a buried `if cli == "copilot"` in the detection/pipeline code.
///
/// Text captured directly from the live incident #428 reports (the user's
/// own screen observation, quoted in the issue body — not reconstructed
/// from memory): Copilot paints "Compaction completed", "A new checkpoint
/// has been added to your session.", and "Use /session checkpoints N to
/// view the compaction summary." Only the first two are used for
/// matching — the third contains a checkpoint NUMBER that changes every
/// time (`N`), so its literal text never repeats and can't be a stable
/// substring.
///
/// Same fragility this repo already accepts for the running-side banner:
/// UI text, not a documented API, and could change wording in a future
/// Copilot CLI release with no notice. This is deliberately an
/// ACCELERATOR, not the only path — `compact_nudge_tick`'s busy-then-quiet
/// resolver keeps running regardless of whether this ever matches, so a
/// wording change degrades this feature back to today's slower (but
/// still-correct) resolution, never to a hang. If this stops firing,
/// re-verify the exact strings against a current Copilot CLI build first
/// — that is the changelog-watch this fragility calls for, not a broader
/// pattern match.
fn copilot_compaction_marker_substrings(cli: &str) -> &'static [&'static str] {
    match cli {
        "copilot" => &["Compaction completed", "A new checkpoint has been added to your session"],
        _ => &[],
    }
}

/// #413 S5: what `compact_nudge_tick` does with a FRESH Claude `PostCompact`
/// marker this tick — see [`postcompact_marker_disposition`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostCompactDisposition {
    /// Consume the marker and change nothing: its compaction is already
    /// accounted for — resolved natively by a paired `SessionStart(compact)`,
    /// or already in the reinjection-delivery phase.
    Absorb,
    /// Leave the marker on disk and hold this agent's arm resolution for the
    /// tick: the settle window (`POSTCOMPACT_SETTLE_MS`) is still open.
    Settle,
    /// Consume the marker and decide loomux's own reinjection now — trusted
    /// evidence that the compaction finished, with no native re-grounding seen.
    Resolve,
}

/// #413 S5: the pure decision behind Claude's `PostCompact` marker, so each
/// branch is pinned without a registry. Checked in this order:
///
/// 1. **Absorb** when a consumed `SessionStart(compact)` marker's mtime
///    (`sessionstart_seen_ts`) lies within `POSTCOMPACT_SESSIONSTART_PAIR_MS`
///    of this one's — one compaction, already resolved natively, whichever
///    hook wrote first — or when a reinjection is already decided and waiting
///    on its delivery (never re-decide a live phase: rev-10 B1's ordering rule).
/// 2. **Settle** on first sight (`first_seen_ms` `None` — the caller records
///    `now`) and until `POSTCOMPACT_SETTLE_MS` has passed on the tick's clock.
/// 3. **Resolve** after that.
///
/// Both clocks stay separate: the pairing compares two marker mtimes, the
/// settle window two tick `now`s. Mixing them would let a skewed mtime hold an
/// arm open forever.
pub fn postcompact_marker_disposition(
    marker_ts: u64,
    sessionstart_seen_ts: Option<u64>,
    reinjection_decided: bool,
    first_seen_ms: Option<u64>,
    now: u64,
) -> PostCompactDisposition {
    let paired = sessionstart_seen_ts.is_some_and(|s| s.abs_diff(marker_ts) <= POSTCOMPACT_SESSIONSTART_PAIR_MS) && false;
    if paired || reinjection_decided {
        return PostCompactDisposition::Absorb;
    }
    match first_seen_ms {
        Some(first) if now.saturating_sub(first) >= POSTCOMPACT_SETTLE_MS => PostCompactDisposition::Resolve,
        _ => PostCompactDisposition::Settle,
    }
}

/// #428 (round 9): whether Copilot's pane output shows its OWN compaction-
/// completion paint — the fast terminal-path analog of Claude's
/// SessionStart(compact) hook marker (round 7), for a CLI that ships no
/// post-compact hook of any kind (no `postCompact` event; `sessionStart`
/// never fires on compact — both docs-confirmed in earlier #417 rounds).
///
/// Pure substring scan over the WHOLE tail, deliberately NOT last-line-
/// anchored like `auto_compact_banner_detected`: that anchoring works
/// because Claude's running-side banner is a continuously-redrawn spinner
/// (the literal last thing on screen while it runs); Copilot's completion
/// message is a static block printed ONCE, so by the time a later tick
/// reads the pane the agent may already have produced more output after
/// it — anchoring to the last line would miss a real match, not just
/// filter false ones.
///
/// Provenance instead comes entirely from the CALLER's gating
/// (`compact_nudge_tick`): only checked while an arm is already open with
/// no reinjection yet decided (so a stale mention from an already-resolved
/// compaction can never resurrect anything), and only after `compact_
/// inference_guard_until_ms` has passed — the same cooldown `human_typed_
/// compact_detected`/`auto_compact_banner_detected` use so loomux's own
/// recent paste can't satisfy its own detector. That cooldown is belt-
/// and-braces here specifically: loomux never writes either matched
/// sentence into anything it pastes (`compact_reinjection_notice`'s three
/// shapes, `compact_escalation_notice`, or the bare `/compact` command) —
/// checked against those functions' actual bodies, not assumed — so there
/// is no loomux-authored echo this could ever match to begin with. The
/// residual risk is the same class `auto_compact_banner_detected`'s own
/// doc accepts: an agent's own conversational prose happening to discuss
/// this exact wording — judged narrow given these are two specific, full
/// sentences, not one generic word.
pub fn copilot_compaction_marker_detected(cli: &str, tail: &str) -> bool {
    let subs = copilot_compaction_marker_substrings(cli);
    if subs.is_empty() {
        return false;
    }
    subs.iter().any(|s| tail.contains(s))
}

/// Directive ledger (#329 expansion): the ledger section embedded verbatim in
/// the post-compact re-grounding notice, or `None` for an empty/missing
/// ledger — nothing is embedded rather than a header with nothing under it.
/// Keeps the TAIL (most recent entries) when `ledger` exceeds `cap_bytes`,
/// cut on a line boundary so truncation never slices one entry in half, and
/// always includes at least the single newest entry even if it alone exceeds
/// the cap — a directive is never silently dropped for being long, only ever
/// declared truncated. States the truncation and points at `ledger_path` for
/// the full file rather than a silent cap (this repo's no-silent-caps rule
/// for every bounded-coverage feature).
pub fn directive_ledger_embed(ledger: &str, cap_bytes: usize, ledger_path: &str) -> Option<String> {
    let trimmed = ledger.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    let mut tail: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in lines.iter().rev() {
        let line_len = line.len() + 1; // +1 for the newline joining it back
        if used + line_len > cap_bytes && !tail.is_empty() {
            break;
        }
        used += line_len;
        tail.push(line);
    }
    tail.reverse();
    let truncated = tail.len() < lines.len();
    let body = tail.join("\n");
    Some(if truncated {
        format!(
            "Your directive ledger (most recent {} of {} entries — full history at {ledger_path}):\n{body}",
            tail.len(),
            lines.len()
        )
    } else {
        format!("Your directive ledger:\n{body}")
    })
}

/// #417: the freshness discriminator for a compact-lifecycle hook marker —
/// the marker FILE's own mtime, not a timestamp encoded in its content (the
/// generic hook script just creates/truncates it; no clock-formatting logic
/// needed in a script that has to stay portable — see `COMPACT_HOOK_SCRIPT`).
/// `None` for a missing file (the hook never fired, or isn't configured at
/// all) — the caller's existing inference tiers cover that silently.
pub(in crate::orchestration) fn read_hook_marker_ts(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    modified.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as u64)
}

/// Directive ledger (#329 review, N2): trim the STORED ledger file to
/// `cap_bytes`, dropping the OLDEST entries (line boundary, never mid-entry)
/// — the backstop for an agent that notes liberally and never curates via
/// `replace: true`. Returns `(retained_content, dropped_entry_count)`;
/// `dropped == 0` means `ledger` already fit and is returned unchanged (not
/// re-serialized, so a no-op call never even rewrites the file). Unlike
/// `directive_ledger_embed` (which keeps the tail for a one-off notice and
/// leaves the file untouched), this one's output IS the new file — so unlike
/// that function it never force-keeps an over-cap single entry: a ledger
/// that's over cap after keeping only its newest line is left at just that
/// line, however large, since there's nothing older left to drop and
/// truncating an entry's own text is not this function's job.
pub fn ledger_capped(ledger: &str, cap_bytes: usize) -> (String, usize) {
    if ledger.len() <= cap_bytes {
        return (ledger.to_string(), 0);
    }
    let lines: Vec<&str> = ledger.lines().collect();
    let mut tail: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in lines.iter().rev() {
        let line_len = line.len() + 1;
        if used + line_len > cap_bytes && !tail.is_empty() {
            break;
        }
        used += line_len;
        tail.push(line);
    }
    tail.reverse();
    let dropped = lines.len() - tail.len();
    let mut body = tail.join("\n");
    body.push('\n');
    (body, dropped)
}

/// The three post-compact re-grounding shapes `compact_reinjection_notice`
/// can render — round 8 review (N2, rev-16): promoted from a `Option<&str>`
/// to its own enum precisely because the third state below is NOT "some
/// text was found" or "no text was found", it's a distinct THING a compact
/// reinjection can be, mirroring [`ContractCarrier`]'s own three states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReinjectShape {
    /// The FULL contract already durably rides the system-prompt layer —
    /// nothing to re-embed, nothing even worth pointing at.
    Slim,
    /// Only the non-negotiable core durably rides the system-prompt layer —
    /// point at the full instructions file, but never embed it (that would
    /// re-spend exactly the tokens a compaction is supposed to reclaim).
    Pointer,
    /// Nothing durable rides the system-prompt layer — embed the full
    /// contract, read back from the instructions file, verbatim. The one
    /// shape that pays for a read and a re-send.
    Verbose(String),
}

/// The mandatory post-compact re-grounding prompt (#328, extended #329;
/// slimmed #417 correction round 5; given a real third shape round 8),
/// delivered once compaction is detected as finished, regardless of which
/// trigger path (loomux-initiated, agent-requested, threshold-escalation, a
/// human typing `/compact` manually, the CLI's own auto-compact banner, or
/// a trusted hook marker) started it, and regardless of hook-tier vs.
/// inference-tier detection — there is exactly ONE notice shape per agent;
/// which of the three below it gets is a system-prompt-layer fact about
/// THAT agent ([`ContractCarrier`], via `AgentEntry::contract_carrier`), not
/// a property of how the compaction was detected. `instructions_path` is
/// only read for its display text (the `Pointer` shape) — never re-read
/// from disk here; see [`reinject_shape`] for the function that actually
/// decides which of the three this call gets and does any file I/O.
///
/// **Slim (`ReinjectShape::Slim`, `ContractCarrier::SystemLayerFull`):**
/// since #416, the block's FULL CONTRACT rides Claude's own system-prompt
/// layer, every block, unconditionally (round #417 correction 6). Claude's
/// own hooks reference frames `PreCompact`/`SessionStart` purely in terms
/// of conversation summarization — never the system prompt — so
/// re-embedding contract text an agent's own system prompt already holds
/// verbatim, permanently, would be pure waste. Still explicitly re-syncs
/// the things that AREN'T durable anywhere but a live query: the task
/// board (`list_tasks`), durable state (`get_state`), the roster
/// (`list_agents`), and the directive ledger — named by path AND (unlike
/// the three runtime-only sources) inlined as a tail (`ledger_embed`,
/// capped, highest value per byte of anything left to re-send) as
/// belt-and-braces on top of the path pointer, since a directive is
/// qualitatively different from a fact a tool call can re-derive: it can
/// never be re-asked for.
///
/// **Pointer (`ReinjectShape::Pointer`, `ContractCarrier::SystemLayerCore`,
/// NEW in round 8):** Copilot's generated-wrapper happy path. Its system
/// prompt durably holds identity + the non-negotiable mechanics core + a
/// pointer to the full instructions file (`copilot_agent_body`'s doc) —
/// but NOT the full role template, which is what routinely blew GitHub's
/// documented 30,000-character body cap. Before round 8 this state didn't
/// exist as a THIRD option; the bool it used to be forced it into the same
/// bucket as `KickoffOnly` below, so every Copilot compaction paid for a
/// full verbose re-embed of the whole instructions file — right after a
/// compaction meant to reclaim exactly that context, and counter to this
/// notice's own slimming principle. The pointer shape re-syncs the same
/// live-state items the slim shape does, PLUS an explicit instruction to
/// re-read the instructions file — honest about what's missing from the
/// system layer, without paying to re-send it.
///
/// **Verbose (`ReinjectShape::Verbose(text)`, `ContractCarrier::
/// KickoffOnly`, the true fallback):** a Copilot block using a
/// user-authored native `.github/agents/*.md` persona (only the user's OWN
/// file rides `--agent`, never loomux's contract — the documented #416
/// residual gap), the rare `~/.copilot/agents`-unwritable fallback, or an
/// over-cap generated body the round-8 write guard refused to write — has
/// NO system-prompt-layer copy of ANYTHING loomux authored, so the
/// pre-#417-correction-5 full embedding is still the only way to guarantee
/// this agent is grounded in what loomux actually seeded it with, not a
/// paraphrase of a paraphrase. `text` is the exact contract the pane was
/// kickoff'd with (read back from the durable instructions file — see
/// `write_instruction_files`); if even that read fails, [`reinject_shape`]
/// degrades to `Pointer` instead — honest ("go look") beats a false claim
/// of durability, which `Slim` would be here.
pub fn compact_reinjection_notice(shape: &ReinjectShape, instructions_path: &str, ledger_path: &str, ledger_embed: Option<&str>) -> String {
    let ledger_section = match ledger_embed {
        Some(l) => format!("\n\n{l}"),
        None => String::new(),
    };
    match shape {
        ReinjectShape::Verbose(text) => format!(
            "[orrerix] Context was compacted. Re-grounding you in your role instructions before \
             you act — the summary above may have diluted them:\n\n{text}{ledger_section}\n\n\
             Now re-sync live state: list_tasks, get_state, list_agents."
        ),
        ReinjectShape::Pointer => format!(
            "[orrerix] Context was compacted. Your identity and non-negotiable mechanics already \
             ride your CLI's own system prompt and survive this structurally, but your FULL role \
             instructions do not — re-read {instructions_path} before acting; the summary above \
             may have diluted or dropped anything beyond the mechanics. Re-sync live state now: \
             list_tasks (task board), get_state (durable state), list_agents (roster), and your \
             directive ledger at {ledger_path}.{ledger_section}"
        ),
        ReinjectShape::Slim => format!(
            "[orrerix] Context was compacted. Your role contract already rides your CLI's own \
             system prompt and survives this structurally — trust it over anything in the \
             summary above; no need to re-read it. Re-sync live state now: list_tasks (task \
             board), get_state (durable state), list_agents (roster), and your directive ledger \
             at {ledger_path}.{ledger_section}"
        ),
    }
}

/// The orchestration-restore kickoff (#411) — an app restart resuming a live
/// session. `ledger_embed` is `directive_ledger_embed`'s output, folded in
/// for the SAME reason `compact_reinjection_notice` folds it into the
/// post-compact notice: a restart is another surprise discontinuity with no
/// other durable channel back to a directive noted before it struck. `None`
/// (a missing/empty ledger, or a group that never calls `note_directive`)
/// reproduces the pre-#411 fixed string byte-for-byte.
pub fn resume_kickoff_notice(ledger_embed: Option<&str>) -> String {
    let ledger_section = match ledger_embed {
        Some(l) => format!("\n\n{l}"),
        None => String::new(),
    };
    format!(
        "[orrerix] Orchestration restored: your MCP tools, the task board, and the audit log are \
         live again in this session. Re-sync now: list_tasks, list_agents, get_state. Your \
         previous worker panes are gone; resume a task session with spawn_agent(resume_session, \
         cwd) when follow-ups need it. Then give the human a short status summary.{ledger_section}"
    )
}

/// rev-10 review (N3, round 7), widened to a real three-way choice by
/// rev-16 review (N2, round 8): decide which [`ReinjectShape`] a compact
/// reinjection gets for a given [`ContractCarrier`], doing the ONE piece of
/// file I/O this decision can require (the `KickoffOnly` read-back) right
/// here so the `to_reinject` processing loop's caller only needs `&self`
/// for the resulting audit line, not for the decision itself.
///
/// - `SystemLayerFull` → `Slim`, no filesystem touch at all — the common
///   case never pays for a read it wouldn't use.
/// - `SystemLayerCore` → `Pointer`, likewise no filesystem touch — the
///   pointer text names the path, it doesn't need the path's CONTENTS.
/// - `KickoffOnly` → reads the instructions file back. A successful,
///   non-empty read is `Verbose(text)`. A missing or empty read degrades
///   to `Pointer`, not `Slim`: `Slim` would falsely claim this agent's
///   system prompt already holds the contract, which is exactly what
///   `KickoffOnly` means it does NOT. `Pointer` — "go read the file" — is
///   still honest advice even when THIS particular read attempt failed
///   (a momentary race, a permissions blip); the caller audits this
///   specific degradation (`compact-reinjection-contract-unreadable`)
///   since, unlike a genuine `SystemLayerCore` agent, this one should have
///   had a full embed and didn't get one.
pub fn reinject_shape(instructions_path: &Path, carrier: ContractCarrier) -> ReinjectShape {
    match carrier {
        ContractCarrier::SystemLayerFull => ReinjectShape::Slim,
        ContractCarrier::SystemLayerCore => ReinjectShape::Pointer,
        ContractCarrier::KickoffOnly => match fs::read_to_string(instructions_path) {
            Ok(s) if !s.trim().is_empty() => ReinjectShape::Verbose(s),
            _ => ReinjectShape::Pointer,
        },
    }
}
