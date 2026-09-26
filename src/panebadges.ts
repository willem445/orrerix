// The pane header's status chips, split out of pane.ts (#3498 F1): the role badge,
// the attention chip and its dismiss, the watched mark (#3319), the fork crumb
// (#3368), the delivery-held (#246), queue-depth (#814), unread-mail (#1161) and
// prompt-cache-age (#3407) chips, and the cross-workspace channel chip (#271).
//
// A satellite of `Pane`, not a model: it owns these chips' DOM and state and reads
// the rest of the pane through `pane`. DOM glue, hand-validated like pane.ts
// itself. Every chip here is header chrome that floats in the header row, so
// CLAUDE.md constraint 1 (never resize the PTY for a UI feature) holds
// trivially. Layout conventions: docs/design/module-layout.md; the header's
// fold ladder that prices these chips: docs/design/pane-header.md.

import { invoke } from "./transport.ts";
import { showToast } from "./toast";
import { attentionPresentation, attentionDismiss, attentionChanged } from "./attention";
import { WATCHED_MARK, WATCHED_TITLE } from "./watchedpanes";
import { dismissStranded, requestCompact } from "./orchestration";
import {
  cacheChipLabel,
  cacheChipTitle,
  cacheState,
  wakeCostLine,
  type CacheAgeReading,
} from "./cacheage";
import { heldPresentation } from "./heldbadge";
import { queuePresentation, type QueueDepthReading } from "./queuebadge";
import { mailboxPresentation } from "./mailboxbadge";
import { showContextMenu, type MenuItem } from "./contextmenu";
import type { PaneBadge, PaneChannelBadge, Pane } from "./pane";

export class PaneBadges {
  /** "needs attention" chip in the header (attention routing #6); hidden until
   *  the backend flags this pane. */
  private attnChip: HTMLButtonElement;
  /** The explicit-dismiss control beside the attention chip (#825 M1). Shown
   *  only for the one LATCHED reason (`stranded`), which is the only chip a
   *  human can otherwise be left unable to clear. */
  private attnDismiss: HTMLButtonElement;
  attentionReason: string | null = null;
  attentionDetail: string | null = null;
  /** The human's "come back to this one" mark (#3319). Header chrome beside
   *  the attention chip, and it is NOT a second attention reason: that one is
   *  derived by the backend from what the agent is doing and cleared by
   *  whatever caused it, while this is set by a human gesture and cleared by
   *  nothing else — not focus, not a report arriving, not the pane going
   *  quiet. The two co-exist on one pane and that is the case the feature
   *  exists for. Hidden when the pane is not watched. */
  private watchChip: HTMLButtonElement;
  /** The fork crumb (#3368): `↰ <parent name>` on a forked pane, which goes to
   *  the parent. Set from outside (`setForkCrumb`) because the parent's name
   *  and whereabouts are the window's to know, not this pane's. */
  private forkCrumb: HTMLButtonElement;
  private forkCrumbGo: (() => void) | null = null;
  private isWatched = false;
  /** "delivery held" chip in the header (#246): the moment loomux is
   *  withholding an outbound prompt to this pane because it believes the
   *  human's own input occupies the CLI's box. Hidden until the backend
   *  flags a hold in progress; cleared by its own paired event, never a
   *  frontend timer. Purely informational — unlike attnChip, clicking it
   *  does nothing to acknowledge/clear (only the backend resolving the hold
   *  does). */
  private heldChip: HTMLElement;
  heldReason: string | null = null;
  /** "delivery queue depth" chip in the header (#814): how many prompts are
   *  waiting for this pane and how long the oldest has waited, so a stuck
   *  queue is visible without hovering anything. Informational like
   *  `heldChip`; driven by the backend's `orch-queue-depth` push and cleared
   *  by this pane's absence from it, never by a frontend timer. */
  private queueChip: HTMLElement;
  private queueReading: QueueDepthReading | null = null;
  /** "unread mail" chip in the header (#1161 M5): how many messages the
   *  orchestrator has posted that this MANAGER pane has not read yet. Worn by no
   *  other role — a group's mailbox belongs to exactly one pane (`MANAGER_MAX`
   *  is 1, enforced at parse), and `mailboxPanes` is the single gate deciding
   *  which pane that is. Informational like `queueChip`, and for a sharper
   *  reason: nothing a click here could do would make the manager read its mail.
   *  The thing that makes it read is the human speaking to it. */
  private mailChip: HTMLElement;
  private mailUnread = 0;
  /** The prompt-cache age chip (#3407): `hot 3m` / `cooling 48m/60m` / `cold`,
   *  or `idle 12m` where no TTL is known. Header chrome like the chips beside
   *  it — it floats in the header and never touches the terminal's geometry, so
   *  constraint 1 holds trivially. Fed by `noteCacheAge` off the tab strip's
   *  existing snapshot read; a click opens its menu (the last wake's cost and
   *  "Compact now"). Hidden until a request has been observed. */
  private cacheChip: HTMLButtonElement;
  /** The last reading `noteCacheAge` was handed, for the menu and `facts()`. */
  cacheReading: CacheAgeReading | null = null;
  /** Whether an `orch-mailbox-changed` PUSH has ever been applied to this pane.
   *  The seed read (`applyMailSeed`) is asynchronous and a push can land while
   *  it is in flight, so without this the seed's older number would overwrite a
   *  newer one — including a push of 0, which is how an emptied mailbox is
   *  reported. A push is always the fresher of the two, so once one has arrived
   *  the seed has nothing left to contribute. */
  private mailPushed = false;
  /** Cross-workspace channel chip (#271): shown when this pane is a live channel
   *  member. Clicking it disconnects — the "easy close from the indicator itself"
   *  requirement — separate from the pane-menu Disconnect item, same destination. */
  private channelChip: HTMLButtonElement;
  private channelInfo: PaneChannelBadge | null = null;

