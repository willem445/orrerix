//! §5.2 `<group-dir>/review_drives.json`: the file's shape ([`ReviewDrivesState`],
//! [`LaneRecord`], [`DrivenPane`]), its read and atomic write, and §5.2's retention
//! ([`prune_terminal`]).
//!
//! Part of the review driver's pure core, split out of the former single-file
//! `reviewdrive.rs` by #3498 P7 as a pure move. The module map is in `mod.rs`;
//! the design note is `docs/design/review-driver.md`.

use super::*;

// ── §5.2 `<group-dir>/review_drives.json` ───────────────────────────────────
//
// Forward compatibility here is the merge queue's, and the OPPOSITE of
// `.orrerix/workflow.yml`'s — §5.2 flags the asymmetry itself, because the two
// persisted surfaces this design adds take opposite postures and a reader will
// otherwise infer one from the other: **policy fails loud, state degrades
// gracefully.** `workflow.yml` is human-authored policy, so a key this build
// does not understand means a human believes a policy is in force that is not.
// `review_drives.json` is machine-authored state, and an older build must be
// able to read it and rewrite it without destroying what a newer one wrote.
//
// "Tolerated" is not enough on its own: serde ignores unknown fields by
// default, and an ignored field is *lost* on the next write. So every type here
// carries a flattened `extra` map, which makes the round trip **preserving**
// rather than merely non-fatal — the property
// `review_drives_round_trip_preserves_unknown_fields` pins.
//
// The one thing that is NOT tolerated is an unknown **state** string, and
// [`DriveState`] carries that argument.

/// Schema version of `review_drives.json`.
pub const REVIEW_DRIVES_VERSION: u32 = 1;
/// The file's name inside the group dir. It sits beside `state.json`,
/// `tasks.json` and `merge_queue.json`; the group dir itself is built by
/// `group_dir_at`, the only place a group id becomes a path.
pub const REVIEW_DRIVES_FILE: &str = "review_drives.json";

