//! The `orrerix-plan` block: extract, parse, validate (#3040 P1).
//!
//! Design note: `doc/design/plan-driver.md` (contract 1 — the fence schema).
//! This module is the whole of that contract's implementation and nothing else:
//! it turns an issue-comment body into a [`PlanDoc`] or into a list of
//! line-numbered refusals. It reads no files, spawns nothing, and knows nothing
//! about drives, rosters or boards.
//!
//! # Why a fenced block rather than the prose plan
//!
//! The two specimen plans in the tree render the same four fields two different
//! ways — one as a markdown table, one as a bulleted list — so a parser over the
//! prose would be guessing, and a guessed brief typed into a worker pane is the
//! worst failure this feature has. The block is unambiguous by construction.
//! YAML rather than JSON because a slice `brief` is multi-paragraph prose: a
//! YAML block scalar carries it with no escaping, which is the form an LLM
//! writes correctly.
//!
//! # Refused, never repaired
//!
//! Nothing here trims, rewrites, defaults or infers. An `id` of `../x` is
//! **refused**, not sanitized to `x`; a branch containing `..` is refused, not
//! cleaned; a dep naming a slice that does not exist is refused, not dropped.
//! Repair is how two different strings come to name one thing, and every
//! downstream consumer of this type (a branch name, a board row, a worktree) is
//! somewhere that must not happen. This is the stance
//! [`PathSegment`](crate::pathseg) takes, for the same reason.
//!
//! `block` is deliberately **not** resolved here. Whether `worker-adv` exists is
//! a question about the roster the group was launched with, which this module
//! cannot see; the drive asks it (#3040 P3a). At parse time this is a non-empty
//! string and no more.
//!
//! # Where the line numbers come from
//!
//! Every refusal is `plan block line N: …` with N **absolute within the
//! comment** — the number a reader can scroll to — so the mapping is
//! `N = first_line + (line within the block) - 1`, where `first_line` is
//! [`Located::first_line`], the comment-relative line of the block's first
//! content line.
//!
//! `serde_norway` (0.9.42, the version in the lock) exposes **no per-value
//! spans**: a successfully parsed `Value` remembers nothing about where it came
//! from, and the crate has no `Spanned` wrapper. What it does expose is
//! [`serde_norway::Error::location`], and its deserializer attaches a mark to
//! *any* error raised from inside a `Deserialize` impl (`error::fix_mark`). So
//! this module gets real per-value lines in two ways:
//!
//! 1. **Per-value checks run inside `Deserialize`.** [`SliceId`] and
//!    [`BranchName`] validate in their own `Deserialize` impls, so a bad id or
//!    branch is a YAML error carrying the offending scalar's own mark. Unknown
//!    keys (`deny_unknown_fields`) and type mismatches come with marks free.
//! 2. **Cross-slice checks re-deserialize with a probe.** A duplicate id, a
//!    colliding branch, an unknown dep and a cycle are facts about the
//!    *document*, so they cannot be raised during a single pass. Once such a
//!    fault is found by index,
//!    `probe::line_of` re-deserializes the same text with a shadow type that
//!    deliberately fails at exactly that value, and reads the mark off the
//!    resulting error.
//!
//! The probe is best-effort by construction: if it cannot reach the addressed
//! value it returns `None` and the reason **degrades to index addressing** —
//! `plan block: slices[2].deps[0]: …`, the `blocks[i]: …` shape
//! [`crate::workflow::parse_workflow`] uses — rather than inventing a line. A
//! reason therefore has one of two shapes and never a third:
//!
//! ```text
//! plan block line 41: slices[2].deps[0]: unknown dep "P9"
//! plan block: slices[2].deps[0]: unknown dep "P9"
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

use crate::pathseg::check_segment;

// ---------------------------------------------------------------------------
// extract
// ---------------------------------------------------------------------------

/// The info string that marks the block. Exactly this, case-sensitive: a
/// planner writing a ```` ```yaml ```` fence has not written a plan block, and
/// guessing that it meant to would make every quoted YAML fence in a plan
/// comment a candidate.
pub const FENCE_INFO: &str = "orrerix-plan";

/// A value plus where in the comment it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located<T> {
    /// 1-based comment-relative line of the opening fence itself.
    pub fence_line: usize,
    /// 1-based comment-relative line of the block's **first content line** —
    /// `fence_line + 1`. This is what [`parse`] takes, because a YAML error's
    /// own line is 1-based within the text it parsed.
    pub first_line: usize,
    /// The block's content, borrowed out of the comment body.
    pub value: T,
}

/// Why a comment body carries no usable single plan block.
///
/// Distinct from [`parse`]'s reasons: these are faults of the *comment*, found
/// before any YAML is read, and each names the line(s) a reader must look at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanErr {
    /// Two (or more) plan blocks. Both fence lines are named: which one the
    /// author meant is exactly the thing that cannot be guessed.
    TwoBlocks {
        /// Opening-fence line of the first block.
        first: usize,
        /// Opening-fence line of the second.
        second: usize,
    },
    /// An opening fence with no closing fence before the end of the comment.
    Unterminated {
        /// Opening-fence line of the block that is never closed.
        fence_line: usize,
    },
}

impl fmt::Display for PlanErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanErr::TwoBlocks { first, second } => write!(
                f,
                "the comment carries two `{FENCE_INFO}` blocks (line {first} and line {second}); \
                 exactly one is expected — remove or re-label the one that is not the plan"
            ),
            PlanErr::Unterminated { fence_line } => write!(
                f,
                "the `{FENCE_INFO}` block opened on line {fence_line} is never closed"
            ),
        }
    }
}

impl std::error::Error for PlanErr {}

/// Find the one ```` ```orrerix-plan ```` block in an issue-comment body.
///
/// `Ok(None)` means the comment has no plan block at all, which is a different
/// thing from a broken one and is left for the caller to interpret (a drive
/// treats it as "the planner did not post a plan"; an ordinary comment is not
/// this module's business).
///
/// # What counts as a fence
///
/// A line whose trimmed content is three or more backticks followed by exactly
/// `orrerix-plan` (surrounding spaces allowed, nothing else). It is closed by
/// the next line whose trimmed content is a run of at least as many backticks
/// and nothing else — CommonMark's rule, so a longer fence inside the block is
/// not mistaken for its end. Indentation is preserved in the returned text
/// rather than stripped: dedenting is a repair, and a block whose YAML does not
/// stand on its own is refused by the parser with a line number instead.
pub fn extract(body: &str) -> Result<Option<Located<&str>>, PlanErr> {
    // (fence_line, content start byte, content end byte)
    let mut found: Option<(usize, usize, usize)> = None;
    let mut second_fence: Option<usize> = None;

    let mut line_no = 0usize;
    // (fence_line, backtick count, content start byte)
    let mut open: Option<(usize, usize, usize)> = None;
    let mut offset = 0usize;
    for line in body.split_inclusive('\n') {
        line_no += 1;
        let line_start = offset;
        offset += line.len();
        let text = line.trim_end_matches(['\n', '\r']);
        let trimmed = text.trim();
        let ticks = trimmed.chars().take_while(|c| *c == '`').count();
        if ticks < 3 {
            continue;
        }
        let rest = trimmed[ticks..].trim();
        match open {
            None => {
                if rest == FENCE_INFO {
                    if found.is_some() {
                        second_fence = Some(line_no);
                        break;
                    }
                    open = Some((line_no, ticks, offset));
                }
            }
            Some((fence_line, open_ticks, content_start)) => {
                // An inner fence carrying an info string is content, not a close.
                if rest.is_empty() && ticks >= open_ticks {
                    found = Some((fence_line, content_start, line_start));
                    open = None;
                }
            }
        }
    }

    if let (Some((first, _, _)), Some(second)) = (found, second_fence) {
        return Err(PlanErr::TwoBlocks { first, second });
    }
    if let Some((fence_line, _, _)) = open {
        return Err(PlanErr::Unterminated { fence_line });
    }
    Ok(found.map(|(fence_line, start, end)| Located {
        fence_line,
        first_line: fence_line + 1,
        value: &body[start..end],
    }))
}

