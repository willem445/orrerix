# The `provider-limit` attention reason

*#2811 S5a. The hold this feeds — `HeldReason::ProviderLimit`, one hold across
every affected review drive — is S5b and is not in this slice.*

## The state nothing could see

When the account behind a block's model runs out of budget, the CLI in that pane
prints the provider's refusal and stops. Everything loomux watches says the pane
is healthy: the process is alive, the session is intact, the exit code never
comes, output has gone quiet the way it goes quiet whenever an agent is
thinking, and no `report(...)` arrives because the agent never got a turn in
which to make one. The pane is, from every existing signal's point of view,
working.

It is not a rare state. This repo's own group hit it four times in two days
(`q-35`, `q-39`, `q-40`, `q-45` in `loomux-68435179`), each time stopping every
pane on one provider at once — on Sep 5, four review lanes together. What
noticed, eventually, was the review driver's sixty-minute lane-stall timeout,
once per affected drive, an hour after the fact, with a hold apiece.

So: read the pane's own text, which is where the answer has been sitting all
along.

## Where it runs, and why there

`OrchRegistry::attention_tick`, phase 2 — the per-agent pass that already reads
each pane's tail with no registry lock held, masks it against that pane's
delivery record, and asks `prompt_wait_detected` whether it looks like a prompt.
The provider read is the same subject asked a second question, off the same
masked string.

**Not a second scanner in the driver.** The plan is explicit about this and it
is the right call: one pane-text classifier, in the one place that owns pane
text. S5b's driver hold consumes the same
`loomux_engine::providerlimit::limit_in_tail` rather than growing a reader of
its own.

The table lives in the engine (`crates/loomux-engine/src/providerlimit.rs`),
beside `model.rs`, for the reason `model.rs`'s own header gives: it is data plus
`match` with no I/O, and its two consumers straddle the engine/`src-tauri` seam.
Plan-2504 filed S5a as "`src-tauri`"; putting the table there would have pointed
S5b's engine-side decision back into `src-tauri`, which is the one arrow the
extraction exists to prevent.

## Sharing the mask is the point, not a convenience

