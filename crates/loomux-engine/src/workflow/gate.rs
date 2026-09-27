//! The merge gate: its decision, the capacity advice, and the spec file the
//! gh shim reads.

use super::*;

// ── the merge gate: the decision, and the spec file the shim reads ──────────

/// Gate conditions this build knows how to check (`gates.merge.also`).
///
/// The list is short on purpose, and the rule for everything *not* on it is the
/// important half: a condition loomux cannot check **refuses the merge** rather
/// than passing it. A gate is a safety claim; silently ignoring a clause of it
/// would turn a stricter-looking workflow file into a weaker one, which is the
/// worst failure mode a gate can have.
/// `body-unchanged` (#565) is **opt-in for a reason**: it only matters where the
/// PR body *becomes* the record — this repo squash-merges, so the body is the
/// permanent commit message. On a repo that merge-commits, the body is discussion,
/// and the check would be noise. Baking it in either way would be baking one
/// repo's merge habit into a generic tool (CLAUDE.md constraint 8), so it is a
/// clause a repo writes down.
/// `base-green` (#1174) is the stop-the-line clause: it refuses a merge while
/// the **base ref's HEAD** is red or its checks cannot be resolved, so a fleet
/// cannot pile work onto a branch that is already broken. Opt-in for the same
/// reason `body-unchanged` is: a repo with no CI would otherwise be refused
/// every merge forever by a clause it never asked for.
pub const KNOWN_CONDITIONS: [&str; 3] = ["ci-green", "body-unchanged", "base-green"];

/// Whether the shim can evaluate this `also:` condition. See [`KNOWN_CONDITIONS`].
pub fn condition_supported(c: &str) -> bool {
    KNOWN_CONDITIONS.contains(&c.trim())
}

/// The `base-green` reductions (#1174) — **one definition, two consumers**: the
/// `gh` shim interpolates these constants into its POSIX body, and
/// `mqdriver::base_check_runs_argv`/`base_status_argv` pass them to `gh --jq`.
///
/// They live here, with the rest of the gate contract, precisely because the
/// first cut had a *copy* in each place. The two were byte-identical, which
/// looked like the two-implementations-one-contract property holding — and it
/// was, but what the contract SAID was wrong in both, and nothing could have
/// told them apart from two copies that had drifted. A shared constant makes
/// "the shim and the queue ask GitHub the same question" a fact about the
/// program rather than a claim in a PR body.
///
/// Each reduces a JSON payload to ONE word from a closed vocabulary —
/// `red` | `truncated` | `pending` | `none` | `green` — because the shim has no
/// JSON parser and must decide a merge from a shell `case`.
///
/// **The clause order is the contract, and each step earns its place:**
///
/// 1. **`red` first, and only for COMPLETED runs.** A visible failure is the
///    most actionable answer, so it outranks everything below — but a run still
///    in progress carries `conclusion: null`, which the conclusion allow-list
///    would otherwise call red. Reporting "the base is RED" about a base that
///    is merely still building would be a false sentence in a refusal, so
///    `.status == "completed"` guards it.
/// 2. **`truncated` next — the #1181 review's blocking finding.**
///    `/commits/{ref}/check-runs` is **paginated**: `check_runs` is capped at
///    `per_page` while `total_count` counts them all. `any(.check_runs[]; …)`
///    therefore asks "is anything on THIS PAGE red", and before this clause a
///    base with more runs than one page — an ordinary OS x version matrix,
///    exactly the repo that adopts a stop-the-line gate — reported **green**
///    with its failures sitting on page 2. Reproduced against this repo's own
///    API: a commit with 3 `failure` runs answered `red` at full page size and
///    `green` at `?per_page=3`. A page that does not carry every run says
///    nothing about the runs it omits, so it is not an answer.
/// 3. `pending`, then `none`, then `green` — the residue, unchanged.
///
/// **The shape the payload is ASSUMED to have is checked, not assumed (#1181
/// rev-lead NB5).** `total_count` is documented as always present, and the
/// truncation clause above rests entirely on it — but jq sorts `null` below
/// every number, so an absent key makes `.total_count > (.check_runs|length)`
/// evaluate `null > N`, which is **false**, and the expression falls straight
/// through to `green`. That is round one's defect wearing a different hat: an
/// unstated assumption about the payload, failing open, in the one clause whose
/// whole premise is that unknown is never green. `has("total_count")` answers
/// `truncated` instead, so a payload that cannot support the question refuses
/// rather than passing.
///
/// **[`BASE_STATUS_JQ`] carries the same guard over ITS inputs**, which the
/// review did not ask for and this repo's own rule requires: a guard reads every
/// one of its inputs by one rule, and taking one signal from a checked shape and
/// the next from an unchecked one is a bypass exactly the width of that
/// asymmetry. `null | length` is `0` in jq rather than an error, so an absent
/// `statuses` would read as the *definite* claim "this commit has no legacy
/// statuses"; an absent `state` would fall to the `else` and report `red`,
/// which refuses but says something false about the base while doing it.
///
/// **Only the check-runs half needs the truncation clause**, and the asymmetry
/// is worth stating rather than leaving to be re-derived: the combined-status
/// endpoint carries a top-level `.state` that is GitHub's own rollup across
/// *all* statuses, so [`BASE_STATUS_JQ`] is pagination-proof by construction.
/// `check-runs` has no rollup field — only `total_count` — which is why one
/// half was safe and the other was not.
///
/// Green is an ALLOW-list of conclusions (`success`, `neutral`, `skipped`), so
/// a conclusion GitHub adds tomorrow reads as red rather than as green.
pub const BASE_CHECK_RUNS_JQ: &str = "if any(.check_runs[]; .status == \"completed\" and .conclusion != \"success\" and .conclusion != \"neutral\" and .conclusion != \"skipped\") then \"red\" elif (has(\"total_count\")|not) then \"truncated\" elif (.total_count > (.check_runs|length)) then \"truncated\" elif any(.check_runs[]; .status != \"completed\") then \"pending\" elif (.check_runs|length) == 0 then \"none\" else \"green\" end";

