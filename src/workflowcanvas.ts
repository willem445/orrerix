// The workflow pane's CANVAS, split out of workflowview.ts (#3498 F3): the graph drawing
// (`renderGraph`), its hit-testing and drop-target rules, the drag and connect gestures, the
// ✕ that erases an advisory edge or a gate seat, and the SVG node/ghost/marker builders.
//
// A satellite of `WorkflowView`, not a model. It owns the two transient gesture fields
// (`dragging`, `connecting`) and reads the rest of the pane through `WorkflowViewApi`
// (workflowviewapi.ts), never through workflowview.ts, so the workflow* import graph stays a
// DAG (test/workflowmodel.test.ts). DOM glue, hand-validated like the view. The geometry is
// workflowlayout.ts and the graph edits are workflowgraph.ts; this is the DOM half. The canvas
// draws inside the pane's own body and nothing here reaches a PTY resize (CLAUDE.md
// constraint 1). Design note: docs/design/workflows.md; layout conventions:
// docs/design/module-layout.md.

import {
  connectBlocks,
  disconnectBlocks,
  connectionError,
  connectToGate,
  disconnectFromGate,
  gateConnectionError,
  isBlockKind,
  isWorkflowCli,
  type GraphNode,
} from "./workflowmodel";
import {
  layoutEquals,
  withPosition,
  resolvePositions,
  rectOf,
  outPort,
  inPort,
  edgePath,
  edgeMidpoint,
  hitTestNodes,
  hitTestEdges,
  hitTestDropTarget,
  gateRect,
  GATE_KEY,
  blockKey,
  ghostKey,
  NODE_W,
  NODE_H,
  PAD,
  type Point,
  type Rect,
} from "./workflowlayout";
import type { Selection } from "./workflowpane";
import { showToast } from "./toast";
import { IDENTITY, SEMANTIC } from "./theme.ts";
import { el } from "./workflowinspector";
import type { WorkflowViewApi, WorkflowCanvasApi } from "./workflowviewapi";

const svg = (tag: string): SVGElement => document.createElementNS("http://www.w3.org/2000/svg", tag);

// The graph's geometry now lives in `workflowlayout.ts` (imported above) — fixed, not
// measured, and pure, which is what lets the hit-testing and edge-routing be tested as
// arithmetic instead of by dragging things around and squinting.

export class WorkflowCanvas implements WorkflowCanvasApi {
  // Canvas interaction state. All three are transient — none of them is ever serialized, and
  // the model never learns they existed.
  /** A node being dragged: which one, and where the pointer grabbed it. */
  private dragging: { key: string; id: string; grab: Point; at: Point } | null = null;
  /** An edge being drawn: the block it left, and where the pointer is now. */
  private connecting: { from: string; at: Point } | null = null;

  constructor(private readonly view: WorkflowViewApi) {}

  // ---------- the canvas (#222 v2: it EDITS the file now) ----------
  //
  // The graph was read-only in v1, on the reasoning that a canvas which can corrupt the file is
  // worse than no canvas. The human demoed it and asked for an editable one. So it edits — and
  // the original reasoning is ANSWERED rather than abandoned: every gesture goes through the
  // pure model (`connectBlocks`, `addBlock`, `removeBlockAt`) and out through the same
  // canonical formatter as every other edit. The canvas cannot express anything the YAML
  // can't, it cannot write a position into the workflow, and it cannot invent an id. It is a
  // second way to EDIT the file, not a second source of truth.
  //
  // Drag a node (position → the LAYOUT file, never the workflow) · drag from a node's port to
  // another node to draw an advisory edge · click an edge to select it, ✕ to erase it · +Block
  // to add one (it asks for the id) · Delete to remove what's selected.

  /** Every node's position right now: stored where the human has dragged one, computed
   *  everywhere else, and overridden by the drag in flight. */
  positions(): Map<string, Point> {
    const pos = resolvePositions(this.view.analysis.graph, this.view.layout, this.ghosts());
    if (this.dragging) pos.set(this.dragging.key, this.dragging.at);
    return pos;
  }

  private nodeRects(): Map<string, Rect> {
    return new Map([...this.positions()].map(([k, p]) => [k, rectOf(p)] as const));
  }

