// A single terminal pane: xterm.js instance wired to a backend PTY,
// with a slim header for naming, splitting, and closing.

import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { pickDirectory } from "./transport.ts";
import {
  resizePty,
  killPty,
  dirInfo,
  changeDir,
  detachOutputOwner,
  detachGitWatchOwner,
  stopGitWatch,
  setGitWatch,
} from "./pty";
import { admitRoot } from "./fileapi";
import { voiceController, type VoiceTargetPane, type VoicePhase } from "./voicecontrol";
import { pathTail, type ShellKind } from "./panesetup";
import { FONT, TERM_METRICS, TERMINAL_THEME } from "./theme";
import { invoke } from "./transport.ts";
import { createOrderedWriter } from "./ptywrite";
import { createHumanOriginLatch } from "./humanorigin";
import { decideRefresh, REPO_SIGNAL_WINDOW_MS } from "./refreshthrottle";
import { showToast } from "./toast";
import { endGroup, notifyPaneDisposed, registerStructuredPane } from "./orchestration";
import { type CacheAgeReading } from "./cacheage";
import { type QueueDepthReading } from "./queuebadge";
import { makeRenameCommit } from "./panerename";
import { shouldResizePty } from "./panefit";
import { planFit, FIT_WINDOW_MS, FIT_MAX_WAIT_MS } from "./resizeburst";
import {
  planHeaderFit,
  menuLeftFor,
  overflowMenuIds,
  RENAME_ENTRY_ID,
  HEADER_GAP_W,
  type HeaderControl,
  type HeaderFitPlan,
  type HeaderFitStage,
} from "./paneheader";
import { swapEditor } from "./domutil";
import { openInEditor, editorConfigDialog } from "./editor";
import { GitView } from "./gitview";
import { type EmbedSide } from "./embedsplit";
import { CWD_DECLARING_VIEWS } from "./embedtoggle";
import {
  exitDiagnosticLine,
  keepOpenOnExit,
  type ExitInfo,
  type KeepOpenReason,
  type DirtyHost,
  type PaneBufferReport,
} from "./dirtystate";
import { FileEditView } from "./fileedit";
import { FileExplorerView } from "./fileexplorer";
import { icon } from "./icons.ts";
import { agentMark, type AgentMarkInput } from "./agenticons.ts";
import { WorkflowView } from "./workflowview";
import { TodoPaneView } from "./todopane";
import { StructuredPaneView } from "./structuredpane";
import type { AnswerFn } from "./structuredpane";
import { WORKFLOW_FILE, workflowNameOf } from "./workflowmodel";
import { persistedKindFor, type PersistedPane, type PersistedPaneKind } from "./tabstore";
import type { TabPaneInfo } from "./tabcounts";
import { hasForkSession, sessionCliFromCommand } from "./panerestore";
// The reconciler's own CLI set, imported rather than re-spelled: `agentCli`
// below exists to be matched against `listSessions()` rows, so the two must
// name the same CLIs by construction (#722).
import type { Cli } from "./sessionreconcile";
import { PaneActivity } from "./paneactivity.ts";
import type { PaneFacts, TabRef } from "./agentrows.ts";
import { notesApplyToPane } from "./notesmodel.ts";
import { ICON_BTN_PX, PaneViews } from "./paneviews";
import { EMBED_KINDS, PaneEmbeds } from "./paneembeds";
import { PaneBadges } from "./panebadges";
import { PaneCompose } from "./panecompose";
import { PaneLifecycle } from "./panelifecycle";
import { capturePane } from "./panecapture";

// The header's icons, from the registry (#879 slice K). They were hand-drawn
// inline here — inline so the toolbar renders identically regardless of
// installed fonts, which still holds — and the artwork now comes from
// src/icons.ts, which also DYES each one by its role: the two meta chips read
// as workspace and repo, the overlay buttons as the board they open, the group
// controls as the fleet. The sizes below are the boxes these glyphs already
// had, so nothing in the header moves.
const ICON_META_PX = 12;
const FOLDER_ICON = icon("folder", ICON_META_PX);
const BRANCH_ICON = icon("git-branch", ICON_META_PX);
// Fold-group toggle (#46): chevrons collapsing toward each other — signals
// "minimize every worker/reviewer pane to the dock at once".
const GROUP_MIN_ICON = icon("chevrons-down-up", ICON_BTN_PX);
// "Open in editor": code-brackets glyph. Opens the pane's workspace folder in
// the user's configured external editor (VS Code, Zed, …).
const EDITOR_ICON = icon("code-xml", ICON_BTN_PX);
// Per-session Notes (#2116): a page of writing. Deliberately `file-text` and
// not `file-pen` — that one is already this header's file-EDITOR button, and
// two buttons sharing a glyph in one row is worse than reusing a vendored
// mark. Reuse rather than a new vendored entry: `test/icons.test.ts` refuses
// an entry no surface asks for, and the `content` role this glyph carries —
// "files you read rather than run" — is what a note is.
const NOTES_ICON = icon("file-text", ICON_BTN_PX);
// Header overflow (#2191): the single glyph that stands in for the folded set.
// A TEXT glyph, like the window-control cluster it sits beside (◫ ⬓ — ⤢ ✕) and
// unlike the dyed registry icons — the overflow button names no ROLE, it names
// "the rest", so `src/icons.ts`'s role→hue table has nothing to say about it and
// a vendored Lucide glyph would be a licence obligation bought for nothing.
const OVERFLOW_GLYPH = "⋯";

/** `.pane-btn.pane-overflow { width: 25px }`, restated here because the button
 *  is `hidden` (and so measures 0) at exactly the moment the policy needs to
 *  know what folding would cost. Kept STRICTLY WIDER than `.pane-btn`'s own
 *  23px, which is not cosmetic: it is what makes `planHeaderFit`'s
 *  "folding buys nothing" guard refuse to fold a header holding a single
 *  control — a welcome pane, whose only control is its ✕. */
const OVERFLOW_BTN_W = 25;

/** Gap between the folder and branch chips (`.pane-meta { gap: 13px }`) — the
 *  header's own gap does not apply inside that box. */
const META_GAP_W = 13;

/** How long the header-overflow pass waits after a resize burst settles. The
 *  decision is not latency-critical (an icon set folding a frame late is
 *  invisible), and coalescing keeps the measuring reads off every drag frame.
 *  Deliberately independent of `resizeburst.ts`, which coalesces a DIFFERENT
 *  thing — the PTY fit. Nothing on this path resizes anything (constraint 1). */
const HEADER_SYNC_MS = 60;

/** Grace period between the pointer leaving the overflow button (or its menu)
 *  and the menu closing, so crossing the gap between the two does not dismiss it. */
const OVERFLOW_CLOSE_MS = 220;

/** An element's width at its NATURAL size, in px — what it would occupy if
 *  flexbox were not shrinking it. `offsetWidth` alone under-reports a shrunk
 *  `.pane-queue`/`.pane-mail`/`.pane-cwd` (each `min-width: 0` with
 *  `text-overflow: ellipsis`), and reporting the shrunk width would make the
 *  header look roomier the tighter it got. `scrollWidth` is the content box's
 *  full width, so add back the border the border box carries.
 *
 *  Returns 0 for an element that is not in the layout at all — `hidden`, or
 *  `display: none` by a pane-kind rule (`.pane.is-content .pane-btn.pty-only`).
 *  A control measuring 0 is not "a zero-width control", it is a control this
 *  pane kind does not have, and the caller drops it from the set entirely.
 *  A control inside the CLOSED overflow menu still measures its real width,
 *  because that menu is `visibility: hidden`, never `display: none` — which is
 *  what makes the policy's decision independent of the current fold state. */
function naturalWidth(el: HTMLElement): number {
  if (el.hidden) return 0;
  const box = el.offsetWidth;
  if (box === 0) return 0;
  return Math.max(box, el.scrollWidth + (box - el.clientWidth));
}

/** The width a header child cannot give up, px: its border plus its padding.
 *
 *  `styles.css` gives every non-control header child `min-width: 0`, so its
 *  CONTENT can be squeezed to nothing — but no `flex-shrink` touches a border or
 *  a padding, so a lit chip keeps ~14px however narrow the pane gets. Assuming
 *  that floor was zero is what let two lit chips push the `⋯` off an 80px pane
 *  (`MIN_PANE_PX`), which is #2335 not fixed (rev-final round 2). A hidden child
 *  costs nothing, matching `naturalWidth`'s reading of the same element. */
function irreducibleWidth(el: HTMLElement): number {
  if (el.hidden) return 0;
  const cs = getComputedStyle(el);
  const pad = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight);
  // `clientWidth` excludes borders and includes padding, so the difference is
  // the borders (there is no scrollbar on a header chip).
  const border = el.offsetWidth - el.clientWidth;
  return Math.max(0, (Number.isFinite(pad) ? pad : 0) + (Number.isFinite(border) ? border : 0));
}

/** Extract a filesystem path from an OSC 7 payload, which may be a raw path
 *  or a `file://host/path` URL. Returns "" if nothing usable. */
function normalizeOscPath(payload: string): string {
  const raw = payload.trim();
  if (!raw.startsWith("file://")) return raw;
  try {
    // Strip scheme + host, then percent-decode. On Windows a URL path looks
    // like `/C:/Users/...`; drop the leading slash before a drive letter.
    let p = decodeURIComponent(new URL(raw).pathname);
    if (/^\/[A-Za-z]:/.test(p)) p = p.slice(1);
    return p;
  } catch {
    return "";
  }
}

/** Trim a path to its last two segments for a compact toolbar label. */
function shortCwd(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  if (parts.length <= 2) return path;
  return "…/" + parts.slice(-2).join("/");
}

/** A hidden-by-default toolbar chip: an icon plus a text span. */
function makeMetaItem(cls: string, icon: string): [HTMLElement, HTMLElement] {
  const wrap = document.createElement("span");
  wrap.className = `pane-meta-item ${cls}`;
  wrap.hidden = true;
  const iconEl = document.createElement("span");
  iconEl.className = "pane-meta-icon";
  iconEl.innerHTML = icon;
  const text = document.createElement("span");
  text.className = "pane-meta-text";
  wrap.append(iconEl, text);
  return [wrap, text];
}

/** Role/group chip shown before the pane title (orchestration panes). */
export interface PaneBadge {
  /** Short uppercase label, e.g. "ORCH", "W", "REV". */
  label: string;
  /** Group accent color; also tints the pane header. */
  color: string;
  title?: string;
}

/** What `setConnected` needs to render the cross-workspace channel chip/accent
 *  (#271). Built by channel.ts's `channelBadge` from the live `OrchChannel`/
 *  `orch-channel` payload — pane.ts stays a pure renderer of it, same division as
 *  `setBadge`/`PaneBadge` above. */
export interface PaneChannelBadge {
  channelId: string;
  /** Per-channel accent, so two concurrently-active channels read as visually
   *  distinct sets of panes (channel.ts's `channelColor`). */
  color: string;
  /** Short chip text, e.g. "⇄2" (channel.ts's `channelChipLabel`). */
  label: string;
  /** Other members' display names, for the chip's tooltip. */
  peers: string[];
  /** THIS pane's direction in the channel (#271 W3 addendum, part C): drives the
   *  chip's arrow — outward (▲) for the sender, inward (▼) for a receiver. */
  direction: "sender" | "receiver";
  /** Whether this pane can currently `channel_send` — always true for the
   *  sender; for a receiver, true only while it holds the reply credit. */
  canSend: boolean;
  /** True for a delivery-only member (no token, ever) — the "receive-only"
   *  chip variant, distinct from a full receiver simply out of credit right
   *  now (which still reads as a plain receiver — it WILL be able to reply). */
  deliveryOnly: boolean;
  /** The channel's current sender — agent id/name — or null if unresolved.
   *  Used by panemenu.ts's join-compatibility rule ("Join as receiver —
   *  driven by {sender}") and by `orchestration.ts` to fill
   *  `PaneConnectState.senderId`/`senderName`. */
  senderId: string | null;
  senderName: string | null;
}

export interface PaneOptions {
  name?: string;
  cwd?: string;
  command?: string;
  /** Which interactive shell a plain Terminal pane spawns (#194 P2). Only used
   *  for shell panes (no `command`/`argv`); omitted panes spawn PowerShell. */
  shellKind?: ShellKind;
  /** Structured agent invocation for direct-CLI spawn (issue #78); the backend
   *  falls back to `command` (shell wrapper) when it can't apply. */
  argv?: string[];
  /** Extra per-pane env (#83): agent panes carry the gh-shim PATH +
   *  `LOOMUX_GROUP_DIR` here so the merge gate is enforced. Omitted for plain
   *  shells. Wire form: `[key, value]` pairs (the backend's `Vec<(String,String)>`). */
  env?: [string, string][];
  badge?: PaneBadge;
  /** Orchestration group this pane belongs to (enables the task board). */
  orchGroup?: string;
  /** "orchestrator" | "worker" | "reviewer". */
  orchRole?: string;
  /** Agent id, for attention acks (clearing a "needs attention" badge). */
  orchAgent?: string;
  /** A STANDALONE pane's channel-scoped MCP identity (#271 W3 addendum, parts
   *  A/C): set by the launcher after `orch_solo_prepare`/`orch_solo_bind` for a
   *  newly-spawned pane. Deliberately NOT `orchGroup`/`orchRole`/`orchAgent` —
   *  those gate the full orchestration chrome (task board, audit button, group
   *  badge, steering strip), which a plain standalone pane must never show just
   *  because it can now join a channel. A pane with no identity at spawn time
   *  (launched before this feature, or adopted later) gets one via
   *  `setChannelAgent` post-construction instead. */
  channelAgent?: { group: string; agentId: string; role: string; canSend: boolean };
  /** Open without stealing keyboard focus (issue #117): an orchestrator-driven
   *  spawn must not yank the cursor from the pane the human is typing in. The
   *  human-initiated paths leave this unset (focus the new pane); only the
   *  orch-spawn-request path sets it. Grid.openPane resolves the actual
   *  decision — an empty grid still focuses regardless (see panefocus.ts). */
  background?: boolean;
  /** #887 S3: present iff this pane's process is a local ssh client connected to
   *  a saved SSH profile — `argv` above is the ssh command line the launcher
   *  composed, and this names the connection it came from (never its contents:
   *  the profile can be renamed or re-edited and the pane stays pointed at it).
   *
   *  Its presence is what suppresses this pane's LOCAL-filesystem affordances,
   *  which is not cosmetic: an SSH pane's shell reports REMOTE paths over OSC 7,
   *  and pointing a local git watch (or a local folder picker's `cd`) at
   *  `/srv/app` is meaningless at best. See `start()`.
   *
   *  `defaultCli` is the profile's far-end CLI (`SshProfile.defaultCli`) — the program
   *  `sshLaunchParams` actually composed the remote command from, so it is the pane's
   *  REAL agent while `argv[0]` is only the transport that carries it. Passed here by
   *  the callers that already hold the profile, because the store read that resolves it
   *  is async and the header mark is drawn synchronously. Optional and nullable: a
   *  profile with no default CLI is a plain remote shell, and a caller that cannot cheaply
   *  supply it degrades to the neutral badge rather than to a wrong one (#992 review B1). */
  ssh?: { profileId: string; defaultCli?: string | null };
  /** Recorded resumable agent session id (#194): so a restored Agent pane can
   *  `--resume <id>` back into its prior context (resuming into an idle TUI
   *  costs nothing until a prompt is sent). Set by the launcher for
   *  session-capable CLIs; absent for terminals/orchestration and best-effort
   *  CLIs. Retained for the layout snapshot — never used to drive the PTY. */
  sessionId?: string;
  /** The PARENT session id this pane was FORKED from (#3318 F1), set by the
   *  pane menu's Fork gesture and by a restore of a pane it opened.
   *
   *  Retained for the capture, where it does two things — it is the record's
   *  provenance, and it is the GATE that discharges the one-shot
   *  `--fork-session` flag out of the persisted command line. See
   *  `PersistedPane.forkOf` for the whole contract and why the gate is this
   *  field rather than the flag's presence. */
  forkOf?: string;
}

/** The PTY-less CONTENT pane kinds. A pane of one of these kinds IS a surface —
 *  a file manager (#214), the file editor, the git view (#217), or the workflow
 *  builder (#222) — rather than a process. They share every pane mechanic (split,
 *  dock, drag, maximize, restore) and differ only in which view fills the content box. */
export type ContentPaneKind =
  | "files"
  | "editor"
  | "git"
  | "workflow"
  | "structured"
  | "todo";

/** What a content pane needs: which surface, the root it is pointed at, and a name.
 *  Deliberately NOT part of PaneOptions — every field there describes a PTY spawn,
 *  and a content pane never has one. */
export interface ContentPaneOptions {
  kind: ContentPaneKind;
  name: string;
  /** Absolute path the surface is rooted at: the folder a manager lists / an editor
   *  trees, or a directory inside the repo a git view shows. Validated for real by
   *  the caller before we get here — `ftRootIsDir` for files/editor, `gitRepoRoot`
   *  for git — so this never builds a pane around a root that isn't what it claims. */
  root: string;
  /** EDITOR kind: a root-relative file to open immediately. Set by the file browser's
   *  "Open in file editor pane" (#217); absent from the welcome flow, which opens the
   *  editor on its tree with nothing selected.
   *
   *  WORKFLOW kind (#222): which workflow file to edit, root-relative. Defaults to
   *  the repo's own workflow path when absent — the welcome flow's case — and is set
   *  when the browser opens a *different* YAML as a workflow. */
  file?: string;
  /** STRUCTURED kind (#2891): which agent's event stream fills this pane. Both are
   *  required for that kind and meaningless for the other four  a structured pane is a
   *  view of ONE agent's log, and there is no default agent to fall back to.
   *
   *  They are optional in the TYPE rather than split into a second options interface
   *  because every other field here (`name`, `root`, `background`) means exactly what it
   *  already meant, and `startContent` refuses the kind without them rather than building
   *  a pane around an agent it cannot name. */
  groupId?: string;
  agentId?: string;
  /** Which agent program runs in this pane, for the header chip. Read off the source and
   *  never derived by branching on one CLI's name; absent renders no chip. */
  cli?: string | null;
  /** How an answered dialog leaves the pane (�3.5's trusted path). Supplied by the
   *  caller so `pane.ts` never imports the orchestration bridge  and so a fixture
   *  replay can hand in a local one. */
  answer?: AnswerFn;
  /** Open without stealing keyboard focus (same contract as PaneOptions). */
  background?: boolean;
}

// The terminal's colours come from the app's one palette (src/theme.ts), not from a copy
// kept here: xterm renders on a WebGL canvas, so CSS custom properties cannot reach it and
// the values have to exist twice — in the stylesheet and in TypeScript. theme.ts is the
// copy, and test/theme.test.ts holds this file to it (no hex literals, and the ITheme
// imported rather than rebuilt). Colour-only ITheme changes are reflow-free; the FONT
// metrics below are the ones that move the cell grid.
const TERM_THEME = TERMINAL_THEME;

