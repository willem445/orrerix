// Text -> model for the repo's workflow file (#222; split out of workflowmodel.ts by #3498 F2):
// the hand-rolled YAML subset (the block/flow readers, plus the scalar emitter that parse's
// own findings quote values through) and `parseWorkflow`. Imports only workflowtypes.ts.
// workflowserialize.ts builds on it, never the other way round. Design note:
// docs/design/workflows.md; module map: docs/design/architecture.md.

import {
  WORKFLOW_VERSION,
  WIP_STATUSES,
  INTAKE_LABEL_KEYS,
} from "./workflowtypes.ts";
import type {
  YamlValue,
  WorkflowBlock,
  WorkflowEdge,
  MergeGate,
  RoutingRule,
  WorkflowIntake,
  WorkflowIntakeLabels,
  WorkflowMergeQueue,
  WorkflowDriver,
  WorkflowResource,
  WorkflowWip,
  WorkflowTriage,
  WorkflowBoard,
  Workflow,
  Finding,
} from "./workflowtypes.ts";

// ---------- YAML subset: reading ----------

interface RawLine {
  /** 0-based index into the source lines. */
  i: number;
  /** Leading-space count. */
  indent: number;
  /** The line with its indent and any trailing comment removed. */
  text: string;
}

/** Strip a `#` comment, ignoring one inside a quoted scalar. A `#` that is not preceded
 *  by whitespace is NOT a comment in YAML (`a#b` is the scalar `a#b`), which matters here
 *  because a model or a branch can legitimately contain one. */
export function stripComment(line: string): string {
  let quote: '"' | "'" | null = null;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (quote) {
      if (c === "\\" && quote === '"') i++;
      else if (c === quote) quote = null;
      continue;
    }
    if (c === '"' || c === "'") {
      quote = c;
      continue;
    }
    if (c === "#" && (i === 0 || /\s/.test(line[i - 1]!))) return line.slice(0, i);
  }
  return line;
}

export const indentOf = (line: string): number => line.length - line.trimStart().length;

class YamlReader {
  /** Cursor into `raw` — the next line not yet consumed. */
  private i = 0;
  readonly findings: Finding[] = [];
  private readonly raw: string[];

  constructor(raw: string[]) {
    this.raw = raw;
  }

  private err(line: number, message: string): void {
    this.findings.push({ severity: "error", code: "yaml-syntax", message, line: line + 1 });
  }

  /** The next SIGNIFICANT line (blank and comment-only lines skipped), without consuming
   *  it. Callers consume by setting the cursor past the line they took.
   *
   *  TABS. YAML forbids a tab in indentation, and this pane must say so — because the
   *  backend validator (a real parser) will refuse the same file, and a pane that reports
   *  `valid` on a file the spawn then rejects is worse than one that reports nothing.
   *  The line is skipped with a finding rather than aborting the read: the rest of the
   *  file still opens, which is this module's whole contract.
   *
   *  (A tab INSIDE a block scalar is content, not indentation, and stays that way —
   *  `blockScalar` reads `this.raw` directly and never comes through here.) */
  private peek(): RawLine | null {
    for (let j = this.i; j < this.raw.length; j++) {
      const raw = this.raw[j]!;
      // Test the RAW leading whitespace. The previous form — `raw.trimStart().startsWith("\t")`
      // — could never fire, because trimStart() strips the very tab it was looking for
      // (rev-5 F2): the guard was dead, and a fully tab-indented file validated clean.
      if (/^[ ]*\t/.test(raw)) {
        if (!this.tabLines.has(j)) {
          this.tabLines.add(j); // peek() is called repeatedly; the finding is reported once
          this.err(j, "tabs cannot be used for indentation in YAML — use spaces");
        }
        continue;
      }
      const text = stripComment(raw).trimEnd();
      if (!text.trim()) continue;
      return { i: j, indent: indentOf(text), text: text.trim() };
    }
    return null;
  }

  /** Lines already reported as tab-indented, so a re-peek doesn't report them twice. */
  private readonly tabLines = new Set<number>();

  /** Read the whole document as a mapping. */
  document(): YamlValue {
    const p = this.peek();
    if (!p) return {};
    if (p.indent !== 0) {
      this.err(p.i, "the document must start at column 0");
      return {};
    }
    if (p.text.startsWith("-")) {
      this.err(p.i, "a workflow file is a mapping (version:, blocks:, …), not a list");
      return {};
    }
    return this.mapping(0);
  }

  private mapping(indent: number): Record<string, YamlValue> {
    const obj: Record<string, YamlValue> = {};
    for (;;) {
      const p = this.peek();
      if (!p || p.indent < indent) break;
      if (p.indent > indent) {
        // Nothing above claimed this line — it is over-indented. Skip it rather than
        // spin, and say so: an unconsumed line with no error is a silently dropped key.
        this.err(p.i, `unexpected indentation — "${p.text}" is indented further than its siblings`);
        this.i = p.i + 1;
        continue;
      }
      if (p.text.startsWith("-")) {
        // `mapping(0)` is only ever called once, from `document()` — there is no enclosing
        // key left to hand this sequence off to (contrast a nested call, where `indent > 0`
        // and the caller — `afterKey`/`sequence` — is waiting for exactly this handoff).
        // Breaking silently here would drop everything from this line to EOF with no finding
        // at all (#270: a hand-edited orphan `-` line truncates the whole rest of the file).
        // Report it, consume the whole orphan sequence so the rest of the document can still
        // be read, and keep going — the same "report and skip" shape as the tab and
        // over-indented-line guards above.
        if (indent === 0) {
          this.err(
            p.i,
            `unexpected "-" at the top level — a workflow file is a mapping (version:, blocks:, …); this sequence belongs to no key`
          );
          this.sequence(indent);
          continue;
        }
        break; // a sequence at this level ends the mapping, handed off to the enclosing key
      }
      const split = splitKey(p.text);
      if (!split) {
        this.err(p.i, `expected "key: value" but found "${p.text}"`);
        this.i = p.i + 1;
        continue;
      }
      this.i = p.i + 1;
      obj[split.key] = this.afterKey(split.rest, indent, p.i);
    }
    return obj;
  }

