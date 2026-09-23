// The fork-name prompt's decision (#3368, #3318 F5) — DOM-free, so the three
// ways out of the prompt are unit-testable. The overlay itself is DOM wiring in
// `src/forkprompt.ts` and is validated by hand.
//
// The name a fork is given here is its PANE NAME — the one `PaneOptions.name`
// sets at open and the header's rename (F2) edits afterwards, persisted in
// `tabs.json`, recorded in `sessionlog.json` and, for a delegate, the roster's
// agent name. It is not a second name: there is no fork-name field anywhere.

/** The most characters a pane name keeps — the frontend's copy of the `.take(40)`
 *  in the backend's `sanitize_agent_name`, which every name a spawn, a rename or
 *  an agent's `rename_agent` carries already goes through.
 *  `test/forklineage.test.ts` reads that Rust function off disk and requires the
 *  two to agree, so the cap cannot move on one side alone. */
export const PANE_NAME_MAX_CHARS = 40;

/** One rule for every pane name orrerix is handed (#3368 review): the backend's
 *  `sanitize_agent_name`, applied where a Solo fork's name never reaches it —
 *  trim, drop control characters, keep the first `PANE_NAME_MAX_CHARS`
 *  characters. Characters are Unicode scalar values, as Rust's `chars()` counts
 *  them (`[...s]`, never `s.length`, which counts UTF-16 units). May return "":
 *  the caller decides what an empty name means. */
export function sanitizePaneName(raw: string): string {
  return [...raw.trim()]
    .filter((c) => !/\p{Cc}/u.test(c))
    .slice(0, PANE_NAME_MAX_CHARS)
    .join("");
}

/** How the prompt was left. */
export type ForkPromptExit = "enter" | "escape" | "cancel";

/** What the gesture does next: fork under this name, or not fork at all. */
export type ForkNameDecision = { fork: true; name: string } | { fork: false };

/** Decide what leaving the prompt means (#3368 AC 1).
 *
 *  - **Enter** forks under what is in the box, trimmed; an EMPTY box forks under
 *    the default — the human pressed Enter to fork, and a pane with no name is
 *    not something any other path in orrerix creates.
 *  - **Escape** forks under the DEFAULT, whatever was typed. That is the issue's
 *    rule: the prompt is an optional rename, not a confirmation, so the quick
 *    way past it still forks.
 *  - **cancel** (the prompt's ✕, or a click outside it) does not fork. It is the
 *    only way out that does not, and it is a pointer gesture on purpose — no
 *    key a human presses to get past the prompt can lose them the fork.
 *
 *  Whitespace runs inside the name collapse to one space: a name is one line of
 *  header chrome, and a pasted newline would otherwise put a line break in the
 *  title, the browser row and the roster. Then `sanitizePaneName`, the rule every
 *  other pane name goes through. */
export function forkNameDecision(exit: ForkPromptExit, typed: string, fallback: string): ForkNameDecision {
  if (exit === "cancel") return { fork: false };
  if (exit === "escape") return { fork: true, name: fallback };
  const name = sanitizePaneName(typed.replace(/\s+/g, " "));
  return { fork: true, name: name || fallback };
}
