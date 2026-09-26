// A pane's process lifecycle, split out of pane.ts (#3498 F1): starting and
// attaching the PTY, the output pipeline into xterm (#720's render throttle),
// respawn (#407), the dormant restore placeholder and its Reconnect card
// (#194, #887), and the WebGL renderer with its loss/retry policy.
//
// A satellite of `Pane`: it owns this cluster's state and reads the rest of the
// pane through `pane`. DOM glue, hand-validated like pane.ts itself; the pure
// policies it calls live in panethrottle.ts, webglretry.ts and panerestore.ts.
// Nothing here resizes the PTY — fit and resize stay on `Pane` (constraint 1).
// Design notes: docs/design/pane-render-throttle.md, docs/design/session-restore.md;
// layout conventions: docs/design/module-layout.md.

import { WebglAddon } from "@xterm/addon-webgl";
import { SerializeAddon } from "@xterm/addon-serialize";
import {
  spawnPty,
  writePty,
  killPty,
  ensureOutputRouter,
  attachOutput,
  detachOutput,
  detachOutputOwner,
  detachGitWatchOwner,
  stopGitWatch,
  attachGitWatch,
  setGitWatch,
  ptyBackendInfo,
} from "./pty";
import { sshOrchestrationRefusal } from "./panesetup";
import { getSettings } from "./settings";
import { createOrderedWriter } from "./ptywrite";
import { hintXtermSyncParse } from "./xtermreach";
import { decideFlush, WOKEN, MAX_PENDING_BYTES } from "./panethrottle";
import { sharedVisibility } from "./pollgate";
import { planWebglRetry } from "./webglretry";
import { seedMailUnread } from "./orchestration";
import { type PersistedPane, type PersistedPaneKind } from "./tabstore";
import { adoptableSessionId } from "./panerestore";
import type { PaneOptions, Pane } from "./pane";

export class PaneLifecycle {
  /** Dormant restore placeholder (#194 P4): a pane rebuilt from a persisted leaf
   *  that we deliberately did NOT auto-spawn — an agent CLI with no resumable id
   *  (a Start button), or an orchestration pane whose whole group stays dormant
   *  until the human resumes it (a Resume button). No PTY until acted on, so the
   *  no-resize invariant holds. `dormantRecord` is the leaf it was built from, so
   *  capture() re-serializes it verbatim: a session closed without resuming
   *  still offers the same restore next boot. Null for any live/welcome pane. */
  dormantEl: HTMLElement | null = null;
  /** The Reconnect card floating over a DISCONNECTED ssh pane's terminal (#887
   *  S4), or null. Distinct from `dormantEl` above in the one way that matters:
   *  the terminal underneath stays mounted and readable, because the bytes it is
   *  holding — "Connection to host closed", a timeout, a rejected key — are the
   *  explanation for the card being there at all. It is chrome floating over the
   *  pane, never a layout change: nothing here resizes the terminal or the ConPTY
   *  (CLAUDE.md constraint 1). */
  private reconnectEl: HTMLElement | null = null;
  /** #720: PTY output that has ARRIVED but has not been handed to `term.write`
   *  yet, in arrival order, plus its byte total. Non-empty only for a
   *  visible-but-unfocused pane mid-stream — see `acceptOutput`. Nothing is
   *  ever dropped or reordered here; only the moment of the `write` call
   *  moves. */
  private pendingOut: Uint8Array[] = [];
  private pendingOutBytes = 0;
  flushTimer: number | undefined;
  /** When this pane last wrote to xterm, or `WOKEN` while it is quiet — the
   *  leading edge of the throttle (panethrottle.ts). */
  private lastFlushMs: number | typeof WOKEN = WOKEN;

  /** The live WebGL renderer addon, if loaded. Held so hidden tabs can drop it
   *  (browsers cap live GL contexts, and N mounted-but-hidden tabs would each
   *  hold one) and reload it on show — the onContextLoss→DOM fallback path. */
  private webgl: WebglAddon | null = null;
  private serializer: SerializeAddon | null = null;
  /** True while this pane's project tab is hidden (#63). Held so `tryWebgl`
   *  refuses to create a context for a hidden pane — start() calls tryWebgl
   *  unconditionally, so a pane opened INTO a hidden tab (a background
   *  orchestrator spawn) would otherwise take a GL context it isn't showing. */
  private hiddenTab = false;
  /** #720: losses handled in the current streak, and when the live context was
   *  acquired — the two inputs `planWebglRetry` needs to tell a transient loss
   *  from a standing one. Both reset when a hide/show cycle re-acquires from
   *  scratch (`setHidden`). */
  private webglLosses = 0;
  private webglAcquiredMs = 0;
  webglRetryTimer: number | undefined;

  constructor(private readonly pane: Pane) {}