  /** The value that follows `key:` on line `at`, whatever form it takes. */
  private afterKey(rest: string, indent: number, at: number): YamlValue {
    if (rest === "") {
      const p = this.peek();
      // A nested block is indented further — EXCEPT a sequence, which YAML allows to sit
      // at the parent key's own indent. Both are common in the wild; accept both.
      if (p && (p.indent > indent || (p.indent === indent && p.text.startsWith("-")))) {
        return p.text.startsWith("-") ? this.sequence(p.indent) : this.mapping(p.indent);
      }
      return null;
    }
    // `|`, `>`, with an optional INDENTATION INDICATOR and/or chomping marker, in either
    // order (`|2`, `|-`, `|2-`, `|-2` are all legal YAML).
    if (/^[|>](?:\d[-+]?|[-+]?\d?)$/.test(rest)) return this.blockScalar(rest, indent);
    return this.flowOrScalar(rest, at);
  }

  private sequence(indent: number): YamlValue[] {
    const items: YamlValue[] = [];
    for (;;) {
      const p = this.peek();
      if (!p || p.indent !== indent) break;
      if (p.text !== "-" && !p.text.startsWith("- ")) break;
      this.i = p.i + 1;
      if (p.text === "-") {
        // The item's content is the block indented under the dash.
        const q = this.peek();
        if (q && q.indent > indent) {
          items.push(q.text.startsWith("-") ? this.sequence(q.indent) : this.mapping(q.indent));
        } else {
          items.push(null);
        }
        continue;
      }
      const rest = p.text.slice(1).trimStart();
      // The column the item's keys live at: where `rest` actually starts on the line.
      // `- id: x` puts them at dash+2, but `-   id: x` is legal too, and getting this
      // wrong silently drops every key after the first.
      const keyIndent = indent + (p.text.length - rest.length);
      const split = rest.startsWith("{") || rest.startsWith("[") ? null : splitKey(rest);
      if (split) {
        const first: Record<string, YamlValue> = {};
        first[split.key] = this.afterKey(split.rest, keyIndent, p.i);
        items.push({ ...first, ...this.mapping(keyIndent) });
      } else {
        items.push(this.flowOrScalar(rest, p.i));
      }
    }
    return items;
  }

  /** A `|` / `>` block scalar: every line indented past the key, dedented by the content's
   *  indent — which the header STATES (`|2`) when it can, and which is otherwise inferred
   *  from the first content line. The explicit form is what we emit, and it is the only one
   *  that survives a prompt whose own first line is indented (rev-5 F3): inferring the
   *  dedent from content that is itself indented eats exactly that indentation.
   *
   *  Comments are NOT stripped here — inside a block scalar a `#` is content, and a prompt
   *  that says "# Review checklist" must survive. Tabs likewise: in here they are text. */
  private blockScalar(header: string, parentIndent: number): string {
    const folded = header.startsWith(">");
    const chomp = header.includes("-") ? "strip" : header.includes("+") ? "keep" : "clip";
    const indicator = /\d/.exec(header);
    const body: string[] = [];
    // -1 = "infer from the first content line". An explicit indicator is RELATIVE to the
    // parent node's indentation, which is what makes it independent of the content.
    let contentIndent = indicator ? parentIndent + Number(indicator[0]) : -1;
    while (this.i < this.raw.length) {
      const raw = this.raw[this.i]!;
      if (!raw.trim()) {
        body.push("");
        this.i++;
        continue;
      }
      const ind = indentOf(raw);
      if (ind <= parentIndent) break;
      if (contentIndent < 0) contentIndent = ind;
      body.push(raw.slice(Math.min(ind, contentIndent)).trimEnd());
      this.i++;
    }
    while (body.length && body[body.length - 1] === "") body.pop();
    if (!body.length) return "";
    const text = folded ? foldLines(body) : body.join("\n");
    return chomp === "strip" ? text : text + "\n";
  }

  private flowOrScalar(text: string, at: number): YamlValue {
    if (text.startsWith("[") || text.startsWith("{")) {
      const flow = new FlowReader(text);
      try {
        const v = flow.parse();
        if (!flow.atEnd()) this.err(at, `trailing text after "${text.slice(0, flow.pos)}"`);
        return v;
      } catch (e) {
        this.err(at, e instanceof Error ? e.message : String(e));
        return null;
      }
    }
    return plainScalar(text);
  }
}

/** Fold a `>` scalar: consecutive non-blank lines join with a space, a blank line is a
 *  paragraph break. (Supported for completeness — `|` is what a prompt actually wants,
 *  because folding a prompt's line breaks changes what the agent reads.) */
function foldLines(lines: string[]): string {
  const out: string[] = [];
  let para: string[] = [];
  const flush = (): void => {
    if (para.length) out.push(para.join(" "));
    para = [];
  };
  for (const l of lines) {
    if (!l.trim()) {
      flush();
      out.push("");
    } else para.push(l.trim());
  }
  flush();
  return out.join("\n");
}

/** Split `key: value` at the first top-level `: ` (or a trailing `:`). Returns null when
 *  the line is not a mapping entry at all. */
