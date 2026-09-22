// Pure pane-connect-menu model (#271, plus the W3 addendum's standalone-pane +
// directional model) — panemenu.ts. Pins the menu SHAPE across every pane/pending-arm
// state: free, connected, planner, solo, delivery-only, non-capable, the armed source
// itself, a fresh two-party directional completion, and a join onto an already-driven
// channel.
//
// #407 adds the promote-to-orchestrator item and its eligibility matrix at the
// bottom of this file — a SECOND, independent gesture on the same menu, which is
// why its tests assert on the promote item alone rather than on `kinds()` of the
// whole menu.
import { test } from "node:test";
import assert from "node:assert/strict";
import { buildPaneMenu, type PaneConnectState, type PaneMenuItem, type PendingConnect } from "../src/panemenu.ts";

const free = (overrides: Partial<PaneConnectState> = {}): PaneConnectState => ({
  group: "g1",
  agentId: "w-1",
  name: "w-1",
  role: "worker",
  channelId: null,
  canSend: true,
  senderId: null,
  senderName: null,
  // #407: an orchestration worker pane — an agent pane, but never promotable
  // (it already belongs to a group). The promote fixtures below override these.
  agentCli: "claude",
  sessionId: "11111111-2222-3333-4444-555555555555",
  workdir: "/repo/poc",
  // #3319: unwatched by default, which is the state every pane is born into.
  // Spelled out rather than left to fall through as `undefined`: this file is
  // not in tsconfig's `include`, so nothing would have told us it was missing.
  watched: false,
  // #3318 F1: the recorded launch line the FORK item rewrites. A real line,
  // not a stub: the fork fixtures below assert the child keeps the flags on it.
  command: "claude --session-id 11111111-2222-3333-4444-555555555555 --model opus",
  argv: null,
  ...overrides,
});

const pendingFrom = (overrides: Partial<PendingConnect> = {}): PendingConnect => ({
  group: "g0",
  agentId: "orch-1",
  name: "orch-1",
  canSend: true,
  senderId: null,
  senderName: null,
  channelId: null,
  ...overrides,
});

/** The CONNECT items alone — the gesture this file's first block is about.
 *
 *  `toggle-watch` (#3319) is dropped here for the same reason #407's promote
 *  tests assert on the promote item alone: it is a THIRD independent gesture on
 *  the same menu, offered on every pane unconditionally, so folding it into
 *  every connect expectation would say "and a watch item" eleven times and
 *  pin it nowhere. It has its own block at the bottom of this file, where it
 *  can be asserted as the thing it is. */
const kinds = (items: ReturnType<typeof buildPaneMenu>) =>
  items.filter((i) => !i.separator && i.action?.kind !== "toggle-watch").map((i) => i.action?.kind);

/** Everything the menu offers, watch item included — for the assertions that
 *  are about the menu as a whole rather than about connecting. */
const allKinds = (items: ReturnType<typeof buildPaneMenu>) =>
  items.filter((i) => !i.separator).map((i) => i.action?.kind);

/** The connect-gesture items, for the "offers a single disabled item" shape
 *  assertions — which are about what CONNECTING offers on an ineligible pane,
 *  not about the menu's length. */
const connectItemsOf = (items: ReturnType<typeof buildPaneMenu>): PaneMenuItem[] =>
  items.filter((i) => !i.separator && i.action?.kind !== "toggle-watch");

test("a free, MCP-capable pane with no pending arm offers only Connect (arm)", () => {
  const items = buildPaneMenu(free(), null);
  assert.deepEqual(kinds(items), ["connect-arm"]);
});

test("a non-orchestration pane (shell/content) offers a single disabled item, never a live action", () => {
  // `agentCli: null` is what makes this a SHELL pane rather than an agent pane
  // whose adopt-on-connect failed. Before #407 nothing in the state told those
  // two apart, and this fixture meant both; now they diverge (an agent pane
  // keeps a promote item — see the #407 block below), so the fixture has to say
  // which one it is. The connect assertion itself is unchanged.
  // Asserted on the CONNECT items, not on `items.length` (#3319): the menu now
  // also carries an unconditional watch item, and the property this test is
  // about — an ineligible pane is offered no LIVE connect action, only a
  // disabled one saying why — is unchanged by that. Relocating it onto a
  // witness that still distinguishes, rather than relaxing it to fit.
  const items = buildPaneMenu(free({ group: null, agentId: null, role: null, agentCli: null }), null);
  const connect = connectItemsOf(items);
  assert.equal(connect.length, 1);
  assert.equal(connect[0].disabled, true);
  assert.ok(connect[0].reason && connect[0].reason.length > 0);
  assert.equal(connect[0].action, undefined);
});

