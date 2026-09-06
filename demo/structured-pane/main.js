// Playback and wiring for the structured-pane mock.
//
// The player replays a fixture at its own recorded cadence, so what you are
// watching is timed like work happening rather than like a file being read.
// It is scaffolding: S2 and S4 inherit decode.js and render.js, never this.
//
// COALESCING IS MODELLED, NOT FAKED. §5.3 holds the ring's rules for renderer
// output too — at most one emit per pane per 16 ms. So the player drains on
// animation frames and applies every event whose timestamp has passed in one
// batch, which is what makes the storm fixture stay smooth: its 461 tool calls
// arrive in a burst and become a handful of frames, not 461 layout passes.

import { decode } from "./decode.js";
import { PaneView } from "./render.js";
import { icon } from "./icons.js";

const FIXTURES = {
  session: { file: "fixtures/session.jsonl", label: "a worker taking a delivery" },
  storm: { file: "fixtures/storm.jsonl", label: "storm — heavy output, 461 calls" },
};

class Player {
  constructor(view, onTick) {
    this.view = view;
    this.onTick = onTick;
    this.lines = [];
    this.i = 0;
    this.clock = 0;       // stream time, ms
    this.speed = 1;
    this.running = false;
    this.last = 0;
    this.raf = 0;
  }

  load(lines) {
    this.stop();
    this.lines = lines;
    this.i = 0;
    this.clock = 0;
    this.state = { turn: 0 };
    this.view.list.replaceChildren();
    this.view.tools.clear();
    this.view.asks.clear();
    this.view.say = this.view.think = null;
    this.view.rows = this.view.dropped = 0;
    this.view.ringEl = this.view.ringText = null;
    this.view.log.length = 0;
    this.view.pinned = true;
    this.view.setState("idle");
    this.onTick();
  }

  start() {
    if (this.running || this.i >= this.lines.length) return;
    this.running = true;
    this.last = performance.now();
    this.raf = requestAnimationFrame(this.frame);
    this.onTick();
  }

  stop() {
    this.running = false;
    cancelAnimationFrame(this.raf);
    this.onTick();
  }

  /**
   * Jump straight to a point in the stream, applying everything up to it in
   * one pass. `?at=<seconds>` uses it, which is how you get to the permission
   * card or the compaction seam without watching the whole run — and it is the
   * same batch drain the frame loop does, so what you land on is what playback
   * would have produced.
   */
  seek(ms) {
    while (this.i < this.lines.length && this.lines[this.i].at <= ms) {
      const line = this.lines[this.i++];
      for (const ev of decode(line, this.state)) this.view.apply(ev, this.lines[this.i - 1].at);
    }
    this.clock = ms;
    this.onTick();
  }

  frame = (now) => {
    if (!this.running) return;
    const dt = Math.min(250, now - this.last); // a backgrounded tab must not fast-forward
    this.last = now;
    this.clock += dt * this.speed;

    // Drain every line whose time has come, in ONE batch — the coalescer's
    // shape. A burst of 461 events is one frame's work, not 461 frames.
    let applied = 0;
    while (this.i < this.lines.length && this.lines[this.i].at <= this.clock) {
      const line = this.lines[this.i++];
      for (const ev of decode(line, this.state)) this.view.apply(ev, this.clock);
      if (++applied > 400) break; // never block a frame indefinitely
    }

    this.onTick();
    if (this.i >= this.lines.length) { this.running = false; this.onTick(); return; }
    this.raf = requestAnimationFrame(this.frame);
  };
}

// ------------------------------------------------------------------ bootstrap

const stage = document.querySelector(".stage");
const panes = [...document.querySelectorAll(".pane")].map((p) => new PaneView(p));
const primary = panes[0];

const els = {
  play: document.querySelector("[data-a=play]"),
  restart: document.querySelector("[data-a=restart]"),
  motion: document.querySelector("[data-a=motion]"),
  fixture: document.querySelector("[data-a=fixture]"),
  speeds: [...document.querySelectorAll("[data-speed]")],
  progress: document.querySelector("[data-f=progress]"),
};

const player = new Player(primary, paint);

function paint() {
  els.play.innerHTML = icon(player.running ? "pause" : "play", "") +
    `<span>${player.running ? "Pause" : "Play"}</span>`;
  const pct = player.lines.length ? Math.round((player.i / player.lines.length) * 100) : 0;
  els.progress.textContent = `${pct}%`;
  els.play.disabled = player.lines.length > 0 && player.i >= player.lines.length;
}

async function load(name) {
  const { file } = FIXTURES[name];
  let text;
  try {
    const res = await fetch(file, { cache: "no-store" });
    if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
    text = await res.text();
  } catch (e) {
    // The one failure this page can actually hit, so it gets a real error state
    // that names the problem AND the recovery — not a blank pane.
    primary.list.replaceChildren();
    const row = primary.row("danger");
    row.innerHTML =
      `<div class="note" data-kind="error"><span class="tag">[demo]</span><span></span></div>`;
    row.querySelector("span:last-child").textContent =
      `Could not load ${file} — ${e.message}. This page reads its fixtures with fetch(), which a ` +
      `file:// open blocks. Serve the directory instead: npx vite demo/structured-pane`;
    return;
  }
  const lines = text.trim().split("\n").filter(Boolean).map((l) => JSON.parse(l));
  player.load(lines);
  const at = Number(new URLSearchParams(location.search).get("at"));
  if (Number.isFinite(at) && at > 0) player.seek(at * 1000);
  else player.start();
}
// Exposed so the page can be driven deterministically — for a screenshot, or
// for a reviewer who wants to land on one moment. It is the same API the
// controls use, not a second path into the renderer.
window.pane = { player, panes };

els.play.addEventListener("click", () => (player.running ? player.stop() : player.start()));
els.restart.addEventListener("click", () => load(els.fixture.value));
els.fixture.addEventListener("change", () => load(els.fixture.value));

for (const b of els.speeds) {
  b.addEventListener("click", () => {
    player.speed = Number(b.dataset.speed);
    for (const o of els.speeds) o.setAttribute("aria-pressed", String(o === b));
  });
}

els.motion.addEventListener("click", () => {
  const off = stage.dataset.motion === "off";
  stage.dataset.motion = off ? "on" : "off";
  els.motion.setAttribute("aria-pressed", String(!off));
  els.motion.querySelector("span").textContent = off ? "Motion on" : "Motion off";
});

// The second pane is a still: it is there because the warp only resolves into
// one picture when the panes carrying it are tiled, and a single pane cannot
// show that. It replays the same fixture, offset and paused partway.
(async () => {
  await load("session");
  const still = panes[1];
  try {
    const res = await fetch(FIXTURES.storm.file, { cache: "no-store" });
    if (!res.ok) return;
    const lines = (await res.text()).trim().split("\n").filter(Boolean).map((l) => JSON.parse(l));
    const st = { turn: 0 };
    let clock = 0;
    for (const line of lines) {
      if (line.at > 9000) break;
      clock = line.at;
      for (const ev of decode(line, st)) still.apply(ev, clock);
    }
    still.setState("working");
  } catch { /* the primary pane already reported the load failure */ }
})();

// Keyboard: space toggles playback unless the human is typing.
addEventListener("keydown", (e) => {
  if (e.key !== " " || e.target.closest("input, textarea, button")) return;
  e.preventDefault();
  player.running ? player.stop() : player.start();
});