/// The combined-status reduction — see [`BASE_CHECK_RUNS_JQ`] for the shared
/// contract and for why this one needs no truncation clause.
///
/// `.state` is `pending` both when a context is pending and when there are no
/// statuses at all, so the count is read first and answers `none`.
pub const BASE_STATUS_JQ: &str = "if (has(\"statuses\")|not) or (has(\"state\")|not) then \"truncated\" elif (.statuses|length) == 0 then \"none\" elif .state == \"success\" then \"green\" elif .state == \"pending\" then \"pending\" else \"red\" end";

/// Why a merge gate is (not) satisfied — the pure spec the shim's shell mirrors,
/// and what the `review_verdict` tool reports back to the reviewer that just voted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateOutcome {
    /// Every requirement met: the merge may proceed to the *other* gates (the
    /// human grant / autonomous markers) — this one never opens a merge by itself.
    Satisfied,
    /// At least one named reviewer recorded `fail`/`escalate`. Blockers beat
    /// approvals: this refuses the merge whatever the others recorded, and
    /// whatever the threshold is.
    Blocked { blocking: Vec<BlockId> },
    /// Not enough live PASS verdicts yet.
    ///
    /// - `outstanding` — named reviewers with **no verdict recorded at all**. The
    ///   #151 case: a merge landing while a dispatched review is still running.
    /// - `stale` — named reviewers whose `pass` was recorded against an **earlier
    ///   revision** of the PR (or against none at all). The branch moved under
    ///   them; what they approved is not what would merge.
    Short { passes: u32, need: u32, outstanding: Vec<BlockId>, stale: Vec<BlockId> },
    /// loomux could not resolve the PR's current head, so it cannot tell whether
    /// any recorded verdict reviewed the code that would merge. Refuses — the same
    /// fail-safe the human gate takes on an undeterminable base.
    UnknownRevision,
}

impl GateOutcome {
    pub fn satisfied(&self) -> bool {
        matches!(self, GateOutcome::Satisfied)
    }
}

/// How many PASS verdicts this gate needs: every named reviewer (`all-pass`) or
/// `threshold: N`.
pub fn gate_need(gate: &Gate) -> u32 {
    match gate.require {
        GateRequire::AllPass => gate.reviewers.len() as u32,
        GateRequire::Threshold(n) => n,
    }
}

