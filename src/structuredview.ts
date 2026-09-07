// The structured pane's PROJECTION: a pure reducer from a stream of harness
// events to the block list a renderer draws (#2891 S2).
//
// This is the DOM half of `harness-adapters.md` §5.1's "two projections, one
// log". The other half — VT bytes into the `OutputBuf` ring, which keeps
// `get_output`, replay, thumbnails and `last_exit_tail` working — is
// `transcript::Renderer` in the engine, in Rust. Both are fed from the same
// `events()` stream by the same drainer, so neither can show what the other
// has not seen. (`demo/structured-pane/DESIGN.md` §9 assigned the VT half to a
// `projectText()` here; §5.1 landed after that mock and puts it in the engine.
// The note wins, as that file says it must.)
//
// Self-contained by the `timelinelayout.ts` / `embedsplit.ts` rule: **no
// intra-`src` imports at all** (TS5097), so the harness vocabulary is
// re-declared here as types rather than imported. That is not only import
// hygiene — it is what lets `test/structuredview.test.ts` run this module
// under `node --test` with no bundler, and what keeps a DOM out of it.
//
// THREE RULES CARRIED FROM THE CONTRACT, because each is one a renderer can
// silently break:
//
//  1. **Unknown is not a value** (§1.3). A fact the pane does not have is
//     `null`, never a sentinel string. An orphan `tool_result` produces a card
//     whose `name` is `null`, not `"unknown"`.
//  2. **Thinking is not text** (§1.2). `Thinking` and `Text` never share a
//     block, because a renderer that quiets thinking cannot do so if they do.
//  3. **An unknown event kind is ignored, not fatal — but it is RECORDED.**
//     `HarnessEvent` is an additive enum: "a consumer that does not match them
//     keeps compiling and keeps working, minus what it does not read" (§1.2).
//     A kind this build does not know becomes a notice block carrying the raw
//     kind, so a later slice can find it. Nothing here throws on input.
//
// AND ONE THIS MODULE OWNS: **an elision the reader cannot see is a transcript
// that lies** (`DESIGN.md` §7). Both ceilings below produce a VISIBLE artifact
// — an eviction sentinel block, and a per-block dropped-byte figure — never a
// silent drop.

// ── the input vocabulary ────────────────────────────────────────────────────
//
// These mirror `loomux_engine::harness::HarnessEvent` as it serializes:
// internally tagged on `kind`, `rename_all = "snake_case"`, newtype ids
// transparent (`TurnId(pub u64)` -> a number, `ToolUseId(pub String)` -> a
// string). They are a re-declaration of a wire shape, not a second definition
// of it: the Rust enum is the contract and this file follows it.

export type TurnId = number;
export type ToolUseId = string;
export type RequestId = string;

/** `harness::Tokens` — four buckets; thinking folds into `output`. */
export interface Tokens {
  input: number;
  output: number;
  cache_read: number;
  cache_creation: number;
}

/** `harness::Usage`. The two token fields answer different questions and are
 *  **never added together**, which is why the reducer only ever REPLACES. */
export interface Usage {
  call_cumulative: Tokens;
  this_turn_main_loop: Tokens | null;
  per_model: Array<{ model: string; tokens: Tokens }>;
}

export interface Cost {
  usd: number;
  basis: string;
}

/** `harness::StopReason`: unit variants are strings, `Other(String)` is
 *  `{"other": "..."}`. */
export type StopReason = string | { other: string };

export type DecisionSource = "policy" | "human" | "pane_exited";
export type UiMethod = "select" | "confirm" | "input" | "editor";
/** `harness::UiAnswer`, which carries `#[serde(rename_all = "snake_case")]` — so
 *  the wire keys are LOWER-CASE and this declaration follows it, as every type in
 *  this block follows the Rust enum rather than defining a second contract.
 *
 *  It said `Value`/`Confirmed`/`Cancelled` until #2891 S4, which is a spelling
 *  nothing has ever emitted: `serde`'s external tagging renames the VARIANT, and
 *  the enum's own doc says as much in prose ("answer with a `value`", "`confirm`
 *  with a `confirmed` boolean"). Every reader matched on the capitalised keys, so
 *  a real settlement would have fallen through to "cancelled" on every dialog —
 *  green on both sides, because the fixture carried the same error and the
 *  fixture reader is an unchecked `as` cast. Found by building §5.1's parity
 *  control, which is the first thing that read the two side by side.
 *
 *  No back-compat arm is owed. CLAUDE.md's rename rule keeps a READER accepting
 *  every spelling that was ever EMITTED, and the capitalised one never was. */
export type UiAnswer = { value: string } | { confirmed: boolean } | "cancelled";

/** `harness::NoteKind` — the closed set a decoder may produce, three kinds a
 *  renderer is meant to draw differently.
 *
 *  `retry` is transient and the harness is retrying itself; `error` is a
 *  failure nothing is retrying, retries EXHAUSTED included; `ui` is something
 *  the harness displayed and expects no answer (pi's fire-and-forget
 *  `notify`/`setStatus`/`setWidget`/`setTitle`). A `Lifecycle` fourth was
 *  considered and rejected as dead. */
export type NoteKind = "retry" | "error" | "ui";

/** The contract's seventeen variants (§1.2's eleven, S1a's five, and #2850's
 *  `Note`). Seven are decision-grade — `ToolCall`, `PermissionRequest`,
 *  `PermissionSettled`, `UiRequest`, `UiSettled`, `TurnEnded`, `Exited` — which
 *  is an audit-log split, not a projection one: this module draws all
 *  seventeen. */
