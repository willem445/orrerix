//! The `gh`/`git` gate decisions the shims defer to: merge, release, close and
//! tag-push classification, the owner roster, and the human's merge grants.
//! Design note: `docs/design/shim-path-integrity.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. It calls no
//! sibling file.

use super::*;

/// Monotonic tiebreaker for grant temp-file names + grant nonces, so a grant
/// write is atomic and each nonce is unique without a randomness/uuid crate
/// (getrandom is banned — see the build notes). Combined with the pid it never
/// collides across concurrent writers.
pub(in crate::orchestration) static GRANT_SEQ: AtomicU64 = AtomicU64::new(0);

// ---------- enforced merge gate (#83): gh-shim decision logic ----------

/// What the `gh` shim should do with an intercepted invocation. The shim mirrors
/// this in shell; the logic is pinned here as a pure, unit-tested Rust function so
/// the security-critical decision has one authoritative specification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GhGate {
    /// Not a merge (or a merge onto a non-default base): run the real gh unchanged.
    PassThrough,
    /// A merge onto the default branch, and both consent markers are present:
    /// autonomous mode is on AND auto-merge is enabled — allow it.
    AllowMerge,
    /// A privileged action (default-branch merge, or a release/tag publish)
    /// authorized by a valid one-time human grant. The shim CONSUMES the grant and
    /// allows exactly this one action.
    AllowGrant,
    /// Allowed by **supervised dangerous mode** (`dangerous_mode` marker, while NOT
    /// autonomous): the human is present and told the agent to just do merges/
    /// releases. Distinct from the autonomous blanket and from a grant so the audit
    /// records which gate path allowed it. Not consumed (it's a standing mode).
    AllowDangerous,
    /// A merge onto the default branch (or a release) without authorization: block
    /// (the human gate — the human can grant a one-time exception).
    Block,
    /// A merge whose base branch couldn't be determined: block, fail-safe — we must
    /// never let an unverifiable merge reach the default branch.
    BlockUnverifiable,
}

/// gh flags (across the subcommands we gate — `pr merge`, `release create/edit/
/// delete`, `-R/--repo`) that take a SEPARATE value token, so a positional scan
/// must consume that value and not mistake it for the command/subcommand/target.
/// Missing one mis-parses the target: e.g. `gh release create --title "X" v1`
/// would read the tag as `X` and fail-safe-block a legitimately granted release
/// (rev-86 LOW). `=`/glued forms are single tokens and handled separately. Keep
/// this in sync with the shim's shell scanner value-flag list.
pub(in crate::orchestration) const GH_VALUE_FLAGS: &[&str] = &[
    "-R", "--repo", "-b", "--body", "-t", "--subject", "--title", "-F", "--body-file",
    // #2985: gh pr close/reopen take -c/--comment, so a positional scan blind to
    // it reads the comment text as the PR selector.
    "-c", "--comment",
    "--author-email", "--match-head-commit", "-n", "--notes", "--notes-file",
    "--notes-start-tag", "--target", "--discussion-category",
];

/// [`GH_VALUE_FLAGS`], for the tests that assert the shim's GENERATED shell arms
/// really cover every entry (#2985 rev-std finding 1). The const stays private:
/// the point of the accessor is that a test can enumerate the ONE list, not that
/// callers get a second way to spell it.
#[doc(hidden)] // pub for integration tests
pub fn gh_value_flags() -> &'static [&'static str] {
    GH_VALUE_FLAGS
}

/// The positional (non-flag) tokens of a gh argv, in order, skipping flags and
/// consuming the values of `GH_VALUE_FLAGS` — crucially the global `-R/--repo` that
/// gh accepts BEFORE or BETWEEN the command tokens (rev-79 F1), and the release
/// value-flags before the tag positional (rev-86). `--flag=x` / `-Rx` are single
/// tokens, skipped as flags. Excludes the leading `gh`. So `positionals[0]`/`[1]`
/// are the command + subcommand and `[2]` is the target (PR ref / tag) wherever
/// the flags land.
pub fn gh_positionals(args: &[&str]) -> Vec<String> {
    let mut pos = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let t = args[i];
        if GH_VALUE_FLAGS.contains(&t) {
            i += 2; // flag + its value
            continue;
        }
        if t.starts_with('-') {
            i += 1; // boolean flag, or `--flag=…` / `-R…` glued (single token)
            continue;
        }
        pos.push(t.to_string());
        i += 1;
    }
    pos
}

