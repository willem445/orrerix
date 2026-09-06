//! Codex usage snapshot (#2515 slice C3): the codex line folder
//! (`usage::parse_codex_transcript`), the store LOOKUP that stands in for a
//! path join (`sessions::find_codex_session_file`), the on-disk reader
//! (`usage::codex_session_usage_in`), and the `compute_usage_snapshot` arm that
//! decides a codex agent's usage comes from the human's own rollout store.
//!
//! An integration test, not inline `#[cfg(test)]`, per repo constraint 4 — a
//! unit-test binary linking the full lib misses the comctl32-v6 manifest
//! `build.rs` only embeds for integration-test targets.
//!
//! **No codex is ever run** (constraint 3). Every fixture below is written here
//! from shapes read off `openai/codex` at tag `rust-v0.153.4`, blob by blob
//! through the GitHub blob API and quoted in `doc/design/codex.md`:
//!
//! - `history/src/rollout_payload.rs` — `RolloutItemWire`,
//!   `#[serde(tag = "type", rename_all = "snake_case")]`, so a line is
//!   `{"type":"token_usage_record","payload":{…}}` and friends;
//! - `protocol/src/protocol.rs` — `struct TokenUsageRecord` (the THREE
//!   `TokenUsage` objects one line carries: `usage`, `turn_token_usage`,
//!   `thread_token_usage`), `struct TokenUsage` (its six counters), and
//!   `struct TurnContextItem` (whose `model` is a required `String`);
//! - `codex-api/src/sse/responses.rs` — `impl From<ResponseCompletedUsage> for
//!   TokenUsage`, which is why `cached_input_tokens` and
//!   `cache_write_input_tokens` are DETAILS of `input_tokens` and
//!   `reasoning_output_tokens` a detail of `output_tokens`;
//! - `rollout/src/recorder.rs` and `rollout/src/compression.rs` — the
//!   `sessions/YYYY/MM/DD` tree, the `rollout-<ts>-<thread>[_<rollout>].jsonl`
//!   name, and the `.jsonl.zst` representation a week-old rollout is kept in.
//!
//! If codex ever changes those shapes these tests keep passing against a record
//! that no longer exists — which is exactly why the production fold contributes
//! nothing for a line it does not recognise rather than trusting the keys to be
//! there.

use loomux_lib::orchestration::{workflow, Guardrails, OrchRegistry, Role};
use loomux_lib::sessions::{set_claude_projects_root_for_test, set_codex_sessions_root_for_test};
use loomux_lib::usage::{
    codex_session_usage_in, parse_codex_transcript, TranscriptCursors, TranscriptKind,
};
// Reached through the engine rather than `loomux_lib`'s re-export list: both
// are already `pub` there, and the thread-local seam the test binds is the
// engine's own — so importing them here asserts the production arm and this
// test are looking at ONE store resolver rather than two spellings of it.
use loomux_engine::pathseg::PathSegment;
use loomux_engine::sessions::{codex_sessions_root, find_codex_session_file};
use std::fs;
use std::path::{Path, PathBuf};

/// A thread id in the shape codex's own round-trip test uses
/// (`rollout_file_name_tests.rs`), so the fixture names below are shapes codex
/// asserts on rather than shapes this repo invented.
const THREAD: &str = "019ff1a2-b3c4-7d5e-8f60-112233445566";
/// The second half of a reverted rollout's name — `thread/revert` writes
/// `rollout-<ts>-<thread>_<rollout>.jsonl`, and the LEADING half is the one a
/// lookup compares against.
const ROLLOUT: &str = "019ff1a2-b3c4-7d5e-8f60-667788990011";
const DATE: (&str, &str, &str) = ("2026", "09", "03");
const TS: &str = "2026-09-03T14-00-00";

// ---------------------------------------------------------------------------
// Fixture builders — codex's record shapes, from the blobs cited above
// ---------------------------------------------------------------------------

/// One response's numbers, in CODEX's vocabulary rather than loomux's — so a
/// reader can compare a fixture against `struct TokenUsage` directly and the
/// mapping under test is never smuggled into the fixture builder.
///
/// `input` is the WHOLE prompt count with `cached` and `cache_write` inside it;
/// `output` is the whole completion count with `reasoning` inside it. `total`
/// is codex's own `total_tokens`, which its API mapping makes `input + output`.
#[derive(Clone, Copy, Default)]
struct Usage {
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
    reasoning: u64,
}

impl Usage {
    /// codex's own `total_tokens` for this response: input + output, both
    /// inclusive of their details. The identity every mapping assertion below
    /// checks itself against.
    fn total(&self) -> u64 {
        self.input + self.output
    }

    fn json(&self) -> String {
        format!(
            "{{\"input_tokens\":{},\"cached_input_tokens\":{},\
             \"cache_write_input_tokens\":{},\"output_tokens\":{},\
             \"reasoning_output_tokens\":{},\"total_tokens\":{}}}",
            self.input,
            self.cached,
            self.cache_write,
            self.output,
            self.reasoning,
            self.total()
        )
    }
}

