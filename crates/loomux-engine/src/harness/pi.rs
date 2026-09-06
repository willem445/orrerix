//! The pi adapter: `--mode rpc` JSONL in and out, [`HarnessEvent`] out
//! (#2850 slice S1b, `doc/design/harness-adapters.md` §1 and
//! `doc/design/pi.md`'s "RPC driver (#2850)").
//!
//! # Every pi fact here is cited to the INSTALLED package, and none was observed
//!
//! No `pi` process was run to write this module (CLAUDE.md constraint 3), and
//! nothing below is recalled. Every claim is a read of the package installed on
//! this machine at the version the docs pin —
//! `@earendil-works/pi-coding-agent` **0.85.1** — and is cited as one of:
//!
//! - `docs/rpc.md:<line>` — that package's own RPC protocol reference;
//! - `dist/<path>:<line>` — that package's shipped JavaScript.
//!
//! Upstream `main` is deliberately **not** a source here: the driver has to
//! match the build the human actually runs, and a protocol read off a newer tree
//! would be a claim about a program nobody has.
//!
//! The consequence, stated rather than left to be inferred: this decoder is
//! proved against the **documented** protocol of that build, not against its
//! bytes. The fixtures under `crates/loomux-engine/tests/fixtures/harness/pi/`
//! are **synthesized from `docs/rpc.md`**, not recorded from a session, and say
//! so in their own README. Replacing them with a human-recorded capture is a
//! live-validation item (`doc/design/pi.md`, "Live items this section does not
//! settle", item 4).
//!
//! # Three things pi does that Claude Code does not, and what each buys
//!
//! 1. **RPC is bidirectional.** pi's stdin is a command channel, not just a
//!    prompt pipe, so [`AgentPane::interrupt`] is finally a real method: `abort`
//!    is a command (`docs/rpc.md:124`), not a signal this Windows baseline does
//!    not have. pi is the **first** harness that honours it —
//!    [`super::claude::ClaudePane`] still refuses, and
//!    [`the_first_harness_that_honours_interrupt_and_the_one_that_still_does_not`]
//!    pins both halves so "pi can interrupt" cannot quietly become "everything
//!    can".
//! 2. **There is no boot event.** RPC mode takes over stdout and writes only
//!    serialized JSON lines (`dist/modes/rpc/rpc-mode.js:24`, `:29`), and after
//!    `rebindSession()` (`:289`) it attaches the stdin reader (`:645-651`) and
//!    emits **nothing** — every `output(...)` in that file sits inside a
//!    handler, and the event table (`docs/rpc.md:859-885`) has no ready, boot or
//!    hello event. A driver that waited for one would wait forever, so
//!    [`HarnessEvent::Booted`] is **synthesized** from the reply to a
//!    [`BOOT_ID`]-tagged `get_state` the driver sends first. It is still a
//!    reported fact — pi answered it — and not a scrape.
//! 3. **Dialogs block the agent.** `select`/`confirm`/`input`/`editor` "emit an
//!    `extension_ui_request` on stdout and block until the client sends back an
//!    `extension_ui_response`" (`docs/rpc.md:1190`), which is why they are their
//!    own [`HarnessEvent::UiRequest`] and why §3.5 governs who may answer.
//!
//! # Framing: `BufRead::lines` is compliant, and that is a measured claim
//!
//! > "RPC mode uses strict JSONL semantics with LF (`\n`) as the only record
//! > delimiter." (`docs/rpc.md:30`)
//!
//! The docs require stripping an optional trailing `\r` (`:34`) and single out
//! Node's `readline` as **not** protocol-compliant "because it also splits on
//! `U+2028` and `U+2029`, which are valid inside JSON strings" (`:37`).
//!
//! Rust's [`BufRead::lines`] splits on `\n` alone, strips a trailing `\r`, and
//! does nothing whatever with `U+2028` — so it is compliant as shipped and this
//! module hand-rolls no framer. `one-turn.jsonl` carries a literal `U+2028`
//! inside a text delta for exactly this reason: it is the byte that separates a
//! compliant reader from `readline`, and
//! [`the_framing_is_lf_only_and_u2028_stays_inside_the_string`] would go red on
//! any reader that split it.
//!
//! # Why a `serde_json::Value` walk rather than derived structs
//!
//! Same reason as [`super::claude`]: the event union is open — twenty event
//! types (`docs/rpc.md:861-884`), nine `assistantMessageEvent` delta types
//! (`:963-973`), nine extension-UI methods (`:1188-1191`) — and a derived struct
//! per message would turn "a field this build has not heard of" into a parse
//! error, which is the opposite of the protocol note's §4.4 rule. What this
//! module must never do is discard the line as well: an unrecognized message is
//! kept verbatim in the pane log ([`super::LogBody::Unknown`]).

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;

use serde_json::Value;

use super::{
    AgentPane, CompactTrigger, Cost, CostBasis, Decision, EventLog, EventRx, Harness, HarnessEvent,
    LogBody, NoteKind, PaneKind, RequestId, SendReceipt, StopReason, Tokens, ToolUseId, Turn,
    TurnId, UiAnswer, UiMethod, Usage,
};
use crate::obs::LockExt;

// ── the launch line ─────────────────────────────────────────────────────────

/// Everything the argv builder needs.
///
/// These are the **same inputs the PTY arm already has** — that is the point of
/// the shape rather than a coincidence. `--mode` takes `text`, `json` or `rpc`
/// and nothing else (`dist/cli/args.js:40-42`), and no other flag is gated on
/// it: the parsed mode is written once and never read again in that file, so
/// every flag below reaches an RPC pane exactly as it reaches a PTY one. The
/// structured driver is therefore a **transport** change and nothing else —
/// containment, session identity and the MCP bridge are the same argv.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// `--session-id <id>`, omitted when absent.
    ///
    /// **One flag for both directions**, which is why there is no `SessionArg`
    /// here as there is on the Claude side: `--session-id` is documented "Use
    /// exact project session ID, creating it if missing"
    /// (`dist/cli/args.js:279`), so the same token opens an existing session and
    /// creates a missing one. A resume line and a fresh line are equal, and
    /// `pi_launch_flags_per_posture` in `src-tauri` already pins that they are.
    pub session_id: Option<String>,
    /// `--session-dir <dir>`: the group's own store.
    pub session_dir: PathBuf,
    /// `--mcp-config <file>`.
    ///
    /// pi's own parser has no such flag and files it into `unknownFlags`
    /// (`dist/cli/args.js:220-230`) for the adapter extension to read, which is
    /// the same seam the PTY arm uses and not a second one.
    pub mcp_config: PathBuf,
    /// `--append-system-prompt <path>` — the role contract, BY FILE.
    ///
    /// A `PathBuf` rather than the text, for the PTY arm's reason: the flag
    /// takes "text or file contents", and a role contract is many KB against
    /// Windows `CreateProcessW`'s 32,767-character command-line limit (#417).
    /// Only the path ever reaches argv.
    pub append_system_prompt: Option<PathBuf>,
    /// Which of `--approve` / `--no-approve` to emit. **Exactly one is always
    /// emitted** (never neither), so pi's one boot dialog cannot appear on a
    /// pane loomux is about to prompt; which one is the containment question,
    /// decided by the caller.
    pub approve: bool,
    /// `--exclude-tools <csv>`, omitted when absent. One comma-separated value,
    /// which is the form the flag documents (`dist/cli/args.js:290`).
    pub exclude_tools: Option<String>,
    /// `--model <pattern>`, omitted when empty — a blank `--model` would be an
    /// argument, not a silence.
    pub model: String,
    /// `--thinking <level>`, omitted when empty.
    pub thinking: String,
}

/// `--mode rpc`: the one flag that separates this launch line from the PTY one.
pub const MODE_FLAG: [&str; 2] = ["--mode", "rpc"];

/// The pair of trust flags, exactly one of which every line carries.
///
/// Spelled here rather than imported because this is an **engine leaf** — it
/// may not depend on `src-tauri`, where the PTY arm's constants live. That the
/// two spellings agree is not left to hope:
/// `pi_rpc_argv_is_the_pty_line_plus_one_flag` in
/// `src-tauri/tests/orchestration.rs` is S3b's to add, and until it exists this
/// is a stated residual rather than a covered one.
pub const APPROVE_FLAG: &str = "--approve";
/// See [`APPROVE_FLAG`].
pub const NO_APPROVE_FLAG: &str = "--no-approve";

impl LaunchSpec {
    /// The argv after the program name.
    ///
    /// The order is the PTY arm's order, so a byte-level comparison of the two
    /// lines is a diff of one inserted flag rather than a reordering nobody can
    /// read.
    pub fn argv(&self) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();
        a.push(MODE_FLAG[0].into());
        a.push(MODE_FLAG[1].into());
        if let Some(s) = &self.session_id {
            a.push("--session-id".into());
            a.push(s.clone());
        }
        a.push("--session-dir".into());
        a.push(self.session_dir.to_string_lossy().into_owned());
        a.push("--mcp-config".into());
        a.push(self.mcp_config.to_string_lossy().into_owned());
        if let Some(p) = &self.append_system_prompt {
            a.push("--append-system-prompt".into());
            a.push(p.to_string_lossy().into_owned());
        }
        a.push(
            if self.approve {
                APPROVE_FLAG
            } else {
                NO_APPROVE_FLAG
            }
            .into(),
        );
        if let Some(x) = &self.exclude_tools {
            a.push("--exclude-tools".into());
            a.push(x.clone());
        }
        if !self.model.is_empty() {
            a.push("--model".into());
            a.push(self.model.clone());
        }
        if !self.thinking.is_empty() {
            a.push("--thinking".into());
            a.push(self.thinking.clone());
        }
        a
    }
}

// ── the session id ──────────────────────────────────────────────────────────

/// Does the id `get_state` reported name the session loomux asked for?
///
/// **An exact compare, and that is not the same choice
/// [`super::claude::session_ids_match`] made.** Claude Code's `--session-id`
/// "must be a valid UUID", so canonicalizing 32 hex nibbles is both possible and
/// necessary there. pi's ids are not UUIDs — `docs/rpc.md:200-213` shows
/// `"sessionId": "abc123"` — so there is no canonical form to reduce to, and
/// inventing a tolerance (case folding? trimming punctuation?) would be a
/// guess about a namespace this module cannot see.
///
/// What justifies the strictness is the flag's own word: `--session-id` is
/// documented "Use **exact** project session ID, creating it if missing"
/// (`dist/cli/args.js:279`). Surrounding whitespace is still tolerated, because
/// that is a transport artifact rather than an identity.
///
/// The residual is live item 3 in `doc/design/pi.md`: the exact textual form
/// `get_state` echoes back has not been observed, so the first real session is
/// what confirms this compare never fires spuriously.
pub fn session_ids_match(asked: &str, reported: &str) -> bool {
    asked.trim() == reported.trim()
}

// ── command ids: the one contract between the driver and the decoder ────────

/// The command id the driver's first `get_state` carries, and the only id the
/// decoder turns into [`HarnessEvent::Booted`].
///
/// **Correlation by id PREFIX rather than by a shared map**, and the reason is
/// the thread boundary: the decoder runs on the pump thread and the pane's
/// senders run on the caller's, so a `Map<id, what-I-expected>` between them
/// would be shared mutable state guarded by a lock on the hot path. A reserved
/// id and two prefixes carry the same information with no state at all, and
/// they are testable without a process — which a lock-guarded map is not.
///
/// `id` is optional on every pi command and is echoed on the response when
/// present (`docs/rpc.md:26`), so this costs nothing on the wire.
pub const BOOT_ID: &str = "boot";
/// Prefix of every `get_session_stats` the driver sends. See [`BOOT_ID`].
pub const STATS_ID_PREFIX: &str = "stats-";
/// Prefix of every `prompt` the driver sends. See [`BOOT_ID`].
pub const PROMPT_ID_PREFIX: &str = "p-";

/// What [`HarnessEvent::Booted`] reports as this pane's capabilities.
///
/// **Derived from the RPC command set this driver implements**, not scraped and
/// not reported by pi — pi has no capability announcement, because it has no
/// boot event at all. Each name is a command this module can actually send
/// (`docs/rpc.md:80`, `:102`, `:124`, `:397`, `:240`, `:304`) plus `ui_dialog`
/// for the extension-UI sub-protocol it can answer (`:1190`). The field is an
/// open set a later consumer feature-detects on, which is why it is a list of
/// strings rather than a bitfield.
pub const CAPABILITIES: [&str; 7] = [
    "steer",
    "follow_up",
    "abort",
    "compact",
    "set_model",
    "set_thinking_level",
    "ui_dialog",
];

// ── the wire: what the driver SENDS ─────────────────────────────────────────
//
// Every command line is built by a free function returning a `String`, so a
// test can assert the exact bytes without constructing a live pane or a child
// process (constraint 3 by construction, the way `claude::wire_line` is).

/// The first command the driver sends: the `get_state` whose reply becomes
/// [`HarnessEvent::Booted`].
pub fn boot_line() -> String {
    line(&serde_json::json!({ "id": BOOT_ID, "type": "get_state" }))
}

/// One delivery.
///
/// `streamingBehavior` is **always `followUp`, never `steer`**, and the choice
/// is semantic rather than a default. If the agent is already streaming and no
/// `streamingBehavior` is given, the command is an error (`docs/rpc.md:65`), so
/// one of the two must be picked. `steer` "is delivered after the current
/// assistant turn finishes executing its tool calls, before the next LLM call"
/// (`:62`) — an interjection *inside* a turn — while `followUp` waits "until the
/// agent finishes" (`:63`). orrerix's delivery vocabulary is a TURN, never a
/// mid-turn interjection, so `followUp` is the one that means what a [`Turn`]
/// means. [`PiPane::steer`] exists for the pi-only case and is a different
/// method for exactly that reason.
pub fn prompt_line(seq: u64, turn: &Turn) -> String {
    line(&serde_json::json!({
        "id": format!("{PROMPT_ID_PREFIX}{seq}"),
        "type": "prompt",
        "message": turn.text(),
        "streamingBehavior": "followUp",
    }))
}

/// A mid-turn steering message. pi-only; not reachable through [`AgentPane`].
pub fn steer_line(text: &str) -> String {
    line(&serde_json::json!({ "type": "steer", "message": text }))
}

/// End the turn in progress without killing the pane (`docs/rpc.md:124`).
pub fn abort_line() -> String {
    line(&serde_json::json!({ "type": "abort" }))
}

/// Ask for the session-wide token and cost totals (`docs/rpc.md:554`).
pub fn stats_line(seq: u64) -> String {
    line(&serde_json::json!({
        "id": format!("{STATS_ID_PREFIX}{seq}"),
        "type": "get_session_stats",
    }))
}

/// Compact the context by hand (`docs/rpc.md:397`).
pub fn compact_line() -> String {
    line(&serde_json::json!({ "type": "compact" }))
}

