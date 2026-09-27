//! Pasting and submitting a delivery, and confirming it landed: the outcome
//! record, pane readiness, prompt-submit records and confirmation state.
//! Design note: `docs/design/pty-input-path.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): `lock_safe`
//! (`crate::obs`). IO: fs. Sibling files it calls: `grouppath.rs`, `tier1.rs`.

use super::*;
/// Output growth (bytes) within the window that counts as a landed submit.
/// Set well above idle cursor-blink noise so confirmation biases against false
/// positives: a false "unconfirmed" only costs a harmless no-op flush next
/// time, whereas a false "confirmed" would let stranded text merge.
const SUBMIT_CONFIRM_MIN_BYTES: u64 = 24;

/// Wrap prompt text in a bracketed paste so multi-line prompts land in the
/// CLI's input box instead of submitting at the first newline. The Enter is
/// sent separately after `PASTE_SUBMIT_DELAY`.
pub fn bracketed_paste(text: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(text.len() + 12);
    v.extend_from_slice(b"\x1b[200~");
    v.extend_from_slice(text.replace("\r\n", "\n").as_bytes());
    v.extend_from_slice(b"\x1b[201~");
    v
}

/// The byte sequence loomux writes to submit a delivered prompt, chosen per
/// CLI. Kept pure and `&'static` so the exact bytes are unit-assertable.
///
/// Claude Code submits on a bare CR (`\r`).
///
/// GitHub Copilot's TUI (#98) gates *keyboard* input on terminal focus: it
/// enables DEC mode 1004 focus reporting (`ESC[?1004h`) and, in its editor's
/// key handler, drops every non-paste keystroke while its focus flag is false
/// (`if (!focused && !key.paste && code != backspace/delete) return`). A
/// *paste* bypasses that guard — which is why a delivered prompt's text lands
/// in the input box — but the Enter that follows is a plain key, so on a pane
/// that isn't the focused one (the normal case when an agent delivers to
/// another agent's pane) it is ignored and the prompt just sits there until a
/// human clicks in (whereupon the terminal emits focus-in and their Enter
/// works). Prefixing the CR with a focus-in report (`ESC[I`, which Copilot
/// parses to a focus event that flips its flag true) makes the very next key —
/// our Enter — accepted, so the prompt submits without a human. Copilot leaves
/// its flag true afterward, so the spaced retry Enters need no re-prefix, but
/// they carry it too so each retry is self-sufficient if a stray blur arrives.
pub fn submit_sequence(cli: &str) -> &'static [u8] {
    match cli {
        "copilot" => b"\x1b[I\r",
        _ => b"\r",
    }
}

/// The outcome of the most recent delivery to a pane, kept in-memory per pty so
/// the next delivery can detect a previous prompt still stranded in the input
/// box (#81/#84). The TYPE is `pub` (#420 rev-19 R3) so `record_aborted_
/// preenter_outcome`/`recorded_confirmed` can name it in their signatures —
/// fields stay private, so outside code (including tests) still can't
/// construct or read one directly, only through those two functions.
#[derive(Clone, Debug)]
pub struct DeliveryOutcome {
    /// Whether that delivery's Enter was observed to submit (box cleared / turn
    /// started). `false` means the text may still be sitting unsubmitted.
    pub(in crate::orchestration) confirmed: bool,
    /// Unix-ms the final Enter was sent — the reference point for deciding
    /// whether a human has since typed into the pane.
    pub(in crate::orchestration) submit_sent_ms: u64,
    /// Production bug fix (PR #329 round 7): the delivery's `from` (the
    /// `deliver_prompt` caller — [`brand::AUDIT_ACTOR`] for every compact-nudge notice,
    /// `"human"` for a message typed through loomux's UI, an agent id for a
    /// forwarded `message_orchestrator`). See `AgentEntry::compact_
    /// inference_guard_until_ms`'s doc for why this specific distinction
    /// matters.
    pub(in crate::orchestration) from: String,
    /// #813 round 2: the text this delivery left sitting in the pane's box, for
    /// readers that must ask **whether our own text is still there** — a
    /// question no other field on this record, and no other signal on the pane,
    /// can answer.
    ///
    /// `input_box_len` cannot answer it: that counter tracks characters the
    /// HUMAN typed (`PtyManager::note_user_input`), and loomux's own paste goes
    /// out through `write_bytes`, which never touches it. So `!input_pending`
    /// means "no human characters are outstanding", NOT "the box is empty", and
    /// a reader that treats the two as the same thing is reasoning about a
    /// different pane than the one in front of it. That conflation is exactly
    /// what #813's first cut got wrong.
    ///
    /// `None` where there is no text to look for — a record from a path that
    /// pasted nothing, or one that predates this field. Every consumer treats
    /// `None` as "no reading available" and takes the conservative branch,
    /// never as "the box is clear".
    pub(in crate::orchestration) stranded_text: Option<String>,
}