  /** Open the terminal in the DOM and spawn its PTY. Call after `el` is attached. */
  async start(opts: PaneOptions = {}, takeFocus = true): Promise<void> {
    // #887/#888 boundary, checked BEFORE anything is opened or spawned so a
    // refusal costs no process: an SSH pane can never carry an orchestration
    // identity. Fails closed and loudly (the caller's catch surfaces it) rather
    // than quietly dropping the identity — a group member whose gh merge gate
    // silently isn't enforced is precisely the outcome this refuses. See
    // `sshOrchestrationRefusal` for why each of those mechanisms cannot follow a
    // pane onto a remote host.
    this.refuseSshOrchestration(opts);
    this.pane.sshProfileId = opts.ssh?.profileId ?? null;
    this.pane.sshDefaultCli = opts.ssh?.defaultCli ?? null;
    this.pane.setName(opts.name ?? "shell");
    this.pane.launchedCommand = !!opts.command?.trim();
    // Retain the launch inputs for a later capture() into the persisted layout
    // (#194). Purely a record — the spawn below still reads them straight off opts.
    this.pane.spawnCommand = opts.command ?? null;
    this.pane.syncNotesBtn();
    this.pane.spawnArgv = opts.argv ?? null;
    this.pane.spawnShellKind = opts.shellKind ?? null;
    this.pane.refreshAgentMark();
    // #440: a caller (the launcher, an orch spawn) that already knows the id
    // wins outright. Otherwise, learn it from the command/argv line itself —
    // a human-typed `claude --resume <id>` / `--session-id <id>` custom
    // command already names its own session (D1); adoptableSessionId is the
    // guarded extractor (refuses on --fork-session). A bare `claude` line, or
    // one with no session flag, yields null here — the reconciler (main.ts)
    // is the OTHER half of D1, catching that case post-start.
    this.pane.agentSessionId = opts.sessionId ?? adoptableSessionId(opts.command ?? null, opts.argv ?? null);
    this.pane.forkOf = opts.forkOf ?? null;
    this.pane.firstInputMs = null; // this process hasn't been typed into yet (#440 B2)
    if (opts.badge) this.pane.setBadge(opts.badge);
    if (opts.orchAgent) this.pane.orchAgent = opts.orchAgent;
    if (opts.channelAgent) this.pane.channelAgentInfo = opts.channelAgent;
    // Build the steering strip BEFORE term.open/fit below, so the terminal sizes
    // to the reduced height once instead of resizing into scrollback later.
    this.applyOrchIdentity(opts);
    // Seed the toolbar from the startup directory. Interactive shells refine
    // this via OSC 7; command panes (agents) keep this initial value since
    // they have no prompt to report from.
    //
    // An SSH pane is neither: the directory its remote shell reports is a path
    // on ANOTHER machine, so the folder chip (which resolves and `cd`s LOCAL
    // paths) is hidden outright for it rather than left showing a local reading
    // of a remote path (#887 S3, plan part 4a). `isSshPane` also stops
    // `onCwdReported` from re-pointing the git watch at whatever the remote
    // reports.
    if (this.pane.isSshPane) this.pane.cwdEl.hidden = true;
    if (opts.cwd && !this.pane.isSshPane) {
      this.pane.cwdRaw = opts.cwd;
      void this.pane.refreshDir(opts.cwd);
    }
    // Tell xterm which ConPTY it is talking to. This drives its resize
    // heuristics: against a modern conhost (sideloaded, honors the
    // resize-quirk flag and emits nothing on resize) xterm keeps its own
    // buffer reflow; against the inbox Win10 conhost (full repaint on every
    // resize) xterm disables reflow so the two don't fight and duplicate
    // content into scrollback.
    try {
      const backend = await ptyBackendInfo();
      if (backend.conpty_build > 0) {
        this.pane.term.options.windowsPty = {
          backend: "conpty",
          buildNumber: backend.conpty_build,
        };
        // #430: the resize-quirk conpty this sideloads never repaints on
        // resize (that's the point of the quirk flag), so xterm's own
        // default of never reflowing the cursor's own line leaves it on
        // stale pre-resize geometry -- exactly where PSReadLine's next
        // repaint lands wrong. xterm.js has no complete fix for this
        // (xterm.js#5321 was reverted wholesale by #5358 after bricking
        // buffers in VS Code; xterm.js#5319 -- this exact symptom -- was
        // closed as "track in vscode#224488" rather than fixed, and VS
        // Code's own sideload of this same conpty build still has open
        // resize-duplication reports). `reflowCursorLine` (xterm.js#5234,
        // off by default because normal shells repaint themselves) is a
        // measurable mitigation, not a confirmed complete fix: verified via
        // test/xterm-reflow.test.ts that it corrects the cursor's ROW on a
        // resize in both directions, but xterm.js#5522 (merged, milestone
        // 7.0.0) documents that it will NEVER correct the cursor's COLUMN,
        // by design. It does also prevent real content loss on a narrowing
        // resize (measured, not just row placement). Residual + upstream
        // watch tracked in #432. Not enabled outside this conpty branch:
        // normal shells against a non-quirked host already repaint their
        // own prompt line, and forcing a reflow there could fight that
        // repaint instead of nothing at all.
        this.pane.term.options.reflowCursorLine = true;
      }
    } catch {
      // Backend info is a tuning hint only — never block the terminal on it.
    }
    this.pane.term.open(this.pane.termEl);
    this.pane.term.textarea?.addEventListener("focus", () => this.pane.events.onFocus(this.pane));
    this.tryWebgl();
    this.pane.fit.fit();

    // Everything is wired before the process exists: input queues in the
    // ordered writer until the PTY is ready, and the output router buffers
    // until we attach.
    // #518: the origin flag is read RIGHT HERE, synchronously, because that is
    // the only moment it means anything. xterm fires `onKey` immediately
    // before the `onData` it produced (and `term.paste()` triggers its own
    // `onData` synchronously from the call), so a human-originated write reads
    // the mark still open; a query auto-reply is emitted while xterm parses
    // program output — a different turn — and reads it closed. The writer
    // carries the flag with the data from here on, since the actual IPC send
    // happens later and asynchronously.
    this.pane.term.onData((data) => this.pane.writer.write(data, this.pane.humanOrigin.isHuman));
    // #440 B2 (review round 3, B2-R): `onData` is NOT a human-input signal —
    // it fires for EVERYTHING the terminal sends to the PTY, including data
    // xterm generates entirely on its own with no key pressed: OSC 10/11
    // color-query replies, a primary-DA reply, focus reports. `wasUserInput`
    // (xterm's own internal flag) gates only scroll-to-bottom and xterm's
    // internal `_onUserInput` — never `onData`. This is the EXACT #179
    // mechanism recurring: Copilot queries the terminal's colors at boot,
    // xterm auto-answers, and that answer used to get misread as human input
    // (there, by the backend's box-occupancy tracker; here, by this pane's
    // own firstInputMs). `onKey` is the fix: it fires ONLY for genuine
    // keyboard events, never for data the terminal manufactures itself — a
    // structural guarantee, not a pattern match against an open set of
    // possible auto-reply shapes (which is why this doesn't try to filter
    // `onData` by escape-sequence prefix instead: bracketed paste itself
    // starts with ESC, so a naive filter would misfire on real pastes).
    this.pane.term.onKey(() => this.pane.markFirstInput());
    // #518: the two human-input paths `onKey` never sees. Both are still
    // structural — they are DOM events on the terminal's own textarea, and
    // xterm never routes a query auto-reply through the textarea (it calls
    // `triggerDataEvent` directly) — but neither is a key event:
    //
    //  - an IME commit, which `_finalizeComposition` sends from a
    //    `setTimeout(…, 0)`, i.e. a LATER TASK than `compositionend`;
    //  - `_inputEvent`'s `insertText` path (dead keys/accents, soft
    //    keyboards), which sends synchronously with no `onKey` at all.
    //
    // Registered in the CAPTURE phase on `termEl`, an ANCESTOR of the
    // textarea: the DOM runs ancestor-capture listeners before any listener on
    // the target itself, so these mark the latch before xterm's own handlers
    // (which are bound to the textarea) can emit anything. That ordering is
    // required for the synchronous `insertText` path — and it is also why the
    // deferred close CANNOT rely on registration order: running first means our
    // close timer is queued FIRST, so a one-hop close beat xterm's send and
    // broke exactly the case this exists for (#528 review B1). `markDeferred`
    // takes two timer hops instead, which no registration order defeats — see
    // `humanorigin.ts`.
    //
    // Without this, CJK/Japanese/Korean typing would classify as non-human and
    // silently lose the very protection these guards exist to give.
    for (const ev of ["compositionstart", "compositionupdate", "compositionend", "input"]) {
      this.pane.termEl.addEventListener(ev, () => this.pane.markHumanInput(), true);
    }
    this.pane.resizeObs.observe(this.pane.termEl);
    // A background (orchestrator-driven) spawn must not pull focus from the
    // pane the human is typing in (#117); grid.openPane decides takeFocus.
    if (takeFocus) this.pane.focus();

    await this.attachPty(opts);
  }

