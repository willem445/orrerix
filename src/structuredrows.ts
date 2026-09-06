// The structured pane's ROW MODEL: everything the DOM renderer decides that a
// `node --test` can decide too (#2891 S4).
//
// `structuredview.ts` (S2) is the projection — events in, a block list out.
// `structuredpane.ts` is the DOM. This module is the layer between them, and it
// exists for one reason: the renderer's real decisions are arithmetic and
// classification, not markup, and arithmetic is testable while `document` is
// not. The repo's rule ("frontend logic that needs tests is extracted into
// DOM-free pure modules") applied to a renderer means the renderer is left
// holding element creation and nothing else.
//
// Every import here is `import type`, so nothing resolves at runtime and this
// module loads under `node --test` off disk exactly as `structuredview.ts` does.
//
// THE THREE THINGS THIS MODULE OWNS, and why each is here rather than there:
//
//  1. **The virtualisation window** (`computeWindow`). #2891's requirement is
//     that thousands of blocks must not cost thousands of nodes. That is a scan
//     over a height array — arithmetic over numbers, once per frame, with no
//     element in sight. Put it in the renderer and the only way to test it is a
//     fake DOM, which this repo does not do.
//  2. **The gutter segment** (`segmentOf`). `demo/structured-pane/DESIGN.md` §3:
//     the warp gutter's vocabulary is the six state dyes AND NOTHING ELSE, and
//     anything that is not an agent state is marked by FORM. That is a total
//     function from a block to one of six names, which is exactly the shape a
//     test can pin exhaustively — and a channel violation is a thing a reviewer
//     should be able to see fail, not have to spot in a stylesheet.
//  3. **The identifying argument** (`argLine`). §6: the one line that makes a
//     COLLAPSED card worth reading. Which key identifies a call depends on the
//     tool — a search by its pattern, a read by its path — and taking whichever
//     key comes first renders every `Grep` in a session as the same word.
//
// WHAT IS DELIBERATELY NOT HERE: anything that reads a colour. The renderer
// names a `data-seg` and the stylesheet paints it from a `--state-*` token, so
// there is no hex in this file and no second copy of the palette to drift from
// `src/theme.ts`. `test/theme.test.ts` watches the stylesheet; nothing needs to
// watch this.

import type {
  Block,
  RequestBlock,
  State,
  ToolBlock,
  TurnBlock,
  UiAnswer,
  ViewState,
} from "./structuredview";

// ── the warp gutter ─────────────────────────────────────────────────────────

/** The six state dyes, and the ONLY vocabulary the transcript gutter has
 *  (`DESIGN.md` §3). Spelled as a closed union rather than a string so a
 *  seventh meaning is a compile error and an edit to `ui-redesign.md` first. */
export type Segment = "working" | "thinking" | "attention" | "ok" | "danger" | "idle";

export const SEGMENTS: readonly Segment[] = [
  "working",
  "thinking",
  "attention",
  "ok",
  "danger",
  "idle",
] as const;

/**
 * Which dye a block's gutter segment carries.
 *
 * TOTAL over `Block`, and that is the property worth having: a new block kind
 * cannot reach the gutter without somebody choosing its dye, because the
 * `switch` has no `default` and TypeScript's exhaustiveness check is what
 * refuses the omission.
 *
 * The events that are NOT an agent state — a delivery arriving, a turn
 * boundary, the ring notice — all take `idle` and are told apart by FORM in the
 * stylesheet (a glyph, a full-width hairline, a dashed rule). That is §3's
 * rule, and it is why `delivery` does not get a hue of its own however much a
 * seam wants one.
 *
 * `thinking` is the one segment name that is not a `--state-*` token: the
 * stylesheet paints it from `--state-held`, the achromatic one, because
 * thinking is the block the eye should be able to skip and a dye would pull the
 * eye to it. It is named separately here so the renderer never has to ask
 * "which state is thinking?" twice.
 */