// ---------------------------------------------------------------------------
// the schema
// ---------------------------------------------------------------------------

/// The only schema version this build understands. A plan carrying anything
/// else is refused rather than read optimistically: a future version exists
/// precisely because it means something different.
pub const SCHEMA_VERSION: u64 = 1;

/// Shortest acceptable `brief`, in characters. A slice brief is delivered
/// verbatim into a worker's kickoff, and a few words is the shape that produces
/// a worker guessing the task. The floor is a smell test, not a quality bar.
pub const MIN_BRIEF_CHARS: usize = 40;

/// Longest acceptable `branch`. Git itself has no fixed limit; this is the
/// point past which a ref name is a mistake rather than a name.
pub const MAX_BRANCH_LEN: usize = 200;

/// A slice identifier, validated through
/// [`check_segment`](crate::pathseg::check_segment) — the shared constructor of
/// CLAUDE.md constraint 6, of which this is the fifth family.
///
/// A slice id is not itself joined onto a path today, but it is interpolated
/// into a branch name, a pane name and a board row title, and it addresses a
/// row in a persisted map. Routing it through the shared gate rather than
/// writing a sixth private "is this a safe id" predicate is the whole point of
/// that constraint: the four it consolidated had drifted, and the weakest was
/// the one guarding a live join.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SliceId(String);

impl SliceId {
    /// The one gate. Refuses; never rewrites.
    pub fn parse(s: &str) -> Result<Self, String> {
        check_segment(s).map_err(|e| format!("slice id {s:?}: {e}"))?;
        Ok(SliceId(s.to_string()))
    }

    /// The validated identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SliceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for SliceId {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SliceId {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        // Raised from inside the visitor, not after it: the deserializer wraps
        // the visitor call with the scalar's own mark, so erroring here is what
        // gives this refusal a line number (see the module doc).
        struct V;
        impl Visitor<'_> for V {
            type Value = SliceId;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a slice id")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<SliceId, E> {
                SliceId::parse(s).map_err(de::Error::custom)
            }
        }
        de.deserialize_string(V)
    }
}

/// A git branch name, checked as a ref name and **refused, never sanitized**.
///
/// The rules are git's own `check_ref_format` minus the parts that need a
/// repository to answer. Each is here because a name breaking it either cannot
/// be created or is dangerous when interpolated into a command line:
///
/// - non-empty, and at most [`MAX_BRANCH_LEN`] bytes;
/// - no ASCII control byte, DEL, or space (a shell word boundary);
/// - none of `~ ^ : ? * [ \` (revision syntax, globbing, Windows separators);
/// - no `..` (revision-range syntax, and the traversal shape);
/// - no `@{`, and not the single character `@`;
/// - no leading `-` (an option to any command the name is passed to);
/// - no leading or trailing `/`, and no empty path component (`//`);
/// - no component beginning with `.`, no component ending `.lock`, and no
///   trailing `.`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchName(String);

impl BranchName {
    /// The one gate. Refuses; never rewrites.
    pub fn parse(s: &str) -> Result<Self, String> {
        fn bad(s: &str, why: &str) -> Result<BranchName, String> {
            Err(format!("branch {s:?}: {why}"))
        }
        if s.is_empty() {
            return bad(s, "is empty");
        }
        if s.len() > MAX_BRANCH_LEN {
            return bad(s, &format!("is {} bytes, max {MAX_BRANCH_LEN}", s.len()));
        }
        if let Some(c) = s.chars().find(|c| c.is_control() || *c == ' ') {
            return bad(s, &format!("contains {c:?}"));
        }
        if let Some(c) = s.chars().find(|c| "~^:?*[\\".contains(*c)) {
            return bad(
                s,
                &format!("contains {c:?}, which git refuses in a ref name"),
            );
        }
        if s.contains("..") {
            return bad(s, "contains \"..\"");
        }
        if s.contains("@{") {
            return bad(s, "contains \"@{\"");
        }
        if s == "@" {
            return bad(s, "is \"@\"");
        }
        if s.starts_with('-') {
            return bad(s, "starts with '-'");
        }
        if s.starts_with('/') || s.ends_with('/') {
            return bad(s, "starts or ends with '/'");
        }
        if s.ends_with('.') {
            return bad(s, "ends with '.'");
        }
        for part in s.split('/') {
            if part.is_empty() {
                return bad(s, "has an empty path component (\"//\")");
            }
            if part.starts_with('.') {
                return bad(s, &format!("has a component starting with '.' ({part:?})"));
            }
            if part.ends_with(".lock") {
                return bad(s, &format!("has a component ending \".lock\" ({part:?})"));
            }
        }
        Ok(BranchName(s.to_string()))
    }

    /// The validated ref name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for BranchName {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BranchName {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = BranchName;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a git branch name")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<BranchName, E> {
                BranchName::parse(s).map_err(de::Error::custom)
            }
        }
        de.deserialize_string(V)
    }
}

/// One slice of a plan: everything the drive needs to spawn one worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slice {
    /// Short identifier, unique within the plan (`P1`, `S3a`).
    pub id: SliceId,
    /// Human title; becomes the board row title beside the id.
    pub title: String,
    /// The branch the worker is to work on.
    pub branch: BranchName,
    /// Roster block id. **Not** resolved here — see the module doc.
    pub block: String,
    /// Slice ids this one waits for. Must name slices in this same plan.
    #[serde(default)]
    pub deps: Vec<SliceId>,
    /// The worker's brief, delivered verbatim.
    pub brief: String,
    /// Paths the worker is told not to touch. Informational: rendered into the
    /// brief header, enforced by nothing.
    #[serde(default)]
    pub avoid_files: Vec<String>,
    /// How this slice's change is to be shown failing first, if the planner
    /// named it.
    #[serde(default)]
    pub red_before_green: Option<String>,
    /// `true` = never auto-spawn. The orchestrator briefs this slice by hand:
    /// the planner's way of flagging a slice with a design call left open in it.
    #[serde(default)]
    pub hold: bool,
}

