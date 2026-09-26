//! The provider-limit attention reason.
//!
//! One module of the `orchestration` integration-test target (`main.rs`),
//! split out of the former single-file `tests/orchestration.rs` by #3498 P1
//! as a pure move. Layout rules: docs/design/module-layout.md.

use super::*;

// ── #2811 S5a: the `provider-limit` attention reason ────────────────────────
//
// Every fixture below is a REAL pane tail, lifted out of this group's own
// audit log for the Sep-5/Sep-6 incidents (`q-35`, `q-39`, `q-40`, `q-45`) and
// written to disk unmodified. That matters twice over: the OpenRouter credits
// message really is broken mid-word behind pi's box gutter, and the Claude one
// really does end in a CURLY apostrophe — two shapes a hand-typed specimen
// would have smoothed away, and each one enough on its own to make a
// plausible-looking detector see nothing.
const FIX_LIMIT_CLAUDE: &str = include_str!("../fixtures/attention/claude-usage-limit.txt");
const FIX_LIMIT_OR_KEY: &str = include_str!("../fixtures/attention/openrouter-key-limit.txt");
const FIX_LIMIT_OR_CREDITS: &str =
    include_str!("../fixtures/attention/openrouter-credits-exhausted.txt");
/// The negative control, and it is not a synthetic one either: this is the
/// ORCHESTRATOR's own `ask_human` text from `q-39`, in which it quotes the
/// Claude refusal (`/usage-credits to finish what you're working on`,
/// mid-sentence and behind a `stopped at "`) while asking the human to top the
/// account up. A detector that searched the tail for its needles anywhere
/// would badge the orchestrator's pane as out of credit for talking about
/// being out of credit.
///
/// **This file is also why `LIMIT_PATTERNS` may only carry captured needles.**
/// Its first 26 bytes are `Claude usage limit reached`, which an earlier
/// revision of the table carried as a needle taken on report — so this very
/// fixture, the control, matched it line-initially and the test below could
/// never have passed. The row is gone; see `LIMIT_PATTERNS`' own doc. Keep
/// that in mind before adding a needle that reads like a sentence someone
/// might write.
const FIX_LIMIT_NEGATIVE: &str =
    include_str!("../fixtures/attention/negative-orchestrator-quotes-a-limit.txt");
/// #3190 item 2: positive controls for the paragraph-start rule's own premise.
/// The refusal lines are verbatim from the captured fixtures above; the
/// context line above the blank row is representative, not a capture. What
/// these files pin is the convention — see the test below and
/// `fixtures/attention/README.md` for the re-bless step.
const FIX_CONV_CLAUDE: &str =
    include_str!("../fixtures/attention/positive-convention-claude-usage-limit.txt");
const FIX_CONV_OR_KEY: &str =
    include_str!("../fixtures/attention/positive-convention-openrouter-key-limit.txt");
const FIX_CONV_OR_CREDITS: &str =
    include_str!("../fixtures/attention/positive-convention-openrouter-credits-exhausted.txt");

/// A group with one worker, plus the tail maps `attention_tick` consumes.
/// `attention_setup` gives the worker a pty; nothing here needs a real one.
fn limit_scan(
    reg: &OrchRegistry,
    now: u64,
    tails: &[(&str, &str)],
) -> Vec<AttentionItem> {
    let outputs: HashMap<String, u64> =
        tails.iter().map(|(id, _)| ((*id).to_string(), 1u64)).collect();
    let tails: HashMap<String, String> = tails
        .iter()
        .map(|(id, t)| ((*id).to_string(), strip_ansi(t.as_bytes())))
        .collect();
    reg.attention_tick(now, &outputs, &tails, &HashMap::new())
}

