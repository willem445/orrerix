// Pure pane-header context-menu model for the cross-workspace connect gesture (#271),
// including the W3 addendum's standalone-pane membership + directional (sender/receiver)
// model. DOM-free — mirrors filemenu.ts: the SHAPE of the menu (what's offered, on which
// pane, in what pending-arm/direction state) is decided and unit-tested here;
// contextmenu.ts renders it and orchestration.ts executes the fired action against the
// backend.
//
// THE GESTURE (human-only, explicit, opt-in — the issue's hard requirement). Right-click
// a free agent pane → "Connect…" arms it. Right-click a SECOND pane → the completion
// menu, which (W3) is now DIRECTIONAL: a fresh two-party connect offers two explicit-arrow
// items ("Connect: A → sends to → B" / "Connect: B → sends to → A"), each disabled with a
// reason if that side can't hold the sender role (no token — a delivery-only pane).
// Joining a channel that already has a sender offers only "Join as receiver — driven by
// {sender}" (B4: a newcomer can only ever join as a receiver). Right-clicking the ARMED
// pane again offers "Cancel connecting…" instead of a second arm — that's how a self-click
// cancels. Arming is only ever offered on a FREE pane; an already-connected pane is still a
// valid completion TARGET while another pane is armed — that's how a free THIRD pane joins
// an existing channel (multi-party), matching connect_agents' join rules (mod.rs): the
// direction of the GESTURE is always "arm the newcomer, complete on the target"; the
// direction of the CHANNEL (who may send) is a separate, explicit choice made at that
// completion moment.
//
// STANDALONE PANES (W3 part A). A standalone launcher pane now carries a channel identity
// too (`group: "__solo__"`, `agentId: "solo-N"`) once `orch_solo_prepare`/`orch_solo_bind`
// or `orch_solo_adopt` has run — so it stops hitting `NOT_CAPABLE_REASON` and is a normal
// connect target/source, full membership (claude/copilot) or delivery-only (everything
// else) exactly like an adopted pre-feature pane. `identity()` below is unchanged: the
// capability gate is still "does this pane have a group+agentId", it's just that solo panes
// now legitimately can.

import type { MenuItem } from "./contextmenu";
import { watchMenuLabel } from "./watchedpanes.ts";
import { canForkCli } from "./panerestore.ts";

/** One pane's orchestration identity, as a connect action needs it — bound at
 *  arm/complete time (the same identity-vs-index discipline filemenu.ts's header
 *  describes for OpTarget), so a fired action carries a complete instruction rather
 *  than a pane reference that may have closed or rebound by the time it's read. */
export interface PaneIdentity {
  group: string;
  agentId: string;
  name: string;
  /** Whether this pane currently holds a channel-send-capable token (#271 W3
   *  addendum, part B6: "sender requires a token"). False for a delivery-only
   *  member — an adopted pre-feature pane, or a solo pane on a CLI with no MCP
   *  config seam (codex/gemini/opencode/custom). Gates whether this pane is
   *  eligible to be designated sender. */
  canSend: boolean;
  /** This pane's CURRENT channel's sender agent id/name, if it's already
   *  connected — `null` for a free pane. Drives the JOIN compatibility rule
   *  (B4): completing a connect against an already-connected pane can only
   *  ever add the newcomer as a receiver of that pane's EXISTING sender. */
  senderId: string | null;
  senderName: string | null;
  /** This pane's current channel id, or `null` if free — carried on the
   *  identity so a `set-sender` action (bound at menu-build time) has
   *  everything it needs without re-reading pane state later. */
  channelId: string | null;
}

/** The armed source of an in-progress connect gesture. There is at most one of these
 *  live at a time, globally, across every tab (channel.ts's `reduceConnect` is the
 *  state machine that maintains it). */
export type PendingConnect = PaneIdentity;

