//! Orchestrator/worker agent groups: registry, guardrails, persistence,
//! audit log, and visible prompt delivery.
//!
//! An orchestration *group* is one orchestrator pane plus the worker and
//! reviewer panes it manages, all running `claude` CLIs connected to the
//! loomux MCP server (see `mcp.rs`) with per-agent identity tokens. Panes
//! are frontend-owned, so spawning round-trips: registry emits
//! `orch-spawn-request` → frontend opens the pane → `bind_agent` reports the
//! pty id back and unblocks the spawner.
//!
//! Inter-agent communication is deliberately *typed into the recipient's
//! CLI* (bracketed paste + Enter) rather than delivered out of band: the
//! human sees every prompt exactly as if they had written it, can steer any
//! pane, and the audit log (`audit.jsonl`) records the full text.

pub mod brief;
pub mod digest;
/// Test-only lock-hold injector for the E2E soak/liveness lane (#1603).
/// Compiled to an empty `start` in a release build — see the module doc.
pub mod e2ehold;
pub mod humanq;
pub mod mcp;
pub mod needsyou;
pub mod structured;
/// The To-Do store (#3263 slice S1): where `todo.json` lives, the
/// load-or-quarantine read, the atomic write, the audit row and the change
/// event. The model and the pure `apply` are `loomux_engine::todo`.
pub mod todo;
/// The repo tuning fingerprint behind the time-plot's marks (#2011 slice B).
/// Walks and hashes the agent-facing config, at most once per series bucket.
/// Never the GUI thread, and serialized per group by the usage memo cell — see
/// [`OrchRegistry::series_sample`] for which threads actually reach it.
pub mod tuningfp;
pub mod views;

// MOVED to the `loomux-engine` crate (#888 slice A2), re-exported here under
// their original paths — whatever a call site in this crate or in the
// integration suite already spells, here or under `orchestration::`, keeps
// resolving to it. The move is what makes a headless daemon able to link the
// orchestration core without Tauri; the re-export is what makes it a pure
// relocation rather than a rename of every call site. The `pub use` lines below
// are the list, so it cannot go stale; read the modules themselves in
// crates/loomux-engine/src/. See docs/design/engine-extraction.md.
pub use loomux_engine::report;
pub use loomux_engine::termgrid;
/// The persisted usage time series' pure core (#2011 slice B): the row schema,
/// the sampling predicate, the fault-tolerant parser and the differencing. The
/// WRITER lives here, in `src-tauri`, beside the usage collector it samples.
pub use loomux_engine::usageseries;

// `self` keeps the module path `orchestration::groupid` alive alongside the two
// types, so nothing that spells either form has to move. `GroupId` living in
// the engine is the point rather than a side effect: the engine's public API
// takes a validated id and never a `&str`, so a crate consumer — a daemon, a
// network peer — cannot call without having parsed. The trust that used to be a
// fact about the transport (#904) is now a fact about the type, which is what
// makes it survive leaving this crate at all.
pub use loomux_engine::groupid::{self, GroupId, GroupIdError};
pub use loomux_engine::pathseg::{PathSegment, SegmentError};
pub use loomux_engine::rootreg::{RootError, RootRegistry};

// #888 slice A2 batch 3. `lessons` and `notify` were leaves in everything but
// one edge each — `lessons` reached back here for `tail_snippet`, `notify` for
// `pr_number` — and both of those are pure string functions rather than
// registry state. So the helpers moved into the engine ahead of their callers
// (`loomux_engine::text`) and the modules followed, which is the cheaper answer
// than inventing a trait to reach back for a `&str` cut. `humanq` stays in this
// crate and reaches `notify` through the re-export below exactly as it always
// did — `super::notify::sanitize_gh_text` still resolves — and it does not move:
// it is #946/#959 trust-boundary code whose relocation is a decision of its own,
// not a side effect of this one. (`intake` was the other module named here; it
// crossed in batch 11 below and reaches `notify` as `crate::notify` now.)
pub use loomux_engine::{lessons, notify};

// The two helpers themselves, re-exported under the names they had while they
// were defined in this file, so every call site here and in the integration
// suite (`orchestration::pr_number`) resolves unchanged. `tail_snippet` keeps
// its `pub(crate)` reach rather than following the engine's `pub`: the engine
// has to expose it to be usable across the crate boundary at all, but nothing
// outside this crate ever spelled it, and a re-export is the one place that
// choice is still ours to make.
pub use loomux_engine::text::pr_number;
pub(crate) use loomux_engine::text::tail_snippet;

