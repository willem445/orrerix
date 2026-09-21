// The To-Do store, frontend half (#3263 S3).
//
// The ONLY module in the pane that reaches the backend, and it does so through
// `./transport.ts` — never `@tauri-apps/*` directly (CLAUDE.md constraint 5;
// `test/transport.test.ts` fails the suite if that stops being true). Its
// siblings are `git.ts`, `fileapi.ts` and `orchestration.ts`: a typed wrapper
// per backend capability, and nothing else.
//
// The division of labour with `todomodel.ts` is deliberate and worth stating,
// because it is what keeps the model testable: EVERY decision — which items a
// view holds, what a move's `order_after` is, what an undo's inverse looks
// like — lives there, DOM-free and clock-injected. This module does exactly
// three things: name the commands, decode the responses through the model's
// `decodeSnapshot`, and subscribe to the event.
//
// THE SCOPE IS A ROOT, NOT A KEY. Every call takes the active pane's project
// root as the caller spells it, and the BACKEND turns it into a workspace key
// through `todo::workspace_key`. That is the same door the MCP path uses, so
// two spellings of one project cannot become two lists — and the frontend
// cannot NAME a scope by inventing its key, because it never sends one.
//
// Scope, and only scope. An `update` / `complete` / `delete` is addressed by
// ID, and the engine's `live_index` does no scope check, so the root sent
// beside one of those does not confine it to that workspace. Harmless here —
// this caller is the trusted webview — and named rather than implied because
// the same sentence one layer down is what an AGENT's path (#3263 S2) must
// NOT rely on. See `docs/design/todo-pane.md`.

import { invoke, listen, type UnlistenFn } from "./transport.ts";
import { decodeSnapshot, type Applied, type TodoOp, type TodoSnapshot } from "./todomodel.ts";

export type {
  Actor,
  AddFields,
  Applied,
  OrderAfter,
  PlannedBucket,
  Scope,
  SmartView,
  Step,
  TodoItem,
  TodoOp,
  TodoSnapshot,
  UpdateFields,
  Workspace,
} from "./todomodel.ts";

/** What the backend emits after any successful write, from either writer (the
 *  pane or an agent through MCP). Deliberately a NOTIFICATION, not a delta:
 *  the pane re-reads the snapshot rather than trying to patch its list from
 *  this, so a missed event costs one stale render and never a divergent one. */
export interface TodoChanged {
  scope: import("./todomodel.ts").Scope;
  ids: string[];
  actor: import("./todomodel.ts").Actor;
}

/** `workspace_root` as the backend command takes it: absent for the global
 *  list, the raw root otherwise. */
function args(workspaceRoot: string | null | undefined, rest: Record<string, unknown> = {}) {
  return workspaceRoot ? { workspaceRoot, ...rest } : rest;
}

/**
 * Read the store for one scope.
 *
 * Pass the active pane's project root for a workspace list, or nothing for the
 * global one. Never throws on a malformed payload: `decodeSnapshot` drops what
 * it cannot understand and keeps the rest.
 */
export async function todoSnapshot(workspaceRoot?: string | null): Promise<TodoSnapshot> {
  return decodeSnapshot(await invoke<unknown>("todo_snapshot", args(workspaceRoot)));
}

/**
 * Apply one op.
 *
 * Rejects with the backend's own message for every refusal the human needs to
 * see — a cap, an `if_rev` conflict, an unknown id, a store written by a newer
 * build — so a caller can put it straight in a toast. The op's shape is the
 * contract `parse_op` enforces (`src-tauri/src/orchestration/todo.rs`): one
 * key, naming one action.
 */
export function todoApply(op: TodoOp, workspaceRoot?: string | null): Promise<Applied> {
  return invoke<Applied>("todo_apply", args(workspaceRoot, { op }));
}

/**
 * Subscribe to `todo-changed`.
 *
 * Fires for EVERY successful write, including an agent's through MCP — which
 * is the point: the pane is a live view of a store two processes write to.
 * Callers put a `CoalescingRefresh` behind this rather than re-reading per
 * event, exactly as the board does with `orch-tasks-changed`.
 */
export function onTodoChanged(handler: (e: TodoChanged) => void): Promise<UnlistenFn> {
  return listen<TodoChanged>("todo-changed", (e) => handler(e.payload));
}
