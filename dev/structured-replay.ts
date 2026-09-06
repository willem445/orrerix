// FIXTURE REPLAY for the structured pane (#2891 S4).
//
// WHY THIS EXISTS. `structuredrows.ts` is DOM-free and covered by
// `test/structuredrows.test.ts`; the DOM wiring in `structuredpane.ts` is
// validated BY HAND, which is this repo's rule ("DOM wiring is validated by
// hand — don't simulate a DOM in tests"). Validating it by hand needs something
// to look at, and until S3b (#2850) emits `orch-pane-event` there is no live
// producer at all. So this page drives the REAL view — the same class the pane
// hosts, imported from `src/`, not a copy — off two sources:
//
//   1. `test/fixtures/structuredview/session.harness.jsonl`, the same fixture
//      the S2 projection tests use. One transcript, both layers, so a
//      divergence between what the tests assert and what the pane draws is
//      visible rather than theoretical.
//   2. A synthetic STORM: 2500 blocks in one burst, past `MAX_BLOCKS`. That is
//      the claim this slice is FOR — thousands of blocks must not cost
//      thousands of nodes — so the page counts the live nodes and prints them
//      beside the block count. The mock's own storm fixture is what found the
//      O(n²) follow-the-live-end defect (`demo/structured-pane/DESIGN.md` §7)
//      by being run rather than read; this is that instrument, one layer up.
//
// It is a DEV PAGE. `vite build` bundles `index.html` only, so nothing here
// reaches `dist/`, and it is outside `src/` so `test/theme.test.ts`'s scans and
// `test/perfpolicy.test.ts`'s listener census do not see it either.
//
// How to open it:
//
//     npm ci                                     # once per worktree
//     npx vite --port 1421 --open /dev/structured-replay.html
//
// `--port` is required: the repo-root vite config pins `port: 1420,
// strictPort: true` for the app, so a running `tauri dev` would make this fail
// to bind rather than roll to a free port.
//
// NOTHING HERE ANSWERS A REQUEST THROUGH THE ENGINE. The `answer` callback logs
// and settles LOCALLY, because there is no backend on this page — and because
// §3.5's rule is that the caller's identity is a property of the entry point:
// the real path is `answerPaneUi` in `orchestration.ts`, and a dev page that
// borrowed it would be claiming a trust boundary it does not sit on.

import "../src/styles.css";
import { StructuredPaneView } from "../src/structuredpane.ts";
import type { ProjectionInput } from "../src/structuredview.ts";

const host = document.getElementById("pane")!;
const stat = document.getElementById("stat")!;

let view: StructuredPaneView | null = null;
let dimmed = false;
/** Requests this page has settled locally, so the replay can show the settled
 *  card without a backend. Keyed by request id. */
const settled = new Set<string>();

function fresh(): StructuredPaneView {
  view?.dispose();
  host.textContent = "";
  settled.clear();
  dimmed = false;
  const v = new StructuredPaneView({
    groupId: "dev-replay",
    agentId: "dev-agent",
    cli: "pi",
    answer: async ({ requestId, channel, answer }) => {
      // Locally only — see the header. In the app this is `answerPaneUi`, and
      // the settlement comes BACK as a `*_settled` event rather than being
      // drawn optimistically, which is what this replay imitates.
      console.log("[replay] answer", { requestId, channel, answer });
      if (settled.has(requestId)) return;
      settled.add(requestId);
      v.apply([
        channel === "permission"
          ? { kind: "permission_settled", id: requestId, decision: answer, by: "human" }
          : { kind: "ui_settled", id: requestId, answer: { Value: answer }, by: "human" },
      ]);
    },
  });
  host.appendChild(v.el);
  v.show();
  view = v;
  return v;
}

/** Live DOM nodes inside the transcript, excluding the two spacers. THE figure
 *  this page exists to show: it must stay O(viewport) while the block count
 *  goes to `MAX_BLOCKS`. */
function liveRows(): number {
  return host.querySelectorAll(".spane-row").length;
}

/** Report AFTER the view's own frame has run, never in the same tick as the
 *  `apply()` that dirtied it: the view defers every DOM write to one rAF, so a
 *  count taken beside the call reports the PREVIOUS frame and reads as a
 *  virtualiser that is not working. Two frames, because the view schedules a
 *  second one whenever it learned a height in the first. */
function report(events: number): void {
  requestAnimationFrame(() =>
    requestAnimationFrame(() => {
      stat.textContent =
        `${events} events · ${liveRows()} row nodes · ` +
        `${document.querySelectorAll("#pane *").length} nodes total`;
    }),
  );
}

// ── the session fixture ─────────────────────────────────────────────────────

async function replaySession(): Promise<void> {
  const v = fresh();
  const text = await fetch(
    new URL("../test/fixtures/structuredview/session.harness.jsonl", import.meta.url),
  ).then((r) => r.text());
  // Split on `\n` only and strip a trailing `\r` — pi's own framing rule, and
  // what makes this reader correct on a CRLF checkout.
  const events = text
    .split("\n")
    .map((l) => (l.endsWith("\r") ? l.slice(0, -1) : l))
    .filter((l) => l.length > 0)
    .map((l) => JSON.parse(l) as ProjectionInput);

  // One event every 90 ms, so the streaming caret, the spinner, the fold
  // animations and the ticker's digit lift are all visible AS motion rather
  // than as a finished screen. A request card is left PENDING — that is the
  // state the attention pulse reports, and it is the one worth looking at.
  let i = 0;
  const step = (): void => {
    if (!view || view !== v || i >= events.length) return;
    const ev = events[i]!;
    i += 1;
    if ((ev.kind === "permission_settled" || ev.kind === "ui_settled") && !settled.has(ev.id)) {
      // Hold the settlement back so the card sits pending and answerable.
      setTimeout(step, 90);
      return;
    }
    v.apply([ev]);
    report(i);
    setTimeout(step, 90);
  };
  step();
}

// ── the storm ───────────────────────────────────────────────────────────────

/** 2500 blocks in one burst — past `MAX_BLOCKS` (2000), so the eviction
 *  sentinel appears too and its stated figure can be read. Delivered in
 *  64-event batches, which is the coalescer's own cap: this is what the pane
 *  really receives, not one call with 2500 events in it. */
function storm(): void {
  const v = fresh();
  const events: ProjectionInput[] = [{ kind: "turn_started", turn: 1 }];
  for (let n = 0; n < 830; n += 1) {
    const id = `t${n}`;
    events.push({ kind: "tool_call", turn: 1, id, name: "Bash", input: { command: `rg -n pattern-${n} src/` } });
    events.push({ kind: "tool_output", turn: 1, id, delta: `src/file${n}.ts:${n}: a match\n`.repeat(3), is_error: false });
    events.push({ kind: "tool_result", turn: 1, id, ok: n % 17 !== 0 });
  }
  let at = 0;
  const pump = (): void => {
    if (!view || view !== v || at >= events.length) {
      if (view === v) report(events.length);
      return;
    }
    v.apply(events.slice(at, at + 64));
    at += 64;
    report(at);
    requestAnimationFrame(pump);
  };
  pump();
}

// ── controls ────────────────────────────────────────────────────────────────

document.getElementById("session")!.addEventListener("click", () => void replaySession());
document.getElementById("storm")!.addEventListener("click", storm);
document.getElementById("reset")!.addEventListener("click", () => {
  fresh();
  report(0);
});
document.getElementById("dim")!.addEventListener("click", () => {
  dimmed = !dimmed;
  view?.setDimThinking(dimmed);
});

fresh();
report(0);
void replaySession();