  /** The names an edge mentions that no block answers to. Drawn, because a graph that quietly
   *  omitted them would disagree with the file it exists to show you. */
  private ghosts(): string[] {
    const g = this.view.analysis.graph;
    const known = new Set(g.nodes.map((n) => n.block.id).filter(Boolean));
    return [...new Set(g.edges.flatMap((e) => [e.from, e.to]).filter((id) => id && !known.has(id)))];
  }

  /** The block (or ghost) a name resolves to. A duplicate id draws to the FIRST row answering
   *  to it — that is a validation error either way, and drawing to one of them beats drawing to
   *  neither. */
  private keyOf(id: string): string | null {
    const n = this.view.analysis.graph.nodes.find((x) => x.block.id === id);
    if (n) return blockKey(n.index);
    return this.ghosts().includes(id) ? ghostKey(id) : null;
  }

  /** Pointer → canvas coordinates. The SVG renders at natural size (no zoom, no viewBox
   *  scaling), so this is a translation and nothing more — which is why there is no transform
   *  maths anywhere else in here to get wrong. */
  private canvasPoint(e: PointerEvent, root: SVGElement): Point {
    const r = (root as unknown as HTMLElement).getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  }

  /** Every line on the canvas as GEOMETRY, in render order, each carrying the SELECTION a
   *  click on it makes — the list the pure hit-test is asked about.
   *
   *  Gate lines are in it since #1388. They were drawn and not clickable, which made the one
   *  enforced thing on the canvas the one thing you could not point at: the amber line said a
   *  reviewer gates the merge and offered no way to ask about it or take it back, so the only
   *  route to un-gating a reviewer was the form's checkbox or the YAML. */
  private drawnEdges(
    rects: ReadonlyMap<string, Rect>,
    gr: Rect | null
  ): { sel: Selection; geom: { from: Point; to: Point } }[] {
    const out: { sel: Selection; geom: { from: Point; to: Point } }[] = [];
    for (const e of this.view.analysis.graph.edges) {
      const a = this.keyOf(e.from);
      const b = this.keyOf(e.to);
      const ra = a ? rects.get(a) : undefined;
      const rb = b ? rects.get(b) : undefined;
      if (!ra || !rb) continue;
      out.push({
        sel: { kind: "edge", from: e.from, to: e.to },
        geom: { from: outPort(ra), to: inPort(rb) },
      });
    }
    for (const rid of this.gateReviewers()) {
      const rKey = this.keyOf(rid);
      const ra = rKey ? rects.get(rKey) : undefined;
      if (!ra || !gr) continue;
      out.push({ sel: { kind: "gate-edge", reviewer: rid }, geom: { from: outPort(ra), to: inPort(gr) } });
    }
    return out;
  }

  /** The reviewers the merge gate names, or none. One reader, so "is there a gate at all" is
   *  asked in one place rather than at each of the four sites that draw or hit-test its lines. */
  private gateReviewers(): readonly string[] {
    return this.view.analysis.graph.gates[0]?.reviewers ?? [];
  }

  /** Where the gate box is, or null when the file declares no gate. The ONE construction
   *  site: the box is drawn from this, hit-tested from this, and the gate lines terminate on
   *  this, and three copies of the same sums is how a drop target stops matching its picture. */
  private gateRectOf(rects: ReadonlyMap<string, Rect>): Rect | null {
    const gate = this.view.analysis.graph.gates[0];
    return gate ? gateRect(rects.values(), gate.reviewers.length) : null;
  }

  /** Every DROP target on the canvas, in draw order — the nodes and ghosts, then the gate box,
   *  which is drawn last and therefore wins an overlap under `hitTestDropTarget`'s rule. */
  private dropRects(rects: ReadonlyMap<string, Rect> = this.nodeRects()): Map<string, Rect> {
    const out = new Map(rects);
    const gr = this.gateRectOf(rects);
    if (gr) out.set(GATE_KEY, gr);
    return out;
  }