export type HarnessEventLike =
  | { kind: "booted"; session: string | null; model: string | null; capabilities: string[] }
  | { kind: "turn_started"; turn: TurnId }
  | { kind: "text"; turn: TurnId; delta: string }
  | { kind: "thinking"; turn: TurnId; delta: string }
  | { kind: "tool_call"; turn: TurnId; id: ToolUseId; name: string; input: unknown }
  | {
      kind: "tool_output";
      turn: TurnId;
      id: ToolUseId;
      delta: string;
      is_error: boolean;
      /** `false` on every ordinary delta: APPEND. `true`: `delta` is the whole
       *  current output for this `ToolUseId` and SUPERSEDES everything held for
       *  it. See the `tool_output` arm for why this is carried rather than
       *  inferred. Absent is read as `false` — a consumer that ignores the
       *  field is exactly where it was before the field existed, which is what
       *  makes it additive. */
      replaces?: boolean;
    }
  | { kind: "tool_result"; turn: TurnId; id: ToolUseId; ok: boolean }
  | { kind: "permission_request"; id: RequestId; tool: string; input: unknown }
  | { kind: "permission_settled"; id: RequestId; decision: string; by: DecisionSource }
  | {
      kind: "ui_request";
      id: RequestId;
      method: UiMethod;
      title: string | null;
      message: string | null;
      options: string[];
      timeout_ms: number | null;
    }
  | { kind: "ui_settled"; id: RequestId; answer: UiAnswer; by: DecisionSource }
  | { kind: "queue_changed"; steering: string[]; follow_up: string[] }
  | { kind: "turn_ended"; turn: TurnId; usage: Usage | null; cost: Cost | null; stop: StopReason }
  | { kind: "compacted"; trigger: "manual" | "auto"; pre_tokens: number | null }
  | { kind: "exited"; code: number | null }
  | { kind: "observed"; observed: string; matched?: string }
  | {
      kind: "note";
      /** A real `Option`: a retry begins before a turn reopens and an extension
       *  can throw at boot. `null` is NOT bucketed into turn 0 — that would be
       *  a made-up fact in the field a renderer groups by (§1.3). */
      turn: TurnId | null;
      /** The inner field is `note`, not `kind`: the outer enum's serde tag is
       *  `kind`, so a variant field of that name is REFUSED BY THE DERIVE: `variant field name \`kind\` conflicts with
       *  internal tag`, so it never compiles (#2850 S1b, run 34046263686).
       *
       *  The reason to record that here is the OTHER fix. Silencing the derive
       *  with a `rename` keeps the field and ships the collision, and THAT
       *  version reaches this module as a duplicate `kind` key — where
       *  `JSON.parse` keeps the last one, so every note would arrive spelled
       *  `"retry"`, match no arm, and be filed as an unrecognised event with
       *  nothing red on either side. The compiler stops the shape; nothing
       *  would stop the rename. */
      note: NoteKind;
      text: string;
    };

/**
 * The one input that is **not** a `HarnessEvent`, and is deliberately not
 * spelled as one.
 *
 * A structured pane's transcript has to show something no harness reports:
 * what orrerix DELIVERED into the pane (`harness::Turn`'s four variants — the
 * one thing in the stream the agent did not produce, `DESIGN.md` §6). Giving
 * that a `HarnessEvent` variant would let a harness FORGE a delivery, which is
 * the same conflation §1.3 rule 2 refuses between scraped and reported facts.
 * It rides the same batch, tagged distinctly.
 *
 * The harness's own asides are NOT here: #2850's `Note` variant is a reported
 * fact and arrives above.
 */
export type LocalEvent = {
  kind: "delivery";
  /** Mirrors `harness::Turn`'s four variants. */
  via: "kickoff" | "prompt" | "notice" | "human";
  from: string | null;
  text: string;
  ts: string | null;
};

export type ProjectionInput = HarnessEventLike | LocalEvent;

// ── the output vocabulary: blocks ───────────────────────────────────────────

export type ToolStatus = "pending" | "running" | "ok" | "error";

interface BlockBase {
  /** Stable across re-projection: derived from a counter in `State`, so
   *  projecting the same event sequence from `emptyState()` twice yields the
   *  same ids. That is what lets `ViewState.collapsed` survive (see below). */
  id: string;
  turn: TurnId | null;
}

export interface TextBlock extends BlockBase {
  kind: "text";
  text: string;
  /** UTF-8 bytes of `text`, tracked incrementally rather than re-measured. */
  bytes: number;
  /** Bytes dropped from the HEAD of this block by the per-block ceiling.
   *  Rendered, never merely logged (`DESIGN.md` §7). */
  droppedBytes: number;
}

export interface ThinkingBlock extends BlockBase {
  kind: "thinking";
  text: string;
  bytes: number;
  droppedBytes: number;
}

export interface ToolBlock extends BlockBase {
  kind: "tool";
  toolUseId: ToolUseId;
  /** `null` when this card was created by a `tool_result`/`tool_output` whose
   *  `tool_call` was never seen — rule 1, not the string `"unknown"`. */
  name: string | null;
  input: unknown;
  status: ToolStatus;
  output: string;
  outputBytes: number;
  outputDroppedBytes: number;
  /** True once any output or result arrived carrying `is_error` / `!ok`. */
  isError: boolean;
  /** Wall-clock ms from the call to its result, or `null` when the caller
   *  supplied no clock (`project`'s `nowMs`) or the call has not returned. */
  durationMs: number | null;
  /** No `tool_call` was ever seen for this id. */
  orphan: boolean;
}

export interface DeliveryBlock extends BlockBase {
  kind: "delivery";
  via: "kickoff" | "prompt" | "notice" | "human";
  from: string | null;
  text: string;
  ts: string | null;
}

