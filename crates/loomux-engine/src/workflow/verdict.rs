//! Review verdicts: the state a merge gate reads (#222 / #197).

use super::*;

// ── verdicts: the state a gate reads (#222 / #197) ──────────────────────────
//
// Before this, a review outcome was a *notification*: `report("done", "approved
// — looks good")`, untyped text typed into the orchestrator's pane. That is
// exactly how PR #151 merged on the first "approve" that arrived while a second,
// dedicated review was still running — and that second review was the one that
// found a real release-gate bypass (#196). #197 asks for the outcome to be
// **state**: durable, attributed to the reviewer that recorded it, and readable
// by something that can refuse a merge.

/// A recorded review outcome. **Deliberately not a boolean.** Dify's Human Input
/// node and Windmill's `resume[...]` both give each decision its own outgoing
/// edge and keep the approver's typed input readable downstream; the investigation
/// (§2d) says to model ours the same way. So a reviewer can say "this needs a
/// human", which is neither an approval nor a defect report — and the gate can
/// treat it as the blocker it is instead of forcing it into a pass/fail bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Reviewed; no blocking findings. The only verdict that satisfies a gate.
    Pass,
    /// Reviewed; blocking findings. Refuses the merge.
    Fail,
    /// Not a defect call — the reviewer is handing the decision to a human
    /// (out of its depth, an ambiguous requirement, a risk it won't sign off on).
    /// Refuses the merge, exactly like `fail`: a gate must never be satisfiable
    /// by a reviewer that declined to decide.
    Escalate,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::Escalate => "escalate",
        }
    }

    /// Parse a verdict word. `None` for anything unrecognized — never coerced,
    /// and never defaulted to `pass`: a verdict loomux cannot read must not be
    /// able to open a gate.
    ///
    /// **Lowercase-strict, and that is a decision, not an oversight.** This is one
    /// half of a gate; the other half is the shim's `case "$v" in pass)`, which is
    /// a shell `case` and is case-sensitive. If this half lowercased, a
    /// hand-edited `PASS` in a verdict file would read as *satisfied* to the
    /// orchestrator (`list_verdicts`, `gate_status_line`) while the shim refused
    /// the merge — two halves of the same gate disagreeing about what a verdict
    /// *is*. One token definition, both sides, and the odd casing fails closed on
    /// both. Whitespace is trimmed because a trailing newline is file format, not
    /// content.
    pub fn parse(s: &str) -> Option<Verdict> {
        match s.trim() {
            "pass" => Some(Verdict::Pass),
            "fail" => Some(Verdict::Fail),
            "escalate" => Some(Verdict::Escalate),
            _ => None,
        }
    }

    /// Whether this verdict refuses a merge on its own. `fail` and `escalate`
    /// both do: **blockers beat approvals** (#197 Scope A.3) — with more than one
    /// reviewer, a disagreement resolves to "do not merge", and first-to-approve
    /// never wins.
    pub fn is_blocking(self) -> bool {
        !matches!(self, Verdict::Pass)
    }
}

/// The verdict words a reviewer may record, for error messages.
pub fn verdict_names() -> String {
    "pass, fail, escalate".to_string()
}

/// Longest verdict summary kept. The summary is durable state and is read back
/// into a gate refusal / the orchestrator's pane, not a transcript — a couple of
/// paragraphs is the useful range, and an unbounded one is a file-size footgun.
pub const MAX_SUMMARY_CHARS: usize = 4000;

/// A reviewer's summary is free prose that lands in a file loomux reads back and
/// re-renders. Drop control characters (they would ride into a terminal) but keep
/// newlines and tabs so the prose survives, and cap the length.
pub fn sanitize_summary(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .take(MAX_SUMMARY_CHARS)
        .collect()
}

