// HarnessEvent -> DOM. The renderer half of the mock.
//
// WHAT THIS MOCK IS ARGUING, and the one place it departs from the contract as
// merged. doc/design/harness-adapters.md §5.1 renders a structured pane by
// turning each event into VT bytes and pushing them into the pane's existing
// OutputBuf ring, so get_output, termgrid replay, thumbnails and last_exit_tail
// keep working with no API change — and it rejects "a DOM transcript view
// beside the terminal" precisely because that breaks all five.
//
// #2891 then raised the bar above that floor: "a designed surface (typography,
// spacing, colour tokens from the theme system), not an xterm emulation of a
// chat log". A VT renderer cannot draw a collapsible card, a fold animation, or
// a button. Those two cannot both be fully true, and RESOLVING IT IS S1a's, not
// this mock's: S1a owns the contract. This file exists to show the human what
// the raised bar buys, so the choice is made against a picture rather than a
// paragraph. DESIGN.md §The one open question states the three ways out.
//
// What the mock does NOT get to hand-wave, and honours anyway:
//  - The transcript is a VIEW OVER THE EVENT LOG, never the only copy. Every
//    block below is derived from an event, and `projectText()` at the bottom is
//    the text projection that keeps the ring, thumbnails and replay fed. That
//    projection is the tested, DOM-free module S2 owns.
//  - The renderer never rewrites bytes it has already emitted (§5.2). Blocks
//    APPEND; a delta extends a text node. Nothing re-lays-out history on a
//    width change, and there is no code path here that could resize anything.

import { icon, toolMark } from "./icons.js";

const MAX_ROWS = 400;      // the ring, made visible — see the storm fixture
const OUT_CAP = 64 * 1024; // per-card output ceiling before elision

export class PaneView {
  constructor(root) {
    this.root = root;
    this.scroll = root.querySelector(".scroll");
    this.list = root.querySelector(".transcript");
    this.pinned = true; // follow the live end unless the human scrolled away

    this.tools = new Map();   // toolCallId -> { el, out, bytes, elided }
    this.asks = new Map();    // requestId  -> el
    this.say = null;          // the open assistant paragraph, if any
    this.think = null;        // the open thinking block, if any
    this.thinkStart = 0;
    this.rows = 0;
    this.dropped = 0;
    this.log = [];            // the event log this view is a projection of

    this.scroll.addEventListener("scroll", () => {
      const gap = this.scroll.scrollHeight - this.scroll.scrollTop - this.scroll.clientHeight;
      this.pinned = gap < 24;
    });
  }

  // ---------------------------------------------------------------- plumbing

  /** Append a row. The gutter segment is the row's `seg`; `live` marks the
   *  one row currently receiving bytes, which is what the caret follows. */
  row(seg, live = false) {
    const el = document.createElement("div");
    el.className = "row";
    el.dataset.seg = seg;
    if (live) el.dataset.live = "1";
    this.list.appendChild(el);
    this.rows++;
    this.trim();
    this.follow();
    return el;
  }

  /** The ring. Oldest rows are dropped, and the drop is STATED rather than
   *  hidden — an elision the reader cannot see is a transcript that lies.
   *
   *  The notice node is CACHED rather than re-queried. Under the storm fixture
   *  this runs on nearly every row, and a `querySelector` per dropped row is
   *  the same shape as the layout thrash `follow()` documents below. */
  trim() {
    while (this.rows > MAX_ROWS) {
      let victim = this.list.firstElementChild;
      if (victim === this.ringEl) victim = victim.nextElementSibling; // never the notice
      if (!victim) break;
      victim.remove();
      this.rows--;
      this.dropped++;
    }
    if (this.dropped > 0) this.ringNote();
  }

  ringNote() {
    if (!this.ringEl) {
      const n = document.createElement("div");
      n.className = "row ringnote";
      n.dataset.seg = "idle";
      n.innerHTML = `<div class="note" data-kind="info"><span class="tag">[ring]</span><span></span></div>`;
      this.list.prepend(n);
      this.ringEl = n;
      this.ringText = n.querySelector(".note span:last-child");
    }
    this.ringText.textContent =
      `${this.dropped.toLocaleString()} earlier rows rolled out of the pane buffer — the full transcript is on disk in the event log`;
  }

