// The workflow pane's FILE PICKER, split out of workflowview.ts (#3498 F3): the menu the
// header's file button opens (#2944) listing the repo's workflows, moving the pane to another
// one after settling unsaved edits, and *New workflow…*.
//
// A satellite of `WorkflowView`, not a model: it owns the listing and reads the rest of the
// pane through `WorkflowViewApi` (workflowviewapi.ts), never through workflowview.ts. What
// the menu offers is decided in workflowfilepicker.ts (DOM-free, unit-tested); this is the DOM
// and the I/O sequencing. The menu floats over the pane, so nothing here reaches a PTY resize
// (CLAUDE.md constraint 1). Design note: docs/design/workflows.md; layout conventions:
// docs/design/module-layout.md.

import { emptyLayout } from "./workflowlayout";
import {
  resolveWorkflowFilePicker,
  canCreateWorkflow,
  switchPlan,
  type WorkflowFilePicker,
} from "./workflowfilepicker";
import { workflowList } from "./orchestration";
import type { WorkflowListing } from "./roster";
import { showContextMenu, type MenuItem } from "./contextmenu";
import { discardEdits } from "./dirtystate";
import { showToast } from "./toast";
import { modal, promptModal } from "./modal";
import type { WorkflowViewApi } from "./workflowviewapi";

/** The file menu's one non-path action. A sentinel rather than a `MenuItem<string | symbol>`
 *  union because every other item's action IS a repo-relative path, and no path can be this:
 *  `:` is one of `fm_new_file`'s illegal name characters and no workflow path carries one, so
 *  the two can never collide. */
const NEW_WORKFLOW = "orrerix:new-workflow";

export class WorkflowFileMenu {
  /** Every workflow this repo declares, as `orch_workflow_list` reported it — the SAME listing
   *  the launcher's picker and the group header read (#2603), never a second discovery.
   *  `undefined`-shaped as `null`: "we could not list" and "there are none" are different
   *  states and only one of them is a repo with no workflows (`resolveWorkflowFilePicker`). */
  private listing: WorkflowListing | null = null;
  /** The root the listing above is ABOUT, recorded with it so the two cannot come to disagree
   *  — the same statement pair `launcher.ts` keeps for its own picker, for the same reason: a
   *  pane that is re-rooted must not offer the previous repo's files. */
  private listingRoot: string | null = null;

  constructor(private readonly view: WorkflowViewApi) {}

  // ---------- the file picker (#2944) ----------

  /** Re-read the repo's workflow listing. Cheap, memo-less, and deliberately so: it is one
   *  IPC on open and after a create, and a memo here would be a second place for the listing
   *  and the disk to disagree — the state this pane exists to REPAIR is a file that has just
   *  changed under someone.
   *
   *  A read that fails leaves `listing` null, which the resolver reads as "we do not know":
   *  the picker then offers nothing rather than claiming the repo has no workflows, and
   *  `canCreateWorkflow` refuses, because a create that cannot rule out a name collision is
   *  the create it exists to stop. */
  async refreshListing(): Promise<void> {
    const root = this.view.root;
    if (!root) {
      this.listing = null;
      this.listingRoot = null;
      return;
    }
    let next: WorkflowListing | null = null;
    try {
      next = await workflowList(root);
    } catch {
      next = null;
    }
    if (this.view.disposed || this.view.root !== root) return; // re-rooted while we were asking
    this.listing = next;
    this.listingRoot = root;
    this.view.render();
  }

  /** The picker as it stands right now. Resolved from the listing and the pane's OWN `rel` —
   *  never from the button's text — so what the menu marks and what a save writes are one
   *  fact asked once. A listing about a different root is not this repo's, so it is not
   *  offered: the pair is checked here rather than trusted, the way `launcher.ts` checks its
   *  own held picker's repo before deciding anything with it. */
  private filePicker(): WorkflowFilePicker {
    const listing = this.listingRoot === this.view.root ? this.listing : null;
    return resolveWorkflowFilePicker(listing, this.view.rel);
  }