/// A `token_usage_record` line: this response's `usage`, plus the two RUNNING
/// totals codex writes on the same line. `turn`/`thread` are passed explicitly
/// so a fixture can make them DIFFER from the per-response figure — which is
/// the only way the "summed per response, not read off the cumulative total"
/// assertion can fail against an implementation that reads the wrong one.
fn usage_line(usage: Usage, turn: Usage, thread: Usage) -> String {
    format!(
        "{{\"timestamp\":\"2026-09-03T14:00:05.000Z\",\"type\":\"token_usage_record\",\
         \"payload\":{{\"thread_id\":\"{THREAD}\",\"turn_id\":\"t1\",\
         \"session_id\":\"{THREAD}\",\"root_turn_id\":\"t1\",\"response_id\":\"r1\",\
         \"usage\":{},\"turn_token_usage\":{},\"thread_token_usage\":{}}}}}\n",
        usage.json(),
        turn.json(),
        thread.json()
    )
}

/// A `turn_context` line, carrying the model this turn ran on.
fn turn_context(model: &str) -> String {
    format!(
        "{{\"timestamp\":\"2026-09-03T14:00:01.000Z\",\"type\":\"turn_context\",\
         \"payload\":{{\"cwd\":\"C:/tmp/codex-repo\",\"approval_policy\":\"on-request\",\
         \"sandbox_policy\":{{\"mode\":\"workspace-write\"}},\"model\":\"{model}\"}}}}\n"
    )
}

/// The `session_meta` first line, in the vendor's key order.
fn header(id: &str, cwd: &str) -> String {
    format!(
        "{{\"timestamp\":\"2026-09-03T14:00:00.000Z\",\"type\":\"session_meta\",\
         \"payload\":{{\"session_id\":\"{id}\",\"id\":\"{id}\",\
         \"timestamp\":\"2026-09-03T14:00:00.000Z\",\"cwd\":\"{cwd}\",\
         \"originator\":\"codex_cli_rs\",\"cli_version\":\"0.153.4\"}}}}\n"
    )
}

/// An `event_msg`/`token_count` line — a shape the fold must IGNORE. It carries
/// `info.total_token_usage`, the same cumulative figure `thread_token_usage`
/// is, written for the TUI's own display.
fn token_count_event(total: Usage) -> String {
    format!(
        "{{\"timestamp\":\"2026-09-03T14:00:06.000Z\",\"type\":\"event_msg\",\
         \"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{},\
         \"last_token_usage\":{},\"model_context_window\":272000}}}}}}\n",
        total.json(),
        total.json()
    )
}

