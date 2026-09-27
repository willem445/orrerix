//! Path-based reviewer routing (#1176).

use super::*;

// ── path-based reviewer routing (#1176) ─────────────────────────────────────

/// One routing rule that MATCHED — the "which rules fired and why" half of the
/// gate's own report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FiredRule {
    /// The rule's 1-based position in `gates.merge.routing`, so a refusal can
    /// name the line the author has to look at.
    pub index: u32,
    /// The rule's **declared** globs — all of them, not the one that happened to
    /// match. Deliberate: the shim streams the changed-file list and tests the
    /// globs per file, this walks the globs per rule, and "the first glob that
    /// matched" is therefore a different string on the two sides whenever a rule
    /// has more than one. The rule's own text is the same on both, and it is
    /// also the thing the author needs to see.
    pub paths: Vec<String>,
    /// The reviewers this rule requires.
    pub reviewers: Vec<BlockId>,
}

/// The required-reviewer set for one PR, once routing has been applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutingDecision {
    /// [`Gate::reviewers`] ∪ every fired rule's reviewers, in that order:
    /// the static list first, then the rules in declaration order, each new id
    /// appended once. The `gh` shim appends in exactly this order too, so the
    /// two produce the same list and not merely the same set.
    pub required: Vec<BlockId>,
    /// Which rules fired. Empty when the gate declares no routing, or when it
    /// declares routing and nothing matched.
    pub fired: Vec<FiredRule>,
}

impl RoutingDecision {
    /// `base` with its reviewer list replaced by [`required`](Self::required)
    /// and its routing spent — **the effective gate**, which is what everything
    /// downstream evaluates.
    ///
    /// Routing resolves to a reviewer list and then gets out of the way, so
    /// [`evaluate_merge_gate`], [`gate_need`] and the `body-unchanged` loop stay
    /// exactly one implementation each. A second gate decision that knew about
    /// routing would be a third implementation of the gate, which this codebase
    /// treats as a defect rather than an optimization.
    pub fn gate(&self, base: &Gate) -> Gate {
        Gate {
            require: base.require,
            reviewers: self.required.clone(),
            also: base.also.clone(),
            max_diff_lines: base.max_diff_lines,
            routing: Vec::new(),
        }
    }
}

/// Apply [`Gate::routing`] to one PR's changed files — **the pure decision the
/// `gh` shim's shell mirrors and the merge queue re-runs.**
///
/// `changed` is the PR's repo-relative changed paths, or `None` when that list
/// could not be resolved *or could not be shown to be complete* (see
/// [`ROUTING_FILES_JQ`]).
///
/// - A gate that declares **no routing** answers without looking at `changed` at
///   all — the absent-config no-op that keeps every repo which never wrote the
///   key on exactly the path it was on before #1176, and the same shape
///   [`check_diff_size`] takes.
/// - A gate that **does** declare routing and cannot see the file list answers
///   `None`: **refuse.** Unknown is never safe here in a particularly sharp way
///   — the unknown thing is *which reviewers are required*, so guessing "none of
///   the rules fired" is guessing in favour of merging.
pub fn route_reviewers(gate: &Gate, changed: Option<&[String]>) -> Option<RoutingDecision> {
    if gate.routing.is_empty() {
        return Some(RoutingDecision { required: gate.reviewers.clone(), fired: Vec::new() });
    }
    let changed = changed?;
    let mut required = gate.reviewers.clone();
    let mut fired: Vec<FiredRule> = Vec::new();
    for (i, rule) in gate.routing.iter().enumerate() {
        if !rule.paths.iter().any(|g| changed.iter().any(|f| glob_match(g, f))) {
            continue;
        }
        for r in &rule.reviewers {
            if !required.contains(r) {
                required.push(r.clone());
            }
        }
        fired.push(FiredRule {
            index: i as u32 + 1,
            paths: rule.paths.clone(),
            reviewers: rule.reviewers.clone(),
        });
    }
    Some(RoutingDecision { required, fired })
}

/// The first line [`ROUTING_FILES_JQ`] emits when — and only when — it can
/// account for every changed file on the PR.
pub const ROUTED_FILES_OK: &str = "ok";

