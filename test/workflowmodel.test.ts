// Structural pins for the workflow model's module split (#3498 F2). `src/workflowmodel.ts`
// is now a re-export barrel over workflowtypes / workflowparse / workflowserialize /
// workflowvalidate / workflowgraph, and the behaviour tests moved with the code into
// test/workflow{parse,serialize,validate,graph}.test.ts. What is left to pin here is the
// property the cut exists for: THE MODULE GRAPH HAS NO CYCLE. A split module that imported
// the barrel (or a sibling that imports it back) would still typecheck and still pass every
// behaviour test, because ES modules tolerate cycles until a module-level constant is read
// before its module has run, which is a load-order bug that shows up later.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { sourceFiles } from "./support/sourcefiles.ts";

const SRC = new URL("../src/", import.meta.url);
const SPLIT = ["workflowtypes.ts", "workflowparse.ts", "workflowserialize.ts", "workflowvalidate.ts", "workflowgraph.ts"];

interface Edge {
  from: string;
  to: string;
  /** `import type` / `export type`: erased before anything runs, so never a load-order edge. */
  typeOnly: boolean;
}

/** Every static relative `import … from` / `export … from` between files of `files`, read off
 *  the source text. Limits, stated because a scan should say what it cannot see: a dynamic
 *  `import()` is lazy and is not read (it cannot take part in a load-order cycle), and a
 *  bare side-effect `import "./x"` is not read either (none exists among these modules). An
 *  `import { type A }` counts as a RUNTIME edge, because type stripping keeps the statement
 *  and so still loads the module. That errs toward reporting a cycle. */