export interface RequestBlock extends BlockBase {
  kind: "request";
  /** A permission is a policy decision `permissions.json` may settle without
   *  anyone; a UI request is a harness asking a HUMAN and blocking on it. §1.2
   *  refuses to conflate them, so the card records which it is. */
  channel: "permission" | "ui";
  requestId: RequestId;
  /** Permission: the tool name. UI: `null`. */
  tool: string | null;
  method: UiMethod | null;
  title: string | null;
  message: string | null;
  options: string[];
  timeoutMs: number | null;
  input: unknown;
  /** `null` while pending — the pane is waiting on a human. */
  settled: { answer: UiAnswer | string; by: DecisionSource } | null;
}

export interface TurnBlock extends BlockBase {
  kind: "turn";
  turn: TurnId;
  ended: boolean;
  usage: Usage | null;
  cost: Cost | null;
  stop: StopReason | null;
  durationMs: number | null;
}

export interface NoticeBlock extends BlockBase {
  kind: "notice";
  level: "info" | "warn" | "error";
  tag: string;
  text: string;
  /** The harness's own `NoteKind` when this notice came from a `Note` event,
   *  and `null` when orrerix generated it (a compaction marker, an exit, a
   *  PTY `Observed(..)`, an unrecognised kind).
   *
   *  Carried BESIDE `level` rather than collapsed into it, because the three
   *  kinds are each meant to be drawn differently and `level` cannot separate a
   *  harness `ui` note from orrerix's own compaction row — both are
   *  informational and they are not the same thing. */
  noteKind: NoteKind | null;
}

/** The visible artifact of the `MAX_BLOCKS` ceiling. Always the first block
 *  when it exists, and never itself evicted. */
export interface EvictionBlock extends BlockBase {
  kind: "evicted";
  blocks: number;
}

export type Block =
  | TextBlock
  | ThinkingBlock
  | ToolBlock
  | DeliveryBlock
  | RequestBlock
  | TurnBlock
  | NoticeBlock
  | EvictionBlock;

// ── ceilings ────────────────────────────────────────────────────────────────

/** Blocks kept in the projection. Older ones roll into the eviction sentinel;
 *  the full transcript is on disk in the per-pane event log (§4.1). */
export const MAX_BLOCKS = 2000;

/** UTF-8 bytes of text kept per block — assistant text, thinking, and each
 *  tool card's output separately. The HEAD is dropped, because the live end is
 *  the part being read. */
export const MAX_TEXT_BYTES_PER_BLOCK = 256 * 1024;

// ── state ───────────────────────────────────────────────────────────────────

/** Everything about the SESSION, rebuilt by replaying the log. */
export interface State {
  blocks: Block[];
  /** Monotonic block-id counter — the source of id stability. */
  seq: number;
  /** Join keys for blocks a later event appends to. */
  byTool: Map<ToolUseId, string>;
  byRequest: Map<RequestId, string>;
  byTurn: Map<TurnId, string>;
  /** The text/thinking block currently being appended to, or `null`. Anything
   *  that is not another delta of the same kind clears them. */
  openText: string | null;
  openThinking: string | null;
  /** Boot facts for the header. Unknown is `null`, never `"unknown"`. */
  session: string | null;
  model: string | null;
  capabilities: string[];
  /** The LATEST usage/cost reported, never a sum: `call_cumulative` is already
   *  cumulative, so adding two reports would multiply a pane's spend by
   *  roughly its turn count. */
  usage: Usage | null;
  cost: Cost | null;
  /** The harness's own downstream queue (`QueueChanged`). Empty is not news. */
  steering: string[];
  followUp: string[];
  currentTurn: TurnId | null;
  exitCode: number | null;
  exited: boolean;
  /** Blocks rolled out by the `MAX_BLOCKS` ceiling. The eviction sentinel's
   *  positive control: a test that claims eviction fired asserts this > 0. */
  evicted: number;
  /** Total bytes dropped from block heads by the per-block ceiling. */
  droppedBytes: number;
  /** Input events whose `kind` this build does not know (rule 3). */
  unknownEvents: number;
  /** Wall-clock ms at which an open tool call / turn started, for durations.
   *  Only ever written when the caller supplied a clock. */
  toolStartedAt: Map<ToolUseId, number>;
  turnStartedAt: Map<TurnId, number>;
}

/** State the VIEW owns, which no re-projection may clobber.
 *
 *  The in-list-editor rule (`CLAUDE.md`): un-submitted / view-only state lives
 *  in the view, never on its DOM elements, because the renderer rebuilds its
 *  elements from the model on every batch. `collapsed` is keyed by block id,
 *  which is why ids are a counter over the event sequence rather than an array
 *  index — an index shifts under an eviction and would silently move a human's
 *  fold onto a different card. */
export interface ViewState {
  collapsed: Set<string>;
  dimThinking: boolean;
}

export function emptyState(): State {
  return {
    blocks: [],
    seq: 0,
    byTool: new Map(),
    byRequest: new Map(),
    byTurn: new Map(),
    openText: null,
    openThinking: null,
    session: null,
    model: null,
    capabilities: [],
    usage: null,
    cost: null,
    steering: [],
    followUp: [],
    currentTurn: null,
    exitCode: null,
    exited: false,
    evicted: 0,
    droppedBytes: 0,
    unknownEvents: 0,
    toolStartedAt: new Map(),
    turnStartedAt: new Map(),
  };
}

export function emptyViewState(): ViewState {
  return { collapsed: new Set(), dimThinking: false };
}

export interface ProjectOptions {
  /** Wall clock for this batch, in ms. Supplied by the caller rather than read
   *  from `Date.now()` so this module stays pure and its durations testable.
   *  Absent -> durations stay `null` rather than being invented. */
  nowMs?: number;
}

// ── the reducer ─────────────────────────────────────────────────────────────

/**
 * Fold one batch of events into the projection.
 *
 * **Mutates and returns `state`.** A batch arrives at most once per pane per
 * 16 ms carrying up to 64 events (§5.6), and a pane holds thousands of blocks;
 * copying the block array per batch would make the consumer pay per event,
 * which is the constraint `DESIGN.md` §7's storm fixture established one layer
 * up. Callers that want a snapshot re-project from `emptyState()` — which is
 * exactly what the id-stability property above makes safe.
 */