/// The `-R/--repo` value from a gh argv, if present, in any accepted form
/// (`-R x`, `--repo x`, `--repo=x`, `-Rx`). The shim passes this to its base /
/// default-branch lookups so they resolve for the SAME repo the user targeted
/// (rev-79 F2), not always the cwd repo. Pure/testable.
pub fn gh_repo_flag(args: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let t = args[i];
        if t == "-R" || t == "--repo" {
            return args.get(i + 1).map(|s| s.to_string());
        }
        if let Some(v) = t.strip_prefix("--repo=") {
            return Some(v.to_string());
        }
        if t.len() > 2 {
            if let Some(v) = t.strip_prefix("-R") {
                return Some(v.to_string());
            }
        }
        i += 1;
    }
    None
}

/// Whether an argv is a GitHub *merge* invocation the shim must gate: `gh pr merge`
/// (in ANY flag arrangement, including `-R/--repo` before or between the command
/// tokens — rev-79 F1) or a raw `gh api` call to a pull-request merge endpoint (the
/// cheap-to-catch API bypass). Pure so both the shim's shell mirror and the tests
/// agree on exactly what counts as a merge. `args` excludes the leading `gh`. The
/// api-graphql `mergePullRequest` mutation is also caught.
pub fn gh_is_merge_invocation(args: &[String]) -> bool {
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let pos = gh_positionals(&a);
    let cmd = pos.first().map(String::as_str);
    let sub = pos.get(1).map(String::as_str);
    // `gh [globals] pr [flags] merge …` — command+subcommand wherever the flags land.
    if cmd == Some("pr") && sub == Some("merge") {
        return true;
    }
    // `gh api …` touching a pulls/<n>/merge REST endpoint or the graphql
    // mergePullRequest mutation. Conservative substring match over the args.
    if cmd == Some("api") {
        let joined = a.join(" ");
        let low = joined.to_ascii_lowercase();
        if low.contains("mergepullrequest") {
            return true;
        }
        // REST: .../pulls/<something>/merge  (also /merge as the tail)
        if joined.contains("/merge") && joined.contains("pulls") {
            return true;
        }
    }
    false
}

/// The gate decision (pure spec for the shim), the SINGLE decision point every
/// merge form routes through. `is_merge` is [`gh_is_merge_invocation`];
/// `base`/`default` are the PR base branch and the repo default branch as resolved
/// by the *real* gh (`None` = couldn't determine); `autonomous`/`auto_merge`/
/// `dangerous`/`grant_valid` are the group's live marker states. A merge onto the
/// default branch is allowed when **`(autonomous && auto_merge)`** (autonomous
/// blanket) OR **`(dangerous && !autonomous)`** (supervised dangerous mode — the
/// human is present) OR a valid one-time grant; a non-default base passes; an
/// undeterminable base fails safe (block). `dangerous` is a no-op while autonomous
/// (the two are mutually exclusive, enforced at the setters; the `!autonomous`
/// guard is defensive).
pub fn gh_gate_decision(
    is_merge: bool,
    base: Option<&str>,
    default: Option<&str>,
    autonomous: bool,
    auto_merge: bool,
    dangerous: bool,
    grant_valid: bool,
) -> GhGate {
    if !is_merge {
        return GhGate::PassThrough;
    }
    match (base, default) {
        (Some(b), Some(d)) if !b.is_empty() && !d.is_empty() => {
            if b != d {
                GhGate::PassThrough // integration-branch flow is untouched
            } else if autonomous && auto_merge {
                GhGate::AllowMerge // blanket opening while in autonomous auto-merge
            } else if dangerous && !autonomous {
                GhGate::AllowDangerous // supervised: human present, told it to merge
            } else if grant_valid {
                GhGate::AllowGrant // one-time human grant for THIS pr — consumed
            } else {
                GhGate::Block
            }
        }
        // A raw `gh api` merge has no cheaply-resolvable base ref → block it as an
        // unverifiable default-branch merge (the api path is a documented bypass
        // surface; blocking the cheap-to-catch shape is the safe default).
        _ => GhGate::BlockUnverifiable,
    }
}

