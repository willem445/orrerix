// Typed forks (#3898): the DOM-side half of opening one — typing copilot's
// `/fork <marker>` into a just-opened Solo fork pane. The pure half (the
// marker) is `src/typedfork.ts`; the backend door is
// `OrchRegistry::deliver_solo_fork`. Validated by hand, like the rest of the
// pane-open glue in `main.ts` (`openForkedPane`), which calls this.
//
// Design note: docs/design/session-fork.md, "copilot: a typed fork".

import { invoke } from "./transport.ts";
import type { Pane } from "./pane";
import { confirmSoloCopilotAutopilot, soloBind } from "./orchestration";
import { shouldWatchCopilotOnRestore } from "./panerestore";
import { showToast } from "./toast";

/** Type a typed fork's command into a just-opened Solo fork pane, through the
 *  backend's delivery pipeline (readiness wait, copilot's focus-in prefix, echo
 *  check, submit confirm) rather than a raw pty write. The backend builds the
 *  text from its own row and refuses anything but a bound Solo pane on a
 *  typed-fork CLI and a marker in `forkMarker`'s grammar. Resolves once the
 *  delivery is QUEUED; the queue's own badge reports one that never lands. */
export const orchSoloForkDeliver = (agentId: string, parentSession: string, marker: string): Promise<void> =>
  invoke("orch_solo_fork_deliver", { agentId, parentSession, marker });

/** Type `/fork <marker>` into THIS pane — never the source — once it is open.
 *
 *  The order is the contract: the solo bind is AWAITED (the remint's own `bind`
 *  thunk is fire-and-forget, and the backend refuses to type into an agent with
 *  no pty); the copilot autopilot watcher starts for an autopilot line exactly
 *  as a restored pane's does (the consent dialog opens on the first submit, and
 *  this `/fork` is that submit); and `noteTypedFork` stamps the marker and the
 *  reconciler's eligibility clock BEFORE the delivery is queued — the child
 *  cannot exist earlier, and must not be missed because the clock was late.
 *
 *  On any failure nothing was queued, so the pane is resuming the parent and
 *  will never be told to fork: `close` removes it rather than leave two panes
 *  in one session, and the human is told. That includes a pane whose Solo
 *  identity could not be minted — best-effort for every other fork, but here
 *  the identity is the only thing that can type the `/fork`, so without one
 *  the "fork" would be a second pane in the parent's session. Resolves whether
 *  the fork is under way. */
export async function typeForkInto(
  pane: Pane,
  remint: { command?: string; argv?: string[]; channelAgent?: { agentId: string } },
  forkOf: string,
  marker: string,
  close: () => void
): Promise<boolean> {
  const agentId = remint.channelAgent?.agentId;
  let why = "the new pane has no terminal identity to type /fork through.";
  if (agentId && pane.ptyId !== null) {
    try {
      await soloBind(agentId, pane.ptyId);
      if (shouldWatchCopilotOnRestore(remint.command ?? null, remint.argv ?? null)) {
        void confirmSoloCopilotAutopilot(pane.ptyId, "copilot").catch(() => {
          /* best-effort — see confirmSoloCopilotAutopilot's own doc comment */
        });
      }
      pane.noteTypedFork(marker);
      await orchSoloForkDeliver(agentId, forkOf, marker);
      return true;
    } catch (err) {
      why = `${String(err)}.`;
    }
  }
  close();
  showToast(`Fork failed: ${why} The new pane was closed; the original is untouched.`, "error");
  return false;
}