export function segmentOf(b: Block): Segment {
  switch (b.kind) {
    case "text":
      return "working";
    case "thinking":
      return "thinking";
    case "tool":
      return b.status === "ok"
        ? "ok"
        : b.status === "error"
          ? "danger"
          : b.status === "running"
            ? "working"
            : "idle";
    case "request":
      // A settled request keeps a dye that says how it went; a pending one is
      // the whole reason the attention channel exists.
      if (!b.settled) return "attention";
      return answerWasRefusal(b.settled.answer) ? "danger" : "ok";
    case "turn":
      return b.ended ? "idle" : "working";
    case "notice":
      return b.level === "error" ? "danger" : "idle";
    case "delivery":
    case "evicted":
      return "idle";
  }
}

/** Whether a settled answer reads as a refusal — a denial, a cancel, a `false`.
 *  Exported because the card's own chip asks the same question the gutter does,
 *  and two spellings of one question is how they drift apart. */
export function answerWasRefusal(answer: UiAnswer | string): boolean {
  if (typeof answer === "string") {
    const s = answer.toLowerCase();
    return s === "cancelled" || s === "deny" || s === "denied" || s === "refuse";
  }
  if ("Confirmed" in answer) return !answer.Confirmed;
  if ("Value" in answer) {
    const s = String(answer.Value).toLowerCase();
    return s === "deny" || s === "denied" || s === "no" || s === "cancel" || s === "cancelled";
  }
  return false;
}

// ── the row list ────────────────────────────────────────────────────────────

/** What the renderer needs to draw one row WITHOUT re-deriving it per frame.
 *  One per block, in block order; `key` is the block id, which is stable across
 *  re-projection (`structuredview.ts`'s id counter) and is what lets a fold, a
 *  measured height and a DOM node survive a batch. */
export interface RowSpec {
  key: string;
  kind: Block["kind"];
  segment: Segment;
  /** This row is receiving bytes right now — what the streaming caret follows,
   *  and the one continuous cue §5's motion budget allows for text. */
  live: boolean;
  /** Folded, by the human or by the `dimThinking` switch. Read from
   *  `ViewState`, never from a DOM element — the renderer rebuilds its elements
   *  from the model on every batch and would lose a fold kept on one. */
  collapsed: boolean;
  /** First guess at this row's pixel height, used until the row has been in the
   *  DOM once and measured. Never a constant: a 400-line tool output and a
   *  one-line notice are two orders of magnitude apart, and one estimate for
   *  both makes the scrollbar jump every time a row is measured. */
  estimate: number;
}

/** Pixel estimates, in one table so a tuning change is one diff and a reviewer
 *  can see what each row is assumed to cost. Deliberately rough — the measured
 *  height replaces it the first time the row is rendered. */
export const ROW_ESTIMATE = {
  line: 22,
  card: 34,
  cardOpen: 120,
  turn: 30,
  delivery: 84,
  request: 110,
  requestSettled: 56,
} as const;

/** Characters per line assumed when estimating an unmeasured text block. A
 *  guess by construction — the real wrap depends on the pane's width and the
 *  font's metrics, neither of which a pure module may read. It only has to be
 *  the right ORDER, because measurement corrects it on first paint. */
export const ESTIMATE_COLS = 74;

function estimateText(text: string, lineHeight: number): number {
  let lines = 0;
  for (const raw of text.split("\n")) lines += Math.max(1, Math.ceil(raw.length / ESTIMATE_COLS));
  return Math.max(lineHeight, lines * lineHeight);
}

/**
 * The row list for a projection, in block order.
 *
 * PURE and cheap: one pass over the blocks, no allocation per event. It runs
 * once per rendered frame rather than once per event, which is the coalescing
 * constraint `DESIGN.md` §7 established one layer up — "a producer may not make
 * the consumer pay per event" — expressed at this layer.
 */
export function rowsFor(state: State, view: ViewState): RowSpec[] {
  const rows: RowSpec[] = [];
  for (const b of state.blocks) {
    const collapsed = view.collapsed.has(b.id) || (b.kind === "thinking" && view.dimThinking);
    rows.push({
      key: b.id,
      kind: b.kind,
      segment: segmentOf(b),
      live: isLive(b, state),
      collapsed,
      estimate: estimateRow(b, collapsed),
    });
  }
  return rows;
}

/**
 * Is this row producing bytes right now?
 *
 * Read off the PROJECTION's own open-block pointers rather than off "is it the
 * last block", because the last block of a finished turn is not live and a
 * caret parked on it would report a state that ended. The caret is the
 * live-text analogue of the attention pulse (§5): it reports a state and stops
 * the instant that state ends.
 */