export type PaneMenuAction =
  | { kind: "connect-arm"; source: PaneIdentity }
  /** `senderAgent` is the explicit direction choice (B2) — always `from.agentId` or
   *  `to.agentId`, chosen at completion, never inferred from gesture order. */
  | { kind: "connect-complete"; from: PaneIdentity; to: PaneIdentity; senderAgent: string }
  | { kind: "connect-cancel" }
  /** Watch, or stop watching, the pane the menu was opened on (#3319). It
   *  carries NO pane reference, deliberately unlike its siblings here: the
   *  other actions are fired at a pane named in the action because the pane
   *  they act on may not be the one you right-clicked (a connect names both
   *  ends). This one always acts on the pane the menu belongs to, which the
   *  dispatcher already holds, and inventing a second way to say so would be a
   *  second thing that can disagree. */
  | { kind: "toggle-watch" }
  | { kind: "disconnect"; pane: PaneIdentity }
  /** Human-only sender swap (B5) — "Make this pane the sender" on a
   *  token-holding receiver of an already-live channel. */
  | { kind: "set-sender"; pane: PaneIdentity }
  /** Promote this standalone agent pane to the orchestrator of a real group
   *  (#407). Carries the whole `promote_to_orchestrator` argument list decided
   *  HERE, at menu-build time — the same identity-vs-reference discipline the
   *  connect actions follow: by the time the human clicks, the pane may have
   *  re-bound or been typed into, and a promotion that resumes a DIFFERENT
   *  session than the menu was built against is the one mistake this gesture
   *  can't take back. */
  | {
      kind: "promote";
      /** The pane's own working directory — becomes the group's repo. */
      repo: string;
      /** Full session id (never a prefix — the backend refuses one). */
      sessionId: string;
      cli: string;
      /** The `__solo__` agent id the promotion retires, or null when this pane
       *  never got a standalone channel identity (adopt-on-connect failed, or
       *  the pane predates channel tools). Optional backend-side. */
      soloAgentId: string | null;
    }
  /** Fork this pane's session (#3318 F1): open a NEW pane running the
   *  vendor's own fork of this one's conversation, leaving this pane
   *  untouched. Same identity-vs-reference discipline as `promote` above, and
   *  it carries the recorded LINE as well as the session id: the fork is built
   *  by rewriting that line, and a pane can be restarted or re-bound between
   *  the menu opening and the click. */
  | {
      kind: "fork";
      /** The session being forked — the PARENT. Full id, never a prefix. */
      sessionId: string;
      cli: string;
      /** The fork opens in the source's own directory: it is the human's own
       *  environment and a side quest on the same work is the gesture's whole
       *  point. A worktree of its own is #3318 F2's, for a DELEGATE fork. */
      workdir: string;
      /** The source pane's recorded launch line, both representations — what
       *  the fork line is rewritten FROM, so the child keeps the model, the
       *  permission posture and every other flag the human launched with. */
      command: string | null;
      argv: string[] | null;
      /** The source pane's name, so the child can be named after it. */
      sourceName: string;
      /** The source was a LEAD pane (#3318 F2): its line carries the lead's
       *  identity, which the child must shed — the child is a standalone pane,
       *  never a second lead (the one-root invariant). */
      sourceWasLead: boolean;
    }
  /** Fork an orchestration DELEGATE's session (#3318 F2): the backend's
   *  `orch_fork_agent`, which opens the child as a new delegate of the same
   *  block — the same registry method an orchestrator's `fork_session` reaches.
   *  Carries only the identity: the session, the line and the workspace are
   *  the roster's, read backend-side at the click. */
  | {
      kind: "fork-delegate";
      group: string;
      agentId: string;
      sourceName: string;
    };

export type PaneMenuItem = MenuItem<PaneMenuAction>;

/** The slice of a pane's state the menu needs — a structural subset of `Pane`'s
 *  orchestration fields, not an import of the `Pane` class (keeps this module
 *  DOM-free and node:test-loadable; see filemenu.ts's header for why a tested
 *  module can't value-import a DOM-touching sibling). */
