// The pane's steering/compose strip, clipboard paths and voice surface, split
// out of pane.ts (#3498 F1): the compose strip and its attachments (#72), the
// OSC 52 copy + clipboard key handling (#65, #370, #402), and the voice-prompt
// indicator (#58).
//
// A satellite of `Pane`: it owns this cluster's DOM and state and reads the rest
// of the pane through `pane`. DOM glue, hand-validated like pane.ts itself; the
// pure decisions it calls live in steer.ts, pasteflow.ts and clipboard.ts.
// Design notes: docs/design/clipboard.md, docs/design/voice.md; layout
// conventions: docs/design/module-layout.md.

import { voiceController, type VoicePhase } from "./voicecontrol";
import { invoke } from "./transport.ts";
import { parseOsc52, writeClipboard, readClipboard } from "./clipboard";
import { keyDisposition } from "./pasteflow";
import { getSettings } from "./settings";
import {
  checkAttachment,
  attachRejectMessage,
  composeSteerText,
  bytesToBase64,
  steerKeyAction,
  steerBoxHeight,
} from "./steer";
import { showToast } from "./toast";
import { isAppShortcut } from "./shortcuts";
import { icon } from "./icons.ts";
import { ICON_BTN_PX } from "./paneviews";
import type { Pane } from "./pane";

// Attach affordance on the steering strip (#72): a paperclip.
const PAPERCLIP_ICON = icon("paperclip", ICON_BTN_PX);
// Voice-prompt push-to-talk button (#58): a microphone.
const MIC_ICON = icon("mic", ICON_BTN_PX);

/** Pull image files out of a paste/drag `DataTransfer`. Returns only entries
 *  the browser tags as images, so a text or mixed paste yields []. */
function imagesFromDataTransfer(dt: DataTransfer | null): File[] {
  if (!dt) return [];
  const out: File[] = [];
  for (const item of Array.from(dt.items)) {
    if (item.kind === "file" && item.type.startsWith("image/")) {
      const f = item.getAsFile();
      if (f) out.push(f);
    }
  }
  return out;
}

export class PaneCompose {
  /** Loomux-owned steering strip docked under orchestrator panes (#43): the
   *  human types here and loomux enqueues it through the same serialized
   *  delivery path as worker reports, so the pane's stdin has one writer. */
  composeInput: HTMLTextAreaElement | null = null;
  private composeStatus: HTMLElement | null = null;
  composeStatusTimer: number | undefined;
  /** Thumbnail-chip row for images pasted/attached into the strip (#72); hidden
   *  until the first image is queued. */
  private composeChips: HTMLElement | null = null;
  /** Images queued for the next steer, in send order. `path` is the on-disk
   *  scratch file (from `orch_save_attachment`); `url` is a blob: object URL for
   *  the chip thumbnail and must be revoked when the chip goes away. */
  private attachments: { path: string; url: string; name: string }[] = [];
  /** Voice-prompt push-to-talk button on the steer strip (#58). Only present on
   *  orchestrator panes; the hotkey (Alt+S) works on any pane regardless. */
  private micBtn: HTMLButtonElement | null = null;
  /** Overlay badge shown while a voice capture targets THIS pane's terminal
   *  (#58). Overlay chrome — floats over `.xterm`, never resizes the PTY. */
  voiceIndicator: HTMLElement | null = null;