/// A release-publishing gh action the shim must gate (`gh release create|edit|
/// delete <tag>`) and the tag it targets. `None` for any other gh (incl. read-only
/// `release view`/`list`/`download`). Pure over the parsed positionals so the shim
/// and tests agree. Uses `gh_positionals` so `-R/--repo` before/between tokens is
/// handled like the merge path.
pub fn gh_release_action(args: &[String]) -> Option<(String, String)> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let pos = gh_positionals(&a);
    if pos.first().map(String::as_str) != Some("release") {
        return None;
    }
    let sub = pos.get(1).map(String::as_str)?;
    if !matches!(sub, "create" | "edit" | "delete") {
        return None; // view/list/download/upload/download → not a publish action
    }
    // `gh release <sub> <tag>` — the tag is the next positional.
    let tag = pos.get(2)?.to_string();
    Some((sub.to_string(), tag))
}

/// The gate decision for a release/tag publish (#83). Parallel to
/// `gh_gate_decision` for merges: allowed when **`(autonomous && auto_release)`**
/// (the blanket opening) OR a valid grant for that tag. Note the asymmetry with
/// merges that #438 introduced: `AllowGrant` here does **not** mean "consumed" —
/// a release grant is a pipeline grant, re-checked per step and bounded by tag
/// identity + TTL rather than by use count (`RELEASE_GRANT_VALID_SH`). This
/// function is unchanged by that: whether the caller spends what it matched is
/// the shim's business, not the decision's. Because
/// publishing to the world (GitHub release + npm via a `v*` tag → release.yml) is a
/// bigger blast radius than a merge, releases get their **own independent** toggle
/// (`auto_release`, default OFF) — turning on autonomous never surprise-publishes;
/// the human opts in separately, or grants each release one at a time.
pub fn release_gate_decision(
    autonomous: bool,
    auto_release: bool,
    dangerous: bool,
    grant_valid: bool,
) -> GhGate {
    if autonomous && auto_release {
        GhGate::AllowMerge // blanket opening (reusing the "allowed, not grant-consumed" variant)
    } else if dangerous && !autonomous {
        GhGate::AllowDangerous // supervised: human present, told it to release
    } else if grant_valid {
        GhGate::AllowGrant
    } else {
        GhGate::Block
    }
}

/// The name of the shim-readable owner roster inside a group dir (#2985). One
/// place, because the Rust writer and the shell reader are two programs that
/// must agree about a path, and `the_shim_reads_the_owner_roster_the_backend_
/// writes` pins that they do.
pub const OWNER_ROSTER_FILE: &str = "agent_owners";

/// Render the shim-readable owner roster (#2985): one line per agent, three
/// space-separated fields — `<agent-id> <role> <branch>` — with the branch
/// omitted where the agent has none.
///
/// **Why a flat file beside `agents.json` rather than the JSON itself.** The
/// consumer is a POSIX `sh` gate that must be able to fail CLOSED, and a shell
/// JSON parser is neither of those things: it would be approximate, and an
/// approximate reader on a security gate fails toward *allowing* the close it
/// could not parse. The shim's own `merge_gate` file is the same shape and the
/// same argument. This is a projection of `agents.json`, written in the same
/// `tasks_lock`-serialized, whole-file atomic replace, from the same record
/// list — so the two cannot disagree about who exists.
///
/// **The alphabet is what makes three space-separated fields safe.** An agent
/// id is a [`PathSegment`], whose alphabet excludes whitespace outright, and a
/// role is one of a fixed set of lowercase words. A git branch name cannot
/// contain a space either (`git check-ref-format` refuses one), but a roster
/// row is not required to be a real branch — a hand-edited `agents.json` could
/// carry anything — so a row whose branch field would introduce a second
/// separator is DROPPED rather than written half-parsed. A dropped row makes
/// that agent unidentifiable to the gate, which refuses; the other direction
/// would let one row's text be read as another field.
pub fn render_owner_roster(rows: &[(String, String, Option<String>)]) -> String {
    let mut out = String::new();
    for (id, role, branch) in rows {
        let b = branch.as_deref().unwrap_or("");
        if id.is_empty()
            || role.is_empty()
            || [id.as_str(), role.as_str(), b]
                .iter()
                .any(|f| f.chars().any(|c| c.is_whitespace()))
        {
            continue;
        }
        out.push_str(id);
        out.push(' ');
        out.push_str(role);
        out.push(' ');
        out.push_str(b);
        out.push('\n');
    }
    out
}

