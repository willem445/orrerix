// The PRE-RUN validation pass over a workflow model (#222; split out of workflowmodel.ts by
// #3498 F2): `validateWorkflow` and the finding builders it composes. Imports only
// workflowtypes.ts, plus the one type-only import below. Design note:
// docs/design/workflows.md; module map: docs/design/architecture.md.

import {
  BLOCK_KINDS,
  isBlockKind,
  WORKFLOW_CLIS,
  isWorkflowCli,
  ROLE_HINTS,
  roleHintRequires,
  personaDenialReason,
  allowDenialReason,
  REMOTE_LABEL_MAX,
  isRemoteLabel,
  remoteDenialReason,
  sanitizeAllowPattern,
  GATE_REQUIRES,
  GATE_REQUIRES_ACCEPTED,
  INTAKE_SOURCES,
  isIntakeSource,
  ID_MAX_CHARS,
  isValidIntakeLabel,
  isValidResourceName,
  RESOURCE_SLOTS_MIN,
  RESOURCE_SLOTS_MAX,
  RESOURCE_MAX_HOLD_MINUTES_MIN,
  RESOURCE_MAX_HOLD_MINUTES_MAX,
  RESOURCES_MAX,
  MERGE_QUEUE_MAX_BATCH_MIN,
  GATE_THRESHOLD_MIN,
  GATE_MAX_DIFF_LINES_MIN,
  GATE_ROUTING_RULES_MAX,
  GATE_ROUTING_PATHS_MAX,
  isRoutingGlob,
  MERGE_QUEUE_CHECKS_TIMEOUT_MIN,
  MERGE_QUEUE_CHECKS_TIMEOUT_MAX,
  DRIVER_MAX_REVIEW_ROUNDS_MIN,
  DRIVER_MAX_REVIEW_ROUNDS_MAX,
  DRIVER_MAX_CI_ATTEMPTS_MIN,
  DRIVER_MAX_CI_ATTEMPTS_MAX,
  DRIVER_MAX_REBASE_ATTEMPTS_MIN,
  DRIVER_MAX_REBASE_ATTEMPTS_MAX,
  DRIVER_TIMEOUT_MIN,
  DRIVER_TIMEOUT_MAX,
  DRIVER_DRIVE_TIMEOUT_MIN,
  DRIVER_DRIVE_TIMEOUT_MAX,
  isValidBlockId,
  INTAKE_LABEL_KEYS,
} from "./workflowtypes.ts";
import type {
  YamlValue,
  WorkflowBlock,
  Workflow,
  FindingCode,
  FindingSection,
  Finding,
} from "./workflowtypes.ts";

// The one import, and it is TYPE-only: the capability answer is handed in by the
// caller (see `KnobLookup`), never imported, because this module mirrors no
// vendor fact of its own — see `validateWorkflow`'s knob pass.
import type { KnobStates } from "./selectorknobs";

// ---------- validate: the pre-run pass ----------

/** How a caller answers "what can this block's cli and model actually carry?" —
 *  normally `(cli, model) => knobState(fetched[cli] ?? null, cli, model)`
 *  (selectorknobs.ts). **`null` means "not known yet"**, and is not the same
 *  answer as "cannot": the pane fetches `agent_cli_knobs` per CLI
 *  asynchronously, and a knob check that ran before the reply landed would
 *  invent a finding out of its own ignorance.
 *
 *  Injected rather than imported so this module's "capability is the backend's
 *  to state, never mirrored here" rule holds at the module-graph level too: the
 *  pure model has no runtime dependency on the capability layer at all. */
export type KnobLookup = (cli: string, model: string) => KnobStates | null;

/** The `effort:` / `context:` half of the pre-run pass (#687).
 *
 *  Three ways a knob is undeliverable, all of them a REFUSAL on the real engine
 *  (`validate_knob`, workflow.rs) rather than a silent no-op, so a pane that
 *  reported the file clean would send a human to a launch that quietly falls back
 *  to the built-in roster:
 *
 *    - a value outside the CLI's own vocabulary (`effort: banana`);
 *    - a CLI with no seam for the knob at all (copilot's effort);
 *    - a model the knob has no documented form on — `haiku` + `context: 1m`
 *      composes `--model haiku[1m]`, which is #709's carried finding, caught here
 *      at the surface a human hand-writes the file in.
 *
 *  **It DEFERS rather than guesses.** A `lookup` that returns `null` — no caps
 *  fetched for this block's CLI yet, or none passed at all — produces no
 *  findings, exactly as the real parser skips the CLI half for a block with no
 *  explicit `cli:` and lets `clamped()` re-check with the resolved CLI in hand. */