  /** Why releasing the rubber band on `key` would NOT connect, or null when it would.
   *
   *  ONE definition, asked twice: while the band is in flight, to colour the affordance, and
   *  again on release, to say the reason out loud. A canvas that lights a target up and then
   *  refuses the drop is worse than one that never lit it — it made a promise. Both answers
   *  come from the pure model (`connectionError` / `gateConnectionError`), so neither can
   *  drift from what the findings strip says about the same pair. */
  private dropError(key: string, from: string): string | null {
    const w = this.view.analysis.workflow;
    if (key === GATE_KEY) return gateConnectionError(w, from);
    // A ghost is the ABSENCE of a block, so `connectionError` answers for it too — "that
    // block doesn't exist" is exactly right, and inventing a second sentence here is how the
    // canvas ends up refusing in words the validator never uses.
    if (key.startsWith("g:")) return connectionError(w, from, key.slice(2));
    return connectionError(w, from, this.view.analysis.workflow.blocks[Number(key.slice(2))]?.id ?? "");
  }

  renderGraph(): void {
    const g = this.view.analysis.graph;

    const bar = el("div", "wf-graph-bar");
    const addBtn = document.createElement("button");
    addBtn.className = "wf-btn";
    addBtn.textContent = "+ Block";
    addBtn.disabled = this.view.syntaxBroken();
    addBtn.addEventListener("click", () => void this.view.createBlock());
    bar.append(
      addBtn,
      el(
        "span",
        "wf-graph-hint",
        "Drag a node to move it · drag from its ● to another node's ● (or onto the merge gate, from a reviewer) to connect · click an edge to select it · double-click the canvas to add a block"
      )
    );
    const legend = el("div", "wf-legend");
    legend.append(
      el("span", "wf-legend-item wf-legend-edge", "— advisory edge (the declared path)"),
      el("span", "wf-legend-item wf-legend-gate", "-- enforced gate (blocks the merge)")
    );
    bar.append(legend);

    if (!g.nodes.length) {
      this.view.graphPane.replaceChildren(bar, el("div", "wf-hint", "No blocks yet — “+ Block” adds one."));
      return;
    }

    const pos = this.positions();
    const rects = this.nodeRects();
    const ghosts = this.ghosts();

    // The gate hangs off the reviewers it names, to the right of everything else. It is not a
    // DRAGGABLE node — it is not a block, it is a rule ABOUT blocks, so it has no position of
    // its own, no roster row and no entry in the layout file, and dragging it would imply it
    // can be moved in a graph it is not part of. It IS a drop target (#1388): a reviewer's
    // out-port may be released on it, which adds that id to `gates.merge.reviewers` and
    // nothing else. Wireable one way, still not a block — see docs/design/content-panes.md.
    const gate = g.gates[0];
    const right = Math.max(...[...pos.values()].map((p) => p.x + NODE_W), PAD);
    // The box's rect comes from `gateRect` (workflowlayout.ts) rather than being arithmetic
    // inlined here, because since #1388 the box is a DROP TARGET as well as a picture: the
    // rect that is drawn and the rect that is hit-tested have to be the same one, and a
    // second copy of the sums is how they stop being.
    const gr = this.gateRectOf(rects);

    const bottom = Math.max(...[...pos.values()].map((p) => p.y + NODE_H), gr ? gr.y + gr.h : PAD);
    const width = (gr ? gr.x + gr.w : right) + PAD * 4;
    const height = bottom + PAD * 4;

    // What the rubber band in flight is over, and whether releasing there would connect. The
    // affordance half of "refuse before the gesture completes": the target the drop will land
    // on says so while the human is still holding it, and says which way it will go.
    const hover = this.connecting
      ? hitTestDropTarget(this.dropRects(rects), this.connecting.at)
      : null;
    const hoverOk = hover ? !this.dropError(hover, this.connecting!.from) : false;
    const dropClass = (key: string): string =>
      hover === key ? (hoverOk ? " wf-drop-ok" : " wf-drop-bad") : "";

    const root = svg("svg");
    root.setAttribute("class", "wf-graph-svg");
    root.setAttribute("width", String(width));
    root.setAttribute("height", String(height));

    const defs = svg("defs");
    // An SVG <marker>'s fill is a presentation attribute on an element the stylesheet does
    // not reach, so these two take their values from theme.ts directly rather than through a
    // custom property (#879 slice B). They mirror `.wf-edge` / `.wf-edge-gate` in styles.css:
    // a plain edge is a faint rule, a gate edge is the identity amber the gate lane uses.
    defs.append(
      arrowMarker("wf-arrow", SEMANTIC.inkFaint),
      arrowMarker("wf-arrow-gate", IDENTITY.amber)
    );
    root.append(defs);

    // ---- advisory edges: solid, selectable, erasable ----
    for (const e of g.edges) {
      const aKey = this.keyOf(e.from);
      const bKey = this.keyOf(e.to);
      const a = aKey ? rects.get(aKey) : undefined;
      const b = bKey ? rects.get(bKey) : undefined;
      if (!a || !b) continue;
      const from = outPort(a);
      const to = inPort(b);
      const selected =
        this.view.selection.kind === "edge" && this.view.selection.from === e.from && this.view.selection.to === e.to;

      const group = svg("g");
      group.setAttribute("class", `wf-edge-g${selected ? " selected" : ""}`);
      const path = svg("path");
      path.setAttribute("d", edgePath(from, to));
      path.setAttribute("class", e.resolved ? "wf-edge" : "wf-edge wf-edge-broken");
      path.setAttribute("marker-end", "url(#wf-arrow)");
      group.append(path);

      group.append(eraseButton(from, to, () => this.eraseEdge(e.from, e.to)));
      root.append(group);
    }

    // ---- the edge being drawn ----
    if (this.connecting) {
      const fromKey = this.keyOf(this.connecting.from);
      const a = fromKey ? rects.get(fromKey) : undefined;
      if (a) {
        const rubber = svg("path");
        rubber.setAttribute("d", edgePath(outPort(a), this.connecting.at));
        rubber.setAttribute("class", "wf-edge wf-edge-draft");
        rubber.setAttribute("marker-end", "url(#wf-arrow)");
        root.append(rubber);
      }
    }

    // ---- nodes ----
    for (const n of g.nodes) {
      const selected = this.view.selection.kind === "block" && this.view.selection.index === n.index;
      root.append(
        nodeGroup(rects.get(blockKey(n.index))!, n, selected, dropClass(blockKey(n.index)))
      );
    }
    for (const id of ghosts) {
      root.append(ghostGroup(rects.get(ghostKey(id))!, id, dropClass(ghostKey(id))));
    }

    // ---- the ENFORCED gate ----
    if (gate && gr) {
      const port = inPort(gr);
      for (const rid of gate.reviewers) {
        const rKey = this.keyOf(rid);
        const a = rKey ? rects.get(rKey) : undefined;
        if (!a) continue;
        const from = outPort(a);
        const selected = this.view.selection.kind === "gate-edge" && this.view.selection.reviewer === rid;
        // Wrapped in the SAME `.wf-edge-g` group an advisory edge gets, so hover, selection
        // and the ✕ behave identically on both. A seat on the gate is erased the way an edge
        // is because it is the same gesture on the same kind of line — what differs is what it
        // MEANS, which is what the colour, the dashes and the inspector panel are for.
        const group = svg("g");
        group.setAttribute("class", `wf-edge-g${selected ? " selected" : ""}`);
        const line = svg("path");
        line.setAttribute("d", edgePath(from, port));
        line.setAttribute("class", "wf-edge wf-edge-gate");
        line.setAttribute("marker-end", "url(#wf-arrow-gate)");
        group.append(line);
        group.append(eraseButton(from, port, () => this.eraseGateEdge(rid)));
        root.append(group);
      }
      const box = svg("rect");
      box.setAttribute("x", String(gr.x));
      box.setAttribute("y", String(gr.y));
      box.setAttribute("width", String(gr.w));
      box.setAttribute("height", String(gr.h));
      box.setAttribute("rx", "8");
      box.setAttribute("class", `wf-gate-box${dropClass(GATE_KEY)}`);
      box.addEventListener("pointerdown", (ev) => {
        ev.stopPropagation();
        this.view.selectItem({ kind: "gate" });
      });
      root.append(box);
      root.append(text(gr.x + 12, gr.y + 22, "⛔ merge gate", "wf-gate-title"));
      root.append(
        text(
          gr.x + 12,
          gr.y + 40,
          gate.require === "threshold"
            ? `${gate.threshold ?? "?"} of ${gate.reviewers.length} must PASS`
            : `all ${gate.reviewers.length} must PASS`,
          "wf-gate-sub"
        )
      );
      // The IN-port, drawn on the gate exactly as it is on a block — because since #1388 it
      // means the same thing on both: this is where a line arrives, and where one may be let
      // go. Drawn AFTER the box so the box's fill cannot cover it.
      const inp = svg("circle");
      inp.setAttribute("cx", String(port.x));
      inp.setAttribute("cy", String(port.y));
      inp.setAttribute("r", "3");
      inp.setAttribute("class", `wf-port wf-port-in wf-port-gate${dropClass(GATE_KEY)}`);
      root.append(inp);
    }

    // Double-click on empty canvas → a block, THERE. (rev-15 minor: `createBlock(at)` took a
    // point no caller ever passed, and its comment promised a gesture that did not exist. It
    // does now — it is the first thing anyone tries on a canvas, and it was one line to honour.)
    root.addEventListener("dblclick", (ev) => {
      const pt = this.canvasPoint(ev as unknown as PointerEvent, root);
      if (hitTestNodes(this.nodeRects(), pt)) return; // double-clicking a node is not "add here"
      void this.view.createBlock(pt);
    });
    root.addEventListener("pointerdown", (ev) => this.onCanvasDown(ev, root));
    root.addEventListener("pointermove", (ev) => this.onCanvasMove(ev, root));
    root.addEventListener("pointerup", (ev) => this.onCanvasUp(ev, root));
    root.addEventListener("pointercancel", () => {
      this.dragging = null;
      this.connecting = null;
      this.renderGraph();
    });

    const scroll = el("div", "wf-graph-scroll");
    scroll.append(root as unknown as HTMLElement);
    this.view.graphPane.replaceChildren(bar, scroll);
  }