export function project(
  state: State,
  batch: readonly ProjectionInput[],
  opts?: ProjectOptions,
): State {
  const now = opts && typeof opts.nowMs === "number" ? opts.nowMs : null;
  for (const ev of batch) reduce(state, ev, now);
  evictBlocks(state);
  return state;
}

function reduce(state: State, ev: ProjectionInput, now: number | null): void {
  switch (ev.kind) {
    case "booted":
      state.session = ev.session ?? null;
      state.model = ev.model ?? null;
      state.capabilities = Array.isArray(ev.capabilities) ? ev.capabilities.slice() : [];
      return;

    case "turn_started": {
      state.currentTurn = ev.turn;
      const b: TurnBlock = {
        id: nextId(state),
        kind: "turn",
        turn: ev.turn,
        ended: false,
        usage: null,
        cost: null,
        stop: null,
        durationMs: null,
      };
      state.byTurn.set(ev.turn, b.id);
      pushBlock(state, b);
      if (now !== null) state.turnStartedAt.set(ev.turn, now);
      return;
    }

    case "turn_ended": {
      const b = findTurn(state, ev.turn);
      if (b) {
        b.ended = true;
        b.usage = ev.usage ?? null;
        b.cost = ev.cost ?? null;
        b.stop = ev.stop ?? null;
        const started = state.turnStartedAt.get(ev.turn);
        if (now !== null && typeof started === "number") b.durationMs = now - started;
      }
      // Latest, never a sum. A turn that reported no usage (the bounded
      // "missing stats" case) leaves the ticker showing the last figure it
      // really had rather than zeroing it.
      if (ev.usage) state.usage = ev.usage;
      if (ev.cost) state.cost = ev.cost;
      closeRuns(state);
      state.currentTurn = null;
      return;
    }

    case "text": {
      const b = openRun(state, "text", ev.turn);
      appendText(state, b, ev.delta);
      return;
    }

    case "thinking": {
      // Rule 2: its own block, always. Never `openRun(state, "text", ...)`.
      const b = openRun(state, "thinking", ev.turn);
      appendText(state, b, ev.delta);
      return;
    }

    case "tool_call": {
      const b = toolBlock(state, ev.id, ev.turn);
      b.name = ev.name;
      b.input = ev.input;
      b.orphan = false;
      if (now !== null && !state.toolStartedAt.has(ev.id)) state.toolStartedAt.set(ev.id, now);
      // The run is closed by `toolBlock` when it CREATES the card, which covers
      // this arm and the orphan paths together. Closing again here would be
      // dead weight that reads as the real guard.
      return;
    }

    case "tool_output": {
      const b = toolBlock(state, ev.id, ev.turn);
      if (b.status === "pending") b.status = "running";
      if (ev.is_error) b.isError = true;
      // §1.2: `ToolOutput.delta` IS A DELTA. The contract picked the delta over
      // pi's accumulated `partialResult` "because the conversion only goes one
      // way cheaply" — the ADAPTER holds the previous value and subtracts, a
      // consumer does not. So this APPENDS; it never replaces. If it ever
      // starts replacing UNASKED, pi's suffix subtraction has moved here and
      // every other harness pays for it.
      //
      // `replaces` is the one case where it does replace, and it is READ, never
      // inferred. pi's subtraction has a precondition — each accumulation
      // extends the last — and where that fails the adapter emits the whole
      // value. A consumer cannot tell that from a legitimate delta that happens
      // to repeat earlier bytes, and any heuristic for it ("does this restate
      // what I hold?") silently eats genuinely repeating output, which is worse
      // than the duplication it fixes. So the adapter marks it (#2850 S1b), by
      // the same argument that put the subtraction in the adapter to begin
      // with.
      if (ev.replaces === true) supersedeToolOutput(b);
      appendToolOutput(state, b, ev.delta);
      return;
    }

    case "tool_result": {
      const b = toolBlock(state, ev.id, ev.turn);
      if (!ev.ok) b.isError = true;
      b.status = ev.ok ? "ok" : "error";
      const started = state.toolStartedAt.get(ev.id);
      if (now !== null && typeof started === "number") b.durationMs = now - started;
      state.toolStartedAt.delete(ev.id);
      return;
    }

    case "permission_request": {
      const b = requestBlock(state, ev.id, "permission");
      b.tool = ev.tool;
      b.input = ev.input;
      return;
    }

    case "permission_settled": {
      const b = requestBlock(state, ev.id, "permission");
      b.settled = { answer: ev.decision, by: ev.by };
      return;
    }

    case "ui_request": {
      const b = requestBlock(state, ev.id, "ui");
      b.method = ev.method;
      b.title = ev.title ?? null;
      b.message = ev.message ?? null;
      b.options = Array.isArray(ev.options) ? ev.options.slice() : [];
      // Descriptive, not a control: it reports a deadline the HARNESS keeps,
      // and orrerix starts no timer of its own against it (§1.2).
      b.timeoutMs = ev.timeout_ms ?? null;
      return;
    }

    case "ui_settled": {
      const b = requestBlock(state, ev.id, "ui");
      b.settled = { answer: ev.answer, by: ev.by };
      return;
    }

    case "queue_changed":
      state.steering = Array.isArray(ev.steering) ? ev.steering.slice() : [];
      state.followUp = Array.isArray(ev.follow_up) ? ev.follow_up.slice() : [];
      return;

    case "compacted": {
      const pre = ev.pre_tokens == null ? "" : ` · ${ev.pre_tokens} tokens before`;
      pushNotice(state, "info", "compaction", `context compacted (${ev.trigger})${pre}`);
      return;
    }

    case "exited":
      state.exited = true;
      state.exitCode = ev.code ?? null;
      pushNotice(
        state,
        ev.code ? "error" : "info",
        "exit",
        ev.code == null ? "pane exited" : `pane exited (${ev.code})`,
      );
      return;

    case "observed":
      // A PTY pane's inferred evidence. A structured pane never emits one, and
      // nothing here promotes a heuristic to a reported fact (§1.3 rule 2): it
      // is recorded and drawn as a note, never as a request or a turn.
      pushNotice(
        state,
        "info",
        "observed",
        ev.matched ? `${ev.observed}: ${ev.matched}` : String(ev.observed),
      );
      return;

    case "delivery": {
      pushBlock(state, {
        id: nextId(state),
        kind: "delivery",
        turn: state.currentTurn,
        via: ev.via,
        from: ev.from ?? null,
        text: ev.text,
        ts: ev.ts ?? null,
      });
      return;
    }

    case "note": {
      // `turn` is taken from the EVENT, never from `state.currentTurn`: a note
      // that says it belongs to no turn belongs to no turn, and substituting
      // the open one would invent the attribution §1.3 refuses.
      const level = ev.note === "error" ? "error" : ev.note === "retry" ? "warn" : "info";
      pushBlock(state, {
        id: nextId(state),
        kind: "notice",
        turn: ev.turn ?? null,
        level,
        tag: ev.note,
        text: ev.text,
        noteKind: ev.note,
      });
      return;
    }

    default: {
      // Rule 3: additive enum, unknown kind recorded rather than thrown on.
      const raw = ev as { kind?: unknown };
      state.unknownEvents += 1;
      pushNotice(state, "info", "unknown", String(raw && raw.kind));
      return;
    }
  }
}

