// Token-charts overlay for orchestration panes (#2011 slice C): a group's
// persisted usage series, plotted as spend over time per `block/cli` and as a
// stacked bar per feature, with the tuning marks that say when the fleet was
// retuned.
//
// Every answer that can be WRONG lives in `tokencharts.ts`, which is DOM-free
// and unit-tested. What is left here is SVG construction, polling and click
// wiring — hand-validated per the repo convention, and deliberately holding no
// arithmetic of its own: every number this file paints was computed there.
//
// Hard constraint 1 (never resize the PTY for a UI feature) lands on this view
// exactly as it lands on `timelineview.ts`: it is an embeddable view, so it
// FLOATS as an overlay by default and docks through the same #361 embed path
// every other panel uses. Nothing here measures or resizes a terminal — the
// chart's width comes from its own container via a `ResizeObserver`, and the
// pure layout takes that width as a parameter.
//
// Poll cadence is 30 s, NOT the 1.5 s the audit-backed views use
// (`polled-views.md`). The series moves once per five-minute bucket by
// construction, so a faster tick could only redraw the same picture — and
// `orch_usage_series` reads the whole file, which is the one read here that
// grows without bound.

import {
  orchTasks,
  usageSeries,
  type OrchTaskRow,
  type UsageSeries,
  type UsageSeriesRow,
} from "./orchestration";
import { RefreshGate } from "./refreshgate";
import { PollGate } from "./pollgate";
import { AuditStore } from "./auditstore";
import {
  DEFAULT_BEFORE_AFTER_K,
  DEFAULT_BUCKET_MS,
  HUE_ORDER,
  ORCHESTRATOR,
  TOTAL_ROW,
  UNATTRIBUTED,
  beforeAfter,
  bucketSeries,
  featureBars,
  firstSpendByBar,
  marks,
  scorecardColumns,
  type BeforeAfterRow,
  type BucketedSeries,
  type ChartMark,
  type FeatureBars,
  type Metric,
} from "./tokencharts";
import { makeScale, niceTicks, xForTs, type TimelineScale } from "./timelinelayout";
import { scorecardTable } from "./tokenscorecard";

/** Re-poll cadence while following. See the header: the series advances once
 *  per five-minute bucket, so 30 s is already far finer than the data. */
const FOLLOW_MS = 30_000;

const SVG_NS = "http://www.w3.org/2000/svg";

/** Window presets, as a span in ms. `null` means "everything the series has",
 *  which is the honest default for a file whose history starts at deploy. */
const WINDOWS: readonly { id: string; label: string; spanMs: number | null }[] = [
  { id: "1h", label: "1h", spanMs: 3_600_000 },
  { id: "6h", label: "6h", spanMs: 6 * 3_600_000 },
  { id: "24h", label: "24h", spanMs: 86_400_000 },
  { id: "7d", label: "7d", spanMs: 7 * 86_400_000 },
  { id: "all", label: "all", spanMs: null },
];
const DEFAULT_WINDOW = "24h";

/** Which token counter the plot draws. One axis, always — two measures of
 *  different scale are two charts, never two y-scales. */
const METRICS: readonly { id: Metric; label: string }[] = [
  { id: "total", label: "all tokens" },
  { id: "in", label: "input" },
  { id: "out", label: "output" },
  { id: "cache_r", label: "cache read" },
  { id: "cache_w", label: "cache write" },
  { id: "cost_usd", label: "cost (USD)" },
];

/** Plot chrome: room above the line, the strip that holds tick labels, and the
 *  left gutter the y-axis labels live in. */
const TOP_PAD_PX = 12;
const AXIS_PX = 22;
const PAD_LEFT_PX = 56;
const PAD_RIGHT_PX = 14;
const PLOT_H_PX = 190;
const BAR_ROW_PX = 26;
/** The 2px surface gap the mark specs put between adjacent stacked fills, so
 *  two segments read as two rather than as one long block. */
const SEG_GAP_PX = 2;
/** Direct labels are for a chart with few enough series to carry them; past
 *  this the legend does the work alone. */
const DIRECT_LABEL_MAX = 4;

function el(tag: string, cls: string, text?: string): HTMLElement {
  const e = document.createElement(tag);
  e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function svgEl(tag: string, cls?: string): SVGElement {
  const e = document.createElementNS(SVG_NS, tag);
  if (cls) e.setAttribute("class", cls);
  return e;
}

/** The CSS class carrying a key's hue. `null` (a ninth block) takes the
 *  neutral ramp rather than recycling slot 0 — a repeated hue would be a false
 *  claim that two blocks are one. */
const hueClass = (hueIndex: number | null): string =>
  hueIndex === null || hueIndex < 0 || hueIndex >= HUE_ORDER.length
    ? "tok-hue-neutral"
    : `tok-hue-${HUE_ORDER[hueIndex]}`;

/** The CSS class carrying a CLI's line style / bar hatch — the SECOND channel,
 *  so `worker-std/opencode` and `worker-std/pi` are told apart without needing
 *  a second hue (and are still told apart in greyscale, in print, and by a
 *  reader with a colour-vision deficiency). Deliberately NOT `cli-<name>`:
 *  that spelling is the agent mark's dye class, which `test/theme.test.ts`
 *  scans and holds to `--cli-<name>`, and nothing here paints a CLI hue. */
function cliClass(cli: string | null): string {
  if (cli === null) return "tok-cli-all";
  return `tok-cli-${cli.replace(/[^a-z0-9-]/gi, "-").toLowerCase()}`;
}

const fmtInt = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });
const fmtUsd = new Intl.NumberFormat(undefined, { style: "currency", currency: "USD" });

/** Tokens, short. A chart axis with `1,240,000` on it is mostly punctuation. */
function fmtTokens(n: number): string {
  const abs = Math.abs(n);
  if (abs >= 1_000_000) return `${(n / 1_000_000).toFixed(abs >= 10_000_000 ? 0 : 1)}M`;
  if (abs >= 1_000) return `${(n / 1_000).toFixed(abs >= 10_000 ? 0 : 1)}k`;
  return fmtInt.format(n);
}

/** A metric's value, formatted for its own units. `null` is always "n/a" and
 *  never `0` — the whole point of the null is that we could not measure. */
