// Switching a RUNNING group's workflow (#1689 slice D2) — DOM-free derivations
// over `WorkflowStatus` and `WorkflowSwitchPreview` (orchestration.ts) for the
// group header's picker, its drift chip and its Review & apply confirmation.
//
// This module never fetches anything and never applies anything. It decides
// which options the picker offers, which one is selected, whether the control
// can be used at all and what the confirmation says — for the same reason
// `roster.ts`'s launch-time picker is DOM-free: this is a consent surface, and
// a consent surface tested by clicking is a consent surface nobody tests.
//
// The selection lives in the GROUP VIEW'S state, never on the `<select>`
// element. `groupview.ts` re-renders on every 2 s poll, so an element read back
// at click time is read off a control the last render rebuilt — CLAUDE.md's
// in-list-editor rule, reached here through a panel that genuinely re-renders
// under the human's hand. `resolveSwitchPicker` is what makes holding the
// selection safe: it takes the name the view holds and returns the name that is
// actually offered, so the element is always written FROM the view.

import { DEFAULT_WORKFLOW_NAME } from "./workflowmodel.ts";
import { gateLine } from "./workflowstatus.ts";
import type { WorkflowStatus, WorkflowSwitchPreview } from "./orchestration";

/** One option in the group header's workflow picker. */
export interface SwitchOption {
  /** The workflow name — what `applyWorkflow` would be given. */
  name: string;
  /** What the option reads on screen. */
  label: string;
  /** Whether this is the workflow the group is running RIGHT NOW. */
  active: boolean;
  /** True for the synthesized row that carries an active workflow the repo no
   *  longer declares — see {@link resolveSwitchPicker}. */
  missing: boolean;
}

/** The header picker's whole state, resolved. */
export interface SwitchPicker {
  /** The options, in the backend's order (`available` is sorted), with the
   *  active workflow guaranteed to be one of them. */
  options: SwitchOption[];
  /** The name Review & apply would send. Always one of `options`, or
   *  {@link DEFAULT_WORKFLOW_NAME} when there are none. */
  selected: string;
  /** The workflow the group is running now. */
  active: string;
  /** Whether the control is worth rendering at all. */
  show: boolean;
  /** Whether the human may operate it. FALSE while the advanced-orchestrator
   *  toggle is off — the toggle is the consent, this picker only answers which
   *  file — and while an apply or a toggle is already in flight. */
  enabled: boolean;
  /** Why it is disabled, for the control's title. `null` when it is enabled. */
  disabledReason: string | null;
  /** The selection differs from what is running. Display only: Review & apply
   *  is offered either way, because a re-apply of the ACTIVE name is how a
   *  drifted file gets adopted, and the preview is what says "nothing would
   *  change" when nothing would. */
  dirty: boolean;
}

/** The drift chip: the group's pinned roster and its workflow file have
 *  diverged. */
export interface DriftChip {
  text: string;
  title: string;
}

/** Resolve the header picker from the group's live status and the name the view
 *  is holding.
 *
 *  `want` is view state, never a value read off the `<select>` — see this
 *  module's header. It is honoured only when the status still offers it;
 *  otherwise the selection falls back to the ACTIVE workflow. Falling back to
 *  what is RUNNING (rather than to `default`, which is where the launcher's
 *  picker falls back) is the difference between the two surfaces: a launch has
 *  no workflow yet and `default` is the file every repo can have, while a
 *  running group always has one right answer already, and offering to switch a
 *  live group to something it is not running is the direction that costs
 *  something if it is wrong.
 *
 *  **The active workflow is always an option, even when `available` does not
 *  list it.** A file deleted or renamed under a running group leaves the group
 *  running the roster it pinned — deliberately (#222 rev-11 F2) — and a picker
 *  that silently showed some other name as selected would be describing a group
 *  that does not exist. The synthesized row is marked, so the human reads "the
 *  file behind this is gone" rather than "this is fine".
 *
 *  A `null` status is the panel before its first successful read: no options, no
 *  control, nothing claimed. */
