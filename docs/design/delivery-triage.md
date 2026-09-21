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
identical to it.** `docs/design/orchestration-evals.md` §4.1 and that script
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

## 8. The eval harness (#3304 S2)

`scripts/orch-triage-eval.cjs` replays a group's own `audit.jsonl` through the
rule tier above and reports what it would have done. It exists because neither
number that decides whether this feature is worth having is knowable from
reading §3: how many wakes the rules actually close, and how many of the
notices they close were ones the orchestrator needed. Both are properties of
the TRAFFIC, so both have to be measured against it.

```
node scripts/orch-triage-eval.cjs \
  --audit <group>/audit.1.jsonl --audit <group>/audit.jsonl \
  --agents <group>/agents.json \
  --labels test/fixtures/orchtriage/labels-loomux-68435179.csv
```

### 8.1 How the harness reuses the rules, and why it is a mirror

The rule table is hand-written prefix tests over `&str`. There is nothing a
`--dump-rules` export could hand a Node script: a dump of the class and rule
NAMES would export the vocabulary and leave the decisions behind, which is the
half that can diverge silently, and shipping a Rust helper binary would put a
`cargo` build on the eval's path — which this repo's agent workers are barred
from running at all. So the script carries a JS mirror of `triage.rs`, and the
mirror is pinned twice, because each pin is blind where the other sees:

1. **Cross-language golden vectors.** `test/fixtures/orchtriage/vectors.json`
   holds delivery cases with their expected `classify` / `never_triaged` /
   `decide` answers. `crates/loomux-engine/tests/triage_vectors.rs` asserts the
   ENGINE agrees with that file; `test/orchtriageeval.test.ts` asserts the
   MIRROR agrees with the same file. One fixture, two readers, one CI run — a
   behavioural divergence reddens on whichever side moved. Vectors are blind to
   a Rust rule that exists and has no vector.
2. **A vocabulary scan.** `test/orchtriageeval.test.ts` reads `triage.rs` and
   asserts the mirror's `KINDS`, `RULES`, `NEVER_REASONS`, `DELIVER_REASONS` and
   both marker arrays are set-equal to Rust's own wire spellings, with a
   per-scan positive control so a regex that stopped matching reddens instead of
   agreeing with an emptied table. A rule added or renamed in Rust reddens here
   with no vector involved.

Both tests also assert every rule and every deliver reason is EXERCISED by a
vector, because a rule with no case is a rule the first pin cannot see.

**The residual, stated rather than left to be found:** a change to the BODY of a
Rust rule that keeps its name and is covered by no vector is invisible to both
pins. The exercised-by-a-vector assertion is what bounds it; it does not remove
it. `Rule::GateSatisfied` is the one rule no vector can name at all, because
`decide` answers `TryEnqueue` for it and only a successful merge-queue enqueue
turns that into a defer — an impure step a fixture cannot contain. Its witness
is a `try-enqueue` vector, asserted by name on both sides.

### 8.2 The population, and the one proxy

The population is `orchestration-evals.md` §4.1's definition of a wake,
unchanged: a `prompt` row whose `detail.to` is a pane whose `agents.json` role
is `orchestrator`.

S1's gate keys on `Delivery::MidSession`, so a KICKOFF is out of scope — but a
`prompt` audit row carries only `{text, to}` and no delivery kind, so the replay
cannot read which it was. It drops the FIRST prompt row per orchestrator pane id
instead, which is right exactly when a pane's first delivery is its kickoff (it
is, for every pane in the census: a pane is spawned with one) and wrong for a
pane whose kickoff predates the `--from` window, where it costs one real
delivery. `--no-drop-kickoff` turns it off and the report prints the count
either way, so the error is bounded and visible rather than assumed away. A
`delivery` field on the `prompt` row would remove the proxy; that is a product
change and is recorded here as a limitation, not made.

One more figure the replay cannot observe: `Decision::TryEnqueue` is resolved
OPTIMISTICALLY (the enqueue is assumed to succeed, so the notice defers). That
is the direction that makes the harness's own headline worse rather than
better — every false defer the resolution can manufacture is counted against
the tier — and the count is printed separately so a reader can subtract it.

### 8.3 The labelling rubric

A label answers one question per delivery:

> Does this notice name an action the ORCHESTRATOR must take, which cannot be
> derived from the notice's leading SHAPE alone, before the group can proceed?

