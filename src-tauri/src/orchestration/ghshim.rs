//! The `gh`, `git` and `orrerix` shims' text: the POSIX bodies, the Windows
//! `.cmd` delegators, the toolchain they bake in, and stale-shim pruning.
//! Design note: `docs/design/shim-path-integrity.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): `crate::winpath`.
//! IO: fs. Sibling files it calls: `ghgate.rs`.

use super::*;

/// The POSIX `gh` shim (#83), with the real gh's absolute path baked in. Mirrors
/// the pure `gh_is_merge_invocation` / `gh_gate_decision` spec in shell: only
/// `gh pr merge` (and cheap `gh api` merge shapes) is gated; a merge onto the
/// default branch is allowed only when both the `autonomous` and `auto_merge`
/// markers are present in the pane's group dir; a non-default base passes through;
/// an undeterminable base fails safe (block). Refusals/allows are appended to the
/// group's `audit.jsonl` in the backend's line format. Everything else `exec`s the
/// real gh with no extra work.
///
/// **Two independent gates live here (#222/#197).** The *human* gate above is one.
/// The other is the repo's own **workflow merge gate**: when `.loomux/workflow.yml`
/// declares `gates.merge`, loomux writes a `merge_gate` spec file into the group
/// dir and the shim refuses the merge until the named reviewers' recorded verdicts
/// (`verdicts/pr-<N>/<block>`, written by the `review_verdict` MCP tool) satisfy it.
/// It is checked **first**, so no grant and no autonomous marker can open it — the
/// `workflow::evaluate_merge_gate` spec is the pure mirror of that decision. With
/// no `merge_gate` file the shim behaves exactly as it did before #222.
/// The machine-derived paths baked into the POSIX shims at write time (#509).
///
/// Both are **derived** from this machine's own Git for Windows install, never
/// hardcoded (CLAUDE.md constraint 8 / #263): `utils_dir` from
/// `winpath::resolve_utils_dir`, `git_dir` from the resolved real `git`. Both
/// are in MSYS form (`/c/Program Files/…`, see `winpath::to_msys_dir`) because
/// that is the only form an MSYS `sh` can resolve inside `$PATH`.
///
/// `None` means shim-write time could not find it. That is never fatal and
/// never silent: a missing `utils_dir` leaves the shim's own dependency
/// self-check to refuse every gated command loudly (fail CLOSED), and a missing
/// `git_dir` just leaves the real gh's git plumbing where it was.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ShimPaths {
    /// Directory holding the POSIX coreutils the gate normalizes with.
    pub utils_dir: Option<String>,
    /// Directory holding the real `git.exe` (gh shim only — see the `.cmd`
    /// re-parse note in `shim_cmd_delegator`).
    pub git_dir: Option<String>,
}

/// Escape a machine-derived path for interpolation inside shell **double**
/// quotes (#509 rev-21 N4).
///
/// `"$dir"` still expands `$`, a backtick and `\` — so a Git install under a
/// directory containing one of those would execute at shim runtime rather than
/// name a directory. Not attacker-controllable and it fails closed (the
/// dependency self-check then refuses), but the shims are careful about exactly
/// this class everywhere else and a security script should not have a
/// "probably fine" interpolation in it. Backslash first, or it would re-escape
/// the escapes it just added.
fn sh_dq_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('$', "\\$")
        .replace('`', "\\`")
        .replace('"', "\\\"")
}

/// The dependency preamble both POSIX shims open with (#509): repair `PATH` so
/// the coreutils the gate normalizes with resolve, then **prove** they do and
/// fail CLOSED if they do not.
///
/// Emitted byte-identically into the `gh` and `git` shims (pinned by
/// `gh_and_git_shim_deps_preamble_stays_byte_identical`) — one gate-integrity
/// guarantee, not two copies that can drift. It is deliberately generic about
/// which shim it is running in: the two differ in which utilities they happen
/// to use today (the git shim never calls `cat`/`tail`), and a per-shim list
/// would be a standing invitation to "this one doesn't need that any more"
/// edits that quietly narrow the check. Git for Windows ships all of them in
/// one `usr\bin`, so the strict list costs nothing and cannot drift.
fn shim_deps_preamble(utils_dir: Option<&str>) -> String {
    // `sh.exe` is launched by ABSOLUTE path from the `.cmd` delegator (#335),
    // so it inherits the CALLER's PATH — and a PowerShell/cmd pane's PATH does
    // not carry Git for Windows' `usr\bin`.
    let repair = match utils_dir {
        // rev-21 N4: the path lands inside shell DOUBLE quotes, where `$`, a
        // backtick and `\` still have meaning. It is machine-derived, not
        // attacker-controlled, and a mangled value fails closed (the self-check
        // below then refuses) — but this file is careful about exactly this
        // class everywhere else, so it should be careful here too.
        Some(dir) => format!(
            "ORX_UTILS=\"{dir}\"\n\
             case \":$PATH:\" in\n\
             \x20 *\":$ORX_UTILS:\"*) ;;\n\
             \x20 # An EMPTY inherited PATH would leave a TRAILING colon, and a trailing empty\n\
             \x20 # entry means the CURRENT DIRECTORY — not something a security shim should be\n\
             \x20 # the one to add. Set, don't prepend, in that case.\n\
             \x20 \"::\") PATH=\"$ORX_UTILS\"; export PATH ;;\n\
             \x20 *) PATH=\"$ORX_UTILS:$PATH\"; export PATH ;;\n\
             esac\n",
            dir = sh_dq_escape(dir),
        ),
        // Nothing resolved at write time: no repair to make, but the assertion
        // below still runs — a shim that cannot normalize must refuse, not guess.
        None => String::new(),
    };
    format!(
        "# ── The gate's own toolchain (#509) ──────────────────────────────────────────\n\
         # Before #509 a missing `tr` was SILENT: `x=$(printf … | tr …)` printed\n\
         # \"tr: command not found\" and set `x` to the EMPTY string, and empty is not a\n\
         # safe default here. An empty `path_low`/`low` matched none of the `gh api`\n\
         # release/merge arms, so `gh api -X DELETE …/git/refs/tags/v1.2.3` and a\n\
         # graphql `mergePullRequest` — both blocked under Git Bash — sailed straight\n\
         # THROUGH the gate from a PowerShell pane. So: prepend the coreutils dir\n\
         # resolved from this machine's own `sh` install at shim-write time (derived,\n\
         # never hardcoded), in the MSYS `/c/…` form `sh` can actually resolve.\n\
         {repair}\
         # …and PROVE it worked, before one line of gate logic runs. A gate that\n\
         # normalizes with tools it does not have is not a gate, and the pre-#509\n\
         # failure was silent — the one thing a gate must never be. Fail CLOSED and\n\
         # loudly instead. `command -v` is a shell builtin, so this check cannot\n\
         # itself be defeated by the very PATH problem it is testing for.\n\
         for _dep in tr head tail date cat rm mv; do\n\
         \x20 command -v \"$_dep\" >/dev/null 2>&1 && continue\n\
         \x20 printf '%s\\n' \"orrerix: the merge/release gate shim cannot find the POSIX tool '$_dep' on PATH, so it cannot normalize the values it gates on. Refusing this command outright rather than running it through a gate that would silently skip those checks (#509). This means orrerix could not locate Git for Windows' coreutils (usr/bin) when it wrote the shim: repair or reinstall Git for Windows, then open a new pane.\" >&2\n\
         \x20 loomux_audit \"gate-degraded-missing-dep\" \"{{\\\"dep\\\":\\\"$_dep\\\"}}\"\n\
         \x20 exit 1\n\
         done\n\
         unset _dep\n\
         # THE INVARIANT (#509 rev-21 N2). The check above makes a failed normalizer\n\
         # improbable; this makes it IMPOSSIBLE. A normalizer that returns EMPTY for a\n\
         # NON-empty input has failed, and empty is precisely the value that matched no\n\
         # gate arm and let a tag deletion through. So the gate never consumes one: it\n\
         # refuses. This catches what a startup probe structurally cannot — a `tr` that\n\
         # RESOLVES but cannot run (broken install, arch mismatch, fork failure)\n\
         # reproduces #509 exactly, and `command -v` says it is fine. Checked at the\n\
         # point of use, so it costs nothing per invocation.\n\
         # $1=raw input $2=normalized output $3=field name for the audit\n\
         loomux_norm_guard() {{\n\
         \x20 [ -n \"$1\" ] && [ -z \"$2\" ] || return 0\n\
         \x20 printf '%s\\n' \"orrerix: the merge/release gate could not normalize $3 — the POSIX tool that folds its case resolved but produced nothing, so every gate pattern below would match against an empty string and let this command through. Refusing it instead (#509). Check that Git for Windows' coreutils are intact.\" >&2\n\
         \x20 loomux_audit \"gate-degraded-normalize-failed\" \"{{\\\"field\\\":\\\"$3\\\"}}\"\n\
         \x20 exit 1\n\
         }}\n"
    )
}

/// The release-grant validity check, embedded **byte-identically** into both the
/// `gh` shim and the `git` shim (two separately generated scripts with no shared
/// shell library) — by construction from this one const, not by two copies that
/// happen to agree today. It is the single place that decides what a release
/// grant is worth, so a one-sided edit is not expressible.
///
/// **A release grant is a PIPELINE grant, not a one-time token (#438).** When the
/// human authorizes "release vX.Y.Z" they are authorizing the steps that release
/// actually takes — the `vX.Y.Z` tag push, `gh release create|edit vX.Y.Z`, and
/// the release-notes write against that tag's release. Burning the grant on the
/// first of those and refusing the rest is the bug: the live v1.1.0-beta6 cut
/// spent its grant on the tag push and then had to ask the human a second time
/// for the notes PATCH, which was part of the same release the human had already
/// said yes to (and v1.0.0 needed three touches for one release).
///
/// **What still bounds it** — and this is the whole security argument, since the
/// grant is no longer self-limiting by use count:
/// - **Tag identity.** The grant file is keyed by tag segment, so it authorizes
///   `vX.Y.Z` and nothing else. Another tag, and another release (an id-addressed
///   call resolves to ITS OWN tag before this check — a `make_latest` flip on some
///   other release resolves to that release's tag and finds no grant), are refused
///   exactly as before.
/// - **The TTL.** Every step re-reads line 1 and re-compares it to the clock, so
///   the window is a hard wall, not a first-use wall. One second past expiry the
///   next step is refused and the file is deleted.
/// - **Default-deny.** No grant file, an unparseable expiry, or a clock we cannot
///   read at all → refuse.
///
/// The clock check is deliberately **stricter** than `loomux_grant_claim`'s (which
/// treats an unreadable `date` as "not yet expired"): a token spent once can
/// tolerate that, a grant that stays live until a timestamp cannot — an
/// unevaluable window is an unbounded one.
///
/// This is also why the release path has **no claim/settle** (the merge gate keeps
/// it): that machinery exists to stop a one-time grant being double-spent and to
/// hand it back when the step it authorized failed. With nothing spent there is
/// nothing to double-spend and nothing to restore — and #303/#315 ("a publish or
/// tag push GitHub refuses must not burn the human's grant") now hold by
/// construction rather than by remembering to call settle.
const RELEASE_GRANT_VALID_SH: &str = r#"loomux_release_grant_valid() { # $1=grantfile
  [ -f "$1" ] || return 1
  exp=$(head -n1 "$1" 2>/dev/null)
  case "$exp" in ''|*[!0-9]*) exp=0 ;; esac
  now=$(date +%s 2>/dev/null)
  case "$now" in ''|*[!0-9]*) return 1 ;; esac
  if [ "$now" -ge "$exp" ]; then
    rm -f "$1"
    return 1
  fi
  return 0
}
"#;

/// The shim's shell positional scanner's two value-flag `case` arms, **built
/// from [`GH_VALUE_FLAGS`] itself** (#2985 rev-std finding 1).
///
/// **This exists because the hand-maintained copy diverged, exactly as its own
/// doc warned it could.** `GH_VALUE_FLAGS` said "keep this in sync with the
/// shim's shell scanner value-flag list", the `-c`/`--comment` entry was added
/// to the const and not to the shell, and the shim then read the comment's value
/// as the PR selector: `gh pr close -c "7" 2942` made the close gate resolve and
/// authorize PR **7** while the real gh closed PR **2942** — a gate deciding
/// about a different PR from the one being closed, which is fail-OPEN whenever
/// the caller happens to own the PR it named in the comment. The sibling failure
/// is a wrong refusal (`--comment "why" 2942` resolves nothing and is refused as
/// unverifiable).
///
/// A comment telling the next editor to update two lists is not a mechanism. So
/// the shell arms are now GENERATED from the one const, and the divergence is not
/// expressible — the same construction this shim already uses for the release
/// grant check and for the close gate's refusal sentences.
///
/// `-R`/`--repo` are excluded: they have their own arms above these (they
/// capture the value rather than discarding it), and a `case` takes its first
/// matching arm, so listing them here would be dead text implying a behaviour it
/// does not have.
///
/// Returns `(separate-token arm, glued `--flag=value` arm)`. Short glued forms
/// (`-b"x"`) are deliberately NOT generated: the pre-#2985 scanner did not handle
/// them either, and inventing that arm here would be a behaviour change riding
/// in on a bug fix. `-R?*` stays hand-written above for the same reason it always
/// was — it captures, it does not skip.
fn gh_shim_value_flag_arms() -> (String, String) {
    let sep: Vec<&str> = GH_VALUE_FLAGS
        .iter()
        .copied()
        .filter(|f| *f != "-R" && *f != "--repo")
        .collect();
    let glued: Vec<String> = sep
        .iter()
        .filter(|f| f.starts_with("--"))
        .map(|f| format!("{f}=*"))
        .collect();
    (sep.join("|"), glued.join("|"))
}