/// Whether a `gh` argv is a PR **close** or **reopen** the shim must account for
/// (#2985), and whether it also asks for the head branch to be deleted.
/// `Some(("close", true))` for `gh pr close --delete-branch 7`. `None` for
/// everything else — including `gh pr merge`, whose `--delete-branch` is the
/// orchestrator's documented post-merge step and stays under the merge gate
/// alone (CLAUDE.md's "whoever performs the merge owns the branch delete").
///
/// Pure over `gh_positionals`, so `-R/--repo` and `-c/--comment` landing before
/// or between the command tokens parse the same way the merge path's do.
pub fn gh_close_action(args: &[String]) -> Option<(String, bool)> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let pos = gh_positionals(&a);
    if pos.first().map(String::as_str) != Some("pr") {
        return None;
    }
    let sub = pos.get(1).map(String::as_str)?;
    if !matches!(sub, "close" | "reopen") {
        return None;
    }
    // `-d` is gh's own shorthand for `--delete-branch`, and a bundled short
    // cluster (`-dc "msg"`) is not a form gh accepts for a value-taking flag, so
    // exact tokens are the whole alphabet here.
    let delete_branch = a.iter().any(|t| *t == "--delete-branch" || *t == "-d");
    Some((sub.to_string(), delete_branch))
}

/// Does `head` belong to the agent whose own branch is `own`? (#2985)
///
/// **Exact, or a descendant under a separator** — the rule issue #2985 states
/// ("a worker's legitimate closes are its own scratch PRs, same branch prefix"),
/// narrowed by requiring the separator. A BARE prefix test is the defect this
/// function exists to avoid: `fix/29` would own `fix/2985-x`, which is another
/// worker's branch, and the incident this guard was written for closed five
/// PRs belonging to other workers. So `fix/2985-x` owns `fix/2985-x-scratch2`
/// and `fix/2985-x/wip`, and owns nothing else.
///
/// An empty `own` owns NOTHING — an agent with no recorded branch (the
/// orchestrator, a planner, a reviewer with no worktree) must not be handed
/// every branch in the repo by an empty-prefix match. The orchestrator's
/// authority comes from its ROLE, checked separately in [`gh_close_decision`].
pub fn gh_branch_is_owned(head: &str, own: &str) -> bool {
    if own.is_empty() || head.is_empty() {
        return false;
    }
    if head == own {
        return true;
    }
    match head.strip_prefix(own) {
        Some(rest) => rest.starts_with('/') || rest.starts_with('-'),
        None => false,
    }
}

/// What the close gate decides. Every variant is audited — the incident's five
/// closes left NO audit row at all, so "allowed" is a record here, not a silence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GhCloseGate {
    /// Not a `gh pr close`/`reopen`: run the real gh unchanged.
    PassThrough,
    /// A reopen. Always allowed — reopening a PR destroys nothing, and the
    /// incident's own remediation was a reopen loop. Audited so the next
    /// orchestrator can read who did it instead of asking the human (#2985 §2).
    AllowReopen,
    /// The caller is the orchestrator, which may close any PR in its group.
    AllowOrchestrator,
    /// The PR's head branch is the caller's own branch (or a descendant of it).
    AllowOwner,
    /// A close of a PR the caller does not own — refused.
    BlockNotOwner,
    /// The head branch, or the caller's own identity, could not be determined.
    /// Fail-closed, exactly as an unverifiable merge base does.
    BlockUnverifiable,
}

