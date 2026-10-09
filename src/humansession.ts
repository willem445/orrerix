// The solo and lead pane's session report (#3831): one backend command that
// records a session id the backend does not yet hold, and the one decision
// every caller goes through. A pane reports wherever its identity is set and
// from the session-identified hook, so a restored or launched pane is covered
// whichever order its identity and its session arrive in. The decision itself
// is pure and lives in cacheage.ts; this module is the IPC half.

import { invoke } from "./transport.ts";
import { sessionToReport, type HumanPaneIdentity, type ReportedSession } from "./cacheage";

/** The pane fields the session report reads. A `Pane` satisfies it, so this module
 *  does not import pane.ts, and so is not part of the pane's import cycle. */
export interface SessionReportPane {
  readonly sessionId: string | null;
  readonly orchRole: string | null;
  readonly orchAgentId: string | null;
  readonly channelAgentRole: string | null;
  readonly channelAgentAgentId: string | null;
}

/** Record a solo or lead pane's session id (#3831), so the cache-age chip can
 *  read its usage from its own transcript. Callers go through
 *  `reportHumanSession`, which decides whether a pane has anything to report.
 *  The backend answers the same id again with success, and refuses a different
 *  id on record, a delegate, and an id that is not one path component. A refusal
 *  is not an error to the human: the pane keeps the chip it has. */
export const humanPaneSession = (agentId: string, sessionId: string): Promise<void> =>
  invoke<void>("orch_human_pane_session", { agentId, sessionId });

/** The session each pane has reported, keyed by the pane, so a pane reports each
 *  (agent, session) pair once (#3831). */
const humanSessionReported = new WeakMap<object, ReportedSession>();

/** A solo or lead pane's identity for the session report, read off the pane's own
 *  fields (#3831). A restored pane needs no special case: its identity is set the
 *  way a launched one's is. */
function humanIdentityOf(pane: SessionReportPane): HumanPaneIdentity | null {
  if (pane.orchRole === "lead" && pane.orchAgentId !== null) {
    return { agentId: pane.orchAgentId, role: "lead" };
  }
  if (pane.channelAgentRole === "solo" && pane.channelAgentAgentId !== null) {
    return { agentId: pane.channelAgentAgentId, role: "solo" };
  }
  return null;
}

/** Report a solo or lead pane's session to the backend, when the pane has one the
 *  backend does not already hold (#3831). Called wherever the pane's identity is
 *  set, and from the session-identified hook. So a session known when the identity
 *  is set is reported then, and one learned later is reported by the hook. Best-
 *  effort, and a refusal is not surfaced. */
export function reportHumanSession(pane: SessionReportPane): void {
  const report = sessionToReport({
    humanIdentity: humanIdentityOf(pane),
    sessionId: pane.sessionId,
    reported: humanSessionReported.get(pane) ?? null,
  });
  if (report === null) return;
  humanPaneSession(report.agentId, report.sessionId).then(
    () => humanSessionReported.set(pane, report),
    () => {
      /* best-effort — the backend refused it, and the pane's chip reads what it has */
    }
  );
}
