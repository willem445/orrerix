// Where `build.rs` puts the sideloaded ConPTY host (#3897).
//
// `build.rs` cannot import compiled lib symbols, so this file is
// dependency-free and is pulled into both places, like `command_manifest.rs`:
//   - `build.rs`, via `include!`, to find the dir `copy_sideloaded_conhost`
//     copies `conpty.dll` + `OpenConsole.exe` into;
//   - `lib.rs` (via `mod buildpaths;`), so `tests/buildpaths.rs` can pin the
//     derivation against both of cargo's build-dir layouts.

/// The `<target>/<profile>` directory a build script's `OUT_DIR` sits under —
/// where cargo places the package's final executables, and so where a file must
/// be for `portable-pty` to find it "next to the executable".
///
/// Cargo exposes no target-dir variable to build scripts (rust-lang/cargo#13663),
/// so this is read off `OUT_DIR`'s shape, and the shape is not fixed. Under the
/// original layout `OUT_DIR` is `<target>/<profile>/build/<pkg>-<hash>/out`, three
/// levels below the profile dir; under cargo's build-dir layout v2 — what CI's
/// Rust 1.100.0 produced, where 1.99.1 still produced the original — it is
/// `<target>/<profile>/build/<pkg>/<hash>/out`, four levels below. A fixed ancestor count (`nth(3)`, the pre-#3897 code) lands
/// in `<target>/<profile>/build` under v2, where no executable ever runs, and every
/// dev build and test silently fell back to the inbox conhost. What both layouts
/// share is the `build` directory itself, so the answer is the parent of the
/// NEAREST ancestor named `build` — nearest, because a checkout path can carry a
/// `build` component of its own higher up.
///
/// Its limits, stated rather than guessed around: a package literally named
/// `build` under the v2 layout would be misread as the layout's own directory
/// (this crate is `orrerix`), and a `build.build-dir` configured apart from the
/// target dir yields the build-dir's profile dir, not the artifact dir — the copy
/// then misses, which is the same harmless fallback to the inbox conhost that a
/// checkout without the binaries gets. `None` when `out_dir` has no `build`
/// ancestor at all, and the caller then copies nothing.
pub fn profile_dir_from_out_dir(out_dir: &std::path::Path) -> Option<&std::path::Path> {
    out_dir.ancestors().nth(3) // MUTATION #3897: the pre-fix derivation
}
