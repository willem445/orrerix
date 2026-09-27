//! The sanitizers a workflow's free-text fields pass through: ids, conditions,
//! globs, display text, personas and profile paths.

use super::*;

// ── sanitizers ──────────────────────────────────────────────────────────────

/// Longest block id. It becomes a file name (`<id>.md`) and an agent-id suffix,
/// and nothing legible needs more.
pub const MAX_ID_CHARS: usize = 48;

/// Block ids reach the shell (folded into a generated custom-agent file's
/// `loomux-<group>-<id>` handle, behind a `--agent <handle>` flag) and the
/// filesystem (`<id>.md` in the group dir, and as part of that generated
/// file's own name). Keep them to a conservative identifier alphabet so
/// neither surface can be escaped — the `sanitize_model` precedent, applied
/// to identity. Returns `None` for an id with no usable characters left.
///
/// The *parser* rejects an id this would have changed rather than accepting the
/// rewrite (see `parse_workflow`); this is the last-resort filter for ids that
/// arrive from somewhere other than a validated file — a hand-edited group.json.
pub fn sanitize_id(s: &str) -> Option<String> {
    let cleaned: String = s
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(MAX_ID_CHARS)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// A gate condition name (`ci-green`). Sub-PR 3 enforces gates inside the `gh`
/// PATH shim — a shell script — so these follow the same conservative alphabet
/// as a block id, with `.` allowed (CI check names carry it). Returns `None` for
/// a name with no usable characters; `parse_workflow` *rejects* anything this
/// would have changed rather than accepting the rewrite.
pub fn sanitize_condition(s: &str) -> Option<String> {
    let cleaned: String = s
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(MAX_ID_CHARS)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// A routing rule's path glob (#1176), on the same reject-never-rewrite
/// contract as [`sanitize_condition`]: the filtered string comes back, and
/// `parse_workflow` refuses anything the filter had to change.
///
/// **The alphabet is `A-Za-z0-9._-/` plus `*`, and nothing else.** The glob is
/// interpolated *unquoted into a POSIX `case` pattern* in the `gh` shim, so
/// every character it may contain has to be one the shell reads as either a
/// literal or `*`. `[`, `]`, `\`, `{`, `}`, whitespace and quotes are all
/// shell-pattern or word-splitting syntax, and none of them appears here.
///
/// **`?` is excluded deliberately, and it is the interesting omission.** It
/// would be trivial to allow — and it is the one character whose meaning the
/// two implementations could not be made to *provably* share: shell `case`
/// matches one character in the shell's locale (one byte in the C locale that
/// `sh` usually runs under), while a Rust mirror matches either a byte or a
/// `char`, and the two answers differ on the first non-ASCII path anybody
/// commits. With `*` as the only metacharacter, "any run of characters" and
/// "any run of bytes" are the same set, and the shim/mirror agreement is a
/// property of the alphabet rather than a claim in a PR body. Nobody routing
/// reviewers by area needs a single-character wildcard.
///
/// Three shapes are refused outright rather than filtered, all for one reason —
/// **a rule that could never fire is the unsatisfiable-gate failure this file
/// refuses everywhere else**, and a routing rule that never fires silently
/// removes a reviewer the repo asked for:
///
/// - a **leading `/`** — GitHub reports changed paths repo-relative, so
///   `/src/**` matches nothing;
/// - a **trailing `/`** — every changed path names a file, never a directory,
///   so `src/` matches nothing (`src/**` is what the author meant);
/// - a **`..` path segment** — no changed path GitHub reports contains one.
///   (Not a containment check: nothing here is joined onto a root. It is the
///   same never-fires argument.)
pub fn sanitize_glob(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.ends_with('/')
        || trimmed.split('/').any(|seg| seg == "..")
    {
        return None;
    }
    let cleaned: String = trimmed
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '*'))
        .take(MAX_GLOB_CHARS)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Does one [`sanitize_glob`]-clean pattern match one repo-relative changed
/// path? **The whole glob contract of #1176, and the thing the shim's `case`
/// mirrors.**
///
/// 1. `*` matches any run of characters, **including `/`**.
/// 2. Every other character matches itself.
/// 3. A leading `**/` is **optional**: `**/Cargo.toml` matches `Cargo.toml` as
///    well as `crates/a/Cargo.toml`.
/// 4. The match is anchored at both ends — the pattern describes the whole
///    path, not a substring of it.
///
/// **Rule 1 is coarser than gitignore's on purpose**, and the argument is the
/// direction of the error. Routing decides *which reviewers are required*:
/// over-matching adds a lane (an extra review nobody needed), under-matching
/// skips one (a merge the repo said needed that lane). Only one of those is
/// survivable, so the semantics err toward matching. Coarse also buys the thing
/// this feature actually has to pay for: `*`-crossing-`/` is exactly what a
/// POSIX `case` does for free, so the shim and this function are the same
/// matcher rather than two hand-written ones that agree today.
///
/// Rule 3 exists because `**/X` is the natural spelling of "every X" and under
/// rules 1–2 alone it would silently miss the one at the repo root — a *skipped*
/// reviewer, the unsurvivable direction. `**` anywhere else is simply `*`
/// repeated, which matches the same set.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    if star_match(pattern.as_bytes(), path.as_bytes()) {
        return true;
    }
    // Rule 3. `strip_prefix` rather than a general "collapse `**`" pass: the
    // ONLY place `**` means more than `*` is this one, and a rule the shim can
    // state in a two-line `case` is a rule the two implementations can be shown
    // to share.
    match pattern.strip_prefix("**/") {
        Some(rest) => star_match(rest.as_bytes(), path.as_bytes()),
        None => false,
    }
}

