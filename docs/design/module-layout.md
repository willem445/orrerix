# Module layout and naming

This note defines how code is divided into navigable modules during refactors.
The goal is one discoverable responsibility per file, without changing runtime
behavior or crossing established crate boundaries.

## Naming

A file is named for the one noun it owns. Product code does not use `util`,
`misc`, `helpers`, or `common`; shared helpers are named for what they do,
such as `text.rs` or `fsatomic.rs`. Every new file starts with a `//!` header
stating its job and pointing to its design note.

## Rust modules

Rust modules use three tiers:

- `commands/` contains Tauri boundary code: parse at the edge and delegate,
  with no business logic.
- `registry/` contains `impl OrchRegistry` blocks grouped by concern.
- Bare `<noun>.rs` files contain logic that does not use registry `self`.

Persistence modules are named for the whole-file store they own (`queuestate`,
`uistate`); atomic writes go through `fsatomic`. Engine versus `src-tauri` is
decided by outbound dependency edges, as described in
[`engine-extraction.md`](engine-extraction.md), not by tidiness.

## Tests

Integration tests are grouped by Cargo target under `tests/<target>/`, with one
`main.rs` and topic modules. Source-scanning guards for a target live together
in its `guards.rs` rather than scattered among behavioral tests.

## Frontend modules

`*model.ts` is DOM-free and unit-tested; `*view.ts` and `*pane.ts` hold DOM
glue that is hand-validated. `transport.ts` plus `pty.ts`, `git.ts`,
`fileapi.ts`, and `orchestration.ts` are the frontend bridges. Satellites of a
large module carry its prefix (`pane*`, `workflow*`, `todo*`, `token*`).

A large module is split behind a **barrel** so that no importer changes: the old
file keeps its name and becomes explicit `export { … } from` and
`export type { … } from` lists naming exactly the names it exported before the
split. It never uses `export *`, which would also publish every helper a sibling
needed widened to `export`. The barrel's types and constants move into a
**types module** that imports no sibling. Every split module imports from that
module or from a sibling, never from the barrel, so the import graph stays
acyclic. ES modules allow cycles, but in one a module-level constant can be
read before its module has run. `workflowmodel.ts` over `workflowtypes`,
`workflowparse`, `workflowserialize`, `workflowvalidate` and `workflowgraph` is
the first instance (#3498 F2). `test/workflowmodel.test.ts` pins its graph
acyclic and its barrel re-export-only.

Folders are recommended only after files have been split, as a held slice by
family: `src/pane/`, `src/workflow/`, `src/todo/`, `src/tokens/`, `src/files/`,
`src/git/`, `src/session/`, `src/board/`, and `src/bridge/`, with `test/`
mirrored. The test glob and TypeScript include are recursive, but every relative
import moves, `architecture.md` paths need updating, and source scans must
recurse to retain coverage.

## Physical-size budget

`test/filebudget.test.ts` enforces physical-line ceilings over tracked files:
Rust source under `src-tauri/src/` and `crates/*/src/` defaults to 3,000 lines;
Rust tests under `src-tauri/tests/` and `crates/*/tests/` default to 5,000;
TypeScript under `src/` defaults to 1,500; and tests under `test/` plus TypeScript
under `e2e/` default to 2,000. These are class defaults, not hard limits on every
file. Grandfathered files have individual ceilings based on their recorded blob
plus five percent headroom; they remain grandfathered only while larger than
85 percent of that ceiling, so splits tighten the table. An oversized legacy
module such as `src-tauri/src/orchestration/mod.rs` remains governed by its own
cited row rather than the class default.

For moved code, use `git blame --ignore-revs-file .git-blame-ignore-revs -C -C -C`
to follow lines across file boundaries while skipping the pure-move commits
recorded in the ignore-revs file.