// #888 slice A2 batch 4 — the shared DATA MODEL: the capability classes, the
// deny tiers they select, the per-CLI capability table and the pure functions
// that read them. All data plus `match`, and all of it is what the `workflow`
// cluster reaches for — `Role` for a block's `kind:`, `CLI_CAPS` for its knob
// remedies, `EFFORT_LEVELS`/`CONTEXT_VARIANTS` for its closed vocabularies,
// `sanitize_model_opt` for its `model:`. Moving `workflow` first would have left
// every one of those edges pointing back into this crate, so the model goes
// ahead of it. `self` keeps `orchestration::model::…` spellable alongside the
// flat names every existing call site uses.
//
// `default_model`/`sanitize_model_opt` were `pub(crate)` here, and the
// `pub(crate) use` below keeps that reach for the FLAT spelling — the one every
// call site in this crate uses. Say it that precisely, because the shorter
// version ("the re-export keeps them `pub(crate)`") is wrong and was written
// here first: the `self` on the line above re-exports the whole module
// publicly, so every `pub` item in the engine's `model` is also reachable as
// `orchestration::model::…`, these two included. That is forced — an item must
// be `pub` in the engine to be callable from here at all — and harmless, since
// `loomux-engine` is `publish = false` and "public" means reachable by a
// sibling crate in this workspace, not a shipped API. Contrast `tail_snippet`
// above, where the claim really is the narrow one: `text` has no module
// re-export, only the single item, so no `orchestration::text::…` path exists.
// `Role::prefix` and `Role::as_str` had the same reach and cannot keep even the
// flat form — a method's visibility is the defining crate's to set, and no
// re-export narrows it.
pub use loomux_engine::model::{
    self, cli_can_host, cli_caps, fork_refusal, premints_session_id, CliCaps, Containment,
    ForkSeam, ReadyMarker, Role, CLAUDE_FORK_PREMINTS_CHILD_ID, CLI_CAPS, CONTEXT_VARIANTS,
    EFFORT_LEVELS, SUPPORTED_CLIS,
};
pub(crate) use loomux_engine::model::{default_model, sanitize_model_opt};

// #888 slice A2 batch 5 — the `workflow` CLUSTER: the `.loomux/workflow.yml`
// parser and its types, the persona/profile loader, and the named-lock state
// machine. They move as one because they are one: `profiles` reads
// `workflow::{kind_from_str, resolve_profile_path}` while `parse_workflow`
// reads `profiles::sanitize_allow` — a cycle, which is legal inside a crate and
// impossible across two, so neither could have gone alone. `locks` joins them
// because `LockTable::sync` is typed on `workflow::ResourcePolicy` in its body
// and its tests; on this side of the boundary that is an intra-crate edge.
//
// It rides batch 4 entirely: every symbol `workflow` used to reach back here
// for — `Role`, `SUPPORTED_CLIS`, `CLI_CAPS`, `EFFORT_LEVELS`,
// `CONTEXT_VARIANTS`, `cli_caps`, `cli_can_host`, `default_model`,
// `sanitize_model_opt` — is already in `loomux_engine::model`, so the imports
// inside the moved files became `crate::model::…` rather than a new dependency.
//
// What deliberately did NOT come: `mqdriver`, which is `workflow`'s biggest
// consumer. It reaches `capture_raw_with_timeout` — and an INBOUND edge does
// not block a move. It stayed here and spelled `super::workflow::…` exactly as
// it always had, through this line. (This comment used to gloss that call as
// "i.e. the pane host, which is slice A3's problem"; batch 9 re-measured it and
// there is no host in it — see that batch's block below. `mqdriver` crossed in
// batch 12a and `mqloop` in 12b, so both import `crate::workflow::…` now and
// neither reaches this line any more.)
// The manager mailbox (#1161 M2) — born in the engine rather than moved into
// it, so it appears in no extraction batch; see `loomux_engine`'s own module doc
// for why. Re-exported flat, like `humanq` is reachable as `humanq::`, so the
// registry methods below and the integration suite spell it the same way.
pub use loomux_engine::mailbox;

pub use loomux_engine::{locks, profiles, workflow};

