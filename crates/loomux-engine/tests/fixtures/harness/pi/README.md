# pi RPC-mode fixtures

**These lines are SYNTHETIC. They were not recorded from a `pi` process.**

Every field in `one-turn.jsonl` is built from the RPC protocol reference shipped
inside the pi package installed on the developer's machine —
`@earendil-works/pi-coding-agent` **0.85.1**, `docs/rpc.md` — plus that
package's own `dist/` for the two facts the prose does not state (that RPC mode
emits no boot event, and that `message_end` passes an `AgentMessage` straight
through). No agent CLI was run to produce them: CLAUDE.md constraint 3 forbids
it, and it would have spent the user's credits.

Upstream `main` is deliberately not a source. The driver has to match the build
the human actually runs, and a protocol read off a newer tree would be a claim
about a program nobody has.

## What that means for what they prove

They prove the decoder against the **documented** protocol of that build. They
cannot prove it against the real CLI's bytes — a field the docs describe
loosely, a key the docs name but spell differently in practice, or an event the
docs do not mention would pass here and fail live.

`doc/design/pi.md`'s "Live items this section does not settle" carries the
capture that replaces this file as item 4. Until then, treat a green decoder
suite as "matches the docs", not as "matches the CLI".

## Rules for editing

- **Agents never regenerate these from the decoder.** A fixture regenerated from
  the code under test asserts nothing — it is a snapshot of today's behaviour
  wearing an expectation's clothes.
- A capture that replaces this file records the **pi version** it came from, in
  this README, the way `doc/design/pi.md` labels its source-read evidence.
- Keep the deliberate awkward cases. `one-turn.jsonl` carries, on purpose:
  - a literal **`U+2028`** inside a `text_delta` string (line 9). This is the
    byte that separates a compliant reader from Node's `readline`, which
    `docs/rpc.md:37` singles out as non-compliant *because* it splits on it.
    `BufRead::lines` does not, and this line is the pin.
  - a `thinking_delta` (which must NOT become transcript text);
  - a `toolcall_start` **and** a `toolcall_delta` before the `toolcall_end` — the
    first has no arguments and the second is a partial-JSON chunk, so exactly one
    `ToolCall` may come out of the three;
  - two `tool_execution_update`s where the second **restates** the first and adds
    to it, because `partialResult` is accumulated rather than incremental
    (`docs/rpc.md:1055`) and the decoder owes the subtraction;
  - a **failing** tool (`isError: true` — the branch an all-success capture never
    reaches);
  - a `message_end` whose `usage` is the only per-turn figure that may be summed,
    beside eleven `message_update`s whose `usage` must not be;
  - a message type no build knows (which must be kept as evidence, not dropped);
  - an `extension_ui_request` with a **fire-and-forget** method (`notify`)
    directly before one with a **dialog** method (`select`), so a decoder that
    treated the envelope as the trigger produces two `UiRequest`s where one is
    correct;
  - a `compaction_start` **and** a `compaction_end`, because only the end event
    carries `tokensBefore`;
  - an `auto_retry_start`/`auto_retry_end` pair (real messages that are not pane
    events).
- **The CRLF tolerance is NOT pinned by this file**, and cannot be: the working
  tree is CRLF on Windows and LF on the Linux and macOS CI runners
  (`core.autocrlf=true`, CLAUDE.md), so what `include_str!` sees here differs by
  platform. `a_line_may_end_with_crlf_and_the_record_is_unchanged` pins it from
  an inline string instead, which is the same on all three.

## Files

| file | what it is |
| --- | --- |
| `one-turn.jsonl` | one complete exchange: boot reply → thinking → streamed text → tool call → streamed tool output → failing result → queue → usage → unknown message → notify + dialog → compaction → retry → `agent_end` → the stats reply that closes the turn |
