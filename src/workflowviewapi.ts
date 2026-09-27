// The workflow pane's INTERFACE as its satellites see it (#3498 F3). `WorkflowView` is split
// into per-panel controllers (workflowcanvas, workflowcliknobs, workflowfilemenu,
// workflowinspector, workflowsections), and each types its `view` back-reference against
// `WorkflowViewApi` here, never against workflowview.ts. The view imports every satellite, so
// a satellite importing the view back, even type-only, would close a cycle, and
// test/workflowmodel.test.ts refuses both the cycle and the import.
//
// The interfaces list exactly the members a SATELLITE reaches across a file. Each is public
// on its class for that reason, and the view and each satellite `implements` its interface,
// so the compiler keeps the two in step. A satellite member only the view calls is public on
// its class and not listed. Also home of `WorkflowHost`, which workflowview.ts re-exports.
// Types only: nothing here exists at runtime. Layout conventions: docs/design/module-layout.md.

import type {
  FieldBounds,
  Workflow,
  WorkflowBlock,
  WorkflowAnalysis,
  Finding,
  FindingSection,
} from "./workflowmodel";
import type { KnobStates } from "./selectorknobs";
import type { CliProbe } from "./modelcatalog";
import type { KnobFieldSpec } from "./workflowknobs";
import type { Point, WorkflowLayout } from "./workflowlayout";
import type { LayoutWrite, Selection, Surface } from "./workflowpane";

/** What the hosting pane provides. Only one host today (the workflow PANE — a workflow
 *  builder is a station you keep open beside an agent, never a glance-and-dismiss
 *  overlay), but the shape mirrors `FileEditHost` so the pane wires it the same way. */
export interface WorkflowHost {
  /** The repo/folder the workflow file lives under (the pane's root). */
  getRoot(): string | null;
  /** Root-relative path of the workflow file. Defaults to `.orrerix/workflow.yml`, falling back to `.loomux/workflow.yml` when only that exists. */
  getFile?(): string;
  /** The pane moved to another of the repo's workflow files (#2944). The pane records the
   *  file it is on (`contentFile`, and the persisted record's `file`) and names itself after
   *  it, so both have to follow the picker — otherwise a restore reopens the workflow the
   *  human navigated AWAY from, under a title naming a third one. Optional because the shape
   *  is a host contract and not every future host has a title to keep. */
  onFileChanged?(rel: string): void;
  /** Never called in embedded mode — the pane's own ✕ closes it (and asks first). */
  onClose(): void;
  /** This view IS a pane's content: no ✕, no Esc-to-close. Same fork as FileEditView. */
  embedded?: boolean;
}

/** What a satellite reaches on `WorkflowView`: exactly the members that cross a file. */
export interface WorkflowViewApi {
  readonly host: WorkflowHost;
  root: string | null;
  rel: string;
  text: string;
  savedText: string;
  savedHash: string;
  exists: boolean;
  loadError: string | null;
  layout: WorkflowLayout;
  savedLayout: WorkflowLayout;
  analysis: WorkflowAnalysis;
  selection: Selection;
  disposed: boolean;
  inspTitleEl: HTMLElement;
  inspSubEl: HTMLElement;
  formPane: HTMLElement;
  graphPane: HTMLElement;
  readonly dirty: boolean;
  load(): Promise<void>;
  retarget(rel: string): void;
  save(): Promise<void>;
  saveLayout(when?: LayoutWrite): Promise<void>;
  scaffold(): Promise<void>;
  setText(text: string): void;
  syntaxBroken(): boolean;
  render(): void;
  setSurface(surface: Surface): void;
  renderRoster(): void;
  sectionFindings(section: FindingSection): Finding[];
  blockFindings(b: WorkflowBlock): Finding[];
  selectItem(sel: Selection): void;
  mutate(edit: (w: Workflow) => void, rerenderForm?: boolean): void;
  createBlock(at?: Point): Promise<void>;
  deleteBlock(b: WorkflowBlock, index: number): Promise<void>;
  renderFindings(): void;
  readonly canvas: WorkflowCanvasApi;
  readonly knobs: WorkflowCliKnobsApi;
  readonly inspector: WorkflowInspectorApi;
  readonly sections: WorkflowSectionsApi;
}

/** What the rest of the pane reaches on `WorkflowCanvas` (workflowcanvas.ts). */
export interface WorkflowCanvasApi {
  renderGraph(): void;
  eraseEdge(from: string, to: string): void;
  eraseGateEdge(reviewer: string): void;
}

/** What the rest of the pane reaches on `WorkflowCliKnobs` (workflowcliknobs.ts). */
export interface WorkflowCliKnobsApi {
  knobLookup: (cli: string, model: string) => KnobStates | null;
  applyDetection(program: string): void;
  probeModels(cli: string): Promise<CliProbe>;
  knobRow(
    label: string,
    spec: KnobFieldSpec,
    onChange: (v: string) => void,
  ): { field: HTMLElement; paint: (next: KnobFieldSpec) => void };
}

/** What the rest of the pane reaches on `WorkflowInspector` (workflowinspector.ts). */
export interface WorkflowInspectorApi {
  repaintBlockKnobs: (() => void) | null;
  refreshBlockModels: (() => void) | null;
  renderInspector(): void;
  field(label: string, control: HTMLElement, hint?: string): HTMLElement;
  textInput(value: string, onChange: (v: string) => void, placeholder?: string): HTMLInputElement;
  select(
    options: readonly string[],
    value: string,
    onChange: (v: string) => void,
  ): HTMLSelectElement;
  labelledSelect(
    options: readonly { value: string; label: string }[],
    value: string,
    onChange: (v: string) => void,
  ): HTMLSelectElement;
  boundedNumber(
    value: number | undefined,
    bounds: FieldBounds,
    onChange: (v: number | undefined) => void,
    placeholder?: string,
  ): HTMLInputElement;
  sectionToggle(label: string, on: boolean, onChange: (on: boolean) => void): HTMLElement;
  sectionFindingList(section: FindingSection): HTMLElement | null;
}

/** What the rest of the pane reaches on `WorkflowSections` (workflowsections.ts). */
export interface WorkflowSectionsApi {
  gateForm(w: Workflow): HTMLElement;
  intakeForm(w: Workflow): HTMLElement;
  driverForm(w: Workflow): HTMLElement;
  mergeQueueForm(w: Workflow): HTMLElement;
  resourcesForm(w: Workflow): HTMLElement;
}
