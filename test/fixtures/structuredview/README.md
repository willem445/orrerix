# `structuredview` fixtures

`session.harness.jsonl` is one line per `HarnessEvent` **as it serializes** —
`loomux_engine::harness::HarnessEvent` is `#[serde(tag = "kind", rename_all =
"snake_case")]` with transparent newtype ids, so a line is
`{"kind":"tool_call","turn":1,"id":"t1",…}`. **One** line is the single local
input `structuredview.ts` documents as deliberately NOT a `HarnessEvent`:
`delivery`, which is orrerix narrating its own action rather than anything a
harness reported. The `note` line is not local — `Note` is a `HarnessEvent`
variant (#2850 S1b), and this sentence said otherwise until review round 1
caught the twin.

**It is synthesized, and it will be swapped.** No pi process has run here
(`CLAUDE.md` constraint 3 forbids spawning a real agent CLI to produce one),
and S1b's `crates/loomux-engine/tests/fixtures/harness/pi/*.jsonl` are pi's own
WIRE shapes, which need `pi.rs`'s decoder to become the events this module
consumes — a decoder this module must not grow a second copy of. So this file
is written directly in the consumed vocabulary against
`doc/design/harness-adapters.md` §1.2 as the S1a branch amends it. When S1b
lands, the honest replacement is a capture of what `pi.rs` really emits over
those wire fixtures, and the swap is a fixture edit with no change to
`structuredview.ts`.

The demo's `demo/structured-pane/fixtures/*.jsonl` (#2891 S0) are the same wire
shapes for the same reason, and `demo/structured-pane/decode.js` is the mock's
throwaway stand-in for `pi.rs`. Neither is imported here.

**Line endings.** No `.gitattributes` row covers this path, so it is CRLF on
disk under this project's `core.autocrlf=true` baseline and LF in the blob. The
reader in `test/structuredview.test.ts` splits on `\n` and strips a trailing
`\r`, which is exactly the framing rule §1's pi row states ("Split on `\n`
only … Strip optional trailing `\r`") — so the checkout artefact is load-bearing
here rather than incidental.