/// The `gh pr close` / `gh pr reopen` ownership gate, as shell (#2985) — the
/// mirror of [`gh_close_decision`], and the one place `gh_shim_sh` gets it from.
///
/// **Every sentence it prints is generated from the Rust builders, not retyped
/// here.** The refusal templates are produced by calling
/// [`gh_close_refusal_with`] / [`gh_close_unverifiable_refusal_with`] with the
/// SHELL's own variable names as their arguments, so the string that ships in
/// the script is literally the string Rust builds, with `$c_pr` where the PR
/// number goes. That is the same construction `RELEASE_GRANT_VALID_SH` uses:
/// two programs, one guarantee, and a one-sided edit that is not expressible.
/// `the_close_refusal_the_shim_prints_is_the_one_rust_builds` executes the real
/// generated script and compares its stderr against the Rust function, so the
/// interpolation itself is pinned too, not just the template.
///
/// **Why this is a separate gate rather than another arm of the merge gate.**
/// The merge gate asks *may this land on the default branch* — a question about
/// the repo. This one asks *whose PR is this* — a question about the group's
/// roster. They share the audit helper and the fail-closed posture and nothing
/// else; folding them together would mean a merge marker (`autonomous`,
/// `auto_merge`, a grant) could open the close path, which is exactly the
/// widening #2985 is about.
fn gh_shim_close_gate() -> String {
    const TPL: &str = r#"# ── THE PR-CLOSE OWNERSHIP GATE (#2985) ──────────────────────────────────────
# A live incident: a worker cleaning up its own scratch PRs ran a loop over
# COMPUTED pr numbers and closed five other workers' open PRs in six seconds.
# Every review drive on them was cancelled; the audit log had no row for any of
# it, because the shim logged merges and not closes; and since `gh` runs under
# the human's token, the GitHub timeline could not tell the human's own close
# from an agent's. Nothing here is about intent — the loop was a typo — so the
# guard is structural: a close is refused unless the caller can be shown to own
# the branch, and every close (allowed or refused) is a row in audit.jsonl with
# the calling agent's id in it.
#
# NOT in scope, deliberately: `gh pr merge --delete-branch`. That is the
# orchestrator's documented post-merge step (CLAUDE.md: "whoever performs the
# merge owns this step") and it already sits behind the merge gate below;
# adding a second condition to it would refuse the one branch delete this repo
# mandates.
if [ "$cmd" = "pr" ] && { [ "$sub" = "close" ] || [ "$sub" = "reopen" ]; }; then
  # No globbing: `$c_rf`/`$sel` are word-split unquoted into the lookup below,
  # exactly as the merge gate's `$rf` is, and a security shim should not leave
  # the next reader working out whether a `*` could reach a filename.
  set -f
  c_del=0
  for c_tok in "$@"; do
    case "$c_tok" in --delete-branch|-d) c_del=1 ;; esac
  done
  # A close this app cannot AUDIT is a close it cannot allow — the same argument
  # (and the same shape) as the merge gate's missing-group-dir refusal below.
  # Reaching the shim with neither group-dir spelling set means they were unset
  # on the way, which is evasion rather than a supported flow.
  if [ -z "$ORX_GD" ]; then
    printf '%s\n' "orrerix: refusing this gh pr $sub — neither ORRERIX_GROUP_DIR nor LOOMUX_GROUP_DIR is set, so this app cannot tell whose PR this is and cannot record who closed it. Run gh from your agent pane's normal environment; do NOT unset them." >&2
    exit 1
  fi
  # Resolve the PR's HEAD branch and number via the REAL gh (one call), honoring
  # the SAME -R/--repo the caller passed — the merge gate resolves its base the
  # same way and for the same reason (rev-79 F2): the answer must be about the
  # repo the caller targeted, not the cwd's.
  c_rf=""
  [ -n "$repo" ] && c_rf="-R $repo"
  c_info=$("$REAL_GH" pr view $c_rf $sel --json headRefName,number --jq '.headRefName+" "+(.number|tostring)' 2>/dev/null)
  c_head=${c_info%% *}
  c_num=${c_info##* }
  # What the message calls this PR. The RESOLVED number when gh gave us one —
  # never the raw selector, which on the incident's own path was a wrong number
  # produced by string concatenation, and echoing it back would confirm the
  # agent's mistaken belief about which PR it was touching.
  c_pr="$c_num"
  [ -n "$c_pr" ] || c_pr="$sel"
  # THE CALLER'S OWN ROW, from the roster the backend projects out of
  # agents.json (`render_owner_roster`): `<agent-id> <role> <branch>`, branch
  # empty for a role that has none. A `while read` fed by a REDIRECT, not a
  # pipe, so the values survive the loop; `|| [ -n "$o_id" ]` for the same
  # reason the gate parser has it — POSIX `read` returns non-zero at EOF, so a
  # final line with no trailing newline would otherwise be dropped, and a
  # dropped row here is an agent the gate cannot identify.
  c_role=""; c_branch=""
  if [ -n "$ORX_AID" ] && [ -f "$ORX_GD/__OWNERS__" ]; then
    while read -r o_id o_role o_branch || [ -n "$o_id" ]; do
      [ "$o_id" = "$ORX_AID" ] || continue
      c_role="$o_role"; c_branch="$o_branch"
    done < "$ORX_GD/__OWNERS__"
  fi
  # THE PR'S OWNER, by name (#2985 rev-std finding 4, and issue #2985's own
  # words: "refuse with the PR's owner named"). A second pass over the same
  # roster, asking which agent's branch owns THIS head by the same rule the
  # gate decides with. Empty when no row owns it — a branch whose agent is
  # gone, or the human's own branch — and the message then says so rather
  # than naming a guess.
  #
  # Several rows can match one head: the descendant rule accepts a head that
  # is a separated descendant of ANY roster branch, so `fix/team` and
  # `fix/team-alpha` both match `fix/team-alpha-2` (#3206). The owner to NAME
  # is the agent whose branch is the closest thing to the head — the longest
  # match — so the scan runs to the end and keeps the longest match it saw
  # instead of taking the first row that matches. Exact and descendant arms
  # score the branch's own length: an exact match's length equals the head's,
  # which is maximal, so it beats every descendant of the same branch.
  # "Closest", not "true owner": the roster cannot know who actually pushed a
  # descendant branch — the human can push one beneath another row's prefix —
  # so this names the closest match the roster HAS, never a git-derived fact.
  # (Decision logic is untouched — this picks the NAME the refusal carries,
  # and the gate's own-ownership test below is the caller's single row.)
  c_owner=""; c_owner_len=-1
  if [ -n "$c_head" ] && [ -f "$ORX_GD/__OWNERS__" ]; then
    while read -r o_id o_role o_branch || [ -n "$o_id" ]; do
      [ -n "$o_branch" ] || continue
      o_hit=0
      [ "$c_head" = "$o_branch" ] && o_hit=1
      case "$c_head" in
        "$o_branch"/*|"$o_branch"-*) o_hit=1 ;;
      esac
      # Longest matching branch wins (#3206): do not stop at the first row —
      # a longer match later in the roster names a different agent.
      if [ "$o_hit" = "1" ]; then
        o_len=${#o_branch}
        [ "$o_len" -gt "$c_owner_len" ] && { c_owner="$o_id"; c_owner_len=$o_len; }
      fi
    done < "$ORX_GD/__OWNERS__"
  fi
  # AUDIT-SAFE COPIES. A git ref name may contain a `"`, and every value below
  # is interpolated into a JSON line in audit.jsonl. Unescaped, a branch named
  # `x","agent":"o-1` does not merely corrupt the row, it FORGES a field — and
  # attribution is this gate's whole second half, so a forgeable audit row
  # would defeat the half that exists to stop the next orchestrator having to
  # ask the human. Deleting the quote (rather than backslash-escaping it) is
  # the choice that cannot itself produce a trailing escape, and `\` goes with
  # it for the same reason. These copies are for the AUDIT only: the ownership
  # comparison and the refusal text keep the real values, so nothing about the
  # DECISION changes here.
  a_head=$(printf '%s' "$c_head" | tr -d '"\\')
  a_branch=$(printf '%s' "$c_branch" | tr -d '"\\')
  a_role=$(printf '%s' "$c_role" | tr -d '"\\')
  a_pr=$(printf '%s' "$c_pr" | tr -d '"\\')
  a_owner=$(printf '%s' "$c_owner" | tr -d '"\\')
  # A REOPEN destroys nothing — and the incident's own remediation was a reopen
  # loop — so it is never refused. It is AUDITED, which is the half that was
  # missing: the next orchestrator reads who reopened what instead of asking
  # the human. Audited before the ownership work below, which a reopen does not
  # need to do.
  if [ "$sub" = "reopen" ]; then
    loomux_audit "pr-reopen" "{\"agent\":\"$ORX_AID\",\"role\":\"$a_role\",\"pr\":\"$a_pr\",\"head\":\"$a_head\"}"
    exec "$REAL_GH" "$@"
  fi
  # The ORCHESTRATOR may close any PR in its group: its authority is over the
  # group, not over a branch, so this is settled before the head ref is even
  # needed. Still audited — "allowed" is a record here, not a silence.
  if [ "$c_role" = "orchestrator" ]; then
    loomux_audit "pr-close-allowed" "{\"agent\":\"$ORX_AID\",\"role\":\"orchestrator\",\"pr\":\"$a_pr\",\"head\":\"$a_head\",\"delete_branch\":$c_del}"
    exec "$REAL_GH" "$@"
  fi
  # An unknown caller (no agent id, or no roster row for it) and an unresolvable
  # head ref are the SAME epistemic state — this app cannot say whose PR this is
  # — and that is never "probably fine". Fail closed, and say which half is
  # missing so a real infrastructure fault does not read as a policy decision.
  if [ -z "$c_role" ] || [ -z "$c_head" ]; then
    if [ -n "$c_head" ]; then c_why="__WHY_NO_AGENT__"; else c_why="__WHY_NO_HEAD__"; fi
    printf '%s\n' "__UNVERIFIABLE__" >&2
    loomux_audit "pr-close-blocked" "{\"agent\":\"$ORX_AID\",\"reason\":\"unverifiable\",\"pr\":\"$a_pr\",\"head\":\"$a_head\",\"delete_branch\":$c_del}"
    exit 1
  fi
  # OWNERSHIP: the head branch is this agent's own branch, or a scratch branch
  # BENEATH it under a `/` or `-` separator. The separator is the whole point —
  # a bare prefix test would make `fix/29` the owner of `fix/2985-x`, which is
  # another worker's branch, and refusing five other workers' PRs is the entire
  # reason this gate exists. An EMPTY $c_branch owns nothing: a role with no
  # branch of its own must not be handed every branch in the repo by an
  # empty-prefix match. (`gh_branch_is_owned` is the Rust mirror.)
  c_own=0
  if [ -n "$c_branch" ]; then
    if [ "$c_head" = "$c_branch" ]; then
      c_own=1
    else
      case "$c_head" in
        "$c_branch"/*|"$c_branch"-*) c_own=1 ;;
      esac
    fi
  fi
  if [ "$c_own" = "1" ]; then
    loomux_audit "pr-close-allowed" "{\"agent\":\"$ORX_AID\",\"role\":\"$a_role\",\"pr\":\"$a_pr\",\"head\":\"$a_head\",\"delete_branch\":$c_del}"
    exec "$REAL_GH" "$@"
  fi
  if [ -n "$c_branch" ]; then c_own_clause="__OWN_SOME__"; else c_own_clause="__OWN_NONE__"; fi
  if [ -n "$c_owner" ]; then c_owner_clause="__OWNER_SOME__"; else c_owner_clause="__OWNER_NONE__"; fi
  if [ "$c_del" = "1" ]; then c_del_clause="__DEL_YES__"; else c_del_clause=""; fi
  printf '%s\n' "__REFUSAL__" >&2
  loomux_audit "pr-close-blocked" "{\"agent\":\"$ORX_AID\",\"role\":\"$a_role\",\"reason\":\"not-owner\",\"pr\":\"$a_pr\",\"head\":\"$a_head\",\"own\":\"$a_branch\",\"owner\":\"$a_owner\",\"delete_branch\":$c_del}"
  exit 1
fi
"#;
    TPL.replace("__OWNERS__", OWNER_ROSTER_FILE)
        .replace("__WHY_NO_AGENT__", gh_close_unverifiable_why(true))
        .replace("__WHY_NO_HEAD__", gh_close_unverifiable_why(false))
        .replace(
            "__UNVERIFIABLE__",
            &gh_close_unverifiable_refusal_with("$c_pr", "$c_why"),
        )
        .replace("__OWN_SOME__", &gh_close_own_clause("$c_branch"))
        .replace("__OWN_NONE__", &gh_close_own_clause(""))
        .replace("__OWNER_SOME__", &gh_close_owner_clause("$c_owner"))
        .replace("__OWNER_NONE__", &gh_close_owner_clause(""))
        .replace("__DEL_YES__", gh_close_del_clause(true))
        .replace(
            "__REFUSAL__",
            &gh_close_refusal_with(
                "$c_pr",
                "$c_head",
                "$c_own_clause",
                "$c_owner_clause",
                "$c_del_clause",
            ),
        )
}

#[doc(hidden)] // pub so the integration test can pin the security-critical guards
pub fn gh_shim_sh(real_gh: &str, paths: &ShimPaths) -> String {
    // Template uses a placeholder (not format!) so the shell's own `$`/`{}` stay
    // literal. The ts value shells out to the system `date`, never a Rust crate
    // (no getrandom, constraint 2) — but `%N` is a GNU coreutils EXTENSION, not
    // POSIX: BSD `date` (macOS) prints a literal `3N` tail (#3202), so every
    // ts site in the template goes through the portable 13-digit fallback the
    // self-launch shim already uses (#3249: an all-digit check alone is
    // magnitude-blind).
    const TPL: &str = r#"#!/bin/sh
# orrerix gh shim (#83) — enforce the human merge gate. Generated by orrerix; do not edit.
REAL_GH="__REAL_GH__"
# #1153 phase 3: the pane exports both spellings during the transition
# (`agent_pane_env`). Resolved once, here, so every read below is one
# variable that cannot disagree with another read further down.
ORX_GD="${ORRERIX_GROUP_DIR:-$LOOMUX_GROUP_DIR}"
ORX_AID="${ORRERIX_AGENT_ID:-$LOOMUX_AGENT_ID}"

loomux_audit() { # $1=action $2=detail-json
  # Same portable form as the self-launch shim (#3202):
  # BSD date (macOS) has no %N, and does not fail on it: `+%s%3N` returns the
  # epoch with a literal `3N` glued on, which lands in ts_ms and makes the whole
  # line unparseable JSON. Emptiness is not the only bad answer — and neither
  # is magnitude: a date that answers %s%3N with plain SECONDS (all-digit, 10
  # digits) passes an all-digit check a thousandfold too small (#3249), so take
  # a 13-digit all-digit result or nothing and refuse every other all-digit
  # magnitude outright (ts=0) — a value that already misbehaved is not
  # re-consulted; the whole-seconds rung answers only a non-digit or empty
  # %s%3N, then to 0 — and the whole-seconds answer must itself carry the
  # right magnitude: exactly a 10-digit epoch-second value becomes ts+000;
  # every other all-digit magnitude is the same lie one rung lower and is
  # refused outright (ts=0) (#3249). Neither accept arm takes a leading
  # zero: a zero-padded answer interpolated bare (`"ts_ms":0170000000`) is
  # a leading-zero literal and no JSON parser accepts it (#3259) — so
  # every accept arm requires a non-zero leading digit. And each arm
  # spells its WHOLE accept shape (`[1-9]` then digit classes, every
  # position), so no ACCEPT arm depends on the junk arm running before
  # it (#3259); the catch-all `*)` must stay LAST — above the 13-digit
  # accept arm it would refuse every good answer to ts=0, and only the
  # happy-path pin would notice.
  ts=$(date +%s%3N 2>/dev/null)
  case "$ts" in
    *[!0-9]*|"")
      ts=$(date +%s 2>/dev/null)
      case "$ts" in
        *[!0-9]*|"") ts=0 ;;
        [1-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]) ts="${ts}000" ;;
        *) ts=0 ;;
      esac ;;
    [1-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]) ;;
    *) ts=0 ;;
  esac
  if [ -n "$ORX_GD" ]; then
    # ONE printf of the whole line (record + \n) — O_APPEND is atomic per write,
    # and the backend can't lock us out of another process. Splitting this across
    # two printfs/redirections would let a concurrent writer splice the record
    # (#240). Keep it a single append.
    printf '{"ts_ms":%s,"actor":"gh-shim","action":"%s","detail":%s}\n' "$ts" "$1" "$2" \
      >> "$ORX_GD/audit.jsonl" 2>/dev/null || true
  fi
}
__DEPS_PREAMBLE__
loomux_block() { # $1=reason $2=base $3=pr
  printf '%s\n' "orrerix: merge to the default branch requires the human gate — enable auto-merge (autonomous mode) or have the human grant this one merge (board Approve). Open the PR and report to the human; do NOT merge." >&2
  loomux_audit "merge-gate-blocked" "{\"reason\":\"$1\",\"base\":\"$2\",\"pr\":\"$3\"}"
  exit 1
}
# The WORKFLOW merge gate (#222/#197), distinct from the human gate above: this is
# the repo's own `gates.merge` clause, and it is an ADDITIONAL necessary condition —
# a human grant, autonomous auto-merge and supervised dangerous mode all sit BELOW
# it and none of them can open it. $1=reason (audit) $2=human-readable detail.
loomux_block_wf() { # $1=reason $2=detail
  printf '%s\n' "orrerix: this repo's workflow.yml declares a merge gate on PR #$num and it is NOT satisfied — $2. The merge is refused. Reviewers record their outcome with the review_verdict MCP tool (pass | fail | escalate); a fail/escalate from ANY named reviewer refuses the merge whatever the others said. Wait for the reviews, or take it to the human — do NOT work around this. Three ways forward: (1) get the named reviewer(s) to run and record a verdict, (2) have the human turn workflow mode off for this session (clears the gate), or (3) merge this PR from the GitHub UI, which is not gated." >&2
  loomux_audit "merge-gate-workflow-blocked" "{\"reason\":\"$1\",\"pr\":\"$num\"}"
  exit 1
}
loomux_block_release() { # $1=tag $2=action $3=conflicting caller-supplied tag (optional)
  if [ -n "$3" ]; then
    # rev B1: the URL names one release and the body names a different tag. Never
    # matched against a grant — a grant authorizes ONE tag and this call names two.
    printf '%s\n' "orrerix: refusing this release call — the release it addresses is tagged '$1', but the call's own tag_name/ref field says '$3'. orrerix takes an id-addressed release's identity from the id in the URL, never from a tag field the caller supplied, so this cannot be matched to a grant: a grant authorizes one tag and this names two. If you meant to edit the '$1' release, drop the tag_name/ref field. If you meant to RETAG it to '$3', that publishes a tag nobody has authorized — ask the human to grant '$3' first; do NOT publish." >&2
  elif [ -n "$1" ]; then
    printf '%s\n' "orrerix: publishing a release/tag ($1) requires an explicit human grant — releases publish to the world (GitHub release + npm), which autonomous mode does NOT authorize. Ask the human to grant the release; do NOT publish." >&2
  else
    # #437: the pre-fix message rendered this case as "release/tag ()" — the empty
    # parens being the only clue that the shim had found no tag to key a grant on,
    # which is a different problem needing a different action from the agent.
    printf '%s\n' "orrerix: this call publishes a release but names no tag orrerix could resolve, so it cannot be matched against a release grant — and a grant authorizes ONE tag, never 'whichever release this turns out to be'. Refusing it rather than guessing. If you addressed a release by numeric id, check that id exists and that gh can read it (orrerix resolves id → tag with one read-only lookup); otherwise address the release by its tag. Then ask the human to grant THAT release; do NOT publish." >&2
  fi
  loomux_audit "release-gate-blocked" "{\"tag\":\"$1\",\"action\":\"$2\"}"
  exit 1
}
# The digest the `also: body-unchanged` condition compares (#565): stdin → 64
# lowercase hex on stdout, or NOTHING when this host has no usable sha256 tool —
# and nothing REFUSES at the call site, which is why this may return empty at all.
#
# Deliberately NOT added to the dependency preamble's proven list. That list is
# emitted byte-identically into the git shim too and is asserted before EVERY
# gated command on every host; a hasher is needed only by a condition a repo opts
# into, so requiring it there would refuse merges on hosts that never declare it.
# macOS ships `shasum` and no `sha256sum`; Linux and Git for Windows ship
# `sha256sum`; `openssl dgst -r` is the last resort. All three print `<hex> ?-`,
# so one parameter expansion takes the digest off any of them — and anything that
# is not exactly 64 hex characters (an openssl too old for `-r`, a tool that
# resolved but produced nothing) becomes empty, i.e. refuse.
loomux_sha256() { # stdin → 64 hex chars, or empty
  if command -v sha256sum >/dev/null 2>&1; then _h=$(sha256sum)
  elif command -v shasum >/dev/null 2>&1; then _h=$(shasum -a 256)
  elif command -v openssl >/dev/null 2>&1; then _h=$(openssl dgst -sha256 -r)
  else _h=''
  fi
  _h=${_h%% *}
  [ "${#_h}" -eq 64 ] || _h=''
  case "$_h" in *[!0-9a-f]*) _h='' ;; esac
  printf '%s' "$_h"
}
# Read a verdict file's LINE 5 the way `workflow::parse_verdict_file` does, and
# apply `workflow::sanitize_digest` to what that yields. Sets `v_digest` (the
# lowercased 64-hex digest, or empty when the line does not carry one) and
# `v_mark` (1 when the #2168 E2 `verified-body` mark is present).
#
# **One helper because the alternative is three approximations** (#2308 review 5).
# Every earlier cut tested `${line%% *}` — the first whitespace FIELD — while
# Rust runs `sanitize_digest` over the WHOLE line minus a trailing mark. The two
# disagree on both sides at once, and each direction was reached by a real
# fixture: line 5 reading `<64-hex> is the body digest this pass read` is a
# first field of 64 hex, so the shim ACCEPTED a pass the Rust half refuses (loose,
# on the half that refuses merges); and an UPPERCASE 64-hex digest is refused by a
# `[!0-9a-f]` class while `sanitize_digest` accepts any case and lowercases
# (strict, so a drive cycles to `drive-stalled` instead of merging). Neither is
# reproduction. `the_shim_and_the_gate_agree_about_which_passes_a_verification_covers`
# runs both halves over the same files, including those two.
loomux_verdict_line5() { # $1=verdict file → sets v_digest, v_mark
  v_raw=$(head -n5 "$1" 2>/dev/null | tail -n1)
  v_sp=' '
  v_tb=$(printf '\t')
  v_cr=$(printf '\r')
  # `str::lines()` strips ONE TRAILING `\r` per line and keeps every interior
  # one, so this does too (#2308 round 5, rev-std NB). `tr -d` deleted them all,
  # which could turn a digest with a CR through it into 64 hex for the shim while
  # Rust still read it as unreadable — loose, on the half that refuses merges,
  # the same class as the two findings above. It is also no longer a normalizer:
  # a parameter expansion cannot fail the way a missing coreutil can, so #509's
  # empty-output hazard does not arise and there is nothing here to guard.
  v_l=${v_raw%"$v_cr"}
  v_mark=0
  # `trim_end` before the mark split, exactly as `parse_verdict_file` orders it.
  while :; do
    case "$v_l" in *"$v_sp"|*"$v_tb") v_l=${v_l%?} ;; *) break ;; esac
  done
  case "$v_l" in
    *" verified-body") v_l=${v_l% verified-body}; v_mark=1 ;;
  esac
  # …then `sanitize_digest`, which trims BOTH ends and demands exactly 64 hex.
  # Interior whitespace is deliberately NOT stripped: removing it could turn a
  # 65-character line into 64 hex and accept what Rust refuses, which is the very
  # direction this helper exists to close.
  while :; do
    case "$v_l" in
      "$v_sp"*|"$v_tb"*) v_l=${v_l#?} ;;
      *"$v_sp"|*"$v_tb") v_l=${v_l%?} ;;
      *) break ;;
    esac
  done
  v_digest=''
  case "$v_l" in
    *[!0-9a-fA-F]*) v_l='' ;;
    *) [ ${#v_l} -eq 64 ] || v_l='' ;;
  esac
  if [ -n "$v_l" ]; then
    v_digest=$(printf '%s' "$v_l" | tr 'A-F' 'a-f')
    loomux_norm_guard "$v_l" "$v_digest" "a verdict file's body digest"
  fi
}
# #256: CLAIM a one-time grant without spending it yet — the MERGE gate's grant
# (`merge_grants/pr-<N>`) is the only one-time grant left, and it must be
# consumed only when the real `gh` call it authorizes actually SUCCEEDS (live
# incident: a merge grant burned on a draft PR that GitHub refused to merge,
# leaving the PR unmerged and the human having to re-Approve). Release grants
# used to share this (#303/#315) and no longer do — they are pipeline grants
# now, checked and never spent (see loomux_release_grant_valid), which is why
# those two issues' property holds here by construction rather than by settle.
# An expired grant is still deleted here (never usable, no reason to
# keep it around). A live grant is instead handed off via `mv` to a
# `.claimed` sibling — a RENAME, which POSIX guarantees atomic — so exactly
# one caller can ever win it: two concurrent claimants both see the file
# present, but only one `mv` succeeds (the loser's source is already gone, so
# its `mv` fails and it falls through to `return 1`, same as "no grant").
# Sets `_grant_claimed` to the claimed path on success; the caller runs the
# real gh and finishes with `loomux_grant_settle` below (consume on success,
# restore on failure). If the process dies between claiming and settling, the
# original grant file stays gone and the orphaned `.claimed` file is never
# consulted again — a crash requires a fresh grant rather than risking a
# second use, the failure mode #256 asks for.
loomux_grant_claim() { # $1=grantfile
  gf="$1"
  [ -f "$gf" ] || return 1
  exp=$(head -n1 "$gf" 2>/dev/null)
  case "$exp" in ''|*[!0-9]*) exp=0 ;; esac
  now=$(date +%s 2>/dev/null); [ -z "$now" ] && now=0
  if [ "$now" -ge "$exp" ]; then
    rm -f "$gf"
    return 1
  fi
  claimed="$gf.claimed"
  if mv "$gf" "$claimed" 2>/dev/null; then
    _grant_claimed="$claimed"
    return 0
  fi
  return 1   # lost the race to another concurrent claimant
}
# Finish a claim made by loomux_grant_claim, once the real gh's exit status is
# known: consume it (rm) on success, or restore it to the original grant path
# on failure so a retry can still use it. The merge gate is now its only
# caller. $1=original grantfile $2=claimed file $3=the real gh's exit code
# $4=audit label to emit on restore (optional — the merge gate passes nothing
# and stays silent).
loomux_grant_settle() { # $1=grantfile $2=claimed $3=exit-code $4=restore-audit-label
  if [ "$3" -eq 0 ]; then
    rm -f "$2"   # succeeded — the one-time grant is spent
  else
    mv "$2" "$1" 2>/dev/null   # failed — restore for a retry
    [ -n "$4" ] && loomux_audit "$4" "{\"grant\":\"$1\"}"
  fi
}
# The SINGLE release-gate decision (#83/#196): every release-publishing shape —
# `gh release create|edit|delete` AND the raw `gh api`/graphql equivalents (create
# a v* tag ref, create/edit/delete a release, graphql *Release mutation) — routes
# through here, so the api path can never diverge from the subcommand path. Allowed
# by autonomous+auto_release (blanket, not grant-consumed), supervised dangerous
# mode (human present, not autonomous), or a valid per-tag grant; else fail-safe
# block. $1=tag as resolved from the argv ("" when the argv named none), $2=action
# label, $3=the `repos/…/releases/<id>` path to resolve when the call addresses a
# release by numeric id AND that id could be isolated safely, $4="1" when the URL
# addresses one specific release at all (#437/rev B1 — then $4, not $1, decides
# identity: see below, and note $4=1 with $3="" means "names a release we cannot
# identify", which refuses). Returns 0 to allow; blocks with a message + exit 1
# (never returns) otherwise. A grant-backed allow does NOT consume anything
# (#438) — see loomux_release_grant_valid — so every caller just `exec`s the real
# gh.
__RELEASE_GRANT_VALID__
loomux_release_gate() { # $1=argv tag $2=action $3=path to resolve ("") $4="1" if the URL names one release $5=tag the URL itself carries
  _tag="$1"; _action="$2"; _relpath="$3"; _relid="$4"; _urltag="$5"
  if [ -n "$ORX_GD" ] && [ -f "$ORX_GD/autonomous" ] && [ -f "$ORX_GD/auto_release" ]; then
    loomux_audit "release-gate-allowed" "{\"tag\":\"$_tag\",\"action\":\"$_action\"}"; return 0
  fi
  if [ -n "$ORX_GD" ] && [ -f "$ORX_GD/dangerous_mode" ] && [ ! -f "$ORX_GD/autonomous" ]; then
    loomux_audit "release-gate-dangerous" "{\"tag\":\"$_tag\",\"action\":\"$_action\"}"; return 0
  fi
  # #437: an id-addressed release write (`gh api … repos/O/R/releases/<id>`) carries
  # NO tag anywhere in its argv, so the per-tag grant lookup below had nothing to key
  # on and every such call was refused — including the release-NOTES write, which the
  # release skill MANDATES be addressed by canonical release id precisely so notes
  # can't drift onto a duplicate release the tag also resolves to (#282). Resolve the
  # id to its tag with ONE read-only GET against the REAL gh (never the shim — no
  # recursion), and gate on that tag like any other. Deliberately placed AFTER the
  # blanket openings so a marker-allowed release costs no extra API call.
  #
  # When the path names a specific release ($_relid), the ID is the identity and any
  # tag the argv supplied is DISCARDED before it can key anything (rev B1 — see the
  # api arm's own note). Three outcomes, none of which can fall back to the argv tag:
  #
  #  1. Resolved, and the argv named no tag or the SAME tag → gate on the resolved
  #     tag. This is the release-notes write, the case #437 exists for.
  #  2. Resolved, but the argv named a DIFFERENT tag → refuse outright and say so.
  #     This is either a retag (moving release <id> onto another tag, which
  #     publishes a tag nobody granted) or a decoy trying to borrow this grant for
  #     another release. Both need their own authorization, so neither may proceed
  #     on the strength of a grant for a third tag. Letting the resolved tag simply
  #     win would allow case 2's retag whenever the resolved tag happened to be the
  #     granted one — refusing is the tighter of the two options the review offered.
  #  3. Not resolvable at all → refuse. FAIL-CLOSED is the whole security argument:
  #     a lookup that errors, 404s, prints nothing, prints `null`, prints anything
  #     that is not a plausible ref name, or a path whose id could not be isolated
  #     ($_relpath empty), leaves $_tag EMPTY, which matches no grant. An id loomux
  #     cannot resolve is never "probably fine".
  if [ "$_relid" = "1" ]; then
    _argvtag="$_tag"
    # The URL's own tag segment, when it has one (`…/releases/tags/<tag>`), is
    # already the answer — no lookup, no API call. Otherwise resolve the id.
    _tag="$_urltag"; _src="url"
    if [ -z "$_tag" ] && [ -n "$_relpath" ]; then
      _src="lookup"
      _rawtag=$("$REAL_GH" api "$_relpath" --jq '.tag_name' 2>/dev/null | head -n1)
      _tag=$(printf '%s' "$_rawtag" | tr -d '\r')
      loomux_norm_guard "$_rawtag" "$_tag" "release-id-tag-name"
      case "$_tag" in ''|null|*[!A-Za-z0-9._+/-]*) _tag="" ;; esac
    fi
    if [ -z "$_tag" ]; then
      loomux_audit "release-id-unresolved" "{\"path\":\"$_relpath\",\"action\":\"$_action\"}"
    elif [ -n "$_argvtag" ] && [ "$_argvtag" != "$_tag" ]; then
      loomux_audit "release-id-tag-mismatch" "{\"path\":\"$_relpath\",\"tag\":\"$_tag\",\"claimed\":\"$_argvtag\",\"src\":\"$_src\",\"action\":\"$_action\"}"
      loomux_block_release "$_tag" "$_action" "$_argvtag"
    else
      loomux_audit "release-id-resolved" "{\"path\":\"$_relpath\",\"tag\":\"$_tag\",\"src\":\"$_src\",\"action\":\"$_action\"}"
    fi
  fi
  _safe=$(printf '%s' "$_tag" | tr -c 'A-Za-z0-9._-' '_')
  loomux_norm_guard "$_tag" "$_safe" "release-grant-tag"
  _rg_gf=""
  [ -n "$ORX_GD" ] && [ -n "$_safe" ] && _rg_gf="$ORX_GD/release_grants/$_safe"
  if [ -n "$_rg_gf" ] && loomux_release_grant_valid "$_rg_gf"; then
    loomux_audit "release-gate-granted" "{\"tag\":\"$_tag\",\"action\":\"$_action\"}"; return 0
  fi
  loomux_block_release "$_tag" "$_action"
}

# Parse the argv ONCE (mirrors the Rust gh_positionals / gh_repo_flag spec):
# collect the command (cmd), subcommand (sub), the target selector (sel = 3rd
# positional: a PR ref for `pr merge`, a tag for `release …`), and the -R/--repo
# value — skipping flags and consuming the values of value-taking flags. gh accepts
# -R/--repo (and other flags) BEFORE or BETWEEN the command tokens, so scanning for
# positionals — not fixed argv slots — closes the `gh -R o/r pr merge` hole.
cmd=""; sub=""; sel=""; repo=""; want=""
for tok in "$@"; do
  if [ "$want" = "repo" ]; then repo="$tok"; want=""; continue; fi
  if [ "$want" = "skip" ]; then want=""; continue; fi
  case "$tok" in
    -R|--repo) want="repo"; continue ;;
    --repo=*) repo="${tok#--repo=}"; continue ;;
    -R?*) repo="${tok#-R}"; continue ;;
    __VF_SEP__) want="skip"; continue ;;
    __VF_GLUED__) continue ;;
    -*) continue ;;
    *)
      if [ -z "$cmd" ]; then cmd="$tok"
      elif [ -z "$sub" ]; then sub="$tok"
      elif [ -z "$sel" ]; then sel="$tok"
      fi ;;
  esac
done
__GIT_PLUMBING__

# RELEASE/TAG publish (#83): create/edit/delete a release is a publish-to-the-world
# action — allowed when the group is autonomous AND has opted in via the auto_release
# marker (parallel to autonomous+auto_merge for merges), OR by an explicit per-tag
# release grant. Read-only release subcommands (view/list/download) pass through.
if [ "$cmd" = "release" ]; then
  case "$sub" in
    create|edit|delete)
      # `gh release <sub> <tag>` names its tag as the SELECTOR — that is the locus,
      # not a body field, so there is no id to resolve and nothing to cross-check.
      loomux_release_gate "$sel" "$sub" "" "" ""   # allow (return) or block (exit); tag = $sel
      exec "$REAL_GH" "$@" ;;
    *) exec "$REAL_GH" "$@" ;;
  esac
fi

# RELEASE via raw `gh api` / graphql (#196): the `gh release` subcommand above is the
# ergonomic path, but the SAME publish can be driven through `gh api`. Decide by
# LOCUS — the request METHOD, the URL PATH, and the parsed `ref`/`query` field — never
# by substring-anywhere over the argv (a cosmetic `refs/heads/` in a header/jq/sha/
# query string must NOT be able to disguise a `refs/tags/` create; #196 r3). We parse
# gh api's own flags here (the shared positional parser above is tuned for pr/release).
if [ "$cmd" = "api" ]; then
  a_method=""; a_url=""; a_ref=""; a_query=""; a_qopaque=0; a_tagname=""
  a_inputval=""; a_hasparam=0; aw=""; seen_cmd=0
  for tok in "$@"; do
    if [ -n "$aw" ]; then
      case "$aw" in
        method) a_method=$(printf '%s' "$tok" | tr '[:lower:]' '[:upper:]')
                loomux_norm_guard "$tok" "$a_method" "api-method" ;;
        field)
          a_hasparam=1
          case "$tok" in
            ref=*)      a_ref=${tok#ref=} ;;
            tag_name=*) a_tagname=${tok#tag_name=} ;;
            query=*)    q=${tok#query=}; case "$q" in @*) a_qopaque=1 ;; *) a_query=$q ;; esac ;;
          esac ;;
        input) a_hasparam=1; a_inputval="$tok" ;;
        skip) : ;;
      esac
      aw=""; continue
    fi
    case "$tok" in
      -X|--method) aw="method"; continue ;;
      -X?*)        a_method=$(printf '%s' "${tok#-X}" | tr '[:lower:]' '[:upper:]')
                   loomux_norm_guard "${tok#-X}" "$a_method" "api-method"; continue ;;
      --method=*)  a_method=$(printf '%s' "${tok#--method=}" | tr '[:lower:]' '[:upper:]')
                   loomux_norm_guard "${tok#--method=}" "$a_method" "api-method"; continue ;;
      -f|-F|--field|--raw-field) aw="field"; continue ;;
      --field=*|--raw-field=*)
        a_hasparam=1; v=${tok#*=}
        case "$v" in
          ref=*)      a_ref=${v#ref=} ;;
          tag_name=*) a_tagname=${v#tag_name=} ;;
          query=*)    q=${v#query=}; case "$q" in @*) a_qopaque=1 ;; *) a_query=$q ;; esac ;;
        esac
        continue ;;
      --input) aw="input"; continue ;;
      --input=*) a_hasparam=1; a_inputval=${tok#--input=}; continue ;;
      # Other value-taking gh-api flags: consume the value so it can never be mistaken
      # for the URL, and so a decoy ref string inside it is never part of the locus.
      -H|--header|-q|--jq|-t|--template|--cache|--hostname|-p|--preview) aw="skip"; continue ;;
      --header=*|--jq=*|--template=*|--cache=*|--hostname=*|--preview=*) continue ;;
      -*) continue ;;   # boolean flags (--paginate, -i/--include, --slurp, --silent, …)
      *) # first bare positional is the `api` command token; the next is the endpoint.
         if [ "$seen_cmd" = "0" ]; then seen_cmd=1; elif [ -z "$a_url" ]; then a_url="$tok"; fi ;;
    esac
  done
  [ -z "$a_method" ] && { [ "$a_hasparam" = "1" ] && a_method="POST" || a_method="GET"; }
  # gh reads the ref from a JSON body too (`--input <file>`): parse the file's "ref"
  # so a heads-locus body is provably a branch. `--input -` (stdin) is unparseable →
  # ref stays empty → cannot prove heads → fail-safe gate below.
  if [ -n "$a_inputval" ] && [ "$a_inputval" != "-" ] && [ -z "$a_ref" ] && [ -f "$a_inputval" ]; then
    body=$(cat "$a_inputval" 2>/dev/null)
    case "$body" in
      *'"ref"'*) r=${body#*\"ref\"}; r=${r#*:}; r=${r#*\"}; a_ref=${r%%\"*} ;;
    esac
  fi

  is_write=0; case "$a_method" in GET|HEAD) is_write=0 ;; *) is_write=1 ;; esac
  # URL PATH only (strip any ?query — a decoy `?d=refs/heads/z` must not read as heads).
  a_path=${a_url%%\?*}
  path_low=$(printf '%s' "$a_path" | tr '[:upper:]' '[:lower:]')
  ref_low=$(printf '%s' "$a_ref" | tr '[:upper:]' '[:lower:]')
  loomux_norm_guard "$a_path" "$path_low" "api-url-path"
  loomux_norm_guard "$a_ref" "$ref_low" "api-ref-field"

  is_rel=0; rtag=""
  # Recognize the graphql endpoint by SUFFIX (like the REST URL arms below), not an
  # exact 'graphql' — gh also accepts `/graphql` and the full-URL host form, all sent
  # as a POST of {"query":…} (#196 r4). Any call to that locus is a graphql write.
  is_graphql=0
  case "$path_low" in graphql|/graphql|*/graphql) is_graphql=1 ;; esac
  if [ "$is_graphql" = "1" ]; then
    # If the query is opaque (from --input/stdin or query=@file) we cannot scan it →
    # fail-safe gate. Otherwise: any ref/tag/release-CREATING mutation gates
    # UNCONDITIONALLY — there is NO "prove it's a safe branch from the text" logic in
    # the graphql arm, by design. Every text heuristic we tried (a refs/tags literal, a
    # -F ref= variable, a no-`$`-variables rule) was defeated by the next encoding —
    # graphql variables, comments, aliases, and string escapes (`refs\/tags\/`) each dodge
    # a text scan, and the next encoding would too (#196 r6). A graphql createRef to a
    # BRANCH is a rare corner (agents branch via `git push` or REST `git/refs`, which the
    # REST arm classifies by real locus); gating it fails safe — markers/grant still
    # allow it. A non-mutation read query carries none of these tokens → passes.
    if [ -n "$a_inputval" ] || [ "$a_qopaque" = "1" ]; then
      is_rel=1
    elif [ -n "$a_query" ]; then
      # rev-32 NB1: the LAST fail-open normalizer. An empty $ql matches none of the
      # mutation tokens below, so a graphql deleteRef of a published tag ref would
      # pass ungated. It was unreachable only because the path_low guard exits
      # first — and "unreachable because another guard happens to run first" is the
      # exact reasoning that produced #509. Guarded on its own merits.
      ql=$(printf '%s' "$a_query" | tr '[:upper:]' '[:lower:]')
      loomux_norm_guard "$a_query" "$ql" "graphql-query"
      # Full create+move+DELETE coverage of refs/tags/releases, matching the REST arm
      # (which gates POST/PATCH/DELETE of git/refs|git/tags and create/edit/delete of
      # releases). deleteRef is destructive — it can drop a published v* tag ref — so it
      # gates like DELETE git/refs/tags/* and deleteRelease. Matched by the field-name
      # token (an unescapable identifier), consistent with the class-closing fix.
      case "$ql" in
        *createref*|*updateref*|*deleteref*|*createtag*|*deletetag*|*createrelease*|*updaterelease*|*deleterelease*) is_rel=1 ;;
      esac
      # resolve the tag for grant-keying: a refs/tags variable, else an inline literal.
      case "$ref_low" in refs/tags/*) rtag=${a_ref#refs/tags/}; rtag=${rtag%% *} ;; esac
      if [ -z "$rtag" ]; then
        case "$a_query" in
          *tagName:*)   rest=${a_query#*tagName:}; rest=$(printf '%s' "$rest" | tr -d ' "'); rtag=${rest%%,*}; rtag=${rtag%%\}*}; rtag=${rtag%%)*} ;;
          *refs/tags/*) rest=${a_query#*refs/tags/}; rest=$(printf '%s' "$rest" | tr -d ' "'); rtag=${rest%%,*}; rtag=${rtag%%\}*}; rtag=${rtag%%)*} ;;
        esac
      fi
    fi
  elif [ "$is_write" = "1" ]; then
    # A non-GET write to the git refs/tags plumbing, decided by the URL path SEGMENT.
    case "$path_low" in
      git/refs|git/refs/*|*/git/refs|*/git/refs/*|git/tags|git/tags/*|*/git/tags|*/git/tags/*)
        # Exempt ONLY when the ref locus is PROVABLY a branch: URL path .../refs/heads/…
        # OR the parsed ref field refs/heads/… — AND refs/tags/ absent from that locus.
        heads=0; tags=0
        case "$path_low" in */refs/heads/*) heads=1 ;; esac
        case "$ref_low"  in refs/heads/*)   heads=1 ;; esac
        case "$path_low" in */refs/tags/*)  tags=1 ;; esac
        case "$ref_low"  in refs/tags/*)    tags=1 ;; esac
        if [ "$heads" = "1" ] && [ "$tags" = "0" ]; then is_rel=0; else is_rel=1; fi ;;
    esac
    # A write to the releases endpoint (read-only GET list/view already excluded above).
    if [ "$is_rel" = "0" ]; then
      case "$path_low" in releases|releases/*|*/releases|*/releases/*) is_rel=1 ;; esac
    fi
    # Resolve the tag for grant-keying from the locus (ref field, URL path, tag_name).
    if [ "$is_rel" = "1" ]; then
      case "$ref_low" in refs/tags/*) rtag=${a_ref#refs/tags/}; rtag=${rtag%% *} ;; esac
      case "$path_low" in */git/refs/tags/*) [ -z "$rtag" ] && { rest=${a_path##*/git/refs/tags/}; rtag=${rest%%/*}; } ;; esac
      [ -z "$rtag" ] && [ -n "$a_tagname" ] && rtag="$a_tagname"
    fi
  fi
  if [ "$is_rel" = "1" ]; then
    # #437: hand the gate the release resource this call addresses, so it can
    # resolve id → tag itself.
    #
    # `_relid=1` means "this URL names ONE SPECIFIC release by numeric id". That
    # flag is computed UNCONDITIONALLY — never skipped because the argv happened
    # to mention a tag — and it is the security-critical half of #437 (rev B1).
    # For such a call the release's identity is the ID; `tag_name=`/`ref=` are
    # body fields the CALLER chose, so trusting them here let one live grant
    # reach every release in the repo:
    #   gh api -X PATCH repos/o/r/releases/777 -f tag_name=v1.2.3 -f make_latest=true
    # keyed the gate on v1.2.3, found the human's grant, and retagged release 777
    # (never authorized by anyone) plus stole `latest`. `-X DELETE … -f
    # tag_name=v1.2.3` did the same to delete one — `tag_name` is ignored by the
    # API on a DELETE, so it existed purely to satisfy the gate. That is the
    # mirror image of the decoys `gh_shim_harness_gates_raw_api_tag_ref_by_locus_
    # defeating_decoys` already defeats: those loosen the gate, these SATISFY it.
    # This is exactly the locus principle the rest of this arm follows — for
    # `git/refs/tags/v9` the locus IS the tag; for `releases/<id>` it is the id.
    #
    # `_relpath` — the resource to GET — is `$a_path` ITSELF, not a reconstructed
    # prefix and not the lowercased copy (rev N2). That is the property the whole
    # resolution rests on: gh normalizes the string it is given, so resolving and
    # writing the SAME string can never address two different releases. An earlier
    # cut resolved `${prefix}releases/${id}`, and there `…/releases/555/../444`
    # really would have read release 555's tag and written to 444 — with `$a_path`
    # that divergence is not expressible.
    #
    # So the two shape tests below are NOT what makes traversal safe (the PR body
    # said they were; that was written against the prefix design and was wrong).
    # What they do is decide `_relid`, and that is load-bearing for a different
    # reason: a URL that names a release loomux cannot pin down must still count
    # as id-addressed, so the argv tag is refused the chance to speak for it.
    #   - `…/releases/555/assets`  → id-addressed, unidentifiable → refuse
    #   - `…/releases/../777`      → id-addressed, unidentifiable → refuse
    # Drop either test and `-f tag_name=<granted>` walks through on those shapes,
    # which is B1 again in a different dress. Both are pinned by mutation.
    #
    # `…/releases/tags/<tag>` (rev round 2) names its release by TAG, in the URL.
    # That is a locus in the plainest sense — no lookup needed, the answer is
    # right there — so it is read from the URL and outranks the argv exactly as a
    # resolved id does. Before that it fell through to the ordinary tag path, and
    # `-X PATCH …/releases/tags/v0.0.9 -f tag_name=<granted>` was allowed while
    # naming somebody else's release. GitHub happens to expose no write on that
    # endpoint today, so it was unreachable rather than harmless — and "unreachable
    # because of the shape of someone else's API surface" is not a property this
    # gate should rest on. `…/releases/latest` names no specific release and is
    # left to the ordinary tag path.
    _relpath=""; _relid=0; _urltag=""
    case "$path_low" in
      releases/*|*/releases/*)
        case "$path_low" in
          *..*)
            # A traversal inside a releases URL: whatever it resolves to, loomux
            # cannot say WHICH release that is, so nothing may speak for it.
            _relid=1 ;;
          releases/tags/*|*/releases/tags/*)
            # Identity comes from the URL's own tag segment. Taken from $a_path,
            # never $path_low, because a tag's CASE is part of it (`vRelease` is
            # not `vrelease`) and this value is matched against a grant. If the
            # caller spelled the fixed segments in another case, $a_path won't
            # match and the tag stays empty — which refuses, not guesses.
            _relid=1
            case "$a_path" in
              *releases/tags/*) _urltag=${a_path##*releases/tags/}; _urltag=${_urltag%%/*} ;;
            esac
            case "$_urltag" in ''|*[!A-Za-z0-9._+-]*) _urltag="" ;; esac ;;
          *)
            _rid=${path_low##*releases/}
            case "$_rid" in
              ''|*[!0-9]*)
                # Not a bare id — but still id-ADDRESSED when it STARTS with
                # digits (`555/assets`), i.e. a sub-resource of one release.
                case "$_rid" in [0-9]*) _relid=1 ;; esac ;;
              *) _relid=1; _relpath="$a_path" ;;
            esac ;;
        esac ;;
    esac
    loomux_release_gate "$rtag" "api" "$_relpath" "$_relid" "$_urltag"   # allow (return) or block (exit)
    exec "$REAL_GH" "$@"
  fi
fi

# Is this a merge we must gate? `gh pr merge` (wherever flags land), or an api shape.
is_merge=0
if [ "$cmd" = "pr" ] && [ "$sub" = "merge" ]; then
  is_merge=1
elif [ "$cmd" = "api" ]; then
  all="$*"
  low=$(printf '%s' "$all" | tr '[:upper:]' '[:lower:]')
  loomux_norm_guard "$all" "$low" "api-argv"
  case "$low" in *mergepullrequest*) is_merge=1 ;; esac
  case "$all" in *pulls*) case "$all" in *"/merge"*) is_merge=1 ;; esac ;; esac
fi

# ── THE PR-OPEN SIZE ADVISORY (#1174) — BEST-EFFORT, AND IT FAILS *OPEN* ──────
# The only thing in this shim that does. It decides NOTHING: `gh pr create` runs
# first, its exit status is passed through untouched, and every step below is
# skipped on any doubt — no gate file, an unreadable limit, a size gh would not
# tell us. A courtesy notice that could break or delay opening a PR would be a
# far worse trade than one that occasionally does not appear.
#
# It goes to the pane of the agent that OPENED the PR — which is the actor that
# can split it, at the moment the split is cheapest. It is deliberately not sent
# to the orchestrator: the shim's only channel into loomux is audit.jsonl, and
# building a durable agent-writable file whose text lands in the orchestrator's
# tool results would be a prompt-injection channel into the trust root. See
# docs/design/workflows.md → "The PR-open advisory, and which way each half fails".
#
# The REFUSAL is the enforced half and lives in the merge gate below, where
# "unknown is never safe" applies in full.
if [ "$cmd" = "pr" ] && [ "$sub" = "create" ]; then
  "$REAL_GH" "$@"
  a_rc=$?
  if [ "$a_rc" -eq 0 ] && [ -n "$ORX_GD" ] && [ -f "$ORX_GD/merge_gate" ]; then
    a_max=0
    # Builtins only — no sed/awk — and the same trailing-newline-safe read as the
    # gate parser. An unrecognized key is simply not this one; unlike the gate
    # parser, an unreadable file here means "say nothing", never "refuse".
    while read -r a_k a_v || [ -n "$a_k" ]; do
      [ "$a_k" = "max-diff-lines" ] && a_max="$a_v"
    done < "$ORX_GD/merge_gate"
    case "$a_max" in ''|*[!0-9]*) a_max=0 ;; esac
    # `--head` names a branch OTHER than the checked-out one, and the lookup below
    # has no PR number to use — it resolves the CURRENT branch's PR, which would
    # then be a confident size for the wrong PR (#1181 rev-lead NB3). Every other
    # doubt on this path goes silent; this one would print misinformation, so it
    # is the one shape that is skipped outright rather than measured. (The
    # neighbouring case — a branch that already has an open PR — closes itself:
    # `gh pr create` fails there, and a non-zero rc already skips all of this.)
    a_head=0
    for a_tok in "$@"; do
      case "$a_tok" in --head|--head=*|-H|-H?*) a_head=1 ;; esac
    done
    if [ "$a_max" -gt 0 ] && [ "$a_head" = "0" ]; then
      # Fully quoted, in both branches — no unquoted expansion, so there is no
      # globbing question to answer here at all (#1181 rev-lead NB2). The merge
      # path below word-splits its own `-R` under `set -f`; this path runs BEFORE
      # `set -f` is reached, so it must not rely on it.
      if [ -n "$repo" ]; then
        a_lines=$("$REAL_GH" pr view -R "$repo" --json additions,deletions --jq '.additions + .deletions' 2>/dev/null)
      else
        a_lines=$("$REAL_GH" pr view --json additions,deletions --jq '.additions + .deletions' 2>/dev/null)
      fi
      case "$a_lines" in ''|*[!0-9]*) a_lines="" ;; esac
      if [ -n "$a_lines" ] && [ "$a_lines" -gt "$a_max" ]; then
        printf '%s\n' "orrerix: heads up — this PR changes $a_lines lines and this repo's merge gate declares max_diff_lines: $a_max, so the merge WILL be refused as it stands. Split it now, before anyone reviews it: a split after review means the review is spent twice. (This notice is advisory only — the PR was created.)" >&2
        loomux_audit "pr-size-advisory" "{\"lines\":$a_lines,\"limit\":$a_max}"
      fi
    fi
  fi
  exit "$a_rc"
fi

__CLOSE_GATE__
if [ "$is_merge" = "0" ]; then
  exec "$REAL_GH" "$@"
fi

# A MERGE WITH NO GROUP DIR IS A MERGE THIS APP CANNOT GATE — refuse it. Every agent
# pane gets the group-dir variables (BOTH spellings, #1153 phase 3) and the
# shimmed PATH together (`agent_pane_env`), and
# a human's own shell never has the shim on PATH at all — so reaching the shim
# without either of them means they were unset on the way (`env -u ORRERIX_GROUP_DIR
# -u LOOMUX_GROUP_DIR gh pr
# merge …`), which is evasion, not a supported flow. The human gate already fails
# closed on this shape for a default-branch merge (no markers, no grant → blocked);
# without this, an unset variable still slipped a NON-default merge past the workflow
# gate — with nothing in the audit, because there is no audit log to write to.
# Symmetry is the honest fix. (This closes the cheap shape only: an agent with a
# shell can still call the real gh by absolute path. See docs/design/workflows.md →
# "The bypass surface, honestly".)
if [ -z "$ORX_GD" ]; then
  printf '%s\n' "orrerix: refusing to merge — neither ORRERIX_GROUP_DIR nor LOOMUX_GROUP_DIR is set, so this merge cannot be checked against the group's gates. Run gh from your agent pane's normal environment; do NOT unset them." >&2
  exit 1
fi

# A raw `gh api` merge has no cheaply-resolvable base ref → fail-safe block.
if [ "$cmd" = "api" ]; then
  loomux_block "api-merge" "(api)" "?"
fi

# Resolve the PR's base branch AND number via the REAL gh (one call), honoring the
# SAME -R/--repo the user passed (rev-79 F2). The number keys the per-PR grant, so
# a grant for one PR can't authorize merging another.
rf=""
[ -n "$repo" ] && rf="-R $repo"
info=$("$REAL_GH" pr view $rf $sel --json baseRefName,number --jq '.baseRefName+" "+(.number|tostring)' 2>/dev/null)
base=${info%% *}
num=${info##* }
# #294: `gh repo view` takes the repo as a POSITIONAL arg, not -R/--repo (unlike
# `pr view` above) — `gh repo view -R o/r` errors "unknown shorthand flag: 'R' in
# -R". Passing $rf here silently broke every -R-qualified merge: this lookup came
# back empty, so the block below fired as "unverifiable-base" even with a valid
# grant sitting in merge_grants/ (the grant is never consumed by a blocked
# attempt, and the block is otherwise fail-safe — both preserved by this fix).
default=$("$REAL_GH" repo view $repo --json defaultBranchRef --jq .defaultBranchRef.name 2>/dev/null)

if [ -z "$base" ] || [ -z "$default" ]; then
  loomux_block "unverifiable-base" "$base" "$sel"
fi

# ── THE WORKFLOW MERGE GATE (#222, closing the orrerix half of #197) ───────────
# When the repo declares `gates.merge`, orrerix writes a `merge_gate` spec file into
# the group dir, and every reviewer's `review_verdict` lands in
# `verdicts/pr-<N>/<block>` with the verdict word (pass|fail|escalate) as line 1.
#
# THREE properties, in the order they are enforced:
#  1. It runs BEFORE the human-grant / autonomous / dangerous-mode openings below,
#     so none of them can satisfy it. #197 Scope B asks for an auto-merge to be
#     "structurally impossible until every required review verdict is recorded
#     PASS"; a gate that a grant could override would not be that.
#  2. It applies to EVERY merge of the PR, not only to the default branch. The
#     declared reviewers reviewed *this PR*; where it lands doesn't change whether
#     they finished. (The human gate below stays default-branch-only — unchanged.)
#  3. No `merge_gate` file → this whole block is skipped → byte-for-byte the
#     pre-#222 flow. Every group without a workflow file is in that case.
if [ -f "$ORX_GD/merge_gate" ]; then
  gatef="$ORX_GD/merge_gate"
  # Without a PR number no verdict can be attributed to this merge → fail closed.
  [ -n "$num" ] || loomux_block_wf "unresolved-pr" "orrerix could not resolve the PR number, so it cannot check the recorded verdicts against it"
  # THE REVISION THIS MERGE WOULD LAND. A verdict binds to a COMMIT, not to a PR
  # number: without this, two reviewers pass #7, the worker pushes "fixed lint",
  # and the gate still reads green over code nobody reviewed — #197's failure class,
  # and the reason GitHub dismisses stale approvals on new commits. Unresolvable →
  # refuse, the same fail-safe an undeterminable base takes.
  cur_head=$("$REAL_GH" pr view $rf "$num" --json headRefOid --jq .headRefOid 2>/dev/null)
  cur_head=$(printf '%s' "$cur_head" | tr '[:upper:]' '[:lower:]')
  [ -n "$cur_head" ] || loomux_block_wf "unresolved-head" "orrerix could not resolve the PR's current head commit, so it cannot tell whether the recorded verdicts reviewed the code that would merge"
  # No globbing anywhere below: the gate file's tokens are word-split into `for`
  # loops, and a security shim should not leave the next reader working out whether
  # a `*` could reach a filename. (orrerix never writes one — sanitize_id /
  # sanitize_condition reject glob characters — so this is belt, not braces.)
  set -f
  g_req="all-pass"; g_thr=0; g_revs=""; g_also=""; g_maxdiff=0; g_rpaths=""; g_rrevs=""
  # `|| [ -n "$g_k" ]` is load-bearing: POSIX `read` returns non-zero at EOF, so a
  # final line with NO trailing newline would otherwise be silently DROPPED — and a
  # dropped `reviewer`/`also` line makes the gate WEAKER, which is the one direction
  # this design says must never happen. A truncated gate file is exactly the case
  # the malformed-gate check below claims to handle.
  while read -r g_k g_v g_w || [ -n "$g_k" ]; do
    case "$g_k" in
      \#*|'')   : ;;   # comment / blank
      require)  g_req="$g_v"; [ -n "$g_w" ] && g_thr="$g_w" ;;
      reviewer) [ -n "$g_v" ] && g_revs="$g_revs $g_v" ;;
      also)     [ -n "$g_v" ] && g_also="$g_also $g_v" ;;
      # #1174's small-batch clause. A structured key, not an `also:` token,
      # because it carries a NUMBER — see `Gate::max_diff_lines`.
      max-diff-lines) g_maxdiff="$g_v" ;;
      # #1176's path routing. Each rule arrives as two kinds of line, stitched
      # back together by the 1-based index in $g_v — three fixed fields, because
      # this loop has three variables and no arrays to unpack a packed line into.
      # Collected as `<index>:<value>` tokens; VALIDATED below, where a half with
      # no partner becomes a malformed gate rather than a rule that quietly
      # requires nobody.
      route-path)     g_rpaths="$g_rpaths $g_v:$g_w" ;;
      route-reviewer) g_rrevs="$g_rrevs $g_v:$g_w" ;;
      # An unrecognized key is NOT skipped. orrerix writes an `unrepresentable` line
      # when it cannot safely serialize a token (rather than dropping the clause),
      # and a hand edit or a truncation lands here too. Skipping any of them would
      # silently drop a requirement from a gate.
      *) loomux_block_wf "malformed-gate" "the merge gate file contains a line orrerix cannot parse ('$g_k') — a gate it cannot read in full is not a gate it will enforce in part. This self-heals on its own (the next background reload regenerates it) if that workflow.yml is well-formed; if the refusal persists, that file is what needs fixing. No relaunch needed either way" ;;
    esac
  done < "$gatef"
  # A gate naming nobody, or a threshold with no usable number, is a MALFORMED gate
  # — refuse rather than wave it through. (orrerix only ever writes well-formed gate
  # files; this is the hand-edited/truncated case.)
  [ -n "$g_revs" ] || loomux_block_wf "malformed-gate" "the declared merge gate names no reviewers"
  # The gate's RULE, validated up front. An unrecognized `require` is refused, not
  # quietly read as all-pass: `all-pass` happens to be the strict one today, so the
  # silent fallback looked safe — but it means the shim would enforce a rule the file
  # does not state, and the Rust half already calls this file MALFORMED. Two halves of
  # one gate must agree about what it says, not merely land on the same answer by luck.
  case "$g_req" in
    all-pass) : ;;
    threshold) case "$g_thr" in ''|*[!0-9]*) g_thr=0 ;; esac
               [ "$g_thr" -ge 1 ] || loomux_block_wf "malformed-gate" "the declared merge gate says require: threshold but carries no usable threshold number" ;;
    *) loomux_block_wf "malformed-gate" "the merge gate declares an unrecognized require value ('$g_req') — orrerix understands 'all-pass' and 'threshold'. A rule it cannot read is not a rule it will guess at" ;;
  esac
  # THE SMALL-BATCH CLAUSE (#1174), checked BEFORE the verdict counting below —
  # deliberately. Its remedy ("split this PR") does not depend on any verdict, and
  # the whole point of a size gate is that the split happens before review effort
  # is spent; telling an agent to wait for reviewers on a PR it is going to have to
  # split anyway is the wrong first sentence. An unusable number is a MALFORMED
  # gate, never "no limit": the lax reading of a bound the repo wrote down is the
  # one direction this design never takes. (orrerix only writes well-formed values —
  # `parse_workflow` refuses 0 and serde refuses a negative — so this is the
  # hand-edited/truncated case, the same one the `require` arm above covers.)
  case "$g_maxdiff" in ''|*[!0-9]*) loomux_block_wf "malformed-gate" "the merge gate declares a max-diff-lines value orrerix cannot read as a number ('$g_maxdiff')" ;; esac
  if [ "$g_maxdiff" -gt 0 ]; then
    # additions+deletions over the whole PR, from gh's own JSON — NOT parsed out of
    # `gh pr diff --stat`'s English summary line, whose wording ("1 file changed, 2
    # insertions(+)") is prose gh may reword and which drops the word entirely for a
    # zero count. A shim that has to word-split a sentence to decide a merge is a
    # shim that fails in a new way every time that sentence changes.
    d_lines=$("$REAL_GH" pr view $rf "$num" --json additions,deletions --jq '.additions + .deletions' 2>/dev/null)
    case "$d_lines" in
      ''|*[!0-9]*) loomux_block_wf "diff-size-unknown" "the gate declares max_diff_lines: $g_maxdiff and orrerix could not read this PR's size, so it cannot tell whether it is within the limit — an unmeasurable PR is refused, not waved through" ;;
    esac
    [ "$d_lines" -le "$g_maxdiff" ] || loomux_block_wf "diff-too-large" "this PR changes $d_lines lines and this repo's merge gate declares max_diff_lines: $g_maxdiff. Split it into PRs that each land under the limit — a review nobody can hold in their head is the failure this gate exists to prevent"
  fi
  # ── PATH-BASED REVIEWER ROUTING (#1176) ──────────────────────────────────────
  # Runs BEFORE the verdict counting below because it decides what that counting
  # counts: `$g_revs` is the static list until here, and the routed lanes are
  # appended to it. Everything downstream — the pass/stale/outstanding split, the
  # `body-unchanged` loop — then treats a routed reviewer exactly like a declared
  # one, which is the whole design: routing resolves to a reviewer list and gets
  # out of the way, so there is one gate decision and not two.
  #
  # It can only ever ADD. There is no spelling here that removes a reviewer the
  # gate already named, which is what makes "declaring a routing rule cannot make
  # this gate easier to satisfy" a property of the code rather than a promise.
  g_routed=""; g_rfired=""; g_rnote=""
  if [ -n "$g_rpaths$g_rrevs" ]; then
    # `threshold` + routing is refused at parse and unrepresentable in the gate
    # file. Reaching here means a hand edit or a regressed writer, so: malformed,
    # never "read it as a threshold gate and hope" — that reading is the laxer one.
    [ "$g_req" = "all-pass" ] || loomux_block_wf "malformed-gate" "the merge gate declares path routing together with require: '$g_req'. Routing makes the required reviewer set depend on the diff and a threshold counts votes over a fixed list; orrerix will not guess which of the two this gate meant"
    # Shape first: every token must be <positive index>:<non-empty value>. A
    # token this cannot read is a rule half, and a dropped rule half is a
    # required reviewer that silently stops being required.
    g_rmax=0
    for g_t in $g_rpaths $g_rrevs; do
      case "$g_t" in *:*) : ;; *) loomux_block_wf "malformed-gate" "the merge gate carries a routing line orrerix cannot read ('$g_t')" ;; esac
      g_i=${g_t%%:*}
      case "$g_i" in ''|*[!0-9]*) loomux_block_wf "malformed-gate" "the merge gate carries a routing line whose rule number is not a number ('$g_t')" ;; esac
      [ "$g_i" -ge 1 ] || loomux_block_wf "malformed-gate" "the merge gate carries a routing line numbered 0 — routing rules are numbered from 1"
      [ -n "${g_t#*:}" ] || loomux_block_wf "malformed-gate" "the merge gate carries an empty routing value for rule $g_i"
      [ "$g_i" -le "$g_rmax" ] || g_rmax="$g_i"
    done
    # The rule cap, from the SAME constant `parse_workflow` and `parse_gate_file`
    # refuse above (`ROUTING_RULES_MAX`, interpolated). Without it the two halves
    # disagree: a hand-edited file with more rules than the cap is unreadable to
    # Rust — which reports "malformed, every merge refused" — and perfectly
    # readable to this loop. That divergence happens to fall on the STRICT side
    # (more rules is more required reviewers), which is exactly why it would
    # never have been noticed; "the two halves agree, and both fail closed" is
    # the property, not "the disagreement is harmless this time".
    [ "$g_rmax" -le __ROUTING_RULES_MAX__ ] || loomux_block_wf "malformed-gate" "the merge gate declares routing rule $g_rmax, past the __ROUTING_RULES_MAX__-rule limit orrerix will load — so this file is one orrerix cannot read back, and an unreadable gate refuses every merge"
    # …then completeness: rules are numbered 1..N and each needs BOTH halves. A
    # rule with paths and no reviewers requires nobody; one with reviewers and no
    # paths can never fire. Both are the same laxening in different clothes.
    g_i=1
    while [ "$g_i" -le "$g_rmax" ]; do
      g_hasp=0; g_hasr=0
      for g_t in $g_rpaths; do case "$g_t" in "$g_i":*) g_hasp=$((g_hasp+1)) ;; esac; done
      for g_t in $g_rrevs;  do case "$g_t" in "$g_i":*) g_hasr=1 ;; esac; done
      { [ "$g_hasp" -ge 1 ] && [ "$g_hasr" = "1" ]; } || loomux_block_wf "malformed-gate" "the merge gate declares routing rule $g_i with only half of it — a rule needs at least one path and at least one reviewer"
      # The PER-RULE path cap, the other half of the bound the Rust readers
      # enforce (#1176 rev-972 N1). Left open, a 33-path rule was unreadable to
      # `parse_gate_file` — malformed, every merge refused — and perfectly
      # readable here: the same two-halves divergence the rule cap above closed,
      # one bound over, and no more harmless for falling on the strict side.
      [ "$g_hasp" -le __ROUTING_PATHS_MAX__ ] || loomux_block_wf "malformed-gate" "the merge gate declares $g_hasp paths on routing rule $g_i, past the __ROUTING_PATHS_MAX__-path limit orrerix will load — so this file is one orrerix cannot read back, and an unreadable gate refuses every merge"
      g_i=$((g_i+1))
    done
    # THE CHANGED-FILE LIST. `__ROUTING_FILES_JQ__` is interpolated from
    # `workflow.rs` (ROUTING_FILES_JQ) so this and the merge queue ask GitHub the
    # SAME question — including the truncation clause, without which a PR of more
    # than 100 files reports a short list and a rule whose only matching file is
    # on page two never fires. Unlike the size gate, that failure is invisible:
    # the gate goes green one lane short and nothing anywhere says so.
    #
    # Piped, never a heredoc and never an unquoted expansion of gh's output: the
    # paths in it are chosen by whoever opened the PR, and an unquoted heredoc
    # body performs command substitution on its content. `printf`/pipe is the
    # only shape where a filename cannot become a command.
    #
    # The scan is a FUNCTION, called from the command substitution rather than
    # written inside it. That is not style: a `case` pattern's `)` is unbalanced,
    # and a shell that finds the end of `$( … )` by COUNTING parens rather than
    # parsing recursively — bash 3.2, which is `/bin/sh` on macOS — mis-locates
    # the closing paren and reports a syntax error at the first `;;`. A function
    # body is parsed where it is DEFINED, out here, so the substitution below
    # contains no `case` at all.
    #
    # It runs in a subshell either way (POSIX pipes do), so its answer LEAVES as
    # stdout: `hit<indices>` when the list was complete, `bad` otherwise. A
    # subshell that set a variable would set it in the subshell.
    loomux_route_scan() {
      r_ok=0; r_hit=" "
      while IFS= read -r r_line; do
        if [ "$r_ok" = "0" ]; then
          # The header, and the only word that means "every changed file is below".
          [ "$r_line" = "ok" ] || { r_ok=2; break; }
          r_ok=1; continue
        fi
        [ -n "$r_line" ] || continue
        # A line without the prefix is a protocol this build does not speak.
        case "$r_line" in "p "?*) : ;; *) r_ok=2; break ;; esac
        r_f=${r_line#p }
        for r_t in $g_rpaths; do
          r_i=${r_t%%:*}; r_g=${r_t#*:}
          # This rule already fired; nothing another file could say changes it.
          case "$r_hit" in *" $r_i "*) continue ;; esac
          # THE GLOB. `$r_g` is unquoted in pattern position, which is what makes
          # `*` a wildcard — and `sanitize_glob` is what makes that safe: the
          # alphabet it permits contains no `[`, `\`, `?`, brace or space, so `*`
          # is the ONLY metacharacter that can reach here. `*` crosses `/` in a
          # shell pattern, and `glob_match` in workflow.rs says the same, on
          # purpose: over-matching requires an extra lane, under-matching skips
          # one, and only one of those is survivable.
          case "$r_f" in $r_g) r_hit="$r_hit$r_i "; continue ;; esac
          # A leading `**/` is optional, so `**/Cargo.toml` covers the one at the
          # repo root too — the mirror of `glob_match`'s rule 3.
          case "$r_g" in '**/'*) r_g2=${r_g#'**/'}; case "$r_f" in $r_g2) r_hit="$r_hit$r_i " ;; esac ;; esac
        done
      done
      if [ "$r_ok" = "1" ]; then printf 'hit%s\n' "$r_hit"; else printf 'bad\n'; fi
    }
    g_hit=$("$REAL_GH" pr view $rf "$num" --json files,changedFiles --jq '__ROUTING_FILES_JQ__' 2>/dev/null | loomux_route_scan)
    case "$g_hit" in
      hit*) g_hit=" ${g_hit#hit} " ;;
      *) loomux_block_wf "routing-unaccountable" "this repo's merge gate routes reviewers by path, and orrerix could not account for every file this PR changed — either gh would not report them, or it reported fewer files than the PR says it has (its file list pages at 100). It therefore cannot tell which routing rules apply, and an unknown reviewer requirement is refused rather than assumed empty. Re-run the merge; if this PR permanently changes more than 100 files, split it — path routing cannot be enforced for it" ;;
    esac
    # The union, in declaration order: the static list, then each fired rule.
    # `workflow::route_reviewers` appends in exactly this order, so the two
    # produce the same LIST and not merely the same set.
    for g_t in $g_rrevs; do
      g_i=${g_t%%:*}; g_r=${g_t#*:}
      case "$g_hit" in *" $g_i "*) : ;; *) continue ;; esac
      case " $g_revs " in *" $g_r "*) continue ;; esac
      g_revs="$g_revs $g_r"; g_routed="$g_routed $g_r"
    done
    # …and which rules fired, so a refusal says WHY a lane it names is required.
    g_i=1
    while [ "$g_i" -le "$g_rmax" ]; do
      case "$g_hit" in
        *" $g_i "*)
          g_gl=""
          for g_t in $g_rpaths; do case "$g_t" in "$g_i":*) g_gl="$g_gl ${g_t#*:}" ;; esac; done
          g_rfired="$g_rfired rule $g_i (paths:$g_gl);" ;;
      esac
      g_i=$((g_i+1))
    done
    [ -z "$g_routed" ] || g_rnote=" This PR's changed files matched path routing$g_rfired so reviewer(s)$g_routed are required on top of the gate's own list."
  fi
  g_pass=0; g_out=""; g_bad=""; g_stale=""
  for g_r in $g_revs; do
    g_vf="$ORX_GD/verdicts/pr-$num/$g_r"
    g_v=""; g_vh=""
    if [ -f "$g_vf" ]; then
      g_v=$(head -n1 "$g_vf" 2>/dev/null)                  # line 1: the verdict word
      g_vh=$(head -n2 "$g_vf" 2>/dev/null | tail -n1)      # line 2: the head it reviewed
    fi
    case "$g_v" in
      # A pass counts ONLY for the revision it reviewed. Recorded against an older
      # head (or against none) → stale: the branch moved, and what that reviewer
      # approved is not what would merge.
      pass)          if [ "$g_vh" = "$cur_head" ]; then g_pass=$((g_pass+1)); else g_stale="$g_stale $g_r"; fi ;;
      # A blocking verdict is revision-INDEPENDENT: "this PR has a defect" does not
      # stop being true because the author pushed more code. Re-review clears it.
      fail|escalate) g_bad="$g_bad $g_r" ;;
      # No verdict recorded — or one this build cannot read (a hand-edited `PASS`,
      # say), which is NOT a pass. The Rust `Verdict::parse` is lowercase-strict for
      # exactly this reason: one token definition, and both halves fail closed on it.
      *)             g_out="$g_out $g_r" ;;
    esac
  done
  # Blockers beat approvals (#197 A.3): one fail/escalate refuses the merge whatever
  # the others recorded and whatever the threshold says. Checked before any counting.
  # `$g_rnote` (#1176) rides this refusal too: a reviewer the gate's own
  # `reviewers:` never names is otherwise a mystery to whoever reads it, and that
  # is as true of a lane that BLOCKED as of one that has not voted.
  [ -z "$g_bad" ] || loomux_block_wf "verdict-blocks" "reviewer(s)$g_bad recorded a fail/escalate verdict.$g_rnote"
  # Say only what is TRUE: a gate held up purely by stale verdicts must not also claim
  # it is waiting on a verdict from nobody, and vice versa. A refusal message is the
  # only thing the agent reading it has to act on.
  g_why=""
  [ -n "$g_out" ] && g_why="no verdict yet from reviewer(s)$g_out"
  if [ -n "$g_stale" ]; then
    [ -n "$g_why" ] && g_why="$g_why; "
    g_why="${g_why}reviewer(s)$g_stale passed an EARLIER revision and must re-review"
  fi
  case "$g_req" in
    threshold)
      [ "$g_pass" -ge "$g_thr" ] || loomux_block_wf "below-threshold" "only $g_pass of the required $g_thr PASS verdicts cover the PR's current head $cur_head — $g_why" ;;
    *)
      # all-pass — THE #151 CASE: a reviewer that has not recorded anything (or whose
      # pass predates the code that would merge) keeps the gate shut, however loudly
      # the others approved.
      # `$g_rnote` (#1176) names the routing rules that fired and the lanes they
      # added, so a refusal naming a reviewer the gate's own `reviewers:` never
      # mentions is not a mystery to whoever reads it.
      [ -z "$g_why" ] || loomux_block_wf "verdict-outstanding" "the PR is now at $cur_head — $g_why.$g_rnote" ;;
  esac
  # `also:` conditions. ci-green is checked against the real gh; anything this build
  # does not know how to check FAILS CLOSED — a clause orrerix silently ignored would
  # make a stricter-looking workflow file a weaker one, the worst thing a gate can do.
  for g_c in $g_also; do
    case "$g_c" in
      ci-green)
        if ! "$REAL_GH" pr checks $rf "$num" >/dev/null 2>&1; then
          # A non-zero `pr checks` is three different facts: a check failed, one
          # is still pending, or — right after the base moved (#2943) — GitHub is
          # still RECOMPUTING mergeability and `mergeStateStatus` reads UNKNOWN.
          # Only the recomputation is retry: ask gh which of the three it is. The
          # first read is poll 0; each UNKNOWN buys one more read, up to 3, spaced
          # 20 s apart. ORRERIX_MSS_POLL_SECS exists so a test does not sleep out
          # the real interval; nothing else sets or reads it.
          g_mss_tries=0
          while :; do
            g_mss=$("$REAL_GH" pr view $rf "$num" --json mergeStateStatus --jq .mergeStateStatus 2>/dev/null)
            case "$g_mss" in
              # GitHub settled and calls the PR mergeable — the checks "failure"
              # was the recomputation, so the gate proceeds to its remaining arms.
              # CLEAN reached WITHOUT a poll (checks non-zero, state already
              # settled) is the no-checks-reported case and refuses as before: a
              # gate asking for green CI is not satisfied by an absent check.
              CLEAN)
                if [ "$g_mss_tries" -gt 0 ]; then break; fi
                loomux_block_wf "ci-not-green" "the gate requires ci-green and 'gh pr checks $num' is not all-green (failing, still running, or no checks reported)" ;;
              UNKNOWN)
                if [ "$g_mss_tries" -ge 3 ]; then
                  loomux_block_wf "mergeability-unknown" "the gate requires ci-green and 'gh pr checks $num' is not green because GitHub is still computing mergeability after a base move — retry in a minute. No check has failed; the merge state is simply not computed yet"
                fi
                g_mss_tries=$((g_mss_tries+1))
                sleep "${ORRERIX_MSS_POLL_SECS:-20}" ;;
              # A genuinely failing check — or a merge state gh cannot read
              # (DIRTY, BLOCKED, BEHIND, an empty answer): unknown is never
              # treated as green here either.
              *) loomux_block_wf "ci-not-green" "the gate requires ci-green and 'gh pr checks $num' is not all-green (failing, still running, or no checks reported)" ;;
            esac
          done
        fi ;;
      # #565: the head oid pins the CODE a verdict reviewed. It does not pin the PR
      # BODY — which a squash merge turns into the permanent commit message, so a
      # `pass` recorded against one body and merged with another lands text no
      # reviewer read. Opt-in, because that is only true of repos that squash.
      #
      # ASYMMETRIC, and that is the whole design: only PASSES are checked here. A
      # fail/escalate whose body moved afterwards is the fix loop working as
      # intended, and re-staling it would ping-pong forever — body finding → worker
      # fixes the body → verdict auto-stales → re-review → repeat. The Rust half
      # REPORTS that side (`list_verdicts`, the gate status line) so the orchestrator
      # can spot an already-fixed finding; nothing acts on it automatically.
      body-unchanged)
        b_raw=$("$REAL_GH" pr view $rf "$num" --json body --jq .body 2>/dev/null) \
          || loomux_block_wf "unresolved-body" "the gate requires body-unchanged and orrerix could not read the PR's body, so it cannot tell whether what would become the squash commit message is what the reviewers passed"
        # The canonical form both halves digest, and ALL of it: strip CR (a CRLF and
        # an LF body are the same commit message), then exactly one trailing newline
        # — `$(…)` ate them all, `printf '%s\n'` puts one back. Kept this small on
        # purpose: every extra rule is one the Rust half (`workflow::canonical_body`)
        # and this shell would have to keep agreeing about forever.
        b_norm=$(printf '%s' "$b_raw" | tr -d '\r')
        loomux_norm_guard "$b_raw" "$b_norm" "the PR body"
        b_now=$(printf '%s\n' "$b_norm" | loomux_sha256)
        [ -n "$b_now" ] || loomux_block_wf "no-sha256" "the gate requires body-unchanged but this host has no usable sha256 tool (sha256sum, shasum or openssl), so orrerix cannot compare the PR body against the one the reviewers passed — a condition it cannot check refuses the merge"
        # #2168 E2, PASS 1: has one of the gate's OWN reviewers recorded a
        # body-VERIFICATION pass covering the body as it stands? That is line 5
        # reading `<digest> verified-body`, on a live pass — a marker only the
        # `review_verdict` tool writes, and only for a lane the review driver
        # briefed BECAUSE every required lane had already passed the code at
        # this head and only the body had moved. A reviewer cannot type it: line
        # 5 is the tool's, and everything a reviewer writes lands on line 6 and
        # below.
        #
        # Two passes rather than one, because the answer is a property of the
        # whole reviewer SET and pass 2 asks it of each member: deciding it
        # inside one loop would make it depend on the order `$g_revs` happens to
        # be in.
        b_verified=0
        for b_r in $g_revs; do
          b_vf="$ORX_GD/verdicts/pr-$num/$b_r"
          [ -f "$b_vf" ] || continue
          [ "$(head -n1 "$b_vf" 2>/dev/null)" = "pass" ] || continue
          [ "$(head -n2 "$b_vf" 2>/dev/null | tail -n1)" = "$cur_head" ] || continue
          loomux_verdict_line5 "$b_vf"
          if [ "$v_mark" = "1" ] && [ -n "$v_digest" ] && [ "$v_digest" = "$b_now" ]; then
            b_verified=1
          fi
        done
        b_bad=""
        for b_r in $g_revs; do
          b_vf="$ORX_GD/verdicts/pr-$num/$b_r"
          [ -f "$b_vf" ] || continue
          # Only a LIVE pass — the verdict word on line 1, recorded against the head
          # that would merge. (A pass the threshold does not need is still checked:
          # "this reviewer approved a different commit message" is true either way,
          # and re-recording is the same action whether or not it was load-bearing.)
          [ "$(head -n1 "$b_vf" 2>/dev/null)" = "pass" ] || continue
          [ "$(head -n2 "$b_vf" 2>/dev/null | tail -n1)" = "$cur_head" ] || continue
          # Line 5 is the body digest it reviewed, read through the one helper
          # that reproduces `parse_verdict_file` + `sanitize_digest`. Absent (a
          # verdict recorded before #565, or one whose body gh could not read)
          # reads as EMPTY, which equals no digest and so refuses — unknown is
          # never "unbound, therefore fine".
          loomux_verdict_line5 "$b_vf"
          b_vd=$v_digest
          if [ -n "$b_vd" ] && [ "$b_vd" = "$b_now" ]; then
            continue
          fi
          # The delegation. This pass is at an EARLIER body, and it is accepted
          # only because a required reviewer verified the body as it stands while
          # this pass stayed bound to the head that would merge — so the code it
          # approved has not moved.
          #
          # **A pass with no READABLE digest is refused** (#2308 reviews 4 and 5).
          # A verdict file written before #565 has SUMMARY PROSE on line 5, and
          # two successive approximations of `sanitize_digest` let one through:
          # first "non-empty", then "the first field is 64 lowercase hex" — which
          # a prose line beginning with a digest satisfies. `v_digest` is the
          # helper's answer over the whole field, so the two halves now decide
          # this from one rule. Agreement is executed, not asserted:
          # `the_shim_and_the_gate_agree_about_which_passes_a_verification_covers`.
          if [ "$b_verified" = "1" ] && [ -n "$b_vd" ]; then
            continue
          fi
          b_bad="$b_bad $b_r"
        done
        [ -z "$b_bad" ] || loomux_block_wf "body-changed" "the PR body is not the one reviewer(s)$b_bad passed — and this repo squash-merges, so that body becomes the permanent commit message. Whoever edited it, the fix is the same: those reviewers re-read the body as it stands and re-record" ;;
      # #1174 STOP THE LINE. `ci-green` asks about THIS PR; `base-green` asks about
      # the branch it would land on. Merging onto a base whose HEAD is already red
      # compounds failures and hands the merge queue's bisect a culprit set it cannot
      # untangle — so the fleet stops until the base is fixed, which is the whole of
      # trunk-based "fix the build first".
      #
      # TWO endpoints, because ONE would be a lie on half of GitHub: the combined
      # status API sees only the legacy Status API, and the check-runs API sees only
      # check runs (GitHub Actions). A repo using either alone reports "none" from
      # the other, so "green" means neither said anything bad AND at least one of them
      # said something at all. Zero signal from BOTH is UNKNOWN, and unknown refuses —
      # the same call `ci-green` makes on a PR with no checks reported, and the same
      # posture the merge queue's `base-unverifiable` takes.
      base-green)
        # `gh api` has no -R: its {owner}/{repo} placeholders resolve from the CWD's
        # remote, which is the WRONG repo whenever the merge was invoked with -R. So
        # the repo is resolved explicitly, through the one flag `gh repo view` accepts
        # (a positional, per #294), and an unresolvable one refuses.
        bg_nwo=$("$REAL_GH" repo view $repo --json nameWithOwner --jq .nameWithOwner 2>/dev/null)
        case "$bg_nwo" in
          ''|*[!A-Za-z0-9._/-]*) loomux_block_wf "base-unverifiable" "the gate requires base-green and orrerix could not resolve which repository this PR belongs to, so it cannot read the base branch's checks" ;;
        esac
        # ONE definition of each reduction, interpolated from `workflow.rs` — see
        # BASE_CHECK_RUNS_JQ. The first cut kept a COPY here; the two were
        # byte-identical, and both were wrong in the same way (#1181 rev-lead),
        # which is precisely the failure a copy cannot surface.
        #
        # `per_page=100` is the API maximum and only a MITIGATION: the reduction's
        # own total_count clause is what stops a truncated page reading green.
        bg_runs=$("$REAL_GH" api "repos/$bg_nwo/commits/$base/check-runs?per_page=100" --jq '__BASE_CHECK_RUNS_JQ__' 2>/dev/null)
        bg_stat=$("$REAL_GH" api "repos/$bg_nwo/commits/$base/status" --jq '__BASE_STATUS_JQ__' 2>/dev/null)
        case "$bg_runs" in none|pending|red|green|truncated) : ;; *) loomux_block_wf "base-unverifiable" "the gate requires base-green and orrerix could not read the check runs on '$base' (its HEAD), so it cannot tell whether the branch this PR would land on is healthy — unknown is never treated as green" ;; esac
        case "$bg_stat" in none|pending|red|green|truncated) : ;; *) loomux_block_wf "base-unverifiable" "the gate requires base-green and orrerix could not read the commit statuses on '$base' (its HEAD), so it cannot tell whether the branch this PR would land on is healthy — unknown is never treated as green" ;; esac
        # Compared word by word, never as a concatenation (#1181 rev-lead NB4). The
        # concatenated form happened to be right for every pair in today's
        # vocabulary, but it was right by an argument nobody had written down and
        # one new word away from being wrong — and this round added a word.
        # Ordered worst-answer-first, mirroring `base_ci_green` arm for arm.
        if [ "$bg_runs" = "red" ] || [ "$bg_stat" = "red" ]; then
          loomux_block_wf "base-not-green" "the gate requires base-green and the HEAD of '$base' is RED. Fix the base branch first — piling more work onto a broken branch is what this clause exists to stop"
        elif [ "$bg_runs" = "truncated" ] || [ "$bg_stat" = "truncated" ]; then
          loomux_block_wf "base-unverifiable" "the gate requires base-green and orrerix cannot account for all of the checks on the HEAD of '$base' — either it reports more check runs than one API page carries, or a payload arrived without the field that says how many there are. Either way orrerix will not guess about the checks it cannot see. Re-run the merge once the run count settles; if this base permanently carries more than 100 checks, base-green cannot be enforced for it and should not be declared"
        elif [ "$bg_runs" = "pending" ] || [ "$bg_stat" = "pending" ]; then
          loomux_block_wf "base-not-green" "the gate requires base-green and the checks on the HEAD of '$base' have not finished. Wait for them: a base whose result is not in yet is not a base known to be green"
        elif [ "$bg_runs" = "none" ] && [ "$bg_stat" = "none" ]; then
          loomux_block_wf "base-not-green" "the gate requires base-green and the HEAD of '$base' reports no checks or statuses at all, so orrerix cannot tell whether it is healthy — unknown is never treated as green. If this repo's CI legitimately skips some commits, do not declare base-green"
        fi ;;
      *) loomux_block_wf "unknown-condition" "the gate names the condition '$g_c', which this orrerix build does not know how to check — an unknown condition fails closed. Remove it from gates.merge.also, or upgrade orrerix" ;;
    esac
  done
  set +f
  loomux_audit "merge-gate-workflow-ok" "{\"pr\":\"$num\",\"require\":\"$g_req\",\"passes\":$g_pass,\"head\":\"$cur_head\"}"
fi

if [ "$base" != "$default" ]; then
  exec "$REAL_GH" "$@"   # integration-branch merge — untouched by the HUMAN gate
fi
# base == default: blanket-allowed while autonomous + auto_merge.
if [ -n "$ORX_GD" ] && [ -f "$ORX_GD/autonomous" ] && [ -f "$ORX_GD/auto_merge" ]; then
  loomux_audit "merge-gate-allowed" "{\"base\":\"$default\",\"pr\":\"$num\"}"
  exec "$REAL_GH" "$@"
fi
# Supervised dangerous mode: the human is present and enabled it (only valid while
# NOT autonomous). Distinct audit marker.
if [ -n "$ORX_GD" ] && [ -f "$ORX_GD/dangerous_mode" ] && [ ! -f "$ORX_GD/autonomous" ]; then
  loomux_audit "merge-gate-dangerous" "{\"base\":\"$default\",\"pr\":\"$num\"}"
  exec "$REAL_GH" "$@"
fi
# Otherwise: a one-time human grant for THIS pr authorizes exactly one merge.
# #256: CLAIM the grant (see loomux_grant_claim) and only spend it once the
# real merge has actually gone through — a merge GitHub refuses (draft,
# branch protection, a just-gone-stale head, a transient API error) must
# leave the grant usable for a retry, not burn it on a merge that never
# happened. This is why the exec above (blanket/dangerous) never needed this:
# those openings aren't a one-time resource, so nothing to protect on failure.
gf=""
[ -n "$ORX_GD" ] && [ -n "$num" ] && gf="$ORX_GD/merge_grants/pr-$num"
_grant_claimed=""
if [ -n "$gf" ] && loomux_grant_claim "$gf"; then
  loomux_audit "merge-gate-granted" "{\"base\":\"$default\",\"pr\":\"$num\"}"
  "$REAL_GH" "$@"
  rc=$?
  loomux_grant_settle "$gf" "$_grant_claimed" "$rc"
  exit "$rc"
fi
loomux_block "gate-closed" "$default" "$num"
"#;
    // Normalize to LF: the raw-string newlines follow this source file's line
    // endings, which git may check out as CRLF on Windows — but a CRLF `#!/bin/sh`
    // script is broken under POSIX sh. The `.cmd` wrapper (which needs CRLF) is
    // built separately with explicit `\r\n`.
    // #1174/#1181: the `base-green` reductions are ONE definition (workflow.rs),
    // interpolated here and passed to `gh --jq` by the merge queue — so the shim
    // and the queue cannot ask GitHub different questions. Both are single-quoted
    // in the template above and neither contains a `'`, which is what makes a
    // plain substitution safe; the test below asserts that, so a future edit that
    // introduces one is red rather than a broken shim.
    // #1176 rides the same arrangement: `ROUTING_FILES_JQ` is the ONE definition
    // of "which files did this PR change, and can we account for all of them",
    // read here by the shim and by `mqdriver::pr_files_argv` in the queue.
    TPL.replace("__BASE_CHECK_RUNS_JQ__", workflow::BASE_CHECK_RUNS_JQ)
        .replace("__BASE_STATUS_JQ__", workflow::BASE_STATUS_JQ)
        .replace("__ROUTING_FILES_JQ__", workflow::ROUTING_FILES_JQ)
        .replace("__ROUTING_RULES_MAX__", &workflow::ROUTING_RULES_MAX.to_string())
        .replace("__ROUTING_PATHS_MAX__", &workflow::ROUTING_PATHS_MAX.to_string())
        .replace("__REAL_GH__", real_gh)
        .replace("__DEPS_PREAMBLE__\n", &shim_deps_preamble(paths.utils_dir.as_deref()))
        .replace("__RELEASE_GRANT_VALID__\n", RELEASE_GRANT_VALID_SH)
        .replace("__GIT_PLUMBING__\n", &gh_shim_git_plumbing(paths.git_dir.as_deref()))
        .replace("__CLOSE_GATE__\n", &gh_shim_close_gate())
        // #2985 rev-std finding 1: the shell scanner's value-flag arms are
        // GENERATED from `GH_VALUE_FLAGS`, never retyped, so the two cannot
        // diverge the way `-c`/`--comment` did.
        .replace("__VF_SEP__", &gh_shim_value_flag_arms().0)
        .replace("__VF_GLUED__", &gh_shim_value_flag_arms().1)
        .replace("\r\n", "\n")
}

