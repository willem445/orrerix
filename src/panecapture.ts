// Capturing a live pane as its persisted record (#194), split out of pane.ts
// (#3498 F1). `Pane.capture()` delegates here; panerestore.ts decides what each
// record becomes on restore, and this module is its other half — what goes INTO
// the record. Reads only the retained launch inputs plus the live cwd — no
// geometry, no PTY — so it is safe under the no-resize invariant.
//
// Not DOM-free (it reads a live `Pane`), so it stays out of panerestore.ts, whose
// DOM-free contract is what lets test/panerestore.test.ts run under node.
// Design note: docs/design/session-restore.md; layout conventions:
// docs/design/module-layout.md.

import { EMBED_SIDES } from "./embedsplit";
import { type PersistedPane } from "./tabstore";
import { forkRecordCommand } from "./panerestore";
import type { EmbedKind, Pane } from "./pane";

/** The subset of `EmbedKind`s whose embed preference is captured for a whole-
 *  session-restart restore (`Pane.capture()` / `Pane.restoreEmbeds`) — every
 *  kind here restores when docked on an ORCHESTRATOR pane specifically
 *  (`kind === "orch"` in `capture()`), because only orch panes stay DORMANT
 *  across a restart and so have a natural "captured, then reapplied once the
 *  real pane exists" hook (`main.ts`'s `resumeDormantGroup`). `issues` is the
 *  one kind still excluded: embeddable on every pane kind, same as
 *  `git`/`editor`, but with no equivalent restore hook on a PLAIN
 *  terminal/agent pane (unchanged limitation — see
 *  docs/design/embedded-panels.md's "Why only these kinds survive a restart"
 *  — nothing about `issues` itself is different, there's simply no reason
 *  yet to special-case it further than `git`/`editor` already are: restoring
 *  it on an orch pane specifically would be exactly as easy to add). A
 *  restored `editor`/`git` never restores their OWN content (which file was
 *  open, which commit was selected) — only the DOCK preference (side +
 *  share), identical to how a restored `group`/`tasks`/`audit` never
 *  restores ITS scroll position or filter either. */
const RESTORABLE_EMBED_KINDS: readonly EmbedKind[] = [
  "tasks",
  // The NEEDS-YOU panel (#1091) is orchestrator-only and group-scoped, exactly
  // like the board it sits beside, so it restores on the same terms — the DOCK
  // preference only, never which card was expanded or half-answered.
  "decisions",
  "audit",
  "group",
  "git",
  "editor",
  // The progress timeline (#608) is group-scoped and gated exactly like the
  // audit log, so it restores on the same terms — dock preference only, never
  // its own window/category selection, the same way a restored audit log does
  // not restore its filters.
  "timeline",
  // The token charts (#2011) are group-scoped and gated exactly like the
  // audit log they read beside, so they restore on the same terms — the
  // DOCK preference only, never the window/metric selection, the same way a
  // restored timeline does not restore its own window or categories.
  "tokens",
];

function isRestorableEmbedKind(
  kind: EmbedKind
): kind is
  | "tasks"
  | "decisions"
  | "audit"
  | "group"
  | "git"
  | "editor"
  | "timeline"
  | "tokens" {
  return (RESTORABLE_EMBED_KINDS as readonly string[]).includes(kind);
}

/** Capture this pane as a serializable record for the persisted layout (#194).
 *  Reads only the retained launch inputs plus the live cwd — no geometry, no
 *  PTY — so it is safe under the no-resize invariant and works even on a hidden
 *  tab. Returns null for a welcome (setup-state) pane: it has no chosen kind
 *  yet, so there is nothing to restore. main.ts pairs these with the flex
 *  weights from grid.layoutSnapshot() to build the PersistedLayoutNode tree;
 *  panerestore.ts decides what each record becomes on restore. */
