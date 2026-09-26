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

### Splitting a large class into satellites

`src/pane.ts` is the worked example (#3498 F1). A class too large for one file
is cut by METHOD CLUSTER into owned helper objects the class delegates to
(`panelifecycle.ts`, `panebadges.ts`, `panecompose.ts`, `paneembeds.ts`,
`paneviews.ts`, plus the free function `panecapture.ts`). The shape, and why:

- **Each satellite owns its cluster's state.** A field used only by one
  cluster moves into that satellite; a field several clusters share stays on
  the class. The satellite reads the rest through a `pane` back-reference, so a
  moved body differs from its original only by `this.` becoming `this.pane.`
  for members it does not own. That receiver rewrite is the whole diff, which
  is what makes the move provable by normalising it away and comparing bodies.
- **The public API does not move.** Every moved PUBLIC member keeps a
  one-line delegator on the class with its exact signature, so no caller
  outside the family changes. A moved PRIVATE member has no delegator; the
  class calls it as `this.<satellite>.<member>`.
- **Visibility widens only as far as the compiler demands.** TypeScript has no
  module-internal visibility, so a member a satellite reads must lose
  `private`. Where a public getter already answers the read, the satellite uses
  the getter instead of widening the field: `PaneBadges.isWatched` stays
  `private` with one writer (#3319), and `capturePane` reads `pane.watched`.
- **Constructor wiring moves in place.** A contiguous run of constructor
  statements that builds one cluster's DOM becomes that satellite's
  constructor (or a `wire…` method for a second run), called at the exact
  point the statements used to run, so the header's DOM order is unchanged.
- **A satellite never imports a value from the class's module.** The class
  imports every satellite, so the reverse import would be a cycle that
  evaluates the satellite's top level first. Shared values live in a satellite
  and the class imports them; type-only imports back are fine.
- **Source-scanning tests move with the code they pin.** A test that reads the
  class file as text reads the satellite now holding the member, and a
  "nothing else touches X" scan adds the satellites to its population. Each
  re-point is a coverage change, so it is shown reddening on a planted
  violation at the new path.
- **What stays for a later cut.** Fit/resize stays on `Pane`, so nothing a
  satellite does can reach a PTY resize that it could not reach before
  (constraint 1). The header-overflow ladder, the content/welcome paths, and
  fit/resize are the remaining clusters.

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