/// The prefix on each path line [`ROUTING_FILES_JQ`] emits.
///
/// A prefix rather than a bare path so the status word and the data live in
/// different shapes: without it, a repo containing a file literally named `ok`
/// would emit a line indistinguishable from the header.
pub const ROUTED_FILES_PREFIX: &str = "p ";

/// Reduce `gh pr view --json files,changedFiles` to the changed-path list
/// routing needs — **one definition, two consumers**, the same arrangement
/// [`BASE_CHECK_RUNS_JQ`] established: the `gh` shim interpolates this constant
/// into its POSIX body and `mqdriver::pr_files_argv` passes it to `gh --jq`, so
/// the shim and the merge queue cannot ask GitHub different questions.
///
/// Output is a line-oriented protocol read by [`parse_routed_files`] in Rust and
/// by a `while read` loop in shell: the word [`ROUTED_FILES_OK`], then one
/// `p <path>` line per changed file. **Anything else is a refusal** — there is
/// no word for "some of the files", because a partial list is not an answer to
/// "did this PR touch `src/**`".
///
/// **The truncation clause is the whole reason this is a reduction and not a
/// plain `.files[].path`.** `gh pr view --json files` fetches ONE page: the
/// GraphQL `files` connection is capped at 100 while `changedFiles` counts them
/// all. Verified live against this repo — PR #1181 answered
/// `{changed: 32, listed: 32}` and PR #1018 answered `{changed: 135, listed:
/// 100}` — so on any PR past a hundred files the list silently omits the tail.
/// For a *size* gate that omission would be visible; for routing it fails
/// **open** and invisibly: the one file that would have matched a rule sits on
/// page two, no rule fires, and the lane the repo asked for is quietly not
/// required. That is #1181's own pagination finding wearing routing's hat, so it
/// gets the same answer — a page that cannot account for every file is not an
/// answer, and `!=` (not `<`) is the comparison, because a count that disagrees
/// in *either* direction means the payload is not the shape this question rests
/// on.
///
/// The shape it rests on is **checked, not assumed**, for the same reason
/// `BASE_CHECK_RUNS_JQ` checks its own: `null` sorts below every number in jq,
/// so an absent `changedFiles` would make a comparison read false and fall
/// through to the answer that merges. `has(...)` answers "unaccountable"
/// instead.
///
/// A path carrying a **newline or carriage return** is refused too. It cannot be
/// expressed in a one-path-per-line protocol at all, so a reader would silently
/// see two files where the repo has one — and the shim's reader is a merge gate
/// being fed a path that a fork PR's author chose.
pub const ROUTING_FILES_JQ: &str = "if (has(\"files\")|not) or (has(\"changedFiles\")|not) then \"unaccountable\" elif (.files|length) != .changedFiles then \"unaccountable\" elif any(.files[]; (.path|type) != \"string\" or (.path|length) == 0 or (.path|test(\"[\\n\\r]\"))) then \"unaccountable\" else ([\"ok\"] + (.files|map(\"p \" + .path)))[] end";

/// Read [`ROUTING_FILES_JQ`]'s output back into a changed-path list — the Rust
/// half of that protocol, mirrored in shell by the `gh` shim.
///
/// `None` for **anything** that is not a complete, well-formed answer: the
/// `unaccountable` word, an empty capture (gh failed, jq errored, the PR is
/// gone), a line without the [`ROUTED_FILES_PREFIX`], an empty path. Callers
/// hand that straight to [`route_reviewers`] as `None`, which refuses.
///
/// A PR with genuinely zero changed files is `Some(vec![])`, not `None`: that is
/// a complete answer that happens to be empty, and the distinction is the whole
/// difference between "no rule matched" and "loomux cannot say".
pub fn parse_routed_files(out: &str) -> Option<Vec<String>> {
    let mut lines = out.lines();
    if lines.next().map(str::trim) != Some(ROUTED_FILES_OK) {
        return None;
    }
    let mut files: Vec<String> = Vec::new();
    for line in lines {
        // A trailing blank line is how a capture ends, not a file.
        if line.trim().is_empty() {
            continue;
        }
        let path = line.strip_prefix(ROUTED_FILES_PREFIX)?;
        if path.is_empty() {
            return None;
        }
        files.push(path.to_string());
    }
    Some(files)
}