  constructor(private readonly pane: Pane, header: HTMLElement) {
    // "Needs attention" chip: clicking it focuses the pane and acknowledges
    // the signal (clears a latched report backend-side). Hidden until flagged.
    this.attnChip = document.createElement("button");
    this.attnChip.className = "pane-attn";
    this.attnChip.hidden = true;
    this.attnChip.addEventListener("click", (e) => {
      e.stopPropagation();
      this.pane.events.onFocus(this.pane);
      this.pane.focus();
      this.acknowledgeAttention();
    });
    header.appendChild(this.attnChip);

    // The explicit dismiss (#825 M1), as a SIBLING button rather than something
    // inside the chip: the chip is itself a <button>, and a button nested in a
    // button is invalid HTML that browsers silently un-nest. Header chrome, like
    // the chip it sits beside — it floats in the header and never touches the
    // terminal's size, so constraint 1 (never resize the PTY for a UI feature)
    // holds trivially. Hidden unless the pane wears a dismissible chip.
    this.attnDismiss = document.createElement("button");
    this.attnDismiss.className = "pane-attn-dismiss";
    this.attnDismiss.hidden = true;
    this.attnDismiss.addEventListener("click", (e) => {
      // Not `onFocus`/`focus()` like the chip beside it: dismissing is the
      // human saying they are DONE with this alert, which is the opposite of
      // asking to be taken to the pane.
      e.stopPropagation();
      this.dismissAttention();
    });
    header.appendChild(this.attnDismiss);

    // The watched mark (#3319). Header chrome like the two chips above it — it
    // floats in the header and never touches the terminal's size, so
    // constraint 1 holds trivially and toggling a watch can never reach a
    // ConPTY resize. Clicking it STOPS the watch rather than focusing the
    // pane: the mark's whole job is to be visible from somewhere else, and a
    // human who clicks their own mark is saying they are done with it (the
    // same reading `attnDismiss` takes, and the opposite of `attnChip`, which
    // exists to take you to the pane).
    this.watchChip = document.createElement("button");
    this.watchChip.className = "pane-watch";
    this.watchChip.textContent = WATCHED_MARK;
    this.watchChip.title = WATCHED_TITLE;
    this.watchChip.hidden = true;
    this.watchChip.addEventListener("click", (e) => {
      e.stopPropagation();
      this.setWatched(false);
    });
    header.appendChild(this.watchChip);

    // The fork crumb (#3368). Header chrome on the same terms as the chips
    // around it — it floats in the header row and never touches the terminal's
    // geometry, so constraint 1 holds trivially. `chip-yields` so it gives up
    // its room (ellipsising the parent's name) before the pane's own name does:
    // the crumb is context, the name is the drag handle. It is walked off the
    // header by `measureHeaderFixed` like every other chip, so the fold ladder
    // prices it without being told.
    this.forkCrumb = document.createElement("button");
    this.forkCrumb.className = "pane-fork-crumb chip-yields";
    this.forkCrumb.hidden = true;
    this.forkCrumb.addEventListener("click", (e) => {
      e.stopPropagation();
      this.forkCrumbGo?.();
    });
    header.appendChild(this.forkCrumb);

    // "Delivery held" chip (#246): purely informational, no click handler —
    // the hold only clears when the backend resolves it.
    this.heldChip = document.createElement("span");
    this.heldChip.className = "pane-held";
    this.heldChip.hidden = true;
    header.appendChild(this.heldChip);

    // "Queue depth" chip (#814): header chrome beside the held chip, floating
    // over nothing — it never touches the terminal's geometry, so constraint 1
    // (never resize the PTY for a UI feature) holds trivially. No click
    // handler: the queue drains when the pane is free, and there is nothing a
    // click here could truthfully do about it.
    this.queueChip = document.createElement("span");
    // `chip-yields`: this chip declares a shrink weight of its own (100), and
    // `styles.css`'s row-shrink rule must not overwrite it. A marker rather than
    // a name in that rule's `:not()` list, because the third chip to want a
    // heavy weight would otherwise be silently flattened to 1 (rev-final round 2
    // finding 1, which is exactly what happened to `.pane-mail`).
    this.queueChip.className = "pane-queue chip-yields";
    this.queueChip.hidden = true;
    header.appendChild(this.queueChip);

    // "Unread mail" chip (#1161 M5): header chrome beside the queue chip, on
    // the same terms — it floats in the header and never touches the terminal's
    // geometry, so constraint 1 (never resize the PTY for a UI feature) holds
    // trivially. No click handler, and here that is not a default: the manager
    // reads its mail when its human speaks to it, so a click that "marked it
    // read" would be the app answering for the human on the one pane whose
    // whole contract is that it never does.
    this.mailChip = document.createElement("span");
    this.mailChip.className = "pane-mail chip-yields";
    this.mailChip.hidden = true;
    header.appendChild(this.mailChip);

    // The prompt-cache age chip (#3407). `chip-yields`, like the queue and mail
    // chips: it is informational, so it gives up its room before the title (the
    // pane's drag handle) does. A button, because it opens a menu — but the
    // menu is `showContextMenu`'s floating overlay, never header geometry.
    this.cacheChip = document.createElement("button");
    this.cacheChip.className = "pane-cache chip-yields";
    this.cacheChip.type = "button";
    this.cacheChip.hidden = true;
    this.cacheChip.addEventListener("click", (e) => {
      e.stopPropagation();
      const r = this.cacheChip.getBoundingClientRect();
      this.openCacheMenu(r.left, r.bottom);
    });
    header.appendChild(this.cacheChip);

    // Cross-workspace channel chip (#271): shown only while this pane is a live
    // channel member. Clicking it disconnects directly — the "easy close from the
    // indicator itself" requirement, distinct from (but going to the same place
    // as) the pane-menu's Disconnect item.
    this.channelChip = document.createElement("button");
    this.channelChip.className = "pane-channel";
    this.channelChip.hidden = true;
    this.channelChip.addEventListener("click", (e) => {
      e.stopPropagation();
      this.pane.events.onDisconnectChannel(this.pane);
    });
    header.appendChild(this.channelChip);
  }