function knobFindings(b: WorkflowBlock, where: string, lookup: KnobLookup | undefined): Finding[] {
  if (b.effort === undefined && b.context === undefined) return [];
  const states = lookup?.(b.cli.trim(), b.model);
  if (!states) return [];
  const out: Finding[] = [];
  for (const [key, declared, state] of [
    ["effort", b.effort, states.effort],
    ["context", b.context, states.context],
  ] as const) {
    const v = (declared ?? "").trim();
    if (!v) continue; // absent or empty = the CLI's own default, always legal
    if (state.enabled && state.values.includes(v.toLowerCase())) continue;
    out.push({
      severity: "error",
      code: "knob-unavailable",
      message:
        `Block "${where}" declares ${key}: ${v}, which orrerix cannot deliver on ${b.cli} — ` +
        (state.reason || `${key} must be one of ${state.values.join(", ")}.`),
      blockId: b.id,
    });
  }
  return out;
}

// ONE definition for both reviewer lists — the gate's own and every routing
// rule's (#1176). They ask the same question ("could a verdict for this id
// ever be recorded?"), and answering it twice is how the static list ends up
// refusing a manager while a routing rule quietly accepts one. `subject`
// names which list, so the finding points at the line to fix; the backend's
// `gate_reviewer_error` is the same function on the other side. It sits at module
// scope rather than inside `validateWorkflow` because the CANVAS asks it too, before
// it lets a rubber band land on the gate (`gateConnectionError`, #1388) — the pane
// refusing a drop for a different reason than it reports a finding for is the same
// drift, one gesture earlier.
export function gateReviewerFinding(
  byId: ReadonlyMap<string, WorkflowBlock>,
  subject: string,
  id: string
): Finding | null {
  const b = byId.get(id);
  if (!b) {
    return {
      severity: "error",
      code: "gate-unknown-reviewer",
      message: `${subject} requires a verdict from "${id}", but no block has that id — the gate could never open.`,
    };
  }
  if (b.kind === "manager") {
    // Reached before the generic kind arm below, which would otherwise
    // describe this as a type error ("that block's kind is manager").
    // An author who named the manager on a gate meant "the human signs
    // off", which is real and which this gate cannot express — so say
    // that. Mirrors the backend's own arm in `parse_workflow` (#1161).
    return {
      severity: "error",
      code: "gate-not-a-reviewer",
      message: `${subject} names "${id}" as a reviewer, but that block is the manager — the human's interface, which records no verdict, so the gate could never open. A gate reads reviewer verdicts; the human's own sign-off is the merge gate orrerix already applies on top of it.`,
      blockId: id,
    };
  }
  if (b.kind !== "reviewer") {
    return {
      severity: "error",
      code: "gate-not-a-reviewer",
      message: `${subject} names "${id}" as a reviewer, but that block's kind is "${b.kind || "(none)"}" — only a reviewer records a verdict.`,
      blockId: id,
    };
  }
  if (b.role_hint?.trim().toLowerCase() === "liaison") {
    // Reviewer-KIND, but a liaison never records a verdict (#891) — so a
    // gate naming one waits on something no code path can produce. Same
    // unsatisfiable-gate finding as the arm above, one kind further in;
    // mirrors the backend's own refusal in `parse_workflow`.
    return {
      severity: "error",
      code: "gate-not-a-reviewer",
      message: `${subject} names "${id}" as a reviewer, but that block is a liaison — it presents the human's questions and never records a verdict, so the gate could never open.`,
      blockId: id,
    };
  }
  return null;
}

/** Everything that is wrong with this workflow, before a single agent is spawned.
 *
 *  This is the pass every surveyed tool skipped (#222 §1a-v): Flowise, Langflow and Dify
 *  all discover a dangling reference at RUN time, and Dify will publish a workflow whose
 *  node isn't even installed. It is cheap, it is pure, and it is the difference between
 *  "your workflow failed after spawning two agents" and "block `rev-perf` doesn't exist —
 *  the merge gate names it". */