function fmtMetric(v: number | null, metric: Metric): string {
  if (v === null) return "n/a";
  return metric === "cost_usd" ? fmtUsd.format(v) : fmtTokens(v);
}

const fmtTime = (ms: number): string =>
  new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });

/** One scorecard cell — `orch-scorecard.cjs`'s `fmtCell`, pane-widthed: the
 *  median, then the IQR and n that say whether the middle moved and whether
 *  the spread swamps it. A cell below the median's floor is `n/a (n=…)` —
 *  legible as "not enough data", never as a missing measurement, and never
 *  a plausible zero. */
function fmtCell(c: { n: number; median: number | null; q1: number | null; q3: number | null }): string {
  if (c.median === null) return `n/a (n=${c.n})`;
  return `${c.median} (IQR ${c.q1}–${c.q3}, n=${c.n})`;
}

export class TokenChartsView {
  readonly el: HTMLElement;
  private countEl: HTMLElement;
  private followBtn: HTMLButtonElement;
  private embedBtn: HTMLButtonElement;
  private closeBtn: HTMLButtonElement;
  private controlsEl: HTMLElement;
  private windowBarEl: HTMLElement;
  private metricBarEl: HTMLElement;
  private collapseBtn: HTMLButtonElement;
  private bodyEl: HTMLElement;
  private legendEl: HTMLElement;
  private plotEl: HTMLElement;
  private barsEl: HTMLElement;
  private readoutEl: HTMLElement;
  private scorecardEl: HTMLElement;
  private notesEl: HTMLElement;

  /** The last successful read, kept whole. Null before the first one lands —
   *  which is a THIRD state beside "empty" and "failed", and the empty text
   *  below distinguishes all three. */
  private series: UsageSeries | null = null;
  private readError: unknown = null;
  /** The last board read failed, so the bar attribution is the previous
   *  one. Surfaced as a note — a stale split is fine, a silent one is not. */
  private boardStale = false;
  private attempted = false;
  /** The board, for the attribution ladder. A failed board read leaves the
   *  last good one rather than emptying it: reading every agent as
   *  unattributed because one IPC call rejected is "could not look" rendered
   *  as "there was nothing there". */
  private board: readonly OrchTaskRow[] = [];

  private windowId = DEFAULT_WINDOW;
  private metric: Metric = "total";
  private collapseCli = false;
  /** The mark the readout is about — held as an INSTANT, not an index, because
   *  every poll rebuilds the mark array. */
  private selectedMarkMs: number | null = null;

  private follow = false;
  private followTimer: number | undefined;
  /** Window-visibility gate around the follow timer (#743 S6, `pollgate.ts`) —
   *  an armed follow behind a minimized window would re-read the whole series
   *  file every 30 s to redraw a chart nobody can see. */
  private followGate: PollGate = new PollGate({
    arm: () => {
      this.followTimer = window.setInterval(() => void this.load(), FOLLOW_MS);
    },
    disarm: () => {
      if (this.followTimer !== undefined) {
        clearInterval(this.followTimer);
        this.followTimer = undefined;
      }
    },
    refresh: () => void this.load(),
  });

  private disposed = false;
  private gate = new RefreshGate();
  private resizeObs: ResizeObserver;
  private rafPending = false;
  private lastWidthPx = 0;
  /** Signature of the last render, so a poll that changed nothing does not
   *  rebuild the SVG under the human's cursor. */
  private lastSig = "";

  private store: AuditStore;

  constructor(
    private groupId: string,
    private opts: {
      onClose: () => void;
      onEmbedMenu?: (anchor: HTMLElement) => void;
      /** The pane's one `orch_audit` read, shared with the audit viewer and
       *  the timeline (#1317) — this view never fires a second one. */
      store: AuditStore;
      /** Whether this pane's board panel is open. Unused by the read — the
       *  chart reads the board itself through the typed wrapper — but kept so
       *  a future slice can share one read with the board rather than adding
       *  a second. */
      isBoardVisible?: () => boolean;
    }
  ) {
    this.store = opts.store;
    this.el = el("div", "tokens-view");

    const head = el("div", "tokens-head");
    head.append(el("span", "tokens-title", "token charts"));
    head.append(el("span", "tokens-group", groupId));
    this.countEl = el("span", "tokens-count");
    head.append(this.countEl);

    this.followBtn = el("button", "tokens-follow", "▶ follow") as HTMLButtonElement;
    this.followBtn.title = "Live-follow: re-poll the series every 30s";
    this.followBtn.addEventListener("click", () => this.toggleFollow());
    head.append(this.followBtn);

    const refresh = el("button", "pane-btn", "⟳") as HTMLButtonElement;
    refresh.title = "Refresh";
    refresh.addEventListener("click", () => void this.load());
    head.append(refresh);

    this.embedBtn = el("button", "pane-btn embed", "⬒") as HTMLButtonElement;
    this.embedBtn.addEventListener("click", () => this.opts.onEmbedMenu?.(this.embedBtn));
    head.append(this.embedBtn);

    this.closeBtn = el("button", "pane-btn close", "✕") as HTMLButtonElement;
    this.closeBtn.title = "Close (Alt+K)";
    this.closeBtn.addEventListener("click", () => this.opts.onClose());
    head.append(this.closeBtn);
    this.setPanelActive(false);

    // Filters in ONE row above the charts.
    this.controlsEl = el("div", "tokens-controls");
    this.windowBarEl = el("div", "tokens-windows");
    for (const w of WINDOWS) {
      const b = el("button", "tokens-chip window", w.label) as HTMLButtonElement;
      b.dataset.window = w.id;
      b.title = w.spanMs === null ? "Everything the series has" : `The last ${w.label}`;
      b.addEventListener("click", () => {
        this.windowId = w.id;
        this.selectedMarkMs = null; // the mark may not be in the new window
        this.syncChips();
        this.rerender();
      });
      this.windowBarEl.append(b);
    }
    this.metricBarEl = el("div", "tokens-metrics");
    for (const m of METRICS) {
      const b = el("button", "tokens-chip metric", m.label) as HTMLButtonElement;
      b.dataset.metric = m.id;
      b.title = `Plot ${m.label}`;
      b.addEventListener("click", () => {
        this.metric = m.id;
        this.syncChips();
        this.rerender();
      });
      this.metricBarEl.append(b);
    }
    this.collapseBtn = el("button", "tokens-chip collapse", "merge CLIs") as HTMLButtonElement;
    this.collapseBtn.title = "Collapse each block's CLI split into one series";
    this.collapseBtn.addEventListener("click", () => {
      this.collapseCli = !this.collapseCli;
      this.syncChips();
      this.rerender();
    });
    this.controlsEl.append(this.windowBarEl, this.metricBarEl, this.collapseBtn);

    this.bodyEl = el("div", "tokens-body");
    this.legendEl = el("div", "tokens-legend");
    this.plotEl = el("div", "tokens-plot");
    this.barsEl = el("div", "tokens-bars");
    this.readoutEl = el("div", "tokens-readout");
    this.scorecardEl = el("div", "tokens-scorecard");
    this.notesEl = el("div", "tokens-notes");
    this.bodyEl.append(this.legendEl, this.plotEl, this.barsEl, this.readoutEl, this.scorecardEl, this.notesEl);

    this.el.append(head, this.controlsEl, this.bodyEl);

    // The chart is laid out against its OWN container's width — never against
    // the terminal's, and never by resizing anything (constraint 1).
    this.resizeObs = new ResizeObserver(() => this.onResize());
    this.resizeObs.observe(this.plotEl);

    this.syncChips();
  }