  /** Mark this pane as part of an orchestration group: role chip before the
   *  title plus a group-colored accent on the header. */
  setBadge(badge: PaneBadge): void {
    const chip = document.createElement("span");
    chip.className = "pane-badge";
    chip.textContent = badge.label;
    if (badge.title) chip.title = badge.title;
    this.pane.el.style.setProperty("--group-color", badge.color);
    this.pane.el.classList.add("grouped");
    this.pane.titleEl.before(chip);
  }

  /** Flag (or clear) this pane as needing the human — driven by the backend
   *  attention scan. Idempotent on (reason, detail) TOGETHER: an identical
   *  repeat of both is a no-op, so the 3-second re-emits don't thrash the DOM.
   *  `null` clears the badge.
   *
   *  Deliberately NOT idempotent on `reason` alone (#1091 slice D review) —
   *  see `attentionChanged` (attention.ts) for why, and for the pinned test.
   *  Still cheap: this only runs when the outer `AttentionGate`
   *  (attentiongate.ts) already decided the payload changed, since ITS
   *  signature includes `detail` too — so a same-text re-emit never even
   *  reaches here, and a changed `detail` was already going to trigger a DOM
   *  pass; this just stops that pass from discarding the new text once it
   *  arrives. */
  setAttention(reason: string | null, detail?: string): void {
    const normalizedDetail = reason ? detail ?? null : null;
    // #2122 slice A2 — ABOVE the change gate, deliberately. That gate exists to
    // stop the 3-second re-emits from thrashing the DOM; it is a rendering
    // optimization, and the at-prompt latch must not inherit its filtering. A
    // reducer fed only the readings that happened to differ from the last one
    // would be reading a different input from the one this method is told.
    this.pane.activity.noteAttention(reason);
    if (!attentionChanged(this.attentionReason, this.attentionDetail, reason, normalizedDetail)) return;
    this.attentionReason = reason;
    this.attentionDetail = normalizedDetail;
    if (!reason) {
      this.attnChip.hidden = true;
      this.pane.el.classList.remove("needs-attention");
      delete this.attnChip.dataset.reason;
    } else {
      const { label } = attentionPresentation(reason);
      this.attnChip.textContent = label;
      this.attnChip.title = detail ?? "This pane needs you";
      this.attnChip.dataset.reason = reason;
      this.attnChip.hidden = false;
      this.pane.el.classList.add("needs-attention");
    }
    // The dismiss control follows the chip (#825 M1) — see `attentionDismiss`
    // for why only the latched `stranded` reason gets one.
    const dismiss = attentionDismiss(reason, this.pane.orchAgent);
    this.attnDismiss.textContent = dismiss.label;
    this.attnDismiss.title = dismiss.title;
    this.attnDismiss.setAttribute("aria-label", dismiss.title);
    this.attnDismiss.hidden = !dismiss.dismissible;
    // A minimized pane's element is detached, so its header chip is invisible;
    // the listener lets the grid mirror this state onto the dock chip.
    this.pane.dockSyncListener?.();
  }

