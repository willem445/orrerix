# Harness adapters: a pane that reports instead of a pane that is read

#84 slice R0. The roadmap this implements is #84's nine-part plan
(comment [5466290210](https://github.com/willem445/orrerix/issues/84#issuecomment-5466290210)
onward — §2 vocabulary, §8.1 slices, §9 risks). R1 built the Claude Code
adapter's engine leaf against this note; R2 wires it. **pi is the harness that
lands the spawn path** (#2850): its RPC mode is a two-way protocol, so it
exercises `interrupt`, acknowledgement and dialogs that a one-way stream
cannot, and the wiring is written harness-neutral so R2 becomes an arm rather
than a second path.

This note is a **public contract**. Everything a driver may rely on is here;
anything not here is not agreed. Changing a name or a shape below is an edit to
this file first.

Amends: `doc/design/engine-extraction.md` §2 (what `PaneHost` hands back) and
`doc/design/remote-engine-protocol.md` §4 (two additive events, one roster
field). Amended by #2850/#2891: six additive `HarnessEvent` variants (§1.2),
extension-UI dialogs (§3.5), and §5's two projections. Reads on: `doc/design/opencode.md` (how a harness is driven today),
`doc/design/session-id-learning.md` (#440), `doc/design/human-questions.md`
(#946), `doc/design/needs-you-items.md` (#1151),
`doc/design/group-cost-tracking.md` (#42), `doc/design/workflows.md` (#222).

**Every Claude Code fact below is cited to the official docs by URL, read
2026-09-03.** No CLI was run (CLAUDE.md constraint 3). Where the docs do not
settle something it is in §9 as a live-validation item for the human, never
guessed.

---

## 0. What changes and what does not

| | today (PTY) | structured |
|---|---|---|
| how orrerix talks to the CLI | bytes into a ConPTY, echo-verified paste, blind Enter | one NDJSON line on the child's stdin |
| how orrerix learns what happened | scrape the pane's ring: readiness markers, question grids, statusline | the child's NDJSON stdout |
| what the human sees | the vendor's TUI, byte for byte | orrerix's own renderer over the same events, in the same grid cell (§5.1) |
| who admits a delivery | `loomux_engine::queue` | **unchanged** — `queue` is the front door for both |
| what a block may grant | nothing (#222 capability closure) | **unchanged** — `driver:` selects a transport, never a capability |

**The one-line summary of the cut:** `PaneHost::request_pane` hands back a
`Box<dyn AgentPane>` — a driver object — instead of a byte pipe. A PTY pane and
a structured pane are two implementations of one trait, so there is one spawn
path, one delivery door, one idle model, and one thing the daemon's `PaneHost`
(#888 C3) has to implement.

---

## 1. The vocabulary: `AgentPane` and `HarnessEvent`

### 1.1 The trait

```rust
pub enum PaneKind { Pty, Structured(Harness) }
pub enum Harness  { Claude, Pi }      // opencode/ACP are R3/R4

pub trait AgentPane: Send + Sync {
    fn kind(&self) -> PaneKind;
    fn send(&self, turn: Turn) -> Result<SendReceipt, String>;
    fn answer(&self, req: RequestId, d: Decision) -> Result<(), String>;
    fn interrupt(&self) -> Result<(), String>;
    fn events(&self) -> Option<EventRx>;         // Receiver<HarnessEvent>, take-once
    fn session_id(&self) -> Option<String>;      // None == not known yet
}

pub enum Turn { Kickoff(String), Prompt(String), Notice(String), Human(String) }
```

**`events()` returns an `Option`, and R1 found out why.** An `mpsc::Receiver`
has exactly one consumer and cannot be cloned, so `&self` has no way to hand one
back twice; the original signature above was unimplementable as written. The
take-once shape is the honest fix — a second caller gets `None` rather than a
silently split stream, which is what a broadcast channel would have cost instead.
Corrected here rather than in R1's PR, because this file is the contract and R1
is what discovered the defect in it.

`Turn` is the delivery vocabulary orrerix already has
(`Delivery::{FreshKickoff, ResumeKickoff, MidSession}` plus the `[orrerix]`
notice channel and the #43 compose strip), named once so the drainer's last step
is `pane.send(turn)` on both kinds.

**`Harness::Pi` is the second variant, and it is what makes two of the trait's
promises checkable.** pi's RPC mode is a JSONL command/event protocol on the
child's stdin and stdout rather than a one-way stream, so it is the first
harness that can honour `interrupt()` — `abort` is a command, not a signal —
and the first whose `send` gets an acknowledgement correlated by command id.
`doc/design/pi.md`'s "RPC driver (#2850)" section carries the wire facts and
their citations; this file carries only what the vocabulary owes them.

### 1.2 The event

```rust
pub enum HarnessEvent {
    Booted { session: Option<String>, model: Option<String>, capabilities: Vec<String> },
    TurnStarted { turn: TurnId },
    Text { turn: TurnId, delta: String },
    ToolCall { turn: TurnId, id: ToolUseId, name: String, input: serde_json::Value },
    ToolResult { turn: TurnId, id: ToolUseId, ok: bool },
    PermissionRequest { id: RequestId, tool: String, input: serde_json::Value },
    PermissionSettled { id: RequestId, decision: Decision, by: DecisionSource },
    TurnEnded { turn: TurnId, usage: Option<Usage>, cost: Option<Cost>, stop: StopReason },
    Compacted { trigger: CompactTrigger, pre_tokens: Option<u64> },
    Exited { code: Option<i32> },
    Observed(ObservedEvent),

    // additive (#2850) — see "The six additive variants" below
    Thinking { turn: TurnId, delta: String },
    ToolOutput { turn: TurnId, id: ToolUseId, delta: String, is_error: bool,
                 replaces: bool },
    UiRequest { id: RequestId, method: UiMethod, title: Option<String>,
                message: Option<String>, options: Vec<String>,
                timeout_ms: Option<u64> },
    UiSettled { id: RequestId, answer: UiAnswer, by: DecisionSource },
    QueueChanged { steering: Vec<String>, follow_up: Vec<String> },
    Note { turn: Option<TurnId>, note: NoteKind, text: String },
}

pub enum ObservedEvent { QuestionSuspected { .. }, ReadyMarker, Quiet, Painted }
pub enum UiMethod { Select, Confirm, Input, Editor }
pub enum UiAnswer { Value(String), Confirmed(bool), Cancelled }
pub enum NoteKind { Retry, Error, Ui }
```

**The six additive variants (#2850).** Each carries something a harness
REPORTS and the human requires visible (#2891), and none of them is a fact any
pane can infer — so none of them is spelled `Observed(..)`, and §1.3's rule is
unchanged. Additive means the same thing here as in
`remote-engine-protocol.md` §4.4: a consumer that does not match them keeps
compiling and keeps working, minus what it does not read.

| variant | what it says | decoder source |
|---|---|---|
| `Thinking{turn, delta}` | streamed reasoning, which is NOT assistant text and must never be concatenated into it — a renderer that quiets thinking (#2891) cannot do so if the two share a variant | pi: `message_update` whose `assistantMessageEvent.type` is `thinking_delta`; claude: the equivalent reasoning delta, R2's to bind |
| `ToolOutput{turn, id, delta, is_error, replaces}` | a tool's output as it streams, keyed to the `ToolUseId` of the `ToolCall` it belongs to. `ToolResult{ok}` still fires once at the end and still carries the verdict; `ToolOutput` carries the bytes, which `ToolResult` never did. `replaces` says whether `delta` appends or supersedes — **see below** | pi: `tool_execution_update` (`partialResult`) and `tool_execution_end` (`result`, `isError`) — accumulated, not incremental |
| `UiRequest{id, method, title, message, options, timeout_ms}` | the harness is asking a HUMAN a question and is blocked on the answer. It is not a `PermissionRequest`: a permission request is a policy decision `permissions.json` may settle without anyone (§3.2), and conflating the two would put an arbitrary extension prompt through a ladder written for tool policy | pi: `extension_ui_request` whose `method` is a dialog method (`select`, `confirm`, `input`, `editor`); the fire-and-forget methods are a `Note`, never this |
| `UiSettled{id, answer, by}` | that question is closed, by whom, and with what. `by: DecisionSource` is the same field `PermissionSettled` carries, for the same reason: an audit that records the answer and not the answerer records nothing worth keeping | the driver's own reply, or the harness self-resolving its own `timeout` (`by: Policy`) |
| `QueueChanged{steering, follow_up}` | what the harness has accepted but not yet run. orrerix's `queue` is the front door and stays so; this is the harness's own downstream queue, and without it a delivered turn that is merely QUEUED is indistinguishable from one being worked | pi: `queue_update` (`steering[]`, `followUp[]`) |
| `Note{turn, note, text}` | something the harness reported that is not a turn, a tool or a question, and that a human still has to be able to SEE. `turn` is an `Option` because these genuinely happen BETWEEN turns, and §1.3's rule is that an absent fact is `None` | pi: `auto_retry_*` and a compaction that did not compact → `Retry`/`Error`; `extension_error` → `Error`; the fire-and-forget UI methods (`notify`, `setStatus`, `setWidget`, `setTitle`, `set_editor_text`) → `Ui` |

**`timeout_ms` is descriptive, not a control.** It reports a deadline the
HARNESS is keeping, so a renderer can show one; orrerix never starts a timer
of its own against it, for §3.5's reason.

**`ToolOutput.delta` is a DELTA, and one harness will have to convert.** The
two sources disagree: pi's `partialResult` "contains the accumulated output so
far (not just the delta), allowing clients to simply replace their display on
each update" (`docs/rpc.md:1055` at 0.85.1), while a delta is what a
stream-json reader gets natively. The contract picks the delta, because the
conversion only goes one way cheaply — a consumer can accumulate deltas with no
state, whereas recovering a delta from an accumulation needs the previous value,
which the ADAPTER already holds and a consumer does not. Making the field
carry whichever the harness happened to send would push that state into every
renderer instead, and there is more than one.

So the pi decoder subtracts: it keeps the last `partialResult` per
`ToolUseId` and emits the suffix. Its precondition is that each update is a
PREFIX-extension of the last, which is what "simply replace their display"
implies but does not promise.

**`replaces` is that precondition's failure carried rather than inferred, and
S1b adds it (#2986).** When an update does NOT extend its predecessor the adapter
emits the whole new value, and a consumer appending deltas would then show the
output TWICE. That cannot be closed downstream: a consumer cannot tell a
restatement from a legitimate delta that happens to repeat earlier bytes, and
any heuristic for it ("does this restate what I hold?") silently eats
genuinely repeating output, which is a worse failure than the one it fixes. So
the fact travels on the event, by the same reasoning that put the subtraction
in the adapter. `false` on every ordinary delta means **append**; `true` means
`delta` is the whole current output for that `ToolUseId` and **replaces**
everything held for it. A consumer that ignores the field is no worse off than
before it existed, which is what makes the addition additive.

**`Note` is the sixth variant, and it exists because the decoder table needs
somewhere to put three things.** Retries, extension errors and the
fire-and-forget UI methods were all routed to "a Note" before one existed on
this enum — only `LogBody::Note`, a log record that reaches no consumer. The
consequence was concrete: a pane spending four minutes in an API retry loop
showed a human nothing at all, and an extension that threw was invisible
outside a file nobody opens. `NoteKind` is the closed set a decoder may
produce (`Retry`, `Error`, `Ui`).

**It is not a channel for everything the stream says.** Protocol bookkeeping —
message boundaries, turn-internal events, settle events, command
acknowledgements — stays log-only: it is per-message volume with nothing for a
human to do about it, and an event stream carrying it would drown the three
kinds this variant exists for. The decoder is where that judgement lives.

### 1.3 How a PTY pane maps onto it — and how "unknown" is spelled

A PTY pane implements the **same** trait and emits the **same** enum. It is not
given a parallel vocabulary, because a second vocabulary means a second consumer
for every feature that reads panes.

**Unknown has exactly two spellings, and neither is a value.**

1. **A fact the pane does not have is `None`**, never a sentinel. An `"unknown"`
   string or a `-1` is a value every `match` arm can forget to check, and it
   reads as data. `session_id() -> Option<String>` and `usage: Option<Usage>`
   are the shape; a PTY pane answers `None` until the session watcher binds an
   id (`session-id-learning.md`).
2. **A fact the pane only INFERRED is `Observed(..)`**, never the reported
   variant. Scraped evidence and reported fact must not share a constructor: a
   consumer written for `PermissionRequest` would otherwise silently accept a
   grid heuristic, and the whole point of the structured path is that it does not
   have to. This is the one place the enum is deliberately not symmetric.

| event | structured (claude) | PTY |
|---|---|---|
| `Booted` | `system/init`: `session_id`, `model`, `capabilities` | emitted on the readiness gate; `session` `None` until learned, `model`/`capabilities` empty |
| `TurnStarted` / `TurnEnded` | per turn, from the stream | **never emitted** — a PTY pane cannot see a turn boundary |
| `Text` | assistant text and `stream_event` deltas | **never** — the bytes go to the ring; there is no message structure to report |
| `ToolCall` / `ToolResult` | `tool_use` / `tool_result` blocks | **never** |
| `PermissionRequest` / `PermissionSettled` | the permission-prompt tool call and its answer | **never** — a suspected question is `Observed(QuestionSuspected{..})` |
| `TurnEnded{usage, cost}` | the `result` message (§7) | **never** — usage is polled out of band, not evented |
| `Compacted` | claude: `system` / `compact_boundary` (`SDKCompactBoundaryMessage`, documented on the Agent SDK surface only — the CLI capture that confirms it on the wire is §9 item 7). pi: `compaction_end`, **not** `compaction_start` — see below | **never** — today's marker hooks stay, and they drive the existing detector, not this event |
| `Exited` | child exit | child exit — the one variant both kinds emit identically |
| `Thinking` / `ToolOutput` / `QueueChanged` | pi: §1.2's decoder sources; claude: R2's to bind | **never** — a PTY pane has no reasoning stream, no per-tool output channel, and no view of the harness's own queue |
| `Note` | pi: retries, a failed compaction and `extension_error` (`Retry`/`Error`), fire-and-forget UI methods (`Ui`); claude: R2's to bind | **never** — a PTY pane's equivalent is bytes on the screen, which is what `Observed(..)` covers |
| `UiRequest` / `UiSettled` | pi: `extension_ui_request` and its reply (§3.5) | **never** — a dialog painted in a TUI is at most `Observed(QuestionSuspected{..})` |
| `Observed(..)` | **never emitted** | readiness marker, question grid, quiet/painted evidence |

**`Compacted` is emitted when a compaction FINISHES, and that is a
correction to plan-2386's decoder row (S1b, #2986).** Two facts the start event
cannot supply: `compaction_start` carries only `reason`, so a `Compacted`
built from it has `pre_tokens: None` always and the field is dead, while
`compaction_end.result.tokensBefore` is the real figure; and a compaction can
abort or fail, so emitting on the start event reports a compaction that never
happened with nothing later retracting it. A compaction that did not complete
is a `Note{Error}` — a human whose pane is about to hit its context window
needs to see that, and the symptom otherwise is a pane that stops working for
no visible reason. Exactly one `Compacted` per successful compaction.

**The two "never emitted" columns are the contract, not an omission.** A feature
that needs `ToolCall` is a feature that works on structured panes and degrades
— visibly, by absence — on PTY panes; it may not fall back to scraping one out of
the ring, because that is the machinery this issue exists to retire.

---

## 2. The `driver:` block key

### 2.1 The key

```yaml
blocks:
  - id: worker-adv
    kind: worker
    cli: claude
    driver: structured      # pty | structured   — default: pty
```

`.orrerix/workflow.yml`, with `.loomux/` still read as the legacy name
(`brand::pick_repo_path`, unchanged by this note).

### 2.2 What it changes

- Which `AgentPane` implementation `PaneHost::request_pane` returns for panes
  spawned from that block.
- The argv that implementation builds (§6).
- Whether the attention scan and the screen-scraped question gate run for that
  pane: **off** for `Structured` (§5.4).
- Whether `write_pty` is accepted for that pane: **refused** for `Structured`
  (§5.5).

### 2.3 What it must NOT change, and why each is mechanical

**It can never grant a capability.** `workflows.md`'s rule is absolute and this
key is inside it: `driver:` is a selection from a closed two-value enum, and
`Role::containment()` — the deny flags, the cwd/worktree rule, the MCP tool scope
— is selected by `kind:`, never by a repo file. Both drivers emit the same
`--disallowedTools`/`--allowedTools`/containment argv for a given class. A
structured driver that dropped a deny flag would be a capability grant by
transport, so R2 owes a test that the containment argv for one block is
byte-identical across both driver values.

**It is refused, never coerced, and the accepted set is a CLI capability rather
than a list in the parser.** `CliCaps` gains
`structured_driver: Option<Harness>` — `Some(Harness::Pi)` on pi's row, `None`
everywhere else — so "which CLIs can be driven structurally" is answered in the
one table that already answers every other per-CLI question, and adding a
harness is a row edit rather than a second enumeration to keep in step. Two
stages then apply, the `cli:`/`effort:` precedent: `parse_workflow` refuses
`driver: structured` on a `cli:` whose `CLI_CAPS` row has `None`, and
`spawn_agent` refuses it again at spawn. An unknown value (`driver: acp`,
before R4 exists) is a `deny_unknown_fields`-class validation error, not a
fall-through to `pty`.

**Accepted on `pi` only.** Refused on `claude`, `copilot`, `gemini` and
`opencode`. claude's row turns `Some(Harness::Claude)` when R2 wires the
stream-json driver (§8.2) — the enum variant exists (§1.1), the capability row
is what gates it, and that is exactly the seam this shape was chosen for.

**It is pinned at launch (#222 consent).** `create_group` reads
`.orrerix/workflow.yml` on `Launch::Fresh` only; a resume runs the blocks
persisted in `group.json`. So editing `driver:` in the repo cannot change the
pane kind of a running or resumed group — the human sees the new roster at the
launcher, or at the live toggle, before it takes effect, and drift on resume is
audited (`workflow-changed-since-launch`), never applied. **A `driver:` change is
exactly the kind that must not arrive silently**: it changes what the human's own
pane shows them.

**It joins the schema manifest, and the name is already taken at the top
level.** `workflow-schema.json` carries a top-level `driver` — the review
driver (#1778) — so the block-section key is only unambiguous if the manifest
is scoped by SECTION. The slice that adds the key (#2850 S3a) verifies that
`workflow_schema_keys()` and `workflow_schema_field_facts()` are keyed on
`(section, name)` and not on `name` alone; if either is flat, the fix is to
key it on the pair, never to rename one of the two live keys — a schema key is
a public surface and the top-level one ships today.

With that settled, `src/workflow-schema.json` gains a `driver`
field on the block section — `values: ["pty","structured"]`, `default: "pty"` —
and both enforcers apply: `src-tauri/tests/orchestration.rs` (field names off
`workflow_schema_keys()`, values off `workflow_schema_field_facts()`, and the
refuse-vs-clamp behaviour driven through `parse_workflow`) and
`test/workflowschema.test.ts` (parsed, serialized, and either claimed by a form
control or explicitly listed as not yet having one).

### 2.4 The roster field

`agents.json` gains `pane_kind` per agent row — additive, absent means `pty`, so
every roster written before R2 reads correctly. It is a **record of what was
spawned**, not a control: nothing reads it to decide how to drive a pane, only to
render it and to answer `PaneKind` for a pane this process did not spawn.

---

## 3. `permissions.json` — the policy, and the prompt the policy cannot decide

A structured pane has no dialog. Every prompt Claude Code would have drawn
becomes a call into orrerix's own MCP server
(`--permission-prompt-tool mcp__orrerix__permission_prompt`, built from
`brand::MCP_TOOL_PREFIX`, never a literal). That call has to be answered by
somebody, and this section says who.

### 3.1 Two things in one file

`<group dir>/permissions.json`, beside `questions.json` and `needs-you.json`,
following `humanq.rs`'s idiom (atomic write, one serializing mutex per group, a
closed source enum, audited settles and refusals):

- **`policy`** — rules that auto-decide, keyed by **agent id**. An agent's cwd is
  its worktree, so "per-worktree policy" and "per-agent policy" name the same
  scope; the file keys on the id because the id is a validated `PathSegment`
  family member and a path is not.
- **`pending`** — requests no rule decided: the tool name, the arguments, when it
  arrived, and once settled the decision and its source.

### 3.2 The decision ladder

1. **Deny rules first**, from the block's class containment. A deny here is final
   and is never raised to the human — a human cannot grant what the class
   forbids, and offering the choice would be the self-served-gate shape CLAUDE.md
   constraint 9 refuses.
2. **Allow rules** — the same patterns the argv path emits today, but applied at
   **decision time**. That is the whole gain: argv can only express what was known
   at launch, while the prompt tool sees the actual arguments, so a policy can
   allow `Bash(git status)` and refuse `Bash(git push)` on the concrete call.
3. **No rule matched → the human.** This is the fail-closed direction, and it is
   §3.3.

### 3.3 How an undecided prompt reaches the human

**Through the needs-you registry, never a pane dialog.** `needs-you.json` gains
an item linking the pending request; the NEEDS-YOU panel renders it beside
questions and demos; the answer arrives through a trusted Tauri command whose
source is a property of the entry point, not an argument. **Every agent may be
prompted. No agent may ever answer a prompt** — the `questions.json` trust
boundary, verbatim, for the same reason: an agent that could answer its own
permission request has a gate that is theatre. R2 owes the boundary test
(`no_agent_token_can_answer_a_permission_request_through_the_mcp_surface`, ending
in a positive control that settles one through the trusted path).

**Why not a dialog in the pane:** INVARIANT 2. A dialog on an orchestrator's
screen stops that pane taking *any* delivery, which strands every agent reporting
to it — the #946 incident. A structured pane has no dialog to draw, and this
design must not reintroduce one on the orrerix side.

**Parking is scoped by role, and the asymmetry is deliberate:**

| block kind | undecided prompt | why |
|---|---|---|
| `worker`, `reviewer`, `planner` | the tool call **parks** until answered; that pane's turn waits | one pane waits on a decision only a human may make — the correct scope, and the same shape as `ask_human` |
| `orchestrator`, `manager` | **denied immediately**, with a message naming the registry, and a needs-you item raised | a parked orchestrator is #946 one level down: machine progress must never stop on human absence |

A parked request is bounded by a **per-pane cap** on pending entries; past the
cap, further prompts are denied with a message naming the cap. It is **not**
bounded by a timer: a timed auto-deny is a decision nobody made, recorded as
though somebody had.

### 3.4 Three Claude Code facts this section is built on

- **With `--permission-prompt-tool` set, Claude Code emits no `permission_denied`
  stream event at all — "not even for the rule denials it decides on its own"** —
  and `permission_denials` on the `result` message is "the authoritative record"
  ([typescript SDK reference, `SDKPermissionDeniedMessage`](https://code.claude.com/docs/en/agent-sdk/typescript#sdkpermissiondeniedmessage)).
  So orrerix's audit of denials is read off `result.permission_denials`, not off
  a per-denial event. A driver that audited only what it was asked about would
  silently under-record every rule denial.
- **An MCP tool marked `_meta["anthropic/requiresUserInteraction"]` cannot be
  approved this way**: "an `allow` result from the prompt tool for a flagged tool
  is converted to a deny"
  ([MCP reference](https://code.claude.com/docs/en/mcp#require-approval-for-a-specific-tool)).
  Consequence: orrerix must not mark its own MCP tools that way, and a `policy`
  allow for such a tool is not honoured — R2 says so in the refusal message
  rather than leaving the allow looking effective.
- **Claude Code waits for the prompt tool's MCP server before the first turn**,
  up to `MCP_TIMEOUT` (30 s by default)
  ([cli-reference, `--permission-prompt-tool`](https://code.claude.com/docs/en/cli-reference)).
  orrerix's MCP server is already listening before a pane spawns, so this is a
  constraint on ordering rather than new work — but the driver must fail the
  spawn loudly if it is not, rather than producing a pane whose every prompt
  times out.

**`AskUserQuestion` stays denied** for adapter agents, as it is for the
orchestrator today, and clarifying questions go through `ask_human`. Whether that
tool even reaches `--permission-prompt-tool` is undocumented — §9.

---

### 3.5 Extension-UI dialogs (#2850) — a second question channel, same trust boundary

A harness may ask a question that is not a permission request at all: pi's
extensions raise `select`, `confirm`, `input` and `editor` dialogs, and in
RPC mode the agent BLOCKS until an answer arrives on stdin
(`doc/design/pi.md`, "RPC driver (#2850)"). These surface as
`UiRequest`/`UiSettled` (§1.2) and are governed here, beside permissions,
because everything §3.3 says about who may answer applies verbatim — and only
that part.

**Not the §3.2 ladder.** `permissions.json` decides tool policy on tool
arguments. A dialog is arbitrary extension text with an arbitrary option list;
there is no rule language that could match it, so there is nothing for a policy
to decide and inventing one would be a rule that fires on a shape it cannot
read.

**Every agent may be asked. No agent may ever answer.** One trusted Tauri
command, `answer_pane_ui(group, agent, request, answer)`, settles a dialog, and
the caller's identity is a property of the entry point rather than an argument
— the `questions.json` boundary, for the reason it exists there: an agent that
could answer its own dialog has a gate that is theatre. The slice that builds it
owes the boundary test, ending in a positive control that settles one through
the trusted path.

**The role asymmetry is §3.3's, and for §3.3's reason:**

| block kind | a dialog | why |
|---|---|---|
| `worker`, `reviewer`, `planner` | **parks** — a needs-you item is raised and that pane waits | one pane waiting on a decision only a human may make is the correct scope, and the same shape as `ask_human` |
| `orchestrator`, `manager` | **cancelled immediately**, audited `by: Policy`, and a needs-you item raised anyway | a parked orchestrator is #946: machine progress must never stop on human absence, and the human still gets to see what was asked |

**Bounded by a cap, never by an orrerix timer.** Pending dialogs per pane are
capped; past the cap a further request is answered `Cancelled` and audited. A
timed auto-answer is a decision nobody made recorded as though somebody had —
§3.3's rule, and it is the same rule. Where the harness keeps a deadline of its
own it self-resolves and reports the outcome, which orrerix records as
`UiSettled{by: Policy}` rather than racing it: `timeout_ms` on `UiRequest` is
descriptive (§1.2).

---

## 4. The per-pane event log

### 4.1 Path and shape

`<group dir>/panes/<agent-id>.events.<n>.jsonl` — one JSON object per line,
append only, `n` a monotonically increasing segment number (§4.2).

`<agent-id>` is interpolated into a **file name**, so it is inside the
`src-tauri/tests/pathseg.rs` scan's trigger shape (an interpolation plus a
file-extension literal in a `format!` template): **R1** adds the allowlist row
and the proof that row names — an earlier draft of this section said R2 would,
which was wrong, because R1 is the slice the interpolation lands in and a scan
that default-denies fires the moment the line exists. The id is a
`pathseg::PathSegment` (CLAUDE.md constraint 6), interpolated **directly** rather
than bound to a `&str` first, so the typed value is at the site the scan reads
and the row's proof pins the only constructor that can set it. `panes/` is
created under the group dir, which is the single `group_dir_at` join — no second
join is added, and R1 takes that directory as an already-resolved `&Path` for
exactly that reason.

Each line carries a monotonic `seq`, a wall-clock `ts`, and the `HarnessEvent` as
emitted.

**Unknown harness message types are ignored, not fatal** (the protocol note's
§4.4 rule applied inward) — but the raw line is written to this log, truncated to
4 KiB, under an `unknown` kind. A silently discarded message is a message nobody
can add support for later.

### 4.2 Rotation — what is contracted, and what is R2's to build

Two things are contracted here. The rest of the mechanics are R2's, and this
section deliberately does not pretend otherwise.

**Contracted — the ceiling.** Size-based: **8 MiB per segment, 4 segments
retained**, so a 32 MiB per-pane ceiling. Deleted with the group directory; no
separate retention policy and no compaction. §4.4's "stronger than H4" claim is
scoped to exactly this ceiling.

**Contracted — a segment is never renamed.** Segments are
`<agent-id>.events.<n>.jsonl` with a monotonically increasing `n`; the live
segment is the highest `n`, rotation *starts a new one*, and retention *deletes
the lowest*. This is a contract rather than an implementation note because the
obvious alternative — shifting `.1` → `.2` on each rotation, the logrotate shape
— renames a file while an append-only writer at `producer` rate holds it open and
a C5 replay reader may be reading it. On this project's Windows baseline that is
the classic sharing-violation shape, and it would appear only under the exact load
this feature creates. Numbering the segments removes the question instead of
answering it: a reader names a segment by `n`, so nothing it holds ever moves
underneath it.

**Not contracted, and R2's to decide with its own note:** when the rotation check
runs relative to the append, how a reader learns a new segment exists, and what a
reader does when the segment it is replaying is deleted by retention mid-read.

### 4.3 What the audit log gets, and what stays here

The group audit log is a small, permanently retained, human-read record of
decisions. It gets, per pane:

- `ToolCall` — name plus a bounded argument preview;
- every `PermissionRequest` with its `PermissionSettled` — decision **and**
  source;
- every `UiRequest` with its `UiSettled` — the question, the answer, **and**
  the `DecisionSource`, for the same reason (§3.5);
- `TurnEnded` usage and cost;
- `Exited`.

It does **not** get assistant text, tool results, `stream_event` deltas,
`Booted` detail, or any of `Thinking`, `ToolOutput`, `QueueChanged` and
`Note` — those four are per-pane. A `Note` is not a decision anybody made: a
retry is the harness coping, not a choice, and the audit log is for choices.
`Thinking` most of all: reasoning prose in the
file a human reads to reconstruct decisions is the drowning this section
exists to prevent, at the highest volume the stream produces. Those stay in the
per-pane log. A transcript in the audit log
would drown the decisions the log exists for, and would put model prose in the
file a human reads to reconstruct what happened.

**The decision-grade set is seven of the seventeen variants** — `ToolCall`,
`PermissionRequest`, `PermissionSettled`, `UiRequest`, `UiSettled`,
`TurnEnded` and `Exited` — and it is enforced rather than described.
`HarnessEvent::is_decision_grade` is the one place the split is expressed, and
the population test over the enum **will** pin both numbers — that the two
lists together cover all **17** variants, so a variant somebody adds cannot be
silently unclassified, and that exactly **7** of them are decision-grade, so a
variant folded into the wrong half keeps the total right and still fails.
Both are written and reviewed on `feat/2850-s1b-pi-rpc-core` (#2986, **open**
at the time this note was last edited), so until that slice merges the counts
here are this note's claim and nothing on `main` checks them.

#2850 moved that set from five to seven by adding `UiRequest`/`UiSettled` — a
dialog is a question a human answered, which is what this log is for — and
kept `Thinking`, `ToolOutput`, `QueueChanged` and `Note` out of it. The
population went 11 → 17: five variants in the contract as first written, plus
`Note`, which S1b adds on discovering the decoder has three things to report
and nowhere to put them.

### 4.4 Relationship to the remote protocol's H4

H4's reattach contract — "the live screen and a recent tail (256 KiB per pane),
not infinite scrollback" — is a fact about the **PTY ring**. A structured pane
can replay from this log instead, so #888's C5 reattach is *stronger* for
structured panes: full transcript back to the oldest retained segment. That is a
ceiling, stated: 32 MiB of events, not infinite either. This is the one place
this roadmap raises a #888 contract rather than consuming one.

---

## 5. The rendering contract

### 5.1 Two projections, one log

**The event log is the record; a projection is a view of it, and there are
two.** Both are fed from the same `events()` stream by the same drainer, in
that order, so neither can show what the other has not seen.

| projection | consumer | mechanism |
|---|---|---|
| **VT bytes** | `get_output`, termgrid replay, thumbnails, `last_exit_tail`, C5 replay-on-attach | a pure `transcript::Renderer` in the engine turns each `HarnessEvent` into VT bytes, into that pane's `OutputBuf` ring through the same coalescer that feeds `pty-output` today |
| **the human's surface** | the pane cell | a designed DOM renderer, fed the events themselves over `orch-pane-event` |

The ring projection is why every one of those five keeps working with no API
change; the forward-only rule below (§5.2) is a property of THAT projection and
is unchanged by anything here.

**What R1 promised, and why it changes.** §5.1 as R1 wrote it said a structured
pane renders into the existing xterm surface, and rejected "a DOM transcript
view beside the terminal" precisely because it would break those five
consumers. The rejection was right about the CONSEQUENCE and wrong about the
CHOICE it was forced into: keeping the ring and putting the human on a DOM
surface are not alternatives, because the ring is fed from the log rather than
from the screen. The human's standing requirement (#2891) is a renderer that
shows thinking, tool calls with streamed output, commands with exit codes, and
orrerix's own events as distinguishable, animated, designed things — none of
which survives being flattened to VT and re-parsed by a terminal emulator. So
the DOM renderer is the visible surface and the ring keeps its five consumers,
which is what "two projections" buys.

**Still a pane.** The renderer lives in the grid cell the terminal would, on
the existing `Pane.contentKind` mechanism — same header, attention chrome,
thumbnails, focus and sizing. There is no PTY behind it and nothing about it
reaches a resize (constraint 1).

**One risk this shape creates, and where it is closed:** two projections of one
log can disagree. The slice that builds the DOM renderer owes a parity control
comparing block counts by kind derived from each projection over the same
recorded log.

### 5.2 Constraint 1 is satisfied by construction, and by one rule

There is no PTY behind a structured pane, so no code path can call a ConPTY
resize for one. The failure constraint 1 exists to prevent — a repaint that
pollutes scrollback — can still arrive by a different road, so:

> **The renderer never rewrites bytes it has already emitted.** Lines are wrapped
> to the pane's cols at emit time; a later width change re-wraps nothing. Reflow
> of already-emitted output is xterm's own, exactly as for a PTY pane.

A renderer that re-emitted its transcript on a resize would be constraint 1's
failure without a ConPTY — worse, because it would not even be findable by
grepping for the resize call.

### 5.3 H4's ring rules apply unchanged

The coalescer bound (at most one emit per pane per 16 ms, 64 KiB batch), the
**tee ordering** (append to the ring *before* sending to the pump channel, so no
client's link quality can affect orchestration), and the remote translation of P6
(a bounded per-client send buffer; on overflow, drop that client's stream and send
a `0x02` resync marker) are properties of the **sink**, not of the source. They
hold for renderer output verbatim. The two new event names join
`test/perfpolicy.test.ts`'s `STREAMS` manifest with a rate class and a bound
(§5.6) — INV-3 is keyed by event name, and a stream that arrives undeclared is the
failure that manifest exists to stop. Those bounds are properties of the SINK,
so they hold for the DOM projection's batches exactly as they held for the VT
one: the coalescer does not know what it is coalescing.

### 5.4 The attention scan and question gate are OFF for `Structured`

orrerix must never scrape what orrerix itself rendered. Beyond being pointless,
it is forgeable by construction: model prose shaped like a question grid would be
read as one. Attention and pending-request state for a structured pane come from
the event stream, from `permissions.json` and from a pending `UiRequest`
(§3.5), which is also what makes them answerable from the board and from a
remote client — the thing scraping could never give #888.

### 5.5 Input

`write_pty` on a structured pane is **refused**, not silently accepted: there is
no PTY, and a write that appears to succeed and reaches nothing is the worst of
the three options. Human input arrives as `Turn::Human` from the #43 compose
strip, which is already the serialized human path for the orchestrator pane. On
the wire this is a typed refusal (`invalid_argument`), and the roster's
`pane_kind` lets a client grey the keyboard out rather than discover it by
trying.

A dialog is answered through `answer_pane_ui` (§3.5), never by typing into the
pane: the harness is reading a JSON reply on stdin, not a keystroke, and a
compose-strip answer would have nowhere to go.

Notices are `Turn::Notice`. Nothing scrapes a structured pane, so nothing needs
masking: `mask_loomux_notices_with_record` and the one-row maskability contract
stay PTY-only, and the `[orrerix]` marker survives on a structured pane as a
display prefix.

### 5.6 The two additive protocol events

`remote-engine-protocol.md` §4.4 is additive-only, and these are additive:

| event | rate class | bound |
|---|---|---|
| `orch-pane-event` | `producer` | backend-coalesced — at most one emit per pane per 16 ms, carrying at most 64 events or 64 KiB, whichever binds first |
| `orch-pane-request` | `lifecycle` | one event per permission request and one per settle, and the same for a `UiRequest`/`UiSettled` pair (§3.5); a pane cannot produce them faster than it produces tool calls |

**`orch-pane-event` REPLACES `orch-pane-transcript`** (#2891). The transcript
event carried rendered VT bytes for a client to write into a terminal; the DOM
renderer needs the events (§5.1), and shipping both would be two copies of one
log on the wire with no consumer for the first. Nothing has emitted
`orch-pane-transcript` — no structured pane has ever been spawned — so this is
a rename of an unimplemented contract, not a wire break. It carries
`{group_id, agent_id, events[]}`; `orch-pane-request` is unchanged.

No new **frame kind**: both ride the existing `{"t":"ev","name":…}` shape. A
client that does not know them ignores them, which is §4.4's rule doing its job.

---

## 6. Session id and resume for Claude Code under stream-json

### 6.1 The launch line

```
claude -p
  --input-format stream-json --output-format stream-json --verbose
  --include-partial-messages
  {--session-id <uuid> | --resume <id>}
  --mcp-config <file> --strict-mcp-config
  --permission-prompt-tool mcp__orrerix__permission_prompt
  --permission-mode <mode> --allowedTools ... --disallowedTools ...
  --settings <hooks file> --agent <handle> --effort <level>
```

Every flag is on
[cli-reference](https://code.claude.com/docs/en/cli-reference) today:
`--input-format` ("options: `text`, `stream-json`"), `--output-format` ("`text`,
`json`, `stream-json`"), `--include-partial-messages` ("Requires `--print` and
`--output-format stream-json`"), `--session-id` ("must be a valid UUID"),
`--resume`, `--mcp-config`, `--strict-mcp-config`, `--permission-prompt-tool`,
`--permission-mode`, `--allowedTools`, `--disallowedTools`, `--settings`,
`--agent`, `--effort`, `--verbose`.

Two flags are deliberately **not** used:

- **`--bare`** — it "skip[s] auto-discovery of hooks, skills, custom commands,
  subagents, plugins, MCP servers, auto memory, and CLAUDE.md"
  ([headless](https://code.claude.com/docs/en/headless#start-faster-with-bare-mode)).
  A pane that does not load the repo's CLAUDE.md is not the pane orrerix launches
  today.
- **`--no-session-persistence`** — it makes the session unresumable
  ([cli-reference](https://code.claude.com/docs/en/cli-reference)), and resume is
  the feature.

### 6.2 The id is minted, not learned

`--session-id` pre-assigns the id, exactly as the PTY path does today, and
`system/init` reports `session_id` back
([`SDKSystemMessage`](https://code.claude.com/docs/en/agent-sdk/typescript#sdksystemmessage)).
So for a structured claude pane there is nothing to discover: **the session
watcher does not run**, and `session-id-learning.md`'s reconciliation and its
"refuse, never guess" ambiguity policy stay scoped to the panes that still need
them — copilot, opencode, and every PTY claude pane.

**Decision — a mismatch kills the pane.** If `system/init` reports a `session_id`
different from the one orrerix minted, the driver terminates the pane and audits
it rather than adopting the reported id. A mismatch means the flag did not take
effect, so the pane is not the session orrerix believes it is, and every
downstream record — usage, resume, transcript — would key on the wrong one. Fail
closed; do not reconcile.

**And the comparison is CANONICALIZED, not a string compare.** cli-reference says
`--session-id` "must be a valid UUID"; **no page states the textual form
`system/init` echoes back**, so a literal `==` would kill every structured pane
at spawn on any build that answers in a different case or spelling — a total
outage produced by the safety check, not by the failure it guards. Both sides are
parsed as a UUID and compared as 128 bits; a value that does not parse at all is
itself a mismatch, so the relaxation does not open a hole. The exact form the CLI
reports is a §9 item (6), and until it is measured this rule is the reason the
gap is not load-bearing.

### 6.3 Resume

`--resume <session-id>` resumes by id. A `-p` session is out of the picker and
out of `--continue`, but "You can still resume one by passing its session ID to
`claude --resume <session-id>`"
([sessions](https://code.claude.com/docs/en/sessions#resume-a-session)); the
search order is on the flag's own row — "When you pass a session ID, Claude Code
searches the current project directory and its git worktrees, then every other
project on this machine"
([cli-reference](https://code.claude.com/docs/en/cli-reference)).

**Three resume facts, and they are the whole reason this section exists:**

1. **Configuration flags are NOT restored.** "If the session depended on
   `--mcp-config`, `--settings`, `--plugin-dir`, `--fallback-model`, or
   directories added with `--add-dir`, pass them again when you resume"
   ([sessions](https://code.claude.com/docs/en/sessions#what-a-resumed-session-restores)).
   So the resume argv repeats **every** flag of the fresh launch, and R2 owes a
   test that the two argv lines differ in exactly one element
   (`--session-id <uuid>` becoming `--resume <id>`).
2. **The permission mode is NOT restored under `-p`.** "Non-interactive: `claude
   -p --resume` or `claude -p --continue`. Claude Code starts the run in the
   permission mode a new `claude -p` run would start in"
   ([sessions](https://code.claude.com/docs/en/sessions#permission-mode-on-resume)),
   and for `-p` "the built-in starting permission mode is Manual on every plan,
   so pass the permission mode you want"
   ([headless](https://code.claude.com/docs/en/headless#auto-approve-tools)).
   `--permission-mode` is therefore mandatory on both lines, not an optional
   posture knob.
3. **One session, one owner.** "If you resume the same session in two terminals
   without forking, messages from both interleave into one transcript"
   ([sessions](https://code.claude.com/docs/en/sessions#branch-a-session)). A
   structured pane and a PTY pane must never hold one session id at once. The
   roster's single-owner rule already enforces that; the consequence of breaking
   it is a corrupted transcript, so it is stated rather than assumed.

`--fork-session` — "When resuming, create a new session ID instead of reusing the
original" ([cli-reference](https://code.claude.com/docs/en/cli-reference)) — is
the documented way to branch, and is **not** used by R2: a forked id is a
different pane identity, and orrerix's identity is the minted one.

### 6.4 Termination

"If you stop a `claude -p` run with SIGTERM … Claude Code exits with code 143 …
leaves the turn that was in progress unfinished and records no result for it. To
end the turn instead, send SIGINT"
([headless](https://code.claude.com/docs/en/headless#stop-a-run-with-sigterm)).

So `kill_agent` on a structured claude pane ends the turn first and kills second.
What "send SIGINT" means on this project's Windows baseline is §9's item 4.

---

## 7. Usage and cost, per harness

### 7.1 What stream-json gives directly

The `result` message — the last line of a turn's stream
([headless](https://code.claude.com/docs/en/headless#stream-responses)) — carries
the numbers in band, with no file to find:

| field | what it is |
|---|---|
| `usage` | token counts for **that turn**, **main loop only** — "Excludes subagent and auxiliary model calls, and is per-turn in streaming-input sessions" |
| `modelUsage` | per model: `inputTokens`, `outputTokens`, `thinkingTokens?`, `cacheReadInputTokens`, `cacheCreationInputTokens`, `webSearchRequests`, `costUSD`, `costBasis?` — subagents included |
| `total_cost_usd` | cumulative estimated USD for the call, subagents included |
| `permission_denials` | the authoritative denial record (§3.4) |

([`SDKResultMessage`](https://code.claude.com/docs/en/agent-sdk/typescript#sdkresultmessage),
[`ModelUsage`](https://code.claude.com/docs/en/agent-sdk/typescript#modelusage),
[cost-tracking](https://code.claude.com/docs/en/agent-sdk/cost-tracking).)

**Four traps, each a decision R2 implements rather than discovers:**

1. **`modelUsage` and `total_cost_usd` are CUMULATIVE for the call; `usage` is
   per turn.** "read the latest result for call totals rather than summing across
   results". Summing `total_cost_usd` across a long-lived pane's turns would
   multiply its cost by roughly the turn count.
2. **They reset on `/clear`, `/reset` and `/new`**, and the `/clear` turn's result
   "carries a new `session_id`". orrerix does not send those to a delegate pane,
   but a human at the compose strip can — so the collector reads the reset
   boundary rather than assuming monotonicity.
3. **`thinkingTokens` is already inside `outputTokens`** — "don't add the two
   together". orrerix's four buckets take it folded into `output`, the same fold
   `opencode.md` already argued for reasoning tokens, so one bucket keeps meaning
   one thing across harnesses.
4. **`usage` undercounts as soon as subagents run**; `modelUsage` is the
   whole-tree figure, and it is the one R2 reads.

### 7.2 Basis, honestly labelled

`total_cost_usd` and `costUSD` are, in the vendor's own words, "client-side
estimates, not authoritative billing data … Do not bill end users or trigger
financial decisions from these fields"
([cost-tracking](https://code.claude.com/docs/en/agent-sdk/cost-tracking)).

So a structured claude pane's dollars land in `group_usage`'s **estimated** basis
— the same basis its PTY sibling uses — not in `reported`. What changes is the
**source tag**, from `transcript`/`statusline` to `stream`. Nothing else in
`group-cost-tracking.md` moves.

### 7.3 The #2167 class

#2167 is a **source-resolution** failure: the collector derives
`~/.claude/projects/<cwd-slug>/<session>.jsonl` from a pane's cwd, that stopped
resolving for worktree cwds, and the fallback is a statusline that reads `$0.00`
on subscription plans — so Claude delegate panes recorded 0 tokens.

A structured pane derives no path at all. The numbers arrive on the stream the
driver is already reading, so the slug derivation, the projects-root scan and the
statusline fallback are all out of the loop for those panes. **This does not fix
#2167**, which is about PTY claude panes and stays open; a structured pane is
simply not exposed to it. (Out of scope here, and recorded because it is the same
class: the docs now describe `CLAUDE_CODE_PROJECT_DIR_NAME` alongside
`CLAUDE_CONFIG_DIR` as a way to *pin* the project directory instead of deriving
it — [sessions](https://code.claude.com/docs/en/sessions#name-the-project-directory-yourself)
— a candidate fix for #2167's PTY half.)

### 7.4 Per harness

| harness | usage source today | under a structured driver |
|---|---|---|
| claude | transcript JSONL, statusline fallback (#2167) | **the `result` message**, in band, `estimated` basis, source `stream` |
| opencode | session DB row plus a recursive `parent_id` rollup, `reported` basis | unchanged — R3 keeps the DB reader; the HTTP API is the control path, not the meter |
| copilot | no readable token record; statusline only | unknown until R4 reads the ACP surface — not claimed here |
| gemini | statusline | PTY only |

---

## 8. The R1 → R2 slice contract

### 8.1 R1 — `feat/harness-claude-core`, an engine LEAF

**Files:** `crates/loomux-engine/src/harness/mod.rs`, `harness/claude.rs`,
`harness/transcript.rs`, plus one `pub mod harness;` line in
`crates/loomux-engine/src/lib.rs` — and **one allowlist row in
`src-tauri/tests/pathseg.rs`** (§4.1). That last one is a test file, not
`mod.rs`, so the zero-`mod.rs` property below is untouched; it is named here
because a slice that adds a line to a default-deny guard should say so in its
own file list rather than have a reviewer discover it.

**Ships:** §1's vocabulary; the stream-json decoder (NDJSON line →
`HarnessEvent`); a child-process driver owning stdin/stdout/stderr and the
child's lifetime; the VT renderer (`HarnessEvent` → bytes).

**Does NOT ship, and R1 is not done by shipping any of it:** an edit to
`src-tauri/src/orchestration/mod.rs` (**zero** — this is what makes R1 parallel
with the A4 chain); a `#[tauri::command]`; the `driver:` key or any `CLI_CAPS`
change that makes it parseable; `permissions.json`; the MCP tool; any wiring into
a spawn path; any role-template edit (so no `pre222` re-bless).

**Dependencies: none.** `serde_json` and `std` only. Nothing that pulls
`getrandom` (constraint 2) — the pane's `--session-id` UUID is minted by the
existing `RandomState` path, not by a new crate.

**Decoder rules:**

| stream line | `HarnessEvent` |
|---|---|
| `system` / `init` | `Booted { session, model, capabilities }` |
| `assistant`, text block | `Text` |
| `assistant`, `tool_use` block | `ToolCall` |
| `user`, `tool_result` block | `ToolResult` |
| `stream_event` | `Text` delta |
| `system` / `compact_boundary` | `Compacted { trigger, pre_tokens }` |
| `result` | `TurnEnded { usage, cost, stop }` |
| `system` / `api_retry` | a log line only; not a `HarnessEvent` |
| anything else | ignored as an event; the raw line goes to the pane log (§4.1) |

**Tests.** Fixtures under
`crates/loomux-engine/tests/fixtures/harness/claude/*.jsonl`. Agents never
regenerate them and never run a real CLI (constraint 3). Decoder tests are
table-driven; a planted unknown message type must be ignored **and logged**
rather than panic; the renderer is pinned on golden VT output. Red-before-green
is captured on CI, since agents may not run cargo locally.

**Three things this paragraph said before R1 built it, and what replaced them
(location and provenance human-approved 2026-09-04; the third is a correction
rev-final round 10 found — the earlier list said "two" and did not account for
its own diff):**

- *Location.* It said `src-tauri/tests/fixtures/`. R1's tests are engine-inline,
  and an engine test reaching `../../src-tauri` for its data points the boundary
  arrow backwards even though nothing links — the arrow is a rule about what this
  crate may depend on, and test data is a dependency. The fixtures live with the
  crate that owns the decoder; R2's `src-tauri` integration test reads that same
  path.
- *Provenance.* It said the fixtures are "recorded once by the human from a real
  CLI". They are not, and could not be for R1: constraint 3 forbids an agent
  producing a recording, and none had been made. **R1's fixtures are synthesized
  from the documented message shapes** and their README says so in its first
  line. They therefore prove the decoder against the **documented** contract and
  not against the CLI's real bytes — a real capture is a live-validation item in
  §9's family, and it is what would close the gap. The rest of the discipline is
  unchanged: a capture that replaces them records the CLI version it came from,
  and no agent regenerates a fixture from the code under test.
- *Mechanism, and this one is a DE-contracting rather than a replacement.* It
  said "the driver is exercised against a **fake harness script** that cats a
  fixture on a schedule". Nothing does that, and nothing is contracted to.
  R1 **split the driver instead of faking a process**: the decode-and-publish
  loop takes a `BufRead` (`harness::claude::pump`), so it is driven over an
  in-memory reader and over the fixture, and the argv builder, the session-id
  comparison, the decoder and the renderer are pure functions with tests of
  their own. That is a better bar than the script for everything below the
  reader — no process, no schedule, no platform variance.
  **What it leaves unexercised, said here so the absence cannot read as
  coverage:** the process seam itself — `Command` → piped stdin/stdout → the
  reader thread → `Drop` — has **no test in R1**, because a fake script is the
  only way to reach it and constraint 3 keeps a real CLI out. So the bar R1's
  reviewer holds the driver to is: *every branch below the reader is tested, and
  the spawn seam is a stated residual*, closed by R2's `src-tauri` integration
  test once there is a pane to spawn into. A driver with no test at all does
  **not** satisfy this section; a driver whose logic is tested and whose spawn
  seam is named as a residual does.

The fixture set deliberately carries the awkward cases an average capture would
miss: a `thinking_delta` (which must not become transcript text), a **failing**
`tool_result`, an `api_retry` (a real message that is not a pane event), a
message type no build knows, an unrecognized `capabilities` entry, and **two**
models in `modelUsage` — so a reader that folds only the first is caught.

### 8.2 R2 — `feat/harness-claude-wire`

Waits on A4-18′ (`PaneHost::request_pane -> Box<dyn AgentPane>`) and edits
`mod.rs`'s spawn path, so it serializes against the A4 chain.

**Ships:** `driver: structured` honoured at parse and at spawn, with the
`CLI_CAPS` row (`structured_driver: Option<Harness>`) and the schema-manifest
entry (§2); the drainer's last step becoming `pane.send(turn)`; the
`permission_prompt` MCP tool (five-place registration per the `add-orch-tool`
skill) and `permissions.json` (§3); the needs-you wiring for an undecided prompt;
the per-pane event log (§4); renderer output into the ring plus the two additive
events (§5.6); `pane_kind` on `agents.json`; the usage collector's `stream` source
(§7); and a `docs/` page for the workflow key.

**The contract between the two slices:** R1 defines the types; **R2 may not
change a name or a shape in §1, §4.1 or §5.6 without amending this note in the
same PR.** That is what makes R0 worth reviewing before R1 exists.

**#2850 moves four of R2's items ahead of it, and the rule above is why they
appear here rather than in R2's PR.** The `CLI_CAPS` row, the schema-manifest
entry, the drainer's `pane.send(turn)` step and the two additive events are
built by the pi slices against §1, §2.3, §3.5 and §5 as amended — so R2's list
is now the Claude-specific remainder: `Harness::Claude` on the `CLI_CAPS` row,
the `permission_prompt` MCP tool, `permissions.json`, and the `stream-json`
decoder. The shared surfaces are amended in THIS note first, which is the rule
holding rather than being waived.

**And §8.1's three lists — Ships, Does NOT ship, Tests — are protected the same
way**, which the earlier wording did not say. They are where a slice's *bar*
lives, and a bar is the one thing whose removal nothing can redden: the note IS
the test, so a deleted clause reads as compliance rather than as a gap. This
paragraph exists because that fired inside this note's own history — the
fake-harness-script sentence left §8.1 in a commit whose change list said "two
things" and enumerated the other two, and it survived eight review rounds
because every round checked the note's figures and citations and none diffed a
change list against its own diff. So: **a commit that edits any of those lists
states, in its own summary, every clause it removed** — not only the ones it
replaced.

**Behavioural-silence bar, inherited from the extraction note:** the existing
integration suite green with **zero test edits** for everything that is not a
structured pane. A group with no `driver:` key is byte-for-byte what it is today
— `default_roster_command_lines_now_carry_the_durable_contract_via_a_generated_claude_agent_file`
(`src-tauri/tests/workflow.rs`) is the pin that says so, and the `pre222` fixture
pins are the other half.

---

## 9. Live-validation items for the human

Each is a fact the official docs do **not** settle, with the pages searched named
beside it. None is guessed anywhere above; **R1 depends on none of them to
build**, and R2 depends on 1, 2, 3, 6 and 7 — item 4 is R2's teardown path and
item 5 is a policy read rather than a measurement. Item 7 is the one R1 *created*
rather than inherited, and it is stated as a debt against R1's own evidence.

1. **The `--permission-prompt-tool` wire contract.** The docs state that the flag
   "specif[ies] an MCP tool to handle permission prompts in non-interactive mode"
   and that an `allow` result exists
   ([cli-reference](https://code.claude.com/docs/en/cli-reference),
   [mcp](https://code.claude.com/docs/en/mcp#require-approval-for-a-specific-tool)),
   but **the tool's input JSON schema and its expected return shape are not
   documented** on cli-reference, headless, mcp, agent-sdk/permissions or
   agent-sdk/user-input. The SDK's in-process analogue returns
   `{ behavior: "allow", updatedInput }` or `{ behavior: "deny", message }`
   ([typescript SDK](https://code.claude.com/docs/en/agent-sdk/typescript)); the
   channels relay uses a third shape again — `request_id`, `tool_name`,
   `description`, `input_preview` in, `{request_id, behavior}` back
   ([channels-reference](https://code.claude.com/docs/en/channels-reference#relay-permission-prompts)).
   **Needed:** one real session, one prompt, the exact request and the accepted
   response recorded as a fixture.
2. **Does `AskUserQuestion` reach the prompt tool?** The SDK docs say it "always
   fall[s] through to the callback"
   ([agent-sdk/permissions](https://code.claude.com/docs/en/agent-sdk/permissions))
   and say nothing about the `--permission-prompt-tool` path. orrerix's plan
   denies the tool either way, so this decides only whether the denial is visible
   as a prompt or silent.
3. **Do the containment flags compose with the stream-json line?** `--agent`,
   `--settings`, `--allowedTools`, `--disallowedTools` and `--effort` are
   documented as flags with no stated mode restriction, but no page shows them
   alongside `-p --input-format stream-json --output-format stream-json`. §2.3's
   "byte-identical containment argv across both drivers" test only means something
   if the CLI honours them there.
4. **Interrupt on Windows.** The docs prescribe SIGINT before SIGTERM
   ([headless](https://code.claude.com/docs/en/headless#stop-a-run-with-sigterm)),
   which this project's Windows baseline does not have as such. Whether the
   documented `interrupt` control request over stdin is the right substitute — and
   whether this CLI build advertises `interrupt_receipt_v1` in
   `system/init`'s `capabilities` — is a live check.
5. **The auth-policy sentence — a human call, not an engineering one.** The Agent
   SDK overview states: "Unless previously approved, Anthropic does not allow
   third party developers to offer claude.ai login or rate limits for their
   products, including agents built on the Claude Agent SDK. Use the API key
   authentication methods"
   ([agent-sdk/overview](https://code.claude.com/docs/en/agent-sdk/overview)).
   orrerix would call **no SDK**: it launches the user's own `claude` binary with
   the user's own login, exactly as the PTY path does today. Whether that sits
   inside or outside that sentence is the human's read. Nothing in R1 depends on
   it; R2 ships a structured pane that does.
6. **The textual form of the echoed `session_id`.** cli-reference says
   `--session-id` "must be a valid UUID"; no page states whether `system/init`
   echoes that UUID back in the same case and spelling it was passed in.
   §6.2's mismatch check is therefore specified as a canonicalized 128-bit
   comparison rather than a string compare, which makes the gap non-fatal — but
   the first real session should record the exact form, because a build that
   answered with something that is not a UUID at all would take the fail-closed
   branch and deserve to be recognised as a version issue rather than a bug.
7. **A real stream-json capture.** R1's fixtures are synthesized from the
   documented message shapes (§8.1), so the decoder is proved against the docs
   and not against the CLI's bytes. A field the pages describe loosely, a key
   spelled differently in practice, or a message type the docs do not mention
   would pass the suite and fail live. One recorded session, committed with the
   CLI version it came from, closes it — and it is the human's to record, because
   an agent may not run a real CLI (constraint 3). R2 depends on this the way it
   depends on item 1: not to compile, but to be believed.