  /** Throw if this spawn would give an SSH pane an orchestration identity
   *  (#887 S3). The decision itself is pure and unit-tested
   *  (`sshOrchestrationRefusal` in panesetup.ts); this is only the two call sites
   *  that can reach it — a fresh spawn and an in-place relaunch.
   *
   *  Both sides are handed over WHOLE — the options describing what the pane is
   *  about to become, and the state it already carries — because a relaunch's
   *  options describe the former while the boundary is about the latter, and that
   *  is true of the orchestration identity in exactly the same way it is true of
   *  ssh-ness. The union rule lives in the pure function so these two call sites
   *  cannot drift apart on it (PR #921 review, rev-441). */
  private refuseSshOrchestration(opts: PaneOptions): void {
    const refusal = sshOrchestrationRefusal(
      {
        ssh: !!opts.ssh,
        orchGroup: opts.orchGroup,
        orchRole: opts.orchRole,
        orchAgent: opts.orchAgent,
      },
      {
        ssh: this.pane.isSshPane,
        orchGroup: this.pane.orchGroup,
        orchRole: this.pane.orchRoleName,
        orchAgent: this.pane.orchAgent,
      }
    );
    if (refusal) throw new Error(refusal);
  }

  /** Give this pane an orchestration identity and the group chrome that comes
   *  with it. Split out of `start` so an IN-PLACE promotion (#407) can apply the
   *  same identity to a pane that is already running — one definition of "what an
   *  orchestration pane looks like", rather than a second copy in `respawnFresh`
   *  that would drift the day a button is added here.
   *
   *  A no-op without `opts.orchGroup`, so both callers can call it unconditionally. */
  private applyOrchIdentity(opts: PaneOptions): void {
    if (!opts.orchGroup) return;
    this.pane.orchGroup = opts.orchGroup;
    this.pane.orchRoleName = opts.orchRole ?? null;
    // The board lives on the orchestrator's pane; workers report there.
    this.pane.views.tasksBtn.hidden = opts.orchRole !== "orchestrator";
    // The NEEDS-YOU panel sits beside the board, on the same pane and the same
    // terms (#1091): the orchestrator is who asks the human, and the demos it
    // shows are its own board rows.
    this.pane.views.decisionsBtn.hidden = opts.orchRole !== "orchestrator";
    // The audit log is per-group and read-only, so it's useful from any
    // agent pane in the group, not just the orchestrator's.
    this.pane.views.auditBtn.hidden = false;
    // The progress timeline reads the same per-group log (plus gh), and is
    // read-only in exactly the same sense — gated identically (#608).
    this.pane.views.timelineBtn.hidden = false;
    // The token charts read the same per-group series and audit log, and are
    // read-only in exactly the same sense — gated identically (#2011).
    this.pane.views.tokensBtn.hidden = false;
    // Group lifecycle controls (pause / end orchestration) live on the
    // orchestrator's pane, alongside the task board.
    this.pane.views.groupBtn.hidden = opts.orchRole !== "orchestrator";
    // Same for the fold-group toggle (#46): it acts on the orchestrator's
    // own worker/reviewer panes.
    this.pane.groupMinBtn.hidden = opts.orchRole !== "orchestrator";
    // Steering strip (#43): only the orchestrator pane gets one. Guarded so a
    // second application (a promotion of a pane that somehow already had one)
    // can't stack two strips.
    if (opts.orchRole === "orchestrator" && !this.pane.compose.composeInput) this.pane.compose.buildComposeStrip();
    // Unread-mail chip (#1161 M5): the push says nothing about mail that was
    // already waiting when this pane opened, so the chip is seeded here — the
    // one place a pane learns it is the manager. `seedMailUnread` is a no-op for
    // every other role, decided by the same gate the push uses.
    seedMailUnread(this.pane);
  }