  /** Is the human watching this pane (#3319)? */
  get watched(): boolean {
    return this.isWatched;
  }

  /** Set, or clear, the human's watch on this pane.
   *
   *  THE ONLY WRITER of `isWatched`, and that is the feature's AC5 rather than
   *  a style preference: a watch is cleared by the human and by nothing else.
   *  There is deliberately no call to this from the focus path, the attention
   *  path, the activity tick, or respawn — `test/watchedwiring.test.ts` scans
   *  this file for exactly that and default-denies a new caller, because "no
   *  auto-clear" is the kind of promise that is true when written and quietly
   *  false two slices later, with nothing red to say so.
   *
   *  Returns the new state so a toggle caller can report it without reading
   *  back. Idempotent: setting what is already set touches no DOM and fires no
   *  dock sync, so the once-a-second refresh paths cannot make this churn. */
  setWatched(watched: boolean): boolean {
    if (watched === this.isWatched) return this.isWatched;
    this.isWatched = watched;
    this.watchChip.hidden = !watched;
    this.pane.el.classList.toggle("watched", watched);
    // No explicit header refit, for the same reason `setAttention` needs none:
    // the chip is a header child and the meta box is the header's flex spacer,
    // so showing one moves that box's width and the ResizeObserver on it
    // schedules the overflow pass. That pass measures and toggles classes; it
    // never reaches a PTY resize (constraint 1).
    this.pane.dockSyncListener?.();
    return this.isWatched;
  }