The false positive that matters is a pane *talking about* a provider limit, and
the pane most likely to do that is the orchestrator's — it types the refusal
into `ask_human` while asking the human to top the account up, and it relays it
into delegates' panes to explain why they are being held. A worker's kickoff
brief can quote one too (this slice's own brief did).

`mask_loomux_notices_with_record` already removes everything loomux itself
delivered into a pane, keyed by session so a resumed pane inherits its own
history. Reading the limit off the masked string gets that for free — the same
self-latch `#576` fixed for the `waiting` chip, arriving at a third consumer.

## Two things about the real text that a hand-typed specimen loses

Both were found by pulling the actual pane tails out of the audit log rather
than retyping the strings from the incident write-ups, and either one alone is
enough to make a plausible-looking detector see nothing.

**It wraps, mid-word, behind a redrawn gutter.** pi renders the OpenRouter
refusal as

```text
  ┃  This request would exceed your available credits given your current in-
  ┃  flight requests. Retry after in-flight requests settle, or add credits.
```

A `tail.contains(needle)` over any needle longer than that first line returns
`false` here — and returns `true` against the unwrapped copy a test would paste
in, so the guard reads green while being blind on exactly the subject. This is
CLAUDE.md's line-oriented-sweep blind spot with a terminal instead of a `grep`.
`limit_in_tail` therefore strips each line's indent and gutter glyph, drops the
blank gutter rows, and tests each needle against a window of up to three
consecutive stripped lines rejoined — with no separator where the break fell on
a hyphen, so `in-` + `flight` rebuilds as `in-flight` and not `in- flight`.

**Claude's message ends in a curly apostrophe.** The pane prints
`/usage-credits to finish what you`U+2019`re working on.`; every prose account
of it, including this slice's brief, writes the ASCII `'`. A needle carrying the
straight quote matches the paraphrase and never the pane. The needle stops at
`what you`, short of the character the two spellings disagree on.

## Why a match must be LINE-INITIAL

Every captured refusal begins its own rendered line, after the indent and gutter
the strip removes. A quotation of one normally sits mid-sentence, behind a
`got "` or a `stopped at "`. That is a discriminating axis that does not depend
on any pane's name, role or CLI, so the match is anchored to the start of a
reassembled line.

**The residual is pinned, not implied.** The anchor cannot separate a pane's
refusal from prose that *opens* with the same words —
`a_quotation_of_a_refusal_is_not_a_refusal` asserts that blind spot directly, so
the disclosure cannot go quietly false. What bounds it is the mask above (the
orchestrator's relayed text is removed before the match) and the dedup below (a
false positive raises one chip, not one per pane).

## One chip per (group, provider)

A provider limit stops every pane on that provider simultaneously. Four
identical red chips and four toasts say one thing four times and need one remedy
once, and dismissing three of them is busywork the human did not earn. So the
item is raised once per `(group, provider)`, on the lowest-sorting affected
agent id — deterministic, where the roster's own iteration order is a
`HashMap`'s and is not — and its `detail` carries how many panes were stopped,
so the one chip still tells the truth about the blast radius.

**Keyed on the provider found in the TEXT, not on the block's model prefix.**
This is the one place S5a deviates from plan-2504 §3 S5a, which says "keyed on
the block's model prefix". pi and opencode surface OpenRouter's refusal
verbatim, so a pane whose model reads `opencode/…` is stopped by *OpenRouter's*
limit; keying on the prefix would file it under a third "provider" with no
remedy of its own, and — worse — would fail to merge it with the OpenRouter
panes it must be counted with, producing exactly the several-chips-for-one-cause
outcome the rule exists to prevent. The brief that carried the plan names the
pi/opencode passthrough itself, so the two halves of the spec disagree; the
half kept is the one the brief argues for. Model-prefix resolution still has a
job in S5b — deciding which *drives* run on a limited provider is a question
about a roster, not about a tail — and lands there.

## Ranking

Under `blocked`, over `stranded`:

| | reason | cleared by |
| --- | --- | --- |
| 1 | `held-dialog` | the human answering the dialog |
| 2 | `blocked` | the agent said so itself — a report, not a diagnosis |
| 3 | `provider-limit` | **nothing inside the terminal** |
| 4 | `stranded` | one Enter in that pane |
| 5 | `waiting` | the human answering the prompt |

`blocked` stays above it because an agent's own statement outranks a diagnosis
loomux made from pane text. `stranded` goes below it because a stranded prompt
is a wedge a keystroke clears, and this one is not: the remedy is billing, or a
different `model:` in the workflow file, which is why the `detail` always
carries it. It is also the only reason here whose blast radius is the whole
group.

The frontend mirrors this in three places, all of which a test now pins:
`attention.ts`'s `LABELS`/`URGENT` (urgent — red, not amber),
`tabroute.ts`'s `REASON_PRIORITY` (4.5, slotted without renumbering anything, on
the rule that comment already states), and `agents-tab.md`'s rung 4.

## Extending it

One row on `LIMIT_PATTERNS`, never a branch in the scan — **and only with a
captured fixture behind it.** Each row names the file under
`src-tauri/tests/fixtures/attention/` it was cut from, and
`every_pattern_is_exercised_by_its_own_captured_fixture` refuses a row whose
needle does not appear *line-initially* in that capture.

That rule is stronger than the one this slice first shipped, and the difference
was found by running a mutation rather than by reading anything. The first
revision carried two extra needles taken on report — `Claude usage limit
reached` and `Your credit balance is too low` — with the provenance honestly
labelled in a `PatternSource` field, on the reasoning that a reader owed the
distinction could then weigh it. The label was not enough. `Claude usage limit
reached` is an ordinary English sentence opener, and the orchestrator's own
`ask_human` text about a provider limit begins with those exact words — this
repo's `q-39`, which was sitting in the tree as the *negative control fixture*.
The line-initial anchor is the whole of what separates a refusal from a
quotation of one, and it cannot separate anything from prose that opens with
the needle: that row made the orchestrator's pane badge itself for talking
about a limit, and S5b would have turned the same reading into a spurious drive
hold across every drive in the group.

The three surviving needles do not have that shape — prose quoting them puts
them behind a `got "` or a `stopped at "`, and one opens with a slash-command.
That is not luck. It is the difference between a string a real pane was
observed printing, line-initially, and a string that sounds like what a
provider would say; only the capture tells you which side of the anchor the
words fall on. Hence the rule is structural now rather than advisory, and
`PatternSource` is gone: there is only one class left, so a field naming it was
dead vocabulary.

Nothing here is machine- or repo-specific (CLAUDE.md constraint 8): these are
the vendors' own error strings, and the table is the product's knowledge of its
providers, not this checkout's.
