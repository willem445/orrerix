//! Session identity and resume: minting and sanitising session ids, resolving
//! a session reference, and finding a session's working directory across the
//! CLI stores (`StoreIndex`).
//! Design note: `docs/design/session-restore.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): `crate::sessions`,
//! `crate::opencodedb`. Sibling files it calls: `agentmodel.rs`, `clis/mod.rs`,
//! `clis/opencode.rs`, `clis/pi.rs`.

use super::*;

/// UUIDv4-format session id from the same entropy source as `new_token`
/// (Claude's `--session-id` requires a valid UUID).
pub(in crate::orchestration) fn new_session_uuid() -> String {
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
pub(in crate::orchestration) fn sanitize_session(s: &str) -> Option<String> {
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
pub(in crate::orchestration) fn is_full_session_id(s: &str) -> bool {
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
pub(in crate::orchestration) fn resolve_session_ref(records: &[AgentRecord], input: &str) -> Result<String, String> {
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
pub(in crate::orchestration) struct StoreIndex {
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
    pub(in crate::orchestration) fn contains(&mut self, cli: &str, session_id: &str) -> bool {
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
