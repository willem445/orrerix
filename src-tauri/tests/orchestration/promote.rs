//! Generated agent files, atomic writes, the low-disk backstop and promoting a pane to orchestrator.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ─────────── #502: generated custom-agent file lifecycle ───────────
//
// The incident: 1,219 generated agent files (67MB) had accumulated in
// `~/.claude/agents` against 13 live groups, enough to trip Claude Code's
// own aggregate agent-description token cap and degrade every session that
// loaded the roster.
//
// The cause was that loomux's own test suite could write into the real
// `~/.claude/agents` at all; #464's startup sweep reclaims what was already
// written, and these cover the write side plus `end_group`'s own reclaim.
//
// Where a case needs a file "loomux wrote", it COPIES one a real spawn
// genuinely generated rather than hand-authoring the bytes — cheap, and it
// keeps the fixture honest if the generated shape ever changes.

/// The Claude-side generated file a default-roster spawn just wrote.
fn generated_claude_agent_file(dir: &tempfile::TempDir, group: &GroupId, block: &str) -> std::path::PathBuf {
    let path = dir.path().join("claude-agents").join(format!("loomux-{group}-{block}.md"));
    assert!(path.is_file(), "the spawn must have generated {path:?}");
    path
}

#[test]
fn end_group_reclaims_a_generated_file_no_member_entry_accounts_for() {
    // #502: teardown used to remove one file per LIVE MEMBER entry, so any
    // file the member list didn't name survived — a block retired by a
    // roster change, or a member entry pruned while its file stayed behind.
    // Ownership is a property of the FILE, not of who happens to still be
    // in the roster, so the reclaim scans the directory instead.
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/claude-repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    let live = generated_claude_agent_file(&dir, &g.id, &w.block);
    let retired = live.with_file_name(format!("loomux-{}-retired.md", g.id));
    fs::copy(&live, &retired).unwrap();

    reg.end_group(&g.id, false).unwrap();
    assert!(
        !retired.exists(),
        "a generated file this group owns must be reclaimed even when no member entry names its block",
    );
    assert!(!live.exists(), "and the member's own file still goes, as before");
}

#[test]
fn end_group_leaves_a_generated_file_a_more_specific_live_group_owns() {
    // Nothing stops one group id from being a prefix of another. With `X`
    // and `X-extra` both live, `loomux-X-extra-worker.md` is X-extra's
    // worker file — not X's file for a block named `extra-worker`. The
    // filename alone can't tell them apart (both readings are well-formed),
    // so ownership is resolved against the group registry, longest match
    // wins. Naive prefix matching would delete a LIVE group's file here.
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/claude-repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    let live = generated_claude_agent_file(&dir, &g.id, &w.block);
    // A second, MORE specific group — its state dir under the orchestration
    // root is what makes it a live group as far as the registry is concerned.
    let nested = format!("{}-extra", g.id);
    fs::create_dir_all(dir.path().join(&nested)).unwrap();
    let nested_file = live.with_file_name(format!("loomux-{nested}-worker.md"));
    fs::copy(&live, &nested_file).unwrap();

    reg.end_group(&g.id, false).unwrap();
    assert!(
        nested_file.is_file(),
        "ending group {} must not reclaim a file the longer-id group {nested} owns",
        g.id,
    );
    assert!(!live.exists(), "its own file still goes");
}

#[test]
fn an_unreadable_group_registry_stops_end_group_reclaiming_anything() {
    // rev-38 review (B2), carried onto the teardown path: ownership is decided
    // by asking which live group CLAIMS a file, so an EMPTY group list says
    // "nothing claims anything" — which would dissolve the longest-match
    // protection below and let `end_group("X")` delete a live `X-extra`
    // group's files. `sweep_orphaned_agent_files` already applies this rule
    // to itself (#464 review B3, `..._deletes_nothing_when_group_enumeration_
    // fails` above); this pins that the reclaim path agrees rather than
    // holding a second opinion.
    //
    // The failure is produced for real, not mocked: a regular FILE occupies
    // the root's path, so `read_dir` genuinely errors.
    //
    // Driven straight at the reclaim rather than through `end_group`, for a
    // reason worth recording: `end_group` kills its members first, and
    // `mark_dead` audits, and `append_audit` does `create_dir_all(root/
    // <group>)` — so a merely-DELETED root is recreated before the reclaim
    // ever runs. Going through the front door here would have produced a
    // test that passes because enumeration quietly succeeded, which is worse
    // than no test at all.
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/claude-repo", rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();
    let live = generated_claude_agent_file(&dir, &g.id, &w.block);

    // A registry over the SAME agent dirs whose root can never be enumerated.
    let broken_root = dir.path().join("root-is-a-regular-file");
    fs::write(&broken_root, "not a directory").unwrap();
    let broken = OrchRegistry::new(broken_root); // containment-pin (#502)
    broken.set_claude_agents_dir_override(dir.path().join("claude-agents"));
    broken.set_copilot_agents_dir_override(dir.path().join("copilot-agents"));

    let removed = broken.reclaim_group_agent_files(&g.id);
    assert!(removed.is_empty(), "an unenumerable registry proves nothing about ownership: {removed:?}");
    assert!(
        live.is_file(),
        "an unenumerable group registry must never cost a LIVE group its contract: {live:?}",
    );
}

#[test]
fn a_dead_group_never_gets_its_agent_file_re_minted() {
    // #502's ROOT CAUSE half, live-observed: after the orphans were deleted
    // by hand, files for long-dead groups reappeared within minutes. Whatever
    // reaches a writer with a dead group id — a stale roster, a restore
    // index, a future periodic refresh — must fail to resurrect its file,
    // or every sweep in this file is just losing a race.
    //
    // The guard is at the WRITER (`group_state_exists`), so this pins the
    // property for every caller at once rather than for one code path.
    let (reg, dir) = test_registry();
    let g = reg.create_group("C:/tmp/claude-repo", rails()).unwrap();
    let b = g.guardrails.block("worker").expect("the default roster declares a worker block");
    let generated = dir.path().join("claude-agents").join(format!("loomux-{}-{}.md", g.id, b.id));

    // A live group writes, as always.
    let inject = reg.persona_inject(&g.id, b, "claude", None, "CONTRACT");
    assert!(inject.claude_agent.is_some(), "a live group must still get its generated agent file");
    assert!(generated.is_file(), "{generated:?}");

    // The group's state is gone (ended, crashed, deleted out from under
    // loomux) and its file is reclaimed...
    fs::remove_dir_all(dir.path().join(g.id.as_str())).unwrap();
    fs::remove_file(&generated).unwrap();

    // ...and a writer reached with that same dead id must NOT bring it back.
    let inject = reg.persona_inject(&g.id, b, "claude", None, "CONTRACT");
    assert!(
        !generated.exists(),
        "a dead group's agent file must never be re-minted — that race is what made the sweeps lose",
    );
    assert!(
        inject.claude_agent.is_none(),
        "and the spawn must fall back rather than claim a handle for a file that was not written",
    );
}

#[test]
fn a_registry_outside_the_default_root_never_writes_into_the_users_home() {
    // #502's re-mint SOURCE, proven live: loomux's own test suite constructs
    // throwaway registries 50+ times, only the shared `test_registry` helper
    // set the agents-dir override, and every other test that spawned an agent
    // minted a real multi-KB file in the developer's `~/.claude/agents` that
    // nothing ever deleted. Group ids are `<repo-dir-name>-<hash>`, which is
    // why the accumulated files were named for `tempfile` temp dirs and the
    // fake `C:/tmp/repo` path — they were never a user's groups at all.
    //
    // So this test deliberately builds the registry the UNGUARDED way — no
    // override, exactly like those 50+ sites — and pins that the containment
    // rule holds anyway. "Every test remembers to opt out of writing to your
    // home directory" is not a property anyone can maintain.
    let dir = tempfile::tempdir().unwrap();
    let reg = OrchRegistry::new(dir.path().to_path_buf()); // containment-pin (#502)
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/claude-repo", rails()).unwrap();
    let b = g.guardrails.block("worker").unwrap();

    let inject = reg.persona_inject(&g.id, b, "claude", None, "CONTRACT");
    let handle = inject.claude_agent.expect("the contract must still be delivered by file");

    // It went somewhere inside this registry's OWN root...
    let contained = dir.path().join("claude-agents").join(format!("{handle}.md"));
    assert!(contained.is_file(), "a non-default-root registry must write inside its own root: {contained:?}");
    // ...and nowhere near the real user directory. (Read the home directory
    // from the environment rather than the lib's `dirs` crate — an
    // integration test asserting about the USER's home should look it up the
    // same way anything else outside the lib would.)
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    if let Some(home) = home {
        let leaked = PathBuf::from(home).join(".claude").join("agents").join(format!("{handle}.md"));
        assert!(!leaked.exists(), "a throwaway registry must never write into the user's real agent dir: {leaked:?}");
    }
}

