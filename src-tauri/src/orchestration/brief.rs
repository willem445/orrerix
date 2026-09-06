//! Composition of the task brief a delegate pane is kicked off with (#3040).
//!
//! The one thing this module owns today is the **definition of done**, and the
//! reason it is a module rather than a `const` beside the other templates is
//! what P3b will do with it: the plan driver assembles a brief as
//! `[header] + "\n\n" + <the planner's text, verbatim> + "\n\n" + <the DoD>`,
//! and the orchestrator's hand-written briefs must quote the same bytes. A
//! shared function is what makes "the same bytes" checkable rather than
//! asserted — `the_dod_trailer_is_the_worker_s_own_definition_of_done`
//! (`src-tauri/tests/prompts.rs`) is that check.
//!
//! **Why one copy at all.** The DoD used to live twice: the full
//! `## Definition of done` section in `worker.md` and a compressed
//! one-sentence recap in `orchestrator.md`'s delegation protocol. Two copies
//! of a rule an agent executes literally is a drift this repo keeps paying
//! for — an edit lands on one, and the other keeps instructing every reader of
//! that surface to do the old thing, with nothing red to say so. So
//! `templates/dod.md` is the single copy; `worker.md` and the orchestrator
//! playbook both substitute `{{DOD}}` under their own heading, and
//! `the_dod_is_one_copy` fails if a second copy is ever written.

/// The single copy of the definition of done — the section BODY, with no
/// `## ` heading of its own.
///
/// Body-only is load-bearing rather than a style choice. The playbook serves
/// itself one `## ` section per `read_playbook` call and derives its section
/// ids from the **template's** headings
/// (`every_playbook_heading_yields_a_unique_id_and_the_tool_enum_lists_exactly_them`),
/// so `## Definition of done` has to be a literal line in
/// `orchestrator-playbook.md` rather than arriving inside a substitution the
/// scan cannot see. `worker.md` carries the same heading literally for
/// symmetry, and both then substitute this body under it. What is deduplicated
/// is the RULE text — every sentence an agent executes — which is what
/// `the_dod_is_one_copy` scans for.
///
/// `pub` for the golden fixture in `tests/workflow.rs`, which pins these bytes
/// against a human-blessed copy — the re-bless gate an edit to the definition
/// of done has to pass. Nothing in the product reads it directly; use
/// [`dod_body`], which is the value the templates actually substitute.
#[doc(hidden)] // pub for integration tests
pub const DOD_TPL: &str = include_str!("templates/dod.md");

/// The `{{DOD}}` substitution value: [`DOD_TPL`] **without** its trailing
/// newline.
///
/// The trim is load-bearing rather than tidiness. `{{DOD}}` sits on a line of
/// its own in both templates that carry it, so that line already supplies the
/// terminating `\n`; a value ending in one too would insert a blank line and
/// change bytes a golden fixture pins. A file on disk ends with a newline
/// (`.gitattributes` pins these templates to `eol=lf`), so the const has one
/// and the value must not.
pub fn dod_body() -> &'static str {
    DOD_TPL.trim_end_matches('\n')
}

/// The heading the DoD is served under, on every surface that carries it.
///
/// Named once here so [`dod_trailer`] cannot drift from the templates: the
/// test named in the module docs asserts this framing reproduces `worker.md`'s
/// rendered section byte for byte.
const DOD_HEADING: &str = "## Definition of done";

/// The definition-of-done trailer appended to a spawned worker's brief.
///
/// **P3b will call this**: the plan driver appends it under the planner's
/// verbatim brief text, so a driver-spawned worker is held to the same bytes
/// the orchestrator's own briefs quote — the orchestrator reads them through
/// `read_playbook("definition-of-done")`, which serves this same body under
/// this same heading. Today the text also reaches a worker through its
/// instruction file (`worker.md`'s `{{DOD}}`), which is why nothing in
/// `kickoff_body` calls this yet.
pub fn dod_trailer() -> String {
    format!("{DOD_HEADING}\n\n{}\n", dod_body())
}