/// Reviewer ids a gate names that the given roster cannot actually spawn — either
/// no block carries that id, or it exists under a different capability class
/// (`kind` != reviewer). A gate's reviewers are validated against a workflow
/// file's OWN blocks at parse time ([`parse_workflow`]), but the roster a live
/// group spawns from can diverge from the file that armed its gate: a broken or
/// absent `.loomux/workflow.yml` on a fresh launch keeps the group's last-known
/// gate but resets `blocks` to [`default_roster`] (see `create_group`'s
/// `merge-gate-retained` branch, and the live incident behind #316 — a gate
/// naming `rev-orch`/`rev-ui`/`rev-tests` with the running registry offering only
/// the built-in four, so `spawn_agent(block: "rev-orch")` failed with "unknown
/// block" and the gate could never be satisfied from inside that session). Pure,
/// so both the arm-time refusal and a live status read share one rule.
/// **Routed reviewers count too** (#1176). A rule naming a block this roster
/// cannot spawn makes the gate unsatisfiable for every PR whose paths match it —
/// which is the same #316 failure the static list is checked for, arriving on a
/// subset of PRs instead of all of them. Reported here rather than left to be
/// discovered as "the merge gate stopped opening on frontend PRs only".
pub fn gate_missing_blocks(gate: &Gate, blocks: &[Block]) -> Vec<BlockId> {
    let mut out: Vec<BlockId> = Vec::new();
    let named = gate.reviewers.iter().chain(gate.routing.iter().flat_map(|r| r.reviewers.iter()));
    for id in named {
        if !blocks.iter().any(|b| &b.id == id && b.kind == Role::Reviewer) && !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

/// The agent-capacity a declared workflow structurally needs (#255) — derived
/// from its roster and its `merge` gate (if any), so the launcher can warn
/// before a `max_agents` cap starves the workflow it just loaded rather than
/// discovering it two hours in as an orchestrator that keeps killing live
/// agents to make room (the #255 incident: a 3-reviewer `all-pass` gate plus a
/// two-tier worker roster under a cap of 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapacityRecommendation {
    /// What **one review round costs without evicting anything already
    /// live**: [`reviewers_needed`](Self::reviewers_needed) plus one worker
    /// slot to have something to review. Below this the orchestrator cannot
    /// complete a single rework loop without killing a live agent to free a
    /// slot.
    pub minimum: u32,
    /// What running **every declared tier concurrently** costs: every
    /// distinct worker block, every distinct reviewer block, and one more if
    /// the workflow declares a planner block. The orchestrator itself is exempt
    /// from `max_agents` and is never counted here — and since #1161 M3 (D3)
    /// **neither is a declared manager**: `live_delegate_count` skips both
    /// classes, so counting either here would advise a human to raise a cap
    /// against a pane that never consumes one.
    ///
    /// A workflow with two planner blocks still adds only one slot here — a
    /// repo declares a *second* planner to give it an alternate persona (a
    /// different model, a narrower prompt), not to run two plan-first phases
    /// at once; the orchestrator only ever has one active planning phase, so
    /// unlike workers/reviewers (genuinely fanned out for parallel lanes) a
    /// planner count would overstate what concurrency the roster needs. This
    /// also matches #255's literal spec: "+1 if a planner block exists".
    pub recommended: u32,
    /// The gate's reviewer requirement folded into `minimum` — [`gate_need`],
    /// or every declared reviewer block when the workflow names no `merge`
    /// gate. Kept as its own field (rather than making a caller subtract the
    /// worker slot back out of `minimum`, or recount reviewer *blocks*) so
    /// anything describing *why* `minimum` is what it is reads this instead of
    /// re-deriving a gate-derived number from the block list — conflating the
    /// two was exactly the bug rev-1 of #255's review caught in `roster.ts`'s
    /// warning text.
    pub reviewers_needed: u32,
}

/// Derive a [`CapacityRecommendation`] from a workflow's blocks and its
/// `gates.merge` clause (`None` when the workflow declares none).
///
/// Gate-aware, per #255's requirement: a roster with 5 reviewer blocks but
/// `require: threshold: 2` has a different (lower) minimum than one requiring
/// `all-pass` over the same 5 — [`gate_need`] is exactly that distinction.
///
/// With no gate declared, nothing *enforces* every reviewer block being live
/// at once — but nothing else tells loomux which subset would be, either, so
/// `minimum` conservatively falls back to every reviewer block the workflow
/// names. That is deliberately the erring-flag-not-erring-silent side: this
/// feature exists because a starved roster surfaced as nothing more than "a
/// slow run" (#255's incident), so a gateless roster warning at a cap that
/// merely *might* be enough is the safer of the two wrong answers.
pub fn recommend_capacity(blocks: &[Block], gate: Option<&Gate>) -> CapacityRecommendation {
    let workers = blocks.iter().filter(|b| b.kind == Role::Worker).count() as u32;
    let reviewers = blocks.iter().filter(|b| b.kind == Role::Reviewer).count() as u32;
    let has_planner = blocks.iter().any(|b| b.kind == Role::Planner);
    // #1161 M3 (D3): a declared manager is NOT counted, and the absence is the
    // decision rather than an omission. M1 landed a `+1` here — correct while
    // `live_delegate_count` exempted only the orchestrator, since a preview
    // that under-advises is how #255 happens — and M3 inverted it in the same
    // commit that gave `live_delegate_count` its `Role::Manager` exemption. The
    // two move together by construction: `recommended` is "what the cap must be
    // for every declared tier to be live at once", and a class the cap does not
    // apply to is live at any cap. Counting it would tell a human to raise a
    // number that was never going to stop them talking to their manager.

    // #1176. A gate that routes by path needs, in the WORST case, its declared
    // list plus every lane any rule can add — a PR that touches all of them. The
    // worst case is the one a capacity floor has to be built on: under-advising
    // here is how #255 happens, an orchestrator discovering two hours in that it
    // must kill a live agent to complete one review round. Deduped against the
    // declared list, and against itself, so a lane two rules both name counts once.
    let reviewers_needed = gate.map_or(reviewers, |g| {
        let mut extra: Vec<&BlockId> = Vec::new();
        for id in g.routing.iter().flat_map(|r| r.reviewers.iter()) {
            if !g.reviewers.contains(id) && !extra.contains(&id) {
                extra.push(id);
            }
        }
        gate_need(g) + extra.len() as u32
    });
    let worker_slot = u32::from(workers > 0);
    CapacityRecommendation {
        // `minimum` is deliberately untouched: it is what ONE REVIEW ROUND
        // costs, and a review round does not involve the manager.
        minimum: reviewers_needed + worker_slot,
        recommended: workers + reviewers + u32::from(has_planner),
        reviewers_needed,
    }
}

/// Which declared tiers `recommended` adds beyond `minimum` — i.e. what a cap
/// sitting at-or-above `minimum` but below `recommended` can never keep live
/// alongside a review round (#255's soft-warning tier). Each entry is a short
/// noun phrase (`"the planner"`, `"1 more worker tier"`) meant to be joined
/// into a sentence, not a standalone description.
///
/// Takes the same `reviewers_needed` [`recommend_capacity`] computed, rather
/// than re-deriving it from `gate`, so this can never disagree with the
/// `minimum` it is describing the excess over.
pub fn extra_tiers(blocks: &[Block], reviewers_needed: u32) -> Vec<String> {
    let workers = blocks.iter().filter(|b| b.kind == Role::Worker).count() as u32;
    let reviewers = blocks.iter().filter(|b| b.kind == Role::Reviewer).count() as u32;
    let has_planner = blocks.iter().any(|b| b.kind == Role::Planner);

    let mut out = Vec::new();
    // `minimum` budgets exactly one worker slot regardless of how many worker
    // blocks are declared — every worker tier beyond the first is "extra".
    let extra_workers = workers.saturating_sub(1);
    if extra_workers > 0 {
        out.push(format!("{extra_workers} more worker tier{}", if extra_workers > 1 { "s" } else { "" }));
    }
    // `minimum` only budgets the gate's requirement — every reviewer block
    // beyond that (an all-pass gate naming a subset, or extra unnamed ones)
    // is "extra".
    let extra_reviewers = reviewers.saturating_sub(reviewers_needed);
    if extra_reviewers > 0 {
        out.push(format!("{extra_reviewers} more reviewer{}", if extra_reviewers > 1 { "s" } else { "" }));
    }
    if has_planner {
        out.push("the planner".to_string());
    }
    // #1161 M3: no manager row, deliberately. This list answers "what can a cap
    // between `minimum` and `recommended` never keep live alongside a review
    // round", and the answer for an exempt class is "nothing" — a manager is
    // live at every cap (D3). Naming it here would tell a human to raise a
    // number to protect a pane the number does not reach.
    out
}

/// English-join a short list of noun phrases: `"a"`, `"a and b"`, `"a, b, and
/// c"`. Used to turn [`extra_tiers`]'s list into one clause of a warning
/// sentence — pulled out so the audit note and the launcher's message build
/// the same phrase instead of each hand-rolling their own `.join(...)`.
pub fn join_with_and(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} and {b}"),
        _ => {
            let (last, rest) = parts.split_last().expect("non-empty, matched above");
            format!("{}, and {last}", rest.join(", "))
        }
    }
}