export interface PaneConnectState {
  /** null for a pane with no channel identity at all — a shell/content pane, or a
   *  standalone launcher pane that hasn't been prepared/bound/adopted yet. Once a
   *  standalone pane HAS a channel identity (W3), this is `"__solo__"`. */
  group: string | null;
  agentId: string | null;
  name: string;
  /** "orchestrator" | "worker" | "reviewer" | "planner" | "solo", or null alongside a
   *  null `group`/`agentId`. */
  role: string | null;
  /** The channel this pane currently belongs to, or null if free. */
  channelId: string | null;
  /** Whether this pane currently holds a channel-send-capable token — see
   *  `PaneIdentity.canSend`. Irrelevant (and meaningless) for a pane with no
   *  identity at all. */
  canSend: boolean;
  /** This pane's current channel's sender, if connected — see `PaneIdentity`. */
  senderId: string | null;
  senderName: string | null;
  // ── #407: what the PROMOTE gesture needs, and nothing else ──────────────
  // These three are read off the pane exactly as `Pane.agentCli` /
  // `Pane.sessionId` / `Pane.workdir` report them. They carry the VALUES rather
  // than the `hasSessionId`/`hasWorkdir` booleans the plan sketched, for the
  // same reason `PaneIdentity` carries a whole identity: the fired action has to
  // be a complete instruction, and a bool cannot fill one.
  /** The normalized agent CLI this pane runs ("claude" | "copilot" | …), or
   *  null for a plain shell, a content pane, or a command that isn't an agent
   *  CLI at all (`npm run dev`). Non-null is what makes a pane one this gesture
   *  is about — see `promoteItem`, where it is the only such gate. v1 promotes
   *  claude. */
  agentCli: string | null;
  /** The pane's recorded full session id, or null when loomux hasn't minted or
   *  learned one yet (#440) — a pane nobody has prompted has no conversation to
   *  carry over. */
  sessionId: string | null;
  /** The pane's working directory — becomes the promoted group's repo. */
  workdir: string | null;
  /** Whether the human is currently watching this pane (#3319) — the only
   *  field here that gates nothing and enables nothing. Every pane can be
   *  watched, so this decides the item's VERB and never its presence. */
  watched: boolean;
  // ── #3318 F1: what the FORK gesture needs beyond the three above ──
  /** The pane's recorded launch line, both representations, exactly as
   *  `Pane.launchLine` reports them. A fork is that line rewritten, so the
   *  child keeps the model and posture the human launched with — carried on
   *  the state (and then on the action) rather than re-read at click time, the
   *  same reason `PaneIdentity` carries a whole identity. */
  command: string | null;
  argv: string[] | null;
}

const NOT_CAPABLE_REASON =
  "This pane has no connectable agent — only orchestrator, worker, reviewer, and standalone agent panes can join a channel.";
const PLANNER_REASON =
  "A planner's pane closes as soon as it reports done, so it can never join a channel.";
const CANT_BE_SENDER_REASON =
  "This pane is receive-only — it has no channel token, so it can't be the sender.";

// ---------- promote to orchestrator (#407) ----------

const PROMOTE_LABEL = "Promote to orchestrator…";
const PROMOTE_CLI_REASON =
  "Promoting is Claude-only for now — a promotion resumes this pane's own session, and orrerix only knows a claude pane's session id well enough to reopen it.";
const PROMOTE_NO_SESSION_REASON =
  "orrerix doesn't know this pane's conversation yet — a promotion resumes the session it already has, so send this agent a prompt first.";
const PROMOTE_NO_WORKDIR_REASON =
  "This pane has no working directory, and a promoted pane's own directory becomes the group's repository.";

/** The promote item for this pane, or `null` when the gesture doesn't apply here
 *  at all.
 *
 *  Deliberately decided OUTSIDE `buildPaneMenu`'s connect short-circuits and
 *  independently of every connect input (`pending`, `channelId`, `canSend`):
 *  promotion has nothing to do with channels, and a claude pane whose
 *  adopt-on-connect failed — no `group`, no `agentId`, so the connect half is
 *  `NOT_CAPABLE` — is still a perfectly promotable session. Ordering that
 *  decision after the short-circuit is exactly how the item would silently go
 *  missing on the panes it exists for.
 *
 *  `null` (no item) vs disabled-with-a-reason:
 *  - **no item** for a pane the gesture is not about — a shell/content pane, one
 *    running a command that is not an agent CLI at all, or one already in an
 *    orchestration group. A delegate's menu must not grow a permanently-dead row,
 *    and the backend refuses a recorded member anyway (`promote-already-managed`),
 *    including the dormant membership this can't see.
 *  - **disabled + reason** for an AGENT pane that could plausibly be promoted but
 *    isn't eligible right now (wrong CLI, no session yet, no cwd). Here the human
 *    is looking for the item, so silence would read as a missing feature. */
