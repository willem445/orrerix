// pi RPC wire events -> HarnessEvent.
//
// The fixtures in ./fixtures are pi's OWN wire shapes, not a convenient
// invention, and this module is the mapping that turns them into the
// harness-neutral vocabulary the renderer consumes. Keeping the seam here is
// the point of the mock: the renderer never sees a pi field name, so what S4
// builds against is the contract, and S1b's real `pi.rs` replaces THIS file
// alone.
//
// SOURCE OF THE MAPPING: doc/design/harness-adapters.md §1.2 (the eleven
// merged variants) plus the five S1a adds (Thinking, ToolOutput, UiRequest,
// UiSettled, QueueChanged) and pi's RPC event list from the #2850 plan. Where
// this file and that note disagree, the note wins — it is the contract and
// this is a mock.
//
// TWO RULES CARRIED FROM THE NOTE, because they are the ones a renderer can
// silently break:
//
//  1. UNKNOWN IS NOT A VALUE. A fact the pane does not have is `null`, never a
//     sentinel string. `session: null` is "not known yet"; `session: "unknown"`
//     would be data every consumer can forget to check.
//  2. AN UNKNOWN MESSAGE TYPE IS IGNORED, NOT FATAL — but it is RECORDED. A
//     silently discarded message is a message nobody can add support for
//     later, so it becomes a `Note` carrying the raw line, truncated.

const RAW_CAP = 4096; // §4.1's 4 KiB truncation for an unrecognised line

/** Every HarnessEvent kind this mock renders: §1.2's eleven plus S1a's five. */
export const EVENT_KINDS = [
  "Booted", "TurnStarted", "Text", "Thinking", "ToolCall", "ToolOutput",
  "ToolResult", "PermissionRequest", "PermissionSettled", "UiRequest",
  "UiSettled", "QueueChanged", "TurnEnded", "Compacted", "Exited", "Note",
];

/**
 * Decode one pi RPC line into zero or more HarnessEvents.
 *
 * Zero is a real answer: `message_start` opens a block that carries nothing a
 * renderer can show until its first delta arrives, and inventing an empty
 * event for it would put a blank row in the transcript.
 */