/// Switch model (`docs/rpc.md:240`).
pub fn set_model_line(provider: &str, model_id: &str) -> String {
    line(&serde_json::json!({
        "type": "set_model",
        "provider": provider,
        "modelId": model_id,
    }))
}

/// Set the reasoning level (`docs/rpc.md:304`).
pub fn set_thinking_level_line(level: &str) -> String {
    line(&serde_json::json!({ "type": "set_thinking_level", "level": level }))
}

/// Settle one extension dialog (`docs/rpc.md:1352-1375`).
///
/// The three answer shapes are three different payloads, not one payload with
/// three readings: `value` for `select`/`input`/`editor`, `confirmed` for
/// `confirm`, and `cancelled: true` for a dismissal of any of them — where the
/// extension receives `undefined` for the first three and `false` for `confirm`.
/// A `Cancelled` sent as `confirmed: false` would tell the extension a human
/// said no when nobody said anything.
pub fn ui_response_line(req: &RequestId, answer: &UiAnswer) -> String {
    let mut o = serde_json::Map::new();
    o.insert("type".into(), Value::String("extension_ui_response".into()));
    o.insert("id".into(), Value::String(req.0.clone()));
    match answer {
        UiAnswer::Value(v) => {
            o.insert("value".into(), Value::String(v.clone()));
        }
        UiAnswer::Confirmed(b) => {
            o.insert("confirmed".into(), Value::Bool(*b));
        }
        UiAnswer::Cancelled => {
            o.insert("cancelled".into(), Value::Bool(true));
        }
    }
    line(&Value::Object(o))
}

/// One JSONL record: the compact JSON plus the single `\n` the protocol names
/// as its only delimiter (`docs/rpc.md:30`).
fn line(v: &Value) -> String {
    format!("{v}\n")
}

// ── the decoder ─────────────────────────────────────────────────────────────

/// What one stream line became.
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded {
    /// A pane event.
    Event(HarnessEvent),
    /// A real message that is not a pane event: a command acknowledgement, a
    /// retry, an extension error, a fire-and-forget UI method. Diagnostic, so it
    /// belongs in the pane log and not in the transcript a human reads.
    Note(String),
    /// A message type this build does not know. Ignored as an event, kept as
    /// evidence.
    Unknown(String),
    /// A command the decoder needs the driver to SEND.
    ///
    /// The one thing pi's protocol requires that a one-way stream does not: the
    /// per-turn usage figure lives behind a `get_session_stats` request
    /// (`docs/rpc.md:554`), so closing a turn means asking a question. Returning
    /// it as a value rather than writing it here keeps the decoder a pure
    /// function of its input — [`pump`] does the writing, and a test asserts the
    /// exact bytes over a `Vec<u8>` with no child process anywhere.
    Command(String),
}

/// What the decoder is waiting for a `get_session_stats` reply to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingStats {
    turn: TurnId,
    seq: u64,
}

/// Turns pi's RPC event stream into events.
///
/// Stateful for five reasons the stream cannot supply on its own: the **turn
/// number** (see [`TurnId`]), the **session id** and **model** learned from the
/// boot reply, the **per-turn usage accumulator**, the **outstanding stats
/// request** a turn's end is waiting on, and the **last accumulated tool
/// output** per tool call, which is what makes a delta out of an accumulation.
#[derive(Debug, Default)]
pub struct Decoder {
    open_turn: Option<TurnId>,
    next_turn: u64,
    session: Option<String>,
    model: Option<String>,
    /// The sum of this turn's `message_end` usages. See [`Decoder::message_end`]
    /// for why it is `message_end` and emphatically not `message_update`.
    turn_tokens: Tokens,
    /// Set to `true` by any `message_end` carrying a `usage` object, so an
    /// absent per-turn figure stays `None` rather than becoming a zero.
    saw_turn_usage: bool,
    pending_stats: Option<PendingStats>,
    next_stats_seq: u64,
    /// Last accumulated `partialResult` text per `toolCallId`.
    tool_output: HashMap<String, String>,
    last_error: Option<String>,
    last_prompt_ack: Option<(String, bool)>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The session id the boot reply reported, once it has.
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// The most recent `success: false` response, as `"<command>: <error>"`.
    ///
    /// Kept because pi reports a rejected command as a response and **never**
    /// as an event (`docs/rpc.md:1378-1397`), so a driver that only watched the
    /// event stream would see a prompt vanish with nothing to show for it.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// The most recent `prompt` response as `(command id, success)`.
    ///
    /// pi is the first harness that gives [`AgentPane::send`] a real
    /// acknowledgement: "the command response is emitted after the prompt is
    /// accepted, queued, or handled" (`docs/rpc.md:45`), and `success: false`
    /// "means the prompt was rejected before acceptance", while a failure AFTER
    /// acceptance arrives on the event stream and never as a second response for
    /// that id (`:76`).
    ///
    /// **So this is an acceptance, not a completion**, and [`SendReceipt`] is
    /// deliberately left as a bytes receipt rather than being widened to carry
    /// it: the ack arrives on the pump thread, asynchronously, after `send` has
    /// already returned. Surfacing it to the queue drainer is S3b's, and this
    /// accessor is the seam it will read.
    pub fn last_prompt_ack(&self) -> Option<(&str, bool)> {
        self.last_prompt_ack.as_ref().map(|(i, s)| (i.as_str(), *s))
    }