  /**
   * Follow the live end — COALESCED TO ONE SCROLL PER FRAME.
   *
   * Reading `scrollHeight` forces a synchronous layout, so doing it once per
   * appended row is O(n^2) in the size of the burst: the storm fixture's 460
   * tool calls froze the tab outright until this was scheduled instead of run
   * inline. That is the same argument §5.3's coalescer makes about the output
   * ring one level down — a producer may not make the consumer pay per event —
   * and it is why the storm fixture exists rather than being a nice-to-have.
   */
  follow() {
    if (!this.pinned || this.followQueued) return;
    this.followQueued = true;
    requestAnimationFrame(() => {
      this.followQueued = false;
      if (this.pinned) this.scroll.scrollTop = this.scroll.scrollHeight;
    });
  }

  /** Close whatever streaming block is open. A block is closed by the arrival
   *  of a different kind of event, which is how a stream signals a boundary. */
  seal() {
    if (this.say) { this.say.el.querySelector(".caret")?.remove(); this.say.row.removeAttribute("data-live"); this.say = null; }
    if (this.think) {
      this.think.row.removeAttribute("data-live");
      const secs = (this.now - this.thinkStart) / 1000;
      this.think.dur.textContent = secs >= 0.05 ? `${secs.toFixed(1)}s` : "";
      // Thinking collapses once it is done: it is the one block the eye should
      // be able to skip, and leaving every one of them open buries the answer.
      this.think.box.dataset.open = "0";
      this.think = null;
    }
  }

  // ------------------------------------------------------------------ events

  apply(ev, nowMs) {
    this.now = nowMs;
    this.log.push(ev);

    switch (ev.k) {
      case "Booted":       return this.onBooted(ev);
      case "TurnStarted":  return this.onTurnStarted(ev);
      case "Text":         return this.onText(ev);
      case "Thinking":     return this.onThinking(ev);
      case "ToolCall":     return this.onToolCall(ev);
      case "ToolRunning":  return this.onToolRunning(ev);
      case "ToolOutput":   return this.onToolOutput(ev);
      case "ToolResult":   return this.onToolResult(ev);
      case "UiRequest":    return this.onUiRequest(ev);
      case "UiSettled":    return this.onUiSettled(ev);
      // A permission card MUST say what it is permitting. §3.2's whole gain
      // over the argv path is that the prompt sees the ACTUAL arguments — a
      // policy can allow `Bash(git status)` and refuse `Bash(git push)` — so a
      // card showing only the tool name throws away the one fact the human is
      // being asked about, and trains them to click Allow without reading.
      case "PermissionRequest":
        return this.onUiRequest({
          ...ev,
          method: "permission",
          title: `Allow ${ev.tool}?`,
          message: argLine(ev.tool, ev.input) || JSON.stringify(ev.input, null, 1),
          options: [],
        });
      case "PermissionSettled": return this.onUiSettled({ ...ev, answer: { t: "Confirmed", v: ev.decision === "allow" } });
      case "QueueChanged": return this.onQueue(ev);
      case "TurnEnded":    return this.onTurnEnded(ev);
      case "Compacted":    return this.onCompacted(ev);
      case "Delivery":     return this.onDelivery(ev);
      case "Note":         return this.onNote(ev);
      case "UsageTick":    return this.onUsage(ev);
      case "Settled":      return this.setState("idle");
      case "Exited":       return this.onExited(ev);
      default:             return;
    }
  }

  setState(s) { this.root.dataset.state = s; }

  onBooted(ev) {
    this.seal();
    const h = this.root.querySelector(".phead");
    h.querySelector("[data-f=model]").textContent = ev.model ?? "—";
    h.querySelector("[data-f=think]").textContent = ev.thinking ?? "—";
    // "Unknown is not a value": a session id we do not have yet renders as an
    // em dash, never as the string "unknown".
    h.querySelector("[data-f=session]").textContent = ev.session ? ev.session.slice(0, 8) : "—";
    this.setState("idle");
  }