  constructor(private readonly pane: Pane) {
    // Clipboard integration: a CLI (e.g. claude code) copies by emitting
    // OSC 52. xterm.js doesn't implement it, so without this handler the
    // sequence is dropped — the CLI says "copied" but the system clipboard
    // stays empty (#65). Decode the base64 payload and write it out; ignore
    // read requests (`?`) so we never leak the clipboard back to the process,
    // and refuse an oversized payload rather than balloon memory decoding it.
    this.pane.term.parser.registerOscHandler(52, (payload) => {
      const parsed = parseOsc52(payload);
      if (parsed.ok) {
        void this.copyToClipboard(parsed.text);
      } else if (parsed.reason === "oversize") {
        showToast("Ignored an oversized copy request from the terminal.");
      }
      return true;
    });

    // Let app-level shortcuts pass through xterm untouched; handle clipboard
    // combos here (Ctrl+Shift+C/V always work; plain Ctrl+V additionally
    // pastes when the `pasteOnPlainCtrlV` setting (default on) allows it —
    // #370's review found the unconditional version silently broke vim's
    // VISUAL BLOCK mode and readline's quoted-insert, so it's an opt-out,
    // not a forced default; see settings.ts).
    //
    // Plain Ctrl+C (#402 third live-demo round: copy appeared to work in
    // agent panes but not plain terminal panes) copies too, but ONLY with an
    // active selection — with none, it MUST fall through as the shell's
    // interrupt, unconditionally, since Ctrl+C is the terminal's actual ^C
    // key. There is no pane-kind branch anywhere here: this handler and
    // `keyDisposition` are identical for a plain terminal, an agent pane, or
    // an orchestrator pane — see keyDisposition's own doc comment
    // (pasteflow.ts) for why a prior perceived divergence was never a
    // pane-kind difference in this code at all.
    //
    // preventDefault() on copy/paste is load-bearing, not decoration (#402
    // live-demo finding): returning `false` from attachCustomKeyEventHandler
    // only tells XTERM not to process the key — it does NOT suppress the
    // browser's own native handling of it. Without preventDefault, plain
    // Ctrl+V ALSO triggered the browser's native paste on xterm's focused
    // textarea, which xterm itself listens for (its own "paste" DOM
    // listener) and pastes AGAIN — the double-paste bug. See keyDisposition's
    // own doc comment (pasteflow.ts) for the full mechanism.
    this.pane.term.attachCustomKeyEventHandler((e) => {
      if (e.type !== "keydown") return true;
      if (isAppShortcut(e)) return false;
      const sel = this.pane.term.getSelection();
      switch (keyDisposition(e, getSettings().pasteOnPlainCtrlV, !!sel)) {
        case "copy":
          e.preventDefault();
          if (sel) {
            void this.copyToClipboard(sel);
            this.pane.term.clearSelection();
          }
          return false;
        case "paste":
          e.preventDefault();
          void this.pasteFromClipboard();
          return false;
        case "pass":
          return true;
      }
    });

    // xterm.js binds its OWN native "paste" DOM-event listener directly on
    // its internal textarea/root element (independent of our keydown
    // handling above), so ANY browser-native paste — e.g. the Ctrl+V
    // accelerator on a path this preventDefault doesn't reach — lands there
    // too, double-pasting alongside our own explicit readClipboard()-driven
    // paste (#402 live-demo finding). We own paste entirely now; xterm's
    // native path is never wanted. Capture phase, so this runs BEFORE the
    // event reaches xterm's own listener (bound to a descendant of termEl,
    // in the bubble phase) no matter what triggered it. Our own paste calls
    // are `this.term.paste(text)` — a direct method call that never
    // dispatches a DOM "paste" event — so this can never block a paste WE
    // intended.
    this.pane.termEl.addEventListener(
      "paste",
      (e) => {
        e.preventDefault();
        e.stopPropagation();
      },
      true
    );
  }

  /** Copy `text` to the system clipboard, surfacing a toast if the write fails
   *  outright (locked-down webview) — otherwise a failed OSC 52 copy would
   *  silently no-op and reintroduce the "said copied, clipboard empty" symptom
   *  from #65 with no signal to the user. */
  private async copyToClipboard(text: string): Promise<void> {
    const ok = await writeClipboard(text);
    if (!ok) showToast("Copy failed — click the pane and try again.");
  }