#[test]
fn a_throwaway_registry_never_rewrites_the_users_real_copilot_hook_config() {
    // Same containment rule, the other direction it leaked: auditing every
    // `dirs::home_dir()` in this module after the agent-file leak turned up
    // one more — `copilot_hooks_dir` (`compact_hook_dir` was already
    // root-relative). Much smaller blast radius: ONE idempotent file the
    // real app rewrites anyway, never one per group, so it never
    // accumulated the way the agent files did. But a throwaway registry
    // still has no business rewriting the user's real Copilot hook config,
    // and it was observed doing exactly that during a test run.
    //
    // Built the UNGUARDED way on purpose (no override), like the test in
    // this file that pins the agent-dir half.
    let dir = tempfile::tempdir().unwrap();
    let reg = OrchRegistry::new(dir.path().to_path_buf()); // containment-pin (#502)
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/copilot-repo", copilot_rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    // Asserting the CONTAINED path exists is what proves the resolver never
    // reached for the home directory — the real `~/.copilot/hooks` file
    // legitimately exists on a developer's box (the app writes it), so its
    // mere presence would prove nothing either way.
    let contained = dir.path().join("copilot-home").join("hooks").join("loomux-compact.json");
    assert!(
        contained.is_file(),
        "a copilot spawn's hook config must land inside this registry's own root: {contained:?}",
    );
}

#[test]
fn a_throwaway_registry_never_writes_the_users_real_copilot_trusted_folders() {
    // rev-38 review (B1): the THIRD uncontained home write, and the worst of
    // the three. `pre_trust_copilot_folder` appends the spawn's workspace
    // path to `trustedFolders` in the user's real `~/.copilot/config.json`.
    //
    // Two things make it worse than the agent files it shipped alongside:
    // `trustedFolders` is a SECURITY setting (it suppresses Copilot's
    // folder-trust prompt), and the entries are per-workspace PATHS — so a
    // suite spawning into fresh temp dirs appends a NEW entry every run and
    // grows without bound. The real config on the machine this was found on
    // had already collected 36 such entries.
    //
    // It was missed by the first audit because it was a free function with no
    // registry to ask, which is why the containment rule now lives in one
    // predicate every home-resolving path consults.
    let dir = tempfile::tempdir().unwrap();
    let reg = OrchRegistry::new(dir.path().to_path_buf()); // containment-pin (#502)
    reg.set_port(45999);
    let g = reg.create_group("C:/tmp/copilot-repo", copilot_rails()).unwrap();
    reg.spawn_agent(&g.id, Role::Worker, "w", "t", false, None).unwrap();

    // Asserting the CONTAINED file exists is what proves the resolver never
    // reached for the home directory — the real `~/.copilot/config.json`
    // legitimately exists on a developer's box, so its presence proves
    // nothing either way.
    let contained = dir.path().join("copilot-home").join("config.json");
    assert!(
        contained.is_file(),
        "a copilot spawn's folder-trust write must land inside this registry's own root: {contained:?}",
    );
    let text = fs::read_to_string(&contained).unwrap();
    assert!(
        text.contains("trustedFolders") && text.contains("C:/tmp/copilot-repo"),
        "and it must be the real pre-trust write, not an empty file: {text}",
    );

    // #802 added a SECOND, documented permission surface to the same write —
    // it must be contained by the same rule, or the containment audit this
    // test exists for would have a fresh hole the day it shipped.
    let perms = dir.path().join("copilot-home").join("permissions-config.json");
    assert!(
        perms.is_file(),
        "the permissions-config write must land inside this registry's own root too: {perms:?}",
    );
    let perms_text = fs::read_to_string(&perms).unwrap();
    assert!(
        perms_text.contains("C:/tmp/copilot-repo") && perms_text.contains("orrerix"),
        "and carry the real grant, not an empty file: {perms_text}",
    );
}

#[test]
fn hold_guard_proceeds_immediately_when_quiet() {
    // No keystrokes recorded (0) → the loop never holds and never reports.
    let quiet = Duration::from_millis(50);
    let cap = Duration::from_secs(5);
    let poll = Duration::from_millis(5);
    assert_eq!(hold_until_quiet(|| 0, quiet, cap, poll), None);
}

#[test]
fn hold_guard_caps_so_reports_are_not_starved() {
    // u64::MAX = "typed in the future" → always inside the quiet window, so the
    // ONLY way the loop can exit is the max-hold cap. That it returns at all
    // proves the starvation backstop fires; the value proves it held ~the cap.
    let cap = Duration::from_millis(40);
    let held = hold_until_quiet(|| u64::MAX, Duration::from_millis(50), cap, Duration::from_millis(5))
        .expect("a capped hold must report its held duration");
    assert!(held >= 30, "must have held near the cap before delivering, got {held}ms");
    assert!(held < 2000, "cap must bound the hold, got {held}ms");
}

#[test]
fn hold_guard_releases_once_the_human_goes_quiet() {
    use std::sync::atomic::{AtomicU64, Ordering};
    // "Typing" (future stamp) for the first few polls, then an ancient stamp
    // (quiet). The loop must hold while typing, then release well before the
    // cap — exercising the poll loop that consults should_hold_for_user, not
    // just the pure decision (the #40 wiring lesson).
    let calls = AtomicU64::new(0);
    let source = move || {
        if calls.fetch_add(1, Ordering::Relaxed) < 3 { u64::MAX } else { 1 }
    };
    let held = hold_until_quiet(source, Duration::from_millis(50), Duration::from_secs(5), Duration::from_millis(5))
        .expect("it must release once the human goes quiet");
    assert!(held < 4000, "must release on quiet, not ride the cap, got {held}ms");
}

// --- #111 pre-paste human-input hold: the loop that drives the pure gate ---

pub(crate) const HB_POLL: Duration = Duration::from_millis(5);

#[test]
fn paste_hold_proceeds_immediately_when_box_is_empty() {
    // Not pending → the box is empty; paste at once with no hold.
    let out = hold_for_human_input(|| false, Duration::from_secs(5), HB_POLL);
    assert_eq!(out, PasteDecision::Paste { held_ms: 0 });
}

#[test]
fn paste_hold_aborts_when_the_line_never_clears() {
    // A human line sits and they never submit/clear it: the box stays pending for
    // the whole bounded wait → abort rather than merge-submit. A small cap keeps
    // the test fast. Importantly, the decision is independent of any output the
    // pane streams meanwhile (finding #1: ambient output can't false-clear it).
    let cap = Duration::from_millis(40);
    let out = hold_for_human_input(|| true, cap, HB_POLL);
    match out {
        PasteDecision::Abort { held_ms } => {
            assert!(held_ms >= 30, "must have held near the cap before aborting, got {held_ms}ms");
            assert!(held_ms < 2000, "cap must bound the hold, got {held_ms}ms");
        }
        other => panic!("expected Abort, got {other:?}"),
    }
}

#[test]
fn paste_hold_releases_once_the_human_submits() {
    use std::sync::atomic::{AtomicU64, Ordering};
    // The line sits for the first few polls, then the human presses Enter and the
    // pending flag flips false: the loop must release with Paste — exercising the
    // poll loop, not just the pure gate (#40 lesson).
    let polls = AtomicU64::new(0);
    let pending = move || polls.fetch_add(1, Ordering::Relaxed) < 3;
    let out = hold_for_human_input(pending, Duration::from_secs(5), HB_POLL);
    match out {
        PasteDecision::Paste { held_ms } => {
            assert!(held_ms < 4000, "must release on submit, not ride the cap, got {held_ms}ms");
        }
        other => panic!("expected Paste after the submit, got {other:?}"),
    }
}

#[test]
fn output_growth_never_flips_input_pending() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    // Finding #1's real property, tested end-to-end rather than tautologically:
    // occupancy responds ONLY to keystroke content — output arriving never clears
    // a sitting line. `feed` mirrors write_pty's exact flag update (pty.rs); the
    // `output` counter models the output pump, which — like the real pump — has no
    // reference to the flag. We interleave large output growth with keystrokes and
    // assert the flag tracks the keystrokes alone.
    let pending = AtomicBool::new(false);
    let output = AtomicU64::new(0);
    let feed = |data: &str| match classify_human_input(data) {
        HumanInput::Content => pending.store(true, Ordering::Relaxed),
        HumanInput::Submit => pending.store(false, Ordering::Relaxed),
        HumanInput::Neutral => {}
    };

    // Human types a line → pending.
    feed("dfgdsfg");
    assert!(pending.load(Ordering::Relaxed));
    // The pane streams a massive burst of output (agent mid-turn, keystroke
    // redraws — the old ≥24-byte false-clear source). It cannot touch the flag.
    output.fetch_add(1_000_000, Ordering::Relaxed);
    assert!(pending.load(Ordering::Relaxed), "output growth must not clear a sitting line");
    // The delivery guard, reading only the flag, holds to the cap and aborts —
    // never merge-submitting the still-sitting line, whatever the pane printed.
    let out = hold_for_human_input(|| pending.load(Ordering::Relaxed), Duration::from_millis(40), HB_POLL);
    assert!(matches!(out, PasteDecision::Abort { .. }), "sitting line must not paste: {out:?}");
    // Only an Enter clears it; more output in between changes nothing.
    output.fetch_add(1_000_000, Ordering::Relaxed);
    feed("\r");
    assert!(!pending.load(Ordering::Relaxed), "only a submit keystroke clears the flag");
    let out2 = hold_for_human_input(|| pending.load(Ordering::Relaxed), Duration::from_secs(60), HB_POLL);
    assert_eq!(out2, PasteDecision::Paste { held_ms: 0 });
}