test("a planner pane offers a single disabled item naming why, even though it has an agent id", () => {
  const items = buildPaneMenu(free({ role: "planner" }), null);
  const connect = connectItemsOf(items);
  assert.equal(connect.length, 1);
  assert.equal(connect[0].disabled, true);
  assert.match(connect[0].reason ?? "", /planner/i);
});

test("a standalone solo pane with a channel identity is capable — it offers Connect like any other agent pane", () => {
  const solo = free({ group: "__solo__", agentId: "solo-3", role: "solo", canSend: true });
  const items = buildPaneMenu(solo, null);
  // Connect first, then #407's promote item, then #3318 F1's fork — a solo
  // claude pane with a known session is exactly the shape all three gestures
  // apply to, so this is the one place they appear together. (#3319's watch is
  // on every pane, so `kinds` filters it out rather than repeating it in every
  // assertion in this file.)
  assert.deepEqual(kinds(items), ["connect-arm", "promote", "fork"]);
});

test("right-clicking the ARMED pane again offers Cancel, not a second arm (self-click cancels)", () => {
  const pane = free();
  const pending: PendingConnect = pendingFrom({ group: pane.group!, agentId: pane.agentId!, name: pane.name });
  const items = buildPaneMenu(pane, pending);
  assert.deepEqual(kinds(items), ["connect-cancel"]);
});

test("a fresh two-party connect (neither side has a channel yet) offers BOTH directional items", () => {
  const pending = pendingFrom();
  const items = buildPaneMenu(free(), pending);
  assert.deepEqual(kinds(items), ["connect-complete", "connect-complete"]);
  const [a, b] = items.map((i) => i.action);
  if (a?.kind === "connect-complete" && b?.kind === "connect-complete") {
    // One item names the ARMED pane as sender, the other names THIS pane —
    // both directions offered, the human picks which arrow is correct.
    assert.deepEqual([a.senderAgent, b.senderAgent].sort(), ["orch-1", "w-1"]);
    assert.deepEqual(a.from, pending);
    assert.deepEqual(a.to, { group: "g1", agentId: "w-1", name: "w-1", canSend: true, senderId: null, senderName: null, channelId: null });
  }
  assert.ok(items.every((i) => !i.disabled), "both sides can send — neither item should be disabled");
});

test("a delivery-only side of a fresh connect is disabled as sender, with a reason, but still offered as the OTHER direction", () => {
  const pending = pendingFrom({ canSend: false }); // the armed pane has no token
  const items = buildPaneMenu(free(), pending);
  // Two CONNECT items (#3319: the menu also carries the unconditional watch
  // item and its separator, which this test is not about).
  assert.equal(connectItemsOf(items).length, 2);
  const asPendingSender = items.find((i) => i.action?.kind === "connect-complete" && i.action.senderAgent === "orch-1");
  const asThisSender = items.find((i) => i.action?.kind === "connect-complete" && i.action.senderAgent === "w-1");
  assert.equal(asPendingSender?.disabled, true, "the delivery-only pane can't be designated sender");
  assert.ok(asPendingSender?.reason && /receive-only|token/i.test(asPendingSender.reason));
  assert.equal(asThisSender?.disabled, undefined, "the OTHER pane (has a token) is still offered as sender");
});

test("completing onto a RECEIVER of an already-driven channel offers ONLY ONE completion item — join-as-receiver, driven by the channel's actual sender (PR #289 review round 2, B1)", () => {
  // The completion TARGET here is itself a plain RECEIVER of its own channel
  // (senderId "w-9" !== its own agentId "w-1") — the exact shape the review
  // reproduced as broken: completing the gesture on a receiver, not the
  // sender. `senderAgent` in the resulting action is "w-9" (the channel's
  // real sender, a THIRD party neither pending's "orch-1" nor target's own
  // "w-1") — this is not a leftover implementation detail, it's the fix:
  // `connect_agents` now treats a join's `sender_agent` as a CONFIRMATION of
  // the existing sender, never a requirement that it be one of the two
  // panes this call names. Verified end-to-end against the real backend by
  // `join_completing_on_a_receiver_pane_succeeds_and_keeps_the_existing_sender`
  // (tests/orchestration.rs) — before the fix that integration test failed
  // with exactly the error this menu action used to trigger
  // ("sender_agent must be one of the two connected panes").
  //
  // The target also legitimately offers Disconnect + "Make this pane the
  // sender" — independent of the join/complete state above. The join rule
  // only constrains the COMPLETION item: exactly one, not two directional
  // choices, since the channel's sender is already fixed.
  const pending = pendingFrom(); // a free armed pane
  const target = free({ channelId: "chan-1", senderId: "w-9", senderName: "w-9" });
  const items = buildPaneMenu(target, pending);
  const completions = items.filter((i) => i.action?.kind === "connect-complete");
  assert.equal(completions.length, 1, "a join onto an already-driven channel offers only one completion item");
  assert.match(completions[0].label, /driven by w-9/);
  if (completions[0].action?.kind === "connect-complete") assert.equal(completions[0].action.senderAgent, "w-9");
});