/// A parsed, validated plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDoc {
    /// Schema version; must be [`SCHEMA_VERSION`].
    pub version: u64,
    /// The issue this plan is for.
    pub issue: u64,
    /// The slices, in the planner's own order.
    pub slices: Vec<Slice>,
    /// Free-form risk notes, carried through untouched.
    #[serde(default)]
    pub risks: Vec<String>,
}

impl PlanDoc {
    /// The slice with this id, if the plan has one.
    pub fn slice(&self, id: &str) -> Option<&Slice> {
        self.slices.iter().find(|s| s.id.as_str() == id)
    }
}

/// The set of ids a plan declares — for the drive (#3040 P3a/P3b), which maps
/// them onto board rows and must not re-walk `slices` to do it.
pub fn declared_ids(doc: &PlanDoc) -> BTreeSet<String> {
    doc.slices.iter().map(|s| s.id.to_string()).collect()
}

// ---------------------------------------------------------------------------
// parse
// ---------------------------------------------------------------------------

/// Parse and validate one plan block.
///
/// `text` is the block's content and `first_line` its comment-relative first
/// line — both straight off [`extract`]'s [`Located`]. Every reason in the `Err`
/// is prefixed `plan block line N:` with N absolute within the comment, or
/// `plan block:` with an index address where no line could be established (see
/// the module doc).
///
/// YAML faults abort at the first one, because that is all `serde_norway`
/// reports and a second reason derived from a document that did not parse would
/// be invented. Semantic faults are collected: they are independent of one
/// another, and a planner fixing them one round-trip at a time is the cost this
/// whole feature exists to avoid.
pub fn parse(text: &str, first_line: usize) -> Result<PlanDoc, Vec<String>> {
    let doc: PlanDoc = match serde_norway::from_str(text) {
        Ok(d) => d,
        Err(e) => {
            let line = e.location().map(|l| l.line());
            return Err(vec![reason(
                first_line,
                line,
                &strip_yaml_position(&e.to_string()),
            )]);
        }
    };

    let mut errs = Vec::new();
    check_document(&doc, text, first_line, &mut errs);
    let edges_all_land = check_slices(&doc, text, first_line, &mut errs);
    check_cycle(&doc, text, first_line, edges_all_land, &mut errs);

    if errs.is_empty() {
        Ok(doc)
    } else {
        Err(errs)
    }
}

/// Faults of the document as a whole: the header scalars, and the two
/// uniqueness rules.
fn check_document(doc: &PlanDoc, text: &str, first_line: usize, errs: &mut Vec<String>) {
    if doc.version != SCHEMA_VERSION {
        errs.push(reason(
            first_line,
            probe::line_of(text, probe::Target::Version),
            &format!(
                "version: {} is not a schema version this build understands (expected \
                 {SCHEMA_VERSION})",
                doc.version
            ),
        ));
    }
    if doc.issue == 0 {
        errs.push(reason(
            first_line,
            probe::line_of(text, probe::Target::Issue),
            "issue: 0 is not an issue number",
        ));
    }
    if doc.slices.is_empty() {
        errs.push(reason(
            first_line,
            None,
            "slices: a plan with no slices drives nothing",
        ));
    }

    // Ids first: every later check addresses slices by id, and with a duplicate
    // present "the slice named P1" does not denote.
    //
    // **Case-insensitively**, and that is not fussiness. A slice id reaches a
    // pane name and a persisted row key, and a branch becomes a worktree
    // DIRECTORY on this project's Windows baseline — where `P1` and `p1`, or
    // `feat/A` and `feat/a`, are one name. Accepting both is precisely the
    // two-strings-name-one-thing hazard this module's own rationale refuses to
    // create by rewriting; it would be odd to refuse to *create* it and then
    // wave it through when a planner writes it directly. `to_ascii_lowercase`
    // is the whole fold: `check_segment` has already made the alphabet ASCII,
    // and `BranchName` has already refused every non-printable byte.
    let mut ids: BTreeMap<String, (usize, &str)> = BTreeMap::new();
    let mut branches: BTreeMap<String, (usize, &str)> = BTreeMap::new();
    for (i, s) in doc.slices.iter().enumerate() {
        let at = probe::line_of(text, probe::Target::SliceId(i));
        match ids.get(&s.id.as_str().to_ascii_lowercase()) {
            Some(&(first, spelling)) if spelling == s.id.as_str() => errs.push(reason(
                first_line,
                at,
                &format!(
                    "slices[{i}]: duplicate slice id {:?} (first declared at slices[{first}])",
                    s.id.as_str()
                ),
            )),
            Some(&(first, spelling)) => errs.push(reason(
                first_line,
                at,
                &format!(
                    "slices[{i}]: slice id {:?} differs from {spelling:?} at slices[{first}] only \
                     by case — the two would name one directory on a case-insensitive filesystem",
                    s.id.as_str()
                ),
            )),
            None => {
                ids.insert(s.id.as_str().to_ascii_lowercase(), (i, s.id.as_str()));
            }
        }

        let at = probe::line_of(text, probe::Target::Branch(i));
        match branches.get(&s.branch.as_str().to_ascii_lowercase()) {
            Some(&(first, spelling)) if spelling == s.branch.as_str() => errs.push(reason(
                first_line,
                at,
                &format!(
                    "slices[{i}] ({}): branch {:?} is already slices[{first}]'s — two slices on \
                     one branch cannot each open their own PR",
                    s.id,
                    s.branch.as_str()
                ),
            )),
            Some(&(first, spelling)) => errs.push(reason(
                first_line,
                at,
                &format!(
                    "slices[{i}] ({}): branch {:?} differs from {spelling:?} at slices[{first}] \
                     only by case — the two would name one worktree directory on a \
                     case-insensitive filesystem",
                    s.id,
                    s.branch.as_str()
                ),
            )),
            None => {
                branches.insert(
                    s.branch.as_str().to_ascii_lowercase(),
                    (i, s.branch.as_str()),
                );
            }
        }
    }
}