| class | meaning |
| --- | --- |
| `decision` | yes — a call, ruling, approval, routing choice or tool call the text names. A human line typed into the pane is always a decision: it is an instruction by construction. |
| `routing` | no — the next step is the standard one for this kind and is readable off the shape (a green `done` starts a review drive; a `request_changes` hands back). |
| `fyi` | nothing is asked and nothing waits on a reply. |
| `escalation` | a HUMAN, not the orchestrator, must decide. |

The eval scores the BINARY, because deliver-or-defer is the only question the
tier asks: **needs-orchestrator** = `decision` + `escalation`, **audit-only** =
`routing` + `fyi`. Scoring the four-way class would be scoring a question
nothing in the system answers.

The shipped set is `test/fixtures/orchtriage/labels-loomux-68435179.csv`: 150
consecutive deliveries, one pass, one labeller, `ts_ms,kind,label` and **no
delivery text** — the rows are a live group's agent-authored prose, and the set
is reproducible from the timestamps against the audit it was cut from. Its
header records the two classes where this rubric diverges from #3304's planning
census, which is most of the gap between that census's 26 % decision rate and
this set's 60 %. A second labeller is still required before any agreement figure
from it is quoted as a pass criterion.

### 8.4 The metrics, and which one decides

- **deferred / delivered**, per rule and per kind, plus the projected saving at
  #3304's census figure of ~1.3 M cache-read tokens per wake (carried, not
  re-measured here).
- **agreement** on the binary, always printed with its denominator: an agreement
  figure over an unstated population is not a number a reader can check.
- **a per-class confusion matrix**, and **ECE** over reliability bins in 0.1
  steps for a provider's confidence. An empty bin prints `-`, never 0 — an
  unsampled bin is not a perfectly calibrated one, and an ECE over zero samples
  is `null` rather than a clean zero.
- **FALSE DEFERS** — a delivery labelled needs-orchestrator that the tier held
  back. This is the only harmful error and it decides: a spared wake costs a
  wake, a swallowed decision costs a stalled group. #3304 Q4's pass criterion
  for any provider is ZERO of these at the chosen floor.
- **wasted wakes** — an audit-only delivery that still woke the pane. The cheap
  error, reported beside the expensive one so the trade is visible.

The **negative control** for every saving figure is `--triage-disabled`: the same
corpus with the tier off must report 0 deferred and 0 false defers. Without it,
"the tier deferred N" is a number with nothing to fail against.

### 8.5 The provider seam, for S3

A `TriageProvider` is an object with `classify({ts_ms, from, kind, text}) ->
{class, confidence} | null`. The residual it is shown is exactly the rule tier's
`no-rule` arm — never-triaged deliveries are excluded by construction, which is
what makes "a human's words are never sent to a classifier" a property of the
code rather than of the rule table happening not to match them.

`FakeTriage` replays verdicts from a JSON file keyed by `ts_ms`, so S3's
decision rule can be swept against this label set with no network and no
account. The fail-safe ladder lives in `providerAction` so the sweep measures
the rule S3 will ship rather than an approximation of it: no verdict delivers, a
sub-floor confidence delivers, and `decision` / `escalation` deliver at any
confidence. A missing or out-of-range confidence is a NO-VERDICT, never a zero
one — a zero would be right by accident under every floor and wrong the moment a
caller reads `class` without one.

### 8.6 What the first run found, and what it does NOT change

The run on this group's own log (812 deliveries, both audit generations, against
the 150-row label set) is on #3304. Its headline is a result about §3's table,
not about the harness: at 150 labels the rule tier holds back **40** deliveries
this rubric calls needs-orchestrator, and **32 of those are `gate-satisfied`** —
every `GATE SATISFIED` line in the window ends "Disposition is yours
(INVARIANT 3): `list_verdicts(...)`", which `NEEDS_YOU_MARKERS` does not match.

Nothing in S2 changes the rule tier: measuring it and editing it are different
slices, and an eval that quietly fixed what it found would have no independent
reading left to report. The candidate remedies belong to a follow-up and are
recorded here so the next reader starts from them rather than re-deriving them:
adding an `is yours` marker (which the census says closes all 32 at once),
splitting the green `run-completed` rule on whether its registered note names a
GREEN-path action rather than only a red one (6 of the remaining 8), and
delivering an `agent-exited` whose text says the pane produced no output at all
(1 — a lost kickoff is not a roster update).