export function splitKey(text: string): { key: string; rest: string } | null {
  let quote: '"' | "'" | null = null;
  let depth = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text[i]!;
    if (quote) {
      if (c === "\\" && quote === '"') i++;
      else if (c === quote) quote = null;
      continue;
    }
    if (c === '"' || c === "'") quote = c;
    else if (c === "[" || c === "{") depth++;
    else if (c === "]" || c === "}") depth--;
    else if (c === ":" && depth === 0) {
      const next = text[i + 1];
      if (next === undefined || next === " ") {
        const key = text.slice(0, i).trim();
        if (!key) return null;
        return { key: unquote(key), rest: text.slice(i + 1).trim() };
      }
    }
  }
  return null;
}

/** Escape codes a double-quoted scalar can carry. `default: the character itself` covers
 *  `\"` and `\\`, which is the whole point of an escape. */
const ESCAPES: Record<string, string> = { n: "\n", t: "\t", r: "\r" };

function unquote(s: string): string {
  if (s.length >= 2 && s[0] === '"' && s.endsWith('"')) {
    // ONE PASS, left to right (rev-6 F8). Chained `.replace()`s unescape in the wrong order:
    // `\\n` (an escaped backslash followed by the letter n) had its `\n` expanded to a
    // NEWLINE by the first replace, before the later one could collapse `\\` to a single
    // backslash — so `"C:\\new"` read back as `C:` + newline + `ew`. A single pass consumes
    // each backslash with the character it actually escapes, so an escaped backslash can
    // never be re-read as the start of another escape.
    return s.slice(1, -1).replace(/\\(.)/g, (_, c: string) => ESCAPES[c] ?? c);
  }
  if (s.length >= 2 && s[0] === "'" && s.endsWith("'")) return s.slice(1, -1).replace(/''/g, "'");
  return s;
}

function plainScalar(text: string): YamlValue {
  if (text[0] === '"' || text[0] === "'") return unquote(text);
  if (text === "null" || text === "~") return null;
  if (text === "true") return true;
  if (text === "false") return false;
  if (/^-?\d+$/.test(text)) return Number(text);
  if (/^-?\d+\.\d+$/.test(text)) return Number(text);
  return text;
}

/** A one-line flow collection: `[a, b]`, `{ from: x, to: [a, b] }`. */
class FlowReader {
  pos = 0;
  private readonly s: string;

  constructor(s: string) {
    this.s = s;
  }

  atEnd(): boolean {
    this.ws();
    return this.pos >= this.s.length;
  }

  parse(): YamlValue {
    this.ws();
    const c = this.s[this.pos];
    if (c === "[") return this.seq();
    if (c === "{") return this.map();
    return this.scalar();
  }

  private ws(): void {
    while (this.pos < this.s.length && /\s/.test(this.s[this.pos]!)) this.pos++;
  }

  private seq(): YamlValue[] {
    this.pos++; // [
    const out: YamlValue[] = [];
    for (;;) {
      this.ws();
      if (this.s[this.pos] === "]") {
        this.pos++;
        return out;
      }
      if (this.pos >= this.s.length) throw new Error("unterminated [ … ] list");
      out.push(this.parse());
      this.ws();
      if (this.s[this.pos] === ",") this.pos++;
      else if (this.s[this.pos] !== "]") throw new Error(`expected "," or "]" in list`);
    }
  }

  private map(): Record<string, YamlValue> {
    this.pos++; // {
    const out: Record<string, YamlValue> = {};
    for (;;) {
      this.ws();
      if (this.s[this.pos] === "}") {
        this.pos++;
        return out;
      }
      if (this.pos >= this.s.length) throw new Error("unterminated { … } mapping");
      const key = this.scalarText();
      this.ws();
      if (this.s[this.pos] !== ":") throw new Error(`expected ":" after "${key}"`);
      this.pos++;
      out[unquote(key)] = this.parse();
      this.ws();
      if (this.s[this.pos] === ",") this.pos++;
      else if (this.s[this.pos] !== "}") throw new Error(`expected "," or "}" in mapping`);
    }
  }

  private scalar(): YamlValue {
    return plainScalar(this.scalarText());
  }

  /** A scalar token up to the next structural character, quotes respected. */
  private scalarText(): string {
    this.ws();
    const c = this.s[this.pos];
    if (c === '"' || c === "'") {
      const start = this.pos;
      this.pos++;
      while (this.pos < this.s.length) {
        const ch = this.s[this.pos]!;
        if (ch === "\\" && c === '"') this.pos += 2;
        else if (ch === c) {
          this.pos++;
          return this.s.slice(start, this.pos);
        } else this.pos++;
      }
      throw new Error("unterminated quoted string");
    }
    const start = this.pos;
    while (this.pos < this.s.length && !",:[]{}".includes(this.s[this.pos]!)) this.pos++;
    const text = this.s.slice(start, this.pos).trim();
    if (!text) throw new Error("expected a value");
    return text;
  }
}

/** Quote a scalar when leaving it bare would change what it means (or fail to parse).
 *
 *  `,` `[` `]` `{` `}` are in the list for a reason worth stating, because leaving them out
 *  was a silent file-corrupting bug (rev-5 F1): this ONE emitter serves both contexts —
 *  block (`name: …`) and FLOW (`reviewers: [a, b]`, `also: […]`, an unknown key's array or
 *  map). In flow context those five characters are STRUCTURAL, so an unquoted
 *  `Bash(gh pr view --json title,body)` re-reads as two list entries and an unquoted
 *  `fmt{x}` closes the collection early and takes the whole value down with it — and both
 *  happen on an ordinary form edit, because every form edit re-serializes the file.
 *
 *  Rather than keep two emitters and a rule about which context is which (the rule you
 *  forget at exactly one of the six call sites), the ONE emitter quotes for the strictest
 *  context. A quote is always SAFE in block context — it just isn't always necessary — and
 *  "sometimes unnecessary" is a far cheaper failure than "sometimes destroys the value". */