/// Write a rollout at `<root>/YYYY/MM/DD/<name>` with `body` verbatim, so a test
/// needing an odd shape (a torn line, a `.zst`, a revert suffix) writes it
/// rather than asking this helper for a flag.
fn write_raw(root: &Path, name: &str, body: &str) -> PathBuf {
    let dir = root.join(DATE.0).join(DATE.1).join(DATE.2);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

/// The canonical (never-reverted) rollout name for a thread.
fn rollout_name(thread: &str) -> String {
    format!("rollout-{TS}-{thread}.jsonl")
}

/// A rollout for `THREAD` under the canonical name, with `body` after the
/// header.
fn write_rollout(root: &Path, body: &str) -> PathBuf {
    write_raw(
        root,
        &rollout_name(THREAD),
        &format!("{}{body}", header(THREAD, "C:/tmp/codex-repo")),
    )
}

// ---------------------------------------------------------------------------
// The fold: what a line MEANS
// ---------------------------------------------------------------------------

#[test]
fn token_usage_records_are_summed_per_response_not_read_off_the_cumulative_thread_total() {
    // There are FOUR readings of this file that are not "sum each record's own
    // `usage`", and a fixture only pins the rule if every one of them lands on a
    // different number. Two shapes are what make that possible, and both are
    // codex's own rather than invented here:
    //
    // - **the thread is RESUMED**, so `thread_token_usage` opens at a non-zero
    //   baseline. `codex resume` continues one thread into a NEW rollout file,
    //   and the thread running total carries the earlier file's spend with it.
    //   Without this the last `thread_token_usage` EQUALS the sum of `usage` by
    //   construction, and that reading is indistinguishable arithmetically —
    //   which is why the fold's doc refuses it on the cursor's incremental
    //   contract instead;
    // - **two TURNS**, so `turn_token_usage` resets between them and its last
    //   value is not the whole file's spend either.
    //
    // Turn 1 is two responses (300 + 700), turn 2 is one (500). The thread
    // arrives carrying 4_000 input from the rollout this one resumed.
    let r1 = Usage { input: 300, output: 30, ..Usage::default() };
    let r2 = Usage { input: 700, output: 70, ..Usage::default() };
    let r3 = Usage { input: 500, output: 50, ..Usage::default() };
    // `turn_token_usage` after each response: the turn's own prefix sums.
    let turn1 = Usage { input: 300, output: 30, ..Usage::default() };
    let turn2 = Usage { input: 1_000, output: 100, ..Usage::default() };
    let turn3 = Usage { input: 500, output: 50, ..Usage::default() };
    // `thread_token_usage` after each: the thread's prefix sums, on top of the
    // 4_000/400 this resumed thread began with.
    let thread1 = Usage { input: 4_300, output: 430, ..Usage::default() };
    let thread2 = Usage { input: 5_000, output: 500, ..Usage::default() };
    let thread3 = Usage { input: 5_500, output: 550, ..Usage::default() };

    let text = format!(
        "{}{}{}{}",
        header(THREAD, "C:/tmp/codex-repo"),
        usage_line(r1, turn1, thread1),
        usage_line(r2, turn2, thread2),
        usage_line(r3, turn3, thread3),
    );
    let u = parse_codex_transcript(&text);

    assert_eq!(u.tokens.input_tokens, 1_500, "the three responses' own `usage`, summed");
    assert_eq!(u.tokens.output_tokens, 150);

    // The discriminating half: all four other readings, each landing somewhere
    // else. Written as a loop so a fixture edit that made any two of them
    // collide — which is exactly the defect this test shipped with in review
    // round 0 — fails HERE rather than quietly stopping the assertions below
    // from discriminating anything.
    let wrong = [
        ("summing `thread_token_usage` — a series of prefixes", 4_300 + 5_000 + 5_500u64),
        ("reading the LAST `thread_token_usage`", 5_500),
        ("summing `turn_token_usage`", 300 + 1_000 + 500),
        ("reading the LAST `turn_token_usage`", 500),
    ];
    for (reading, figure) in wrong {
        assert_ne!(
            figure, 1_500,
            "fixture: {reading} must land on a DIFFERENT number from the right answer, or the \
             assertion below pins nothing"
        );
        assert_ne!(
            u.tokens.input_tokens, figure,
            "{reading} would report {figure}: N responses report roughly N times the real \
             spend, and nothing about the number looks wrong"
        );
    }

    // And the identity the mapping is designed around: loomux's four disjoint
    // buckets sum to codex's own `total_tokens` for the same responses.
    assert_eq!(u.tokens.total(), r1.total() + r2.total() + r3.total());
}

#[test]
fn reasoning_tokens_are_not_added_to_output() {
    // `reasoning_output_tokens` is `output_tokens_details.reasoning_tokens` —
    // a detail OF `output_tokens`, exactly as the cache figures are of input
    // (`codex-api/src/sse/responses.rs`). Adding it would double-count.
    let r = Usage { input: 1_000, output: 400, reasoning: 250, ..Usage::default() };
    let u = parse_codex_transcript(&format!(
        "{}{}",
        header(THREAD, "C:/tmp/codex-repo"),
        usage_line(r, r, r)
    ));

    assert_eq!(u.tokens.output_tokens, 400, "output is codex's `output_tokens`, whole");
    assert_ne!(u.tokens.output_tokens, 650, "the reasoning half must not be added on top");
    // The control that makes the assertion above non-vacuous: the fixture
    // really does carry a reasoning figure, and it is big enough that folding
    // it in would be unmistakable.
    assert!(r.reasoning > 0, "fixture: a zero reasoning count would pin nothing");
    assert_eq!(u.tokens.total(), r.total(), "and the total still matches codex's own");
}

#[test]
fn cache_read_and_write_land_in_their_own_buckets() {
    // The vendor's own fixture arithmetic (`parses_cache_write_token_usage`):
    // input 100 = cached 40 + cache_write 60, output 10, total 110. Scaled up
    // here so a fresh-input remainder exists and a mapping that forgot ONE of
    // the two subtractions is still caught.
    let r = Usage { input: 1_000, cached: 400, cache_write: 100, output: 60, reasoning: 20 };
    let u = parse_codex_transcript(&format!(
        "{}{}",
        header(THREAD, "C:/tmp/codex-repo"),
        usage_line(r, r, r)
    ));

    assert_eq!(u.tokens.cache_read_tokens, 400, "`cached_input_tokens`");
    assert_eq!(u.tokens.cache_creation_tokens, 100, "`cache_write_input_tokens`");
    // The bucket that carries the correction. codex's `input_tokens` is the
    // WHOLE prompt count with both cache figures inside it, and loomux's four
    // buckets are disjoint — so fresh input is the remainder, and mapping
    // `input_tokens` across whole would report 1500 total for a response codex
    // calls 1060.
    assert_eq!(u.tokens.input_tokens, 500, "1000 − 400 cached − 100 written");
    assert_eq!(
        u.tokens.total(),
        r.total(),
        "the identity the mapping is designed around: the four disjoint buckets sum to \
         codex's own `total_tokens`"
    );
    assert_ne!(u.tokens.total(), 1_500, "which is what mapping `input_tokens` whole would give");
}

#[test]
fn the_model_is_the_latest_turn_context() {
    // codex writes a fresh `turn_context` per user turn, so "which model is
    // this pane on" is answered by the last one — not by the first, and not by
    // a best-priced-by-output choice (there are no prices here at all).
    let r = Usage { input: 100, output: 10, ..Usage::default() };
    let text = format!(
        "{}{}{}{}{}",
        header(THREAD, "C:/tmp/codex-repo"),
        turn_context("gpt-5.1-codex-mini"),
        usage_line(r, r, r),
        turn_context("gpt-5.1-codex-max"),
        usage_line(r, r, r),
    );
    let u = parse_codex_transcript(&text);
    assert_eq!(u.model.as_deref(), Some("gpt-5.1-codex-max"));
    // Non-vacuity: the two fixture models really do differ, so the assertion
    // above would fail against an implementation keeping the FIRST.
    assert_ne!("gpt-5.1-codex-mini", "gpt-5.1-codex-max");
}

#[test]
fn a_cumulative_token_count_event_is_not_folded_in() {
    // `event_msg`/`token_count` carries `info.total_token_usage`, the same
    // cumulative figure `thread_token_usage` is. It sits on its own LINE rather
    // than beside a per-response figure, so a fold that recognised it would add
    // a whole extra copy of the thread's spend — invisible in the two
    // assertions above, which never write one.
    let r = Usage { input: 500, output: 50, ..Usage::default() };
    let text = format!(
        "{}{}{}",
        header(THREAD, "C:/tmp/codex-repo"),
        usage_line(r, r, r),
        token_count_event(r),
    );
    let u = parse_codex_transcript(&text);
    assert_eq!(u.tokens.input_tokens, 500, "the record's `usage` once, not twice");
    assert_eq!(u.tokens.total(), r.total());
}

#[test]
fn an_unpriced_codex_model_leaves_cost_unknown_and_estimated_true() {
    // codex writes no dollars, and `price_for`'s table is dated ANTHROPIC
    // rates with no codex model in it. So the fold reports tokens and a blank
    // cost — an honest blank rather than an undated OpenAI price column
    // invented here — while the snapshot arm still labels the row `estimated`.
    let r = Usage { input: 9_000, output: 900, ..Usage::default() };
    let u = parse_codex_transcript(&format!(
        "{}{}{}",
        header(THREAD, "C:/tmp/codex-repo"),
        turn_context("gpt-5.1-codex-max"),
        usage_line(r, r, r)
    ));
    assert_eq!(u.cost_usd, None, "no codex model is priced");
    assert_eq!(u.model.as_deref(), Some("gpt-5.1-codex-max"), "but the model is still reported");
    assert!(u.tokens.total() > 0, "and the tokens are exact");

    // The snapshot half of the claim, through the real arm.
    let (reg, _d, seam) = codex_registry();
    let g = reg.create_group("C:/tmp/codex-repo", rails("codex")).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let mut entry = w.clone();
    entry.session_id = Some(THREAD.to_string());
    write_rollout(
        &seam.codex,
        &format!("{}{}", turn_context("gpt-5.1-codex-max"), usage_line(r, r, r)),
    );

    let snap = reg.compute_usage_snapshot(&entry, "codex");
    assert_eq!(snap.source, "codex-transcript");
    assert_eq!(snap.cost_usd, None);
    assert!(
        snap.estimated,
        "codex records no dollars, so any figure here would be OURS — the claude posture, \
         not opencode's and pi's reported one. The label is what keeps a group total that \
         mixes codex with claude describable."
    );
}

// ---------------------------------------------------------------------------
// The on-disk reader: which file, and what a partial write does
// ---------------------------------------------------------------------------

#[test]
fn a_torn_last_line_keeps_everything_before_it() {
    // codex appends a record and its newline as separate bytes, so a poll can
    // land between them. The partial line is held back, not folded — folding a
    // truncated record is not "one tick early", it is permanently wrong.
    let seam = seam();
    let complete = Usage { input: 400, output: 40, ..Usage::default() };
    // The running totals on the same line are deliberately NOT the per-response
    // figure, so this fixture also discriminates the cumulative-column defect —
    // a fold reading `thread_token_usage` here reports 9_000, not 400.
    let running = Usage { input: 9_000, output: 900, ..Usage::default() };
    let torn_line = usage_line(
        Usage { input: 5_000, output: 500, ..Usage::default() },
        running,
        running,
    );
    write_rollout(
        &seam.codex,
        &format!("{}{}", usage_line(complete, running, running), &torn_line[..torn_line.len() / 2]),
    );

    let u = codex_session_usage_in(&seam.codex, THREAD).expect("the rollout is found");
    assert_eq!(u.tokens.input_tokens, 400, "the complete record counts");
    assert_eq!(u.tokens.output_tokens, 40, "and the torn one contributes nothing");
    // Non-vacuity: the held-back half really did carry a figure, and a large
    // one, so an implementation that folded it would be unmistakable here.
    assert_ne!(u.tokens.input_tokens, 5_400);
}

#[test]
fn a_reverted_rollouts_name_still_answers_for_its_thread() {
    // `thread/revert` writes `rollout-<ts>-<thread>_<rollout>.jsonl`. The
    // TRAILING half is the rollout id, not a sequence number, so the LEADING
    // half is what a lookup compares against and a reverted thread goes on
    // answering for its own id. A `format!("{id}.jsonl")` could not name this
    // file at all, which is why the codex arm is a lookup.
    let seam = seam();
    let r = Usage { input: 250, output: 25, ..Usage::default() };
    write_raw(
        &seam.codex,
        &format!("rollout-{TS}-{THREAD}_{ROLLOUT}.jsonl"),
        &format!("{}{}", header(THREAD, "C:/tmp/codex-repo"), usage_line(r, r, r)),
    );

    let found = find_codex_session_file(&seam.codex, &PathSegment::parse(THREAD).unwrap())
        .expect("a reverted rollout is still this thread's");
    assert!(found.to_string_lossy().contains(ROLLOUT), "the fixture really is the revert form");
    let u = codex_session_usage_in(&seam.codex, THREAD).expect("and its usage reads");
    assert_eq!(u.tokens.input_tokens, 250);
}

#[test]
fn a_compressed_rollout_reports_no_usage_rather_than_a_partial_total() {
    // A rollout older than about seven days is zstd-compressed in place. C2
    // could degrade to "found, workspace unknown" because the by-id CWD lookup
    // has a distinct empty answer; usage has no such rung — a total assembled
    // from part of a file would be a WRONG number, the one failure this meter
    // refuses — so a compressed rollout answers `None`. Decompressing means a
    // new `src-tauri` dependency and its getrandom audit (constraint 2).
    let seam = seam();
    let r = Usage { input: 900, output: 90, ..Usage::default() };
    // Not real zstd: the decision is taken off the NAME, and writing plausible
    // JSONL under a `.zst` name is the stronger fixture — it fails against an
    // implementation that opened the file and read whatever was inside.
    write_raw(
        &seam.codex,
        &format!("{}.zst", rollout_name(THREAD)),
        &format!("{}{}", header(THREAD, "C:/tmp/codex-repo"), usage_line(r, r, r)),
    );

    assert_eq!(
        find_codex_session_file(&seam.codex, &PathSegment::parse(THREAD).unwrap()),
        None,
        "a compressed rollout is not a readable transcript"
    );
    assert!(codex_session_usage_in(&seam.codex, THREAD).is_none(), "and reports no usage");

    // The positive control that makes both absences mean something: the very
    // same bytes under the PLAIN name do read. Without this the assertions
    // above would pass against a lookup that found nothing for any reason at
    // all — a wrong root, a broken walk, a fixture written to the wrong date.
    write_rollout(&seam.codex, &usage_line(r, r, r));
    let u = codex_session_usage_in(&seam.codex, THREAD).expect("the plain sibling reads");
    assert_eq!(u.tokens.input_tokens, 900);
}

#[test]
fn a_header_only_rollout_reports_no_usage_not_zero() {
    // codex creates the rollout file at its first `persist()` and writes the
    // header before any response completes, so "spawned and never answered" is
    // the ordinary case. A zero must not pre-empt the statusline fallback or
    // overwrite captured history with four zero counters.
    let (reg, _d, seam) = codex_registry();
    let g = reg.create_group("C:/tmp/codex-repo", rails("codex")).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let mut entry = w.clone();
    entry.session_id = Some(THREAD.to_string());
    write_rollout(&seam.codex, "");

    // The file really is there and really is empty of usage — so `none` below
    // is the guard firing, not the lookup missing.
    assert!(
        find_codex_session_file(&seam.codex, &PathSegment::parse(THREAD).unwrap()).is_some(),
        "fixture: the rollout exists"
    );
    let u = codex_session_usage_in(&seam.codex, THREAD).expect("and it reads");
    assert_eq!(u.tokens.total(), 0);

    let snap = reg.compute_usage_snapshot(&entry, "codex");
    assert_eq!(snap.source, "none", "not `codex-transcript` with four zeros");
    assert_eq!(snap.input_tokens, 0);
    assert_eq!(snap.cost_usd, None);
}

// ---------------------------------------------------------------------------
// The cursor: two harnesses, one cache
// ---------------------------------------------------------------------------

#[test]
fn a_codex_cursor_and_a_claude_cursor_never_serve_each_others_totals() {
    // The operands must COLLIDE for this to be fail-able: the cursor cache is
    // keyed on (kind, root, session id), so a fixture using two different roots
    // would hold under an implementation that dropped `kind` from the key
    // entirely. Both stores are therefore built under ONE root directory, under
    // ONE session id — which they can be, because their layouts do not overlap
    // (`<root>/<encoded-cwd>/<id>.jsonl` for claude, `<root>/YYYY/MM/DD/` for
    // codex).
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();

    // claude's transcript for this id: one assistant message, 11 tokens.
    let proj = root.join("C--tmp-mixed");
    fs::create_dir_all(&proj).unwrap();
    fs::write(
        proj.join(format!("{THREAD}.jsonl")),
        "{\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"model\":\"claude-opus-4-8\",\
         \"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n",
    )
    .unwrap();

    // codex's rollout for the SAME id, in the SAME root: 800 tokens.
    let r = Usage { input: 700, output: 100, ..Usage::default() };
    write_rollout(&root, &usage_line(r, r, r));

    let cursors = TranscriptCursors::default();
    let claude = cursors
        .session_usage(TranscriptKind::Claude, &root, THREAD)
        .expect("claude's transcript is there");
    let codex = cursors
        .session_usage(TranscriptKind::Codex, &root, THREAD)
        .expect("and so is codex's rollout");

    assert_eq!(claude.tokens.total(), 11, "the claude cursor serves the claude transcript");
    assert_eq!(codex.tokens.total(), 800, "the codex cursor serves the codex rollout");

    // Order-independence: asking again in the other order must give the same
    // two answers, so neither reading can be an artefact of which cursor was
    // built first into a shared entry.
    assert_eq!(
        cursors.session_usage(TranscriptKind::Codex, &root, THREAD).unwrap().tokens.total(),
        800
    );
    assert_eq!(
        cursors.session_usage(TranscriptKind::Claude, &root, THREAD).unwrap().tokens.total(),
        11
    );
    // Non-vacuity: the two fixtures really do disagree, so a cache that served
    // one out of the other would be caught rather than coincidentally right.
    assert_ne!(claude.tokens.total(), codex.tokens.total());
}

// ---------------------------------------------------------------------------
// The wiring: which source a codex agent's usage comes from
// ---------------------------------------------------------------------------

fn rails(cli: &str) -> Guardrails {
    Guardrails {
        max_agents: 4,
        agent_cli: cli.into(),
        blocks: workflow::default_roster(&[
            (Role::Orchestrator, "", ""),
            (Role::Worker, "", ""),
            (Role::Reviewer, "", ""),
        ]),
        auto_ops: false,
        idle_kill_minutes: 0,
        max_spawns_per_hour: 0,
        watchdog_stall_minutes: 0,
        ..Guardrails::default()
    }
}

/// A roster whose worker CLASS defaults to claude and whose SECOND worker block
/// runs codex — the #2167 shape, in the direction this slice adds.
fn rails_second_worker_block_is_codex() -> Guardrails {
    let mut g = rails("claude");
    g.blocks.push(workflow::Block {
        id: "worker-codex".into(),
        name: "worker-codex".into(),
        kind: Role::Worker,
        cli: "codex".into(),
        model: String::new(),
        prompt: None,
        profile: None,
        allow: vec![],
        role_hint: None,
        effort: String::new(),
        context: String::new(),
        remote: None,
    });
    g
}

/// A temp root bound to BOTH the codex seam and the claude one, for the calling
/// thread only. Binding claude too is not incidental: `the_codex_arm_never_
/// fires_for_another_cli` drives the claude arm deliberately, and an unbound
/// claude root would send it to the maintainer's real `~/.claude`, where it
/// would miss for the wrong reason and the assertion would pass while proving
/// nothing.
struct Seam {
    _tmp: tempfile::TempDir,
    codex: PathBuf,
}

fn seam() -> Seam {
    let tmp = tempfile::tempdir().unwrap();
    let codex = tmp.path().join("codex-sessions");
    let claude = tmp.path().join("claude-projects");
    fs::create_dir_all(&codex).unwrap();
    fs::create_dir_all(&claude).unwrap();
    set_codex_sessions_root_for_test(Some(codex.clone()));
    set_claude_projects_root_for_test(Some(claude));
    Seam { _tmp: tmp, codex }
}

impl Drop for Seam {
    fn drop(&mut self) {
        // Restored from `Drop` rather than at the end of each test: a failing
        // test would otherwise leave the seam bound for whatever the harness
        // runs next on this thread (CLAUDE.md's `lock_safe` bullet, same
        // hazard one mechanism over).
        set_codex_sessions_root_for_test(None);
        set_claude_projects_root_for_test(None);
    }
}

fn test_registry() -> (OrchRegistry, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let reg = OrchRegistry::new(dir.path().to_path_buf());
    reg.set_port(45991);
    reg.set_claude_projects_dir(dir.path().join("claude-projects"));
    reg.set_claude_agents_dir_override(dir.path().join("claude-agents"));
    reg.set_copilot_agents_dir_override(dir.path().join("copilot-agents"));
    reg.set_compact_hook_dir_override(dir.path().join("compacthook"));
    reg.set_copilot_hooks_dir_override(dir.path().join("copilot-hooks"));
    (reg, dir)
}

/// [`test_registry`] plus the codex store seam, which the arm under test reads
/// through `sessions::codex_sessions_root()`.
fn codex_registry() -> (OrchRegistry, tempfile::TempDir, Seam) {
    let seam = seam();
    let (reg, dir) = test_registry();
    // The seam really is what the production arm will read — asserted rather
    // than assumed, because a thread-local bound on the wrong thread is
    // invisible and would make every arm assertion below pass as `none`.
    assert_eq!(codex_sessions_root().as_deref(), Some(seam.codex.as_path()));
    (reg, dir, seam)
}

/// The proof the #464 allowlist row for this file names (`tests/orchestration.rs`,
/// `no_registry_construction_bypasses_the_test_agent_dir_overrides`). That row
/// permits ONE raw `OrchRegistry::new` here, and it permits it only because
/// `test_registry` applies every agent/hook dir override — without which a spawn
/// in this file writes a generated agent file into the maintainer's real
/// `~/.claude` or `~/.copilot`. A textual scan cannot see that, so the row would
/// otherwise be pure trust; this makes it a claim that fails in THIS binary the
/// moment the helper stops applying one.
#[test]
fn its_registry_helper_applies_every_override_this_allowlist_row_assumes() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/codexusage.rs"),
    )
    .expect("this file reads itself");

    let start = src
        .find("fn test_registry() -> (OrchRegistry, tempfile::TempDir) {")
        .expect("the sanctioned helper must exist, under the name the row names");
    let body = &src[start..];
    let end = body.find("\n}").expect("the helper must terminate") + 2;
    let body = &body[..end];

    for needed in [
        "set_claude_agents_dir_override",
        "set_copilot_agents_dir_override",
        "set_compact_hook_dir_override",
        "set_copilot_hooks_dir_override",
    ] {
        assert!(
            body.contains(needed),
            "the #464 allowlist row for tests/codexusage.rs assumes this helper applies every \
             override; it no longer applies {needed}, so a registry built through it can reach \
             the real agent dirs and the row's premise is gone"
        );
    }

    // The population control: the extraction really did isolate the helper, so
    // the four assertions above are about ITS body and not about the whole file
    // — which contains those same names in prose above.
    assert!(
        body.len() < 1_200,
        "the helper's body extraction ran away ({} chars); the assertions above would then be \
         satisfied by any other function in this file",
        body.len()
    );
    assert!(
        !body.contains("#[test]"),
        "the extraction swallowed a test, so it is no longer reading only the helper"
    );
}