/// The provenance rule `LIMIT_PATTERNS` states, enforced where the fixtures
/// live. A needle ships only when a CAPTURED pane tail proves both that the
/// provider prints it and that it prints it **line-initially** — the second
/// half being the one the line-initial anchor actually depends on.
///
/// This exists because the honest-label version of the rule was not enough. An
/// earlier revision carried needles marked "taken on report", one of which
/// (`Claude usage limit reached`) is an ordinary sentence opener that the
/// orchestrator's own `ask_human` text begins with — so it badged the
/// orchestrator's pane for talking about a limit. A field recording provenance
/// let a reader notice that; a test refusing the row stops it.
#[test]
fn every_pattern_is_exercised_by_its_own_captured_fixture() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/attention");
    for p in providerlimit::LIMIT_PATTERNS {
        let path = dir.join(p.fixture);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("pattern {:?} names {:?}: {e}", p.needle, p.fixture));
        // Line-initial in the capture, modulo the indent/gutter the scan
        // strips — the property the anchor rests on, not merely "appears
        // somewhere in the file".
        assert!(
            providerlimit::limit_in_tail(&text).is_some_and(|hit| hit.needle == p.needle),
            "pattern {:?} does not match its own capture {:?} line-initially — either the \
             fixture is not what the provider prints, or the needle was not cut from it",
            p.needle,
            p.fixture
        );
    }
    // Non-vacuity: the loop saw the whole table, and the table is not empty.
    assert!(
        providerlimit::LIMIT_PATTERNS.len() >= 3,
        "only {} patterns scanned",
        providerlimit::LIMIT_PATTERNS.len()
    );
}

#[test]
fn a_captured_pane_raises_its_providers_limit_with_the_remedy() {
    // The three captured refusals, each on its own pane in its own group so
    // the once-per-group-per-provider dedup cannot mask a miss as a merge.
    for (fixture, provider_id, display, label) in [
        (FIX_LIMIT_CLAUDE, "anthropic", "Anthropic (Claude)", "claude usage credits"),
        (FIX_LIMIT_OR_KEY, "openrouter", "OpenRouter", "openrouter key limit"),
        (FIX_LIMIT_OR_CREDITS, "openrouter", "OpenRouter", "openrouter credits"),
    ] {
        let (reg, _d, _g, wid) = attention_setup();
        let items = limit_scan(&reg, 1_000_000_000_000, &[(wid.as_str(), fixture)]);
        let item = items
            .iter()
            .find(|i| i.agent_id == wid)
            .unwrap_or_else(|| panic!("{label}: the captured pane raised no attention item at all"));
        assert_eq!(
            item.reason, "provider-limit",
            "{label}: a pane parked on a provider refusal must read as provider-limit, not {:?}",
            item.reason
        );
        assert!(
            item.detail.contains(display),
            "{label}: the detail must NAME the provider — {}",
            item.detail
        );
        let remedy = providerlimit::provider(provider_id).expect("declared provider").remedy;
        assert!(
            item.detail.contains(remedy),
            "{label}: the detail must tell the human what to DO — {}",
            item.detail
        );
        assert_eq!(
            item.pty_id,
            reg.agent(&wid).unwrap().pty_id,
            "{label}: the badge must carry the pty the header chip and dock dot key off"
        );
    }
}

#[test]
fn the_orchestrator_quoting_a_refusal_raises_no_limit() {
    // The false positive the line-initial anchor exists for, on the real text
    // (`q-39`). This pane is not out of credit; it is ASKING about a pane that
    // is, and badging it would send the human to the wrong terminal.
    let (reg, _d, _g, wid) = attention_setup();
    let items = limit_scan(&reg, 1_000_000_000_000, &[(wid.as_str(), FIX_LIMIT_NEGATIVE)]);
    assert!(
        items.iter().all(|i| i.reason != "provider-limit"),
        "quoting a refusal must not raise one: {items:?}"
    );
    // Non-vacuity: the SAME scan, same pane, same registry, does raise one for
    // a tail that really is a refusal — so the empty result above is the
    // anchor working rather than the scan never running.
    let live = limit_scan(&reg, 1_000_000_005_000, &[(wid.as_str(), FIX_LIMIT_OR_KEY)]);
    assert!(
        live.iter().any(|i| i.agent_id == wid && i.reason == "provider-limit"),
        "control: the scan does raise the reason when the tail really carries one"
    );
}