#[test]
fn sub_floor_submit_does_not_wedge_future_deliveries() {
    // Finding #2 (adversarial ordering): a human submit whose output burst is tiny
    // (empty Enter, short command) must not leave the box "pending" forever and
    // wedge every later delivery in a 60s hold→abort loop. With keystroke-content
    // tracking, the Enter positively clears occupancy: classify a sub-floor submit
    // as Submit, and a delivery consulting the resulting (false) flag pastes at
    // once with no hold.
    assert_eq!(classify_human_input("\r"), HumanInput::Submit); // empty Enter
    assert_eq!(classify_human_input("q\r"), HumanInput::Submit); // one-char command + Enter
    // The flag those submits leave (false) drives an immediate paste — no wedge.
    let out = hold_for_human_input(|| false, Duration::from_secs(60), HB_POLL);
    assert_eq!(out, PasteDecision::Paste { held_ms: 0 });
}

#[test]
fn box_occupancy_delta_counts_typed_characters_and_backspaces() {
    // #171: the counter half of occupancy tracking. Printable content adds,
    // backspace/DEL removes, everything else nets zero.
    assert_eq!(box_occupancy_delta("a"), 1);
    assert_eq!(box_occupancy_delta("hello"), 5);
    assert_eq!(box_occupancy_delta("\u{7f}"), -1, "DEL removes one character");
    assert_eq!(box_occupancy_delta("\u{08}"), -1, "BS removes one character too");
    assert_eq!(box_occupancy_delta("\u{7f}\u{7f}\u{7f}"), -3, "three backspaces in one write");
    // Arrows and other CSI sequences are pure navigation — no occupancy change.
    assert_eq!(box_occupancy_delta("\u{1b}[C"), 0); // right arrow
    assert_eq!(box_occupancy_delta("\u{1b}[A"), 0); // up arrow
    assert_eq!(box_occupancy_delta(""), 0);
    // A bracketed paste's markers are CSI-shaped and skipped; only the pasted
    // text itself counts.
    assert_eq!(box_occupancy_delta("\u{1b}[200~hi\u{1b}[201~"), 2);

    // #179 regression guard: a terminal query-reply echo must never read as a
    // removal OR an addition, exactly like it must never read as Content.
    assert_eq!(box_occupancy_delta("\x1b]11;rgb:0d0d/1111/1717\x07"), 0);
    assert_eq!(box_occupancy_delta("\x1bP>|xterm(370)\x1b\\"), 0);
}

#[test]
fn box_occupancy_delta_counts_multibyte_characters_not_bytes() {
    // #171 review follow-up: counting raw UTF-8 BYTES over-counted non-ASCII
    // input — a 3-byte CJK character or 4-byte emoji added 3/4 to the
    // counter for one keystroke, but the single backspace that deletes it
    // only ever subtracts 1 (backspace/DEL is always a single control byte,
    // regardless of what it deletes). That mismatch reproduced #171's exact
    // stuck-occupied symptom for anyone typing non-ASCII: the counter could
    // never get back down to zero by backspacing alone.
    //
    // "日" (U+65E5) is a 3-byte UTF-8 sequence — one character, one keystroke.
    assert_eq!(box_occupancy_delta("日"), 1, "one CJK character is one occupancy unit, not 3 bytes");
    // "😀" (U+1F600) is a 4-byte UTF-8 sequence — same story.
    assert_eq!(box_occupancy_delta("😀"), 1, "one emoji is one occupancy unit, not 4 bytes");
    // A run of each, plus a mix, still counts one per character.
    assert_eq!(box_occupancy_delta("日本語"), 3);
    assert_eq!(box_occupancy_delta("a日😀b"), 4);
}

#[test]
fn backspacing_a_cjk_or_emoji_character_reads_the_box_as_empty_again() {
    // #171 review follow-up, the end-to-end version of the byte-vs-character
    // fix: type one non-ASCII character, backspace it once, and the box must
    // read empty — exactly like the all-ASCII case just above. Before the
    // byte-vs-character fix this got stuck (delta +3 for "日", only -1 for
    // the one backspace, net +2 forever).
    use std::sync::atomic::Ordering;
    let counter = std::sync::atomic::AtomicI64::new(0);
    let feed = |data: &str| {
        match classify_human_input(data) {
            HumanInput::Submit => counter.store(0, Ordering::Relaxed),
            HumanInput::Content | HumanInput::Neutral => {
                let delta = box_occupancy_delta(data);
                if delta != 0 {
                    let cur = counter.load(Ordering::Relaxed);
                    counter.store((cur + delta as i64).max(0), Ordering::Relaxed);
                }
            }
        }
    };
    let pending = || counter.load(Ordering::Relaxed) > 0;

    // A CJK IME commit typically arrives as one write per composed character.
    feed("日");
    assert!(pending(), "a typed CJK character must occupy the box");
    feed("\u{7f}");
    assert!(!pending(), "backspacing the one character out must read the box as empty again");

    // Same story for an emoji (4-byte UTF-8).
    feed("😀");
    assert!(pending());
    feed("\u{7f}");
    assert!(!pending(), "backspacing the one emoji out must read the box as empty again");

    // A mixed multi-character line backspaced out character-by-character.
    feed("a日😀b");
    assert!(pending());
    feed("\u{7f}");
    feed("\u{7f}");
    feed("\u{7f}");
    assert!(pending(), "three of four characters backspaced — still occupied");
    feed("\u{7f}");
    assert!(!pending(), "the fourth backspace empties a 4-character mixed-width line");
}

#[test]
fn backspacing_a_typed_line_all_the_way_out_reads_the_box_as_empty_again() {
    // #171: the incident this issue reports — start typing, backspace back out,
    // and (before this fix) `input_pending` stayed stuck true because every
    // individual backspace classified as `Neutral`, indistinguishable from an
    // arrow key. A subsequent delivery then held for the full 60s and aborted
    // instead of pasting into a pane whose box was, in fact, empty — "blocks
    // the loomux prompts" from the human's report.
    //
    // This models write_pty's fixed logic exactly (pty.rs): `Submit` resets the
    // counter to zero directly; everything else applies `box_occupancy_delta`,
    // clamped at zero. `xterm.js` delivers one write per keystroke, so three
    // typed characters and three backspaces arrive as six separate writes.
    use std::sync::atomic::Ordering;
    let counter = std::sync::atomic::AtomicI64::new(0);
    let feed = |data: &str| {
        match classify_human_input(data) {
            HumanInput::Submit => counter.store(0, Ordering::Relaxed),
            HumanInput::Content | HumanInput::Neutral => {
                let delta = box_occupancy_delta(data);
                if delta != 0 {
                    let cur = counter.load(Ordering::Relaxed);
                    counter.store((cur + delta as i64).max(0), Ordering::Relaxed);
                }
            }
        }
    };
    let pending = || counter.load(Ordering::Relaxed) > 0;

    // Human starts typing "abc" — box occupied.
    feed("a");
    feed("b");
    feed("c");
    assert!(pending(), "typed content must occupy the box");
    // ...then backspaces it all out, one keystroke at a time.
    feed("\u{7f}");
    feed("\u{7f}");
    assert!(pending(), "two of three characters backspaced — still occupied");
    feed("\u{7f}");
    assert!(!pending(), "the box is empty again once every typed character is backspaced out");

    // A delivery landing right after must paste immediately, not hold for 60s.
    let out = hold_for_human_input(pending, Duration::from_secs(60), HB_POLL);
    assert_eq!(out, PasteDecision::Paste { held_ms: 0 });

    // Over-backspacing an already-empty box must never go negative and get
    // "stuck occupied" on the next keystroke because of a lingering negative
    // counter absorbing it.
    feed("\u{7f}");
    feed("\u{7f}");
    assert!(!pending(), "backspacing past empty must clamp at zero, not go negative");
    feed("x");
    assert!(pending(), "a fresh keystroke after clamped-empty occupies the box normally");
}

// ---------- #133: atomic durable writes ----------