/// The gh subcommands whose PATH is adjusted so the real gh's own `git` calls
/// reach a native `git.exe` (#509) — see `gh_shim_git_plumbing`.
///
/// Every entry is a **built-in** `gh` command (verified against `gh --help`,
/// gh 2.95.0). That is the safety property, not a detail: gh refuses to let a
/// user alias or an extension shadow a built-in, so a token in this list can
/// never be agent-authored code. Anything NOT listed — an alias (`gh myalias`,
/// which for a `!`-alias runs a shell), a `gh extension` invocation, or simply
/// a gh command newer than this list — keeps the gated `git` on PATH. A gh
/// version that grows a new git-using command therefore degrades to the old
/// broken-argument behavior rather than opening anything: the fail direction
/// this list is chosen for.
const GH_GIT_PLUMBING_CMDS: &[&str] =
    &["api", "auth", "browse", "gist", "issue", "pr", "release", "repo", "run", "status", "workflow"];

/// Hand the real `gh` a PATH whose `git` is the real `git.exe` (#509), for the
/// built-in subcommands that shell out to git.
///
/// **Why this is needed at all.** `gh pr create|status|…` runs `git` with
/// arguments like `--get-regexp ^branch\.<b>\.(remote|merge)$`. With the shim
/// dir first on PATH that resolves to our `git.cmd`, and Windows can only run a
/// `.cmd` through a `cmd.exe /c` layer — which re-parses the command line, so
/// the unquoted `|` splits it and gh dies with
/// `failed to run git: 'merge' is not recognized`. **No batch quoting can fix
/// that**: the mangling happens in the `cmd.exe` the *caller* spawns, before
/// the `.cmd`'s first line runs (measured in the #509 PR against `%*`, a
/// `set "ARGS=%*"` + delayed-expansion capture, and a per-argument `%~1`
/// re-quoting loop — all three fail identically, while the same argument
/// reaches a native `.exe` intact). A shim that breaks `gh pr create` pushes
/// agents onto raw `gh api`, which is the exact route the shim exists to
/// intercept (#196) — a worse security outcome than the one this costs.
///
/// **What it costs.** For those subcommands the real gh's *internal* git calls
/// are not tag-gated. gh has no command that pushes a tag (`gh release create`
/// creates the tag through the API, and is gated by this same shim), so the
/// residual is a third-party program gh runs on the agent's behalf — which is
/// why aliases and extensions are excluded above. See
/// `docs/design/shim-path-integrity.md` and workflows.md's "The bypass surface,
/// honestly".
/// Resolve everything the shims need baked in, from ONE walk of this machine's
/// Git for Windows install layout: the absolute `sh.exe` for the `.cmd`
/// delegator (#335), and the `ShimPaths` for the POSIX bodies (#509).
///
/// `#[doc(hidden)] pub` so the integration tests can build a shim exactly the
/// way `ensure_shims` does — a test that hand-assembled its own paths would
/// stop pinning what actually ships.
///
/// Off Windows there is no `.cmd` layer and the coreutils are simply on PATH,
/// so both come back empty; the shims' own dependency self-check still covers
/// the case where they are not.
#[doc(hidden)]
pub fn resolve_shim_toolchain() -> (Option<String>, ShimPaths) {
    #[cfg(target_os = "windows")]
    {
        let (path, pathext) = (crate::winpath::launch_path(), crate::winpath::launch_pathext());
        let real_git = crate::winpath::resolve_program("git", &path, &pathext);
        let sh = real_git
            .as_ref()
            .and_then(|git| crate::winpath::resolve_sh(git, &path, &pathext));
        let utils = sh
            .as_ref()
            .and_then(|sh| crate::winpath::resolve_utils_dir(sh, &path, &pathext));
        let paths = ShimPaths {
            utils_dir: utils.as_deref().map(crate::winpath::to_msys_dir),
            git_dir: real_git.as_ref().and_then(|g| g.parent()).map(crate::winpath::to_msys_dir),
        };
        (sh.map(|p| p.to_string_lossy().replace('\\', "/")), paths)
    }
    #[cfg(not(target_os = "windows"))]
    {
        (None, ShimPaths::default())
    }
}