// ── block helpers ───────────────────────────────────────────────────────────

function nextId(state: State): string {
  state.seq += 1;
  return `b${state.seq}`;
}

function push(state: State, b: Block): void {
  state.blocks.push(b);
}

/**
 * Append a block that is NOT a streaming run, ending whatever run was open.
 *
 * This exists because the rule "anything that is not another delta of the same
 * kind interrupts the run" is a property of EVERY such block, and stating it
 * once per factory is how it gets missed. It was: round 1 of review found
 * `tool_call` closing the run and the orphan paths not; the fix moved the close
 * into `toolBlock`'s creation path on the argument that it was "the ONE place
 * that can say so for every path" — which was true of tool cards and false of
 * the module, because `requestBlock` is the same factory for a different card
 * and still did not close. Round 3 found that twin: `permission_settled` /
 * `ui_settled` create a card on a path with no close, reachable both by a
 * reconnect and — with no reconnect at all — by eviction dropping the
 * `byRequest` key so a late settle re-creates the card.
 *
 * So the guard lives at the one place every non-run block is appended, and a
 * sixth block kind added later inherits it instead of re-deriving it. Runs
 * themselves (`openRun`) do not come through here: they manage the pointer they
 * are setting.
 */
function pushBlock(state: State, b: Block): void {
  closeRuns(state);
  push(state, b);
}

function pushNotice(
  state: State,
  level: "info" | "warn" | "error",
  tag: string,
  text: string,
): void {
  pushBlock(state, {
    id: nextId(state),
    kind: "notice",
    turn: state.currentTurn,
    level,
    tag,
    text,
    // orrerix's own row, not the harness's: `noteKind` is what separates them.
    noteKind: null,
  });
}

/** End the open text/thinking runs. Anything that is not another delta of the
 *  same kind interrupts them, so the next delta starts a new block instead of
 *  reaching back over a card that has since been drawn. */
function closeRuns(state: State): void {
  state.openText = null;
  state.openThinking = null;
}

function openRun(
  state: State,
  kind: "text" | "thinking",
  turn: TurnId,
): TextBlock | ThinkingBlock {
  const openId = kind === "text" ? state.openText : state.openThinking;
  if (openId !== null) {
    const found = state.blocks.find((b) => b.id === openId);
    if (found && found.kind === kind) return found;
    // Evicted out from under us — start a fresh run rather than resurrecting.
  }
  // The other run ends: text and thinking interleave, and a renderer that
  // quiets thinking must not be left with a text block straddling it.
  closeRuns(state);
  const id = nextId(state);
  const b: TextBlock | ThinkingBlock =
    kind === "text"
      ? { id, kind: "text", turn, text: "", bytes: 0, droppedBytes: 0 }
      : { id, kind: "thinking", turn, text: "", bytes: 0, droppedBytes: 0 };
  if (kind === "text") state.openText = id;
  else state.openThinking = id;
  push(state, b);
  return b;
}

function toolBlock(state: State, id: ToolUseId, turn: TurnId): ToolBlock {
  const openId = state.byTool.get(id);
  if (openId !== undefined) {
    const found = state.blocks.find((b) => b.id === openId);
    if (found && found.kind === "tool") return found;
    state.byTool.delete(id);
  }
  // Reached by a `tool_output` / `tool_result` whose `tool_call` was never seen
  // — a batch that started mid-session, or a harness that reordered them.
  // `orphan` records it and `name` stays `null` (rule 1); nothing throws.
  //
  // CREATING a card ends the open text/thinking run, and this is the ONE place
  // that can say so for every path — the `tool_call` arm is not the only one
  // that makes a card. An orphan created from `tool_output` used to leave the
  // run open, so text arriving after the card appended to the paragraph ABOVE
  // it, which is exactly the invariant the design note claims. That is the
  // reconnect case rather than a corner: a client attaching mid-session replays
  // a rotated log and rejoins mid-tool, so `tool_output` without its
  // `tool_call` is the NORMAL first event for that card.
  const b: ToolBlock = {
    id: nextId(state),
    kind: "tool",
    turn,
    toolUseId: id,
    name: null,
    input: null,
    status: "pending",
    output: "",
    outputBytes: 0,
    outputDroppedBytes: 0,
    isError: false,
    durationMs: null,
    orphan: true,
  };
  state.byTool.set(id, b.id);
  pushBlock(state, b);
  return b;
}