  /** Spawn (or respawn) the PTY for this already-open terminal and wire output /
   *  git-watch / the ordered input writer to it. Split out of `start` so the
   *  fresh-session backstop (`respawnFresh`) can reuse it without re-opening the
   *  terminal (#194 BUG-1). */
  private async attachPty(opts: PaneOptions): Promise<void> {
    try {
      await ensureOutputRouter();
      const cols = Number.isFinite(this.pane.term.cols) && this.pane.term.cols > 1 ? this.pane.term.cols : 80;
      const rows = Number.isFinite(this.pane.term.rows) && this.pane.term.rows > 1 ? this.pane.term.rows : 24;
      const ptyId = await spawnPty({
        cols,
        rows,
        cwd: opts.cwd,
        command: opts.command,
        argv: opts.argv,
        env: opts.env,
        shellKind: opts.shellKind,
      });
      if (this.pane.disposed) {
        // Retire the id as well as killing it (#1301). The backend may already
        // have drained bytes for this pty, and without this they sit in the
        // router's pre-attach buffer for a handler that is never coming —
        // `dispose` already ran, so nothing else will ever name this id.
        detachOutput(ptyId);
        killPty(ptyId).catch(() => {});
        return;
      }
      this.pane.ptyId = ptyId;
      this.pane.sentSize = `${cols}x${rows}`;
      // Reconcile: if the pane was resized while the spawn was in flight,
      // the debounced fit will notice the size drifted and resend once.
      this.pane.applyFit();
      // `this` is the OWNER, not decoration: the router releases whatever this
      // pane was attached to before, so a respawn cannot leave the previous
      // id's closure — which captures this pane, its terminal and its whole
      // buffer — stranded in a module-level map (#1301, see ptyroute.ts).
      attachOutput(this.pane, ptyId, (bytes) => {
        // Latched on ARRIVAL, never on flush: this is "did the process ever
        // print anything" (the DOA-revival signature, #281/#280), a fact about
        // the pty, not about when we chose to render it.
        this.pane.receivedOutput ||= bytes.length > 0;
        this.acceptOutput(bytes);
      });
      // React to repo changes made outside this pane's shell (#36): the
      // backend watch is pointed at the repo on each cwd report below.
      //
      // Never for an SSH pane (#887 S3): the repo this pane works in is on the
      // remote host, and every path it will ever report names a filesystem this
      // machine cannot see. A watch registered here would either resolve to
      // nothing or — worse, for a remote path that happens to also exist
      // locally — report changes from a completely unrelated local repo as
      // though they were the remote one's.
      if (!this.pane.isSshPane) {
        attachGitWatch(this.pane, ptyId, () => this.pane.onExternalGitChange());
        if (this.pane.cwdRaw) {
          this.pane.watchedPath = this.pane.cwdRaw;
          setGitWatch(ptyId, this.pane.cwdRaw);
        }
      }
      // Bind the ordered writer to this PTY and flush anything typed/pasted
      // while it was starting, in arrival order.
      this.pane.writer.ready((data, human) => writePty(ptyId, data, human));
    } catch (err) {
      // Never leave a dead black pane: surface the failure in-terminal.
      this.pane.term.writeln(`\x1b[91morrerix: failed to start shell\x1b[0m`);
      this.pane.term.writeln(`\x1b[90m${String(err)}\x1b[0m`);
    }
  }