/// **The gate decision** (reviewer half; the `also:` conditions are checked in the
/// shim, which is the only place that can call `gh pr checks`). Pure, so the
/// semantics are pinned by fast tests and the shell mirror has something to agree
/// with. `head` is the PR's current head commit — `None` when loomux could not
/// resolve it.
///
/// Order matters, and it is the order #197 asks for:
///
/// 1. **A blocking verdict refuses the merge** — before any counting, and
///    regardless of which revision it was recorded against. One reviewer's `fail`
///    is not outvoted by two passes, and `threshold: 2` does not mean "two yeses
///    beat a no". (A `fail` against an older commit still stands: "this PR has a
///    defect" does not stop being true because the author pushed more code. The
///    reviewer clears it by re-reviewing and re-recording.)
/// 2. **A `pass` only counts for the revision it reviewed.** A pass recorded
///    against an earlier head is *stale*: the branch moved, and what that reviewer
///    approved is not what would merge. It counts as outstanding, not as a pass —
///    which is why GitHub's own review model dismisses stale approvals on new
///    commits, and it is the #197 failure class ("merging code no reviewer saw")
///    that a PR-keyed verdict would have left wide open.
/// 3. Then the live PASS count must reach [`gate_need`]. Under `all-pass` that
///    means every named reviewer has passed *this* revision — a reviewer that
///    hasn't recorded anything keeps the gate shut, which is precisely the bug that
///    produced #197.
///
/// `threshold: N` deliberately does *not* wait for the reviewers it doesn't need:
/// an author who writes `threshold: 2` over three reviewers has said, in the file,
/// that two passes are enough. They still cannot merge over a `fail` (rule 1), and
/// the passes still have to be for the code that would actually merge (rule 2).
/// `all-pass` — the default when `require:` is omitted — is the one that waits for
/// everybody.
pub fn evaluate_merge_gate(
    gate: &Gate,
    verdicts: &BTreeMap<BlockId, ReviewVerdict>,
    head: Option<&str>,
) -> GateOutcome {
    let mut blocking: Vec<BlockId> = Vec::new();
    let mut outstanding: Vec<BlockId> = Vec::new();
    let mut stale: Vec<BlockId> = Vec::new();
    let mut passes = 0u32;
    // No resolvable head → no way to know whether any pass reviewed the code that
    // would merge. Refuse, rather than fall back to "a pass is a pass" — that
    // fallback IS the bug this binding closes.
    let Some(head) = head else {
        return GateOutcome::UnknownRevision;
    };
    for r in &gate.reviewers {
        match verdicts.get(r) {
            Some(v) if v.verdict.is_blocking() => blocking.push(r.clone()),
            Some(v) if v.reviewed(head) => passes += 1,
            Some(_) => stale.push(r.clone()),
            None => outstanding.push(r.clone()),
        }
    }
    if !blocking.is_empty() {
        return GateOutcome::Blocked { blocking };
    }
    let need = gate_need(gate);
    if passes >= need {
        GateOutcome::Satisfied
    } else {
        GateOutcome::Short { passes, need, outstanding, stale }
    }
}