#[test]
fn one_chip_per_group_per_provider_however_many_panes_stopped() {
    // The plan's rule, and the reason for it: an OpenRouter key cap stops
    // every GLM pane in the group at the same instant (four of them, on
    // Sep 5). Four identical red chips and four toasts say one thing four
    // times and need one remedy once.
    let (reg, _d, g, first) = attention_setup();
    let second = reg.spawn_agent(&g, Role::Reviewer, "rev", "review", false, None).unwrap();
    let third = reg.spawn_agent(&g, Role::Reviewer, "rev2", "review", false, None).unwrap();

    let items = limit_scan(
        &reg,
        1_000_000_000_000,
        &[
            (first.as_str(), FIX_LIMIT_OR_KEY),
            (second.id.as_str(), FIX_LIMIT_OR_CREDITS),
            (third.id.as_str(), FIX_LIMIT_OR_KEY),
        ],
    );
    let raised: Vec<&AttentionItem> =
        items.iter().filter(|i| i.reason == "provider-limit").collect();
    assert_eq!(
        raised.len(),
        1,
        "three OpenRouter-stopped panes must produce ONE chip, not three: {raised:?}"
    );
    // On the lowest-sorting affected agent id — deterministic, where the
    // roster's own iteration order is a `HashMap`'s and is not.
    let mut ids = vec![first.clone(), second.id.clone(), third.id.clone()];
    ids.sort();
    assert_eq!(raised[0].agent_id, ids[0], "the chip lands on a deterministic pane");
    assert!(
        raised[0].detail.contains("3 panes"),
        "the one chip must still tell the truth about the blast radius: {}",
        raised[0].detail
    );

    // And a SECOND provider in the same group is its own chip: the dedup key
    // is (group, provider), not (group).
    let mixed = limit_scan(
        &reg,
        1_000_000_005_000,
        &[(first.as_str(), FIX_LIMIT_OR_KEY), (second.id.as_str(), FIX_LIMIT_CLAUDE)],
    );
    let providers: HashSet<&str> = mixed
        .iter()
        .filter(|i| i.reason == "provider-limit")
        .map(|i| if i.detail.contains("OpenRouter") { "openrouter" } else { "anthropic" })
        .collect();
    assert_eq!(
        providers.len(),
        2,
        "two providers limited at once are two chips: {mixed:?}"
    );
    // Singular/plural is read, not glued on: a one-pane limit says "1 pane".
    let alone = limit_scan(&reg, 1_000_000_010_000, &[(first.as_str(), FIX_LIMIT_OR_KEY)]);
    assert!(
        alone.iter().any(|i| i.reason == "provider-limit" && i.detail.contains("1 pane stopped")),
        "{alone:?}"
    );
}

#[test]
fn provider_limit_sits_under_blocked_and_over_stranded_and_waiting() {
    // The chain in `attention_tick`, pinned where a reader can fail it. An
    // agent that SAID it is blocked outranks a diagnosis loomux made from pane
    // text; a wedge one Enter clears does not outrank one nothing typed in the
    // terminal can clear.
    let (reg, _d, g, wid) = attention_setup();
    let now = 1_000_000_000_000u64;

    // Over `waiting`: park the pane long enough that `waiting` would fire, on
    // a tail that is ALSO a refusal.
    let tails = [(wid.as_str(), FIX_LIMIT_OR_KEY)];
    limit_scan(&reg, now, &tails);
    let over_waiting = limit_scan(&reg, now + 5_000, &tails);
    assert_eq!(
        over_waiting.iter().find(|i| i.agent_id == wid).map(|i| i.reason),
        Some("provider-limit"),
        "a provider limit outranks a pane merely parked on a prompt"
    );

    // Over `stranded`, which is latched and outranks `waiting` itself.
    reg.mark_stranded(&g, &wid, Some(StrandedBlocker::HumanInput));
    let over_stranded = limit_scan(&reg, now + 6_000, &tails);
    assert_eq!(
        over_stranded.iter().find(|i| i.agent_id == wid).map(|i| i.reason),
        Some("provider-limit"),
        "a provider limit outranks a stranded prompt: one Enter clears that, nothing clears this"
    );
    reg.clear_stranded(&g, &wid, "test");

    // Under `blocked`.
    reg.note_report_attention(&wid, "blocked");
    let under_blocked = limit_scan(&reg, now + 7_000, &tails);
    assert_eq!(
        under_blocked.iter().find(|i| i.agent_id == wid).map(|i| i.reason),
        Some("blocked"),
        "an agent that explicitly reported blocked still outranks the diagnosis"
    );
}