function promoteItem(p: PaneConnectState): PaneMenuItem | null {
  // A recognized agent CLI is the whole gate, and it is deliberately the ONLY
  // one: "was this pane launched with a command" (`Pane.isAgentPane`) is the
  // weaker half of the same fact — both derive from `Pane.spawnCommand`, so a
  // pane with a recognized CLI always had a command, while a `npm run dev` or
  // `htop` pane has a command and no CLI. Gating on the command would put a
  // permanently-dead "promotion is Claude-only" row on a build watcher's menu;
  // gating on both would add a condition nothing — in production or in a
  // fixture — can make fire on its own. (A copilot pane answers `"copilot"` and
  // still gets the greyed row below: that is the case worth explaining.)
  if (p.agentCli === null) return null;
  // An orchestration member has a group AND a real role; a standalone pane's
  // identity is the `__solo__` carrier, whose role is "solo". Keyed off the role
  // rather than the `"__solo__"` group sentinel so this module doesn't grow a
  // second spelling of a constant that lives in mod.rs (mirrored once, in
  // orchestration.ts's `SOLO_GROUP`).
  if (p.group !== null && p.role !== "solo") return null;

  const refuse = (reason: string): PaneMenuItem => ({ label: PROMOTE_LABEL, disabled: true, reason });
  if (p.agentCli !== "claude") return refuse(PROMOTE_CLI_REASON);
  if (!p.sessionId) return refuse(PROMOTE_NO_SESSION_REASON);
  if (!p.workdir) return refuse(PROMOTE_NO_WORKDIR_REASON);
  return {
    label: PROMOTE_LABEL,
    action: {
      kind: "promote",
      repo: p.workdir,
      sessionId: p.sessionId,
      cli: p.agentCli,
      soloAgentId: p.role === "solo" ? p.agentId : null,
    },
  };
}

// ---------- fork this session (#3318 F1) ----------

const FORK_LABEL = "Fork session…";
const FORK_CLI_REASON =
  "This CLI has no command-line fork, and a fork here runs the CLI's own — copilot's /fork is interactive-only and gemini has none (#3318).";
const FORK_NO_SESSION_REASON =
  "orrerix doesn't know this pane's conversation yet — a fork copies the session it already has, so send this agent a prompt first.";
const FORK_NO_WORKDIR_REASON =
  "This pane has no working directory, and a fork opens in the directory its source is working in.";
const FORK_NO_COMMAND_REASON =
  "orrerix has no launch line recorded for this pane, and a fork is that line rewritten — there is nothing to rewrite.";

/** The fork item for this pane, or `null` when the gesture doesn't apply here
 *  at all.
 *
 *  Decided outside `buildPaneMenu`'s connect short-circuits for exactly
 *  `promoteItem`'s reason, and it takes the same null-vs-disabled rule: no item
 *  for a pane the gesture is not about (a shell, a build watcher, a pane
 *  already in an orchestration group), a disabled row WITH a reason for an
 *  agent pane that could plausibly be forked but is not eligible right now.
 *
 *  **Three routes, by who the pane belongs to** (#3318 F2):
 *
 *  - a SOLO pane (or one with no identity yet) forks to a Solo pane, built
 *    here in the frontend — F1's gesture, widened from claude to every CLI
 *    with a fork seam (`canForkCli`);
 *  - a LEAD's own pane forks to a Solo pane too, never a second lead (the
 *    one-root invariant, `docs/design/lead-pane.md`): the same frontend route,
 *    with the lead's identity shed from the line (`sourceWasLead`);
 *  - an orchestration DELEGATE (worker, reviewer, planner) forks through the
 *    backend (`fork-delegate` → `orch_fork_agent`), which owns the roster row,
 *    the audit row, the worktree and every refusal a delegate fork has — a
 *    drive-owned pane, a session-less one. Nothing is decided about a delegate
 *    here beyond the CLI, so the menu never contradicts the backend's answer.
 *
 *  An orchestrator or a manager pane gets no item: neither is a delegate
 *  anyone may open a second of, and the backend refuses both. */