/// Record a pre-Enter question-abort's outcome (#420 rev-15 B3, extracted
/// rev-19 R3): the paste already landed but the Enter was withheld, so the
/// text is sitting unsubmitted in the box — record it exactly like a normal
/// (non-aborted) delivery would, so the NEXT delivery's `flush_stranded_text`
/// can see it's stranded and clear it. Extracted into its own function (not
/// inlined at the one call site) so an integration test can assert the
/// INSERT itself happens — deleting the call `deliver_prompt` makes to this
/// function is exactly the rev-19 mutation that must fail a dedicated test,
/// not just a test of the downstream consequence a test could fabricate the
/// input for. `DeliveryOutcome` stays private (this crate's own precedent,
/// see `DeliveryConfirmation`'s doc, for not exposing delivery-plumbing
/// internals) — callers (including tests) never need to name it: they own
/// the map through `Mutex<HashMap<u32, _>>` and let this function's
/// signature pin the value type.
#[doc(hidden)] // pub for integration tests
pub fn record_aborted_preenter_outcome(
    last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>,
    pty_id: u32,
    delivery_from: String,
    // #813: the text that is sitting in the box right now. This call site is
    // the one that KNOWS it — the paste it made is the stranded text — and the
    // marker queued immediately after carries no text of its own, so if it is
    // not recorded here nothing downstream can ever ask whether it is still
    // there.
    pasted_text: Option<String>,
) {
    record_inflight_delivery(last_delivery, pty_id, now_ms(), delivery_from, pasted_text);
}

/// Publish a delivery's OWNERSHIP of a pane into the ledger (#454) — the
/// same `DeliveryOutcome` shape every other writer uses, `confirmed: false`,
/// written at a moment when nothing about the outcome is known yet.
///
/// This is what makes supersession a START-of-delivery fact rather than an
/// END-of-delivery one. `deliver_now` calls it immediately before its FIRST
/// Enter, so for any `promptsubmit` record that Enter can produce there is a
/// happens-before chain — **ledger insert ≺ Enter ≺ hook record** — and an
/// OLDER delivery's late monitor, which takes its ledger observation AFTER
/// its hook read (see `observe_ledger`'s call site in
/// `run_late_confirmation_monitor`), can never see that record while still
/// believing it owns the pane.
///
/// Before this, the ledger was written only at the END of the newer
/// delivery's confirm window (≲1s typical, ~9s worst case). That left
/// exactly that window open for a stale monitor to read "still mine" from
/// the ledger, then match the NEWER delivery's hook record, and resolve its
/// OWN delivery off it — a misattributed `delivery-confirmed-late` audit row
/// and, if that monitor had already declared `Failed`, a "no re-send needed"
/// correction notice about a re-send that is the only reason anything landed
/// at all. #451 B1's supersession rule closed the dangerous version of that
/// (a correction arriving BEFORE a re-send and suppressing it); #454 is the
/// narrowed residual it deferred, and this is the "compare against an
/// in-flight delivery marker" fix that issue asked for — closing the window
/// rather than narrowing it further.
///
/// Note the marker is not a fourth piece of state: it is the SAME map, under
/// the SAME lock, so the monitor still decides from one atomic read. A
/// separate `in_flight` map would have re-opened the hazard one level down —
/// two locks means a torn observation (read the in-flight map, miss the
/// newer delivery, then read the ledger), which is the fused-lock mistake
/// #496 PR-C's own admission gate had to be fixed for (rev-47 B1).
///
/// Extracted (not inlined at its one call site) for the same reason
/// `record_aborted_preenter_outcome` is: deleting the call `deliver_now`
/// makes to it is precisely the mutation a dedicated test must catch.
#[doc(hidden)] // pub for integration tests
pub fn record_inflight_delivery(
    last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>,
    pty_id: u32,
    submit_sent_ms: u64,
    delivery_from: String,
    // #813: see `DeliveryOutcome::stranded_text`.
    pasted_text: Option<String>,
) {
    last_delivery.lock_safe().insert(pty_id, DeliveryOutcome {
        confirmed: false,
        submit_sent_ms,
        from: delivery_from,
        stranded_text: pasted_text,
    });
}