#[test]
fn durable_writes_round_trip() {
    // Happy path: the crash-safe temp+rename writers persist and read back
    // unchanged, so making the writes atomic didn't alter their semantics.
    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    reg.upsert_task(&g.id, "orch", None, patch(Some("first"), None, None)).unwrap();
    reg.upsert_task(&g.id, "orch", None, patch(Some("second"), None, None)).unwrap();
    let titles: Vec<String> = reg.tasks(&g.id).iter().map(|t| t.title.clone()).collect();
    assert_eq!(titles, vec!["first".to_string(), "second".to_string()]);
    reg.set_state(&g.id, r#"{"cursor":7}"#).unwrap();
    assert_eq!(reg.get_state(&g.id), r#"{"cursor":7}"#);
}

#[cfg(windows)]
#[test]
fn failed_task_write_leaves_board_intact() {
    // #133: the incident — a disk-full write over tasks.json truncated it and
    // wiped 13 live tasks. Fault-inject by making tasks.json read-only so the
    // atomic rename-over AND the direct-write fallback both fail; the previous
    // good board must survive, not come back empty.
    //
    // Windows-gated: rename-over-existing fails on a read-only *file* here — the
    // OS the incident happened on. POSIX rename keys on directory write, not
    // file perms, so this injection wouldn't bite there.
    let (reg, d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let t1 = reg.upsert_task(&g.id, "orch", None, patch(Some("keep me"), None, None)).unwrap();
    reg.upsert_task(&g.id, "orch", None, patch(Some("keep me too"), None, None)).unwrap();

    let board = d.path().join(g.id.as_str()).join("tasks.json");
    let before = fs::read_to_string(&board).unwrap();

    let mut perms = fs::metadata(&board).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&board, perms).unwrap();

    // A failed durable write must surface as an error, not silent data loss.
    let res = reg.upsert_task(&g.id, "orch", Some(&t1.id), patch(None, Some("in-progress"), None));
    assert!(res.is_err(), "a failed durable write must return Err, not swallow it");

    // The last good board is byte-for-byte intact — the whole point of #133.
    let after = fs::read_to_string(&board).unwrap();
    assert_eq!(after, before, "the previous good tasks.json survived the failed write");
    assert_eq!(reg.tasks(&g.id).len(), 2, "the board is not empty after a failed write");

    // Restore write so the TempDir can be cleaned up.
    let mut perms = fs::metadata(&board).unwrap().permissions();
    perms.set_readonly(false);
    fs::set_permissions(&board, perms).unwrap();
}

// ---------- #134: low-disk backstop ----------

#[test]
fn low_disk_transition_latches_once_with_hysteresis() {
    let (low, clear) = (5u64, 7u64);
    // Above the floor, unarmed: quiet, stays unarmed.
    assert_eq!(low_disk_transition(9, low, clear, false), (false, false));
    // Cross below → arm and fire exactly this tick.
    assert_eq!(low_disk_transition(4, low, clear, false), (true, true));
    // Still low, already armed → latched, no re-fire (one per episode).
    assert_eq!(low_disk_transition(4, low, clear, true), (true, false));
    // Recovered a little but below the clear mark → stay latched (hysteresis).
    assert_eq!(low_disk_transition(6, low, clear, true), (true, false));
    // Recovered past the clear mark → reset the latch, no fire.
    assert_eq!(low_disk_transition(7, low, clear, true), (false, false));
    // ...and a fresh dip fires again.
    assert_eq!(low_disk_transition(4, low, clear, false), (true, true));
}

#[test]
fn low_disk_notice_reports_free_space() {
    let n = low_disk_notice(3 * 1024 * 1024 * 1024 + 512 * 1024 * 1024); // 3.5 GB
    assert!(n.contains("[orrerix]"));
    assert!(n.contains("3.5 GB"), "surfaces the free space so the orchestrator can judge urgency");
    assert!(n.to_lowercase().contains("once per"), "promises one notice per episode");
}

#[test]
fn disk_tick_notifies_once_per_episode_and_skips_paused() {
    // #134: crossing below the free-space floor fires ONE audited low-disk
    // notice per group; a second sub-threshold tick stays quiet until space
    // recovers past the hysteresis mark; paused groups are skipped entirely.
    let (reg, d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();
    let low = 4 * 1024 * 1024 * 1024; // below LOW_DISK_BYTES (5 GB)
    let recovered = 9 * 1024 * 1024 * 1024; // above LOW_DISK_CLEAR_BYTES (7 GB)

    let count_low_disk = || {
        fs::read_to_string(d.path().join(g.id.as_str()).join("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains("low-disk"))
            .count()
    };

    reg.disk_tick(low);
    assert_eq!(count_low_disk(), 1, "the first dip fires exactly one notice");
    reg.disk_tick(low);
    assert_eq!(count_low_disk(), 1, "still low → latched, no second notice");
    reg.disk_tick(recovered);
    reg.disk_tick(low);
    assert_eq!(count_low_disk(), 2, "recovery re-arms the latch for the next dip");

    // A paused group is skipped: no low-disk audit accrues while paused.
    reg.disk_tick(recovered); // clear the latch
    reg.pause_group(&g.id).unwrap();
    reg.disk_tick(low);
    assert_eq!(count_low_disk(), 2, "a paused group is skipped, so no new notice");
}

#[test]
fn a_launch_that_asks_for_no_starter_workers_opens_none() {
    // #1020 item 5. The launcher's "Initial workers" field is gone, so a launch now sends
    // NO count at all — and the whole point of the change is what that absence resolves to.
    // Before this, the form defaulted to 2 and every group came up with two idle workers
    // sitting on a repo nobody had briefed them about; the human's instruction was that
    // spawning workers at startup makes no sense, so the orchestrator opens what the work
    // needs instead.
    assert_eq!(
        starter_workers(None, 4),
        0,
        "an unasked-for count must be 0 — the orchestrator decides what it needs"
    );

    // A caller that DOES ask still gets what it asked for. `PromoteConfig` is that caller,
    // and it carries its own defaulted field, so making `None` mean 0 must not quietly mean
    // "0 for everyone" — the distinction between "nobody asked" and "asked for two" is the
    // reason this takes an `Option` rather than a `u32` the launcher passes 0 in.
    assert_eq!(starter_workers(Some(2), 4), 2);

    // The cap wins over the request, unchanged: a group may never open more starters than
    // its own live-agent guardrail allows, or the launch itself would breach the ceiling the
    // human set on the same form.
    assert_eq!(starter_workers(Some(9), 4), 4, "a request above the cap is clamped to it");
    assert_eq!(starter_workers(Some(1), 0), 0, "a zero cap admits nothing, however small the ask");

    // An explicit zero and an absent value agree on the NUMBER while meaning different
    // things — pinned so a future reader does not "simplify" the Option away on the grounds
    // that both come out 0 today. They are the same only while the default is 0.
    assert_eq!(starter_workers(Some(0), 4), starter_workers(None, 4));
}

#[test]
fn create_orchestration_group_maps_resume_session_onto_the_workflow_pin() {
    use std::sync::Arc;
    // #222 rev-11 F2, at the entry point instead of one layer below it.
    //
    // `create_group_ex(.., Launch::Resume)` pins the roster, and tests/workflow.rs
    // asserts that directly. What THIS asserts is the wiring above it — that the two
    // real callers land on the right side of the switch. `create_orchestration`
    // passes no resume session (a human at the launcher, who has just been shown the
    // roster preview: read the file). `resume_orch_session` passes one (a recorded
    // session being reopened, which is nobody's consent moment: pin the roster).
    // Swap those two and the unit test still passes while the feature is inverted.
    let state = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let repo_path = repo.path().to_string_lossy().replace('\\', "/");
    let loomux = repo.path().join(".loomux");
    fs::create_dir_all(&loomux).unwrap();
    let declare = |id: &str| {
        fs::write(
            loomux.join("workflow.yml"),
            format!("version: 1\nblocks:\n  - id: {id}\n    kind: reviewer\n"),
        )
        .unwrap()
    };

    let reg = Arc::new(relaunch_registry(state.path()));
    reg.set_port(45999);
    let advanced = Guardrails { advanced_orchestrator: true, ..rails() };

    // ── SessionOrigin::Fresh ⇒ the repo's file is read ──
    declare("rev-approved");
    let launched = create_orchestration_group(&reg, &repo_path, advanced.clone(), SessionOrigin::Fresh, None, None)
        .unwrap();
    let gid = launched.group_id.clone();
    assert!(
        reg.group(&gid).unwrap().guardrails.block("rev-approved").is_some(),
        "a launch reads the workflow file — this is the roster the human was shown"
    );

    // The human ends the group, and the repo moves on underneath them: a `git pull`
    // brings a reviewer block they have never seen.
    reg.end_group(&gid, false).unwrap();
    declare("rev-never-seen");

    // ── SessionOrigin::Resume ⇒ the PERSISTED roster stands, whatever the session id is
    // (#412 rev-17: these used to be the same bool — `launch` is now independent) ──
    let (persisted_repo, persisted) = reg.load_group_file(&gid).expect("group.json");
    create_orchestration_group(
        &reg,
        &persisted_repo,
        persisted,
        SessionOrigin::Resume("11111111-2222-3333-4444-555555555555".into()),
        Some(&gid),
        None,
    )
    .expect("a resume must not fail");

    let resumed = reg.group(&gid).unwrap().guardrails;
    assert!(
        resumed.block("rev-approved").is_some(),
        "the resumed group keeps the reviewer its human approved"
    );
    assert!(
        resumed.block("rev-never-seen").is_none(),
        "a block the repo gained AFTER the launch must not join a resumed group through the \
         real entry point either — nobody consented to it"
    );

    // ...and a FRESH launch on that same repo does pick the new one up, so the pin is
    // "a resume doesn't re-read", not "loomux stopped reading the file".
    let state2 = tempfile::tempdir().unwrap();
    let reg2 = Arc::new(relaunch_registry(state2.path()));
    reg2.set_port(45999);
    let relaunched =
        create_orchestration_group(&reg2, &repo_path, advanced, SessionOrigin::Fresh, None, None).unwrap();
    assert!(
        reg2.group(&relaunched.group_id).unwrap().guardrails.block("rev-never-seen").is_some(),
        "editing the workflow and launching again must pick up the new roster"
    );
}

// ── #407: promoting a standalone pane to orchestrator ───────────────────────
//
// No real CLI is ever launched (CLAUDE.md constraint 3): the promote path is
// driven through `promote_to_orchestrator_sync` and the composed `SpawnRequest`
// is INSPECTED, never executed, exactly as every other spawn-composition test
// in this file does. The claude session store the validation reads is a
// fixtured temp directory (`fixture_claude_session` + the thread-local override
// `set_claude_projects_root_for_test`), the same seam the orchestrator-resume
// tests above use.

/// A promotable pane: a real repo directory plus a claude session fixtured into
/// the store `promote_to_orchestrator_sync`'s existence check reads. The
/// override is thread-local to the calling test; the caller clears it with
/// [`drop_claude_store`] once the promote under test has run.
fn promotable_pane(tag: &str) -> (tempfile::TempDir, String, String, std::path::PathBuf) {
    let repo = tempfile::tempdir().unwrap();
    let repo_path = repo.path().to_string_lossy().replace('\\', "/");
    let session_id = "aaaabbbb-cccc-4ddd-8eee-ffff00001111".to_string();
    let store = scratch_dir(tag);
    fixture_claude_session(&store, &session_id, &repo_path);
    loomux_lib::sessions::set_claude_projects_root_for_test(Some(store.clone()));
    (repo, repo_path, session_id, store)
}

fn drop_claude_store(store: &std::path::Path) {
    loomux_lib::sessions::set_claude_projects_root_for_test(None);
    let _ = fs::remove_dir_all(store);
}

/// Every group id with a `group.json` on disk. The building block for
/// [`group_fingerprints`], which is what the refusal tests actually assert on.
fn groups_on_disk(reg: &OrchRegistry) -> Vec<GroupId> {
    let mut out: Vec<GroupId> = fs::read_dir(reg.state_root())
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().join("group.json").is_file())
                .filter_map(|e| GroupId::parse(&e.file_name().to_string_lossy()).ok())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Every group's `group.json` text and audit-log length, keyed by id — "what
/// did that refusal touch?", which is the half of a refusal that is easy to get
/// wrong, and the half that is only *half* answered by a list of ids: a promote
/// can be refused after REWRITING an existing group rather than after creating
/// a new one, and the id list is identical either way. `group.json` carries a
/// `created_ms` stamped at each write, so a rewrite shows up here even when it
/// persists byte-identical guardrails.
///
/// Every refusal test asserts on this (rev-2 N3), including the five that run
/// against a state root with no groups at all, where it degrades to the same
/// emptiness check a name list would give — uniform, and one less thing for the
/// next person adding a refusal to have to choose between.
fn group_fingerprints(reg: &OrchRegistry) -> Vec<(GroupId, String, usize)> {
    let mut out: Vec<(GroupId, String, usize)> = groups_on_disk(reg)
        .into_iter()
        .map(|id| {
            let json = fs::read_to_string(reg.state_root().join(id.as_str()).join("group.json"))
                .unwrap_or_default();
            let audit = reg.audit_log(&id).len();
            (id, json, audit)
        })
        .collect();
    out.sort();
    out
}

/// Declare a one-reviewer workflow file in `repo`.
fn declare_workflow(repo: &std::path::Path, block_id: &str) {
    let loomux = repo.join(".loomux");
    fs::create_dir_all(&loomux).unwrap();
    fs::write(
        loomux.join("workflow.yml"),
        format!("version: 1\nblocks:\n  - id: {block_id}\n    kind: reviewer\n"),
    )
    .unwrap();
}

#[test]
fn session_origin_separates_resuming_a_session_from_needing_the_full_kickoff() {
    // The decomposition #407 exists for, pinned as a table. The old
    // `(Launch, Option<String>)` pair answered "does the CLI get `--resume`"
    // and "which kickoff is typed" with the SAME bool, so the one start that
    // answers them differently — a promote — could not be spelled at all.
    // Invert either column below and a promoted orchestrator either loses the
    // conversation it was promoted to keep, or is typed a three-line "you were
    // restarted" notice as its entire role contract.
    let sid = "11112222-3333-4444-8555-666677778888".to_string();
    let cases = [
        (SessionOrigin::Fresh, Launch::Fresh, None, false, true),
        (SessionOrigin::Resume(sid.clone()), Launch::Resume, Some(sid.as_str()), true, false),
        // #412's cold restart of an existing group, which the pair spelled as
        // the easily-misread `(Launch::Resume, None)`.
        (SessionOrigin::StartFresh, Launch::Resume, None, false, true),
        (
            SessionOrigin::Promote { session_id: sid.clone(), cli: "claude".into() },
            Launch::Promote,
            Some(sid.as_str()),
            true,
            true,
        ),
    ];
    for (origin, launch, session, resumes, full) in cases {
        let what = origin.as_str();
        assert_eq!(origin.launch(), launch, "{what}: group semantics");
        assert_eq!(origin.session_id(), session, "{what}: session to reopen");
        assert_eq!(origin.resumes_session(), resumes, "{what}: `--resume` or a minted id");
        assert_eq!(origin.wants_full_kickoff(), full, "{what}: full contract or resume notice");
    }
}

#[test]
fn a_promoted_pane_resumes_its_own_session_and_gets_orchestrator_wiring() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (_repo, repo_path, sid, store) = promotable_pane("promote-resume");
    let req = promote_to_orchestrator_sync(&reg, &repo_path, &sid, "claude", PromoteConfig::default());
    drop_claude_store(&store);
    let req = req.expect("a promotable pane must promote");

    assert_eq!(req.role, Role::Orchestrator);
    // The whole feature: the POC conversation carries over. A minted
    // `--session-id` here would be a fresh orchestrator wearing the gesture's
    // name, with the context it exists to keep discarded.
    assert!(
        req.command.contains(&format!("--resume {sid}")),
        "the promoted pane must reopen its own conversation: {}",
        req.command
    );
    assert!(
        !req.command.contains("--session-id"),
        "a resumed session must never also be handed a minted id: {}",
        req.command
    );
    // …with a real orchestrator's capabilities, which is what the relaunch is
    // FOR (a prompt-only re-roling would have none of these).
    assert!(req.command.contains("--mcp-config"), "MCP identity: {}", req.command);
    assert!(req.command.contains("--strict-mcp-config"), "{}", req.command);
    assert!(req.command.contains("--add-dir"), "group dir access: {}", req.command);
    // The silent-failure trap named in the plan: a promoted pane spawned
    // without env looks fine, and has no gh shim and no group dir.
    let env: HashMap<&str, &str> = req.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(env.get("LOOMUX_AGENT_ID"), Some(&req.agent_id.as_str()), "env: {env:?}");
    assert!(
        env.get("LOOMUX_GROUP_DIR").is_some_and(|d| d.contains(req.group_id.as_str())),
        "env: {env:?}"
    );
    assert!(
        env.get("PATH").is_some_and(|p| !p.is_empty()),
        "the gh/git shim rides PATH — without it the merge gate is not enforced: {env:?}"
    );

    // Recorded as a real orchestrator of a real group, on that session.
    let entry = reg.agent(&req.agent_id).expect("registered");
    assert_eq!(entry.session_id.as_deref(), Some(sid.as_str()));
    assert_eq!(entry.group, req.group_id);
    assert_eq!(reg.group(&req.group_id).unwrap().repo, repo_path);
    // The audit says WHICH of the four starts this was — `resume: true` alone
    // can no longer tell a promote from a session-browser resume.
    let spawn = reg
        .audit_log(&req.group_id)
        .into_iter()
        .find(|e| e.action == "agent-spawn")
        .expect("agent-spawn audited");
    assert_eq!(spawn.detail["origin"], json!("promote"));
    assert_eq!(spawn.detail["resume"], json!(true));
}

/// #1153 phase 3. The kickoff and the transcript scraper are one contract with
/// no compiler between them: `kickoff_body` WRITES a phrase, and
/// `detect_orch_signature` READS it back out of the CLI's own transcript months
/// later to say what role a pane held and which group it belonged to. Nothing
/// couples the two — they are a `format!` in one crate and a phrase table in
/// another — so a rename on either side is silently one-way, and the symptom is
/// not an error: it is a session that simply stops offering to resume.
///
/// This is the only test that puts a REAL kickoff through the real scraper.
/// The unit tests beside `detect_orch_signature` pin its phrase table against
/// strings typed into that file, which is exactly the coupling this one exists
/// to not rely on.
#[test]
fn every_kickoff_this_app_writes_is_one_session_restore_can_read_back() {
    use loomux_lib::sessions::detect_orch_signature;

    let (reg, _d) = test_registry();
    let g = reg.create_group("C:/tmp/repo", rails()).unwrap();

    for (role, expect) in [
        (Role::Orchestrator, "orchestrator"),
        (Role::Worker, "worker"),
        (Role::Reviewer, "reviewer"),
    ] {
        let a = reg.spawn_agent(&g.id, role, "n", "t", false, None).unwrap();
        let agent = reg.agent(&a.id).unwrap();
        let group = reg.group(&g.id).unwrap();
        let kickoff = reg.kickoff_prompt_ex(&agent, &group, "", None, KickoffOrigin::Normal);

        let (found_role, found_gid) = detect_orch_signature(&kickoff).unwrap_or_else(|| {
            panic!(
                "session restore cannot recognise the kickoff this app just wrote for a \
                 {expect}. The two spell the same phrase and nothing makes them agree:\n{kickoff}"
            )
        });
        assert_eq!(found_role, expect, "…and it must read back as the role it was written for");
        assert_eq!(
            found_gid.as_deref(),
            Some(g.id.as_str()),
            "…carrying the group it names, which is what a resume joins on"
        );
    }
}

#[test]
fn a_promoted_orchestrator_is_typed_the_full_contract_with_a_preamble() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (_repo, repo_path, sid, store) = promotable_pane("promote-kickoff");
    let req = promote_to_orchestrator_sync(&reg, &repo_path, &sid, "claude", PromoteConfig::default());
    drop_claude_store(&store);
    let req = req.expect("promote");

    let a = reg.agent(&req.agent_id).unwrap();
    let g = reg.group(&req.group_id).unwrap();
    let promoted = reg.kickoff_prompt_ex(&a, &g, "", None, KickoffOrigin::Promoted);
    let normal = reg.kickoff_prompt_ex(&a, &g, "", None, KickoffOrigin::Normal);

    // A promoted session has never seen the orchestrator contract in its life,
    // so it gets the WHOLE thing — asserted as "the ordinary kickoff, with a
    // preamble in front", which is stronger than a marker sweep: it fails if
    // any part of the contract is dropped for a promoted pane, including parts
    // added to the kickoff years from now.
    assert!(
        promoted.starts_with("Promoted to orchestrator"),
        "the preamble explains why the conversation above exists; it must come first: {}",
        &promoted[..promoted.len().min(120)]
    );
    assert!(
        promoted.ends_with(&normal),
        "a promoted orchestrator's kickoff must be the full contract, unmodified, after the preamble"
    );
    // …and the distinguishing markers of the full body, none of which the
    // resume notice a resumed orchestrator gets carries.
    for marker in ["First read your role instructions:", "Guardrails (enforced by orrerix)", "Delivery id:"] {
        assert!(promoted.contains(marker), "missing {marker:?} from a promoted kickoff");
    }
    let resume_notice = resume_kickoff_notice(None);
    assert!(
        !resume_notice.contains("First read your role instructions:"),
        "test premise: the resume notice is NOT the contract — that is why a promote cannot use it"
    );
    // Every pane that was spawned as what it is keeps its pre-#407 kickoff,
    // byte for byte.
    assert!(!normal.contains("Promoted to orchestrator"));
    assert_eq!(
        normal,
        reg.kickoff_prompt(&a, &g, "", None),
        "the four-argument wrapper must stay the Normal path"
    );
}

#[test]
fn promote_refuses_a_non_claude_pane() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (_repo, repo_path, sid, store) = promotable_pane("promote-cli");
    let err = promote_to_orchestrator_sync(&reg, &repo_path, &sid, "copilot", PromoteConfig::default())
        .unwrap_err();
    drop_claude_store(&store);
    assert!(err.starts_with("promote-unsupported-cli:"), "got: {err}");
    assert!(group_fingerprints(&reg).is_empty(), "a refused promote must create nothing");
}

#[test]
fn promote_refuses_anything_short_of_a_full_session_id() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (_repo, repo_path, sid, store) = promotable_pane("promote-prefix");
    // The 8-hex prefix a human copies out of a session row. `--resume <prefix>`
    // fails INSIDE the pane — after the promotion already killed the process
    // holding the context — so it is refused here instead of resolved.
    let err = promote_to_orchestrator_sync(&reg, &repo_path, &sid[..8], "claude", PromoteConfig::default())
        .unwrap_err();
    let empty = promote_to_orchestrator_sync(&reg, &repo_path, "  ", "claude", PromoteConfig::default())
        .unwrap_err();
    drop_claude_store(&store);
    assert!(err.starts_with("promote-bad-session:"), "got: {err}");
    assert!(empty.starts_with("promote-bad-session:"), "got: {empty}");
    assert!(group_fingerprints(&reg).is_empty(), "a refused promote must create nothing");
}

#[test]
fn promote_refuses_a_repo_path_that_is_not_a_usable_directory() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (_repo, repo_path, sid, store) = promotable_pane("promote-repo");
    let gone = format!("{repo_path}/does-not-exist");
    let missing = promote_to_orchestrator_sync(&reg, &gone, &sid, "claude", PromoteConfig::default())
        .unwrap_err();
    // A quote would escape the quoting of the composed command line.
    let quoted = promote_to_orchestrator_sync(
        &reg,
        "/tmp/evil\" ; rm -rf /",
        &sid,
        "claude",
        PromoteConfig::default(),
    )
    .unwrap_err();
    drop_claude_store(&store);
    assert!(missing.starts_with("promote-bad-repo:"), "got: {missing}");
    assert!(quoted.starts_with("promote-bad-repo:"), "got: {quoted}");
    assert!(group_fingerprints(&reg).is_empty(), "a refused promote must create nothing");
}