export interface PaneEvents {
  onFocus: (pane: Pane) => void;
  onCloseRequest: (pane: Pane) => void;
  onSplit: (pane: Pane, dir: "row" | "column") => void;
  /** Park this pane in the dock (out of the grid, still running). */
  onMinimize: (pane: Pane) => void;
  /** Toggle this pane to/from fullscreen over the grid. */
  onMaximize: (pane: Pane) => void;
  /** How many LIVE children a lead pane's group currently holds (#2519), for
   *  the close confirm's "this ends N subagents". Asked of the host for the
   *  same reason `onOpenEditorPane` is: the answer is a walk over every tab's
   *  panes, and a `Pane` holds no back-reference to the tab it is in.
   *
   *  OPTIONAL, and `null` is a real answer distinct from `0`: a host that does
   *  not implement it (the grid's own test doubles) leaves the confirm saying
   *  what it is about to do without a count, which is still the truth. `0` is
   *  the different statement "a lead with no children right now" — the close
   *  still ends its group. */
  leadChildCount?: (pane: Pane) => number | null;
  /** Minimize (or restore) this pane's whole orchestration group's
   *  worker/reviewer panes at once (#46). No-op off an orchestrator pane. */
  onToggleGroupMinimize: (pane: Pane) => void;
  /** The pane's PERSISTED identity changed without any grid mutation, so the saved
   *  layout is now stale: a content pane was re-rooted, or a pane was renamed (#214).
   *  Nothing opened or closed, so no grid event fires and nothing would otherwise
   *  re-persist until the next unrelated one — meaning a quit right after a re-root
   *  would restore the OLD root. The host re-persists. */
  onRecordChanged: (pane: Pane) => void;
  /** Open an EDITOR pane beside `pane` (#217) — the file browser's "Open in file
   *  editor pane". The pane can't reach the grid itself (it doesn't know which tab
   *  it is in), so it asks its host, exactly as `onSplit` does for a welcome pane. */
  onOpenEditorPane: (pane: Pane, opts: { name: string; root: string; file?: string }) => void;
  /** Open a WORKFLOW pane beside `pane` (#222) — the file browser's "Open in workflow
   *  pane" on a YAML row. Same shape, same reason, as `onOpenEditorPane`.
   *
   *  `file` is optional since #1689 slice D2: the group header's *Edit…* sends none
   *  for `default`, which is what keeps the pane's own default path AND its ability
   *  to CREATE a workflow in a repo that has none — the launcher's button already
   *  relies on both. The browser's row always sends one. */
  onOpenWorkflowPane: (pane: Pane, opts: { name: string; root: string; file?: string }) => void;
  /** Right-click on the pane header (#271): the pane can't build/show its own connect
   *  menu — that needs the cross-tab armed-connect state and the backend wrappers,
   *  neither of which a Pane knows about — so, like `onOpenEditorPane`, it asks its
   *  host. `x`/`y` are viewport coords for `showContextMenu`. */
  onPaneContextMenu: (pane: Pane, x: number, y: number) => void;
  /** The pane's own channel chip was clicked (#271's "easy close" requirement: a
   *  one-click disconnect from the indicator itself, not just the menu). */
  onDisconnectChannel: (pane: Pane) => void;
  /** The Notes button was clicked (#2116). Same shape and same reason as
   *  `onOpenEditorPane`: the overlay reads and writes `sessionlog.json` through
   *  a store the host owns, and a `Pane` must not hold one — it would give
   *  every pane its own handle on a single-file multi-tenant store, which is
   *  exactly the shape the store's read-before-write rule exists to protect. */
  onOpenNotes: (pane: Pane) => void;
  /** This pane just LEARNED its agent session id (#2116, and
   *  `docs/design/session-id-learning.md`).
   *
   *  Fires from `adoptSessionId` and from nowhere else, at most once per pane
   *  per adopted id — `adoptSessionId` refuses a second adoption, so this
   *  inherits that. The spawn-time path (`--session-id` on the launch line,
   *  which is claude's) never goes through `adoptSessionId` and so never fires
   *  this; the host records that pane directly instead. The host's job here is
   *  to re-key any notes written against the pane before the id existed. */
  onSessionIdentified: (pane: Pane) => void;
}

/** Every view that can occupy a pane's embed-panel slot (#361) — a real flex
 *  sibling of the terminal instead of a floating overlay. `"editor"` is the
 *  file-editor OVERLAY (`FileEditView` hosted on a normal pane) — distinct
 *  from the #217 editor CONTENT PANE (a whole pane, `host.embedded`), which
 *  stays exactly what it was: the strictly-better path for "keep an editor
 *  open beside my agent" that made embedding the overlay unnecessary in the
 *  single-view (#361) round. A later round added it anyway (user-directed
 *  scope increase) once the multi-slot generalization made "one more
 *  dockable view" cheap to reason about — see docs/design/embedded-panels.md's
 *  "What's embeddable, and what isn't". */
export type EmbedKind =
  | "tasks"
  | "git"
  | "issues"
  | "audit"
  | "group"
  | "editor"
  | "timeline"
  | "tokens"
  | "decisions";

/** #1042: compile-time pin that every view `embedtoggle.ts` lets declare the
 *  pane's cwd really is an `EmbedKind`. That module takes a plain `string` so it
 *  stays free of the pane, which means a typo'd or renamed kind there would
 *  silently stop matching and quietly declare nothing — a guard that has become
 *  a no-op. This line is what makes that a build failure instead. */
const _CWD_DECLARING_VIEWS_ARE_EMBED_KINDS: readonly EmbedKind[] = CWD_DECLARING_VIEWS;
void _CWD_DECLARING_VIEWS_ARE_EMBED_KINDS;

/** Source of `Pane.key` (#2122 slice A): a per-WINDOW counter, so two panes
 *  alive at once never share a key and a key is never reused. Deliberately not
 *  persisted and deliberately not derived from anything the human or the
 *  backend controls — its only job is to let a view diff its rows across a
 *  re-render. `ptyId` cannot do that (it changes on every respawn) and the
 *  pane name cannot either (the human renames panes). */
let paneKeySeq = 0;

export class Pane implements VoiceTargetPane {
  readonly el: HTMLElement;
  readonly term: Terminal;
  /** This pane's identity for the lifetime of this window — see `paneKeySeq`.
   *  Read through `facts()`; nothing else should key on it. */
  readonly key = `pane-${++paneKeySeq}`;
  ptyId: number | null = null;
  name = "shell";

  titleEl: HTMLElement;
  /** The agent-type mark (#992): which CLI is running in this pane, as a glyph. Sits at
   *  the head of the header row, before the group role badge, and stays hidden until a
   *  launch line names a program — a plain shell has no agent type to report. */
  private agentMarkEl: HTMLElement;
  termEl: HTMLElement;
  cwdEl: HTMLElement;
  private cwdTextEl: HTMLElement;
  private branchEl: HTMLElement;
  private branchTextEl: HTMLElement;
  /** Latest un-abbreviated directory the shell reported, for the picker. */
  cwdRaw: string | null = null;
  /** Directory the external-change git watch is currently pointed at (#36),
   *  so we only re-issue the backend call when the pane actually changes dir. */
  watchedPath: string | null = null;
  /** When the last signal-driven `dir_info` read ran, and the trailing timer if
   *  one is booked — the pane half of the repo-signal throttle (signalDirRefresh). */
  private dirRefreshAt = 0;
  private dirRefreshTimer: number | undefined;
  /** The Notes button (#2116). Hidden until a launch line gives this pane a
   *  harness — see `syncNotesBtn`. */
  private notesBtn: HTMLButtonElement;
  /** How many notes the host's store holds for this pane's session, as of the
   *  last `setNotesCount`. Kept only to render the button's title; the store
   *  is the truth and this pane never reads it. */
  private notesCount = 0;
  /** Fold-group toggle (orchestrator panes only, #46): minimizes every
   *  worker/reviewer pane in the group to the dock, or restores them all. */
  groupMinBtn: HTMLButtonElement;
  /** Fullscreen toggle; its glyph flips to a restore affordance when active. */
  private maximizeBtn: HTMLButtonElement;
  orchGroup: string | null = null;
  orchRoleName: string | null = null;
  orchAgent: string | null = null;
  /** #887 S3: the SSH profile this pane's ssh client was launched from, or null
   *  for every other pane. Non-null IS "this pane is an SSH pane" — the single
   *  fact every local-filesystem suppression below reads, so a ninth pane kind
   *  can't be added to one site and forgotten at another. */
  sshProfileId: string | null = null;
  /** The far-end CLI of this pane's SSH profile, when a caller supplied it — the pane's
   *  real agent, which `spawnArgv` (the local ssh client) cannot report. Only the header
   *  mark reads it (#992). */
  sshDefaultCli: string | null = null;
  /** Standalone pane's channel-scoped identity (#271 W3 addendum) — a carrier
   *  DELIBERATELY separate from orchGroup/orchAgent/orchRoleName (those gate
   *  the full orchestration chrome; a plain standalone pane must never show
   *  it just because it can now join a channel). Set at construction
   *  (`opts.channelAgent`) or later via `setChannelAgent` (adopt-on-connect). */
  channelAgentInfo: { group: string; agentId: string; role: string; canSend: boolean } | null = null;
  /** The orchestrator's CLI, learned from the save-attachment response; decides
   *  how image paths are referenced in the steer text (#72). Defaults to the
   *  Claude form until a save reports otherwise. */
  orchCli = "claude";
  /** Notified when something the dock chip shows changes (attention state or
   *  the pane name); the grid uses it to keep a minimized pane's chip in sync,
   *  since a docked pane's header is out of the DOM (#6, #95r). */
  dockSyncListener: (() => void) | null = null;
  /** True for agent/command panes (vs plain shells). */
  launchedCommand = false;
  /** Whether this pane's pty has emitted a single byte since it spawned.
   *  Distinguishes a crash-with-real-output from one that died silently
   *  before printing anything — the DOA-revival signature (#281/#280) — so
   *  the exit banner (and #280's auto-close) can tell them apart. */
  receivedOutput = false;
  /** Spawn inputs retained for the session-restore layout snapshot (#194): how
   *  this pane was launched, so `capture()` can serialize it. Record-only —
   *  never read back to drive the live PTY. */
  spawnCommand: string | null = null;
  spawnArgv: string[] | null = null;
  spawnShellKind: ShellKind | null = null;
  agentSessionId: string | null = null;
  /** #3318 F1 — the parent session this pane was forked from, or null. Set
   *  from `PaneOptions.forkOf` on every path that starts a pane, and read only
   *  by `capture()`. */
  forkOf: string | null = null;
  /** Wall-clock time the HUMAN's first input (keystroke/paste) reached this
   *  pane's current process — not when the process was spawned (#440; review
   *  round 2, B2). The session-reconciler needs a boundary before which a
   *  session transcript can't be this pane's, and gating on SPAWN time left a
   *  pane adoption-eligible for its entire idle-before-first-prompt lifetime
   *  (a transcript is only created once prompted — #194 BUG-1), a window in
   *  which an unrelated same-CLI/same-cwd session could be a sole,
   *  uncontested false match. Gating on first input instead means a pane with
   *  nothing typed into it yet is never even a reconcile candidate — see
   *  `sessionreconcile.ts`'s `ReconcilePane.eligibleSinceMs`. Set ONCE per
   *  spawn via `markFirstInput()` (review round 3, B2-R: from `term.onKey` and
   *  the two `term.paste()` sites — deliberately NOT `term.onData`, which
   *  also fires for data xterm generates on its own with no key pressed; see
   *  `markFirstInput`'s comment). Reset to null at the top of both `start()`
   *  and `respawnFresh()`; never touched by `adoptSessionId` (adopting an id
   *  doesn't respawn or re-prompt anything). */
  firstInputMs: number | null = null;
  /** #518: the per-turn "this data came from a human" flag `onData` stamps
   *  each write with. Marked by `markFirstInput()` (i.e. by `term.onKey` and
   *  the two `term.paste()` sites — the same structural signal `firstInputMs`
   *  already trusts), read synchronously in the `onData` handler. Unlike
   *  `firstInputMs` it belongs to the PANE, not to one process: it carries no
   *  state across turns and so has nothing to reset on respawn. */
  humanOrigin = createHumanOriginLatch();
  private shiftTimer: number | undefined;
  fit = new FitAddon();
  resizeObs: ResizeObserver;
  disposed = false;
  // ---- header overflow (#2191) ----
  /** The header row itself — the box the overflow policy measures. */
  private headerEl!: HTMLElement;
  /** The folder/branch box, which is also the header's flex spacer. Measured by
   *  its ITEMS, never by its own (stretched) box — see `measureHeaderFixed`. */
  private metaEl!: HTMLElement;
  /** The single button the folded set collapses into. Hidden while unfolded. */
  private overflowBtn!: HTMLButtonElement;
  /** The floating icon strip the overflow button opens. An OVERLAY over the
   *  terminal (`position: absolute`, out of flow) — never a second header row,
   *  which would change `.pane-term`'s box and resize the PTY (constraint 1). */
  private overflowMenu!: HTMLElement;
  /** Every header control in DOM order, plus the two members that are not
   *  controls (the overflow button and its menu) at their slots — replaying this
   *  sequence with `appendChild` is what restores header order on unfold. */
  private headerTail: HTMLElement[] = [];
  /** The foldable/priority registry the policy decides over, in header order. */
  private headerControls: { id: string; el: HTMLElement; priority: boolean }[] = [];
  private headerObs: ResizeObserver;
  /** The header's content-box width from the last `ResizeObserver` delivery. 0
   *  until the pane is laid out, which `planHeaderFit` reads as "carry state". */
  private headerContentW = 0;
  /** The fold rung the header is on. Carries the policy's hysteresis across
   *  passes, and is the ONE record of it — `.header-folded` / `.header-minimal`
   *  are rendered from the plan, never read back as state (#2335). */
  private headerStage: HeaderFitStage = "full";
  private headerFolded = false;
  /** The menu's stand-in for the pane name, shown only at the `minimal` rung
   *  where the name has left the row. Lives in the menu permanently, so
   *  `applyHeaderPlan`'s replay never has to know about it. */
  private renameEntry!: HTMLButtonElement;
  private headerSyncTimer: number | undefined;
  private overflowOpen = false;
  /** Opened by a CLICK rather than by hover, so the pointer leaving does not
   *  dismiss it. Cleared by every close. */
  private overflowPinned = false;
  /** True for exactly the duration of the `focus()` call Escape makes to hand
   *  focus back to ⋯ — without it, that focus re-opens the menu Escape closed. */
  private overflowRefocusing = false;
  private overflowCloseTimer: number | undefined;
  /** Registered only while the menu is open — a click-away listener per pane,
   *  live for every pane at once, would be a document listener per terminal. */
  private overflowAwayListener: ((e: PointerEvent) => void) | null = null;
  /** Welcome / pane-setup content (#194): a pane can exist with NO PTY, showing
   *  the setup form until the user picks a kind. The PTY spawns only on submit
   *  (`startFromWelcome`), so the no-resize invariant holds — there's nothing to
   *  resize before then. Null once the pane has become a real terminal. */
  private welcomeEl: HTMLElement | null = null;
  dormantRecord: PersistedPane | null = null;
  /** CONTENT pane (#214 files, #217 editor + git): the pane's permanent content is a
   *  view — the file manager, the file editor, or the git view — rooted at
   *  `contentRoot`. No terminal is ever opened and no PTY ever spawns (the
   *  `startWelcome` precedent taken to its conclusion: a pane that is content, not a
   *  process), so the no-resize invariant holds trivially — there is no ConPTY to
   *  resize. `contentKind` non-null IS the "this is a content pane" flag; `contentRoot`
   *  doubles as the pane's cwd for the capture and for "open in editor". Exactly one
   *  of the four views below is non-null on such a pane; all are null elsewhere. */
  contentKind: ContentPaneKind | null = null;
  private contentRoot: string | null = null;
  /** The file a content pane opened ON: the editor's open file, or the workflow pane's
   *  workflow file (#222). Null for the kinds whose only input is a root. */
  private contentFile: string | null = null;
  private filesView: FileExplorerView | null = null;
  editorPaneView: FileEditView | null = null;
  private gitPaneView: GitView | null = null;
  workflowPaneView: WorkflowView | null = null;
  private structuredPaneView: StructuredPaneView | null = null;
  private todoPaneView: TodoPaneView | null = null;
  /** True once the pane's process has exited but the pane was kept open to show
   *  its output (notifyExited). The counter must not count a dead agent as live
   *  (#194 P4 LOW-7). */
  exited = false;
  /** #407: true between "loomux killed this pane's process on purpose" and "the
   *  replacement is spawned" — see `isRelaunching`. */
  private relaunching = false;
  /** Ordered input pipe to the PTY: serializes every keystroke/paste so the
   *  async IPC writes can't reorder (a bracketed-paste terminator overtaking
   *  its body wedges the target app — #65). Buffers input produced before the
   *  PTY exists and flushes it in order once ready. */
  writer = createOrderedWriter();
  /** Whether this pane is its grid's active pane (`setActive`). The one input
   *  that decides whether output keeps its per-frame cadence. */
  isActivePane = false;
  /** This pane's activity reducer (#2122 slice A2, `paneactivity.ts`): what it
   *  has been outputting, when the human last typed into it, and whether it is
   *  believed parked at a prompt. Fed from `acceptOutput`, `markFirstInput` /
   *  `markHumanInput`, `setAttention` and `noteRosterIdle`; read back through
   *  `facts()`. Pure state — no timer, no IPC, no DOM. */
  activity = new PaneActivity();

  // The satellites this pane delegates to (#3498 F1). Each owns one cluster's state and
  // methods and reads the rest of the pane through its `pane` back-reference; the
  // module-layout note (docs/design/module-layout.md) carries the shape.
  readonly lifecycle = new PaneLifecycle(this);
  readonly badges: PaneBadges;
  readonly embeds = new PaneEmbeds(this);
  readonly views: PaneViews;
  readonly compose: PaneCompose;