/// One ATOMIC observation of a pane's delivery ledger, from the point of
/// view of the delivery that pressed Enter at `submit_sent_ms` (#454).
///
/// Every fact `run_late_confirmation_monitor` decides from comes from this
/// one read, so no two of them can be drawn from different moments. The
/// monitor used to derive `superseded` and `ledger_outstanding` from one
/// snapshot already and then re-derive `outstanding` inline later in the
/// same tick; this makes the single-observation discipline the type's job
/// rather than a convention the next edit can quietly break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)] // pub for integration tests
pub struct LedgerView {
    /// A DIFFERENT delivery owns this pane now — this monitor must exit
    /// without writing or notifying anything (`MonitorAction::Superseded`).
    pub superseded: bool,
    /// The recorded outcome is still THIS delivery's and still unconfirmed —
    /// the durable "not landed yet" fact #496's self-heal judges from, ahead
    /// of any pane reading.
    pub outstanding: bool,
    /// Superseded AND the newer delivery is recorded confirmed — the pane is
    /// demonstrably unwedged, so a stranded badge can come down on the way
    /// out. A newer UNCONFIRMED outcome leaves it up: that delivery's own
    /// monitor (or, if it confirmed in-window, `deliver_now` itself) owns the
    /// pane's state from here.
    pub newer_confirmed: bool,
}

#[doc(hidden)] // pub for integration tests
pub fn observe_ledger(
    last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>,
    pty_id: u32,
    submit_sent_ms: u64,
) -> LedgerView {
    let recorded = last_delivery.lock_safe().get(&pty_id).cloned();
    let superseded = recorded.as_ref().is_some_and(|o| o.submit_sent_ms != submit_sent_ms);
    LedgerView {
        superseded,
        outstanding: recorded
            .as_ref()
            .is_some_and(|o| o.submit_sent_ms == submit_sent_ms && !o.confirmed),
        newer_confirmed: superseded && recorded.as_ref().is_some_and(|o| o.confirmed),
    }
}

/// Whether the pane's most recently recorded delivery outcome reads as
/// "confirmed" (#420 rev-19 R3) — the exact extraction `deliver_prompt`'s
/// flush step performs on `last_delivery` (`prev.as_ref().map(|o|
/// o.confirmed)`), pulled out so a test can read back what
/// `record_aborted_preenter_outcome` stored without naming `DeliveryOutcome`
/// either.
#[doc(hidden)] // pub for integration tests
pub fn recorded_confirmed(last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>, pty_id: u32) -> Option<bool> {
    last_delivery.lock_safe().get(&pty_id).map(|o| o.confirmed)
}

/// Why a live, idle, typeable pane on the right session is **not** ready to take
/// a brief (#2089). `None` from [`pane_delivery_readiness`] means it is.
///
/// Every variant is a fact orrerix's own delivery machinery already recorded —
/// never a reading of the pane's screen, which `docs/design/review-driver.md` §3
/// keeps out of the review driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneNotReady {
    /// Something is already waiting to be pasted into this pane. Whatever the
    /// CLI is doing, it has not drained what it was last given, so a brief
    /// admitted now lands BEHIND that.
    Queued,
    /// The last delivery to this pane is on record as not having landed —
    /// `Pending` or `Failed` in [`DeliveryConfirmState`]'s three-state sense,
    /// which `DeliveryOutcome::confirmed` folds into one `false`. Its text may
    /// still be sitting unsubmitted in the box.
    ///
    /// **The complement is WIDER than the hook signal**, and that is disclosed
    /// rather than implied: [`confirm_state_for`] resolves `Box`, `Hook` AND
    /// `Burst` to `Confirmed`, so a delivery decided by #112's Tier 3 output
    /// heuristic reads ready here even though a repaint can satisfy that tier.
    /// Narrowing to `Box`/`Hook` needs [`ConfirmSource`] carried on
    /// `DeliveryOutcome`, which nothing stores; the trade and how to settle it
    /// from `prompt-typed`'s own `confirm_source` column are in
    /// `docs/design/review-driver.md` §3.1 item 5.
    Unconfirmed,
    /// Nothing has ever been delivered to this pty, so there is no evidence
    /// either way. Refused rather than assumed: "we could not look" is not
    /// "there was nothing there" — the same asymmetry `rd_pane_exit` states
    /// about a `Dead` reading.
    NoRecord,
}