  /** Flip the watch. The chord's and the menu item's one entry point, so the
   *  two gestures cannot drift apart on what "toggle" means. */
  toggleWatched(): boolean {
    return this.setWatched(!this.isWatched);
  }

  /** Flag (or clear) that loomux is currently withholding a prompt delivery
   *  to this pane because it believes the human's own input occupies the
   *  CLI's box (#246) — driven by the backend's paired
   *  orch-delivery-held / orch-delivery-held-cleared events. `null` clears the
   *  badge.
   *
   *  Idempotent on the REASON ALONE — deliberately unlike `setAttention` above,
   *  which keys on `(reason, detail)` together. The two differ because their
   *  details do: an attention detail is free text the scan can change under a
   *  steady reason, while a held detail is `delivery_held_detail(agent_id,
   *  reason)` (orchestration/mod.rs), a total function over a three-variant
   *  enum that interpolates nothing but `agent_id`. So under a steady reason it
   *  cannot move, and the cheaper check loses nothing.
   *
   *  Naming the input that is ASSUMED rather than proven, since this comment
   *  exists because its predecessor over-claimed: that argument holds only
   *  while `agent_id` is constant for the pane, which is a property of the
   *  pane-to-agent binding, not of the function. The events dispatch by
   *  `pty_id` (orchestration.ts), so a pane rebound to a different agent while
   *  held, with the reason unchanged, would take the early return and keep the
   *  previous agent's name. The blast radius is the chip's `title` tooltip and
   *  nothing else — no state, no delivery — which is why the cheap check still
   *  wins; if a rebind ever needs to repaint it, key on `(reason, detail)` like
   *  `setAttention` rather than reaching for a rebind hook here. Copy `setAttention`'s rule
   *  here only if that stops being true; copy this one to a NEW badge only
   *  after checking its detail the same way (`setQueueDepth` below is the
   *  other outcome — it keys on the whole reading).
   *
   *  Header chrome only: this never touches the pane's size, so the
   *  no-PTY-resize invariant holds trivially. */
  setHeld(reason: string | null, detail?: string): void {
    if (reason === this.heldReason) return;
    this.heldReason = reason;
    if (!reason) {
      this.heldChip.hidden = true;
      delete this.heldChip.dataset.reason;
    } else {
      const { label } = heldPresentation(reason);
      this.heldChip.textContent = label;
      this.heldChip.title = detail ?? "Prompt delivery paused — human input occupies this pane's box";
      this.heldChip.dataset.reason = reason;
      this.heldChip.hidden = false;
    }
  }

  /** Show (or clear) how deep this pane's delivery queue is (#814): the count,
   *  the cap and how long the oldest queued prompt has been waiting, plus a
   *  stalled cue once nothing has moved for the backend's stall threshold.
   *  `null` clears it — which is how a drained queue is reported, since the
   *  backend pushes the full set of panes that HAVE a queue and says nothing
   *  about the rest.
   *
   *  Idempotent on the whole reading, not just the depth: this arrives on a 3 s
   *  tick, so re-writing identical text would churn the DOM once per tick per
   *  queued pane for as long as a pane is held. Header chrome only — never
   *  touches the pane's size. */
  setQueueDepth(reading: QueueDepthReading | null): void {
    const same =
      reading === null
        ? this.queueReading === null
        : this.queueReading !== null &&
          this.queueReading.depth === reading.depth &&
          this.queueReading.cap === reading.cap &&
          this.queueReading.waiting_ms === reading.waiting_ms &&
          this.queueReading.stalled === reading.stalled &&
          this.queueReading.agent_id === reading.agent_id;
    if (same) return;
    this.queueReading = reading;
    if (!reading) {
      this.queueChip.hidden = true;
      this.queueChip.textContent = "";
      delete this.queueChip.dataset.stalled;
    } else {
      const { label, title, stalled } = queuePresentation(reading);
      this.queueChip.textContent = label;
      this.queueChip.title = title;
      if (stalled) this.queueChip.dataset.stalled = "true";
      else delete this.queueChip.dataset.stalled;
      this.queueChip.hidden = false;
    }
    // A minimized pane's header is detached, so the chip above is invisible for
    // exactly the panes that queue the most (delegate roles open minimized) —
    // the grid mirrors this onto the dock chip. Same listener setAttention and
    // setConnected use.
    this.pane.dockSyncListener?.();
  }