#[test]
fn a_refusal_loomux_itself_delivered_into_the_pane_raises_nothing() {
    // #576's self-latch, arriving at a third consumer. The orchestrator relays
    // "[orch] rev-2313 stopped: Key limit exceeded (total limit)" into a
    // worker's pane; that line is now in the worker's own tail, line-initial
    // once the relay's prefix wraps. The worker is not out of credit.
    let (reg, _d, g, _w) = attention_setup();
    let session = "5f3a9d02-1111-4222-8333-444455556666";
    let a = reg.spawn_agent(&g, Role::Reviewer, "rev", "review", false, None).unwrap();
    reg.set_session_for_test(&a.id, session);
    reg.set_pty_for_test(&a.id, 4101);

    let relayed = "Key limit exceeded (total limit) — that is rev-2313, not you; hold.";
    reg.record_delivered_prompt(4101, relayed, Delivery::MidSession);
    // The blank line is load-bearing, and it is the paragraph-start rule
    // (#3178 review round 3) showing up in an older fixture: with the relayed
    // line glued directly under `$ cargo test`, it is a wrap continuation as
    // far as the scan can tell, so NEITHER pane raises the reason and the
    // control below passes for the wrong reason — the detector never fires at
    // all rather than the mask suppressing it. Written as its own paragraph,
    // the way a real pane renders a relayed notice, this test discriminates
    // the mask again.
    let tail = format!("$ cargo test\n\n{relayed}\n");

    let items = limit_scan(&reg, 1_000_000_000_000, &[(a.id.as_str(), &tail)]);
    assert!(
        items.iter().all(|i| i.reason != "provider-limit"),
        "a refusal loomux itself typed into the pane must be masked out: {items:?}"
    );
    // Non-vacuity: the identical tail on a pane with NO delivery record for it
    // does raise the reason, so the silence above is the mask and not the
    // detector failing on this string.
    let bare = reg.spawn_agent(&g, Role::Reviewer, "rev-bare", "review", false, None).unwrap();
    reg.set_session_for_test(&bare.id, "6a4b0e13-2222-4333-8444-555566667777");
    reg.set_pty_for_test(&bare.id, 4102);
    let unmasked = limit_scan(&reg, 1_000_000_005_000, &[(bare.id.as_str(), &tail)]);
    assert!(
        unmasked.iter().any(|i| i.agent_id == bare.id && i.reason == "provider-limit"),
        "control: the same text with no delivery record behind it DOES raise one"
    );
}

/// #3178 review B1 — the single chip must not be handed to a pane that
/// something else will outrank, because only the CARRIER holds a `limit_chip`
/// entry and every other affected pane falls through the chain.
///
/// The sequence the review traced: a worker reported `blocked` (the latch lives
/// in `attn_reports` and clears on new-work assignment, human focus, or the
/// agent's next report — none of which happen while it silently retries), then
/// hit the provider limit as the LOWEST-sorting of three stopped panes. Under
/// the first revision it rendered `blocked`, the other two rendered nothing,
/// and the group showed ZERO provider-limit chips while three panes sat
/// stopped — the feature's own rationale inverted.
#[test]
fn the_single_chip_avoids_a_pane_whose_blocked_latch_would_swallow_it() {
    let (reg, _d, g, first) = attention_setup();
    let second = reg.spawn_agent(&g, Role::Reviewer, "rev", "review", false, None).unwrap();
    let third = reg.spawn_agent(&g, Role::Reviewer, "rev2", "review", false, None).unwrap();

    // The lowest-sorting affected id — the carrier the plain lowest-id rule
    // would pick, and therefore the pane that must carry the latch for this
    // test to exercise the collision at all.
    //
    // Derived from the sort rather than assumed to be any particular pane:
    // `attention_setup`'s worker is `w-2` and the reviewers are `rev-3`/`rev-4`,
    // so the lowest is a REVIEWER, not the worker. An earlier revision asserted
    // it was the worker and CI rejected the fixture — which is the guard below
    // doing its job, and the reason it is an assertion rather than a comment.
    let mut ids = vec![first.clone(), second.id.clone(), third.id.clone()];
    ids.sort();
    let lowest = ids[0].clone();
    assert!(
        ids.len() >= 2 && ids[1] != lowest,
        "fixture: there must be another affected pane for the chip to move TO, \
         or a pass proves nothing about the carrier choice: {ids:?}"
    );

    // ...and it is `blocked`, which outranks `provider-limit`.
    reg.note_report_attention(&lowest, "blocked");

    let tails = [
        (first.as_str(), FIX_LIMIT_OR_KEY),
        (second.id.as_str(), FIX_LIMIT_OR_KEY),
        (third.id.as_str(), FIX_LIMIT_OR_CREDITS),
    ];
    let items = limit_scan(&reg, 1_000_000_000_000, &tails);

    let raised: Vec<&AttentionItem> =
        items.iter().filter(|i| i.reason == "provider-limit").collect();
    assert_eq!(
        raised.len(),
        1,
        "the group must still show its one provider-limit chip: {items:?}"
    );
    assert_ne!(
        raised[0].agent_id, lowest,
        "the chip must not be parked on the pane whose `blocked` latch outranks it"
    );
    // It still counts every stopped pane, the latched one included — the latch
    // changes who WEARS the chip, not how many panes the provider stopped.
    assert!(
        raised[0].detail.contains("3 panes"),
        "the blast radius still counts the latched pane: {}",
        raised[0].detail
    );
    // And the latched pane keeps its own, more urgent reason.
    assert_eq!(
        items.iter().find(|i| i.agent_id == lowest).map(|i| i.reason),
        Some("blocked"),
        "the outranking reason is untouched — this is a carrier choice, not a demotion"
    );
}