/// The close-gate decision (pure spec for the shim's shell mirror), the single
/// decision point every `gh pr close`/`reopen` routes through (#2985).
///
/// `action` is [`gh_close_action`]'s subcommand; `role` is the calling agent's
/// role as recorded in the group's owner roster (empty = the caller could not be
/// identified); `own_branch` is that agent's recorded branch (empty = none);
/// `head` is the PR's `headRefName` as resolved by the *real* gh (`None` =
/// couldn't determine).
///
/// **`delete_branch` is not an input to this decision, deliberately.** A close
/// the caller is allowed to make is allowed to take its own branch with it, and
/// a close it is not allowed to make is refused whether or not it asked for the
/// delete. The flag is carried through [`gh_close_action`] because the refusal
/// MESSAGE names it — a refused `--delete-branch` is the shape that was one flag
/// away from being unrecoverable — and because the audit row records it.
pub fn gh_close_decision(
    action: Option<&str>,
    role: &str,
    own_branch: &str,
    head: Option<&str>,
) -> GhCloseGate {
    match action {
        None => GhCloseGate::PassThrough,
        Some("reopen") => GhCloseGate::AllowReopen,
        Some(_) => {
            // The orchestrator's authority is over the GROUP, not over a branch,
            // so it is settled before the head ref is even needed — an
            // orchestrator closing a PR whose head gh cannot report is still the
            // orchestrator. (It is audited either way.)
            if role == "orchestrator" {
                return GhCloseGate::AllowOrchestrator;
            }
            // Everything below needs BOTH halves of the ownership question
            // answerable. An unknown caller (no roster row, no agent id in the
            // environment) and an unresolvable head ref are the same epistemic
            // state — "this app cannot say whose PR this is" — and that is never
            // "probably fine".
            match head {
                Some(h) if !h.is_empty() && !role.is_empty() => {
                    if gh_branch_is_owned(h, own_branch) {
                        GhCloseGate::AllowOwner
                    } else {
                        GhCloseGate::BlockNotOwner
                    }
                }
                _ => GhCloseGate::BlockUnverifiable,
            }
        }
    }
}

/// The refusal a non-owning close is answered with (#2985) — the **template**,
/// with its two variable clauses already rendered.
///
/// This is the level the shim is generated from, and that is the whole point:
/// `gh_shim_sh` calls this function at shim-WRITE time with the shell's own
/// variable names (`"$c_pr"`, `"$c_head"`, …) as the arguments, so the sentence
/// the shim prints is emitted from this one Rust string rather than retyped in
/// shell. A one-sided edit is not expressible — the same construction
/// `RELEASE_GRANT_VALID_SH` uses for the release-grant check, and for the same
/// reason (two programs, one guarantee).
///
/// One paragraph, per the house idiom: a `\` line continuation strips the
/// newline AND the source indentation, so nothing here ships a hard break or a
/// run of leading spaces to the agent reading it.
///
/// It names all four things the agent needs in order to do the right thing
/// instead of retrying: WHICH PR, WHICH branch it is, WHOSE it is (the owning
/// agent, by name, where the roster knows one — and that nobody on it does
/// where it does not), and what its own branch is — the incident's worker
/// had none of those and did not notice for minutes.
pub fn gh_close_refusal_with(
    pr: &str,
    head: &str,
    own_clause: &str,
    owner_clause: &str,
    del_clause: &str,
) -> String {
    format!(
        "orrerix: refusing to close PR #{pr} — its head branch is '{head}', which this agent does \
         not own ({own_clause}), so {owner_clause}.{del_clause} \
         A worker, reviewer or planner may close only a PR opened from its own branch or a scratch \
         branch beneath it; anything else is the orchestrator's call or the human's. This is the \
         guard for a live incident in which a loop over computed PR numbers closed five other \
         workers' open PRs in six seconds. If this PR really should be closed, say so in a report \
         and let the orchestrator or the human close it — do NOT retry, and do NOT close by \
         computed number: enumerate what is actually yours with 'gh pr list --author @me --head \
         <your-branch>' and close from that list."
    )
}