export function validateWorkflow(w: Workflow, knobs?: KnobLookup): Finding[] {
  const findings: Finding[] = [];
  const byId = new Map<string, WorkflowBlock>();

  if (!w.blocks.length) {
    findings.push({
      severity: "error",
      code: "no-blocks",
      message: "This workflow declares no blocks — add at least one agent block.",
    });
  }

  const seen = new Set<string>();
  for (const b of w.blocks) {
    const where = b.id || b.name;
    if (!b.id) {
      findings.push({
        severity: "error",
        code: "block-id-missing",
        message: `A block has no id. The id is the block's identity — edges and gates reference it.`,
        blockId: b.id,
      });
    } else if (!isValidBlockId(b.id)) {
      findings.push({
        severity: "error",
        code: "block-id-invalid",
        message: `"${b.id}" is not a valid block id — use lowercase letters, digits, - and _ (e.g. rev-security).`,
        blockId: b.id,
      });
    } else if (seen.has(b.id)) {
      findings.push({
        severity: "error",
        code: "block-id-duplicate",
        message: `Two blocks share the id "${b.id}" — an edge or a gate naming it would be ambiguous.`,
        blockId: b.id,
      });
    }
    if (b.id) {
      seen.add(b.id);
      if (!byId.has(b.id)) byId.set(b.id, b);
    }

    if (!isBlockKind(b.kind)) {
      findings.push({
        severity: "error",
        code: "unknown-kind",
        message: b.kind
          ? `Block "${where}" has kind "${b.kind}", which is not one of ${BLOCK_KINDS.join(", ")}. A workflow can define any persona, but never a new capability class.`
          : `Block "${where}" has no kind — pick one of ${BLOCK_KINDS.join(", ")}.`,
        blockId: b.id,
      });
    }
    if (!isWorkflowCli(b.cli)) {
      findings.push({
        severity: "error",
        code: "unknown-cli",
        message: b.cli
          ? `Block "${where}" runs cli "${b.cli}", which orrerix cannot spawn (supported: ${WORKFLOW_CLIS.join(", ")}).`
          : `Block "${where}" has no cli — pick one of ${WORKFLOW_CLIS.join(", ")}.`,
        blockId: b.id,
      });
    }
    if (b.prompt !== undefined && b.profile !== undefined) {
      findings.push({
        severity: "error",
        code: "prompt-and-profile",
        message: `Block "${where}" declares both a prompt and a profile — pick one. (An inline prompt compiles to the CLI's native inline agent; a profile points at a file the CLI loads by name.)`,
        blockId: b.id,
      });
    }
    // The engine refuses a persona on a loomux-owned block outright, failing the
    // WHOLE file — so a pane that reported this clean would let an author save a
    // workflow that silently launches on the built-in roster instead. Reported
    // whichever key carries it, since `parse_workflow` names both.
    if (b.prompt !== undefined || b.profile !== undefined) {
      const denial = personaDenialReason(b.kind);
      if (denial) {
        const key = b.prompt !== undefined ? "prompt:" : "profile:";
        findings.push({
          severity: "error",
          code: "persona-not-permitted",
          message: `Block "${where}" declares ${key}, which a ${b.kind} block may not — ${denial}.`,
          blockId: b.id,
        });
      }
    }
    if (b.role_hint !== undefined) {
      const required = roleHintRequires(b.role_hint);
      if (!required) {
        findings.push({
          severity: "error",
          code: "role-hint-unknown",
          message: `Block "${where}" has role_hint "${b.role_hint}", which is not one of ${ROLE_HINTS.join(", ")}.`,
          blockId: b.id,
        });
      } else if (required !== b.kind) {
        findings.push({
          severity: "error",
          code: "role-hint-wrong-kind",
          message: `Block "${where}" has role_hint "${b.role_hint}", which requires kind: ${required} (this block is kind: "${b.kind}").`,
          blockId: b.id,
        });
      } else if (b.role_hint.trim().toLowerCase() === "liaison") {
        // A WARNING, never an error: the hint parses on the real engine and the
        // group runs exactly as it did (#1161 decision D4). What the author is
        // owed is where the feature went — a hint on a reviewer cannot express
        // what the manager class is, and someone reading a file written before
        // that class existed has no way to know a better shape is available.
        findings.push({
          severity: "warning",
          code: "role-hint-superseded",
          message: `Block "${where}" has role_hint: liaison, which is superseded by kind: manager — the first-class human-interface class. The hint still parses and this block still runs exactly as it does today; a manager instead gets its own capability class, a durable mailbox from the orchestrator, and a pane no fleet traffic is delivered into. See docs/orchestration.md.`,
          blockId: b.id,
        });
      }
    }
    // #1457. Mirrors `parse_workflow`'s three refusals, in its order, for the
    // reason `personaDenialReason` states: each of them fails the WHOLE file on
    // the engine, so a pane that reported one clean would let an author save a
    // workflow whose launch silently falls back to the built-in roster.
    if (b.remote !== undefined) {
      const denial = remoteDenialReason(b.kind);
      if (!isRemoteLabel(b.remote)) {
        findings.push({
          severity: "error",
          code: "remote-invalid-label",
          message: `Block "${where}" has remote "${b.remote}", which is not a usable label — letters, digits, '-' and '_' only, at most ${REMOTE_LABEL_MAX} characters, and not starting with '-'. A remote label is an abstract name the OPERATOR binds to a host outside this repo, never an address.`,
          blockId: b.id,
        });
      } else if (denial) {
        findings.push({
          severity: "error",
          code: "remote-not-permitted",
          message: `Block "${where}" declares remote:, which a ${b.kind} block may not — ${denial}.`,
          blockId: b.id,
        });
      } else if (b.cli !== "claude") {
        findings.push({
          severity: "error",
          code: "remote-requires-claude",
          message: b.cli
            ? `Block "${where}" declares remote: with cli "${b.cli}" — a remote block must run cli: claude, the only CLI orrerix drives remotely today.`
            : `Block "${where}" declares remote: with no cli — a remote block must spell out cli: claude. An omitted cli inherits the group default, which is picked at launch, so orrerix cannot check it here.`,
          blockId: b.id,
        });
      }
    }
    findings.push(...allowFindings(b, where));
    findings.push(...knobFindings(b, where, knobs));
  }

  // At most one manager (#1161) — mirrors the backend's own post-loop check in
  // `parse_workflow`. A roster property, not a block one: the second
  // declaration is no more wrong than the first, so the finding names them all
  // and anchors on none.
  const managers = w.blocks.filter((b) => b.kind === "manager").map((b) => b.id || "(no id)");
  if (managers.length > 1) {
    findings.push({
      severity: "error",
      code: "manager-not-unique",
      message: `${managers.length} blocks declare kind: manager (${managers.join(", ")}) — a workflow may declare at most one. The manager is the human's single interface to the group: two would each hold half a conversation.`,
    });
  }

  for (const e of w.edges) {
    for (const [end, id] of [
      ["from", e.from],
      ["to", e.to],
    ] as const) {
      if (!byId.has(id)) {
        findings.push({
          severity: "error",
          code: "edge-unknown-block",
          message: `The edge ${e.from} → ${e.to} names a block that doesn't exist: "${id}" (${end}:).`,
        });
      }
    }
    if (e.from && e.from === e.to) {
      findings.push({
        severity: "error",
        code: "edge-self",
        message: `Block "${e.from}" has an edge to itself.`,
        blockId: e.from,
      });
    }
  }

  const gate = w.gates.merge;
  if (gate) {
    if (!(GATE_REQUIRES_ACCEPTED as readonly string[]).includes(gate.require)) {
      findings.push({
        severity: "error",
        code: "gate-unknown-require",
        message: `The merge gate requires "${gate.require}", which is not one of ${GATE_REQUIRES.join(", ")}.`,
      });
    }
    if (!gate.reviewers.length) {
      findings.push({
        severity: "error",
        code: "gate-no-reviewers",
        message: "The merge gate names no reviewers — a gate with nothing to wait for gates nothing.",
      });
    }
    // `gateReviewerFinding` (module scope, above) is the ONE definition — the canvas's
    // `gateConnectionError` asks it the same question before it lets a rubber band land
    // on the gate (#1388), and a second copy here is how the two would drift apart.
    const reviewerFinding = (subject: string, id: string): Finding | null =>
      gateReviewerFinding(byId, subject, id);
    for (const id of gate.reviewers) {
      const f = reviewerFinding("The merge gate", id);
      if (f) findings.push(f);
    }
    // #1176's routing rules. Every refusal below is one the ENGINE refuses the
    // whole file over, so a pane that stayed quiet would be blessing a workflow
    // that will not load — and what it would have blessed is a gate that
    // silently stopped requiring a lane.
    (gate.routing ?? []).forEach((rule, i) => {
      const subject = `Routing rule ${i + 1}`;
      if (!rule.paths.length) {
        findings.push({
          severity: "error",
          code: "gate-bad-routing",
          message: `${subject} declares no paths — a rule that matches nothing can never require anybody.`,
        });
      }
      if (!rule.reviewers.length) {
        findings.push({
          severity: "error",
          code: "gate-bad-routing",
          message: `${subject} names no reviewers — a rule that requires nobody is not a rule.`,
        });
      }
      if (rule.paths.length > GATE_ROUTING_PATHS_MAX) {
        findings.push({
          severity: "error",
          code: "gate-bad-routing",
          message: `${subject} declares ${rule.paths.length} paths — at most ${GATE_ROUTING_PATHS_MAX}.`,
        });
      }
      for (const p of rule.paths) {
        if (!isRoutingGlob(p)) {
          findings.push({
            severity: "error",
            code: "gate-bad-routing",
            message: `${subject}: "${p}" is not a usable path glob. Use letters, digits, '.', '_', '-', '/' and '*' — and write a file glob, not a directory: "src/**", never "src/", "/src/**" or a ".." segment, which match nothing at all.`,
          });
        }
      }
      for (const id of rule.reviewers) {
        const f = reviewerFinding(subject, id);
        if (f) findings.push(f);
      }
    });
    if ((gate.routing?.length ?? 0) > GATE_ROUTING_RULES_MAX) {
      findings.push({
        severity: "error",
        code: "gate-bad-routing",
        message: `The merge gate declares ${gate.routing!.length} routing rules — at most ${GATE_ROUTING_RULES_MAX}.`,
      });
    }
    // The pair with no honest reading: a threshold counts passes over a FIXED
    // list, routing makes the list depend on the diff, so together an extra lane
    // could SUPPLY one of the N passes instead of adding one — the gate would get
    // easier to satisfy the more rules you wrote. The engine refuses the file.
    if (gate.routing?.length && gate.require === "threshold") {
      findings.push({
        severity: "error",
        code: "gate-bad-routing",
        message:
          "routing: and require: threshold cannot both be declared — a threshold counts passes over a fixed reviewer list, and a routing rule makes that list depend on the diff. Use require: all-pass with routing:.",
      });
    }
    // #1174. UNCONDITIONAL — unlike `threshold`, which is only meaningful under
    // `require: threshold` and so is only checked there. This clause has no mode to
    // hide behind: a `max_diff_lines: 0` is a file the engine refuses whatever else
    // the gate says, and a pane that stayed quiet about it would be blessing a
    // workflow that will not load.
    if (gate.max_diff_lines !== undefined) {
      const n = gate.max_diff_lines;
      if (!Number.isInteger(n) || n < GATE_MAX_DIFF_LINES_MIN) {
        findings.push({
          severity: "error",
          code: "gate-bad-max-diff-lines",
          message: `max_diff_lines: ${n} — a merge gate's size limit must be a whole number ≥ ${GATE_MAX_DIFF_LINES_MIN}. Omit the key to declare no limit; 0 is refused, not read as "unlimited".`,
        });
      }
    }
    // The other half of "which gate is a threshold gate" (#1388 review N1). With the
    // shorthand normalised in `readGate`, a model that says all-pass AND carries a number got
    // there by spelling both out — or by the gate form's picker being switched to all-pass
    // over a threshold that is still in the file — and the engine refuses that pair outright
    // ("require: all-pass takes no threshold — drop it, or use require: threshold"). A pane
    // that stayed quiet would be blessing a workflow that will not load. Scoped to the two
    // accepted all-pass spellings so an UNKNOWN require still gets one finding, not two.
    if ((gate.require === "all-pass" || gate.require === "all") && gate.threshold !== undefined) {
      findings.push({
        severity: "error",
        code: "gate-bad-threshold",
        message: `The merge gate says require: ${gate.require} and also names threshold: ${gate.threshold} — the engine refuses the pair outright. Drop the threshold, or use require: threshold.`,
      });
    }
    if (gate.require === "threshold") {
      const t = gate.threshold;

      if (t === undefined || !Number.isInteger(t) || t < GATE_THRESHOLD_MIN) {
        findings.push({
          severity: "error",
          code: "gate-bad-threshold",
          message: 'A "threshold" merge gate needs threshold: N with N ≥ 1.',
        });
      } else if (t > gate.reviewers.length) {
        findings.push({
          severity: "error",
          code: "gate-bad-threshold",
          message: `The merge gate needs ${t} passing reviews but names only ${gate.reviewers.length} reviewer(s) — it could never open.`,
        });
      }
    }
  }

  findings.push(...sectionFindings(w));
  findings.push(...unknownKeyFindings(w));
  findings.push(...reachabilityFindings(w, byId));
  return findings;
}