impl PaneNotReady {
    pub fn as_str(self) -> &'static str {
        match self {
            PaneNotReady::Queued => "queued",
            PaneNotReady::Unconfirmed => "unconfirmed",
            PaneNotReady::NoRecord => "no-record",
        }
    }
}

/// **Is a pane at a point where a brief typed into it will be READ rather than
/// parked behind something?** (#2089) `None` = ready.
///
/// Pure, so the rule itself is pinnable without a live pty; the impure reader is
/// [`OrchRegistry::pane_readiness`], which supplies
/// [`OrchRegistry::queue_depth`] and [`recorded_confirmed`].
///
/// **Queue depth is asked FIRST**, and the precedence is a decision rather than
/// an ordering accident: a non-empty queue decides on its own, whatever the last
/// delivery did, because the brief would sit behind entries nothing has pasted
/// yet. `Unconfirmed` and `NoRecord` are only ever reached for an empty queue.
///
/// **What this CANNOT see, stated because a predicate must state its residual.**
/// It is evidence about the last DELIVERY, not about the pane right now: a
/// dialog raised AFTER a confirmed delivery, with nothing queued behind it,
/// reads ready and is not caught. Closing that would mean judging a pane's
/// screen, which §3 forbids the driver; it is bounded exactly as it was before
/// #2089, by `held(fix-stalled)`/`held(lane-stalled)` naming the pane.
///
/// Nor is it ATOMIC with the paste that follows it: anything admitted to the
/// pane's queue between this answer and `deliver_prompt` is pasted first. The
/// window is not widened by #2089 — the arm it replaces had the same gap with no
/// readiness read to race — but "ready" means "was ready when asked", not "will
/// still be when the brief lands". See `docs/design/review-driver.md` §3.1 item 5
/// for both residuals and the trade behind the wider `Confirmed`.
#[doc(hidden)] // pub for integration tests
pub fn pane_delivery_readiness(
    queue_depth: usize,
    last_confirmed: Option<bool>,
) -> Option<PaneNotReady> {
    if queue_depth > 0 {
        return Some(PaneNotReady::Queued);
    }
    match last_confirmed {
        Some(true) => None,
        Some(false) => Some(PaneNotReady::Unconfirmed),
        None => Some(PaneNotReady::NoRecord),
    }
}

/// What [`OrchRegistry::idle_pane_on_session`] found: the pane to reuse, if any,
/// and every candidate that got as far as the readiness test and was refused by
/// it (#2089).
///
/// `declined` exists so that refusal is not SILENT. Its only other visible
/// effect is a fresh pane, and on the audit log that is indistinguishable from
/// there having been no candidate at all — the shape of silence §3 objects to.
/// `rd_reuse_pane` turns each entry into an `rd-reuse-declined` row (§5.4).
pub struct ReusablePane {
    pub agent: Option<String>,
    pub declined: Vec<(String, PaneNotReady)>,
}

/// Record a stranded (unconfirmed) delivery whose submit happened at an
/// EXPLICIT time (#518, integration tests only). `record_aborted_preenter_
/// outcome` always stamps `now_ms()`, which cannot express the one state
/// #518's bound is about: a delivery whose submit is in the past, with a
/// keystroke stamp after it that has since gone stale. Paired with
/// `PtyManager::set_user_input_ms_for_test`, this lets a test place both ends
/// of that comparison without sleeping out the ten-minute bound — and without
/// making `DeliveryOutcome` public, which this file's own precedent
/// (`DeliveryConfirmation`'s doc) argues against.
#[doc(hidden)] // pub for integration tests
pub fn record_stranded_outcome_at_for_test(
    last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>,
    pty_id: u32,
    delivery_from: String,
    submit_sent_ms: u64,
    // #813: the stranded text this back-dated record is about, so a test can
    // drive the box reading the real decision now turns on.
    pasted_text: Option<String>,
) {
    last_delivery.lock_safe().insert(pty_id, DeliveryOutcome {
        confirmed: false,
        submit_sent_ms,
        from: delivery_from,
        stranded_text: pasted_text,
    });
}