/// The whole of `review_drives.json` (§5.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewDrivesState {
    /// Schema version. **Required** — a state file with no version is
    /// malformed, not a v1 file, and §2.4's reconcile refuses such a file
    /// loudly rather than guessing at it.
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<DriveEntry>,
    /// Fields written by a newer build, preserved verbatim across a read/write
    /// cycle. See the section comment above this type.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for ReviewDrivesState {
    fn default() -> Self {
        ReviewDrivesState {
            version: REVIEW_DRIVES_VERSION,
            entries: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

impl ReviewDrivesState {
    /// Whether this build understands the file's schema.
    ///
    /// The conservative half of forward compatibility: unknown *fields* are
    /// preserved, but a file whose whole schema moved is one this build must
    /// **not act on** — the fields it recognizes may no longer mean what it
    /// thinks. Refuse to operate and leave the file alone, which is the only
    /// way "an older build does not destroy what a newer one wrote" survives a
    /// version bump that changes meanings rather than adding keys.
    pub fn version_supported(&self) -> bool {
        self.version == REVIEW_DRIVES_VERSION
    }

    /// The entry for a PR, if this file has one at all.
    pub fn entry(&self, pr: u64) -> Option<&DriveEntry> {
        self.entries.iter().find(|e| e.pr == pr)
    }

    /// The entry for a PR, mutably.
    pub fn entry_mut(&mut self, pr: u64) -> Option<&mut DriveEntry> {
        self.entries.iter_mut().find(|e| e.pr == pr)
    }

    /// Whether this PR is **live** in §5.1's `already-driven` sense — a working
    /// or `gate-check` entry. A parked entry is deliberately not live: §2.3
    /// calls resuming one the default.
    pub fn is_driven(&self, pr: u64) -> bool {
        self.entry(pr).is_some_and(|e| e.state().is_live())
    }
}

/// One reviewer lane of one drive (§5.2's `lanes`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LaneRecord {
    /// The reviewer **block** id the gate names (`rev-std`).
    pub block: BlockId,
    /// The session the driver spawned or resumed for this lane. The **full**
    /// resolved id, never a prefix, for §3.2's reason: a prefix that resolves
    /// uniquely today can become ambiguous tomorrow as the roster grows.
    #[serde(default)]
    pub session: String,
    /// The **agent id** — the pane — this lane's delegate is running in.
    /// Beyond §5.2's example, and it answers two questions the session id
    /// cannot.
    ///
    /// §2.2's `lane-stalled` row says the notice **names the pane**, and a pane
    /// is an agent id (`rev-4`), never a session UUID. And §7's interception is
    /// "keyed on the agent, never on text": an MCP caller arrives as a
    /// `caller.agent_id`, so without this field the driver cannot tell whether
    /// the delegate now calling `report` is one it spawned — and the only
    /// alternative key is something the delegate typed, which is precisely what
    /// §7 forbids.
    ///
    /// Empty when this build recorded the lane before the field existed, or
    /// when the spawn returned no id. Empty never matches a caller, so an
    /// unrecorded pane fails **closed**: its traffic is delivered to the
    /// orchestrator as it always was, rather than being consumed by a drive
    /// that cannot prove it owns the speaker.
    #[serde(default)]
    pub agent: String,
    /// Every EARLIER pane this drive opened for this lane, oldest first.
    ///
    /// **A re-brief supersedes a pane; it does not un-own it** (#1871 B2's
    /// sibling arc). [`open_lane`](DriveEntry::open_lane) replaces this lane's
    /// record wholesale, so before this field the previous pane's id was simply
    /// gone — and that id is §7's interception key. A reviewer still finishing
    /// its previous round then reported as if undriven, into the orchestrator's
    /// pane, which is the one thing the drive exists to absorb.
    ///
    /// Superseded is not dead: `rd_open_lane` resumes the lane's SESSION, and
    /// where it cannot type the brief into that session's own live idle pane
    /// (#1960) orrerix mints a new pane id for the resume while the old pane
    /// keeps running until something closes it. Both panes are this drive's, for
    /// as long as the drive is live. Reuse changed how OFTEN a lane pane is
    /// superseded; it changed nothing about what this field owes one that is.
    #[serde(default)]
    pub prior_agents: Vec<String>,
    /// The last verdict seen for this lane — a **record of what was read**,
    /// never a gate input. The live verdict file is re-read every tick, so
    /// nothing decides from this field; it is what `review_drive_status()`
    /// shows.
    ///
    /// Routed through [`Verdict::parse`] on the way in rather than kept as a
    /// raw string, the way `GroupId`'s `Deserialize` re-validates a persisted
    /// id. The alternative stores something that *looks* like a verdict and was
    /// never parsed, which is an invitation to a later `== "pass"` — and §4 is
    /// explicit that the driver is a reader of the gate, never a third
    /// implementation of it.
    #[serde(default, deserialize_with = "de_opt_verdict")]
    pub last_verdict: Option<Verdict>,
    /// The head that **verdict** bound to. Empty until this lane has answered
    /// at least once.
    #[serde(default)]
    pub at_head: String,
    /// The head this lane was last **briefed** at. Beyond §5.2's example — see
    /// the module header for why it has to exist.
    ///
    /// **Not the same question as [`at_head`](LaneRecord::at_head), and the two
    /// must never be conflated into one field.** `at_head` is the head the last
    /// verdict binds to; this is the head the lane was last asked about. A
    /// freshly spawned lane has this and not `at_head`, and that gap is exactly
    /// the call `review-wait` has to make on every tick: a lane already open at
    /// the live head is one to *wait* for, a lane whose brief predates the live
    /// head is one to *re-open*. One field answering both would make "has it
    /// been asked" and "has it answered" indistinguishable, so the driver would
    /// either re-brief a lane on every tick or wait forever on one it never
    /// briefed.
    #[serde(default)]
    pub briefed_head: String,
    /// The body digest this lane was last briefed at; empty when the body could
    /// not be read at brief time. Beyond §5.2's example, with `briefed_head` —
    /// and it travels with it, because **the two are one key**.
    ///
    /// That key is the same `(head, digest)` the gate binds a verdict to — arc
    /// 4 is "the last required lane passed at (head, digest)" — and a lane is
    /// open for exactly the revision it was asked about. The head **alone** is
    /// not that key, and the difference is not cosmetic: a lane that already
    /// answered `pass` at this head, whose body then moved, is indistinguishable
    /// under a head-only comparison from one still thinking about this head.
    /// §8's body-changed row wants the first re-briefed with a body-only delta
    /// and the second waited for, so a head-only key waits on a reviewer that
    /// has already spoken — forever, or until `lane-stalled` reports a stall
    /// that never happened. See [`lane_open_for`].
    #[serde(default)]
    pub briefed_digest: String,
    /// **The head this lane has been told to STOP reviewing** (#3176) — empty
    /// until the driver has sent that line, and per-revision like the two keys
    /// above.
    ///
    /// It exists to make the stop line arrive exactly ONCE. The rule that sends
    /// it is a standing property of the tick's facts (a conflicted PR with an
    /// open lane), so without a mark it would re-send on every tick the drive
    /// spends waiting out the same conflict — a reviewer's context re-filled
    /// with the same paragraph every thirty seconds, which is the cost §6 makes
    /// about notices applied to a delegate's own pane.
    ///
    /// **Persisted, and that is the point rather than an incidental**: a restart
    /// must not re-send a line the previous process already sent, and orrerix
    /// cannot read a pane's transcript to find out whether it did.
    ///
    /// **A mark, never a permission.** Nothing decides a release or an arc from
    /// this field: the release rule asks the pane whether it is idle, and a lane
    /// that was told to stop and has not yet reported is still that drive's to
    /// wait on. Cleared by [`LaneRecord::reseeded`] with the rest of the
    /// per-revision fields, so the lane is tellable again at the rebased head.
    #[serde(default)]
    pub stopped_head: String,
    /// When this lane's delegate was last spawned or resumed — the
    /// `lane-stalled` anchor. Beyond §5.2's example; see the module header.
    #[serde(default)]
    pub spawned_ms: u64,
    /// **This lane's current brief is a body-verification delta** (#2168 E2) —
    /// the head has not moved, every required lane has already passed it, and
    /// only the PR body has.
    ///
    /// Per-revision, like `briefed_head`/`briefed_digest`, and read only
    /// together with them: it is the *brief that is out*, never a standing
    /// property of the lane. `review_verdict` consults it to decide whether the
    /// verdict it is about to write carries
    /// [`ReviewVerdict::verified_body`](crate::workflow::ReviewVerdict::verified_body),
    /// which is what lets the gate accept the passes this one supersedes — so
    /// it is a capability grant, and it is checked against an exact
    /// `(briefed_head, briefed_digest)` match rather than through
    /// [`lane_open_for`]'s unknown-tolerant comparison. A brief whose revision
    /// cannot be pinned grants nothing.
    ///
    /// Cleared by [`LaneRecord::reseeded`] with the rest of the per-revision
    /// fields, per the rule stated there.
    #[serde(default)]
    pub briefed_verify: bool,
    /// **This lane's current brief is about the BODY of a revision whose CODE
    /// this drive has already reviewed to completion** (#2509) — the head has
    /// not moved, this lane was already briefed at it, and every required lane
    /// has ANSWERED at it.
    ///
    /// Strictly weaker than [`briefed_verify`](LaneRecord::briefed_verify),
    /// which it nests inside: that one needs every required lane to have
    /// *passed*, this one only that each has *spoken*. The gap between them is
    /// exactly the case #2509 is for — a lane that recorded `fail` on the body,
    /// whose worker then moved the body and not the head.
    ///
    /// Per-revision, like `briefed_head`/`briefed_digest`, and read only
    /// together with them: it describes the *brief that is out*, never a
    /// standing property of the lane. It is read at
    /// [`decide_review_wait`]'s `fail` arm against an exact
    /// `(briefed_head, briefed_digest)` match rather than through
    /// [`lane_open_for`]'s unknown-tolerant comparison — same posture as
    /// `briefed_verify`, and for the same reason: a brief whose revision
    /// cannot be pinned grants nothing.
    ///
    /// **It rides no verdict and reaches no gate.** #2509 considered putting
    /// the bit on [`ReviewVerdict`](crate::workflow::ReviewVerdict) and did
    /// not: line 5 of a verdict file is also read by the `gh` shim, and a mark
    /// the shim did not learn reads there as *no digest*, which makes
    /// `body-unchanged` refuse a merge it should allow. That is #2308's
    /// divergence exactly, and this grant needs none of it — the only consumer
    /// is the driver's own bound.
    ///
    /// Cleared by [`LaneRecord::reseeded`] with the rest of the per-revision
    /// fields, per the rule stated there.
    #[serde(default)]
    pub briefed_body_only: bool,
    /// Preserved unknown fields — see [`ReviewDrivesState`].
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl LaneRecord {
    /// This lane's **memory**, carried onto a fresh drive of the same PR and
    /// nothing else (#2153).
    ///
    /// **The gap is the cross-drive boundary, and it is the ordinary path.**
    /// Inside a live drive the resume is complete: `rd_lane_session` falls back
    /// from the record to the roster to the merged records, so even a killed
    /// lane pane resumes its conversation. But lane memory lives only on the
    /// entry, and `drive_review` on a PR whose previous entry is TERMINAL drops
    /// that entry and pushes a `DriveEntry::new` with `lanes: []` — so the
    /// sequence a satisfied gate is designed to produce (satisfied →
    /// orchestrator dispositions the findings → re-drive) spawned every lane
    /// cold. Measured on PR #2141: two lanes with live, resolvable sessions
    /// that had already read the PR once, both re-opened `resumed=false`, on
    /// the round where the warm session is cheapest.
    ///
    /// **What is kept is the conversation and what §7 owns; what is dropped is
    /// every claim about a revision THIS BUILD CAN NAME.** `briefed_head`,
    /// `briefed_digest`, `briefed_verify`, `briefed_body_only`, `last_verdict`,
    /// `at_head` and
    /// `spawned_ms` all describe the drive that ended, so carrying any of them
    /// would have the first tick either wait on a brief nobody sent, read a
    /// stale verdict as this round's answer, or — for `briefed_verify` and
    /// `briefed_body_only` — stamp a fresh round with a grant the previous
    /// drive made.
    ///
    /// **`extra` is the disclosed exception, and the qualifier above is what
    /// makes this doc honest** (#2169 review 2, premortem 2). That field is by
    /// construction the lane fields this build has no name for
    /// ([`ReviewDrivesState`]), and it is carried across rather than dropped
    /// because "a field a newer build wrote is not this build's to erase" is
    /// the rule the whole passthrough exists for — an older binary that
    /// silently deleted one would be the data-loss this type is designed to
    /// prevent. The residual is the mirror of that: if a FUTURE build stores a
    /// revision-scoped lane field in `extra`, an older binary performing the
    /// re-drive carries that build's claim about the round that just ended into
    /// the new one, and cannot know it did. No test can reach it — a fixture
    /// would have to invent a field the code does not know — so it is stated
    /// here and its trigger named: **any new per-revision lane field must be
    /// cleared in this function at the same time it is added.** Every field
    /// this build knows about is named above, so that check is a read of one
    /// function rather than a search.
    /// `lane_open_for` is then false for every head, which is what makes the
    /// first tick BRIEF each lane rather than wait on it.
    ///
    /// **That is a claim about the RECORD, not about what the brief says.** The
    /// verdict FILE outlives the drive that produced it, and the tick re-reads
    /// it through the gate's own parser as it does on every other tick — so
    /// [`DriveEntry::record_verdict_seen`] re-derives `last_verdict`/`at_head`
    /// from the file before the brief is built, and a lane that really did
    /// answer gets the §5.5 delta template naming the head it answered at. That
    /// is #2109's point rather than a leak past this one: a reviewer is asked
    /// again in its own conversation instead of being replaced by a stranger who
    /// is told what "its" previous verdict had been. What clearing the pair buys
    /// is that the new entry asserts nothing this drive has not itself read —
    /// so a first tick whose verdict file is absent or unreadable brief this
    /// lane as new rather than as having answered.
    ///
    /// **The pane moves to `prior_agents` rather than staying current.** It is
    /// still this drive's to intercept — a reviewer finishing its previous
    /// round must not report to the orchestrator as if undriven — but it is no
    /// longer the pane the drive would speak to, and [`DrivenPane::current`] is
    /// exactly that distinction. Leaving it as `agent` would also make
    /// `rd_live_lane_pane` treat it as a duplicate and `pane_dead` treat its
    /// death as this drive's, both of which are claims about a round that is
    /// over.
    ///
    /// `spawned_ms` goes to zero: it is the `lane-stalled` anchor, and a lane
    /// that has not been briefed has not been silent.
    ///
    /// **`session` is passed in rather than copied off this record**, and that
    /// is #2109's lesson applied one boundary over: `LaneRecord::session` is
    /// what the spawn RETURNED, which is a session id only on a CLI that
    /// pre-assigns one, so a copilot or opencode lane carries `""` for its whole
    /// life and its conversation lives on the pane and the roster row instead.
    /// A seed built from this field alone would therefore drop exactly the lanes
    /// #2109 was about. The caller resolves it through `rd_lane_session` — the
    /// one function that knows all three sources — and hands the answer here.
    pub fn reseeded(&self, session: &str) -> LaneRecord {
        let mut prior = self.prior_agents.clone();
        prior.push(self.agent.clone());
        LaneRecord {
            block: self.block.clone(),
            session: session.to_string(),
            agent: String::new(),
            // `""` as the "superseding" pane, which `retain_panes` treats as no
            // pane at all — it drops empties either way, so nothing is excluded
            // by it and the dedup and ordering still apply.
            prior_agents: retain_panes(prior, ""),
            last_verdict: None,
            at_head: String::new(),
            briefed_head: String::new(),
            briefed_digest: String::new(),
            stopped_head: String::new(),
            spawned_ms: 0,
            briefed_verify: false,
            briefed_body_only: false,
            // Preserved for `ReviewDrivesState`'s reason: a field a newer build
            // wrote is not this build's to drop. **This is the one thing here
            // that is NOT re-derived**, and the doc above names the residual it
            // leaves and the trigger that would close it.
            extra: self.extra.clone(),
        }
    }
}

/// Which side of a drive an agent is — the answer §7's interception asks of
/// every incoming `report` and `review_verdict`.
///
/// `Lane` carries the block id because the two consumers need it: the audit
/// line says which lane spoke, and `review-wait` reads that lane's verdict file
/// next tick. `Worker` needs no payload — a drive has exactly one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DrivenRole {
    /// A reviewer lane this drive spawned or resumed, by block id.
    Lane(BlockId),
    /// The worker this drive resumed for a hand-back.
    Worker,
}

/// One pane this drive owns, and **whether it is the pane the drive would speak
/// to now** — the two questions §7 has to answer separately (#1871 B2).
///
/// **Owning a pane and taking its word are different decisions, and collapsing
/// them is a live defect in either direction.** A pane the drive superseded is
/// still the drive's: its `report` is exactly the routing traffic §7 exists to
/// absorb, and leaving it to reach the orchestrator defeats the quiet-pane
/// property that is the whole measured benefit. But it is no longer the pane the
/// hand-back went to, and its report describes a revision the drive has moved
/// past — a `done` from a worker pane that was evicted two heads ago would
/// satisfy, through arc 8, work the CURRENT worker is still in the middle of.
///
/// So: consume it, audit it under its own kind so a reader can tell the two
/// apart, and give the state machine nothing. Only [`current`](DrivenPane::current)
/// traffic advances a drive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrivenPane {
    pub role: DrivenRole,
    /// `false` for a pane a later spawn or hand-back superseded.
    pub current: bool,
}

