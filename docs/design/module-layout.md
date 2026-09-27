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
- `registry/` contains `OrchRegistry` itself (`registry/mod.rs`: the struct,
  `new()` and the constraint-10 barrier) and `impl OrchRegistry` blocks
  grouped by concern. A concern's file also takes the free functions that are
  registry-bound — ones taking the registry as a parameter, like
  `deliver_now` — and leaves the pure logic its methods call (`Tier1Scan`,
  the `stranded_*` decision functions) for a bare `<noun>.rs`. The struct's
  fields are `pub(super)` wherever code outside `registry/` reads them: the
  struct moved down a level, and `pub(super)` from `registry/` is exactly the
  visibility a private field had in `orchestration/mod.rs`.
- Bare `<noun>.rs` files contain logic that does not use registry `self`.

Persistence modules are named for the whole-file store they own (`queuestate`,
`uistate`); atomic writes go through `fsatomic`. Engine versus `src-tauri` is
decided by outbound dependency edges, as described in
[`engine-extraction.md`](engine-extraction.md), not by tidiness.

## Tests

Integration tests are grouped by Cargo target under `tests/<target>/`, with one
`main.rs` and topic modules. Source-scanning guards for a target live together
in its `guards.rs` rather than scattered among behavioral tests.

`src-tauri/tests/orchestration/` is the first target in this shape (#3498 P1).
Cargo builds `tests/<name>/main.rs` as the target `<name>`, so it is still one
test binary: `cargo test -p orrerix --test orchestration <filter>` is unchanged,
no second Windows link happens, and `CAPTURE_SERIAL` still serialises the whole
binary. `tests/common/` is not the pattern for this: `mod common;` compiles the
helper into every binary that includes it, and each target keeps its own copy
of `rails()`/`test_registry()` inside its own directory instead.

How the directory is wired, because the single file it replaced had one module
namespace and the split must not change what any name resolves to:

- `main.rs` holds the old file's header and every `use` line, then the module
  tree. `helpers` is declared first with `#[macro_use]`, because
  `macro_rules!` is textually scoped and the three shared macros live there.
- Every topic module opens with `use super::*`, so it sees the imports and the
  items `main.rs` re-exports with `use <module>::*`.
- An item another module uses is `pub(crate)`; everything else stays private.
  `pub(crate)` is the only visibility these files use.
- `include_str!` resolves against the source file's own directory, so a fixture
  path gains one `../` when its test moves into the directory.
- A guard keyed by file path (the #464 `OrchRegistry::new` allowlist in
  `guards.rs`) names the module file, relative to `tests/`, where the item now
  lives: `orchestration/helpers.rs`.

Prose written before the split, in code comments and design notes, still names
`tests/orchestration/`; read that as this target. Its tests kept their names,
so `grep -rn <test name> src-tauri/tests/orchestration/` finds the new home.

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

### Splitting a large class into satellites

A large **class** is split into satellites it delegates to, one per panel or
method cluster. There are two instances: `pane.ts` with `panelifecycle.ts`,
`panebadges.ts`, `panecompose.ts`, `paneembeds.ts`, `paneviews.ts` and the
free function `panecapture.ts` (#3498 F1), and `workflowview.ts` with
`workflowviewapi.ts` and five satellites (#3498 F3). The shape, and why:

- **Each satellite owns its cluster's state.** A field used only by one
  cluster moves into that satellite; a field several clusters share stays on
  the class. The satellite reads the rest through a back-reference (`pane` in
  the pane family, `view` in the workflow family), so a moved body differs
  from the original only in its receiver. That receiver rewrite is the whole
  diff, which is what makes the move provable by normalising it away and
  comparing bodies.
- **The public API does not move.** Every moved PUBLIC member keeps a
  one-line delegator on the class with its exact signature, so no caller
  outside the family changes. A moved PRIVATE member has no delegator; the
  class calls it as `this.<satellite>.<member>`.
- **A member leaves `private` only when another file reaches it.** TypeScript
  has no module-internal visibility, so that is as far as the compiler lets
  the widening go. Where a public getter already answers the read, the
  satellite uses the getter instead of widening the field:
  `PaneBadges.isWatched` stays `private` with one writer (#3319), and
  `capturePane` reads `pane.watched`.
- **Constructor wiring moves in place.** A contiguous run of constructor
  statements that builds one cluster's DOM becomes that satellite's
  constructor (or a `wire…` method for a second run), called at the exact
  point the statements used to run, so the header's DOM order is unchanged.
- **Values the class and its satellites share live in a satellite, never in
  the class's module.** The class imports every satellite, so a value import
  back would be a cycle that evaluates the satellite's top level first. How
  the back-reference is TYPED depends on the family's import pin:
  - Where the family's graph is NOT pinned acyclic with type imports included
    (the `pane*` family), a type-only import back is erased at compile time,
    so `pane` is typed against `Pane` directly.
  - Inside a family that IS pinned that way (the `workflow*` family), even a
    type-only import back closes a cycle. There the back-reference is typed
    against an **interface module**, which lists exactly the members that
    cross a file, and the class and each satellite `implements` their
    interface so the compiler keeps the lists in step.
    `test/workflowmodel.test.ts` refuses a satellite importing the view.
- **Source-scanning tests move with the code they pin.** A test that reads the
  class file as text reads the satellite now holding the member, and a
  "nothing else touches X" scan adds the satellites to its population. Each
  re-point is a coverage change, so it is shown reddening on a planted
  violation at the new path.
- **What stays for a later cut.** Fit/resize stays on `Pane`, so nothing a
  satellite does can reach a PTY resize that it could not reach before
  (constraint 1). Satellites DO still reach one, through base call sites they
  carried: `PaneLifecycle.attachPty`'s post-spawn `applyFit()` reconcile,
  `start`'s `resizeObs.observe` (its callback is `applyFit`),
  `start`/`respawnFresh`'s `fit.fit()`, and `PaneEmbeds.wireEmbedDivider`'s
  resize hold, whose release runs `runFit()`. So constraint 1 applies to every
  satellite, not only to `pane.ts`. The header-overflow ladder, the
  content/welcome paths, and fit/resize are the remaining clusters.

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
85 percent of that ceiling, so splits tighten the table. A row whose file is
back at or under its class ceiling is refused as redundant: the class default
governs it again and the row is removed, not tightened. An oversized legacy
module such as `src-tauri/src/orchestration/mod.rs` remains governed by its own
cited row rather than the class default.

For moved code, use `git blame --ignore-revs-file .git-blame-ignore-revs -C -C -C`
to follow lines across file boundaries while skipping the pure-move commits
recorded in the ignore-revs file.
