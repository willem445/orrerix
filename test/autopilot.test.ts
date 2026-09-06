// Unit tests for the single-pane autopilot toggle's persisted default-ON
// semantics (#101), the standalone channel-tools toggle (#271 W3
// addendum / PR #289 review round 2, N1) which shares the identical
// default-ON/explicit-"0"-off shape, and the orrerix-subagents toggle (#2519
// C1) whose polarity is the inverse of both — default OFF. Run with `npm
// test`. The pure `*FromStored` functions are tested directly so the default
// rules need no localStorage shim; `getSubagents`/`setSubagents`' own
// try/catch contract is tested over a throwing shim, because a read/write
// that raises must degrade to OFF / a silent no-op, not crash the launcher.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  autopilotFromStored,
  channelToolsFromStored,
  subagentsFromStored,
  getSubagents,
  setSubagents,
  subagentsToggleState,
  subagentsLaunchDecision,
  leadLaunchCount,
} from "../src/agents.ts";

test("autopilot defaults ON when nothing is stored", () => {
  // A brand-new user (no key yet) launches with autopilot on.
  assert.equal(autopilotFromStored(null), true);
});

test('only an explicit "0" turns autopilot off', () => {
  assert.equal(autopilotFromStored("0"), false);
});

test('a stored "1" keeps autopilot on', () => {
  assert.equal(autopilotFromStored("1"), true);
});

test("an empty or unrecognized value stays ON (fail-safe to the default)", () => {
  // A corrupted value must not silently disable autopilot — default wins.
  assert.equal(autopilotFromStored(""), true);
  assert.equal(autopilotFromStored("yes"), true);
  assert.equal(autopilotFromStored("false"), true);
});

test("channel tools default ON when nothing is stored", () => {
  // A brand-new user (no key yet) launches claude/copilot agent panes with
  // eager solo-prepare on — the addendum's stated "full membership at spawn"
  // default (#271 W3, N1 fix).
  assert.equal(channelToolsFromStored(null), true);
});

test('only an explicit "0" turns channel tools off', () => {
  assert.equal(channelToolsFromStored("0"), false);
});

test('a stored "1" keeps channel tools on', () => {
  assert.equal(channelToolsFromStored("1"), true);
});

test("an empty or unrecognized channel-tools value stays ON (fail-safe to the default)", () => {
  assert.equal(channelToolsFromStored(""), true);
  assert.equal(channelToolsFromStored("yes"), true);
  assert.equal(channelToolsFromStored("false"), true);
});

// ---------- the orrerix-subagents toggle (#2519 C1) ----------
// The polarity is deliberately the INVERSE of the two toggles above. Autopilot
// and channel tools default ON because doing nothing should not silently
// downgrade a feature the user already has; spawning a fleet of worker panes
// is the opposite kind of gesture — it mints real groups and real processes —
// so an absent, stale, or corrupted value must read OFF, and only an explicit
// "1" (what `setSubagents(true)` writes) turns it on.

test("orrerix subagents default OFF when nothing is stored", () => {
  // A brand-new user (no key yet) must not find fleet-spawning enabled.
  assert.equal(subagentsFromStored(null), false);
});

test('a stored "1" turns orrerix subagents on', () => {
  assert.equal(subagentsFromStored("1"), true);
});

test('only the exact "1" reads on — "0" and every garbage value read off', () => {
  assert.equal(subagentsFromStored("0"), false);
  assert.equal(subagentsFromStored(""), false);
  assert.equal(subagentsFromStored("yes"), false);
  assert.equal(subagentsFromStored("false"), false);
  assert.equal(subagentsFromStored("on"), false);
  assert.equal(subagentsFromStored(" 1"), false);
});

/** Swap in a localStorage shim and restore the previous global afterwards, so
 *  a throwing test cannot poison the suite's own storage (the lock_safe rule:
 *  a global overridden by a harness is restored from a cleanup, not leaked). */