/// Anchored `*`-only wildcard match, iterative with one backtrack point — the
/// classic greedy algorithm, so a pathological pattern cannot blow the stack the
/// way a naive recursive matcher can. Bytes rather than `char`s because the
/// alphabet [`sanitize_glob`] permits is ASCII and `*` spans whole runs either
/// way; see that function for why `?` is not in it.
fn star_match(pat: &[u8], s: &[u8]) -> bool {
    let (mut p, mut i) = (0usize, 0usize);
    // `star` = the last `*` in the pattern we can fall back to; `mark` = the
    // input position it had consumed up to when we took that branch.
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while i < s.len() {
        if p < pat.len() && pat[p] == s[i] {
            p += 1;
            i += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = p;
            mark = i;
            p += 1;
        } else if star != usize::MAX {
            // Mismatch after a `*`: let that `*` swallow one more byte.
            p = star + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    // Trailing `*`s may match nothing; anything else left over is a mismatch,
    // which is what makes this anchored at the end.
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

/// Display names are cosmetic (pane title, roster row) and are rendered via
/// `textContent`, never HTML — so this is hygiene, not a boundary: drop control
/// characters (a pasted name must not smuggle escape codes into a pane title)
/// and cap the length. Mirrors `sanitize_agent_name`.
pub fn sanitize_display(s: &str) -> String {
    // Braces go too (rev-11 F3). A display string is repo-authored text that gets
    // substituted INTO a `{{KEY}}` template — the block note, the orchestrator's
    // roster rows — and `render_template` is a dumb ordered replace with no idea
    // which text is template and which is data. Substitution order alone is not
    // enough to make that safe: it protects a name against the passes that come
    // *after* it, not against a template whose own later keys it can name. Nobody
    // needs a brace in a pane title, so the character never gets that far.
    s.trim()
        .chars()
        .filter(|c| !c.is_control() && *c != '{' && *c != '}')
        .take(40)
        .collect()
}

/// Strips characters that could be structurally hazardous wherever persona
/// text ends up — a generated agent FILE (Claude's `~/.claude/agents/*.md`,
/// round #417 correction 6; Copilot's `~/.copilot/agents/*.agent.md`, #416)
/// or PTY-typed kickoff text (Copilot's write-failure fallback). Control
/// characters other than newline/tab are dropped outright: they have no
/// meaning in a persona and would ride straight into a terminal.
///
/// The `'` → typographic-apostrophe (U+2019) mapping predates round 6: it
/// protected the SINGLE-QUOTED shell token `claude --agents '<json>'` this
/// text used to ride on, before that mechanism was replaced with a
/// generated file (see `PersonaInject::claude_agent`'s doc for why — the
/// argv-length bug the replacement fixes). No current consumer is a raw
/// shell token, so this mapping is inert today — kept rather than removed,
/// both because it costs nothing (the prose still reads fine: "don't"
/// stays "don't", just with a curlier mark) and as defense-in-depth against
/// a future consumer reintroducing a shell-token use without re-deriving
/// this exact hazard from scratch. `ascii_escape_json`, which existed
/// solely to keep the OLD `--agents` JSON payload pure-ASCII on a
/// non-UTF-8 pane code page, had no other consumer and was removed
/// entirely alongside that mechanism, rather than left orphaned.
pub fn sanitize_persona(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\'' { '\u{2019}' } else { c })
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

/// Confine a `profile:` path to the repo. A workflow file is repo-authored input
/// and its `profile:` names a file loomux **reads and injects into an agent's
/// system prompt** — so an absolute path or a `..` escape would let a repo pull
/// any file on the operator's disk into an agent's context.
///
/// **The rules are the same on every platform, deliberately.** A workflow file is
/// committed and shared between developers (the #51 requirement), so a `profile:`
/// that is an escape on Windows and an innocent relative path on Linux is exactly
/// the divergence to kill: `std::path` would happily read `C:/Windows/win.ini` as
/// a *relative* path called `C:` on Unix, and `\\server\share\x` as a filename.
/// Both are rejected everywhere. The `Component` walk below is then belt and
/// braces on the platform that does understand them.
pub fn resolve_profile_path(repo: &str, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err("profile path is empty".into());
    }
    // Platform-independent rejections, done on the STRING before `std::path` gets
    // a chance to interpret it differently per OS.
    let norm = rel.replace('\\', "/");
    if norm.starts_with('/') {
        return Err(format!("profile {rel:?} must be a repo-relative path, not absolute"));
    }
    if norm.chars().nth(1) == Some(':') && norm.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
    {
        return Err(format!("profile {rel:?} must be a repo-relative path (no drive letter)"));
    }
    if norm.split('/').any(|seg| seg == "..") {
        return Err(format!("profile {rel:?} must stay inside the repo (no '..')"));
    }
    let p = Path::new(&norm);
    if p.is_absolute() {
        return Err(format!("profile {rel:?} must be a repo-relative path, not absolute"));
    }
    for c in p.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!("profile {rel:?} must stay inside the repo (no '..')"))
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(format!("profile {rel:?} must be a repo-relative path"))
            }
        }
    }
    // Join the FORWARD-SLASH form: Windows accepts it, and it means a file
    // written `.github\agents\x.md` by a Windows author still resolves for a
    // colleague on Linux, where a backslash is an ordinary filename character.
    Ok(Path::new(repo).join(p))
}