function isLive(b: Block, state: State): boolean {
  switch (b.kind) {
    case "text":
      return b.id === state.openText;
    case "thinking":
      return b.id === state.openThinking;
    case "tool":
      return b.status === "running";
    case "request":
      return b.settled === null;
    default:
      return false;
  }
}

/** A card that opens itself: a failed call, and a call streaming output right
 *  now. Both are §6's "nobody should have to click to find out why something
 *  broke" — the second because a command producing bytes IS what the human is
 *  watching. Suppressed once the human has folded it by hand, which is why this
 *  takes the fold set rather than deciding alone. */
export function autoOpen(b: Block, view: ViewState): boolean {
  if (b.kind !== "tool") return false;
  if (view.collapsed.has(b.id)) return false;
  return b.status === "error" || b.isError || (b.status === "running" && b.output.length > 0);
}

function estimateRow(b: Block, collapsed: boolean): number {
  switch (b.kind) {
    case "text":
      return estimateText(b.text, ROW_ESTIMATE.line);
    case "thinking":
      return collapsed
        ? ROW_ESTIMATE.card
        : ROW_ESTIMATE.card + estimateText(b.text, ROW_ESTIMATE.line);
    case "tool":
      return collapsed ? ROW_ESTIMATE.card : ROW_ESTIMATE.cardOpen;
    case "turn":
      return ROW_ESTIMATE.turn;
    case "delivery":
      return ROW_ESTIMATE.delivery;
    case "request":
      return b.settled ? ROW_ESTIMATE.requestSettled : ROW_ESTIMATE.request;
    case "notice":
    case "evicted":
      return ROW_ESTIMATE.line;
  }
}

// ── virtualisation ──────────────────────────────────────────────────────────

/** The slice of rows that must exist as DOM nodes, plus the two spacer heights
 *  that keep the scrollbar honest about the rows that do not. */
export interface WindowSpec {
  /** First row index in the DOM, inclusive. */
  start: number;
  /** Last row index in the DOM, EXCLUSIVE. `start === end` is legal and means
   *  nothing is in view, never "render everything". */
  end: number;
  /** Pixels of nothing above `start`, and below `end`. */
  padTop: number;
  padBottom: number;
  /** Every row's height summed — what the scrollbar is scaled to. */
  total: number;
}

/** How far outside the viewport rows are kept alive. Enough that a normal
 *  wheel tick lands on rows that already exist (so the frame after a scroll has
 *  nothing to build), and small enough that the DOM stays O(viewport). */
export const OVERSCAN_PX = 600;

/**
 * Which rows to build.
 *
 * A linear scan over the height array, not a maintained prefix-sum table: with
 * `MAX_BLOCKS` at 2000 this is at most 2000 additions once per frame — tens of
 * microseconds — while a prefix-sum table has to be INVALIDATED every time a
 * row is measured or a block's height changes, which is most frames. The cheap
 * correct thing beats the clever stale thing, and the ceiling that makes it
 * cheap is a ceiling the projection already enforces.
 *
 * `heights[i] <= 0` is treated as `0` rather than rejected: a row measured
 * while its pane was `display:none` really does report zero, and refusing it
 * would turn a hidden tab into a thrown exception.
 */
export function computeWindow(
  heights: readonly number[],
  scrollTop: number,
  viewportHeight: number,
  overscanPx: number = OVERSCAN_PX,
): WindowSpec {
  const at = Math.max(0, scrollTop);
  const top = at - overscanPx;
  const bottom = at + Math.max(0, viewportHeight) + overscanPx;

  let total = 0;
  for (let i = 0; i < heights.length; i += 1) total += Math.max(0, heights[i] ?? 0);

  let start = -1;
  let end = heights.length;
  let padTop = 0;
  let y = 0;
  for (let i = 0; i < heights.length; i += 1) {
    const h = Math.max(0, heights[i] ?? 0);
    if (start === -1 && y + h > top) {
      start = i;
      padTop = y;
    }
    if (start !== -1 && y >= bottom) {
      end = i;
      break;
    }
    y += h;
  }
  if (start === -1) {
    // Scrolled past every row (a shrinking transcript, or an empty one). The
    // window is empty and the whole height is above it — never "draw the last
    // row anyway", which would put a row on screen that is not where the
    // scrollbar says it is.
    return { start: heights.length, end: heights.length, padTop: total, padBottom: 0, total };
  }

  let below = 0;
  for (let i = end; i < heights.length; i += 1) below += Math.max(0, heights[i] ?? 0);
  return { start, end, padTop, padBottom: below, total };
}

