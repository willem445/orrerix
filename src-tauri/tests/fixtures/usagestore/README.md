# `usage.json` as older builds wrote it

Fixtures for `tests/orchestration/usagestore.rs` (#3677): the file the usage
store has to keep loading, in the two shapes that exist on disk.

| file | shape | rows |
| --- | --- | --- |
| `usage-v1.3.1-beta7.json` | v1.3.1-beta7: every row carries `current_model` and `activity` | 7 |
| `usage-v1.3.0.json` | v1.3.0: neither field exists | 7 |

**Where the rows come from.** `usage-v1.3.1-beta7.json` is seven rows cut as
text out of a `usage.json` that v1.3.1-beta7 wrote, one per `source` the store
held (`transcript` with and without a block, `none`, `statusline`,
`session-db`, `pi-transcript`, `codex-transcript`), in the layout
`serde_json::to_string_pretty` gave them. One thing is changed: each row's
`key`, a CLI session id, is replaced with a placeholder. Every other byte is
what the app wrote, including the number formatting (`0.0`,
`1.4326490000000005`) and the model string opencode reports as JSON.

`usage-v1.3.0.json` is the same seven rows with the `current_model` line and
the `activity` object removed, which is the field set v1.3.0's `UsageSnapshot`
had. It is derived rather than cut: every row in a real store has since been
rewritten by a newer build, because until #3677 each tick rewrote the whole
file, so no row in the v1.3.0 shape survives on disk to copy.

**The figures the tests assert** are summed off these files by a separate
script, not by the code under test: 10,528,902 tokens across the four counters
and 2.58092079 dollars across the seven rows.

Both files are read with `include_str!`, so a checkout that converts line
endings changes the embedded whitespace and nothing a JSON parser sees.

`tests/orchestration/promptcost.rs` (#3831) reads `usage-v1.3.1-beta7.json` too.
Its seventeen-field row is still the shape v1.3.1-beta8 wrote, so it is the file
from before `first_context_tokens` and `detected_cache_ttl_minutes` existed.
