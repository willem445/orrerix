// The workflow PANE's file picker, DECIDED (#2944). DOM-free and I/O-free, for the reason
// `workflowpane.ts` states at its own head and `roster.ts`'s `resolveWorkflowPicker` states
// for the launcher's picker: the questions this control answers — which files are on offer,
// which one is open, what an unreadable one says about itself, and whether a name may be
// created — are decisions, and a decision tested by clicking is a decision nobody tests.
//
// Why a SECOND picker resolver rather than reusing `resolveWorkflowPicker`: the launcher's
// picker answers "which workflow will this launch RUN", keyed by NAME, and it hides itself
// for a repo with one workflow because a single-option control cannot be used. This one
// answers "which file is this pane EDITING", keyed by PATH, and it must show for a repo with
// one workflow — because its other half is *New workflow…*, which is exactly how a repo gets
// its second one. Same listing (`orch_workflow_list`, #2603 — never a second discovery), two
// different questions; folding them into one function would mean a flag deciding which of two
// unrelated behaviours you get, which is two functions with extra steps.

import {
  DEFAULT_WORKFLOW_NAME,
  LEGACY_CONFIG_DIR,
  WORKFLOW_NAME_MAX,
  isWorkflowName,
  workflowRelFor,
} from "./workflowmodel.ts";
import type { WorkflowListing, WorkflowEntry } from "./roster";

/** One file the picker offers. */
export interface WorkflowFileOption {
  /** The workflow's name — the file's stem; `default` is the repo's `workflow.yml`. */
  name: string;
  /** The repo-relative file, as the BACKEND resolved it — never re-derived here, so the
   *  picker and a launch cannot disagree about which file a name means (including which of
   *  the two config-dir spellings this repo uses). */
  path: string;
  /** What the option reads on screen. */
  label: string;
  /** Whether the file parsed and validated. */
  valid: boolean;
  /** Why it does not, in one line, or null. An unparseable file is LISTED — carrying this —
   *  and never dropped: a workflow that vanishes from the picker the moment it gets a syntax
   *  error is one the human cannot navigate back to in order to fix it, and this pane is the
   *  fix. */
  finding: string | null;
  /** Is this the file the pane is showing right now? */
  current: boolean;
}

/** The pane picker's whole state, resolved. */
export interface WorkflowFilePicker {
  /** The options, in the listing's order (the backend sorts by name). */
  options: WorkflowFileOption[];
  /** The open file's own path as the LISTING spells it, or null when the pane is showing a
   *  file the repo does not declare as a workflow. */
  currentPath: string | null;
  /** The pane is on a `.yml` that is not one of this repo's workflows — the file browser's
   *  *Open in workflow pane* accepts any `.yml` (#217/#222), so this is an ordinary state and
   *  not an error. The control says so rather than marking an unrelated option current. */
  offListing: boolean;
  /** What the LISTING could not make sense of, as opposed to what one file could not parse:
   *  `default` declared twice, a stem that is not a usable name, more files than the listing
   *  carries. Advisory — none of it stops the pane opening anything. */
  findings: string[];
}