/// The superseded-pane list, normalized: no empties, no duplicates, never the
/// pane that is superseding them, oldest first.
///
/// **Deduplicated by agent id because a pane resumed twice is one pane.** The
/// list is read by [`DriveEntry::driven_role`] — where a duplicate changes
/// nothing — and printed in the exit notices, where naming `w-1715` twice reads
/// as two panes a human then goes looking for.
pub(super) fn retain_panes(prior: Vec<String>, current: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in prior {
        if a.is_empty() || a == current || out.iter().any(|s| *s == a) {
            continue;
        }
        out.push(a);
    }
    out
}

/// A persisted verdict word, re-validated on the way in.
///
/// An absent or `null` field is `None`; a word [`Verdict::parse`] does not know
/// is an **error**, not a `None`. Silently reading it as "no verdict yet" would
/// make the driver re-open a lane that had already answered.
fn de_opt_verdict<'de, D>(d: D) -> Result<Option<Verdict>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error as _;
    match Option::<String>::deserialize(d)? {
        None => Ok(None),
        Some(s) => Verdict::parse(&s)
            .map(Some)
            .ok_or_else(|| D::Error::custom(format!("unknown verdict {s:?}"))),
    }
}

/// How long a terminal entry is kept alive for the sake of a notice that has
/// not reached a pane — §5.2's retention **ceiling** (#1857).
///
/// **A retry with no ceiling is a leak, not a guarantee.** The orchestrator's
/// pane can be gone for good — the group closed, the agent killed and never
/// respawned — and an entry retained on "until it is delivered" alone would sit
/// in `review_drives.json` forever, re-attempted on every wake of the poll loop
/// that also delivers every `notify_when` watch in the fleet.
///
/// **One hour, which is this subsystem's own unit for exactly this judgment.**
/// `lane_timeout_minutes` and `fix_timeout_minutes` both default to 60 and both
/// answer the same question — long enough that a transient (a pane restarting,
/// a full queue, a paused agent) has cleared, short enough that a dead one is
/// not held indefinitely — and `notify_when`'s own TTL defaults to the same 60
/// minutes. It is deliberately **not** a `driver:` policy knob: §5.3's block
/// paces a drive, and how long orrerix keeps its own undelivered record is not
/// a repo's call.
///
/// **Reaching it drops the entry and audits the notice text**, which is what
/// makes the bound honest rather than merely bounded: the defect #1857 names is
/// "no line in the pane AND no record that could produce one", and the second
/// half stays closed by `rd-notice-dropped` carrying the text even in the case
/// where the first cannot be.
pub const NOTICE_RETENTION_MS: u64 = 60 * 60_000;