/// Per-slice faults. Returns whether every dep edge lands on a slice that
/// exists — the cycle walk is only meaningful once it does.
fn check_slices(doc: &PlanDoc, text: &str, first_line: usize, errs: &mut Vec<String>) -> bool {
    let mut edges_all_land = true;
    for (i, s) in doc.slices.iter().enumerate() {
        if s.title.trim().is_empty() {
            errs.push(reason(
                first_line,
                probe::line_of(text, probe::Target::SliceId(i)),
                &format!("slices[{i}] ({}): title is empty", s.id),
            ));
        }
        if s.block.trim().is_empty() {
            errs.push(reason(
                first_line,
                probe::line_of(text, probe::Target::SliceId(i)),
                &format!("slices[{i}] ({}): block is empty", s.id),
            ));
        }
        // `chars`, not `len`: the floor is about how much a worker was told, and
        // a brief in any script says as much per character as one in ASCII.
        let brief_chars = s.brief.trim().chars().count();
        if brief_chars < MIN_BRIEF_CHARS {
            errs.push(reason(
                first_line,
                probe::line_of(text, probe::Target::SliceId(i)),
                &format!(
                    "slices[{i}] ({}): brief is {brief_chars} characters, minimum \
                     {MIN_BRIEF_CHARS} — a slice brief is delivered verbatim into a worker's \
                     kickoff",
                    s.id
                ),
            ));
        }
        for (j, d) in s.deps.iter().enumerate() {
            if d == &s.id {
                edges_all_land = false;
                errs.push(reason(
                    first_line,
                    probe::line_of(text, probe::Target::Dep { slice: i, dep: j }),
                    &format!("slices[{i}].deps[{j}]: slice {} depends on itself", s.id),
                ));
            } else if doc.slice(d.as_str()).is_none() {
                edges_all_land = false;
                errs.push(reason(
                    first_line,
                    probe::line_of(text, probe::Target::Dep { slice: i, dep: j }),
                    &format!(
                        "slices[{i}].deps[{j}]: unknown dep {:?} — no slice in this plan has \
                         that id",
                        d.as_str()
                    ),
                ));
            }
        }
    }
    edges_all_land
}

/// The cycle walk, run only once every edge is known to land somewhere: a walk
/// over dangling edges reports a cycle that is really the unknown dep above.
fn check_cycle(
    doc: &PlanDoc,
    text: &str,
    first_line: usize,
    edges_all_land: bool,
    errs: &mut Vec<String>,
) {
    if !edges_all_land {
        return;
    }
    if let Some(cycle) = find_cycle(doc) {
        let (i, j) = cycle.edge;
        errs.push(reason(
            first_line,
            probe::line_of(text, probe::Target::Dep { slice: i, dep: j }),
            &format!(
                "slices[{i}].deps[{j}]: dependency cycle {} — nothing in it can ever start",
                cycle.path.join(" -> ")
            ),
        ));
    }
}

/// `plan block line N: msg`, or `plan block: msg` when no line could be
/// established. `block_line` is 1-based **within the block**, so the absolute
/// line is `first_line + block_line - 1`.
fn reason(first_line: usize, block_line: Option<usize>, msg: &str) -> String {
    match block_line {
        Some(n) => format!("plan block line {}: {msg}", first_line + n - 1),
        None => format!("plan block: {msg}"),
    }
}

/// `serde_norway` appends its own `at line L column C` to a `Display`ed error.
/// That number is block-relative, so leaving it in would put a second, wrong
/// line beside the absolute one this module prints.
///
/// **`rfind`, not `find`**: the position is a *suffix*, and the rest of the
/// message quotes the planner's own text. A key literally named
/// `foo at line 9` makes serde print ``unknown field `foo at line 9`, expected
/// one of … at line 12 column 5``, and cutting at the first match would
/// truncate the message to ``unknown field `foo`` — dropping the list of legal
/// keys, which is the only part a planner can act on. The plan is refused
/// either way, so this fails toward a useless message rather than toward a
/// wrong verdict; that is still a reason to get it right, and
/// `a_key_named_like_the_position_suffix_keeps_its_message` pins it.
fn strip_yaml_position(s: &str) -> String {
    match s.rfind(" at line ") {
        Some(i) => s[..i].trim_end().to_string(),
        None => s.to_string(),
    }
}

struct Cycle {
    /// `(slice index, dep index)` of the edge that closes the cycle.
    edge: (usize, usize),
    /// The cycle as ids, starting and ending on the same one.
    path: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Colour {
    White,
    Grey,
    Black,
}

/// Depth-first walk with a colour per node — the shape `find_dep_cycle` uses on
/// the board. Reports the first cycle found *and the edge that closed it*, so
/// the refusal can point at a line rather than at the whole plan.
fn find_cycle(doc: &PlanDoc) -> Option<Cycle> {
    let index: BTreeMap<&str, usize> = doc
        .slices
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect();
    let mut colour = vec![Colour::White; doc.slices.len()];
    let mut stack: Vec<usize> = Vec::new();

    for i in 0..doc.slices.len() {
        if colour[i] == Colour::White {
            if let Some(c) = walk_cycle(i, doc, &index, &mut colour, &mut stack) {
                return Some(c);
            }
        }
    }
    None
}

fn walk_cycle(
    i: usize,
    doc: &PlanDoc,
    index: &BTreeMap<&str, usize>,
    colour: &mut Vec<Colour>,
    stack: &mut Vec<usize>,
) -> Option<Cycle> {
    colour[i] = Colour::Grey;
    stack.push(i);
    for (j, d) in doc.slices[i].deps.iter().enumerate() {
        // A dangling edge is reported as an unknown dep, not as a cycle.
        let Some(&t) = index.get(d.as_str()) else {
            continue;
        };
        match colour[t] {
            Colour::Grey => {
                let from = stack.iter().position(|&n| n == t).unwrap_or(0);
                let mut path: Vec<String> = stack[from..]
                    .iter()
                    .map(|&n| doc.slices[n].id.to_string())
                    .collect();
                path.push(doc.slices[t].id.to_string());
                return Some(Cycle { edge: (i, j), path });
            }
            Colour::White => {
                if let Some(c) = walk_cycle(t, doc, index, colour, stack) {
                    return Some(c);
                }
            }
            Colour::Black => {}
        }
    }
    stack.pop();
    colour[i] = Colour::Black;
    None
}

// ---------------------------------------------------------------------------
// probe — a line number for a value serde already accepted
// ---------------------------------------------------------------------------

/// Re-deserializes the block with a shadow type that deliberately fails at one
/// addressed value, and reads the mark off the resulting error.
///
/// This exists because `serde_norway` keeps no spans on parsed values (module
/// doc). It is **best-effort and never load-bearing**: `line_of` returning
/// `None` costs a reason its line number and nothing else, and it cannot change
/// whether a plan is accepted — the document it re-reads has already parsed.
///
/// The counters are thread-local because serde's derive gives a `Deserialize`
/// impl nowhere to carry state. They are set and cleared inside
/// [`line_of`](probe::line_of), so nothing survives the call, and the shadow
/// types are private to this module so nothing else can observe them.
///
/// The shadow structs deliberately do **not** use `deny_unknown_fields` or
/// `#[serde(flatten)]`: unknown keys must be skipped (the probe walks a
/// document that has already parsed), and `flatten` would route the whole
/// struct through serde's buffering `Content` deserializer, which is exactly
/// the thing that would drop the marks this module is here to read.
mod probe {
    use super::{de, fmt, Deserialize, Deserializer, Visitor};
    use std::cell::Cell;