  /** Take one arrived chunk of PTY output and either write it to xterm now or
   *  hold it for the rest of this pane's throttle window (#720). The policy is
   *  pure and lives in panethrottle.ts; this is only its DOM/timer wiring.
   *
   *  Why hold anything at all: xterm coalesces every write inside one animation
   *  frame into a single `renderRows` pass and coalesces nothing ACROSS frames
   *  (`RenderDebouncer`), so a pane written to every frame renders every frame.
   *  #714 already made that at most one event per frame per pane; what is left
   *  is the render pass itself, paid N times over for N visible panes on one JS
   *  thread. Writing an unfocused pane once per window instead collapses those
   *  passes without touching a byte of the stream. */
  private acceptOutput(bytes: Uint8Array): void {
    if (this.pane.disposed) return;
    // #2122 slice A2: count the chunk as it ARRIVES, not as it is flushed —
    // the throttle below decides when xterm sees these bytes, and "is this
    // pane producing output" must not move with a rendering decision. One
    // counter add; `Date.now()` (not `performance.now()`, which this method
    // uses for the throttle) because the snapshot compares this against
    // `firstInputMs`'s clock.
    this.pane.activity.noteOutput(bytes.length, Date.now());
    this.pendingOut.push(bytes);
    this.pendingOutBytes += bytes.length;
    const decision = decideFlush({
      live: this.pane.isActivePane,
      nowMs: performance.now(),
      lastFlushMs: this.lastFlushMs,
      pendingBytes: this.pendingOutBytes,
      windowMs: getSettings().unfocusedRenderThrottleMs,
      maxPendingBytes: MAX_PENDING_BYTES,
      // #813: read per chunk rather than latched on `visibilitychange`. The
      // event is a notification and a notification can be missed — the same
      // reason pollgate.ts re-READS the state instead of trusting the event it
      // was suppressed by — and here a missed event would mean a pane that
      // keeps deferring behind a clamped timer for as long as the window stays
      // hidden, with nothing to release it. A property read per chunk is
      // cheaper than the timer it replaces.
      hidden: !sharedVisibility().visible(),
    });
    if (decision.kind === "flush") {
      this.flushOutput();
      return;
    }
    // One timer per window, armed by the chunk that opened it. Re-arming on
    // every subsequent chunk would push the flush out indefinitely for a pane
    // that never goes quiet — a trailing-edge debounce, which is precisely the
    // starvation this must not be.
    if (this.flushTimer === undefined) {
      this.flushTimer = window.setTimeout(() => this.flushOutput(), decision.dueInMs);
    }
  }

  /** Hand every held chunk to xterm, in arrival order, and re-open the window.
   *  Safe (and cheap) to call with nothing held — every caller that is about to
   *  write to, reset, or re-geometry the terminal itself calls it first, so the
   *  held bytes can never land AFTER something that was issued later. */
  flushOutput(): void {
    if (this.pane.disposed) return; // `term.write` on a disposed terminal throws
    if (this.flushTimer !== undefined) {
      clearTimeout(this.flushTimer);
      this.flushTimer = undefined;
    }
    if (!this.pendingOut.length) {
      // Nothing was written, so nothing started a window: leaving the pane
      // marked quiet is what keeps the NEXT chunk on the leading edge instead
      // of making an unrelated flush call (a fit, a focus change) silently cost
      // the following chunk a full window of latency.
      this.lastFlushMs = WOKEN;
      return;
    }
    const chunks = this.pendingOut;
    this.pendingOut = [];
    this.pendingOutBytes = 0;
    this.lastFlushMs = performance.now();
    // Separate writes, not one concatenated buffer: xterm's WriteBuffer only
    // schedules a parse task when it was empty, so these N pushes cost one
    // parse task and one render pass between them — the same as a single write,
    // without copying every byte a second time to get there.
    //
    // #813 residual — the half of the deferral `decideFlush` cannot see.
    // Flushing is the loomux side; each `Terminal.write` still lands in
    // `WriteBuffer.write`, which schedules its parse on a `setTimeout` unless a
    // keystroke set `_didUserInput` first. A hidden document clamps that timer
    // exactly as it clamps the one `decideFlush` removed, so the terminal's
    // auto-replies (DA/DSR/OSC-colour/XTVERSION answers, DEC-1004 focus
    // reports — emitted only when the query is PARSED) stay stalled in front
    // of the one path out of xterm that is not display, and an agent CLI
    // waiting on one of them waits on our timer. A workstation lock is hidden
    // for its whole duration, which is #813's window. While hidden, re-arm
    // xterm's own fast path per chunk so the FIRST chunk of a quiet pane is
    // parsed — and its reply emitted back down the pty — in this same call,
    // with no clamped timer in series. Best-effort beyond that: the fast path
    // only engages while the write buffer is empty, and `_innerWrite` still
    // yields at its 12ms write timeout and reschedules via `setTimeout`
    // (clamped again), so a busy pane under lock can outlast it; the next full
    // repaint re-parses. Visible, the timer path is fine (nothing is clamped)
    // and forcing sync parses would only cost the render thread.
    const syncParse = !sharedVisibility().visible();
    for (const chunk of chunks) {
      if (syncParse) hintXtermSyncParse(this.pane.term);
      this.pane.term.write(chunk);
    }
  }

  /** Throw held output away and return the pane to quiet (#720). Dropping,
   *  not flushing, and both callers have the same reason: nothing downstream
   *  will ever render these bytes, so queueing them to `term.write` would only
   *  schedule a parse against a terminal that is about to be wiped or is
   *  already gone. `respawnFresh` is about to `term.reset()` (see the call
   *  site); `dispose` is about to `term.dispose()` and drop the pane. Anywhere
   *  else, held output is flushed. */
  discardOutput(): void {
    if (this.flushTimer !== undefined) {
      clearTimeout(this.flushTimer);
      this.flushTimer = undefined;
    }
    this.pendingOut = [];
    this.pendingOutBytes = 0;
    this.lastFlushMs = WOKEN;
  }