  /** Paste the system clipboard into the terminal (#370): Ctrl+V, Ctrl+Shift+V,
   *  and the right-click menu all route here. Never fails silently — a genuine
   *  read failure (readClipboard exhausts its own fallback) surfaces a toast
   *  instead of the prior `.catch(() => {})`, which looked identical to a
   *  keypress that simply did nothing. An empty clipboard is not a failure. */
  private async pasteFromClipboard(): Promise<void> {
    const res = await readClipboard();
    if (!res.ok) {
      // Wording deliberately doesn't assert "blocked" — readClipboard can also
      // fail on a transient condition (a momentary focus loss) that a retry
      // clears on its own, and claiming a permission block for that would be
      // its own small lie (#370 review).
      showToast("Paste failed — click the pane and try again.");
      return;
    }
    if (res.text) {
      this.pane.markFirstInput(); // #440 B2-R: a clipboard paste IS human input, same as a keystroke
      this.pane.term.paste(res.text);
    }
  }

  /** Build the loomux steering strip and dock it under the terminal (#43,
   *  option C). It is a plain DOM textarea — NOT part of xterm — so it never
   *  steals the terminal's keys: keystrokes only reach it while it holds focus
   *  (click or Alt+P). Enter submits; Shift+Enter inserts a newline (the box
   *  wraps and grows, #100); Esc hands focus back to the term. */
  buildComposeStrip(): void {
    const strip = document.createElement("div");
    strip.className = "orch-compose";

    const row = document.createElement("div");
    row.className = "orch-compose-row";
    // The textarea floats inside a fixed-height field: it grows UPWARD over the
    // terminal (see .orch-compose CSS) so a multi-line draft never shrinks the
    // strip's flow footprint — that would resize .pane-term / the PTY (#100).
    const field = document.createElement("div");
    field.className = "orch-compose-field";
    const input = document.createElement("textarea");
    input.className = "dlg-input orch-compose-input";
    // Terse enough to sit on one line at typical pane widths — a long hint here
    // wraps the box to multi-line before the human even types (#163). The full
    // Shift+Enter/Esc rules live in this method's doc comment, not the ghost text.
    input.placeholder = "Steer the orchestrator — Enter sends";
    input.rows = 1;
    input.spellcheck = false;
    input.autocomplete = "off";
    input.addEventListener("keydown", (e) => {
      // Keep this keydown from bubbling to pane/ancestor handlers. (App
      // shortcuts are dispatched capture-phase on `document` and still fire
      // while the strip is focused — but Enter/Esc/plain typing aren't app
      // shortcuts, so the strip handles them normally regardless.)
      e.stopPropagation();
      // Only Enter (send) and Escape (back to terminal) are ours; Shift+Enter,
      // IME-commit Enter, and ordinary typing fall through to the textarea so it
      // inserts a newline and auto-grows. See steerKeyAction for the rules.
      switch (steerKeyAction(e)) {
        case "submit":
          e.preventDefault();
          void this.submitCompose();
          break;
        case "blur":
          e.preventDefault();
          this.pane.focus();
          break;
      }
    });
    // Reflow the box on every content change (typing, newline, paste, cut) so it
    // tracks the draft's line count up to the CSS cap.
    input.addEventListener("input", () => this.growCompose());
    // Ctrl+V of a screenshot: pull image blobs out of the clipboard and queue
    // them as attachments (#72). Text pastes fall through to the input's default
    // handling untouched — we only preventDefault when we actually took images.
    input.addEventListener("paste", (e) => {
      const files = imagesFromDataTransfer(e.clipboardData);
      if (files.length === 0) return;
      e.preventDefault();
      for (const f of files) void this.addAttachment(f, f.name);
    });

    // Attach affordance: a paperclip that opens a native file picker. A hidden
    // <input type=file> keeps the styling ours while reusing the OS dialog.
    const attach = document.createElement("button");
    attach.className = "dlg-btn orch-compose-attach";
    attach.type = "button";
    attach.title = "Attach image(s) — or paste a screenshot with Ctrl+V";
    attach.setAttribute("aria-label", "Attach images");
    attach.innerHTML = PAPERCLIP_ICON;
    const filePicker = document.createElement("input");
    filePicker.type = "file";
    filePicker.accept = "image/*";
    filePicker.multiple = true;
    filePicker.style.display = "none";
    attach.addEventListener("click", (e) => {
      e.stopPropagation();
      filePicker.click();
    });
    filePicker.addEventListener("change", () => {
      const files = filePicker.files ? Array.from(filePicker.files) : [];
      for (const f of files) void this.addAttachment(f, f.name);
      filePicker.value = ""; // allow re-picking the same file next time
    });

    // Voice-prompt push-to-talk (#58): click to record, click again to stop and
    // transcribe locally. Transcript is inserted into the input, NOT submitted —
    // the human reviews it and hits Enter, same as typing.
    const mic = document.createElement("button");
    mic.className = "dlg-btn orch-compose-mic";
    mic.type = "button";
    mic.title = "Voice prompt — click to record, click again to transcribe";
    mic.setAttribute("aria-label", "Record voice prompt");
    mic.innerHTML = MIC_ICON;
    mic.addEventListener("click", (e) => {
      e.stopPropagation();
      voiceController.toggleForCompose(this.pane);
    });

    const send = document.createElement("button");
    send.className = "dlg-btn primary orch-compose-send";
    send.textContent = "Send";
    send.addEventListener("click", (e) => {
      e.stopPropagation();
      void this.submitCompose();
    });
    // #100 wraps the textarea in a fixed-height field so its upward auto-grow
    // never resizes the PTY; #58's mic sits between the paperclip and Send.
    field.appendChild(input);
    row.append(field, attach, filePicker, mic, send);
    this.micBtn = mic;

    // Thumbnail-chip row for queued images (#72). Hidden (via .orch-compose-chips
    // being empty + CSS) until something is queued; kept above the status slot.
    const chips = document.createElement("div");
    chips.className = "orch-compose-chips";

    // Fixed-height slot (see .orch-compose-status): always in layout, so
    // showing/hiding a rejected-send message never changes the strip's height
    // and never resizes .pane-term / the PTY.
    const status = document.createElement("div");
    status.className = "orch-compose-status";

    strip.append(row, chips, status);
    this.composeInput = input;
    this.composeStatus = status;
    this.composeChips = chips;
    this.pane.el.appendChild(strip);
    // Set the box's initial one-line height explicitly (it's attached now), so
    // the baseline matches the field's reserved height before any typing.
    this.growCompose();
  }