/// How long the live-delegate cap may refuse this drive's lane before the drive
/// parks as [`HeldReason::CapFull`] (#2109).
///
/// **A grace period, because a capped lane usually clears itself** — §8's
/// live-delegate-cap row, and the reason the refusal is a back-off rather than a
/// hold in the first place: another drive's lane finishes, the reaper takes an
/// idle pane, a human closes one, and the next tick spawns. Holding on the first
/// refusal would spend an orchestrator turn on a condition that resolves in one
/// `RD_BACKOFF_MS` interval, which is the opposite defect from the one #2109
/// reports.
///
/// **Fifteen minutes**, which is three back-off intervals: long enough that a
/// transient cap never reaches it, and short enough that the measured incident —
/// three hours in `review-wait` with `lanes: []`, invisible until a human read
/// `review_drive_status` by hand — is impossible. The other bound that would
/// eventually have caught it is `drive_timeout_minutes`, at its twelve-hour
/// default, whose notice does now name the state and the clock but still says
/// nothing about the cap.
///
/// **Not a `driver:` policy knob**, on `NOTICE_RETENTION_MS`'s argument: §5.3's
/// block paces a drive against INVARIANT 9's budget, and how long orrerix waits
/// on its own group's slot pressure before saying so is not a repo's call. A
/// repo that wants a longer wait raises its own `max_agents`.
pub const CAP_HOLD_MS: u64 = 15 * 60_000;

/// Why `review_drives.json` could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateError {
    /// The file is there and does not parse — torn, hand-edited, or naming a
    /// state this build does not know. §2.4: the tick refuses, audits
    /// `rd-state-unreadable` and backs off; it never repairs and never deletes.
    Malformed(String),
    /// A schema this build does not understand. Do not operate; do not write.
    Unsupported(u32),
    /// The file is there and could not be read at all.
    Io(String),
}