/// Group-dir file holding the declared merge gate, written from the repo's
/// `.loomux/workflow.yml` at group create/resume and read by the `gh` shim.
/// **Absent = no gate**, which is what makes a repo with no workflow file (or one
/// declaring no `gates.merge`) behave byte-for-byte as it did before #222.
pub const MERGE_GATE_FILE: &str = "merge_gate";

/// Serialize a gate for [`MERGE_GATE_FILE`].
///
/// Line-oriented `key value [value]`, because the reader is a POSIX `while read`
/// loop with no JSON parser — the same reason the verdicts are a file tree. Every
/// token written here is already sanitized: block ids through [`sanitize_id`] and
/// conditions through [`sanitize_condition`], both of which *reject* (never
/// rewrite) anything outside their alphabet at parse time. That is the contract
/// #225 established for exactly this consumer, and it is what lets the shim word-
/// split the line without quoting. Belt and braces anyway: a token that would not
/// survive its sanitizer is dropped here rather than written into a shell's
/// `for` loop.
///
/// **A token that fails its sanitizer poisons the file rather than vanishing from
/// it.** The first draft silently dropped such a token — which, if the parse
/// contract ever regressed, would have emitted a *weaker* gate than the repo
/// declared (a reviewer or a condition just disappears, and the gate goes green
/// one requirement short). Every other fork in this feature chooses fail-closed on
/// exactly that question; this one now does too. [`POISON_KEY`] is a line the shim
/// cannot parse, and an unparseable line refuses every merge until a human looks.
pub fn gate_file_text(gate: &Gate) -> String {
    let mut out = String::from(
        // The source file is named GENERICALLY (#1153 phase 4): a repo may
        // declare its workflow at `.orrerix/workflow.yml` or the legacy
        // `.loomux/workflow.yml`, this function has no repo to resolve which,
        // and a header naming the wrong one would send a human editing a file
        // that isn't there. The brand word in the first phrase IS protocol
        // text, and flipped with #1153 phase 3. No parser reads it — the shim
        // skips `#` lines — but an agent opening the file does.
        "# orrerix merge gate — generated from this repo's workflow file (#222). Do not edit.\n",
    );
    match gate.require {
        GateRequire::AllPass => out.push_str("require all-pass\n"),
        GateRequire::Threshold(n) => out.push_str(&format!("require threshold {n}\n")),
    }
    for r in &gate.reviewers {
        match sanitize_id(r) {
            Some(clean) if clean == *r => out.push_str(&format!("reviewer {r}\n")),
            _ => out.push_str(&format!("{POISON_KEY} unusable-reviewer-id\n")),
        }
    }
    for c in &gate.also {
        match sanitize_condition(c) {
            Some(clean) if clean == *c => out.push_str(&format!("also {c}\n")),
            _ => out.push_str(&format!("{POISON_KEY} unusable-condition\n")),
        }
    }
    // #1174. A `0` here would be a clause that gates nothing, and `parse_workflow`
    // has already refused it — so if one ever reaches this far the file is
    // poisoned rather than written with a limit the shim would ignore.
    match gate.max_diff_lines {
        None => {}
        Some(0) => out.push_str(&format!("{POISON_KEY} unusable-max-diff-lines\n")),
        Some(n) => out.push_str(&format!("{MAX_DIFF_LINES_KEY} {n}\n")),
    }
    // #1176's routing rules, one line per (rule, glob) and one per (rule,
    // reviewer), each carrying the rule's 1-based index.
    //
    // **Why not one line per rule.** The reader is a POSIX `while read -r k v w`
    // loop with no arrays: a rule packed onto one line would have to be re-split
    // inside the shell, and every spelling of that (an `IFS` swap, a `set --`)
    // either clobbers the shim's own positional parameters or introduces a
    // second delimiter for a glob alphabet to have to avoid. Three fixed fields
    // fit the loop that is already there, and the index is what stitches the
    // halves back together — see [`parse_gate_file`], which refuses any file
    // where they do not stitch.
    if !gate.routing.is_empty() {
        if matches!(gate.require, GateRequire::Threshold(_)) {
            // `parse_workflow` refuses this pair outright, so reaching here means
            // the parse contract has regressed. Poison rather than write a file
            // whose two halves would be read as a LAXER gate than either says.
            out.push_str(&format!("{POISON_KEY} routing-with-threshold\n"));
        }
        if gate.routing.len() > ROUTING_RULES_MAX {
            out.push_str(&format!("{POISON_KEY} too-many-routing-rules\n"));
        }
    }
    for (i, rule) in gate.routing.iter().enumerate() {
        let idx = i + 1;
        // A rule missing either half is unsatisfiable-or-vacuous, and both are
        // refused at parse. Poisoned here for the same reason the tokens below
        // are: the file must never be a weaker gate than the workflow declared.
        if rule.paths.is_empty() || rule.reviewers.is_empty() {
            out.push_str(&format!("{POISON_KEY} incomplete-routing-rule\n"));
        }
        // The per-rule path cap, poisoned for the same reason the rule cap above
        // is (#1176 rev-972 N1): `parse_gate_file` refuses a file past it, so
        // writing one would emit a file loomux itself calls malformed.
        if rule.paths.len() > ROUTING_PATHS_MAX {
            out.push_str(&format!("{POISON_KEY} too-many-routing-paths\n"));
        }
        for p in &rule.paths {
            match sanitize_glob(p) {
                Some(clean) if clean == *p => {
                    out.push_str(&format!("{ROUTE_PATH_KEY} {idx} {p}\n"))
                }
                _ => out.push_str(&format!("{POISON_KEY} unusable-routing-glob\n")),
            }
        }
        for r in &rule.reviewers {
            match sanitize_id(r) {
                Some(clean) if clean == *r => {
                    out.push_str(&format!("{ROUTE_REVIEWER_KEY} {idx} {r}\n"))
                }
                _ => out.push_str(&format!("{POISON_KEY} unusable-routing-reviewer\n")),
            }
        }
    }
    out
}