/// The other half of B1's fix, and the arm that DISCLOSES a residual rather
/// than hiding it: when EVERY affected pane is outranked there is no
/// un-outranked carrier to choose, and the provider attribution really is lost.
///
/// That is a chosen trade, not an oversight. Every affected pane is already
/// showing an urgent chip that summons the human to this same group, so what
/// goes missing is which provider stopped them — and raising a second chip
/// instead would reintroduce exactly the per-pane spam the one-chip rule
/// exists to prevent. Pinned here so the disclosure in `attention_tick`'s
/// comment and in `docs/design/attention-provider-limit.md` cannot go quietly
/// false under a later edit.
#[test]
fn a_provider_limit_is_subsumed_when_every_affected_pane_is_outranked() {
    let (reg, _d, g, first) = attention_setup();
    let second = reg.spawn_agent(&g, Role::Reviewer, "rev", "review", false, None).unwrap();
    reg.note_report_attention(&first, "blocked");
    reg.note_report_attention(&second.id, "blocked");

    let tails = [(first.as_str(), FIX_LIMIT_OR_KEY), (second.id.as_str(), FIX_LIMIT_OR_KEY)];
    let items = limit_scan(&reg, 1_000_000_000_000, &tails);

    assert!(
        items.iter().all(|i| i.reason != "provider-limit"),
        "documented residual: with every affected pane outranked the attribution is lost: {items:?}"
    );
    // Non-vacuity, and the reason the residual is survivable: both panes ARE
    // still wearing an urgent chip, so nothing goes unreported to the human —
    // only the provider attribution does.
    assert_eq!(
        items.iter().filter(|i| i.reason == "blocked").count(),
        2,
        "both panes must still be flagged urgently: {items:?}"
    );
    // The control for the whole test: drop ONE latch and the chip comes back,
    // so the silence above is the all-outranked arm and not the scan failing.
    //
    // **The latch dropped is the HIGHER-sorting pane's, deliberately.**
    // `attention_setup`'s worker is `w-2` and its reviewer `rev-3`, so `rev-3`
    // sorts first: acking `rev-3` would leave the un-outranked candidate and
    // the lowest-id candidate as the same pane, and a plain lowest-id rule —
    // the exact thing B1 removed — would satisfy this assertion too. Acking the
    // worker instead makes the two disagree, so this row fails if the
    // precedence-aware choice is ever reverted. Measured: with `rev-3` acked
    // the mutation run left this test GREEN.
    let unlatched = if first > second.id { first.clone() } else { second.id.clone() };
    assert_ne!(
        unlatched,
        {
            let mut ids = vec![first.clone(), second.id.clone()];
            ids.sort();
            ids[0].clone()
        },
        "fixture: the un-outranked pane must NOT be the lowest-sorting one, or a \
         plain lowest-id carrier rule satisfies this row and it discriminates nothing"
    );
    reg.ack_attention(&unlatched);
    let after = limit_scan(&reg, 1_000_000_005_000, &tails);
    assert_eq!(
        after.iter().find(|i| i.reason == "provider-limit").map(|i| i.agent_id.as_str()),
        Some(unlatched.as_str()),
        "the chip goes to the un-outranked pane, not to the lowest-sorting one: {after:?}"
    );
}