function forkItem(p: PaneConnectState): PaneMenuItem | null {
  // Same gate, and the same argument, as `promoteItem`'s: a recognized agent
  // CLI is what makes a pane one this gesture is about.
  if (p.agentCli === null) return null;
  const delegate = p.group !== null && (p.role === "worker" || p.role === "reviewer" || p.role === "planner");
  if (p.group !== null && p.role !== "solo" && p.role !== "lead" && !delegate) return null;

  const refuse = (reason: string): PaneMenuItem => ({ label: FORK_LABEL, disabled: true, reason });
  if (!canForkCli(p.agentCli)) return refuse(FORK_CLI_REASON);
  if (delegate) {
    // A delegate's fork is the backend's, keyed on its agent id — and a
    // delegate pane with no id to name has no route at all, rather than
    // falling through to the SOLO route below and forking a group agent as a
    // standalone pane nobody's roster knows.
    if (p.group === null || p.agentId === null) return null;
    return {
      label: FORK_LABEL,
      action: { kind: "fork-delegate", group: p.group, agentId: p.agentId, sourceName: p.name },
    };
  }
  if (!p.sessionId) return refuse(FORK_NO_SESSION_REASON);
  if (!p.workdir) return refuse(FORK_NO_WORKDIR_REASON);
  if (!p.command?.trim() && !p.argv?.length) return refuse(FORK_NO_COMMAND_REASON);
  return {
    label: FORK_LABEL,
    action: {
      kind: "fork",
      sessionId: p.sessionId,
      cli: p.agentCli,
      workdir: p.workdir,
      command: p.command,
      argv: p.argv,
      sourceName: p.name,
      sourceWasLead: p.role === "lead",
    },
  };
}

/** The fork action a pane's CURRENT state yields, or the reason it yields none
 *  — `forkItem` without the menu around it (#3318 F2). The lead's self-fork
 *  (`orch-fork-solo-request`) arrives with no menu open, and this is what it
 *  builds its action from, so a request from the backend is held to exactly
 *  the rules a right-click is. */
export function forkActionFor(
  p: PaneConnectState
): { action: Extract<PaneMenuAction, { kind: "fork" }> } | { refusal: string } {
  const item = forkItem(p);
  const action = item?.action;
  if (action?.kind === "fork") return { action };
  return { refusal: item?.reason ?? "this pane is not one orrerix can fork from here" };
}

/** Why a fork must NOT proceed at the CLICK, though it was offered when the
 *  menu opened — or null when it may (#3331 item 1).
 *
 *  The menu binds the pane's session at BUILD time, and a pane can be
 *  restarted or re-bound between the right-click and the click: forking the id
 *  the menu captured would then fork the PREVIOUS session and hand the child a
 *  conversation the pane is no longer having. So the click re-reads the pane
 *  and refuses when either fact the fork line is built from has moved — the
 *  session, or the CLI (a different CLI means a different fork grammar
 *  altogether). Refusing rather than silently forking the new session: the
 *  human chose to fork the conversation they were looking at. */
export function forkClickRefusal(
  action: { sessionId: string; cli: string },
  now: { sessionId: string | null; agentCli: string | null }
): string | null {
  if (now.agentCli !== action.cli) {
    return "This pane is running a different CLI than when the menu opened — open the menu again to fork what it runs now.";
  }
  if (now.sessionId !== action.sessionId) {
    return "This pane's session changed after the menu opened (it was restarted or re-bound) — open the menu again to fork the session it is running now.";
  }
  return null;
}

function identity(p: PaneConnectState): PaneIdentity | null {
  return p.group !== null && p.agentId !== null
    ? {
        group: p.group,
        agentId: p.agentId,
        name: p.name,
        canSend: p.canSend,
        senderId: p.senderId,
        senderName: p.senderName,
        channelId: p.channelId,
      }
    : null;
}

/** Build the pane header's context menu.
 *
 *  `pending` is the currently-armed connect source (channel.ts's module-level state,
 *  threaded in by the caller), or null if no gesture is in progress. Every action this
 *  returns carries the full identity it needs — see the module header.
 *
 *  The menu is FOUR independent gestures, and saying the number is the point:
 *  the connect model below, #407's promote item, #3318 F1's fork item, and
 *  #3319's watch toggle. Every one of the three non-connect items is composed
 *  at THIS level rather than inside the connect branches, so none of them can
 *  be lost to a connect short-circuit — see `promoteItem` for the argument,
 *  which has applied unchanged to each gesture added since. */