  constructor(readonly events: PaneEvents) {
    this.el = document.createElement("div");
    this.el.className = "pane";

    const header = document.createElement("div");
    header.className = "pane-header";
    this.headerEl = header;

    // The agent-type mark (#992). Appended BEFORE the title so that `setBadge`, which
    // inserts the role chip immediately before the title, lands between the two: the
    // header reads mark → role → name, i.e. what program, which agent, what it's called.
    // Pure header chrome — it floats in the header row and never touches the terminal's
    // geometry, so CLAUDE.md constraint 1 (never resize the PTY for a UI feature) holds
    // trivially.
    this.agentMarkEl = document.createElement("span");
    this.agentMarkEl.className = "pane-cli-icon";
    this.agentMarkEl.hidden = true;
    header.appendChild(this.agentMarkEl);

    this.titleEl = document.createElement("span");
    this.titleEl.className = "pane-title";
    this.titleEl.title = "Double-click to rename (F2)";
    this.titleEl.addEventListener("dblclick", () => this.startRename());
    header.appendChild(this.titleEl);

    // The header chips (attention + dismiss, watch, fork crumb, held, queue, mail,
    // cache, channel) are built, appended and owned by PaneBadges (panebadges.ts).
    this.badges = new PaneBadges(this, header);

    // The connect gesture (#271): right-click anywhere on the header shows the
    // Connect/Disconnect menu. Buttons that already have their own contextmenu
    // handling (the editor button, below) call stopPropagation in theirs, so this
    // never double-fires for them. The pane itself can't build the menu — that
    // needs the cross-tab armed-connect state and the backend wrappers — so, like
    // `onOpenEditorPane`, it asks its host.
    header.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      e.stopPropagation();
      this.events.onPaneContextMenu(this, (e as MouseEvent).clientX, (e as MouseEvent).clientY);
    });

    // Live metadata: current folder + git branch, reported by the shell.
    // The folder chip picks a folder to cd into; the branch chip opens the
    // git view.
    const meta = document.createElement("div");
    meta.className = "pane-meta";
    [this.cwdEl, this.cwdTextEl] = makeMetaItem("pane-cwd", FOLDER_ICON);
    [this.branchEl, this.branchTextEl] = makeMetaItem("pane-branch", BRANCH_ICON);
    this.cwdEl.setAttribute("role", "button");
    this.cwdEl.tabIndex = 0;
    this.cwdEl.addEventListener("click", (e) => {
      e.stopPropagation();
      void this.pickFolder();
    });
    this.branchEl.setAttribute("role", "button");
    this.branchEl.tabIndex = 0;
    this.branchEl.addEventListener("click", (e) => {
      e.stopPropagation();
      this.toggleGitView();
    });
    meta.append(this.cwdEl, this.branchEl);
    header.appendChild(meta);
    this.metaEl = meta;

    // The orchestration view toggles (tasks, decisions, audit, timeline, tokens, group)
    // are built, appended and owned by PaneViews (paneviews.ts).
    this.views = new PaneViews(this, header);

    // Fold the whole group's worker/reviewer panes to the dock in one click
    // (or restore them). Orchestrator panes only; the group's real-estate
    // control when it grows large (#46).
    this.groupMinBtn = document.createElement("button");
    this.groupMinBtn.className = "pane-btn";
    this.groupMinBtn.innerHTML = GROUP_MIN_ICON;
    this.groupMinBtn.title = "Minimize / restore all group panes";
    this.groupMinBtn.hidden = true; // shown for orchestrator panes in start()
    this.groupMinBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.events.onToggleGroupMinimize(this);
    });
    header.appendChild(this.groupMinBtn);

    // Open the pane's workspace folder in the configured external editor.
    // Left-click opens (prompting for the editor on first use); right-click
    // reconfigures the editor command.
    const editorBtn = document.createElement("button");
    editorBtn.className = "pane-btn";
    editorBtn.innerHTML = EDITOR_ICON;
    editorBtn.title = "Open in editor (Alt+E) · right-click to configure";
    editorBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      void this.openInEditor();
    });
    editorBtn.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      e.stopPropagation();
      void editorConfigDialog().then(() => this.focus());
    });
    header.appendChild(editorBtn);

    // …and so are the overlay toggles (issues, git, file editor), at this slot in the row.
    this.views.wireOverlayToggles(header);

    // Per-session Notes (#2116). `pty-only` takes the content panes (the CSS
    // rule hides the class on `.is-content`); `syncNotesBtn` takes the other
    // two gates, which are not expressible as a class — a harness must be
    // running, and an SSH pane has no LOCAL session id to key a note to even
    // when a CLI is running at the far end.
    this.notesBtn = document.createElement("button");
    this.notesBtn.className = "pane-btn pty-only";
    this.notesBtn.innerHTML = NOTES_ICON;
    this.notesBtn.hidden = true; // until a launch line says otherwise
    this.notesBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.events.onOpenNotes(this);
    });
    header.appendChild(this.notesBtn);
    this.setNotesCount(0);

    // Minimize / maximize live next to close: the same window-control cluster
    // users expect. Maximize keeps a stored ref so its glyph can flip to a
    // "restore" affordance while fullscreen.
    this.maximizeBtn = document.createElement("button");
    this.maximizeBtn.className = "pane-btn";
    this.maximizeBtn.textContent = "⤢";
    this.maximizeBtn.title = "Maximize (Ctrl+Shift+M)";
    this.maximizeBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.events.onMaximize(this);
    });

    // The overflow affordance (#2191) sits at the head of the window-control
    // cluster, so a folded header reads ⋯ — ⤢ : "the rest", then the two
    // controls that never fold. Built here rather than after the cluster so its
    // MENU (the next node) precedes the priority buttons in DOM order, which is
    // what puts Tab from the ⋯ button INTO the menu instead of past it.
    this.overflowBtn = document.createElement("button");
    this.overflowBtn.className = "pane-btn pane-overflow";
    this.overflowBtn.textContent = OVERFLOW_GLYPH;
    this.overflowBtn.title = "More pane controls";
    this.overflowBtn.setAttribute("aria-haspopup", "true");
    this.overflowBtn.setAttribute("aria-expanded", "false");
    this.overflowBtn.hidden = true; // shown only while the header is folded
    header.appendChild(this.overflowBtn);

    this.overflowMenu = document.createElement("div");
    this.overflowMenu.className = "pane-overflow-menu";
    this.overflowMenu.setAttribute("role", "group");
    this.overflowMenu.setAttribute("aria-label", "More pane controls");
    header.appendChild(this.overflowMenu);

    // The name's stand-in (#2335). At the narrowest width the pane name is out
    // of the row entirely — a name clipped to one character carries nothing and
    // still spends the room the ⋯ button needs — so the menu carries it
    // instead: the label IS the current name, and pressing it starts the same
    // rename F2 and a double-click on the title start. First child, because that
    // is where the name sits in the row it is standing in for.
    this.renameEntry = document.createElement("button");
    this.renameEntry.className = "pane-btn pane-rename-entry";
    this.renameEntry.hidden = true;
    this.renameEntry.addEventListener("click", (e) => {
      e.stopPropagation();
      this.closeOverflowMenu();
      this.startRename();
    });
    this.overflowMenu.appendChild(this.renameEntry);

    this.wireOverflowOpen();
    this.wireOverflowDismissal();

    const clusterBtns: Record<string, HTMLButtonElement> = {};
    for (const [id, glyph, cls, tip, fn] of [
      ["split-right", "◫", "", "Split right", () => this.events.onSplit(this, "row")],
      ["split-down", "⬓", "", "Split down", () => this.events.onSplit(this, "column")],
      ["minimize", "—", "", "Minimize to dock (Alt+M)", () => this.events.onMinimize(this)],
    ] as const) {
      const btn = document.createElement("button");
      btn.className = `pane-btn ${cls}`;
      btn.textContent = glyph;
      btn.title = tip;
      btn.addEventListener("click", (e) => {
        e.stopPropagation();
        fn();
      });
      header.appendChild(btn);
      clusterBtns[id] = btn;
    }
    header.appendChild(this.maximizeBtn);

    const closeBtn = document.createElement("button");
    closeBtn.className = "pane-btn close";
    closeBtn.textContent = "✕";
    closeBtn.title = "Close pane";
    closeBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      this.requestClose();
    });
    // Retained for the lead-pane close confirm (#2519), which repaints this
    // button rather than opening a modal — see `requestClose`.
    this.closeBtn = closeBtn;
    header.appendChild(closeBtn);
    this.el.appendChild(header);

    // The overflow registry (#2191), in header order. `priority: true` is the
    // set the human named as always-visible — minimize, maximize and the pane
    // name (the name is not a control, so it is the policy's `titleMinWidth`
    // floor rather than a row here). Everything else folds, close included;
    // docs/design/pane-header.md carries the argument, and moving a control
    // between the two sets is this flag and nothing else.
    this.headerControls = [
      { id: "tasks", el: this.views.tasksBtn, priority: false },
      { id: "decisions", el: this.views.decisionsBtn, priority: false },
      { id: "audit", el: this.views.auditBtn, priority: false },
      { id: "timeline", el: this.views.timelineBtn, priority: false },
      { id: "tokens", el: this.views.tokensBtn, priority: false },
      { id: "group", el: this.views.groupBtn, priority: false },
      { id: "group-min", el: this.groupMinBtn, priority: false },
      { id: "editor", el: editorBtn, priority: false },
      { id: "issues", el: this.views.issuesBtn, priority: false },
      { id: "git", el: this.views.gitBtn, priority: false },
      { id: "file-edit", el: this.views.fileEditBtn, priority: false },
      // #2116's Notes button folds like every other view toggle. Registered
      // rather than merely appended: the registry is what `headerTail` is
      // filtered from, so an unregistered control is not replayed by
      // `applyHeaderPlan` and drifts out of DOM order the first time the header
      // folds — as well as never folding at all, which is the whole point of
      // #2191. It is `hidden` on any pane without a local harness, and
      // `syncHeaderOverflow` reads a zero natural width as "a control this pane
      // does not have", so a plain shell's header is unaffected.
      { id: "notes", el: this.notesBtn, priority: false },
      { id: "split-right", el: clusterBtns["split-right"], priority: false },
      { id: "split-down", el: clusterBtns["split-down"], priority: false },
      { id: "minimize", el: clusterBtns["minimize"], priority: true },
      { id: "maximize", el: this.maximizeBtn, priority: true },
      { id: "close", el: closeBtn, priority: false },
    ];
    // DOM order, including the two non-controls at their slots. Read OFF the
    // header rather than re-listed, so it is the append order above by
    // construction and cannot drift from it — replaying it with `appendChild` is
    // how `applyHeaderPlan` puts the row back exactly as it was, with no
    // per-element anchor to go stale.
    const controlEls = new Set<HTMLElement>(this.headerControls.map((c) => c.el));
    this.headerTail = (Array.from(header.children) as HTMLElement[]).filter(
      (el) => controlEls.has(el) || el === this.overflowBtn || el === this.overflowMenu
    );

    this.termEl = document.createElement("div");
    this.termEl.className = "pane-term";
    this.el.appendChild(this.termEl);

    this.term = new Terminal({
      allowProposedApi: true,
      cursorBlink: true,
      cursorStyle: "bar",
      fontFamily: FONT.mono,
      fontSize: TERM_METRICS.fontSize,
      lineHeight: TERM_METRICS.lineHeight,
      scrollback: 10000,
      theme: TERM_THEME,
    });
    this.term.loadAddon(this.fit);
    this.term.loadAddon(new WebLinksAddon());
    this.term.loadAddon(new Unicode11Addon());
    this.term.unicode.activeVersion = "11";

    // Shell integration: the shell emits OSC 7 with its working directory on
    // every prompt (see PWSH_CWD_HOOK / PROMPT_COMMAND in the backend). The
    // payload is the raw path; consume it and refresh the toolbar.
    this.term.parser.registerOscHandler(7, (payload) => {
      this.onCwdReported(payload);
      return true;
    });

    // OSC 52 copy, the clipboard key handler and the capture-phase paste block are
    // wired by PaneCompose (panecompose.ts), which owns the clipboard paths they call.
    this.compose = new PaneCompose(this);

    // NOTE (#402 second live-demo round): a right-click Copy/Paste context
    // menu briefly lived here. Removed — the menu's paste path was
    // unreliable in practice and the human chose not to iterate on it rather
    // than keep debugging a second right-click-specific native-event path.
    // Right-click on a terminal is back to its pre-#370 behavior: nothing (a
    // no-op past the global `.pane-term` contextmenu preventDefault in
    // main.ts, which only suppresses the browser's own native menu and
    // predates this PR). Copy still works via Ctrl+C or Ctrl+Shift+C above
    // (selection → copyToClipboard) — see docs/design/clipboard.md's #370
    // section for the supported copy/paste surface.

    this.el.addEventListener("mousedown", () => {
      this.events.onFocus(this);
      // Turning to a flagged pane acknowledges it (clears a latched report).
      this.acknowledgeAttention();
    });

    // Keep the cursor row visible under the git overlay as output arrives.
    this.term.onCursorMove(() => this.scheduleShift());

    this.resizeObs = new ResizeObserver(() => this.applyFit());

    // The header's OWN size decides the overflow fold — never the terminal's,
    // and nothing on this path calls `fit()` or `resizePty` (constraint 1).
    // Two targets: the header (a pane resize) and the meta box (a CHIP
    // appearing or a folder/branch label changing, which leaves the header's own
    // box untouched but takes room out of the same row — the meta box is the
    // header's flex spacer, so every such change shows up as its width moving).
    this.headerObs = new ResizeObserver((entries) => {
      for (const e of entries) {
        if (e.target !== header) continue;
        const box = e.contentBoxSize?.[0];
        this.headerContentW = box ? box.inlineSize : e.contentRect.width;
      }
      this.scheduleHeaderSync();
    });
    this.headerObs.observe(header);
    this.headerObs.observe(meta);

    this.setName("shell");
  }

  // ------------------------------------------------------------------
  // Header overflow (#2191). See docs/design/pane-header.md and the policy in
  // src/paneheader.ts; this half owns pixels, elements and dismissal only.
  // ------------------------------------------------------------------

  /** Coalesce the overflow pass to the end of a resize burst. */
  private scheduleHeaderSync(): void {
    if (this.disposed) return;
    clearTimeout(this.headerSyncTimer);
    this.headerSyncTimer = window.setTimeout(() => this.syncHeaderOverflow(), HEADER_SYNC_MS);
  }

  /** Measure the header, ask the policy, and move whatever it names. */
  private syncHeaderOverflow(): void {
    if (this.disposed) return;
    const controls: HeaderControl[] = [];
    for (const c of this.headerControls) {
      const width = naturalWidth(c.el);
      // 0 means the control is not in this pane's layout at all — `hidden`, or
      // `display: none` by a pane-kind rule. Not a zero-width control: a control
      // this pane does not have, so it is not the policy's business.
      if (width > 0) controls.push({ id: c.id, width, priority: c.priority });
    }
    // Two numbers, because the ladder needs both: what the chrome WANTS decides
    // when to fold (folding is how the row buys it that room), and what it
    // cannot give up decides the two rungs below the fold, which are what
    // happens after it has already given way (#2335).
    const chrome = this.measureHeaderFixed();
    const plan = planHeaderFit({
      headerWidth: this.headerContentW,
      fixedWidth: chrome.want,
      controls,
      // Measurable only while folded (it is `hidden` otherwise), so an unfolded
      // header uses the width the stylesheet gives it. That constant is
      // deliberately WIDER than one `.pane-btn`, which is what makes the
      // policy's "folding buys nothing" guard refuse to fold a header down to a
      // single control — a welcome pane, whose only control is its ✕.
      overflowWidth: naturalWidth(this.overflowBtn) || OVERFLOW_BTN_W,
      chromeFloorWidth: chrome.floor,
      stage: this.headerStage,
    });
    // The strip is placed from the ⋯ button's rect, so a pane that resized under
    // an OPEN menu has a stale `left` even when the fold decision has not moved.
    // Before the early return, not after it.
    if (this.overflowOpen) this.positionOverflowMenu();

    // The early return is against the PLACEMENT, not against the fold flag, and
    // the difference is a real case: a control the pane un-hides while the
    // header is ALREADY folded (`start()` revealing an orchestrator pane's six
    // toggles, a promotion) was not in the control set when the fold ran, so it
    // is sitting inline in a folded header. The flag has not moved, so a
    // flag-keyed check would leave it there — visible, non-priority, beside the
    // ⋯ that is supposed to stand for it. These are parent reads, not layout.
    const want = new Set(plan.overflow);
    const misplaced = this.headerControls.some(
      (c) => (c.el.parentElement === this.overflowMenu) !== want.has(c.id)
    );
    if (plan.stage === this.headerStage && !misplaced) return;
    this.applyHeaderPlan(plan, want);
  }

  /** The header items that are neither the pane name nor a control — the CLI
   *  mark, the role badge, the status chips, and the folder/branch items —
   *  measured twice over.
   *
   *   - `want` is each item at its natural, unshrunk width. That is what decides
   *     when to FOLD, because folding is how the row buys the chrome the room it
   *     is asking for.
   *   - `floor` is what the same items cannot give up however hard flexbox
   *     squeezes them. That is what prices the two rungs below the fold, which
   *     are what happens after the chrome has already given way (#2335).
   *
   *  Walked off the header rather than listed, so a chip added later is counted
   *  in both without anyone remembering to add it here.
   *
   *  Two children are skipped for the same reason — they GROW, so their rendered
   *  box is the row's leftover space rather than anything they need: the meta box
   *  (`flex: 1`, the header's spacer — its ITEMS are measured instead) and the
   *  rename input that replaces the title while F2 is open (`flex: 1`). The meta
   *  box contributes nothing to `floor`: it is `min-width: 0; overflow: hidden`,
   *  so it clips its items rather than pushing the row wider. */
  private measureHeaderFixed(): { want: number; floor: number } {
    let want = 0;
    let floor = 0;
    for (const child of Array.from(this.headerEl.children) as HTMLElement[]) {
      if (child === this.titleEl || child === this.overflowMenu) continue;
      if (child.classList.contains("pane-title-input")) continue;
      if (child === this.metaEl) {
        for (const item of Array.from(child.children) as HTMLElement[]) {
          const w = naturalWidth(item);
          if (w > 0) want += w + META_GAP_W;
        }
        continue;
      }
      if (this.headerTail.includes(child)) continue;
      const w = naturalWidth(child);
      if (w > 0) {
        want += w + HEADER_GAP_W;
        // The gap is on the floor side too: `flex-shrink` scales children, never
        // the row's `gap`, so an item that is present costs one whatever happens
        // to its content.
        floor += irreducibleWidth(child) + HEADER_GAP_W;
      }
    }
    return { want, floor };
  }

  /** Move the folded set into the menu (or back), preserving header order in
   *  both directions, and render the plan's treatment of the pane name. Called
   *  only when the stage actually moves (or a control was found in the wrong
   *  home). */
  private applyHeaderPlan(plan: HeaderFitPlan, overflowIds: Set<string>): void {
    const folded = plan.folded;
    this.headerStage = plan.stage;
    this.headerFolded = folded;
    if (!folded) this.closeOverflowMenu();
    // Replaying the whole sequence re-establishes order in both directions with
    // no per-element anchor, but `appendChild` DETACHES and re-attaches, which
    // blurs anything focused inside — and a fold is triggered by a resize, which
    // a keyboard user can be sitting in the middle of. Restored below.
    const focused = document.activeElement;
    const idOf = new Map<HTMLElement, string>(this.headerControls.map((c) => [c.el, c.id]));
    for (const el of this.headerTail) {
      const id = idOf.get(el);
      const home = folded && id !== undefined && overflowIds.has(id) ? this.overflowMenu : this.headerEl;
      home.appendChild(el);
    }
    this.overflowBtn.hidden = !folded;
    this.el.classList.toggle("header-folded", folded);
    // `minimal` is the rung where the name leaves the row, and this class is what
    // takes it out — a name clipped to one glyph carries nothing and still spends
    // the room ⋯ needs, so the menu's first row carries it instead (#2335). That
    // the row FITS is a separate promise and a separate rule: `styles.css` makes
    // every non-control header child shrinkable at every width, so flexbox
    // absorbs the deficit rather than overflowing and letting `.pane`'s
    // `overflow: hidden` clip the controls off the right end. Header chrome only:
    // the header's height is fixed at every rung, so `.pane-term` does not move
    // and no PTY is resized (constraint 1).
    this.el.classList.toggle("header-minimal", plan.title === "hidden");
    const menuIds = overflowMenuIds(plan);
    this.renameEntry.hidden = !menuIds.includes(RENAME_ENTRY_ID);
    if (!this.renameEntry.hidden) this.syncRenameEntry();
    if (
      focused instanceof HTMLElement &&
      focused !== document.activeElement &&
      this.el.contains(focused) &&
      !focused.hidden
    ) {
      if (this.overflowMenu.contains(focused)) {
        // The control the human was ON is the one that just folded, and a plain
        // `focus()` here is REFUSED: the strip is `visibility: hidden` until
        // opened, and a hidden subtree is not a focusable area, so focus would
        // silently fall to <body> and the next Tab would restart at the top of
        // the document (rev-final round 1 — the fold direction is the only one
        // that moves a control into the menu, so this was exactly the direction
        // the "restored below" comment did not cover).
        //
        // Opening the strip and giving the control its focus back is better than
        // the alternative of parking on ⋯: it is where the human was, and this
        // only ever runs while they are actively keyboarding the header. Pinned,
        // because a hover-opened strip dismisses itself and nothing here is a
        // hover.
        this.openOverflowMenu();
        this.overflowPinned = true;
        focused.focus();
      } else {
        // Guarded like Escape's hand-back: re-focusing ⋯ must not count as the
        // keyboard opening the menu.
        this.overflowRefocusing = true;
        focused.focus();
        this.overflowRefocusing = false;
      }
    }
  }

  /** Everything that OPENS the strip. Split from the dismissal half below
   *  (rev-final round 1): the two are wired flat, so this was length rather
   *  than complexity, and length is what a reader pays for. */
  private wireOverflowOpen(): void {
    const btn = this.overflowBtn;

    btn.addEventListener("pointerenter", () => this.openOverflowMenu());
    // Focus opens it — that is the keyboard route in. Suppressed for the ONE
    // focus this class produces itself: Escape closes the menu and hands focus
    // back to ⋯, and without the guard that hand-back re-opens what Escape just
    // closed. `focus()` dispatches synchronously, so the flag is scoped to
    // exactly that call.
    btn.addEventListener("focus", () => {
      if (this.overflowRefocusing) return;
      this.openOverflowMenu();
    });
    btn.addEventListener("click", (e) => {
      // A click PINS the menu open, and a second one closes it. Hover alone is
      // not enough for every way this gets used: a touch pointer never hovers,
      // and a hover-opened menu that dismisses itself while the pointer crosses
      // the gap makes the icons feel like they are running away. Pinned, the
      // menu stays until Escape, a click away, or a click back on ⋯.
      e.stopPropagation();
      if (this.overflowPinned) {
        this.closeOverflowMenu();
      } else {
        this.openOverflowMenu();
        this.overflowPinned = true;
      }
    });
  }

  /** Everything that CLOSES the strip: the hover grace, Escape, tabbing out,
   *  clicking away, and picking a control. */
  private wireOverflowDismissal(): void {
    const btn = this.overflowBtn;
    const menu = this.overflowMenu;

    menu.addEventListener("pointerenter", () => {
      clearTimeout(this.overflowCloseTimer);
    });
    for (const el of [btn, menu]) {
      el.addEventListener("pointerleave", () => this.scheduleOverflowClose());
      el.addEventListener("keydown", (e) => {
        if ((e as KeyboardEvent).key !== "Escape") return;
        // The open check comes BEFORE `stopPropagation`, and that order is the
        // whole of it (rev-final round 1). Stopping the bubble unconditionally
        // swallowed every Escape pressed while focus sat on ⋯ or inside the
        // strip — including the one right after Escape closed it and handed
        // focus back — and `src/main.ts`'s window keydown, which cancels a
        // pending cross-workspace connect, is a BUBBLE-phase listener and so
        // the one Escape consumer that swallow could reach. Only an Escape this
        // menu actually consumes may be stopped.
        if (!this.overflowOpen) return;
        e.stopPropagation();
        this.closeOverflowMenu(true);
      });
      // Tabbing (or clicking) out of the pair closes it. `relatedTarget` is the
      // element GAINING focus, so this fires once for the whole pair rather than
      // once per item.
      el.addEventListener("focusout", (e) => {
        const next = (e as FocusEvent).relatedTarget as Node | null;
        if (next && (btn.contains(next) || menu.contains(next))) return;
        this.closeOverflowMenu();
      });
    }
    // The menu is a header child (so that Tab from ⋯ lands inside it), which puts
    // it under the grid's pane-reorder pointerdown. Its buttons are already
    // exempt there; this covers the strip's own padding, which is not a button.
    menu.addEventListener("pointerdown", (e) => e.stopPropagation());
    // Picking a control dismisses the strip, as any menu does. CAPTURE phase,
    // because every folded button calls `stopPropagation` in its own handler —
    // these are the header's buttons, unchanged, and that call is what stops a
    // header click from also focusing the pane. The close is deferred to a
    // microtask so it lands AFTER the action's own listener has run.
    menu.addEventListener(
      "click",
      (e) => {
        if (e.target === menu) return; // the strip's own padding
        queueMicrotask(() => this.closeOverflowMenu());
      },
      true
    );
  }

  private openOverflowMenu(): void {
    if (this.disposed || !this.headerFolded || this.overflowOpen) return;
    clearTimeout(this.overflowCloseTimer);
    this.overflowOpen = true;
    this.positionOverflowMenu();
    this.overflowMenu.classList.add("open");
    this.overflowBtn.setAttribute("aria-expanded", "true");
    // Click-away. Registered per OPEN rather than per pane: a document listener
    // that every pane keeps alive is one per terminal on the wall.
    this.overflowAwayListener = (e: PointerEvent) => {
      const t = e.target as Node | null;
      if (t && (this.overflowBtn.contains(t) || this.overflowMenu.contains(t))) return;
      this.closeOverflowMenu();
    };
    document.addEventListener("pointerdown", this.overflowAwayListener, true);
  }

  private closeOverflowMenu(returnFocus = false): void {
    clearTimeout(this.overflowCloseTimer);
    if (this.overflowAwayListener) {
      document.removeEventListener("pointerdown", this.overflowAwayListener, true);
      this.overflowAwayListener = null;
    }
    this.overflowPinned = false;
    if (!this.overflowOpen) return;
    this.overflowOpen = false;
    this.overflowMenu.classList.remove("open");
    this.overflowBtn.setAttribute("aria-expanded", "false");
    if (returnFocus && !this.overflowBtn.hidden) {
      this.overflowRefocusing = true;
      this.overflowBtn.focus();
      this.overflowRefocusing = false;
    }
  }

  private scheduleOverflowClose(): void {
    if (this.overflowPinned) return; // a clicked-open menu waits to be dismissed
    clearTimeout(this.overflowCloseTimer);
    this.overflowCloseTimer = window.setTimeout(
      () => this.closeOverflowMenu(),
      OVERFLOW_CLOSE_MS
    );
  }

  /** Place the strip under the ⋯ button, inside the pane. The menu is laid out
   *  even while closed (`visibility: hidden`, not `display: none`), so its width
   *  is readable here without a flash.
   *
   *  Written as a `transform`, never as `left` — see the rule in styles.css. The
   *  strip is `position: absolute` with `width: auto`, so its width is
   *  shrink-to-fit against the space between its `left` and the pane's right
   *  edge; writing the computed offset back as `left` would change the very
   *  width it was computed from, and a strip near the right edge would wrap into
   *  rows it has room not to need. `left: 0` is fixed in the stylesheet and this
   *  slides the laid-out box, which costs layout nothing. */
  private positionOverflowMenu(): void {
    const pane = this.el.getBoundingClientRect();
    const anchor = this.overflowBtn.getBoundingClientRect();
    const left = menuLeftFor(
      anchor.left - pane.left,
      anchor.right - pane.left,
      this.overflowMenu.offsetWidth,
      pane.width
    );
    this.overflowMenu.style.transform = `translateX(${Math.round(left)}px)`;
  }

  /** Mark genuine human input into this pane (#440 B2, review round 3 B2-R;
   *  #518). Called from `term.onKey` (real keyboard events only) and from both
   *  `this.term.paste(...)` call sites (`pasteFromClipboard`,
   *  `pasteToTerminal` — loomux owns paste entirely, #402, so these two are
   *  the only paste vectors). Deliberately NEVER called from `term.onData`
   *  — see that handler's comment for why it can't tell a keystroke from
   *  data xterm generated on its own.
   *
   *  Three consumers, deliberately with different lifetimes. `firstInputMs` is
   *  idempotent — only the first call after a `start`/`respawnFresh` reset
   *  does anything. `humanOrigin` is marked on EVERY call: it is a per-turn
   *  origin flag for the write that is about to follow, not a once-per-process
   *  fact (#518, see `humanorigin.ts`). `activity.noteHumanInput` is marked on
   *  every call too, but is a PANE-level fact that outlives any one process
   *  (#2122 slice A2). All three hang off this one function so "what counts as
   *  human input" has a single answer and a future input vector can only be
   *  added in one place. */
  markFirstInput(): void {
    this.firstInputMs ??= Date.now();
    // #2122 slice A2: a THIRD consumer of this one answer, with a third
    // lifetime — `lastHumanInputMs` is marked on every call like `humanOrigin`,
    // but it is a pane-level fact that also clears the at-prompt latch. It
    // hangs here, and on `markHumanInput` below, for exactly the reason the two
    // above do: "what counts as human input" has one answer, and the answer
    // structurally excludes `term.onData` (#440 B2-R), which is what keeps a
    // copilot boot-time OSC reply from reading as a keystroke.
    this.activity.noteHumanInput(Date.now());
    // #720: the same single answer to "what counts as human input" also decides
    // when this pane's output is an interactive latency again, so the wake hangs
    // here rather than on `onData` (which also fires for xterm's own query
    // auto-replies — see the registration in `start`).
    //
    // STRICTLY BEFORE `humanOrigin.mark()`, and this order is load-bearing.
    // `wakeOutput` can call `term.write`, and xterm parses a write SYNCHRONOUSLY
    // when it lands on an empty buffer right after user input — `WriteBuffer.
    // write`'s `_didUserInput` fast path, which exists to cut echo latency. A
    // parse can make the terminal emit an auto-reply (a DA/OSC answer) through
    // `onData`, and the origin latch's entire correctness argument is that such
    // data "arrives while its own `term.write()` is being parsed — always a
    // different turn" (humanorigin.ts). Marking first would put exactly that
    // write inside the marked turn and hand the backend's keystroke clock a
    // program-generated reply as a human keystroke — the #179/#518 failure,
    // re-created by a perf change. Flushing what the PROGRAM already said, and
    // only then opening the HUMAN's turn, is also the honest order.
    this.lifecycle.wakeOutput();
    this.humanOrigin.mark();
  }

  /** #518: the `markFirstInput` sibling for human input whose data xterm emits
   *  on a later task (an IME commit) or through a path `onKey` never sees
   *  (`input`/`insertText`). Same `firstInputMs` semantics — a composition IS
   *  the human's first input into this process — but the origin mark is the
   *  DEFERRED one, since the write it licenses has not happened yet. See the
   *  listener registration in `start()` and `humanorigin.ts`'s scheduling
   *  note. */
  markHumanInput(): void {
    this.firstInputMs ??= Date.now();
    // #2122 slice A2, and the SAME call as in `markFirstInput` deliberately: an
    // IME commit is the human typing. Reading the latch's human-input clear off
    // one of these two siblings and not the other would be a bypass exactly the
    // width of the composition path (CLAUDE.md: a guard reads every one of its
    // inputs by one rule) — an IME user's keystroke would leave their pane
    // reading `turn-done` after they had already answered it.
    this.activity.noteHumanInput(Date.now());
    this.lifecycle.wakeOutput(); // #720 — before the mark, same load-bearing order as markFirstInput
    this.humanOrigin.markDeferred();
  }

  // Delegated to PaneLifecycle (panelifecycle.ts), which carries the doc (#3498 F1).
  start(opts: PaneOptions = {}, takeFocus = true): Promise<void> { return this.lifecycle.start(opts, takeFocus); }

  /** True when this pane's process is a local ssh client (#887 S3) — the one
   *  fact every local-filesystem suppression in this file reads. */
  get isSshPane(): boolean {
    return this.sshProfileId !== null;
  }

  /** The SSH profile this pane was launched from, or null. */
  get sshProfile(): string | null {
    return this.sshProfileId;
  }

  // Delegated to PaneLifecycle (panelifecycle.ts), which carries the doc (#3498 F1).
  respawnFresh(opts: PaneOptions = {}): Promise<void> { return this.lifecycle.respawnFresh(opts); }

  /** True while an in-place relaunch (#407's promotion) is deliberately tearing
   *  this pane's process down and starting another in the same terminal.
   *
   *  The exit that follows the kill is loomux's OWN — `kill_pty` marks it
   *  expected, which is precisely the shape main.ts's `onPtyExit` reaper reads as
   *  "the human's process ended, retire the pane". Without this flag the reaper
   *  would close the pane mid-promotion, taking the conversation the whole
   *  gesture exists to preserve with it. */
  get isRelaunching(): boolean {
    return this.relaunching;
  }

  /** Mark the start/end of an in-place relaunch (#407). Always paired via
   *  `try/finally` by the caller (orchestration.ts's promote flow) so a failed
   *  promotion can't leave a pane permanently immune to its own exit. */
  setRelaunching(active: boolean): void {
    this.relaunching = active;
  }

  /** Render the welcome / pane-setup surface in this pane instead of a terminal
   *  (#194). No terminal is opened and no PTY is spawned — the pane is inert
   *  content until the user submits, so nothing can trigger a ConPTY resize
   *  before then (constraint 1). `formEl` is the welcome form's root DOM. */
  startWelcome(formEl: HTMLElement): void {
    this.setName("welcome");
    this.el.classList.add("is-welcome");
    const wrap = document.createElement("div");
    wrap.className = "pane-welcome";
    wrap.appendChild(formEl);
    this.welcomeEl = wrap;
    this.el.appendChild(wrap);
  }

  /** True while this pane is showing the welcome form (no PTY yet). */
  get isWelcome(): boolean {
    return this.welcomeEl !== null;
  }

  /** Focus the welcome form's preferred initial control (its `data-initial-focus`
   *  marker — the repository field — falling back to the first focusable). Harmless
   *  on a hidden tab: focusing inside a `display:none` subtree is a no-op. */
  focusWelcome(): void {
    const el =
      this.welcomeEl?.querySelector<HTMLElement>("[data-initial-focus]") ??
      this.welcomeEl?.querySelector<HTMLElement>("select, input, button");
    el?.focus();
  }

  /** Convert a welcome pane into a real terminal: tear down the setup surface and
   *  spawn the chosen kind in place. The PTY is created here — its first and only
   *  spawn — so the welcome-before-PTY flow never resizes anything. */
  async startFromWelcome(opts: PaneOptions = {}): Promise<void> {
    this.welcomeEl?.remove();
    this.welcomeEl = null;
    this.el.classList.remove("is-welcome");
    await this.start(opts, true);
  }

  /** Turn this pane into a CONTENT pane (#214 files, #217 editor + git): its content
   *  becomes one of three views, rooted at `opts.root` —
   *
   *    files  — FileExplorerView: a native-style file MANAGER (browse, open with the
   *             OS default app, new folder/file, rename, delete, jump-to-file). NOT
   *             the in-app editor: the human's ruling on #214 is that a .png belongs
   *             in an image viewer and a .pdf in a PDF reader.
   *    editor — FileEditView: the #174 file tree + code editor + #207 streaming
   *             search, EMBEDDED (no ✕, no Esc-to-close; the pane's ✕ closes it, and
   *             asks first when a buffer is dirty — see confirmClose).
   *    git    — GitView: graph, status, diffs, staging, #208 worktree switching, over
   *             the repo `root` names. Embedded on the same terms.
   *
   *  Used both to convert a welcome pane in place (the user picked the kind) and to
   *  open one directly on restore or from the browser's "open in editor pane".
   *
   *  No terminal is opened and no PTY is ever spawned, so:
   *   - nothing can resize a ConPTY from here (constraint 1 holds by construction —
   *     there is no ConPTY);
   *   - `.pane-term` stays in the layout but empty, and `.pane-content` covers it the
   *     way `.pane-welcome` does, so the pane's own chrome (splits, dock, maximize)
   *     works unchanged;
   *   - the PTY-dependent chrome (folder + branch chips, the git/issues/file-editor
   *     overlay buttons — all of which float over the TERMINAL and are sized from it)
   *     is hidden via `.is-content` rather than left clickable and inert.
   *
   *  Each view fills the content box and lays ITSELF out (all three are `flex: 1`,
   *  and GitView re-clamps its sub-panes against its own live size via its own
   *  ResizeObserver) — which is the whole of the "second sizing model" the git view
   *  needed to become pane content: a box, not a terminal to measure.
   *
   *  `root` must already be what it claims — a readable directory (files/editor) or a
   *  git work tree (git) — validated by the caller at setup and again at restore. */
  startContent(opts: ContentPaneOptions): void {
    this.welcomeEl?.remove(); // converting a setup pane in place
    this.welcomeEl = null;
    this.el.classList.remove("is-welcome");
    // ONE class for all three kinds: the chrome they hide is identical (everything that
    // describes a shell or floats over a terminal), and the surfaces style themselves.
    // A per-kind class would be a hook with nothing on the other end of it.
    this.el.classList.add("is-content");
    this.contentKind = opts.kind;
    this.contentFile = opts.file ?? null;
    this.setContentRoot(opts.root);
    this.setName(opts.name);

    const view = this.buildContentView(opts);
    const wrap = document.createElement("div");
    wrap.className = "pane-content";
    wrap.appendChild(view.el);
    this.el.appendChild(wrap);
    // ATTACH, THEN show. `GitView.show()` clamps its sub-panes against its container's
    // live size, so showing it before it is in the document would measure a zero-width
    // box. (Its ResizeObserver would recover on the next frame, but a view that has to
    // be rescued by a resize event is a view that flashes wrong first.)
    view.show();
    // The editor pane may have been opened ON a file (the browser's "open in editor
    // pane"). `openPath` waits for the listing show() just kicked off, so the reveal
    // lands in the tree that ends up on screen rather than racing it.
    if (opts.file && this.editorPaneView) void this.editorPaneView.openPath(opts.file);
    if (!opts.background) this.focus();
  }

  /** Construct the view a content pane hosts (not shown yet — see startContent). Split
   *  out so the per-kind wiring reads as three cases, not one branching block. */
  private buildContentView(opts: ContentPaneOptions): { el: HTMLElement; show(): void } {
    // Re-rooting from a view's own folder picker re-roots the PANE, so the persisted
    // record follows and a restore reopens what was actually on screen.
    //
    // The TITLE follows only if it was auto-derived from the old root — the same
    // "don't clobber what the human typed" rule the welcome form's name field uses
    // (nameDirty). A pane the user renamed to "docs" keeps that name across a re-root;
    // one still called "loomux" (its old folder) becomes the new folder, instead of
    // sitting there naming a directory it no longer shows.
    const adoptRoot = (root: string): void => {
      const autoNamed = this.name === this.defaultContentName(this.contentRoot);
      this.setContentRoot(root);
      if (autoNamed) this.setName(this.defaultContentName(root));
      // Re-persist NOW. No grid event fired (nothing opened or closed), so without this
      // the new root would sit unsaved until some unrelated layout change came along —
      // and a quit in between would restore the old one (rev-99 finding 4).
      this.events.onRecordChanged(this);
    };

    if (opts.kind === "files") {
      this.filesView = new FileExplorerView({
        getRoot: () => this.contentRoot ?? "",
        onRootChanged: adoptRoot,
        // Right-click → "Open in file editor pane" (#217): an editor pane beside this
        // one, rooted where this browser is rooted, with the clicked file open. The
        // browser hands over a root-relative path and stays exactly where it was.
        onOpenEditorPane: (req) =>
          this.events.onOpenEditorPane(this, {
            name: req.file ? pathTail(req.file) : this.defaultContentName(req.root) || "editor",
            root: req.root,
            file: req.file ?? undefined,
          }),
        // Right-click a .yml → "Open in workflow pane" (#222). Named after the FILE, like
        // the editor pane is: a pane called "workflow.yml" says what it is showing, while
        // one called after the repo would collide with every other pane in it.
        onOpenWorkflowPane: (req) =>
          this.events.onOpenWorkflowPane(this, {
            name: pathTail(req.file) || "workflow",
            root: req.root,
            file: req.file,
          }),
      });
      return this.filesView;
    }

    if (opts.kind === "editor") {
      this.editorPaneView = new FileEditView({
        getCwd: () => this.contentRoot,
        // Never called: `embedded` drops the ✕ and the Esc binding, which are the only
        // two things that request a close. The pane's own ✕ is the close affordance.
        onClose: () => {},
        embedded: true,
        onRootChanged: adoptRoot,
      });
      return this.editorPaneView;
    }

    if (opts.kind === "structured") {
      // #2891: the pane cell holds a DOM transcript rather than a terminal. There is no
      // PTY behind it, so constraint 1 holds by construction  nothing in this view
      // measures or resizes one, and `.is-content` already hides the chrome that floats
      // over a terminal.
      //
      // The two ids are required rather than defaulted: a structured pane is a view of ONE
      // agent's event log, and a pane built around an agent nobody named would silently
      // render an empty transcript forever.
      if (!opts.groupId || !opts.agentId) {
        throw new Error("a structured pane needs both groupId and agentId");
      }
      const view = new StructuredPaneView({
        groupId: opts.groupId,
        agentId: opts.agentId,
        cli: opts.cli ?? null,
        // �3.5: every agent may be asked and NO AGENT MAY EVER ANSWER. The caller supplies
        // the trusted path; refusing outright beats a silent no-op, which would look to
        // the human exactly like an answer that went nowhere.
        answer:
          opts.answer ??
          (() => Promise.reject(new Error("this structured pane has no answer channel"))),
      });
      this.structuredPaneView = view;
      // Bind the view to its agent so `orch-pane-event` can reach it in O(1). The
      // matching unregister rides `notifyPaneDisposed`, which `dispose()` already
      // calls — one lifecycle, not two.
      registerStructuredPane(opts.agentId, view);
      return view;
    }

    if (opts.kind === "todo") {
      // #3263 S4. The one content kind whose `root` is NOT a thing it opens:
      // every other kind fails without a readable directory, where this one
      // reads a list the backend keys off the root and falls back to the GLOBAL
      // list when there is no root at all. So the view takes the root as a
      // getter and decides for itself whether the workspace half of its scope
      // switch is available — it never probes, and there is nothing here to
      // fail soft to. `docs/design/todo-pane.md` §"The pane" is the argument.
      //
      // The root goes to the backend RAW. The frontend must never name a
      // workspace KEY (§"The caller names a ROOT, never a key"), so there is
      // deliberately no normalisation on this path.
      this.todoPaneView = new TodoPaneView({
        getRoot: () => this.contentRoot,
        onClose: () => {}, // never called — see the editor's note above
        embedded: true,
      });
      return this.todoPaneView;
    }

    if (opts.kind === "workflow") {
      // The workflow file rides in `file`, exactly as the editor's open file does, so the
      // capture/restore path needed no new field (tabstore.ts). Absent = the repo's
      // default workflow path, which is what the welcome form creates.
      this.workflowPaneView = new WorkflowView({
        getRoot: () => this.contentRoot,
        getFile: () => this.contentFile ?? WORKFLOW_FILE,
        // The pane's own in-header picker moved to another workflow (#2944). Three things
        // follow the file and none of them follows on its own: `contentFile` (what a
        // re-`show()` re-opens), the pane's NAME (which is the file's tail, exactly as the
        // file browser's "Open in workflow pane" set it), and the persisted record — which
        // reads `openPathRel` and so is already right, but only gets WRITTEN when something
        // says the record changed. Renaming is conditional on the name still being the one
        // we gave it, the same `autoNamed` rule a re-root obeys: a pane the human has
        // renamed keeps the name they chose.
        onFileChanged: (rel) => {
          // "Did WE name this pane, or did the human?" — the same question `adoptRoot` asks
          // before renaming on a re-root, and it has to be asked against every derivation a
          // workflow pane's name has ever come from, not just the one this pane happened to
          // take: the launcher names a `default` pane after the REPO and a named one after
          // the WORKFLOW, and the file browser names it after the FILE. Checking only the
          // last would leave the other two stuck on a name for the file they left.
          const auto = new Set(
            [
              pathTail(this.contentFile ?? ""),
              workflowNameOf(this.contentFile ?? ""),
              this.defaultContentName(this.contentRoot),
              "workflow",
            ].filter((n): n is string => !!n)
          );
          const autoNamed = auto.has(this.name);
          this.contentFile = rel;
          // Named after the WORKFLOW where the file is one of the repo's — the launcher's own
          // convention, and the thing a human is looking for in a tab strip — and after the
          // file otherwise, which is all an arbitrary `.yml` has.
          if (autoNamed) this.setName(workflowNameOf(rel) || pathTail(rel) || "workflow");
          this.events.onRecordChanged(this);
        },
        onClose: () => {}, // never called — see the editor's note above
        embedded: true,
      });
      return this.workflowPaneView;
    }

    this.gitPaneView = new GitView({
      getCwd: () => this.contentRoot,
      onClose: () => {}, // never called — see the editor's note above
      embedded: true,
    });
    return this.gitPaneView;
  }

  /** The working directory this pane was launched in, as the human spelled it —
   *  null for a pane that has none (a content pane, an ssh pane, a terminal in
   *  home). ONE reader: the `Alt+J` open-or-focus shortcut (#3263 S4), which
   *  roots a new To-Do pane at the active pane's project.
   *
   *  RAW, deliberately. It is the same string `openInEditor` hands out, and the
   *  To-Do backend is the thing that turns a root into a workspace key — the
   *  frontend normalising it here would be a second answer to that question
   *  (`docs/design/todo-pane.md` §"The caller names a ROOT, never a key"). */
  get cwdForRooting(): string | null {
    return this.cwdRaw ?? this.contentRoot;
  }

  /** This pane's To-Do view (#3263 S4), or null on every other kind. Exposed for
   *  ONE reader — the `open-or-focus` shortcut in `main.ts`, which focuses an
   *  already-open todo pane rather than opening a second one. The kind itself
   *  stays private, as `isContent`'s note says it must. */
  get todoView(): TodoPaneView | null {
    return this.todoPaneView;
  }

  /** This pane's structured transcript view (#2891), or null on every other kind.
   *  Exposed for ONE reader — `orchestration.ts`'s registry, which drops its entry
   *  by identity when the pane is disposed. The kind itself stays private, as
   *  `isContent`'s note below says it must: nothing else needs to know WHICH
   *  surface this is. */
  get structuredView(): StructuredPaneView | null {
    return this.structuredPaneView;
  }

  /** True when this pane is a PTY-less content pane (#214 files, #217 editor / git,
   *  #2891 structured) — no PTY, ever. The kind itself stays private: nothing outside
   *  needs to know WHICH surface it is, and the moment something does, it should ask a
   *  question about the behavior it cares about rather than switch on the kind. */
  get isContent(): boolean {
    return this.contentKind !== null;
  }

  /** The human asked to close this pane — from its header ✕, its dock chip's ✕, or
   *  Ctrl+Shift+W. THE single entry point for a human-initiated single-pane close, so
   *  every affordance goes through one path instead of re-deriving it (the dock chip's
   *  ✕ used to call `grid.closePane` directly — #214 rev-100).
   *
   *  It runs the unsaved-edits guard (`confirmClose`) HERE, and only tells the host to
   *  close once the human has said yes. Anything calling `grid.closePane` directly
   *  bypasses that guard — exactly the bug this method exists to prevent, now that a
   *  pane can hold a dirty buffer (an editor pane's, or an Alt+F overlay's).
   *
   *  `closing` is a one-shot latch, and it is load-bearing: the guard is ASYNC (a
   *  modal), while the app's shortcut handler is registered capture-phase on
   *  `document` — so a second Ctrl+Shift+W while the discard dialog is up still
   *  reaches this method. Without the latch it would stack a second identical dialog
   *  for the same pane, and answering both would re-enter `closePane` on a disposed
   *  pane. Released on a declined close so the human can try again.
   *
   *  Automatic closes (a PTY exiting, a group ending, a tab disposing) deliberately do
   *  NOT come here — they are bulk operations with their own semantics, not "the human
   *  closed this pane". The tab-close path asks its own question (see
   *  `hasUnsavedWork` / tabbar's arm-and-confirm). */
  requestClose(): void {
    if (this.closing || this.disposed) return;
    // A LEAD pane (#2519) asks first, and asks the way this app already asks
    // about something irreversible: ARM, then confirm. Closing one ends its
    // whole group — every child pane it spawned, their agents killed — and that
    // is not what "close this pane" means anywhere else in the app.
    //
    // Arm-and-confirm rather than the modal `confirmClose` uses, matching
    // `tabbar.ts`'s destructive tab close and `groupview.ts`'s "End
    // orchestration": the human has met this affordance, it is synchronous, and
    // it says what is at stake in the button's own tooltip. The unsaved-edits
    // modal still runs underneath — `confirmClose` is asked below either way,
    // so a lead pane holding a dirty Alt+F buffer asks both questions, in the
    // order the human meets them (the cheap one first).
    if (this.isLead && this.leadCloseArmedAt === null) {
      this.armLeadClose();
      return;
    }
    this.disarmLeadClose();
    this.closing = true;
    void this.confirmClose().then(
      (ok) => {
        this.closing = false;
        if (!ok || this.disposed) return;
        if (this.isLead) {
          this.endLeadGroup();
          return;
        }
        this.events.onCloseRequest(this);
      },
      () => {
        this.closing = false; // a failed dialog must not wedge the pane shut
      }
    );
  }

  /** True while a close request is waiting on the human's answer. */
  private closing = false;

  /** The pane's own — button, retained so the lead close confirm (#2519) can
   *  repaint it into its armed state. */
  private closeBtn: HTMLButtonElement | null = null;
  /** When the lead close was armed, or null when it is not armed. A timestamp
   *  rather than a boolean because the disarm TIMER is not the only thing that
   *  can end the window, and a boolean would need a second field to say so. */
  private leadCloseArmedAt: number | null = null;
  private leadCloseTimer: number | null = null;

  /** Arm the lead-pane close: repaint the — button as a confirm and start the
   *  4 s auto-disarm, the same window `tabbar.ts` and `groupview.ts` use.
   *
   *  The COUNT is read at arm time and named in the tooltip, because "end 3
   *  subagents" is the fact the human is actually deciding about and a bare
   *  "are you sure" is not. It is a reading, not a subscription: a child that
   *  spawns during the 4 s window is not re-counted, and the confirm still ends
   *  the whole group — the tooltip is what the human was told, and `endGroup`
   *  is what happens. */
  private armLeadClose(): void {
    this.disarmLeadClose();
    this.leadCloseArmedAt = Date.now();
    const n = this.events.leadChildCount?.(this) ?? null;
    if (this.closeBtn) {
      this.closeBtn.classList.add("confirm");
      this.closeBtn.textContent = "✕?";
      this.closeBtn.title =
        n === null || n === 0
          ? "Click again to close this lead pane and end its group"
          : n === 1
            ? "Click again — this ends 1 subagent"
            : `Click again — this ends ${n} subagents`;
    }
    this.leadCloseTimer = window.setTimeout(() => this.disarmLeadClose(), 4000);
  }

  /** Put the — button back. Safe to call when nothing is armed (the ordinary
   *  close path calls it unconditionally) and on a disposed pane. */
  private disarmLeadClose(): void {
    if (this.leadCloseTimer !== null) {
      clearTimeout(this.leadCloseTimer);
      this.leadCloseTimer = null;
    }
    if (this.leadCloseArmedAt === null) return;
    this.leadCloseArmedAt = null;
    if (this.closeBtn) {
      this.closeBtn.classList.remove("confirm");
      this.closeBtn.textContent = "✕";
      this.closeBtn.title = "Close pane";
    }
  }

  /** End the group this lead owns, which is what closes its panes — this one
   *  included. The `orch-group-ended` event the backend emits is the ONE
   *  teardown path (`orchestration.ts`'s listener closes every pane in the
   *  group), so this deliberately does not also call `onCloseRequest` on the
   *  way in: two teardowns for one gesture is how a pane gets disposed twice.
   *
   *  `cleanupWorktrees: false` — the children's worktrees hold their work, and
   *  a human closing the pane they were helping in has not asked for that to be
   *  deleted. The Git view and `git worktree list` still find them.
   *
   *  **A FAILED end still closes the pane, and SAYS SO** (review round 1, B2).
   *  The pane closes because the human said close and the alternative is a pane
   *  that refuses to — but what is left behind when `orch_end_group` rejects is
   *  a group whose child agents are still running, with the pane that could
   *  steer them gone. On this feature's own default guardrails (`0 = off` for
   *  idle-kill and spawn rate) those helpers run unbounded, so a silent swallow
   *  here would strand live processes with nothing anywhere saying it happened.
   *  The toast names the group, which is what a human needs to end it by hand
   *  from another pane. Same standard, and the same channel, as a failed mint on
   *  the launch and restore paths. */
  private endLeadGroup(): void {
    const group = this.orchGroup;
    if (!group) {
      this.events.onCloseRequest(this);
      return;
    }
    void endGroup(group, false).catch((err) => {
      showToast(
        `Couldn't end group ${group}: ${String(err)}. Its agents may still be running — end it from another pane.`,
        "error"
      );
    }).finally(() => {
      // The group-ended event normally disposed this pane already; this is the
      // path where it did not (the call failed, or the event was lost).
      if (!this.disposed) this.events.onCloseRequest(this);
    });
  }

  /** The view in this pane that owns unsaved work, if any — asked ONCE, here, so every
   *  guard (close, tab-close, app-quit, a dead process) sees the same set of holders and
   *  a new one cannot be wired into two of the four and forgotten in the others.
   *
   *  THREE can hold a buffer: the editor PANE (#217 — the pane IS the buffer), the
   *  WORKFLOW pane (#222 — same, over the workflow file), and the Alt+F OVERLAY
   *  inside any terminal/agent pane (#174) — the likeliest one, because it is the one you
   *  forget you left open. They share the contract (`dirty` / `canDiscard` / `bufferReport`)
   *  rather than a base class: it is three methods, and the shared gate that matters is
   *  the pure one in dirtystate.ts. */
  private unsavedHolder(): {
    readonly dirty: boolean;
    canDiscard(): Promise<boolean>;
    bufferReport(): { file: string | null; dirty: boolean } | null;
  } | null {
    return this.editorPaneView ?? this.workflowPaneView ?? this.views.fileEditView;
  }

  /** May the human close this pane right now? True unless it holds unsaved work they
   *  then decline to discard. */
  async confirmClose(): Promise<boolean> {
    const holder = this.unsavedHolder();
    return holder ? holder.canDiscard() : true;
  }

  /** Does this pane hold unsaved edits RIGHT NOW — without asking the human anything?
   *  `confirmClose` prompts; this only reports. The tab-close path needs the fact
   *  before it can decide how to ask (a tab holding unsaved work closes behind the
   *  same arm-and-confirm an orchestration tab does — tabbar.ts), and a question that
   *  pops its own modal is no use to a synchronous bulk teardown. */
  hasUnsavedWork(): boolean {
    return this.unsavedHolder()?.dirty ?? false;
  }

  /** Point this content pane at `root`, keeping `cwdRaw` in step so the chrome that
   *  legitimately works without a PTY — "open in editor", the capture's cwd —
   *  targets the folder actually on screen. */
  private setContentRoot(root: string): void {
    this.contentRoot = root;
    this.cwdRaw = root;
  }

  /** The title a content pane gets when nobody has named it: the root's short name.
   *  The SAME derivation the welcome form uses, so `name === defaultContentName(root)`
   *  reliably means "this title was auto-derived, not typed by the human". */
  private defaultContentName(root: string | null): string {
    return root ? pathTail(root) || root : "";
  }

  /** This pane's working directory: a shell's live (OSC 7) cwd, an agent's launch
   *  folder, or a files pane's root. Null when it has none yet (a welcome pane).
   *  Used to seed a split's welcome form with the folder you split FROM. */
  get workdir(): string | null {
    return this.cwdRaw;
  }

  // Delegated to PaneLifecycle (panelifecycle.ts), which carries the doc (#3498 F1).
  startDormant(record: PersistedPane, contentEl: HTMLElement): void {
    this.lifecycle.startDormant(record, contentEl);
  }
  get isDormant(): boolean { return this.lifecycle.isDormant; }
  showReconnectCard(el: HTMLElement): () => void { return this.lifecycle.showReconnectCard(el); }
  get dormantKind(): PersistedPaneKind | null { return this.lifecycle.dormantKind; }
  startFromDormant(opts: PaneOptions = {}): Promise<void> { return this.lifecycle.startFromDormant(opts); }
  setHidden(hidden: boolean): void { this.lifecycle.setHidden(hidden); }
  serializeViewportHtml(): string { return this.lifecycle.serializeViewportHtml(); }

  private fitTimer: number | undefined;
  /** Last grid size actually confirmed by the PTY, as `cols x rows`. Resizing
   *  ConPTY is never free (the inbox Win10 conhost repaints the whole
   *  screen, which TUIs then duplicate into scrollback), so same-size calls
   *  are skipped. Only latches on a *successful* `resizePty` (#432 item 3) —
   *  see `doResize`. */
  sentSize = "";
  /** The size of a `resizePty` call currently in flight, or null. Lets a
   *  later fit tick tell "identical call already outstanding, don't re-send"
   *  apart from "genuinely new size, do send" while `sentSize` hasn't
   *  latched yet (#432 item 3). */
  private resizePending: string | null = null;
  /** >0 while a divider drag (grid split or this pane's own embed-slot
   *  divider) is coalescing resizes (#432 item 1). See `beginResizeHold`. */
  private resizeHolds = 0;

  /** When the current unbroken run of geometry changes began (#1149), or null
   *  while this pane is quiet. Cleared wherever a fit actually runs — `runFit`
   *  is the single place that does both — because a ceiling measured from a
   *  burst that is already over binds on the very next tick. */
  private fitBurstStartMs: number | null = null;

  applyFit(): void {
    // Coalesce a burst of geometry changes into ONE fit (#1149). The debounce
    // this replaced was a fixed 16 ms, which is NARROWER than the interval
    // between the ResizeObserver deliveries it was debouncing (once per frame,
    // 16.7 ms at 60 Hz) — so it coalesced nothing, and anything animated fit,
    // reflowed and resized the ConPTY once per frame for its whole duration.
    // `#sessions` animates its width over 240 ms, so a single toggle cost ~15
    // ResizePseudoConsole calls PER PANE (CLAUDE.md constraint 1). `planFit`
    // waits for the geometry to settle instead, with a ceiling so a gesture
    // that never settles still reflows periodically — see resizeburst.ts for
    // both constants and why the ceiling has to clear the transition.
    const plan = planFit({
      nowMs: Date.now(),
      burstStartMs: this.fitBurstStartMs,
      windowMs: FIT_WINDOW_MS,
      maxWaitMs: FIT_MAX_WAIT_MS,
    });
    this.fitBurstStartMs = plan.burstStartMs;
    clearTimeout(this.fitTimer);
    this.fitTimer = window.setTimeout(() => this.runFit(), plan.dueInMs);
  }

  /** Run the debounced fit and end the burst it belonged to. Every path that
   *  fits goes through here — the timer above and `endResizeHold`'s immediate
   *  flush — so no path can leave a stale burst start behind. */
  private runFit(): void {
    this.fitBurstStartMs = null;
    this.doFit();
  }

  /** Begin coalescing this pane's PTY resizes: `doResize` keeps fitting
   *  xterm's own buffer on every debounced tick (so the terminal renders at
   *  the right size throughout a drag) but withholds the
   *  `ResizePseudoConsole` IPC call until the last matching `endResizeHold`
   *  releases — collapsing what would otherwise be one resize per animation
   *  frame for the whole drag into a single call issued at drag-end (#432
   *  item 1). Call around any drag that can change this pane's `termEl`
   *  size: grid.ts's split divider (on every pane in the grid, since a
   *  nested split's drag can resize leaves it doesn't directly touch) and
   *  this pane's own `wireEmbedDivider`. The overlay height divider does
   *  NOT call this — it never touches `termEl` (CLAUDE.md constraint 1). */
  beginResizeHold(): void {
    this.resizeHolds++;
  }

  /** End a hold begun by `beginResizeHold`. Once the last outstanding hold
   *  releases, forces an immediate (non-debounced) fit so the drag's
   *  settled size reaches the PTY right away instead of waiting out however
   *  much of the debounce window was left. */
  endResizeHold(): void {
    this.resizeHolds = Math.max(0, this.resizeHolds - 1);
    if (this.resizeHolds === 0) {
      clearTimeout(this.fitTimer);
      this.runFit();
    }
  }

  private doFit(): void {
    if (this.disposed || !this.termEl.isConnected) return;
    if (this.termEl.clientWidth === 0) return; // hidden (inactive tab / maximized-behind) or unlaid — fit.fit() needs a laid-out element
    // Defer the actual geometry change behind xterm's own write queue (#432
    // item 2): `fit.fit()` resizes xterm's buffer SYNCHRONOUSLY, but
    // `term.write()` is parsed ASYNCHRONOUSLY (xterm's internal
    // WriteBuffer) — without this, bytes the PTY already sent under the OLD
    // geometry could still be sitting unparsed and get interpreted under the
    // NEW one once fit.fit() lands. `term.write("", cb)` queues an empty
    // write, so `cb` runs only once every write already queued ahead of it
    // has been parsed — i.e. once it's actually safe to change geometry.
    //
    // #720: "already queued ahead of it" is xterm's queue, and the throttle
    // adds one of loomux's own in front of it. Held bytes were produced under
    // the OLD geometry, so they must be in xterm's queue BEFORE the empty write
    // that gates the resize — otherwise this fix (#432 item 2) would still be
    // in place and still be defeated, one layer up.
    this.lifecycle.flushOutput();
    this.term.write("", () => this.doResize());
    // Geometry-independent UI bits run every tick, unqueued: the pane
    // itself changed size, so keep the overlay within bounds and re-anchor
    // the visible strip on the cursor right away rather than waiting on
    // however much output happens to be queued.
    const overlay = this.embeds.activeOverlay();
    if (overlay) {
      overlay.style.height = `${this.embeds.overlayClamp(overlay.offsetHeight)}px`;
      this.updateTermShift();
    }
    // The steer box wraps to the strip's width, so a width change alters how
    // many lines the placeholder/draft occupies. growCompose only ran on input
    // events, so a widened pane never re-measured and the box stayed tall
    // (#163). Re-measure here; it's a no-op on panes without a compose strip.
    this.compose.growCompose();
  }

  /** The write-queue-serialized half of a fit: actually resizes xterm's
   *  buffer and, if that settled on a genuinely new size, the PTY. Runs
   *  after `doFit`'s `term.write("", …)` callback, so re-check everything
   *  `doFit` already checked — disposal/detach/hide can happen while this
   *  was queued behind pending output. */
  private doResize(): void {
    if (this.disposed || !this.termEl.isConnected) return;
    if (this.termEl.clientWidth === 0) return;
    this.fit.fit();
    const size = `${this.term.cols}x${this.term.rows}`;
    // The zero-width / same-size / no-pty / held / already-in-flight skips
    // live in the pure, tested shouldResizePty (panefit.ts) — THE invariant
    // that keeps tab switches and maximize free of ConPTY repaints (#63,
    // CLAUDE.md constraint 1).
    if (
      !shouldResizePty({
        clientWidth: this.termEl.clientWidth,
        size,
        sentSize: this.sentSize,
        ptyId: this.ptyId,
        held: this.resizeHolds > 0,
        pending: this.resizePending,
      })
    ) {
      return;
    }
    this.resizePending = size;
    resizePty(this.ptyId!, this.term.cols, this.term.rows).then(
      () => {
        if (this.resizePending === size) this.resizePending = null;
        // Only latch on success (#432 item 3): a single failed
        // ResizePseudoConsole used to still mark `size` as sent, leaving
        // xterm's idea of the PTY's geometry wrong until some unrelated
        // later size change happened to paper over it. Leaving `sentSize`
        // stale on failure instead means the very next fit tick sees the
        // same mismatch again and retries.
        this.sentSize = size;
      },
      () => {
        if (this.resizePending === size) this.resizePending = null;
      }
    );
  }

  setName(name: string): void {
    this.name = name;
    this.titleEl.textContent = name;
    // The name is priority chrome that ELLIPSISES rather than folding at every
    // rung but the narrowest, where it moves into the menu instead (#2191, #2335),
    // so the tooltip has to carry the full one — on a narrow pane the rendered
    // text is cut, and it is the only place the name appears until the narrowest
    // rung hands it to `renameEntry`. The rename hint stays,
    // second: the tooltip's job is now "what is this pane called", and the
    // gesture is the footnote.
    this.titleEl.title = name ? `${name} — double-click to rename (F2)` : "Double-click to rename (F2)";
    // The menu's stand-in carries the same name, so a rename performed at the
    // narrowest width shows up where the name currently lives (#2335).
    if (this.renameEntry) this.syncRenameEntry();
    // A docked pane's header is detached, so refresh its dock chip too — else an
    // orchestrator/human rename leaves the chip showing the stale name (#95r).
    this.dockSyncListener?.();
  }

  /** Draw (or clear) the agent-type mark from this pane's launch line (#992).
   *
   *  Reads `spawnCommand`/`spawnArgv` rather than taking an argument, so every path
   *  that changes what this pane is running — first start, respawn, promotion —
   *  reports the same answer by calling this after it has set them. The resolver
   *  returns `null` for a pane with no command, which is how a plain shell ends up
   *  wearing no mark instead of a neutral one.
   *
   *  `innerHTML` is the same injection the header's other glyphs use; what makes it
   *  safe is on the other side (`src/agenticons.ts` §Safety) — the fallback badge
   *  clamps the program name to a single `[A-Z0-9]` character, so no part of a launch
   *  command can be expressed as markup here. The label is set as TEXT (`.title`, and an
   *  `aria-label` ATTRIBUTE) precisely because that clamp does not apply to it — it is the
   *  one place a raw program name survives, so it must never reach markup.
   *
   *  `sshDefaultCli` is passed as the AUTHORITATIVE answer and `isSshPane` as the flag
   *  meaning "the launch line is a transport": an SSH pane's `spawnArgv[0]` is the local
   *  ssh client, not the agent, and reading it captioned panes "Agent CLI: ssh" while they
   *  ran Claude on the far end (#992 review B1).
   *
   *  The wrapper carries `role="img"` + `aria-label` rather than leaving the name in a
   *  `title` alone: this glyph is the only thing in the header that reports which CLI the
   *  pane runs, so unlike the app's decorative icons it needs an accessible name. The
   *  `<svg>` inside stays `aria-hidden` — it is the labelled element's artwork, and
   *  announcing both would read the name twice. */
  /** Everything `agenticons.ts` is allowed to know about this pane, as ONE
   *  reading — the single source for every surface that draws this pane's
   *  agent mark.
   *
   *  It exists because there were two (#2371 review round 2, W1). The header
   *  resolved its mark from the launch line through `programFromRestore`, which
   *  reads any program name; the Agents row resolved from `facts().harness`,
   *  which is `agentCli` — `sessionCliFromCommand`, a CLOSED four-name
   *  membership test that exists for SESSION-STORE ADOPTION and answers `null`
   *  for `codex`, `gemini`, `hermes` and `ante`. So a local `codex` pane wore
   *  "Agent CLI: codex" in its header and NOTHING in its row, and an SSH
   *  profile declaring `defaultCli: "codex"` drew a mark where its local twin
   *  drew none. Widening the row's whitelist would have fixed today's four
   *  names and left the divergence in place for the fifth; one derivation
   *  cannot diverge at all.
   *
   *  `harness` is deliberately NOT this and is not being replaced: it answers
   *  "which session store covers this pane", which is a different question with
   *  a legitimately narrower answer. See `PaneFacts.harness`. */
  get agentMarkInput(): AgentMarkInput {
    return {
      command: this.spawnCommand,
      argv: this.spawnArgv,
      knownCli: this.sshDefaultCli,
      remote: this.isSshPane,
    };
  }

  /** This pane's recorded launch line, both representations (#3318 F1).
   *
   *  The one read the FORK gesture needs and the one thing it could not
   *  otherwise get: a fork is this line rewritten, so the child keeps the
   *  model, the permission posture and every other flag the human launched
   *  with. Deliberately NOT `agentMarkInput` above, which answers a narrower
   *  question (what glyph to draw) and carries two fields — `knownCli`,
   *  `remote` — that would be noise here; and deliberately not `capture()`,
   *  whose `command` is the PERSISTED line, already discharged of a one-shot
   *  fork flag, which is the wrong thing to fork from. */
  get launchLine(): { command: string | null; argv: string[] | null } {
    return { command: this.spawnCommand, argv: this.spawnArgv };
  }

  refreshAgentMark(): void {
    const view = agentMark(this.agentMarkInput);
    this.agentMarkEl.hidden = !view;
    this.agentMarkEl.innerHTML = view?.svg ?? "";
    if (view) {
      this.agentMarkEl.title = view.label;
      this.agentMarkEl.setAttribute("role", "img");
      this.agentMarkEl.setAttribute("aria-label", view.label);
    } else {
      this.agentMarkEl.removeAttribute("title");
      this.agentMarkEl.removeAttribute("role");
      this.agentMarkEl.removeAttribute("aria-label");
    }
  }

  // Delegated to PaneBadges (panebadges.ts), which carries the doc (#3498 F1).
  setBadge(badge: PaneBadge): void { this.badges.setBadge(badge); }
  setAttention(reason: string | null, detail?: string): void { this.badges.setAttention(reason, detail); }
  get watched(): boolean { return this.badges.watched; }
  setWatched(watched: boolean): boolean { return this.badges.setWatched(watched); }
  toggleWatched(): boolean { return this.badges.toggleWatched(); }
  setHeld(reason: string | null, detail?: string): void { this.badges.setHeld(reason, detail); }
  setQueueDepth(reading: QueueDepthReading | null): void { this.badges.setQueueDepth(reading); }
  get queueDepth(): QueueDepthReading | null { return this.badges.queueDepth; }
  setMailUnread(unread: number): void { this.badges.setMailUnread(unread); }
  applyMailSeed(unread: number): void { this.badges.applyMailSeed(unread); }
  get mailUnreadCount(): number { return this.badges.mailUnreadCount; }
  noteCacheAge(reading: CacheAgeReading | null, nowMs: number = Date.now()): void {
    this.badges.noteCacheAge(reading, nowMs);
  }
  setConnected(info: PaneChannelBadge | null): void { this.badges.setConnected(info); }
  get channelId(): string | null { return this.badges.channelId; }
  get channelBadge(): PaneChannelBadge | null { return this.badges.channelBadge; }
  setPendingConnect(pending: boolean): void { this.badges.setPendingConnect(pending); }

  /** Has `dispose()` already run? (#271 review finding 1.) Orchestration.ts's
   *  module-level "armed connect source" reference has no dispose hook of its
   *  own — it's a plain `Pane` object reference, so a closed pane doesn't
   *  un-arm itself — so the connect-menu wiring checks this lazily, on the
   *  next menu-open, rather than needing a new close callback. Deliberately
   *  NOT `!this.el.isConnected`: a MINIMIZED (docked) pane also detaches its
   *  `.el` from the DOM while very much still alive and a valid connect
   *  target, so DOM attachment can't stand in for "this pane still exists". */
  get isDisposed(): boolean {
    return this.disposed;
  }

  // Delegated to PaneBadges (panebadges.ts), which carries the doc (#3498 F1).
  get attention(): { reason: string; label: string; urgent: boolean; detail: string | null } | null { return this.badges.attention; }

  /** Register a callback fired whenever the dock chip's content changes
   *  (attention state or name) — used by the grid to refresh the chip of a
   *  minimized pane, whose header is out of the DOM. */
  setDockSyncListener(fn: (() => void) | null): void {
    this.dockSyncListener = fn;
  }

  // Delegated to PaneBadges (panebadges.ts), which carries the doc (#3498 F1).
  acknowledgeAttention(): void { this.badges.acknowledgeAttention(); }

  /** Handle an OSC 7 working-directory report from the shell. Payloads are
   *  usually a raw path, but tolerate a `file://host/path` URL too. */
  private onCwdReported(payload: string): void {
    // An SSH pane's OSC 7 comes from the REMOTE shell and names a remote path
    // (#887 S3). Nothing local may act on it: not the git watch below, not the
    // `dir_info` lookup, not the folder chip (already hidden in `start`). Return
    // before the git view's prompt signal too — that view is over a LOCAL repo
    // this pane has nothing to do with.
    if (this.isSshPane) return;
    // Every prompt is a "something may have happened" signal for the git
    // view, even when the directory itself didn't change.
    this.views.gitView?.notifyPrompt();
    const path = normalizeOscPath(payload);
    if (!path) return;
    this.cwdRaw = path;
    // Repoint the external-change watch when the directory changes (#36); the
    // backend dedupes same-repo calls so cd-within-a-repo is a no-op there.
    if (path !== this.watchedPath && this.ptyId !== null) {
      this.watchedPath = path;
      setGitWatch(this.ptyId, path);
    }
    // Refresh even when the path is unchanged: the *branch* can change
    // without a cd (git checkout). Throttled on the same policy as the git
    // view's own reaction to this signal, in its own window — see
    // signalDirRefresh.
    this.signalDirRefresh();
  }

  /** The backend saw this pane's repo change on disk (an external checkout /
   *  commit / stage). Drive the same refresh a shell prompt would: the git
   *  view (throttled) and the header branch chip (throttled the same way). */
  onExternalGitChange(): void {
    this.views.gitView?.notifyPrompt();
    this.signalDirRefresh();
  }

  /** React to a "this pane's repository may have moved" signal — a shell prompt
   *  (OSC 7) or the backend's `git-changed` watcher — by re-reading `dir_info`
   *  for the header's cwd/branch chips.
   *
   *  Bounded to one read per `REPO_SIGNAL_WINDOW_MS` per pane (#743 S5). Both
   *  signals are producer-paced, not gestures: `git-changed` polls at 1 Hz and
   *  fires on every index/HEAD/ref move, so a rebase, a `git add -p` session or
   *  another agent's commit loop used to put one sync `dir_info` on the webview
   *  thread per event, on every pane watching that repo. Leading edge, so a
   *  plain `cd` still moves the chip immediately; trailing run reads the
   *  CURRENT cwd, so the last signal of a burst is never the one dropped.
   *
   *  Deliberately not applied to the mount-time and repo-action refreshes:
   *  those are one-shot and gesture-paced, and delaying them would trade a real
   *  latency for no bound at all. */
  private signalDirRefresh(): void {
    const d = decideRefresh({
      nowMs: Date.now(),
      lastRunMs: this.dirRefreshAt,
      timerPending: this.dirRefreshTimer !== undefined,
      windowMs: REPO_SIGNAL_WINDOW_MS,
    });
    if (d.kind === "run") {
      this.runDirRefresh();
    } else if (d.kind === "schedule") {
      this.dirRefreshTimer = window.setTimeout(() => {
        this.dirRefreshTimer = undefined;
        this.runDirRefresh();
      }, d.dueInMs);
    }
  }

  private runDirRefresh(): void {
    // Guard BEFORE stamping: the window records when a read actually happened.
    // Stamping a no-op — a pane that has not reported a cwd yet, or one being
    // disposed — would start a window nothing paid for, and the next real
    // signal inside it would be deferred half a second for no reason.
    if (this.disposed || !this.cwdRaw) return;
    this.dirRefreshAt = Date.now();
    void this.refreshDir(this.cwdRaw);
  }

  // Delegated to PaneViews (paneviews.ts), which carries the doc (#3498 F1).
  toggleGitView(): void { this.views.toggleGitView(); }
  toggleIssuesView(): void { this.views.toggleIssuesView(); }
  toggleTasksView(): void { this.views.toggleTasksView(); }
  toggleDecisionsView(): void { this.views.toggleDecisionsView(); }

  // Delegated to PaneEmbeds (paneembeds.ts), which carries the doc (#3498 F1).
  requestEmbedFocus(kind: EmbedKind, target: string): boolean { return this.embeds.requestEmbedFocus(kind, target); }
  restoreEmbeds(embeds: readonly { view: EmbedKind; side: EmbedSide; share: number }[]): void {
    this.embeds.restoreEmbeds(embeds);
  }

  // Delegated to PaneViews (paneviews.ts), which carries the doc (#3498 F1).
  toggleAuditView(): void { this.views.toggleAuditView(); }
  toggleTimelineView(): void { this.views.toggleTimelineView(); }
  toggleTokensView(): void { this.views.toggleTokensView(); }
  toggleGroupView(): void { this.views.toggleGroupView(); }
  toggleFileEditView(): void { this.views.toggleFileEditView(); }

  /** Open this pane's workspace folder in the configured external editor.
   *  Prompts for the editor command on first use; errors surface as a toast.
   *  Uses the shell-reported cwd, falling back to the startup directory. */
  async openInEditor(): Promise<void> {
    await openInEditor(this.cwdRaw);
    this.focus(); // return focus to the terminal after any dialog
  }

  /** The orchestration group this pane belongs to, if any (for group-wide
   *  actions like end-orchestration closing every pane in the group). */
  get orchGroupId(): string | null {
    return this.orchGroup;
  }

  /** The orchestration agent id this pane hosts, if any. Lets a cancelled
   *  spawn (#106) find and close the pane it opened before the bind timed out. */
  get orchAgentId(): string | null {
    return this.orchAgent;
  }

  /** This pane's orchestration role ("orchestrator" | "worker" | "reviewer"),
   *  or null for a non-orchestration pane. Lets group-wide actions (#46) tell
   *  the orchestrator's own pane apart from its workers/reviewers. */
  get orchRole(): string | null {
    return this.orchRoleName;
  }

  /** Bind (or clear) this pane's standalone channel identity after
   *  construction (#271 W3 addendum, part A3) — used by the Connect
   *  gesture's adopt-on-connect path (`orch_solo_adopt`) for a pane that had
   *  none at spawn time (launched before this feature, or on a CLI the human
   *  didn't opt into channel tools for at launch). Never touches
   *  orchGroup/orchAgent/orchRoleName — this carrier stays deliberately
   *  separate so a plain standalone pane never lights up the orchestration
   *  chrome. */
  setChannelAgent(info: { group: string; agentId: string; role: string; canSend: boolean } | null): void {
    this.channelAgentInfo = info;
  }

  get channelAgentGroupId(): string | null {
    return this.channelAgentInfo?.group ?? null;
  }

  get channelAgentAgentId(): string | null {
    return this.channelAgentInfo?.agentId ?? null;
  }

  get channelAgentRole(): string | null {
    return this.channelAgentInfo?.role ?? null;
  }

  get channelAgentCanSend(): boolean {
    return this.channelAgentInfo?.canSend ?? false;
  }

  /** Whether this pane was launched with a command (an agent CLI), as
   *  opposed to a plain interactive shell — the same distinction
   *  `liveKind()` makes internally. #271 W3 addendum, part A3: the
   *  adopt-on-connect gesture is offered for agent panes with no channel
   *  identity yet; a plain terminal stays not-capable regardless.
   *
   *  NOT the Agents tab's membership rule, which shares this name: see
   *  `agentrows.ts`'s exported `isAgentPane(facts)`. This getter is true for
   *  a hand-typed `make` pane — any launch command counts — and that one is
   *  false for it, because being in the tab is a claim about which CLI is
   *  running. Same name, disjoint consumers, different questions (#2514
   *  review round 1, finding 1). */
  get isAgentPane(): boolean {
    return this.launchedCommand;
  }

  /** The agent CLI program this pane was launched with — the first token of
   *  its launch command, normalized via `normalizeAgentProgram` (#457: path
   *  prefix and `.exe`/`.cmd`/`.bat` stripped, then lower-cased) — the same
   *  helper `panerestore.ts`'s `programFromRestore` and main.ts's D2
   *  dormant-card sniff now call, converging what used to be three
   *  independent (and identically incomplete) first-token derivations
   *  (#440/#452). `Cli | null` — null for a plain shell, an unrecognized
   *  program, or before launch. Used by the session reconciler to match this
   *  pane's cwd against `listSessions()`'s `source` without re-deriving the
   *  parse elsewhere, which is why the set recognized here is exactly the set
   *  of sources a row can carry: a CLI the scanner lists but this getter
   *  answers `null` for is a pane that can never adopt the session sitting in
   *  the sidebar under its own cwd (#722). */
  get agentCli(): Cli | null {
    return sessionCliFromCommand(this.spawnCommand);
  }

  /** True when this pane's current launch command/argv carries `--fork-session`
   *  (#440 B3, review round 2). The reconciler and the D2 card must NEVER
   *  attach a learned session id to a pane whose line will fork away from it
   *  on its very next resume — that id would look authoritative while being
   *  wrong on every future restart (a silent, groundhog-day loss of whatever
   *  the human did after adopting it). Such panes stay unrecorded/dormant-
   *  eligible by exclusion here, same policy `adoptableSessionId` already
   *  applies at spawn time — see docs/design/session-id-learning.md. */
  get hasForkSession(): boolean {
    return hasForkSession(this.spawnCommand, this.spawnArgv);
  }

  /** The parent session this pane was forked FROM by loomux's own fork
   *  gesture (#3318), or null — for a human's own fork line too, which is the
   *  distinction that matters. The reconciler reads it (#3318 F2): a line
   *  loomux BUILT may learn its child's id, because loomux knows which session
   *  on it is the parent and holds that one out of the match; a human's line
   *  stays under B3's exclusion. */
  get forkedFrom(): string | null {
    return this.forkOf;
  }

  /** This pane's recorded agent session id, or null when none has been minted,
   *  named on its command line, or learned yet (#440). Read by the reconciler
   *  to build its plain-data pane projection and to decide whether a pane is
   *  even a candidate for adoption (only null-id agent panes are). */
  get sessionId(): string | null {
    return this.agentSessionId;
  }

  /** When the HUMAN's first input reached this pane's CURRENT process (#440;
   *  review round 2, B2) — not when the process was spawned. The reconciler
   *  uses this (not spawn time) as the earliest a session transcript could be
   *  this pane's, since a transcript is only created once prompted; see
   *  `firstInputMs`'s field comment for why spawn time left too wide a false-
   *  match window. Null before this process has ever received input (a
   *  welcome pane, or an agent pane sitting idle unprompted). */
  get firstInputAt(): number | null {
    return this.firstInputMs;
  }

  /** Record a session id the reconciler LEARNED for this pane post-start
   *  (#440 D1 option B) — the bare-`claude`-line case adoptableSessionId can't
   *  cover. Idempotent/no-op once an id is already recorded: an id already in
   *  hand (minted, adopted from the line, or a prior adoption) is never
   *  overwritten by a later reconciler pass — that would risk cross-wiring a
   *  pane that has since gained its own id onto a different transcript. The
   *  caller (main.ts) is responsible for persisting the layout afterward;
   *  this only updates the live pane's in-memory record. */
  adoptSessionId(id: string): void {
    if (this.agentSessionId !== null) return;
    this.agentSessionId = id;
    // #2116: the single choke point every LATE-learned id passes through, so it
    // is the one place that can tell the host "whatever was written against
    // this pane now has somewhere durable to live". Fired only on an adoption
    // that actually happened — the early return above is what makes this at
    // most once per pane per id, rather than a promise this line has to keep on
    // its own.
    this.events.onSessionIdentified(this);
  }

  /** Show the Notes button only where a note has something to be keyed to.
   *
   *  Three gates, and only one of them is a CSS class. `pty-only` hides the
   *  button on a content pane (files/editor/git/workflow — the stylesheet rule
   *  on `.is-content`). The other two cannot be classes because they are read
   *  off the launch line and can change on a respawn:
   *
   *   - a HARNESS must be running. A plain shell has no agent session, so a
   *     note would have no key at all.
   *   - not an SSH pane. It may well have a CLI at the far end — `facts()`
   *     reports one — but the session lives on the remote machine and this
   *     store is per-LOCAL-machine, so there is nothing here to key to. That
   *     is the same reason `sessionlog.json` is not in a group dir.
   *
   *  Read through `facts()` rather than by re-deriving the harness, so this and
   *  the Agents tab cannot disagree about what a pane is running. */
  syncNotesBtn(): void {
    const hide = !notesApplyToPane(this.facts().harness, this.isSshPane);
    if (this.notesBtn.hidden === hide) return;
    this.notesBtn.hidden = hide;
    // Revealing a control changes the set the overflow policy has to fit
    // (#2191). The `misplaced` check in `syncHeaderOverflow` is written for
    // exactly this case — a control un-hidden while the header is ALREADY
    // folded — but it only runs when a pass runs, and this reveal happens on
    // `start()`, which need not coincide with a resize. Scheduling one is
    // coalesced and touches no PTY.
    this.scheduleHeaderSync();
  }

  /** Tell this pane how many notes its session carries, so the button can say
   *  so. Pushed by the host on every store change rather than pulled, because
   *  the store is the host's (see `PaneEvents.onOpenNotes`).
   *
   *  The count is in the TITLE and not a badge on purpose: a note count is not
   *  an attention signal, and the pane header already spends its visual budget
   *  on things that are. */
  setNotesCount(n: number): void {
    this.notesCount = Math.max(0, n);
    this.notesBtn.title =
      this.notesCount === 0
        ? "Notes for this session"
        : this.notesCount === 1
          ? "Notes for this session (1)"
          : `Notes for this session (${this.notesCount})`;
    this.notesBtn.classList.toggle("has-notes", this.notesCount > 0);
  }

  /** Whether this pane's process has emitted a single byte since it spawned
   *  (#280/#281) — a crashed pane that never did is a DOA revival, not a
   *  crash worth keeping open to read. */
  get hasReceivedOutput(): boolean {
    return this.receivedOutput;
  }

  // Delegated to capturePane (panecapture.ts), which carries the doc (#3498 F1).
  capture(): PersistedPane | null { return capturePane(this); }

  /** The persisted record a dormant restore placeholder stands in for, or null
   *  when this pane isn't dormant. Lets a whole-group resume read the CAPTURED
   *  group members (session id + role) straight off the tab's dormant orch
   *  placeholders — the set that was live at close — rather than the backend's
   *  full historical roster (#194.5). */
  get restoreRecord(): PersistedPane | null {
    return this.dormantRecord;
  }

  /** This pane's persisted kind from its live launch state — a CONTENT kind
   *  (#214/#217, no PTY at all) > ssh (#887) > lead (#2519) > orch > agent >
   *  plain terminal. `capture()`'s per-kind ternaries above then null every
   *  field that kind doesn't have, leaving exactly what it needs to come back.
   *
   *  THE LADDER ITSELF LIVES IN `persistedKindFor` (`tabstore.ts`), which is
   *  pure and carries the argument for each rung's position — including the one
   *  that decides a lead's whole restore contract. It was extracted there
   *  (#2519 C2) rather than left here because this method reads six private
   *  fields off a live pane, so nothing could pin it without a DOM, and "an ssh
   *  pane must not fall through to terminal" is exactly the kind of claim that
   *  should not rest on someone re-reading the order. This method is now the
   *  READING of those fields, which is all a `Pane` is the authority on. */
  liveKind(): PersistedPaneKind {
    return persistedKindFor({
      // A structured pane goes in as the flag, not as a content kind: it has no PTY but
      // it is an agent, and a root alone cannot restore one. See the ladder's own note.
      contentKind: this.contentKind === "structured" ? null : this.contentKind,
      structured: this.contentKind === "structured",
      ssh: this.isSshPane,
      orchRole: this.orchRoleName,
      orchGroup: this.orchGroup,
      launchedCommand: this.launchedCommand,
    });
  }


  /** Is this pane a LEAD (#2519) — a human's agent pane that owns a lightweight
   *  orchestration group and spawns orrerix panes as its helpers?
   *
   *  Read off the orchestration ROLE, which is the backend's own word for the
   *  capability class (`Role::Lead`), and never off the toggle that launched it
   *  or the CLI it runs: the role is what the registry, the audit log and the
   *  Agents tab all key on, so anything else here would be a second answer to
   *  one question. */
  get isLead(): boolean {
    return this.orchRoleName === "lead";
  }

  /** Hand this pane the roster's own idleness reading for its agent (#2122
   *  slice A2) — "the reaper would call this idle / it holds no assignment",
   *  from `idle_since_ms` on the tab-strip poll's summary. `null` for a pane
   *  the roster does not cover, and for an orchestration pane before the first
   *  poll lands. Explicitly NOT "parked at a prompt" (#2089): it feeds the
   *  `idle` rung of `deriveAgentState` and nothing else. */
  noteRosterIdle(idle: boolean | null): void {
    this.activity.noteRosterIdle(idle);
  }

  /** This pane's whole state as plain data (#2122 slice A1) — the one thing the
   *  Agents tab (#2122) and the pane Notes rows (#2116) read, so neither view
   *  couples to `Pane`'s shape and both can be tested against literals
   *  (`agentrows.ts` owns the type and what the reading MEANS).
   *
   *  Same contract as `tabPaneInfo()` below and for the same reason: it reads
   *  no geometry, starts no IPC and touches no timer, so it is safe to call on
   *  a hidden tab, on every row of a list, once a second.
   *
   *  @param tab which tab this reading is being taken from (#2371), or `null`
   *  when the caller is not grouping and has nothing to name. Supplied rather
   *  than derived because a `Pane` holds no back-reference to its `Workspace`
   *  — see `TabRef` in `agentrows.ts`. (Folded into this block rather than
   *  given its own: two consecutive doc comments leave TypeScript attaching
   *  only the second, so the paragraphs above reached no hover and no generated
   *  doc — #1229's doc-splice signature in its TypeScript form, found in
   *  #2371 review round 2, W4.) */
  facts(tab: TabRef | null = null): PaneFacts {
    // ONE reading of `tabPaneInfo()`, for `kind` and for `alive` both. `alive`
    // is deliberately NOT `ptyId !== null && !exited`: a CONTENT pane (files,
    // editor, git, workflow) has no PTY by design and is fully functional the
    // moment it exists, so that expression calls every one of them dead —
    // `tabPaneInfo()` already owns the one answer to "is this pane live", and
    // asking it twice by two rules is how the two drift apart.
    const info = this.tabPaneInfo();
    return {
      key: this.key,
      name: this.name,
      kind: info.kind,
      tab,
      // Read off the launch line, never branched on a CLI name to produce a
      // name (#722/#841): `agentCli` is the shared first-token parse, and the
      // SSH profile's declared far-end CLI is the only answer available for a
      // pane whose local argv is an ssh client. A CLI neither knows shows up
      // as null rather than inheriting some other CLI's identity.
      //
      // This is the SESSION-STORE question and answers only the four CLIs
      // loomux can scan sessions for. It is not what decides the icon — `mark`
      // below is, and reading this one instead is #2371 review round 2's W1.
      harness: this.agentCli ?? this.sshDefaultCli ?? null,
      // ONE reading, shared with the pane header (`refreshAgentMark`), so the
      // two surfaces cannot disagree about which program a pane is running.
      mark: this.agentMarkInput,
      orch: this.orchGroup
        ? { group: this.orchGroup, agentId: this.orchAgent, role: this.orchRoleName }
        : null,
      sessionId: this.agentSessionId,
      alive: info.live,
      dormant: this.isDormant,
      welcome: this.isWelcome,
      attention: this.badges.attentionReason
        ? { reason: this.badges.attentionReason, detail: this.badges.attentionDetail }
        : null,
      held: this.badges.heldReason,
      // The human's mark (#3319), beside the agent's reading and never folded
      // into it: a listing surface shows both, and a consumer that wanted
      // "does this pane want me" must not be able to get a human's bookmark by
      // accident.
      watched: this.watched,
      activity: this.activity.snapshot(Date.now()),
      // The backend's reading, as last delivered (#3407); the Agents tab derives
      // its own label from it against its own clock.
      cache: this.badges.cacheReading,
    };
  }

  /** Classify this pane for the per-tab agent counter / orch markers (#194 P4,
   *  tabcounts.ts). A welcome (setup) or dormant placeholder reports `live:false`
   *  so it never inflates the count; a running pane reports its kind + that it has
   *  a PTY. A content pane reports its own kind (files / editor / git), which the
   *  counter ignores outright — those are viewers, not agents (#214, #217). Reads no
   *  geometry, so it's safe on a hidden tab. */
  tabPaneInfo(): TabPaneInfo {
    if (this.isWelcome) return { kind: "terminal", live: false };
    if (this.dormantRecord) {
      // A dormant placeholder reports the kind it stands in for, not "some
      // pane". `orch` and `ssh` say so; everything else collapses to "agent",
      // which is the historical bucket for a Start card.
      //
      // #887 S4 / PR #926 review NB1: an SSH placeholder used to land in that
      // bucket too. Nothing counts it today (`live: false`, and `dormantOrch`
      // keys off `"orch"`), but tabcounts.ts now DOCUMENTS that an ssh pane is
      // neither an agent nor an orchestration member — a property the dormant
      // path didn't have, and the first consumer to count dormant panes would
      // have counted Reconnect cards as agents.
      const kind = this.dormantRecord.paneKind;
      return { kind: kind === "orch" || kind === "ssh" ? kind : "agent", live: false };
    }
    // A content pane has no PTY by design, so `live` can't be derived from one; it is
    // fully functional the moment it exists. Routed through `liveKind` rather than
    // reporting `contentKind` raw, so a structured pane counts as the AGENT it is
    // (#2891) — the four PTY-less surfaces still report themselves, unchanged, because
    // that is the ladder's first rung.
    if (this.contentKind !== null) return { kind: this.liveKind(), live: true };
    const kind = this.liveKind();
    return { kind, live: this.ptyId !== null && !this.exited, connectedChannel: this.channelId };
  }

  /** Debounced cursor-follow for the overlay: TUIs sweep the cursor around
   *  while repainting, so settle before measuring. */
  private scheduleShift(): void {
    if (!this.embeds.activeOverlay()) return;
    clearTimeout(this.shiftTimer);
    this.shiftTimer = window.setTimeout(() => this.updateTermShift(), 80);
  }

  /** With the git overlay covering the top of the terminal, shift the
   *  terminal down (visually only — the grid/PTY size is untouched) just
   *  enough to keep the cursor's row inside the visible bottom strip.
   *  Full-screen TUIs write at the bottom and need no shift; a fresh shell
   *  writes at the top, which the overlay would otherwise hide. */
  updateTermShift(): void {
    if (this.disposed) return;
    const overlay = this.embeds.activeOverlay();
    if (!overlay) {
      this.termEl.style.transform = "";
      return;
    }
    const screen = this.termEl.querySelector<HTMLElement>(".xterm-screen");
    const xtermEl = this.termEl.querySelector<HTMLElement>(".xterm");
    if (!screen || !xtermEl || !this.term.rows) return;
    const cell = screen.offsetHeight / this.term.rows;
    if (!cell) return;
    const covered = overlay.offsetHeight;
    const padTop = parseFloat(getComputedStyle(xtermEl).paddingTop) || 0;
    const cursorTop = padTop + this.term.buffer.active.cursorY * cell;
    // One extra row of context above the cursor when shifted.
    const shift = Math.max(0, Math.min(covered, Math.round(covered - cursorTop + cell)));
    this.termEl.style.transform = shift > 0 ? `translateY(${shift}px)` : "";
  }

  async refreshDir(path: string): Promise<void> {
    let info;
    try {
      info = await dirInfo(path);
    } catch {
      return;
    }
    if (this.disposed || this.cwdRaw !== path) return; // superseded
    this.setMeta(this.cwdEl, this.cwdTextEl, shortCwd(info.cwd), info.cwd);
    this.setMeta(this.branchEl, this.branchTextEl, info.branch, info.branch);
  }

  /** Open a native folder picker and cd the shell into the chosen directory. */
  private async pickFolder(): Promise<void> {
    if (this.ptyId === null) return;
    // The chip this hangs off is hidden for an SSH pane (`start`), so this is
    // the belt on that suspender — a folder picker chooses a path on THIS
    // machine, and `change_dir` would type it into a shell on another one.
    if (this.isSshPane) return;
    const picked = await pickDirectory({
      title: "Change folder",
      defaultPath: this.cwdRaw ?? undefined,
    });
    if (typeof picked === "string" && this.ptyId !== null) {
      // #1042: a human chose this folder in a native dialog the backend never
      // sees, and `change_dir` takes it as a root argument (slice C scopes it).
      await admitRoot(picked);
      await changeDir(this.ptyId, picked);
      this.focus(); // return focus to the terminal after the dialog
    }
  }

  private setMeta(
    wrap: HTMLElement,
    text: HTMLElement,
    label: string | null | undefined,
    tip: string | null
  ): void {
    if (label) {
      text.textContent = label;
      wrap.title = tip ?? label;
      wrap.hidden = false;
    } else {
      wrap.hidden = true;
    }
  }

  // Delegated to PaneBadges (panebadges.ts), which carries the doc (#3498 F1).
  setForkCrumb(crumb: { label: string; title: string; go: (() => void) | null } | null): void {
    this.badges.setForkCrumb(crumb);
  }

  /** The menu's name row, re-labelled from the current name. Called when the
   *  entry is revealed and on every rename, so the menu never shows a stale name
   *  (the entry is hidden at every other rung, so there is nothing to update). */
  private syncRenameEntry(): void {
    const name = this.name || "this pane";
    this.renameEntry.textContent = `✎  ${name}`;
    this.renameEntry.title = `${name} — rename (F2)`;
  }

  startRename(): void {
    const input = document.createElement("input");
    input.className = "pane-title-input";
    input.value = this.name;
    this.titleEl.replaceWith(input);
    input.focus();
    input.select();
    // Enter/Escape commit AND blur commits; the first commit swaps the input
    // back, and detaching the focused input itself fires blur → a second commit.
    // makeRenameCommit is idempotent so that redundant call is a no-op, Escape
    // (save=false) beats the trailing blur-save, and — for #113 — a blur caused
    // by an orchestrator-driven grid/dock move (input no longer connected) is
    // treated as a cancel rather than saving a half-typed name (see isConnected).
    const commit = makeRenameCommit({
      value: () => input.value,
      isConnected: () => input.isConnected,
      save: (name) => {
        const changed = name !== this.name;
        this.name = name;
        // Sync a human rename to the backend so the roster name matches the
        // pane title and the human's choice takes precedence over any later
        // orchestrator rename_agent (#95r). Best-effort: the title is already
        // updated locally, so a backend hiccup is non-fatal. Skip the round-trip
        // when nothing changed so a no-op Enter/blur doesn't re-broadcast a rename.
        if (this.orchAgent && changed) {
          invoke("orch_agent_renamed", { agentId: this.orchAgent, name }).catch(() => {});
        }
        // The pane's persisted name just changed with no grid mutation — same
        // stale-snapshot class as a files-pane re-root, so re-persist here too.
        // (persistTabs dedups on identical bytes, so a no-op rename costs nothing.)
        if (changed) this.events.onRecordChanged(this);
      },
      restore: () => {
        // Put the label back showing the current name (the pre-edit name on a
        // cancel, the saved name on a commit), then swap the input out. swapEditor
        // tolerates the input having been detached OR moved mid-edit by a grid/dock
        // restructure: it leaves the header consistent (label back, no orphaned
        // input) and only reports `live` — safe to refocus — when the input was
        // still on the document, i.e. the ordinary Enter/click-away path.
        this.titleEl.textContent = this.name;
        // The commit path assigns `this.name` directly rather than going through
        // `setName`, so the menu's name row is re-labelled here as well — it is
        // the only place the name is visible at the narrowest width (#2335).
        this.syncRenameEntry();
        if (swapEditor(input, this.titleEl).live) this.focus();
      },
    });
    input.addEventListener("keydown", (e) => {
      e.stopPropagation();
      if (e.key === "Enter") commit(true);
      if (e.key === "Escape") commit(false);
    });
    input.addEventListener("blur", () => commit(true));
  }

  /** Should this pane survive its process's death — and why? Null to dispose it.
   *
   *  Two reasons, composed in the pure `keepOpenOnExit` (dirtystate.ts): a command pane
   *  that died unexpectedly stays so the human can read the error (the original rule),
   *  and — new in #219 — a pane holding a DIRTY Alt+F buffer stays no matter how it
   *  died, because an automatic teardown must never destroy work the human never agreed
   *  to lose. Clean exits and loomux-initiated kills close as usual, unless that. */
  keepOpenOnExit(exit: ExitInfo): KeepOpenReason | null {
    return keepOpenOnExit({
      launchedCommand: this.launchedCommand,
      // #887 S4: an ssh pane is argv-spawned, so `launchedCommand` is false for
      // it — without this the pane would vanish the instant the link dropped,
      // taking the scrollback that says why with it. See the field's own comment
      // in dirtystate.ts.
      isSshPane: this.isSshPane,
      exit,
      hasUnsavedWork: this.hasUnsavedWork(),
    });
  }

  /** Announce a kept-open pane's exit inside its terminal, saying WHY it is still here.
   *  A pane that outlives its process for an unsaved buffer must say so — otherwise it
   *  reads as a bug ("why didn't this close?") and the buffer it is protecting stays
   *  invisible, which is how it gets lost anyway. */
  notifyExited(code: number | null, reason: KeepOpenReason = "output"): void {
    // The pane stays open to show output, but its process is DEAD — mark it so the
    // agent counter stops counting it as live (#194 P4 LOW-7). ptyId is left set
    // (the buffer/scrollback is still attached) so isExited, not ptyId, gates live.
    this.exited = true;
    // #720: the process's own last bytes were produced BEFORE this banner, so
    // they must be written before it. Without this the banner could print above
    // the final prompt/goodbye it is announcing the end of.
    this.lifecycle.flushOutput();
    const codeTxt = code === null ? "" : ` (code ${code})`;
    const why =
      reason === "unsaved"
        ? "— kept open: the file editor (Alt+F) here has UNSAVED edits. Save them, then close with Ctrl+Shift+W"
        : "— pane kept open so you can read the output; close it with Ctrl+Shift+W";
    this.term.writeln(`
[91mprocess exited${codeTxt}[0m [90m${why}[0m`);
    // A crashed pane that ALSO holds unsaved edits gets both facts: the dead process is
    // the louder one, but the buffer is the one that can still be lost.
    if (reason === "output" && this.hasUnsavedWork()) {
      this.term.writeln(
        `[93mThe file editor (Alt+F) in this pane has unsaved edits — closing the pane will ask.[0m`
      );
    }
    this.setName(`${this.name} · exited`);
    // A crash that never produced a single byte (#281) reads as a bare exit
    // code with nothing to go on -- say so explicitly rather than leaving
    // the human to guess whether this ever even started.
    if (reason === "output") {
      const diag = exitDiagnosticLine(this.receivedOutput);
      if (diag) this.term.writeln(`\r\n[90m${diag}[0m`);
    }
  }

  /** What this pane is holding, for the app-quit guard's enumeration (#219): its editor's
   *  buffer — the pane's own (an editor pane), its Alt+F overlay's, or that same Alt+F
   *  editor DOCKED (#361 scope increase) — labelled with the tab and pane it lives in, so
   *  the confirm can say WHERE. Null when the pane has no editor with a file open at all. */
  bufferReport(tab: string): PaneBufferReport | null {
    const report = this.unsavedHolder()?.bufferReport();
    if (!report) return null;
    // "pane" vs "overlay" vs "docked" is what the quit confirm needs to say WHERE the
    // work is: a content pane is visibly what it is; an Alt+F overlay is tucked inside a
    // terminal that looks like any other; a DOCKED Alt+F editor looks like a permanent
    // fixture of the pane rather than something someone opened — `sideOf` only applies
    // when the holder actually IS `fileEditView` (the `editorPaneView`/`workflowPaneView`
    // branch above already took the "pane" case, so reaching here means it is).
    const host: DirtyHost =
      this.editorPaneView || this.workflowPaneView
        ? "pane"
        : this.embeds.sideOf("editor") !== null
          ? "docked"
          : "overlay";
    return { tab, pane: this.name, host, file: report.file, dirty: report.dirty };
  }

  setActive(active: boolean): void {
    this.el.classList.toggle("active", active);
    // #720: the active pane is the one the human is reading, so it keeps its
    // per-frame write cadence; every other VISIBLE pane batches. Becoming
    // active must write out whatever is held immediately — otherwise clicking a
    // pane would show it up to a window stale, which is the one place a render
    // throttle is actually perceptible.
    this.isActivePane = active;
    if (active) this.lifecycle.wakeOutput();
  }

  /** Reflect fullscreen state: the `.maximized` class drives the CSS overlay
   *  (no PTY resize is forced — the pane genuinely changes size, so its own
   *  ResizeObserver issues at most one debounced fit) and the button glyph
   *  flips between maximize and restore. */
  setMaximized(on: boolean): void {
    this.el.classList.toggle("maximized", on);
    this.maximizeBtn.textContent = on ? "⤡" : "⤢";
    this.maximizeBtn.title = on ? "Restore (Ctrl+Shift+M)" : "Maximize (Ctrl+Shift+M)";
  }

  /** Group accent color, if this pane carries an orchestration badge — used to
   *  tint its chip in the minimize dock. */
  get accentColor(): string | null {
    return this.el.style.getPropertyValue("--group-color").trim() || null;
  }

  focus(): void {
    // A setup-state pane has no terminal; route focus into its welcome form so
    // keyboard nav (Alt+arrow), window-refocus (main.ts), and dock-restore land
    // on a usable control instead of no-op'ing on an unopened terminal (rev-74
    // LOW-6). `isWelcome` is null once the pane becomes a real terminal.
    if (this.isWelcome) {
      this.focusWelcome();
      return;
    }
    // A dormant placeholder likewise has no terminal — land focus on its
    // Start/Resume affordance so keyboard nav reaches a usable control.
    if (this.lifecycle.dormantEl) {
      this.lifecycle.dormantEl.querySelector<HTMLElement>("button, [tabindex]")?.focus();
      return;
    }
    // A content pane has no terminal either: focus its view (each is tabIndex -1), so
    // Alt+arrow nav, window refocus, and dock-restore land ON the surface instead of
    // no-oping on a terminal that was never opened. None of them grabs an inner control
    // — the user clicks into whichever they want.
    if (this.filesView) {
      this.filesView.focus();
      return;
    }
    if (this.editorPaneView) {
      this.editorPaneView.el.focus();
      return;
    }
    if (this.workflowPaneView) {
      this.workflowPaneView.focus();
      return;
    }
    if (this.gitPaneView) {
      this.gitPaneView.el.focus();
      // NOT a refresh. Refreshing on focus is the obvious idea and it is wrong: the
      // git view rebuilds its changes strip from scratch (`renderWorking` →
      // `replaceChildren`), which includes the COMMIT MESSAGE textarea — so a refresh
      // fired by "the window regained focus" or "you tabbed back to this pane" would
      // silently wipe a half-typed commit message. That never bit the overlay, whose
      // only refresh trigger is a shell prompt (impossible while you type into the
      // view). A git pane has no prompt and no PTY to hang a git watch off, so it
      // refreshes on open, after its own actions, and on the ↻ button — an explicit,
      // safe pull rather than an implicit, destructive one.
      return;
    }
    this.term.focus();
  }

  // Delegated to PaneCompose (panecompose.ts), which carries the doc (#3498 F1).
  focusCompose(): void { this.compose.focusCompose(); }
  isComposeFocused(): boolean { return this.compose.isComposeFocused(); }

  /** Does this pane have an OPEN terminal? `term.element` is set by `term.open()`,
   *  which only the PTY-backed start paths call — so this is false for a files pane
   *  (#214, never), and for a welcome or dormant pane (not yet). The single honest
   *  answer to "can this pane take a paste / be serialized / hold a WebGL context",
   *  used by all three; without it, dictating (Alt+S) at a files pane would record,
   *  transcribe, and paste the transcript into an xterm that isn't there. */
  hasTerminal(): boolean {
    return !!this.term.element;
  }

  // Delegated to PaneCompose (panecompose.ts), which carries the doc (#3498 F1).
  setVoicePhase(kind: "compose" | "terminal", phase: VoicePhase): void { this.compose.setVoicePhase(kind, phase); }
  pasteToTerminal(text: string): void { this.compose.pasteToTerminal(text); }
  showVoiceStatus(msg: string): void { this.compose.showVoiceStatus(msg); }
  insertTranscript(text: string): void { this.compose.insertTranscript(text); }

  /** Tear down DOM + terminal. Kills the PTY unless it already exited. */
  dispose(killBackend = true): void {
    if (this.disposed) return;
    this.disposed = true;
    this.resizeObs.disconnect();
    this.headerObs.disconnect(); // #2191 header-overflow observer
    this.closeOverflowMenu(); // drops its document-level click-away listener
    clearTimeout(this.headerSyncTimer);
    clearTimeout(this.overflowCloseTimer);
    clearTimeout(this.fitTimer);
    clearTimeout(this.shiftTimer);
    clearTimeout(this.compose.composeStatusTimer);
    clearTimeout(this.lifecycle.flushTimer); // #720 output throttle
    clearTimeout(this.dirRefreshTimer); // #743 repo-signal throttle
    clearTimeout(this.lifecycle.webglRetryTimer); // #720 WebGL re-acquire
    // Abort any in-flight voice capture aimed at this pane (releases the mic).
    voiceController.notifyPaneDisposed(this);
    // Same shape, different module-level holder: an armed cross-workspace
    // connect keeps a reference to its source pane (#1301).
    notifyPaneDisposed(this);
    this.compose.voiceIndicator?.remove();
    this.compose.clearAttachments(); // revoke any lingering thumbnail object URLs
    this.views.gitView?.dispose();
    this.views.issuesView?.dispose();
    this.views.tasksView?.dispose();
    this.views.decisionsView?.dispose();
    // Drop any focus request parked for a view that will never render again,
    // so it cannot be picked up by a later instance of that view (#1091 slice
    // C — `PendingEmbedFocus.clear`'s own stated purpose).
    for (const kind of EMBED_KINDS) this.embeds.pendingFocus.clear(kind);
    this.views.auditView?.dispose();
    this.views.timelineView?.dispose();
    this.views.groupView?.dispose();
    this.views.fileEditView?.dispose();
    // The surfaces a CONTENT pane hosts (#214, #217). Exactly one is ever non-null.
    this.filesView?.dispose();
    this.editorPaneView?.dispose();
    this.gitPaneView?.dispose();
    this.workflowPaneView?.dispose();
    this.structuredPaneView?.dispose();
    this.todoPaneView?.dispose();
    // Drop anything the #720 throttle was holding. Cheap on its own, and it
    // matters exactly when something else has gone wrong: a pane that is still
    // reachable from somewhere should at least not be dragging its output
    // backlog along with it (#1301).
    this.lifecycle.discardOutput();
    // By OWNER, not by `this.ptyId` (#1301). A pane that respawned in place has
    // already forgotten the ids it used to hold, so an id-aimed detach here can
    // only ever release the last one — which is how a module-level map came to
    // hold panes the grid had closed. The owner-keyed release needs no such
    // memory, and `respawnFresh` keeps the set to one by releasing as it goes.
    detachOutputOwner(this);
    detachGitWatchOwner(this);
    // The backend poll is stopped by ID as well, and that is not redundant: an
    // ssh pane never attaches a git-watch HANDLER (#887 S3) but can still have
    // pointed the backend at a repo from an OSC-7 cwd report, so an owner-only
    // teardown would narrow what `dispose` used to unwatch.
    if (this.ptyId !== null) {
      stopGitWatch(this.ptyId);
      if (killBackend) killPty(this.ptyId).catch(() => {});
    }
    this.term.dispose();
    this.el.remove();
  }
}