/** The `allow:` half of the pre-run pass (#1020) — two rules, and they fail in opposite
 *  directions, which is why they carry different severities.
 *
 *  A kind that may not declare `allow:` at all is a REFUSAL on the engine
 *  (`parse_workflow`): the file does not load, so it is an error here. A pattern the
 *  engine's `sanitize_allow` would REWRITE is the quieter one and arguably the worse: the
 *  file loads, the agent spawns, and the tool pattern it was pre-approved for is not the
 *  one anybody wrote — `Bash(echo "$X")` reaches the CLI as `Bash(echo X)`, which matches
 *  nothing. Nothing downstream can tell the human that; this can. */
function allowFindings(b: WorkflowBlock, where: string): Finding[] {
  if (!b.allow?.length) return [];
  const denial = allowDenialReason(b.kind);
  if (denial) {
    return [
      {
        severity: "error",
        code: "allow-not-permitted",
        message: `Block "${where}" declares allow:, which a ${b.kind} block may not — ${denial}.`,
        blockId: b.id,
      },
    ];
  }
  const out: Finding[] = [];
  for (const pattern of b.allow) {
    const clean = sanitizeAllowPattern(pattern);
    if (clean === null) {
      out.push({
        severity: "warning",
        code: "allow-sanitized",
        message:
          `Block "${where}" declares the allow: pattern "${pattern}", which has no characters ` +
          `orrerix can pass to the CLI — it is dropped, and pre-approves nothing.`,
        blockId: b.id,
      });
    } else if (clean !== pattern.trim()) {
      out.push({
        severity: "warning",
        code: "allow-sanitized",
        message:
          `Block "${where}" declares the allow: pattern "${pattern}", but orrerix passes only ` +
          `letters, digits and ( ) : * _ - . / , and spaces — the CLI will be given "${clean}".`,
        blockId: b.id,
      });
    }
  }
  return out;
}