  /** Called by the pane whenever the view becomes visible, in either mode. */
  show(): void {
    void this.load();
  }

  /** Called by the pane whenever the view is hidden — stop the poll. Without
   *  this, every close or slot-eviction leaks a live interval (#361 rev-38). */
  hide(): void {
    this.stopFollow();
    if (this.follow) {
      this.follow = false;
      this.followBtn.classList.remove("on");
      this.followBtn.textContent = "▶ follow";
    }
  }

  setPanelActive(active: boolean): void {
    this.embedBtn.classList.toggle("active", active);
    this.embedBtn.textContent = active ? "⬓" : "⬒";
    this.embedBtn.title = active
      ? "Un-embed — back to a floating overlay"
      : "Embed beside the terminal (resizes this pane)";
    this.closeBtn.disabled = active;
    this.closeBtn.title = active
      ? "Docked — un-embed it (side menu) to close"
      : "Close (Alt+K)";
  }

  dispose(): void {
    this.disposed = true;
    this.stopFollow();
    this.resizeObs.disconnect();
    this.el.remove();
  }

  private toggleFollow(): void {
    this.follow = !this.follow;
    this.followBtn.classList.toggle("on", this.follow);
    this.followBtn.textContent = this.follow ? "⏸ following" : "▶ follow";
    if (this.follow) {
      this.followGate.enable();
      void this.load();
    } else {
      this.stopFollow();
    }
  }

  private stopFollow(): void {
    this.followGate.disable();
  }

  private syncChips(): void {
    for (const b of Array.from(this.windowBarEl.children) as HTMLButtonElement[]) {
      b.classList.toggle("on", b.dataset.window === this.windowId);
    }
    for (const b of Array.from(this.metricBarEl.children) as HTMLButtonElement[]) {
      b.classList.toggle("on", b.dataset.metric === this.metric);
    }
    this.collapseBtn.classList.toggle("on", this.collapseCli);
  }

  private onResize(): void {
    const w = Math.round(this.plotEl.clientWidth);
    if (w === this.lastWidthPx) return;
    if (this.rafPending) return;
    // Coalesce a drag's per-frame resizes into one relayout.
    this.rafPending = true;
    requestAnimationFrame(() => {
      this.rafPending = false;
      if (this.disposed) return;
      this.rerender();
    });
  }

  /** Force the next `render()` to do real work — the width, the window and the
   *  metric are all part of the geometry rather than of the data, so a chip
   *  click must not be skipped by the no-op signature check. */
  private rerender(): void {
    this.lastSig = "";
    this.render();
  }

  private async load(): Promise<void> {
    if (this.disposed) return;
    // Single-flight with a trailing re-run, so a click during an in-flight
    // fetch is neither dropped nor run concurrently (`refreshgate.ts`).
    if (!this.gate.begin()) return;
    try {
      this.attempted = true;
      this.boardStale = false;
      // `sinceMs: 0` reads the file whole. That IS the command's shape while
      // the writer is append-only and unrotated (slice B's design note:
      // "`since_ms` filters, it does not seek"), and the windowing this view
      // offers is a view over what was read, not a second read. The audit
      // store is the pane's SHARED read (#1317) and is taken at its own
      // window rather than forced, so this view never triggers an extra
      // `orch_audit` for a file the audit viewer is already polling.
      const [read] = await Promise.all([usageSeries(this.groupId, 0), this.store.read()]);
      if (read !== null) {
        this.series = read;
        this.readError = null;
      } else {
        // `null` is "the id did not validate, or the read was refused" — a
        // real failure. An absent series FILE is a normal empty payload, not
        // a null, so this never fires for a group that simply has not spent.
        this.readError = "the group's usage series could not be read";
      }
      // The board is read through the typed wrapper (constraint 5), and a
      // FAILED read leaves the last good rows rather than emptying them:
      // reading every agent as unattributed because one IPC call rejected
      // is "could not look" rendered as "there was nothing there".
      try {
        this.board = await orchTasks(this.groupId);
      } catch {
        // Additive, exactly like the timeline's gh half: the series is the
        // chart, the board only decides which BAR each agent lands on, so a
        // board that cannot be read degrades to the previous attribution
        // rather than blanking a plot that is perfectly readable.
        this.boardStale = true;
      }
    } catch (err) {
      // The last good series is KEPT: a transient rejection must leave the
      // chart as it was rather than blanking it.
      this.readError = err;
    } finally {
      // `end()` first, above `render()` — `render()` is the widest throwing
      // surface here, and leaving the gate `running` behind a throw would
      // make every later `load()` return at `begin()` and freeze the view for
      // the session with nothing on screen to say so (#1317 review N4).
      const rerun = this.gate.end();
      if (!this.disposed) this.render();
      if (rerun && !this.disposed) void this.load();
    }
  }