#[test]
fn promote_refuses_a_session_that_is_already_an_orchestration_member() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (_repo, repo_path, _sid, store) = promotable_pane("promote-member");
    // A group whose worker recorded a session — a delegate's transcript carries
    // a delegate contract, and promoting it would seat two conflicting role
    // contracts in one conversation.
    let g = reg.create_group(&repo_path, rails()).unwrap();
    let w = reg.spawn_agent(&g.id, Role::Worker, "w", "", false, None).unwrap();
    let member = w.session_id.clone().expect("a claude worker records a session id");
    let before = group_fingerprints(&reg);
    let err = promote_to_orchestrator_sync(&reg, &repo_path, &member, "claude", PromoteConfig::default())
        .unwrap_err();
    drop_claude_store(&store);
    assert!(err.starts_with("promote-already-managed:"), "got: {err}");
    assert!(err.contains(g.id.as_str()), "the refusal must name the group it is already in: {err}");
    // Fingerprints, not names: this refusal runs against an EXISTING group, so
    // "created no second group" is not the whole statement — it must not have
    // rewritten the live one either.
    assert_eq!(
        group_fingerprints(&reg),
        before,
        "a refused promote must create no second group, and must not touch the one it refused against"
    );
}

#[test]
fn promote_refuses_a_session_the_cli_store_has_never_heard_of() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (_repo, repo_path, _sid, store) = promotable_pane("promote-unknown");
    // A pane that has never been spoken to writes no transcript at all, and a
    // cleared history looks the same: `--resume` would fail inside the pane
    // with claude's own opaque "No conversation found", after the promotion had
    // already killed it.
    let err = promote_to_orchestrator_sync(
        &reg,
        &repo_path,
        "99999999-8888-4777-8666-555544443333",
        "claude",
        PromoteConfig::default(),
    )
    .unwrap_err();
    drop_claude_store(&store);
    assert!(err.starts_with("promote-not-found:"), "got: {err}");
    assert!(group_fingerprints(&reg).is_empty(), "a refused promote must create nothing");
}