/// The SAME text as `negative-orchestrator-quotes-a-limit.txt`, hard-wrapped at
/// 72 columns behind a `┃` gutter — the form the orchestrator's pane actually
/// renders it in, and the form `attention_tail` actually reads.
///
/// #3178 review round 3 found the whole class through this: the unwrapped
/// control was the only fixture in the set that was NOT a pane render (the body
/// sources it to the `ask_human` audit row `q-39`), and that asymmetry is what
/// hid the defect. A terminal hard-wraps at the column, mid-token, so a
/// rendered line boundary falls at an arbitrary character — and at width 72,
/// and only 72 of the widths 40..200, the wrap puts `/usage-credits to finish
/// what you` at the start of a rendered line.
const FIX_LIMIT_NEGATIVE_WRAPPED: &str =
    include_str!("../fixtures/attention/negative-orchestrator-quotes-a-limit-wrapped72.txt");

/// #3178 review round 3, B1 — a wrap must not synthesise a line-initial match.
///
/// Before the paragraph-start rule this returned `Some(anthropic)`: the
/// orchestrator's pane badged itself for TALKING about a provider limit, which
/// is the exact outcome the table dropped two needles to prevent, and which
/// #2811 S5b would turn into a spurious hold across every drive in the group.
#[test]
fn a_wrapped_quotation_is_not_a_refusal() {
    let (reg, _d, _g, wid) = attention_setup();
    let items = limit_scan(
        &reg,
        1_000_000_000_000,
        &[(wid.as_str(), FIX_LIMIT_NEGATIVE_WRAPPED)],
    );
    assert!(
        items.iter().all(|i| i.reason != "provider-limit"),
        "a quotation whose wrap happens to start a line with a needle must not raise \
         a limit: {items:?}"
    );

    // NON-VACUITY, and the reason this fixture is worth its bytes: the hazard
    // really is present in it. Some rendered line DOES begin with a needle
    // (line 1: `/usage-credits to finish what you're working on" (all Opus…`),
    // so the silence above is the paragraph-start rule working — not a fixture
    // that lost the shape it was cut to carry.
    let starts_a_line = FIX_LIMIT_NEGATIVE_WRAPPED.lines().any(|l| {
        let s = l.trim_matches(|c: char| c.is_whitespace() || c == '\u{2503}');
        providerlimit::LIMIT_PATTERNS.iter().any(|p| s.starts_with(p.needle))
    });
    assert!(
        starts_a_line,
        "fixture: some rendered line must open with a needle, or this test is asserting \
         the absence of a hazard that is not there"
    );

    // ...and the unwrapped sibling still reads the same way, so the fix did not
    // merely move the problem.
    let flat = limit_scan(&reg, 1_000_000_005_000, &[(wid.as_str(), FIX_LIMIT_NEGATIVE)]);
    assert!(flat.iter().all(|i| i.reason != "provider-limit"), "{flat:?}");
}

/// The residual the paragraph-start rule CANNOT close, pinned so the disclosure
/// in `limit_in_tail`'s doc and in `docs/design/attention-provider-limit.md`
/// cannot go quietly false.
///
/// `attention_tail` returns a byte-bounded tail, so line 0 of the scan window
/// is a fragment whose provenance is unknowable — it may open a paragraph or
/// sit mid-sentence. It must stay an eligible candidate, because
/// `claude-usage-limit.txt` is a REAL refusal whose needle is line 0, cut
/// exactly that way. The cost is that a quotation is still readable as a
/// refusal when the cut lands immediately before a needle: one byte offset,
/// where the pre-fix rule was one pane width in ~160.
#[test]
fn the_scan_window_cut_is_the_residual_line_zero_cannot_close() {
    let (reg, _d, _g, wid) = attention_setup();
    // A tail whose first line is the MIDDLE of a quoted sentence, cut so the
    // needle opens it — what a byte-bounded window can hand the scan.
    let cut_mid_quotation =
        "/usage-credits to finish what you're working on\" — that is rev-2313, not you.\n";
    let items = limit_scan(&reg, 1_000_000_000_000, &[(wid.as_str(), cut_mid_quotation)]);
    assert!(
        items.iter().any(|i| i.reason == "provider-limit"),
        "documented residual: a cut landing before a needle is indistinguishable from a \
         refusal, because line 0's provenance is unknowable: {items:?}"
    );
    // The bound on it: move the same text one line down, behind any other
    // output, and the paragraph rule refuses it again. So the residual really
    // is the FIRST line only, not any line.
    let one_line_down = format!("$ gh pr view 2747\n{cut_mid_quotation}");
    let bounded = limit_scan(&reg, 1_000_000_005_000, &[(wid.as_str(), &one_line_down)]);
    assert!(
        bounded.iter().all(|i| i.reason != "provider-limit"),
        "the residual is line 0 alone — one line down, the same text raises nothing: {bounded:?}"
    );
}