  /** The window the presets resolve to, against the data rather than the
   *  clock alone: "all" spans the series itself, and every preset is clamped
   *  so a window wider than the history does not draw a mile of empty grid.
   *
   *  **The extent is FOLDED, never spread.** `Math.min(...rows)` is a spread
   *  onto the call stack and throws `RangeError` past ~125k elements
   *  (measured on this repo's Node: 124,000 fine, 125,000 throws). This runs
   *  at the top of every `render()`, before any guard, over a file that is
   *  append-only and never rotated — so the spread form was not a slow path,
   *  it was a date on which the panel freezes permanently and never recovers,
   *  because the file cannot shrink back under the limit. `scorecardColumns`
   *  folds its own floor for exactly this reason and says so; this is the
   *  same operation and takes the same shape. */
  private resolveWindow(rows: readonly UsageSeriesRow[]): { startMs: number; endMs: number } {
    const now = Date.now();
    let lo: number | null = null;
    let hi: number | null = null;
    for (const r of rows) {
      const t = r.ts_ms;
      if (!Number.isFinite(t)) continue;
      if (lo === null || t < lo) lo = t;
      if (hi === null || t > hi) hi = t;
    }
    const oldest = lo ?? now - DEFAULT_BUCKET_MS;
    const newest = hi ?? now;
    const preset = WINDOWS.find((w) => w.id === this.windowId) ?? WINDOWS[0];
    if (preset.spanMs === null) return { startMs: oldest, endMs: Math.max(newest, oldest) };
    const endMs = Math.max(newest, now);
    return { startMs: Math.max(oldest, endMs - preset.spanMs), endMs };
  }

  private render(): void {
    if (this.disposed) return;
    const widthPx = Math.round(this.plotEl.clientWidth);
    const rows = this.series?.rows ?? [];
    const range = this.resolveWindow(rows);

    const sig = [
      rows.length,
      this.series?.first_ts_ms ?? "",
      this.series?.skipped ?? "",
      this.store.cached.length,
      this.series?.agents.length ?? "",
      this.board.length,
      this.boardStale ? "1" : "0",
      this.windowId,
      this.metric,
      this.collapseCli ? "1" : "0",
      this.selectedMarkMs ?? "",
      widthPx,
      this.readError === null ? "" : String(this.readError),
      // "now" slides even when the data does not — but only at bucket
      // resolution, so a still session does not rebuild twice a minute.
      Math.floor(range.endMs / DEFAULT_BUCKET_MS),
    ].join("|");
    if (sig === this.lastSig) return;
    this.lastSig = sig;
    this.lastWidthPx = widthPx;

    // The block order the hues follow. Taken from the group's whole ROSTER
    // (plus any block only the rows know about), never from the windowed
    // data — a filter that changes which series are on screen must not
    // repaint the survivors.
    const blockOrder = this.blockOrder();

    const series = bucketSeries(rows, {
      startMs: range.startMs,
      endMs: range.endMs,
      bucketMs: DEFAULT_BUCKET_MS,
      collapseCli: this.collapseCli,
      blockOrder,
    });
    const bars = featureBars(rows, this.series?.agents ?? [], this.board, {
      collapseCli: this.collapseCli,
      startMs: range.startMs,
      endMs: range.endMs,
      blockOrder,
    });
    const markList = marks(rows).filter((m) => m.tsMs >= range.startMs && m.tsMs <= range.endMs);

    this.countEl.textContent =
      rows.length === 0 ? "" : `${fmtInt.format(series.keys.length)} series · ${fmtInt.format(rows.length)} rows`;

    this.renderLegend(bars, series);
    this.renderPlot(series, markList, widthPx);
    this.renderBars(bars);
    this.renderReadout(series, markList);
    this.renderScorecard();
    this.renderNotes(series, bars);
  }

  /** The stable hue ordering. Roster first (its own order — the group's own
   *  sense of who is who), then any block that only the rows carry, so a block
   *  whose agents have all exited is still drawable. */
  private blockOrder(): string[] {
    const out: string[] = [];
    const seen = new Set<string>();
    const push = (b: string) => {
      const v = (b ?? "").trim();
      if (!v || seen.has(v)) return;
      seen.add(v);
      out.push(v);
    };
    for (const a of this.series?.agents ?? []) push(a.block);
    for (const r of this.series?.rows ?? []) if (r.kind === "sample") push(r.block);
    return out;
  }

  /** The legend, which is ALWAYS present for two or more series — identity is
   *  never carried by colour alone — and which prints the three-number split
   *  beside it.
   *
   *  The total carries its own SCOPE in its label. These figures cover the
   *  selected window, not the group's lifetime, and the two are only the
   *  same under the `all` preset — so the label says which, rather than
   *  leaving a reader to compare a 24 h figure against the group panel's
   *  lifetime one and conclude the chart is broken. */
  private renderLegend(bars: FeatureBars, series: BucketedSeries): void {
    this.legendEl.replaceChildren();
    if (series.keys.length === 0) return;

    const totals = el("div", "tokens-lifetime");
    const num = (label: string, v: number, cls: string) => {
      const s = el("span", `tokens-lifetime-item ${cls}`);
      s.append(el("span", "tokens-lifetime-label", label));
      s.append(el("span", "tokens-lifetime-value", fmtTokens(v)));
      return s;
    };
    totals.append(num("features", bars.totals.features, "features"));
    totals.append(el("span", "tokens-lifetime-op", "+"));
    totals.append(num("orchestrator", bars.totals.orchestrator, "orchestrator"));
    totals.append(el("span", "tokens-lifetime-op", "+"));
    totals.append(num("unattributed", bars.totals.unattributed, "unattributed"));
    totals.append(el("span", "tokens-lifetime-op", "="));
    // The scope is IN the label, because the figure is only a lifetime under
    // the `all` preset and a reader has no other way to tell.
    const allWindow = this.windowId === "all";
    const scopeLabel = allWindow
      ? "all time"
      : (WINDOWS.find((w) => w.id === this.windowId)?.label ?? this.windowId);
    totals.append(num(`total (${scopeLabel})`, bars.totals.total, "total"));
    totals.title =
      `Every token in the ${scopeLabel} window, split three ways. An orchestrator's ` +
      "spend is group-wide by construction — there is no per-turn PR attribution " +
      "for one — and unattributed is what no rung of the ladder could place." +
      (allWindow
        ? " This is the whole series, so it is the figure the group panel's own " +
          "lifetime total is comparable with."
        : " This is the SELECTED WINDOW, not the group's lifetime — switch to " +
          "'all' before comparing it with the group panel's lifetime total.");
    this.legendEl.append(totals);

    const keys = el("div", "tokens-legend-keys");
    for (const k of series.keys) {
      const chip = el("span", "tokens-legend-key");
      const swatch = svgEl("svg", "tokens-swatch") as SVGSVGElement;
      swatch.setAttribute("width", "22");
      swatch.setAttribute("height", "10");
      const line = svgEl("line", `tokens-line ${hueClass(k.hueIndex)} ${cliClass(k.cli)}`);
      line.setAttribute("x1", "1");
      line.setAttribute("x2", "21");
      line.setAttribute("y1", "5");
      line.setAttribute("y2", "5");
      swatch.append(line);
      chip.append(swatch);
      chip.append(el("span", "tokens-legend-label", k.key));
      chip.append(el("span", "tokens-legend-total", fmtTokens(k.total)));
      chip.title = `${k.key} — ${fmtInt.format(k.total)} tokens over ${fmtInt.format(k.samples)} sampled intervals`;
      keys.append(chip);
    }
    this.legendEl.append(keys);
  }