  onTurnStarted(ev) {
    this.seal();
    this.setState("working");
    const r = this.row("working");
    r.innerHTML = `<div class="turnrule"><span>turn ${ev.turn}</span></div>`;
  }

  onText(ev) {
    if (this.think) this.seal();
    if (!this.say) {
      const row = this.row("working", true);
      const el = document.createElement("div");
      el.className = "say";
      const txt = document.createTextNode("");
      el.append(txt, caretEl());
      row.appendChild(el);
      this.say = { row, el, txt };
    }
    // Append to the existing text node. The renderer never rewrites bytes it
    // has already emitted (§5.2) — a delta extends, it does not re-render.
    this.say.txt.appendData(ev.delta);
    this.setState("working");
    this.follow();
  }

  onThinking(ev) {
    if (this.say) this.seal();
    if (!this.think) {
      this.thinkStart = this.now;
      const row = this.row("thinking", true);
      const box = document.createElement("div");
      box.className = "think";
      box.dataset.open = "1";
      box.innerHTML =
        `<button class="think-head" type="button" aria-expanded="true">` +
          `${icon("chevron", "chev")}` +
          `${icon("spiral", "")}` +
          `<span class="think-word">thinking</span>` +
          `<span class="think-dur num"></span>` +
        `</button>` +
        `<div class="fold"><div><div class="think-body"></div></div></div>`;
      const head = box.querySelector(".think-head");
      const body = box.querySelector(".think-body");
      head.addEventListener("click", () => toggle(box, head));
      row.appendChild(box);
      this.think = { row, box, body, dur: box.querySelector(".think-dur"), txt: document.createTextNode("") };
      body.appendChild(this.think.txt);
    }
    this.think.txt.appendData(ev.delta);
    this.setState("working");
    this.follow();
  }

  onToolCall(ev) {
    this.seal();
    const { mark, family } = toolMark(ev.name);
    const isShell = ev.name === "Bash";
    const row = this.row("working", true);

    const el = document.createElement("div");
    el.dataset.status = "pending";
    el.dataset.open = "0";

    if (isShell) {
      // A command reads differently from a tool: the command line first, the
      // exit status second, the output only if the status says to look.
      el.className = "cmd";
      el.innerHTML =
        `<button class="cmd-head" type="button" aria-expanded="false">` +
          `<span class="cmd-sigil">$</span>` +
          `<span class="cmd-line"></span>` +
          `<span class="cmd-meta">` +
            `<span class="tool-dur num"></span>` +
            `<span class="chip" data-status="pending"><span class="dotmark"></span>queued</span>` +
            `${icon("chevron", "chev")}` +
          `</span>` +
        `</button>` +
        `<div class="fold"><div><pre class="out"></pre></div></div>`;
      el.querySelector(".cmd-line").textContent = String(ev.input.command ?? "");
    } else {
      el.className = "tool";
      if (family) el.dataset.family = family;
      el.innerHTML =
        `<button class="tool-head" type="button" aria-expanded="false">` +
          `${icon(mark, "tool-ic")}` +
          `<span class="tool-name"></span>` +
          `<span class="tool-arg"><span></span></span>` +
          `<span class="tool-dur num"></span>` +
          `<span class="chip" data-status="pending"><span class="dotmark"></span>queued</span>` +
          `${icon("chevron", "chev")}` +
        `</button>` +
        `<div class="fold"><div class="tool-body"></div></div>`;
      el.querySelector(".tool-name").textContent = ev.name;
      el.querySelector(".tool-arg > span").textContent = argLine(ev.name, ev.input);
      const body = el.querySelector(".tool-body");
      body.appendChild(kvOf(ev.input));
      const pre = document.createElement("pre");
      pre.className = "out";
      pre.hidden = true;
      body.appendChild(pre);
    }

    const head = el.querySelector(".cmd-head, .tool-head");
    head.addEventListener("click", () => toggle(el, head));
    row.appendChild(el);

    const out = el.querySelector(".out");
    this.tools.set(ev.id, { el, row, out, bytes: 0, elided: 0, isShell, name: ev.name, input: ev.input });
    this.setState("working");
  }