#[test]
fn a_codex_agents_usage_comes_from_the_humans_own_store_and_is_estimated_not_reported() {
    let (reg, _d, seam) = codex_registry();
    let g = reg.create_group("C:/tmp/codex-repo", rails("codex")).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    // codex has NO public pre-mint flag, so unlike claude and pi the spawn
    // carries no session id: the store watcher binds one after the pane's first
    // turn. Until then this arm is idle — pinned separately below.
    assert!(
        w.session_id.is_none(),
        "fixture: codex does not premint, so the arm's idle case is the ordinary one"
    );
    let mut entry = w.clone();
    entry.session_id = Some(THREAD.to_string());

    let r1 = Usage { input: 21_650, cached: 1_328, cache_write: 64, output: 121, reasoning: 78 };
    let r2 = Usage { input: 900, output: 90, ..Usage::default() };
    write_rollout(
        &seam.codex,
        &format!(
            "{}{}{}",
            turn_context("gpt-5.1-codex-max"),
            usage_line(r1, r1, r1),
            usage_line(r2, r2, r2),
        ),
    );

    let snap = reg.compute_usage_snapshot(&entry, "codex");
    assert_eq!(snap.source, "codex-transcript", "usage must come from the codex rollout store");
    assert_eq!(snap.input_tokens, (21_650 - 1_328 - 64) + 900, "fresh input, both details out");
    assert_eq!(snap.output_tokens, 121 + 90, "and `reasoning` is not added to it");
    assert_eq!(snap.cache_read_tokens, 1_328);
    assert_eq!(snap.cache_creation_tokens, 64);
    assert_eq!(
        snap.input_tokens + snap.output_tokens + snap.cache_read_tokens + snap.cache_creation_tokens,
        r1.total() + r2.total(),
        "the four buckets sum to codex's own `total_tokens`"
    );
    assert_eq!(snap.cost_usd, None, "codex writes no dollars and no codex model is priced");
    assert!(snap.estimated, "any dollar figure here would be ours, not codex's");
    assert_eq!(snap.model.as_deref(), Some("gpt-5.1-codex-max"));
}