/// One durable, **reviewer-attributed** verdict: which block recorded it, which
/// agent instance that was, **which revision it reviewed**, when, and why. The
/// attribution is the point — #197's second requirement is that "the specific
/// dispatched reviewer's recorded verdict is the gate, not the first approve that
/// arrives from any agent".
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewVerdict {
    pub pr: u64,
    /// The reviewer **block** id (`rev-security`) — the identity a gate names.
    pub block: BlockId,
    /// The agent instance that recorded it (`rev-4`). Two spawns of the same
    /// block are the same gate slot; this says which one actually spoke.
    pub agent_id: String,
    pub verdict: Verdict,
    /// **The PR head commit this verdict reviewed** (`headRefOid`), captured when
    /// it was recorded.
    ///
    /// A verdict binds to a *revision*, not to a PR number. Without this a `pass`
    /// survives a re-push: two reviewers approve #7, the worker pushes "fixed
    /// lint" and "one more edge case", and the gate still reads green over commits
    /// nobody reviewed — #197's failure class exactly, and the reason GitHub's own
    /// review model dismisses stale approvals on new commits. The gate compares
    /// this against the PR's current head and treats a mismatch as **outstanding**.
    ///
    /// Empty when loomux could not resolve the head at record time (no gh, no
    /// network, a repo gh can't see). That is *not* treated as "unbound, therefore
    /// fine" — an empty head can never equal a real one, so it reads as stale and
    /// the reviewer must re-record. Fail closed, like everything else here.
    pub head: String,
    /// **The PR body this verdict reviewed** (#565), as a sha256 of
    /// [`canonical_body`] — captured by the tool at record time, exactly like
    /// `head`, and never passed in by the reviewer.
    ///
    /// The head SHA pins the *code*. It does not pin the **PR body**, which on a
    /// squash-merging repo becomes the permanent commit message: reviewed content
    /// with the weight of a diff and none of a diff's version pinning. It moves in
    /// both directions — a reviewer passes a body and the author then edits it, so
    /// the merge carries text nobody reviewed; or a reviewer fails a body that has
    /// already been fixed, and the PR is blocked on a defect that no longer exists
    /// (the #525 incident that filed #565: review comment at 14:44:23Z, body edited
    /// at 14:47:49Z, and no mechanism could tell either agent).
    ///
    /// A digest rather than the body text: fixed size, and a mismatch is *exact*.
    /// Storing ~250 lines per verdict archives the artifact but still leaves a human
    /// to diff it by eye — which is the manual step that cost the round. A
    /// `updatedAt` timestamp was the other option and is worse than nothing: it
    /// moves for labels and assignees, so it cries wolf, and when it does fire it
    /// says *that* something changed, never *what*.
    ///
    /// Empty when loomux could not resolve the body at record time — read the same
    /// fail-closed way as an empty `head`: unknown, never "unbound, therefore fine".
    pub body_digest: String,
    /// **This review was a body-verification delta** (#2168 E2): the review
    /// driver briefed this lane because every required lane had already passed
    /// the code at this head and only the PR body had moved, so what it was
    /// asked for is the body as it stands rather than the diff.
    ///
    /// It is what lets [`crate::mergeq::recheck_gate`]'s `body-unchanged` clause
    /// accept the passes this one supersedes — see [`body_verified`] for the
    /// rule and the property it narrows the clause to.
    ///
    /// **Computed by the tool, never passed in, exactly like `body_digest`.**
    /// `review_verdict` sets it from the drive's own lane record — the brief it
    /// sent — so a reviewer cannot mark its own pass as a verification, and a
    /// verdict recorded outside a drive never carries it. That is what confines
    /// this clause's loosening to the one case the driver produces: an
    /// undriven repo, and a hand-recorded verdict, see the rule exactly as it
    /// was.
    ///
    /// It rides line 5 beside the digest rather than on a line of its own,
    /// because everything after line 5 is the summary and the summary is the
    /// one field a reviewer writes. A marker a reviewer could type would be a
    /// marker a reviewer could forge.
    pub verified_body: bool,
    /// **How many findings the reviewer left OPEN, as the reviewer declared it**
    /// (#3367 item 5) — `review_verdict`'s optional `open_findings` argument.
    ///
    /// The structured form of the count `reviewdrive::stated_findings` parses
    /// out of the summary: that parser stays, as the FALLBACK for a verdict that
    /// carries no declaration. `None` means the reviewer did not say, and it is
    /// never read as zero — the clean case (`reviewdrive::lanes_are_clean`)
    /// needs `Some(0)` on every lane, so a lane that omitted it is not clean.
    ///
    /// **Reviewer-declared, and that is not a hole in the gate.** Unlike
    /// `body_digest` and `verified_body`, which the tool computes because a
    /// reviewer could otherwise open `body-unchanged` for lanes that never read
    /// the body, this field decides nothing the gate reads: `evaluate_merge_gate`
    /// and `recheck_gate` never consult it. What it changes is whether the
    /// orchestrator is WOKEN to disposition findings, and a reviewer declaring
    /// `0` over a finding it would have written down has miscounted its own
    /// review — the same trust the gate already places in its `pass`.
    ///
    /// Serialized only when present, so a `list_verdicts` row for a verdict
    /// recorded without it is byte-for-byte what it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_findings: Option<u32>,
    pub summary: String,
    pub ts_ms: u64,
}