test("completing directly onto the SENDER of an already-driven channel also offers exactly one join item, naming that same sender", () => {
  // The symmetric, already-working case: here the completion target IS the
  // channel's sender (senderId === its own agentId), so senderAgent in the
  // resulting action equals target's own id — a degenerate case of the same
  // rule the test above pins for a receiver target.
  const pending = pendingFrom();
  const target = free({ channelId: "chan-1", senderId: "w-1", senderName: "w-1" }); // target IS the sender
  const items = buildPaneMenu(target, pending);
  const completions = items.filter((i) => i.action?.kind === "connect-complete");
  assert.equal(completions.length, 1);
  assert.match(completions[0].label, /driven by w-1/);
  if (completions[0].action?.kind === "connect-complete") assert.equal(completions[0].action.senderAgent, "w-1");
});

test("an ALREADY-CONNECTED pane (with a resolved sender) is still a valid completion target — how a third pane joins (multi-party)", () => {
  const pending = pendingFrom({ group: "g0", agentId: "w-9", name: "w-9" });
  const items = buildPaneMenu(free({ channelId: "chan-1", senderId: "w-1", senderName: "w-1" }), pending);
  assert.deepEqual(kinds(items), ["connect-complete", "disconnect"]);
});

test("a connected pane with no pending arm and no resolved sender offers only Disconnect — arming never starts from a connected pane", () => {
  const items = buildPaneMenu(free({ channelId: "chan-1" }), null);
  assert.deepEqual(kinds(items), ["disconnect"]);
});

test("disconnect carries the pane's own identity, not the peer's", () => {
  const pane = free({ channelId: "chan-1", agentId: "rev-2", name: "rev-2", senderId: "w-1", senderName: "w-1" });
  const items = buildPaneMenu(pane, null);
  const action = items[0].action;
  assert.equal(action?.kind, "disconnect");
  if (action?.kind === "disconnect") {
    assert.equal(action.pane.group, "g1");
    assert.equal(action.pane.agentId, "rev-2");
    assert.equal(action.pane.name, "rev-2");
  }
});

test("a token-holding RECEIVER of a live channel also gets 'Make this pane the sender'", () => {
  const pane = free({ channelId: "chan-1", agentId: "rev-2", name: "rev-2", canSend: true, senderId: "w-1", senderName: "w-1" });
  const items = buildPaneMenu(pane, null);
  assert.deepEqual(kinds(items), ["disconnect", "set-sender"]);
});

test("the current SENDER never gets 'Make this pane the sender' offered on itself", () => {
  const pane = free({ channelId: "chan-1", agentId: "w-1", name: "w-1", canSend: true, senderId: "w-1", senderName: "w-1" });
  const items = buildPaneMenu(pane, null);
  assert.deepEqual(kinds(items), ["disconnect"]);
});

test("a delivery-only RECEIVER never gets 'Make this pane the sender' — it has no token", () => {
  const pane = free({ channelId: "chan-1", agentId: "rev-2", name: "rev-2", canSend: false, senderId: "w-1", senderName: "w-1" });
  const items = buildPaneMenu(pane, null);
  assert.deepEqual(kinds(items), ["disconnect"]);
});

// ---------- promote to orchestrator (#407 slice B, plan step 6) ----------
//
// The eligibility matrix. Promotion is a SECOND gesture on this menu, decided
// independently of the connect state: a pane with no channel identity at all
// (adopt-on-connect failed, or the pane predates channel tools) is still a
// perfectly promotable claude session, so the promote item must be built OUTSIDE
// the connect short-circuits rather than after them.