/// Production bug fix (rev-42 delta, round 2): a per-agent snapshot of the
/// most recent delivery to that agent's pane, in the shape `compact_nudge_
/// tick`'s reinjection-confirmation resolver needs. Mirrors the private
/// `DeliveryOutcome` above (same two fields, same meaning) but kept as its
/// own `pub` type rather than making delivery-plumbing internals public: a
/// synthetic-input test constructs one directly, with no live pty/app
/// handle round trip — `deliver_prompt` is fire-and-forget and unit tests
/// can't exercise it for real (D1's rejected precedent, see the design
/// doc), so this is threaded through as an ordinary input map exactly like
/// `context_tokens`/`context_boundary_counts`, with `agent_last_deliveries`
/// as the impure reader that supplies it in production.
#[derive(Clone, Debug)]
pub struct DeliveryConfirmation {
    pub submit_sent_ms: u64,
    pub confirmed: bool,
    /// See `DeliveryOutcome::from`'s doc — mirrored verbatim.
    pub from: String,
}

/// Whether output growth after the submit Enter counts as the submit landing.
/// A successful submit clears the box and the CLI repaints / starts a turn (a
/// burst of output); an ignored Enter produces effectively none.
///
/// Only trustworthy when the pane reached quiet *before* the Enter. If the
/// submit-wait hit `SUBMIT_MAX_WAIT` while output was still streaming
/// (`reached_quiet == false`), the Enter landed mid-stream and the window's
/// growth is that stream, not the submit's — which would false-confirm and
/// strand a prompt recorded as confirmed (rev-32). So a cap-hit-without-quiet
/// is never confirmed; a false "unconfirmed" is just a harmless flush next
/// time. Pure so the rule is testable; the polling loop lives in
/// `deliver_prompt`.
pub fn submit_confirmed(reached_quiet: bool, baseline_total: u64, observed_total: u64) -> bool {
    reached_quiet && observed_total.saturating_sub(baseline_total) >= SUBMIT_CONFIRM_MIN_BYTES
}

// ─────────────────────── #112: real prompt-landed hook signal ───────────────────────
//
// `submit_confirmed` above trusts ANY output burst after Enter as evidence the
// prompt landed — error repaints, dialog interactions, and spinner ticks all
// clear its 24-byte bar, so the two live failures in #112's issue body were both
// recorded confirmed while the task was in fact destroyed (false confirm). The
// SAME missing signal produces the inverse failure too: a busy pane that never
// reaches quiet skips this heuristic entirely (`while reached_quiet && ...` in
// `deliver_prompt`), which is why 4 of 5 spawns drew a spurious "unconfirmed"
// notice in the #112 field-evidence comment even though every one had actually
// landed (false unconfirm).
//
// The fix is an AUTHORITATIVE signal, not a retuned threshold: Claude Code's
// `UserPromptSubmit` hook fires "when you submit a prompt, before Claude
// processes it" (code.claude.com/docs/en/hooks, "UserPromptSubmit input"
// section — fetched and grepped directly, not inferred), with no matcher
// (always fires) and the submitted text on stdin as JSON under the `prompt`
// field ("UserPromptSubmit hooks receive the `prompt` field containing the
// text the user submitted" — verbatim). `user_input`/`user_prompt` are
// tolerated as legacy/cross-CLI fallback field names only, never the
// documented one — round 1 review caught this module citing `user_input` as
// primary, which the live page does not contain at all. Copilot's
// `userPromptSubmitted` fires when "The user
// submits a prompt" (docs.github.com/en/copilot/reference/hooks-reference,
// "userPromptSubmitted" section) but that page does NOT document the payload
// TRANSPORT for this event (unlike Claude's stdin-JSON contract, which the
// hooks reference nails down explicitly) — so the Copilot arm never attempts to
// capture prompt text at all (see `PromptSubmitRecord`'s doc); it degrades to
// an existence+offset marker, still strictly better than the burst heuristic
// for a busy pane, at the cost of the content-match precision Claude's tier
// gets. This is a DOCS-SILENT residual, not an assumption papered over.
//
// A second docs-silent residual: whether a submission that gets swallowed/
// misparsed as an unknown slash command (the exact `/model`-merge failure
// #112's issue documents) still fires `UserPromptSubmit` at all. Neither
// reference page says. If it doesn't fire, that case stays *unconfirmed* under
// this design — the correct, safe direction (rev-32's own "never false-confirm"
// property, preserved) — so this is left unresolved rather than guessed at.
//
// SAFETY-CRITICAL per the hooks reference: exit code 2 on `UserPromptSubmit`
// "blocks prompt processing and erases the prompt", and on exit 0 anything
// printed to stdout "is added as context Claude can see". `COMPACT_HOOK_
// SCRIPT`'s new `promptsubmit` arm (below) is therefore held to the SAME
// unconditional-exit-0/no-stdout discipline the file's own doc already argues
// for `precompact`/`sessionstart-compact`, extended here to a case where the
// hazard of getting it wrong is destroying the user's own prompt, not merely
// skipping a compact.