/// The `({own})` clause of [`gh_close_refusal_with`]. Emitted into the shim
/// twice — once with the shell's `$c_branch` for the has-a-branch arm, once with
/// `""` for the arm that has none — so both spellings come from here.
pub fn gh_close_own_clause(own_branch: &str) -> String {
    if own_branch.is_empty() {
        "this pane has no branch of its own recorded".to_string()
    } else {
        format!("your own branch is '{own_branch}'")
    }
}

/// Who the PR belongs to, for [`gh_close_refusal_with`]. Issue #2985 asks the
/// refusal to name the PR's **owner**, not merely the branch: "refuse with the
/// PR's owner named". The shim resolves it from the same roster it decides
/// with, by the same ownership rule.
///
/// An empty `owner` is not a failure and must not read as one: a branch whose
/// agent has exited, or one the human pushed, legitimately belongs to nobody on
/// the roster. That arm says so instead of naming a guess — which is what the
/// pre-fix sentence ("belongs to another agent or to the human") did for EVERY
/// refusal, including the ones where the owner was sitting in the roster.
pub fn gh_close_owner_clause(owner: &str) -> String {
    if owner.is_empty() {
        "no agent on this group's roster owns that branch, so it is another agent's from an \
         earlier session or the human's"
            .to_string()
    } else {
        format!("it belongs to {owner}")
    }
}

/// The `--delete-branch` sentence of [`gh_close_refusal_with`], or empty. A
/// refused `--delete-branch` is the shape the incident was one flag away from
/// making unrecoverable, so the refusal says out loud that it was asked for.
pub fn gh_close_del_clause(delete_branch: bool) -> &'static str {
    if delete_branch {
        " This call also passed --delete-branch, which would have deleted that branch as well."
    } else {
        ""
    }
}

/// The whole refusal, for callers that have the raw facts rather than rendered
/// clauses (the Rust tests, and anything that wants the message without going
/// through the shim). Composes the three functions above, so it cannot say
/// anything the generated shim does not.
pub fn gh_close_refusal(
    pr: &str,
    head: &str,
    own_branch: &str,
    owner: &str,
    delete_branch: bool,
) -> String {
    gh_close_refusal_with(
        pr,
        head,
        &gh_close_own_clause(own_branch),
        &gh_close_owner_clause(owner),
        gh_close_del_clause(delete_branch),
    )
}

/// The refusal for a close whose ownership this app could not establish at all
/// (#2985) — the template, with the `why` clause already rendered. Generated
/// into the shim the same way [`gh_close_refusal_with`] is.
///
/// Fail-closed, and the message says which half is missing so the agent does not
/// read a real infrastructure fault as a policy decision. One paragraph, same
/// idiom as [`gh_close_refusal_with`].
pub fn gh_close_unverifiable_refusal_with(pr: &str, why: &str) -> String {
    format!(
        "orrerix: refusing to close PR #{pr} — {why}. A close that cannot be attributed is refused \
         rather than guessed at, the same way an unverifiable merge base is: a wrong close cancels \
         another agent's review drive and, with --delete-branch, is not recoverable. Report this \
         to the orchestrator or the human and let them close it."
    )
}

/// The two `why` clauses of [`gh_close_unverifiable_refusal_with`]. `head_known`
/// distinguishes "gh answered, but this pane is a stranger to the roster" from
/// "gh could not tell us the head ref at all" — different faults, different
/// things for the agent to do about them.
pub fn gh_close_unverifiable_why(head_known: bool) -> &'static str {
    if head_known {
        "orrerix could not identify which agent this pane is, so it cannot tell whether this PR is \
         yours (the group's owner roster has no row for this pane's agent id)"
    } else {
        "orrerix could not resolve this PR's head branch from gh, so it cannot tell whose branch \
         it is"
    }
}

/// The whole unverifiable refusal, composed from the two above.
pub fn gh_close_unverifiable_refusal(pr: &str, head_known: bool) -> String {
    gh_close_unverifiable_refusal_with(pr, gh_close_unverifiable_why(head_known))
}