/** A standalone claude agent pane that has been adopted as `__solo__` — the
 *  common shape at menu-build time, since `adoptIfEligible` runs before every
 *  right-click. */
const soloPane = (overrides: Partial<PaneConnectState> = {}): PaneConnectState =>
  free({ group: "__solo__", agentId: "solo-3", name: "poc", role: "solo", ...overrides });

const promoteOf = (items: PaneMenuItem[]): PaneMenuItem | undefined =>
  items.find((i) => /^Promote/.test(i.label));

/** #3318 F1's item, found the same way — by LABEL, so a DISABLED row (which
 *  carries no action to match on) is found too. That is the whole reason both
 *  helpers key on the label: the disabled rows are half of what these fixtures
 *  are about. */
const forkOf = (items: PaneMenuItem[]): PaneMenuItem | undefined =>
  items.find((i) => /^Fork session/.test(i.label));

test("#407: a standalone claude pane with a session and a workdir offers Promote, carrying everything the backend call needs", () => {
  const items = buildPaneMenu(soloPane(), null);
  const promote = promoteOf(items);
  assert.ok(promote, "a standalone claude pane must offer the promote item");
  assert.equal(promote.disabled, undefined);
  assert.match(promote.label, /orchestrator/i);
  assert.deepEqual(promote.action, {
    kind: "promote",
    repo: "/repo/poc",
    sessionId: "11111111-2222-3333-4444-555555555555",
    cli: "claude",
    // The `__solo__` identity the promotion retires (PromoteConfig.soloAgentId).
    soloAgentId: "solo-3",
  });
});

test("#407: an agent pane with NO channel identity yet still offers promote — the connect short-circuit must not swallow it", () => {
  // `adoptIfEligible` is best-effort: when it fails, the pane has no group/agentId
  // and `buildPaneMenu` returns the not-capable CONNECT item. Promotion does not
  // need a channel identity at all (soloAgentId is optional backend-side), so the
  // item has to survive that arm — this is the ordering trap the plan named.
  const pane = free({ group: null, agentId: null, role: null, name: "poc" });
  const items = buildPaneMenu(pane, null);
  const promote = promoteOf(items);
  assert.ok(promote, "an un-adopted claude agent pane is still promotable");
  assert.equal(promote.disabled, undefined);
  assert.deepEqual(promote.action, {
    kind: "promote",
    repo: "/repo/poc",
    sessionId: "11111111-2222-3333-4444-555555555555",
    cli: "claude",
    soloAgentId: null, // nothing to retire — there is no solo identity
  });
  // …and the connect half is unchanged: still exactly one disabled Connect item.
  // Every gesture that is not connect is excluded — `connectItemsOf` drops the
  // separators and #3319's watch, and these two drop #407's promote and #3318
  // F1's fork. Naming what this filter KEEPS is what stops the next gesture
  // silently reddening a connect assertion again.
  const connect = connectItemsOf(items).filter((i) => i !== promote && i !== forkOf(items));
  assert.equal(connect.length, 1);
  assert.equal(connect[0].disabled, true);
});

test("#407: a non-claude agent pane offers promote DISABLED, naming the v1 limit rather than silently omitting it", () => {
  const items = buildPaneMenu(soloPane({ agentCli: "copilot" }), null);
  const promote = promoteOf(items);
  assert.ok(promote);
  assert.equal(promote.disabled, true);
  assert.equal(promote.action, undefined, "a disabled item must never carry a fireable action");
  assert.match(promote.reason ?? "", /claude/i);
});

test("#407: an agent pane whose session id loomux has not learned offers promote DISABLED — a promotion resumes a conversation", () => {
  const items = buildPaneMenu(soloPane({ sessionId: null }), null);
  const promote = promoteOf(items);
  assert.ok(promote);
  assert.equal(promote.disabled, true);
  assert.equal(promote.action, undefined);
  assert.match(promote.reason ?? "", /conversation|session/i);
});

test("#407: an agent pane with no working directory offers promote DISABLED — its cwd becomes the group's repo", () => {
  const items = buildPaneMenu(soloPane({ workdir: null }), null);
  const promote = promoteOf(items);
  assert.ok(promote);
  assert.equal(promote.disabled, true);
  assert.equal(promote.action, undefined);
  assert.match(promote.reason ?? "", /director|repositor/i);
});