  /** Queue one image for the next steer: vet it, base64 it to the backend
   *  scratch dir, and add a thumbnail chip. Refusals (wrong type, oversize, too
   *  many) surface as a toast and are dropped. */
  private async addAttachment(blob: Blob, name: string): Promise<void> {
    if (!this.pane.orchGroup || !this.composeChips) return;
    const check = checkAttachment(blob.type, blob.size, this.attachments.length);
    if (!check.ok) {
      showToast(attachRejectMessage(check.reason, name));
      return;
    }
    try {
      const bytes = new Uint8Array(await blob.arrayBuffer());
      const saved = await invoke<{ path: string; cli: string }>("orch_save_attachment", {
        groupId: this.pane.orchGroup,
        ext: check.ext,
        dataB64: bytesToBase64(bytes),
      });
      this.pane.orchCli = saved.cli; // format references the way this orchestrator's CLI reads them
      // Only mint the thumbnail URL once the file is safely on disk.
      const url = URL.createObjectURL(blob);
      this.attachments.push({ path: saved.path, url, name: name || `image.${check.ext}` });
      this.renderChips();
    } catch (err) {
      showToast(`Attach failed: ${String(err)}`);
    }
  }

  /** Remove a queued attachment by its on-disk path, revoking its thumbnail URL.
   *  The scratch file itself is left for the group-end sweep (the cheap cleanup
   *  policy — no per-image delete round-trip). */
  private removeAttachment(path: string): void {
    const idx = this.attachments.findIndex((a) => a.path === path);
    if (idx < 0) return;
    URL.revokeObjectURL(this.attachments[idx].url);
    this.attachments.splice(idx, 1);
    this.renderChips();
  }