export function resolveSwitchPicker(
  status: WorkflowStatus | null,
  want: string | null,
  busy: boolean
): SwitchPicker {
  if (!status) {
    return {
      options: [],
      selected: DEFAULT_WORKFLOW_NAME,
      active: DEFAULT_WORKFLOW_NAME,
      show: false,
      enabled: false,
      disabledReason: null,
      dirty: false,
    };
  }
  const active = status.workflow || DEFAULT_WORKFLOW_NAME;
  const available = status.available ?? [];
  const names = available.includes(active) ? available : [...available, active];
  const options: SwitchOption[] = names.map((name) => ({
    name,
    label: switchOptionLabel(name, name === active, !available.includes(name)),
    active: name === active,
    missing: !available.includes(name),
  }));
  const has = (n: string | null): boolean => !!n && options.some((o) => o.name === n);
  const selected = has(want) ? want! : active;
  const enabled = status.advanced && !busy;
  return {
    options,
    selected,
    active,
    // One option and no drift is a group with nothing to switch to and nothing
    // to adopt — every repo that has not opted into named workflows. Drift alone
    // earns the control, because re-applying the ACTIVE name is the only way to
    // adopt an edited file (design note, "Drift became a badge as well as an
    // audit row"), so hiding it there would hide the fix for the thing the chip
    // beside it is complaining about.
    show: options.length > 1 || status.drift !== null,
    enabled,
    disabledReason: !status.advanced
      ? "Turn workflow mode on first — the toggle is the consent for running a repo-authored roster; this picker only chooses which file."
      : busy
        ? "A workflow change is already in flight."
        : null,
    dirty: selected !== active,
  };
}

/** What one option reads on screen. The running one says so — the picker is the
 *  only place the group's active workflow is named next to its alternatives, and
 *  a `<select>` whose value happens to equal the running name is not a statement
 *  that it IS running. */
function switchOptionLabel(name: string, active: boolean, missing: boolean): string {
  if (missing) return `${name} (running — file is gone)`;
  return active ? `${name} (running)` : name;
}

/** The drift chip, or `null` when the pinned roster and the file agree — which
 *  includes every group with workflow mode off, because a group running the
 *  built-in roster has no declared file to have drifted from.
 *
 *  `note` is the BACKEND's wording, shared word for word with the
 *  `workflow-changed-since-launch` audit row, so the chip and the trail cannot
 *  say different things about one divergence. This adds the sentence the chip
 *  needs and the audit row does not: what the human can DO about it. */
export function driftChip(status: WorkflowStatus | null): DriftChip | null {
  const drift = status?.drift;
  if (!drift) return null;
  const ids = drift.on_disk_blocks;
  const onDisk = ids.length
    ? `The file now declares: ${ids.join(", ")}.`
    : "The file declares nothing this group could run.";
  return {
    text: `drift: ${drift.note}`,
    title:
      `This group is still running the roster it pinned — ${drift.note}. ${onDisk} ` +
      "Nothing changes behind your back: Review & apply this workflow to adopt it.",
  };
}

/** The confirmation the Review & apply modal renders. */
export interface SwitchConfirm {
  title: string;
  /** The modal body, one line per statement. */
  lines: string[];
  /** The affirmative button's label. */
  affirm: string;
  /** Whether applying is offered at all. `false` makes this an explanation with
   *  no action — the modal still opens, because a bare refusal that names
   *  neither the diff nor the reason is worse than one that explains. */
  canApply: boolean;
}

/** Turn a resolved preview into the confirmation a human reads before a live
 *  group's roster changes.
 *
 *  Every statement is derived from the preview the BACKEND resolved, never
 *  re-derived from the status: the same resolution runs behind the preview and
 *  the apply, so the diff shown and the diff applied are one value, and this
 *  module's job is to say it rather than to compute it again.
 *
 *  Three states get no apply button:
 *
 *  - **`refusal`** — a change a live switch cannot make however the human
 *    answers (today, only the orchestrator block's CLI, because that pane is
 *    already running a program). The diff is shown beside it.
 *  - **`empty`** — nothing would change. Offering a confirmation for that would
 *    be asking a human to authorize nothing.
 *  - **`digest === null`** — loomux could not fingerprint the file. `applyWorkflow`
 *    accepts a `null` digest and audits it as "the caller had no confirmation to
 *    honour", which is exactly the wrong record to write for an apply a human
 *    just read a diff and clicked through: the trail would say nobody confirmed
 *    while somebody did. Declining is the consent-preserving direction, and the
 *    line says so. */