test("#407: a pane that is already an orchestration member has NO promote item at all", () => {
  // Two conflicting role contracts in one session is what the backend's
  // `promote-already-managed` refuses; the frontend refuses it by never offering
  // the gesture, so a delegate's menu doesn't grow a permanently-dead row.
  for (const role of ["worker", "reviewer", "orchestrator"]) {
    const items = buildPaneMenu(free({ role }), null);
    assert.equal(promoteOf(items), undefined, `a ${role} pane must not offer promote`);
  }
});

test("#407: a planner pane offers neither promote nor connect — its single disabled item is unchanged", () => {
  const items = buildPaneMenu(free({ role: "planner" }), null);
  assert.equal(promoteOf(items), undefined);
  const connect = connectItemsOf(items);
  assert.equal(connect.length, 1);
  assert.match(connect[0].reason ?? "", /planner/i);
});

test("#407: no recognized agent CLI, no promote item — a shell, a content pane, and a command that isn't an agent CLI are one case (rev-1 N1, rev-2 B1)", () => {
  // One test because they are one predicate. `agentCli === null` is the whole
  // gate: a shell and a content pane have no command at all, and a `npm run dev`
  // or `htop` pane has one that resolves to no CLI — for all three, a greyed
  // "promotion is Claude-only" row would be the permanently-dead row the
  // absent-vs-disabled rule exists to avoid. (These used to be two tests split
  // on `isAgentPane`, which `agentCli` subsumes — see promoteItem.)
  const shell = free({ group: null, agentId: null, role: null, agentCli: null, sessionId: null });
  const buildWatcher = free({ group: null, agentId: null, role: null, agentCli: null, name: "npm run dev" });
  for (const pane of [shell, buildWatcher]) {
    assert.equal(promoteOf(buildPaneMenu(pane, null)), undefined, pane.name);
  }
  const items = buildPaneMenu(shell, null);
  const connect = connectItemsOf(items);
  assert.equal(connect.length, 1, "a non-agent pane keeps its single not-capable item");
  assert.equal(connect[0].disabled, true);
});

test("#407: promote is offered alongside an in-progress connect gesture, not swallowed by it", () => {
  // A pending arm rewrites the CONNECT half of the menu entirely (completion
  // items instead of "Connect…"); promotion is orthogonal and must survive.
  const items = buildPaneMenu(soloPane(), pendingFrom());
  assert.ok(promoteOf(items), "an armed connect elsewhere must not hide promote");
  assert.equal(kinds(items).filter((k) => k === "connect-complete").length, 2);
});


// ---------- #3319: the watch item, a third independent gesture ----------

test("#3319: every pane offers the watch item, including the ones connect refuses", () => {
  // THE POINT OF COMPOSING IT IN `buildPaneMenu` RATHER THAN IN
  // `connectItems`. Those branches short-circuit on a pane with no channel
  // identity and on a planner, returning a single disabled row — and a human
  // who right-clicks a plain shell to mark it would otherwise find a menu with
  // nothing in it they can use. The three panes below are exactly the ones
  // that short-circuit, plus one that does not.
  const cases: [string, PaneConnectState][] = [
    ["a worker", free()],
    ["a shell", free({ group: null, agentId: null, role: null, agentCli: null })],
    ["a planner", free({ role: "planner" })],
    ["a connected pane", free({ channelId: "c1", senderId: "orch-1", senderName: "orch-1" })],
  ];
  for (const [what, pane] of cases) {
    const items = buildPaneMenu(pane, null);
    const watch = items.find((i) => i.action?.kind === "toggle-watch");
    assert.ok(watch, `${what} offers no watch item`);
    assert.equal(watch.disabled, undefined, `${what}'s watch item is disabled — no pane is unwatchable`);
  }
});

test("#3319: the watch item's verb follows the pane's current state", () => {
  const off = buildPaneMenu(free({ watched: false }), null).find((i) => i.action?.kind === "toggle-watch");
  const on = buildPaneMenu(free({ watched: true }), null).find((i) => i.action?.kind === "toggle-watch");
  assert.match(off?.label ?? "", /^Watch/);
  assert.match(on?.label ?? "", /^Stop watching/);
  // The discriminator: a label built from the state rather than from the
  // action would read the same on both, and both menus would still "have a
  // watch item".
  assert.notEqual(off?.label, on?.label);
});

test("#3319: the watch item is last, behind its own separator", () => {
  // Position is the claim `buildPaneMenu`'s comment makes — the item that
  // changes nothing outside this window sits below the two that do — and a
  // separator is what keeps it from reading as a third connect option.
  const items = buildPaneMenu(soloPane(), null);
  assert.equal(items[items.length - 1].action?.kind, "toggle-watch");
  assert.equal(items[items.length - 2].separator, true);
  // And it does not displace what was there: promote is still offered on the
  // same menu.
  assert.ok(promoteOf(items));
});