    /// Decode one line.
    ///
    /// Returns a **vector** because one line is legitimately several things: a
    /// `tool_execution_end` is an output delta *and* a result, and an `agent_end`
    /// is both a turn boundary and a command that has to go out.
    ///
    /// A line that is not JSON at all is [`Decoded::Unknown`], not an error: the
    /// child's stdout is a pipe, and a decoder that failed the pane on one
    /// malformed line would turn a cosmetic glitch into an outage.
    pub fn decode_line(&mut self, line: &str) -> Vec<Decoded> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![Decoded::Unknown(line.to_string())];
        };
        match v.get("type").and_then(Value::as_str) {
            Some("response") => self.response(&v, line),

            // An orrerix TURN spans one agent RUN. pi emits `turn_start` per
            // assistant turn *inside* a run (`docs/rpc.md:913-915`), so mapping
            // each of those to `TurnStarted` would report three turns where a
            // human saw one exchange, and `TurnEnded` — which needs the stats
            // round trip `agent_end` triggers — would not pair with them.
            // `ensure_turn` therefore opens a turn only when none is open, and
            // both events route through it.
            Some("agent_start") | Some("turn_start") => {
                let mut out = Vec::new();
                self.ensure_turn(&mut out);
                out
            }
            Some("agent_end") => self.agent_end(&v),
            Some("message_update") => self.message_update(&v, line),
            Some("message_end") => self.message_end(&v),
            Some("tool_execution_update") => self.tool_execution_update(&v),
            Some("tool_execution_end") => self.tool_execution_end(&v),
            Some("queue_update") => self.queue_update(&v),
            Some("compaction_end") => self.compaction_end(&v),
            Some("extension_ui_request") => self.extension_ui_request(&v, line),

            // Real messages that are not pane events. Each is diagnostic and
            // each is named rather than swept into `Unknown`, because `Unknown`
            // means "nobody taught this build about it" and these are taught.
            Some("agent_settled") => vec![Decoded::Note("agent_settled".into())],
            Some("turn_end") => vec![Decoded::Note("turn_end".into())],
            Some("message_start") => vec![Decoded::Note("message_start".into())],
            Some("tool_execution_start") => {
                let tool = str_at(&v, "toolName").unwrap_or("?");
                let id = str_at(&v, "toolCallId").unwrap_or("?");
                vec![Decoded::Note(format!("tool_execution_start {tool} ({id})"))]
            }
            Some("bash_execution_update") => vec![Decoded::Note("bash_execution_update".into())],
            Some("compaction_start") => {
                // Deliberately NOT `Compacted`: this says a compaction BEGAN,
                // and `compaction_end` is where both facts the event carries
                // exist — see `compaction_end`.
                let reason = str_at(&v, "reason").unwrap_or("?");
                vec![Decoded::Note(format!("compaction_start ({reason})"))]
            }
            // ── the three kinds a human has to SEE ───────────────────────
            //
            // These are `HarnessEvent::Note`s and not [`Decoded::Note`]s, and the
            // difference is the whole point of that variant: a `Decoded::Note`
            // reaches the pane LOG and no consumer, so a pane that spent four
            // minutes in a retry loop would show a human nothing. See
            // [`HarnessEvent::Note`] for where the line is drawn and why the
            // bookkeeping below it stays log-only.
            Some("auto_retry_start") => {
                let attempt = v.get("attempt").and_then(Value::as_u64).unwrap_or(0);
                let max = v.get("maxAttempts").and_then(Value::as_u64).unwrap_or(0);
                let delay = v.get("delayMs").and_then(Value::as_u64).unwrap_or(0);
                let err = str_at(&v, "errorMessage").unwrap_or("unknown");
                self.note(NoteKind::Retry, format!("retry {attempt}/{max} in {delay} ms: {err}"))
            }
            Some("auto_retry_end") => {
                let ok = v.get("success").and_then(Value::as_bool).unwrap_or(false);
                let attempt = v.get("attempt").and_then(Value::as_u64).unwrap_or(0);
                if ok {
                    self.note(NoteKind::Retry, format!("retry {attempt} succeeded"))
                } else {
                    // The final failure is an ERROR, not a retry: nothing is
                    // retrying it any more, which is the distinction the two
                    // kinds exist to keep.
                    let fin = str_at(&v, "finalError").unwrap_or("no error text");
                    self.note(NoteKind::Error, format!("retries exhausted after {attempt}: {fin}"))
                }
            }
            Some("summarization_retry_scheduled") => {
                let attempt = v.get("attempt").and_then(Value::as_u64).unwrap_or(0);
                let max = v.get("maxAttempts").and_then(Value::as_u64).unwrap_or(0);
                let err = str_at(&v, "errorMessage").unwrap_or("unknown");
                self.note(
                    NoteKind::Retry,
                    format!("summarization retry {attempt}/{max}: {err}"),
                )
            }
            // The other two summarization events carry no failure and no figure
            // — they bracket a retry already reported above — so they are
            // bookkeeping.
            Some("summarization_retry_attempt_start") | Some("summarization_retry_finished") => {
                let t = v.get("type").and_then(Value::as_str).unwrap_or("?");
                vec![Decoded::Note(t.to_string())]
            }
            Some("extension_error") => {
                let path = str_at(&v, "extensionPath").unwrap_or("?");
                let event = str_at(&v, "event").unwrap_or("?");
                let err = str_at(&v, "error").unwrap_or("?");
                self.note(NoteKind::Error, format!("extension {path} failed on {event}: {err}"))
            }
            _ => vec![Decoded::Unknown(line.to_string())],
        }
    }

    /// A command response.
    ///
    /// Three cases, split by the id the driver put on the way out (see
    /// [`BOOT_ID`]): the boot `get_state` becomes [`HarnessEvent::Booted`], a
    /// `get_session_stats` reply closes the turn that asked for it, and
    /// everything else is a note — plus [`Decoder::last_error`] when it failed.
    fn response(&mut self, v: &Value, raw: &str) -> Vec<Decoded> {
        let id = str_at(v, "id").unwrap_or("");
        let command = str_at(v, "command").unwrap_or("");
        let success = v.get("success").and_then(Value::as_bool).unwrap_or(false);

        if !success {
            let err = str_at(v, "error").unwrap_or("no error text");
            self.last_error = Some(format!("{command}: {err}"));
        }
        if command == "prompt" {
            self.last_prompt_ack = Some((id.to_string(), success));
        }

        if id == BOOT_ID && command == "get_state" {
            if !success {
                // A failed boot query is not a `Booted` with empty fields:
                // synthesizing one would report a session that never answered.
                return vec![Decoded::Note(format!(
                    "boot get_state failed: {}",
                    str_at(v, "error").unwrap_or("no error text")
                ))];
            }
            let data = v.get("data");
            self.session = data.and_then(|d| str_at(d, "sessionId")).map(str::to_string);
            // `model` is a full Model object or null (`docs/rpc.md:216`); its
            // `id` is the identity a usage row and a pane header key on, and
            // `name` is a display string. Take `id`.
            self.model = data
                .and_then(|d| d.get("model"))
                .and_then(|m| str_at(m, "id"))
                .map(str::to_string);
            return vec![Decoded::Event(HarnessEvent::Booted {
                session: self.session.clone(),
                model: self.model.clone(),
                capabilities: CAPABILITIES.iter().map(|s| s.to_string()).collect(),
            })];
        }

        if id.starts_with(STATS_ID_PREFIX) && command == "get_session_stats" {
            return self.stats_reply(id, v, success);
        }

        // A response with no `command` at all is not a shape this build knows.
        // Keep the whole line as evidence rather than logging a note about a
        // command nobody can name.
        if command.is_empty() {
            return vec![Decoded::Unknown(raw.to_string())];
        }
        if success {
            // An acknowledgement is bookkeeping: one per command, nothing for a
            // human to do, and `last_prompt_ack` is where a caller reads it.
            return vec![Decoded::Note(format!("response {command} ok"))];
        }
        // A REJECTION is not. pi reports a rejected command as a response and
        // never as an event (`docs/rpc.md:1378-1397`), so this is the only place
        // a dropped prompt is visible at all.
        self.note(
            NoteKind::Error,
            format!(
                "{command} was rejected: {}",
                str_at(v, "error").unwrap_or("no error text")
            ),
        )
    }

    /// One [`HarnessEvent::Note`], attributed to the open turn if there is one.
    ///
    /// **It does not OPEN a turn**, which is why this is a helper and not a call
    /// to `ensure_turn`: a retry that begins before the first turn, or an
    /// extension that throws at boot, would otherwise manufacture a turn nobody
    /// prompted and shift every later `TurnId` by one. `None` is the honest
    /// answer there, and §1.3 says so.
    fn note(&self, note: NoteKind, text: String) -> Vec<Decoded> {
        vec![Decoded::Event(HarnessEvent::Note {
            turn: self.open_turn,
            note,
            text,
        })]
    }

    /// The `get_session_stats` reply that closes a turn.
    fn stats_reply(&mut self, id: &str, v: &Value, success: bool) -> Vec<Decoded> {
        let Some(pending) = self.pending_stats else {
            // A reply to a request this decoder does not have outstanding — a
            // second reply, or one that arrived after the turn was already
            // bounded by a fresh `turn_start`. Keep it as evidence rather than
            // emitting a second `TurnEnded` for a turn that already ended.
            return vec![Decoded::Note(format!("unmatched stats reply {id}"))];
        };
        if id != format!("{STATS_ID_PREFIX}{}", pending.seq) {
            return vec![Decoded::Note(format!("unmatched stats reply {id}"))];
        }
        self.pending_stats = None;

        let data = success.then(|| v.get("data")).flatten();
        let usage = data.map(|d| self.usage_from_stats(d));
        let cost = data
            .and_then(|d| d.get("cost"))
            .and_then(Value::as_f64)
            .map(|usd| Cost {
                usd,
                basis: CostBasis::HarnessEstimate,
            });
        let ev = HarnessEvent::TurnEnded {
            turn: pending.turn,
            usage,
            cost,
            stop: StopReason::Completed,
        };
        self.reset_turn_usage();
        vec![Decoded::Event(ev)]
    }

    /// Token counts out of a `get_session_stats` reply.
    ///
    /// **`call_cumulative` is session-wide and must never be summed across
    /// turns.** `tokens` and `cost` "include assistant messages, usage reported
    /// by tools, and compaction/branch-summary generation across the full
    /// session" (`docs/rpc.md:593`), so adding two readings double-counts
    /// everything before the first — which is exactly the trap
    /// [`Usage::call_cumulative`]'s own doc names.
    ///
    /// `per_model` is left **empty**, and that is a stated absence rather than
    /// an omission: pi's stats reply has no per-model breakdown at all
    /// (`docs/rpc.md:562-591`), and a single-row vector attributing the whole
    /// session to whichever model the pane happens to be on now would be a
    /// fabricated attribution for a pane that switched models mid-session —
    /// precisely the failure the field exists to prevent.
    fn usage_from_stats(&self, data: &Value) -> Usage {
        let t = data.get("tokens");
        let call_cumulative = Tokens {
            input: u64_at(t, "input"),
            output: u64_at(t, "output"),
            cache_read: u64_at(t, "cacheRead"),
            cache_creation: u64_at(t, "cacheWrite"),
        };
        Usage {
            call_cumulative,
            this_turn_main_loop: Some(self.turn_tokens),
            per_model: Vec::new(),
        }
    }

    /// One agent run completed.
    ///
    /// Closes the open turn and asks for the figures that turn's `TurnEnded`
    /// needs. `willRetry: true` means "an automatic retry will follow"
    /// (`docs/rpc.md:895`), so the run is not over and the turn stays open —
    /// ending it here would report a turn that is about to keep going, and the
    /// retry's own output would then land on the NEXT turn number.
    fn agent_end(&mut self, v: &Value) -> Vec<Decoded> {
        if v.get("willRetry").and_then(Value::as_bool).unwrap_or(false) {
            return vec![Decoded::Note("agent_end (retry follows)".into())];
        }
        let Some(turn) = self.open_turn.take() else {
            return vec![Decoded::Note("agent_end with no open turn".into())];
        };
        let seq = self.next_stats_seq;
        self.next_stats_seq += 1;
        self.pending_stats = Some(PendingStats { turn, seq });
        vec![Decoded::Command(stats_line(seq))]
    }

    fn message_update(&mut self, v: &Value, raw: &str) -> Vec<Decoded> {
        let Some(ev) = v.get("assistantMessageEvent") else {
            return vec![Decoded::Unknown(raw.to_string())];
        };
        // NOTE: the top-level `usage` on a `message_update` is deliberately NOT
        // accumulated. It is "the latest cumulative provider-reported usage"
        // (`docs/rpc.md:983`) — cumulative for the message in flight, re-sent on
        // every delta — so adding it up would multiply one message's tokens by
        // its delta count. `message_end` is the once-per-message figure, and
        // that is what `message_end` sums.
        match ev.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                let Some(d) = str_at(ev, "delta").filter(|d| !d.is_empty()) else {
                    return Vec::new();
                };
                let mut out = Vec::new();
                let turn = self.ensure_turn(&mut out);
                out.push(Decoded::Event(HarnessEvent::Text {
                    turn,
                    delta: d.to_string(),
                }));
                out
            }
            Some("thinking_delta") => {
                let Some(d) = str_at(ev, "delta").filter(|d| !d.is_empty()) else {
                    return Vec::new();
                };
                let mut out = Vec::new();
                let turn = self.ensure_turn(&mut out);
                out.push(Decoded::Event(HarnessEvent::Thinking {
                    turn,
                    delta: d.to_string(),
                }));
                out
            }
            // A tool call becomes an event ONCE, at `toolcall_end`, which
            // "contains the completed call" (`docs/rpc.md:995`). `toolcall_start`
            // has the id and name but no arguments, and `toolcall_delta` carries
            // "a tool call arguments chunk" (`:972`) — partial JSON, which is not
            // a call. Emitting on either would put an argument-less or
            // half-parsed `ToolCall` into the audit log, which is the one place
            // a tool call's arguments are read.
            Some("toolcall_end") => {
                let Some(call) = ev.get("toolCall") else {
                    return vec![Decoded::Unknown(raw.to_string())];
                };
                let mut out = Vec::new();
                let turn = self.ensure_turn(&mut out);
                out.push(Decoded::Event(HarnessEvent::ToolCall {
                    turn,
                    id: ToolUseId(str_at(call, "id").unwrap_or_default().to_string()),
                    name: str_at(call, "name").unwrap_or_default().to_string(),
                    input: call.get("arguments").cloned().unwrap_or(Value::Null),
                }));
                out
            }
            // The block-boundary deltas. Known and carrying nothing a pane
            // event needs, so they draw nothing and are not `Unknown`.
            Some("text_start") | Some("text_end") | Some("thinking_start")
            | Some("thinking_end") | Some("toolcall_start") | Some("toolcall_delta") => Vec::new(),
            _ => vec![Decoded::Unknown(raw.to_string())],
        }
    }

    /// A message completed: the once-per-message usage figure.
    ///
    /// This is the ONLY input to [`Usage::this_turn_main_loop`]. `message_end`
    /// passes an `AgentMessage` straight through
    /// (`dist/modes/json-event.js:16-19`), so `message.usage` is that message's
    /// own `{input, output, cacheRead, cacheWrite}` (`docs/rpc.md:1457-1463`),
    /// and summing those across a turn's messages gives the turn.
    fn message_end(&mut self, v: &Value) -> Vec<Decoded> {
        let Some(u) = v.get("message").and_then(|m| m.get("usage")) else {
            return vec![Decoded::Note("message_end".into())];
        };
        self.saw_turn_usage = true;
        let t = self.turn_tokens;
        self.turn_tokens = Tokens {
            input: t.input.saturating_add(u64_at(Some(u), "input")),
            output: t.output.saturating_add(u64_at(Some(u), "output")),
            cache_read: t.cache_read.saturating_add(u64_at(Some(u), "cacheRead")),
            cache_creation: t
                .cache_creation
                .saturating_add(u64_at(Some(u), "cacheWrite")),
        };
        vec![Decoded::Note("message_end (usage accumulated)".into())]
    }

    /// A tool's streamed output.
    ///
    /// **The subtraction the contract owes.** `partialResult` "contains the
    /// accumulated output so far (not just the delta), allowing clients to
    /// simply replace their display on each update" (`docs/rpc.md:1055`), while
    /// [`HarnessEvent::ToolOutput`]`.delta` is a delta on every harness. So the
    /// decoder keeps the last accumulation per `toolCallId` and emits the
    /// suffix.
    ///
    /// Its precondition — each update is a PREFIX-extension of the last — is
    /// what "simply replace their display" implies but does **not** promise, so
    /// the failure case is handled rather than assumed: an update that is not an
    /// extension of its predecessor emits the **whole new value**, marked
    /// [`HarnessEvent::ToolOutput`]`.replaces`.
    ///
    /// **The mark is the point, and an earlier version of this module did not
    /// have it.** Emitting the whole value unmarked left a consumer appending it
    /// to what it already held, showing the output twice; #2891 S2 established
    /// that this cannot be closed downstream, because no consumer can tell a
    /// restatement from a legitimate delta that repeats earlier bytes. The
    /// adapter can, so the adapter says so.
    fn tool_execution_update(&mut self, v: &Value) -> Vec<Decoded> {
        let Some(id) = str_at(v, "toolCallId") else {
            return vec![Decoded::Note("tool_execution_update without a toolCallId".into())];
        };
        let accumulated = content_text(v.get("partialResult"));
        self.emit_tool_output(id, &accumulated, false)
    }

    /// A tool completed: the last of its output, then the verdict.
    ///
    /// Two events from one line, in that order, because they say different
    /// things: [`HarnessEvent::ToolOutput`] carries the bytes and
    /// [`HarnessEvent::ToolResult`] carries `ok`. A consumer that only reads the
    /// verdict is unaffected; one that only reads output still sees the tail
    /// that no `tool_execution_update` delivered.
    fn tool_execution_end(&mut self, v: &Value) -> Vec<Decoded> {
        let Some(id) = str_at(v, "toolCallId") else {
            return vec![Decoded::Note("tool_execution_end without a toolCallId".into())];
        };
        let id = id.to_string();
        let is_error = v.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let final_text = content_text(v.get("result"));
        let mut out = self.emit_tool_output(&id, &final_text, is_error);
        // The accumulation is per call and the call is over; keeping it would
        // grow one map entry per tool call for the life of the pane.
        self.tool_output.remove(&id);
        let turn = self.ensure_turn(&mut out);
        out.push(Decoded::Event(HarnessEvent::ToolResult {
            turn,
            id: ToolUseId(id),
            ok: !is_error,
        }));
        out
    }

    /// The subtraction itself. Emits nothing when the accumulation has not
    /// grown — a repeated identical `partialResult` is not new output, and an
    /// empty `ToolOutput` would be a renderer's problem for no information.
    fn emit_tool_output(&mut self, id: &str, accumulated: &str, is_error: bool) -> Vec<Decoded> {
        let previous = self.tool_output.get(id);
        let (delta, restated) = match previous {
            Some(p) if accumulated.starts_with(p.as_str()) => {
                (accumulated[p.len()..].to_string(), false)
            }
            Some(_) => (accumulated.to_string(), true),
            // The FIRST output for a call is not a restatement: there is nothing
            // held for it to replace, and marking it `true` would make every tool
            // start with a replace nobody needs.
            None => (accumulated.to_string(), false),
        };
        self.tool_output
            .insert(id.to_string(), accumulated.to_string());

        let mut out = Vec::new();
        if restated {
            out.push(Decoded::Note(format!(
                "tool output for {id} was not an extension of the previous \
                 partialResult; restating the whole value"
            )));
        }
        if delta.is_empty() {
            return out;
        }
        let turn = self.ensure_turn(&mut out);
        out.push(Decoded::Event(HarnessEvent::ToolOutput {
            turn,
            id: ToolUseId(id.to_string()),
            delta,
            is_error,
            replaces: restated,
        }));
        out
    }

    fn queue_update(&mut self, v: &Value) -> Vec<Decoded> {
        vec![Decoded::Event(HarnessEvent::QueueChanged {
            steering: strings_at(v, "steering"),
            follow_up: strings_at(v, "followUp"),
        })]
    }

    /// A compaction finished.
    ///
    /// **[`HarnessEvent::Compacted`] is emitted here, not on `compaction_start`,
    /// and that is a deliberate deviation from plan-2386's decoder row.** Two
    /// reasons, both of which the start event cannot answer:
    ///
    /// 1. `compaction_start` carries only `reason` (`docs/rpc.md:1074`), so a
    ///    `Compacted` built from it has `pre_tokens: None` **always** — the
    ///    field would be dead. `compaction_end.result.tokensBefore` (`:1086`) is
    ///    the real figure, and it exists only here.
    /// 2. A compaction can abort or fail: `result` is `null` with `aborted:
    ///    true`, or `null` with an `errorMessage` (`:1105-1107`). Emitting on the
    ///    start event reports a compaction that never happened, and nothing
    ///    later retracts it.
    ///
    /// So a compaction that did not complete is a note, and `compaction_start`
    /// is a note. Exactly one `Compacted` per successful compaction.
    fn compaction_end(&mut self, v: &Value) -> Vec<Decoded> {
        let reason = str_at(v, "reason").unwrap_or("");
        let aborted = v.get("aborted").and_then(Value::as_bool).unwrap_or(false);
        let result = v.get("result").filter(|r| !r.is_null());
        let Some(result) = result.filter(|_| !aborted) else {
            let err = str_at(v, "errorMessage").unwrap_or("no result");
            // An ERROR the human sees, not a log line: a compaction that failed
            // on quota leaves the pane about to hit its context window, and the
            // symptom a human would otherwise get is a pane that stops working
            // for no visible reason.
            return self.note(
                NoteKind::Error,
                format!("compaction ({reason}) did not compact: aborted={aborted} {err}"),
            );
        };
        vec![Decoded::Event(HarnessEvent::Compacted {
            // "manual", "threshold" or "overflow" (`docs/rpc.md:1077`). The two
            // automatic ones fold into `Auto` because the distinction the
            // vocabulary keeps is who asked, and an unrecognized value folds
            // there too rather than being reported as a human's decision.
            trigger: if reason == "manual" {
                CompactTrigger::Manual
            } else {
                CompactTrigger::Auto
            },
            pre_tokens: result.get("tokensBefore").and_then(Value::as_u64),
        })]
    }

    /// An extension dialog, or one of the fire-and-forget methods that share its
    /// envelope.
    ///
    /// **Only the four dialog methods become a [`HarnessEvent::UiRequest`].**
    /// `notify`, `setStatus`, `setWidget`, `setTitle` and `set_editor_text`
    /// "emit an `extension_ui_request` on stdout but do not expect a response"
    /// (`docs/rpc.md:1191`), so routing one through the request/settle machinery
    /// would park a pane on a status-bar update and raise a needs-you item for a
    /// question nobody asked.
    fn extension_ui_request(&mut self, v: &Value, raw: &str) -> Vec<Decoded> {
        let Some(id) = str_at(v, "id") else {
            return vec![Decoded::Unknown(raw.to_string())];
        };
        let method = match v.get("method").and_then(Value::as_str) {
            Some("select") => UiMethod::Select,
            Some("confirm") => UiMethod::Confirm,
            Some("input") => UiMethod::Input,
            Some("editor") => UiMethod::Editor,
            // Named, so a fire-and-forget method is evidence in the pane log
            // rather than an unknown line — and so a method pi adds later shows
            // up as `Unknown` and gets classified, instead of being silently
            // treated as fire-and-forget.
            Some(m @ ("notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text")) => {
                // A `Ui` note, not a `UiRequest`: the harness DISPLAYED something
                // and expects no answer (`docs/rpc.md:1191`). `notify` in
                // particular is how an extension tells a human it blocked a
                // command, which is exactly the thing that must not vanish into
                // a log file.
                let text = str_at(v, "message")
                    .or_else(|| str_at(v, "statusText"))
                    .or_else(|| str_at(v, "title"))
                    .or_else(|| str_at(v, "text"))
                    .unwrap_or(m);
                return self.note(NoteKind::Ui, text.to_string());
            }
            _ => return vec![Decoded::Unknown(raw.to_string())],
        };
        vec![Decoded::Event(HarnessEvent::UiRequest {
            id: RequestId(id.to_string()),
            method,
            title: str_at(v, "title").map(str::to_string),
            message: str_at(v, "message").map(str::to_string),
            options: strings_at(v, "options"),
            // Milliseconds (`docs/rpc.md:1213`). Descriptive: pi "will
            // auto-resolve with a default value when the timeout expires. The
            // client does not need to track timeouts" (`:1193`), so orrerix
            // records what pi decided and never races it.
            timeout_ms: v.get("timeout").and_then(Value::as_u64),
        })]
    }

    /// The turn every content event belongs to, opening one if none is.
    ///
    /// Turn boundaries are **derived**, exactly as on the Claude side, but from
    /// a different signal: pi has no user-message echo either, so the rule is
    /// that `agent_start`/`turn_start`, or the first content event, opens a
    /// turn, and `agent_end` closes it.
    ///
    /// **A pending turn is bounded by the next turn**, which is what stops a
    /// lost or never-answered `get_session_stats` from swallowing the turn it
    /// was closing: if a fresh turn opens while a stats request is still
    /// outstanding, the old turn ends here with `usage: None` — the honest
    /// "not known" — rather than staying open forever or inheriting the next
    /// turn's figures.
    fn ensure_turn(&mut self, out: &mut Vec<Decoded>) -> TurnId {
        if let Some(t) = self.open_turn {
            return t;
        }
        if let Some(pending) = self.pending_stats.take() {
            out.push(Decoded::Event(HarnessEvent::TurnEnded {
                turn: pending.turn,
                usage: None,
                cost: None,
                stop: StopReason::Completed,
            }));
            self.reset_turn_usage();
        }
        let t = TurnId(self.next_turn);
        self.next_turn += 1;
        self.open_turn = Some(t);
        out.push(Decoded::Event(HarnessEvent::TurnStarted { turn: t }));
        t
    }

    fn reset_turn_usage(&mut self) {
        self.turn_tokens = Tokens::default();
        self.saw_turn_usage = false;
    }
}

