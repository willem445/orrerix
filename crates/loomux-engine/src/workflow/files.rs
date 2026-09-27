//! Where a workflow lives and which ones a repo declares: the file paths,
//! [`WorkflowName`], and the named-workflow listing (#1689).

use super::*;

/// Where in the repo a workflow lives. Committed and shareable: a repo's
/// workflow is a property of the *project*, not of one developer's machine
/// (the #51 requirement).
pub const WORKFLOW_PATH: &str = ".orrerix/workflow.yml";

/// The pre-#1153 spelling, still discovered when `.orrerix/workflow.yml` is
/// absent — permanently, and never renamed on the repo's behalf: it is a
/// tracked file in somebody's git history. See [`crate::brand`] for the rule
/// and `docs/design/rebrand-filesystem.md` for the argument.
pub const LEGACY_WORKFLOW_PATH: &str = ".loomux/workflow.yml";

/// Which of the two spellings a given repo actually uses — the string every
/// message, audit line and preview must name, so that "this repo declares a
/// workflow (`…`)" points at the file that was really read.
pub fn workflow_path(repo: &str) -> &'static str {
    crate::brand::resolve_repo_file(repo, WORKFLOW_PATH, LEGACY_WORKFLOW_PATH)
}

/// The absolute workflow file for `repo`, resolved through [`workflow_path`].
pub fn workflow_file(repo: &str) -> std::path::PathBuf {
    Path::new(repo).join(workflow_path(repo))
}

/// The directory a repo's NAMED workflows live in (#1689).
///
/// `.orrerix/workflow.yml` stays exactly what it was — the workflow named
/// [`DEFAULT_WORKFLOW_NAME`] — and this directory is purely additive: a repo
/// that has never created it behaves byte-for-byte as it did before named
/// workflows existed, because nothing here is ever opened.
pub const WORKFLOWS_DIR: &str = ".orrerix/workflows";

/// The pre-#1153 spelling of [`WORKFLOWS_DIR`], discovered on the same terms
/// `.loomux/workflow.yml` is: when the preferred directory is absent, and never
/// renamed on the repo's behalf. See [`crate::brand`].
pub const LEGACY_WORKFLOWS_DIR: &str = ".loomux/workflows";

/// The name of the workflow a group runs when nothing says otherwise — and the
/// name `.orrerix/workflow.yml` is listed under.
///
/// A `group.json` written before #1689 has no `workflow` key at all, so this is
/// what it reads as: the one file that has always been there.
pub const DEFAULT_WORKFLOW_NAME: &str = "default";

/// Which of the two spellings of the workflows DIRECTORY this repo uses.
pub fn workflows_dir(repo: &str) -> &'static str {
    crate::brand::resolve_repo_dir(repo, WORKFLOWS_DIR, LEGACY_WORKFLOWS_DIR)
}

/// A workflow's name — the fifth identifier family through
/// [`check_segment`](crate::pathseg::check_segment) (#925's list, extended by
/// #1689).
///
/// It is a name that becomes `<name>.yml` under [`workflows_dir`], so it is a
/// path component and is validated as one: refused, never rewritten. A newtype
/// rather than a bare [`PathSegment`] so a workflow name and a session id cannot
/// be handed to each other's functions, exactly as
/// [`GroupId`](crate::groupid::GroupId) is a distinct type over the same checks —
/// and so [`workflow_file_named`] can demand the type at its signature, which is
/// what actually holds the property (a textual scan cannot see types; see
/// `src-tauri/tests/pathseg.rs`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkflowName(PathSegment);

impl WorkflowName {
    /// The one gate. Every `WorkflowName` in the process came through here, and it
    /// delegates to `PathSegment` rather than restating the rules — the drift #925
    /// closed is not worth reopening for a fifth family.
    pub fn parse(s: &str) -> Result<Self, SegmentError> {
        PathSegment::parse(s).map(WorkflowName)
    }

    /// [`DEFAULT_WORKFLOW_NAME`] as a validated name. Infallible by construction —
    /// `"default"` is in the alphabet — and written as an `expect` rather than an
    /// `unwrap` so a future edit to the constant that broke it says why.
    pub fn default_name() -> Self {
        WorkflowName::parse(DEFAULT_WORKFLOW_NAME)
            .expect("DEFAULT_WORKFLOW_NAME must be a valid path segment")
    }

    /// The validated name.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Whether this names the workflow `.orrerix/workflow.yml` is listed under.
    pub fn is_default(&self) -> bool {
        self.0.as_str() == DEFAULT_WORKFLOW_NAME
    }
}

impl std::fmt::Display for WorkflowName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl Default for WorkflowName {
    fn default() -> Self {
        WorkflowName::default_name()
    }
}

/// Transparent on the wire, like [`PathSegment`]: `group.json` carries a bare
/// string, so no persisted file changes shape.
impl serde::Serialize for WorkflowName {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.0.as_str())
    }
}