  /** Rebuild the thumbnail-chip row from `this.attachments`. */
  private renderChips(): void {
    const chips = this.composeChips;
    if (!chips) return;
    chips.replaceChildren();
    for (const a of this.attachments) {
      const chip = document.createElement("span");
      chip.className = "orch-compose-chip";
      chip.title = a.name;
      const thumb = document.createElement("img");
      thumb.className = "orch-compose-chip-thumb";
      thumb.src = a.url;
      thumb.alt = a.name;
      const rm = document.createElement("button");
      rm.className = "orch-compose-chip-x";
      rm.type = "button";
      rm.textContent = "✕";
      rm.title = `Remove ${a.name}`;
      rm.setAttribute("aria-label", `Remove ${a.name}`);
      rm.addEventListener("click", (e) => {
        e.stopPropagation();
        this.removeAttachment(a.path);
      });
      chip.append(thumb, rm);
      chips.appendChild(chip);
    }
  }

  /** Drop every queued attachment, revoking thumbnail URLs. Used after a
   *  successful send and on dispose. */
  clearAttachments(): void {
    for (const a of this.attachments) URL.revokeObjectURL(a.url);
    this.attachments = [];
    this.renderChips();
  }

  /** Focus the steering strip (Alt+P). No-op on non-orchestrator panes. */
  focusCompose(): void {
    if (!this.composeInput) return;
    this.composeInput.focus();
    this.composeInput.select();
  }

  /** Auto-grow the steer box to fit its draft, capped at the CSS `max-height`
   *  (a few lines). The box is absolutely positioned and grows upward over the
   *  terminal, so its height changes never touch .pane-term / the PTY (#100).
   *  Past the cap it scrolls internally instead of getting taller. */
  growCompose(): void {
    const t = this.composeInput;
    if (!t) return;
    // Collapse to content height first so the box can also SHRINK (e.g. after a
    // send or a delete), then measure and clamp to the cap.
    t.style.height = "auto";
    const cs = getComputedStyle(t);
    // scrollHeight is content+padding but excludes the border; under border-box
    // the applied height must include it, or the box under-sizes by ~2px and
    // clips the last line. maxHeight (border-box) is the CSS cap.
    const border = (parseFloat(cs.borderTopWidth) || 0) + (parseFloat(cs.borderBottomWidth) || 0);
    const maxPx = parseFloat(cs.maxHeight) || 0;
    const { heightPx, scroll } = steerBoxHeight(t.scrollHeight + border, maxPx);
    t.style.height = `${heightPx}px`;
    t.style.overflowY = scroll ? "auto" : "hidden";
  }

  // ----- VoiceTargetPane (#58): the surface the global voiceController drives.
  // The controller owns the single-capture state machine; a Pane only knows how
  // to receive a transcript and show a recording indicator.

  /** Is this pane's compose box the focused element? Decides caret-insert vs
   *  terminal-paste when the voice hotkey fires. */
  isComposeFocused(): boolean {
    return !!this.composeInput && document.activeElement === this.composeInput;
  }

  /** Reflect the capture phase on this pane's indicator. For a compose target
   *  it's the mic button (pulse while recording, spin while transcribing); for a
   *  terminal target it's a lazily-created overlay badge floating over `.xterm`
   *  (so it never resizes the PTY). */
  setVoicePhase(kind: "compose" | "terminal", phase: VoicePhase): void {
    if (kind === "compose") {
      this.micBtn?.classList.toggle("recording", phase === "recording");
      this.micBtn?.classList.toggle("transcribing", phase === "transcribing");
      return;
    }
    if (phase === "off") {
      this.voiceIndicator?.remove();
      this.voiceIndicator = null;
      return;
    }
    if (!this.voiceIndicator) {
      const badge = document.createElement("div");
      badge.className = "pane-voice-indicator";
      this.pane.termEl.appendChild(badge);
      this.voiceIndicator = badge;
    }
    const recording = phase === "recording";
    this.voiceIndicator.classList.toggle("transcribing", !recording);
    this.voiceIndicator.innerHTML = recording
      ? `<span class="pane-voice-dot"></span>Recording — Alt+S to insert · Esc to cancel`
      : `<span class="pane-voice-spinner"></span>Transcribing… · Esc to cancel`;
  }