/** The policy sections' half of the pre-run pass (#1020): `intake:`, `merge_queue:` and
 *  `resources:`, checked against the same bounds and vocabularies `parse_workflow` uses.
 *
 *  All of these are things the pane can now WRITE, which is exactly why it must also be
 *  able to say them: a form that offers a number the engine refuses turns a config screen
 *  into a way to break your own workflow file, and the pane reporting "valid" over a file
 *  that will not load is the failure mode this whole pass exists to prevent. Each finding
 *  carries its `section`, so clicking it lands on the form that can fix it. */
function sectionFindings(w: Workflow): Finding[] {
  const out: Finding[] = [];
  const err = (section: FindingSection, code: FindingCode, message: string): void => {
    out.push({ severity: "error", code, message, section });
  };

  if (w.intake) {
    if (w.intake.source !== undefined && !isIntakeSource(w.intake.source)) {
      err(
        "intake",
        "intake-unknown-source",
        `intake.source: "${w.intake.source}" is not a source orrerix knows — use one of ${INTAKE_SOURCES.join(", ")} (or leave it out to inherit).`
      );
    }
    for (const key of INTAKE_LABEL_KEYS) {
      const v = w.intake.labels?.[key];
      if (v !== undefined && !isValidIntakeLabel(v)) {
        err(
          "intake",
          "intake-bad-label",
          `intake.labels.${key}: "${v}" is not a usable label — letters, digits, - and _, no leading -, at most ${ID_MAX_CHARS} characters. ` +
            `orrerix rejects it rather than rewriting it, so your repo's own labels keep matching.`
        );
      }
    }
  }

  const mq = w.merge_queue;
  if (mq) {
    if (
      mq.max_batch !== undefined &&
      (!Number.isInteger(mq.max_batch) || mq.max_batch < MERGE_QUEUE_MAX_BATCH_MIN)
    ) {
      err(
        "merge_queue",
        "section-out-of-range",
        `merge_queue.max_batch: ${mq.max_batch} — a batch must carry at least ${MERGE_QUEUE_MAX_BATCH_MIN} PR, and a batch of none could never land anything.`
      );
    }
    const timeout = mq.checks_timeout_minutes;
    if (timeout !== undefined && Number.isInteger(timeout)) {
      if (timeout < MERGE_QUEUE_CHECKS_TIMEOUT_MIN || timeout > MERGE_QUEUE_CHECKS_TIMEOUT_MAX) {
        // CLAMPED by the engine, not refused — so this is a warning, and it says what will
        // actually happen rather than implying the file is broken.
        out.push({
          severity: "warning",
          code: "section-out-of-range",
          message:
            `merge_queue.checks_timeout_minutes: ${timeout} is outside ${MERGE_QUEUE_CHECKS_TIMEOUT_MIN}–${MERGE_QUEUE_CHECKS_TIMEOUT_MAX}, ` +
            `so orrerix will clamp it — the queue will not wait for the time this file names.`,
          section: "merge_queue",
        });
      }
    }
  }

  const dv = w.driver;
  if (dv) {
    // Every driver field is a `u32` on the engine, so serde refuses a value's
    // TYPE before any range or clamp logic runs: a float (`2.5`), a negative
    // (`-1`), anything above 4_294_967_295. A `Number.isInteger`-guarded check
    // goes silent on the first, and an out-of-DECLARED-range check turns the
    // other two into clamp warnings - both bless files the engine refuses at
    // load (#1784 review rounds 1-2). So the type class is tested FIRST, by
    // this one predicate, and the message says refusal, not clamping.
    const outsideU32 = (v: number): boolean => v < 0 || v > 4294967295;
    // The INVARIANT-9 counters are REFUSED by the engine (#1778 2.3) outside
    // their closed range: an error here, because a repo file may tighten
    // INVARIANT 9 but never loosen it. A file this pane calls valid must load.
    const counter = (field: string, v: number | undefined, min: number, max: number): void => {
      if (v === undefined) return;
      if (!Number.isInteger(v) || outsideU32(v)) {
        err(
          "driver",
          "section-bad-value",
          `driver.${field}: ${v} is not a value the engine's u32 field accepts - the engine ` +
            `refuses the type before any range check runs.`
        );
        return;
      }
      if (v < min || v > max) {
        err(
          "driver",
          "section-out-of-range",
          `driver.${field}: ${v} must be an integer in ${min}-${max} - the engine refuses the ` +
            `whole file, because a repo file may tighten INVARIANT 9 but never loosen it.`
        );
      }
    };
    counter(
      "max_review_rounds",
      dv.max_review_rounds,
      DRIVER_MAX_REVIEW_ROUNDS_MIN,
      DRIVER_MAX_REVIEW_ROUNDS_MAX
    );
    counter(
      "max_ci_attempts",
      dv.max_ci_attempts,
      DRIVER_MAX_CI_ATTEMPTS_MIN,
      DRIVER_MAX_CI_ATTEMPTS_MAX
    );
    counter(
      "max_rebase_attempts",
      dv.max_rebase_attempts,
      DRIVER_MAX_REBASE_ATTEMPTS_MIN,
      DRIVER_MAX_REBASE_ATTEMPTS_MAX
    );
    // The backstops are CLAMPED, like `checks_timeout_minutes` - an
    // out-of-range INTEGER inside the u32 type is a warning that says what
    // will actually happen. But the TYPE class comes first: a non-integer
    // (`2.5`) and an integer outside u32 (`-1`, `4294967296`) are refused by
    // the engine outright, and a clamp warning about a file that refuses to
    // load is the same lie in a friendlier tone (#1784 review round 2).
    const backstop = (
      field: string,
      v: number | undefined,
      min = DRIVER_TIMEOUT_MIN,
      max = DRIVER_TIMEOUT_MAX
    ): void => {
      if (v === undefined) return;
      if (!Number.isInteger(v) || outsideU32(v)) {
        err(
          "driver",
          "section-bad-value",
          `driver.${field}: ${v} is not a value the engine's u32 field accepts - the engine ` +
            `refuses the type before any clamp runs.`
        );
        return;
      }
      if (v < min || v > max) {
        out.push({
          severity: "warning",
          code: "section-out-of-range",
          message:
            `driver.${field}: ${v} is outside ${min}-${max}, ` +
            `so orrerix will clamp it - the driver will not wait for the time this file names.`,
          section: "driver",
        });
      }
    };
    backstop("lane_timeout_minutes", dv.lane_timeout_minutes);
    backstop("fix_timeout_minutes", dv.fix_timeout_minutes);
    // #2110: its own range, not the family's - see DRIVER_DRIVE_TIMEOUT_MAX.
    backstop(
      "drive_timeout_minutes",
      dv.drive_timeout_minutes,
      DRIVER_DRIVE_TIMEOUT_MIN,
      DRIVER_DRIVE_TIMEOUT_MAX
    );
  }

  const resources = w.resources;
  if (resources) {
    const names = Object.keys(resources);
    if (names.length > RESOURCES_MAX) {
      err(
        "resources",
        "section-out-of-range",
        `resources: ${names.length} declared — at most ${RESOURCES_MAX} are allowed (every name is listed in the acquire_lock tool description every agent in the group reads).`
      );
    }
    for (const name of names) {
      if (!isValidResourceName(name)) {
        err(
          "resources",
          "resource-name-invalid",
          `resources: "${name}" is not a usable resource name — letters, digits, - and _, at most ${ID_MAX_CHARS} characters. ` +
            `orrerix rejects it rather than rewriting it, so the name an agent's acquire_lock call uses is the name you wrote.`
        );
      }
      const r = resources[name]!;
      const bound = (
        key: "slots" | "max_hold_minutes",
        min: number,
        max: number,
        why: string
      ): void => {
        const v = r[key];
        if (v === undefined) return;
        if (!Number.isInteger(v) || v < min || v > max) {
          err(
            "resources",
            "section-out-of-range",
            `resources.${name}.${key}: ${v} is outside ${min}–${max} — ${why}.`
          );
        }
      };
      bound(
        "slots",
        RESOURCE_SLOTS_MIN,
        RESOURCE_SLOTS_MAX,
        "a resource with no slots could never be acquired, and past the maximum a declaration serializes nothing"
      );
      bound(
        "max_hold_minutes",
        RESOURCE_MAX_HOLD_MINUTES_MIN,
        RESOURCE_MAX_HOLD_MINUTES_MAX,
        "a hold that expires as it is granted serializes nothing, and one on a scarce resource has to be bounded by something a working session outlives"
      );
    }
  }
  return out;
}