  private renderPlot(series: BucketedSeries, markList: readonly ChartMark[], widthPx: number): void {
    this.plotEl.replaceChildren();

    if (!this.attempted) {
      this.plotEl.append(el("div", "tokens-empty", "Reading this group's usage series…"));
      return;
    }
    if (this.readError !== null && this.series === null) {
      this.plotEl.append(
        el("div", "tokens-empty", `Could not read this group's usage series — ${String(this.readError)}`)
      );
      return;
    }
    if (series.keys.length === 0) {
      // Three answers, not two. An empty series is the ORDINARY state for a
      // group that has not spent yet, and it must not read as a failure.
      this.plotEl.append(
        el(
          "div",
          "tokens-empty",
          "No usage recorded for this group yet. History starts when the build that writes the " +
            "series first ran against it — nothing is rebuilt from transcripts."
        )
      );
      return;
    }

    const height = TOP_PAD_PX + PLOT_H_PX + AXIS_PX;
    const scale = makeScale(
      { startMs: series.startMs, endMs: series.endMs },
      Math.max(0, widthPx),
      PAD_LEFT_PX,
      PAD_RIGHT_PX
    );
    const svg = svgEl("svg", "tokens-svg") as SVGSVGElement;
    svg.setAttribute("width", String(Math.max(0, widthPx)));
    svg.setAttribute("height", String(height));

    const valueAt = (kIndex: number, bIndex: number): number => {
      const p = series.keys[kIndex].points[bIndex];
      const v = this.metric === "cost_usd" ? p.cost_usd : p[this.metric];
      return v ?? 0;
    };
    let max = 0;
    for (let k = 0; k < series.keys.length; k++) {
      for (let b = 0; b < series.buckets.length; b++) max = Math.max(max, valueAt(k, b));
    }
    // A flat-zero window still gets a sensible axis rather than a divide by
    // zero; the y labels then read 0 throughout, which is the truth.
    const yMax = max > 0 ? max : 1;
    const yFor = (v: number): number => TOP_PAD_PX + PLOT_H_PX - (v / yMax) * PLOT_H_PX;

    // Recessive grid: three horizontal rules with their values, and the time
    // ticks the shared layout picks.
    for (const frac of [0, 0.5, 1]) {
      const v = yMax * frac;
      const y = yFor(v);
      const rule = svgEl("line", "tokens-grid");
      rule.setAttribute("x1", String(scale.x0));
      rule.setAttribute("x2", String(scale.x1));
      rule.setAttribute("y1", String(y));
      rule.setAttribute("y2", String(y));
      svg.append(rule);
      const label = svgEl("text", "tokens-y-label");
      label.setAttribute("x", String(scale.x0 - 6));
      label.setAttribute("y", String(y + 3));
      label.setAttribute("text-anchor", "end");
      label.textContent = fmtMetric(v, this.metric);
      svg.append(label);
    }

    const ticks = niceTicks({ startMs: series.startMs, endMs: series.endMs });
    const fmt = new Intl.DateTimeFormat(
      undefined,
      ticks.stepMs >= 86_400_000
        ? { month: "short", day: "numeric" }
        : { hour: "2-digit", minute: "2-digit" }
    );
    for (const t of ticks.ticks) {
      const x = xForTs(scale, t);
      const line = svgEl("line", "tokens-tick");
      line.setAttribute("x1", String(x));
      line.setAttribute("x2", String(x));
      line.setAttribute("y1", String(TOP_PAD_PX));
      line.setAttribute("y2", String(TOP_PAD_PX + PLOT_H_PX));
      svg.append(line);
      const label = svgEl("text", "tokens-tick-label");
      label.setAttribute("x", String(x));
      label.setAttribute("y", String(height - 7));
      label.setAttribute("text-anchor", "middle");
      label.textContent = fmt.format(new Date(t));
      svg.append(label);
    }

    // The marks, UNDER the lines so a vertical never hides a data point.
    for (const m of markList) {
      const x = xForTs(scale, m.tsMs);
      const g = svgEl("g", `tokens-mark-g${this.selectedMarkMs === m.tsMs ? " selected" : ""}`);
      const line = svgEl("line", "tokens-mark");
      line.setAttribute("x1", String(x));
      line.setAttribute("x2", String(x));
      line.setAttribute("y1", String(TOP_PAD_PX));
      line.setAttribute("y2", String(TOP_PAD_PX + PLOT_H_PX));
      g.append(line);
      // A wide invisible hit target — the mark itself is 1px, which is not a
      // clickable thing.
      const hit = svgEl("rect", "tokens-mark-hit");
      hit.setAttribute("x", String(x - 6));
      hit.setAttribute("y", String(TOP_PAD_PX));
      hit.setAttribute("width", "12");
      hit.setAttribute("height", String(PLOT_H_PX));
      g.append(hit);
      const label = svgEl("text", "tokens-mark-label");
      label.setAttribute("x", String(x + 4));
      label.setAttribute("y", String(TOP_PAD_PX + 9));
      label.textContent = m.label;
      g.append(label);
      const title = svgEl("title");
      title.textContent =
        `${fmtTime(m.tsMs)} — ${m.label}\n` +
        `fingerprint components changed: ${m.changed.length > 0 ? m.changed.join(", ") : "(none recorded)"}` +
        (m.fpPartial
          ? "\nA component could not be read in full when this mark was written " +
            "(a file over the size cap, a tree past the depth cap, or a read that " +
            "failed), so an unchanged component here is not proof that nothing " +
            "under it moved. A cap can persist, so this may show on every mark."
          : "") +
        "\nClick for the before/after readout.";
      g.append(title);
      g.addEventListener("click", () => {
        this.selectedMarkMs = this.selectedMarkMs === m.tsMs ? null : m.tsMs;
        this.rerender();
      });
      svg.append(g);
    }

    // One polyline per key. Colour is the block, dash the CLI.
    series.keys.forEach((k, ki) => {
      const pts = series.buckets
        .map((t, bi) => `${xForTs(scale, t).toFixed(2)},${yFor(valueAt(ki, bi)).toFixed(2)}`)
        .join(" ");
      const poly = svgEl("polyline", `tokens-line ${hueClass(k.hueIndex)} ${cliClass(k.cli)}`);
      poly.setAttribute("points", pts);
      const title = svgEl("title");
      title.textContent = `${k.key} — ${fmtInt.format(k.total)} tokens in this window`;
      poly.append(title);
      svg.append(poly);

      // Direct labels for a chart with few enough series to carry them; past
      // that the legend does the work alone rather than the plot becoming a
      // wall of text.
      if (series.keys.length <= DIRECT_LABEL_MAX && series.buckets.length > 0) {
        const last = series.buckets.length - 1;
        const label = svgEl("text", `tokens-direct-label ${hueClass(k.hueIndex)}`);
        label.setAttribute("x", String(scale.x1 - 2));
        label.setAttribute("y", String(Math.max(TOP_PAD_PX + 8, yFor(valueAt(ki, last)) - 4)));
        label.setAttribute("text-anchor", "end");
        label.textContent = k.key;
        svg.append(label);
      }
    });

    // The hover layer: a crosshair that follows the pointer and a tooltip
    // naming every key's value at that bucket. A line chart without one is a
    // picture; with one it is readable.
    this.attachCrosshair(svg, scale, series, height);

    this.plotEl.append(svg);
  }