// Delivery triage's pure core (#3304 S1) — the leading-shape classifier, the
// never-triaged set, the rule table and the deferred store's wire form.
// Re-exported for `workflow`'s reason: every decision is made there, and what
// stays on this side is the wiring.
pub use loomux_engine::triage;

// Delivery triage's registry wiring (#3304 S1), in a file of its own, for the
// reason `rdtick` is: a gate that can SUPPRESS a delivery to the one pane a
// human supervises has to be findable as ONE scope, and a FILE is a scope a
// rename cannot step over. `src-tauri/tests/triage.rs` reads it as one.
mod triagegate;

/// #2811 S5a: the per-provider spend/usage-limit table the attention scan reads
/// a pane tail against. Re-exported here beside `workflow` because the two
/// consumers straddle the seam — `attention_tick` (`registry/idle.rs`) raises the
/// `provider-limit` chip from it, and S5b's driver hold reads the same rows.
pub use loomux_engine::providerlimit;

// Batch 5's one revision to a batch-4 decision, and the reason it is here
// rather than above with the templates: `Block::instructions_file` is a
// `workflow` method, so the function it calls had to be on the engine side or
// the whole cluster stayed. `role_template` is unaffected — it loads the
// fixture-pinned `templates/*.md` and stays. Re-exported `pub(crate)`, the same
// choice `default_model` got above — and carrying the same caveat stated there:
// this line fixes the reach of the flat `orchestration::role_instructions_file`
// spelling, not of the item, which the module re-export also exposes as
// `orchestration::model::role_instructions_file`.
pub(crate) use loomux_engine::model::role_instructions_file;

// #888 slice A2 batch 6 — the merge queue's pure core (#581 slice C) and the
// read-only projection the chrome renders (slice F). Batch 5 left `mergeq`
// behind because every symbol it imports comes from `workflow`; that module is
// in the engine now, so the edge points forward and the move is import-prefix
// deep. `mergeqview` reads `mergeq` and nothing else, and comes with it.
//
// What stays HERE is the wiring, which is the whole point of the split: the
// `#[tauri::command]` `orch_merge_queue` below still calls
// `mergeqview::merge_queue_view` through this line. `mqloop` used to spell
// `super::mergeq::…` and `super::mergeqview::…` in its body — a genuine inbound
// edge, not a doc-comment mention, and this line was what answered it. Both it
// and `mqdriver` stayed for the edges they had at the time
// (`capture_raw_with_timeout`, `atomic_write` — both in the engine as of batch
// 9, and neither a host call after all); an inbound edge has never been what
// decides a batch. `mqdriver` crossed in batch 12a and `mqloop` in 12b, and both
// reach `mergeq`/`mergeqview` as `crate::…` now.
//
// No visibility widened: neither module had a `pub(crate)` or `pub(super)` item
// to widen, so unlike batches 3-5 there is no re-export choice to argue here.
pub use loomux_engine::{mergeq, mergeqview};