/// A single `promptsubmit` hook record, read from `<agent-id>.promptsubmit.
/// jsonl` in the group's `hooks/` dir (#112) — one JSON line per
/// `UserPromptSubmit`/`userPromptSubmitted` firing since the marker file was
/// created, appended by the SAME generic hook script/Copilot command that
/// already write `.precompact.json`/`.sessionstart-compact.json` there (see
/// `COMPACT_HOOK_SCRIPT`'s doc).
///
/// `text` is `None` for every Copilot record and for an unparseable (e.g.
/// torn mid-write) line — see the module doc above for why Copilot's payload
/// transport is never read at all. A `None` record still counts as evidence
/// *something* fired (the existence tier — `PromptLandedMatch::Existence`);
/// it just can't be matched against what THIS delivery pasted.
#[derive(Debug, Clone, PartialEq, Eq)]
#[doc(hidden)] // pub for integration tests
pub struct PromptSubmitRecord {
    pub text: Option<String>,
}

/// Parse `content` (a `promptsubmit` marker file's full text) into the
/// records written since byte `offset` — the delivery's OWN baseline,
/// snapshotted before it pasted anything, so a record from an earlier
/// delivery to the same pane (or a human's own prompt) can never satisfy
/// THIS delivery's confirmation by construction. `offset` is clamped via
/// `str::get` rather than sliced directly: a torn read racing a concurrent
/// write could land mid-character on a multi-byte UTF-8 boundary, and an
/// out-of-bounds/invalid-boundary offset degrades to "no new records yet"
/// (an empty tail) rather than panicking the delivery thread.
///
/// Each non-empty line is one record. Valid JSON with a recognized text
/// field (`prompt` — the documented Claude field, per the "UserPromptSubmit
/// input" section of code.claude.com/docs/en/hooks: "UserPromptSubmit hooks
/// receive the `prompt` field containing the text the user submitted";
/// `user_input`/`user_prompt` tolerated as legacy/cross-CLI fallbacks only)
/// yields `text: Some(..)`.
/// Anything else non-empty (Copilot's existence-only marker line, or a
/// trailing line still mid-`>>` when this races the hook script's own
/// write) yields `text: None` rather than being dropped — losing a whole
/// poll cycle's worth of existence evidence to a torn read would be exactly
/// the false-unconfirm failure mode this feature exists to close.
#[doc(hidden)] // pub for integration tests
pub fn promptsubmit_records_since(content: &str, offset: usize) -> Vec<PromptSubmitRecord> {
    let tail = content.get(offset..).unwrap_or("");
    tail.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let text = serde_json::from_str::<Value>(line).ok().and_then(|v| {
                v.get("prompt")
                    .or_else(|| v.get("user_input"))
                    .or_else(|| v.get("user_prompt"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            });
            PromptSubmitRecord { text }
        })
        .collect()
}

