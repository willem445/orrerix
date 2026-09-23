# Forking a session (issue #3318)

From a live agent pane, open a **new** pane whose session starts as the vendor's
own fork of the source's conversation at this moment, in the source's own
directory, without disturbing the source. Slice **F1** shipped that gesture for
**Claude Code alone**, on a standalone (Solo) pane. Slice **F2** adds codex, pi and
opencode, the delegate fork (`fork_session` / `orch_fork_agent`), a lead's
self-fork into a Solo pane, and the two F1 residuals #3331 names. The rejoin
slice (F3) is dropped at the human's direction, and copilot stays refused (F4,
held on live check L4). Slice **F5** (#3368) names a fork as it is made and
derives its lineage for the session browser and the pane header.

This note carries the survey citations both slices rest on, the argued carve-out
of [session-id-learning.md](session-id-learning.md) B3, the live checks (L1 still
the human's; L2 and L3 answered from the vendors' own code in F2), and every
public contract the feature changed.

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

| CLI | row | the fork line | child id | why |
| --- | --- | --- | --- | --- |
| **claude** | `Flag { flag: "--fork-session", premints_child: L1 }` | the resume line + `--fork-session` | pre-minted (L1 arm) | documented: "When resuming, create a new session ID instead of reusing the original" (CLI reference, checked 2026-09-21) |
| **opencode** | `Flag { flag: "--fork", premints_child: false }` | `--session <parent> --fork` | learned by the store watcher | documented: "Fork the session when continuing (use with `--continue` or `--session`)"; read at the pinned tag `v1.18.25` (also the installed build): `cli/cmd/tui.ts` refuses `--fork` without `--session`/`--continue`, and the TUI calls `session.fork` once sync completes |
| **codex** | `Subcommand("fork")` | `fork <parent>` in `resume <id>`'s slot, last on the line | learned by the store watcher | `codex fork [SESSION_ID]` — `cli/src/main.rs` at the pin `rust-v0.153.4` (`Subcommand::Fork`, `ForkCommand`); the TUI resolves a fork's cwd through the same `resolve_cwd_for_resume_or_fork` a resume uses, so the resume line's `-C` does the same work |
| **pi** | `ParentFlag("--fork")` | `--session-id <child> --fork <parent>` | pre-minted, always | `--fork <path\|id>` ("Fork a session file or partial session ID into a new session"), and L2 answered below from the installed package |
| **copilot** | `None` | — | — | no argv fork exists. `/fork` is interactive, absent from both published references, and per the issue lead it moves the *current* pane into the fork — which would invert the roster. Held behind live check L4 (#3318 F4) |
| **gemini** | `None` | — | — | documented *not* to exist: the command reference lists `/chat save\|resume\|list\|delete\|share` and `/resume`, and no fork |

**Four variants, because the four vendors spell a fork three ways.** A flag with
no value on the resume line (claude, opencode); a subcommand in the resume
subcommand's slot (codex); a flag carrying the PARENT beside the CLI's own
open-or-create id flag naming the CHILD (pi). Each row is pinned by a test that
builds the fork line and the same CLI's resume (or, for pi, fresh) line and
asserts they differ in exactly the documented token(s) — `a_codex_fork_line_…`,
`an_opencode_fork_line_…`, `a_pi_fork_line_…` beside F1's claude test — and
`every_fork_line_tokenizes_to_its_argv_form` keeps the two spawn forms equal.

The frontend holds a mirror of the four rows (`FORK_SEAMS`, `src/panerestore.ts`),
because a Solo fork builds its line there and never reaches the backend builder.
`the frontend's fork table agrees with the engine's CliCaps rows, row by row`
(`test/panerestore.test.ts`) reads the Rust table off disk, with a population
control that every row was parsed, and requires each forkable row's token to be
on the line the frontend emits — so a row cannot move on one side alone.

### L2 and L3, answered from the vendors' own code

**L2 — does `pi --fork <old> --session-id <new> --session-dir <d>` compose?**
Yes, read from the INSTALLED package (`@earendil-works/pi-coding-agent` 0.85.1,
`dist/main.js`, per the `agent-cli-reference` skill's installed-build rule):
`validateForkFlags` refuses `--fork` beside `--session`, `--continue`, `--resume`
and `--no-session`, and NOT beside `--session-id`; `createSessionManager` then
calls `SessionManager.forkFrom(source, cwd, sessionDir, { id: sessionId })`,
which writes the child's header under exactly that id and refuses up front
("Session already exists with id …") if one already exists. So the **exact-id
arm ships** for pi and the flat-dir baseline watcher the plan designed as a
fallback is not built: pi's fork is as exact as its resume, and needs no
watcher. The parent must have a session file (pi writes one on the first
assistant response), or pi exits with "No session found" — a loud failure, not
a silent fresh session.

**L3 — does `opencode --session <id> --fork` set `parent_id` on the new row?**
No, read from `session/session.ts` at `v1.18.25`: `Session.fork` creates the
new row through `createNext` with no `parentID`, then copies the messages. So a
fork is a TOP-LEVEL session: the store watcher sees it as a new session in its
directory, and the subagent spend rollup (which follows `parent_id`) never folds
the fork's spend into its parent's.

Both are source readings, not runs — constraint 3 forbids loomux spawning a real
CLI — and both are labelled against the version read. What neither source read
settles is **opencode across directories**: a delegate fork runs in a NEW
worktree, and `validateSession` fetches the parent with the TUI's own directory
header. The group-local `OPENCODE_DB` holds the parent's row either way; whether
opencode scopes that lookup by project, and a worktree of the same repo is the
same project, is the human's to confirm in passing.

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
  it costs the child's identity: claude takes no session baseline
  (`premints_session_id` is true for it), so nothing learns that id. The human
  ran F1 and kept the pre-mint arm, so F2 did not add a claude baseline; a
  delegate fork on the learned arm records NO session rather than one the pane
  is not running under.

**L1 is the human's to run**, and this is the one thing about the claude arm
that needs confirming rather than reviewing:

> Does `claude --session-id <new> --resume <old> --fork-session` produce a
> session whose id is `<new>`?

If **yes**, nothing changes. If **no**, flip `CLAUDE_FORK_PREMINTS_CHILD_ID` to
`false` (and its frontend mirror `FORK_PREMINTS_CHILD_ID`, which a
source-reading test keeps equal to it).

**That flip is a real one-line edit on both sides, and it is pinned as such.**
Every reader selects its arm by reading the constant, never by inferring one
from what a caller passed. Since F2 the constant is carried on claude's ROW as
its seam's `premints_child`, and every backend reader asks the row: the claude
arms of `build_agent_command_ex` / `build_agent_argv_ex`, and `fork_agent`
deciding whether to mint the child an id at all. `agentForkCommand` reads the
frontend mirror through `forkIdFlags`. The tests that keep it honest —
`the_claude_fork_arm_is_selected_by_the_constant_not_by_the_caller` reads the
same constant and requires the emitted line to agree with it (and, since F2,
that claude's row carries it), so a builder that ignored the constant reddens
**once the constant is flipped** — at the shipped `true` the two shapes
coincide byte for byte, so a revert to caller-selection stays invisible until
the flip, which is exactly when it would do harm (the residual #3331 item 3
records). `forkIdFlags` takes the bit as a parameter so **both** arms are
executed by a test rather than only the one the shipped value selects.

**The failure is bounded and stated rather than silent.** With L1 false and the
flag left true, the pane really *is* a fork of the right parent — that half is
documented — and only its *recorded* id is another session's. A later restart
then resumes the parent's fork-point rather than the child, and the pane's own
work since is not reachable from loomux. It is not a wrong-transcript write and
not a loss of the parent; it is a restore that lands one branch over.

## The one-shot rule — an argued carve-out of B3

[session-id-learning.md](session-id-learning.md) **B3** says a `--fork-session`
line must never acquire a *learned* id, and chose **exclusion** over stripping
the flag, because "overriding the human's explicit `--fork-session` intent by
silently dropping the flag felt like a second instance of the exact objection
that already bars rewriting a command line the human owns".

That argument is about a line **the human owns**. An orrerix-built fork line is
different in exactly the way that matters, and the carve-out covers only that
case:

- **The fork token is one-shot.** It is consumed at the fork spawn. A recorded
  fork line re-forks on *every* restart, minting a fresh session each boot and
  losing whatever the human did in that pane — which is the same harm B3 refused
  to paper over with a learned id, arriving by the other road.
- **`PersistedPane.forkOf` is the gate, and it is not the token's presence.**
  Only loomux's own fork gesture sets it. `forkRecordCommand`
  (`src/panerestore.ts`) rewrites the record **only** for a pane that field marks,
  so a human-typed fork line still comes back byte-identical and B3's exclusion
  continues to govern it, unchanged. That is pinned from both poles:
  `a_fork_line_never_persists_its_fork_flag` and the B3 control beside it.
- **What gets written — in each CLI's own grammar (F2).** With the child's id
  known, the record is that CLI's plain resume of the CHILD: `--resume <child>`
  on claude, `--session <child>` on opencode, `--session-id <child>` on pi,
  `resume <child>` on codex — no fork token (pi's with its value), nothing to
  re-fork. With no id yet (claude's learned arm; a codex or opencode fork the
  reconciler has not matched), the fork token *and* the parent's session are both
  dropped, leaving a fresh-session line: keeping the parent's would be two panes
  resumed into one session id, which is the interleaved transcript claude's docs
  describe, not a degraded restore.
- **It is idempotent**, which is what makes it safe on *every* capture rather
  than on a special one: a record already discharged comes back unchanged.

**F2's one exception to B3's reconciler exclusion.** A codex or opencode fork
cannot be handed its child's id at open, so the frontend reconciler is the only
thing that can learn it for a Solo pane — and B3's objection does not reach it:
that pass matches the CLI's own session store and never reads an id off the
line, and `claimedSessionIds` holds every fork's PARENT out of the match (even
once the parent's pane is gone). So a pane `forkOf` marks may be a reconcile
candidate while its line still forks; a human's own fork line may not. The
argument is recorded beside B3 in [session-id-learning.md](session-id-learning.md).

`forkOf` also survives the discharge as the pane's **provenance** — after the
command line is scrubbed, it is the only thing that still says this conversation
began as a copy of that one.

## The delegate fork (F2)

`OrchRegistry::fork_agent` is the registry half of both the `fork_session` MCP
tool and the human's `orch_fork_agent` command, so an agent and a human meet the
same refusals in the same words.

**One spawn path, not two.** A fork is `spawn_agent_full` — the ordinary spawn,
under `spawn_agent_bound` as a new tier — with a `ForkSpawn` naming the parent.
The fork is read at exactly four places and nowhere else: the child's session id
(minted only where the seam's `premints_child` says the line names it), the
launch line (`fork_of`), the roster row (`forked_from`) and the audit/kickoff
pair. The cap, the spawn-rate backstop, the CLI pin, the persona, the MCP
identity and the worktree cut are the ordinary spawn's, unchanged — which is the
argument against a separate fork path: every guardrail a second path would have
to remember is one this path cannot forget.

**It inherits the source's BLOCK**, and therefore its persona, CLI, model and
capability class. That inheritance is also what makes several refusals
structural: an orchestrator or manager source would be a second fixture, and a
lead source a second root, so all three are refused outright.

**Refusals, all before anything is minted or cut**, each with its own sentence:

| refused | why |
| --- | --- |
| an unknown or foreign agent | the `unknown agent` wording, so no other group's ids leak |
| orchestrator, manager, lead | a fork inherits the block; none of those is a delegate anyone may open a second of (a lead's own pane forks into a Solo pane instead, below) |
| a pane a live review or plan drive owns | the drivers' ownership ladders are per agent id (`rd_owner`, `pd_owner`), and a fork is a new agent they never briefed — refusing is honest, a third owner would be invention. An UNREADABLE review-drive record refuses too (`rd_driven_panes`), the same fail-closed reading `kill_agent` takes on the same file |
| a CLI whose seam is `None` | the row's own note, verbatim (`fork_refusal`) |
| a structured-driver block | `pi_launch_spec` has no fork parameter, so a fork there would start a FRESH session and call it a fork |
| a source with no recorded session | nothing to fork yet; codex and opencode record theirs a few seconds after the first prompt |
| `worktree: false` on a worker or reviewer | #338/#359: two agents sharing one checkout is the conflict that rule exists for |

**The workspace.** A worker or reviewer fork gets a NEW worktree cut from the
source's branch — when that branch EXISTS. A worktree pane's recorded branch was
cut at its spawn; a shared-repo pane's is only the name it was told to create,
which it may not have yet (CI caught exactly this on the first run:
`cannot resolve base "agent/w-5"`), so such a fork cuts from the default branch.
A planner fork gets no worktree, as no planner does. A reviewer's workspace note
names the branch its worktree was really cut from (`reviewer_worktree_note`) —
it used to say "the default branch" unconditionally, which a reviewer fork cut
from its source's branch would have been told falsely.

**The first turn.** A fork already holds its role and its whole history, so it
gets a `ResumeKickoff`-class turn (`fork_kickoff_prompt`), never the fresh
kickoff that would re-brief a conversation mid-stream. It names the three things
that changed at the fork: its NEW agent id (every orrerix call it makes is
attributed to it, not to the parent whose id fills its history), its workspace,
and its task — or, with none, that it waits for a brief rather than carrying on
with the parent's work. It carries its own delivery id: the parent's is in the
copied history already acted on, and a fork that took its brief for a duplicate
of its parent's would do nothing.

**The record.** `AgentEntry.forked_from` and its durable twin
`AgentRecord.forked_from` hold the parent SESSION (not the parent agent, which
may be long dead when anyone reads this); one `agent-fork` audit row — written
beside `agent-spawn` before the pane opens, see *Public-contract changes* — names both
sides — `agent`, `parent_agent`, `parent_session`, `child_session` (null where the
vendor mints it; the child's later `session-bound` row is unchanged), `cli`,
`cwd`, `worktree`, `branch`, `base`, `requested_by`.

**What F2 does not carry across a rejoin.** A session-browser rejoin of a forked
agent re-spawns it under a new agent id, and that row's `forked_from` is empty:
the provenance lives on the original row (which stays in `agents.json`) and in
the audit log, not on every later row naming the session.

## A lead forks into a Solo pane

A lead is its group's ROOT (`docs/design/lead-pane.md`), `kind_from_str` has no
`lead` arm, and a fork inheriting the lead's block would be a second root. So
`fork_session` on a lead's OWN id is not a `fork_agent`: `request_solo_fork`
refuses the same facts the gesture would (no fork seam, no session yet), takes
the group's spawn-rate backstop — a Solo pane is outside the delegate cap, but
it is still a pane an agent's call opened, and a runaway loop is possible in a
human-driven pane too — audits `agent-fork-requested` (`into: "solo"`), and
emits `orch-fork-solo-request`. The frontend then runs the ordinary Solo fork on
the lead's pane, built through `forkActionFor` from the pane's state NOW, so a
request from the backend is held to exactly the rules a right-click is — and a
request naming a pane this window does not have is a refusal with its own
sentence, never a silent no-op.

**A request, then an outcome** (review round 1). Nothing is open when the
backend answers the lead, so the row it writes says so: `agent-fork-requested`.
The frontend ACKS through `orch_fork_solo_result`, and
`record_solo_fork_outcome` writes the outcome beside it — `agent-fork` for an
opened pane, `agent-fork-failed` with the reason otherwise (pane not open here,
a refusal the menu would give, a failed open). The ack is checked against the
group's lead, so a stale or foreign id records nothing. The spawn-rate slot
stays spent at the REQUEST, deliberately: the backstop bounds an agent's calls,
and a bound spent only on success would let a loop whose opens all fail run
unbounded. The
lead's line sheds its identity AND its `--disallowedTools Agent` marker
(`remintSoloIdentity`'s `stripLeadMarker`) before a plain solo identity is
minted — never a lead one. The human can do the same from the lead pane's menu.

## The pane menu's three routes

| pane | route | built where |
| --- | --- | --- |
| Solo (or no identity yet) | a Solo fork beside it | frontend (`agentForkCommand`) — every CLI with a seam since F2 |
| a lead's own pane | a Solo fork, the lead identity shed | frontend, the same route |
| worker / reviewer / planner | a new delegate of the same block | backend (`fork-delegate` → `orch_fork_agent` → `fork_agent`) |
| orchestrator / manager | no row | — |

A delegate row decides nothing beyond the CLI's seam, so the menu can never
contradict the backend: every other refusal is `fork_agent`'s, surfaced as a
toast.

## #3331: the two F1 residuals F2 had to take

- **Item 1 — the click re-reads the pane.** The menu binds the session when it
  opens, and a pane can be restarted or re-bound before the click. `forkClickRefusal`
  (`src/panemenu.ts`) refuses when the session or the CLI moved, and the line the
  fork is built from is re-read at the click too. Refusing rather than forking
  the new session: the human chose the conversation they were looking at.
- **Item 2 — the builder refuses loudly.** F1's builders dropped `fork_of`
  silently for a CLI with no seam; F2's tool is the first caller that could
  reach that with a copilot or gemini source, and the silent drop would have
  launched a FRESH session and reported it as a fork. `build_agent_command_ex`
  and `build_agent_argv_ex` now return `Result`, refusing through `fork_line`
  with `fork_refusal`'s sentence. Their infallible bodies (`agent_command_line`,
  `agent_argv`) are split out so the wrappers that never fork need no `unwrap` —
  a panic on a spawn path is a process abort (constraint 10).
- **Item 3** stays as F1 recorded it: the constant-selection test only bites
  once the constant is flipped, and L1 is still the human's.

## Where the pieces live

| piece | where | what |
| --- | --- | --- |
| the table | `crates/loomux-engine/src/model.rs` | `ForkSeam` (`Flag`/`Subcommand`/`ParentFlag`/`None`), `CliCaps.fork`, `fork_refusal`, `CLAUDE_FORK_PREMINTS_CHILD_ID` |
| the backend line | `src-tauri/src/orchestration/mod.rs` | `build_agent_command_ex` / `build_agent_argv_ex` (`Result`), `fork_line`, each CLI arm reading its row |
| the delegate fork | `src-tauri/src/orchestration/mod.rs` | `fork_agent`, `spawn_agent_full`, `ForkSpawn`, `fork_kickoff_prompt`, `request_solo_fork` |
| the MCP tool | `src-tauri/src/orchestration/mcp.rs` | `fork_session_tool`, the `fork_session` arm, the lead's listing and gate rows |
| the commands | `src-tauri/src/orchestration/mod.rs` + `src/orchestration.ts` | `orch_fork_agent` / `orchForkAgent`; `orch_fork_solo_result` / `orchForkSoloResult` (the lead self-fork's ack) |
| the frontend line | `src/panerestore.ts` | `FORK_SEAMS`, `forkGrammarOf`, `agentForkCommand`, `canForkCli`, `forkPremintsChild`, `forkPaneName` |
| the one-shot rule | `src/panerestore.ts` + `src/pane.ts` | `forkRecordCommand`, applied in `Pane.capture`; `hasForkSession` per CLI |
| the reconciler exception | `src/main.ts` + `src/pane.ts` | `reconcileCandidates`, `claimedSessionIds`, `Pane.forkedFrom` |
| the record | `src/tabstore.ts` | `PersistedPane.forkOf` (additive, blank coerces to null) |
| the gesture | `src/panemenu.ts` | `forkItem`, `forkActionFor`, `forkClickRefusal` |
| the execution | `src/orchestration.ts` + `src/main.ts` | `forkPaneSession` → `OrchWiring.openForkedPane`; the `orch-fork-solo-request` listener |
| the name prompt (F5) | `src/forkname.ts` + `src/forkprompt.ts` | `forkNameDecision` (tested), `promptForkName` (DOM) |
| the lineage (F5) | `src/forklineage.ts` | `gatherForkPointers`, `buildForkIndex`, `lineageOf`, `parentLink`, `forkTreeRows`, `sessionDisplayName` |
| the durable Solo pointer (F5) | `src/sessionlog.ts` | `SessionRecord.fork_of`, set once in `record` |
| the roster pointer on the wire (F5) | `src-tauri/src/orchestration/mod.rs` | `SessionRole.forked_from` |
| the tree and the crumb (F5) | `src/sessions.ts` + `src/pane.ts` + `src/main.ts` | `SessionForkHost`, `forkIndex`; `Pane.setForkCrumb`; `refreshForkCrumbs`, `returnToSession` |

## Public-contract changes

- **`PersistedPane.forkOf`** (`tabs.json`, F1) — additive. Absent, blank or
  malformed decodes as `null`, i.e. "not a fork", so every pre-F1 snapshot
  restores exactly as it did.
- **`CliCaps.fork`** — an internal table, no workflow-schema key. A workflow file
  cannot ask for a fork and nothing in it parses differently. F2 changes its
  TYPE (four variants; `ForkSeam::flag()` is replaced by `token()` and
  `premints_child()`), which reaches no file and no wire.
- **`build_agent_command_ex` / `build_agent_argv_ex` return `Result`** (F2) —
  `#[doc(hidden)]` integration-test seams, not an external surface; every caller
  in the tree moved with them.
- **`AgentRecord.forked_from`** (`agents.json`, F2) — additive in both
  directions: `#[serde(default, skip_serializing_if = "Option::is_none")]`, so a
  pre-F2 row reads `None`, a non-fork row never gains the key, and an older
  loomux reading a newer roster ignores it. Pinned by
  `forked_from_is_additive_on_the_durable_roster`.
- **The `fork_session` MCP tool** (F2) — agent-facing; listed for the
  orchestrator and, as an argued row of its positive enumeration, the lead; gated
  by `require_spawner` and, for the lead, the dispatch gate's own row.
- **The `orch_fork_agent` Tauri command** (F2) — in the `orch-control` ACL set.
  **`async` through `run_blocking`, not a synchronous `mutating_command`, and
  that is forced**: a fork is a spawn, and a spawn blocks until the FRONTEND opens
  and binds the new pane, which it does on the webview thread a synchronous
  command would be occupying — a sync fork would wait on itself for the whole
  bind timeout. Constraint 10's barrier is for commands on that thread; this is
  `resume_orch_session`'s shape, for the same reason.
- **The `orch-fork-solo-request` event** (F2) — backend → frontend, one per lead
  self-fork, declared in `test/perfpolicy.test.ts`'s stream manifest.
- **The `agent-fork` audit action** (F2) — its timing differs by route, and a
  reader must not take it as "a pane opened" on the delegate route:
  - **a delegate fork** writes it in `spawn_agent_full` beside the `agent-spawn`
    row, once the fork is admitted and registered and BEFORE the frontend has
    opened or bound its pane — so it records that the fork was started. A fork
    whose pane then fails to bind is recorded the way every spawn's is: `mark_dead`
    writes that agent's `agent-exit`;
  - **a lead's self-fork** writes `agent-fork-requested` at the request, and
    `agent-fork` only from the frontend's ack once the Solo pane opened
    (`agent-fork-failed` with the reason otherwise) — there the row does mean
    the pane opened, because the open happens after the backend has answered.
- **The `orch_fork_solo_result` Tauri command** (F2, review round 1) — the
  frontend's ack for `orch-fork-solo-request`, in the `orch-control` ACL set,
  `async` through `run_blocking` because it writes the audit log.

No new dependencies. No PTY resize (a fork is one more pane through the ordinary
open path). No getrandom (a Solo child's id is `crypto.randomUUID`, the webview's
Web Crypto; a delegate child's is the existing `new_session_uuid` mint). No new
path join: the parent session id is validated by `sanitize_session`
(`pathseg::check_segment`) before it reaches a line.

## F5: the fork's name, and its lineage (#3368)

The human's feedback on beta2, verbatim: "id like to support renaming the session
so I can easily identify it. Also if there was some way to visualize or trace the
lineage of forked sessions so a user could easily identify which fork to return
to."

### The name is the pane's name

Fork session… first asks for a name, in a popover pinned over the source pane
(`position: fixed` on `document.body`, so no layout moves and no PTY resize can
follow — constraint 1), pre-filled with `forkPaneName(<source name>)`. Enter
forks under what is typed (blank → the default), Esc forks under the default,
and only the popover's ✕ or a click away backs out — `forkNameDecision`, which
is the tested half. The asymmetry is the issue's rule: the prompt is an optional
rename, not a confirmation, so no KEY a human presses to get past it can lose
them the fork.

There is no fork-name store. The answer goes in as `PaneOptions.name` on a Solo
fork and as `orch_fork_agent`'s existing `name` on a delegate one, which is the
field the header's rename edits, `tabs.json` persists, `sessionlog.json`
records against the session and the roster carries as the agent name — so the
browser row shows it through the path that already shows a renamed pane.

`fork_session(name)` already named a DELEGATE fork. It did not name a lead's
self-fork: `request_solo_fork` took no name, and the Solo pane opened as
`<lead> (fork)` whatever was asked. F5 threads it through the
`orch-fork-solo-request` payload (and records it on the `agent-fork-requested`
row, which is the half a test can observe), collapsed to one line.

### The lineage is derived, never stored

orrerix records ONE fact per fork — the parent SESSION — and has three homes for
it, one per lifetime:

| copy | where | lives as long as |
| --- | --- | --- |
| `PersistedPane.forkOf` | `tabs.json` | the pane (the live copy) |
| `AgentRecord.forked_from` | the group's `agents.json` | the group's record (a delegate's durable copy) |
| `SessionRecord.fork_of` | `sessionlog.json` | the session's record (a Solo fork's durable copy) |

The third is F5's one new field, and it is argued, because #3368 says to add
nothing that duplicates the first two. **What that rule forbids is a stored
CHAIN, not a durable home for the one-level pointer.** Without it a Solo fork's
parent lives only in `tabs.json`, which forgets it the moment the fork's pane is
closed — and a closed fork falling out of the tree is exactly the "which fork to
return to" case the human asked about. It is the same rule `forked_from` already
follows for delegates: the live pane has one copy, the durable record another.
It is written ONCE — `SessionLogStore.record` takes `fork_of` only onto a record
that has none, and never clears or moves it, since a later record of the same
session (a resume from the browser opens a pane with no `forkOf`) must not undo
its birth. A record that is not a fork is written without the key, byte for byte
what a pre-F5 build writes, and an older build keeps the key through its
`unknown` passthrough.

`SessionRole.forked_from` is not a fourth copy: it is the roster's field read
onto the `orch_session_roles` wire, which is how the frontend reads the roster.

**The chain is walked every time it is asked for** (`src/forklineage.ts`):
`gatherForkPointers` reads the three records, `buildForkIndex` joins them, and
`lineageOf` walks parent pointers root-ward. A stored chain would be a second
copy of those pointers that could disagree with them, and one that had to be
rewritten whenever an ancestor's record changed or aged out; a walk cannot
disagree with what it walks. Where two copies of one pointer DO disagree (only a
hand-edit can do that — all three are written from one value), the live pane
wins over the roster over the log, and the loser is kept on `conflicts` rather
than silently dropped.

**The dangling-parent rule.** A parent is KNOWN when anything has a record of
it: a browser row (the CLI store scan), an open or dormant pane, a roster row,
a sessions-log record. A pointer to a parent nothing knows — its transcript
deleted, its log record evicted at the 500-record cap — ends the walk at the
last known node with `dangling` and the id it could not follow. The row still
says `fork of session <id> — no longer on record` and sits at the top level,
rather than passing for a conversation of its own. **A cycle is refused**: a
child id is always freshly minted, so a loop is impossible by construction, and
the walk stops at the first repeat instead of looping; the tree puts every
member of a loop at the top level, since none of them is reachable from a root.

**orrerix's record is the source of truth for all four forkable CLIs.** pi's
`parentId` and opencode's `parent_id` are NOT read: opencode does not set
`parent_id` on a fork at all (L3 above), claude and codex have no such field
to read, and a lineage that read vendor fields would mean something different
per CLI. orrerix's pointer is written by orrerix's gesture, identically for all
four.

### Where it shows

- **The session browser's tree** (`forkTreeRows`). A fork nests under its
  DIRECT parent when that parent is in the list, and otherwise sits at the top
  level in its own display position — never under a grandparent, because its
  row says "fork of <parent>" and the indentation must not contradict it. A
  parent's forks are collapsed behind a "▸ n" expander (a session forked five
  times is one row until asked) and a typed filter expands everything. Every
  fork row carries "↳ fork of <parent> · forked <when>" — the when is the
  sessions log's `created_ms` for the child, i.e. when its session first became
  known — and a "↰" return button: the parent's pane if one is open (live or
  dormant), otherwise the same resume a click on the parent's row does.
- **The pane header's crumb**, `↰ <parent name>`, the same gesture. A header
  chip like the others — `chip-yields`, so a long parent name ellipsises before
  the pane's own name gives up room, and walked by `measureHeaderFixed` like any
  chip, so the fold ladder prices it unasked.
- **No overlay tree.** The browser tree answers "which fork do I return to" in
  the place the human already goes to find sessions, and the crumb answers "what
  is this pane a fork of" where they are looking. A git-view-style overlay would
  be a third surface carrying the same derivation, with nothing it can show
  that those two cannot at this depth.

**Stated residuals.** A delegate fork's crumb and tree row read the roster
through `orch_session_roles`, which the browser loads on its own refresh (at
boot, on showing the Sessions tab, on ↻) and not on every spawn — that call fans
out over every group on disk, and a spawn is not worth a scan. So an
orchestration fork's crumb appears at the next refresh, not the instant its pane
opens; a Solo fork's is immediate. And a pointer is lost with the record that
held it: an unnoted sessions-log record can age out at the cap, after which that
closed Solo fork shows as a root — the dangling rule covers its CHILDREN, not
the fork itself. Neither is silent: the first corrects itself on a refresh, and
the second is the eviction rule the log already documents.

## What F2 did not change, and why

- **`orchestrator.md` did not move.** The resident core sits at 44,955 B against
  the 45,000 B `RESIDENT_CORE_BUDGET`, and a new playbook section needs a resident
  stub there too. The orchestrator learns `fork_session` from the tool's own
  description (read on every call) and from one paragraph in the playbook's
  **Planning and scheduling** section; `lead.md` carries its own bullet.
- **No rejoin.** Nothing merges two session files, in any harness surveyed: the
  single vendor mechanism that folds one path into another is pi's `/tree` branch
  summary, and it is intra-file. claude's docs describe cross-terminal sharing of
  one id as *interleaving*, i.e. the failure mode. The rejoin slice (F3) was
  dropped by the human; a true transcript merge would mean writing a vendor's
  internal format, and orrerix will not.
- **copilot stays refused** (F4, held on live check L4).
- **The Agents tab does not show "fork of …" yet.** The roster records the
  parent; rendering it is a UI follow-up, not part of F2's brief.