fn gh_shim_git_plumbing(git_dir: Option<&str>) -> String {
    let Some(dir) = git_dir else { return String::new() };
    format!(
        "# ── The real gh's own git plumbing (#509) ────────────────────────────────────\n\
         # `gh pr create` runs `git config --get-regexp ^branch\\.<b>\\.(remote|merge)$`.\n\
         # Routed through our `git.cmd`, cmd.exe re-parses that line and the unquoted\n\
         # `|` splits it: \"failed to run git: 'merge' is not recognized\". The mangling\n\
         # is in the caller's cmd.exe, so the `.cmd` cannot quote its way out — only a\n\
         # native `git.exe` receives the argument intact. Restricted to gh BUILT-IN\n\
         # commands (an alias or extension can never shadow one), so `gh <alias>` and\n\
         # `gh ext …` keep the gated git. See docs/design/shim-path-integrity.md.\n\
         case \"$cmd\" in\n\
         \x20 {arms}) PATH=\"{dir}:$PATH\"; export PATH ;;\n\
         esac\n",
        arms = GH_GIT_PLUMBING_CMDS.join("|"),
        dir = sh_dq_escape(dir),
    )
}

/// The Windows `gh.cmd` wrapper: delegates to the POSIX shim (single source of
/// gate logic) via an ABSOLUTE `sh.exe` path baked in at shim-write time (#335),
/// or runs the real gh — loudly audited as degraded, never a silent bypass —
/// when no `sh` could be found anywhere on the machine. `real_gh` is
/// forward-slashed but valid for `CreateProcess`.
#[doc(hidden)] // pub so the integration test can pin the security-critical routing
pub fn gh_shim_cmd(real_gh: &str, sh_path: Option<&str>) -> String {
    shim_cmd_delegator("gh", &real_gh.replace('/', "\\"), sh_path)
}