  onToolRunning(ev) {
    const t = this.tools.get(ev.id);
    if (!t) return;
    t.started = this.now;
    setChip(t.el, "running", "running");
  }

  onToolOutput(ev) {
    const t = this.tools.get(ev.id);
    if (!t || !ev.delta) return;
    t.out.hidden = false;
    if (ev.is_error) t.out.dataset.error = "1";

    // The per-card output ceiling. Past it, the head of the output is dropped
    // and the drop is stated — the same stance as the row ring above, one
    // level down. This is what the storm fixture is for.
    t.bytes += ev.delta.length;
    t.out.appendChild(document.createTextNode(ev.delta));
    if (t.bytes > OUT_CAP) {
      const keep = t.out.textContent.slice(-OUT_CAP);
      t.elided += t.bytes - keep.length;
      t.bytes = keep.length;
      t.out.textContent = keep;
      let n = t.el.querySelector(".elided");
      if (!n) {
        n = document.createElement("div");
        n.className = "elided";
        t.out.parentElement.insertBefore(n, t.out);
      }
      n.textContent = `${t.elided.toLocaleString()} bytes elided from the head of this output`;
    }
    // Live output auto-opens the card: a command producing bytes right now is
    // the thing the human is watching, and making them click to see it is the
    // whole failure the structured pane exists to fix.
    if (t.el.dataset.open === "0" && t.el.dataset.status === "running") {
      t.el.dataset.open = "1";
      t.el.querySelector(".cmd-head, .tool-head")?.setAttribute("aria-expanded", "true");
    }
    this.follow();
  }

  onToolResult(ev) {
    const t = this.tools.get(ev.id);
    if (!t) return;
    const ms = ev.ms ?? (t.started ? this.now - t.started : null);
    if (ms != null) t.el.querySelector(".tool-dur").textContent = fmtMs(ms);

    if (t.isShell) {
      const code = ev.meta && typeof ev.meta.exitCode === "number" ? ev.meta.exitCode : (ev.ok ? 0 : 1);
      setChip(t.el, ev.ok ? "ok" : "error", `exit ${code}`);
    } else {
      setChip(t.el, ev.ok ? "ok" : "error", ev.ok ? "ok" : "error");
    }

    // An Edit's real payload is a diff, not a JSON blob. Rendering it as one is
    // the difference between "a tool ran" and "here is what changed".
    if (ev.ok && isEditTool(t.name)) {
      const d = diffOf(t.input);
      if (d) {
        const body = t.el.querySelector(".tool-body");
        body.querySelector(".kv")?.remove();
        body.prepend(d);
      }
    }

    t.el.dataset.status = ev.ok ? "ok" : "error";
    t.row.dataset.seg = ev.ok ? "ok" : "danger";
    t.row.removeAttribute("data-live");

    // A failure opens itself. The human should never have to click to find out
    // why something broke.
    if (!ev.ok) {
      t.el.dataset.open = "1";
      t.el.querySelector(".cmd-head, .tool-head")?.setAttribute("aria-expanded", "true");
      this.setState("danger");
    } else if (t.el.dataset.open === "1" && t.isShell) {
      t.el.dataset.open = "0";
      t.el.querySelector(".cmd-head")?.setAttribute("aria-expanded", "false");
    }
    this.follow();
  }

