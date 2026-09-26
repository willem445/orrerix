import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";

// Every `var(--x)` the stylesheet reads with NO fallback must name a custom
// property something defines — the stylesheet itself, or a `setProperty` in
// `src/`. An undefined one is not an error anywhere: the declaration is
// invalid at computed-value time and the property silently falls back, which
// for `stroke` is `none`. Three of the #3475 token-chart trend lines read
// `--state-success` / `--state-warning`, neither of which exists, and so never
// painted (#3505).

const stripComments = (css: string): string => css.replace(/\/\*[\s\S]*?\*\//g, "");

export function undefinedVars(css: string, runtimeSet: ReadonlySet<string>): { uses: number; missing: string[] } {
  const body = stripComments(css);
  const defined = new Set([...body.matchAll(/(--[A-Za-z0-9_-]+)\s*:/g)].map((m) => m[1]));
  let uses = 0;
  const missing = new Set<string>();
  for (const m of body.matchAll(/var\(\s*(--[A-Za-z0-9_-]+)\s*([,)])/g)) {
    uses++;
    if (m[2] === ",") continue; // has a fallback
    if (!defined.has(m[1]) && !runtimeSet.has(m[1])) missing.add(m[1]);
  }
  return { uses, missing: [...missing].sort() };
}

function runtimeSetVars(): Set<string> {
  const out = new Set<string>();
  for (const f of readdirSync("src")) {
    if (!f.endsWith(".ts")) continue;
    for (const m of readFileSync(`src/${f}`, "utf8").matchAll(/setProperty\(\s*["'`](--[A-Za-z0-9_-]+)["'`]/g)) out.add(m[1]);
  }
  return out;
}

test("every fallback-less var() in styles.css names a defined custom property", () => {
  const runtime = runtimeSetVars();
  assert.ok(runtime.has("--tab-color"), "the runtime scan sees a known setProperty");
  const r = undefinedVars(readFileSync("src/styles.css", "utf8"), runtime);
  assert.ok(r.uses > 1000, `the scan read the stylesheet (${r.uses} var() uses)`);
  assert.deepEqual(r.missing, []);
});

test("the scan catches an undefined property, honours a fallback, and ignores comments", () => {
  const css = ":root { --a: red; }\n.x { color: var(--a); stroke: var(--nope); fill: var(--also-nope, blue); }\n/* var(--in-comment) */";
  assert.deepEqual(undefinedVars(css, new Set()).missing, ["--nope"]);
  assert.deepEqual(undefinedVars(css, new Set(["--nope"])).missing, []);
});