/// Shared `.cmd` delegator shape for the `gh`/`git` interceptor shims (#83,
/// #335). `sh_path`, when `Some`, is the ABSOLUTE `sh.exe` path resolved at
/// shim-write time from `sh`'s actual install location (see
/// `winpath::resolve_sh`) — baking it in means the delegator never depends on
/// the *invoking* shell's PATH containing `sh.exe` (a default Git for Windows
/// install leaves `usr\bin` off PATH, which silently defeated the gate for
/// any PowerShell/cmd invocation before #335). `sh_path` is `None` only when
/// shim-write time genuinely found no `sh` anywhere on the machine
/// (`OrchRegistry::ensure_shims`) — that fallback to the real binary is
/// audited loudly (`gate-degraded-no-sh`) rather than silently skipping the
/// gate.
///
/// **A limit this file cannot fix, stated so nobody re-derives it (#509).**
/// `%*` below forwards the caller's arguments, and cmd.exe re-parses them for
/// `| & < > ^` *after* expanding them — so an argument carrying an unquoted
/// metacharacter (gh's own
/// `git config --get-regexp ^branch\.<b>\.(remote|merge)$`) is split into
/// commands and the invocation dies with `'merge' is not recognized`. There is
/// **no batch quoting that avoids this**: `%*`, `set "ARGS=%*"` +
/// delayed-expansion, and a per-argument `%~1` re-quoting loop were all
/// measured against that exact argument and fail identically, because the split
/// happens in the `cmd.exe /c` layer Windows requires to run a `.cmd` at
/// all — before this file's first line executes. The same argument reaches a
/// native `.exe` intact, which is why the fix for gh's internal git calls is to
/// route them to the real `git.exe` (`gh_shim_git_plumbing`) rather than to
/// quote harder here. A gh/git invocation an *agent* types with an unquoted
/// metacharacter is still mangled; it fails loudly and is not a gate hole.
fn shim_cmd_delegator(program: &str, real_bs: &str, sh_path: Option<&str>) -> String {
    let set_sh = match sh_path {
        Some(p) => format!("set \"ORRERIX_SH={}\"\r\n", p.replace('/', "\\")),
        None => String::new(),
    };
    // `goto`, not a nested `if (...) exit /b %errorlevel%` — inside a
    // parenthesized block cmd.exe expands `%errorlevel%` once at PARSE time
    // (before the guarded command even runs), so a naive `if defined
    // ORRERIX_SH ( "%ORRERIX_SH%" ... & exit /b %errorlevel% )` would always
    // exit with the *pre-block* errorlevel — silently turning a gate refusal
    // (real exit 1) back into success. `exit /b %errorlevel%` on its own
    // top-level line, outside any parens, expands fresh at execution time.
    //
    // The unconditional `set "ORRERIX_SH="` below is load-bearing (review on
    // #335): `setlocal` only makes changes *revertible*, it does not clear
    // variables the invoking shell already exported. Without clearing it
    // first, an INHERITED `ORRERIX_SH` (accidental or adversarial) would
    // satisfy `if not defined ORRERIX_SH` in the degraded (no `sh` resolved)
    // case and route through whatever binary that variable named — an
    // untrusted substitute for the baked-in path, defeating the very
    // "never a silent bypass" guarantee this delegator exists for. Clearing
    // first means only the `set` line this function itself emits (when
    // `sh_path` is `Some`) can ever populate it.
    //
    // The POSIX shim beside this file is located through `call
    // :orrerix_self_dir`, never a top-level `%~dp0` (#3477). cmd.exe expands a
    // top-level `%~dp0` against the CURRENT DIRECTORY, not the script's own,
    // when the batch was started by a QUOTED name that cmd resolved through
    // PATH (`"gh" pr merge 5`) — the exact shape npm's own `.cmd` wrappers use
    // (`"%_prog%" …` with `_prog=node`), so every npm script calling a shimmed
    // program handed sh `<cwd>\<program>`. For a gate that is worse than a
    // `No such file`: a file of that name in the agent's own worktree would
    // run IN PLACE OF the gate. Inside a `call`ed label `%~dp0` reads the
    // batch file's real path — the same workaround npm's cmd-shim ships
    // (`:find_dp0`). The variable is assigned unconditionally, so an
    // inherited value can never stand in for it.
    format!(
        "@echo off\r\n\
         rem loomux {program} shim (#83) — delegate to the POSIX shim using an\r\n\
         rem absolute sh path baked in at shim-write time (#335): the invoking\r\n\
         rem shell's PATH may not include sh.exe, so this no longer re-resolves\r\n\
         rem sh at invocation time.\r\n\
         setlocal\r\n\
         call :orrerix_self_dir\r\n\
         set \"ORRERIX_SH=\"\r\n\
         {set_sh}\
         if not defined ORRERIX_SH goto :orrerix_no_sh\r\n\
         \"%ORRERIX_SH%\" \"%ORRERIX_SHIM_DIR%{program}\" %*\r\n\
         exit /b %errorlevel%\r\n\
         \r\n\
         :orrerix_self_dir\r\n\
         rem #3477: the batch dir is only trustworthy inside a called label.\r\n\
         set \"ORRERIX_SHIM_DIR=%~dp0\"\r\n\
         exit /b 0\r\n\
         \r\n\
         :orrerix_no_sh\r\n\
         rem #335: no sh was found when this shim was generated — the merge/\r\n\
         rem release gate is DEGRADED for this call (falling straight through\r\n\
         rem to the real binary). Audit it loudly; never bypass silently.\r\n\
         if not defined ORRERIX_GROUP_DIR if defined LOOMUX_GROUP_DIR set \"ORRERIX_GROUP_DIR=%LOOMUX_GROUP_DIR%\"\r\n\
         if defined ORRERIX_GROUP_DIR (\r\n\
         \x20 >>\"%ORRERIX_GROUP_DIR%\\audit.jsonl\" echo {{\"ts_ms\":0,\"actor\":\"{program}-shim-cmd\",\"action\":\"gate-degraded-no-sh\",\"detail\":{{}}}} 2>nul\r\n\
         )\r\n\
         \"{real_bs}\" %*\r\n\
         exit /b %errorlevel%\r\n"
    )
}