#[test]
fn a_codex_agent_whose_session_is_not_bound_yet_reports_no_usage() {
    // A rollout with real spend in it — but nothing ties this agent to that
    // thread, and guessing (newest rollout, only rollout) would attribute
    // another pane's conversation in a store several panes of one group share.
    let (reg, _d, seam) = codex_registry();
    let g = reg.create_group("C:/tmp/codex-repo", rails("codex")).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let r = Usage { input: 44_444, output: 4_444, ..Usage::default() };
    write_rollout(&seam.codex, &usage_line(r, r, r));

    assert!(w.session_id.is_none(), "fixture: the watcher has not bound one");
    let snap = reg.compute_usage_snapshot(&w, "codex");
    assert_eq!(snap.source, "none");
    assert_eq!(snap.input_tokens, 0);

    // The positive control: the very same store DOES answer once an id is
    // bound, so `none` above is the arm declining to guess rather than the
    // fixture being unreadable.
    let mut bound = w.clone();
    bound.session_id = Some(THREAD.to_string());
    assert_eq!(reg.compute_usage_snapshot(&bound, "codex").source, "codex-transcript");
}

#[test]
fn a_missing_codex_store_leaves_the_agent_at_zero_rather_than_wedging() {
    let (reg, _d, seam) = codex_registry();
    let g = reg.create_group("C:/tmp/codex-repo", rails("codex")).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let mut entry = w.clone();
    entry.session_id = Some(THREAD.to_string());
    fs::remove_dir_all(&seam.codex).ok();

    let snap = reg.compute_usage_snapshot(&entry, "codex");
    assert_eq!(snap.source, "none");
    assert_eq!(snap.cost_usd, None);
}