function withStorage(
  shim: { getItem(key: string): string | null; setItem(key: string, value: string): void },
  fn: () => void,
): void {
  const g = globalThis as { localStorage?: unknown };
  const prev = g.localStorage;
  g.localStorage = shim;
  try {
    fn();
  } finally {
    g.localStorage = prev;
  }
}

test("getSubagents/setSubagents persist through the one key and round-trip", () => {
  const store = new Map<string, string>();
  const seenKeys: string[] = [];
  withStorage(
    {
      getItem: (k) => {
        seenKeys.push(k);
        return store.get(k) ?? null;
      },
      setItem: (k, v) => {
        store.set(k, v);
        seenKeys.push(k);
      },
    },
    () => {
      assert.equal(getSubagents(), false, "an empty store reads off");
      setSubagents(true);
      assert.equal(getSubagents(), true);
      setSubagents(false);
      assert.equal(getSubagents(), false);
      // ONE key: the toggle must not scatter state across the profile.
      assert.deepEqual([...new Set(seenKeys)], ["loomux.orrerixSubagents"]);
      assert.equal(store.get("loomux.orrerixSubagents"), "0", "OFF is written as an explicit 0");
    },
  );
});

test("a throwing read degrades to OFF; a throwing write is swallowed", () => {
  withStorage(
    {
      getItem: () => {
        throw new Error("quota / security / no storage");
      },
      setItem: () => {
        throw new Error("quota / security / no storage");
      },
    },
    () => {
      assert.equal(getSubagents(), false, "a refused read reads off, never throws");
      assert.doesNotThrow(() => setSubagents(true), "a refused write must not crash the caller");
    },
  );
});

// ---------- the toggle's launcher GATE (#2519 C2) ----------
//
// `subagentsToggleState` decides three outcomes, and the two that are not
// "hidden" are the ones worth pinning: a control that is SHOWN AND DISABLED
// teaches the human what to do next, and a control that is shown and enabled is
// a launch that will mint a real group. Every field the gate reads has a
// fixture that varies it (the #1182 rule), and each of the four gates below is
// varied ALONE, from the one enabled baseline — so a gate deleted from the
// implementation reddens exactly its own row.

const LEAD_OK = {
  kind: "agent",
  program: "claude",
  isCustom: false,
  leadCapableCli: true,
  tabOwnsGroup: false,
} as const;

test("the toggle is offered for a plain claude agent launch (#2519)", () => {
  assert.deepEqual(subagentsToggleState(LEAD_OK), { hidden: false, disabled: false, reason: null });
});

test("the toggle is HIDDEN wherever it does not apply, one field at a time (#2519)", () => {
  const hidden = { hidden: true, disabled: false, reason: null };
  assert.deepEqual(subagentsToggleState({ ...LEAD_OK, kind: "orchestrator" }), hidden, "another pane kind");
  assert.deepEqual(subagentsToggleState({ ...LEAD_OK, isCustom: true }), hidden, "the human's own command line");
  assert.deepEqual(subagentsToggleState({ ...LEAD_OK, program: null }), hidden, "no program named");
  assert.deepEqual(
    subagentsToggleState({ ...LEAD_OK, leadCapableCli: false }),
    hidden,
    "a CLI whose MCP config cannot ride the command line (opencode, codex)"
  );
});

test("a tab that already owns a group DISABLES the toggle with a reason, never hides it (#2519)", () => {
  // The distinction is the point: hiding teaches nothing, and the human's next
  // move (a new tab) is only obvious if something says so. The reason is
  // asserted for CONTENT, not just for presence — a disabled control whose
  // explanation is an empty string is the failure this is guarding against.
  const state = subagentsToggleState({ ...LEAD_OK, tabOwnsGroup: true });
  assert.equal(state.hidden, false, "shown");
  assert.equal(state.disabled, true, "and disabled");
  assert.match(state.reason ?? "", /already runs an orchestration group/);
  assert.match(state.reason ?? "", /new tab/, "and it names the way out");
});