test("#3319: a watch toggle names no pane — it is always the menu's own", () => {
  // The action carries no pane reference, unlike every connect action here.
  // Pinned because the alternative (a `pane` field the dispatcher could
  // disagree with) is the shape a reader would expect from its siblings.
  const watch = buildPaneMenu(free(), null).find((i) => i.action?.kind === "toggle-watch");
  assert.deepEqual(watch?.action, { kind: "toggle-watch" });
});

// ---------------------------------------------------------------------------
// #3318 F1 — the Fork item: a THIRD independent gesture on this menu, with
// the same null-vs-disabled rule as promote and its own eligibility matrix.
// ---------------------------------------------------------------------------

const forkLine = "claude --session-id 11111111-2222-3333-4444-555555555555 --model opus";

test("#3318 F1: a solo claude pane with a session, a workdir and a line offers Fork, carrying all of them", () => {
  const fork = forkOf(buildPaneMenu(soloPane(), null));
  assert.ok(fork, "a solo claude pane is exactly what the gesture is for");
  assert.equal(fork.disabled, undefined);
  assert.deepEqual(fork.action, {
    kind: "fork",
    sessionId: "11111111-2222-3333-4444-555555555555",
    cli: "claude",
    workdir: "/repo/poc",
    command: forkLine,
    argv: null,
    sourceName: soloPane().name,
  });
});

test("#3318 F1: each ineligibility is a DISABLED row naming its own reason, never a silent omission", () => {
  // The human is looking for the item on an agent pane, so silence would read
  // as a missing feature. Each reason is distinct, and asserted as distinct:
  // a single shared string would pass a per-case test while telling the human
  // the wrong thing three times out of four.
  const cases: Array<[string, Partial<PaneConnectState>, RegExp]> = [
    ["a CLI F1 has not wired", { agentCli: "copilot" }, /Claude-only/i],
    ["no session yet", { sessionId: null }, /prompt/i],
    ["no working directory", { workdir: null }, /working directory/i],
    ["no recorded launch line", { command: null, argv: null }, /launch line/i],
  ];
  const reasons = new Set<string>();
  for (const [what, over, re] of cases) {
    const fork = forkOf(buildPaneMenu(soloPane(over), null));
    assert.ok(fork, `${what}: the row is still offered`);
    assert.equal(fork.disabled, true, what);
    assert.equal(fork.action, undefined, `${what}: a disabled row fires nothing`);
    assert.match(fork.reason ?? "", re, what);
    reasons.add(fork.reason ?? "");
  }
  assert.equal(reasons.size, cases.length, "each refusal says its own thing");
});

test("#3318 F1: a pane the gesture is not about gets NO row — a shell, and an orchestration delegate", () => {
  // A shell/build-watcher pane: no agent CLI at all, so no permanently-dead row.
  assert.equal(forkOf(buildPaneMenu(free({ group: null, agentId: null, role: null, agentCli: null }), null)), undefined);
  // An orchestration delegate. Not a refusal with a reason but no row at all,
  // and that is a SCOPE line: forking a delegate is #3318 F2, which owes it a
  // roster row, an audit row, a worktree policy and a drive-owned refusal.
  assert.equal(forkOf(buildPaneMenu(free(), null)), undefined, "a group worker is F2's");
  // ...and a lead's own pane is excluded by the same rung.
  assert.equal(forkOf(buildPaneMenu(free({ role: "lead" }), null)), undefined, "a lead pane is F2's");
});

test("#3318 F1: the fork item survives every connect short-circuit the menu has", () => {
  // `promoteItem`'s lesson, re-run for the second gesture: ordering the fork
  // decision after a connect branch is exactly how the row goes missing on the
  // panes it exists for. A solo pane whose adopt-on-connect FAILED has no
  // group and no agent id, so its connect half is the NOT_CAPABLE row — and it
  // is still a perfectly forkable session.
  const unadopted = soloPane({ group: null, agentId: null, role: null });
  assert.ok(forkOf(buildPaneMenu(unadopted, null)), "un-adopted, still forkable");
  // ...and mid-connect-gesture, where the menu is showing completion items.
  const pending = pendingFrom();
  assert.ok(forkOf(buildPaneMenu(soloPane(), pending)), "armed elsewhere, still forkable");
});