impl ReviewVerdict {
    /// Whether this verdict reviewed the PR's current head. A blocking verdict is
    /// *revision-independent* — a `fail` recorded against an older commit still
    /// refuses the merge until the reviewer re-records, because "this PR has a
    /// defect" does not stop being true when the author pushes more code.
    pub fn reviewed(&self, head: &str) -> bool {
        !self.head.is_empty() && self.head == head
    }

    /// Whether the PR body has changed since this verdict was recorded (#565).
    /// `None` when that cannot be *known* — either this verdict carries no digest
    /// (recorded by a build that predates #565, or with gh unable to read the body)
    /// or the current body could not be read now. Never `Some(false)` on a guess:
    /// "we could not check" and "it is unchanged" are different answers, and only
    /// one of them may quiet a warning.
    pub fn body_changed(&self, current_digest: Option<&str>) -> Option<bool> {
        let now = current_digest.filter(|d| !d.is_empty())?;
        (!self.body_digest.is_empty()).then(|| self.body_digest != now)
    }

    /// Whether this verdict is a **body-verification pass covering the body as
    /// it stands** (#2168 E2) — a `pass`, bound to this head, marked
    /// [`verified_body`](ReviewVerdict::verified_body) by the driver, and
    /// carrying the digest of the body now on the PR.
    ///
    /// All four, and each closes a different hole. `pass`: a `fail` that read
    /// the current body is the fix loop, not an approval of it. `reviewed`: a
    /// verification of a body sitting on a head nobody passed says nothing
    /// about what would merge. `verified_body`: an ordinary pass that happens
    /// to be the newest is not a review OF the delta, and accepting one would
    /// weaken the clause for every repo rather than for the driven case this
    /// is for. The digest equality: a verification of a body that has since
    /// moved again is spent.
    ///
    /// `None` for `now` — the body could not be read — is **not** a match, the
    /// same fail-closed direction [`body_changed`](ReviewVerdict::body_changed)
    /// takes: "we could not check" may never discharge a gate condition.
    pub fn verifies_body(&self, head: &str, now: Option<&str>) -> bool {
        let Some(now) = now.filter(|d| !d.is_empty()) else { return false };
        self.verdict == Verdict::Pass
            && self.verified_body
            && self.reviewed(head)
            && !self.body_digest.is_empty()
            && self.body_digest == now
    }

    /// Whether this **pass** still covers the body that would be committed —
    /// the question `body-unchanged` asks of one verdict, and the same question
    /// the review driver asks before it re-briefs a lane (#2168 E2).
    ///
    /// Directly, when its own digest is the current one. Or **by delegation**,
    /// when `verified` says some required reviewer recorded a body-verification
    /// pass at this head — see [`body_verified`]. A pass with **no** digest is
    /// covered by neither: unknown is never "unbound, therefore fine".
    ///
    /// **Both arms require the pass to be bound to the head that would merge**,
    /// and the delegation arm is the one where that is load-bearing: the
    /// verification lane read the BODY, and what makes standing in for this
    /// reviewer honest is that the CODE it approved has not moved since. Without
    /// it a `pass` from three commits ago would ride in on someone else's body
    /// review.
    ///
    /// It is asked here rather than left to the caller because the two callers
    /// ask it in different places and one of them did not (#2168 E2, first CI
    /// read). `mergeq::body_unchanged` filters `!reviewed(head)` before this
    /// line, so for the gate the clause is a no-op; `reviewdrive`'s
    /// `lane_pass_settles` had no such filter, and a `pass` bound to a head the
    /// worker had already fixed read as settling the revision in front of the
    /// drive — arc 8 skipping the very lane #1871 B1 exists to re-open. A
    /// predicate whose safety depends on where it is called is not a predicate.
    pub fn pass_covers_body(&self, head: &str, now: Option<&str>, verified: bool) -> bool {
        if self.verdict != Verdict::Pass || self.body_digest.is_empty() || !self.reviewed(head) {
            return false;
        }
        let Some(now) = now.filter(|d| !d.is_empty()) else { return false };
        self.body_digest == now || verified
    }
}