  onUiRequest(ev) {
    this.seal();
    this.setState("attention");
    const row = this.row("attention", true);
    const el = document.createElement("div");
    el.className = "ask";
    el.dataset.open = "pending";

    const opts = (ev.options && ev.options.length)
      ? ev.options.map((o, i) =>
          `<button class="btn${i === 0 ? " btn-primary" : ""}" type="button" data-v="${escAttr(String(o.value ?? o))}">${esc(String(o.label ?? o))}</button>`).join("")
      : `<button class="btn btn-primary" type="button" data-v="allow">Allow</button>` +
        `<button class="btn btn-danger" type="button" data-v="deny">Deny</button>`;

    el.innerHTML =
      `<div class="ask-head">${icon("shield", "")}<span>needs you</span>` +
        `<span class="grow"></span><span class="ask-method">${esc(ev.method)}</span></div>` +
      `<div class="ask-body">` +
        `<p class="ask-title"></p>` +
        // A permission's payload is a literal the machine handed us — a command
        // line — so it takes the mono face. A dialog's message is PROSE written
        // for the human, and setting prose in mono is the costume the type
        // roles exist to prevent.
        (ev.message ? `<div class="ask-what"${ev.method === "permission" ? "" : ` data-prose="1"`}></div>` : "") +
        `<div class="ask-acts">${opts}` +
          `<span class="ask-hint">${ev.timeout_ms ? `times out in ${Math.round(ev.timeout_ms / 1000)}s` : "waits until answered"}</span>` +
        `</div>` +
      `</div>`;

    el.querySelector(".ask-title").textContent = ev.title ?? "Permission required";
    if (ev.message) el.querySelector(".ask-what").textContent = ev.message;

    // The mock answers locally so the human can feel the transition. In the
    // app the answer travels a trusted command and NO AGENT MAY EVER ANSWER
    // ONE (§3.3) — a settle from this button is the human's, by construction,
    // because a button in the human's own window is the trusted path.
    for (const b of el.querySelectorAll(".ask-acts .btn")) {
      b.addEventListener("click", () => {
        this.apply({ k: "UiSettled", id: ev.id, answer: { t: "Value", v: b.dataset.v }, by: "human", label: b.textContent }, this.now);
      });
    }

    row.appendChild(el);
    this.asks.set(ev.id, { el, row });
    this.follow();
  }

  onUiSettled(ev) {
    const a = this.asks.get(ev.id);
    if (!a) return;
    const v = ev.answer && ev.answer.t === "Cancelled" ? "cancelled"
            : ev.answer && ev.answer.t === "Confirmed" ? (ev.answer.v ? "allowed" : "denied")
            : String((ev.answer && ev.answer.v) ?? "answered");
    const denied = v === "denied" || v === "deny" || v === "cancelled";

    a.el.dataset.open = "settled";
    a.el.querySelector(".ask-acts").innerHTML =
      `<span class="chip" data-status="${denied ? "denied" : "allowed"}"><span class="dotmark"></span>${esc(ev.label || v)}</span>` +
      `<span class="ask-hint">by ${esc(ev.by || "human")}</span>`;
    a.row.dataset.seg = denied ? "danger" : "ok";
    a.row.removeAttribute("data-live");
    this.asks.delete(ev.id);
    if (!this.asks.size) this.setState("working");
    this.follow();
  }

  onQueue(ev) {
    const n = (ev.steering?.length ?? 0) + (ev.follow_up?.length ?? 0);
    const q = this.root.querySelector(".queue");
    q.hidden = n === 0;
    if (n === 0) return;
    q.querySelector("span").textContent =
      `${n} queued` + (ev.steering?.length ? ` · ${ev.steering.length} steering` : "");
    q.classList.remove("bumped");
    void q.offsetWidth; // restart the one-shot
    q.classList.add("bumped");
  }

  onTurnEnded(ev) {
    this.seal();
    const r = this.row("idle");
    const stop = ev.stop || "end_turn";
    const bits = [`turn ${ev.turn ?? "—"} ended`];
    if (ev.usage) bits.push(`${fmtTok(ev.usage.input)} in · ${fmtTok(ev.usage.output)} out`);
    if (ev.usage?.reasoning) bits.push(`${fmtTok(ev.usage.reasoning)} reasoning`);
    if (ev.cost != null) bits.push(`$${Number(ev.cost).toFixed(4)}`);
    r.innerHTML = `<div class="turnrule" data-stop="${stop === "error" ? "error" : "ok"}">` +
      `<span class="stop">${esc(stop)}</span><span>${esc(bits.join("  ·  "))}</span></div>`;
    this.onUsage(ev);
    this.setState("idle");
  }