#[test]
fn promote_into_a_fresh_group_reads_the_repo_workflow_file() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (repo, repo_path, sid, store) = promotable_pane("promote-fresh-workflow");
    declare_workflow(repo.path(), "rev-approved");
    // Fresh GROUP semantics: there is no prior group.json, so the promote modal
    // IS the moment the human is shown (and consents to) the repo's roster.
    let req = promote_to_orchestrator_sync(
        &reg,
        &repo_path,
        &sid,
        "claude",
        PromoteConfig { advanced_orchestrator: true, ..PromoteConfig::default() },
    );
    drop_claude_store(&store);
    let req = req.expect("promote");
    assert!(
        reg.group(&req.group_id).unwrap().guardrails.block("rev-approved").is_some(),
        "a promote with no group to inherit reads the workflow file, like any launch"
    );
    assert!(
        reg.audit_log(&req.group_id).iter().any(|e| e.action == "group-create"),
        "a brand-new group dir, not a reattach"
    );
}

/// The dormant group a promote reattaches to: launched advanced (roster
/// `rev-approved`), its cap adjusted live to 7, ended — and then the repo moved
/// on underneath it, so `.loomux/workflow.yml` now declares `rev-never-seen`
/// instead. Returns `(registry, repo_dir, repo_path, session_id, store, gid)`.
fn dormant_group_and_a_promotable_pane(
    tag: &str,
) -> (std::sync::Arc<OrchRegistry>, tempfile::TempDir, String, String, std::path::PathBuf, GroupId, tempfile::TempDir)
{
    let (reg, state) = test_registry();
    let reg = std::sync::Arc::new(reg);
    let (repo, repo_path, sid, store) = promotable_pane(tag);
    declare_workflow(repo.path(), "rev-approved");
    let launched = create_orchestration_group(
        &reg,
        &repo_path,
        Guardrails { advanced_orchestrator: true, max_agents: 7, ..rails() },
        SessionOrigin::Fresh,
        None,
        None,
    )
    .unwrap();
    let gid = launched.group_id.clone();
    assert!(
        reg.group(&gid).unwrap().guardrails.block("rev-approved").is_some(),
        "setup: the launch read the workflow file"
    );
    reg.end_group(&gid, false).unwrap();
    // …and the repo moves on underneath them: a `git pull` brings a reviewer
    // block nobody in that group ever approved.
    declare_workflow(repo.path(), "rev-never-seen");
    (reg, repo, repo_path, sid, store, gid, state)
}

