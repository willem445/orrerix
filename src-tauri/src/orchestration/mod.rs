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
// here, `intake::eligible_deltas` in `tests/workflow.rs`, and
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

// The review driver's registry wiring (#1778 S3), in a file of its own.
//
// Not for size alone, though `mod.rs` being tens of thousands of lines is
// reason enough. It is what gives §3.1 item 1's source scan a scope that a
// RENAME cannot step over: CLAUDE.md's source-scanning-guard convention
// forbids deciding from a binding's name, and the design note names an
// `rd_*` prefix as exactly the scope that fails that test. A FILE is not a
// name — every landing verb the driver could reach has to be written
// somewhere, and `tests/reviewdrive.rs` default-denies the whole of this one.
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
mod board;
pub use board::*;
mod channelmodel;
pub use channelmodel::*;
mod clis;
pub use clis::*;
mod compactnudge;
pub use compactnudge::*;
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
mod idlepolicy;
pub use idlepolicy::*;
mod kickoff;
pub use kickoff::*;
mod persona;
pub use persona::*;
mod screen;
pub use screen::*;
mod solopane;
pub use solopane::*;
mod spawnpolicy;
pub use spawnpolicy::*;
mod statusline;
pub use statusline::*;
mod tuning;
pub use tuning::*;
mod worktrees;
pub use worktrees::*;
/// How often the idle reaper wakes to look for workers to auto-kill.
const IDLE_REAP_INTERVAL: Duration = Duration::from_secs(30);
/// How often the watchdog wakes to look for stalled working agents.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(30);
/// How often the low-disk backstop samples free space on the workspace drive
/// (#134). Slow on purpose — disk pressure builds over minutes of cargo builds,
/// not seconds — so the sysinfo scan stays negligible.
const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Autonomous mode (#83): how often the idle-tick loop wakes to check whether an
/// autonomous group's orchestrator has gone output-quiet long enough to warrant a
/// tick, and to enforce autonomy budgets. Coarser than the watchdog because the
/// gate is measured in minutes, not seconds — a 60s wake is cheap and precise
/// enough. See `start_idle_tick` / `run_idle_tick`.
const IDLE_TICK_INTERVAL: Duration = Duration::from_secs(60);
/// Round 10 (#428 follow-up, user-directed): the compact-nudge loop's poll
/// cadence WHILE any agent has a compact arm open — evidence (marker files,
/// the Copilot completion-paint detector) only gets consumed on this
/// thread's own wake, so riding the full `IDLE_TICK_INTERVAL` (60s) while an
/// outcome is already effectively decided (hook-confirmed, just waiting on
/// the poll) is exactly the residual UX gap a live user re-test caught: the
/// badge sits in "awaiting evidence" limbo for up to a minute per poll even
/// though nothing is actually undecided anymore. Idle cost is zero — this
/// is only ever consulted while `any_compact_pending()` is true, which is
/// false for the overwhelming majority of a session's wall-clock time (no
/// compaction in flight). See `compact_nudge_poll_interval`.
const COMPACT_NUDGE_FAST_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Idle-tick intake gate (#332): how often the intake half of the unified
/// `gh` poller scans for a group whose `intake_poll_minutes` interval has
/// elapsed. Coarser polling than this would risk missing a group's own
/// interval by a noticeable margin; finer buys nothing, since
/// `intake::due_intake_polls` still gates the actual `gh` calls to each
/// group's configured minutes.
///
/// #406: this is no longer a thread's sleep — it is the scan floor
/// `intake_scan_due` applies inside the single poll loop, whose own wake
/// cadence is the finer `notify::NOTIFY_POLL_INTERVAL`. Same cadence, one
/// fewer thread.
const INTAKE_POLL_SCAN_INTERVAL: Duration = Duration::from_secs(60);
/// Merge-gate hot-reload (#385): how often `run_workflow_gate_reload` re-checks
/// every advanced-orchestrator group's `.loomux/workflow.yml` against its
/// currently-armed gate. A human edit reading the workflow file wants to feel
/// "took effect", not "cache-invalidated eventually" — but the gate is
/// security-enforcing, not UI-cosmetic, so this errs toward the same 30s
/// cadence as the idle reaper/watchdog rather than the 3s attention-router
/// tick: prompt enough that nobody waits a minute wondering if their edit
/// landed, coarse enough that a small per-group YAML read+parse is negligible
/// even with several live groups.
const WORKFLOW_GATE_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Which projection of the memoised group-usage value a reader wants (#1317).
///
/// Both come out of ONE computation and one memo cell — see
/// [`OrchRegistry::group_usage_memoed`].
#[derive(Clone, Copy)]
enum UsageView {
    /// The whole-roster value: every snapshot the group ever captured. What
    /// the MCP `group_usage` tool, the autonomy anchor and the budget enforcer
    /// read.
    Full,
    /// [`live_usage_view`] of it — what the POLLED GUI command returns.
    Live,
}

/// Project [`OrchRegistry::group_usage`]'s value down to what a POLLED reader
/// needs: the live agents' rows, and no per-agent row for anyone else (#1317).
///
/// **What it is fixing.** `agents` is O(agents-EVER), not O(agents-live):
/// `compute_group_usage` merges each live agent's fresh snapshot with every
/// snapshot `mark_dead` ever captured, so the array only grows with how long
/// the human has been running. Nothing accumulates ACROSS ticks — the array is
/// replaced wholesale each time — but the size of ONE tick grows with session
/// length, on a fixed cadence (a 2 s group view and a 4 s per-group-bound-tab
/// sweep when #1317 measured it; one 1 s publisher pass since #1608, which
/// changed which thread pays it, not that it is paid).
/// That is allocation churn proportional to session length, which is what a
/// long session feels as GC pressure. The MCP twin was capped for exactly this
/// (`mcp::summarize_group_usage`: "a 654-agent lifetime roster serialized to
/// 173,245 chars"); the command the GUI polls was not.
///
/// **Why LIVE is the right cut, rather than a top-N cap.** The GUI never
/// wanted the historical rows: `groupview.ts` indexes this array by agent id
/// and looks up only the agents `orch_group_summary` reports LIVE, and
/// `tabbar.ts` reads `live_cost_usd` and no row at all. Live agents are
/// bounded by the group's `max_agents`; the lifetime roster is bounded by
/// nothing. A top-N cap would be both looser and — since top-N is by lifetime
/// tokens — capable of dropping a live agent's row, which is the one row this
/// caller actually renders.
///
/// **Not a silent truncation**, the property `summarize_group_usage`'s `rest`
/// count holds for the MCP twin:
/// - every LIFETIME total passes through untouched — `lifetime_tokens`,
///   `lifetime_cost_usd` and `lifetime_cost_basis` still sum the whole roster,
///   so no spend disappears from the figure the group view puts on screen;
/// - `agent_count` names the roster's real size, so `agent_count !=
///   live_agents.len()` is readable rather than invisible;
/// - the key is RENAMED (`agents` → `live_agents`) rather than filtered in
///   place, so a reader written against the whole-roster `agents` fails loudly
///   instead of quietly seeing a subset. #866 took the same decision for
///   `top_agents` on the MCP side, for the same reason.
///
/// Builds a fresh object rather than cloning and stripping: cloning the full
/// value to throw the historical rows away would pay the very allocation this
/// exists to remove.
#[doc(hidden)] // pub for integration tests
pub fn live_usage_view(full: &Value) -> Value {
    let rows = full.get("agents").and_then(|a| a.as_array());
    let agent_count = rows.map_or(0, |r| r.len());
    let live: Vec<Value> = rows
        .map(|r| {
            r.iter()
                .filter(|a| a["live"].as_bool().unwrap_or(false))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mut out = serde_json::Map::new();
    if let Some(obj) = full.as_object() {
        for (k, v) in obj {
            if k == "agents" {
                continue;
            }
            out.insert(k.clone(), v.clone());
        }
    }
    out.insert("agent_count".to_string(), json!(agent_count));
    out.insert("live_agents".to_string(), Value::Array(live));
    Value::Object(out)
}
/// How often the attention scan recomputes which panes need the human
/// (idle-with-prompt detection; report/gate signals are event-driven and
/// picked up on the next tick).
const ATTENTION_INTERVAL: Duration = Duration::from_secs(3);
/// How many attention ticks apart the stuck-prompt badge janitor runs (#825
/// M2) — every 10th, so ~30s. It rides this loop rather than owning a thread
/// because it is the same "does this pane still need the human" question the
/// loop already exists to ask, just answered from the pane instead of from the
/// registry; a second thread would be a second cadence to reason about for no
/// gain. Divided rather than per-tick because the answer only changes when a
/// human touches the pane: at 3s the pass would take ten times the pane reads
/// to notice the same thing ten times later at worst, which is the wrong
/// trade for work that reads a pty ring. See `start_attention` for the cost
/// bound this cadence is half of.
///
/// (#825 M3) It paces the `QueueFull` re-admission pass on the same tick. That
/// one is strictly cheaper — two map reads per badged pane and no pty read at
/// all — so it is here to share one cadence to reason about rather than because
/// it needs slowing down.
const STRANDED_JANITOR_EVERY_N_TICKS: u64 = 10;
/// How many RAW bytes of a pane's output ring one attention tick reads (#717).
/// Everything the scan looks for (a prompt, a question, a menu) is the last
/// thing the CLI painted, so the tail has always been all it needed — but the
/// read used to fetch the whole (up to 256 KB) ring and slice these bytes off
/// the end, and that fetch happens under the global `ptys` mutex, the one
/// `write_pty`/`note_user_input` take on every keystroke and the one the pane's
/// own reader thread contends with on the ring behind it. Bounding the REQUEST
/// is the entire point: the stripped text handed to the detectors is
/// byte-identical either way (`strip_ansi(&ring[len-N..])` is
/// `strip_ansi(&last_N)`), so what changes is only how long the lock is held.
pub const ATTENTION_SCAN_BYTES: usize = 4096;
/// Output growth (bytes) within the window that counts as a landed submit.
/// Set well above idle cursor-blink noise so confirmation biases against false
/// positives: a false "unconfirmed" only costs a harmless no-op flush next
/// time, whereas a false "confirmed" would let stranded text merge.
const SUBMIT_CONFIRM_MIN_BYTES: u64 = 24;

// #112 round-2 redesign: acceptance vs. processing. A queued prompt into a
// busy pane produces no PROCESSING evidence (no burst, no hook) for an
// arbitrarily long time — that's normal, correct CLI behavior, not a fault.
// Tier 1 (box consumption) answers a different, faster question — did the
// CLI ACCEPT the paste at all — and does so two-sidedly wherever it can
// verify its own precondition (see `box_holds_paste`'s doc). The extended
// "is this genuinely idle, not just busy" monitor below is what replaces a
// fixed timeout with an observed one for everything Tier 1 can't answer.
/// How much of the pane's tail (bytes, raw/pre-strip) Tier 1 reads to check
/// whether our own pasted text is still sitting at the box's tail end.
/// Same sizing philosophy as `QUESTION_SCAN_TAIL_BYTES` — reused directly
/// rather than inventing a second "how much tail matters" constant.
///
/// #559: this is Tier 1's FLOOR, not its budget. A scan window fixed at 4 KiB
/// while `QUEUE_FLUSH_MAX_BYTES` lets one coalesced flush paste 24 KiB makes
/// `box_holds_paste` structurally false for every large paste — the evidence
/// is not absent, it is unreachable — and two constants chosen for unrelated
/// reasons (question-scan cost vs. token budget) then jointly decide whether a
/// stranded delivery is ever noticed. `tier1_scan_bytes` derives the actual
/// read from the paste, with this as its lower bound; `TIER1_SCAN_MAX_BYTES`
/// is the ceiling, and it is derived from the flush cap rather than picked.
const BOX_TAIL_SCAN_BYTES: usize = QUESTION_SCAN_TAIL_BYTES;
/// Ceiling on one Tier 1 box read (#559) — the largest paste a single flush
/// may produce (`QUEUE_FLUSH_MAX_BYTES`) plus the same `BOX_TAIL_SCAN_BYTES`
/// of headroom the base window already allows for box framing, prompt symbol
/// and cursor chrome around our text. Derived, deliberately: the whole point
/// of this issue is that the scan budget and the flush cap were independent
/// numbers, so the relationship is expressed in the code rather than in a
/// comment asking a future reader to keep them in step. A single queue entry
/// larger than the flush cap still delivers alone (`plan_flush`'s "the cap is
/// a CEILING, never a floor"), so this bound can still be exceeded by the
/// paste — which is exactly the residual `BoxReading::Unverifiable` exists to
/// say out loud instead of reading as an idle pane.
const TIER1_SCAN_MAX_BYTES: usize = queue::QUEUE_FLUSH_MAX_BYTES + BOX_TAIL_SCAN_BYTES;
/// Ceiling on one WIDENED Tier 1 read (#685) — the pty output ring itself,
/// because that is the whole readable universe: a request past it cannot come
/// back with a byte the ring does not hold. Derived rather than picked, for the
/// same reason `TIER1_SCAN_MAX_BYTES` is. In practice the widening lands far
/// below this (it asks for what the measured retention says it needs, see
/// `Tier1Scan`); the cap only binds for a tail that is almost entirely escape
/// bytes.
const TIER1_SCAN_WIDEN_MAX_BYTES: usize = crate::pty::OUTPUT_RING_CAP;
/// How many times one Tier 1 read may re-request a wider tail (#685) before it
/// settles for the widest it got. Each round re-measures, so the estimate is
/// corrected rather than repeated; three is enough for a scaled request to
/// converge on a tail whose density is not uniform, and the bound exists so a
/// pathological one cannot spin. Running out is not a failure mode of its own:
/// the widest read still classifies, and a short one classifies as
/// `Unverifiable` exactly as it did before. `pub` so the bound is directly
/// testable rather than inferred from a loose "it terminated" assertion.
pub const TIER1_SCAN_WIDEN_ROUNDS: u32 = 3;
/// Extra slack (normalized characters) added around the pasted text's own
/// length when windowing "the tail end" for containment — covers box
/// framing/prompt-symbol/cursor characters immediately around our text
/// without widening the window enough to reach unrelated older output.
const BOX_TAIL_WINDOW_SLACK: usize = 200;
/// How much of one paste LINE [`paste_echo_probe`] samples for
/// [`box_reading`]'s "a partial match is not an absence" arm (#821).
///
/// Bounded above by what a single rendered row can comfortably hold. 48
/// characters is inside the narrowest pane anyone runs while being far past
/// coincidence for prose.
///
/// **"It must stay inside one row" is the conservative heuristic, not the
/// mechanism** (rev-305 Ground 4). The probe is matched against
/// [`normalize_deframed`] of the tail, where every row has already had its
/// LEADING decoration stripped and the whole thing flattened to
/// single-space-joined words — so a probe spanning a word-wrapped row boundary
/// matches perfectly well, which is the entire known copilot shape. Two things
/// actually break it, and a short probe only lowers the odds of meeting either:
/// a boundary carrying decoration `deframe` cannot reach (a trailing scrollbar
/// `┃`), or a HARD mid-word wrap, which splits one word into two tokens and so
/// inserts a space the needle does not have. Sizing this from the pane's real
/// width would be a new coupling to terminal geometry for a case that is
/// already strictly better than the status quo.
///
/// **The cost of firing too readily is real, and it is not the safe
/// direction.** `Unverifiable` falls through to `stranded_marker_action`'s
/// ordinary gates, so a probe that fires on everything erodes #813/#819's
/// repair back toward the deadlock it exists to break — a conditional deadlock
/// traded for a more frequent one. Too strict, and a decorated tail keeps
/// reading as a confident absence, which is the collision #819's licence exists
/// to prevent. The second is the expensive one, which is why the floor below
/// sits where it does rather than higher.
const PASTE_ECHO_PROBE_CHARS: usize = 48;
/// The evidence floor under [`paste_echo_probe`]: a line shorter than this
/// yields no probe at all (#821).
///
/// Same figure and same argument as `R_TOP_MIN_ANCHOR_CHARS` and
/// `SELF_ECHO_MIN_POINTER_CHARS` — short fragments are not evidence, and a
/// probe built from one would fire on coincidence, which here means declining
/// to retire a marker that should have retired. A paste with no line this long
/// simply keeps the pre-#821 reading.
const PASTE_ECHO_PROBE_MIN_CHARS: usize = 24;
/// #539 (rev-13 N3): how many delivery ids the coalesced notice names before
/// it summarizes the rest. Deliberately the SAME number as
/// `PAUSE_SUPPRESSION_LIST_MAX`, whose doc makes the argument this borrows: a
/// notice is itself a delivery pasted into a pane, so a list it cannot bound
/// is a paste it cannot bound. The audit log holds every id either way
/// (`delivery-unconfirmed-notice` records the full `delivery_ids` array), so
/// nothing is lost by capping the pasted copy.
pub const UNCONFIRMED_NOTICE_IDS_MAX: usize = PAUSE_SUPPRESSION_LIST_MAX;

// Human-typing backstop (#43, option A): even with the loomux compose strip,
// a human can still type directly into the terminal. Before the paste AND
// before the first Enter, hold delivery while the pane has seen recent
// keystrokes so a report can't land in — or submit — the human's half-typed
// line. Capped so a long compose session can't starve reports forever.
/// Treat the human as "still typing" if they hit a key within this window.
const USER_QUIET_HOLD: Duration = Duration::from_secs(4);
/// Deliver anyway once a single hold has waited this long (never starve).
const USER_QUIET_MAX_HOLD: Duration = Duration::from_secs(90);
/// Poll interval while holding for the human to go quiet.
const USER_QUIET_POLL: Duration = Duration::from_millis(250);

// Human-input paste guard (#111): the quiet backstop above only waits out
// active typing — it does NOT stop a paste landing on top of text a human
// typed and then LEFT sitting in the box (a half-written `/model`, say). Pasting
// there and pressing Enter merge-submits the human's line with the prompt (the
// live `Unknown command: /modelRun ...` collision). So before pasting, if the
// box still holds a human's unsubmitted line (tracked per keystroke as
// `input_pending`), hold for them to submit/clear it, and if it never clears,
// abort rather than blind-merge.
/// Bounded wait for the box to clear before aborting the delivery.
const HUMAN_INPUT_HOLD_MAX: Duration = Duration::from_secs(60);
/// Poll interval while holding for the box to clear.
const HUMAN_INPUT_POLL: Duration = Duration::from_millis(250);

// Interactive-question paste guard (#420): Copilot (and other CLIs) surface
// numbered/radio-select questions and y/n permission prompts as an interactive
// TUI, not as text sitting in the input box — so the guard above doesn't see
// them. A programmatic paste+Enter landing there is worse than the box-occupied
// case: Enter doesn't merge text, it SELECTS the highlighted option (usually
// the first), silently steering the agent's turn in an answer nobody chose. So
// before pasting AND before the first Enter, hold delivery while
// `prompt_wait_detected` reads a live question off the pane's output tail —
// same detector attention routing already uses (#6/#40) — until it clears
// (the human answers) or the bound elapses, in which case abort rather than
// blind-select.
/// Bounded wait for the question to clear before aborting the delivery.
/// Longer than [`HUMAN_INPUT_HOLD_MAX`]: reading and deciding a substantive
/// question takes more of a human's attention than submitting an already-typed
/// line.
const QUESTION_HOLD_MAX: Duration = Duration::from_secs(120);
/// Poll interval while holding for the question to clear.
const QUESTION_HOLD_POLL: Duration = Duration::from_millis(250);
/// Non-empty rendered rows a composition must have before it is allowed to
/// say a question is gone (#534). Two, not a coherence proof: a replay that
/// began mid-stream and was never painted over composes to near-nothing, and
/// "the screen is blank" must not be mistaken for "the screen is clear". Any
/// CLI at rest paints more than this (an input box alone is three rows), so
/// the floor only ever catches the degenerate case it names.
const GRID_MIN_RENDERED_ROWS: usize = 2;
/// How many CONSECUTIVE polls `prompt_wait_detected` must read false before
/// the interactive-question guard releases (rev-19 R1): release is state-
/// based (is the menu still on screen?), not activity-based (did a keystroke
/// arrive?) — a single false read could be a transient mid-redraw miss, so
/// two in a row is the bar. Only applies once a hold has genuinely started
/// (`question_hold_predicate`'s `ever_shown` gate) — a checkpoint that was
/// never shown a question releases on its very first check, no extra delay.
const QUESTION_RELEASE_CONSECUTIVE_CLEAR_POLLS: u32 = 2;

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

// ── lock-resource prose helpers (#858) ─────────────────────────────────────

/// Whole minutes left until `deadline`, rounded UP and never below 1 — the
/// `watchline.ts` rule: a deadline 40 seconds away is "1 min", never "0 min",
/// which reads as expired.
fn minutes_until(deadline_ms: u64, now: u64) -> u64 {
    (deadline_ms.saturating_sub(now) + 59_999) / 60_000
}

/// A duration a human reads in a notice: `45s`, `12m`, `2h 5m`.
fn human_span(ms: u64) -> String {
    let total_min = ms / 60_000;
    if total_min == 0 {
        return format!("{}s", ms / 1000);
    }
    if total_min < 60 {
        return format!("{total_min}m");
    }
    format!("{}h {}m", total_min / 60, total_min % 60)
}

/// The queued-acquire reply. It says three things on purpose: where the caller
/// is, that it must NOT poll, and what will actually wake it — a worker told
/// only "queued" reliably invents a sleep loop, which is the #590 deadlock
/// (the grant notice is typed into the pane, and a pane blocked mid-turn
/// cannot take delivery of it).
fn queued_text(name: &str, position: usize, wait_minutes: u64, repeat: bool) -> String {
    let lead = if repeat {
        format!("you are ALREADY queued for '{name}' — your original place is kept")
    } else {
        format!("'{name}' is busy — you are queued for it")
    };
    format!(
        "{lead}, position {position}. Do NOT wait, sleep, or re-poll: END YOUR TURN. loomux types \
         an [orrerix] notice into this pane the moment the lock is yours. Your place is kept for \
         {wait_minutes} min, after which the request is dropped and you are told so."
    )
}

/// Should prompt delivery keep holding for the human to stop typing? (#43,
/// option A). Returns true to keep waiting, false to proceed. Pure so the
/// hold/deadline decision is unit-testable without a live PTY.
///
/// - `last_input_ms` is the pane's last-keystroke time (0 = none recorded).
/// - `held` is how long THIS hold has already waited; once it reaches
///   `max_hold` we deliver anyway so a long compose session can't starve the
///   report queue.
fn should_hold_for_user(
    last_input_ms: u64,
    now_ms: u64,
    held: Duration,
    quiet_window: Duration,
    max_hold: Duration,
) -> bool {
    if held >= max_hold {
        return false; // cap reached — deliver anyway
    }
    if last_input_ms == 0 {
        return false; // nobody has typed in this pane
    }
    let since = now_ms.saturating_sub(last_input_ms);
    since < quiet_window.as_millis() as u64
}

/// Poll-and-hold loop that drives `should_hold_for_user`: block while
/// `last_input_ms()` reports recent keystrokes, until quiet or the hold hits
/// `max_hold`. Returns `Some(held_ms)` when it actually waited (so the caller
/// can audit the held duration), `None` when it was already quiet on entry.
///
/// Generic over the keystroke source and timings so the wiring — that the
/// loop consults the decision every `poll` and honours the starvation cap —
/// is integration-testable without a live PTY (see the #40 twice-bitten
/// lesson: the pure decision alone isn't enough; the loop that calls it must
/// be exercised too).
#[doc(hidden)] // pub for integration tests
pub fn hold_until_quiet<F: Fn() -> u64>(
    last_input_ms: F,
    quiet_window: Duration,
    max_hold: Duration,
    poll: Duration,
) -> Option<u64> {
    let start = std::time::Instant::now();
    let mut held = false;
    while should_hold_for_user(last_input_ms(), now_ms(), start.elapsed(), quiet_window, max_hold) {
        held = true;
        std::thread::sleep(poll);
    }
    held.then(|| start.elapsed().as_millis() as u64)
}

/// Production wrapper: hold delivery to `pty_id` while its human is typing,
/// using the shipped window/cap/poll timings.
fn wait_for_user_quiet(ptys: &crate::pty::PtyManager, pty_id: u32) -> Option<u64> {
    hold_until_quiet(
        || ptys.last_user_input_ms(pty_id).unwrap_or(0),
        USER_QUIET_HOLD,
        USER_QUIET_MAX_HOLD,
        USER_QUIET_POLL,
    )
}

/// UUIDv4-format session id from the same entropy source as `new_token`
/// (Claude's `--session-id` requires a valid UUID).
fn new_session_uuid() -> String {
    let hex = new_token(); // 32 hex chars
    let b = hex.as_bytes();
    let s = |r: std::ops::Range<usize>| std::str::from_utf8(&b[r]).unwrap();
    // Stamp version (4) and variant (8) nibbles per RFC 4122.
    format!(
        "{}-{}-4{}-8{}-{}",
        s(0..8),
        s(8..12),
        s(13..16),
        s(17..20),
        s(20..32)
    )
}

/// Session ids get interpolated into a shell command line; validate (not
/// filter — a mangled id would silently resume the wrong session).
///
/// **The alphabet is ASCII alphanumerics plus `-` and `_`, and the widening to
/// reach that is deliberate (#722).** It used to be hex digits and `-`, which
/// is exactly a Claude UUID and nothing else — so every opencode id was
/// rejected outright: opencode mints `ses_` + 12 hex + 14 base62
/// (`ses_03bd2d53dffeiBvu9PvuCPjxT7`, `SOURCE`, `id.ts`), whose `_` and
/// mixed-case letters both fell outside. `spawn_agent(resume_session = <an
/// opencode id>)` failed as "invalid resume session id" with nothing malformed
/// about the id.
///
/// **The size of the widening, stated accurately:** the alphabet goes from 23
/// characters (`0-9a-fA-F` plus `-`) to 64 (`0-9A-Za-z` plus `-` and `_`), so
/// **41** characters were added — the non-hex ASCII letters, and `_`. It is
/// emphatically not "two more characters"; an earlier revision of this comment
/// said so and was wrong, which is worth not repeating in a validator whose
/// whole job is to bound what may reach a path join.
///
/// What the widening does *not* admit is the property that matters, and it is
/// unchanged: no path separator, no `.` (so `.`/`..` cannot be spelled), no
/// whitespace, no quote, no shell or PowerShell metacharacter, no NUL. Every
/// one of the 41 added characters is an ASCII letter, inert in a path
/// component and inert on a command line, so everything downstream that treats
/// a session id as a path component (the `Path::join` in
/// `read_session_transcript_events`) or interpolates it into a command line
/// keeps every guarantee it had. Same deliberate-widening shape as
/// `sanitize_model`'s `/`, and pinned the same way.
///
/// **The rules now live in `loomux_engine::pathseg` (#925), and two arrive with
/// them.** This was one of four near-identical copies of the same check; the
/// weakest of the four (`digest::is_safe_session_id`, which this comment used to
/// name as a downstream guarantee) was the one actually guarding the copilot
/// digest's `Path::join`, and it is gone. Consolidating adds two rules here that
/// were not written above: a **leading `-`** is refused (an id is interpolated
/// into a command line, where `-foo` is an option), and a **Windows reserved
/// device name** is refused. Neither can occur in a real session id — Claude
/// mints hyphenated hex UUIDs, opencode mints `ses_…`, both far longer than any
/// device name and neither starting with `-` — so the widening story above is
/// untouched and nothing real is newly rejected.
///
/// **This gate is global, not per-CLI, and that is a deliberate trade
/// (rev-306 NB2).** A malformed *claude* id — one carrying `g`-`z` — now gets
/// past this door and fails later, inside the CLI, instead of failing here.
/// Making the alphabet per-CLI is entirely possible (both callers have `cli`
/// in scope), and was rejected: this function is a **safety** gate answering
/// "can this string escape a path or an argument", not an **authenticity**
/// gate answering "is this a well-formed id for this vendor". It never
/// answered the second question anyway — `aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa`
/// names no session and passed cleanly before this change too — so per-CLI
/// shapes would buy an earlier error only for the narrow "typo that happens to
/// contain a non-hex letter" case, while adding a vendor-shape table whose
/// failure mode is refusing a *valid* session the day a vendor changes its id
/// format. That is precisely the bug this slice just fixed for opencode, and
/// it is not one to reintroduce for the others. Shape questions are answered
/// by `is_full_session_id` and roster resolution, where being wrong yields a
/// diagnosable "unknown session" rather than a flat refusal.
fn sanitize_session(s: &str) -> Option<String> {
    // The `trim()` is this function's own pre-existing contract, kept
    // deliberately across the #925 consolidation; only the checks are shared.
    let t = s.trim();
    loomux_engine::pathseg::check_segment(t)
        .ok()
        .map(|()| t.to_string())
}

/// A Claude Code session id's full length: `8-4-4-4-12` hex hyphenated (see
/// `new_session_uuid`). Below this, `resolve_session_ref` treats the input as a
/// truncated prefix rather than a (possibly external/unrecorded) full id.
const FULL_SESSION_ID_LEN: usize = 36;

/// Is `s` already a complete session id for some CLI loomux supports, as
/// opposed to a prefix of one?
///
/// The distinction decides whether [`resolve_session_ref`] passes an input
/// through untouched or insists on resolving it against this group's roster.
/// Length alone answered that while claude was the only CLI minting ids
/// loomux had to recognize; an opencode id is 30 characters, so length alone
/// would call every complete one a prefix and reject any that this group's
/// roster happened not to record — a session from another group's audit log, a
/// roster that lost the entry — as "unknown session", where the equivalent
/// claude id passes through. Two shapes, one question.
fn is_full_session_id(s: &str) -> bool {
    s.len() >= FULL_SESSION_ID_LEN || is_opencode_session_id(s)
}

/// Resolve a caller-supplied `resume_session` value to the one full session id
/// it names (#190). A hand-copied or logged session id is naturally truncated
/// (8 hex chars is what humans and terminals show), and Claude Code session ids
/// are full UUIDs — before this, a truncated id just failed to resolve with no
/// indication of why. An exact match against this group's roster wins outright,
/// whatever its length. Otherwise, an input that is already a complete id for
/// some supported CLI ([`is_full_session_id`] — length for claude, shape for
/// opencode) is passed through unchanged — it may be a genuine session this
/// group never recorded (a resume with an explicit `kind`/`block` has always
/// allowed that; #190 is only about *truncated* ids, which can never be "the
/// real thing" on their own). Only a shorter, shapeless input is treated as a
/// prefix to resolve: zero
/// matches is a plain "unknown session" (never seen it, in full or part), two
/// or more is "ambiguous" and lists every candidate so the caller can pick —
/// this must never silently choose one.
///
/// `records` is always this caller's OWN group roster (`merged_records(caller.group)`)
/// — a prefix is matched only against sessions this group already knows about, so
/// it can never resolve to (or even see) another group's session, and nothing here
/// touches the filesystem, so there is no path-traversal surface (CLAUDE.md #6).
fn resolve_session_ref(records: &[AgentRecord], input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("resume_session must not be empty".into());
    }
    if records.iter().any(|r| r.session.as_deref() == Some(input)) {
        return Ok(input.to_string());
    }
    if is_full_session_id(input) {
        return Ok(input.to_string());
    }
    let mut matches: Vec<&str> =
        records.iter().filter_map(|r| r.session.as_deref()).filter(|s| s.starts_with(input)).collect();
    matches.sort_unstable();
    matches.dedup();
    match matches.as_slice() {
        // Tagged (#412c) so a caller — an orchestrator's `spawn_agent(resume_session)`
        // included — can tell "never seen it" from "seen it more than once" from
        // "resolvable to a place that's since vanished" (`resolve_resume_cwd`, below)
        // programmatically, without parsing prose.
        [] => Err(format!(
            "resume-not-found: unknown session {input:?} — no session in this group's roster \
             matches that id or prefix"
        )),
        [one] => Ok(one.to_string()),
        many => Err(format!(
            "resume-ambiguous: ambiguous session prefix {input:?} — matches {} sessions, resolve \
             with a longer prefix or the full id: {}",
            many.len(),
            many.join(", "),
        )),
    }
}

/// Resolve the cwd a worker/reviewer resume should launch from, authoritatively
/// from the CLI's OWN session store (#412) — never from a cached copy alone.
/// Claude Code's `--resume <id>` only searches the launch cwd's project
/// directory and its live git worktrees (cli-reference: "passing a session ID
/// searches only the current project directory and its git worktrees"), so a
/// worktree that moved or was deleted since the session ran makes the OLD cwd
/// wrong — and launching from it anyway either hard-fails inside the pane (if
/// the directory is gone) or, worse, silently launches from some OTHER
/// default (the group's main clone) whose project store never contained this
/// session, which is exactly how "the session is plainly in the CLI's own
/// history, but resume says it can't find it" happens. See
/// `sessions::find_session_cwd` for the store lookup itself.
///
/// Tagged, distinguishable errors (#412c) so a caller can tell "resolvable,
/// but its home is gone" (`resume-workspace-missing`) from "never existed
/// here" (`resume-not-found`) from "couldn't even check"
/// (`resume-store-unreadable`), and offer — or automate — a fresh start
/// instead of stranding the caller with an opaque string. "Ambiguous" doesn't
/// arise at this layer: a session id names at most one file in the store, by
/// construction — that outcome is scoped to `resolve_session_ref`'s prefix
/// matching, above.
pub(crate) fn resolve_resume_cwd(
    cli: &str,
    session_id: &str,
    opencode_db: Option<&Path>,
    pi_sessions: Option<&Path>,
) -> Result<String, String> {
    match session_cwd_in_store(cli, session_id, opencode_db, pi_sessions) {
        Ok(Some(cwd)) if Path::new(&cwd).is_dir() => Ok(cwd),
        // Found the session, but its record carries no cwd at all (a session
        // whose first ≤60 lines never mention one) — distinct from "not
        // found": the session exists, its workspace is merely unknown, which
        // is closer to "gone" than to "never existed here" (#412 review N6).
        Ok(Some(cwd)) if cwd.is_empty() => Err(format!(
            "resume-workspace-missing: session {session_id} is recorded in the {cli} session \
             history, but it recorded no working directory — there is nowhere to resume it from. \
             Start fresh instead of resuming."
        )),
        Ok(Some(cwd)) => Err(format!(
            "resume-workspace-missing: session {session_id} is recorded in the {cli} session \
             history under {cwd:?}, but that directory no longer exists on disk — the worktree \
             or workspace may have been removed. Start fresh instead of resuming."
        )),
        Ok(None) => Err(format!(
            "resume-not-found: session {session_id} was not found in the {cli} session history \
             on this machine — it may have been cleared, or the record is stale."
        )),
        Err(e) => Err(format!("resume-store-unreadable: could not read the {cli} session store: {e}")),
    }
}

/// Where a session recorded that it ran, read from the CLI's own store (#722).
///
/// This exists because `sessions::find_session_cwd` answers for exactly two
/// CLIs and sends *everything else* down its claude arm — so before this, an
/// opencode resume searched `~/.claude/projects`, found nothing, and told the
/// caller, in those words, that the session "was not found in the opencode
/// session history on this machine". Not a cosmetic wrong: `register_group_pane`
/// hard-fails a resume on that answer, which made an opencode group
/// unresumable outright.
///
/// opencode's and pi's answers come from **this group's** store rather than a
/// global one, because that is where a group's panes write: `OPENCODE_DB`
/// points each opencode pane at `opencode_db_path(group)` and `--session-dir`
/// points each pi pane at `pi_sessions_dir(group)` (`group_local_session_store`
/// is the predicate). `None` for either path (a caller with no group in hand)
/// is "not found", never a fall-through to another CLI's store.
///
/// `Absent` maps to `Ok(None)` deliberately — a group whose store was never
/// created has no such session, which is exactly "not found in the history",
/// not a store failure the caller should be told to go investigate.
#[doc(hidden)] // pub for integration tests
pub fn session_cwd_in_store(
    cli: &str,
    session_id: &str,
    opencode_db: Option<&Path>,
    pi_sessions: Option<&Path>,
) -> Result<Option<String>, String> {
    if cli == "pi" {
        // `None` (a caller with no group in hand) is "not found", never a
        // fall-through to another CLI's store — the same rule opencode's
        // branch below states, and the reason both are stated is that the
        // fall-through is exactly the defect this function was written for.
        let Some(dir) = pi_sessions else {
            return Ok(None);
        };
        return pi_session_cwd_in_dir(dir, session_id);
    }
    if cli != "opencode" {
        return crate::sessions::find_session_cwd(cli, session_id);
    }
    let Some(db) = opencode_db else {
        return Ok(None);
    };
    match crate::opencodedb::session_directory(db, session_id) {
        Ok(dir) => Ok(dir),
        Err(crate::opencodedb::Unavailable::Absent) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Store membership for a WHOLE listing, enumerated at most once per store
/// (#1592).
///
/// [`session_cwd_in_store`] above answers "is this one id in this one store?",
/// which is the right shape for a resume — one click, one id. A LISTING asks it
/// once per group, and the file-backed stores answer it by enumerating
/// themselves: claude probes `<id>.jsonl` in every project directory, and
/// copilot, which has no filename-is-the-id shortcut, parses every session
/// directory's `workspace.yaml`. Both stop early on a HIT and pay the full
/// enumeration on a MISS — and a stale group, the one that misses, is exactly
/// what accumulates in a long history. So the listing's cost was
/// O(groups × store), and on the install #1592 was reported from that is
/// hundreds of groups against a store of ~1000 sessions.
///
/// This makes it O(store + groups): each store is enumerated the first time a
/// group asks for it and never again within the same listing.
///
/// **Where the trade actually turns, stated rather than glossed.** The
/// per-group lookup stops at the first HIT, so for ONE group whose session sits
/// early in claude's projects root it can finish in a handful of `stat` calls,
/// where this always walks the whole store once. The index is therefore MORE
/// work in exactly one case — a single group that hits early — and less from
/// two groups, or from any single MISS, which already costs the full
/// enumeration. That is the right side to be wrong on for a LISTING: its
/// premise is many groups, a stale group is precisely the one that misses, and
/// #1592 was reported from an install with hundreds of them. It is also off the
/// webview thread and coalesced by the sidebar's `RefreshGate`, so the walk it
/// does pay cannot block the UI or stack up.
///
/// **Lazy per store, on purpose.** A root holding only claude groups must not
/// pay for copilot's enumeration, and vice versa — which is also why the two
/// halves are separate fields rather than one merged set. `None` is "not asked
/// yet"; an empty set is "asked, and the store held nothing".
///
/// **Membership is the whole question here.** `resumable` is
/// `matches!(…, Ok(Some(_)))` — it never reads the cwd, only whether the store
/// has the id — so the three ways `session_cwd_in_store` can answer "no"
/// (`Ok(None)`, an unreadable root's `Err`, and a root that does not exist)
/// all collapse to `false` here exactly as they did through `matches!`.
///
/// **Not for a resume.** A resume needs the recorded cwd to launch in, and this
/// deliberately does not read one; `resume_recorded_session` still goes through
/// `session_cwd_in_store`, so the listing and the resume keep asking one
/// question each rather than sharing a weakened one.
#[derive(Default)]
struct StoreIndex {
    claude: Option<HashSet<String>>,
    copilot: Option<HashSet<String>>,
    /// #2515 C1. A third field rather than a merged set, for the reason the
    /// first two are separate: a root holding only claude groups must not pay
    /// for a walk of the human's whole codex history, which is three directory
    /// levels deep and the most expensive of the three enumerations.
    codex: Option<HashSet<String>>,
}

impl StoreIndex {
    /// Whether `cli`'s store holds `session_id` — the same answer
    /// `matches!(session_cwd_in_store(cli, session_id, _, _), Ok(Some(_)))`
    /// gives for every cli whose store is NOT group-local, including the
    /// default-arm CLIs `find_session_cwd` routes to claude.
    ///
    /// A group-local store (`group_local_session_store`: opencode, pi) is
    /// `false` here rather than searched, and the guard below is what keeps
    /// that sentence true. Its caller already routes those elsewhere, so this
    /// is belt-and-braces — kept because the invariant must live at the site
    /// that depends on it: without it, a future caller reaching here with a
    /// group-local CLI would silently be answered from CLAUDE's projects
    /// directory (the `else` branch below), which is precisely the
    /// wrong-store fall-through `session_cwd_in_store` exists to have ended.
    fn contains(&mut self, cli: &str, session_id: &str) -> bool {
        if group_local_session_store(cli) {
            return false;
        }
        // The same admission `find_session_cwd` applies before it will touch a
        // store at all (#925): an id that is not a single path component is
        // `Ok(None)` there, so it is `false` here. Kept rather than left to the
        // set lookup, so the two agree by construction and not by the accident
        // that no real file is named `../x`.
        let Ok(seg) = PathSegment::parse(session_id) else { return false };
        // A `match` with explicit arms, not an `if`/`else` (#2515 C1). The
        // `else` used to read CLAUDE's projects directory for every CLI that
        // was not copilot, which was right only while claude was the only
        // OTHER non-group-local store — and it stopped being right the moment
        // codex arrived with a store of its own. A codex id looked up in
        // claude's projects root is a MISS, and a miss here renders as
        // `resumable: false`: the Resume affordance silently is not offered,
        // and nothing says why. That is the mistype shape #2515's per-CLI
        // sweep classifies this site under, and the fix is to stop having a
        // default that names a store.
        //
        // The `_` arm is still claude, deliberately: `find_session_cwd` routes
        // an unknown CLI to claude's store too (its own default arm), and this
        // function's doc promises the same answer that function gives. What
        // changed is that claude is now the answer for the CLIs that have no
        // store of their own, rather than for everything that is not copilot.
        let set = match cli {
            "copilot" => self.copilot.get_or_insert_with(|| {
                crate::sessions::copilot_session_state_root()
                    .map(|root| crate::sessions::copilot_session_ids(&root))
                    .unwrap_or_default()
            }),
            "codex" => self.codex.get_or_insert_with(|| {
                crate::sessions::codex_sessions_root()
                    .map(|root| crate::sessions::codex_session_ids(&root))
                    .unwrap_or_default()
            }),
            _ => self.claude.get_or_insert_with(|| {
                crate::sessions::claude_projects_root()
                    .map(|root| crate::sessions::claude_session_ids(&root))
                    .unwrap_or_default()
            }),
        };
        set.contains(seg.as_str())
    }
}

/// Resolve a worker/reviewer resume's launch cwd: prefer a still-valid
/// caller/roster-supplied cwd when there is one (the common case — nothing
/// moved since the session ran, so this is a cheap no-op), falling back to
/// `resolve_resume_cwd`'s store lookup when it's missing, empty, or points at
/// a directory that's gone (#412).
///
/// `group_repo` is the group's main clone. The pre-existing roster fast path
/// already tolerated a recorded cwd equal to it (a worker/reviewer spawned
/// directly through the registry API with `use_worktree: false`, bypassing
/// the MCP-layer guardrail — several tests rely on exactly this) — that gap
/// predates this PR and is left alone. What must NOT happen is the NEW store
/// fallback resolving into the main clone on its own: a store-only match is
/// weaker evidence (it's a fallback specifically because nothing else is
/// known) and accepting `group_repo` there would let the fallback quietly
/// recreate the "resume into the human's own clone" failure #338/#359 exist
/// to prevent (#412 review N3). So the rejection applies ONLY to the store
/// result. Callers must reach this ONLY for a role `needs_dedicated_workspace`
/// — an orchestrator/planner resume has no such restriction and must not call
/// this at all.
pub(crate) fn resolve_worker_resume_cwd(
    cli: &str,
    session_id: &str,
    roster_cwd: Option<&str>,
    group_repo: &str,
    opencode_db: Option<&Path>,
    pi_sessions: Option<&Path>,
) -> Result<String, String> {
    if let Some(c) = roster_cwd.filter(|c| !c.trim().is_empty() && Path::new(c).is_dir()) {
        return Ok(c.to_string());
    }
    let cwd = resolve_resume_cwd(cli, session_id, opencode_db, pi_sessions)?;
    if Path::new(&cwd) == Path::new(group_repo) {
        return Err(format!(
            "resume-workspace-missing: session {session_id}'s only recorded workspace is the \
             group's main clone ({group_repo}) — a dedicated-workspace resume must never launch \
             there (#338/#359). Start fresh to cut a new worktree instead."
        ));
    }
    Ok(cwd)
}

/// Whether this build's host uses Windows path semantics. A `const` from
/// `cfg!` rather than `#[cfg]`-duplicated function bodies, so the rules below
/// stay one readable pair of branches — and, more importantly, so
/// [`normalize_path_key_for`] / [`same_path_key_for`] can be driven with BOTH
/// values from a test on ANY platform. A `#[cfg]`-split pair would compile only
/// its host's half, leaving the other half untested everywhere it matters.
const HOST_IS_WINDOWS: bool = cfg!(windows);

/// Normalize a path for use as (or comparison against) a copilot
/// `permissions-config.json` location key, under `windows` path semantics.
///
/// **Platform-correct, not Windows-shaped (#803 review B1).** loomux ships
/// macOS and Linux builds as well as Windows, and the previous unconditional
/// `'/' -> '\\'` rewrite was actively destructive off Windows: it turned
/// `/home/u/repo` into `\home\u\repo`, a key copilot would never match while
/// looking up `/home/u/repo` — a permission write that silently does nothing,
/// which is the exact failure class #802 is about.
///
/// - **Separators.** On Windows both `/` and `\` are separators, so `/` folds
///   to `\`. On every other platform `\` is a legal *filename character*, so
///   rewriting it would corrupt a real path rather than normalize one.
/// - **Trailing separator.** Stripped either way — `…/repo` and `…/repo/` name
///   one directory. Never stripped down to nothing: a bare root (`/`, or a
///   Windows `C:\`) keeps its last separator rather than becoming `""`.
#[doc(hidden)] // pub for integration tests: BOTH platform shapes, on any host
pub fn normalize_path_key_for(s: &str, windows: bool) -> String {
    let sep = if windows { '\\' } else { '/' };
    let swapped = if windows { s.replace('/', "\\") } else { s.to_string() };
    let trimmed = swapped.trim_end_matches(sep);
    // `"/"` / `"C:\"` trim to `""`; keep one separator instead of an empty key.
    if trimmed.is_empty() { swapped } else { trimmed.to_string() }
}

/// Whether two paths name the same location under `windows` path semantics.
///
/// Case folding is **Windows-only**, per the copilot configuration-directory
/// reference on this very field: the CLI *"compares paths case-insensitively on
/// Windows, and compares paths **case-sensitively on other platforms**"*. Case
/// folding everywhere would merge `/srv/App` and `/srv/app` — two distinct
/// directories on Linux — into one entry, so loomux would write its grant under
/// whichever spelling it saw first and copilot would fail to match the other.
#[doc(hidden)] // pub for integration tests: BOTH platform shapes, on any host
pub fn same_path_key_for(a: &str, b: &str, windows: bool) -> bool {
    let norm = |s: &str| {
        let n = normalize_path_key_for(s, windows);
        if windows { n.to_lowercase() } else { n }
    };
    norm(a) == norm(b)
}

/// [`normalize_path_key_for`] at this host's semantics.
fn normalize_path_key(s: &str) -> String {
    normalize_path_key_for(s, HOST_IS_WINDOWS)
}

/// [`same_path_key_for`] at this host's semantics.
fn same_path_key(a: &str, b: &str) -> bool {
    same_path_key_for(a, b, HOST_IS_WINDOWS)
}

/// Size cap after which the audit log rolls over to `audit.1.jsonl` (one
/// generation kept). Full prompt texts land in the audit, so it grows fast.
const AUDIT_ROTATE_BYTES: u64 = 8 * 1024 * 1024;

/// Serializes every in-process audit writer — appends *and* rotation — against
/// each other (#240). Two guarantees hang off it: no thread holds an append
/// handle across another thread's rotation rename, and two threads can't both
/// decide to rotate (the second rename would discard the generation the first
/// just created). Uncontended in practice — an append is a few hundred bytes
/// every few seconds — and held only for the open+write, never across
/// orchestration work, so it can't meaningfully block a pane. `lock_safe`
/// keeps a poisoned lock from turning best-effort auditing into a panic
/// cascade (see `obs::LockExt`).
static AUDIT_LOCK: std::sync::OnceLock<TrackedMutex<()>> = std::sync::OnceLock::new();

/// [`AUDIT_LOCK`], initialised on first use.
///
/// A getter rather than a `static TrackedMutex` because registering a lock with
/// the watchdog is not a `const` operation — and it must be registered, or the
/// one lock every refusal path takes under three other registry locks would be
/// the one lock a hold report cannot name. Same `OnceLock::get_or_init` shape
/// `obs::data_root` uses.
fn audit_lock() -> &'static TrackedMutex<()> {
    AUDIT_LOCK.get_or_init(|| TrackedMutex::new_ranked("audit", lockorder::AUDIT, ()))
}

thread_local! {
    /// Test-only seam (#240): how long *this thread* pauses between rotation's
    /// size check and its rename. Rotation is check-then-rename, and the window
    /// between the two is a few instructions wide — too narrow for a test to
    /// force a second rotator into it, which is why the lock's rotation-race
    /// protection would otherwise ship unverified. Widening the window on demand
    /// makes the race a real reproducer (see
    /// `concurrent_rotations_keep_the_retained_generation`).
    ///
    /// Zero in production, and read only when a rotation actually fires (an 8 MB
    /// rollover), so the production path pays one thread-local read per rollover
    /// and nothing else. Thread-local rather than a global so it can't leak into
    /// the other tests cargo runs in parallel in this process. Mirrors the
    /// existing `set_claude_projects_dir` test seam.
    static ROTATE_CHECK_PAUSE: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

/// Widen this thread's rotation check-to-rename window. Test-only (see
/// `ROTATE_CHECK_PAUSE`); production never calls it, so the window stays as
/// narrow as the code makes it.
#[doc(hidden)] // pub for integration tests
pub fn set_rotate_check_pause_for_test(pause: Duration) {
    ROTATE_CHECK_PAUSE.with(|p| p.set(pause));
}

/// Roll `audit.jsonl` over to `audit.1.jsonl` once it exceeds `cap`.
/// Factored out so the threshold behavior is testable with a tiny cap.
#[doc(hidden)] // pub for integration tests
pub fn rotate_audit_if_needed(dir: &Path, cap: u64) {
    let _guard = audit_lock().lock_safe();
    rotate_audit_locked(dir, cap);
}

/// Rotation body. Callers must already hold `AUDIT_LOCK` — `append_audit` takes
/// it once and covers rotate+append with a single acquisition (the lock is not
/// reentrant).
///
/// A *cross-process* writer (the gh/git shims' `>>`) can still open the log a
/// moment before this rename and write through the handle afterwards. That's
/// accepted, not a defect: the handle keeps pointing at the same file, so the
/// line lands at the tail of `audit.1.jsonl` instead of the fresh `audit.jsonl`
/// — never lost, and the viewer reads both generations (`audit_log`). Only its
/// position in the timeline shifts, and only for a record that raced an 8 MB
/// rollover.
fn rotate_audit_locked(dir: &Path, cap: u64) {
    let path = dir.join("audit.jsonl");
    if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > cap {
        // Check-then-rename: the size we just read is only still true because
        // `AUDIT_LOCK` is held. Without it a second rotator could pass this same
        // check, wait out the first one's rename, and then rename the *fresh*
        // log over `audit.1.jsonl` — discarding the generation the first just
        // retained. `ROTATE_CHECK_PAUSE` (zero outside tests) widens exactly
        // this window so that race can be reproduced rather than argued.
        let pause = ROTATE_CHECK_PAUSE.with(|p| p.get());
        if !pause.is_zero() {
            std::thread::sleep(pause);
        }
        let _ = fs::rename(&path, dir.join("audit.1.jsonl")); // replaces the old generation
    }
}

/// Append `bytes` to an append-only durable file and **fsync it** (#547).
///
/// Two differences from `append_audit`, both deliberate:
///
/// - **It fsyncs.** The audit log is best-effort history; the staged-orphan
///   archive holds the only remaining copy of payloads that were just removed
///   from `queue.json`, so it has to be at least as durable as the snapshot
///   it took them out of — which `atomic_write` fsyncs.
/// - **It reports failure** instead of swallowing it. A failed append means
///   the caller must NOT go on to remove the entries from staging; see
///   `archive_staged_overflow`'s ordering argument.
///
/// The single-`write_all` rule is `append_audit`'s rule 1 and applies here for
/// the same reason: append atomicity is per syscall, so a batch of records has
/// to reach the OS as one buffer or a concurrent writer can be scheduled into
/// the middle of it. The caller assembles the whole batch.
fn append_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Audit-log writer usable from background threads (delivery outcomes)
/// without holding a registry reference.
///
/// Appends are atomic *per record*, which the sibling `atomic_write` does not
/// give you — that one makes whole-file *replaces* crash-safe (#133), a
/// different failure mode. Two rules keep a record whole (#240):
///
/// 1. **One buffer, one `write_all`.** The record and its newline are serialized
///    up front and handed to the OS in a single call. Append-mode atomicity is
///    per write *syscall*, so a record emitted as many writes is a record other
///    writers can be scheduled into the middle of. The old code wrote
///    `writeln!(f, "{line}")` with `line` a `serde_json::Value`: `Display` walks
///    the tree and emits a write per token, and concurrent writers (mass
///    agent-exit at shutdown, delivery threads) spliced each other character by
///    character — real logs ended up with
///    `{{""actionaction""::""agent-exitagent-exit""`.
///
///    Precisely: `write_all` *loops* on a short write, and each iteration is its
///    own append — so the atomicity rests on the file not short-writing, not on
///    a contract. For a regular file on our baselines (Windows, Linux) a
///    blocking write of a record-sized buffer is issued as one write and returns
///    complete or fails; short writes are a pipe/socket/`ENOSPC` behavior. That
///    is the practice this relies on, and it is worth restating rather than
///    claiming a guarantee the API doesn't make: audit records can be large
///    (full prompt texts land here).
/// 2. **`AUDIT_LOCK` for in-process writers**, so appends don't race rotation.
///
/// The *other* writers are the gh/git shims (`gh_shim_sh`, `git_shim_sh`), in
/// other processes and beyond any mutex of ours. They rely on rule 1 alone, and
/// satisfy it the same way: one `printf` of one whole line, appended with `>>`.
/// Any shim audit line must stay a single `printf`; building a line across two
/// redirections would reintroduce exactly this bug across processes.
///
/// #904: the group id arrives here **already validated** — the parameter is a
/// [`GroupId`], and the path is built by [`group_dir_at`], the one assembly
/// point. There is no check in this function and there is deliberately nothing
/// to check: a caller cannot construct an id that would escape.
///
/// It was worth the type. This function is `root.join(group)` plus
/// `create_dir_all`, so before #904 a `..` component wrote the audit log
/// outside the orchestration root entirely — demonstrated on CI, not
/// theorized. Auditing is best-effort by contract (see the doc's opening
/// line) and returns `()`, so there was nowhere to put a refusal even once one
/// was possible; making the argument unforgeable is what removed the need.
fn append_audit(root: &Path, group: &GroupId, actor: &str, action: &str, detail: Value) {
    let dir = group_dir_at(root, group);
    let record = json!({ "ts_ms": now_ms(), "actor": actor, "action": action, "detail": detail });
    let mut line = record.to_string();
    line.push('\n'); // newline in the same buffer — a separate write could be split off
    // Serialize before taking the lock: JSON formatting is the expensive part
    // and no other writer cares about it.
    let _ = fs::create_dir_all(&dir);
    let _guard = audit_lock().lock_safe(); // covers rotate + append as one unit
    rotate_audit_locked(&dir, AUDIT_ROTATE_BYTES);
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(dir.join("audit.jsonl")) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Directive ledger (#329 expansion): append one line to a pane's ledger
/// file. Same single-`write_all` rule as `append_audit` (rule 1 in its doc)
/// is what makes the append atomic — but unlike `audit.jsonl`, a ledger file
/// has exactly one writer (the owning agent, self-scoped via `note_directive`)
/// and no rotation, so there is no second process or rotate-vs-append race to
/// guard against and no need for `AUDIT_LOCK`-style serialization.
fn append_ledger_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut buf = line.to_string();
    buf.push('\n');
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(&buf.into_bytes())
}

/// The usage series' file name inside a group directory (#2011 slice B).
///
/// A **persisted schema** — `docs/design/token-charts.md` is its contract. It is
/// append-only, has **exactly one writer at a time** (see
/// [`OrchRegistry::series_sample`]: the usage tick, whichever thread is running
/// it, serialized per group by the usage memo cell), is **never rotated** and
/// is **never rebuilt** from `audit.jsonl` or a
/// transcript. Rotation is what the audit log needs and what this file must not
/// have: a rotated series loses the baseline every reader differences against,
/// and the whole point of cumulative rows is that a reader can lose one and
/// still be right.
pub const USAGE_SERIES_FILE: &str = "usage-series.jsonl";

/// The size at which reading `usage-series.jsonl` whole stops being obviously
/// cheap, and the read starts saying so (#2941 review).
///
/// **A revisit trigger, not a limit.** Nothing rotates or compacts this file,
/// so it grows with the calendar: at roughly 288 rows per day per moving key
/// and ~300 bytes a row, a busy group is single-digit MB per month — fine at a
/// 30 s poll — and a group left open for a year is not. The failure mode of
/// leaving that unstated is that nobody can tell when it has arrived, because
/// the read gets gradually slower and never says anything.
///
/// 32 MB is deliberately well past where anything hurts (it is four times the
/// `AUDIT_ROTATE_BYTES` the audit log rotates at) and well short of where a
/// 30 s poll would stall. Crossing it sets `oversize` on the payload; it never
/// shortens the answer. When it does start firing, the fix is one of the two
/// this slice consciously deferred: seek to `since_ms` rather than filter, or
/// compact. See `docs/design/token-charts.md`.
pub const SERIES_REVISIT_BYTES: u64 = 32 * 1024 * 1024;

/// The hard ceiling on a `usage-series.jsonl` read (#3469): four times the
/// revisit trigger above. Unlike that trigger this one **is** a refusal — past
/// it the chart read returns its degrade (`Null`, a skipped tick) and audits
/// `poll-read-failed` rather than buffering the file — because a file four
/// times past "revisit this" is the case where holding it whole on a 30 s poll
/// is the hazard, not the answer. The fail-soft mechanism for every size under
/// it is the fallible reservation in [`loomux_engine::boundedread`].
pub const SERIES_READ_LIMIT_BYTES: u64 = 4 * SERIES_REVISIT_BYTES;

/// Append one row to a group's `usage-series.jsonl`.
///
/// Delegates to [`append_ledger_line`] rather than reimplementing its
/// single-`write_all` append: the properties are the same and the reasons are
/// the same. One writer, no rotation, so no `AUDIT_LOCK`-style serialization —
/// see that function's doc for why that combination is what makes the append
/// atomic without a lock.
fn append_series_line(dir: &Path, row: &usageseries::SeriesRow) -> std::io::Result<()> {
    let line = serde_json::to_string(row)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    append_ledger_line(&dir.join(USAGE_SERIES_FILE), &line)
}

/// One parsed audit-log line, for the in-app timeline viewer. Mirrors the
/// shape written by `append_audit`; `detail` stays an opaque JSON value so the
/// frontend can render per-action without the backend knowing every schema.
#[derive(Clone, Debug, Serialize)]
pub struct AuditEntry {
    pub ts_ms: u64,
    pub actor: String,
    pub action: String,
    pub detail: Value,
}

/// Parse audit JSONL text into entries, in file order (oldest first), skipping
/// malformed lines. Pure so ordering/robustness is testable without touching
/// the filesystem or a registry.
#[doc(hidden)] // pub for integration tests
pub fn parse_audit_lines(text: &str) -> Vec<AuditEntry> {
    parse_audit_lines_counted(text).0
}

/// Same, but also reports how many non-blank lines failed to parse. Skipping
/// silently is how #240 stayed invisible for so long: a corrupt log read as a
/// slightly shorter timeline, with nothing anywhere saying lines had been
/// dropped. Blank lines don't count — a torn tail or a trailing newline is
/// normal; unparseable *content* is not.
#[doc(hidden)] // pub for integration tests
pub fn parse_audit_lines_counted(text: &str) -> (Vec<AuditEntry>, usize) {
    let mut skipped = 0usize;
    let entries = text
        .lines()
        .filter_map(|line| match parse_audit_line(line)? {
            Ok(e) => Some(e),
            Err(()) => {
                skipped += 1;
                None
            }
        })
        .collect();
    (entries, skipped)
}

/// One audit line: `None` for a blank one (not a fault — a trailing newline is
/// normal), `Some(Err(()))` for one that will not parse, which the callers
/// count. Shared by the whole-text parser above and the windowed reader, so
/// the two cannot disagree about what a line means.
fn parse_audit_line(line: &str) -> Option<Result<AuditEntry, ()>> {
    if line.trim().is_empty() {
        return None;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Some(Err(()));
    };
    Some(Ok(AuditEntry {
        ts_ms: v["ts_ms"].as_u64().unwrap_or(0),
        actor: v["actor"].as_str().unwrap_or("").to_string(),
        action: v["action"].as_str().unwrap_or("").to_string(),
        detail: v.get("detail").cloned().unwrap_or(Value::Null),
    }))
}

/// Upper bound on entries returned to the viewer: the audit grows fast (full
/// prompt texts) and only the most recent slice is worth rendering. Keeps the
/// payload bounded even against a rotated + current pair near the 8 MB cap.
#[doc(hidden)] // pub for integration tests
pub const AUDIT_VIEW_LIMIT: usize = 5000;

/// Per-file ceiling on an audit-window read (#3469): four rotations' worth.
/// Rotation keeps each generation near [`AUDIT_ROTATE_BYTES`], so a file past
/// this means rotation itself is broken — and the window read then reports
/// that rather than buffering an unbounded file on a poll path. A **sanity
/// cap**, not the fail-soft mechanism: that is the fallible reservation in
/// [`loomux_engine::boundedread`], which covers every size under it too.
pub const AUDIT_READ_LIMIT_BYTES: u64 = 4 * AUDIT_ROTATE_BYTES;

/// #569: WHY one delivery a pause window lost is gone — the two are not the
/// same event and must not be reported as one (review B2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuppressedCause {
    /// A build predating #569 option 2 destroyed it at the pause branch
    /// (`prompt-suppressed-paused`). Nothing writes that action now, so this
    /// variant is reachable only by pausing under an older loomux, upgrading,
    /// and resuming — a real sequence, and the reason the scan is kept rather
    /// than deleted: those payloads genuinely are gone and this notice is the
    /// only thing that will ever say so.
    LegacyDiscard,
    /// THIS build refused it: the target pane was already at
    /// `queue::QUEUE_MAX_PER_PANE` when it arrived, so `enqueue_text`'s
    /// `RejectFull` arm dropped the payload and returned `Err`
    /// (`delivery-dropped`, `enqueue_reason: group-paused`).
    ///
    /// **The claim this variant exists to stop being false.** Option 2 was
    /// documented — here, in `pause_suppression_notice`, in `resume_group` and
    /// in the design note — as making a pause incapable of destroying a
    /// payload. It is not: the per-pane cap is 8, the orchestrator's pane is
    /// where a whole fleet converges, and loomux's own advisories now queue
    /// there too, so a long pause can fill it and refuse a worker's
    /// `report("done")` ninth. The sender is told (`Err`), but pre-B2 the
    /// ORCHESTRATOR never was, which is the #569 stall arriving through the
    /// queue instead of around it.
    QueueFullDuringPause,
}

impl SuppressedCause {
    /// Stable audit/report token.
    pub fn as_str(self) -> &'static str {
        match self {
            SuppressedCause::LegacyDiscard => "legacy-discard",
            SuppressedCause::QueueFullDuringPause => "queue-full-during-pause",
        }
    }
}

/// #569: one delivery a pause window lost, recovered from the audit log.
///
/// **Why there is no id here.** Neither source has a usable one. The pre-#569
/// pause branch returned *before* the front door, so no id was ever minted;
/// `enqueue_text`'s `RejectFull` arm mints none either, for the reason #563
/// gave when it met the same problem — a rejected entry has nothing to join
/// against. Both are named by `{from, to, preview}`, which the record does
/// establish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuppressedDelivery {
    /// Whoever called `deliver_prompt` — the audit entry's actor.
    pub from: String,
    /// The agent the payload was headed for.
    pub to: String,
    /// One-line, bounded preview of the lost payload
    /// (`queue::dropped_payload_preview`, reused so a lost payload reads the
    /// same wherever it is reported).
    pub preview: String,
    /// Which of the two ways it was lost — see [`SuppressedCause`]. The notice
    /// groups on this, because "an older build threw it away" and "this build
    /// refused it, and would refuse the next one too" call for different
    /// actions from the reader.
    pub cause: SuppressedCause,
}

/// #569: everything ONE pause window swallowed, oldest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PauseSuppression {
    pub items: Vec<SuppressedDelivery>,
    /// Whether the `group-pause` line that OPENED this window was still in the
    /// entries handed to [`suppressed_during_pause`]. False means the scan ran
    /// off the start of a timeline that is both rotated on disk and capped at
    /// `AUDIT_VIEW_LIMIT` by `audit_log`, so the list may reach back into an
    /// EARLIER pause — a caveat the notice states rather than swallows, since
    /// an over-count presented as exact is the same unbacked claim
    /// `.loomux/lessons.md` catalogues.
    pub window_start_seen: bool,
}

/// #569: read a pause window's discarded deliveries out of `entries`
/// (oldest-first audit timeline), scanning BACKWARDS from the end.
///
/// **Two sources, one window** (review B2). `prompt-suppressed-paused` is the
/// legacy discard; `delivery-dropped` carrying `enqueue_reason: group-paused`
/// is THIS build refusing an admission because the target pane was already at
/// `queue::QUEUE_MAX_PER_PANE`. Both destroy a payload inside a pause and
/// neither is otherwise shown to anyone, so both belong in the one notice the
/// resume already sends. The `enqueue_reason` filter is what keeps this to the
/// pause: `delivery-dropped` is written for ordinary queue-full rejections too,
/// and those are the sender's own synchronous `Err` to deal with, not something
/// a resume should re-report.
///
/// **Why `group-pause` alone bounds the window.** A `prompt-suppressed-paused`
/// line can only be written while the group is paused, and a `delivery-dropped`
/// line whose `enqueue_reason` is `group-paused` likewise, so any such line
/// after the most recent `group-pause` belongs to the window that line opened —
/// there is no intervening resume it could have survived. `group-resume` is
/// deliberately NOT a boundary: `create_group` audits that same action name
/// for a group RESTORED from disk (a different event with the same string), so
/// stopping on it would silently truncate a window that spanned an app
/// restart. `group-pause` has no such collision.
///
/// Pure: takes entries, returns a summary, touches no registry state — so the
/// window arithmetic is testable without a paused group or a filesystem.
pub fn suppressed_during_pause(entries: &[AuditEntry]) -> PauseSuppression {
    let mut items = Vec::new();
    let mut window_start_seen = false;
    for e in entries.iter().rev() {
        if e.action == "group-pause" {
            window_start_seen = true;
            break;
        }
        if e.action == "prompt-suppressed-paused" {
            items.push(SuppressedDelivery {
                from: e.actor.clone(),
                to: e.detail["to"].as_str().unwrap_or("?").to_string(),
                preview: queue::dropped_payload_preview(e.detail["text"].as_str().unwrap_or("")),
                cause: SuppressedCause::LegacyDiscard,
            });
            continue;
        }
        // A queue-full refusal of a PAUSE-held admission. `enqueue_text` writes
        // this line with the preview already bounded, so it is taken verbatim
        // rather than re-truncated; `from` is on the detail here (the actor is
        // `loomux`, which did the dropping, not the sender who lost the work).
        if e.action == "delivery-dropped"
            && e.detail["enqueue_reason"] == json!(queue::EnqueueReason::GroupPaused.as_str())
        {
            items.push(SuppressedDelivery {
                from: e.detail["from"].as_str().unwrap_or("?").to_string(),
                to: e.detail["to"].as_str().unwrap_or("?").to_string(),
                preview: e.detail["preview"].as_str().unwrap_or("").to_string(),
                cause: SuppressedCause::QueueFullDuringPause,
            });
        }
    }
    items.reverse(); // scanned newest-first; report in the order they arrived
    PauseSuppression { items, window_start_seen }
}

/// How many discarded deliveries the resume notice names individually before
/// it summarizes the rest. A long pause can swallow an unbounded number of
/// them and this notice is itself a delivery — one pasted into a pane — so the
/// list is capped and the remainder is pointed at the audit log, which holds
/// every payload IN FULL (`prompt-suppressed-paused` carries `text`, not a
/// preview).
pub const PAUSE_SUPPRESSION_LIST_MAX: usize = 8;

/// #569: the resume-time notice naming what a pause LOST — from either cause.
///
/// Reworked by option 2 rather than deleted, and corrected by review B2. The
/// wording has to do two jobs the audit lines cannot:
///
/// - **Say which pause it is talking about.** Both behaviors now coexist in a
///   group's history, and a reader who takes "will not be replayed" as the
///   current rule re-requests work that is already on its way — the duplicate
///   `queued_notice`'s "do NOT re-send" exists to prevent, arriving from the
///   opposite direction.
/// - **Not claim a pause can no longer destroy anything.** It can: a pane at
///   `queue::QUEUE_MAX_PER_PANE` refuses further admissions for as long as the
///   pause lasts, and a refusal is a payload that is gone. That half is not
///   history — it is a live property of this build, and it is the half a reader
///   can still act on, so it is stated in the present tense and its sentence
///   says what to do about it.
///
/// Pure so the copy is unit-testable, matching `queue::dropped_notice` /
/// `delivery_held_detail`.
pub fn pause_suppression_notice(s: &PauseSuppression) -> String {
    let n = s.items.len();
    let refused = s.items.iter().filter(|i| i.cause == SuppressedCause::QueueFullDuringPause).count();
    let legacy = n - refused;
    let (count, verb) = if n == 1 {
        ("1 delivery".to_string(), "was")
    } else {
        (format!("{n} deliveries"), "were")
    };
    let mut out = format!(
        "[orrerix] Group resumed — {count} {verb} LOST while this group was paused. Anything the \
         pause merely HELD is delivering on its own right now; do not re-request that. What is \
         listed below is not held, it is gone."
    );
    // Each cause gets its own sentence, and only the causes actually present:
    // a notice that explains a failure mode this window did not have is one
    // more paragraph between the reader and the one that matters.
    if legacy > 0 {
        out.push_str(
            " Some were DISCARDED by an EARLIER loomux version that did not queue while paused — \
             those are history and cannot recur on this build.",
        );
    }
    if refused > 0 {
        out.push_str(
            " Some were REFUSED by this build because the target pane's delivery queue was \
             already full (8 deep) — that is not history: a pane at capacity keeps refusing for \
             as long as a pause lasts, so if this group is paused again for a long stretch, \
             expect it again.",
        );
    }
    out.push_str(" Whatever you are still waiting on from these has to be re-requested. Lost:");
    for it in s.items.iter().take(PAUSE_SUPPRESSION_LIST_MAX) {
        // The cause is per-item because a single window can mix them, and
        // "which of these can happen to me again" is the reader's next question.
        let why = match it.cause {
            SuppressedCause::LegacyDiscard => "discarded by an earlier loomux",
            SuppressedCause::QueueFullDuringPause => "refused, queue full",
        };
        // #632: `  • ` + the marker, never the old bare `  - `. This notice is
        // an in-band DELIVERY — it rides the pty into an orchestrator's pane —
        // and every row loomux writes there has to be one
        // `mask_loomux_notices` can claim, or the item rows are text ABOUT a
        // question sitting in the tail of the pane most exposed to #576's
        // self-latch. `deframe` strips whitespace and `│ ┃ | * ● • ◆` but NOT
        // `-`, which is exactly why the bullet changed (the #624 convention).
        //
        // The preview is bounded AGENT text, and it is masked here rather than
        // left to latch because this row is loomux's own framing QUOTING a
        // payload — the #576 relay case exactly — not a rendered dialog. It is
        // re-collapsed through `dropped_payload_preview` (idempotent on
        // well-formed input) rather than trusted: it is read back out of a
        // durable `audit.jsonl` that an EARLIER loomux version may have
        // written, and a preview carrying a newline would split this into two
        // rows with only the first marker-led — #632 reintroduced from disk.
        out.push_str(&format!(
            "\n  • {NOTICE_MARKER} {} -> {} ({why}): {}",
            it.from,
            it.to,
            queue::dropped_payload_preview(&it.preview),
        ));
    }
    if n > PAUSE_SUPPRESSION_LIST_MAX {
        out.push_str(&format!(
            "\n  • {NOTICE_MARKER} ...and {} more — every lost payload is in this group's \
             audit log in full (actions `prompt-suppressed-paused` and `delivery-dropped`).",
            n - PAUSE_SUPPRESSION_LIST_MAX
        ));
    }
    if !s.window_start_seen {
        out.push_str(&format!(
            "\n  • {NOTICE_MARKER} (The `group-pause` line that opened this window is no \
             longer in the readable audit log, so this list may reach back into an earlier pause.)"
        ));
    }
    // The door for this producer (#632), the way `OrchNoticeInbox::park` is the
    // door for #624's single-line notices: asserted through the real mask, so a
    // later edit that adds an unmarked row fails where it is introduced rather
    // than in a pane. Debug only — CI's test builds are debug, and a release
    // build must never panic a live session over it; the degraded outcome is a
    // gate that holds too long, which `QuestionStale` already reports.
    debug_assert!(
        unmaskable_framing_rows(&out, &[]).is_empty(),
        "every row of the pause-suppression notice must be maskable (#632) — got leftovers \
         {:?} from {out:?}",
        unmaskable_framing_rows(&out, &[])
    );
    out
}

/// #579: what a front-door refusal was carrying — the two shapes
/// `queue-full-at-call` is written for, which need different advice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusedPayload {
    /// `enqueue_text`'s `RejectFull` arm: a text delivery. Its bytes are
    /// recoverable from the paired `prompt` line — see
    /// [`front_door_refusals`].
    Prompt,
    /// `audit_stranded_push`'s rejection: a `StrandedSubmit` marker, which
    /// never carried text at all (the bytes were already pasted into the
    /// pane; only the Enter was queued). Nothing to re-send — the pane needs
    /// an Enter, or its text re-pasted by hand.
    StrandedSubmit,
}

impl RefusedPayload {
    pub fn as_str(self) -> &'static str {
        match self {
            RefusedPayload::Prompt => "prompt",
            RefusedPayload::StrandedSubmit => "stranded-submit",
        }
    }
}

/// #633: WHY a delivery was refused at the front door — the discriminator that
/// turns [`front_door_refusals`] from a queue-full list into a refusal list.
///
/// **Why this exists at all.** #630 scanned for exactly one `delivery-dropped`
/// reason (`queue-full-at-call`) because it was the only refusal that wrote an
/// audit line. `deliver_prompt`'s two PRE-admission refusals — the target is
/// dead, the target has no terminal bound — wrote nothing, so no derivation
/// could ever surface them; #615 created the second of those by turning a silent
/// `Ok` into a silent `Err`, which is a strictly better contract for the sender
/// and no better at all for anyone reading the log afterwards. A refusal that
/// leaves no record cannot be enumerated by anything, which is the whole #579
/// class. So every refusal now writes a line, each under its own reason string,
/// and this enum is what a reader joins on.
///
/// **Every arm is a real, distinct instruction to whoever reads the row**, which
/// is why this is a typed discriminator rather than the raw string carried
/// through: a queue-full refusal says "the pane is busy, this may be worth
/// re-sending later"; a dead-target refusal says "re-target it, that pane is
/// gone"; a no-terminal refusal says "it was too early, send it again once the
/// pane binds". Collapsing them into one row shape would make the list
/// enumerable and still not actionable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalReason {
    /// `enqueue_text`'s `RejectFull` arm (#563/#579): the target pane's queue
    /// was already at `queue::QUEUE_MAX_PER_PANE`.
    QueueFull,
    /// `deliver_prompt_as` (#633): the target agent's status was
    /// [`AgentStatus::Dead`]. Refused BEFORE admission, so no id was minted and
    /// no queue was touched.
    AgentDead,
    /// `deliver_prompt_as` (#633): the target agent had no `pty_id` bound yet —
    /// a queue is keyed by pane, so there was nowhere to hold the payload.
    /// Created as an `Err` by #615 (it was a silent `Ok` before), audited by
    /// #633.
    NoTerminal,
    /// `withdraw_unprocessable` (#470): the admission succeeded and was then
    /// UNDONE because no `AppHandle` existed to ever drain it. Unreachable in
    /// production — see [`front_door_refusals`] for why it is surfaced anyway.
    NoAppHandle,
    /// `withdraw_unprocessable` (#470), the sibling case: an `AppHandle`
    /// existed but the registry was not held in an `Arc`, so no drainer could
    /// be spawned. Pre-#633 this wrote the `no-app-handle` reason too — one
    /// line claiming a cause that was not the one that fired.
    RegistryNotShared,
    /// `deliver_prompt_as` (#1161 M2): the target was this group's MANAGER and
    /// the delivery was not one of the two the no-injection guarantee permits.
    /// Refused BEFORE admission, like the two above, so no id was minted and no
    /// queue was touched.
    ///
    /// **Unlike every other reason here, this one is a POLICY refusal rather
    /// than a resource one**, and the difference matters to whoever reads the
    /// row: the other five say loomux could not deliver, this one says it will
    /// not. Nothing about the pane changes that — resuming it, freeing its
    /// queue and binding it a terminal all leave the answer the same.
    ManagerPane,
}

impl RefusalReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RefusalReason::QueueFull => "queue-full-at-call",
            RefusalReason::AgentDead => "agent-dead-at-call",
            RefusalReason::NoTerminal => "no-terminal-at-call",
            RefusalReason::NoAppHandle => "no-app-handle",
            RefusalReason::RegistryNotShared => "registry-not-shared",
            RefusalReason::ManagerPane => "manager-pane",
        }
    }

    /// Parse a `delivery-dropped` line's `reason` back into an arm — `None` for
    /// every reason that is NOT a front-door refusal, which is what makes
    /// [`front_door_refusals`]'s filter an enumeration rather than a guess.
    pub fn from_audit(reason: &str) -> Option<Self> {
        match reason {
            "queue-full-at-call" => Some(RefusalReason::QueueFull),
            "agent-dead-at-call" => Some(RefusalReason::AgentDead),
            "no-terminal-at-call" => Some(RefusalReason::NoTerminal),
            "no-app-handle" => Some(RefusalReason::NoAppHandle),
            "registry-not-shared" => Some(RefusalReason::RegistryNotShared),
            "manager-pane" => Some(RefusalReason::ManagerPane),
            _ => None,
        }
    }

    /// What this refusal means for the payload, in the reader's terms — carried
    /// on the audit line itself (`consequence`) so one string serves every
    /// channel, the same discipline the stranded-marker refusal already
    /// follows.
    ///
    /// `None` for [`RefusalReason::QueueFull`], and deliberately: that row
    /// already says what happened in fields the reader has (`queue_depth`,
    /// `enqueue_reason`) and at length in `queue_orphans`' own description, and
    /// its ONE case with a consequence — a refused `StrandedSubmit` marker,
    /// which leaves pasted-but-unsubmitted text in the pane — writes its own
    /// string from `audit_stranded_push` (#579). Restating that prose here
    /// would be a second copy of a sentence already maintained elsewhere, which
    /// is the failure mode `consequence` was introduced to avoid.
    fn consequence(self) -> Option<&'static str> {
        Some(match self {
            RefusalReason::QueueFull => return None,
            RefusalReason::AgentDead => {
                "the target agent was already dead — nothing was queued, and that pane will \
                 never take it; re-target this to a live or resumed agent"
            }
            RefusalReason::NoTerminal => {
                "the target agent had no terminal bound yet, so there was no queue to hold \
                 this — nothing was queued; re-send once the pane binds"
            }
            RefusalReason::NoAppHandle => {
                "loomux had no app handle to process this pane's queue, so the admission was \
                 withdrawn rather than left to strand — nothing is queued"
            }
            RefusalReason::RegistryNotShared => {
                "the registry was not shared, so no drainer could be started — the admission \
                 was withdrawn rather than left to strand; nothing is queued"
            }
            RefusalReason::ManagerPane => {
                "the target is the group's manager — the human's own pane, which takes no \
                 delivery from any agent; nothing was queued and nothing will be. Post status to \
                 message_manager, or put a decision to the human with ask_human"
            }
        })
    }
}

/// #579: one delivery REFUSED at the front door — the target pane's queue was
/// already at `queue::QUEUE_MAX_PER_PANE`, so nothing was ever queued.
///
/// **Why this is a separate type from [`queue::OrphanedQueueEntry`] rather than
/// that struct with an optional id.** A refused delivery never reached
/// `queue_seq.fetch_add`, so it has no id — and `OrphanedQueueEntry.id: u64` is
/// the wire shape of the `queue_orphans` MCP tool as well as the join key both
/// orphan derivations run on (`queue::merge_orphans` dedupes on it, the audit
/// scan opens and closes on it). Widening it to `Option<u64>` would make every
/// existing row's id nullable for the benefit of rows that can never have one,
/// and a synthetic id would be a number that joins against nothing while
/// looking like one that does. So refusals are surfaced as their own list, on
/// their own key — `{from, to, preview}`, the same naming [`SuppressedDelivery`]
/// settled on for the same reason. See `docs/design/orchestration.md`'s
/// "Front-door refusals (#579)".
///
/// The other half of that argument is behavioral: an orphan is a payload
/// loomux still HOLDS (staged in `recovered_queue`, re-admitted the moment its
/// pane rebinds), while a refusal was explicitly declined and the sender told
/// so synchronously. Keeping them in one list would put refusals within reach
/// of `readmit_recovered`, and silently re-admitting a declined delivery later
/// would reorder it against everything the pane accepted in the meantime.
/// Being audit-derived and read-only, this list structurally cannot do that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusedDelivery {
    /// Whoever called `deliver_prompt` — off the audit line's `from` detail,
    /// not its actor (the actor is `loomux`, which did the refusing, not the
    /// sender who lost the work). `"?"` for a pre-#563 line that recorded
    /// only `{to, reason, depth}`.
    pub from: String,
    /// The pane the payload was headed for.
    pub to: String,
    pub refused_ms: u64,
    /// Why loomux refused it (#633) — the discriminator the reader acts on.
    pub reason: RefusalReason,
    /// The pane's queue depth at the moment of refusal — `QUEUE_MAX_PER_PANE`
    /// in every real `queue-full-at-call` case, carried through rather than
    /// assumed.
    ///
    /// `None` (#633) for a refusal that never reached the queue at all: a
    /// dead-target or no-terminal refusal returns before admission, so there is
    /// no depth to report and reporting `0` would be a measurement nobody took
    /// dressed as one that says the pane was empty.
    pub depth: Option<usize>,
    /// Which [`EnqueueReason`](queue::EnqueueReason) the refused admission was
    /// made under, when the line recorded one. `None` for a marker refusal
    /// (never admitted under a reason) and for a pre-#563 line.
    pub enqueue_reason: Option<String>,
    pub payload: RefusedPayload,
    /// `text.len()` as the refusal recorded it, so the true size is known even
    /// when the bytes are not recoverable. `None` on a pre-#563 line.
    pub bytes: Option<usize>,
    /// The bounded one-line preview the refusal line carries
    /// (`queue::dropped_payload_preview`) — empty for a marker refusal and a
    /// pre-#563 line.
    pub preview: String,
    /// The full payload, recovered from the paired `prompt` audit line and
    /// VERIFIED against this refusal's own record — see
    /// [`front_door_refusals`]. `None` when it could not be verified, which is
    /// a different fact from an empty payload and asks for different handling
    /// (re-derive rather than re-send verbatim).
    pub text: Option<String>,
    /// The consequence a marker refusal states in its own audit line, carried
    /// verbatim rather than re-worded here — one string, every channel.
    pub consequence: Option<String>,
}

/// #579: every front-door refusal the readable audit window holds, plus how
/// many there were in total — see [`front_door_refusals`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontDoorRefusals {
    /// Oldest first, capped at [`REFUSED_LIST_MAX`] — the MOST RECENT that
    /// many, since a pane at capacity keeps refusing and the newest refusals
    /// are the ones still likely to matter.
    pub items: Vec<RefusedDelivery>,
    /// How many refusals the scan found, before the cap. Never implied by
    /// `items.len()`: a caller that reports a capped list as complete is the
    /// silent-truncation defect `.loomux/lessons.md` names.
    pub total: usize,
    /// Whether the audit window the scan ran over had itself been cut at
    /// `AUDIT_VIEW_LIMIT` (#579 review NB1) — i.e. whether `total` is a count
    /// of ALL this group's refusals or only of the readable tail.
    ///
    /// **Why this is load-bearing and not a nicety.** `total` and the
    /// `refused_omitted` derived from it are honest about the *list* cap and
    /// silent about the *window* cap, and the two compose badly: a group with
    /// 6000 audit entries and four refusals among the oldest thousand reports
    /// `refused_count: 0, refused_omitted: 0` — which reads as "nothing was
    /// refused", the strongest possible claim, from a scan that never saw the
    /// evidence. One bounded flag turns that into "nothing was refused in what
    /// I could read," which is the true statement and the one the reader can
    /// act on (go read `audit.jsonl`). Same job as #569's
    /// `PauseSuppression::window_start_seen`, in the same lineage, for the same
    /// reason: a scan that ran off the start of its timeline has to say so.
    pub window_truncated: bool,
}

/// #579: how many refusals `queue_orphans` lists individually. Deliberately
/// the same number as [`PAUSE_SUPPRESSION_LIST_MAX`], and for a related but
/// not identical reason: that cap bounds a notice PASTED into a pane, this one
/// bounds a tool result READ INTO an orchestrator's context — and each row here
/// can carry up to `queue::ORPHAN_TEXT_CAP_BYTES` of recovered payload, so an
/// uncapped list is an unbounded read. Unlike the orphan list, which the
/// per-pane cap of 8 already bounds, refusals accumulate without limit: a pane
/// held at capacity refuses every arrival for as long as it stays there.
/// Everything past the cap stays in `audit.jsonl`, and `FrontDoorRefusals::
/// total` says how much was left there.
pub const REFUSED_LIST_MAX: usize = 8;

/// #579: read a group's front-door refusals out of `entries` (an oldest-first
/// audit timeline, as `audit_log` returns them).
///
/// A delivery refused at the cap is the one loss with NO queue entry to its
/// name: `enqueue_text`'s `RejectFull` arm returns before `queue_seq.fetch_add`,
/// so it can never join the id-keyed orphan derivations, which is exactly why
/// #563 split this out (#579) instead of folding it into #572's visibility fix.
/// The audit line is the only record that will ever exist, and since #572 it
/// carries enough to act on: `from`, `bytes` and a bounded `preview`.
///
/// **Every refusal, not just the capped one (#633).** #579 shipped scanning one
/// reason because one reason was all that wrote a line. `deliver_prompt_as`
/// refuses two ways BEFORE admission — the target is dead, the target has no
/// terminal bound — and both wrote nothing at all, so this derivation, the
/// orphan derivations and a human reading `audit.jsonl` were equally blind to
/// them; #615 made the no-terminal case an `Err` instead of a silent `Ok`,
/// which fixed the sender's contract and left the record exactly as empty. Each
/// now writes its own [`RefusalReason`], and the filter above is an enumeration
/// over that enum rather than an equality test against one string.
///
/// **Where the payload comes from differs by reason, and it has to.** A
/// queue-full refusal happens AFTER `deliver_prompt` has audited `prompt` with
/// the full text, so #579 recovers the bytes by pairing (below). The two
/// pre-admission refusals happen BEFORE that line is written — and moving the
/// `prompt` write earlier was rejected, because `prompt` is what the whole
/// suite (and `delivered_texts`) reads as "this was offered to a pane", and a
/// delivery to a dead agent was never offered to anything. So those lines carry
/// their own `text` inline, and this reads it verbatim: it is the same line and
/// the same write, so there is no join to get wrong and nothing to verify
/// against. That is a rare line (a refusal), not a per-delivery cost.
///
/// **The no-app-handle drop is SURFACED, not excluded (#633's other half).**
/// `withdraw_unprocessable`'s undo is unreachable in production — `set_app` and
/// `set_self_arc` both run in `lib.rs`'s `setup` block, before the MCP server
/// thread that is the only way an agent can call `deliver_prompt` at all — and
/// #630 excluded it on exactly that argument, silently. Two things decided it
/// the other way here. First, once the list is reason-discriminated the cost is
/// one enum arm, so the exclusion was buying nothing. Second, "unreachable in
/// production" is a claim about today's startup order that nothing enforces,
/// and the failure mode of it going stale is precisely the #579 class: a loss
/// nothing can enumerate. Surfacing it means a broken assumption shows up as a
/// row instead of as silence. It does not double-report: the withdrawal's own
/// line carries the `id`, which CLOSES that id for
/// `queue::orphaned_queue_entries`, so the entry is gone from the orphan
/// derivation by the time it appears here — one loss, one row, the same rule
/// the `recovered` exclusion below enforces from the other side.
///
/// **Recovering the payload, verified rather than assumed.** `deliver_prompt`
/// audits `prompt` — with the FULL text — immediately before it admits, on both
/// the paused and unpaused paths, so the bytes a refusal lost are still in the
/// log. This pairs a refusal with the most recent `prompt` line from the SAME
/// sender to the SAME target, and accepts it only if BOTH of that line's
/// fingerprints match the refusal's own record: `text.len() == bytes`, and
/// `queue::dropped_payload_preview(text) == preview` recomputed. Two checks
/// rather than positional adjacency, because audit writes from concurrent
/// delivery threads interleave and "the line just before" is not a guarantee.
/// If either check fails the row reports `text: None` and the reader falls back
/// to the preview — the safe direction, since the failure mode of guessing here
/// is handing an orchestrator the wrong bytes to paste into somebody's terminal.
/// The residual it does not close: a second `prompt` line for the same
/// (sender, target) pair, written by another thread inside the window between
/// this delivery's own `prompt` line and its refusal, whose text ALSO matches
/// both fingerprints — i.e. the same payload, or one that agrees on length and
/// on its whitespace-collapsed first `queue::DROPPED_PREVIEW_MAX` chars.
///
/// **`recovered` refusals are deliberately excluded.** A recovery re-admission
/// refused at the cap (`readmit_recovered`, `EnqueueReason::Recovered`) writes
/// this same line, but that entry is put straight back into staging and keeps
/// being reported as an ORPHAN — with its payload, and by an id. Listing it here
/// too would show one lost payload twice in one tool result, and the documented
/// response to both lists is to re-send.
///
/// `window_truncated` is passed IN rather than guessed at from `entries.len()`
/// — see [`OrchRegistry::audit_log_windowed`], which is where the cut happens
/// and therefore the only place that knows. It is a required parameter, not a
/// field a caller fills in afterwards, so a future call site cannot forget it
/// and silently re-introduce a complete-looking count over a partial window
/// (#579 review NB1).
///
/// One audit entry read as a front-door refusal, or `None` if it is not one
/// (#658, extracted from [`front_door_refusals`]).
///
/// **Extracted rather than re-spelled.** #658's drain-time roster
/// ([`refusal_roster`]) needs exactly this classification — which
/// `delivery-dropped` lines are refusals, which reason each carries, which
/// shape (`prompt` vs marker) it is, and which are excluded — over a different
/// window and for a single target. Re-deriving it there would mean two filters
/// that must agree forever about what counts as a refusal, and the failure mode
/// of them drifting is the #579 class again: a loss one channel enumerates and
/// the other silently does not.
///
/// `text` is filled ONLY from an inline `text` field (the #633 pre-admission
/// refusals, which write their own payload because no `prompt` line exists to
/// pair with). Recovering a queue-full refusal's bytes needs the timeline
/// either side of this line, so that stays in `front_door_refusals`, which has
/// it.
///
/// **The two exclusions are here, not at the call sites**, so both consumers
/// inherit them:
/// - An unmodelled `delivery-dropped` reason. `delivery-dropped` is written for
///   reasons that are not front-door refusals at all: `agent-died` and
///   `queue-full` (a whole queue dropped at once) carry an `id` and are already
///   reported by the id-keyed orphan derivations, so listing them here too
///   would show one loss twice in one tool result. Anything
///   [`RefusalReason::from_audit`] does not know is skipped by the same rule —
///   an unmodelled reason is not silently folded into a list whose documented
///   response is "re-send".
/// - A `recovered` re-admission refused at the cap. That entry is put straight
///   back into staging and keeps being reported as an ORPHAN, with its payload
///   and by an id.
fn refusal_row(e: &AuditEntry) -> Option<RefusedDelivery> {
    if e.action != "delivery-dropped" {
        return None;
    }
    let reason = e.detail["reason"].as_str().and_then(RefusalReason::from_audit)?;
    let to = e.detail["to"].as_str().unwrap_or("?").to_string();
    let depth = e.detail["depth"].as_u64().map(|d| d as usize);
    if e.detail["payload"] == json!(RefusedPayload::StrandedSubmit.as_str()) {
        return Some(RefusedDelivery {
            // A marker push is loomux's own act, and its line records no
            // `from` — the actor is the honest answer here, unlike on a
            // prompt refusal where a sender lost the work.
            from: e.actor.clone(),
            to,
            refused_ms: e.ts_ms,
            reason,
            depth,
            enqueue_reason: None,
            payload: RefusedPayload::StrandedSubmit,
            bytes: None,
            preview: String::new(),
            text: None,
            consequence: e.detail["consequence"].as_str().map(str::to_string),
        });
    }
    let enqueue_reason = e.detail["enqueue_reason"].as_str().map(str::to_string);
    if enqueue_reason.as_deref() == Some(queue::EnqueueReason::Recovered.as_str()) {
        return None;
    }
    Some(RefusedDelivery {
        from: e.detail["from"].as_str().unwrap_or("?").to_string(),
        to,
        refused_ms: e.ts_ms,
        reason,
        depth,
        enqueue_reason,
        payload: RefusedPayload::Prompt,
        bytes: e.detail["bytes"].as_u64().map(|b| b as usize),
        preview: e.detail["preview"].as_str().unwrap_or("").to_string(),
        text: e.detail["text"].as_str().map(str::to_string),
        consequence: e.detail["consequence"].as_str().map(str::to_string),
    })
}

/// Pure: entries in, summary out, no registry and no filesystem — the same split
/// [`suppressed_during_pause`] follows, and for the same reason (this reads
/// `AuditEntry`, which lives here rather than in `queue.rs`).
pub fn front_door_refusals(entries: &[AuditEntry], window_truncated: bool) -> FrontDoorRefusals {
    // (sender, target) -> the full text of the last `prompt` line between them.
    let mut offered: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    let mut items: Vec<RefusedDelivery> = Vec::new();
    for e in entries {
        if e.action == "prompt" {
            if let (Some(to), Some(text)) = (e.detail["to"].as_str(), e.detail["text"].as_str()) {
                offered.insert((e.actor.clone(), to.to_string()), text.to_string());
            }
            continue;
        }
        let Some(mut row) = refusal_row(e) else { continue };
        // #633: a pre-admission refusal carries its own payload, because no
        // `prompt` line was ever written for it to pair with — see this
        // function's doc. When the line has one, `refusal_row` has already
        // taken it: same line, same write, nothing to join and so nothing to
        // verify against. Otherwise fall back to #579's verified pairing.
        if row.text.is_none() {
            row.text = row
                .bytes
                .and_then(|bytes| {
                    offered.get(&(row.from.clone(), row.to.clone())).filter(|t| {
                        t.len() == bytes
                            && queue::dropped_payload_preview(t.as_str()) == row.preview
                    })
                })
                .cloned();
        }
        items.push(row);
    }
    let total = items.len();
    if total > REFUSED_LIST_MAX {
        items.drain(..total - REFUSED_LIST_MAX);
    }
    FrontDoorRefusals { items, total, window_truncated }
}

/// #658: the audit action the drain-time refusal roster writes — and the
/// watermark [`refusal_roster`] reads back, which is why it is a constant
/// rather than a literal at each end.
pub const REFUSAL_ROSTER_ACTION: &str = "refusal-roster";

/// #658: the roster's opening sentence, held as a constant because it is load
/// bearing twice over: [`refusal_roster_notice`] writes it, and
/// [`refusal_roster`] recognises a refused roster BY it. Sharing one literal is
/// what makes the second use impossible to drift from the first.
pub const REFUSAL_ROSTER_OPENER: &str =
    "[orrerix] your pane's delivery queue has drained back below its cap.";

/// #658: how many refusals one roster names individually.
///
/// Smaller than [`REFUSED_LIST_MAX`] (8) on purpose, and the difference is the
/// CHANNEL, not the fact. That cap bounds a JSON list an orchestrator pulls
/// deliberately with `queue_orphans`; this one bounds a SINGLE LINE that is
/// either pasted into a pane or ridden back on a tool result — single because
/// [`OrchNoticeInbox::park`] requires it (every row of the relay block has to
/// stay maskable). Four entries at [`ROSTER_PREVIEW_MAX`] each keeps that line
/// in the same order of magnitude as [`NOTICE_AUDIT_TEXT_CAP`], which is what
/// every other loomux notice is sized against. Everything past the cap is
/// counted, said out loud, and still in `audit.jsonl`.
pub const ROSTER_LIST_MAX: usize = 4;

/// #658: how much of each refusal's preview one roster row carries. Tighter
/// than [`queue::DROPPED_PREVIEW_MAX`] (160) because a roster carries up to
/// [`ROSTER_LIST_MAX`] of them on ONE line — see that constant. Long enough
/// that the recipient can tell WHICH delivery it was, which is the whole job:
/// the payload itself is not re-sendable from here and is not offered as if it
/// were (`queue_orphans` has the verified bytes).
pub const ROSTER_PREVIEW_MAX: usize = 80;

/// #658: one refused delivery as the drain-time roster reports it — sender,
/// bounded preview, reason, and whether the sender has since got it through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RosterEntry {
    /// The sender who lost the work — who the recipient has to ask.
    pub from: String,
    pub reason: RefusalReason,
    /// Re-clamped to [`ROSTER_PREVIEW_MAX`] from the refusal line's own
    /// preview, never re-derived from the payload.
    pub preview: String,
    /// **Marked, not suppressed** (#658's own wording). A refusal whose sender
    /// has since re-sent the same payload successfully is still listed —
    /// because the recipient cannot tell from the outside which of its
    /// arriving deliveries was a re-send, and a list that silently omitted them
    /// would read as "these are all still missing" while being short. See
    /// [`refusal_roster`] for what "successfully" is derived from.
    pub resent: bool,
}

/// #658: everything one drain's roster says, before it is worded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusalRoster {
    /// The pane this roster is FOR — the one that refused these deliveries.
    pub to: String,
    /// Oldest first, capped at [`ROSTER_LIST_MAX`] — the most recent that many,
    /// since a pane held at capacity keeps refusing and the newest refusals are
    /// the ones still likely to matter.
    pub items: Vec<RosterEntry>,
    /// Refusals in the window before the cap. Never implied by `items.len()`.
    pub total: usize,
    /// `total - items.len()` — counted and said out loud, never silently cut.
    pub omitted: usize,
    /// The newest `refused_ms` this roster covers, and how many of the covered
    /// refusals share it. Written to the roster's own audit line and read back
    /// as the next roster's start point — see [`refusal_roster`]'s watermark
    /// note for why a bare timestamp is not enough.
    pub through_ms: u64,
    pub at_through: usize,
    /// Whether the audit window this was derived from had itself been cut at
    /// `AUDIT_VIEW_LIMIT` — same job as [`FrontDoorRefusals::window_truncated`],
    /// and said in the notice rather than swallowed.
    pub window_truncated: bool,
}

/// #658: every delivery `to_agent` refused that it has not been told about
/// yet — derived from `entries` (an oldest-first audit timeline) and nothing
/// else.
///
/// **Why the audit log is the record and there is no second bookkeeping
/// structure.** Every front-door refusal already writes a `delivery-dropped`
/// line carrying the four things a roster says (`from`, `to`, `reason`,
/// `preview`) — #563 put the preview there and #633 made every refusal reason
/// write one. A parallel in-memory list of "refusals not yet relayed" would be
/// a second copy of that, one that a restart empties and that can disagree with
/// the log a human reads. The one thing the log does NOT hold is how far the
/// last roster got, so that — and only that — is what this writes back, as a
/// line of the same log (see below).
///
/// **The watermark is a timestamp AND a count, and both are needed.** A roster
/// records the newest `refused_ms` it covered plus how many of the covered
/// refusals carried exactly that millisecond; the next scan skips everything
/// older and the first `at_through` at that same millisecond. A bare timestamp
/// with `>` would DROP a refusal stamped in the same millisecond as the last
/// one reported (a report burst into one pane is precisely when that happens),
/// and with `>=` it would repeat one forever. The count is exact instead:
/// audit entries are appended in write order, so "the first N at that
/// millisecond" names the same N on every re-read, and a same-millisecond
/// refusal appended AFTER the roster ran is the (N+1)th and is picked up next
/// time.
///
/// **Only a DELIVERED roster moves the watermark** (`delivered: true` on its
/// audit line). A roster that was itself refused reports nothing to anybody, so
/// letting it advance the mark would lose exactly the payloads it was written
/// to name.
///
/// **Exclusions beyond [`refusal_row`]'s own**, each load-bearing:
/// - A roster that was itself refused. Including it would put the previous
///   roster's text inside the next roster's preview, and that one inside the one
///   after: the recursion the issue asks this mechanism not to have. Recognised
///   two ways because a refusal has two shapes and neither test covers both — a
///   queue-full refusal records the [`queue::EnqueueReason::RefusalRoster`] the
///   admission was attempted under, while the #633 pre-admission refusals never
///   reach an admission and so record no reason at all, and are caught by
///   [`REFUSAL_ROSTER_OPENER`] leading the preview loomux itself wrote.
/// - A [`RefusedPayload::StrandedSubmit`] marker refusal. It has no sender to
///   ask and no payload to re-send; what it means is "there is unsubmitted text
///   in this pane's box", which is a different instruction with its own
///   `consequence` string already surfaced verbatim by `queue_orphans`.
///   Folding it into a list whose every other row means "ask this agent to
///   send it again" would misdirect the reader.
///
/// Pure — entries in, roster out — for the same reason
/// [`front_door_refusals`] is.
pub fn refusal_roster(
    entries: &[AuditEntry],
    to_agent: &str,
    window_truncated: bool,
) -> RefusalRoster {
    let (mut through_ms, mut at_through) = (0u64, 0usize);
    for e in entries {
        if e.action == REFUSAL_ROSTER_ACTION
            && e.detail["to"] == json!(to_agent)
            && e.detail["delivered"] == json!(true)
        {
            through_ms = e.detail["through_ms"].as_u64().unwrap_or(0);
            at_through = e.detail["at_through"].as_u64().unwrap_or(0) as usize;
        }
    }
    let mut skipped_at_through = 0usize;
    // (index into `entries`, the row) — the index is what the re-send scan
    // below needs, since "already re-sent" is a fact about what came AFTER.
    let mut covered: Vec<(usize, RefusedDelivery)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let Some(row) = refusal_row(e) else { continue };
        if row.to != to_agent || row.payload != RefusedPayload::Prompt {
            continue;
        }
        if row.enqueue_reason.as_deref() == Some(queue::EnqueueReason::RefusalRoster.as_str())
            || row.preview.starts_with(REFUSAL_ROSTER_OPENER)
        {
            continue;
        }
        if row.refused_ms < through_ms {
            continue;
        }
        if row.refused_ms == through_ms && skipped_at_through < at_through {
            skipped_at_through += 1;
            continue;
        }
        covered.push((i, row));
    }
    let total = covered.len();
    let new_through_ms = covered.last().map(|(_, r)| r.refused_ms).unwrap_or(through_ms);
    // Counted over EVERYTHING covered, not over the capped list: the cap drops
    // the oldest rows from the wording, never from what this roster is
    // answerable for, and a watermark that stopped short of them would re-report
    // them at every future drain.
    let new_at_through =
        covered.iter().filter(|(_, r)| r.refused_ms == new_through_ms).count();
    if total > ROSTER_LIST_MAX {
        covered.drain(..total - ROSTER_LIST_MAX);
    }
    let items = covered
        .into_iter()
        .map(|(i, r)| RosterEntry {
            resent: refusal_was_resent(entries, i, &r),
            preview: queue::clamp_preview(&r.preview, ROSTER_PREVIEW_MAX),
            from: r.from,
            reason: r.reason,
        })
        .collect::<Vec<_>>();
    RefusalRoster {
        to: to_agent.to_string(),
        omitted: total - items.len(),
        items,
        total,
        through_ms: new_through_ms,
        at_through: new_at_through,
        window_truncated,
    }
}

/// Two sender names, out of two audit rows, that mean the same sender.
///
/// `audit.jsonl` spans the #1153 phase 3 flag day: one row can carry the
/// pre-rename host actor and the next the current one, for deliveries this app
/// sent minutes apart. A bare `==` between two such rows is a comparison of
/// spellings where the question is identity, and it answers wrong in both
/// directions — see the two call sites, which fail opposite ways.
///
/// Agent ids are unaffected: they are never host actors, so the second arm
/// cannot make two different agents compare equal.
fn same_audit_sender(a: &str, b: &str) -> bool {
    a == b || (brand::is_host_actor(a) && brand::is_host_actor(b))
}

/// #658: did `row`'s sender get this same payload through after `entries[at]`
/// refused it?
///
/// **Derived from the timeline, not assumed from silence.** `deliver_prompt`
/// audits `prompt` with the full text immediately before every admission, and a
/// refusal writes its own `delivery-dropped` line — so a re-send of the same
/// payload by the same sender to the same target is a later `prompt` line, and
/// a re-send that was refused AGAIN is that line followed by another refusal.
/// This walks forward counting the first and spending the second; anything left
/// over is a `prompt` that no refusal accounts for, i.e. one that was admitted
/// (or coalesced onto an entry already queued, which is the same fact for the
/// recipient: the payload is in the queue).
///
/// **Only a queue-full-shaped refusal spends a credit.** The #633 pre-admission
/// refusals write no `prompt` line at all — they carry their payload inline
/// instead, which is exactly what [`refusal_row`] surfaces as `text: Some(_)` —
/// so charging them for one would consume a re-send that really did land.
///
/// Matched on the same two fingerprints [`front_door_refusals`] pairs on
/// (`text.len() == bytes` and a recomputed
/// [`queue::dropped_payload_preview`]), never on positional adjacency, because
/// audit writes from concurrent delivery threads interleave. It inherits that
/// pairing's residual too: a DIFFERENT payload from the same sender to the same
/// target that agrees on length and on its collapsed first
/// [`queue::DROPPED_PREVIEW_MAX`] chars would be counted as this one's re-send.
/// The cost of being wrong is one row reading "already re-sent" instead of "ask
/// for it again" — which is why the answer is `false` whenever it cannot be
/// established at all (a refusal line too old to carry `bytes`): the roster
/// would rather ask for a delivery twice than tell a pane that a lost report is
/// already handled.
fn refusal_was_resent(entries: &[AuditEntry], at: usize, row: &RefusedDelivery) -> bool {
    let Some(bytes) = row.bytes else { return false };
    let mut credit: i64 = 0;
    for e in entries.iter().skip(at + 1) {
        if e.action == "prompt" {
            // rev-967 N5: across the flag day a bare `==` here fails to
            // credit a genuine re-send, and the roster tells an orchestrator
            // to ask again for a delivery that landed.
            if same_audit_sender(&e.actor, &row.from) && e.detail["to"] == json!(row.to) {
                if let Some(t) = e.detail["text"].as_str() {
                    if t.len() == bytes && queue::dropped_payload_preview(t) == row.preview {
                        credit += 1;
                    }
                }
            }
            continue;
        }
        let Some(later) = refusal_row(e) else { continue };
        if later.text.is_none()
            && later.to == row.to
            // The SECOND site of rev-967 N5's class, found by sweeping for it
            // rather than named in the review. It fails the OTHER way: an
            // unmatched spelling skips this `credit -= 1`, so a delivery that
            // was refused again reads as one that got through and drops off
            // the roster entirely — a loss, where the site above only causes
            // a duplicate ask.
            && same_audit_sender(&later.from, &row.from)
            && later.bytes == row.bytes
            && later.preview == row.preview
        {
            credit -= 1;
        }
    }
    credit > 0
}

/// #658: word a roster as the ONE line it has to be, or `None` when there is
/// nothing to say — the ordinary case, and the one that must cost a pane
/// nothing at all.
///
/// **One line is a hard requirement, not a style choice.** On an orchestrator
/// target this text is parked in [`OrchNoticeInbox`], whose `park` asserts
/// every notice is a single [`NOTICE_MARKER`]-led line so that every row
/// of the relay block stays maskable (#576/#621). Using the same string on the
/// pane-delivery path too means the recipient reads identical words whichever
/// channel carried it.
///
/// **It tells the reader what to DO, and never overstates.** Every row names
/// the sender to ask, because loomux does not and will not re-send these
/// itself: the payloads were declined synchronously and their senders were
/// told. Rows the sender has already got through say so rather than being
/// dropped from the list — see [`RosterEntry::resent`].
pub fn refusal_roster_notice(r: &RefusalRoster) -> Option<String> {
    if r.items.is_empty() {
        return None;
    }
    let n = r.total;
    // Opens with the shared constant — which itself leads with
    // `NOTICE_MARKER`, the single-line maskability requirement above.
    let mut out = format!(
        "{REFUSAL_ROSTER_OPENER} While it was full, {n} deliver{y} to you {was} REFUSED and \
         never queued — loomux does NOT re-send them, so anything below that is not marked \
         re-sent is still missing:",
        y = if n == 1 { "y" } else { "ies" },
        was = if n == 1 { "was" } else { "were" },
    );
    for (i, it) in r.items.iter().enumerate() {
        out.push_str(if i == 0 { " " } else { " | " });
        let clause = if it.resent {
            format!("{} has since re-sent it — nothing to do", it.from)
        } else {
            format!("NOT re-sent — ask {} for it", it.from)
        };
        out.push_str(&format!(
            "{} \"{}\" ({}; {clause})",
            it.from,
            it.preview,
            it.reason.as_str()
        ));
    }
    if r.omitted > 0 {
        out.push_str(&format!(
            " | plus {} earlier refusal{s} not listed here (this roster names {ROSTER_LIST_MAX}) \
             — every one is in this group's audit.jsonl as a `delivery-dropped` line",
            r.omitted,
            s = if r.omitted == 1 { "" } else { "s" }
        ));
    }
    if r.window_truncated {
        out.push_str(
            " | the audit window this was read from was itself cut, so there may be older \
             refusals it could not see",
        );
    }
    Some(out)
}

/// Cap for the `task` field in a `list_agents` roster row (#851): a spawn
/// brief can run multiple hundred words, and a roster with a dozen dead
/// agents each carrying its full brief verbatim pushed one group's
/// `list_agents` response to ~4k tokens. The full brief stays retrievable
/// where it already lives (the audit log, the task board) — this is only
/// the roster excerpt, so the orchestrator can tell what a row is about
/// without paying for the whole text on every call.
const TASK_EXCERPT_CHARS: usize = 140;

/// First `n` **chars** (not bytes) of `s`, with `…` appended when something
/// was dropped. Counting chars rather than bytes keeps the cap UTF-8-safe
/// by construction — `s.chars().take(n)` can never land mid-codepoint, so
/// there is no boundary search to get wrong, unlike a byte-offset cut.
fn task_excerpt(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

/// The diagnostic clause `on_pty_exit` puts in its orchestrator notice (#281):
/// a bare exit code can't distinguish "produced real output, then failed" from
/// "exited before printing a single byte" (the resume-CLI-boots-and-
/// immediately-exits signature this issue is about) — `total_bytes == 0` names
/// that case explicitly instead of leaving the orchestrator to guess. Only
/// ever called for an exit loomux did NOT itself cause (see `on_pty_exit`'s
/// `expected` guard), but still covers a clean, voluntary exit_code 0 that
/// happened to produce nothing — so the wording says "exited", never "died"
/// or "crashed", which would misname a graceful (if unhelpfully silent) stop
/// as a failure. Factored out as a pure function (no pty/app handle needed)
/// so it's directly unit-testable.
pub fn exit_diagnostic(tail: &str, total_bytes: u64) -> String {
    if total_bytes == 0 {
        "produced no output before exiting — it likely exited before the CLI printed \
         anything at all (a missing/corrupt session file, a rejected resume flag, or \
         a gone cwd are the usual causes)"
            .to_string()
    } else {
        format!("last output before it exited: {:?}", tail_snippet(tail, 400))
    }
}

/// The `cause` clause `on_pty_exit` puts in its notice — `exit_diagnostic`,
/// gated behind whether loomux itself caused this exit.
///
/// This gate exists because `PtyManager::kill` (pty.rs) removes the pty
/// handle from its live map BEFORE the waiter thread ever gets to snapshot
/// its output, so an `expected` exit (kill_agent, idle-kill, a pane close)
/// always arrives here with `tail == ""` and `total_bytes == 0` — REGARDLESS
/// of how much real work the agent actually did. Feeding that straight into
/// `exit_diagnostic` would misdiagnose a productive delegate the orchestrator
/// deliberately stopped as having silently died before printing anything,
/// pointing it at a corrupt session file or a bad resume flag that was never
/// the issue. The diagnostic is only meaningful for an exit loomux did NOT
/// cause; an expected one gets a plain, honest "loomux stopped it" instead.
/// Pure so this gate — the actual fix — is directly unit-testable without the
/// live pty/bind machinery `on_pty_exit` itself needs.
pub fn exit_cause(expected: bool, tail: &str, total_bytes: u64) -> String {
    if expected {
        "loomux stopped it (kill or idle-timeout) — not a crash".to_string()
    } else {
        exit_diagnostic(tail, total_bytes)
    }
}

/// Who initiated an agent's termination (#533-B) — recorded on the agent's
/// own record by the code that issues the kill, BEFORE the pty is touched.
///
/// This exists because the pre-#533 signal for "loomux stopped it" was
/// `on_pty_exit`'s `expected` flag, which is a property of the PTY
/// (`PtyManager::kill` inserts the id into `expected_exits`) and therefore
/// answers a different question: it says loomux closed the pane, not WHO
/// decided to. A human closing a pane, `end_group`'s teardown, and the
/// orchestrator's own `kill_agent` all arrive as `expected: true` and are
/// indistinguishable there. Routing a notice on that would demote exits
/// the orchestrator never initiated, which is precisely what #533-B says
/// must keep prompting — so the initiator is recorded as a FACT at the call
/// site instead of inferred at the exit site.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExitInitiator {
    /// The orchestrator's own `kill_agent` MCP call — it asked for this
    /// exit, so being told about it is a turn spent learning what it
    /// already knows.
    Orchestrator,
    /// The idle-kill guardrail's reaper (`reap_idle_agents`). By
    /// construction it only ever kills workers/reviewers that are IDLE —
    /// no task in flight, nothing to lose — which is what makes it safe to
    /// demote alongside an orchestrator-initiated kill.
    IdleTimeout,
    /// The review driver releasing a pane it no longer needs (#2501, #2811 S1) — a
    /// reviewer lane whose verdict is recorded at the drive's current head, a
    /// worker that reported and went idle once the drive had consumed the
    /// report, or either of them at the step that ENDS the drive.
    /// `reviewdrive::releasable` is the closed rule and
    /// `OrchRegistry::release_driven_pane` is the only thing that stamps this.
    DriverRelease,
    /// The LEAD pane of this delegate's group, by dying (#2519). A lead group
    /// has no root once its lead is gone, so its helpers are ended with it —
    /// `OrchRegistry::end_lead_children` is the only thing that stamps this.
    ///
    /// Demoted for a reason the other three do not have: there is nobody left
    /// to prompt. The pane an exit notice would be typed into is the one whose
    /// death caused the exit, so a `Prompt` here is a delivery whose only
    /// possible outcome is a dropped notice.
    LeadExit,
    /// A PLANNER closing itself after its `report(done)` (#203, demoted by
    /// #3040 N2). `OrchRegistry::close_completed_planner` is the only thing
    /// that stamps this.
    ///
    /// Its own variant rather than a reuse of `Orchestrator`, on the
    /// instruction in [`exit_notice_route`]'s doc: nothing in this process
    /// asked for it in the sense that variant means — the planner's own final
    /// report is what triggers it — so recording it as an orchestrator kill
    /// would put a false initiator on the audit row that exists to make the
    /// demotion readable.
    ///
    /// Demoted for the reason that doc applies to `IdleTimeout`, plus one it
    /// does not have: nothing is in flight (the planner's contract is one plan
    /// → one report → exit, and the report has landed), the output is durable
    /// (the plan is on GitHub and the report is the previous prompt in the
    /// recipient's own pane), and the roster carries the liveness half.
    PlannerCompleted,
}

impl ExitInitiator {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitInitiator::Orchestrator => "orchestrator",
            ExitInitiator::IdleTimeout => "idle-timeout",
            ExitInitiator::DriverRelease => "driver-release",
            ExitInitiator::LeadExit => "lead-exit",
            ExitInitiator::PlannerCompleted => "planner-completed",
        }
    }
}

/// Where an agent-exit notice goes (#533-B).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitNoticeRoute {
    /// Deliver to the orchestrator's pane — costs it a turn, and is worth
    /// one: it did not cause this and cannot know about it otherwise.
    Prompt,
    /// Write the notice to the audit log and stop. The roster
    /// (`list_agents`) already reflects liveness, so the orchestrator can
    /// read this on demand rather than being interrupted by it.
    AuditOnly,
}

/// Route an agent-exit notice from the RECORDED initiator (#533-B).
///
/// Pure, and deliberately takes only the recorded initiator — not
/// `expected`, not the exit code, not the tail. Anything else would be the
/// inference this replaces.
///
/// **If the idle reaper ever grows a mode that kills agents with in-flight
/// work, that mode must PROMPT.** `IdleTimeout` is demoted here only
/// because `reap_idle_agents` is, by construction, restricted to agents
/// that are idle — no task, nothing lost. A future "kill the stalled
/// worker" reaper is a materially different event the orchestrator did not
/// ask for and cannot reconstruct from the roster: give it its own
/// `ExitInitiator` variant and route it to `Prompt`, rather than reusing
/// this one.
///
/// **`DriverRelease` is demoted, and it is worth saying why it is not the
/// "kill to reclaim a slot" case that sentence turns away** (#2501). That
/// case is a reaper choosing a victim to make room; this is a drive
/// disposing of a pane whose work it has already taken delivery of, in one
/// of the states `reviewdrive::releasable` closes over: a lane whose
/// verdict is recorded at the drive's current head, a worker whose
/// `report` the drive has consumed, or — since #2811 S1 — either of them at
/// the step that ends the drive, where there is nothing left to wait for.
/// All carry `IdleTimeout`'s own property — the pane is idle, so nothing
/// is in flight — and add the one it does not: the output is already
/// durable (a verdict file, a consumed report) and the conversation is
/// resumable by session, so nothing is lost rather than merely nothing in
/// progress.
///
/// The orchestrator can reconstruct it, which is the actual test this
/// function applies: `rd-lane-released` / `rd-worker-released` name the
/// pane, the session and the reason on the audit log, the roster reflects
/// the liveness, and the drive's own exit notice lists the panes that are
/// still there. And prompting would be self-defeating — an orchestrator
/// turn per released pane is precisely the cost #2501 measured and exists
/// to remove, so a `Prompt` here would spend the saving on announcing it.
pub fn exit_notice_route(initiator: Option<ExitInitiator>) -> ExitNoticeRoute {
    match initiator {
        Some(ExitInitiator::Orchestrator)
        | Some(ExitInitiator::IdleTimeout)
        | Some(ExitInitiator::DriverRelease)
        | Some(ExitInitiator::LeadExit)
        | Some(ExitInitiator::PlannerCompleted) => ExitNoticeRoute::AuditOnly,
        // Crash, watchdog-driven death, an agent quitting unexpectedly, a
        // human closing the pane — nobody in this process asked for it.
        None => ExitNoticeRoute::Prompt,
    }
}

/// What `agent_output_tail` returns given whatever the live pty produced (if
/// still alive) and whatever was captured at exit (#281). Factored out as a
/// pure function so the fallback — the actual behavior change — is directly
/// unit-testable without a live pty/app handle, which `agent_output_tail`
/// itself can't be driven with in a unit test.
pub fn resolve_output_text(live: Option<String>, last_exit_tail: Option<&str>) -> Result<String, String> {
    if let Some(t) = live {
        return Ok(t);
    }
    match last_exit_tail {
        Some(t) if !t.is_empty() => Ok(t.to_string()),
        // The live pty is already gone (the agent exited) and nothing was
        // captured at exit time either — the pre-#281 behavior, kept as the
        // last resort rather than inventing content that was never seen.
        _ => Err("terminal already closed".to_string()),
    }
}

/// Everything `attention_tick`'s phase 2 reads out of ONE pane's masked tail.
///
/// A struct rather than two parallel `HashMap`s (#2811 S5a) because both
/// answers come from the same `mask_loomux_notices_with_record` call, and
/// keeping them together is what makes that literal: two maps built in one
/// closure invite a later edit to compute one of them somewhere else, off a
/// tail that was never masked — which is precisely the defect the mask exists
/// to prevent.
struct PaneTailSignals {
    /// The pane's tail looks like an interactive prompt awaiting an answer
    /// (`prompt_wait_detected`) — one input to the `waiting` reason.
    shaped: bool,
    /// The provider spend/usage limit this pane is sitting on, if any
    /// (`providerlimit::limit_in_tail`).
    limit: Option<&'static providerlimit::LimitPattern>,
}

/// One pane's attention-scan tail: a BOUNDED raw read of its output ring,
/// ANSI-stripped (#717).
///
/// `read` is the raw-byte reader — `PtyManager::output_tail_bounded` at every
/// production call site (`attention_inputs` for agent panes,
/// `pane_attention_inputs_from` for plain ones). A closure rather than the
/// manager itself for the same reason `Tier1Scan::read` takes one: the SIZE of
/// the request is the only thing an assertion can reach here. Slicing the last
/// `ATTENTION_SCAN_BYTES` off a whole-ring read produces a byte-identical
/// string, so a test that only looks at the returned text cannot tell a 4 KB
/// copy under the `ptys` mutex from a 256 KB one — and the copy is the defect.
///
/// The strip runs OUTSIDE the read (the reader has already released both locks
/// by the time it returns), so no scanning happens under the lock at all.
pub fn attention_tail(read: impl FnOnce(usize) -> Option<Vec<u8>>) -> Option<String> {
    read(ATTENTION_SCAN_BYTES).map(|raw| strip_ansi(&raw))
}

/// Strip ANSI escape sequences (CSI, OSC, two-byte ESC) and carriage
/// returns so `get_output` returns readable text from raw terminal bytes.
pub fn strip_ansi(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            i += 1;
            match bytes.get(i) {
                Some(b'[') => {
                    // CSI: parameters/intermediates until a final byte 0x40-0x7E.
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                Some(b']') => {
                    // OSC: until BEL or ESC \.
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                Some(_) => i += 2 - 1, // two-byte escape: skip the introducer
                None => {}
            }
            continue;
        }
        if b == b'\r' || (b < 0x20 && b != b'\n' && b != b'\t') {
            i += 1;
            continue;
        }
        // Decode this UTF-8 unit; fall back to skipping the byte.
        let len = match b {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        if let Ok(s) = std::str::from_utf8(&bytes[i..(i + len).min(bytes.len())]) {
            out.push_str(s);
        }
        i += len;
    }
    out
}

/// #480/#496 PR-E: a redrawn spinner/statusline frame's stable "core", for
/// deciding whether two CONSECUTIVE lines are the same repaint. Strips
/// exactly one leading glyph run (the spinner symbol — braille dots, Claude's
/// `✻`/`✢`/`✽` star family, `*`, a box-drawing bullet, whatever a given CLI
/// redraws each tick) plus the space after it, and exactly one trailing
/// parenthesized group (the `(esc to interrupt · 8s · ↓ 172 tokens)` shape
/// documented at `auto_compact_banner_substrings` above — elapsed time and
/// token counts that change every frame live here). What is left is the
/// stable prose a human actually wants to read once.
///
/// Deliberately NO fuzzy matching beyond that: two lines collapse only when
/// this core is byte-identical. A line that merely shares a leading glyph but
/// says something different keeps its own core and is never merged — see
/// `collapse_repeated_frames`'s doc for the conservatism this buys.
///
/// The trailing-paren strip is gated on the line actually having had a
/// leading glyph stripped (rev-29's #501 review finding, N1). Applying it to
/// *any* line ending in `)` over-collapses ordinary content that just happens
/// to differ only inside trailing parens — `fn parse(input: &str)` next to
/// `fn parse(input: &[u8])` is exactly the shape a worker-pane tail is full
/// of (code listings, rustc diagnostics), and both are real, distinct lines,
/// not a redraw. Every real spinner shape this repo has documented is
/// glyph-led, so gating on that costs nothing for the case this function
/// exists to catch while closing the false-merge class entirely.
fn spinner_frame_core(line: &str) -> &str {
    let trimmed = line.trim();
    let leads_with_glyph = trimmed.chars().next().is_some_and(|c| !c.is_alphanumeric() && !c.is_whitespace());
    let after_glyph = trimmed.trim_start_matches(|c: char| !c.is_alphanumeric() && !c.is_whitespace());
    let after_glyph = after_glyph.strip_prefix(' ').unwrap_or(after_glyph);
    if leads_with_glyph && after_glyph.ends_with(')') {
        match after_glyph.rfind('(') {
            Some(i) => after_glyph[..i].trim_end(),
            None => after_glyph,
        }
    } else {
        after_glyph
    }
}

/// A core shorter than this never collapses, even if repeated — guards short,
/// legitimately-repeated lines (a bare prompt character, several blank lines
/// in a row) from ever being read as a redrawn frame. Conservatism per
/// #480/#496 PR-E's brief: prefer under-collapsing to over-collapsing.
const SPINNER_FRAME_MIN_CORE_LEN: usize = 6;

/// #480/#496 PR-E: collapse a run of CONSECUTIVE lines that share a
/// `spinner_frame_core` into one — the freshest (last) line of the run,
/// verbatim, plus a `(N repeated frames collapsed)` marker so the elision is
/// visible rather than silent (this repo has a whole batch of lessons about
/// claims of completeness that turned out false; a silent drop here would be
/// exactly that). Only ADJACENT lines are ever compared: two identical lines
/// separated by other content are a real repeat in the transcript (e.g. the
/// same log line printed twice, far apart), not a redraw, and are left alone.
pub fn collapse_repeated_frames(lines: &[&str]) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let core = spinner_frame_core(lines[i]);
        if core.chars().count() >= SPINNER_FRAME_MIN_CORE_LEN {
            let mut j = i + 1;
            while j < lines.len() && spinner_frame_core(lines[j]) == core {
                j += 1;
            }
            let run = j - i;
            if run > 1 {
                out.push(lines[j - 1].to_string()); // freshest frame, verbatim
                out.push(format!("[... {} repeated frames collapsed ...]", run - 1));
                i = j;
                continue;
            }
        }
        out.push(lines[i].to_string());
        i += 1;
    }
    out
}

/// What `get_output` (`agent_output_tail`) actually returns for an already-
/// `strip_ansi`'d pane render: repeated spinner/statusline frames collapsed
/// (`collapse_repeated_frames`), then the last `n_lines` of THAT, `n_lines`
/// clamped to `[1, 500]` exactly like `agent_output_tail` always has. Factored
/// out pure, same reasoning as `resolve_output_text` above, so `get_output`'s
/// behavior is directly testable without a live pty/app handle.
///
/// This is `get_output`'s OWN path only, strictly after the shared
/// `strip_ansi` this function's caller already applied — nothing here changes
/// what `strip_ansi` itself returns to its other callers (`box_holds_paste`,
/// `prompt_wait_detected`, the compact/menu detectors above); they never call
/// this function, and never see collapsed text.
pub fn format_output_tail(text: &str, n_lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let collapsed = collapse_repeated_frames(&all);
    let n = n_lines.clamp(1, 500);
    let start = collapsed.len().saturating_sub(n);
    cap_output_bytes(collapsed[start..].join("\n"))
}

/// Hard ceiling on what one `get_output` call can put into the caller's
/// context, in bytes, whatever `lines` was asked for (#520).
///
/// `lines` bounds distinct content *lines*; nothing bounded the *payload*. A
/// pane rendering a 200-column TUI can put several KB on a single line, and
/// 500 lines of that is a six-figure token bill delivered to an orchestrator
/// that asked a small question. The two limits are independent on purpose:
/// whichever binds first wins.
///
/// 8 KB is roughly two full screens of a wide pane — enough to answer "what
/// is this agent doing right now", which is what the tool is for. Anything
/// larger is a job for the agent's own report, not for monitoring.
pub const OUTPUT_TAIL_MAX_BYTES: usize = 8 * 1024;

/// Headroom reserved for the truncation marker so the *returned* string —
/// marker included — is always within [`OUTPUT_TAIL_MAX_BYTES`]. A cap that
/// the cap's own announcement can push you over is not a cap.
const OUTPUT_TAIL_MARKER_RESERVE: usize = 96;

/// Trim `text` to [`OUTPUT_TAIL_MAX_BYTES`], keeping the **newest** end —
/// a monitoring read wants what the pane is doing now, not how it started —
/// and saying so on the line it dropped.
///
/// The marker states plainly that bytes were dropped and how many. It does
/// NOT characterise them ("animation residue" was the phrasing #520 proposed):
/// by the time this runs the composed-grid replay has already removed the
/// redraw churn, so anything still here and still over budget is as likely to
/// be a legitimately enormous build log. Labelling real output as residue
/// would be a claim the code can't back — the thing this repo keeps writing
/// lessons about — so the marker reports the fact (bytes dropped, cap hit)
/// and leaves the interpretation to the reader.
fn cap_output_bytes(text: String) -> String {
    if text.len() <= OUTPUT_TAIL_MAX_BYTES {
        return text;
    }
    let budget = OUTPUT_TAIL_MAX_BYTES - OUTPUT_TAIL_MARKER_RESERVE;
    let mut keep_from = text.len() - budget;
    // Char boundary FIRST: pane output is full of multibyte glyphs (box
    // drawing, arrows, spinner stars), and `text[keep_from..]` panics on an
    // offset that lands inside one. Only then look for a line boundary, so
    // the first surviving line isn't a fragment.
    while keep_from < text.len() && !text.is_char_boundary(keep_from) {
        keep_from += 1;
    }
    if let Some(i) = text[keep_from..].find('\n') {
        keep_from += i + 1;
    }
    format!(
        "[... truncated {} bytes: over get_output's {} KB cap ...]\n{}",
        keep_from,
        OUTPUT_TAIL_MAX_BYTES / 1024,
        &text[keep_from..]
    )
}

/// Wrap prompt text in a bracketed paste so multi-line prompts land in the
/// CLI's input box instead of submitting at the first newline. The Enter is
/// sent separately after `PASTE_SUBMIT_DELAY`.
pub fn bracketed_paste(text: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(text.len() + 12);
    v.extend_from_slice(b"\x1b[200~");
    v.extend_from_slice(text.replace("\r\n", "\n").as_bytes());
    v.extend_from_slice(b"\x1b[201~");
    v
}

/// The byte sequence loomux writes to submit a delivered prompt, chosen per
/// CLI. Kept pure and `&'static` so the exact bytes are unit-assertable.
///
/// Claude Code submits on a bare CR (`\r`).
///
/// GitHub Copilot's TUI (#98) gates *keyboard* input on terminal focus: it
/// enables DEC mode 1004 focus reporting (`ESC[?1004h`) and, in its editor's
/// key handler, drops every non-paste keystroke while its focus flag is false
/// (`if (!focused && !key.paste && code != backspace/delete) return`). A
/// *paste* bypasses that guard — which is why a delivered prompt's text lands
/// in the input box — but the Enter that follows is a plain key, so on a pane
/// that isn't the focused one (the normal case when an agent delivers to
/// another agent's pane) it is ignored and the prompt just sits there until a
/// human clicks in (whereupon the terminal emits focus-in and their Enter
/// works). Prefixing the CR with a focus-in report (`ESC[I`, which Copilot
/// parses to a focus event that flips its flag true) makes the very next key —
/// our Enter — accepted, so the prompt submits without a human. Copilot leaves
/// its flag true afterward, so the spaced retry Enters need no re-prefix, but
/// they carry it too so each retry is self-sufficient if a stray blur arrives.
pub fn submit_sequence(cli: &str) -> &'static [u8] {
    match cli {
        "copilot" => b"\x1b[I\r",
        _ => b"\r",
    }
}

/// The outcome of the most recent delivery to a pane, kept in-memory per pty so
/// the next delivery can detect a previous prompt still stranded in the input
/// box (#81/#84). The TYPE is `pub` (#420 rev-19 R3) so `record_aborted_
/// preenter_outcome`/`recorded_confirmed` can name it in their signatures —
/// fields stay private, so outside code (including tests) still can't
/// construct or read one directly, only through those two functions.
#[derive(Clone, Debug)]
pub struct DeliveryOutcome {
    /// Whether that delivery's Enter was observed to submit (box cleared / turn
    /// started). `false` means the text may still be sitting unsubmitted.
    confirmed: bool,
    /// Unix-ms the final Enter was sent — the reference point for deciding
    /// whether a human has since typed into the pane.
    submit_sent_ms: u64,
    /// Production bug fix (PR #329 round 7): the delivery's `from` (the
    /// `deliver_prompt` caller — [`brand::AUDIT_ACTOR`] for every compact-nudge notice,
    /// `"human"` for a message typed through loomux's UI, an agent id for a
    /// forwarded `message_orchestrator`). See `AgentEntry::compact_
    /// inference_guard_until_ms`'s doc for why this specific distinction
    /// matters.
    from: String,
    /// #813 round 2: the text this delivery left sitting in the pane's box, for
    /// readers that must ask **whether our own text is still there** — a
    /// question no other field on this record, and no other signal on the pane,
    /// can answer.
    ///
    /// `input_box_len` cannot answer it: that counter tracks characters the
    /// HUMAN typed (`PtyManager::note_user_input`), and loomux's own paste goes
    /// out through `write_bytes`, which never touches it. So `!input_pending`
    /// means "no human characters are outstanding", NOT "the box is empty", and
    /// a reader that treats the two as the same thing is reasoning about a
    /// different pane than the one in front of it. That conflation is exactly
    /// what #813's first cut got wrong.
    ///
    /// `None` where there is no text to look for — a record from a path that
    /// pasted nothing, or one that predates this field. Every consumer treats
    /// `None` as "no reading available" and takes the conservative branch,
    /// never as "the box is clear".
    stranded_text: Option<String>,
}

/// Record a pre-Enter question-abort's outcome (#420 rev-15 B3, extracted
/// rev-19 R3): the paste already landed but the Enter was withheld, so the
/// text is sitting unsubmitted in the box — record it exactly like a normal
/// (non-aborted) delivery would, so the NEXT delivery's `flush_stranded_text`
/// can see it's stranded and clear it. Extracted into its own function (not
/// inlined at the one call site) so an integration test can assert the
/// INSERT itself happens — deleting the call `deliver_prompt` makes to this
/// function is exactly the rev-19 mutation that must fail a dedicated test,
/// not just a test of the downstream consequence a test could fabricate the
/// input for. `DeliveryOutcome` stays private (this crate's own precedent,
/// see `DeliveryConfirmation`'s doc, for not exposing delivery-plumbing
/// internals) — callers (including tests) never need to name it: they own
/// the map through `Mutex<HashMap<u32, _>>` and let this function's
/// signature pin the value type.
#[doc(hidden)] // pub for integration tests
pub fn record_aborted_preenter_outcome(
    last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>,
    pty_id: u32,
    delivery_from: String,
    // #813: the text that is sitting in the box right now. This call site is
    // the one that KNOWS it — the paste it made is the stranded text — and the
    // marker queued immediately after carries no text of its own, so if it is
    // not recorded here nothing downstream can ever ask whether it is still
    // there.
    pasted_text: Option<String>,
) {
    record_inflight_delivery(last_delivery, pty_id, now_ms(), delivery_from, pasted_text);
}

/// Publish a delivery's OWNERSHIP of a pane into the ledger (#454) — the
/// same `DeliveryOutcome` shape every other writer uses, `confirmed: false`,
/// written at a moment when nothing about the outcome is known yet.
///
/// This is what makes supersession a START-of-delivery fact rather than an
/// END-of-delivery one. `deliver_now` calls it immediately before its FIRST
/// Enter, so for any `promptsubmit` record that Enter can produce there is a
/// happens-before chain — **ledger insert ≺ Enter ≺ hook record** — and an
/// OLDER delivery's late monitor, which takes its ledger observation AFTER
/// its hook read (see `observe_ledger`'s call site in
/// `run_late_confirmation_monitor`), can never see that record while still
/// believing it owns the pane.
///
/// Before this, the ledger was written only at the END of the newer
/// delivery's confirm window (≲1s typical, ~9s worst case). That left
/// exactly that window open for a stale monitor to read "still mine" from
/// the ledger, then match the NEWER delivery's hook record, and resolve its
/// OWN delivery off it — a misattributed `delivery-confirmed-late` audit row
/// and, if that monitor had already declared `Failed`, a "no re-send needed"
/// correction notice about a re-send that is the only reason anything landed
/// at all. #451 B1's supersession rule closed the dangerous version of that
/// (a correction arriving BEFORE a re-send and suppressing it); #454 is the
/// narrowed residual it deferred, and this is the "compare against an
/// in-flight delivery marker" fix that issue asked for — closing the window
/// rather than narrowing it further.
///
/// Note the marker is not a fourth piece of state: it is the SAME map, under
/// the SAME lock, so the monitor still decides from one atomic read. A
/// separate `in_flight` map would have re-opened the hazard one level down —
/// two locks means a torn observation (read the in-flight map, miss the
/// newer delivery, then read the ledger), which is the fused-lock mistake
/// #496 PR-C's own admission gate had to be fixed for (rev-47 B1).
///
/// Extracted (not inlined at its one call site) for the same reason
/// `record_aborted_preenter_outcome` is: deleting the call `deliver_now`
/// makes to it is precisely the mutation a dedicated test must catch.
#[doc(hidden)] // pub for integration tests
pub fn record_inflight_delivery(
    last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>,
    pty_id: u32,
    submit_sent_ms: u64,
    delivery_from: String,
    // #813: see `DeliveryOutcome::stranded_text`.
    pasted_text: Option<String>,
) {
    last_delivery.lock_safe().insert(pty_id, DeliveryOutcome {
        confirmed: false,
        submit_sent_ms,
        from: delivery_from,
        stranded_text: pasted_text,
    });
}

/// One ATOMIC observation of a pane's delivery ledger, from the point of
/// view of the delivery that pressed Enter at `submit_sent_ms` (#454).
///
/// Every fact `run_late_confirmation_monitor` decides from comes from this
/// one read, so no two of them can be drawn from different moments. The
/// monitor used to derive `superseded` and `ledger_outstanding` from one
/// snapshot already and then re-derive `outstanding` inline later in the
/// same tick; this makes the single-observation discipline the type's job
/// rather than a convention the next edit can quietly break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)] // pub for integration tests
pub struct LedgerView {
    /// A DIFFERENT delivery owns this pane now — this monitor must exit
    /// without writing or notifying anything (`MonitorAction::Superseded`).
    pub superseded: bool,
    /// The recorded outcome is still THIS delivery's and still unconfirmed —
    /// the durable "not landed yet" fact #496's self-heal judges from, ahead
    /// of any pane reading.
    pub outstanding: bool,
    /// Superseded AND the newer delivery is recorded confirmed — the pane is
    /// demonstrably unwedged, so a stranded badge can come down on the way
    /// out. A newer UNCONFIRMED outcome leaves it up: that delivery's own
    /// monitor (or, if it confirmed in-window, `deliver_now` itself) owns the
    /// pane's state from here.
    pub newer_confirmed: bool,
}

#[doc(hidden)] // pub for integration tests
pub fn observe_ledger(
    last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>,
    pty_id: u32,
    submit_sent_ms: u64,
) -> LedgerView {
    let recorded = last_delivery.lock_safe().get(&pty_id).cloned();
    let superseded = recorded.as_ref().is_some_and(|o| o.submit_sent_ms != submit_sent_ms);
    LedgerView {
        superseded,
        outstanding: recorded
            .as_ref()
            .is_some_and(|o| o.submit_sent_ms == submit_sent_ms && !o.confirmed),
        newer_confirmed: superseded && recorded.as_ref().is_some_and(|o| o.confirmed),
    }
}

/// Whether the pane's most recently recorded delivery outcome reads as
/// "confirmed" (#420 rev-19 R3) — the exact extraction `deliver_prompt`'s
/// flush step performs on `last_delivery` (`prev.as_ref().map(|o|
/// o.confirmed)`), pulled out so a test can read back what
/// `record_aborted_preenter_outcome` stored without naming `DeliveryOutcome`
/// either.
#[doc(hidden)] // pub for integration tests
pub fn recorded_confirmed(last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>, pty_id: u32) -> Option<bool> {
    last_delivery.lock_safe().get(&pty_id).map(|o| o.confirmed)
}

/// Why a live, idle, typeable pane on the right session is **not** ready to take
/// a brief (#2089). `None` from [`pane_delivery_readiness`] means it is.
///
/// Every variant is a fact orrerix's own delivery machinery already recorded —
/// never a reading of the pane's screen, which `docs/design/review-driver.md` §3
/// keeps out of the review driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneNotReady {
    /// Something is already waiting to be pasted into this pane. Whatever the
    /// CLI is doing, it has not drained what it was last given, so a brief
    /// admitted now lands BEHIND that.
    Queued,
    /// The last delivery to this pane is on record as not having landed —
    /// `Pending` or `Failed` in [`DeliveryConfirmState`]'s three-state sense,
    /// which `DeliveryOutcome::confirmed` folds into one `false`. Its text may
    /// still be sitting unsubmitted in the box.
    ///
    /// **The complement is WIDER than the hook signal**, and that is disclosed
    /// rather than implied: [`confirm_state_for`] resolves `Box`, `Hook` AND
    /// `Burst` to `Confirmed`, so a delivery decided by #112's Tier 3 output
    /// heuristic reads ready here even though a repaint can satisfy that tier.
    /// Narrowing to `Box`/`Hook` needs [`ConfirmSource`] carried on
    /// `DeliveryOutcome`, which nothing stores; the trade and how to settle it
    /// from `prompt-typed`'s own `confirm_source` column are in
    /// `docs/design/review-driver.md` §3.1 item 5.
    Unconfirmed,
    /// Nothing has ever been delivered to this pty, so there is no evidence
    /// either way. Refused rather than assumed: "we could not look" is not
    /// "there was nothing there" — the same asymmetry `rd_pane_exit` states
    /// about a `Dead` reading.
    NoRecord,
}

impl PaneNotReady {
    pub fn as_str(self) -> &'static str {
        match self {
            PaneNotReady::Queued => "queued",
            PaneNotReady::Unconfirmed => "unconfirmed",
            PaneNotReady::NoRecord => "no-record",
        }
    }
}

/// **Is a pane at a point where a brief typed into it will be READ rather than
/// parked behind something?** (#2089) `None` = ready.
///
/// Pure, so the rule itself is pinnable without a live pty; the impure reader is
/// [`OrchRegistry::pane_readiness`], which supplies
/// [`OrchRegistry::queue_depth`] and [`recorded_confirmed`].
///
/// **Queue depth is asked FIRST**, and the precedence is a decision rather than
/// an ordering accident: a non-empty queue decides on its own, whatever the last
/// delivery did, because the brief would sit behind entries nothing has pasted
/// yet. `Unconfirmed` and `NoRecord` are only ever reached for an empty queue.
///
/// **What this CANNOT see, stated because a predicate must state its residual.**
/// It is evidence about the last DELIVERY, not about the pane right now: a
/// dialog raised AFTER a confirmed delivery, with nothing queued behind it,
/// reads ready and is not caught. Closing that would mean judging a pane's
/// screen, which §3 forbids the driver; it is bounded exactly as it was before
/// #2089, by `held(fix-stalled)`/`held(lane-stalled)` naming the pane.
///
/// Nor is it ATOMIC with the paste that follows it: anything admitted to the
/// pane's queue between this answer and `deliver_prompt` is pasted first. The
/// window is not widened by #2089 — the arm it replaces had the same gap with no
/// readiness read to race — but "ready" means "was ready when asked", not "will
/// still be when the brief lands". See `docs/design/review-driver.md` §3.1 item 5
/// for both residuals and the trade behind the wider `Confirmed`.
#[doc(hidden)] // pub for integration tests
pub fn pane_delivery_readiness(
    queue_depth: usize,
    last_confirmed: Option<bool>,
) -> Option<PaneNotReady> {
    if queue_depth > 0 {
        return Some(PaneNotReady::Queued);
    }
    match last_confirmed {
        Some(true) => None,
        Some(false) => Some(PaneNotReady::Unconfirmed),
        None => Some(PaneNotReady::NoRecord),
    }
}

/// What [`OrchRegistry::idle_pane_on_session`] found: the pane to reuse, if any,
/// and every candidate that got as far as the readiness test and was refused by
/// it (#2089).
///
/// `declined` exists so that refusal is not SILENT. Its only other visible
/// effect is a fresh pane, and on the audit log that is indistinguishable from
/// there having been no candidate at all — the shape of silence §3 objects to.
/// `rd_reuse_pane` turns each entry into an `rd-reuse-declined` row (§5.4).
pub struct ReusablePane {
    pub agent: Option<String>,
    pub declined: Vec<(String, PaneNotReady)>,
}

/// Record a stranded (unconfirmed) delivery whose submit happened at an
/// EXPLICIT time (#518, integration tests only). `record_aborted_preenter_
/// outcome` always stamps `now_ms()`, which cannot express the one state
/// #518's bound is about: a delivery whose submit is in the past, with a
/// keystroke stamp after it that has since gone stale. Paired with
/// `PtyManager::set_user_input_ms_for_test`, this lets a test place both ends
/// of that comparison without sleeping out the ten-minute bound — and without
/// making `DeliveryOutcome` public, which this file's own precedent
/// (`DeliveryConfirmation`'s doc) argues against.
#[doc(hidden)] // pub for integration tests
pub fn record_stranded_outcome_at_for_test(
    last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>,
    pty_id: u32,
    delivery_from: String,
    submit_sent_ms: u64,
    // #813: the stranded text this back-dated record is about, so a test can
    // drive the box reading the real decision now turns on.
    pasted_text: Option<String>,
) {
    last_delivery.lock_safe().insert(pty_id, DeliveryOutcome {
        confirmed: false,
        submit_sent_ms,
        from: delivery_from,
        stranded_text: pasted_text,
    });
}

/// Production bug fix (rev-42 delta, round 2): a per-agent snapshot of the
/// most recent delivery to that agent's pane, in the shape `compact_nudge_
/// tick`'s reinjection-confirmation resolver needs. Mirrors the private
/// `DeliveryOutcome` above (same two fields, same meaning) but kept as its
/// own `pub` type rather than making delivery-plumbing internals public: a
/// synthetic-input test constructs one directly, with no live pty/app
/// handle round trip — `deliver_prompt` is fire-and-forget and unit tests
/// can't exercise it for real (D1's rejected precedent, see the design
/// doc), so this is threaded through as an ordinary input map exactly like
/// `context_tokens`/`context_boundary_counts`, with `agent_last_deliveries`
/// as the impure reader that supplies it in production.
#[derive(Clone, Debug)]
pub struct DeliveryConfirmation {
    pub submit_sent_ms: u64,
    pub confirmed: bool,
    /// See `DeliveryOutcome::from`'s doc — mirrored verbatim.
    pub from: String,
}

/// Whether output growth after the submit Enter counts as the submit landing.
/// A successful submit clears the box and the CLI repaints / starts a turn (a
/// burst of output); an ignored Enter produces effectively none.
///
/// Only trustworthy when the pane reached quiet *before* the Enter. If the
/// submit-wait hit `SUBMIT_MAX_WAIT` while output was still streaming
/// (`reached_quiet == false`), the Enter landed mid-stream and the window's
/// growth is that stream, not the submit's — which would false-confirm and
/// strand a prompt recorded as confirmed (rev-32). So a cap-hit-without-quiet
/// is never confirmed; a false "unconfirmed" is just a harmless flush next
/// time. Pure so the rule is testable; the polling loop lives in
/// `deliver_prompt`.
pub fn submit_confirmed(reached_quiet: bool, baseline_total: u64, observed_total: u64) -> bool {
    reached_quiet && observed_total.saturating_sub(baseline_total) >= SUBMIT_CONFIRM_MIN_BYTES
}

// ─────────────────────── #112: real prompt-landed hook signal ───────────────────────
//
// `submit_confirmed` above trusts ANY output burst after Enter as evidence the
// prompt landed — error repaints, dialog interactions, and spinner ticks all
// clear its 24-byte bar, so the two live failures in #112's issue body were both
// recorded confirmed while the task was in fact destroyed (false confirm). The
// SAME missing signal produces the inverse failure too: a busy pane that never
// reaches quiet skips this heuristic entirely (`while reached_quiet && ...` in
// `deliver_prompt`), which is why 4 of 5 spawns drew a spurious "unconfirmed"
// notice in the #112 field-evidence comment even though every one had actually
// landed (false unconfirm).
//
// The fix is an AUTHORITATIVE signal, not a retuned threshold: Claude Code's
// `UserPromptSubmit` hook fires "when you submit a prompt, before Claude
// processes it" (code.claude.com/docs/en/hooks, "UserPromptSubmit input"
// section — fetched and grepped directly, not inferred), with no matcher
// (always fires) and the submitted text on stdin as JSON under the `prompt`
// field ("UserPromptSubmit hooks receive the `prompt` field containing the
// text the user submitted" — verbatim). `user_input`/`user_prompt` are
// tolerated as legacy/cross-CLI fallback field names only, never the
// documented one — round 1 review caught this module citing `user_input` as
// primary, which the live page does not contain at all. Copilot's
// `userPromptSubmitted` fires when "The user
// submits a prompt" (docs.github.com/en/copilot/reference/hooks-reference,
// "userPromptSubmitted" section) but that page does NOT document the payload
// TRANSPORT for this event (unlike Claude's stdin-JSON contract, which the
// hooks reference nails down explicitly) — so the Copilot arm never attempts to
// capture prompt text at all (see `PromptSubmitRecord`'s doc); it degrades to
// an existence+offset marker, still strictly better than the burst heuristic
// for a busy pane, at the cost of the content-match precision Claude's tier
// gets. This is a DOCS-SILENT residual, not an assumption papered over.
//
// A second docs-silent residual: whether a submission that gets swallowed/
// misparsed as an unknown slash command (the exact `/model`-merge failure
// #112's issue documents) still fires `UserPromptSubmit` at all. Neither
// reference page says. If it doesn't fire, that case stays *unconfirmed* under
// this design — the correct, safe direction (rev-32's own "never false-confirm"
// property, preserved) — so this is left unresolved rather than guessed at.
//
// SAFETY-CRITICAL per the hooks reference: exit code 2 on `UserPromptSubmit`
// "blocks prompt processing and erases the prompt", and on exit 0 anything
// printed to stdout "is added as context Claude can see". `COMPACT_HOOK_
// SCRIPT`'s new `promptsubmit` arm (below) is therefore held to the SAME
// unconditional-exit-0/no-stdout discipline the file's own doc already argues
// for `precompact`/`sessionstart-compact`, extended here to a case where the
// hazard of getting it wrong is destroying the user's own prompt, not merely
// skipping a compact.

/// A single `promptsubmit` hook record, read from `<agent-id>.promptsubmit.
/// jsonl` in the group's `hooks/` dir (#112) — one JSON line per
/// `UserPromptSubmit`/`userPromptSubmitted` firing since the marker file was
/// created, appended by the SAME generic hook script/Copilot command that
/// already write `.precompact.json`/`.sessionstart-compact.json` there (see
/// `COMPACT_HOOK_SCRIPT`'s doc).
///
/// `text` is `None` for every Copilot record and for an unparseable (e.g.
/// torn mid-write) line — see the module doc above for why Copilot's payload
/// transport is never read at all. A `None` record still counts as evidence
/// *something* fired (the existence tier — `PromptLandedMatch::Existence`);
/// it just can't be matched against what THIS delivery pasted.
#[derive(Debug, Clone, PartialEq, Eq)]
#[doc(hidden)] // pub for integration tests
pub struct PromptSubmitRecord {
    pub text: Option<String>,
}

/// Parse `content` (a `promptsubmit` marker file's full text) into the
/// records written since byte `offset` — the delivery's OWN baseline,
/// snapshotted before it pasted anything, so a record from an earlier
/// delivery to the same pane (or a human's own prompt) can never satisfy
/// THIS delivery's confirmation by construction. `offset` is clamped via
/// `str::get` rather than sliced directly: a torn read racing a concurrent
/// write could land mid-character on a multi-byte UTF-8 boundary, and an
/// out-of-bounds/invalid-boundary offset degrades to "no new records yet"
/// (an empty tail) rather than panicking the delivery thread.
///
/// Each non-empty line is one record. Valid JSON with a recognized text
/// field (`prompt` — the documented Claude field, per the "UserPromptSubmit
/// input" section of code.claude.com/docs/en/hooks: "UserPromptSubmit hooks
/// receive the `prompt` field containing the text the user submitted";
/// `user_input`/`user_prompt` tolerated as legacy/cross-CLI fallbacks only)
/// yields `text: Some(..)`.
/// Anything else non-empty (Copilot's existence-only marker line, or a
/// trailing line still mid-`>>` when this races the hook script's own
/// write) yields `text: None` rather than being dropped — losing a whole
/// poll cycle's worth of existence evidence to a torn read would be exactly
/// the false-unconfirm failure mode this feature exists to close.
#[doc(hidden)] // pub for integration tests
pub fn promptsubmit_records_since(content: &str, offset: usize) -> Vec<PromptSubmitRecord> {
    let tail = content.get(offset..).unwrap_or("");
    tail.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let text = serde_json::from_str::<Value>(line).ok().and_then(|v| {
                v.get("prompt")
                    .or_else(|| v.get("user_input"))
                    .or_else(|| v.get("user_prompt"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            });
            PromptSubmitRecord { text }
        })
        .collect()
}

/// Normalize prompt text for a landed-signal comparison: trim, collapse every
/// run of whitespace (including CR/LF, so CRLF-vs-LF and any TUI/JSON
/// re-wrapping wash out) to a single space. Deliberately NOT case-folding —
/// unlike an on-screen echo check (rejected in the design note precisely for
/// rendering fragility), this compares the raw text loomux itself pasted
/// against the JSON payload's OWN copy of that same text; case should already
/// agree, and folding it would only widen false positives.
fn normalize_prompt_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// [`normalize_prompt_text`], with each LINE [`deframe`]d first — the form the
/// Tier 1 box comparison runs in (#821).
///
/// **The defect.** Flattening a rendered tail whole puts the CLI's per-row
/// decoration *inside* the haystack, interleaved through whatever it decorated.
/// Copilot's framed composer draws `┃ ` down **every** row it wrapped a paste
/// onto, so `normalize_prompt_text` of that tail is `"┃ line one ┃ line two"`
/// while the paste it is compared against is `"line one line two"`. Containment
/// fails on text that is plainly still in the box.
///
/// **Applied to BOTH sides, and that is load bearing rather than tidy.** The
/// tempting version de-frames only the tail, on the reasoning that our own
/// paste carries no decoration. It is wrong, and wrong in the dangerous
/// direction: [`is_frame_char`] counts `*`, `•`, `●`, `◆` and `|` as framing,
/// so a brief containing an ordinary **markdown bullet list or table** would
/// have its `*`/`|` stripped out of the tail and kept in the needle. Every such
/// paste — and orchestrator briefs are full of them — would fail containment,
/// land past the length guard (the tail being longer, exactly as in the gutter
/// case), and read as a confident `NotHolding`. That is the very failure this
/// change exists to remove, re-introduced by the change itself. De-framing both
/// sides cancels it: the two lose the same characters, so containment is
/// unaffected and only specificity is spent.
///
/// The read-SIZING sites (`tier1_scan_bytes`, `Tier1Scan::for_paste`)
/// deliberately keep `normalize_prompt_text`: they size a request rather than
/// compare, so measuring the needle un-de-framed only ever asks for MORE tail
/// than the comparison needs, which is the conservative direction and preserves
/// #559's "no read is ever narrower than it was".
///
/// **Why `deframe` and not #820's full reconstruction.** `mask_own_paste`
/// stitches a wrapped line back together with [`reconstructs_to_end`], which is
/// strictly more precise — it verifies row STRUCTURE, not just the character
/// sequence — and strictly more brittle: any decoration it cannot account for
/// (a trailing gutter, the scrollbar's own `┃` on the right edge, a re-indent)
/// ends the run. There, brittleness was free: a failed reconstruction
/// under-masks, and an under-mask only costs a hold. **Here it is not.** This
/// feeds [`BoxReading`], where the expensive error is a confident `NotHolding`,
/// so the recogniser wants to be permissive about `Holds` and the residual
/// imprecision is handled by refusing to call a partial match an absence (see
/// [`box_reading`]). Same shared notion of what decoration is — one
/// [`deframe`], not a second copy of the rule — but a different instrument
/// built on it, because the failure direction is inverted.
fn normalize_deframed(s: &str) -> String {
    s.lines().flat_map(|l| deframe(l).split_whitespace()).collect::<Vec<_>>().join(" ")
}

/// The slice of a normalized tail that a needle of `needle_len` is looked for
/// within — the last `needle_len + BOX_TAIL_WINDOW_SLACK` characters.
///
/// One definition, because [`box_holds_paste`] and [`box_reading`]'s partial
/// probe must search the *same* window: a probe that ranged wider than the
/// containment it is qualifying could call a match partial on evidence the
/// containment was never allowed to see.
fn box_tail_window(norm_tail: &str, needle_len: usize) -> &str {
    let start = norm_tail.len().saturating_sub(needle_len + BOX_TAIL_WINDOW_SLACK);
    norm_tail.get(start..).unwrap_or(norm_tail)
}

/// The three tiers a delivery's `promptsubmit` hook records can resolve to
/// against the text it pasted, in ascending strength:
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)] // pub for integration tests
pub enum PromptLandedMatch {
    /// No record since baseline is any kind of evidence.
    None,
    /// A record fired since baseline, but its tier never captures text at
    /// all (every Copilot record today — see `PromptSubmitRecord`'s doc), so
    /// there is nothing to match content against. Trusted on the strength of
    /// the baseline alone: the hook is documented to fire unconditionally on
    /// every submission, and the baseline already excludes every record but
    /// the ones THIS delivery's own submit could have produced.
    Existence,
    /// A record's own (normalized) text CONTAINS this delivery's normalized
    /// paste — the strongest tier. `merged: true` means containment, not
    /// equality: the exact "prompt merged with human-typed `/model`" shape
    /// from #112's own issue body, still counted as landed (the agent DID
    /// receive the task text — plan-14 decision #3) but flagged so the audit
    /// can distinguish a clean submit from a merged one.
    Content { merged: bool },
}

/// Resolve `records` (already filtered to since-baseline by
/// `promptsubmit_records_since`) against `pasted` — the pure decision half
/// of the hook confirmation tier. An empty normalized `pasted` (should never
/// happen — `deliver_prompt` never pastes empty text) resolves to `None`
/// rather than trivially matching every record via an empty-string
/// containment check.
#[doc(hidden)] // pub for integration tests
pub fn prompt_landed(records: &[PromptSubmitRecord], pasted: &str) -> PromptLandedMatch {
    let norm_pasted = normalize_prompt_text(pasted);
    if norm_pasted.is_empty() {
        return PromptLandedMatch::None;
    }
    let mut existence = false;
    for r in records {
        match &r.text {
            Some(t) => {
                let norm_t = normalize_prompt_text(t);
                if norm_t.contains(&norm_pasted) {
                    return PromptLandedMatch::Content { merged: norm_t != norm_pasted };
                }
            }
            None => existence = true,
        }
    }
    if existence { PromptLandedMatch::Existence } else { PromptLandedMatch::None }
}

/// Which tier decided a delivery's outcome — carried into the `prompt-typed`
/// audit event (`confirm_source`) so every direction stays distinguishable
/// after the fact. #112 round 2 (three-state redesign — see the design note
/// section of the same name): `"box"`/`"box_veto"` are Tier 1 (two-sided,
/// only when its precondition verified — `box_holds_paste`'s doc); `"hook"`
/// is Tier 2 (the `promptsubmit` marker, in-window or late); `"burst"` is
/// Tier 3 (rev-32's output heuristic, last resort); `"idle"` is the extended
/// monitor's own trigger (pane genuinely quiet, no question on screen, still
/// no evidence — `PENDING_IDLE_QUIET`'s doc); `"none"` means nothing has
/// decided yet (the delivery is `DeliveryConfirmState::Pending`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmSource {
    Box,
    Hook,
    Burst,
    BoxVeto,
    Idle,
    None,
}

impl ConfirmSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ConfirmSource::Box => "box",
            ConfirmSource::Hook => "hook",
            ConfirmSource::Burst => "burst",
            ConfirmSource::BoxVeto => "box_veto",
            ConfirmSource::Idle => "idle",
            ConfirmSource::None => "none",
        }
    }
}

/// The three-state outcome a delivery resolves to (#112 round 2) — replacing
/// the old confirmed/unconfirmed binary. `Pending` is the state the round-1
/// design didn't have a name for: no evidence yet, which for a prompt queued
/// into a busy pane is the NORMAL, CORRECT state, potentially for a long
/// time, not a failure. Only `Failed` should ever draw the orchestrator's
/// unconfirmed notice — never `Pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryConfirmState {
    Confirmed,
    Pending,
    Failed,
}

impl DeliveryConfirmState {
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryConfirmState::Confirmed => "confirmed",
            DeliveryConfirmState::Pending => "pending",
            DeliveryConfirmState::Failed => "failed",
        }
    }
}

/// Map a decided `ConfirmSource` to its `DeliveryConfirmState` — pure so the
/// mapping itself is pinnable (a mutation swapping `BoxVeto`'s arm for
/// `Confirmed` would be exactly backwards, and a dedicated test catches it
/// directly rather than through some downstream consequence).
#[doc(hidden)] // pub for integration tests
pub fn confirm_state_for(source: ConfirmSource) -> DeliveryConfirmState {
    match source {
        ConfirmSource::Box | ConfirmSource::Hook | ConfirmSource::Burst => DeliveryConfirmState::Confirmed,
        ConfirmSource::BoxVeto | ConfirmSource::Idle => DeliveryConfirmState::Failed,
        ConfirmSource::None => DeliveryConfirmState::Pending,
    }
}

/// #112 round 3 (rev-20 B3): may Tier 1's own box-consumption reading be
/// trusted to decide anything RIGHT NOW? Any human input since OUR OWN
/// submit contaminates the reading in BOTH directions — a box that cleared
/// because the human typed/submitted/cancelled their own line reads
/// identically to one the CLI cleared for our delivery, and a box that's
/// STILL occupied because the human is mid-edit reads identically to one
/// the CLI never took. Once contaminated, Tier 1 must decline for the rest
/// of this delivery's decision (the delivery falls to `Pending`, and the
/// question-guarded late monitor decides from there) — this is what makes
/// the design note's "closed via `last_user_input_ms`" claim actually true
/// rather than aspirational.
#[doc(hidden)] // pub for integration tests
pub fn tier1_trusted(last_user_input_ms: u64, submit_sent_ms: u64) -> bool {
    last_user_input_ms <= submit_sent_ms
}

/// #518: how long the human-input block may stand on a keystroke TIMESTAMP
/// alone — no new keystroke evidence, and no human characters outstanding in
/// the box — before it is treated as stale. The delivery-hold sibling of
/// #500's `DEFAULT_IDLE_TICK_INPUT_DEFER_MAX_MINUTES`, and the same principle
/// a third time: a suppression driven by a fallible signal must be BOUNDED,
/// because nothing else will ever clear it.
///
/// Ten minutes, chosen against the delivery machinery's own longest legitimate
/// window rather than picked round: `REINJECT_CONFIRM_TIMEOUT_MS` (5 min) is
/// already documented as "comfortably" longer than `deliver_prompt`'s entire
/// worst-case hold chain (two `USER_QUIET_MAX_HOLD` waits plus
/// `SUBMIT_MAX_WAIT` and the echo/retry window), so 2x that cannot elapse
/// inside any single delivery attempt — this bound can only ever fire on a
/// pane that has genuinely been sitting still.
///
/// Deliberately NOT a per-group guardrail, unlike its #500 sibling. That one
/// is classification-BLIND by design (it caps the raw timestamp whatever
/// produced it), so a group whose humans really do sit typing for 20 minutes
/// has a legitimate reason to want it longer. This one releases only on
/// POSITIVE evidence that there is nothing of the human's to clobber
/// (`input_pending` false — see `human_input_block`), so there is no workflow
/// for which a longer value is more correct, and a knob with no correct second
/// setting is a knob that only ever gets set wrong.
pub const HUMAN_INPUT_BLOCK_BOUND_MS: u64 = 10 * 60 * 1000;

/// Whether the human-input block on a delivery still stands (#518).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HumanInputBlock {
    /// No keystroke evidence at all since our own submit — `tier1_trusted`.
    /// Nothing to block on; this is the ordinary case.
    None,
    /// Human keystroke evidence stands: either it is recent, or characters
    /// the human typed are still sitting in the box. Never bounded out.
    Blocked,
    /// #518: the evidence is a timestamp older than `bound_ms` AND the box
    /// holds no human-typed characters. The block is released, and — unlike
    /// `None` — the caller knows the bound is WHY, so it can say so in the
    /// audit instead of releasing silently.
    BoundedOut,
}

impl HumanInputBlock {
    /// The boolean the old inline `last_user_input_ms > submit_sent_ms`
    /// derivations produced — "a human typed since our submit, so do not
    /// touch this box". `BoundedOut` reads FALSE here: that is the whole
    /// point of the bound.
    pub fn holds(self) -> bool {
        matches!(self, HumanInputBlock::Blocked)
    }
}

/// The ONE derivation of "has a human typed into this pane since our own
/// submit?" (#518). Every delivery-path consumer used to inline
/// `ptys.last_user_input_ms(pty) > submit_sent_ms` — `tier1_trusted`'s
/// inverse — and that expression is an unbounded LATCH: a single stamp landing
/// after our submit pins it true for the entire life of the delivery and its
/// late monitor (up to `LATE_MONITOR_MAX_LIFETIME`, four hours). #496 PR-A
/// (#499) made a false stamp much rarer by gating the stamp itself on
/// keystroke evidence, but "rarer" is not "never": that gate is a byte-shape
/// classification over an OPEN set of terminal auto-reply shapes, and #496's
/// own plan §7 left "which copilot emission recurs mid-session" unresolved.
/// #518 is that residue firing — a copilot orchestrator's prompt sat
/// unsubmitted under a badge that no longer had any live fact behind it, and
/// only a human's physical Enter recovered the group.
///
/// So the latch gets an aggregate bound, and the bound releases on EVIDENCE,
/// not merely on elapsed time:
///
/// - `tier1_trusted` first: with no stamp after our submit there is nothing
///   to bound, and this returns `None` exactly as before.
/// - **`box_pending` outranks the bound.** If the pane's occupancy counter
///   (#111/#171, `PtyManager::input_pending`) says human-typed characters are
///   still sitting in the box, the block NEVER times out, however stale the
///   timestamp is. This is what keeps #510's absolute — never submit over
///   genuine human content — absolute: the bound cannot release while there
///   is any human content to submit over. `box_occupancy_delta`'s own doc
///   commits to the direction that makes this sound ("deliberately biased to
///   never UNDER-count real occupancy"), so a `false` reading here is the
///   trustworthy one.
/// - Only then does time matter: a timestamp older than `bound_ms`, with an
///   empty box, is a fact about the past and not about the pane now.
///
/// `bound_ms == 0` disables the bound (pre-#518 behaviour) rather than making
/// it fire instantly — a 0 that meant "always bounded out" would turn a
/// mis-set constant into the exact clobber this guard exists to prevent.
///
/// Pure, and returning a three-way rather than a bool, so the release is
/// auditable at the call sites that have a seam and directly pinnable by
/// tests — including that `box_pending` beats the bound, which is the one
/// property a future edit must not reorder.
#[doc(hidden)] // pub for integration tests
pub fn human_input_block(
    last_user_input_ms: u64,
    submit_sent_ms: u64,
    box_pending: bool,
    now_ms: u64,
    bound_ms: u64,
) -> HumanInputBlock {
    if tier1_trusted(last_user_input_ms, submit_sent_ms) {
        return HumanInputBlock::None;
    }
    if box_pending {
        return HumanInputBlock::Blocked;
    }
    if bound_ms == 0 {
        return HumanInputBlock::Blocked;
    }
    if now_ms.saturating_sub(last_user_input_ms) >= bound_ms {
        HumanInputBlock::BoundedOut
    } else {
        HumanInputBlock::Blocked
    }
}

/// Production wrapper: `human_input_block` against a live pane, with the
/// shipped bound. A closed pty reads `last_user_input_ms` as `0`, which
/// `tier1_trusted` already resolves to `None` before `box_pending` is
/// consulted at all — the `unwrap_or(true)` below is the fail-safe direction
/// for a reading we cannot take, not a case this can actually reach.
fn human_input_block_now(
    ptys: &crate::pty::PtyManager,
    pty_id: u32,
    submit_sent_ms: u64,
) -> HumanInputBlock {
    human_input_block(
        ptys.last_user_input_ms(pty_id).unwrap_or(0),
        submit_sent_ms,
        ptys.input_pending(pty_id).unwrap_or(true),
        now_ms(),
        HUMAN_INPUT_BLOCK_BOUND_MS,
    )
}

/// The audit `reason`/`release` token recorded when #518's bound — not an
/// absence of keystroke evidence — is why a human-input block was not
/// honoured. Its own string so a grep can tell "no human ever typed" apart
/// from "a human typed, and we decided that fact had gone stale".
const HUMAN_INPUT_BLOCK_BOUND_REASON: &str = "human-input-block-bound";

/// What a `DeclareFailed` tick should actually DO about the pane (#522).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnconfirmedDisposition {
    /// The pane is structurally idle — nothing of ours is sitting in the box,
    /// nothing of the human's is either, AND the pane produced a turn's worth
    /// of output since our Enter (#585). Record it and stop; do NOT tell the
    /// orchestrator to go re-send something that is not stuck.
    IdleAuditOnly,
    /// #585: the same empty, quiet box — but with NO turn evidence behind it.
    /// The pane never ran a turn on our text and our text is not there to run,
    /// which is the eaten-paste signature, not a finished pane. Announce it
    /// (with `delivery_eaten_notice`'s wording, not the "sitting unsubmitted"
    /// one — the text is gone, not stuck) and let the caller fall through to
    /// the strand path so the badge and `kickoff_recovery_action` are reached.
    EatenNotify,
    /// #539: the box was observed to no longer hold our text AND the agent's
    /// own process reached loomux after this delivery settled. The pane is not
    /// idle — it is working (and #585's turn evidence agrees: the pane painted
    /// a turn's worth of output since our Enter) — and the only thing still
    /// arguing for an alarm is `box_pending`, a signal known to latch true
    /// over an empty box. Reachable ONLY from the `box_pending` cell: an empty
    /// box with no human text is #585's territory, and activity must never
    /// silence an eaten delivery there. Record
    /// it and stop, but under its own name: "we watched the pane go quiet with
    /// an empty box" and "we watched the agent act" are different facts, and a
    /// timeline that showed them as one could not tell whether #539's evidence
    /// is doing any work.
    ActiveAuditOnly,
    /// Our text is still in the box, or the reading is indeterminate. This is
    /// the genuine strand the notice exists for — announce it unchanged.
    Notify,
}

/// What the `DeclareFailed` arm does once it has classified the pane (#585) —
/// the arm's own PRECEDENCE, lifted out of control flow and into a value.
///
/// **Why this exists at all.** #585 was not a wrong decision; it was a
/// correctly-decided one that a `return` statement preempted. The idle-pane
/// arm stopped the monitor ~90 lines above the test gating #517's kickoff
/// recovery, so the recovery never ran — and the entire test suite was green
/// throughout, because the ordering existed only as control flow inside a
/// function no test can execute (`late_monitor` needs a live `AppHandle` and
/// a real `PtyManager`).
///
/// #517's own wiring test shows the failure mode exactly: it hand-composes
/// `stranded_selfheal_action` and then `kickoff_recovery_action` in the order
/// its author believed the monitor used, and passes — asserting the intended
/// composition rather than the shipped one. A test that IS the composition
/// cannot observe that the real composition differs. That is the
/// "unpinnable wiring nobody could assert a property against" hazard this file
/// names elsewhere, and it cost this project two releases of a dead feature.
///
/// Making the precedence a value does not make `late_monitor` executable in a
/// test, and this is deliberately not claimed to: the arm could still be
/// mis-wired to ignore the route it computes. What it does buy is that the
/// ORDERING — "an eaten paste reaches the recovery; an idle pane does not" —
/// is now a property a test reads off a pure function instead of a property a
/// reader has to re-derive by tracing `return`s. The one thing that silently
/// regressed is the one thing now pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedArmRoute {
    /// Record the idle pane and stop. Nothing is stranded, nothing is owed.
    QuietStop,
    /// Announce, badge, and consult `kickoff_recovery_action`. `eaten`
    /// selects the notice wording only (see `delivery_eaten_notice`); both
    /// values take the identical path, which is the point — an eaten delivery
    /// must not be routed anywhere a merely-unconfirmed one is not.
    Escalate { eaten: bool },
}

/// The `DeclareFailed` arm's precedence (#585). See `FailedArmRoute`.
///
/// Total over `UnconfirmedDisposition`, so adding a variant without deciding
/// its route is a compile error rather than a silent fall-through into
/// whichever branch happens to sit first.
pub fn failed_arm_route(disposition: UnconfirmedDisposition) -> FailedArmRoute {
    match disposition {
        UnconfirmedDisposition::IdleAuditOnly => FailedArmRoute::QuietStop,
        // #539: silent for the same reason `IdleAuditOnly` is — the pane is
        // demonstrably not stranded — but reached on different evidence, so it
        // keeps its own audit action at the call site. The totality of this
        // match is what forced the new variant to be routed consciously
        // instead of falling through a wildcard into an escalation.
        UnconfirmedDisposition::ActiveAuditOnly => FailedArmRoute::QuietStop,
        UnconfirmedDisposition::EatenNotify => FailedArmRoute::Escalate { eaten: true },
        UnconfirmedDisposition::Notify => FailedArmRoute::Escalate { eaten: false },
    }
}

/// Should a late-confirmation failure raise the actionable "the prompt may be
/// sitting unsubmitted in its pane; get_output it and re-send" notice? (#522)
///
/// The notice was firing on panes that were simply DONE: a worker finished its
/// turn, went idle at the CLI's rest prompt, and the monitor — which only
/// knows that no `PromptSubmit` hook record ever showed up — announced a
/// strand that did not exist. Copilot has no such hook at all, and a claude
/// pane can miss one, so "no hook record" is a weak signal on its own. The
/// cost is not merely noise: every one of these tells the orchestrator to
/// `get_output` the pane (the #520 token flood on an animated pane) and tempts
/// a duplicate re-send — the double-delivery loop #451/#510 exist to prevent.
///
/// The disambiguation is STRUCTURAL, deliberately reusing the seams that
/// already exist rather than inferring anything from output bytes — the same
/// discipline #518's origin bit follows:
/// - `reading` — is OUR pasted text still identifiably at the box's
///   tail end? That, and only that, is what "sitting unsubmitted" means.
///   #559 made this a `BoxReading` rather than a bool: `Unverifiable` — the
///   tail we could read is shorter than the paste we are looking for — is
///   NOT an idle pane. It is no reading at all, and the whole point of this
///   function is to withhold an alarm only when the pane was actually
///   observed to be at rest. Treating a foregone `false` as an observation
///   is how a coalesced flush over the scan window went silent (see
///   `BoxReading`'s doc).
/// - `box_pending` — `PtyManager::input_pending`, whether any human-typed
///   characters are outstanding. A box with a person's half-written line in it
///   is not an idle pane, and the human still needs to hear about it.
/// - `turn_evidence` (#585) — did the pane produce `KICKOFF_TURN_EVIDENCE_
///   BYTES` of output since this delivery's own Enter? See below; this is the
///   input whose absence made every #517 recovery unreachable.
///
/// **#585: an empty box is two different panes, and #522 only modelled one.**
/// `NotHolding` on a quiet pane has two causes that are indistinguishable from
/// the reading alone:
/// - the CLI consumed our text, ran its turn, and came back to rest — #522's
///   case, where an alarm is noise; and
/// - the paste was never accepted at all (a blind paste into a CLI whose stdin
///   reader had not attached, or an Enter that landed on a busy CLI and was
///   dropped). The box is equally empty, and the delivery is equally gone —
///   except this one is a LOST message, and staying silent about it strands
///   the agent.
///
/// Live evidence (#585): across this project's whole recorded audit history,
/// `kickoff_recovery_action` — the #517/#526 recovery written specifically for
/// the second case — fired **zero** times and declined-with-a-reason zero
/// times, against 13 `delivery-unconfirmed-idle-pane` records and 3 fresh
/// kickoffs whose submit was never confirmed. One agent sat idle for 11
/// minutes on a kickoff nobody knew was lost. The reason is precedence, not
/// logic: this function's `IdleAuditOnly` arm returns from `DeclareFailed`
/// ~90 lines ABOVE the `NotHolding` test that gates the recovery, so the
/// recovery was reachable only when `NotHolding` coincided with a human's
/// half-typed line (`box_pending`) or with `Unverifiable`. It fired on a
/// coincidence, which is to say it did not fire. That is the same shape #559
/// fixed for `Unverifiable` — and #559 fixed only the lower half of it.
///
/// **The discriminator already existed; it was simply never consulted here.**
/// A pane that really did run a turn painted kilobytes since our Enter (the
/// monitor only looks after `PENDING_IDLE_QUIET` of total silence, so a turn
/// that happened has long since finished painting). An eaten paste leaves a
/// stray Enter on an empty box: a repaint, nothing more. That is exactly
/// `KICKOFF_TURN_EVIDENCE_BYTES`, and `kickoff_recovery_action` — 90 lines
/// below — already trusts it to gate the far more dangerous decision of
/// re-sending text into a live pane. **If the bar is good enough to authorise
/// a re-delivery, it is more than good enough to authorise a notice.** This is
/// not a weakening of #526's evidence bar and not a second mechanism beside
/// it: it is the same bar, newly applied to the path that was bypassing it.
///
/// **What this does NOT re-open.** #522's flood stays suppressed where #522
/// aimed it: a worker that finished a turn and went idle has turn evidence by
/// construction, so it still takes `IdleAuditOnly` and still says nothing. The
/// only deliveries that newly speak up are ones where the pane produced less
/// than a turn's output since our Enter AND the box no longer holds our
/// text — which is not a finished pane under any reading.
///
/// - `agent_acted` (#539) — `agent_acted_since(last_mcp_activity_ms,
///   submit_sent_ms + UNCONFIRMED_ACK_SETTLE_MS)`: the agent's OWN process
///   authenticated with its token and invoked a loomux tool after this
///   delivery settled. #535 built this clock and named this function as its
///   intended second consumer; until then the detector had no evidence about
///   the agent at all, only about the box, which is why panes that had
///   reported and pushed within the same minute still drew the alarm.
/// - `kickoff_recoverable` (#539) — whether this delivery is a fresh spawn's
///   kickoff that #517 could still re-deliver.
///
/// **The merged table (#585 + #539).** These two landed against the same
/// function within hours of each other and both split the `NotHolding` row, so
/// the composition is stated here in full rather than left to be inferred from
/// match order:
///
/// | `reading` | `box_pending` | `turn_evidence` | `agent_acted` | `kickoff_recoverable` | result |
/// |---|---|---|---|---|---|
/// | `Holds` | any | any | any | any | `Notify` |
/// | `Unverifiable` | any | any | any | any | `Notify` |
/// | `NotHolding` | `true` | `true` | `true` | `false` | `ActiveAuditOnly` (#539) |
/// | `NotHolding` | `true` | `true` | `true` | `true` | `Notify` (kickoff veto) |
/// | `NotHolding` | `true` | otherwise | | | `Notify` |
/// | `NotHolding` | `false` | `true` | — | — | `IdleAuditOnly` (#585) |
/// | `NotHolding` | `false` | `false` | — | — | `EatenNotify` (#585) |
///
/// The `—` are not shorthand for "any": `agent_acted` is **structurally
/// absent** from the `box_pending: false` arms, and that is the load-bearing
/// half of this merge.
///
/// **Why AND, not OR — correcting this doc's own earlier prediction.** #585
/// shipped anticipating that #539 would compose as *evidence-OR* ("any
/// independent sign that the agent acted on our delivery justifies silence").
/// That is not what shipped, and the difference matters in exactly the cell
/// #585 exists to protect. An MCP call is **not** a sign that the agent acted
/// on *our delivery* — it is a sign the agent's process is alive. An agent
/// whose paste was eaten still calls loomux: every role's instructions end
/// with "if you have no task yet, report progress and wait". So under OR, a
/// lost kickoff on a pane with no turn evidence would be silenced by the very
/// report that proves the agent never got its brief — re-deading the recovery
/// #585 had just un-deaded, from a different input.
///
/// Hence two rules, both restrictions rather than extensions:
/// - `agent_acted` is read **only** in the `box_pending` cell. The
///   `IdleAuditOnly`/`EatenNotify` split below is #585's alone.
/// - Inside that cell it must be accompanied by `turn_evidence`. Silence
///   requires the pane to have *painted a turn since our Enter* AND the agent
///   to have *reached loomux after it settled* — two independent post-submit
///   observations, neither of which the other can manufacture. The panes this
///   was built for (#539's ~28 false alarms, agents that "had reported and
///   pushed within the same minute") satisfy both by construction, so the fix
///   loses nothing it was aimed at.
///
/// The honest summary is that the two changes compose as evidence-AND within
/// one cell and as disjoint ownership across the rest of the row — which is
/// stricter than either author predicted alone, and strictly safer than both.
///
/// "CLI at rest / turn complete" is the third condition #522 names, and it is
/// already a PRECONDITION of ever reaching this decision rather than an input
/// here: `late_monitor_tick` returns `DeclareFailed` only when
/// `quiet_long_enough` (no `output_total` growth for `PENDING_IDLE_QUIET`) and
/// no question is on screen. A pane mid-turn keeps `output_total` creeping —
/// spinner and statusline frames included (#480) — so it never gets this far.
/// Taking it as an input anyway would let a caller pass `true` for a pane that
/// is demonstrably busy, inventing a state the surrounding code cannot
/// produce.
///
/// **Precedence, and what #539 deliberately does NOT touch.** Activity is
/// added as evidence; it never overrides evidence that points the other way:
///
/// | `reading` | `box_pending` | `agent_acted` | result |
/// |---|---|---|---|
/// | `Holds` | any | any | `Notify` |
/// | `Unverifiable` | any | any | `Notify` |
/// | `NotHolding` | `true` | `true` | `ActiveAuditOnly` (#539) |
/// | `NotHolding` | `true` | `false` | `Notify` |
/// | `NotHolding` | `false` | any | `IdleAuditOnly` |
///
/// - `Holds` is a direct observation that our text is sitting unsubmitted.
///   An agent can be busy for reasons that have nothing to do with our paste,
///   so liveness must not silence a strand we can see.
/// - `Unverifiable` is #559's honest-uncertainty arm and stays untouched. We
///   have no box reading at all there, and activity is not a reading of the
///   box: suppressing on it would re-create exactly the defect #559 fixed
///   (silence drawn from an answer the pane was never consulted about), just
///   sourced from a different signal.
/// - So activity governs precisely ONE cell — the one where the box was
///   observed to have lost our text and the only remaining argument for an
///   alarm is `box_pending`, i.e. `PtyManager::input_pending`, which is known
///   to latch >0 over an empty box (bare ESC, a TUI line-clear, a CLI
///   consuming the line — see the design note's input-origin section). Two
///   independent readings — the box no longer holds our text, and the agent
///   itself reached loomux afterwards — is the same "release on a second,
///   independent reading" shape #518 used, not a timer.
///
/// **The suppression is bounded by construction** (`.loomux/lessons.md`: any
/// suppression driven by a fallible signal must be BOUNDED). It is not a hold
/// that waits for a condition to clear, so there is no "what if the signal is
/// wrong and never clears" state to get stuck in: it is a one-shot decision
/// taken against a POSITIVE stamp that must already exist. No stamp, no
/// suppression — a pane with no activity signal keeps its pre-#539 behaviour
/// exactly. The activity evidence IS the bound.
///
/// **Why `kickoff_recoverable` vetoes the suppression.** #517's lost-kickoff
/// re-delivery is reached through the notify path on a `NotHolding` reading,
/// and a spawn whose brief never arrived is precisely an agent that will call
/// a loomux tool anyway: every role's instructions end with "if you have no
/// task yet, report progress and wait". So on a kickoff, an activity stamp is
/// as consistent with "the brief was eaten and the agent announced itself
/// idle" as with "the brief landed" — the one case where this evidence points
/// at nothing. Rather than let it silence the only recovery a fresh spawn has,
/// a recoverable kickoff always takes the unchanged path.
///
/// Note what this does NOT do: it does not mark the delivery confirmed. An
/// idle-pane reading is good enough to withhold an alarm, not to assert
/// something landed, and leaving the ledger `unconfirmed` keeps every
/// downstream behaviour exactly as it was — including the next delivery's
/// stranded flush, whose own doc already blesses this residual ("A false
/// 'unconfirmed' here is safe: the flush Enter lands on an already empty box
/// and is a no-op").
#[doc(hidden)] // pub for integration tests
pub fn unconfirmed_disposition(
    reading: BoxReading,
    box_pending: bool,
    turn_evidence: bool,
    agent_acted: bool,
    kickoff_recoverable: bool,
) -> UnconfirmedDisposition {
    match reading {
        // Our text is still in the box — the genuine strand.
        BoxReading::Holds => UnconfirmedDisposition::Notify,
        // #559: no reading. Silence here would be a claim ("the pane is
        // idle") drawn from an answer that was fixed before the pane was
        // consulted. #539 does not weaken this: activity is evidence about
        // the agent, never about the box.
        BoxReading::Unverifiable => UnconfirmedDisposition::Notify,
        // Observed empty of our text — idle only if it is empty of the
        // human's too.
        // #539 + #585, merged deliberately (see this function's doc). The
        // human-input cell is the ONLY one activity governs, and it now
        // requires turn evidence as well — see "Why AND, not OR" in the doc.
        BoxReading::NotHolding if box_pending => {
            if agent_acted && turn_evidence && !kickoff_recoverable {
                UnconfirmedDisposition::ActiveAuditOnly
            } else {
                UnconfirmedDisposition::Notify
            }
        }
        // #585: and only if the pane can show it actually ran a turn. This is
        // the cell that swallowed every lost kickoff this feature exists to
        // catch — see this function's doc. #539 does NOT reach past here:
        // `agent_acted` is deliberately absent from both arms below, because
        // an agent that calls a tool while our paste is missing is the
        // SYMPTOM of the eaten delivery, not evidence against it.
        BoxReading::NotHolding if turn_evidence => UnconfirmedDisposition::IdleAuditOnly,
        BoxReading::NotHolding => UnconfirmedDisposition::EatenNotify,
    }
}

/// #112 round 3 (rev-20 B2 + B3): the ONE decision point for whether Tier
/// 1's accumulated reading may become an authoritative veto
/// (`ConfirmSource::BoxVeto`) at the end of the confirm+retry window. Pure
/// so the polarity — arguably the single most important property in this
/// redesign — is directly pinnable rather than an inline `if` a future edit
/// could silently invert (exactly the shape round 1 shipped and failed
/// live: unpinnable wiring nobody could assert a property against).
///
/// A veto requires ALL of:
/// - nothing else already decided this delivery (`confirm_source ==
///   ConfirmSource::None`);
/// - Tier 1 governs this delivery at all (`tier1_governs`);
/// - Tier 1's own reading, at the end, was "still holding"
///   (`tier1_reading == Some(true)`);
/// - the confirm+retry window ran to NATURAL exhaustion —
///   `window_exhausted_naturally`. An early exit for a question on screen,
///   a human typing, or a failed retry write all mean the box's state
///   cannot be trusted as evidence of NON-acceptance: a question may be
///   intercepting Enter rather than refusing it, and a human mid-edit means
///   the box's contents aren't about our delivery either way. B2's fix:
///   ANY early exit leaves the delivery `Pending`, never `Failed` — the
///   question-guarded late monitor (`late_monitor_tick`) is what's allowed
///   to decide from there, because unlike this one-shot end-of-window
///   check, it re-observes the question state on every subsequent poll;
/// - Tier 1 is still trusted at the end (`tier1_trusted_at_end` —
///   B3's fix: no human input since our own submit).
///
/// Never returns anything OTHER than the input `confirm_source` unchanged
/// when any condition fails — this function can only ever produce
/// `BoxVeto`, never invent a different outcome or downgrade an existing
/// one.
#[doc(hidden)] // pub for integration tests
pub fn final_window_outcome(
    confirm_source: ConfirmSource,
    tier1_governs: bool,
    tier1_reading: Option<bool>,
    window_exhausted_naturally: bool,
    tier1_trusted_at_end: bool,
) -> ConfirmSource {
    if matches!(confirm_source, ConfirmSource::None)
        && tier1_governs
        && tier1_reading == Some(true)
        && window_exhausted_naturally
        && tier1_trusted_at_end
    {
        ConfirmSource::BoxVeto
    } else {
        confirm_source
    }
}

/// #112 round 3 (rev-20 B1): one tick's decision for the late-confirmation
/// monitor — pure, so the precedence is directly pinnable rather than an
/// inline `if`/`continue` chain a future edit could silently reorder.
///
/// - `Superseded` — a NEWER delivery to the same pane has recorded its own
///   `DeliveryOutcome` since this monitor started (`submit_sent_ms` no
///   longer matches what's in `last_delivery`): this monitor no longer owns
///   anything and must exit WITHOUT writing or notifying — that's precisely
///   the clobber/false-correction hazard (rev-20 B1) this variant exists to
///   prevent. Checked first: even a hook match arriving on a superseded
///   monitor's tick would be writing a confirmation for the WRONG delivery.
/// - `Confirm` — a `promptsubmit` match arrived. `correction: true` when
///   `already_failed` (this is upgrading an alarm that already fired, so
///   the orchestrator needs the correction notice, not a second success
///   notice); checked before `Expired` so a match on the very last tick
///   before the cap still resolves the delivery rather than timing out.
/// - `Expired` — the lifetime cap was hit with nothing resolved.
/// - `KeepWaiting` — nothing to act on: either already `failed` (only a
///   hook match, handled above, has anything left to do) or not yet quiet
///   long enough / a question is on screen.
/// - `DeclareFailed` — genuinely idle, no question, not yet failed: the
///   ONE point this delivery is ever declared `Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)] // pub for integration tests
pub enum MonitorAction {
    Superseded,
    Expired,
    Confirm { merged: bool, correction: bool },
    DeclareFailed,
    KeepWaiting,
}

#[doc(hidden)] // pub for integration tests
pub fn late_monitor_tick(
    superseded: bool,
    hook_match: PromptLandedMatch,
    already_failed: bool,
    expired: bool,
    quiet_long_enough: bool,
    showing_question: bool,
) -> MonitorAction {
    if superseded {
        return MonitorAction::Superseded;
    }
    if !matches!(hook_match, PromptLandedMatch::None) {
        let merged = matches!(hook_match, PromptLandedMatch::Content { merged: true });
        return MonitorAction::Confirm { merged, correction: already_failed };
    }
    if expired {
        return MonitorAction::Expired;
    }
    if already_failed {
        return MonitorAction::KeepWaiting;
    }
    if quiet_long_enough && !showing_question {
        return MonitorAction::DeclareFailed;
    }
    MonitorAction::KeepWaiting
}

// ────────────── #496 PR-C: actuation on a stranded delivery ──────────────
//
// #451 gave a delivery three honest states and #445/#470 made a held payload
// queued-never-destroyed — but nothing ACTS on `Failed`. For an
// orchestrator-target delivery both notices are suppressed by design (loop
// avoidance — `should_notify_unconfirmed`/`should_notify_paste_held`), and in
// an idle group there is no NEXT delivery whose own pre-paste flush would
// eventually press the withheld Enter. The prompt then sits in the box until
// a human notices and presses Enter by hand — the human being the recovery
// mechanism, which is the defect #496 is about (observed on Claude panes as
// well as copilot ones: the root cause is CLI-agnostic).
//
// The guarantee this section adds: **a delivery ends Confirmed, or a bounded
// self-heal fires, or a human-visible attention badge is raised. No path
// ends silent-and-wedged.**
//
// Two deliberate non-inventions:
// - The re-submit is NOT a new delivery and NOT a raw write from the monitor
//   thread. It is admitted as a `StrandedSubmit` marker at the FRONT of the
//   pane's queue (`enqueue_stranded_front`) and pressed by the drainer —
//   the same single-consumer path #470 made the only way anything reaches a
//   pane, and the same marker `AbortedPreEnter` already uses. Front, not
//   back, because the stranded text is physically in the box already:
//   anything queued behind it must not paste on top of it. A raw write here
//   would race the drainer mid-paste and re-open the ordering hole #470
//   closed.
// - The guardrails are re-used, not re-implemented: `drain_stranded_submit`
//   → `flush_stranded_text` re-derives `human_typed_since` from the ledger
//   and re-reads the live question state (#420) at the instant of the press.
//   `stranded_selfheal_action` below is the TRIGGER gate, deciding whether a
//   heal is worth admitting at all; it never becomes the last word on
//   whether the Enter is safe.

/// Why a stranded delivery could not be self-healed (#496 PR-C). Carried into
/// the attention badge so "this pane needs you" always says what is in the
/// way, rather than leaving the human to work it out from the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrandedBlocker {
    /// A human has typed into the pane since our own submit, so the box may
    /// hold THEIR line. Never merge-submit over human-typed content — the
    /// same rule `should_flush_before_paste` has enforced since #81/#84.
    HumanInput,
    /// A live interactive question owns the Enter key (#420): pressing it
    /// would answer the question, not submit our prompt.
    Question,
    /// Our pasted text is no longer at the box's tail end, so there is
    /// nothing identifiable to re-submit — the delivery still failed, and
    /// the human still needs to know, but loomux must not press Enter into
    /// a pane whose state it can no longer account for.
    NotHolding,
    /// #559: loomux could not read enough of the pane's tail to contain the
    /// text it pasted (`BoxReading::Unverifiable`), so whether the prompt is
    /// still sitting in the box is unknown. Deliberately NOT folded into
    /// `NotHolding`: that variant's wording tells the human their text is
    /// *gone*, which here would be a claim from a `false` that was arithmetic
    /// rather than observation. Same non-action as `NotHolding` — loomux
    /// never presses Enter into a pane it cannot account for — but an honest
    /// name for a different reason, so a badge, an audit line and a grep can
    /// all tell "we looked and it is gone" from "we could not look".
    Unverifiable,
    /// The per-delivery heal budget (`STRANDED_SELFHEAL_MAX_HEALS`) is spent.
    Exhausted,
    /// The pane's delivery queue is at cap, so the re-submit could not even
    /// be queued (rev-47 NB4). Deliberately NOT folded into `Exhausted`:
    /// that one's wording says a heal already fired and did not take, which
    /// would be a false claim here — nothing was attempted at all.
    QueueFull,
    /// #563: the pane's delivery queue has reached `queue::QUEUE_NEAR_FULL_AT`
    /// — nothing has been lost yet, and the point of the badge is that it says
    /// so *before* anything is.
    ///
    /// **Why this is a separate variant and not `QueueFull`.** `QueueFull`'s
    /// wording asserts that loomux *could not queue* something, which would be
    /// a false claim while the queue is still accepting — the same
    /// unbacked-claim class that made `QuestionStale` a separate variant from
    /// `Question` and `QueueFull` a separate one from `Exhausted`. The two
    /// also want different actions from the human: `QueueFull` says "work has
    /// already been dropped, go look"; this one says "release the pane and
    /// nothing will be".
    ///
    /// **Why a badge and not only a notice.** The in-band channel
    /// (`notify_queue`) is suppressed whenever the target is the group's own
    /// orchestrator, which is precisely the pane #563 was reported on. A badge
    /// has no such suppression, so this is the channel that survives the case
    /// that matters. Released on evidence — the depth falling back to
    /// `CapacityState::Normal` — never on a timer.
    QueueNearFull,
    /// #563: the pane's queue has reached `queue::QUEUE_MAX_PER_PANE`, read
    /// from the DEPTH and nothing else.
    ///
    /// **Why this is not `QueueFull` (rev-10 finding 1, applied consistently).**
    /// `QueueFull`'s wording says loomux *could not even queue a re-send* and
    /// tells the human to *press Enter in the pane*. Both are true where it is
    /// raised, from `actuate_stranded`: there a self-heal really was refused
    /// and stranded text really is sitting in the box. Neither is established
    /// by a depth reading — `note_queue_capacity` never consults hold state,
    /// and the transition INTO `Full` fires on the admission that took the last
    /// slot, before anything has been rejected at all. Reusing `QueueFull`
    /// there would send a human to press Enter on a pane with nothing to
    /// submit: the same false-claim class this issue exists to eliminate, and
    /// the same reason `QuestionStale` was minted rather than reusing
    /// `Question`.
    ///
    /// Says what depth establishes and then stops: the queue is at the cap, so
    /// arrivals are being dropped. Released on the same evidence
    /// `QueueNearFull` is — a depth that has actually come back down.
    QueueAtCapacity,
    /// #532: the interactive-question guard has held this pane's delivery for
    /// longer than [`QUESTION_HOLD_STALE_AFTER`], and loomux cannot tell
    /// whether the question it still detects is real.
    ///
    /// **Why this is a badge and never a release.** `prompt_wait_detected`
    /// reads `PtyManager::output_tail_bounded`, which is an append-only byte
    /// RING, not a screen: an answered question stays inside the last
    /// `QUESTION_SCAN_TAIL_BYTES` on a pane that has gone quiet, so the
    /// structured signals it matches on ("(y/n)", "do you want to proceed")
    /// keep matching indefinitely. That detector's own doc says as much — "this
    /// alone can't tell a live prompt from the same words scrolled past" — and
    /// the #420 hold is the one consumer that pairs it with nothing. From bytes
    /// alone the two states are identical, so there is no reading that could
    /// justify releasing.
    ///
    /// The asymmetry decides what to do about that. A prompt held too long is
    /// recoverable by a human who is *told* about it; an Enter that
    /// auto-answers a live consent dialog is not recoverable at all — it
    /// silently steers the agent, which is the exact harm #420 exists to
    /// prevent. So the bound stops the hold re-arming *silently* and names the
    /// staleness hypothesis to the human; it never converts into a write. See
    /// [`held_escalation`].
    ///
    /// **Which human, and when (rev-12 NB4).** Be exact about the channel this
    /// argument rests on, because it is narrower than "told". The telling is
    /// `mark_stranded` — the `attn_stranded` badge (a chip in the desktop
    /// window) plus an audit line, the established #496 PR-C channel. It is
    /// **not** an in-band notice to any agent: `notify_queue` returns early
    /// with `notice-suppressed` whenever `target_is_orchestrator`, which is
    /// exactly the pane in #532's own incident. So on an attended session the
    /// human sees the chip; on an unattended overnight run **nobody is told
    /// until someone next looks at the window**, which is what happened. That
    /// does not change the release/badge decision — an unattended run is
    /// precisely where a wrong Enter is least recoverable — but "a human is
    /// told" should not be read as "somebody is paged".
    ///
    /// The approach that could answer what this variant can only ask is
    /// [`termgrid`] (#530): rendered rows, where "still displayed" is a
    /// real reading. Note it is NOT a drop-in — `termgrid::render_screen`
    /// returns scrolled-off history rows *followed by* the on-screen rows, so
    /// pointing the detector at it unchanged would reproduce this same bug.
    /// See `docs/design/orchestration.md`'s #532 section for what the follow-up
    /// actually needs.
    QuestionStale,
    /// #569: deliveries aimed at this pane were DISCARDED while the group was
    /// paused, and the resume-time notice that would have said so could not be
    /// delivered to the group's orchestrator.
    ///
    /// **Two ways to earn it, and only one of them is history** (review B2).
    /// Option 2 (enqueue-while-paused) removed the pause branch's discard, so
    /// the LEGACY cause needs a group paused under an older loomux. But a pane
    /// already at `queue::QUEUE_MAX_PER_PANE` still has admissions refused for
    /// as long as a pause lasts, and that loses a payload on THIS build — see
    /// `SuppressedCause` and `announce_pause_suppression`.
    ///
    /// **Why this is a badge and not only the notice.** The notice
    /// (`announce_pause_suppression`) is an in-band delivery to the
    /// orchestrator, so it fails exactly when a paused group has been left long
    /// enough for its orchestrator to idle out or be killed — which is the
    /// longest, most damaging pause, not the mildest. A notice that goes
    /// missing precisely in the worst case is the same silent-loss shape #569
    /// exists to close, one level up. `mark_stranded` has no role suppression
    /// and needs no orchestrator at all, so it is the channel that survives
    /// (the #563 argument, applied to a different hold).
    ///
    /// **It claims only what the audit record establishes**: that something
    /// addressed to this pane was thrown away, and that nothing is queued to
    /// arrive. It does NOT say the pane is held, that text is sitting
    /// unsubmitted in its box, or that loomux is re-sending anything — none of
    /// which a suppression record shows, and each of which is some other
    /// variant's sentence (`HumanInput`, `Exhausted`, the `None` heal wording).
    /// Cleared like any other badge, by `clear_stranded`.
    PauseSuppressed,
}

impl StrandedBlocker {
    /// Stable audit token — the string that lands in the audit log, kept
    /// separate from the human-facing badge text so one can change without
    /// silently breaking greps over the other.
    pub fn as_str(self) -> &'static str {
        match self {
            StrandedBlocker::HumanInput => "human-input",
            StrandedBlocker::Question => "question",
            StrandedBlocker::NotHolding => "not-holding",
            // #559: its own token so a grep tells "we read the box and our
            // text is gone" from "we could not read enough of the box to
            // tell" — the distinction the whole change exists to preserve.
            StrandedBlocker::Unverifiable => "box-unverifiable",
            StrandedBlocker::Exhausted => "heal-budget-spent",
            StrandedBlocker::QueueFull => "queue-full",
            StrandedBlocker::QueueNearFull => "queue-near-full",
            StrandedBlocker::QueueAtCapacity => "queue-at-capacity",
            StrandedBlocker::QuestionStale => "question-hold-stale",
            StrandedBlocker::PauseSuppressed => "pause-suppressed",
        }
    }
}

/// What to do about a delivery the late monitor just declared `Failed`
/// (#496 PR-C).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrandedAction {
    /// The ledger says this delivery is no longer outstanding (confirmed, or
    /// superseded by one that was). Nothing to heal and nothing to badge.
    Resolved,
    /// Safe: admit a bounded re-submit through the delivery queue.
    SelfHeal,
    /// Not safe (or budget spent): raise the pane's attention badge instead
    /// of waiting silently.
    Attention(StrandedBlocker),
}

/// How many self-heal submits loomux may fire for ONE stranded delivery
/// (#496 PR-C). One, deliberately: the heal is an Enter into a pane whose
/// state we can only observe indirectly, and the failure mode of pressing it
/// twice (a second Enter landing on whatever the first one opened) is worse
/// than the failure mode of not pressing it again (a badge the human
/// already has in front of them). The bound is also structural today —
/// `late_monitor_tick` returns `DeclareFailed` at most once per monitor,
/// since every later tick sees `already_failed` — but that is the *caller's*
/// precedence, and a future edit could change it without noticing this;
/// counting explicitly means the cap survives that edit and is directly
/// testable rather than inferred.
pub const STRANDED_SELFHEAL_MAX_HEALS: u32 = 1;

/// The one decision point for #496 PR-C: given everything known about a
/// just-`Failed` delivery, self-heal or badge? Pure, so the precedence — and
/// in particular that the human-content guard is checked FIRST — is directly
/// pinnable rather than an inline `if` chain a future edit could reorder
/// (the argument `final_window_outcome` and `late_monitor_tick` already make
/// in this file).
///
/// Inputs, in the order they are consulted:
/// - `ledger_outstanding` — the DURABLE artifact, not a pane heuristic: this
///   pane's recorded `DeliveryOutcome` is still THIS delivery's and still
///   unconfirmed. Anything else means the ledger already moved on (a heal
///   that landed, a newer delivery that confirmed) and this monitor has
///   nothing to actuate. Checked first so no pane reading can ever override
///   what the ledger says about whether a delivery is still outstanding.
/// - `human_typed_since` — a human keystroke landed after our submit
///   (`tier1_trusted`'s inverse). The one absolute: loomux never submits
///   over human-typed content, so this outranks every other input including
///   the heal budget.
/// - `question_on_screen` — #420's guard. False by construction at today's
///   only call site (`late_monitor_tick` will not declare failure while a
///   question is up), and re-checked authoritatively at press time by
///   `flush_stranded_text`; taken as a parameter anyway so this function
///   does not silently depend on the caller's precedence staying that way.
/// - `reading` — what Tier 1's box read established (`BoxReading`). #559 made
///   this a three-state rather than a bool: `Unverifiable` badges as its own
///   blocker instead of borrowing `NotHolding`'s "its text is gone" claim.
///   Both refuse to self-heal, so this widens nothing about when loomux
///   presses Enter — the ONE thing that could make a badge-only distinction
///   dangerous — it only stops the badge from asserting what loomux does not
///   know.
/// - `heals_used` / `max_heals` — the bound.
pub fn stranded_selfheal_action(
    ledger_outstanding: bool,
    human_typed_since: bool,
    question_on_screen: bool,
    reading: BoxReading,
    heals_used: u32,
    max_heals: u32,
) -> StrandedAction {
    if !ledger_outstanding {
        return StrandedAction::Resolved;
    }
    if human_typed_since {
        return StrandedAction::Attention(StrandedBlocker::HumanInput);
    }
    if question_on_screen {
        return StrandedAction::Attention(StrandedBlocker::Question);
    }
    match reading {
        BoxReading::Unverifiable => {
            return StrandedAction::Attention(StrandedBlocker::Unverifiable);
        }
        BoxReading::NotHolding => return StrandedAction::Attention(StrandedBlocker::NotHolding),
        BoxReading::Holds => {}
    }
    if heals_used >= max_heals {
        return StrandedAction::Attention(StrandedBlocker::Exhausted);
    }
    StrandedAction::SelfHeal
}

// ───────────── #517: a fresh spawn's kickoff that never reached the pane ─────────────
//
// #496 PR-C (above) recovers a delivery whose text IS in the box and whose
// Enter was withheld. A fresh spawn's kickoff fails a DIFFERENT way: the
// paste itself is swallowed by a CLI whose stdin reader has not attached yet
// (`ECHO_WINDOW`'s own comment: "observed live with copilot, whose input
// attaches well after its UI paints"), so nothing ever lands in the box.
// `stranded_selfheal_action` correctly refuses that state — `NotHolding`,
// "there is nothing identifiable to re-submit" — and today the story ends
// there: a badge, a notice, and a worker sitting idle with no brief until a
// human or the orchestrator re-sends by hand. That is #517: six instances in
// one day, and the reason the agent's own recovery ("re-send with
// send_prompt") is a person's job.
//
// The recovery for THIS shape is not an Enter, it is the brief again — and
// it goes through `deliver_prompt`'s own front door (#470), not a write from
// the monitor thread, so it inherits ordered admission, the byte-identical
// coalesce, the audit trail, and every paste guard. The kickoff was never
// outside that machinery; only its FAILURE was.
//
// **Why this cannot double-deliver a kickoff that actually landed.** Three
// independent layers, in the order they are consulted:
//   1. A landed kickoff resolves before it ever gets here — `late_monitor_
//      tick` returns `Confirm` on the `promptsubmit` hook record (installed
//      for every spawned agent, both CLIs, at spawn) and the monitor exits.
//      `DeclareFailed` is unreachable for it.
//   2. `ledger_outstanding` — the durable artifact, not a pane heuristic.
//   3. `output_since_submit` — a kickoff that landed makes the agent run its
//      whole first turn; one that was eaten leaves the pane exactly as boot
//      left it. This is the observed discriminator from the live data: two
//      fresh spawns lost their brief and sat silent while a third received
//      its brief and worked normally.
// Plus the queue's own coalesce, which collapses a re-admission of
// byte-identical text into an entry still waiting, and a budget of one.

/// How many re-deliveries loomux may fire for ONE lost fresh kickoff (#517).
/// One, for the same reason `STRANDED_SELFHEAL_MAX_HEALS` is one: the
/// recovery acts on a pane whose state we can only observe indirectly, and
/// the cost of a second wrong re-delivery (a duplicate brief the agent has
/// to reconcile) is worse than the cost of stopping (a badge and a notice
/// the human already has in front of them). Counted explicitly rather than
/// relied on structurally, so the bound survives an edit to the caller's
/// precedence and is directly testable.
pub const KICKOFF_REDELIVERY_MAX: u32 = 1;

/// Output growth since a kickoff's own submit baseline that counts as "this
/// agent really did receive its brief and start a turn" (#517) — the guard
/// that makes re-delivery safe.
///
/// Sized to sit far above the residual an EATEN kickoff can produce and far
/// below what a LANDED one produces. A landed kickoff makes the CLI echo the
/// brief into its transcript and begin a reply, and the monitor only ever
/// looks after `PENDING_IDLE_QUIET` of total silence — so a turn that
/// happened has painted kilobytes by then. (Review F3: an earlier version of
/// this comment also claimed a kickoff brief is itself larger than this. It
/// is not — real briefs run ~1-2 KB — and the discriminator never rested on
/// that; the claim is deleted rather than softened.) An eaten kickoff leaves
/// a stray Enter on an empty box: a repaint, nothing more. The asymmetry is
/// deliberate — this bar being too HIGH only declines a recovery that was
/// needed (falling back to today's badge, the pre-#517 behavior), while too
/// LOW would re-deliver a brief that landed. Bias toward the reversible
/// mistake.
pub const KICKOFF_TURN_EVIDENCE_BYTES: u64 = 4096;

/// Why a lost-kickoff re-delivery was declined (#517) — carried into the
/// audit so "loomux did not re-send" always says which condition stopped it,
/// the same discipline `StrandedBlocker` gives the badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickoffDecline {
    /// Not a fresh spawn's kickoff at all (a mid-session delivery, or a
    /// resume re-sync). Only a FRESH kickoff's payload is unrecoverable
    /// when it never lands — see `kickoff_recovery_action`'s doc.
    NotAKickoff,
    /// The ledger says this delivery is no longer outstanding.
    Resolved,
    /// A human typed into the pane after our submit.
    HumanInput,
    /// A live interactive question owns the pane (#420).
    Question,
    /// The pane produced a turn's worth of output after our submit, on a
    /// pane we watched go ready before pasting — the brief landed after all,
    /// whatever the confirmation tiers saw.
    TurnStarted,
    /// The pane produced a turn's worth of output after our submit, but this
    /// delivery pasted BLIND (`ReadyWait::TimedOut` — the boot wait hit
    /// `READY_MAX_WAIT` with the CLI still painting), so that growth cannot
    /// be attributed to a turn rather than to the boot paint that was
    /// already running (review F5). Declines exactly like `TurnStarted` —
    /// the conservative fallback is the same — but must not CLAIM a turn the
    /// evidence does not support: `.loomux/lessons.md`, "a claim is a
    /// deliverable". The delivery's own `prompt-typed` record carries
    /// `ready_observed: false` alongside, so the two facts are greppable
    /// together.
    OutputUnattributable,
    /// The per-delivery re-delivery budget is spent.
    Exhausted,
}

impl KickoffDecline {
    /// Stable audit token, kept separate from any human-facing wording so
    /// one can change without silently breaking greps over the other.
    pub fn as_str(self) -> &'static str {
        match self {
            KickoffDecline::NotAKickoff => "not-a-kickoff",
            KickoffDecline::Resolved => "resolved",
            KickoffDecline::HumanInput => "human-input",
            KickoffDecline::Question => "question",
            KickoffDecline::TurnStarted => "turn-started",
            KickoffDecline::OutputUnattributable => "output-unattributable",
            KickoffDecline::Exhausted => "redelivery-budget-spent",
        }
    }
}

/// What to do about a fresh kickoff the late monitor just declared `Failed`
/// with nothing left in the box (#517).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KickoffRecovery {
    /// Re-admit the brief through the delivery queue's front door.
    Redeliver,
    /// Leave it to the attention badge `stranded_selfheal_action` already
    /// raised, and audit why.
    Decline(KickoffDecline),
}

/// The one decision point for #517. Pure, so the precedence is directly
/// pinnable rather than an inline `if` chain a future edit could reorder —
/// the argument `stranded_selfheal_action` and `late_monitor_tick` already
/// make in this file.
///
/// Consulted ONLY after `stranded_selfheal_action` has returned
/// `Attention(StrandedBlocker::NotHolding)`: that is the eaten-paste
/// signature (the delivery failed AND its text is not in the box), and it is
/// the only state where re-sending the text is the right recovery rather
/// than pressing Enter. Every other `StrandedAction` keeps its pre-#517
/// behavior untouched.
///
/// Inputs, in the order they are consulted:
/// - `is_fresh_kickoff` — a FRESH spawn's brief, not a mid-session delivery
///   and not a resume re-sync. Narrow on purpose: a fresh kickoff is the one
///   payload with no other route to the agent (it is the agent's entire
///   reason to exist, and nothing will re-send it), whereas a mid-session
///   prompt has a sender who is still around and a resume notice is
///   re-derivable from durable state.
/// - `ledger_outstanding` — the DURABLE artifact: this pane's recorded
///   `DeliveryOutcome` is still THIS delivery's and still unconfirmed.
///   Checked before any pane reading, exactly as `stranded_selfheal_action`
///   checks it first.
/// - `human_typed_since` / `question_on_screen` — the same two absolutes the
///   self-heal honors. A re-delivery pastes rather than pressing Enter, so
///   it is less dangerous than a heal here, but "loomux does not act on a
///   pane a person is using" is a rule this feature has no business
///   weakening.
/// - `output_since_submit` vs `turn_evidence_bytes` — the anti-double-
///   delivery guard (see the section comment above). `ready_observed`
///   (review F5) only ever changes which DECLINE this reports, never whether
///   it declines: growth on a pane we never watched go ready is
///   `OutputUnattributable` rather than `TurnStarted`, because the boot paint
///   that was still running is an equally good explanation for it.
/// - `redeliveries_used` / `max_redeliveries` — the bound.
#[allow(clippy::too_many_arguments)]
pub fn kickoff_recovery_action(
    is_fresh_kickoff: bool,
    ledger_outstanding: bool,
    human_typed_since: bool,
    question_on_screen: bool,
    output_since_submit: u64,
    turn_evidence_bytes: u64,
    ready_observed: bool,
    redeliveries_used: u32,
    max_redeliveries: u32,
) -> KickoffRecovery {
    if !is_fresh_kickoff {
        return KickoffRecovery::Decline(KickoffDecline::NotAKickoff);
    }
    if !ledger_outstanding {
        return KickoffRecovery::Decline(KickoffDecline::Resolved);
    }
    if human_typed_since {
        return KickoffRecovery::Decline(KickoffDecline::HumanInput);
    }
    if question_on_screen {
        return KickoffRecovery::Decline(KickoffDecline::Question);
    }
    if output_since_submit >= turn_evidence_bytes {
        return KickoffRecovery::Decline(if ready_observed {
            KickoffDecline::TurnStarted
        } else {
            KickoffDecline::OutputUnattributable
        });
    }
    if redeliveries_used >= max_redeliveries {
        return KickoffRecovery::Decline(KickoffDecline::Exhausted);
    }
    KickoffRecovery::Redeliver
}

/// The kickoff treatment a lost-kickoff RE-delivery's own drain runs under
/// (#517, review F4) — `None` when it must run under none at all.
///
/// **The finding.** The recovery used to nudge the drainer with `None`, so
/// the re-delivery pasted with `wait_ready: false`. That is the one place
/// this feature could have reproduced the bug it exists to fix: a CLI whose
/// stdin reader has still not attached would eat the re-delivered brief too.
/// It is NOT impossible by construction — merely unlikely, since the monitor
/// only declares failure after `READY_MAX_WAIT` plus `PENDING_IDLE_QUIET`,
/// by which point a healthy CLI has long been reading — and "unlikely" is
/// not the bar for the ghost of the original defect. So the re-delivery gets
/// the SAME boot wait the original kickoff had. It costs `READY_MIN_WAIT`
/// (1.5s) on a pane that is already ready, which is nothing against a lost
/// brief.
///
/// The other two flags are deliberately NOT copied from the original:
/// - `confirm_autopilot: false` — copilot's consent dialog is triggered by
///   the FIRST submit and has either been answered already or will never
///   appear; re-arming that watcher would put a stray Enter into a pane
///   whose state we are trying to recover.
/// - `fresh_kickoff: false` — this is what bounds the whole feature at ONE
///   recovery. A re-delivery that is itself eaten degrades to the loud
///   pre-#517 badge instead of triggering another recovery, so there is no
///   re-send loop even if every budget check were removed.
///
/// `None` when the re-delivery did NOT land alone at the front of the queue:
/// kickoff treatment belongs to whichever entry the drainer's first pass
/// actually picks up, and mirroring `deliver_prompt`'s own `was_first` rule
/// keeps that decision in one place rather than relying on
/// `FreshFirstAttempt`'s id guard to catch a mismatch after the fact.
pub fn redelivery_treatment(was_first: bool) -> Option<KickoffTreatment> {
    was_first.then_some(KickoffTreatment {
        wait_ready: true,
        confirm_autopilot: false,
        fresh_kickoff: false,
    })
}

/// The kickoff treatment a delivery HELD THROUGH A PAUSE runs under when
/// `flush_paused_queues` finally starts a drainer for its pane (#620).
///
/// **The finding.** Nothing guards `spawn_agent` against a paused group, and
/// since #569 a spawn during a pause routes its `Delivery::FreshKickoff`
/// through `deliver_prompt`'s pause branch like any other delivery. The
/// resume then flushed it with `None` — `wait_ready: false`,
/// `confirm_autopilot: false`, `fresh_kickoff: false` — because the queue
/// entry carried no kind. For a copilot pane under `--autopilot` that is a
/// wedge rather than a timing nit: per `confirm_copilot_autopilot_dialog`'s
/// own note the consent dialog appears AFTER the kickoff Enter, so nobody
/// dismisses it, and #517's late-kickoff recovery is unarmed because the
/// drainer was never told this was a kickoff.
///
/// So, unlike `redelivery_treatment`, this copies the ORIGINAL delivery's own
/// flags rather than a narrowed set — the pause changed when the brief lands,
/// not what it is. `confirm_autopilot` is the one flag that cannot come from
/// the kind alone (it also depends on the group's CLI and posture); the caller
/// passes `should_confirm_copilot_autopilot`'s verdict, computed exactly as
/// `deliver_prompt`'s front door computes it.
///
/// `None` for `MidSession`, which is every ordinary held prompt and needs no
/// treatment at all — the same `None` this path has always passed, now said
/// rather than assumed.
pub fn paused_flush_treatment(kind: Delivery, confirm_autopilot: bool) -> Option<KickoffTreatment> {
    (kind != Delivery::MidSession).then_some(KickoffTreatment {
        wait_ready: kind.wait_ready(),
        confirm_autopilot,
        fresh_kickoff: kind.recovers_lost_kickoff(),
    })
}

/// The three flags `redelivery_treatment` and `paused_flush_treatment` decide,
/// named rather than a bare `(bool, bool, bool)`: they are the same type and
/// mean opposite things, so a positional tuple is one transposed edit away
/// from arming the autopilot watcher on a recovery, or making a re-delivery
/// recoverable and unbounding the feature. The public mirror of the private
/// `FreshFirstAttempt` fields they populate.
///
/// #620 renamed this from `RedeliveryTreatment`: it is no longer the shape of
/// one producer's answer, and each producer's own doc — not this struct's —
/// is where the values it chooses are argued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KickoffTreatment {
    /// Hold the paste until the CLI has painted.
    pub wait_ready: bool,
    /// Watch for copilot's autopilot-consent dialog (#101/#364).
    pub confirm_autopilot: bool,
    /// Whether THIS delivery may itself be re-delivered by the late monitor
    /// if it turns out never to have landed (#517).
    pub fresh_kickoff: bool,
}

/// Whether the late monitor's live re-check should RE-WORD a badge that
/// currently reads "loomux is re-sending it" (`blocker: None`), and to what.
/// `None` = leave the in-flight wording alone.
///
/// Two reasons to leave it alone, and they are different facts:
/// - `Exhausted` (#496 rev-47 NB1) — the budget arm firing on our OWN
///   already-queued marker, which IS the "still re-sending" state the badge
///   already shows. Only a REAL blocker is worth re-wording for.
/// - `NotHolding` while a lost-kickoff re-delivery is in flight (#517,
///   review F2) — "its text is gone — check the pane" is *true* here and
///   still wrong to say: the text is gone precisely because the paste was
///   eaten, which is why a re-delivery is queued. Without this arm the badge
///   flips back to telling the human to act on a pane loomux is actively
///   recovering, on the very next 5s tick, for as long as the re-delivery
///   waits behind a busy drain or an occupied box. It self-corrects on the
///   re-delivery's confirm or supersede, but a badge that says "your problem"
///   about loomux's own in-flight work is exactly the honesty failure the
///   NB1 re-check exists to prevent, pointed the other way.
///
/// Every other blocker re-words as before: `HumanInput` and `Question` are
/// real, human-clearable states that outrank an in-flight recovery, and
/// `QueueFull` means nothing was queued at all.
///
/// #559 adds `Unverifiable` to the second arm for the identical reason. It is
/// the same delivery shape — a paste with no confirming trace, which is why a
/// re-delivery was queued — reached because the paste was too large to verify
/// rather than because it was verifiably gone. Telling the human to go check a
/// pane loomux is actively recovering is the same honesty failure whichever of
/// the two readings produced it.
pub fn stranded_reword(live: StrandedBlocker, redelivery_in_flight: bool) -> Option<StrandedBlocker> {
    match live {
        StrandedBlocker::Exhausted => None,
        StrandedBlocker::NotHolding | StrandedBlocker::Unverifiable if redelivery_in_flight => None,
        other => Some(other),
    }
}

/// What one live box reading is allowed to do to a badge that is already up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeRelease {
    /// Take the chip down, with this `clear_stranded` reason.
    Clear(&'static str),
    /// Keep the chip up and say something truer about it. Never an insert —
    /// see [`OrchRegistry::reword_stranded`].
    Reword(StrandedBlocker),
    /// This reading establishes nothing that changes the chip.
    Keep,
}

/// The badge-honesty matrix (#825 M2): given a raised badge and one live
/// reading of the pane, may the chip come down, and if not, does it still say
/// the right thing?
///
/// # Why this is a function and not two pieces of inline logic
///
/// The check itself is not new. `run_late_confirmation_monitor`'s `KeepWaiting`
/// arm has kept a raised badge honest every `LATE_MONITOR_POLL` since #496
/// PR-C — but only for as long as that monitor lives, which is
/// `LATE_MONITOR_MAX_LIFETIME` (four hours) at the outside and usually far
/// less. So the badge latch has never really been two regimes of *logic*, only
/// two regimes of *observation*: while a monitor is alive the chip
/// self-corrects, and after it exits nothing looks at the pane again, ever.
/// That is the whole of #825's "indefinite latch" for the three classes below.
///
/// The fix is therefore a hoist, not a second opinion. This function is the
/// decision, extracted; the monitor's arm and `OrchRegistry::stranded_janitor_
/// pass` are two *observers* that ask it the same question. Two honesty checks
/// that could drift is the failure mode a copy would have shipped — the one
/// where a chip clears under a live monitor and re-latches an hour later, or
/// the reverse, and no reader could say which was right.
///
/// # What each caller can and cannot see
///
/// `saw_text_in_box` is the one input the two observers genuinely differ on,
/// and it is a fact about the *observer*, not the pane. The monitor watches one
/// delivery continuously, so it can witness a genuine present→absent
/// TRANSITION: our text was in that box, and now it is not, so whoever pressed
/// Enter, the pane is no longer wedged. The janitor arrives with no such
/// memory — it starts, by construction, on panes whose monitor is already gone
/// — so it can never pass anything but `false` here, and needs strictly
/// stronger evidence to clear on. Hence the two clear arms.
///
/// # The evidence bar for a clear without a transition
///
/// Exactly #819's `HumanResolved` bar, extended from the queued-marker path to
/// badge-only panes: a positive [`BoxReading::NotHolding`] on the text the
/// ledger still records **and** a human keystroke on this pane since our own
/// submit. Both halves are load-bearing.
///
/// - `NotHolding` alone is [`StrandedRetireReason::TextGone`], which #819
///   deliberately declined to clear on: our text left the box with nobody at
///   the keyboard is precisely [`StrandedBlocker::NotHolding`]'s own sentence
///   ("never confirmed and its text is gone — check the pane"). Clearing there
///   would answer a question loomux cannot answer.
/// - The keystroke alone is #518's phantom — a terminal auto-reply
///   misclassified as a person — and is why the stamp only ever *names* a
///   release that a box reading has already licensed. It never licenses one.
///
/// Requiring both also contains the two residuals this inherits rather than
/// absorbing them silently:
///
/// 1. **#828's remaining false `NotHolding`** (a hard mid-word wrap). A lone
///    false reading only ever re-words here; it takes two independent failures
///    — a misread box AND a phantom keystroke — to reach a clear, and that
///    conjunction is the same one #819 already accepts on the marker path.
/// 2. **#518's phantom stamp.** Same bar, same path, same acceptance. A
///    phantom over a genuinely-gone text is a clear on `TextGone`, which is why
///    nothing below may clear on `NotHolding` alone.
///
/// # Reading the matrix
///
/// Nothing but a positive `NotHolding` establishes anything at all. `Holds`
/// (our text is demonstrably still sitting there), `Unverifiable` (we looked
/// and could not tell) and `None` (the ledger records no text to look for) are
/// three different facts, and all three keep the chip: this is the direction
/// where being wrong hides a prompt nobody will re-send.
///
/// Pure and total, so every cell is directly assertable.
#[doc(hidden)] // pub for integration tests
pub fn stranded_badge_release(
    // The chip that is currently up. `None` is the in-flight-heal wording
    // ("loomux is re-sending it"), which is not this matrix's business — the
    // monitor re-derives that one live from `stranded_selfheal_action`.
    blocker: Option<StrandedBlocker>,
    // What Tier 1 says about OUR OWN stranded text, or `None` when there is no
    // recorded text to look for. Kept distinct for the reason
    // `stranded_marker_action` keeps them distinct: "nothing to look for" and
    // "looked and could not tell" are different facts that happen to take the
    // same conservative branch.
    reading: Option<BoxReading>,
    // A human keystroke is on record for this pane since our submit —
    // `tier1_trusted`'s inverse, the same derivation `drain_stranded_submit`
    // takes. Names a release; never licenses one.
    human_stamped_since: bool,
    // The observer WATCHED our text sit in this box earlier in its own life, so
    // a `NotHolding` now is a transition rather than a standing state. Only the
    // late monitor can ever pass `true`; see the header.
    saw_text_in_box: bool,
) -> BadgeRelease {
    // Nothing weaker than a positive absence is evidence. Ordered first so no
    // arm below can be reached on a reading that never happened.
    if reading != Some(BoxReading::NotHolding) {
        return BadgeRelease::Keep;
    }
    if saw_text_in_box {
        // #496 PR-C's own clear, unchanged and still the strongest reading
        // available: present→absent, whoever pressed the Enter. It outranks the
        // keystroke-named clear below because it is about the text rather than
        // about who was at the keyboard, and it holds for EVERY class — a
        // transition unwedges the pane whatever the chip happened to say.
        return BadgeRelease::Clear("text-left-the-box");
    }
    match blocker {
        // The three classes #825 leaves with no release: each one's badge is a
        // claim about where our text is, so each one is answerable by a later
        // reading of the same box. (`QueueFull` is a retry, not a clear — M3.
        // `PauseSuppressed` describes a loss that already happened and can
        // never become untrue, so no pane reading may release it; the human's
        // explicit dismiss is its release, M1. Every hold class already has
        // one.)
        Some(StrandedBlocker::NotHolding)
        | Some(StrandedBlocker::Unverifiable)
        | Some(StrandedBlocker::Exhausted) => {
            if human_stamped_since {
                // The one name in this vocabulary for "a person dealt with it",
                // taken from #819's enum rather than spelled again, so the
                // drainer's retirement and this clear can never come to mean
                // two different things in the audit.
                BadgeRelease::Clear(StrandedRetireReason::HumanResolved.as_str())
            } else if blocker != Some(StrandedBlocker::NotHolding) {
                // The honesty upgrade. `Unverifiable` says "we could not read
                // the box" and `Exhausted` says "a heal fired and did not
                // take, and the text read `Holds`" — both are now stale
                // sentences about a box we CAN read and which does not hold
                // our text. The chip stays up (nobody has shown a human dealt
                // with it) but it stops claiming more than we know.
                BadgeRelease::Reword(StrandedBlocker::NotHolding)
            } else {
                // Already the truest thing we can say.
                BadgeRelease::Keep
            }
        }
        _ => BadgeRelease::Keep,
    }
}

/// Whether a raised chip is worth a janitor pane read at all (#825 M2).
///
/// **Derived from the matrix rather than restated as a list.** The classes the
/// janitor watches are, by definition, exactly the ones where some reading it
/// can actually take would change the chip — so this asks
/// [`stranded_badge_release`] instead of naming `NotHolding` / `Unverifiable` /
/// `Exhausted` a second time. A hand-written list is a second copy of the
/// matrix's domain, and it goes stale the first time a class is added to one
/// and not the other: too narrow silently drops a class from the janitor with
/// nothing red, too wide only costs a read. Neither can happen if there is one
/// list and it is the function.
///
/// `NotHolding` is the only reading that ever produces a verdict (every other
/// arm keeps the chip), and `saw_text_in_box` is always `false` for this
/// observer, so the two keystroke states are the whole space to probe.
fn janitor_watches(blocker: Option<StrandedBlocker>) -> bool {
    [true, false].into_iter().any(|stamped| {
        stranded_badge_release(blocker, Some(BoxReading::NotHolding), stamped, false)
            != BadgeRelease::Keep
    })
}

/// Whether a self-heal may admit its `StrandedSubmit` marker right now
/// (#496 PR-C), and if not, the audited reason. `None` = admit.
///
/// **`drainer-active` is a safety rule, not a nicety.** Pushing to the FRONT
/// of a pane's queue is only safe while no drainer OWNS an entry: a drainer
/// sitting inside `deliver_now` has already peeked its front entry and will,
/// on completion, call `pop_front_dequeued(that entry's id)` — which pops
/// ONLY on an id match. Slip a marker in front of it and that pop matches
/// nothing, leaving an ALREADY-DELIVERED text entry queued for a second,
/// duplicate delivery. Pre-#496 nothing could hit this (the front door
/// pushes to the BACK; the drainer's own `AbortedPreEnter` marker is pushed
/// after it pops), and the self-heal must not become the first thing that
/// does.
///
/// **This predicate is only half the rule; the other half is WHERE it is
/// evaluated.** Deciding from `queue_draining` and then pushing is not
/// enough on its own — a drainer registering between the two re-opens the
/// very hazard above, which is what this PR first shipped and review rev-47
/// B1 caught. The caller (`admit_stranded_selfheal`) must therefore consult
/// this while HOLDING `queues`, and push in that same critical section, so
/// the observation cannot go stale before it is acted on. What makes that
/// sufficient: any drainer that could ever peek this front must register
/// before it peeks (`ensure_drainer`) and must take `queues` TO peek, so it
/// is either already registered when the fused section reads
/// `queue_draining` (→ decline) or it peeks strictly after the push (→ finds
/// the marker at the front, the safe case — it drains the submit first and
/// pops it by a matching id). See `admit_stranded_selfheal`'s doc for the
/// lock order and `queue.rs`'s `stranded_admission_property` for the
/// exhaustive proof, whose unfused variant is the mutation control that
/// keeps the fused one from passing vacuously.
///
/// **Nothing is lost by declining, in either case.** A live drainer means a
/// delivery is already queued for this pane, and THAT delivery's own
/// pre-paste `flush_stranded_text` is the pre-existing mechanism which
/// presses exactly the Enter this heal wanted pressed — the self-heal exists
/// for the case where no such next delivery exists (an idle group), which is
/// precisely when no drainer is running. A marker already at the front is
/// the same submit, already pending, retried by the drainer with no cap.
/// Either way the badge is still raised: the human is told regardless of
/// which mechanism does the pressing.
pub fn stranded_admission_gate(drainer_active: bool, front_is_marker: bool) -> Option<&'static str> {
    if drainer_active {
        return Some("drainer-active");
    }
    if front_is_marker {
        return Some("submit-already-queued");
    }
    None
}

/// Whether a raised [`StrandedBlocker::QueueFull`] chip's refused re-send may be
/// admitted **now** (#825 M3), and if not, the reason it is not yet time.
/// `None` = go.
///
/// `QueueFull` is the one unreleased class whose release is a **retry rather
/// than a reading**. Its badge does not claim anything about where our text is
/// — [`stranded_badge_release`]'s matrix has nothing to say about it — it says
/// loomux could not even *queue* the re-send. The honest answer to that is to
/// queue it once there is room, and let the confirm/retire machinery that owns
/// every other marker own this one too.
///
/// # Why this is not a hook on the drain edge
///
/// plan-312's M3 put the retry at `note_queue_capacity`'s `Full` → not-`Full`
/// transition, beside `announce_refusal_roster` (#658). That edge cannot admit
/// anything, and the reason is a runtime fact a read-only plan could not see:
/// every production caller able to produce it — [`OrchRegistry::pop_front_dequeued`],
/// [`OrchRegistry::pop_batch_dequeued`] and [`OrchRegistry::drop_superseded`] —
/// runs on the drainer thread, which holds its `queue_draining` registration
/// from `ensure_drainer` right through to `commit_exit`. So
/// [`stranded_admission_gate`] would answer `Some("drainer-active")` at every
/// real drain edge, every time, forever. (The one other caller,
/// `OrchRegistry::drop_queue`, is destroying the pane's queue; re-admitting
/// there would be queueing for a pane that is going away.)
///
/// It is also the wrong moment **on the merits**, which is what makes this a
/// relocation rather than a workaround. `stranded_admission_gate`'s own doc
/// says what a live drainer means: a delivery is already queued for this pane,
/// and THAT delivery's pre-paste `flush_stranded_text` presses exactly the
/// Enter this marker wants pressed. The moment nothing else will press it is
/// the moment the queue has gone quiet and the drainer has exited — which is
/// also the only moment the marker can be admitted at all. So M3 observes the
/// pane *after* the drain rather than during it: the same "hoist the work out
/// of the lifetime of the thread that dies" M2 applied to the late monitor,
/// pointed at the drainer instead.
///
/// *Rejected:* teaching the gate that the drainer may admit its own marker
/// between a `pop_front_dequeued` and its next peek. It is true that the
/// drainer owns no entry at that instant — but it re-opens #496 PR-C rev-47
/// B1 by construction, replacing a fused check-and-push with a parameter every
/// future caller of `note_queue_capacity` would have to get right, for a repair
/// the very next queued delivery's pre-paste flush already performs.
///
/// Pure and total, so every cell is directly assertable. The depth check
/// mirrors `push_stranded_front_locked`'s own refusal condition rather than
/// `queue::capacity_state`'s badge classification: this is a pre-check that
/// exists to keep a still-full pane from attempting-and-failing (and auditing a
/// `delivery-dropped` line) on every pass, so it has to ask the same question
/// the push will ask. The real check is still the one inside
/// [`OrchRegistry::admit_stranded_selfheal`], taken under the `queues` lock.
pub fn queuefull_readmit_gate(
    blocker: Option<StrandedBlocker>,
    depth: usize,
    drainer_active: bool,
) -> Option<&'static str> {
    // Only `QueueFull` names a re-send that was REFUSED and never queued. Every
    // other chip is somebody else's release — M1's dismiss, M2's matrix, or the
    // hold and depth classes' own conditions — and the in-flight wording
    // (`None`) is a submit that is already pending.
    if blocker != Some(StrandedBlocker::QueueFull) {
        return Some("not-queue-full");
    }
    // The condition that raised the chip is still true, so the chip is still
    // right and there is nothing to retry.
    if depth >= queue::QUEUE_MAX_PER_PANE {
        return Some("still-full");
    }
    // Same word as `stranded_admission_gate`'s, and the same fact — pushing in
    // front of a drainer that owns the front entry is the rev-47 B1 race. Named
    // here as well so a caller can decline BEFORE attempting, which is what
    // keeps a declined pass silent instead of writing a skip line every 30s.
    if drainer_active {
        return Some("drainer-active");
    }
    None
}

/// One pane's stranded-delivery state (#496 PR-C), held in
/// `OrchRegistry::attn_stranded` and rendered by `attention_tick`. Only the
/// facts are stored; the human-facing wording is built at render time by
/// `stranded_detail`, so the badge text lives in exactly one place next to
/// every other attention reason's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrandedNote {
    /// `None` — a self-heal is in flight for this pane. `Some(blocker)` —
    /// loomux cannot act and the human must.
    pub blocker: Option<StrandedBlocker>,
    /// When the delivery was declared stranded (Unix-ms).
    pub since_ms: u64,
}

/// #569: whether the resume-time suppression notice's FALLBACK badge fires
/// for one pane that lost deliveries to a pause.
///
/// Pure, and separate from `announce_pause_suppression`, because the branch
/// that matters most cannot be reached in a headless integration test: with no
/// `AppHandle`, `deliver_prompt` always ends in `Err("no app handle")` after
/// admitting, so "the notice landed, therefore no badge" is unobservable there
/// (the same limitation `delivered_texts`' doc in the test file describes from
/// the other side). Extracting the decision makes all three rules assertable
/// directly, which is the only way "delivered ⇒ never badge" gets tested at
/// all rather than asserted in a comment.
///
/// - `notice_delivered` — the orchestrator has the full list, so a chip on
///   every pane that missed something would be noise on top of a channel that
///   already worked.
/// - `target_alive` — a dead pane's badge is a chip nobody can act on.
/// - `existing` — never stomp another mechanism's badge (`note_queue_capacity`
///   follows the same rule): whatever raised it is telling the human to look at
///   this same pane, and a *held* pane's badge is the more urgent claim because
///   it names something still fixable. An in-flight heal (`Some(note)` whose
///   own `blocker` is `None`) counts as somebody else's badge for the same
///   reason. Our OWN badge is re-stamped rather than skipped, so a second pause
///   that loses more does not go quiet.
pub fn pause_badge_decision(
    notice_delivered: bool,
    target_alive: bool,
    existing: Option<StrandedNote>,
) -> bool {
    if notice_delivered || !target_alive {
        return false;
    }
    match existing {
        None => true,
        Some(n) => matches!(n.blocker, Some(StrandedBlocker::PauseSuppressed)),
    }
}

/// The badge/tooltip wording for a stranded pane (#496 PR-C) — pure, and
/// deliberately phrased as what the HUMAN should do, since the entire point
/// of the badge is that a wedged pane is never discovered by accident.
pub fn stranded_detail(name: &str, blocker: Option<StrandedBlocker>) -> String {
    match blocker {
        None => format!("{name}'s prompt was never submitted — loomux is re-sending it"),
        Some(StrandedBlocker::HumanInput) => {
            format!("{name}'s prompt is stuck behind text you typed — press Enter or clear the box")
        }
        Some(StrandedBlocker::Question) => {
            format!("{name}'s prompt is stuck behind a question on screen — answer it")
        }
        Some(StrandedBlocker::NotHolding) => {
            format!("{name}'s prompt was never confirmed and its text is gone — check the pane")
        }
        // #559: names the uncertainty rather than resolving it, and gives an
        // action that is safe under either branch — if the prompt is sitting
        // there, Enter sends it; if it is not, the human has looked and lost
        // nothing. Never says the text is gone: loomux does not know that.
        Some(StrandedBlocker::Unverifiable) => {
            format!(
                "{name}'s prompt was never confirmed and is too large for loomux to verify in the \
                 pane — check the pane and press Enter if it is still sitting there unsent"
            )
        }
        Some(StrandedBlocker::Exhausted) => {
            format!("{name}'s prompt is still unsubmitted after a self-heal — press Enter in the pane")
        }
        Some(StrandedBlocker::QueueFull) => {
            format!("{name}'s pane has a full delivery queue, so loomux could not even queue a re-send — press Enter in the pane")
        }
        // #563: says what is true NOW (nothing lost) and what happens if the
        // pane is left alone (deliveries start being dropped). Deliberately
        // does not claim a re-send failed — that is `QueueFull`'s sentence,
        // and it would be false here.
        //
        // rev-10 finding 1: nor does it claim the pane is HELD.
        // `note_queue_capacity` raises this badge on depth alone and never
        // reads any hold state, so a pane whose senders simply outrun a healthy
        // drainer lands here with nothing to release. The hold is offered as a
        // condition for the human to check — "if the pane is held" — because
        // that is the shape of what loomux actually knows. See
        // `queue::pressure_notice`'s doc for the full argument.
        Some(StrandedBlocker::QueueNearFull) => {
            format!(
                "{name}'s delivery queue is nearly full and still filling — deliveries are \
                 arriving faster than that pane accepts them. If it is held (unsubmitted text, \
                 or a question on screen), releasing it drains the backlog; at the cap further \
                 deliveries are dropped"
            )
        }
        // #563 / rev-10 finding 1: the depth-derived counterpart to
        // `QueueFull`. States the consequence (arrivals are being dropped) and
        // the same conditional check, and claims nothing about a re-send that
        // this badge's caller never attempted.
        Some(StrandedBlocker::QueueAtCapacity) => {
            format!(
                "{name}'s delivery queue is FULL — further deliveries to that pane are being \
                 DROPPED, not queued. If it is held (unsubmitted text, or a question on screen), \
                 releasing it drains the backlog"
            )
        }
        // #532: this wording must stay honest about WHICH state loomux is in,
        // because it does not know. It names both branches and gives an action
        // that is safe under either — answering a real question, or typing and
        // deleting a character, which both moves the pane's output past the
        // stale bytes and leaves the box empty again.
        Some(StrandedBlocker::QuestionStale) => {
            format!(
                "{name}'s prompt has been held for minutes on a question loomux still detects in this pane — \
                 answer it if one is on screen; if the pane looks clear, that reading is stale: type a \
                 character and delete it to release the hold"
            )
        }
        // #569: past tense, and no action for the PANE — there is nothing to
        // release here. What was addressed to this pane is gone, and the only
        // repair is upstream: whoever sent it has to send it again. Says so,
        // and claims nothing about the pane's own state (see the variant's doc).
        Some(StrandedBlocker::PauseSuppressed) => {
            format!(
                "{name} was sent deliveries that were DISCARDED while this group was paused, and \
                 loomux could not reach the orchestrator to say so — nothing is queued for that \
                 pane. Whatever it is waiting on has to be sent again"
            )
        }
    }
}

/// Tier 1 (#112 round 2): does `stripped_tail` (ANSI-stripped current pane
/// output) still hold `pasted` at its TAIL END? Deliberately "at the tail
/// end", not "anywhere" — a CLI that echoes an ACCEPTED prompt into
/// scrollback/transcript history would make "appears anywhere" trivially
/// true forever and say nothing about acceptance. Both sides normalized the
/// same way `prompt_landed` already does (trim + collapse whitespace,
/// washing out line-wrap/CRLF noise); containment is checked only within
/// the last `pasted`-length-plus-slack window of the normalized tail
/// (`BOX_TAIL_WINDOW_SLACK`), so text that's scrolled up into older output
/// because something else happened since reads as gone even though it's
/// technically still present somewhere earlier in the buffer.
///
/// This same function serves BOTH of Tier 1's uses: checked once, on the
/// pre-Enter tail, it verifies Tier 1's own precondition (`deliver_prompt`'s
/// `tier1_governs`); polled after Enter, it's the two-sided signal itself
/// (`true` => still pending/vetoable, `false` => consumed/accepted).
#[doc(hidden)] // pub for integration tests
pub fn box_holds_paste(stripped_tail: &str, pasted: &str) -> bool {
    // #821: two routes, and the disjunction is the whole safety property —
    // de-framing is a SUPERSET of the flat comparison, never a replacement
    // (rev-306 B1). Either route finding our text is our text being present.
    //
    // The de-framed route is what #821 adds: it sees through a per-row gutter
    // the flat comparison cannot. Both sides are de-framed there — see
    // `normalize_deframed` for why the tail alone would be worse than the bug.
    //
    // The flat route is the pre-#821 comparison, kept because de-framing can
    // otherwise LOSE a match it used to find. A wrap that pushes a mid-line
    // `|`/`*` to a row START strips it from the tail (row-leading) and not from
    // the needle (mid-line, and `deframe` is leading-only), so `ps aux | grep`
    // wrapped before the pipe fails de-framed containment on text that is
    // verbatim on screen — and the probe, sampling that same line, carries the
    // same `|` and fails with it. That is a new route to `NotHolding`, the one
    // reading this whole change exists to make expensive, and it needs only a
    // pane under 48 columns rather than anything degenerate.
    //
    // Keeping both makes the property structural rather than case-analytic:
    // #821 can only ever ADD `Holds` readings. A narrower `deframe` (box-drawing
    // glyphs only, sparing `*` and `|`) would dodge today's two known shapes
    // and would still be a case analysis — and it would mint a second notion of
    // decoration, which is exactly what sharing `deframe` exists to avoid.
    holds_paste_under(stripped_tail, pasted, normalize_deframed)
        || holds_paste_under(stripped_tail, pasted, normalize_prompt_text)
}

/// One route of [`box_holds_paste`]: is `pasted` inside the tail-end window of
/// `stripped_tail`, with both sides put through `norm` (#821)?
///
/// Taking the normalizer as a parameter rather than spelling the windowing
/// twice is the point — the two routes must agree about what "the tail end"
/// means, or the disjunction would be comparing different questions.
fn holds_paste_under(
    stripped_tail: &str,
    pasted: &str,
    norm: fn(&str) -> String,
) -> bool {
    let norm_pasted = norm(pasted);
    if norm_pasted.is_empty() {
        return false; // nothing to still be holding
    }
    box_tail_window(&norm(stripped_tail), norm_pasted.len()).contains(&norm_pasted)
}

/// How many raw tail bytes Tier 1 must ask for to have any chance of finding
/// `pasted` in the box (#559) — the paste's own normalized length plus
/// `BOX_TAIL_SCAN_BYTES` of headroom for the framing/prompt/cursor chrome
/// around it, capped at `TIER1_SCAN_MAX_BYTES`.
///
/// **Why the read is derived from the paste rather than fixed.** Containment
/// needs the tail to be at least as long as what we are looking for, so a
/// window that is smaller than the paste cannot return `true` no matter what
/// the pane contains. The *semantic* window was already paste-relative —
/// `box_holds_paste` checks only the last `pasted`-length-plus-slack of the
/// normalized tail — so sizing the READ this way widens nothing about what
/// counts as "the tail end"; it only stops the read from truncating the
/// window the comparison already uses. A paste of 4 KiB or less asks for
/// exactly what it always did, so nothing changes for the ordinary delivery
/// (only pastes past the old fixed window move at all).
///
/// **Why the cost is acceptable.** This is proportional to a paste we have
/// already paid to write into the pane, and it only exceeds the old constant
/// for deliveries that are themselves multi-KiB. `LATE_MONITOR_QUESTION_SCAN_
/// BYTES` (32 KiB) is the standing precedent — and its own doc already
/// contemplates "Tier 1 governing a multi-KB brief", a state that until this
/// change could not actually occur.
///
/// Best-effort, not a guarantee: a heavily-ANSI-escaped tail strips down to
/// far fewer characters than the bytes read, so even this size can come back
/// too short. #583 measured how often (routinely, from ~1.5 KiB up) and #685
/// turned that residual into a re-read — see `Tier1Scan`, which is what every
/// call site now goes through. This function keeps the FLOOR: the first request
/// any Tier 1 read makes, unchanged, so no read is ever narrower than it was.
#[doc(hidden)] // pub for integration tests
pub fn tier1_scan_bytes(pasted: &str) -> usize {
    normalize_prompt_text(pasted)
        .len()
        .saturating_add(BOX_TAIL_SCAN_BYTES)
        .min(TIER1_SCAN_MAX_BYTES)
}

/// One delivery's Tier 1 scan window, counted in the unit the comparison
/// actually runs in (#685).
///
/// #559 derived the read from the paste; #583 measured what that bought. A live
/// tail retains roughly half its raw bytes through `strip_ansi` +
/// `normalize_prompt_text`, and the slack (`BOX_TAIL_SCAN_BYTES`) is a
/// CONSTANT, so the shortfall grows with the paste and crosses the needle's own
/// length while the paste is still only a couple of KiB. From there up
/// `box_reading` answered `Unverifiable` out of arithmetic, having learned
/// nothing about the pane: 58% of live deliveries in the 1.5-2 KiB band, and
/// every one from 2.5 KiB. That is the SAFE direction and it stayed safe — it
/// is simply not verification.
///
/// So the budget is counted in POST-STRIP characters and the raw request is
/// whatever it takes to deliver them: read, measure what survived stripping in
/// THIS pane, and — only when that came up short of the window the comparison
/// uses — re-request scaled by the retention this pane's own tail just
/// demonstrated. Scaling by a measured ratio rather than by a chosen inflation
/// constant is the point: density is a property of a CLI's repaint stream
/// (#583's whole finding), so any constant is right for one CLI and wrong for
/// the next.
///
/// **What bounds it.**
/// - `target_chars` is the containment window `box_holds_paste` compares
///   within — the paste plus `BOX_TAIL_WINDOW_SLACK` — and nothing past it is
///   ever looked at. An ordinary delivery's first read clears that target with
///   room to spare and widens not at all; only a read that is truncating the
///   comparison widens, which is also the only case that costs anything.
/// - The `TIER1_SCAN_MAX_BYTES` ceiling still caps the target, so a paste past
///   it stays `Unverifiable` at any density. Raising that ceiling — what
///   verification should even claim for a paste larger than a whole flush — is
///   a separate decision (#685's posture half) and is deliberately not taken
///   here.
/// - A read the ring UNDER-FILLS ends the widening on the spot: fewer bytes
///   back than asked for means the ring holds no more, so no wider request can
///   add one. This is #583's short-*pane* confounder, and it is exactly the
///   regime where widening buys nothing.
/// - `TIER1_SCAN_WIDEN_ROUNDS` bounds the re-reads regardless, and running out
///   returns the widest read taken rather than no read at all.
///
/// **Why this cannot manufacture a confirm.** #559's invariant was that every
/// box read for a delivery uses the same size, because a precondition verified
/// against a wide tail and then polled against a NARROW one could see the box
/// cut mid-paste, read `NotHolding`, and confirm a delivery nothing observed.
/// Two things rule that out here, and it is worth being exact about which:
/// - **Within one of these**, `request_floor` only ever rises, so the confirm
///   loop and the retry loop can never poll narrower than the precondition read
///   they revisit. That is the case the invariant was written for, kept in a
///   strictly stronger form — monotone, not merely equal.
/// - **Across the two that exist** — `deliver_now`'s and the late monitor's,
///   independent and on different threads — the monitor's floor starts at
///   `tier1_scan_bytes` again, so its first REQUEST can be narrower than a
///   widened one `deliver_now` already made. What makes that safe is not the
///   request size but that every read widens ITSELF before anything classifies
///   it: a reading is decided from a tail that covers the containment window
///   whenever the pane's density allows it, and when it does not the tail is
///   shorter than the needle, which is `Unverifiable` and never `NotHolding`.
///   A narrower first request costs a re-read, not a confirm.
///
/// The load-bearing property, so an edit preserves the right one: what makes a
/// `NotHolding` trustworthy is that the read COVERED THE WINDOW, not that it
/// matched an earlier read's byte count. And every read here is at least as
/// wide as the one this call site took before this change, so nothing that was
/// sound before became less so.
#[doc(hidden)] // pub for integration tests
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier1Scan {
    /// Where the NEXT read starts. Seeded with `tier1_scan_bytes` and raised
    /// (never lowered) by a widening, so a delivery that has already learned
    /// its pane is dense pays the discovery once instead of on every poll —
    /// and so successive reads are monotone, which is what makes the
    /// same-size invariant above hold in its stronger form.
    request_floor: usize,
    /// Post-strip characters a read must deliver to cover the containment
    /// window whole.
    target_chars: usize,
}

/// One Tier 1 tail read, as taken (#685) — the request that produced it beside
/// what came back, so the census records the size actually asked for rather
/// than the size the delivery started from.
#[doc(hidden)] // pub for integration tests
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier1TailRead {
    /// The FINAL raw request — `Tier1Scan`'s floor when nothing widened.
    pub requested_bytes: usize,
    /// Raw bytes the ring returned for it.
    pub tail_bytes: usize,
    /// Those bytes after `strip_ansi` — the haystack `box_reading` gets.
    pub stripped: String,
}

impl Tier1Scan {
    /// The scan window for one delivery's `pasted` text. Built once per
    /// delivery (and once per late monitor), never per read: `pasted_text` is
    /// fixed for a delivery's whole life, and normalizing a multi-KiB paste is
    /// not something a 100ms poll should repeat.
    pub fn for_paste(pasted: &str) -> Self {
        Self {
            // Deliberately the same function the old call sites called, rather
            // than a second copy of its arithmetic: one definition of the floor
            // is worth one extra normalize per delivery.
            request_floor: tier1_scan_bytes(pasted),
            target_chars: normalize_prompt_text(pasted)
                .len()
                .saturating_add(BOX_TAIL_WINDOW_SLACK)
                .min(TIER1_SCAN_MAX_BYTES),
        }
    }

    /// Post-strip characters a read has to deliver to cover the whole window
    /// `box_holds_paste` compares within.
    pub fn target_chars(&self) -> usize {
        self.target_chars
    }

    /// Where the next read starts — `tier1_scan_bytes` until a widening raises
    /// it.
    pub fn request_floor(&self) -> usize {
        self.request_floor
    }

    /// The next, WIDER raw request after a read of `requested` bytes came back
    /// as `tail_bytes` raw / `tail_chars` post-strip, or `None` to stop.
    ///
    /// Pure, and the whole of the sizing decision — the reader below is just
    /// the loop around it. Each `None` is a different reason to stop, and they
    /// are all "a wider request cannot change the answer":
    /// the window is already covered; the ring under-filled this one; or the
    /// ratio is undefined because nothing survived stripping (the same
    /// zero-denominator `retained_pct` refuses to report, and for the same
    /// reason — a guess dressed as a measurement is worse than the honest
    /// `Unverifiable` that follows).
    pub fn widen(&self, requested: usize, tail_bytes: usize, tail_chars: usize) -> Option<usize> {
        if tail_chars >= self.target_chars || tail_bytes < requested || tail_chars == 0 {
            return None;
        }
        // Scale the RAW request by the shortfall the read just measured:
        // `requested` bytes bought `tail_chars`, so `target_chars` needs this
        // many. Rounded up, so a read one character short still grows.
        let next = (requested as u64)
            .saturating_mul(self.target_chars as u64)
            .div_ceil(tail_chars as u64)
            .min(TIER1_SCAN_WIDEN_MAX_BYTES as u64) as usize;
        (next > requested).then_some(next)
    }

    /// Take one Tier 1 tail read, widening until it covers the window or until
    /// one of `widen`'s stopping conditions says nothing wider would help.
    /// `None` is no read at all (the pty is gone) — never a short read, which
    /// is a different fact and is `Some` with the shortfall visible in it.
    ///
    /// `read` is the raw-byte reader — `PtyManager::output_tail_bounded` at
    /// every call site. A closure rather than the manager itself so the loop is
    /// drivable by a test that counts the requests and sizes them, which is the
    /// only way to pin "never narrower" and "bounded" on the real code rather
    /// than on a re-implementation of it.
    pub fn read(&mut self, mut read: impl FnMut(usize) -> Option<Vec<u8>>) -> Option<Tier1TailRead> {
        let mut requested = self.request_floor;
        let mut rounds = 0u32;
        loop {
            let raw = read(requested)?;
            let stripped = strip_ansi(&raw);
            // #821: measured through the SAME normalization the containment
            // runs on. Counting the un-de-framed length credits the read with
            // the CLI's own gutters — characters `box_holds_paste` will never
            // search — so the widening would stop early believing it had
            // covered a window it had not.
            let chars = normalize_deframed(&stripped).len();
            let wider = (rounds < TIER1_SCAN_WIDEN_ROUNDS)
                .then(|| self.widen(requested, raw.len(), chars))
                .flatten();
            let Some(next) = wider else {
                return Some(Tier1TailRead {
                    requested_bytes: requested,
                    tail_bytes: raw.len(),
                    stripped,
                });
            };
            requested = next;
            // Monotone by construction (`widen` only returns a larger request),
            // and asserted as `max` rather than assignment so the floor cannot
            // be walked backwards by a future edit to `widen`.
            self.request_floor = self.request_floor.max(next);
            rounds += 1;
        }
    }

    /// The census for one read of this scan (#583's record, #685's sizes).
    /// `None` is a read that never happened, and the request recorded then is
    /// the one that was about to be made — built here rather than at each
    /// `json!` site for the same reason `Tier1ScanCensus::to_json` is.
    pub fn census(&self, read: Option<&Tier1TailRead>, pasted: &str) -> Tier1ScanCensus {
        Tier1ScanCensus::measure(
            read.map_or(self.request_floor, |r| r.requested_bytes),
            read.map(|r| (r.tail_bytes, r.stripped.as_str())),
            pasted,
        )
    }
}

/// What a Tier 1 box read actually ESTABLISHED (#559) — the three-state
/// replacement for the bare `box_holds_paste` bool every consumer used to
/// pass around.
///
/// The bool conflated two states that mean opposite things. `false` because
/// the tail was read and our text is not in it is evidence about the pane.
/// `false` because the tail we could read is SHORTER than our own paste is
/// arithmetic: containment cannot hold when the haystack is smaller than the
/// needle, so the answer was fixed before the pane was ever consulted. Every
/// consumer that treated the second as the first — `unconfirmed_disposition`
/// reading it as "the box holds nothing, so this pane is simply idle",
/// `stranded_selfheal_action` reading it as "its text is gone" — was drawing
/// a conclusion from a foregone `false`.
///
/// This is the same lesson #536 landed for escalation and #445 for suppressed
/// notices: a path that quietly opts out of governing must not be
/// indistinguishable from one that governed and found nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxReading {
    /// Our pasted text is identifiably at the box's tail end.
    Holds,
    /// The tail was long enough to have contained our paste, and does not.
    /// An informative absence: the CLI consumed it, collapsed it to a
    /// placeholder, or it never landed.
    NotHolding,
    /// No usable read: the pty is gone, or the tail we got back is shorter
    /// than our own paste. `box_holds_paste` is false either way, and that
    /// `false` says nothing at all about the pane.
    Unverifiable,
}

impl BoxReading {
    /// Stable audit token, so every record of a Tier 1 reading spells the
    /// three states the same way — kept next to the enum rather than
    /// stringified at each `json!` site, which is how two records of the same
    /// fact drift into two vocabularies.
    pub fn as_str(self) -> &'static str {
        match self {
            BoxReading::Holds => "holds",
            BoxReading::NotHolding => "not-holding",
            BoxReading::Unverifiable => "unverifiable",
        }
    }
}

/// Classify one Tier 1 box read (#559). `stripped_tail` is `None` when the
/// tail could not be read at all (pty gone).
///
/// Ordering matters: a positive `box_holds_paste` is decided FIRST, so a
/// reading that did find our text can never be downgraded by the length
/// arithmetic below it (it cannot be — `Holds` implies the tail was at least
/// as long as the paste — but the order makes that obvious rather than
/// inferred).
#[doc(hidden)] // pub for integration tests
pub fn box_reading(stripped_tail: Option<&str>, pasted: &str) -> BoxReading {
    let Some(tail) = stripped_tail else { return BoxReading::Unverifiable };
    if box_holds_paste(tail, pasted) {
        return BoxReading::Holds;
    }
    // An empty paste is `NotHolding`, matching `box_holds_paste`'s own
    // "nothing to still be holding" — the answer is false for a reason that
    // is about the paste, not about how much tail we could see. Both routes
    // must agree it is empty: a paste that is ONLY framing (`* * *`) de-frames
    // to nothing while still being text under the flat route.
    if normalize_deframed(pasted).is_empty() && normalize_prompt_text(pasted).is_empty() {
        return BoxReading::NotHolding;
    }
    // **Every arm below is asked of BOTH routes** (#821, rev-307). Once
    // `box_holds_paste` became a disjunction, an arm that consults one
    // normalization is asking about one of the two comparisons that could have
    // found our text — and answering for both. That asymmetry is this file's
    // recurring defect, now in its third instance:
    //
    // - the LENGTH arm. De-framing shrinks a frame-heavy needle (a markdown
    //   table row) more than it shrinks an un-gutted tail, so the de-framed
    //   test can decline to fire exactly where the flat one would have. On a
    //   truncated read that lands on `NotHolding` where the pre-#821 code said
    //   `Unverifiable` — the dangerous direction, on a narrower trigger than
    //   the containment defect but the same kind.
    // - the PROBE arm, found by the rev-307 sweep rather than reported. The
    //   probe carries whatever frame characters are MID-line in our own text,
    //   and a wrap that pushed one to a row start removes it from the de-framed
    //   tail — so a de-framed probe can miss where a flat probe matches. It
    //   only bites once both containments have failed, which a truncated read
    //   supplies.
    //
    // Asking each arm per route and OR-ing makes the property structural
    // instead of a claim to re-verify every time this function grows an arm:
    // any route that could have found the text gets to say "I could not tell".
    // `Unverifiable` is the safe answer here, so a union is the safe shape.
    for norm in [normalize_deframed as fn(&str) -> String, normalize_prompt_text] {
        let norm_pasted = norm(pasted);
        if norm_pasted.is_empty() {
            continue; // this route has nothing to look for; the other may.
        }
        let norm_tail = norm(tail);
        if norm_tail.len() < norm_pasted.len() {
            return BoxReading::Unverifiable;
        }
        // **A PARTIAL match is not an absence.** The length test above only
        // catches a tail too SHORT to have held our paste; per-row decoration
        // ADDS characters, so the tail that defeated containment is typically
        // LONGER than the paste it failed to contain and sails past it. What
        // is left would be `NotHolding`: not an absence of evidence, but
        // counterfeit evidence of an absence, which is the single input
        // `stranded_marker_action`'s retirement licence does not defend
        // against.
        //
        // So: if our paste's own line is still sitting in the window while the
        // whole of it will not match, the text is evidently there and the
        // rendering is what we failed to read. That covers decoration nobody
        // has catalogued — a trailing gutter, the scrollbar's `┃` on the right
        // edge, a re-indent, whatever the next TUI release paints — without
        // modelling any of it.
        if let Some(probe) = paste_echo_probe(pasted, norm) {
            if box_tail_window(&norm_tail, norm_pasted.len()).contains(&probe) {
                return BoxReading::Unverifiable;
            }
        }
    }
    BoxReading::NotHolding
}

/// The strongest fragment of `pasted` that a single rendered ROW can be
/// expected to hold intact — [`box_reading`]'s partial-match probe — or `None`
/// where no line is long enough to be evidence of anything (#821).
///
/// **Why a line and not the flattened paste.** The obvious probe is "the first
/// N characters of the needle", and it is wrong for exactly the reason the
/// needle itself failed: as soon as N runs past the first LOGICAL line it spans
/// a `\n` the tail never had, so it can only match by accident. A probe that
/// dies to the same thing it is meant to detect is not a backstop. Taking it
/// from one line keeps the probe a contiguous run of the pane's own words.
///
/// Note this is about logical lines, not rendered rows: a long line the CLI
/// wrapped is still one contiguous run in [`normalize_deframed`]'s output (see
/// [`PASTE_ECHO_PROBE_CHARS`] for what does and does not survive a row
/// boundary).
///
/// The LONGEST line, because that is the strongest evidence available: the more
/// of our own prose a fragment carries, the less it can be something the CLI
/// happened to paint. Truncated to [`PASTE_ECHO_PROBE_CHARS`] so a long line
/// that WRAPS still probes only its first row, and floored at
/// [`PASTE_ECHO_PROBE_MIN_CHARS`] so a paste of short lines yields no probe at
/// all rather than a coincidence-prone one — that paste keeps the pre-#821
/// behaviour, which is the honest outcome when there is no evidence to be had.
///
/// **Residual, stated accurately** (rev-305 Ground 4 corrected an earlier,
/// more pessimistic version of this): the probe fails only where its span
/// crosses a row boundary that carries decoration `deframe` cannot reach, or a
/// hard mid-word wrap — a narrow pane raises the odds of crossing a boundary
/// at all, it does not itself break anything. Where that happens the reading
/// falls through to `NotHolding`.
///
/// **Why that is still not a regression** (rev-306 F1). An earlier wording
/// justified this with "containment is wrap-agnostic", and that does not
/// survive the second cause above: a hard mid-word wrap splits one word into
/// two tokens, so it defeats CONTAINMENT too, not merely the probe. The
/// conclusion holds on a different argument — that case is **unchanged by
/// #821, not introduced by it**. `normalize_prompt_text` flattened the break
/// into a space the needle lacked in exactly the same way, so a hard mid-word
/// wrap read `NotHolding` before this change and reads `NotHolding` after it.
/// What #821 moves is elsewhere, and both directions are away from the
/// dangerous reading: de-framing turns the gutter case from `NotHolding` into
/// `Holds`, and the probe turns residual failures into `Unverifiable`.
///
/// **This probe cannot be the thing that guarantees no regression, and an
/// earlier version of this doc wrongly implied it could** (rev-306 B1). A wrap
/// that pushes a mid-line `|`/`*` to a row start strips it from the tail and
/// not from the needle — and this probe samples that same needle line, so it
/// carries the same character and fails for the same reason. A backstop that
/// shares the failure mode of what it backstops adds nothing there. Worse, the
/// bound previously stated here (a paste whose longest line is under
/// [`PASTE_ECHO_PROBE_MIN_CHARS`]) governs only whether a probe is FORMED and
/// says nothing about whether a formed probe MATCHES: a wrap boundary lands
/// inside the 48-character sample whenever the composer is under 48 columns, so
/// the exposure was an ordinary 25-47 column split, not a degenerate pane.
///
/// The guarantee lives in [`box_holds_paste`] instead, where it is structural:
/// the de-framed route is a SUPERSET of the flat one, so #821 can only add
/// `Holds` readings. This probe's job is narrower — turning a residual
/// containment failure into `Unverifiable` rather than a confident absence.
///
/// **`norm` is a parameter for the same reason (rev-307 sweep).** A probe built
/// under one normalization and searched in a tail under that same one is a
/// probe for ONE of [`box_holds_paste`]'s two routes; run alone it answers for
/// both, and can therefore miss where the other route's probe would have hit.
/// [`box_reading`] runs it once per route and takes the union, so no route's
/// evidence is silently spent on the other's behalf.
fn paste_echo_probe(pasted: &str, norm: fn(&str) -> String) -> Option<String> {
    pasted
        .lines()
        .map(norm)
        .max_by_key(|l| l.chars().count())
        .filter(|l| l.chars().count() >= PASTE_ECHO_PROBE_MIN_CHARS)
        .map(|l| l.chars().take(PASTE_ECHO_PROBE_CHARS).collect())
}

/// One Tier 1 box read, MEASURED (#583) — the numbers the `BoxReading` beside
/// it was decided from, so a live group's audit log can say whether the read
/// is routinely too short rather than only that this one was.
///
/// #559 sized the read from the paste and accepted a residual: the slack it
/// adds (`BOX_TAIL_SCAN_BYTES`) is counted in RAW bytes, while the containment
/// it protects runs on the tail AFTER `strip_ansi` and `normalize_prompt_text`.
/// A repaint-heavy TUI tail can therefore strip below the length of the very
/// paste it is meant to contain, and near-cap deliveries land in
/// `Unverifiable` — the SAFE direction (stated, notified, never a false
/// confirm), but delivering none of the verification coverage the derived read
/// was for. Whether that is the norm or the exception turns on one number
/// nothing recorded: how many characters survive per raw byte of a live pane's
/// tail.
///
/// So this is instrumentation, and ONLY instrumentation — nothing here decides
/// anything, deliberately. #583's question is what the distribution is; the
/// slack is a tuning decision that should follow the measurement rather than a
/// guess about it. `tier1_decline` already says WHICH reading was reached;
/// this says how close it ran, in the terms that would size a fix:
/// `retained_pct` is the density the slack has to buy through, and
/// `margin_chars` is the headroom this read had over its own needle.
#[doc(hidden)] // pub for integration tests
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier1ScanCensus {
    /// What the ring was asked for — since #685 the FINAL request of a read
    /// that may have widened itself, not the `tier1_scan_bytes` floor it
    /// started from. Both jq recipes in the design note still read it the same
    /// way: it remains "the window this reading was decided against", which is
    /// what `tail_bytes == requested_bytes` (the short-ring confounder) and
    /// every breakeven computed from it depend on.
    pub requested_bytes: usize,
    /// Raw bytes the ring actually returned — short of `requested_bytes`
    /// whenever the pane has simply not produced that much output yet, which
    /// is the confounder any reading of `tail_chars` has to rule out first.
    /// `None` is no read AT ALL (pty gone), never `Some(0)`: those are
    /// different facts, and not conflating them is the whole of #559.
    pub tail_bytes: Option<usize>,
    /// What those bytes came to after `strip_ansi` + `normalize_deframed` —
    /// the haystack the containment check actually gets (#821).
    pub tail_chars: Option<usize>,
    /// The needle's own length under that SAME normalization (#821). Not the
    /// same number as `tier1_paste_bytes` (which is raw), nor as the
    /// `normalize_prompt_text` length the read-sizing sites use, and it is
    /// this one the arithmetic compares against.
    pub paste_chars: usize,
}

impl Tier1ScanCensus {
    /// Measure one read. `read` is `None` when no read happened at all;
    /// otherwise `(raw bytes returned, the stripped text those bytes made)` —
    /// ONE option rather than two, so a call site cannot answer "was there a
    /// read" two ways.
    pub fn measure(requested_bytes: usize, read: Option<(usize, &str)>, pasted: &str) -> Self {
        Self {
            requested_bytes,
            tail_bytes: read.map(|(raw, _)| raw),
            // The same normalization `box_reading` compares through, not a
            // char count of the stripped text: whitespace collapse is a real
            // part of the shrink (a TUI pads every box row out to the terminal
            // width), and a census that measured only the ANSI half would
            // under-report the loss that actually decides the reading.
            //
            // #821: "the same normalization" is now `normalize_deframed`, and
            // BOTH lines move with it. `margin_chars` is a DIFFERENCE, so it
            // is only meaningful while its two terms measure the same kind of
            // string — the first cut of this moved `tail_chars` alone and left
            // `paste_chars` on `normalize_prompt_text` one line below, which
            // differenced two normalizations and understated the margin by
            // exactly the framing characters a brief's own markdown bullets
            // and table rows carry (rev-305 B1). That is the shape `h5` exists
            // for, so the bias was systematic, one-directional, and pointed at
            // over-reporting the very arm this number is read to count.
            //
            // Contrast the read-SIZING sites (`tier1_scan_bytes`,
            // `Tier1Scan::for_paste`), which deliberately keep
            // `normalize_prompt_text`: those size a REQUEST, where an
            // un-de-framed needle only ever asks for more tail and larger is
            // the safe direction. These two MEASURE, and a measurement has to
            // match the comparison it describes.
            tail_chars: read.map(|(_, stripped)| normalize_deframed(stripped).len()),
            paste_chars: normalize_deframed(pasted).len(),
        }
    }

    /// Characters of headroom the read had over the text it was looking for.
    /// **Negative is exactly the LENGTH `Unverifiable` arm** for every paste
    /// `deliver_prompt` can actually make (a non-empty one): `box_reading`
    /// compares the same two normalized lengths, so a histogram of this over a
    /// live group IS the distribution #583 asks for rather than a proxy for
    /// it. An empty paste is `NotHolding` for its own reason and never reaches
    /// that arm, so the equivalence holds there vacuously rather than by
    /// exception. `None` is the one `Unverifiable` this cannot speak for — no
    /// read happened, so there is no margin, which is itself the reading.
    ///
    /// **#821 added a SECOND `Unverifiable` arm this cannot see.** A read with
    /// ample margin can still be unreadable — the partial-match arm fires on a
    /// tail that is *longer* than the paste, which is the whole shape of the
    /// gutter defect. So a non-negative margin no longer implies the reading
    /// was decisive, and a histogram of this measures short reads only. Said
    /// plainly rather than left for a reader to discover, because "negative is
    /// exactly the arm" was true when written and quietly stopped being so.
    ///
    /// **And since the rev-307 sweep there are two LENGTH arms, of which this
    /// measures one.** `box_reading` runs its length test once per
    /// normalization and takes the union, so a read can be `Unverifiable` on
    /// the flat route's arithmetic while this de-framed margin is comfortably
    /// positive. The implication (negative margin ⇒ `Unverifiable`) still
    /// holds — a union only ever adds — but the converse is now false twice
    /// over rather than once.
    pub fn margin_chars(&self) -> Option<i64> {
        self.tail_chars.map(|chars| chars as i64 - self.paste_chars as i64)
    }

    /// Characters surviving per 100 raw bytes read — the ANSI/whitespace
    /// density #583 exists to measure, since it is what a raw-byte slack has
    /// to buy through. `None` when nothing was read, and also for a zero-byte
    /// read, where the ratio is undefined rather than 0%.
    pub fn retained_pct(&self) -> Option<u32> {
        let raw = self.tail_bytes.filter(|n| *n > 0)?;
        let chars = self.tail_chars?;
        Some(((chars as u64 * 100) / raw as u64) as u32)
    }

    /// The audit shape, built once here rather than at each `json!` site — the
    /// same argument `BoxReading::as_str` makes next to its own enum: two
    /// records of one fact in two vocabularies cannot be aggregated, and this
    /// record exists only to be aggregated. The derived fields are carried
    /// rather than left to the reader for the same reason.
    pub fn to_json(&self) -> Value {
        json!({
            "requested_bytes": self.requested_bytes,
            "tail_bytes": self.tail_bytes,
            "tail_chars": self.tail_chars,
            "paste_chars": self.paste_chars,
            "margin_chars": self.margin_chars(),
            "retained_pct": self.retained_pct(),
        })
    }
}

/// The `promptsubmit` hook marker path for one agent — a sibling of the
/// `.precompact.json`/`.sessionstart-compact.json` markers in the same
/// group's `hooks/` dir (`read_hook_marker_ts`'s doc). JSONL, not a single
/// overwritten file: unlike the compact markers (where only the LATEST
/// firing's mtime matters), confirmation here needs to see every record
/// since a per-delivery baseline OFFSET, so appending (never truncating) is
/// required.
///
/// #904: takes a [`GroupId`] and builds through [`group_dir_at`], so it is
/// infallible again — there is no id it can be handed that would escape the
/// root. It briefly returned `Option` during the first slice, when it still
/// took a `&str` and validated at its own join; that was one of the raw joins
/// the second slice removed.
#[doc(hidden)] // pub for integration tests
/// **Takes a validated agent id (#925), for the same reason it takes a
/// `GroupId`.** The id becomes part of a FILE NAME under the group's `hooks`
/// dir, so an unvalidated string here is a second path-assembly point wearing a
/// `format!`. Infallible by construction rather than by trust: the caller has to
/// hold the proof before it can call, which is what keeps this function's return
/// type a plain `PathBuf` instead of reintroducing the `Option` #904 removed.
pub fn promptsubmit_marker_path(root: &Path, group: &GroupId, agent_id: &PathSegment) -> PathBuf {
    group_dir_at(root, group)
        .join("hooks")
        .join(format!("{agent_id}.promptsubmit.jsonl"))
}

/// This delivery's baseline byte length into the `promptsubmit` marker,
/// snapshotted before it pastes anything (see `promptsubmit_records_since`'s
/// doc for why). `0` for a missing file — no hook has fired for this agent
/// this session, and offset 0 is a safe baseline (every record in the file,
/// once one exists, counts).
#[doc(hidden)] // pub for integration tests
pub fn promptsubmit_marker_len(path: &Path) -> usize {
    fs::metadata(path).map(|m| m.len() as usize).unwrap_or(0)
}

/// Read the `promptsubmit` marker and resolve it against `pasted` since
/// `offset` — the impure half of the hook confirmation tier
/// (`promptsubmit_records_since` + `prompt_landed` are the pure decision).
/// A missing/unreadable file resolves to `PromptLandedMatch::None`, the same
/// "hook never fired / isn't configured" degrade every other reader in this
/// module uses (`read_hook_marker_ts`'s doc) — never an error the delivery
/// thread has to branch on.
#[doc(hidden)] // pub for integration tests
pub fn poll_promptsubmit_hook(path: &Path, offset: usize, pasted: &str) -> PromptLandedMatch {
    let Ok(content) = fs::read_to_string(path) else { return PromptLandedMatch::None };
    prompt_landed(&promptsubmit_records_since(&content, offset), pasted)
}

// ─────────────────────────── #445: delivery queue ───────────────────────────
//
// "Hold means queued, never doomed." `deliver_prompt`'s three hold-cap seams
// used to DESTROY the payload once their bounded wait expired (see
// `queue.rs`'s module doc for the full argument). The fix keeps the caps —
// they bound *thread blocking*, a legitimate concern — but changes what
// happens AT the cap: enqueue, not destroy. This section is the impure half
// (queue map, drainer thread); `queue.rs` is the pure policy.

/// What admitting a payload into a pane's queue (`OrchRegistry::enqueue_text`)
/// resolved to (#470): `id` for audit/removal purposes, and `was_first` —
/// whether the queue was empty immediately before this admission, decided
/// atomically with the push. See `enqueue_text`'s doc for why `was_first`
/// is the one fact the whole ordering fix hangs off of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmitOutcome {
    pub id: u64,
    pub was_first: bool,
    /// #517: this admission collapsed into an ALREADY-QUEUED byte-identical
    /// entry (`queue::AdmitDecision::Coalesce`) rather than adding one.
    /// Reported out of the same critical section that decided it, so a
    /// caller that must not act twice on one payload — the lost-kickoff
    /// re-delivery — can tell "queued" from "already queued" without a
    /// second, racy look at the queue.
    pub coalesced: bool,
}

/// The part of a delivery target's identity that outlives a loomux restart
/// (#467) — resolved once at admission by `OrchRegistry::durable_target` and
/// stamped onto the queued entry, because at recovery time the agent it came
/// from may not exist. See `queue::QueuedDelivery`'s `to_orchestrator` /
/// `session_id` fields for why the obvious keys (`pty_id`, `agent_id`) are
/// both re-minted by a restore and therefore cannot serve.
#[derive(Clone, Debug, Default)]
struct DurableTarget {
    is_orchestrator: bool,
    session_id: Option<String>,
}

/// What `deliver_now` reports back to its caller — always `run_queue_drainer`
/// as of #470 (the front door no longer calls `deliver_now` directly; see
/// its doc). `Done` means nothing is left to queue; the two `Aborted*`
/// variants mean the entry stays exactly where it already sits at the front
/// of the queue (#470: every entry is admitted BEFORE its first delivery
/// attempt, unlike pre-#470 where a fresh delivery's abort had to enqueue
/// something that wasn't in the queue yet) — the drainer's own match on the
/// outcome decides whether to retry in place, convert to a
/// `StrandedSubmit` marker, or pop and move on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeliverOutcome {
    /// Ran to a terminal state that is NOT a hold-cap abort: delivered and
    /// resolved by #112's three-state machinery (Confirmed/Pending/Failed),
    /// or the pty closed mid-delivery. Either way nothing is left to queue
    /// — a closed pty has nowhere to deliver to, and a completed attempt
    /// already entered #451's own (untouched) confirmation lifecycle.
    Done,
    /// A pre-paste hold (box-occupied or question) capped out — nothing was
    /// pasted. The exact text this call was given still needs queuing.
    AbortedPrePaste(queue::EnqueueReason),
    /// A pre-Enter gate declined (seam 3): the text WAS pasted, only the Enter
    /// was withheld. `record_aborted_preenter_outcome` has already run (so the
    /// NEXT thing to touch this pane's box sees it as stranded) — the caller's
    /// job is to queue a `StrandedSubmit` marker, never the text again.
    ///
    /// **Carries its reason since #532 (rev-12 NB1).** This used to have one
    /// cause — the question hold — so the drainer hardcoded
    /// `EnqueueReason::Question` in the notice it sends. #532 gave the path a
    /// second cause (the pre-Enter occupancy gate), which made that notice tell
    /// the orchestrator "an interactive question is on screen" for a pane whose
    /// only blocker was the human's own half-typed line — sending it to look
    /// for a dialog that does not exist. Mirrors `AbortedPrePaste`, which has
    /// carried its reason all along, and closes the same mislabel
    /// `write_admission_badges_the_gate_that_actually_blocked` exists to
    /// prevent.
    AbortedPreEnter(queue::EnqueueReason),
    /// #813: a `StrandedSubmit` marker was dropped without pressing anything —
    /// see `stranded_marker_action`. Its own variant rather than `Done`
    /// because `Done`'s whole meaning is that the pane took a write, and this
    /// pane did not: reusing it would make `HoldObservation::Delivered` — "the
    /// pane accepted a write" — a false claim in the audit trail, which is the
    /// unbacked-claim class this module mints separate variants to avoid
    /// (`QuestionStale` vs `Question`, `QueueFull` vs `Exhausted`).
    Retired(StrandedRetireReason),
}

/// Why a queued `StrandedSubmit` marker was retired instead of pressed
/// (#813). Each variant is a distinct audit token, because "there was nothing
/// left to submit", "a human submitted it for us" and "our text left the box
/// with nobody at the keyboard" are three different facts about the same pane,
/// and a shared string could not tell a reader which one happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrandedRetireReason {
    /// The pane's delivery ledger no longer says anything is stranded — the
    /// last delivery confirmed, or there is no record at all (a restart drops
    /// the in-memory ledger while the queue is persisted). Pressing Enter
    /// against that is a blind press with nothing behind it.
    NothingStranded,
    /// #813's incident cell. Our stranded text is verifiably **gone from the
    /// box**, and a human keystroke landed on this pane after our own submit —
    /// so a person dealt with it, which is exactly what the queue was waiting
    /// for and exactly what it could not see.
    HumanResolved,
    /// Our stranded text is verifiably gone from the box, and **no** human
    /// keystroke is on record since our submit — the CLI consumed it, replaced
    /// it with a placeholder, or it never really landed. Nothing left to press
    /// Enter against either way, but deliberately NOT `HumanResolved`: that
    /// name would put a person in the audit trail who was never there, and it
    /// is the one of these two that licenses taking the badge down.
    TextGone,
}

impl StrandedRetireReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StrandedRetireReason::NothingStranded => "nothing-stranded",
            StrandedRetireReason::HumanResolved => "human-resolved",
            StrandedRetireReason::TextGone => "text-gone",
        }
    }

    /// Whether this retirement is evidence that the pane no longer needs a
    /// human, i.e. whether it takes the `attn_stranded` chip down.
    ///
    /// Only `HumanResolved`. `TextGone` says our text left the box with nobody
    /// at the keyboard, which is precisely [`StrandedBlocker::NotHolding`]'s
    /// situation — "never confirmed and its text is gone — check the pane" —
    /// and clearing on it would answer a question loomux cannot answer.
    /// `NothingStranded` establishes nothing about the pane at all.
    pub fn resolves_the_pane(self) -> bool {
        matches!(self, StrandedRetireReason::HumanResolved)
    }
}

/// What the drainer should do with the `StrandedSubmit` marker at the front of
/// a pane's queue (#813) — the precedence as a VALUE, for the reason
/// `failed_arm_route` gives for the same shape: a `return`-ordered version of
/// this lives only as control flow inside a function no test in this repo can
/// construct, and #585 is the precedent for that being how a wrong ordering
/// survives review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrandedMarkerAction {
    /// Press the Enter the stranded delivery never got.
    Press,
    /// Drop the marker and let the queue move on.
    Retire(StrandedRetireReason),
    /// A live gate is in the way, or our text is still sitting there and
    /// cannot be pressed yet — leave the marker queued and retry, carrying the
    /// gate that actually declined so the caller's notice and audit name it
    /// (the mislabel #532 rev-12 NB1 closed on `AbortedPreEnter`, arriving
    /// here).
    Retry(queue::EnqueueReason),
}

/// **A marker is a repair, not a payload** — and that is the whole argument for
/// #813 (see `docs/design/orchestration.md`).
///
/// Before this, a marker that could not fire stayed at the FRONT of the pane's
/// queue and was retried "next tick, no cap, exactly like every other queued
/// entry". But it is not like every other queued entry: every other one carries
/// text that exists nowhere else, whereas a marker carries an **Enter**. So the
/// marker was the one queue entry whose failure to fire cost *other* work, and
/// nothing bounded it.
///
/// Worse, its release condition was ANTI-CORRELATED with the human's own
/// recovery. `human_input_block` re-arms for `HUMAN_INPUT_BLOCK_BOUND_MS` from
/// the human's LAST keystroke, so a human who does the sane thing — click into
/// the wedged pane, press Enter to submit the stranded prompt, then keep typing
/// to talk to the CLI — pins the marker at the head of the queue for as long as
/// they stay engaged, and every steering prompt behind it silently never
/// delivers. That is #813's live incident, step for step.
///
/// # The evidence a retirement rests on, and why it is about the TEXT
///
/// The first cut of this fix retired on "a keystroke landed after our submit
/// **and** `input_pending` is false", and that was wrong twice over.
///
/// **It could not tell a person from a phantom.** #518 exists because a
/// terminal auto-reply can be misclassified as a keystroke, and that is exactly
/// the shape this reading has: a stamp, an empty occupancy counter. Retiring on
/// it fired inside `HUMAN_INPUT_BLOCK_BOUND_MS`, i.e. strictly before
/// `BoundedOut` — which is only reachable with `!box_pending` — could ever be
/// reached, so it did not merely coexist with #518's bound, it made that bound
/// **unreachable from this path**, and wrote `human-resolved` into the audit
/// trail for a pane no human had touched.
///
/// **And `!box_pending` is not "the box is empty".** `input_box_len` counts
/// characters the HUMAN typed; loomux's own paste never touches it. So the
/// first cut retired while our own prompt could still be sitting in that box —
/// and the next queue entry, whose `deliver_now` does NOT abort when
/// `flush_stranded_text` declines, would then paste **on top of it** and submit
/// both prompts merged as one. That is the #81/#84/#111 collision the stranded
/// flush exists to prevent, re-opened by the repair meant to help.
///
/// Both hazards have one root — reasoning about *who typed* instead of *where
/// our text is* — so both close with one change. A retirement now requires a
/// positive [`BoxReading::NotHolding`]: the tail was long enough to have
/// contained our paste and does not. Then, and only then, the keystroke record
/// picks the NAME (`HumanResolved` vs `TextGone`), which is all it was ever fit
/// to decide.
///
/// `Holds`, `Unverifiable` and "no text on record" all fall through to the
/// ordinary gates. Nothing retires on an absence of evidence — the one
/// direction that could re-open either hazard.
///
/// # What retiring cannot lose
///
/// A retirement means our text is not in the box. There is therefore no Enter
/// left for this marker to press that could submit it, and nothing for a later
/// paste to collide with. The case where our Enter WOULD still have been right
/// is precisely `Holds`, and that is the case this function keeps retrying.
///
/// Pure and total so every cell of the matrix is directly assertable.
#[doc(hidden)] // pub for integration tests
pub fn stranded_marker_action(
    prev_confirmed: Option<bool>,
    // What Tier 1 says about OUR OWN stranded text, or `None` when the ledger
    // carries no text to look for. `None` and `Unverifiable` are different
    // facts ("we have nothing to look for" vs "we looked and could not tell")
    // and both take the same conservative branch, which is why neither is
    // collapsed into the other.
    text_reading: Option<BoxReading>,
    // Whether a human keystroke is on record for this pane since our own
    // submit — `tier1_trusted`'s inverse. Names a retirement; never licenses
    // one.
    human_stamped_since: bool,
    human_block: HumanInputBlock,
    box_pending: bool,
    // A CLOSURE, not a bool, for the reason `question_hold_predicate_sampled`
    // takes a sampler: answering it costs a 64 KiB grid recomposition, and most
    // of the decisions below never need to ask. Called at most once, and only
    // on the path that actually consults it.
    question_active: impl FnOnce() -> bool,
) -> StrandedMarkerAction {
    // Nothing stranded to submit. `should_flush_before_paste`'s own condition,
    // read as a retire rather than as a decline: a decline here retried forever
    // against a ledger that can never say `Some(false)` again.
    if !matches!(prev_confirmed, Some(false)) {
        return StrandedMarkerAction::Retire(StrandedRetireReason::NothingStranded);
    }
    // The one positive reading that licenses a retirement: our text is gone
    // from the box. See the header for why nothing weaker will do.
    if text_reading == Some(BoxReading::NotHolding) {
        return StrandedMarkerAction::Retire(if human_stamped_since {
            StrandedRetireReason::HumanResolved
        } else {
            StrandedRetireReason::TextGone
        });
    }
    // #510's absolute, unchanged: human-typed characters are outstanding in the
    // box, so never press Enter over them.
    if box_pending {
        return StrandedMarkerAction::Retry(queue::EnqueueReason::BoxOccupied);
    }
    // #518, unchanged and now genuinely reachable: a human typed here since our
    // submit, so hold — but only until the bound, after which `holds()` reads
    // false and this falls through to the press. That bound is the ONLY thing
    // that releases a stamp which may have been a phantom, which is why nothing
    // above may retire on the stamp alone.
    //
    // `BoxOccupied` is the honest reason: the box is occupied — by OUR text,
    // which is the entire premise of a marker existing. Whose text it is lives
    // in the `stranded-marker-*` audit, not in the queue's reason vocabulary.
    if human_block.holds() {
        return StrandedMarkerAction::Retry(queue::EnqueueReason::BoxOccupied);
    }
    // #420, unchanged: a live question owns the Enter key.
    if question_active() {
        return StrandedMarkerAction::Retry(queue::EnqueueReason::Question);
    }
    StrandedMarkerAction::Press
}

/// Replay a queued `StrandedSubmit` marker (#445 seam 3): the text is already
/// sitting in the box from an earlier paste whose Enter was withheld — press it
/// via the SAME `flush_stranded_text` logic a normal delivery's own pre-paste
/// step already uses (guarded by `human_typed_since`, so a person's own line is
/// never blind-submitted).
///
/// #813: returns `stranded_marker_action`'s three-way rather than a bool. A
/// `Retry` is the old `false` (leave it queued); a `Retire` is new and is the
/// fix — see that function for the evidence a retirement rests on and why it
/// has to be about our text rather than about who typed.
#[doc(hidden)] // pub for integration tests (#496 PR-C drives the REAL replay)
pub fn drain_stranded_submit(
    ptys: &crate::pty::PtyManager,
    last_delivery: &TrackedMutex<HashMap<u32, DeliveryOutcome>>,
    delivery_from: String,
    pty_id: u32,
    submit: &[u8],
    // #576: threaded through to `flush_stranded_text`'s question gate — see
    // there for why this reader gets the record too.
    delivered: Vec<String>,
) -> StrandedMarkerAction {
    let prev = last_delivery.lock_safe().get(&pty_id).cloned();
    let prev_confirmed = prev.as_ref().map(|o| o.confirmed);
    // #518: the SAME derivation the self-heal's trigger used
    // (`human_input_block`), not a second opinion. rev-47 NB1 is the failure
    // this avoids: a marker admitted on one rule and pressed under another
    // never fires, and the badge quietly goes on claiming loomux is handling a
    // re-send it will decline forever.
    let human_block = prev
        .as_ref()
        .map(|o| human_input_block_now(ptys, pty_id, o.submit_sent_ms))
        .unwrap_or(HumanInputBlock::None);
    // The same `tier1_trusted` reading `human_input_block` takes, kept separate
    // because it answers a different question: the block decides whether to
    // WRITE, this decides what to CALL a retirement that has already been
    // licensed by the box reading below.
    let human_stamped_since = prev.as_ref().is_some_and(|o| {
        !tier1_trusted(ptys.last_user_input_ms(pty_id).unwrap_or(0), o.submit_sent_ms)
    });
    // #532: taken HERE, at the press, never inherited. `unwrap_or(true)` on a
    // closed pty is the fail-safe direction, matching `flush_stranded_text`.
    let box_pending = ptys.input_pending(pty_id).unwrap_or(true);
    // #813: Tier 1, on our OWN text — the same `Tier1Scan` widening read and
    // the same `box_reading` the late-confirmation monitor takes, not a second
    // implementation of either.
    //
    // **Cost, declared (performance.md §3 INV-4: cadenced work says what it
    // costs).** This runs on the drainer's `QUEUE_DRAIN_POLL` (2 s), so it is
    // fixed-cadence work and owes a bound. Three things bound it:
    //
    // 1. **Scope.** It runs only while a `StrandedSubmit` marker is at the
    //    FRONT of a pane's queue — not per pane, not per poll of an ordinary
    //    queue, and not at all on a pane that has never stranded a delivery.
    //    A marker is rare and self-limiting: this very reading is what retires
    //    it, so the work ends the condition that schedules it.
    // 2. **Per call.** One `Tier1Scan::for_paste` normalize of the recorded
    //    paste, then at most `TIER1_SCAN_WIDEN_ROUNDS` ring reads capped at
    //    `TIER1_SCAN_WIDEN_MAX_BYTES` — `Tier1Scan::widen`'s own bound, not a
    //    fresh one. No IPC and no lock beyond the ring's own.
    // 3. **Precedent.** `run_late_confirmation_monitor` takes the identical
    //    read every `LATE_MONITOR_POLL` (5 s) for the whole of a delivery's
    //    unconfirmed life, so this is a shape the app already pays at a
    //    comparable cadence, on the same panes, for the same question.
    //
    // Deliberately gated on the ledger rather than taken unconditionally: a
    // marker whose pane reports anything but `Some(false)` retires on that
    // alone (see `stranded_marker_action`'s first cell), so paying for a box
    // read there would be buying an answer nothing consults. Not hoisted out
    // of the poll the way the late monitor hoists its `Tier1Scan` — this
    // function is called fresh per pass and owns no state between them, and
    // #559 rev-13 N2's hoist is available to a future caller that does.
    let text_reading = matches!(prev_confirmed, Some(false))
        .then(|| prev.as_ref().and_then(|o| o.stranded_text.clone()))
        .flatten()
        .map(|text| {
            let mut scan = Tier1Scan::for_paste(&text);
            let read = scan.read(|n| ptys.output_tail_bounded(pty_id, n));
            box_reading(read.as_ref().map(|r| r.stripped.as_str()), &text)
        });
    let action = stranded_marker_action(
        prev_confirmed,
        text_reading,
        human_stamped_since,
        human_block,
        box_pending,
        // #576: the record reaches BOTH readers of the question gate — this one
        // and `flush_stranded_text`'s own. Fixing one and leaving the other was
        // rev-126's finding (`e7`).
        || question_active_now(ptys, pty_id, None, delivered.clone()),
    );
    if action != StrandedMarkerAction::Press {
        return action;
    }
    // The press itself still goes through `flush_stranded_text`, which
    // re-derives its own gates: this function decides, that one writes, and
    // neither trusts the other's reading.
    if flush_stranded_text(ptys, pty_id, prev_confirmed, human_block.holds(), submit, delivered) {
        last_delivery.lock_safe().insert(
            pty_id,
            DeliveryOutcome {
                confirmed: true,
                submit_sent_ms: now_ms(),
                from: delivery_from,
                // #813: pressed, so nothing is stranded any more.
                stranded_text: None,
            },
        );
        StrandedMarkerAction::Press
    } else {
        // A decline AFTER we decided `Press` means a gate flipped between the
        // two reads (or the write itself failed on a pty that has since
        // closed). Re-read the cheap half rather than asserting a reason:
        // `flush_stranded_text` consults exactly two gates, so a box that is
        // now occupied names itself, and the question is the only thing left it
        // could have been. That inference is stated rather than hardcoded —
        // the hardcoded `Question` here was itself the mislabel this PR removes
        // everywhere else.
        StrandedMarkerAction::Retry(if ptys.input_pending(pty_id).unwrap_or(true) {
            queue::EnqueueReason::BoxOccupied
        } else {
            queue::EnqueueReason::Question
        })
    }
}

/// The delivery-queue drainer (#445, generalized by #470 into the ONLY way
/// anything ever reaches `deliver_now`): one per pane with a non-empty
/// queue, spawned by `OrchRegistry::ensure_drainer`. Polls deliverability
/// at `QUEUE_DRAIN_POLL` — no cap, no timeout, matching the standing
/// requirement that a hold whose release condition is "a human answers"
/// must not expire — and replays the queue's FRONT entry the instant the
/// pane is deliverable. Exits when the queue is empty, the pty closes, or
/// the agent dies (dropping any remaining entries with an audit line +
/// notice — today that case is silent).
///
/// **Ownership discipline** (what makes ordering race-free, #470). Only
/// THIS thread ever pops from the FRONT of a given pane's queue.
/// `deliver_prompt`'s front door only ever pushes to the BACK, and — as of
/// #470 — EVERY delivery goes through that same push, atomically with the
/// emptiness check that decides whether it's the one to spawn this thread
/// (`enqueue_text`'s `was_first`) or whether it's landing behind an
/// existing queue a drainer is already (or about to be) working through.
/// There is no longer a separate "race a raw mutex, bypass the queue
/// entirely" path for a later arrival to use to cut ahead — see
/// `docs/design/orchestration.md`'s Ordering subsection for the argument
/// this closes (a plain fair mutex does NOT: a reviewer proved a delivery
/// deferred at its OWN paste-point recheck can still lose its arrival
/// position to a later arrival that queued via a bypass the fair lock never
/// touched).
///
/// **Zero added latency for the common case (#470).** This thread's FIRST
/// pass never sleeps and never pre-checks deliverability — it calls
/// `deliver_now` immediately, exactly as the pre-#470 direct-spawn path
/// did, and `deliver_now`'s own internal waits (bounded, with the same caps
/// as ever) do the real work. Every later pass (a retry, or a second entry
/// that piled up behind the first) uses the normal poll cadence, exactly as
/// before #470.
///
/// **Panic safety (rev-35 review, NB2).** Removal from `queue_draining` is
/// an RAII guard (below), not a manual call at each exit — a panic
/// anywhere in a drain attempt (including inside `deliver_now`, which this
/// thread calls directly, unguarded by `catch_unwind`) used to leave the
/// pty latched in `queue_draining` forever, permanently blocking every
/// future `ensure_drainer` call for that pane. The guard's `Drop` runs on
/// unwind exactly like it does on a normal `return`.
///
/// **Generation-checked, not unconditional (#470 B1 review round 2).** A
/// committed exit (`OrchRegistry::commit_exit`) ALREADY deregisters
/// `pty_id` atomically with confirming the queue is empty — this guard
/// still runs afterwards regardless (it has no way to know commit_exit
/// already acted; RAII fires on every `return`, committed or not). If its
/// `Drop` unconditionally removed `pty_id` again, it could erase a
/// SUCCESSOR drainer's live registration: a fresh delivery can arrive,
/// `was_first: true`, spawn Drainer2, and register — all in the window
/// between Drainer1's `commit_exit` and Drainer1's OWN guard dropping —
/// and an unconditional second removal would strip Drainer2's
/// registration out from under it while it's still running (possibly
/// mid-`deliver_now`, a 60–120s hold), letting a THIRD arrival spawn
/// Drainer3 concurrently with Drainer2 — two live drainers walking the
/// same queue, the same entry pasted twice. This is why `generation`
/// exists: `commit_exit` and this `Drop` both remove `pty_id` ONLY if the
/// CURRENTLY stored generation still matches the one THIS drainer was
/// minted at spawn (`ensure_drainer`). Whichever of them acts first wins
/// the real removal; whichever acts second finds either nothing to remove
/// (already gone) or a DIFFERENT generation (a successor already claimed
/// it) and is a no-op either way — structurally, not because every call
/// site remembered to "arm"/"disarm" anything. See
/// `unified_admission_property::drainer_lifecycle` (queue.rs) for the
/// exhaustive proof, including the stale-guard-drop event modeled
/// explicitly rather than assumed away.
///
/// **#497: the generation check is now the only removal that exists.**
/// `queue_draining` is a [`queuestate::DrainerRegistry`], whose sole
/// removal takes the generation — so the paragraph above stopped being a
/// rule this `Drop` had to honour and became a rule it cannot state
/// otherwise. That matters for the case a property test structurally
/// cannot reach: `drainer_lifecycle` models the three call sites that
/// existed when it was written, so a raw removal added at a FOURTH site
/// changes nothing the model explores. See `queuestate.rs`.
struct DrainerGuard {
    queue_draining: Arc<queuestate::DrainerRegistry>,
    pty_id: u32,
    generation: u64,
}

impl Drop for DrainerGuard {
    fn drop(&mut self) {
        // Whether this fired or found a successor's generation is not this
        // guard's business — both are correct outcomes, which is the point
        // of the generation (see the type doc above).
        self.queue_draining.release(self.pty_id, self.generation);
    }
}

/// The kickoff-specific behavior (`Delivery::wait_ready`/
/// `confirms_autopilot_dialog`) a FRESH delivery resolved at
/// `deliver_prompt`'s front door, threaded through to this drainer's very
/// first pass ONLY (#470). Every pre-#470 replay already hardcoded
/// `false, false` here — a queued entry, by the time anything replays it,
/// is never "the first prompt to a just-booted CLI" in the sense that
/// matters; this preserves that exactly for iteration 2+, and restores it
/// for iteration 1 where #470's unification would otherwise have silently
/// dropped it (a freshly spawned drainer used to only ever mean "replaying
/// a hold-cap timeout," never "the very first attempt").
///
/// `id` guards against the (purely theoretical — see `ensure_drainer`'s
/// doc) race where a DIFFERENT, non-kickoff caller's `ensure_drainer` call
/// happens to win the idempotent spawn instead of the kickoff's own: if the
/// front entry on the first pass isn't this id, `wait_ready`/
/// `confirm_autopilot` are NOT applied — falling back to the always-safe
/// `false, false` rather than misapplying kickoff behavior to a different
/// delivery.
struct FreshFirstAttempt {
    id: u64,
    wait_ready: bool,
    confirm_autopilot: bool,
    /// #517: this attempt is a FRESH spawn's kickoff brief specifically
    /// (`Delivery::FreshKickoff`), the one payload with no other route to
    /// the agent if it never lands. Deliberately NOT the same fact as
    /// `wait_ready`, which is also true for a resume re-sync: the boot wait
    /// is about "hold the paste", this is about "the payload is
    /// unrecoverable". See `kickoff_recovery_action`.
    fresh_kickoff: bool,
}

/// The ONE place a decided `KickoffTreatment` is copied onto the attempt it
/// arms (#620 review NB1).
///
/// `KickoffTreatment`'s own doc argues these three flags are "the same type
/// and mean opposite things, so a positional tuple is one transposed edit
/// away from arming the autopilot watcher on a recovery" — and then every
/// producer copied them across field by field, by hand. A transposition in
/// one of those copies is invisible: the treatment functions still return the
/// right answer (so their tests pass), `kickoff_panes` is derived from
/// `is_some()` (so the audit test passes), and only the drainer — private
/// struct, needs a real `AppHandle`, unreachable by every test in this repo —
/// sees the wrong flags. The mapping cannot be made testable, so it is made
/// singular instead: #620 established that this treatment gains producers,
/// and each new one now inherits the hazard rather than re-creating it.
impl From<(u64, KickoffTreatment)> for FreshFirstAttempt {
    fn from((id, t): (u64, KickoffTreatment)) -> Self {
        FreshFirstAttempt {
            id,
            wait_ready: t.wait_ready,
            confirm_autopilot: t.confirm_autopilot,
            fresh_kickoff: t.fresh_kickoff,
        }
    }
}

fn run_queue_drainer(
    reg: Arc<OrchRegistry>,
    app: AppHandle,
    group: GroupId,
    pty_id: u32,
    fresh_first: Option<FreshFirstAttempt>,
    // #470 B1 review round 2: the generation `ensure_drainer` minted for
    // THIS spawn — threaded through so both this thread's own
    // `DrainerGuard` and its `commit_exit` calls remove `queue_draining`'s
    // entry only if it's still THIS generation. See `DrainerGuard`'s doc.
    generation: u64,
) {
    let _draining_guard = DrainerGuard { queue_draining: reg.queue_draining.clone(), pty_id, generation };
    let ptys = app.state::<crate::pty::PtyManager>();
    // #470: suppressed until real contention is observed (a retry, or more
    // than one entry queued) — see the loop body below. Pre-#470, a drainer
    // only ever existed BECAUSE something already needed to queue, so
    // showing it unconditionally on the first successful send was correct;
    // #470 also spawns this thread for the common, uncontended, zero-hold
    // case, which must never claim anything was "queued while blocked."
    let mut header_pending = false;
    // #563: which reason THIS drainer currently has the pane-header
    // delivery-held chip up for, or `None` for no chip. Owned here rather than
    // in `held_escalation` for the reason `HeldEscalation::Chip`'s doc gives:
    // "is the pane held" is a fact about the pane, "have we said so, and as
    // what" is drainer state. Kept per-drainer (not in the registry) because a
    // drainer is per-pty and its exit is exactly when the chip must come down —
    // see the `ChipGuard` below, which drops it on EVERY exit from this
    // function, including the early returns for a closed pty or a dead agent.
    //
    // **The REASON, not just a bool (rev-10 finding 3).** A bool made the
    // reason freeze at whatever the first blocked poll saw, for the whole
    // episode. The two gates genuinely alternate mid-hold — that alternation is
    // the documented behaviour `PREPASTE_RECHECK_ROUNDS` exists for — and the
    // two chips read materially differently ("submit or clear the text in this
    // pane's box" vs "an interactive question is on screen, answer it"). A
    // frozen reason can tell a human to clear an empty box while a dialog is
    // what is actually waiting, which is a false claim on the very badge this
    // PR added to stop false silence.
    //
    // (a `Cell`, purely so the RAII guard below can share it with the loop
    // body — nothing here is concurrent.)
    let chip_reason: std::cell::Cell<Option<HeldReason>> = std::cell::Cell::new(None);
    // #563: a chip left up by a drainer that exited would be a permanent lie
    // on a pane nothing is holding — the mirror image of the bug this fixes,
    // and worse, because a stale "held" chip trains a human to ignore the real
    // one. RAII rather than a clear before each `return`: this function has
    // four exits and a future edit would add a fifth.
    struct ChipGuard<'a> {
        app: &'a AppHandle,
        pty_id: u32,
        shown: &'a std::cell::Cell<Option<HeldReason>>,
    }
    impl Drop for ChipGuard<'_> {
        fn drop(&mut self) {
            if self.shown.get().is_some() {
                let _ = self
                    .app
                    .emit("orch-delivery-held-cleared", delivery_held_cleared_event(self.pty_id));
            }
        }
    }
    let _chip_guard = ChipGuard { app: &app, pty_id, shown: &chip_reason };
    // Raise/lower the chip. `raise` is called on every held poll and emits only
    // when the chip is not already up FOR THIS REASON, so a pane held for hours
    // on one gate produces one event rather than one every `QUEUE_DRAIN_POLL`,
    // while a pane whose blocking gate actually changes re-words its chip. The
    // frontend's `setHeld` overwrites, so a re-emit needs no paired clear.
    let raise_chip = |agent_id: &str, reason: HeldReason| {
        if chip_reason.get() != Some(reason) {
            chip_reason.set(Some(reason));
            let _ = app.emit(
                "orch-delivery-held",
                delivery_held_event(agent_id, &group, pty_id, reason),
            );
        }
    };
    let lower_chip = || {
        if chip_reason.get().is_some() {
            chip_reason.set(None);
            let _ = app.emit("orch-delivery-held-cleared", delivery_held_cleared_event(pty_id));
        }
    };
    let mut iteration = 0u32;
    // #903: consecutive polls whose FRESH composed-screen read showed this pane
    // sitting at an empty input prompt. Drainer-local for the same reason
    // `chip_reason` is — a drainer is per-pty and its exit is exactly when the
    // observation stops being about anything — and a plain counter rather than a
    // timestamp because what the override needs is "still idle now", proven
    // repeatedly, not "was idle at some point".
    let mut question_idle_streak = 0u32;
    // #903 rev-427 NB4: the hold episode whose override has already been
    // recorded, so the grant can repeat while the audit line does not. `None`
    // until one fires; a new episode (the pane delivered, then wedged again) is
    // a new line, which is what a reader wants.
    let mut override_audited_since: Option<u64> = None;
    loop {
        iteration += 1;
        // #903: set by this iteration's own poll (below) when the last-resort
        // override fires. Per-iteration, never carried: an override is a
        // decision about one attempt against one reading of the pane.
        let mut question_overridden = false;
        // #470: the very first pass of a freshly spawned drainer attempts
        // immediately — no poll sleep, no outer deliverability pre-check —
        // exactly matching the pre-#470 direct path's latency for the
        // common uncontended case. `deliver_now`'s own internal waits are
        // the real (and only) gate; this outer pair exist purely to avoid
        // re-entering `deliver_now`'s setup on every 2s poll once genuine
        // contention is already known, which iteration 1 never is.
        let immediate_first_pass = iteration == 1;
        if !immediate_first_pass {
            std::thread::sleep(queue::QUEUE_DRAIN_POLL);
        }

        // Pty closed: nothing left to drain to. #470 B1: `commit_exit`
        // (not the RAII guard alone) is what makes this exit atomic with
        // deregistering from `queue_draining` — see its doc.
        if ptys.output_total(pty_id).is_none() {
            if let Some(entries) = reg.commit_exit(&group, pty_id, generation, true) {
                reg.announce_dropped(&group, entries, queue::DropReason::AgentDied);
            }
            return;
        }
        let agent_id = reg.by_pty.lock_safe().get(&pty_id).cloned();
        let agent = agent_id.as_ref().and_then(|id| reg.agent(id));
        // Agent record gone or dead, but the pty somehow lingers (teardown
        // ordering) — same treatment as a closed pty.
        if agent.as_ref().map(|a| a.status == AgentStatus::Dead).unwrap_or(true) {
            if let Some(entries) = reg.commit_exit(&group, pty_id, generation, true) {
                reg.announce_dropped(&group, entries, queue::DropReason::AgentDied);
            }
            return;
        }
        let Some(a) = agent else { continue };

        // #470 B1: decide, ATOMICALLY with deregistering from
        // `queue_draining`, whether this thread may exit — BEFORE peeking
        // anything else. A plain "peek front, see None, return" (the
        // pre-fix shape) leaves a window between that peek and the
        // `DrainerGuard`'s eventual drop where a fresh `was_first`
        // admission's own `ensure_drainer` call sees this pty still
        // marked draining, no-ops, and stands no chance of ever being
        // picked up — a silently stranded delivery the sender was
        // already told `Ok` for. `commit_exit` closes that window by
        // holding `queues`'s lock across BOTH the emptiness check and the
        // `queue_draining` removal, so whichever of "a push" or "this
        // exit" the OS schedules first is fully visible to the other.
        if let Some(entries) = reg.commit_exit(&group, pty_id, generation, false) {
            debug_assert!(entries.is_empty(), "force:false only commits when the queue was already empty");
            return;
        }
        // #569: the human paused this group — the queue HOLDS. Nothing is
        // planned, nothing is pasted, no notice is spent and no hold clock
        // runs: a pause is not a stuck pane, and escalating one as though it
        // were would badge the human about a state they set themselves.
        //
        // Placed AFTER the liveness checks and `commit_exit`, which is what
        // makes those two still reachable during a pause: an empty paused
        // queue exits its drainer rather than polling for the whole pause,
        // and a pane whose agent died mid-pause has its queue dropped and
        // announced through the ordinary `AgentDied` path instead of being
        // retained forever with nothing left to look at it.
        //
        // Reached at all only because `deliver_prompt`'s pause branch is not
        // the sole way a drainer starts: `readmit_recovered` kicks one when a
        // pane rebinds after a restart, which for a group paused across that
        // restart would otherwise paste straight into the pause.
        //
        // The chip comes down because the pane is no longer held on anything
        // the human can clear from the pane — the group's paused state is
        // what is holding it, and the group UI already says so
        // (`HoldClass::GroupPaused`'s `PausedGroupUi` channel).
        if reg.is_paused(&group) {
            lower_chip();
            // Only pass 1 sleeps here (review N2). Every later pass already
            // slept at the loop top, and sleeping again would poll a paused
            // pane at half cadence — doubling resume latency for a second
            // sleep that reads as if it were doing something. Pass 1 skips the
            // loop-top sleep by design (`immediate_first_pass`), so without
            // this the gate would spin.
            if immediate_first_pass {
                std::thread::sleep(queue::QUEUE_DRAIN_POLL);
            }
            continue;
        }
        // Not empty (confirmed above) — safe to peek normally now; only
        // this single-consumer thread ever pops the front, so nothing can
        // make it empty again out from under us before we act on it.
        //
        // #533-A: the whole queue is snapshotted, not just the front,
        // because what this pass submits is now decided over the entire
        // backlog (`queue::plan_flush`) rather than one entry at a time.
        // The snapshot is a clone taken under the lock and then released:
        // an admission racing this read lands BEHIND everything planned
        // here, so the worst case is that it flushes on the next pass —
        // never that it is skipped or reordered.
        let mut entries = reg.queue_snapshot(pty_id);
        // #533-A: superseded constituents drop out BEFORE anything is
        // combined — never merged into the paste, never silently dropped
        // either (each gets its own `delivery-dropped` audit line).
        let plan = queue::plan_flush(&entries, queue::QUEUE_FLUSH_MAX_BYTES);
        if !plan.superseded.is_empty() {
            reg.drop_superseded(&group, pty_id, &plan.superseded);
            // rev-13 F4: the drop both REMOVES entries and MOVES their
            // coalesce counts onto survivors, so every number rendered
            // below — the per-constituent "+N identical repeats", the
            // single-entry header's own depth and coalesce total — must
            // come from a re-read, not from a snapshot that predates it.
            entries = reg.queue_snapshot(pty_id);
        }
        let depth = entries.len();
        let batch: Vec<queue::QueuedDelivery> = plan
            .batch
            .iter()
            .filter_map(|id| entries.iter().find(|e| e.id == *id).cloned())
            .collect();
        let front = batch.first().cloned();
        let Some(front) = front else {
            // Should be unreachable — `commit_exit` just confirmed
            // non-empty and nothing else pops. Never trust that blindly
            // on a liveness-critical path: loop again rather than assume.
            continue;
        };
        // The plan's `stranded` flag and the front entry's own payload are
        // two statements of one fact; if they ever disagree the batch would
        // be pasted as text or submitted as a marker against its own
        // content. Cheap to assert, and it keeps `plan_flush`'s contract
        // honest against a future edit to either side.
        debug_assert_eq!(
            plan.stranded,
            matches!(front.payload, queue::QueuedPayload::StrandedSubmit),
            "plan_flush's stranded flag must match the planned front entry's payload"
        );
        // #569: WHY this batch waited, which is also the header's wording.
        // Computed from the batch rather than from `reg.is_paused` — by the
        // time a pause-held entry drains the group is unpaused by definition,
        // so the live flag says nothing; the entries' own admission reasons
        // are the record of what happened to them.
        let cause = queue::flush_cause(&batch);
        if !immediate_first_pass || depth > 1 || cause == queue::FlushCause::GroupPaused {
            // Genuine contention observed: either this pass needed a retry
            // at all, or more than the one entry we started with is now
            // backed up. Sticky for the rest of this drain's lifetime.
            //
            // #569 adds the third term. A lone delivery held through a pause
            // reaches a FRESH drainer at resume, whose very first pass is
            // `immediate_first_pass` with `depth == 1` — the uncontended
            // shape — so without this it would land as a bare payload with
            // nothing saying it had been waiting since before the pause. That
            // silence is the receiver-side half of the stall #569 was filed
            // for: a report arriving with no timestamp context reads as
            // current.
            header_pending = true;
        }

        if !immediate_first_pass {
            // Visibility, never destruction (#445): a queue behind an
            // unanswered question/box is the DESIGNED case for this
            // feature, not a leak — see `queue::still_queued_notice`'s doc.
            let already_notified = reg.queue_still_notified.lock_safe().contains(&pty_id);
            // #560: the same defect the escalation clock had. `front.enqueued_ms`
            // is the FRONT entry's stamp, and a `StrandedSubmit` marker pushed
            // by `enqueue_stranded_front` makes the front the YOUNGEST entry —
            // so this notice was deferred by the very event that proves the pane
            // is stuck. `undelivered_since` takes the earlier of the pane's open
            // hold episode and the oldest entry actually queued; see its doc for
            // why BOTH terms are load-bearing.
            let oldest_entry_ms =
                entries.iter().map(|e| e.enqueued_ms).min().unwrap_or(front.enqueued_ms);
            let waiting_since =
                queue::undelivered_since(oldest_entry_ms, reg.hold_episode_since(pty_id));
            if queue::should_fire_still_queued_notice(waiting_since, now_ms(), already_notified) {
                reg.queue_still_notified.lock_safe().insert(pty_id);
                let minutes = queue::QUEUE_STILL_QUEUED_NOTICE_AFTER.as_secs() / 60;
                let target_is_orchestrator = a.role == Role::Orchestrator;
                reg.notify_queue(&group, &front.agent_id, target_is_orchestrator,
                    &queue::still_queued_notice(&front.agent_id, depth, minutes));
            }

            // #532: the same `write_admission` the paste point uses, rather
            // than a second inline spelling of the same two gates. This poll
            // is where a held delivery actually LIVES — `deliver_now` holds
            // for at most its own capped waits, but this loop re-arms them
            // with no cap — so it is also where the aggregate hold has to be
            // bounded.
            let box_pending = ptys.input_pending(pty_id).unwrap_or(false);
            // #576: THE blind spot this record exists for. This gate has no
            // `pasted_text` (the entry it is considering has written nothing
            // yet), so `mask_own_paste` cannot help — and the pane in front of
            // it is full of the PREVIOUS delivery's notices, wraps and all.
            // #820: witnessed, not merely decided. This poll is the ONLY
            // reader of the gate whose hold has no cap, so it is the one whose
            // audit has to say what it keyed on — see
            // `question_active_witnessed`.
            let reading = question_active_witnessed(
                &ptys,
                pty_id,
                None,
                // #1702: the pty->session resolution is spelled out here rather
                // than hidden inside the record read. This poll holds no
                // registry guard, which is exactly the fact the old shape made
                // impossible to check at a glance.
                reg.delivered_mask_lines(pty_id, reg.session_for_pty(pty_id).as_deref()),
            );
            let question_seen = reading.witnessed;
            let admission = write_admission(box_pending, reading.active);
            // #903: the streak the last-resort override keys on. Counted here,
            // on the poll that took the reading, and reset by ANY poll that did
            // not see an idle prompt — so it can only ever describe the pane's
            // present, never a state it was in before something repainted.
            question_idle_streak =
                if reading.idle_prompt { question_idle_streak.saturating_add(1) } else { 0 };
            // #532: the escalation decision is `held_escalation`'s, not an
            // inline `&&` chain here (rev-12 B1) — this loop is where a held
            // delivery actually LIVES, since `deliver_now` holds only for its
            // own capped waits while this re-arms them with no cap, so it is
            // also where the aggregate hold has to be bounded and reported.
            //
            // #560: and the whole step is `hold_escalation_step`'s, not eight
            // lines here, for the reason that method's doc gives: this function
            // needs a real `AppHandle`, so anything inline here is unreachable
            // by every test in this repo — which is how #560's badge churn
            // survived a review round that verified `held_escalation` itself.
            // What is left below is only the pane-header chip, which is
            // genuinely drainer-local state.
            let escalation = reg.hold_escalation_step(
                &group,
                &front.agent_id,
                pty_id,
                admission,
                depth,
                now_ms(),
                QUESTION_HOLD_STALE_AFTER.as_millis() as u64,
                // #590 L2: the pane's human-keystroke stamp, read from the same
                // `ptys` this poll already consulted for `box_pending`, so the
                // classification and the gate reading describe the same instant.
                ptys.last_user_input_ms(pty_id),
                // #820: from the SAME poll as `admission` above, for the same
                // reason `last_user_input_ms` is read there — an audit that
                // described a different instant than the decision it annotates
                // would be worse than no audit at all.
                question_seen.as_ref(),
            );
            match escalation {
                // #563: the pane is held right now — say so right now. This
                // arm is the fix: pre-#563 it did not exist, `held_escalation`
                // returned `None` for every poll inside the bound, and the
                // pane showed nothing at all until the ten-minute escalation.
                HeldEscalation::Chip(reason) => raise_chip(&front.agent_id, reason),
                HeldEscalation::Badge(_) => {
                    // #563: a hold can reach the bound having never been
                    // chipped — a drainer starting on an entry enqueued long
                    // ago evaluates its very first poll already past it. The
                    // badge must never be the ONLY thing up.
                    if let Some(reason) = admission.held_reason() {
                        raise_chip(&front.agent_id, reason);
                    }
                }
                // #563: fires on every writable poll now, and `lower_chip` is
                // guarded on our own `chip_reason`, so a pane that was never
                // chipped costs no event. #560: the BADGE no longer comes down
                // here — a writable poll is a provisional reading, not proof the
                // delivery can land, and dropping the badge on it (then
                // re-raising it on the next held poll) is symptom 1. The badge
                // now comes down where the pane proves it: `Delivered`.
                HeldEscalation::Clear => lower_chip(),
                // #560: the badge is up and the bound has elapsed — nothing to
                // change about it. The chip is still raised, because it may have
                // been lowered by a `Clear` earlier in this same episode and the
                // pane is held right now. `raise_chip` is idempotent per reason,
                // so a steady hold still emits exactly one event.
                HeldEscalation::None => {
                    if let Some(reason) = admission.held_reason() {
                        raise_chip(&front.agent_id, reason);
                    }
                }
            }
            // #903: the last resort. A question hold that has outlived
            // `QUESTION_HOLD_OVERRIDE_AFTER` on a pane whose composed screen
            // keeps showing an empty input prompt is not a question — it is the
            // detector being wrong about a pane nobody is being asked anything
            // by, and the human has already been badged about it for five
            // minutes (`QUESTION_HOLD_STALE_AFTER`). Paste — and, since this
            // grant now CARRIES to the Enter, deliver: the pre-Enter checkpoint
            // re-proves the pane on fresh reads at this same weak-idleness
            // standard (`override_enter_admits`) rather than aborting against a
            // reading the grant exists because loomux has stopped believing. It
            // was the abort that stranded the paste and wedged the queue behind
            // it; the residual that carrying leaves is named in
            // `docs/design/question-gate-authorship.md`.
            //
            // The chip is NOT lowered here: the pane is still held as far as
            // every gate is concerned, and a successful delivery lowers it below
            // on `Done` — the same place every other release does. Lowering it
            // on a provisional decision is #560's symptom 1.
            //
            // The clock is read ONCE and shared with the audit below. Two reads
            // could disagree — `note_hold` on another thread can end the episode
            // between them — and the record would then describe a hold the
            // decision was not made about, the same "different instant" defect
            // `question_seen` exists to avoid one line up.
            let override_now = now_ms();
            let held_since = reg.hold_episode_since(pty_id);
            question_overridden = question_override_admits(
                admission,
                held_since,
                override_now,
                QUESTION_HOLD_OVERRIDE_AFTER.as_millis() as u64,
                question_idle_streak,
            );
            if question_overridden {
                // Reset so a delivery that goes on to abort for some OTHER
                // reason cannot re-grant an override on every 2s poll: the pane
                // has to re-prove itself idle from scratch.
                question_idle_streak = 0;
            }
            // #903 rev-427 NB4: the GRANT may repeat — an override whose
            // delivery aborts for an unrelated reason must be able to try again,
            // or one transient abort disables the last resort for the rest of a
            // wedge, which is the failure mode this whole layer exists to end.
            // The RECORD must not: re-granting every ~6s wrote hundreds of
            // identical lines into an 8 MiB rotating log over a long hold, which
            // is how a genuinely important line becomes noise. One per hold
            // EPISODE — the same clock the bound and the badge are measured
            // against, so the log reads as one event per thing the human
            // experienced.
            if question_overridden && override_audited_since != held_since {
                override_audited_since = held_since;
                reg.audit(&group, brand::AUDIT_ACTOR, "delivery-question-override", json!({
                    "to": &front.agent_id,
                    "held_ms": held_since.map(|s| override_now.saturating_sub(s)),
                    "bound_ms": QUESTION_HOLD_OVERRIDE_AFTER.as_millis() as u64,
                    "depth": depth,
                    // What the detector was holding for, so a human reading this
                    // line can tell which signal keeps misfiring — the whole
                    // point of #820's witness, and the input to the next
                    // narrowing.
                    "matched": witness_audit(question_seen.as_ref()),
                    "reason": "the question hold outlived its bound while the pane's own \
                               screen showed an idle, empty input prompt",
                }));
            }
            if !admission.go() && !question_overridden {
                continue; // keep polling — no cap
            }
        }

        let target_is_orchestrator = a.role == Role::Orchestrator;
        let cli = reg.cli_for_agent(&a);
        let lock = reg
            .delivery
            .lock_safe()
            .entry(pty_id)
            .or_insert_with(|| Arc::new(TrackedMutex::new("delivery_pane", ())))
            .clone();
        let root = reg.root.clone();
        let reg_for_call = reg.arc();
        // #470: only iteration 1's OWN front entry ever gets kickoff
        // treatment — see `FreshFirstAttempt`'s doc for why the id match
        // matters and why every other pass is unconditionally `false, false`
        // exactly as every pre-#470 replay already was.
        // #470: also gates whether an abort below sends `queued_notice` —
        // true only for THIS entry's own very first attempt (matching the
        // pre-#470 behavior where only a FRESH direct delivery's hold-cap
        // abort ever notified; a replay's abort — including every attempt
        // after this one — stays silent, since the sender was already
        // notified once, at the moment this entry first became genuinely
        // queued, and a `BehindQueue` admission was never notified at all).
        let is_fresh_attempt = fresh_first.as_ref().is_some_and(|f| immediate_first_pass && f.id == front.id);
        let (wait_ready, confirm_autopilot, fresh_kickoff) = fresh_first
            .as_ref()
            .filter(|_| is_fresh_attempt)
            .map(|f| (f.wait_ready, f.confirm_autopilot, f.fresh_kickoff))
            .unwrap_or((false, false, false));

        let outcome = match &front.payload {
            queue::QueuedPayload::Text(text) => {
                // #533-A: when this pass's plan holds more than one entry,
                // the ENTIRE flushable backlog goes out as this one paste —
                // header, then every constituent behind its own itemization
                // banner, in queue order. Pre-#533 this sent `front` alone
                // and came back for the next entry on the following pass,
                // which cost the receiving agent one full turn per queued
                // delivery.
                // #903 B1': what the prompt record may take from this paste —
                // the entries' own texts, never the framed string built below.
                let record_contributions = record_contributions_for(&batch);
                let payload = if batch.len() > 1 {
                    let items: Vec<queue::FlushConstituent> = batch
                        .iter()
                        .filter_map(|e| {
                            e.payload.text().map(|t| queue::FlushConstituent {
                                id: e.id,
                                from: &e.from,
                                enqueued_ms: e.enqueued_ms,
                                coalesced: e.coalesced,
                                text: t,
                            })
                        })
                        .collect();
                    let rendered =
                        queue::coalesced_flush_text(&items, plan.remaining, now_ms(), cause);
                    // #632's door for this producer. `pause_suppression_notice`
                    // can assert "masks away entirely"; a flush cannot, because
                    // the constituent payloads are agent text by design and
                    // must stay unmasked. So the invariant asserted here is the
                    // exact split: everything loomux FRAMED is maskable, and
                    // the only survivors are payload rows. Written through the
                    // real `mask_loomux_notices` (via
                    // `unmaskable_framing_rows`) so it cannot drift from the
                    // rule it stands in for, and `debug_assert` for the same
                    // reason `OrchNoticeInbox::park`'s is — CI's test builds
                    // are debug, a live release session must never panic over
                    // a gate that merely holds too long.
                    debug_assert!(
                        {
                            let payloads: Vec<&str> = items.iter().map(|c| c.text).collect();
                            unmaskable_framing_rows(&rendered, &payloads).is_empty()
                        },
                        "a coalesced flush must leave only constituent payload rows unmasked \
                         (#632) — got {:?}",
                        unmaskable_framing_rows(
                            &rendered,
                            &items.iter().map(|c| c.text).collect::<Vec<_>>()
                        )
                    );
                    rendered
                } else if header_pending {
                    // #445: the flush header ("N deliveries queued ... are
                    // now delivering") rides on the front of the FIRST
                    // replayed text this drain sends, rather than as its own
                    // separate delivery — one paste, header then content,
                    // instead of an extra full echo/confirm cycle for a
                    // single notice line.
                    let coalesced = {
                        let queues = reg.queues.read();
                        queues.get(&pty_id).map(|q| q.iter().map(|e| e.coalesced as usize).sum()).unwrap_or(0)
                    };
                    format!("{}\n\n{text}", queue::flush_header_text(depth.max(1), coalesced, cause))
                } else {
                    text.clone()
                };
                // #517: the re-delivery payload is `text`, never `payload` —
                // a flush header, or the itemization banners of a coalesced
                // batch, are about THIS drain, not part of the brief.
                //
                // #533 rev-13 F2: `payload` and `text` now differ by TWO
                // independent routes, not one. The old note argued they were
                // "equal in practice" because `header_pending` is false on a
                // fresh kickoff's own first pass; the `batch.len() > 1`
                // branch above sits ahead of `header_pending` and is a
                // second route that premise does not cover. Passing `text`
                // is still right either way — that is what makes this
                // correct rather than lucky.
                //
                // The consequence #533 raises, stated rather than left
                // implicit: if `batch.len() > 1` ever coincided with a fresh
                // kickoff on iteration 1, #517's recovery would re-admit
                // only the kickoff brief while constituents 2..N had already
                // been popped as `Done` without landing — N-1 lost, where
                // pre-#533 it was at most 1. `ensure_drainer`'s doc argues
                // that coincidence is purely theoretical (a fresh spawn's
                // pty has no other deliveries to race against yet), and that
                // argument is unchanged by this PR; what changed is the
                // price if it were ever wrong, which is why it is written
                // down here instead of being left to the reader.
                let out = deliver_now(
                    app.clone(), root, group.clone(), front.agent_id.clone(), pty_id,
                    payload, front.from.clone(), confirm_autopilot, wait_ready, cli, lock,
                    reg.last_delivery.clone(), target_is_orchestrator, reg_for_call,
                    fresh_kickoff.then(|| text.clone()),
                    question_overridden,
                    record_contributions,
                );
                if matches!(out, DeliverOutcome::Done) {
                    header_pending = false;
                }
                out
            }
            queue::QueuedPayload::StrandedSubmit => {
                let _guard = lock.lock_safe();
                let submit = submit_sequence(&cli);
                match drain_stranded_submit(
                    &ptys, &reg.last_delivery, front.from.clone(), pty_id, submit,
                    // #1702: unchanged in what it acquires — `delivered_mask_
                    // lines` already reached `by_pty` + `agents` from here — but
                    // now visibly so, under the per-pane delivery `lock` this arm
                    // holds. That nesting predates #1702 and is NOT this PR's to
                    // change; it is filed, with the reverse-order question, as
                    // its own row.
                    reg.delivered_mask_lines(pty_id, reg.session_for_pty(pty_id).as_deref()),
                ) {
                    StrandedMarkerAction::Press => DeliverOutcome::Done,
                    StrandedMarkerAction::Retire(why) => DeliverOutcome::Retired(why),
                    // #532 rev-12 NB1, arriving here: the gate that ACTUALLY
                    // declined, never a hardcoded `Question`. This arm used to
                    // report every decline as an interactive question, sending
                    // the orchestrator to look for a dialog that a box-occupied
                    // hold never painted.
                    StrandedMarkerAction::Retry(reason) => DeliverOutcome::AbortedPrePaste(reason),
                }
            }
        };

        // #533-A: every constituent of this pass's batch closes out on the
        // ONE submit that carried them — see `pop_batch_dequeued`. A
        // single-entry batch takes the identical pre-#533 path.
        let closed: Vec<(u64, u64)> = batch.iter().map(|e| (e.id, e.enqueued_ms)).collect();

        // #560: the hold episode's lifecycle, at the ONE place that knows
        // whether this pane accepted a delivery. `Done` is the only evidence
        // that ends an episode (`ends_hold_episode`); either abort OPENS one if
        // the poll above did not, which is what covers a hold that lives
        // entirely inside `deliver_now` — every poll reads writable, every
        // attempt then aborts, and pre-#560 nothing ever started a clock.
        // #813 adds a second ender that is not a delivery — see `Retired`.
        let observation = match outcome {
            DeliverOutcome::Done => HoldObservation::Delivered,
            DeliverOutcome::Retired(_) => HoldObservation::Retired,
            DeliverOutcome::AbortedPrePaste(_) | DeliverOutcome::AbortedPreEnter(_) => {
                HoldObservation::Aborted
            }
        };
        reg.note_hold(&group, &front.agent_id, pty_id, observation, now_ms());

        match outcome {
            DeliverOutcome::Done => {
                reg.pop_batch_dequeued(&group, pty_id, &closed);
                reg.queue_still_notified.lock_safe().remove(&pty_id);
                // #532/#560: the hold episode ended at the `note_hold` call
                // above — a delivered entry means whatever was holding this pane
                // is over, so a LATER hold clocks and badges afresh.
                lower_chip();
            }
            // #813: the marker leaves the queue without having pressed
            // anything, so the entries behind it get their turn — that release
            // IS the fix. Everything else here mirrors `Done` because the queue
            // consequence is the same (this entry is finished with), and only
            // the audit and the badge treat it differently.
            DeliverOutcome::Retired(why) => {
                reg.audit(&group, brand::AUDIT_ACTOR, "stranded-marker-retired", json!({
                    "to": &front.agent_id,
                    "reason": why.as_str(),
                    // The whole point, in one number: how much work this
                    // marker was holding behind it when it was retired.
                    "depth_behind": depth.saturating_sub(1),
                }));
                // The chip the human has been staring at comes down on the
                // evidence that they resolved it themselves — `HumanResolved`
                // and nothing else (`resolves_the_pane`). `TextGone` says our
                // text left the box with nobody at the keyboard, which is
                // `StrandedBlocker::NotHolding`'s own situation and NOT a
                // clear; `NothingStranded` establishes nothing about the pane.
                // #496 PR-C's badge is the channel that must survive a repair
                // loomux could not make. `clear_stranded` is a no-op when no
                // note is up, so this never invents a clear.
                if why.resolves_the_pane() {
                    reg.clear_stranded(&group, &front.agent_id, why.as_str());
                }
                reg.pop_batch_dequeued(&group, pty_id, &closed);
                reg.queue_still_notified.lock_safe().remove(&pty_id);
                lower_chip();
            }
            DeliverOutcome::AbortedPrePaste(reason) => {
                // Nothing pasted (or the stranded flush declined) — leave
                // the entry at the front exactly as it was; retry next tick.
                // #470: the ONE point at which this entry transitions from
                // "attempted immediately" to "now genuinely queued" — the
                // moment #445's notice vocabulary exists to announce, and
                // (unlike every later retry of the SAME entry, which stays
                // silent) the only point it's still correct to announce,
                // since a `BehindQueue` admission was never announced at
                // admission time either.
                if is_fresh_attempt {
                    reg.notify_queue(&group, &front.agent_id, target_is_orchestrator,
                        &queue::queued_notice(&front.agent_id, reason));
                }
            }
            DeliverOutcome::AbortedPreEnter(reason) => {
                // The Text entry WAS pasted; only the Enter was withheld.
                // Replace it at the front with a StrandedSubmit marker so
                // draining resumes there next tick instead of re-pasting.
                //
                // rev-12 NB1: `reason` comes from the gate that actually
                // declined, never a hardcoded `Question`. #532 gave this path
                // a second cause, and the hardcode was telling the
                // orchestrator to go find a dialog that did not exist.
                if is_fresh_attempt {
                    reg.notify_queue(&group, &front.agent_id, target_is_orchestrator,
                        &queue::queued_notice(&front.agent_id, reason));
                }
                // #533-A: the paste that landed in the box was the WHOLE
                // batch, so the whole batch closes out here and ONE marker
                // stands for all of it. Popping only the front (the
                // pre-#533 shape) would leave the other constituents queued
                // and re-paste text already sitting in the box the moment
                // the marker's Enter submits it.
                reg.pop_batch_dequeued(&group, pty_id, &closed);
                // #445 rev-35 NB3: this rejection is rare (it needs the
                // front door to fill the freed slot in the narrow window
                // between the pop above and this push) but was previously
                // silent — the `let _ =` skipped the loud `dropped_notice`
                // every OTHER drop path sends, leaving pasted-but-
                // unsubmitted text with no signal at all (mitigated only by
                // the next delivery's own stranded-text flush eventually
                // submitting it — never guaranteed to be loud about it).
                // #560: `reason` is this arm's own binding — the gate that
                // actually declined the Enter, `BoxOccupied` as readily as
                // `Question` since #532 — and it is what the marker is
                // recorded under. It is the same value the notice above was
                // already sent with; the audit line simply stopped disagreeing
                // with the notice about one event.
                if let Err(_queue_full) =
                    reg.enqueue_stranded_front(&group, &front.agent_id, &front.from, pty_id, reason)
                {
                    // #533-A: the marker that failed to push stood for the
                    // WHOLE batch that was pasted, so the count names every
                    // delivery left pasted-but-unsubmitted — saying "1" here
                    // would understate what a reader has to go look at.
                    reg.notify_queue(&group, &front.agent_id, target_is_orchestrator,
                        &queue::dropped_notice(&front.agent_id, closed.len(), queue::DropReason::QueueFull));
                }
            }
        }
    }
}

/// Whether to flush a previous delivery's stranded text (a single submit press)
/// before pasting the next prompt (#81/#84).
///
/// Flush only on the exact stranded-text signature: the previous delivery to
/// this pane was NOT confirmed as submitted, AND no human has typed into the
/// pane since (so the box holds the earlier *agent* prompt, not a person's
/// half-written line — which must never be blind-submitted). Never flushes on
/// the first delivery to a pane (`prev_confirmed == None`) or after a confirmed
/// one. A false "unconfirmed" here is safe: the flush Enter lands on an already
/// empty box and is a no-op.
pub fn should_flush_before_paste(prev_confirmed: Option<bool>, human_typed_since: bool) -> bool {
    matches!(prev_confirmed, Some(false)) && !human_typed_since
}

/// Whether the stranded-text flush should ACTUALLY press its Enter right now
/// (#420 rev-15 B1) — `should_flush_before_paste`'s decision, additionally
/// gated on there being no live interactive question on screen. The flush's
/// Enter is the FIRST write `deliver_prompt` makes; without this gate it fires
/// unconditionally, before the interactive-question checkpoint that follows
/// it ever runs — so a question already on screen from BEFORE this delivery
/// even started would eat that Enter and select whatever's highlighted, on
/// the exact path this guard exists to close. A named function (not an inline
/// `&&` at the call site) so the combination is independently testable and
/// can't be silently dropped by a future edit to either input.
///
/// **`box_pending` (#532) — the #510 rule, read STRUCTURALLY at the press.**
/// `human_typed_since` is a TIMESTAMP compare (`last_user_input_ms >
/// submit_sent_ms`), and a timestamp answers "did a keystroke land after our
/// own submit", which is a strictly narrower question than "is there human
/// content in this box right now". The gap is reachable and was live in #532:
/// a human who typed a line and left it sitting BEFORE our submit stamps
/// `last_user_input_ms` at or before `submit_sent_ms`, so `human_typed_since`
/// reads FALSE and this gate used to fire an Enter that submitted their line.
/// `input_pending` (#111/#171 — the per-pane occupancy counter, moved by writes
/// arriving through `write_pty`/`note_user_input`, never by loomux's own
/// `write_bytes`) is a **much closer** reading of the box than a keystroke
/// timestamp, so it is consulted here rather than inferred.
///
/// It is not, however, the box itself, and rev-12 was right to flag an earlier
/// version of this doc for saying so. It is a running count that
/// `classify_human_input` zeroes on only `\r`/`\n`, Ctrl-U and Ctrl-C, so it
/// has a reachable **stuck-true** mode — see [`hold_bound_elapsed`], which is
/// deliberately not allowed to consult it for exactly that reason. Stuck-true
/// is the safe direction *here* (a withheld Enter, recoverable) and the unsafe
/// direction *there* (a suppressed escalation, not), which is why the same
/// signal is trusted at this gate and refused at that one.
///
/// It does NOT suppress the legitimate case this flush exists for: our own
/// stranded paste never moves that counter.
pub fn should_flush_before_paste_now(
    prev_confirmed: Option<bool>,
    human_typed_since: bool,
    question_active: bool,
    box_pending: bool,
) -> bool {
    should_flush_before_paste(prev_confirmed, human_typed_since)
        && !question_active
        && !box_pending
}

/// What `deliver_now` does about a stranded-text flush that **declined** (#824).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrandedPasteGuard {
    /// Nothing of ours is provably in the box — paste, exactly as before.
    Paste,
    /// Our own prompt is still sitting in that box and the flush could not
    /// clear it. Do not paste on top of it.
    AbortStranded,
}

/// #824: the missing half of `flush_stranded_text`'s contract.
///
/// The flush is the mechanism that clears a previous delivery's stranded text
/// before this one pastes. It can DECLINE — `should_flush_before_paste_now`
/// returns false whenever a human typed since our submit, a question is live,
/// or human characters are outstanding — and `deliver_now` used to ignore that,
/// carrying straight on to the paste.
///
/// **Why no existing guard catches it.** The pre-paste guard between the flush
/// and the paste is `wait_for_box_clear`, which reads
/// `PtyManager::input_pending`, i.e. `input_box_len`. That counter is written by
/// `note_user_input` and by nothing else, and `note_user_input` is reached from
/// exactly one place — `write_from_frontend`. Orchestration's own typing goes
/// out through `write_bytes`, which touches no counter at all. So
/// `!input_pending` means "no HUMAN characters are outstanding", **never** "the
/// box is empty", and our own pasted prompt is invisible to every guard on this
/// path. The sequence that follows is ordinary rather than exotic: a delivery
/// strands, a human presses Enter (or types a character and backspaces it out),
/// the flush declines on the human block, `input_pending` reads false, and the
/// next delivery pastes ON TOP of the stranded prompt — which the pre-Enter
/// quiet wait then submits as one merged prompt. That is the #81/#84/#111
/// collision the flush exists to prevent, reached through the flush's own
/// decline.
///
/// **Gated on a POSITIVE reading, never on the decline.** `Holds` is the only
/// answer that aborts. `Unverifiable` keeps today's behaviour deliberately: it
/// is common near the Tier 1 scan cap (#583/#685's census), and aborting on it
/// would hold ordinary traffic for a reading that says nothing — trading a rare
/// merge for a routine stall. `NotHolding` means our text is gone, which is
/// exactly when pasting is safe. So this can only ever abort a delivery loomux
/// can SEE the collision coming for, which bounds the blast radius to the
/// evidence.
///
/// **#828 is what makes that reading load-bearing.** Before it,
/// `box_holds_paste` could answer a confident `NotHolding` for text that was on
/// screen behind per-row gutter decoration — so this guard would have been
/// silently absent precisely under the copilot rendering it is for. Post-#828
/// the reading is a structural superset whose safe answer is `Unverifiable`,
/// which is the branch that keeps today's behaviour here.
///
/// Pure and total, so every cell is directly assertable — `deliver_now` needs a
/// concrete `Wry` `AppHandle` and is unreachable by every test in this repo,
/// which is the same reason `preenter_admission` and `flush_stranded_text` were
/// extracted rather than left inline.
#[doc(hidden)] // pub for integration tests
pub fn stranded_paste_guard(
    prev_confirmed: Option<bool>,
    // Whether `flush_stranded_text` actually pressed. `true` means the box was
    // cleared by our own Enter, so there is nothing left to collide with.
    flushed: bool,
    // Tier 1 on the PREVIOUS delivery's own recorded text, or `None` when the
    // ledger carries none to look for.
    text_reading: Option<BoxReading>,
) -> StrandedPasteGuard {
    if flushed {
        return StrandedPasteGuard::Paste;
    }
    if !matches!(prev_confirmed, Some(false)) {
        return StrandedPasteGuard::Paste;
    }
    if text_reading == Some(BoxReading::Holds) {
        return StrandedPasteGuard::AbortStranded;
    }
    StrandedPasteGuard::Paste
}

/// The stranded-text flush STEP (#81/#84, #420 rev-15 B1) — decides via
/// `should_flush_before_paste_now` (reading the pane's live question state
/// through `question_active_now`) and, if it says to, presses `submit`.
/// Extracted out of `deliver_prompt`'s body specifically so an integration
/// test can drive this EXACT logic — the one `deliver_prompt` actually calls,
/// not a reimplementation of it — against a real (fake-child-backed, see
/// `PtyManager::register_fake_for_test`) `PtyManager`, without needing a real
/// Tauri `AppHandle` (unavailable headless — `tauri::test`'s `MockRuntime`
/// isn't the concrete `Wry` runtime the rest of `deliver_prompt`'s setup
/// requires) or a real agent CLI (CLAUDE.md constraint 3). rev-19 R3: a test
/// that only calls `should_flush_before_paste_now` directly proves the
/// *decision* is right but not that `deliver_prompt` acts on it — this
/// closes that gap, since `deliver_prompt` has nothing left to get wrong
/// here beyond calling this one function. Returns whether it actually wrote.
#[doc(hidden)] // pub for integration tests
pub fn flush_stranded_text(
    ptys: &crate::pty::PtyManager,
    pty_id: u32,
    prev_confirmed: Option<bool>,
    human_typed_since: bool,
    submit: &[u8],
    // #576: this reader gets the delivery record for the same reason the
    // drainer gate does — it is a blind Enter decided off `question_active_now`
    // with no `pasted_text`, and a wrapped notice of ours left it declining
    // forever. Fixing one reader of the gate and leaving another was rev-126's
    // finding (`e7`), so both move together.
    delivered: Vec<String>,
) -> bool {
    let question_active = question_active_now(ptys, pty_id, None, delivered);
    // #532: BOTH live readings are taken here, at the press, and neither is
    // inherited from a check some earlier stage passed. `unwrap_or(true)` is
    // the fail-safe direction for an occupancy reading we cannot take (a
    // closed pty): decline the Enter rather than press blind. It matches
    // `human_input_block_now`'s treatment of the same unreadable case.
    let box_pending = ptys.input_pending(pty_id).unwrap_or(true);
    should_flush_before_paste_now(prev_confirmed, human_typed_since, question_active, box_pending)
        && ptys.write_bytes(pty_id, submit).is_ok()
}

/// What a spaced submit retry should do this iteration (#420 rev-15 B2):
/// `deliver_prompt`'s retry loop used to press Enter unconditionally once the
/// human-typing check passed — exactly the window (a few seconds after the
/// first submit) Copilot most often paints a permission/question dialog. A
/// question appearing here means HOLD, not retry, so a fresh
/// `PasteDecision` (already gated on `prompt_wait_detected` and capped, same
/// as every other checkpoint) is threaded through as its own case rather than
/// being collapsed into a bool — the caller's audit text differs by WHY a
/// retry didn't fire, same as every other checkpoint in this function.
///
/// rev-19 N9: the human-typing check is NOT a variant here. It used to be
/// (`SkipHumanTyping`), but the caller already checks it and `break`s BEFORE
/// ever calling this function — meaning that arm could never actually be
/// reached, and its match arm carried an `unreachable!()` in code that runs
/// on a detached thread: a latent panic waiting for some future refactor to
/// reorder the caller and make it reachable for real. Dropping the variant
/// (and the parameter) removes the dead arm entirely instead of trusting
/// nobody ever reaches it — this function's only job now is "given the
/// question hold's outcome, write or don't", which is also all it can be
/// asked, since human-typing precedence is enforced by the caller's own
/// control flow, not by anything this function could get wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryGate {
    /// No question is in the way — press Enter. Carries the hold duration
    /// (0 if it was never active) straight through so the caller never needs
    /// to re-destructure the original `PasteDecision` to audit it (rev-19
    /// N9: no `unreachable!()` fallback needed for a value this enum already
    /// carries).
    Write { held_ms: u64 },
    /// A live question was still on screen when the hold for it capped out.
    SkipQuestionPending { held_ms: u64 },
}

/// Decide `RetryGate` for one spaced retry from the question hold's outcome.
pub fn retry_gate(question_decision: PasteDecision) -> RetryGate {
    match question_decision {
        PasteDecision::Paste { held_ms } => RetryGate::Write { held_ms },
        PasteDecision::Abort { held_ms } => RetryGate::SkipQuestionPending { held_ms },
    }
}

/// How a single human write into a pane's input changes box occupancy (#111).
/// Classified from the keystroke's *content*, which is what tells a line still
/// sitting in the box from one already submitted — an output-byte heuristic
/// can't (one keystroke's input-line redraw, or ambient agent streaming, can
/// exceed any fixed burst floor, and a sub-floor submit never clears it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HumanInput {
    /// Printable text was entered — a line now sits unsubmitted in the box.
    Content,
    /// The line was submitted (Enter) or explicitly cleared — the box is empty.
    Submit,
    /// Navigation/editing that neither adds visible text nor submits (arrows,
    /// backspace, bare escape sequences) — box occupancy is unchanged **by
    /// this coarse read**. Backspace/DEL does in fact remove a character;
    /// `box_occupancy_delta` (#171) is the finer-grained sibling that tracks
    /// that, for callers that need to notice a typed line getting backspaced
    /// all the way back out rather than submitted.
    Neutral,
}

/// Classify one human write for the delivery paste guard (#111). Pure so the
/// rule is testable; `write_pty` calls it to maintain the per-pane
/// `input_pending` flag.
///
/// - A write carrying **bracketed-paste markers** (`ESC[200~` / `ESC[201~`) is
///   pasted text held UNSUBMITTED in the box → `Content`, even if it ends in a
///   newline: under bracketed-paste mode (Claude Code and most modern TUIs) a
///   pasted newline is literal, not a submit — the human's separate Enter
///   afterwards is the submit. Checked first so an interior/trailing newline
///   can't misread the paste as submitted (the #111 loss otherwise).
/// - Otherwise a carriage return / newline submits the current line, UNLESS
///   printable text follows the last newline (then that trailing text is a fresh
///   unsubmitted line → `Content`).
/// - Ctrl-U (kill-line) / Ctrl-C (interrupt) empty the box → `Submit`.
/// - Any remaining printable/graphic character (after skipping escape sequences)
///   → `Content`.
/// - Otherwise (arrows, backspace, lone escape sequences) → `Neutral`.
///
/// Erring toward `Content`/`Neutral` on ambiguous input keeps the guard biased
/// to the safe hold: a real sitting line is never misread as empty. This
/// three-way read alone can't recognize a line that was typed and then fully
/// backspaced back out (every backspace reads as `Neutral`, individually
/// indistinguishable from an arrow key) — that was issue #171: the box read
/// as occupied forever, holding every subsequent delivery until the 60s abort.
/// `write_pty` closes that gap by pairing this call with `box_occupancy_delta`,
/// which *does* count backspace/DEL as a removal. Other residual clears that
/// leave the flag stuck — Esc-to-clear, Ctrl-W/Ctrl-K, and soft-newline
/// editors — still need true box-occupancy detection and remain open.
pub fn classify_human_input(data: &str) -> HumanInput {
    // Bracketed paste: the text lands in the box unsubmitted regardless of any
    // newline it contains, so never read it as a submit.
    if data.contains(BRACKETED_PASTE_START) || data.contains(BRACKETED_PASTE_END) {
        return HumanInput::Content;
    }
    if let Some(pos) = data.rfind(['\r', '\n']) {
        // `\r`/`\n` are single-byte, so `pos + 1` is a valid char boundary.
        let after = &data[pos + 1..];
        return if input_has_printable(after) { HumanInput::Content } else { HumanInput::Submit };
    }
    // Line-clear controls empty the box even without a newline.
    const KILL_LINE: char = '\u{15}'; // Ctrl-U
    const INTERRUPT: char = '\u{03}'; // Ctrl-C
    if !data.is_empty() && data.chars().all(|c| c == KILL_LINE || c == INTERRUPT) {
        return HumanInput::Submit;
    }
    if input_has_printable(data) {
        HumanInput::Content
    } else {
        HumanInput::Neutral
    }
}

/// xterm bracketed-paste bracket sequences: the terminal wraps pasted text in
/// these so an app can tell a paste from typing (and hold pasted newlines soft).
const BRACKETED_PASTE_START: &str = "\u{1b}[200~";
const BRACKETED_PASTE_END: &str = "\u{1b}[201~";

/// Shared walk over a human write, skipping terminal escape sequences, that
/// backs both `input_has_printable` and `box_occupancy_delta`. Skips CSI
/// (`ESC [ … final`, e.g. arrow keys, bracketed-paste markers) AND the string
/// sequences a terminal emits in *reply* to a program's query — OSC (`ESC ]`)
/// and DCS/SOS/PM/APC (`ESC P`/`X`/`^`/`_`) — plus other short `ESC`-led
/// sequences, so none of their printable bytes read as typed content.
///
/// The OSC/DCS skip is #179: GitHub Copilot queries the terminal's colors
/// (`ESC]10;?`, `ESC]11;?`, `ESC]4;n;?`) and version (`ESC[>q`) at boot; the
/// webview's xterm auto-answers, and those answers reach us through `write_pty`
/// exactly like a keystroke. Their bodies are printable (`11;rgb:0d0d/1111/1717`),
/// so without skipping the whole string they were misread as a human's line,
/// wedging `input_pending` true and stalling the fresh-copilot kickoff paste in
/// the #111 box-clear hold (up to its 60s abort) — the "prompt never delivered"
/// symptom. Claude Code issues no such query, so only copilot tripped it.
///
/// #496: the same auto-replies reach `write_pty` with NO human present at
/// all, and until PR-A they *also* unconditionally re-stamped
/// `user_input_ms` — the keystroke-recency clock everything from the
/// autonomous idle tick to the stranded-text flush reads. This scan is now
/// consulted for THAT gate too (`PtyManager::note_user_input`): a write
/// classifies `Neutral` with a zero `occupancy_delta` — this skip is exactly
/// why — and `note_user_input` treats that combination as "not a keystroke",
/// so the timestamp isn't touched either. One classifier, two consumers; see
/// `note_user_input` for the gate and the tradeoff it accepts.
///
/// Returns `(has_printable, occupancy_delta)`: the first is "did this write
/// put visible text in the box at all" (`input_has_printable`'s job); the
/// second is the signed change to box occupancy — one Unicode CHARACTER
/// added counts `+1` regardless of its UTF-8 byte width, backspace/DEL
/// (`\x08`/`\x7f`) removes `1` (#171) — that `box_occupancy_delta` exposes so
/// a run of backspaces emptying a typed line is recognized even though no
/// single write in the run looks like a submit.
fn scan_box_units(s: &str) -> (bool, i32) {
    let b = s.as_bytes();
    let mut i = 0;
    let mut printable = false;
    let mut delta: i32 = 0;
    while i < b.len() {
        if b[i] == 0x1b {
            i += 1;
            match b.get(i) {
                // CSI: `ESC [` … final byte in 0x40..=0x7e.
                Some(b'[') => {
                    i += 1;
                    while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                        i += 1;
                    }
                    i += 1; // consume the CSI final byte
                }
                // OSC / DCS / SOS / PM / APC: a string sequence whose body is
                // arbitrary (often printable) text, terminated by BEL (0x07) or
                // ST (`ESC \`). Skip the whole thing — it's a query reply, not
                // typed input (#179).
                Some(b']') | Some(b'P') | Some(b'X') | Some(b'^') | Some(b'_') => {
                    i += 1;
                    while i < b.len() {
                        if b[i] == 0x07 {
                            i += 1; // BEL terminator
                            break;
                        }
                        if b[i] == 0x1b && b.get(i + 1) == Some(&b'\\') {
                            i += 2; // ST terminator (ESC \)
                            break;
                        }
                        i += 1;
                    }
                }
                // Any other 2-byte / lone ESC sequence (charset select, `ESC=`, …).
                _ => {
                    i += 1;
                }
            }
            continue;
        }
        // Backspace / DEL: one character removed from the box (#171).
        if b[i] == 0x08 || b[i] == 0x7f {
            delta -= 1;
            i += 1;
            continue;
        }
        // Printable ASCII, or a UTF-8 multibyte LEAD byte (0xC0..=0xFF): count
        // one occupancy unit per character, not per byte. A 3-byte CJK
        // character or a 4-byte emoji is still exactly one keystroke and one
        // backspace, so counting raw bytes here (+3/+4 on type, -1 on the one
        // backspace that removes it) over-counted occupancy for any non-ASCII
        // typer and reproduced #171's exact stuck-occupied symptom for them —
        // the counter never gets back to `<= 0` because the single removal
        // can't cancel a multi-byte addition. Continuation bytes (0x80..=0xBF)
        // are still consumed (and still mark the write as printable) but add
        // no further delta — they're part of the character its lead byte
        // already counted.
        if (0x20..0x7f).contains(&b[i]) || b[i] >= 0xc0 {
            printable = true;
            delta += 1;
            i += 1;
            continue;
        }
        if b[i] >= 0x80 {
            printable = true;
            i += 1; // continuation byte — already counted via its lead byte
            continue;
        }
        i += 1; // other C0 control (tab, etc.)
    }
    (printable, delta)
}

/// Whether `s` contains a graphic character once terminal escape sequences are
/// skipped — the test for "this write put visible text in the box". See
/// `scan_box_units` for what's skipped and why.
fn input_has_printable(s: &str) -> bool {
    scan_box_units(s).0
}

/// The signed change to box occupancy from a single human write: `+1` per
/// Unicode CHARACTER (a UTF-8 lead byte, not a raw byte — a 3-byte CJK
/// character or a 4-byte emoji is one keystroke, so it counts as one, not
/// three or four), `-1` per backspace/DEL, `0` for anything else (arrows,
/// bare escape sequences, an OSC/DCS query-reply echo — #179). Used alongside
/// `classify_human_input` to track real occupancy rather than a bare
/// pending/not flag: `write_pty` applies this to a running per-pane counter
/// (clamped at zero) so a typed line that gets fully backspaced out reads
/// back to empty even though no single write in the run looks like a submit
/// (#171) — `classify_human_input`'s `Submit` still resets the counter
/// directly to zero, which is exact where it applies (Enter, Ctrl-U,
/// Ctrl-C).
///
/// Counting by character rather than by byte matters for correctness, not
/// just cosmetics: byte-counting added 3/4 for one CJK/emoji character typed
/// but only subtracted 1 for the single backspace that removes it, so the
/// counter never returned to zero and a non-ASCII typer hit this exact
/// stuck-occupied symptom (a prior revision of this fix had that bug).
///
/// The counter is deliberately biased to never UNDER-count real occupancy
/// (never read `0`/empty while a human's line is in fact still sitting in the
/// box — that's the clobber hazard the whole #111 guard exists to prevent).
/// It may OVER-count, and that direction is safe: if some CLI's line editor
/// ever deletes more than one character for a single backspace/DEL byte (a
/// multi-codepoint grapheme cluster, say), this still only subtracts `1`,
/// leaving the counter higher than the box's real contents — worst case, a
/// delivery holds a little longer than strictly necessary before pasting,
/// never earlier. Only backspace/DEL is counted as removal at all: Ctrl-W
/// (delete word), Ctrl-K (kill to end) and Esc-to-clear editors still net `0`
/// here — full box-occupancy tracking for those remains open (see
/// `classify_human_input`).
pub fn box_occupancy_delta(s: &str) -> i32 {
    scan_box_units(s).1
}

/// One tick of the pre-paste human-input hold (#111): given whether a human's
/// line is still sitting in the box, decide whether to paste, keep holding, or
/// abort. Pure so the hold/abort rule is testable without a live PTY;
/// `hold_for_human_input` drives it. Mirrors `should_flush_before_paste` — a
/// small, total gate.
///
/// - `box_pending`: does the box still hold a human's unsubmitted line?
/// - `held` / `max_hold`: the bounded wait; at the cap we abort rather than
///   paste onto a line the human never cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteGate {
    /// Box is clear (or was never dirty) — paste the prompt.
    Paste,
    /// Human's line is still in the box — keep waiting.
    Hold,
    /// The box never cleared within the bound — do not paste; notify instead.
    Abort,
}

pub fn resolve_paste_gate(box_pending: bool, held: Duration, max_hold: Duration) -> PasteGate {
    if !box_pending {
        return PasteGate::Paste; // box is empty — paste the prompt
    }
    if held >= max_hold {
        return PasteGate::Abort; // bounded wait elapsed and the line never cleared
    }
    PasteGate::Hold
}

/// Outcome of the pre-paste human-input hold (#111): either the box is clear and
/// delivery may paste, or it never cleared and the delivery must abort. Carries
/// the held duration so the caller can audit how long it waited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteDecision {
    Paste { held_ms: u64 },
    Abort { held_ms: u64 },
}

/// Poll-and-hold loop that drives `resolve_paste_gate`: if a human's line is
/// sitting in the box (`box_pending`), block until they submit/clear it (the
/// flag flips false) or the bounded wait elapses, then return `Paste`/`Abort`.
/// Returns `Paste { held_ms: 0 }` immediately when the box is already clear.
///
/// Generic over the occupancy source and timings so the wiring — that the loop
/// re-reads the flag each poll and honours the abort cap — is integration-
/// testable without a live PTY (the #40 lesson: exercise the loop, not just the
/// pure decision).
#[doc(hidden)] // pub for integration tests
pub fn hold_for_human_input<P: Fn() -> bool>(
    box_pending: P,
    max_hold: Duration,
    poll: Duration,
) -> PasteDecision {
    if !box_pending() {
        return PasteDecision::Paste { held_ms: 0 };
    }
    let start = std::time::Instant::now();
    loop {
        let held = start.elapsed();
        match resolve_paste_gate(box_pending(), held, max_hold) {
            PasteGate::Paste => return PasteDecision::Paste { held_ms: held.as_millis() as u64 },
            PasteGate::Abort => return PasteDecision::Abort { held_ms: held.as_millis() as u64 },
            PasteGate::Hold => std::thread::sleep(poll),
        }
    }
}

/// Production wrapper: hold prompt delivery to `pty_id` while a human's line is
/// sitting in its input box, using the shipped cap / poll. A closed pty reads as
/// "not pending" so a dead pane never blocks the thread.
fn wait_for_box_clear(ptys: &crate::pty::PtyManager, pty_id: u32) -> PasteDecision {
    hold_for_human_input(
        || ptys.input_pending(pty_id).unwrap_or(false),
        HUMAN_INPUT_HOLD_MAX,
        HUMAN_INPUT_POLL,
    )
}

/// Remove every line of `tail` that exactly matches one of `pasted_text`'s
/// own lines (rev-19 B-A). This is the self-echo exclusion for the
/// interactive-question guard — CONTENT-based, not byte-COUNT-based. A round
/// that gated on "has the pane's output total grown since a baseline
/// snapshot" was proven broken twice: the baseline is always ONE number, so
/// it can only mark a single point in time as "before" — a dialog that
/// renders WHILE the paste is still echoing (the canonical #420 timeline:
/// loomux pastes, Copilot processes it, paints a dialog, all before loomux's
/// own settle-wait finishes) gets baked into whichever snapshot the
/// checkpoint happened to take, and reads as "no growth" — invisible —
/// forever after. No amount of moving WHEN the snapshot is taken closes that
/// gap, because delta-vs-one-number can't distinguish "still my own paste
/// settling" from "their dialog, appeared in the same window" — both are
/// just "more bytes since X." Content can: `deliver_prompt` knows EXACTLY
/// what it pasted. Masking out only the lines that are OUR OWN text leaves
/// whatever the CLI itself painted — including a
/// dialog that rendered mid-paste — fully visible to the detector; a paste
/// whose own text happens to contain "(y/n)" or "do you want to run" masks
/// itself away to nothing, because every line of it IS a pasted line.
///
/// **#820: a CLI does not render our paste as our lines, and comparing whole
/// lines for equality is what made this mask miss.** The original rule was
/// `row.trim().to_lowercase() == pasted_line`, and every way a CLI paints our
/// text defeats it: copilot 1.0.7x prefixes its fallback composer row with
/// `❯ ` (U+276F — a [`POINTER_GLYPHS`] member), draws a `┃ ` border down
/// *every* row its framed composer wrapped our line onto, and echoes the
/// submitted prompt back into its transcript as `❯ <text>` with a right-aligned
/// `HH:mm`. Any one of those leaves a row of OUR text in front of the detector,
/// and it then takes only one such row leading with `❯`/`›`/`→` — which the
/// composer prefix supplies outright, and a wrap boundary supplies whenever a
/// brief's own prose wraps onto one (`red → green`, `main → beta10`) — for
/// `pointer-option` to fire on loomux's own prompt. Nothing repaints an
/// unsubmitted box, so the ring stays frozen on it and
/// [`pointer_rendered`] finds the same glyph among the rendered rows: the #727
/// latch exactly, re-entered through our own paste, and neither reading can
/// ever release it.
///
/// So the comparison is made against the row a CLI actually paints:
///
/// - **De-framed**, so a bordered composer (`┃ our text`) is compared on its
///   content — the same [`deframe`] every other reading here already uses.
/// - **Wrap-reconstructed**, so one pasted line spread over several rows is
///   claimed as one run, via the same [`reconstructs_to_end`] discipline
///   [`mask_loomux_notices_with_record`] uses: contiguous, in order, from the
///   line's own start, and to its END rather than a prefix. A run that does not
///   account for the whole line claims nothing, so the failure direction is
///   under-masking — a hold that stands, which is the cheap error.
/// - **Pointer-stripped, but only as a SECOND attempt and only above an
///   evidence floor.** See [`SELF_ECHO_MIN_POINTER_CHARS`]: a box border is
///   decoration by construction, whereas a pointer glyph is the entire
///   `pointer-option` signal, so stripping one is a claim that needs paying for.
///
/// What does NOT change is the reason this mask is allowed to be greedy about
/// our own text at all: `deliver_prompt` knows EXACTLY what it pasted. A paste
/// whose own text happens to contain `(y/n)` still masks itself away to
/// nothing, because every line of it IS a pasted line, while a dialog the CLI
/// painted mid-paste stays fully visible — that was always the point of a
/// CONTENT-based exclusion, and it is what a byte-COUNT baseline could never do
/// (the history above). #820 changes only HOW a row is recognised as ours,
/// never WHICH text counts as ours.
pub fn mask_own_paste(tail: &str, pasted_text: &str) -> String {
    let rows: Vec<&str> = tail.lines().collect();
    let mut norm: Vec<String> = rows.iter().map(|r| wrap_normalize(deframe(r))).collect();
    // The same rows read once more with a leading pointer glyph removed. Kept
    // beside the ordinary reading rather than replacing it, so the plain
    // comparison is always tried first and the pointer strip can only ever
    // claim MORE — never differently.
    let unpointed: Vec<Option<String>> =
        rows.iter().map(|r| strip_leading_pointer(r).map(wrap_normalize)).collect();
    let pasted: Vec<String> =
        pasted_text.lines().map(wrap_normalize).filter(|l| !l.is_empty()).collect();
    let mut keep = vec![true; rows.len()];
    let mut i = 0;
    while i < rows.len() {
        if norm[i].is_empty() {
            i += 1;
            continue;
        }
        let mut claimed = pasted.iter().find_map(|line| reconstructs_to_end(&norm, i, line, 0));
        if claimed.is_none() {
            if let Some(head) = unpointed[i].clone() {
                // Only this row is re-read without its glyph; the continuation
                // rows of the same wrap run stay on the ordinary reading,
                // because only the FIRST row of a composer's line carries the
                // prompt chevron.
                let framed = std::mem::replace(&mut norm[i], head);
                // #820 residual, the human's case: a SHORT steer. The floor
                // refuses to claim `❯ merge` (5 chars) because `❯ Overwrite` is
                // byte-identical whether it is our composer holding a one-word
                // paste or a live dialog's highlighted choice — and the row
                // cannot tell the difference. Its NEIGHBORHOOD can: every real
                // dialog's option list is headed by the question it is asking —
                // g4's `? Overwrite the existing file?`, Copilot's
                // `● Allow Copilot to run …?` and `● Which retry strategy …?`,
                // Claude's boxed `Which authentication …?` — and a composer
                // never paints one: its chrome above the input row is a
                // divider, the cwd, and `● Ready.`. So the floor is waived
                // exactly when no such question row heads the block above: a
                // short pointer row that is byte-for-byte one of our pasted
                // lines, under no dialog question, is our own composer, and
                // masking it cannot release an Enter into a dialog — there is
                // none there to select into. `dialog_header_above` is the
                // shape-tracking scan (see its doc); the one captured dialog
                // with no question row, the Claude MCP approval's prose, is
                // kept a question by its confirm footer — see g8's h3.
                let own_short_composer = !dialog_header_above(&rows, &norm, &keep, i);
                claimed = pasted
                    .iter()
                    .filter(|line| {
                        line.chars().count() >= SELF_ECHO_MIN_POINTER_CHARS || own_short_composer
                    })
                    .find_map(|line| reconstructs_to_end(&norm, i, line, 0));
                if claimed.is_none() {
                    norm[i] = framed;
                }
            }
        }
        // #871/#903: the CLI COLLAPSED this paste rather than echoing it, so
        // there is no row here that IS our text and every reading above has
        // nothing to match. Claude Code replaces a multi-line paste with a
        // placeholder of its own (`[Pasted text #1 +6 lines]` on the live
        // incident), and this file already knows that happens — the Tier 1
        // precondition declines to govern for exactly this reason. The composer
        // reading never learned it, which is why `idle_row` flips true to FALSE
        // between the pre-paste checkpoint and the pre-Enter one on an unchanged
        // screen, and the Enter is then withheld from a pane that is visibly
        // idle: the delivery aborts with the paste stranded in the box, and every
        // later delivery queues behind it.
        //
        // **The evidence this claims on is the CLI's, not a shape we invented.**
        // A pane that echoed our bytes into a free-text composer is a pane that
        // was not showing a modal — a dialog does not take a paste and render a
        // placeholder for it. What the row proves is authorship, which is all
        // this mask ever claims.
        //
        // **Two terms, both narrowing.** The paste must be MULTI-LINE, because a
        // single line is not one a CLI collapses, so a placeholder beside one is
        // not ours (a long single-line paste that some CLI collapses anyway is a
        // stated residual: it fails to match and the gate holds). And no dialog
        // question row may head the block above, the same term every record
        // claim now carries.
        //
        // Deliberately NOT keyed on the placeholder's `+N lines` count: the
        // exact text is the CLI's to change and this repo has no citable
        // specification of its arithmetic, so pinning one would be a guess
        // dressed as a check. The shape plus the multi-line term is what is
        // actually known.
        if claimed.is_none()
            && pasted.len() > 1
            && collapsed_paste_row(rows[i])
            && !dialog_header_above(&rows, &norm, &keep, i)
        {
            claimed = Some(i + 1);
        }
        match claimed {
            Some(end) => {
                keep[i..end].fill(false);
                i = end;
            }
            None => i += 1,
        }
    }
    rows.iter().zip(keep).filter_map(|(r, k)| k.then_some(*r)).collect::<Vec<_>>().join("\n")
}

/// This row with a leading menu-pointer glyph removed, after any box framing —
/// or `None` where it does not lead with one (#820).
///
/// Deliberately NOT folded into [`deframe`]. `deframe`'s strip set is
/// decoration: a border, a bullet, an indent, none of which any detector rule
/// keys on. A pointer glyph is the opposite — it *is* [`leads_with_pointer`]'s
/// whole signal — so a reading that stripped it globally would delete the
/// `pointer-option` signal outright rather than narrowing it. It is removed in
/// exactly one place, for exactly one question: *is this row our own text with
/// the CLI's prompt chevron in front of it.*
fn strip_leading_pointer(line: &str) -> Option<&str> {
    let d = deframe(line);
    POINTER_GLYPHS.iter().find_map(|g| d.strip_prefix(*g))
}
/// Is this rendered row a CLI's own placeholder for a paste it COLLAPSED
/// instead of echoing (#871/#903)?
///
/// Shape only, and loosely: a bracketed row that names a pasted text. The
/// bracket pair is what keeps it from matching prose — a CLI's placeholder is a
/// self-contained token on the composer's line, not a clause inside a sentence
/// — and the wording is matched case-insensitively and without reading the
/// `#N`/`+M lines` parts, because those are the CLI's to change and there is no
/// citable specification of them to pin. See the call site in
/// [`mask_own_paste`] for the terms that do the narrowing; this predicate is
/// deliberately not one of them.
///
/// A leading pointer glyph is stripped first: Claude Code's composer paints
/// `❯ [Pasted text #1 +6 lines]`, and it is that whole row — chevron included —
/// that must stop being read as a pointer at a menu option.
fn collapsed_paste_row(row: &str) -> bool {
    let inner = strip_leading_pointer(row).unwrap_or_else(|| deframe(row));
    let inner = inner.trim().trim_end_matches(is_frame_char);
    inner.starts_with('[')
        && inner.ends_with(']')
        && inner.to_lowercase().contains("pasted text")
}

/// How long a pasted line must be before [`mask_own_paste`] will claim a row
/// on the strength of a POINTER-stripped comparison (#820).
///
/// The floor exists because the two things being told apart can be
/// byte-identical. `❯ Overwrite` is our own composer holding a one-word paste,
/// and it is also a live dialog's highlighted choice; the only evidence
/// separating them is that we know what we pasted, and a short line is weak
/// evidence — a brief containing a bare `Yes` line would otherwise let
/// `❯ Yes` be masked out of a genuine permission dialog, which releases an
/// Enter into it. That is the #420 harm, and over-masking is the one direction
/// this file never chooses.
///
/// 24 characters, the same figure and the same argument as
/// [`R_TOP_MIN_ANCHOR_CHARS`]: it clears every stock menu option a CLI paints
/// (`Yes`, `No`, `Overwrite`, `Keep both`, `1. Yes`) while sitting far below
/// any line of an orchestrator brief or a steer. The cost is borne only by
/// short claims that were never good evidence.
///
/// **Stated residual:** a delivery whose every line is shorter than this, sitting
/// in a chevron composer, is still not masked and can still latch the gate. The
/// pointer strip is what answers the shape #820 reported; the floor is what
/// keeps that from being a hole, and buying the last case would need evidence
/// this mask does not have. The `matched` field added to
/// `delivery-held-in-queue` in the same change is what makes such a hold name
/// itself.
const SELF_ECHO_MIN_POINTER_CHARS: usize = 24;

/// Does this row look like a live dialog's question line?
///
/// `row` is expected ALREADY reduced — a `norm` entry ([`deframe`] +
/// [`wrap_normalize`]) — because the captured dialog fixtures head their
/// option lists four ways and that reduction is what unifies them: g4's
/// `? Overwrite the existing file?` (opens and closes with the glyph),
/// Copilot's `●`-bulleted `Allow Copilot to run the following command?` and
/// `Which retry strategy …?` (the bullet deframes away), and Claude's boxed
/// `Which authentication approach …?`. Every one of those is a row ending in
/// `?`; the trailing `trim_end_matches(is_frame_char)` additionally unwraps a
/// box that CLOSES its border (`│ Which …? │`) — the captured Claude fixture
/// happens not to paint one. A bare `?`, or a row that is only the glyph, has
/// no words and is not a question. The one captured dialog that asks no
/// question, the Claude MCP approval's prose preamble, has nothing here to
/// detect and is kept a question by its confirm footer instead. Used only as
/// a veto in [`mask_own_paste`], so the bar is set on the side of *seeing* a
/// dialog: a row that merely resembles a prompt keeps the pointer row below
/// it unmasked, which errs into a hold we can release, never into an Enter
/// into a live dialog.
fn is_dialog_header(row: &str) -> bool {
    let row = row.trim_end_matches(is_frame_char);
    let words = row.trim_matches('?').trim();
    !words.is_empty() && (row.starts_with('?') || row.ends_with('?'))
}

/// Is this RAW row part of a dialog's option block rather than its head?
///
/// The upward scan in [`dialog_header_above`] steps over exactly the rows a
/// dialog interposes between its question and the highlighted choice: blank
/// rows, sibling options (a row leading with a pointer glyph, or a box-framed
/// row still indented once the frame is peeled), and indented `$ command`
/// context under Copilot's command dialogs. [`deframe`] would not serve here —
/// it eats indentation, and indentation is the evidence — so the peel set is
/// only the box glyphs and pointers, leaving leading whitespace in place. A
/// row that is none of these ends the block and, once tested as a question,
/// stops the scan.
fn option_block_row(raw: &str) -> bool {
    if raw.trim().is_empty() {
        return true;
    }
    let peeled = raw.trim_start_matches(|c| matches!(c, '│' | '┃' | '|' | '❯' | '›' | '→'));
    peeled.starts_with(char::is_whitespace)
}

/// Is a live dialog's question row in the block above the pointer row at
/// `from`?
///
/// Not a fixed window — a row-count budget would only be calibrated against
/// the captured fixtures, where the pointer sits on the FIRST option and the
/// question is one row away. Real dialogs violate both: arrowing down moves
/// `❯` through the option list, a two-line `$ command` block or a wrapped
/// question adds rows, and Copilot pads with blanks. The scan instead follows
/// the dialog's actual shape: step upward from the pointer, vetoing on the
/// first question row, stepping over the option block ([`option_block_row`]:
/// blanks, sibling options, indented command rows), and stopping at the first
/// row that is neither a question nor option block — a composer's divider or
/// cwd, a dialog's prose preamble. Rows already claimed as our own text
/// (`!keep[j]`) are skipped outright — a row proven to be our paste cannot be
/// a dialog's question, and Copilot echoes a submitted prompt into its
/// transcript as `❯ <text>`, the exact shape the waiver is for.
///
/// The evidence is NEGATIVE — absence of a dialog, not presence of a composer
/// — the one place in this file a claim is bought without paying with content.
/// It is accepted only because every real dialog's question row ends in `?`
/// and the composer chrome above its input row (a divider, the cwd,
/// `● Ready.`) never does, and because the cost of an error is a hold, not a
/// release. Residuals: a command row the CLI does NOT indent under the question
/// stops the scan as if it were chrome — a gap for the rare dialog that paints
/// one unindented (Copilot and Claude both indent theirs); a headerless
/// composer with no divider above its input row, holding agent prose that ends
/// in `?`, still vetoes a short steer — the #820 hold, best-effort; and the
/// ring read is emission order, so an option-block repaint that omits the
/// question line reads headerless until the next full repaint. Another: for
/// this scan, [`option_block_row`]'s peel set is narrower than [`deframe`]'s —
/// it strips only box glyphs and pointers, not the bullets (`*`, `●`, `•`,
/// `◆`) `deframe` also treats as decoration — so a dialog that BULLETS its
/// sibling options instead of indenting them stops the scan before the
/// question, and the waiver then engages on a live highlighted choice; no
/// captured fixture does this, so it is speculative rather than demonstrated.
/// This is unexercised scope, not a protective tradeoff: a composer's divider
/// row (`──…`, U+2500) is peeled by NEITHER set — it carries none of
/// `option_block_row`'s glyphs, none of `deframe`'s bullets — so widening the
/// peel set to match `deframe`'s would not touch g9 h3's divider protection at
/// all; the two are unrelated. The trust model is unchanged from the 24-char
/// floor (an agent that knows the pasted text can paint a header-less row to
/// induce masking); this does not widen it.
fn dialog_header_above(rows: &[&str], norm: &[String], keep: &[bool], from: usize) -> bool {
    let mut j = from;
    while j > 0 {
        j -= 1;
        // #903 B2: the header test runs BEFORE the `keep` check, and the order is
        // the whole finding. Testing `keep` first let a claimed row be stepped
        // over — so two recorded lines were enough to walk this scan past the
        // very question row it vetoes on: claim the dialog's question row with
        // the first, and the second's option row then reads as having no header
        // above it, masks, and the gate releases an Enter into a live dialog.
        //
        // "Is a dialog's question row above this one" is a fact about the
        // SCREEN, and consulting the mask to answer it let the mask decide its
        // own bound. A header now vetoes whether or not its row was claimed,
        // which is a term the record cannot buy at any number of claims.
        if is_dialog_header(&norm[j]) {
            return true;
        }
        // Claimed NON-header rows are still stepped over, unchanged: a loomux
        // notice interleaved above an option block must not end the scan, which
        // is what this clause was for before it was asked to do more.
        if !keep[j] {
            continue;
        }
        if option_block_row(rows[j]) {
            continue;
        }
        return false;
    }
    false
}

/// Remove the rows loomux itself wrote into this pane before the question
/// detector ever sees them (#576).
///
/// **The bug.** `prompt_wait_detected` asks "does this pane look parked on a
/// question", and loomux's own notices are text *about* questions: a relayed
/// `report` note or `message_orchestrator` text lands as
/// `[orrerix] w-7 reports blocked: Copilot asked "do you want to run npm test?
/// (y/n)"`. That satisfies two of the detector's structured signals, so the
/// gate latches — and because a held pane emits nothing new, no fresh output
/// ever pushes it back out of the scan window. An orchestrator pane is the most
/// exposed, since relayed worker prose is most of what gets written to it.
///
/// #534 does not cover this and could not: it lets the *grid* release a hold
/// the ring is still asserting, but our own text is genuinely **rendered**, so
/// both readings agree it is on screen. They are right — it is on screen. It is
/// simply not a question, and only the marker can say so.
///
/// **Exactly one row per marker, and never the rows around it.** The tempting
/// version masks the marker's whole wrap-run: a notice is one logical line
/// (`truncate_notice` strips control characters, so it cannot contain a
/// newline), a terminal wraps one logical line into a contiguous run of
/// non-blank rows, and only the first row carries the marker — so a run-mask
/// would also catch a `(y/n)` that wrapped onto row two. It is rejected because
/// the marker cannot support it. Since an agent can print a marker row itself
/// (see [`NOTICE_MARKER`]), a run-mask hands any pane the power to
/// delete the seven rows below an attacker-chosen row — and a genuine
/// permission dialog painted there would be masked into "no question", which
/// releases an Enter into it. That is the #420 harm, reachable from pane
/// output. Failing OPEN is the dangerous direction, so the mask claims only
/// the row the marker actually leads.
///
/// **A multi-row notice is the PRODUCER's problem, never this function's
/// (#632).** The rule above says one row per marker, so loomux text occupying
/// several rows is only fully maskable if every row it emits leads with the
/// marker once `deframe`d — which is a shape each producer owes, not a claim
/// this mask can widen to cover for them. Both directions of that are load
/// bearing: a producer that skips it hands the detector a row of loomux prose
/// (the #632 bug), and a mask that compensated by taking neighbouring rows
/// would be the run-mask rejected two paragraphs up. See
/// [`unmaskable_framing_rows`] for the shared invariant the producers assert
/// themselves against, and `deframe`'s strip set for why the bullet in a
/// continuation row must be `•` and never `-`.
///
/// **What the marker alone leaves, and what closes it (#576 residual).** A
/// notice that wraps keeps whatever tokens landed past the first row; a marker
/// row that has itself scrolled off leaves its continuation unmarked. Both are
/// under-masks — the gate holds when it might have cleared, which is the cheap
/// error the ten-minute `QuestionStale` badge already covers. Closing them
/// needs loomux to know *what* it wrote to a pane, not merely that a row claims
/// it did: that is [`DeliveredNotices`], and the mask that consults it is
/// [`mask_loomux_notices_with_record`]. THIS function is the record-free form —
/// the marker rule and nothing else — and it stays, because a pane whose record
/// was lost (a restart) and a producer asserting its own maskability at its
/// door ([`unmaskable_framing_rows`]) must both be answered by the marker rule
/// alone. See the record-aware function for why the record cannot widen a
/// producer's door.
///
/// The residual false-release surface is correspondingly one row wide: a pane
/// would have to paint a row that both leads with the marker and *is itself*
/// the live question. A CLI paints its dialog rows itself and does not prefix
/// them with our marker, and an agent printing the whole thing has not rendered
/// a dialog at all — it has printed prose about one.
pub fn mask_loomux_notices(tail: &str) -> String {
    mask_loomux_notices_with_record(tail, &[])
}

/// Does this row LEAD with the notice marker, after any box framing?
///
/// The single definition of the marker rule (the `leads_with_pointer`
/// precedent): [`mask_loomux_notices_with_record`] and
/// [`loomux_authored_lines`] both call it, so what the record REMEMBERS and
/// what the mask CLAIMS cannot drift apart — a record holding lines the mask
/// would not have recognised is a record that widens nothing, and the reverse
/// is a mask reaching for lines that were never kept.
///
/// De-framed so a notice echoed inside a box UI (`│ [orrerix] …`) is still seen
/// to LEAD its row — the same rule, and the same reason, as
/// `leads_with_pointer`. Lowercasing is [`brand::leading_notice_marker`]'s own
/// contract, so a re-cased echo still matches.
///
/// **Every accepted spelling, not just today's** (#1153 phase 3). A pane's
/// scrollback is written once and read for as long as the pane lives: rows an
/// agent captured before the rename lead with the legacy marker, and a mask
/// that stopped recognising them would quietly start leaking pre-rename
/// notices into the very run-mask this function exists to feed.
fn leads_with_notice_marker(line: &str) -> bool {
    brand::leading_notice_marker(deframe(line)).is_some()
}

/// One row (or one recorded line) reduced to what a wrap cannot change:
/// case-folded, with every whitespace run collapsed to a single space and the
/// ends trimmed.
///
/// A terminal wrapping one logical line re-distributes it across rows, and a
/// CLI re-rendering it may re-indent the continuations; neither alters the
/// sequence of words. Comparing on this normal form is therefore what lets a
/// run of rows be checked against the line it came from without the check
/// depending on the pane's width — which the mask deliberately does not know
/// (see [`mask_loomux_notices_with_record`]).
fn wrap_normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Do `rows[from..]` reconstruct `line` from byte offset `at` all the way to
/// its END, one row at a time?
///
/// Returns the row index just past the run, or `None` if the rows do not
/// account for the whole remainder. Both halves are load bearing:
///
/// - **Contiguous, in order, from the anchor.** Each row must continue exactly
///   where the previous one stopped, optionally across the single space a word
///   wrap eats at the boundary (a hard wrap mid-word eats nothing, so the space
///   is optional rather than required). A blank row ends the run: a wrapped
///   logical line has none.
/// - **To the end, never a prefix.** A run that merely *starts* a recorded line
///   proves nothing about the rows after it, and claiming them would be the
///   run-mask [`mask_loomux_notices`] rejects. Requiring the remainder to be
///   consumed exactly means the last claimed row is the line's last row, so
///   whatever a pane painted below it is untouched. The direction of the error
///   is right too: a notice the CLI truncated with an ellipsis, or one cut off
///   by the bottom of the reading, simply fails to reconstruct and the gate
///   keeps holding.
fn reconstructs_to_end(rows: &[String], from: usize, line: &str, at: usize) -> Option<usize> {
    let mut rest = &line[at..];
    let mut i = from;
    while i < rows.len() && !rest.is_empty() {
        let row = rows[i].as_str();
        if row.is_empty() {
            break;
        }
        let candidate = rest.strip_prefix(' ').unwrap_or(rest);
        match candidate.strip_prefix(row) {
            Some(next) => {
                rest = next;
                i += 1;
            }
            None => break,
        }
    }
    (rest.is_empty() && i > from).then_some(i)
}

/// [`mask_loomux_notices`], plus the rows a per-pane record of delivered text
/// proves loomux wrote (#576 — the wrap residual and the scrolled-off marker).
///
/// **What `delivered` is, and why it is the only thing allowed to widen the
/// mask.** It is the marker-led lines loomux has actually written into THIS
/// pane ([`DeliveredNotices`]) that a producer has ALSO promised the pane's own
/// agent did not author any span of ([`OrchRegistry::mark_notice_maskable`]).
/// A pane cannot add to it: the record is written on the delivery side, from
/// the text loomux pasted, so pane output — the direction
/// [`NOTICE_MARKER`] is forgeable in — cannot reach it. That is the
/// property #576 asked for and the reason the widening below is keyed off the
/// record and never off the marker: **an agent-printed marker row still widens
/// nothing**, because a row that merely looks like a notice matches no recorded
/// line (#420's harm, restated as the rule this function obeys).
///
/// The second half of that — the producer's promise — is what stops the
/// unforgeability being hollow: a line loomux wrote can still be a line an
/// agent CHOSE, and the record must not hand an agent the mask over its own
/// pane. See the residual note at the bottom for the review finding that
/// established this.
///
/// **#903 widened what `delivered` may contain, and narrowed what a claim
/// costs.** It now also carries the PROMPT bodies loomux delivered into this
/// pane's CLI **session** ([`DeliveredPrompts`], unioned in by
/// [`OrchRegistry::delivered_mask_lines`]), because the rows this mask could
/// not claim on a resumed pane were loomux's own previous deliveries replayed
/// by the CLI — not notices at all. Two things keep the widening from being a
/// weakening:
///
/// - **Admission is by provenance and excludes notice text.**
///   [`delivered_prompt_lines`] drops every marker-led line, so the one-party
///   route [`OrchRegistry::mark_notice_maskable`] documents — an agent putting a
///   line of its own choosing into its own pane's record via
///   `notify_when(note:)` — stays exactly as closed as it is today. What is
///   admitted is a kickoff brief or an orchestrator's `send_prompt` body, and no
///   agent can address one of those to itself — and that is now ENFORCED rather
///   than observed: `send_prompt` refuses `a.id == caller.agent_id` outright
///   ("cannot send a prompt to yourself"), and a kickoff's target is an agent
///   `spawn_agent` has just created, which the caller cannot be. The two routes
///   that DID let an agent reach its own pane — the post-compact re-grounding
///   notice and `resume_kickoff_notice`, both of which paste the agent's own
///   directive ledger — are refused by [`prompt_record_admits_kind`] and by
///   [`delivered_prompt_lines`]'s first-line rule respectively.
/// - **Every record claim now owes `dialog_header_above`**, not only the
///   pointer-stripped ones. That is a strictly smaller set of claims than the
///   pre-#903 rule made, so the widening cannot reach a row the old rule would
///   have refused.
///
/// The residual that remains — an orchestrator delivering rows that coincide
/// with a real dialog's, a dialog with no question row of its own, a header
/// whose text the same party controls — is argued in
/// `docs/design/question-gate-authorship.md` and is bounded, not closed.
///
/// **Two rules, both anchored in the record:**
///
/// - *A wrapped notice (`R-wrap`).* A run of rows starting at a row that is the
///   START of a recorded line and reconstructing that line to its end is
///   claimed whole. The anchor is the recorded line's own beginning, so the
///   marker plays no part in the decision — a recorded line leads with the
///   marker by construction, which makes its first row one
///   [`mask_loomux_notices`] would have claimed anyway; the new part is only
///   the continuation, and only where the continuation *is* the rest of that
///   line.
/// - *A marker that scrolled off (`R-top`).* Every reading here is truncated at
///   the TOP (the byte ring keeps the last bytes; the grid keeps the last
///   rows), so the one place a line can legitimately appear headless is the
///   first non-empty row of the reading. There, and only there, a run may
///   anchor MID-line — at any offset of a recorded line — provided it still
///   reconstructs that line to its end. Anywhere else a mid-line anchor would
///   be a claim about rows loomux cannot show it wrote.
///
/// **Why the width is not consulted.** Predicting the wrap points would need
/// the pane's geometry AND the CLI's own re-rendering of the text (prefixes,
/// re-indents, hard vs word wrap), and being wrong about them fails in the
/// dangerous direction — a wrongly-predicted run would claim a row that is not
/// ours. Reconstruction asks the safe question instead: not "where would this
/// have wrapped" but "is this row, verbatim, the next thing we wrote".
///
/// **The residual, restated accurately after review (rev-163 B1).** The record's
/// CONTENT is partly agent-authored — a relayed `report` note is the worker's
/// own words inside loomux's framing — so an agent that can (a) get a
/// marker-led line delivered into a pane, (b) print that line's head into that
/// same pane, and (c) have its CLI paint a live dialog whose rows are
/// byte-identical to that line's remainder, in order, hides the dialog.
///
/// An earlier version of this paragraph called that "two parties and three
/// coincidences". **That was wrong**, and the way it was wrong is the reason
/// entry to the record is now an explicit producer promise: via `notify_when`'s
/// agent-supplied `note`, all three are available to ONE agent in its OWN pane,
/// deterministically — the note is a tool argument it writes itself, the
/// delivered line is typed into its own pane so it knows the bytes, and the
/// "coincidence" in (c) is a choice it makes when authoring the note. See
/// [`OrchRegistry::mark_notice_maskable`], which is what closes it: a line is
/// claimable only where its producer has promised the pane's own occupant did
/// not author any span of it.
///
/// **What remains is PROXY-AUTHORSHIP by the recipient, and it is
/// orchestrator-shaped (rev-163 B3).** An earlier version of this paragraph
/// called the remainder a two-party case needing the author to induce the
/// recipient. That is also too kind, for the same reason as before: the check
/// [`OrchRegistry::deliver_relayed_to_root`] performs is CALLERSHIP
/// (`from != root`), not authorship. An orchestrator that tells a worker
/// "report with this exact note" gets its own chosen text marked — `from` is
/// the worker, so the check passes — and delivered into its OWN pane. The
/// worker complying is ordinary compliance, not a compromise, and the
/// orchestrator then prints the head row into its own pane at will, needing no
/// inducement at all.
///
/// The scope is the whole surface, not a corner: both marked call sites target
/// the orchestrator, so **every claimable line in the system lands in the pane
/// of the one agent best placed to dictate its content**. What it buys that
/// agent is precisely the #420 harm — loomux pressing Enter on a permission
/// dialog its own CLI was asking a HUMAN to answer.
///
/// **Accepted knowingly, not mitigated.** Authorship is not enforceable here:
/// an orchestrator instructing a worker is loomux's ordinary traffic and has
/// no attack signature to key on. The alternatives are the ones already
/// rejected — refusing question-shaped rows is vacuous, and refusing
/// agent-supplied fields deletes the motivating case. Two levers exist if this
/// is judged too expensive, and both are subtractions rather than fixes: drop
/// `R-top`'s mid-line anchor for marked lines (shrinks the surface, does not
/// close it, and gives up #576's scrolled-off half), or stop marking
/// altogether (the pre-record behaviour — the wrap residual simply stays open).
///
/// Under-masking stays the cheap error and this function keeps choosing it: any
/// mismatch, any missing tail, any unmarked line, any lost record ends the run.
pub fn mask_loomux_notices_with_record(tail: &str, delivered: &[String]) -> String {
    let rows: Vec<&str> = tail.lines().collect();
    let mut norm: Vec<String> = rows.iter().map(|r| wrap_normalize(deframe(r))).collect();
    let recorded: Vec<String> = delivered.iter().map(|d| wrap_normalize(d)).collect();
    let first_row = norm.iter().position(|n| !n.is_empty());
    let mut keep = vec![true; rows.len()];
    let mut i = 0;
    while i < rows.len() {
        if norm[i].is_empty() {
            i += 1;
            continue;
        }
        // The marker rule: one row, no record needed. Everything below only ever
        // claims MORE rows, never this one differently.
        //
        // #903 reads it through a leading POINTER glyph as well, and the reason is
        // the same replay that made the session record necessary. A resumed CLI
        // renders a replayed user turn with a `❯` in front of it, and `deframe`
        // does not treat a pointer as framing — so a notice loomux delivered and
        // the CLI replayed comes back as `❯ [orrerix] …` and the marker rule
        // stops seeing its own marker. Fifteen of the thirty-nine
        // `pointer-option` holds in the group's audit log were marker-led notices
        // wearing exactly that chevron.
        //
        // It claims no more than the marker rule already claims: ONE row, the one
        // the marker leads. The #420 objection to widening this — that an agent
        // can print a marker row itself, so a RUN-mask hands a pane the power to
        // delete the rows beneath an attacker-chosen row — is untouched, because
        // a pane that can print `[orrerix] …` can already print it and get that
        // single row claimed. Adding a chevron in front of it buys the pane
        // nothing it did not have.
        if leads_with_notice_marker(rows[i])
            || strip_leading_pointer(rows[i]).is_some_and(leads_with_notice_marker)
        {
            keep[i] = false;
        }
        let mut claimed = record_claim(&norm, &recorded, first_row, i);
        if claimed.is_none() {
            // #903: the same POINTER-stripped second reading `mask_own_paste`
            // has, and here for a reason that is not symmetry. A resumed CLI
            // renders a replayed USER turn with a leading `❯`, so the row that
            // starts a recorded prompt on a resumed pane's screen is the
            // recorded line with a chevron in front of it and matches nothing.
            // That row is the one this whole record exists to claim.
            //
            // Only the anchor row is re-read: a wrap run's continuations carry
            // no chevron, so stripping one off them would be inventing evidence.
            // And only lines that clear [`SELF_ECHO_MIN_POINTER_CHARS`] may
            // claim on the strength of a stripped pointer, for that constant's
            // own reason — `❯ Yes` is byte-identical whether it is a replayed
            // one-word turn of ours or a live dialog's highlighted choice.
            if let Some(head) = strip_leading_pointer(rows[i]).map(wrap_normalize) {
                if !head.is_empty() {
                    let long: Vec<String> = recorded
                        .iter()
                        .filter(|l| l.chars().count() >= SELF_ECHO_MIN_POINTER_CHARS)
                        .cloned()
                        .collect();
                    let framed = std::mem::replace(&mut norm[i], head);
                    claimed = record_claim(&norm, &long, first_row, i);
                    if claimed.is_none() {
                        norm[i] = framed;
                    }
                }
            }
        }
        // #903: a record claim is REFUSED when a live dialog's own question row
        // heads the block above the anchor.
        //
        // This narrows every record claim, not only the pointer-stripped ones,
        // and the uniformity is deliberate: one rule for the whole record path
        // means "which record did this line come from" is never something the
        // mask has to get right per row. It can only ever refuse a claim the
        // pre-#903 rule would have made — the fail-CLOSED direction, which costs
        // a hold the ten-minute `QuestionStale` badge already reports.
        //
        // It is also the term the widened record is bounded by. A recorded line
        // that happens to coincide with a dialog's rows cannot delete them out
        // from under the detector while the dialog's own question row is still
        // above them; `dialog_header_above` is the same shape-tracking scan
        // `mask_own_paste` uses for its short-pointer case. What it does NOT
        // bound is stated in `docs/design/question-gate-authorship.md`: a dialog
        // with no question row of its own, and a header whose text the same
        // party controls.
        if claimed.is_some() && dialog_header_above(&rows, &norm, &keep, i) {
            claimed = None;
            // N3: put the row back the way the pointer-stripped attempt found
            // it. That attempt rewrites `norm[i]` in place and only restores it
            // on ITS own miss, so a claim it made and this veto then nulled left
            // the stripped form behind for every later reader of `norm` — the
            // upward scans above included. It was inert only by coincidence
            // before B2; now that those scans test `is_dialog_header(&norm[j])`
            // on rows regardless of `keep`, a row left stripped of its leading
            // glyph is a row this function reads differently than the screen
            // shows it.
            norm[i] = wrap_normalize(deframe(rows[i]));
        }
        match claimed {
            Some(end) => {
                keep[i..end].fill(false);
                i = end;
            }
            None => i += 1,
        }
    }
    rows.iter()
        .zip(keep)
        .filter_map(|(r, k)| k.then_some(*r))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One record-anchored claim attempt at row `i`: `R-wrap` first, then `R-top`.
///
/// Extracted from [`mask_loomux_notices_with_record`] (#903) so the row can be
/// re-read with its leading pointer glyph stripped without the two rules being
/// written out twice — a second spelling of `R-top`'s floor or of
/// reconstruct-to-end is exactly the drift this file's guards keep finding.
fn record_claim(
    norm: &[String],
    recorded: &[String],
    first_row: Option<usize>,
    i: usize,
) -> Option<usize> {
    recorded.iter().find_map(|line| {
        reconstructs_to_end(norm, i, line, 0).or_else(|| {
            // `R-top`: a mid-line anchor, allowed at the first non-empty
            // row of the reading and nowhere else. `at > 0` because offset
            // zero is `R-wrap` above — a headless run is by definition
            // missing something.
            (first_row == Some(i) && norm[i].chars().count() >= R_TOP_MIN_ANCHOR_CHARS)
                .then(|| {
                    line.match_indices(norm[i].as_str())
                        .find_map(|(at, _)| {
                            (at > 0).then(|| reconstructs_to_end(norm, i, line, at)).flatten()
                        })
                })
                .flatten()
        })
    })
}

/// The lines of a delivery that [`mask_loomux_notices`] would claim — i.e. the
/// part of what loomux just pasted that is loomux's OWN writing, and the only
/// part [`DeliveredNotices`] can ever hold.
///
/// **Why the filter, rather than recording the whole payload.** Most of what
/// `deliver_now` pastes is agent text: a kickoff brief, an orchestrator's
/// prompt, the verbatim constituent payloads of a coalesced flush. Recording
/// those would give the record-aware mask the power to blind the gate to
/// ordinary agent content — the exact blindness `e11` pins as deliberately NOT
/// taken (a queued delivery whose own body is question-shaped must park the
/// pane exactly as it would have unqueued). Marker-led lines are loomux's
/// framing, so the record holds only what the mask was always allowed to claim,
/// and the record's job is limited to the rows those lines WRAPPED onto.
///
/// **A necessary condition, never a sufficient one (rev-163 B1).** A line
/// passing this filter is still not recordable: it must ALSO have been marked
/// by its producer through [`OrchRegistry::mark_notice_maskable`]. See that
/// method for the attack this ordering exists to stop — a marker-led line is
/// loomux's *framing*, and framing routinely carries a field some agent chose.
///
/// De-framed on the way in, so what the record holds is what
/// [`mask_loomux_notices_with_record`] compares against: the mask normalizes
/// rows as `wrap_normalize(deframe(row))`, and a payload line that arrived
/// framed would otherwise be recognised here (`leads_with_notice_marker` itself
/// de-frames), stored WITH the frame, and then match nothing (rev-163 N2 — a
/// fail-closed drift, but a drift, and `leads_with_notice_marker`'s doc claims
/// there is none).
pub fn loomux_authored_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| leads_with_notice_marker(l))
        .map(|l| deframe(l).trim().chars().take(DELIVERED_NOTICE_CHARS).collect())
        .collect()
}
/// The lines of a delivery that are loomux-ORIGINATED **prompt** text — a
/// kickoff brief or a `send_prompt` body — as opposed to a notice (#903).
///
/// The complement of [`loomux_authored_lines`], and the complement is the whole
/// safety argument rather than a tidy split. A notice is text loomux *writes*,
/// and the record of one is deliberately opt-in per producer, because
/// `notify_when(kind, pr, note)` takes an agent-supplied `note` and delivers the
/// fired notice into the **registering agent's own pane** — so admitting notice
/// text by provenance alone would let one agent, with one tool call it makes
/// itself, put a line of its choosing into its own pane's record, print that
/// line's head, and have its CLI paint a dialog whose rows are that line's
/// remainder. That is [`OrchRegistry::mark_notice_maskable`]'s documented
/// one-party attack, and this filter is what keeps it closed: marker-led lines
/// are excluded here and stay on the opt-in rule, unchanged.
///
/// What is left is text that reached a pane because loomux was asked to deliver
/// a PROMPT there — a kickoff brief, or an orchestrator's `send_prompt` body
/// (a coalesced flush's constituent payloads arrive here ONE AT A TIME, as
/// their own prompt bodies — the drainer splits the framing off before calling,
/// which is why this function never has to parse a flush apart; see
/// `record_contributions` on [`deliver_now`], and #632 for the split it reuses).
/// No agent can address one of those to itself — `send_prompt` refuses
/// a caller that names its own id, and a kickoff goes to an agent that did not
/// exist when the caller asked for it. `send_prompt` is the
/// orchestrator's tool, so the residual this admits is chiefly an orchestrator
/// writing into somebody else's pane — a party that already holds spawn and prompt
/// powers over that pane. The design note argues that asymmetry at length; it is
/// bounded, not closed.
///
/// Lines longer than [`DELIVERED_PROMPT_CHARS`] are DROPPED, never truncated —
/// see that constant for why a prefix would be a claim whose meaning depends on
/// the pane's width.
pub fn delivered_prompt_lines(text: &str) -> Vec<String> {
    // #903 B1 — TERM 1, and the finding that forced it. Filtering marker-led
    // lines ONE AT A TIME is not the same rule as excluding notices: a loomux
    // notice is one marker-led first line followed by a body that is not, so
    // every continuation row of a multi-line notice was entering this record.
    //
    // That is a one-party hole, not a cosmetic one. `note_directive(text,
    // replace: true)` writes the ledger RAW — no sanitize, no cap, no `[ts]`
    // prefix, and it is self-scoped by construction — and both
    // `compact_reinjection_notice` (via the self-callable `request_compact`) and
    // `resume_kickoff_notice` paste that ledger VERBATIM into the SAME agent's
    // pane, below their marker-led first line. So one agent, with two tool calls
    // it makes itself, could put a line of its own choosing into its own
    // session's record — exactly the capability
    // [`OrchRegistry::mark_notice_maskable`] is default-closed to withhold.
    //
    // The rule is therefore about the authorship of the DELIVERY, not of each
    // line: if loomux wrote the first non-empty line as a notice, loomux is
    // relaying whatever follows rather than originating it. That generalises to
    // every future embed, because a loomux notice is marker-led by producer
    // convention (#624/#632 enforce it at the producer), where a content
    // blocklist would need editing for each new one.
    //
    // Unforgeable in the direction that matters: this text is what loomux
    // pasted, and an agent controls only the block BELOW that first line. An
    // orchestrator prefixing its own `send_prompt` with the marker only
    // downgrades its own masking, which is the safe direction.
    if text.lines().find(|l| !l.trim().is_empty()).is_some_and(leads_with_notice_marker) {
        return Vec::new();
    }
    text.lines()
        .filter(|l| !leads_with_notice_marker(l))
        .map(|l| deframe(l).trim())
        .filter(|l| !l.is_empty() && l.chars().count() <= DELIVERED_PROMPT_CHARS)
        .map(str::to_string)
        .collect()
}

/// #903 B1': what a paste may contribute to the session prompt record.
///
/// One rule for both shapes, which is why it is three lines: the entries' OWN
/// texts under their OWN kinds, never the string that was pasted. A coalesced
/// flush pastes loomux's framing wrapped around N constituent payloads (#533-A),
/// and even a lone delivery can carry a flush header on its front — so the bytes
/// on the wire are a mixture whose parts have different authors and, in the flush
/// case, different [`Delivery`] kinds. The queue still has them apart at this
/// point; #632 already owns the framing/payload split
/// ([`unmaskable_framing_rows`]), and nothing is gained by making the record
/// re-derive it from a rendered string.
///
/// **Pure, and that is the point of extracting it** — the same argument
/// [`override_enter_admits`] carries. Welded into the drainer this rule would
/// need an `AppHandle` to exercise, so nothing in this repo could drive it, and
/// the round that introduced it shipped a regression no test could see: term 1
/// excluded the framed whole, which took the constituent payloads #903 needs
/// masked out of the record with the header.
///
/// Each pair is still admitted on its own merits downstream — both #903 B1 terms
/// run per contribution — so a re-grounding notice riding in a batch is refused
/// by its kind and a `resume_kickoff_notice` by its marker-led first line,
/// whatever they are flushed alongside.
#[doc(hidden)] // pub for integration tests
pub fn record_contributions_for(batch: &[queue::QueuedDelivery]) -> Vec<(String, Delivery)> {
    batch
        .iter()
        .filter_map(|e| e.payload.text().map(|t| (t.to_string(), e.delivery_kind)))
        .collect()
}

/// #903 B1 — TERM 2: may a delivery of this KIND contribute to the session
/// prompt record at all?
///
/// Spelled as an exhaustive match with **no wildcard arm**, for the reason
/// [`question_shown`] spells its `GridEvidence` reading the same way: a fifth
/// [`Delivery`] variant added later must decide which side it is on at the
/// compiler's insistence, instead of inheriting admission from a `_ => true`
/// nobody re-reads.
///
/// - `FreshKickoff` / `ResumeKickoff` — the brief an orchestrator wrote for a
///   spawn. `ResumeKickoff` in particular is the incident's OWN payload: the
///   `[orch] Round 3 (cap) re-record…` that wedged `rev-1277` reached `rev-1262`
///   on this kind, so refusing it would close the door by regressing #903.
/// - `MidSession` — a `send_prompt` body, orchestrator-authored. The accepted
///   two-party residual, argued in `docs/design/question-gate-authorship.md`.
/// - `Regrounding` — REFUSED. Its entire payload is the post-compact notice
///   whose body is the agent's own directive ledger.
///
/// This is the SECOND of two independent terms, and neither is redundant:
/// `ResumeKickoff` carries both an orchestrator brief (admitted here) and, at
/// the promoted-orchestrator call site, `resume_kickoff_notice`'s ledger embed —
/// which only TERM 1 refuses. Kind alone would let that through; content alone
/// would let a future non-marker-led embed through.
#[doc(hidden)] // pub for integration tests
pub fn prompt_record_admits_kind(kind: Delivery) -> bool {
    match kind {
        Delivery::FreshKickoff | Delivery::ResumeKickoff | Delivery::MidSession => true,
        Delivery::Regrounding => false,
    }
}

/// One pane's delivery record, or empty when there is no registry to ask.
///
/// `deliver_now`'s `reg` is `Option` for the headless wiring that has no
/// registry at all; an absent record is the same legitimate "nothing known"
/// an untouched pane has, and means the marker rule (#576).
fn delivered_lines(reg: &Option<Arc<OrchRegistry>>, pty_id: u32) -> Vec<String> {
    // #1702: this helper is the one place the delivery path resolves a pty to a
    // session, and it does so with no registry guard held — `deliver_now` and
    // the gates that call it take the per-pane delivery lock, never a registry
    // map. A caller that DOES hold one must pass its own session instead; see
    // [`OrchRegistry::session_for_pty`].
    reg.as_ref()
        .map(|r| r.delivered_mask_lines(pty_id, r.session_for_pty(pty_id).as_deref()))
        .unwrap_or_default()
}

/// The evidence floor a MID-LINE anchor must clear (rev-163 N1).
///
/// `R-top` claims a run whose first row is a fragment of a recorded line, on
/// the argument that the head scrolled off. Without a floor the fragment could
/// be one character, so any short first row that happens to appear somewhere in
/// a recorded line would carry a claim over everything below it that completes
/// the line — a one-character "proof" that the head scrolled off.
///
/// 24 chars is chosen from what the case being served actually looks like: a
/// scrolled-off wrap continuation is a full terminal row, so it is tens of
/// characters at minimum, and the narrowest pane anyone runs still leaves a
/// continuation far above this. The cost of the floor is therefore borne only
/// by fragments that were never good evidence.
const R_TOP_MIN_ANCHOR_CHARS: usize = 24;

/// Longest recorded line, in chars. `notify::NOTICE_TOTAL_CAP` already caps a
/// composed notice at 400; 512 leaves room for the framing a producer adds
/// around one without ever letting an unbounded payload in. A line longer than
/// this is truncated, which costs the tail of that one notice its wrap masking
/// (it can no longer reconstruct to its end) and nothing else — fail-closed, in
/// the direction everything here fails.
pub const DELIVERED_NOTICE_CHARS: usize = 512;

/// Lines remembered per pane. The mask can only ever use a line that is still
/// RENDERED, and `prompt_wait_detected` honours its structured signals across
/// the last twelve non-empty lines, so a notice that twenty-three later notices
/// have already pushed past is long out of every reading that consults this.
/// Drop-oldest, the `PendingIntake`/`OrchNoticeInbox::park` shape.
pub const DELIVERED_NOTICES_PER_PANE: usize = 24;
/// #903: how many characters of one delivered PROMPT line the session record
/// below keeps.
///
/// Four times [`DELIVERED_NOTICE_CHARS`], and the figure is measured rather
/// than picked: the live wedge this record exists for replayed an
/// **853-character** orchestrator prompt as ONE logical line, and
/// [`reconstructs_to_end`] can only claim a run that accounts for a recorded
/// line *to its end*. A cap sized for a notice's one sentence would therefore
/// record a prefix that reconstructs against nothing, and the gate would keep
/// holding with the record looking populated.
///
/// A line LONGER than this is dropped rather than truncated. Recording a prefix
/// would leave a line that can only ever be claimed by accident — a run whose
/// rows happen to end exactly where the truncation did — so the record would
/// carry entries whose meaning depends on a pane's width. Dropping degrades to
/// the same cheap error every other loss here does: the gate holds.
pub const DELIVERED_PROMPT_CHARS: usize = 2048;

/// #903: prompt lines remembered per SESSION, drop-oldest.
///
/// A resumed CLI replays its transcript's tail, so what can be on a resumed
/// pane's screen is the last few user turns — not the session's whole history.
/// Sixteen covers that with room to spare while keeping the record small enough
/// that its worst case is arithmetic rather than a hope.
pub const DELIVERED_PROMPT_LINES_PER_SESSION: usize = 16;

/// #903: sessions tracked at once, evicting the least-recently-written.
///
/// The ceiling this and the two constants above fix is
/// 2048 x 16 x 32 = 1 MiB, argued the same way [`DELIVERED_NOTICE_PANES`]
/// argues its own: a group is a handful of agents and a long session resumes
/// some of them, so this is generous for the live fleet while bounding a map
/// that would otherwise grow with every session id a run ever saw.
pub const DELIVERED_PROMPT_SESSIONS: usize = 32;

/// Panes tracked at once, evicting the least-recently-written. A group is a
/// handful of panes and a long session respawns some, so this is generous for
/// the live fleet while bounding the map that would otherwise grow with every
/// pty id a session ever used (`last_delivery`'s map does the same and is never
/// pruned, but its entries are three fields — these are up to
/// 24 × 512 chars, so the ceiling has to be stated: 64 panes ≈ 786 KiB worst
/// case).
pub const DELIVERED_NOTICE_PANES: usize = 64;

/// What loomux knows it wrote into one pane (#576): a bounded, drop-oldest
/// record of the marker-led lines it has pasted there.
///
/// **In memory only, and deliberately so.** The record's single consumer is a
/// mask over a LIVE pane's rendered tail, so it is worth exactly as long as the
/// rows it explains are still on screen. Persisting it would put agent-authored
/// note text in a second on-disk place with its own schema-version burden
/// (`queue.json` is versioned; a second file or a new field in that one is a
/// contract change), to buy masking for rows that a restarted pane has almost
/// always redrawn past. Losing it degrades to exactly the pre-#576 behaviour —
/// the marker rule alone, so a wrapped notice latches the gate until the
/// ten-minute `QuestionStale` badge surfaces it — which is the cheap error, and
/// the same one every other failure path here chooses.
/// **Two phases, and both are required (rev-163 B1).** A line becomes
/// claimable only if its PRODUCER marked it ([`OrchRegistry::mark_notice_maskable`],
/// which is the authorship promise) and a WRITE then delivered it
/// ([`OrchRegistry::record_delivered_text`], which is the "it really is on that
/// screen" half). Marking without a write claims text nobody painted; writing
/// without a mark claims text an agent may have chosen. Neither alone is
/// enough, and the default — a line nobody marked — is never claimable no
/// matter how often it is delivered.
#[derive(Debug, Default)]
pub struct DeliveredNotices {
    lines: VecDeque<RecordedLine>,
    /// Monotonic write stamp, for evicting the least-recently-written PANE.
    /// Not a clock: eviction only needs an order, and `now_ms()` would make the
    /// record's bound depend on the wall clock.
    seq: u64,
}

/// One line of [`DeliveredNotices`], and which of the two phases it has
/// reached. Only `written` lines are handed to the mask.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedLine {
    text: String,
    written: bool,
}

impl DeliveredNotices {
    /// Phase 1: a producer promises this text's spans are safe to claim in this
    /// pane. Nothing is claimable yet — the line is parked until a write
    /// delivers it.
    fn note_pending(&mut self, text: &str, seq: u64) {
        self.seq = seq;
        for text in loomux_authored_lines(text) {
            if self.lines.iter().any(|l| l.text == text) {
                continue;
            }
            self.lines.push_back(RecordedLine { text, written: false });
            while self.lines.len() > DELIVERED_NOTICES_PER_PANE {
                self.lines.pop_front();
            }
        }
    }

    /// Phase 2: these bytes just went to the pane. A line that was marked
    /// becomes claimable; a line that was NOT marked is ignored — deliberately
    /// silently, since most deliveries carry unmarked loomux framing and that
    /// is the ordinary case, not an error.
    fn note_written(&mut self, text: &str, seq: u64) {
        self.seq = seq;
        for delivered in loomux_authored_lines(text) {
            if let Some(l) = self.lines.iter_mut().find(|l| l.text == delivered) {
                l.written = true;
            }
        }
    }

    /// The claimable lines: marked by a producer AND delivered.
    fn claimable(&self) -> Vec<String> {
        self.lines.iter().filter(|l| l.written).map(|l| l.text.clone()).collect()
    }
}

/// #903: the prompt bodies loomux has delivered into one CLI **session**.
///
/// Keyed by session and not by pane, which is the entire point. A resumed pane
/// is a NEW pty replaying an OLD transcript, so a per-pane record is empty
/// exactly when the screen is fullest — and the rows on it are loomux's own
/// previous deliveries, rendered by the CLI with a leading pointer glyph, which
/// is what latched `prompt_wait_match`'s `pointer-option` signal for the whole
/// life of the pane. The record has to outlive the pane because the text does.
///
/// **One phase, not two** — unlike [`DeliveredNotices`], which parks a line
/// until a producer promises it. There is no producer to ask here: admission is
/// by PROVENANCE, and [`delivered_prompt_lines`] is the promise, made once,
/// structurally, about what may enter at all. Every line in here is text loomux
/// put on the wire as a prompt, recorded after the write succeeded.
///
/// **Not verbatim, and the difference is load-bearing rather than sloppy**: each
/// line is stored [`deframe`]d and trimmed, because that is the form the mask
/// compares against. A rendered row reaches `record_claim` as
/// `wrap_normalize(deframe(row))`, so a record holding the raw line would fail to
/// match any row whose leading glyph the CLI painted — and a brief line beginning
/// `* ` or `● ` is exactly such a row. Storing the compared form is what keeps the
/// two sides of that comparison from drifting.
#[derive(Debug, Default)]
pub struct DeliveredPrompts {
    lines: VecDeque<String>,
    /// Monotonic write stamp for evicting the least-recently-written SESSION —
    /// an order, not a clock, for [`DeliveredNotices`]'s reason.
    seq: u64,
}

impl DeliveredPrompts {
    /// These bytes just went to a pane bound to this session.
    fn note_written(&mut self, text: &str, seq: u64) {
        self.seq = seq;
        for line in delivered_prompt_lines(text) {
            if self.lines.iter().any(|l| *l == line) {
                continue;
            }
            self.lines.push_back(line);
            while self.lines.len() > DELIVERED_PROMPT_LINES_PER_SESSION {
                self.lines.pop_front();
            }
        }
    }

    fn claimable(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }
}

/// The rows of `rendered` that loomux WROTE and [`mask_loomux_notices`] could
/// not claim (#632). Empty is the invariant every multi-row notice owes.
///
/// **Why a helper rather than an assertion spelled out at each producer.**
/// [`mask_loomux_notices`] claims exactly one row per marker, so a notice that
/// occupies several rows is only fully maskable if *every* row it emits leads
/// with the marker once `deframe`d. #624 established that convention for
/// single-line notices and enforced it at `OrchNoticeInbox::park`; the two
/// pre-existing multi-row producers (`pause_suppression_notice` and
/// `queue::coalesced_flush_text`) build their own blocks, so their door is
/// their own last line. Both call this, and so do their tests — written
/// *through* `mask_loomux_notices` rather than re-deriving its rule, so the
/// check cannot drift from what it stands in for (the `park` precedent).
///
/// **`payloads` is the deliberate exemption, and it is narrow.** A coalesced
/// flush carries each constituent delivery's text VERBATIM, and those rows are
/// agent-authored by design: they are byte-identical to what the same delivery
/// would have painted on its own, so masking them would blind the gate to
/// ordinary pane content that no other delivery path hides. They are passed in
/// here and excused; everything else in the block is loomux's own framing and
/// must mask away. See the #632 section of `docs/design/orchestration.md` for
/// why leaving them to latch is the conservative direction.
///
/// Comparison is on trimmed, non-empty rows: blank rows carry no tokens for
/// `prompt_wait_detected` to match, and a framing row that were ever
/// byte-identical to a payload row would be excused — a false negative in an
/// assertion, which is the harmless direction for a guard.
pub fn unmaskable_framing_rows(rendered: &str, payloads: &[&str]) -> Vec<String> {
    let payload_rows: HashSet<&str> = payloads
        .iter()
        .flat_map(|p| p.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    mask_loomux_notices(rendered)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !payload_rows.contains(l))
        .map(str::to_string)
        .collect()
}

/// The live interactive-question guard's hold decision (#420), generic over
/// the tail read so it's integration-tested with a scripted closure — no
/// `PtyManager`, no real PTY (rev-15 B4: the old production-bound version of
/// this predicate could never be exercised by a test that disabled it, since
/// nothing but a live pty could drive it; this generic form is what a test
/// drives directly, and the production wrapper below just supplies a real
/// closure over `ptys`).
///
/// - `tail()` — raw (ANSI-included) bytes of the pane's current output tail.
/// - `pasted_text` — `None` for a checkpoint that runs BEFORE this delivery
///   has written anything (nothing of ours is on screen yet to mask);
///   `Some(the exact text this delivery pasted)` for a checkpoint that runs
///   after it (pre-Enter, each retry) — `mask_own_paste` (above) strips our
///   own lines out of the tail before the detector ever sees it (rev-15 N1 /
///   rev-19 B-A).
///
/// **Release is STATE-based, not ACTIVITY-based (rev-19 R1).** An earlier cut
/// released the hold on a human keystroke (a submitted, non-sitting one). That
/// was wrong on both ends: not sufficient (an arrow key *navigating the still-
/// open menu* — `HumanInput::Neutral`, no text left sitting — satisfied the old
/// condition and let the freed Enter fire straight into the still-open dialog)
/// and not necessary (a question can clear with no local keystroke at all —
/// the CLI times its own prompt out, or answers itself from a recorded
/// consent). Worse, xterm's own automatic terminal-query replies (#179) can
/// stamp a pane's keystroke-recency clock with no human present at all. So
/// human activity is dropped from this decision ENTIRELY: the only thing that
/// gets to say the question is gone is the SAME detector that said it was
/// there — `prompt_wait_detected` reading false. A single false read is not
/// enough on its own (a redraw mid-flicker could transiently miss the menu),
/// so release requires it read false on two CONSECUTIVE polls; the very first
/// poll of a hold that was never actually shown a question releases
/// immediately (`ever_shown` gates the two-poll requirement to only kick in
/// once a real hold has genuinely started — see `wait_for_question_clear`'s
/// own fast-path for why a never-active hold must not pay any extra latency).
///
/// #496 PR-A made `PtyManager::note_user_input` gate the keystroke-recency
/// stamp itself on `classify_human_input`, so an xterm auto-reply no longer
/// refreshes it either — the clock this comment distrusts is materially more
/// trustworthy than it was when rev-19 R1 was written. That does NOT change
/// this decision: release still needs to be STATE-based, because the "not
/// sufficient" half of the finding above — an arrow key navigating a
/// still-open menu reads `HumanInput::Neutral` too, and #496 does not stamp
/// pure-Neutral input either — is untouched by tightening what the clock
/// tracks. This predicate stays exactly as-is.
///
/// #534 kept all of the above and added a second reading. The logic now lives
/// in [`question_hold_predicate_sampled`]; THIS function is the ring-only
/// entry point — the pre-#534 guard exactly, and the fallback whenever a
/// pane's screen cannot be composed. Everything the paragraphs above say about
/// what may and may not release a hold applies unchanged to both, because both
/// are the same closure: the grid narrows *when* the detector reads clear, it
/// does not touch the two-consecutive-poll rule that decides what a clear read
/// is worth. See [`question_shown`] for the one behaviour that differs.
pub fn question_hold_predicate<T>(
    tail: T,
    pasted_text: Option<String>,
    delivered: Vec<String>,
) -> impl Fn() -> bool
where
    T: Fn() -> Option<Vec<u8>>,
{
    // Ring only, no composition — exactly the reading this guard had before
    // #534, and the one it falls back to whenever a pane's screen cannot be
    // trusted. `visible: None` is not "nothing is rendered"; it is "we have no
    // rendered-rows evidence", which `question_shown` treats as the ring's
    // word being final.
    question_hold_predicate_sampled(
        move || QuestionSample { ring: tail(), visible: None },
        pasted_text,
        None,
        delivered,
    )
}

/// One poll's worth of readings for the question guard (#534): the same
/// instant seen two ways.
pub struct QuestionSample {
    /// Raw (ANSI-included) bytes from the pane's append-only output ring —
    /// what the guard has always read.
    pub ring: Option<Vec<u8>>,
    /// The pane's currently-rendered rows, composed by [`question_visible`]
    /// ([`termgrid::render_visible`]'s rows, less the input box's faint
    /// placeholder, #3426). `None` means *no trustworthy
    /// composition*, never *a blank screen* — the two must not collapse,
    /// because one licenses a release and the other must not.
    pub visible: Option<String>,
}

/// What the composed screen says about a match the byte ring made (#534).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridEvidence {
    /// The match — or some other question shape — is among the rendered rows.
    StillRendered,
    /// The screen composed cleanly and holds neither. Releases the hold.
    NotRendered,
    /// The screen composed cleanly and shows the CLI sitting at an EMPTY input
    /// prompt with no menu selection anywhere on it (#903) — whatever the ring
    /// matched, and whatever of it is still rendered, is text on an idle pane
    /// rather than a question anybody can answer. Releases the hold.
    ///
    /// Its own variant rather than folding into `NotRendered` because the two
    /// are different findings and the audit is read by humans: `not-rendered`
    /// says the dialog went away, `idle-prompt` says there was never a dialog —
    /// which is the whole of #903 and the single most useful line in the log
    /// when the detector fires on prose again.
    IdlePrompt,
    /// No composition worth reading (pty gone, geometry unknown, nothing
    /// painted). Proves nothing in either direction; the ring's word stands.
    Unreadable,
}

impl GridEvidence {
    /// Audit vocabulary — stable, kebab-case, read by humans diagnosing the
    /// next #513-shaped incident.
    pub fn as_str(&self) -> &'static str {
        match self {
            GridEvidence::StillRendered => "still-rendered",
            GridEvidence::NotRendered => "not-rendered",
            GridEvidence::IdlePrompt => "idle-prompt",
            GridEvidence::Unreadable => "unreadable",
        }
    }
}

/// Read the composed screen for evidence about `m`.
///
/// Two independent ways to answer "still displayed", OR'd, because a false
/// `NotRendered` is the expensive error (it releases an Enter toward a dialog
/// that may be live) and a false `StillRendered` is the cheap one (a hold the
/// human is already badged about at ten minutes):
///
/// - [`prompt_wait_detected`] over the rendered rows — catches a dialog the
///   CLI repainted with different text than the ring matched.
/// - [`match_still_rendered`] — catches a dialog sitting outside the
///   detector's own last-12-lines window, which is a *chronological* rule
///   applied here to a *spatial* layout and so cannot be relied on alone.
///
/// **#903 puts one reading ahead of both of them**, and the ordering is the
/// change: [`idle_prompt_rendered`] is consulted FIRST, so a pane sitting at an
/// empty composer answers `IdlePrompt` even when the matched text is plainly
/// still on the screen. That inverts the asymmetry above for exactly one case,
/// deliberately. The asymmetry is right when the question is "did the dialog go
/// away" — absence of evidence is weak, so weight it toward holding. It is
/// wrong when the question is "was this ever a dialog", because there the
/// screen is offering *positive* evidence that it was not: a CLI showing an
/// empty free-text box is not blocked on an answer. #903's two lost panes had a
/// matched line, still rendered, forever, on a pane nobody was being asked
/// anything by — `StillRendered` was true and useless.
pub fn grid_evidence_for(m: &QuestionMatch, visible: Option<Composed<'_>>) -> GridEvidence {
    match visible {
        None => GridEvidence::Unreadable,
        Some(c) if idle_prompt_rendered(c) => GridEvidence::IdlePrompt,
        // Both of these read the MASKED view, unchanged: "is a question
        // displayed" was never allowed to be answered by loomux's own rows.
        // `Composed` widens what the guard can see, not what counts as a
        // question.
        Some(c) if prompt_wait_detected(c.masked) || match_still_rendered(c.masked, m) => {
            GridEvidence::StillRendered
        }
        Some(_) => GridEvidence::NotRendered,
    }
}

/// The question guard's reading for one poll, from both signals (#534).
///
/// **The ring is the trigger; the grid can only ever release.** Written out,
/// because the asymmetry is the safety argument and not an implementation
/// detail:
///
/// | ring | grid | reading | |
/// |---|---|---|---|
/// | no match | (not consulted) | clear | |
/// | match | `Unreadable` | hold | |
/// | match | `StillRendered` | hold | |
/// | match | `NotRendered` | **clear** | #534's one change |
/// | match | `IdlePrompt` | **clear** | #903's one change |
///
/// Every row but the last is today's behaviour, so the entire behavioural
/// surface of this change is a single transition, in a single direction, and
/// the design note argues exactly that one. Notably the grid is never allowed
/// to *create* a hold the ring did not: that would be a new false-positive
/// class (screen content the ring had already scrolled past), and #420/#427
/// forbid weakening the guard but nothing asks us to strengthen it here.
///
/// A release still is not a write. This reading feeds
/// `question_hold_predicate`, which requires
/// [`QUESTION_RELEASE_CONSECUTIVE_CLEAR_POLLS`] consecutive clear reads; the
/// write that follows is admitted by `write_admission`, which re-reads box
/// occupancy at the instant of the write (#532). "The question is gone AND the
/// box is empty" is therefore a conjunction the caller already enforces — see
/// the design note for why it is not re-derived here.
pub fn question_shown(ring: Option<&QuestionMatch>, visible: Option<Composed<'_>>) -> bool {
    match ring {
        None => false,
        // Spelled as the two readings that HOLD rather than as `!= NotRendered`
        // (#903): a sixth `GridEvidence` added later must decide which side it
        // is on at the compiler's insistence, instead of inheriting "hold" — or,
        // worse, "release" — from whichever way the comparison happened to be
        // written.
        Some(m) => matches!(
            grid_evidence_for(m, visible),
            GridEvidence::StillRendered | GridEvidence::Unreadable
        ),
    }
}

/// Is a composed screen worth reading as evidence, or is it a replay that
/// began blind and was never painted over (#534)?
///
/// A composition with essentially nothing on it is not a clear screen; it is
/// an absent one, and the distinction is the whole difference between "the
/// dialog is gone" and "we did not see the dialog". Returning `None` costs a
/// hold that was already going to happen; the alternative is treating a blank
/// grid as proof.
///
/// It is not a coherence check and does not pretend to be one — a garbled but
/// well-populated composition passes here. See the design note's limits
/// section for what that leaves open.
pub fn trustworthy_composition(visible: String) -> Option<String> {
    (visible.lines().filter(|l| !l.trim().is_empty()).count() >= GRID_MIN_RENDERED_ROWS)
        .then_some(visible)
}

/// What the guard held for, recorded for the abort audit (#513(c)/F2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionWitnessed {
    /// The detector's match — signal class and the line it fired on.
    pub matched: QuestionMatch,
    /// What the composed screen said about it on that same poll.
    pub grid: GridEvidence,
    /// Did that same screen show the CLI's own input prompt — empty, or holding
    /// nothing but loomux's paste ([`idle_prompt_row_rendered`], the WEAK
    /// reading)? #903.
    ///
    /// It rides the witness rather than a fifth out-parameter for two reasons.
    /// It is genuinely diagnostic — "the guard held while a composer was on
    /// screen" is the #903 finding in one bit, and it is emitted by
    /// [`witness_audit`] so the next narrowing has it. And it is only ever
    /// *needed* when the ring matched, which is exactly when a witness exists:
    /// the last-resort override is defined over a pane whose question gate is
    /// holding, and a gate cannot hold without a match.
    pub idle_row: bool,
}

/// Slot the hold predicate writes its last observed match into, so the abort
/// site can say WHY it held. Single-threaded by construction (the predicate is
/// a closure polled by one delivery thread), hence `Rc`/`RefCell` rather than
/// a lock.
pub type QuestionWitness = std::rc::Rc<std::cell::RefCell<Option<QuestionWitnessed>>>;

/// The `matched` field every question-guard audit record now carries
/// (#513(c)/F2) — or `null` where the guard genuinely never saw a question.
///
/// `null` is a real answer and is why this returns a value rather than
/// omitting the key: for a `delivery-aborted-question` record it means the
/// abort outcome and the detector disagree, which is itself the finding. The
/// old records were indistinguishable from that case in every direction, which
/// is why #513's live 27-minute incident is still unexplained.
///
/// Four fields, each earning its place in a log that rotates at 8 MiB:
/// `signal` says which detector rule fired (the fastest way to spot a rule
/// misfiring on prose), `line` says what it fired on (bounded by
/// [`QuestionMatch::MAX_LINE`]), `grid` says whether the composed screen
/// agreed — the one that turns "the guard held" into "the guard held and the
/// screen backed it up", or into the opposite — and `idle_row` (#903) says
/// whether the CLI's own composer was on that screen, which is the term the
/// last-resort override is decided on and therefore the one a human has to be
/// able to audit after it fires.
pub fn witness_audit(seen: Option<&QuestionWitnessed>) -> Value {
    match seen {
        None => Value::Null,
        Some(w) => json!({
            "signal": w.matched.signal,
            "line": w.matched.line,
            "grid": w.grid.as_str(),
            "idle_row": w.idle_row,
        }),
    }
}

/// [`question_hold_predicate`], generalized over a sample that may carry
/// composed-screen evidence, and able to record what it saw (#534).
pub fn question_hold_predicate_sampled<T>(
    sample: T,
    pasted_text: Option<String>,
    witness: Option<QuestionWitness>,
    // #576: what loomux knows it wrote to this pane
    // (`OrchRegistry::delivered_mask_lines` — the per-pane notice record plus
    // the per-session prompt record, #576/#903). A REQUIRED argument rather than
    // a defaulted one: every production caller has it, and a call site that
    // silently got an empty record would be a gate quietly running on the
    // pre-#576 rule with nothing to say so (the #544 "never acquired by
    // omission" shape). Empty is a legitimate value — a pane loomux has written
    // nothing into, or one whose record a restart dropped — and it means
    // exactly the marker rule.
    delivered: Vec<String>,
) -> impl Fn() -> bool
where
    T: Fn() -> QuestionSample,
{
    let ever_shown = std::cell::Cell::new(false);
    let consecutive_clear = std::cell::Cell::new(0u32);
    move || {
        let s = sample();
        // `mask_own_paste` runs on BOTH readings or the guard would be
        // inconsistent with itself: our own just-pasted text sits in the box
        // and is therefore *rendered*, so an unmasked grid read would answer
        // "still displayed" about our own paste (rev-15 N1 / rev-19 B-A).
        // Two masks, both on BOTH readings. `mask_own_paste` needs this
        // delivery's text and so is `None` at every checkpoint that runs before
        // one (`question_active_now`'s call sites — four since #819 added
        // `stranded_marker_action`'s); the notice mask needs
        // no `pasted_text` at all, which is exactly why it closes #576 — the
        // outer drainer gate has no `pasted_text` to mask with, yet the pane is
        // full of the PREVIOUS delivery's notices. Since #576's residual it
        // reads `delivered` as well, so a notice that WRAPPED is masked over
        // every row it wrapped onto rather than only its first.
        let mask = |t: &str| {
            let t = mask_loomux_notices_with_record(t, &delivered);
            match &pasted_text {
                Some(p) => mask_own_paste(&t, p),
                None => t,
            }
        };
        let ring_match =
            s.ring.as_deref().and_then(|out| prompt_wait_match(&mask(&strip_ansi(out))));
        // #903 rev-427 B1: BOTH views are kept, and the fully-masked one is still
        // the only thing "is a question displayed" is asked of. The other answers
        // one narrow question the masked one structurally cannot — is the CLI's
        // composer on screen — because `mask_own_paste` deletes the very row that
        // proves it.
        //
        // rev-433: the pair differs by the PASTE mask ALONE. Both have had the
        // notice mask applied first, so the authorship diff cannot mistake one of
        // loomux's own `[orrerix] …` notice rows near the bottom of a transcript
        // for the composer. See [`Composed`].
        let with_paste =
            s.visible.as_deref().map(|v| mask_loomux_notices_with_record(v, &delivered));
        let masked = with_paste.as_deref().map(|v| match &pasted_text {
            Some(p) => mask_own_paste(v, p),
            None => v.to_string(),
        });
        let composed = match (with_paste.as_deref(), masked.as_deref()) {
            (Some(with_paste), Some(masked)) => Some(Composed { masked, with_paste }),
            _ => None,
        };
        let shown = question_shown(ring_match.as_ref(), composed);
        if let (Some(w), Some(m)) = (&witness, ring_match) {
            // Recorded on every poll the ring matched, INCLUDING the polls
            // that read clear on the grid — the abort audit wants the last
            // thing seen, and "the ring kept matching but the screen said
            // gone" is the single most useful line a #513 diagnosis could
            // have. Never cleared on a no-match poll: an abort is preceded by
            // whatever the hold was about, and blanking it would leave the
            // audit saying nothing again.
            let grid = grid_evidence_for(&m, composed);
            // #903: the override's own term, recorded on the SAME poll as the
            // decision it will be used for — never re-read at the override site,
            // for the reason this whole witness exists.
            let idle_row = composed.is_some_and(idle_prompt_row_rendered);
            *w.borrow_mut() = Some(QuestionWitnessed { matched: m, grid, idle_row });
        }
        if shown {
            ever_shown.set(true);
            consecutive_clear.set(0);
            return true;
        }
        if !ever_shown.get() {
            return false; // never actually holding — no artificial delay
        }
        let n = consecutive_clear.get() + 1;
        consecutive_clear.set(n);
        n < QUESTION_RELEASE_CONSECUTIVE_CLEAR_POLLS
    }
}

/// Production wrapper: hold prompt delivery to `pty_id` while a live
/// interactive question is on screen (#420), reusing `hold_for_human_input`'s
/// generic block-until-clear-or-capped loop exactly like `wait_for_box_clear`
/// does, just with `question_hold_predicate` (bound to this pane, via
/// `output_tail_bounded` — rev-15 N4) as the "still occupied" predicate.
/// `pasted_text` is threaded straight through — see `question_hold_predicate`'s
/// doc for what `None` vs `Some` means at each call site. Owns the
/// delivery-held badge around the wait (the SAME shape every other hold in
/// this function uses: pre-check with zero elapsed hold, so the badge only
/// fires for a hold that actually happens) so every caller — the pre-paste
/// checkpoint, the pre-Enter checkpoint, and each spaced retry (rev-15 B2) —
/// gets identical badge/hold behavior from one place instead of re-deriving
/// it. A closed pty or unreadable tail reads as "no question" so a dead/gone
/// pane never blocks the thread.
///
/// Since #534 it reads both signals per poll (see [`question_sample`]) and
/// returns, alongside the decision, the LAST thing the detector matched —
/// `None` when the guard never saw a question at all. Callers that abort put
/// it in the audit; that field is the whole of #513(c), and the reason it is
/// returned rather than re-derived at the abort site is that a fresh read
/// there would describe a different instant than the one that decided.
fn wait_for_question_clear(
    ptys: &crate::pty::PtyManager,
    pty_id: u32,
    pasted_text: Option<&str>,
    delivered: Vec<String>,
    emit_held: &impl Fn(HeldReason),
    emit_held_cleared: &impl Fn(),
) -> (PasteDecision, Option<QuestionWitnessed>) {
    let witness: QuestionWitness = Default::default();
    let predicate = question_hold_predicate_sampled(
        || question_sample(ptys, pty_id),
        pasted_text.map(str::to_string),
        Some(std::rc::Rc::clone(&witness)),
        delivered,
    );
    let will_hold = predicate();
    if will_hold {
        emit_held(HeldReason::InteractiveQuestion);
    }
    let decision = hold_for_human_input(&predicate, QUESTION_HOLD_MAX, QUESTION_HOLD_POLL);
    if will_hold {
        emit_held_cleared();
    }
    let seen = witness.borrow().clone();
    (decision, seen)
}

/// Both of the question guard's readings for one pane, taken from ONE tail
/// read (#534).
///
/// One read, not two, and the reason is correctness rather than cost: a second
/// `output_tail_bounded` call would sample a *different instant*, so the ring
/// could match text the composition — taken microseconds later, after the CLI
/// erased it — legitimately no longer holds. The guard would then be comparing
/// two screens and calling the difference evidence. Slicing the ring window out
/// of the tail the grid replays keeps both readings answering about the same
/// bytes.
///
/// The ring slice is the LAST [`QUESTION_SCAN_TAIL_BYTES`], preserving the
/// pre-#534 detector window exactly — including its habit of starting
/// mid-codepoint, which `strip_ansi` has always absorbed.
fn question_sample(ptys: &crate::pty::PtyManager, pty_id: u32) -> QuestionSample {
    let raw = ptys.output_tail_bounded(pty_id, QUESTION_GRID_REPLAY_BYTES);
    // Geometry is required, never defaulted. `get_output` may fall back to
    // 80x24 because a wrong width there only re-wraps prose in something a
    // human reads; here a wrong width changes which cells hold which
    // characters, and a wrapped `(y/n)` that fails to match would read as
    // "not displayed" — the one direction this must never be wrong in. No
    // size, no grid evidence.
    let visible = match (raw.as_deref(), ptys.size(pty_id)) {
        (Some(bytes), Some((cols, rows))) => question_visible(bytes, cols, rows),
        _ => None,
    };
    let ring = raw.map(|b| b[b.len().saturating_sub(QUESTION_SCAN_TAIL_BYTES)..].to_vec());
    QuestionSample { ring, visible }
}

/// The composed screen the question guard reads (#534), with the input box's
/// PLACEHOLDER removed (#3426).
///
/// Split out of [`question_sample`] so the one decision it adds is drivable from
/// raw bytes by a test — `question_sample` itself needs a live `PtyManager`.
///
/// **Why the placeholder has to go.** After a turn, Claude Code writes a guess
/// at the human's next prompt into its empty input box as placeholder text
/// (`❯ main is green now — rebase onto origin/main and re-run CI`). As TEXT
/// that row is a prompt glyph leading content, which is two things this guard
/// reads as NOT idle: a `pointer-option` row (the ring's trigger, and
/// [`pointer_rendered`]'s veto on the grid), and a composer that is not empty,
/// so [`idle_prompt_row_rendered`] is false. The second is what wedged #3426: it
/// blocks #903's idle-composer release AND starves
/// [`question_override_admits`] of the idle reads it counts, so a hold on
/// prose that the idle release exists for held for thirty minutes and the
/// fifteen-minute override never fired.
///
/// **What tells it apart, and why nothing else would.** Text cannot: a
/// suggestion and a line the human typed are the same characters in the same
/// place. The attribute can. Claude Code paints its placeholder with chalk
/// `dim` — SGR 2, faint — with at most the first character in inverse video (its
/// block cursor, drawn only while the terminal has focus); typed input is
/// painted at normal intensity. So [`placeholder_blanked`] clears a row's
/// content only when EVERY content cell is faint, allowing just that one
/// leading inverse cell. The CLI's idle SIGNAL was the alternative the issue
/// named and it is not used: the one idleness signal this guard has is the
/// rendered composer, and a hook- or transcript-based "turn ended" is
/// CLI-specific plumbing that would still not say whether a dialog is up now.
///
/// **Scoped to one row: the LOWEST row that leads with a prompt glyph** — the
/// composer, since a CLI's input box sits below its transcript. A faint row
/// higher up is transcript, and clearing it could manufacture an "empty
/// composer" above a live dialog. A dialog's highlighted choice (`❯ 1. Yes`) is
/// painted at normal intensity, so the rule leaves it — and every question row
/// on screen — exactly as it was. Every other row is `render_visible`'s,
/// unchanged.
///
/// **The rule does not check that the lowest glyph-led row IS the composer.**
/// With a glyph-less dialog up (reverse-video `AskUserQuestion`), the lowest
/// such row is whatever sits above it. Any faint row led by a prompt glyph, such
/// as a past prompt or a dim hint or tool-output line starting with `$` or `>`,
/// is cleared, and the WEAK idle reading turns true. Facts about today's screens
/// keep that closed, not this rule (Claude Code paints past prompts at normal
/// intensity). The residual is argued in `docs/design/orchestration.md`'s #3426
/// section and pinned by
/// `residual_a_faint_prompt_row_above_a_glyphless_dialog_reads_as_an_idle_composer`.
#[doc(hidden)] // pub for integration tests
pub fn question_visible(bytes: &[u8], cols: u16, rows: u16) -> Option<String> {
    let styled = termgrid::render_visible_styled(bytes, cols, rows);
    let mut text: Vec<String> =
        styled.iter().map(|r| r.iter().map(|c| c.ch).collect()).collect();
    let composer = text.iter().rposition(|t| {
        let d = deframe(t);
        PROMPT_GLYPHS.iter().any(|g| d.starts_with(*g))
    });
    if let Some(i) = composer {
        if let Some(blanked) = placeholder_blanked(&styled[i]) {
            text[i] = blanked;
        }
    }
    trustworthy_composition(text.join("\n"))
}

/// This composer row with its placeholder cleared, or `None` when the row holds
/// anything a human could have typed (#3426).
///
/// Reads the cells after the prompt glyph, less any trailing frame (the `│` of a
/// boxed composer). `Some` only when there is content, at least one content cell
/// is faint, and every content cell is faint — except the FIRST, which may
/// instead be inverse, because that is where the CLI draws its cursor over the
/// placeholder. A typed character at normal intensity anywhere refuses, and
/// refusing leaves the row as it was: the guard's pre-#3426 reading, which is
/// the direction it is always allowed to err in.
///
/// Whitespace is never evidence either way: an inverse SPACE is the cursor
/// after typed text, and a faint space is indistinguishable from any other.
fn placeholder_blanked(row: &[termgrid::StyledCell]) -> Option<String> {
    let glyph = row.iter().position(|c| !is_frame_char(c.ch))?;
    if !PROMPT_GLYPHS.contains(&row[glyph].ch) {
        return None;
    }
    let end = row.iter().rposition(|c| !is_frame_char(c.ch)).map_or(glyph + 1, |e| e + 1);
    let content: Vec<(usize, &termgrid::StyledCell)> = row
        .iter()
        .enumerate()
        .take(end)
        .skip(glyph + 1)
        .filter(|(_, c)| !c.ch.is_whitespace())
        .collect();
    let first = content.first()?.0;
    let placeholder = content.iter().all(|(i, c)| c.faint || (*i == first && c.inverse))
        && content.iter().any(|(_, c)| c.faint);
    if !placeholder {
        return None;
    }
    let s: String = row
        .iter()
        .enumerate()
        .map(|(i, c)| if i > glyph && i < end { ' ' } else { c.ch })
        .collect();
    Some(s.trim_end().to_string())
}

/// One-shot "is a question on screen right now" snapshot (#420 rev-15 B1) —
/// for a checkpoint that just needs to know NOW, not hold-and-wait: the
/// stranded-text flush must not blind-Enter into a live dialog, but it's not
/// itself the guard responsible for holding — the pre-paste checkpoint that
/// immediately follows it owns that. A single call to the hold predicate is
/// exactly a one-shot read: its two-consecutive-clear release requirement only
/// engages once a hold has actually observed a question at least once
/// (`ever_shown`), so a lone call just answers "is it shown right now".
///
/// #534: this reads the composed screen too. It has to — this is the gate
/// `write_admission` consults at the instant of the write, so a checkpoint
/// still keyed on the byte ring alone would re-assert, one call later, the
/// hold the guard had just released on grid evidence. No witness: nothing
/// downstream of a one-shot read has an abort record to put one in.
fn question_active_now(
    ptys: &crate::pty::PtyManager,
    pty_id: u32,
    pasted_text: Option<&str>,
    delivered: Vec<String>,
) -> bool {
    question_active_witnessed(ptys, pty_id, pasted_text, delivered).active
}

/// One one-shot reading of a pane's question gate (#903): the decision, the
/// evidence, and — for the last-resort override — whether that same instant's
/// composed screen showed the CLI's own input prompt.
///
/// A struct rather than a widening tuple because the third field is easy to
/// mistake for the second's negation and is not: `active` is the gate,
/// `idle_prompt` is a fact about the CURRENT render that
/// [`question_override_admits`] is allowed to act on only after a bound has
/// elapsed. They disagree exactly in the case #903 exists for.
pub struct QuestionReading {
    /// Is the question gate holding right now?
    pub active: bool,
    /// What the detector matched on this poll, for the audit.
    pub witnessed: Option<QuestionWitnessed>,
    /// Did this poll's composed screen show the CLI's input prompt — empty, or
    /// holding nothing but loomux's own paste ([`idle_prompt_row_rendered`], the
    /// WEAK reading, without [`idle_prompt_rendered`]'s menu-absent conjunct)?
    ///
    /// **The weak one, deliberately, and it is the whole of the override's
    /// design** (rev-427 B2). Feeding this from the strong reading would make the
    /// override unreachable: the strong reading is what `grid_evidence_for`
    /// already releases on, so a pane satisfying it is not holding, and there
    /// would be nothing left to override. The override IS the time-bounded
    /// downgrade of that one conjunct — see [`question_override_admits`] and the
    /// design note for what stops it pressing Enter into a live menu.
    ///
    /// `false` for an unreadable screen, and `false` when the ring matched
    /// nothing: no composition (or no hold) means no idleness claim.
    pub idle_prompt: bool,
}

/// [`question_active_now`], plus WHAT the detector matched (#820).
///
/// The one-shot read is where a *sustained* hold lives. `deliver_now`'s own
/// holds are individually capped and every one of them already audits its
/// match (`witness_audit`, #513(c)/F2); the queue drainer re-arms this read
/// every `QUEUE_DRAIN_POLL` with **no** cap, so the record a human actually
/// diagnoses a strand from is the drainer's — and that record said
/// `blocked_on: "question"` and nothing whatever about which shape, on which
/// line, with what the screen said about it. #513's blind spot, one hold
/// class over, and the reason #820's false positive could not name itself:
/// the pane was held for the whole of a session by a signal no record
/// identified.
///
/// A witness rather than a re-read at the audit site, for the same reason
/// `wait_for_question_clear` returns one: a fresh read there would describe a
/// different instant than the one that decided.
fn question_active_witnessed(
    ptys: &crate::pty::PtyManager,
    pty_id: u32,
    pasted_text: Option<&str>,
    delivered: Vec<String>,
) -> QuestionReading {
    let witness: QuestionWitness = Default::default();
    let active = question_hold_predicate_sampled(
        || question_sample(ptys, pty_id),
        pasted_text.map(str::to_string),
        Some(std::rc::Rc::clone(&witness)),
        delivered,
    )();
    let seen = witness.borrow().clone();
    // #903: taken off the witness rather than re-derived here, so the term the
    // override is decided on and the record that explains it are the same poll's
    // reading of the same screen — and so there is exactly ONE place
    // (`question_hold_predicate_sampled`) that decides what "the composer is on
    // screen" means. `None` — the ring matched nothing, so no witness — reads
    // `false`, which is right twice over: there is no hold to override, and a
    // streak must not be built out of polls that were never holding.
    let idle_prompt = seen.as_ref().is_some_and(|w| w.idle_row);
    QuestionReading { active, witnessed: seen, idle_prompt }
}

/// #532: the guards a delivery must find satisfied **at one instant**, on the
/// pass that actually commits to a write.
///
/// A straight line of checkpoints is what inverted both safety signals in
/// #532. `deliver_now` checked box occupancy (#111/#171) FIRST and the
/// interactive question (#420) SECOND — and the second one *blocks*, for up to
/// `QUESTION_HOLD_MAX` (two minutes). Nothing re-read occupancy afterwards, so
/// the delivery pasted on a green light earned two minutes earlier against a
/// box that was empty *then*.
///
/// That is not a rare interleaving; it is the ordinary one, because the same
/// event resolves the second gate and violates the first. A human typing is
/// what pushes a stale question out of `prompt_wait_detected`'s window (their
/// echo shifts the byte ring), so on a stale hold the keystroke that *releases*
/// the question gate is the same keystroke that *occupies* the box. The
/// delivery then pasted onto their half-written line and the pre-Enter quiet
/// wait submitted the merged result the moment they paused — the human's
/// report, mechanism for mechanism.
///
/// So every gate is re-read here, together, and a checkpoint's answer is never
/// carried across a wait that can outlive it. `box_pending` is checked first
/// because #510 is the absolute: human-typed content in the box outranks even a
/// question, since the cost of holding too long is a badge and the cost of
/// submitting over a person's line is unrecoverable.
///
/// Pure and three-way (rather than the `bool` this replaces at three call
/// sites) so the precedence is directly pinnable and so a caller can badge and
/// enqueue with the reason that actually blocked instead of re-deriving it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteAdmission {
    /// Every gate is clear right now — paste, or press.
    Go,
    /// Human-typed characters are outstanding in the box (#111/#171, #510).
    HoldBoxOccupied,
    /// An interactive question/permission TUI is on screen (#420).
    HoldQuestion,
}

impl WriteAdmission {
    /// Whether this admission permits a write right now.
    pub fn go(self) -> bool {
        matches!(self, WriteAdmission::Go)
    }
    /// The delivery-held badge reason for the gate that blocked, so a caller
    /// never picks one that disagrees with the gate it actually stopped on.
    /// `None` for `Go` — there is no hold to badge.
    pub fn held_reason(self) -> Option<HeldReason> {
        match self {
            WriteAdmission::Go => None,
            WriteAdmission::HoldBoxOccupied => Some(HeldReason::BoxOccupied),
            WriteAdmission::HoldQuestion => Some(HeldReason::InteractiveQuestion),
        }
    }
    /// The queue's reason for enqueueing when a re-verify round gives up.
    /// `Go` maps to `BoxOccupied` only because the type demands a value; no
    /// caller enqueues on `Go` (see `deliver_now`, which returns early).
    pub fn enqueue_reason(self) -> queue::EnqueueReason {
        match self {
            WriteAdmission::HoldQuestion => queue::EnqueueReason::Question,
            WriteAdmission::Go | WriteAdmission::HoldBoxOccupied => queue::EnqueueReason::BoxOccupied,
        }
    }
}

#[doc(hidden)] // pub for integration tests
pub fn write_admission(box_pending: bool, question_active: bool) -> WriteAdmission {
    if box_pending {
        return WriteAdmission::HoldBoxOccupied;
    }
    if question_active {
        return WriteAdmission::HoldQuestion;
    }
    WriteAdmission::Go
}

/// The pre-Enter write gate as its own STEP (#532, extracted for rev-12 B1) —
/// the last reading `deliver_now` takes before pressing Enter, against the live
/// pane.
///
/// Extracted for exactly the reason `flush_stranded_text`'s doc gives for the
/// same shape: inline at the call site, this — the single most safety-critical
/// line the PR adds — could be deleted with the whole suite still green,
/// because nothing can construct `deliver_now` (it needs a concrete `Wry`
/// `AppHandle`, unavailable headless). As a named function it is drivable
/// against a real fake-child-backed `PtyManager`, and `deliver_now` has nothing
/// left to get wrong beyond calling it.
///
/// Only the box is consulted. The question gate is a *blocking hold*
/// (`wait_for_question_clear`) that runs immediately before this and has
/// already resolved by the time we get here; re-reading it would either be
/// redundant or would re-open a hold this delivery already paid for. So this
/// answers the one question no other pre-Enter check asks: is there human-typed
/// content in the box *right now*, which this Enter would submit.
///
/// `unwrap_or(false)` for a closed pty is deliberate, and differs from
/// `flush_stranded_text`'s `unwrap_or(true)` for a stated reason rather than by
/// oversight: there, the decision is a blind Enter with no downstream net, so an
/// unreadable pane must decline; here the write immediately below fails on its
/// own and audits `prompt-failed`, which is both the pre-existing behaviour and
/// the more informative one. A closed pane has no box and nobody typing into
/// it, so there is nothing for this guard to protect either way.
#[doc(hidden)] // pub for integration tests
pub fn preenter_admission(ptys: &crate::pty::PtyManager, pty_id: u32) -> WriteAdmission {
    write_admission(ptys.input_pending(pty_id).unwrap_or(false), false)
}

/// #532: how many times `deliver_now`'s pre-paste checkpoints may re-verify
/// each other before giving up and letting the queue hold the delivery.
///
/// Small on purpose. Each round already contains the full, individually
/// capped holds (`HUMAN_INPUT_HOLD_MAX` + `QUESTION_HOLD_MAX`), so the rounds
/// bound *re-arming*, not waiting — and re-arming more than a couple of times
/// means the two gates are genuinely alternating, which is a pane a human is
/// actively working in. Giving up there is not a loss: the entry stays at the
/// front of its queue and the drainer retries it with no cap, which is both
/// the pre-existing recovery and the correct one. Raising this would make
/// loomux hold the delivery mutex longer against a busy human for no extra
/// chance of success.
const PREPASTE_RECHECK_ROUNDS: u32 = 3;

/// #532: how long the interactive-question guard may keep holding ONE pane's
/// delivery before loomux stops re-arming that hold silently and badges the
/// pane for a human ([`StrandedBlocker::QuestionStale`]).
///
/// The hold this bounds is per-pane and aggregate, not per-attempt. A single
/// `wait_for_question_clear` is already capped at `QUESTION_HOLD_MAX` (two
/// minutes) — but capping out only aborts *that* attempt, leaving the entry at
/// the front of the queue for `run_queue_drainer` to retry with no cap, which
/// re-arms the same hold every `QUEUE_DRAIN_POLL` forever. That is the same
/// unbounded-latch shape #518 found in the human-input block, one guard over:
/// the per-attempt cap was never the bound, because nothing bounds the
/// attempts.
///
/// Ten minutes, and **what that is and is not sized against** (rev-12 NB2).
/// It is comfortably shorter than `QUEUE_STILL_QUEUED_NOTICE_AFTER` (30 min),
/// so the specific diagnosis lands before the generic "still queued" one. It is
/// NOT, as this doc previously claimed, longer than `deliver_prompt`'s
/// worst-case hold chain: `PREPASTE_RECHECK_ROUNDS` re-arms the pre-paste pair
/// up to three times, so that chain is now `3 x (HUMAN_INPUT_HOLD_MAX +
/// QUESTION_HOLD_MAX)` plus the pre-Enter waits — roughly 13 minutes, past this
/// bound rather than inside it.
///
/// The bound cannot fire mid-delivery anyway, but for a *structural* reason
/// rather than an arithmetic one, and the difference matters to anyone editing
/// either constant: the drainer thread that evaluates this bound is the same
/// thread that blocks inside `deliver_now`, so while a delivery is holding,
/// nothing is polling. The consequence to know is on the other side — time to
/// badge is measured from the pane's hold-episode start ([`HoldEpisode`], #560;
/// `enqueued_ms` before that) but only *sampled* between attempts, so a pane
/// that keeps entering `deliver_now` can take up to roughly 19 minutes
/// to badge, not 10. Sizing this constant against the hold chain would be
/// reasoning about a race that cannot happen; sizing it against how long a
/// human will accept silence is the real constraint.
///
/// Unlike #518's bound this one does **not** release a write — see
/// [`StrandedBlocker::QuestionStale`] for why a byte ring cannot justify one —
/// so it is not a per-group guardrail either. There is no workflow for which a
/// human wants to be told *later* that loomux may be stuck.
pub const QUESTION_HOLD_STALE_AFTER: Duration = Duration::from_secs(10 * 60);

/// #532: has this pane's delivery been held long enough to be escalated to a
/// human? Pure, and deliberately a function of the CLOCK ALONE.
///
/// **rev-12 NB3 — why no signal may veto this.** The first cut took
/// `box_pending` and returned `false` whenever it was set, on the theory that a
/// hold explained by the human's own line needs no report. That was the same
/// mistake this PR exists to fix, one level up. `input_pending` is not a
/// reading of the box; it is `input_box_len > 0`, a running counter that only
/// human writes move and that `classify_human_input` zeroes on **only**
/// `\r`/`\n`, Ctrl-U and Ctrl-C (see `pty.rs`'s `note_user_input`). Every other
/// route to an empty box — a bare `ESC`, a TUI clearing the line in response to
/// a key with no occupancy delta, or the CLI consuming the line itself, which
/// loomux never observes at all — leaves the counter stuck above zero with
/// nothing in the box.
///
/// So the counter has a reachable stuck-true mode, and letting it veto the
/// escalation meant a pane could hold forever *and never tell anyone*: the
/// pre-paste loop holds, the pre-Enter gate declines, the flush declines, and
/// the badge that exists to report exactly that never fires. A staleness check
/// that a stuck flag can silence is not a bound. **An escalation must not be
/// suppressible by any of the signals it exists to report on** — which is why
/// this takes no signal at all, and [`held_escalation`] decides only what to
/// *call* the blocker, never whether to speak.
///
/// `bound_ms == 0` disables the bound (pre-#532 behaviour) rather than making
/// it fire instantly, so a mis-set constant degrades to silence rather than to
/// a badge on every pane.
///
/// `held_since_ms` is the PANE's hold-episode start ([`HoldEpisode`], #560) —
/// the moment this pane last failed to accept a delivery and has not accepted
/// one since — so the clock measures the thing the human experienced (a pane
/// that has not moved), not the lifetime of any one attempt and not the age of
/// whichever entry happens to be at the queue front. It was `front.enqueued_ms`
/// until #560; see [`ends_hold_episode`] for why an entry-scoped clock could be
/// restarted by a `StrandedSubmit` marker on a pane that never recovered.
#[doc(hidden)] // pub for integration tests
pub fn hold_bound_elapsed(held_since_ms: u64, now_ms: u64, bound_ms: u64) -> bool {
    if bound_ms == 0 {
        return false;
    }
    now_ms.saturating_sub(held_since_ms) >= bound_ms
}

/// #903: how long a pane's delivery may be held by the QUESTION gate alone
/// before loomux stops believing its own detector and **pastes** anyway.
///
/// Pasted, and — since #903 B2 — delivered: a granted override carries to the
/// Enter rather than stopping at the paste. The earlier version of this doc said
/// the pre-Enter checkpoint is not overridden and rested the safety argument on
/// that; it is no longer true and the argument now rests elsewhere. What
/// withholds the Enter from a live dialog is [`override_enter_admits`]: fresh
/// re-reads, every one of which must show this pane's own composer holding this
/// delivery's paste. What that does NOT bound is written up in
/// `docs/design/question-gate-authorship.md` rather than left implicit here. See
/// [`question_override_admits`] for the grant itself.
///
/// **Sized between the two clocks that already exist**, which is the whole of
/// the choice: longer than [`QUESTION_HOLD_STALE_AFTER`] (10 min), so the human
/// is badged and given five minutes to look before loomux acts on their behalf;
/// shorter than `QUEUE_STILL_QUEUED_NOTICE_AFTER` (30 min), so a queue moves
/// before the generic "still queued" notice is the first anyone hears of it.
/// #903's own incidents ran 25 and 30+ minutes and ended with panes killed by
/// hand — the bound has to land inside a human's patience, not merely inside
/// infinity.
///
/// **What a wrong override actually costs, in the right order** (rev-427 B2 —
/// the earlier version of this paragraph led with delivery-id dedup, which is
/// the *second* line of defence and does not cover the expensive failure):
///
/// 1. **The Enter is still gated.** An override skips the PRE-PASTE question
///    checkpoint only. `deliver_now`'s pre-Enter `wait_for_question_clear` runs
///    unskipped, against a screen masked with this delivery's own paste, and it
///    is what withholds the Enter from a live dialog. That matters because the
///    unrecoverable harm here is not a stray paste — it is an Enter *selecting*
///    a highlighted option, which no dedup rule can undo.
/// 2. **Then dedup covers the rest.** A paste that lands in an open dialog's lap
///    is recoverable: the human answers the dialog, and if the paste was eaten
///    the drainer re-sends under the same delivery id, which every receiver
///    treats as a duplicate and drops.
///
/// The cost of never overriding is what this issue is: a queue that never moves
/// and a pane a human has to kill. The design note argues both sides at length,
/// including the residual this leaves — see `idle_prompt` on [`QuestionReading`]
/// for why the term below is the WEAK idleness reading and why the strong one
/// would make this function dead code.
///
/// `0` disables the override (pre-#903 behaviour), the same convention
/// [`hold_bound_elapsed`] gives every bound here, so a mis-set constant degrades
/// to today's holding rather than to delivering into every dialog on screen.
pub const QUESTION_HOLD_OVERRIDE_AFTER: Duration = Duration::from_secs(15 * 60);

/// #903: how many CONSECUTIVE drainer polls must read an idle prompt before the
/// override is allowed to fire.
///
/// The same reasoning as [`QUESTION_RELEASE_CONSECUTIVE_CLEAR_POLLS`], and the
/// same number: one reading of a composed screen can catch a mid-redraw
/// instant, and this reading licenses a WRITE. Two polls is four seconds
/// against a bound measured in quarter-hours, so it costs nothing that matters.
#[doc(hidden)] // pub for integration tests
pub const QUESTION_OVERRIDE_CONSECUTIVE_READS: u32 = 2;

/// #903: may this poll deliver despite the question gate saying no?
///
/// Pure, and every term is load-bearing:
///
/// - **`admission` must be `HoldQuestion`.** A box-occupied hold is never
///   overridden — #510's absolute (human-typed content in the box outranks
///   everything) is untouched by this, and `write_admission` checks the box
///   first precisely so that a pane with both blockers reports the box one and
///   lands here ineligible.
/// - **`held_since_ms` is the PANE's hold-episode start** ([`HoldEpisode`]),
///   never an entry's `enqueued_ms` — the same clock [`hold_bound_elapsed`]
///   already measures the badge against, so "badged at 10, overridden at 15"
///   describes one continuous thing a human watched rather than two unrelated
///   timers. `None` — no open episode — is never eligible: nothing has been
///   measured, so nothing can be overdue.
/// - **`idle_streak` is a count of FRESH reads**, taken by the caller on the
///   same polls that produced `admission`. Not a latched flag and not a
///   historical one: the override re-proves the pane is idle every time it
///   fires, which is the property #903 asks for and the reason this takes a
///   streak rather than a bool the caller could have set minutes ago.
///
/// Every term must hold on the SAME poll. A pane that reads idle for ten
/// minutes and then paints a dialog is ineligible on the very next poll, with no
/// memory of having been eligible.
#[doc(hidden)] // pub for integration tests
pub fn question_override_admits(
    admission: WriteAdmission,
    held_since_ms: Option<u64>,
    now_ms: u64,
    bound_ms: u64,
    idle_streak: u32,
) -> bool {
    if admission != WriteAdmission::HoldQuestion {
        return false;
    }
    if idle_streak < QUESTION_OVERRIDE_CONSECUTIVE_READS {
        return false;
    }
    held_since_ms.is_some_and(|since| hold_bound_elapsed(since, now_ms, bound_ms))
}

/// One fresh pre-Enter re-read, reduced to the two bits the decision uses (#903
/// B2).
///
/// A named pair rather than a tuple because both bits are booleans and swapping
/// them silently inverts the safety argument — `active` says the gate is still
/// holding, `idle_prompt` says the pane's own screen shows its composer holding
/// this paste.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuestionReread {
    /// Did the question gate still read as holding on this poll?
    pub active: bool,
    /// Did that same screen show the CLI's composer holding this delivery's
    /// paste — [`idle_prompt_row_rendered`], the WEAK reading, which is the
    /// override's own standard?
    pub idle_prompt: bool,
}

impl QuestionReread {
    /// Does THIS one read permit the Enter?
    ///
    /// `!active` counts, and it is not a widening: a poll where the gate has
    /// simply gone clear is a poll the ordinary checkpoint would have released
    /// on. Without it a screen that repainted between the abort and this re-read
    /// would strand the paste for having got BETTER.
    pub fn admits(&self) -> bool {
        !self.active || self.idle_prompt
    }
}

/// #903 B2: do these fresh pre-Enter re-reads carry a granted override's Enter?
///
/// [`question_override_admits`] decides the PASTE, minutes earlier, on the
/// drainer's poll. This decides the ENTER, here, now — and the split is the whole
/// of what makes the grant worth anything. Skipping only the pre-paste gate left
/// the pre-Enter checkpoint to re-read an unchanged screen, reach the same false
/// positive the grant was issued because of, and abort with the text already in
/// the box.
///
/// Pure, and separated from the pane reading for the reason
/// [`question_hold_predicate_sampled`]'s own doc gives about rev-15 B4: a
/// decision welded to a live `PtyManager` is one no test in this repo can drive,
/// so the rule ends up pinned by nothing. The GRANT itself is not a term here —
/// it is the caller's `question_overridden`, decided by the drainer poll that
/// observed the pane, and re-deriving it at this site would describe a different
/// instant than the one that admitted the write.
///
/// Two terms, both narrowing:
///
/// - **Enough reads.** [`QUESTION_OVERRIDE_CONSECUTIVE_READS`], for its own
///   reason: one reading of a composed screen can catch a mid-redraw instant,
///   and this one licenses an Enter. An empty slice is never enough, so a caller
///   that took no reads at all cannot pass by omission.
/// - **EVERY read admits.** Not a majority and not the last one — a pane that
///   painted a dialog on any of these polls is ineligible, with no memory of
///   having been eligible on the others.
#[doc(hidden)] // pub for integration tests
pub fn override_enter_admits(reads: &[QuestionReread]) -> bool {
    reads.len() as u32 >= QUESTION_OVERRIDE_CONSECUTIVE_READS
        && reads.iter().all(QuestionReread::admits)
}

/// The production half of [`override_enter_admits`]: take the fresh reads this
/// pane owes, then let that function decide.
///
/// Stops early on the first read that does not admit — there is nothing for a
/// second poll to rescue, and the delivery is aborting anyway, so the sleep
/// would be latency spent on a decision already made.
fn preenter_override_admits(
    ptys: &crate::pty::PtyManager,
    pty_id: u32,
    pasted_text: &str,
    delivered: Vec<String>,
) -> bool {
    let mut reads: Vec<QuestionReread> = Vec::new();
    for round in 0..QUESTION_OVERRIDE_CONSECUTIVE_READS {
        if round > 0 {
            std::thread::sleep(QUESTION_HOLD_POLL);
        }
        let r = question_active_witnessed(ptys, pty_id, Some(pasted_text), delivered.clone());
        let read = QuestionReread { active: r.active, idle_prompt: r.idle_prompt };
        reads.push(read);
        if !read.admits() {
            break;
        }
    }
    override_enter_admits(&reads)
}

/// What the drainer should do about a pane it is not yet allowed to write to
/// (#532, extracted for rev-12 B1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldEscalation {
    /// Nothing to do: the hold has already been badged and nothing has changed
    /// (the chip raised earlier in the episode stays up — see `Chip`).
    None,
    /// #563: the pane is held RIGHT NOW and the human should see that right
    /// now. Raise the pane-header delivery-held chip naming this reason.
    ///
    /// This is the outcome that did not exist before #563, and its absence was
    /// the whole bug. `deliver_now` badges its own capped in-attempt waits
    /// (`emit_held`), but the hold a human actually sits in front of lives
    /// BETWEEN attempts, in `run_queue_drainer`'s poll loop — which used to
    /// `continue` on a blocked admission with no UI event of any kind. So the
    /// chip existed only for the seconds inside one attempt, and the pane went
    /// dark for the whole sustained hold, until `Badge` fired
    /// `QUESTION_HOLD_STALE_AFTER` (ten minutes, in practice up to ~19) later.
    ///
    /// Deliberately NOT one-shot in this function. The caller owns idempotence
    /// (it tracks whether the chip is up), for the same reason `Badge`'s
    /// one-shot is an INPUT: a decision that "the pane is held" is a fact
    /// about right now, whereas "have we said so already" is caller state.
    Chip(HeldReason),
    /// Raise the pane's attention badge, naming this blocker. Fires at most
    /// once per hold episode — the caller's one-shot is an INPUT here
    /// (`already_badged`), not a second decision made somewhere else.
    ///
    /// #563: the chip must be up here too. A hold that reaches the bound
    /// without ever having been chipped is reachable — a drainer that starts
    /// on an entry enqueued long ago evaluates its first poll already past the
    /// bound — so the caller raises the chip on this arm as well rather than
    /// assuming a prior `Chip` did.
    Badge(StrandedBlocker),
    /// The pane is writable again: drop whatever this escalation raised — the
    /// chip, the badge, or both.
    ///
    /// #563: this now fires on EVERY writable poll, not only when a badge was
    /// raised. With a chip that can be up without a badge ever having fired,
    /// "we badged, so there is something to clear" stopped being derivable
    /// here, and the alternative — a sixth parameter — would have made this
    /// function's contract depend on two separate pieces of caller state. The
    /// caller's clears are already guarded (`clear_stranded` audits only when
    /// a badge was really up; the chip clear is gated on the caller's own
    /// `chip_reason`), so a `Clear` for a pane with nothing up is a no-op, not
    /// a spurious audit line.
    Clear,
}

/// The one decision point for #532's escalation — pure, so that the whole
/// contract (when it fires, at most once, what it names, and when it comes
/// down) is directly pinnable.
///
/// **Why this is extracted rather than inline (rev-12 B1).** It shipped as an
/// `&&` chain at the drainer's call site, which meant the entire escalation
/// path could be deleted with the whole suite still green — on a PR whose
/// reason for existing is a hold that escalated to nobody. That is precisely
/// the failure `should_flush_before_paste_now`'s own doc argues against ("a
/// named function, not an inline `&&` at the call site, so the combination is
/// independently testable and can't be silently dropped by a future edit"); the
/// doctrine was stated in this file and then not followed one function over.
///
/// Precedence:
/// - Writable (`admission.go()`) ⇒ `Clear`. The pane recovering is what ends
///   the episode, not a timer. (#563: unconditional now — see `Clear`'s doc.)
/// - Inside the bound ⇒ `Chip(reason)` (#563). The escalation is not due, but
///   the pane is held and the human must be able to SEE that from the first
///   poll. This arm returned `None` before #563, and that silence — not a
///   misclassification — is what the issue reports.
/// - Already badged ⇒ `None`. The one-shot lives here so "at most once per
///   episode" is a property of this function rather than of whichever caller
///   remembers to check a set first. The chip stays up; only the badge is
///   one-shot.
/// - Otherwise ⇒ `Badge`, naming the gate that is actually blocking:
///   `HoldQuestion` ⇒ [`StrandedBlocker::QuestionStale`] (loomux may be reading
///   a question that is no longer on screen), `HoldBoxOccupied` ⇒
///   [`StrandedBlocker::HumanInput`].
///
/// That last arm is the NB3 fix in behavioural terms: a long hold is *always*
/// reported, and the only thing the blocked gate decides is which sentence the
/// human reads. Neither arm ever presses Enter.
///
/// **What `HumanInput`'s existing wording does and does not promise (rev-27
/// NB-E).** It reads *"stuck behind text you typed — press Enter or clear the
/// box"*, and reusing it here rather than minting a variant is deliberate — but
/// not because it is unconditionally true. Under the very stuck-true mode
/// [`hold_bound_elapsed`] refuses to trust, `input_pending` can be set over an
/// empty box, and then "text you typed" asserts content that is not there.
/// That is the same unbacked-claim class which justified minting
/// `QuestionStale` instead of reusing `Question`, so it is named rather than
/// glossed.
///
/// What makes reuse right anyway is the *action*, which is the part a human
/// acts on: `classify_human_input` reads `\r` as `Submit` and zeroes the
/// counter outright, so "press Enter" genuinely releases the hold in **both**
/// branches — real leftover text, or a stuck counter over an empty box. That is
/// the same both-branches-safe standard `QuestionStale`'s wording was held to,
/// and it is met; only the diagnosis, not the remedy, can be wrong.
///
/// **The one-shot freezes the wording, not just the count (rev-27 NB-F).** A
/// badge raised as `QuestionStale` keeps its "type a character and delete it"
/// advice even if the human then starts typing and the real blocker becomes
/// `HoldBoxOccupied`. The chip is then offering advice for the other branch.
/// This is the deliberate cost of suppressing re-badges: the pane is genuinely
/// held either way, and neither prescribed action is unsafe on the other's
/// branch (typing-and-deleting leaves the box as it found it; pressing Enter
/// submits a line the human owns and would have submitted anyway). Recorded so
/// the tradeoff is chosen rather than rediscovered.
#[doc(hidden)] // pub for integration tests
pub fn held_escalation(
    admission: WriteAdmission,
    held_since_ms: u64,
    now_ms: u64,
    bound_ms: u64,
    already_badged: bool,
) -> HeldEscalation {
    if admission.go() {
        return HeldEscalation::Clear;
    }
    // #563: held, and inside the bound — the escalation is not due yet, but the
    // VISIBILITY is due immediately. Returning `None` here (the pre-#563
    // behaviour) is precisely the invisible window the issue reports: nothing
    // at all reported the hold until the bound elapsed.
    if !hold_bound_elapsed(held_since_ms, now_ms, bound_ms) {
        return match admission.held_reason() {
            Some(reason) => HeldEscalation::Chip(reason),
            // Unreachable — `admission.go()` returned above, and every other
            // variant has a reason. Named rather than `unreachable!()` for the
            // same reason the `Go` arm below is: this runs on a detached
            // drainer thread, where a panic is a silently dead pane.
            None => HeldEscalation::None,
        };
    }
    if already_badged {
        // The badge is up and stays up; so does the chip the caller raised
        // earlier in this episode. Nothing to change.
        return HeldEscalation::None;
    }
    match admission {
        WriteAdmission::HoldQuestion => HeldEscalation::Badge(StrandedBlocker::QuestionStale),
        WriteAdmission::HoldBoxOccupied => HeldEscalation::Badge(StrandedBlocker::HumanInput),
        // Unreachable — `admission.go()` returned above. Named rather than
        // `unreachable!()`: this runs on a detached drainer thread, where a
        // panic is a silently dead pane (rev-19 N9's finding, same reasoning).
        WriteAdmission::Go => HeldEscalation::None,
    }
}

/// #560: one pane's open hold episode — *this pane has not accepted a delivery
/// since `started_ms`* — and what has already been said about it.
///
/// **The episode, not the entry.** #532 measured its escalation from the front
/// queue entry's `enqueued_ms`, which is a fact about a *payload*, not about the
/// pane. The two come apart the moment the queue front changes under a pane that
/// never recovered, which is exactly what `enqueue_stranded_front` does after an
/// `AbortedPreEnter`: it pops the batch that was pasted and pushes a
/// `StrandedSubmit` marker carrying a fresh `now_ms()`, handing the bound a
/// brand-new clock for a pane that has been blocked continuously. Keying on the
/// pane instead makes "held since T" mean what both the badge's wording and the
/// one-shot already assumed it meant.
///
/// **In memory, and what a restart does.** This is not queue state and owes no
/// `persist_queues` call (#468) — the same argument its `question_stale_notified`
/// predecessor made. A restart therefore forgets any mid-flight episode: the
/// process that would have badged is gone, `queue.json` recovery re-admits the
/// payloads (#467), and the fresh drainer opens a new episode on its first
/// failed observation, so the ten-minute clock restarts from the restart. That
/// is the right direction to be wrong in — persisting it would badge instantly
/// on boot for a hold a human may well have resolved while loomux was down,
/// which is a false claim on the badge whose whole job is to be trustworthy —
/// and the loss is bounded by the restart already announcing itself through
/// recovery notices and `queue_orphans`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HoldEpisode {
    /// When this pane's current hold episode began — the drainer's first
    /// observation that the pane did not accept a delivery.
    started_ms: u64,
    /// Whether `delivery-held-in-queue` has been written for THIS episode. One
    /// line per episode, not per poll: the record has to show that loomux held
    /// and said so without becoming an entry every two seconds.
    ///
    /// #560: this replaces keying that one-shot on the drainer's pane-header
    /// chip being down. The chip is LIVE state (it follows the gate on every
    /// poll and comes down the instant the pane reads writable), so keying an
    /// episode-scoped audit line on it churned for the same reason the badge
    /// did — a flickering pane re-raised the chip and re-wrote the line.
    announced: bool,
    /// Whether the ten-minute escalation badge ([`StrandedBlocker::QuestionStale`]
    /// / [`StrandedBlocker::HumanInput`]) has fired for THIS episode — the
    /// `already_badged` input [`held_escalation`] takes.
    ///
    /// **Stated limit.** This says *we raised it*, not *it is still up*: another
    /// mechanism's `clear_stranded` (the late monitor's `Resolved` arm, or
    /// `attention_tick` pruning a dead agent) can take the badge down while this
    /// stays `true`, and the escalation will not re-raise until the episode
    /// ends. Not repaired here on purpose — a re-raise loop would fight whatever
    /// just cleared it, and the one realistic clearer (`Resolved`) means the
    /// delivery resolved, which produces the `Delivered` observation that ends
    /// the episode anyway. Pre-#560 the same latch existed for as long as no
    /// writable poll intervened; what changed is that a writable poll no longer
    /// resets it.
    badged: bool,
    /// #590 L2: whether `notice-undeliverable` has been reported for THIS
    /// episode ([`OrchRegistry::note_undeliverable_notice`]).
    ///
    /// **Its own flag rather than a read of `badged`**, which is the whole
    /// reason it exists as a field, and the two come apart in BOTH directions:
    ///
    /// - a badge RAISE sets `badged`, after which `held_escalation` returns
    ///   `None` for the rest of the episode (`if already_badged`), so sharing
    ///   the flag would mean never reporting after the instant the bound was
    ///   crossed;
    /// - a badge raise DECLINED — another mechanism already owns the badge —
    ///   leaves `badged` false while `Badge` comes back on every poll, so
    ///   sharing it would mean re-reporting every couple of seconds, on the
    ///   pane with the most wrong with it.
    ///
    /// **Set by the poll that REPORTS, never by one that merely looked**
    /// (rev-128's blocking finding). Claiming it at the top of
    /// [`OrchRegistry::note_undeliverable_notice`] spent an episode's only
    /// report on a poll that found nothing queued, which silenced the case
    /// where a notice joins a pane that is ALREADY held — one step from #590's
    /// own incident, and unbounded, since an episode ends only when the pane
    /// accepts a delivery.
    ///
    /// Same lifetime as the rest of the episode: cleared when the pane accepts
    /// a delivery, and forgotten across a restart, so the worst a restart costs
    /// is one repeated diagnosis of a pane that is still stuck.
    notice_reported: bool,
}

/// #560: what one drainer iteration observed about a pane, as a closed set, and
/// the single place that says which observations END its hold episode.
///
/// **Why a type rather than four `if`s at the call sites.** The rule this
/// encodes — *a momentary writable reading is not the end of an episode; a
/// delivery landing is* — is the entire fix for #560's first symptom, and the
/// pre-#560 code expressed the opposite rule implicitly, by clearing the badge
/// one-shot inside the drainer's `HeldEscalation::Clear` arm. Nothing named it,
/// so nothing could test it and nothing failed when it was wrong. Matching
/// exhaustively in [`ends_hold_episode`] means a future observation cannot be
/// added without deciding, in writing and in one place, what it does to the
/// clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldObservation {
    /// A poll found the pane not writable (`write_admission` held).
    HeldPoll,
    /// A poll found the pane writable. **Provisional**, not proof: the drainer
    /// goes straight on to `deliver_now`, which re-reads both gates at the
    /// paste and at the Enter and can abort on either — so "writable at the
    /// instant of one poll" is precisely the reading a flickering pane produces
    /// between two aborted attempts.
    WritablePoll,
    /// An attempt ran and did not deliver (`AbortedPrePaste`/`AbortedPreEnter`).
    /// OPENS an episode if none is open: a hold that lives entirely inside
    /// `deliver_now` (every poll reads writable, every attempt then aborts)
    /// would otherwise never start a clock at all, and that pane is exactly as
    /// stuck as one that fails at the poll.
    Aborted,
    /// A delivery LANDED (`DeliverOutcome::Done`) — the pane accepted a write.
    Delivered,
    /// #813: loomux gave up on a `StrandedSubmit` repair it could not safely
    /// perform (`DeliverOutcome::Retired`). Ends the episode exactly as
    /// `Delivered` does — loomux is no longer holding this pane on that entry,
    /// and the next entry clocks and badges afresh — but says so without
    /// claiming the pane accepted a write.
    Retired,
}

/// #560: does this observation end the pane's hold episode?
///
/// Only evidence that the pane actually **accepted** a delivery does. The
/// alternative — ending an episode on a writable reading — is what #532
/// effectively did, and it fails in both directions at once:
///
/// - *Churn.* A badged pane that reads writable for one poll and then aborts
///   drops and re-raises its badge (`stranded-attention` / `stranded-cleared`
///   per flicker), which is the audit flood the one-shot exists to prevent.
/// - *Suppression, which is worse.* If the clock restarted on every writable
///   poll, a pane whose occupancy toggles faster than the bound — a human
///   typing with a delivery queued behind them, #560's own reported scenario —
///   would never badge **at all**. That re-creates the #532 bug class
///   ("an escalation that never fires") and violates the rule
///   [`hold_bound_elapsed`]'s doc states outright: an escalation must not be
///   suppressible by the signals it exists to report on.
///
/// The queue emptying ends an episode too, but not through here: `commit_exit`
/// drops the record in the same generation-guarded step that deregisters the
/// drainer, because that removal has to be atomic with the queue check.
pub fn ends_hold_episode(observation: HoldObservation) -> bool {
    match observation {
        // #813: a retired marker ends the episode for the same reason a
        // delivery does — whatever loomux was holding this pane for is over.
        HoldObservation::Delivered | HoldObservation::Retired => true,
        HoldObservation::HeldPoll | HoldObservation::WritablePoll | HoldObservation::Aborted => false,
    }
}

/// #560: does this observation OPEN a hold episode (start the clock) if none is
/// open yet? Both of the drainer's ways of failing to deliver do; a writable
/// poll neither opens nor closes one.
pub fn opens_hold_episode(observation: HoldObservation) -> bool {
    match observation {
        HoldObservation::HeldPoll | HoldObservation::Aborted => true,
        HoldObservation::WritablePoll
        | HoldObservation::Delivered
        | HoldObservation::Retired => false,
    }
}

/// Why a prompt delivery is currently being held for human input (#246): the
/// UI-facing counterpart to the audit-log holds above. `Typing` is
/// `wait_for_user_quiet`'s "human is actively typing" hold (#43); `BoxOccupied`
/// is `wait_for_box_clear`'s "an unsubmitted line sits in the box" hold (#111,
/// backed by `box_occupancy_delta`/#171). Two reasons because they read
/// differently to a human watching the pane — one is "wait, I'm still typing",
/// the other is "wait, I left something in the box" — even though both boil
/// down to "loomux won't paste over you".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldReason {
    Typing,
    BoxOccupied,
    /// A question/permission TUI is on screen (#420): the pane's own agent is
    /// mid-dialog (Copilot's numbered/radio-select question, a y/n permission
    /// prompt, Claude's `AskUserQuestion`), and a programmatic paste+Enter would
    /// land ON that dialog — worse than the other two reasons, because Enter
    /// there doesn't just merge text, it SELECTS an option (the highlighted
    /// default, usually the first) and silently steers the agent. See
    /// `prompt_wait_detected` for the detector and `confirm_copilot_autopilot_dialog`
    /// for why the autopilot-consent dialog specifically is exempt from this hold.
    InteractiveQuestion,
}

impl HeldReason {
    pub fn as_str(self) -> &'static str {
        match self {
            HeldReason::Typing => "typing",
            HeldReason::BoxOccupied => "box-occupied",
            HeldReason::InteractiveQuestion => "question",
        }
    }
}

/// #563: every way loomux can withhold a delivery, as a closed set.
///
/// **Why this exists as a type rather than a paragraph.** #563 is an
/// *invisible* hold: a delivery held against an empty-looking pane, with no
/// warning, until the queue filled and work was dropped. The reason it could
/// happen is that "which holds are visible" was never written down anywhere a
/// compiler or a test could check — the pane-header chip covered the holds
/// inside one delivery attempt (`deliver_now`) and simply did not exist for
/// the hold BETWEEN attempts (`run_queue_drainer`'s poll loop), and nothing
/// anywhere said so. A prose table would have rotted the same way; this one
/// fails the build.
///
/// The enforcement is two-part and neither half is decorative:
/// - [`hold_channels`] matches exhaustively, so a new hold classification
///   cannot be added without declaring how a human learns about it;
/// - `every_hold_class_reaches_a_human` (integration tests) asserts every
///   classification has at least one channel that
///   [`HoldChannel::survives_orchestrator_target`] — because the in-band
///   notice channel is suppressed on exactly the pane #563 was reported on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldClass {
    /// #43, pre-paste: the human is actively typing in the pane.
    PrePasteTyping,
    /// #111/#510, pre-paste: a human line is sitting unsubmitted in the box.
    PrePasteBoxOccupied,
    /// #420, pre-paste: a question/permission TUI owns the Enter key.
    PrePasteQuestion,
    /// #532, pre-paste: the two gates kept re-arming for
    /// `PREPASTE_RECHECK_ROUNDS`, so the attempt gave up without pasting.
    PrePasteRecheckExhausted,
    /// #43, pre-Enter: the human started typing during the paste settle.
    PreEnterTyping,
    /// #420, pre-Enter: a dialog appeared while our paste was settling.
    PreEnterQuestion,
    /// #532, pre-Enter: human-typed content is in the box at submit time.
    PreEnterBoxOccupied,
    /// #563: the drainer's poll found the box occupied BETWEEN attempts. This
    /// is where a sustained hold actually lives, and it is the classification
    /// that had no channel at all before #563.
    QueuePollBoxOccupied,
    /// #563: as above, blocked on the interactive-question gate.
    QueuePollQuestion,
    /// #532: a poll hold that has outlived `QUESTION_HOLD_STALE_AFTER`.
    QueueStaleEscalation,
    /// #563: the pane's queue has reached `queue::QUEUE_NEAR_FULL_AT` —
    /// nothing lost yet, and the warning exists to keep it that way.
    QueueNearFull,
    /// #445/#563: the queue is at `queue::QUEUE_MAX_PER_PANE`; further
    /// arrivals are rejected outright.
    QueueFull,
    /// The human paused the group, so loomux delivers nothing (`deliver_prompt`).
    GroupPaused,
    /// #590 L2: a hold that has outlived `QUESTION_HOLD_STALE_AFTER` **on a
    /// pane whose queue holds one of loomux's own `[orrerix]` notices**
    /// (`queue::is_loomux_notice`).
    ///
    /// A strict subset of [`HoldClass::QueueStaleEscalation`]'s panes, split
    /// out because the two are not the same event to a reader. A stale hold on
    /// a kickoff is work waiting; a stale hold on a NOTICE means an agent is
    /// being kept from something loomux decided it needed to know — and in
    /// #590's incident that agent was blocked *on the very condition the
    /// undelivered notice reported*, so the pane could not clear itself and no
    /// channel reached anyone who could. Hence the extra two channels: this is
    /// the only hold classification whose harm lands on an AGENT rather than
    /// on a human's attention.
    UndeliverableNotice,
}

impl HoldClass {
    /// Every classification. Maintained against [`HoldClass::ordinal`]'s
    /// exhaustive match and the length assertion in
    /// `every_hold_class_reaches_a_human`.
    pub const ALL: &'static [HoldClass] = &[
        HoldClass::PrePasteTyping,
        HoldClass::PrePasteBoxOccupied,
        HoldClass::PrePasteQuestion,
        HoldClass::PrePasteRecheckExhausted,
        HoldClass::PreEnterTyping,
        HoldClass::PreEnterQuestion,
        HoldClass::PreEnterBoxOccupied,
        HoldClass::QueuePollBoxOccupied,
        HoldClass::QueuePollQuestion,
        HoldClass::QueueStaleEscalation,
        HoldClass::QueueNearFull,
        HoldClass::QueueFull,
        HoldClass::GroupPaused,
        HoldClass::UndeliverableNotice,
    ];

    /// Dense index into [`HoldClass::ALL`]. Exhaustive on purpose: a new
    /// variant cannot compile without being given an index, and the test then
    /// fails until `ALL` lists it — so the completeness of `ALL` is enforced
    /// rather than trusted.
    pub fn ordinal(self) -> usize {
        match self {
            HoldClass::PrePasteTyping => 0,
            HoldClass::PrePasteBoxOccupied => 1,
            HoldClass::PrePasteQuestion => 2,
            HoldClass::PrePasteRecheckExhausted => 3,
            HoldClass::PreEnterTyping => 4,
            HoldClass::PreEnterQuestion => 5,
            HoldClass::PreEnterBoxOccupied => 6,
            HoldClass::QueuePollBoxOccupied => 7,
            HoldClass::QueuePollQuestion => 8,
            HoldClass::QueueStaleEscalation => 9,
            HoldClass::QueueNearFull => 10,
            HoldClass::QueueFull => 11,
            HoldClass::GroupPaused => 12,
            HoldClass::UndeliverableNotice => 13,
        }
    }
}

/// #563: how a human can learn that a delivery is being withheld.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldChannel {
    /// The pane-header `⏸ held` chip (`orch-delivery-held`). A UI event keyed
    /// on `pty_id` alone — no role or pause suppression anywhere in its path
    /// (`src/orchestration.ts`'s listener, `heldbadge.ts`'s mapping).
    HeldChip,
    /// The pane's attention badge (`mark_stranded` → `stranded_detail`). Also
    /// a UI channel, also unsuppressed by role.
    AttentionBadge,
    /// An in-band `[orrerix] …` notice delivered to the group's orchestrator
    /// (`notify_queue`).
    ///
    /// **Never sufficient on its own.** It is suppressed outright when the
    /// held pane IS the orchestrator's (a notice about the orchestrator's
    /// blocked pane would queue behind the very block it reports) and again
    /// while the group is paused. #563 was reported on an orchestrator pane;
    /// classifications whose only channel was this one were, on that pane,
    /// silent.
    OrchestratorNotice,
    /// #578: the notice [`HoldChannel::OrchestratorNotice`] could NOT deliver
    /// to an orchestrator target, parked by `notify_queue` and handed back on
    /// that orchestrator's next MCP tool result
    /// (`OrchRegistry::take_orchestrator_notices`).
    ///
    /// **Why this one survives an orchestrator target.** It is not a delivery.
    /// It consumes no slot in the target pane's queue and types nothing into
    /// the pane, so it cannot queue behind the very block it reports — the
    /// loop that makes the in-band notice structurally undeliverable here.
    /// It rides back on a call the orchestrator itself made, which is also the
    /// proof that the orchestrator is running and reading at that instant.
    ///
    /// **And so it needs no cap exemption**, which is the sharpest way to see
    /// the difference from the other orchestrator-facing notice on this list.
    /// #615's pause-loss notice IS a delivery, so on a pane at
    /// `queue::QUEUE_MAX_PER_PANE` — exactly the pane it has the most to report
    /// about — it was certain to be destroyed by the same cap it was reporting,
    /// and needed `queue::EnqueueReason::PauseLossNotice`'s one entry of
    /// headroom to survive. This channel never meets that problem: a full queue
    /// is the CONDITION it reports, not an obstacle to reporting it.
    ///
    /// **A pull, not a push, and listed ALONGSIDE the badge rather than
    /// instead of it.** An orchestrator that never calls another tool never
    /// reads its inbox: this channel reaches the orchestrator *agent* on its
    /// next turn, while the badge/chip reach the *human* regardless. The two
    /// cover different failures (a wedged human-facing UI vs. an unattended
    /// overnight run with nobody at the window) and neither subsumes the
    /// other.
    OrchestratorInbox,
    /// A synchronous `Err` returned to the agent that called the MCP tool
    /// (`queue::queue_full_error`). Reaches the *sender*, never the human and
    /// never the held pane's own agent.
    CallerError,
    /// The group's paused state, which the human set and the group UI shows.
    /// The one classification a human cannot be surprised by.
    PausedGroupUi,
}

impl HoldChannel {
    /// Whether this channel still reaches a human when the held pane IS the
    /// group's own orchestrator — the #563 case, and the property the
    /// completeness test asserts.
    ///
    /// Matched exhaustively rather than written as a `!matches!` negation
    /// (#578): a channel added later must state its own answer here instead
    /// of defaulting to "survives", which is the optimistic half of the
    /// answer and the one that would make the completeness test pass by
    /// accident.
    pub fn survives_orchestrator_target(self) -> bool {
        match self {
            HoldChannel::OrchestratorNotice => false,
            HoldChannel::HeldChip
            | HoldChannel::AttentionBadge
            | HoldChannel::OrchestratorInbox
            | HoldChannel::CallerError
            | HoldChannel::PausedGroupUi => true,
        }
    }
}

/// #563: the channels each hold classification actually fires. Exhaustive, so
/// a hold added later cannot be silent by omission.
///
/// This is a description of what the code does, not an aspiration — every arm
/// is backed by a call site, and the arms that changed in #563
/// (`QueuePoll*`, `QueueNearFull`, `QueueFull`) changed because the call sites
/// did.
///
/// **#578 and the [`HoldChannel::OrchestratorInbox`] arms.** The inbox is a
/// property of `notify_queue` specifically — it is that function's suppressed
/// branch, parked instead of discarded — so it is listed on exactly the
/// classifications whose notice goes through `notify_queue`, and nowhere else.
/// [`HoldClass::GroupPaused`] is the one that looks like it should have it and
/// must not: its `OrchestratorNotice` is `announce_pause_suppression`, a
/// different call on a different path that `notify_queue` never sees — and
/// since #615 an actual in-band DELIVERY, admitted past the cap with
/// `queue::EnqueueReason::PauseLossNotice`. Listing an inbox it never parks
/// into would make this table say something untrue.
pub fn hold_channels(class: HoldClass) -> &'static [HoldChannel] {
    match class {
        // `deliver_now`'s in-attempt holds all badge via `emit_held`.
        HoldClass::PrePasteTyping
        | HoldClass::PrePasteBoxOccupied
        | HoldClass::PrePasteQuestion
        | HoldClass::PreEnterTyping
        | HoldClass::PreEnterQuestion => &[HoldChannel::HeldChip],
        // These two abort the attempt without ever having badged (the recheck
        // loop's gates alternated; the pre-Enter box gate is a single reading,
        // not a hold). Neither leaves the pane unattended: the entry stays at
        // the front of the queue and the drainer's poll — which now chips
        // immediately — is the next thing to look at it.
        HoldClass::PrePasteRecheckExhausted | HoldClass::PreEnterBoxOccupied => {
            &[HoldChannel::HeldChip, HoldChannel::OrchestratorNotice, HoldChannel::OrchestratorInbox]
        }
        // #563's fix: the poll hold chips from the first blocked tick and
        // escalates to a badge at `QUESTION_HOLD_STALE_AFTER`.
        HoldClass::QueuePollBoxOccupied | HoldClass::QueuePollQuestion => {
            &[HoldChannel::HeldChip, HoldChannel::AttentionBadge]
        }
        HoldClass::QueueStaleEscalation => &[HoldChannel::AttentionBadge, HoldChannel::HeldChip],
        // #563's fix: a badge (orchestrator-safe) alongside the notice.
        // #578 adds the inbox: the badge tells a human who is looking, the
        // inbox tells the orchestrator agent on its next turn — which is the
        // only one of the two that fires on an unattended overnight run.
        HoldClass::QueueNearFull => &[
            HoldChannel::AttentionBadge,
            HoldChannel::OrchestratorNotice,
            HoldChannel::OrchestratorInbox,
        ],
        HoldClass::QueueFull => &[
            HoldChannel::AttentionBadge,
            HoldChannel::OrchestratorNotice,
            HoldChannel::OrchestratorInbox,
            HoldChannel::CallerError,
        ],
        // #569, and re-stated for option 2 (enqueue-while-paused), which
        // changed what this hold DOES: the payload is now held in the pane's
        // durable queue and flushed on resume, so a pause is a delay, no
        // longer the one hold that destroys what it withholds.
        //
        //  - `PausedGroupUi` is the hold itself: the human set the pause, the
        //    group UI shows it for as long as it lasts, and the deliveries
        //    behind it are safe. The one classification a human cannot be
        //    surprised by.
        //  - `OrchestratorNotice` / `AttentionBadge` are
        //    `announce_pause_suppression` and its no-live-orchestrator
        //    fallback (`StrandedBlocker::PauseSuppressed`), and they are NOT
        //    vestigial (review B2). Besides the legacy discard, they carry the
        //    one loss a pause on this build can still cause: a pane at
        //    `queue::QUEUE_MAX_PER_PANE` refuses admissions for as long as the
        //    pause lasts, and the refused sender's `Err` reaches the sender,
        //    never the human. That is precisely what this table exists to
        //    catch, so all three arms are live.
        //
        // The resume flush itself is not a channel: it is a delivery to the
        // agent that was waiting, not a way for a HUMAN to learn about a hold,
        // which is the only question this table answers.
        HoldClass::GroupPaused => &[
            HoldChannel::PausedGroupUi,
            HoldChannel::OrchestratorNotice,
            HoldChannel::AttentionBadge,
        ],
        // #590 L2. A separate classification from `QueueStaleEscalation`
        // rather than two more channels bolted onto it, because the two say
        // different things and only one of them is unconditionally true.
        // `QueueStaleEscalation` fires for ANY pane held past the bound and
        // reaches the human only; this one fires for the subset where the held
        // payload is loomux's OWN notice, and reaches the orchestrator agent
        // too. Folding the extra channels into that row would make the table
        // claim an orchestrator-facing channel for a held kickoff, which
        // nothing sends.
        HoldClass::UndeliverableNotice => &[
            HoldChannel::AttentionBadge,
            HoldChannel::HeldChip,
            HoldChannel::OrchestratorNotice,
            HoldChannel::OrchestratorInbox,
        ],
    }
}

/// #590 L2: what a held pane's own readings say about WHO is holding it, at
/// the moment loomux concludes that its own notice cannot be delivered there.
///
/// **Why the diagnosis is worth a type.** The channel this feeds is read by
/// the orchestrator *agent*, and the two hold reasons the drainer can report
/// (`box-occupied`, `question`) do not distinguish the cases that need
/// opposite responses: a human's half-written line (wait — a person is right
/// there) from a CLI's own turn state (do not wait — the pane will not clear
/// until the agent's turn ends, and the notice sitting undelivered may be the
/// very thing that would end it). #590's incident is the second one and read
/// on the wire exactly like the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndeliverableCause {
    /// The box is occupied and **no human keystroke has landed in this pane
    /// since the hold episode opened** (`PtyManager::last_user_input_ms`, which
    /// #496 gates on `classify_human_input` so a terminal's own query replies
    /// never stamp it). So whatever occupies the box is not a person's — it is
    /// the pane's own CLI, mid-turn.
    ///
    /// **What this claim is, exactly.** It is *negative* evidence — "not a
    /// human" — plus the box reading, and that is the actionable half. It is
    /// not proof that a turn is running: `input_pending` is documented to latch
    /// over an already-empty box (a bare ESC, a TUI line-clear, a CLI consuming
    /// the line), so a quiescent pane with a latched counter reads the same. The
    /// wording therefore states the evidence it has rather than the inference
    /// alone, and every response it invites (look at the pane, do not wait for
    /// it) is correct under both readings.
    PaneMidTurn,
    /// The box is occupied and a human HAS typed in this pane since the episode
    /// opened. #510's absolute is doing exactly what it exists to do; the badge
    /// and the chip are the right channels and the orchestrator should not
    /// treat this as a stall.
    HumanTyping,
    /// #420: a question/permission dialog owns the pane's Enter key. Checked
    /// BEFORE the keystroke evidence because it is a direct observation of the
    /// pane rather than an inference from its absence — and because it covers
    /// that evidence's documented blind spot: a human navigating a dialog with
    /// arrow keys classifies `Neutral` with a zero occupancy delta and never
    /// stamps `last_user_input_ms` (#496's stated tradeoff), so a
    /// keystroke-first order would report a human mid-menu as a pane mid-turn.
    QuestionOnScreen,
    /// No reading to classify from — a closed pty, or a hold with no gate
    /// attributed to it. Named rather than folded into one of the others so a
    /// notice never asserts a cause it did not observe.
    Unknown,
}

impl UndeliverableCause {
    /// The audit-line token.
    pub fn as_str(self) -> &'static str {
        match self {
            UndeliverableCause::PaneMidTurn => "pane-mid-turn",
            UndeliverableCause::HumanTyping => "human-typing",
            UndeliverableCause::QuestionOnScreen => "question-on-screen",
            UndeliverableCause::Unknown => "unknown",
        }
    }

    /// The clause [`undeliverable_notice`] renders, which is where the
    /// diagnosis becomes an instruction. Each one names the evidence behind it,
    /// so a reader can check the claim instead of trusting it.
    fn phrase(self) -> &'static str {
        match self {
            UndeliverableCause::PaneMidTurn => {
                "pane mid-turn (no human keystroke since the hold began), so nothing but that \
                 agent's own turn ending will clear it — read the pane, do not wait on it"
            }
            UndeliverableCause::HumanTyping => {
                "a human has typed in this pane since the hold began, so it is waiting behind \
                 their line and will clear when they submit it"
            }
            UndeliverableCause::QuestionOnScreen => {
                "a question/permission dialog owns the pane and needs an answer before anything \
                 can be delivered into it"
            }
            UndeliverableCause::Unknown => {
                "loomux could not read the pane's input state, so it cannot say what is holding it"
            }
        }
    }
}

/// #590 L2: classify a held pane at the escalation bound.
///
/// `held` is the blocking gate the drainer's own `write_admission` reported
/// (`WriteAdmission::held_reason`), `last_user_input_ms` the pane's
/// human-keystroke stamp (`None` when the pty is gone), and
/// `episode_started_ms` the hold episode's start ([`HoldEpisode`]).
///
/// **The comparison is against the episode, not against a fresh window**, and
/// that is why this needs no new constant. The question worth answering is not
/// "did a human type recently" — recently against what? — but "has a human
/// touched this pane at any point in the whole time it has been refusing our
/// delivery". A stamp older than the episode is a fact about some earlier
/// session at this pane and says nothing about what is in the box now.
///
/// Pure, so the precedence (dialog beats keystroke evidence beats its absence)
/// is directly pinnable — it is the one ordering a future edit must not swap.
pub fn undeliverable_cause(
    held: Option<HeldReason>,
    last_user_input_ms: Option<u64>,
    episode_started_ms: u64,
) -> UndeliverableCause {
    match held {
        None => UndeliverableCause::Unknown,
        Some(HeldReason::InteractiveQuestion) => UndeliverableCause::QuestionOnScreen,
        Some(HeldReason::Typing) | Some(HeldReason::BoxOccupied) => match last_user_input_ms {
            None => UndeliverableCause::Unknown,
            Some(ms) if ms >= episode_started_ms => UndeliverableCause::HumanTyping,
            Some(_) => UndeliverableCause::PaneMidTurn,
        },
    }
}

/// #590 L2: the orchestrator-facing notice for a pane that has been holding
/// loomux's own notice past the escalation bound.
///
/// **One line, marker-led**, like every other text that can reach
/// [`OrchNoticeInbox::park`] — see that method's `debug_assert`, and #621 for
/// why a row loomux writes must be one `mask_loomux_notices` can claim.
///
/// **What it says that `queue::still_queued_notice` does not**, which is the
/// whole reason it exists as a second notice rather than a reworded first one:
/// that the stuck payload is loomux's OWN notice (so an agent is waiting on
/// something it will never be told), and a cause for the hold. The still-queued
/// notice fires at 30 minutes and reports depth and elapsed time; #590's live
/// incident was diagnosed and cleared by a human at ~20, so that notice never
/// fired at all, and had it fired it would have said "nothing lost, delivers
/// automatically once clear" — true, and precisely the wrong thing to read
/// about a pane that cannot clear itself.
pub fn undeliverable_notice(
    agent_id: &str,
    notices: usize,
    depth: usize,
    minutes: u64,
    cause: UndeliverableCause,
) -> String {
    let subject = if notices == 1 {
        format!("1 of {depth} deliveries queued for {agent_id} is this app's own notice")
    } else {
        format!("{notices} of {depth} deliveries queued for {agent_id} are loomux's own notices")
    };
    format!(
        "{NOTICE_MARKER} notice undeliverable {minutes} min: {subject}, and the pane has \
         accepted nothing since — {}",
        cause.phrase()
    )
}

/// #578: how many parked notices one group's orchestrator inbox holds before
/// the oldest start being elided.
///
/// Sized for a burst, not a backlog. The inbox drains on the orchestrator's
/// very next MCP tool call, so a group that is past this number has an
/// orchestrator that has not called a tool in a long time — at which point the
/// twentieth "your queue is backed up" tells a reader nothing the first one
/// did, and every one of them is in `audit.jsonl` verbatim regardless. Bounded
/// because this map is written by loomux's own delivery paths and read by an
/// agent whose context is the scarce resource: an unbounded relay would be a
/// memory leak at one end and a context bomb at the other.
pub const ORCH_NOTICE_INBOX_MAX: usize = 20;

/// #578: how much of a suppressed notice's text the `notice-suppressed` audit
/// line carries. Matches the `tool-result` line's own cap — the notices are
/// one-liners, and the cap exists so a future caller passing something large
/// cannot bloat the log.
const NOTICE_AUDIT_TEXT_CAP: usize = 500;

/// #578: one group's parked orchestrator-target queue notices — the durable
/// half of [`HoldChannel::OrchestratorInbox`].
#[derive(Default, Debug, Clone)]
pub struct OrchNoticeInbox {
    /// Oldest first, capped at [`ORCH_NOTICE_INBOX_MAX`].
    pub notices: Vec<String>,
    /// How many the cap pushed out. **Counted, not forgotten**: a relay that
    /// silently held back N notices would read as complete while being
    /// short — the exact "looks complete, isn't" defect the whole
    /// #445/#467/#563 lineage exists to eliminate.
    pub elided: usize,
}

impl OrchNoticeInbox {
    /// Park one notice, evicting the oldest (and counting it) past the cap.
    /// Oldest-first eviction on purpose: the newest notice is the one whose
    /// claim is still true — a `dropped_notice` from ten minutes ago has been
    /// superseded by whatever the queue did since.
    ///
    /// **The maskability invariant is checked at the door** (#576/#621, review
    /// NB3). [`orch_notice_relay_text`] can only keep every row maskable if
    /// every notice it is given is itself one marker-led line; a caller passing
    /// a multi-line or unprefixed string would produce rows
    /// [`mask_loomux_notices`] cannot claim, and the failure would surface in
    /// the rendered block, far from the call site that caused it. Asserted
    /// through `mask_loomux_notices` itself rather than by re-deriving the
    /// rule, so the check cannot drift from what it is standing in for.
    /// `debug_assert` because CI's test builds are debug (so it fires where a
    /// mistake is introduced) while a release build never panics a live session
    /// over it — the degraded outcome is a gate that holds too long, which
    /// `QuestionStale` already reports.
    pub fn park(&mut self, text: &str) {
        debug_assert!(
            mask_loomux_notices(text).is_empty(),
            "a parked queue notice must be a single {NOTICE_MARKER}-led line or the relay \
             block stops being maskable (#576/#621) — got {text:?}"
        );
        self.notices.push(text.to_string());
        if self.notices.len() > ORCH_NOTICE_INBOX_MAX {
            let overflow = self.notices.len() - ORCH_NOTICE_INBOX_MAX;
            self.notices.drain(..overflow);
            self.elided += overflow;
        }
    }
}

/// #578: render a group's parked notices as the extra MCP content block an
/// orchestrator's next tool result carries back. `None` when there is nothing
/// to say — the ordinary case, and the one that must add not a single byte to
/// a tool result.
///
/// Pure so the copy is testable without a dispatch harness, the way every
/// other notice string in this codebase is.
///
/// **The wording has one job beyond informing.** It must stop the orchestrator
/// from "helpfully" re-sending anything: the payloads these notices describe
/// are either already queued and delivering (`queued_notice`) or already gone
/// (`dropped_notice`), and a re-send is a duplicate in the first case and a
/// guess in the second. It also says plainly that this is its OWN pane, since
/// every other `[orrerix]` notice an orchestrator reads is about somebody else.
///
/// **Every row stays maskable (#576/#621).** This block rides an MCP tool
/// result, never the pty, so no reader of a live pane sees it directly. But
/// [`mask_loomux_notices`]'s own argument covers the path that puts it in a
/// pane anyway — an agent can print marker text itself — and an orchestrator
/// quoting its relay back into a summary would leave text *about* a question
/// in the tail of the pane most exposed to #576's self-latch. So the header
/// leads with [`NOTICE_MARKER`], every constituent notice already does,
/// and the two rows that would not (the bullet and the elision line) are
/// shaped so `deframe` still finds the marker leading them. A `-` bullet is
/// the specific thing that breaks this, since `deframe` does not strip it.
pub fn orch_notice_relay_text(notices: &[String], elided: usize) -> Option<String> {
    if notices.is_empty() {
        return None;
    }
    let n = notices.len();
    let mut out = format!(
        "[orrerix] {n} queue notice{s} about YOUR OWN pane could not be delivered to you as a \
         prompt — a delivery announcing your pane's blocked delivery would queue behind the very \
         block it reports (#578). Relayed here instead, riding back on a call you just made. \
         Nothing needs acknowledging and nothing needs re-sending:",
        s = if n == 1 { "" } else { "s" }
    );
    for t in notices {
        // `•`, not `-`, and the elision line below carries the marker rather
        // than opening with prose (#576/#621). Every row of this block has to
        // stay maskable by `mask_loomux_notices`, which drops a row that LEADS
        // with `NOTICE_MARKER` once `deframe`d — and `deframe` strips
        // whitespace and `│ ┃ | * ● • ◆`, but NOT `-`. This block never reaches
        // a pane on its own, but #621's own argument covers the path that puts
        // it there: an agent can print marker text itself, and an orchestrator
        // quoting its own relay into a summary would otherwise leave text
        // ABOUT a question sitting in the tail of the pane most exposed to
        // #576's self-latch.
        out.push_str("\n  • ");
        out.push_str(t);
    }
    if elided > 0 {
        out.push_str(&format!(
            "\n  • {NOTICE_MARKER} plus {elided} earlier notice{s} elided (this relay \
             holds {ORCH_NOTICE_INBOX_MAX}) — every one of them is in this group's audit.jsonl \
             as a `notice-suppressed` line.",
            s = if elided == 1 { "" } else { "s" }
        ));
    }
    Some(out)
}

/// Human-readable "what's held and why" line for a delivery-held badge/toast
/// (#246). Pure so the copy is unit-testable; callers pair it with
/// `delivery_held_event`'s payload.
pub fn delivery_held_detail(agent_id: &str, reason: HeldReason) -> String {
    match reason {
        HeldReason::Typing => format!(
            "Prompt delivery to {agent_id} is paused — you're typing in this pane."
        ),
        HeldReason::BoxOccupied => format!(
            "Prompt delivery to {agent_id} is paused — submit or clear the text in this pane's box to continue."
        ),
        HeldReason::InteractiveQuestion => format!(
            "Prompt delivery to {agent_id} is paused — an interactive question is on screen, answer it to release delivery."
        ),
    }
}

/// Build the `orch-delivery-held` event payload (#246): pushed the moment a
/// delivery to `agent_id` starts waiting on human input in `pty_id`'s box.
/// Pure so the shape is unit-testable without a harness that can capture an
/// actually-emitted Tauri event (see `channel_connected_event`'s doc for why
/// this codebase always factors payload construction out this way).
pub fn delivery_held_event(agent_id: &str, group: &GroupId, pty_id: u32, reason: HeldReason) -> Value {
    json!({
        "agent_id": agent_id, "group": group, "pty_id": pty_id,
        "reason": reason.as_str(), "detail": delivery_held_detail(agent_id, reason),
    })
}

/// Build the `orch-delivery-held-cleared` event payload (#246): pushed the
/// instant a hold resolves, whether the prompt then delivered or the delivery
/// aborted — either way nothing is held on `pty_id` anymore, so the frontend
/// badge for it drops.
pub fn delivery_held_cleared_event(pty_id: u32) -> Value {
    json!({ "pty_id": pty_id })
}

/// Whether an unconfirmed delivery should raise a one-shot notice to the group's
/// orchestrator so it can close the loop (#103). Fires only for a delivery to a
/// NON-orchestrator agent whose submit went unconfirmed: the prompt may be
/// sitting unsubmitted in the pane while the orchestrator, believing it landed,
/// is none the wiser. Suppressed when the target IS the orchestrator — a notice
/// about a delivery to the orchestrator would itself be a delivery to the
/// orchestrator, an endless loop; those rely on #99's stranded-text flush on the
/// next delivery instead. Suppressed when confirmed: the prompt landed, nothing
/// to chase. Pure so the gate is testable; emission (exactly once per delivery,
/// past the submit retries) lives in `deliver_prompt`'s delivery thread.
pub fn should_notify_unconfirmed(target_is_orchestrator: bool, confirmed: bool) -> bool {
    !target_is_orchestrator && !confirmed
}

/// The notice delivered to the orchestrator for the unconfirmed deliveries to
/// `agent_id` buffered by one coalescing window (#103, coalesced in #539).
///
/// **Why it names ids.** Pre-#539 this was one notice per delivery and the
/// orchestrator could tell them apart only by arrival order. Coalescing makes
/// that impossible — several alarms arrive as one line — so each constituent
/// is named. `delivery_ids` are `submit_sent_ms` stamps: the identity the
/// delivery ledger and `late_monitor_tick`'s supersession check already key on
/// (no two deliveries to a pane share one), and the only id EVERY delivery
/// has, queued or straight through the front door. The same value is written
/// as `delivery_id` on this pane's `delivery-failed-idle` /
/// `delivery-unconfirmed-*` audit lines, so a reader can resolve any id in
/// this notice back to the record it came from.
///
/// The verb agrees with the count, per #533's `flush_header_text` — every one
/// of these lines is read by an agent as an instruction, and one that doesn't
/// parse is one more reason to skim it. The plural also changes the ASK: with
/// several deliveries in play the recovery is ONE `get_output`, not one per
/// id, which is the whole point of coalescing.
///
/// **The id list is capped** (rev-13 N3) at `UNCONFIRMED_NOTICE_IDS_MAX`, with
/// the remainder pointed at the audit log — which holds every id, since
/// `delivery-unconfirmed-notice` records the whole `delivery_ids` array. This
/// notice is itself a delivery pasted into a pane, and an uncapped list is the
/// shape `PAUSE_SUPPRESSION_LIST_MAX` and `dropped_payload_preview` were both
/// written for. Realistic batches are far below the cap; the cap exists so the
/// worst case is bounded rather than because the common case needs it.
///
/// An empty slice cannot reach here (`flush_unconfirmed_notices` returns
/// early on an empty bucket); it degrades to the plural wording with no ids
/// rather than panicking.
pub fn unconfirmed_delivery_notice(agent_id: &str, delivery_ids: &[u64]) -> String {
    let ids = notice_id_list(delivery_ids);
    if delivery_ids.len() == 1 {
        format!(
            "[orrerix] delivery to {agent_id} unconfirmed (id {ids}) — the prompt may be sitting \
             unsubmitted in its pane; get_output it and re-send if needed"
        )
    } else {
        format!(
            "[orrerix] {n} deliveries to {agent_id} unconfirmed (ids {ids}) — one or more prompts \
             may be sitting unsubmitted in its pane; get_output it ONCE and re-send whichever \
             did not land",
            n = delivery_ids.len()
        )
    }
}

/// The capped, comma-joined id list both coalesced notices render (#539,
/// rev-13 N3). Shared so `delivery_eaten_notice` and
/// `unconfirmed_delivery_notice` cannot drift into different caps — they are
/// two wordings of the same batch shape, and a cap that applied to one of them
/// would be a bound that looked enforced and was not.
fn notice_id_list(delivery_ids: &[u64]) -> String {
    let shown = delivery_ids.len().min(UNCONFIRMED_NOTICE_IDS_MAX);
    let mut ids = delivery_ids[..shown].iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
    if delivery_ids.len() > shown {
        ids.push_str(&format!(", and {} more — see the audit log", delivery_ids.len() - shown));
    }
    ids
}

/// #585: the notice for a delivery whose text was EATEN — the box was read and
/// observed to hold neither our paste nor anything of the human's, and the
/// pane produced no turn's worth of output since our Enter.
///
/// Deliberately worded apart from `unconfirmed_delivery_notice`, which tells
/// the orchestrator the prompt "may be sitting unsubmitted in its pane". Here
/// it demonstrably is not: we looked, and it is gone. Telling an orchestrator
/// to go find text that no longer exists sends it to `get_output`, where it
/// sees an idle pane and — reasonably — concludes nothing is wrong. That is
/// not hypothetical: it is exactly how #585's two live losses were misread,
/// and the pre-emptive re-sends it invites are the #455 duplicate-kickoff
/// class. `.loomux/lessons.md`, "a claim is a deliverable": the notice states
/// what was observed, and names the idle pane the orchestrator is about to see
/// so an idle pane is not mistaken for a refutation.
///
/// #539 coalesces these per pane like the unconfirmed ones, so it names every
/// id it stands for and agrees with its own count. The parenthetical is kept
/// verbatim in both forms: it is the part that stops an idle pane being read
/// as a refutation, and it is no less needed when several were lost at once.
pub fn delivery_eaten_notice(agent_id: &str, delivery_ids: &[u64]) -> String {
    let ids = notice_id_list(delivery_ids);
    if delivery_ids.len() == 1 {
        format!(
            "[orrerix] delivery to {agent_id} was LOST (id {ids}) — its text never reached the \
             pane's box and the pane ran no turn on it. Re-send it. (get_output will show an \
             idle pane: that is the symptom, not evidence the delivery landed.)"
        )
    } else {
        format!(
            "[orrerix] {n} deliveries to {agent_id} were LOST (ids {ids}) — their text never \
             reached the pane's box and the pane ran no turn on them. Re-send them. (get_output \
             will show an idle pane: that is the symptom, not evidence they landed.)",
            n = delivery_ids.len()
        )
    }
}

/// #112 round 2: the correction notice for a delivery that already drew
/// `unconfirmed_delivery_notice` but has since been proven to have landed
/// after all — a late `promptsubmit` hook record arrived after the `failed`
/// alarm fired (see `DeliveryConfirmState`'s doc: `Failed` is reachable via
/// `ConfirmSource::BoxVeto`, itself not infallible — the rejection/error
/// residual named in the design note — or via the idle-without-evidence
/// trigger, which by construction can never rule out a coverage gap in the
/// hook itself). Named as a correction, not a re-confirmation, so the
/// orchestrator reads it as "stand down, the earlier alarm was wrong" rather
/// than a second, redundant success notice.
pub fn delivery_confirmed_late_notice(agent_id: &str) -> String {
    format!(
        "[orrerix] correction: the earlier \"delivery to {agent_id} unconfirmed\" alarm was wrong — \
         a prompt-landed signal for that same delivery has now arrived. It landed; no re-send needed."
    )
}

/// The sleep a supervised loop falls back to when it could not compute its own
/// next interval, because the closure that computes it panicked (#1702).
///
/// One minute: long enough that a loop whose interval read is broken cannot
/// spin the CPU, short enough that it keeps trying often enough to recover if
/// whatever broke clears. Only two loops compute an interval at all
/// (`compact_nudge_poll_interval`); for the rest this is unreachable, because
/// returning a constant cannot panic.
const TICK_FALLBACK_INTERVAL: Duration = Duration::from_secs(60);

/// Spawn a cadenced background loop whose body is SUPERVISED — a panic inside
/// it ends that tick and is breadcrumbed, rather than ending the thread
/// (#1702, `obs::TickSupervisor`).
///
/// **The supervision lives here rather than at each call site on purpose.** Ten
/// loops that each had to remember a `catch_unwind` is exactly the "did we
/// remember to do this one" review dependency #1600 §2 is about, and the
/// eleventh loop written next month is the one that would forget. A loop
/// written through this helper cannot: there is no `std::thread::spawn` in its
/// call site to leave unwrapped.
///
/// `interval` is a closure rather than a `Duration` because two of these loops
/// choose their next sleep from registry state
/// ([`compact_nudge_poll_interval`]), which reads locks and can therefore panic
/// exactly as a body can. It runs under the same supervisor, and a panicked
/// interval falls back to [`TICK_FALLBACK_INTERVAL`] so the thread cannot spin.
///
/// `start_attention` and `start_view_publisher` are deliberately NOT written
/// through this: the first runs three independent bodies on one thread and
/// wants three supervisors, so one broken pass cannot latch the other two off;
/// the second is one body but with a name of its own. Both use
/// `obs::TickSupervisor` directly. Eight loops here plus those two is the
/// whole of `mod.rs`'s cadenced set.
///
/// **One cadenced loop in this app is still unsupervised, and it is not in this
/// file**: `gitwatch::start` (`src-tauri/src/gitwatch.rs`) is the only
/// remaining bare `thread::spawn(move || loop { .. })` in the tree, and its
/// body reaches `lock_safe` through `poll_changed`. It is not a re-entrancy
/// site — `poll_changed` is snapshot, then stat, then re-acquire — so #1702
/// left it alone rather than widening past the plan's scope (§2 item 3a names
/// `mod.rs`'s `start_*` loops and `start_view_publisher`). Named here so "the
/// ticks are supervised" is read as a claim about this file, which is what it
/// is, rather than about the app (#1713 review N3).
fn spawn_tick_loop(
    tick: &'static str,
    mut interval: impl FnMut() -> Duration + Send + 'static,
    mut body: impl FnMut() + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut sup = crate::obs::TickSupervisor::new(tick);
        loop {
            let wait = sup.run_for(&mut interval).unwrap_or(TICK_FALLBACK_INTERVAL);
            std::thread::sleep(wait);
            sup.run(&mut body);
        }
    });
}

/// Background loop that enforces the idle-worker auto-kill guardrail: every
/// `IDLE_REAP_INTERVAL` it kills each worker/reviewer whose idle time has
/// crossed its group's `idle_kill_minutes` (groups with the guardrail off
/// are skipped inside `reap_idle_agents`, and so is a **liaison** block —
/// `idle_reap_candidates` owns both exclusions). Started once at app setup.
pub fn start_idle_reaper(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "idle-reaper",
        || IDLE_REAP_INTERVAL,
        move || {
            reg.reap_idle_agents(now_ms());
        },
    );
}

/// Background loop for the stalled-agent watchdog: every `WATCHDOG_INTERVAL`
/// it nudges the orchestrator (once per stall) about any working agent that
/// has gone silent — no terminal output, no report — past its group's
/// `watchdog_stall_minutes`. Groups with the guardrail off and paused groups
/// are skipped inside `run_watchdog`. Started once at app setup.
pub fn start_watchdog(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "watchdog",
        || WATCHDOG_INTERVAL,
        move || {
            reg.run_watchdog(now_ms());
        },
    );
}

/// What one wake of the unified `gh` poller did (#406) — returned so a test
/// can pin that a single tick services BOTH features, and which halves ran,
/// without a thread or a `gh` subprocess.
// `Default` is what a SKIPPED tick returns (#1609): the same "nothing
// happened" value a poll with no watches produces, so a skip needs no second
// code path in any caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GhPollTick {
    /// Watch ids the notify half resolved this tick (fired, expired, or
    /// cancelled) — `notify_tick`'s return, unchanged.
    pub fired: Vec<String>,
    /// Whether the intake half's due-group scan ran on this wake.
    pub intake_scanned: bool,
    /// The group the merge-queue driver serviced on this wake, if any (#698).
    ///
    /// The group rather than a bool, because "one group per wake, oldest
    /// serviced first" is the tick's whole bound and a bool could not tell a
    /// rotation from a group monopolising every wake.
    pub mq_serviced: Option<GroupId>,
    /// The group the review driver serviced on this wake, if any (#1778).
    ///
    /// Its own field rather than a shared one, for the same reason `mq_serviced`
    /// is a group and not a bool: the two loops rotate independently, and a
    /// single field could not tell a tick that drove one from a tick that drove
    /// both.
    pub rd_serviced: Option<GroupId>,
}

/// Whether the unified poller's INTAKE half is due on a wake at `now`, given
/// when its scan last ran (`None` = never, i.e. the first wake after launch).
/// Pure so the shared cadence is directly testable without spinning up the
/// thread — the `compact_nudge_poll_interval` idiom.
///
/// This is a scan-cadence floor, not the `gh` budget: `intake::
/// due_intake_polls` still owns the per-group interval that decides whether
/// a scan makes any `gh` call at all. Keeping the floor here means merging
/// the two loops does not silently move the intake scan onto the notify
/// cadence (twice as often), which is the one behavior change unification
/// would otherwise smuggle in.
///
/// A stamp in the FUTURE means the system clock moved BACKWARDS since the
/// last scan (an NTP correction, a VM resume, a manual set), and it rescans
/// rather than waiting for wall-clock to catch up — a plain elapsed check
/// stalls the intake scan for the entire size of the jump, which the
/// `sleep`-driven thread this replaced could not do (rev-157, non-blocking
/// 2). Rescanning re-stamps, so one jump costs one early scan, not a loop:
/// the per-group `gh` floor (`intake::due_intake_polls`) is what actually
/// bounds the API cost of an early scan, and it is unchanged.
pub fn intake_scan_due(now: u64, last_scan_ms: Option<u64>) -> bool {
    match last_scan_ms {
        None => true,
        Some(last) if now < last => true,
        Some(last) => now - last >= INTAKE_POLL_SCAN_INTERVAL.as_millis() as u64,
    }
}

/// The single background loop that makes `gh` calls in this process (#406).
/// Every `notify::NOTIFY_POLL_INTERVAL` it runs one `run_gh_poll_tick`, which
/// services both `gh`-polling features against one sampled instant:
///
/// - the notification backend (#243) on EVERY wake — it polls due watches
///   (`gh pr checks` / `gh run view`, backend-owned argv only, see
///   `gh_capture`) and delivers an `[orrerix] …` notice into the registering
///   agent's own pane the moment its condition is met, its TTL expires, or it
///   fails `notify::NOTIFY_FAIL_STREAK_LIMIT` polls running;
/// - the idle-tick intake gate (#332) on the wakes where `intake_scan_due`
///   says its coarser `INTAKE_POLL_SCAN_INTERVAL` scan cadence has elapsed —
///   it polls any autonomous group whose `intake_poll_minutes` guardrail is
///   due (`gh issue list` / `gh pr list`) and folds new label/PR-check signal
///   into that group's pending wake summary for `idle_tick_tick`.
///
/// Why one loop: both features spend the same account's GitHub API budget,
/// and two independently-clocked threads share no accounting of it — the
/// latent coupling #406 closes before the intake gate is load-bearing. The
/// PURE decision layers stay separate and separately tested
/// (`notify::due_watches`, `intake::due_intake_polls`); only the thread and
/// its clock are shared. Started once at app setup, beside `start_watchdog`.
pub fn start_gh_poller(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "gh-poller",
        || notify::NOTIFY_POLL_INTERVAL,
        move || {
            reg.run_gh_poll_tick();
        },
    );
}

/// Background loop for autonomous mode (#83): every `IDLE_TICK_INTERVAL` it
/// enforces autonomy token budgets (suspending a group that has overspent) and
/// then delivers one `[orrerix] idle tick` to any autonomous group's orchestrator
/// that has been output-quiet past `IDLE_TICK_MINUTES`, so the template's
/// idle-cadence intake/monitoring actually runs unattended. Non-autonomous and
/// paused groups are skipped inside `run_idle_tick`; the tick is self-regulating
/// (any orchestrator action resets the quiet clock) and hard-capped per hour.
/// Started once at app setup.
pub fn start_idle_tick(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "idle-tick",
        || IDLE_TICK_INTERVAL,
        move || {
            reg.run_idle_tick(now_ms());
        },
    );
}

/// One running orchestrator as `OrchRegistry::cache_idle_gather` saw it (#3407).
struct CacheIdleCandidate {
    id: String,
    group: GroupId,
    block: String,
    pty_id: Option<u32>,
    idle_ms: u64,
    latched: bool,
    compact_busy: bool,
}

/// Everything `OrchRegistry::cache_idle_decide` reads that lives behind a
/// registry lock, taken in one gather pass (#3407).
struct CacheIdleSnapshot {
    groups: HashMap<GroupId, Guardrails>,
    watch_groups: HashSet<GroupId>,
    intake_groups: HashSet<GroupId>,
    delegate_groups: HashSet<GroupId>,
    candidates: Vec<CacheIdleCandidate>,
}

/// What `OrchRegistry::cache_idle_decide` decided: latches to release, and
/// fires carrying the context percent and TTL the notice names (#3407).
#[derive(Default)]
struct CacheIdlePlan {
    release: Vec<String>,
    fire: Vec<(CacheIdleCandidate, u32, u32)>,
}

/// Round 10 (#428 follow-up): which cadence the compact-nudge loop's NEXT
/// sleep should use — pure so the selection is directly testable without
/// spinning up the thread/registry. `any_pending` is the ONLY input: no
/// hysteresis, no latch, no memory of a PREVIOUS tick's state — an arm that
/// opens and closes within one wake is simply seen or not seen on the wake
/// that actually happens to land, exactly like every other observation this
/// tick makes. That simplicity is deliberate: a debounced/latched cadence
/// would need its own state to get wrong, for a problem ("thrash") that
/// doesn't otherwise exist here — recomputing fresh every iteration already
/// can't oscillate faster than the loop itself runs.
pub fn compact_nudge_poll_interval(any_pending: bool) -> Duration {
    if any_pending { COMPACT_NUDGE_FAST_POLL_INTERVAL } else { IDLE_TICK_INTERVAL }
}

/// Background loop for compact-nudge (#287): normally wakes every
/// `IDLE_TICK_INTERVAL` to paste `/compact` for any eligible pane (default:
/// the orchestrator, on Claude Code) that has gone output-quiet past its
/// group's `compact_nudge_minutes` — the same idleness signal `idle_tick_
/// tick` reads, not a second one. Off by default (`compact_nudge_minutes ==
/// 0`); non-Claude CLIs and paused groups are skipped inside `run_compact_
/// nudge` / `compact_nudge_tick`.
///
/// Round 10: the sleep is now COMPUTED each iteration
/// (`compact_nudge_poll_interval`) rather than the fixed constant — while
/// `any_compact_pending()` is true anywhere in the registry, this thread
/// wakes on `COMPACT_NUDGE_FAST_POLL_INTERVAL` (10s) instead, so hook/marker
/// evidence gets consumed within seconds on both CLIs rather than riding out
/// the full 60s idle cadence. The interval is read BEFORE the sleep, from
/// whatever the previous tick left the registry in (or the empty state at
/// first launch) — so an arm opening mid-sleep is caught on the very next
/// wake, and the loop drops back to the normal cadence the wake after the
/// last open arm resolves. Started once at app setup.
pub fn start_compact_nudge(reg: Arc<OrchRegistry>) {
    let cadence = reg.clone();
    spawn_tick_loop(
        "compact-nudge",
        move || compact_nudge_poll_interval(cadence.any_compact_pending()),
        move || {
            reg.run_compact_nudge(now_ms());
        },
    );
}

/// Background loop for the merge-gate hot-reload (#385): every
/// `WORKFLOW_GATE_POLL_INTERVAL` it re-derives every advanced-orchestrator
/// group's merge gate from the repo's CURRENT `.loomux/workflow.yml`, so an
/// in-place edit to `gates.merge` takes effect without a relaunch or a manual
/// toggle off/on. See `OrchRegistry::reload_merge_gate_if_changed` for the
/// fail-closed contract this loop enforces. Started once at app setup, beside
/// `start_gh_poller`. (This one reads a file, not `gh` — it is deliberately
/// NOT folded into the unified poller of #406, whose subject is the shared
/// GitHub API budget.)
pub fn start_workflow_gate_reload(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "workflow-gate-reload",
        || WORKFLOW_GATE_POLL_INTERVAL,
        move || {
            reg.run_workflow_gate_reload();
        },
    );
}

/// Free bytes on the disk that hosts `path`: the mounted volume whose mount
/// point is the longest prefix of `path`. `None` if no volume matches (or the
/// listing is empty), so the caller no-ops rather than guessing.
fn free_disk_bytes(path: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

/// Background loop for the low-disk backstop (#134): every `DISK_CHECK_INTERVAL`
/// it samples free space on the workspace drive and, on crossing below the
/// threshold, sends one latched notice per group orchestrator. Started once at
/// app setup. Slow cadence keeps the sysinfo scan negligible.
pub fn start_disk_monitor(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "disk-monitor",
        || DISK_CHECK_INTERVAL,
        move || {
            reg.run_disk_monitor();
        },
    );
}

/// Background loop for the debounced cap-change notice (#79): every
/// `MAX_NOTICE_FLUSH_INTERVAL` it delivers any coalesced max-agents notice
/// whose quiet window has elapsed, so a burst of stepper clicks reaches the
/// orchestrator as one re-plan prompt instead of one per click. Started once
/// at app setup.
pub fn start_max_notice_flusher(reg: Arc<OrchRegistry>) {
    spawn_tick_loop(
        "max-notice-flusher",
        || MAX_NOTICE_FLUSH_INTERVAL,
        move || {
            reg.flush_due_max_notices(now_ms());
        },
    );
}

/// Background loop for the polled-view publisher (#1608, plan #1600 §3 Phase
/// 1): every [`views::VIEW_PUBLISH_INTERVAL`] it recomputes the group-view and
/// tab-strip payloads for every group the registry knows and publishes them as
/// one immutable snapshot, so the two polled commands can be served by pointer
/// clone instead of by acquiring registry mutexes on every tick.
///
/// **This is the thread that pays the wait.** If a registry lock is held
/// pathologically long, exactly one thread parks here — not one per poller per
/// tick on the shared blocking pool, which is the accumulation #1600 §1.2
/// derives beta6 from. Readers keep answering with the last snapshot and a
/// growing `age_ms`, and the frontend badges it stale past
/// [`views::VIEW_STALE_AFTER_MS`]. Started once at app setup.
///
/// **The pass is supervised** (#1702, `obs::TickSupervisor`). It was not, and
/// `docs/design/polled-views.md` disclosed that: a panic inside one group's
/// `compute_group` ended this thread permanently and froze the snapshot for
/// BOTH polled surfaces at once, with only the stale badge between a dead
/// publisher and a plausible-looking frozen UI. A panic now costs one pass and
/// a `tick-panicked` breadcrumb; the badge still covers the interval, which is
/// what makes the degrade the same one the rest of that design already has.
pub fn start_view_publisher(reg: Arc<OrchRegistry>) {
    std::thread::spawn(move || {
        let mut sup = crate::obs::TickSupervisor::new("view-publisher");
        loop {
            std::thread::sleep(views::VIEW_PUBLISH_INTERVAL);
            sup.run(|| reg.views.publish_pass(&reg));
        }
    });
}

/// Background loop for attention routing (#6): every `ATTENTION_INTERVAL` it
/// recomputes which panes need the human (idle-with-prompt, worker reports,
/// human merge gates), pushes the set to the frontend for pane badges, and
/// toasts newly-attention panes in notification-enabled groups. Started once at
/// app setup.
/// (#825 M2) Every `STRANDED_JANITOR_EVERY_N_TICKS`th pass also runs the
/// stuck-prompt badge janitor, so a raised chip keeps being checked against
/// the pane after `run_late_confirmation_monitor` — which has done exactly
/// that check every 5s since #496 PR-C — exits at
/// `LATE_MONITOR_MAX_LIFETIME`. It runs FIRST so a chip this pass takes down
/// is already gone from the item set `run_attention` emits in the same tick,
/// rather than being shown one more time and disappearing 3s later.
///
/// **Cost, declared (performance.md §3 INV-4: cadenced work says what it
/// costs).** Fixed-cadence work, so it owes a bound. Four things bound it:
///
/// 1. **Scope.** Zero pty reads unless a stuck-prompt chip is actually up, in
///    one of the classes a pane reading can answer, on a Running agent with a
///    pty. `attn_stranded` is empty on every healthy session, and the pass
///    returns on that check having taken one uncontended map lock and nothing
///    else — no agents lock, no ledger, no ring. The badged case is rare and
///    self-limiting: this very reading is what takes the chip down, so the
///    work ends the condition that schedules it (#819 F3's argument, and
///    `drain_stranded_submit`'s).
/// 2. **Per badged pane.** One ledger lock + clone, one `Tier1Scan::for_paste`
///    normalize of the recorded text, then at most `TIER1_SCAN_WIDEN_ROUNDS`
///    ring reads capped at `TIER1_SCAN_WIDEN_MAX_BYTES` — `Tier1Scan::widen`'s
///    own bound, not a fresh one — and one `last_user_input_ms` stamp read. No
///    IPC, no fs, no lock beyond the ring's own, and nothing held across a pty
///    read.
/// 3. **Cadence.** 30s, against the 5s the late monitor already spends on the
///    identical read on the same panes: one sixth the rate, for the far
///    smaller set of panes that still carry a chip. Where the two overlap (a
///    chip up while its monitor is alive) the extra cost is one such read per
///    30s, and the verdict is identical by construction — both ask
///    `stranded_badge_release`.
/// 4. **Worst case.** Bounded by the number of badged panes, which is bounded
///    by the fleet size; a fleet where every pane carries a stuck-prompt chip
///    has a much louder problem than this loop.
///
/// (#825 M3) The same tick then runs the `QueueFull` re-admission, which is the
/// one unreleased class whose answer is a retry rather than a reading — see
/// [`queuefull_readmit_gate`] for why it observes the pane *after* a drain
/// instead of hooking the drain edge itself. It rides this cadence rather than
/// a second one because it asks the same question at the same moment, and it is
/// **cheaper than the janitor by a wide margin**: no pty read, no ledger clone,
/// no ring scan. Its bound, same INV-4 discipline:
///
/// 1. **Scope.** Returns after one uncontended `attn_stranded` lock unless a
///    chip is up somewhere — the healthy-session case, identical to the
///    janitor's. Self-limiting in the same sense: a successful re-admission
///    re-words the chip off `QueueFull`, so the work ends the condition that
///    schedules it, and a pane can only be re-admitted for as long as its own
///    badge says the re-send was refused.
/// 2. **Per badged pane.** Two uncontended map reads (`queue_depth`,
///    `drainer_active`) before anything else, and for the pane that passes them
///    one `admit_stranded_selfheal` — a single `queues` critical section and one
///    ledger read. No pty read at all, at any point: nothing here looks at a
///    pane, which is the whole difference between a retry and a reading.
/// 3. **Cadence and worst case.** As above — 30s, bounded by the badged-pane
///    count.
///
/// Both passes run BEFORE `run_attention` so a chip either of them changed is
/// emitted this tick with its new wording, rather than being shown stale once
/// more and corrected 3s later.
///
/// (#814) `run_attention` itself then pushes the delivery-queue badge set
/// (`orch-queue-depth`), which rides this cadence for the reason the badge exists:
/// the age it shows has to keep growing on screen, and the frontend deliberately
/// has no clock of its own. Its bound, same discipline:
///
/// 1. **Scope.** One uncontended `queues` read per tick, and a return with no
///    emit at all when no pane has anything queued — the ordinary state.
/// 2. **Per pane with a queue.** A count and a minimum over at most
///    `queue::QUEUE_MAX_PER_PANE` (8) stamps, taken under the queue lock and
///    nothing cloned out of it, then one `hold_episodes` read. No pty read.
/// 3. **Emit.** At most one event per tick, and only when the coarsened reading
///    actually differs from the last one pushed
///    (`OrchRegistry::queue_depth_push`, `queue::coarsen_waiting_ms`) — so a pane
///    stuck for an hour costs about one emit a minute, not one every 3s — plus
///    one unconditional re-push of a non-empty set every `QUEUE_DEPTH_REPUSH_MS`,
///    which is that suppression's independent release rather than a cadence of
///    its own.
///
/// **Three supervisors on one thread** (#1702). This loop runs three
/// independent bodies, and they are supervised separately rather than as one
/// pass: a panic in the janitor is not a reason to stop emitting attention
/// badges, and `obs::TickSupervisor`'s consecutive-panic latch would otherwise
/// take all three down for one broken one. `spawn_tick_loop` cannot express
/// that, which is why this loop is still written out.
pub fn start_attention(reg: Arc<OrchRegistry>) {
    std::thread::spawn(move || {
        let mut tick: u64 = 0;
        let mut sup_janitor = crate::obs::TickSupervisor::new("attention-stranded-janitor");
        let mut sup_readmit = crate::obs::TickSupervisor::new("attention-queuefull-readmit");
        let mut sup_attention = crate::obs::TickSupervisor::new("attention");
        loop {
            std::thread::sleep(ATTENTION_INTERVAL);
            tick = tick.wrapping_add(1);
            if tick % STRANDED_JANITOR_EVERY_N_TICKS == 0 {
                sup_janitor.run(|| reg.run_stranded_janitor());
                sup_readmit.run(|| reg.run_stranded_queuefull_readmit());
            }
            sup_attention.run(|| reg.run_attention(now_ms()));
        }
    });
}

mod commands;
pub use commands::*;

/// WinRT toast script (see `notify_desktop`). Title/body come in via
/// environment variables — never interpolated into the script — so agent/board
/// text can't inject PowerShell. XML-escaped before templating. The AppUserModel
/// id is the stock PowerShell shortcut, which lets an unpackaged process raise a
/// toast on Windows 10; it renders attributed to PowerShell, which is fine for
/// an optional signal.
#[cfg(target_os = "windows")]
const TOAST_PS1: &str = r#"
$ErrorActionPreference='SilentlyContinue'
[void][Windows.UI.Notifications.ToastNotificationManager,Windows.UI.Notifications,ContentType=WindowsRuntime]
[void][Windows.Data.Xml.Dom.XmlDocument,Windows.Data.Xml.Dom,ContentType=WindowsRuntime]
$t=[System.Security.SecurityElement]::Escape($env:LOOMUX_TOAST_TITLE)
$b=[System.Security.SecurityElement]::Escape($env:LOOMUX_TOAST_BODY)
$xml="<toast><visual><binding template='ToastGeneric'><text>$t</text><text>$b</text></binding></visual></toast>"
$doc=New-Object Windows.Data.Xml.Dom.XmlDocument
$doc.LoadXml($xml)
$toast=New-Object Windows.UI.Notifications.ToastNotification $doc
$app='{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($app).Show($toast)
"#;

/// Best-effort OS desktop notification (attention routing #6). On Windows this
/// spawns a hidden PowerShell that raises a WinRT toast, passing the title/body
/// as environment variables (injection-proof — see `TOAST_PS1`). Deliberately
/// no notification crate: those pull getrandom, which this project's Windows 10
/// baseline can't load (0xc0000139 — see the Cargo.toml note). Silently a no-op
/// on failure and on non-Windows; the pane badges and board highlight are the
/// primary signal regardless.
#[cfg(target_os = "windows")]
fn notify_desktop(title: &str, body: &str) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", TOAST_PS1])
        .env("LOOMUX_TOAST_TITLE", title)
        .env("LOOMUX_TOAST_BODY", body)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn();
}

#[cfg(not(target_os = "windows"))]
fn notify_desktop(_title: &str, _body: &str) {}

#[cfg(test)]
mod hold_tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(4);
    const CAP: Duration = Duration::from_secs(90);

    #[test]
    fn holds_while_human_typed_recently() {
        // Typed 1s ago (< 4s window), well under the cap: keep holding.
        assert!(should_hold_for_user(9_000, 10_000, Duration::from_secs(5), WINDOW, CAP));
    }

    #[test]
    fn proceeds_once_human_is_quiet() {
        // Last keystroke was 5s ago (> 4s window): deliver.
        assert!(!should_hold_for_user(5_000, 10_000, Duration::from_secs(2), WINDOW, CAP));
    }

    #[test]
    fn proceeds_when_nobody_typed() {
        // 0 == no keystroke ever recorded for this pane.
        assert!(!should_hold_for_user(0, 10_000, Duration::ZERO, WINDOW, CAP));
    }

    #[test]
    fn cap_forces_delivery_even_if_still_typing() {
        // Human is still typing (0ms ago) but the hold hit the 90s cap:
        // deliver anyway so reports aren't starved forever.
        assert!(!should_hold_for_user(10_000, 10_000, CAP, WINDOW, CAP));
        // One tick over the cap also delivers.
        assert!(!should_hold_for_user(10_000, 10_000, CAP + Duration::from_millis(1), WINDOW, CAP));
    }

    #[test]
    fn boundary_at_exactly_the_window_proceeds() {
        // `since == window` is not "< window", so it proceeds (quiet enough).
        assert!(!should_hold_for_user(6_000, 10_000, Duration::from_secs(1), WINDOW, CAP));
    }

    #[test]
    fn future_timestamp_does_not_underflow() {
        // A clock skew where last_input is "after" now must not panic or wrap;
        // saturating_sub yields 0 → within window → hold.
        assert!(should_hold_for_user(11_000, 10_000, Duration::from_secs(1), WINDOW, CAP));
    }
}

/// #385/B1: the stability check `read_workflow_stably` layers on top of a
/// plain read — pure numeric logic, no filesystem, so it's unit-tested here
/// rather than by trying to race a real write in an integration test (which
/// would be exactly the kind of flaky, timing-dependent test this fix was
/// designed to avoid depending on).
#[cfg(test)]
mod workflow_gate_reload_read_tests {
    use super::*;

    #[test]
    fn agreeing_stats_and_read_are_stable() {
        assert!(OrchRegistry::workflow_read_is_stable(120, 120, 120));
    }

    #[test]
    fn a_shrink_between_the_before_stat_and_the_read_is_unstable() {
        // The file was still being truncated when we read it: the pre-read
        // stat saw the old (larger) size, the read itself caught the new one.
        assert!(!OrchRegistry::workflow_read_is_stable(200, 120, 120));
    }

    #[test]
    fn growth_between_the_read_and_the_after_stat_is_unstable() {
        // The writer kept appending right after our read finished — we may
        // have caught an early, incomplete slice of a multi-write save.
        assert!(!OrchRegistry::workflow_read_is_stable(120, 120, 260));
    }

    #[test]
    fn a_fully_torn_read_disagreeing_on_all_three_is_unstable() {
        assert!(!OrchRegistry::workflow_read_is_stable(50, 120, 260));
    }
}