/** Every key this build doesn't know, wherever it sits (#880).
 *
 *  The pane PRESERVES unknown keys — a file written by a newer loomux must survive a
 *  round-trip through an older pane rather than be quietly stripped by it (that is
 *  what `extra` is for, and it stays). But preserving alone was a half-truth: this
 *  build's engine is `deny_unknown_fields`, so a single typo (`promt:`) makes
 *  `parse_workflow` refuse the WHOLE file — gates and all — down the loud
 *  `workflow-invalid` path, while the pane cheerfully rendered "valid". Preserving and
 *  warning is the honest pair; dropping is destructive and ignoring is a lie.
 *
 *  `gates:` is exempt, and that is not an oversight: it is a map keyed by gate NAME
 *  (`BTreeMap<String, RawGate>`), so a gate loomux doesn't enforce still parses. See
 *  `KNOWN_GATE`. */
function unknownKeyFindings(w: Workflow): Finding[] {
  const out: Finding[] = [];
  const report = (
    where: string,
    extra: Record<string, YamlValue> | undefined,
    blockId?: string,
    section?: FindingSection
  ): void => {
    for (const key of Object.keys(extra ?? {})) {
      out.push({
        severity: "error",
        code: "unknown-key",
        message:
          `${where} declares "${key}", which is not part of the workflow schema — ` +
          `this build's engine refuses unknown keys, so the file will not load. ` +
          `(The pane keeps the line as written; check the spelling, or the file needs a newer orrerix.)`,
        blockId,
        section,
      });
    }
  };
  report("This workflow", w.extra);
  for (const b of w.blocks) report(`Block "${b.id || b.name}"`, b.extra, b.id || undefined);
  if (w.intake) report("intake:", w.intake.extra);
  if (w.intake?.labels) report("intake.labels:", w.intake.labels.extra);
  if (w.merge_queue) report("merge_queue:", w.merge_queue.extra);
  // #1778. The optional `section` routes the finding onto the driver's
  // read-only summary, the same promise the FindingSection member makes - a
  // mistyped key is the likeliest driver authoring error, and it must not be
  // the one finding the driver surface cannot show. (`board:`'s missing line
  // below is a pre-existing gap, not this section's.)
  if (w.driver) report("driver:", w.driver.extra, undefined, "driver");
  if (w.triage) report("triage:", w.triage.extra);
  for (const [name, r] of Object.entries(w.resources ?? {})) report(`resources.${name}:`, r.extra);
  return out;
}