  /** The menu behind the file button: every workflow the repo declares, the open one ticked,
   *  a broken one still listed with its finding as its tooltip, then *New workflow…*. */
  showFileMenu(x: number, y: number): void {
    const picker = this.filePicker();
    const items: MenuItem<string>[] = picker.options.map((o) => ({
      // The tick is in the LABEL rather than a class, because `MenuItem` has no "checked" and
      // inventing one for a single caller would be a menu feature with one user. The spaces
      // keep the names aligned when nothing is ticked in a row.
      label: `${o.current ? "✓ " : "   "}${o.label}`,
      action: o.path,
      // An unparseable workflow is SELECTABLE — it is the file this pane exists to fix — so
      // its finding rides as a tooltip rather than as a `disabled` reason.
      reason: o.finding ?? undefined,
    }));
    // The pane is on a `.yml` that is not one of the repo's workflows (the file browser's
    // *Open in workflow pane* takes any of them). Say so, rather than leaving a menu in which
    // nothing is ticked and letting the human conclude the tick is broken.
    //
    // Gated on there being options at all, and that is not a tidiness rule. A repo with NO
    // workflow yet is `offListing` too — the listing is empty and honest, and the pane is
    // sitting on the default path *offering to create it*, which is the ordinary beginning of
    // every repo. Telling that human their file "is not one of this repo's workflows" would
    // be true, useless, and read as an error over the start surface's invitation.
    if (picker.offListing && picker.options.length) {
      items.unshift(
        { label: `${this.view.rel} — not one of this repo's workflows`, disabled: true },
        { label: "", separator: true }
      );
    }
    for (const f of picker.findings) items.push({ label: f, disabled: true });
    if (items.length) items.push({ label: "", separator: true });
    items.push({ label: "New workflow…", action: NEW_WORKFLOW });
    showContextMenu(x, y, items, (action) => {
      if (action === NEW_WORKFLOW) void this.newWorkflow();
      else void this.openFile(action);
    });
  }

  /** Move the pane to another workflow file.
   *
   *  THE RULE, and it is `switchPlan`'s whole reason for existing: an unsaved buffer belongs
   *  to the file it was typed against. `save()` writes `this.rel`, so retargeting first and
   *  asking afterwards would arm the next Ctrl+S to write one workflow's text over another
   *  workflow's file. Every branch below therefore settles the buffer BEFORE `retarget`.
   *
   *  A "Save and switch" whose save did not land (a conflict, a claimed path, a write error)
   *  leaves the buffer dirty, and the switch is abandoned rather than completed — the human
   *  asked to keep those edits, and carrying on would drop the very thing they said to keep. */
  private async openFile(rel: string): Promise<void> {
    const plan = switchPlan({ current: this.view.rel, dirty: this.view.dirty }, rel);
    if (plan.kind === "same-file") return;
    if (plan.kind === "ask" && !(await this.settleBuffer(plan.file))) return;
    this.view.retarget(plan.file);
    this.view.host.onFileChanged?.(plan.file);
    // A different file is a different workflow: a selection into the old roster would address
    // a row in the new one (`Selection` is by index, deliberately — see workflowpane.ts), and
    // a YAML toggle left over from the file you were fixing is not where you want to land in
    // the one you just opened.
    this.view.selection = { kind: "workflow" };
    this.view.setSurface("canvas");
    await this.view.load();
  }

  /** Settle the unsaved buffer that belongs to `this.rel` before the pane moves to `target`.
   *  True = settled, the caller may retarget; false = the human cancelled, or a "save" did not
   *  land and abandoning is the only way to keep what they asked to keep.
   *
   *  One method rather than the same eight lines in `openFile` and `newWorkflow` (rev-std
   *  round 1, N2). That duplication is the shape this rule cannot afford: it is the ONLY thing
   *  standing between a switch and a save into the wrong file, so a later edit that fixes one
   *  copy and not the other would leave the second path silently unguarded. */
  private async settleBuffer(target: string): Promise<boolean> {
    const choice = await this.confirmSwitch(target);
    if (choice === "cancel") return false;
    if (choice === "save") {
      await this.view.save();
      return !this.view.dirty; // still dirty = the save did not land; the toast said why
    }
    this.view.setText(discardEdits(this.view.savedText));
    return true;
  }