    /// The value to fail at.
    #[derive(Clone, Copy, PartialEq)]
    pub enum Target {
        /// The document's `version` scalar.
        Version,
        /// The document's `issue` scalar.
        Issue,
        /// `slices[i].id`.
        SliceId(usize),
        /// `slices[i].branch`.
        Branch(usize),
        /// `slices[slice].deps[dep]`.
        Dep {
            /// Index of the slice.
            slice: usize,
            /// Index within that slice's `deps`.
            dep: usize,
        },
    }

    thread_local! {
        static TARGET: Cell<Option<Target>> = Cell::new(None);
        /// Index of the slice currently being deserialized; `None` until the
        /// first one starts.
        static CUR_SLICE: Cell<Option<usize>> = Cell::new(None);
        static CUR_DEP: Cell<usize> = Cell::new(0);
    }

    /// The 1-based line, **within the block**, of the addressed value.
    pub fn line_of(text: &str, target: Target) -> Option<usize> {
        TARGET.with(|t| t.set(Some(target)));
        CUR_SLICE.with(|c| c.set(None));
        CUR_DEP.with(|c| c.set(0));
        let out = serde_norway::from_str::<ProbeDoc>(text);
        TARGET.with(|t| t.set(None));
        match out {
            // The probe is meant to fail. A success means the shadow type never
            // reached the addressed value (a reshaped document, or a target
            // computed against some other parse) — no line, and no guess.
            Ok(_) => None,
            Err(e) => e.location().map(|l| l.line()),
        }
    }

    fn is_target(t: Target) -> bool {
        TARGET.with(|c| c.get()) == Some(t)
    }