  /** This pane's current queue reading, or null. Lets the grid put an equivalent
   *  marker on the dock chip while the pane is minimized (`dockChipQueue`), the
   *  same shape as the `attention` and `channelBadge` getters. */
  get queueDepth(): QueueDepthReading | null {
    return this.queueReading;
  }

  /** Set (or clear) the unread-mail count on a MANAGER pane's header (#1161 M5).
   *
   *  `0` hides the chip, which is how an emptied mailbox is reported: the
   *  backend emits the whole current count on every write, so there is no paired
   *  "cleared" event to miss and a dropped push self-corrects on the next one.
   *
   *  Idempotent on the count, like `setQueueDepth`: the seed read and the push
   *  can both land on one pane in the same breath, and re-writing identical text
   *  would churn the DOM for nothing. Header chrome only — never touches the
   *  pane's size. */
  setMailUnread(unread: number): void {
    // Every caller of THIS method is the push (`orch-mailbox-changed`); the
    // seed goes through `applyMailSeed` below, which defers to it.
    this.mailPushed = true;
    this.renderMailUnread(unread);
  }

  /** Apply the one-shot seed read taken when this pane learned it is the
   *  manager — but only while no push has arrived. See `mailPushed`. */
  applyMailSeed(unread: number): void {
    if (this.mailPushed) return;
    this.renderMailUnread(unread);
  }

  private renderMailUnread(unread: number): void {
    const p = mailboxPresentation(unread);
    const next = p ? unread : 0;
    if (next === this.mailUnread) return;
    this.mailUnread = next;
    if (!p) {
      this.mailChip.hidden = true;
      this.mailChip.textContent = "";
      this.mailChip.title = "";
    } else {
      this.mailChip.textContent = p.label;
      this.mailChip.title = p.title;
      this.mailChip.hidden = false;
    }
    // A minimized pane's header is detached, so the chip above is invisible —
    // the grid mirrors this onto the dock chip. Same listener setAttention,
    // setConnected and setQueueDepth use.
    this.pane.dockSyncListener?.();
  }

  /** This pane's unread-mail count (0 when there is none). Lets the grid put an
   *  equivalent marker on the dock chip while the pane is minimized
   *  (`dockChipMail`), the same shape as the `queueDepth` getter above. */
  get mailUnreadCount(): number {
    return this.mailUnread;
  }

  /** Hand this pane its prompt-cache reading (#3407), or `null` when the strip
   *  does not cover it. Called on every strip delivery, which is also what
   *  advances the label against the clock — so the chip needs no timer of its
   *  own. Idempotent on the rendered text: re-writing identical text would churn
   *  the DOM once per delivery for nothing. Header chrome only — never touches
   *  the pane's size. */
  noteCacheAge(reading: CacheAgeReading | null, nowMs: number = Date.now()): void {
    this.cacheReading = reading;
    const label = reading === null ? null : cacheChipLabel(reading, nowMs);
    if (label === null || reading === null) {
      if (!this.cacheChip.hidden) {
        this.cacheChip.hidden = true;
        this.cacheChip.textContent = "";
        this.cacheChip.title = "";
        delete this.cacheChip.dataset.state;
      }
      return;
    }
    const state = cacheState(reading, nowMs).state;
    if (this.cacheChip.textContent !== label) this.cacheChip.textContent = label;
    if (this.cacheChip.dataset.state !== state) this.cacheChip.dataset.state = state;
    const title = cacheChipTitle(reading, nowMs);
    if (this.cacheChip.title !== title) this.cacheChip.title = title;
    if (this.cacheChip.hidden) this.cacheChip.hidden = false;
  }