  /** Where a gesture begins: on a node's PORT (draw an edge), on a node (move it, select it),
   *  on an edge (select it), or on nothing (deselect). */
  private onCanvasDown(e: PointerEvent, root: SVGElement): void {
    if (e.button !== 0 || this.view.syntaxBroken()) return;
    const pt = this.canvasPoint(e, root);
    const rects = this.nodeRects();
    const key = hitTestNodes(rects, pt);

    if (key?.startsWith("b:")) {
      const index = Number(key.slice(2));
      const block = this.view.analysis.workflow.blocks[index];
      const rect = rects.get(key)!;
      const port = outPort(rect);

      if (Math.hypot(pt.x - port.x, pt.y - port.y) <= PORT_HIT && block?.id) {
        // An edge is a pair of IDS, so a block with no id cannot be an endpoint. Offering the
        // gesture would only manufacture the dangling reference the validator then complains
        // about — the file would be describing a mistake the canvas talked you into.
        this.connecting = { from: block.id, at: pt };
        capturePointer(root, e);
        this.renderGraph();
        return;
      }

      this.dragging = {
        key,
        id: block?.id ?? "",
        grab: { x: pt.x - rect.x, y: pt.y - rect.y },
        at: { x: rect.x, y: rect.y },
      };
      capturePointer(root, e);
      // THE #880 GESTURE. This handler always did the selecting; what it never did was bring the
      // editor into view, because the editor was behind a tab and only the gate box remembered
      // to switch to it. There is no tab now and no second thing to remember: `selectItem`
      // refreshes the roster, the inspector and the canvas together, so the block's editor
      // appears beside the node under the pointer.
      this.view.selectItem({ kind: "block", index });
      return;
    }

    // Not a node. An edge, then? THIS is where the pure hit-test earns its keep: an edge is a
    // 1.5px line and nobody can hit that with a mouse — the tolerance is what makes it
    // clickable at all, and it is arithmetic, so it is tested rather than eyeballed.
    const drawn = this.drawnEdges(rects, this.gateRectOf(rects));
    const hit = hitTestEdges(
      drawn.map((d) => d.geom),
      pt
    );
    this.view.selectItem(hit !== null ? drawn[hit]!.sel : { kind: "workflow" });
  }