  /** Crosshair + per-bucket tooltip. Kept here rather than in the pure layer
   *  because it computes nothing — it indexes into buckets the projection
   *  already built. */
  private attachCrosshair(
    svg: SVGSVGElement,
    scale: TimelineScale,
    series: BucketedSeries,
    height: number
  ): void {
    if (series.buckets.length === 0) return;
    const cross = svgEl("line", "tokens-crosshair");
    cross.setAttribute("y1", String(TOP_PAD_PX));
    cross.setAttribute("y2", String(TOP_PAD_PX + PLOT_H_PX));
    (cross as SVGElement & { style: CSSStyleDeclaration }).style.display = "none";
    svg.append(cross);

    const hit = svgEl("rect", "tokens-hover-hit");
    hit.setAttribute("x", String(scale.x0));
    hit.setAttribute("y", String(TOP_PAD_PX));
    hit.setAttribute("width", String(Math.max(0, scale.x1 - scale.x0)));
    hit.setAttribute("height", String(PLOT_H_PX));
    const tip = svgEl("title");
    hit.append(tip);
    svg.append(hit);
    void height;

    const bucketAt = (clientX: number): number => {
      const box = svg.getBoundingClientRect();
      const x = clientX - box.left;
      const width = scale.x1 - scale.x0;
      if (width <= 0) return 0;
      const frac = Math.min(1, Math.max(0, (x - scale.x0) / width));
      return Math.min(series.buckets.length - 1, Math.round(frac * (series.buckets.length - 1)));
    };

    hit.addEventListener("mousemove", (ev) => {
      const i = bucketAt((ev as MouseEvent).clientX);
      const x = xForTs(scale, series.buckets[i]);
      cross.setAttribute("x1", String(x));
      cross.setAttribute("x2", String(x));
      (cross as SVGElement & { style: CSSStyleDeclaration }).style.display = "";
      const lines = series.keys
        .map((k) => {
          const p = k.points[i];
          const v = this.metric === "cost_usd" ? p.cost_usd : p[this.metric];
          return `${k.key}: ${fmtMetric(v, this.metric)}${p.reset ? " (counter reset — clamped)" : ""}`;
        })
        .join("\n");
      tip.textContent = `${fmtTime(series.buckets[i])}\n${lines}`;
    });
    hit.addEventListener("mouseleave", () => {
      (cross as SVGElement & { style: CSSStyleDeclaration }).style.display = "none";
    });
  }