/// Whether any of `verdicts` is a body-verification pass at `(head, now)` —
/// [`ReviewVerdict::verifies_body`] asked of a whole reviewer set (#2168 E2).
///
/// **One definition, three consumers**, for §4's reason: the merge gate
/// ([`crate::mergeq::recheck_gate`]), the review driver's `review-wait`
/// (`reviewdrive::first_stale_lane`) and the `gh` shim's `body-unchanged` loop
/// all have to reach the same answer, or a drive reports `satisfied` on a merge
/// the shim then refuses. The shim is the one that cannot call this; it
/// reproduces it in POSIX shell, and
/// `the_shim_and_the_gate_agree_about_which_passes_a_verification_covers`
/// (`src-tauri/tests/orchestration/`) is what keeps the two honest: it walks
/// one set of verdict files past both halves and asserts they answer alike —
/// including the shapes two successive approximations of [`sanitize_digest`]
/// got wrong in OPPOSITE directions: prose that begins with a 64-hex word (the
/// shim accepted, Rust refuses) and an uppercase digest (the shim refused, Rust
/// accepts and lowercases). The shim now derives line 5 through one helper that
/// reproduces this function and [`parse_verdict_file`]'s split, rather than
/// testing the first whitespace field.
/// A glob was cited here before that test existed (#2308 review 4, R1), and a
/// citation to a name nothing answers to is worse than none — it reads as
/// coverage.
///
/// The caller passes the reviewers the **gate requires** — the routed list, not
/// every verdict on disk. A block the gate does not name has no standing to
/// discharge a condition of it.
pub fn body_verified<'a>(
    verdicts: impl IntoIterator<Item = &'a ReviewVerdict>,
    head: &str,
    now: Option<&str>,
) -> bool {
    verdicts.into_iter().any(|v| v.verifies_body(head, now))
}

/// The PR body reduced to the form both halves of the gate digest (#565).
///
/// Two normalizations, and **only** two, because the shim has to reproduce this
/// exactly in POSIX shell — a richer rule (per-line trailing whitespace, re-wrap
/// tolerance, markdown awareness) is one the two halves would eventually disagree
/// about, and a gate whose halves disagree is the failure mode this file keeps
/// coming back to:
///
/// 1. `\r` removed — a CRLF body and an LF body are the same commit message, and
///    which one `gh` hands back depends on the platform, not on the content.
///    Shell: `| tr -d '\r'`.
/// 2. Trailing newlines collapsed to exactly one. Shell: `$(…)` strips them all,
///    `printf '%s\n'` puts one back.
///
/// Everything else is content. In particular a re-wrapped paragraph **is** a
/// change: the body is about to become a permanent commit message, and the claim
/// this makes is "the bytes that will be recorded are the bytes that were
/// reviewed" — not "the meaning is close enough", which nothing could check.
pub fn canonical_body(body: &str) -> String {
    format!("{}\n", body.replace('\r', "").trim_end_matches('\n'))
}

/// sha256 of [`canonical_body`], lowercase hex. The one definition; the shim
/// pipes the same canonical bytes through `sha256sum`/`shasum`/`openssl`.
///
/// The two implementations are held together by an executed test, not by this
/// comment: case 1 of
/// `the_shim_refuses_a_merge_whose_body_moved_after_the_pass_when_the_repo_opts_in`
/// records a verdict through the real MCP tool and then merges through the real
/// shim over the SAME body — a body carrying CRLF, trailing blank lines, trailing
/// spaces, non-ASCII and `$`-bearing text — so the merge is allowed only if both
/// sides produced the same 64 characters. Disagreement surfaces as that case
/// failing with the gate's own refusal text.
pub fn body_digest(body: &str) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(canonical_body(body).as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// A stored body digest is compared inside a shell `case`, so keep it to what a
/// sha256 can actually be: **exactly** 64 hex characters. Anything else — a
/// truncated write, a hand edit, the first line of a summary in a verdict file
/// written before #565 — stores/reads as empty, i.e. *unknown*, which the gate
/// treats as it treats an unknown head: refuse, never wave through.
///
/// Deliberately stricter than [`sanitize_sha`], which accepts any hex run up to
/// 64: a 40-char head oid must not be readable as a body digest.
pub fn sanitize_digest(s: &str) -> String {
    let s = s.trim();
    if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        s.to_ascii_lowercase()
    } else {
        String::new()
    }
}