#[test]
fn the_codex_arm_never_fires_for_another_cli() {
    let (reg, _d, seam) = codex_registry();
    let g = reg.create_group("C:/tmp/mixed-repo", rails("claude")).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "task", false, None).unwrap();
    let sid = w.session_id.clone().expect("claude premints");

    // A codex rollout in the human's store under this very session id. A
    // mixed-CLI group is the normal case and the store is the human's, not the
    // group's, so a claude agent must not be charged out of it just because the
    // ids line up.
    write_raw(
        &seam.codex,
        &rollout_name(&sid),
        &format!(
            "{}{}",
            header(&sid, "C:/tmp/mixed-repo"),
            usage_line(
                Usage { input: 55_555, output: 5_555, ..Usage::default() },
                Usage::default(),
                Usage::default()
            )
        ),
    );

    let snap = reg.compute_usage_snapshot(&w, "claude");
    assert_ne!(snap.source, "codex-transcript");
    assert_eq!(snap.input_tokens, 0);

    // The positive control that makes the two assertions above mean the ARM
    // declined rather than the fixture being unreadable: the same store, the
    // same id, read as codex, does answer.
    let codex_snap = reg.compute_usage_snapshot(&w, "codex");
    assert_eq!(codex_snap.source, "codex-transcript");
    assert_eq!(codex_snap.input_tokens, 55_555);
}