  /** Return this pane to the leading edge and write out anything held (#720).
   *  Called wherever the human touches the pane — focusing it, typing into it —
   *  because from that instant its output is an interactive latency again, and
   *  a throttle the human can perceive is worse than the render passes it
   *  saves. */
  wakeOutput(): void {
    this.flushOutput();
    // Mark quiet AFTER the flush, not before: the flush that just wrote would
    // otherwise open a window, and the echo of the keystroke the human just
    // pressed — which has not even reached the pty yet — would land in it.
    this.lastFlushMs = WOKEN;
  }

  /** Respawn this pane with a FRESH process in place, reusing the already-open
   *  terminal — the runtime backstop when a resumed agent's `--resume` exited on
   *  a missing/deleted conversation (#194 BUG-1). Same pane, position, and cwd;
   *  clears the dead error text and starts `opts`' command with a fresh ordered
   *  writer bound to the new PTY. Not itself a resume, so it can't re-trigger the
   *  fallback (the caller also makes the fallback one-shot).
   *
   *  #407 gave it a second caller: an in-place PROMOTION, where the pane keeps
   *  its terminal but comes back as an orchestrator (`opts.orchGroup`/`orchRole`/
   *  `orchAgent`/`badge`, and `opts.env` — a promoted pane's gh-shim PATH and
   *  `LOOMUX_GROUP_DIR` are carried by `attachPty` below exactly as a spawned
   *  agent pane's are). "Fresh" describes the PROCESS, not the conversation: the
   *  command it is handed can perfectly well be `claude --resume <its own id>`. */
  async respawnFresh(opts: PaneOptions = {}): Promise<void> {
    if (this.pane.disposed) return;
    // The promotion door onto the #887/#888 boundary (#407 relaunches a pane
    // in place WITH an orchestration identity). It closes here rather than only
    // in `start`, because a promotion never calls `start` — and it reads this
    // pane's EXISTING ssh-ness, not just `opts`, since a promotion's options
    // describe the orchestrator it wants, not the pane it is rewriting.
    this.refuseSshOrchestration(opts);
    if (opts.ssh) {
      this.pane.sshProfileId = opts.ssh.profileId;
      this.pane.sshDefaultCli = opts.ssh.defaultCli ?? null;
    }
    // #887 S4: this pane is coming back to life, so any floating Reconnect card
    // is stale by definition — dropped here rather than at the (several) call
    // sites, so no future relaunch path can forget it and leave a card offering
    // to reconnect a pane that just did.
    this.clearReconnectCard();
    this.pane.exited = false;
    // Release the OUTGOING pty's router attachments before the id is forgotten
    // (#1301). Two things go wrong without it. The router keeps a handler
    // closure over this pane under an id `dispose` will never name again — a
    // whole retained pane, terminal buffer included — and the backend keeps
    // polling the old id's repo, because `git_unwatch` is only ever sent for
    // an id someone still remembers. `attach` would release the old id on its
    // own when the new pty arrives, but that is one `await` later and only if
    // the spawn succeeds: a failed respawn must not be the case that leaks.
    detachOutputOwner(this.pane);
    detachGitWatchOwner(this.pane);
    if (this.pane.ptyId !== null) stopGitWatch(this.pane.ptyId);
    this.pane.ptyId = null;
    if (opts.name) this.pane.setName(opts.name);
    this.pane.launchedCommand = !!opts.command?.trim();
    this.pane.spawnCommand = opts.command ?? null;
    this.pane.syncNotesBtn();
    this.pane.spawnArgv = opts.argv ?? null;
    this.pane.spawnShellKind = opts.shellKind ?? null;
    // A respawn can change the program outright — a dormant shell Started with a
    // recorded agent command, or a welcome pane promoted to an orchestrator — so the
    // mark is re-derived here rather than only at first start.
    this.pane.refreshAgentMark();
    // Same learn-it-from-the-line fallback as start() (#440) — a fresh respawn
    // (BUG-1 backstop, or a dormant Start with a recorded command) can equally
    // carry a self-naming --resume/--session-id.
    this.pane.agentSessionId = opts.sessionId ?? adoptableSessionId(opts.command ?? null, opts.argv ?? null);
    this.pane.forkOf = opts.forkOf ?? null;
    this.pane.firstInputMs = null; // fresh process, nothing typed into it yet (#440 B2)
    if (opts.cwd) {
      this.pane.cwdRaw = opts.cwd;
      void this.pane.refreshDir(opts.cwd);
    }
    // #720: DROP the dead process's held output — the one place this file
    // discards rather than flushes, and deliberately. `reset()` clears the
    // buffer synchronously while `term.write` parses asynchronously, so
    // flushing here would not preserve those bytes anyway: it would queue them
    // to be parsed AFTER the wipe, painting a dead pty's tail over the fresh
    // session. They are bytes `reset()` was always going to erase; the only
    // choice is whether they are erased or resurrected in the wrong place.
    this.discardOutput();
    this.pane.term.reset(); // wipe the "No conversation found …" error + resume banner
    this.pane.writer = createOrderedWriter(); // a fresh input pipe for the new PTY
    // #407: an in-place PROMOTION also changes what this pane IS. Applied here,
    // before the spawn below, for two reasons: the badge/board chrome must be up
    // when the orchestrator's first bytes arrive, and the steering strip changes
    // the terminal's height — doing it now means the fit below is computed once,
    // against a pane with NO live pty, so the geometry never reaches a ConPTY as
    // a resize (CLAUDE.md constraint 1).
    if (opts.orchGroup) {
      if (opts.badge) this.pane.setBadge(opts.badge);
      if (opts.orchAgent) this.pane.orchAgent = opts.orchAgent;
      // The standalone channel identity does not survive: the promotion retired
      // that `__solo__` agent backend-side, so leaving the carrier on would show
      // a channel chip for an endpoint nothing can deliver to. Scoped to this
      // arm — `respawnFresh` still ignores `opts.channelAgent` for every other
      // caller (main.ts's BUG-1 fallback sets it explicitly afterwards).
      //
      // The CHIP goes with it, explicitly rather than by waiting for the
      // `orch-channel` disconnect event: that event is matched to a pane by agent
      // id, and this pane's id is about to become the orchestrator's — so an
      // event landing a moment later would find nothing to clear and leave a live
      // chip on a channel this pane is no longer in.
      this.pane.channelAgentInfo = null;
      this.pane.setConnected(null);
      this.applyOrchIdentity(opts);
      this.pane.fit.fit();
    }
    await this.attachPty(opts);
  }