  /** The stacked bars: one row per feature, plus the two group-wide bars. */
  private renderBars(bars: FeatureBars): void {
    this.barsEl.replaceChildren();
    if (bars.totals.total === 0) return;

    this.barsEl.append(el("div", "tokens-section-title", "tokens per feature"));
    // Folded, not spread, for the same reason `resolveWindow` is: the bar
    // list is one entry per BOARD feature plus two, so it is caller data with
    // no ceiling of its own. Cheaper to fold than to argue about how big a
    // task board may get.
    let max = 1;
    for (const b of bars.bars) if (b.total > max) max = b.total;

    for (const bar of bars.bars) {
      const rowEl = el("div", `tokens-bar-row kind-${bar.kind}`);
      const label = el("span", "tokens-bar-label", bar.label);
      label.title =
        bar.agents.length > 0
          ? `${bar.label}\nagents: ${bar.agents.join(", ")}`
          : `${bar.label}\nno agent spent on this in this window`;
      rowEl.append(label);

      const track = el("div", "tokens-bar-track");
      const svg = svgEl("svg", "tokens-bar-svg") as SVGSVGElement;
      svg.setAttribute("height", String(BAR_ROW_PX - 10));
      svg.setAttribute("preserveAspectRatio", "none");
      // A percentage viewBox lets the stack scale with the container without
      // this file measuring anything — no width read, no resize coupling.
      svg.setAttribute("viewBox", `0 0 1000 ${BAR_ROW_PX - 10}`);
      let x = 0;
      for (const seg of bar.segments) {
        if (seg.tokens <= 0) continue;
        const w = (seg.tokens / max) * 1000;
        const rect = svgEl("rect", `tokens-seg ${hueClass(seg.hueIndex)} ${cliClass(seg.cli)}`);
        rect.setAttribute("x", String(x));
        rect.setAttribute("y", "0");
        // The 2px surface gap between adjacent fills, in viewBox units, and
        // never wider than the segment itself.
        rect.setAttribute("width", String(Math.max(0.5, w - SEG_GAP_PX)));
        rect.setAttribute("height", String(BAR_ROW_PX - 10));
        rect.setAttribute("rx", "2");
        const title = svgEl("title");
        title.textContent =
          `${bar.label} · ${seg.key}\n${fmtInt.format(seg.tokens)} tokens` +
          (seg.costUsd === null ? "\ncost: n/a (a contributing interval carried no figure)" : `\ncost: ${fmtUsd.format(seg.costUsd)}`);
        rect.append(title);
        svg.append(rect);
        x += w;
      }
      track.append(svg);
      rowEl.append(track);

      const value = el("span", "tokens-bar-value", fmtTokens(bar.total));
      value.title = `${fmtInt.format(bar.total)} tokens`;
      rowEl.append(value);

      const cost = el(
        "span",
        `tokens-bar-cost cost-${bar.costLabel}`,
        bar.costUsd === null ? "n/a" : fmtUsd.format(bar.costUsd)
      );
      cost.title =
        bar.costLabel === "none"
          ? "no cost figures for this bar"
          : `cost figures on this bar are ${bar.costLabel}`;
      rowEl.append(cost);

      this.barsEl.append(rowEl);
    }
  }

  /** The before/after readout for the selected mark — the whole reason the
   *  marks are labelled. */
  private renderReadout(series: BucketedSeries, markList: readonly ChartMark[]): void {
    this.readoutEl.replaceChildren();
    if (this.selectedMarkMs === null) {
      if (markList.length > 0) {
        this.readoutEl.append(
          el("div", "tokens-hint", "Click a change mark on the plot for the before/after readout.")
        );
      }
      return;
    }
    const mark = markList.find((m) => m.tsMs === this.selectedMarkMs);
    if (!mark) {
      this.selectedMarkMs = null;
      return;
    }

    const head = el("div", "tokens-readout-head");
    head.append(el("span", "tokens-readout-title", `${mark.label} · ${fmtTime(mark.tsMs)}`));
    head.append(
      el(
        "span",
        "tokens-readout-scope",
        `mean per 5-min bucket, ${DEFAULT_BEFORE_AFTER_K} buckets (1h) each side`
      )
    );
    const close = el("button", "pane-btn close", "✕") as HTMLButtonElement;
    close.title = "Clear selection";
    close.addEventListener("click", () => {
      this.selectedMarkMs = null;
      this.rerender();
    });
    head.append(close);
    this.readoutEl.append(head);

    const rows = beforeAfter(series, mark.tsMs, this.metric, DEFAULT_BEFORE_AFTER_K);
    const table = el("table", "tokens-table");
    const thead = el("thead", "");
    const hr = el("tr", "");
    for (const h of ["series", "before", "after", "change"]) hr.append(el("th", "", h));
    thead.append(hr);
    table.append(thead);
    const tbody = el("tbody", "");
    for (const r of rows) {
      tbody.append(this.readoutRow(r));
    }
    table.append(tbody);
    this.readoutEl.append(table);

    if (rows.every((r) => r.before === null && r.after === null)) {
      this.readoutEl.append(
        el(
          "div",
          "tokens-note",
          `Fewer than ${DEFAULT_BEFORE_AFTER_K} buckets on one side of this mark, so there is no ` +
            "mean to report. n/a is the honest answer here — a zero would say the fleet spent nothing."
        )
      );
    }
  }

  private readoutRow(r: BeforeAfterRow): HTMLElement {
    const tr = el("tr", r.key === TOTAL_ROW ? "tokens-row-total" : "");
    tr.append(el("td", "tokens-cell-key", r.key));
    tr.append(el("td", "tokens-cell-num", fmtMetric(r.before, this.metric)));
    tr.append(el("td", "tokens-cell-num", fmtMetric(r.after, this.metric)));
    const change = el(
      "td",
      `tokens-cell-num ${r.delta === null ? "" : r.delta > 0 ? "up" : r.delta < 0 ? "down" : ""}`
    );
    change.textContent =
      r.delta === null
        ? "n/a"
        : `${r.delta > 0 ? "+" : ""}${fmtMetric(r.delta, this.metric)}${
            r.pct === null ? "" : ` (${r.pct > 0 ? "+" : ""}${(r.pct * 100).toFixed(0)}%)`
          }`;
    tr.append(change);
    return tr;
  }