/// Every name `ensure_shims` can write into the shim dir (#3477) — bare, each
/// covering its `.cmd` twin. A CONSTANT on purpose, never the set a given spawn
/// actually wrote: `gh`/`git` are written only when `resolve_program` finds the
/// real binary, and that lookup misses transiently (an upgrade uninstalls, then
/// reinstalls). Were "kept" that spawn's set, one spawn in the window would delete
/// the MERGE GATE from a dir every live pane of every group has first on PATH, and
/// each of them would reach the real `gh` ungated once it was back (#3481 B1). A
/// gate shim left for a program that is really gone shadows nothing, so keeping it
/// costs nothing. "Kept" is THIS build's set: a build that adds a shim name must
/// add it here, and an older build running beside it will still prune that name
/// on its own spawns (both share `%APPDATA%orrerixghshim`).
pub const GENERATED_SHIM_NAMES: [&str; 4] = ["gh", "git", "orrerix", "loomux"];

/// Whether a file found in the shared shim dir is a STALE product-generated shim
/// that `ensure_shims` should delete (#3477): its name is not in
/// [`GENERATED_SHIM_NAMES`] (a `.cmd` twin counts as its bare name), and one of its
/// first four lines is a comment carrying the product's own shim header — `#` or
/// `rem`, then a brand name (current or legacy), then `shim (#`, which is how every
/// shim this product has ever generated opens (`# orrerix gh shim (#83)`,
/// `rem loomux resource-guard shim (#318)`).
///
/// Why it exists: the shim dir is prepended to every agent pane's PATH, so ANY
/// file left there shadows the real program of that name in every pane, forever.
/// A build that once wrote `node`/`npm`/`cargo` shims (#322, closed unmerged — see
/// `docs/design/lock-resources.md`) left them behind, and nothing removed them;
/// they broke `npm run` in every pane. The marker gate is the fail-safe direction:
/// a file this product did not write is never deleted, whatever its name, so the
/// worst case of a missed orphan is today's behaviour, never a lost user file.
#[doc(hidden)] // pub so the integration test can pin the pruning rule
pub fn is_stale_generated_shim(file_name: &str, head: &str) -> bool {
    let bare = file_name.strip_suffix(".cmd").unwrap_or(file_name);
    if GENERATED_SHIM_NAMES.contains(&bare) {
        return false;
    }
    head.lines().take(4).any(|line| {
        let l = line.trim_start();
        let rest = if let Some(r) = l.strip_prefix('#') {
            r
        } else if l.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("rem ")) {
            &l[4..]
        } else {
            return false;
        };
        let rest = rest.trim_start();
        [brand::NAME, brand::LEGACY_NAME].iter().any(|b| {
            rest.strip_prefix(*b)
                .is_some_and(|after| after.starts_with(' ') && after.contains("shim (#"))
        })
    })
}