#[test]
fn promote_reattaching_a_dormant_group_inherits_the_roster_and_knobs_it_ran_under() {
    let (reg, _repo, repo_path, sid, store, gid, _state) =
        dormant_group_and_a_promotable_pane("promote-reattach-inherit");

    // The promote modal's defaults — no advanced tick, no roster, cap 0 —
    // because a right-click is not the launcher and carries no preview.
    let req = promote_to_orchestrator_sync(&reg, &repo_path, &sid, "claude", PromoteConfig::default());
    drop_claude_store(&store);
    let req = req.expect("promote");

    assert_eq!(req.group_id, gid, "a dormant group is reattached, not orphaned");
    let rails_now = reg.group(&gid).unwrap().guardrails;
    // A promote arrives with the MODAL's defaults, not the launcher's preview,
    // so the roster has to come back off disk: without it, one right-click
    // silently replaces the roster this group's human approved with the
    // built-in four — and clears its merge gate on the way past.
    assert!(
        rails_now.block("rev-approved").is_some(),
        "the reattached group keeps the roster its human approved"
    );
    assert!(rails_now.advanced_orchestrator, "the toggle travels with the roster it selected");
    assert_eq!(
        rails_now.max_agents, 7,
        "live-adjustable knobs come back off disk — the dormant group's settings beat the modal's defaults"
    );
    assert!(
        reg.audit_log(&gid).iter().any(|e| e.action == "group-resume"),
        "reattach, not create"
    );
}

#[test]
fn promote_reattaching_a_dormant_group_does_not_re_read_the_workflow_file() {
    let (reg, _repo, repo_path, sid, store, gid, _state) =
        dormant_group_and_a_promotable_pane("promote-reattach-pin");
    // Ticked ON deliberately, so this pins the CONSENT rule rather than the
    // toggle being off: even asked for the advanced roster, a reattach runs the
    // one its own human approved. The moment of consent was that group's
    // launcher preview, and it outlived the session that gave it — the board,
    // audit log and backlog it governs are all still there.
    let req = promote_to_orchestrator_sync(
        &reg,
        &repo_path,
        &sid,
        "claude",
        PromoteConfig { advanced_orchestrator: true, ..PromoteConfig::default() },
    );
    drop_claude_store(&store);
    let req = req.expect("promote");

    assert_eq!(req.group_id, gid);
    assert!(
        reg.group(&gid).unwrap().guardrails.block("rev-never-seen").is_none(),
        "a block the repo gained AFTER that launch must not join it through a right-click either"
    );
    assert!(
        reg.audit_log(&gid).iter().any(|e| e.action == "workflow-changed-since-launch"),
        "the drift is audited so the human can SEE the repo has moved on — pinned, not stale"
    );
}

#[test]
fn promote_beside_a_live_group_opens_a_sibling_rather_than_a_second_orchestrator() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (_repo, repo_path, sid, store) = promotable_pane("promote-sibling");
    // A live orchestration on this repo. Joining it would seat a SECOND
    // orchestrator in one group — refused everywhere else in loomux (a resume
    // refuses while one is live; `spawn_agent` refuses `kind: orchestrator`),
    // and promote must not become the loophole.
    let live = create_orchestration_group(&reg, &repo_path, rails(), SessionOrigin::Fresh, None, None)
        .unwrap();
    let req = promote_to_orchestrator_sync(&reg, &repo_path, &sid, "claude", PromoteConfig::default());
    drop_claude_store(&store);
    let req = req.expect("promote");

    assert_ne!(req.group_id, live.group_id, "a live group is never joined");
    assert_eq!(req.group_id, format!("{}-2", live.group_id), "the sibling id, same as a concurrent launch");
    assert_eq!(
        reg.agent(&live.agent_id).unwrap().group,
        live.group_id,
        "the live orchestration is untouched"
    );
}

#[test]
fn promote_refuses_when_the_reattached_roster_pins_another_cli() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (_repo, repo_path, sid, store) = promotable_pane("promote-cli-mismatch");
    // A dormant group whose roster pins its orchestrator to a different CLI.
    // The reattach inherits that roster (correctly — it is what its human
    // approved), and the composed command would then be `copilot --resume
    // <claude uuid>`: a failure INSIDE the pane, after the promotion has
    // already killed the process holding the conversation. Fail closed instead.
    let pinned = Guardrails {
        blocks: workflow::default_roster(&[
            (Role::Orchestrator, "copilot", "sonnet"),
            (Role::Worker, "", "sonnet"),
            (Role::Reviewer, "", "sonnet"),
            (Role::Planner, "", "opus"),
        ]),
        ..rails()
    };
    let dormant = reg.create_group_ex(&repo_path, pinned, Launch::Fresh).unwrap();
    let before = group_fingerprints(&reg);
    let err = promote_to_orchestrator_sync(&reg, &repo_path, &sid, "claude", PromoteConfig::default())
        .unwrap_err();
    drop_claude_store(&store);
    assert!(err.starts_with("promote-cli-mismatch:"), "got: {err}");
    // rev-1 B1: the side-effect half every other refusal test asserts, and the
    // one this test used to be missing — which is exactly where it would have
    // failed. This refusal needs a RESOLVED roster, so it is the one that could
    // only fire after `create_group_ex` had already rewritten the dormant
    // group's `group.json`, re-seeded its markers and audited a `group-resume`
    // for a resume that never happened.
    assert_eq!(
        group_fingerprints(&reg),
        before,
        "a refused promote must not have touched the dormant group it was refused against"
    );
    assert!(
        !reg.audit_log(&dormant.id).iter().any(|e| e.action == "group-resume"),
        "…and must not have audited a reattach that did not happen"
    );
}

#[test]
fn promote_refuses_a_fresh_group_whose_workflow_file_pins_another_cli() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (repo, repo_path, sid, store) = promotable_pane("promote-cli-mismatch-fresh");
    // The orphan-group path (rev-1 B1): no group exists yet, the human ticks
    // the advanced box, and the repo's own workflow file pins the orchestrator
    // to another CLI. The mismatch is just as real here — and there is no
    // dormant group to fall back to, so a group created and then refused would
    // be pure residue: a group.json, instruction files, an armed merge gate and
    // a `group-create` audit for an orchestration the human never got.
    fs::create_dir_all(repo.path().join(".loomux")).unwrap();
    fs::write(
        repo.path().join(".loomux").join("workflow.yml"),
        "version: 1\nblocks:\n  - id: orchestrator\n    kind: orchestrator\n    cli: copilot\n",
    )
    .unwrap();
    let err = promote_to_orchestrator_sync(
        &reg,
        &repo_path,
        &sid,
        "claude",
        PromoteConfig { advanced_orchestrator: true, ..PromoteConfig::default() },
    )
    .unwrap_err();
    drop_claude_store(&store);
    assert!(err.starts_with("promote-cli-mismatch:"), "got: {err}");
    assert!(
        group_fingerprints(&reg).is_empty(),
        "a refused promote must leave no orphan group behind — there is nothing to resume it with"
    );
}

#[test]
fn promote_refuses_a_dormant_group_whose_own_agent_cli_is_not_the_panes() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (_repo, repo_path, sid, store) = promotable_pane("promote-cli-inherited");
    // rev-1 N1: the built-in roster persists every block's `cli` as `""` —
    // "inherit the group default" — so a dormant group launched on copilot has
    // no per-block CLI to mismatch on. Inheriting `blocks` without `agent_cli`
    // would let a claude pane promote straight in, re-CLI every delegate that
    // group ever spawns, and never say so: the promote modal has no CLI field.
    let dormant = reg
        .create_group_ex(&repo_path, Guardrails { agent_cli: "copilot".into(), ..rails() }, Launch::Fresh)
        .unwrap();
    let before = group_fingerprints(&reg);
    let err = promote_to_orchestrator_sync(&reg, &repo_path, &sid, "claude", PromoteConfig::default())
        .unwrap_err();
    drop_claude_store(&store);
    assert!(
        err.starts_with("promote-cli-mismatch:"),
        "the group's own CLI is part of the roster its human approved: {err}"
    );
    assert_eq!(
        group_fingerprints(&reg),
        before,
        "and the refusal leaves the dormant group exactly as it was"
    );
    assert_eq!(
        reg.load_group_file(&dormant.id).unwrap().1.agent_cli,
        "copilot",
        "its persisted CLI above all — that is the thing a silent promote would have rewritten"
    );
}