function requestBlock(
  state: State,
  id: RequestId,
  channel: "permission" | "ui",
): RequestBlock {
  const openId = state.byRequest.get(id);
  if (openId !== undefined) {
    const found = state.blocks.find((b) => b.id === openId);
    if (found && found.kind === "request") return found;
    state.byRequest.delete(id);
  }
  const b: RequestBlock = {
    id: nextId(state),
    kind: "request",
    turn: state.currentTurn,
    channel,
    requestId: id,
    tool: null,
    method: null,
    title: null,
    message: null,
    options: [],
    timeoutMs: null,
    input: null,
    settled: null,
  };
  state.byRequest.set(id, b.id);
  pushBlock(state, b);
  return b;
}

function findTurn(state: State, turn: TurnId): TurnBlock | null {
  const id = state.byTurn.get(turn);
  if (id === undefined) return null;
  const found = state.blocks.find((b) => b.id === id);
  return found && found.kind === "turn" ? found : null;
}

// ── the two ceilings ────────────────────────────────────────────────────────

/** UTF-8 byte length. Hand-rolled rather than `TextEncoder`, which allocates a
 *  byte array per call and is measured per delta on a streaming path. */
export function utf8Bytes(s: string): number {
  let n = 0;
  for (let i = 0; i < s.length; i += 1) {
    const c = s.charCodeAt(i);
    if (c < 0x80) n += 1;
    else if (c < 0x800) n += 2;
    else if (c >= 0xd800 && c <= 0xdbff && i + 1 < s.length) {
      const d = s.charCodeAt(i + 1);
      if (d >= 0xdc00 && d <= 0xdfff) {
        n += 4;
        i += 1;
        continue;
      }
      n += 3;
    } else n += 3;
  }
  return n;
}

function appendText(state: State, b: TextBlock | ThinkingBlock, delta: string): void {
  b.text += delta;
  b.bytes += utf8Bytes(delta);
  if (b.bytes <= MAX_TEXT_BYTES_PER_BLOCK) return;
  const cut = trimHead(b.text, b.bytes);
  b.text = cut.text;
  b.droppedBytes += cut.dropped;
  b.bytes = cut.bytes;
  state.droppedBytes += cut.dropped;
}

/** Drop everything held for a card, because a `replaces` output supersedes it.
 *
 *  `outputDroppedBytes` resets: it describes what was trimmed from the value
 *  the card is SHOWING, and that value is gone. `State.droppedBytes` does NOT
 *  reset — it is a session-lifetime figure ("how much has this pane elided"),
 *  and rewinding it would make a monotonic counter go backwards. The asymmetry
 *  is deliberate; the two answer different questions. */
function supersedeToolOutput(b: ToolBlock): void {
  b.output = "";
  b.outputBytes = 0;
  b.outputDroppedBytes = 0;
}

function appendToolOutput(state: State, b: ToolBlock, delta: string): void {
  b.output += delta;
  b.outputBytes += utf8Bytes(delta);
  if (b.outputBytes <= MAX_TEXT_BYTES_PER_BLOCK) return;
  const cut = trimHead(b.output, b.outputBytes);
  b.output = cut.text;
  b.outputDroppedBytes += cut.dropped;
  b.outputBytes = cut.bytes;
  state.droppedBytes += cut.dropped;
}

/** Drop from the HEAD until the tail fits, never splitting a surrogate pair —
 *  a cut between the halves of one leaves a lone surrogate, which is not a
 *  character and renders as a replacement glyph. */
function trimHead(
  text: string,
  bytes: number,
): { text: string; bytes: number; dropped: number } {
  // The loop already knows every skipped character's width, so it ACCUMULATES
  // them rather than re-measuring the tail afterwards. An earlier version ended
  // `utf8Bytes(kept)`, which walks up to `MAX_TEXT_BYTES_PER_BLOCK` characters
  // on EVERY trim — and once a block is saturated every subsequent delta trims,
  // so the cost is O(total x block / delta): measured 20.4 s for 100 MB at
  // 4 KiB deltas against 1.6 s at 64 KiB, about 51 ms per 64-event batch at
  // saturation, against the 16 ms/batch budget `project` cites. That is the
  // module's own "a producer may not make the consumer pay per event" rule
  // being broken by this function. The widths below are the same rules
  // `utf8Bytes` applies, lone surrogates included, so the arithmetic is
  // identical — `the head trim agrees with utf8Bytes on the kept tail` pins
  // that rather than leaving it asserted.
  let over = bytes - MAX_TEXT_BYTES_PER_BLOCK;
  let dropped = 0;
  let i = 0;
  while (over > 0 && i < text.length) {
    const c = text.charCodeAt(i);
    if (c >= 0xd800 && c <= 0xdbff && i + 1 < text.length) {
      const d = text.charCodeAt(i + 1);
      if (d >= 0xdc00 && d <= 0xdfff) {
        over -= 4;
        dropped += 4;
        i += 2;
        continue;
      }
    }
    const w = c < 0x80 ? 1 : c < 0x800 ? 2 : 3;
    over -= w;
    dropped += w;
    i += 1;
  }
  return { text: text.slice(i), bytes: bytes - dropped, dropped };
}

/** Roll the oldest blocks into the sentinel. The sentinel is a BLOCK, so the
 *  elision is on screen next to what survived it (`DESIGN.md` §7); it is
 *  always index 0 and is never itself a candidate for eviction. */
