// Fork lineage (#3368, #3318 F5) — DOM-free, so it is unit-testable under
// `node --test`. main.ts gathers the pointers, the session browser and the pane
// header render what this module derives.
//
// ---------------------------------------------------------------------------
// What is recorded, and what is only ever derived
// ---------------------------------------------------------------------------
//
// orrerix records exactly ONE fact per fork: the parent SESSION it was forked
// from. That pointer has three homes, one per lifetime:
//
//   - `PersistedPane.forkOf` (`tabs.json`) — the live pane's copy, gone when the
//     pane is closed;
//   - `AgentRecord.forked_from` (`agents.json`) — a delegate fork's durable copy;
//   - `SessionRecord.fork_of` (`sessionlog.json`) — a Solo fork's durable copy,
//     written once when the fork's session is first recorded, so a closed Solo
//     fork keeps its place in the tree the way a delegate does.
//
// The LINEAGE — the chain from a session back to the conversation it all began
// as — is never stored. It is walked here, parent pointer by parent pointer,
// every time it is asked for. A stored chain would be a second copy of those
// pointers that could disagree with them, and would have to be rewritten when an
// ancestor's record is evicted; a walk cannot disagree with what it walks.
//
// A CLI's own fork record (pi's `parentId`, opencode's `parent_id`) is NOT read.
// orrerix's record is the source of truth for every forkable CLI alike: opencode
// does not even set `parent_id` on a fork (`docs/design/session-fork.md` L3), and
// a lineage that read vendor fields would mean something different per CLI.

/** Where a parent pointer was read from. Order is precedence (`buildForkIndex`). */
export type PointerSource = "pane" | "roster" | "log";

/** One recorded "this session was forked from that one". */
export interface ForkPointer {
  child: string;
  parent: string;
  source: PointerSource;
}

/** The pointers, joined. `known` is every session anything has a record of — a
 *  browser row, an open pane, a roster row, a sessions-log record — which is
 *  what separates a parent that is merely a ROOT from one nobody knows at all. */
export interface ForkIndex {
  parentOf: ReadonlyMap<string, string>;
  /** Direct children per parent, in pointer order. */
  childrenOf: ReadonlyMap<string, readonly string[]>;
  known: ReadonlySet<string>;
  /** Children whose sources named two DIFFERENT parents. The first source in
   *  precedence order wins; the loser is kept here rather than silently
   *  dropped, so a disagreement is visible to whoever looks. */
  conflicts: readonly { child: string; kept: string; ignored: string; source: PointerSource }[];
}

const PRECEDENCE: Record<PointerSource, number> = { pane: 0, roster: 1, log: 2 };

/** Join every source's pointers into one index.
 *
 *  **Precedence: pane, then roster, then log.** The three copies are written
 *  from one value at one gesture, so they agree by construction; the order only
 *  decides a disagreement nobody should ever see (a hand-edited file), and the
 *  live pane — the one the human is looking at — is the one it keeps.
 *
 *  Blank ids are dropped, and a self-pointer (`child === parent`) is dropped as
 *  the one-node cycle it is, without being recorded as a parent at all. */
export function buildForkIndex(pointers: Iterable<ForkPointer>, known: Iterable<string>): ForkIndex {
  const sorted = [...pointers]
    .map((p, i) => ({ p, i }))
    .sort((a, b) => PRECEDENCE[a.p.source] - PRECEDENCE[b.p.source] || a.i - b.i)
    .map(({ p }) => p);
  const parentOf = new Map<string, string>();
  const childrenOf = new Map<string, string[]>();
  const knownSet = new Set<string>();
  const conflicts: { child: string; kept: string; ignored: string; source: PointerSource }[] = [];
  for (const id of known) if (id) knownSet.add(id);
  for (const { child, parent, source } of sorted) {
    if (!child || !parent) continue;
    knownSet.add(child);
    if (child === parent) continue;
    const had = parentOf.get(child);
    if (had !== undefined) {
      if (had !== parent) conflicts.push({ child, kept: had, ignored: parent, source });
      continue;
    }
    parentOf.set(child, parent);
    const kids = childrenOf.get(parent) ?? [];
    kids.push(child);
    childrenOf.set(parent, kids);
  }
  return { parentOf, childrenOf, known: knownSet, conflicts };
}

/** How a walk ended.
 *
 *  - `root`: the last session has no parent — the conversation it all began as.
 *  - `dangling`: the last session names a parent nothing has a record of (its
 *    transcript deleted, its sessions-log record evicted). The chain stops at the
 *    last KNOWN node and says which id it could not follow.
 *  - `cycle`: a pointer leads back into the chain. Impossible by construction — a
 *    fork's child id is always newly minted — so it is refused rather than looped
 *    on: `at` is the id that repeated, and the chain holds each node once. */
export type LineageEnd =
  | { kind: "root" }
  | { kind: "dangling"; missing: string }
  | { kind: "cycle"; at: string };

export interface Lineage {
  /** The session asked about first, then its parent, grandparent… root-ward.
   *  Every id in it is KNOWN, except the first when it is not (asking about an
   *  unknown id yields a one-node chain ending `root` if it has no pointer). */
  chain: string[];
  end: LineageEnd;
}

/** Walk parent pointers from `id` toward the root. */
export function lineageOf(index: ForkIndex, id: string): Lineage {
  const chain = [id];
  const seen = new Set([id]);
  let cur = id;
  for (;;) {
    const parent = index.parentOf.get(cur);
    if (parent === undefined) return { chain, end: { kind: "root" } };
    if (seen.has(parent)) return { chain, end: { kind: "cycle", at: parent } };
    if (!index.known.has(parent)) return { chain, end: { kind: "dangling", missing: parent } };
    chain.push(parent);
    seen.add(parent);
    cur = parent;
  }
}