  /** Route a transcript into this pane's terminal as if pasted — xterm's paste
   *  path applies bracketed-paste semantics (when the app enabled them) and adds
   *  NO trailing newline, so the human reviews and presses Enter. */
  pasteToTerminal(text: string): void {
    if (this.pane.disposed) return; // pane closed during transcription — drop it
    const t = text.trim();
    if (t) {
      this.pane.markFirstInput(); // #440 B2-R: a dictated transcript is human input too
      this.pane.term.paste(t);
    }
  }

  /** Surface a voice status/error on the strip (compose targets have one). */
  showVoiceStatus(msg: string): void {
    this.showComposeStatus(msg);
  }

  /** Insert transcribed text into the strip at the caret (or append), keeping a
   *  single space between words, then focus the input so the human can edit and
   *  press Enter. Never auto-submits. */
  insertTranscript(text: string): void {
    if (this.pane.disposed) return; // pane closed during transcription — drop it
    const input = this.composeInput;
    if (!input) return;
    const t = text.trim();
    if (!t) return;
    const start = input.selectionStart ?? input.value.length;
    const end = input.selectionEnd ?? input.value.length;
    const before = input.value.slice(0, start);
    const after = input.value.slice(end);
    // Add a separating space only when butting up against existing text.
    const lead = before && !/\s$/.test(before) ? " " : "";
    const trail = after && !/^\s/.test(after) ? " " : "";
    input.value = before + lead + t + trail + after;
    const caret = (before + lead + t).length;
    input.focus();
    input.setSelectionRange(caret, caret);
    // Setting .value programmatically doesn't fire the "input" event that drives
    // the auto-grow (#100), so reflow explicitly — a dictated multi-line prompt
    // must expand the box, not sit clipped at one row until the human types.
    this.growCompose();
  }

  /** Show a transient status line under the strip (errors only — a successful
   *  send is confirmed by the message landing in the terminal above). */
  private showComposeStatus(msg: string): void {
    const status = this.composeStatus;
    if (!status) return;
    status.textContent = msg;
    status.title = msg; // full text if the one-line slot ellipsises it
    status.classList.add("show");
    clearTimeout(this.composeStatusTimer);
    this.composeStatusTimer = window.setTimeout(() => status.classList.remove("show"), 6000);
  }

  /** Enqueue the strip's text to the orchestrator through loomux's serialized
   *  delivery path. Each Enter enqueues one message (rapid sends queue in
   *  arrival order backend-side), so the input stays live rather than locking
   *  while a send is in flight. Clears optimistically; on failure the text is
   *  restored — unless the human has already started a newer draft — so a
   *  rejected message (paused group, dead orchestrator) isn't lost. */
  private async submitCompose(): Promise<void> {
    const input = this.composeInput;
    if (!input || !this.pane.orchGroup) return;
    const draft = input.value;
    // Queued images each become an "Attached image: <path>" line (#72); a
    // message may be images-only (no typed text), so gate on either being
    // present rather than on the text alone.
    const queued = this.attachments;
    const text = composeSteerText(draft, queued.map((a) => a.path), this.pane.orchCli);
    if (!text) return;
    input.value = "";
    this.growCompose(); // collapse the (now empty) box back to one line
    this.attachments = [];
    this.renderChips();
    this.composeStatus?.classList.remove("show");
    try {
      await invoke("orch_steer", { groupId: this.pane.orchGroup, text });
      // Sent: the scratch files have served their purpose (the agent reads them
      // by path); drop only the thumbnail URLs. The files are swept on group end.
      for (const a of queued) URL.revokeObjectURL(a.url);
    } catch (err) {
      // Restore the draft and re-queue the images so a rejected send (paused
      // group, dead orchestrator) isn't lost — unless the human already started
      // a newer draft, which we must not clobber.
      if (input.value === "") {
        input.value = draft;
        this.growCompose(); // regrow to fit the restored draft
      }
      if (this.attachments.length === 0) {
        this.attachments = queued;
        this.renderChips();
      } else {
        for (const a of queued) URL.revokeObjectURL(a.url); // superseded; free them
      }
      this.showComposeStatus(`Not sent: ${String(err)}`);
    }
  }
}