/// Validating on the way in, for the same reason `PathSegment` does: a
/// hand-edited state file is a construction site like any other.
impl<'de> serde::Deserialize<'de> for WorkflowName {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        WorkflowName::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// The absolute file for one NAMED workflow.
///
/// `default` resolves to [`workflow_file`] whenever that file exists, and to
/// `workflows/default.yml` only when it does not — which is what makes a repo
/// that has only ever had `.orrerix/workflow.yml` read byte-for-byte as it did
/// before #1689. When BOTH exist the plain file wins and [`list_workflows`]
/// reports the ambiguity; a launch is never blocked by it.
pub fn workflow_file_named(repo: &str, name: &WorkflowName) -> std::path::PathBuf {
    Path::new(repo).join(workflow_path_named(repo, name))
}

/// The repo-relative path one named workflow resolves to — the string every
/// message, audit line and preview must name, so "this group runs `…`" points
/// at the file that was really read. [`workflow_path`]'s named sibling.
pub fn workflow_path_named(repo: &str, name: &WorkflowName) -> String {
    if !name.is_default() {
        return workflows_dir_file(repo, name);
    }
    // `default` IS `.orrerix/workflow.yml` — the file that has always been
    // there, and the file to CREATE when a repo has none. `workflows/default.yml`
    // is tolerated when somebody made one and there is no plain file, but it is
    // never the answer for a repo that declares NEITHER: every refusal and
    // every "add the file" message names this path, and pointing a human at
    // `workflows/default.yml` would send them somewhere they have no reason to
    // go. Three arms, one `pick_repo_path`-shaped rule: prefer the plain file,
    // fall back to the directory only when it really holds one, else name the
    // canonical home.
    let plain = workflow_path(repo);
    if Path::new(repo).join(plain).is_file() {
        return plain.to_string();
    }
    let under_dir = workflows_dir_file(repo, name);
    if Path::new(repo).join(&under_dir).is_file() {
        return under_dir;
    }
    plain.to_string()
}

/// One name's path UNDER [`workflows_dir`], asked without the `default` special
/// case — which is what [`list_workflows`] needs for a row it read out of that
/// directory. `workflow_path_named` answers "the file this name RESOLVES to",
/// and for `default` beside a plain `workflow.yml` that is a different file;
/// a listing row that borrowed that answer would name the file that SHADOWED it
/// rather than itself (found by CI, not by reading).
///
/// **The one place a workflow name becomes a file name**, which is why it takes
/// `&WorkflowName` rather than `&str`: the signature is what holds the
/// property, and `src-tauri/tests/pathseg.rs` allowlists this line on the
/// strength of it. `workflow_file_named` joins the repo root onto
/// `workflow_path_named`'s result, so there is no second assembly point.
fn workflows_dir_file(repo: &str, name: &WorkflowName) -> String {
    format!("{}/{name}.yml", workflows_dir(repo))
}

/// [`load_workflow`] for a NAMED workflow. Identical contract: `Ok(None)` for
/// "no such file", `Err` for "the file is there and will not parse".
pub fn load_workflow_named(
    repo: &str,
    name: &WorkflowName,
) -> Result<Option<Workflow>, Vec<String>> {
    let path = workflow_file_named(repo, name);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| vec![format!("{} is unreadable: {e}", path.display())])?;
    parse_workflow(&text).map(Some)
}

/// One row of [`list_workflows`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowEntry {
    /// The name a group pins in `group.json` and a picker offers.
    pub name: WorkflowName,
    /// The repo-relative path this name resolves to, so every display site can
    /// say which file it really read.
    pub path: String,
    /// The file's own `name:` field — human prose, empty when the file will not
    /// parse.
    pub display_name: String,
    /// Why this file will not parse, if it will not. **A broken file is carried
    /// as an entry rather than dropped**: a workflow that vanishes from the
    /// picker the moment it gets a syntax error is a workflow the human cannot
    /// find their way back to.
    pub errors: Vec<String>,
}

/// Everything [`list_workflows`] found, plus what it could not make sense of.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkflowListing {
    /// One row per name, sorted by name.
    pub workflows: Vec<WorkflowEntry>,
    /// Listing-level problems — an ambiguous `default`, a file whose stem is not a
    /// usable name. **Not `errors` on an entry**, because these are not about a
    /// file failing to parse; and not silence, because a file dropped without a
    /// word is indistinguishable from a directory that never had it. Advisory:
    /// nothing here blocks a launch or an apply.
    pub findings: Vec<String>,
}

/// The most workflow files one repo may declare (#1689 slice B, closing the
/// unbounded-read residual #2658 raised on slice A).
///
/// A ceiling, not a design limit: a repo with more than this is not a repo
/// anybody picks a workflow out of by hand, and `read_dir` here is on a path
/// the group-view publisher walks once a second per leased group. Exceeding it
/// is a listing FINDING rather than an error, for the same reason every other
/// listing problem is: a workflow file may never block a launch (#225).
pub const WORKFLOWS_MAX: usize = 64;

/// The largest workflow file this build will parse (#1689 slice B, #2658).
///
/// Same posture as [`WORKFLOWS_MAX`]: a file above it is carried as an entry
/// with an error — visible in the picker, navigable back to — rather than read
/// into memory to find out.
pub const WORKFLOW_FILE_MAX_BYTES: u64 = 256 * 1024;

/// One name the repo declares, resolved to the file it means.
struct ScannedWorkflow {
    name: WorkflowName,
    /// Absolute, for reading.
    abs: PathBuf,
    /// Repo-relative, for display.
    rel: String,
}

