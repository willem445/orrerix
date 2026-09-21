# Delivery triage — the rule tier

#3304 slice S1. The feature that decides which orchestrator-bound notices close
by a shape RULE instead of waking the pane, holds those for a bounded time, and
flushes them as one framed delivery. There is no model and no network in this
slice; `triage.provider` accepts exactly one value, `none`.

The measurement this is built on is #3304's findings comment, and it is worth
restating because every design choice below is downstream of it: over 368
orchestrator wakes in fifteen days, **234 (64 %) close by a leading-shape rule**
and a further 40 are batchable FYI, while a wake costs **≈1.3 M cache-read
tokens**. The residual — the 136 deliveries whose text would have to be *read*
to classify — is what S2's harness measures and S3's provider would answer. S1
banks the 64 % without either.

## 1. The choke point

`OrchRegistry::deliver_prompt_as`, above the `prompt` audit line and above every
admission, keyed on `a.role == Role::Orchestrator && delivery == Delivery::MidSession`.

**Not `deliver_to_orchestrator`**, which reads as the obvious door and is not the
only one: a `notify_when` fire reaches the pane through `watchdog_tick` →
`deliver_prompt`, and a relayed line through `deliver_relayed_to_root`. The three
meet only at `deliver_prompt_as`, which is the same argument the manager
no-injection guarantee makes for enforcing itself there — one refusal covers every
producer, including the ones a future slice writes without knowing this exists.

**Above the `prompt` row**, because `front_door_refusals`' doc makes `prompt` mean
"offered to a pane". A deferred notice was not offered to one, and a row saying it
was would corrupt every derivation over the log — this feature's own wake count
first.

**Below the manager / dead / no-terminal refusals.** Those are facts about the
TARGET, which triage has no opinion on. Putting triage above them would let a
notice bound for a dead pane be "deferred" into a store that will later flush it
at a pane that no longer exists.

### 1.1 Two deviations from the plan, both toward DELIVER

The S1 brief specifies `a.role.is_root()`; the code keys on
`Role::Orchestrator`. `is_root()` also admits `Role::Lead` — **the human's own
pane** — and holding a notice back from a pane a human is sitting in front of is
a different product decision from cutting an agent's wakes, which is what #3304
measured. A lead group also has no merge queue and no review driver, so two of
the five rules could never fire there anyway. The narrowing can only deliver
more.

The brief specifies the `workflow_run` rule as "`conclusion: success` **on the
default branch**"; the code keys on the conclusion alone. The reason is that the
branch is not in the notice: `notify::watch_fired_notice` for a `WorkflowRun`
carries the run id and the conclusion, and nothing else. A branch clause would
therefore have been a condition this module cannot evaluate, silently true or
silently false. It costs nothing — a run that went green is news to nobody
whichever branch it was on, and the one red run in the census's 65 is caught by
the conclusion.

## 2. The order of decision

`loomux_engine::triage::decide`, pure and total:

1. **`triage.enabled: false`** (the default, and every repo that has not opted
   in) → deliver. Nothing else runs; no file is read.
2. **The never-triaged set** → deliver, with the reason on the audit row:
   a relayed human line (the sender is a `Manager` or a `Lead`), a re-grounding
   or restored notice, a `HELD` drive, a `blocked` report, a watchdog stall, and
   anything whose text NAMES the orchestrator (`blocking on you`, `needs you`,
   `your call`, …).
3. **`triage.kinds:`** — if the repo narrowed the set and this kind is not on it,
   deliver.
4. **The rule table** (§3).
5. **Fail-safe: deliver.** Every shape not positively recognised.

Step 2 is mostly redundant against step 5 — the rule table would not have matched
a `HELD` notice anyway — and it is stated explicitly for two reasons. The audit
row then says WHY rather than leaving it to be read off an absence; and S3's
provider tier is gated on the same predicate, so "a human's relayed words are
never sent to a classifier" becomes a property of a function rather than of the
rule table happening not to match them.

**The never-triaged markers are read off the text, and that is safe in exactly
one direction.** A marker can only ever move a delivery toward DELIVER, so an
agent that writes one gets a wake it may not have needed — never a suppression it
did not ask for. That asymmetry is the whole reason a text-keyed test is
admissible here while a rule keyed on agent-authored prose would not be. The
census's own note applies: this text is agent-authored, and a rule that could be
*talked out of* its answer by the thing it is routing is a rule with an injection
surface.

## 3. The rule table