/// What a `git push` publishes with respect to tags (#83). Local `git tag` is
/// harmless — only the PUSH reaches the world — so the git shim gates pushes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitTagPush {
    /// Not a tag push (a branch push, or not `git push` at all): pass through.
    None,
    /// Pushes ALL/annotated tags in bulk (`--tags`/`--follow-tags`/`--mirror`) —
    /// can't be matched to a single tag grant, so block with guidance to push the
    /// one approved tag instead.
    Bulk,
    /// An explicit tag ref (`refs/tags/<t>`, `tag <t>`, or a bare `v*` refspec) →
    /// gate on a release grant for `<t>`.
    Tag(String),
}

/// Classify a `git` argv for tag-push gating (#83). Pure over the args (git global
/// options like `-C <dir>` / `-c <k=v>` are skipped to find the `push` command).
/// A bare refspec is treated as a tag only when it matches the release pattern
/// (`v*` — the `release.yml` `on.push.tags` trigger; these MUST stay in sync);
/// the shim confirms ambiguous cases against the real git, but the classification
/// here is the testable spec.
pub fn git_tag_push(args: &[String]) -> GitTagPush {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    // Locate the git subcommand, skipping value-taking globals.
    let mut i = 0;
    let mut cmd: Option<&str> = None;
    while i < a.len() {
        let t = a[i];
        if matches!(t, "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path") {
            i += 2;
            continue;
        }
        if t.starts_with('-') {
            i += 1;
            continue;
        }
        cmd = Some(t);
        break;
    }
    if cmd != Some("push") {
        return GitTagPush::None;
    }
    let rest = &a[i + 1..];
    // Bulk tag pushes.
    if rest.iter().any(|t| matches!(*t, "--tags" | "--follow-tags" | "--mirror")) {
        return GitTagPush::Bulk;
    }
    // Positional refspecs after `push`: the first non-flag is the remote; the rest
    // are refspecs. Also handle the `git push <remote> tag <name>` form.
    let mut positionals = rest.iter().filter(|t| !t.starts_with('-'));
    let _remote = positionals.next();
    let mut prev_tag_kw = false;
    for spec in positionals {
        if *spec == "tag" {
            prev_tag_kw = true;
            continue;
        }
        if prev_tag_kw {
            return GitTagPush::Tag(grant_segment(spec));
        }
        // `src:dst` — the destination ref is what lands on the remote.
        let dst = spec.rsplit(':').next().unwrap_or(spec);
        if let Some(t) = dst.strip_prefix("refs/tags/") {
            return GitTagPush::Tag(grant_segment(t));
        }
        // A bare refspec matching the RELEASE TRIGGER pattern. This MUST track
        // `.github/workflows/release.yml`'s `on.push.tags` (currently `v*` — ANY
        // ref starting with `v`, not just `v<digit>`), or a `vbeta`/`vRelease` tag
        // push would publish to the world yet slip the gate (rev-86). It's only a
        // *candidate* — the shim confirms it's actually a tag (not a same-prefixed
        // branch) against the real git before gating, so a branch like `vfeature`
        // still passes.
        let name = dst.trim_start_matches('+');
        if name.starts_with('v') {
            return GitTagPush::Tag(grant_segment(name));
        }
    }
    GitTagPush::None
}

/// Whether an unexpired grant authorizes an action right now: a grant exists and
/// its expiry (unix seconds) is in the future. Pure so the TTL rule is testable;
/// the shim reads the expiry from the grant file. `None` = no grant file.
pub fn grant_unexpired(expires_secs: Option<u64>, now_secs: u64) -> bool {
    matches!(expires_secs, Some(exp) if now_secs < exp)
}

/// Sanitize a grant target (PR ref / tag) into a safe single path segment: keep
/// only `[A-Za-z0-9._-]`, everything else → `_`. Prevents a `/` or `..` in a tag
/// from escaping the grant dir, and MUST match the shim's `tr -c` sanitizer so the
/// backend and shim agree on the grant filename. Pure/testable.
pub fn grant_segment(target: &str) -> String {
    let s: String = target
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    if s.is_empty() { "_".to_string() } else { s }
}