/// The group's review-drive file.
pub fn state_path(group_dir: &Path) -> PathBuf {
    group_dir.join(REVIEW_DRIVES_FILE)
}

/// Read the group's drive state.
///
/// **An absent file is no drives, not an error** — that is the product default
/// (§5.3: no `driver:` block, nothing ever driven). Every other failure is a
/// [`StateError`], because the difference between "nothing is driven" and
/// "orrerix cannot tell what is driven" is exactly what §5.1 gives the tools
/// their own `rd-state-unreadable` for: answering `not-driven` over a torn file
/// asserts something orrerix cannot know, while a drive may well be live.
pub fn load_state(group_dir: &Path) -> Result<ReviewDrivesState, StateError> {
    let text = match std::fs::read_to_string(state_path(group_dir)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ReviewDrivesState::default())
        }
        Err(e) => return Err(StateError::Io(e.to_string())),
    };
    parse_state(&text)
}

/// The parse half of [`load_state`], without the file — so the refusals can be
/// pinned on a string.
pub fn parse_state(text: &str) -> Result<ReviewDrivesState, StateError> {
    let state: ReviewDrivesState =
        serde_json::from_str(text).map_err(|e| StateError::Malformed(e.to_string()))?;
    if !state.version_supported() {
        return Err(StateError::Unsupported(state.version));
    }
    Ok(state)
}

/// Write the drive state atomically, reusing [`crate::fsatomic::atomic_write`]
/// — the #133-hardened writer (same-directory temp, `sync_all` before the
/// rename, a fallback that keeps the temp on failure).
///
/// Deliberately not a fresh `fs::write`: a disk-full `fs::write` is what
/// truncated `tasks.json` and destroyed a live board in #133, and this file has
/// the same "losing it loses in-flight work" property — worse, since §2.3's
/// counters are the only record of how much of INVARIANT 9's budget a drive has
/// spent. One hardened writer, not two.
pub fn store_state(group_dir: &Path, state: &ReviewDrivesState) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    crate::fsatomic::atomic_write(&state_path(group_dir), &bytes).map_err(|e| e.to_string())
}

/// §5.2's retention: **drop terminal entries whose notice has been delivered,
/// keep parked ones.** Returns what was dropped, so the caller can audit
/// `rd-pruned` per entry — and `rd-notice-dropped` for the one case that leaves
/// with a notice still owing.
///
/// Two reasons that are not merely hygiene. Unpruned terminal entries would
/// flow through `review_drive_status()` into the orchestrator's resident
/// context, which is the cost this whole feature exists to remove; and they
/// would make §5.1's `already-driven` refuse every re-drive of a PR forever.
///
/// **`held` entries are never pruned**, and that is the whole reason §2.1 makes
/// `held` parked rather than terminal: §2.3's resume needs their counters, and
/// pruning one would silently grant three fresh review rounds. A parked drive
/// leaves this file by being resumed to completion or cancelled, never by
/// retention.
///
/// **§5.2's ordering rule is enforced HERE rather than asked of the caller**
/// (#1857). This function used to drop every terminal entry and say in its own
/// doc that the caller owned "prune once the notice has been delivered" — which
/// no caller implemented, so a drive whose final notice failed to deliver was
/// pruned anyway and ended with no line in the pane and no record that could
/// produce one.
///
/// It is enforced rather than delegated because a rule stated on one side of a
/// call and satisfied on neither is what #1857 actually was: this function reads
/// the entry, so it is the thing that can read [`DriveEntry::owed_notice`], and
/// there is no reason for a second party to hold half the condition. What the
/// caller still owns is delivery itself — it must clear the notice
/// ([`DriveEntry::notice_delivered`]) on an attempt that succeeded, and that
/// obligation *is* enforceable from here, because an entry it forgets simply
/// stays.
///
/// **The ceiling is the third condition, and it is why this takes a clock.** An
/// entry whose notice can never be delivered would otherwise be retained
/// forever; at `retention_ms` past [`OwedNotice::owed_ms`] it is dropped anyway,
/// with its text handed back in [`Pruned::undelivered`] so the caller can put it
/// on the audit log. See [`NOTICE_RETENTION_MS`] for the value and the argument.
///
/// A clock that went backwards yields a saturated zero — "not yet" — so the
/// failure direction is keeping the record, never dropping it early.
///
/// Two reasons that are not merely hygiene. Unpruned terminal entries would
/// flow through `review_drive_status()` into the orchestrator's resident
/// context, which is the cost this whole feature exists to remove; and they
/// would make §5.1's `already-driven` refuse every re-drive of a PR forever.
///
/// **Neither reason is weakened by holding one back for a notice.** Both those
/// surfaces already filter on `is_terminal()` — `review_drive_status` lists only
/// live drives and `is_driven` answers `is_live()` — so a retained terminal
/// entry reaches no orchestrator context and refuses no re-drive. What it does
/// do is sit in the file, which is what the ceiling bounds.
///
/// **`held` entries are never pruned**, and that is the whole reason §2.1 makes
/// `held` parked rather than terminal: §2.3's resume needs their counters, and
/// pruning one would silently grant three fresh review rounds. A parked drive
/// leaves this file by being resumed to completion or cancelled, never by
/// retention.
pub fn prune_terminal(
    state: &mut ReviewDrivesState,
    now_ms: u64,
    retention_ms: u64,
) -> Vec<Pruned> {
    // One predicate, read twice — by the collect and by the retain — so the two
    // can never answer differently. A `retain` whose condition is the hand-written
    // negation of the collect's is where a third condition gets added to one and
    // not the other.
    let droppable = |e: &DriveEntry| -> Option<Option<String>> {
        if !e.state().is_terminal() {
            return None;
        }
        match e.owed_notice() {
            // Delivered (or never owed one at all — an entry that went terminal
            // before this field existed). §5.2's ordering rule, satisfied.
            None => Some(None),
            // Past the ceiling. Dropped, and its text goes to the audit log.
            Some(n) if now_ms.saturating_sub(n.owed_ms) >= retention_ms => {
                Some(Some(n.text.clone()))
            }
            // Owing, inside the ceiling: kept, so a later tick re-attempts it.
            Some(_) => None,
        }
    };
    let pruned: Vec<Pruned> = state
        .entries
        .iter()
        .filter_map(|e| droppable(e).map(|undelivered| Pruned { pr: e.pr, undelivered }))
        .collect();
    state.entries.retain(|e| droppable(e).is_none());
    pruned
}