  onCompacted(ev) {
    if (ev.phase !== "end") return; // one seam per compaction, drawn when it lands
    this.seal();
    const r = this.row("idle");
    const from = ev.pre_tokens ? fmtTok(ev.pre_tokens) : "—";
    const to = ev.post_tokens ? fmtTok(ev.post_tokens) : "—";
    r.innerHTML = `<div class="compact">${icon("compact", "")}` +
      `<span>context compacted (<span class="esc-trigger"></span>)</span>` +
      `<span class="num">${from} → ${to} tokens</span></div>`;
    r.querySelector(".esc-trigger").textContent = ev.trigger;
  }

  onDelivery(ev) {
    this.seal();
    const r = this.row("idle");
    const el = document.createElement("div");
    el.className = "deliv";
    el.dataset.from = ev.from === "human" ? "human" : "agent";
    el.innerHTML =
      `<div class="deliv-head">${icon(ev.from === "human" ? "human" : "inbound", "")}` +
        `<span class="deliv-from"></span><span></span>` +
        `<span class="grow"></span><span class="deliv-ts"></span></div>` +
      `<div class="deliv-body"></div>`;
    el.querySelector(".deliv-from").textContent = ev.from;
    el.querySelectorAll(".deliv-head span")[1].textContent = ev.kind ? `· ${ev.kind}` : "";
    el.querySelector(".deliv-ts").textContent = ev.ts ?? "";
    el.querySelector(".deliv-body").textContent = ev.text;
    r.appendChild(el);
  }

  onNote(ev) {
    const r = this.row(ev.kind === "error" ? "danger" : "idle");
    const el = document.createElement("div");
    el.className = "note";
    el.dataset.kind = ev.kind || "info";
    el.innerHTML = `<span class="tag"></span><span></span>`;
    el.querySelector(".tag").textContent = `[${ev.tag}]`;
    el.querySelector("span:last-child").textContent = ev.text;
    r.appendChild(el);
  }

  onUsage(ev) {
    const tk = this.root.querySelector(".ticker");
    if (ev.usage) {
      bump(tk.querySelector("[data-f=tin]"), fmtTok(ev.usage.input));
      bump(tk.querySelector("[data-f=tout]"), fmtTok(ev.usage.output));
      bump(tk.querySelector("[data-f=cache]"), fmtTok(ev.usage.cacheRead));
    }
    if (ev.cost != null) bump(tk.querySelector("[data-f=cost]"), `$${Number(ev.cost).toFixed(4)}`);
    if (ev.context) {
      const m = this.root.querySelector(".ctxmeter");
      const pct = Math.max(0, Math.min(1, ev.context.percent ?? 0));
      m.querySelector(".ctxmeter-fill").style.transform = `scaleX(${pct.toFixed(4)})`;
      m.querySelector("[data-f=ctx]").textContent = `${Math.round(pct * 100)}%`;
      m.dataset.pressure = pct >= 0.9 ? "high" : pct >= 0.75 ? "warn" : "ok";
    }
  }

  onExited(ev) {
    this.seal();
    const r = this.row(ev.code === 0 ? "idle" : "danger");
    r.innerHTML = `<div class="turnrule" data-stop="${ev.code === 0 ? "ok" : "error"}">` +
      `<span class="stop">exited</span><span>code ${ev.code ?? "—"}</span></div>`;
    this.setState(ev.code === 0 ? "idle" : "danger");
  }

