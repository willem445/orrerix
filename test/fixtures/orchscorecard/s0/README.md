# `s0` — the corpus for the driver S0 counters (plan-2504 §3 S0, board t-769)

Real rows, copied verbatim. Every line of `audit.jsonl` is an **unmodified** audit
row lifted from this group's live audit log
(`C:/Users/WillH/AppData/Roaming/orrerix/orchestration/loomux-68435179/audit.1.jsonl`
+ `audit.jsonl`), restricted to the beta9 session the plan-2504 §1 control was
hand-tallied on (`on_behalf_of: orch-2385`, ts window
`[1788706648042, 1788729593887]`, 2026-09-06 14:57Z–21:19Z). Ids, heads, sessions
and texts are unredacted — they are already ids on a log this repo's own tooling
reads; nothing private enters a fixture, because the rows are the same shape the
app writes for every group.

`agents.json` carries the real `orch-2385` store entry and nothing else — the
prompt-class counters need to know which panes are orchestrators, and the kill
attribution needs no other agent record. `usage.json` is empty: the S0 counters
read the audit, not the usage store.

The rows are selected so every new counter has a witness with its own value, and
no two counters share one:

| Counter | Witness rows |
| --- | --- |
| `rd-refused` cap + `starved_ms` max/sum | #2941: four `cap:true` refusals, starved 0 / 315733 / 631091 / **689089 (the §1 max)**; #2942: two cap rows, starved 313290 / 629023; #3061: one **non-cap** (`already-driven`) refusal, so the cap count is not the refusal count |
| lane scope histogram, deduped on (pr, block, round) | **three replaced panes**, each two `rd-lane-spawned` rows on one triple: #2942 `rev-std` r4 (body-only → whole-diff), #2946 `rev-std` r1 (whole-diff ×2), #3061 `rev-std` r1 (whole-diff → delta); plus #2941 (two distinct triples, whole-diff) and #2945 (one delta triple) |
| `rd-handback` vs `rd-worker-released` ratio | #2942 and #2945: one hand-back AND one worker release each (ratio 1); #2941: one hand-back, no release (ratio `null` — the denominator is 0, and a null says "nothing was released" where a 0 would claim "no hand-backs") |
| `agent-kill` by initiator | driver-release of lanes (rev-2416, rev-2468, rev-2410) and of hand-back-named workers (w-2436, w-2426); orchestrator of a worker the hand-back row attributes (w-2476 → #2941); orchestrator of two agents the fixture attributes to **nothing** (w-2388 has no spawn row here; w-2391's real spawn brief names #2011, which is not in the selection) |
| orchestrator prompt classes | #2941: GATE SATISFIED ×2, HELD ×1, a delegate report; #2942/#2946: CANCELLED; plus a `run … completed` check row and a human-typed row — both to the orchestrator pane, neither naming a selected PR |

The replaced-pane rows are the point of the dedup rule: the same triple twice with
**different scopes** (the #2942 and #3061 pairs) pins that a replaced pane counts
once, under the **first** row's scope.

The hand-back/release pairing on #2942 and #2945 is finding (b) of plan-2504 §1
made concrete: w-2436 and w-2426 were released only because a body-only / pushed
report was consumed, while #2941's w-2476 was never released and the orchestrator
killed the pane by hand 12 s after the second GATE SATISFIED.