test("hidden always beats disabled — a tab-owned group on a custom line stays hidden (#2519)", () => {
  // Order matters and is not arbitrary: the applicability gates run first, so a
  // form state that could never mint a lead at all does not explain to the
  // human why it will not. `reason` is null exactly when `disabled` is false,
  // which is the invariant a caller renders against.
  const state = subagentsToggleState({ ...LEAD_OK, isCustom: true, tabOwnsGroup: true });
  assert.deepEqual(state, { hidden: true, disabled: false, reason: null });
});

test("a reason is present exactly when the control is disabled (#2519)", () => {
  // Swept over every combination the gate's five inputs can take, so the
  // invariant is a property of the FUNCTION rather than of the rows above.
  let disabledSeen = 0;
  for (const kind of ["agent", "orchestrator", "terminal"]) {
    for (const program of ["claude", null]) {
      for (const isCustom of [false, true]) {
        for (const leadCapableCli of [false, true]) {
          for (const tabOwnsGroup of [false, true]) {
            const s = subagentsToggleState({ kind, program, isCustom, leadCapableCli, tabOwnsGroup });
            assert.equal(s.reason !== null, s.disabled, `reason<->disabled for ${JSON.stringify({ kind, program, isCustom, leadCapableCli, tabOwnsGroup })}`);
            if (s.disabled) disabledSeen++;
          }
        }
      }
    }
  }
  // The sweep's positive control: it really did reach the disabled branch, so a
  // gate that could never disable anything would fail here rather than pass
  // vacuously over 24 rows that were all `hidden`.
  assert.equal(disabledSeen, 1, "exactly one of the 24 combinations is the disabled one");
});

test("a lead launch opens exactly one pane, whatever the fan-out field says (#2519)", () => {
  // N leads is N orchestration groups minted into one tab from one gesture,
  // which is what the toggle's own disabled reason says a tab cannot have. The
  // field is disabled in the DOM AND the count is clamped here, because the DOM
  // is not what decides.
  assert.equal(leadLaunchCount(4, true), 1);
  assert.equal(leadLaunchCount(1, true), 1);
  // …and it changes NOTHING for an ordinary launch, which is the control that
  // makes the assertions above about leads rather than about clamping.
  assert.equal(leadLaunchCount(4, false), 4);
  assert.equal(leadLaunchCount(1, false), 1);
});

test("a ticked box the live gate now refuses is REPORTED, not silently dropped (#2519 B1)", () => {
  // The finding this pins: the gate's answer can change while a form sits open
  // (a second welcome form in the same tab launches a lead; a session restore
  // binds a group), so deciding from the checkbox alone minted a second group
  // into one tab. Deciding from the gate alone would be the opposite defect —
  // the human ticks a box and nothing happens, with no word about it.
  const disabled = subagentsToggleState({ ...LEAD_OK, tabOwnsGroup: true });
  const refused = subagentsLaunchDecision(disabled, true);
  assert.equal(refused.mint, false, "no group is minted into a tab that has one");
  assert.match(refused.refusal ?? "", /already runs an orchestration group/, "…and the human is told why");
});

test("the launch decision's other three outcomes (#2519 B1)", () => {
  const ok = subagentsToggleState(LEAD_OK);
  const hidden = subagentsToggleState({ ...LEAD_OK, isCustom: true });
  const disabled = subagentsToggleState({ ...LEAD_OK, tabOwnsGroup: true });
  assert.deepEqual(subagentsLaunchDecision(ok, true), { mint: true, refusal: null }, "ticked and allowed");
  assert.deepEqual(subagentsLaunchDecision(ok, false), { mint: false, refusal: null }, "not ticked");
  // A ticked-but-HIDDEN box is a stale preference from a previous launch, not a
  // request just made, so it is silent — the distinction from the disabled case
  // above is the whole point, and asserting only one of them would not hold it.
  assert.deepEqual(subagentsLaunchDecision(hidden, true), { mint: false, refusal: null }, "ticked but not applicable");
  assert.equal(subagentsLaunchDecision(disabled, false).refusal, null, "not ticked, so nothing to report");
});