/**
 * Is the view following the live end?
 *
 * The gap is measured in pixels rather than by `scrollTop === scrollHeight -
 * clientHeight`, because sub-pixel layout means that equality is false on a
 * view that is visually pinned. `slack` is about one row's worth.
 */
export function pinnedAtBottom(
  scrollTop: number,
  scrollHeight: number,
  clientHeight: number,
  slack = 24,
): boolean {
  return scrollHeight - scrollTop - clientHeight <= slack;
}

// ── the identifying argument ────────────────────────────────────────────────

/**
 * Which argument identifies a call, per tool.
 *
 * A search is identified by its pattern and a read by its path. Taking
 * whichever key happens to come first in one fixed list renders every `Grep` in
 * a session as the same word (`"src"`) and throws the identifying half away —
 * which is the failure `demo/structured-pane/render.js` records having made.
 *
 * Tool names are matched case-insensitively because the two harnesses spell
 * them differently: Claude Code emits `Bash`, pi emits `bash`. That is a
 * VOCABULARY difference between harnesses, not a per-CLI branch on a name — the
 * table has one row per tool, and no arm of it asks which CLI is running.
 */
const ARG_KEYS: Record<string, readonly string[]> = {
  grep: ["pattern", "path"],
  glob: ["pattern", "path"],
  bash: ["command"],
  bashoutput: ["command"],
  webfetch: ["url"],
  websearch: ["query"],
  task: ["description", "prompt"],
  agent: ["description", "prompt"],
};
const ARG_FALLBACK: readonly string[] = ["file_path", "path", "command", "pattern", "url", "query"];

export function argLine(name: string | null, input: unknown): string {
  if (!input || typeof input !== "object" || Array.isArray(input)) return "";
  const obj = input as Record<string, unknown>;
  const keys = name ? (ARG_KEYS[name.toLowerCase()] ?? ARG_FALLBACK) : ARG_FALLBACK;
  for (const k of keys) {
    const v = obj[k];
    if (typeof v === "string" && v) return v;
  }
  const first = Object.keys(obj)[0];
  if (first === undefined) return "";
  return `${first}=${JSON.stringify(obj[first]).slice(0, 60)}`;
}

/** A shell call reads command-first (§6), so it takes a different shape from a
 *  tool card. Same case-insensitivity, same reason. */
export function isShellTool(name: string | null): boolean {
  return name !== null && name.toLowerCase() === "bash";
}

export function isEditTool(name: string | null): boolean {
  if (name === null) return false;
  const n = name.toLowerCase();
  return n === "edit" || n === "write" || n === "multiedit";
}

// ── the chip ────────────────────────────────────────────────────────────────

export interface Chip {
  status: "pending" | "running" | "ok" | "error";
  label: string;
  /** A spinner rather than a dot — the one continuous cue a running call gets
   *  (§5's motion budget), and it stops the instant the call returns. */
  spinning: boolean;
}

/** What a tool card's status chip says. */
export function chipFor(b: ToolBlock): Chip {
  if (b.status === "pending") return { status: "pending", label: "queued", spinning: false };
  if (b.status === "running") return { status: "running", label: "running", spinning: true };
  const ok = b.status === "ok";
  return { status: ok ? "ok" : "error", label: ok ? "ok" : "error", spinning: false };
}

// ── figures ─────────────────────────────────────────────────────────────────

/** Token counts, abbreviated. `null` renders as an em dash — "unknown is not a
 *  value" (§1.3), so a figure nobody reported is not `0`. */
export function fmtTok(n: number | null | undefined): string {
  if (n === null || n === undefined || !Number.isFinite(n)) return "—";
  if (n < 1000) return String(Math.round(n));
  if (n < 1e6) return `${(n / 1000).toFixed(n < 10000 ? 1 : 0)}k`;
  return `${(n / 1e6).toFixed(1)}M`;
}