/// A `{content: [{type: "text", text}, ...]}` block flattened to its text.
///
/// Non-text content parts are skipped rather than rendered as a placeholder: a
/// tool result's image part has no text, and inventing one would put a
/// loomux-authored string into a stream of the tool's own bytes.
fn content_text(result: Option<&Value>) -> String {
    let Some(parts) = result.and_then(|r| r.get("content")).and_then(Value::as_array) else {
        return String::new();
    };
    let mut s = String::new();
    for p in parts {
        if p.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                s.push_str(t);
            }
        }
    }
    s
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn u64_at(v: Option<&Value>, key: &str) -> u64 {
    v.and_then(|v| v.get(key)).and_then(Value::as_u64).unwrap_or(0)
}

fn strings_at(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ── the one thing a pi pane refuses ─────────────────────────────────────────

/// Why a pi pane cannot settle a permission request.
///
/// A free function rather than a literal inside the `impl`, so a test can assert
/// what it SAYS without constructing a live pane. What that costs, stated: the
/// wiring from [`AgentPane::answer`] to it is a one-line body the compiler
/// checks and no test does.
///
/// **This refusal is not [`super::claude::permission_answer_unavailable`]'s.**
/// Claude's is "not yet — R2 builds the channel". pi's is structural: pi has no
/// permission-prompt mechanism at all — no permission engine, no prompts, and
/// nothing in the RPC command set to answer one with (`docs/rpc.md:39-853`).
/// pi's questions arrive as [`HarnessEvent::UiRequest`] and are settled through
/// [`PiPane::answer_ui`], which is a different channel with a different trust
/// boundary. A pi pane that returned `Ok(())` here would report that it had
/// applied a decision to a request that cannot exist.
pub fn permission_answer_unavailable(req: &RequestId, decision: Decision) -> String {
    format!(
        "cannot settle {req:?} as {decision:?}: pi has no permission-prompt \
         channel to settle one on — its questions arrive as UiRequest and are \
         answered with answer_ui — see doc/design/harness-adapters.md §3.5"
    )
}

// ── the child driver ────────────────────────────────────────────────────────

/// Read every line of a stream, decode it, log it, publish the events, and send
/// back the commands the decoder asks for.
///
/// Separated from process spawning so the whole decode-publish-respond path is
/// exercised over an in-memory reader and an in-memory writer: this slice's
/// tests never start a child, which is how "never spawn a real agent CLI"
/// (CLAUDE.md constraint 3) is satisfied by construction rather than by
/// remembering.
///
/// A send failure ends the pump: the receiver is gone, so there is nobody left
/// to publish to.
///
/// **Memory is bounded by ONE line**, for the reason
/// [`super::claude::pump`] states: `lines()` allocates each line whole and
/// nothing accumulates across lines — except [`Decoder::tool_output`], which
/// holds one accumulation per *in-flight* tool call and drops it at
/// `tool_execution_end`. A tool whose output grows unboundedly is therefore
/// bounded by pi's own line length twice over rather than once, which is the
/// honest statement rather than a claim of constant memory.
pub fn pump<R: BufRead, W: Write>(
    reader: R,
    decoder: &mut Decoder,
    log: &mut Option<EventLog>,
    tx: &Sender<HarnessEvent>,
    commands: &mut Option<W>,
) {
    for line in reader.lines() {
        let Ok(line) = line else { break };
        for decoded in decoder.decode_line(&line) {
            match decoded {
                Decoded::Event(ev) => {
                    if let Some(log) = log.as_mut() {
                        let _ = log.record(ev.clone());
                    }
                    if tx.send(ev).is_err() {
                        return;
                    }
                }
                Decoded::Note(note) => {
                    if let Some(log) = log.as_mut() {
                        let _ = log.append(LogBody::Note { note });
                    }
                }
                Decoded::Unknown(raw) => {
                    if let Some(log) = log.as_mut() {
                        let _ = log.record_unknown(&raw);
                    }
                }
                Decoded::Command(cmd) => {
                    // A write failure here is the child's stdin having gone
                    // away, which the reader loop is about to discover on its
                    // own. Logging it and carrying on beats failing the pane on
                    // a usage query.
                    if let Some(w) = commands.as_mut() {
                        if let Err(e) = w.write_all(cmd.as_bytes()).and_then(|()| w.flush()) {
                            if let Some(log) = log.as_mut() {
                                let _ = log.append(LogBody::Note {
                                    note: format!("could not send {}: {e}", cmd.trim()),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

/// How long [`PiPane::drop`] waits for a child to leave after its stdin closes.
///
/// pi has no exit or quit command; `process.stdin.on("end", ...)` calls
/// `shutdown()` (`dist/modes/rpc/rpc-mode.js:641-644`), so EOF **is** the
/// teardown and nothing needs a console control event — which is the Windows
/// problem this avoids rather than solves. Two seconds is a bound on a
/// cooperative exit, not an estimate of one: the kill below is what actually
/// guarantees the process leaves.
pub const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The child's stdin, shared by the pane and its pump thread.
///
/// **Both write to it, so there is exactly one handle behind one lock.** The
/// pump needs a writer because the decoder asks for a `get_session_stats` on
/// every turn boundary ([`Decoded::Command`]), and `ChildStdin` cannot be cloned
/// portably. The two alternatives were worse: a second thread relaying a channel
/// onto the handle adds a thread and a failure mode nobody reads, and routing
/// [`AgentPane::send`] through a channel too would put every delivery behind a
/// queue whose failures a caller could not see — turning `send`'s `Result` into
/// a lie about bytes that had not been written.
///
/// The `Option` is the close: `None` means stdin has been dropped, which is
/// pi's documented shutdown, and every writer then fails rather than blocking.
type SharedStdin = std::sync::Arc<Mutex<Option<ChildStdin>>>;

/// A [`Write`] view of [`SharedStdin`], so [`pump`] can take a plain `W: Write`
/// and its tests can pass a `Vec<u8>` with no pipe anywhere.
struct StdinSink(SharedStdin);

impl Write for StdinSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self.0.lock_safe();
        let w = guard.as_mut().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pane stdin is closed")
        })?;
        w.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut guard = self.0.lock_safe();
        match guard.as_mut() {
            Some(w) => w.flush(),
            // Nothing buffered can survive a closed handle, so a flush after the
            // close is a no-op rather than an error: the write that mattered has
            // already failed and been reported.
            None => Ok(()),
        }
    }
}

/// One pi pane, driven over stdin/stdout in `--mode rpc`.
pub struct PiPane {
    child: Mutex<Child>,
    stdin: SharedStdin,
    rx: Mutex<Option<EventRx>>,
    session: Mutex<Option<String>>,
    sent_bytes: AtomicU64,
    /// The `prompt` command counter. See [`BOOT_ID`] for why an id is a counter
    /// with a prefix and not a uuid — and CLAUDE.md constraint 2 for why it
    /// could not have been a uuid anyway.
    prompt_seq: AtomicU64,
}

impl PiPane {
    /// Spawn the user's own `pi` on `spec`, and send the boot query.
    ///
    /// **`program` is a parameter and never a literal**, so nothing in this
    /// crate can start a real CLI on its own: this slice's tests exercise
    /// [`pump`] over in-memory streams and never reach this function
    /// (constraint 3). On Windows `pi` is an npm `.cmd` shim, and resolving that
    /// to something `CreateProcessW` will start is the CALLER's job (S3b) — the
    /// same resolution `src-tauri` already does for `gh` and `git`.
    ///
    /// stderr is inherited rather than piped, for [`super::claude::ClaudePane`]'s
    /// reason: a piped stderr nobody drains fills its buffer and blocks the
    /// child.
    ///
    /// The boot `get_state` goes out **here**, before any caller can send a
    /// turn, because [`HarnessEvent::Booted`] is synthesized from its reply and
    /// a pane that never asked would never boot.
    pub fn spawn(program: &Path, spec: &LaunchSpec, log: Option<EventLog>) -> std::io::Result<Self> {
        let mut child = std::process::Command::new(program)
            .args(spec.argv())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no stdout pipe"))?;

        if let Some(w) = stdin.as_mut() {
            w.write_all(boot_line().as_bytes())?;
            w.flush()?;
        }
        let stdin: SharedStdin = std::sync::Arc::new(Mutex::new(stdin));

        let (tx, rx) = mpsc::channel();
        let mut log = log;
        let mut sink = Some(StdinSink(std::sync::Arc::clone(&stdin)));
        std::thread::spawn(move || {
            let mut decoder = Decoder::new();
            pump(
                std::io::BufReader::new(stdout),
                &mut decoder,
                &mut log,
                &tx,
                &mut sink,
            );
        });

        Ok(PiPane {
            child: Mutex::new(child),
            stdin,
            rx: Mutex::new(Some(rx)),
            session: Mutex::new(None),
            sent_bytes: AtomicU64::new(0),
            prompt_seq: AtomicU64::new(0),
        })
    }

    /// Write one already-built command line to the child.
    fn write_line(&self, line: &str) -> Result<u64, String> {
        let mut guard = self.stdin.lock_safe();
        let stdin = guard
            .as_mut()
            .ok_or_else(|| "this pane's stdin is closed".to_string())?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("writing to the pane failed: {e}"))?;
        let n = line.len() as u64;
        Ok(self.sent_bytes.fetch_add(n, Ordering::Relaxed) + n)
    }

    /// Settle one extension dialog (§3.5).
    ///
    /// **Every agent may be asked; no agent may ever answer.** Who is allowed to
    /// call this is a property of the ENTRY POINT — S3b's one trusted
    /// `answer_pane_ui` command — and never an argument here, which is the
    /// `questions.json` boundary and exists for its reason: an agent that could
    /// answer its own dialog has a gate that is theatre. This method is the
    /// mechanism; the gate is upstream of it.
    pub fn answer_ui(&self, req: RequestId, answer: UiAnswer) -> Result<(), String> {
        self.write_line(&ui_response_line(&req, &answer)).map(|_| ())
    }

    /// Queue a mid-turn steering message. pi-only, deliberately not on
    /// [`AgentPane`]: see [`prompt_line`] for why a [`Turn`] is never a steer.
    pub fn steer(&self, text: &str) -> Result<(), String> {
        self.write_line(&steer_line(text)).map(|_| ())
    }

    /// Compact the context by hand. pi-only.
    pub fn compact(&self) -> Result<(), String> {
        self.write_line(&compact_line()).map(|_| ())
    }

    /// Switch model mid-session. pi-only.
    pub fn set_model(&self, provider: &str, model_id: &str) -> Result<(), String> {
        self.write_line(&set_model_line(provider, model_id)).map(|_| ())
    }

    /// Set the reasoning level mid-session. pi-only.
    pub fn set_thinking_level(&self, level: &str) -> Result<(), String> {
        self.write_line(&set_thinking_level_line(level)).map(|_| ())
    }

    /// Close the child's stdin, which is pi's documented shutdown
    /// (`dist/modes/rpc/rpc-mode.js:641-644`).
    pub fn close_input(&self) -> Result<(), String> {
        *self.stdin.lock_safe() = None;
        Ok(())
    }

    /// Record the session id the boot reply reported, refusing a mismatch.
    ///
    /// **A mismatch kills the pane**: it means `--session-id` did not take
    /// effect, so the pane is not the session loomux believes it is, and every
    /// downstream record — usage, resume, transcript — would key on the wrong
    /// one. Fail closed; do not reconcile.
    pub fn bind_session(&self, asked: &str, reported: &str) -> Result<(), String> {
        if !session_ids_match(asked, reported) {
            return Err(format!(
                "session id mismatch: asked for {asked}, pi reported {reported} — \
                 the --session-id flag did not take effect, so this pane is not \
                 the session loomux thinks it is"
            ));
        }
        *self.session.lock_safe() = Some(asked.to_string());
        Ok(())
    }
}

impl AgentPane for PiPane {
    fn kind(&self) -> PaneKind {
        PaneKind::Structured(Harness::Pi)
    }

    fn send(&self, turn: Turn) -> Result<SendReceipt, String> {
        let seq = self.prompt_seq.fetch_add(1, Ordering::Relaxed);
        let total = self.write_line(&prompt_line(seq, &turn))?;
        Ok(SendReceipt {
            accepted_at_bytes: total,
        })
    }

    /// **Refused, and structurally rather than pending a slice** — see
    /// [`permission_answer_unavailable`]. pi has no permission-prompt channel;
    /// its questions are [`HarnessEvent::UiRequest`]s, settled with
    /// [`PiPane::answer_ui`].
    fn answer(&self, req: RequestId, decision: Decision) -> Result<(), String> {
        Err(permission_answer_unavailable(&req, decision))
    }

    /// **Honoured** — the first harness where it is.
    ///
    /// `abort` is a command (`docs/rpc.md:124`), not a signal, so there is
    /// nothing to invent and no SIGINT this Windows baseline does not have. The
    /// Claude adapter still refuses, and
    /// [`the_first_harness_that_honours_interrupt_and_the_one_that_still_does_not`]
    /// pins both halves.
    fn interrupt(&self) -> Result<(), String> {
        self.write_line(&abort_line()).map(|_| ())
    }

    fn events(&self) -> Option<EventRx> {
        self.rx.lock_safe().take()
    }

    fn session_id(&self) -> Option<String> {
        self.session.lock_safe().clone()
    }
}

impl Drop for PiPane {
    /// End the turn, then the process: `abort`, close stdin, wait
    /// [`SHUTDOWN_GRACE`], kill.
    ///
    /// The graceful path exists here and not on the Claude side because pi has
    /// the command that makes it graceful. The kill is still unconditional after
    /// the wait: a cooperative shutdown that has not happened in two seconds is
    /// not one that is about to.
    fn drop(&mut self) {
        let _ = self.write_line(&abort_line());
        *self.stdin.lock_safe() = None;
        let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
        loop {
            let mut child = self.child.lock_safe();
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(_) => break,
            }
            drop(child);
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let _ = self.child.lock_safe().kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // No test in this module starts a process. Everything runs the decoder over
    // an in-memory reader and asserts on an in-memory writer, which is how
    // CLAUDE.md constraint 3 ("never spawn a real agent CLI") is satisfied by
    // construction here rather than by remembering.

    fn spec() -> LaunchSpec {
        LaunchSpec {
            session_id: Some("abc123".into()),
            session_dir: PathBuf::from("C:/g/pi"),
            mcp_config: PathBuf::from("C:/g/mcp.json"),
            append_system_prompt: Some(PathBuf::from("C:/g/w-1.md")),
            approve: true,
            exclude_tools: Some("edit,write".into()),
            model: "z-ai/glm-5.3-flash".into(),
            thinking: "medium".into(),
        }
    }

    /// Decode a whole stream, returning everything it produced.
    fn decode_all(d: &mut Decoder, text: &str) -> Vec<Decoded> {
        let mut out = Vec::new();
        for line in text.lines() {
            out.extend(d.decode_line(line));
        }
        out
    }

    fn events(all: &[Decoded]) -> Vec<HarnessEvent> {
        all.iter()
            .filter_map(|d| match d {
                Decoded::Event(e) => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    fn notes(all: &[Decoded]) -> Vec<String> {
        all.iter()
            .filter_map(|d| match d {
                Decoded::Note(n) => Some(n.clone()),
                _ => None,
            })
            .collect()
    }

    fn commands(all: &[Decoded]) -> Vec<String> {
        all.iter()
            .filter_map(|d| match d {
                Decoded::Command(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    fn unknowns(all: &[Decoded]) -> Vec<String> {
        all.iter()
            .filter_map(|d| match d {
                Decoded::Unknown(u) => Some(u.clone()),
                _ => None,
            })
            .collect()
    }

    // ── the launch line ─────────────────────────────────────────────────────

    #[test]
    fn the_rpc_launch_line_is_the_pty_line_plus_one_flag_and_carries_it_first() {
        let argv = spec().argv();
        assert_eq!(
            &argv[..2],
            &["--mode".to_string(), "rpc".to_string()],
            "the mode flag leads, so a diff against the PTY line is one insert"
        );
        // Every flag the PTY arm emits is here, because `--mode` gates none of
        // them (dist/cli/args.js:40-43).
        for flag in [
            "--session-id",
            "--session-dir",
            "--mcp-config",
            "--append-system-prompt",
            "--approve",
            "--exclude-tools",
            "--model",
            "--thinking",
        ] {
            assert!(argv.iter().any(|a| a == flag), "argv dropped {flag}: {argv:?}");
        }
    }

    #[test]
    fn exactly_one_trust_flag_is_always_emitted_and_never_both_or_neither() {
        // pi's one boot dialog ("Trust project folder?") can never be allowed to
        // appear on a pane loomux is about to prompt, so the pair is not
        // optional — a spec with neither flag has no spelling, because the field
        // is a `bool` and not an `Option<bool>`. This pins that the type's
        // guarantee really reaches argv, on both settings.
        for approve in [true, false] {
            let argv = LaunchSpec {
                approve,
                ..spec()
            }
            .argv();
            let n = argv
                .iter()
                .filter(|a| *a == APPROVE_FLAG || *a == NO_APPROVE_FLAG)
                .count();
            assert_eq!(n, 1, "approve={approve} produced {n} trust flags: {argv:?}");
            let expected = if approve { APPROVE_FLAG } else { NO_APPROVE_FLAG };
            assert!(argv.iter().any(|a| a == expected), "{argv:?}");
        }
    }

    #[test]
    fn an_empty_model_or_thinking_is_a_silence_and_not_a_blank_argument() {
        let argv = LaunchSpec {
            model: String::new(),
            thinking: String::new(),
            exclude_tools: None,
            append_system_prompt: None,
            session_id: None,
            ..spec()
        }
        .argv();
        for flag in [
            "--model",
            "--thinking",
            "--exclude-tools",
            "--append-system-prompt",
            "--session-id",
        ] {
            assert!(
                !argv.iter().any(|a| a == flag),
                "an absent value must omit {flag} entirely, never pass it empty: {argv:?}"
            );
        }
        // The two that are never optional survive the stripping, which is the
        // control that this test is not just asserting an empty argv.
        assert!(argv.iter().any(|a| a == "--session-dir"), "{argv:?}");
        assert!(argv.iter().any(|a| a == "--mcp-config"), "{argv:?}");
    }

    #[test]
    fn a_resumed_pi_line_is_byte_identical_to_a_fresh_one() {
        // pi's `--session-id` is "Use exact project session ID, creating it if
        // missing" (dist/cli/args.js:279), so one flag serves both directions and
        // there is no resume branch to get wrong. `pi_launch_flags_per_posture`
        // pins the same equality on the PTY side; this is the RPC half of it, and
        // it fails the moment someone adds a `--resume`-shaped branch here.
        let fresh = spec().argv();
        let resumed = spec().argv();
        assert_eq!(fresh, resumed);
        // The discriminating half: a DIFFERENT id really does change the line, so
        // the equality above is not the vacuous "argv() is a pure function".
        let other = LaunchSpec {
            session_id: Some("def456".into()),
            ..spec()
        }
        .argv();
        assert_ne!(fresh, other);
    }

    #[test]
    fn a_pi_session_id_is_compared_exactly_because_it_is_not_a_uuid() {
        // Claude's ids are UUIDs and are canonicalized (`claude::session_ids_match`).
        // pi's are not — docs/rpc.md:200-213 shows `"sessionId": "abc123"` — so
        // there is no canonical form to reduce to, and the flag's own word is
        // "exact" (dist/cli/args.js:279).
        assert!(session_ids_match("abc123", "abc123"));
        assert!(session_ids_match("abc123", " abc123\n"), "transport whitespace is not identity");
        assert!(!session_ids_match("abc123", "ABC123"), "case is part of a pi id");
        assert!(!session_ids_match("abc123", "abc124"));
        assert!(!session_ids_match("abc123", ""));
    }

    // ── framing ─────────────────────────────────────────────────────────────

    #[test]
    fn the_framing_is_lf_only_and_u2028_stays_inside_the_string() {
        // The claim the module header makes about `BufRead::lines`, measured
        // rather than asserted. `docs/rpc.md:37` singles out Node `readline` as
        // non-compliant BECAUSE it splits on U+2028, which is valid inside a JSON
        // string — so a reader that split here would hand `serde_json` two
        // fragments and the delta would vanish into an `Unknown`.
        let delta = "Listing\u{2028}now";
        let line = serde_json::json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": delta},
        })
        .to_string();
        assert!(line.contains('\u{2028}'), "the fixture byte must be literal, not escaped");

        let mut d = Decoder::new();
        let all = pump_to_vec(&mut d, &format!("{line}\n"));
        assert_eq!(
            events(&all),
            vec![
                HarnessEvent::TurnStarted { turn: TurnId(0) },
                HarnessEvent::Text {
                    turn: TurnId(0),
                    delta: delta.into(),
                },
            ],
            "U+2028 must survive framing INSIDE the delta"
        );
    }

    #[test]
    fn a_line_may_end_with_crlf_and_the_record_is_unchanged() {
        // "Accept optional `\r\n` input by stripping a trailing `\r`"
        // (docs/rpc.md:34). Pinned from an INLINE string rather than from the
        // fixture on purpose: the fixture's own line endings differ by platform
        // under `core.autocrlf=true`, so a fixture-based assertion would be
        // measuring the checkout rather than the reader.
        let lf = "{\"type\":\"queue_update\",\"steering\":[\"a\"],\"followUp\":[]}\n";
        let crlf = lf.replace('\n', "\r\n");
        assert_ne!(lf, crlf, "the two inputs must actually differ");

        let mut a = Decoder::new();
        let mut b = Decoder::new();
        assert_eq!(pump_to_vec(&mut a, lf), pump_to_vec(&mut b, &crlf));
        assert_eq!(
            events(&pump_to_vec(&mut Decoder::new(), &crlf)),
            vec![HarnessEvent::QueueChanged {
                steering: vec!["a".into()],
                follow_up: vec![],
            }]
        );
    }

    /// Run `text` through [`pump`] and collect what came out, so the framing
    /// tests exercise the real `BufRead::lines` path rather than a hand split.
    fn pump_to_vec(d: &mut Decoder, text: &str) -> Vec<Decoded> {
        // `pump` publishes events and swallows notes into a log, so this drives
        // the decoder over the same reader `pump` uses and keeps everything.
        let mut out = Vec::new();
        for line in std::io::BufRead::lines(std::io::BufReader::new(text.as_bytes())) {
            out.extend(d.decode_line(&line.unwrap()));
        }
        out
    }

    // ── boot ────────────────────────────────────────────────────────────────

    #[test]
    fn booted_is_synthesized_from_the_boot_reply_and_a_failed_one_boots_nothing() {
        let mut d = Decoder::new();
        let reply = serde_json::json!({
            "id": BOOT_ID, "type": "response", "command": "get_state", "success": true,
            "data": {"sessionId": "abc123", "model": {"id": "m/1", "name": "M One"}},
        })
        .to_string();
        let all = d.decode_line(&reply);
        assert_eq!(
            events(&all),
            vec![HarnessEvent::Booted {
                session: Some("abc123".into()),
                model: Some("m/1".into()),
                capabilities: CAPABILITIES.iter().map(|s| s.to_string()).collect(),
            }],
            "the model IDENTITY is the id, not the display name"
        );
        assert_eq!(d.session(), Some("abc123"));

        // The failure direction: a `get_state` that failed is NOT a `Booted` with
        // empty fields, which would report a session that never answered.
        let mut d = Decoder::new();
        let failed = serde_json::json!({
            "id": BOOT_ID, "type": "response", "command": "get_state",
            "success": false, "error": "no session",
        })
        .to_string();
        let all = d.decode_line(&failed);
        assert!(events(&all).is_empty(), "{all:?}");
        assert_eq!(d.session(), None);
        assert_eq!(d.last_error(), Some("get_state: no session"));
    }

    #[test]
    fn a_pi_pane_never_reports_observed_evidence() {
        // §1.3's rule, on the harness that has the least excuse to break it:
        // pi reports everything, so nothing here is inferred and `Observed(..)`
        // must never appear. Run over the whole fixture, which is the widest
        // input this module has.
        let text = include_str!("../../tests/fixtures/harness/pi/one-turn.jsonl");
        let mut d = Decoder::new();
        let all = decode_all(&mut d, text);
        let evs = events(&all);
        assert!(
            !evs.iter().any(|e| matches!(e, HarnessEvent::Observed(_))),
            "a structured pane must never emit inferred evidence: {evs:?}"
        );
        // The population control: the scan really saw a stream. Without it this
        // assertion passes just as well over an empty decode.
        assert!(
            evs.len() >= 10,
            "the fixture decoded to {} events, so the scan above saw almost nothing",
            evs.len()
        );
    }

    // ── the decoder table ───────────────────────────────────────────────────

    #[test]
    fn reasoning_becomes_thinking_and_can_never_become_transcript_text() {
        // The whole reason `Thinking` is its own variant (#2891): a renderer that
        // quiets thinking cannot do so if the two share a constructor.
        let mut d = Decoder::new();
        let all = decode_all(
            &mut d,
            &[
                serde_json::json!({"type":"message_update","assistantMessageEvent":
                    {"type":"thinking_delta","delta":"the user wants"}})
                .to_string(),
                serde_json::json!({"type":"message_update","assistantMessageEvent":
                    {"type":"text_delta","delta":"Listing now"}})
                .to_string(),
            ]
            .join("\n"),
        );
        assert_eq!(
            events(&all),
            vec![
                HarnessEvent::TurnStarted { turn: TurnId(0) },
                HarnessEvent::Thinking {
                    turn: TurnId(0),
                    delta: "the user wants".into(),
                },
                HarnessEvent::Text {
                    turn: TurnId(0),
                    delta: "Listing now".into(),
                },
            ]
        );
        // Stated as the property rather than as a list, so a decoder that emitted
        // BOTH a Thinking and a Text for the reasoning delta fails here too.
        let texts: Vec<_> = events(&all)
            .into_iter()
            .filter_map(|e| match e {
                HarnessEvent::Text { delta, .. } => Some(delta),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Listing now".to_string()]);
    }

    #[test]
    fn a_tool_call_is_reported_once_and_only_from_toolcall_end() {
        // `toolcall_start` has the id and name but no arguments, and
        // `toolcall_delta` carries "a tool call arguments chunk"
        // (docs/rpc.md:971-972) — partial JSON, which is not a call. Emitting on
        // either would put an argument-less or half-parsed `ToolCall` into the
        // AUDIT log, which is the one place a tool call's arguments are read.
        let mut d = Decoder::new();
        let all = decode_all(
            &mut d,
            &[
                serde_json::json!({"type":"message_update","assistantMessageEvent":
                    {"type":"toolcall_start","contentIndex":0,"id":"call_1","toolName":"bash"}})
                .to_string(),
                serde_json::json!({"type":"message_update","assistantMessageEvent":
                    {"type":"toolcall_delta","contentIndex":0,"delta":"{\"comm"}})
                .to_string(),
                serde_json::json!({"type":"message_update","assistantMessageEvent":
                    {"type":"toolcall_end","contentIndex":0,
                     "toolCall":{"id":"call_1","name":"bash","arguments":{"command":"ls -la"}}}})
                .to_string(),
            ]
            .join("\n"),
        );
        let calls: Vec<_> = events(&all)
            .into_iter()
            .filter(|e| matches!(e, HarnessEvent::ToolCall { .. }))
            .collect();
        assert_eq!(
            calls,
            vec![HarnessEvent::ToolCall {
                turn: TurnId(0),
                id: ToolUseId("call_1".into()),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls -la"}),
            }],
            "exactly one call, carrying the ARGUMENTS the end event completed"
        );
        // The two ignored deltas are not `Unknown` either: they are known and
        // deliberately silent, and logging them as unrecognized would fill the
        // pane log with lines nobody needs to add support for.
        assert!(unknowns(&all).is_empty(), "{all:?}");
    }

    #[test]
    fn tool_output_is_a_delta_although_pi_sends_an_accumulation() {
        // The subtraction the contract owes. `partialResult` "contains the
        // accumulated output so far (not just the delta)" (docs/rpc.md:1055),
        // and `ToolOutput.delta` is a delta on every harness — so the adapter
        // converts, because it holds the previous value and a renderer does not.
        let mut d = Decoder::new();
        let all = decode_all(&mut d, &tool_stream(&["total 48\n", "total 48\ndrwx\n"], "total 48\ndrwx\nls: no\n", true));
        let outs: Vec<_> = events(&all)
            .into_iter()
            .filter_map(|e| match e {
                HarnessEvent::ToolOutput {
                    delta,
                    is_error,
                    replaces,
                    ..
                } => Some((delta, is_error, replaces)),
                _ => None,
            })
            .collect();
        assert_eq!(
            outs,
            vec![
                ("total 48\n".to_string(), false, false),
                ("drwx\n".to_string(), false, false),
                ("ls: no\n".to_string(), true, false),
            ],
            "each update must yield only what is NEW since the last one, and a \
             well-behaved stream never asks a consumer to replace anything"
        );
        // Concatenating the deltas reconstructs the accumulation, which is the
        // property a consumer relies on and the one an off-by-one would break.
        let joined: String = outs.iter().map(|(d, _, _)| d.as_str()).collect();
        assert_eq!(joined, "total 48\ndrwx\nls: no\n");

        // The verdict still arrives once, separately, and after the bytes.
        let evs = events(&all);
        let result_at = evs
            .iter()
            .position(|e| matches!(e, HarnessEvent::ToolResult { .. }))
            .expect("a ToolResult");
        let last_out_at = evs
            .iter()
            .rposition(|e| matches!(e, HarnessEvent::ToolOutput { .. }))
            .expect("a ToolOutput");
        assert!(last_out_at < result_at, "the bytes come before the verdict: {evs:?}");
        assert_eq!(
            evs[result_at],
            HarnessEvent::ToolResult {
                turn: TurnId(0),
                id: ToolUseId("call_1".into()),
                ok: false,
            }
        );
    }

    #[test]
    fn a_partial_result_that_is_not_an_extension_restates_instead_of_slicing() {
        // The failure case S1a's §1.2 says this slice owes AS A TEST rather than
        // as an assumption: the prefix-extension precondition is what "simply
        // replace their display" IMPLIES and does not promise. An update that is
        // not an extension of its predecessor must emit the whole new value and
        // say it did — a consumer that appends deltas then shows the output
        // twice, which is visibly wrong, rather than a fragment sliced at an
        // offset that means nothing, which is invisibly wrong.
        let mut d = Decoder::new();
        let all = decode_all(&mut d, &tool_stream(&["aaaaaaaa", "bb"], "bb", false));
        let outs: Vec<_> = events(&all)
            .into_iter()
            .filter_map(|e| match e {
                HarnessEvent::ToolOutput {
                    delta, replaces, ..
                } => Some((delta, replaces)),
                _ => None,
            })
            .collect();
        assert_eq!(
            outs,
            vec![
                ("aaaaaaaa".to_string(), false),
                // The whole new value, never a suffix taken at the old length —
                // and MARKED, which is the half a consumer needs. Unmarked, a
                // renderer appending deltas shows the output twice, and #2891 S2
                // established it cannot tell this from a legitimate repeat.
                ("bb".to_string(), true),
            ]
        );
        assert!(
            notes(&all).iter().any(|n| n.contains("not an extension")),
            "and it is recorded for a human too: {:?}",
            notes(&all)
        );
        // The discriminating control: the same shapes that ARE extensions neither
        // set the flag nor record the note, so neither is written on every update.
        let mut d = Decoder::new();
        let clean = decode_all(&mut d, &tool_stream(&["aa", "aabb"], "aabb", false));
        assert!(
            !notes(&clean).iter().any(|n| n.contains("not an extension")),
            "{:?}",
            notes(&clean)
        );
        assert!(
            events(&clean).iter().all(|e| !matches!(
                e,
                HarnessEvent::ToolOutput { replaces: true, .. }
            )),
            "{clean:?}"
        );
    }

    #[test]
    fn an_unchanged_partial_result_emits_no_empty_delta() {
        // A repeated identical `partialResult` is not new output. An empty
        // `ToolOutput` would be a renderer's problem carrying no information.
        let mut d = Decoder::new();
        let all = decode_all(&mut d, &tool_stream(&["same", "same"], "same", false));
        let outs = events(&all)
            .into_iter()
            .filter(|e| matches!(e, HarnessEvent::ToolOutput { .. }))
            .count();
        assert_eq!(outs, 1, "only the first carries anything new: {all:?}");
    }

    /// A `tool_execution_update`* + `tool_execution_end` stream for one call.
    fn tool_stream(partials: &[&str], final_text: &str, is_error: bool) -> String {
        let mut lines = Vec::new();
        for p in partials {
            lines.push(
                serde_json::json!({
                    "type": "tool_execution_update", "toolCallId": "call_1", "toolName": "bash",
                    "partialResult": {"content": [{"type": "text", "text": p}]},
                })
                .to_string(),
            );
        }
        lines.push(
            serde_json::json!({
                "type": "tool_execution_end", "toolCallId": "call_1", "toolName": "bash",
                "result": {"content": [{"type": "text", "text": final_text}]},
                "isError": is_error,
            })
            .to_string(),
        );
        lines.join("\n")
    }

    #[test]
    fn only_a_dialog_method_becomes_a_ui_request_and_the_rest_are_notes() {
        // Both categories share the `extension_ui_request` envelope
        // (docs/rpc.md:1188-1191), so a decoder that triggered on the TYPE would
        // park a pane on a status-bar update and raise a needs-you item for a
        // question nobody asked.
        let mut d = Decoder::new();
        let dialogs = [
            ("select", UiMethod::Select),
            ("confirm", UiMethod::Confirm),
            ("input", UiMethod::Input),
            ("editor", UiMethod::Editor),
        ];
        for (name, expected) in dialogs {
            let all = d.decode_line(
                &serde_json::json!({
                    "type": "extension_ui_request", "id": "u1", "method": name,
                    "title": "T", "message": "M", "options": ["A", "B"], "timeout": 5000,
                })
                .to_string(),
            );
            assert_eq!(
                events(&all),
                vec![HarnessEvent::UiRequest {
                    id: RequestId("u1".into()),
                    method: expected,
                    title: Some("T".into()),
                    message: Some("M".into()),
                    options: vec!["A".into(), "B".into()],
                    timeout_ms: Some(5000),
                }],
                "{name} is a dialog method"
            );
        }
        // The negative control, which is the half that matters: the same envelope
        // with a fire-and-forget method is never a QUESTION. It is still visible —
        // a `Ui` note carrying what was displayed — because `notify` is how an
        // extension tells a human it blocked a command, and that must not vanish
        // into a log file.
        for name in ["notify", "setStatus", "setWidget", "setTitle", "set_editor_text"] {
            let all = d.decode_line(
                &serde_json::json!({"type": "extension_ui_request", "id": "u2", "method": name,
                                    "message": "Command blocked by user"})
                .to_string(),
            );
            assert_eq!(
                events(&all),
                vec![HarnessEvent::Note {
                    turn: None,
                    note: NoteKind::Ui,
                    text: "Command blocked by user".into(),
                }],
                "{name} must be seen but never asked"
            );
            assert!(
                !events(&all)
                    .iter()
                    .any(|e| matches!(e, HarnessEvent::UiRequest { .. })),
                "{name} must not become a UiRequest: {all:?}"
            );
        }
        // And a method pi adds LATER is `Unknown` — kept for classification —
        // rather than silently treated as one of the two categories.
        let all = d.decode_line(
            &serde_json::json!({"type": "extension_ui_request", "id": "u3", "method": "hologram"})
                .to_string(),
        );
        assert!(events(&all).is_empty(), "{all:?}");
        assert_eq!(unknowns(&all).len(), 1, "{all:?}");
    }

    #[test]
    fn a_compaction_is_reported_once_at_the_end_and_never_when_it_did_not_happen() {
        // A deliberate deviation from plan-2386's decoder row, argued on the
        // function: `compaction_start` carries only `reason` (docs/rpc.md:1074),
        // so a `Compacted` built from it has `pre_tokens: None` ALWAYS, and a
        // compaction that aborts or fails (`:1105-1107`) would already have been
        // reported as one that happened.
        let mut d = Decoder::new();
        let start = serde_json::json!({"type": "compaction_start", "reason": "threshold"}).to_string();
        assert!(events(&d.decode_line(&start)).is_empty(), "the START is not the fact");

        let ok = serde_json::json!({
            "type": "compaction_end", "reason": "threshold", "aborted": false,
            "result": {"tokensBefore": 150000, "estimatedTokensAfter": 32000},
        })
        .to_string();
        assert_eq!(
            events(&d.decode_line(&ok)),
            vec![HarnessEvent::Compacted {
                trigger: CompactTrigger::Auto,
                pre_tokens: Some(150000),
            }],
            "the END is where tokensBefore exists at all"
        );

        // `manual` is the one trigger a human asked for; `threshold` and
        // `overflow` are both automatic, and an unrecognized reason folds to
        // automatic rather than being reported as a human's decision.
        for (reason, expected) in [
            ("manual", CompactTrigger::Manual),
            ("threshold", CompactTrigger::Auto),
            ("overflow", CompactTrigger::Auto),
            ("some_new_reason", CompactTrigger::Auto),
        ] {
            let line = serde_json::json!({
                "type": "compaction_end", "reason": reason, "aborted": false,
                "result": {"tokensBefore": 1},
            })
            .to_string();
            assert_eq!(
                events(&d.decode_line(&line)),
                vec![HarnessEvent::Compacted {
                    trigger: expected,
                    pre_tokens: Some(1),
                }],
                "reason {reason}"
            );
        }

        // The two failure shapes, which must report nothing.
        for line in [
            serde_json::json!({"type": "compaction_end", "reason": "threshold",
                               "aborted": true, "result": Value::Null}),
            serde_json::json!({"type": "compaction_end", "reason": "threshold", "aborted": false,
                               "result": Value::Null, "errorMessage": "quota exceeded"}),
        ] {
            let all = d.decode_line(&line.to_string());
            assert!(
                !events(&all)
                    .iter()
                    .any(|e| matches!(e, HarnessEvent::Compacted { .. })),
                "a compaction that did not compact must not be reported as one: {all:?}"
            );
            // It is not SILENT either, and that is the other half: a compaction
            // that failed on quota leaves the pane about to hit its context
            // window, and a human whose pane then stops working for no visible
            // reason has been told nothing.
            assert!(
                matches!(
                    events(&all).as_slice(),
                    [HarnessEvent::Note {
                        note: NoteKind::Error,
                        ..
                    }]
                ),
                "{all:?}"
            );
        }
    }

    #[test]
    fn an_unknown_message_type_is_ignored_as_an_event_and_kept_as_evidence() {
        let mut d = Decoder::new();
        let line = r#"{"type":"something_pi_added_later","v":1}"#;
        let all = d.decode_line(line);
        assert_eq!(all, vec![Decoded::Unknown(line.to_string())]);

        // A line that is not JSON at all is the same: evidence, never an error
        // that fails the pane. The child's stdout is a pipe.
        let junk = "not json at all";
        assert_eq!(
            d.decode_line(junk),
            vec![Decoded::Unknown(junk.to_string())]
        );
        // And an empty line is neither: it is nothing.
        assert!(d.decode_line("   ").is_empty());
    }

    // ── usage ───────────────────────────────────────────────────────────────

    #[test]
    fn usage_is_cumulative_from_stats_and_the_turn_figure_is_summed_from_message_end() {
        // The two figures answer different questions and are never added
        // together, which is what `Usage`'s own doc says and what this measures
        // across TWO turns — the shape where a decoder that summed the stats
        // readings would report roughly double.
        let mut d = Decoder::new();
        let all = decode_all(&mut d, &two_turns());
        let ends: Vec<_> = events(&all)
            .into_iter()
            .filter_map(|e| match e {
                HarnessEvent::TurnEnded { turn, usage, cost, .. } => Some((turn, usage, cost)),
                _ => None,
            })
            .collect();
        assert_eq!(ends.len(), 2, "two turns, two ends: {all:?}");

        let (t0, u0, c0) = ends[0].clone();
        let (t1, u1, c1) = ends[1].clone();
        assert_eq!((t0, t1), (TurnId(0), TurnId(1)));

        let u0 = u0.expect("turn 0 usage");
        let u1 = u1.expect("turn 1 usage");
        // Session-wide, taken as read: the SECOND reading is the session total,
        // not the second turn's share and not the sum of the two readings.
        assert_eq!(u0.call_cumulative.input, 100);
        assert_eq!(u1.call_cumulative.input, 250);
        assert_ne!(
            u1.call_cumulative.input,
            u0.call_cumulative.input + 250,
            "the readings must never be summed — that is the trap the field's doc names"
        );
        // Per-turn, summed from `message_end` only. Turn 0 had two messages
        // (7 + 5 output) and turn 1 had one (9), so a decoder that accumulated
        // `message_update.usage` instead would report a much larger figure.
        assert_eq!(u0.this_turn_main_loop.expect("turn 0").output, 12);
        assert_eq!(u1.this_turn_main_loop.expect("turn 1").output, 9);
        assert_eq!(
            u0.this_turn_main_loop.expect("turn 0").cache_creation,
            10,
            "cacheWrite maps to cache_creation, SUMMED over the turn's two \
             messages (5 + 5) — the same summation the output figure above shows"
        );
        // The accumulator RESETS between turns; without that, turn 1 reads 21.
        assert!(u1.this_turn_main_loop.expect("turn 1").output < u0.this_turn_main_loop.expect("t0").output + 9);

        // pi has no per-model breakdown at all (docs/rpc.md:562-591), so the
        // field is EMPTY rather than one fabricated row attributing the whole
        // session to whichever model the pane is on now.
        assert!(u0.per_model.is_empty(), "{:?}", u0.per_model);

        assert_eq!(c0.expect("cost 0").usd, 0.10);
        assert_eq!(c1.expect("cost 1").basis, CostBasis::HarnessEstimate);
    }

    #[test]
    fn a_message_update_usage_is_never_accumulated() {
        // `message_update.usage` is "the latest cumulative provider-reported
        // usage" (docs/rpc.md:983) — cumulative for the message in flight and
        // re-sent on EVERY delta — so summing it multiplies one message's tokens
        // by its delta count. This is the mutation that would not otherwise
        // redden anything: the figures are plausible either way.
        let mut d = Decoder::new();
        let mut lines = Vec::new();
        for out in [1, 5, 9] {
            lines.push(
                serde_json::json!({"type":"message_update",
                    "usage":{"input":100,"output":out,"cacheRead":0,"cacheWrite":0},
                    "assistantMessageEvent":{"type":"text_delta","delta":"x"}})
                .to_string(),
            );
        }
        lines.push(
            serde_json::json!({"type":"message_end","message":{"role":"assistant",
                "usage":{"input":100,"output":9,"cacheRead":0,"cacheWrite":0}}})
            .to_string(),
        );
        lines.push(serde_json::json!({"type":"agent_end","willRetry":false}).to_string());
        lines.push(
            serde_json::json!({"id":"stats-0","type":"response","command":"get_session_stats",
                "success":true,"data":{"tokens":{"input":100,"output":9},"cost":0.01}})
            .to_string(),
        );
        let all = decode_all(&mut d, &lines.join("\n"));
        let turn_figure = events(&all)
            .into_iter()
            .find_map(|e| match e {
                HarnessEvent::TurnEnded { usage, .. } => usage,
                _ => None,
            })
            .and_then(|u| u.this_turn_main_loop)
            .expect("a per-turn figure");
        assert_eq!(
            turn_figure.output, 9,
            "the ONE message_end figure, never 1+5+9 from the updates and never \
             1+5+9+9 from both"
        );
        assert_eq!(turn_figure.input, 100, "and the input is not counted four times");
    }

    #[test]
    fn a_turn_with_no_message_end_reports_no_turn_figure_rather_than_a_zero() {
        // "A fact the pane does not have is `None`, never a sentinel" (§1.3).
        // A zero here reads as data — a turn that really cost nothing.
        let mut d = Decoder::new();
        let all = decode_all(
            &mut d,
            &[
                serde_json::json!({"type":"agent_start"}).to_string(),
                serde_json::json!({"type":"agent_end","willRetry":false}).to_string(),
                serde_json::json!({"id":"stats-0","type":"response","command":"get_session_stats",
                    "success":true,"data":{"tokens":{"input":7}}})
                .to_string(),
            ]
            .join("\n"),
        );
        let usage = events(&all)
            .into_iter()
            .find_map(|e| match e {
                HarnessEvent::TurnEnded { usage, .. } => usage,
                _ => None,
            })
            .expect("stats answered, so there IS a cumulative figure");
        assert_eq!(usage.call_cumulative.input, 7);
        assert_eq!(
            usage.this_turn_main_loop, None,
            "no message_end means the per-turn figure is unknown, not zero"
        );
    }

    /// Two complete turns, each ending with its own `get_session_stats` reply.
    fn two_turns() -> String {
        let mut l = Vec::new();
        l.push(serde_json::json!({"type":"agent_start"}).to_string());
        for out in [7, 5] {
            l.push(
                serde_json::json!({"type":"message_end","message":{"role":"assistant",
                    "usage":{"input":10,"output":out,"cacheRead":0,"cacheWrite":5}}})
                .to_string(),
            );
        }
        l.push(serde_json::json!({"type":"agent_end","willRetry":false}).to_string());
        l.push(
            serde_json::json!({"id":"stats-0","type":"response","command":"get_session_stats",
                "success":true,
                "data":{"tokens":{"input":100,"output":12,"cacheRead":0,"cacheWrite":10},"cost":0.10}})
            .to_string(),
        );
        l.push(serde_json::json!({"type":"agent_start"}).to_string());
        l.push(
            serde_json::json!({"type":"message_end","message":{"role":"assistant",
                "usage":{"input":10,"output":9,"cacheRead":0,"cacheWrite":0}}})
            .to_string(),
        );
        l.push(serde_json::json!({"type":"agent_end","willRetry":false}).to_string());
        l.push(
            serde_json::json!({"id":"stats-1","type":"response","command":"get_session_stats",
                "success":true,
                "data":{"tokens":{"input":250,"output":21,"cacheRead":0,"cacheWrite":10},"cost":0.22}})
            .to_string(),
        );
        l.join("\n")
    }

    // ── turn boundaries ─────────────────────────────────────────────────────

    #[test]
    fn agent_end_asks_for_the_figures_and_a_retry_keeps_the_turn_open() {
        let mut d = Decoder::new();
        let all = decode_all(
            &mut d,
            &[
                serde_json::json!({"type":"agent_start"}).to_string(),
                // "If `willRetry` is true, an automatic retry will follow"
                // (docs/rpc.md:895) — the run is not over.
                serde_json::json!({"type":"agent_end","willRetry":true}).to_string(),
            ]
            .join("\n"),
        );
        assert!(commands(&all).is_empty(), "a retry must not ask for figures yet: {all:?}");
        assert_eq!(
            events(&all),
            vec![HarnessEvent::TurnStarted { turn: TurnId(0) }],
            "and it must not end the turn: {all:?}"
        );

        // The real end asks, with the id the decoder will match the reply on.
        let all = d.decode_line(&serde_json::json!({"type":"agent_end","willRetry":false}).to_string());
        assert_eq!(commands(&all), vec![stats_line(0)]);
        assert!(commands(&all)[0].contains(STATS_ID_PREFIX));
        assert!(commands(&all)[0].ends_with('\n'), "one JSONL record");
        assert!(events(&all).is_empty(), "the turn ends when the REPLY lands");
    }

    #[test]
    fn a_stats_reply_that_never_comes_is_bounded_by_the_next_turn() {
        // Otherwise a lost or never-answered `get_session_stats` swallows the
        // turn it was closing, and the next turn's output lands on a turn that
        // never ended.
        let mut d = Decoder::new();
        let all = decode_all(
            &mut d,
            &[
                serde_json::json!({"type":"agent_start"}).to_string(),
                serde_json::json!({"type":"agent_end","willRetry":false}).to_string(),
                // No reply. A fresh turn opens instead.
                serde_json::json!({"type":"agent_start"}).to_string(),
            ]
            .join("\n"),
        );
        assert_eq!(
            events(&all),
            vec![
                HarnessEvent::TurnStarted { turn: TurnId(0) },
                HarnessEvent::TurnEnded {
                    turn: TurnId(0),
                    usage: None,
                    cost: None,
                    stop: StopReason::Completed,
                },
                HarnessEvent::TurnStarted { turn: TurnId(1) },
            ],
            "the old turn ends with the honest 'not known', in ORDER, before the new one opens"
        );

        // And the late reply then closes nothing: a second `TurnEnded` for a turn
        // that already ended would be a duplicate in the audit log.
        let late = d.decode_line(
            &serde_json::json!({"id":"stats-0","type":"response","command":"get_session_stats",
                "success":true,"data":{"tokens":{"input":1}}})
            .to_string(),
        );
        assert!(events(&late).is_empty(), "{late:?}");
        assert!(notes(&late).iter().any(|n| n.contains("unmatched")), "{late:?}");
    }

    #[test]
    fn one_orrerix_turn_spans_a_whole_agent_run_however_many_pi_turns_it_holds() {
        // pi emits `turn_start` per assistant turn INSIDE a run
        // (docs/rpc.md:913-915). Mapping each to `TurnStarted` would report three
        // turns where a human saw one exchange, and `TurnEnded` — which needs the
        // stats round trip only `agent_end` triggers — would not pair with them.
        let mut d = Decoder::new();
        let all = decode_all(
            &mut d,
            &[
                serde_json::json!({"type":"agent_start"}).to_string(),
                serde_json::json!({"type":"turn_start"}).to_string(),
                serde_json::json!({"type":"turn_end","toolResults":[]}).to_string(),
                serde_json::json!({"type":"turn_start"}).to_string(),
                serde_json::json!({"type":"turn_end","toolResults":[]}).to_string(),
                serde_json::json!({"type":"agent_end","willRetry":false}).to_string(),
            ]
            .join("\n"),
        );
        assert_eq!(
            events(&all),
            vec![HarnessEvent::TurnStarted { turn: TurnId(0) }],
            "three pi turn boundaries, ONE orrerix turn: {all:?}"
        );
        assert_eq!(commands(&all).len(), 1, "and one stats request, at the run's end");
    }

    // ── the wire the driver writes ──────────────────────────────────────────

    #[test]
    fn a_delivery_is_a_follow_up_prompt_and_a_steer_is_a_different_command() {
        // If the agent is streaming and no `streamingBehavior` is given the
        // command is an ERROR (docs/rpc.md:65), so one of the two must be
        // chosen. `steer` is delivered mid-turn, "before the next LLM call"
        // (`:62`); `followUp` waits "until the agent finishes" (`:63`). orrerix's
        // delivery vocabulary is a TURN, so a `Turn` is always the second.
        let line = prompt_line(3, &Turn::Prompt("do the thing".into()));
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["type"], "prompt");
        assert_eq!(v["streamingBehavior"], "followUp");
        assert_eq!(v["message"], "do the thing");
        assert_eq!(v["id"], format!("{PROMPT_ID_PREFIX}3"));
        assert!(line.ends_with('\n'), "one JSONL record (docs/rpc.md:30)");
        assert_eq!(line.matches('\n').count(), 1, "and exactly one: {line:?}");

        // Every `Turn` variant goes out the same way — the variant is what a
        // renderer and a policy read, not something that changes the command.
        for t in [
            Turn::Kickoff("k".into()),
            Turn::Prompt("p".into()),
            Turn::Notice("[orrerix] n".into()),
            Turn::Human("h".into()),
        ] {
            let v: Value = serde_json::from_str(prompt_line(0, &t).trim()).unwrap();
            assert_eq!(v["streamingBehavior"], "followUp", "{t:?}");
            assert_eq!(v["message"], t.text(), "{t:?}");
        }

        // A steer is a DIFFERENT command with no `streamingBehavior` at all, and
        // it is not reachable through `AgentPane`.
        let s: Value = serde_json::from_str(steer_line("stop").trim()).unwrap();
        assert_eq!(s["type"], "steer");
        assert!(s.get("streamingBehavior").is_none(), "{s}");
    }

    #[test]
    fn a_multiline_turn_stays_one_record() {
        // The framing rule cuts both ways: a prompt containing newlines must not
        // become several commands. `serde_json` escapes them, so this pins that
        // nothing re-expands them on the way out.
        let line = prompt_line(0, &Turn::Human("one\ntwo\nthree".into()));
        assert_eq!(line.matches('\n').count(), 1, "{line:?}");
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["message"], "one\ntwo\nthree");
    }

    #[test]
    fn a_ui_answer_is_three_wire_shapes_and_cancelled_is_not_a_declined() {
        // A cancellation gives the extension `undefined` for select/input/editor
        // and `false` for confirm (docs/rpc.md:1370). Sending `Cancelled` as
        // `confirmed: false` would tell the extension a human said no when
        // nobody said anything.
        let req = RequestId("uuid-1".into());
        let v: Value =
            serde_json::from_str(ui_response_line(&req, &UiAnswer::Value("Allow".into())).trim())
                .unwrap();
        assert_eq!(v["type"], "extension_ui_response");
        assert_eq!(v["id"], "uuid-1");
        assert_eq!(v["value"], "Allow");
        assert!(v.get("confirmed").is_none() && v.get("cancelled").is_none(), "{v}");

        let v: Value =
            serde_json::from_str(ui_response_line(&req, &UiAnswer::Confirmed(false)).trim()).unwrap();
        assert_eq!(v["confirmed"], false);
        assert!(v.get("cancelled").is_none(), "{v}");

        let v: Value =
            serde_json::from_str(ui_response_line(&req, &UiAnswer::Cancelled).trim()).unwrap();
        assert_eq!(v["cancelled"], true);
        assert!(
            v.get("confirmed").is_none(),
            "a cancellation must not be spelled as a declined confirm: {v}"
        );
    }

    #[test]
    fn the_boot_command_carries_the_one_id_the_decoder_turns_into_booted() {
        let v: Value = serde_json::from_str(boot_line().trim()).unwrap();
        assert_eq!(v["type"], "get_state");
        assert_eq!(v["id"], BOOT_ID);
        // The whole correlation contract in one place: a `get_state` the DRIVER
        // did not send (an extension's, a future feature's) carries a different
        // id and must not boot the pane a second time.
        let mut d = Decoder::new();
        let other = serde_json::json!({
            "id": "someone-elses", "type": "response", "command": "get_state",
            "success": true, "data": {"sessionId": "zzz"},
        })
        .to_string();
        let all = d.decode_line(&other);
        assert!(events(&all).is_empty(), "{all:?}");
        assert_eq!(d.session(), None, "and it must not rebind the session id");
    }

    #[test]
    fn the_pi_only_commands_are_the_shapes_the_docs_state() {
        let v: Value = serde_json::from_str(abort_line().trim()).unwrap();
        assert_eq!(v["type"], "abort");
        let v: Value = serde_json::from_str(compact_line().trim()).unwrap();
        assert_eq!(v["type"], "compact");
        let v: Value = serde_json::from_str(set_model_line("anthropic", "m-1").trim()).unwrap();
        assert_eq!((&v["type"], &v["provider"], &v["modelId"]),
                   (&Value::from("set_model"), &Value::from("anthropic"), &Value::from("m-1")));
        let v: Value = serde_json::from_str(set_thinking_level_line("high").trim()).unwrap();
        assert_eq!((&v["type"], &v["level"]), (&Value::from("set_thinking_level"), &Value::from("high")));
    }

    #[test]
    fn a_prompt_response_is_an_acceptance_and_a_rejection_is_recorded() {
        let mut d = Decoder::new();
        let ok = serde_json::json!({"id":"p-0","type":"response","command":"prompt","success":true})
            .to_string();
        d.decode_line(&ok);
        assert_eq!(d.last_prompt_ack(), Some(("p-0", true)));
        assert_eq!(d.last_error(), None);

        // "`success: false` means the prompt was rejected before acceptance"
        // (docs/rpc.md:76) — the one failure that arrives as a RESPONSE and never
        // as an event, so a driver watching only events would see a prompt vanish.
        let bad = serde_json::json!({"id":"p-1","type":"response","command":"prompt",
                                     "success":false,"error":"already streaming"})
            .to_string();
        d.decode_line(&bad);
        assert_eq!(d.last_prompt_ack(), Some(("p-1", false)));
        assert_eq!(d.last_error(), Some("prompt: already streaming"));
    }

    // ── the trait's two answers ─────────────────────────────────────────────

    #[test]
    fn the_first_harness_that_honours_interrupt_and_the_one_that_still_does_not() {
        // pi is the first harness where `interrupt()` is a real method, and the
        // pair is asserted together so "pi can interrupt" cannot quietly become
        // "everything can": `abort` is a COMMAND (docs/rpc.md:124), while Claude
        // Code's interrupt envelope is undocumented and this baseline has no
        // SIGINT.
        //
        // What this test does NOT reach, stated rather than implied: it pins the
        // WIRE FORM pi's `interrupt` writes, not the one-line trait body that
        // writes it — that body is checked by the compiler and by no test, which
        // is the same residual `claude.rs` states for its refusals.
        let v: Value = serde_json::from_str(abort_line().trim()).unwrap();
        assert_eq!(v["type"], "abort");
        assert!(v.get("id").is_none(), "abort needs no correlation id: {v}");

        let claude_refusal = super::super::claude::interrupt_unavailable();
        assert!(
            claude_refusal.contains("interrupt is not wired"),
            "the claude side must still refuse: {claude_refusal}"
        );
    }

    #[test]
    fn each_refusal_names_the_thing_that_would_lift_it_and_pi_s_is_structural() {
        // pi's `answer` refusal is NOT claude's. Claude's is "not yet — R2 builds
        // the channel"; pi has no permission-prompt mechanism at all, so its
        // questions arrive as `UiRequest` and are settled through `answer_ui`. A
        // refusal that said "not yet" would point at a slice that is never coming.
        let msg = permission_answer_unavailable(&RequestId("r1".into()), Decision::Allow);
        assert!(msg.contains("UiRequest"), "{msg}");
        assert!(msg.contains("answer_ui"), "{msg}");
        assert!(msg.contains("§3.5"), "the refusal points at the policy: {msg}");
        assert!(
            !msg.contains("R2"),
            "pi's refusal is structural, not pending a slice: {msg}"
        );
        // Both ids and both decisions reach the message, so a caller reading a
        // log can tell which request was refused.
        let deny = permission_answer_unavailable(&RequestId("r2".into()), Decision::Deny);
        assert!(deny.contains("r2") && deny.contains("Deny"), "{deny}");
    }

    // ── notes: what a human sees vs what only the log keeps ─────────────────

    #[test]
    fn the_three_kinds_a_human_sees_are_events_and_the_bookkeeping_is_log_only() {
        // The split `HarnessEvent::Note` exists to draw, stated as a PAIR because
        // either half alone is vacuous: a test that only checked the events would
        // pass on a decoder that promoted everything, and one that only checked
        // the log-only set would pass on a decoder that promoted nothing.
        //
        // The line is judgement, and it is this: a human can DO something about a
        // retry, an error and a thing the harness displayed. Message boundaries,
        // settle events and command acknowledgements are per-message volume with
        // nothing to do about them, and an event stream carrying those would drown
        // the three kinds this variant was added for.
        let visible: &[(&str, String, NoteKind)] = &[
            (
                "a retry beginning",
                serde_json::json!({"type":"auto_retry_start","attempt":1,"maxAttempts":3,
                                   "delayMs":2000,"errorMessage":"529 Overloaded"})
                .to_string(),
                NoteKind::Retry,
            ),
            (
                "a retry succeeding",
                serde_json::json!({"type":"auto_retry_end","success":true,"attempt":2}).to_string(),
                NoteKind::Retry,
            ),
            (
                "retries exhausted",
                serde_json::json!({"type":"auto_retry_end","success":false,"attempt":3,
                                   "finalError":"529 overloaded_error"})
                .to_string(),
                NoteKind::Error,
            ),
            (
                "a summarization retry",
                serde_json::json!({"type":"summarization_retry_scheduled","attempt":1,
                                   "maxAttempts":3,"errorMessage":"terminated"})
                .to_string(),
                NoteKind::Retry,
            ),
            (
                "an extension throwing",
                serde_json::json!({"type":"extension_error","extensionPath":"/x.ts",
                                   "event":"tool_call","error":"boom"})
                .to_string(),
                NoteKind::Error,
            ),
            (
                "a rejected command",
                serde_json::json!({"id":"p-0","type":"response","command":"prompt",
                                   "success":false,"error":"already streaming"})
                .to_string(),
                NoteKind::Error,
            ),
            (
                "a compaction that failed",
                serde_json::json!({"type":"compaction_end","reason":"threshold",
                                   "aborted":false,"result":Value::Null,
                                   "errorMessage":"quota exceeded"})
                .to_string(),
                NoteKind::Error,
            ),
            (
                "an extension displaying something",
                serde_json::json!({"type":"extension_ui_request","id":"u","method":"notify",
                                   "message":"blocked"})
                .to_string(),
                NoteKind::Ui,
            ),
        ];
        for (what, line, kind) in visible {
            let mut d = Decoder::new();
            let all = d.decode_line(line);
            let got: Vec<_> = events(&all)
                .into_iter()
                .filter_map(|e| match e {
                    HarnessEvent::Note { note, .. } => Some(note),
                    _ => None,
                })
                .collect();
            assert_eq!(got, vec![*kind], "{what} must reach a consumer: {all:?}");
        }

        // The other half. Each of these is a real message this build KNOWS — so
        // none is an `Unknown` either — and each stays in the pane log alone.
        let log_only = [
            serde_json::json!({"type":"agent_settled"}).to_string(),
            serde_json::json!({"type":"message_start","message":{}}).to_string(),
            serde_json::json!({"type":"message_end","message":{"role":"assistant"}}).to_string(),
            serde_json::json!({"type":"turn_end","toolResults":[]}).to_string(),
            serde_json::json!({"type":"tool_execution_start","toolCallId":"c","toolName":"bash"})
                .to_string(),
            serde_json::json!({"type":"bash_execution_update","id":"r","delta":"x"}).to_string(),
            serde_json::json!({"type":"compaction_start","reason":"threshold"}).to_string(),
            serde_json::json!({"type":"summarization_retry_finished"}).to_string(),
            serde_json::json!({"id":"p-0","type":"response","command":"prompt","success":true})
                .to_string(),
        ];
        for line in &log_only {
            let mut d = Decoder::new();
            let all = d.decode_line(line);
            assert!(
                events(&all).is_empty(),
                "bookkeeping must not reach a consumer: {line} -> {all:?}"
            );
            assert_eq!(notes(&all).len(), 1, "but it must be KEPT: {line} -> {all:?}");
            assert!(unknowns(&all).is_empty(), "{line} -> {all:?}");
        }
    }

    #[test]
    fn a_note_carries_the_open_turn_and_never_opens_one() {
        // A retry that begins before the first turn, or an extension that throws
        // at boot, would otherwise manufacture a turn nobody prompted and shift
        // every later `TurnId` by one — and §1.3 says a fact the pane does not
        // have is `None`, never a sentinel.
        let err = serde_json::json!({"type":"extension_error","extensionPath":"/x.ts",
                                     "event":"boot","error":"boom"})
        .to_string();
        let mut d = Decoder::new();
        let before = d.decode_line(&err);
        assert_eq!(
            events(&before),
            vec![HarnessEvent::Note {
                turn: None,
                note: NoteKind::Error,
                text: "extension /x.ts failed on boot: boom".into(),
            }],
            "no turn is open, so the turn is not known"
        );
        // The discriminating half: the very next turn is STILL turn 0, which is
        // what a note that opened one would have consumed.
        let opened = d.decode_line(&serde_json::json!({"type":"agent_start"}).to_string());
        assert_eq!(
            events(&opened),
            vec![HarnessEvent::TurnStarted { turn: TurnId(0) }]
        );
        // And inside a turn, the note is attributed to it.
        let during = d.decode_line(&err);
        assert!(
            matches!(
                events(&during).first(),
                Some(HarnessEvent::Note {
                    turn: Some(TurnId(0)),
                    ..
                })
            ),
            "{during:?}"
        );
    }

    // ── the pump ────────────────────────────────────────────────────────────

    #[test]
    fn pump_publishes_events_logs_the_rest_and_writes_the_commands_back() {
        let text = [
            serde_json::json!({"type":"agent_start"}).to_string(),
            r#"{"type":"not_a_thing"}"#.to_string(),
            serde_json::json!({"type":"agent_settled"}).to_string(),
            serde_json::json!({"type":"agent_end","willRetry":false}).to_string(),
        ]
        .join("\n");
        let (tx, rx) = mpsc::channel();
        let mut d = Decoder::new();
        let mut log = None;
        let mut sink = Some(Vec::<u8>::new());
        pump(
            std::io::BufReader::new(text.as_bytes()),
            &mut d,
            &mut log,
            &tx,
            &mut sink,
        );
        drop(tx);
        let got: Vec<_> = rx.iter().collect();
        assert_eq!(got, vec![HarnessEvent::TurnStarted { turn: TurnId(0) }]);
        // The command the decoder asked for really left the process.
        let written = String::from_utf8(sink.unwrap()).unwrap();
        assert_eq!(written, stats_line(0), "{written:?}");
    }

    #[test]
    fn pump_stops_when_the_receiver_is_gone() {
        // Continuing would read a whole session into a dropped channel. The
        // stream here is long enough that a pump which did not stop would write
        // the stats command at its end — so the sink is the discriminator, not
        // the absence of a panic.
        let mut text: Vec<String> =
            (0..50).map(|_| serde_json::json!({"type":"agent_start"}).to_string()).collect();
        text.push(serde_json::json!({"type":"agent_end","willRetry":false}).to_string());
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let mut d = Decoder::new();
        let mut log = None;
        let mut sink = Some(Vec::<u8>::new());
        pump(
            std::io::BufReader::new(text.join("\n").as_bytes()),
            &mut d,
            &mut log,
            &tx,
            &mut sink,
        );
        assert!(
            sink.unwrap().is_empty(),
            "the pump must stop at the first failed send, not read to the end"
        );
    }

    // ── the fixture ─────────────────────────────────────────────────────────

    #[test]
    fn the_fixture_decodes_to_one_whole_exchange() {
        // The end-to-end shape, over the awkward cases the fixture's README
        // enumerates. Every assertion below names a case that README lists, so a
        // fixture edit that drops one fails here rather than quietly narrowing
        // what this suite covers.
        let text = include_str!("../../tests/fixtures/harness/pi/one-turn.jsonl");
        let mut d = Decoder::new();
        let all = decode_all(&mut d, text);
        let evs = events(&all);

        // Boot, synthesized from the reply and not from a boot event.
        assert_eq!(
            evs.first(),
            Some(&HarnessEvent::Booted {
                session: Some("abc123".into()),
                model: Some("z-ai/glm-5.3-flash".into()),
                capabilities: CAPABILITIES.iter().map(|s| s.to_string()).collect(),
            })
        );

        // Exactly one turn, opened once and closed once.
        let starts = evs.iter().filter(|e| matches!(e, HarnessEvent::TurnStarted { .. })).count();
        let ends = evs.iter().filter(|e| matches!(e, HarnessEvent::TurnEnded { .. })).count();
        assert_eq!((starts, ends), (1, 1), "{evs:?}");

        // Reasoning is `Thinking` and never text.
        let thinking: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::Thinking { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, vec!["the user wants a listing"]);
        let text_deltas: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::Text { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_deltas, vec!["Listing\u{2028}now"], "U+2028 survived framing");

        // One tool call, from `toolcall_end` alone, with its arguments.
        let calls: Vec<_> = evs
            .iter()
            .filter(|e| matches!(e, HarnessEvent::ToolCall { .. }))
            .collect();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(
            calls[0],
            &HarnessEvent::ToolCall {
                turn: TurnId(0),
                id: ToolUseId("call_abc123".into()),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls -la"}),
            }
        );

        // The accumulation, subtracted back into deltas that reassemble it.
        let outs: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::ToolOutput { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            outs.concat(),
            "total 48\ndrwxr-xr-x .\nls: cannot access 'x'\n",
            "the deltas must reassemble the final accumulation exactly"
        );
        assert!(outs.len() > 1, "and they must really be several: {outs:?}");
        assert!(
            evs.contains(&HarnessEvent::ToolResult {
                turn: TurnId(0),
                id: ToolUseId("call_abc123".into()),
                ok: false,
            }),
            "the FAILING branch an all-success capture never reaches"
        );

        // Retries and the extension notify are NOTES a human sees, not log lines.
        let note_texts: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::Note { note, text, .. } => Some((*note, text.as_str())),
                _ => None,
            })
            .collect();
        assert!(
            note_texts
                .iter()
                .any(|(k, t)| *k == NoteKind::Retry && t.contains("529 overloaded_error")),
            "the retry a human would otherwise never see: {note_texts:?}"
        );
        assert!(
            note_texts
                .iter()
                .any(|(k, t)| *k == NoteKind::Ui && *t == "Command blocked by user"),
            "{note_texts:?}"
        );

        // The fire-and-forget `notify` sits directly before the `select` dialog,
        // and exactly one of the two is a question.
        let reqs: Vec<_> = evs
            .iter()
            .filter(|e| matches!(e, HarnessEvent::UiRequest { .. }))
            .collect();
        assert_eq!(reqs.len(), 1, "notify must not be a question: {reqs:?}");
        assert_eq!(
            reqs[0],
            &HarnessEvent::UiRequest {
                id: RequestId("uuid-1".into()),
                method: UiMethod::Select,
                title: Some("Allow dangerous command?".into()),
                message: None,
                options: vec!["Allow".into(), "Block".into()],
                timeout_ms: Some(10000),
            }
        );

        // The compaction, with the figure only the END event carries.
        assert!(
            evs.contains(&HarnessEvent::Compacted {
                trigger: CompactTrigger::Auto,
                pre_tokens: Some(150000),
            }),
            "{evs:?}"
        );

        // The queue.
        assert!(
            evs.contains(&HarnessEvent::QueueChanged {
                steering: vec![],
                follow_up: vec!["After that, summarize the result".into()],
            }),
            "{evs:?}"
        );

        // The unknown message was kept, not dropped.
        assert_eq!(unknowns(&all).len(), 1, "{:?}", unknowns(&all));
        assert!(unknowns(&all)[0].contains("something_pi_added_later"));

        // And the turn closed on the stats reply the decoder itself asked for.
        assert_eq!(commands(&all), vec![stats_line(0)]);
        let end = evs
            .iter()
            .find_map(|e| match e {
                HarnessEvent::TurnEnded { usage, cost, .. } => Some((usage.clone(), *cost)),
                _ => None,
            })
            .expect("a TurnEnded");
        let usage = end.0.expect("usage");
        assert_eq!(usage.call_cumulative.input, 50000);
        assert_eq!(usage.call_cumulative.cache_creation, 5000);
        assert_eq!(
            usage.this_turn_main_loop.expect("the turn figure").output,
            28,
            "summed from the ONE message_end, not from the eleven message_updates"
        );
        assert_eq!(end.1.expect("cost").usd, 0.45);
    }
}
