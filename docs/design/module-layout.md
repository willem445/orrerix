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
`tests/orchestration.rs`; read that as this target. Its tests kept their names,
so `grep -rn <test name> src-tauri/tests/orchestration/` finds the new home.

## Frontend modules

`*model.ts` is DOM-free and unit-tested; `*view.ts` and `*pane.ts` hold DOM
glue that is hand-validated. `transport.ts` plus `pty.ts`, `git.ts`,
`fileapi.ts`, and `orchestration.ts` are the frontend bridges. Satellites of a
large module carry its prefix (`pane*`, `workflow*`, `todo*`, `token*`).

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