/// Which names this repo declares, and which files they resolve to — the ONE
/// walk behind both [`list_workflows`] and [`list_workflow_names`], so the two
/// cannot disagree about what a repo offers.
///
/// Sorted by name (it is a `BTreeMap`), because `read_dir` order is a
/// filesystem accident — and on Windows not even a stable one — and a picker
/// that reorders itself between reads is a picker nobody can point at.
fn scan_workflows(repo: &str) -> (BTreeMap<String, ScannedWorkflow>, Vec<String>) {
    let mut seen: BTreeMap<String, ScannedWorkflow> = BTreeMap::new();
    let mut findings: Vec<String> = Vec::new();

    // An empty root is NOT a root, and this is the one place that can say so
    // (#2659 review round 1, rev-std 3). Every path below is built with
    // `Path::new(repo).join(..)`, and joining onto `""` yields a RELATIVE path
    // — so an empty string does not read "nothing", it reads "whatever directory
    // this process happens to be running in". A caller that has no repo must
    // get no listing, not a listing about the wrong machine.
    //
    // The call site was fixed too (`workflow_status_within` passes an `Option`
    // rather than `unwrap_or_default()`), and that is the enforcement; this is
    // the structural half, so the next caller cannot reintroduce it.
    if repo.trim().is_empty() {
        return (seen, findings);
    }

    let dir_rel = workflows_dir(repo);
    let dir = Path::new(repo).join(dir_rel);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("yml") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            let name = match WorkflowName::parse(stem) {
                Ok(n) => n,
                Err(e) => {
                    // The file name as `read_dir` spelled it — a diagnostic, never
                    // a path: the whole reason this row exists is that `stem` did
                    // NOT parse, so nothing here may be joined onto anything.
                    let file = path.file_name().and_then(|s| s.to_str()).unwrap_or(stem);
                    findings.push(format!("{dir_rel}/{file} is not offered: its {e}"));
                    continue;
                }
            };
            if seen.len() >= WORKFLOWS_MAX {
                // Said once, not once per surplus file: the finding is about the
                // directory, and a listing that scrolls its own truncation notice
                // has buried it.
                if !findings.iter().any(|f| f.contains("more than")) {
                    findings.push(format!(
                        "{dir_rel} declares more than {WORKFLOWS_MAX} workflow files — the \
                         listing is capped at {WORKFLOWS_MAX} names"
                    ));
                }
                break;
            }
            // The directory's own answer, never `workflow_path_named`'s — see
            // `workflows_dir_file`.
            let rel = workflows_dir_file(repo, &name);
            seen.insert(
                name.as_str().to_string(),
                ScannedWorkflow { name, abs: path, rel },
            );
        }
    }

    // The plain file, listed under `default` — and it WINS, so it is inserted
    // last. When it displaces a `workflows/default.yml`, say which file the name
    // now means rather than letting the human guess from a picker that shows one
    // row for two files.
    let plain_rel = workflow_path(repo);
    let plain = Path::new(repo).join(plain_rel);
    if plain.is_file() {
        if let Some(shadowed) = seen.remove(DEFAULT_WORKFLOW_NAME) {
            findings.push(format!(
                "both {plain_rel} and {shadowed} declare the workflow named \
                 '{DEFAULT_WORKFLOW_NAME}' — {plain_rel} is the one that is read",
                shadowed = shadowed.rel
            ));
        }
        seen.insert(
            DEFAULT_WORKFLOW_NAME.to_string(),
            ScannedWorkflow {
                name: WorkflowName::default_name(),
                abs: plain,
                rel: plain_rel.to_string(),
            },
        );
    }

    // The plain file is inserted AFTER the loop and WINS, so on a repo that
    // already filled the ceiling from `workflows/` it would push the listing one
    // over — 65 rows under a finding promising 64 (#2659 review round 1,
    // rev-std 2). The ceiling bounds the LISTING, so trim here; and never trim
    // the plain file, which the layout rule says is what the name `default`
    // means. Sorted, so the row dropped is the last by name.
    while seen.len() > WORKFLOWS_MAX {
        let Some(victim) =
            seen.keys().rev().find(|k| k.as_str() != DEFAULT_WORKFLOW_NAME).cloned()
        else {
            break;
        };
        seen.remove(&victim);
    }

    (seen, findings)
}

/// Every workflow this repo declares (#1689).
///
/// `.orrerix/workflow.yml` is listed as [`DEFAULT_WORKFLOW_NAME`]; every
/// `<name>.yml` under [`workflows_dir`] is listed under its own stem. Sorted by
/// name.
///
/// **Reads and parses every file it lists**, which is why it is bounded twice
/// ([`WORKFLOWS_MAX`], [`WORKFLOW_FILE_MAX_BYTES`]) and why the group-view
/// publisher uses [`list_workflow_names`] instead: a picker needs to know why a
/// file will not parse, a status payload only needs the names (#2658).
pub fn list_workflows(repo: &str) -> WorkflowListing {
    let (seen, findings) = scan_workflows(repo);
    WorkflowListing {
        workflows: seen.into_values().map(|s| read_entry(&s.abs, s.name, s.rel)).collect(),
        findings,
    }
}