  /**
   * THE TEXT PROJECTION — the half that keeps §5.1's promise.
   *
   * The DOM above is a view; THIS is the copy that feeds the output ring,
   * get_output, thumbnails and replay-on-attach. It is derived from the same
   * event log, so the two cannot disagree, and it is pure — no DOM read — which
   * is what lets S2 own it as a tested module. Shipped here so the mock cannot
   * be mistaken for an argument that the DOM is the only copy.
   */
  projectText() {
    let buf = "";
    // A DELTA concatenates — that is what a delta is. A structural LINE has to
    // start on a fresh line and end one, and it cannot assume the delta before
    // it ended in a newline. So the two append differently, and conflating them
    // is what makes a projection that reads as one run-on paragraph.
    const raw = (s) => { buf += s; };
    const line = (s) => {
      if (buf && !buf.endsWith("\n")) buf += "\n";
      buf += s + "\n";
    };
    const blank = () => { if (buf && !buf.endsWith("\n\n")) line(""); };

    let thinkOpen = false;
    const closeThink = () => { if (thinkOpen) { thinkOpen = false; line("  ---"); } };

    for (const e of this.log) {
      if (e.k !== "Thinking" && thinkOpen) closeThink();
      switch (e.k) {
        case "Booted":
          line(`[booted] model=${e.model ?? "?"} thinking=${e.thinking ?? "?"} session=${e.session ?? "unbound"}`);
          break;
        case "TurnStarted": blank(); line(`-- turn ${e.turn} --`); break;
        case "Text": raw(e.delta); break;
        case "Thinking":
          // Thinking is kept, but FENCED: a reader of the ring must be able to
          // tell the model's reasoning from its answer, which is the same
          // separation the DOM makes with a dim collapsible block.
          if (!thinkOpen) { thinkOpen = true; blank(); line("  --- thinking"); }
          raw(e.delta);
          break;
        case "ToolCall": line(`  ${e.name}(${argLine(e.name, e.input)})`); break;
        case "ToolResult":
          line(`    -> ${e.ok ? "ok" : "ERROR"}${e.ms != null ? ` ${fmtMs(e.ms)}` : ""}`);
          break;
        case "UiRequest": line(`  [needs you] ${e.title ?? e.method}`); break;
        case "UiSettled": line(`  [settled] ${JSON.stringify(e.answer)} by ${e.by}`); break;
        case "PermissionRequest": line(`  [needs you] allow ${e.tool}: ${argLine(e.tool, e.input)}`); break;
        case "PermissionSettled": line(`  [settled] ${e.decision} by ${e.by}`); break;
        case "Compacted":
          if (e.phase === "end") line(`[compacted] ${e.trigger} ${e.pre_tokens ?? "?"} -> ${e.post_tokens ?? "?"}`);
          break;
        case "Delivery": blank(); line(`[${e.from}] ${e.text}`); blank(); break;
        case "Note": line(`[${e.tag}] ${e.text}`); break;
        case "TurnEnded":
          line(`-- turn ${e.turn} ${e.stop}${e.cost != null ? ` $${Number(e.cost).toFixed(4)}` : ""} --`);
          break;
        case "Exited": line(`[exited] code ${e.code ?? "?"}`); break;
        default: break;
      }
    }
    closeThink();
    return buf;
  }
}

// ------------------------------------------------------------------- helpers

function toggle(box, head) {
  const open = box.dataset.open !== "0";
  box.dataset.open = open ? "0" : "1";
  head.setAttribute("aria-expanded", String(!open));
}

function caretEl() {
  const c = document.createElement("span");
  c.className = "caret";
  return c;
}

function setChip(el, status, label) {
  const chip = el.querySelector(".chip");
  chip.dataset.status = status;
  chip.innerHTML = (status === "running" ? `<span class="spin"></span>` : `<span class="dotmark"></span>`) + esc(label);
}

/**
 * The one line that makes a COLLAPSED card worth reading — so it has to be the
 * argument that IDENTIFIES the call, and which one that is depends on the tool.
 * A search is identified by its pattern and a read by its path; taking
 * whichever key happens to come first in a fixed list renders every Grep in a
 * session as the same word ("src") and throws the identifying half away.
 */