/// The [`MERGE_GATE_FILE`] key carrying one routing rule's path glob (#1176):
/// `route-path <1-based rule index> <glob>`. Hyphenated to match the file's own
/// spelling convention (`all-pass`, `max-diff-lines`), which is not the YAML
/// key's.
pub const ROUTE_PATH_KEY: &str = "route-path";

/// The [`MERGE_GATE_FILE`] key carrying one routing rule's required reviewer:
/// `route-reviewer <1-based rule index> <block id>`. See [`ROUTE_PATH_KEY`].
pub const ROUTE_REVIEWER_KEY: &str = "route-reviewer";

/// The [`MERGE_GATE_FILE`] key carrying [`Gate::max_diff_lines`]. Hyphenated to
/// match `all-pass`/`ci-green` — the file's own spelling convention, which is
/// not the YAML key's (`max_diff_lines`), because the two have different
/// readers and the shim's is a `case` over word-split tokens.
pub const MAX_DIFF_LINES_KEY: &str = "max-diff-lines";

/// The key [`gate_file_text`] writes when a token cannot be represented safely.
/// Nothing parses it — by design: the shim refuses any gate-file line whose key it
/// does not recognize, so an unrepresentable gate refuses merges instead of
/// silently becoming a laxer one. Unreachable while the parse contract holds
/// (`parse_workflow` rejects such tokens outright); this is what happens if it
/// ever stops holding.
pub const POISON_KEY: &str = "unrepresentable";

