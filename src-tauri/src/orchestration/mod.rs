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
mod humaninput;
pub use humaninput::*;
mod idlepolicy;
pub use idlepolicy::*;
mod kickoff;
pub use kickoff::*;
mod lockprose;
use lockprose::*;
mod panetail;
pub use panetail::*;
mod pathkey;
pub use pathkey::*;
mod persona;
pub use persona::*;
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
mod tier1;
pub use tier1::*;
mod tuning;
pub use tuning::*;
mod unconfirmed;
pub use unconfirmed::*;
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