/// The three tiers a delivery's `promptsubmit` hook records can resolve to
/// against the text it pasted, in ascending strength:
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)] // pub for integration tests
pub enum PromptLandedMatch {
    /// No record since baseline is any kind of evidence.
    None,
    /// A record fired since baseline, but its tier never captures text at
    /// all (every Copilot record today — see `PromptSubmitRecord`'s doc), so
    /// there is nothing to match content against. Trusted on the strength of
    /// the baseline alone: the hook is documented to fire unconditionally on
    /// every submission, and the baseline already excludes every record but
    /// the ones THIS delivery's own submit could have produced.
    Existence,
    /// A record's own (normalized) text CONTAINS this delivery's normalized
    /// paste — the strongest tier. `merged: true` means containment, not
    /// equality: the exact "prompt merged with human-typed `/model`" shape
    /// from #112's own issue body, still counted as landed (the agent DID
    /// receive the task text — plan-14 decision #3) but flagged so the audit
    /// can distinguish a clean submit from a merged one.
    Content { merged: bool },
}

/// Resolve `records` (already filtered to since-baseline by
/// `promptsubmit_records_since`) against `pasted` — the pure decision half
/// of the hook confirmation tier. An empty normalized `pasted` (should never
/// happen — `deliver_prompt` never pastes empty text) resolves to `None`
/// rather than trivially matching every record via an empty-string
/// containment check.
#[doc(hidden)] // pub for integration tests
pub fn prompt_landed(records: &[PromptSubmitRecord], pasted: &str) -> PromptLandedMatch {
    let norm_pasted = normalize_prompt_text(pasted);
    if norm_pasted.is_empty() {
        return PromptLandedMatch::None;
    }
    let mut existence = false;
    for r in records {
        match &r.text {
            Some(t) => {
                let norm_t = normalize_prompt_text(t);
                if norm_t.contains(&norm_pasted) {
                    return PromptLandedMatch::Content { merged: norm_t != norm_pasted };
                }
            }
            None => existence = true,
        }
    }
    if existence { PromptLandedMatch::Existence } else { PromptLandedMatch::None }
}

/// Which tier decided a delivery's outcome — carried into the `prompt-typed`
/// audit event (`confirm_source`) so every direction stays distinguishable
/// after the fact. #112 round 2 (three-state redesign — see the design note
/// section of the same name): `"box"`/`"box_veto"` are Tier 1 (two-sided,
/// only when its precondition verified — `box_holds_paste`'s doc); `"hook"`
/// is Tier 2 (the `promptsubmit` marker, in-window or late); `"burst"` is
/// Tier 3 (rev-32's output heuristic, last resort); `"idle"` is the extended
/// monitor's own trigger (pane genuinely quiet, no question on screen, still
/// no evidence — `PENDING_IDLE_QUIET`'s doc); `"none"` means nothing has
/// decided yet (the delivery is `DeliveryConfirmState::Pending`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmSource {
    Box,
    Hook,
    Burst,
    BoxVeto,
    Idle,
    None,
}

impl ConfirmSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ConfirmSource::Box => "box",
            ConfirmSource::Hook => "hook",
            ConfirmSource::Burst => "burst",
            ConfirmSource::BoxVeto => "box_veto",
            ConfirmSource::Idle => "idle",
            ConfirmSource::None => "none",
        }
    }
}

/// The three-state outcome a delivery resolves to (#112 round 2) — replacing
/// the old confirmed/unconfirmed binary. `Pending` is the state the round-1
/// design didn't have a name for: no evidence yet, which for a prompt queued
/// into a busy pane is the NORMAL, CORRECT state, potentially for a long
/// time, not a failure. Only `Failed` should ever draw the orchestrator's
/// unconfirmed notice — never `Pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryConfirmState {
    Confirmed,
    Pending,
    Failed,
}

impl DeliveryConfirmState {
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryConfirmState::Confirmed => "confirmed",
            DeliveryConfirmState::Pending => "pending",
            DeliveryConfirmState::Failed => "failed",
        }
    }
}