  /** Render a DORMANT restore placeholder (#194 P4): no terminal, no PTY, just
   *  `contentEl` (a Start/Resume affordance the caller wires). `record` is the
   *  persisted leaf this pane stands in for, retained so capture() re-serializes
   *  it unchanged — a restore left dormant persists identically for next boot.
   *  Nothing here spawns anything, honoring the group no-double-spawn contract. */
  startDormant(record: PersistedPane, contentEl: HTMLElement): void {
    this.pane.setName(record.name);
    this.pane.el.classList.add("is-dormant");
    const wrap = document.createElement("div");
    wrap.className = "pane-dormant";
    wrap.appendChild(contentEl);
    this.dormantEl = wrap;
    this.pane.dormantRecord = record;
    this.pane.el.appendChild(wrap);
  }

  /** True while this pane is a dormant restore placeholder (no PTY yet). */
  get isDormant(): boolean {
    return this.dormantEl !== null;
  }

  /** Float a card over this pane's still-mounted terminal (#887 S4) — the
   *  Reconnect offer on an ssh pane whose connection dropped. Returns a
   *  disposer; mounting a second card replaces the first, so a pane can never
   *  accumulate them (a reconnect that itself drops re-offers exactly one).
   *
   *  Deliberately additive-only: no fit, no resize, no reflow. The card is
   *  positioned over the terminal by CSS, so the ConPTY never learns it exists
   *  and the scrollback under it is untouched (CLAUDE.md constraint 1). */
  showReconnectCard(el: HTMLElement): () => void {
    this.clearReconnectCard();
    const wrap = document.createElement("div");
    wrap.className = "pane-reconnect";
    wrap.appendChild(el);
    this.reconnectEl = wrap;
    this.pane.el.appendChild(wrap);
    return () => this.clearReconnectCard();
  }

  /** Remove the floating Reconnect card, if one is up. Idempotent — called both
   *  by the disposer above and unconditionally by `respawnFresh`, so a pane that
   *  comes back to life can never keep offering to reconnect. */
  private clearReconnectCard(): void {
    this.reconnectEl?.remove();
    this.reconnectEl = null;
  }

  /** The kind of a dormant placeholder ("agent" | "orch" | "ssh"), or null when
   *  not dormant. Lets the grid/tab-bar tell a dormant Start pane from a dormant
   *  group pane — and from a dormant SSH Reconnect card (#887 S4) — without
   *  re-reading the whole record. */
  get dormantKind(): PersistedPaneKind | null {
    return this.pane.dormantRecord?.paneKind ?? null;
  }

  /** Convert a dormant agent placeholder into a live pane when the human clicks
   *  Start: tear down the placeholder and spawn the recorded command in place.
   *  Only used for `dormant-agent` (no-session CLIs) — a dormant GROUP is revived
   *  through resumeOrchSession, never here (the double-spawn contract).
   *
   *  #479 review finding 2: this used to remove the placeholder element from
   *  the document IMMEDIATELY, before awaiting `start()` at all — so ANY
   *  later failure in the caller's own post-spawn wiring (main.ts's
   *  dormant-agent `onClick` — `remint.bind`, `onGridChanged`, both of which
   *  run strictly AFTER this method returns) rendered its error card into an
   *  element already gone from the DOM: invisible, worse than the pre-PR
   *  behavior where the same throw escaped as an uncaught rejection and hit
   *  the global banner. The element itself now stays mounted until `start()`
   *  settles (success OR throw, via `finally`) rather than being torn down
   *  before it even begins — this closes the window for a failure INSIDE
   *  `start()` itself, but NOT for `remint.bind`/`onGridChanged` afterward
   *  (this method has already returned and its own `finally` has already
   *  removed the element by the time those run). That remaining window is
   *  covered by `dormantCard`'s `render()` falling back to a toast when its
   *  element is no longer connected — the design note's "residual gap"
   *  section states plainly which failures land on which surface; this
   *  comment is not the place to re-claim more than that.
   *
   *  `this.dormantEl` (the FIELD, distinct from the captured local `el`) is
   *  still nulled immediately, unchanged from before: `isDormant` (`this.
   *  dormantEl !== null`) must flip false the instant Start is clicked, not
   *  after the whole spawn settles — main.ts's #440 D2 background
   *  resume-candidate prefetch gates on `pane.isDormant` to decide whether
   *  to append a SECOND action button, and widening that window would let
   *  it add one while this Start is still in flight (a race this fix must
   *  not introduce while closing the other one). */
  async startFromDormant(opts: PaneOptions = {}): Promise<void> {
    const el = this.dormantEl;
    this.dormantEl = null;
    this.pane.dormantRecord = null;
    this.pane.el.classList.remove("is-dormant");
    try {
      await this.start(opts, true);
    } finally {
      el?.remove();
    }
  }