/// One entry §5.2's retention dropped, and whether its notice ever reached a
/// pane (#1857).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pruned {
    pub pr: u64,
    /// `Some(text)` when the entry left at the retention **ceiling** with its
    /// notice still owing. The caller audits that text, so the record that could
    /// still produce the line outlives the entry that owed it — which is the
    /// half of #1857 a bound would otherwise reintroduce.
    ///
    /// `None` is the ordinary exit: the notice was delivered (or the entry never
    /// owed one), and nothing is lost.
    pub undelivered: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── §5.2 the state file ─────────────────────────────────────────────────

    /// §5.2's own example entry, verbatim in shape — the fields it shows and no
    /// others. It parses, which is what makes the note's documented shape a
    /// contract rather than an illustration, and the three fields this module
    /// adds are absent from it on purpose: each is `serde(default)`, so a file
    /// written against the published shape still reads.
    const NOTE_EXAMPLE: &str = r#"{
      "version": 1,
      "entries": [
        { "pr": 1758,
          "state": "review-wait",
          "held_reason": null,
          "head": "abc123",
          "body_digest": "3f1a",
          "worker_session": "cafb930d-0000-0000-0000-000000000000",
          "on_behalf_of": "orch-1",
          "lanes": [ { "block": "rev-std", "session": "1111",
                       "last_verdict": "pass", "at_head": "abc123" } ],
          "lane_index": 0,
          "counters": { "review_rounds": 1, "ci_attempts": 0, "rebase_attempts": 0 },
          "started_ms": 0 }
      ]
    }"#;

    /// **The kick-back budget survives a backwards wall-clock step** (rev-std
    /// round 2, premortem 1).
    ///
    /// `kickback_owed` compares two wall-clock stamps, so an unclamped
    /// `record_kickback` fed a `now` from before the hand-back writes a stamp
    /// that never overtakes `fix_handback_ms` — the budget never spends, and the
    /// tick re-emits on every wake for as long as the progress signal stands.
    /// The forward case is the control: without it this would pass against a
    /// `record_kickback` that ignored its argument entirely.
    #[test]
    fn a_kick_back_stamped_before_its_own_hand_back_still_spends_the_budget() {
        let mut e = entry_at(DriveState::FixWait);
        e.fix_handback_ms = 10_000;
        assert!(e.kickback_owed(), "the pre-state: a fresh hand-back owes one");

        // A clock that stepped backwards between the hand-back and the report.
        e.record_kickback(9_000);
        assert!(
            !e.kickback_owed(),
            "a backwards clock step must not re-arm the budget — that is one prompt per \
             tick into the worker's pane for as long as it keeps reporting progress"
        );

        // The control: an ordinary forward stamp spends it too, and the next
        // hand-back renews it with nothing having to reset anything.
        let mut f = entry_at(DriveState::FixWait);
        f.fix_handback_ms = 10_000;
        f.record_kickback(11_000);
        assert!(!f.kickback_owed(), "the ordinary case still spends");
        f.fix_handback_ms = 20_000;
        assert!(f.kickback_owed(), "…and the next hand-back renews it");
    }

    #[test]
    fn the_notes_own_example_entry_parses() {
        let s = parse_state(NOTE_EXAMPLE).unwrap();
        assert_eq!(s.version, REVIEW_DRIVES_VERSION);
        let e = s.entry(1758).unwrap();
        assert_eq!(e.state(), DriveState::ReviewWait);
        assert_eq!(e.held_reason, None);
        assert_eq!(e.counters.review_rounds, 1);
        assert_eq!(e.lanes[0].block, "rev-std");
        assert_eq!(e.lanes[0].last_verdict, Some(Verdict::Pass));
        // The added fields default rather than refusing the published shape.
        assert_eq!(e.fix_handback_ms, 0);
        // #1959's, and its default is load-bearing rather than incidental: an
        // entry that has never handed back must owe no kick-back, which is what
        // `0 < 0` being false says.
        assert_eq!(e.fix_kickback_ms, 0);
        assert!(!e.kickback_owed(), "a `review-wait` entry owes nobody a kick-back");
        assert_eq!(e.lanes[0].spawned_ms, 0);
        assert_eq!(e.lanes[0].briefed_head, "");
        assert_eq!(e.lanes[0].briefed_digest, "");
        // #2168 E1's, and its default is load-bearing in the same way: an entry
        // written before the field existed must read as a head this drive did
        // NOT hand back for.
        assert!(!e.fix_pushed());
    }

    /// **An entry written before `fix_pushed_ms` existed degrades toward the
    /// pre-#2168 behaviour, never toward a false park** (#2168 E1).
    ///
    /// `serde(default)` gives `false`, and which way that falls is the whole
    /// question: `false` means such a drive advances on green alone and pays at
    /// most the one re-record round it was already going to pay, while `true`
    /// would hold it in `ci-wait` waiting for a `report(done)` its worker was
    /// never asked for and park it `fix-stalled` an hour later. The default is
    /// asserted through `decide` rather than off the field, because the field's
    /// value is not the promise — what the drive DOES with it is.
    #[test]
    fn an_entry_from_before_the_field_advances_on_green_as_it_used_to() {
        let limits = DriveLimits::default();
        let old = parse_state(NOTE_EXAMPLE).unwrap();
        let mut e = old.entry(1758).unwrap().clone();
        // Walk it to `ci-wait` the way arc 6 does, so the state is one the
        // machine really reaches and `fix_pushed_ms` is whatever the arc leaves.
        e.advance(DriveState::CiWait, None, None, 1_000).unwrap();
        assert!(!e.fix_pushed(), "the pre-state: nothing in the older file said otherwise");
        assert_eq!(
            decide(
                &e,
                &DriveFacts {
                    ci: CiObservation::Green,
                    worker: WorkerSignal::Silent,
                    ..facts_at("head-a")
                },
                &limits,
            ),
            DriveStep::to(DriveState::ReviewWait),
            "an upgrade mid-drive must not strand the drive on a signal nobody asked \
             its worker for"
        );
    }

    #[test]
    fn review_drives_round_trip_preserves_unknown_fields() {
        // §5.2: unknown fields are tolerated AND preserved. "Tolerated" alone
        // is what serde does by default, and a field ignored on read is lost on
        // the next write — which breaks the actual promise, that an older build
        // can read this file and rewrite it without destroying what a newer one
        // wrote. Every level carries an `extra`, and this walks all four.
        let text = r#"{
          "version": 1,
          "future_top": {"k": 1},
          "entries": [
            { "pr": 7,
              "state": "ci-wait",
              "head": "h",
              "counters": {"review_rounds": 1, "ci_attempts": 0, "rebase_attempts": 0,
                           "future_counter": 42},
              "lanes": [{"block": "rev-std", "future_lane": ["x"]}],
              "future_entry": "keep me" }
          ]
        }"#;
        let s = parse_state(text).expect("a newer file must still read");
        let back: Value = serde_json::to_value(&s).unwrap();
        assert_eq!(back["future_top"], serde_json::json!({"k": 1}));
        assert_eq!(back["entries"][0]["future_entry"], "keep me");
        assert_eq!(back["entries"][0]["counters"]["future_counter"], 42);
        assert_eq!(back["entries"][0]["lanes"][0]["future_lane"], serde_json::json!(["x"]));
        // ...and the known fields survived the trip too, so the assertions
        // above are not passing over a document that lost everything else.
        assert_eq!(back["entries"][0]["pr"], 7);
        assert_eq!(back["entries"][0]["state"], "ci-wait");
        assert_eq!(back["entries"][0]["counters"]["review_rounds"], 1);
    }

    #[test]
    fn an_unknown_state_string_refuses_the_whole_file() {
        // The asymmetry §5.2 argues for, and the half that is easy to get
        // backwards: an unknown FIELD is carried, an unknown STATE refuses.
        let bad = NOTE_EXAMPLE.replace(r#""state": "review-wait""#, r#""state": "reviewing""#);
        assert_ne!(bad, NOTE_EXAMPLE, "the mutation must actually land");
        match parse_state(&bad) {
            Err(StateError::Malformed(_)) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_held_reason_and_an_unknown_verdict_refuse_the_file_too() {
        // Both are closed vocabularies for the state's reason: a hold whose
        // reason this build cannot read is a hold it cannot explain, and a
        // verdict word that never went through `Verdict::parse` is one a later
        // `== "pass"` could act on.
        let bad_reason = NOTE_EXAMPLE
            .replace(r#""state": "review-wait""#, r#""state": "held""#)
            .replace(r#""held_reason": null"#, r#""held_reason": "vibes""#);
        assert!(bad_reason.contains("vibes"), "the mutation must actually land");
        assert!(matches!(
            parse_state(&bad_reason),
            Err(StateError::Malformed(_))
        ));

        let bad_verdict = NOTE_EXAMPLE.replace(r#""last_verdict": "pass""#, r#""last_verdict": "PASS""#);
        assert!(bad_verdict.contains("PASS"), "the mutation must actually land");
        assert!(matches!(
            parse_state(&bad_verdict),
            Err(StateError::Malformed(_))
        ));

        // A `held` entry with a KNOWN reason still parses, so the two refusals
        // above are about the vocabulary and not about the shape.
        let good = NOTE_EXAMPLE
            .replace(r#""state": "review-wait""#, r#""state": "held""#)
            .replace(r#""held_reason": null"#, r#""held_reason": "review-limit""#);
        let s = parse_state(&good).unwrap();
        assert_eq!(s.entry(1758).unwrap().held_reason, Some(HeldReason::ReviewLimit));
    }

    #[test]
    fn a_versionless_or_counterless_file_is_malformed_and_a_future_one_is_unsupported() {
        // No version is malformed, not "a v1 file" — §5.2's `version` is
        // required for the same reason the queue's is.
        assert!(matches!(
            parse_state(r#"{"entries":[]}"#),
            Err(StateError::Malformed(_))
        ));
        // Missing counters is refused rather than defaulted to zeros: zeros
        // would silently grant a full fresh budget.
        let no_counters = NOTE_EXAMPLE.replace(
            r#""counters": { "review_rounds": 1, "ci_attempts": 0, "rebase_attempts": 0 },"#,
            "",
        );
        assert!(!no_counters.contains("counters"), "the mutation must actually land");
        assert!(matches!(
            parse_state(&no_counters),
            Err(StateError::Malformed(_))
        ));
        // A schema this build does not understand: do not operate, do not write.
        assert_eq!(
            parse_state(r#"{"version":2,"entries":[]}"#),
            Err(StateError::Unsupported(2))
        );
    }

    #[test]
    fn retention_prunes_the_terminals_and_never_the_parked_one() {
        // §5.2's asymmetry, and the whole reason §2.1 makes `held` parked:
        // pruning a parked entry would silently grant three fresh rounds.
        let mut s = ReviewDrivesState::default();
        for (pr, st) in [
            (1u64, DriveState::CiWait),
            (2, DriveState::Held),
            (3, DriveState::Satisfied),
            (4, DriveState::Cancelled),
            (5, DriveState::GateCheck),
        ] {
            let mut e = entry_at(st);
            e.pr = pr;
            s.entries.push(e);
        }
        let mut pruned: Vec<u64> =
            prune_terminal(&mut s, 1_000, NOTICE_RETENTION_MS).into_iter().map(|p| p.pr).collect();
        pruned.sort_unstable();
        assert_eq!(pruned, vec![3, 4]);
        let left: Vec<u64> = s.entries.iter().map(|e| e.pr).collect();
        assert_eq!(left, vec![1, 2, 5]);
        // The parked entry kept its counters, which is what the resume spends.
        assert!(s.entry(2).unwrap().state().is_parked());
    }

    /// **§5.2's ordering rule, which no caller implemented before #1857**: a
    /// terminal entry leaves the file once its notice has reached a pane, and
    /// not before.
    ///
    /// The three arms are asserted together because each alone passes under an
    /// implementation that is wrong in one of the other two directions —
    /// "prune everything" passes arm 1, "prune nothing terminal" passes arm 2,
    /// and "retain forever" passes arms 1 and 2 while leaking.
    #[test]
    fn a_terminal_entry_is_kept_while_its_notice_is_owed_and_dropped_at_the_ceiling() {
        let owing = |pr: u64, owed_ms: u64| {
            let mut e = entry_at(DriveState::Satisfied);
            e.pr = pr;
            e.owe_notice(&format!("[orrerix] review drive PR #{pr}: GATE SATISFIED"), owed_ms);
            e
        };
        let mut s = ReviewDrivesState::default();
        // 1: delivered — the ordinary exit.
        let mut delivered = owing(1, 0);
        delivered.notice_delivered();
        s.entries.push(delivered);
        // 2: owing, inside the ceiling — kept, so a later tick re-attempts it.
        s.entries.push(owing(2, 1_000));
        // 3: owing, past the ceiling — dropped, with its text handed back.
        s.entries.push(owing(3, 0));
        // 4: parked and owing nothing — never pruned, on either rule.
        let mut held = entry_at(DriveState::Held);
        held.pr = 4;
        s.entries.push(held);

        let now = NOTICE_RETENTION_MS + 500;
        let pruned = prune_terminal(&mut s, now, NOTICE_RETENTION_MS);
        assert_eq!(
            pruned.iter().map(|p| p.pr).collect::<Vec<_>>(),
            vec![1, 3],
            "a delivered notice prunes and an expired one prunes; an owed one inside the \
             ceiling must not: {pruned:?}"
        );
        assert_eq!(pruned[0].undelivered, None, "#1's notice reached a pane; nothing was lost");
        assert_eq!(
            pruned[1].undelivered.as_deref(),
            Some("[orrerix] review drive PR #3: GATE SATISFIED"),
            "an entry dropped AT the ceiling hands its text back, or the bound reintroduces \
             the very silence #1857 is about"
        );
        assert_eq!(s.entries.iter().map(|e| e.pr).collect::<Vec<_>>(), vec![2, 4]);
        // The retained one still owes exactly what it owed: retention did not
        // quietly re-arm its clock, which is what makes the ceiling reachable.
        assert_eq!(s.entry(2).unwrap().owed_notice().map(|n| n.owed_ms), Some(1_000));
    }

    /// The ceiling clock is anchored at the FIRST owing and a re-owe cannot move
    /// it — a clock re-armed by the retry it bounds is an unbounded retry.
    #[test]
    fn re_owing_a_notice_neither_replaces_the_text_nor_re_arms_the_ceiling() {
        let mut e = entry_at(DriveState::Cancelled);
        e.owe_notice("first", 1_000);
        e.notice_delivery_failed();
        e.owe_notice("second", 50_000);
        let owed = e.owed_notice().expect("still owing");
        assert_eq!(owed.text, "first", "the notice is the one the ending arc produced");
        assert_eq!(owed.owed_ms, 1_000, "the ceiling anchor is the FIRST owing");
        assert_eq!(owed.failures, 1, "and a failed attempt is counted, not the bound");
        // Delivery is the only thing that clears it, and then a fresh drive on
        // the same PR can owe again.
        e.notice_delivered();
        assert!(e.owed_notice().is_none());
        e.owe_notice("second", 50_000);
        assert_eq!(e.owed_notice().map(|n| n.owed_ms), Some(50_000));
    }

    /// A file written before the field existed parses, and its entries owe
    /// nothing — §5.2's read tolerance, checked on the one field #1857 adds.
    /// The round trip is the other half: an owed notice must survive a
    /// load/store cycle, or the whole mechanism is a per-process local again.
    #[test]
    fn an_owed_notice_round_trips_and_a_file_without_one_owes_nothing() {
        let mut e = entry_at(DriveState::Satisfied);
        let text = "[orrerix] review drive PR #1758: GATE SATISFIED at df6a73d0";
        e.owe_notice(text, 7_000);
        let s = ReviewDrivesState { entries: vec![e], ..ReviewDrivesState::default() };
        let json = serde_json::to_string(&s).unwrap();
        let back = parse_state(&json).expect("an owed notice must survive the file");
        let owed = back.entry(1758).unwrap().owed_notice().expect("owed after a round trip");
        assert_eq!((owed.text.as_str(), owed.owed_ms), (text, 7_000));

        // The absent case, spelled as a file this build did not write.
        let older = r#"{"version":1,"entries":[{"pr":9,"state":"satisfied",
            "counters":{"review_rounds":0,"ci_attempts":0,"rebase_attempts":0}}]}"#;
        let old = parse_state(older).expect("§5.2's read tolerance");
        assert!(
            old.entry(9).unwrap().owed_notice().is_none(),
            "an entry from before the field owes nothing — it must not be retained forever"
        );
    }

    #[test]
    fn already_driven_covers_the_working_and_gate_states_only() {
        // §5.1: a flat `already-driven` would make §2.3's resume unreachable
        // and `reset_counters` a parameter nothing can pass.
        for (st, driven) in [
            (DriveState::CiWait, true),
            (DriveState::ReviewWait, true),
            (DriveState::FixWait, true),
            (DriveState::GateCheck, true),
            (DriveState::Held, false),
            (DriveState::Satisfied, false),
            (DriveState::Cancelled, false),
        ] {
            let mut s = ReviewDrivesState::default();
            s.entries.push(entry_at(st));
            assert_eq!(s.is_driven(1758), driven, "{}", st.as_str());
        }
        // A PR with no entry at all is not driven either.
        assert!(!ReviewDrivesState::default().is_driven(1758));
    }
}