/// Map a decided `ConfirmSource` to its `DeliveryConfirmState` — pure so the
/// mapping itself is pinnable (a mutation swapping `BoxVeto`'s arm for
/// `Confirmed` would be exactly backwards, and a dedicated test catches it
/// directly rather than through some downstream consequence).
#[doc(hidden)] // pub for integration tests
pub fn confirm_state_for(source: ConfirmSource) -> DeliveryConfirmState {
    match source {
        ConfirmSource::Box | ConfirmSource::Hook | ConfirmSource::Burst => DeliveryConfirmState::Confirmed,
        ConfirmSource::BoxVeto | ConfirmSource::Idle => DeliveryConfirmState::Failed,
        ConfirmSource::None => DeliveryConfirmState::Pending,
    }
}

/// #112 round 3 (rev-20 B3): may Tier 1's own box-consumption reading be
/// trusted to decide anything RIGHT NOW? Any human input since OUR OWN
/// submit contaminates the reading in BOTH directions — a box that cleared
/// because the human typed/submitted/cancelled their own line reads
/// identically to one the CLI cleared for our delivery, and a box that's
/// STILL occupied because the human is mid-edit reads identically to one
/// the CLI never took. Once contaminated, Tier 1 must decline for the rest
/// of this delivery's decision (the delivery falls to `Pending`, and the
/// question-guarded late monitor decides from there) — this is what makes
/// the design note's "closed via `last_user_input_ms`" claim actually true
/// rather than aspirational.
#[doc(hidden)] // pub for integration tests
pub fn tier1_trusted(last_user_input_ms: u64, submit_sent_ms: u64) -> bool {
    last_user_input_ms <= submit_sent_ms
}

/// The `promptsubmit` hook marker path for one agent — a sibling of the
/// `.precompact.json`/`.sessionstart-compact.json` markers in the same
/// group's `hooks/` dir (`read_hook_marker_ts`'s doc). JSONL, not a single
/// overwritten file: unlike the compact markers (where only the LATEST
/// firing's mtime matters), confirmation here needs to see every record
/// since a per-delivery baseline OFFSET, so appending (never truncating) is
/// required.
///
/// #904: takes a [`GroupId`] and builds through [`group_dir_at`], so it is
/// infallible again — there is no id it can be handed that would escape the
/// root. It briefly returned `Option` during the first slice, when it still
/// took a `&str` and validated at its own join; that was one of the raw joins
/// the second slice removed.
#[doc(hidden)] // pub for integration tests
/// **Takes a validated agent id (#925), for the same reason it takes a
/// `GroupId`.** The id becomes part of a FILE NAME under the group's `hooks`
/// dir, so an unvalidated string here is a second path-assembly point wearing a
/// `format!`. Infallible by construction rather than by trust: the caller has to
/// hold the proof before it can call, which is what keeps this function's return
/// type a plain `PathBuf` instead of reintroducing the `Option` #904 removed.
pub fn promptsubmit_marker_path(root: &Path, group: &GroupId, agent_id: &PathSegment) -> PathBuf {
    group_dir_at(root, group)
        .join("hooks")
        .join(format!("{agent_id}.promptsubmit.jsonl"))
}

/// This delivery's baseline byte length into the `promptsubmit` marker,
/// snapshotted before it pastes anything (see `promptsubmit_records_since`'s
/// doc for why). `0` for a missing file — no hook has fired for this agent
/// this session, and offset 0 is a safe baseline (every record in the file,
/// once one exists, counts).
#[doc(hidden)] // pub for integration tests
pub fn promptsubmit_marker_len(path: &Path) -> usize {
    fs::metadata(path).map(|m| m.len() as usize).unwrap_or(0)
}

/// Read the `promptsubmit` marker and resolve it against `pasted` since
/// `offset` — the impure half of the hook confirmation tier
/// (`promptsubmit_records_since` + `prompt_landed` are the pure decision).
/// A missing/unreadable file resolves to `PromptLandedMatch::None`, the same
/// "hook never fired / isn't configured" degrade every other reader in this
/// module uses (`read_hook_marker_ts`'s doc) — never an error the delivery
/// thread has to branch on.
#[doc(hidden)] // pub for integration tests
pub fn poll_promptsubmit_hook(path: &Path, offset: usize, pasted: &str) -> PromptLandedMatch {
    let Ok(content) = fs::read_to_string(path) else { return PromptLandedMatch::None };
    prompt_landed(&promptsubmit_records_since(&content, offset), pasted)
}