/// The other side of the same trade, pinned rather than left to be discovered:
/// the paragraph-start rule makes the scan MISS a refusal printed directly
/// under other output with no blank line between.
///
/// That is a deliberate direction. A false positive costs a spurious chip now
/// and, under #2811 S5b, a spurious hold across every drive on that provider; a
/// false negative costs the sixty-minute lane stall that existed before this
/// feature. Failing toward silence is the survivable half. It also costs
/// nothing on the real captures — all three agent CLIs render an error as its
/// own block, which is why every captured needle sits at line 0 or after a
/// blank gutter row.
#[test]
fn a_refusal_glued_under_other_output_is_not_detected() {
    let (reg, _d, _g, wid) = attention_setup();
    let glued = "  ┃  building the workspace, this takes a while\n  ┃  Key limit exceeded (total limit).\n";
    let items = limit_scan(&reg, 1_000_000_000_000, &[(wid.as_str(), glued)]);
    assert!(
        items.iter().all(|i| i.reason != "provider-limit"),
        "documented false negative: a refusal with no blank line above it is missed: {items:?}"
    );
    // Control: the identical refusal with the blank gutter row the real panes
    // actually draw IS detected, so this pins the paragraph rule and not a
    // detector that has stopped working on that needle.
    let blocked_out = "  ┃  building the workspace, this takes a while\n  ┃\n  ┃  Key limit exceeded (total limit).\n";
    let seen = limit_scan(&reg, 1_000_000_005_000, &[(wid.as_str(), blocked_out)]);
    assert!(
        seen.iter().any(|i| i.reason == "provider-limit"),
        "control: with the blank row the real TUIs draw, the same refusal is seen: {seen:?}"
    );
}

/// The convention itself, pinned on the fixture: the needle's rendered
/// line sits directly under a line that strips to nothing — the blank
/// gutter row these TUIs draw before a refusal block. (Same inline
/// strip as `a_wrapped_quotation_is_not_a_refusal`; these fixtures use
/// pi's U+2503 and no other gutter glyph.) A free fn, not a stored
/// closure: the closure form failed to infer its return lifetime at
/// the definition site (E0521-adjacent, "lifetime may not live long
/// enough" on the nested pattern closure), and the fix CI suggested
/// was a `move` the code does not need — the fn signature states
/// `&str -> &str` and the body uses the pattern closure immediately.
fn strip_conv_line(l: &str) -> &str {
    l.trim_matches(|c: char| c.is_whitespace() || c == '\u{2503}')
}