/// A content digest of the file `name` resolves to (#1689 slice B, review
/// round 1 premortem 1) — what binds a confirmation to the bytes the human
/// was actually shown.
///
/// `None` when the file is absent or unreadable, which a caller must treat as
/// "cannot confirm" rather than "unchanged". Canonicalised by [`body_digest`],
/// so a checkout that differs only in line endings is the same document —
/// this tree is CRLF on disk and LF in the blob, and a digest that moved with
/// that would refuse every apply on one of the two.
///
/// It reads the file a second time, after the parse. That is deliberate and
/// cheap where it is used: a preview and an apply are human-gated actions, not
/// a poll, and threading the raw text out of `load_workflow_named` would widen
/// a signature every group-scoped reader shares to serve two callers.
pub fn workflow_digest(repo: &str, name: &WorkflowName) -> Option<String> {
    std::fs::read_to_string(workflow_file_named(repo, name)).ok().map(|t| body_digest(&t))
}

/// The names alone, sorted — no file is opened (#1689 slice B).
///
/// This is what `workflow_status` publishes as `available`, on a call the group
/// view's publisher makes once a second per leased group. [`list_workflows`]
/// would answer the same question by reading and YAML-parsing every declared
/// file, which is the unbounded read #2658 recorded against slice A; a name is
/// a directory-entry fact and needs none of it.
pub fn list_workflow_names(repo: &str) -> Vec<String> {
    scan_workflows(repo).0.into_keys().collect()
}

/// One [`WorkflowEntry`] off an absolute path already known to be a file.
fn read_entry(path: &Path, name: WorkflowName, rel: String) -> WorkflowEntry {
    // Size-checked BEFORE the read (#2658): a file above the ceiling is carried
    // as an entry with an error, never read into memory to find out. A file
    // whose size cannot be read at all falls through to the read below, which
    // produces the unreadable-file error with the real reason.
    if let Ok(md) = std::fs::metadata(path) {
        if md.len() > WORKFLOW_FILE_MAX_BYTES {
            return WorkflowEntry {
                name,
                path: rel,
                display_name: String::new(),
                errors: vec![format!(
                    "the file is {} bytes, above the {WORKFLOW_FILE_MAX_BYTES}-byte ceiling a \
                     workflow file is read under",
                    md.len()
                )],
            };
        }
    }
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| vec![format!("{} is unreadable: {e}", path.display())])
        .and_then(|text| parse_workflow(&text));
    match parsed {
        Ok(wf) => WorkflowEntry { name, path: rel, display_name: wf.name, errors: Vec::new() },
        Err(errors) => WorkflowEntry { name, path: rel, display_name: String::new(), errors },
    }
}
/// Schema version this build understands. Recorded in the file so a future
/// breaking change can be detected rather than mis-parsed.

#[cfg(test)]
mod tests {
    use super::*;

    /// **Field-inventory pin, not a runtime one.** The three `human_gate`
    /// denial tests in `tests/workflow/` catch a field *renamed* onto the
    /// reserved spelling — they would flip from red to a passing rejection if
    /// someone tried it. They do NOT catch a field *added* under a different,
    /// still gate-shaped name (`auto_merge:`, `skip_review:`, …): that field
    /// would pass every existing test, because nothing previously asserted
    /// the SET of fields these types accept, only that a few specific
    /// spellings are absent from it.
    ///
    /// This closes that gap at compile time instead of runtime: each
    /// destructure below names every field the type has right now and binds
    /// it to `_`, with no `..` to swallow the rest. Rust's exhaustive
    /// struct-pattern rule means a field ADDED to `RawWorkflow`, `RawIntake`
    /// or `RawIntakeLabels` without being named here is a **compile error**,
    /// not a silently passing test.
    ///
    /// NEW FIELD ADDED TO THE INTAKE SCHEMA — confirm it cannot weaken the
    /// human gate, then update this inventory (name the field in the
    /// matching destructure below and re-run this test to prove it still
    /// compiles).

    // ── `remote:` (#1457) ────────────────────────────────────────────────
    //
    // Engine UNIT tests rather than `src-tauri/tests/workflow/` integration
    // ones: `parse_workflow` is pure and lives here, and nothing below links
    // the Tauri lib, so CLAUDE.md constraint 4 (test executables that link the
    // lib need the comctl32-v6 manifest, which only integration targets get)
    // does not apply. The three refusals are the whole of what R1 ships, so
    // they are tested against the parser directly.

    // ── named workflows: discovery + naming (#1689) ──────────────────────
    //
    // Unit tests, for the reason the `remote:` block above states: discovery
    // and naming are pure functions over a directory, and nothing here links
    // the Tauri lib.

    /// A throwaway repo root under the OS temp dir.
    ///
    /// No `tempfile`: CLAUDE.md constraint 2 bans getrandom-based crates from
    /// anything linked into the shipped Windows binary, and this crate is. The
    /// name is minted from the caller's label plus a process-local counter, so
    /// two tests in one binary — and two binaries at once — cannot collide.
    fn temp_repo(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("loomux-wfdisco-{}-{label}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp repo");
        dir
    }

    /// A minimal file that parses, carrying `name:` so the display name is
    /// distinguishable from the file name.
    fn wf_doc(display: &str) -> String {
        format!("version: 1\nname: {display}\nblocks:\n  - id: w\n    kind: worker\n")
    }

    fn write_at(root: &std::path::Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
    }