/// Group-dir subdirectory holding recorded verdicts, one file per reviewer block:
/// `verdicts/pr-<N>/<block-id>`.
///
/// **Why a file tree and not JSON:** the enforcement point is the `gh` PATH shim
/// — a POSIX shell script with no `jq` — and the existing gate state it reads
/// (`autonomous`, `auto_merge`, `merge_grants/pr-<N>`) is already exactly this:
/// small files whose presence and first line say everything. A verdict file's
/// first line is the verdict word, so the shim's read is `head -n1`. Keeping the
/// durable record and the enforcement input as *one* artifact means they cannot
/// drift.
pub const VERDICTS_DIR: &str = "verdicts";

/// A commit id is compared against gh's `headRefOid` inside a shell `case`, so
/// keep it to what a git object id can actually be. Anything else stores as empty,
/// which reads as **stale** — never as "unbound, therefore fine".
pub fn sanitize_sha(s: &str) -> String {
    let s = s.trim();
    if !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        s.to_ascii_lowercase()
    } else {
        String::new()
    }
}

/// The word line 5 carries **after** the digest when the verdict is a
/// body-verification pass (#2168 E2) — see [`ReviewVerdict::verified_body`].
///
/// **On line 5 rather than on a line of its own, and that placement is the
/// security property.** Line 6 onwards is the summary, which is the one field a
/// reviewer writes; a marker there would be a marker a reviewer could type. Line
/// 5 is written entirely by the tool whenever there is a digest at all, so a
/// reviewer cannot reach it.
pub const VERIFIED_BODY_MARK: &str = "verified-body";

/// The token line 4 carries **after** the agent id when the reviewer declared
/// [`ReviewVerdict::open_findings`] (#3367 item 5): `<agent-id> open-findings=<n>`.
///
/// **Line 4, and not line 5 or a line of its own.** Line 6 onward is the
/// summary, so a new line would shift it and misread every file written before;
/// line 5 is the one the `gh` shim reads (`loomux_verdict_line5`), and a second
/// token there would make the shim's digest split refuse the line. Line 4 is
/// read by nothing but [`parse_verdict_file`], and an agent id is a
/// `PathSegment` that can never contain a space, so the split is unambiguous.
/// An older build reading a newer file sees the token as part of the agent id,
/// which is display-only — it decides nothing.
pub const OPEN_FINDINGS_KEY: &str = "open-findings=";

/// Serialize a verdict record for `verdicts/pr-<N>/<block>`. Line-oriented, with
/// the verdict word FIRST (the shim reads it with `head -n1`), the reviewed head
/// SECOND and the reviewed body's digest FIFTH (`head -n5 | tail -n1`); the
/// summary runs to EOF, being the only field that may contain newlines — so every
/// fixed field has to sit above it.
///
/// Line 5 is the digest **alone** unless this verdict is a body verification, in
/// which case it is `<digest> verified-body` (#2168 E2). Both halves of the gate
/// therefore split a trailing [`VERIFIED_BODY_MARK`] off the line and run
/// [`sanitize_digest`] over **what remains**, which is what
/// [`parse_verdict_file`] does and what the shim's `loomux_verdict_line5`
/// reproduces. Reading the first whitespace FIELD instead is the approximation
/// this slice shipped twice and retracted twice: it accepts prose that begins
/// with a hex word, which is looser than the Rust half on the side that refuses
/// merges (#2308 rounds 4 and 5).
pub fn verdict_file_text(v: &ReviewVerdict) -> String {
    let digest = sanitize_digest(&v.body_digest);
    // The marker is meaningless without a digest to qualify it: what it says is
    // "the body THIS digest names was verified", and there is no such body when
    // the read failed. Dropped here rather than judged downstream, so the
    // parser's digest-first guard never has to decide about a bare marker.
    let mark = if v.verified_body && !digest.is_empty() {
        format!(" {VERIFIED_BODY_MARK}")
    } else {
        String::new()
    };
    let open = v.open_findings.map(|n| format!(" {OPEN_FINDINGS_KEY}{n}")).unwrap_or_default();
    format!(
        "{}\n{}\n{}\n{}{}\n{}{}\n{}\n",
        v.verdict.as_str(),
        sanitize_sha(&v.head),
        v.ts_ms,
        v.agent_id,
        open,
        digest,
        mark,
        sanitize_summary(&v.summary)
    )
}