  private onCanvasMove(e: PointerEvent, root: SVGElement): void {
    if (!this.dragging && !this.connecting) return;
    const pt = this.canvasPoint(e, root);
    if (this.dragging) {
      this.dragging.at = { x: pt.x - this.dragging.grab.x, y: pt.y - this.dragging.grab.y };
    }
    if (this.connecting) this.connecting.at = pt;
    this.renderGraph();
  }

  private onCanvasUp(e: PointerEvent, root: SVGElement): void {
    const pt = this.canvasPoint(e, root);

    if (this.dragging) {
      const { id, at } = this.dragging;
      this.dragging = null;
      if (id) {
        // A drag writes the LAYOUT file and nothing else. The workflow is not re-serialized, the
        // dirty flag does not move, and your teammate's `git pull` does not show a change to the
        // logic because you nudged a box (§4 — the thing Dify, ComfyUI and Langflow all get
        // wrong by embedding x/y in the semantic file).
        const moved = withPosition(this.view.layout, id, { x: Math.max(0, at.x), y: Math.max(0, at.y) });
        if (!layoutEquals(moved, this.view.layout)) {
          this.view.layout = moved;
          void this.view.saveLayout();
        }
      }
      this.renderGraph();
      return;
    }

    if (this.connecting) {
      const from = this.connecting.from;
      this.connecting = null;
      // `hitTestDropTarget`, not `hitTestNodes` (#1387): the drop used to be the node's BODY,
      // and the in-port the arrowhead points at is drawn on the body's left EDGE — so a
      // release aimed at the target the picture offers landed on nothing at all. It also
      // carries the gate box (#1388), which is why this is one hit-test and not two.
      const key = hitTestDropTarget(this.dropRects(), pt);
      if (key) {
        // Refused BEFORE the edge exists, with the reason. A canvas that lets you complete the
        // gesture and only then tells you the edge was invalid has wasted the gesture and left
        // you to undo it — and a canvas that says nothing at all, which is what a release on
        // the gate used to do, is worse still: nothing happened and nothing said why.
        const err = this.dropError(key, from);
        if (err) showToast(err, "info");
        else if (key === GATE_KEY) {
          this.view.mutate((next) => Object.assign(next, connectToGate(next, from)));
        } else if (key.startsWith("b:")) {
          const to = this.view.analysis.workflow.blocks[Number(key.slice(2))]?.id ?? "";
          this.view.mutate((next) => Object.assign(next, connectBlocks(next, from, to)));
        }
        // No trailing else: a ghost is the only other key shape, and `dropError` has already
        // refused it out loud. Spelling the block branch out rather than making it the
        // fallthrough keeps a future third target from silently being treated as a block.
      }
      this.renderGraph();
    }
  }

