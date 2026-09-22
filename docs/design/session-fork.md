# Forking a session (issue #3318)

From a live agent pane, open a **new** pane whose session starts as the vendor's
own fork of the source's conversation at this moment, in the source's own
directory, without disturbing the source. Slice **F1** ships that gesture for
**Claude Code alone**, on a standalone (Solo) pane; F2 adds the delegate fork and
the other CLIs, F3 adds a one-shot rejoin summary.

This note carries the survey citations F1 rests on, the argued carve-out of
[session-id-learning.md](session-id-learning.md) B3, and the one live check the
human must run before the last arm is fixed.

## Why the vendor's fork and not a file copy

The issue's first candidate was copying Claude Code's
`~/.claude/projects/<slug>/<session>.jsonl`. The vendor's own reference rules it
out, on three counts, and the survey on #3318 records each:

1. "The entry format is internal to Claude Code and changes between versions, so
   scripts that parse these files directly can break on any release."
2. The cross-project id search "resolves the ID only when exactly one other
   project holds a transcript with messages for it, so a hand-copied duplicate
   makes Claude Code report not-found rather than resume an arbitrary copy" — a
   copy under the same id is actively *refused*.
3. A copy under a new filename would still carry the old id in every entry, and
   the format being internal makes that unverifiable.

`--fork-session` exists so nobody has to do any of this. **Decision: the native
fork only, for every CLI, ever.**

What a fork loses relative to the in-session `/branch` command is per-session
permission grants — "Allow for this session" approvals are not carried when the
fork runs as a new process, so it re-asks — and the launch flags, which orrerix
already re-passes on every resume anyway. That first half is the survey's
reading of Claude Code's own reference (recorded on
[#3318's findings comment](https://github.com/willem445/orrerix/issues/3318#issuecomment-5768238045),
checked 2026-09-21), **not something loomux has observed**: constraint 3 rules
out running a real claude to confirm it. It costs nothing if wrong — a fork that
*did* carry the grants would simply prompt less — so it is stated for the
reader's expectations rather than relied on by any code, and the human's L1 run
is the cheapest place to confirm it in passing.

## The per-CLI table

`CliCaps.fork: ForkSeam` (`crates/loomux-engine/src/model.rs`) is the one place
that answers "does this vendor have a fork, and what is it spelled". Data, not a
branch (CLAUDE.md constraint 8), and `fork_refusal` is the single predicate a
gesture asks.

