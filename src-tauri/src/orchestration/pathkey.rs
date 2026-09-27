//! Path comparison keys (case and separator folding on Windows).
//! Design note: `docs/design/groupid-and-path-roots.md`.
//!
//! Moved out of `orchestration/mod.rs` verbatim by #3498 P4
//! (`docs/design/module-layout.md`). Outbound edges (#888): no `tauri`,
//! `PtyManager`, `OrchRegistry` or other `src-tauri` module. It calls no
//! sibling file.

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
pub(in crate::orchestration) fn normalize_path_key(s: &str) -> String {
    normalize_path_key_for(s, HOST_IS_WINDOWS)
}

/// [`same_path_key_for`] at this host's semantics.
pub(in crate::orchestration) fn same_path_key(a: &str, b: &str) -> bool {
    same_path_key_for(a, b, HOST_IS_WINDOWS)
}