  /** Erase one edge. No confirm: an edge is one gesture to redraw, and a dialog for something
   *  that cheap is a dialog people learn to click through. A BLOCK is different — it carries a
   *  prompt, a model, a seat on the gate — and deleting one still asks. */
  eraseEdge(from: string, to: string): void {
    // No selection tidy-up here: `mutate` re-renders the inspector, and an edge the workflow no
    // longer declares is exactly the case `inspectorTarget` falls back on — so the selection
    // lands on the workflow's own settings, once, by the rule rather than by a second check
    // that had to stay in step with it.
    this.view.mutate((next) => Object.assign(next, disconnectBlocks(next, from, to)));
  }

  /** Take a reviewer's seat off the merge gate — `eraseEdge`'s gate mirror, and no confirm
   *  for the same reason: it is one gesture to redraw.
   *
   *  The toast is the one thing this has that `eraseEdge` doesn't. `disconnectFromGate` may
   *  lower a `threshold` to keep the gate satisfiable (the engine refuses the WHOLE file over
   *  "3 passes from 2 reviewers"), and a policy number that changes itself without saying so
   *  is a number the human finds in `git diff` later and cannot account for. */
  eraseGateEdge(reviewer: string): void {
    const before = this.view.analysis.workflow.gates.merge?.threshold;
    this.view.mutate((next) => Object.assign(next, disconnectFromGate(next, reviewer)));
    const after = this.view.analysis.workflow.gates.merge?.threshold;
    if (after !== undefined && after !== before) {
      showToast(
        `Threshold lowered to ${after} — the gate can only require passes from the reviewers it names.`,
        "info"
      );
    }
  }