/// Read [`MERGE_GATE_FILE`] back into a [`Gate`] — the inverse of
/// [`gate_file_text`], used by the registry to report gate status to the agent
/// that just recorded a verdict (the shim does its own read, in shell).
///
/// `None` means **this file is not a usable gate**, which the callers must report
/// as "malformed — every merge refused" rather than as "no gate": the file is on
/// disk, the shim will read it, and the shim refuses on exactly the things that
/// return `None` here. Those are a file with no reviewers (nobody could ever
/// satisfy it) and any line whose key loomux does not recognize — a poison line
/// ([`POISON_KEY`]), a truncation, a hand edit. The two halves agree, and both fail
/// closed.
pub fn parse_gate_file(text: &str) -> Option<Gate> {
    let mut require = GateRequire::AllPass;
    let mut reviewers: Vec<BlockId> = Vec::new();
    let mut also: Vec<String> = Vec::new();
    let mut max_diff_lines: Option<u32> = None;
    // #1176. Halves of a routing rule arrive on separate lines and are stitched
    // back together by index after the loop; `BTreeMap` so the stitch walks them
    // in rule order rather than file order.
    let mut rule_paths: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    let mut rule_reviewers: BTreeMap<u32, Vec<BlockId>> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut f = line.split_whitespace();
        match (f.next(), f.next(), f.next()) {
            // A threshold that doesn't parse (or is 0) leaves `require` at
            // `all-pass` — the STRICTER of the two. A malformed gate line must
            // never be the reason a merge gets easier.
            (Some("require"), Some("threshold"), Some(n)) => {
                if let Some(n) = n.parse().ok().filter(|n| *n > 0) {
                    require = GateRequire::Threshold(n);
                }
            }
            (Some("require"), Some("all-pass"), _) => require = GateRequire::AllPass,
            (Some("reviewer"), Some(id), _) => match sanitize_id(id) {
                Some(id) => reviewers.push(id),
                None => return None,
            },
            (Some("also"), Some(c), _) => match sanitize_condition(c) {
                Some(c) => also.push(c),
                None => return None,
            },
            // #1174. Unlike `require threshold`, an unusable number here has no
            // stricter fallback to land on — "no limit" is the LAXER reading, so
            // the whole file is unusable instead, which the callers report as
            // "malformed — every merge refused". Same direction as the
            // `reviewer`/`also` arms above.
            (Some(MAX_DIFF_LINES_KEY), Some(n), _) => {
                match n.parse::<u32>().ok().filter(|n| *n > 0) {
                    Some(n) => max_diff_lines = Some(n),
                    None => return None,
                }
            }
            // #1176. Same direction as every arm above: a routing line loomux
            // cannot read makes the whole file unusable, because the thing it
            // would have added is a REQUIRED reviewer. A dropped one is a merge
            // that skipped a lane, which is precisely the laxening this reader
            // refuses to perform.
            // **Rejected, never rewritten** — and the comparison is what makes
            // that true. `sanitize_glob`/`sanitize_id` FILTER: `src/[ab]` comes
            // back as `src/ab`, which is not a refusal, it is a DIFFERENT RULE
            // silently substituted for the one the file carries. `gate_file_text`
            // poisons rather than writes such a token, so anything reaching here
            // is a hand edit or a corruption — exactly the case this reader must
            // refuse rather than quietly reinterpret.
            //
            // A **fourth token** is refused for the same reason: neither
            // alphabet contains whitespace, so a line that has any has already
            // been truncated by the word split — `route-path 1 src/a b` reads as
            // the narrower glob `src/a`, which is clean and wrong. Exactly three
            // fields, or the file is not a gate.
            (Some(ROUTE_PATH_KEY), Some(i), Some(g)) => {
                let idx = routing_index(i)?;
                let clean = sanitize_glob(g)?;
                if clean != g || f.next().is_some() {
                    return None;
                }
                rule_paths.entry(idx).or_default().push(clean);
            }
            (Some(ROUTE_REVIEWER_KEY), Some(i), Some(r)) => {
                let idx = routing_index(i)?;
                let clean = sanitize_id(r)?;
                if clean != r || f.next().is_some() {
                    return None;
                }
                rule_reviewers.entry(idx).or_default().push(clean);
            }
            // Anything else — a poison line, a truncated key, a hand edit — makes
            // the whole file unusable. Skipping it would drop a requirement.
            _ => return None,
        }
    }
    // Stitch the two halves. The indices must be exactly 1..=N with BOTH halves
    // present for every one of them: a gap, a duplicate index that lost its
    // partner, or a `route-path` with no `route-reviewer` is a file that cannot
    // be read as the gate someone declared, and an unreadable gate refuses.
    let n = rule_paths.len().max(rule_reviewers.len());
    if n > ROUTING_RULES_MAX {
        return None;
    }
    let mut routing: Vec<RoutingRule> = Vec::new();
    for idx in 1..=n as u32 {
        let paths = rule_paths.remove(&idx)?;
        let reviewers = rule_reviewers.remove(&idx)?;
        if paths.is_empty() || reviewers.is_empty() || paths.len() > ROUTING_PATHS_MAX {
            return None;
        }
        routing.push(RoutingRule { paths, reviewers });
    }
    // Anything left over means the indices were not contiguous — an index past
    // `n`, which nothing above could have consumed.
    if !rule_paths.is_empty() || !rule_reviewers.is_empty() {
        return None;
    }
    // The pair `parse_workflow` refuses (see there for why). Refused here too,
    // rather than trusted to be impossible: this reader's whole job is to be the
    // half that does not assume the other half held.
    if !routing.is_empty() && matches!(require, GateRequire::Threshold(_)) {
        return None;
    }
    (!reviewers.is_empty()).then_some(Gate { require, reviewers, also, max_diff_lines, routing })
}

