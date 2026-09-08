// Unit tests for the pure attention-routing presentation mapping shared by the
// pane header chip and the minimize-dock chip. Run with `npm test`.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import {
  attentionPresentation,
  dockChipAttention,
  attentionDismiss,
  attentionChanged,
  DECISION_REASONS,
  REPORT_REASONS,
  KNOWN_ATTENTION_REASONS,
} from "../src/attention.ts";

test("each known reason maps to its label", () => {
  // #946 Q4 / #1091 slice H: the latched-attention belt's reason.
  assert.equal(attentionPresentation("held-dialog").label, "⛔ held on a dialog");
  assert.equal(attentionPresentation("blocked").label, "⚠ blocked");
  assert.equal(attentionPresentation("stranded").label, "⚠ stuck prompt");
  // #2811 S5a: the account behind the pane's model is out of budget.
  assert.equal(attentionPresentation("provider-limit").label, "⛔ provider limit");
  assert.equal(attentionPresentation("waiting").label, "⚠ waiting");
  assert.equal(attentionPresentation("report").label, "✓ reported");
  // #1091 slice D: a pending `ask_human` row on this pane's own asker.
  assert.equal(attentionPresentation("question").label, "❓ question");
  assert.equal(attentionPresentation("gate").label, "⚑ your call");
});

/** Every reason the backend attention scan emits — the mirror of
 *  `attention_tick`'s reason chain (src-tauri/src/orchestration/mod.rs), in
 *  chain order. Shared by the two completeness tests below. */
const BACKEND_REASONS = [
  "held-dialog",
  "blocked",
  "provider-limit",
  "dialog",
  "stranded",
  "waiting",
  "report",
  "question",
  "gate",
];

test("every reason the backend attention scan emits has a table row", () => {
  // `KNOWN_ATTENTION_REASONS` is read off `LABELS`, so it cannot catch a reason
  // the backend emits that this table never added: #2850 S3b added `dialog`
  // backend-side and no row landed here (#3190), so those panes' chips fell
  // through `attentionPresentation` to the generic "⚠ attention" default.
  // Enumerate the backend's reasons and refuse that fallback for each.
  for (const reason of BACKEND_REASONS) {
    assert.notEqual(
      attentionPresentation(reason).label,
      "⚠ attention",
      `${reason} has no row in LABELS — it renders the generic fallback`,
    );
  }
  // Lockstep, both directions: a reason in LABELS but missing from the list
  // above is enumeration drift, and a list longer than LABELS is a backend
  // reason the table still has not added.
  for (const known of KNOWN_ATTENTION_REASONS) {
    assert.ok(
      BACKEND_REASONS.includes(known),
      `${known} is in LABELS but not in this test's backend enumeration — update the list`,
    );
  }
  assert.ok(
    KNOWN_ATTENTION_REASONS.length >= 9,
    `only ${KNOWN_ATTENTION_REASONS.length} reasons in LABELS`,
  );
});