export function emitScalar(v: string): string {
  if (v === "") return '""';
  if (
    /^[-?:,[\]{}#&*!|>'"%@`]/.test(v) ||
    /[,[\]{}]/.test(v) || // structural in a flow collection, anywhere in the string
    /:\s/.test(v) ||
    /\s#/.test(v) ||
    v !== v.trim() ||
    v === "true" ||
    v === "false" ||
    v === "null" ||
    v === "~" ||
    /^-?\d+(\.\d+)?$/.test(v) ||
    /[\n\t\r]/.test(v)
  ) {
    // Backslash FIRST, so the escapes introduced below aren't themselves re-escaped — the
    // mirror image of the reader's single pass (see `unquote`), and the two must stay
    // symmetric or a value stops surviving the round-trip it just survived.
    return `"${v
      .replace(/\\/g, "\\\\")
      .replace(/"/g, '\\"')
      .replace(/\n/g, "\\n")
      .replace(/\t/g, "\\t")
      .replace(/\r/g, "\\r")}"`;
  }
  return v;
}

export function emitValue(v: YamlValue): string {
  if (v === null) return "null";
  if (typeof v === "boolean" || typeof v === "number") return String(v);
  if (typeof v === "string") return emitScalar(v);
  if (Array.isArray(v)) return `[${v.map(emitValue).join(", ")}]`;
  // The KEY goes through the emitter too (rev-6 F9). A key is a string in a flow mapping and
  // is every bit as capable of holding a `,` or a `}` as a value is — emitting it raw was the
  // value-side bug (F1) with the two halves of the pair swapped, and it survived F1's fix
  // only because nothing had put a structural character in a key yet.
  return `{ ${Object.keys(v)
    .sort()
    .map((k) => `${emitScalar(k)}: ${emitValue(v[k]!)}`)
    .join(", ")} }`;
}

// ---------- parse: text → model ----------

export interface ParseResult {
  workflow: Workflow;
  /** Syntax + shape findings. SEMANTIC findings (dangling edges, unknown kinds …) come
   *  from `validateWorkflow` — split because the pane re-validates a model the human is
   *  editing in the form, where there is no text to have a syntax error in. */
  findings: Finding[];
}

const asString = (v: YamlValue): string | null =>
  typeof v === "string" ? v : typeof v === "number" || typeof v === "boolean" ? String(v) : null;

// The keys this build knows, per section. They mirror the engine's `Raw*` structs
// (`crates/loomux-engine/src/workflow.rs`) — the same set `src/workflow-schema.json`
// declares, which `test/workflowschema.test.ts` pins field for field. Hand-written
// rather than read from the manifest on purpose: this module is pure and import-free
// (see the header — its ONE import is a type), and a data file it had to load at
// startup would be a second way for the pane to fail to open a file. The test is the
// link between the two, and it is cheaper than the coupling would be.
const KNOWN_TOP = new Set([
  "version",
  "name",
  "authored_with",
  "blocks",
  "edges",
  "gates",
  "intake",
  "merge_queue",
  "driver",
  "resources",
  "board",
  "triage",
]);
/** The block keys this build knows — the pane's half of the #880 schema
 *  lockstep, EXPORTED so a test can read it as a set.
 *
 *  Exported for one reason, and it is not convenience: the manifest -> pane
 *  direction was already pinned (every declared field is read), but pane ->
 *  manifest was not, so a key added HERE reddened nothing. That asymmetry is
 *  load-bearing for `remote:` (#1457): the whole argument for the key is that a
 *  repo file may not author a destination, and the pane's half of that is that
 *  no destination-shaped key is a field. An 8-name test enumerating
 *  `host`/`port`/… catches only the names it lists; a set equality against the
 *  manifest catches every name nobody thought of.
 *  `test/workflowschema.test.ts` holds both directions. */
export const KNOWN_BLOCK = new Set([
  "id",
  "name",
  "kind",
  "cli",
  "model",
  "prompt",
  "profile",
  "allow",
  "role_hint",
  "effort",
  "context",
  "remote",
  "driver",
  "cache_ttl_minutes",
]);
/** `gates:` is a MAP keyed by gate name, not a fixed struct: the engine reads it as
 *  `BTreeMap<String, RawGate>`, so a `release:` gate parses fine — loomux simply
 *  enforces none but `merge`. That is why a key here lands in `extra` WITHOUT an
 *  `unknown-key` finding, unlike every other section's leftovers. */
const KNOWN_GATE = new Set(["merge"]);
const KNOWN_INTAKE = new Set(["source", "labels"]);
const KNOWN_INTAKE_LABELS = new Set(INTAKE_LABEL_KEYS);
const KNOWN_MERGE_QUEUE = new Set(["enabled", "max_batch", "checks_timeout_minutes"]);
const KNOWN_DRIVER = new Set([
  "enabled",
  "max_review_rounds",
  "max_ci_attempts",
  "max_rebase_attempts",
  "lane_timeout_minutes",
  "fix_timeout_minutes",
  "drive_timeout_minutes",
  "plan_enabled",
  "plan_review_minutes",
  "planner_timeout_minutes",
  "fix_nonblocking_rounds",
  "auto_drive_on_done",
]);
const KNOWN_RESOURCE = new Set(["slots", "max_hold_minutes"]);
const KNOWN_BOARD = new Set(["wip", "enforce"]);

const KNOWN_TRIAGE = new Set(["enabled", "provider", "kinds", "max_defer_minutes"]);
const KNOWN_WIP = new Set<string>(WIP_STATUSES);

function collectExtra(
  obj: Record<string, YamlValue>,
  known: Set<string>
): Record<string, YamlValue> | undefined {
  const extra: Record<string, YamlValue> = {};
  for (const k of Object.keys(obj)) if (!known.has(k)) extra[k] = obj[k]!;
  return Object.keys(extra).length ? extra : undefined;
}

/** A nested-mapping section's raw value, or `null` when there is nothing to read.
 *
 *  YAML null — `intake:` with nothing after it — means ABSENT, the same reading the
 *  engine's `Option<RawIntake>` gives it, so it produces no section and no finding.
 *  A scalar or a list where a mapping belongs is a shape finding: the engine would
 *  refuse the whole file over it, and reporting nothing would leave the pane calling
 *  a file valid that cannot load. */
function readSection(
  v: YamlValue | undefined,
  where: string,
  findings: Finding[]
): Record<string, YamlValue> | null {
  if (v === undefined || v === null) return null;
  if (typeof v !== "object" || Array.isArray(v)) {
    findings.push({
      severity: "error",
      code: "section-not-a-mapping",
      message: `${where}: must be a mapping (found ${emitValue(v)}).`,
    });
    return null;
  }
  return v as Record<string, YamlValue>;
}

/** A field whose TYPE is wrong — `max_batch: soon`, `enabled: yes please`. Rejected,
 *  never coerced: the engine's serde would refuse the file, and a pane that quietly
 *  read `soon` as "the default" would be lying about what is going to run. */
function badValue(where: string, want: string, got: YamlValue): Finding {
  return {
    severity: "error",
    code: "section-bad-value",
    message: `${where}: must be ${want} (found ${emitValue(got)}).`,
  };
}

/** Read one declared-or-absent number field. Absent stays absent — the engine
 *  resolves an omitted number against its own default, so writing one in here would
 *  turn "inherit" into "pin" on the next save. */
function readNumberField(
  r: Record<string, YamlValue>,
  key: string,
  where: string,
  findings: Finding[]
): number | undefined {
  const v = r[key];
  if (v === undefined) return undefined;
  if (typeof v === "number") return v;
  findings.push(badValue(`${where}.${key}`, "a number", v));
  return undefined;
}

function readIntake(r: Record<string, YamlValue>, findings: Finding[]): WorkflowIntake {
  const intake: WorkflowIntake = {};
  if (r.source !== undefined) intake.source = asString(r.source) ?? "";
  const labels = readSection(r.labels, "intake.labels", findings);
  if (labels) {
    const out: WorkflowIntakeLabels = {};
    for (const key of INTAKE_LABEL_KEYS) {
      if (labels[key] !== undefined) out[key] = asString(labels[key]!) ?? "";
    }
    const extra = collectExtra(labels, KNOWN_INTAKE_LABELS);
    if (extra) out.extra = extra;
    intake.labels = out;
  }
  const extra = collectExtra(r, KNOWN_INTAKE);
  if (extra) intake.extra = extra;
  return intake;
}

function readMergeQueue(r: Record<string, YamlValue>, findings: Finding[]): WorkflowMergeQueue {
  const mq: WorkflowMergeQueue = {};
  if (r.enabled !== undefined) {
    if (typeof r.enabled === "boolean") mq.enabled = r.enabled;
    else findings.push(badValue("merge_queue.enabled", "true or false", r.enabled));
  }
  const batch = readNumberField(r, "max_batch", "merge_queue", findings);
  if (batch !== undefined) mq.max_batch = batch;
  const timeout = readNumberField(r, "checks_timeout_minutes", "merge_queue", findings);
  if (timeout !== undefined) mq.checks_timeout_minutes = timeout;
  const extra = collectExtra(r, KNOWN_MERGE_QUEUE);
  if (extra) mq.extra = extra;
  return mq;
}

function readDriver(r: Record<string, YamlValue>, findings: Finding[]): WorkflowDriver {
  const dv: WorkflowDriver = {};
  if (r.enabled !== undefined) {
    if (typeof r.enabled === "boolean") dv.enabled = r.enabled;
    else findings.push(badValue("driver.enabled", "true or false", r.enabled));
  }
  const rounds = readNumberField(r, "max_review_rounds", "driver", findings);
  if (rounds !== undefined) dv.max_review_rounds = rounds;
  const ci = readNumberField(r, "max_ci_attempts", "driver", findings);
  if (ci !== undefined) dv.max_ci_attempts = ci;
  const rebase = readNumberField(r, "max_rebase_attempts", "driver", findings);
  if (rebase !== undefined) dv.max_rebase_attempts = rebase;
  const lane = readNumberField(r, "lane_timeout_minutes", "driver", findings);
  if (lane !== undefined) dv.lane_timeout_minutes = lane;
  const fix = readNumberField(r, "fix_timeout_minutes", "driver", findings);
  if (fix !== undefined) dv.fix_timeout_minutes = fix;
  const drive = readNumberField(r, "drive_timeout_minutes", "driver", findings);
  if (drive !== undefined) dv.drive_timeout_minutes = drive;
  if (r.plan_enabled !== undefined) {
    if (typeof r.plan_enabled === "boolean") dv.plan_enabled = r.plan_enabled;
    else findings.push(badValue("driver.plan_enabled", "true or false", r.plan_enabled));
  }
  const planReview = readNumberField(r, "plan_review_minutes", "driver", findings);
  if (planReview !== undefined) dv.plan_review_minutes = planReview;
  const plannerTimeout = readNumberField(r, "planner_timeout_minutes", "driver", findings);
  if (plannerTimeout !== undefined) dv.planner_timeout_minutes = plannerTimeout;
  const nitRounds = readNumberField(r, "fix_nonblocking_rounds", "driver", findings);
  if (nitRounds !== undefined) dv.fix_nonblocking_rounds = nitRounds;
  if (r.auto_drive_on_done !== undefined) {
    if (typeof r.auto_drive_on_done === "boolean") dv.auto_drive_on_done = r.auto_drive_on_done;
    else findings.push(badValue("driver.auto_drive_on_done", "true or false", r.auto_drive_on_done));
  }
  const extra = collectExtra(r, KNOWN_DRIVER);
  if (extra) dv.extra = extra;
  return dv;
}

/** `resources:` is a MAP of repo-chosen names, so every key here is a resource, never
 *  an unknown one. A name written with nothing under it (`build:`) reads as a resource
 *  DECLARED WITH DEFAULTS — that is what a human means by it — and re-emits as
 *  `build: {}`, which is the spelling the engine's serde actually accepts. */
function readResources(
  r: Record<string, YamlValue>,
  findings: Finding[]
): Record<string, WorkflowResource> {
  const out: Record<string, WorkflowResource> = {};
  for (const name of Object.keys(r)) {
    const where = `resources.${name}`;
    const body = readSection(r[name], where, findings);
    const res: WorkflowResource = {};
    if (body) {
      const slots = readNumberField(body, "slots", where, findings);
      if (slots !== undefined) res.slots = slots;
      const hold = readNumberField(body, "max_hold_minutes", where, findings);
      if (hold !== undefined) res.max_hold_minutes = hold;
      const extra = collectExtra(body, KNOWN_RESOURCE);
      if (extra) res.extra = extra;
    }
    out[name] = res;
  }
  return out;
}

/** `board:` (#1175) — per-status WIP limits plus one posture bool.
 *
 *  `wip:` is read against `WIP_STATUSES` rather than as an open map: the engine's
 *  `RawWip` is a closed struct, so `in-porgress: 4` is a file that will not load, and a
 *  pane that quietly preserved it as an unremarked key would call that file valid. The
 *  unknown key is still PRESERVED (`wipExtra`) — dropping it is destructive — it is just
 *  also reported, which is the honest pair `collectExtra` establishes everywhere else. */
function readBoard(r: Record<string, YamlValue>, findings: Finding[]): WorkflowBoard {
  const board: WorkflowBoard = {};
  const wipSection = readSection(r.wip, "board.wip", findings);
  if (wipSection) {
    const wip: WorkflowWip = {};
    for (const status of WIP_STATUSES) {
      const n = readNumberField(wipSection, status, "board.wip", findings);
      if (n !== undefined) wip[status] = n;
    }
    board.wip = wip;
    const extra = collectExtra(wipSection, KNOWN_WIP);
    if (extra) board.wipExtra = extra;
  }
  if (r.enforce !== undefined) {
    if (typeof r.enforce === "boolean") board.enforce = r.enforce;
    else findings.push(badValue("board.enforce", "true or false", r.enforce));
  }
  const extra = collectExtra(r, KNOWN_BOARD);
  if (extra) board.extra = extra;
  return board;
}

/** `triage:` (#3304 S1) — one switch, one closed-vocabulary string, one list and one
 *  bounded number.
 *
 *  `provider` is read as a STRING rather than against the accepted set: the engine's
 *  refusal is the authority on which providers this build has, and a pane that
 *  second-guessed it would report a file broken that a newer engine loads. The
 *  unknown-KEY report is a different question and is still made, as everywhere else. */
function readTriage(r: Record<string, YamlValue>, findings: Finding[]): WorkflowTriage {
  const triage: WorkflowTriage = {};
  if (r.enabled !== undefined) {
    if (typeof r.enabled === "boolean") triage.enabled = r.enabled;
    else findings.push(badValue("triage.enabled", "true or false", r.enabled));
  }
  if (r.provider !== undefined) {
    const p = asString(r.provider);
    if (p !== null && p !== undefined) triage.provider = p;
    else findings.push(badValue("triage.provider", "a provider name", r.provider));
  }
  if (r.kinds !== undefined) {
    if (Array.isArray(r.kinds)) {
      triage.kinds = r.kinds.map((x) => asString(x) ?? "").filter(Boolean);
    } else {
      findings.push(badValue("triage.kinds", "a list of delivery kinds", r.kinds));
    }
  }
  const defer = readNumberField(r, "max_defer_minutes", "triage", findings);
  if (defer !== undefined) triage.max_defer_minutes = defer;
  const extra = collectExtra(r, KNOWN_TRIAGE);
  if (extra) triage.extra = extra;
  return triage;
}

/** Read a workflow file. NEVER throws and NEVER refuses: a file it cannot fully
 *  understand still yields a workflow (with stub blocks) plus the findings that say why,
 *  because the pane's job is to let the human FIX the file — which it cannot do if the
 *  file won't open. */
export function parseWorkflow(text: string): ParseResult {
  // Strip a BOM. A workflow file written by a Windows editor (or by `Set-Content` without
  // `-Encoding utf8NoBOM`) starts with U+FEFF, and the reader would otherwise take it as part
  // of the first KEY — so `version: 1` arrived as a key named "﻿version", the version
  // read as missing, and the pane reported a file the human could see was right as broken.
  // It is invisible, so nothing about the error message could have led them to the cause.
  const reader = new YamlReader(text.replace(/^﻿/, "").split(/\r?\n/));
  const doc = reader.document();
  const findings = reader.findings;
  const w: Workflow = { version: WORKFLOW_VERSION, name: "", blocks: [], edges: [], gates: {} };

  if (doc === null || typeof doc !== "object" || Array.isArray(doc)) {
    if (text.trim()) {
      findings.push({
        severity: "error",
        code: "not-a-mapping",
        message: "A workflow file is a mapping with version:, blocks: and (optionally) edges: / gates:.",
      });
    }
    return { workflow: w, findings };
  }
  const root = doc as Record<string, YamlValue>;

  if (root.version === undefined) {
    findings.push({
      severity: "error",
      code: "version-missing",
      message: `No version: — this file should declare "version: ${WORKFLOW_VERSION}".`,
    });
  } else if (typeof root.version !== "number") {
    findings.push({
      severity: "error",
      code: "version-unsupported",
      message: `version: must be a number (found "${String(root.version)}").`,
    });
  } else {
    w.version = root.version;
    if (root.version !== WORKFLOW_VERSION) {
      findings.push({
        severity: "error",
        code: "version-unsupported",
        message: `version: ${root.version} is not supported by this build of orrerix (it reads version ${WORKFLOW_VERSION}).`,
      });
    }
  }

  w.name = asString(root.name ?? "") ?? "";
  // Declared-or-absent, never defaulted: an empty `authored_with:` is a file that says
  // it doesn't know, and a missing one is a file that never claimed to.
  if (root.authored_with !== undefined) w.authored_with = asString(root.authored_with) ?? "";
  w.extra = collectExtra(root, KNOWN_TOP);

  // `blocks:` / `edges:` written with nothing after them are YAML null, and null here means
  // EMPTY — an empty roster, no edges. Only a value that is present and is not a list is a
  // shape error (rev-5 F4): reporting "must be a list" against an empty one would have the
  // pane complain about the file it just wrote itself when you delete the last block.
  const blocks = root.blocks;
  if (blocks !== undefined && blocks !== null && !Array.isArray(blocks)) {
    findings.push({
      severity: "error",
      code: "block-not-a-mapping",
      message: "blocks: must be a list of blocks.",
    });
  } else if (Array.isArray(blocks)) {
    blocks.forEach((raw, i) => w.blocks.push(readBlock(raw, i, findings)));
  }

  const edges = root.edges;
  if (edges !== undefined && edges !== null && !Array.isArray(edges)) {
    findings.push({
      severity: "error",
      code: "edge-not-a-mapping",
      message: "edges: must be a list of { from: …, to: … } entries.",
    });
  } else if (Array.isArray(edges)) {
    edges.forEach((raw, i) => w.edges.push(...readEdge(raw, i, findings)));
  }

  const gates = root.gates;
  if (gates !== undefined && (typeof gates !== "object" || gates === null || Array.isArray(gates))) {
    findings.push({
      severity: "error",
      code: "gate-unknown-require",
      message: "gates: must be a mapping (today the only gate is `merge`).",
    });
  } else if (gates && typeof gates === "object" && !Array.isArray(gates)) {
    const g = gates as Record<string, YamlValue>;
    if (g.merge !== undefined) w.gates.merge = readGate(g.merge, findings);
    w.gates.extra = collectExtra(g, KNOWN_GATE);
  }

  // The policy sections (#382 / #581 / #858). Each is optional and each is absent
  // rather than defaulted when the file says nothing — see `WorkflowIntake`.
  const intake = readSection(root.intake, "intake", findings);
  if (intake) w.intake = readIntake(intake, findings);
  const mergeQueue = readSection(root.merge_queue, "merge_queue", findings);
  if (mergeQueue) w.merge_queue = readMergeQueue(mergeQueue, findings);
  const driver = readSection(root.driver, "driver", findings);
  if (driver) w.driver = readDriver(driver, findings);
  const resources = readSection(root.resources, "resources", findings);
  if (resources) w.resources = readResources(resources, findings);
  const board = readSection(root.board, "board", findings);
  if (board) w.board = readBoard(board, findings);
  const triage = readSection(root.triage, "triage", findings);
  if (triage) w.triage = readTriage(triage, findings);

  return { workflow: w, findings };
}

/** One block, ALWAYS — a malformed entry becomes a stub with the findings that explain
 *  it, never a dropped row. A block you cannot see is a block you cannot repair. */
function readBlock(raw: YamlValue, index: number, findings: Finding[]): WorkflowBlock {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
    findings.push({
      severity: "error",
      code: "block-not-a-mapping",
      message: `blocks[${index}] is not a block mapping (expected id:, name:, kind:, cli: …).`,
    });
    return { id: "", name: `block ${index + 1}`, kind: "", cli: "", model: "" };
  }
  const r = raw as Record<string, YamlValue>;
  const id = asString(r.id ?? "") ?? "";
  const block: WorkflowBlock = {
    id,
    name: asString(r.name ?? "") ?? id,
    kind: asString(r.kind ?? "") ?? "",
    cli: asString(r.cli ?? "") ?? "",
    model: asString(r.model ?? "") ?? "",
    extra: collectExtra(r, KNOWN_BLOCK),
  };
  if (r.prompt !== undefined) block.prompt = asString(r.prompt) ?? "";
  if (r.profile !== undefined) block.profile = asString(r.profile) ?? "";
  // A list, or a finding — never a coerced scalar. `allow: Bash(git push)` (no
  // brackets) is a file the engine refuses, and pretending it declared nothing would
  // hide a line whose whole purpose is to pre-approve a tool.
  if (r.allow !== undefined) {
    if (Array.isArray(r.allow)) block.allow = r.allow.map((v) => asString(v) ?? "");
    else findings.push(badValue(`blocks[${index}].allow`, "a list of tool patterns", r.allow));
  }
  if (r.role_hint !== undefined) block.role_hint = asString(r.role_hint) ?? "";
  // #687. `undefined` (never declared) and `""` (declared empty) are kept apart
  // the way role_hint keeps them, so a save can't turn one into the other.
  if (r.effort !== undefined) block.effort = asString(r.effort) ?? "";
  if (r.context !== undefined) block.context = asString(r.context) ?? "";
  // #1457. Read as written — the label is validated, never rewritten, so what the
  // pane shows and what the engine refuses are the same string.
  //
  // A NULL is not an empty label, and the two are a real difference here rather
  // than a pedantic one: a bare `remote:` line is YAML null, which the engine
  // reads into `Option<String>` as None — a local block, loaded fine — while
  // `remote: ""` is `Some("")` and is REFUSED (`check_segment` -> Empty). The
  // `?? ""` idiom the neighbouring fields use would collapse the two, and the
  // pane would then paint a file red that the engine loads. So a null is treated
  // as the absent key it means, which is also what the engine's own error would
  // say if you asked it.
  const remote = asString(r.remote);
  if (remote !== null) block.remote = remote;
  // #2850. Read as written, like `remote` above: the VALUE is closed on the
  // engine side (`DRIVER_MODES`) and whether the block's CLI can carry it is
  // capability data, so the pane's job here is to show and re-emit the file's
  // text, not to second-guess it. A bare `driver:` line is YAML null — the
  // absent key, which is what the engine reads it as too.
  const driver = asString(r.driver);
  if (driver !== null) block.driver = driver;
  // #3407. A number or a finding, never a coerced scalar — `readNumberField`'s rule.
  const ttl = readNumberField(r, "cache_ttl_minutes", `blocks[${index}]`, findings);
  if (ttl !== undefined) block.cache_ttl_minutes = ttl;
  return block;
}