  /** Delete whatever is selected — the keyboard half of the canvas. A canvas you can only
   *  operate with a mouse is a canvas that is tiring to use. */
  deleteSelection(): void {
    if (this.view.selection.kind === "edge") {
      this.eraseEdge(this.view.selection.from, this.view.selection.to);
      return;
    }
    if (this.view.selection.kind === "gate-edge") {
      this.eraseGateEdge(this.view.selection.reviewer);
      return;
    }
    if (this.view.selection.kind === "block") {
      const block = this.view.analysis.workflow.blocks[this.view.selection.index];
      if (block) void this.view.deleteBlock(block, this.view.selection.index);
    }
  }
}

/** How close to a node's out-port a press must land to mean "draw an edge" rather than "move
 *  the node". Generous — the port is a 5px dot, and the two gestures start in the same place. */
const PORT_HIT = 12;

/** Take pointer capture, BEST EFFORT — never letting it abort the gesture it belongs to.
 *
 *  `setPointerCapture` throws (`NotFoundError`) for a pointer id the browser doesn't consider
 *  active, and it is called from the handler that also SELECTS the block. An exception here
 *  would therefore skip the selection and re-create, exactly, the dead click #880 exists to fix
 *  — a click that changes nothing the human can see. That trade is never worth taking, because
 *  the capture is close to decorative anyway: the very next thing every caller does is
 *  `renderGraph()`, which replaces the SVG root the capture was taken on, so the capture is
 *  released a line later regardless and the drag continues on the new root's own listeners. */
function capturePointer(root: SVGElement, e: PointerEvent): void {
  try {
    root.setPointerCapture(e.pointerId);
  } catch {
    // See above: the gesture is worth more than the capture.
  }
}

// ---------- SVG helpers ----------

function arrowMarker(id: string, color: string): SVGElement {
  const m = svg("marker");
  m.setAttribute("id", id);
  m.setAttribute("viewBox", "0 0 10 10");
  m.setAttribute("refX", "9");
  m.setAttribute("refY", "5");
  m.setAttribute("markerWidth", "6");
  m.setAttribute("markerHeight", "6");
  m.setAttribute("orient", "auto-start-reverse");
  const p = svg("path");
  p.setAttribute("d", "M 0 0 L 10 5 L 0 10 z");
  p.setAttribute("fill", color);
  m.append(p);
  return m;
}

function text(x: number, y: number, s: string, cls: string): SVGElement {
  const t = svg("text");
  t.setAttribute("x", String(x));
  t.setAttribute("y", String(y));
  t.setAttribute("class", cls);
  t.textContent = s;
  return t;
}

/** Clip a label to the node box. Cheaper and steadier than measuring: the box is a fixed
 *  width, so a fixed budget is the honest bound. */
const clip = (s: string, max: number): string => (s.length > max ? s.slice(0, max - 1) + "…" : s);

/** The ✕ that erases a line, hung off the CURVE's midpoint — on a line that doubles back
 *  (the reviewer → worker rework loop, a real workflow) the straight-line middle is nowhere
 *  near the line you can see, and a ✕ floating in empty space is a ✕ nobody trusts.
 *
 *  One builder for both kinds of line since #1388: an advisory edge and a gate seat are
 *  erased by the same gesture on the same-shaped control, and two copies of it is how one of
 *  them ends up with a delete button that hangs somewhere else. */
function eraseButton(from: Point, to: Point, erase: () => void): SVGElement {
  const mid = edgeMidpoint(from, to);
  const del = svg("g");
  del.setAttribute("class", "wf-edge-del");
  const disc = svg("circle");
  disc.setAttribute("cx", String(mid.x));
  disc.setAttribute("cy", String(mid.y));
  disc.setAttribute("r", "9");
  const glyph = text(mid.x, mid.y + 4, "✕", "wf-edge-del-x");
  glyph.setAttribute("text-anchor", "middle");
  del.append(disc, glyph);
  del.addEventListener("pointerdown", (ev) => {
    ev.stopPropagation(); // this is a click on the ✕, not a canvas gesture
    erase();
  });
  return del;
}