/** The parent to show and to "return to" for `id`, or why there is none.
 *
 *  `null` for a session that is not a fork. A fork whose walk ends in a cycle
 *  AT ITS OWN FIRST STEP has no trustworthy parent and is reported as such, the
 *  same refusal `lineageOf` makes — a parent chosen out of a loop would be one
 *  of two sessions, arbitrarily. */
export type ParentLink =
  | null
  | { kind: "known"; parent: string }
  | { kind: "dangling"; parent: string }
  | { kind: "cycle"; parent: string };

export function parentLink(index: ForkIndex, id: string): ParentLink {
  const parent = index.parentOf.get(id);
  if (parent === undefined) return null;
  const walk = lineageOf(index, id);
  if (walk.end.kind === "cycle") return { kind: "cycle", parent };
  if (!index.known.has(parent)) return { kind: "dangling", parent };
  return { kind: "known", parent };
}

/** One row of the session browser's fork tree. */
export interface ForkTreeRow {
  id: string;
  /** 0 for a row at the top of the list; 1 under its parent, 2 under that… */
  depth: number;
  /** Direct forks of this session that are in the list — what the "show forks"
   *  expander counts. Forks filtered out of the list are not counted: the
   *  expander promises rows it can reveal. */
  forks: number;
  /** Whether this row's forks are shown under it. Meaningful only when
   *  `forks > 0`. */
  expanded: boolean;
}

/** Arrange a list of session ids (already in display order) into a fork tree.
 *
 *  - A session sits under its DIRECT parent when that parent is in the list;
 *    otherwise at the top level, in its own display position. The nearest
 *    listed ANCESTOR is deliberately not used: the row says "fork of <parent>",
 *    and indenting it under its grandparent would contradict its own label.
 *  - A parent's forks are emitted only while it is expanded (`expanded` holds
 *    ids, or `expandAll` — the browser passes that while a filter is typed, so a
 *    fork the human searched for is never hidden behind a collapsed parent).
 *  - Siblings keep display order.
 *  - A session in a pointer cycle is placed at the top level: every member of a
 *    loop has its parent in the list, so none would ever be reached from a root,
 *    and a row the human cannot see is the one outcome worse than a flat one.
 *
 *  Every input id appears in the output at most once, and every input id
 *  appears exactly once when everything is expanded. */
export function forkTreeRows(
  ids: readonly string[],
  index: ForkIndex,
  expanded: ReadonlySet<string>,
  expandAll = false
): ForkTreeRow[] {
  const listed = new Set(ids);
  const treeParent = new Map<string, string>();
  for (const id of ids) {
    const link = parentLink(index, id);
    if (link?.kind === "known" && listed.has(link.parent)) treeParent.set(id, link.parent);
  }
  const kids = new Map<string, string[]>();
  for (const id of ids) {
    const p = treeParent.get(id);
    if (p === undefined) continue;
    const list = kids.get(p) ?? [];
    list.push(id);
    kids.set(p, list);
  }
  const out: ForkTreeRow[] = [];
  const emitted = new Set<string>();
  const emit = (id: string, depth: number): void => {
    if (emitted.has(id)) return;
    emitted.add(id);
    const children = kids.get(id) ?? [];
    const open = expandAll || expanded.has(id);
    out.push({ id, depth, forks: children.length, expanded: open });
    if (open) for (const c of children) emit(c, depth + 1);
  };
  for (const id of ids) if (!treeParent.has(id)) emit(id, 0);
  return out;
}

/** Read the pointers out of the three records (the three homes named at the
 *  top of this file) — the one place that knows which field of which record is
 *  a parent pointer, so main.ts hands over records and never picks fields.
 *
 *  A pane contributes only when it knows both its session and its parent; a
 *  codex/opencode fork before its child id is learned has no session to key a
 *  pointer on yet, and its crumb reads `Pane.forkedFrom` directly instead. */
export function gatherForkPointers(records: {
  panes?: Iterable<{ sessionId: string | null; forkOf: string | null }>;
  roster?: Iterable<{ session_id: string; forked_from?: string | null }>;
  log?: Iterable<[string, { fork_of?: string }]>;
}): ForkPointer[] {
  const out: ForkPointer[] = [];
  for (const p of records.panes ?? []) {
    if (p.sessionId && p.forkOf) out.push({ child: p.sessionId, parent: p.forkOf, source: "pane" });
  }
  for (const r of records.roster ?? []) {
    if (r.session_id && r.forked_from) out.push({ child: r.session_id, parent: r.forked_from, source: "roster" });
  }
  for (const [id, rec] of records.log ?? []) {
    if (id && rec.fork_of) out.push({ child: id, parent: rec.fork_of, source: "log" });
  }
  return out;
}

/** What a session is called, for a crumb or a "fork of …" line. The first
 *  source that has a non-blank answer wins: the open pane's name (the human's
 *  latest word), the sessions log's recorded pane name, the roster's agent
 *  name, the CLI store's transcript title — and, with none, the short id, so a
 *  dangling parent is still NAMED rather than blank. */
export function sessionDisplayName(
  id: string,
  sources: {
    paneName?: (id: string) => string | undefined;
    loggedName?: (id: string) => string | undefined;
    agentName?: (id: string) => string | undefined;
    title?: (id: string) => string | undefined;
  }
): string {
  for (const read of [sources.paneName, sources.loggedName, sources.agentName, sources.title]) {
    const v = read?.(id)?.trim();
    if (v) return v;
  }
  return `session ${id.slice(0, 8)}`;
}

/** The pane-header crumb's label for a forked pane. */
export function forkCrumbLabel(parentName: string): string {
  return `↰ ${parentName}`;
}