/** `{ from: x, to: y }` or `{ from: x, to: [a, b] }` — the fan-out form expands into one
 *  flat edge per target, because that is what every graph question is asked of. */
function readEdge(raw: YamlValue, index: number, findings: Finding[]): WorkflowEdge[] {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
    findings.push({
      severity: "error",
      code: "edge-not-a-mapping",
      message: `edges[${index}] is not a { from: …, to: … } mapping.`,
    });
    return [];
  }
  const r = raw as Record<string, YamlValue>;
  const from = asString(r.from ?? "") ?? "";
  const targets = Array.isArray(r.to) ? r.to : r.to === undefined ? [] : [r.to];
  if (!from || !targets.length) {
    findings.push({
      severity: "error",
      code: "edge-not-a-mapping",
      message: `edges[${index}] needs both a from: and a to:.`,
    });
    return [];
  }
  return targets.map((t) => ({ from, to: asString(t) ?? "" }));
}

function readGate(raw: YamlValue, findings: Finding[]): MergeGate {
  const gate: MergeGate = { require: "all-pass", reviewers: [], also: [] };
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
    findings.push({
      severity: "error",
      code: "gate-unknown-require",
      message: "gates.merge must be a mapping (require:, reviewers:, …).",
    });
    return gate;
  }
  const r = raw as Record<string, YamlValue>;
  // `threshold: N` with NO `require:` key is a threshold gate — the engine's own rule
  // ("`threshold: N` alone implies a threshold gate; spelling `require: threshold` as well is
  // allowed but redundant", workflow.rs `(Some("threshold") | None, Some(n))`). Defaulting the
  // absent key to "all-pass" here made the pane read such a file as an all-pass gate that
  // happens to carry a number, with three consequences, all silent: the threshold rules below
  // never ran on it, `withGateReviewers` never clamped it, and — worst — the next gate edit
  // re-serialized it as `require: all-pass` + `threshold: N`, a PAIR the engine refuses
  // outright, so the group fell back to the built-in roster over an unrelated edit.
  //
  // ONLY when the key is absent. An empty or non-string `require:` is left exactly as it was:
  // the engine refuses `require: ""` as an unknown value, and quietly reading it as something
  // else is the same lie in the other direction.
  gate.require =
    r.require === undefined
      ? typeof r.threshold === "number"
        ? "threshold"
        : "all-pass"
      : asString(r.require) ?? "all-pass";
  if (typeof r.threshold === "number") gate.threshold = r.threshold;
  else if (r.threshold !== undefined) {
    findings.push({
      severity: "error",
      code: "gate-bad-threshold",
      message: `gates.merge.threshold must be a number (found "${String(r.threshold)}").`,
    });
  }
  const list = (v: YamlValue): string[] =>
    Array.isArray(v) ? v.map((x) => asString(x) ?? "").filter(Boolean) : [];
  gate.reviewers = list(r.reviewers ?? []);
  gate.also = list(r.also ?? []);
  // #1174. Read the same way `threshold` is — a non-number is a finding, never a
  // coerced value — because `MergeGate` has no unknown-key bag: a key this function
  // does not read is a line the next form edit DELETES.
  if (typeof r.max_diff_lines === "number") gate.max_diff_lines = r.max_diff_lines;
  else if (r.max_diff_lines !== undefined) {
    findings.push({
      severity: "error",
      code: "gate-bad-max-diff-lines",
      message: `gates.merge.max_diff_lines must be a number (found "${String(r.max_diff_lines)}").`,
    });
  }
  // #1176. Same rule as `max_diff_lines` above and for a sharper version of the
  // same reason: what a dropped key costs here is a REQUIRED REVIEWER. So a
  // routing block this reader cannot make sense of is a finding plus an
  // entry kept as-far-as-it-was-read, never a silent omission.
  if (Array.isArray(r.routing)) {
    const rules: RoutingRule[] = [];
    r.routing.forEach((raw, i) => {
      if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
        findings.push({
          severity: "error",
          code: "gate-bad-routing",
          message: `gates.merge.routing[${i}] must be a { paths: […], reviewers: […] } mapping.`,
        });
        return;
      }
      const rr = raw as Record<string, YamlValue>;
      for (const key of Object.keys(rr)) {
        if (key !== "paths" && key !== "reviewers") {
          findings.push({
            severity: "error",
            code: "gate-bad-routing",
            message: `gates.merge.routing[${i}]: unknown key "${key}" — a routing rule takes paths: and reviewers: only. The engine refuses the whole file over one, so this pane will not bless it.`,
          });
        }
      }
      rules.push({ paths: list(rr.paths ?? []), reviewers: list(rr.reviewers ?? []) });
    });
    if (rules.length) gate.routing = rules;
  } else if (r.routing !== undefined) {
    findings.push({
      severity: "error",
      code: "gate-bad-routing",
      message: `gates.merge.routing must be a list of rules (found "${String(r.routing)}").`,
    });
  }
  return gate;
}