| CLI | F1 row | why |
| --- | --- | --- |
| **claude** | `Flag("--fork-session")` | documented: "When resuming, create a new session ID instead of reusing the original" (CLI reference, checked 2026-09-21) |
| **copilot** | `None` | no argv fork exists. `/fork` is interactive, absent from both published references, and per the issue lead it moves the *current* pane into the fork — which would invert the roster. Held behind live check L4 (#3318 F4) |
| **gemini** | `None` | documented *not* to exist: the command reference lists `/chat save\|resume\|list\|delete\|share` and `/resume`, and no fork |
| **opencode** | `None` | `--fork` **is** documented ("Fork the session when continuing") — not wired in F1 |
| **pi** | `None` | `--fork <path\|id>` **is** documented — not wired in F1, and it has an open question of its own (L2, below) |
| **codex** | `None` | `codex fork [SESSION_ID]` **is** documented — not wired in F1, and it is a *subcommand*, so its row will need a variant `ForkSeam` does not have yet |

The last three rows are the ones worth being precise about: they say "loomux has
not wired or tested it yet", not "this CLI cannot fork". F1's scope is one CLI
because the human demos and tests a fork on claude before the rest are added, and
a row claiming a spelling no test has exercised is a claim loomux could not keep.
`every_cli_row_states_its_fork_position` pins that exactly one row carries a
flag, so F2 changes that number together with the rows it adds.

## The child's id, and live check L1

A fork needs two session ids: the **parent** (what is being forked) and the
**child** (what the fork becomes). claude's reference settles the first and is
silent on the second.

`--session-id` pre-assigns an id; `--fork-session` "create[s] a new session ID
instead of reusing the original". Whether
`--session-id <new> --resume <old> --fork-session` **composes** — whether the
"new" id is the one loomux named — is documented nowhere, and CLAUDE.md
constraint 3 forbids loomux spawning a real claude to find out.

So both arms are built, and one bit chooses between them:

- **Pre-mint arm** (`CLAUDE_FORK_PREMINTS_CHILD_ID = true`, what F1 ships):
  `claude --session-id <child> --resume <parent> … --fork-session`. The child's
  id is known before it boots, so a forked pane has an exact recorded identity
  from its first turn, exactly like every other claude pane.
- **Learned arm** (`false`): `claude --resume <parent> … --fork-session`, and the
  child's id is whatever claude mints. Correct under *either* answer to L1, and
  it costs the child's identity: claude takes no session baseline today
  (`premints_session_id` is true for it), so nothing can learn that id until F2
  adds one.

**L1 is the human's to run**, and this is the one thing about F1 that needs
confirming rather than reviewing:

> Does `claude --session-id <new> --resume <old> --fork-session` produce a
> session whose id is `<new>`?

If **yes**, F1 is already correct and nothing changes. If **no**, flip
`CLAUDE_FORK_PREMINTS_CHILD_ID` to `false` (and its frontend mirror
`FORK_PREMINTS_CHILD_ID`, which a source-reading test keeps equal to it).

**That flip is a real one-line edit on both sides, and it is pinned as such.**
Every reader selects its arm by reading the constant, never by inferring one
from what a caller passed: `build_agent_command_ex` and `build_agent_argv_ex`
each read `CLAUDE_FORK_PREMINTS_CHILD_ID` inside their claude fork arm, and
`agentForkCommand` reads the mirror through `forkIdFlags`. Two tests keep that
honest —
`the_claude_fork_arm_is_selected_by_the_constant_not_by_the_caller` reads the
same constant the builder reads and requires the emitted line to agree with it
(so a builder that ignored the constant reddens **once the constant is
flipped** — at the shipped `true` the two shapes coincide byte for byte, so a
revert to caller-selection stays invisible until the flip, which is exactly when
it would do harm), and `forkIdFlags` takes the
bit as a parameter so **both** arms are executed by a test rather than only the
one the shipped value selects. Review round 1 found the first version of this
guilty of exactly the failure it warns about: the constant had no Rust reader at
all, so the documented flip was inert on the backend.

**The failure is bounded and stated rather than silent.** With L1 false and the
flag left true, the pane really *is* a fork of the right parent — that half is
documented — and only its *recorded* id is another session's. A later restart
then resumes the parent's fork-point rather than the child, and the pane's own
work since is not reachable from loomux. It is not a wrong-transcript write and
not a loss of the parent; it is a restore that lands one branch over.

Two sibling checks belong to F2 and are recorded here so they are not re-derived:
**L2** — does `pi --fork <old> --session-id <new> --session-dir <d>` compose, or
does pi refuse the pair as it already refuses `--session` beside `--session-id`?
**L3** — does `opencode --session <id> --fork` set `parent_id` on the new row?

## The one-shot rule — an argued carve-out of B3

[session-id-learning.md](session-id-learning.md) **B3** says a `--fork-session`
line must never acquire a *learned* id, and chose **exclusion** over stripping
the flag, because "overriding the human's explicit `--fork-session` intent by
silently dropping the flag felt like a second instance of the exact objection
that already bars rewriting a command line the human owns".

That argument is about a line **the human owns**. An orrerix-built fork line is
different in exactly the way that matters, and F1 carves out only that case:

- **The flag is one-shot.** It is consumed at the fork spawn. A recorded
  `--fork-session` line re-forks on *every* restart, minting a fresh session each
  boot and losing whatever the human did in that pane — which is the same harm
  B3 refused to paper over with a learned id, arriving by the other road.
- **`PersistedPane.forkOf` is the gate, and it is not the flag's presence.**
  Only loomux's own fork gesture sets it. `forkRecordCommand`
  (`src/panerestore.ts`) rewrites the record **only** for a pane that field marks,
  so a human-typed `--fork-session` line still comes back byte-identical and
  B3's exclusion continues to govern it, unchanged. That is pinned from both
  poles: `a_fork_line_never_persists_its_fork_flag` and the B3 control beside it.
- **What gets written.** With the child's id known, the record is that child's
  own plain `--resume <child>` line — no fork flag, nothing to re-fork. With no
  id (the learned arm), the fork flag *and* the parent's `--resume` are both
  dropped, leaving a fresh-session line: keeping `--resume <parent>` would be
  worse than dropping it, because two panes resumed into one session id is the
  interleaved transcript claude's docs describe, not a degraded restore.
- **It is idempotent**, which is what makes it safe on *every* capture rather
  than on a special one: a record already discharged comes back unchanged.

`forkOf` also survives the discharge as the pane's **provenance** — after the
command line is scrubbed, it is the only thing that still says this conversation
began as a copy of that one, and it is what F2's roster row will read.

## Where the pieces live

| piece | where | what |
| --- | --- | --- |
| the table | `crates/loomux-engine/src/model.rs` | `ForkSeam`, `CliCaps.fork`, `fork_refusal`, `CLAUDE_FORK_PREMINTS_CHILD_ID` |
| the backend line | `src-tauri/src/orchestration/mod.rs` | `build_agent_command_ex` / `build_agent_argv_ex` grow `fork_of: Option<&str>`; the claude arm reads the table |
| the frontend line | `src/panerestore.ts` | `agentForkCommand`, `canForkCli`, `forkPaneName`, `FORK_PREMINTS_CHILD_ID` |
| the one-shot rule | `src/panerestore.ts` + `src/pane.ts` | `forkRecordCommand`, applied in `Pane.capture` |
| the record | `src/tabstore.ts` | `PersistedPane.forkOf` (additive, blank coerces to null) |
| the gesture | `src/panemenu.ts` | `forkItem` — the eligibility matrix and every refusal's wording |
| the execution | `src/orchestration.ts` + `src/main.ts` | `forkPaneSession` → `OrchWiring.openForkedPane` |

**The backend line has no production caller in F1**, and that is deliberate
rather than an oversight: F1's gesture is a Solo pane's, which builds its line in
the frontend and never goes through `build_agent_command_ex`. The `fork_of`
parameter, the table and their tests land here because F2's `fork_session` MCP
tool is the caller, and landing the seam with the slice that argues for it is
what keeps F2 a wiring change rather than a design one.

## Public-contract changes

- **`PersistedPane.forkOf`** (`tabs.json`) — additive. Absent, blank or malformed
  decodes as `null`, i.e. "not a fork", so every pre-F1 snapshot restores exactly
  as it did.
- **`CliCaps.fork`** — an internal table, no workflow-schema key. A workflow file
  cannot ask for a fork and nothing in it parses differently.

No new dependencies. No PTY resize (a fork is one more pane through the ordinary
open path). No getrandom (the child's id is `crypto.randomUUID`, the webview's
Web Crypto, on the frontend — constraint 2 governs `src-tauri` Rust).

## Scope lines F1 draws on purpose

- **A delegate is not forkable from this menu** — no row at all, rather than a
  disabled one. Forking an orchestration delegate needs a roster row naming the
  parent, an audit row, a worktree policy, and a refusal for a pane a review or
  plan drive owns. All of that is F2; offering a greyed row for it here would
  promise a gesture that does not exist yet.
- **A lead's own pane is excluded by the same rung.** F2 makes a lead fork yield
  a *Solo* pane, never a second lead (the one-root invariant in
  [lead-pane.md](lead-pane.md)).
- **No rejoin.** Nothing merges two session files, in any harness surveyed: the
  single vendor mechanism that folds one path into another is pi's `/tree` branch
  summary, and it is intra-file. claude's docs describe cross-terminal sharing of
  one id as *interleaving*, i.e. the failure mode. F3 offers a one-shot summary
  *delivery* into the parent pane; a true transcript merge would mean writing a
  vendor's internal format, and orrerix will not.