/// A `route-path`/`route-reviewer` line's rule index — 1-based, so `0` is not an
/// index and is refused with everything else that is not a plain positive number.
fn routing_index(s: &str) -> Option<u32> {
    s.parse::<u32>().ok().filter(|n| *n > 0)
}

/// What the small-batch clause (#1174) says about one PR — the pure decision
/// the shim's shell mirrors and the merge queue re-runs, so there is exactly
/// one definition of "too big" in this codebase and two readers of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSizeVerdict {
    /// No `max_diff_lines` declared, or the PR is within it.
    Ok,
    /// The PR's changed-line count exceeds the declared limit.
    TooLarge { lines: u64, limit: u32 },
    /// The limit is declared and the PR's size could not be read at all.
    /// **Refuses** — the same posture `ci-green` takes on unreadable checks and
    /// the queue takes on an unverifiable base: unknown is never "fine".
    Unknown { limit: u32 },
}

impl DiffSizeVerdict {
    pub fn ok(&self) -> bool {
        matches!(self, DiffSizeVerdict::Ok)
    }
}

/// Apply [`Gate::max_diff_lines`] to a PR whose changed-line count is `lines`
/// (additions + deletions), or `None` when that could not be resolved.
///
/// A gate that declares no limit answers [`DiffSizeVerdict::Ok`] **without
/// looking at `lines`** — the absent-config no-op that keeps every repo which
/// never declared the key on exactly the path it was on before #1174.
pub fn check_diff_size(gate: &Gate, lines: Option<u64>) -> DiffSizeVerdict {
    let Some(limit) = gate.max_diff_lines else {
        return DiffSizeVerdict::Ok;
    };
    match lines {
        None => DiffSizeVerdict::Unknown { limit },
        Some(lines) if lines > u64::from(limit) => DiffSizeVerdict::TooLarge { lines, limit },
        Some(_) => DiffSizeVerdict::Ok,
    }
}