export function decode(line, state) {
  const t = line.type;

  switch (t) {
    // --- boot. pi's RPC has NO ready event; the driver synthesizes Booted by
    //     sending get_state and reading the reply. The fixture models that: a
    //     `response` whose command is get_state IS the boot fact.
    case "response":
      if (line.command === "get_state" && line.success) {
        const d = line.data || {};
        return [{
          k: "Booted",
          session: d.sessionId ?? null,
          model: d.model ?? null,
          thinking: d.thinkingLevel ?? null,
          capabilities: d.capabilities || [],
        }];
      }
      if (line.command === "get_session_stats" && line.success) {
        // The stats reply is where a turn's usage and cost really come from.
        return [{ k: "TurnEnded", turn: state.turn, ...statsOf(line.data), stop: state.stop || "end_turn" }];
      }
      return [];

    case "turn_start":
      state.turn = line.turnId ?? (state.turn ?? 0) + 1;
      return [{ k: "TurnStarted", turn: state.turn }];

    case "turn_end":
      // pi ends a turn on the wire; the usage arrives with the stats reply, so
      // a turn_end with no stats attached is still a boundary worth drawing.
      state.stop = line.stopReason || "end_turn";
      return line.usage
        ? [{ k: "TurnEnded", turn: state.turn, ...usageOf(line.usage), stop: state.stop }]
        : [];

    // --- the assistant message stream. One envelope, three payload families,
    //     discriminated by assistantMessageEvent.type.
    case "message_start":
      return [];

    case "message_update": {
      const ev = line.assistantMessageEvent || {};
      const out = [];
      if (ev.type === "text_delta") {
        out.push({ k: "Text", turn: state.turn, delta: ev.delta ?? "" });
      } else if (ev.type === "thinking_delta") {
        out.push({ k: "Thinking", turn: state.turn, delta: ev.delta ?? "" });
      } else if (ev.type === "toolcall_end") {
        // toolcall_start / toolcall_delta are deliberately ignored: a tool call
        // is only actionable once its arguments are complete, and a card that
        // rendered from a half-parsed argument blob would show a path that is
        // not yet a path.
        out.push({
          k: "ToolCall",
          turn: state.turn,
          id: ev.toolCallId,
          name: ev.toolName,
          input: ev.arguments || {},
        });
      }
      // A message_update also carries running usage — the live ticker source.
      if (line.usage) out.push({ k: "UsageTick", ...usageOf(line.usage) });
      return out;
    }

    case "message_end":
      return [];

    // --- tool execution, which is a SEPARATE stream from the message stream:
    //     the model announces a call, the host runs it, and the output arrives
    //     on its own channel. That separation is why ToolOutput exists as its
    //     own event rather than riding on ToolResult.
    case "tool_execution_start":
      return [{ k: "ToolRunning", id: line.toolCallId }];

    case "tool_execution_update":
      return [{
        k: "ToolOutput",
        turn: state.turn,
        id: line.toolCallId,
        delta: textOf(line.partialResult),
        is_error: false,
      }];

    case "tool_execution_end": {
      const out = [];
      const tail = textOf(line.result);
      if (tail) out.push({ k: "ToolOutput", turn: state.turn, id: line.toolCallId, delta: tail, is_error: !!line.isError });
      out.push({
        k: "ToolResult",
        turn: state.turn,
        id: line.toolCallId,
        ok: !line.isError,
        ms: line.durationMs ?? null,
        meta: line.meta || null,
      });
      return out;
    }

    // --- the queue. steering[] jumps the current turn; followUp[] waits for
    //     the next one. Both are deliveries orrerix's own queue admitted, so
    //     the count is a supervisor fact, not a pi internal.
    case "queue_update":
      return [{
        k: "QueueChanged",
        steering: line.steering || [],
        follow_up: line.followUp || [],
      }];

    // --- extension UI. select|confirm|input|editor are decision-grade and
    //     become UiRequest; notify|setStatus|setWidget|setTitle|set_editor_text
    //     are presentational and become a Note, because a renderer that turned
    //     a status update into a card the human must dismiss would be inventing
    //     an interruption pi never asked for.
    case "extension_ui_request": {
      const decisionGrade = ["select", "confirm", "input", "editor"];
      if (!decisionGrade.includes(line.method)) {
        return [{ k: "Note", kind: "info", tag: line.method, text: shortJson(line.params) }];
      }
      return [{
        k: "UiRequest",
        id: line.id,
        method: line.method,
        title: line.title ?? null,
        message: line.message ?? null,
        options: line.options || [],
        timeout_ms: line.timeoutMs ?? null,
      }];
    }

    case "extension_ui_response":
      return [{
        k: "UiSettled",
        id: line.id,
        answer: line.cancelled ? { t: "Cancelled" }
              : "confirmed" in line ? { t: "Confirmed", v: line.confirmed }
              : { t: "Value", v: line.value },
        by: line.by || "human",
      }];

    // --- orrerix's own permission gate. Distinct from an extension UI request:
    //     a permission is decided by permissions.json's policy ladder and only
    //     reaches the human when no rule matched (§3.2).
    case "permission_request":
      return [{ k: "PermissionRequest", id: line.id, tool: line.tool, input: line.input || {} }];

    case "permission_settled":
      return [{ k: "PermissionSettled", id: line.id, decision: line.decision, by: line.by }];

    // --- compaction. The trigger matters: `manual` is something a human did,
    //     `threshold` is routine, `overflow` is the one that lost context.
    case "compaction_start":
      return [{ k: "Compacted", phase: "start", trigger: line.reason || "threshold", pre_tokens: line.preTokens ?? null }];

    case "compaction_end":
      return [{ k: "Compacted", phase: "end", trigger: line.reason || "threshold", pre_tokens: line.preTokens ?? null, post_tokens: line.postTokens ?? null }];

    // --- retries and extension faults. Neither is the agent's voice and
    //     neither is fatal, so both are notes rather than transcript content.
    case "auto_retry_start":
      return [{ k: "Note", kind: "warn", tag: "retry", text: `${line.reason || "request failed"} — retry ${line.attempt}/${line.maxAttempts} in ${line.delayMs}ms` }];

    case "auto_retry_end":
      return [{ k: "Note", kind: line.success ? "info" : "error", tag: "retry", text: line.success ? "recovered" : "gave up" }];

    case "extension_error":
      return [{ k: "Note", kind: "error", tag: line.extension || "extension", text: line.message || "extension failed" }];

    case "agent_settled":
      return [{ k: "Settled" }];

    // --- orrerix's own inbound: a delivery the queue admitted. Not a pi event
    //     at all — it is orrerix narrating its own action into the transcript,
    //     which is what Turn::{Kickoff,Prompt,Notice,Human} become on screen.
    case "orrerix_delivery":
      return [{ k: "Delivery", from: line.from, kind: line.deliveryKind, text: line.text, ts: line.ts }];

    case "orrerix_notice":
      return [{ k: "Note", kind: line.level || "info", tag: "orrerix", text: line.text }];

    case "exit":
      return [{ k: "Exited", code: line.code ?? null }];

    // Ignored, but never silently: §4.1's rule.
    default:
      return [{ k: "Note", kind: "info", tag: "unknown", text: shortJson(line) }];
  }
}

/**
 * pi's Usage. `reasoning` is documented as a SUBSET of `output` — "output
 * already includes these tokens" — so it is reported alongside and never
 * folded in. Adding it would double-count every thinking token in the bill.
 */
function usageOf(u) {
  const input = u.input ?? 0, output = u.output ?? 0;
  const cacheRead = u.cacheRead ?? 0, cacheWrite = u.cacheWrite ?? 0;
  return {
    usage: {
      input, output, cacheRead, cacheWrite,
      reasoning: u.reasoning ?? 0,
      total: u.totalTokens ?? input + output + cacheRead + cacheWrite,
    },
    cost: u.cost ?? null,
  };
}

function statsOf(d) {
  const t = (d && d.tokens) || {};
  return {
    usage: {
      input: t.input ?? 0, output: t.output ?? 0,
      cacheRead: t.cacheRead ?? 0, cacheWrite: t.cacheWrite ?? 0,
      reasoning: t.reasoning ?? 0,
      total: t.total ?? 0,
    },
    cost: d ? d.cost ?? null : null,
    context: d ? d.contextUsage ?? null : null,
  };
}

/** pi hands back content blocks; the renderer wants the text they carry. */
function textOf(result) {
  if (result == null) return "";
  if (typeof result === "string") return result;
  const content = result.content || (Array.isArray(result) ? result : null);
  if (!Array.isArray(content)) return typeof result.text === "string" ? result.text : "";
  return content.map((c) => (typeof c === "string" ? c : c && c.type === "text" ? c.text ?? "" : "")).join("");
}

function shortJson(v) {
  let s;
  try { s = JSON.stringify(v); } catch { s = String(v); }
  s = s ?? String(v);
  return s.length > RAW_CAP ? s.slice(0, RAW_CAP) + `… (+${s.length - RAW_CAP}B)` : s;
}