export function buildPaneMenu(pane: PaneConnectState, pending: PendingConnect | null): PaneMenuItem[] {
  const items = connectItems(pane, pending);
  const promote = promoteItem(pane);
  const fork = forkItem(pane);
  if (promote || fork) {
    if (items.length) items.push({ label: "", separator: true });
    if (promote) items.push(promote);
    if (fork) items.push(fork);
  }
  // The watch (#3319), composed HERE and outside `connectItems` for the reason
  // the promote item is: those branches short-circuit on a pane with no channel
  // identity, and this gesture is about EVERY pane — a plain shell and a files
  // pane are watchable, and a human who right-clicks one and finds only a
  // greyed "Connect" has been told the menu has nothing for them.
  //
  // LAST in the menu, after its own separator: it is the only item here that
  // changes nothing outside this window and asks nothing of any agent, so it
  // sits below the two that do. It is never `disabled` — there is no pane this
  // question cannot be asked about, which is why `PaneConnectState.watched`
  // gates nothing.
  if (items.length) items.push({ label: "", separator: true });
  items.push({ label: watchMenuLabel(pane.watched), action: { kind: "toggle-watch" } });
  return items;
}

function connectItems(pane: PaneConnectState, pending: PendingConnect | null): PaneMenuItem[] {
  const id = identity(pane);
  if (!id) return [{ label: "Connect", disabled: true, reason: NOT_CAPABLE_REASON }];
  if (pane.role === "planner") return [{ label: "Connect", disabled: true, reason: PLANNER_REASON }];

  const items: PaneMenuItem[] = [];
  const isPendingSource = pending !== null && pending.agentId === id.agentId;

  if (isPendingSource) {
    items.push({ label: "Cancel connecting…", action: { kind: "connect-cancel" } });
  } else if (pending) {
    // A JOIN happens when EITHER side already belongs to a live channel — that
    // channel's sender is fixed, so the only compatible completion is "join as
    // receiver, driven by the existing sender" (B4). Both sides can't already be
    // connected here: `buildPaneMenu` never offers Connect-here as a target's
    // OWN pending-arm source (self-click is handled above), and the caller
    // (orchestration.ts) only shows this menu against a target that isn't
    // itself the armed pane.
    const existingSenderId = pending.senderId ?? id.senderId;
    const existingSenderName = pending.senderId ? pending.senderName : id.senderName;
    if (existingSenderId) {
      items.push({
        label: `Join as receiver — driven by ${existingSenderName ?? existingSenderId}`,
        action: { kind: "connect-complete", from: pending, to: id, senderAgent: existingSenderId },
      });
    } else {
      const pendingAsSender: PaneMenuItem = {
        label: `Connect: ${pending.name} → sends to → ${id.name}`,
        action: { kind: "connect-complete", from: pending, to: id, senderAgent: pending.agentId },
      };
      if (!pending.canSend) {
        pendingAsSender.disabled = true;
        pendingAsSender.reason = CANT_BE_SENDER_REASON;
      }
      const idAsSender: PaneMenuItem = {
        label: `Connect: ${id.name} → sends to → ${pending.name}`,
        action: { kind: "connect-complete", from: pending, to: id, senderAgent: id.agentId },
      };
      if (!id.canSend) {
        idAsSender.disabled = true;
        idAsSender.reason = CANT_BE_SENDER_REASON;
      }
      items.push(pendingAsSender, idAsSender);
    }
  } else if (!pane.channelId) {
    items.push({ label: "Connect…", action: { kind: "connect-arm", source: id } });
  }

  // Disconnect is independent of the arm/complete state above — a connected pane
  // can be both a valid join TARGET (a pending arm elsewhere) and disconnectable.
  if (pane.channelId && !isPendingSource) {
    items.push({ label: "Disconnect", action: { kind: "disconnect", pane: id } });
    // "Make this pane the sender" (B5): only for a token-holding receiver of an
    // already-live channel — never for the current sender itself, never for a
    // delivery-only pane.
    if (pane.senderId && pane.senderId !== id.agentId && id.canSend) {
      items.push({ label: "Make this pane the sender", action: { kind: "set-sender", pane: id } });
    }
  }

  return items;
}