// #888 slice A3 batch 8 — four small pure items lifted out of this file:
// `Delivery` (a serde enum whose kebab-case wire form travels verbatim — the
// queue.json compatibility it decides is pinned by the queue snapshot
// round-trip tests, which do NOT move this batch), the notice marker (then
// `LOOMUX_NOTICE_MARKER`, joining `pr_number` in `text` — batch 3's precedent
// for a shared pure string item; #1153 phase 3 renamed it `NOTICE_MARKER` and
// moved it on to `brand`, which is where the re-export below points and where
// its legacy spelling is kept), and `DEFAULT_IDLE_TICK_MINUTES` +
// `DEFAULT_INTAKE_POLL_MINUTES`
// (join `model`, since the latter is defined IN TERMS OF the former and the
// two travel together).
//
// Visibility widened, batch-3 precedent (state it, don't only imply it — and
// state it precisely: model.rs:61-73 is the standing correction for the
// convenient-but-wrong phrasing here, and it applies again below):
// - `Delivery::wait_ready` was bare module-private in `src-tauri`; it is
//   `pub` in the engine now, forced by the crate boundary, with no
//   re-export to narrow it back — a method's visibility is the defining
//   crate's to set, same fact batch 4 states for `Role::prefix`/`as_str`.
// - `DEFAULT_IDLE_TICK_MINUTES` and `DEFAULT_INTAKE_POLL_MINUTES` were both
//   bare module-private consts; they are `pub` in the engine now (forced,
//   same reason). The `pub(crate) use` below narrows only the FLAT spelling
//   (`orchestration::DEFAULT_IDLE_TICK_MINUTES`, the one this file actually
//   calls) back to "this crate". It does NOT narrow the item overall: line 92's
//   `self` already re-exports the whole `model` module publicly, so the const is
//   also reachable as `orchestration::model::DEFAULT_IDLE_TICK_MINUTES` — and,
//   since that path crosses no crate-private boundary, as
//   `loomux_lib::orchestration::model::DEFAULT_IDLE_TICK_MINUTES` from
//   outside this crate too. Forced and harmless, same terms as `model.rs`'s
//   own header states for `default_model`/`sanitize_model_opt`.
// - `NOTICE_MARKER` was already `pub`, so nothing widens there.
//
// Batch 11 amendment: `DEFAULT_INTAKE_POLL_MINUTES` was on the `pub(crate) use`
// line above for exactly one caller, `intake.rs`, which was still in this crate.
// It crossed in batch 11 (below) and spells `crate::model::…` now, so no code in
// this crate names the const at all and the flat spelling has no consumer left.
// These `pub use` lines are meant to read as the live list of what moved, so a
// dead entry comes off rather than sitting here for the next reader to re-derive.
// The item itself is untouched — still `pub` in the engine, still reachable as
// `orchestration::model::DEFAULT_INTAKE_POLL_MINUTES` through line 92's `self`.
pub use loomux_engine::model::Delivery;
pub(crate) use loomux_engine::model::DEFAULT_IDLE_TICK_MINUTES;
pub use loomux_engine::brand;
pub use loomux_engine::brand::NOTICE_MARKER;

// #888 slice A3 batch 9 — the two HOST PRIMITIVES this file was still carrying,
// into two modules on purpose:
// - `subproc`, the bounded child-process capture (#656/#698): the timeout
//   constants, `wait_bounded`, the two-pipe drain, and the process-wide ceiling
//   on readers abandoned by a capture that gave up.
// - `fsatomic`, the durable whole-file replace (#133): `atomic_write` and the
//   sequence counter that keeps two concurrent writers' temp names apart.
//
// They are NOT combined into one `hostio` module, and the reason is that they
// share nothing but the word "host": a bounded subprocess wait and a crash-safe
// file replace answer different failure modes, cite different design notes, and
// have no symbol in common. Neither is a pane-host call — both are pure `std`
// (`std::process`/`std::thread` and `std::fs`), which is what let them move as
// ordinary leaves without waiting on the host traits A3 otherwise introduces.
// The comments above at the batch-5 and batch-6 lines called
// `capture_raw_with_timeout` a pane-host call; re-measuring it for this batch
// found no host in it, and both of those comments are corrected in place.
//
// `subproc`'s one outward edge is `lock_safe` (the backlog list's `Mutex`),
// which is `crate::obs::LockExt` in the engine since batch 7 — that ordering
// was batch 7's whole reason to go first. `fsatomic` has no outward edge at all.
//
// Visibility, stated precisely (the correction at model.rs:61-73 applies here
// too): every item below was already `pub` in this file except `atomic_write`,
// which was `pub(super)`. It has to be `pub` in the engine to be callable from
// here at all, so `loomux_engine::fsatomic::atomic_write` IS public API of that
// crate now — forced, not chosen. The `pub(super) use` below fixes only the
// reach of the FLAT `orchestration::atomic_write` spelling (the one this file
// calls — `mqloop.rs` was the other caller until it crossed in batch 12b and
// began spelling `crate::fsatomic::atomic_write`); it does not narrow the item,
// and nothing here claims
// it does. Harmless because `loomux-engine` is `publish = false`: "public"
// means reachable by a sibling crate in this workspace, not a shipped API.
// Unlike `model`/`groupid`, neither line re-exports its module (`{self}`), so
// no `orchestration::subproc::…` / `orchestration::fsatomic::…` path exists —
// the items below are the whole surface, and the private members of each
// cluster (`GH_CAPTURE_POLL_STEP`, `GH_CAPTURE_REAP_TIMEOUT`,
// `GH_CAPTURE_LEAKED_READERS`, `sweep_leaked_readers`,
// `abandon_child_and_readers`, `capture_raw_inner`, `ATOMIC_WRITE_SEQ`) stayed
// private in the engine rather than being widened to make a move compile.
pub use loomux_engine::subproc::{
    capture_raw_with_timeout, gh_capture_admitted, wait_bounded, GH_CAPTURE_MAX_LEAKED_READERS,
    GH_CAPTURE_TIMEOUT,
};
// The `#[doc(hidden)]` test seams (#699), kept hidden on the re-export too so
// the flat spelling documents exactly as the items do.
#[doc(hidden)]
pub use loomux_engine::subproc::{
    capture_raw_with_failing_wait_for_test, drain_parked_readers_for_test, gh_capture_live_readers,
    gh_capture_parked_readers, seed_leaked_readers_for_test,
};
pub(super) use loomux_engine::fsatomic::atomic_write;