  /** The scorecard table (#2011 slice D): one row per `block/cli` lane with
   *  the review-loop columns beside it, computed by `tokenscorecard.ts` (a
   *  port of `scripts/orch-scorecard.cjs`, which stays the spec) over the
   *  SAME reads this view already holds — the shared `AuditStore` and the
   *  series roster. The caption is the coverage floor's spawn-row-missing
   *  rule: a window credited with a delegate whose `agent-spawn` row did
   *  not survive the read has lost rows, and says so rather than reading as
   *  complete. */
  private renderScorecard(): void {
    this.scorecardEl.replaceChildren();
    const sc = scorecardTable(this.store.cached, this.series?.agents ?? []);
    if (sc.floor.rowsRead === 0) {
      // "We have not looked" — distinguishable from a genuinely quiet log
      // only by saying which of the two it is.
      this.scorecardEl.append(
        el("div", "tokens-section-title", "scorecard — per block × cli"),
        el("div", "tokens-note", "The audit log has not been read yet, so there is nothing to score."),
      );
      return;
    }
    this.scorecardEl.append(el("div", "tokens-section-title", "scorecard — per block × cli"));
    if (sc.cards.length === 0) {
      this.scorecardEl.append(
        el(
          "div",
          "tokens-note",
          "No PR in the audit window has a review loop yet — the table appears when review verdicts do."
        ),
      );
      return;
    }
    const table = el("table", "tokens-table");
    const thead = el("thead", "");
    const hr = el("tr", "");
    for (const h of ["lane", "PRs", "median rounds to pass", "fail rate", "median wall-clock (h)"]) {
      hr.append(el("th", "", h));
    }
    thead.append(hr);
    table.append(thead);
    const tbody = el("tbody", "");
    for (const r of sc.rows) {
      const tr = el("tr", "");
      tr.append(el("td", "tokens-cell-key", r.key));
      const prs = el("td", "tokens-cell-num", String(r.prs.length));
      prs.title = `PRs scored for this lane: ${r.prs.map((p) => `#${p}`).join(", ")}`;
      tr.append(prs);
      tr.append(el("td", "tokens-cell-num", fmtCell(r.roundsToPass)));
      tr.append(el("td", "tokens-cell-num", fmtCell(r.failRate)));
      tr.append(el("td", "tokens-cell-num", fmtCell(r.wallClockH)));
      tbody.append(tr);
    }
    table.append(tbody);
    this.scorecardEl.append(table);

    // The coverage floor caption (slice A's `coverageFloor`, spawn-row
    // half). Stated even when empty — a caption that only speaks when it
    // has bad news reads as vacuous the first time it has none.
    if (sc.floor.missingSpawn.length > 0) {
      const parts = sc.floor.missingSpawn.map(
        (m) => `#${m.pr} (${m.agents.join(", ")})`,
      );
      this.scorecardEl.append(
        el(
          "div",
          "tokens-note",
          `${fmtInt.format(sc.floor.missingSpawn.length)} window(s) PROVEN truncated — a credited ` +
            `delegate's agent-spawn row did not survive the read, so rows for it were dropped: ` +
            `${parts.join("; ")}. Those counters are a lower bound.`
        ),
      );
    } else {
      this.scorecardEl.append(
        el(
          "div",
          "tokens-note",
          "No window is proven truncated: every credited delegate's agent-spawn row survived the read."
        ),
      );
    }
    if (sc.excluded.length > 0) {
      const shown = sc.excluded
        .slice(0, 3)
        .map((x) => `#${x.pr} ${x.block}: ${x.why}`);
      const rest = sc.excluded.length - shown.length;
      this.scorecardEl.append(
        el(
          "div",
          "tokens-note",
          `${fmtInt.format(sc.excluded.length)} lane(s) not on the table — the lane's CLI could not be ` +
            `resolved to one, and nothing is guessed: ${shown.join("; ")}` +
            (rest > 0 ? `; +${fmtInt.format(rest)} more` : "") +
            "."
        ),
      );
    }
  }

  /** Everything this chart is NOT showing. The coverage floor above all — a
   *  chart that does not say where its history starts draws a flat line where
   *  there is simply no data. */
  private renderNotes(series: BucketedSeries, bars: FeatureBars): void {
    this.notesEl.replaceChildren();
    const notes: string[] = [];

    const floor = this.series?.first_ts_ms ?? null;
    if (floor !== null) {
      notes.push(
        `Series since ${fmtTime(floor)} — history starts when the build that writes it first ran ` +
          "against this group, and nothing is rebuilt from transcripts."
      );
    }
    if ((this.series?.skipped ?? 0) > 0) {
      notes.push(
        `${fmtInt.format(this.series!.skipped)} line(s) of the series file would not parse and are ` +
          "not plotted. A corrupt file must not read as a quiet period."
      );
    }
    if (series.baselineOnlyKeys > 0) {
      notes.push(
        `${fmtInt.format(series.baselineOnlyKeys)} usage key(s) have a single row so far. A first row ` +
          "is a baseline, not a bucket's spend, so it draws nothing."
      );
    }
    if (series.resets > 0) {
      notes.push(
        `${fmtInt.format(series.resets)} interval(s) had a cumulative counter go backwards (a ` +
          "transcript cursor revalidation) and are clamped to zero rather than drawn as a cliff."
      );
    }
    if (series.dropped > 0) {
      notes.push(
        `${fmtInt.format(series.dropped)} interval(s) fall outside this window. Counted, never ` +
          "clamped onto an edge, where they would read as spend at a time they did not happen."
      );
    }
    if (bars.unknownAgentTokens > 0) {
      notes.push(
        `${fmtTokens(bars.unknownAgentTokens)} tokens were written by an agent the group roster does ` +
          `not list. Those sit in ${UNATTRIBUTED} — the roster includes exited agents, so this is a real hole.`
      );
    }
    // The audit-backed coverage floor, for the columns slice D will render
    // beside these bars. Surfaced now because the FLOOR is a fact about this
    // chart's honesty whether or not the columns are drawn yet.
    const sc = scorecardColumns(
      this.store.cached,
      bars,
      this.board,
      { firstSpendMs: firstSpendByBar(this.series?.rows ?? [], bars.attribution) }
    );
    if (sc.floorMs !== null) {
      const below = sc.columns.filter((c) => c.belowFloor).length;
      notes.push(
        `Audit rows from ${fmtTime(sc.floorMs)} (${fmtInt.format(sc.auditRows)} read)` +
          (below > 0
            ? ` — ${fmtInt.format(below)} bar(s) have spend older than that, so any review counts for them are a lower bound.`
            : ".")
      );
    }
    if (this.boardStale) {
      notes.push(
        "The task board could not be read on the last refresh, so which feature each " +
          "agent's spend lands on is the previous answer. The series itself is current."
      );
    }
    if (this.readError !== null && this.series !== null) {
      notes.push(`The last refresh failed (${String(this.readError)}); this chart is the previous read.`);
    }
    const orchBar = bars.bars.find((b) => b.id === ORCHESTRATOR);
    if (orchBar && orchBar.total > 0) {
      notes.push(
        "Orchestrator spend is group-wide and is deliberately on no feature bar: there is no " +
          "per-turn PR attribution for a long-lived orchestrator session."
      );
    }

    for (const n of notes) this.notesEl.append(el("div", "tokens-note", n));
  }
}