export function capturePane(pane: Pane): PersistedPane | null {
  if (pane.isWelcome) return null;
  // A dormant restore placeholder persists exactly as it came in, so a session
  // closed without resuming offers the identical restore next boot.
  // …except for the watch (#3319), which is the human's and not the dormant
  // session's: a Reconnect card is exactly the kind of pane someone marks
  // ("come back and resume this one"), and re-emitting the record verbatim
  // would drop that mark on the next capture with nothing to show for it.
  // `isWatched` is seeded FROM this record on restore, so a placeholder
  // nobody touched re-emits the identical value.
  if (pane.dormantRecord) return { ...pane.dormantRecord, watched: pane.watched };
  const kind = pane.liveKind();
  const forked = forkRecordCommand(pane.spawnCommand, pane.spawnArgv, pane.agentSessionId, pane.forkOf);
  return {
    paneKind: kind,
    name: pane.name,
    // An SSH pane records NO cwd (#887 S4). Its local working directory is
    // deliberately home (the repo is on the far end and no local path stands
    // for it — plan part 4a), and its remote directory is the profile's, not a
    // field of this record. A captured local cwd could only ever be a folder
    // the human never picked, restored into a pane that hides it.
    cwd: kind === "ssh" ? null : pane.cwdRaw,
    // #3318 F1: an agent pane's line goes through the one-shot fork
    // discharge on its way into the record. For every pane that is not
    // loomux's own fork — which is every pane but one gesture's output —
    // `forkRecordCommand` returns both halves byte-identical, so this is the
    // same capture it always was. For a fork, it is what stops the record
    // re-forking on the next restart (see `PersistedPane.forkOf`).
    command: kind === "agent" ? forked.command : null,
    argv: kind === "agent" ? forked.argv : null,
    shellKind: kind === "terminal" ? pane.spawnShellKind : null,
    // Capture the session id for orch panes too (#194.5) so a group resume
    // restores exactly the captured members from their own recorded sessions.
    // …and for an SSH pane (#887 S4), where it is the id loomux minted for a
    // REMOTE claude — the one session mechanism that survives the trip through
    // ssh, because it travels on the command line rather than in a local store
    // (`sshMintsSessionId`). A remote copilot/opencode pane captures null here
    // and reconnects to a fresh conversation, honestly.
    sessionId: kind === "agent" || kind === "orch" || kind === "ssh" ? pane.agentSessionId : null,
    // The orchestration role distinguishes the orchestrator from its delegates.
    role: kind === "orch" ? pane.orchRoleName : null,
    // …and the group says WHICH group's orchestrator/delegate it is (#485).
    // A tab can hold panes from two groups, so a whole-group resume that
    // reads the group off the tab attributes them all to one; this is what
    // lets it partition them by their own group instead.
    groupId: kind === "orch" ? pane.orchGroup : null,
    // An editor pane's OPEN FILE (#217) — a path, never a buffer. Without it a pane
    // opened on `src/pane.ts` (and titled after it) restores as a bare tree that
    // names a file it isn't showing. The file is re-read from disk on restore; what
    // was typed and not saved is deliberately not persisted (panerestore.ts).
    // A workflow pane (#222) records the workflow file it is on, for the same reason
    // and on the same terms.
    file:
      kind === "editor"
        ? pane.editorPaneView?.openPathRel ?? null
        : kind === "workflow"
          ? pane.workflowPaneView?.openPathRel ?? null
          : null,
    // The saved CONNECTION an ssh pane belongs to (#887 S4) — the whole of what
    // a reconnect needs, alongside the session id above. Not a command line:
    // see `PersistedPane.sshProfileId` for why the profile is what gets
    // re-derived and a captured argv would be actively wrong to replay.
    sshProfileId: kind === "ssh" ? pane.sshProfileId : null,
    // #2519. Recorded on the AGENT record this lead persists as (see
    // `liveKind`), and the ONE thing restore needs beyond the command line:
    // the group this pane owned is not coming back, a fresh one is minted for
    // it. Gated on `kind === "agent"` as every other per-kind field is, so a
    // pane that is somehow both cannot smuggle the flag onto another kind's
    // record.
    lead: kind === "agent" && pane.isLead,
    // The human's watch (#3319). Deliberately NOT gated on `kind`, unlike
    // every field above it: those are per-kind launch inputs that would be
    // meaningless or actively wrong on another kind's record, and this is a
    // mark the human puts on a PANE. Someone who marks the editor pane
    // holding the file they were mid-way through means it, and a gate would
    // silently drop that watch on restart rather than refuse it visibly.
    watched: pane.watched,
    // #3318 F1. Gated on the agent kind exactly as `lead` above is, and
    // recorded even after the fork flag it gates has been discharged out of
    // the command line above: it is this pane's provenance, and the gate
    // that keeps the discharge idempotent across every later capture.
    forkOf: kind === "agent" ? pane.forkOf : null,
    // Every view CURRENTLY docked (#361), and at what side + share of the
    // split — up to three entries, one per occupied slot. Empty = nothing
    // embedded — every view opens as its floating overlay (the default).
    // Only the orchestration-family kinds (tasks/audit/group) are
    // captured for restore: git/issues are available on every pane kind,
    // but nothing short-lived like a plain terminal restore has the
    // natural "captured, then reapplied once the real pane exists" hook
    // orch panes get from staying dormant — see
    // docs/design/embedded-panels.md's persistence section. The share
    // mirrors how a split's own `weight` is already persisted as a
    // flex-grow ratio rather than a pixel size — not new geometry-
    // persistence territory, the same one grid.layoutSnapshot() occupies.
    embeds:
      kind === "orch" && pane.embeds.embedSlots
        ? EMBED_SIDES.flatMap((side) => {
            const slot = pane.embeds.embedSlots![side];
            return slot.kind !== null && isRestorableEmbedKind(slot.kind)
              ? [{ view: slot.kind, side, share: slot.frac }]
              : [];
          })
        : [],
  };
}