#[test]
fn a_codex_pane_in_a_second_block_of_its_class_is_read_as_codex_not_as_the_class_default() {
    // #2167's shape, in this slice's direction. The class default for Worker is
    // claude; the pane's own block runs codex. Resolving the CLI from the ROLE
    // would hand `compute_usage_snapshot` "claude", its codex arm would never
    // run, and the row would land as `none` with four zero counters — for a
    // rollout sitting in the store the whole time.
    let (reg, _d, seam) = codex_registry();
    let g = reg
        .create_group("C:/tmp/codex-second-block", rails_second_worker_block_is_codex())
        .unwrap();

    let rails = reg.group(&g.id).unwrap().guardrails;
    assert_eq!(rails.cli_for(Role::Worker), "claude", "fixture: the class default disagrees");
    assert_eq!(rails.cli_for_block("worker-codex", Role::Worker), "codex");

    let r = Usage { input: 3_000, output: 300, ..Usage::default() };
    write_rollout(&seam.codex, &usage_line(r, r, r));

    let w = reg
        .spawn_agent_ex(
            &g.id,
            Role::Worker,
            Some("worker-codex".into()),
            "w",
            "task",
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
    let mut entry = w.clone();
    entry.session_id = Some(THREAD.to_string());

    let snap = reg.compute_usage_snapshot(&entry, rails.cli_for_block("worker-codex", Role::Worker));
    assert_eq!(snap.source, "codex-transcript");
    assert_eq!(snap.input_tokens, 3_000);
}