const normPath = (p: string): string => p.replace(/\\/g, "/").replace(/^\.\//, "");
const foldPath = (p: string): string => normPath(p).toLowerCase();

/** Two repo-relative paths naming the same FILE.
 *
 *  Separators are normalised because a restored pane record and the backend's listing need not
 *  spell them the same way. Case is compared only as a FALLBACK, and never to choose between
 *  two options: the platforms this ships on are case-insensitive, so `.orrerix/Workflows/x.yml`
 *  and `.orrerix/workflows/x.yml` are one file there — but `Default.yml` beside `default.yml`
 *  is precisely the collision #2892 is about, and picking one of those by case-folding would
 *  be the picker guessing. Hence: an exact match wins outright, and the fold is consulted only
 *  when nothing matched exactly. */
const samePath = (a: string, b: string): boolean => normPath(a) === normPath(b);

/** Resolve the pane's file picker from the backend's listing and the file the pane is on.
 *
 *  `currentRel` is the pane's OWN `rel` — the path its header shows and its saves write —
 *  not a value read off an element, for the reason `resolveWorkflowPicker`'s header gives.
 *
 *  A `null` listing (the read failed, or none has been made yet) is not an empty repo: it is
 *  "we do not know". It yields no options and `offListing: false` — the pane is showing the
 *  file it was asked to show, and a control that has not managed to list anything must not
 *  tell the human their file is off the listing. */
export function resolveWorkflowFilePicker(
  listing: WorkflowListing | null,
  currentRel: string
): WorkflowFilePicker {
  const entries = listing?.workflows ?? [];
  const exact = entries.find((e) => samePath(e.path, currentRel));
  const current = exact ?? entries.find((e) => foldPath(e.path) === foldPath(currentRel)) ?? null;
  return {
    options: entries.map((e) => ({
      name: e.name,
      path: e.path,
      label: optionLabel(e),
      valid: e.valid,
      finding: findingOf(e),
      current: e === current,
    })),
    currentPath: current?.path ?? null,
    offListing: listing !== null && current === null,
    findings: (listing?.findings ?? []).filter((f) => f.trim() !== ""),
  };
}

/** One option's on-screen text. The NAME is the identity, so it leads; the file's own `name:`
 *  follows only when it says something the name does not. An unparseable file is MARKED rather
 *  than hidden — see {@link WorkflowFileOption.finding}. */
function optionLabel(e: WorkflowEntry): string {
  const prose = e.display_name.trim();
  const head = prose && prose !== e.name ? `${e.name} — ${prose}` : e.name;
  return e.valid ? head : `${head} (has errors)`;
}

/** The one line an option says about why it will not parse, or null when it does.
 *
 *  The FIRST finding, not all of them: this is a line in a dropdown, the pane opens the file
 *  and shows every finding in its own findings strip, and an option that wraps to five lines
 *  is one nobody reads. A valid entry is null; an invalid one with an empty `errors` still
 *  says something, because "this file is broken" is the fact the option must carry even when
 *  the backend did not say why. */
function findingOf(e: WorkflowEntry): string | null {
  if (e.valid) return null;
  return e.errors.find((x) => x.trim() !== "")?.trim() ?? "the file could not be read";
}

/** Whether *New workflow…* may create `name`, and where.
 *
 *  A verdict, not a boolean, because every refusal here is one the human has to act on and
 *  "no" alone would leave them guessing which rule they broke. */
export type CreateVerdict =
  /** Create `path`. */
  | { ok: true; name: string; path: string }
  /** Refuse, saying this. */
  | { ok: false; reason: string };

/** May `name` be created as a new workflow file in this repo?
 *
 *  The refusals, and the last is the one #2944 asks for by name.
 *
 *  1. **The name is not a usable one** — {@link isWorkflowName}, which mirrors the engine's
 *     `check_segment` verbatim. REFUSED, never rewritten: rewriting is the specific thing
 *     that rule forbids, because two spellings that normalise to one name are two files
 *     claiming one workflow.
 *  2. **It is `default`** — that name belongs to the repo's own `workflow.yml`, which this
 *     picker already lists. Creating `workflows/default.yml` beside it is the two-files-one-
 *     name shape from the other end.
 *  3. **It already exists** — the listing declares it. Creating would either fail at the
 *     atomic claim or, worse, be understood as "open that one", and the pane already has a
 *     control for opening one.
 *  4. **It collides with an existing name only by CASE** — `Review` beside `review`. This is
 *     the half of #2892 a creation path can close: the platforms orrerix ships on are
 *     case-insensitive, so the two names are one file there and two on Linux, and a repo that
 *     acquires the pair has a workflow whose identity depends on which machine reads it.
 *     Discovery still has to decide what to do about a pair that is ALREADY on disk (that is
 *     the rest of #2892, and it is a `scan_workflows` change); what this closes is orrerix
 *     itself being the thing that creates one.
 *
 *  A `null` listing REFUSES, and that is deliberate: without a listing there is nothing to
 *  check rules 3 and 4 against, and a create that cannot rule out a collision is exactly the
 *  create this function exists to stop. The pane asks again once the listing lands. */
export function canCreateWorkflow(name: string, listing: WorkflowListing | null): CreateVerdict {
  if (!name) return { ok: false, reason: "Give the workflow a name." };
  if (!isWorkflowName(name)) return { ok: false, reason: nameRuleFor(name) };
  if (name === DEFAULT_WORKFLOW_NAME) {
    return {
      ok: false,
      reason:
        "`default` is the repo's own workflow file — open it from this list rather than creating a second file claiming that name.",
    };
  }
  if (!listing) {
    return {
      ok: false,
      reason:
        "orrerix could not list this repo's workflows, so it can't tell whether that name is already taken. Reload the pane and try again.",
    };
  }
  const taken = listing.workflows.find((e) => e.name === name);
  if (taken) {
    return { ok: false, reason: `${name} already exists (${taken.path}) — open it from this list instead.` };
  }
  const clash = listing.workflows.find((e) => e.name.toLowerCase() === name.toLowerCase());
  if (clash) {
    return {
      ok: false,
      reason:
        `${name} differs from ${clash.name} only by capitalisation. On Windows and macOS those are one file, so the repo ` +
        `would end up with a workflow whose identity depends on which machine reads it (#2892). Pick another name, or edit ${clash.name}.`,
    };
  }
  const path = workflowRelFor(name, { legacy: usesLegacyConfigDir(listing) });
  // Unreachable in this function as written: `isWorkflowName` passed above and `workflowRelFor`
  // refuses on exactly that predicate. Stated as a refusal rather than asserted, so a future
  // divergence between the two is a "no" the human can read and not a `null` path handed to a
  // write. It is not, however, what makes the top-of-function check load-bearing: that is the
  // ORDER (see `the alphabet rule is asked FIRST`), because reaching here means every
  // collision branch was consulted first and one of them may have answered.
  if (!path) return { ok: false, reason: nameRuleFor(name) };
  return { ok: true, name, path };
}

/** Which config-dir spelling this repo uses, read off the listing's own resolved paths rather
 *  than guessed — the same rule `workflowRelFor`'s doc states. A repo with nothing listed at
 *  all gets the preferred spelling, which is where a new workflow belongs. */
function usesLegacyConfigDir(listing: WorkflowListing): boolean {
  const first = listing.workflows[0];
  return !!first && normPath(first.path).startsWith(`${LEGACY_CONFIG_DIR}/`);
}

/** Which of the name rules `name` broke, in the words that name it. One message per rule
 *  rather than one message listing them all: the human typed one thing, and what they want to
 *  know is what is wrong with it. */
function nameRuleFor(name: string): string {
  if (name.length > WORKFLOW_NAME_MAX) {
    return `A workflow name is at most ${WORKFLOW_NAME_MAX} characters (that one is ${name.length}).`;
  }
  if (name.startsWith("-")) {
    return "A workflow name can't start with `-` — it would read as an option to any command line the name reaches.";
  }
  if (!/^[A-Za-z0-9_-]+$/.test(name)) {
    return "A workflow name is letters, digits, `_` and `-` only — it becomes a file name, so `.`, `/` and `:` are refused rather than rewritten.";
  }
  return `\`${name}\` is a reserved device name on Windows, so it can't be a file there.`;
}

// ---------- what switching files is allowed to do to the buffer ----------

/** What the pane must do before it can show another file. */
export type SwitchPlan =
  /** Already on it. Not an error, and not a reload either — a picker whose current option
   *  re-read the file would discard an unsaved buffer for a no-op click. */
  | { kind: "same-file" }
  /** Nothing is at stake: retarget and load. */
  | { kind: "open"; file: string }
  /** There are unsaved edits. ASK first, with the same three answers a close offers, and do
   *  not touch anything until the human has given one. */
  | { kind: "ask"; file: string };

/** May the pane move to `target` right now?
 *
 *  THE RULE THIS STATES, and it is the one a picker gets wrong by being convenient: an
 *  unsaved buffer belongs to the file it was typed against. It may be SAVED there and then
 *  left behind, or DISCARDED, or the switch may be CANCELLED — and it may never be carried
 *  across, because carrying it across means the next Save writes one workflow's text over
 *  another workflow's file. Nothing in the pane's own machinery would stop that: `save()`
 *  writes `this.rel`, and `this.rel` is whatever the picker last set.
 *
 *  `same-file` is separate from `open` for a reason a boolean would lose: clicking the option
 *  you are already on is the commonest click a marked-current list gets, and treating it as an
 *  open would re-read the file — which discards the buffer, silently, for a gesture that asked
 *  for nothing. */
export function switchPlan(
  state: { current: string; dirty: boolean },
  target: string
): SwitchPlan {
  if (samePath(state.current, target) || foldPath(state.current) === foldPath(target)) {
    return { kind: "same-file" };
  }
  return state.dirty ? { kind: "ask", file: target } : { kind: "open", file: target };
}