#[test]
fn promote_reattaching_a_dormant_group_keeps_the_cli_its_other_blocks_inherit() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (_repo, repo_path, sid, store) = promotable_pane("promote-cli-inherit-keep");
    // The other half of rev-1 N1, and the half a refusal cannot pin: a dormant
    // group on copilot whose ORCHESTRATOR block is explicitly `cli: claude`.
    // A claude pane is legitimately promotable into it — no mismatch — so the
    // promote SUCCEEDS, and the question becomes what its delegates run. The
    // group's `agent_cli` is not a setting beside the roster; it is the half of
    // it that says what every `cli: ""` block runs, and letting the modal's
    // default overwrite it silently re-CLIs every worker that group will ever
    // spawn, with nothing shown to the human.
    let mixed = Guardrails {
        agent_cli: "copilot".into(),
        blocks: workflow::default_roster(&[
            (Role::Orchestrator, "claude", "opus"),
            (Role::Worker, "", "sonnet"),
            (Role::Reviewer, "", "sonnet"),
            (Role::Planner, "", "opus"),
        ]),
        ..rails()
    };
    let dormant = reg.create_group_ex(&repo_path, mixed, Launch::Fresh).unwrap();
    let req = promote_to_orchestrator_sync(&reg, &repo_path, &sid, "claude", PromoteConfig::default());
    drop_claude_store(&store);
    let req = req.expect("a claude pane promotes into a group whose orchestrator block is claude");

    assert_eq!(req.group_id, dormant.id);
    assert!(req.command.starts_with("claude "), "the promoted pane is still claude: {}", req.command);
    let rails_now = reg.group(&dormant.id).unwrap().guardrails;
    assert_eq!(
        rails_now.agent_cli, "copilot",
        "the group default its blocks inherit is part of the roster its human approved"
    );
    assert_eq!(rails_now.cli_for(Role::Worker), "copilot", "so a delegate still resolves to it");
    assert_eq!(
        reg.load_group_file(&dormant.id).unwrap().1.agent_cli,
        "copilot",
        "and it survives the group.json rewrite the reattach performs"
    );
    let w = reg.spawn_agent(&dormant.id, Role::Worker, "w", "", false, None).unwrap();
    let spawned = reg.spawn_request_for_test(&w.id).expect("worker spawn request");
    assert!(
        spawned.command.starts_with("copilot "),
        "the delegate a promoted orchestrator spawns runs the group's CLI, not the promoted pane's: {}",
        spawned.command
    );
}

#[test]
fn promote_retires_the_panes_standalone_identity() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (_repo, repo_path, sid, store) = promotable_pane("promote-solo");
    // The pane is usually already a `__solo__` member by the time the menu is
    // built (`adoptIfEligible` mints one on right-click). Left alive after the
    // promotion it is a stale channel endpoint keyed to a pty that is now an
    // orchestrator: a peer's `channel_send` would be delivered into a session
    // that never joined that channel.
    let (solo_id, _token) = spawn_solo(&reg, "claude", 7001);
    assert_eq!(reg.agent(&solo_id).unwrap().status, AgentStatus::Running);
    let req = promote_to_orchestrator_sync(
        &reg,
        &repo_path,
        &sid,
        "claude",
        PromoteConfig { solo_agent_id: solo_id.clone(), ..PromoteConfig::default() },
    );
    drop_claude_store(&store);
    let req = req.expect("promote");
    assert_eq!(
        reg.agent(&solo_id).unwrap().status,
        AgentStatus::Dead,
        "the standalone identity is retired by the promotion that replaced it"
    );
    assert!(
        reg.audit_log(&req.group_id).iter().any(|e| e.action == "solo-retired"),
        "and says so in the trail"
    );
}

#[test]
fn promote_never_retires_an_agent_that_is_not_a_standalone_pane() {
    use std::sync::Arc;
    let (reg, _d) = test_registry();
    let reg = Arc::new(reg);
    let (_repo, repo_path, sid, store) = promotable_pane("promote-solo-guard");
    // `solo_agent_id` arrives from the webview. Naming a real orchestration
    // agent must never kill it through this door — that would be a one-click
    // way to take out another group's orchestrator.
    let other = tempfile::tempdir().unwrap();
    let other_repo = other.path().to_string_lossy().replace('\\', "/");
    let victim = create_orchestration_group(&reg, &other_repo, rails(), SessionOrigin::Fresh, None, None)
        .unwrap();
    let req = promote_to_orchestrator_sync(
        &reg,
        &repo_path,
        &sid,
        "claude",
        PromoteConfig { solo_agent_id: victim.agent_id.clone(), ..PromoteConfig::default() },
    );
    drop_claude_store(&store);
    let req = req.expect("promote");
    assert_eq!(
        reg.agent(&victim.agent_id).unwrap().status,
        AgentStatus::Running,
        "an orchestration agent is never retired through the solo door"
    );
    assert!(
        reg.audit_log(&req.group_id).iter().any(|e| e.action == "solo-retire-skipped"),
        "and the attempt leaves a trail"
    );
}

#[test]
fn start_fresh_on_an_orchestrator_does_not_re_read_the_workflow_file() {
    // #412 rev-17 blocker: `resume_recorded_session`'s `start_fresh` path
    // passes `resume_session: None` (it mints a NEW session id, same as a
    // fresh launch would) — before the fix, `create_orchestration_group`
    // derived `Launch` from `resume_session.is_some()` alone, so `None` here
    // was indistinguishable from a human at the launcher and re-read
    // `.loomux/workflow.yml`. Proven live (rev-17's own execution): the
    // group's roster silently swapped to whatever the repo currently
    // declares, and its `merge_gate` spec file was DELETED when the repo no
    // longer declares one. Both directions pinned below.
    use std::sync::Arc;
    let state = tempfile::tempdir().unwrap();
    let repo = gated_repo(""); // rev-security, rev-tests, an all-pass merge gate
    let repo_path = repo.path().to_string_lossy().replace('\\', "/");
    let reg = Arc::new(relaunch_registry(state.path()));
    reg.set_port(45999);

    let launched = create_orchestration_group(
        &reg,
        &repo_path,
        Guardrails { advanced_orchestrator: true, ..rails() },
        SessionOrigin::Fresh,
        None,
        None,
    )
    .unwrap();
    let gid = launched.group_id.clone();
    let orch_sid = reg.agent(&launched.agent_id).unwrap().session_id.clone().unwrap();
    assert!(
        reg.group(&gid).unwrap().guardrails.block("rev-security").is_some(),
        "test setup sanity: the launch read the gated roster"
    );
    assert!(reg.merge_gate_declared(&gid), "test setup sanity: the launch declared the gate");
    let roster_before: Vec<String> =
        reg.group(&gid).unwrap().guardrails.blocks.iter().map(|b| b.id.clone()).collect();
    let gate_before = reg.merge_gate(&gid);

    // The human ends the session; the repo changes underneath them — a
    // DIFFERENT roster, and the gate dropped entirely.
    reg.end_group(&gid, false).unwrap();
    fs::write(
        repo.path().join(".loomux").join("workflow.yml"),
        "version: 1\nname: changed\nblocks:\n  - id: worker\n    kind: worker\n",
    )
    .unwrap();

    use loomux_lib::orchestration::resume_recorded_session;
    // Fixture the session into the store so B1's existence pre-check passes
    // (this test is about the workflow-read boundary, not that check).
    let store = scratch_dir("start-fresh-no-reread");
    fixture_claude_session(&store, &orch_sid, &repo_path);
    loomux_lib::sessions::set_claude_projects_root_for_test(Some(store.clone()));
    let outcome = resume_recorded_session(&reg, &orch_sid, None, /* start_fresh */ true);
    loomux_lib::sessions::set_claude_projects_root_for_test(None);
    let _ = fs::remove_dir_all(&store);
    outcome.expect("start-fresh on an existing group must not fail");

    let roster_after: Vec<String> =
        reg.group(&gid).unwrap().guardrails.blocks.iter().map(|b| b.id.clone()).collect();
    assert_eq!(
        roster_before, roster_after,
        "start-fresh must NOT re-read the workflow file and swap the roster — the human never \
         previewed/approved 'changed'"
    );
    assert!(
        reg.merge_gate_declared(&gid),
        "start-fresh must NOT delete the merge-gate spec just because the changed file dropped it"
    );
    assert_eq!(
        reg.merge_gate(&gid),
        gate_before,
        "the merge gate content itself must be byte-identical, not just 'still declared'"
    );
}