// #888 slice A3 batch 10 — the delivery queue:
// - `queue`, the pure core of the per-pane FIFO (#445/#468/#467): admission and
//   coalescing, the flush plan, the `queue.json` snapshot and its recovery
//   split, the archive, and the audit-derived orphan view.
// - `queuestate`, the two mutable maps this file used to hold as plain fields
//   (#562/#497): `QueueMap`, whose only `&mut` door writes the snapshot on the
//   way out, and `DrainerRegistry`, whose only removal is generation-checked.
//   Its module doc argues the file boundary IS the mechanism — the maps are
//   private to a module rather than to this 30k-line one — and that argument
//   survives the move unchanged, since it was never about which crate the
//   module sits in.
//
// A chain, not a cycle (batch 6's distinction): `queuestate` names `queue`,
// `queue` never names back, so `queue` could have moved alone. They travel
// together because `queuestate`'s only other edges are `GroupId` and
// `crate::obs::LockExt` — both across since batches 2 and 7 — and because the
// maps have nothing left in the Tauri half to be near. `queue`'s own outbound
// set is three items, all likewise already across: `GroupId` (batch 2),
// `Delivery` and `NOTICE_MARKER` (batch 8). The impure half stays here
// and is unaffected: `enqueue_text`, `deliver_now`, `run_queue_drainer`,
// `persist_queues`, `readmit_recovered` all still spell `queue::…` /
// `queuestate::…` through the line below.
//
// The re-export is the plain MODULE form rather than batch 9's curated item
// list, and the choice is measured rather than stylistic. Every consumer — this
// file and `src-tauri/tests/orchestration/` alike — spells the module path
// (`queue::QueuedDelivery`, `queuestate::QueueMap`), never a flat
// `orchestration::…` name, so an item list would preserve no call site at all.
// #988's visibility trap is what a curated list buys protection from, and there
// is nothing here for it to catch: neither file has a single `pub(super)` or
// `pub(crate)` item, so the crate boundary force-widens NOTHING, and the
// private members of each (`lenient_group_id`, `flush_cause_clause`,
// `constituent_banner`, `age_clause`, `archive_line_version`,
// `FLUSH_ITEM_OVERHEAD`, `QueueDirty::write_needed`, and both maps' `inner`
// fields) stay private in the engine. `pub mod queue` already sat under
// `pub mod orchestration`, so `loomux_lib::orchestration::queue::…` reached
// exactly this set before the move and reaches exactly it after.
//
// Stated precisely, because "unchanged" is the word model.rs:61-73 exists to
// correct and it would be wrong here too if left bare: NO ITEM WIDENED — not
// one visibility keyword in either file differs from what it was — and the
// `orchestration::` spelling reaches the identical set. What IS new is a second
// spelling, `loomux_engine::queue::…`, and that is inherent to crossing the
// boundary at all rather than a consequence of the re-export shape: an item
// must be `pub` in the engine to be callable from here, and every batch since
// batch 2 has added the same. Harmless on the same terms — `loomux-engine` is
// `publish = false`, so "public" means reachable by a sibling crate in this
// workspace, not a shipped API.
pub use loomux_engine::{queue, queuestate};