function evictBlocks(state: State): void {
  if (state.blocks.length <= MAX_BLOCKS) return;
  const first = state.blocks[0];
  const hasSentinel = first !== undefined && first.kind === "evicted";
  const sentinel: EvictionBlock = hasSentinel
    ? (first as EvictionBlock)
    : { id: nextId(state), kind: "evicted", turn: null, blocks: 0 };
  const body = hasSentinel ? state.blocks.slice(1) : state.blocks;
  // The sentinel occupies one of the `MAX_BLOCKS` slots, so the elision is
  // visible without pushing the pane over its own ceiling.
  const drop = body.length + 1 - MAX_BLOCKS;
  if (drop <= 0) return;
  for (const b of body.slice(0, drop)) forgetBlock(state, b);
  sentinel.blocks += drop;
  state.evicted += drop;
  state.blocks = [sentinel, ...body.slice(drop)];
}

/** Drop an evicted block's join keys, so a later event for it opens a fresh
 *  card rather than appending to a block nobody can see. */
function forgetBlock(state: State, b: Block): void {
  if (b.kind === "tool") {
    state.byTool.delete(b.toolUseId);
    state.toolStartedAt.delete(b.toolUseId);
  } else if (b.kind === "request") {
    state.byRequest.delete(b.requestId);
  } else if (b.kind === "turn") {
    state.byTurn.delete(b.turn);
    state.turnStartedAt.delete(b.turn);
  }
  if (state.openText === b.id) state.openText = null;
  if (state.openThinking === b.id) state.openThinking = null;
}

// ── read models ─────────────────────────────────────────────────────────────

/**
 * The last `n` characters of the projection as plain text, for a pane
 * thumbnail.
 *
 * Thinking is included only when the view is not dimming it, so a thumbnail
 * shows what the human chose to look at rather than what the model muttered.
 * A tool card contributes its name and status, not its output: a thumbnail of
 * a 4 000-line grep result says nothing about what the agent is doing.
 */
export function textTail(state: State, n: number, view?: ViewState): string {
  if (n <= 0) return "";
  const dim = view ? view.dimThinking : false;
  const lines: string[] = [];
  let size = 0;
  for (let i = state.blocks.length - 1; i >= 0 && size < n; i -= 1) {
    const line = blockLine(state.blocks[i]!, dim);
    if (line === null) continue;
    lines.push(line);
    size += line.length + 1;
  }
  lines.reverse();
  const joined = lines.join("\n");
  return joined.length <= n ? joined : joined.slice(joined.length - n);
}

function blockLine(b: Block, dimThinking: boolean): string | null {
  switch (b.kind) {
    case "text":
      return b.text;
    case "thinking":
      return dimThinking ? null : b.text;
    case "tool":
      return `${b.name ?? "?"} · ${b.status}`;
    case "delivery":
      return `[${b.via}] ${b.text}`;
    case "request":
      return `[${b.channel}] ${b.title ?? b.tool ?? b.requestId}`;
    case "turn":
      return b.ended ? `— turn ${b.turn} ended —` : `— turn ${b.turn} —`;
    case "notice":
      return `[${b.tag}] ${b.text}`;
    case "evicted":
      return `[ring] ${b.blocks} earlier blocks rolled out of the pane; the full transcript is in the event log`;
    default:
      return null;
  }
}

/** Whether a block is folded, for a renderer. Kept here so `ViewState` has one
 *  reader and a renderer never reads a fold off a DOM element. */
export function isCollapsed(view: ViewState, block: Block): boolean {
  return view.collapsed.has(block.id);
}

/** Toggle a fold. Returns the same `ViewState` — it is the view's, and the
 *  reducer never touches it, which is the whole point of the split. */
export function toggleCollapsed(view: ViewState, blockId: string): ViewState {
  if (view.collapsed.has(blockId)) view.collapsed.delete(blockId);
  else view.collapsed.add(blockId);
  return view;
}

/** Drop folds for blocks that no longer exist, so the set cannot grow without
 *  bound across a long session (the `selected` / `collapsed` pruning idiom). */
export function pruneViewState(view: ViewState, state: State): ViewState {
  if (view.collapsed.size === 0) return view;
  const live = new Set(state.blocks.map((b) => b.id));
  for (const id of Array.from(view.collapsed)) if (!live.has(id)) view.collapsed.delete(id);
  return view;
}

// ── the decoder: the cast, replaced (#2891 S4) ──────────────────────────────
//
// WHY THIS EXISTS, and what it closes. The types above are a re-declaration of
// a wire shape defined in Rust. Nothing checked that they matched it: every
// reader — the fixture readers, the replay page, and the `orch-pane-event`
// listener — did `JSON.parse(line) as ProjectionInput`, which is an assertion
// the compiler is REQUIRED to believe. Three spellings were wrong when this was
// written and the suite was green over all three (#2891 S4):
//
//   - `UiAnswer` was `{ Value } | { Confirmed } | "Cancelled"`, where the Rust
//     enum carries `rename_all = "snake_case"` and the wire is
//     `{ value } | { confirmed } | "cancelled"`. Every reader matched the
//     capitalised keys, so a real settlement would have fallen through to
//     "cancelled" on every dialog;
//   - the fixture carried the same capitalised answer, which is why nothing
//     reddened;
//   - and a `compacted` trigger of `"threshold"`, which is not a
//     `CompactTrigger` — and did not satisfy the type it was cast to either.
//
// The engine side of the fix is a round-trip test in
// `crates/loomux-engine/src/harness/transcript.rs`, which reddens if a serde
// attribute in that crate stops agreeing with the fixture the frontend runs on.
// This is the other side: a spelling that reaches THIS build at runtime is
// refused rather than believed.
//
// TWO KINDS OF UNKNOWN, and they are not the same thing.
//
//   - **An unknown `kind` is not an error.** `HarnessEvent` is an additive enum
//     and §1.2 is explicit: "a consumer that does not match them keeps
//     compiling and keeps working, minus what it does not read". So a kind this
//     build has never heard of passes through and `project()` files it as a
//     notice carrying the raw kind — rule 3, unchanged.
//   - **A known kind with a payload this contract does not allow IS an error.**
//     `{"kind":"compacted","trigger":"threshold"}` is not a newer protocol, it
//     is a value nothing may emit; believing it means rendering a fact nobody
//     reported. That is refused.
//
// The check is deliberately SHALLOW: it validates the closed vocabularies —
// the enums whose spellings serde decides, which is exactly where the three
// defects were — and the shape of the fields those enums live in. It does not
// re-validate every string and number, because a `string` that arrives as a
// string is not where a rename can hurt you. What it is scoped to is stated
// here rather than implied, and `test/structuredrows.test.ts` runs it over the
// whole fixture with a positive control that the refusal really fires.