const ARG_KEY = {
  Grep: ["pattern", "path"],
  Glob: ["pattern", "path"],
  Bash: ["command"],
  BashOutput: ["command"],
  WebFetch: ["url"],
  WebSearch: ["query"],
  Task: ["description", "prompt"],
  Agent: ["description", "prompt"],
};
const ARG_FALLBACK = ["file_path", "path", "command", "pattern", "url", "query"];

function argLine(name, input) {
  if (!input || typeof input !== "object") return "";
  for (const key of ARG_KEY[name] || ARG_FALLBACK) {
    if (typeof input[key] === "string" && input[key]) return input[key];
  }
  const keys = Object.keys(input);
  return keys.length ? `${keys[0]}=${JSON.stringify(input[keys[0]]).slice(0, 60)}` : "";
}

function kvOf(input) {
  const dl = document.createElement("dl");
  dl.className = "kv";
  for (const [k, v] of Object.entries(input || {})) {
    const dt = document.createElement("dt");
    dt.textContent = k;
    const dd = document.createElement("dd");
    dd.textContent = typeof v === "string" ? v : JSON.stringify(v, null, 1);
    dl.append(dt, dd);
  }
  if (!dl.children.length) { const dt = document.createElement("dt"); dt.textContent = "(no arguments)"; dl.append(dt, document.createElement("dd")); }
  return dl;
}

function isEditTool(n) { return n === "Edit" || n === "Write" || n === "MultiEdit"; }

/**
 * A minimal line diff for an Edit's old_string/new_string. Deliberately naive —
 * it is a mock, and a real one belongs in the tested projection — but it is a
 * REAL diff of the real arguments, not a picture of one.
 */
function diffOf(input) {
  const a = String(input.old_string ?? "").split("\n");
  const b = String(input.new_string ?? "").split("\n");
  if (!input.old_string && !input.new_string) return null;

  const wrap = document.createElement("div");
  wrap.className = "diff";
  const hunk = document.createElement("div");
  hunk.className = "diff-hunk";
  hunk.textContent = `@@ ${input.file_path ?? ""} @@`;
  wrap.appendChild(hunk);

  // Common prefix/suffix, then the changed middle — enough to read an edit.
  let p = 0;
  while (p < a.length && p < b.length && a[p] === b[p]) p++;
  let s = 0;
  while (s < a.length - p && s < b.length - p && a[a.length - 1 - s] === b[b.length - 1 - s]) s++;

  const line = (k, sg, n, text) => {
    const d = document.createElement("div");
    d.className = "diff-line";
    if (k) d.dataset.k = k;
    d.innerHTML = `<span class="ln"></span><span class="sg"></span><span class="tx"></span>`;
    d.querySelector(".ln").textContent = n == null ? "" : String(n);
    d.querySelector(".sg").textContent = sg;
    d.querySelector(".tx").textContent = text;
    wrap.appendChild(d);
  };

  for (let i = Math.max(0, p - 2); i < p; i++) line("", " ", i + 1, a[i]);
  for (let i = p; i < a.length - s; i++) line("del", "-", i + 1, a[i]);
  for (let i = p; i < b.length - s; i++) line("add", "+", i + 1, b[i]);
  for (let i = a.length - s; i < Math.min(a.length, a.length - s + 2); i++) line("", " ", i + 1, a[i]);
  return wrap;
}

function bump(el, text) {
  if (!el || el.textContent === text) return;
  el.textContent = text;
  el.classList.remove("bumped");
  void el.offsetWidth;
  el.classList.add("bumped");
}

function fmtMs(ms) {
  if (ms == null) return "";
  return ms < 1000 ? `${Math.round(ms)}ms` : `${(ms / 1000).toFixed(ms < 10000 ? 1 : 0)}s`;
}

function fmtTok(n) {
  if (n == null) return "—";
  if (n < 1000) return String(n);
  if (n < 1e6) return `${(n / 1000).toFixed(n < 10000 ? 1 : 0)}k`;
  return `${(n / 1e6).toFixed(1)}M`;
}

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}
function escAttr(s) { return esc(s); }