function importEdges(texts: Map<string, string>): Edge[] {
  const edges: Edge[] = [];
  const re = /^\s*(import|export)(\s+type)?\b([^;]*?)\bfrom\s*["'](\.{1,2}\/[^"']+)["']/gm;
  for (const [file, text] of texts) {
    for (const m of text.matchAll(re)) {
      const dir = file.includes("/") ? file.slice(0, file.lastIndexOf("/") + 1) : "";
      let to = new URL(m[4], new URL(dir, "file:///r/")).pathname.replace(/^\/r\//, "");
      if (!to.endsWith(".ts")) to += ".ts";
      if (texts.has(to)) edges.push({ from: file, to, typeOnly: m[2] !== undefined });
    }
  }
  return edges;
}

/** The strongly connected components of size > 1 (plus any self-loop): the cycles (Tarjan). */
function cycles(nodes: Iterable<string>, edges: readonly Edge[]): string[][] {
  const out = new Map<string, string[]>();
  for (const e of edges) out.set(e.from, [...(out.get(e.from) ?? []), e.to]);
  const index = new Map<string, number>();
  const low = new Map<string, number>();
  const stack: string[] = [];
  const onStack = new Set<string>();
  const found: string[][] = [];
  let next = 0;
  const visit = (v: string): void => {
    index.set(v, next);
    low.set(v, next);
    next++;
    stack.push(v);
    onStack.add(v);
    for (const w of out.get(v) ?? []) {
      if (!index.has(w)) {
        visit(w);
        low.set(v, Math.min(low.get(v)!, low.get(w)!));
      } else if (onStack.has(w)) {
        low.set(v, Math.min(low.get(v)!, index.get(w)!));
      }
    }
    if (low.get(v) === index.get(v)) {
      const scc: string[] = [];
      let w: string;
      do {
        w = stack.pop()!;
        onStack.delete(w);
        scc.push(w);
      } while (w !== v);
      if (scc.length > 1 || (out.get(v) ?? []).includes(v)) found.push(scc.sort());
    }
  };
  for (const v of nodes) if (!index.has(v)) visit(v);
  return found;
}

function srcTexts(): Map<string, string> {
  return new Map(sourceFiles(SRC, [".ts"]).map((f) => [f, readFileSync(new URL(f, SRC), "utf8")]));
}

test("the cycle finder finds a cycle, and the edge reader sees the split's real edges", () => {
  // POSITIVE CONTROLS for the two instruments the pins below use. A finder that never
  // reported anything, or a reader that never matched an import, would pass them all.
  const edge = (from: string, to: string): Edge => ({ from, to, typeOnly: false });
  assert.deepEqual(cycles(["a", "b", "c"], [edge("a", "b"), edge("b", "a"), edge("b", "c")]), [["a", "b"]]);
  assert.deepEqual(cycles(["a"], [edge("a", "a")]), [["a"]]);
  assert.deepEqual(cycles(["a", "b", "c"], [edge("a", "b"), edge("b", "c")]), []);

  const texts = srcTexts();
  assert.ok(texts.size > 100, `only ${texts.size} source files read — the walk is broken`);
  const edges = importEdges(texts);
  const has = (from: string, to: string, typeOnly: boolean) =>
    edges.some((e) => e.from === from && e.to === to && e.typeOnly === typeOnly);
  // A multi-line value import, a multi-line `import type`, and a one-line `import type`
  // to a module outside the family: one of each shape the pins rely on.
  assert.ok(has("workflowgraph.ts", "workflowvalidate.ts", false), "a value import between split modules was not read");
  assert.ok(has("workflowgraph.ts", "workflowtypes.ts", true), "an `import type` block was not read as type-only");
  assert.ok(has("workflowvalidate.ts", "selectorknobs.ts", true), "the one-line type import out of the family was not read");
  assert.ok(has("workflowmodel.ts", "workflowgraph.ts", false), "the barrel's re-exports were not read as edges");
});

test("the workflow*.ts modules import each other as a DAG, type imports included", () => {
  const all = srcTexts();
  const family = new Map([...all].filter(([f]) => /^workflow[^/]*\.ts$/.test(f)));
  assert.ok(family.size >= 13, `only ${family.size} workflow*.ts modules found`);
  for (const f of ["workflowmodel.ts", ...SPLIT]) assert.ok(family.has(f), `${f} is missing from the scan`);
  assert.deepEqual(cycles(family.keys(), importEdges(family)), []);
});

test("no module on a load-order cycle anywhere in src/ is one of the split modules or the barrel", () => {
  // The whole-tree graph DOES have cycles (docs/design/code-metrics.md counts one big
  // strongly-connected component, pane and orchestration among them), so this pin is scoped
  // to what the split owns: none of those cycles passes through the barrel or a split module
  // at LOAD time. Type-only edges are left out on purpose, since they are erased before
  // anything runs. `workflowvalidate.ts`'s `import type { KnobStates }` is the one edge the
  // family has into that component, and it is exactly that kind.
  const texts = srcTexts();
  const runtime = importEdges(texts).filter((e) => !e.typeOnly);
  const touching = cycles(texts.keys(), runtime).filter((scc) =>
    scc.some((f) => f === "workflowmodel.ts" || SPLIT.includes(f)),
  );
  assert.deepEqual(touching, []);
});

test("no split module imports the barrel, and the barrel declares nothing of its own", () => {
  const texts = srcTexts();
  const edges = importEdges(texts);
  for (const f of SPLIT) {
    const intoBarrel = edges.filter((e) => e.from === f && e.to === "workflowmodel.ts");
    assert.deepEqual(intoBarrel, [], `${f} imports the barrel. Import from workflowtypes.ts or a sibling instead.`);
  }
  // The barrel is comments plus `export { … } from` / `export type { … } from` lists, and
  // no `export *`: a star re-export would publish every internal a sibling needed widened.
  const code = texts
    .get("workflowmodel.ts")!
    .split(/\r?\n/)
    .filter((l) => l.trim() !== "" && !l.trimStart().startsWith("//"));
  assert.ok(code.length > 100, "the barrel's re-export lists were not read");
  const stray = code.filter((l) => !/^(export( type)? \{|  [A-Za-z_$][\w$]*,|\} from "\.\/workflow\w+\.ts";)$/.test(l));
  assert.deepEqual(stray, [], "the barrel carries something other than re-export lists");
});