export function fmtMs(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms)) return "";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  return `${(ms / 1000).toFixed(ms < 10000 ? 1 : 0)}s`;
}

export function fmtUsd(usd: number | null | undefined): string {
  if (usd === null || usd === undefined || !Number.isFinite(usd)) return "—";
  return `$${usd.toFixed(4)}`;
}

export function fmtBytes(n: number): string {
  if (n < 1024) return `${n} bytes`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MiB`;
}

/** The usage ticker's four figures, off the LATEST report the projection kept.
 *  Never a sum: `call_cumulative` is already cumulative, and adding two reports
 *  would multiply a pane's spend by roughly its turn count. */
export interface TickerFields {
  input: string;
  output: string;
  cache: string;
  cost: string;
}

export function tickerFields(state: State): TickerFields {
  const t = state.usage ? state.usage.call_cumulative : null;
  return {
    input: fmtTok(t ? t.input : null),
    output: fmtTok(t ? t.output : null),
    cache: fmtTok(t ? t.cache_read : null),
    cost: fmtUsd(state.cost ? state.cost.usd : null),
  };
}

/** The header's boot facts. Every unknown is an em dash and never the string
 *  `"unknown"` — the contract's §1.3 rule, carried to the one surface a human
 *  reads it off. */
export interface HeaderFacts {
  model: string;
  session: string;
  capabilities: string;
}

export function headerFacts(state: State): HeaderFacts {
  return {
    model: state.model ?? "—",
    session: state.session ? state.session.slice(0, 8) : "—",
    capabilities: state.capabilities.length ? state.capabilities.join(" · ") : "—",
  };
}

/** The one-line summary a turn's rule carries. Built here so a turn receipt is
 *  testable without a DOM and cannot silently start reporting `0` for a turn
 *  that reported nothing. */
export function turnReceipt(b: TurnBlock): string {
  if (!b.ended) return `turn ${b.turn}`;
  const bits: string[] = [`turn ${b.turn} ended`];
  const stop = b.stop === null ? null : typeof b.stop === "string" ? b.stop : b.stop.other;
  if (stop) bits.push(stop);
  if (b.usage) {
    bits.push(
      `${fmtTok(b.usage.call_cumulative.input)} in · ${fmtTok(b.usage.call_cumulative.output)} out`,
    );
  }
  if (b.cost) bits.push(fmtUsd(b.cost.usd));
  if (b.durationMs !== null) bits.push(fmtMs(b.durationMs));
  return bits.join("  ·  ");
}

/** The sentence an elision states. Both ceilings render one; an elision the
 *  reader cannot see is a transcript that lies (`DESIGN.md` §7). */
export function evictionNotice(blocks: number): string {
  return (
    `${blocks.toLocaleString()} earlier blocks rolled out of the pane — ` +
    `the full transcript is on disk in the event log`
  );
}

export function droppedNotice(bytes: number): string {
  return `${fmtBytes(bytes)} elided from the head of this text`;
}

// ── the Edit diff ───────────────────────────────────────────────────────────

export interface DiffLine {
  k: "add" | "del" | "ctx";
  sign: string;
  n: number | null;
  text: string;
}

export interface DiffView {
  file: string;
  lines: DiffLine[];
}

/**
 * A real diff of an `Edit`'s real arguments (§6) — the difference between "a
 * tool ran" and "here is what changed".
 *
 * Common prefix and suffix, then the changed middle with `context` lines either
 * side. Naive by design; the payload it is given is two strings the agent
 * itself wrote, and anything cleverer would need a diff algorithm this bundle
 * does not carry.
 */
export function diffOf(input: unknown, context = 2): DiffView | null {
  if (!input || typeof input !== "object") return null;
  const obj = input as Record<string, unknown>;
  const oldS = typeof obj.old_string === "string" ? obj.old_string : "";
  const newS = typeof obj.new_string === "string" ? obj.new_string : "";
  if (!oldS && !newS) return null;

  const a = oldS.split("\n");
  const b = newS.split("\n");
  let p = 0;
  while (p < a.length && p < b.length && a[p] === b[p]) p += 1;
  let s = 0;
  while (s < a.length - p && s < b.length - p && a[a.length - 1 - s] === b[b.length - 1 - s]) s += 1;

  const lines: DiffLine[] = [];
  for (let i = Math.max(0, p - context); i < p; i += 1) {
    lines.push({ k: "ctx", sign: " ", n: i + 1, text: a[i]! });
  }
  for (let i = p; i < a.length - s; i += 1) lines.push({ k: "del", sign: "-", n: i + 1, text: a[i]! });
  for (let i = p; i < b.length - s; i += 1) lines.push({ k: "add", sign: "+", n: i + 1, text: b[i]! });
  for (let i = a.length - s; i < Math.min(a.length, a.length - s + context); i += 1) {
    lines.push({ k: "ctx", sign: " ", n: i + 1, text: a[i]! });
  }
  return { file: typeof obj.file_path === "string" ? obj.file_path : "", lines };
}

// ── the request card ────────────────────────────────────────────────────────

export interface RequestAction {
  /** What travels to `answer_pane_ui` / the permission command. */
  value: string;
  label: string;
  /** The default, drawn as the primary action (gold ink on a gold hairline —
   *  never a gold fill; `--accent` is never a ground). */
  primary: boolean;
  /** Drawn as the refusal. */
  danger: boolean;
}

/**
 * The buttons a pending request offers.
 *
 * A `confirm` and a permission get Allow/Deny; a `select` gets its own options
 * and nothing else. An `input` or `editor` dialog gets NO buttons here
 * (`wantsFreeText` below): it needs a text field, and offering an Allow button
 * for a question that wants a string would send an answer the extension cannot
 * read.
 */
export function actionsFor(b: RequestBlock): RequestAction[] {
  if (b.options.length) {
    return b.options.map((o, i) => ({ value: o, label: o, primary: i === 0, danger: false }));
  }
  if (wantsFreeText(b)) return [];
  return [
    { value: "allow", label: "Allow", primary: true, danger: false },
    { value: "deny", label: "Deny", primary: false, danger: true },
  ];
}

/** Does this request want a typed answer rather than a choice? */
export function wantsFreeText(b: RequestBlock): boolean {
  return b.channel === "ui" && (b.method === "input" || b.method === "editor");
}

/**
 * Whether a request's payload is a MACHINE LITERAL or PROSE — which decides
 * which of the two faces draws it (§4).
 *
 * A permission's payload is a command line the machine handed us, so it takes
 * mono. A dialog's message is prose an extension author wrote for a human, and
 * setting prose in mono is the costume the type roles exist to prevent. The
 * rule has a real edge and this is it, in one predicate rather than in a
 * ternary at the markup site.
 */
export function payloadIsProse(b: RequestBlock): boolean {
  return b.channel === "ui";
}

/**
 * The title a request card carries.
 *
 * A permission has no title of its own, and the card MUST still say what it is
 * permitting: §3.2's whole gain over the argv path is that the prompt sees the
 * ACTUAL call — allow `Bash(git status)`, refuse `Bash(git push)` — so a card
 * showing only the tool name throws away the one fact the human is being asked
 * about and trains them to click Allow without reading.
 */
export function requestTitle(b: RequestBlock): string {
  if (b.title) return b.title;
  if (b.channel === "permission") return `Allow ${b.tool ?? "this call"}?`;
  return "Needs you";
}

/** The payload line under that title. For a permission it is the identifying
 *  argument, falling back to the whole input when no key identifies it. */
export function requestPayload(b: RequestBlock): string {
  if (b.message) return b.message;
  if (b.channel !== "permission") return "";
  const arg = argLine(b.tool, b.input);
  if (arg) return `${b.tool ?? "?"}(${arg})`;
  return b.input === undefined || b.input === null ? "" : JSON.stringify(b.input);
}

/** What a settled card records. Both halves matter: the answer, and WHO
 *  decided — a settled request is the audit trail, so it collapses rather than
 *  vanishing. */
export function settlementLine(b: RequestBlock): string | null {
  if (!b.settled) return null;
  const a = b.settled.answer;
  const what =
    typeof a === "string"
      ? a
      : "Confirmed" in a
        ? a.Confirmed
          ? "allowed"
          : "denied"
        : "Value" in a
          ? a.Value
          : "cancelled";
  return `${what} · by ${b.settled.by}`;
}
