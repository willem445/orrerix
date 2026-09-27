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