export function switchConfirm(preview: WorkflowSwitchPreview): SwitchConfirm {
  const lines: string[] = [];
  const target = preview.display_name.trim()
    ? `${preview.name} — "${preview.display_name.trim()}" (${preview.path})`
    : `${preview.name} (${preview.path})`;
  lines.push(`Switching from ${preview.from} to ${target}.`);

  if (preview.refusal) {
    lines.push(`Cannot be applied to a running group: ${preview.refusal}`);
  }

  if (preview.empty) {
    lines.push(
      "Nothing would change — this file resolves to the roster, gate and intake labels this group is already running."
    );
  } else {
    lines.push(...diffLines(preview));
  }

  if (!preview.refusal && !preview.empty && preview.digest === null) {
    lines.push(
      "loomux could not fingerprint this file, so an apply could not be bound to the diff above. " +
        "Re-open the diff once the file is readable."
    );
  }

  if (preview.refusal || !preview.empty) {
    // True of every apply, and worth saying next to the removals rather than
    // only in the docs: it is the reason a switch is safe to make on a group
    // with agents live in it.
    lines.push("Agents already running keep the block they were spawned under.");
  }

  const canApply = !preview.refusal && !preview.empty && preview.digest !== null;
  return {
    title: preview.empty ? "Nothing to apply" : `Apply workflow "${preview.name}"?`,
    lines,
    affirm: "Apply workflow",
    canApply,
  };
}

/** The diff, as the sentences a human decides on. Each axis is skipped when it
 *  is empty, so a one-key model change reads as one line rather than as six
 *  headings with five "none"s under them. */
function diffLines(preview: WorkflowSwitchPreview): string[] {
  const d = preview.diff;
  const out: string[] = [];
  if (d.added.length) {
    out.push(`Adds ${plural(d.added.length, "block")}: ${d.added.join(", ")}.`);
  }
  if (d.removed.length) {
    out.push(
      `Removes ${plural(d.removed.length, "block")}: ${d.removed.join(", ")} — ` +
        "a pane already running under one keeps running, but a bare resume of its session is refused afterwards."
    );
  }
  for (const c of d.changed) {
    out.push(`Changes ${c.id}: ${c.fields.join(", ")}.`);
  }
  if (d.gate_changed) {
    const gate = gateLine(preview.gate);
    out.push(gate ? `Re-arms the merge gate: ${gate}.` : "Clears the armed merge gate.");
  }
  // Satisfiability is computed against the roster this switch would INSTALL, so
  // it can be said before the click rather than discovered by a merge that
  // bounces. Reported whenever the new gate is unsatisfiable, not only when the
  // gate changed: the same gate can become unsatisfiable because the ROSTER
  // moved under it.
  if (preview.gate && !preview.gate.satisfiable) {
    const missing = preview.gate.missing_blocks;
    out.push(
      `The gate this leaves armed names ${missing.join(", ")}, which the new roster cannot spawn — merges will bounce.`
    );
  }
  if (d.intake_changed) {
    out.push(
      "Changes the intake label vocabulary this group answers to, including the hold label your own vetoes use."
    );
  }
  if (preview.next_resume.length) {
    out.push(
      `The orchestrator pane keeps its current ${preview.next_resume.join(", ")} until it is resumed — ` +
        "the new value is written to the group now and picked up then."
    );
  }
  return out;
}

const plural = (n: number, word: string): string => `${n} ${word}${n === 1 ? "" : "s"}`;

export type { WorkflowStatus, WorkflowSwitchPreview };