/// The positive control the paragraph-start rule owes its own premise (#3190).
///
/// The rule above is exact about what it costs: a refusal glued under other
/// output is missed, and `a_refusal_glued_under_other_output_is_not_detected`
/// pins that miss as EXPECTED — so a CLI update that changed its rendering
/// convention would leave that guard green straight through the regression.
/// These three fixtures are the other horn: each carries one refusal VERBATIM
/// from its captured fixture (`FIX_LIMIT_*` above), rendered the way the CLI
/// prints it TODAY — its own block, a blank gutter row before it. The test
/// demands detection on each, so a re-blessed fixture whose convention moved
/// reddens here instead of passing silently through the guard that documents
/// the miss. Re-bless procedure: `fixtures/attention/README.md`.
#[test]
fn the_current_convention_positive_controls_are_detected() {
    for (fixture, needle, provider_id, label) in [
        (
            FIX_CONV_CLAUDE,
            "/usage-credits to finish what you",
            "anthropic",
            "claude usage limit",
        ),
        (
            FIX_CONV_OR_KEY,
            "Key limit exceeded",
            "openrouter",
            "openrouter key limit",
        ),
        (
            FIX_CONV_OR_CREDITS,
            "This request would exceed your available credits",
            "openrouter",
            "openrouter credits",
        ),
    ] {
        // The needle we demand must be a row of the table it claims to
        // control — otherwise this loop could demand detection of a spelling
        // nothing matches any more and fail for the wrong reason.
        assert!(
            providerlimit::LIMIT_PATTERNS
                .iter()
                .any(|p| p.needle == needle),
            "{label}: needle is not a LIMIT_PATTERNS row"
        );
        // The convention itself, pinned on the fixture: the needle's rendered
        // line sits directly under a line that strips to nothing — the blank
        // gutter row these TUIs draw before a refusal block. (Same inline
        // strip as `a_wrapped_quotation_is_not_a_refusal`; these fixtures use
        // pi's U+2503 and no other gutter glyph.) Asserted per fixture so a
        // re-bless that loses the row fails HERE, naming the convention.
        let lines: Vec<&str> = fixture.lines().collect();
        let strip = strip_conv_line;
        let hit_line = lines
            .iter()
            .position(|l| {
                providerlimit::LIMIT_PATTERNS
                    .iter()
                    .any(|p| strip(l).starts_with(p.needle))
            })
            .unwrap_or_else(|| panic!("{label}: fixture carries no line-initial needle at all"));
        assert!(
            hit_line > 0 && strip(lines[hit_line - 1]).is_empty(),
            "{label}: the needle's line must sit directly under a blank gutter row — \
             the rendering convention `limit_in_tail`'s paragraph-start rule depends on"
        );
        assert!(
            strip(lines[hit_line]).starts_with(needle),
            "{label}: the fixture's line-initial needle drifted from the row it controls"
        );
        // ...and the scan really does see it — the positive control: the same
        // needle glued one row higher is pinned above as NOT detected.
        let hit = providerlimit::limit_in_tail(fixture)
            .unwrap_or_else(|| panic!("{label}: the current-convention refusal is not detected"));
        assert_eq!(hit.needle, needle, "{label}: wrong row matched");
        assert_eq!(hit.provider, provider_id, "{label}: wrong provider matched");
    }
}

/// #3178 review round 2, N4 — the `held-dialog` arm of the `outranked` set had
/// no test, so deleting `question_held.contains(*id)` from it reddened nothing
/// while the comment claimed both arms were pinned.
///
/// Same shape as the `blocked` case: only the carrier holds a `limit_chip`
/// entry, and `held-dialog` outranks `provider-limit`, so a carrier chosen
/// without consulting that latch is a carrier whose chip is swallowed.
#[test]
fn the_single_chip_avoids_a_pane_whose_held_dialog_latch_would_swallow_it() {
    let (reg, _d, g, first) = attention_setup();
    let second = reg.spawn_agent(&g, Role::Reviewer, "rev", "review", false, None).unwrap();
    let third = reg.spawn_agent(&g, Role::Reviewer, "rev2", "review", false, None).unwrap();

    let mut ids = vec![first.clone(), second.id.clone(), third.id.clone()];
    ids.sort();
    let lowest = ids[0].clone();
    assert!(
        ids.len() >= 2 && ids[1] != lowest,
        "fixture: another affected pane must exist for the chip to move to: {ids:?}"
    );

    // The latch this test exists for — the arm N4 found unpinned.
    reg.latch_question_held(&lowest);

    let tails = [
        (first.as_str(), FIX_LIMIT_OR_KEY),
        (second.id.as_str(), FIX_LIMIT_OR_KEY),
        (third.id.as_str(), FIX_LIMIT_OR_KEY),
    ];
    let items = limit_scan(&reg, 1_000_000_000_000, &tails);
    let raised: Vec<&AttentionItem> =
        items.iter().filter(|i| i.reason == "provider-limit").collect();
    assert_eq!(raised.len(), 1, "the group must still show its one chip: {items:?}");
    assert_ne!(
        raised[0].agent_id, lowest,
        "the chip must not be parked on the pane whose held-dialog latch outranks it"
    );
    assert_eq!(
        items.iter().find(|i| i.agent_id == lowest).map(|i| i.reason),
        Some("held-dialog"),
        "the outranking reason is untouched — a carrier choice, not a demotion"
    );

    // Control: drop the latch and the lowest-sorting pane becomes the carrier
    // again, so this test discriminates the `question_held` arm specifically
    // rather than passing for any reason at all.
    reg.unlatch_question_held(&lowest);
    let after = limit_scan(&reg, 1_000_000_005_000, &tails);
    assert_eq!(
        after.iter().find(|i| i.reason == "provider-limit").map(|i| i.agent_id.as_str()),
        Some(lowest.as_str()),
        "with the latch gone the lowest-sorting pane carries the chip again: {after:?}"
    );
}