  /** The cache chip's menu: what the last wake cost (read-only rows) and
   *  "Compact now", which asks the backend to type `/compact` at the pane's next
   *  idle moment through the same path an agent's own `request_compact` takes.
   *  Disabled, with the reason, where it cannot do anything — never offered as a
   *  click that silently fails. */
  private openCacheMenu(x: number, y: number): void {
    const reading = this.cacheReading;
    if (reading === null) return;
    type CacheAction = "compact";
    const items: MenuItem<CacheAction>[] = [];
    const wake = wakeCostLine(reading.lastWake);
    items.push({
      label: wake ?? "No wake recorded yet",
      disabled: true,
      reason: "Read-only: tokens the first request after the last quiet stretch read from the cache, wrote to it, and sent uncached.",
    });
    items.push({ label: "", separator: true });
    const group = this.pane.orchGroup;
    const agent = this.pane.orchAgent;
    const why =
      group === null || agent === null
        ? "Only an orchestration agent pane can be compacted from here."
        : !reading.compactSupported
          ? "This pane's CLI has no /compact that orrerix can type."
          : null;
    items.push(
      why === null ? { label: "Compact now", action: "compact" } : { label: "Compact now", disabled: true, reason: why }
    );
    showContextMenu(x, y, items, (action) => {
      if (action !== "compact" || group === null || agent === null) return;
      requestCompact(group, agent)
        .then((msg) => showToast(`Compact ${msg}`, "info"))
        .catch((err) => showToast(`Compact refused: ${String(err)}`));
    });
  }

  /** Mark (or clear) this pane's cross-workspace channel membership (#271): a
   *  colored/numbered chip before the title plus a `--connect-color` accent, so
   *  panes on either end of a channel — and a third pane joined into it — read as
   *  one connected set even across tabs. `null` clears it (disconnect/teardown).
   *
   *  #271 W3 addendum, part C: the chip also carries a DIRECTION arrow — ▲
   *  (outward) for the sender, ▼ (inward) for a receiver — and a distinct
   *  `receive-only` CSS variant for a delivery-only member (no token, ever),
   *  so the direction and the honest capability both read at a glance. */
  setConnected(info: PaneChannelBadge | null): void {
    this.channelInfo = info;
    if (!info) {
      this.channelChip.hidden = true;
      this.pane.el.classList.remove("connected");
      this.pane.el.style.removeProperty("--connect-color");
      delete this.channelChip.dataset.channel;
      delete this.channelChip.dataset.direction;
      this.channelChip.classList.remove("receive-only");
    } else {
      const arrow = info.direction === "sender" ? "▲" : "▼";
      this.channelChip.textContent = `${arrow} ${info.label}`;
      const capability = info.deliveryOnly
        ? "receive-only — it has no channel token"
        : info.direction === "sender"
          ? "you are the SENDER — you may message anyone connected, any time"
          : info.canSend
            ? "you are a RECEIVER with a reply credit — you may answer the sender now"
            : "you are a RECEIVER — you may answer once the sender messages you";
      this.channelChip.title = `Channel ${info.channelId} — connected to ${
        info.peers.join(", ") || "…"
      } (${capability}). Click to disconnect.`;
      this.channelChip.dataset.channel = info.channelId;
      this.channelChip.dataset.direction = info.direction;
      this.channelChip.classList.toggle("receive-only", info.deliveryOnly);
      this.channelChip.hidden = false;
      this.pane.el.classList.add("connected");
      this.pane.el.style.setProperty("--connect-color", info.color);
    }
    // A minimized pane's element is detached; mirror to the dock chip (#95r's
    // precedent, same listener setAttention/setName already use).
    this.pane.dockSyncListener?.();
  }

  /** The channel this pane currently belongs to, or null — panemenu.ts's
   *  `PaneConnectState.channelId` reads this to decide Connect vs. Disconnect. */
  get channelId(): string | null {
    return this.channelInfo?.channelId ?? null;
  }