| kind | rule | census |
| --- | --- | --- |
| `drive-gate-satisfied` | the merge queue is enabled AND `queue_merge` accepted the PR → defer. Any refusal → deliver. | 74 |
| `run-completed` | `conclusion: success` → defer; anything else → deliver | 65 (64 green, 1 red) |
| `pr-checks` | `SUCCESS` → defer; anything else → deliver | 1 |
| `planner-exited` | defer — the plan drive (#3040) consumes this; the notice is a slot-free FYI | 4 |
| `agent-exited` | defer — the roster already reflects it | 4 |
| `drive-cancelled` | defer — something already decided the drive should end | 1 |
| `message-from` carrying `---BEGIN PLAN k/n---`, `k < n` | defer; the LAST chunk is a genuine wake and carries its siblings out with it | 17 → 4 |

Everything else — 90 `done` reports, 17 reviewer reports, the rest of
`message from` — delivers. #3304 Q2 is explicit that the `done` class is where
the residual judgement lives, and S1 ships no judgement.

**`GATE SATISFIED` earns its rule by the ENQUEUE, not by its shape**, which is
why it is a third `Decision` variant rather than a plain defer. The
orchestrator's own next step for a satisfied gate is `queue_merge`; a notice
suppressed *without* one having happened is a PR nobody is driving. The registry
attempts the enqueue and turns a refusal — a gate the queue's own re-check does
not accept, `also:` conditions unmet, a state file it cannot read — back into a
delivery.

**The vocabulary is inherited from `orch-scorecard.cjs`, and it is not
identical to it.** `doc/design/orchestration-evals.md` §4.1 and that script
already decide an orchestrator-bound prompt's class from its leading shape, and
the census that motivated this feature was taken with them. `triage::classify`
reuses those shapes, their first-match-wins discipline and their class names
where they exist — hand-written prefix tests rather than regexes, because the
engine has no `regex` dependency and CLAUDE.md constraint 2 is not worth
spending on one.

**The four differences are named rather than left to be discovered** (review
round 1, B4), because the scorecard answers "what did this cost" over a log
while this answers "must this wake the pane" on the delivery path, and the
classes diverge where those questions do:

| # | difference | why |
| --- | --- | --- |
| 1 | no `verdict-notice` — it folds into `system-notice` | §4.1 calls that tie-break load-bearing for the scorecard; here no rule fires on either class, so both deliver. A rule for one would have to bring the class back first. |
| 2 | `message-from` split out of `delegate-blocked`, which the scorecard pools | LOAD-BEARING: the plan-chunk rule exists only because `message-from` can be reasoned about apart from a blocked report, which is never triaged |
| 3 | `pr-checks`, `planner-exited`, `agent-exited` are in neither scorecard table | the census counted them by hand (#3304 Q1), and a rule needs a class to hang on |
| 4 | `run-completed` requires `run <digits>: completed` | it now matches the scorecard's own regex; an earlier draft accepted any token after `run `, which was looser than the table it claims to reuse, and would have SUPPRESSED a prose line merely shaped like a green run |

A second, divergent classifier is what this module exists not to be. A
documented, tested divergence is a different thing from an undisclosed one —
and (4) was a real defect rather than only a documentation gap.

## 4. The deferral, and its three bounds

A deferred notice is appended to `<group-dir>/deferred.json` and audited
`delivery-triaged {to, from, kind, action: "rule:<name>", text}`. A delivered
one is audited with the same row, `action` set to the deliver reason, and **no
`text`** — it already has a `prompt` row carrying its text, and a second copy
would be a second record to keep in step.

**The `text` on a deferral is load-bearing, not diagnostic** (review round 1,
B1). Before it, the full text of a held notice existed in exactly one place,
`deferred.json`, and only until the flush cleared it. The deferrable classes
audit no notice text of their own (the agent-exit row is `{agent, exit_code}`)
and `prompt` is deliberately not written for a deferral, so a flushed notice’s
words existed nowhere afterwards. That made three of this feature’s own claims
false at once: the flush frame’s pointer, the "in the audit log either way"
sentence below, and "NOTHING IS EVER DROPPED" for the crash between the clear
and the paste. One field makes all three true.

It is flushed as ONE framed delivery:

```
[orrerix] 3 notices deferred over the last 12 min (flushed because something did
need you). Each closed by a shape rule, none of them a decision; full text is on
this group’s audit log as delivery-triaged:
- [run-green] run-completed from loomux: [orrerix] run 17812: completed — …
- …
```

Three bounds, and **all three are the #496/#513 lesson** rather than one being
belt-and-braces:

1. **the next genuine wake** — the flush is admitted in front of the delivery
   that was allowed through, and the queue is FIFO, so the orchestrator reads
   what it slept through before the thing that woke it;
2. **`max_defer_minutes`** (default 30, refused outside `1..=240`) — on the
   watchdog's own timer, because bound (1) is a wait on a signal that may never
   come, and no wait on a fallible signal is unbounded. It targets **live**
   orchestrators only (review round 1, B2): flushing at a dead pane would clear
   the store, have `deliver_prompt` refuse `AgentDead`, and lose every held
   notice — §1's placement argument reintroduced on the flush side. A group
   with no live orchestrator keeps holding until one exists;
3. **`MAX_DEFERRED`** (40) — a CI storm inside one window flushes early rather
   than growing a frame nobody can read.

**Turning triage OFF is a fourth release, and it has to be** (review round 1,
premortem 1). All three bounds above are gated on the same policy that created
the entries, so a store held by a policy since switched off — or whose file
stopped parsing — had nothing that would ever release it: `list_deferred`
answered `enabled: false, count: N` indefinitely. The flush tick now treats a
non-empty store under a disabled policy as due immediately. Turning the feature
off hands back what it is holding.

It carries **its own `FlushCause`** (`policy-off`), not the deadline's (review
round 2). The cause is rendered into the frame a human-supervised pane reads,
and "flushed on the deferral deadline" is untrue of a release triggered by the
policy going away — usually, as the test pins, nowhere near that deadline. A
wrong value on a user-facing surface is a defect, not a tidiness point.

**Nothing is ever dropped.** The store is written BEFORE the notice leaves the
delivery path, and a write that fails DELIVERS: a deferral nobody recorded is a
notice lost at the next restart, and losing one is worse than spending a wake.
`deferred.json` survives a restart, and `list_deferred()` reads it back in full.

**The flush clears the file before delivering the frame**, and the residual that
order chooses is stated rather than hidden: a crash between the two costs one
frame. Clearing afterwards would risk re-delivering the same frame on every
restart, which is the worse failure — and every held notice's full text is on
the audit log either way, which is what the `text` field above is for.

## 5. Fail-safe is DELIVER, at every layer

A group that cannot be resolved, a workflow file that will not parse, a
`deferred.json` that cannot be read, a store that cannot be written, an enqueue
that refuses: every one of them delivers. The registry never declines to deliver
because something went wrong; it declines only when the engine positively
recognised a shape AND the deferral was durably recorded.

Resolving an unreadable workflow file to the default is **fail-open**, which is
the wrong direction for a security check and the right one here, and the
difference is worth naming because the reflex goes the other way: what this
policy switches on is a SUPPRESSION, so failing open delivers MORE. A file
caught mid-save makes every notice reach the pane exactly as it did before this
feature existed. Same posture, same reasoning, as `merge_queue_policy` and
`board_policy`.

## 6. Config, and CLAUDE.md constraint 8

```yaml
triage:
  enabled: false          # the product default; an absent block means the same
  provider: none          # the ONLY value this build accepts
  kinds: []               # empty = every kind the rule table covers
  max_defer_minutes: 30   # refused outside 1..=240
```

Parsed like `driver:`, with `deny_unknown_fields`, and **policy rather than
mechanism**: nothing here names a pane, an agent, a PR, a branch or a program.
The two string-shaped keys are each a closed vocabulary the parse refuses
outside, and the rule table itself is compiled in — there is no spelling of this
block that writes a rule. Every field can make orrerix deliver MORE; the only one
that can make it deliver less is `enabled`.

`provider` outside the set is refused rather than defaulted, and it is the one
value where a silent substitution would be a **privacy claim**: an author who
wrote `provider: typesafe` believes agent-authored text is being sent to a third
party and classified there. Running the rule tier anyway would leave that belief
in place while the behaviour was something else entirely. The error names #3304
S3.

### 6.1 The privacy line, for the provider that does not exist yet

Stated here so S3 inherits it rather than re-deciding it. With
`provider: none` — the only value this build has — **no text leaves the
machine.**

**What backs that sentence, precisely, because an earlier draft of it
overreached.** It said "there is no HTTP client anywhere in this workspace",
which is false: `tauri` brings `reqwest`, `hyper` and `hyper-util` into
`Cargo.lock` transitively, as the webview host has done since long before this
feature. The claim reached three surfaces before the guard that was supposed to
check it was ever RUN — which is CLAUDE.md's "a guard that REFUSES ships only
after it has run clean over known-good subjects", landing on its author.

The true and checkable statement is narrower, and
`neither_the_engine_nor_triage_can_reach_a_network` pins both halves of it:
`loomux-engine` — the crate triage lives in — declares no HTTP client among
its short, individually-audited dependencies; and neither triage source names a
network primitive, default-denied over TOKENS (`https://`, `TcpStream`,
`Client::new`) rather than over a binding's name. The shipped binary does link
an HTTP stack; what S1 guarantees is that no delivery can reach it, because
there is no call, no client and no address anywhere between
`deliver_prompt_as` and a decision. S3 adding a provider has to add a direct
edge or a socket, and either reddens that test.

With any future `provider != none`, the TEXT of `report` and
`message_orchestrator` lines would leave the machine, and `kinds:` is what
bounds which. Whatever S3 ships, that sentence belongs in the user docs in the
same PR.

## 7. What this slice does not do

- No provider, no network, no key (S3, and held on a measured agreement run).
- No pane surface: `triage.*` is listed as pending in
  `test/workflowschema.test.ts`'s editor inventory, because a form control for a
  gate that suppresses deliveries wants the deferred count and
  `list_deferred`'s rows beside it rather than a bare checkbox.
- No write tool at any tier. Nothing on the MCP surface can defer a notice,
  flush one, or clear the store: deferring is a rule's answer about a shape, and
  an agent that could ask for one could ask for its own report to be held back
  from the pane that routes it.
