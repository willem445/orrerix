// Typed forks (#3898): the pure half of forking a CLI whose fork is an
// INTERACTIVE command rather than a launch flag — copilot's `/fork [NAME]`.
//
// copilot's command reference (checked 2026-11-17) documents `/fork [NAME]` as
// "Fork the current session into a new session, optionally with a name. Usable
// while the agent is running—the source session keeps working in the
// background", and lists no argv fork at all. So the pane that TYPES `/fork`
// becomes the child; typed into the source pane it would invert the roster.
// orrerix opens a NEW pane on copilot's plain resume of the parent and types
// `/fork <marker>` into that pane instead (`src/forkopen.ts`, the DOM glue;
// `OrchRegistry::deliver_solo_fork`, the backend door). This module is the
// half a test can reach: the marker the child is named, and so learned, by.
//
// Design note: docs/design/session-fork.md, "copilot: a typed fork".

/** The child's NAME for a typed fork, and the handle its session id is learned
 *  by. `/fork [NAME]` names the new session; copilot records the name in the
 *  session's `workspace.yaml`, which the session scan reads as the row's title;
 *  and the reconciler adopts, for this pane, only a session carrying exactly
 *  this title (`ReconcilePane.title`). So the match is by identity rather than
 *  by "the one new copilot session in this folder", and a sibling copilot pane
 *  in the same folder cannot be recorded in the child's place (#3888).
 *
 *  ONE token of `[a-z0-9-]`, because the reference documents `/fork [NAME]`
 *  and is silent on how NAME is parsed: nothing that could split it or be
 *  quoted differently reaches the pane. The pane's own name, slugged and capped
 *  at 40, keeps it readable in copilot's own `/resume` picker; the `-<nonce>`
 *  tail makes two forks of one pane differ, and means the marker is never the
 *  bare word `worktree` — `/fork worktree` is a different gesture (a fork into
 *  a new git worktree). The backend re-checks the same grammar
 *  (`slash_fork_text`) and refuses rather than rewrites, since the frontend
 *  matches on exactly this string later. */
export function forkMarker(paneName: string, nonce: string): string {
  const slug = paneName
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40)
    .replace(/-+$/, "");
  const tail = nonce.toLowerCase().replace(/[^0-9a-f]/g, "").slice(0, 12) || "0";
  return `${slug || "fork"}-${tail}`;
}

/** A fresh marker nonce: six hex digits off the webview's Web Crypto (the mint
 *  `forkPaneSession` already uses for a child id — not a getrandom crate,
 *  which CLAUDE.md constraint 2 governs in `src-tauri` only). */
export function forkMarkerNonce(): string {
  return crypto.randomUUID().replace(/-/g, "").slice(0, 6);
}
