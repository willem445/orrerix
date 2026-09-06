# The plan driver (#3040)

**Status: a stub.** Slice P1 has landed the `orrerix-plan` block — contract 1
below, and that section is written in the present tense because it describes
code in the tree (`crates/loomux-engine/src/plandoc.rs`). Everything else here
is WILL-tense: it names a contract #3040's later slices are to land, and no line
of it is shipped yet. Do not act on a WILL-tense section as though it described
the build you are looking at.

The full design is the plan comment on #3040. This note exists so each contract
gets a durable home as its slice lands, rather than living only in an issue
comment.

## 1. The `orrerix-plan` block (P1 — shipped)

A planner's issue comment carries **exactly one** fenced block whose info string
is `orrerix-plan`. Its content is YAML, schema v1:

```yaml
version: 1
issue: 3040
slices:
  - id: P1
    title: plan block parser
    branch: feat/3040-p1-plan-block
    block: worker-adv
    deps: []
    brief: |
      Delivered verbatim into the worker's kickoff. Multi-paragraph prose is
      why this is YAML rather than JSON — a block scalar needs no escaping.
    avoid_files: [src-tauri/src/orchestration/mod.rs]
    red_before_green: "cargo test --locked -p loomux-engine plandoc"
    hold: false
risks:
  - "Free-form notes, carried through untouched."
```

| field | required | meaning |
| --- | --- | --- |
| `version` | yes | Must be `1`. A different version is refused, not read optimistically. |
| `issue` | yes | The issue this plan is for. |
| `slices` | yes | At least one. Order is the planner's; it carries no scheduling meaning — `deps` does. |
| `slices[].id` | yes | Unique within the plan. Validated through `pathseg::check_segment` (CLAUDE.md constraint 6). |
| `slices[].title` | yes | Non-empty. Becomes the board row title beside the id. |
| `slices[].branch` | yes | A git ref name, checked and **refused, never sanitized**. |
| `slices[].block` | yes | A roster block id. **Not** resolved at parse time. |
| `slices[].deps` | no (`[]`) | Slice ids in this same plan. Unknown ids and cycles are refused. |
| `slices[].brief` | yes | At least 40 characters. Delivered verbatim. |
| `slices[].avoid_files` | no (`[]`) | Informational; rendered into the brief header, enforced by nothing. |
| `slices[].red_before_green` | no (`null`) | How the slice's change is to be shown failing first. |
| `slices[].hold` | no (`false`) | `true` = never auto-spawn; the orchestrator briefs this slice by hand. |
| `risks` | no (`[]`) | Free-form notes, carried through untouched. |

Unknown keys at either level are **refused**, not ignored: a misspelled `depends`
that parsed silently would ship a slice with no dependencies at all.

**Three properties this contract rests on.**

*Exactly one block.* Two `orrerix-plan` fences in one comment are refused naming
both fence lines. Which one the author meant is precisely the thing that cannot
be guessed, and a reviewer quoting a plan back is a realistic way for a second
one to appear.

*Nothing is repaired.* An `id` of `../x` is refused, not rewritten to `x`; a
branch containing `..` is refused, not cleaned. Repair is how two distinct
strings come to name one thing, and a slice id reaches a branch name, a pane
name and a persisted row key.

*`block` is a string here.* Whether `worker-adv` exists is a question about the
roster the group was launched with, which the parser cannot see. The drive asks
it (P3a below).

Every refusal reads `plan block line N: …`, with N absolute **within the
comment**, so the planner can scroll to it. `serde_norway` 0.9.42 exposes no
per-value spans on parsed values, but it does attach a mark to any error raised
from inside a `Deserialize` impl. `plandoc` uses that twice: per-value checks
(`id`, `branch`, unknown keys, type mismatches) run during deserialization and
get their line for free, and cross-document checks (duplicate id, unknown dep,
cycle) are located by re-deserializing with a shadow type that deliberately
fails at the addressed value. Where that probe cannot reach the value, the
reason degrades to `plan block: slices[i].deps[j]: …` — the index addressing
`workflow::parse_workflow` uses — rather than inventing a line. The module doc
in `plandoc.rs` is the reference for this.

## 2. `<group-dir>/plan_drives.json` v1 — WILL (P3a)

The drive record will be a v1 JSON file under the group directory, written
atomically under its own lock, preserving unknown fields, and refusing rather
than repairing an unparseable file. See #3040.

## 3. `driver:` block keys — WILL (P3a)

`plan_enabled`, `plan_review_minutes` and `planner_timeout_minutes` will join
the workflow `driver:` block, refused outside their ranges like the existing
review-driver keys. See #3040.

## 4. The four MCP tools — WILL (P3a)

`drive_plan`, `plan_drive_status`, `cancel_plan_drive` and `resume_plan_drive`
will be orchestrator-only tools behind the driver gate. See #3040.

## 5. `post_issue_comment` for a driven planner — WILL (P3a)

When the caller is the planner of a live plan drive, the comment body will be
validated with `plandoc` **before** posting, and an invalid or missing block
will be a tool error with nothing posted. A planner not spawned by a drive will
be unaffected. See #3040.

## 6. `templates/dod.md` and `{{DOD}}` — WILL (P2)

The Definition of Done will exist in exactly one file, quoted into both the
orchestrator template and every driver-composed brief. See #3040.

## 7. The `pd-*` audit vocabulary — WILL (P3a/P3b)

A `pd-*` family beside the review driver's `rd-*`, every row carrying
`on_behalf_of`. See #3040.

## 8. The planner's output contract — WILL (P2/P4)

`planner.md` will state that the `orrerix-plan` block is mandatory when the
planner was spawned by a drive, and recommended otherwise. See #3040.

## Related

- `doc/design/review-driver.md` — the review driver this one reuses, and whose
  record/reconcile/notice shape the plan driver copies rather than abstracts.
- `doc/design/groupid-and-path-roots.md` — why the slice id goes through
  `pathseg::check_segment` rather than through a private predicate.