/// Read a verdict file back. `None` for anything that isn't a verdict this build
/// understands — an unparseable file is *not* a pass (see [`Verdict::parse`]).
/// `pr`/`block` come from the path, which is loomux-generated.
///
/// Line 5 (the body digest, #565) is read **tolerantly**: a file written before
/// #565 has the first line of its summary there, and swallowing it would mangle
/// durable prose a human reads. So a line 5 that is not a valid digest is handed
/// back to the summary, and the digest reads empty — *unknown*. The shim
/// reproduces this function's own split and [`sanitize_digest`] rather than
/// approximating either (`loomux_verdict_line5`, #2308 round 5), so the two
/// agree on the only thing a gate decides: no readable digest means the body
/// cannot be shown unchanged, so `body-unchanged` refuses. The divergence that
/// remains is confined to which text is displayed as the summary — the shim has
/// no summary to display.
///
/// Line 5 may carry [`VERIFIED_BODY_MARK`] after the digest (#2168 E2), and the
/// split is **shape-checked, not whitespace-split**: the line is read as
/// `<digest> verified-body` only when the tail is exactly that word. A legacy
/// line 5 that happens to hold two words therefore parses exactly as it did
/// before — the whole line is offered to `sanitize_digest`, which refuses it,
/// and it stays in the summary. Splitting on the first space unconditionally
/// would have changed how a pre-#565 file reads, which is durable prose a human
/// wrote.
pub fn parse_verdict_file(pr: u64, block: &str, text: &str) -> Option<ReviewVerdict> {
    let mut lines = text.lines();
    let verdict = Verdict::parse(lines.next()?)?;
    let head = sanitize_sha(lines.next().unwrap_or(""));
    let ts_ms = lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
    // Line 4: the agent id, optionally followed by ` open-findings=<n>` (#3367
    // item 5). Shape-checked like line 5's mark: the tail is split off only when
    // it is exactly that key and a count that parses, so a legacy line 4 reads
    // exactly as it did — and a malformed count is NOT a declaration (never a
    // guessed zero), it stays on the id where a reader can see it.
    let line4 = lines.next().unwrap_or("").trim();
    let (agent_id, open_findings) = match line4.rsplit_once(' ') {
        Some((id, tail)) => match tail.strip_prefix(OPEN_FINDINGS_KEY).map(str::parse::<u32>) {
            Some(Ok(n)) => (id.trim(), Some(n)),
            _ => (line4, None),
        },
        None => (line4, None),
    };
    let agent_id = agent_id.to_string();
    let rest: Vec<&str> = lines.collect();
    let line5 = rest.first().copied().unwrap_or("");
    let (digest_field, marked) = match line5.trim_end().rsplit_once(' ') {
        Some((d, mark)) if mark == VERIFIED_BODY_MARK => (d, true),
        _ => (line5, false),
    };
    let body_digest = sanitize_digest(digest_field);
    // The marker only ever qualifies a digest this build could read. A line that
    // ends in the word but whose leading field is not a digest is not a
    // verification of anything — the same "unknown is never unbound, therefore
    // fine" the empty-digest case takes, one field over.
    let verified_body = marked && !body_digest.is_empty();
    let from = usize::from(!body_digest.is_empty());
    let summary = rest[from.min(rest.len())..].join("\n");
    Some(ReviewVerdict {
        pr,
        block: sanitize_id(block)?,
        agent_id,
        verdict,
        head,
        body_digest,
        verified_body,
        open_findings,
        summary: sanitize_summary(&summary),
        ts_ms,
    })
}