test("the test's backend list is read off attention_tick's own chain", () => {
  // #3190 rev-std finding 3 (non-blocking, taken): BACKEND_REASONS above is a
  // hand-maintained copy, so it enforces LABELS↔list lockstep but not
  // backend↔list — the next reason `attention_tick` emits would miss both and
  // every table stays green, the exact #3190 class one hop later. So the chain
  // is read off the source, the repo's established idiom
  // (test/transport.test.ts scans `src/`; CLAUDE.md's source-scanning guard
  // convention): every reason literal is a tuple whose FIRST element is a
  // string literal — `("held-dialog", format!(…))` — and a scan for that shape
  // over `attention_tick`'s region extracts exactly the chain, no noise.
  //
  // Scan limits, stated: a reason raised OUTSIDE `attention_tick` (today only
  // the plain-pane path, which emits just `waiting`) is not seen; a
  // commented-out arm IS still seen and fails loud rather than silently
  // shrinking the population. Either way this test fails naming the reason,
  // and the fix is the table row, not the assertion.
  const MOD = fileURLToPath(
    new URL("../src-tauri/src/orchestration/mod.rs", import.meta.url),
  );
  const source = readFileSync(MOD, "utf8");
  const start = source.indexOf("pub fn attention_tick(");
  const end = source.indexOf("pub fn plain_pane_attention(", start);
  assert.ok(start >= 0, "attention_tick not found in orchestration/mod.rs");
  assert.ok(end > start, "the function bound after attention_tick vanished");
  const region = source.slice(start, end);
  const scanned = new Set<string>();
  for (const m of region.matchAll(/\(\s*"([a-z][a-z-]+)"\s*,/g)) {
    scanned.add(m[1]);
  }
  const unknown = [...scanned].filter((r) => !BACKEND_REASONS.includes(r));
  assert.deepEqual(
    unknown,
    [],
    `attention_tick emits ${unknown.join(", ")} and no table row covers it — ` +
      "add the row to attention.ts (LABELS/URGENT), tabroute.ts, and BACKEND_REASONS above",
  );
  // Non-vacuity: the scan must have SEEN the chain, not a stub of it — and
  // must include the two reasons that historically slipped through.
  assert.ok(scanned.size >= 9, `only ${scanned.size} chain reasons scanned`);
  for (const required of ["dialog", "provider-limit"]) {
    assert.ok(
      scanned.has(required),
      `attention_tick's chain no longer emits ${required} — re-pin this test's scan`,
    );
  }
});

test("'held-dialog', 'blocked', 'provider-limit', 'dialog' and 'stranded' are the urgent reasons", () => {
  // #946 Q4 / #1091 slice H: a blocking dialog holding the orchestrator's own
  // delivery pipe strands every OTHER agent's report behind it too — at
  // least as urgent as a plain `blocked` report, never merely amber.
  assert.equal(attentionPresentation("held-dialog").urgent, true);
  assert.equal(attentionPresentation("blocked").urgent, true);
  // #496 PR-C: a prompt that was delivered but never submitted wedges the
  // pane until an Enter lands — red, not the amber of a pane that is merely
  // parked on a question it is happy to keep asking.
  assert.equal(attentionPresentation("stranded").urgent, true);
  // #2811 S5a: red, and for a reason stronger than `stranded`'s — an Enter in
  // the pane clears a stranded prompt, and nothing typed into a
  // provider-limited pane clears that at all.
  assert.equal(attentionPresentation("provider-limit").urgent, true);
  // #2850 S3b: a structured pane parked on an extension-UI dialog. Ranked with
  // `stranded` backend-side — pi waits on stdin indefinitely, so the pane will
  // not un-wedge itself, and every delivery behind it waits too.
  assert.equal(attentionPresentation("dialog").urgent, true);
  for (const reason of ["waiting", "report", "question", "gate"]) {
    assert.equal(attentionPresentation(reason).urgent, false, `${reason} not urgent`);
  }
});

test("a stranded pane's dock chip is red and keeps the backend's instruction", () => {
  // #496 PR-C: minimizing a wedged pane must not hide it — the dock chip is
  // the only surface left, and its tooltip carries the badge detail verbatim
  // (which names what the human has to clear).
  const stranded = attentionPresentation("stranded");
  const chip = dockChipAttention("orch", {
    label: stranded.label,
    urgent: stranded.urgent,
    detail: "orch's prompt is stuck behind text you typed — press Enter or clear the box",
  });
  assert.equal(chip.needsAttention, true);
  assert.equal(chip.urgent, true, "a wedged pane is red on the dock too");
  assert.match(chip.title, /stuck prompt/);
  assert.match(chip.title, /press Enter or clear the box/);
});

test("an unknown reason falls back to a generic, non-urgent badge", () => {
  const p = attentionPresentation("some-future-reason");
  assert.equal(p.label, "⚠ attention");
  assert.equal(p.urgent, false);
});

// The dock-dot path (#40): once detection sets an attention reason, a minimized
// pane's dock chip must mirror it — the dot is how attention survives docking.
test("a docked pane with attention shows the dot and mirrors urgency", () => {
  // An agent parked on an interactive question surfaces as reason "waiting".
  const waiting = attentionPresentation("waiting");
  const chip = dockChipAttention("copilot", {
    label: waiting.label,
    urgent: waiting.urgent,
    detail: "copilot is waiting on a prompt",
  });
  assert.equal(chip.needsAttention, true, "waiting must light the dock dot");
  assert.equal(chip.urgent, false, "waiting is amber, not urgent red");
  assert.match(chip.title, /waiting/);
  assert.match(chip.title, /restore copilot/);

  // A blocked report is the urgent (red) variant.
  const blocked = attentionPresentation("blocked");
  const urgentChip = dockChipAttention("w", {
    label: blocked.label,
    urgent: blocked.urgent,
    detail: null,
  });
  assert.equal(urgentChip.needsAttention, true);
  assert.equal(urgentChip.urgent, true);
  assert.match(urgentChip.title, /needs you/);
});

test("a docked pane with no attention shows no dot, only a restore hint", () => {
  const chip = dockChipAttention("editor", null);
  assert.equal(chip.needsAttention, false);
  assert.equal(chip.urgent, false);
  assert.equal(chip.title, "Restore editor");
});

// #825 M1: the explicit dismiss. `stranded` is the one LATCHED reason — it
// stays up until something removes it backend-side, and for several blocker
// classes nothing ever does — so it is the one that needs a gesture of its own.
test("the latched stranded chip is the one that offers an explicit dismiss", () => {
  const d = attentionDismiss("stranded", "w-3");
  assert.equal(d.dismissible, true);
  assert.notEqual(d.label, "", "a dismissible chip needs something to click");
});

test("the live-recomputed reasons offer no dismiss control", () => {
  // These are re-derived by every 3-second attention scan (waiting/gate) or
  // already released by the focus ack (report/blocked). A dismiss control on
  // them would be a button that visibly does nothing — the chip is back on the
  // next tick — which teaches the human that dismissing does not work, the
  // exact complaint #825 exists to fix.
  for (const reason of ["waiting", "report", "question", "gate", "blocked"]) {
    assert.equal(
      attentionDismiss(reason, "w-3").dismissible,
      false,
      `${reason} is not dismissible`,
    );
  }
  assert.equal(attentionDismiss(null, "w-3").dismissible, false, "no chip, nothing to dismiss");
});

test("a stranded chip with no agent identity offers no dismiss", () => {
  // The backend releases the badge by agent id (`orch_dismiss_stranded`), so a
  // plain pane — which has no orchestration identity — has nothing to send.
  // Offering the control anyway would be a click that silently fails.
  assert.equal(attentionDismiss("stranded", null).dismissible, false);
  assert.equal(attentionDismiss("stranded", "").dismissible, false, "an empty id is no id");
});

// #946 Q4 / #1091 slice H: `held-dialog` is ALSO latched (backend-side, until
// the hold itself clears — see `attn_question_held`'s Rust doc), same as
// `stranded`, and still deliberately gets no dismiss control. Unlike
// `stranded`, nothing can leave it up forever: the hold that raised it is
// bounded (`QUESTION_HOLD_MAX`) and clears it unconditionally when it ends,
// so a human-facing "take it down early" gesture would race a backend clear
// that is already coming — the button `attentionDismiss`'s own doc warns
// against, one whose effect would sometimes evaporate on the very next tick.
test("the held-dialog chip offers no dismiss, unlike stranded", () => {
  assert.equal(attentionDismiss("held-dialog", "orch-1").dismissible, false);
});

test("the dismiss tooltip promises only what the dismiss actually does", () => {
  // It takes the CHIP down; it does not unstick the pane. A tooltip that
  // implied otherwise would be the false claim this whole issue is about —
  // a human who reads "resolve" and walks away from a genuinely wedged pane.
  //
  // The disclaimer is pinned as a phrase rather than a vibe because it IS the
  // guarantee: a chip the human can take down on their own say-so is only
  // honest while the control says what it settles and what it leaves alone.
  const { title } = attentionDismiss("stranded", "w-3");
  assert.match(title, /dismiss/i);
  assert.match(
    title,
    /does not unstick the pane/i,
    `the tooltip must say what it does NOT do: ${title}`,
  );
});

// #1091 slice D review: `Pane.setAttention` used to be idempotent on `reason`
// ALONE, which meant a pending-question count going from 1 to 2 — same
// `reason: "question"`, different `detail` — never reached the chip's
// tooltip: the docs claimed "hover it for the question count" while the code
// silently kept showing whatever count first raised the badge. Same defect
// shape for `gate` (detail carries the task's status, which can change
// without the task leaving the gate-status set). `attentionChanged` is the
// extracted, pure identity check `setAttention` now gates on — this pins
// that a detail-only change is still a change.
test("attentionChanged fires on a detail change even when the reason stays the same", () => {
  assert.equal(
    attentionChanged("question", "1 pending question — needs your answer", "question", "1 pending question — needs your answer"),
    false,
    "an identical repeat is a no-op",
  );
  assert.equal(
    attentionChanged("question", "1 pending question — needs your answer", "question", "2 pending questions — needs your answer"),
    true,
    "a growing count must not be swallowed by same-reason idempotency",
  );
  assert.equal(
    attentionChanged("gate", "task is pr — awaiting your call", "gate", "task is human-testing — awaiting your call"),
    true,
    "gate's status text is the same live-detail shape as question's count",
  );
});

test("attentionChanged treats a fresh reason and a clear as changes too", () => {
  assert.equal(attentionChanged(null, null, "question", "1 pending question — needs your answer"), true);
  assert.equal(attentionChanged("question", "1 pending question — needs your answer", null, null), true);
  assert.equal(attentionChanged(null, null, null, null), false, "clear-on-clear is still a no-op");
});

test("every known attention reason is classified exactly once", () => {
  // #2122 slice A / #2195 review, rev-std finding 2. Four independent classes
  // consume this module: URGENT (via `attentionPresentation().urgent`, the red
  // chips), DECISION_REASONS (a call waiting on the human's own pace),
  // REPORT_REASONS (#2367 — the agent called `report(...)` and is waiting on
  // the ORCHESTRATOR, not on a human decision; before #2367 `report` sat in
  // DECISION_REASONS and the Agents tab read it as `question`), and
  // `waiting` — which is non-urgent and emphatically NOT a decision: it is a
  // finished turn, read on `agentrows.ts`'s own `turn-done` rung.
  //
  // The partition is what is pinned, not any one set. A reason added to LABELS
  // without being classified renders a chip and then falls through every rung
  // that matters: no red badge, no `question` row, no `needsYouCount` — the
  // quietest possible wrong answer. This fails instead, naming the reason.
  const unclassified: string[] = [];
  const doubled: string[] = [];
  for (const reason of KNOWN_ATTENTION_REASONS) {
    const classes = [
      attentionPresentation(reason).urgent && "urgent",
      DECISION_REASONS.has(reason) && "decision",
      REPORT_REASONS.has(reason) && "report",
      reason === "waiting" && "waiting",
    ].filter(Boolean);
    if (classes.length === 0) unclassified.push(reason);
    if (classes.length > 1) doubled.push(`${reason} (${classes.join(" + ")})`);
  }
  assert.deepEqual(unclassified, [], "reason(s) in LABELS that no consumer class claims");
  assert.deepEqual(doubled, [], "reason(s) claimed by more than one class");
  // Non-vacuity: the loop must have SEEN the population, and seen every class
  // in it. An empty or one-class LABELS would satisfy both assertions above.
  assert.ok(KNOWN_ATTENTION_REASONS.length >= 8, `only ${KNOWN_ATTENTION_REASONS.length} reasons scanned`);
  assert.ok(
    KNOWN_ATTENTION_REASONS.includes("provider-limit"),
    "#2811 S5a's reason must be in the scanned population, not merely labelled",
  );
  assert.ok(KNOWN_ATTENTION_REASONS.some((r) => attentionPresentation(r).urgent));
  assert.ok(KNOWN_ATTENTION_REASONS.some((r) => DECISION_REASONS.has(r)));
  assert.ok(KNOWN_ATTENTION_REASONS.some((r) => REPORT_REASONS.has(r)));
  assert.ok(KNOWN_ATTENTION_REASONS.includes("waiting"));
});

test("the classification guard is not vacuous — an unclassified reason is visible to it", () => {
  // The positive control for the loop above: a reason NOT in any class really
  // does read as unclassified, so the empty list up there is the guard working
  // rather than the guard looking at nothing.
  const invented = "brand-new-reason-nobody-classified";
  assert.equal(KNOWN_ATTENTION_REASONS.includes(invented), false);
  assert.equal(attentionPresentation(invented).urgent, false);
  assert.equal(DECISION_REASONS.has(invented), false);
  assert.notEqual(invented, "waiting");
});