/// One PR the human just granted a one-time merge of, with their optional
/// per-PR note (already trimmed; `None` when they left it empty).
#[derive(Debug, Clone, Copy)]
pub struct GrantedPr<'a> {
    pub num: u64,
    pub note: Option<&'a str>,
}

/// One item the human approved at the merge gate that had **no** PR number to
/// key a grant on — approved and marked done, but nothing was authorized, so
/// the orchestrator has to close it out by hand.
#[derive(Debug, Clone, Copy)]
pub struct PlainApproval<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub note: Option<&'a str>,
}

/// Build the merge-gate notice the orchestrator receives for an approval —
/// the single source of the wording for BOTH a single Approve and a bulk one
/// (#507), so "bulk changes delivery, not authority" holds at the text level
/// too: N grants are minted exactly as before, and this says so once.
///
/// Shapes:
/// - **One PR, no plain approvals** — reproduces the original single-grant
///   notice byte-for-byte (#83), note inline before the instruction. That is
///   what `grant_merge` still delivers, and what a bulk of one delivers, so
///   the two are the same string rather than merely similar ones.
/// - **Several PRs** — one sentence listing every granted PR, then the
///   per-PR notes on their own trailing lines (there can be several, so they
///   cannot stay inline).
/// - **Plain approvals** — appended as their own sentence, so items that got
///   no grant are never silently folded into a list of granted PRs.
///
/// Pure (no registry, no I/O) so every shape is directly testable.
pub fn merge_grant_notice(
    granted: &[GrantedPr<'_>],
    plain: &[PlainApproval<'_>],
    mins: u64,
) -> String {
    // The legacy single-grant layout puts the note between the grant sentence
    // and the instruction. That only reads correctly when there is exactly one
    // thing to say a note about.
    let inline_note = granted.len() == 1 && plain.is_empty();
    let list = granted.iter().map(|g| format!("#{}", g.num)).collect::<Vec<_>>().join(", ");
    let mut lines: Vec<String> = Vec::new();

    if let Some(one) = granted.first().filter(|_| granted.len() == 1) {
        let head = format!(
            "[orrerix] the human GRANTED a one-time merge of PR #{} (valid ~{mins} min).",
            one.num
        );
        let tail =
            format!("You may now merge THAT PR once (only #{}); report when done.", one.num);
        lines.push(match one.note.filter(|_| inline_note) {
            Some(c) => format!("{head} Note from the human: {c}\n{tail}"),
            None => format!("{head} {tail}"),
        });
    } else if !granted.is_empty() {
        lines.push(format!(
            "[orrerix] the human GRANTED one-time merges of PRs {list} (valid ~{mins} min each). \
             You may now merge EACH of THOSE PRs once (only {list}), one grant per PR; report when done."
        ));
    }

    if !plain.is_empty() {
        let items = plain
            .iter()
            .map(|p| format!("{} \"{}\"", p.id, p.title))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(if granted.is_empty() {
            // "no PR number could be resolved" rather than "no PR is linked":
            // a task CAN carry a `pr` field that yields no number (a URL typo,
            // a placeholder), and telling the orchestrator nothing is linked
            // when something is would send it looking for the wrong problem.
            format!(
                "[orrerix] the human APPROVED {items} at the merge gate and marked {} done. \
                 No PR number could be resolved for {}, so nothing was authorized — merge and \
                 close out by hand.",
                if plain.len() == 1 { "it" } else { "them" },
                if plain.len() == 1 { "it" } else { "them" }
            )
        } else {
            format!(
                "Also APPROVED at the merge gate, with no PR number to grant — merge and close \
                 out by hand: {items}."
            )
        });
    }

    if !inline_note {
        for g in granted {
            if let Some(c) = g.note {
                lines.push(format!("Note from the human on #{}: {c}", g.num));
            }
        }
        for p in plain {
            if let Some(c) = p.note {
                lines.push(format!("Note from the human on {}: {c}", p.id));
            }
        }
    }

    lines.join("\n")
}