// #888 slice A3 batch 11 — `intake`, the pure core of the idle-tick intake gate
// (#332/#429/#795/#864/#778): the host-side, zero-token diff of what changed on
// GitHub since the last poll (label deltas, PR check transitions, PR
// comment/review activity, the full-autonomy eligible-unstarted set), the
// bounded wake summary it composes, the poll-scheduling policy, and the pure
// decision of whether a tick that cleared its quiet window should actually wake
// the orchestrator.
//
// One import prefix deep, because every outbound edge was already across:
// `super::notify` → `crate::notify` (batch 3), `super::DEFAULT_INTAKE_POLL_MINUTES`
// → `crate::model::…` (batch 8), `super::GroupId` → `crate::groupid::GroupId`
// (batch 2). The impure half stays HERE and is unaffected — `poll_intake` (the
// two `gh` calls, the `gh` allow-list they go through, the audit records) and
// `idle_tick_tick` still spell `intake::…` through the line below, exactly as
// `mqdriver`/`mqloop` did for batches 5/6/9 (both have since crossed themselves,
// in batches 12a and 12b).
//
// The re-export is the plain MODULE form, and — as in batch 10 — that is
// measured rather than stylistic. EVERY consumer spells the module path:
// `intake::due_intake_polls`, `intake::PendingIntake`, `intake::pr_list_argv`
// here, `intake::eligible_deltas` in `tests/workflow/intakeprofile.rs`, and
// `loomux_lib::orchestration::intake::MAX_INTAKE_POLLS_PER_TICK` in
// `tests/orchestration/`. Not one flat `orchestration::<item>` spelling
// exists, so a curated item list (#988) would preserve no call site at all.
// #988's trap is what a curated list buys protection from, and there is nothing
// here to catch: `intake.rs` contains not a single `pub(super)` or `pub(crate)`
// item, so the crate boundary force-widens NOTHING and the file's private
// members (`RawLabel`, `RawIssueJson`, `RawRollupEntry`, `RawCommentJson`,
// `RawReviewJson`, `RawPrJson`, `rollup_entry_state`, `parse_task_issue_ref`,
// and `PendingIntake`'s `blocks`/`dropped` fields) stay private in the engine.
// `pub mod intake` already sat under `pub mod orchestration`, so
// `loomux_lib::orchestration::intake::…` reached exactly this set before the
// move and reaches exactly it after.
//
// What this batch does NOT claim is zero test edits, and the reason is a kind of
// edge no grep for `super::` finds — batch 2's "where can the violation be
// spelled now?" asked of a FILE rather than a type.
// `poll_intake_still_asks_gh_for_comment_and_review_activity` reads `intake.rs`
// **by literal path** to pin that the `createdAt`/`submittedAt` serde renames
// survive, because losing them degrades the #864 comment signal to permanent
// silence with every other test still green. Moving the file breaks that read
// outright, so the test's path is repointed at `crates/loomux-engine/src` (the
// spelling `tests/groupid.rs` already uses for its second root) and nothing else
// about it changes. Its OTHER half — the scan asserting THIS file reaches `gh`
// through the `intake` argv builder rather than hand-rolling an argv beside it —
// is untouched and still true, which is one more thing the module re-export buys
// that an item list would have broken. (Note for anyone editing this comment:
// that scan is textual and `contains`-based, so the call spelling it looks for —
// the builder name followed by an empty argument list — is deliberately NOT
// written out anywhere in this file except at the real call site in
// `poll_intake`. Spelling it in prose would satisfy the pin from a comment and
// leave it green over a poller that had stopped calling the builder at all.)
pub use loomux_engine::intake;