  /** Current channel badge, or null — lets the grid render an equivalent
   *  indicator on the dock chip while this pane is minimized, mirroring the
   *  `attention` getter just above. */
  get channelBadge(): PaneChannelBadge | null {
    return this.channelInfo;
  }

  /** Toggle the "armed connect source" visual (#271's "visible pending state"
   *  requirement) — the persistent cue that THIS pane is what a right-click
   *  elsewhere will complete a channel against, until it's completed or
   *  cancelled (self-click or Esc). Never touches the channel chip itself: a
   *  free pane being armed has no channel yet. */
  setPendingConnect(pending: boolean): void {
    this.pane.el.classList.toggle("connect-pending", pending);
  }

  /** Current needs-attention state, or null. Lets the grid render an equivalent
   *  badge on the dock chip while this pane is minimized (its header is out of
   *  the DOM). */
  get attention(): { reason: string; label: string; urgent: boolean; detail: string | null } | null {
    if (!this.attentionReason) return null;
    const { label, urgent } = attentionPresentation(this.attentionReason);
    return { reason: this.attentionReason, label, urgent, detail: this.attentionDetail };
  }

  /** The human is now on this pane: acknowledge its attention backend-side so
   *  the badge drops and (for `waiting`) stays down until the prompt changes.
   *  Agent panes ack by agent id; a plain pane (no agent identity) acks by its
   *  pty id (#40). Public so restoring a docked pane clears it the same way
   *  turning to a pane does. */
  acknowledgeAttention(): void {
    if (!this.attentionReason) return;
    if (this.pane.orchAgent) {
      invoke("orch_ack_attention", { agentId: this.pane.orchAgent }).catch(() => {});
    } else if (this.pane.ptyId !== null) {
      invoke("orch_ack_attention_pty", { ptyId: this.pane.ptyId }).catch(() => {});
    }
  }

  /** The human deliberately dismissed this pane's stuck-prompt chip (#825 M1):
   *  release the latched backend badge, which — unlike the focus ack above —
   *  is an unambiguous "I have seen this", so it is allowed to clear a warning
   *  that no reading of the pane could.
   *
   *  The chip goes down locally first: the backend clear is what makes it STAY
   *  down, but a chip that lingered for a tick after a deliberate click is
   *  precisely the "clearing alerts is unreliable" feel this gesture exists to
   *  fix.
   *
   *  A failed call therefore has to put it back **here**, and cannot be left to
   *  the next attention scan. The tempting version of this comment — "if the
   *  call fails the next tick re-badges us" — is false: `applyAttention`'s gate
   *  (#743 S5) skips the whole pass while the backend payload is unchanged, and
   *  a dismissal the backend never applied is exactly the case where it does not
   *  change. The chip would stay down on a pane still wedged, which is the one
   *  outcome this feature must never produce. */
  private dismissAttention(): void {
    const agentId = this.pane.orchAgent;
    const reason = this.attentionReason;
    if (!agentId || reason !== "stranded") return;
    const detail = this.attentionDetail ?? undefined;
    this.setAttention(null);
    dismissStranded(agentId).catch(() => this.setAttention(reason, detail));
  }

  /** Show (or, with `null`, hide) the fork crumb (#3368). `go` null leaves it
   *  visible but inert — a parent nothing has a record of any more is still
   *  worth naming, and a button that did nothing when clicked would be worse
   *  than one that says it cannot. Writes the DOM only when something changed:
   *  this runs on every grid change and every sessions-log change. */
  setForkCrumb(crumb: { label: string; title: string; go: (() => void) | null } | null): void {
    this.forkCrumbGo = crumb?.go ?? null;
    const hidden = crumb === null;
    if (this.forkCrumb.hidden !== hidden) this.forkCrumb.hidden = hidden;
    if (!crumb) return;
    if (this.forkCrumb.textContent !== crumb.label) this.forkCrumb.textContent = crumb.label;
    if (this.forkCrumb.title !== crumb.title) this.forkCrumb.title = crumb.title;
    this.forkCrumb.disabled = crumb.go === null;
    this.forkCrumb.setAttribute("aria-label", crumb.title);
  }
}