/** One block, as a draggable, connectable node. `drop` is the affordance for a rubber band
 *  currently over this node: empty, or the class that says whether releasing here connects. */
function nodeGroup(r: Rect, n: GraphNode, selected: boolean, drop = ""): SVGElement {
  const bad = !n.known || !isWorkflowCli(n.block.cli);
  const g = svg("g");
  g.setAttribute("class", `wf-node-g${selected ? " selected" : ""}`);

  const box = svg("rect");
  box.setAttribute("x", String(r.x));
  box.setAttribute("y", String(r.y));
  box.setAttribute("width", String(r.w));
  box.setAttribute("height", String(r.h));
  box.setAttribute("rx", "8");
  box.setAttribute("class", `wf-node wf-node-${isBlockKind(n.block.kind) ? n.block.kind : "unknown"}`);
  g.append(box);
  g.append(text(r.x + 12, r.y + 21, clip(n.block.name || n.block.id || "(no id)", 20), "wf-node-title"));
  g.append(
    text(
      r.x + 12,
      r.y + 38,
      clip(`${bad ? "⚠ " : ""}${n.block.kind || "?"} · ${n.block.cli || "?"}`, 22),
      "wf-node-sub"
    )
  );

  // The ports. The OUT port is the handle you drag an edge from, so it is drawn — a gesture
  // nobody can see is a gesture nobody performs. The IN port is drawn too, smaller, because an
  // arrow that arrives somewhere unmarked looks like it is pointing at the box rather than
  // connecting to it. An id-less block gets no out-port at all: it cannot be an edge's endpoint
  // (an edge is a pair of ids), and offering the handle would be offering a broken promise.
  if (n.block.id) {
    const out = svg("circle");
    const p = outPort(r);
    out.setAttribute("cx", String(p.x));
    out.setAttribute("cy", String(p.y));
    out.setAttribute("r", "5");
    out.setAttribute("class", "wf-port wf-port-out");
    g.append(out);
  }
  const inp = svg("circle");
  const ip = inPort(r);
  inp.setAttribute("cx", String(ip.x));
  inp.setAttribute("cy", String(ip.y));
  inp.setAttribute("r", "3");
  // #1387: the in-port is also the DROP target now, and it says so while a band is over it.
  // The affordance and the hit-test are the same answer (`dropError`), so the dot that lights
  // up green is the one that will actually take the edge.
  inp.setAttribute("class", `wf-port wf-port-in${drop}`);
  g.append(inp);
  return g;
}

/** A name an edge mentions that no block answers to. Dashed, unmovable, unconnectable — it is
 *  not a block, it is the ABSENCE of one, and it disappears the moment the file stops
 *  mentioning it.
 *
 *  It still ANSWERS a rubber band held over it (rev-lead round 1, N3), because it is a drop
 *  target — it sits in `dropRects()` with the same in-port tolerance a real node has — and a
 *  target that stays dark while you hover it and then refuses on release is the quiet half of
 *  the same broken promise #1387 is about. The answer is only ever "no": `dropError` asks
 *  `connectionError` about a name no block answers to, which cannot return null (pinned in
 *  test/workflowgraph.test.ts, "an edge that would be nonsense is refused before it is
 *  drawn"). So there is no `wf-drop-ok` rule for a ghost, and no in-port dot either — a dot
 *  sitting there permanently would offer a connection that can never be made. */
function ghostGroup(r: Rect, id: string, drop = ""): SVGElement {
  const g = svg("g");
  g.setAttribute("class", "wf-node-g");
  const box = svg("rect");
  box.setAttribute("x", String(r.x));
  box.setAttribute("y", String(r.y));
  box.setAttribute("width", String(r.w));
  box.setAttribute("height", String(r.h));
  box.setAttribute("rx", "8");
  box.setAttribute("class", `wf-node wf-node-ghost${drop}`);
  g.append(box);
  g.append(text(r.x + 12, r.y + 21, clip(id, 20), "wf-node-title"));
  g.append(text(r.x + 12, r.y + 38, "no such block", "wf-node-sub"));
  return g;
}