// #888 slice A3 batches 12a and 12b — the bisecting merge queue's two impure
// halves, and the last of A3's module moves:
// - `mqdriver`, the WRITE PRIMITIVES (#581 slice D1): the `MqRunner` seam and
//   its process implementation, the live default-branch/PR lookups,
//   `validate_target`'s constraint-7 refusal core, scratch minting and the
//   create-only push, `land_batch`, cleanup. Batch 12a.
// - `mqloop`, the DRIVER LOOP (#581 D2/D3): §8's batch construction, the draft
//   PR, the bounded check observation, §9's bisect and culprit attribution,
//   §4's crash reconcile, `merge_queue.json` persistence, and `drive`, the
//   one-step-per-call tick. Batch 12b.
//
// Split into two batches deliberately (a chain, not a cycle — `mqloop` imports
// `mqdriver`, `mqdriver` names `mqloop` only in prose): these are the feature's
// two largest files and this one is the repo's highest-conflict file, so two
// reviewable diffs beat one. Everything outbound from either was already across
// before it went — `mergeq`/`mergeqview` (batch 6), `notify` (batch 3),
// `workflow` (batch 5), `capture_raw_with_timeout` and `atomic_write` (batch 9)
// — so both moves are import prefixes and nothing else.
//
// The re-export is the plain MODULE form for both, and for `mqdriver` that is a
// change from what batch 12a shipped, not a restatement of it. Between the two
// batches `orchestration/mqdriver.rs` survived as a curated re-export MODULE
// (batch 7's `obs.rs` shape) for one reason: the module had three `pub(super)`
// items — `as_args`, `landable`, `declares_ci_green` — whose only caller,
// `mqloop`, was still in this crate, so the crate boundary had force-widened
// them to `pub` and a `pub(super) use` was the only thing that could keep the
// `orchestration::mqdriver::…` spelling reaching what it used to. Batch 12b
// moved that caller into the engine, where it spells `crate::mqdriver::…`. With
// no consumer left on this side, the three went back down to `pub(crate)` in
// the engine — the faithful translation of the old `pub(super)`, since the
// scope that was "the `orchestration` module" is now "the engine crate" — and
// the curated file, having nothing left to narrow, collapsed into the single
// line below. That is not a tidy-up: while the items were `pub`, nothing could
// stop `loomux_engine::mqdriver::landable` compiling from anywhere in this
// crate, and `landable` is only HALF of the constraint-7 refusal
// (`validate_target` is the whole of it). Batch 12a's header said it could do
// nothing about that; 12b can, and did.
//
// So the shape follows the callers, as in batches 10 and 11: every consumer
// spells the module path (`mqdriver::MqRunner`, `mqdriver::runner_for`,
// `mqdriver::audit_action::…`, `mqloop::drive`, `mqloop::refusal::…` here;
// `loomux_lib::orchestration::{mqdriver,mqloop}::…` in
// `src-tauri/tests/mergequeue.rs` and `tests/orchestration/`), and no flat
// `orchestration::<item>` spelling of either exists. #988's trap — a curated
// list buying a narrowing that is real — now has nothing left to catch here:
// after the `pub(crate)` reversion neither module has a single item this crate
// can reach that it could not reach before the move, and `mqloop` never had a
// `pub(super)` or `pub(crate)` item at all, so its own move force-widened
// NOTHING. Both files' private members stay private in the engine.
//
// What stays HERE is the wiring it always was: `queue_merge_with`,
// `mq_drive_group_with`, `mq_driver_tick`, the two `merge_queue_reconcile*`
// methods and the `mq_runner_override` field, all of which resolve paths,
// delegate, and audit what comes back.
pub use loomux_engine::{mqdriver, mqloop};

// The review driver's pure core (#1778 S1) — the state machine, the persisted
// shape and the per-tick decision, none of which touch a file or a child
// process. Re-exported for the same reason `mqloop` is: what stays HERE is the
// wiring — `rd_drive_group_with`, `rd_driver_tick`, the reconcile and the
// `rd_runner_override` field — which resolves paths, spawns, delivers and
// audits what comes back. `reviewdrive::decide` makes every decision.
pub use loomux_engine::{rddrive, reviewdrive};

// The review driver's registry wiring (#1778 S3), in a module of its own,
// split by tick phase into `rdtick/` (#3498 P5).
//
// Not for size alone, though `mod.rs` was then tens of thousands of lines,
// reason enough. It is what gives §3.1 item 1's source scan a scope that a
// RENAME cannot step over: CLAUDE.md's source-scanning-guard convention
// forbids deciding from a binding's name, and the design note names an
// `rd_*` prefix as exactly the scope that fails that test. A FILE is not a
// name — every landing verb the driver could reach has to be written
// somewhere, and `tests/reviewdrive/guards.rs` default-denies the whole of this
// one, reading every file under `rdtick/` as one scope.
mod rdtick;
pub use rdtick::{
    RdDriveReport, RdEvent, RdSignal, DRIVER_DELTA_TPL, DRIVER_FIX_TPL, DRIVER_REVIEW_TPL,
};

// The plan driver's pure core (#3040 P1/P3a) — the plan block's parser and the
// drive's state machine, record and per-tick decision. Re-exported for
// `reviewdrive`'s reason, and the split is the same one: `plandrive::decide`
// makes every decision, and what stays HERE is the wiring.
pub use loomux_engine::{plandoc, plandrive};