const NOTE_KINDS: readonly string[] = ["retry", "error", "ui"];
const UI_METHODS: readonly string[] = ["select", "confirm", "input", "editor"];
const DECISION_SOURCES: readonly string[] = ["policy", "human", "pane_exited"];
const COMPACT_TRIGGERS: readonly string[] = ["manual", "auto"];
const DELIVERY_VIA: readonly string[] = ["kickoff", "prompt", "notice", "human"];

/** Thrown by `decodeProjectionInput` when a KNOWN kind carries a payload the
 *  contract does not allow. Carries the offending value so a reviewer reading a
 *  CI log can see the spelling rather than only the field name. */
export class ProjectionDecodeError extends Error {
  /** Fields are declared and assigned, NOT constructor parameter properties:
   *  `node --test` loads `src/*.ts` off disk in strip-only mode, which refuses
   *  a parameter property outright (`ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX`).
   *  `tsc --noEmit` is perfectly happy with them, so the compiler is not the
   *  instrument that catches this — the suite is. */
  readonly kind: string;
  readonly field: string;
  readonly value: unknown;

  constructor(kind: string, field: string, value: unknown) {
    super(
      `${kind}.${field} is ${JSON.stringify(value)}, which this contract does not allow — ` +
        "the wire shape is crates/loomux-engine/src/harness/mod.rs",
    );
    this.kind = kind;
    this.field = field;
    this.value = value;
    this.name = "ProjectionDecodeError";
  }
}

function requireOneOf(kind: string, field: string, value: unknown, allowed: readonly string[]): void {
  if (typeof value !== "string" || !allowed.includes(value)) {
    throw new ProjectionDecodeError(kind, field, value);
  }
}

/**
 * Decode one event off the wire (or off a fixture line).
 *
 * Returns the value typed, or throws `ProjectionDecodeError`. An unrecognised
 * `kind` is returned unchanged for `project()`'s rule-3 handling — see the
 * header for why those two unknowns are not the same thing.
 */
export function decodeProjectionInput(raw: unknown): ProjectionInput {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
    throw new ProjectionDecodeError("?", "(event)", raw);
  }
  const o = raw as Record<string, unknown>;
  const kind = o.kind;
  if (typeof kind !== "string") throw new ProjectionDecodeError("?", "kind", kind);

  switch (kind) {
    case "note":
      requireOneOf(kind, "note", o.note, NOTE_KINDS);
      break;
    case "compacted":
      requireOneOf(kind, "trigger", o.trigger, COMPACT_TRIGGERS);
      break;
    case "ui_request":
      requireOneOf(kind, "method", o.method, UI_METHODS);
      break;
    case "ui_settled":
      requireDecisionSource(kind, o.by);
      requireUiAnswer(o.answer);
      break;
    case "permission_settled":
      requireDecisionSource(kind, o.by);
      break;
    case "delivery":
      requireOneOf(kind, "via", o.via, DELIVERY_VIA);
      break;
    default:
      // Every other kind's payload is strings, numbers and free-form JSON —
      // nothing whose spelling a serde attribute decides. An unknown kind lands
      // here too, and passes through by design.
      break;
  }
  return raw as ProjectionInput;
}

function requireDecisionSource(kind: string, by: unknown): void {
  requireOneOf(kind, "by", by, DECISION_SOURCES);
}

/** `UiAnswer` is the one payload that is itself an enum, and the one the three
 *  defects were in. Externally tagged with `rename_all = "snake_case"`: a
 *  one-key object `{value}` or `{confirmed}`, or the bare string `"cancelled"`.
 *  Anything else — the capitalised spellings included — is refused. */
function requireUiAnswer(answer: unknown): void {
  if (answer === "cancelled") return;
  if (answer && typeof answer === "object" && !Array.isArray(answer)) {
    const keys = Object.keys(answer);
    if (keys.length === 1 && keys[0] === "value") {
      if (typeof (answer as { value: unknown }).value === "string") return;
    }
    if (keys.length === 1 && keys[0] === "confirmed") {
      if (typeof (answer as { confirmed: unknown }).confirmed === "boolean") return;
    }
  }
  throw new ProjectionDecodeError("ui_settled", "answer", answer);
}

/**
 * Decode a whole batch, dropping what it refuses rather than throwing.
 *
 * The listener's form. One malformed event must not cost the batch it rode in
 * on — the other 63 are fine and the transcript is what the human is watching —
 * so a refusal is DROPPED and REPORTED, never swallowed and never fatal. The
 * `rejected` array is what a caller logs; returning it rather than logging here
 * keeps this module pure and testable.
 */
export function decodeBatch(raws: readonly unknown[]): {
  events: ProjectionInput[];
  rejected: ProjectionDecodeError[];
} {
  const events: ProjectionInput[] = [];
  const rejected: ProjectionDecodeError[] = [];
  for (const raw of raws) {
    try {
      events.push(decodeProjectionInput(raw));
    } catch (e) {
      rejected.push(e instanceof ProjectionDecodeError ? e : new ProjectionDecodeError("?", "(event)", raw));
    }
  }
  return { events, rejected };
}