    fn names(listing: &WorkflowListing) -> Vec<&str> {
        listing.workflows.iter().map(|e| e.name.as_str()).collect()
    }

    /// The naming rule, one row per shape — and every row is a REFUSAL, never a
    /// repair. `sanitize_id`, the predicate a workflow BLOCK id goes through,
    /// rewrites `../x` to `x`; a workflow NAME must not, because two strings
    /// naming one file is how a picker and a path join end up disagreeing about
    /// which thing they are talking about.
    #[test]
    fn a_workflow_name_is_refused_never_rewritten() {
        let cases: &[(&str, SegmentError)] = &[
            ("../x", SegmentError::IllegalChar('.')),
            ("..", SegmentError::IllegalChar('.')),
            ("a/b", SegmentError::IllegalChar('/')),
            ("a\\b", SegmentError::IllegalChar('\\')),
            ("my.workflow", SegmentError::IllegalChar('.')),
            ("C:", SegmentError::IllegalChar(':')),
            ("", SegmentError::Empty),
            // Path-safe, but an OPTION to any command line the name reaches.
            ("-x", SegmentError::LeadingDash),
            // Opens a device on Windows rather than naming a file.
            ("CON", SegmentError::ReservedDeviceName),
            ("nul", SegmentError::ReservedDeviceName),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                WorkflowName::parse(raw),
                Err(expected.clone()),
                "WorkflowName::parse({raw:?}) must be REFUSED with {expected:?} — never \
                 normalized into a neighbouring valid name"
            );
        }
        // The acceptance half: refusing a name a human would really use is an
        // outage, not hardening.
        for good in ["default", "review-heavy", "solo_fast", "a", "wf2"] {
            assert!(WorkflowName::parse(good).is_ok(), "{good:?} must be accepted");
        }
    }

    /// A repo that declares NOTHING still resolves `default` to
    /// `.orrerix/workflow.yml`, never to the `workflows/` spelling.
    ///
    /// Not cosmetic: every "this repo declares no workflow" message names this
    /// path, and the live workflow-mode toggle's refusal says *add the file*.
    /// Naming `.orrerix/workflows/default.yml` there sends a human to a
    /// directory they have no reason to create. Caught by
    /// `advanced_orchestrator_toggle_on_refuses_when_the_repo_declares_no_workflow_file`
    /// (`src-tauri/tests/orchestration/`), which is the surface that says so.
    #[test]
    fn a_repo_declaring_nothing_still_names_the_plain_file_as_default() {
        let root = temp_repo("declares-nothing");
        let repo = root.to_str().unwrap();
        let default = WorkflowName::default_name();

        assert_eq!(workflow_path_named(repo, &default), ".orrerix/workflow.yml");
        assert!(!workflow_file_named(repo, &default).is_file(), "and it is not there");
        assert_eq!(load_workflow_named(repo, &default), Ok(None), "absence, not an error");
        assert!(list_workflows(repo).workflows.is_empty());

        // The `workflows/` spelling becomes the answer only once a file really
        // sits there — the middle arm, which the two assertions above and below
        // it would both pass without.
        write_at(&root, ".orrerix/workflows/default.yml", &wf_doc("Only under the dir"));
        assert_eq!(workflow_path_named(repo, &default), ".orrerix/workflows/default.yml");
        assert_eq!(
            load_workflow_named(repo, &default).unwrap().unwrap().name,
            "Only under the dir"
        );

        // …and the plain file, once it exists, takes the name back.
        write_at(&root, ".orrerix/workflow.yml", &wf_doc("The plain one"));
        assert_eq!(workflow_path_named(repo, &default), ".orrerix/workflow.yml");

        // A NON-default name is unconditional: it names its file whether or not
        // one is there, because there is no other place it could live.
        let nope = WorkflowName::parse("nope").unwrap();
        assert_eq!(workflow_path_named(repo, &nope), ".orrerix/workflows/nope.yml");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The listing: named files under `workflows/`, sorted, plus the plain file
    /// under `default`.
    #[test]
    fn list_workflows_lists_the_named_files_and_the_plain_one_as_default() {
        let root = temp_repo("listing");
        // Written b-then-a so a listing that merely echoed `read_dir` order
        // could come back wrong on some filesystem — the sort is the claim.
        write_at(&root, ".orrerix/workflows/b.yml", &wf_doc("Bee"));
        write_at(&root, ".orrerix/workflows/a.yml", &wf_doc("Ay"));
        write_at(&root, ".orrerix/workflow.yml", &wf_doc("Plain"));
        let repo = root.to_str().unwrap();

        let listing = list_workflows(repo);
        assert_eq!(names(&listing), vec!["a", "b", "default"]);
        assert_eq!(listing.findings, Vec::<String>::new());
        assert_eq!(listing.workflows[0].path, ".orrerix/workflows/a.yml");
        assert_eq!(listing.workflows[0].display_name, "Ay");
        // `default` names the PLAIN file, not `workflows/default.yml`.
        assert_eq!(listing.workflows[2].path, ".orrerix/workflow.yml");
        assert_eq!(listing.workflows[2].display_name, "Plain");
        // Non-`.yml` neighbours are not workflows.
        write_at(&root, ".orrerix/workflows/a.layout.json", "{}");
        write_at(&root, ".orrerix/workflows/c.yaml", &wf_doc("See"));
        assert_eq!(
            names(&list_workflows(repo)),
            vec!["a", "b", "default"],
            "only `<name>.yml` is a workflow — a layout sidecar and a `.yaml` \
             spelling are not"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A repo that has only ever had `.orrerix/workflow.yml` is what it always
    /// was: one workflow, named `default`, and nothing under `workflows/` is
    /// ever opened, because there is no such directory.
    #[test]
    fn a_repo_with_only_the_plain_file_is_exactly_what_it_was() {
        let root = temp_repo("plain-only");
        write_at(&root, ".orrerix/workflow.yml", &wf_doc("Plain"));
        let repo = root.to_str().unwrap();
        let default = WorkflowName::default_name();

        let listing = list_workflows(repo);
        assert_eq!(names(&listing), vec!["default"]);
        assert_eq!(listing.findings, Vec::<String>::new());
        assert_eq!(workflow_path_named(repo, &default), ".orrerix/workflow.yml");
        assert_eq!(workflow_file_named(repo, &default), workflow_file(repo));
        // `load_workflow` IS `load_workflow_named` at `default` — the two must
        // not be able to disagree, which is why one calls the other.
        assert_eq!(
            load_workflow(repo).unwrap().unwrap().name,
            load_workflow_named(repo, &default).unwrap().unwrap().name
        );
        // A name this repo does not declare is absence, not an error.
        let b = WorkflowName::parse("b").unwrap();
        assert_eq!(load_workflow_named(repo, &b), Ok(None));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The legacy directory, discovered on exactly the terms
    /// `.loomux/workflow.yml` is — only when the preferred one is absent, and
    /// never renamed on the repo's behalf.
    #[test]
    fn the_legacy_workflows_directory_is_discovered_when_the_preferred_one_is_absent() {
        let root = temp_repo("legacy");
        write_at(&root, ".loomux/workflows/a.yml", &wf_doc("Legacy Ay"));
        let repo = root.to_str().unwrap();
        let a = WorkflowName::parse("a").unwrap();

        assert_eq!(workflows_dir(repo), LEGACY_WORKFLOWS_DIR);
        let listing = list_workflows(repo);
        assert_eq!(names(&listing), vec!["a"]);
        assert_eq!(listing.workflows[0].path, ".loomux/workflows/a.yml");
        assert_eq!(load_workflow_named(repo, &a).unwrap().unwrap().name, "Legacy Ay");

        // The preferred spelling, once it exists, is the one that is read — and
        // the legacy directory is left on disk untouched.
        write_at(&root, ".orrerix/workflows/a.yml", &wf_doc("Preferred Ay"));
        assert_eq!(workflows_dir(repo), WORKFLOWS_DIR);
        assert_eq!(list_workflows(repo).workflows[0].path, ".orrerix/workflows/a.yml");
        assert_eq!(load_workflow_named(repo, &a).unwrap().unwrap().name, "Preferred Ay");
        assert!(root.join(".loomux/workflows/a.yml").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file that will not parse stays IN the listing, carrying its errors. A
    /// workflow that vanishes from the picker the moment it gets a syntax error
    /// is one the human cannot navigate back to in order to fix it.
    #[test]
    fn a_broken_workflow_is_carried_as_an_entry_with_its_errors_not_dropped() {
        let root = temp_repo("broken");
        write_at(&root, ".orrerix/workflows/a.yml", &wf_doc("Ay"));
        write_at(&root, ".orrerix/workflows/b.yml", "version: 1\nblocks: [[[\n");
        let repo = root.to_str().unwrap();

        let listing = list_workflows(repo);
        assert_eq!(names(&listing), vec!["a", "b"], "the broken file is still offered");
        // Positive control on the two absence-shaped assertions below: this
        // listing really did open two files, rather than reporting an empty
        // directory — which would satisfy `errors.is_empty()` just as well.
        assert_eq!(listing.workflows.len(), 2);
        let b = &listing.workflows[1];
        assert!(!b.errors.is_empty(), "b.yml must carry why it will not parse");
        assert_eq!(b.display_name, "", "a file that will not parse has no display name");
        assert!(listing.workflows[0].errors.is_empty(), "a's entry is unaffected by b");
        // And it is an entry, not a listing-level finding: the file is there and
        // addressable, it just will not parse.
        assert_eq!(listing.findings, Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Both `workflow.yml` and `workflows/default.yml`: one name, two files.
    /// The plain file wins, the listing SAYS so, and nothing is blocked.
    #[test]
    fn a_default_declared_twice_is_a_finding_and_the_plain_file_wins() {
        let root = temp_repo("ambiguous");
        write_at(&root, ".orrerix/workflow.yml", &wf_doc("The plain one"));
        write_at(&root, ".orrerix/workflows/default.yml", &wf_doc("The named one"));
        let repo = root.to_str().unwrap();
        let default = WorkflowName::default_name();

        let listing = list_workflows(repo);
        assert_eq!(names(&listing), vec!["default"], "one NAME, however many files");
        assert_eq!(listing.workflows[0].path, ".orrerix/workflow.yml");
        assert_eq!(listing.workflows[0].display_name, "The plain one");
        assert_eq!(listing.findings.len(), 1, "the ambiguity is reported exactly once");
        assert!(
            listing.findings[0].contains(".orrerix/workflows/default.yml")
                && listing.findings[0].contains(".orrerix/workflow.yml"),
            "the finding must name BOTH files, or it cannot be acted on: {:?}",
            listing.findings[0]
        );
        // Advisory, not blocking: the load still succeeds, from the winner.
        assert_eq!(
            load_workflow_named(repo, &default).unwrap().unwrap().name,
            "The plain one"
        );
        // The other half of the rule: with the plain file gone, the same name
        // resolves to the file under `workflows/` and the finding goes away.
        std::fs::remove_file(root.join(".orrerix/workflow.yml")).unwrap();
        let listing = list_workflows(repo);
        assert_eq!(listing.findings, Vec::<String>::new());
        assert_eq!(listing.workflows[0].path, ".orrerix/workflows/default.yml");
        assert_eq!(
            load_workflow_named(repo, &default).unwrap().unwrap().name,
            "The named one"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file whose stem is not a usable name is REPORTED, not silently
    /// swallowed — an absent row and a directory that never had the file look
    /// identical to whoever is wondering where their workflow went.
    #[test]
    fn a_file_whose_stem_is_not_a_usable_name_is_reported_rather_than_dropped() {
        let root = temp_repo("unnamable");
        write_at(&root, ".orrerix/workflows/a.yml", &wf_doc("Ay"));
        write_at(&root, ".orrerix/workflows/my.workflow.yml", &wf_doc("Dotted"));
        write_at(&root, ".orrerix/workflows/CON.yml", &wf_doc("Device"));
        let repo = root.to_str().unwrap();

        let listing = list_workflows(repo);
        assert_eq!(names(&listing), vec!["a"], "neither unusable stem is offered");
        assert_eq!(listing.findings.len(), 2, "and BOTH are accounted for");
        assert!(
            listing.findings.iter().any(|m| m.contains("my.workflow.yml")),
            "findings: {:?}",
            listing.findings
        );
        assert!(
            listing.findings.iter().any(|m| m.contains("CON.yml")),
            "findings: {:?}",
            listing.findings
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every listing finding is ONE PARAGRAPH, pinned as a SHAPE beside the
    /// content the tests above pin.
    ///
    /// CLAUDE.md's rule, and it earned its place here rather than being
    /// precautionary: the ambiguity finding shipped with a `\` line
    /// continuation that had not survived authoring, so it carried an
    /// eighteen-space run mid-sentence with no newline at all. The
    /// `contains(…)` assertions above did not see it — no asserted substring
    /// straddled the break — and it reached CI only because a *different*
    /// assertion in the same test happened to print the whole string.
    ///
    /// Both shapes, because they are different failures: a `\n` plus
    /// indentation ships the source's leading spaces; a continuation that
    /// collapsed leaves the same run of spaces with no `\n`.
    #[test]
    fn every_listing_finding_is_one_paragraph() {
        let root = temp_repo("one-paragraph");
        // Every finding this function can produce, in one listing.
        write_at(&root, ".orrerix/workflow.yml", &wf_doc("Plain"));
        write_at(&root, ".orrerix/workflows/default.yml", &wf_doc("Shadowed"));
        write_at(&root, ".orrerix/workflows/my.workflow.yml", &wf_doc("Dotted"));
        let listing = list_workflows(root.to_str().unwrap());

        // Positive control: an empty findings list satisfies every assertion in
        // the loop below just as well.
        assert_eq!(listing.findings.len(), 2, "{:?}", listing.findings);
        for m in &listing.findings {
            assert!(!m.contains('\n'), "a finding must not carry a newline: {m:?}");
            assert!(
                !m.contains("          "),
                "a finding must not carry a ten-space run — a `\\` continuation that \
                 collapsed leaves one with no newline to notice: {m:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── the listing's two bounds, and the names-only walk (#1689 slice B) ──
    //
    // #2658 recorded `list_workflows` as an unbounded read against slice A:
    // `read_dir` plus a full YAML parse of every file, per call, with the
    // group-view publisher about to call it once a second.

    #[test]
    fn the_names_only_walk_answers_exactly_what_the_full_listing_does() {
        // The two share one scan on purpose — a second walk is a second answer
        // to "what does this repo declare", and the first time they disagreed
        // the picker and the status payload would be offering different rosters.
        let root = temp_repo("names-agree");
        write_at(&root, ".orrerix/workflow.yml", &wf_doc("Plain"));
        write_at(&root, ".orrerix/workflows/b.yml", &wf_doc("B"));
        // A file that will NOT parse is still a declared name, and both surfaces
        // must say so — the full listing carries its errors, the names-only walk
        // still offers it, because a workflow that vanishes the moment it breaks
        // is one the human cannot navigate back to.
        write_at(&root, ".orrerix/workflows/broken.yml", "version: 1\nblocks:\n  - id: m\n    kind: wizard\n");
        let repo = root.to_str().unwrap();

        let full = list_workflows(repo);
        assert_eq!(names(&full), vec!["b", "broken", "default"], "{:?}", names(&full));
        assert_eq!(list_workflow_names(repo), vec!["b", "broken", "default"]);
        assert!(
            full.workflows.iter().any(|e| e.name.as_str() == "broken" && !e.errors.is_empty()),
            "positive control: the broken file must really be broken, or this proves nothing"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_listing_stops_at_the_file_ceiling_and_says_so() {
        let root = temp_repo("too-many");
        for i in 0..(WORKFLOWS_MAX + 3) {
            // Zero-padded so the ordering the cap truncates is the SORTED one a
            // reader can predict, not `read_dir`'s.
            write_at(&root, &format!(".orrerix/workflows/w{i:03}.yml"), &wf_doc("W"));
        }
        let repo = root.to_str().unwrap();
        let listing = list_workflows(repo);
        assert_eq!(
            listing.workflows.len(),
            WORKFLOWS_MAX,
            "the listing must stop at the ceiling, not read every file on disk"
        );
        assert_eq!(
            listing.findings.iter().filter(|f| f.contains("more than")).count(),
            1,
            "said once, about the directory — not once per surplus file: {:?}",
            listing.findings
        );
        assert_eq!(list_workflow_names(repo).len(), WORKFLOWS_MAX, "the names-only walk is bounded too");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_root_lists_nothing_rather_than_the_working_directory() {
        // #2659 review round 1, rev-std 3. Every path in `scan_workflows` is
        // `Path::new(repo).join(..)`, and joining onto `""` yields a RELATIVE
        // path — so an empty string does not mean "nothing", it means "wherever
        // this process is running".
        //
        // DELIBERATELY WEAK, and labelled: with the guard this holds for every
        // CWD, and without it, it holds for every CWD that happens to declare no
        // workflow — which includes the one this suite runs in. So it pins the
        // answer, not the guard; nothing in this harness can redden the guard,
        // because proving it needs a CWD that DOES declare a workflow, i.e.
        // mutating a process-global from a parallel test suite.
        let listing = list_workflows("");
        assert!(listing.workflows.is_empty(), "{:?}", listing.workflows);
        assert!(listing.findings.is_empty(), "{:?}", listing.findings);
        assert!(list_workflow_names("").is_empty());
        assert!(list_workflows("   ").workflows.is_empty(), "whitespace is not a root either");

        // Positive control: the walk is not simply switched off — a real root
        // with a real file still lists it, in this same process.
        let root = temp_repo("empty-root-control");
        write_at(&root, ".orrerix/workflow.yml", &wf_doc("Plain"));
        assert_eq!(list_workflow_names(root.to_str().unwrap()), vec!["default".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_ceiling_counts_the_plain_file_too() {
        // #2659 review round 1, rev-std 2: the cap guarded only the `workflows/`
        // loop, and the plain file is inserted AFTER it and WINS — so a repo that
        // filled the ceiling from the directory listed 65 rows under a finding
        // promising 64. The ceiling bounds the LISTING.
        let root = temp_repo("ceiling-plus-plain");
        for i in 0..WORKFLOWS_MAX {
            write_at(&root, &format!(".orrerix/workflows/w{i:03}.yml"), &wf_doc("W"));
        }
        write_at(&root, ".orrerix/workflow.yml", &wf_doc("Plain"));
        let repo = root.to_str().unwrap();

        let listing = list_workflows(repo);
        assert_eq!(listing.workflows.len(), WORKFLOWS_MAX, "the listing is capped, not the loop");
        assert_eq!(list_workflow_names(repo).len(), WORKFLOWS_MAX, "and the names-only walk with it");
        // The plain file is never the row dropped: the layout rule says it is what
        // the name `default` means, so trimming it would answer the wrong question.
        assert!(
            names(&listing).contains(&"default"),
            "the plain file survives the trim: {:?}",
            names(&listing)
        );
        // The row that goes is the last by name — the directory rows are `w000`…,
        // which sort after `default`, so `w063` is the one that loses.
        assert!(
            !names(&listing).contains(&format!("w{:03}", WORKFLOWS_MAX - 1).as_str()),
            "{:?}",
            names(&listing)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_oversized_file_is_an_entry_with_an_error_not_a_read() {
        let root = temp_repo("oversize");
        let mut fat = wf_doc("Fat");
        // Comment padding: the file stays VALID YAML, so what the entry reports
        // can only be the size ceiling and never a parse failure.
        fat.push_str(&format!("# {}\n", "x".repeat(WORKFLOW_FILE_MAX_BYTES as usize + 1)));
        write_at(&root, ".orrerix/workflows/fat.yml", &fat);
        write_at(&root, ".orrerix/workflows/thin.yml", &wf_doc("Thin"));
        let listing = list_workflows(root.to_str().unwrap());

        let fat_entry = listing.workflows.iter().find(|e| e.name.as_str() == "fat").unwrap();
        assert!(
            fat_entry.errors.iter().any(|e| e.contains("ceiling")),
            "the oversized file must be carried with the size reason: {:?}",
            fat_entry.errors
        );
        assert!(fat_entry.display_name.is_empty(), "nothing was parsed out of it");
        // Negative control: the ceiling refuses the fat file only. A cap that
        // rejected everything would satisfy the assertion above just as well.
        let thin = listing.workflows.iter().find(|e| e.name.as_str() == "thin").unwrap();
        assert_eq!(thin.display_name, "Thin", "{:?}", thin.errors);
        let _ = std::fs::remove_dir_all(&root);
    }
}