  /** The three answers a switch may get, which are the close guard's two plus the one a
   *  switch can offer that a close cannot: the file you are leaving is still there to be
   *  saved into. Never a silent drop, and never a carry-across. */
  private confirmSwitch(target: string): Promise<"save" | "discard" | "cancel"> {
    return modal<"save" | "discard" | "cancel">((resolve) => ({
      title: "Unsaved workflow changes",
      body: `${this.view.rel} has unsaved edits. They belong to that file — orrerix will not carry them into ${target}.`,
      buttons: [
        { label: "Cancel", value: "cancel" },
        { label: `Discard and open ${target}`, value: "discard", kind: "danger" },
        { label: `Save ${this.view.rel}, then open`, value: "save" },
      ],
      onKey: (k) => (k === "Escape" ? resolve("cancel") : undefined),
    }));
  }

  /** *New workflow…* — name it, create `workflows/<name>.yml` from the built-in default
   *  roster, and open it.
   *
   *  The name is validated in the DIALOG, on every keystroke (`promptModal`'s `validate`), by
   *  `canCreateWorkflow` — so a refusal is a message beside the box the human is still typing
   *  in, rather than a file that failed to appear. That is also where the #2892 case-collision
   *  refusal lands.
   *
   *  Creating goes through the pane's ordinary create path and not a new one: `ensureConfigDir`
   *  makes `workflows/`, `claimFile` claims the name atomically (`create_new(true)` — it
   *  refuses, without truncating, if anything is already there), and the write is guarded by
   *  the claimed file's own hash. So even a name the listing said was free, taken between the
   *  dialog and the write, is a refusal rather than an overwrite. */
  private async newWorkflow(): Promise<void> {
    if (!this.view.root) return;
    // Ask the disk again first: the listing may be minutes old, and its age is exactly what
    // the collision check is about.
    await this.refreshListing();
    if (this.view.disposed) return;
    const listing = this.listingRoot === this.view.root ? this.listing : null;
    const name = await promptModal({
      title: "New workflow",
      body: "A new workflow file under this repo's config directory, scaffolded from orrerix's built-in roster. Letters, digits, `_` and `-`.",
      label: "Name",
      placeholder: "review-heavy",
      affirm: "Create",
      validate: (v) => {
        const verdict = canCreateWorkflow(v, listing);
        return verdict.ok ? null : verdict.reason;
      },
    });
    if (name === null || this.view.disposed) return;
    // Asked AGAIN on the value that came back, and not merely trusted from the dialog: the
    // dialog's `validate` is what the human read, this is what decides. (`promptModal` trims
    // what it returns, so the two are asked about the same string only if we re-derive it.)
    const verdict = canCreateWorkflow(name, listing);
    if (!verdict.ok) {
      showToast(verdict.reason);
      return;
    }
    // Settle the buffer we are leaving before anything is created — same rule as `openFile`,
    // through the same method, and the reason it runs first is that a create is a save into
    // `this.rel`.
    if (this.view.dirty && !(await this.settleBuffer(verdict.path))) return;
    this.view.retarget(verdict.path);
    this.view.host.onFileChanged?.(verdict.path);
    this.view.selection = { kind: "workflow" };
    // Reset to the "no file here" state so `createAllowed` is true for the new path and
    // `savePlan` chooses `claim-then-write`. Nothing is read from disk first on purpose: the
    // claim IS the read, and it is atomic, so a file that appeared in the meantime is refused
    // by `claimFile` instead of being raced against a stale `exists`.
    this.view.exists = false;
    this.view.savedHash = "";
    this.view.savedText = "";
    this.view.setText("");
    this.view.loadError = null;
    this.view.layout = emptyLayout();
    this.view.savedLayout = this.view.layout;
    await this.view.scaffold();
    await this.refreshListing();
  }
}