/// Delete every stale product-generated shim in `dir` (#3477) — the I/O half of
/// `is_stale_generated_shim`, which carries the rule and its argument. Reads at
/// most 512 bytes of each regular file (every shim header sits in its first
/// lines); a file it cannot read or delete is left alone, best-effort like every
/// other write in `ensure_shims`.
#[doc(hidden)] // pub so the integration test can drive the real deletion
pub fn prune_stale_shims(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let mut head = Vec::new();
        let Ok(f) = fs::File::open(entry.path()) else {
            continue;
        };
        if f.take(512).read_to_end(&mut head).is_err() {
            continue;
        }
        if is_stale_generated_shim(&name, &String::from_utf8_lossy(&head)) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The POSIX `git` shim (#83): gates a `git push` that publishes a TAG (a `v*`
/// tag push triggers `release.yml` → GitHub release + npm), requiring an explicit
/// release grant. Local `git tag` is harmless — only the push reaches the world —
/// so only `git push` is inspected, and only when it targets a tag; every other
/// git call (including a plain branch push) `exec`s the real git with no extra
/// work. Mirrors the pure `git_tag_push` spec.
#[doc(hidden)] // pub so the integration test can pin the guards
pub fn git_shim_sh(real_git: &str, paths: &ShimPaths) -> String {
    const TPL: &str = r#"#!/bin/sh
# orrerix git shim (#83) — gate release/tag pushes. Generated by orrerix; do not edit.
REAL_GIT="__REAL_GIT__"
# #1153 phase 3: the pane exports both spellings during the transition
# (`agent_pane_env`). Resolved once, here, so every read below is one
# variable that cannot disagree with another read further down.
ORX_GD="${ORRERIX_GROUP_DIR:-$LOOMUX_GROUP_DIR}"
ORX_AID="${ORRERIX_AGENT_ID:-$LOOMUX_AGENT_ID}"

loomux_audit() { # $1=action $2=detail-json
  # Same portable form as the self-launch shim (#3202):
  # BSD date (macOS) has no %N, and does not fail on it: `+%s%3N` returns the
  # epoch with a literal `3N` glued on, which lands in ts_ms and makes the whole
  # line unparseable JSON. Emptiness is not the only bad answer — and neither
  # is magnitude: a date that answers %s%3N with plain SECONDS (all-digit, 10
  # digits) passes an all-digit check a thousandfold too small (#3249), so take
  # a 13-digit all-digit result or nothing and refuse every other all-digit
  # magnitude outright (ts=0) — a value that already misbehaved is not
  # re-consulted; the whole-seconds rung answers only a non-digit or empty
  # %s%3N, then to 0 — and the whole-seconds answer must itself carry the
  # right magnitude: exactly a 10-digit epoch-second value becomes ts+000;
  # every other all-digit magnitude is the same lie one rung lower and is
  # refused outright (ts=0) (#3249). Neither accept arm takes a leading
  # zero: a zero-padded answer interpolated bare (`"ts_ms":0170000000`) is
  # a leading-zero literal and no JSON parser accepts it (#3259) — so
  # every accept arm requires a non-zero leading digit. And each arm
  # spells its WHOLE accept shape (`[1-9]` then digit classes, every
  # position), so no ACCEPT arm depends on the junk arm running before
  # it (#3259); the catch-all `*)` must stay LAST — above the 13-digit
  # accept arm it would refuse every good answer to ts=0, and only the
  # happy-path pin would notice.
  ts=$(date +%s%3N 2>/dev/null)
  case "$ts" in
    *[!0-9]*|"")
      ts=$(date +%s 2>/dev/null)
      case "$ts" in
        *[!0-9]*|"") ts=0 ;;
        [1-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]) ts="${ts}000" ;;
        *) ts=0 ;;
      esac ;;
    [1-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]) ;;
    *) ts=0 ;;
  esac
  if [ -n "$ORX_GD" ]; then
    # ONE printf of the whole line — see the gh shim's note (#240): cross-process
    # append atomicity is per write syscall, and no backend mutex reaches here.
    printf '{"ts_ms":%s,"actor":"git-shim","action":"%s","detail":%s}\n' "$ts" "$1" "$2" \
      >> "$ORX_GD/audit.jsonl" 2>/dev/null || true
  fi
}
__DEPS_PREAMBLE__
loomux_block_release() { # $1=tag $2=action
  printf '%s\n' "orrerix: pushing a release tag ($1) requires an explicit human grant — a v* tag push publishes to the world (GitHub release + npm via release.yml), which autonomous mode does NOT authorize. Ask the human to grant the release; do NOT push the tag." >&2
  loomux_audit "release-gate-blocked" "{\"tag\":\"$1\",\"action\":\"$2\"}"
  exit 1
}
# The release-grant validity check — emitted from the SAME Rust const as the gh
# shim's copy (RELEASE_GRANT_VALID_SH), whose doc comment carries the full
# rationale: a release grant is a PIPELINE grant for its tag (#438), checked and
# never consumed, bounded by tag identity + TTL. Nothing here is one-time, so
# unlike the gh shim's merge gate this script needs no claim/settle at all —
# #315 ("a push git/GitHub refuses must not burn the grant") is now true because
# no push can burn it, not because a settle call remembers to hand it back.
__RELEASE_GRANT_VALID__