    /// Consume one scalar, failing if it is the target. The error is raised
    /// from inside the visitor so the deserializer attaches that scalar's own
    /// mark to it.
    fn eat_scalar<'de, D: Deserializer<'de>>(de: D, t: Target) -> Result<(), D::Error> {
        struct V(Target);
        impl V {
            fn check<E: de::Error>(self) -> Result<(), E> {
                if is_target(self.0) {
                    Err(de::Error::custom("plandoc probe"))
                } else {
                    Ok(())
                }
            }
        }
        impl Visitor<'_> for V {
            type Value = ();
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any scalar")
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
                self.check()
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
                self.check()
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
                self.check()
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
                self.check()
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
                self.check()
            }
            fn visit_unit<E: de::Error>(self) -> Result<(), E> {
                self.check()
            }
        }
        de.deserialize_any(V(t))
    }

    /// One dep scalar: bumps the per-slice dep counter, then fails if addressed.
    struct ProbeDep;
    impl<'de> Deserialize<'de> for ProbeDep {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            let slice = CUR_SLICE.with(|c| c.get()).unwrap_or(usize::MAX);
            let dep = CUR_DEP.with(|c| {
                let n = c.get();
                c.set(n + 1);
                n
            });
            eat_scalar(de, Target::Dep { slice, dep })?;
            Ok(ProbeDep)
        }
    }

    /// One slice's `id`: fails if this slice is the addressed one.
    struct ProbeId;
    impl<'de> Deserialize<'de> for ProbeId {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            let slice = CUR_SLICE.with(|c| c.get()).unwrap_or(usize::MAX);
            eat_scalar(de, Target::SliceId(slice))?;
            Ok(ProbeId)
        }
    }

    struct ProbeVersion;
    impl<'de> Deserialize<'de> for ProbeVersion {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            eat_scalar(de, Target::Version)?;
            Ok(ProbeVersion)
        }
    }

    struct ProbeIssue;
    impl<'de> Deserialize<'de> for ProbeIssue {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            eat_scalar(de, Target::Issue)?;
            Ok(ProbeIssue)
        }
    }

    /// One slice's `branch`: fails if this slice is the addressed one.
    struct ProbeBranch;
    impl<'de> Deserialize<'de> for ProbeBranch {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            let slice = CUR_SLICE.with(|c| c.get()).unwrap_or(usize::MAX);
            eat_scalar(de, Target::Branch(slice))?;
            Ok(ProbeBranch)
        }
    }

    #[derive(Deserialize)]
    struct ProbeSliceInner {
        #[allow(dead_code)]
        id: ProbeId,
        #[allow(dead_code)]
        branch: ProbeBranch,
        #[serde(default)]
        #[allow(dead_code)]
        deps: Vec<ProbeDep>,
    }

    /// Bumps the slice counter **before** delegating, so the counter is correct
    /// for every scalar inside this slice. Sequence elements are deserialized in
    /// document order, which is what makes the count an index.
    struct ProbeSlice;
    impl<'de> Deserialize<'de> for ProbeSlice {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            CUR_SLICE.with(|c| c.set(Some(c.get().map_or(0, |n| n + 1))));
            CUR_DEP.with(|c| c.set(0));
            ProbeSliceInner::deserialize(de)?;
            Ok(ProbeSlice)
        }
    }

    #[derive(Deserialize)]
    pub struct ProbeDoc {
        #[allow(dead_code)]
        version: ProbeVersion,
        #[allow(dead_code)]
        issue: ProbeIssue,
        #[allow(dead_code)]
        slices: Vec<ProbeSlice>,
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The specimen: plan-2386's Slices table (#2850, comment 5560145576)
    /// rewritten as an `orrerix-plan` block. Its last row (`S5 claude R2`) has
    /// no branch and no block in the table — it is a follow-up, not a slice a
    /// drive can spawn — so it is not in the block, and the block says so.
    const SPECIMEN: &str = "\
version: 1
issue: 2850
slices:
  - id: S0
    title: mock
    branch: feat/2891-s0-mock
    block: worker-adv
    deps: []
    brief: |
      Build the fixture-replay mock so S2 and S4 have a subject
      to render before the real projection lands.
    avoid_files: [src-tauri/src/orchestration/mod.rs]
    red_before_green: \"npm test test/mock.test.ts — fails on base with no such module\"
  - id: S1a
    title: harness contract
    branch: docs/2850-s1a-harness-contract
    block: worker-adv
    deps: []
    brief: |
      Write the harness contract note first: every other slice quotes it,
      so it lands before any of them.
  - id: S1b
    title: pi.rs rpc core
    branch: feat/2850-s1b-pi-rpc-core
    block: worker-adv
    deps: [S1a]
    brief: |
      The RPC core against the contract S1a fixes. No wiring into the
      registry yet; that is S3b's.
  - id: S2
    title: projection
    branch: feat/2891-s2-projection
    block: worker-adv
    deps: [S0, S1a]
    brief: |
      The projection from the transcript events onto the row model, driven
      by S0's fixtures so it can land in parallel with S1b.
  - id: S3a
    title: driver key caps schema
    branch: feat/2850-s3a-driver-key
    block: worker-quick
    deps: [S1a]
    brief: |
      The driver key, the capability set and the schema rows, mechanically
      from the contract. Parallel with S1b and S2.
  - id: S3b
    title: spawn drainer dialogs
    branch: feat/2850-s3b-structured-spawn
    block: worker-deep
    deps: [S1b, S3a]
    brief: |
      Structured spawn, the run-queue drainer and the dialogs. Serialized
      against every other mod.rs writer.
    avoid_files: [src-tauri/src/orchestration/mcp.rs]
    hold: true
  - id: S4
    title: DOM renderer
    branch: feat/2891-s4-renderer
    block: worker-adv
    deps: [S2]
    brief: |
      The DOM renderer. Starts on fixture replay and merges after S3b, so
      it is queued behind S2 only.
risks:
  - \"S3b and S4 both touch mod.rs — serialize them.\"
";

    fn in_comment(block: &str) -> String {
        format!("Plan for #2850.\n\nHere it is:\n\n```orrerix-plan\n{block}```\n\nEnd.\n")
    }

    fn extract_ok(body: &str) -> Located<&str> {
        extract(body)
            .expect("extract refused a body the test expects to be well-formed")
            .expect("extract found no plan block")
    }

    // -- extract ------------------------------------------------------------

    #[test]
    fn a_plan_with_two_fences_is_refused_naming_both_lines() {
        let body = "intro\n\n```orrerix-plan\nversion: 1\n```\n\nmore\n\n```orrerix-plan\nversion: 1\n```\n";
        let err = extract(body).expect_err("two blocks must be refused");
        assert_eq!(
            err,
            PlanErr::TwoBlocks {
                first: 3,
                second: 9
            }
        );
        let msg = err.to_string();
        assert!(msg.contains("line 3"), "{msg}");
        assert!(msg.contains("line 9"), "{msg}");
    }

    #[test]
    fn one_fence_is_found_with_its_lines() {
        let body = in_comment(SPECIMEN);
        let got = extract_ok(&body);
        assert_eq!(got.fence_line, 5);
        assert_eq!(got.first_line, 6);
        assert_eq!(got.value, SPECIMEN);
    }

    #[test]
    fn a_comment_with_no_block_is_not_an_error() {
        assert_eq!(extract("just prose\n\n```yaml\nversion: 1\n```\n"), Ok(None));
    }

    #[test]
    fn an_unterminated_fence_is_refused() {
        let err = extract("a\n```orrerix-plan\nversion: 1\n").expect_err("must be refused");
        assert_eq!(err, PlanErr::Unterminated { fence_line: 2 });
    }

    #[test]
    fn an_inner_fence_does_not_close_the_block() {
        // A brief quoting a shell block is the ordinary case; a shorter fence
        // carrying an info string must stay content.
        let body = "````orrerix-plan\nversion: 1\nissue: 1\nslices: []\n```bash\necho hi\n```\n````\n";
        let got = extract_ok(body);
        assert!(got.value.contains("echo hi"), "{:?}", got.value);
        assert_eq!(got.fence_line, 1);
    }

    // -- parse: the specimen ------------------------------------------------

    #[test]
    fn a_valid_specimen_round_trips() {
        let body = in_comment(SPECIMEN);
        let block = extract_ok(&body);
        let doc = parse(block.value, block.first_line)
            .unwrap_or_else(|e| panic!("the specimen must parse; got {e:#?}"));

        assert_eq!(doc.version, 1);
        assert_eq!(doc.issue, 2850);
        assert_eq!(doc.slices.len(), 7);
        assert_eq!(
            declared_ids(&doc).into_iter().collect::<Vec<_>>(),
            ["S0", "S1a", "S1b", "S2", "S3a", "S3b", "S4"]
        );

        let s3b = doc.slice("S3b").expect("S3b is in the specimen");
        assert_eq!(s3b.branch.as_str(), "feat/2850-s3b-structured-spawn");
        assert_eq!(s3b.block, "worker-deep");
        assert_eq!(
            s3b.deps.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
            ["S1b", "S3a"]
        );
        assert!(s3b.hold, "S3b carries hold: true");
        assert_eq!(s3b.avoid_files, ["src-tauri/src/orchestration/mcp.rs"]);
        assert_eq!(s3b.red_before_green, None);

        let s0 = doc.slice("S0").expect("S0 is in the specimen");
        assert!(!s0.hold, "hold defaults to false");
        assert!(s0.deps.is_empty());
        assert!(
            s0.red_before_green
                .as_deref()
                .is_some_and(|r| r.contains("npm test")),
            "{:?}",
            s0.red_before_green
        );
        // The brief travels verbatim, newlines and all — this is the field the
        // drive types into a worker pane.
        assert!(
            s0.brief.contains("Build the fixture-replay mock so S2 and S4 have a subject\nto render"),
            "{:?}",
            s0.brief
        );
        assert_eq!(doc.risks.len(), 1);
    }

    #[test]
    fn the_specimen_survives_a_serialize_round_trip() {
        let doc = parse(SPECIMEN, 1).expect("specimen parses");
        let yaml = serde_norway::to_string(&doc).expect("serializes");
        let again: PlanDoc = serde_norway::from_str(&yaml).expect("re-parses");
        assert_eq!(doc, again);
    }

    // -- parse: refusals ----------------------------------------------------

    fn refuse(block: &str) -> Vec<String> {
        // first_line 6 is the specimen's own placement in `in_comment`, so
        // every asserted absolute line here is one a reader could scroll to.
        parse(block, 6).expect_err("this block must be refused")
    }

    #[test]
    fn an_unknown_dep_is_refused_with_its_line() {
        let block = "\
version: 1
issue: 3040
slices:
  - id: P1
    title: one
    branch: feat/3040-p1
    block: worker-adv
    brief: a brief long enough to clear the forty character floor
  - id: P2
    title: two
    branch: feat/3040-p2
    block: worker-adv
    deps: [P9]
    brief: a brief long enough to clear the forty character floor
";
        let errs = refuse(block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        // `deps: [P9]` is block line 13; the block starts at comment line 6.
        assert_eq!(
            errs[0],
            "plan block line 18: slices[1].deps[0]: unknown dep \"P9\" — no slice in this plan \
             has that id"
        );
    }

    #[test]
    fn a_dep_cycle_is_refused() {
        let block = "\
version: 1
issue: 3040
slices:
  - id: A
    title: a
    branch: feat/a
    block: worker-adv
    deps: [C]
    brief: a brief long enough to clear the forty character floor
  - id: B
    title: b
    branch: feat/b
    block: worker-adv
    deps: [A]
    brief: a brief long enough to clear the forty character floor
  - id: C
    title: c
    branch: feat/c
    block: worker-adv
    deps: [B]
    brief: a brief long enough to clear the forty character floor
";
        let errs = refuse(block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(
            errs[0].contains("dependency cycle A -> C -> B -> A"),
            "{}",
            errs[0]
        );
        assert!(errs[0].starts_with("plan block line "), "{}", errs[0]);
    }

    #[test]
    fn a_self_dep_is_refused_as_itself_not_as_a_cycle() {
        let block = "\
version: 1
issue: 3040
slices:
  - id: A
    title: a
    branch: feat/a
    block: worker-adv
    deps: [A]
    brief: a brief long enough to clear the forty character floor
";
        let errs = refuse(block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(errs[0].contains("depends on itself"), "{}", errs[0]);
        assert!(
            !errs[0].contains("dependency cycle"),
            "a self-dep must not also be reported as a cycle: {}",
            errs[0]
        );
    }

    #[test]
    fn a_branch_with_dotdot_is_refused() {
        let block = "\
version: 1
issue: 3040
slices:
  - id: P1
    title: one
    branch: feat/../../etc
    block: worker-adv
    brief: a brief long enough to clear the forty character floor
";
        let errs = refuse(block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(errs[0].contains("contains \"..\""), "{}", errs[0]);
        // `branch:` is block line 6 → comment line 11.
        assert!(errs[0].starts_with("plan block line 11: "), "{}", errs[0]);
        // Refused, not repaired: no rewritten name appears in the reason.
        assert!(!errs[0].contains("feat/etc"), "{}", errs[0]);
    }

    #[test]
    fn nothing_is_repaired() {
        // Each case is a value a "helpful" parser would rewrite. Every one must
        // be refused, and the refusal must never quote the cleaned form.
        let cases: [(&str, &str, &str, &str); 5] = [
            ("id", "\"../x\"", "identifier contains", "\"x\""),
            ("id", "NUL", "reserved Windows device name", "\"nul\""),
            ("id", "\" P1 \"", "identifier contains", "\"P1\""),
            ("branch", "\"-delete\"", "starts with '-'", "\"delete\""),
            ("branch", "\"feat/a b\"", "contains ' '", "\"feat/ab\""),
        ];
        for (field, written, needle, repaired) in cases {
            let base = "version: 1
issue: 3040
slices:
  - id: P1
    title: one
    branch: feat/p1
    block: worker-adv
    brief: a brief long enough to clear the forty character floor
";
            let block = if field == "id" {
                base.replace("  - id: P1", &format!("  - id: {written}"))
            } else {
                base.replace("    branch: feat/p1", &format!("    branch: {written}"))
            };
            let errs = parse(&block, 6).expect_err("must be refused");
            assert_eq!(errs.len(), 1, "{field}={written}: {errs:#?}");
            assert!(errs[0].contains(needle), "{field}={written}: {}", errs[0]);
            assert!(
                !errs[0].contains(repaired),
                "{field}={written} was repaired to {repaired}: {}",
                errs[0]
            );
        }
    }

    #[test]
    fn a_traversal_id_is_refused_and_not_rewritten() {
        let block = "\
version: 1
issue: 3040
slices:
  - id: \"../x\"
    title: one
    branch: feat/p1
    block: worker-adv
    brief: a brief long enough to clear the forty character floor
";
        let errs = refuse(block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(
            errs[0].contains("identifier contains") && errs[0].contains("../x"),
            "{}",
            errs[0]
        );
        // The whole point: the refusal names the string as written. A repaired
        // `"x"` would mean two strings had come to name one slice.
        assert!(
            !errs[0].contains("slice id \"x\""),
            "the id must not be rewritten: {}",
            errs[0]
        );
        assert!(errs[0].starts_with("plan block line 9: "), "{}", errs[0]);
    }

    #[test]
    fn a_leading_dash_branch_is_refused() {
        assert!(BranchName::parse("-delete").is_err());
        assert!(BranchName::parse("feat/ok").is_ok());
    }

    #[test]
    fn the_branch_check_refuses_git_ref_illegals_and_accepts_real_names() {
        for good in [
            "main",
            "feat/3040-p1-plan-block",
            "docs/2850-s1a-harness-contract",
            "release/v1.3.0-beta9",
        ] {
            assert!(BranchName::parse(good).is_ok(), "{good} must be accepted");
        }
        for bad in [
            "",
            "feat/../x",
            "feat/ x",
            "feat/x~1",
            "feat/x^",
            "feat/x:y",
            "feat/x?",
            "feat/x*",
            "feat/x[1]",
            "feat\\x",
            "feat/x@{1}",
            "@",
            "-x",
            "/x",
            "x/",
            "a//b",
            "feat/.hidden",
            "feat/x.lock",
            "feat/x.",
            "feat/x\u{7f}y",
        ] {
            assert!(BranchName::parse(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(BranchName::parse(&"a".repeat(MAX_BRANCH_LEN)).is_ok());
        assert!(BranchName::parse(&"a".repeat(MAX_BRANCH_LEN + 1)).is_err());
    }

    #[test]
    fn a_duplicate_id_is_refused_with_the_second_occurrence_line() {
        let block = "\
version: 1
issue: 3040
slices:
  - id: P1
    title: one
    branch: feat/p1
    block: worker-adv
    brief: a brief long enough to clear the forty character floor
  - id: P1
    title: two
    branch: feat/p2
    block: worker-adv
    brief: a brief long enough to clear the forty character floor
";
        let errs = refuse(block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        // The second `- id: P1` is block line 9 → comment line 14.
        assert_eq!(
            errs[0],
            "plan block line 14: slices[1]: duplicate slice id \"P1\" (first declared at \
             slices[0])"
        );
    }

    /// Two slices whose bodies differ only in the `id` and `branch` values the
    /// caller supplies. Everything else is held constant so a refusal can only
    /// be about the pair under test.
    fn two_slices(id_a: &str, branch_a: &str, id_b: &str, branch_b: &str) -> String {
        let mut b = String::from("version: 1\nissue: 3040\nslices:\n");
        for (id, branch) in [(id_a, branch_a), (id_b, branch_b)] {
            b.push_str(&format!("  - id: {id}\n"));
            b.push_str("    title: a slice\n");
            b.push_str(&format!("    branch: {branch}\n"));
            b.push_str("    block: worker-adv\n");
            b.push_str("    brief: a brief long enough to clear the forty character floor\n");
        }
        b
    }

    #[test]
    fn two_distinct_slices_are_accepted() {
        // The control for the four collision tests below: same fixture shape,
        // no collision, accepted. Without it "refuse everything" would pass
        // every one of them.
        let block = two_slices("P1", "feat/p1", "P2", "feat/p2");
        parse(&block, 6).expect("two genuinely distinct slices must be accepted");
    }

    #[test]
    fn two_slice_ids_differing_only_by_case_are_refused() {
        let block = two_slices("P1", "feat/p1", "p1", "feat/p2");
        let errs = refuse(&block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(errs[0].contains("only by case"), "{}", errs[0]);
        assert!(errs[0].contains("\"p1\""), "{}", errs[0]);
        assert!(errs[0].contains("\"P1\""), "{}", errs[0]);
        // Refused, not folded: neither spelling is rewritten into the other.
        assert!(errs[0].starts_with("plan block line "), "{}", errs[0]);
    }

    #[test]
    fn two_slices_on_one_branch_are_refused() {
        let block = two_slices("P1", "feat/same", "P2", "feat/same");
        let errs = refuse(&block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(
            errs[0].contains("is already slices[0]'s"),
            "{}",
            errs[0]
        );
        assert!(errs[0].starts_with("plan block line "), "{}", errs[0]);
    }

    #[test]
    fn two_branches_differing_only_by_case_are_refused() {
        let block = two_slices("P1", "feat/A", "P2", "feat/a");
        let errs = refuse(&block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(errs[0].contains("only by case"), "{}", errs[0]);
        assert!(errs[0].contains("worktree directory"), "{}", errs[0]);
    }

    #[test]
    fn a_key_named_like_the_position_suffix_keeps_its_message() {
        // `strip_yaml_position` cuts serde's trailing "at line L column C". A
        // planner key that itself contains that phrase must not truncate the
        // message: the list of legal keys is the only part they can act on.
        let block = "\
version: 1
issue: 3040
slices:
  - id: P1
    title: one
    branch: feat/p1
    block: worker-adv
    brief: a brief long enough to clear the forty character floor
    \"foo at line 9\": 1
";
        let errs = refuse(block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(errs[0].contains("unknown field"), "{}", errs[0]);
        assert!(
            errs[0].contains("expected one of"),
            "the legal-key list must survive the strip: {}",
            errs[0]
        );
        assert!(
            errs[0].contains("foo at line 9"),
            "the offending key is named as written: {}",
            errs[0]
        );
        // Exactly one position phrase was cut, and none survives.
        assert!(
            !errs[0].contains(" column "),
            "the block-relative position must not survive: {}",
            errs[0]
        );
    }

    #[test]
    fn a_short_brief_is_refused() {
        let block = "\
version: 1
issue: 3040
slices:
  - id: P1
    title: one
    branch: feat/p1
    block: worker-adv
    brief: do the thing
";
        let errs = refuse(block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(errs[0].contains("brief is 12 characters"), "{}", errs[0]);
        assert!(errs[0].contains("minimum 40"), "{}", errs[0]);
    }

    #[test]
    fn a_wrong_version_is_refused() {
        let block = "version: 2\nissue: 3040\nslices: []\n";
        let errs = refuse(block);
        assert!(
            errs.iter().any(|e| e.contains("version: 2 is not a schema")),
            "{errs:#?}"
        );
        assert!(
            errs.iter().any(|e| e.contains("no slices drives nothing")),
            "both faults are reported, not just the first: {errs:#?}"
        );
        // `version:` is block line 1 → comment line 6.
        assert!(errs[0].starts_with("plan block line 6: "), "{}", errs[0]);
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        let block = "\
version: 1
issue: 3040
slices:
  - id: P1
    title: one
    branch: feat/p1
    block: worker-adv
    brief: a brief long enough to clear the forty character floor
    reviewer: rev-lead
";
        let errs = refuse(block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(errs[0].contains("unknown field"), "{}", errs[0]);
        assert!(errs[0].starts_with("plan block"), "{}", errs[0]);
    }

    #[test]
    fn a_yaml_syntax_error_carries_its_absolute_line_once() {
        let block = "version: 1\nissue: 3040\nslices:\n  - id: [unclosed\n";
        let errs = refuse(block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(errs[0].starts_with("plan block line "), "{}", errs[0]);
        // The block-relative position serde_norway appends is stripped, so the
        // reason carries exactly one line number and it is the absolute one.
        assert!(
            !errs[0].contains(" at line "),
            "the block-relative position must not survive: {}",
            errs[0]
        );
    }

    #[test]
    fn a_missing_required_field_is_refused() {
        let block = "version: 1\nissue: 3040\nslices:\n  - id: P1\n    title: one\n";
        let errs = refuse(block);
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(errs[0].contains("missing field"), "{}", errs[0]);
    }

    #[test]
    fn the_block_is_not_resolved_against_any_roster() {
        // `block` is a string here and nothing more — P3a asks the roster.
        let block = "\
version: 1
issue: 3040
slices:
  - id: P1
    title: one
    branch: feat/p1
    block: no-such-block-anywhere
    brief: a brief long enough to clear the forty character floor
";
        let doc = parse(block, 6).expect("an unknown block id parses");
        assert_eq!(doc.slices[0].block, "no-such-block-anywhere");
    }

    // -- the probe ----------------------------------------------------------

    #[test]
    fn the_probe_addresses_the_value_it_is_asked_for() {
        // A positive control for every reason above that carries a line: the
        // probe must resolve each target to the line that value really sits on,
        // and resolve *different* targets to *different* lines.
        let block = "\
version: 1
issue: 3040
slices:
  - id: A
    title: a
    branch: feat/a
    block: worker-adv
    deps: []
    brief: a brief long enough to clear the forty character floor
  - id: B
    title: b
    branch: feat/b
    block: worker-adv
    deps: [A, A]
    brief: a brief long enough to clear the forty character floor
";
        assert_eq!(probe::line_of(block, probe::Target::Version), Some(1));
        assert_eq!(probe::line_of(block, probe::Target::Issue), Some(2));
        assert_eq!(probe::line_of(block, probe::Target::SliceId(0)), Some(4));
        assert_eq!(probe::line_of(block, probe::Target::SliceId(1)), Some(10));
        assert_eq!(probe::line_of(block, probe::Target::Branch(0)), Some(6));
        assert_eq!(probe::line_of(block, probe::Target::Branch(1)), Some(12));
        assert_eq!(
            probe::line_of(block, probe::Target::Dep { slice: 1, dep: 0 }),
            Some(14)
        );
        assert_eq!(
            probe::line_of(block, probe::Target::Dep { slice: 1, dep: 1 }),
            Some(14)
        );
        // Out of range: the probe never reaches it, so it says so rather than
        // guessing — this is the degradation path the module doc describes.
        assert_eq!(probe::line_of(block, probe::Target::SliceId(9)), None);
        assert_eq!(
            probe::line_of(block, probe::Target::Dep { slice: 0, dep: 0 }),
            None
        );
    }

    #[test]
    fn a_reason_without_a_line_degrades_to_the_index_address() {
        assert_eq!(
            reason(6, None, "slices[2].deps[0]: unknown dep \"P9\""),
            "plan block: slices[2].deps[0]: unknown dep \"P9\""
        );
        assert_eq!(
            reason(6, Some(1), "version: bad"),
            "plan block line 6: version: bad"
        );
    }

    #[test]
    fn extract_then_parse_agree_on_the_absolute_line() {
        // The two halves are only useful together, so pin the composition: a
        // fault on a known comment line must be reported on that line.
        let block = "\
version: 1
issue: 3040
slices:
  - id: P1
    title: one
    branch: feat/../x
    block: worker-adv
    brief: a brief long enough to clear the forty character floor
";
        let body = in_comment(block);
        let located = extract_ok(&body);
        let errs = parse(located.value, located.first_line).expect_err("must be refused");
        let want_line = body
            .lines()
            .position(|l| l.contains("feat/../x"))
            .expect("the offending line is in the comment")
            + 1;
        assert!(
            errs[0].starts_with(&format!("plan block line {want_line}: ")),
            "expected line {want_line}: {}",
            errs[0]
        );
    }
}