/** The two structural warnings — a block nothing points at, and a block nothing can
 *  reach. Both are WARNINGS, not errors: edges are advisory (§2g), so an isolated block
 *  is a workflow the orchestrator can still run — it is just almost certainly a mistake
 *  (a fan-out you forgot to wire, a reviewer that will never be asked). */
function reachabilityFindings(w: Workflow, byId: Map<string, WorkflowBlock>): Finding[] {
  const out: Finding[] = [];
  if (!w.edges.length || w.blocks.length < 2) return out;

  const ids = [...byId.keys()];
  // Nothing here has an ID, so there is no graph to reason about — every edge is dangling
  // and `edge-unknown-block` has already said so. Without this, `entries` came out empty
  // and we announced that "every block is pointed at by another", which is neither true nor
  // useful about a file whose blocks have no identities yet (rev-5 F6).
  if (!ids.length) return out;
  const inDeg = new Map(ids.map((id) => [id, 0]));
  const outAdj = new Map<string, string[]>(ids.map((id) => [id, []]));
  for (const e of w.edges) {
    if (!byId.has(e.from) || !byId.has(e.to) || e.from === e.to) continue;
    inDeg.set(e.to, (inDeg.get(e.to) ?? 0) + 1);
    outAdj.get(e.from)!.push(e.to);
  }

  for (const id of ids) {
    if (inDeg.get(id) === 0 && outAdj.get(id)!.length === 0) {
      out.push({
        severity: "warning",
        code: "isolated-block",
        message: `Block "${id}" has no edges — nothing declares when it runs.`,
        blockId: id,
      });
    }
  }

  // Entries are the blocks nothing points at. A workflow with none is all cycles — the
  // rework loop (worker ⇄ reviewer) is a legitimate cycle, so a cycle is not itself a
  // finding; having NOWHERE TO START is.
  const entries = ids.filter((id) => inDeg.get(id) === 0);
  if (!entries.length) {
    out.push({
      severity: "warning",
      code: "no-entry-block",
      message: "Every block is pointed at by another — the declared path has no starting point.",
    });
    return out;
  }

  const reached = new Set<string>(entries);
  const queue = [...entries];
  while (queue.length) {
    const id = queue.shift()!;
    for (const next of outAdj.get(id)!) {
      if (!reached.has(next)) {
        reached.add(next);
        queue.push(next);
      }
    }
  }
  for (const id of ids) {
    if (!reached.has(id)) {
      out.push({
        severity: "warning",
        code: "unreachable-block",
        message: `Block "${id}" is unreachable — no path leads to it from a starting block.`,
        blockId: id,
      });
    }
  }
  return out;
}