# Find the git subcommand, skipping value-taking globals. Non-push → exec now.
cmd=""; want=""
for tok in "$@"; do
  if [ "$want" = "1" ]; then want=""; continue; fi
  case "$tok" in
    -C|-c|--git-dir|--work-tree|--namespace|--exec-path) want="1"; continue ;;
    -*) continue ;;
    *) cmd="$tok"; break ;;
  esac
done
if [ "$cmd" != "push" ]; then
  exec "$REAL_GIT" "$@"
fi

# Bulk tag pushes can't be matched to a single-tag grant → block with guidance.
for a in "$@"; do
  case "$a" in
    --tags|--follow-tags|--mirror)
      printf '%s\n' "orrerix: a bulk tag push ($a) is not allowed — push the specific approved tag and have the human grant that release." >&2
      loomux_audit "release-gate-blocked" "{\"tag\":\"(bulk)\",\"action\":\"push $a\"}"
      exit 1 ;;
  esac
done

# Scan refspecs after `push` (skip the remote) for a tag ref.
seen=0; got_remote=0; want=""; tag=""; prevtag=0
for tok in "$@"; do
  if [ "$want" = "1" ]; then want=""; continue; fi
  case "$tok" in
    -C|-c|--git-dir|--work-tree|--namespace|--exec-path) want="1"; continue ;;
    -*) continue ;;
    *)
      if [ "$seen" = "0" ]; then [ "$tok" = "push" ] && seen=1; continue; fi
      if [ "$got_remote" = "0" ]; then got_remote=1; continue; fi
      if [ "$prevtag" = "1" ]; then tag="$tok"; break; fi
      if [ "$tok" = "tag" ]; then prevtag=1; continue; fi
      dst=${tok##*:}; dst=${dst#+}
      # Match release.yml's on.push.tags (v*) — MUST track it; a bare `v*` is only
      # a candidate, confirmed a real tag (not a same-named branch) below.
      case "$dst" in
        refs/tags/*) tag=${dst#refs/tags/}; break ;;
        v*)
          if "$REAL_GIT" rev-parse -q --verify "refs/tags/$dst" >/dev/null 2>&1; then tag="$dst"; break; fi ;;
      esac ;;
  esac
done

if [ -z "$tag" ]; then
  exec "$REAL_GIT" "$@"   # branch push — untouched
fi
# Blanket: autonomous + auto_release opt-in (parallel to the gh release path).
if [ -n "$ORX_GD" ] && [ -f "$ORX_GD/autonomous" ] && [ -f "$ORX_GD/auto_release" ]; then
  loomux_audit "release-gate-allowed" "{\"tag\":\"$tag\",\"action\":\"push\"}"
  exec "$REAL_GIT" "$@"
fi
# Supervised dangerous mode (human present, not autonomous). Distinct audit.
if [ -n "$ORX_GD" ] && [ -f "$ORX_GD/dangerous_mode" ] && [ ! -f "$ORX_GD/autonomous" ]; then
  loomux_audit "release-gate-dangerous" "{\"tag\":\"$tag\",\"action\":\"push\"}"
  exec "$REAL_GIT" "$@"
fi
# Otherwise a per-tag grant authorizes this tag push — and the rest of THAT
# tag's release pipeline (#438): the grant is checked, not spent, so the
# `gh release`/notes steps that follow this push ride the same authorization
# until it expires. It still authorizes exactly one tag.
safe=$(printf '%s' "$tag" | tr -c 'A-Za-z0-9._-' '_')
loomux_norm_guard "$tag" "$safe" "release-grant-tag"
gf=""
[ -n "$ORX_GD" ] && [ -n "$safe" ] && gf="$ORX_GD/release_grants/$safe"
if [ -n "$gf" ] && loomux_release_grant_valid "$gf"; then
  loomux_audit "release-gate-granted" "{\"tag\":\"$tag\",\"action\":\"push\"}"
  exec "$REAL_GIT" "$@"
fi
loomux_block_release "$tag" "push"
"#;
    // Normalize to LF (see gh_shim_sh) — a CRLF POSIX script is broken.
    TPL.replace("__REAL_GIT__", real_git)
        .replace("__DEPS_PREAMBLE__\n", &shim_deps_preamble(paths.utils_dir.as_deref()))
        .replace("__RELEASE_GRANT_VALID__\n", RELEASE_GRANT_VALID_SH)
        .replace("\r\n", "\n")
}

/// The Windows `git.cmd` wrapper: delegates to the POSIX git shim via an
/// ABSOLUTE `sh.exe` path baked in at shim-write time (#335), same shape and
/// same degraded-fallback audit as `gh_shim_cmd`.
#[doc(hidden)] // pub so the integration test can pin the security-critical routing
pub fn git_shim_cmd(real_git: &str, sh_path: Option<&str>) -> String {
    shim_cmd_delegator("git", &real_git.replace('/', "\\"), sh_path)
}

/// The POSIX launcher shim (#815): refuse, always. `orrerix` on an agent's PATH
/// is the npm launcher, and that launcher is an INSTALLER — plain `orrerix`
/// installs the desktop app when none is present and `orrerix update` reinstalls
/// it (#845), and either way the install runs silently. Run from an agent pane,
/// that install terminates the running app to replace it, killing every agent in
/// it mid-task — including the shell that invoked it, which is why the evidence
/// is a process that vanishes with no shutdown path and no crash report.
///
/// One body serves both spellings. #1153 phase 5 renamed the npm package and its
/// bin to `orrerix`, but a global install of the old `loomux-desktop` survives
/// that rename on PATH, so `ensure_shims` writes this script under BOTH names —
/// the script itself resolves nothing and reads no argv, so it does not care
/// which one invoked it. The function keeps its `loomux_` prefix because that is
/// the cargo-crate axis, whose LIBRARY half (`loomux_lib`) the rebrand
/// deliberately left alone even after #1562 renamed the binary to
/// `orrerix`; see docs/design/rebrand-bundle.md.
///
/// Unlike the gh/git shims this is not a gate: there is no agent use of the
/// launcher to authorize (agents reach loomux through its MCP tools), so there is
/// no grant path, no delegation, and deliberately no fallback that could run the
/// real thing. A refusal that degrades into the guarded action is not a refusal.
#[doc(hidden)] // pub so the integration test can pin the refusal
pub fn loomux_shim_sh() -> String {
    const TPL: &str = r#"#!/bin/sh
# orrerix self-launch shim (#815) — block agent-run launcher. Generated by orrerix; do not edit.
# #1153 phase 3: the pane exports both spellings during the transition
# (`agent_pane_env`). Resolved once, here, so every read below is one
# variable that cannot disagree with another read further down.
ORX_GD="${ORRERIX_GROUP_DIR:-$LOOMUX_GROUP_DIR}"
ORX_AID="${ORRERIX_AGENT_ID:-$LOOMUX_AGENT_ID}"
if [ -n "$ORX_GD" ]; then
  # BSD date (macOS) has no %N, and does not fail on it: `+%s%3N` returns the
  # epoch with a literal `3N` glued on, which lands in ts_ms and makes the whole
  # line unparseable JSON. Emptiness is not the only bad answer — and neither
  # is magnitude: a date that answers %s%3N with plain SECONDS (all-digit, 10
  # digits) passes an all-digit check a thousandfold too small (#3249), so take
  # a 13-digit all-digit result or nothing and refuse every other all-digit
  # magnitude outright (ts=0) — a value that already misbehaved is not
  # re-consulted; the whole-seconds rung answers only a non-digit or empty
  # %s%3N, then to 0 — and the whole-seconds answer must itself carry the
  # right magnitude: exactly a 10-digit epoch-second value becomes ts+000;
  # every other all-digit magnitude is the same lie one rung lower and is
  # refused outright (ts=0) (#3249). Neither accept arm takes a leading
  # zero: a zero-padded answer interpolated bare (`"ts_ms":0170000000`) is
  # a leading-zero literal and no JSON parser accepts it (#3259) — so
  # every accept arm requires a non-zero leading digit. And each arm
  # spells its WHOLE accept shape (`[1-9]` then digit classes, every
  # position), so no ACCEPT arm depends on the junk arm running before
  # it (#3259); the catch-all `*)` must stay LAST — above the 13-digit
  # accept arm it would refuse every good answer to ts=0, and only the
  # happy-path pin would notice.
  ts=$(date +%s%3N 2>/dev/null)
  case "$ts" in
    *[!0-9]*|"")
      ts=$(date +%s 2>/dev/null)
      case "$ts" in
        *[!0-9]*|"") ts=0 ;;
        [1-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]) ts="${ts}000" ;;
        *) ts=0 ;;
      esac ;;
    [1-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]) ;;
    *) ts=0 ;;
  esac
  # ONE printf of the whole line — see the gh shim's note (#240): cross-process
  # append atomicity is per write syscall, and no backend mutex reaches here.
  printf '{"ts_ms":%s,"actor":"orrerix-shim","action":"self-launch-blocked","detail":{"agent":"%s"}}\n' \
    "$ts" "$ORX_AID" >> "$ORX_GD/audit.jsonl" 2>/dev/null || true
fi
printf '%s\n' "orrerix: running the desktop launcher from an agent pane is blocked. It is an installer, not a window switcher: plain orrerix installs the desktop app when it is missing and orrerix update reinstalls it, and the silent install kills the running app — terminating this pane and every other agent mid-task. The pre-rename loomux launcher is blocked by this same shim. Orrerix is reachable from here through its MCP tools only. If the app needs restarting or updating, ask the human (message_orchestrator, or report blocked); never run it yourself." >&2
exit 1
"#;
    // Normalize to LF (see gh_shim_sh) — a CRLF POSIX script is broken.
    TPL.replace("\r\n", "\n")
}

/// The Windows `.cmd` twin — written as both `orrerix.cmd` and `loomux.cmd`: the
/// same flat refusal, self-contained. It does NOT
/// delegate through `sh` the way `gh.cmd`/`git.cmd` do — those delegate because
/// the gate logic is worth keeping in one place, and they fall through to the real
/// binary when no `sh` exists. Both reasons invert here: there is no logic to
/// share, and a fallback would run the very launcher this shim exists to stop, on
/// exactly the machines where `sh` is missing. The message avoids `( ) < > | & ^ %`
/// so cmd.exe echoes it verbatim rather than re-parsing it.
#[doc(hidden)] // pub so the integration test can pin the refusal
pub fn loomux_shim_cmd() -> String {
    "@echo off\r\n\
     rem orrerix self-launch shim (#815) — block agent-run launcher. Generated by orrerix; do not edit.\r\n\
     rem Self-contained on purpose: a refusal must never degrade into running the\r\n\
     rem real launcher, so there is no sh delegation and no fallback path.\r\n\
     setlocal\r\n\
     if not defined ORRERIX_GROUP_DIR if defined LOOMUX_GROUP_DIR set \"ORRERIX_GROUP_DIR=%LOOMUX_GROUP_DIR%\"\r\n\
     if defined ORRERIX_GROUP_DIR (\r\n\
     \x20 >>\"%ORRERIX_GROUP_DIR%\\audit.jsonl\" echo {\"ts_ms\":0,\"actor\":\"orrerix-shim-cmd\",\"action\":\"self-launch-blocked\",\"detail\":{}} 2>nul\r\n\
     )\r\n\
     >&2 echo orrerix: running the desktop launcher from an agent pane is blocked. It is an installer, not a window switcher - plain orrerix installs the app when it is missing and orrerix update reinstalls it, and the silent install kills the running app, terminating this pane and every other agent mid-task. The pre-rename loomux launcher is blocked by this same shim. Use the orrerix MCP tools; ask the human to restart or update the app.\r\n\
     exit /b 1\r\n"
        .to_string()
}