// The plan driver's registry wiring (#3040 P3a), in a file of its own — for
// `rdtick`'s reason above, which is a FILE being a scope a rename cannot step
// over. `tests/plandrive.rs` default-denies the whole of it.
mod pdtick;
pub use pdtick::{
    PdDriveReport, PdEvent, PdPlanCheck, PdSignal, PdWorkerSignal, PD_MAX_GH_PER_TICK,
    PD_MAX_PR_CHECKS_PER_TICK,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Weak};
// #1609: the thread-local read budget, MutationScope and the six budget
// constants. See `docs/design/lock-liveness.md`.
use loomux_engine::budget;
use loomux_engine::lockwatch::TrackedMutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

use crate::obs::LockExt;

mod templates;
pub use templates::*;
mod activeworkflow;
pub use activeworkflow::*;
mod agentmodel;
pub use agentmodel::*;
mod auditlog;
pub use auditlog::*;
mod board;
pub use board::*;
mod channelmodel;
pub use channelmodel::*;
mod clis;
pub use clis::*;
mod compactnudge;
pub use compactnudge::*;
mod desktopnotify;
use desktopnotify::*;
mod drainer;
pub use drainer::*;
mod exits;
pub use exits::*;
mod fork;
pub use fork::*;
mod ghcomment;
pub use ghcomment::*;
mod ghgate;
pub use ghgate::*;
mod ghshim;
pub use ghshim::*;
mod grouppath;
pub use grouppath::*;
mod guardrails;
pub use guardrails::*;
mod holds;
pub use holds::*;
mod humaninput;
pub use humaninput::*;
mod idlepolicy;
pub use idlepolicy::*;
mod kickoff;
pub use kickoff::*;
mod lockprose;
use lockprose::*;
mod noticemask;
pub use noticemask::*;
mod panetail;
pub use panetail::*;
mod pathkey;
pub use pathkey::*;
mod persona;
pub use persona::*;
mod questionhold;
pub use questionhold::*;
mod refusals;
pub use refusals::*;
mod screen;
pub use screen::*;
mod solopane;
pub use solopane::*;
mod spawnpolicy;
pub use spawnpolicy::*;
mod statusline;
pub use statusline::*;
mod strandedpolicy;
pub use strandedpolicy::*;
mod submit;
pub use submit::*;
mod ticks;
pub use ticks::*;
mod tier1;
pub use tier1::*;
mod tuning;
pub use tuning::*;
mod unconfirmed;
pub use unconfirmed::*;
mod usagestore;
pub use usagestore::*;
mod usageview;
pub use usageview::*;
mod worktrees;
pub use worktrees::*;

/// The registry's declared lock order (#1610, plan §3 Phase 3a).
///
/// **Smaller is outer.** A thread may take a larger rank while holding a
/// smaller one; taking a smaller one while holding a larger one is an
/// inversion, and [`loomux_engine::lockwatch`] panics on it in debug and test
/// builds and breadcrumbs `lock-order-violation` in release.
///
/// # Why a table and not thirteen comments
///
/// #1600 §3 opens on the problem this closes: seventeen mutexes on one struct
/// with no declared order, and `resolve_token`'s own comment explaining that
/// "locking them together would pin a lock order no other call site promises to
/// respect". Thirteen doc comments in this file DO state an order. Every one of
/// them is true and none of them can fail a build — which is §2.2's finding
/// about this repo's guards in one sentence. The consts below are those
/// thirteen claims, merged, in a form the run time checks.
///
/// # What is NOT here, and why that is not a hole
///
/// About sixty-five registry fields are still UNRANKED, and a lock with no rank
/// may nest under anything. That is the plan's design rather than an unfinished
/// edge: an unranked lock breadcrumbs `lock-rank-unranked` the first time it
/// nests under anything, so the rows this table is missing announce themselves
/// from the field instead of waiting to be noticed. The ranked set is exactly
/// the set someone has already written an ordering claim about — a rank
/// invented for a lock nobody has reasoned about would be a fact the checker
/// enforces and no one has checked.
///
/// # The gaps
///
/// Ranks are spaced so a new lock can be slotted between two existing ones
/// without renumbering: a renumbering changes every line at once, and a diff in
/// which every line changed is one nobody can review for ORDER — the single
/// property these values carry.
pub mod lockorder;

mod registry;
pub use registry::*;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

mod commands;
pub use commands::*;