  private tryWebgl(): void {
    // No open terminal = a welcome, dormant, or files pane. WebglAddon.activate()
    // throws on such a terminal (caught below, but pointlessly) and there is nothing
    // to render anyway — setHidden() reaches here on every tab switch, so this is a
    // real throw/catch per hidden PTY-less pane, not a hypothetical.
    if (this.webgl || this.hiddenTab || !this.pane.hasTerminal()) return;
    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => this.handleWebglLoss(webgl));
      this.pane.term.loadAddon(webgl);
      this.webgl = webgl;
      this.webglAcquiredMs = performance.now();
    } catch {
      // WebGL unavailable — xterm's DOM renderer still works fine.
    }
  }

  /** Fall back to the DOM renderer and schedule a BOUNDED re-acquire (#720).
   *
   *  Disposing is still the right immediate move — the context is gone and
   *  xterm's DOM renderer is what keeps the pane painting — but it used to be
   *  the whole story, which left one lost context making one pane in a grid of
   *  six permanently and invisibly the expensive one. `planWebglRetry` decides
   *  whether and when to try again; the bound is not optional, because a WebGL
   *  context is a capped resource and one pane re-acquiring is what evicts
   *  another's, so an unbounded retry is a live-lock between panes rather than
   *  a recovery. See webglretry.ts. */
  private handleWebglLoss(lost: WebglAddon): void {
    lost.dispose(); // falls back to DOM renderer
    if (this.webgl !== lost) return; // superseded (hide/show already replaced it)
    this.webgl = null;
    const plan = planWebglRetry({
      priorLosses: this.webglLosses,
      healthyMs: performance.now() - this.webglAcquiredMs,
    });
    this.webglLosses = plan.losses;
    if (plan.delayMs === null) return; // budget spent: stay on DOM until a hide/show
    clearTimeout(this.webglRetryTimer);
    this.webglRetryTimer = window.setTimeout(() => {
      this.webglRetryTimer = undefined;
      // Re-check rather than trust the state this timer was armed in: the pane
      // can be disposed, detached, or hidden during the wait, and tryWebgl's own
      // guards are the single place that decision lives.
      if (this.pane.disposed || !this.pane.termEl.isConnected) return;
      this.tryWebgl();
    }, plan.delayMs);
  }

  /** Show/hide bookkeeping for a project-tab switch (#63). Hiding drops the
   *  WebGL context (freeing it for the active tab and cutting idle VRAM) and
   *  latches `hiddenTab` so start()/tryWebgl won't re-create one while hidden;
   *  showing clears the latch and reloads it (via the onContextLoss→DOM fallback
   *  if the GPU is out of contexts). Purely a rendering concern — the PTY and
   *  buffer are untouched, so no resize and no scrollback loss. Safe to call
   *  before the terminal is even open (tryWebgl no-ops until start opens it). */
  setHidden(hidden: boolean): void {
    if (this.pane.disposed) return;
    this.hiddenTab = hidden;
    // #720: a hide/show is a deliberate act by the human that changes the
    // context situation wholesale (every pane in the outgoing tab just released
    // one), so it clears the retry streak — the manual half of the bound in
    // webglretry.ts. Cancel any pending re-acquire either way: hiding makes it
    // wrong (tryWebgl would refuse anyway, but leaving a live timer on a hidden
    // pane is just litter), and showing supersedes it with the immediate
    // attempt below.
    clearTimeout(this.webglRetryTimer);
    this.webglRetryTimer = undefined;
    this.webglLosses = 0;
    if (hidden) {
      this.webgl?.dispose();
      this.webgl = null;
    } else if (this.pane.termEl.isConnected) {
      this.tryWebgl();
    }
  }

  /** An HTML snapshot of the terminal viewport, for a background tab's preview
   *  thumbnail (#63). Serializes the in-memory buffer (NOT the DOM),
   *  so it works while the pane is hidden/zero-width — the whole point: a preview
   *  must never require a laid-out element, which would re-arm applyFit and fire
   *  a PTY resize.
   *
   *  serializeAsHTML (not serialize): the string serializer emits cursor-forward
   *  escapes (`ESC[nC`) to skip blank cells, which stripping collapses runs of
   *  spaces ("Please count" → "Pleasecount", #63). The HTML serializer
   *  emits a literal space per blank cell and per-run `<span style='color:…'>`,
   *  so the preview keeps spacing AND color. The caller parses this SAFELY (spans
   *  → textContent + whitelisted styles), never innerHTML — the addon does not
   *  escape cell text. Returns "" if serialization isn't available. */
  serializeViewportHtml(): string {
    // A pane whose terminal was never opened (welcome / dormant / files) has no
    // viewport to serialize — an empty string leaves the tab preview blank for
    // that slot rather than painting a phantom 80×24 of nothing.
    if (this.pane.disposed || !this.pane.hasTerminal()) return "";
    try {
      if (!this.serializer) {
        this.serializer = new SerializeAddon();
        this.pane.term.loadAddon(this.serializer);
      }
      // scrollback: 0 → just the visible screen, which is all a thumbnail shows.
      return this.serializer.serializeAsHTML({ scrollback: 0 });
    } catch {
      return "";
    }
  }
}
